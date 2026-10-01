//! The hub connects the network side (client sessions) with the host side
//! (in-process host in standalone mode, or the helper process behind a named
//! pipe in service mode).
//!
//! * several clients can be attached: one operates the host (the
//!   controller), the others watch the same picture (FEATURE_MULTI_CLIENT)
//! * commands flow client session → hub → current host; sessions only send
//!   the controller's requests (`Attachment::role`)
//! * events flow host → hub → every attached session; clipboard and gamepad
//!   events only to the controller. Video waits for the controller (flow
//!   control) but never for a watching client: one that falls behind loses
//!   frames (`Attachment::video_dropped`) and asks for a keyframe
//! * `generation` increments whenever the host (helper) restarts, so the
//!   controller can re-send its stream request

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nya_proto::pb;
use tokio::sync::{mpsc, watch, Notify};

use crate::ipc_pb::{host_command::Cmd, host_event::Ev, HostCommand, HostEvent};

pub struct Attachment {
    pub token: u64,
    pub events: mpsc::Receiver<HostEvent>,
    /// Fires when this session is replaced or disconnected locally / by another client.
    pub kicked: Arc<Kick>,
    /// Who operates the host (changes when control moves).
    pub role: watch::Receiver<Role>,
    /// Video was dropped for this (watching) session: it needs a keyframe.
    pub video_dropped: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Role {
    pub controlling: bool,
    pub controller: String,
    pub viewers: Vec<String>,
}

impl Role {
    pub fn to_pb(&self) -> pb::SessionRole {
        pb::SessionRole { controlling: self.controlling, controller: self.controller.clone(), viewers: self.viewers.clone() }
    }
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
    name: String,
    /// The client's certificate fingerprint: one client, one session.
    client: String,
    tx: mpsc::Sender<HostEvent>,
    kicked: Arc<Kick>,
    role: watch::Sender<Role>,
    video_dropped: Arc<AtomicBool>,
}

#[derive(Default)]
struct Clients {
    /// Oldest first.
    subs: Vec<Subscriber>,
    controller: Option<u64>,
}

impl Clients {
    /// Tell every session its role.
    fn broadcast(&self) {
        let controller = self.controller.and_then(|t| self.subs.iter().find(|s| s.token == t)).map(|s| s.name.clone()).unwrap_or_default();
        let viewers: Vec<String> = self.subs.iter().filter(|s| Some(s.token) != self.controller).map(|s| s.name.clone()).collect();
        for s in &self.subs {
            let role = Role { controlling: Some(s.token) == self.controller, controller: controller.clone(), viewers: viewers.clone() };
            s.role.send_if_modified(|r| {
                let changed = *r != role;
                *r = role;
                changed
            });
        }
    }
}

/// State of the picture, for clients that join while it runs.
#[derive(Default)]
struct Picture {
    /// The running streams, by slot.
    streams: BTreeMap<u32, pb::StreamStarted>,
    displays: Option<pb::DisplayChanged>,
}

pub struct Hub {
    cmd_tx: mpsc::UnboundedSender<HostCommand>,
    clients: Mutex<Clients>,
    next_token: Mutex<u64>,
    picture: Mutex<Picture>,
    pub session_info: watch::Sender<Option<pb::SessionInfo>>,
    pub generation: watch::Sender<u64>,
}

impl Hub {
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<HostCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let hub = Arc::new(Self {
            cmd_tx,
            clients: Mutex::new(Clients::default()),
            next_token: Mutex::new(1),
            picture: Mutex::new(Picture::default()),
            session_info: watch::channel(None).0,
            generation: watch::channel(0).0,
        });
        (hub, cmd_rx)
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(HostCommand { cmd: Some(cmd) });
    }

    /// Attach a client session. With `shared` (the client can watch) it
    /// operates the host only if nobody does; otherwise it replaces every
    /// attached session. A client reconnecting (`client`: its fingerprint)
    /// replaces its own previous session, which may not have noticed the
    /// connection loss yet, and keeps its role.
    pub fn attach(&self, name: &str, client: &str, shared: bool) -> Attachment {
        let (tx, rx) = mpsc::channel(256);
        let kicked = Arc::new(Kick::default());
        let video_dropped = Arc::new(AtomicBool::new(false));
        let token = {
            let mut t = self.next_token.lock().unwrap();
            *t += 1;
            *t
        };
        let (role_tx, role_rx) = watch::channel(Role::default());
        let mut c = self.clients.lock().unwrap();
        if !shared {
            for s in &c.subs {
                s.kicked.fire("另一个客户端已连接");
            }
        }
        let mut was_controller = false;
        if !client.is_empty() {
            let controller = c.controller;
            c.subs.retain(|s| {
                if s.client != client {
                    return true;
                }
                s.kicked.fire("同一客户端已重新连接");
                was_controller |= Some(s.token) == controller;
                false
            });
        }
        c.subs.push(Subscriber {
            token,
            name: name.to_owned(),
            client: client.to_owned(),
            tx,
            kicked: kicked.clone(),
            role: role_tx,
            video_dropped: video_dropped.clone(),
        });
        let controller_gone = c.controller.is_none_or(|t| !c.subs.iter().any(|s| s.token == t));
        if !shared || was_controller || controller_gone {
            c.controller = Some(token);
        }
        c.broadcast();
        Attachment { token, events: rx, kicked, role: role_rx, video_dropped }
    }

    /// Session `token` operates the host from now on; the previous operator
    /// watches, or is disconnected with `kick`.
    pub fn take_control(&self, token: u64, kick: bool) {
        let mut c = self.clients.lock().unwrap();
        if c.controller == Some(token) || !c.subs.iter().any(|s| s.token == token) {
            return;
        }
        let name = c.subs.iter().find(|s| s.token == token).map(|s| s.name.clone()).unwrap_or_default();
        if let Some(old) = c.controller.and_then(|t| c.subs.iter().find(|s| s.token == t)) {
            if kick {
                old.kicked.fire(&format!("{name} 接管了被控端并断开了你的连接"));
            }
        }
        c.controller = Some(token);
        c.broadcast();
        drop(c);
        self.send(Cmd::ControllerChanged(Default::default()));
    }

    /// The operating session, if any.
    pub fn controller(&self) -> Option<u64> {
        self.clients.lock().unwrap().controller
    }

    /// Drop one session.
    pub fn kick_one(&self, token: u64, reason: &str) {
        if let Some(s) = self.clients.lock().unwrap().subs.iter().find(|s| s.token == token) {
            s.kicked.fire(reason);
        }
    }

    /// Drop every attached session. Returns whether there was one.
    pub fn kick(&self, reason: &str) -> bool {
        let c = self.clients.lock().unwrap();
        for s in &c.subs {
            s.kicked.fire(reason);
        }
        !c.subs.is_empty()
    }

    pub fn detach(&self, token: u64) {
        let mut c = self.clients.lock().unwrap();
        let before = c.subs.len();
        c.subs.retain(|s| s.token != token);
        if c.subs.len() == before {
            return;
        }
        if c.subs.is_empty() {
            c.controller = None;
            drop(c);
            *self.picture.lock().unwrap() = Picture::default();
            self.send(Cmd::ClientGone(Default::default()));
            return;
        }
        if c.controller == Some(token) {
            // The longest-watching client operates the host now.
            c.controller = c.subs.first().map(|s| s.token);
            c.broadcast();
            drop(c);
            self.send(Cmd::ControllerChanged(Default::default()));
        } else {
            c.broadcast();
        }
    }

    /// The running streams and displays, for a client joining now.
    pub fn picture(&self) -> (Vec<pb::StreamStarted>, Option<pb::DisplayChanged>) {
        let p = self.picture.lock().unwrap();
        (p.streams.values().cloned().collect(), p.displays.clone())
    }

    /// Called by the host side for every event.
    pub async fn publish(&self, ev: HostEvent) {
        match &ev.ev {
            Some(Ev::SessionInfo(info)) => {
                self.session_info.send_replace(Some(info.clone()));
            }
            Some(Ev::StreamStarted(s)) => {
                self.picture.lock().unwrap().streams.insert(s.slot, s.clone());
            }
            Some(Ev::DisplayChanged(d)) => {
                self.picture.lock().unwrap().displays = Some(d.clone());
            }
            _ => {}
        }
        // The operating client's own business.
        let controller_only = matches!(
            &ev.ev,
            Some(Ev::Clipboard(_) | Ev::ClipboardFiles(_) | Ev::ClipboardImage(_) | Ev::ClipboardPaste(_) | Ev::GamepadRumble(_))
        );
        let targets: Vec<(mpsc::Sender<HostEvent>, bool, Arc<AtomicBool>)> = {
            let c = self.clients.lock().unwrap();
            c.subs
                .iter()
                .map(|s| (s.tx.clone(), Some(s.token) == c.controller, s.video_dropped.clone()))
                .filter(|(_, controlling, _)| *controlling || !controller_only)
                .collect()
        };
        let video = matches!(ev.ev, Some(Ev::Video(_)));
        for (tx, controlling, dropped) in targets {
            match &ev.ev {
                // Audio is disposable; never let it stall the pipe.
                Some(Ev::Audio(_)) => {
                    let _ = tx.try_send(ev.clone());
                }
                // The operating client paces the host; watchers never stall it.
                _ if controlling => {
                    let _ = tx.send(ev.clone()).await;
                }
                _ => {
                    if tx.try_send(ev.clone()).is_err() && video {
                        dropped.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// The host (helper) was (re)started.
    pub fn host_restarted(&self) {
        self.generation.send_modify(|g| *g += 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(a: &Attachment) -> Role {
        a.role.borrow().clone()
    }

    #[tokio::test]
    async fn one_operates_the_others_watch() {
        let (hub, mut cmds) = Hub::new();
        let a = hub.attach("A", "a", true);
        let b = hub.attach("B", "b", true);
        assert!(role(&a).controlling && !role(&b).controlling);
        assert_eq!((role(&b).controller.as_str(), role(&b).viewers.clone()), ("A", vec!["B".to_string()]));

        // B takes over; A watches.
        hub.take_control(b.token, false);
        assert!(!role(&a).controlling && role(&b).controlling);
        assert!(matches!(cmds.try_recv().unwrap().cmd, Some(Cmd::ControllerChanged(_))));

        // B leaves: A operates again; the last one leaving ends it all.
        hub.detach(b.token);
        assert!(role(&a).controlling && role(&a).viewers.is_empty());
        assert!(matches!(cmds.try_recv().unwrap().cmd, Some(Cmd::ControllerChanged(_))));
        hub.detach(a.token);
        assert!(matches!(cmds.try_recv().unwrap().cmd, Some(Cmd::ClientGone(_))));
    }

    #[tokio::test]
    async fn kick_and_old_clients() {
        let (hub, _cmds) = Hub::new();
        let a = hub.attach("A", "a", true);
        let b = hub.attach("B", "b", true);
        hub.take_control(b.token, true);
        assert_eq!(a.kicked.reason(), "B 接管了被控端并断开了你的连接");
        // A client that cannot watch replaces everyone.
        let c = hub.attach("old", "c", false);
        assert_eq!(b.kicked.reason(), "另一个客户端已连接");
        assert!(role(&c).controlling);
    }

    #[tokio::test]
    async fn reconnecting_client_replaces_its_old_session() {
        let (hub, mut cmds) = Hub::new();
        let a = hub.attach("A", "a", true);
        let b = hub.attach("B", "b", true);
        // A's network dropped; it reconnects before the old session noticed.
        let a2 = hub.attach("A", "a", true);
        assert_eq!(a.kicked.reason(), "同一客户端已重新连接");
        assert!(role(&a2).controlling && !role(&b).controlling, "A keeps operating");
        assert_eq!(role(&b).viewers, vec!["B".to_string()]);
        // The old session ending later changes nothing.
        hub.detach(a.token);
        assert!(role(&a2).controlling);
        assert!(cmds.try_recv().is_err());
        // A watcher reconnecting stays a watcher.
        let b2 = hub.attach("B", "b", true);
        assert!(!role(&b2).controlling && role(&a2).controlling);
    }

    #[tokio::test]
    async fn watchers_never_stall_the_host() {
        let (hub, _cmds) = Hub::new();
        let mut a = hub.attach("A", "a", true);
        let b = hub.attach("B", "b", true);
        let frame = || HostEvent { ev: Some(Ev::Video(Default::default())) };
        // More frames than B's queue holds, while A keeps reading.
        for _ in 0..300 {
            hub.publish(frame()).await;
            a.events.try_recv().unwrap();
        }
        assert!(b.video_dropped.load(Ordering::Relaxed));
        assert!(!a.video_dropped.load(Ordering::Relaxed));
        // Clipboard goes to the operator only.
        hub.publish(HostEvent { ev: Some(Ev::Clipboard(Default::default())) }).await;
        assert!(a.events.try_recv().is_ok());
    }
}
