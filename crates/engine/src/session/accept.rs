//! Inbound TCP accept on the Compio accept runtime (`seedchamp-acc`).
//!
//! Same shape as a Compio webserver — completion engine drives the wait:
//!
//! ```ignore
//! let listener = TcpListener::bind(addr).await?;
//! loop {
//!     let (stream, peer) = listener.accept().await?;
//!     pool.spawn_peer(|| async move { handle(stream).await });
//! }
//! ```
//!
//! Least-peers workers are other Compio threads; `TcpStream` is `!Send`, so the
//! accepted socket is handed off as `std::net::TcpStream` and re-attached with
//! [`TcpStream::from_std`] on the peer worker.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use compio::buf::IntoInner;
use compio::driver::SharedFd;
use compio::net::{TcpListener, TcpStream};
use compio::time::sleep;
use futures::future::{self, Either};
use parking_lot::Mutex;
use socket2::Socket as Socket2;

use crate::peer::{run_inbound_peer, PeerConfig};
use crate::runtime::PeerWorkerPool;

use super::snapshot::PeerCrypto;
use super::{Inner, LivePeer, PeerDirection, TorrentBytes};

/// Detach the OS socket for cross-thread handoff (`TcpStream` is `!Send`).
fn into_std_stream(stream: TcpStream) -> io::Result<std::net::TcpStream> {
    let poll = stream.into_poll_fd()?;
    let shared: SharedFd<Socket2> = poll.into_inner();
    let sock2 = shared
        .try_unwrap()
        .map_err(|_| io::Error::other("SharedFd still has clones; cannot hand off"))?;
    Ok(sock2.into())
}

/// First-completes `a` or `b` (futures select — not Tokio).
async fn race<A, B>(a: A, b: B) -> Either<A::Output, B::Output>
where
    A: Future,
    B: Future,
{
    let a = pin!(a);
    let b = pin!(b);
    match future::select(a, b).await {
        Either::Left((out, _)) => Either::Left(out),
        Either::Right((out, _)) => Either::Right(out),
    }
}

/// Compio completion accept loop. Parks on `listener.accept()` until a connection
/// arrives (or session cancel disconnects).
pub(super) async fn accept_loop(
    inner: Arc<Inner>,
    pool: Arc<PeerWorkerPool>,
    cancel: flume::Receiver<()>,
) {
    let listener = match TcpListener::bind(inner.cfg.listen).await {
        Ok(l) => l,
        Err(e) => {
            *inner.status.write() = format!("bind {}: {e}", inner.cfg.listen);
            tracing::error!(listen = %inner.cfg.listen, error = %e, "bind failed");
            return;
        }
    };
    *inner.status.write() = format!("listening {}", inner.cfg.listen);
    tracing::info!(
        listen = %inner.cfg.listen,
        workers = pool.workers(),
        "accept listening (compio TcpListener::accept + least-peers)"
    );

    loop {
        if inner.stop.load(Ordering::SeqCst) {
            break;
        }

        // Completion wait: Compio parks until accept completes or cancel drops.
        // (Same idea as the webserver loop; cancel only for clean session stop.)
        let accepted = race(cancel.recv_async(), listener.accept()).await;
        let (stream, addr) = match accepted {
            Either::Left(_) => break,
            Either::Right(Ok(pair)) => pair,
            Either::Right(Err(e)) => {
                tracing::warn!(error = %e, "accept");
                if matches!(
                    race(cancel.recv_async(), sleep(Duration::from_millis(200))).await,
                    Either::Left(_)
                ) {
                    break;
                }
                continue;
            }
        };

        let std_stream = match into_std_stream(stream) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%addr, error = %e, "accept handoff failed");
                continue;
            }
        };

        // Peer runs on a least-peers Compio worker (not this accept task).
        if let Err(e) = spawn_inbound(&inner, &pool, std_stream, addr) {
            tracing::warn!(%addr, error = %e, "spawn_peer failed");
        }
    }
}

fn spawn_inbound(
    inner: &Arc<Inner>,
    pool: &PeerWorkerPool,
    std_stream: std::net::TcpStream,
    addr: SocketAddr,
) -> crate::error::Result<()> {
    let max_conn = inner.cfg.max_connections.max(1);
    let n_peers = inner.peers.read().len();
    if n_peers >= max_conn {
        tracing::debug!(
            %addr,
            n_peers,
            max_conn,
            "inbound refused — max_connections"
        );
        drop(std_stream);
        return Ok(());
    }

    let send_buf = inner.cfg.send_buffer_bytes;
    let recv_buf = inner.cfg.recv_buffer_bytes;
    let reg = inner.registry.clone();
    let inner2 = Arc::clone(inner);
    let peer_up = Arc::new(AtomicU64::new(0));
    let peer_down = Arc::new(AtomicU64::new(0));
    let on_up = {
        let i = Arc::clone(inner);
        let peer_up = peer_up.clone();
        Arc::new(move |tid: i64, n: u64| {
            peer_up.fetch_add(n, Ordering::Relaxed);
            let mut map = i.torrent_bytes.write();
            let e = map.entry(tid).or_insert_with(|| {
                Arc::new(TorrentBytes {
                    up: AtomicU64::new(0),
                })
            });
            e.up.fetch_add(n, Ordering::Relaxed);
        }) as Arc<dyn Fn(i64, u64) + Send + Sync>
    };

    pool.spawn_peer(move || {
        let peer_up = peer_up;
        let peer_down = peer_down;
        let on_up = on_up;
        let reg = reg;
        let inner2 = inner2;
        async move {
            // Re-attach on the peer worker's Compio runtime.
            let stream = match TcpStream::from_std(std_stream) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(%addr, error = %e, "compio from_std failed");
                    return;
                }
            };
            crate::net::apply_socket_buffers(&stream, send_buf, recv_buf);

            let pid = inner2.next_peer_id.fetch_add(1, Ordering::Relaxed);
            let on_bound = {
                let inner_b = Arc::clone(&inner2);
                Arc::new(move |tid: i64, name: String| -> bool {
                    let max = inner_b.max_peers.load(Ordering::Relaxed).max(1);
                    let mut peers = inner_b.peers.write();
                    let n = peers.values().filter(|p| p.torrent_id == tid).count();
                    if n >= max {
                        return false;
                    }
                    if let Some(p) = peers.get_mut(&pid) {
                        p.torrent_id = tid;
                        p.torrent_name = name;
                    }
                    true
                }) as Arc<dyn Fn(i64, String) -> bool + Send + Sync>
            };
            let interested = Arc::new(AtomicBool::new(false));
            let peer_choking = Arc::new(AtomicBool::new(true));
            let am_interested = Arc::new(AtomicBool::new(false));
            let up_pend = Arc::new(AtomicU64::new(0));
            let peer_have = Arc::new(AtomicU32::new(0));
            let piece_count = Arc::new(AtomicU32::new(0));
            let crypto = Arc::new(AtomicU8::new(PeerCrypto::Unknown as u8));
            let client_label = Arc::new(Mutex::new(String::new()));
            let q_out = Arc::new(AtomicU64::new(0));
            let q_tgt = Arc::new(AtomicU64::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let (stop_tx, stop_rx) = flume::bounded::<()>(0);
            let on_piece = {
                let i = Arc::clone(&inner2);
                Arc::new(move |tid, idx, len| {
                    i.queue_piece_have(tid, idx, len);
                }) as Arc<dyn Fn(i64, u32, u32) + Send + Sync>
            };
            inner2.peers.write().insert(
                pid,
                LivePeer {
                    id: pid,
                    torrent_id: 0,
                    torrent_name: "…".into(),
                    addr,
                    direction: PeerDirection::Inbound,
                    wire_up: Some(peer_up),
                    wire_down: Some(peer_down.clone()),
                    connected_at: Instant::now(),
                    cancel: stop.clone(),
                    stop_tx: Mutex::new(Some(stop_tx)),
                    queue_outstanding: q_out.clone(),
                    queue_target: q_tgt.clone(),
                    peer_interested: interested.clone(),
                    peer_choking: peer_choking.clone(),
                    am_interested: am_interested.clone(),
                    upload_pending: up_pend.clone(),
                    peer_have: peer_have.clone(),
                    piece_count: piece_count.clone(),
                    crypto: crypto.clone(),
                    client_label: client_label.clone(),
                    listen_port: None,
                },
            );
            let pcfg = PeerConfig {
                peer_id: inner2.peer_id,
                encryption: inner2.cfg.encryption,
                upload: inner2.cfg.upload,
                allow_upload: true,
                allow_download: true,
                pipeline: inner2.cfg.pipeline,
                pipeline_max: inner2.cfg.pipeline_max,
                staging_mem_limit: inner2.cfg.staging_mem_limit,
                hash: Some(inner2.hash.clone()),
                on_piece: Some(on_piece),
                stop: Some(stop.clone()),
                stop_rx: Some(stop_rx),
                on_bound: Some(on_bound),
                piece_count: Some(piece_count),
                wire_up: None,
                wire_down: Some(peer_down),
                on_upload: Some(on_up),
                queue_outstanding: Some(q_out),
                queue_target: Some(q_tgt),
                peer_interested: Some(interested),
                peer_choking: Some(peer_choking),
                am_interested: Some(am_interested),
                upload_pending: Some(up_pend),
                peer_have: Some(peer_have),
                crypto: Some(crypto),
                client_label: Some(client_label),
                ltep_client: inner2.cfg.ltep_client.clone(),
                listen_port: inner2.cfg.listen.port(),
                redundant_seed_idle: Duration::from_secs(inner2.cfg.redundant_seed_idle_secs),
                send_buffer_bytes: 0,
                recv_buffer_bytes: 0,
                wire_limiter: Some(inner2.wire_limiter.clone()),
            };
            inner2.peer_connects.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = run_inbound_peer(stream, reg, pcfg).await {
                tracing::debug!(%addr, error = %e, "inbound end");
            }
            inner2.peer_disconnects.fetch_add(1, Ordering::Relaxed);
            stop.store(true, Ordering::SeqCst);
            inner2.peers.write().remove(&pid);
            inner2.peer_rate_state.lock().remove(&pid);
        }
    })
    .map(|_| ())
}
