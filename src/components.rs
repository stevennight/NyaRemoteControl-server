//! One-click installation of the optional third-party components. Versions
//! and hashes are pinned; bundled copies in `drivers\` (see
//! common/scripts/fetch-drivers.ps1) are used before downloading.

use std::path::Path;

use anyhow::{bail, Context, Result};
use nya_win::package::{self, Package};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Id {
    Cable,
    Usbip,
    Vigem,
    Vdd,
}

pub const VIGEM: Package = Package {
    file: "ViGEmBus_1.22.0_x64_x86_arm64.exe",
    url: "https://github.com/nefarius/ViGEmBus/releases/download/v1.22.0/ViGEmBus_1.22.0_x64_x86_arm64.exe",
    sha256: "89220a7865076b342892f98865f3499fb7c4cfd673159e89d352c360fd014c6a",
};

pub const USBIP: Package = Package {
    file: "USBip-0.9.8.1-x64.exe",
    url: "https://github.com/vadimgrn/usbip-win2/releases/download/v.0.9.8.1/USBip-0.9.8.1-x64.exe",
    sha256: "38cad6d4432b52d5bb9409d9ad03b72fdffc4ada4cd3a48fbeca1a2752a8518a",
};

pub const VDD: Package = Package {
    file: "VirtualDisplayDriver-x86.Driver.Only.zip",
    url: "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/download/25.7.23/VirtualDisplayDriver-x86.Driver.Only.zip",
    sha256: "e24210692b442b39af763536330ce78b423f19342b7a7792c26de3944e418b3a",
};

/// Not redistributable: always downloaded from vb-audio.com.
pub const CABLE: Package = Package {
    file: "VBCABLE_Driver_Pack45.zip",
    url: "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack45.zip",
    sha256: "b950e39f01af1d04ea623c8f6d8eb9b6ea5c477c637295fabf20631c85116bfb",
};

pub const VDD_HWID: &str = "Root\\MttVDD";
const CABLE_HWID: &str = "VBAudioVACWDM";
/// Where the virtual display driver reads its settings.
pub const VDD_SETTINGS_DIR: &str = "C:\\VirtualDisplayDriver";

/// Outcome of a successful install.
pub struct Installed {
    pub reboot: bool,
    pub note: String,
}

/// Install one component (the caller is elevated). `status` receives progress text.
pub fn install(id: Id, status: &mut dyn FnMut(String)) -> Result<Installed> {
    let pkg = match id {
        Id::Cable => CABLE,
        Id::Usbip => USBIP,
        Id::Vigem => VIGEM,
        Id::Vdd => VDD,
    };
    status("准备安装包…".into());
    let file = package::obtain(&pkg, &mut |done, total| {
        status(match total {
            Some(t) if t > 0 => format!("下载中 {:.1} / {:.1} MB", done as f64 / 1e6, t as f64 / 1e6),
            _ => format!("下载中 {:.1} MB", done as f64 / 1e6),
        })
    })?;
    status("安装中，请稍候…".into());
    tracing::info!("installing {id:?} from {}", file.display());
    let r = match id {
        Id::Vigem => setup(&file, &["/exenoui", "/qn", "/norestart"]),
        Id::Usbip => setup(&file, &["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"]).map(|mut r| {
            r.note = "USB 透传需要客户端也安装 usbipd-win（客户端 USB 窗口里可一键安装）".into();
            r
        }),
        Id::Cable => install_cable(&file),
        Id::Vdd => install_vdd(&file),
    };
    match &r {
        Ok(i) => tracing::info!("{id:?} installed (reboot needed: {})", i.reboot),
        Err(e) => tracing::warn!("{id:?} install failed: {e:#}"),
    }
    r
}

fn setup(exe: &Path, args: &[&str]) -> Result<Installed> {
    let code = package::run_hidden(exe, args)?;
    match package::setup_ok(code) {
        Some(reboot) => Ok(Installed { reboot, note: String::new() }),
        None => bail!("安装程序返回错误代码 {code}"),
    }
}

fn install_cable(zip: &Path) -> Result<Installed> {
    let dir = package::cache_dir().join("vbcable");
    package::unzip(zip, &dir)?;
    // -i install, -h hidden (no dialogs).
    let code = package::run_hidden(&dir.join("VBCABLE_Setup_x64.exe"), &["-i", "-h"])?;
    if !nya_win::devnode::exists(CABLE_HWID) {
        bail!("VB-Cable 安装程序没有装上驱动（返回代码 {code}）");
    }
    Ok(Installed { reboot: true, note: "重启后，在被控端软件里选择“CABLE Output”作为麦克风".into() })
}

fn install_vdd(zip: &Path) -> Result<Installed> {
    let dir = package::cache_dir().join("vdd");
    package::unzip(zip, &dir)?;
    let src = dir.join("VirtualDisplayDriver");
    let settings = Path::new(VDD_SETTINGS_DIR);
    std::fs::create_dir_all(settings).context("创建 C:\\VirtualDisplayDriver")?;
    if !settings.join("vdd_settings.xml").exists() {
        std::fs::copy(src.join("vdd_settings.xml"), settings.join("vdd_settings.xml"))?;
    }
    let reboot = nya_win::devnode::install_root_device(VDD_HWID, &nya_win::devnode::CLASS_DISPLAY, "Display", &src.join("MttVDD.inf"))?;
    // Keep it off until a session asks for a virtual display, so the local
    // desktop does not suddenly extend onto an invisible monitor.
    if let Err(e) = nya_win::devnode::set_enabled(VDD_HWID, false) {
        tracing::warn!("disable virtual display after install: {e:#}");
    }
    Ok(Installed { reboot, note: "已安装为禁用状态，远程会话需要时自动启用".into() })
}
