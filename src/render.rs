//! Binaural rendering of virtual speakers with HRTFs from a SOFA file.
//!
//! The worker side ([`Scene`]) turns the head pose into one HRTF per speaker
//! and publishes it; the audio side ([`Engine`]) convolves each input channel
//! with its current HRTF and crossfades on every update, without allocating.

use std::path::Path;

use anyhow::{Context, Result};
use pipewire::spa::sys as spa_sys;
use sofar::reader::{Filter, OpenOptions, Sofar};
use sofar::render::{FilterPublisher, FilterTransition, RealtimeRenderer, Renderer, RendererPlan};

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
            if self.lfe[ch] {
                for ((l, r), x) in self.left.iter_mut().zip(self.right.iter_mut()).zip(input) {
                    *l += x * self.lfe_gain;
                    *r += x * self.lfe_gain;
                }
            } else if let Some(renderer) = self.renderers[ch].as_mut() {
                // Errors only occur for invalid lengths, which cannot happen here.
                let _ = renderer.process_block_add(input, &mut self.left, &mut self.right);
            }
        }
        for s in self.left.iter_mut().chain(self.right.iter_mut()) {
            *s *= self.gain;
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
