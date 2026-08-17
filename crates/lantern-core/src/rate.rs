//! Turning "n bytes done" into a speed a person can read.
//!
//! DESIGN §5.3 wants a transfer to say how fast it's going, not just how far
//! it's got. Two things make that harder than `done / elapsed`:
//!
//! * A running average lags. Pull the cable half way through a 2 GB file and
//!   the average still claims 90 MB/s for a minute. What people read a speed
//!   for is "is this moving, and roughly how long do I wait" — a recent rate,
//!   not a lifetime one.
//! * A raw recent rate jitters. Chunks land in 1 MiB bursts, so the
//!   instantaneous figure between two chunks swings wildly, and a number
//!   that flickers between 4 and 90 MB/s reads as broken.
//!
//! So: sample no faster than `MIN_WINDOW`, and smooth the samples
//! exponentially. Nothing is reported until the first window closes — an
//! honest blank beats a wild guess.

use std::time::{Duration, Instant};

/// Shortest interval that counts as a measurement. Below this the divisor is
/// small enough that scheduler noise dominates the answer.
const MIN_WINDOW: Duration = Duration::from_millis(200);

/// Weight of the newest sample. Low enough to steady the burstiness of 1 MiB
/// chunks, high enough that stalling shows up within about a second.
const ALPHA: f64 = 0.4;

pub struct RateMeter {
    last_bytes: u64,
    last_at: Instant,
    /// Smoothed bytes per second, once measurable.
    bps: Option<f64>,
}

impl RateMeter {
    pub fn new(now: Instant) -> Self {
        Self {
            last_bytes: 0,
            last_at: now,
            bps: None,
        }
    }

    /// Feed cumulative bytes moved so far. Returns `true` when this call
    /// closed a measurement window — callers use that as the cue to emit a
    /// progress event, which keeps event volume tied to the clock rather than
    /// to chunk size.
    pub fn sample(&mut self, done: u64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last_at);
        if elapsed < MIN_WINDOW {
            return false;
        }
        let moved = done.saturating_sub(self.last_bytes) as f64;
        let instant = moved / elapsed.as_secs_f64();
        self.bps = Some(match self.bps {
            Some(prev) => prev * (1.0 - ALPHA) + instant * ALPHA,
            None => instant,
        });
        self.last_bytes = done;
        self.last_at = now;
        true
    }

    /// Smoothed bytes per second, or None until the first window closed.
    pub fn bps(&self) -> Option<u64> {
        self.bps.map(|b| b.max(0.0) as u64)
    }

    /// Seconds left for `remaining` bytes at the current rate. None while the
    /// rate is unknown or has fallen to zero — an unbounded ETA is a lie, and
    /// "stalled" is the shell's word to say, not this module's.
    pub fn eta_s(&self, remaining: u64) -> Option<u64> {
        match self.bps {
            Some(b) if b >= 1.0 => Some((remaining as f64 / b).round() as u64),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_reported_before_a_window_closes() {
        let t0 = Instant::now();
        let mut m = RateMeter::new(t0);
        // 1 MiB in 10 ms is a real burst, but far too short to measure from.
        assert!(!m.sample(1 << 20, t0 + Duration::from_millis(10)));
        assert_eq!(m.bps(), None);
        assert_eq!(m.eta_s(1 << 20), None);
    }

    #[test]
    fn steady_transfer_reports_its_actual_rate() {
        let t0 = Instant::now();
        let mut m = RateMeter::new(t0);
        // 10 MB/s, sampled every second: smoothing has nothing to correct,
        // so the answer must be the true rate, not an approach to it.
        let mut done = 0u64;
        for s in 1..=6 {
            done += 10_000_000;
            assert!(m.sample(done, t0 + Duration::from_secs(s)));
        }
        let bps = m.bps().unwrap();
        assert!(
            (9_900_000..=10_100_000).contains(&bps),
            "expected ~10 MB/s, got {bps}"
        );
        // 50 MB left at 10 MB/s is 5 s, give or take rounding.
        assert_eq!(m.eta_s(50_000_000), Some(5));
    }

    #[test]
    fn a_stall_pulls_the_rate_down_within_a_second() {
        let t0 = Instant::now();
        let mut m = RateMeter::new(t0);
        let mut done = 0u64;
        for s in 1..=5 {
            done += 10_000_000;
            m.sample(done, t0 + Duration::from_secs(s));
        }
        // Bytes stop; the clock doesn't. After a few windows the figure has
        // to collapse, not keep quoting 10 MB/s.
        for ms in [5_200u64, 5_400, 5_600, 5_800, 6_000] {
            m.sample(done, t0 + Duration::from_millis(ms));
        }
        let bps = m.bps().unwrap();
        assert!(bps < 2_000_000, "stalled transfer still claims {bps} B/s");
    }

    #[test]
    fn a_finished_transfer_reports_no_time_left() {
        let t0 = Instant::now();
        let mut m = RateMeter::new(t0);
        m.sample(10_000_000, t0 + Duration::from_secs(1));
        assert_eq!(m.eta_s(0), Some(0));
    }
}
