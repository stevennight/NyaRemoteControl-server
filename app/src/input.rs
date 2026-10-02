//! Keyboard capture.
//!
//! * A low-level hook forwards keys (including Win / Alt+Tab combinations)
//!   while the window is focused and the keyboard is grabbed.
//! * Some environments (cloud desktops) never deliver keys to low-level hooks;
//!   there keys reach the window and are forwarded from window events. To keep
//!   shell hotkeys such as Win+D / Win+E from acting locally, raw keyboard input
//!   is registered with `RIDEV_NOHOTKEYS` while grabbed.
//!
//! Hotkeys are Ctrl+Alt+Shift+<key>.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use nya_proto::pb::{self, input_msg::Ev};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC_EX};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, SetWindowsHookExW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP,
    WH_KEYBOARD_LL,
};

use crate::events::{Hotkey, NetCmd, Ui, UiEvent};

struct HookState {
    hwnd: AtomicIsize,
    /// Extra session windows (other host displays); keys go to the host from those too.
    extra: Mutex<Vec<isize>>,
    grab: AtomicBool,
    mods: AtomicU8,
    tx: Mutex<Option<UnboundedSender<NetCmd>>>,
    ui: Mutex<Option<Ui>>,
}

static STATE: OnceLock<HookState> = OnceLock::new();
static HOOK_KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HOOK_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Hook invocations for any window (tells "hook not running" from "not our window").
pub fn hook_call_count() -> u64 {
    HOOK_CALLS.load(Ordering::Relaxed)
}

/// Forward keys only while a session is running (not in the launcher UI).
pub fn set_active(on: bool) {
    ACTIVE.store(on, Ordering::SeqCst);
}

/// Suppress application-defined hotkeys (Win+D, Win+E, …) for our process.
pub fn set_no_hotkeys(hwnd: HWND, on: bool) {
    use windows::Win32::UI::Input::{RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_NOHOTKEYS, RAWINPUTDEVICE_FLAGS};
    let dev = RAWINPUTDEVICE {
        usUsagePage: 0x01, // generic desktop
        usUsage: 0x06,     // keyboard
        dwFlags: if on { RIDEV_NOHOTKEYS } else { RAWINPUTDEVICE_FLAGS(0) },
        hwndTarget: hwnd,
    };
    unsafe {
        if let Err(e) = RegisterRawInputDevices(&[dev], std::mem::size_of::<RAWINPUTDEVICE>() as u32) {
            tracing::debug!("RegisterRawInputDevices(no_hotkeys={on}): {e}");
        }
    }
}

/// Key events our window received through the hook since start.
pub fn hook_key_count() -> u64 {
    HOOK_KEYS.load(Ordering::Relaxed)
}

const CTRL: u8 = 1;
const ALT: u8 = 2;
const SHIFT: u8 = 4;

fn state() -> &'static HookState {
    STATE.get_or_init(|| HookState {
        hwnd: AtomicIsize::new(0),
        extra: Mutex::new(Vec::new()),
        grab: AtomicBool::new(true),
        mods: AtomicU8::new(0),
        tx: Mutex::new(None),
        ui: Mutex::new(None),
    })
}

/// Install the hook once per process.
pub fn install(hwnd: HWND, ui: Ui) {
    let s = state();
    s.hwnd.store(hwnd.0 as isize, Ordering::SeqCst);
    *s.ui.lock().unwrap() = Some(ui);
    unsafe {
        let module = GetModuleHandleW(None).unwrap_or_default();
        if let Err(e) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module, 0) {
            tracing::error!("keyboard hook: {e}");
        }
    }
}

/// An extra session window opened / closed: it counts as ours for the hook.
pub fn set_extra_window(hwnd: HWND, open: bool) {
    let mut v = state().extra.lock().unwrap();
    v.retain(|h| *h != hwnd.0 as isize);
    if open {
        v.push(hwnd.0 as isize);
    }
}

/// Where forwarded keys go; `None` outside a session. Also toggles forwarding.
pub fn set_session(tx: Option<UnboundedSender<NetCmd>>) {
    set_active(tx.is_some());
    *state().tx.lock().unwrap() = tx;
    reset_modifiers();
}

/// A session ended: stop forwarding if keys were going to it (another
/// session's window may have the keyboard by now).
pub fn end_session(tx: &UnboundedSender<NetCmd>) {
    let ours = state().tx.lock().unwrap().as_ref().is_some_and(|t| t.same_channel(tx));
    if ours {
        set_session(None);
    }
}

pub fn set_grab(on: bool) {
    state().grab.store(on, Ordering::SeqCst);
}

pub fn grabbed() -> bool {
    state().grab.load(Ordering::SeqCst)
}

fn hotkey_for(vk: u32) -> Option<Hotkey> {
    Some(match vk {
        0x51 => Hotkey::ToggleGrab,       // Q
        0x53 => Hotkey::ToggleStats,      // S
        0x4D => Hotkey::ToggleMode,       // M
        0x52 => Hotkey::ToggleRelative,   // R
        0x46 => Hotkey::ToggleFullscreen, // F
        0x44 => Hotkey::CtrlAltDel,       // D
        0x58 => Hotkey::Quit,             // X
        0x31..=0x39 => Hotkey::Display((vk - 0x30) as u8),
        _ => return None,
    })
}

fn modifier_bit(vk: u32) -> u8 {
    match vk {
        0x10 | 0xA0 | 0xA1 => SHIFT,
        0x11 | 0xA2 | 0xA3 => CTRL,
        0x12 | 0xA4 | 0xA5 => ALT,
        _ => 0,
    }
}

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
        let s = state();
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let fg = GetForegroundWindow().0 as isize;
        let ours = fg == s.hwnd.load(Ordering::Relaxed) || s.extra.try_lock().is_ok_and(|v| v.contains(&fg));
        // Injected keys are processed too: in cloud desktops / remote sessions
        // every keystroke arrives injected. We never inject locally, so no loop.
        if ours && ACTIVE.load(Ordering::Relaxed) {
            if HOOK_KEYS.fetch_add(1, Ordering::Relaxed) == 0 {
                tracing::info!("keyboard hook: first key vk={:#x} scan={:#x} flags={:#x}", kb.vkCode, kb.scanCode, kb.flags.0);
            }
            let down = kb.flags.0 & LLKHF_UP.0 == 0;
            let bit = modifier_bit(kb.vkCode);
            if bit != 0 {
                if down {
                    s.mods.fetch_or(bit, Ordering::Relaxed);
                } else {
                    s.mods.fetch_and(!bit, Ordering::Relaxed);
                }
            }
            if down && s.mods.load(Ordering::Relaxed) == CTRL | ALT | SHIFT {
                if let Some(h) = hotkey_for(kb.vkCode) {
                    if let Some(ui) = s.ui.lock().unwrap().as_ref() {
                        ui.send(UiEvent::Hotkey(h));
                    }
                    return LRESULT(1);
                }
            }
            if s.grab.load(Ordering::Relaxed) {
                let mut sc = kb.scanCode;
                let mut ext = kb.flags.0 & LLKHF_EXTENDED.0 != 0;
                if sc == 0 {
                    let v = MapVirtualKeyW(kb.vkCode, MAPVK_VK_TO_VSC_EX);
                    sc = v & 0xff;
                    ext = v & 0xff00 == 0xe000;
                }
                if sc != 0 {
                    if let Some(tx) = s.tx.lock().unwrap().as_ref() {
                        let _ = tx.send(NetCmd::Input(pb::InputMsg {
                            ev: Some(Ev::Key(pb::Key { scancode: sc, extended: ext, down })),
                        }));
                    }
                }
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Forget modifier state (focus changes).
pub fn reset_modifiers() {
    state().mods.store(0, Ordering::Relaxed);
}
