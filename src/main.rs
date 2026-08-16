//! Command line front end: parse options, drive the panel, shut it down cleanly.

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use sudokoo_linux::device::Device;
use sudokoo_linux::frame::{self, Unit};
use sudokoo_linux::sensors::{Config, Cpu, Reading};

const USAGE: &str = "\
sudokoo — drive the Sudokoo SK700V cooler panel

Usage: sudokoo [COMMAND] [OPTIONS]

Commands:
  stream    push live CPU stats until interrupted (default)
  once      open the panel and push a single live sample, then exit
  demo      push fixed, recognisable values for 30 seconds
  open      open the panel and exit
  close     blank the panel and exit

Options:
  -u, --unit <C|F>        temperature unit (default C)
  -i, --interval <SECS>   refresh period (default 1.0)
  -t, --tdp <WATTS>       CPU TDP; scales the power estimate and the load bar (default 65)
      --temp-label <NAME> hwmon sensor label (default: Tctl, Tdie, then Package id 0)
      --device <PATH>     hidraw node to use instead of searching by USB id
  -q, --quiet             print nothing
  -h, --help              show this text
  -V, --version           show the version

The panel blanks a few seconds after the last frame it accepted, so keep the refresh
interval well under that. Writing to the device needs the udev rule from packaging/,
or root.
";

/// How long `demo` holds its fixed values on screen.
const DEMO_RUNTIME: Duration = Duration::from_secs(30);
/// Longest single sleep while waiting for the next refresh. Sleeping the whole interval
/// in one call would delay shutdown by up to that interval, because the kernel resumes
/// an interrupted sleep rather than returning early.
const SLEEP_SLICE: Duration = Duration::from_millis(50);

fn main() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        // --help and --version already printed what they had to say.
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sudokoo: {message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match run(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sudokoo: {e}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Stream,
    Once,
    Demo,
    Open,
    Close,
}

#[derive(Debug, Clone)]
struct Options {
    command: Command,
    unit: Unit,
    interval: Duration,
    tdp: f32,
    temp_label: Option<String>,
    device: Option<String>,
    quiet: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            command: Command::Stream,
            unit: Unit::Celsius,
            interval: Duration::from_secs(1),
            tdp: 65.0,
            temp_label: None,
            device: None,
            quiet: false,
        }
    }
}

impl Options {
    /// `Ok(None)` means the arguments were handled entirely by printing something.
    fn parse(args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut options = Options::default();
        let mut command_seen = false;
        let mut args = args;

        while let Some(arg) = args.next() {
            let mut value = |flag: &str| -> Result<String, String> {
                args.next().ok_or_else(|| format!("{flag} needs a value"))
            };
            match arg.as_str() {
                "-h" | "--help" => {
                    print!("{USAGE}");
                    return Ok(None);
                }
                "-V" | "--version" => {
                    println!("sudokoo {}", env!("CARGO_PKG_VERSION"));
                    return Ok(None);
                }
                "-u" | "--unit" => options.unit = parse_unit(&value(&arg)?)?,
                "-i" | "--interval" => options.interval = parse_interval(&value(&arg)?)?,
                "-t" | "--tdp" => options.tdp = parse_tdp(&value(&arg)?)?,
                "--temp-label" => options.temp_label = Some(value(&arg)?),
                "--device" => options.device = Some(value(&arg)?),
                "-q" | "--quiet" => options.quiet = true,
                other if other.starts_with('-') => {
                    return Err(format!("unknown option {other}"));
                }
                other => {
                    if command_seen {
                        return Err(format!("unexpected argument {other}"));
                    }
                    options.command = parse_command(other)?;
                    command_seen = true;
                }
            }
        }
        Ok(Some(options))
    }
}

fn parse_command(name: &str) -> Result<Command, String> {
    match name {
        "stream" => Ok(Command::Stream),
        "once" => Ok(Command::Once),
        "demo" => Ok(Command::Demo),
        "open" => Ok(Command::Open),
        "close" => Ok(Command::Close),
        other => Err(format!("unknown command {other}")),
    }
}

fn parse_unit(value: &str) -> Result<Unit, String> {
    match value {
        "C" | "c" => Ok(Unit::Celsius),
        "F" | "f" => Ok(Unit::Fahrenheit),
        other => Err(format!("unit must be C or F, not {other}")),
    }
}

fn parse_interval(value: &str) -> Result<Duration, String> {
    let seconds: f64 = value
        .parse()
        .map_err(|_| format!("interval must be a number of seconds, not {value}"))?;
    if !(seconds.is_finite() && seconds > 0.0) {
        return Err(format!("interval must be greater than zero, not {value}"));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn parse_tdp(value: &str) -> Result<f32, String> {
    let watts: f32 = value
        .parse()
        .map_err(|_| format!("TDP must be a number of watts, not {value}"))?;
    if !(watts.is_finite() && watts > 0.0) {
        return Err(format!("TDP must be greater than zero, not {value}"));
    }
    Ok(watts)
}

fn run(options: &Options) -> Result<(), Box<dyn std::error::Error>> {
    let mut device = match &options.device {
        Some(path) => Device::open_path(path)?,
        None => Device::open()?,
    };

    let mut out = Output::new(options.quiet);
    let info = device.info();
    out.note(format_args!(
        "device: {} ({}{})",
        info.path.display(),
        if info.name.is_empty() {
            "unidentified"
        } else {
            &info.name
        },
        info.serial
            .as_deref()
            .map(|s| format!(", serial {s}"))
            .unwrap_or_default()
    ));

    match options.command {
        Command::Open => {
            device.send(&frame::open())?;
            out.note(format_args!(
                "panel opened; it blanks in a few seconds unless refreshed"
            ));
            return Ok(());
        }
        Command::Close => {
            device.send(&frame::close())?;
            out.note(format_args!("panel closed"));
            return Ok(());
        }
        Command::Stream | Command::Once | Command::Demo => {}
    }

    signals::install();
    device.send(&frame::open())?;
    device.send(&frame::set_unit(options.unit))?;

    if options.command == Command::Demo {
        return demo(&mut device, options, &mut out);
    }
    stream(&mut device, options, &mut out)
}

/// Push live samples until interrupted, or just one for `once`.
fn stream(
    device: &mut Device,
    options: &Options,
    out: &mut Output,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cpu = Cpu::new(Config {
        tdp: options.tdp,
        temp_label: options.temp_label.clone(),
        ..Config::default()
    });

    match cpu.temp_input() {
        Some(path) => out.note(format_args!("temperature: {}", path.display())),
        None => out.note(format_args!(
            "temperature: no CPU sensor found, the field will read 0"
        )),
    }
    match cpu.energy_path() {
        Some(path) => out.note(format_args!("power: measured, {}", path.display())),
        None => out.note(format_args!(
            "power: estimated from load, RAPL is not readable by this user"
        )),
    }

    // The first sample has no previous counters to difference against, so it is only
    // useful for priming them.
    cpu.sample();

    let mut next = Instant::now() + options.interval;
    while !signals::stopping() {
        sleep_until(next);
        if signals::stopping() {
            break;
        }
        next += options.interval;

        let reading = cpu.sample();
        device.send(&frame::sensor(
            &reading.to_frame(options.unit),
            options.unit,
        ))?;
        out.sample(&reading, options.unit);

        if options.command == Command::Once {
            // Leave the value on screen; the firmware blanks it a few seconds later.
            out.finish();
            return Ok(());
        }
    }

    out.finish();
    device.send(&frame::close())?;
    out.note(format_args!("panel closed"));
    Ok(())
}

/// Hold deliberately distinctive values, to confirm which field is which.
fn demo(
    device: &mut Device,
    options: &Options,
    out: &mut Output,
) -> Result<(), Box<dyn std::error::Error>> {
    let reading = Reading {
        temp_c: 73.5,
        usage: 88.0,
        power: 142.0,
        freq: 5300.0,
        ratio: 77.0,
        power_measured: false,
    };
    out.note(format_args!(
        "demo: 73.5 C / 88 % / 142 W / 5300 MHz for {} seconds",
        DEMO_RUNTIME.as_secs()
    ));

    let end = Instant::now() + DEMO_RUNTIME;
    while !signals::stopping() && Instant::now() < end {
        device.send(&frame::sensor(
            &reading.to_frame(options.unit),
            options.unit,
        ))?;
        sleep_until(Instant::now() + options.interval);
    }
    device.send(&frame::close())?;
    out.note(format_args!("panel closed"));
    Ok(())
}

/// Sleep until `deadline`, waking early if a signal arrives.
fn sleep_until(deadline: Instant) {
    while !signals::stopping() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        std::thread::sleep(remaining.min(SLEEP_SLICE));
    }
}

/// Progress reporting.
///
/// Per-sample output goes to a terminal only. Under systemd stdout is a pipe to the
/// journal, where a line per second would be noise rather than information, so only the
/// startup notes are logged there.
struct Output {
    quiet: bool,
    interactive: bool,
    wrote_sample: bool,
}

impl Output {
    fn new(quiet: bool) -> Self {
        Output {
            quiet,
            interactive: !quiet && std::io::stdout().is_terminal(),
            wrote_sample: false,
        }
    }

    fn note(&self, message: std::fmt::Arguments<'_>) {
        if !self.quiet {
            println!("{message}");
        }
    }

    fn sample(&mut self, reading: &Reading, unit: Unit) {
        if !self.interactive {
            return;
        }
        let temp = match unit {
            Unit::Celsius => reading.temp_c,
            Unit::Fahrenheit => reading.temp_c * 1.8 + 32.0,
        };
        let symbol = match unit {
            Unit::Celsius => 'C',
            Unit::Fahrenheit => 'F',
        };
        print!(
            "\r{temp:5.1}°{symbol}  {:3.0}%  {:4.0}W{}  {:5.0}MHz   ",
            reading.usage,
            reading.power,
            if reading.power_measured { ' ' } else { '~' },
            reading.freq
        );
        let _ = std::io::stdout().flush();
        self.wrote_sample = true;
    }

    /// Close off the in-place status line so later output starts on its own row.
    fn finish(&mut self) {
        if self.wrote_sample {
            println!();
            self.wrote_sample = false;
        }
    }
}

/// Termination handling, so the panel is blanked rather than left to time out.
mod signals {
    use std::sync::atomic::{AtomicBool, Ordering};

    static STOP: AtomicBool = AtomicBool::new(false);

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    // libc's signal(2), declared rather than pulled in as a dependency: the process
    // already links libc through std, and this is the only foreign function the crate
    // needs. The previous disposition it returns may be SIG_DFL, which is not a valid
    // function pointer, so the return type stays an integer.
    unsafe extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }

    /// Runs in signal context, so it may only touch an atomic.
    extern "C" fn on_signal(_signum: i32) {
        STOP.store(true, Ordering::Relaxed);
    }

    pub fn install() {
        // SAFETY: the handler does nothing but store to a static atomic, which is
        // async-signal-safe. Failures are ignored; the default disposition (terminate)
        // remains, which the firmware watchdog cleans up after.
        unsafe {
            signal(SIGINT, on_signal);
            signal(SIGTERM, on_signal);
        }
    }

    pub fn stopping() -> bool {
        STOP.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Options>, String> {
        Options::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_to_streaming_in_celsius_once_a_second() {
        let options = parse(&[]).unwrap().unwrap();
        assert_eq!(options.command, Command::Stream);
        assert_eq!(options.unit, Unit::Celsius);
        assert_eq!(options.interval, Duration::from_secs(1));
        assert_eq!(options.tdp, 65.0);
        assert!(!options.quiet);
    }

    #[test]
    fn options_parse_in_any_order_around_the_command() {
        let options = parse(&["--tdp", "105", "demo", "-u", "F", "-i", "0.5", "-q"])
            .unwrap()
            .unwrap();
        assert_eq!(options.command, Command::Demo);
        assert_eq!(options.unit, Unit::Fahrenheit);
        assert_eq!(options.interval, Duration::from_millis(500));
        assert_eq!(options.tdp, 105.0);
        assert!(options.quiet);
    }

    #[test]
    fn help_and_version_stop_before_touching_the_device() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["-V"]).unwrap().is_none());
        // Even alongside something that would otherwise run.
        assert!(parse(&["stream", "--help"]).unwrap().is_none());
    }

    #[test]
    fn bad_arguments_are_rejected_rather_than_defaulted() {
        assert!(parse(&["spin"]).is_err(), "unknown command");
        assert!(parse(&["--nonsense"]).is_err(), "unknown option");
        assert!(parse(&["open", "close"]).is_err(), "two commands");
        assert!(parse(&["--unit"]).is_err(), "missing value");
        assert!(parse(&["--unit", "K"]).is_err(), "not a unit");
        // A zero or negative interval would busy-loop or never refresh.
        assert!(parse(&["-i", "0"]).is_err());
        assert!(parse(&["-i", "-1"]).is_err());
        assert!(parse(&["-i", "soon"]).is_err());
        assert!(parse(&["--tdp", "0"]).is_err());
        assert!(parse(&["--tdp", "lots"]).is_err());
    }

    #[test]
    fn a_value_that_looks_like_a_flag_is_still_taken_as_the_value() {
        // `--device -q` should complain about the node, not silently enable quiet.
        let options = parse(&["--device", "-q"]).unwrap().unwrap();
        assert_eq!(options.device.as_deref(), Some("-q"));
        assert!(!options.quiet);
    }
}
