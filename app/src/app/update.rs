//! Client updates: look for a newer release on GitHub (at start and on
//! request), download and verify its installer, start it silently and
//! elevated (`/S /UPDATE`), and quit; the installer starts the new client
//! when it is done. A client not installed by the installer (portable zip)
//! gets the release page instead.

use std::path::PathBuf;

use nya_win::update::{self as gh, Release};
use serde::Serialize;

use super::App;
use super::launcher::Kind;
use crate::events::UiEvent;

/// What the launcher shows (the same shape as the host's update status).
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateInfo {
    pub state: &'static str,
    pub current: String,
    pub latest: String,
    pub notes: String,
    pub page: String,
    pub progress: u32,
    pub message: String,
    pub checked_unix: u64,
}

#[derive(Default)]
pub struct Updates {
    pub info: UpdateInfo,
    release: Option<Release>,
}

impl Updates {
    pub fn new() -> Self {
        Self { info: UpdateInfo { state: "idle", current: env!("CARGO_PKG_VERSION").into(), ..Default::default() }, release: None }
    }

    fn busy(&self) -> bool {
        matches!(self.info.state, "checking" | "downloading" | "installing")
    }
}

/// Installed by the installer (it can replace this program)?
fn installed() -> bool {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("uninstall.exe").exists())).unwrap_or(false)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl App {
    pub(super) fn check_update(&mut self) {
        if self.updates.busy() {
            return;
        }
        self.updates.info.state = "checking";
        self.push_state();
        let ui = self.ui_tx.clone();
        std::thread::spawn(move || ui.send(UiEvent::UpdateChecked(gh::latest(gh::CLIENT_REPO).map_err(|e| format!("{e:#}")))));
    }

    pub(super) fn on_update_checked(&mut self, r: Result<Release, String>) {
        let u = &mut self.updates;
        match r {
            Ok(rel) => {
                let newer = gh::is_newer(&rel.version, env!("CARGO_PKG_VERSION"));
                tracing::info!("update check: latest {}{}", rel.version, if newer { " (newer)" } else { "" });
                u.info.state = if newer { "available" } else { "up_to_date" };
                u.info.latest = rel.version.clone();
                u.info.notes = rel.notes.clone();
                u.info.page = rel.page.clone();
                u.info.checked_unix = now();
                u.info.message.clear();
                u.release = newer.then_some(rel);
            }
            Err(e) => {
                tracing::warn!("update check: {e}");
                u.info.state = "failed";
                u.info.message = format!("检查更新失败：{e}");
            }
        }
        self.push_state();
    }

    /// Download and install the newer release (or open its page for a portable copy).
    pub(super) fn install_update(&mut self) -> Result<(), String> {
        if self.updates.busy() {
            return Err("正在检查或下载更新".into());
        }
        let Some(rel) = self.updates.release.clone() else { return Err("没有可安装的新版本".into()) };
        if !installed() {
            let _ = std::process::Command::new("explorer").arg(&rel.page).spawn();
            return Err("这份客户端不是用安装包安装的（便携版），已打开发布页，请下载新版本".into());
        }
        self.updates.info.state = "downloading";
        self.updates.info.progress = 0;
        self.updates.info.message.clear();
        self.push_state();
        let ui = self.ui_tx.clone();
        std::thread::spawn(move || {
            let dir = std::env::temp_dir().join("nya-client-update");
            let size = rel.installer.size.max(1);
            let mut last = 0;
            let r = gh::download_installer(&rel, &dir, &mut |done, total| {
                let pct = (done * 100 / total.unwrap_or(size).max(1)).min(100) as u32;
                if pct != last {
                    last = pct;
                    ui.send(UiEvent::UpdateProgress(pct));
                }
            });
            ui.send(UiEvent::UpdateDownloaded(r.map_err(|e| format!("{e:#}"))));
        });
        Ok(())
    }

    pub(super) fn on_update_progress(&mut self, pct: u32) {
        self.updates.info.progress = pct;
        self.push_state();
    }

    pub(super) fn on_update_downloaded(&mut self, r: Result<PathBuf, String>) {
        let started = r.and_then(|installer| {
            tracing::info!("starting the installer {}", installer.display());
            // /UPDATE: wait for us to quit, then start the new client.
            nya_win::package::start_elevated(&installer, "/S /UPDATE").map_err(|e| format!("{e:#}"))
        });
        match started {
            Ok(()) => {
                self.updates.info.state = "installing";
                self.updates.info.message = "正在安装，完成后会自动重新打开".into();
                self.push_state();
                // Leave so the installer can replace the files; sessions end with a Bye.
                self.exit = true;
            }
            Err(e) => {
                self.updates.info.state = "available";
                self.updates.info.message = format!("更新失败：{e}");
                self.notice(Kind::Error, format!("更新失败：{e}"));
                self.push_state();
            }
        }
    }
}
