//! Local microphone → host: WASAPI capture of the default recording device,
//! Opus in 10 ms packets, sent as MIC datagrams.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nya_media::audio::OpusEncoder;
use nya_proto::frame::{datagram_type, AudioPacket};
use nya_win::audio::LoopbackCapture;
use tokio::sync::mpsc::UnboundedSender;

use crate::events::NetCmd;

/// Runs until `stop` is set.
pub fn spawn(net: UnboundedSender<NetCmd>, stop: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("nya-mic".into())
        .spawn(move || {
            nya_win::com_init();
            nya_win::mmcss_boost("Pro Audio");
            let mut cap = match LoopbackCapture::microphone() {
                Ok(c) => c,
                Err(e) => return tracing::warn!("microphone unavailable: {e:#}"),
            };
            let mut enc = match OpusEncoder::new(64_000) {
                Ok(e) => e,
                Err(e) => return tracing::error!("mic opus encoder: {e:#}"),
            };
            tracing::info!("microphone on");
            let (mut pcm, mut packets, mut seq) = (Vec::new(), Vec::new(), 0u32);
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
                if let Err(e) = cap.read(&mut pcm) {
                    tracing::warn!("microphone capture: {e:#}");
                    std::thread::sleep(Duration::from_millis(500));
                    match LoopbackCapture::microphone() {
                        Ok(c) => cap = c,
                        Err(_) => continue,
                    }
                }
                let chunk = enc.frame_size * 2;
                while pcm.len() >= chunk {
                    let frame: Vec<f32> = pcm.drain(..chunk).collect();
                    let _ = enc.encode(&frame, &mut packets);
                    for data in packets.drain(..) {
                        seq = seq.wrapping_add(1);
                        let d = AudioPacket { seq, capture_ts_us: nya_proto::now_us(), data }.encode_as(datagram_type::MIC);
                        if net.send(NetCmd::Mic(d)).is_err() {
                            return;
                        }
                    }
                }
            }
            tracing::info!("microphone off");
        })
        .expect("spawn mic thread");
}
