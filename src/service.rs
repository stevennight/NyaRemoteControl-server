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
use crate::host::{self, HostConfig};
use crate::hub::Hub;
use crate::ipc_pb::HostCommand;
use crate::{ipc, net, paths, winutil};

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
    let ip: IpAddr = cfg.bind.parse().with_context(|| format!("bind address {:?}", cfg.bind))?;
    let bind = SocketAddr::new(ip, cfg.port);
    let (hub, cmd_rx) = Hub::new();

    match mode {
        HostMode::InProcess => {
            println!("被控端已启动（开发模式），UDP 端口 {}", cfg.port);
            println!("配对码：{}", auth.key().to_code());
            println!("证书指纹：{}", identity.fingerprint());
            tokio::spawn(run_in_process(hub.clone(), cmd_rx, HostConfig::from(&cfg)));
        }
        HostMode::Helper { session_changed } => {
            tokio::spawn(helper_manager(hub.clone(), cmd_rx, session_changed));
        }
    }
    net::serve(net::NetConfig { bind, server_name: cfg.display_name() }, identity, auth, hub).await
}

pub async fn run_standalone(dir: PathBuf, port: Option<u16>) -> Result<()> {
    serve_all(dir, port, HostMode::InProcess).await
}

async fn run_in_process(hub: Arc<Hub>, mut commands: mpsc::UnboundedReceiver<HostCommand>, cfg: HostConfig) {
    let (host_tx, host_rx) = mpsc::unbounded_channel();
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    std::thread::Builder::new()
        .name("nya-host".into())
        .spawn(move || host::run(host_rx, ev_tx, cfg))
        .expect("spawn host");
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
                    return;
                }
            },
        }
    }
}

/// Keep one helper running in the active console session; restart it when
/// the session changes (logon, logoff, fast user switching) or it dies.
async fn helper_manager(hub: Arc<Hub>, mut commands: mpsc::UnboundedReceiver<HostCommand>, session_changed: Arc<Notify>) {
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
                }
            }
        }
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
