/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Sample-rate conversion for one channel, streaming: a windowed-sinc (Blackman) kernel read
//! from a table, low-passed below the lower of the two Nyquist rates.

use std::f64::consts::PI;
use std::sync::Arc;

/// Kernel zero crossings on each side of the centre, at the lower of the two rates.
const ZERO_CROSSINGS: usize = 16;
/// Table entries per unit of kernel time.
const STEPS: usize = 256;
/// Where the passband ends, as a share of the lower Nyquist rate.
const PASSBAND: f64 = 0.95;

/// The kernel's right half, shared by every channel converting between the same two rates.
pub struct Kernel {
    table: Vec<f32>,
    /// Kernel time per input sample (the cutoff, relative to the input's Nyquist rate).
    scale: f64,
    /// Half-width in input samples.
    half: usize,
    /// Input samples per output sample.
    step: f64,
}

impl Kernel {
    pub fn new(from: f64, to: f64) -> Arc<Kernel> {
        let scale = PASSBAND * (to / from).min(1.);
        let half = (ZERO_CROSSINGS as f64 / scale).ceil() as usize;
        let length = ZERO_CROSSINGS * STEPS + 2;
        let table = (0..length)
            .map(|i| {
                let x = i as f64 / STEPS as f64;
                let sinc = if x == 0. { 1. } else { (PI * x).sin() / (PI * x) };
                let w = (x / ZERO_CROSSINGS as f64).min(1.);
                let window = 0.42 + 0.5 * (PI * w).cos() + 0.08 * (2. * PI * w).cos();
                (scale * sinc * window) as f32
            })
            .collect();
        Arc::new(Kernel {
            table,
            scale,
            half,
            step: from / to,
        })
    }

    fn at(&self, distance: f64) -> f32 {
        let x = distance.abs() * self.scale * STEPS as f64;
        let i = x as usize;
        if i + 1 >= self.table.len() {
            return 0.;
        }
        let f = (x - i as f64) as f32;
        self.table[i] + (self.table[i + 1] - self.table[i]) * f
    }
}

/// One channel's converter. Feed it input with [`Resampler::push`]; [`Resampler::finish`] drains
/// what the kernel still holds at the end.
pub struct Resampler {
    kernel: Arc<Kernel>,
    /// Input not yet past the kernel, starting at input index `base`.
    input: Vec<f32>,
    /// The next output's position, in input samples from the start of `input`.
    position: f64,
}

impl Resampler {
    pub fn new(kernel: Arc<Kernel>) -> Self {
        // Silence before the first sample, so the first output is centred on it.
        let input = vec![0.; kernel.half];
        let position = kernel.half as f64;
        Resampler {
            kernel,
            input,
            position,
        }
    }

    pub fn push(&mut self, samples: &[f32], out: &mut Vec<f32>) {
        self.input.extend_from_slice(samples);
        let half = self.kernel.half;
        while (self.position as usize) + half < self.input.len() {
            let centre = self.position as usize;
            let mut sum = 0.;
            for k in centre + 1 - half..=centre + half {
                sum += self.input[k] * self.kernel.at(self.position - k as f64);
            }
            out.push(sum);
            self.position += self.kernel.step;
        }
        // Keep only what the next output's kernel reaches.
        let keep_from = (self.position as usize + 1).saturating_sub(half);
        if keep_from > 0 {
            self.input.drain(..keep_from);
            self.position -= keep_from as f64;
        }
    }

    pub fn finish(&mut self, out: &mut Vec<f32>) {
        let silence = vec![0.; self.kernel.half + 1];
        self.push(&silence, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(samples: &[f32], from: f64, to: f64) -> Vec<f32> {
        let mut resampler = Resampler::new(Kernel::new(from, to));
        let mut out = Vec::new();
        resampler.push(samples, &mut out);
        resampler.finish(&mut out);
        out
    }

    #[test]
    fn keeps_the_length_and_a_tone() {
        let tone: Vec<f32> = (0..48_000)
            .map(|i| (2. * PI * 440. * i as f64 / 48_000.).sin() as f32 * 0.5)
            .collect();
        let out = convert(&tone, 48_000., 44_100.);
        assert!((out.len() as i64 - 44_100).abs() <= 2, "{} samples", out.len());
        // Away from the edges, the output is the same tone at the new rate.
        let worst = (1000..43_000)
            .map(|i| {
                let expected = (2. * PI * 440. * i as f64 / 44_100.).sin() as f32 * 0.5;
                (out[i] - expected).abs()
            })
            .fold(0., f32::max);
        assert!(worst < 0.002, "worst error {worst}");
    }

    #[test]
    fn streams_the_same_as_one_push() {
        let noise: Vec<f32> = (0..10_000)
            .map(|i| (((i * 7919) % 1000) as f32 / 500.) - 1.)
            .collect();
        let whole = convert(&noise, 44_100., 48_000.);
        let mut resampler = Resampler::new(Kernel::new(44_100., 48_000.));
        let mut parts = Vec::new();
        for chunk in noise.chunks(128) {
            resampler.push(chunk, &mut parts);
        }
        resampler.finish(&mut parts);
        // The same samples, up to rounding in where each output falls.
        assert_eq!(whole.len(), parts.len());
        let worst = whole.iter().zip(&parts).map(|(a, b)| (a - b).abs()).fold(0., f32::max);
        assert!(worst < 1e-4, "worst difference {worst}");
    }
}
