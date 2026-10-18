//! Records a real program session into a fixture under `tests/fixtures/<name>/`.
//!
//! Usage: `cargo run -p cmdterm-vt --example record -- <scenario>`, where `<scenario>` is
//! a name in `tests/fixtures/scenarios/<name>.toml` or a path to such a file.
//!
//! A scenario names a command, a terminal size, environment overrides, files to create
//! in the working directory and a script of steps:
//!
//! ```toml
//! command = "sh"
//! args = ["-c", "printf 'hello\\n'"]
//! rows = 24
//! cols = 80
//! [env]
//! FOO = "bar"
//! [files]
//! "a.txt" = "content"
//! [[step]]
//! wait_for = "hello"      # or wait_quiet_ms = 200
//! [[step]]
//! send = "exit\r"
//! [[step]]
//! resize = [30, 100]
//! [[step]]
//! sleep_ms = 100
//! ```
//!
//! The program runs in a clean environment: only a fixed PATH, TERM, LANG, a scratch
//! HOME and XDG directories, a fixed PS1, and the scenario's overrides. The tool refuses
//! to write a fixture whose output contains the real $USER, $HOME, the host name or a
//! home directory path. It then writes the fixture and its snapshot. Review the snapshot
//! before committing it, and note differences from xterm in COMPAT.md. Nothing here is
//! part of the library, which does no I/O; CI only replays the fixtures.

#[path = "../tests/fixture/mod.rs"]
mod fixture;

use cmdterm_env::pty::{CommandBuilder, Pty, PtySize};
use fixture::{Ev, Split};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

const PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const TERM: &str = "xterm-256color";
const LANG: &str = "C.UTF-8";
const PS1: &str = "$ ";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "d_rows")]
    rows: usize,
    #[serde(default = "d_cols")]
    cols: usize,
    /// Longest wait for a step or for the program to end.
    #[serde(default = "d_timeout")]
    timeout_ms: u64,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Variables whose value is a path under the real home directory, written `~/...` in
    /// the scenario (for example `RUSTUP_HOME = "~/.rustup"`). The path reaches the
    /// program but not the fixture; the record tool refuses output that shows it.
    #[serde(default)]
    host_paths: BTreeMap<String, String>,
    /// Files created in the working directory before the program starts.
    #[serde(default)]
    files: BTreeMap<String, String>,
    /// Command whose first output line is stored as the program version. Defaults to
    /// `<command> --version`; name the real program when `command` is a shell wrapper.
    version_command: Option<Vec<String>>,
    /// Command run after the session, with the same environment, to stop what the
    /// program left behind (a tmux server, for instance).
    cleanup: Option<Vec<String>>,
    #[serde(default, rename = "step")]
    steps: Vec<Step>,
}

fn d_rows() -> usize {
    24
}
fn d_cols() -> usize {
    80
}
fn d_timeout() -> u64 {
    10_000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    wait_for: Option<String>,
    wait_quiet_ms: Option<u64>,
    send: Option<String>,
    resize: Option<[usize; 2]>,
    sleep_ms: Option<u64>,
    /// Ends the session here: the program is killed, so the fixture keeps its screen
    /// (an alternate screen, for instance) as the final state.
    stop: Option<bool>,
}

/// The output captured so far, in the chunks the reads delivered.
struct Session {
    rx: Receiver<Vec<u8>>,
    output: Vec<u8>,
    events: Vec<Ev>,
    /// Where the next `wait_for` starts looking.
    scan_from: usize,
    closed: bool,
}

impl Session {
    /// Waits up to `wait` for one chunk. False when none came.
    fn pull(&mut self, wait: Duration) -> bool {
        match self.rx.recv_timeout(wait) {
            Ok(chunk) => {
                self.events.push(Ev::Chunk(chunk.len()));
                self.output.extend_from_slice(&chunk);
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => {
                self.closed = true;
                false
            }
        }
    }

    fn drain_ready(&mut self) {
        while self.pull(Duration::ZERO) {}
    }

    fn wait_for(&mut self, needle: &str, timeout: Duration) -> Result<(), String> {
        let end = Instant::now() + timeout;
        loop {
            let seen = String::from_utf8_lossy(&self.output[self.scan_from..]).into_owned();
            if let Some(i) = seen.find(needle) {
                self.scan_from += i + needle.len();
                return Ok(());
            }
            if self.closed || Instant::now() >= end {
                return Err(format!(
                    "waiting for {needle:?}: not seen in {:?}",
                    String::from_utf8_lossy(&self.output[self.scan_from..])
                ));
            }
            self.pull(Duration::from_millis(20));
        }
    }

    fn wait_quiet(&mut self, quiet: Duration, timeout: Duration) -> Result<(), String> {
        let end = Instant::now() + timeout;
        while self.pull(quiet) {
            if Instant::now() >= end {
                return Err("output never went quiet".into());
            }
        }
        Ok(())
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("record: {e}");
        std::process::exit(1);
    }
}

fn scenario_path(arg: &str) -> PathBuf {
    let p = Path::new(arg);
    if p.extension().is_some() || arg.contains('/') {
        return p.to_path_buf();
    }
    fixture::fixtures_dir()
        .join("scenarios")
        .join(format!("{arg}.toml"))
}

fn run() -> Result<(), String> {
    let arg = std::env::args()
        .nth(1)
        .ok_or("usage: record <scenario name or path>")?;
    let path = scenario_path(&arg);
    let name = path.file_stem().unwrap().to_string_lossy().into_owned();
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sc: Scenario = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;

    let scratch = std::env::temp_dir().join(format!("cmdterm-vt-record-{name}"));
    let _ = std::fs::remove_dir_all(&scratch);
    let result = record(&sc, &name, &scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn record(sc: &Scenario, name: &str, scratch: &Path) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let home = scratch.join("home");
    let work = scratch.join("work");
    for d in [
        "home",
        "work",
        "home/.config",
        "home/.local/share",
        "home/.cache",
        "home/.local/state",
    ] {
        std::fs::create_dir_all(scratch.join(d)).map_err(io)?;
    }
    for (file, content) in &sc.files {
        let p = work.join(file);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        std::fs::write(&p, content).map_err(io)?;
    }

    let version = program_version(sc)?;
    println!("program version: {version}");

    let mut cmd = CommandBuilder::new(&sc.command);
    cmd.args(&sc.args);
    cmd.env_clear();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in [
        ("PATH", PATH.to_string()),
        ("TERM", TERM.to_string()),
        ("LANG", LANG.to_string()),
        ("HOME", home.to_string_lossy().into_owned()),
        (
            "XDG_CONFIG_HOME",
            home.join(".config").to_string_lossy().into_owned(),
        ),
        (
            "XDG_DATA_HOME",
            home.join(".local/share").to_string_lossy().into_owned(),
        ),
        (
            "XDG_CACHE_HOME",
            home.join(".cache").to_string_lossy().into_owned(),
        ),
        (
            "XDG_STATE_HOME",
            home.join(".local/state").to_string_lossy().into_owned(),
        ),
        ("PS1", PS1.to_string()),
    ] {
        env.insert(k.into(), v);
    }
    for (k, v) in &sc.env {
        env.insert(k.clone(), v.clone());
    }
    if !sc.host_paths.is_empty() {
        let real = std::env::var("HOME").map_err(|_| "HOME is not set")?;
        for (k, v) in &sc.host_paths {
            let rest = v
                .strip_prefix("~/")
                .ok_or_else(|| format!("host_paths {k}: the value must start with ~/"))?;
            env.insert(k.clone(), format!("{real}/{rest}"));
        }
    }
    for (k, v) in &env {
        cmd.env(k, v);
    }
    cmd.cwd(&work);

    let pty = Pty::open(PtySize {
        rows: sc.rows as u16,
        cols: sc.cols as u16,
        pixel_width: 0,
        pixel_height: 0,
    })
    .map_err(|e| e.to_string())?;
    let mut child = pty.spawn(&cmd).map_err(|e| e.to_string())?;
    let mut reader = pty.reader().map_err(|e| e.to_string())?;
    let mut writer = pty.writer().map_err(|e| e.to_string())?;
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let timeout = Duration::from_millis(sc.timeout_ms);
    let mut s = Session {
        rx,
        output: Vec::new(),
        events: Vec::new(),
        scan_from: 0,
        closed: false,
    };
    let mut failure = None;
    let mut stopped = false;
    for (i, step) in sc.steps.iter().enumerate() {
        let r = (|| -> Result<(), String> {
            if let Some(n) = &step.wait_for {
                s.wait_for(n, timeout)?;
            }
            if let Some(ms) = step.wait_quiet_ms {
                s.wait_quiet(Duration::from_millis(ms), timeout)?;
            }
            if let Some(t) = &step.send {
                writer.write_all(t.as_bytes()).map_err(|e| e.to_string())?;
                writer.flush().map_err(|e| e.to_string())?;
            }
            if let Some([r, c]) = step.resize {
                // Output read so far belongs before the resize.
                s.drain_ready();
                pty.resize(PtySize {
                    rows: r as u16,
                    cols: c as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| e.to_string())?;
                s.events.push(Ev::Resize(r, c));
            }
            if let Some(ms) = step.sleep_ms {
                std::thread::sleep(Duration::from_millis(ms));
            }
            Ok(())
        })();
        if let Err(e) = r {
            failure = Some(format!("step {}: {e}", i + 1));
            break;
        }
        if step.stop == Some(true) {
            stopped = true;
            break;
        }
    }
    if let Some(e) = failure {
        let _ = child.kill();
        return Err(e);
    }

    if stopped {
        // Output the program prints while it dies (a hangup handler leaves the alternate
        // screen) must not reach the fixture, so nothing is read after this point.
        s.wait_quiet(Duration::from_millis(300), timeout)?;
        let _ = child.kill();
        let _ = child.wait();
        println!(
            "stopped by the scenario, {} bytes in {} events",
            s.output.len(),
            s.events.len()
        );
    } else {
        // The program should end by itself; read until the terminal closes.
        let end = Instant::now() + timeout;
        while !s.closed {
            s.pull(Duration::from_millis(20));
            if Instant::now() >= end {
                let _ = child.kill();
                return Err("the program did not end; add a step that makes it exit".into());
            }
        }
        let status = child.wait().map_err(io)?;
        println!(
            "program ended: {status:?}, {} bytes in {} events",
            s.output.len(),
            s.events.len()
        );
    }
    drop(writer);
    if let Some((prog, args)) = sc.cleanup.as_ref().and_then(|c| c.split_first()) {
        let _ = std::process::Command::new(prog)
            .args(args)
            .env_clear()
            .envs(&env)
            .current_dir(&work)
            .output();
    }

    refuse_personal_data(&s.output)?;

    let dir = fixture::fixtures_dir().join(name);
    std::fs::create_dir_all(&dir).map_err(io)?;
    let meta = meta_text(sc, &env, &version)?;
    std::fs::write(dir.join("meta.toml"), meta).map_err(io)?;
    std::fs::write(dir.join("output.bin"), &s.output).map_err(io)?;
    std::fs::write(dir.join("chunks.txt"), fixture::format_chunks(&s.events)).map_err(io)?;

    let fx = fixture::load(&dir)?;
    let ours = fixture::replay(&fx, Split::Recorded);
    let snap = fixture::render(&ours);
    std::fs::write(dir.join("snapshot.txt"), &snap).map_err(io)?;
    println!("wrote {}", dir.display());
    println!("{snap}");

    Ok(())
}

/// The first line the version command prints, run in the same clean environment.
fn program_version(sc: &Scenario) -> Result<String, String> {
    let argv = sc
        .version_command
        .clone()
        .unwrap_or_else(|| vec![sc.command.clone(), "--version".into()]);
    let (prog, args) = argv.split_first().ok_or("version_command is empty")?;
    let out = std::process::Command::new(prog)
        .args(args)
        .env_clear()
        .env("PATH", PATH)
        .env("LANG", LANG)
        .output()
        .map_err(|e| format!("{prog}: {e}"))?;
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| format!("{prog} printed no version"))?
        .to_string();
    refuse_personal_data(line.as_bytes())?;
    Ok(line)
}

/// Fails when the output holds anything that identifies the person who recorded it.
fn refuse_personal_data(out: &[u8]) -> Result<(), String> {
    let hay = String::from_utf8_lossy(out);
    let mut secrets: Vec<(&str, String)> = Vec::new();
    for var in ["USER", "LOGNAME", "HOME"] {
        if let Ok(v) = std::env::var(var) {
            secrets.push((var, v));
        }
    }
    let host = ["/etc/hostname", "/proc/sys/kernel/hostname"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_default();
    secrets.push(("hostname", host.trim().to_string()));
    for (what, value) in secrets {
        // A 1 or 2 character name matches everything; the path scan covers HOME.
        let long_enough = value.len() >= 3 && value != "/";
        if let Some(at) = hay.find(&value).filter(|_| long_enough) {
            let from = hay.floor_char_boundary(at.saturating_sub(30));
            let to = hay.ceil_char_boundary((at + value.len() + 30).min(hay.len()));
            return Err(format!(
                "the output contains your {what} ({value:?}); not writing the fixture. \
                 Change the scenario so the program does not print it. Context: {:?}",
                &hay[from..to]
            ));
        }
    }
    let leaks = fixture::path_leaks(out);
    if !leaks.is_empty() {
        return Err(format!(
            "the output contains {leaks:?}; not writing the fixture"
        ));
    }
    Ok(())
}

/// Days since 1970-01-01 as a civil date (Howard Hinnant's algorithm).
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn meta_text(
    sc: &Scenario,
    env: &BTreeMap<String, String>,
    version: &str,
) -> Result<String, String> {
    let (rows, cols) = (sc.rows, sc.cols);
    use toml::Value;
    let mut t = toml::Table::new();
    t.insert("program".into(), Value::String(sc.command.clone()));
    t.insert(
        "args".into(),
        Value::Array(sc.args.iter().cloned().map(Value::String).collect()),
    );
    t.insert("program_version".into(), Value::String(version.into()));
    t.insert("recorded_at".into(), Value::String(today()));
    t.insert(
        "tool_version".into(),
        Value::String(format!("cmdterm-vt record {}", env!("CARGO_PKG_VERSION"))),
    );
    let mut size = toml::Table::new();
    size.insert("rows".into(), Value::Integer(rows as i64));
    size.insert("cols".into(), Value::Integer(cols as i64));
    t.insert("size".into(), Value::Table(size));
    // The scratch paths differ per run, so only the names of those variables are kept.
    let mut summary = toml::Table::new();
    let scratch_vars = [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
    ];
    summary.insert("cleared".into(), Value::Boolean(true));
    summary.insert("PATH".into(), Value::String(PATH.into()));
    summary.insert("TERM".into(), Value::String(TERM.into()));
    summary.insert("LANG".into(), Value::String(LANG.into()));
    summary.insert("PS1".into(), Value::String(PS1.into()));
    summary.insert(
        "scratch_dirs".into(),
        Value::Array(
            scratch_vars
                .iter()
                .map(|v| Value::String((*v).into()))
                .collect(),
        ),
    );
    let overrides: Vec<Value> = sc
        .env
        .keys()
        .filter(|k| env.contains_key(*k))
        .map(|k| Value::String(format!("{k}={}", sc.env[k])))
        .collect();
    summary.insert("overrides".into(), Value::Array(overrides));
    summary.insert(
        "host_paths".into(),
        Value::Array(
            sc.host_paths
                .iter()
                .map(|(k, v)| Value::String(format!("{k}={v}")))
                .collect(),
        ),
    );
    t.insert("env".into(), Value::Table(summary));
    toml::to_string(&t).map_err(|e| e.to_string())
}
