//! Per-tracker-host concurrency gate for announces.
//!
//! Many torrents often share a handful of tracker hosts. Without a gate,
//! startup can open thousands of parallel announces against the same host.
//!
//! [`HostLimiter::acquire`] uses a flume token channel so waiters work on Compio
//! (tracker thread) without Tokio.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

/// At most `max` announces run concurrently per host key.
#[derive(Debug)]
pub struct HostLimiter {
    max: usize,
    hosts: Mutex<HashMap<String, Arc<HostSem>>>,
}

/// One host's permit pool: bounded channel pre-filled with `max` tokens.
#[derive(Debug)]
struct HostSem {
    /// Return tokens here on permit drop.
    tx: flume::Sender<()>,
    /// Wait for a free slot.
    rx: flume::Receiver<()>,
}

/// RAII permit — releases on drop.
pub struct HostPermit {
    release: Option<flume::Sender<()>>,
}

impl Drop for HostPermit {
    fn drop(&mut self) {
        if let Some(tx) = self.release.take() {
            let _ = tx.send(());
        }
    }
}

impl HostLimiter {
    /// `max == 0` means unlimited (no waiting).
    pub fn new(max_concurrent_per_host: usize) -> Self {
        Self {
            max: max_concurrent_per_host,
            hosts: Mutex::new(HashMap::new()),
        }
    }

    pub fn max_per_host(&self) -> usize {
        self.max
    }

    fn unlimited_permit() -> HostPermit {
        HostPermit { release: None }
    }

    fn sem_for(&self, host_key: &str) -> Arc<HostSem> {
        let mut map = self.hosts.lock();
        map.entry(host_key.to_string())
            .or_insert_with(|| {
                let max = self.max.max(1);
                let (tx, rx) = flume::bounded(max);
                for _ in 0..max {
                    // Pre-fill permits; bounded capacity == max.
                    let _ = tx.send(());
                }
                Arc::new(HostSem { tx, rx })
            })
            .clone()
    }

    /// Wait until a slot is free for `host_key` (async; Compio-friendly).
    ///
    /// Empty host key or unlimited max → no-op permit.
    pub async fn acquire(&self, host_key: &str) -> HostPermit {
        if self.max == 0 || host_key.is_empty() {
            return Self::unlimited_permit();
        }
        let sem = self.sem_for(host_key);
        sem.rx
            .recv_async()
            .await
            .expect("HostLimiter channel closed");
        HostPermit {
            release: Some(sem.tx.clone()),
        }
    }
}

/// Normalize announce URL → host key for rate limiting (`host:port`, lowercased).
///
/// Falls back to the full lowercased URL when parsing fails so distinct bad
/// strings still get separate buckets.
pub fn tracker_host_key(announce_url: &str) -> String {
    let raw = announce_url.trim();
    if raw.is_empty() {
        return String::new();
    }
    // url crate needs a scheme; prepend dummy for bare hosts.
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    match url::Url::parse(&candidate) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("").to_ascii_lowercase();
            if host.is_empty() {
                return raw.to_ascii_lowercase();
            }
            let port = u.port_or_known_default().unwrap_or(80);
            format!("{host}:{port}")
        }
        Err(_) => raw.to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn host_key_http_and_udp() {
        assert_eq!(
            tracker_host_key("http://Tracker.Example/announce"),
            "tracker.example:80"
        );
        assert_eq!(tracker_host_key("https://t.example:443/a"), "t.example:443");
        assert_eq!(
            tracker_host_key("udp://open.tracker:1337/announce"),
            "open.tracker:1337"
        );
    }

    #[test]
    fn unlimited_when_zero() {
        let lim = HostLimiter::new(0);
        let rt = compio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let t0 = Instant::now();
            let _a = lim.acquire("h:1").await;
            let _b = lim.acquire("h:1").await;
            let _c = lim.acquire("h:1").await;
            assert!(t0.elapsed() < Duration::from_millis(50));
        });
    }

    #[test]
    fn limiter_caps_concurrent() {
        let lim = Arc::new(HostLimiter::new(2));
        let peak = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let rt = compio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let mut joins = Vec::new();
            for _ in 0..8 {
                let lim = lim.clone();
                let peak = peak.clone();
                let in_flight = in_flight.clone();
                joins.push(compio::runtime::spawn(async move {
                    let _p = lim.acquire("host.example:80").await;
                    let n = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    compio::time::sleep(Duration::from_millis(30)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                }));
            }
            for j in joins {
                j.await.ok();
            }
            assert!(
                peak.load(Ordering::SeqCst) <= 2,
                "peak concurrent was {}",
                peak.load(Ordering::SeqCst)
            );
        });
    }
}
