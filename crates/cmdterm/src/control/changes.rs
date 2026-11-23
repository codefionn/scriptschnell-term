//! The change requests of worktrees for agents: what the app's own forge
//! checks (`crate::forges`) last saw, served without asking a forge.
//!
//! The UI thread publishes one [`ChangeEntry`] per worktree whenever a
//! check, a discussion or a snapshot changes what it would say. An entry
//! whose report differs from the one before gets a new revision.
//! `change_request` reads an entry, `change_request_wait` waits until its
//! revision passes the one the agent saw.
//!
//! A read marks the worktree as watched for [`LINGER`], a wait for as long
//! as it waits and [`LINGER`] after. The app adds the branches of watched
//! worktrees to its regular checks, which otherwise only look at agents
//! that finished, and asks the forge for their discussion again when the
//! check sees their refs move or their change request change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Terminal;

/// How long a worktree stays watched after an agent last asked about it.
pub const LINGER: Duration = Duration::from_secs(15 * 60);
/// How long `change_request_wait` waits without a `timeout`.
const DEFAULT_WAIT: Duration = Duration::from_secs(240);
/// The longest `change_request_wait` waits: below the 900 seconds the MCP
/// server on stdio (`browser-mcp`) gives each answer.
pub const MAX_TOOL_WAIT: Duration = Duration::from_secs(840);

/// The names of the built-in tools.
pub const TOOLS: [&str; 2] = ["change_request", "change_request_wait"];

/// The change request of one worktree, as the UI thread publishes it.
#[derive(Clone, Debug)]
pub struct ChangeEntry {
    pub project: u64,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    /// What the tools answer, without `revision`.
    pub report: Value,
    /// The change request page, for `full`.
    pub page: Option<Arc<crate::discussion::Rendered>>,
}

struct Entry {
    info: ChangeEntry,
    revision: u64,
}

struct Watch {
    /// Waits running on it.
    waiting: usize,
    /// When it stops counting once nobody waits.
    until: Instant,
}

/// The published entries and the watched worktrees.
#[derive(Default)]
pub(super) struct Changes {
    entries: Vec<Entry>,
    revision: u64,
    published: bool,
    watched: HashMap<(u64, PathBuf), Watch>,
    /// Worktrees watched since the UI thread looked last.
    fresh: Vec<(u64, PathBuf)>,
}

impl Changes {
    /// Replaces the entries. True when one changed, came or went.
    pub(super) fn publish(&mut self, entries: Vec<ChangeEntry>) -> bool {
        let mut old: HashMap<(u64, PathBuf), Entry> = std::mem::take(&mut self.entries)
            .into_iter()
            .map(|e| ((e.info.project, e.info.worktree.clone()), e))
            .collect();
        let mut changed = !self.published;
        self.published = true;
        for info in entries {
            let key = (info.project, info.worktree.clone());
            let revision = match old.remove(&key) {
                Some(entry) if entry.info.report == info.report => entry.revision,
                _ => {
                    changed = true;
                    self.revision += 1;
                    self.revision
                }
            };
            self.entries.push(Entry { info, revision });
        }
        changed || !old.is_empty()
    }

    /// Marks `key` as watched; `wait` counts a running wait. True when
    /// it was not watched before.
    fn watch(&mut self, key: (u64, PathBuf), wait: bool) -> bool {
        let now = Instant::now();
        let watch = self.watched.entry(key.clone()).or_insert(Watch {
            waiting: 0,
            until: now,
        });
        let new = watch.waiting == 0 && watch.until <= now;
        watch.waiting += usize::from(wait);
        watch.until = now + LINGER;
        if new {
            self.fresh.push(key);
        }
        new
    }

    /// Ends a wait on `key`.
    fn unwatch(&mut self, key: &(u64, PathBuf)) {
        if let Some(watch) = self.watched.get_mut(key) {
            watch.waiting = watch.waiting.saturating_sub(1);
            watch.until = Instant::now() + LINGER;
        }
    }

    pub(super) fn watched(&mut self) -> Vec<(u64, PathBuf)> {
        let now = Instant::now();
        self.watched.retain(|_, w| w.waiting > 0 || w.until > now);
        self.watched.keys().cloned().collect()
    }

    pub(super) fn take_fresh(&mut self) -> Vec<(u64, PathBuf)> {
        std::mem::take(&mut self.fresh)
    }

    /// The entry of the worktree `wanted` names in `terminal`'s project:
    /// its path, branch or folder name. Without one, the terminal's own
    /// worktree, or the one it runs in a directory of.
    fn find(&self, terminal: &Terminal, wanted: Option<&str>) -> Result<usize, (u16, String)> {
        let Some(project) = terminal.project_id else {
            return Err((404, "this terminal belongs to no project".into()));
        };
        if !self.published {
            return Err((
                503,
                "the app has not read its projects yet; ask again".into(),
            ));
        }
        let mine = || (self.entries.iter().enumerate()).filter(|(_, e)| e.info.project == project);
        let found = match wanted.filter(|w| !w.is_empty()) {
            Some(wanted) => {
                let path = Path::new(wanted);
                (mine().find(|(_, e)| e.info.worktree == path))
                    .or_else(|| mine().find(|(_, e)| e.info.branch.as_deref() == Some(wanted)))
                    .or_else(|| {
                        mine().find(|(_, e)| {
                            e.info
                                .worktree
                                .file_name()
                                .is_some_and(|name| name == wanted)
                        })
                    })
                    .map(|(index, _)| index)
                    .ok_or_else(|| {
                        let names: Vec<String> = mine()
                            .map(|(_, e)| match &e.info.branch {
                                Some(branch) => format!("{branch} ({})", e.info.worktree.display()),
                                None => e.info.worktree.display().to_string(),
                            })
                            .collect();
                        (
                            404,
                            format!(
                                "no worktree '{wanted}' in {}; it has {}",
                                terminal.project,
                                names.join(", ")
                            ),
                        )
                    })?
            }
            None => mine()
                .filter(|(_, e)| terminal.worktree.starts_with(&e.info.worktree))
                .max_by_key(|(_, e)| e.info.worktree.components().count())
                .map(|(index, _)| index)
                .ok_or_else(|| {
                    (
                        404,
                        format!(
                            "the app knows no worktree at {} yet; ask again",
                            terminal.worktree.display()
                        ),
                    )
                })?,
        };
        Ok(found)
    }

    /// What the tools answer for entry `index`.
    fn answer(&self, index: usize, full: bool, changed: Option<bool>) -> Value {
        let entry = &self.entries[index];
        let mut answer = entry.info.report.clone();
        if let Some(object) = answer.as_object_mut() {
            object.insert("revision".into(), json!(entry.revision));
            if let Some(changed) = changed {
                object.insert("changed".into(), json!(changed));
            }
            if full && let Some(page) = &entry.info.page {
                object.insert("page".into(), json!(page.markdown()));
            }
        }
        answer
    }
}

/// The built-in tools, as MCP lists them.
pub fn tool_list() -> Vec<Value> {
    let worktree = json!({
        "type": "string",
        "description": "A worktree of this terminal's project: its path, branch or folder name. Defaults to this terminal's worktree.",
    });
    let full = json!({
        "type": "boolean",
        "description": "Also return the whole change request page as Markdown (description, conversation, review threads placed on the current code), when the app holds it.",
    });
    vec![
        json!({
            "name": TOOLS[0],
            "description": "The pull or merge request of a worktree's branch, as scriptschnell-term's own forge checks last saw it, from any forge an extension supports (GitHub, GitLab, Forgejo, Gitea, Bitbucket, Azure DevOps). Answers state (open, merged, closed, none: no change request, unknown: not checked yet, unsupported: no forge extension knows the remote, no_branch), number, title and URL, and once the app has read the discussion, details: draft, conflicts, reviews, CI checks, comment counts and the head commit. It never asks the forge itself, so it costs no API quota; after this call the app keeps the branch in its checks for 15 minutes. To wait for a change, pass the returned revision to change_request_wait instead of calling this in a loop.",
            "inputSchema": {
                "type": "object",
                "properties": { "worktree": worktree, "full": full },
            },
        }),
        json!({
            "name": TOOLS[1],
            "description": "Waits until scriptschnell-term's own forge checks see the pull or merge request of a worktree's branch change (opened, merged, closed, retitled, pushed to, CI checks, reviews or comments), then answers like change_request with changed: true; after timeout seconds it answers changed: false, and you call it again. While it waits the app keeps the branch in its checks: git ls-remote every three minutes finds pushes and merges without using the forge's API, and the forge is asked about the rest when refs move and at least every 30 minutes, so a comment without a push can take that long to show.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "worktree": worktree,
                    "since": {
                        "type": "integer",
                        "description": "The revision of an earlier answer; returns at once when the change request changed after it. Defaults to the current revision.",
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Seconds to wait, 0 to 840, default 240. Keep it below your MCP client's tool timeout.",
                    },
                    "full": full,
                },
            },
        }),
    ]
}

impl super::Control {
    /// Whether the change requests were never published to this socket.
    /// Allocation-free.
    pub fn changes_wanted(&self) -> bool {
        !self
            .changes_published
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Replaces the change requests of every worktree and wakes the waits
    /// whose entry changed.
    pub fn publish_changes(&self, entries: Vec<ChangeEntry>) {
        let changed = self.lock().changes.publish(entries);
        self.changes_published
            .store(true, std::sync::atomic::Ordering::Release);
        if changed {
            self.changes_changed.notify_all();
        }
    }

    /// Whether an agent started watching a worktree since
    /// [`Self::take_new_watches`]. Allocation-free.
    pub fn has_new_watches(&self) -> bool {
        self.new_watches.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The worktrees agents started watching since the last call.
    pub fn take_new_watches(&self) -> Vec<(u64, PathBuf)> {
        self.new_watches
            .store(false, std::sync::atomic::Ordering::Release);
        self.lock().changes.take_fresh()
    }

    /// Every watched worktree, by project id and path.
    pub fn watched(&self) -> Vec<(u64, PathBuf)> {
        self.lock().changes.watched()
    }

    /// Runs built-in tool `name` for `terminal`; `None` when there is no
    /// such tool.
    pub(super) fn builtin_tool(
        &self,
        terminal: &Terminal,
        name: &str,
        arguments: &Value,
    ) -> Option<Result<Value, (u16, String)>> {
        let wait = match name {
            "change_request" => None,
            "change_request_wait" => {
                let timeout = match &arguments["timeout"] {
                    Value::Null => Ok(DEFAULT_WAIT),
                    value => value
                        .as_u64()
                        .map(|secs| Duration::from_secs(secs).min(MAX_TOOL_WAIT))
                        .ok_or((400, "timeout is a number of seconds".to_owned())),
                };
                Some(timeout)
            }
            _ => return None,
        };
        let since = match &arguments["since"] {
            Value::Null => None,
            value => match value.as_u64() {
                Some(since) => Some(since),
                None => return Some(Err((400, "since is a revision number".into()))),
            },
        };
        let wait = match wait.transpose() {
            Ok(wait) => wait,
            Err(error) => return Some(Err(error)),
        };
        Some(self.change_request(
            terminal,
            arguments["worktree"].as_str(),
            arguments["full"].as_bool().unwrap_or(false),
            wait.map(|wait| (wait, since)),
        ))
    }

    /// The change request of `terminal`'s worktree, or of the one
    /// `worktree` names. With `wait`, waits up to its duration until the
    /// revision passes its `since`, else the current one.
    pub(super) fn change_request(
        &self,
        terminal: &Terminal,
        worktree: Option<&str>,
        full: bool,
        wait: Option<(Duration, Option<u64>)>,
    ) -> Result<Value, (u16, String)> {
        let mut state = self.lock();
        let index = state.changes.find(terminal, worktree)?;
        let key = {
            let info = &state.changes.entries[index].info;
            (info.project, info.worktree.clone())
        };
        let new = state.changes.watch(key.clone(), wait.is_some());
        if new {
            self.new_watches
                .store(true, std::sync::atomic::Ordering::Release);
            drop(state);
            (self.wake)();
            state = self.lock();
        }
        let Some((wait, since)) = wait else {
            let index = state.changes.find(terminal, worktree)?;
            return Ok(state.changes.answer(index, full, None));
        };
        let at = |state: &super::State| {
            (state.changes.entries.iter())
                .position(|e| e.info.project == key.0 && e.info.worktree == key.1)
        };
        let since = since
            .unwrap_or_else(|| at(&state).map_or(0, |index| state.changes.entries[index].revision));
        let deadline = Instant::now() + wait;
        let answer = loop {
            let Some(index) = at(&state) else {
                break Err((404, format!("the worktree {} is gone", key.1.display())));
            };
            if state.changes.entries[index].revision > since {
                break Ok(state.changes.answer(index, full, Some(true)));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Ok(state.changes.answer(index, full, Some(false)));
            }
            state = self
                .changes_changed
                .wait_timeout(state, left)
                .unwrap_or_else(|poison| poison.into_inner())
                .0;
        };
        state.changes.unwatch(&key);
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, branch: &str, state: &str) -> ChangeEntry {
        ChangeEntry {
            project: 1,
            worktree: path.into(),
            branch: Some(branch.into()),
            report: json!({ "branch": branch, "state": state }),
            page: None,
        }
    }

    fn terminal(worktree: &str) -> Terminal {
        Terminal {
            project_id: Some(1),
            project: "app".into(),
            worktree: worktree.into(),
            machine: "local".into(),
            kind: "local",
        }
    }

    #[test]
    fn worktrees_are_found_by_path_branch_or_name() {
        let mut changes = Changes::default();
        assert_eq!(
            changes.find(&terminal("/src/app"), None).unwrap_err().0,
            503
        );
        changes.publish(vec![
            entry("/src/app", "main", "none"),
            entry("/src/fix", "fix/x", "open"),
        ]);
        let here = terminal("/src/fix/sub");
        assert_eq!(changes.find(&here, None), Ok(1));
        assert_eq!(changes.find(&here, Some("main")), Ok(0));
        assert_eq!(changes.find(&here, Some("/src/app")), Ok(0));
        assert_eq!(changes.find(&here, Some("fix")), Ok(1));
        let missing = changes.find(&here, Some("nope")).unwrap_err();
        assert_eq!(missing.0, 404);
        assert!(missing.1.contains("fix/x (/src/fix)"), "{}", missing.1);
        let elsewhere = Terminal {
            project_id: None,
            ..terminal("/tmp")
        };
        assert_eq!(changes.find(&elsewhere, None).unwrap_err().0, 404);
    }

    #[test]
    fn only_changed_reports_get_a_new_revision() {
        let mut changes = Changes::default();
        assert!(changes.publish(vec![
            entry("/src/app", "main", "none"),
            entry("/src/fix", "fix", "open"),
        ]));
        let revisions = |c: &Changes| c.entries.iter().map(|e| e.revision).collect::<Vec<_>>();
        assert_eq!(revisions(&changes), [1, 2]);
        assert!(!changes.publish(vec![
            entry("/src/app", "main", "none"),
            entry("/src/fix", "fix", "open"),
        ]));
        assert!(changes.publish(vec![
            entry("/src/app", "main", "none"),
            entry("/src/fix", "fix", "merged"),
        ]));
        assert_eq!(revisions(&changes), [1, 3]);
        // A worktree that goes is a change too.
        assert!(changes.publish(vec![entry("/src/app", "main", "none")]));
    }

    /// The tools answer from what the UI thread published, over HTTP and
    /// MCP, and a wait returns once a publish changes the entry.
    #[test]
    fn agents_read_and_wait_for_change_requests_over_the_socket() {
        use crate::control::call;
        let dir = tempfile::tempdir().unwrap();
        let server = crate::control::start(dir.path(), dir.path(), || {}).unwrap();
        let control = server.control.clone();
        let socket = control.socket().to_path_buf();
        let token = control.register(terminal("/src/fix"));
        let get = |path: &str| {
            let (status, body) = call(&socket, "GET", path, &token, b"");
            (status, serde_json::from_str::<Value>(&body).unwrap())
        };
        assert_eq!(get("/v1/change-request").0, 503);
        assert!(control.changes_wanted());
        control.publish_changes(vec![
            entry("/src/app", "main", "none"),
            entry("/src/fix", "fix", "open"),
        ]);
        assert!(!control.changes_wanted());
        let (status, answer) = get("/v1/change-request");
        assert_eq!(status, 200, "{answer}");
        assert_eq!(answer["state"], "open");
        assert_eq!(answer["revision"], 2);
        // The read made the worktree watched, once.
        assert!(control.has_new_watches());
        assert_eq!(control.take_new_watches(), [(1, PathBuf::from("/src/fix"))]);
        assert!(!control.has_new_watches());
        assert_eq!(get("/v1/change-request?worktree=main").1["state"], "none");
        let (status, answer) = get("/v1/change-request?wait=0");
        assert_eq!((status, &answer["changed"]), (200, &json!(false)));

        let waiting = {
            let (socket, token) = (socket.clone(), token.clone());
            std::thread::spawn(move || {
                call(
                    &socket,
                    "GET",
                    "/v1/change-request?wait=30&since=2",
                    &token,
                    b"",
                )
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        // Another worktree's change does not end the wait.
        control.publish_changes(vec![
            entry("/src/app", "main", "open"),
            entry("/src/fix", "fix", "open"),
        ]);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!waiting.is_finished());
        control.publish_changes(vec![
            entry("/src/app", "main", "open"),
            entry("/src/fix", "fix", "merged"),
        ]);
        let (status, body) = waiting.join().unwrap();
        assert_eq!(status, 200, "{body}");
        let answer: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (&answer["state"], &answer["changed"]),
            (&json!("merged"), &json!(true))
        );
        assert_eq!(answer["revision"], 4);

        let mcp = |message: Value| {
            let (_, body) = call(
                &socket,
                "POST",
                "/v1/mcp",
                &token,
                message.to_string().as_bytes(),
            );
            serde_json::from_str::<Value>(&body).unwrap()
        };
        let list = mcp(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
        let names: Vec<&str> = (list["result"]["tools"].as_array().unwrap().iter())
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"change_request") && names.contains(&"change_request_wait"));
        // Since an older revision, the wait answers at once.
        let called = mcp(json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "change_request_wait", "arguments": { "since": 2, "timeout": 5 } },
        }));
        let text = called["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("\"merged\"") && text.contains("\"changed\": true"),
            "{text}"
        );
        let refused = mcp(json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "change_request", "arguments": { "worktree": "nope" } },
        }));
        assert_eq!(refused["result"]["isError"], true);
        let (status, body) = call(&socket, "POST", "/v1/tools/change_request", &token, b"{}");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"merged\""), "{body}");
        let (_, tools) = get("/v1/tools");
        assert_eq!(tools["tools"][0]["name"], "change_request");
    }

    #[test]
    fn watches_are_fresh_once_and_linger() {
        let mut changes = Changes::default();
        let key = (1, PathBuf::from("/src/fix"));
        assert!(changes.watch(key.clone(), true));
        assert!(!changes.watch(key.clone(), false));
        assert_eq!(changes.take_fresh(), std::slice::from_ref(&key));
        assert!(changes.take_fresh().is_empty());
        changes.unwatch(&key);
        assert_eq!(changes.watched(), std::slice::from_ref(&key));
        changes.watched.get_mut(&key).unwrap().until = Instant::now();
        assert!(changes.watched().is_empty());
        assert!(changes.watch(key, false));
    }
}
