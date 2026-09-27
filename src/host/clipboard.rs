//! Clipboard sync on the host. Polls the clipboard sequence number and
//! reports, in order of preference: copied files (offered to the client),
//! text, or an image. Remembers what it last exchanged to avoid ping-pong.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use nya_proto::pb;
use nya_win::clipboard;

use super::Sink;
use crate::ipc_pb::{host_event::Ev, ClipboardFiles, ClipboardImage};

pub enum ClipCmd {
    Set(String),
    SetImage(Vec<u8>),
    SetFiles(Vec<String>),
    Enable(bool),
}

const MAX_TEXT: usize = 1 << 20;
const MAX_IMAGE: usize = 64 << 20;

fn hash(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

pub fn thread(rx: Receiver<ClipCmd>, sink: Sink) {
    let mut enabled = false;
    let mut last_seq = clipboard::sequence_number();
    let mut last_text: Option<String> = None;
    let mut last_image: Option<u64> = None;
    let mut last_files: Vec<String> = Vec::new();
    loop {
        let set_result = match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(ClipCmd::Set(text)) => {
                if last_text.as_deref() == Some(text.as_str()) {
                    None
                } else {
                    let r = clipboard::set_text(&text);
                    last_text = Some(text);
                    Some(r)
                }
            }
            Ok(ClipCmd::SetImage(dib)) => {
                last_image = Some(hash(&dib));
                Some(clipboard::set_dib(&dib))
            }
            Ok(ClipCmd::SetFiles(paths)) => {
                let list: Vec<std::path::PathBuf> = paths.iter().map(Into::into).collect();
                last_files = paths;
                Some(clipboard::set_files(&list))
            }
            Ok(ClipCmd::Enable(on)) => {
                enabled = on;
                last_seq = clipboard::sequence_number();
                None
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if let Some(r) = set_result {
            match r {
                Ok(()) => last_seq = clipboard::sequence_number(),
                Err(e) => tracing::debug!("set clipboard: {e:#}"),
            }
        }
        if !enabled {
            continue;
        }
        let seq = clipboard::sequence_number();
        if seq == last_seq {
            continue;
        }
        last_seq = seq;

        if clipboard::has_files() {
            if let Ok(Some(files)) = clipboard::get_files() {
                let paths: Vec<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
                if !paths.is_empty() && paths != last_files {
                    last_files = paths.clone();
                    sink.send(Ev::ClipboardFiles(ClipboardFiles { paths }));
                }
            }
        } else if clipboard::has_text() {
            if let Ok(Some(text)) = clipboard::get_text() {
                if text.len() <= MAX_TEXT && last_text.as_deref() != Some(text.as_str()) {
                    last_text = Some(text.clone());
                    sink.send(Ev::Clipboard(pb::ClipboardText { text }));
                }
            }
        } else if clipboard::has_image() {
            if let Ok(Some(dib)) = clipboard::get_dib() {
                let h = hash(&dib);
                if dib.len() <= MAX_IMAGE && last_image != Some(h) {
                    last_image = Some(h);
                    sink.send(Ev::ClipboardImage(ClipboardImage { dib }));
                }
            }
        }
    }
}
