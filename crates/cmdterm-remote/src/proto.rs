//! Channel messages carried in frames.
//!
//! Each message starts with a type byte and a channel id. Numbers are
//! big-endian; strings and byte runs carry a `u32` length.
//!
//! A client opens a channel with [`Msg::Open`], streams input with
//! [`Msg::Data`] and [`Msg::Eof`], and ends it with [`Msg::Close`], also after
//! the server has sent [`Msg::Exit`]. The server sends output as
//! [`Msg::Data`] and [`Msg::Stderr`], then one [`Msg::Exit`] or
//! [`Msg::OpenFailed`].
//!
//! A channel that names a session ([`OpenSpec::session`]) outlives its
//! connection ([`crate::session`]). The server answers it with
//! [`Msg::Attached`] before the session's output. Servers from before
//! sessions ignore the name and never send [`Msg::Attached`].
//!
//! A session's client that does not show the terminal right now sends
//! [`Msg::Batch`], and the server then holds its output back and sends it
//! in fewer, larger messages. Older servers end the connection on a type
//! they do not know, so a client sends it only to a server whose
//! [`Msg::Attached`] said it takes it.

use std::io;

/// What a channel runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenSpec {
    /// Firecracker VM to run in. Empty runs on the host.
    pub vm: String,
    /// Allocate a PTY of this size.
    pub pty: Option<(u16, u16)>,
    /// Run the login shell instead of `program`.
    pub shell: bool,
    pub cwd: String,
    pub program: String,
    pub args: Vec<String>,
    pub vars: Vec<(String, String)>,
    /// Name of a session that outlives the connection. Empty opens a
    /// channel that ends with it. Only PTY channels keep sessions.
    pub session: String,
    /// How many bytes of the session's output the client already has, when
    /// it attaches to the session again.
    pub resume: Option<u64>,
    /// The CPU limit the server starts the PTY's program under,
    /// [`cmdterm_env::CpuLimit::to_spec`]. Empty for none. Older servers
    /// skip it and start the program unlimited.
    pub cpu: String,
    /// An earlier open of this session may have reached the server, but
    /// the client never saw [`Msg::Attached`]. A running session attaches
    /// as usual. When the session ended and the server still keeps its
    /// exit, the server sends that exit and starts nothing; without one it
    /// starts the program. Encoded last, so older servers skip it and
    /// start the program as for any open without `resume`.
    pub exit_if_ended: bool,
    /// Picked by the client once for all opens of one run of the session,
    /// and kept by the server with the session and its exit. An
    /// [`Self::exit_if_ended`] open gets only the exit of the run with the
    /// same id, so the exit of an earlier program under the same name does
    /// not end it. `None` from clients that pick none. Encoded after the
    /// flag; older servers skip it.
    pub run: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    Open(u32, OpenSpec),
    /// Input from the client, or standard output (the PTY) from the server.
    Data(u32, Vec<u8>),
    Stderr(u32, Vec<u8>),
    /// The client's input ended.
    Eof(u32),
    /// New PTY size in columns and rows.
    Resize(u32, u16, u16),
    /// The command ended with this code; signals map to 128 + signal.
    Exit(u32, i32),
    OpenFailed(u32, String),
    /// The client is done with the channel. The server kills what still
    /// runs, or detaches a session's channel and keeps it running.
    Close(u32),
    /// The channel attached to its session. [`Msg::Data`] that follows is
    /// the session's output from this byte offset on; data sent before it
    /// restores the terminal's modes and counts toward no offset. The
    /// flag says whether the server takes [`Msg::Batch`].
    Attached(u32, u64, bool),
    /// Whether the client shows the session's output right now. While it
    /// does not, the server sends output in larger, rarer messages.
    Batch(u32, bool),
}

const OPEN: u8 = 1;
const DATA: u8 = 2;
const STDERR: u8 = 3;
const EOF: u8 = 4;
const RESIZE: u8 = 5;
const EXIT: u8 = 6;
const OPEN_FAILED: u8 = 7;
const CLOSE: u8 = 8;
const ATTACHED: u8 = 9;
const BATCH: u8 = 10;

impl Msg {
    pub fn channel(&self) -> u32 {
        match self {
            Self::Open(c, _)
            | Self::Data(c, _)
            | Self::Stderr(c, _)
            | Self::Eof(c)
            | Self::Resize(c, ..)
            | Self::Exit(c, _)
            | Self::OpenFailed(c, _)
            | Self::Close(c)
            | Self::Attached(c, ..)
            | Self::Batch(c, _) => *c,
        }
    }

    /// The same message on another channel.
    pub fn with_channel(mut self, channel: u32) -> Self {
        match &mut self {
            Self::Open(c, _)
            | Self::Data(c, _)
            | Self::Stderr(c, _)
            | Self::Eof(c)
            | Self::Resize(c, ..)
            | Self::Exit(c, _)
            | Self::OpenFailed(c, _)
            | Self::Close(c)
            | Self::Attached(c, ..)
            | Self::Batch(c, _) => *c = channel,
        }
        self
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        let (kind, channel) = match self {
            Self::Open(c, _) => (OPEN, c),
            Self::Data(c, _) => (DATA, c),
            Self::Stderr(c, _) => (STDERR, c),
            Self::Eof(c) => (EOF, c),
            Self::Resize(c, ..) => (RESIZE, c),
            Self::Exit(c, _) => (EXIT, c),
            Self::OpenFailed(c, _) => (OPEN_FAILED, c),
            Self::Close(c) => (CLOSE, c),
            Self::Attached(c, ..) => (ATTACHED, c),
            Self::Batch(c, _) => (BATCH, c),
        };
        out.push(kind);
        out.extend_from_slice(&channel.to_be_bytes());
        match self {
            Self::Open(_, spec) => {
                put_str(out, &spec.vm);
                match spec.pty {
                    Some((cols, rows)) => {
                        out.push(1);
                        out.extend_from_slice(&cols.to_be_bytes());
                        out.extend_from_slice(&rows.to_be_bytes());
                    }
                    None => out.push(0),
                }
                out.push(u8::from(spec.shell));
                put_str(out, &spec.cwd);
                put_str(out, &spec.program);
                out.extend_from_slice(&(spec.args.len() as u32).to_be_bytes());
                for arg in &spec.args {
                    put_str(out, arg);
                }
                out.extend_from_slice(&(spec.vars.len() as u32).to_be_bytes());
                for (key, value) in &spec.vars {
                    put_str(out, key);
                    put_str(out, value);
                }
                // Trailing, so a spec without a session encodes as before
                // and older peers skip what they do not know. A resume
                // offset counts as well: dropping it would make
                // decode(encode(m)) lose the offset.
                let more = spec.exit_if_ended || spec.run.is_some();
                if !spec.session.is_empty()
                    || !spec.cpu.is_empty()
                    || more
                    || spec.resume.is_some()
                {
                    put_str(out, &spec.session);
                    match spec.resume {
                        Some(offset) => {
                            out.push(1);
                            out.extend_from_slice(&offset.to_be_bytes());
                        }
                        None => out.push(0),
                    }
                }
                if !spec.cpu.is_empty() || more {
                    put_str(out, &spec.cpu);
                }
                if more {
                    out.push(u8::from(spec.exit_if_ended));
                }
                if let Some(run) = spec.run {
                    out.extend_from_slice(&run.to_be_bytes());
                }
            }
            Self::Data(_, bytes) | Self::Stderr(_, bytes) => out.extend_from_slice(bytes),
            Self::Resize(_, cols, rows) => {
                out.extend_from_slice(&cols.to_be_bytes());
                out.extend_from_slice(&rows.to_be_bytes());
            }
            Self::Exit(_, code) => out.extend_from_slice(&code.to_be_bytes()),
            Self::Attached(_, offset, batches) => {
                out.extend_from_slice(&offset.to_be_bytes());
                // Trailing, so older clients read the offset and skip it.
                if *batches {
                    out.push(1);
                }
            }
            Self::Batch(_, on) => out.push(u8::from(*on)),
            Self::OpenFailed(_, message) => put_str(out, message),
            Self::Eof(_) | Self::Close(_) => {}
        }
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut r = Reader(bytes);
        let kind = r.u8()?;
        let channel = r.u32()?;
        let msg = match kind {
            OPEN => {
                let vm = r.string()?;
                let pty = match r.u8()? {
                    0 => None,
                    _ => Some((r.u16()?, r.u16()?)),
                };
                let shell = r.u8()? != 0;
                let cwd = r.string()?;
                let program = r.string()?;
                let args = (0..r.count()?)
                    .map(|_| r.string())
                    .collect::<io::Result<_>>()?;
                let vars = (0..r.count()?)
                    .map(|_| Ok((r.string()?, r.string()?)))
                    .collect::<io::Result<_>>()?;
                let (session, resume) = if r.0.is_empty() {
                    (String::new(), None)
                } else {
                    let session = r.string()?;
                    let resume = match r.u8()? {
                        0 => None,
                        _ => Some(r.u64()?),
                    };
                    (session, resume)
                };
                let cpu = if r.0.is_empty() {
                    String::new()
                } else {
                    r.string()?
                };
                let exit_if_ended = !r.0.is_empty() && r.u8()? != 0;
                let run = if r.0.is_empty() { None } else { Some(r.u64()?) };
                Self::Open(
                    channel,
                    OpenSpec {
                        vm,
                        pty,
                        shell,
                        cwd,
                        program,
                        args,
                        vars,
                        session,
                        resume,
                        cpu,
                        exit_if_ended,
                        run,
                    },
                )
            }
            DATA => Self::Data(channel, r.rest()),
            STDERR => Self::Stderr(channel, r.rest()),
            EOF => Self::Eof(channel),
            RESIZE => Self::Resize(channel, r.u16()?, r.u16()?),
            EXIT => Self::Exit(channel, r.u32()? as i32),
            OPEN_FAILED => Self::OpenFailed(channel, r.string()?),
            CLOSE => Self::Close(channel),
            ATTACHED => Self::Attached(channel, r.u64()?, !r.0.is_empty() && r.u8()? != 0),
            BATCH => Self::Batch(channel, r.u8()? != 0),
            _ => return Err(bad()),
        };
        Ok(msg)
    }
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.0.len() < n {
            return Err(bad());
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| bad())?,
        ))
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| bad())?,
        ))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| bad())?,
        ))
    }

    /// An element count, bounded by the bytes left so a hostile count
    /// cannot make the decoder loop or allocate far beyond the frame.
    fn count(&mut self) -> io::Result<usize> {
        let n = self.u32()? as usize;
        if n > self.0.len() / 4 {
            return Err(bad());
        }
        Ok(n)
    }

    fn string(&mut self) -> io::Result<String> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| bad())
    }

    fn rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0).to_vec()
    }
}

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed channel message")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let messages = [
            Msg::Open(
                7,
                OpenSpec {
                    vm: "ci".into(),
                    pty: Some((80, 24)),
                    shell: false,
                    cwd: "/srv/a b".into(),
                    program: "git".into(),
                    args: vec!["status".into(), "-z".into()],
                    vars: vec![("GIT_PAGER".into(), "cat".into())],
                    ..OpenSpec::default()
                },
            ),
            Msg::Open(
                10,
                OpenSpec {
                    pty: Some((80, 24)),
                    shell: true,
                    session: "a1b2".into(),
                    resume: Some(1 << 40),
                    ..OpenSpec::default()
                },
            ),
            Msg::Open(
                11,
                OpenSpec {
                    session: "fresh".into(),
                    ..OpenSpec::default()
                },
            ),
            Msg::Open(
                16,
                OpenSpec {
                    shell: true,
                    session: "s".into(),
                    cpu: "scriptschnell-term-s:1:100:100".into(),
                    ..OpenSpec::default()
                },
            ),
            Msg::Open(
                17,
                OpenSpec {
                    program: "top".into(),
                    cpu: "scriptschnell-term-x:0:100:11".into(),
                    ..OpenSpec::default()
                },
            ),
            Msg::Open(
                18,
                OpenSpec {
                    pty: Some((80, 24)),
                    session: "s".into(),
                    exit_if_ended: true,
                    ..OpenSpec::default()
                },
            ),
            Msg::Open(
                19,
                OpenSpec {
                    pty: Some((80, 24)),
                    session: "s".into(),
                    run: Some(u64::MAX - 1),
                    ..OpenSpec::default()
                },
            ),
            Msg::Attached(12, 4096, false),
            Msg::Attached(13, 1, true),
            Msg::Batch(14, true),
            Msg::Batch(15, false),
            Msg::Open(1, OpenSpec::default()),
            Msg::Data(2, b"hi\0there".to_vec()),
            Msg::Stderr(3, Vec::new()),
            Msg::Eof(4),
            Msg::Resize(5, 120, 40),
            Msg::Exit(6, -1),
            Msg::OpenFailed(8, "no such vm".into()),
            Msg::Close(9),
        ];
        let mut buf = Vec::new();
        for msg in messages {
            msg.encode(&mut buf);
            assert_eq!(Msg::decode(&buf).unwrap(), msg);
        }
    }

    /// A spec without a session encodes as before sessions existed, so
    /// guest agents in older VM images still read it.
    #[test]
    fn specs_without_a_session_keep_the_old_encoding() {
        let spec = OpenSpec {
            program: "true".into(),
            ..OpenSpec::default()
        };
        let mut buf = Vec::new();
        Msg::Open(1, spec.clone()).encode(&mut buf);
        let mut named = Vec::new();
        Msg::Open(
            1,
            OpenSpec {
                session: "s".into(),
                ..spec.clone()
            },
        )
        .encode(&mut named);
        assert!(named.starts_with(&buf));
        assert_eq!(named.len(), buf.len() + 4 + 1 + 1);
        // A CPU limit comes after the session, where older servers stop
        // reading.
        let mut limited = Vec::new();
        Msg::Open(
            1,
            OpenSpec {
                session: "s".into(),
                cpu: "scriptschnell-term-s:1:100:100".into(),
                ..spec
            },
        )
        .encode(&mut limited);
        assert!(limited.starts_with(&named));
    }

    /// The flag of an open that may have reached the server before, and
    /// the run id, come after everything older servers read, so they decode
    /// the rest as before and start the program.
    #[test]
    fn exit_if_ended_and_run_come_after_what_older_servers_read() {
        let spec = OpenSpec {
            pty: Some((80, 24)),
            session: "s".into(),
            cpu: "scriptschnell-term-s:1:100:100".into(),
            ..OpenSpec::default()
        };
        let mut old = Vec::new();
        Msg::Open(1, spec.clone()).encode(&mut old);
        let mut new = Vec::new();
        Msg::Open(
            1,
            OpenSpec {
                exit_if_ended: true,
                ..spec.clone()
            },
        )
        .encode(&mut new);
        assert_eq!(new.len(), old.len() + 1);
        assert!(new.starts_with(&old));
        // The run id comes after the flag.
        let mut with_run = Vec::new();
        Msg::Open(
            1,
            OpenSpec {
                exit_if_ended: true,
                run: Some(7),
                ..spec.clone()
            },
        )
        .encode(&mut with_run);
        assert_eq!(with_run.len(), new.len() + 8);
        assert!(with_run.starts_with(&new));
        // Without a CPU limit, an empty one goes in front of the flag.
        let bare = OpenSpec {
            cpu: String::new(),
            exit_if_ended: true,
            run: Some(9),
            ..spec
        };
        let mut buf = Vec::new();
        Msg::Open(1, bare.clone()).encode(&mut buf);
        assert_eq!(Msg::decode(&buf).unwrap(), Msg::Open(1, bare));
    }

    /// An `Attached` from a server that takes no `Batch` encodes as before,
    /// so older clients read it.
    #[test]
    fn attached_without_batches_keeps_the_old_encoding() {
        let mut old = Vec::new();
        Msg::Attached(1, 7, false).encode(&mut old);
        assert_eq!(old.len(), 1 + 4 + 8);
        let mut new = Vec::new();
        Msg::Attached(1, 7, true).encode(&mut new);
        assert!(new.starts_with(&old));
    }

    /// A resume offset with no session name must survive a re-encode: the
    /// relay re-encodes every message it forwards, and the encoder used to
    /// drop the offset when the session name was empty (found by fuzzing).
    #[test]
    fn resume_offset_without_a_session_round_trips() {
        // The smallest input the fuzzer reduced to this: an Open whose only
        // trailing field is a resume offset.
        let wire: &[u8] = &[
            0x01, 0, 0, 0, 7, // OPEN, channel 7
            0, 0, 0, 0, // vm
            0, 0, // pty: none, shell: false
            0, 0, 0, 0, // cwd
            0, 0, 0, 0, // program
            0, 0, 0, 0, // args
            0, 0, 0, 0, // vars
            0, 0, 0, 0, // session: ""
            1, // resume flag
            0, 0, 0, 0, 0, 0, 0, 42, // offset
        ];
        let msg = Msg::decode(wire).unwrap();
        let Msg::Open(_, spec) = &msg else {
            panic!("not an Open: {msg:?}");
        };
        assert_eq!(spec.resume, Some(42));
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(Msg::decode(&buf).unwrap(), msg);
    }

    /// Every combination of the trailing fields survives encode+decode.
    #[test]
    fn every_trailing_field_combination_round_trips() {
        for session in ["", "s"] {
            for resume in [None, Some(42)] {
                for cpu in ["", "c"] {
                    for exit_if_ended in [false, true] {
                        for run in [None, Some(7)] {
                            let msg = Msg::Open(
                                3,
                                OpenSpec {
                                    session: session.into(),
                                    resume,
                                    cpu: cpu.into(),
                                    exit_if_ended,
                                    run,
                                    ..OpenSpec::default()
                                },
                            );
                            let mut buf = Vec::new();
                            msg.encode(&mut buf);
                            assert_eq!(Msg::decode(&buf).unwrap(), msg, "spec {msg:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn truncated_and_hostile_messages_fail() {
        let mut buf = Vec::new();
        Msg::Resize(1, 2, 3).encode(&mut buf);
        assert!(Msg::decode(&buf[..buf.len() - 1]).is_err());
        assert!(Msg::decode(&[99, 0, 0, 0, 0]).is_err());
        // An Open whose argument count claims four billion entries.
        let mut open = vec![OPEN, 0, 0, 0, 1];
        open.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        open.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(Msg::decode(&open).is_err());
    }
}
