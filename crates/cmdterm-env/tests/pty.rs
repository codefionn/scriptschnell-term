//! PTY tests. The Unix ones drive `sh`, the Windows ones `cmd.exe`.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cmdterm_env::pty::{CommandBuilder, Pty, PtyReader, PtySize};

/// Collects everything the master reads on a thread, so tests can wait with a
/// deadline instead of blocking forever.
struct Collector {
    rx: mpsc::Receiver<Vec<u8>>,
    seen: Vec<u8>,
}

impl Collector {
    fn new(mut reader: PtyReader) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            rx,
            seen: Vec::new(),
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.seen).replace('\r', "")
    }

    /// Reads until `needle` shows up. Panics after ten seconds.
    fn until(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.text().contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(chunk) => self.seen.extend(chunk),
                Err(_) => panic!("no {needle:?} in {:?}", self.text()),
            }
        }
        self.text()
    }

    /// Reads until the reader reports end of input.
    fn drain(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(chunk) => self.seen.extend(chunk),
                Err(mpsc::RecvTimeoutError::Disconnected) => return self.text(),
                Err(mpsc::RecvTimeoutError::Timeout) => panic!("no end of input"),
            }
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;

    fn sh(script: &str) -> CommandBuilder {
        let mut b = CommandBuilder::new("sh");
        b.arg("-c").arg(script);
        b
    }

    fn run(script: &str) -> (String, u32) {
        run_with(sh(script), PtySize::default())
    }

    fn run_with(builder: CommandBuilder, size: PtySize) -> (String, u32) {
        let pty = Pty::open(size).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&builder).unwrap();
        let status = child.wait().unwrap();
        (out.drain(), status.exit_code())
    }

    #[test]
    fn prints_and_reads_a_line_from_stdin() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        let mut child = pty
            .spawn(&sh("echo ready; read line; echo got:$line"))
            .unwrap();
        out.until("ready");
        writer.write_all(b"hello\n").unwrap();
        out.until("got:hello");
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn child_sees_a_terminal() {
        let (_, code) = run("test -t 0 && test -t 1 && test -t 2");
        assert_eq!(code, 0);
    }

    #[test]
    fn child_is_a_session_leader_with_the_pty_as_controlling_terminal() {
        // `stat` and `tty` mean the same in procps and BSD ps, which macOS
        // has; BSD ps knows no `sid`. An `s` in the state marks a session
        // leader.
        let (text, _) = run("ps -o stat=,tty= -p $$; tty");
        let line = text
            .lines()
            .find(|l| l.split_whitespace().count() == 2)
            .unwrap_or_else(|| panic!("no ps line in {text:?}"));
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert!(fields[0].contains('s'), "a session leader: {line}");
        assert!(
            !fields[1].starts_with('?'),
            "has a controlling terminal: {line}"
        );
        assert!(text.contains("/dev/"), "tty names a device: {text:?}");
    }

    #[test]
    fn size_is_set_at_open_and_after_resize() {
        let (text, _) = run_with(sh("stty size"), PtySize::new(30, 100));
        assert!(text.contains("30 100"), "{text:?}");

        let pty = Pty::open(PtySize::new(10, 20)).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut child = pty
            .spawn(&sh(
                "trap 'echo winch; stty size' WINCH; echo up; while :; do sleep 0.1; done",
            ))
            .unwrap();
        out.until("up");
        pty.resize(PtySize::new(41, 133)).unwrap();
        out.until("winch");
        out.until("41 133");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn env_and_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = cmdterm_env::canonicalize(dir.path()).unwrap();
        let mut b = sh("echo A=$A B=$B C=$C TERM=$TERM; pwd");
        b.env("A", "one")
            .env("B", "two")
            .env_remove("B")
            .cwd(&dir_path);
        let (text, _) = run_with(b, PtySize::default());
        assert!(text.contains("A=one B= C="), "{text:?}");
        assert!(text.contains("TERM=xterm-256color"), "{text:?}");
        assert!(text.contains(dir_path.to_str().unwrap()), "{text:?}");

        let mut b = CommandBuilder::new("/usr/bin/env");
        b.env_clear().env("ONLY", "1").env("TERM", "dumb");
        let (text, _) = run_with(b, PtySize::default());
        let mut lines: Vec<&str> = text.lines().collect();
        lines.sort();
        assert_eq!(lines, ["ONLY=1", "TERM=dumb"]);
    }

    #[test]
    fn a_missing_cwd_falls_back_to_home() {
        let mut b = sh("pwd");
        b.cwd("/nonexistent/for/sure");
        let (text, _) = run_with(b, PtySize::default());
        let home = std::env::var("HOME").unwrap();
        assert_eq!(text.trim(), home);
    }

    #[test]
    fn exit_status_is_reported() {
        assert_eq!(run("exit 7").1, 7);
        let pty = Pty::open(PtySize::default()).unwrap();
        let _out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&sh("kill -KILL $$")).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(9));
        assert!(!status.success());
    }

    #[test]
    fn try_wait_does_not_block() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let _out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&sh("sleep 30")).unwrap();
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success());
        assert_eq!(child.try_wait().unwrap(), Some(status));
    }

    #[test]
    fn kill_works_from_another_thread() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&sh("echo started; sleep 30")).unwrap();
        let killer = child.killer();
        out.until("started");
        let started = Instant::now();
        std::thread::spawn(move || killer.kill().unwrap());
        let status = child.wait().unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(status.signal(), Some(1), "SIGHUP");
        // Killing a reaped child is not an error and signals nobody.
        child.killer().kill().unwrap();
    }

    #[test]
    fn without_a_cwd_the_child_starts_in_home() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&sh("pwd -P")).unwrap();
        child.wait().unwrap();
        let home = cmdterm_env::canonicalize(std::env::var_os("HOME").unwrap()).unwrap();
        assert!(out.drain().contains(home.to_str().unwrap()));
    }

    #[test]
    fn kill_ends_a_child_that_ignores_sighup() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut child = pty
            .spawn(&sh("trap '' HUP; echo started; while :; do sleep 1; done"))
            .unwrap();
        out.until("started");
        let started = Instant::now();
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(status.signal(), Some(9), "SIGKILL");
    }

    #[test]
    fn dropping_the_master_hangs_up_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("hup");
        let script = format!(
            "trap 'echo hup > {}; exit 0' HUP; echo ready; while :; do sleep 0.1; done",
            marker.display()
        );
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut reader = pty.reader().unwrap();
        let mut child = pty.spawn(&sh(&script)).unwrap();
        let mut seen = String::new();
        let mut buf = [0u8; 256];
        while !seen.contains("ready") {
            let n = reader.read(&mut buf).unwrap();
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        drop(reader);
        drop(pty);
        let status = child.wait().unwrap();
        assert!(status.success(), "{status:?}");
        assert!(marker.exists(), "the trap for SIGHUP ran");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn master_is_not_inherited_by_the_child() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&sh("ls -l /proc/self/fd")).unwrap();
        child.wait().unwrap();
        let text = out.drain();
        // Only 0, 1 and 2 are the terminal. Counting the descriptors would
        // also count those this test process inherited itself, which CI
        // runners pass on.
        let terminals: Vec<&str> = (text.lines())
            .filter_map(|line| line.split_once(" -> "))
            .filter(|(fd, target)| {
                let fd = fd.rsplit(' ').next().unwrap_or_default();
                !matches!(fd, "0" | "1" | "2")
                    && (target.contains("ptmx") || target.starts_with("/dev/pts/"))
            })
            .map(|(_, target)| target)
            .collect();
        assert!(terminals.is_empty(), "child also has {terminals:?}: {text}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn master_is_close_on_exec() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let _reader = pty.reader().unwrap();
        let mut found = 0;
        for entry in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            if !target.to_string_lossy().ends_with("/ptmx") {
                continue;
            }
            // Other tests open and close descriptors in parallel, so one may
            // be gone by now.
            let Ok(info) = std::fs::read_to_string(
                std::path::Path::new("/proc/self/fdinfo").join(entry.file_name()),
            ) else {
                continue;
            };
            let flags = info
                .lines()
                .find_map(|l| l.strip_prefix("flags:"))
                .map(|f| u32::from_str_radix(f.trim(), 8).unwrap())
                .unwrap();
            assert!(flags & 0o2000000 != 0, "O_CLOEXEC is set: {info}");
            found += 1;
        }
        assert!(found >= 1);
    }

    /// macOS drops what the master has not read yet when the last slave
    /// descriptor closes, and the read after that never returns. The app
    /// reads while the child runs, which the other tests cover there.
    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "macOS drops unread PTY output when the child exits"
    )]
    fn reading_after_exit_is_end_of_input() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut reader = pty.reader().unwrap();
        let mut child = pty.spawn(&sh("echo bye")).unwrap();
        child.wait().unwrap();
        let mut all = Vec::new();
        reader.read_to_end(&mut all).unwrap();
        assert!(String::from_utf8_lossy(&all).contains("bye"));
        assert_eq!(reader.read(&mut [0u8; 8]).unwrap(), 0);
    }

    #[test]
    fn foreground_group_is_the_child() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&sh("echo up; sleep 30")).unwrap();
        out.until("up");
        assert_eq!(pty.foreground_group(), Some(child.pid() as i32));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_missing_program_is_an_error() {
        let pty = Pty::open(PtySize::default()).unwrap();
        assert!(
            pty.spawn(&CommandBuilder::new("/nonexistent/prog"))
                .is_err()
        );
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    fn cmd(script: &str) -> CommandBuilder {
        let mut b = CommandBuilder::new("cmd.exe");
        b.arg("/d").arg("/c").arg(script);
        b
    }

    #[test]
    fn prints_and_reports_the_exit_code() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&cmd("echo hello-pty & exit 7")).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.exit_code(), 7);
        assert!(out.drain().contains("hello-pty"));
    }

    #[test]
    fn reads_a_line_from_stdin() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        let mut child = pty.spawn(&CommandBuilder::new("cmd.exe")).unwrap();
        writer.write_all(b"echo got-%OS%\r\n").unwrap();
        out.until("got-Windows_NT");
        writer.write_all(b"exit\r\n").unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn env_and_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = cmd("echo A=%A% B=%B% & cd");
        b.env("A", "one").env_remove("B").cwd(dir.path());
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&b).unwrap();
        child.wait().unwrap();
        let text = out.drain();
        assert!(text.contains("A=one"), "{text:?}");
        assert!(text.contains("B=%B%"), "{text:?}");
        // The last component only: `cd` may print the long form of a short
        // 8.3 temp path.
        let name = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_lowercase();
        assert!(text.to_lowercase().contains(&name), "{text:?}");
    }

    #[test]
    fn env_clear_keeps_the_variables_set_after_it() {
        let mut b = cmd("set");
        b.env_clear().env("ONLY", "1");
        if let Some(root) = std::env::var_os("SystemRoot") {
            b.env("SystemRoot", root);
        }
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&b).unwrap();
        child.wait().unwrap();
        let text = out.drain();
        assert!(text.contains("ONLY=1"), "{text:?}");
        assert!(!text.contains("USERPROFILE="), "{text:?}");
    }

    #[test]
    fn size_and_resize_reach_the_child() {
        let pty = Pty::open(PtySize::new(30, 100)).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        let mut child = pty.spawn(&CommandBuilder::new("powershell.exe")).unwrap();
        writer
            .write_all(b"[Console]::WindowWidth; [Console]::WindowHeight\r\n")
            .unwrap();
        out.until("100");
        pty.resize(PtySize::new(41, 133)).unwrap();
        writer
            .write_all(b"Start-Sleep -Milliseconds 500; [Console]::WindowWidth\r\n")
            .unwrap();
        out.until("133");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn kill_works_from_another_thread() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let _out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&cmd("ping -n 30 127.0.0.1 > nul")).unwrap();
        assert!(child.try_wait().unwrap().is_none());
        let killer = child.killer();
        std::thread::spawn(move || killer.kill().unwrap());
        let status = child.wait().unwrap();
        assert!(!status.success());
        child.killer().kill().unwrap();
    }

    #[test]
    fn reading_after_exit_is_end_of_input() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let mut child = pty.spawn(&cmd("echo bye")).unwrap();
        child.wait().unwrap();
        assert!(out.drain().contains("bye"));
    }

    #[test]
    fn dropping_the_master_ends_the_child_and_the_reader() {
        let pty = Pty::open(PtySize::default()).unwrap();
        let mut out = Collector::new(pty.reader().unwrap());
        let writer = pty.writer().unwrap();
        // `set /a` prints 42, which the window title does not hold.
        let mut child = pty
            .spawn(&cmd("set /a 40+2 & ping -n 30 127.0.0.1 > nul"))
            .unwrap();
        // A console closed before the child attached to it sends that child
        // no close event, so the test waits for its first output.
        out.until("42");
        assert!(child.try_wait().unwrap().is_none());
        drop(pty);
        drop(writer);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait().map(|_| ()));
        });
        rx.recv_timeout(Duration::from_secs(15))
            .expect("the child outlived the master")
            .unwrap();
        out.drain();
    }
}
