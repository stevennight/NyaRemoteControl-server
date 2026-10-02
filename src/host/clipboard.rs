//! Clipboard sync on the host. Polls the clipboard sequence number and
//! reports, in order of preference: copied files (offered to the client),
//! text, or an image. Remembers what it last exchanged to avoid ping-pong.
//!
//! Files copied on the client are put on the host clipboard as virtual files
//! (nya_win::clipboard_files): pasting them asks the service to fetch them
//! (`ClipboardPaste`) and waits for `ClipboardPasteDone`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use nya_proto::pb;
use nya_win::clipboard;

use super::Sink;
use crate::ipc_pb::{host_event::Ev, ClipboardFiles, ClipboardImage, ClipboardPaste};

pub enum ClipCmd {
    Set(String),
    SetImage(Vec<u8>),
    SetFiles(Vec<String>),
    /// The client copied files (offer id): put them on the clipboard.
    Offer(u64),
    /// The files of a paste in progress are here (or not).
    PasteDone(u64, Result<Vec<String>, String>),
    Enable(bool),
}

/// Pastes waiting for their files, by offer id.
type Waiters = Arc<Mutex<HashMap<u64, std::sync::mpsc::Sender<Result<Vec<PathBuf>, String>>>>>;

/// How long a paste may wait for the client's files.
const PASTE_TIMEOUT: Duration = Duration::from_secs(3600);

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
    let virtual_files = nya_win::clipboard_files::VirtualClipboard::start();
    let waiters: Waiters = Default::default();
    let fail_all = |w: &Waiters, msg: &str| {
        for (_, tx) in w.lock().unwrap().drain() {
            let _ = tx.send(Err(msg.to_owned()));
        }
    };
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
                Some(clipboard::set_files(&list))
            }
            Ok(ClipCmd::Offer(id)) => {
                let (sink, waiters) = (sink.clone(), waiters.clone());
                virtual_files.offer(std::sync::Arc::new(move || {
                    let (tx, rx) = std::sync::mpsc::channel();
                    waiters.lock().unwrap().insert(id, tx);
                    tracing::info!("client files pasted on the host; fetching (offer {id:016x})");
                    sink.send(Ev::ClipboardPaste(ClipboardPaste { transfer_id: id }));
                    let r = rx.recv_timeout(PASTE_TIMEOUT).unwrap_or_else(|_| Err("等待客户端的文件超时".into()));
                    waiters.lock().unwrap().remove(&id);
                    r
                }));
                None
            }
            Ok(ClipCmd::PasteDone(id, r)) => {
                if let Some(tx) = waiters.lock().unwrap().remove(&id) {
                    let _ = tx.send(r.map(|v| v.into_iter().map(PathBuf::from).collect()));
                }
                None
            }
            Ok(ClipCmd::Enable(on)) => {
                enabled = on;
                last_seq = clipboard::sequence_number();
                if !on {
                    // The client is gone: its files can no longer be pasted.
                    virtual_files.clear();
                    fail_all(&waiters, "客户端已断开");
                }
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

        if virtual_files.is_ours() {
            // The client's own files (not yet fetched): nothing to offer back.
        } else if clipboard::has_files() {
            if let Ok(Some(files)) = clipboard::get_files() {
                let paths: Vec<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
                // Every copy is a new offer (the clipboard changed), even of the same files.
                if !paths.is_empty() {
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
