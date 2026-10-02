//! Virtual Xbox 360 controllers through ViGEmBus (optional component).
//! Each client pad index gets its own virtual pad, created on first use.
//! Force feedback from games is sent back as `GamepadRumble`.

use nya_proto::pb;

use super::Sink;

#[cfg(vigem)]
mod ffi {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct XusbReport {
        pub buttons: u16,
        pub left_trigger: u8,
        pub right_trigger: u8,
        pub lx: i16,
        pub ly: i16,
        pub rx: i16,
        pub ry: i16,
    }

    pub type Notification = unsafe extern "system" fn(
        client: *mut c_void,
        target: *mut c_void,
        large: u8,
        small: u8,
        led: u8,
        user: *mut c_void,
    );

    pub const VIGEM_ERROR_NONE: u32 = 0x2000_0000;

    extern "C" {
        pub fn vigem_alloc() -> *mut c_void;
        pub fn vigem_free(c: *mut c_void);
        pub fn vigem_connect(c: *mut c_void) -> u32;
        pub fn vigem_disconnect(c: *mut c_void);
        pub fn vigem_target_x360_alloc() -> *mut c_void;
        pub fn vigem_target_free(t: *mut c_void);
        pub fn vigem_target_add(c: *mut c_void, t: *mut c_void) -> u32;
        pub fn vigem_target_remove(c: *mut c_void, t: *mut c_void) -> u32;
        pub fn vigem_target_x360_update(c: *mut c_void, t: *mut c_void, r: XusbReport) -> u32;
        pub fn vigem_target_x360_register_notification(
            c: *mut c_void,
            t: *mut c_void,
            n: Notification,
            user: *mut c_void,
        ) -> u32;
        pub fn vigem_target_x360_unregister_notification(t: *mut c_void);
    }
}

/// Can virtual pads be created (ViGEmBus installed and reachable)?
pub fn available() -> bool {
    #[cfg(vigem)]
    unsafe {
        let c = ffi::vigem_alloc();
        if c.is_null() {
            return false;
        }
        let ok = ffi::vigem_connect(c) == ffi::VIGEM_ERROR_NONE;
        if ok {
            ffi::vigem_disconnect(c);
        }
        ffi::vigem_free(c);
        ok
    }
    #[cfg(not(vigem))]
    false
}

#[cfg(vigem)]
struct Pad {
    target: *mut std::ffi::c_void,
    /// Boxed so the pointer handed to the rumble callback stays valid.
    _ctx: Box<(Sink, u32)>,
}

pub struct Pads {
    #[cfg(vigem)]
    client: *mut std::ffi::c_void,
    #[cfg(vigem)]
    pads: [Option<Pad>; 4],
    sink: Sink,
    failed: bool,
}

// Used only by the input thread; ViGEm handles are thread-safe to free there.
unsafe impl Send for Pads {}

#[cfg(vigem)]
unsafe extern "system" fn on_rumble(
    _c: *mut std::ffi::c_void,
    _t: *mut std::ffi::c_void,
    large: u8,
    small: u8,
    _led: u8,
    user: *mut std::ffi::c_void,
) {
    let ctx = &*(user as *const (Sink, u32));
    ctx.0.try_send(crate::ipc_pb::host_event::Ev::GamepadRumble(pb::GamepadRumble {
        index: ctx.1,
        large_motor: large as u32,
        small_motor: small as u32,
    }));
}

impl Pads {
    pub fn new(sink: Sink) -> Self {
        Self {
            #[cfg(vigem)]
            client: std::ptr::null_mut(),
            #[cfg(vigem)]
            pads: Default::default(),
            sink,
            failed: false,
        }
    }

    #[cfg(vigem)]
    fn client(&mut self) -> Option<*mut std::ffi::c_void> {
        if self.client.is_null() && !self.failed {
            unsafe {
                let c = ffi::vigem_alloc();
                if !c.is_null() && ffi::vigem_connect(c) == ffi::VIGEM_ERROR_NONE {
                    self.client = c;
                } else {
                    if !c.is_null() {
                        ffi::vigem_free(c);
                    }
                    tracing::warn!("gamepad input received but ViGEmBus is not installed");
                    self.failed = true;
                }
            }
        }
        (!self.client.is_null()).then_some(self.client)
    }

    pub fn update(&mut self, g: &pb::Gamepad) {
        #[cfg(vigem)]
        {
            let idx = g.index as usize;
            if idx >= 4 {
                return;
            }
            if !g.connected {
                self.remove(idx);
                return;
            }
            let Some(c) = self.client() else { return };
            if self.pads[idx].is_none() {
                unsafe {
                    let t = ffi::vigem_target_x360_alloc();
                    if t.is_null() || ffi::vigem_target_add(c, t) != ffi::VIGEM_ERROR_NONE {
                        if !t.is_null() {
                            ffi::vigem_target_free(t);
                        }
                        tracing::warn!("cannot plug in virtual gamepad {idx}");
                        return;
                    }
                    let ctx = Box::new((self.sink.clone(), idx as u32));
                    ffi::vigem_target_x360_register_notification(c, t, on_rumble, &*ctx as *const _ as *mut _);
                    tracing::info!("virtual gamepad {idx} plugged in");
                    self.pads[idx] = Some(Pad { target: t, _ctx: ctx });
                }
            }
            let t = self.pads[idx].as_ref().unwrap().target;
            let r = ffi::XusbReport {
                buttons: g.buttons as u16,
                left_trigger: g.left_trigger.min(255) as u8,
                right_trigger: g.right_trigger.min(255) as u8,
                lx: g.lx.clamp(-32768, 32767) as i16,
                ly: g.ly.clamp(-32768, 32767) as i16,
                rx: g.rx.clamp(-32768, 32767) as i16,
                ry: g.ry.clamp(-32768, 32767) as i16,
            };
            unsafe {
                ffi::vigem_target_x360_update(c, t, r);
            }
        }
        #[cfg(not(vigem))]
        {
            let _ = g;
            if !self.failed {
                tracing::warn!("gamepad input received but this build has no gamepad support");
                self.failed = true;
            }
        }
    }

    #[cfg(vigem)]
    fn remove(&mut self, idx: usize) {
        if let Some(p) = self.pads[idx].take() {
            unsafe {
                ffi::vigem_target_x360_unregister_notification(p.target);
                ffi::vigem_target_remove(self.client, p.target);
                ffi::vigem_target_free(p.target);
            }
            tracing::info!("virtual gamepad {idx} unplugged");
        }
    }

    /// Unplug every virtual pad (client gone).
    pub fn remove_all(&mut self) {
        #[cfg(vigem)]
        for i in 0..4 {
            self.remove(i);
        }
    }
}

impl Drop for Pads {
    fn drop(&mut self) {
        self.remove_all();
        #[cfg(vigem)]
        if !self.client.is_null() {
            unsafe {
                ffi::vigem_disconnect(self.client);
                ffi::vigem_free(self.client);
            }
        }
    }
}
