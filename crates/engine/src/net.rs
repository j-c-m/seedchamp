//! Peer TCP helpers on **Compio completion I/O** (design K19).
//!
//! Use Compio like the examples: `accept` / `read` / `write` / `write_all`.
//! Full-duplex peer tasks race those futures with channels/timers via
//! `futures` select — not Tokio, not a homemade `nix` try_* reactor.

use std::time::Duration;

use compio::buf::{BufResult, IntoInner, IoBuf};
use compio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use compio::net::TcpStream;
use compio::time::timeout;
use socket2::SockRef;

use crate::crypto::Rc4;
use crate::error::{Error, Result};
use crate::wire::MAX_MESSAGE_LENGTH;

/// Socket `read_some` max. Matches the experimental pipeline lowat cap.
pub const WIRE_READ_CHUNK: usize = 256 * 1024;

/// Do not raise `SO_RCVLOWAT` for a smaller remainder (setsockopt not worth it).
const RCVLOWAT_MIN: usize = 4 * 1024;

/// Experimental cap: `min(N × 16KiB, SO_RCVBUF/2, this)`.
const PIPELINE_LOWAT_CAP: usize = 256 * 1024;

/// Apply `SO_SNDBUF` / `SO_RCVBUF` when non-zero.
///
/// **0** leaves the kernel default (recommended). Non-zero is best-effort
/// `setsockopt` only — failures are ignored; platforms may clamp or reject
/// oversized requests. The kernel may double the requested size for bookkeeping.
pub fn apply_socket_buffers(stream: &impl std::os::fd::AsFd, send_bytes: u64, recv_bytes: u64) {
    if send_bytes == 0 && recv_bytes == 0 {
        return;
    }
    let sock = SockRef::from(stream);
    if send_bytes > 0 {
        let _ = sock.set_send_buffer_size(send_bytes.min(usize::MAX as u64) as usize);
    }
    if recv_bytes > 0 {
        let _ = sock.set_recv_buffer_size(recv_bytes.min(usize::MAX as u64) as usize);
    }
}

/// Compio `write_all` of an **owned** buffer (no clone).
///
/// Returns the buffer after a successful write. On I/O error returns `(Error, buf)`
/// so callers can restore upload scratch / requeue.
pub async fn write_all_owned(
    stream: &mut TcpStream,
    buf: Vec<u8>,
) -> std::result::Result<Vec<u8>, (Error, Vec<u8>)> {
    if buf.is_empty() {
        return Ok(buf);
    }
    let BufResult(r, buf) = stream.write_all(buf).await;
    match r {
        Ok(()) => Ok(buf),
        Err(e) => Err((Error::Msg(format!("write: {e}")), buf)),
    }
}

/// Compio `write_all` of `buf[..len]` only, keeping the full `Vec` capacity/len
/// (no `truncate` / `resize` churn on the upload scratch).
///
/// On success or error, returns the original buffer with length restored to
/// whatever it was before the call (typically [`crate::upload::UPLOAD_SCRATCH_LEN`]).
pub async fn write_all_owned_prefix(
    stream: &mut TcpStream,
    buf: Vec<u8>,
    len: usize,
) -> std::result::Result<Vec<u8>, (Error, Vec<u8>)> {
    if len == 0 {
        return Ok(buf);
    }
    if len > buf.len() {
        return Err((
            Error::Msg(format!(
                "write_all_owned_prefix len {len} past buf {}",
                buf.len()
            )),
            buf,
        ));
    }
    let full_len = buf.len();
    // Write only the prefix via IoBuf Slice; parent capacity/len stay intact.
    let slice = buf.slice(..len);
    let BufResult(r, slice) = stream.write_all(slice).await;
    let mut buf = slice.into_inner();
    if buf.len() != full_len {
        // Defensive: write path should not change len; restore if a driver does.
        buf.resize(full_len, 0);
    }
    match r {
        Ok(()) => Ok(buf),
        Err(e) => Err((Error::Msg(format!("write: {e}")), buf)),
    }
}

/// Compio `write_all` from a slice (clones into an owned IoBuf). Prefer
/// [`write_all_owned`] on hot paths that already own a `Vec`.
pub async fn write_all(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write_all_owned(stream, data.to_vec())
        .await
        .map(|_| ())
        .map_err(|(e, _)| e)
}

pub async fn write_all_crypto(
    stream: &mut TcpStream,
    data: &[u8],
    encrypt: Option<&mut Rc4>,
) -> Result<()> {
    if let Some(c) = encrypt {
        let mut buf = data.to_vec();
        c.crypt_inplace(&mut buf);
        write_all_owned(stream, buf)
            .await
            .map(|_| ())
            .map_err(|(e, _)| e)
    } else {
        write_all(stream, data).await
    }
}

/// Append a socket read into `read_buf`, decrypting RC4 **in place** on the new bytes.
pub fn append_wire_read(read_buf: &mut Vec<u8>, plain_or_cipher: &[u8], decrypt: Option<&mut Rc4>) {
    if plain_or_cipher.is_empty() {
        return;
    }
    if let Some(c) = decrypt {
        let start = read_buf.len();
        read_buf.extend_from_slice(plain_or_cipher);
        c.crypt_inplace(&mut read_buf[start..]);
    } else {
        read_buf.extend_from_slice(plain_or_cipher);
    }
}

/// Peer read buffer with a consume cursor (avoids per-message `drain` memmove).
pub struct ReadCursor {
    buf: Vec<u8>,
    pos: usize,
}

impl ReadCursor {
    pub fn from_vec(buf: Vec<u8>) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn unparsed(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    pub fn advance(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.buf.len());
    }

    pub fn compact_if_needed(&mut self) {
        const THRESH: usize = 16 * 1024;
        if self.pos == 0 {
            return;
        }
        if self.pos >= THRESH || self.pos * 2 >= self.buf.len() {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }

    pub fn append(&mut self, plain_or_cipher: &[u8], decrypt: Option<&mut Rc4>) {
        self.compact_if_needed();
        append_wire_read(&mut self.buf, plain_or_cipher, decrypt);
    }

    pub fn has_complete_frame(&self) -> bool {
        matches!(self.frame_remaining(), Some(0))
    }

    /// Bytes still needed to finish the current BT frame, if the 4-byte length
    /// is present and not over [`MAX_MESSAGE_LENGTH`]. `Some(0)` = complete.
    pub fn frame_remaining(&self) -> Option<usize> {
        let u = self.unparsed();
        if u.len() < 4 {
            return None;
        }
        let msg_len = u32::from_be_bytes([u[0], u[1], u[2], u[3]]) as usize;
        if msg_len > MAX_MESSAGE_LENGTH {
            return None;
        }
        Some((4 + msg_len).saturating_sub(u.len()))
    }

    /// Mid-frame remainder only (no pipeline speculation).
    pub fn recv_lowat(&self) -> Option<usize> {
        let need = self.frame_remaining()?;
        if need < RCVLOWAT_MIN {
            return None;
        }
        Some(need.min(WIRE_READ_CHUNK))
    }

    /// Park lowat while a leech is in progress.
    ///
    /// The larger of the mid-frame remainder and, when `speculative`,
    /// `min(outstanding × block, SO_RCVBUF/2, 256KiB)` clamped to the bytes
    /// still due on the socket. Bytes already in this buffer are not in the
    /// socket; counting them makes Linux poll-first and FreeBSD kqueue sleep
    /// until the stall timer.
    ///
    /// `downloading == false` returns `None` so the park leaves `SO_RCVLOWAT`
    /// at 1. Otherwise a seed peer's requests, HAVE, and a closed connection
    /// with a short recv queue would never wake.
    pub fn recv_lowat_park(
        &self,
        outstanding: usize,
        block: usize,
        rcvbuf: usize,
        speculative: bool,
        downloading: bool,
    ) -> Option<usize> {
        if !downloading {
            return None;
        }
        let frame = self.recv_lowat();
        let spec = if speculative {
            speculative_lowat(self.unparsed().len(), outstanding, block, rcvbuf)
        } else {
            None
        };
        match (frame, spec) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

/// `min(N × block, rcvbuf/2, 256KiB)` (`block` is the request size, ≤ 16KiB).
pub fn pipeline_lowat(outstanding: usize, block: usize, rcvbuf: usize) -> Option<usize> {
    if outstanding == 0 || rcvbuf < 2 || block == 0 {
        return None;
    }
    let want = outstanding.saturating_mul(block);
    let n = want.min(rcvbuf / 2).min(PIPELINE_LOWAT_CAP);
    if n < RCVLOWAT_MIN {
        None
    } else {
        Some(n)
    }
}

/// Speculative watermark, never above the bytes still expected on the socket.
///
/// `buffered` is the incomplete frame already in the user read buffer.
/// One request occupies [`crate::wire::SIZEOF_PIECE`] + `block` on the wire.
fn speculative_lowat(
    buffered: usize,
    outstanding: usize,
    block: usize,
    rcvbuf: usize,
) -> Option<usize> {
    let raw = pipeline_lowat(outstanding, block, rcvbuf)?;
    let wire = block.saturating_add(crate::wire::SIZEOF_PIECE);
    let expect = outstanding.saturating_mul(wire).saturating_sub(buffered);
    let n = raw.min(expect);
    if n < RCVLOWAT_MIN {
        None
    } else {
        Some(n)
    }
}

/// `SO_RCVLOWAT` for one peer reader.
///
/// [`Self::apply`] calls `setsockopt` only when the watermark changes.
/// On Linux each call takes the socket lock (`tcp_set_rcvlowat`). A steady
/// leech keeps the same value, so the hot path does not touch it. Drop
/// restores 1.
pub struct RecvLowat {
    #[cfg(unix)]
    fd: std::os::fd::RawFd,
    #[cfg(unix)]
    applied: usize,
}

impl RecvLowat {
    pub fn new(stream: &impl std::os::fd::AsFd) -> Self {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            Self {
                fd: stream.as_fd().as_raw_fd(),
                applied: 1,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = stream;
            Self {}
        }
    }

    /// `want` is the park watermark from [`ReadCursor::recv_lowat_park`].
    /// `None` means 1. Returns whether the socket watermark is above 1.
    pub fn apply(&mut self, want: Option<usize>) -> bool {
        #[cfg(unix)]
        {
            let want = want.unwrap_or(1).max(1);
            if self.applied != want && set_recv_lowat_fd(self.fd, want).is_ok() {
                self.applied = want;
            }
            self.applied > 1
        }
        #[cfg(not(unix))]
        {
            let _ = want;
            false
        }
    }
}

impl Drop for RecvLowat {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.applied != 1 {
            let _ = set_recv_lowat_fd(self.fd, 1);
        }
    }
}

#[cfg(all(unix, test))]
fn set_recv_lowat(stream: &impl std::os::fd::AsFd, n: usize) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    set_recv_lowat_fd(stream.as_fd().as_raw_fd(), n)
}

/// Kernel `SO_RCVBUF` (Linux often reports 2× the requested size).
#[allow(unsafe_code)]
pub fn socket_recv_buffer(stream: &impl std::os::fd::AsFd) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = stream.as_fd().as_raw_fd();
        let mut val: libc::c_int = 0;
        let mut len = std::mem::size_of_val(&val) as libc::socklen_t;
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&mut val as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        if rc == 0 && val > 0 {
            Ok(val as usize)
        } else if rc == 0 {
            Ok(0)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    {
        let _ = stream;
        Ok(0)
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn set_recv_lowat_fd(fd: std::os::fd::RawFd, n: usize) -> std::io::Result<()> {
    let val = n.max(1) as libc::c_int;
    // SAFETY: `fd` is a live socket for the guard's lifetime; `SO_RCVLOWAT` takes `c_int`.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVLOWAT,
            (&val as *const libc::c_int).cast(),
            std::mem::size_of_val(&val) as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub async fn read_exact(stream: &mut TcpStream, buf: &mut [u8]) -> Result<()> {
    let v = vec![0u8; buf.len()];
    let BufResult(r, v) = stream.read_exact(v).await;
    r.map_err(|e| Error::Msg(format!("read: {e}")))?;
    // IoBuf tracks init length separately from Vec::len — use as_init().
    let init = IoBuf::as_init(&v);
    if init.len() != buf.len() {
        return Err(Error::Msg(format!(
            "read_exact short init {}/{}",
            init.len(),
            buf.len()
        )));
    }
    buf.copy_from_slice(init);
    Ok(())
}

pub async fn read_exact_crypto(
    stream: &mut TcpStream,
    buf: &mut [u8],
    decrypt: Option<&mut Rc4>,
) -> Result<()> {
    read_exact(stream, buf).await?;
    if let Some(c) = decrypt {
        c.crypt_inplace(buf);
    }
    Ok(())
}

pub async fn read_exact_timeout(
    stream: &mut TcpStream,
    buf: &mut [u8],
    dur: Duration,
) -> Result<()> {
    match timeout(dur, read_exact(stream, buf)).await {
        Ok(r) => r,
        Err(_) => Err(Error::Msg("read timeout".into())),
    }
}

pub async fn read_exact_crypto_timeout(
    stream: &mut TcpStream,
    buf: &mut [u8],
    decrypt: Option<&mut Rc4>,
    dur: Duration,
) -> Result<()> {
    match timeout(dur, read_exact_crypto(stream, buf, decrypt)).await {
        Ok(r) => r,
        Err(_) => Err(Error::Msg("read timeout".into())),
    }
}

/// Compio completion read into a **reused** buffer. Parks until the op completes.
///
/// On success, `scratch[..n]` holds the bytes (`n == 0` is EOF). At most `max`
/// bytes are read. Compio fills **capacity**, so the op uses `slice(..max)` and
/// does **not** shrink a larger scratch (no realloc when alternating sizes).
pub async fn read_some(stream: &mut TcpStream, scratch: &mut Vec<u8>, max: usize) -> Result<usize> {
    read_some_inner(stream, scratch, max, false).await
}

/// [`read_some`] while [`RecvLowat`] is above 1.
///
/// On Linux, Compio's `IORING_OP_RECV` is a nonblocking recv. It returns any
/// queued bytes and does not wait for `SO_RCVLOWAT`. This submit sets
/// `IORING_RECVSEND_POLL_FIRST` so io_uring polls first. epoll uses the
/// watermark, matching a FreeBSD `kqueue` read. Kernels before 5.19 ignore
/// the flag. Other platforms keep the normal read; their poll already waits.
pub async fn read_some_at_lowat(
    stream: &mut TcpStream,
    scratch: &mut Vec<u8>,
    max: usize,
) -> Result<usize> {
    read_some_inner(stream, scratch, max, true).await
}

async fn read_some_inner(
    stream: &mut TcpStream,
    scratch: &mut Vec<u8>,
    max: usize,
    at_lowat: bool,
) -> Result<usize> {
    let cap = max.max(1);
    let mut taken = std::mem::take(scratch);
    taken.clear();
    if taken.capacity() < cap {
        taken.reserve(cap - taken.capacity());
    }
    // Cap the IoBuf destination to `cap` without changing `taken.capacity()`.
    let slice = taken.slice(..cap);
    let BufResult(r, slice) = read_buf(stream, slice, at_lowat).await;
    let mut buf = slice.into_inner();
    let n = r.map_err(|e| Error::Msg(format!("read: {e}")))?;
    let n = n.min(cap).min(buf.len());
    buf.truncate(n);
    *scratch = buf;
    Ok(n)
}

#[allow(unsafe_code)]
async fn read_buf<B>(stream: &mut TcpStream, buf: B, at_lowat: bool) -> BufResult<usize, B>
where
    B: compio::buf::IoBufMut + 'static,
{
    #[cfg(target_os = "linux")]
    if at_lowat {
        use compio::buf::SetLen;
        use compio::driver::op::{Recv, RecvFlags};
        use compio::driver::PollFirst;

        // Clone is a `SharedFd` refcount, not a dup. The op must be `'static`.
        let mut op = Recv::new(stream.clone(), buf, RecvFlags::empty());
        PollFirst::poll_first(&mut op);
        let BufResult(res, op) = compio::runtime::submit(op).await;
        let mut buf = op.into_inner();
        if let Ok(n) = res {
            // SAFETY: io_uring wrote `n` initialized bytes at the start of `buf`.
            unsafe { SetLen::advance_to(&mut buf, n) };
        }
        return BufResult(res, buf);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = at_lowat;
    stream.read(buf).await
}

pub async fn read_some_timeout(
    stream: &mut TcpStream,
    scratch: &mut Vec<u8>,
    max: usize,
    dur: Duration,
) -> Result<usize> {
    match timeout(dur, read_some(stream, scratch, max)).await {
        Ok(r) => r,
        Err(_) => Err(Error::Msg("read timeout".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(bytes: &[u8]) -> ReadCursor {
        ReadCursor::from_vec(bytes.to_vec())
    }

    fn len_prefix(msg_len: u32, extra: usize) -> Vec<u8> {
        let mut v = msg_len.to_be_bytes().to_vec();
        v.resize(4 + extra, 0);
        v
    }

    #[test]
    fn frame_remaining_needs_length_prefix() {
        assert_eq!(cursor(&[]).frame_remaining(), None);
        assert_eq!(cursor(&[0, 0, 0]).frame_remaining(), None);
    }

    #[test]
    fn frame_remaining_keepalive_complete() {
        assert_eq!(cursor(&[0, 0, 0, 0]).frame_remaining(), Some(0));
        assert!(cursor(&[0, 0, 0, 0]).has_complete_frame());
    }

    #[test]
    fn frame_remaining_piece_body() {
        // 16 KiB PIECE: len = 9 + 16384 = 16393
        let c = cursor(&len_prefix(16393, 0));
        assert_eq!(c.frame_remaining(), Some(16393));
        assert_eq!(c.recv_lowat(), Some(16393));

        let mid = cursor(&len_prefix(16393, 10_000));
        assert_eq!(mid.frame_remaining(), Some(6393));
        assert_eq!(mid.recv_lowat(), Some(6393));

        let almost = cursor(&len_prefix(16393, 13_000));
        assert_eq!(almost.frame_remaining(), Some(3393));
        assert_eq!(almost.recv_lowat(), None);

        let done = cursor(&len_prefix(16393, 16393));
        assert_eq!(done.frame_remaining(), Some(0));
        assert!(done.has_complete_frame());
        assert_eq!(done.recv_lowat(), None);
    }

    #[test]
    fn recv_lowat_caps_at_wire_chunk() {
        let c = cursor(&len_prefix(MAX_MESSAGE_LENGTH as u32, 0));
        assert_eq!(c.frame_remaining(), Some(MAX_MESSAGE_LENGTH));
        assert_eq!(c.recv_lowat(), Some(WIRE_READ_CHUNK));
    }

    #[test]
    fn recv_lowat_skips_oversized_length() {
        let c = cursor(&((MAX_MESSAGE_LENGTH as u32) + 1).to_be_bytes());
        assert_eq!(c.frame_remaining(), None);
        assert_eq!(c.recv_lowat(), None);
        assert!(!c.has_complete_frame());
    }

    #[test]
    fn recv_lowat_skips_small_control() {
        // HAVE: len=5
        assert_eq!(cursor(&len_prefix(5, 0)).recv_lowat(), None);
    }

    #[test]
    fn pipeline_lowat_formula() {
        const B: usize = crate::staging::BLOCK_SIZE as usize;
        assert_eq!(pipeline_lowat(0, B, 128 * 1024), None);
        assert_eq!(pipeline_lowat(4, B, 0), None);
        assert_eq!(pipeline_lowat(4, B, 128 * 1024), Some(4 * B));
        assert_eq!(pipeline_lowat(4, B, 64 * 1024), Some(32 * 1024));
        assert_eq!(
            pipeline_lowat(32, B, 2 * 1024 * 1024),
            Some(PIPELINE_LOWAT_CAP)
        );
        assert_eq!(pipeline_lowat(1, B, 128 * 1024), Some(B));
        assert_eq!(pipeline_lowat(3, 32, 128 * 1024), None);
    }

    #[test]
    fn park_prefers_larger_of_frame_and_pipeline() {
        let c = cursor(&len_prefix(16393, 0));
        const B: usize = crate::staging::BLOCK_SIZE as usize;
        assert_eq!(c.recv_lowat_park(4, B, 128 * 1024, true, true), Some(4 * B));
        assert_eq!(
            c.recv_lowat_park(4, B, 128 * 1024, false, true),
            Some(16393)
        );
        assert_eq!(
            cursor(&[]).recv_lowat_park(4, B, 128 * 1024, true, true),
            Some(64 * 1024)
        );
        assert_eq!(
            cursor(&[]).recv_lowat_park(4, B, 128 * 1024, false, true),
            None
        );
    }

    /// Partial PIECE already in the user buffer must not make the watermark
    /// larger than the bytes the socket can still deliver.
    #[test]
    fn park_lowat_clamps_below_bytes_already_buffered() {
        const B: usize = crate::staging::BLOCK_SIZE as usize;
        let wire = crate::wire::SIZEOF_PIECE + B;
        // 15 KiB of the first block is already parsed into the user buffer.
        let buffered = 15 * 1024;
        assert!(buffered < wire);
        let c = cursor(&len_prefix((wire - 4) as u32, buffered - 4));
        assert_eq!(c.unparsed().len(), buffered);
        let outstanding = 9;
        let expect = outstanding * wire - buffered;
        let got = c
            .recv_lowat_park(outstanding, B, 2 * 1024 * 1024, true, true)
            .expect("tail still large enough to arm");
        assert!(
            got <= expect,
            "lowat {got} exceeds the {expect} bytes still on the wire"
        );
        assert!(got < PIPELINE_LOWAT_CAP);
        // Old formula waited for outstanding × block (144 KiB) even though
        // part of that payload is already in the user buffer.
        assert!(got < outstanding * B);
    }

    #[test]
    fn park_lowat_off_when_not_downloading() {
        const B: usize = crate::staging::BLOCK_SIZE as usize;
        let mid = cursor(&len_prefix(16393, 0));
        assert_eq!(mid.recv_lowat_park(4, B, 128 * 1024, true, false), None);
        assert_eq!(
            cursor(&[]).recv_lowat_park(32, B, 2 * 1024 * 1024, true, false),
            None
        );
    }

    /// Close with less than `SO_RCVLOWAT` queued must still complete the read
    /// (EOF / leftover), not park forever.
    #[cfg(unix)]
    #[compio::test]
    async fn rcvlowat_close_unblocks() {
        use compio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let s = std::net::TcpStream::connect(addr).unwrap();
            let _ = s.set_nodelay(true);
            use std::io::Write;
            let mut s = s;
            s.write_all(&[1, 2, 3, 4, 5]).unwrap();
            // FIN with 5 bytes queued; peer has lowat 16 KiB.
            drop(s);
        });

        let (mut server, _) = listener.accept().await.unwrap();
        set_recv_lowat(&server, 16 * 1024).unwrap();
        let mut scratch = Vec::new();
        let n = timeout(
            Duration::from_secs(2),
            read_some_at_lowat(&mut server, &mut scratch, 64 * 1024),
        )
        .await
        .expect("close with high SO_RCVLOWAT must not hang")
        .unwrap();
        // Leftover bytes and/or EOF — not a hang.
        assert!(n == 0 || scratch[..n] == [1, 2, 3, 4, 5]);
        client.join().unwrap();
    }

    /// Queued bytes under `SO_RCVLOWAT` must not complete a lowat park.
    /// The rest of the watermark must.
    #[cfg(unix)]
    #[compio::test]
    async fn rcvlowat_park_waits_for_watermark() {
        use std::io::Write;
        use std::os::fd::{AsFd, AsRawFd};
        use std::sync::mpsc;

        use compio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let client = std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(addr).unwrap();
            s.set_nodelay(true).unwrap();
            s.write_all(&[9u8; 1024]).unwrap();
            release_rx.recv().unwrap();
            s.write_all(&[9u8; 7 * 1024]).unwrap();
            done_rx.recv().unwrap();
        });

        let (mut server, _) = listener.accept().await.unwrap();
        set_recv_lowat(&server, 8 * 1024).unwrap();
        let fd = server.as_fd().as_raw_fd();
        let wait_queued = std::time::Instant::now();
        while queued_bytes(fd) < 1024 {
            assert!(
                wait_queued.elapsed() < Duration::from_secs(2),
                "1 KiB never queued"
            );
            compio::time::sleep(Duration::from_millis(5)).await;
        }

        let mut scratch = Vec::new();
        // Keep this future across the early timeout. Dropping it cancels the
        // io_uring recv, and a second poll-first submit can miss bytes that
        // arrived during the cancel.
        let n = {
            let mut read = std::pin::pin!(read_some_at_lowat(&mut server, &mut scratch, 64 * 1024));
            let early = timeout(Duration::from_millis(200), read.as_mut()).await;
            assert!(early.is_err(), "park returned before the watermark");
            release_tx.send(()).unwrap();
            timeout(Duration::from_secs(2), read)
                .await
                .expect("watermark read hung")
                .unwrap()
        };
        assert!(n >= 8 * 1024, "short read {n}");
        assert!(scratch[..n].iter().all(|b| *b == 9));
        done_tx.send(()).unwrap();
        client.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn recv_lowat_changes_only_when_the_value_changes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || std::net::TcpStream::connect(addr).unwrap());
        let (server, _) = listener.accept().unwrap();
        let _client = client.join().unwrap();

        let mut lowat = RecvLowat::new(&server);
        assert!(lowat.apply(Some(8 * 1024)));
        assert_eq!(recv_lowat_of(&server), 8 * 1024);
        assert!(lowat.apply(Some(8 * 1024)));
        assert_eq!(recv_lowat_of(&server), 8 * 1024);
        assert!(!lowat.apply(None));
        assert_eq!(recv_lowat_of(&server), 1);
        assert!(lowat.apply(Some(16 * 1024)));
        drop(lowat);
        assert_eq!(recv_lowat_of(&server), 1);
    }

    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn recv_lowat_of(stream: &impl std::os::fd::AsFd) -> usize {
        use std::os::fd::AsRawFd;
        let mut val: libc::c_int = 0;
        let mut len = std::mem::size_of_val(&val) as libc::socklen_t;
        // SAFETY: `fd` is a live socket; SO_RCVLOWAT is a c_int out-parameter.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_fd().as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVLOWAT,
                (&mut val as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        assert_eq!(
            rc,
            0,
            "getsockopt SO_RCVLOWAT: {}",
            std::io::Error::last_os_error()
        );
        val.max(0) as usize
    }

    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn queued_bytes(fd: std::os::fd::RawFd) -> usize {
        let mut n: libc::c_int = 0;
        // SAFETY: `fd` is the live accepted socket; FIONREAD writes one c_int.
        let rc = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut n) };
        assert_eq!(rc, 0, "FIONREAD: {}", std::io::Error::last_os_error());
        n.max(0) as usize
    }

    /// A `write_all` parked on a full window must end when the deadline wins.
    /// Dropping that future is what cancels the kqueue op.
    #[cfg(unix)]
    #[compio::test]
    async fn blocked_write_hits_deadline() {
        use std::sync::mpsc;

        use compio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let peer = std::thread::spawn(move || {
            let sock = std::net::TcpStream::connect(addr).unwrap();
            let _ = SockRef::from(&sock).set_recv_buffer_size(4 * 1024);
            let _ = release_rx.recv();
            drop(sock);
        });

        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = SockRef::from(&stream).set_send_buffer_size(4 * 1024);
        let payload = vec![0u8; 4 * 1024 * 1024];
        let started = std::time::Instant::now();
        let timed_out = timeout(
            Duration::from_millis(400),
            write_all_owned(&mut stream, payload),
        )
        .await;
        assert!(
            timed_out.is_err(),
            "write into a peer that never reads must hit the deadline"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = release_tx.send(());
        peer.join().unwrap();
    }
}
