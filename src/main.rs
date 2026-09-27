//! NyaRemoteControl host.
//!
//! One executable, several roles (design doc §1.1):
//! * `service`    – Windows service (Session 0, SYSTEM): network, sessions, helper management
//! * `helper`     – runs in the active console session with a SYSTEM token: capture, encode, input
//! * `standalone` – single user-mode process for development (no lock screen / UAC support)
//! * `install` / `uninstall` / `pair` / `clients` / `diag` – administration

mod auth;
mod config;
mod diag;
mod host;
mod hub;
mod install;
mod ipc;
mod logging;
mod net;
mod paths;
mod service;
mod winutil;

pub mod ipc_pb {
    include!(concat!(env!("OUT_DIR"), "/nya.ipc.rs"));
}

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nya-server", version, about = "NyaRemoteControl 被控端")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 开发模式：单进程运行（当前用户权限，不支持锁屏/UAC 界面）
    Standalone {
        /// 数据目录（默认 %LOCALAPPDATA%\NyaRemoteControl\server）
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
        /// 覆盖配置中的端口
        #[arg(long)]
        port: Option<u16>,
    },
    /// 安装为 Windows 服务（需要管理员权限）
    Install {
        #[arg(long)]
        port: Option<u16>,
    },
    /// 卸载 Windows 服务
    Uninstall {
        /// 同时删除数据目录（证书、配对信息、日志）
        #[arg(long)]
        purge: bool,
    },
    /// 显示配对码
    Pair {
        /// 重新生成配对码（旧配对码失效，已配对的客户端不受影响）
        #[arg(long)]
        reset: bool,
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// 管理已配对的客户端
    Clients {
        /// 移除指定指纹（前缀即可）的客户端
        #[arg(long)]
        remove: Option<String>,
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// 诊断：列出显卡、显示器、编码器能力、截屏与音频测试结果
    Diag {
        /// 把结果另存到文件
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// （内部）由服务控制管理器启动
    #[command(hide = true)]
    Service,
    /// （内部）由服务在用户会话中启动
    #[command(hide = true)]
    Helper {
        #[arg(long)]
        pipe: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Standalone { data_dir, port } => {
            let dir = data_dir.unwrap_or_else(paths::standalone_dir);
            let _log = logging::init(&dir, "standalone", true);
            nya_win::dpi::set_per_monitor_aware();
            nya_media::check_runtime_versions()?;
            nya_media::init_log_level();
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(service::run_standalone(dir, port))
        }
        Cmd::Service => service::run_as_service(),
        Cmd::Helper { pipe } => {
            let _log = logging::init(&paths::service_dir(), "helper", false);
            nya_win::dpi::set_per_monitor_aware();
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(ipc::run_helper(&pipe))
        }
        Cmd::Install { port } => install::install(port),
        Cmd::Uninstall { purge } => install::uninstall(purge),
        Cmd::Pair { reset, data_dir } => install::pair(data_dir, reset),
        Cmd::Clients { remove, data_dir } => install::clients(data_dir, remove),
        Cmd::Diag { out } => {
            nya_win::dpi::set_per_monitor_aware();
            diag::run(out)
        }
    }
}
