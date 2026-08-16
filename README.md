# sudokoo-linux

A driver and `systemd` service for Sudokoo CPU fan screens.

Drives the **SK700V** (USB `381c:0003`) cooler's 480×480 status panel — FREQ, LOAD,
POWER and CPU TEMP — from live sysfs readings. No dependencies beyond the Rust standard
library.

The vendor's own MasterCraft software cannot drive this device on any platform: its
sensor frames are malformed, and it has no command to open a panel that powers up
closed. The protocol was recovered by decompiling it and probing the firmware; see
`CLAUDE.md` for the frame format and the details.

## Build

```sh
cargo build --release          # target/release/sudokoo
cargo test                     # unit tests, no hardware needed
cargo test --lib -- --ignored  # lights the real panel
```

## Use

```sh
sudokoo                        # stream live stats at 1 Hz until interrupted
sudokoo --unit F --tdp 105     # Fahrenheit, 105 W CPU
sudokoo once                   # open the panel and push a single sample
sudokoo demo                   # fixed values, to check the field mapping
sudokoo open / close           # just open or blank the panel
sudokoo --help                 # every option
```

`--tdp` should match the CPU: it scales the power estimate and the panel's load bar.

The panel blanks a few seconds after the last frame the firmware accepted, so `stream`
must keep refreshing — the default 1 s interval is well inside that. `open` and `once`
deliberately leave the panel to blank on its own.

## Install

```sh
sudo install -m 0755 target/release/sudokoo /usr/local/bin/
sudo install -m 0644 packaging/sudokoo.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now sudokoo
```

Edit `--tdp` in the unit to match the CPU before enabling it.

The service runs as root, which is what makes package power *measured* rather than
estimated: `/sys/class/powercap/*/energy_uj` is mode 0400. Everything else in the unit
narrows what that root process can reach — no network, no capabilities, a read-only
filesystem, and only the `hidraw` device class.

To run it as an ordinary user instead, install the udev rule and drop the privileged
unit:

```sh
sudo install -m 0644 packaging/99-sudokoo.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger -s hidraw
```

Power is then estimated from load and TDP rather than measured. Everything else is
identical.

## Notes

- The hidraw node moves between boots; it is always located by USB id at runtime.
  `--device` overrides that.
- The device sometimes drops off the USB bus after a warm reboot. `Restart=always`
  covers it coming back; a cold power cycle is what reliably revives it if it does not.
- Temperature comes from `k10temp`, `zenpower` or `coretemp`, preferring the package
  sensor. `--temp-label` picks a specific one.

## Licence

BSD 3-Clause. See `LICENSE`.
