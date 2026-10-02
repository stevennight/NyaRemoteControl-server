//! Latency / throughput statistics shared between threads.

use std::sync::Mutex;

use nya_media::jitter::JitterStats;
use nya_proto::stats::Rolling;

#[derive(Default)]
pub struct Stats {
    pub rtt_ms: f32,
    /// server_clock - client_clock (µs), estimated from ping.
    pub clock_offset_us: i64,
    pub decode_ms: Vec<f32>,
    pub render_ms: Vec<f32>,
    pub latency_ms: Vec<f32>,
    pub frames_decoded: u32,
    pub frames_rendered: u32,
    pub frames_dropped: u32,
    pub bytes: u64,
    pub decoder: String,
    /// Audio jitter buffer (published by the audio thread).
    pub audio: Option<JitterStats>,
    /// Video datagrams of the interval (FEATURE_VIDEO_DATAGRAM).
    pub dgram: DgramTotals,
    /// Samples of the last 10 s, for the 99th percentiles.
    decode_window: Rolling,
    render_window: Rolling,
    latency_window: Rolling,
    /// Totals since start (never reset), for the "no picture" status log.
    pub total_rx_frames: u64,
    pub total_rx_bytes: u64,
    pub total_decoded: u64,
    pub total_rendered: u64,
}

/// Video datagram counters added up over a statistics interval.
#[derive(Default, Clone, Copy)]
pub struct DgramTotals {
    pub shards_received: u32,
    pub shards_lost: u32,
    pub frames_recovered: u32,
    pub frames_lost: u32,
}

impl DgramTotals {
    pub fn add(&mut self, s: &nya_transport::videodgram::DgramStats) {
        self.shards_received += s.shards_received;
        self.shards_lost += s.shards_lost;
        self.frames_recovered += s.frames_recovered;
        self.frames_lost += s.frames_lost;
    }

    /// Lost shards in percent.
    pub fn loss_pct(&self) -> f32 {
        let total = self.shards_received + self.shards_lost;
        if total == 0 { 0.0 } else { self.shards_lost as f32 * 100.0 / total as f32 }
    }
}

pub struct Shared(pub Mutex<Stats>);

impl Shared {
    pub fn new() -> Self {
        Self(Mutex::new(Stats::default()))
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Stats) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }

    pub fn on_pong(&self, sent_us: u64, server_us: u64) {
        let now = nya_proto::now_us();
        let rtt = now.saturating_sub(sent_us);
        let offset = server_us as i64 - (sent_us + rtt / 2) as i64;
        self.with(|s| {
            s.rtt_ms = rtt as f32 / 1000.0;
            // Smooth the offset; ping jitter would otherwise make latency noisy.
            s.clock_offset_us = if s.clock_offset_us == 0 { offset } else { (s.clock_offset_us * 7 + offset) / 8 };
        });
    }

    /// Capture-to-now latency for a frame captured at `capture_ts` (server clock).
    pub fn latency_ms(&self, capture_ts: u64) -> f32 {
        let off = self.with(|s| s.clock_offset_us);
        let now = nya_proto::now_us() as i64;
        ((now - (capture_ts as i64 - off)) as f32 / 1000.0).max(0.0)
    }
}

/// Summary over one interval.
#[derive(Default, Clone)]
pub struct Summary {
    pub rtt_ms: f32,
    /// Medians of the interval, and 99th percentiles of the last 10 s.
    pub decode_ms: f32,
    pub decode_p99: f32,
    pub render_ms: f32,
    pub render_p99: f32,
    pub latency_ms: f32,
    pub latency_p99: f32,
    pub fps: u32,
    pub dropped: u32,
    pub kbps: u32,
    pub decoder: String,
    pub audio: Option<JitterStats>,
    pub dgram: DgramTotals,
}

impl Shared {
    /// Take and reset the per-interval counters.
    pub fn take_summary(&self, secs: f32) -> Summary {
        self.with(|s| {
            let (decode_ms, decode_p99) = s.decode_window.close(std::mem::take(&mut s.decode_ms));
            let (render_ms, render_p99) = s.render_window.close(std::mem::take(&mut s.render_ms));
            let (latency_ms, latency_p99) = s.latency_window.close(std::mem::take(&mut s.latency_ms));
            let sum = Summary {
                rtt_ms: s.rtt_ms,
                decode_ms,
                decode_p99,
                render_ms,
                render_p99,
                latency_ms,
                latency_p99,
                fps: (s.frames_rendered as f32 / secs).round() as u32,
                dropped: s.frames_dropped,
                kbps: (s.bytes as f32 * 8.0 / 1000.0 / secs) as u32,
                decoder: s.decoder.clone(),
                audio: s.audio,
                dgram: std::mem::take(&mut s.dgram),
            };
            s.frames_decoded = 0;
            s.frames_rendered = 0;
            s.frames_dropped = 0;
            s.bytes = 0;
            sum
        })
    }
}
