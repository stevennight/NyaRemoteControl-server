//! Audio playback: Opus decode → adaptive jitter buffer → WASAPI.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use nya_media::audio::OpusDecoder;
use nya_media::jitter::JitterBuffer;
use nya_proto::frame::AudioPacket;
use nya_win::audio::AudioRenderer;

use crate::stats::Shared;

/// Keep roughly this much queued in the device.
const DEVICE_TARGET_MS: usize = 20;

pub fn spawn(rx: Receiver<AudioPacket>, stats: Arc<Shared>) {
    std::thread::Builder::new()
        .name("nya-audio".into())
        .spawn(move || run(rx, stats))
        .expect("spawn audio thread");
}

fn run(rx: Receiver<AudioPacket>, stats: Arc<Shared>) {
    nya_win::com_init();
    nya_win::mmcss_boost("Pro Audio");
    let mut decoder = match OpusDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("opus decoder: {e:#}");
            return;
        }
    };
    let mut renderer: Option<AudioRenderer> = None;
    let mut retry_at = Instant::now();
    let mut jb = JitterBuffer::new();
    let mut chunk = Vec::new();
    let epoch = Instant::now();
    let now_us = || epoch.elapsed().as_micros() as u64;
    let mut published = Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_millis(3)) {
            Ok(p) => {
                for p in std::iter::once(p).chain(rx.try_iter()) {
                    jb.push(p.seq, p.capture_ts_us, now_us(), |out| {
                        if let Err(e) = decoder.decode(&p.data, out) {
                            tracing::debug!("opus decode: {e:#}");
                        }
                    });
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if published.elapsed() >= Duration::from_millis(500) {
            published = Instant::now();
            let s = jb.stats();
            stats.with(|st| st.audio = Some(s));
        }

        if renderer.is_none() {
            if Instant::now() < retry_at {
                jb.reset(); // nowhere to play: don't pile up audio
                continue;
            }
            match AudioRenderer::new() {
                Ok(r) => renderer = Some(r),
                Err(e) => {
                    tracing::warn!("audio output unavailable: {e:#}");
                    retry_at = Instant::now() + Duration::from_secs(3);
                    continue;
                }
            }
        }
        let r = renderer.as_mut().unwrap();
        let queued = match r.queued_frames() {
            Ok(q) => q as usize,
            Err(_) => {
                renderer = None; // device changed
                continue;
            }
        };
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
