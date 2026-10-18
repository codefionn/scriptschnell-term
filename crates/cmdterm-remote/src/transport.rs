//! Length-prefixed frames, plain or sealed with ChaCha20-Poly1305.
//!
//! A frame is a big-endian `u32` length followed by that many bytes. Plain
//! frames carry the handshake, and channel traffic over local sockets and
//! vsock, which only this machine can reach. After the handshake every TCP
//! frame is sealed: the body is AEAD ciphertext under a per-direction key,
//! the nonce is a message counter, and the length prefix is authenticated
//! data. A dropped, replayed, reordered, or altered frame fails to open and
//! ends the connection.

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::Duration;

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use zeroize::Zeroizing;

/// Makes a dead peer end the connection within about half a minute, also
/// while nothing is sent: TCP keepalive probes after 15 seconds of silence,
/// and on Linux unacknowledged data gives up after 30 seconds. A session's
/// client then reconnects, and the server detaches the session. Best
/// effort; a socket that refuses an option keeps the system default.
pub fn keep_alive(stream: &std::net::TcpStream) {
    use rustix::net::sockopt;
    let _ = sockopt::set_socket_keepalive(stream, true);
    let _ = sockopt::set_tcp_keepidle(stream, std::time::Duration::from_secs(15));
    let _ = sockopt::set_tcp_keepintvl(stream, std::time::Duration::from_secs(5));
    let _ = sockopt::set_tcp_keepcnt(stream, 3);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let _ = sockopt::set_tcp_user_timeout(stream, 30_000);
}

/// Runs `expire` unless the returned guard is dropped within `limit`.
///
/// The handshake runs under one that shuts the socket down. A read timeout
/// alone starts over with every byte, so a peer that sends one byte every
/// few seconds would hold its connection, and one of the server's
/// connection slots, forever.
pub fn deadline(limit: Duration, expire: impl FnOnce() + Send + 'static) -> Deadline {
    let (armed, done) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if done.recv_timeout(limit) == Err(mpsc::RecvTimeoutError::Timeout) {
            expire();
        }
    });
    Deadline { _armed: armed }
}

/// See [`deadline`]. Dropping it disarms the deadline.
pub struct Deadline {
    _armed: mpsc::Sender<()>,
}

/// Largest frame payload accepted, sealed or plain. A sealed frame's body
/// is longer by the tag, so a plain frame read from a VM always fits into a
/// sealed one when it is passed on.
pub const MAX_FRAME: usize = 1 << 20;
const TAG: usize = 16;

/// Reads whole frames.
pub trait FrameRead: Send {
    /// Reads the next frame into `buf`, replacing its contents. `Ok(false)`
    /// means the peer closed the stream cleanly between frames.
    fn read_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<bool>;
}

/// Writes whole frames.
pub trait FrameWrite: Send {
    fn write_frame(&mut self, payload: &[u8]) -> io::Result<()>;
}

impl<R: FrameRead + ?Sized> FrameRead for Box<R> {
    fn read_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        (**self).read_frame(buf)
    }
}

impl<W: FrameWrite + ?Sized> FrameWrite for Box<W> {
    fn write_frame(&mut self, payload: &[u8]) -> io::Result<()> {
        (**self).write_frame(payload)
    }
}

/// Plain frames over any byte stream.
pub struct Plain<S>(pub S);

impl<S: Read + Send> FrameRead for Plain<S> {
    fn read_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        read_raw(&mut self.0, buf, MAX_FRAME)
    }
}

impl<S: Write + Send> FrameWrite for Plain<S> {
    fn write_frame(&mut self, payload: &[u8]) -> io::Result<()> {
        write_raw(&mut self.0, payload, MAX_FRAME)
    }
}

impl<S: Read> Plain<S> {
    /// Reads the next frame like [`FrameRead::read_frame`], but refuses one
    /// longer than `max` before reading its body. The handshake reads with
    /// the size it expects, so a caller that has not logged in yet cannot
    /// make the server hold a large buffer.
    pub fn read_frame_max(&mut self, buf: &mut Vec<u8>, max: usize) -> io::Result<bool> {
        read_raw(&mut self.0, buf, max.min(MAX_FRAME))
    }
}

/// Reads one frame of at most `max` bytes. A longer length fails before
/// any of the body is read or allocated.
fn read_raw(stream: &mut impl Read, buf: &mut Vec<u8>, max: usize) -> io::Result<bool> {
    let mut len = [0u8; 4];
    let mut filled = 0;
    while filled < 4 {
        match stream.read(&mut len[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    buf.clear();
    buf.resize(len, 0);
    stream.read_exact(buf)?;
    Ok(true)
}

/// Writes the length of `body`, then `body`, in one write call. A body
/// longer than `max` fails, so nothing is sent that the peer would refuse.
fn write_raw(stream: &mut impl Write, body: &[u8], max: usize) -> io::Result<()> {
    let len = body.len();
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut frame = Vec::with_capacity(4 + len);
    frame.extend_from_slice(&(len as u32).to_be_bytes());
    frame.extend_from_slice(body);
    stream.write_all(&frame)?;
    stream.flush()
}

/// One direction's AEAD state.
pub struct SealKey {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl SealKey {
    pub fn new(key: &Zeroizing<[u8; 32]>) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key.as_ref())),
            counter: 0,
        }
    }

    fn next_nonce(&mut self) -> io::Result<Nonce> {
        let counter = self.counter;
        self.counter = counter.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "message counter exhausted")
        })?;
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&counter.to_be_bytes());
        Ok(*Nonce::from_slice(&nonce))
    }

    /// Encrypts `buf` in place and appends the tag.
    pub fn seal(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        let nonce = self.next_nonce()?;
        let aad = ((buf.len() + TAG) as u32).to_be_bytes();
        self.cipher
            .encrypt_in_place(&nonce, &aad, buf)
            .map_err(|_| io::Error::other("encryption failed"))
    }

    /// Decrypts `buf` in place and removes the tag.
    pub fn open(&mut self, buf: &mut Vec<u8>) -> io::Result<()> {
        let nonce = self.next_nonce()?;
        let aad = (buf.len() as u32).to_be_bytes();
        self.cipher
            .decrypt_in_place(&nonce, &aad, buf)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame failed to decrypt"))
    }
}

/// Sealed frames read from a stream.
pub struct SealedReader<S> {
    stream: S,
    key: SealKey,
}

impl<S> SealedReader<S> {
    pub fn new(stream: S, key: SealKey) -> Self {
        Self { stream, key }
    }
}

impl<S: Read + Send> FrameRead for SealedReader<S> {
    fn read_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        self.read_frame_max(buf, MAX_FRAME)
    }
}

impl<S: Read> SealedReader<S> {
    /// Reads and opens the next frame, refusing one whose payload would be
    /// longer than `max` before reading it.
    pub fn read_frame_max(&mut self, buf: &mut Vec<u8>, max: usize) -> io::Result<bool> {
        if !read_raw(&mut self.stream, buf, max.min(MAX_FRAME) + TAG)? {
            return Ok(false);
        }
        self.key.open(buf)?;
        Ok(true)
    }
}

/// Sealed frames written to a stream.
pub struct SealedWriter<S> {
    stream: S,
    key: SealKey,
    scratch: Vec<u8>,
}

impl<S> SealedWriter<S> {
    pub fn new(stream: S, key: SealKey) -> Self {
        Self {
            stream,
            key,
            scratch: Vec::new(),
        }
    }
}

impl<S: Write + Send> FrameWrite for SealedWriter<S> {
    fn write_frame(&mut self, payload: &[u8]) -> io::Result<()> {
        if payload.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame too large",
            ));
        }
        self.scratch.clear();
        self.scratch.extend_from_slice(payload);
        self.key.seal(&mut self.scratch)?;
        write_raw(&mut self.stream, &self.scratch, MAX_FRAME + TAG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (SealedWriter<Vec<u8>>, SealKey) {
        let key = Zeroizing::new([9u8; 32]);
        (
            SealedWriter::new(Vec::new(), SealKey::new(&key)),
            SealKey::new(&key),
        )
    }

    #[test]
    fn sealed_frames_round_trip() {
        let (mut writer, key) = pair();
        writer.write_frame(b"one").unwrap();
        writer.write_frame(b"").unwrap();
        writer.write_frame(b"three").unwrap();
        let mut reader = SealedReader::new(io::Cursor::new(writer.stream), key);
        let mut buf = Vec::new();
        for expected in [&b"one"[..], b"", b"three"] {
            assert!(reader.read_frame(&mut buf).unwrap());
            assert_eq!(buf, expected);
        }
        assert!(!reader.read_frame(&mut buf).unwrap());
    }

    #[test]
    fn altered_or_reordered_frames_fail() {
        let (mut writer, key) = pair();
        writer.write_frame(b"hello").unwrap();
        let mut bytes = writer.stream.clone();
        bytes[6] ^= 1;
        let mut reader = SealedReader::new(io::Cursor::new(bytes), key);
        assert!(reader.read_frame(&mut Vec::new()).is_err());

        // The second frame alone opens with the first counter value: no.
        let (mut writer, key) = pair();
        writer.write_frame(b"a").unwrap();
        let first = writer.stream.len();
        writer.write_frame(b"b").unwrap();
        let second = writer.stream[first..].to_vec();
        let mut reader = SealedReader::new(io::Cursor::new(second), key);
        assert!(reader.read_frame(&mut Vec::new()).is_err());
    }

    #[test]
    fn plain_frames_reject_oversized_lengths() {
        // Larger than a sealed frame could carry on, so a VM's frame is
        // refused where it is read, not where it is sent on.
        let mut bytes = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0; 8]);
        let mut reader = Plain(io::Cursor::new(bytes));
        assert!(reader.read_frame(&mut Vec::new()).is_err());
        assert!(Plain(Vec::new()).write_frame(&[0; MAX_FRAME + 1]).is_err());

        // The largest plain frame fits into a sealed one.
        let mut plain = Plain(Vec::new());
        plain.write_frame(&[7; MAX_FRAME]).unwrap();
        let mut payload = Vec::new();
        assert!(
            Plain(io::Cursor::new(plain.0))
                .read_frame(&mut payload)
                .unwrap()
        );
        let (mut writer, key) = pair();
        writer.write_frame(&payload).unwrap();
        let mut reader = SealedReader::new(io::Cursor::new(writer.stream), key);
        assert!(reader.read_frame(&mut payload).unwrap());
        assert_eq!(payload.len(), MAX_FRAME);
    }

    /// A frame longer than the reader expects fails on its length, before
    /// a body that never comes.
    #[test]
    fn capped_reads_refuse_long_frames_before_their_body() {
        let bytes = (4096u32).to_be_bytes().to_vec();
        let mut reader = Plain(io::Cursor::new(bytes.clone()));
        let error = reader.read_frame_max(&mut Vec::new(), 100).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // Without the cap the reader waits for the body.
        let mut reader = Plain(io::Cursor::new(bytes));
        let error = reader.read_frame(&mut Vec::new()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_deadline_fires_unless_dropped_in_time() {
        let (fired, rx) = mpsc::channel();
        let _guard = deadline(Duration::from_millis(50), move || {
            let _ = fired.send(());
        });
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());

        let (fired, rx) = mpsc::channel();
        drop(deadline(Duration::from_millis(50), move || {
            let _ = fired.send(());
        }));
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }
}
