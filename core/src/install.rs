//! `install` / `uninstall` / `pair` / `clients`.

use std::ffi::OsString;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use nya_transport::Identity;
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::auth::load_or_create_key;
use crate::config::ServerConfig;
use crate::paths;
use crate::{SERVICE_DISPLAY, SERVICE_NAME};
use crate::win::is_elevated;

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(cmd).args(args).output().with_context(|| format!("run {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} {:?} failed: {}", args, String::from_utf8_lossy(&out.stdout));
    }
    Ok(())
}

fn require_admin() -> Result<()> {
    if !is_elevated() {
        bail!("需要管理员权限：请在“以管理员身份运行”的终端中执行");
    }
    Ok(())
}

pub fn install(port: Option<u16>) -> Result<String> {
    require_admin()?;
    let exe = paths::service_exe()?;
    if !exe.exists() {
        bail!("找不到 {}：它应当和 nya-server.exe 放在同一个目录", exe.display());
    }
    let dir = paths::service_dir();
    std::fs::create_dir_all(&dir)?;
    // Keys and pairing data: SYSTEM and Administrators only.
    run(
        "icacls",
        &[
            &dir.to_string_lossy(),
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "/grant:r",
            "*S-1-5-32-544:(OI)(CI)F",
        ],
    )
    .context("restrict data directory ACL")?;

    let mut cfg = ServerConfig::load_or_create(&dir)?;
    if let Some(p) = port {
        cfg.port = p;
        std::fs::write(dir.join("server.toml"), toml::to_string_pretty(&cfg)?)?;
    }
    let migrated = migrate_standalone_identity(&dir);
    let identity = Identity::load_or_create(&dir)?;
    let key = load_or_create_key(&dir, false)?;

    // Allow the service to send Ctrl+Alt+Del (1 = services).
    run(
        "reg",
        &[
            "add",
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System",
            "/v",
            "SoftwareSASGeneration",
            "/t",
            "REG_DWORD",
            "/d",
            "1",
            "/f",
        ],
    )?;

    firewall_allow_program(cfg.port, &exe)?;

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: vec![OsString::from("service")],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::CHANGE_CONFIG;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(s) => {
            stop_and_wait(&s);
            s.change_config(&info)?;
            s
        }
        Err(_) => manager.create_service(&info, access)?,
    };
    service.set_description("NyaRemoteControl 被控端：远程桌面画面采集、编码与输入")?;
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(3) },
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(10) },
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(30) },
        ]),
    })?;
    service.start(&[] as &[&std::ffi::OsStr])?;

    let mut out = vec![
        format!("已安装并启动服务 {SERVICE_NAME}"),
        format!("  程序：{}", exe.display()),
        format!("  数据：{}", dir.display()),
        format!("  端口：UDP {}（已添加防火墙规则）", cfg.port),
        format!("  配对码：{}", key.to_code()),
        format!("  证书指纹：{}", identity.fingerprint()),
    ];
    if migrated {
        out.push("已沿用开发模式的证书、配对码和已配对客户端，客户端无需重新配对。".into());
    }
    out.push("注意：服务直接使用上面的程序路径，移动程序目录前请先卸载。".into());
    Ok(out.join("\n"))
}

/// First install after using standalone mode: reuse its certificate and
/// pairing data so already-paired clients keep working. Returns true if copied.
fn migrate_standalone_identity(service_dir: &std::path::Path) -> bool {
    let src = paths::standalone_dir();
    if service_dir.join("identity.cert.der").exists() || !src.join("identity.cert.der").exists() {
        return false;
    }
    let mut ok = true;
    for f in ["identity.cert.der", "identity.key.der", "pairing.key", "clients.toml"] {
        let from = src.join(f);
        if from.exists() {
            if let Err(e) = std::fs::copy(&from, service_dir.join(f)) {
                eprintln!("复制 {} 失败：{e}", from.display());
                ok = false;
            }
        }
    }
    if !ok {
        // Never leave a certificate without its key; start fresh instead.
        for f in ["identity.cert.der", "identity.key.der"] {
            let _ = std::fs::remove_file(service_dir.join(f));
        }
    }
    ok
}

pub fn stop_and_wait(s: &windows_service::service::Service) {
    if let Ok(st) = s.query_status() {
        if st.current_state != ServiceState::Stopped {
            let _ = s.stop();
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(200));
                if s.query_status().map(|x| x.current_state == ServiceState::Stopped).unwrap_or(true) {
                    break;
                }
            }
        }
    }
}

pub fn uninstall(purge: bool) -> Result<String> {
    let mut out = Vec::new();
    require_admin()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE) {
        Ok(s) => {
            stop_and_wait(&s);
            s.delete()?;
            out.push(format!("已删除服务 {SERVICE_NAME}"));
        }
        Err(_) => out.push("服务未安装".into()),
    }
    let _ = run("netsh", &["advfirewall", "firewall", "delete", "rule", &format!("name={SERVICE_NAME}")]);
    if purge {
        let dir = paths::service_dir();
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
            out.push(format!("已删除数据目录 {}", dir.display()));
        }
    }
    Ok(out.join("\n"))
}

/// (Re)create the inbound UDP rule for the service executable.
fn firewall_allow_program(port: u16, exe: &std::path::Path) -> Result<()> {
    let _ = run("netsh", &["advfirewall", "firewall", "delete", "rule", &format!("name={SERVICE_NAME}")]);
    run(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name={SERVICE_NAME}"),
            "dir=in",
            "action=allow",
            "protocol=UDP",
            &format!("localport={port}"),
            &format!("program={}", exe.display()),
        ],
    )
}

/// Called by the running service (SYSTEM) when the port changes.
pub fn firewall_allow(port: u16) -> Result<()> {
    firewall_allow_program(port, &std::env::current_exe()?)
}

/// Is the installed service this directory's `nya-server-svc.exe`? `None`:
/// not installed. `Some(false)` after the program was moved, or for services
/// installed by versions that had a single executable — installing again fixes it.
pub fn service_points_here() -> Option<bool> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let s = manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_CONFIG).ok()?;
    let cmd = s.query_config().ok()?.executable_path.to_string_lossy().to_lowercase();
    let ours = paths::service_exe().ok()?.to_string_lossy().to_lowercase();
    Some(cmd.contains(&ours))
}

/// Update the firewall rule of the installed service (management side, elevated).
pub fn firewall_allow_service(port: u16) -> Result<()> {
    require_admin()?;
    firewall_allow_program(port, &paths::service_exe()?)
}
