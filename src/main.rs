mod audio;
mod bluez;
mod control;
mod hid;
mod pose;
mod render;
mod tracking;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    version,
    about = "Head-tracked spatial audio for PipeWire, using headphones that implement the Android Head Tracker HID protocol"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create the virtual 7.1 sink and render it binaurally (default).
    Run(RunArgs),
    /// Print the head pose reported by the tracker (no audio).
    Monitor(TrackerArgs),
    /// List head trackers.
    List,
    /// Make the current head direction the new front.
    Recenter,
    /// Toggle head tracking (off = speakers follow the head).
    Toggle,
    /// Turn head tracking on.
    On,
    /// Turn head tracking off.
    Off,
    /// Show the running instance's state.
    Status,
    /// Exit 0 if a hidraw device (sysfs path) is a head tracker; for udev.
    #[command(hide = true)]
    IsHeadTracker { syspath: PathBuf },
}

#[derive(Args, Clone)]
struct TrackerArgs {
    /// hidraw node (/dev/hidrawN) or Bluetooth address of the tracker.
    #[arg(long)]
    device: Option<String>,
    /// Requested report interval in milliseconds.
    #[arg(long, default_value_t = 20)]
    interval_ms: u32,
    /// Treat the tracker rotation as world->head instead of head->world
    /// (use if sounds move with your head instead of staying put).
    #[arg(long)]
    invert: bool,
    /// Let the front follow the head over this many seconds while it is
    /// still (compensates gyro drift); 0 = never.
    #[arg(long, default_value_t = 0.0)]
    follow_secs: f32,
    /// Do not ask BlueZ to connect the HID channel of headphones.
    #[arg(long)]
    no_connect_hid: bool,
}

#[derive(Args, Clone)]
struct RunArgs {
    #[command(flatten)]
    tracker: TrackerArgs,
    /// SOFA file with the HRTF set.
    #[arg(long, env = "SPATIAL_AUDIO_SOFA")]
    sofa: Option<PathBuf>,
    /// Processing sample rate.
    #[arg(long, default_value_t = 48000)]
    rate: u32,
    /// Convolution partition (and HRTF update) length in samples.
    #[arg(long, default_value_t = 128)]
    partition: usize,
    /// Azimuth of the front left/right speakers in degrees.
    #[arg(long, default_value_t = 30.0)]
    front_angle: f32,
    /// Output gain in dB.
    #[arg(long, default_value_t = -3.0, allow_hyphen_values = true)]
    gain_db: f32,
    /// LFE gain in dB (relative to the other channels).
    #[arg(long, default_value_t = -6.0, allow_hyphen_values = true)]
    lfe_gain_db: f32,
    /// Virtual speaker distance in metres (HRTF lookup).
    #[arg(long, default_value_t = 1.0)]
    distance: f32,
    /// Output node (node.name or object.serial). Default: the Bluetooth
    /// output of the headphones the tracker belongs to.
    #[arg(long)]
    target: Option<String>,
    /// node.name of the virtual sink.
    #[arg(long, default_value = "spatial_audio_sink")]
    sink_name: String,
    /// Description of the virtual sink shown in volume controls.
    #[arg(long, default_value = "Spatial Audio (Head Tracking)")]
    description: String,
}

fn default_sofa() -> Option<PathBuf> {
    let candidates = [
        option_env!("SPATIAL_AUDIO_DEFAULT_SOFA").map(PathBuf::from),
        Some(PathBuf::from(
            "/run/current-system/sw/share/libmysofa/default.sofa",
        )),
        Some(PathBuf::from("/usr/share/libmysofa/default.sofa")),
        Some(PathBuf::from("/usr/local/share/libmysofa/default.sofa")),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

fn tracking_options(args: &TrackerArgs, verbose: bool) -> tracking::TrackingOptions {
    tracking::TrackingOptions {
        device: args.device.clone(),
        interval_ms: args.interval_ms,
        invert: args.invert,
        follow_secs: args.follow_secs,
        connect_hid: !args.no_connect_hid,
        verbose,
    }
}

fn run(args: RunArgs) -> Result<()> {
    let sofa = match args.sofa.clone().or_else(default_sofa) {
        Some(p) => p,
        None => bail!("no SOFA file found; pass --sofa (e.g. libmysofa's default.sofa)"),
    };
    let speakers = render::layout_7_1(args.front_angle);
    let (scene, engine) = render::build(render::RenderConfig {
        sofa_path: &sofa,
        sample_rate: args.rate,
        partition_len: args.partition,
        speakers: speakers.clone(),
        gain: 10f32.powf(args.gain_db / 20.0),
        lfe_gain: 10f32.powf(args.lfe_gain_db / 20.0),
        distance: args.distance,
    })?;
    eprintln!("HRTF: {}", sofa.display());

    let shared = tracking::Shared::new();
    control::serve(shared.clone())?;
    let tracker = std::thread::Builder::new()
        .name("head-tracker".into())
        .spawn({
            let shared = shared.clone();
            let opts = tracking_options(&args.tracker, false);
            move || tracking::run(Some(scene), shared, opts)
        })?;

    let result = audio::run(
        engine,
        &speakers,
        audio::AudioOptions {
            sample_rate: args.rate,
            partition_len: args.partition,
            target: args.target,
            sink_name: args.sink_name,
            description: args.description,
        },
        shared.clone(),
    );
    shared.quit.store(true, Ordering::Relaxed);
    let _ = tracker.join();
    let _ = std::fs::remove_file(control::socket_path());
    result
}

fn monitor(args: TrackerArgs) -> Result<()> {
    let shared = tracking::Shared::new();
    for sig in [libc::SIGINT, libc::SIGTERM] {
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
    std::thread::spawn({
        let shared = shared.clone();
        move || loop {
            if QUIT.load(Ordering::Relaxed) {
                shared.quit.store(true, Ordering::Relaxed);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    tracking::run(None, shared, tracking_options(&args, true));
    Ok(())
}

static QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn on_signal(_: libc::c_int) {
    QUIT.store(true, Ordering::Relaxed);
}

fn list() {
    let trackers = hid::discover();
    if trackers.is_empty() {
        println!("no head trackers found");
    }
    for t in trackers {
        println!(
            "{}  {}{}",
            t.devnode.display(),
            t.name,
            t.address.map(|a| format!("  {a}")).unwrap_or_default()
        );
    }
    if let Ok(bluez) = bluez::Bluez::new()
        && let Ok(headsets) = bluez.headsets()
    {
        for h in headsets {
            println!("bluetooth: {} ({}) offers HID", h.name, h.address);
        }
    }
}

fn is_head_tracker(syspath: &Path) -> bool {
    hid::probe_sysfs(syspath).is_some()
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => run(RunArgs::parse_from(["run"]).clone()),
        Some(Command::Run(args)) => run(args),
        Some(Command::Monitor(args)) => monitor(args),
        Some(Command::List) => {
            list();
            Ok(())
        }
        Some(Command::IsHeadTracker { syspath }) => {
            return if is_head_tracker(&syspath) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Some(cmd) => {
            let word = match cmd {
                Command::Recenter => "recenter",
                Command::Toggle => "toggle",
                Command::On => "on",
                Command::Off => "off",
                _ => "status",
            };
            control::send(word)
                .map(|reply| println!("{reply}"))
                .context("control socket")
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

impl RunArgs {
    fn parse_from<const N: usize>(argv: [&str; N]) -> RunArgs {
        #[derive(Parser)]
        struct Only {
            #[command(flatten)]
            run: RunArgs,
        }
        Only::parse_from(argv).run
    }
}
