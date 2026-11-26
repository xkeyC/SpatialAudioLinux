//! Head tracking thread: finds a tracker, reads it and drives the scene.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bluez::Bluez;
use crate::hid::{self, Tracker};
use crate::pose::{HeadPose, Quat, angular_speed};

/// Window over which the rotation speed is measured.
const SPEED_WINDOW: Duration = Duration::from_secs(1);
/// Below this rotation speed (rad/s) the head counts as still. The Sony
/// WF-1000XM5 drifts at ~0.05 rad/s while converging; its jitter over a 1 s
/// window is well below 0.01 rad/s.
const SETTLED_SPEED: f32 = 0.02;
/// How long the orientation must stay still before the first recentring.
const SETTLE_TIME: Duration = Duration::from_millis(1500);
/// Recentre anyway after this long, even if the head keeps moving.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);
use crate::render::Scene;

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub device: Option<String>,
    pub devnode: Option<String>,
    pub address: Option<String>,
    pub protocol: Option<String>,
    /// Apparent direction of a source straight ahead (azimuth left-positive).
    pub front_azimuth: f32,
    pub front_elevation: f32,
    pub samples: u64,
}

pub struct Shared {
    pub recenter: AtomicBool,
    pub tracking: AtomicBool,
    pub quit: AtomicBool,
    pub status: Mutex<Status>,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            recenter: AtomicBool::new(false),
            tracking: AtomicBool::new(true),
            quit: AtomicBool::new(false),
            status: Mutex::new(Status::default()),
        })
    }
}

#[derive(Clone, Debug)]
pub struct TrackingOptions {
    /// hidraw node or Bluetooth address to use; any tracker if `None`.
    pub device: Option<String>,
    pub interval_ms: u32,
    pub invert: bool,
    /// Seconds over which the front slowly follows the head while it is
    /// still; 0 disables.
    pub follow_secs: f32,
    pub connect_hid: bool,
    /// Print the pose for every sample.
    pub verbose: bool,
}

fn matches(info: &hid::DeviceInfo, wanted: &str) -> bool {
    info.devnode.to_string_lossy() == wanted
        || info
            .address
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(wanted))
}

pub fn run(mut scene: Option<Scene>, shared: Arc<Shared>, opts: TrackingOptions) {
    let mut pose = HeadPose::new(opts.invert);
    let bluez = if opts.connect_hid {
        Bluez::new()
            .inspect_err(|e| {
                eprintln!("BlueZ unavailable, HID channels will not be connected: {e}")
            })
            .ok()
    } else {
        None
    };
    // Last HID connect attempt per Bluetooth address.
    let mut attempts: HashMap<String, Instant> = HashMap::new();
    let mut open_errors: HashMap<String, String> = HashMap::new();
    let mut idle_published = false;

    while !shared.quit.load(Ordering::Relaxed) {
        let candidates: Vec<_> = hid::discover()
            .into_iter()
            .filter(|d| opts.device.as_deref().is_none_or(|w| matches(d, w)))
            .collect();

        if candidates.is_empty() {
            if !idle_published {
                pose.reset();
                if let Some(scene) = scene.as_mut() {
                    scene.update(Quat::IDENTITY);
                }
                *shared.status.lock().unwrap() = Status::default();
                eprintln!("waiting for a head tracker...");
                idle_published = true;
            }
            if let Some(bluez) = &bluez {
                connect_headsets(bluez, &mut attempts, opts.device.as_deref());
            }
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        let mut opened = None;
        for info in candidates {
            let key = info.devnode.to_string_lossy().into_owned();
            match Tracker::open(info) {
                Ok(t) => {
                    open_errors.remove(&key);
                    opened = Some(t);
                    break;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if open_errors.get(&key) != Some(&msg) {
                        eprintln!("{msg}");
                        open_errors.insert(key, msg);
                    }
                }
            }
        }
        let Some(mut tracker) = opened else {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        };
        idle_published = false;
        track(&mut tracker, &mut pose, scene.as_mut(), &shared, &opts);
        drop(tracker);
        pose.reset();
        if let Some(scene) = scene.as_mut() {
            scene.update(Quat::IDENTITY);
        }
        *shared.status.lock().unwrap() = Status::default();
    }
}

fn connect_headsets(bluez: &Bluez, attempts: &mut HashMap<String, Instant>, wanted: Option<&str>) {
    let headsets = match bluez.headsets() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("BlueZ: {e}");
            return;
        }
    };
    for headset in headsets {
        if wanted.is_some_and(|w| w.starts_with('/') || !w.eq_ignore_ascii_case(&headset.address)) {
            continue;
        }
        let due = attempts
            .get(&headset.address)
            .is_none_or(|t| t.elapsed() > Duration::from_secs(20));
        if !due {
            continue;
        }
        attempts.insert(headset.address.clone(), Instant::now());
        eprintln!(
            "connecting HID channel of {} ({})",
            headset.name, headset.address
        );
        if let Err(e) = bluez.connect_hid(&headset) {
            eprintln!("  failed: {e}");
        }
    }
}

fn track(
    tracker: &mut Tracker,
    pose: &mut HeadPose,
    mut scene: Option<&mut Scene>,
    shared: &Shared,
    opts: &TrackingOptions,
) {
    let info = tracker.info.clone();
    let interval = match tracker.enable(true, opts.interval_ms) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{}: cannot enable sensor: {e:#}", info.devnode.display());
            std::thread::sleep(Duration::from_secs(2));
            return;
        }
    };
    eprintln!(
        "head tracker: {} ({}{}), {}, report interval {interval} ms",
        info.name,
        info.devnode.display(),
        info.address
            .as_deref()
            .map(|a| format!(", {a}"))
            .unwrap_or_default(),
        tracker.description.as_deref().unwrap_or("no description"),
    );
    {
        let mut st = shared.status.lock().unwrap();
        *st = Status {
            device: Some(info.name.clone()),
            devnode: Some(info.devnode.to_string_lossy().into_owned()),
            address: info.address.clone(),
            protocol: tracker.description.clone(),
            ..Status::default()
        };
    }

    let enabled_at = Instant::now();
    let mut last_sample = Instant::now();
    let mut last_time = Instant::now();
    // Recent orientations, to measure rotation speed over a window: the
    // sample-to-sample jitter is as large as the slow drift while the
    // tracker's attitude converges.
    let mut history: VecDeque<(Instant, Quat)> = VecDeque::new();
    let mut speed = 0.0f32;
    let mut still_since: Option<Instant> = None;
    let mut last_discontinuity = None;
    let mut was_tracking = true;
    let mut samples = 0u64;

    while !shared.quit.load(Ordering::Relaxed) {
        let sample = match tracker.read(200) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {e:#}", info.devnode.display());
                return;
            }
        };
        let Some(sample) = sample else {
            if last_sample.elapsed() > Duration::from_secs(3) {
                // Some trackers drop the enable request while the HID channel settles.
                if let Err(e) = tracker.enable(true, opts.interval_ms) {
                    eprintln!("{}: {e:#}", info.devnode.display());
                    return;
                }
                last_sample = Instant::now();
            }
            continue;
        };
        let now = Instant::now();
        let dt = now.duration_since(last_time).as_secs_f32().min(0.5);
        last_time = now;
        last_sample = now;
        samples += 1;

        let q = Quat::from_rotation_vector(sample.rotation);
        history.push_back((now, q));
        while history.len() > 2 && now.duration_since(history[1].0) >= SPEED_WINDOW {
            history.pop_front();
        }
        let (t0, q0) = history[0];
        let span = now.duration_since(t0).as_secs_f32();
        if span >= 0.25 {
            speed = angular_speed(q0, q, span);
        }
        if speed < SETTLED_SPEED {
            still_since.get_or_insert(now);
        } else {
            still_since = None;
        }
        pose.update(q);

        // The tracker's attitude estimate converges towards gravity for a
        // few seconds after it is enabled; wait for that before choosing the
        // front, and keep the speakers in front of the head meanwhile.
        if !pose.is_centered() {
            let settled = still_since.is_some_and(|t| now.duration_since(t) > SETTLE_TIME)
                && now.duration_since(enabled_at) > Duration::from_secs(1);
            if settled || now.duration_since(enabled_at) > SETTLE_TIMEOUT {
                pose.recenter();
                eprintln!("orientation settled, front set to the current direction");
            }
        }
        if sample.discontinuity.is_some() && sample.discontinuity != last_discontinuity {
            if last_discontinuity.is_some() && pose.is_centered() {
                eprintln!("tracker reference frame reset, recentring");
                pose.recenter();
            }
            last_discontinuity = sample.discontinuity;
        }
        if shared.recenter.swap(false, Ordering::Relaxed) {
            pose.recenter();
            eprintln!("recentred");
        }
        if opts.follow_secs > 0.0 && speed < 0.2 {
            pose.drift_towards_current((dt / opts.follow_secs).min(1.0));
        }

        let tracking = shared.tracking.load(Ordering::Relaxed);
        if let Some(scene) = scene.as_deref_mut() {
            if tracking {
                scene.update(pose.room_to_head());
            } else if was_tracking {
                scene.update(Quat::IDENTITY);
            }
        }
        was_tracking = tracking;

        let (az, el) = pose.front_source_angles();
        {
            let mut st = shared.status.lock().unwrap();
            st.front_azimuth = az;
            st.front_elevation = el;
            st.samples = samples;
        }
        if opts.verbose {
            println!(
                "front source at azimuth {az:7.1}° elevation {el:6.1}°  (head yaw {:6.1}°)  speed {speed:5.2} rad/s  rot [{:6.3} {:6.3} {:6.3}]{}",
                -az,
                sample.rotation[0],
                sample.rotation[1],
                sample.rotation[2],
                if pose.is_centered() {
                    ""
                } else {
                    "  (settling)"
                },
            );
        }
    }
}
