use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use nya_proto::pb::{self, input_msg::Ev};
use nya_win::desktop::DesktopTracker;
use nya_win::input::{Button, DisplayRect, Injector};

pub enum InputCmd {
    Event(pb::InputMsg),
    SetRect(DisplayRect),
    ReleaseAll,
    Shutdown,
}

fn button(b: i32) -> Option<Button> {
    Some(match pb::MouseButton::try_from(b).ok()? {
        pb::MouseButton::Left => Button::Left,
        pb::MouseButton::Right => Button::Right,
        pb::MouseButton::Middle => Button::Middle,
        pb::MouseButton::X1 => Button::X1,
        pb::MouseButton::X2 => Button::X2,
        pb::MouseButton::Unspecified => return None,
    })
}

pub fn thread(rx: Receiver<InputCmd>) {
    let mut inj = Injector::new();
    let mut desktop = DesktopTracker::new();
    let mut last_sync = Instant::now() - Duration::from_secs(1);
    let (mut events, mut since) = (0u64, Instant::now());
    for cmd in rx {
        if matches!(cmd, InputCmd::Event(_)) {
            events += 1;
        }
        if events > 0 && since.elapsed() >= Duration::from_secs(5) {
            tracing::info!(
                "input (5 s): {events} events, desktop {}, SendInput failures so far {}",
                desktop.name(),
                nya_win::input::send_failures()
            );
            (events, since) = (0, Instant::now());
        }
        // Follow the input desktop so injection reaches the lock screen / UAC prompt.
        if last_sync.elapsed() > Duration::from_millis(100) {
            if let Err(e) = desktop.sync() {
                tracing::debug!("input desktop sync: {e:#}");
            }
            last_sync = Instant::now();
        }
        match cmd {
            InputCmd::Event(pb::InputMsg { ev: Some(ev) }) => match ev {
                Ev::MouseAbs(m) => inj.mouse_abs(m.x, m.y),
                Ev::MouseRel(m) => inj.mouse_rel(m.dx, m.dy),
                Ev::MouseButton(b) => {
                    if let Some(btn) = button(b.button) {
                        inj.button(btn, b.down);
                    }
                }
                Ev::Wheel(w) => inj.wheel(w.dx, w.dy),
                Ev::Key(k) => inj.key(k.scancode as u16, k.extended, k.down),
                Ev::ReleaseAll(_) => inj.release_all(),
            },
            InputCmd::Event(_) => {}
            InputCmd::SetRect(r) => inj.set_display_rect(r),
            InputCmd::ReleaseAll => {
                if inj.pressed_count() > 0 {
                    tracing::info!("releasing {} stuck keys/buttons", inj.pressed_count());
                }
                inj.release_all();
            }
            InputCmd::Shutdown => break,
        }
    }
    inj.release_all();
}
