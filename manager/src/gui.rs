//! Host management GUI (double-click nya-server.exe): a web page
//! (common/web/src/manager) in a WebView2 window. This side keeps the model —
//! service state, the control pipe (or the files while the service is
//! stopped), background jobs — and answers the page's calls.
//!
//! Page → Rust calls: snapshot, svc, reset_code, set_config, remove_client,
//! disconnect, diag, components, install, log, open_logs, open_url,
//! open_sound_settings, relaunch_elevated.
//! Rust → page events: `snapshot` (whenever something changed), `job`
//! (a background job started / finished), `install` (component install progress).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use nya_webui::{Call, WebUi};
use serde::Serialize;
use serde_json::{json, Value};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use nya_server_core::backend::Backend;
use nya_server_core::config::{ServerConfig, ENCODERS};
use nya_server_core::control_pb::{self as cpb, event::Kind};
use nya_server_core::SERVICE_NAME;
use nya_server_core::{components, install, paths, win as winutil};

fn service_exists(name: &str) -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(name, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

/// Is the (driver) service loaded? The host itself checks by connecting to it.
fn service_running(name: &str) -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(name, ServiceAccess::QUERY_STATUS))
        .and_then(|s| s.query_status())
        .is_ok_and(|s| s.current_state == ServiceState::Running)
}

/// An optional third-party component: installed only when the user asks.
#[derive(Serialize)]
struct Component {
    id: &'static str,
    /// What it is for, as users call it.
    name: &'static str,
    /// The driver / program's own name.
    product: &'static str,
    purpose: &'static str,
    status: Option<String>,
    installed: bool,
    url: &'static str,
    note: &'static str,
}

fn component_id(id: &str) -> Option<(components::Id, &'static str)> {
    Some(match id {
        "vdd" => (components::Id::Vdd, "虚拟显示器"),
        "cable" => (components::Id::Cable, "虚拟声卡"),
        "vigem" => (components::Id::Vigem, "手柄"),
        "usbip" => (components::Id::Usbip, "USB 透传"),
        _ => return None,
    })
}

/// Detect optional components (COM is initialised on the calling thread).
fn detect_components() -> Vec<Component> {
    nya_win::com_init();
    let cable = components::cable_device_name();
    let vdd_installed = nya_win::devnode::exists(components::VDD_HWID);
    let vdd = vdd_installed.then(|| {
        if nya_win::devnode::is_started(components::VDD_HWID) {
            "正在使用（有客户端选择了虚拟显示器 / 隐私屏）".to_owned()
        } else {
            "平时停用，有客户端需要时自动启用".to_owned()
        }
    });
    let usbip = components::usbip_exe().map(|p| p.display().to_string());
    vec![
        Component {
            id: "vdd",
            name: "虚拟显示器",
            product: "Virtual Display Driver 25.7.23",
            purpose: "在被控端新建显示器：分辨率跟随客户端窗口、多屏、隐私屏（本机显示器黑屏、本机键鼠屏蔽）。需要服务模式",
            installed: vdd_installed,
            status: vdd,
            url: "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases",
            note: "免费开源；平时保持停用，不影响本机显示器",
        },
        Component {
            id: "cable",
            name: "虚拟声卡",
            product: "VB-Cable",
            purpose: "把客户端麦克风送进被控端：客户端工具条打开“麦克风”，被控端软件选择“CABLE Output”作为麦克风",
            installed: cable.is_some(),
            status: cable.map(|n| {
                if nya_win::audio::default_render_is("CABLE") {
                    format!("{n}（注意：它现在是默认播放设备，本机会听不到声音，请在声音设置里把默认播放设备改回扬声器）")
                } else {
                    n
                }
            }),
            url: "https://vb-audio.com/Cable/",
            note: "捐赠软件（安装即表示同意 VB-Audio 许可），需联网从官网下载；安装后需要重启一次",
        },
        Component {
            id: "vigem",
            name: "手柄",
            product: "ViGEmBus 1.22.0",
            purpose: "客户端的手柄在被控端显示为 Xbox 手柄",
            installed: service_exists("ViGEmBus"),
            status: service_running("ViGEmBus").then(|| "驱动已加载".into()),
            url: "https://github.com/nefarius/ViGEmBus/releases",
            note: "免费；作者已停止维护，但仍可用。客户端插上 Xbox / XInput 手柄即自动使用",
        },
        Component {
            id: "usbip",
            name: "USB 透传",
            product: "usbip-win2 0.9.8.1",
            purpose: "U 盾、加密狗等 USB 设备从客户端透传到被控端",
            installed: usbip.is_some(),
            status: usbip,
            url: "https://github.com/vadimgrn/usbip-win2/releases",
            note: "客户端另需 usbipd-win（客户端工具条“USB 设备”里可一键安装）",
        },
    ]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SvcState {
    NotInstalled,
    Stopped,
    Running,
    Pending,
    Unknown,
}

impl SvcState {
    fn key(self) -> &'static str {
        match self {
            SvcState::NotInstalled => "not_installed",
            SvcState::Stopped => "stopped",
            SvcState::Running => "running",
            SvcState::Pending => "pending",
            SvcState::Unknown => "unknown",
        }
    }
}

fn service_state() -> SvcState {
    let Ok(m) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) else { return SvcState::Unknown };
    match m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Err(_) => SvcState::NotInstalled,
        Ok(s) => match s.query_status().map(|x| x.current_state) {
            Ok(ServiceState::Running) => SvcState::Running,
            Ok(ServiceState::Stopped) => SvcState::Stopped,
            Ok(_) => SvcState::Pending,
            Err(_) => SvcState::Unknown,
        },
    }
}

fn control_service(start: bool, stop: bool) -> Result<String> {
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let s = m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP)?;
    if stop {
        install::stop_and_wait(&s);
    }
    if start {
        s.start(&[] as &[&std::ffi::OsStr])?;
    }
    Ok(match (start, stop) {
        (true, true) => "服务已重启".into(),
        (true, false) => "服务已启动".into(),
        _ => "服务已停止".into(),
    })
}

/// Diagnostics run in `nya-server-svc.exe`, which has the capture / encoding
/// stack; the report is saved to `out`.
fn run_diag(out: &Path) -> Result<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = paths::service_exe()?;
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d)?;
    }
    let r = std::process::Command::new(&exe)
        .arg("diag")
        .arg("--out")
        .arg(out)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| anyhow!("无法运行 {}：{e}", exe.display()))?;
    if !r.status.success() {
        return Err(anyhow!("诊断失败（{}）：{}", r.status, String::from_utf8_lossy(&r.stderr)));
    }
    Ok(std::fs::read_to_string(out)?)
}

fn relaunch_elevated() -> Result<()> {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let exe = std::env::current_exe()?;
    let r = unsafe { ShellExecuteW(None, w!("runas"), &HSTRING::from(exe.as_os_str()), None, None, SW_SHOWNORMAL) };
    if r.0 as isize <= 32 {
        return Err(anyhow!("未获得管理员权限"));
    }
    Ok(())
}

fn open(target: impl AsRef<std::ffi::OsStr>) {
    let _ = std::process::Command::new("explorer").arg(target).spawn();
}

fn event_kind(k: i32) -> &'static str {
    match Kind::try_from(k).unwrap_or(Kind::Other) {
        Kind::Connected => "connected",
        Kind::Disconnected => "disconnected",
        Kind::Paired => "paired",
        Kind::PairingFailed => "pairing_failed",
        Kind::Rejected => "rejected",
        Kind::Service => "service",
        Kind::Other => "other",
    }
}

fn status_json(s: &cpb::Status) -> Value {
    json!({
        "server_version": s.server_version,
        "listen": s.listen,
        "listen_error": s.listen_error,
        "host": s.host.as_ref().map(|h| json!({ "running": h.running, "console_session": h.console_session, "stream": h.stream })),
        "session": s.session.as_ref().map(|c| json!({
            "client_name": c.client_name, "client_version": c.client_version,
            "remote_addr": c.remote_addr, "since_unix": c.since_unix,
        })),
        "recent": s.recent.iter().rev().take(30).map(|e| json!({ "unix": e.unix, "kind": event_kind(e.kind), "text": e.text })).collect::<Vec<_>>(),
    })
}

/// Events delivered to the winit loop.
enum UserEvent {
    Call(Call),
    /// (call id or 0, label, result) of a background job.
    JobDone(u64, &'static str, Result<String, String>),
    Components(u64, Vec<Component>),
    InstallProgress,
}

struct Model {
    elevated: bool,
    dir: PathBuf,
    svc: SvcState,
    svc_checked: Instant,
    /// The running service (control pipe) or, while it is stopped, its files.
    /// `None` until (re)connected.
    backend: Option<Backend>,
    status: Option<cpb::Status>,
    /// Does the installed service run this directory's nya-server-svc.exe?
    points_here: Option<bool>,
    code: String,
    fingerprint: String,
    cfg: ServerConfig,
    clients: Vec<nya_server_core::auth::PairedClient>,
    load_error: Option<String>,
    busy: Option<&'static str>,
    install_job: Option<Arc<Mutex<InstallJob>>>,
    /// Last snapshot sent to the page (to push only changes).
    sent: String,
}

impl Model {
    fn new() -> Self {
        let mut m = Self {
            elevated: winutil::is_elevated(),
            dir: paths::service_dir(),
            svc: SvcState::Unknown,
            svc_checked: Instant::now() - Duration::from_secs(10),
            backend: None,
            status: None,
            points_here: None,
            code: String::new(),
            fingerprint: String::new(),
            cfg: ServerConfig::default(),
            clients: Vec::new(),
            load_error: None,
            busy: None,
            install_job: None,
            sent: String::new(),
        };
        m.svc = service_state();
        m.reload();
        m
    }

    /// Run `f` on the backend, connecting first if needed. A failure drops
    /// the connection so the next call reconnects (the service may have restarted).
    fn with_backend<T>(&mut self, f: impl FnOnce(&mut Backend) -> Result<T>) -> Result<T> {
        if self.backend.is_none() {
            self.backend = Some(Backend::service("nya-server gui")?);
        }
        let r = f(self.backend.as_mut().unwrap());
        if r.is_err() {
            self.backend = None;
        }
        r
    }

    fn live(&self) -> bool {
        self.backend.as_ref().is_some_and(|b| b.is_live())
    }

    /// Re-read pairing data, settings and clients from the service (or its
    /// files while it is stopped).
    fn reload(&mut self) {
        if !self.elevated {
            return;
        }
        self.backend = None;
        self.points_here = install::service_points_here();
        self.load_error = None;
        match self.with_backend(|b| b.pairing()) {
            Ok(p) => {
                self.code = p.code;
                self.fingerprint = p.fingerprint;
            }
            Err(e) => self.load_error = Some(format!("读取配对码失败：{e:#}")),
        }
        if let Ok(c) = self.with_backend(|b| b.config()) {
            self.cfg = c;
        }
        self.clients = self.with_backend(|b| b.clients()).unwrap_or_default();
        self.refresh_status();
    }

    fn refresh_status(&mut self) {
        self.status = self.with_backend(|b| b.status()).ok().flatten();
    }

    /// Once a second: follow the service (started / stopped / reinstalled).
    fn tick(&mut self) {
        if self.svc_checked.elapsed() < Duration::from_secs(1) {
            return;
        }
        let before = self.svc;
        self.svc = service_state();
        self.svc_checked = Instant::now();
        if self.elevated && self.busy.is_none() {
            // Started / stopped: switch between the pipe and the files.
            if before != self.svc || (self.svc == SvcState::Running && !self.live()) {
                self.reload();
            } else {
                self.refresh_status();
            }
        }
    }

    fn snapshot(&self) -> Value {
        json!({
            "elevated": self.elevated,
            "computer": winutil::computer_name(),
            "version": env!("CARGO_PKG_VERSION"),
            "svc": self.svc.key(),
            "live": self.live(),
            "points_here": self.points_here,
            "status": self.status.as_ref().map(status_json),
            "code": self.code,
            "fingerprint": self.fingerprint,
            "config": self.cfg,
            "encoders": ENCODERS,
            "clients": self.clients,
            "load_error": self.load_error,
            "busy": self.busy,
            "log_dir": self.dir.join("logs"),
        })
    }

    fn install_json(&self) -> Value {
        match &self.install_job {
            None => Value::Null,
            Some(j) => {
                let j = j.lock().unwrap();
                json!({
                    "current": j.current,
                    "status": j.status,
                    "log": j.log.iter().map(|(err, t)| json!({ "error": err, "text": t })).collect::<Vec<_>>(),
                    "reboot": j.reboot,
                    "done": j.done,
                })
            }
        }
    }
}

struct App {
    model: Model,
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    web: Option<WebUi>,
    next_tick: Instant,
}

impl App {
    fn reply(&self, id: u64, r: Result<Value, String>) {
        if id != 0 {
            if let Some(w) = &self.web {
                w.reply(id, r);
            }
        }
    }

    fn push(&mut self) {
        let snap = self.model.snapshot();
        let text = snap.to_string();
        if text != self.model.sent {
            if let Some(w) = &self.web {
                w.emit("snapshot", &snap);
            }
            self.model.sent = text;
        }
    }

    /// Start a background job; its result answers call `id` and refreshes.
    fn job(&mut self, id: u64, label: &'static str, f: impl FnOnce() -> Result<String> + Send + 'static) {
        if let Some(b) = self.model.busy {
            return self.reply(id, Err(format!("正在{b}，请稍候")));
        }
        self.model.busy = Some(label);
        self.push();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let r = f().map_err(|e| format!("{e:#}"));
            let _ = proxy.send_event(UserEvent::JobDone(id, label, r));
        });
    }

    fn call(&mut self, c: Call) {
        let id = c.id;
        let m = &mut self.model;
        let str_arg = |k: &str| c.args.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let r: Result<Value, String> = match c.cmd.as_str() {
            "snapshot" => Ok(m.snapshot()),
            "relaunch_elevated" => match relaunch_elevated() {
                Ok(()) => std::process::exit(0),
                Err(e) => Err(format!("{e:#}")),
            },
            _ if !m.elevated && !matches!(c.cmd.as_str(), "open_url" | "open_logs") => Err("需要管理员权限".into()),
            "svc" => {
                let action = str_arg("action");
                let (label, f): (&'static str, Box<dyn FnOnce() -> Result<String> + Send>) = match action.as_str() {
                    "install" => ("安装服务", Box::new(|| install::install(None))),
                    "uninstall" => ("卸载服务", Box::new(|| install::uninstall(false))),
                    "start" => ("启动服务", Box::new(|| control_service(true, false))),
                    "stop" => ("停止服务", Box::new(|| control_service(false, true))),
                    "restart" => ("重启服务", Box::new(|| control_service(true, true))),
                    other => return self.reply(id, Err(format!("未知操作 {other}"))),
                };
                return self.job(id, label, f);
            }
            "diag" => {
                let out = m.dir.join("logs").join("nya-diag.txt");
                return self.job(id, "诊断", move || run_diag(&out));
            }
            "reset_code" => m.with_backend(|b| b.reset_pairing_code()).map_err(|e| format!("{e:#}")).map(|p| {
                m.code = p.code;
                let when = if m.live() { "已生效" } else { "服务启动后生效" };
                Value::String(format!("已生成新配对码，{when}"))
            }),
            "set_config" => (|| {
                let cfg: ServerConfig = serde_json::from_value(c.args.get("config").cloned().unwrap_or_default())
                    .map_err(|e| format!("设置格式不对：{e}"))?;
                cfg.validate().map_err(|e| format!("{e:#}"))?;
                let msg = m.with_backend(|b| b.set_config(&cfg)).map_err(|e| format!("保存失败：{e:#}"))?;
                m.cfg = cfg;
                Ok(Value::String(msg))
            })(),
            "remove_client" => {
                let fp = str_arg("fingerprint");
                let r = m.with_backend(|b| b.remove_client(&fp)).map_err(|e| format!("{e:#}"));
                m.clients = m.with_backend(|b| b.clients()).unwrap_or_default();
                r.map(Value::String)
            }
            "disconnect" => {
                let r = m.with_backend(|b| b.disconnect("被控端管理员断开了连接")).map_err(|e| format!("{e:#}"));
                m.refresh_status();
                r.map(Value::String)
            }
            "log" => {
                let name = str_arg("name");
                let name = ["service", "helper", "gui", "standalone", "vdd-test"].into_iter().find(|n| *n == name).unwrap_or("service");
                m.with_backend(|b| b.tail_log(name, 400)).map(Value::String).map_err(|e| format!("{e:#}"))
            }
            "components" => {
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let _ = proxy.send_event(UserEvent::Components(id, detect_components()));
                });
                return;
            }
            "install" => {
                if m.install_job.as_ref().is_some_and(|j| !j.lock().unwrap().done) {
                    Err("正在安装，请稍候".into())
                } else {
                    let ids: Vec<(components::Id, &'static str)> = c
                        .args
                        .get("ids")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(|v| v.as_str().and_then(component_id)).collect())
                        .unwrap_or_default();
                    if ids.is_empty() {
                        Err("没有要安装的组件".into())
                    } else {
                        m.install_job = Some(start_install(ids, self.proxy.clone()));
                        Ok(m.install_json())
                    }
                }
            }
            "open_logs" => {
                open(m.dir.join("logs"));
                Ok(Value::Null)
            }
            "open_url" => {
                let url = str_arg("url");
                if url.starts_with("https://") || url.starts_with("ms-settings:") {
                    open(url);
                }
                Ok(Value::Null)
            }
            "open_sound_settings" => {
                open("ms-settings:sound");
                Ok(Value::Null)
            }
            other => Err(format!("未知命令 {other}")),
        };
        self.reply(id, r);
        self.push();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("NyaRemoteControl 被控端")
            .with_inner_size(LogicalSize::new(1000.0, 700.0))
            .with_min_inner_size(LogicalSize::new(640.0, 480.0));
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                nya_server_core::fatal(&format!("无法创建窗口：{e}"));
                el.exit();
                return;
            }
        };
        let dark = matches!(window.theme(), Some(winit::window::Theme::Dark));
        let opts = nya_webui::Options {
            page: "manager.html",
            data_dir: std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join("NyaRemoteControl")
                .join("manager-webview"),
            background: if dark { (0x17, 0x18, 0x1c) } else { (0xf7, 0xf8, 0xfa) },
        };
        let proxy = self.proxy.clone();
        match WebUi::new(&*window, window.inner_size(), opts, move |c| {
            let _ = proxy.send_event(UserEvent::Call(c));
        }) {
            Ok(w) => self.web = Some(w),
            Err(e) => {
                nya_server_core::fatal(&format!("{e:#}"));
                el.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                if let Some(w) = &self.web {
                    w.resize(size);
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Call(c) => self.call(c),
            UserEvent::JobDone(id, label, r) => {
                self.model.busy = None;
                self.model.svc_checked = Instant::now() - Duration::from_secs(10);
                if label != "诊断" {
                    // The service was (un)installed / started / stopped.
                    self.model.svc = service_state();
                    self.model.reload();
                }
                if let Some(w) = &self.web {
                    w.emit("job", &json!({ "label": label, "ok": r.is_ok() }));
                }
                self.reply(id, r.map(Value::String));
                self.push();
            }
            UserEvent::Components(id, list) => self.reply(id, Ok(serde_json::to_value(list).unwrap_or_default())),
            UserEvent::InstallProgress => {
                if let Some(w) = &self.web {
                    w.emit("install", &self.model.install_json());
                }
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if Instant::now() >= self.next_tick {
            self.next_tick = Instant::now() + Duration::from_secs(1);
            self.model.tick();
            self.push();
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_tick));
    }
}

pub fn run() -> Result<()> {
    // Managing the service needs admin rights: ask for them up front.
    if !winutil::is_elevated() && relaunch_elevated().is_ok() {
        return Ok(());
    }
    let _log = nya_server_core::logging::init(&paths::service_dir(), "gui", false);
    let el = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = el.create_proxy();
    let mut app = App { model: Model::new(), proxy, window: None, web: None, next_tick: Instant::now() };
    el.run_app(&mut app)?;
    Ok(())
}

#[derive(Default)]
struct InstallJob {
    current: Option<&'static str>,
    status: String,
    /// (is error, message)
    log: Vec<(bool, String)>,
    reboot: bool,
    done: bool,
}

/// Install components one after another on a worker thread.
fn start_install(list: Vec<(components::Id, &'static str)>, proxy: EventLoopProxy<UserEvent>) -> Arc<Mutex<InstallJob>> {
    let job = Arc::new(Mutex::new(InstallJob::default()));
    let j = job.clone();
    std::thread::spawn(move || {
        let notify = || {
            let _ = proxy.send_event(UserEvent::InstallProgress);
        };
        for (id, name) in list {
            {
                let mut g = j.lock().unwrap();
                g.current = Some(name);
                g.status.clear();
            }
            notify();
            let r = components::install(id, &mut |s| {
                j.lock().unwrap().status = s;
                notify();
            });
            let mut g = j.lock().unwrap();
            match r {
                Ok(i) => {
                    g.reboot |= i.reboot;
                    let mut line = format!("{name} 安装完成");
                    if i.reboot {
                        line.push_str("（需要重启）");
                    }
                    if !i.note.is_empty() {
                        line.push('。');
                        line.push_str(&i.note);
                    }
                    g.log.push((false, line));
                }
                Err(e) => g.log.push((true, format!("{name} 安装失败：{e:#}"))),
            }
            drop(g);
            notify();
        }
        let mut g = j.lock().unwrap();
        g.current = None;
        g.done = true;
        drop(g);
        notify();
    });
    job
}
