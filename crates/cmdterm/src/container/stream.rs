//! Runs a command and copies its output to a log line by line as it arrives.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;

use cmdterm_env::EnvCommand;

/// How many stderr lines a failure message quotes.
const TAIL_LINES: usize = 5;

pub struct Ran {
    pub status: ExitStatus,
    /// Every stdout line, without line endings.
    pub stdout: Vec<String>,
    /// The last few stderr lines, for error messages.
    pub stderr_tail: Vec<String>,
}

enum Line {
    Out(Vec<u8>),
    Err(Vec<u8>),
}

/// Runs `command` to completion. Stderr always goes to `log`; stdout goes
/// there too when `log_stdout` is set. Reader threads only send lines over a
/// channel, so `log` stays on the calling thread.
pub fn run(command: &EnvCommand, log: &mut dyn Write, log_stdout: bool) -> io::Result<Ran> {
    let mut child = command
        .to_std()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (tx, rx) = mpsc::channel();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let tx = tx.clone();
        readers.push(thread::spawn(move || read_lines(stdout, &tx, Line::Out)));
    }
    if let Some(stderr) = child.stderr.take() {
        let tx = tx.clone();
        readers.push(thread::spawn(move || read_lines(stderr, &tx, Line::Err)));
    }
    drop(tx);

    let mut stdout = Vec::new();
    let mut tail = VecDeque::with_capacity(TAIL_LINES);
    for line in rx {
        // A failing log must not fail the command, so write errors are dropped.
        match line {
            Line::Out(bytes) => {
                if log_stdout {
                    let _ = log.write_all(&bytes);
                }
                stdout.push(trimmed(&bytes));
            }
            Line::Err(bytes) => {
                let _ = log.write_all(&bytes);
                let text = trimmed(&bytes);
                if !text.trim().is_empty() {
                    if tail.len() == TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(text);
                }
            }
        }
        let _ = log.flush();
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child.wait()?;
    Ok(Ran {
        status,
        stdout,
        stderr_tail: tail.into(),
    })
}

fn read_lines(pipe: impl Read, tx: &mpsc::Sender<Line>, wrap: fn(Vec<u8>) -> Line) {
    let mut reader = BufReader::new(pipe);
    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if !line.ends_with(b"\n") {
                    line.push(b'\n');
                }
                if tx.send(wrap(line)).is_err() {
                    return;
                }
            }
        }
    }
}

fn trimmed(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches(['\n', '\r'])
        .to_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use cmdterm_env::Environment;

    #[test]
    fn copies_both_streams_and_keeps_stdout() {
        let command = Environment::Local
            .command("sh")
            .args(["-c", "echo out1; echo err1 >&2; printf out2; exit 3"]);
        let mut log = Vec::new();
        let ran = run(&command, &mut log, false).unwrap();
        assert_eq!(ran.status.code(), Some(3));
        assert_eq!(ran.stdout, ["out1", "out2"]);
        assert_eq!(ran.stderr_tail, ["err1"]);
        assert_eq!(String::from_utf8(log).unwrap(), "err1\n");

        let mut log = Vec::new();
        run(&command, &mut log, true).unwrap();
        let log = String::from_utf8(log).unwrap();
        assert!(log.contains("out1\n") && log.contains("out2\n") && log.contains("err1\n"));
    }
}
