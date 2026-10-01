//! Updates of the installed service: checking GitHub Releases (on start, every
//! 12 hours and on request), downloading the installer and handing over to
//! the updater process (nya_server_core::updater), which installs it, checks
//! the new service and rolls back if it does not come up.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use nya_server_core::updater::{self, Outcome};
use nya_win::update::{self as gh, Release};
use tokio::sync::Notify;

use crate::control_pb::{self as cpb, event::Kind, update_status::State as St, Mode};
use crate::state::{unix_now, State};

const CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Updates {
    status: Mutex<cpb::UpdateStatus>,
    release: Mutex<Option<Release>>,
    /// Check now (settings changed).
    pub wake: Notify,
}

impl Updates {
    pub fn status(&self) -> cpb::UpdateStatus {
        let mut s = self.status.lock().unwrap().clone();
        s.current = env!("CARGO_PKG_VERSION").into();
        s
    }

    fn set(&self, f: impl FnOnce(&mut cpb::UpdateStatus)) {
        f(&mut self.status.lock().unwrap());
    }

    fn busy(&self) -> bool {
        matches!(St::try_from(self.status.lock().unwrap().state), Ok(St::Checking | St::Downloading | St::Installing))
    }
}

/// The directory of the running service executable (the install directory).
fn install_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_owned())
}

/// Background checks; also reports how the last update went.
pub async fn run(state: Arc<State>) {
    if let Some(o) = Outcome::take() {
        let kind = if o.ok { Kind::Service } else { Kind::Other };
        tracing::info!("last update: {}", o.message);
        state.event(kind, o.message.clone());
        state.updates.set(|s| s.message = o.message);
    }
    tokio::time::sleep(FIRST_CHECK_AFTER).await;
    loop {
        if state.config().check_updates {
            check(&state).await;
        }
        tokio::select! {
            _ = tokio::time::sleep(CHECK_EVERY) => {}
            _ = state.updates.wake.notified() => {}
        }
    }
}

/// Look for a newer release now; returns the status afterwards.
pub async fn check(state: &Arc<State>) -> cpb::UpdateStatus {
    let u = &state.updates;
    if u.busy() {
        return u.status();
    }
    u.set(|s| s.state = St::Checking as i32);
    let r = tokio::task::spawn_blocking(|| gh::latest(gh::SERVER_REPO)).await.map_err(anyhow::Error::from).and_then(|r| r);
    match r {
        Ok(rel) => {
            let newer = gh::is_newer(&rel.version, env!("CARGO_PKG_VERSION"));
            tracing::info!("update check: latest {} ({})", rel.version, if newer { "newer" } else { "not newer" });
            u.set(|s| {
                s.state = if newer { St::Available } else { St::UpToDate } as i32;
                s.latest = rel.version.clone();
                s.notes = rel.notes.clone();
                s.page = rel.page.clone();
                s.checked_unix = unix_now();
                s.progress = 0;
            });
            *u.release.lock().unwrap() = newer.then_some(rel);
        }
        Err(e) => {
            tracing::warn!("update check: {e:#}");
            u.set(|s| {
                s.state = St::Failed as i32;
                s.message = format!("检查更新失败：{e:#}");
            });
        }
    }
    u.status()
}

/// Download the newer release and hand over to the updater. Returns at once.
pub fn apply(state: &Arc<State>) -> Result<String> {
    let u = &state.updates;
    if state.mode != Mode::Service {
        bail!("开发模式不能自动更新，请运行安装包");
    }
    let Some(dir) = install_dir().filter(|d| updater::installed_by_setup(d)) else {
        let page = u.status().page;
        bail!("这份被控端不是用安装包安装的，不能自动更新；请下载安装包安装{}", if page.is_empty() { String::new() } else { format!("：{page}") });
    };
    if u.busy() {
        bail!("正在检查或安装更新，请稍候");
    }
    let Some(rel) = u.release.lock().unwrap().clone() else { bail!("没有可安装的新版本（先检查更新）") };
    let from = env!("CARGO_PKG_VERSION").to_owned();
    let to = rel.version.clone();
    u.set(|s| {
        s.state = St::Downloading as i32;
        s.progress = 0;
        s.message.clear();
    });
    state.event(Kind::Service, format!("开始更新到 {to}"));
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let u = &state.updates;
        let dl = updater::update_dir().join("download");
        let r = gh::download_installer(&rel, &dl, &mut |done, total| {
            let total = total.unwrap_or(rel.installer.size).max(1);
            let pct = (done * 100 / total).min(100) as u32;
            u.set(|s| s.progress = pct);
        })
        .and_then(|installer| {
            tracing::info!("update {from} -> {to}: installer verified, starting the updater");
            u.set(|s| {
                s.state = St::Installing as i32;
                s.progress = 100;
                s.message = format!("正在安装 {to}，服务会重启，约 1 分钟后恢复；连接会自动重连");
            });
            state.event(Kind::Service, format!("安装 {to}：服务即将重启"));
            updater::launch(&installer, &dir, &from, &to)
        });
        if let Err(e) = r {
            tracing::warn!("update: {e:#}");
            state.event(Kind::Other, format!("更新到 {to} 失败：{e:#}"));
            u.set(|s| {
                s.state = St::Failed as i32;
                s.message = format!("更新失败：{e:#}");
            });
        }
    });
    Ok(format!("正在下载 {}", u.status().latest))
}
