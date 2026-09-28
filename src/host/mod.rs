//! The host: everything that must run inside the user's session — capture,
//! encoding, audio, input injection, clipboard. Runs in the helper process
//! (service mode) or in-process (standalone). Each concern gets its own thread;
//! the dispatcher routes [`HostCommand`]s to them.

mod audio;
mod clipboard;
mod cursor;
mod gamepad;
mod input;
mod mic;
mod pipeline;
pub mod select;
mod vdisplay;
mod video;

use std::thread;

use nya_proto::pb;
use tokio::sync::mpsc;

use crate::ipc_pb::{host_command::Cmd, host_event::Ev, HostCommand, HostEvent};

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub name: String,
    pub encoder: String,
    pub office_bitrate_kbps: u32,
    pub game_bitrate_kbps: u32,
    pub max_fps: u32,
    pub audio: bool,
}

impl From<&crate::config::ServerConfig> for HostConfig {
    fn from(c: &crate::config::ServerConfig) -> Self {
        Self {
            name: c.display_name(),
            encoder: c.encoder.clone(),
            office_bitrate_kbps: c.office_bitrate_kbps,
            game_bitrate_kbps: c.game_bitrate_kbps,
            max_fps: c.max_fps,
            audio: c.audio,
        }
    }
}

/// Event channel into the hub / pipe, used from plain threads.
#[derive(Clone)]
pub struct Sink(mpsc::Sender<HostEvent>);

impl Sink {
    pub fn send(&self, ev: Ev) {
        let _ = self.0.blocking_send(HostEvent { ev: Some(ev) });
    }

    /// Drop instead of waiting when the channel is full (audio).
    pub fn try_send(&self, ev: Ev) {
        let _ = self.0.try_send(HostEvent { ev: Some(ev) });
    }
}

/// Run the host until `Shutdown` or the command channel closes. Blocking.
pub fn run(mut commands: mpsc::UnboundedReceiver<HostCommand>, events: mpsc::Sender<HostEvent>, cfg: HostConfig) {
    let sink = Sink(events);
    // Before anything enumerates displays.
    vdisplay::cleanup_stale();

    let (input_tx, input_rx) = crossbeam_channel::unbounded();
    let (video_tx, video_rx) = crossbeam_channel::unbounded();
    let (audio_tx, audio_rx) = crossbeam_channel::unbounded();
    let (clip_tx, clip_rx) = crossbeam_channel::unbounded();
    let (mic_tx, mic_rx) = crossbeam_channel::bounded(256);

    let threads = vec![
        spawn("nya-video", {
            let (sink, cfg, input_tx) = (sink.clone(), cfg.clone(), input_tx.clone());
            move || video::thread(video_rx, sink, input_tx, cfg)
        }),
        spawn("nya-input", {
            let sink = sink.clone();
            move || input::thread(input_rx, sink)
        }),
        spawn("nya-audio", {
            let sink = sink.clone();
            move || audio::thread(audio_rx, sink)
        }),
        spawn("nya-mic", move || mic::thread(mic_rx)),
        spawn("nya-clipboard", {
            let sink = sink.clone();
            move || clipboard::thread(clip_rx, sink)
        }),
    ];

    while let Some(HostCommand { cmd: Some(cmd) }) = commands.blocking_recv() {
        match cmd {
            Cmd::StartStream(s) => {
                let _ = video_tx.send(video::VideoCmd::Start(s));
            }
            Cmd::StopStream(_) => {
                let _ = video_tx.send(video::VideoCmd::Stop);
            }
            Cmd::RequestKeyframe(_) => {
                let _ = video_tx.send(video::VideoCmd::Keyframe);
            }
            Cmd::SetMode(m) => {
                let mode = pb::StreamMode::try_from(m.mode).unwrap_or(pb::StreamMode::Office);
                let _ = video_tx.send(video::VideoCmd::SetMode(mode));
            }
            Cmd::ClientCaps(c) => {
                // A client just attached: clipboard sync is useful from now on.
                let _ = clip_tx.send(clipboard::ClipCmd::Enable(true));
                let _ = video_tx.send(video::VideoCmd::Caps(c));
            }
            Cmd::ClipboardImage(i) => {
                let _ = clip_tx.send(clipboard::ClipCmd::SetImage(i.dib));
            }
            Cmd::ClipboardFiles(f) => {
                let _ = clip_tx.send(clipboard::ClipCmd::SetFiles(f.paths));
            }
            Cmd::MicAudio(m) => {
                let _ = mic_tx.try_send(m.datagram);
            }
            Cmd::SetBitrate(b) => {
                let _ = video_tx.send(video::VideoCmd::SetBitrate(b.kbps));
            }
            Cmd::FrameSent(f) => {
                let _ = video_tx.send(video::VideoCmd::FrameSent(f.frame_id));
            }
            Cmd::Input(i) => {
                let _ = input_tx.send(input::InputCmd::Event(i));
            }
            Cmd::Clipboard(c) => {
                let _ = clip_tx.send(clipboard::ClipCmd::Set(c.text));
            }
            Cmd::SetAudio(a) => {
                let _ = audio_tx.send(a.enabled && cfg.audio);
            }
            Cmd::ClientGone(_) => {
                let _ = video_tx.send(video::VideoCmd::Stop);
                let _ = audio_tx.send(false);
                let _ = input_tx.send(input::InputCmd::ReleaseAll);
                let _ = input_tx.send(input::InputCmd::UnplugPads);
                let _ = clip_tx.send(clipboard::ClipCmd::Enable(false));
            }
            Cmd::Shutdown(_) => break,
        }
    }

    let _ = video_tx.send(video::VideoCmd::Shutdown);
    let _ = input_tx.send(input::InputCmd::ReleaseAll);
    let _ = input_tx.send(input::InputCmd::Shutdown);
    let _ = audio_tx.send(false);
    drop(audio_tx);
    drop(clip_tx);
    drop(mic_tx);
    for t in threads {
        let _ = t.join();
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> thread::JoinHandle<()> {
    thread::Builder::new().name(name.into()).spawn(f).expect("spawn host thread")
}
