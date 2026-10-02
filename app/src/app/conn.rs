//! Several sessions at once (to different hosts), each in its own window.
//!
//! The session state of one window lives in the `App` fields (the "current"
//! connection, which all session code works on); the others are parked in
//! `App::others` as `Conn`s. Before handling anything for a connection, the
//! app swaps it in (`activate`): window events by window id, session events
//! by the connection id they are tagged with (`Ui::for_conn`), global
//! keyboard and raw mouse input to the focused one.
//!
//! One idle connection (hidden window, no session) is kept ready for the
//! next connection attempt.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use winit::dpi::LogicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

use super::{device_for_window, extra, hwnd, App};
use crate::config::Defaults;
use crate::input;
use crate::render::Renderer;
use crate::session::Session;
use nya_ui::Gui;

/// The per-window session state, as parked while another one is current.
#[derive(Default)]
pub(super) struct Conn {
    id: u64,
    sd: Defaults,
    session_host: String,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    gui: Option<Gui>,
    adapter_luid: u64,
    session: Option<Session>,
    focused: bool,
    fullscreen: bool,
    toolbar_open: bool,
    cursor_over_ui: bool,
    remote_buttons: u8,
    repaint_at: Option<Instant>,
    hovering_file: bool,
    dropped: Vec<PathBuf>,
    vd_resize_at: Option<Instant>,
    extras: std::collections::HashMap<WindowId, extra::ExtraWindow>,
    open_requests: Vec<u32>,
    auto_opened: extra::AutoOpened,
    sync_extras: bool,
    pending_virtual: Option<u32>,
}

impl Conn {
    fn owns(&self, id: WindowId) -> bool {
        self.window.as_ref().is_some_and(|w| w.id() == id) || self.extras.contains_key(&id)
    }

    fn idle(&self) -> bool {
        self.session.is_none()
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(h) = self.window.as_ref().and_then(|w| hwnd(w)) {
            input::set_extra_window(h, false);
        }
    }
}

impl App {
    /// Exchange the current connection with `c`.
    fn swap_conn(&mut self, c: &mut Conn) {
        use std::mem::swap;
        swap(&mut self.conn_id, &mut c.id);
        swap(&mut self.sd, &mut c.sd);
        swap(&mut self.session_host, &mut c.session_host);
        swap(&mut self.window, &mut c.window);
        swap(&mut self.renderer, &mut c.renderer);
        swap(&mut self.gui, &mut c.gui);
        swap(&mut self.adapter_luid, &mut c.adapter_luid);
        swap(&mut self.session, &mut c.session);
        swap(&mut self.focused, &mut c.focused);
        swap(&mut self.fullscreen, &mut c.fullscreen);
        swap(&mut self.toolbar_open, &mut c.toolbar_open);
        swap(&mut self.cursor_over_ui, &mut c.cursor_over_ui);
        swap(&mut self.remote_buttons, &mut c.remote_buttons);
        swap(&mut self.repaint_at, &mut c.repaint_at);
        swap(&mut self.hovering_file, &mut c.hovering_file);
        swap(&mut self.dropped, &mut c.dropped);
        swap(&mut self.vd_resize_at, &mut c.vd_resize_at);
        swap(&mut self.extras, &mut c.extras);
        swap(&mut self.open_requests, &mut c.open_requests);
        swap(&mut self.auto_opened, &mut c.auto_opened);
        swap(&mut self.sync_extras, &mut c.sync_extras);
        swap(&mut self.pending_virtual, &mut c.pending_virtual);
    }

    /// Make connection `id` the current one; false if it is gone.
    pub(super) fn activate(&mut self, id: u64) -> bool {
        if self.conn_id == id {
            return true;
        }
        let Some(i) = self.others.iter().position(|c| c.id == id) else { return false };
        let mut c = std::mem::take(&mut self.others[i]);
        self.swap_conn(&mut c);
        self.others[i] = c;
        true
    }

    /// Ids of all connections, the current one first.
    pub(super) fn conn_ids(&self) -> Vec<u64> {
        std::iter::once(self.conn_id).chain(self.others.iter().map(|c| c.id)).collect()
    }

    /// The connection owning this window (session or extra window).
    pub(super) fn conn_of_window(&self, id: WindowId) -> Option<u64> {
        if self.window.as_ref().is_some_and(|w| w.id() == id) || self.extras.contains_key(&id) {
            return Some(self.conn_id);
        }
        self.others.iter().find(|c| c.owns(id)).map(|c| c.id)
    }

    /// Make the connection whose window has the keyboard focus current.
    pub(super) fn activate_focused(&mut self) {
        let focused = |a: &App| a.focused || a.extras.values().any(|w| w.focused());
        if focused(self) {
            return;
        }
        let id = self.others.iter().find(|c| c.focused || c.extras.values().any(|w| w.focused())).map(|c| c.id);
        if let Some(id) = id {
            self.activate(id);
        }
    }

    /// Is any connection in a session?
    pub(super) fn any_session(&self) -> bool {
        self.session.is_some() || self.others.iter().any(|c| c.session.is_some())
    }

    /// Is there a session with this host (by address)?
    pub(super) fn connected_to(&self, address: &str) -> bool {
        (self.session.is_some() && self.session_host == address) || self.others.iter().any(|c| c.session.is_some() && c.session_host == address)
    }

    /// Make an idle connection current (for a session about to start).
    pub(super) fn activate_idle(&mut self) -> bool {
        if self.session.is_none() && self.window.is_some() {
            return true;
        }
        match self.others.iter().find(|c| c.idle() && c.window.is_some()).map(|c| c.id) {
            Some(id) => self.activate(id),
            None => false,
        }
    }

    /// Create a new idle connection (hidden session window) and make it current.
    pub(super) fn new_conn(&mut self, el: &ActiveEventLoop) -> anyhow::Result<()> {
        self.next_conn += 1;
        let mut fresh = Conn::default();
        fresh.id = self.next_conn;
        self.swap_conn(&mut fresh);
        let had_previous = fresh.window.is_some();
        if had_previous {
            self.others.push(fresh);
        }
        let r = self.open_conn_window(el);
        if r.is_err() && had_previous {
            // Back to the previous connection; the broken one is dropped.
            let mut prev = self.others.pop().unwrap();
            self.swap_conn(&mut prev);
        }
        r
    }

    fn open_conn_window(&mut self, el: &ActiveEventLoop) -> anyhow::Result<()> {
        let attrs = Window::default_attributes()
            .with_title("NyaRemoteControl")
            .with_inner_size(LogicalSize::new(1280.0, 800.0))
            .with_min_inner_size(LogicalSize::new(640.0, 480.0))
            .with_visible(false);
        let window = Arc::new(el.create_window(attrs).map_err(|e| anyhow::anyhow!("无法创建窗口：{e}"))?);
        self.window = Some(window.clone());
        let dev = device_for_window(&window).map_err(|e| anyhow::anyhow!("无法创建 D3D11 设备：{e:#}"))?;
        self.create_renderer(dev).map_err(|e| anyhow::anyhow!("无法初始化渲染：{e:#}"))?;
        tracing::info!("session window {} on adapter luid {:#x}", self.conn_id, self.adapter_luid);
        if let Some(h) = hwnd(&window) {
            // The keyboard hook takes keys for any of our session windows.
            if self.hook_installed {
                input::set_extra_window(h, true);
            } else {
                input::install(h, self.ui_tx.clone());
                self.hook_installed = true;
            }
        }
        Ok(())
    }

    /// Keep exactly one idle connection ready (create one, drop surplus).
    pub(super) fn keep_one_idle(&mut self, el: &ActiveEventLoop) {
        let current_idle = self.session.is_none() && self.window.is_some();
        let mut idle = usize::from(current_idle);
        self.others.retain(|c| {
            if c.idle() {
                idle += 1;
                idle == 1
            } else {
                true
            }
        });
        if idle == 0 {
            if let Err(e) = self.new_conn(el) {
                tracing::error!("{e:#}");
            }
        }
    }
}
