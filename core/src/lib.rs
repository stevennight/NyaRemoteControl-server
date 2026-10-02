//! What the host service (`nya-server-svc.exe`, `host/`), the program users
//! open (`NyaRemoteControl.exe`, `app/`, its "本机" section) and the command
//! line (`nya-server.exe`, `cli/`) share about the host: settings,
//! pairing data, paths, logging, installation, and the control pipe protocol
//! with its client.
//!
//! This crate must not depend on the capture / encoding stack (nya-media,
//! FFmpeg): the command line links it, and must be able to run — and
//! keep running — while the host's binaries are replaced.

pub mod auth;
pub mod backend;
pub mod components;
pub mod config;
pub mod control;
pub mod install;
pub mod logging;
pub mod paths;
pub mod updater;
pub mod win;

pub mod control_pb {
    include!(concat!(env!("OUT_DIR"), "/nya.control.rs"));
}

pub const SERVICE_NAME: &str = "NyaRemoteControl";
pub const SERVICE_DISPLAY: &str = "NyaRemoteControl 远程桌面";

/// Show an error to a user who may have no console.
pub fn fatal(msg: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    tracing::error!("{msg}");
    eprintln!("错误：{msg}");
    unsafe {
        MessageBoxW(None, &HSTRING::from(msg), &HSTRING::from("NyaRemoteControl"), MB_OK | MB_ICONERROR);
    }
}

/// GUI-subsystem executables: reuse the terminal we were started from, if any.
pub fn attach_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
