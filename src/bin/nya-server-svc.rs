//! `nya-server-svc.exe`: runs the host — Windows service, per-session helper,
//! standalone (development) mode and diagnostics. Managed with `nya-server.exe`.

#![windows_subsystem = "windows"]

use anyhow::Result;
use clap::{Parser, Subcommand};
use nya_server::{diag, ipc, logging, paths, service};

#[derive(Parser)]
#[command(name = "nya-server-svc", version, about = "NyaRemoteControl 被控端服务程序（管理请用 nya-server.exe）")]
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
    /// 诊断：列出显卡、显示器、编码器能力、截屏与音频测试结果
    Diag {
        /// 把结果另存到文件
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// 测试虚拟显示器：创建 1920x1080 虚拟显示器，打印驱动和显示器状态，保持一会儿后移除（需要管理员）
    VddTest {
        /// 隐私屏模式（关闭物理显示器并屏蔽本机键鼠）
        #[arg(long)]
        private: bool,
        /// 保持的秒数
        #[arg(long, default_value_t = 15)]
        secs: u64,
        /// 打开驱动自身的日志并打印（C:\VirtualDisplayDriver\Logs）
        #[arg(long)]
        driver_log: bool,
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

fn main() {
    nya_server::attach_parent_console();
    if let Err(e) = real_main(Cli::parse()) {
        eprintln!("错误：{e:#}");
        std::process::exit(1);
    }
}

fn real_main(cli: Cli) -> Result<()> {
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
        Cmd::VddTest { private, secs, driver_log } => {
            let _log = logging::init(&paths::service_dir(), "vdd-test", true);
            nya_win::dpi::set_per_monitor_aware();
            nya_server::host::vdisplay::self_test(private, std::time::Duration::from_secs(secs.clamp(3, 600)), driver_log)
        }
        Cmd::Diag { out } => {
            nya_win::dpi::set_per_monitor_aware();
            diag::run(out)
        }
    }
}
