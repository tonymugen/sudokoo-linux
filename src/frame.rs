//! Wire frames for the Sudokoo SK700V panel.
//!
//! Every frame is a 64-byte HID output report: the report id `0x10` followed by a
//! DL/T-645-style message padded with zeros.
//!
//! ```text
//! 10 | 68 <addr=01> <ctrl=09> <len> <data ...> <cs> 16 | 00 ...
//!      len = number of data bytes following the length field
//!      cs  = sum(0x68 .. last data byte) & 0xFF        (a plain byte sum, not a CRC)
//! ```
//!
//! The first data byte selects the operation; see [`OP_DISPLAY`] and [`OP_SET_UNIT`].
//! Multi-byte integers are big-endian and the temperature is a raw big-endian IEEE-754
//! `f32` — not the per-hex-character expansion the vendor software emits, which is why
//! the vendor software cannot drive this device at all.

/// HID report id every frame is sent under.
pub const REPORT_ID: u8 = 0x10;
/// Device address. The panel is always 1; nothing else shares the bus.
pub const ADDR: u8 = 0x01;
/// Control byte. Constant across every frame the vendor software emits.
pub const CTRL: u8 = 0x09;
/// Frame start delimiter.
pub const START: u8 = 0x68;
/// Frame end delimiter.
pub const END: u8 = 0x16;

/// Every report is padded to the endpoint's 64-byte packet size.
pub const REPORT_LEN: usize = 64;
/// Report id, start, address, control, length, checksum and terminator.
const OVERHEAD: usize = 7;
/// Largest payload that still fits in one report. Far more than any frame needs.
pub const MAX_DATA: usize = REPORT_LEN - OVERHEAD;

/// Display operation. The second data byte then selects open, close or sensor report.
pub const OP_DISPLAY: u8 = 0x01;
/// Set the temperature unit shown on the panel.
pub const OP_SET_UNIT: u8 = 0x03;

const MODE_CLOSE: u8 = 0x00;
const MODE_OPEN: u8 = 0x01;
const MODE_REPORT: u8 = 0x02;

/// The 11-byte payload shared by the open and close frames.
///
/// Taken verbatim from the `displayClose` frame hardcoded in the vendor software. Its
/// fields are not decoded — likely brightness, rotation and the bar-graph ranges — and it
/// is passed through unchanged.
pub const PANEL_PAYLOAD: [u8; 11] = [
    0x00, 0x64, 0x32, 0x00, 0x42, 0x70, 0x00, 0x00, 0x28, 0x0E, 0x10,
];

/// Temperature unit the panel labels its reading with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Unit {
    #[default]
    Celsius,
    Fahrenheit,
}

impl Unit {
    /// The unit's wire encoding, used both by [`set_unit`] and inside a sensor frame.
    pub const fn as_byte(self) -> u8 {
        match self {
            Unit::Celsius => 0x00,
            Unit::Fahrenheit => 0x01,
        }
    }
}

/// The four values the panel displays, plus the undisplayed load bar.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Sensors {
    /// CPU temperature, already converted to the unit the frame is built with.
    pub temp: f32,
    /// CPU utilisation, percent.
    pub usage: u8,
    /// Package power, watts.
    pub power: u16,
    /// Core frequency, MHz.
    pub freq: u16,
    /// Power as a percentage of TDP. Believed to drive a bar graph; this is the one
    /// field never confirmed against the panel, because the vendor software's own
    /// formula yields 0 for every realistic input.
    pub ratio: u8,
}

/// One complete HID output report, padded and ready to write to the hidraw node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report([u8; REPORT_LEN]);

impl Report {
    /// The bytes to write to the device, report id first.
    pub const fn as_bytes(&self) -> &[u8; REPORT_LEN] {
        &self.0
    }
}

impl AsRef<[u8]> for Report {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Wrap `data` in the frame delimiters, checksum it and pad to a full report.
///
/// The payload size is checked at compile time, so no caller has a runtime error path.
fn build<const N: usize>(data: [u8; N]) -> Report {
    const { assert!(N <= MAX_DATA, "frame payload does not fit in one report") };

    let mut report = [0u8; REPORT_LEN];
    report[0] = REPORT_ID;
    report[1] = START;
    report[2] = ADDR;
    report[3] = CTRL;
    report[4] = N as u8;
    report[5..5 + N].copy_from_slice(&data);

    // The checksum covers the framing from START through the last data byte, but not
    // the report id, which never reaches the wire as part of the message.
    let checksum = report[1..5 + N]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b));
    report[5 + N] = checksum;
    report[6 + N] = END;

    Report(report)
}

/// Concatenate the display opcode, a mode byte and the opaque panel payload.
fn display_frame(mode: u8) -> Report {
    let mut data = [0u8; 2 + PANEL_PAYLOAD.len()];
    data[0] = OP_DISPLAY;
    data[1] = mode;
    data[2..].copy_from_slice(&PANEL_PAYLOAD);
    build(data)
}

/// Wake the panel. Must be sent once before any sensor report; the device powers up
/// closed and the vendor software has no equivalent command at all.
pub fn open() -> Report {
    display_frame(MODE_OPEN)
}

/// Blank the panel.
pub fn close() -> Report {
    display_frame(MODE_CLOSE)
}

/// Set the unit label. The temperature in a sensor frame is not converted by the device,
/// so the caller must convert it to match.
pub fn set_unit(unit: Unit) -> Report {
    build([OP_SET_UNIT, unit.as_byte()])
}

/// A sensor report. Send these at ~1 Hz: the firmware blanks the panel a few seconds
/// after the last frame it accepted.
pub fn sensor(s: &Sensors, unit: Unit) -> Report {
    let power = s.power.to_be_bytes();
    let temp = s.temp.to_be_bytes();
    let freq = s.freq.to_be_bytes();

    build([
        OP_DISPLAY,
        MODE_REPORT,
        power[0],
        power[1],
        s.ratio,
        unit.as_byte(),
        temp[0],
        temp[1],
        temp[2],
        temp[3],
        s.usage,
        freq[0],
        freq[1],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check the framing invariants that hold for every frame, and return the data
    /// bytes. A frame the device rejects is indistinguishable from silence, so these
    /// are the only checks available away from hardware.
    fn data_of(report: &Report) -> &[u8] {
        let b = report.as_bytes();
        assert_eq!(b.len(), REPORT_LEN, "reports are always a full packet");
        assert_eq!(b[0], REPORT_ID);
        assert_eq!(b[1], START);
        assert_eq!(b[2], ADDR);
        assert_eq!(b[3], CTRL);

        let len = b[4] as usize;
        let data = &b[5..5 + len];
        let sum = b[1..5 + len]
            .iter()
            .fold(0u8, |acc, &x| acc.wrapping_add(x));
        assert_eq!(b[5 + len], sum, "checksum is the byte sum from START");
        assert_eq!(b[6 + len], END);
        assert!(
            b[7 + len..].iter().all(|&x| x == 0),
            "everything past the terminator is padding"
        );
        data
    }

    /// The demo values confirmed against the panel on 2026-08-15.
    fn demo() -> Sensors {
        Sensors {
            temp: 55.0,
            usage: 50,
            power: 60,
            freq: 4200,
            ratio: 0,
        }
    }

    #[test]
    fn open_matches_known_good_frame() {
        let expected = [
            0x10, 0x68, 0x01, 0x09, 0x0d, 0x01, 0x01, 0x00, 0x64, 0x32, 0x00, 0x42, 0x70, 0x00,
            0x00, 0x28, 0x0e, 0x10, 0x0f, 0x16,
        ];
        assert_eq!(&open().as_bytes()[..expected.len()], expected);
    }

    #[test]
    fn close_matches_the_vendors_hardcoded_frame() {
        let expected = [
            0x10, 0x68, 0x01, 0x09, 0x0d, 0x01, 0x00, 0x00, 0x64, 0x32, 0x00, 0x42, 0x70, 0x00,
            0x00, 0x28, 0x0e, 0x10, 0x0e, 0x16,
        ];
        assert_eq!(&close().as_bytes()[..expected.len()], expected);
    }

    #[test]
    fn set_unit_matches_the_vendors_hardcoded_frames() {
        let celsius = [0x10, 0x68, 0x01, 0x09, 0x02, 0x03, 0x00, 0x77, 0x16];
        let fahrenheit = [0x10, 0x68, 0x01, 0x09, 0x02, 0x03, 0x01, 0x78, 0x16];
        assert_eq!(
            &set_unit(Unit::Celsius).as_bytes()[..celsius.len()],
            celsius
        );
        assert_eq!(
            &set_unit(Unit::Fahrenheit).as_bytes()[..fahrenheit.len()],
            fahrenheit
        );
    }

    #[test]
    fn sensor_matches_known_good_frame() {
        let expected = [
            0x10, 0x68, 0x01, 0x09, 0x0d, 0x01, 0x02, 0x00, 0x3c, 0x00, 0x00, 0x42, 0x5c, 0x00,
            0x00, 0x32, 0x10, 0x68, 0x06, 0x16,
        ];
        let frame = sensor(&demo(), Unit::Celsius);
        assert_eq!(&frame.as_bytes()[..expected.len()], expected);
    }

    #[test]
    fn every_frame_satisfies_the_framing_invariants() {
        for report in [
            open(),
            close(),
            set_unit(Unit::Celsius),
            set_unit(Unit::Fahrenheit),
            sensor(&demo(), Unit::Celsius),
            sensor(&Sensors::default(), Unit::Fahrenheit),
        ] {
            data_of(&report);
        }
    }

    #[test]
    fn length_field_counts_only_the_data_bytes() {
        // The vendor software hardcodes 0x0D while emitting 17 data bytes, which is why
        // its sensor frames are rejected. Ours must always agree with itself.
        assert_eq!(data_of(&sensor(&demo(), Unit::Celsius)).len(), 13);
        assert_eq!(data_of(&open()).len(), 13);
        assert_eq!(data_of(&close()).len(), 13);
        assert_eq!(data_of(&set_unit(Unit::Celsius)).len(), 2);
    }

    #[test]
    fn checksum_wraps_rather_than_saturating() {
        // Values chosen so the byte sum runs well past 0xFF several times over.
        let hot = Sensors {
            temp: -273.15,
            usage: 100,
            power: 65535,
            freq: 65535,
            ratio: 255,
        };
        let frame = sensor(&hot, Unit::Fahrenheit);
        let b = frame.as_bytes();
        let wide: u32 = b[1..18].iter().map(|&x| x as u32).sum();
        assert!(wide > 0xFF, "test is only meaningful if the sum overflows");
        assert_eq!(b[18], (wide & 0xFF) as u8);
        data_of(&frame);
    }

    #[test]
    fn temperature_is_a_big_endian_f32() {
        for temp in [55.0f32, 73.5, 0.0, -40.0, 212.0] {
            let frame = sensor(&Sensors { temp, ..demo() }, Unit::Celsius);
            let data = data_of(&frame);
            assert_eq!(&data[6..10], temp.to_be_bytes());
        }
    }

    #[test]
    fn sensor_fields_land_in_their_documented_positions() {
        let s = Sensors {
            temp: 73.5,
            usage: 88,
            power: 142,
            freq: 5300,
            ratio: 77,
        };
        let frame = sensor(&s, Unit::Fahrenheit);
        let data = data_of(&frame);

        assert_eq!(data[0], OP_DISPLAY);
        assert_eq!(data[1], MODE_REPORT);
        assert_eq!(u16::from_be_bytes([data[2], data[3]]), s.power);
        assert_eq!(data[4], s.ratio);
        assert_eq!(data[5], Unit::Fahrenheit.as_byte());
        assert_eq!(
            f32::from_be_bytes([data[6], data[7], data[8], data[9]]),
            s.temp
        );
        assert_eq!(data[10], s.usage);
        assert_eq!(u16::from_be_bytes([data[11], data[12]]), s.freq);
    }
}
