//! Clipboard sync (local side): text and images are sent right away. Copied
//! files are only announced to the host; they travel when they are pasted
//! there. The host's copied files sit on our clipboard as virtual files and
//! are fetched when something here pastes them (nya_win::clipboard_files).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use nya_proto::pb::{self, control_msg::Msg};
use nya_win::clipboard;
use tokio::sync::mpsc::UnboundedSender;

use crate::events::NetCmd;

const MAX_TEXT: usize = 1 << 20;
const MAX_IMAGE: usize = 64 << 20;

/// Clipboard content received from the host.
pub enum ClipIn {
    Text(String),
    Image(Vec<u8>),
    /// The host copied files (offer id, what they are): put them on our clipboard.
    Offer(u64, Vec<nya_win::clipboard_files::VirtualFile>),
}

/// How long a paste may wait for the host's files.
const PASTE_TIMEOUT: Duration = Duration::from_secs(3600);

/// Fetch the host's files of `id` through the network task.
fn provider(id: u64, net: UnboundedSender<NetCmd>) -> nya_win::clipboard_files::Provider {
    std::sync::Arc::new(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        tracing::info!("host files pasted here; fetching (offer {id:016x})");
        net.send(NetCmd::ClipboardPaste(id, tx)).map_err(|_| "会话已结束".to_string())?;
        match rx.recv_timeout(PASTE_TIMEOUT) {
            Ok(r) => r,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err("等待被控端的文件超时".into()),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err("与被控端的连接断开了".into()),
        }
    })
}

/// An offer's list as virtual files.
pub fn virtual_files(files: &[pb::FileEntry]) -> Vec<nya_win::clipboard_files::VirtualFile> {
    files
        .iter()
        .map(|f| nya_win::clipboard_files::VirtualFile {
            path: if f.path.is_empty() { f.name.clone() } else { f.path.clone() },
            size: f.size,
            dir: f.is_dir,
        })
        .collect()
}

fn hash(b: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// `files`: the host takes part in copying files (FEATURE_CLIPBOARD_FILES).
pub fn spawn(remote: Receiver<ClipIn>, net: UnboundedSender<NetCmd>, files: bool) {
    std::thread::Builder::new()
        .name("nya-clipboard".into())
        .spawn(move || {
            let mut last_seq = clipboard::sequence_number();
            let mut last_text: Option<String> = None;
            let mut last_image: Option<u64> = None;
            let virtual_files = files.then(nya_win::clipboard_files::VirtualClipboard::start);
            // Copied files that could not be read yet (clipboard busy: other
            // programs read every change too): tries left.
            let mut file_retries = 0u32;
            loop {
                let applied = match remote.recv_timeout(Duration::from_millis(300)) {
                    Ok(ClipIn::Text(text)) => {
                        if last_text.as_deref() == Some(text.as_str()) {
                            false
                        } else {
                            let ok = clipboard::set_text(&text).is_ok();
                            last_text = Some(text);
                            ok
                        }
                    }
                    Ok(ClipIn::Image(dib)) => {
                        last_image = Some(hash(&dib));
                        clipboard::set_dib(&dib).is_ok()
                    }
                    Ok(ClipIn::Offer(id, files)) => {
                        if let Some(v) = &virtual_files {
                            v.offer(files, provider(id, net.clone()));
                        }
                        false
                    }
                    Err(RecvTimeoutError::Timeout) => false,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                if applied {
                    last_seq = clipboard::sequence_number();
                }
                let seq = clipboard::sequence_number();
                // Reading again copied files the clipboard was too busy for.
                let retry = seq == last_seq;
                if retry {
                    if file_retries == 0 {
                        continue;
                    }
                    file_retries -= 1;
                } else {
                    file_retries = 0;
                }
                last_seq = seq;
                let ours = virtual_files.as_ref().is_some_and(|v| v.is_ours());
                let cmd = if ours {
                    // The host's files, not fetched yet: nothing to send back.
                    None
                } else if files && clipboard::has_files() {
                    match clipboard::get_files() {
                        // Every copy is a new offer (the clipboard changed), even of the same files.
                        Ok(Some(paths)) if !paths.is_empty() => {
                            file_retries = 0;
                            Some(NetCmd::OfferFiles(paths))
                        }
                        Err(e) => {
                            if !retry {
                                file_retries = 10;
                            } else if file_retries == 0 {
                                tracing::warn!("copied files not offered: {e:#}");
                            }
                            None
                        }
                        _ => {
                            file_retries = 0;
                            None
                        }
                    }
                } else if clipboard::has_text() {
                    match clipboard::get_text() {
                        Ok(Some(text)) if text.len() <= MAX_TEXT && last_text.as_deref() != Some(text.as_str()) => {
                            last_text = Some(text.clone());
                            Some(NetCmd::Control(pb::ControlMsg { msg: Some(Msg::ClipboardText(pb::ClipboardText { text })) }))
                        }
                        _ => None,
                    }
                } else if clipboard::has_image() && !clipboard::has_files() {
                    match clipboard::get_dib() {
                        Ok(Some(dib)) if dib.len() <= MAX_IMAGE && last_image != Some(hash(&dib)) => {
                            last_image = Some(hash(&dib));
                            Some(NetCmd::SendImage(dib))
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(cmd) = cmd {
                    if net.send(cmd).is_err() {
                        return;
                    }
                }
            }
        })
        .expect("spawn clipboard thread");
}
