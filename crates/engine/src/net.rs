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

/// Socket `read_some` max. Covers several 16 KiB PIECE frames per recv.
pub const WIRE_READ_CHUNK: usize = 64 * 1024;

/// Do not raise `SO_RCVLOWAT` for a smaller remainder (setsockopt not worth it).
const RCVLOWAT_MIN: usize = 4 * 1024;

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

    /// `SO_RCVLOWAT` to arm before a park: remainder of a known large frame,
    /// capped at [`WIRE_READ_CHUNK`]. `None` = leave the kernel default (1).
    pub fn recv_lowat(&self) -> Option<usize> {
        let need = self.frame_remaining()?;
        if need < RCVLOWAT_MIN {
            return None;
        }
        Some(need.min(WIRE_READ_CHUNK))
    }
}

/// Raises `SO_RCVLOWAT` for a mid-frame park; restores 1 on drop.
/// Holds a raw fd so the stream can be mutably borrowed for the read.
pub struct RecvLowatGuard {
    #[cfg(unix)]
    fd: Option<std::os::fd::RawFd>,
}

impl RecvLowatGuard {
    /// Arm only when [`ReadCursor::recv_lowat`] is `Some`. Failures are ignored.
    pub fn arm(stream: &impl std::os::fd::AsFd, cursor: &ReadCursor) -> Self {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if let Some(n) = cursor.recv_lowat() {
                let fd = stream.as_fd().as_raw_fd();
                if set_recv_lowat_fd(fd, n).is_ok() {
                    return Self { fd: Some(fd) };
                }
            }
            Self { fd: None }
        }
        #[cfg(not(unix))]
        {
            let _ = (stream, cursor);
            Self {}
        }
    }
}

impl Drop for RecvLowatGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(fd) = self.fd.take() {
            let _ = set_recv_lowat_fd(fd, 1);
        }
    }
}

#[cfg(all(unix, test))]
fn set_recv_lowat(stream: &impl std::os::fd::AsFd, n: usize) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    set_recv_lowat_fd(stream.as_fd().as_raw_fd(), n)
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
    let cap = max.max(1);
    let mut taken = std::mem::take(scratch);
    taken.clear();
    if taken.capacity() < cap {
        taken.reserve(cap - taken.capacity());
    }
    // Cap the IoBuf destination to `cap` without changing `taken.capacity()`.
    let slice = taken.slice(..cap);
    let BufResult(r, slice) = stream.read(slice).await;
    let mut buf = slice.into_inner();
    let n = r.map_err(|e| Error::Msg(format!("read: {e}")))?;
    let n = n.min(cap).min(buf.len());
    buf.truncate(n);
    *scratch = buf;
    Ok(n)
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
            read_some(&mut server, &mut scratch, 64 * 1024),
        )
        .await
        .expect("close with high SO_RCVLOWAT must not hang")
        .unwrap();
        // Leftover bytes and/or EOF — not a hang.
        assert!(n == 0 || scratch[..n] == [1, 2, 3, 4, 5]);
        client.join().unwrap();
    }
}
