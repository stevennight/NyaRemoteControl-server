//! What the running host knows about itself. Shared by the network side, the
//! helper manager and the control pipe (which reports and changes it).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::auth::AuthStore;
use crate::config::ServerConfig;
use crate::control_pb::{self as cpb, event::Kind};
use crate::hub::Hub;

const MAX_EVENTS: usize = 100;

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub struct State {
    pub mode: cpb::Mode,
    pub dir: PathBuf,
    pub started_unix: u64,
    pub fingerprint: String,
    pub auth: Arc<AuthStore>,
    pub hub: Arc<Hub>,
    cfg: Mutex<ServerConfig>,
    /// Bound address, or why binding failed.
    listen: Mutex<Result<String, String>>,
    host: Mutex<cpb::Host>,
    /// Connected clients by hub token, oldest first.
    sessions: Mutex<Vec<(u64, cpb::Session)>>,
    events: Mutex<VecDeque<cpb::Event>>,
    /// Restart the host (helper) so it picks up new settings.
    pub restart_host: Notify,
    /// Re-bind the network endpoint (port / address changed).
    pub rebind: Notify,
}

impl State {
    pub fn new(mode: cpb::Mode, dir: PathBuf, cfg: ServerConfig, fingerprint: String, auth: Arc<AuthStore>, hub: Arc<Hub>) -> Arc<Self> {
        Arc::new(Self {
            mode,
            dir,
            started_unix: unix_now(),
            fingerprint,
            auth,
            hub,
            cfg: Mutex::new(cfg),
            listen: Mutex::new(Err("尚未开始监听".into())),
            host: Mutex::new(cpb::Host::default()),
            sessions: Mutex::new(Vec::new()),
            events: Mutex::new(VecDeque::new()),
            restart_host: Notify::new(),
            rebind: Notify::new(),
        })
    }

    pub fn config(&self) -> ServerConfig {
        self.cfg.lock().unwrap().clone()
    }

    pub fn set_config(&self, cfg: ServerConfig) {
        *self.cfg.lock().unwrap() = cfg;
    }

    pub fn server_name(&self) -> String {
        self.cfg.lock().unwrap().display_name()
    }

    pub fn event(&self, kind: Kind, text: impl Into<String>) {
        let mut e = self.events.lock().unwrap();
        if e.len() >= MAX_EVENTS {
            e.pop_front();
        }
        e.push_back(cpb::Event { unix: unix_now(), kind: kind as i32, text: text.into() });
    }

    pub fn set_listening(&self, r: Result<String, String>) {
        *self.listen.lock().unwrap() = r;
    }

    pub fn set_host(&self, running: bool, console_session: u32) {
        let mut h = self.host.lock().unwrap();
        h.running = running;
        h.console_session = console_session;
        if !running {
            h.stream.clear();
        }
    }

    pub fn set_stream(&self, text: String) {
        self.host.lock().unwrap().stream = text;
    }

    pub fn session_started(&self, token: u64, s: cpb::Session) {
        self.sessions.lock().unwrap().push((token, s));
    }

    pub fn session_ended(&self, token: u64) {
        let mut s = self.sessions.lock().unwrap();
        s.retain(|(t, _)| *t != token);
        if s.is_empty() {
            self.host.lock().unwrap().stream.clear();
        }
    }

    /// The client `token` operates the host now (or watches).
    pub fn set_controlling(&self, token: u64, on: bool) {
        if on {
            let who = self.sessions.lock().unwrap().iter().find(|(t, _)| *t == token).map(|(_, s)| s.client_name.clone());
            if let Some(who) = who {
                self.event(Kind::Connected, format!("{who} 正在操作被控端"));
            }
        }
    }

    /// Hub tokens of the connected sessions of the client with this fingerprint (hex).
    pub fn sessions_of(&self, fingerprint: &str) -> Vec<u64> {
        self.sessions.lock().unwrap().iter().filter(|(_, s)| s.fingerprint.eq_ignore_ascii_case(fingerprint)).map(|(t, _)| *t).collect()
    }

    /// Status for the control pipe; `admin` includes the sensitive parts.
    pub fn status(&self, admin: bool) -> cpb::Status {
        let (listen, listen_error) = match &*self.listen.lock().unwrap() {
            Ok(a) => (a.clone(), String::new()),
            Err(e) => (String::new(), e.clone()),
        };
        let controller = self.hub.controller();
        let mut session = None;
        let mut viewers = Vec::new();
        for (t, s) in self.sessions.lock().unwrap().iter() {
            let mut s = s.clone();
            if !admin {
                s.fingerprint.clear();
                s.remote_addr.clear();
            }
            if Some(*t) == controller {
                session = Some(s);
            } else {
                viewers.push(s);
            }
        }
        cpb::Status {
            server_version: env!("CARGO_PKG_VERSION").into(),
            mode: self.mode as i32,
            started_unix: self.started_unix,
            server_name: self.server_name(),
            listen,
            listen_error,
            fingerprint: self.fingerprint.clone(),
            host: Some(self.host.lock().unwrap().clone()),
            session,
            recent: if admin { self.events.lock().unwrap().iter().cloned().collect() } else { Vec::new() },
            data_dir: if admin { self.dir.display().to_string() } else { String::new() },
            exe: std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
            viewers,
        }
    }
}
