//! `install` / `uninstall` / `pair` / `clients`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use nya_transport::Identity;
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::auth::{load_or_create_key, AuthStore};
use crate::config::ServerConfig;
use crate::paths;
use crate::service::{SERVICE_DISPLAY, SERVICE_NAME};
use crate::winutil::is_elevated;

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

pub fn install(port: Option<u16>) -> Result<()> {
    require_admin()?;
    let exe = std::env::current_exe()?;
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
            &format!("localport={}", cfg.port),
            &format!("program={}", exe.display()),
        ],
    )?;

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

    println!("已安装并启动服务 {SERVICE_NAME}");
    println!("  程序：{}", exe.display());
    println!("  数据：{}", dir.display());
    println!("  端口：UDP {}（已添加防火墙规则）", cfg.port);
    println!("  配对码：{}", key.to_code());
    println!("  证书指纹：{}", identity.fingerprint());
    println!("注意：服务直接使用上面的程序路径，移动或删除该文件前请先卸载。");
    Ok(())
}

fn stop_and_wait(s: &windows_service::service::Service) {
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

pub fn uninstall(purge: bool) -> Result<()> {
    require_admin()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE) {
        Ok(s) => {
            stop_and_wait(&s);
            s.delete()?;
            println!("已删除服务 {SERVICE_NAME}");
        }
        Err(_) => println!("服务未安装"),
    }
    let _ = run("netsh", &["advfirewall", "firewall", "delete", "rule", &format!("name={SERVICE_NAME}")]);
    if purge {
        let dir = paths::service_dir();
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
            println!("已删除数据目录 {}", dir.display());
        }
    }
    Ok(())
}

fn pick_dir(data_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(d) = data_dir {
        return Ok(d);
    }
    let svc = paths::service_dir();
    if svc.join("pairing.key").exists() || svc.exists() {
        if !is_elevated() {
            bail!("服务的数据目录只有管理员能读取：请以管理员身份运行，或用 --data-dir 指定开发模式目录");
        }
        return Ok(svc);
    }
    Ok(paths::standalone_dir())
}

pub fn pair(data_dir: Option<PathBuf>, reset: bool) -> Result<()> {
    let dir = pick_dir(data_dir)?;
    let key = load_or_create_key(&dir, reset)?;
    let id = Identity::load_or_create(&dir)?;
    if reset {
        println!("已重新生成配对码（服务会在下次连接时使用新配对码；已配对的客户端不受影响）");
    }
    println!("配对码：{}", key.to_code());
    println!("证书指纹：{}", id.fingerprint());
    println!("数据目录：{}", dir.display());
    Ok(())
}

pub fn clients(data_dir: Option<PathBuf>, remove: Option<String>) -> Result<()> {
    let dir = pick_dir(data_dir)?;
    let mut list = AuthStore::list(&dir);
    if let Some(prefix) = remove {
        let prefix = prefix.to_ascii_lowercase().replace('-', "");
        let before = list.len();
        list.retain(|c| !c.fingerprint.starts_with(&prefix));
        AuthStore::save_list(&dir, list.clone())?;
        println!("已移除 {} 个客户端", before - list.len());
    }
    if list.is_empty() {
        println!("没有已配对的客户端");
    }
    for c in &list {
        println!("{}  {}  {}", &c.fingerprint[..16], c.name, c.paired_at);
    }
    Ok(())
}
