//! A small WebSocket client for Firefox's WebDriver BiDi endpoint: plain
//! `ws://` on loopback, text frames only. The reader and the writer own
//! separate clones of the stream, so the reader thread never blocks a
//! command.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;

pub struct Writer(Mutex<TcpStream>);

pub struct Reader {
    stream: BufReader<TcpStream>,
    /// A fragmented message waiting for its last frame.
    partial: Vec<u8>,
}

/// Opens `url` (`ws://host:port/path`).
pub fn connect(url: &str) -> io::Result<(Reader, Writer)> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| io::Error::other(format!("not a ws:// URL: {url}")))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = TcpStream::connect(host)?;
    stream.set_nodelay(true)?;
    let key = base64_key();
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if !status.split(' ').nth(1).is_some_and(|code| code == "101") {
        return Err(io::Error::other(format!(
            "the browser refused the WebSocket: {}",
            status.trim()
        )));
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if line == "\r\n" {
            break;
        }
    }
    Ok((
        Reader {
            stream: reader,
            partial: Vec::new(),
        },
        Writer(Mutex::new(stream)),
    ))
}

fn base64_key() -> String {
    use base64::Engine as _;
    let mut key = [0u8; 16];
    let _ = getrandom::fill(&mut key);
    base64::engine::general_purpose::STANDARD.encode(key)
}

impl Writer {
    pub fn send_text(&self, text: &str) -> io::Result<()> {
        self.frame(0x1, text.as_bytes())
    }

    fn frame(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut mask = [0u8; 4];
        let _ = getrandom::fill(&mut mask);
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        match payload.len() {
            len @ 0..=125 => frame.push(0x80 | len as u8),
            len @ 126..=0xffff => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        let mut stream = self.0.lock().unwrap_or_else(|p| p.into_inner());
        stream.write_all(&frame)?;
        stream.flush()
    }

    pub fn close(&self) {
        let _ = self.frame(0x8, &[]);
        let stream = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

impl Reader {
    /// The next text message, `None` once the browser closed the socket.
    /// Answers pings through `writer`.
    pub fn next(&mut self, writer: &Writer) -> io::Result<Option<String>> {
        loop {
            let mut head = [0u8; 2];
            match self.stream.read_exact(&mut head) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(error) => return Err(error),
            }
            let fin = head[0] & 0x80 != 0;
            let opcode = head[0] & 0x0f;
            let masked = head[1] & 0x80 != 0;
            let len = match head[1] & 0x7f {
                126 => {
                    let mut bytes = [0u8; 2];
                    self.stream.read_exact(&mut bytes)?;
                    u64::from(u16::from_be_bytes(bytes))
                }
                127 => {
                    let mut bytes = [0u8; 8];
                    self.stream.read_exact(&mut bytes)?;
                    u64::from_be_bytes(bytes)
                }
                len => u64::from(len),
            };
            if len > 256 * 1024 * 1024 {
                return Err(io::Error::other("WebSocket frame too large"));
            }
            let mut mask = [0u8; 4];
            if masked {
                self.stream.read_exact(&mut mask)?;
            }
            let mut payload = vec![0u8; len as usize];
            self.stream.read_exact(&mut payload)?;
            if masked {
                for (i, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[i % 4];
                }
            }
            match opcode {
                0x0..=0x2 => {
                    self.partial.extend_from_slice(&payload);
                    if fin {
                        let message = std::mem::take(&mut self.partial);
                        return String::from_utf8(message)
                            .map(Some)
                            .map_err(|_| io::Error::other("WebSocket text is not UTF-8"));
                    }
                }
                0x8 => return Ok(None),
                0x9 => writer.frame(0xA, &payload)?,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    /// A server that accepts the handshake, sends `frames` raw and returns
    /// what the client sent after the handshake.
    fn serve(frames: Vec<Vec<u8>>) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/session", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            let mut out = stream.try_clone().unwrap();
            out.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n")
                .unwrap();
            for frame in frames {
                out.write_all(&frame).unwrap();
            }
            let mut rest = Vec::new();
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(300)));
            let _ = reader.read_to_end(&mut rest);
            rest
        });
        (url, handle)
    }

    #[test]
    fn reads_fragmented_text_and_answers_pings() {
        let frames = vec![
            vec![0x01, 3, b'a', b'b', b'c'],
            vec![0x89, 1, b'p'],
            vec![0x80, 2, b'd', b'e'],
            vec![0x81, 126, 0, 200]
                .into_iter()
                .chain(std::iter::repeat_n(b'x', 200))
                .collect(),
            vec![0x88, 0],
        ];
        let (url, server) = serve(frames);
        let (mut reader, writer) = connect(&url).unwrap();
        assert_eq!(reader.next(&writer).unwrap().as_deref(), Some("abcde"));
        assert_eq!(reader.next(&writer).unwrap().map(|t| t.len()), Some(200));
        assert_eq!(reader.next(&writer).unwrap(), None);
        writer.send_text("hi").unwrap();
        writer.close();
        let sent = server.join().unwrap();
        // The pong comes first, masked like every client frame.
        assert_eq!(sent[0], 0x8A);
        assert_eq!(sent[1], 0x81);
        let pong: Vec<u8> = sent[6..7].iter().map(|b| b ^ sent[2]).collect();
        assert_eq!(pong, b"p");
        let text = &sent[7..];
        assert_eq!(text[0], 0x81);
        assert_eq!(text[1], 0x82);
        let mask = &text[2..6];
        let hi: Vec<u8> = text[6..8]
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i])
            .collect();
        assert_eq!(hi, b"hi");
    }
}
