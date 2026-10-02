//! Local XInput gamepads → host virtual Xbox pads (host needs ViGEmBus).
//! Polls at 250 Hz, sends a `Gamepad` input message whenever a pad's state
//! changes. While the window is not focused the host sees neutral pads, so a
//! game does not keep running with a held stick. Force feedback from the host
//! is applied locally.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nya_proto::pb::{self, input_msg::Ev};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::UI::Input::XboxController::{XInputGetState, XInputSetState, XINPUT_STATE, XINPUT_VIBRATION};

use crate::events::NetCmd;

const PADS: usize = 4;
const ERROR_SUCCESS: u32 = 0;

#[derive(Default)]
pub struct Shared {
    stop: AtomicBool,
    /// Window focused: forward real states.
    pub active: AtomicBool,
    /// Bit i set: local pad i is connected.
    pub connected: AtomicU32,
    rumble: Mutex<[Option<(u16, u16)>; PADS]>,
}

pub struct Gamepads(Arc<Shared>);

impl Gamepads {
    pub fn spawn(net: UnboundedSender<NetCmd>, active: bool) -> Self {
        let shared = Arc::new(Shared::default());
        shared.active.store(active, Ordering::Relaxed);
        let s = shared.clone();
        std::thread::Builder::new()
            .name("nya-gamepad".into())
            .spawn(move || run(net, s))
            .expect("spawn gamepad thread");
        Self(shared)
    }

    pub fn set_active(&self, on: bool) {
        self.0.active.store(on, Ordering::Relaxed);
    }

    /// Number of local pads connected.
    pub fn count(&self) -> u32 {
        self.0.connected.load(Ordering::Relaxed).count_ones()
    }

    pub fn rumble(&self, r: &pb::GamepadRumble) {
        if let Some(slot) = self.0.rumble.lock().unwrap().get_mut(r.index as usize) {
            // XInput motors are 16-bit; the host reports 8-bit values.
            *slot = Some(((r.large_motor.min(255) as u16) * 257, (r.small_motor.min(255) as u16) * 257));
        }
    }
}

impl Drop for Gamepads {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Relaxed);
    }
}

fn to_msg(index: u32, st: Option<&XINPUT_STATE>, neutral: bool) -> pb::Gamepad {
    match st {
        Some(st) if !neutral => {
            let g = &st.Gamepad;
            pb::Gamepad {
                index,
                connected: true,
                buttons: g.wButtons.0 as u32,
                left_trigger: g.bLeftTrigger as u32,
                right_trigger: g.bRightTrigger as u32,
                lx: g.sThumbLX as i32,
                ly: g.sThumbLY as i32,
                rx: g.sThumbRX as i32,
                ry: g.sThumbRY as i32,
            }
        }
        Some(_) => pb::Gamepad { index, connected: true, ..Default::default() },
        None => pb::Gamepad { index, connected: false, ..Default::default() },
    }
}

fn set_vibration(i: usize, large: u16, small: u16) {
    let v = XINPUT_VIBRATION { wLeftMotorSpeed: large, wRightMotorSpeed: small };
    unsafe {
        XInputSetState(i as u32, &v);
    }
}

fn run(net: UnboundedSender<NetCmd>, s: Arc<Shared>) {
    let mut last: [Option<pb::Gamepad>; PADS] = Default::default();
    let mut vibrating = [false; PADS];
    // Checking absent pads is slow in XInput; do it only every ~second.
    let mut tick = 0u32;
    while !s.stop.load(Ordering::Relaxed) {
        let active = s.active.load(Ordering::Relaxed);
        let rumble = std::mem::take(&mut *s.rumble.lock().unwrap());
        for i in 0..PADS {
            let present = last[i].as_ref().is_some_and(|g| g.connected);
            if !present && tick % 250 != 0 {
                continue;
            }
            let mut st = XINPUT_STATE::default();
            let ok = unsafe { XInputGetState(i as u32, &mut st) } == ERROR_SUCCESS;
            let msg = to_msg(i as u32, ok.then_some(&st), !active);
            if ok {
                s.connected.fetch_or(1 << i, Ordering::Relaxed);
            } else {
                s.connected.fetch_and(!(1 << i), Ordering::Relaxed);
            }
            if let Some((large, small)) = rumble[i] {
                if ok {
                    set_vibration(i, large, small);
                    vibrating[i] = large != 0 || small != 0;
                }
            }
            if !active && vibrating[i] {
                set_vibration(i, 0, 0);
                vibrating[i] = false;
            }
            // Nothing to tell the host about a pad that was never plugged in.
            let changed = match &last[i] {
                Some(prev) => *prev != msg,
                None => msg.connected,
            };
            if changed {
                if msg.connected != present {
                    tracing::info!("gamepad {i} {}", if msg.connected { "connected" } else { "disconnected" });
                }
                let _ = net.send(NetCmd::Input(pb::InputMsg { ev: Some(Ev::Gamepad(msg.clone())) }));
                last[i] = Some(msg);
            }
        }
        tick = tick.wrapping_add(1);
        std::thread::sleep(Duration::from_millis(4));
    }
    for (i, v) in vibrating.iter().enumerate() {
        if *v {
            set_vibration(i, 0, 0);
        }
    }
}
