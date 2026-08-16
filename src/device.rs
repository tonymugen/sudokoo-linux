//! Finding the panel's hidraw node and writing frames to it.
//!
//! The node number is not stable — it depends on enumeration order and moves between
//! boots — so the device is always located by vendor and product id at runtime.
//!
//! Nothing here reads from the device. It never acknowledges a frame, never sends
//! anything unsolicited on its IN endpoint, and returns zeros to every feature request,
//! so a successful `write` means only that the kernel accepted the report. Whether the
//! firmware accepted it is visible solely on the panel.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::frame::{REPORT_LEN, Report};
use crate::trailing_number;

/// USB vendor id: Sudokoo.
pub const VID: u32 = 0x381C;
/// USB product id: SK700V. The same protocol serves the SK620 under a different id.
pub const PID: u32 = 0x0003;

const HIDRAW_CLASS: &str = "/sys/class/hidraw";

// Errno values that mean the device went away rather than rejecting the write. Rust
// gives these no stable `io::ErrorKind`, and this crate is Linux-only by construction.
const ENXIO: i32 = 6;
const EIO: i32 = 5;
const ENODEV: i32 = 19;
const EPIPE: i32 = 32;

/// What went wrong talking to the panel.
#[derive(Debug)]
pub enum Error {
    /// No hidraw node with the panel's vendor and product id.
    NotFound,
    /// The node exists but is not writable by this process.
    Access { path: PathBuf, source: io::Error },
    /// Any other failure opening or writing the node.
    Io { path: PathBuf, source: io::Error },
    /// The kernel took only part of a report. One write is one HID report, so a
    /// partial write cannot be resumed — the frame is lost.
    ShortWrite { path: PathBuf, wrote: usize },
}

impl Error {
    /// Whether this error means the device left the bus, as opposed to refusing the
    /// operation. The panel is prone to dropping off after a warm reboot, and a service
    /// should exit and let itself be restarted rather than spin on a dead handle.
    pub fn is_disconnected(&self) -> bool {
        match self {
            Error::NotFound => true,
            Error::Access { .. } | Error::ShortWrite { .. } => false,
            Error::Io { source, .. } => {
                matches!(source.raw_os_error(), Some(ENODEV | ENXIO | EIO | EPIPE))
            }
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => write!(
                f,
                "no SK700V ({VID:04x}:{PID:04x}) found — is usbhid bound to it?"
            ),
            Error::Access { path, .. } => write!(
                f,
                "no write access to {} — install packaging/99-sudokoo.rules, or run as root",
                path.display()
            ),
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Error::ShortWrite { path, wrote } => write!(
                f,
                "{}: wrote {wrote} of {REPORT_LEN} bytes; the frame was truncated",
                path.display()
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Access { source, .. } | Error::Io { source, .. } => Some(source),
            Error::NotFound | Error::ShortWrite { .. } => None,
        }
    }
}

/// A hidraw node that belongs to the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The character device to write to, e.g. `/dev/hidraw6`.
    pub path: PathBuf,
    /// `HID_NAME` as the kernel reports it, e.g. `SK SK700V`.
    pub name: String,
    /// `HID_UNIQ`, the USB serial number. Empty for many HID devices, but this one
    /// reports it, so it can disambiguate two panels on one machine.
    pub serial: Option<String>,
}

/// Every hidraw node matching the panel's vendor and product id, in node order.
///
/// Returns an empty vector rather than an error when no HID devices exist at all.
pub fn discover() -> Result<Vec<DeviceInfo>, Error> {
    let entries = match fs::read_dir(HIDRAW_CLASS) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                path: PathBuf::from(HIDRAW_CLASS),
                source,
            });
        }
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let node = entry.file_name();
        let Some(node) = node.to_str() else { continue };
        // A node that vanishes mid-scan, or one whose uevent is unreadable, is simply
        // not our device; scanning must not fail because of somebody else's hardware.
        let Ok(uevent) = fs::read_to_string(entry.path().join("device/uevent")) else {
            continue;
        };
        if hid_id(&uevent) != Some((VID, PID)) {
            continue;
        }
        found.push((
            trailing_number(node),
            DeviceInfo {
                path: PathBuf::from("/dev").join(node),
                name: uevent_value(&uevent, "HID_NAME")
                    .unwrap_or_default()
                    .to_owned(),
                serial: uevent_value(&uevent, "HID_UNIQ")
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            },
        ));
    }

    // Sort by node number, not by name: `hidraw10` sorts before `hidraw2` as a string.
    found.sort_by_key(|(number, info)| (*number, info.path.clone()));
    Ok(found.into_iter().map(|(_, info)| info).collect())
}

/// An open handle to the panel.
#[derive(Debug)]
pub struct Device {
    file: File,
    info: DeviceInfo,
}

impl Device {
    /// Open the first panel found.
    pub fn open() -> Result<Self, Error> {
        let info = discover()?.into_iter().next().ok_or(Error::NotFound)?;
        Self::from_info(info)
    }

    /// Open a specific node, for a machine with more than one panel or to override
    /// discovery. The node is not checked against the vendor and product id.
    pub fn open_path(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let info = discover()?
            .into_iter()
            .find(|i| i.path == path)
            .unwrap_or_else(|| DeviceInfo {
                path: path.to_path_buf(),
                name: String::new(),
                serial: None,
            });
        Self::from_info(info)
    }

    fn from_info(info: DeviceInfo) -> Result<Self, Error> {
        // Write-only: there is no read path to this device, so do not ask for one.
        let file =
            OpenOptions::new()
                .write(true)
                .open(&info.path)
                .map_err(|source| match source.kind() {
                    io::ErrorKind::PermissionDenied => Error::Access {
                        path: info.path.clone(),
                        source,
                    },
                    _ => Error::Io {
                        path: info.path.clone(),
                        source,
                    },
                })?;
        Ok(Device { file, info })
    }

    /// Which node this handle is on, and what the kernel calls it.
    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    /// Send one frame.
    ///
    /// One `write` is one HID report, so the whole 64-byte report goes in a single call
    /// and a partial write is an error rather than something to retry.
    pub fn send(&mut self, report: &Report) -> Result<(), Error> {
        let wrote = self
            .file
            .write(report.as_bytes())
            .map_err(|source| Error::Io {
                path: self.info.path.clone(),
                source,
            })?;
        if wrote != REPORT_LEN {
            return Err(Error::ShortWrite {
                path: self.info.path.clone(),
                wrote,
            });
        }
        Ok(())
    }
}

/// The value of `key` in a `uevent` file's `KEY=VALUE` lines.
fn uevent_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.trim_end())
}

/// The vendor and product id from a `HID_ID=<bus>:<vendor>:<product>` line, all hex.
fn hid_id(text: &str) -> Option<(u32, u32)> {
    let mut parts = uevent_value(text, "HID_ID")?.split(':');
    let _bus = parts.next()?;
    let vendor = u32::from_str_radix(parts.next()?, 16).ok()?;
    let product = u32::from_str_radix(parts.next()?, 16).ok()?;
    Some((vendor, product))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;

    /// The panel's own uevent, copied from hardware.
    const SK700V: &str = "\
DRIVER=hid-generic
HID_ID=0003:0000381C:00000003
HID_NAME=SK SK700V
HID_PHYS=usb-0000:11:00.0-2.4/input0
HID_UNIQ=0FCDD7A805A7
MODALIAS=hid:b0003g0001v0000381Cp00000003
";

    /// A keyboard that shares the machine, to check we do not match it.
    const KEYBOARD: &str = "\
DRIVER=hid-generic
HID_ID=0003:000024F0:00000105
HID_NAME=Das Keyboard Das Keyboard P13
HID_UNIQ=
MODALIAS=hid:b0003g0001v000024F0p00000105
";

    #[test]
    fn hid_id_identifies_the_panel() {
        assert_eq!(hid_id(SK700V), Some((VID, PID)));
        assert_eq!(hid_id(KEYBOARD), Some((0x24F0, 0x0105)));
        assert_eq!(hid_id("HID_NAME=nothing here\n"), None);
        assert_eq!(hid_id("HID_ID=0003:0000381C\n"), None);
        assert_eq!(hid_id("HID_ID=0003:zzzz:00000003\n"), None);
    }

    #[test]
    fn uevent_values_are_read_by_exact_key() {
        assert_eq!(uevent_value(SK700V, "HID_NAME"), Some("SK SK700V"));
        assert_eq!(uevent_value(SK700V, "HID_UNIQ"), Some("0FCDD7A805A7"));
        assert_eq!(uevent_value(KEYBOARD, "HID_UNIQ"), Some(""));
        assert_eq!(uevent_value(SK700V, "HID"), None);
        assert_eq!(uevent_value(SK700V, "ID"), None);
        assert_eq!(uevent_value(SK700V, "MISSING"), None);
    }

    #[test]
    fn missing_device_reports_not_found_and_reads_as_disconnected() {
        let err = Device::open_path("/dev/hidraw-does-not-exist").unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "got {err:?}");
        assert!(!err.is_disconnected(), "ENOENT is not a live disconnect");
        assert!(Error::NotFound.is_disconnected());
    }

    #[test]
    fn discovery_does_not_fail_on_a_real_machine() {
        // Whatever is plugged in, scanning must succeed and only ever return hidraw
        // nodes. `starts_with` on a Path compares whole components, so ask for the
        // parent and the file name separately.
        for info in discover().expect("scanning /sys/class/hidraw must not fail") {
            assert_eq!(info.path.parent(), Some(Path::new("/dev")), "{info:?}");
            let node = info.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            assert!(node.starts_with("hidraw"), "{info:?}");
        }
    }

    #[test]
    fn send_writes_one_whole_report_per_call() {
        let path = std::env::temp_dir().join(format!("sk700-send-{}", std::process::id()));
        File::create(&path).unwrap();
        let mut device = Device::open_path(&path).unwrap();

        device.send(&frame::open()).unwrap();
        device.send(&frame::close()).unwrap();

        let written = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(
            written.len(),
            2 * REPORT_LEN,
            "one report per send, unpadded"
        );
        assert_eq!(&written[..REPORT_LEN], frame::open().as_bytes());
        assert_eq!(&written[REPORT_LEN..], frame::close().as_bytes());
    }

    /// Lights the real panel. Run with `cargo test -- --ignored --nocapture`; the panel
    /// blanks a few seconds later since nothing refreshes it.
    #[test]
    #[ignore = "requires the panel to be plugged in"]
    fn hardware_accepts_an_open_and_a_sensor_frame() {
        let mut device = Device::open().expect("panel not found");
        println!("device: {:?}", device.info());

        device.send(&frame::open()).unwrap();
        device.send(&frame::set_unit(frame::Unit::Celsius)).unwrap();
        device
            .send(&frame::sensor(
                &frame::Sensors {
                    temp: 73.5,
                    usage: 88,
                    power: 142,
                    freq: 5300,
                    ratio: 77,
                },
                frame::Unit::Celsius,
            ))
            .unwrap();
        println!("sent: watch for 73.5 C / 88 % / 142 W / 5300 MHz");
    }
}
