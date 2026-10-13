/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A servo-media backend for audio without GStreamer: Web Audio's output and `decodeAudioData`,
//! and `<audio>` playback of Ogg Opus and WAV. Output goes through cpal (CoreAudio, WASAPI, ALSA,
//! AAudio); symphonia reads the containers and PCM, and libopus decodes Opus.
//!
//! Video, capture, media streams and WebRTC are not supported: those parts answer as the dummy
//! backend does.

mod decoder;
mod media;
mod output;
mod player;
mod resample;
mod sink;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use servo_base::generic_channel::GenericCallback;
use servo_media::{Backend, BackendInit, MediaInstanceError, SupportsMediaType};
use servo_media_audio::context::{AudioContext, AudioContextOptions};
use servo_media_audio::decoder::AudioDecoder;
use servo_media_audio::sink::AudioSinkError;
use servo_media_audio::{AudioBackend, AudioStreamReader};
use servo_media_dummy::{DummyBackend, DummyStreamReader};
use servo_media_player::context::PlayerGLContext;
use servo_media_player::{Player, PlayerEvent, StreamType, audio, video};
use servo_media_streams::capture::MediaTrackConstraintSet;
use servo_media_streams::device_monitor::MediaDeviceMonitor;
use servo_media_streams::registry::MediaStreamId;
use servo_media_streams::{MediaOutput, MediaSocket, MediaStreamType};
use servo_media_traits::{BackendMsg, ClientContextId, MediaInstance};
use servo_media_webrtc::{WebRtcController, WebRtcSignaller};

type Instances = HashMap<ClientContextId, Vec<(usize, Weak<Mutex<dyn MediaInstance>>)>>;

pub struct CpalBackend {
    dummy: DummyBackend,
    instances: Arc<Mutex<Instances>>,
    next_instance_id: AtomicUsize,
    backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
}

impl BackendInit for CpalBackend {
    fn init() -> Box<dyn Backend> {
        let instances: Arc<Mutex<Instances>> = Default::default();
        let (backend_chan, messages) = mpsc::channel();
        let instances_ = instances.clone();
        // Forgets each media instance as it shuts down.
        let _ = thread::Builder::new()
            .name("CpalBackend ShutdownThread".to_owned())
            .spawn(move || {
                for message in messages {
                    let BackendMsg::Shutdown { context, id, tx_ack } = message;
                    let mut instances = instances_.lock().unwrap();
                    if let Some(list) = instances.get_mut(&context) {
                        list.retain(|(instance, _)| *instance != id);
                        if list.is_empty() {
                            instances.remove(&context);
                        }
                    }
                    let _ = tx_ack.send(());
                }
            });
        Box::new(CpalBackend {
            dummy: DummyBackend,
            instances,
            next_instance_id: AtomicUsize::new(0),
            backend_chan: Arc::new(Mutex::new(backend_chan)),
        })
    }
}

impl CpalBackend {
    fn register(&self, context: &ClientContextId, id: usize, instance: Weak<Mutex<dyn MediaInstance>>) {
        self.instances
            .lock()
            .unwrap()
            .entry(*context)
            .or_default()
            .push((id, instance));
    }

    fn each_instance(
        &self,
        context: &ClientContextId,
        action: &dyn Fn(&dyn MediaInstance) -> Result<(), MediaInstanceError>,
    ) {
        if let Some(list) = self.instances.lock().unwrap().get_mut(context) {
            list.retain(|(_, weak)| match weak.upgrade() {
                Some(instance) => {
                    if action(&*instance.lock().unwrap()).is_err() {
                        log::warn!("Error executing media instance action");
                    }
                    true
                },
                None => false,
            });
        }
    }
}

impl Backend for CpalBackend {
    fn create_player(
        &self,
        context: &ClientContextId,
        _: StreamType,
        events: GenericCallback<PlayerEvent>,
        _: Option<Arc<Mutex<dyn video::VideoFrameRenderer>>>,
        audio_renderer: Option<Arc<Mutex<dyn audio::AudioRenderer>>>,
        _: Box<dyn PlayerGLContext>,
    ) -> Arc<Mutex<dyn Player>> {
        let id = self.next_instance_id.fetch_add(1, Ordering::Relaxed);
        let player = Arc::new(Mutex::new(player::CpalPlayer::new(
            id,
            *context,
            self.backend_chan.clone(),
            events,
            audio_renderer,
        )));
        let weak: Weak<Mutex<dyn MediaInstance>> = Arc::downgrade(&player) as _;
        self.register(context, id, weak);
        player
    }

    fn create_audiostream(&self) -> MediaStreamId {
        self.dummy.create_audiostream()
    }

    fn create_videostream(&self) -> MediaStreamId {
        self.dummy.create_videostream()
    }

    fn create_stream_output(&self) -> Box<dyn MediaOutput> {
        self.dummy.create_stream_output()
    }

    fn create_stream_and_socket(&self, ty: MediaStreamType) -> (Box<dyn MediaSocket>, MediaStreamId) {
        self.dummy.create_stream_and_socket(ty)
    }

    fn create_audioinput_stream(&self, set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
        self.dummy.create_audioinput_stream(set)
    }

    fn create_videoinput_stream(&self, set: MediaTrackConstraintSet) -> Option<MediaStreamId> {
        self.dummy.create_videoinput_stream(set)
    }

    fn create_audio_context(
        &self,
        context: &ClientContextId,
        options: AudioContextOptions,
    ) -> Result<Arc<Mutex<AudioContext>>, AudioSinkError> {
        let id = self.next_instance_id.fetch_add(1, Ordering::Relaxed);
        let audio_context = Arc::new(Mutex::new(AudioContext::new::<Self>(
            id,
            context,
            self.backend_chan.clone(),
            options,
        )?));
        let weak: Weak<Mutex<dyn MediaInstance>> = Arc::downgrade(&audio_context) as _;
        self.register(context, id, weak);
        Ok(audio_context)
    }

    fn create_webrtc(&self, signaller: Box<dyn WebRtcSignaller>) -> WebRtcController {
        self.dummy.create_webrtc(signaller)
    }

    fn can_play_type(&self, media_type: &str) -> SupportsMediaType {
        let Ok(mime) = media_type.parse::<mime::Mime>() else {
            return SupportsMediaType::No;
        };
        let codecs: Vec<String> = mime
            .get_param("codecs")
            .map(|codecs| codecs.as_str().split(',').map(|c| c.trim().to_ascii_lowercase()).collect())
            .unwrap_or_default();
        let (container, known): (bool, &[&str]) =
            match (mime.type_().as_str(), mime.subtype().as_str()) {
                ("audio", "ogg") | ("audio", "opus") | ("application", "ogg") => (true, &["opus"]),
                ("audio", "wav") | ("audio", "wave") | ("audio", "x-wav") | ("audio", "vnd.wave") => {
                    (true, &["1", "3"])
                },
                _ => (false, &[]),
            };
        match (container, codecs.is_empty()) {
            (false, _) => SupportsMediaType::No,
            (true, true) => SupportsMediaType::Maybe,
            (true, false) if codecs.iter().all(|c| known.contains(&c.as_str())) => {
                SupportsMediaType::Probably
            },
            (true, false) => SupportsMediaType::No,
        }
    }

    fn mute(&self, context: &ClientContextId, muted: bool) {
        self.each_instance(context, &move |instance| instance.mute(muted));
    }

    fn suspend(&self, context: &ClientContextId) {
        self.each_instance(context, &|instance| instance.suspend());
    }

    fn resume(&self, context: &ClientContextId) {
        self.each_instance(context, &|instance| instance.resume());
    }

    fn get_device_monitor(&self) -> Box<dyn MediaDeviceMonitor> {
        self.dummy.get_device_monitor()
    }
}

impl AudioBackend for CpalBackend {
    type Sink = sink::CpalAudioSink;

    fn make_decoder() -> Box<dyn AudioDecoder> {
        Box::new(decoder::CpalAudioDecoder)
    }

    fn make_sink() -> Result<Self::Sink, AudioSinkError> {
        Ok(sink::CpalAudioSink::default())
    }

    fn make_streamreader(
        _: MediaStreamId,
        _: f32,
    ) -> Result<Box<dyn AudioStreamReader + Send>, AudioSinkError> {
        Ok(Box::new(DummyStreamReader))
    }
}
