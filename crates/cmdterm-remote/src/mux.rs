//! Serving channels over one frame stream.
//!
//! The server runs this after the handshake, and the guest agent runs it on
//! each vsock connection. A [`Backend`] decides where a channel runs. At
//! most [`MAX_CHANNELS`] channels run at once per connection; further opens
//! fail, so one peer cannot fill the machine with processes and threads.

use std::collections::HashMap;
use std::io;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use crate::exec::{self, Channel, OUTBOX_DEPTH, Outbox};
use crate::proto::{Msg, OpenSpec};
use crate::transport::{FrameRead, FrameWrite};

/// Most channels one connection may run at once.
pub const MAX_CHANNELS: usize = 1024;

/// Starts channels.
pub trait Backend {
    /// Starts `spec` as channel `id`. Output and the exit go to `out`.
    fn open(&self, id: u32, spec: &OpenSpec, out: Outbox) -> Result<Box<dyn Channel>, String>;
}

/// Runs every channel on this machine.
pub struct LocalBackend;

impl Backend for LocalBackend {
    fn open(&self, id: u32, spec: &OpenSpec, out: Outbox) -> Result<Box<dyn Channel>, String> {
        if !spec.vm.is_empty() {
            return Err(format!("this machine has no VM named {}", spec.vm));
        }
        exec::open_local(id, spec, out)
    }
}

/// Writes queued messages to `writer` until every sender is gone or a write
/// fails.
pub fn spawn_writer(writer: impl FrameWrite + 'static, rx: Receiver<Msg>) {
    spawn_writer_closing(writer, rx, || {});
}

/// [`spawn_writer`] that runs `close` when a write fails. `close` shuts the
/// connection down, so its reader ends too instead of serving channels
/// whose output goes nowhere.
pub fn spawn_writer_closing(
    mut writer: impl FrameWrite + 'static,
    rx: Receiver<Msg>,
    close: impl FnOnce() + Send + 'static,
) {
    thread::spawn(move || {
        let mut buf = Vec::new();
        while let Ok(msg) = rx.recv() {
            msg.encode(&mut buf);
            if writer.write_frame(&buf).is_err() {
                close();
                return;
            }
        }
    });
}

/// Serves channels until the peer closes the stream. Channels still open
/// then are killed.
pub fn serve(
    reader: impl FrameRead,
    writer: impl FrameWrite + 'static,
    backend: &dyn Backend,
) -> io::Result<()> {
    serve_closing(reader, writer, backend, || {})
}

/// [`serve`] that runs `close` when a write fails. `close` must make the
/// reader fail, usually by shutting the socket down; then the channels end.
pub fn serve_closing(
    mut reader: impl FrameRead,
    writer: impl FrameWrite + 'static,
    backend: &dyn Backend,
    close: impl FnOnce() + Send + 'static,
) -> io::Result<()> {
    let (out, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
    spawn_writer_closing(writer, rx, close);
    let mut channels: HashMap<u32, Box<dyn Channel>> = HashMap::new();
    let mut buf = Vec::new();
    let result = loop {
        match reader.read_frame(&mut buf) {
            Ok(true) => {}
            Ok(false) => break Ok(()),
            Err(error) => break Err(error),
        }
        let msg = match Msg::decode(&buf) {
            Ok(msg) => msg,
            Err(error) => break Err(error),
        };
        match msg {
            Msg::Open(id, spec) => {
                if channels.contains_key(&id) {
                    let _ = out.send(Msg::OpenFailed(id, "channel id in use".into()));
                    continue;
                }
                if channels.len() >= MAX_CHANNELS {
                    let _ = out.send(Msg::OpenFailed(id, "too many channels open".into()));
                    continue;
                }
                match backend.open(id, &spec, out.clone()) {
                    Ok(channel) => {
                        channels.insert(id, channel);
                    }
                    Err(message) => {
                        let _ = out.send(Msg::OpenFailed(id, message));
                    }
                }
            }
            Msg::Data(id, data) => {
                if let Some(channel) = channels.get_mut(&id) {
                    channel.input(data);
                }
            }
            Msg::Eof(id) => {
                if let Some(channel) = channels.get_mut(&id) {
                    channel.close_input();
                }
            }
            Msg::Resize(id, cols, rows) => {
                if let Some(channel) = channels.get_mut(&id) {
                    channel.resize(cols, rows);
                }
            }
            Msg::Batch(id, on) => {
                if let Some(channel) = channels.get_mut(&id) {
                    channel.batch(on);
                }
            }
            Msg::Close(id) => {
                if let Some(mut channel) = channels.remove(&id) {
                    channel.kill();
                }
            }
            // Server-to-client messages have no meaning here.
            Msg::Stderr(..) | Msg::Exit(..) | Msg::OpenFailed(..) | Msg::Attached(..) => {}
        }
    };
    for (_, mut channel) in channels {
        channel.kill();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::transport::Plain;

    struct Stub;

    impl Channel for Stub {
        fn input(&mut self, _data: Vec<u8>) {}
        fn close_input(&mut self) {}
        fn resize(&mut self, _cols: u16, _rows: u16) {}
        fn kill(&mut self) {}
    }

    struct Counting(Arc<Mutex<usize>>);

    impl Backend for Counting {
        fn open(
            &self,
            _id: u32,
            _spec: &OpenSpec,
            _out: Outbox,
        ) -> Result<Box<dyn Channel>, String> {
            *self.0.lock().unwrap() += 1;
            Ok(Box::new(Stub))
        }
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<Vec<u8>>>>);

    impl FrameWrite for Sink {
        fn write_frame(&mut self, payload: &[u8]) -> io::Result<()> {
            self.0.lock().unwrap().push(payload.to_vec());
            Ok(())
        }
    }

    #[cfg(unix)]
    struct Failing;

    #[cfg(unix)]
    impl FrameWrite for Failing {
        fn write_frame(&mut self, _payload: &[u8]) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }

    /// Answers every open with output, which the writer cannot send.
    #[cfg(unix)]
    struct Talking;

    #[cfg(unix)]
    impl Backend for Talking {
        fn open(&self, id: u32, _spec: &OpenSpec, out: Outbox) -> Result<Box<dyn Channel>, String> {
            let _ = out.send(Msg::Data(id, b"hi".to_vec()));
            Ok(Box::new(Stub))
        }
    }

    /// When the writer fails, the connection closes, so the reader and its
    /// channels end instead of waiting for a peer that never hears back.
    #[test]
    #[cfg(unix)]
    fn a_failed_write_ends_the_connection() {
        let (ours, mut theirs) = crate::sys::UnixStream::pair().unwrap();
        let shutdown = ours.try_clone().unwrap();
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            let result = serve_closing(Plain(ours), Failing, &Talking, move || {
                let _ = shutdown.shutdown(std::net::Shutdown::Both);
            });
            let _ = done.send(result);
        });
        let mut buf = Vec::new();
        Msg::Open(1, OpenSpec::default()).encode(&mut buf);
        Plain(&mut theirs).write_frame(&buf).unwrap();
        // The peer stays connected and silent; the server ends anyway.
        assert!(finished.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    /// One connection runs only so many channels at once.
    #[test]
    fn too_many_opens_fail_instead_of_starting_channels() {
        let mut plain = Plain(Vec::new());
        let mut buf = Vec::new();
        for id in 0..=MAX_CHANNELS as u32 {
            Msg::Open(id, OpenSpec::default()).encode(&mut buf);
            plain.write_frame(&buf).unwrap();
        }
        let opens = Arc::new(Mutex::new(0));
        let sink = Sink::default();
        serve(
            Plain(Cursor::new(plain.0)),
            sink.clone(),
            &Counting(opens.clone()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while sink.0.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "no OpenFailed was sent");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*opens.lock().unwrap(), MAX_CHANNELS);
        let frames = sink.0.lock().unwrap();
        assert_eq!(frames.len(), 1);
        assert!(matches!(
            Msg::decode(&frames[0]).unwrap(),
            Msg::OpenFailed(..)
        ));
    }
}
