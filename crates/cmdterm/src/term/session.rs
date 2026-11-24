//! One PTY-backed terminal: the child process, its cmdterm-vt model, and the
//! reader thread that feeds one into the other.
//!
//! The model never writes to the PTY. Replies, encoded keys, pastes and
//! mouse reports all go through [`PtyWriter`], and always after the model's
//! lock is released, so a slow child that stopped reading cannot stall the
//! UI thread inside the lock.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use cmdterm_env::pty::{ChildKiller, CommandBuilder, Pty, PtySize};
use cmdterm_vt::{Event, KeyCode, KeyEventKind, Mods, MouseButton, MouseEvent, SemanticKind, Size};

use crate::term::Terminal;
use crate::term::attention::{
    Attention, AttentionCell, STATUS_PROBE_REPLY, StatusEvent, status_event,
};
use crate::term::config::TermColors;

/// Whether an OSC 52 target names the system clipboard: `c`, or the default
/// target an empty one stands for. The primary selection is not reachable
/// through `SystemClipboard`.
fn is_clipboard_target(selection: &str) -> bool {
    selection.contains('c') || selection == "s0"
}

/// Puts the text a program sends with OSC 52 on this machine's clipboard.
/// Programs that cannot reach the clipboard themselves, in a sandbox or on
/// another machine, copy this way; the clipboard shims in `clipboard.rs`
/// turn their `xclip`, `xsel` and `wl-copy` calls into it. Reading the
/// clipboard through OSC 52 stays unanswered.
fn copy_osc52(selection: &str, data: &[u8]) {
    if is_clipboard_target(selection) {
        crate::clipboard::copy_text(String::from_utf8_lossy(data).into_owned());
    }
}

/// Acts on one event the model queued, after the model's lock was released.
/// Clipboard sets go to the system clipboard, OSC 7501 probes get their
/// reply, and everything else updates the attention state.
fn handle_event(attention: &AttentionCell, writer: &PtyWriter, event: Event) {
    match event {
        Event::Clipboard { selection, data } => copy_osc52(&selection, &data),
        Event::Osc { code, payload, end } => match status_event(&code, &payload, end) {
            Some(StatusEvent::Probe) => {
                let _ = writer.send(STATUS_PROBE_REPLY);
            }
            Some(StatusEvent::Report(body)) => attention.apply_status_report(body),
            None => {}
        },
        event => attention.apply(event),
    }
}

/// Grid and pixel size of a terminal viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl TermSize {
    fn pty(self) -> PtySize {
        PtySize {
            rows: self.rows.max(1),
            cols: self.cols.max(1),
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
        }
    }

    fn terminal(self) -> Size {
        Size {
            rows: self.rows.max(1) as usize,
            cols: self.cols.max(1) as usize,
        }
    }

    /// The size of one cell in pixels, for mouse reports in pixels.
    fn cell_pixels(self) -> (u32, u32) {
        (
            u32::from(self.pixel_width) / u32::from(self.cols.max(1)),
            u32::from(self.pixel_height) / u32::from(self.rows.max(1)),
        )
    }
}

/// Wakes the UI thread after the model changed. Called from worker threads.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

const EXIT_NONE: u64 = u64::MAX;
const EXIT_SIGNAL: u64 = u64::MAX - 1;

struct Shared {
    term: Mutex<Terminal>,
    writer: PtyWriter,
    /// The bytes of the input being encoded, kept between calls so typing
    /// does not allocate.
    scratch: Mutex<Vec<u8>>,
    /// Bumped after every change the UI must pick up: output, resize, exit.
    revision: AtomicU64,
    /// `EXIT_NONE` while running, `EXIT_SIGNAL` when killed, else the code.
    exit: AtomicU64,
    attention: Arc<AttentionCell>,
    /// The child wrote something, so it runs its own code by now.
    heard: AtomicBool,
}

/// Writes go to the PTY. Shared between the reader thread (replies), input
/// (keys, pastes, mouse reports) and direct `write_bytes` calls, since a PTY
/// master hands out its writer only once.
#[derive(Clone)]
struct PtyWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl PtyWriter {
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self.0.lock().map_err(|_| io::ErrorKind::Other)?;
        writer.write_all(bytes)?;
        writer.flush()
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Terminal> {
        self.term
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Encodes input with `encode` under the model's lock, then writes the
    /// bytes to the PTY after the lock is released. `encode` returns whether
    /// it wrote anything. Allocation-free once the scratch buffer has grown.
    fn send(&self, encode: impl FnOnce(&mut Terminal, &mut Vec<u8>)) -> bool {
        let mut out = self
            .scratch
            .lock()
            .map(|mut scratch| std::mem::take(&mut *scratch))
            .unwrap_or_default();
        out.clear();
        encode(&mut self.lock(), &mut out);
        let wrote = !out.is_empty();
        if wrote {
            let _ = self.writer.send(&out);
        }
        // A huge paste would otherwise keep its buffer for the session.
        if out.capacity() <= 64 * 1024
            && let Ok(mut scratch) = self.scratch.lock()
        {
            *scratch = out;
        }
        wrote
    }

    fn key_down(&self, code: KeyCode, mods: Mods) {
        self.send(|term, out| {
            term.encode_key(code, mods, KeyEventKind::Press, None, out);
        });
    }

    fn paste(&self, text: &str) {
        self.send(|term, out| term.encode_paste(text, out));
    }
}

/// A running terminal. Dropping it kills the child.
pub struct TerminalSession {
    id: u64,
    shared: Arc<Shared>,
    writer: PtyWriter,
    master: Pty,
    killer: Mutex<ChildKiller>,
    size: Mutex<TermSize>,
    /// The process the PTY started, which leads its session.
    child_pid: Option<u32>,
}

impl TerminalSession {
    /// Spawns `command` on a new PTY. `wake` runs on the reader thread after
    /// each chunk of output is applied; it should only schedule a redraw.
    pub fn spawn(
        id: u64,
        command: CommandBuilder,
        size: TermSize,
        colors: TermColors,
        wake: Wake,
    ) -> io::Result<Self> {
        let pty = Pty::open(size.pty())?;
        let mut child = pty.spawn(&command)?;
        let killer = child.killer();
        let child_pid = Some(child.pid());
        let mut reader = pty.reader()?;
        let writer = PtyWriter(Arc::new(Mutex::new(Box::new(pty.writer()?))));

        let attention = Arc::new(AttentionCell::new());
        let mut term = Terminal::new(size.terminal(), colors.config());
        let (cell_width, cell_height) = size.cell_pixels();
        term.set_cell_pixels(cell_width, cell_height);
        let shared = Arc::new(Shared {
            term: Mutex::new(term),
            writer: writer.clone(),
            scratch: Mutex::new(Vec::new()),
            revision: AtomicU64::new(1),
            exit: AtomicU64::new(EXIT_NONE),
            attention,
            heard: AtomicBool::new(false),
        });

        let reader_shared = shared.clone();
        let reader_wake = wake.clone();
        let reply = writer.clone();
        // Disconnects once the reader reached the end of the output.
        let (read_all, all_read) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new()
            .name(format!("cmdterm-pty-{id}"))
            .spawn(move || {
                let _read_all = read_all;
                // These buffers live as long as the session.
                let mut buf = vec![0u8; 64 * 1024];
                let mut replies = Vec::new();
                let mut events = Vec::new();
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut term) = reader_shared.term.lock() {
                                term.advance(&buf[..n]);
                                term.take_replies(&mut replies);
                                events.extend(term.drain_events());
                            }
                            // The lock is free from here: the child may be
                            // slow to read, and nothing here needs the model.
                            if !replies.is_empty() {
                                let _ = reply.send(&replies);
                                replies.clear();
                            }
                            for event in events.drain(..) {
                                handle_event(&reader_shared.attention, &reply, event);
                            }
                            reader_shared.heard.store(true, Ordering::Release);
                            reader_shared.revision.fetch_add(1, Ordering::Release);
                            reader_wake();
                        }
                    }
                }
            })?;

        let waiter_shared = shared.clone();
        std::thread::Builder::new()
            .name(format!("cmdterm-wait-{id}"))
            .spawn(move || {
                let code = match child.wait() {
                    Ok(status) if status.signal().is_some() => EXIT_SIGNAL,
                    Ok(status) => u64::from(status.exit_code()),
                    Err(_) => EXIT_SIGNAL,
                };
                // What the program printed last, often why it failed, may
                // still wait in the PTY. Give the reader a moment to take it
                // into the model before the exit is told, unless something
                // the program left behind keeps the terminal open. ConPTY's
                // output stays open until the pseudoconsole closes, so
                // Windows does not wait.
                #[cfg(unix)]
                let _ = all_read.recv_timeout(std::time::Duration::from_millis(500));
                #[cfg(not(unix))]
                drop(all_read);
                waiter_shared.exit.store(code, Ordering::Release);
                waiter_shared.revision.fetch_add(1, Ordering::Release);
                wake();
            })?;

        Ok(Self {
            id,
            shared,
            writer,
            master: pty,
            killer: Mutex::new(killer),
            size: Mutex::new(size),
            child_pid,
        })
    }

    /// The process group in the terminal's foreground, such as a program a
    /// shell runs. `None` where the platform does not tell, and for the
    /// process the PTY started itself: on SSH or in a container that is
    /// the client, whose group holds everything behind it.
    pub fn foreground_group(&self) -> Option<i32> {
        let group = self.master.foreground_group()?;
        let leader = self.child_pid.and_then(|pid| i32::try_from(pid).ok());
        (group > 0 && Some(group) != leader).then_some(group)
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// The process the PTY started, which leads the terminal's session.
    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// Changes whenever the model or the process state changed.
    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    /// `None` while running; `Some(None)` when killed by a signal.
    pub fn exit_code(&self) -> Option<Option<i32>> {
        match self.shared.exit.load(Ordering::Acquire) {
            EXIT_NONE => None,
            EXIT_SIGNAL => Some(None),
            // Windows codes above `i32::MAX` (NTSTATUS values) read as std
            // reports them, negative.
            code => Some(Some(code as u32 as i32)),
        }
    }

    /// What the program wants from the user. `None` once it exited.
    pub fn attention(&self) -> Attention {
        if self.exit_code().is_some() {
            return Attention::None;
        }
        self.shared.attention.get()
    }

    /// Changes whenever [`Self::attention`] or its message changed.
    pub fn attention_revision(&self) -> u64 {
        self.shared.attention.revision()
    }

    /// The text of the last notification or agent hook, often empty.
    pub fn attention_message(&self) -> String {
        self.shared.attention.message()
    }

    /// The window title the program set last while an agent session ran
    /// in it, without a leading spinner. Empty until one did.
    pub fn agent_title(&self) -> String {
        self.shared.attention.agent_title()
    }

    /// Changes whenever [`Self::agent_title`] changed. A change also moves
    /// [`Self::attention_revision`].
    pub fn agent_title_revision(&self) -> u64 {
        self.shared.attention.agent_title_revision()
    }

    /// The state the agent reported last through its hooks or OSC 7501,
    /// which bells and notifications leave alone. `None` once the user
    /// typed after a `Done`.
    pub fn agent_state(&self) -> Attention {
        if self.exit_code().is_some() {
            return Attention::None;
        }
        self.shared.attention.agent_state()
    }

    /// The agent session the hooks named last in [`super::SESSION_VAR`]:
    /// `agent` or `agent:session-id`. Empty when none runs.
    pub fn agent_session(&self) -> String {
        self.shared.attention.agent_session()
    }

    /// Changes whenever [`Self::agent_session`] changed. A change also moves
    /// [`Self::attention_revision`].
    pub fn agent_session_revision(&self) -> u64 {
        self.shared.attention.agent_session_revision()
    }

    /// A handle that types into the session from another thread, such as
    /// one that first stages a pasted image where the program runs.
    pub fn input(&self) -> SessionInput {
        SessionInput(self.shared.clone())
    }

    /// A handle that reads the attention state and whether the program
    /// exited from another thread, such as the control socket's.
    pub fn status(&self) -> SessionStatus {
        SessionStatus(self.shared.clone())
    }

    /// Records that the user typed into the session, which answers a program
    /// that waited for input. Allocation-free; call it per key.
    pub fn note_input(&self) {
        self.shared.attention.note_input();
    }

    pub fn size(&self) -> TermSize {
        self.size.lock().map(|size| *size).unwrap_or(TermSize {
            cols: 80,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
        })
    }

    /// Resizes the PTY and the model. A no-op when the size is unchanged.
    pub fn resize(&self, size: TermSize) {
        let Ok(mut current) = self.size.lock() else {
            return;
        };
        if *current == size {
            return;
        }
        *current = size;
        let _ = self.master.resize(size.pty());
        {
            let mut term = self.lock();
            term.resize(size.terminal());
            let (width, height) = size.cell_pixels();
            term.set_cell_pixels(width, height);
        }
        self.shared.revision.fetch_add(1, Ordering::Release);
    }

    /// Switches the default colors and the 16 ANSI colors. The rest of the
    /// palette, which a program may have set through OSC escapes, stays. A
    /// program's own change to one of the 16 is replaced. Cold path: the UI
    /// calls this when the color scheme changes.
    pub fn set_colors(&self, colors: TermColors) {
        let config = colors.config();
        {
            let mut term = self.lock();
            let palette = term.palette_mut();
            palette.fg = config.fg;
            palette.bg = config.bg;
            palette.cursor = config.cursor;
            for (index, color) in (0..).zip(config.ansi) {
                palette.set(index, color);
            }
        }
        self.shared.revision.fetch_add(1, Ordering::Release);
    }

    /// Locks the model. Hold it briefly: the reader thread waits on it.
    pub fn lock(&self) -> MutexGuard<'_, Terminal> {
        self.shared.lock()
    }

    /// Writes raw bytes to the child's input.
    pub fn write_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        self.writer.send(bytes)
    }

    /// Pastes text, bracketed when the application asked for it.
    pub fn paste(&self, text: &str) {
        self.note_input();
        self.shared.paste(text);
    }

    /// Sends one key, encoded the way the program asked for.
    pub fn key_down(&self, code: KeyCode, mods: Mods) {
        self.shared.key_down(code, mods);
    }

    /// Tells the program the terminal gained or lost focus, if it asked.
    pub fn focus_changed(&self, focused: bool) {
        self.shared.send(|term, out| {
            term.encode_focus(focused, out);
        });
    }

    /// Reports a pointer event `count` times, such as a wheel with several
    /// notches. Nothing is sent unless the program grabbed the mouse.
    pub fn send_mouse(&self, event: MouseEvent, count: usize) {
        self.shared.send(|term, out| {
            for _ in 0..count.max(1) {
                term.encode_mouse(&event, out);
            }
        });
    }

    /// Turns `count` wheel notches into cursor keys when the program runs on
    /// the alternate screen and asked for alternate scroll (mode 1007).
    /// Returns whether it did, so the caller scrolls the history otherwise.
    pub fn send_alt_scroll(&self, button: MouseButton, count: usize) -> bool {
        self.shared.send(|term, out| {
            for _ in 0..count.max(1) {
                term.encode_alt_scroll(button, out);
            }
        })
    }

    pub fn kill(&self) {
        if let Ok(killer) = self.killer.lock() {
            let _ = killer.kill();
        }
    }

    /// Tells the `scriptschnell-term-remote exec` client the PTY runs, in a remote
    /// session, whether its terminal shows: `SIGUSR2` when it does,
    /// `SIGUSR1` when not. False when nothing was sent: before the client
    /// wrote anything, since until it handles the signals they end it,
    /// after it exited, and where there are no signals.
    pub fn tell_shown(&self, shown: bool) -> bool {
        if !self.shared.heard.load(Ordering::Acquire) || self.exit_code().is_some() {
            return false;
        }
        #[cfg(unix)]
        {
            use rustix::process::{Pid, Signal, kill_process};
            let Some(pid) = self
                .child_pid
                .and_then(|pid| i32::try_from(pid).ok())
                .and_then(Pid::from_raw)
            else {
                return false;
            };
            let signal = if shown { Signal::USR2 } else { Signal::USR1 };
            kill_process(pid, signal).is_ok()
        }
        #[cfg(not(unix))]
        {
            let _ = shown;
            false
        }
    }
}

/// Reads a session's attention state from any thread. It keeps the session's
/// model alive, so drop it when the session closes.
#[derive(Clone)]
pub struct SessionStatus(Arc<Shared>);

impl SessionStatus {
    /// As [`TerminalSession::attention`].
    pub fn attention(&self) -> Attention {
        if self.exited() {
            return Attention::None;
        }
        self.0.attention.get()
    }

    /// As [`TerminalSession::attention_message`].
    pub fn attention_message(&self) -> String {
        self.0.attention.message()
    }

    /// Whether the program ended.
    pub fn exited(&self) -> bool {
        self.0.exit.load(Ordering::Acquire) != EXIT_NONE
    }
}

/// Types into a session from any thread. It keeps the session's model
/// alive, so drop it soon.
#[derive(Clone)]
pub struct SessionInput(Arc<Shared>);

impl SessionInput {
    /// As [`TerminalSession::paste`].
    pub fn paste(&self, text: &str) {
        self.0.attention.note_input();
        self.0.paste(text);
    }

    /// Sets user variable `name` to `value`, as if the program had written
    /// OSC 1337 SetUserVar. For agent hooks that cannot reach the terminal,
    /// which they tell through the control socket instead. The caller
    /// wakes the UI.
    pub fn set_user_var(&self, name: &str, value: &str) {
        self.0.attention.apply(Event::UserVar {
            name: name.to_owned(),
            value: value.to_owned(),
        });
        self.0.revision.fetch_add(1, Ordering::Release);
    }

    /// As [`TerminalSession::revision`].
    pub fn revision(&self) -> u64 {
        self.0.revision.load(Ordering::Acquire)
    }

    /// Whether the program ended.
    pub fn exited(&self) -> bool {
        self.0.exit.load(Ordering::Acquire) != EXIT_NONE
    }

    /// Whether the shell marked a prompt (OSC 133) as the last thing on
    /// screen, so it reads a command now. `None` when it never marked one.
    /// Cold: collects the screen's zones.
    pub fn at_prompt(&self) -> Option<bool> {
        let zones = self.0.lock().semantic_zones();
        let prompt = |zone: &cmdterm_vt::SemanticZone| {
            matches!(zone.kind, SemanticKind::Prompt | SemanticKind::Input)
        };
        if !zones.iter().any(|zone| zone.kind == SemanticKind::Prompt) {
            return None;
        }
        Some(zones.last().is_some_and(prompt))
    }

    /// As [`TerminalSession::agent_session`].
    pub fn agent_session(&self) -> String {
        self.0.attention.agent_session()
    }

    /// As [`TerminalSession::agent_session_revision`].
    pub fn agent_session_revision(&self) -> u64 {
        self.0.attention.agent_session_revision()
    }

    /// Sends one key, encoded the way the program asked for.
    pub fn key_down(&self, code: KeyCode, mods: Mods) {
        self.0.attention.note_input();
        self.0.key_down(code, mods);
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.exit_code().is_none() {
            self.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The viewport's text, one line per row.
    fn screen_text(session: &TerminalSession) -> String {
        let term = session.lock();
        (0..term.size().rows)
            .filter_map(|y| term.row(y))
            .map(|row| term.row_text(row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn output_reaches_the_model_and_exit_is_reported() {
        let command = cmdterm_env::Environment::Local
            .command("sh")
            .args(["-c", "printf 'hello cmdterm'; exit 3"])
            .to_pty();
        let size = TermSize {
            cols: 40,
            rows: 5,
            pixel_width: 0,
            pixel_height: 0,
        };
        let session =
            TerminalSession::spawn(1, command, size, TermColors::default(), Arc::new(|| {}))
                .unwrap();
        wait_for(|| session.exit_code().is_some());
        assert_eq!(session.exit_code(), Some(Some(3)));
        wait_for(|| screen_text(&session).contains("hello cmdterm"));
    }

    /// Programs such as Claude Code ask the terminal for its name and its
    /// modes instead of trusting `TERM`.
    #[test]
    fn the_terminal_answers_capability_queries() {
        // Raw mode keeps the replies off the screen until `dd` reads them
        // in one go; `tr` makes ESC printable.
        let script = "stty raw -echo; printf '\\033[>0q\\033[?2026$p\\033[?u'; sleep 0.5; \
                      dd bs=4096 count=1 2>/dev/null | tr '\\033' E; sleep 5";
        let command = cmdterm_env::Environment::Local
            .command("sh")
            .args(["-c", script])
            .to_pty();
        let size = TermSize {
            cols: 200,
            rows: 5,
            pixel_width: 0,
            pixel_height: 0,
        };
        let session =
            TerminalSession::spawn(1, command, size, TermColors::default(), Arc::new(|| {}))
                .unwrap();
        // Windows' own ConPTY answers DECRQM itself, and it does not know
        // mode 2026.
        let sync = if cfg!(windows) { 0 } else { 2 };
        let expected = format!(
            "EP>|{} {}E\\E[?2026;{sync}$yE[?0u",
            crate::term::identity::PROGRAM,
            crate::term::identity::VERSION
        );
        wait_for(|| screen_text(&session).contains(&expected));
    }

    fn spawn_script(script: &str, cols: u16) -> TerminalSession {
        let command = cmdterm_env::Environment::Local
            .command("sh")
            .args(["-c", script])
            .to_pty();
        let size = TermSize {
            cols,
            rows: 5,
            pixel_width: 0,
            pixel_height: 0,
        };
        TerminalSession::spawn(1, command, size, TermColors::default(), Arc::new(|| {})).unwrap()
    }

    #[test]
    fn program_status_reports_set_the_attention_state() {
        // "Allow?" in base64.
        let session = spawn_script(
            "printf '\\033]7501;state=working:app=x\\033\\\\'; sleep 1; \
             printf '\\033]7501;state=blocked:kind=permission:app=x:msg=QWxsb3c/\\007'; sleep 5",
            40,
        );
        wait_for(|| session.attention() == Attention::Busy);
        wait_for(|| session.attention() == Attention::Permission);
        assert_eq!(session.attention_message(), "Allow?");
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "Windows' own ConPTY drops the OSC 7501 query or its reply"
    )]
    fn program_status_probes_are_answered() {
        let session = spawn_script(
            "stty raw -echo; printf '\\033]7501;?\\033\\\\'; sleep 0.5; \
             dd bs=4096 count=1 2>/dev/null | tr '\\033' E; sleep 5",
            40,
        );
        wait_for(|| screen_text(&session).contains("E]7501;?E\\"));
    }

    #[test]
    fn typed_keys_reach_the_child() {
        let command = cmdterm_env::Environment::Local
            .command("sh")
            .args(["-c", "read line; printf 'got:%s' \"$line\""])
            .to_pty();
        let size = TermSize {
            cols: 40,
            rows: 5,
            pixel_width: 0,
            pixel_height: 0,
        };
        let session =
            TerminalSession::spawn(2, command, size, TermColors::default(), Arc::new(|| {}))
                .unwrap();
        session.write_bytes(b"abc\r").unwrap();
        wait_for(|| session.exit_code().is_some());
        wait_for(|| screen_text(&session).contains("got:abc"));
    }

    /// Keys, pastes, focus changes and mouse reports are encoded by the
    /// model for the modes the program set, and reach the child.
    #[test]
    fn input_is_encoded_for_the_modes_the_program_set() {
        let script = "stty raw -echo; \
                      printf '\\033[?2004h\\033[?1004h\\033[?1000h\\033[?1006h'; \
                      head -c 36 | tr '\\033' E; sleep 5";
        let session = spawn_script(script, 80);
        wait_for(|| {
            let modes = session.lock().modes();
            modes.bracketed_paste
                && modes.focus_events
                && modes.mouse != cmdterm_vt::MouseTracking::Off
        });
        session.key_down(KeyCode::Char('a'), Mods::NONE);
        session.paste("hi");
        session.focus_changed(true);
        session.send_mouse(
            MouseEvent::new(cmdterm_vt::MouseEventKind::Press, MouseButton::Left, 2, 1),
            1,
        );
        session.send_mouse(
            MouseEvent::new(cmdterm_vt::MouseEventKind::Release, MouseButton::Left, 2, 1),
            1,
        );
        wait_for(|| screen_text(&session).contains("aE[200~hiE[201~E[IE[<0;3;2ME[<0;3;2m"));
    }

    #[test]
    fn alternate_scroll_only_applies_on_the_alternate_screen() {
        let session = spawn_script("sleep 5", 80);
        assert!(!session.send_alt_scroll(MouseButton::WheelUp, 1));
        session.lock().advance(b"\x1b[?1049h\x1b[?1007h");
        assert!(session.send_alt_scroll(MouseButton::WheelUp, 3));
    }

    /// A child that stops reading blocks the writer, not the model.
    #[test]
    fn a_blocked_write_does_not_hold_the_model_lock() {
        let session = Arc::new(spawn_script("stty raw -echo; sleep 5", 80));
        wait_for(|| session.lock().size().cols == 80);
        std::thread::sleep(Duration::from_millis(200));
        let input = session.input();
        // Far more than a PTY buffers, so the write blocks.
        std::thread::spawn(move || input.paste(&"x".repeat(4 << 20)));
        std::thread::sleep(Duration::from_millis(300));
        let (done, locked) = std::sync::mpsc::channel();
        let probe = session.clone();
        std::thread::spawn(move || {
            drop(probe.lock());
            let _ = done.send(());
        });
        assert!(locked.recv_timeout(Duration::from_secs(3)).is_ok());
        session.kill();
    }

    #[test]
    fn osc_52_targets_the_system_clipboard_only() {
        assert!(is_clipboard_target("c"));
        assert!(is_clipboard_target("s0"));
        assert!(is_clipboard_target("cp"));
        assert!(!is_clipboard_target("p"));
        assert!(!is_clipboard_target("s"));
    }

    #[test]
    fn a_status_probe_is_answered_with_the_fixed_reply() {
        let attention = AttentionCell::new();
        let end = cmdterm_vt::OscEnd {
            truncated: false,
            bell: false,
        };
        assert_eq!(status_event("7501", b"?", end), Some(StatusEvent::Probe));
        assert_eq!(STATUS_PROBE_REPLY, b"\x1b]7501;?\x1b\\");
        handle_event_report(&attention);
    }

    fn handle_event_report(attention: &AttentionCell) {
        let end = cmdterm_vt::OscEnd {
            truncated: false,
            bell: true,
        };
        let Some(StatusEvent::Report(body)) = status_event("7501", b"state=working", end) else {
            panic!("not a report");
        };
        attention.apply_status_report(body);
        assert_eq!(attention.get(), Attention::Busy);
    }
}
