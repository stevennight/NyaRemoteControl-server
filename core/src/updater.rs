//! Installing an update without losing the host.
//!
//! The service downloads and verifies the installer, copies the management
//! program to `%ProgramData%\NyaRemoteControl\update\nya-updater.exe` and
//! starts it (`nya-updater.exe apply-update …`) as a process of its own. The
//! updater outlives the service, which the installer stops:
//!
//! 1. back up the installed program files;
//! 2. run the installer silently (it stops the service, replaces the files
//!    and re-registers + starts the service);
//! 3. check the new service answers on the control pipe with the new version;
//! 4. if not: put the backup back, re-register and start the old service;
//! 5. whatever happened: make sure the service runs.
//!
//! The outcome goes to `update\result.json`, which the service reports
//! (recent events, update status) when it starts; the steps to `logs\update.log`.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::control::ControlClient;
use crate::control_pb::Mode;
use crate::paths;
use crate::SERVICE_NAME;

pub const UPDATER_EXE: &str = "nya-updater.exe";
/// The management program, which is also the updater.
pub const MANAGER_EXE: &str = "nya-server.exe";

/// `%ProgramData%\NyaRemoteControl\update`: downloads, the updater, the backup and the result.
pub fn update_dir() -> PathBuf {
    paths::service_dir().join("update")
}

/// What the updater did, for the service to report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Outcome {
    pub from: String,
    pub to: String,
    pub ok: bool,
    pub rolled_back: bool,
    pub message: String,
    pub unix: u64,
}

impl Outcome {
    pub fn path() -> PathBuf {
        update_dir().join("result.json")
    }

    /// Read and remove the outcome of the last update, if any.
    pub fn take() -> Option<Self> {
        let p = Self::path();
        let text = std::fs::read_to_string(&p).ok()?;
        let _ = std::fs::remove_file(&p);
        serde_json::from_str(&text).ok()
    }
}

/// Look for a newer release from this program (no service to ask).
pub fn check_here() -> Result<(crate::control_pb::UpdateStatus, Option<nya_win::update::Release>)> {
    use crate::control_pb::update_status::State as St;
    let rel = nya_win::update::latest(nya_win::update::SERVER_REPO)?;
    let current = env!("CARGO_PKG_VERSION");
    let newer = nya_win::update::is_newer(&rel.version, current);
    let s = crate::control_pb::UpdateStatus {
        state: if newer { St::Available } else { St::UpToDate } as i32,
        current: current.into(),
        latest: rel.version.clone(),
        notes: rel.notes.clone(),
        page: rel.page.clone(),
        checked_unix: now(),
        ..Default::default()
    };
    Ok((s, newer.then_some(rel)))
}

/// Without a service that updates itself: download the newer installer
/// and start it normally (its window, the user's choices).
pub fn install_here() -> Result<String> {
    let (_, rel) = check_here()?;
    let Some(rel) = rel else { bail!("已经是最新版本") };
    let installer = nya_win::update::download_installer(&rel, &update_dir().join("download"), &mut |_, _| {})?;
    Command::new(&installer).spawn().context("启动安装程序")?;
    Ok(format!("已启动 {} 的安装程序", rel.version))
}

/// Was this program installed by the installer (so an update can replace it)?
pub fn installed_by_setup(install_dir: &Path) -> bool {
    install_dir.join("uninstall.exe").exists()
}

/// Copy the management program out of the install directory and start it
/// as the updater. Returns once it runs; it continues on its own.
pub fn launch(installer: &Path, install_dir: &Path, from: &str, to: &str) -> Result<()> {
    let dir = update_dir();
    std::fs::create_dir_all(&dir)?;
    let updater = dir.join(UPDATER_EXE);
    std::fs::copy(install_dir.join(MANAGER_EXE), &updater).context("复制更新程序")?;
    let _ = std::fs::remove_file(Outcome::path());
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let spawn = |flags: u32| {
        Command::new(&updater)
            .arg("apply-update")
            .arg("--installer")
            .arg(installer)
            .arg("--install-dir")
            .arg(install_dir)
            .arg("--from")
            .arg(from)
            .arg("--to")
            .arg(to)
            .current_dir(&dir)
            .creation_flags(flags)
            .spawn()
    };
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    // Breaking away fails when the caller's job does not allow it; it is not in one normally.
    spawn(base | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| spawn(base)).context("启动更新程序")?;
    Ok(())
}

struct Log(std::fs::File);

impl Log {
    fn open() -> Self {
        let dir = paths::service_dir().join("logs");
        let _ = std::fs::create_dir_all(&dir);
        let f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("update.log"));
        Self(f.unwrap_or_else(|_| std::fs::File::create(std::env::temp_dir().join("nya-update.log")).expect("log file")))
    }

    fn line(&mut self, s: impl AsRef<str>) {
        let t = timestamp();
        let _ = writeln!(self.0, "{t} {}", s.as_ref());
        let _ = self.0.flush();
    }
}

/// The updater: `nya-updater.exe apply-update --installer … --install-dir … --from … --to …`.
pub fn apply(installer: &Path, install_dir: &Path, from: &str, to: &str) -> Result<()> {
    let mut log = Log::open();
    log.line(format!("update {from} -> {to}: installer {}, program {}", installer.display(), install_dir.display()));
    // Let the service answer the request that started us.
    std::thread::sleep(Duration::from_secs(2));

    let mut outcome = Outcome { from: from.into(), to: to.into(), unix: now(), ..Default::default() };
    let backup = update_dir().join("backup");
    let result = (|| -> Result<()> {
        copy_program(install_dir, &backup).context("备份当前版本")?;
        log.line("backed up the current version");
        let code = run_installer(installer, install_dir, &mut log)?;
        if code != 0 {
            bail!("安装程序返回错误代码 {code}");
        }
        wait_for_version(to, Duration::from_secs(120)).context("新版本的服务没有正常启动")?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            outcome.ok = true;
            outcome.message = format!("已更新到 {to}");
            log.line("update done; new service answers");
        }
        Err(e) => {
            log.line(format!("update failed: {e:#}; rolling back"));
            outcome.message = format!("更新到 {to} 失败：{e:#}");
            match rollback(&backup, install_dir, from, &mut log) {
                Ok(()) => {
                    outcome.rolled_back = true;
                    outcome.message += &format!("；已恢复到 {from}");
                    log.line("rolled back");
                }
                Err(e) => {
                    outcome.message += &format!("；恢复旧版本也失败：{e:#}");
                    log.line(format!("rollback failed: {e:#}"));
                }
            }
        }
    }
    // Whatever happened: the host must stay reachable.
    match ensure_running() {
        Ok(()) => log.line("service is running"),
        Err(e) => log.line(format!("cannot start the service: {e:#}")),
    }
    if let Ok(text) = serde_json::to_string_pretty(&outcome) {
        let _ = std::fs::write(Outcome::path(), text);
    }
    if outcome.ok {
        let _ = std::fs::remove_dir_all(&backup);
        let _ = std::fs::remove_file(installer);
    }
    Ok(())
}

/// Local time, `2026-10-01 20:15:03`.
fn timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The program files (top level and `drivers`), not the uninstaller's state.
fn copy_program(from: &Path, to: &Path) -> Result<()> {
    let _ = std::fs::remove_dir_all(to);
    copy_dir(from, to)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), &target).with_context(|| format!("复制 {}", e.path().display()))?;
        }
    }
    Ok(())
}

fn no_window(cmd: &mut Command) -> &mut Command {
    cmd.creation_flags(0x0800_0000)
}

/// Silent install into the current directory; 15 minutes at most.
fn run_installer(installer: &Path, install_dir: &Path, log: &mut Log) -> Result<i32> {
    // NSIS: /S silent, /UPDATE (close the management program instead of
    // asking), /D=<dir> last and unquoted.
    let mut child = no_window(&mut Command::new(installer))
        .arg("/S")
        .arg("/UPDATE")
        .raw_arg(format!("/D={}", install_dir.display()))
        .spawn()
        .context("启动安装程序")?;
    log.line("installer started");
    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    loop {
        if let Some(st) = child.try_wait()? {
            let code = st.code().unwrap_or(-1);
            log.line(format!("installer exited with {code}"));
            return Ok(code);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!("安装程序 15 分钟没有结束，已终止");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Wait until the service answers on its control pipe with this version.
fn wait_for_version(version: &str, within: Duration) -> Result<()> {
    let deadline = Instant::now() + within;
    loop {
        let last = match ControlClient::connect(Mode::Service, "nya-updater") {
            Ok(c) => {
                let v = c.hello.server_version.clone();
                if v == version || v.starts_with(&format!("{version} ")) {
                    return Ok(());
                }
                format!("服务的版本是 {v}")
            }
            Err(e) => format!("{e:#}"),
        };
        if Instant::now() > deadline {
            bail!("{} 秒内没有等到 {version} 的服务（{last}）", within.as_secs());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Put the previous version back and start it.
fn rollback(backup: &Path, install_dir: &Path, from: &str, log: &mut Log) -> Result<()> {
    if !backup.join(MANAGER_EXE).exists() {
        bail!("没有备份");
    }
    stop_service();
    let _ = no_window(&mut Command::new("taskkill")).args(["/F", "/IM", paths::SERVICE_EXE]).status();
    // Files may stay locked for a moment after the processes exit.
    let mut tries = 0;
    loop {
        match copy_dir(backup, install_dir) {
            Ok(()) => break,
            Err(e) if tries < 10 => {
                tries += 1;
                log.line(format!("restore: {e:#}; retrying"));
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(e) => return Err(e),
        }
    }
    log.line("restored the previous files");
    let st = no_window(&mut Command::new(install_dir.join(MANAGER_EXE))).arg("install").status().context("重新安装服务")?;
    log.line(format!("install (previous version) exited with {:?}", st.code()));
    wait_for_version(from, Duration::from_secs(60))
}

fn stop_service() {
    if let Ok(m) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
        if let Ok(s) = m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP) {
            crate::install::stop_and_wait(&s);
        }
    }
}

/// Start the service unless it runs (waits up to 30 s for it to come up).
fn ensure_running() -> Result<()> {
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let s = m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::START)?;
    for _ in 0..60 {
        match s.query_status()?.current_state {
            ServiceState::Running => return Ok(()),
            ServiceState::Stopped => {
                let _ = s.start(&[] as &[&std::ffi::OsStr]);
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!("服务 30 秒内没有进入运行状态")
}
