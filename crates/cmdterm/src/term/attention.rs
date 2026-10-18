//! What a program in a terminal wants from the user, read from the escape
//! sequences it writes: the bell, desktop notifications (OSC 9 and OSC 777),
//! progress reports (OSC 9;4), the `cmdterm_agent` user variable
//! (OSC 1337 SetUserVar) that the agent hooks from the settings write, and
//! program status reports (OSC 7501) that Goose, Mistral Vibe and the
//! Antigravity CLI write on their own.
//!
//! cmdterm-vt does not handle OSC 7501, so it comes out of the model as an
//! `Event::Osc` with the code `7501`, and [`status_event`] reads it. A report
//! looks like `ESC ] 7501 ; state=blocked:kind=permission:app=goose:msg=BASE64
//! ST`. A program first probes with `ESC ] 7501 ; ? ST` and reports only after
//! the terminal answers with [`STATUS_PROBE_REPLY`].
//!
//! The reader thread updates the state from the model's events, after it
//! released the model's lock. The UI reads
//! it through atomics and compares [`AttentionCell::revision`] to notice a
//! change without locking the model.
//!
//! The cell also keeps the window title (OSC 0 and 2) the program set while
//! an agent session ran, from the first state an agent reports to its
//! `clear`. Agents such as Claude Code name the task there, behind a
//! spinner glyph that changes many times a second; [`clean_title`] drops
//! it, so only a new task counts as a change.
//!
//! The hooks also name the agent and its session in the `cmdterm_session`
//! user variable when a session starts, and empty it when it ends. The app
//! saves it with the panel and resumes that session on the next start.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

use cmdterm_vt::{Event, OscEnd, Progress};

/// The user variable agent hooks set. Its value is `state` or
/// `state:message`, where state is one of the [`Attention::parse`] names.
pub const AGENT_VAR: &str = "cmdterm_agent";
/// The user variable agent hooks set when a session starts, to `agent` or
/// `agent:session-id`, and empty when it ends.
pub const SESSION_VAR: &str = "cmdterm_session";

/// A bell this soon after the user's last input is the shell complaining
/// about a key, not a program asking for attention.
const BELL_AFTER_INPUT_MS: u64 = 1000;

/// What a terminal's program wants. Ordered by urgency, so the most urgent
/// session of a worktree is the maximum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Attention {
    #[default]
    None = 0,
    /// An agent is working.
    Busy = 1,
    /// An agent finished its turn and waits for the next prompt.
    Done = 2,
    /// The program asked a question or rang the bell.
    Input = 3,
    /// An agent waits for the user to approve a tool call.
    Permission = 4,
}

impl Attention {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Busy,
            2 => Self::Done,
            3 => Self::Input,
            4 => Self::Permission,
            _ => Self::None,
        }
    }

    /// The state named by an agent hook. `None` when the name is unknown.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.trim() {
            "" | "none" | "clear" => Self::None,
            "busy" | "running" => Self::Busy,
            "done" | "idle" | "stop" => Self::Done,
            "input" | "waiting" | "question" => Self::Input,
            "permission" | "approval" => Self::Permission,
            _ => return None,
        })
    }

    /// Whether the program waits for the user.
    pub fn wants_user(self) -> bool {
        self >= Self::Done
    }
}

/// The attention state of one session, shared between the reader thread
/// and the UI.
pub(crate) struct AttentionCell {
    state: AtomicU8,
    /// Bumped on every change of the state or the message.
    revision: AtomicU64,
    /// Text from the last notification or hook, empty when there is none.
    message: Mutex<String>,
    /// Milliseconds after `epoch` of the user's last input.
    last_input: AtomicU64,
    epoch: Instant,
    /// Whether an agent session runs: an agent reported a state through
    /// its hooks or OSC 7501, and has not cleared it since.
    agent: AtomicBool,
    /// The state the agent reported last, through its hooks or OSC 7501,
    /// unlike `state` untouched by bells and notifications. A keystroke
    /// turns `Done` into `None`: the user started on the next prompt.
    agent_state: AtomicU8,
    /// The title set last while an agent session ran. Empty until one did.
    agent_title: Mutex<String>,
    /// Bumped on every change of `agent_title`.
    agent_title_revision: AtomicU64,
    /// The last value of [`SESSION_VAR`]. Empty when no agent session runs.
    agent_session: Mutex<String>,
    /// Bumped on every change of `agent_session`.
    agent_session_revision: AtomicU64,
    /// The last session [`SESSION_VAR`] named that was not empty. The title
    /// belongs to it, so a different one clears the title.
    titled_session: Mutex<String>,
}

impl AttentionCell {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicU8::new(Attention::None as u8),
            revision: AtomicU64::new(0),
            message: Mutex::new(String::new()),
            last_input: AtomicU64::new(0),
            epoch: Instant::now(),
            agent: AtomicBool::new(false),
            agent_state: AtomicU8::new(Attention::None as u8),
            agent_title: Mutex::new(String::new()),
            agent_title_revision: AtomicU64::new(0),
            agent_session: Mutex::new(String::new()),
            agent_session_revision: AtomicU64::new(0),
            titled_session: Mutex::new(String::new()),
        }
    }

    pub(crate) fn agent_session(&self) -> String {
        self.agent_session
            .lock()
            .map(|session| session.clone())
            .unwrap_or_default()
    }

    pub(crate) fn agent_session_revision(&self) -> u64 {
        self.agent_session_revision.load(Ordering::Acquire)
    }

    /// The hooks set [`SESSION_VAR`] to `value`. Every report counts, also
    /// one that repeats the value: a resumed agent names the same session
    /// again. A session other than the one before it in this terminal
    /// clears the title, which was that one's.
    fn note_session(&self, value: &str) {
        let Ok(mut current) = self.agent_session.lock() else {
            return;
        };
        current.clear();
        current.push_str(value);
        drop(current);
        if !value.is_empty()
            && let Ok(mut titled) = self.titled_session.lock()
            && titled.as_str() != value
        {
            let switched = !titled.is_empty();
            titled.clear();
            titled.push_str(value);
            drop(titled);
            if switched && let Ok(mut title) = self.agent_title.lock() {
                title.clear();
                drop(title);
                self.agent_title_revision.fetch_add(1, Ordering::Release);
            }
        }
        self.agent_session_revision.fetch_add(1, Ordering::Release);
        // The UI looks at sessions when the attention revision moves.
        self.revision.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn agent_title(&self) -> String {
        self.agent_title
            .lock()
            .map(|title| title.clone())
            .unwrap_or_default()
    }

    pub(crate) fn agent_title_revision(&self) -> u64 {
        self.agent_title_revision.load(Ordering::Acquire)
    }

    /// An agent reported `state`: a session starts with any state but
    /// `None` and ends with `None`.
    fn note_agent(&self, state: Attention) {
        self.agent
            .store(state != Attention::None, Ordering::Release);
        self.agent_state.store(state as u8, Ordering::Release);
    }

    pub(crate) fn agent_state(&self) -> Attention {
        Attention::from_u8(self.agent_state.load(Ordering::Acquire))
    }

    /// The program set its window title. Allocation-free while only the
    /// spinner in front of it changes.
    fn note_title(&self, title: &str) {
        if !self.agent.load(Ordering::Acquire) {
            return;
        }
        let title = clean_title(title);
        if title.is_empty() {
            return;
        }
        let Ok(mut current) = self.agent_title.lock() else {
            return;
        };
        if current.as_str() == title {
            return;
        }
        current.clear();
        current.push_str(title);
        drop(current);
        self.agent_title_revision.fetch_add(1, Ordering::Release);
        // The UI looks at titles when the attention revision moves.
        self.revision.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn get(&self) -> Attention {
        Attention::from_u8(self.state.load(Ordering::Acquire))
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub(crate) fn message(&self) -> String {
        self.message.lock().map(|m| m.clone()).unwrap_or_default()
    }

    pub(crate) fn set(&self, state: Attention, message: &str) {
        // The message first, so a reader that sees the new state also sees
        // its message.
        let mut changed = false;
        if let Ok(mut current) = self.message.lock()
            && current.as_str() != message
        {
            current.clear();
            current.push_str(message);
            changed = true;
        }
        changed |= self.state.swap(state as u8, Ordering::AcqRel) != state as u8;
        if changed {
            self.revision.fetch_add(1, Ordering::Release);
        }
    }

    /// The user typed or pasted into the session: whatever the program waited
    /// for has been answered. Allocation-free; runs per keystroke.
    pub(crate) fn note_input(&self) {
        // Plus one, so input in the first millisecond still counts as input.
        let now = self.epoch.elapsed().as_millis() as u64 + 1;
        self.last_input.store(now, Ordering::Release);
        let _ = self.agent_state.compare_exchange(
            Attention::Done as u8,
            Attention::None as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if self.get().wants_user() {
            self.state.store(Attention::None as u8, Ordering::Release);
            self.revision.fetch_add(1, Ordering::Release);
        }
    }

    fn input_was_recent(&self) -> bool {
        let last = self.last_input.load(Ordering::Acquire);
        last != 0
            && (self.epoch.elapsed().as_millis() as u64 + 1).saturating_sub(last)
                < BELL_AFTER_INPUT_MS
    }

    /// Applies one event the model queued. Events the cell does not track
    /// are ignored. Never blocks on the model.
    pub(crate) fn apply(&self, event: Event) {
        match event {
            Event::Bell => {
                if !self.input_was_recent() && self.get() < Attention::Input {
                    self.set(Attention::Input, "");
                }
            }
            Event::Toast { title, body } => {
                let text = match title {
                    Some(title) if !title.is_empty() && !body.is_empty() => {
                        format!("{title}: {body}")
                    }
                    Some(title) if !title.is_empty() => title,
                    _ => body,
                };
                self.set(classify_notification(&text), &text);
            }
            Event::Progress(progress) => match progress {
                // A paused bar is no progress, as before.
                Progress::None | Progress::Paused(_) => {
                    if self.get() == Attention::Busy {
                        self.set(Attention::None, "");
                    }
                }
                Progress::Percentage(_) | Progress::Error(_) | Progress::Indeterminate => {
                    if self.get() == Attention::None {
                        self.set(Attention::Busy, "");
                    }
                }
            },
            Event::UserVar { name, value } if name == AGENT_VAR => {
                let (state, message) = value.split_once(':').unwrap_or((&value, ""));
                if let Some(state) = Attention::parse(state) {
                    self.note_agent(state);
                    self.set(state, message.trim());
                }
            }
            Event::UserVar { name, value } if name == SESSION_VAR => {
                self.note_session(value.trim());
            }
            Event::Title(title) => self.note_title(&title),
            _ => {}
        }
    }
}

/// Guesses what a desktop notification asks for from its text. Agents word
/// these for people: "Claude needs your permission to use Bash", "Approval
/// requested", "Agent turn complete".
fn classify_notification(text: &str) -> Attention {
    let text = text.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| text.contains(word));
    if has(&["permission", "approv", "allow"]) {
        Attention::Permission
    } else if has(&["input", "waiting", "question", "answer"]) {
        Attention::Input
    } else if has(&["complete", "finished", "done"]) {
        Attention::Done
    } else {
        Attention::Input
    }
}

/// What the terminal writes back to the PTY when a program probes for
/// OSC 7501 support with `ESC ] 7501 ; ? ST`.
pub(crate) const STATUS_PROBE_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

/// The OSC body that marks a probe.
const STATUS_PROBE: &[u8] = b"?";

/// The OSC code of program status reports.
const STATUS_CODE: &str = "7501";

/// The longest report body kept. A message holds at most 2048 bytes of text,
/// about 2732 in base64, so 4 KiB leaves room for the other fields. Longer
/// bodies are dropped.
const STATUS_PAYLOAD_MAX: usize = 4096;

/// One OSC 7501 sequence found among the model's events.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StatusEvent<'a> {
    /// `ESC ] 7501 ; ? ST`: answer with [`STATUS_PROBE_REPLY`].
    Probe,
    /// A report body without the `7501;` prefix, for
    /// [`AttentionCell::apply_status_report`].
    Report(&'a [u8]),
}

/// Reads an unhandled OSC the model passed through. `None` for other codes
/// and for bodies that are too long or were cut by the parser's limit.
pub(crate) fn status_event<'a>(
    code: &str,
    payload: &'a [u8],
    end: OscEnd,
) -> Option<StatusEvent<'a>> {
    if code != STATUS_CODE || end.truncated || payload.len() > STATUS_PAYLOAD_MAX {
        return None;
    }
    Some(if payload == STATUS_PROBE {
        StatusEvent::Probe
    } else {
        StatusEvent::Report(payload)
    })
}

impl AttentionCell {
    /// Applies the body of an OSC 7501 report, without the `7501;` prefix.
    /// Only allocates when the report carries a message. Reports with an
    /// unknown or missing state are ignored, and so are unknown fields.
    pub(crate) fn apply_status_report(&self, body: &[u8]) {
        let mut state = None;
        let mut kind: &[u8] = b"";
        let mut msg: &[u8] = b"";
        for field in body.split(|&b| b == b':') {
            let Some(eq) = field.iter().position(|&b| b == b'=') else {
                continue;
            };
            let (key, value) = (&field[..eq], &field[eq + 1..]);
            match key {
                b"state" => state = Some(value),
                b"kind" => kind = value,
                b"msg" => msg = value,
                _ => {}
            }
        }
        let attention = match state {
            Some(b"working") => Attention::Busy,
            Some(b"done" | b"error") => Attention::Done,
            // Goose sends idle right after done; keep the finished turn
            // visible until the user answers it.
            Some(b"idle") if self.get() == Attention::Done => return,
            Some(b"idle" | b"clear") => Attention::None,
            Some(b"blocked") if kind == b"permission" => Attention::Permission,
            Some(b"blocked") => Attention::Input,
            _ => return,
        };
        self.note_agent(attention);
        if msg.is_empty() {
            self.set(attention, "");
            return;
        }
        let text = decode_base64(msg)
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
            .unwrap_or_default();
        self.set(attention, &text);
    }
}

/// `title` without the spinner or status glyphs agents put in front of it,
/// such as `✳ ` or a braille dot, and without surrounding space.
pub(crate) fn clean_title(title: &str) -> &str {
    title
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim_end()
}

/// Decodes standard base64. Padding is optional; anything after the first
/// `=` is ignored. `None` on a byte outside the alphabet.
fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 2);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &byte in input {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_values_set_the_state_and_message() {
        let cell = AttentionCell::new();
        cell.apply(Event::UserVar {
            name: AGENT_VAR.into(),
            value: "permission:Bash wants to run".into(),
        });
        assert_eq!(cell.get(), Attention::Permission);
        assert_eq!(cell.message(), "Bash wants to run");
        cell.apply(Event::UserVar {
            name: AGENT_VAR.into(),
            value: "busy".into(),
        });
        assert_eq!(cell.get(), Attention::Busy);
        assert_eq!(cell.message(), "");
        cell.apply(Event::UserVar {
            name: "other".into(),
            value: "done".into(),
        });
        assert_eq!(cell.get(), Attention::Busy);
    }

    #[test]
    fn the_session_variable_names_the_agent_session() {
        let cell = AttentionCell::new();
        let session = |value: &str| Event::UserVar {
            name: SESSION_VAR.into(),
            value: value.into(),
        };
        cell.apply(session("claude:3f2a-11"));
        let revision = cell.agent_session_revision();
        let attention = cell.revision();
        assert_eq!(cell.agent_session(), "claude:3f2a-11");
        cell.apply(session("claude:3f2a-11"));
        assert!(cell.agent_session_revision() > revision);
        let revision = cell.agent_session_revision();
        cell.apply(session(""));
        assert_eq!(cell.agent_session(), "");
        assert!(cell.agent_session_revision() > revision);
        assert!(cell.revision() > attention);
    }

    #[test]
    fn titles_count_only_while_an_agent_session_runs() {
        let cell = AttentionCell::new();
        let hook = |value: &str| Event::UserVar {
            name: AGENT_VAR.into(),
            value: value.into(),
        };
        cell.apply(Event::Title("user@host: ~/src".into()));
        assert_eq!(cell.agent_title(), "");
        cell.apply(hook("busy"));
        cell.apply(Event::Title("⠂ Fix the login form".into()));
        let revision = cell.agent_title_revision();
        let attention = cell.revision();
        // Only the spinner changes.
        cell.apply(Event::Title("⠐ Fix the login form".into()));
        assert_eq!(cell.agent_title(), "Fix the login form");
        assert_eq!(cell.agent_title_revision(), revision);
        assert_eq!(cell.revision(), attention);
        // The shell's title after the session ends is not the agent's.
        cell.apply(hook("clear"));
        cell.apply(Event::Title("user@host: ~/src".into()));
        assert_eq!(cell.agent_title(), "Fix the login form");
        cell.apply_status_report(b"state=working:app=goose");
        cell.apply(Event::Title("Goose: Add a search".into()));
        assert_eq!(cell.agent_title(), "Goose: Add a search");
    }

    #[test]
    fn a_new_session_in_the_terminal_clears_the_last_title() {
        let cell = AttentionCell::new();
        let session = |value: &str| Event::UserVar {
            name: SESSION_VAR.into(),
            value: value.into(),
        };
        // The first session keeps a title set before it named itself.
        cell.apply_status_report(b"state=working:app=claude");
        cell.apply(Event::Title("Fix the login form".into()));
        cell.apply(session("claude:3f2a-11"));
        cell.apply(session("claude:3f2a-11"));
        assert_eq!(cell.agent_title(), "Fix the login form");
        let revision = cell.agent_title_revision();
        // Ending it keeps the title; the next session drops it.
        cell.apply(session(""));
        assert_eq!(cell.agent_title(), "Fix the login form");
        cell.apply(session("claude:9b7c-22"));
        assert_eq!(cell.agent_title(), "");
        assert!(cell.agent_title_revision() > revision);
    }

    #[test]
    fn notifications_are_classified_by_their_text() {
        assert_eq!(
            classify_notification("Claude needs your permission to use Bash"),
            Attention::Permission
        );
        assert_eq!(
            classify_notification("Codex: Approval requested"),
            Attention::Permission
        );
        assert_eq!(
            classify_notification("Claude is waiting for your input"),
            Attention::Input
        );
        assert_eq!(
            classify_notification("Agent turn complete"),
            Attention::Done
        );
        assert_eq!(classify_notification("hello"), Attention::Input);
    }

    #[test]
    fn input_answers_a_waiting_program_but_not_a_busy_one() {
        let cell = AttentionCell::new();
        cell.set(Attention::Permission, "x");
        let before = cell.revision();
        cell.note_input();
        assert_eq!(cell.get(), Attention::None);
        assert!(cell.revision() > before);
        cell.set(Attention::Busy, "");
        cell.note_input();
        assert_eq!(cell.get(), Attention::Busy);
    }

    #[test]
    fn a_bell_right_after_typing_is_ignored() {
        let cell = AttentionCell::new();
        cell.note_input();
        cell.apply(Event::Bell);
        assert_eq!(cell.get(), Attention::None);
        let cell = AttentionCell::new();
        cell.apply(Event::Bell);
        assert_eq!(cell.get(), Attention::Input);
        cell.set(Attention::Permission, "");
        cell.apply(Event::Bell);
        assert_eq!(cell.get(), Attention::Permission);
    }

    #[test]
    fn progress_marks_busy_until_it_ends() {
        let cell = AttentionCell::new();
        cell.apply(Event::Progress(Progress::Indeterminate));
        assert_eq!(cell.get(), Attention::Busy);
        cell.apply(Event::Progress(Progress::None));
        assert_eq!(cell.get(), Attention::None);
        cell.set(Attention::Input, "");
        cell.apply(Event::Progress(Progress::Percentage(40)));
        assert_eq!(cell.get(), Attention::Input);
    }

    /// Feeds `chunks` to a model like the reader does and returns the status
    /// events it passes through, with probes as `?`.
    fn pass_all(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut term = cmdterm_vt::Terminal::new(
            cmdterm_vt::Size { rows: 4, cols: 40 },
            cmdterm_vt::Config::default(),
        );
        let mut found = Vec::new();
        for chunk in chunks {
            term.advance(chunk);
            for event in term.drain_events() {
                let Event::Osc { code, payload, end } = event else {
                    continue;
                };
                found.extend(status_event(&code, &payload, end).map(|event| match event {
                    StatusEvent::Probe => b"?".to_vec(),
                    StatusEvent::Report(body) => body.to_vec(),
                }));
            }
        }
        found
    }

    fn report(body: &str) -> AttentionCell {
        let cell = AttentionCell::new();
        cell.apply_status_report(body.as_bytes());
        cell
    }

    #[test]
    fn status_reports_map_to_attention_states() {
        assert_eq!(report("state=working:app=goose").get(), Attention::Busy);
        assert_eq!(report("state=done:app=goose").get(), Attention::Done);
        assert_eq!(report("state=error:app=goose").get(), Attention::Done);
        assert_eq!(report("state=idle:app=goose").get(), Attention::None);
        assert_eq!(report("state=clear").get(), Attention::None);
        assert_eq!(
            report("state=blocked:kind=permission:app=goose").get(),
            Attention::Permission
        );
        assert_eq!(
            report("state=blocked:kind=question:app=vibe").get(),
            Attention::Input
        );
        assert_eq!(report("state=blocked:app=vibe").get(), Attention::Input);
        // No state or an unknown one changes nothing.
        let cell = report("state=working");
        cell.apply_status_report(b"app=goose");
        cell.apply_status_report(b"state=dancing");
        assert_eq!(cell.get(), Attention::Busy);
    }

    #[test]
    fn idle_keeps_a_finished_turn_but_clear_does_not() {
        let cell = report("state=done:app=goose:msg=VHVybiBjb21wbGV0ZQ==");
        cell.apply_status_report(b"state=idle:app=goose");
        assert_eq!(cell.get(), Attention::Done);
        assert_eq!(cell.message(), "Turn complete");
        cell.apply_status_report(b"state=clear:app=goose");
        assert_eq!(cell.get(), Attention::None);
        assert_eq!(cell.message(), "");
        let cell = report("state=working");
        cell.apply_status_report(b"state=idle");
        assert_eq!(cell.get(), Attention::None);
    }

    #[test]
    fn status_messages_are_base64_and_unknown_fields_are_ignored() {
        // "Run shell: ls -lä"
        let cell = report(
            "x=1:state=blocked:future:kind=permission:app=goose:msg=UnVuIHNoZWxsOiBscyAtbMOk:y=",
        );
        assert_eq!(cell.get(), Attention::Permission);
        assert_eq!(cell.message(), "Run shell: ls -lä");
        let cell = report("state=error:msg=Ym9vbQ");
        assert_eq!(cell.message(), "boom");
        let cell = report("state=error:msg=not*base64");
        assert_eq!(cell.get(), Attention::Done);
        assert_eq!(cell.message(), "");
    }

    #[test]
    fn passthrough_finds_reports_with_either_terminator() {
        assert_eq!(
            pass_all(&[b"text\x1b]7501;state=working:app=x\x1b\\more\x1b]7501;state=done\x07end"]),
            vec![b"state=working:app=x".to_vec(), b"state=done".to_vec()]
        );
        // Other sequences pass by.
        assert_eq!(
            pass_all(&[
                b"\x1b[1m\x1b]0;title\x07\x1b]750;x\x07\x1b]75011;x\x07\x1b\x1b]7501;a\x07"
            ]),
            vec![b"a".to_vec()]
        );
    }

    #[test]
    fn passthrough_joins_reports_split_across_reads() {
        let whole: &[u8] = b"ab\x1b]7501;state=blocked:kind=question\x1b\\cd";
        for at in 1..whole.len() {
            let (head, tail) = whole.split_at(at);
            assert_eq!(
                pass_all(&[head, tail]),
                vec![b"state=blocked:kind=question".to_vec()],
                "split at {at}"
            );
        }
        let bytes: Vec<&[u8]> = whole.chunks(1).collect();
        assert_eq!(
            pass_all(&bytes),
            vec![b"state=blocked:kind=question".to_vec()]
        );
    }

    #[test]
    fn passthrough_reports_probes() {
        assert_eq!(pass_all(&[b"\x1b]7501;?\x1b\\"]), vec![b"?".to_vec()]);
        assert_eq!(pass_all(&[b"\x1b]7501", b";?\x07"]), vec![b"?".to_vec()]);
    }

    #[test]
    fn passthrough_drops_overlong_bodies() {
        let mut long = b"\x1b]7501;state=working:msg=".to_vec();
        long.extend(std::iter::repeat_n(b'A', STATUS_PAYLOAD_MAX));
        long.extend(b"\x1b\\\x1b]7501;state=done\x1b\\");
        assert_eq!(pass_all(&[&long]), vec![b"state=done".to_vec()]);
        // Split into reads, the overflow happens in a later one.
        let parts: Vec<&[u8]> = long.chunks(1000).collect();
        assert_eq!(pass_all(&parts), vec![b"state=done".to_vec()]);
        // A body of exactly the limit still counts.
        let mut full = b"\x1b]7501;".to_vec();
        full.extend(std::iter::repeat_n(b'x', STATUS_PAYLOAD_MAX));
        full.push(0x07);
        assert_eq!(pass_all(&[&full]).len(), 1);
        // Past the parser's own limit the sequence is flagged as cut.
        let mut huge = b"\x1b]7501;".to_vec();
        huge.extend(std::iter::repeat_n(b'x', cmdterm_vt::OSC_MAX_LEN + 10));
        huge.push(0x07);
        assert!(pass_all(&[&huge]).is_empty());
    }

    #[test]
    fn an_unterminated_report_does_not_hide_a_later_one() {
        // The next escape sequence ends the cut-off report, whose state no
        // report names, and the later report still arrives.
        let found = pass_all(&[
            b"\x1b]7501;state=work",
            b"ing and then plain output\r\n\x1b[31mred",
            b"\x1b]7501;state=done:app=x\x1b\\",
        ]);
        assert_eq!(found.last().unwrap(), b"state=done:app=x");
        let cell = AttentionCell::new();
        for body in &found {
            cell.apply_status_report(body);
        }
        assert_eq!(cell.get(), Attention::Done);
        // Cancelled by CAN, then plain text that looks like a body end.
        assert_eq!(
            pass_all(&[b"\x1b]7501;state=working\x18 state=x\x07\x1b]7501;state=clear\x07"]),
            vec![b"state=clear".to_vec()]
        );
    }

    #[test]
    fn events_from_the_model_drive_the_cell() {
        let mut term = cmdterm_vt::Terminal::new(
            cmdterm_vt::Size { rows: 4, cols: 40 },
            cmdterm_vt::Config::default(),
        );
        let cell = AttentionCell::new();
        // OSC 9 notification, OSC 9;4 progress, OSC 1337 user variable, title.
        term.advance(b"\x1b]9;Claude needs your permission\x07");
        for event in term.drain_events() {
            cell.apply(event);
        }
        assert_eq!(cell.get(), Attention::Permission);
        assert_eq!(cell.message(), "Claude needs your permission");
        cell.note_input();
        term.advance(b"\x1b]1337;SetUserVar=cmdterm_agent=YnVzeQ==\x07\x1b]2;Fix it\x07");
        for event in term.drain_events() {
            cell.apply(event);
        }
        assert_eq!(cell.get(), Attention::Busy);
        assert_eq!(cell.agent_title(), "Fix it");
        term.advance(b"\x1b]9;4;0\x07");
        for event in term.drain_events() {
            cell.apply(event);
        }
        assert_eq!(cell.get(), Attention::None);
    }

    #[test]
    fn a_paused_progress_bar_is_no_progress() {
        let cell = AttentionCell::new();
        cell.apply(Event::Progress(Progress::Indeterminate));
        assert_eq!(cell.get(), Attention::Busy);
        cell.apply(Event::Progress(Progress::Paused(30)));
        assert_eq!(cell.get(), Attention::None);
    }

    #[test]
    fn toasts_join_title_and_body() {
        let cell = AttentionCell::new();
        cell.apply(Event::Toast {
            title: Some("Codex".into()),
            body: "Approval requested".into(),
        });
        assert_eq!(cell.get(), Attention::Permission);
        assert_eq!(cell.message(), "Codex: Approval requested");
    }
}
