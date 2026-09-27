//! Host management GUI (double-click nya-server.exe): service control,
//! pairing code, paired clients, settings, diagnostics and logs.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use nya_transport::Identity;
use nya_ui::egui::{self, Color32, RichText};
use nya_ui::{Gui, Surface};
use nya_win::d3d::D3dDevice;
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

use crate::auth::{load_or_create_key, AuthStore, PairedClient};
use crate::config::ServerConfig;
use crate::service::SERVICE_NAME;
use crate::{components, install, paths, winutil};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Settings,
    Clients,
    Diagnostics,
    Components,
    Logs,
}

/// An optional third-party component: installed only when the user asks.
struct Component {
    id: components::Id,
    name: &'static str,
    purpose: &'static str,
    status: Option<String>,
    installed: bool,
    ready: bool,
    url: &'static str,
    note: &'static str,
}

fn service_exists(name: &str) -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(name, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

/// Detect optional components (COM is initialised on the calling thread).
fn detect_components() -> Vec<Component> {
    nya_win::com_init();
    let cable = crate::host::mic_cable_name();
    let vdd_active = nya_win::topology::Topology::enumerate()
        .ok()
        .and_then(|t| t.adapters.iter().find(|a| a.name.to_lowercase().contains("virtual display")).map(|a| a.name.clone()));
    let vdd_installed = vdd_active.is_some() || nya_win::devnode::exists(components::VDD_HWID);
    let vdd = vdd_active.or_else(|| vdd_installed.then(|| "已安装（未启用）".to_owned()));
    let usbip = crate::usb::usbip_exe().map(|p| p.display().to_string());
    vec![
        Component {
            id: components::Id::Cable,
            name: "VB-Cable 虚拟声卡",
            purpose: "接收客户端麦克风：客户端工具条打开“麦克风”，被控端软件选择“CABLE Output”作为麦克风",
            installed: cable.is_some(),
            status: cable.map(|n| {
                if crate::host::mic_cable_name().is_some() && nya_win::audio::default_render_is("CABLE") {
                    format!("{n}（注意：它现在是默认播放设备，本机会听不到声音，建议在声音设置里把默认播放设备改回扬声器）")
                } else {
                    n
                }
            }),
            ready: true,
            url: "https://vb-audio.com/Cable/",
            note: "捐赠软件（安装即表示同意 VB-Audio 许可），需联网从官网下载；安装后需要重启一次",
        },
        Component {
            id: components::Id::Usbip,
            name: "usbip-win2",
            purpose: "USB 设备透传（U 盾、加密狗等）：被控端虚拟 USB 控制器",
            installed: usbip.is_some(),
            status: usbip,
            ready: false,
            url: "https://github.com/vadimgrn/usbip-win2/releases",
            note: "开发中；客户端另需 usbipd-win",
        },
        Component {
            id: components::Id::Vigem,
            name: "ViGEmBus",
            purpose: "手柄：把客户端的手柄模拟成被控端的 Xbox 手柄",
            installed: service_exists("ViGEmBus"),
            status: if crate::host::gamepad_available() { Some("驱动可用".into()) } else { None },
            ready: true,
            url: "https://github.com/nefarius/ViGEmBus/releases",
            note: "免费；作者已停止维护，但仍可用。客户端插上 Xbox/XInput 手柄即自动使用",
        },
        Component {
            id: components::Id::Vdd,
            name: "Virtual Display Driver",
            purpose: "虚拟显示器：不接显示器也能用，分辨率 / 刷新率可自定义",
            installed: vdd_installed,
            status: vdd,
            ready: false,
            url: "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases",
            note: "开发中",
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

/// Work done off the UI thread.
enum Job {
    Install,
    Uninstall,
    Start,
    Stop,
    Restart,
    Diag,
}

struct Model {
    elevated: bool,
    dir: PathBuf,
    tab: Tab,
    svc: SvcState,
    svc_checked: Instant,
    code: String,
    fingerprint: String,
    cfg: ServerConfig,
    cfg_dirty: bool,
    clients: Vec<PairedClient>,
    message: Option<(bool, String)>,
    busy: Option<&'static str>,
    job_rx: Option<mpsc::Receiver<Result<String, String>>>,
    diag_text: String,
    log_text: String,
    log_name: &'static str,
    components: Option<Vec<Component>>,
    components_rx: Option<mpsc::Receiver<Vec<Component>>>,
    install_job: Option<Arc<Mutex<InstallJob>>>,
    confirm_reset: bool,
    confirm_uninstall: bool,
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

/// Last `n` lines of the newest `<prefix>.*.log`.
fn tail_log(dir: &Path, prefix: &str, n: usize) -> String {
    let newest = std::fs::read_dir(dir.join("logs")).ok().and_then(|rd| {
        rd.flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("{prefix}.")))
            .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
    });
    let Some(entry) = newest else { return format!("没有 {prefix} 日志") };
    let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

impl Model {
    fn new() -> Self {
        let elevated = winutil::is_elevated();
        let dir = paths::service_dir();
        let mut m = Self {
            elevated,
            dir,
            tab: Tab::Overview,
            svc: SvcState::Unknown,
            svc_checked: Instant::now() - Duration::from_secs(10),
            code: String::new(),
            fingerprint: String::new(),
            cfg: ServerConfig::default(),
            cfg_dirty: false,
            clients: Vec::new(),
            message: None,
            busy: None,
            job_rx: None,
            diag_text: String::new(),
            log_text: String::new(),
            log_name: "service",
            components: None,
            components_rx: None,
            install_job: None,
            confirm_reset: false,
            confirm_uninstall: false,
        };
        m.reload();
        m
    }

    /// Re-read pairing data, config and clients from the service directory.
    fn reload(&mut self) {
        if !self.elevated {
            return;
        }
        let _ = std::fs::create_dir_all(&self.dir);
        match load_or_create_key(&self.dir, false) {
            Ok(k) => self.code = k.to_code(),
            Err(e) => self.message = Some((true, format!("读取配对码失败：{e:#}"))),
        }
        if let Ok(id) = Identity::load_or_create(&self.dir) {
            self.fingerprint = id.fingerprint().to_string();
        }
        if let Ok(c) = ServerConfig::load_or_create(&self.dir) {
            self.cfg = c;
            self.cfg_dirty = false;
        }
        self.clients = AuthStore::list(&self.dir);
        self.log_text = tail_log(&self.dir, self.log_name, 200);
    }

    fn run_job(&mut self, job: Job, label: &'static str) {
        let (tx, rx) = mpsc::channel();
        self.busy = Some(label);
        self.job_rx = Some(rx);
        std::thread::spawn(move || {
            let r = match job {
                Job::Install => install::install(None),
                Job::Uninstall => install::uninstall(false),
                Job::Start => control_service(true, false),
                Job::Stop => control_service(false, true),
                Job::Restart => control_service(true, true),
                Job::Diag => Ok(crate::diag::collect()),
            };
            let _ = tx.send(r.map_err(|e| format!("{e:#}")));
        });
    }

    fn poll_job(&mut self) {
        let Some(rx) = &self.job_rx else { return };
        if let Ok(r) = rx.try_recv() {
            let was_diag = self.busy == Some("诊断");
            self.job_rx = None;
            self.busy = None;
            match r {
                Ok(text) if was_diag => self.diag_text = text,
                Ok(text) => self.message = Some((false, text)),
                Err(e) => self.message = Some((true, e)),
            }
            self.svc_checked = Instant::now() - Duration::from_secs(10);
            self.reload();
        }
    }

    fn ui(&mut self, ctx: &egui::Context) {
        self.poll_job();
        if self.svc_checked.elapsed() > Duration::from_secs(2) {
            self.svc = service_state();
            self.svc_checked = Instant::now();
        }

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("NyaRemoteControl 被控端");
                ui.add_space(16.0);
                for (t, name) in [
                    (Tab::Overview, "概览"),
                    (Tab::Settings, "设置"),
                    (Tab::Clients, "已配对客户端"),
                    (Tab::Diagnostics, "诊断"),
                    (Tab::Components, "可选组件"),
                    (Tab::Logs, "日志"),
                ] {
                    ui.selectable_value(&mut self.tab, t, name);
                }
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(20.0))
            .show(ctx, |ui| {
                if !self.elevated {
                    ui.label(RichText::new("需要管理员权限才能管理服务、查看配对码。").color(Color32::from_rgb(255, 180, 90)));
                    if ui.button("以管理员身份重新打开").clicked() {
                        match relaunch_elevated() {
                            Ok(()) => std::process::exit(0),
                            Err(e) => self.message = Some((true, format!("{e:#}"))),
                        }
                    }
                    ui.add_space(12.0);
                }
                if let Some((err, text)) = &self.message {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let color = if *err { Color32::from_rgb(255, 120, 110) } else { Color32::LIGHT_GREEN };
                            ui.label(RichText::new(text).color(color));
                        });
                    });
                    if ui.small_button("关闭提示").clicked() {
                        self.message = None;
                    }
                    ui.add_space(8.0);
                }
                if let Some(b) = self.busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("{b}…"));
                    });
                    ui.add_space(8.0);
                }
                ui.add_enabled_ui(self.elevated && self.busy.is_none(), |ui| match self.tab {
                    Tab::Overview => self.overview(ui),
                    Tab::Settings => self.settings(ui),
                    Tab::Clients => self.clients_tab(ui),
                    Tab::Diagnostics => self.diagnostics(ui),
                    Tab::Components => self.components(ui),
                    Tab::Logs => self.logs(ui),
                });
            });

        if self.busy.is_some() || self.install_job.as_ref().is_some_and(|j| !j.lock().unwrap().done) {
            ctx.request_repaint_after(Duration::from_millis(200));
        } else {
            ctx.request_repaint_after(Duration::from_secs(2));
        }
    }

    fn overview(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("服务").strong());
        ui.horizontal(|ui| {
            let (text, color) = match self.svc {
                SvcState::Running => ("运行中", Color32::LIGHT_GREEN),
                SvcState::Stopped => ("已停止", Color32::from_rgb(255, 180, 90)),
                SvcState::Pending => ("正在切换…", Color32::LIGHT_BLUE),
                SvcState::NotInstalled => ("未安装", Color32::GRAY),
                SvcState::Unknown => ("未知", Color32::GRAY),
            };
            ui.label(RichText::new(text).color(color).strong());
            ui.add_space(12.0);
            match self.svc {
                SvcState::NotInstalled => {
                    if ui.button(RichText::new("安装服务").strong()).clicked() {
                        self.run_job(Job::Install, "正在安装服务");
                    }
                }
                SvcState::Stopped => {
                    if ui.button("启动").clicked() {
                        self.run_job(Job::Start, "正在启动服务");
                    }
                    if ui.button("卸载").clicked() {
                        self.confirm_uninstall = true;
                    }
                }
                SvcState::Running => {
                    if ui.button("重启").clicked() {
                        self.run_job(Job::Restart, "正在重启服务");
                    }
                    if ui.button("停止").clicked() {
                        self.run_job(Job::Stop, "正在停止服务");
                    }
                    if ui.button("卸载").clicked() {
                        self.confirm_uninstall = true;
                    }
                }
                _ => {}
            }
        });
        if self.confirm_uninstall {
            ui.horizontal(|ui| {
                ui.label("确定卸载服务？（证书和配对信息会保留）");
                if ui.button("卸载").clicked() {
                    self.confirm_uninstall = false;
                    self.run_job(Job::Uninstall, "正在卸载服务");
                }
                if ui.button("取消").clicked() {
                    self.confirm_uninstall = false;
                }
            });
        }
        ui.label(
            RichText::new(format!("端口 UDP {}  ·  程序 {}", self.cfg.port, std::env::current_exe().unwrap_or_default().display()))
                .weak(),
        );

        ui.add_space(16.0);
        ui.label(RichText::new("配对码").strong());
        ui.label(RichText::new("客户端第一次连接时输入。已配对的客户端之后不再需要。").weak());
        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.code).monospace().size(22.0).strong());
            if ui.button("复制").clicked() {
                ui.ctx().copy_text(self.code.clone());
            }
            if ui.button("重新生成").clicked() {
                self.confirm_reset = true;
            }
        });
        if self.confirm_reset {
            ui.horizontal(|ui| {
                ui.label("旧配对码将失效（已配对的客户端不受影响）。继续？");
                if ui.button("重新生成").clicked() {
                    self.confirm_reset = false;
                    match load_or_create_key(&self.dir, true) {
                        Ok(k) => {
                            self.code = k.to_code();
                            self.message = Some((false, "已生成新配对码；重启服务后生效".into()));
                        }
                        Err(e) => self.message = Some((true, format!("{e:#}"))),
                    }
                }
                if ui.button("取消").clicked() {
                    self.confirm_reset = false;
                }
            });
        }

        ui.add_space(16.0);
        ui.label(RichText::new("证书指纹").strong());
        ui.label(RichText::new(&self.fingerprint).monospace());
        ui.label(RichText::new("客户端提示“证书已变化”时，用这里核对。").weak());

        ui.add_space(16.0);
        ui.label(RichText::new("最近的连接").strong());
        let recent: Vec<String> = tail_log(&self.dir, "service", 400)
            .lines()
            .filter(|l| l.contains("client ") || l.contains("paired") || l.contains("session ended") || l.contains("bye"))
            .map(|l| l.chars().take(160).collect())
            .collect();
        egui::ScrollArea::vertical().max_height(160.0).id_salt("recent").show(ui, |ui| {
            if recent.is_empty() {
                ui.label(RichText::new("暂无").weak());
            }
            for l in recent.iter().rev().take(12) {
                ui.label(RichText::new(l).monospace().small());
            }
        });
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        let before = format!("{:?}", self.cfg);
        let c = &mut self.cfg;
        egui::Grid::new("server-settings").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            ui.label("端口（UDP）");
            ui.add(egui::DragValue::new(&mut c.port).range(1024..=65535));
            ui.end_row();

            ui.label("监听地址");
            ui.vertical(|ui| {
                ui.add(egui::TextEdit::singleline(&mut c.bind).desired_width(220.0));
                ui.label(RichText::new(":: 表示所有网卡；填组网 IP（如 100.x.y.z）则只接受该网卡").weak().small());
            });
            ui.end_row();

            ui.label("显示名称");
            ui.add(egui::TextEdit::singleline(&mut c.name).hint_text("留空 = 计算机名").desired_width(220.0));
            ui.end_row();

            ui.label("编码器");
            egui::ComboBox::from_id_salt("enc").selected_text(c.encoder.clone()).show_ui(ui, |ui| {
                for e in ["auto", "nvenc", "qsv", "amf", "software"] {
                    ui.selectable_value(&mut c.encoder, e.to_string(), e);
                }
            });
            ui.end_row();

            ui.label("办公模式码率");
            kbps(ui, &mut c.office_bitrate_kbps);
            ui.end_row();

            ui.label("游戏模式码率");
            kbps(ui, &mut c.game_bitrate_kbps);
            ui.end_row();

            ui.label("最高帧率");
            ui.add(egui::Slider::new(&mut c.max_fps, 30..=240).suffix(" fps"));
            ui.end_row();

            ui.label("声音");
            ui.checkbox(&mut c.audio, "传输系统声音");
            ui.end_row();

            ui.label("日志级别");
            egui::ComboBox::from_id_salt("log").selected_text(c.log_level.clone()).show_ui(ui, |ui| {
                for l in ["error", "warn", "info", "debug"] {
                    ui.selectable_value(&mut c.log_level, l.to_string(), l);
                }
            });
            ui.end_row();
        });
        if before != format!("{:?}", self.cfg) {
            self.cfg_dirty = true;
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(self.cfg_dirty, egui::Button::new("保存")).clicked() {
                match toml::to_string_pretty(&self.cfg).map_err(|e| anyhow!(e)).and_then(|t| {
                    std::fs::write(self.dir.join("server.toml"), t)?;
                    Ok(())
                }) {
                    Ok(()) => {
                        self.cfg_dirty = false;
                        self.message = Some((false, "已保存。重启服务后生效（端口变更需要重新安装以更新防火墙规则）".into()));
                    }
                    Err(e) => self.message = Some((true, format!("保存失败：{e:#}"))),
                }
            }
            if self.svc == SvcState::Running && ui.button("重启服务").clicked() {
                self.run_job(Job::Restart, "正在重启服务");
            }
            if ui.button("放弃修改").clicked() {
                self.reload();
            }
        });
    }

    fn clients_tab(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("已配对的客户端可以免配对码直接连接。移除后需要重新配对。").weak());
        ui.add_space(8.0);
        if self.clients.is_empty() {
            ui.label(RichText::new("没有已配对的客户端").weak());
        }
        let mut remove = None;
        egui::Grid::new("clients").num_columns(4).striped(true).spacing([20.0, 8.0]).show(ui, |ui| {
            for (i, c) in self.clients.iter().enumerate() {
                ui.label(RichText::new(&c.name).strong());
                ui.label(RichText::new(&c.fingerprint[..16.min(c.fingerprint.len())]).monospace());
                ui.label(RichText::new(&c.paired_at).weak());
                if ui.button("移除").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            let mut list = self.clients.clone();
            let c = list.remove(i);
            match AuthStore::save_list(&self.dir, list) {
                Ok(()) => self.message = Some((false, format!("已移除 {}", c.name))),
                Err(e) => self.message = Some((true, format!("{e:#}"))),
            }
            self.clients = AuthStore::list(&self.dir);
        }
    }

    fn diagnostics(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("运行诊断").clicked() {
                self.run_job(Job::Diag, "诊断");
            }
            if !self.diag_text.is_empty() {
                if ui.button("复制结果").clicked() {
                    ui.ctx().copy_text(self.diag_text.clone());
                }
                if ui.button("保存到日志目录").clicked() {
                    let p = self.dir.join("logs").join("nya-diag.txt");
                    match std::fs::write(&p, &self.diag_text) {
                        Ok(()) => self.message = Some((false, format!("已保存 {}", p.display()))),
                        Err(e) => self.message = Some((true, format!("{e}"))),
                    }
                }
            }
        });
        ui.label(RichText::new("检测显卡、显示器、编码器、截屏、跨显卡传输和音频，大约需要 10 秒。").weak());
        egui::ScrollArea::both().id_salt("diag").show(ui, |ui| {
            ui.label(RichText::new(&self.diag_text).monospace().small());
        });
    }

    fn components(&mut self, ui: &mut egui::Ui) {
        if let Some(rx) = &self.components_rx {
            if let Ok(c) = rx.try_recv() {
                self.components = Some(c);
                self.components_rx = None;
            }
        }
        if self.components.is_none() && self.components_rx.is_none() {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(detect_components());
            });
            self.components_rx = Some(rx);
        }
        ui.label(RichText::new("以下组件都是可选的，不装不影响其他功能，装不装由你决定。“一键安装”会自动下载固定版本并校验 SHA-256 后静默安装（程序目录下 drivers 文件夹里有离线安装包时优先使用）。").weak());
        let running = self.install_job.as_ref().is_some_and(|j| !j.lock().unwrap().done);
        if let Some(job) = &self.install_job {
            let mut j = job.lock().unwrap();
            if j.done && !j.redetected {
                j.redetected = true;
                self.components = None;
            }
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(!running, egui::Button::new("重新检测")).clicked() {
                self.components = None;
            }
            let missing: Vec<(components::Id, &'static str)> = self
                .components
                .iter()
                .flatten()
                .filter(|c| !c.installed)
                .map(|c| (c.id, c.name))
                .collect();
            if !missing.is_empty()
                && ui
                    .add_enabled(!running, egui::Button::new("全部一键安装"))
                    .on_hover_text(missing.iter().map(|m| m.1).collect::<Vec<_>>().join("、"))
                    .clicked()
            {
                self.install_job = Some(start_install(missing));
            }
            if ui.button("打开声音设置").on_hover_text("把 CABLE Output 设为默认麦克风").clicked() {
                let _ = std::process::Command::new("explorer").arg("ms-settings:sound").spawn();
            }
        });
        if let Some(job) = &self.install_job {
            let j = job.lock().unwrap();
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                for (err, line) in &j.log {
                    let col = if *err { Color32::from_rgb(255, 120, 110) } else { Color32::LIGHT_GREEN };
                    ui.label(RichText::new(line).color(col));
                }
                if let Some(cur) = j.current {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("{cur}：{}", j.status));
                    });
                } else if j.done && j.reboot {
                    ui.label(RichText::new("部分组件需要重启电脑后才能生效。").color(Color32::YELLOW));
                }
            });
        }
        ui.add_space(8.0);
        let Some(list) = &self.components else {
            ui.spinner();
            return;
        };
        let mut install = None;
        for c in list {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(c.name).strong().size(16.0));
                    let (t, col) = if c.installed { ("已安装", Color32::LIGHT_GREEN) } else { ("未安装", Color32::GRAY) };
                    ui.label(RichText::new(t).color(col));
                    if !c.ready {
                        ui.label(RichText::new("（配套功能开发中）").weak());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("官网").clicked() {
                            let _ = std::process::Command::new("explorer").arg(c.url).spawn();
                        }
                        if !c.installed && ui.add_enabled(!running, egui::Button::new("一键安装")).clicked() {
                            install = Some((c.id, c.name));
                        }
                    });
                });
                ui.label(c.purpose);
                if let Some(s) = &c.status {
                    ui.label(RichText::new(format!("检测到：{s}")).weak().small());
                }
                ui.label(RichText::new(c.note).weak().small());
            });
        }
        if let Some(one) = install {
            self.install_job = Some(start_install(vec![one]));
        }
    }

    fn logs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (name, label) in [("service", "服务"), ("helper", "采集进程"), ("standalone", "开发模式")] {
                if ui.selectable_label(self.log_name == name, label).clicked() {
                    self.log_name = name;
                    self.log_text = tail_log(&self.dir, name, 200);
                }
            }
            if ui.button("刷新").clicked() {
                self.log_text = tail_log(&self.dir, self.log_name, 200);
            }
            if ui.button("打开日志目录").clicked() {
                let _ = std::process::Command::new("explorer").arg(self.dir.join("logs")).spawn();
            }
        });
        egui::ScrollArea::both().id_salt("logs").stick_to_bottom(true).show(ui, |ui| {
            ui.label(RichText::new(&self.log_text).monospace().small());
        });
    }
}

fn kbps(ui: &mut egui::Ui, v: &mut u32) {
    ui.horizontal(|ui| {
        let mut auto = *v == 0;
        if ui.checkbox(&mut auto, "自动").changed() {
            *v = if auto { 0 } else { 10_000 };
        }
        if !auto {
            ui.add(egui::Slider::new(v, 1_000..=80_000).suffix(" kbps").logarithmic(true));
        }
    });
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

struct App {
    model: Model,
    window: Option<Arc<Window>>,
    surface: Option<Surface>,
    gui: Option<Gui>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("NyaRemoteControl 被控端")
            .with_inner_size(LogicalSize::new(900.0, 680.0));
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                crate::fatal(&format!("无法创建窗口：{e}"));
                el.exit();
                return;
            }
        };
        let setup = (|| -> Result<(Surface, Gui)> {
            let dev = D3dDevice::default_adapter()?;
            let hwnd = match window.window_handle()?.as_raw() {
                RawWindowHandle::Win32(h) => windows::Win32::Foundation::HWND(h.hwnd.get() as *mut _),
                _ => return Err(anyhow!("no HWND")),
            };
            let size = window.inner_size();
            Ok((Surface::new(&dev, hwnd, size.width, size.height)?, Gui::new(&window, &dev)?))
        })();
        match setup {
            Ok((s, g)) => {
                self.surface = Some(s);
                self.gui = Some(g);
            }
            Err(e) => {
                crate::fatal(&format!("无法初始化界面：{e:#}"));
                el.exit();
                return;
            }
        }
        self.window = Some(window.clone());
        window.request_redraw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let (Some(window), Some(gui)) = (self.window.clone(), self.gui.as_mut()) else { return };
        if gui.on_event(&window, &event).repaint {
            window.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                if let Some(s) = self.surface.as_mut() {
                    let _ = s.resize(size.width, size.height);
                }
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                let model = &mut self.model;
                let frame = gui.run(&window, |ctx| model.ui(ctx));
                if let Some(s) = self.surface.as_mut() {
                    let r = s.begin([0.0, 0.0, 0.0, 1.0]).and_then(|rtv| {
                        gui.paint(&rtv, (s.width, s.height), &frame)?;
                        s.present()
                    });
                    if let Err(e) = r {
                        tracing::warn!("gui render: {e:#}");
                    }
                }
                let next = Instant::now() + frame.repaint_after.min(Duration::from_secs(2));
                el.set_control_flow(ControlFlow::WaitUntil(next));
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if let ControlFlow::WaitUntil(t) = el.control_flow() {
            if Instant::now() >= t {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
        }
    }
}

pub fn run() -> Result<()> {
    // Managing the service needs admin rights: ask for them up front.
    if !winutil::is_elevated() && relaunch_elevated().is_ok() {
        return Ok(());
    }
    let _log = crate::logging::init(&paths::service_dir(), "gui", false);
    let el = EventLoop::new()?;
    let mut app = App { model: Model::new(), window: None, surface: None, gui: None };
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
    redetected: bool,
}

/// Install components one after another on a worker thread.
fn start_install(list: Vec<(components::Id, &'static str)>) -> Arc<Mutex<InstallJob>> {
    let job = Arc::new(Mutex::new(InstallJob::default()));
    let j = job.clone();
    std::thread::spawn(move || {
        for (id, name) in list {
            {
                let mut g = j.lock().unwrap();
                g.current = Some(name);
                g.status.clear();
            }
            let r = components::install(id, &mut |s| j.lock().unwrap().status = s);
            let mut g = j.lock().unwrap();
            match r {
                Ok(i) => {
                    g.reboot |= i.reboot;
                    let mut line = format!("{name} 安装完成");
                    if i.reboot {
                        line.push_str("（需要重启）");
                    }
                    if !i.note.is_empty() {
                        line.push_str("。");
                        line.push_str(&i.note);
                    }
                    g.log.push((false, line));
                }
                Err(e) => g.log.push((true, format!("{name} 安装失败：{e:#}"))),
            }
        }
        let mut g = j.lock().unwrap();
        g.current = None;
        g.done = true;
    });
    job
}
