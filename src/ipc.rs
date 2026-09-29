//! Named-pipe link between the service (pipe server) and the helper (pipe
//! client). Messages are length-delimited `HostCommand` / `HostEvent`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use nya_proto::framing::{read_msg, write_msg};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;

use crate::host::{self, HostConfig};
use crate::hub::Hub;
use crate::ipc_pb::{host_command::Cmd, HostCommand, HostEvent};
use crate::winutil::{PipeSa, SYSTEM_ONLY};

/// Video frames can be several MB (4K keyframes).
const MAX_IPC_MSG: usize = nya_proto::MAX_VIDEO_FRAME_LEN + 4096;

pub fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

/// Create the pipe server end, accessible to SYSTEM only.
pub fn create_server(name: &str) -> Result<NamedPipeServer> {
    let mut sa = PipeSa::new(SYSTEM_ONLY)?;
    let server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .max_instances(1)
            .in_buffer_size(1 << 20)
            .out_buffer_size(8 << 20)
            .create_with_security_attributes_raw(pipe_path(name), &mut sa.sa as *mut _ as *mut std::ffi::c_void)
            .context("create helper pipe")?
    };
    Ok(server)
}

/// Service side: forward events to the hub and commands to the helper until
/// the pipe breaks.
pub async fn bridge(pipe: NamedPipeServer, hub: &Arc<Hub>, commands: &mut mpsc::UnboundedReceiver<HostCommand>) -> Result<()> {
    let (mut rd, mut wr) = tokio::io::split(pipe);
    let mut reader = Box::pin(async move {
        loop {
            match read_msg::<HostEvent, _>(&mut rd, MAX_IPC_MSG).await {
                Ok(Some(ev)) => hub.publish(ev).await,
                Ok(None) => return Ok::<(), anyhow::Error>(()),
                Err(e) => return Err(e.into()),
            }
        }
    });
    loop {
        tokio::select! {
            r = &mut reader => return r,
            cmd = commands.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                write_msg(&mut wr, &cmd).await?;
            }
        }
    }
}

/// Helper process entry point.
pub async fn run_helper(pipe: &str) -> Result<()> {
    // Explorer (the logged-on user) calls into our clipboard data object when
    // the client's files are pasted; this process runs as SYSTEM.
    if let Err(e) = nya_win::clipboard_files::allow_interactive_callers() {
        tracing::warn!("COM security for clipboard files: {e:#}");
    }
    let path = pipe_path(pipe);
    let client = loop {
        match ClientOptions::new().open(&path) {
            Ok(c) => break c,
            Err(e) if e.raw_os_error() == Some(231) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(e) => return Err(e).context("open helper pipe"),
        }
    };
    tracing::info!("helper started in session {:?}", crate::winutil::active_console_session());
    let cfg = crate::config::ServerConfig::load_or_create(&crate::paths::service_dir())?;
    nya_media::check_runtime_versions()?;
    nya_media::init_log_level();

    let (mut rd, mut wr) = tokio::io::split(client);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    let host_cfg = HostConfig::from(&cfg);
    let host = std::thread::spawn(move || host::run(cmd_rx, ev_tx, host_cfg));

    let tx = cmd_tx.clone();
    let reader = async move {
        loop {
            match read_msg::<HostCommand, _>(&mut rd, MAX_IPC_MSG).await {
                Ok(Some(c)) => {
                    if tx.send(c).is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    };
    let writer = async move {
        while let Some(ev) = ev_rx.recv().await {
            if write_msg(&mut wr, &ev).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = reader => tracing::info!("service closed the pipe"),
        _ = writer => tracing::info!("host stopped"),
    }
    let _ = cmd_tx.send(HostCommand { cmd: Some(Cmd::Shutdown(Default::default())) });
    // Give the host a moment to release keys and GPU resources.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !host.is_finished() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}
