/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `decodeAudioData`: the whole file to planar f32 at the context's rate.

use servo_media_audio::decoder::{
    AudioDecoder, AudioDecoderCallbacks, AudioDecoderError, AudioDecoderOptions,
};

use crate::media::Media;
use crate::resample::{Kernel, Resampler};

pub struct CpalAudioDecoder;

impl AudioDecoder for CpalAudioDecoder {
    fn decode(
        &self,
        data: Vec<u8>,
        callbacks: AudioDecoderCallbacks,
        options: Option<AudioDecoderOptions>,
    ) {
        let rate = options.unwrap_or_default().sample_rate as f64;
        match decode(data, rate) {
            Ok(channels) => {
                callbacks.ready(channels.len() as u32);
                for (index, samples) in channels.into_iter().enumerate() {
                    // The channel's position as a mask, as GStreamer reports it: script takes its log2.
                    callbacks.progress(Box::new(samples), 1 << index);
                }
                callbacks.eos();
            },
            Err(error) => {
                log::warn!("decodeAudioData: {error}");
                callbacks.error(AudioDecoderError::Backend(error));
            },
        }
    }
}

fn decode(data: Vec<u8>, rate: f64) -> Result<Vec<Vec<f32>>, String> {
    let mut media = Media::open(data)?;
    let mut channels = vec![Vec::new(); media.channels];
    while media.next(&mut channels)?.is_some() {}
    if media.rate as f64 == rate {
        return Ok(channels);
    }
    let kernel = Kernel::new(media.rate as f64, rate);
    Ok(channels
        .iter()
        .map(|samples| {
            let mut resampler = Resampler::new(kernel.clone());
            let mut out = Vec::with_capacity((samples.len() as f64 * rate / media.rate as f64) as usize + 1);
            resampler.push(samples, &mut out);
            resampler.finish(&mut out);
            out.truncate((samples.len() as f64 * rate / media.rate as f64).round() as usize);
            out
        })
        .collect())
}
