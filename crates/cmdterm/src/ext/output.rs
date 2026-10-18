//! Line-prefixing writer for extension output.

use std::io::{self, Write};

/// Writes every complete line as `<prefix><line>\n` to `out` with one
/// `write_all`. A trailing partial line is held until its newline arrives or
/// the writer drops. `flush` is a no-op on purpose: the runtime flushes after
/// each unbuffered write, which would split `io.write("a") io.write("b\n")`
/// into two prefixed lines.
pub(crate) struct PrefixWriter<W: Write> {
    prefix_len: usize,
    /// The prefix followed by the pending partial line.
    line: Vec<u8>,
    out: W,
}

/// Longest line held before it is written out without waiting for a newline.
const MAX_LINE: usize = 64 * 1024;

impl<W: Write> PrefixWriter<W> {
    pub(crate) fn new(prefix: &str, out: W) -> Self {
        let mut line = Vec::with_capacity(prefix.len() + 128);
        line.extend_from_slice(prefix.as_bytes());
        Self {
            prefix_len: prefix.len(),
            line,
            out,
        }
    }

    fn emit(&mut self) {
        self.line.push(b'\n');
        // Output is best effort: a closed stderr must not fail the script.
        let _ = self.out.write_all(&self.line);
        self.line.truncate(self.prefix_len);
    }
}

impl<W: Write> Write for PrefixWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut rest = bytes;
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            self.line.extend_from_slice(&rest[..end]);
            self.emit();
            rest = &rest[end + 1..];
        }
        self.line.extend_from_slice(rest);
        if self.line.len() - self.prefix_len > MAX_LINE {
            self.emit();
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write> Drop for PrefixWriter<W> {
    fn drop(&mut self) {
        if self.line.len() > self.prefix_len {
            self.emit();
        }
        let _ = self.out.flush();
    }
}
