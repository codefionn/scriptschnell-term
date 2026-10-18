//! `scriptschnell-term fix`: starts the user's coding agent, without the UI,
//! to repair a configuration that keeps the app from working. The agent runs
//! on this machine in the configuration directory, and the app types a
//! prompt into it that names the app's files. The prompt is not sent: the
//! user adds what broke and presses Enter.
//!
//! On Unix the agent gets a PTY of its own that this process relays to the
//! user's terminal, so the prompt can be typed into it once it drew its
//! input. Elsewhere the agent takes the terminal itself and the prompt is
//! printed for the user to paste.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use cmdterm_env::{AppDirs, Environment};

use crate::agents::{self, Agent, Status};

/// How long the agent's output must stay still before the prompt is typed.
const QUIET: Duration = Duration::from_millis(800);
/// How long to wait for the agent to turn on bracketed paste before typing
/// into one that never does.
const NO_PASTE_GRACE: Duration = Duration::from_secs(5);
/// The prompt is typed after this, even into output that never stops.
const MAX_WAIT: Duration = Duration::from_secs(30);

pub fn main(dirs: &AppDirs, agent: Option<&str>) -> ExitCode {
    let agent = match agent {
        Some(name) => match Agent::find(name) {
            Some(agent) => agent,
            None => {
                let known: Vec<&str> = Agent::all().iter().map(|agent| agent.program()).collect();
                eprintln!(
                    "scriptschnell-term: no agent named {name}; known agents: {}",
                    known.join(", ")
                );
                return ExitCode::FAILURE;
            }
        },
        None => match preferred(&Environment::Local) {
            Some(agent) => agent,
            None => {
                eprintln!(
                    "scriptschnell-term: found no coding agent on PATH; install one or name it with --agent"
                );
                return ExitCode::FAILURE;
            }
        },
    };
    if !on_path(agent.program()) {
        eprintln!("scriptschnell-term: {} is not on PATH", agent.program());
        return ExitCode::FAILURE;
    }
    if let Err(error) = std::fs::create_dir_all(&dirs.config) {
        eprintln!(
            "scriptschnell-term: cannot create {}: {error}",
            dirs.config.display()
        );
        return ExitCode::FAILURE;
    }
    let command = Environment::Local
        .command(agent.program())
        .args(extra_args(agent, dirs))
        .cwd(&dirs.config);
    let prompt = prompt(dirs);
    match start(&command, &prompt) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!(
                "scriptschnell-term: cannot run {}: {error}",
                agent.program()
            );
            ExitCode::FAILURE
        }
    }
}

/// The agent to start when the user names none: the first whose
/// integration is installed, else the first with a configuration
/// directory, else the first on `PATH`, in [`Agent::ALL`] order. Only
/// agents whose program is on `PATH` count.
fn preferred(env: &Environment) -> Option<Agent> {
    let found: Vec<(Agent, Status)> = (Agent::ALL.into_iter())
        .filter(|agent| on_path(agent.program()))
        .map(|agent| (agent, agents::status(env, agent)))
        .collect();
    let first = |wanted: fn(&Status) -> bool| {
        found
            .iter()
            .find(|(_, status)| wanted(status))
            .map(|(agent, _)| *agent)
    };
    first(|status| matches!(status, Status::On | Status::Outdated))
        .or_else(|| first(|status| !matches!(status, Status::NotFound)))
        .or_else(|| found.first().map(|(agent, _)| *agent))
}

/// Whether `program` is a file in one of `PATH`'s directories.
fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let names: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    std::env::split_paths(&path)
        .any(|dir| (names.iter()).any(|suffix| dir.join(format!("{program}{suffix}")).is_file()))
}

/// Arguments that let `agent` reach the app's other directories without
/// asking for each file.
fn extra_args(agent: Agent, dirs: &AppDirs) -> Vec<std::ffi::OsString> {
    if agent != Agent::ClaudeCode {
        return Vec::new();
    }
    let mut args = Vec::new();
    for dir in [&dirs.data, &dirs.state, &dirs.logs] {
        if dir != &dirs.config && !dir.starts_with(&dirs.config) && dir.is_dir() {
            args.push("--add-dir".into());
            args.push(dir.clone().into_os_string());
        }
    }
    args
}

/// What the app types into the agent: one line, so no agent sends part of
/// it on a newline, and short, so agents show it instead of folding it into
/// a "pasted text" label. It ends where the user describes the problem.
fn prompt(dirs: &AppDirs) -> String {
    let at = |dir: &std::path::Path, file: &str| dir.join(file).display().to_string();
    format!(
        "scriptschnell-term no longer works as it should; help me fix my setup. \
         It is not running. Its settings are in {settings}, my extensions in {extensions}, \
         projects and layouts in {state}, logs in {logs}. {reference} documents every \
         setting and {api} the extension API. Find the cause before you change anything, \
         back up each file you edit and tell me what you changed. What broke: ",
        settings = at(&dirs.config, "settings.json"),
        extensions = at(&dirs.config, "extensions"),
        state = at(&dirs.state, "state.json"),
        logs = dirs.logs.display(),
        reference = at(&dirs.data, "CONFIGURATION.md"),
        api = at(&dirs.data, "extensions/README.md"),
    )
}

/// Decides when to type the prompt into the agent, from the agent's output
/// and the user's keys. An agent may open with a dialog, such as Claude
/// Code asking whether to trust the folder, that drops what is pasted into
/// it. So the prompt counts as typed only once the agent shows its start;
/// until then each Enter the user presses types it again when the screen
/// settled, up to [`ATTEMPTS`] times.
struct Typist {
    prompt: String,
    /// The prompt's first characters without whitespace, as the agent's
    /// echo shows them once escape sequences and spaces are gone.
    marker: Vec<u8>,
    /// When the prompt may be typed next. `None` while it waits for Enter.
    armed: Option<Instant>,
    attempts: usize,
    /// The agent showed the prompt, or the session ended.
    done: bool,
    last_output: Option<Instant>,
    /// The agent turned on bracketed paste (`CSI ? 2004 h`).
    bracketed: bool,
    escape: Escape,
    /// The parameters of the control sequence being read.
    params: Vec<u8>,
    /// Printable output since the prompt was last typed, without whitespace.
    echo: Vec<u8>,
}

/// Where the output parser is in an escape sequence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    Ground,
    Esc,
    Csi,
    /// An OSC, DCS, APC, PM or SOS string, up to BEL or ST.
    Text,
    TextEsc,
}

/// How often the prompt is typed before the app gives up on it.
const ATTEMPTS: usize = 3;
/// The start of the label Claude Code and Codex show instead of a long
/// paste, without whitespace.
const FOLDED: &[u8] = b"[Pasted";
/// How many characters of the prompt have to show.
const MARKER_LEN: usize = 24;

impl Typist {
    fn new(prompt: String, now: Instant) -> Self {
        let marker = (prompt.bytes())
            .filter(|b| !b.is_ascii_whitespace())
            .take(MARKER_LEN)
            .collect();
        Self {
            prompt,
            marker,
            armed: Some(now),
            attempts: 0,
            done: false,
            last_output: None,
            bracketed: false,
            escape: Escape::Ground,
            params: Vec::new(),
            echo: Vec::new(),
        }
    }

    fn output(&mut self, chunk: &[u8], now: Instant) {
        if chunk.is_empty() {
            return;
        }
        self.last_output = Some(now);
        for &byte in chunk {
            self.escape = match (self.escape, byte) {
                (_, 0x1b) if self.escape != Escape::Text => Escape::Esc,
                (Escape::Ground, byte) => {
                    if byte > b' ' && byte != 0x7f && self.attempts > 0 {
                        self.echo.push(byte);
                    }
                    Escape::Ground
                }
                (Escape::Esc, b'[') => {
                    self.params.clear();
                    Escape::Csi
                }
                (Escape::Esc, b']' | b'P' | b'_' | b'^' | b'X') => Escape::Text,
                (Escape::Esc, _) => Escape::Ground,
                (Escape::Csi, 0x40..=0x7e) => {
                    self.control(byte);
                    Escape::Ground
                }
                (Escape::Csi, byte) => {
                    self.params.push(byte);
                    Escape::Csi
                }
                (Escape::Text, 0x07) => Escape::Ground,
                (Escape::Text, 0x1b) => Escape::TextEsc,
                (Escape::Text, _) => Escape::Text,
                (Escape::TextEsc, b'\\') => Escape::Ground,
                (Escape::TextEsc, _) => Escape::Text,
            };
        }
        let shows = |marker: &[u8]| {
            !marker.is_empty() && (self.echo.windows(marker.len())).any(|window| window == marker)
        };
        if shows(&self.marker) || shows(FOLDED) {
            self.done = true;
        }
        let keep = (self.echo.len()).saturating_sub(self.marker.len().max(FOLDED.len()));
        self.echo.drain(..keep);
    }

    /// A control sequence ended with `last`.
    fn control(&mut self, last: u8) {
        let Some(modes) = self.params.strip_prefix(b"?") else {
            return;
        };
        if matches!(last, b'h' | b'l') && modes.split(|&b| b == b';').any(|mode| mode == b"2004") {
            self.bracketed = last == b'h';
        }
    }

    /// Keys the user typed into the agent.
    fn input(&mut self, keys: &[u8], now: Instant) {
        if !self.done && self.armed.is_none() && self.attempts < ATTEMPTS && keys.contains(&b'\r') {
            self.armed = Some(now);
        }
    }

    /// The bytes to write into the agent now.
    fn due(&mut self, now: Instant) -> Option<Vec<u8>> {
        let armed = self.armed.filter(|_| !self.done)?;
        let waited = now.duration_since(armed);
        // Output since it was armed, which then stopped.
        let quiet = (self.last_output)
            .is_some_and(|last| last >= armed && now.duration_since(last) >= QUIET);
        let ready = quiet && (self.bracketed || waited >= NO_PASTE_GRACE);
        if !ready && waited < MAX_WAIT {
            return None;
        }
        self.armed = None;
        self.attempts += 1;
        self.echo.clear();
        Some(if self.bracketed {
            [b"\x1b[200~", self.prompt.as_bytes(), b"\x1b[201~"].concat()
        } else {
            self.prompt.clone().into_bytes()
        })
    }
}

/// Runs the agent in a PTY relayed to this terminal and types `prompt` into
/// it. Returns the agent's exit code.
#[cfg(unix)]
fn start(command: &cmdterm_env::EnvCommand, prompt: &str) -> std::io::Result<i32> {
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    let (cols, rows) = terminal_size().unwrap_or((80, 24));
    let pty = cmdterm_env::pty::Pty::open(cmdterm_env::pty::PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut builder = command.to_pty();
    // The user's own terminal, not the xterm the app's panels emulate.
    if let Some(term) = std::env::var_os("TERM") {
        builder.env("TERM", term);
    }
    let mut child = pty.spawn(&builder)?;
    let mut reader = pty.reader()?;
    let writer = Arc::new(Mutex::new(pty.writer()?));
    let master = Arc::new(pty);
    let typist = Arc::new(Mutex::new(Typist::new(prompt.to_owned(), Instant::now())));
    let _raw = RawMode::enter();

    let input = writer.clone();
    let keys = typist.clone();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; 16 * 1024];
        while let Ok(n @ 1..) = stdin.read(&mut buf) {
            if let Ok(mut typist) = keys.lock() {
                typist.input(&buf[..n], Instant::now());
            }
            let mut input = input
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if input
                .write_all(&buf[..n])
                .and_then(|()| input.flush())
                .is_err()
            {
                break;
            }
        }
    });
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH]) {
        let master = master.clone();
        std::thread::spawn(move || {
            for _ in signals.forever() {
                if let Some((cols, rows)) = terminal_size() {
                    let _ = master.resize(cmdterm_env::pty::PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
        });
    }
    {
        let typist = typist.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(50));
                let Ok(mut typist) = typist.lock() else {
                    return;
                };
                if typist.done || (typist.armed.is_none() && typist.attempts >= ATTEMPTS) {
                    return;
                }
                if let Some(bytes) = typist.due(Instant::now()) {
                    drop(typist);
                    let mut input = writer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let _ = input.write_all(&bytes).and_then(|()| input.flush());
                }
            }
        });
    }

    let mut stdout = std::io::stdout();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // End of input once the agent exited and the PTY closed.
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if let Ok(mut typist) = typist.lock() {
            typist.output(&buf[..n], Instant::now());
        }
        stdout.write_all(&buf[..n])?;
        stdout.flush()?;
    }
    if let Ok(mut typist) = typist.lock() {
        typist.done = true;
    }
    let status = child.wait()?;
    Ok(i32::try_from(status.exit_code()).unwrap_or(1))
}

/// Lets the agent take this terminal and prints `prompt` for the user to
/// paste, where there is no PTY relay.
#[cfg(not(unix))]
fn start(command: &cmdterm_env::EnvCommand, prompt: &str) -> std::io::Result<i32> {
    eprintln!("Paste this into the agent and say what broke:\n\n{prompt}\n");
    let status = command.to_std().status()?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(unix)]
fn terminal_size() -> Option<(u16, u16)> {
    let size = rustix::termios::tcgetwinsize(rustix::stdio::stdin()).ok()?;
    (size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
}

/// Puts stdin's terminal in raw mode until dropped, so keys reach the
/// agent's PTY unchanged.
#[cfg(unix)]
struct RawMode {
    saved: rustix::termios::Termios,
}

#[cfg(unix)]
impl RawMode {
    fn enter() -> Option<Self> {
        let stdin = rustix::stdio::stdin();
        if !rustix::termios::isatty(stdin) {
            return None;
        }
        let saved = rustix::termios::tcgetattr(stdin).ok()?;
        let mut raw = saved.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(stdin, rustix::termios::OptionalActions::Now, &raw).ok()?;
        Some(Self { saved })
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(
            rustix::stdio::stdin(),
            rustix::termios::OptionalActions::Now,
            &self.saved,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn the_prompt_waits_for_quiet_output_and_pastes_when_asked_to() {
        let start = Instant::now();
        let mut typist = Typist::new("fix it now".into(), start);
        assert_eq!(typist.due(at(start, 1000)), None);
        // The mode change arrives split across two reads.
        typist.output(b"> \x1b[?1004;20", at(start, 1000));
        typist.output(b"04h", at(start, 1100));
        assert_eq!(typist.due(at(start, 1500)), None);
        let typed = typist.due(at(start, 2000)).unwrap();
        assert_eq!(typed, b"\x1b[200~fix it now\x1b[201~");
        // The echo moves the cursor between words.
        typist.output(b"> fix\x1b[1Cit\x1b[1Cnow", at(start, 2100));
        assert!(typist.done);
        typist.input(b"\r", at(start, 2200));
        assert_eq!(typist.due(at(start, 9000)), None);
    }

    #[test]
    fn a_prompt_a_dialog_dropped_is_typed_again_after_enter() {
        let start = Instant::now();
        let mut typist = Typist::new("fix it".into(), start);
        typist.output(b"\x1b[?2004hTrust this folder?", at(start, 100));
        assert!(typist.due(at(start, 1000)).is_some());
        typist.output(b"\x1b]0;title\x07", at(start, 1100));
        assert!(!typist.done);
        // Keys other than Enter, and Enter before the screen changed, wait.
        typist.input(b"\x1b[B", at(start, 2000));
        assert_eq!(typist.due(at(start, 4000)), None);
        typist.input(b"\r", at(start, 4000));
        assert_eq!(typist.due(at(start, 6000)), None);
        typist.output(b"> ", at(start, 6100));
        assert!(typist.due(at(start, 7000)).is_some());
        typist.output(b"> fixit", at(start, 7100));
        assert!(typist.done);
    }

    #[test]
    fn a_folded_paste_counts_as_shown() {
        let start = Instant::now();
        let mut typist = Typist::new("fix it".into(), start);
        typist.output(b"\x1b[?2004h> ", at(start, 100));
        assert!(typist.due(at(start, 1000)).is_some());
        typist.output(b"> [Pasted\x1b[1CContent 1064 chars]", at(start, 1100));
        assert!(typist.done);
    }

    #[test]
    fn an_agent_without_bracketed_paste_gets_the_prompt_typed_later() {
        let start = Instant::now();
        let mut typist = Typist::new("fix it".into(), start);
        typist.output(b"\x1b[?2004h\x1b[?2004l> ", at(start, 100));
        assert_eq!(typist.due(at(start, 2000)), None);
        assert_eq!(
            typist.due(start + NO_PASTE_GRACE).as_deref(),
            Some(&b"fix it"[..])
        );
    }

    #[test]
    fn output_that_never_stops_still_gets_the_prompt() {
        let start = Instant::now();
        let mut typist = Typist::new("fix it".into(), start);
        typist.output(b"\x1b[?2004h", start + MAX_WAIT);
        assert!(typist.due(start + MAX_WAIT).is_some());
    }

    #[test]
    fn the_prompt_is_one_line_naming_the_files() {
        let dirs = AppDirs::under(std::path::Path::new("/cfg"));
        let prompt = prompt(&dirs);
        assert!(!prompt.contains('\n'));
        assert!(prompt.contains("/cfg/settings.json"));
        assert!(prompt.contains("/cfg/CONFIGURATION.md"));
        assert!(prompt.ends_with("What broke: "));
    }

    #[test]
    fn agents_are_found_by_program_and_name() {
        assert_eq!(Agent::find("claude"), Some(Agent::ClaudeCode));
        assert_eq!(Agent::find("claude code"), Some(Agent::ClaudeCode));
        assert_eq!(Agent::find("emacs"), None);
    }
}
