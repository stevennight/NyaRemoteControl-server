//! USB passthrough, client side: devices are shared with usbipd-win (optional
//! component, https://github.com/dorssel/usbipd-win) and reached by the host
//! through TUNNEL streams that end at usbipd's port on this machine.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use nya_transport::quinn::{RecvStream, SendStream};
use tokio::io::AsyncWriteExt;

pub const USBIPD_PORT: u16 = 3240;
pub const DOWNLOAD_URL: &str = "https://github.com/dorssel/usbipd-win/releases";

/// Pinned usbipd-win installer (bundled in `drivers\` or downloaded).
pub const USBIPD: nya_win::package::Package = nya_win::package::Package {
    file: "usbipd-win_5.3.0_x64.msi",
    url: "https://github.com/dorssel/usbipd-win/releases/download/v5.3.0/usbipd-win_5.3.0_x64.msi",
    sha256: "1c984914aec944de19b64eff232421439629699f8138e3ddc29301175bc6d938",
};

/// Download (if needed) and install usbipd-win silently; one UAC prompt.
pub fn install_usbipd(status: &mut dyn FnMut(String)) -> Result<()> {
    status("准备安装包…".into());
    let msi = nya_win::package::obtain(&USBIPD, &mut |done, total| {
        status(match total {
            Some(t) if t > 0 => format!("下载中 {:.1} / {:.1} MB", done as f64 / 1e6, t as f64 / 1e6),
            _ => format!("下载中 {:.1} MB", done as f64 / 1e6),
        })
    })
    .with_context(|| format!("可以手动下载 {} 放到 NyaRemoteControl.exe 旁边的 drivers 文件夹后重试", USBIPD.file))?;
    status("安装中（请在弹出的权限确认里点“是”）…".into());
    let msiexec = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| "C:\\Windows".into()).join("System32\\msiexec.exe");
    let code = nya_win::package::run_elevated(&msiexec, &format!("/i \"{}\" /qn /norestart", msi.display()))?;
    if nya_win::package::setup_ok(code).is_none() {
        bail!("安装程序返回错误代码 {code}");
    }
    if usbipd_exe().is_none() {
        bail!("安装完成但没有找到 usbipd.exe");
    }
    tracing::info!("usbipd-win installed");
    Ok(())
}

#[derive(Debug, Clone)]
pub struct UsbDevice {
    pub busid: String,
    pub description: String,
    /// Shared with usbipd (`usbipd bind`).
    pub bound: bool,
    /// Currently attached by some USB/IP client.
    pub in_use: bool,
}

/// Runs `where` when usbipd is not in its usual place: call it off the UI thread.
pub fn usbipd_exe() -> Option<PathBuf> {
    let pf = std::env::var_os("ProgramFiles").map(PathBuf::from)?;
    let p = pf.join("usbipd-win").join("usbipd.exe");
    if p.exists() {
        return Some(p);
    }
    let out = no_window(&mut Command::new("where")).arg("usbipd").output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next().map(|l| PathBuf::from(l.trim())).filter(|p| p.exists())
}

fn no_window(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
}

/// Connected devices, from `usbipd state` (JSON).
pub fn list() -> Result<Vec<UsbDevice>> {
    let exe = usbipd_exe().ok_or_else(|| anyhow!("本机没有安装 usbipd-win"))?;
    let out = no_window(&mut Command::new(exe)).arg("state").output().context("运行 usbipd")?;
    if !out.status.success() {
        bail!("usbipd state: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("解析 usbipd state")?;
    let mut devices = Vec::new();
    for d in v["Devices"].as_array().into_iter().flatten() {
        let Some(busid) = d["BusId"].as_str() else { continue }; // not plugged in
        devices.push(UsbDevice {
            busid: busid.to_owned(),
            description: d["Description"].as_str().unwrap_or("").to_owned(),
            bound: !d["PersistedGuid"].is_null(),
            in_use: !d["ClientIPAddress"].is_null(),
        });
    }
    Ok(devices)
}

/// Share a device (needs administrator rights: shows a UAC prompt).
pub fn bind(busid: &str) -> Result<()> {
    if !busid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
        bail!("无效的 busid");
    }
    let exe = usbipd_exe().ok_or_else(|| anyhow!("本机没有安装 usbipd-win"))?;
    let script = format!(
        "Start-Process -FilePath '{}' -ArgumentList 'bind','--busid','{}' -Verb RunAs -Wait -WindowStyle Hidden",
        exe.display(),
        busid
    );
    let st = no_window(&mut Command::new("powershell")).args(["-NoProfile", "-Command", &script]).status()?;
    if !st.success() {
        bail!("没有获得管理员权限，无法共享设备");
    }
    if !list()?.iter().any(|d| d.busid == busid && d.bound) {
        bail!("usbipd 没能共享该设备");
    }
    Ok(())
}

/// A TUNNEL stream from the host: connect it to usbipd on this machine.
pub async fn tunnel(mut send: SendStream, mut recv: RecvStream, port: u64) -> Result<()> {
    if port != USBIPD_PORT as u64 {
        bail!("refusing tunnel to port {port}");
    }
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", USBIPD_PORT)).await.context("连接 usbipd")?;
    let _ = tcp.set_nodelay(true);
    let (mut tr, mut tw) = tcp.split();
    let up = async {
        tokio::io::copy(&mut tr, &mut send).await?;
        send.finish()?;
        anyhow::Ok(())
    };
    let down = async {
        tokio::io::copy(&mut recv, &mut tw).await?;
        tw.shutdown().await?;
        anyhow::Ok(())
    };
    tokio::try_join!(up, down)?;
    Ok(())
}
