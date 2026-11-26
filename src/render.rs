//! Binaural rendering of virtual speakers with HRTFs from a SOFA file.
//!
//! The worker side ([`Scene`]) turns the head pose into one HRTF per speaker
//! and publishes it; the audio side ([`Engine`]) convolves each input channel
//! with its current HRTF and crossfades on every update, without allocating.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Context, Result};
use pipewire::spa::sys as spa_sys;
use sofar::reader::{Filter, OpenOptions, Sofar};
use sofar::render::{FilterPublisher, FilterTransition, RealtimeRenderer, Renderer, RendererPlan};

use crate::eq::EqMode;
use crate::pose::{Quat, head_to_sofa, sofa_direction, sofa_to_head};

/// A virtual loudspeaker fixed in the room.
#[derive(Clone, Copy, Debug)]
pub struct Speaker {
    /// SPA channel position (FL, FR, ...).
    pub position: u32,
    pub name: &'static str,
    /// Degrees, counter-clockwise from the front (left is positive).
    pub azimuth: f32,
    pub elevation: f32,
    /// Low-frequency effects: mixed to both ears without an HRTF.
    pub lfe: bool,
}

/// ITU-R BS.775 style 7.1 layout, in PipeWire's channel order.
pub fn layout_7_1(front_angle: f32) -> Vec<Speaker> {
    let s = |position, name, azimuth, lfe| Speaker {
        position,
        name,
        azimuth,
        elevation: 0.0,
        lfe,
    };
    vec![
        s(spa_sys::SPA_AUDIO_CHANNEL_FL, "FL", front_angle, false),
        s(spa_sys::SPA_AUDIO_CHANNEL_FR, "FR", -front_angle, false),
        s(spa_sys::SPA_AUDIO_CHANNEL_FC, "FC", 0.0, false),
        s(spa_sys::SPA_AUDIO_CHANNEL_LFE, "LFE", 0.0, true),
        s(spa_sys::SPA_AUDIO_CHANNEL_RL, "RL", 150.0, false),
        s(spa_sys::SPA_AUDIO_CHANNEL_RR, "RR", -150.0, false),
        s(spa_sys::SPA_AUDIO_CHANNEL_SL, "SL", 90.0, false),
        s(spa_sys::SPA_AUDIO_CHANNEL_SR, "SR", -90.0, false),
    ]
}

pub struct RenderConfig<'a> {
    pub sofa_path: &'a Path,
    pub sample_rate: u32,
    pub partition_len: usize,
    pub speakers: Vec<Speaker>,
    pub gain: f32,
    pub lfe_gain: f32,
    /// Source distance in metres used for the HRTF lookup.
    pub distance: f32,
    /// Equaliser in use, switchable at runtime (an [`EqMode::index`]).
    pub eq: Arc<AtomicU8>,
}

/// Minimum-phase equaliser applied to both ears after the HRTF mix.
struct OutputEq {
    left: Renderer,
    right: Renderer,
    out: Vec<f32>,
    unused: Vec<f32>,
}

impl OutputEq {
    fn new(taps: &[f32], sample_rate: u32, partition_len: usize) -> Result<OutputEq> {
        let plan = RendererPlan::builder(taps.len())
            .with_sample_rate(sample_rate as f32)
            .with_partition_len(partition_len)
            .build()?;
        let mut filter = Filter::new(taps.len());
        filter.left.copy_from_slice(taps);
        let mut left = Renderer::new(&plan);
        left.set_filter(&filter)?;
        let right = left.clone();
        Ok(OutputEq {
            left,
            right,
            out: vec![0.0; partition_len],
            unused: vec![0.0; partition_len],
        })
    }

    fn apply(&mut self, left: &mut [f32], right: &mut [f32]) {
        let _ = self
            .left
            .process_block(left, &mut self.out, &mut self.unused);
        left.copy_from_slice(&self.out);
        let _ = self
            .right
            .process_block(right, &mut self.out, &mut self.unused);
        right.copy_from_slice(&self.out);
    }
}

/// Worker-side state: picks HRTFs for the current head pose.
pub struct Scene {
    sofa: Sofar,
    filter: Filter,
    speakers: Vec<Speaker>,
    /// Room directions of the speakers, in the head frame at recentring.
    room_dirs: Vec<[f32; 3]>,
    publishers: Vec<Option<FilterPublisher>>,
    last_dirs: Vec<Option<[f32; 3]>>,
    distance: f32,
}

/// Audio-thread state.
pub struct Engine {
    renderers: Vec<Option<RealtimeRenderer>>,
    input: Vec<Vec<f32>>,
    fill: usize,
    left: Vec<f32>,
    right: Vec<f32>,
    lfe: Vec<bool>,
    gain: f32,
    lfe_gain: f32,
    /// One equaliser per [`EqMode`], indexed by [`EqMode::index`].
    eqs: Vec<Option<OutputEq>>,
    eq_select: Arc<AtomicU8>,
    eq_current: u8,
}

pub fn build(cfg: RenderConfig) -> Result<(Scene, Engine)> {
    let sofa = OpenOptions::new()
        .sample_rate(cfg.sample_rate as f32)
        .open(cfg.sofa_path)
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .with_context(|| format!("cannot load SOFA file {}", cfg.sofa_path.display()))?;
    let filt_len = sofa.filter_len();
    let plan = RendererPlan::builder(filt_len)
        .with_sample_rate(cfg.sample_rate as f32)
        .with_partition_len(cfg.partition_len)
        .build()?;

    let mut filter = Filter::new(filt_len);
    let room_dirs: Vec<[f32; 3]> = cfg
        .speakers
        .iter()
        .map(|s| sofa_to_head(sofa_direction(s.azimuth, s.elevation)))
        .collect();

    let mut publishers = Vec::new();
    let mut renderers = Vec::new();
    for (speaker, dir) in cfg.speakers.iter().zip(&room_dirs) {
        if speaker.lfe {
            publishers.push(None);
            renderers.push(None);
            continue;
        }
        let mut renderer = Renderer::builder(&plan)
            .with_filter_transition(FilterTransition::crossfade(cfg.partition_len))
            .with_max_delay(0.005)
            .build()?;
        let d = head_to_sofa(*dir);
        sofa.filter(
            d[0] * cfg.distance,
            d[1] * cfg.distance,
            d[2] * cfg.distance,
            &mut filter,
        );
        renderer.set_filter(&filter)?;
        let (publisher, rt) = renderer.into_realtime();
        publishers.push(Some(publisher));
        renderers.push(Some(rt));
    }

    let mut eqs = Vec::new();
    for mode in EqMode::ALL {
        let taps = crate::eq::design(&sofa, mode, &cfg.speakers, cfg.distance, cfg.sample_rate);
        eqs.push(match taps {
            Some(taps) => Some(OutputEq::new(&taps, cfg.sample_rate, cfg.partition_len)?),
            None => None,
        });
    }
    let eq_current = cfg.eq.load(Ordering::Relaxed);

    let channels = cfg.speakers.len();
    let engine = Engine {
        renderers,
        input: (0..channels)
            .map(|_| vec![0.0; cfg.partition_len])
            .collect(),
        fill: 0,
        left: vec![0.0; cfg.partition_len],
        right: vec![0.0; cfg.partition_len],
        lfe: cfg.speakers.iter().map(|s| s.lfe).collect(),
        gain: cfg.gain,
        lfe_gain: cfg.lfe_gain,
        eqs,
        eq_select: cfg.eq,
        eq_current,
    };
    let scene = Scene {
        sofa,
        filter,
        last_dirs: vec![None; channels],
        speakers: cfg.speakers,
        room_dirs,
        publishers,
        distance: cfg.distance,
    };
    Ok((scene, engine))
}

impl Scene {
    /// Publish new HRTFs for a head rotation (room -> current head frame).
    /// Speakers that moved less than about 0.3 degrees keep their filter.
    pub fn update(&mut self, room_to_head: Quat) {
        const MIN_CHANGE: f32 = 0.005; // ~0.3 degrees
        for i in 0..self.speakers.len() {
            let Some(publisher) = self.publishers[i].as_mut() else {
                continue;
            };
            let d = head_to_sofa(room_to_head.rotate(self.room_dirs[i]));
            if let Some(last) = self.last_dirs[i] {
                let delta =
                    (d[0] - last[0]).powi(2) + (d[1] - last[1]).powi(2) + (d[2] - last[2]).powi(2);
                if delta.sqrt() < MIN_CHANGE {
                    publisher.reclaim();
                    continue;
                }
            }
            self.sofa.filter(
                d[0] * self.distance,
                d[1] * self.distance,
                d[2] * self.distance,
                &mut self.filter,
            );
            match publisher.publish_filter(&self.filter) {
                Ok(()) => self.last_dirs[i] = Some(d),
                Err(e) => eprintln!("HRTF update for {} failed: {e}", self.speakers[i].name),
            }
        }
    }
}

/// Transparent below 0.8, then smoothly approaches 1.0 instead of clipping.
fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh()).copysign(x)
    }
}

impl Engine {
    /// Feed planar input (one slice per channel, equal lengths) and emit
    /// interleaved stereo through `out` whenever a partition is complete.
    pub fn process(&mut self, planes: &[&[f32]], mut out: impl FnMut(&[f32], &[f32])) {
        let frames = planes.first().map_or(0, |p| p.len());
        let part = self.left.len();
        let mut pos = 0;
        while pos < frames {
            let n = (part - self.fill).min(frames - pos);
            for (ch, buf) in self.input.iter_mut().enumerate() {
                let dst = &mut buf[self.fill..self.fill + n];
                match planes.get(ch) {
                    Some(src) if src.len() >= pos + n => dst.copy_from_slice(&src[pos..pos + n]),
                    _ => dst.fill(0.0),
                }
            }
            self.fill += n;
            pos += n;
            if self.fill == part {
                self.render_partition();
                out(&self.left, &self.right);
                self.fill = 0;
            }
        }
    }

    fn render_partition(&mut self) {
        self.left.fill(0.0);
        self.right.fill(0.0);
        for (ch, input) in self.input.iter().enumerate() {
            if let Some(renderer) = self.renderers[ch].as_mut() {
                // Errors only occur for invalid lengths, which cannot happen here.
                let _ = renderer.process_block_add(input, &mut self.left, &mut self.right);
            }
        }
        let selected = self.eq_select.load(Ordering::Relaxed);
        if selected != self.eq_current {
            // The newly selected equaliser starts from silence.
            if let Some(Some(eq)) = self.eqs.get_mut(selected as usize) {
                eq.left.reset();
                eq.right.reset();
            }
            self.eq_current = selected;
        }
        if let Some(Some(eq)) = self.eqs.get_mut(self.eq_current as usize) {
            eq.apply(&mut self.left, &mut self.right);
        }
        // LFE bypasses both the HRTF and the equaliser.
        for (ch, input) in self.input.iter().enumerate() {
            if self.lfe[ch] {
                for ((l, r), x) in self.left.iter_mut().zip(self.right.iter_mut()).zip(input) {
                    *l += x * self.lfe_gain;
                    *r += x * self.lfe_gain;
                }
            }
        }
        for s in self.left.iter_mut().chain(self.right.iter_mut()) {
            *s = soft_clip(*s * self.gain);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render an impulse on one channel and return (left, right) energy.
    fn energies(
        scene: &mut Scene,
        engine: &mut Engine,
        channel: usize,
        room_to_head: Quat,
    ) -> (f32, f32) {
        scene.update(room_to_head);
        let part = engine.left.len();
        let mut planes = vec![vec![0.0f32; part * 64]; engine.input.len()];
        planes[channel][0] = 1.0;
        // Let the crossfade to the new filter finish before the impulse.
        let silence = vec![vec![0.0f32; part * 4]; engine.input.len()];
        let refs: Vec<&[f32]> = silence.iter().map(|p| p.as_slice()).collect();
        engine.process(&refs, |_, _| {});
        let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
        let (mut el, mut er) = (0.0, 0.0);
        engine.process(&refs, |l, r| {
            el += l.iter().map(|x| x * x).sum::<f32>();
            er += r.iter().map(|x| x * x).sum::<f32>();
        });
        (el, er)
    }

    #[test]
    fn hrtf_directions() {
        let Some(sofa) = std::env::var_os("SPATIAL_AUDIO_SOFA") else {
            eprintln!("SPATIAL_AUDIO_SOFA not set, skipping");
            return;
        };
        let (mut scene, mut engine) = build(RenderConfig {
            sofa_path: Path::new(&sofa),
            sample_rate: 48000,
            partition_len: 128,
            speakers: layout_7_1(30.0),
            gain: 1.0,
            lfe_gain: 1.0,
            distance: 1.0,
            eq: Arc::new(AtomicU8::new(EqMode::None.index())),
        })
        .unwrap();
        // FL (index 0) at +30 degrees: louder on the left.
        let (l, r) = energies(&mut scene, &mut engine, 0, Quat::IDENTITY);
        assert!(l > r * 2.0, "FL facing front: l={l} r={r}");
        // Head turned 90 degrees left: FL is now at -60 degrees, on the right.
        let turned = Quat::from_rotation_vector([0.0, 0.0, std::f32::consts::FRAC_PI_2]).conj();
        let (l, r) = energies(&mut scene, &mut engine, 0, turned);
        assert!(r > l * 2.0, "FL after turning left: l={l} r={r}");
        // SR (index 7) at -90 degrees: louder on the right.
        let (l, r) = energies(&mut scene, &mut engine, 7, Quat::IDENTITY);
        assert!(r > l * 2.0, "SR: l={l} r={r}");
    }
}

#[cfg(test)]
mod response {
    use super::*;

    /// Print the magnitude response of FL -> both ears (diagnostic).
    #[test]
    #[ignore]
    fn print_front_left_response() {
        let sofa = std::env::var_os("SPATIAL_AUDIO_SOFA").expect("SPATIAL_AUDIO_SOFA");
        let raw = OpenOptions::new()
            .sample_rate(48000.0)
            .open(Path::new(&sofa))
            .unwrap();
        println!("filter_len = {}", raw.filter_len());
        for eq in [EqMode::None, EqMode::Diffuse, EqMode::Front] {
            println!("== {eq:?}");
            let (mut scene, mut engine) = build(RenderConfig {
                sofa_path: Path::new(&sofa),
                sample_rate: 48000,
                partition_len: 128,
                speakers: layout_7_1(30.0),
                gain: 1.0,
                lfe_gain: 0.0,
                distance: 1.0,
                eq: Arc::new(AtomicU8::new(eq.index())),
            })
            .unwrap();
            scene.update(Quat::IDENTITY);
            let n = 16384;
            let mut planes = vec![vec![0.0f32; n]; 8];
            planes[0][0] = 1.0;
            if std::env::var_os("MONO").is_some() {
                planes[1][0] = 1.0;
            }
            let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
            let (mut l, mut r) = (Vec::new(), Vec::new());
            engine.process(&refs, |a, b| {
                l.extend_from_slice(a);
                r.extend_from_slice(b);
            });
            for (name, x) in [("left", &l), ("right", &r)] {
                let mut line = format!("{name:5}:");
                for f in [
                    50.0, 100.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 6000.0, 8000.0, 10000.0,
                    12000.0, 14000.0, 16000.0, 18000.0, 20000.0,
                ] {
                    let w = 2.0 * std::f64::consts::PI * f / 48000.0;
                    let (mut re, mut im) = (0.0f64, 0.0f64);
                    for (k, v) in x.iter().enumerate() {
                        re += *v as f64 * (w * k as f64).cos();
                        im -= *v as f64 * (w * k as f64).sin();
                    }
                    let db = 20.0 * (re.hypot(im) + 1e-12).log10();
                    line += &format!(" {:>5}:{db:6.1}", f as u32);
                }
                println!("{line}");
            }
            let peak = l.iter().chain(&r).fold(0.0f32, |m, v| m.max(v.abs()));
            let first = l.iter().position(|v| v.abs() > peak * 0.1).unwrap_or(0);
            println!("peak {peak:.4}, onset at sample {first}");
        }
    }
}
