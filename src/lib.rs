//! Driver for the Sudokoo SK700V cooler panel (USB 381c:0003).
//!
//! The device is write-only in practice: it never acknowledges a frame, never sends
//! anything unsolicited, and returns zeros to every feature request. A rejected frame is
//! indistinguishable from silence — the only feedback is whether the panel stays lit.

pub mod device;
pub mod frame;
pub mod sensors;

/// The number at the end of a `hidraw6` or `hwmon10` style name, for ordering.
///
/// Sorting those names as strings puts `hidraw10` before `hidraw2`, which silently
/// picks the wrong device on a machine with ten or more of them.
pub(crate) fn trailing_number(name: &str) -> u32 {
    name.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::trailing_number;

    #[test]
    fn names_order_numerically_not_lexically() {
        let mut names = ["hidraw10", "hidraw2", "hidraw1"];
        names.sort_by_key(|n| trailing_number(n));
        assert_eq!(names, ["hidraw1", "hidraw2", "hidraw10"]);
        assert_eq!(trailing_number("hwmon6"), 6);
        assert_eq!(trailing_number("nonsense"), u32::MAX);
    }
}
