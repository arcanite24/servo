/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The Web Audio graph's output: render quanta from the render thread, queued on the device.

use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use servo_media_audio::block::{Chunk, FRAMES_PER_BLOCK};
use servo_media_audio::render_thread::{AudioRenderThreadMsg, SinkEosCallback};
use servo_media_audio::sink::{AudioSink, AudioSinkError};
use servo_media_streams::MediaSocket;

use crate::output::{Converter, Output};

/// What the sink keeps queued ahead of the device: enough to ride out a busy render thread.
const LEAD_SECONDS: f64 = 0.04;

#[derive(Default)]
pub struct CpalAudioSink {
    output: RefCell<Option<Output>>,
    converter: RefCell<Option<Converter>>,
    scratch: RefCell<Vec<f32>>,
}

impl AudioSink for CpalAudioSink {
    fn init(
        &self,
        sample_rate: f32,
        render_thread: Sender<AudioRenderThreadMsg>,
    ) -> Result<(), AudioSinkError> {
        let output = Output::open(sample_rate as u32, LEAD_SECONDS).map_err(AudioSinkError::Backend)?;
        let render_thread = Mutex::new(render_thread);
        output.on_need_data(move || {
            let _ = render_thread
                .lock()
                .unwrap()
                .send(AudioRenderThreadMsg::SinkNeedData);
        });
        *self.converter.borrow_mut() = Some(Converter::new(sample_rate, output.rate, output.channels));
        *self.output.borrow_mut() = Some(output);
        Ok(())
    }

    fn init_stream(&self, _: u8, _: f32, _: Box<dyn MediaSocket>) -> Result<(), AudioSinkError> {
        Err(AudioSinkError::Backend(
            "media stream destinations are not supported".to_owned(),
        ))
    }

    fn play(&self) -> Result<(), AudioSinkError> {
        if let Some(output) = self.output.borrow().as_ref() {
            output.play();
        }
        Ok(())
    }

    fn stop(&self) -> Result<(), AudioSinkError> {
        if let Some(output) = self.output.borrow().as_ref() {
            output.pause();
        }
        Ok(())
    }

    fn has_enough_data(&self) -> bool {
        self.output
            .borrow()
            .as_ref()
            .is_none_or(|output| output.has_enough())
    }

    fn push_data(&self, mut chunk: Chunk) -> Result<(), AudioSinkError> {
        let output = self.output.borrow();
        let output = output.as_ref().ok_or(AudioSinkError::BufferPushFailed)?;
        let frames = FRAMES_PER_BLOCK.0 as usize;
        let silence = vec![0.; frames];
        let planar: Vec<Vec<f32>> = match chunk.blocks.first_mut() {
            Some(block) if !block.is_silence() && block.chan_count() > 0 => {
                let channels = block.chan_count() as usize;
                let interleaved = block.interleave();
                (0..channels)
                    .map(|c| interleaved.iter().skip(c).step_by(channels).copied().collect())
                    .collect()
            },
            _ => vec![silence.clone(), silence],
        };
        let refs: Vec<&[f32]> = planar.iter().map(Vec::as_slice).collect();
        let mut scratch = self.scratch.borrow_mut();
        scratch.clear();
        self.converter
            .borrow_mut()
            .as_mut()
            .ok_or(AudioSinkError::BufferPushFailed)?
            .convert(&refs, &mut scratch);
        output.push(&scratch);
        Ok(())
    }

    fn set_eos_callback(&self, _: SinkEosCallback) {}
}
