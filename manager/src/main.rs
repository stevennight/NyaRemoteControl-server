//! `nya-server.exe`: management of the host — the GUI (no arguments) and the
//! command line. The host itself is `nya-server-svc.exe` (see lib.rs); this
//! program talks to it through the control pipe, or edits its files while it
//! is not running.

#![windows_subsystem = "windows"]

mod gui;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use nya_server_core::backend::Backend;
use nya_server_core::control_pb::event::Kind;
use nya_server_core::config::ServerConfig;
use nya_server_core::{install, paths};

#[derive(Parser)]
#[command(name = "nya-server", version, about = "NyaRemoteControl 被控端管理（不带参数运行打开图形界面）")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
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
    /// 查看被控端运行状态、当前连接和最近事件
    Status,
    /// 显示配对码
    Pair {
        /// 重新生成配对码（旧配对码失效，已配对的客户端不受影响）
        #[arg(long)]
        reset: bool,
        /// 直接读写这个数据目录（不经过运行中的被控端）
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
    /// 查看或修改设置（server.toml）；被控端运行中时立即生效
    Config {
        /// 修改一项，可重复，例如 --set port=47101 --set encoder=nvenc
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// 断开当前的远程连接
    Disconnect,
    /// 诊断：显卡、显示器、编码器、截屏与音频（由 nya-server-svc.exe 执行）
    Diag {
        /// 把结果另存到文件
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

const TOOL: &str = "nya-server cli";

fn main() {
    nya_server_core::attach_parent_console();
    let cli = Cli::parse();
    let gui = cli.cmd.is_none();
    if let Err(e) = real_main(cli) {
        if gui {
            nya_server_core::fatal(&format!("{e:#}"));
        } else {
            eprintln!("错误：{e:#}");
        }
        std::process::exit(1);
    }
}

fn real_main(cli: Cli) -> Result<()> {
    let Some(cmd) = cli.cmd else { return gui::run() };
    match cmd {
        Cmd::Install { port } => install::install(port).map(|s| println!("{s}")),
        Cmd::Uninstall { purge } => install::uninstall(purge).map(|s| println!("{s}")),
        Cmd::Status => status(),
        Cmd::Pair { reset, data_dir } => {
            let mut b = Backend::any(TOOL, data_dir)?;
            let p = if reset { b.reset_pairing_code()? } else { b.pairing()? };
            if reset {
                println!("已重新生成配对码（已配对的客户端不受影响）");
            }
            println!("配对码：{}", p.code);
            println!("证书指纹：{}", p.fingerprint);
            println!("（{}）", b.describe());
            Ok(())
        }
        Cmd::Clients { remove, data_dir } => {
            let mut b = Backend::any(TOOL, data_dir)?;
            if let Some(fp) = remove {
                println!("{}", b.remove_client(&fp)?);
            }
            let list = b.clients()?;
            if list.is_empty() {
                println!("没有已配对的客户端");
            }
            for c in &list {
                println!("{}  {}  {}", &c.fingerprint[..16.min(c.fingerprint.len())], c.name, c.paired_at);
            }
            Ok(())
        }
        Cmd::Config { set, data_dir } => {
            let mut b = Backend::any(TOOL, data_dir)?;
            let mut cfg = b.config()?;
            if !set.is_empty() {
                cfg = apply_settings(&cfg, &set)?;
                println!("{}", b.set_config(&cfg)?);
                cfg = b.config()?;
            }
            print!("{}", toml::to_string_pretty(&cfg)?);
            println!("（{}）", b.describe());
            Ok(())
        }
        Cmd::Disconnect => {
            let mut b = Backend::any(TOOL, None)?;
            println!("{}", b.disconnect("")?);
            Ok(())
        }
        Cmd::Diag { out } => {
            let exe = paths::service_exe()?;
            let mut cmd = std::process::Command::new(&exe);
            cmd.arg("diag");
            if let Some(o) = out {
                cmd.arg("--out").arg(o);
            }
            let st = cmd.status().map_err(|e| anyhow::anyhow!("无法运行 {}：{e}", exe.display()))?;
            if !st.success() {
                bail!("诊断失败（{st}）");
            }
            Ok(())
        }
    }
}

/// Apply `key=value` pairs; values are parsed as TOML where possible
/// (numbers, booleans), otherwise taken as strings.
fn apply_settings(cfg: &ServerConfig, set: &[String]) -> Result<ServerConfig> {
    let mut table = toml::Table::try_from(cfg)?;
    for kv in set {
        let Some((k, v)) = kv.split_once('=') else { bail!("{kv:?} 不是 KEY=VALUE 形式") };
        let k = k.trim();
        let Some(old) = table.get(k) else {
            bail!("没有设置项 {k:?}（可用：{}）", table.keys().cloned().collect::<Vec<_>>().join(", "))
        };
        let v = v.trim();
        let new = match old {
            toml::Value::String(_) => toml::Value::String(v.to_owned()),
            _ => toml::from_str::<toml::Table>(&format!("v = {v}"))
                .ok()
                .and_then(|mut t| t.remove("v"))
                .filter(|n| n.same_type(old))
                .ok_or_else(|| anyhow::anyhow!("{k} 的值 {v:?} 类型不对"))?,
        };
        table.insert(k.to_owned(), new);
    }
    Ok(table.try_into()?)
}

fn status() -> Result<()> {
    let mut b = Backend::any(TOOL, None)?;
    let Some(s) = b.status()? else {
        println!("被控端没有运行");
        return Ok(());
    };
    println!("{}  版本 {}", b.describe(), s.server_version);
    println!("名称：{}", s.server_name);
    if s.listen.is_empty() {
        println!("监听：!! {}", s.listen_error);
    } else {
        println!("监听：UDP {}", s.listen);
    }
    println!("证书指纹：{}", s.fingerprint);
    let host = s.host.unwrap_or_default();
    if host.running {
        println!("采集进程：运行中（会话 {}）", host.console_session);
    } else {
        println!("采集进程：未运行");
    }
    match &s.session {
        Some(c) => {
            println!("当前连接：{} {}  {}", c.client_name, c.client_version, c.remote_addr);
            if !host.stream.is_empty() {
                println!("  画面：{}", host.stream);
            }
        }
        None => println!("当前连接：无"),
    }
    for v in &s.viewers {
        println!("正在观看：{} {}  {}", v.client_name, v.client_version, v.remote_addr);
    }
    if !s.recent.is_empty() {
        println!("最近事件：");
        for e in s.recent.iter().rev().take(15).rev() {
            let tag = match Kind::try_from(e.kind).unwrap_or(Kind::Other) {
                Kind::Connected => "连接",
                Kind::Disconnected => "断开",
                Kind::Paired => "配对",
                Kind::PairingFailed => "配对失败",
                Kind::Rejected => "拒绝",
                Kind::Service | Kind::Other => "服务",
            };
            println!("  {}  [{tag}] {}", format_unix(e.unix), e.text);
        }
    }
    Ok(())
}

/// Local "MM-DD HH:MM:SS".
pub fn format_unix(t: u64) -> String {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ft = (t + 11_644_473_600) * 10_000_000;
    let ft = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err() || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err() {
            return format!("unix:{t}");
        }
    }
    format!("{:02}-{:02} {:02}:{:02}:{:02}", local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond)
}
