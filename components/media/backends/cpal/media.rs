/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A media file in memory, decoded packet by packet to planar f32: Ogg Opus through libopus, and
//! whatever else symphonia's enabled formats and codecs read (WAV and PCM).

use std::io::Cursor;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_OPUS, Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader, Packet, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

/// Opus always decodes at 48 kHz (RFC 7845 §4).
const OPUS_RATE: u32 = 48_000;
/// The longest Opus packet: 120 ms at 48 kHz.
const OPUS_MAX_FRAMES: usize = 5760;

enum Codec {
    Opus(opus::Decoder),
    Symphonia(Box<dyn Decoder>),
}

pub struct Media {
    format: Box<dyn FormatReader>,
    track: u32,
    codec: Codec,
    pub channels: usize,
    pub rate: u32,
    /// The length in frames, when the container says.
    pub frames: Option<u64>,
    pub format_name: &'static str,
    /// Frames to drop before this timestamp, after a seek.
    skip_until: u64,
    scratch: Vec<f32>,
}

impl Media {
    pub fn open(bytes: Vec<u8>) -> Result<Media, String> {
        let source = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        // Gapless: the container's encoder delay and padding (Opus pre-skip) come off each packet.
        let options = FormatOptions {
            enable_gapless: true,
            ..Default::default()
        };
        let probed = symphonia::default::get_probe()
            .format(&Hint::new(), source, &options, &MetadataOptions::default())
            .map_err(|e| format!("unsupported media: {e}"))?;
        let format = probed.format;
        let track = format.default_track().ok_or("no audio track")?;
        let params = track.codec_params.clone();
        let (codec, channels, rate) = if params.codec == CODEC_TYPE_OPUS {
            let channels = match params.extra_data.as_deref() {
                Some(head) if head.len() >= 10 && &head[..8] == b"OpusHead" => head[9] as usize,
                _ => params.channels.map_or(2, |c| c.count()),
            };
            let layout = match channels {
                1 => opus::Channels::Mono,
                2 => opus::Channels::Stereo,
                n => return Err(format!("Opus with {n} channels is not supported")),
            };
            let decoder =
                opus::Decoder::new(OPUS_RATE, layout).map_err(|e| format!("libopus: {e}"))?;
            (Codec::Opus(decoder), channels, OPUS_RATE)
        } else {
            let decoder = symphonia::default::get_codecs()
                .make(&params, &DecoderOptions::default())
                .map_err(|e| format!("unsupported codec: {e}"))?;
            let channels = params.channels.map_or(0, |c| c.count());
            let rate = params.sample_rate.ok_or("no sample rate")?;
            (Codec::Symphonia(decoder), channels, rate)
        };
        if channels == 0 {
            return Err("no channels".to_owned());
        }
        let format_name = if params.codec == CODEC_TYPE_OPUS { "ogg" } else { "wav" };
        Ok(Media {
            track: track.id,
            frames: params.n_frames,
            format,
            codec,
            channels,
            rate,
            format_name,
            skip_until: 0,
            scratch: Vec::new(),
        })
    }

    pub fn duration(&self) -> Option<f64> {
        self.frames.map(|frames| frames as f64 / self.rate as f64)
    }

    /// Decodes the next packet onto the end of `out` (one Vec per channel). Returns the timestamp
    /// in frames of the packet's first kept frame, or None at the end.
    pub fn next(&mut self, out: &mut [Vec<f32>]) -> Result<Option<u64>, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                },
                Err(Error::ResetRequired) => return Ok(None),
                Err(e) => return Err(e.to_string()),
            };
            if packet.track_id() != self.track {
                continue;
            }
            let frames = self.decode(&packet)?;
            let channels = self.channels;
            // Trim the packet's delay and padding, then any frames before a seek's target.
            let mut start = (packet.trim_start as usize).min(frames);
            let end = frames.saturating_sub(packet.trim_end as usize).max(start);
            let ts = packet.ts;
            if ts + ((end - start) as u64) <= self.skip_until {
                continue;
            }
            if ts < self.skip_until {
                start += (self.skip_until - ts) as usize;
            }
            for (channel, samples) in out.iter_mut().enumerate().take(channels) {
                samples.extend((start..end).map(|frame| self.scratch[frame * channels + channel]));
            }
            return Ok(Some(ts.max(self.skip_until)));
        }
    }

    /// Decodes one packet into `scratch`, interleaved; returns its frame count.
    fn decode(&mut self, packet: &Packet) -> Result<usize, String> {
        match &mut self.codec {
            Codec::Opus(decoder) => {
                self.scratch.resize(OPUS_MAX_FRAMES * self.channels, 0.);
                decoder
                    .decode_float(&packet.data, &mut self.scratch, false)
                    .map_err(|e| format!("libopus: {e}"))
            },
            Codec::Symphonia(decoder) => {
                let decoded = match decoder.decode(packet) {
                    Ok(decoded) => decoded,
                    // A corrupt packet is skipped, as symphonia's own players do.
                    Err(Error::DecodeError(_)) => return Ok(0),
                    Err(e) => return Err(e.to_string()),
                };
                let frames = decoded.frames();
                let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
                buffer.copy_interleaved_ref(decoded);
                self.scratch.clear();
                self.scratch.extend_from_slice(buffer.samples());
                Ok(frames)
            },
        }
    }

    /// Moves to `seconds`; the next decoded frame is the one at that time.
    pub fn seek(&mut self, seconds: f64) -> Result<(), String> {
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::from(seconds.max(0.)),
                    track_id: Some(self.track),
                },
            )
            .map_err(|e| e.to_string())?;
        self.skip_until = seeked.required_ts;
        match &mut self.codec {
            Codec::Opus(decoder) => decoder.reset_state().map_err(|e| e.to_string()),
            Codec::Symphonia(decoder) => {
                decoder.reset();
                Ok(())
            },
        }
    }
}
