/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The default output device through cpal. The stream lives on a thread of its own (it is not
//! `Send` on every platform) and plays a queue of interleaved samples in the device's format;
//! [`Converter`] turns any rate and channel count into that format.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::resample::{Kernel, Resampler};

type NeedData = Box<dyn Fn() + Send>;

struct Shared {
    queue: Mutex<VecDeque<f32>>,
    channels: usize,
    /// Below this many queued frames, the stream asks for more.
    low: usize,
    need: Mutex<Option<NeedData>>,
    volume: AtomicU32,
    /// Frames the device has taken from the queue.
    played: AtomicU64,
}

enum Control {
    Play,
    Pause,
}

pub struct Output {
    shared: Arc<Shared>,
    control: Sender<Control>,
    pub rate: u32,
    pub channels: usize,
}

impl Output {
    /// Opens the default output device, at `rate` when the device supports it. The stream starts
    /// paused. `lead` is how much it keeps queued, in seconds.
    pub fn open(rate: u32, lead: f64) -> Result<Output, String> {
        let (opened, result) = mpsc::sync_channel(1);
        let (control, commands) = mpsc::channel();
        thread::Builder::new()
            .name("AudioOutput".to_owned())
            .spawn(move || {
                let (stream, shared) = match build(rate, lead) {
                    Ok(built) => built,
                    Err(error) => {
                        let _ = opened.send(Err(error));
                        return;
                    },
                };
                let _ = opened.send(Ok(shared));
                // The stream lives until the Output is dropped and the channel closes.
                for command in commands {
                    let result = match command {
                        Control::Play => stream.play().map_err(|e| e.to_string()),
                        Control::Pause => stream.pause().map_err(|e| e.to_string()),
                    };
                    if let Err(error) = result {
                        log::warn!("Audio output: {error}");
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let shared = result.recv().map_err(|e| e.to_string())??;
        let rate = shared.1;
        let shared = shared.0;
        Ok(Output {
            channels: shared.channels,
            shared,
            control,
            rate,
        })
    }

    pub fn play(&self) {
        let _ = self.control.send(Control::Play);
    }

    pub fn pause(&self) {
        let _ = self.control.send(Control::Pause);
    }

    /// Called from the device's thread when the queue runs low.
    pub fn on_need_data(&self, need: impl Fn() + Send + 'static) {
        *self.shared.need.lock().unwrap() = Some(Box::new(need));
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared
            .volume
            .store(volume.to_bits(), Ordering::Relaxed);
    }

    pub fn queued_frames(&self) -> usize {
        self.shared.queue.lock().unwrap().len() / self.shared.channels
    }

    pub fn has_enough(&self) -> bool {
        self.queued_frames() >= self.shared.low * 2
    }

    /// Frames the device has played since it opened.
    pub fn played_frames(&self) -> u64 {
        self.shared.played.load(Ordering::Relaxed)
    }

    /// Queues samples already in the device's format.
    pub fn push(&self, samples: &[f32]) {
        self.shared.queue.lock().unwrap().extend(samples);
    }

    /// Drops everything queued (a seek or a stop).
    pub fn clear(&self) {
        self.shared.queue.lock().unwrap().clear();
    }
}

fn build(rate: u32, lead: f64) -> Result<(cpal::Stream, (Arc<Shared>, u32)), String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no audio output device")?;
    let default = device
        .default_output_config()
        .map_err(|e| format!("no output configuration: {e}"))?;
    // The requested rate if a configuration in the device's own sample format allows it.
    let supported = device
        .supported_output_configs()
        .ok()
        .and_then(|mut configs| {
            configs.find(|c| {
                c.channels() == default.channels() &&
                    c.sample_format() == default.sample_format() &&
                    c.min_sample_rate().0 <= rate &&
                    rate <= c.max_sample_rate().0
            })
        })
        .map(|c| c.with_sample_rate(cpal::SampleRate(rate)))
        .unwrap_or(default);
    let config: StreamConfig = supported.config();
    let channels = config.channels as usize;
    let device_rate = config.sample_rate.0;
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        channels,
        low: ((device_rate as f64 * lead) as usize / 2).max(256),
        need: Mutex::new(None),
        volume: AtomicU32::new(1f32.to_bits()),
        played: AtomicU64::new(0),
    });
    let stream = match supported.sample_format() {
        SampleFormat::F32 => stream::<f32>(&device, &config, shared.clone()),
        SampleFormat::I16 => stream::<i16>(&device, &config, shared.clone()),
        SampleFormat::I32 => stream::<i32>(&device, &config, shared.clone()),
        SampleFormat::U16 => stream::<u16>(&device, &config, shared.clone()),
        format => return Err(format!("unsupported output sample format {format}")),
    }?;
    stream.pause().map_err(|e| e.to_string())?;
    Ok((stream, (shared, device_rate)))
}

fn stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &StreamConfig,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String> {
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let mut queue = shared.queue.lock().unwrap();
                let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
                let n = data.len().min(queue.len());
                for (out, sample) in data.iter_mut().zip(queue.drain(..n)) {
                    *out = T::from_sample(sample * volume);
                }
                for out in &mut data[n..] {
                    *out = T::EQUILIBRIUM;
                }
                let left = queue.len() / shared.channels;
                drop(queue);
                shared
                    .played
                    .fetch_add((n / shared.channels) as u64, Ordering::Relaxed);
                if left < shared.low {
                    if let Some(need) = shared.need.lock().unwrap().as_ref() {
                        need();
                    }
                }
            },
            |error| log::warn!("Audio output stream: {error}"),
            None,
        )
        .map_err(|e| format!("could not open the audio output: {e}"))
}

/// Converts planar audio at one rate and channel count to interleaved audio in the device's.
pub struct Converter {
    kernel: Option<Arc<Kernel>>,
    resamplers: Vec<Resampler>,
    device_channels: usize,
    planar: Vec<Vec<f32>>,
}

impl Converter {
    pub fn new(from: f32, to: u32, device_channels: usize) -> Self {
        let (from, to) = (from as f64, to as f64);
        Converter {
            kernel: (from != to).then(|| Kernel::new(from, to)),
            resamplers: Vec::new(),
            device_channels,
            planar: Vec::new(),
        }
    }

    /// Appends `channels` (planar, all the same length) to `out`, interleaved for the device.
    pub fn convert(&mut self, channels: &[&[f32]], out: &mut Vec<f32>) {
        if channels.is_empty() {
            return;
        }
        let converted: Vec<&[f32]> = match &self.kernel {
            None => channels.to_vec(),
            Some(kernel) => {
                if self.resamplers.len() != channels.len() {
                    self.resamplers = channels.iter().map(|_| Resampler::new(kernel.clone())).collect();
                    self.planar = vec![Vec::new(); channels.len()];
                }
                for ((resampler, input), output) in
                    self.resamplers.iter_mut().zip(channels).zip(&mut self.planar)
                {
                    output.clear();
                    resampler.push(input, output);
                }
                self.planar.iter().map(Vec::as_slice).collect()
            },
        };
        mix_into(&converted, self.device_channels, out);
    }

    /// Forgets the resamplers' history (a seek).
    pub fn reset(&mut self) {
        self.resamplers.clear();
    }
}

/// Interleaves `channels` into `device` channels: mono to the first two, stereo to the first two
/// of more, everything averaged into one.
fn mix_into(channels: &[&[f32]], device: usize, out: &mut Vec<f32>) {
    let frames = channels.iter().map(|c| c.len()).min().unwrap_or(0);
    let count = channels.len();
    out.reserve(frames * device);
    for frame in 0..frames {
        for slot in 0..device {
            let sample = if device == 1 {
                channels.iter().map(|c| c[frame]).sum::<f32>() / count as f32
            } else if count == 1 {
                if slot < 2 { channels[0][frame] } else { 0. }
            } else if slot < count {
                channels[slot][frame]
            } else {
                0.
            };
            out.push(sample);
        }
    }
}
