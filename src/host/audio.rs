//! System audio: WASAPI loopback → Opus (10 ms) → datagrams.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use nya_media::audio::OpusEncoder;
use nya_proto::frame::AudioPacket;
use nya_win::audio::LoopbackCapture;

use super::Sink;
use crate::ipc_pb::{host_event::Ev, AudioDatagram};

/// Drop the oldest audio if more than this is waiting (keeps latency bounded).
const MAX_BACKLOG_SAMPLES: usize = 48_000 / 10 * 2; // 100 ms stereo

pub fn thread(rx: Receiver<bool>, sink: Sink) {
    nya_win::com_init();
    nya_win::mmcss_boost("Pro Audio");
    let mut enabled = false;
    let mut capture: Option<LoopbackCapture> = None;
    let mut encoder: Option<OpusEncoder> = None;
    let mut pcm: Vec<f32> = Vec::new();
    let mut packets = Vec::new();
    let mut seq = 0u32;
    let mut retry_at = Instant::now();
    let mut warned = false;

    loop {
        let msg = if enabled {
            rx.recv_timeout(Duration::from_millis(5))
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match msg {
            Ok(on) => {
                if on != enabled {
                    tracing::info!("audio {}", if on { "on" } else { "off" });
                }
                enabled = on;
                if !enabled {
                    capture = None;
                    pcm.clear();
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        if capture.is_none() && Instant::now() >= retry_at {
            // Leave out our own playback (the microphone we feed into
            // VB-Cable) so the client never hears itself; older Windows falls
            // back to the plain loopback of the default device.
            let opened = LoopbackCapture::excluding_process(std::process::id())
                .map(|c| (c, false))
                .or_else(|e| {
                    tracing::info!("process loopback unavailable ({e:#}); using device loopback");
                    LoopbackCapture::new().map(|c| (c, true))
                });
            match opened {
                Ok((c, plain)) => {
                    super::mic::PLAIN_LOOPBACK.store(plain, std::sync::atomic::Ordering::Relaxed);
                    capture = Some(c);
                    warned = false;
                }
                Err(e) => {
                    if !warned {
                        tracing::warn!("audio loopback unavailable: {e:#}");
                        warned = true;
                    }
                    retry_at = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        if encoder.is_none() {
            match OpusEncoder::new(128_000) {
                Ok(e) => encoder = Some(e),
                Err(e) => {
                    tracing::error!("opus encoder: {e:#}");
                    enabled = false;
                    continue;
                }
            }
        }
        let Some(cap) = capture.as_mut() else { continue };
        if let Err(e) = cap.read(&mut pcm) {
            // Typically the default device changed (headphones plugged in).
            tracing::info!("audio capture restart: {e:#}");
            capture = None;
            retry_at = Instant::now() + Duration::from_millis(300);
            continue;
        }
        if pcm.len() > MAX_BACKLOG_SAMPLES {
            let excess = pcm.len() - MAX_BACKLOG_SAMPLES;
            pcm.drain(..excess - excess % 2);
        }
        let enc = encoder.as_mut().unwrap();
        let chunk = enc.frame_size * 2;
        while pcm.len() >= chunk {
            let frame: Vec<f32> = pcm.drain(..chunk).collect();
            if let Err(e) = enc.encode(&frame, &mut packets) {
                tracing::warn!("opus encode: {e:#}");
            }
            for data in packets.drain(..) {
                seq = seq.wrapping_add(1);
                let datagram = AudioPacket { seq, capture_ts_us: nya_proto::now_us(), data }.encode();
                sink.try_send(Ev::Audio(AudioDatagram { datagram }));
            }
        }
    }
}
