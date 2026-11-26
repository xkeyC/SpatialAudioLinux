//! Headphone equalisation of the HRTF set.
//!
//! Raw HRTF measurements carry everything the measurement chain added:
//! MIT KEMAR (libmysofa's default.sofa) is measured at the ear-canal
//! entrance of a dummy head with a small loudspeaker, so every direction
//! shares a +15 dB ear-canal resonance around 2-4 kHz and a steep bass
//! roll-off. Played directly it sounds like a telephone. The common cure is
//! to divide out the response that all directions share (diffuse-field
//! equalisation), leaving only the direction-dependent cues.

use std::f64::consts::PI;

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;
use sofar::reader::{Filter, Sofar};

use crate::render::Speaker;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum EqMode {
    /// Diffuse-field equalisation: average over all directions.
    Diffuse,
    /// Equalise to the front left/right speakers: stereo content keeps its
    /// original timbre.
    Front,
    /// Raw HRTFs.
    None,
}

impl EqMode {
    pub const ALL: [EqMode; 3] = [EqMode::Front, EqMode::Diffuse, EqMode::None];

    pub fn index(self) -> u8 {
        match self {
            EqMode::Front => 0,
            EqMode::Diffuse => 1,
            EqMode::None => 2,
        }
    }

    pub fn from_index(i: u8) -> EqMode {
        Self::ALL.get(i as usize).copied().unwrap_or(EqMode::Front)
    }

    pub fn name(self) -> &'static str {
        match self {
            EqMode::Front => "front",
            EqMode::Diffuse => "diffuse",
            EqMode::None => "none",
        }
    }
}

const FFT_LEN: usize = 16384;
pub const EQ_LEN: usize = 4096;
const MAX_BOOST_DB: f64 = 24.0;
const MAX_CUT_DB: f64 = -24.0;
const HIGHPASS_HZ: f64 = 25.0;

/// Directions spread evenly over the sphere (Fibonacci lattice), as unit
/// vectors in the SOFA frame.
fn sphere_points(n: usize) -> Vec<[f32; 3]> {
    let golden = PI * (3.0 - 5f64.sqrt());
    (0..n)
        .map(|i| {
            let z = 1.0 - 2.0 * (i as f64 + 0.5) / n as f64;
            let r = (1.0 - z * z).sqrt();
            let a = golden * i as f64;
            [(r * a.cos()) as f32, (r * a.sin()) as f32, z as f32]
        })
        .collect()
}

/// Design a minimum-phase equaliser (EQ_LEN taps) for the chosen mode.
/// Returns `None` for [`EqMode::None`].
pub fn design(
    sofa: &Sofar,
    mode: EqMode,
    speakers: &[Speaker],
    distance: f32,
    sample_rate: u32,
) -> Option<Vec<f32>> {
    let directions: Vec<[f32; 3]> = match mode {
        EqMode::None => return None,
        EqMode::Diffuse => sphere_points(600),
        EqMode::Front => speakers
            .iter()
            .filter(|s| !s.lfe && s.azimuth.abs() > 1.0 && s.azimuth.abs() < 60.0)
            .map(|s| crate::pose::sofa_direction(s.azimuth, s.elevation))
            .collect(),
    };

    let mut planner = RealFftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(FFT_LEN);
    let ifft = planner.plan_fft_inverse(FFT_LEN);
    let bins = FFT_LEN / 2 + 1;

    // Average power spectrum over directions and both ears.
    let mut power = vec![0.0f64; bins];
    let mut filter = Filter::new(sofa.filter_len());
    let mut input = fft.make_input_vec();
    let mut spectrum = fft.make_output_vec();
    let mut count = 0.0;
    for d in &directions {
        sofa.filter(
            d[0] * distance,
            d[1] * distance,
            d[2] * distance,
            &mut filter,
        );
        for ir in [&filter.left, &filter.right] {
            input.fill(0.0);
            for (dst, src) in input.iter_mut().zip(ir.iter()) {
                *dst = *src as f64;
            }
            fft.process(&mut input, &mut spectrum).ok()?;
            for (p, c) in power.iter_mut().zip(&spectrum) {
                *p += c.norm_sqr();
            }
            count += 1.0;
        }
    }
    for p in &mut power {
        *p /= count;
    }

    // 1/6-octave smoothing of the power spectrum.
    let hz_per_bin = sample_rate as f64 / FFT_LEN as f64;
    let mut prefix = vec![0.0f64; bins + 1];
    for i in 0..bins {
        prefix[i + 1] = prefix[i] + power[i];
    }
    let smooth: Vec<f64> = (0..bins)
        .map(|i| {
            let f = (i as f64 * hz_per_bin).max(hz_per_bin);
            let lo = ((f * 2f64.powf(-1.0 / 12.0)) / hz_per_bin).floor() as usize;
            let hi = (((f * 2f64.powf(1.0 / 12.0)) / hz_per_bin).ceil() as usize).min(bins - 1);
            let lo = lo.min(i);
            let hi = hi.max(i);
            (prefix[hi + 1] - prefix[lo]) / (hi + 1 - lo) as f64
        })
        .collect();

    // Inverse magnitude in dB, normalised to 0 dB average over 200 Hz - 4 kHz.
    let mut gain_db: Vec<f64> = smooth
        .iter()
        .map(|p| -10.0 * p.max(1e-20).log10())
        .collect();
    let band: Vec<f64> = (0..bins)
        .filter(|&i| {
            let f = i as f64 * hz_per_bin;
            (200.0..4000.0).contains(&f)
        })
        .map(|i| gain_db[i])
        .collect();
    let reference = band.iter().sum::<f64>() / band.len() as f64;
    for (i, g) in gain_db.iter_mut().enumerate() {
        let f = i as f64 * hz_per_bin;
        *g = (*g - reference).clamp(MAX_CUT_DB, MAX_BOOST_DB);
        // Second-order high-pass so subsonic content is not boosted.
        let x = f / HIGHPASS_HZ;
        *g += 10.0 * (x.powi(4) / (1.0 + x.powi(4))).max(1e-12).log10();
    }

    // Minimum phase via the real cepstrum.
    let mut log_mag: Vec<Complex<f64>> = gain_db
        .iter()
        .map(|g| Complex::new(g / 20.0 * std::f64::consts::LN_10, 0.0))
        .collect();
    let mut cepstrum = ifft.make_output_vec();
    ifft.process(&mut log_mag, &mut cepstrum).ok()?;
    for c in &mut cepstrum {
        *c /= FFT_LEN as f64;
    }
    for (n, c) in cepstrum.iter_mut().enumerate() {
        if n == 0 || n == FFT_LEN / 2 {
            continue;
        }
        *c = if n < FFT_LEN / 2 { *c * 2.0 } else { 0.0 };
    }
    let mut folded = fft.make_output_vec();
    fft.process(&mut cepstrum, &mut folded).ok()?;
    let mut min_phase: Vec<Complex<f64>> = folded.iter().map(|c| c.exp()).collect();
    min_phase[0].im = 0.0;
    min_phase[bins - 1].im = 0.0;
    let mut ir = ifft.make_output_vec();
    ifft.process(&mut min_phase, &mut ir).ok()?;

    // Truncate with a half-Hann fade-out.
    let fade = EQ_LEN / 4;
    let taps = (0..EQ_LEN)
        .map(|n| {
            let w = if n + fade >= EQ_LEN {
                let k = (n + fade - EQ_LEN) as f64 / fade as f64;
                0.5 * (1.0 + (PI * k).cos())
            } else {
                1.0
            };
            (ir[n] / FFT_LEN as f64 * w) as f32
        })
        .collect();
    Some(taps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sphere_points_are_unit() {
        for p in sphere_points(100) {
            let n = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
            assert!((n - 1.0).abs() < 1e-5);
        }
    }
}
