//! Runtime I/O topology: peer worker pool, disk worker, hash pool, pipeline knobs.

pub mod disk_worker;
pub mod hash_worker;
pub mod pipeline;
pub mod pool;
pub mod recheck_pool;

pub use disk_worker::{
    DiskBackendKind, DiskWorker, DiskWriteJob, DEFAULT_DISK_DEPTH, DISK_WORKER_DEAD_STATUS,
    MAX_DISK_RESTARTS,
};
pub use hash_worker::{
    default_hash_workers, HashJob, HashOutcome, HashPool, RecheckJob, RecheckPieceResult,
};
pub use pipeline::{
    adapt_pipeline, clamp_initial_pipeline, desired_pipeline_blocks, PipelineAdaptOutcome,
    PipelineAdaptState, PipelineTuning, DEFAULT_PIPELINE, MAX_PIPELINE, MIN_PIPELINE,
    REQUEST_QUEUE_TIME_SECS,
};
pub use pool::{default_peer_workers, PeerWorkerPool};
pub use recheck_pool::recheck_torrent_with_pool;
