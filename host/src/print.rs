//! Printing on the host comes out on the client (FEATURE_PRINT).
//!
//! The optional component "打印到客户端" adds a printer that uses Windows'
//! own "Microsoft Print To PDF" driver with a file port: every job is written
//! to `print\job.pdf` in the data directory. This watcher picks each finished
//! file up, renames it and hands it to the sessions; the operating client's
//! session sends it (FILE stream, purpose PRINT) and deletes it. Files nobody
//! took are removed after a day.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nya_proto::pb;
use nya_server_core::components;

use crate::state::State;

/// Done when the spooler has closed it: nobody else may have it open.
fn finished(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    let Ok(m) = std::fs::metadata(path) else { return false };
    let idle = m.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d > Duration::from_millis(1500));
    m.len() > 0 && idle && std::fs::OpenOptions::new().read(true).share_mode(0).open(path).is_ok()
}

/// "被控端打印 2026-10-01 21.30.05" (local time).
fn stamp() -> String {
    // SAFETY: no arguments; returns a SYSTEMTIME by value.
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("被控端打印 {:04}-{:02}-{:02} {:02}.{:02}.{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let old = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|d| d > Duration::from_secs(86_400));
        if old && e.path() != components::print_port() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Watch the printer's output file (service mode only).
pub async fn watch(state: Arc<State>) {
    let dir = components::print_spool_dir();
    let job = components::print_port();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut pruned = std::time::Instant::now() - Duration::from_secs(3600);
    loop {
        tick.tick().await;
        if pruned.elapsed() > Duration::from_secs(3600) {
            pruned = std::time::Instant::now();
            prune(&dir);
        }
        if !finished(&job) {
            continue;
        }
        let mut target = dir.join(format!("{}.pdf", stamp()));
        let mut n = 2;
        while target.exists() {
            target = dir.join(format!("{} ({n}).pdf", stamp()));
            n += 1;
        }
        match std::fs::rename(&job, &target) {
            Ok(()) => {
                tracing::info!("print job ready: {}", target.display());
                if state.prints.send(target.clone()).is_err() {
                    tracing::info!("no client connected to receive the print job; it is kept for a day in {}", dir.display());
                }
            }
            Err(e) => tracing::debug!("print job not ready yet: {e}"),
        }
    }
}

/// Send one print job to the client, then delete it.
pub async fn send(link: nya_transport::files::FileLink, path: PathBuf) {
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "print.pdf".into());
    let h = pb::FileHeader {
        transfer_id: rand::random(),
        name: name.clone(),
        size,
        purpose: pb::FilePurpose::Print as i32,
        index: 0,
        count: 1,
        path: String::new(),
    };
    match link.send_file(h, &path, None, |_| {}).await {
        Ok(()) => {
            tracing::info!("print job {name} sent to the client ({size} bytes)");
            let _ = std::fs::remove_file(&path);
        }
        Err(e) => tracing::warn!("sending print job {name}: {e:#}"),
    }
}
