//! Client microphone → host: Opus datagrams are decoded and played into the
//! input side of a virtual audio cable (VB-Cable "CABLE Input"). Applications
//! on the host then use "CABLE Output" as their microphone.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use nya_media::audio::OpusDecoder;
use nya_proto::frame::AudioPacket;
use nya_win::audio::{find_render_device, AudioRenderer};

/// Playback device names that belong to a virtual cable (first match wins).
pub const CABLE_NAMES: [&str; 2] = ["CABLE Input", "VB-Audio Virtual Cable"];

pub fn cable_device_name() -> Option<String> {
    CABLE_NAMES.iter().find_map(|n| find_render_device(n).map(|(_, name)| name))
}

const SAMPLES_PER_MS: usize = 48 * 2;

pub fn thread(rx: Receiver<Vec<u8>>) {
    nya_win::com_init();
    nya_win::mmcss_boost("Pro Audio");
    let mut decoder = match OpusDecoder::new() {
        Ok(d) => d,
        Err(e) => return tracing::error!("mic opus decoder: {e:#}"),
    };
    let mut renderer: Option<AudioRenderer> = None;
    let mut retry_at = Instant::now();
    let mut buf: VecDeque<f32> = VecDeque::new();
    let mut scratch = Vec::new();
    let mut warned = false;
    let mut last_packet = Instant::now() - Duration::from_secs(10);

    loop {
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(d) => {
                last_packet = Instant::now();
                for d in std::iter::once(d).chain(rx.try_iter()) {
                    if let Some(p) = AudioPacket::decode_any(&d) {
                        scratch.clear();
                        if decoder.decode(&p.data, &mut scratch).is_ok() {
                            buf.extend(scratch.iter().copied());
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Idle: release the device so other apps see a quiet cable.
        if last_packet.elapsed() > Duration::from_secs(3) {
            renderer = None;
            buf.clear();
            continue;
        }
        // Keep latency low: at most 120 ms queued.
        if buf.len() > 120 * SAMPLES_PER_MS {
            let excess = buf.len() - 40 * SAMPLES_PER_MS;
            buf.drain(..excess - excess % 2);
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
        let Some(r) = renderer.as_mut() else { continue };
        let queued = match r.queued_frames() {
            Ok(q) => q,
            Err(_) => {
                renderer = None;
                continue;
            }
        };
        let target = 30 * 48;
        if queued >= target || buf.is_empty() {
            continue;
        }
        let want = ((target - queued) as usize * 2).min(buf.len());
        let chunk: Vec<f32> = buf.drain(..want - want % 2).collect();
        match r.write(&chunk) {
            Ok(n) => {
                for &s in chunk[n * 2..].iter().rev() {
                    buf.push_front(s);
                }
            }
            Err(_) => renderer = None,
        }
    }
}
