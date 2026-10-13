/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `<audio>`: the element pushes the file's bytes; once they have all arrived, the player decodes
//! as it plays, on a thread of its own. With an audio renderer (a `MediaElementAudioSourceNode`)
//! the decoded audio goes into the Web Audio graph at the file's rate, paced by the clock;
//! otherwise it plays on the default output device.
//!
//! The whole file is kept in memory and never fetched again, so the player asks for no byte
//! ranges (`SeekData`) and seeks within what it holds.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use servo_base::generic_channel::GenericCallback;
use servo_media_player::audio::AudioRenderer;
use servo_media_player::metadata::Metadata;
use servo_media_player::{PlaybackState, Player, PlayerError, PlayerEvent};
use servo_media_streams::registry::MediaStreamId;
use servo_media_traits::{BackendMsg, ClientContextId, MediaInstance, MediaInstanceError};

use crate::media::Media;
use crate::output::{Converter, Output};

/// How far decoded audio runs ahead of what is heard.
const LEAD: Duration = Duration::from_millis(200);
/// How often a playing player reports its position, as GStreamer's player does.
const POSITION_INTERVAL: Duration = Duration::from_millis(500);
/// How often a playing player wakes to decode more.
const TICK: Duration = Duration::from_millis(10);

enum Command {
    Data(Vec<u8>),
    EndOfStream,
    InputSize(u64),
    Play,
    Pause,
    Stop,
    Seek(f64),
    Volume(f32),
    Suspend(bool),
}

/// What the element can ask without waiting for the player's thread.
#[derive(Default)]
struct Shared {
    paused: AtomicBool,
    muted: AtomicBool,
    volume: AtomicU64,
    duration: AtomicU64,
}

impl Shared {
    fn duration(&self) -> Option<f64> {
        let duration = f64::from_bits(self.duration.load(Ordering::Relaxed));
        (duration > 0.).then_some(duration)
    }

    fn gain(&self) -> f32 {
        if self.muted.load(Ordering::Relaxed) {
            0.
        } else {
            f64::from_bits(self.volume.load(Ordering::Relaxed)) as f32
        }
    }
}

pub struct CpalPlayer {
    id: usize,
    context_id: ClientContextId,
    backend: Arc<Mutex<Sender<BackendMsg>>>,
    commands: Sender<Command>,
    shared: Arc<Shared>,
}

impl CpalPlayer {
    pub fn new(
        id: usize,
        context_id: ClientContextId,
        backend: Arc<Mutex<Sender<BackendMsg>>>,
        events: GenericCallback<PlayerEvent>,
        renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
    ) -> CpalPlayer {
        let shared = Arc::new(Shared::default());
        shared.paused.store(true, Ordering::Relaxed);
        shared.volume.store(1f64.to_bits(), Ordering::Relaxed);
        let (commands, receiver) = mpsc::channel();
        let worker = Worker {
            events,
            shared: shared.clone(),
            sink: match renderer {
                Some(renderer) => Sink::Renderer(renderer),
                None => Sink::Device(None),
            },
            bytes: Vec::new(),
            input_size: None,
            input_done: false,
            media: None,
            failed: false,
            playing: false,
            suspended: false,
            ended: false,
            clock: Clock::default(),
            last_report: Instant::now(),
        };
        let _ = thread::Builder::new()
            .name("AudioPlayer".to_owned())
            .spawn(move || worker.run(receiver));
        CpalPlayer {
            id,
            context_id,
            backend,
            commands,
            shared,
        }
    }

    fn send(&self, command: Command) -> Result<(), PlayerError> {
        self.commands
            .send(command)
            .map_err(|_| PlayerError::Backend("the player has stopped".to_owned()))
    }
}

impl Drop for CpalPlayer {
    fn drop(&mut self) {
        let (tx_ack, rx_ack) = mpsc::channel();
        let sent = self.backend.lock().unwrap().send(BackendMsg::Shutdown {
            context: self.context_id,
            id: self.id,
            tx_ack,
        });
        if sent.is_ok() {
            let _ = rx_ack.recv();
        }
    }
}

impl MediaInstance for CpalPlayer {
    fn get_id(&self) -> usize {
        self.id
    }

    fn mute(&self, muted: bool) -> Result<(), MediaInstanceError> {
        self.set_mute(muted).map_err(|_| MediaInstanceError)
    }

    fn suspend(&self) -> Result<(), MediaInstanceError> {
        self.send(Command::Suspend(true)).map_err(|_| MediaInstanceError)
    }

    fn resume(&self) -> Result<(), MediaInstanceError> {
        self.send(Command::Suspend(false)).map_err(|_| MediaInstanceError)
    }
}

impl Player for CpalPlayer {
    fn play(&self) -> Result<(), PlayerError> {
        self.shared.paused.store(false, Ordering::Relaxed);
        self.send(Command::Play)
    }

    fn pause(&self) -> Result<(), PlayerError> {
        self.shared.paused.store(true, Ordering::Relaxed);
        self.send(Command::Pause)
    }

    fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn stop(&self) -> Result<(), PlayerError> {
        self.shared.paused.store(true, Ordering::Relaxed);
        self.send(Command::Stop)
    }

    fn seek(&self, time: f64) -> Result<(), PlayerError> {
        self.send(Command::Seek(time))
    }

    fn seekable(&self) -> Vec<Range<f64>> {
        self.shared.duration().map(|d| 0.0..d).into_iter().collect()
    }

    fn set_mute(&self, muted: bool) -> Result<(), PlayerError> {
        self.shared.muted.store(muted, Ordering::Relaxed);
        self.send(Command::Volume(self.shared.gain()))
    }

    fn muted(&self) -> bool {
        self.shared.muted.load(Ordering::Relaxed)
    }

    fn set_volume(&self, volume: f64) -> Result<(), PlayerError> {
        self.shared.volume.store(volume.to_bits(), Ordering::Relaxed);
        self.send(Command::Volume(self.shared.gain()))
    }

    fn volume(&self) -> f64 {
        f64::from_bits(self.shared.volume.load(Ordering::Relaxed))
    }

    fn set_input_size(&self, size: u64) -> Result<(), PlayerError> {
        self.send(Command::InputSize(size))
    }

    fn set_seekable(&self, _: bool) -> Result<(), PlayerError> {
        Ok(())
    }

    fn set_playback_rate(&self, rate: f64) -> Result<(), PlayerError> {
        if rate == 1. {
            Ok(())
        } else {
            Err(PlayerError::Backend(
                "only a playback rate of 1 is supported".to_owned(),
            ))
        }
    }

    fn playback_rate(&self) -> f64 {
        1.
    }

    fn push_data(&self, data: Vec<u8>) -> Result<(), PlayerError> {
        self.send(Command::Data(data))
            .map_err(|_| PlayerError::BufferPushFailed)
    }

    fn end_of_stream(&self) -> Result<(), PlayerError> {
        self.send(Command::EndOfStream)
            .map_err(|_| PlayerError::EOSFailed)
    }

    fn buffered(&self) -> Vec<Range<f64>> {
        self.seekable()
    }

    fn set_stream(&self, _: &MediaStreamId, _: bool) -> Result<(), PlayerError> {
        Err(PlayerError::SetStreamFailed)
    }

    fn render_use_gl(&self) -> bool {
        false
    }

    fn set_audio_track(&self, _: i32, _: bool) -> Result<(), PlayerError> {
        Ok(())
    }

    fn set_video_track(&self, _: i32, _: bool) -> Result<(), PlayerError> {
        Err(PlayerError::SetTrackFailed)
    }
}

enum Sink {
    /// The element's `MediaElementAudioSourceNode`.
    Renderer(Arc<Mutex<dyn AudioRenderer>>),
    /// The default output device, opened on first play.
    Device(Option<(Output, Converter)>),
}

/// Where playback is: the media time it started from, and how much has been sent since.
#[derive(Default)]
struct Clock {
    /// Media time (seconds) at `since`.
    from: f64,
    /// When playback last started, for the renderer; None while paused.
    since: Option<Instant>,
    /// Frames of media sent since `from`.
    sent: u64,
    /// The device's played-frame count at `from`.
    device_start: u64,
    /// The decoder has nothing more.
    drained: bool,
}

struct Worker {
    events: GenericCallback<PlayerEvent>,
    shared: Arc<Shared>,
    sink: Sink,
    bytes: Vec<u8>,
    input_size: Option<u64>,
    input_done: bool,
    media: Option<Media>,
    failed: bool,
    playing: bool,
    suspended: bool,
    ended: bool,
    clock: Clock,
    last_report: Instant,
}

impl Worker {
    fn run(mut self, commands: Receiver<Command>) {
        // The element holds its data until the player asks for it; this player takes it all.
        self.notify(PlayerEvent::NeedData);
        loop {
            let command = if self.active() {
                match commands.recv_timeout(TICK) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match commands.recv() {
                    Ok(command) => Some(command),
                    Err(_) => return,
                }
            };
            if let Some(command) = command {
                self.handle(command);
            }
            if self.active() {
                if let Err(error) = self.feed() {
                    self.fail(error);
                }
                if self.last_report.elapsed() >= POSITION_INTERVAL {
                    self.last_report = Instant::now();
                    let position = self.position();
                    self.notify(PlayerEvent::PositionChanged(position));
                }
            }
        }
    }

    fn active(&self) -> bool {
        self.playing && !self.suspended && !self.ended && self.media.is_some()
    }

    fn notify(&self, event: PlayerEvent) {
        let _ = self.events.send(event);
    }

    fn fail(&mut self, error: String) {
        log::warn!("Audio player: {error}");
        self.failed = true;
        self.playing = false;
        self.notify(PlayerEvent::Error(error));
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Data(data) => {
                if !self.input_done {
                    self.bytes.extend_from_slice(&data);
                    if self.input_size.is_some_and(|size| self.bytes.len() as u64 >= size) {
                        self.input_complete();
                    }
                }
            },
            Command::InputSize(size) => self.input_size = Some(size),
            Command::EndOfStream => self.input_complete(),
            Command::Play => {
                self.playing = true;
                if self.media.is_some() {
                    self.start();
                }
            },
            Command::Pause => {
                self.playing = false;
                if self.media.is_some() {
                    self.halt();
                    self.notify(PlayerEvent::StateChanged(PlaybackState::Paused));
                }
            },
            Command::Stop => {
                self.playing = false;
                self.halt();
                self.notify(PlayerEvent::StateChanged(PlaybackState::Stopped));
            },
            Command::Seek(time) => self.seek(time),
            Command::Volume(gain) => {
                if let Sink::Device(Some((output, _))) = &self.sink {
                    output.set_volume(gain);
                }
            },
            Command::Suspend(suspended) => {
                self.suspended = suspended;
                if suspended {
                    self.halt();
                } else if self.playing {
                    self.start();
                }
            },
        }
    }

    /// Every byte is here: read the file and report what it is.
    fn input_complete(&mut self) {
        if self.input_done {
            return;
        }
        self.input_done = true;
        match Media::open(std::mem::take(&mut self.bytes)) {
            Ok(media) => {
                let duration = media.duration();
                self.shared
                    .duration
                    .store(duration.unwrap_or(0.).to_bits(), Ordering::Relaxed);
                let duration = duration.map(Duration::from_secs_f64);
                self.notify(PlayerEvent::DurationChanged(duration));
                self.notify(PlayerEvent::MetadataUpdated(Metadata {
                    duration,
                    width: 0,
                    height: 0,
                    format: media.format_name.to_owned(),
                    is_seekable: true,
                    video_tracks: Vec::new(),
                    audio_tracks: vec!["audio".to_owned()],
                    is_live: false,
                    title: None,
                }));
                self.media = Some(media);
                // Ready to play, as GStreamer's player is once it has prerolled.
                self.notify(PlayerEvent::StateChanged(PlaybackState::Paused));
                if self.playing {
                    self.start();
                }
            },
            Err(error) => self.fail(error),
        }
    }

    fn position(&self) -> f64 {
        let Some(media) = &self.media else { return 0. };
        let heard = match &self.sink {
            Sink::Renderer(_) => self
                .clock
                .since
                .map_or(0., |since| since.elapsed().as_secs_f64())
                .min(self.clock.sent as f64 / media.rate as f64),
            Sink::Device(Some((output, _))) => {
                output.played_frames().saturating_sub(self.clock.device_start) as f64 /
                    output.rate as f64
            },
            Sink::Device(None) => 0.,
        };
        let position = self.clock.from + heard;
        self.shared.duration().map_or(position, |d| position.min(d))
    }

    fn start(&mut self) {
        if self.suspended || self.ended {
            return;
        }
        let media_rate = self.media.as_ref().map_or(48_000, |m| m.rate);
        match &mut self.sink {
            Sink::Renderer(_) => self.clock.since = Some(Instant::now()),
            Sink::Device(device) => {
                if device.is_none() {
                    match Output::open(media_rate, LEAD.as_secs_f64()) {
                        Ok(output) => {
                            output.set_volume(self.shared.gain());
                            let converter = Converter::new(media_rate as f32, output.rate, output.channels);
                            self.clock.device_start = output.played_frames();
                            *device = Some((output, converter));
                        },
                        Err(error) => return self.fail(error),
                    }
                }
                if let Some((output, _)) = device {
                    output.play();
                }
            },
        }
        self.last_report = Instant::now();
        self.notify(PlayerEvent::StateChanged(PlaybackState::Playing));
    }

    fn halt(&mut self) {
        match &self.sink {
            Sink::Renderer(_) => {
                // The source node plays out what it holds even while the element is paused, so
                // playback resumes after everything sent so far.
                if let Some(media) = &self.media {
                    self.clock.from += self.clock.sent as f64 / media.rate as f64;
                }
                self.clock.since = None;
                self.clock.sent = 0;
            },
            Sink::Device(Some((output, _))) => output.pause(),
            Sink::Device(None) => {},
        }
    }

    fn seek(&mut self, time: f64) {
        let Some(media) = &mut self.media else { return };
        if let Err(error) = media.seek(time) {
            log::warn!("Audio player: seek to {time} failed: {error}");
        }
        let time = self.shared.duration().map_or(time, |d| time.min(d)).max(0.);
        let device_start = match &mut self.sink {
            Sink::Device(Some((output, converter))) => {
                output.clear();
                converter.reset();
                output.played_frames()
            },
            _ => 0,
        };
        self.clock = Clock {
            from: time,
            since: matches!(self.sink, Sink::Renderer(_)).then(Instant::now).filter(|_| self.playing),
            sent: 0,
            device_start,
            drained: false,
        };
        self.ended = false;
        self.notify(PlayerEvent::SeekDone(time));
        self.notify(PlayerEvent::PositionChanged(time));
    }

    /// Decodes until the sink holds `LEAD` ahead of what is heard; reports the end once it has
    /// all been heard.
    fn feed(&mut self) -> Result<(), String> {
        let Some((rate, channels)) = self.media.as_ref().map(|m| (m.rate as f64, m.channels)) else {
            return Ok(());
        };
        let gain = self.shared.gain();
        loop {
            let ahead = match &self.sink {
                Sink::Renderer(_) => {
                    let heard = self.clock.since.map_or(0., |s| s.elapsed().as_secs_f64());
                    self.clock.sent as f64 / rate - heard
                },
                Sink::Device(Some((output, _))) => output.queued_frames() as f64 / output.rate as f64,
                Sink::Device(None) => return Ok(()),
            };
            if self.clock.drained {
                // Everything is queued: the end comes when it has played out.
                if ahead <= 0. {
                    self.ended = true;
                    self.halt();
                    self.notify(PlayerEvent::PositionChanged(self.position()));
                    self.notify(PlayerEvent::EndOfStream);
                }
                return Ok(());
            }
            if ahead >= LEAD.as_secs_f64() {
                return Ok(());
            }
            let mut planar = vec![Vec::new(); channels];
            let media = self.media.as_mut().ok_or("no media")?;
            if media.next(&mut planar)?.is_none() {
                self.clock.drained = true;
                continue;
            }
            let frames = planar.first().map_or(0, Vec::len);
            self.clock.sent += frames as u64;
            match &mut self.sink {
                Sink::Renderer(renderer) => {
                    let mut renderer = renderer.lock().unwrap();
                    for (index, mut samples) in planar.into_iter().enumerate() {
                        if gain != 1. {
                            samples.iter_mut().for_each(|s| *s *= gain);
                        }
                        renderer.render(Box::new(samples), 1 << index);
                    }
                },
                Sink::Device(Some((output, converter))) => {
                    let refs: Vec<&[f32]> = planar.iter().map(Vec::as_slice).collect();
                    let mut interleaved = Vec::new();
                    converter.convert(&refs, &mut interleaved);
                    output.push(&interleaved);
                },
                Sink::Device(None) => {},
            }
        }
    }
}
