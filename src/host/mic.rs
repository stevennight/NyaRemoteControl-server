//! Client microphone → host: Opus datagrams are decoded and played into the
//! input side of a virtual audio cable (VB-Cable "CABLE Input"). Applications
//! on the host then use "CABLE Output" as their microphone.

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

pub fn thread(rx: Receiver<Vec<u8>>) {
    nya_win::com_init();
    nya_win::mmcss_boost("Pro Audio");
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
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Idle: release the device so other apps see a quiet cable.
        if last_packet.elapsed() > Duration::from_secs(3) {
            if renderer.take().is_some() {
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
