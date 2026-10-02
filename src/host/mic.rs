//! Client microphone → host: Opus datagrams are decoded and played into the
//! input side of a virtual audio cable (VB-Cable "CABLE Input"). While that
//! happens, "CABLE Output" is made the host's default recording device, so
//! applications using "the default microphone" hear the client; the previous
//! default comes back when the client's microphone stops (also after a crash:
//! the previous devices are kept in a file until restored).

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use nya_media::audio::OpusDecoder;
use nya_media::jitter::JitterBuffer;
use nya_proto::frame::AudioPacket;
use nya_win::audio::{find_render_device, AudioRenderer};
pub use nya_server_core::components::{cable_device_name, CABLE_NAMES};

/// Keep roughly this much queued in the cable.
const DEVICE_TARGET_MS: usize = 30;

/// The audio thread captures the default device's plain loopback (no
/// process exclusion). If that device is the cable itself, playing the
/// microphone into it would send it straight back to the client as echo.
pub static PLAIN_LOOPBACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn would_echo() -> bool {
    PLAIN_LOOPBACK.load(std::sync::atomic::Ordering::Relaxed) && CABLE_NAMES.iter().any(|n| nya_win::audio::default_render_is(n))
}

/// Recording side of the virtual cable.
const CABLE_CAPTURE_NAMES: [&str; 2] = ["CABLE Output", "VB-Audio Virtual Cable"];

fn default_mic_file() -> std::path::PathBuf {
    crate::paths::service_dir().join("mic-default.txt")
}

/// Make the cable's recording side the default microphone (all roles),
/// remembering the previous defaults.
fn take_default_mic() {
    use nya_win::audio::{default_capture_id, find_capture_device, set_default_endpoint};
    use windows::Win32::Media::Audio::{eCommunications, eConsole, eMultimedia};
    let Some((cable, name)) = CABLE_CAPTURE_NAMES.iter().find_map(|n| find_capture_device(n)) else { return };
    let console = default_capture_id(eConsole).unwrap_or_default();
    let comms = default_capture_id(eCommunications).unwrap_or_default();
    if console == cable && comms == cable {
        return;
    }
    let file = default_mic_file();
    // A file left by a crash still holds the user's real devices: keep it.
    if !file.exists() {
        if let Err(e) = std::fs::write(&file, format!("{console}\n{comms}\n")) {
            tracing::warn!("remember default microphone: {e}");
        }
    }
    for role in [eConsole, eMultimedia, eCommunications] {
        if let Err(e) = set_default_endpoint(&cable, role) {
            tracing::warn!("default microphone -> {name}: {e:#}");
            return;
        }
    }
    tracing::info!("default microphone -> {name} while the client's microphone is on");
}

/// Put back the default microphone saved by `take_default_mic`.
pub fn restore_default_mic() {
    use nya_win::audio::set_default_endpoint;
    use windows::Win32::Media::Audio::{eCommunications, eConsole, eMultimedia};
    let file = default_mic_file();
    let Ok(text) = std::fs::read_to_string(&file) else { return };
    let mut lines = text.lines();
    let console = lines.next().unwrap_or_default().trim().to_string();
    let comms = lines.next().unwrap_or_default().trim().to_string();
    let mut failed = false;
    for (id, roles) in [(&console, &[eConsole, eMultimedia][..]), (&comms, &[eCommunications][..])] {
        if id.is_empty() {
            continue;
        }
        for &role in roles {
            if let Err(e) = set_default_endpoint(id, role) {
                // The device may be gone (unplugged): nothing to restore then.
                tracing::warn!("restore default microphone: {e:#}");
                failed = true;
            }
        }
    }
    let _ = std::fs::remove_file(&file);
    if !failed {
        tracing::info!("default microphone restored");
    }

}

pub fn thread(rx: Receiver<Vec<u8>>) {
    nya_win::com_init();
    nya_win::mmcss_boost("Pro Audio");
    // Left over from a helper that ended while the microphone was on.
    restore_default_mic();
    let mut decoder = match OpusDecoder::new() {
        Ok(d) => d,
        Err(e) => return tracing::error!("mic opus decoder: {e:#}"),
    };
    let mut renderer: Option<AudioRenderer> = None;
    let mut retry_at = Instant::now();
    let mut jb = JitterBuffer::new();
    let mut chunk = Vec::new();
    let epoch = Instant::now();
    let now_us = || epoch.elapsed().as_micros() as u64;
    let mut warned = false;
    let mut last_packet = Instant::now() - Duration::from_secs(10);

    loop {
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(d) => {
                last_packet = Instant::now();
                for d in std::iter::once(d).chain(rx.try_iter()) {
                    if let Some(p) = AudioPacket::decode_any(&d) {
                        jb.push(p.seq, p.capture_ts_us, now_us(), |out| {
                            let _ = decoder.decode(&p.data, out);
                        });
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if renderer.is_some() {
                    restore_default_mic();
                }
                return;
            }
        }
        // Idle: release the device so other apps see a quiet cable.
        if last_packet.elapsed() > Duration::from_secs(3) {
            if renderer.take().is_some() {
                restore_default_mic();
                let s = jb.stats();
                tracing::info!(
                    "microphone idle; buffer target {:.0} ms, jitter {:.0} ms, underruns {}, dropped {} ms, concealed {} ms",
                    s.target_ms,
                    s.jitter_ms,
                    s.underruns,
                    s.dropped_ms,
                    s.concealed_ms
                );
            }
            jb.reset();
            continue;
        }
        if renderer.is_none() && Instant::now() >= retry_at && would_echo() {
            tracing::warn!("default playback device is the virtual cable: microphone muted to avoid echo; set the speakers as default playback device");
            retry_at = Instant::now() + Duration::from_secs(10);
        }
        if renderer.is_none() && Instant::now() >= retry_at {
            let dev = super::mic::CABLE_NAMES.iter().find_map(|n| find_render_device(n));
            match dev {
                Some((d, name)) => match AudioRenderer::on_device(&d) {
                    Ok(r) => {
                        tracing::info!("microphone -> {name}");
                        renderer = Some(r);
                        warned = false;
                        take_default_mic();
                    }

                    Err(e) => {
                        tracing::warn!("open {name}: {e:#}");
                        retry_at = Instant::now() + Duration::from_secs(3);
                    }
                },
                None => {
                    if !warned {
                        tracing::warn!("microphone data received but no virtual cable (VB-Cable) is installed");
                        warned = true;
                    }
                    retry_at = Instant::now() + Duration::from_secs(5);
                }
            }
        }
        let Some(r) = renderer.as_mut() else {
            jb.reset(); // nowhere to play: don't pile up audio
            continue;
        };
        let queued = match r.queued_frames() {
            Ok(q) => q,
            Err(_) => {
                renderer = None;
                continue;
            }
        };
        let queued = queued as usize;
        let target = DEVICE_TARGET_MS * 48;
        if queued >= target {
            continue;
        }
        chunk.clear();
        if jb.pull(now_us(), target - queued, queued, &mut chunk) == 0 {
            continue;
        }
        // The device buffer is larger than the target, so this all fits.
        if r.write(&chunk).is_err() {
            renderer = None;
        }
    }
}
