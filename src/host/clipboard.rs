//! Text clipboard sync. Polls the clipboard sequence number; remembers the
//! last text we exchanged to avoid ping-pong.

use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use nya_proto::pb;
use nya_win::clipboard;

use super::Sink;
use crate::ipc_pb::host_event::Ev;

pub enum ClipCmd {
    Set(String),
    Enable(bool),
}

const MAX_TEXT: usize = 1 << 20;

pub fn thread(rx: Receiver<ClipCmd>, sink: Sink) {
    let mut enabled = false;
    let mut last_seq = clipboard::sequence_number();
    let mut last_text: Option<String> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(ClipCmd::Set(text)) => {
                if last_text.as_deref() != Some(text.as_str()) {
                    match clipboard::set_text(&text) {
                        Ok(()) => {
                            last_text = Some(text);
                            last_seq = clipboard::sequence_number();
                        }
                        Err(e) => tracing::debug!("set clipboard: {e:#}"),
                    }
                }
            }
            Ok(ClipCmd::Enable(on)) => {
                enabled = on;
                last_seq = clipboard::sequence_number();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if !enabled {
            continue;
        }
        let seq = clipboard::sequence_number();
        if seq == last_seq {
            continue;
        }
        last_seq = seq;
        if let Ok(Some(text)) = clipboard::get_text() {
            if text.len() <= MAX_TEXT && last_text.as_deref() != Some(text.as_str()) {
                last_text = Some(text.clone());
                sink.send(Ev::Clipboard(pb::ClipboardText { text }));
            }
        }
    }
}
