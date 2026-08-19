//! Compio full-duplex peer I/O: split halves + concurrent reader/writer + channel.
//!
//! ```text
//!   reader ──WriterMsg──► writer
//!     │                      │
//!  read().await         write / pump
//!  PIECE → staging      OutQueue / upload
//!  parse / download
//! ```
//!
//! Both futures run on the same Compio worker via [`futures::try_join`].
//!
//! **Wake model (K19):** inter-socket progress (hash, stall, Requests, rate-limit
//! sleep) never holds a Compio socket future across `select`. Socket park is a
//! single `read_some`. All BT frames, including PIECE, land in `read_buf` and
//! go through [`parse_available_messages`]. Writer idle select is only
//! cmd/HAVE/keepalive (no write future in select).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use compio::net::TcpStream;
use compio::time::{sleep, sleep_until, timeout};
use flume::{Receiver as FlumeReceiver, Sender as FlumeSender};
use futures::future::FutureExt;
use futures::{select_biased, try_join};

use crate::catalog::bitfield_size_bytes;
use crate::crypto::Rc4;
use crate::error::Result;
use crate::hot::{HotTorrent, PeerAvailability};
use crate::runtime::{
    adapt_pipeline, clamp_initial_pipeline, HashOutcome, PipelineAdaptOutcome, PipelineAdaptState,
    PipelineTuning, MIN_PIPELINE,
};
use crate::staging::StagingPool;
use crate::upload::UploadBlock;
use crate::wire::{encode_message, encode_possession_fast, FastSession, Message};

use super::super::net;
use super::config::PeerConfig;
use super::download::PeerDownload;
use super::established::{parse_available_messages, PURE_LEECH_EXIT_TIMEOUT};
use super::helpers::{
    clear_dl_queue, enqueue_have_messages, enqueue_have_messages_from, publish_am_interested,
    publish_dl_queue, publish_peer_choking, publish_upload_pending, KEEPALIVE_INTERVAL,
    REQUEST_STALL, REQUEST_STALL_ENDGAME,
};
use super::out_queue::OutProgress;
use super::send::PeerSend;

/// Commands from the read half → write half (ordered with encrypt/write).
#[derive(Debug)]
pub(crate) enum WriterMsg {
    /// Plain (pre-RC4) control frame bytes.
    Ctrl(Vec<u8>),
    /// Remote Request accepted for upload.
    Upload(UploadBlock),
    /// Remote Cancel for an upload block.
    Cancel(UploadBlock),
    /// Drop pending upload pieces (NotInterested).
    ClearPieces,
    /// Reader finished; writer should drain then exit.
    ReaderDone,
}

/// Outbound sink used by parse / download (channel or direct queue).
pub(crate) trait PeerOut {
    /// Take ownership of encoded control bytes (no clone of the bytes).
    fn push_ctrl_owned(&mut self, plain: Vec<u8>);
    /// Session encode scratch for Request/Have/Reject/… (reader half).
    fn ctrl_scratch(&mut self) -> &mut super::ctrl_scratch::CtrlScratch;
    fn try_push_piece(&mut self, block: UploadBlock) -> bool;
    fn cancel_piece(&mut self, block: UploadBlock) -> bool;
    fn clear_pieces(&mut self);
}

/// Channel-backed outbound (reader → writer).
pub(crate) struct OutCmd {
    tx: FlumeSender<WriterMsg>,
    /// Encode scratch for Interested / NotInterested / single-frame ctrl.
    ctrl: super::ctrl_scratch::CtrlScratch,
}

impl OutCmd {
    fn new(tx: FlumeSender<WriterMsg>) -> Self {
        Self {
            tx,
            ctrl: super::ctrl_scratch::CtrlScratch::new(),
        }
    }

    fn reader_done(&self) {
        let _ = self.tx.send(WriterMsg::ReaderDone);
    }

    fn push_interested(&mut self) {
        self.ctrl.clear();
        self.ctrl.append_interested();
        let frame = self.ctrl.take();
        self.push_ctrl_owned(frame);
    }

    fn push_not_interested(&mut self) {
        self.ctrl.clear();
        self.ctrl.append_not_interested();
        let frame = self.ctrl.take();
        self.push_ctrl_owned(frame);
    }
}

impl PeerOut for OutCmd {
    fn push_ctrl_owned(&mut self, plain: Vec<u8>) {
        if plain.is_empty() {
            return;
        }
        let _ = self.tx.send(WriterMsg::Ctrl(plain));
    }
    fn ctrl_scratch(&mut self) -> &mut super::ctrl_scratch::CtrlScratch {
        &mut self.ctrl
    }
    fn try_push_piece(&mut self, block: UploadBlock) -> bool {
        self.tx.send(WriterMsg::Upload(block)).is_ok()
    }
    fn cancel_piece(&mut self, block: UploadBlock) -> bool {
        self.tx.send(WriterMsg::Cancel(block)).is_ok()
    }
    fn clear_pieces(&mut self) {
        let _ = self.tx.send(WriterMsg::ClearPieces);
    }
}

/// Run full-duplex after the post-handshake hello has been written on `wr`.
///
/// Reader owns the socket half: hash/timer never cancel a Compio read.
pub(crate) async fn run_duplex(
    rd: TcpStream,
    wr: TcpStream,
    torrent: Arc<HotTorrent>,
    cfg: PeerConfig,
    encrypt: Option<Rc4>,
    decrypt: Option<Rc4>,
    initial_plain: Vec<u8>,
    peer_supports_fast: bool,
    allowed_to_peer: HashSet<u32>,
    have_rx: FlumeReceiver<u32>,
) -> Result<()> {
    let stop = cfg
        .stop
        .clone()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let stop_rx = cfg.stop_rx.clone();

    let (cmd_tx, cmd_rx) = flume::unbounded::<WriterMsg>();

    let want_download = cfg.allow_download && cfg.hash.is_some() && !torrent.is_download_complete();
    let pipe_tuning = PipelineTuning::with_max(cfg.pipeline_max.max(MIN_PIPELINE));
    let pipe_initial = clamp_initial_pipeline(cfg.pipeline, pipe_tuning.max);

    let reader = reader_loop(
        rd,
        Arc::clone(&torrent),
        cfg.clone(),
        decrypt,
        initial_plain,
        peer_supports_fast,
        allowed_to_peer,
        cmd_tx,
        Arc::clone(&stop),
        stop_rx.clone(),
        pipe_tuning,
        pipe_initial,
        want_download,
    );
    let writer = writer_loop(
        wr,
        torrent,
        cfg,
        encrypt,
        cmd_rx,
        have_rx,
        peer_supports_fast,
        stop,
        stop_rx,
    );

    match try_join!(reader, writer) {
        Ok(((), ())) => Ok(()),
        Err(e) => Err(e),
    }
}

async fn reader_loop(
    mut rd: TcpStream,
    torrent: Arc<HotTorrent>,
    cfg: PeerConfig,
    mut decrypt: Option<Rc4>,
    initial_plain: Vec<u8>,
    peer_supports_fast: bool,
    allowed_to_peer: HashSet<u32>,
    cmd_tx: FlumeSender<WriterMsg>,
    stop: Arc<AtomicBool>,
    stop_rx: Option<FlumeReceiver<()>>,
    pipe_tuning: PipelineTuning,
    pipe_initial: usize,
    want_download: bool,
) -> Result<()> {
    let mut out = OutCmd::new(cmd_tx);
    let mut peer_interested = false;
    let mut fast = FastSession::new(peer_supports_fast);
    fast.allowed_to_peer = allowed_to_peer;

    let staging = if want_download {
        torrent.set_staging_mem_limit(cfg.staging_mem_limit);
        torrent.ensure_staging_pool();
        match torrent.staging_pool() {
            Some(pool) => StagingPool::from_pool(pool),
            None => StagingPool::empty(torrent.layout().piece_length),
        }
    } else {
        StagingPool::empty(torrent.layout().piece_length)
    };
    let mut dl = PeerDownload::new(
        Arc::clone(&torrent),
        vec![0u8; bitfield_size_bytes(torrent.piece_count)],
        staging,
        if want_download { pipe_initial } else { 0 },
        if want_download {
            Some(PeerAvailability::new(Arc::clone(&torrent)))
        } else {
            None
        },
    );
    let mut read_buf = net::ReadCursor::from_vec(initial_plain);
    let mut scratch = Vec::with_capacity(WIRE_READ_CHUNK);
    let mut last_interested = Instant::now();
    let mut last_piece_at = Instant::now();
    let mut am_interested = want_download;
    let mut sent_not_interested = false;
    let mut pipe_state = PipelineAdaptState::new(pipe_initial, &pipe_tuning);
    let (hash_tx, hash_rx) = flume::unbounded::<HashOutcome>();
    let on_piece = cfg.on_piece.clone();

    publish_peer_choking(&cfg, true);
    publish_am_interested(&cfg, am_interested);
    if want_download {
        publish_dl_queue(&cfg, 0, dl.pipeline as u64);
    } else {
        clear_dl_queue(&cfg);
    }

    if want_download && dl.can_request() {
        let _ = dl.issue_requests(&mut out, &cfg);
        publish_dl_queue(&cfg, dl.outstanding(), dl.pipeline as u64);
    }

    let downloading0 = want_download && !torrent.is_download_complete();
    let _ = parse_available_messages(
        &mut read_buf,
        &mut dl,
        &mut out,
        &mut fast,
        &mut peer_interested,
        &mut last_piece_at,
        &torrent,
        &cfg,
        downloading0,
        &hash_tx,
    )?;

    'read: while !stop.load(Ordering::SeqCst) {
        let can_download = cfg.allow_download && cfg.hash.is_some();
        let downloading = can_download && !torrent.is_download_complete();

        if downloading && !am_interested {
            if dl.peer_avail.is_none() {
                dl.peer_avail = Some(PeerAvailability::new(Arc::clone(&torrent)));
                if let Some(ref mut a) = dl.peer_avail {
                    a.on_bitfield(&dl.peer_bf);
                }
            }
            dl.pipeline = pipe_initial;
            pipe_state = PipelineAdaptState::new(pipe_initial, &pipe_tuning);
            out.push_interested();
            last_interested = Instant::now();
            last_piece_at = Instant::now();
            am_interested = true;
            publish_am_interested(&cfg, true);
            sent_not_interested = false;
            dl.endgame = false;
            publish_dl_queue(&cfg, dl.outstanding(), dl.pipeline as u64);
            let _ = dl.issue_requests(&mut out, &cfg);
        }

        if am_interested && !downloading && !sent_not_interested {
            let _ = dl.cancel_outstanding(&mut out);
            dl.staging.clear();
            out.push_not_interested();
            am_interested = false;
            publish_am_interested(&cfg, false);
            sent_not_interested = true;
            clear_dl_queue(&cfg);
            if cfg.allow_upload {
                let bf = torrent.bitfield_snapshot();
                let possession = if peer_supports_fast {
                    encode_possession_fast(torrent.have_count(), torrent.piece_count, bf)
                } else {
                    encode_message(&Message::Bitfield(bf))
                };
                out.push_ctrl_owned(possession);
            }
            if !cfg.allow_upload {
                let exit_deadline = Instant::now() + PURE_LEECH_EXIT_TIMEOUT;
                while !dl.hashing.is_empty() {
                    let left = exit_deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    match timeout(left, hash_rx.recv_async()).await {
                        Ok(Ok(outcome)) => {
                            let _ = dl.apply_hash_outcome(outcome, on_piece.as_ref());
                            while let Ok(more) = hash_rx.try_recv() {
                                let _ = dl.apply_hash_outcome(more, on_piece.as_ref());
                            }
                        }
                        Ok(Err(_)) | Err(_) => break,
                    }
                }
                while let Ok(outcome) = hash_rx.try_recv() {
                    let _ = dl.apply_hash_outcome(outcome, on_piece.as_ref());
                }
                out.reader_done();
                break 'read;
            }
        }

        if !downloading && !cfg.allow_upload {
            while let Ok(outcome) = hash_rx.try_recv() {
                let _ = dl.apply_hash_outcome(outcome, on_piece.as_ref());
            }
            out.reader_done();
            break 'read;
        }

        if !downloading {
            clear_dl_queue(&cfg);
        }

        // Inter-socket progress: never holds a Compio socket future.
        let progress = reader_inter_socket(
            &mut dl,
            &mut out,
            &cfg,
            &torrent,
            &mut pipe_state,
            &pipe_tuning,
            &hash_rx,
            on_piece.as_ref(),
            &mut last_piece_at,
            &mut last_interested,
            am_interested,
            downloading,
            read_buf.has_complete_frame(),
        )
        .await;
        if progress.reloop {
            continue;
        }
        let mut need_fill = progress.need_fill;

        if read_buf.has_complete_frame() {
            need_fill |= parse_available_messages(
                &mut read_buf,
                &mut dl,
                &mut out,
                &mut fast,
                &mut peer_interested,
                &mut last_piece_at,
                &torrent,
                &cfg,
                downloading,
                &hash_tx,
            )?;
            read_buf.compact_if_needed();
            dl.refresh_outbound(&mut out, &cfg, downloading, need_fill);
            continue;
        }

        let outstanding = dl.outstanding();
        if outstanding == 0 && !dl.hashing.is_empty() && !read_buf.has_complete_frame() {
            let tick = Duration::from_secs(5);
            match timeout(tick, hash_rx.recv_async()).await {
                Ok(Ok(outcome)) => {
                    need_fill = dl.apply_hash_outcome(outcome, on_piece.as_ref());
                    while let Ok(more) = hash_rx.try_recv() {
                        if dl.apply_hash_outcome(more, on_piece.as_ref()) {
                            need_fill = true;
                        }
                    }
                    dl.refresh_outbound(&mut out, &cfg, downloading, need_fill);
                    continue;
                }
                Ok(Err(_)) => {
                    out.reader_done();
                    break 'read;
                }
                Err(_) => {
                    dl.refresh_outbound(&mut out, &cfg, downloading, need_fill);
                    continue;
                }
            }
        }

        // Sole Compio socket await. Incomplete frames (including mid-PIECE)
        // stay in read_buf; stall Cancels and re-Requests without dropping.
        let mut socket_need_fill = need_fill;
        let stall = if dl.endgame {
            REQUEST_STALL_ENDGAME
        } else {
            REQUEST_STALL
        };
        let stall_deadline = if downloading && dl.can_request() && dl.outstanding() > 0 {
            Some(last_piece_at + stall)
        } else {
            None
        };
        match read_some_until(
            &mut rd,
            &mut scratch,
            WIRE_READ_CHUNK,
            stall_deadline,
            stop_rx.as_ref(),
        )
        .await?
        {
            None => {
                let outstanding = dl.outstanding();
                if outstanding > 0 {
                    let _ = dl.cancel_outstanding(&mut out);
                    dl.staging.requeue_timed_out();
                }
                last_piece_at = Instant::now();
                dl.refresh_outbound(&mut out, &cfg, downloading, true);
                continue;
            }
            Some(0) => {
                out.reader_done();
                break 'read;
            }
            Some(n) => {
                read_buf.append(&scratch[..n], decrypt.as_mut());
            }
        }

        socket_need_fill |= parse_available_messages(
            &mut read_buf,
            &mut dl,
            &mut out,
            &mut fast,
            &mut peer_interested,
            &mut last_piece_at,
            &torrent,
            &cfg,
            downloading,
            &hash_tx,
        )?;
        read_buf.compact_if_needed();
        while let Ok(outcome) = hash_rx.try_recv() {
            if dl.apply_hash_outcome(outcome, on_piece.as_ref()) {
                socket_need_fill = true;
            }
        }
        dl.refresh_outbound(&mut out, &cfg, downloading, socket_need_fill);
    }

    out.reader_done();
    Ok(())
}

/// Result of [`reader_inter_socket`] (between Compio socket parks).
struct InterSocketProgress {
    need_fill: bool,
    /// True when progress already slept or otherwise wants another loop without socket.
    reloop: bool,
}

/// Hash drain, pipeline adapt, stall requeue, Request top-up, download rate sleep.
///
/// Must not await a Compio socket op (cancel-safe rule).
async fn reader_inter_socket(
    dl: &mut PeerDownload,
    out: &mut OutCmd,
    cfg: &PeerConfig,
    torrent: &HotTorrent,
    pipe_state: &mut PipelineAdaptState,
    pipe_tuning: &PipelineTuning,
    hash_rx: &FlumeReceiver<HashOutcome>,
    on_piece: Option<&Arc<dyn Fn(i64, u32, u32) + Send + Sync>>,
    last_piece_at: &mut Instant,
    last_interested: &mut Instant,
    am_interested: bool,
    downloading: bool,
    has_complete_frame: bool,
) -> InterSocketProgress {
    if downloading {
        let bytes = cfg
            .wire_down
            .as_ref()
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0);
        match adapt_pipeline(pipe_state, bytes, Instant::now(), pipe_tuning) {
            PipelineAdaptOutcome::Grew { pipeline } => {
                dl.pipeline = pipeline;
                publish_dl_queue(cfg, dl.outstanding(), dl.pipeline as u64);
                dl.refresh_outbound(out, cfg, downloading, true);
            }
            PipelineAdaptOutcome::Shrank { pipeline } => {
                dl.pipeline = pipeline;
                publish_dl_queue(cfg, dl.outstanding(), dl.pipeline as u64);
            }
            PipelineAdaptOutcome::Unchanged => {}
        }
        if !dl.endgame && torrent.should_endgame() {
            dl.endgame = true;
            dl.staging.enable_endgame();
            let outstanding = dl.outstanding();
            if outstanding > 0 {
                let _ = dl.cancel_outstanding(out);
            }
            dl.staging.requeue_timed_out();
            dl.refresh_outbound(out, cfg, downloading, true);
        }
        if am_interested && dl.peer_choking && last_interested.elapsed() > Duration::from_secs(45) {
            out.push_interested();
            *last_interested = Instant::now();
        }
    }

    let mut need_fill = false;
    while let Ok(outcome) = hash_rx.try_recv() {
        if dl.apply_hash_outcome(outcome, on_piece) {
            need_fill = true;
        }
    }

    if downloading {
        let stall = if dl.endgame {
            REQUEST_STALL_ENDGAME
        } else {
            REQUEST_STALL
        };
        if dl.can_request() && last_piece_at.elapsed() >= stall {
            let outstanding = dl.outstanding();
            if outstanding > 0 {
                let _ = dl.cancel_outstanding(out);
                dl.staging.requeue_timed_out();
            } else if dl.hashing.is_empty() {
                dl.staging.requeue_timed_out();
            }
            need_fill = outstanding > 0 || dl.hashing.is_empty();
            *last_piece_at = Instant::now();
        } else if dl.can_request() && need_fill {
            // keep need_fill
        } else if dl.can_request() && dl.outstanding() < dl.pipeline as u64 {
            need_fill = true;
        }
    }

    // Top-up Requests before any socket park (loopback deadlock otherwise).
    if downloading {
        dl.refresh_outbound(out, cfg, downloading, need_fill);
        need_fill = false;
    }

    // Download cap: sleep for tokens when nothing will wake a bare socket read.
    if downloading
        && dl.can_request()
        && dl.outstanding() == 0
        && dl.hashing.is_empty()
        && !has_complete_frame
    {
        if let Some(lim) = cfg.wire_limiter.as_ref() {
            let wait = lim.download_delay_for(crate::staging::BLOCK_SIZE as u64);
            if !wait.is_zero() {
                sleep(wait).await;
                return InterSocketProgress {
                    need_fill: false,
                    reloop: true,
                };
            }
        }
    }

    InterSocketProgress {
        need_fill,
        reloop: false,
    }
}

/// Socket `read_some` max. Covers several 16 KiB PIECE frames per recv.
const WIRE_READ_CHUNK: usize = 64 * 1024;

/// Compio `read_some` with optional stall deadline and optional stop-wake.
///
/// `None` = stall timeout. `Some(0)` = EOF **or** session stop.
/// Rate-limit sleeps do **not** use this.
async fn read_some_until(
    stream: &mut TcpStream,
    scratch: &mut Vec<u8>,
    max: usize,
    stall_deadline: Option<Instant>,
    stop_rx: Option<&FlumeReceiver<()>>,
) -> Result<Option<usize>> {
    if let Some(deadline) = stall_deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        if let Some(rx) = stop_rx {
            select_biased! {
                r = net::read_some(stream, scratch, max).fuse() => {
                    return Ok(Some(r?));
                }
                _ = rx.recv_async().fuse() => return Ok(Some(0)),
                _ = sleep(left).fuse() => return Ok(None),
            }
        }
        match timeout(left, net::read_some(stream, scratch, max)).await {
            Ok(r) => Ok(Some(r?)),
            Err(_) => Ok(None),
        }
    } else if let Some(rx) = stop_rx {
        select_biased! {
            r = net::read_some(stream, scratch, max).fuse() => Ok(Some(r?)),
            _ = rx.recv_async().fuse() => Ok(Some(0)),
        }
    } else {
        Ok(Some(net::read_some(stream, scratch, max).await?))
    }
}

async fn writer_loop(
    mut wr: TcpStream,
    torrent: Arc<HotTorrent>,
    cfg: PeerConfig,
    mut encrypt: Option<Rc4>,
    cmd_rx: FlumeReceiver<WriterMsg>,
    mut have_rx: FlumeReceiver<u32>,
    fast_enabled: bool,
    stop: Arc<AtomicBool>,
    stop_rx: Option<FlumeReceiver<()>>,
) -> Result<()> {
    let mut send = PeerSend::new();
    publish_upload_pending(&cfg, 0);
    let mut reader_done = false;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        while let Ok(msg) = cmd_rx.try_recv() {
            if apply_writer_msg(&mut send, &cfg, fast_enabled, msg) {
                reader_done = true;
            }
        }
        let _ = enqueue_have_messages(&mut send.out, &mut send.ctrl, &mut have_rx);

        if send.out.can_progress_write() {
            // Allow finishing in-flight work after reader is done.
            let allow_up = cfg.allow_upload || send.out.has_work();
            let outcome = send
                .pump(
                    &mut wr,
                    &torrent,
                    &cfg,
                    encrypt.as_mut(),
                    allow_up,
                    fast_enabled,
                )
                .await?;
            if let OutProgress::RateLimited(wait) = outcome.kind {
                // Plain sleep only — do not race stop_rx on the rate-limit hot path.
                sleep(wait).await;
            }
            if reader_done && !send.out.has_work() {
                break;
            }
            continue;
        }

        if reader_done && !send.out.has_work() {
            break;
        }

        // Idle: wait for cmd / HAVE / keepalive / stop (no Compio socket; no rate sleep).
        let ka_at = send.last_send_at + KEEPALIVE_INTERVAL;
        if let Some(rx) = stop_rx.as_ref() {
            select_biased! {
                msg = cmd_rx.recv_async().fuse() => {
                    match msg {
                        Ok(m) => {
                            if apply_writer_msg(&mut send, &cfg, fast_enabled, m) {
                                reader_done = true;
                            }
                        }
                        Err(_) => reader_done = true,
                    }
                }
                idx = have_rx.recv_async().fuse() => {
                    if let Ok(first) = idx {
                        let _ = enqueue_have_messages_from(
                            &mut send.out,
                            &mut send.ctrl,
                            &mut have_rx,
                            Some(first),
                        );
                    }
                }
                _ = sleep_until(ka_at).fuse() => {
                    if send.last_send_at.elapsed() >= KEEPALIVE_INTERVAL {
                        send.out.push_keepalive(&mut send.ctrl);
                    }
                }
                _ = rx.recv_async().fuse() => break,
            }
        } else {
            select_biased! {
                msg = cmd_rx.recv_async().fuse() => {
                    match msg {
                        Ok(m) => {
                            if apply_writer_msg(&mut send, &cfg, fast_enabled, m) {
                                reader_done = true;
                            }
                        }
                        Err(_) => reader_done = true,
                    }
                }
                idx = have_rx.recv_async().fuse() => {
                    if let Ok(first) = idx {
                        let _ = enqueue_have_messages_from(
                            &mut send.out,
                            &mut send.ctrl,
                            &mut have_rx,
                            Some(first),
                        );
                    }
                }
                _ = sleep_until(ka_at).fuse() => {
                    if send.last_send_at.elapsed() >= KEEPALIVE_INTERVAL {
                        send.out.push_keepalive(&mut send.ctrl);
                    }
                }
            }
        }
    }

    // Best-effort final flush of control.
    let deadline = Instant::now() + Duration::from_secs(2);
    while send.out.has_work() && Instant::now() < deadline {
        let outcome = send
            .pump(
                &mut wr,
                &torrent,
                &cfg,
                encrypt.as_mut(),
                false,
                fast_enabled,
            )
            .await?;
        if matches!(
            outcome.kind,
            OutProgress::Idle | OutProgress::RateLimited(_)
        ) {
            break;
        }
    }
    Ok(())
}

/// Apply one writer command. Returns true if `ReaderDone`.
fn apply_writer_msg(
    send: &mut PeerSend,
    cfg: &PeerConfig,
    fast_enabled: bool,
    msg: WriterMsg,
) -> bool {
    match msg {
        WriterMsg::Ctrl(b) => {
            send.out.push_ctrl_owned(b);
            false
        }
        WriterMsg::Upload(block) => {
            if !send.out.try_push_piece(block) && fast_enabled {
                send.ctrl.clear();
                send.ctrl
                    .append_reject_request(block.index, block.begin, block.length);
                let frame = send.ctrl.take();
                send.out.push_ctrl_owned(frame);
            }
            publish_upload_pending(cfg, send.out.piece_pending_count());
            false
        }
        WriterMsg::Cancel(block) => {
            if send.out.cancel_piece(block) {
                publish_upload_pending(cfg, send.out.piece_pending_count());
            }
            false
        }
        WriterMsg::ClearPieces => {
            send.out.clear_pieces();

            publish_upload_pending(cfg, send.out.piece_pending_count());
            false
        }
        WriterMsg::ReaderDone => true,
    }
}
