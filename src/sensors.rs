//! Live CPU statistics for the panel's four fields, read from sysfs and procfs.
//!
//! Every field degrades to zero rather than failing. A missing sensor is a blank spot on
//! a fan display, not a reason to stop driving the panel — and the panel blanks itself a
//! few seconds after the last frame, so giving up would take the whole display down.
//!
//! The paths are injectable through [`Config`] so the discovery logic can be tested
//! against a fixture tree instead of whatever happens to be plugged into the machine.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::frame::{self, Unit};
use crate::trailing_number;

/// hwmon chips that expose a CPU package temperature, most specific first.
const TEMP_CHIPS: [&str; 3] = ["k10temp", "zenpower", "coretemp"];
/// Sensor labels naming the package temperature: AMD's two, then Intel's.
const TEMP_LABELS: [&str; 3] = ["Tctl", "Tdie", "Package id 0"];

/// Where to read from, and what the CPU is rated at.
#[derive(Debug, Clone)]
pub struct Config {
    /// CPU TDP in watts. Sets the power estimate's ceiling and scales the load bar.
    pub tdp: f32,
    /// hwmon sensor label to prefer, e.g. `Tccd1`. Defaults to [`TEMP_LABELS`] in order.
    pub temp_label: Option<String>,
    /// sysfs mount point.
    pub sysfs: PathBuf,
    /// The kernel's aggregate CPU time counters.
    pub proc_stat: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            tdp: 65.0,
            temp_label: None,
            sysfs: PathBuf::from("/sys"),
            proc_stat: PathBuf::from("/proc/stat"),
        }
    }
}

/// One sample. Temperature is always Celsius here; conversion happens in
/// [`Reading::to_frame`], because the panel displays whatever number it is given.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// Package temperature, degrees Celsius.
    pub temp_c: f32,
    /// CPU utilisation since the previous sample, percent.
    pub usage: f32,
    /// Package power, watts.
    pub power: f32,
    /// Mean core frequency, MHz.
    pub freq: f32,
    /// Power as a percentage of TDP.
    pub ratio: f32,
    /// Whether `power` came from RAPL or from the load-based estimate.
    pub power_measured: bool,
}

impl Reading {
    /// Convert to the wire representation, rounding and saturating to the field widths.
    pub fn to_frame(&self, unit: Unit) -> frame::Sensors {
        let temp = match unit {
            Unit::Celsius => self.temp_c,
            Unit::Fahrenheit => self.temp_c * 1.8 + 32.0,
        };
        // Float-to-integer `as` casts saturate at the target's bounds and turn NaN into
        // zero, which is exactly the degrade-to-zero behaviour wanted here. The
        // temperature is a float on the wire, so it needs the NaN check spelled out.
        frame::Sensors {
            temp: if temp.is_finite() { temp } else { 0.0 },
            usage: self.usage.round() as u8,
            power: self.power.round() as u16,
            freq: self.freq.round() as u16,
            ratio: self.ratio.round() as u8,
        }
    }
}

/// A RAPL energy counter and the value at which it wraps.
#[derive(Debug, Clone)]
struct EnergyZone {
    path: PathBuf,
    max: Option<u64>,
}

/// Samples the CPU, holding the counters that only make sense as differences.
#[derive(Debug)]
pub struct Cpu {
    config: Config,
    temp_input: Option<PathBuf>,
    energy: Option<EnergyZone>,
    prev_cpu: Option<(u64, u64)>,
    prev_energy: Option<(u64, Instant)>,
}

impl Cpu {
    /// Locate the sensors. Discovery happens once; sampling afterwards is only reads.
    pub fn new(config: Config) -> Self {
        let temp_input = find_temp_input(&config.sysfs, config.temp_label.as_deref());
        let energy = find_energy_zone(&config.sysfs);
        Cpu {
            config,
            temp_input,
            energy,
            prev_cpu: None,
            prev_energy: None,
        }
    }

    /// The temperature sensor in use, if one was found.
    pub fn temp_input(&self) -> Option<&Path> {
        self.temp_input.as_deref()
    }

    /// The RAPL counter in use. `None` means power will be estimated — the counters are
    /// root-readable only on most distributions, so this is the common case outside the
    /// systemd unit.
    pub fn energy_path(&self) -> Option<&Path> {
        self.energy.as_ref().map(|z| z.path.as_path())
    }

    /// Take a sample.
    ///
    /// Utilisation and power are differences against the previous call, so the first
    /// sample reports zero usage and an estimated power. Prime the deltas by sampling
    /// once and discarding the result.
    pub fn sample(&mut self) -> Reading {
        let usage = self.usage();
        let power = self.power(usage);
        Reading {
            temp_c: self.temperature(),
            usage,
            power: power.0,
            freq: self.frequency(),
            ratio: if self.config.tdp > 0.0 {
                (100.0 * power.0 / self.config.tdp).clamp(0.0, 100.0)
            } else {
                0.0
            },
            power_measured: power.1,
        }
    }

    fn temperature(&self) -> f32 {
        let Some(path) = &self.temp_input else {
            return 0.0;
        };
        read_number::<i64>(path).map_or(0.0, |milli| milli as f32 / 1000.0)
    }

    fn usage(&mut self) -> f32 {
        let Some(cur) = fs::read_to_string(&self.config.proc_stat)
            .ok()
            .and_then(|s| parse_cpu_times(&s))
        else {
            return 0.0;
        };
        let usage = match self.prev_cpu {
            Some(prev) => usage_between(prev, cur),
            None => 0.0,
        };
        self.prev_cpu = Some(cur);
        usage
    }

    fn frequency(&self) -> f32 {
        let cpu_dir = self.config.sysfs.join("devices/system/cpu");
        let Ok(entries) = fs::read_dir(&cpu_dir) else {
            return 0.0;
        };

        let mut total_khz = 0u64;
        let mut cores = 0u32;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // `cpufreq` and `cpuidle` also live here; only `cpu<N>` are cores.
            if !name.starts_with("cpu") || !name[3..].bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            if let Some(khz) = read_number::<u64>(&entry.path().join("cpufreq/scaling_cur_freq")) {
                total_khz += khz;
                cores += 1;
            }
        }
        if cores == 0 {
            return 0.0;
        }
        total_khz as f32 / cores as f32 / 1000.0
    }

    /// Package watts, and whether they were measured rather than estimated.
    fn power(&mut self, usage: f32) -> (f32, bool) {
        if let Some(watts) = self.rapl_power() {
            return (watts, true);
        }
        (estimate_power(self.config.tdp, usage), false)
    }

    fn rapl_power(&mut self) -> Option<f32> {
        let zone = self.energy.as_ref()?;
        let now_uj = read_number::<u64>(&zone.path)?;
        let now = Instant::now();

        let watts = match self.prev_energy {
            Some((prev_uj, prev_at)) => {
                let elapsed = now.duration_since(prev_at).as_secs_f32();
                let delta = energy_delta(prev_uj, now_uj, zone.max);
                match (delta, elapsed > 0.0) {
                    (Some(delta), true) => Some(delta as f32 / 1e6 / elapsed),
                    _ => None,
                }
            }
            None => None,
        };
        self.prev_energy = Some((now_uj, now));
        watts
    }
}

/// Busy and total jiffies from the aggregate `cpu` line of `/proc/stat`.
///
/// `guest` and `guest_nice` are already counted inside `user` and `nice`, so only the
/// first eight fields are summed; including them would inflate the total and understate
/// utilisation on a host running virtual machines.
fn parse_cpu_times(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|f| f.parse().unwrap_or(0))
        .collect();
    if fields.len() < 5 {
        return None;
    }
    let total: u64 = fields.iter().sum();
    let idle = fields[3] + fields[4]; // idle + iowait
    Some((total.saturating_sub(idle), total))
}

/// Utilisation between two `(busy, total)` samples, as a percentage.
fn usage_between(prev: (u64, u64), cur: (u64, u64)) -> f32 {
    let busy = cur.0.saturating_sub(prev.0) as f32;
    let total = cur.1.saturating_sub(prev.1) as f32;
    if total <= 0.0 {
        return 0.0;
    }
    (100.0 * busy / total).clamp(0.0, 100.0)
}

/// Microjoules consumed between two counter readings, accounting for wraparound.
///
/// RAPL counters are narrow enough to wrap in normal use — this machine's package zone
/// wraps every ~65 kJ, which is under two minutes at full load.
fn energy_delta(prev: u64, now: u64, max: Option<u64>) -> Option<u64> {
    if now >= prev {
        return Some(now - prev);
    }
    // Without a known wrap point the counter was probably reset; drop the sample rather
    // than report a wild figure.
    max.map(|max| max.saturating_sub(prev) + now)
}

/// Plausible package watts from load alone. Not measured — RAPL is root-only on most
/// distributions, so an unprivileged run has nothing better to offer.
fn estimate_power(tdp: f32, usage: f32) -> f32 {
    (tdp * (0.25 + 0.75 * usage.clamp(0.0, 100.0) / 100.0)).max(0.0)
}

/// The hwmon `tempN_input` for the CPU package.
///
/// Chips are tried in [`TEMP_CHIPS`] order so an AMD sensor wins over a stray
/// `coretemp` regardless of hwmon numbering, and within a chip the labelled sensor wins
/// over `temp1_input`.
fn find_temp_input(sysfs: &Path, label: Option<&str>) -> Option<PathBuf> {
    let chips = hwmon_chips(&sysfs.join("class/hwmon"));
    let labels: Vec<&str> = match label {
        Some(label) => vec![label],
        None => TEMP_LABELS.to_vec(),
    };

    for chip in TEMP_CHIPS {
        for dir in chips
            .iter()
            .filter(|(_, name)| name == chip)
            .map(|(p, _)| p)
        {
            for wanted in &labels {
                if let Some(path) = labelled_temp_input(dir, wanted) {
                    return Some(path);
                }
            }
            // An explicit label was asked for and this chip does not have it; do not
            // silently substitute a different sensor.
            let fallback = dir.join("temp1_input");
            if label.is_none() && fallback.exists() {
                return Some(fallback);
            }
        }
    }
    None
}

/// Every hwmon directory with its chip name, in numeric node order.
fn hwmon_chips(hwmon: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = fs::read_dir(hwmon) else {
        return Vec::new();
    };
    let mut chips: Vec<(u32, PathBuf, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = fs::read_to_string(entry.path().join("name")).ok()?;
            let node = trailing_number(entry.file_name().to_str()?);
            Some((node, entry.path(), name.trim().to_owned()))
        })
        .collect();
    chips.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    chips.into_iter().map(|(_, p, n)| (p, n)).collect()
}

/// The `tempN_input` whose `tempN_label` reads `wanted`.
fn labelled_temp_input(chip: &Path, wanted: &str) -> Option<PathBuf> {
    let mut matches: Vec<(u32, PathBuf)> = fs::read_dir(chip)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let file = entry.file_name();
            let file = file.to_str()?;
            let index = file.strip_prefix("temp")?.strip_suffix("_label")?;
            if fs::read_to_string(entry.path()).ok()?.trim() != wanted {
                return None;
            }
            let input = chip.join(format!("temp{index}_input"));
            input
                .exists()
                .then(|| (index.parse().unwrap_or(u32::MAX), input))
        })
        .collect();
    matches.sort();
    matches.into_iter().next().map(|(_, path)| path)
}

/// The best readable RAPL energy counter.
///
/// Package zones are preferred over their `core` and `dram` subzones, which measure only
/// part of the package. Unreadable zones are skipped: the counters are root-only on most
/// distributions, and a zone that cannot be read is no better than no zone at all.
fn find_energy_zone(sysfs: &Path) -> Option<EnergyZone> {
    let Ok(entries) = fs::read_dir(sysfs.join("class/powercap")) else {
        return None;
    };

    let mut zones: Vec<(u8, PathBuf, Option<u64>)> = entries
        .flatten()
        .filter_map(|entry| {
            let energy = entry.path().join("energy_uj");
            // Reading is the only honest readability test: `energy_uj` is commonly
            // present but mode 0400, and permissions can differ per zone.
            read_number::<u64>(&energy)?;
            let name = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
            let rank = match name.trim() {
                "package-0" => 0,
                n if n.starts_with("package") => 1,
                _ => 2,
            };
            let max = read_number::<u64>(&entry.path().join("max_energy_range_uj"));
            Some((rank, energy, max))
        })
        .collect();
    zones.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    zones
        .into_iter()
        .next()
        .map(|(_, path, max)| EnergyZone { path, max })
}

/// Read a sysfs file holding one number. Any failure is `None`; callers substitute zero.
fn read_number<T: std::str::FromStr>(path: &Path) -> Option<T> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A throwaway directory tree, so discovery can be tested against known layouts.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let root = std::env::temp_dir().join(format!(
                "sk700-sensors-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Fixture(root)
        }

        /// Create `rel` and everything above it, holding `contents`.
        fn write(&self, rel: &str, contents: &str) -> &Self {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
            self
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// This machine's actual k10temp layout: Tctl on temp1, Tccd1 on temp3.
    fn amd_hwmon(fixture: &Fixture) {
        fixture
            .write("class/hwmon/hwmon0/name", "acpitz_0\n")
            .write("class/hwmon/hwmon6/name", "k10temp\n")
            .write("class/hwmon/hwmon6/temp1_label", "Tctl\n")
            .write("class/hwmon/hwmon6/temp1_input", "52125\n")
            .write("class/hwmon/hwmon6/temp3_label", "Tccd1\n")
            .write("class/hwmon/hwmon6/temp3_input", "48000\n");
    }

    #[test]
    fn temp_discovery_prefers_the_package_label() {
        let f = Fixture::new();
        amd_hwmon(&f);
        assert_eq!(
            find_temp_input(&f.0, None),
            Some(f.path("class/hwmon/hwmon6/temp1_input"))
        );
        assert_eq!(
            find_temp_input(&f.0, Some("Tccd1")),
            Some(f.path("class/hwmon/hwmon6/temp3_input"))
        );
    }

    #[test]
    fn temp_discovery_prefers_amd_over_intel_regardless_of_hwmon_number() {
        let f = Fixture::new();
        f.write("class/hwmon/hwmon1/name", "coretemp\n")
            .write("class/hwmon/hwmon1/temp1_label", "Package id 0\n")
            .write("class/hwmon/hwmon1/temp1_input", "40000\n")
            .write("class/hwmon/hwmon9/name", "k10temp\n")
            .write("class/hwmon/hwmon9/temp1_label", "Tctl\n")
            .write("class/hwmon/hwmon9/temp1_input", "50000\n");
        assert_eq!(
            find_temp_input(&f.0, None),
            Some(f.path("class/hwmon/hwmon9/temp1_input"))
        );
    }

    #[test]
    fn temp_discovery_falls_back_to_temp1_but_never_past_an_explicit_label() {
        let f = Fixture::new();
        f.write("class/hwmon/hwmon0/name", "k10temp\n")
            .write("class/hwmon/hwmon0/temp1_input", "45000\n");
        assert_eq!(
            find_temp_input(&f.0, None),
            Some(f.path("class/hwmon/hwmon0/temp1_input"))
        );
        // Asking for a label that does not exist must fail rather than quietly return
        // some other sensor's reading.
        assert_eq!(find_temp_input(&f.0, Some("Tdie")), None);
        assert_eq!(find_temp_input(&Fixture::new().0, None), None);
    }

    #[test]
    fn energy_zone_prefers_the_package_over_its_subzones() {
        let f = Fixture::new();
        f.write("class/powercap/intel-rapl/enabled", "1\n")
            .write("class/powercap/intel-rapl:0/name", "package-0\n")
            .write("class/powercap/intel-rapl:0/energy_uj", "1234\n")
            .write(
                "class/powercap/intel-rapl:0/max_energy_range_uj",
                "65532610987\n",
            )
            .write("class/powercap/intel-rapl:0:0/name", "core\n")
            .write("class/powercap/intel-rapl:0:0/energy_uj", "99\n");

        let zone = find_energy_zone(&f.0).expect("a readable package zone");
        assert_eq!(zone.path, f.path("class/powercap/intel-rapl:0/energy_uj"));
        assert_eq!(zone.max, Some(65_532_610_987));
        // A tree with no energy counters at all, as when RAPL is root-only.
        assert!(find_energy_zone(&Fixture::new().0).is_none());
    }

    #[test]
    fn cpu_times_ignore_guest_which_is_already_inside_user() {
        // The `cpu` aggregate line, not `cpu0`.
        let stat = "cpu  100 20 30 700 50 0 0 0 900 10\ncpu0 1 1 1 1 1 0 0 0 0 0\n";
        let (busy, total) = parse_cpu_times(stat).unwrap();
        assert_eq!(total, 100 + 20 + 30 + 700 + 50, "guest must not be summed");
        assert_eq!(busy, 150, "idle and iowait are not busy");
        assert_eq!(parse_cpu_times("intr 1 2 3\n"), None);
    }

    #[test]
    fn usage_is_the_busy_share_of_elapsed_jiffies() {
        assert_eq!(usage_between((0, 0), (25, 100)), 25.0);
        assert_eq!(usage_between((100, 400), (200, 500)), 100.0);
        // A counter that did not move, or went backwards across a suspend.
        assert_eq!(usage_between((10, 100), (10, 100)), 0.0);
        assert_eq!(usage_between((10, 100), (5, 50)), 0.0);
    }

    #[test]
    fn energy_delta_handles_the_counter_wrapping() {
        assert_eq!(energy_delta(100, 500, Some(1000)), Some(400));
        // Wrapped: 900 -> 1000, then 0 -> 50.
        assert_eq!(energy_delta(900, 50, Some(1000)), Some(150));
        // Went backwards with no known wrap point: a reset, so drop the sample.
        assert_eq!(energy_delta(900, 50, None), None);
    }

    #[test]
    fn power_estimate_spans_a_quarter_of_tdp_to_all_of_it() {
        assert_eq!(estimate_power(100.0, 0.0), 25.0);
        assert_eq!(estimate_power(100.0, 100.0), 100.0);
        assert_eq!(estimate_power(100.0, 50.0), 62.5);
        assert_eq!(estimate_power(0.0, 100.0), 0.0);
    }

    #[test]
    fn readings_round_and_saturate_into_the_wire_fields() {
        let reading = Reading {
            temp_c: 73.5,
            usage: 87.6,
            power: 142.4,
            freq: 5300.0,
            ratio: 77.0,
            power_measured: true,
        };
        let wire = reading.to_frame(Unit::Celsius);
        assert_eq!(wire.temp, 73.5);
        assert_eq!(wire.usage, 88, "rounded, not truncated");
        assert_eq!(wire.power, 142);
        assert_eq!(wire.freq, 5300);

        let fahrenheit = reading.to_frame(Unit::Fahrenheit);
        assert_eq!(fahrenheit.temp, 164.3);

        // Nonsense from a misbehaving sensor must not become a nonsense frame.
        let broken = Reading {
            temp_c: f32::NAN,
            usage: 500.0,
            power: 1e9,
            freq: -1.0,
            ratio: f32::NAN,
            power_measured: false,
        };
        let wire = broken.to_frame(Unit::Celsius);
        assert_eq!(wire.temp, 0.0);
        assert_eq!(wire.usage, 255, "saturates at the field width");
        assert_eq!(wire.power, u16::MAX);
        assert_eq!(wire.freq, 0);
        assert_eq!(wire.ratio, 0, "NaN casts to zero");
    }

    #[test]
    fn sampling_a_fixture_tree_reads_every_field() {
        let f = Fixture::new();
        amd_hwmon(&f);
        f.write(
            "devices/system/cpu/cpu0/cpufreq/scaling_cur_freq",
            "4000000\n",
        )
        .write(
            "devices/system/cpu/cpu1/cpufreq/scaling_cur_freq",
            "5000000\n",
        )
        .write("devices/system/cpu/cpufreq/boost", "1\n")
        .write("proc/stat", "cpu  100 0 100 800 0 0 0 0\n");

        let config = Config {
            tdp: 100.0,
            temp_label: None,
            sysfs: f.0.clone(),
            proc_stat: f.path("proc/stat"),
        };
        let mut cpu = Cpu::new(config);

        let first = cpu.sample();
        assert_eq!(first.temp_c, 52.125);
        assert_eq!(first.freq, 4500.0, "mean of the cores, kHz to MHz");
        assert_eq!(first.usage, 0.0, "no previous sample to difference against");
        assert!(
            !first.power_measured,
            "no readable RAPL zone in the fixture"
        );
        assert_eq!(first.power, 25.0, "idle estimate is a quarter of TDP");
        assert_eq!(first.ratio, 25.0);

        // 200 more busy jiffies out of 400 elapsed.
        f.write("proc/stat", "cpu  200 0 200 900 100 0 0 0\n");
        let second = cpu.sample();
        assert_eq!(second.usage, 50.0);
        assert_eq!(second.power, 62.5);
    }

    #[test]
    fn missing_sensors_read_as_zero_rather_than_failing() {
        let f = Fixture::new();
        let mut cpu = Cpu::new(Config {
            tdp: 0.0,
            temp_label: None,
            sysfs: f.path("nothing/here"),
            proc_stat: f.path("nothing/here/stat"),
        });
        let reading = cpu.sample();
        assert_eq!(reading.temp_c, 0.0);
        assert_eq!(reading.usage, 0.0);
        assert_eq!(reading.freq, 0.0);
        assert_eq!(reading.power, 0.0);
        assert_eq!(reading.ratio, 0.0, "a zero TDP must not divide by zero");
        assert_eq!(cpu.temp_input(), None);
        assert_eq!(cpu.energy_path(), None);
    }

    #[test]
    fn the_real_machine_is_readable() {
        let mut cpu = Cpu::new(Config::default());
        let reading = cpu.sample();
        println!("{:?}\n{reading:?}", cpu.temp_input());
        assert!(
            (0.0..=150.0).contains(&reading.temp_c),
            "implausible temperature {reading:?}"
        );
        assert!(reading.freq >= 0.0 && reading.power >= 0.0, "{reading:?}");
    }
}
