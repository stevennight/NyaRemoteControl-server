//! Tell the user's Explorer about the drive of the client's folders.
//!
//! The service (session 0) creates the drive letter through WinFsp; Windows
//! does not announce letters created with DefineDosDevice, so Explorer in the
//! user's session never refreshes "This PC" and the drive seems missing.
//! The helper runs in that session and sends what a new disk would: a
//! WM_DEVICECHANGE volume arrival / removal and the shell's drive event.

use windows::core::HSTRING;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_DRIVEADD, SHCNE_DRIVEREMOVED, SHCNF_PATHW};
use windows::Win32::System::StationsAndDesktops::{
    BroadcastSystemMessageW, BSF_FORCEIFHUNG, BSF_IGNORECURRENTTASK, BSF_NOHANG, BSM_APPLICATIONS,
};
use windows::Win32::UI::WindowsAndMessaging::WM_DEVICECHANGE;

const DBT_DEVICEARRIVAL: usize = 0x8000;
const DBT_DEVICEREMOVECOMPLETE: usize = 0x8004;
const DBT_DEVTYP_VOLUME: u32 = 2;

/// DEV_BROADCAST_VOLUME (dbt.h).
#[repr(C)]
struct DevBroadcastVolume {
    size: u32,
    device_type: u32,
    reserved: u32,
    unit_mask: u32,
    flags: u16,
}

/// `letter`: "Z:" (or "Z"). Runs on its own thread: a hung window must not stall the helper.
pub fn announce(letter: &str, added: bool) {
    let Some(c) = letter.chars().next().filter(|c| c.is_ascii_alphabetic()).map(|c| c.to_ascii_uppercase()) else { return };
    let _ = std::thread::Builder::new().name("nya-drive-notify".into()).spawn(move || {
        let dbv = DevBroadcastVolume {
            size: std::mem::size_of::<DevBroadcastVolume>() as u32,
            device_type: DBT_DEVTYP_VOLUME,
            reserved: 0,
            unit_mask: 1 << (c as u8 - b'A'),
            flags: 0,
        };
        let mut recipients = BSM_APPLICATIONS;
        let event = if added { DBT_DEVICEARRIVAL } else { DBT_DEVICEREMOVECOMPLETE };
        let path = HSTRING::from(format!("{c}:\\"));
        unsafe {
            BroadcastSystemMessageW(
                BSF_IGNORECURRENTTASK | BSF_FORCEIFHUNG | BSF_NOHANG,
                Some(&mut recipients),
                WM_DEVICECHANGE,
                WPARAM(event),
                LPARAM(&dbv as *const DevBroadcastVolume as isize),
            );
            SHChangeNotify(
                if added { SHCNE_DRIVEADD } else { SHCNE_DRIVEREMOVED },
                SHCNF_PATHW,
                Some(path.as_ptr() as *const _),
                None,
            );
        }
        tracing::info!("announced drive {c}: {}", if added { "arrival" } else { "removal" });
    });
}
