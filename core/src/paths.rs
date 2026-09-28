use std::path::PathBuf;

/// Data directory of the installed service (`%ProgramData%\NyaRemoteControl`).
pub fn service_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("NyaRemoteControl")
}

/// Data directory for standalone (development) mode.
pub fn standalone_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("NyaRemoteControl").join("server")
}

pub const SERVICE_EXE: &str = "nya-server-svc.exe";

/// The host executable next to the running one (installed as the service).
pub fn service_exe() -> std::io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name(SERVICE_EXE))
}
