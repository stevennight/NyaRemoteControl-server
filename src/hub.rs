//! The hub connects the network side (one active client session) with the
//! host side (in-process host in standalone mode, or the helper process
//! behind a named pipe in service mode).
//!
//! * commands flow client session → hub → current host
//! * events flow host → hub → the attached client session
//! * `generation` increments whenever the host (helper) restarts, so the
//!   session can re-send its stream request

use std::sync::{Arc, Mutex};

use nya_proto::pb;
use tokio::sync::{mpsc, watch, Notify};

use crate::ipc_pb::{host_command::Cmd, host_event::Ev, HostCommand, HostEvent};

pub struct Attachment {
    pub token: u64,
    pub events: mpsc::Receiver<HostEvent>,
    /// Fires when a newer session replaces this one or it is disconnected locally.
    pub kicked: Arc<Kick>,
}

#[derive(Default)]
pub struct Kick {
    pub notify: Notify,
    reason: Mutex<String>,
}

impl Kick {
    fn fire(&self, reason: &str) {
        *self.reason.lock().unwrap() = reason.to_owned();
        self.notify.notify_one();
    }

    pub fn reason(&self) -> String {
        self.reason.lock().unwrap().clone()
    }
}

struct Subscriber {
    token: u64,
    tx: mpsc::Sender<HostEvent>,
    kicked: Arc<Kick>,
}

pub struct Hub {
    cmd_tx: mpsc::UnboundedSender<HostCommand>,
    subscriber: Mutex<Option<Subscriber>>,
    next_token: Mutex<u64>,
    pub session_info: watch::Sender<Option<pb::SessionInfo>>,
    pub generation: watch::Sender<u64>,
}

impl Hub {
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<HostCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let hub = Arc::new(Self {
            cmd_tx,
            subscriber: Mutex::new(None),
            next_token: Mutex::new(1),
            session_info: watch::channel(None).0,
            generation: watch::channel(0).0,
        });
        (hub, cmd_rx)
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(HostCommand { cmd: Some(cmd) });
    }

    /// Attach a client session; any previous session is kicked.
    pub fn attach(&self) -> Attachment {
        let (tx, rx) = mpsc::channel(256);
        let kicked = Arc::new(Kick::default());
        let token = {
            let mut t = self.next_token.lock().unwrap();
            *t += 1;
            *t
        };
        let old = self.subscriber.lock().unwrap().replace(Subscriber { token, tx, kicked: kicked.clone() });
        if let Some(old) = old {
            old.kicked.fire("另一个客户端已连接");
        }
        Attachment { token, events: rx, kicked }
    }

    /// Drop the attached session, if any. Returns whether there was one.
    pub fn kick(&self, reason: &str) -> bool {
        let s = self.subscriber.lock().unwrap();
        if let Some(s) = s.as_ref() {
            s.kicked.fire(reason);
        }
        s.is_some()
    }

    pub fn detach(&self, token: u64) {
        let mut s = self.subscriber.lock().unwrap();
        if s.as_ref().is_some_and(|s| s.token == token) {
            *s = None;
            drop(s);
            self.send(Cmd::ClientGone(Default::default()));
        }
    }

    /// Called by the host side for every event.
    pub async fn publish(&self, ev: HostEvent) {
        if let Some(Ev::SessionInfo(info)) = &ev.ev {
            self.session_info.send_replace(Some(info.clone()));
        }
        let tx = self.subscriber.lock().unwrap().as_ref().map(|s| s.tx.clone());
        let Some(tx) = tx else { return };
        match &ev.ev {
            // Audio is disposable; never let it stall the pipe.
            Some(Ev::Audio(_)) => {
                let _ = tx.try_send(ev);
            }
            _ => {
                let _ = tx.send(ev).await;
            }
        }
    }

    /// The host (helper) was (re)started.
    pub fn host_restarted(&self) {
        self.generation.send_modify(|g| *g += 1);
    }
}
