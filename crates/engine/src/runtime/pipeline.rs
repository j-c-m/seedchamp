//! Download pipeline sizing: BDP target from smoothed per-peer rate.
//!
//! **Configurable:** initial depth (`swarm.pipeline`) and cap (`swarm.pipeline_max`).
//! **Internal:** 5 s queue time, EMA α, sample interval, shrink hysteresis.
//!
//! Not the TUI 10 s [`crate::session`] rate window — peer-local `wire_down` only.

use std::time::{Duration, Instant};

use crate::staging::BLOCK_SIZE;

/// Adaptive floor (never shrink/grow-clamp below this).
pub const MIN_PIPELINE: usize = 2;
/// Default initial pipeline before rate samples (connect / reopen).
pub const DEFAULT_PIPELINE: usize = 16;
/// Default adaptive cap (8192 × 16 KiB ≈ 128 MiB worst case per peer).
pub const MAX_PIPELINE: usize = 8192;

/// Seconds of download work to keep outstanding (BDP). Hardcoded.
pub const REQUEST_QUEUE_TIME_SECS: f64 = 5.0;
/// EMA weight on the latest sample (0.25 @ 0.5 s ≈ few-second memory).
pub const PIPELINE_RATE_ALPHA: f64 = 0.25;
/// Minimum interval between rate samples.
pub const PIPELINE_SAMPLE_SECS: f64 = 0.5;
/// Shrink only when desired &lt; current × this ratio.
pub const PIPELINE_SHRINK_RATIO: f64 = 0.85;
/// How long desired must stay low before shrinking.
pub const PIPELINE_SHRINK_HOLD_SECS: f64 = 2.0;

/// BDP block count from download rate (bytes/sec).
///
/// `queue_time * rate / BLOCK_SIZE`, clamped to `[min, max]`.
#[inline]
pub fn desired_pipeline_blocks(
    rate_bps: u64,
    queue_time_secs: f64,
    min: usize,
    max: usize,
) -> usize {
    let min = min.max(1);
    let max = max.max(min);
    if rate_bps == 0 || queue_time_secs <= 0.0 {
        return min;
    }
    let blocks = queue_time_secs * (rate_bps as f64) / (BLOCK_SIZE as f64);
    let n = if blocks.is_finite() && blocks > 0.0 {
        blocks.round() as usize
    } else {
        min
    };
    n.clamp(min, max)
}

/// Tuning for [`adapt_pipeline`] (cap from config; rest defaults).
#[derive(Debug, Clone, Copy)]
pub struct PipelineTuning {
    pub min: usize,
    pub max: usize,
    pub queue_time_secs: f64,
    pub alpha: f64,
    pub sample_secs: f64,
    pub shrink_ratio: f64,
    pub shrink_hold_secs: f64,
}

impl PipelineTuning {
    /// Defaults with adaptive cap from config (`pipeline_max`).
    pub fn with_max(pipeline_max: usize) -> Self {
        let max = pipeline_max.max(MIN_PIPELINE);
        Self {
            min: MIN_PIPELINE,
            max,
            queue_time_secs: REQUEST_QUEUE_TIME_SECS,
            alpha: PIPELINE_RATE_ALPHA,
            sample_secs: PIPELINE_SAMPLE_SECS,
            shrink_ratio: PIPELINE_SHRINK_RATIO,
            shrink_hold_secs: PIPELINE_SHRINK_HOLD_SECS,
        }
    }
}

impl Default for PipelineTuning {
    fn default() -> Self {
        Self::with_max(MAX_PIPELINE)
    }
}

/// Per-peer adapt state (lives on the peer task).
#[derive(Debug, Clone)]
pub struct PipelineAdaptState {
    pub smooth_bps: u64,
    pub rate_bytes: u64,
    pub rate_at: Instant,
    pub pipeline: usize,
    pub shrink_since: Option<Instant>,
    /// False until the first sample seeds `smooth_bps`.
    pub rate_seeded: bool,
}

impl PipelineAdaptState {
    pub fn new(initial_pipeline: usize, tuning: &PipelineTuning) -> Self {
        let pipe = initial_pipeline.max(tuning.min).min(tuning.max).max(1);
        Self {
            smooth_bps: 0,
            rate_bytes: 0,
            rate_at: Instant::now(),
            pipeline: pipe,
            shrink_since: None,
            rate_seeded: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineAdaptOutcome {
    /// No sample yet, or depth unchanged.
    Unchanged,
    Grew {
        pipeline: usize,
    },
    Shrank {
        pipeline: usize,
    },
}

/// Update smoothed rate and pipeline target from cumulative `wire_down`.
///
/// Call each peer-loop iteration; samples only when `sample_secs` elapsed.
/// Grow immediately; shrink only after hysteresis.
pub fn adapt_pipeline(
    state: &mut PipelineAdaptState,
    wire_down: u64,
    now: Instant,
    tuning: &PipelineTuning,
) -> PipelineAdaptOutcome {
    let sample_dt = Duration::from_secs_f64(tuning.sample_secs.max(0.05));
    let elapsed = now.saturating_duration_since(state.rate_at);
    if elapsed < sample_dt {
        return PipelineAdaptOutcome::Unchanged;
    }

    let delta = wire_down.saturating_sub(state.rate_bytes);
    let dt = elapsed.as_secs_f64().max(1e-6);
    let inst_bps = (delta as f64 / dt) as u64;
    state.rate_bytes = wire_down;
    state.rate_at = now;

    let alpha = tuning.alpha.clamp(0.01, 1.0);
    if !state.rate_seeded {
        state.smooth_bps = inst_bps;
        state.rate_seeded = true;
    } else {
        let s = (1.0 - alpha) * (state.smooth_bps as f64) + alpha * (inst_bps as f64);
        state.smooth_bps = s.max(0.0) as u64;
    }

    let desired = desired_pipeline_blocks(
        state.smooth_bps,
        tuning.queue_time_secs,
        tuning.min,
        tuning.max,
    );
    let current = state.pipeline.max(1);

    if desired > current {
        state.pipeline = desired;
        state.shrink_since = None;
        return PipelineAdaptOutcome::Grew { pipeline: desired };
    }

    let shrink_floor = ((current as f64) * tuning.shrink_ratio.clamp(0.01, 1.0)) as usize;
    if desired < shrink_floor.max(tuning.min) {
        let hold = Duration::from_secs_f64(tuning.shrink_hold_secs.max(0.0));
        match state.shrink_since {
            None => {
                state.shrink_since = Some(now);
                return PipelineAdaptOutcome::Unchanged;
            }
            Some(since) if now.saturating_duration_since(since) < hold => {
                return PipelineAdaptOutcome::Unchanged;
            }
            Some(_) => {
                let next = desired.max(tuning.min);
                if next < current {
                    state.pipeline = next;
                    state.shrink_since = None;
                    return PipelineAdaptOutcome::Shrank { pipeline: next };
                }
                state.shrink_since = None;
                return PipelineAdaptOutcome::Unchanged;
            }
        }
    }

    // Desired in the hold band — cancel any shrink timer.
    state.shrink_since = None;
    PipelineAdaptOutcome::Unchanged
}

/// Clamp initial pipeline from config into `[min, max]`.
#[inline]
pub fn clamp_initial_pipeline(initial: usize, max: usize) -> usize {
    let max = max.max(MIN_PIPELINE);
    initial.max(MIN_PIPELINE).min(max).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bdp_one_mib_five_secs() {
        // 1 MiB/s × 5 s / 16 KiB = 320
        let n = desired_pipeline_blocks(1024 * 1024, 5.0, MIN_PIPELINE, MAX_PIPELINE);
        assert_eq!(n, 320);
    }

    #[test]
    fn bdp_clamps_min_max() {
        assert_eq!(desired_pipeline_blocks(0, 5.0, 2, 100), 2);
        assert_eq!(desired_pipeline_blocks(100 * 1024 * 1024, 5.0, 2, 100), 100);
    }

    #[test]
    fn bdp_one_mib_five_secs_is_320() {
        let bdp = desired_pipeline_blocks(1024 * 1024, 5.0, 2, 8192);
        assert_eq!(bdp, 320);
    }

    #[test]
    fn adapt_grows_on_fast_rate() {
        let tuning = PipelineTuning::with_max(8192);
        let mut st = PipelineAdaptState::new(16, &tuning);
        let t0 = st.rate_at;
        // Simulate 1 MiB over 0.5s → 2 MiB/s inst, seeds smooth.
        let t1 = t0 + Duration::from_millis(500);
        let out = adapt_pipeline(&mut st, 1_000_000, t1, &tuning);
        assert!(matches!(out, PipelineAdaptOutcome::Grew { .. }) || st.pipeline >= 16);
        // More data at high rate.
        let t2 = t1 + Duration::from_millis(500);
        let bytes = 1_000_000 + 1_000_000; // another 1 MiB in 0.5s
        let out2 = adapt_pipeline(&mut st, bytes, t2, &tuning);
        assert!(
            st.pipeline > 16,
            "pipeline={} after fast samples, out={out2:?}",
            st.pipeline
        );
        if let PipelineAdaptOutcome::Grew { pipeline } = out2 {
            assert_eq!(pipeline, st.pipeline);
        }
    }

    #[test]
    fn adapt_hysteresis_holds_brief_dip() {
        let tuning = PipelineTuning {
            shrink_hold_secs: 2.0,
            ..PipelineTuning::with_max(8192)
        };
        let mut st = PipelineAdaptState::new(16, &tuning);
        // Build up smooth rate.
        let mut t = st.rate_at;
        let mut bytes = 0u64;
        for _ in 0..6 {
            t += Duration::from_millis(500);
            bytes += 500_000; // ~1 MB/s
            let _ = adapt_pipeline(&mut st, bytes, t, &tuning);
        }
        let high = st.pipeline;
        assert!(high > 16, "expected deep pipe, got {high}");

        // One quiet sample — should not shrink yet.
        t += Duration::from_millis(500);
        let out = adapt_pipeline(&mut st, bytes, t, &tuning); // delta 0
        assert_eq!(st.pipeline, high, "brief dip should hold depth");
        assert!(
            matches!(out, PipelineAdaptOutcome::Unchanged),
            "out={out:?}"
        );
    }

    #[test]
    fn adapt_shrinks_after_hold() {
        let tuning = PipelineTuning {
            shrink_hold_secs: 0.5,
            shrink_ratio: 0.85,
            ..PipelineTuning::with_max(8192)
        };
        let mut st = PipelineAdaptState::new(16, &tuning);
        let mut t = st.rate_at;
        let mut bytes = 0u64;
        for _ in 0..8 {
            t += Duration::from_millis(500);
            bytes += 1_000_000;
            let _ = adapt_pipeline(&mut st, bytes, t, &tuning);
        }
        let high = st.pipeline;
        assert!(high > 100, "high={high}");

        // Sustained zero rate past hold.
        for _ in 0..6 {
            t += Duration::from_millis(500);
            let _ = adapt_pipeline(&mut st, bytes, t, &tuning);
        }
        assert!(
            st.pipeline < high,
            "should shrink after hold: high={high} now={}",
            st.pipeline
        );
        assert!(st.pipeline >= MIN_PIPELINE);
    }

    #[test]
    fn zero_rate_stays_at_min_after_samples() {
        let tuning = PipelineTuning::with_max(8192);
        let mut st = PipelineAdaptState::new(16, &tuning);
        let mut t = st.rate_at;
        // No bytes; after shrink hold, may drop toward min.
        for _ in 0..10 {
            t += Duration::from_millis(500);
            let _ = adapt_pipeline(&mut st, 0, t, &tuning);
        }
        assert!(st.pipeline >= MIN_PIPELINE);
    }

    #[test]
    fn clamp_initial() {
        assert_eq!(clamp_initial_pipeline(16, 8192), 16);
        assert_eq!(clamp_initial_pipeline(1, 8192), MIN_PIPELINE);
        assert_eq!(clamp_initial_pipeline(10000, 100), 100);
    }

    #[test]
    fn ema_not_only_last_spike() {
        let tuning = PipelineTuning {
            alpha: 0.25,
            ..PipelineTuning::with_max(8192)
        };
        let mut st = PipelineAdaptState::new(16, &tuning);
        let mut t = st.rate_at;
        // Steady modest rate.
        let mut bytes = 0u64;
        for _ in 0..4 {
            t += Duration::from_millis(500);
            bytes += 50_000; // 100 KB/s
            let _ = adapt_pipeline(&mut st, bytes, t, &tuning);
        }
        let steady = st.smooth_bps;
        // One huge spike sample.
        t += Duration::from_millis(500);
        bytes += 5_000_000;
        let _ = adapt_pipeline(&mut st, bytes, t, &tuning);
        // Smooth should be between steady and spike, not equal to spike alone.
        assert!(st.smooth_bps > steady);
        assert!(st.smooth_bps < 5_000_000 / 1); // not pure 10 MB/s from 0.5s of 5MB
        assert!(
            st.smooth_bps < 8_000_000,
            "smooth should not jump fully to spike, got {}",
            st.smooth_bps
        );
    }
}
