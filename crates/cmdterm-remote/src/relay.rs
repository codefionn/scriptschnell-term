//! Unix socket forwards from a remote machine back to the client, like
//! `ssh -R`. scriptschnell-term uses them so agents on a remote machine or in
//! a VM reach the app's control socket (`exec --forward INSIDE=LOCAL`).
//!
//! `exec` opens a second channel whose program is [`PROGRAM`] and whose one
//! argument is the path to listen on. [`exec::open_local`] runs it in the
//! server (or guest agent) itself instead of starting a process: it binds
//! the path with mode 0600 and carries each connection over the channel.
//! A server without relays fails the open, and the command runs without
//! the forward.
//!
//! Both directions of the channel carry [`Frame`]s: a type byte, a
//! connection id and a `u32` length, then the bytes. The server sends
//! [`READY`] once it listens and [`OPEN`] for each connection; either side
//! sends [`DATA`] and [`CLOSE`].
//!
//! Neither side waits for a connection that reads slowly: up to
//! `CONN_LIMIT` (64 MiB) wait for it. A connection that lets more wait is
//! shut down, and its reader sees the end of the stream after the bytes it
//! got. A Unix socket has no portable way to end a stream with an error,
//! and the bytes are the forwarded program's own, so the reader cannot
//! tell this from a clean end; a protocol on top, such as HTTP with its
//! `Content-Length`, can. The limit is far above what the control socket
//! answers, so only a reader that stopped reading reaches it.
//!
//! A side that receives something that is not relay frames ends the
//! channel: the server with [`Msg::Exit`], the client with [`Msg::Close`].
//!
//! [`exec::open_local`]: crate::exec::open_local

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::exec::{Channel, Outbox};
use crate::proto::{Msg, OpenSpec};
use crate::sys::{UnixListener, UnixStream};
use crate::transport::{FrameRead, FrameWrite};

/// The program name `exec` opens a relay channel with.
pub const PROGRAM: &str = "cmdterm-remote:relay";

pub const READY: u8 = 0;
pub const OPEN: u8 = 1;
pub const DATA: u8 = 2;
pub const CLOSE: u8 = 3;

/// Largest payload of one frame.
const MAX_FRAME: usize = 64 * 1024;
/// How long `exec` waits for the server to listen before it runs the
/// command without the forward.
pub const READY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub conn: u32,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn encode(kind: u8, conn: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(9 + data.len());
        out.push(kind);
        out.extend_from_slice(&conn.to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        out
    }
}

/// Splits a byte stream into frames; channel data arrives in any chunks.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The next whole frame, `Err` when the stream is not relay frames.
    pub fn next_frame(&mut self) -> io::Result<Option<Frame>> {
        if self.buf.len() < 9 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(self.buf[5..9].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "relay frame too large",
            ));
        }
        if self.buf.len() < 9 + len {
            return Ok(None);
        }
        let frame = Frame {
            kind: self.buf[0],
            conn: u32::from_be_bytes(self.buf[1..5].try_into().unwrap()),
            data: self.buf[9..9 + len].to_vec(),
        };
        self.buf.drain(..9 + len);
        Ok(Some(frame))
    }
}

/// Reads `stream` until it ends and hands each chunk to `send` as
/// [`DATA`] frames of connection `conn`, then sends [`CLOSE`].
fn pump(mut stream: UnixStream, conn: u32, send: impl Fn(Vec<u8>) -> bool) {
    let mut buf = vec![0u8; MAX_FRAME];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if !send(Frame::encode(DATA, conn, &buf[..n])) {
                    return;
                }
            }
        }
    }
    let _ = send(Frame::encode(CLOSE, conn, &[]));
}

/// Bytes from the peer that may wait for one local connection to read them.
const CONN_LIMIT: usize = 64 << 20;

/// One carried connection: its socket, and the queue its writer thread
/// writes to it from.
struct Conn {
    stream: UnixStream,
    queue: crate::queue::Sender<Vec<u8>>,
}

type Streams = Arc<Mutex<HashMap<u32, Conn>>>;

/// Starts the thread that writes what the peer sends to `stream`, so the
/// channel's reader never waits for one connection.
fn start_conn(stream: UnixStream) -> io::Result<Conn> {
    let mut sink = stream.try_clone()?;
    let (queue, rx) = crate::queue::bounded::<Vec<u8>>(CONN_LIMIT);
    thread::spawn(move || {
        while let Some(data) = rx.recv() {
            if sink.write_all(&data).is_err() {
                let _ = sink.shutdown(Shutdown::Both);
                return;
            }
        }
        // The peer's CLOSE, after everything it sent before.
        let _ = sink.shutdown(Shutdown::Write);
    });
    Ok(Conn { stream, queue })
}

/// Hands a frame from the peer to its connection.
fn deliver(streams: &Streams, frame: Frame) {
    let mut streams = streams.lock().unwrap();
    match frame.kind {
        DATA => {
            let Some(conn) = streams.get(&frame.conn) else {
                return;
            };
            let cost = frame.data.len();
            // A connection that lets more wait than the limit is closed
            // instead of holding up the others. Its pump then sees the end
            // and sends the peer CLOSE.
            if conn
                .queue
                .send(frame.data, cost, Some(Duration::ZERO))
                .is_err()
                && let Some(conn) = streams.remove(&frame.conn)
            {
                let _ = conn.stream.shutdown(Shutdown::Both);
            }
        }
        CLOSE => {
            // Dropping the queue closes it once the writer is through.
            streams.remove(&frame.conn);
        }
        _ => {}
    }
}

/// The server end: a listener on the remote machine.
struct RelayChannel {
    id: u32,
    out: Outbox,
    path: PathBuf,
    streams: Streams,
    decoder: Decoder,
    stopping: Arc<AtomicBool>,
    /// The peer sent something that is not relay frames.
    broken: bool,
}

/// Opens relay channel `id` for `spec`, see the module docs.
pub fn open(id: u32, spec: &OpenSpec, out: Outbox) -> Result<Box<dyn Channel>, String> {
    let [path] = spec.args.as_slice() else {
        return Err("a relay takes one socket path".into());
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(format!("{}: not an absolute path", path.display()));
    }
    let listener = bind_private(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let streams: Streams = Arc::default();
    let stopping = Arc::new(AtomicBool::new(false));
    let _ = out.send(Msg::Data(id, Frame::encode(READY, 0, &[])));
    let channel_out = out.clone();
    let accept_streams = streams.clone();
    let accept_stopping = stopping.clone();
    thread::spawn(move || {
        let next = AtomicU32::new(1);
        for stream in listener.incoming() {
            if accept_stopping.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let Ok(reader) = stream.try_clone() else {
                continue;
            };
            let Ok(entry) = start_conn(stream) else {
                continue;
            };
            let conn = next.fetch_add(1, Ordering::Relaxed);
            accept_streams.lock().unwrap().insert(conn, entry);
            if out
                .send(Msg::Data(id, Frame::encode(OPEN, conn, &[])))
                .is_err()
            {
                break;
            }
            let out = out.clone();
            thread::spawn(move || {
                pump(reader, conn, |frame| out.send(Msg::Data(id, frame)).is_ok())
            });
        }
    });
    Ok(Box::new(RelayChannel {
        id,
        out: channel_out,
        path,
        streams,
        decoder: Decoder::default(),
        stopping,
        broken: false,
    }))
}

/// Binds `path` so only this user can connect, from the moment the socket
/// exists (see [`crate::sys::bind_private_socket`]). A path that already
/// exists is left alone: the bind then fails, so an existing file is never
/// replaced.
fn bind_private(path: &Path) -> io::Result<UnixListener> {
    let listener = crate::sys::bind_private_socket(path)?;
    crate::sys::set_mode(path, 0o600)?;
    Ok(listener)
}

impl Channel for RelayChannel {
    fn input(&mut self, data: Vec<u8>) {
        if self.broken {
            return;
        }
        self.decoder.push(&data);
        loop {
            match self.decoder.next_frame() {
                Ok(Some(frame)) => deliver(&self.streams, frame),
                Ok(None) => return,
                // Nothing after a bad frame can be read either: close the
                // relay, drop what waits and tell the client it ended.
                Err(_) => {
                    self.broken = true;
                    self.decoder = Decoder::default();
                    self.kill();
                    let _ = self.out.send(Msg::Exit(self.id, 1));
                    return;
                }
            }
        }
    }

    fn close_input(&mut self) {}

    fn resize(&mut self, _cols: u16, _rows: u16) {}

    fn kill(&mut self) {
        // Once: the path may belong to another relay by a second call.
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        // Wakes the accept loop so it sees the flag and drops the listener.
        let _ = UnixStream::connect(&self.path);
        let _ = std::fs::remove_file(&self.path);
        for (_, conn) in self.streams.lock().unwrap().drain() {
            let _ = conn.stream.shutdown(Shutdown::Both);
        }
    }
}

/// The client end: carries connections to `inside` on the remote machine
/// to the Unix socket `local` here. Sends `Open` on `writer`, reports on
/// `ready` once the server listens or failed, then relays until the
/// connection ends.
pub fn forward(
    mut reader: impl FrameRead + 'static,
    writer: impl FrameWrite + 'static,
    vm: String,
    inside: String,
    local: PathBuf,
    ready: SyncSender<bool>,
) {
    let writer = Arc::new(Mutex::new(writer));
    let send = {
        let writer = writer.clone();
        move |msg: Msg| {
            let mut buf = Vec::new();
            msg.encode(&mut buf);
            writer.lock().unwrap().write_frame(&buf).is_ok()
        }
    };
    let spec = OpenSpec {
        vm,
        program: PROGRAM.into(),
        args: vec![inside],
        ..OpenSpec::default()
    };
    if !send(Msg::Open(0, spec)) {
        let _ = ready.send(false);
        return;
    }
    let streams: Streams = Arc::default();
    let mut decoder = Decoder::default();
    let mut buf = Vec::new();
    let mut announced = false;
    let (frames, queue) = mpsc::channel::<Vec<u8>>();
    // Local connections send through one thread, in order.
    let queue_send = send.clone();
    thread::spawn(move || {
        while let Ok(frame) = queue.recv() {
            if !queue_send(Msg::Data(0, frame)) {
                break;
            }
        }
    });
    while let Ok(true) = reader.read_frame(&mut buf) {
        let Ok(msg) = Msg::decode(&buf) else { break };
        match msg {
            Msg::Data(_, data) => decoder.push(&data),
            Msg::OpenFailed(..) | Msg::Exit(..) => break,
            _ => continue,
        }
        loop {
            let frame = match decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => {
                    let _ = send(Msg::Close(0));
                    let _ = ready.send(announced);
                    return;
                }
            };
            match frame.kind {
                READY if !announced => {
                    announced = true;
                    let _ = ready.send(true);
                }
                OPEN => match UnixStream::connect(&local) {
                    Ok(stream) => {
                        let Ok(reader) = stream.try_clone() else {
                            continue;
                        };
                        let Ok(entry) = start_conn(stream) else {
                            continue;
                        };
                        streams.lock().unwrap().insert(frame.conn, entry);
                        let frames = frames.clone();
                        let conn = frame.conn;
                        thread::spawn(move || pump(reader, conn, |f| frames.send(f).is_ok()));
                    }
                    Err(_) => {
                        let _ = frames.send(Frame::encode(CLOSE, frame.conn, &[]));
                    }
                },
                _ => deliver(&streams, frame),
            }
        }
    }
    if !announced {
        let _ = ready.send(false);
    }
    for (_, conn) in streams.lock().unwrap().drain() {
        let _ = conn.stream.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::exec::OUTBOX_DEPTH;

    #[test]
    fn frames_survive_any_chunking() {
        let mut stream = Frame::encode(OPEN, 7, &[]);
        stream.extend(Frame::encode(DATA, 7, b"hello"));
        stream.extend(Frame::encode(CLOSE, 7, &[]));
        let mut decoder = Decoder::default();
        let mut frames = Vec::new();
        for byte in stream {
            decoder.push(&[byte]);
            while let Some(frame) = decoder.next_frame().unwrap() {
                frames.push(frame);
            }
        }
        assert_eq!(
            frames.iter().map(|f| (f.kind, f.conn)).collect::<Vec<_>>(),
            [(OPEN, 7), (DATA, 7), (CLOSE, 7)]
        );
        assert_eq!(frames[1].data, b"hello");
    }

    /// A relay channel listens, carries one connection both ways and
    /// removes its socket when closed.
    #[test]
    #[cfg(unix)]
    fn the_server_end_listens_privately_and_carries_connections() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.sock");
        let (out, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let spec = OpenSpec {
            program: PROGRAM.into(),
            args: vec![path.to_string_lossy().into_owned()],
            ..OpenSpec::default()
        };
        let mut channel = crate::exec::open_local(1, &spec, out).unwrap();
        let mut decoder = Decoder::default();
        let mut next = || loop {
            if let Some(frame) = decoder.next_frame().unwrap() {
                return frame;
            }
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Msg::Data(1, data) => decoder.push(&data),
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(next().kind, READY);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let mut client = UnixStream::connect(&path).unwrap();
        let open = next();
        assert_eq!(open.kind, OPEN);
        client.write_all(b"ping").unwrap();
        let data = next();
        assert_eq!((data.kind, data.data.as_slice()), (DATA, &b"ping"[..]));
        channel.input(Frame::encode(DATA, open.conn, b"pong"));
        channel.input(Frame::encode(CLOSE, open.conn, &[]));
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, b"pong");

        channel.kill();
        assert!(!path.exists());
    }

    /// Opens a relay channel on a socket in `dir` and connects to it.
    /// Returns the channel, the client end and its connection id.
    #[cfg(unix)]
    fn relay_with_client(
        dir: &Path,
    ) -> (
        Box<dyn Channel>,
        UnixStream,
        u32,
        PathBuf,
        mpsc::Receiver<Msg>,
    ) {
        let path = dir.join("relay.sock");
        let (out, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let spec = OpenSpec {
            program: PROGRAM.into(),
            args: vec![path.to_string_lossy().into_owned()],
            ..OpenSpec::default()
        };
        let channel = crate::exec::open_local(1, &spec, out).unwrap();
        let mut decoder = Decoder::default();
        let mut next = || loop {
            if let Some(frame) = decoder.next_frame().unwrap() {
                return frame;
            }
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Msg::Data(1, data) => decoder.push(&data),
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(next().kind, READY);
        let client = UnixStream::connect(&path).unwrap();
        let open = next();
        assert_eq!(open.kind, OPEN);
        (channel, client, open.conn, path, rx)
    }

    /// A local connection that reads nothing does not stop the channel:
    /// once too much waits for it, it is closed.
    #[test]
    #[cfg(unix)]
    fn a_connection_that_does_not_read_is_closed_not_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let (mut channel, mut client, conn, _, _rx) = relay_with_client(dir.path());
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            let chunk = Frame::encode(DATA, conn, &[b'x'; MAX_FRAME]);
            for _ in 0..(2 * CONN_LIMIT / MAX_FRAME + 64) {
                channel.input(chunk.clone());
            }
            let _ = done.send(channel);
        });
        let mut channel = finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the relay waited for a connection that does not read");
        // The client finds its connection closed after what was written.
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut got = Vec::new();
        let _ = client.read_to_end(&mut got);
        assert!(got.len() < (2 * CONN_LIMIT / MAX_FRAME + 64) * MAX_FRAME);
        channel.kill();
    }

    /// A large answer to a reader that falls behind arrives whole, and the
    /// channel's reader does not wait for it.
    #[test]
    #[cfg(unix)]
    fn a_slow_reader_gets_a_large_answer_whole() {
        let dir = tempfile::tempdir().unwrap();
        let (mut channel, mut client, conn, _, _rx) = relay_with_client(dir.path());
        let total = 6 << 20;
        let started = std::time::Instant::now();
        for _ in 0..total / MAX_FRAME {
            channel.input(Frame::encode(DATA, conn, &[b'x'; MAX_FRAME]));
        }
        channel.input(Frame::encode(CLOSE, conn, &[]));
        assert!(started.elapsed() < Duration::from_secs(5));
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), total);
        channel.kill();
    }

    /// After a frame that is not one, the relay closes instead of
    /// buffering everything that follows.
    #[test]
    #[cfg(unix)]
    fn a_bad_frame_closes_the_relay() {
        let dir = tempfile::tempdir().unwrap();
        let (mut channel, mut client, _, path, rx) = relay_with_client(dir.path());
        let mut bad = vec![DATA, 0, 0, 0, 1];
        bad.extend_from_slice(&((MAX_FRAME + 1) as u32).to_be_bytes());
        channel.input(bad);
        assert!(!path.exists(), "the socket goes with the relay");
        // The client learns that the relay ended.
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Msg::Exit(1, _) => break,
                Msg::Data(1, _) => continue,
                other => panic!("unexpected {other:?}"),
            }
        }
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut rest = Vec::new();
        assert_eq!(client.read_to_end(&mut rest).unwrap(), 0);
        // Later input is ignored, not kept.
        channel.input(vec![0; 1024]);
        channel.kill();
    }
}
