# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust driver and `systemd` service for the **Sudokoo SK700V** CPU-cooler status
panel (USB `381c:0003`), on Linux. It is a rewrite of the working Python driver in the
sibling reverse-engineering repo at `../sudokoo/` (`sk700ctl/sk700.py`). The panel is a
fixed four-field layout — FREQ / LOAD / POWER / CPU TEMP — with no image or wallpaper
path on this model.

Edition 2024, BSD-3-Clause, **no dependencies** — everything is std. Keep it that way
unless there is a real reason not to: this installs as a root systemd service, and the
only foreign call it needs is `signal(2)`, declared directly in `src/main.rs`.

## Commands

```
cargo build --release            # target/release/sudokoo
cargo test                       # no hardware needed
cargo test <name>                # single test by substring
cargo test --lib -- --ignored    # the one hardware test; lights the real panel
cargo clippy --all-targets -- -D warnings
cargo fmt
systemd-analyze verify packaging/sudokoo.service
```

## Layout

| file | role |
|---|---|
| `src/frame.rs` | wire frames. Pure, no I/O; `build` is generic over payload length so an oversized frame is a compile error |
| `src/device.rs` | hidraw discovery by USB id, and the write path. Write-only by construction |
| `src/sensors.rs` | sysfs/procfs readings. Paths come from `Config`, so discovery is tested against fixture trees |
| `src/main.rs` | argument parsing, the refresh loop, signal handling, output |
| `packaging/` | systemd unit and udev rule |

Layering runs one way: `main` → `sensors` → `frame` → `device`. `sensors::Reading` holds
floats in Celsius; `Reading::to_frame` is the single place that converts units, rounds,
and saturates into the wire widths.

Every sensor degrades to zero rather than erroring — a missing sensor is a blank field,
not a reason to stop refreshing, and stopping would blank the whole panel.

## Reference material (`../sudokoo/`)

Do not re-derive the protocol; it is fully solved and confirmed on hardware 2026-08-15.

| path | what it holds |
|---|---|
| `sk700ctl/STATUS.md` | **the authoritative protocol write-up** — read this first |
| `sk700ctl/sk700.py` | the working Python driver this project ports |
| `sk700ctl/probe_mode.py` | the frame-grammar probe that discovered the open command |
| `sk700ctl/hidprobe.py` | low-level hidraw read/write for recon |
| `sk700ctl/99-sk700.rules`, `sk700ctl/sk700.service` | udev rule and systemd unit to mirror |
| `decompile/mastercraft_decompiled.js` | 60k lines of decompiled MasterCraft 1.0.3 bytecode; the SK700 service is `Sk700SeriesUsbService` around line 20207, `sendReport` at 19956, `displayClose` at 20066 |
| `MasterCraft/SK700_Series.pdf` | vendor manual; p.15 has the panel layout |

## The protocol

HID **output reports, report id `0x10`**, written to the device's hidraw node and padded
to 64 bytes total (`[id][63 bytes]`). Framing is DL/T-645-style:

```
68 <addr=01> <ctrl=09> <len> <data ...> <cs> 16
    len = number of data bytes following the length field
    cs  = sum(0x68 .. last data byte) & 0xFF   (a plain byte sum, NOT a CRC)
```

Byte 1 of `<data>` selects the operation:

| data | len | meaning |
|---|---|---|
| `01 01` + 11-byte panel payload | `0D` | **display OPEN** — absent from the vendor app; found by probing |
| `01 00` + 11-byte panel payload | `0D` | display CLOSE |
| `01 02 <power:u16be> <ratio:u8> <unit:u8> <temp:f32be> <usage:u8> <freq:u16be>` | `0D` | sensor report |
| `03 <unit>` | `02` | set unit, `00` = °C, `01` = °F |

The 11-byte panel payload is the constant `00 64 32 00 42 70 00 00 28 0E 10`, lifted
verbatim from the app's hardcoded `displayClose` and passed through unchanged. Its
internal fields are **not** decoded (likely brightness / rotation / bar-graph ranges —
decoding them is open work).

Multi-byte integers are big-endian; temperature is a raw IEEE-754 `f32`, big-endian
(`f32::to_be_bytes`), *not* the app's per-hex-character expansion.

Known-good frames (report id included):

```
open      10 68 01 09 0d 01 01 00 64 32 00 42 70 00 00 28 0e 10 0f 16
close     10 68 01 09 0d 01 00 00 64 32 00 42 70 00 00 28 0e 10 0e 16
set unit  10 68 01 09 02 03 00 77 16                    (°C; °F = ... 03 01 78 16)
sensor    10 68 01 09 0d 01 02 00 3c 00 00 42 5c 00 00 32 10 68 06 16
          (power 60 W, ratio 0, °C, temp 55.0, usage 50 %, freq 4200 MHz)
```

**Sequence:** send `open` once, then a sensor report at ~1 Hz (`--interval` well under the
watchdog). The firmware blanks the panel a few seconds after the last frame it *accepted*.

**The device is write-only in practice.** It never ACKs, never sends anything unsolicited
on the IN endpoint, and returns zeros to every `GET_FEATURE`. A rejected frame is
indistinguishable from silence — the lit screen is the only oracle. Design the driver
accordingly: no read path, no waiting for replies.

### `ratio` is the one unverified field

All four displayed fields (temp, usage, power, freq) were confirmed on hardware. `ratio`
is believed to drive a bar graph. The Python driver sends `power/TDP * 100`. The vendor
app computes `floor(power / (TDP * 1.3 for AMD, * 2 for Intel) / 10) * 10000` truncated to
one byte, which is 0 for all realistic inputs — so the app never exercised it either. Keep
it a distinct, documented parameter rather than folding it into power.

### Why the vendor software never worked

Three defects in `Sk700SeriesUsbService`, all visible in the decompiled bytecode. Knowing
them prevents "faithfully reproducing the capture" and failing, as both we and
`deepcool-digital-linux` issue #100 did:

1. `tToHFloat` splits the float32's hex string **per character** — `"425C0000"` becomes 8
   bytes, one per hex digit, instead of 4. Every sensor frame is 4 bytes longer than its
   own hardcoded `0x0D` length field claims, so a conforming parser rejects it.
2. `generateCheckDigit` leaves a JS array hole, emitting a stray `0x00` between the
   checksum and the `0x16` terminator.
3. There is **no `displayOpen`** — only `displayClose`. A panel that powers up closed can
   never be opened by the app. (`L120UsbService` next door does have one.)

Only the runtime-built sensor frame is malformed; the hardcoded set-unit and close frames
are well-formed. 1.0.3 is the last release, so there is no fixed vendor build.

## Hardware facts

- USB `381c:0003` "SK SK700V", `bcdDevice 2.00`, **full-speed (12 Mbps)**, 1 config,
  1 HID interface, vendor usage page `0xFF00`, 500 mA. Panel is 480×480.
- Interrupt endpoints `0x01` OUT / `0x81` IN, 64 bytes, `bInterval` 10. The IN endpoint has
  never produced a byte.
- Report descriptor: `0x02`/`0x03`/`0x04` = Feature+Output 63 B, `0x05` = status,
  `0x10` = Output 63 B + Input 63 B.
- **Locate the hidraw node by VID/PID at runtime** (scan `/sys/class/hidraw/*/device/uevent`
  for `381C`/`0003` as 8-digit uppercase hex). It moves between boots — never hardcode
  `/dev/hidrawN`.
- Access comes from `packaging/99-sudokoo.rules` (`MODE="0660" GROUP="users"
  TAG+="uaccess"`), or root. The systemd unit runs as root because RAPL needs it.
- The device sometimes wedges off the bus after a warm reboot. A hub-port toggle
  (`recover-usb.sh`) sometimes helps; a cold PSU-off cycle reliably revives it. Under
  systemd, `Restart=always` covers the device disappearing and coming back.

## Sensor sources (Linux side)

Implemented in `src/sensors.rs`. Where this deviates from the Python driver, the
deviation is deliberate:

- **temp** — hwmon, chips tried `k10temp` → `zenpower` → `coretemp` and labels `Tctl` →
  `Tdie` → `Package id 0`, falling back to `temp1_input`. An explicitly requested label
  that does not exist returns nothing rather than substituting another sensor.
- **usage** — delta of the `cpu ` line of `/proc/stat`; busy = `total - idle - iowait`.
  Only the first eight fields are summed: `guest`/`guest_nice` are already counted inside
  `user`/`nice`, and the Python double-counts them.
- **freq** — mean of `/sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq`, kHz → MHz.
- **power** — RAPL `/sys/class/powercap/*/energy_uj` delta when readable, preferring the
  `package-0` zone over its `core`/`dram` subzones. `max_energy_range_uj` is used to
  handle the counter wrapping, which on this machine happens every ~65 kJ — under two
  minutes at load. Unreadable (mode 0400, so root-only) falls back to the estimate
  `TDP * (0.25 + 0.75 * usage)`; `Reading::power_measured` says which was used.

The measured-power path has only ever been exercised against fixtures — verifying it
needs a root run.

## Dead ends — do not re-attempt

- **Wine capture** — never exposes the device to the app's WinUSB/libusb backend; usbmon
  recorded 0 packets.
- **Windows capture VM** — QEMU's emulated xHCI BSODs on this full-speed device. Not needed
  anymore; the protocol is solved.
- **`hub.deepcool.com` firmware activation** — refuted, no backend round-trip is involved.
- **CRC16/MODBUS on reports `0x02`/`0x03`/`0x04`** — wrong checksum family entirely.
- **The `DCLd` libusb image path / L111 `Start`/`trans`/`DCLdfinish`** — that is the PID1
  family, a different device.
- **The `deepcool-digital-linux` digit format** (`0xAA` init + decimal digits) — VID
  `0x3633`/`0x34d3`, a different product line.

## Related devices

`Sk620SeriesUsbService` in the decompiled app is byte-for-byte the same protocol with the
same bugs, so the SK620 should work with an unchanged frame layer. Keep the framing and
frame builders free of SK700-specific assumptions so that stays cheap.
