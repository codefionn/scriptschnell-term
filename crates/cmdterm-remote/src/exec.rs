//! Running a channel's command on this machine.
//!
//! The server runs host channels here, and so does the guest agent inside a
//! Firecracker VM. Processes start through `cmdterm_env::Environment::Local`,
//! like every other process cmdterm starts.

use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cmdterm_env::Environment;
use cmdterm_env::pty::{ChildKiller, Pty, PtySize};

use crate::proto::{Msg, OpenSpec};

/// Where a channel sends its output and exit.
pub type Outbox = SyncSender<Msg>;

/// Size of the bounded outbox queue. A slow peer blocks output readers
/// once it fills, which pushes back on the commands.
pub const OUTBOX_DEPTH: usize = 256;

/// Input that may wait for one channel's program to read it, with or
/// without a PTY. Large enough that uploads and pastes fit while a program
/// or a VM starts.
pub(crate) const INPUT_LIMIT: usize = 64 << 20;
/// How long a connection's reader waits for room in the input of a channel
/// without a PTY, which holds up the connection's other channels. A
/// program that reads more slowly still gets all its input; one that lets
/// [`INPUT_LIMIT`] wait and reads nothing for this long is ended, so it
/// cannot stop the connection. Well past a VM's boot timeout, so input for
/// a command in a booting VM waits for it. Tests wait one second.
pub(crate) const INPUT_WAIT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 120 });

/// A running channel, driven by the connection's reader.
pub trait Channel: Send {
    fn input(&mut self, data: Vec<u8>);
    fn close_input(&mut self);
    fn resize(&mut self, cols: u16, rows: u16);
    /// Ends the command if it still runs. Called once, on close.
    fn kill(&mut self);
    /// Whether the client shows the output right now ([`Msg::Batch`]).
    /// Only sessions hold output back.
    ///
    /// [`Msg::Batch`]: crate::proto::Msg::Batch
    fn batch(&mut self, _on: bool) {}
}

/// The channel of a [`cmdterm_env::cpu::REMOTE_PROGRAM`] open, which has
/// already answered.
struct Answered;

impl Channel for Answered {
    fn input(&mut self, _data: Vec<u8>) {}
    fn close_input(&mut self) {}
    fn resize(&mut self, _cols: u16, _rows: u16) {}
    fn kill(&mut self) {}
}

/// Starts `spec` on this machine. Output goes to `out` on channel `id`.
pub fn open_local(id: u32, spec: &OpenSpec, out: Outbox) -> Result<Box<dyn Channel>, String> {
    if !spec.shell && spec.program == crate::relay::PROGRAM {
        return crate::relay::open(id, spec, out);
    }
    // CPU limits change in this process, which holds the terminals' job
    // objects on Windows.
    if !spec.shell && spec.program == cmdterm_env::cpu::REMOTE_PROGRAM {
        let (code, text) = cmdterm_env::cpu::answer(&spec.args);
        if !text.is_empty() {
            let _ = out.send(Msg::Data(id, text.into_bytes()));
        }
        let _ = out.send(Msg::Exit(id, code));
        return Ok(Box::new(Answered));
    }
    if spec.pty.is_some() {
        open_pty(id, spec, out)
    } else {
        open_pipes(id, spec, out)
    }
}

fn command(spec: &OpenSpec) -> cmdterm_env::EnvCommand {
    let (program, args) = if spec.shell {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        (shell, vec!["-l".to_owned()])
    } else {
        (spec.program.clone(), spec.args.clone())
    };
    let mut command = Environment::Local.command(program).args(args);
    if !spec.cwd.is_empty() {
        command = command.cwd(expand_home(&spec.cwd));
    }
    // A client's bubblewrap sandbox with the host's network runs this
    // machine's bridge from it, to scope itself away from the abstract
    // Unix sockets here.
    if let Some(bridge) = cmdterm_env::scope_exe() {
        command = command.var(cmdterm_env::SCOPE_EXE_VAR, bridge);
    }
    for (key, value) in &spec.vars {
        command = command.var(key, value);
    }
    command
}

/// `~` and `~/x` relative to this machine's home, as a remote shell would.
fn expand_home(path: &str) -> String {
    let home = || std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix('~') {
        Some("") => home(),
        Some(rest) if rest.starts_with('/') => format!("{}{rest}", home()),
        _ => path.to_owned(),
    }
}

/// Input for a writer thread, at most [`INPUT_LIMIT`] bytes. Dropping it
/// closes the program's input once the queued bytes are written.
type InputQueue = crate::queue::Sender<Vec<u8>>;

fn spawn_input(mut sink: impl Write + Send + 'static) -> InputQueue {
    let (tx, rx) = crate::queue::bounded::<Vec<u8>>(INPUT_LIMIT);
    thread::spawn(move || {
        while let Some(data) = rx.recv() {
            if sink.write_all(&data).and_then(|()| sink.flush()).is_err() {
                break;
            }
        }
    });
    tx
}

fn spawn_output(
    mut source: impl Read + Send + 'static,
    out: Outbox,
    wrap: impl Fn(Vec<u8>) -> Msg + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match source.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out.send(wrap(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

struct PipeChannel {
    id: u32,
    input: Option<InputQueue>,
    pid: u32,
    exited: Arc<Mutex<bool>>,
    out: Outbox,
}

fn open_pipes(id: u32, spec: &OpenSpec, out: Outbox) -> Result<Box<dyn Channel>, String> {
    let mut std = command(spec).to_std();
    std.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so closing the channel ends what it started.
    crate::sys::own_process_group(&mut std);
    let mut child = std
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", spec.program))?;
    let pid = child.id();
    let input = spawn_input(child.stdin.take().expect("piped stdin"));
    let stdout = spawn_output(
        child.stdout.take().expect("piped stdout"),
        out.clone(),
        move |d| Msg::Data(id, d),
    );
    let stderr = spawn_output(
        child.stderr.take().expect("piped stderr"),
        out.clone(),
        move |d| Msg::Stderr(id, d),
    );
    let exited = Arc::new(Mutex::new(false));
    let waiter_exited = exited.clone();
    let note = out.clone();
    thread::spawn(move || {
        let _ = stdout.join();
        let _ = stderr.join();
        // Wait without reaping first, so `kill` never signals a process
        // group whose leader's pid could already be reused.
        crate::sys::wait_without_reaping(pid);
        let code = {
            let mut exited = waiter_exited.lock().unwrap();
            *exited = true;
            child.wait().map(crate::sys::exit_code).unwrap_or(255)
        };
        let _ = out.send(Msg::Exit(id, code));
    });
    Ok(Box::new(PipeChannel {
        id,
        input: Some(input),
        pid,
        exited,
        out: note,
    }))
}

impl Channel for PipeChannel {
    fn input(&mut self, data: Vec<u8>) {
        let Some(input) = &self.input else { return };
        let cost = data.len();
        if input.send(data, cost, Some(INPUT_WAIT)) == Err(crate::queue::SendError::Full) {
            let note = format!(
                "scriptschnell-term-remote: the command read none of its input for {}s; ended it\n",
                INPUT_WAIT.as_secs()
            );
            let _ = self.out.try_send(Msg::Stderr(self.id, note.into_bytes()));
            self.kill();
        }
    }

    fn close_input(&mut self) {
        self.input.take();
    }

    fn resize(&mut self, _cols: u16, _rows: u16) {}

    fn kill(&mut self) {
        self.close_input();
        // Held across the kill: the waiter reaps the process only after it
        // set the flag under this lock, so the pid cannot be reused while
        // the group is signalled.
        let exited = self.exited.lock().unwrap();
        if !*exited {
            crate::sys::kill_group(self.pid);
        }
    }
}

struct PtyChannel {
    input: Option<InputQueue>,
    master: Pty,
    killer: ChildKiller,
}

fn open_pty(id: u32, spec: &OpenSpec, out: Outbox) -> Result<Box<dyn Channel>, String> {
    let (cols, rows) = spec.pty.unwrap_or((80, 24));
    let pty = Pty::open(pty_size(cols, rows))
        .map_err(|error| format!("could not open a PTY: {error}"))?;
    // The client's CPU limit, applied here as for this machine's own
    // terminals: a script on Unix, job objects on Windows.
    let cpu = cmdterm_env::CpuLimit::from_spec(&spec.cpu);
    let builder = if spec.shell {
        let cwd = (!spec.cwd.is_empty()).then(|| std::path::PathBuf::from(expand_home(&spec.cwd)));
        let mut builder =
            Environment::Local.limited_shell(cwd.as_deref(), &[], &[], None, cpu.as_ref());
        for (key, value) in &spec.vars {
            builder.env(key, value);
        }
        builder
    } else {
        command(spec).limit_cpu(cpu).to_pty()
    };
    let mut child = pty
        .spawn(&builder)
        .map_err(|error| format!("could not start {}: {error}", spec.program))?;
    let reader = pty.reader().map_err(|error| error.to_string())?;
    let writer = pty.writer().map_err(|error| error.to_string())?;
    let killer = child.killer();

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let output = out.clone();
    thread::spawn(move || {
        let mut reader = reader;
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if output.send(Msg::Data(id, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = done_tx.send(());
    });
    thread::spawn(move || {
        let code = child
            .wait()
            .map(|status| status.exit_code() as i32)
            .unwrap_or(255);
        // Let the reader forward the last output, but do not wait for
        // background processes that keep the PTY open.
        let _ = done_rx.recv_timeout(Duration::from_millis(500));
        let _ = out.send(Msg::Exit(id, code));
    });
    Ok(Box::new(PtyChannel {
        input: Some(spawn_input(writer)),
        master: pty,
        killer,
    }))
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl Channel for PtyChannel {
    fn input(&mut self, data: Vec<u8>) {
        // Typing and pastes never wait: input beyond [`INPUT_LIMIT`], for a
        // program that reads none of it, is dropped, as the connection's
        // reader must not wait on one terminal.
        if let Some(input) = &self.input {
            let cost = data.len();
            let _ = input.send(data, cost, Some(Duration::ZERO));
        }
    }

    fn close_input(&mut self) {
        // A PTY has no separate end of input; ^D is the user's to type.
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self.master.resize(pty_size(cols, rows));
    }

    fn kill(&mut self) {
        self.input.take();
        let _ = self.killer.kill();
    }
}

// They drive `sh`.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;

    fn collect(rx: &Receiver<Msg>) -> (Vec<u8>, Vec<u8>, i32) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Msg::Data(_, d) => out.extend(d),
                Msg::Stderr(_, d) => err.extend(d),
                Msg::Exit(_, code) => return (out, err, code),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn pipes_carry_stdin_stdout_stderr_and_exit() {
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let spec = OpenSpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "read l; printf \"out:$l:$V\"; printf err >&2; exit 4".into(),
            ],
            vars: vec![("V".into(), "x".into())],
            cwd: "/".into(),
            ..OpenSpec::default()
        };
        let mut channel = open_local(1, &spec, tx).unwrap();
        channel.input(b"hi\n".to_vec());
        channel.close_input();
        assert_eq!(collect(&rx), (b"out:hi:x".to_vec(), b"err".to_vec(), 4));
    }

    /// The CPU program answers in the server and starts nothing.
    #[test]
    fn cpu_channels_answer_from_the_server() {
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let spec = OpenSpec {
            program: cmdterm_env::cpu::REMOTE_PROGRAM.into(),
            args: vec!["topology".into()],
            ..OpenSpec::default()
        };
        open_local(1, &spec, tx).unwrap();
        let (out, _, code) = collect(&rx);
        if cfg!(target_os = "linux") {
            assert_eq!(code, 0);
            assert!(String::from_utf8_lossy(&out).starts_with("online "));
        }
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let spec = OpenSpec {
            program: cmdterm_env::cpu::REMOTE_PROGRAM.into(),
            args: vec!["fly".into()],
            ..OpenSpec::default()
        };
        open_local(1, &spec, tx).unwrap();
        assert_eq!(collect(&rx), (Vec::new(), Vec::new(), 1));
    }

    #[test]
    fn kill_ends_the_process_group() {
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let spec = OpenSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "sleep 30 & sleep 30".into()],
            ..OpenSpec::default()
        };
        let mut channel = open_local(1, &spec, tx).unwrap();
        thread::sleep(Duration::from_millis(100));
        channel.kill();
        assert_eq!(collect(&rx).2, 128 + 9);
    }

    /// A command that starts reading its input later than the wait, such
    /// as after a slow start, still gets all of it.
    #[test]
    fn a_command_that_reads_late_gets_all_its_input() {
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let spec = OpenSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "sleep 2; wc -c".into()],
            ..OpenSpec::default()
        };
        let mut channel = open_local(1, &spec, tx).unwrap();
        for _ in 0..48 {
            channel.input(vec![b'x'; 64 * 1024]);
        }
        channel.close_input();
        let (out, err, code) = collect(&rx);
        assert_eq!((code, err), (0, Vec::new()));
        assert_eq!(
            String::from_utf8_lossy(&out).trim(),
            (48 * 64 * 1024).to_string()
        );
    }

    /// A command that reads none of its input cannot make the connection
    /// hold more than the limit: the reader waits, then ends it.
    #[test]
    fn a_command_that_reads_no_input_is_ended_instead_of_buffering_it() {
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let spec = OpenSpec {
            program: "sleep".into(),
            args: vec!["30".into()],
            ..OpenSpec::default()
        };
        let mut channel = open_local(1, &spec, tx).unwrap();
        let started = std::time::Instant::now();
        // The pipe takes some, the queue its limit, then the reader waits.
        for _ in 0..(INPUT_LIMIT / (64 * 1024) + 64) {
            channel.input(vec![b'x'; 64 * 1024]);
            if started.elapsed() >= INPUT_WAIT {
                break;
            }
        }
        assert!(started.elapsed() >= INPUT_WAIT);
        let (_, err, code) = collect(&rx);
        assert!(String::from_utf8_lossy(&err).contains("read none of its input"));
        assert_eq!(code, 128 + 9);
    }

    #[test]
    fn pty_channels_echo_and_report_exit() {
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let spec = OpenSpec {
            pty: Some((40, 10)),
            program: "sh".into(),
            args: vec!["-c".into(), "stty size; read l; echo got:$l; exit 2".into()],
            ..OpenSpec::default()
        };
        let mut channel = open_local(1, &spec, tx).unwrap();
        channel.input(b"abc\r".to_vec());
        let (out, _, code) = collect(&rx);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("10 40"), "{text}");
        assert!(text.contains("got:abc"), "{text}");
        assert_eq!(code, 2);
    }

    #[test]
    fn pty_channels_start_in_home_without_a_cwd() {
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let spec = OpenSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "pwd -P".into()],
            pty: Some((80, 24)),
            ..OpenSpec::default()
        };
        let _channel = open_local(1, &spec, tx).unwrap();
        let (out, _, _) = collect(&rx);
        let home = std::fs::canonicalize(std::env::var("HOME").unwrap()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains(home.to_str().unwrap()), "{text}");
    }

    #[test]
    fn missing_programs_fail_to_open() {
        let (tx, _rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let spec = OpenSpec {
            program: "/nonexistent/cmdterm-test".into(),
            ..OpenSpec::default()
        };
        assert!(open_local(1, &spec, tx).is_err());
    }
}
