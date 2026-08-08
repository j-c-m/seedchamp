//! Rolling-window + EMA rate estimates for TUI snapshots.
//!
//! 1. Sliding average of cumulative counters over [`RATE_WINDOW`] (truth).
//! 2. EMA on that average with [`RATE_EMA_ALPHA`] (display feel).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// One cumulative byte counter sample for the rolling rate window.
#[derive(Clone, Copy)]
pub(super) struct RatePoint {
    pub(super) at: Instant,
    pub(super) up: u64,
    pub(super) down: u64,
}

/// Rate state: window mean + EMA for UI.
pub(super) struct RateSample {
    pub(super) history: VecDeque<RatePoint>,
    /// Smoothed rates exposed to the TUI / snapshot.
    pub(super) up_bps: u64,
    pub(super) down_bps: u64,
    /// First valid window sample seeds EMA (avoids slow ramp from 0).
    ema_seeded: bool,
}

/// Sliding-window span for the raw average (bytes over time).
pub(super) const RATE_WINDOW: Duration = Duration::from_secs(15);
/// EMA weight on the latest window rate per sample (~1/α samples to settle).
/// With ~1 s TUI polls, 0.15 ≈ multi-second soft settle on top of the 15 s window.
pub(super) const RATE_EMA_ALPHA: f64 = 0.15;
/// Min spacing between history points (TUI may poll faster).
pub(super) const RATE_MIN_DT: Duration = Duration::from_millis(400);

impl RateSample {
    pub(super) fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(48),
            up_bps: 0,
            down_bps: 0,
            ema_seeded: false,
        }
    }
}

/// Update rates from cumulative `up` / `down` counters.
///
/// Window: `rate = (bytes_now − bytes_at_window_start) / elapsed` over up to
/// [`RATE_WINDOW`]. Display: EMA of that rate with α = [`RATE_EMA_ALPHA`].
///
/// Aggregate counters (torrent/global wire sums) can step down when a peer
/// disconnects. Re-anchor history only — never zero the displayed EMA, or the
/// TUI flashes `—` between two high rates.
pub(super) fn update_rate(sample: &mut RateSample, up: u64, down: u64, now: Instant) {
    if let Some(last) = sample.history.back() {
        if now.duration_since(last.at) < RATE_MIN_DT {
            return; // keep last displayed rate
        }
        // Non-monotonic (session restart, or peer-sum dip): re-anchor window.
        if up < last.up || down < last.down {
            sample.history.clear();
        }
    }

    sample.history.push_back(RatePoint { at: now, up, down });

    // Drop points older than the window, but keep one anchor at/before the edge
    // so (now − anchor) covers ~RATE_WINDOW.
    while sample.history.len() >= 2 {
        let second = sample.history[1].at;
        if now.duration_since(second) >= RATE_WINDOW {
            sample.history.pop_front();
        } else {
            break;
        }
    }
    // Cap history (poll every 0.4s → ~38 pts / 15s; allow slack).
    while sample.history.len() > 50 {
        sample.history.pop_front();
    }

    let Some(first) = sample.history.front() else {
        return;
    };
    let last = *sample.history.back().unwrap();
    let dt = last.at.duration_since(first.at).as_secs_f64();

    // Need a real span before trusting a window mean (else win=0 pulls EMA to —).
    if dt <= 0.05 {
        return;
    }

    let mut win_up = (last.up.saturating_sub(first.up) as f64 / dt) as u64;
    let mut win_down = (last.down.saturating_sub(first.down) as f64 / dt) as u64;

    // Floor tiny noise on the window mean before EMA.
    if win_up < 512 {
        win_up = 0;
    }
    if win_down < 512 {
        win_down = 0;
    }

    if !sample.ema_seeded {
        sample.up_bps = win_up;
        sample.down_bps = win_down;
        sample.ema_seeded = true;
        return;
    }

    sample.up_bps = ema_u64(sample.up_bps, win_up, RATE_EMA_ALPHA);
    sample.down_bps = ema_u64(sample.down_bps, win_down, RATE_EMA_ALPHA);

    // Keep floor after EMA so residual doesn't chatter at 1–500 B/s.
    if sample.up_bps < 512 {
        sample.up_bps = 0;
    }
    if sample.down_bps < 512 {
        sample.down_bps = 0;
    }
}

#[inline]
fn ema_u64(prev: u64, next: u64, alpha: f64) -> u64 {
    let a = alpha.clamp(0.0, 1.0);
    ((a * next as f64) + ((1.0 - a) * prev as f64)).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ema_decays_when_window_goes_quiet() {
        let mut s = RateSample::new();
        let t0 = Instant::now();
        // Burst in the first second, then idle long enough that the burst
        // ages out of the 15 s window → window mean ~0 → EMA decays.
        update_rate(&mut s, 0, 0, t0);
        update_rate(&mut s, 15_000_000, 0, t0 + Duration::from_secs(1));
        assert!(
            s.up_bps > 100_000,
            "seeded high window rate, got {}",
            s.up_bps
        );
        let peak = s.up_bps;
        for sec in 2..=20 {
            update_rate(&mut s, 15_000_000, 0, t0 + Duration::from_secs(sec));
        }
        assert!(s.up_bps < peak, "EMA below peak after quiet window");
        let mid = ema_u64(peak, 0, RATE_EMA_ALPHA);
        assert!(mid > 0 && mid < peak);
    }

    #[test]
    fn ema_u64_formula() {
        assert_eq!(ema_u64(1000, 0, 0.15), 850);
        assert_eq!(ema_u64(0, 1000, 0.15), 150);
    }

    /// Aggregate wire_down can drop when a peer leaves; must not flash 0 / `—`.
    #[test]
    fn counter_dip_keeps_displayed_rate() {
        let mut s = RateSample::new();
        let t0 = Instant::now();
        // ~35 MiB/s down for a few seconds.
        let mut down = 0u64;
        for sec in 0..=4 {
            down = 35 * 1024 * 1024 * sec;
            update_rate(&mut s, 0, down, t0 + Duration::from_secs(sec));
        }
        let before = s.down_bps;
        assert!(
            before > 30 * 1024 * 1024,
            "expected ~35MiB/s class rate, got {before}"
        );
        // Peer disconnect style: sum drops by half, then resumes climbing.
        down /= 2;
        update_rate(&mut s, 0, down, t0 + Duration::from_secs(5));
        assert_eq!(
            s.down_bps, before,
            "single-point re-anchor must keep last EMA, not zero"
        );
        // Resume at ~31 MiB/s from the new base.
        for sec in 6..=10 {
            down += 31 * 1024 * 1024;
            update_rate(&mut s, 0, down, t0 + Duration::from_secs(sec));
            assert!(
                s.down_bps > 512,
                "must not flash idle after dip (sec={sec}, bps={})",
                s.down_bps
            );
        }
        assert!(
            s.down_bps > 20 * 1024 * 1024,
            "should re-settle near 31MiB/s class, got {}",
            s.down_bps
        );
    }
}
