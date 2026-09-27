//! Adaptive bitrate: back off quickly when the path shows congestion, creep
//! back up slowly while it is clear (design doc §6.2, phase 2).
//!
//! Signals, sampled every 250 ms by the network session:
//! * backlog – video bytes handed to QUIC but not yet sent (queueing in our buffers)
//! * queueing delay – RTT above the lowest RTT seen on this connection
//! * packet loss – newly lost packets since the previous sample

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub now: Instant,
    pub rtt: Duration,
    pub lost_packets: u64,
    pub backlog_bytes: u64,
}

#[derive(Debug)]
pub struct Abr {
    max: u32,
    min: u32,
    target: u32,
    reported: u32,
    min_rtt: Option<Duration>,
    last_lost: Option<u64>,
    last_decrease: Instant,
    last_increase: Instant,
}

const DECREASE: f64 = 0.8;
const INCREASE: f64 = 1.08;
const DECREASE_GAP: Duration = Duration::from_millis(500);
const CLEAR_BEFORE_INCREASE: Duration = Duration::from_secs(3);
const INCREASE_GAP: Duration = Duration::from_secs(1);

impl Abr {
    pub fn new(max_kbps: u32, now: Instant) -> Self {
        let max = max_kbps.max(500);
        Self {
            max,
            min: (max / 10).max(800).min(max),
            target: max,
            reported: max,
            min_rtt: None,
            last_lost: None,
            last_decrease: now - Duration::from_secs(10),
            last_increase: now,
        }
    }

    #[cfg(test)]
    pub fn target(&self) -> u32 {
        self.target
    }

    /// Is the path congested according to this sample?
    fn congested(&mut self, s: &Sample) -> bool {
        let min_rtt = *self.min_rtt.get_or_insert(s.rtt);
        if s.rtt < min_rtt {
            self.min_rtt = Some(s.rtt);
        }
        let min_rtt = self.min_rtt.unwrap();
        let queue_delay = s.rtt.saturating_sub(min_rtt);
        let lost = s.lost_packets.saturating_sub(self.last_lost.unwrap_or(s.lost_packets));
        self.last_lost = Some(s.lost_packets);
        // bytes * 8 / kbit/s = ms
        let backlog_ms = s.backlog_bytes * 8 / self.target.max(1) as u64;
        backlog_ms > 200 || queue_delay > Duration::from_millis(80) + min_rtt / 2 || lost > 3
    }

    /// Feed one sample; returns the new target when it moved by 5 % or more.
    pub fn update(&mut self, s: Sample) -> Option<u32> {
        let congested = self.congested(&s);
        if congested {
            if s.now - self.last_decrease >= DECREASE_GAP {
                self.target = ((self.target as f64 * DECREASE) as u32).max(self.min);
                self.last_decrease = s.now;
            }
        } else if s.now - self.last_decrease >= CLEAR_BEFORE_INCREASE && s.now - self.last_increase >= INCREASE_GAP {
            self.target = ((self.target as f64 * INCREASE) as u32).min(self.max);
            self.last_increase = s.now;
        }
        let diff = (self.target as i64 - self.reported as i64).unsigned_abs() as f64;
        if diff >= self.reported as f64 * 0.05 || (self.target == self.max && self.reported != self.max) {
            self.reported = self.target;
            Some(self.target)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(now: Instant, rtt_ms: u64, lost: u64, backlog: u64) -> Sample {
        Sample { now, rtt: Duration::from_millis(rtt_ms), lost_packets: lost, backlog_bytes: backlog }
    }

    #[test]
    fn stays_at_max_when_clear() {
        let t0 = Instant::now();
        let mut a = Abr::new(10_000, t0);
        for i in 0..40 {
            assert_eq!(a.update(sample(t0 + Duration::from_millis(250 * i), 20, 0, 0)), None);
        }
        assert_eq!(a.target(), 10_000);
    }

    #[test]
    fn backs_off_on_backlog_then_recovers() {
        let t0 = Instant::now();
        let mut a = Abr::new(10_000, t0);
        a.update(sample(t0, 20, 0, 0));
        // 1 MB backlog at 10 Mbit/s = 800 ms of queue.
        let r = a.update(sample(t0 + Duration::from_millis(250), 20, 0, 1_000_000));
        assert_eq!(r, Some(8_000));
        // Not again within 500 ms.
        assert_eq!(a.update(sample(t0 + Duration::from_millis(500), 20, 0, 1_000_000)), None);
        let r = a.update(sample(t0 + Duration::from_millis(800), 20, 0, 1_000_000));
        assert_eq!(r, Some(6_400));
        // Clear path: nothing for 3 s, then slow growth back to the max.
        let mut t = t0 + Duration::from_millis(800);
        let mut last = 6_400;
        for _ in 0..200 {
            t += Duration::from_millis(250);
            if let Some(v) = a.update(sample(t, 20, 0, 0)) {
                assert!(v > last && v <= 10_000);
                assert!(t - (t0 + Duration::from_millis(800)) >= CLEAR_BEFORE_INCREASE);
                last = v;
            }
        }
        assert_eq!(a.target(), 10_000);
    }

    #[test]
    fn rtt_growth_and_loss_count_as_congestion() {
        let t0 = Instant::now();
        let mut a = Abr::new(20_000, t0);
        a.update(sample(t0, 10, 0, 0));
        assert!(a.update(sample(t0 + Duration::from_millis(250), 200, 0, 0)).is_some(), "queueing delay");
        let mut b = Abr::new(20_000, t0);
        b.update(sample(t0, 10, 100, 0));
        assert!(b.update(sample(t0 + Duration::from_millis(250), 10, 110, 0)).is_some(), "loss");
    }

    #[test]
    fn never_below_floor() {
        let t0 = Instant::now();
        let mut a = Abr::new(10_000, t0);
        let mut t = t0;
        for _ in 0..100 {
            t += Duration::from_millis(600);
            a.update(sample(t, 20, 0, 10_000_000));
        }
        assert_eq!(a.target(), 1_000);
    }
}
