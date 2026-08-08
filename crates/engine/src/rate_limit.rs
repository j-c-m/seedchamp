//! Global wire rate limits (`max_upload_bps` / `max_download_bps`).
//!
//! **`0` = unlimited** for that direction. Unlimited paths do **not** call
//! `Instant::now()` or take a mutex — only a relaxed atomic load.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Burst capacity as a multiple of the sustained rate (seconds of traffic).
const BURST_SECS: f64 = 1.5;

/// Process-wide wire rate limiter shared by all peer tasks.
pub struct WireRateLimiter {
    /// Bytes/sec; **0 = unlimited**.
    up_cap: AtomicU64,
    down_cap: AtomicU64,
    up: Mutex<TokenBucket>,
    down: Mutex<TokenBucket>,
}

struct TokenBucket {
    /// Current tokens (bytes).
    tokens: f64,
    /// Cap for this bucket (rate × BURST_SECS); 0 when unlimited / inactive.
    capacity: f64,
    /// Sustained refill rate (bytes/sec).
    rate: f64,
    last: Instant,
}

impl TokenBucket {
    fn unlimited() -> Self {
        Self {
            tokens: 0.0,
            capacity: 0.0,
            rate: 0.0,
            last: Instant::now(),
        }
    }

    fn reconfigure(&mut self, rate_bps: u64) {
        if rate_bps == 0 {
            self.rate = 0.0;
            self.capacity = 0.0;
            self.tokens = 0.0;
            return;
        }
        let rate = rate_bps as f64;
        let capacity = rate * BURST_SECS;
        // On first enable or increase, give a full burst so startup is not starved.
        self.rate = rate;
        self.capacity = capacity;
        self.tokens = capacity;
        self.last = Instant::now();
    }

    fn refill(&mut self, now: Instant) {
        if self.rate <= 0.0 {
            return;
        }
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        if dt > 0.0 {
            self.tokens = (self.tokens + self.rate * dt).min(self.capacity);
            self.last = now;
        }
    }

    /// Bytes available without deducting.
    fn available(&mut self, now: Instant) -> u64 {
        self.refill(now);
        self.tokens.max(0.0) as u64
    }

    fn deduct(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        self.tokens = (self.tokens - n as f64).max(0.0);
    }

    /// Deduct up to `want`; returns granted amount.
    fn try_consume(&mut self, want: u64, now: Instant) -> u64 {
        if want == 0 {
            return 0;
        }
        self.refill(now);
        let grant = (want as f64).min(self.tokens).max(0.0) as u64;
        self.tokens -= grant as f64;
        grant
    }
}

impl WireRateLimiter {
    pub fn new(up_bps: u64, down_bps: u64) -> Self {
        let lim = Self {
            up_cap: AtomicU64::new(0),
            down_cap: AtomicU64::new(0),
            up: Mutex::new(TokenBucket::unlimited()),
            down: Mutex::new(TokenBucket::unlimited()),
        };
        lim.set_caps(up_bps, down_bps);
        lim
    }

    /// Update caps (`0` = unlimited). Safe to call from any thread.
    pub fn set_caps(&self, up_bps: u64, down_bps: u64) {
        self.up_cap.store(up_bps, Ordering::Relaxed);
        self.down_cap.store(down_bps, Ordering::Relaxed);
        self.up.lock().reconfigure(up_bps);
        self.down.lock().reconfigure(down_bps);
    }

    #[inline]
    pub fn upload_unlimited(&self) -> bool {
        self.up_cap.load(Ordering::Relaxed) == 0
    }

    #[inline]
    pub fn download_unlimited(&self) -> bool {
        self.down_cap.load(Ordering::Relaxed) == 0
    }

    /// How many of `want` payload bytes may be sent now (peek; does not deduct).
    /// Unlimited → `want` with no clock/mutex.
    #[inline]
    pub fn allow_upload(&self, want: u64) -> u64 {
        if want == 0 || self.upload_unlimited() {
            return want;
        }
        self.up.lock().available(Instant::now()).min(want)
    }

    /// Deduct after bytes actually left the socket. No-op if unlimited.
    #[inline]
    pub fn commit_upload(&self, n: u64) {
        if n == 0 || self.upload_unlimited() {
            return;
        }
        self.up.lock().deduct(n);
    }

    /// Reserve upload tokens for a full block before starting the PIECE on the wire.
    /// Unlimited → `want`. Partial grant means "not enough for this block now".
    #[inline]
    pub fn try_consume_upload(&self, want: u64) -> u64 {
        if want == 0 || self.upload_unlimited() {
            return want;
        }
        self.up.lock().try_consume(want, Instant::now())
    }

    /// Put back unused upload tokens (e.g. piece aborted before any wire bytes).
    #[inline]
    pub fn refund_upload(&self, n: u64) {
        if n == 0 || self.upload_unlimited() {
            return;
        }
        let mut b = self.up.lock();
        b.tokens = (b.tokens + n as f64).min(b.capacity.max(n as f64));
    }

    /// Reserve up to `want` download bytes (Request issue). Unlimited → `want`.
    #[inline]
    pub fn try_consume_download(&self, want: u64) -> u64 {
        if want == 0 || self.download_unlimited() {
            return want;
        }
        self.down.lock().try_consume(want, Instant::now())
    }

    /// Put back unused download tokens.
    #[inline]
    pub fn refund_download(&self, n: u64) {
        if n == 0 || self.download_unlimited() {
            return;
        }
        let mut b = self.down.lock();
        b.tokens = (b.tokens + n as f64).min(b.capacity.max(n as f64));
    }

    /// True if at least `need` download tokens are available (peek).
    #[inline]
    pub fn allow_download(&self, need: u64) -> bool {
        if need == 0 || self.download_unlimited() {
            return true;
        }
        self.down.lock().available(Instant::now()) >= need
    }

    /// How long until `need` download tokens are available (for Request pacing).
    ///
    /// `Duration::ZERO` when unlimited, `need == 0`, or tokens already available.
    /// Used so the leech reader can sleep instead of parking forever on an empty
    /// socket after the burst is spent (no outstanding PIECE to wake on).
    pub fn download_delay_for(&self, need: u64) -> Duration {
        Self::bucket_delay(&self.down, need, self.download_unlimited())
    }

    /// How long until `need` upload tokens are available (writer rate-limit sleep).
    pub fn upload_delay_for(&self, need: u64) -> Duration {
        Self::bucket_delay(&self.up, need, self.upload_unlimited())
    }

    fn bucket_delay(bucket: &Mutex<TokenBucket>, need: u64, unlimited: bool) -> Duration {
        if need == 0 || unlimited {
            return Duration::ZERO;
        }
        let mut b = bucket.lock();
        let now = Instant::now();
        b.refill(now);
        if b.tokens >= need as f64 || b.rate <= 0.0 {
            return Duration::ZERO;
        }
        let deficit = need as f64 - b.tokens;
        let secs = (deficit / b.rate).max(0.001);
        // Cap so a misconfigured tiny rate still rechecks periodically.
        Duration::from_secs_f64(secs.min(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn unlimited_allow_is_identity() {
        let lim = WireRateLimiter::new(0, 0);
        assert_eq!(lim.allow_upload(1_000_000), 1_000_000);
        lim.commit_upload(1_000_000); // no-op
        assert_eq!(lim.try_consume_download(999), 999);
        assert!(lim.allow_download(1));
    }

    #[test]
    fn upload_bucket_caps_and_refills() {
        let lim = WireRateLimiter::new(1000, 0); // 1 KiB/s
                                                 // Full burst = 1500 bytes
        let a = lim.allow_upload(10_000);
        assert!((1000..=1500).contains(&a), "burst grant {a}");
        lim.commit_upload(a);
        assert_eq!(lim.allow_upload(10_000), 0);
        thread::sleep(Duration::from_millis(200));
        let b = lim.allow_upload(10_000);
        assert!(b > 0, "should refill after sleep, got {b}");
    }

    #[test]
    fn download_try_consume_deducts() {
        let lim = WireRateLimiter::new(0, 16_384); // 16 KiB/s, burst ~24 KiB
        let g = lim.try_consume_download(16_384);
        assert_eq!(g, 16_384);
        let g2 = lim.try_consume_download(100_000);
        assert!(g2 < 100_000);
    }

    #[test]
    fn set_caps_to_zero_disables() {
        let lim = WireRateLimiter::new(100, 100);
        assert!(!lim.upload_unlimited());
        lim.set_caps(0, 0);
        assert!(lim.upload_unlimited());
        assert_eq!(lim.allow_upload(9_999), 9_999);
    }

    #[test]
    fn download_delay_zero_when_available() {
        let lim = WireRateLimiter::new(0, 100_000);
        assert_eq!(lim.download_delay_for(16_384), Duration::ZERO);
        assert_eq!(lim.download_delay_for(0), Duration::ZERO);
        let unlimited = WireRateLimiter::new(0, 0);
        assert_eq!(unlimited.download_delay_for(1_000_000), Duration::ZERO);
    }

    #[test]
    fn download_delay_positive_when_bucket_empty() {
        let lim = WireRateLimiter::new(0, 10_000); // 10 KiB/s, burst 15 KiB
        let _ = lim.try_consume_download(20_000);
        let d = lim.download_delay_for(16_384);
        assert!(d > Duration::ZERO, "expected wait, got {d:?}");
        assert!(d <= Duration::from_secs(1));
    }

    #[test]
    fn upload_delay_matches_deficit() {
        let lim = WireRateLimiter::new(16_384, 0); // 16 KiB/s
        let _ = lim.try_consume_upload(100_000); // drain burst
        let d = lim.upload_delay_for(16_384);
        // ~1s for a full block when empty; allow slack.
        assert!(d >= Duration::from_millis(50), "got {d:?}");
        assert!(d <= Duration::from_secs(1), "got {d:?}");
    }
}
