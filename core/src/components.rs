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
    Winfsp,
    Printer,
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

pub const WINFSP: Package = Package {
    file: "winfsp-2.1.25156.msi",
    url: "https://github.com/winfsp/winfsp/releases/download/v2.1/winfsp-2.1.25156.msi",
    sha256: "073a70e00f77423e34bed98b86e600def93393ba5822204fac57a29324db9f7a",
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

/// The printer that sends print jobs to the client (Windows' own PDF driver).
pub const PRINTER_NAME: &str = "打印到 NyaRemoteControl 客户端";
const PDF_DRIVER: &str = "Microsoft Print To PDF";

/// Where the printer writes its jobs (`%ProgramData%\NyaRemoteControl\print`).
pub fn print_spool_dir() -> std::path::PathBuf {
    crate::paths::service_dir().join("print")
}

/// The printer's port: the file each job is written to.
pub fn print_port() -> std::path::PathBuf {
    print_spool_dir().join("job.pdf")
}

/// Whether the printer exists.
pub fn printer_installed() -> bool {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ};
    let key = HSTRING::from(format!("SYSTEM\\CurrentControlSet\\Control\\Print\\Printers\\{PRINTER_NAME}"));
    let mut h = HKEY::default();
    // SAFETY: opening and closing a registry key.
    unsafe {
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, &key, 0, KEY_READ, &mut h).is_ok() {
            let _ = RegCloseKey(h);
            return true;
        }
    }
    false
}

fn install_printer() -> Result<Installed> {
    let dir = print_spool_dir();
    std::fs::create_dir_all(&dir).context("创建打印文件夹")?;
    // The spooler may write the job as the printing user: let users write here.
    let system32 = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into())).join("System32");
    let d = dir.to_string_lossy().into_owned();
    package::run_hidden(&system32.join("icacls.exe"), &[&d, "/grant", "*S-1-5-32-545:(OI)(CI)M"])?;
    let script = format!(
        "$ErrorActionPreference='Stop'; $port='{port}'; $name='{name}'; \
         if (-not (Get-PrinterDriver -Name '{driver}' -ErrorAction SilentlyContinue)) {{ exit 3 }}; \
         if (-not (Get-PrinterPort -Name $port -ErrorAction SilentlyContinue)) {{ Add-PrinterPort -Name $port }}; \
         if (-not (Get-Printer -Name $name -ErrorAction SilentlyContinue)) {{ Add-Printer -Name $name -DriverName '{driver}' -PortName $port }}; \
         exit 0",
        port = print_port().to_string_lossy(),
        name = PRINTER_NAME,
        driver = PDF_DRIVER,
    );
    let ps = system32.join("WindowsPowerShell").join("v1.0").join("powershell.exe");
    let code = package::run_hidden(&ps, &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])?;
    match code {
        0 if printer_installed() => Ok(Installed {
            reboot: false,
            note: "在被控端的程序里选择这台打印机，打印内容会发给正在操作的客户端".into(),
        }),
        3 => bail!("Windows 的“Microsoft Print to PDF”功能没有打开（控制面板 → 启用或关闭 Windows 功能）"),
        c => bail!("添加打印机失败（PowerShell 返回 {c}）"),
    }
}

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
        Id::Winfsp => WINFSP,
        Id::Printer => unreachable!("not a package"),
    };
    if id == Id::Printer {
        status("添加打印机…".into());
        let r = install_printer();
        if let Err(e) = &r {
            tracing::warn!("printer install failed: {e:#}");
        }
        return r;
    }
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
        Id::Printer => unreachable!("not a package"),
        Id::Winfsp => {
            let msiexec = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
                .join("System32")
                .join("msiexec.exe");
            let msi = file.to_string_lossy().into_owned();
            setup(&msiexec, &["/i", &msi, "/qn", "/norestart"]).map(|mut r| {
                r.note = "客户端在“连接设置 → 共享文件夹”里选择要共享的文件夹，连接后出现在被控端的一个盘符里".into();
                r
            })
        }
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
    Ok(Installed { reboot: true, note: "重启后即可使用；客户端麦克风打开期间，“CABLE Output”会自动成为被控端的默认麦克风".into() })
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

/// Playback device names that belong to a virtual cable (first match wins).
pub const CABLE_NAMES: [&str; 2] = ["CABLE Input", "VB-Audio Virtual Cable"];

/// VB-Cable's playback device, if installed (COM initialised on this thread).
pub fn cable_device_name() -> Option<String> {
    CABLE_NAMES.iter().find_map(|n| nya_win::audio::find_render_device(n).map(|(_, name)| name))
}

/// WinFsp's 64-bit DLL, if WinFsp is installed (folder mounting).
pub fn winfsp_dll() -> Option<std::path::PathBuf> {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buf = [0u16; 520];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: valid buffer and length; the value is a REG_SZ.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\WOW6432Node\\WinFsp"),
            w!("InstallDir"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    let dir = if r == ERROR_SUCCESS {
        let n = (len as usize / 2).saturating_sub(1).min(buf.len());
        std::path::PathBuf::from(String::from_utf16_lossy(&buf[..n]))
    } else {
        std::path::PathBuf::from(r"C:\Program Files (x86)\WinFsp")
    };
    let dll = dir.join("bin").join("winfsp-x64.dll");
    dll.exists().then_some(dll)
}

/// usbip-win2's command-line tool, if installed.
pub fn usbip_exe() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let candidates = [
        std::env::var_os("ProgramFiles").map(|p| PathBuf::from(p).join("USBip").join("usbip.exe")),
        std::env::var_os("ProgramFiles").map(|p| PathBuf::from(p).join("usbip-win2").join("usbip.exe")),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}
