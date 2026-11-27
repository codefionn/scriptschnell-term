//! The approval overlay: a request from a sandbox that an extension guard
//! in `Ask` mode holds back, such as a `git push`, with Allow and Block, or
//! a change to an extension an agent asked for through the control socket
//! (`control.rs`), with Allow and Deny, a settings change an agent
//! suggests, such as one more allowed host, or a new worktree and agents to
//! start there, which an agent asked for the same way. A guarded request
//! and a launch can be allowed for the project until the app quits or for
//! good, and a guarded request blocked for good. It is its own region and shows the
//! oldest question, guards first; answering it shows the next one.
//! Dismissing it refuses, and so does Enter, since the refusing button has
//! the focus. A change a coding agent made to its settings in a sandbox
//! (`agent_sync.rs`) asks as a guarded request does, listing the changed
//! keys.

use std::path::{Path, PathBuf};

use std::borrow::Cow;

use schnellui::scene::Size;
use schnellui::widgets::{
    Align, Button, ButtonAppearance, Column, Dialog, DialogPosition, Justify, Row, Text,
    ThemeProvider, View, WrapMode,
};

use super::theme::Scheme;
use super::{Closed, truncate};
use crate::state::{GuardEntry, Project};

use crate::control::{FileChange, Outcome, SettingOp, Target};
use crate::workspace::Workspace;

impl Workspace {
    /// Answers question `id` of the approval overlay.
    pub fn answer_approval(&self, id: u64, allow: bool) {
        let mut inner = self.borrow_mut();
        inner.runtimes.approvals.answer(id, allow);
        inner.dirty.approval = true;
    }

    /// Answers question `id` and keeps the answer for good for its guard
    /// and host in the project it came from: in the project's always
    /// allowed list, or with `allow` false in its always blocked one. The
    /// project keeps them in the state.
    pub fn remember_approval(&self, id: u64, allow: bool) {
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            let question = (inner.runtimes.approvals.pending().into_iter()).find(|q| q.id == id);
            let project = question
                .as_ref()
                .and_then(|q| q.scope.as_deref()?.parse().ok());
            if let (Some(question), Some(project)) = (question, project)
                && let Some(project) = inner.state.project_mut(project)
            {
                let entry = GuardEntry {
                    guard: question.guard,
                    host: question.host,
                };
                put_answer(project, entry, !allow);
                inner.runtimes.share_always_allowed(project);
            }
            inner.runtimes.approvals.answer(id, allow);
            inner.dirty.approval = true;
            inner.dirty.settings = true;
        }
        self.save();
    }

    /// Allows question `id` and, for good, its guard and host in the
    /// project it came from.
    pub fn allow_approval_always(&self, id: u64) {
        self.remember_approval(id, true);
    }

    /// Blocks question `id` and, for good, its guard and host in the
    /// project it came from.
    pub fn block_approval_always(&self, id: u64) {
        self.remember_approval(id, false);
    }

    /// Allows question `id`, and its guard and host for requests from the
    /// same project, until the app quits.
    pub fn allow_approval_session(&self, id: u64) {
        let mut inner = self.borrow_mut();
        inner.runtimes.approvals.allow_for_session(id);
        inner.dirty.approval = true;
    }

    /// Takes back entry `index` of what project `project` allowed for good.
    pub fn revoke_always_allowed(&self, project: u64, index: usize) {
        self.remove_project_answer(project, false, index);
    }

    /// Changes the always allowed and blocked lists of project `project`
    /// with `change`, which says whether it changed them, then hands them to
    /// the approvals and saves.
    fn change_project_answers(&self, project: u64, change: impl FnOnce(&mut Project) -> bool) {
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            let Some(project) = inner.state.project_mut(project) else {
                return;
            };
            if !change(project) {
                return;
            }
            inner.runtimes.share_always_allowed(project);
            inner.dirty.settings = true;
        }
        self.save();
    }

    /// Takes back entry `index` of the always allowed list of project
    /// `project`, or with `blocked` of its always blocked list.
    pub fn remove_project_answer(&self, project: u64, blocked: bool, index: usize) {
        self.change_project_answers(project, |project| {
            let list = project.answers_mut(blocked);
            if index >= list.len() {
                return false;
            }
            list.remove(index);
            true
        });
    }

    /// Moves entry `index` of the always allowed list of project `project`
    /// to its always blocked list, or with `blocked` the other way round.
    pub fn move_project_answer(&self, project: u64, blocked: bool, index: usize) {
        self.change_project_answers(project, |project| {
            let list = project.answers_mut(blocked);
            if index >= list.len() {
                return false;
            }
            let entry = list.remove(index);
            put_answer(project, entry, !blocked);
            true
        });
    }

    /// Picks the guard the Allowed and Blocked pages add an entry for.
    pub fn set_project_answer_guard(&self, guard: &str) {
        let mut inner = self.borrow_mut();
        inner.settings_ui.machines.form.project_guard = Some(guard.to_owned());
        inner.settings_ui.picker = None;
        inner.dirty.settings = true;
    }

    /// Adds the guard picked and the host typed on the Allowed page to the
    /// active project's always allowed list, or with `blocked` on the
    /// Blocked page to its always blocked list. An entry is in one list at
    /// most, so it leaves the other one.
    pub fn add_project_answer(&self, blocked: bool) {
        let (project, entry) = {
            let mut inner = self.borrow_mut();
            let Some(project) = inner.state.active().map(|p| p.id) else {
                return;
            };
            let guard = match inner.settings_ui.machines.form.project_guard.clone() {
                Some(guard) => guard,
                None => {
                    let guards = crate::runtime::all_guards(inner.ext.sandbox_rules());
                    let Some(first) = guards.into_iter().next() else {
                        return;
                    };
                    first.id
                }
            };
            let typed = inner.settings_ui.machines.form.project_answer_host.trim();
            if typed.is_empty() {
                return;
            }
            // The proxy compares host names in lowercase; a secret's label
            // and a settings file keep their case.
            let host = if [
                cmdterm_env::KEYRING_GUARD,
                cmdterm_env::AGENT_SETTINGS_GUARD,
                cmdterm_env::GIT_SIGN_GUARD,
            ]
            .contains(&guard.as_str())
            {
                typed.to_owned()
            } else {
                typed.to_ascii_lowercase()
            };
            inner.settings_ui.machines.form.project_answer_host.clear();
            inner.dirty.settings = true;
            (project, GuardEntry { guard, host })
        };
        self.change_project_answers(project, |project| {
            put_answer(project, entry, blocked);
            true
        });
    }

    pub fn approval_is_open(&self) -> bool {
        let inner = self.borrow();
        inner.pending_approval().is_some()
            || inner.pending_change().is_some()
            || inner.pending_suggestion().is_some()
            || inner.pending_launch().is_some()
    }

    /// Answers settings suggestion `id`. Allow applies its steps to the
    /// settings as they are now, which may differ from when the agent
    /// asked, and returns the changed keys' new values to the agent.
    pub fn answer_suggestion(&self, id: u64, allow: bool) {
        let control = {
            let mut inner = self.borrow_mut();
            inner.dirty.approval = true;
            inner.running_control()
        };
        let Some(control) = control else { return };
        let Some(suggestion) = control.take_suggestion(id) else {
            return;
        };
        if !allow {
            control.finish(id, Outcome::Denied);
            return;
        }
        let current = serde_json::to_value(&self.borrow().settings).unwrap_or_default();
        let changed = crate::control::apply_ops(&current, &suggestion.ops)
            .and_then(|value| Ok((crate::control::check_settings(value.clone())?, value)));
        let (settings, value) = match changed {
            Ok(changed) => changed,
            Err(error) => {
                control.finish(id, Outcome::Failed(error));
                return;
            }
        };
        self.replace_settings(settings);
        let mut changed = serde_json::Map::new();
        for op in &suggestion.ops {
            let path = op_path(op);
            let mut node = &value;
            for key in path.split('.') {
                node = &node[key];
            }
            changed.insert(path.to_owned(), node.clone());
        }
        let tr = self.borrow().tr;
        self.notify(tr.control_settings_applied);
        control.finish(
            id,
            Outcome::Applied(serde_json::json!({ "applied": true, "settings": changed })),
        );
    }

    /// Answers launch `id`. Allow creates the worktree, or switches to the
    /// existing one, and starts the agents there; the agent's request
    /// returns once the worktree exists.
    pub fn answer_launch(&self, id: u64, allow: bool) {
        let control = {
            let mut inner = self.borrow_mut();
            inner.dirty.approval = true;
            inner.running_control()
        };
        let Some(control) = control else { return };
        let Some(launch) = control.take_launch(id) else {
            return;
        };
        if !allow {
            control.finish(id, Outcome::Denied);
            return;
        }
        let Some(project) = launch.terminal.project_id else {
            control.finish(
                id,
                Outcome::Failed("the terminal belongs to no project".into()),
            );
            return;
        };
        match launch.target.clone() {
            Target::New { branch, from, name } => {
                self.borrow_mut().launches.push((project, launch));
                let added = self.add_worktree_in(
                    project,
                    branch.as_deref(),
                    from.as_deref(),
                    name.as_deref(),
                );
                if !added {
                    self.fail_launches(project, "the project's repository is not loaded yet");
                }
            }
            Target::Existing(worktree) => {
                let Some(path) = self.find_worktree(project, &worktree) else {
                    let message = format!("{worktree} names no worktree of the project");
                    control.finish(id, Outcome::Failed(message));
                    return;
                };
                self.activate_worktree(project, &path);
                self.start_agents(&control, launch, &path);
            }
        }
    }

    /// Allows launch `id`, and every later one from the same project:
    /// until the app quits, or with `for_good` in the project's state.
    pub fn allow_launches(&self, id: u64, for_good: bool) {
        let project = (self.pending_launches().into_iter())
            .find(|launch| launch.id == id)
            .and_then(|launch| launch.terminal.project_id);
        if let Some(project) = project {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            if for_good {
                if let Some(project) = inner.state.project_mut(project) {
                    project.allows_launches = true;
                }
                inner.dirty.settings = true;
            } else if !inner.launches_allowed.contains(&project) {
                inner.launches_allowed.push(project);
            }
        }
        if for_good {
            self.save();
        }
        self.answer_launch(id, true);
        self.allow_launches_without_asking();
    }

    /// Takes back that project `project` allowed its launches for good.
    pub fn revoke_launches(&self, project: u64) {
        {
            let mut inner = self.borrow_mut();
            let Some(project) = inner.state.project_mut(project) else {
                return;
            };
            project.allows_launches = false;
            inner.dirty.settings = true;
        }
        self.save();
    }

    /// Allows the waiting launches of projects that allowed them for this
    /// session or for good.
    pub(crate) fn allow_launches_without_asking(&self) {
        let allowed: Vec<u64> = {
            let inner = self.borrow();
            (self.pending_launches().into_iter())
                .filter(|launch| {
                    launch.terminal.project_id.is_some_and(|project| {
                        inner.launches_allowed.contains(&project)
                            || inner
                                .state
                                .project(project)
                                .is_some_and(|p| p.allows_launches)
                    })
                })
                .map(|launch| launch.id)
                .collect()
        };
        for id in allowed {
            self.answer_launch(id, true);
        }
    }

    fn pending_launches(&self) -> Vec<crate::control::Launch> {
        let control = self.borrow().running_control();
        control.map_or_else(Vec::new, |control| control.pending_launches())
    }

    /// The worktree of project `project` that `worktree` names: its
    /// absolute path, the branch checked out in it, or its directory's name.
    fn find_worktree(&self, project: u64, worktree: &str) -> Option<PathBuf> {
        let inner = self.borrow();
        let listed = (inner.snapshots.get(&project)).map_or(&[][..], |s| &s.worktrees[..]);
        let mut listed = listed.iter().filter(|w| !w.prunable);
        let path = Path::new(worktree);
        if path.is_absolute() {
            let known = listed.any(|w| w.path == path)
                || inner.state.project(project).is_some_and(|p| p.root == path);
            return known.then(|| path.to_path_buf());
        }
        let listed: Vec<_> = listed.collect();
        (listed.iter())
            .find(|w| w.branch.as_deref() == Some(worktree))
            .or_else(|| (listed.iter()).find(|w| w.path.file_name().is_some_and(|n| n == worktree)))
            .map(|w| w.path.clone())
    }

    /// Starts the agents of allowed launches in project `project` once its
    /// new worktree at `path` is active.
    pub(crate) fn finish_launches(&self, project: u64, path: &Path) {
        let waiting = self.take_launches(project);
        let Some(control) = self.borrow().running_control() else {
            return;
        };
        for launch in waiting {
            self.start_agents(&control, launch, path);
        }
    }

    /// Ends allowed launches in `project` whose worktree could not be made.
    pub(crate) fn fail_launches(&self, project: u64, message: &str) {
        let waiting = self.take_launches(project);
        if let Some(control) = self.borrow().running_control() {
            for launch in waiting {
                control.finish(launch.id, Outcome::Failed(message.to_owned()));
            }
        }
    }

    fn take_launches(&self, project: u64) -> Vec<crate::control::Launch> {
        let mut inner = self.borrow_mut();
        let (waiting, rest) = std::mem::take(&mut inner.launches)
            .into_iter()
            .partition(|(id, _)| *id == project);
        inner.launches = rest;
        waiting.into_iter().map(|(_, launch)| launch).collect()
    }

    /// Opens one terminal per agent in the active worktree and reports it.
    fn start_agents(
        &self,
        control: &crate::control::Control,
        launch: crate::control::Launch,
        path: &Path,
    ) {
        let mut started = Vec::new();
        for start in &launch.agents {
            let agent = start.agent;
            // The arguments, with a prompt among them, run this time only.
            let argv = vec![agent.program().to_owned()];
            let terminal =
                self.open_terminal_once(argv, start.args.clone(), agent.name().to_owned());
            // A prompt the agent takes typed waits until it is ready.
            if let (Some(terminal), Some(prompt)) = (terminal, &start.paste) {
                crate::agents::paste_when_ready(terminal.session.input(), agent, prompt.clone());
            }
            started.push(agent.program());
        }
        let vcs = launch.terminal.project_id.and_then(|project| {
            let inner = self.borrow();
            Some(inner.snapshots.get(&project)?.repo.kind)
        });
        control.finish(
            launch.id,
            Outcome::Applied(
                serde_json::json!({ "worktree": path, "vcs": vcs, "agents": started }),
            ),
        );
    }

    /// Answers extension change `id`. Allow writes its files and reloads
    /// the extensions; the agent's request then returns the extension's
    /// status, so it sees whether the extension loaded.
    pub fn answer_change(&self, id: u64, allow: bool) {
        let control = {
            let mut inner = self.borrow_mut();
            inner.dirty.approval = true;
            inner.running_control()
        };
        let Some((change, files)) = control.as_ref().and_then(|c| c.take(id)) else {
            return;
        };
        let control = control.expect("took the change from it");
        if !allow {
            control.finish(id, Outcome::Denied);
            return;
        }
        if let Err(error) = control.write(&change.extension, &files) {
            let tr = self.borrow().tr;
            self.notify((tr.control_failed)(&error));
            control.finish(id, Outcome::Failed(error));
            return;
        }
        self.reload_extensions();
        let (status, tr) = {
            let inner = self.borrow();
            let status = crate::workspace::extension_status(&inner.ext, &inner.settings)
                .into_iter()
                .find(|status| status["name"] == change.extension.as_str());
            (status, inner.tr)
        };
        self.notify((tr.control_applied)(&change.extension));
        control.finish(id, Outcome::Applied(status.unwrap_or_default()));
    }
}

/// The overlay: a dialog while a question waits, an empty slot otherwise.
pub fn approval(ws: &Workspace, viewport: Size) -> Box<dyn View> {
    let inner = ws.borrow();
    let Some(question) = inner.pending_approval() else {
        let (change_waits, suggestion_waits) = (
            inner.pending_change().is_some(),
            inner.pending_suggestion().is_some(),
        );
        drop(inner);
        return if change_waits {
            change(ws, viewport)
        } else if suggestion_waits {
            suggestion(ws, viewport)
        } else {
            launch(ws, viewport)
        };
    };
    let (scheme, tr, focus) = (inner.scheme, inner.tr, inner.regions.approval_focus);
    // Only a question from a known project can be allowed for good.
    let project = (question.scope.as_deref())
        .and_then(|scope| scope.parse().ok())
        .and_then(|id| inner.state.project(id))
        .map(|project| project.name.clone());
    drop(inner);
    let width = content_width(viewport);
    let id = question.id;
    let keyring = question.guard == cmdterm_env::KEYRING_GUARD;
    let agent_settings = question.guard == cmdterm_env::AGENT_SETTINGS_GUARD;
    let signing = question.guard == cmdterm_env::GIT_SIGN_GUARD;
    let (request, note_text) = if agent_settings {
        let file = truncate(&question.host, LONG);
        (
            (tr.agent_settings_request)(&question.method, &file),
            tr.agent_settings_note,
        )
    } else if keyring {
        let label = truncate(&question.host, LONG);
        (
            (tr.keyring_request)(&question.method, &label),
            tr.keyring_note,
        )
    } else if signing {
        let key = truncate(&question.host, LONG);
        (
            (tr.git_sign_request)(&question.target, &key),
            tr.git_sign_note,
        )
    } else if question.guard == cmdterm_env::OPEN_URL_GUARD {
        let link = truncate(&question.target, LONG);
        ((tr.open_url_request)(&link), tr.open_url_note)
    } else {
        let request = format!(
            "{} {}{}",
            question.method,
            question.host,
            truncate(&question.target, LONG)
        );
        (request, tr.approval_note)
    };
    let (allow, block, dismiss, always) = (ws.clone(), ws.clone(), ws.clone(), ws.clone());
    let (session, never) = (ws.clone(), ws.clone());
    let mut body = Column::new()
        .width(width)
        .gap(6.0)
        .child(heading((tr.approval_question)(&question.title)))
        .child(line(request));
    match &question.detail {
        // One line per key: its path, then a tab and the new value.
        Some(detail) if agent_settings => {
            let mut keys = Column::new().width(width).gap(2.0);
            const SHOWN: usize = 12;
            for change in detail.lines().take(SHOWN) {
                let text = match change.split_once('\t') {
                    Some((key, value)) => {
                        format!("{} = {}", truncate(key, LONG), truncate(value, LONG))
                    }
                    None => (tr.agent_settings_removed)(&truncate(change, LONG)),
                };
                keys = keys.child(line(text));
            }
            let more = detail.lines().count().saturating_sub(SHOWN);
            if more > 0 {
                keys = keys.child(note(scheme, (tr.agent_settings_more)(more)));
            }
            body = body.child(keys);
        }
        Some(detail) if signing => {
            body = body.child(line((tr.git_sign_about)(&truncate(detail, LONG))));
        }
        Some(detail) => body = body.child(line((tr.approval_sends)(&truncate(detail, LONG)))),
        None => {}
    }
    let mut lasting = vec![lasting_button(tr.control_allow_session, move || {
        session.allow_approval_session(id)
    })];
    if let Some(name) = &project {
        body = body.child(note(scheme, (tr.approval_project)(name)));
        lasting.push(lasting_button(tr.approval_always, move || {
            always.allow_approval_always(id)
        }));
        lasting.push(lasting_button(tr.approval_never, move || {
            never.block_approval_always(id)
        }));
    }
    let body = body.child(note(scheme, note_text));
    let footer = footer(
        width,
        lasting,
        refuse_button(tr.approval_block, move || block.answer_approval(id, false)).with_ref(focus),
        allow_button(tr.approval_allow, move || allow.answer_approval(id, true)),
    );
    Box::new(
        dialog(tr.approval_dialog, scheme, width, body, footer)
            .on_dismiss(move || dismiss.answer_approval(id, false)),
    )
}

/// Puts `entry` into the always allowed list of `project`, or with
/// `blocked` into its always blocked list, and takes it out of the other.
fn put_answer(project: &mut Project, entry: GuardEntry, blocked: bool) {
    project
        .answers_mut(!blocked)
        .retain(|known| *known != entry);
    let list = project.answers_mut(blocked);
    if !list.contains(&entry) {
        list.push(entry);
    }
}

// --- layout ------------------------------------------------------------------
//
// Every approval dialog is a column of wrapping text over one footer row.
// Text breaks into lines at the dialog's width instead of being cut to a
// character count, so long paths, commands and German strings stay inside
// the frame. `LONG` only bounds how tall one value can make the dialog.

/// Widest the dialog gets, in pixels.
const MAX_WIDTH: f32 = 560.0;
/// Narrowest the dialog gets, even in a smaller window.
const MIN_WIDTH: f32 = 300.0;
/// Space kept between the dialog and the window's edges.
const MARGIN: f32 = 24.0;
const PAD: f32 = 16.0;
/// Characters shown of one path, command or value before it is cut.
const LONG: usize = 240;

/// The width the dialog's content gets in a `viewport`-sized window.
fn content_width(viewport: Size) -> f32 {
    (viewport.width - MARGIN * 2.0).clamp(MIN_WIDTH, MAX_WIDTH) - PAD * 2.0
}

/// The question a dialog asks.
fn heading(text: impl Into<Cow<'static, str>>) -> Text {
    Text::new(text).size(15.0).wrap(WrapMode::Anywhere)
}

/// One detail of the request, such as a file or a command.
fn line(text: impl Into<Cow<'static, str>>) -> Text {
    Text::new(text).size(12.5).wrap(WrapMode::Anywhere)
}

/// Muted context under the question.
fn note<T: Into<Cow<'static, str>>>(scheme: &Scheme, text: T) -> impl View + use<T> {
    ThemeProvider::new(
        scheme.muted_theme(),
        Text::new(text).size(12.0).wrap(WrapMode::Anywhere),
    )
}

/// A button that answers for longer than this one request.
fn lasting_button(text: &'static str, click: impl FnMut() + 'static) -> Button {
    Button::new(text)
        .appearance(ButtonAppearance::Ghost)
        .text_size(13.0)
        .on_click(click)
}

/// The refusing button. Callers give it the focus, so Enter refuses.
fn refuse_button(text: &'static str, click: impl FnMut() + 'static) -> Button {
    lasting_button(text, click)
}

fn allow_button(text: &'static str, click: impl FnMut() + 'static) -> Button {
    Button::new(text).text_size(13.0).on_click(click)
}

/// The buttons, right-aligned on one line. When a language's labels do not
/// fit, the line breaks: the lasting answers flow onto as many lines as they
/// need, and the refusing and allowing buttons stay together at the end.
fn footer(width: f32, lasting: Vec<Button>, refuse: impl View, allow: Button) -> Row {
    let answers = Row::new()
        .gap(8.0)
        .align(Align::Center)
        .child(refuse)
        .child(allow);
    let mut row = Row::new()
        .width(width)
        .wrap()
        .gap(16.0)
        .justify(Justify::End)
        .align(Align::Center);
    for button in lasting {
        row = row.child(button);
    }
    row.child(answers)
}

/// The frame every approval dialog shares, sized to `width` of content.
fn dialog(name: &'static str, scheme: &Scheme, width: f32, body: Column, footer: Row) -> Dialog {
    Dialog::new(name)
        .modal()
        .undecorated()
        .position(DialogPosition::Center)
        .width(width + PAD * 2.0)
        .max_width(width + PAD * 2.0)
        .backdrop(scheme.backdrop)
        .surface(scheme.rail)
        .padding(PAD)
        .gap(12.0)
        .child(body)
        .child(footer)
}

/// The dialog for an extension change: which terminal asks, what it
/// writes, and what the extension will ask for.
fn change(ws: &Workspace, viewport: Size) -> Box<dyn View> {
    let inner = ws.borrow();
    let Some(change) = inner.pending_change() else {
        return Box::new(Closed);
    };
    let (scheme, tr, focus) = (inner.scheme, inner.tr, inner.regions.approval_focus);
    drop(inner);
    let width = content_width(viewport);
    let id = change.id;
    let question = if change.new {
        (tr.control_install)(&change.extension)
    } else {
        (tr.control_change)(&change.extension)
    };
    let terminal = &change.terminal;
    let worktree = truncate(&terminal.worktree.to_string_lossy(), LONG);
    let place = if terminal.project.is_empty() {
        worktree
    } else {
        format!("{} ({worktree})", terminal.project)
    };
    let source = (tr.control_source)(&place, &terminal.machine, terminal.kind);
    let mut files = Column::new().width(width).gap(2.0);
    const SHOWN: usize = 12;
    for file in change.files.iter().take(SHOWN) {
        let text = match file {
            FileChange::Added { path, lines } => {
                (tr.control_file_added)(&truncate(path, LONG), *lines)
            }
            FileChange::Changed { path, lines } => {
                (tr.control_file_changed)(&truncate(path, LONG), *lines)
            }
            FileChange::Removed { path } => (tr.control_file_removed)(&truncate(path, LONG)),
        };
        files = files.child(line(text));
    }
    if change.files.len() > SHOWN {
        files = files.child(note(
            scheme,
            (tr.control_more_files)(change.files.len() - SHOWN),
        ));
    }
    let permissions = if change.permissions.is_empty() {
        tr.control_no_permissions.to_owned()
    } else {
        (tr.control_permissions)(&change.permissions.join(", "))
    };
    let mut body = Column::new()
        .width(width)
        .gap(6.0)
        .child(heading(question))
        .child(note(scheme, source))
        .child(files)
        .child(line(permissions));
    if change.shipped {
        body = body.child(note(scheme, tr.control_shipped));
    }
    body = body.child(note(scheme, tr.control_note));
    let (allow, deny, dismiss) = (ws.clone(), ws.clone(), ws.clone());
    let footer = footer(
        width,
        Vec::new(),
        refuse_button(tr.control_deny, move || deny.answer_change(id, false)).with_ref(focus),
        allow_button(tr.approval_allow, move || allow.answer_change(id, true)),
    );
    Box::new(
        dialog(tr.control_dialog, scheme, width, body, footer)
            .on_dismiss(move || dismiss.answer_change(id, false)),
    )
}

/// The dialog for a launch: the worktree to create or open, and the agents
/// to start there with their arguments.
fn launch(ws: &Workspace, viewport: Size) -> Box<dyn View> {
    let inner = ws.borrow();
    let Some(launch) = inner.pending_launch() else {
        return Box::new(Closed);
    };
    let (scheme, tr, focus) = (inner.scheme, inner.tr, inner.regions.approval_focus);
    drop(inner);
    let width = content_width(viewport);
    let id = launch.id;
    let terminal = &launch.terminal;
    let (question, start) = match &launch.target {
        Target::New { branch, from, .. } => {
            let question = match branch {
                Some(branch) => {
                    (tr.control_new_worktree)(&truncate(branch, LONG), &terminal.project)
                }
                None => (tr.control_new_detached)(&terminal.project),
            };
            let start = from.as_deref().map_or_else(
                || tr.control_worktree_from_main.to_owned(),
                |from| (tr.control_worktree_from)(&truncate(from, LONG)),
            );
            (question, Some(start))
        }
        Target::Existing(worktree) => ((tr.control_open_worktree)(&truncate(worktree, LONG)), None),
    };
    let worktree = terminal.worktree.to_string_lossy();
    let source = (tr.control_source)(&truncate(&worktree, LONG), &terminal.machine, terminal.kind);
    let mut agents = Column::new().width(width).gap(2.0);
    for start in &launch.agents {
        let command = std::iter::once(start.agent.program().to_owned())
            .chain(start.args.iter().map(|arg| shell_quote(arg)))
            .collect::<Vec<_>>()
            .join(" ");
        agents = agents.child(line((tr.control_start_agent)(&truncate(&command, LONG))));
        if let Some(prompt) = &start.paste {
            agents = agents.child(line((tr.control_type_prompt)(&truncate(prompt, LONG))));
        }
    }
    let mut body = Column::new().width(width).gap(6.0).child(heading(question));
    if let Some(start) = start {
        body = body.child(line(start));
    }
    let body = body
        .child(note(scheme, source))
        .child(agents)
        .child(note(scheme, tr.control_launch_note));
    let (allow, deny, dismiss) = (ws.clone(), ws.clone(), ws.clone());
    let (session, always) = (ws.clone(), ws.clone());
    let footer = footer(
        width,
        vec![
            lasting_button(tr.control_allow_session, move || {
                session.allow_launches(id, false)
            }),
            lasting_button(tr.approval_always, move || always.allow_launches(id, true)),
        ],
        refuse_button(tr.control_deny, move || deny.answer_launch(id, false)).with_ref(focus),
        allow_button(tr.approval_allow, move || allow.answer_launch(id, true)),
    );
    Box::new(
        dialog(tr.control_launch_dialog, scheme, width, body, footer)
            .on_dismiss(move || dismiss.answer_launch(id, false)),
    )
}

/// `arg` as a shell would need it, so the dialog shows what runs.
fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@".contains(&b))
    {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn op_path(op: &SettingOp) -> &str {
    match op {
        SettingOp::Set(path, _) | SettingOp::Add(path, _) | SettingOp::Remove(path, _) => path,
    }
}

/// A JSON value on one line, cut so one value cannot fill the dialog.
fn compact(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    truncate(&text, LONG)
}

/// The dialog for a settings suggestion: the agent's reason and each key
/// it changes, before and after.
fn suggestion(ws: &Workspace, viewport: Size) -> Box<dyn View> {
    let inner = ws.borrow();
    let Some(suggestion) = inner.pending_suggestion() else {
        return Box::new(Closed);
    };
    let current = serde_json::to_value(&inner.settings).unwrap_or_default();
    let (scheme, tr, focus) = (inner.scheme, inner.tr, inner.regions.approval_focus);
    drop(inner);
    let width = content_width(viewport);
    let id = suggestion.id;
    let terminal = &suggestion.terminal;
    let worktree = terminal.worktree.to_string_lossy();
    let source = (tr.control_source)(&truncate(&worktree, LONG), &terminal.machine, terminal.kind);
    let mut steps = Column::new().width(width).gap(2.0);
    for op in &suggestion.ops {
        let text = match op {
            SettingOp::Set(path, value) => {
                let mut before = &current;
                for key in path.split('.') {
                    before = &before[key];
                }
                (tr.control_setting_set)(path, &compact(before), &compact(value))
            }
            SettingOp::Add(path, items) => {
                let items: Vec<String> = items.iter().map(compact).collect();
                (tr.control_setting_add)(path, &items.join(", "))
            }
            SettingOp::Remove(path, items) => {
                let items: Vec<String> = items.iter().map(compact).collect();
                (tr.control_setting_remove)(path, &items.join(", "))
            }
        };
        steps = steps.child(line(text));
    }
    let body = Column::new()
        .width(width)
        .gap(6.0)
        .child(heading(tr.control_settings_question))
        .child(note(scheme, source))
        .child(line((tr.control_reason)(&truncate(
            &suggestion.reason,
            300,
        ))))
        .child(steps)
        .child(note(scheme, tr.control_settings_note));
    let (allow, deny, dismiss) = (ws.clone(), ws.clone(), ws.clone());
    let footer = footer(
        width,
        Vec::new(),
        refuse_button(tr.control_deny, move || deny.answer_suggestion(id, false)).with_ref(focus),
        allow_button(tr.approval_allow, move || allow.answer_suggestion(id, true)),
    );
    Box::new(
        dialog(tr.control_settings_dialog, scheme, width, body, footer)
            .on_dismiss(move || dismiss.answer_suggestion(id, false)),
    )
}
