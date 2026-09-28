//! Service mode (SCM integration + helper manager) and standalone mode.

use std::ffi::OsString;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use nya_transport::Identity;
use rand::Rng;
use tokio::sync::{mpsc, Notify};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::auth::AuthStore;
use crate::config::ServerConfig;
use crate::control_pb::{event::Kind, Mode};
use crate::host::{self, HostConfig};
use crate::hub::Hub;
use crate::ipc_pb::{host_command::Cmd, HostCommand};
use crate::state::State;
use crate::{control, ipc, net, paths, winutil};

pub const SERVICE_NAME: &str = "NyaRemoteControl";
pub const SERVICE_DISPLAY: &str = "NyaRemoteControl 远程桌面";

enum HostMode {
    InProcess,
    Helper { session_changed: Arc<Notify> },
}

async fn serve_all(dir: PathBuf, port: Option<u16>, mode: HostMode) -> Result<()> {
    let mut cfg = ServerConfig::load_or_create(&dir)?;
    if let Some(p) = port {
        cfg.port = p;
    }
    let identity = Identity::load_or_create(&dir)?;
    let _ = net::SERVER_FP.set(identity.fingerprint());
    let auth = Arc::new(AuthStore::open(&dir)?);
    let (hub, cmd_rx) = Hub::new();
    let pb_mode = match mode {
        HostMode::InProcess => Mode::Standalone,
        HostMode::Helper { .. } => Mode::Service,
    };
    let state = State::new(pb_mode, dir, cfg.clone(), identity.fingerprint().to_string(), auth.clone(), hub);
    state.event(Kind::Service, format!("被控端已启动（版本 {}）", env!("CARGO_PKG_VERSION")));
    tokio::spawn(control::serve(state.clone()));

    match mode {
        HostMode::InProcess => {
            println!("被控端已启动（开发模式），UDP 端口 {}", cfg.port);
            println!("配对码：{}", auth.key().to_code());
            println!("证书指纹：{}", identity.fingerprint());
            tokio::spawn(run_in_process(state.clone(), cmd_rx));
        }
        HostMode::Helper { session_changed } => {
            tokio::spawn(helper_manager(state.clone(), cmd_rx, session_changed));
        }
    }
    listen(state, identity).await
}

/// Keep the network endpoint bound to the configured address; re-bind when
/// the control pipe changes it. A bind failure is reported (status, event)
/// and retried instead of stopping the service, so it can still be fixed
/// from the management tools.
async fn listen(state: Arc<State>, identity: Identity) -> Result<()> {
    loop {
        let cfg = state.config();
        let bound = cfg
            .bind
            .parse::<IpAddr>()
            .with_context(|| format!("监听地址 {:?} 无效", cfg.bind))
            .and_then(|ip| nya_transport::endpoint::server_endpoint(SocketAddr::new(ip, cfg.port), &identity));
        let endpoint = match bound {
            Ok(ep) => ep,
            Err(e) => {
                let msg = format!("无法监听 UDP {}：{e:#}", cfg.port);
                tracing::error!("{msg}");
                state.set_listening(Err(msg.clone()));
                state.event(Kind::Service, msg);
                tokio::select! {
                    _ = state.rebind.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                }
                continue;
            }
        };
        state.set_listening(Ok(endpoint.local_addr().map(|a| a.to_string()).unwrap_or_default()));
        tokio::select! {
            r = net::serve_endpoint(endpoint.clone(), state.clone()) => return r,
            _ = state.rebind.notified() => {
                tracing::info!("listen address changed; re-binding");
                endpoint.close(0u32.into(), "被控端的监听地址已更改".as_bytes());
                let _ = tokio::time::timeout(Duration::from_secs(3), endpoint.wait_idle()).await;
            }
        }
    }
}

pub async fn run_standalone(dir: PathBuf, port: Option<u16>) -> Result<()> {
    serve_all(dir, port, HostMode::InProcess).await
}

/// Standalone mode: the host runs on a thread of this process. It is
/// restarted when the capture / encoding settings change.
async fn run_in_process(state: Arc<State>, mut commands: mpsc::UnboundedReceiver<HostCommand>) {
    let hub = state.hub.clone();
    loop {
        let cfg = HostConfig::from(&state.config());
        let (host_tx, host_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::channel(256);
        std::thread::Builder::new()
            .name("nya-host".into())
            .spawn(move || host::run(host_rx, ev_tx, cfg))
            .expect("spawn host");
        state.set_host(true, winutil::active_console_session().unwrap_or(0));
        hub.host_restarted();
        loop {
            tokio::select! {
                c = commands.recv() => match c {
                    Some(c) => { let _ = host_tx.send(c); }
                    None => return,
                },
                e = ev_rx.recv() => match e {
                    Some(e) => hub.publish(e).await,
                    None => {
                        tracing::error!("host stopped");
                        state.set_host(false, 0);
                        return;
                    }
                },
                _ = state.restart_host.notified() => break,
            }
        }
        tracing::info!("restarting host with new settings");
        let _ = host_tx.send(HostCommand { cmd: Some(Cmd::Shutdown(Default::default())) });
        drop(host_tx);
        // The host threads block on a full event channel: drain until they are gone.
        let _ = tokio::time::timeout(Duration::from_secs(5), async { while ev_rx.recv().await.is_some() {} }).await;
        state.set_host(false, 0);
    }
}

/// Keep one helper running in the active console session; restart it when
/// the session changes (logon, logoff, fast user switching), it dies, or the
/// capture / encoding settings change.
async fn helper_manager(state: Arc<State>, mut commands: mpsc::UnboundedReceiver<HostCommand>, session_changed: Arc<Notify>) {
    let hub = state.hub.clone();
    let job = winutil::kill_on_close_job().map_err(|e| tracing::warn!("job object: {e:#}")).ok();
    let exe = std::env::current_exe().unwrap_or_default();
    loop {
        let Some(session) = winutil::active_console_session() else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        let name = format!("nya-helper-{:016x}", rand::thread_rng().gen::<u64>());
        let server = match ipc::create_server(&name) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("{e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let cmdline = format!("\"{}\" helper --pipe {}", exe.display(), name);
        let process = match winutil::spawn_in_session(session, &cmdline, job.as_ref()) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                tracing::error!("cannot start helper in session {session}: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        if !matches!(tokio::time::timeout(Duration::from_secs(15), server.connect()).await, Ok(Ok(()))) {
            tracing::error!("helper did not connect");
            let p = process.clone();
            let _ = tokio::task::spawn_blocking(move || winutil::wait_or_kill(&p, 0)).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        // Drop commands meant for the previous helper (stale input must not replay).
        while commands.try_recv().is_ok() {}
        hub.host_restarted();
        state.set_host(true, session);
        tracing::info!("helper connected (session {session})");

        {
            let bridge = ipc::bridge(server, &hub, &mut commands);
            tokio::pin!(bridge);
            let mut poll = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    r = &mut bridge => {
                        tracing::info!("helper link closed: {r:?}");
                        break;
                    }
                    _ = session_changed.notified() => {
                        if winutil::active_console_session() != Some(session) {
                            tracing::info!("console session changed");
                            break;
                        }
                    }
                    _ = poll.tick() => {
                        if winutil::active_console_session() != Some(session) {
                            tracing::info!("console session changed");
                            break;
                        }
                    }
                    _ = state.restart_host.notified() => {
                        tracing::info!("settings changed; restarting helper");
                        break;
                    }
                }
            }
        }
        state.set_host(false, 0);
        // The pipe is closed now; the helper exits on its own.
        let p = process.clone();
        let _ = tokio::task::spawn_blocking(move || winutil::wait_or_kill(&p, 4000)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

define_windows_service!(ffi_service_main, service_main);

pub fn run_as_service() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).context("service dispatcher (is this started by the SCM?)")?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    let dir = paths::service_dir();
    let _log = crate::logging::init(&dir, "service", false);
    if let Err(e) = service_body(dir) {
        tracing::error!("service failed: {e:#}");
    }
}

fn service_body(dir: PathBuf) -> Result<()> {
    let stop = Arc::new(Notify::new());
    let session_changed = Arc::new(Notify::new());
    let (stop2, sess2) = (stop.clone(), session_changed.clone());
    let handler = move |ev| match ev {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            stop2.notify_one();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(_) => {
            sess2.notify_one();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    let set = |state: ServiceState, accept: ServiceControlAccept| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(5),
            process_id: None,
        });
    };
    set(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN | ServiceControlAccept::SESSION_CHANGE,
    );
    tracing::info!("service started (version {})", env!("CARGO_PKG_VERSION"));

    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async {
        tokio::select! {
            r = serve_all(dir, None, HostMode::Helper { session_changed }) => r,
            _ = stop.notified() => Ok(()),
        }
    });
    if let Err(e) = &result {
        tracing::error!("{e:#}");
    }
    set(ServiceState::StopPending, ServiceControlAccept::empty());
    rt.shutdown_timeout(Duration::from_secs(3));
    set(ServiceState::Stopped, ServiceControlAccept::empty());
    tracing::info!("service stopped");
    Ok(())
}
