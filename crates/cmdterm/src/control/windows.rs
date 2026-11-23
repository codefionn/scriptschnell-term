//! The control socket's streams on Windows, which has had Unix sockets since
//! Windows 10 1803. The standard library offers them only on Unix, so they
//! come from `uds_windows`.
//!
//! A blocking read on such a socket sometimes misses its wakeup: the data or
//! the end of the stream has arrived, yet the read waits on. Seen on
//! Windows 11 with eight connections at once, where a few of every thousand
//! reads hung for good. A read that waits again after a short timeout finds
//! what arrived, so [`Stream`] reads in slices of [`SLICE`] up to the
//! timeout its caller set.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long one blocking read waits before it looks again.
const SLICE: Duration = Duration::from_millis(250);

pub struct Stream {
    inner: uds_windows::UnixStream,
    /// The caller's read timeout in milliseconds, 0 for none.
    timeout: AtomicU64,
}

impl Stream {
    fn new(inner: uds_windows::UnixStream) -> io::Result<Self> {
        inner.set_read_timeout(Some(SLICE))?;
        Ok(Self {
            inner,
            timeout: AtomicU64::new(0),
        })
    }

    #[cfg(test)]
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::new(uds_windows::UnixStream::connect(path)?)
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        let clone = Self::new(self.inner.try_clone()?)?;
        let timeout = self.timeout.load(Ordering::Relaxed);
        clone.timeout.store(timeout, Ordering::Relaxed);
        Ok(clone)
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        let millis = timeout.map_or(0, |timeout| {
            u64::try_from(timeout.as_millis())
                .unwrap_or(u64::MAX)
                .max(1)
        });
        self.timeout.store(millis, Ordering::Relaxed);
        Ok(())
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }

    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        self.inner.shutdown(how)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let start = Instant::now();
        loop {
            match self.inner.read(buf) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    let timeout = self.timeout.load(Ordering::Relaxed);
                    if timeout != 0 && start.elapsed() >= Duration::from_millis(timeout) {
                        return Err(error);
                    }
                }
                result => return result,
            }
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub struct Listener(uds_windows::UnixListener);

impl Listener {
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        uds_windows::UnixListener::bind(path).map(Self)
    }

    pub fn incoming(&self) -> impl Iterator<Item = io::Result<Stream>> + '_ {
        self.0.incoming().map(|stream| Stream::new(stream?))
    }
}

/// Removes the sockets in `dir` that no app listens on anymore. Unlike a
/// Unix runtime directory, the one on Windows outlives the session, so
/// what an app that crashed left would pile up. A socket file whose app
/// is gone refuses connections.
pub fn remove_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("control-") && name.ends_with(".sock")) {
            continue;
        }
        let path = entry.path();
        if uds_windows::UnixStream::connect(&path)
            .is_err_and(|error| error.kind() == io::ErrorKind::ConnectionRefused)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}
