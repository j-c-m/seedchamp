//! DiskWorker write throughput only (no peers, hash, network, or seed reads).
//!
//! Piece buffers use a per-index xorshift fill (high entropy) so ZFS/lz4 cannot
//! collapse repeated bytes. One cell per process; expand via `bench/diskworker.py`.
//!
//! ```text
//! cargo run -p seedchamp-engine --example disk_write_bench --release -- \
//!   --backend thread --path durable --size 256M --piece-length 1M --depth 32
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use seedchamp_engine::disk::{ensure_storage, FileLayout, StorageLayout};
use seedchamp_engine::runtime::{DiskWorker, DiskWriteJob, HashOutcome, DEFAULT_DISK_DEPTH};

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("disk_write_bench: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<(), String> {
    let args = Args::parse(std::env::args().skip(1))?;
    let piece_len = args.piece_length;
    if piece_len == 0 || piece_len > u32::MAX as u64 {
        return Err(format!("bad --piece-length {piece_len}"));
    }
    let plen_u32 = piece_len as u32;
    let n_pieces = (args.size / piece_len).max(1) as u32;
    let total_write = n_pieces as u64 * piece_len;

    let work = args.work.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("seedchamp-disk-write-bench-{}", std::process::id()))
    });
    std::fs::create_dir_all(&work).map_err(|e| format!("mkdir {}: {e}", work.display()))?;
    let cell_dir = work.join(format!(
        "{}-{}-d{}-p{}",
        args.backend,
        args.path.as_str(),
        args.depth,
        piece_len
    ));
    if cell_dir.exists() {
        let _ = std::fs::remove_dir_all(&cell_dir);
    }
    std::fs::create_dir_all(&cell_dir).map_err(|e| format!("mkdir {}: {e}", cell_dir.display()))?;

    let layout = Arc::new(build_layout(&cell_dir, plen_u32, n_pieces, args.layout)?);

    let discard = args.path == PathMode::Discard;
    if !discard {
        ensure_storage(&layout).map_err(|e| format!("ensure_storage: {e}"))?;
    }

    let worker = DiskWorker::spawn_with_options(discard, &args.backend, args.depth)
        .map_err(|e| format!("spawn DiskWorker: {e}"))?;
    let backend_resolved = worker.backend_name();

    // Warmup: fill the pipeline once (does not count toward timed metrics).
    let warm_n = (args.depth as u32).max(1).min(n_pieces);
    run_pass(&worker, &layout, plen_u32, 0, warm_n, args.depth)?;

    let t0 = Instant::now();
    let latencies = run_pass(&worker, &layout, plen_u32, 0, n_pieces, args.depth)?;
    let elapsed = t0.elapsed();
    // Drop worker so the disk thread exits before we remove work (durable).
    drop(worker);

    let elapsed_s = elapsed.as_secs_f64().max(1e-9);
    let rate_mbps = (total_write as f64 / 1_000_000.0) / elapsed_s;
    let (p50_us, p99_us) = percentile_us(&latencies);

    // Machine-readable result line (wrapper / scripts parse this).
    println!(
        "backend={backend_resolved} want={} path={} depth={} piece={piece_len} layout={} \
         pieces={n_pieces} written={total_write} elapsed_s={elapsed_s:.4} rate_MBps={rate_mbps:.2} \
         p50_us={p50_us} p99_us={p99_us} status=ok work={}",
        args.backend,
        args.path.as_str(),
        args.depth,
        args.layout.as_str(),
        cell_dir.display()
    );

    if !args.keep_work {
        let _ = std::fs::remove_dir_all(&cell_dir);
        if work
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
        {
            let _ = std::fs::remove_dir(&work);
        }
    }
    Ok(())
}

/// Submit pieces [start, start+count) with up to `depth` in flight.
/// Returns per-completion latencies (submit → outcome) in order of completion.
fn run_pass(
    worker: &DiskWorker,
    layout: &Arc<StorageLayout>,
    plen: u32,
    start: u32,
    count: u32,
    depth: usize,
) -> Result<Vec<Duration>, String> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let (tx, rx) = flume::unbounded();
    let mut free: Vec<Vec<u8>> = (0..depth.max(1))
        .map(|_| vec![0u8; plen as usize])
        .collect();
    // index -> submit Instant (unique indices in one pass)
    let mut submitted_at: Vec<Option<Instant>> = vec![None; (start + count) as usize];
    let mut latencies = Vec::with_capacity(count as usize);

    let mut next = start;
    let end = start + count;
    let mut inflight = 0usize;
    let mut done = 0u32;

    while done < count {
        while inflight < depth && next < end {
            let mut data = free.pop().unwrap_or_else(|| vec![0u8; plen as usize]);
            let index = next;
            next += 1;
            fill_incompressible(&mut data, index);
            submitted_at[index as usize] = Some(Instant::now());
            if let Err((e, job)) = worker.submit_write(DiskWriteJob {
                index,
                plen,
                data,
                layout: Arc::clone(layout),
                reply: tx.clone(),
            }) {
                free.push(job.data);
                return Err(format!("submit_write piece {index}: {e}"));
            }
            inflight += 1;
        }

        let outcome = rx
            .recv_timeout(Duration::from_secs(120))
            .map_err(|_| "timeout waiting for DiskWorker outcome (120s)".to_string())?;
        match outcome {
            HashOutcome::Ok { index, data, .. } => {
                if let Some(t) = submitted_at.get(index as usize).and_then(|s| *s) {
                    latencies.push(t.elapsed());
                }
                free.push(data);
                inflight = inflight.saturating_sub(1);
                done += 1;
            }
            HashOutcome::WriteFail { index, data, .. } => {
                free.push(data);
                return Err(format!("WriteFail piece {index}"));
            }
            HashOutcome::HashFail { index, data, .. } => {
                free.push(data);
                return Err(format!("unexpected HashFail piece {index}"));
            }
        }
    }
    Ok(latencies)
}

fn percentile_us(samples: &[Duration]) -> (u64, u64) {
    if samples.is_empty() {
        return (0, 0);
    }
    let mut v: Vec<u64> = samples.iter().map(|d| d.as_micros() as u64).collect();
    v.sort_unstable();
    let p = |pct: f64| -> u64 {
        let i = ((v.len() as f64 - 1.0) * pct).round() as usize;
        v[i.min(v.len() - 1)]
    };
    (p(0.50), p(0.99))
}

/// High-entropy piece body (not zeros / not a fixed byte). Seeded by piece index
/// so pieces differ and FS compression/dedup cannot collapse the stream.
fn fill_incompressible(buf: &mut [u8], piece_index: u32) {
    // SplitMix64-ish seed from index; never zero (xorshift needs non-zero state).
    let mut state = (u64::from(piece_index).wrapping_add(1)).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ 0xA076_1D64_78BD_642F;
    if state == 0 {
        state = 0xDEAD_BEEF_CAFE_BABE;
    }
    let mut off = 0usize;
    while off + 8 <= buf.len() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        buf[off..off + 8].copy_from_slice(&state.to_le_bytes());
        off += 8;
    }
    if off < buf.len() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let tail = state.to_le_bytes();
        let n = buf.len() - off;
        buf[off..].copy_from_slice(&tail[..n]);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PathMode {
    Durable,
    Discard,
}

impl PathMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Durable => "durable",
            Self::Discard => "discard",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayoutKind {
    Single,
    Multi,
}

impl LayoutKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Multi => "multi",
        }
    }
}

struct Args {
    backend: String,
    path: PathMode,
    size: u64,
    piece_length: u64,
    depth: usize,
    layout: LayoutKind,
    work: Option<PathBuf>,
    keep_work: bool,
}

impl Args {
    fn parse(argv: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut backend = "auto".to_string();
        let mut path = PathMode::Durable;
        let mut size = 256 * 1024 * 1024u64;
        let mut piece_length = 1024 * 1024u64;
        let mut depth = DEFAULT_DISK_DEPTH;
        let mut layout = LayoutKind::Single;
        let mut work = None;
        let mut keep_work = false;

        let mut it = argv.into_iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "--backend" => {
                    backend = it
                        .next()
                        .ok_or_else(|| "--backend needs a value".to_string())?;
                }
                "--path" => {
                    let v = it
                        .next()
                        .ok_or_else(|| "--path needs a value".to_string())?;
                    path = match v.as_str() {
                        "durable" => PathMode::Durable,
                        "discard" => PathMode::Discard,
                        other => {
                            return Err(format!("--path {other:?} (durable|discard)"));
                        }
                    };
                }
                "--size" => {
                    let v = it
                        .next()
                        .ok_or_else(|| "--size needs a value".to_string())?;
                    size = parse_size(&v)?;
                }
                "--piece-length" => {
                    let v = it
                        .next()
                        .ok_or_else(|| "--piece-length needs a value".to_string())?;
                    piece_length = parse_size(&v)?;
                }
                "--depth" => {
                    let v = it
                        .next()
                        .ok_or_else(|| "--depth needs a value".to_string())?;
                    depth = v.parse().map_err(|_| format!("bad --depth {v}"))?;
                    if depth == 0 {
                        return Err("--depth must be >= 1".into());
                    }
                }
                "--layout" => {
                    let v = it
                        .next()
                        .ok_or_else(|| "--layout needs a value".to_string())?;
                    layout = match v.as_str() {
                        "single" => LayoutKind::Single,
                        "multi" => LayoutKind::Multi,
                        other => {
                            return Err(format!("--layout {other:?} (single|multi)"));
                        }
                    };
                }
                "--work" => {
                    let v = it
                        .next()
                        .ok_or_else(|| "--work needs a value".to_string())?;
                    work = Some(PathBuf::from(v));
                }
                "--keep-work" => keep_work = true,
                other if other.starts_with('-') => {
                    return Err(format!("unknown flag {other} (try --help)"));
                }
                other => return Err(format!("unexpected arg {other}")),
            }
        }

        if size < piece_length {
            size = piece_length;
        }
        // Whole pieces only.
        size = (size / piece_length) * piece_length;

        Ok(Self {
            backend,
            path,
            size,
            piece_length,
            depth,
            layout,
            work,
            keep_work,
        })
    }
}

fn print_help() {
    eprintln!(
        "\
disk_write_bench — DiskWorker write throughput (no peers/hash/network)

Options:
  --backend auto|thread|uring|aio   (default: auto)
  --path durable|discard            (default: durable)
  --size SIZE                       total bytes written (default: 256M)
  --piece-length SIZE               (default: 1M)
  --depth N                         in-flight piece jobs (default: {DEFAULT_DISK_DEPTH})
  --layout single|multi             single file, or dual-span every piece (default: single)
  --work DIR                        work directory (default: $TMPDIR/…)
  --keep-work                       leave payload files after run
  -h, --help

SIZE: bare bytes or 64K / 1M / 2G (binary units).
One cell per process; use bench/diskworker.py for matrices."
    );
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim().to_ascii_uppercase().replace(' ', "");
    if s.is_empty() {
        return Err("empty size".into());
    }
    let (num, mult) = if let Some(rest) = s.strip_suffix("TIB").or_else(|| s.strip_suffix('T')) {
        (rest, 1024u64.pow(4))
    } else if let Some(rest) = s.strip_suffix("GIB").or_else(|| s.strip_suffix('G')) {
        (rest, 1024u64.pow(3))
    } else if let Some(rest) = s.strip_suffix("MIB").or_else(|| s.strip_suffix('M')) {
        (rest, 1024u64.pow(2))
    } else if let Some(rest) = s.strip_suffix("KIB").or_else(|| s.strip_suffix('K')) {
        (rest, 1024u64)
    } else if let Some(rest) = s.strip_suffix('B') {
        (rest, 1u64)
    } else {
        (s.as_str(), 1u64)
    };
    let n: u64 = num.parse().map_err(|_| format!("bad size {s:?}"))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("size overflow {s:?}"))
}

fn build_layout(
    root: &Path,
    piece_length: u32,
    piece_count: u32,
    kind: LayoutKind,
) -> Result<StorageLayout, String> {
    if piece_count == 0 {
        return Err("piece_count is 0".into());
    }
    let plen = piece_length as u64;
    let total_size = plen * piece_count as u64;
    let files = match kind {
        LayoutKind::Single => vec![FileLayout {
            path: PathBuf::from("payload.bin"),
            size: total_size,
            offset: 0,
            priority: 1,
        }],
        // Every piece is two half-piece files → multi-span write every time.
        LayoutKind::Multi => {
            if piece_length < 2 || !piece_length.is_multiple_of(2) {
                return Err("--layout multi needs even --piece-length >= 2".into());
            }
            let half = plen / 2;
            let mut files = Vec::with_capacity(piece_count as usize * 2);
            for i in 0..piece_count {
                let base = i as u64 * plen;
                files.push(FileLayout {
                    path: PathBuf::from(format!("p{i:06}.a")),
                    size: half,
                    offset: base,
                    priority: 1,
                });
                files.push(FileLayout {
                    path: PathBuf::from(format!("p{i:06}.b")),
                    size: half,
                    offset: base + half,
                    priority: 1,
                });
            }
            files
        }
    };
    Ok(StorageLayout {
        data_root: root.to_path_buf(),
        piece_length,
        piece_count,
        total_size,
        files,
    })
}
