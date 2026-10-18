//! A change request's page for the explore panel: its status, checks and
//! reviews, and its discussion, with every code comment placed on the
//! worktree's current head, whatever happened to the branch since the
//! comment was written. [`Rendered`] keeps the parts `ui/change.rs` shows
//! as widgets, and the same page as Markdown, one section per list entry,
//! for the discussion view and agents.
//!
//! Forges keep a comment on the commit it was written on. After a rebase
//! or an amend and a force push, GitHub marks it outdated, GitLab keeps
//! the old version, and Gitea, Forgejo and Bitbucket keep the old line
//! number. This module follows the line itself, in the same way for every
//! forge:
//!
//! 1. When the comment's commit is in the repository, `git diff -M -U0`
//!    from it to `HEAD` carries the line through renames and edits: it
//!    stays, moves, or its hunk changed it.
//! 2. When the commit is gone (force-pushed away by someone else, never
//!    fetched), the commented line's text from the forge's diff hunk is
//!    looked up in the file at `HEAD`, nearest to the old line number.
//!
//! The pushes come from the forge where it reports them and from the
//! branch's reflog, which keeps every local head, rebases and amends
//! included. A push that rewrote the branch shows `git range-diff`'s
//! summary of which commits stayed, changed, came or went.
//!
//! All of it is cold and runs on a thread of its own: one `cat-file
//! --batch-check` for every commit, then one git call per distinct
//! commit, file and push, up to [`WORKERS`] at a time, since each costs a
//! channel round trip on a remote project.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;

use cmdterm_env::{EnvCommand, Environment};

use crate::ext::{BranchState, Check, CheckState, Comment, Discussion, Thread, Verdict};
use crate::forge::{Pipeline, PipelineStatus};
use crate::i18n::Strings;
use crate::vcs::{KnownRepo, Repository};

/// Lines of code shown around a comment's line.
const CONTEXT: u64 = 2;
/// Reflog entries looked at, newest first.
const REFLOG_LIMIT: usize = 50;

/// One hunk of a `-U0` diff: the old lines it replaces and the new ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u64,
    pub old_len: u64,
    pub new_start: u64,
    pub new_len: u64,
}

/// What a `-U0` diff did to one file of the old side.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDiff {
    /// Its path on the new side; `None` when it was deleted.
    pub new_path: Option<String>,
    pub hunks: Vec<Hunk>,
}

/// Reads `git diff -M -U0 --no-color` output: per old path, where the file
/// went and its hunks. Files the diff does not name did not change.
pub fn parse_diff(diff: &str) -> HashMap<String, FileDiff> {
    let mut files: HashMap<String, FileDiff> = HashMap::new();
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `a/old b/new`; the `---`, `rename from` and `deleted` lines
            // that follow say it precisely.
            let old = rest
                .strip_prefix("a/")
                .and_then(|r| r.split(" b/").next())
                .unwrap_or(rest)
                .to_owned();
            let new = rest.rsplit_once(" b/").map(|(_, n)| n.to_owned());
            files.insert(
                old.clone(),
                FileDiff {
                    new_path: new,
                    hunks: Vec::new(),
                },
            );
            current = Some(old);
        } else if let Some(old) = &current {
            if let Some(from) = line.strip_prefix("rename from ") {
                if let Some(diff) = files.remove(old) {
                    files.insert(from.to_owned(), diff);
                    current = Some(from.to_owned());
                }
            } else if let Some(to) = line.strip_prefix("rename to ") {
                if let Some(diff) = files.get_mut(old) {
                    diff.new_path = Some(to.to_owned());
                }
            } else if line.starts_with("deleted file mode") {
                if let Some(diff) = files.get_mut(old) {
                    diff.new_path = None;
                }
            } else if let Some(hunk) = line.strip_prefix("@@ ").and_then(parse_hunk)
                && let Some(diff) = files.get_mut(old)
            {
                diff.hunks.push(hunk);
            }
        }
    }
    files
}

/// `-a[,b] +c[,d] @@ ...`.
fn parse_hunk(text: &str) -> Option<Hunk> {
    let mut parts = text.split(' ');
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let range = |part: &str| -> Option<(u64, u64)> {
        match part.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((part.parse().ok()?, 1)),
        }
    };
    let (old_start, old_len) = range(old)?;
    let (new_start, new_len) = range(new)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
    })
}

/// Where a line went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapped {
    /// The same line number, untouched.
    Same(u64),
    /// Untouched, at another line number.
    Moved(u64),
    /// A hunk replaced it; the number is where the replacement starts.
    Changed(u64),
}

/// Carries old line `line` through `hunks`.
pub fn map_line(hunks: &[Hunk], line: u64) -> Mapped {
    let mut shift: i64 = 0;
    for hunk in hunks {
        // A pure insertion (`-a,0`) goes after old line `a`.
        let (start, end) = if hunk.old_len == 0 {
            (hunk.old_start + 1, hunk.old_start + 1)
        } else {
            (hunk.old_start, hunk.old_start + hunk.old_len)
        };
        if line < start {
            break;
        }
        if line < end {
            return Mapped::Changed(hunk.new_start.max(1));
        }
        shift +=
            i64::try_from(hunk.new_len).unwrap_or(0) - i64::try_from(hunk.old_len).unwrap_or(0);
    }
    let new = u64::try_from(i64::try_from(line).unwrap_or(0) + shift).unwrap_or(1);
    if new == line {
        Mapped::Same(line)
    } else {
        Mapped::Moved(new)
    }
}

/// The commented line of a forge's diff hunk: its last line, without the
/// `+`, `-` or space in front.
pub fn hunk_anchor(hunk: &str) -> Option<&str> {
    let last = hunk.lines().rev().find(|l| !l.starts_with('\\'))?;
    let text = last.get(1..).unwrap_or("");
    (!text.trim().is_empty()).then_some(text)
}

/// The 1-based line of `text` whose content equals `anchor` (ignoring the
/// indentation around it) nearest to `near`.
pub fn find_line(text: &str, anchor: &str, near: u64) -> Option<u64> {
    let anchor = anchor.trim();
    (1_u64..)
        .zip(text.lines())
        .filter(|(_, line)| line.trim() == anchor)
        .map(|(number, _)| number)
        .min_by_key(|number| number.abs_diff(near))
}

/// Where a thread sits on the current head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// A general comment on the file, or one without a line.
    File,
    /// Followed through the diff from its commit.
    Followed(Mapped),
    /// Its commit is not in the repository; found by its line's text.
    Found(u64),
    /// The forge names neither a commit nor the line's text; taken as it
    /// is on the head.
    Given(u64),
    /// The file is gone on the current head.
    FileGone,
    /// Neither its commit nor its line's text could be found.
    Lost,
    /// On a removed line of the base, which no head has.
    Removed,
}

impl Placement {
    /// The line the thread shows code around.
    fn line(&self) -> Option<u64> {
        match self {
            Self::Followed(Mapped::Same(l) | Mapped::Moved(l) | Mapped::Changed(l))
            | Self::Found(l)
            | Self::Given(l) => Some(*l),
            _ => None,
        }
    }
}

/// Runs git in a worktree and keeps what it learned per commit and path.
struct Git<'a> {
    /// `None` when `cwd` is in no repository.
    repo: Option<Repository>,
    cwd: &'a Path,
    present: HashMap<String, bool>,
    diffs: HashMap<String, Option<HashMap<String, FileDiff>>>,
    files: HashMap<String, Option<String>>,
}

impl<'a> Git<'a> {
    /// Reads the repository with its metadata where `known` says, see
    /// [`crate::vcs::open`].
    fn new(env: &Environment, cwd: &'a Path, known: Option<&KnownRepo>) -> Self {
        Self {
            repo: crate::vcs::open(env, cwd, false, known).ok(),
            cwd,
            present: HashMap::new(),
            diffs: HashMap::new(),
            files: HashMap::new(),
        }
    }

    /// git with the options of the app's own queries, see
    /// [`Repository::git_command`].
    fn command(&self) -> Option<EnvCommand> {
        self.repo.as_ref()?.git_command(self.cwd).ok()
    }

    fn run(&self, args: &[&str]) -> Option<String> {
        let output = self.command()?.args(args.iter().copied()).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn has_commit(&mut self, commit: &str) -> bool {
        if !valid_commit(commit) {
            return false;
        }
        if let Some(known) = self.present.get(commit) {
            return *known;
        }
        let spec = format!("{commit}^{{commit}}");
        let found = self.run(&["cat-file", "-e", &spec]).is_some();
        self.present.insert(commit.to_owned(), found);
        found
    }

    /// The diff from `commit` to `HEAD`, per old path.
    fn diff(&mut self, commit: &str) -> Option<&HashMap<String, FileDiff>> {
        if !self.diffs.contains_key(commit) {
            let diff = self.run_diff(commit);
            self.diffs.insert(commit.to_owned(), diff);
        }
        self.diffs.get(commit)?.as_ref()
    }

    fn run_diff(&self, commit: &str) -> Option<HashMap<String, FileDiff>> {
        self.run(&[
            "diff",
            "-M",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            commit,
            "HEAD",
        ])
        .map(|text| parse_diff(&text))
    }

    /// `path` at `HEAD`.
    fn file(&mut self, path: &str) -> Option<&str> {
        if !self.files.contains_key(path) {
            let text = self.run_file(path);
            self.files.insert(path.to_owned(), text);
        }
        self.files.get(path)?.as_deref()
    }

    fn run_file(&self, path: &str) -> Option<String> {
        let spec = format!("HEAD:{path}");
        self.run(&["show", "--no-color", &spec])
    }

    /// The path `thread` has on `HEAD`, from the diffs fetched already.
    fn head_path(&self, thread: &Thread) -> Option<String> {
        let diffs = self.diffs.get(thread.commit.as_deref()?)?.as_ref()?;
        diffs.get(&thread.path)?.new_path.clone()
    }

    /// Where `thread` sits on `HEAD`, with the path it has there.
    fn place(&mut self, thread: &Thread) -> (Placement, String) {
        let path = thread.path.clone();
        let Some(line) = thread.line else {
            return (Placement::File, path);
        };
        if thread.old_side {
            return (Placement::Removed, path);
        }
        if let Some(commit) = thread.commit.as_deref()
            && self.has_commit(commit)
        {
            let Some(diffs) = self.diff(commit) else {
                return (Placement::Lost, path);
            };
            return match diffs.get(&path) {
                None => (Placement::Followed(Mapped::Same(line)), path),
                Some(FileDiff { new_path: None, .. }) => (Placement::FileGone, path),
                Some(FileDiff {
                    new_path: Some(new),
                    hunks,
                }) => (Placement::Followed(map_line(hunks, line)), new.clone()),
            };
        }
        let anchor = thread
            .hunk
            .as_deref()
            .and_then(hunk_anchor)
            .map(str::to_owned);
        if thread.commit.is_none() && anchor.is_none() {
            return match self.file(&path) {
                Some(_) => (Placement::Given(line), path),
                None => (Placement::FileGone, path),
            };
        }
        match self.file(&path) {
            None => (Placement::FileGone, path),
            Some(text) => match anchor.and_then(|anchor| find_line(text, &anchor, line)) {
                Some(found) => (Placement::Found(found), path),
                None => (Placement::Lost, path),
            },
        }
    }

    /// The local heads of `branch`, oldest first: commit, time, reflog
    /// message.
    fn reflog(&self, branch: &str) -> Vec<(String, i64, String)> {
        let reference = format!("refs/heads/{branch}");
        let limit = format!("-n{REFLOG_LIMIT}");
        let Some(text) = self.run(&[
            "reflog",
            "show",
            "--no-abbrev",
            &limit,
            "--format=%H%x09%ct%x09%gs",
            &reference,
        ]) else {
            return Vec::new();
        };
        let mut entries: Vec<(String, i64, String)> = text
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                let commit = parts.next()?.to_owned();
                let time = parts.next()?.parse().ok()?;
                Some((commit, time, parts.next().unwrap_or("").to_owned()))
            })
            .collect();
        entries.reverse();
        entries
    }

    /// Looks up which of `commits` the repository has, in one
    /// `cat-file --batch-check`, so [`Self::has_commit`] needs no call.
    fn prefetch_commits<'c>(&mut self, commits: impl IntoIterator<Item = &'c str>) {
        let mut asked: Vec<&str> = Vec::new();
        for commit in commits {
            if valid_commit(commit)
                && !self.present.contains_key(commit)
                && !asked.contains(&commit)
            {
                asked.push(commit);
            }
        }
        if asked.is_empty() {
            return;
        }
        let mut input = String::new();
        for commit in &asked {
            let _ = writeln!(input, "{commit}^{{commit}}");
        }
        let Some(command) = self.command() else {
            return;
        };
        let batch = command.args(["cat-file", "--batch-check"]);
        let Ok(output) = batch.stdout_with_input(input.as_bytes()) else {
            return;
        };
        // One line per name: `<oid> commit <size>`, or `<name> missing`.
        let text = String::from_utf8_lossy(&output);
        for (commit, line) in asked.iter().zip(text.lines()) {
            let found = line.split(' ').nth(1) == Some("commit");
            self.present.insert((*commit).to_owned(), found);
        }
    }

    /// Runs the diffs from each of `commits` to `HEAD` at the same time.
    fn prefetch_diffs<'c>(&mut self, commits: impl IntoIterator<Item = &'c str>) {
        let mut missing: Vec<&str> = Vec::new();
        for commit in commits {
            if !self.diffs.contains_key(commit)
                && self.has_commit(commit)
                && !missing.contains(&commit)
            {
                missing.push(commit);
            }
        }
        let diffs = parallel(&missing, |commit| self.run_diff(commit));
        for (commit, diff) in missing.into_iter().zip(diffs) {
            self.diffs.insert(commit.to_owned(), diff);
        }
    }

    /// Reads each of `paths` at `HEAD` at the same time.
    fn prefetch_files<'p>(&mut self, paths: impl IntoIterator<Item = &'p str>) {
        let mut missing: Vec<&str> = Vec::new();
        for path in paths {
            if !self.files.contains_key(path) && !missing.contains(&path) {
                missing.push(path);
            }
        }
        let files = parallel(&missing, |path| self.run_file(path));
        for (path, text) in missing.into_iter().zip(files) {
            self.files.insert(path.to_owned(), text);
        }
    }

    /// Whether `old` is an ancestor of `new`. Both must be present.
    fn is_ancestor(&self, old: &str, new: &str) -> bool {
        self.command().is_some_and(|command| {
            command
                .args(["merge-base", "--is-ancestor", old, new])
                .output()
                .is_ok_and(|output| output.status.success())
        })
    }

    /// `git range-diff`'s summary lines for a rewrite of `old` into `new`.
    fn range_diff(&self, old: &str, new: &str) -> Vec<String> {
        let range = format!("{old}...{new}");
        let Some(text) = self.run(&["range-diff", "--no-color", &range]) else {
            return Vec::new();
        };
        // The pairs start in the first column; patches are indented.
        text.lines()
            .filter(|line| !line.is_empty() && !line.starts_with(' '))
            .map(str::to_owned)
            .collect()
    }
}

/// Whether `commit` is a full or short hex object name, so it never reads
/// as an option or a revision expression.
fn valid_commit(commit: &str) -> bool {
    (4..=64).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One head in the branch's history.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Head {
    commit: String,
    /// ISO 8601 or empty.
    at: String,
    /// Why it changed: the reflog message, or the forge's word.
    what: String,
    forced: bool,
}

/// The pushes of the forge and the local reflog in one timeline, oldest
/// first, without repeats.
fn timeline(git: &mut Git, tr: &Strings, discussion: &Discussion, branch: &str) -> Vec<Head> {
    let mut heads: Vec<Head> = Vec::new();
    for push in &discussion.pushes {
        if !heads.iter().any(|h| h.commit == push.commit) {
            heads.push(Head {
                commit: push.commit.clone(),
                at: push.at.clone().unwrap_or_default(),
                what: if push.force {
                    tr.discussion_force_pushed
                } else {
                    tr.discussion_pushed
                }
                .to_owned(),
                forced: push.force,
            });
        }
    }
    for (commit, time, message) in git.reflog(branch) {
        if heads.iter().any(|h| h.commit == commit) {
            continue;
        }
        let forced = ["rebase", "amend", "reset", "forced"]
            .iter()
            .any(|word| message.contains(word));
        heads.push(Head {
            commit,
            at: iso(time),
            what: message,
            forced,
        });
    }
    heads.sort_by(|a, b| a.at.cmp(&b.at));
    heads
}

/// Seconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn iso(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// `2026-10-28T14:02:11Z` as `2026-10-28 14:02`.
pub fn short_time(at: &str) -> String {
    let at = at.replace('T', " ");
    at.get(..16).unwrap_or(&at).to_owned()
}

fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

/// The language a code fence of `path` names, from its extension.
fn fence_language(path: &str) -> &str {
    path.rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// A fence that the text does not close early.
fn fence_for(text: &str) -> String {
    let mut fence = String::from("```");
    while text.contains(&fence) {
        fence.push('`');
    }
    fence
}

fn push_comments(out: &mut String, comments: &[Comment]) {
    for comment in comments {
        let _ = write!(out, "\n**{}**", comment.author);
        if let Some(at) = &comment.at {
            let _ = write!(out, " · {}", short_time(at));
        }
        out.push_str("\n\n");
        out.push_str(comment.body.trim());
        out.push('\n');
    }
}

/// What one entry of the change request page's list stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SectionKind {
    Overview,
    /// With the worst state of the checks.
    Checks(CheckState),
    Pushes,
    Conversation,
    Thread {
        resolved: bool,
    },
}

/// One entry of the change request page: its list label and its Markdown.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Section {
    pub label: String,
    pub kind: SectionKind,
    pub markdown: String,
}

/// One head in the push timeline, ready to show.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PushEntry {
    /// `2026-10-28 14:02`, or empty.
    pub at: String,
    /// The head, shortened.
    pub commit: String,
    /// The head it replaced, shortened, when the push rewrote the branch.
    pub rewrote: Option<String>,
    /// Why it changed: the reflog message, or the forge's word.
    pub what: String,
    /// `git range-diff`'s summary of a rewrite.
    pub range_diff: Vec<String>,
}

/// A rendered change request page: its parts for the explore panel's view,
/// and the same page as Markdown, one section per entry of its list.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rendered {
    /// `!12 Title`, for tab and panel titles.
    pub title: String,
    /// The line above the page: title, state and checks in short.
    pub header: String,
    pub sections: Vec<Section>,
    pub number: u64,
    /// The worktree's branch.
    pub branch: String,
    /// What the forge said. Its checks, pushes and threads are shown from
    /// the fields below.
    pub discussion: Discussion,
    /// The worst first.
    pub checks: Vec<Check>,
    /// Oldest first.
    pub pushes: Vec<PushEntry>,
    /// Open threads first, in the order of their sections.
    pub threads: Vec<PlacedThread>,
}

impl Rendered {
    /// The whole page as one Markdown document.
    pub fn markdown(&self) -> String {
        let mut out = String::new();
        for section in &self.sections {
            out.push_str(&section.markdown);
            out.push('\n');
        }
        out
    }

    /// The thread section `index` shows, if it shows one. Thread sections
    /// come last, in the order of `threads`.
    pub fn thread(&self, index: usize) -> Option<&PlacedThread> {
        let first = self.sections.len().checked_sub(self.threads.len())?;
        self.threads.get(index.checked_sub(first)?)
    }

    /// Passed, failed, and running or waiting checks.
    pub fn tally(&self) -> (usize, usize, usize) {
        tally(&self.checks)
    }
}

/// What a change request page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    /// Asked the forge; nothing to show yet.
    Loading,
    /// The worktree's repository, branch or change request is not known
    /// yet; asked again after the next check.
    Waiting,
    /// The worktree's branch has no change request, or no branch is
    /// checked out.
    NoChange,
    Unsupported,
    Failed(String),
    Ready(Arc<Rendered>),
}

/// The mark of a check state, one cell wide.
pub fn check_mark(state: CheckState) -> char {
    match state {
        CheckState::Success => '✓',
        CheckState::Failed => '✗',
        CheckState::Running => '●',
        CheckState::Pending => '○',
        CheckState::Skipped => '–',
        CheckState::Canceled => '⊘',
    }
}

/// The checks the forge reported, else the pipeline the built-in client
/// found for the branch.
fn checks(tr: &Strings, discussion: &Discussion, pipeline: Option<&Pipeline>) -> Vec<Check> {
    let mut checks = discussion.checks.clone();
    if checks.is_empty()
        && let Some(pipeline) = pipeline
    {
        let state = match pipeline.status {
            PipelineStatus::Success => CheckState::Success,
            PipelineStatus::Failed => CheckState::Failed,
            PipelineStatus::Running => CheckState::Running,
            PipelineStatus::Canceled => CheckState::Canceled,
            PipelineStatus::Pending | PipelineStatus::Unknown => CheckState::Pending,
        };
        checks.push(Check {
            name: tr.change_pipeline.to_owned(),
            state,
            url: pipeline.url.clone(),
        });
    }
    // The worst first, otherwise in the forge's order.
    checks.sort_by_key(|check| std::cmp::Reverse(check.state));
    checks
}

/// Passed, failed, and running or waiting checks. Skipped and canceled
/// ones count as neither.
fn tally(checks: &[Check]) -> (usize, usize, usize) {
    let count = |states: &[CheckState]| {
        checks
            .iter()
            .filter(|check| states.contains(&check.state))
            .count()
    };
    (
        count(&[CheckState::Success]),
        count(&[CheckState::Failed]),
        count(&[CheckState::Running, CheckState::Pending]),
    )
}

/// The overview: state, branches, reviews, checks and the description.
fn overview(
    tr: &Strings,
    branch: &str,
    number: u64,
    discussion: &Discussion,
    checks: &[Check],
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# !{number} {}\n", discussion.title);
    let state = match discussion.state {
        BranchState::Open => tr.change_open,
        BranchState::Merged => tr.change_merged,
        BranchState::Closed => tr.change_closed,
        BranchState::None => "",
    };
    let mut meta = vec![match &discussion.target {
        Some(target) => format!("`{branch}` → `{target}`"),
        None => format!("`{branch}`"),
    }];
    if !state.is_empty() {
        meta.push(state.to_owned());
    }
    if discussion.draft {
        meta.push(tr.change_draft.to_owned());
    }
    if let Some(author) = &discussion.author {
        meta.push((tr.discussion_by)(author));
    }
    if let Some(url) = &discussion.url {
        meta.push(format!("[{}]({url})", tr.discussion_on_forge));
    }
    let _ = writeln!(out, "{}\n", meta.join(" · "));
    if discussion.conflicts {
        let _ = writeln!(out, "**{}**\n", tr.change_conflicts);
    }
    if !checks.is_empty() {
        let (passed, failed, waiting) = tally(checks);
        let worst = checks.iter().map(|c| c.state).max().unwrap_or_default();
        let _ = writeln!(
            out,
            "{} {}: {}\n",
            check_mark(worst),
            tr.change_checks,
            (tr.change_checks_summary)(passed, failed, waiting)
        );
    }
    if !discussion.reviews.is_empty() {
        let _ = writeln!(out, "{}:\n", tr.change_reviews);
        for review in &discussion.reviews {
            let (mark, verdict) = match review.verdict {
                Verdict::Approved => ('✓', tr.change_approved),
                Verdict::Changes => ('✗', tr.change_requested_changes),
                Verdict::Commented => ('●', tr.change_commented),
                Verdict::Pending => ('○', tr.change_review_pending),
            };
            let _ = writeln!(out, "- {mark} **{}** {verdict}", review.author);
        }
        out.push('\n');
    }
    let open = discussion.threads.iter().filter(|t| !t.resolved).count();
    if !discussion.threads.is_empty() {
        let resolved = discussion.threads.len() - open;
        let _ = writeln!(out, "{}\n", (tr.change_threads)(open, resolved));
    }
    if let Some(body) = discussion.body.as_deref().filter(|b| !b.trim().is_empty()) {
        let _ = writeln!(out, "---\n\n{}\n", body.trim());
    }
    out
}

/// A code thread placed on the worktree's head.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlacedThread {
    /// Its path on the head.
    pub path: String,
    /// Its line on the head, when it has one.
    pub line: Option<u64>,
    /// Where the line went since the comment was written; empty when there
    /// is nothing to say.
    pub note: String,
    pub resolved: bool,
    pub outdated: bool,
    /// The code around `line` on the head, by line number.
    pub code: Vec<(u64, String)>,
    /// The forge's own hunk, shown when `code` is empty.
    pub hunk: Option<String>,
    pub comments: Vec<Comment>,
}

impl PlacedThread {
    /// `src/lib.rs:12`, or the path alone.
    pub fn label(&self) -> String {
        match self.line {
            Some(line) => format!("{}:{line}", self.path),
            None => self.path.clone(),
        }
    }
}

/// Places `thread` on the head and takes the code around its line there.
fn place_thread(tr: &Strings, git: &mut Git, thread: &Thread) -> PlacedThread {
    let (placement, path) = git.place(thread);
    let line = placement.line();
    let since = thread.commit.as_deref().map_or("", short);
    let old = thread.line.unwrap_or(0);
    let note = match &placement {
        Placement::File | Placement::Given(_) => String::new(),
        Placement::Followed(Mapped::Same(_)) => (tr.discussion_unchanged)(since),
        Placement::Followed(Mapped::Moved(_)) => (tr.discussion_moved)(old, since),
        Placement::Followed(Mapped::Changed(_)) => (tr.discussion_changed)(old, since),
        Placement::Found(_) => (tr.discussion_found)(old),
        Placement::FileGone => (tr.discussion_file_gone)(since),
        Placement::Lost => (tr.discussion_line_gone)(old, since),
        Placement::Removed => (tr.discussion_removed)(old),
    };
    let mut code = Vec::new();
    if let Some(line) = line
        && let Some(text) = git.file(&path)
    {
        let first = line.saturating_sub(CONTEXT).max(1);
        for (number, text) in (1_u64..).zip(text.lines()).skip_while(|(n, _)| *n < first) {
            if number > line + CONTEXT {
                break;
            }
            code.push((number, text.to_owned()));
        }
    }
    PlacedThread {
        path,
        line,
        note,
        resolved: thread.resolved,
        outdated: thread.outdated,
        code,
        hunk: thread.hunk.clone(),
        comments: thread.comments.clone(),
    }
}

/// The page of a thread: where it sits on the head, the code around its
/// line there, and its comments.
fn thread_markdown(tr: &Strings, thread: &PlacedThread) -> String {
    let mut out = String::new();
    match thread.line {
        Some(line) => {
            let _ = writeln!(out, "### {}\n", (tr.discussion_line)(&thread.path, line));
        }
        None => {
            let _ = writeln!(out, "### `{}`\n", thread.path);
        }
    }
    let mut flags = Vec::new();
    if thread.resolved {
        flags.push(tr.discussion_resolved);
    }
    if thread.outdated {
        flags.push(tr.discussion_outdated);
    }
    let note = &thread.note;
    if !note.is_empty() || !flags.is_empty() {
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" ({})", flags.join(", "))
        };
        let space = if note.is_empty() { "" } else { " " };
        let flags = flags.trim_start();
        let _ = writeln!(out, "{note}{space}{flags}\n");
    }
    if let Some(line) = thread.line.filter(|_| !thread.code.is_empty()) {
        let mut code = String::new();
        for (number, text) in &thread.code {
            let mark = if *number == line { '>' } else { ' ' };
            let _ = writeln!(code, "{mark}{number:>5}  {text}");
        }
        let fence = fence_for(&code);
        let _ = writeln!(
            out,
            "{fence}{}\n{code}{fence}",
            fence_language(&thread.path)
        );
    } else if let Some(hunk) = &thread.hunk {
        // Nothing to show on the head: the forge's own hunk.
        let fence = fence_for(hunk);
        let _ = writeln!(out, "{fence}diff\n{}\n{fence}", hunk.trim_end());
    }
    push_comments(&mut out, &thread.comments);
    out
}

/// The change request page, with its threads placed on the head of the
/// worktree at `cwd`, whose project's recorded git repository is `known`.
/// `pipeline` is the built-in client's pipeline of the branch, shown when
/// the forge reports no checks. Runs git; call it off the UI thread.
#[allow(clippy::too_many_arguments)]
pub fn render(
    tr: &Strings,
    env: &Environment,
    cwd: &Path,
    known: Option<&KnownRepo>,
    branch: &str,
    number: u64,
    discussion: &Discussion,
    pipeline: Option<&Pipeline>,
) -> Rendered {
    let mut git = Git::new(env, cwd, known);
    let title = format!("!{number} {}", discussion.title);
    let checks = checks(tr, discussion, pipeline);
    let mut sections = vec![Section {
        label: tr.change_overview.to_owned(),
        kind: SectionKind::Overview,
        markdown: overview(tr, branch, number, discussion, &checks),
    }];
    let mut header = title.clone();
    let state = match discussion.state {
        BranchState::Open => tr.change_open,
        BranchState::Merged => tr.change_merged,
        BranchState::Closed => tr.change_closed,
        BranchState::None => "",
    };
    if !state.is_empty() {
        let _ = write!(header, " · {state}");
    }
    if discussion.draft {
        let _ = write!(header, " · {}", tr.change_draft);
    }

    if !checks.is_empty() {
        let (passed, ..) = tally(&checks);
        let worst = checks[0].state;
        let label = (tr.change_checks_label)(passed, checks.len());
        let _ = write!(header, " · {} {label}", check_mark(worst));
        let mut out = String::new();
        let _ = writeln!(out, "## {}\n", tr.change_checks);
        for check in &checks {
            let mark = check_mark(check.state);
            match &check.url {
                Some(url) => {
                    let _ = writeln!(out, "- {mark} [{}]({url})", check.name);
                }
                None => {
                    let _ = writeln!(out, "- {mark} {}", check.name);
                }
            }
        }
        sections.push(Section {
            label,
            kind: SectionKind::Checks(worst),
            markdown: out,
        });
    }

    let heads = timeline(&mut git, tr, discussion, branch);
    git.prefetch_commits(
        (heads.iter().map(|h| h.commit.as_str())).chain(
            discussion
                .threads
                .iter()
                .filter_map(|t| t.commit.as_deref()),
        ),
    );
    // Each head against the one before it: whether it rewrote the branch,
    // and then what range-diff says of it, with the git calls in parallel.
    let pairs: Vec<(&Head, &Head)> = heads.iter().zip(heads.iter().skip(1)).collect();
    let rewrote = parallel(&pairs, |(old, new)| {
        new.forced
            || (git.present.get(&old.commit) == Some(&true)
                && git.present.get(&new.commit) == Some(&true)
                && !git.is_ancestor(&old.commit, &new.commit))
    });
    let rewrites: Vec<(&Head, &Head)> = (pairs.iter().zip(&rewrote))
        .filter(|(_, rewrote)| **rewrote)
        .map(|(pair, _)| *pair)
        .collect();
    let mut range_diffs = parallel(&rewrites, |(old, new)| {
        git.range_diff(&old.commit, &new.commit)
    })
    .into_iter();
    let mut pushes = Vec::with_capacity(heads.len());
    for (index, head) in heads.iter().enumerate() {
        let previous = index.checked_sub(1).map(|i| &heads[i]);
        let rewrote = previous.filter(|_| rewrote[index - 1]);
        pushes.push(PushEntry {
            at: if head.at.is_empty() {
                String::new()
            } else {
                short_time(&head.at)
            },
            commit: short(&head.commit).to_owned(),
            rewrote: rewrote.map(|p| short(&p.commit).to_owned()),
            what: head.what.clone(),
            range_diff: rewrote.and_then(|_| range_diffs.next()).unwrap_or_default(),
        });
    }
    if !pushes.is_empty() {
        sections.push(Section {
            label: format!("{} ({})", tr.discussion_pushes, pushes.len()),
            kind: SectionKind::Pushes,
            markdown: pushes_markdown(tr, &pushes),
        });
    }

    if !discussion.comments.is_empty() {
        let mut out = String::new();
        let _ = writeln!(out, "## {}", tr.discussion_conversation);
        push_comments(&mut out, &discussion.comments);
        sections.push(Section {
            label: format!(
                "{} ({})",
                tr.discussion_conversation,
                discussion.comments.len()
            ),
            kind: SectionKind::Conversation,
            markdown: out,
        });
    }

    // Open threads first, in the order the forge gave.
    let mut threads: Vec<&Thread> = discussion.threads.iter().collect();
    threads.sort_by_key(|t| t.resolved);
    git.prefetch_diffs(
        (threads.iter())
            .filter(|t| t.line.is_some() && !t.old_side)
            .filter_map(|t| t.commit.as_deref()),
    );
    let moved: Vec<String> = threads.iter().filter_map(|t| git.head_path(t)).collect();
    git.prefetch_files(
        (threads.iter())
            .map(|t| t.path.as_str())
            .chain(moved.iter().map(String::as_str)),
    );
    let threads: Vec<PlacedThread> = threads
        .into_iter()
        .map(|thread| place_thread(tr, &mut git, thread))
        .collect();
    for thread in &threads {
        sections.push(Section {
            label: thread.label(),
            kind: SectionKind::Thread {
                resolved: thread.resolved,
            },
            markdown: thread_markdown(tr, thread),
        });
    }
    Rendered {
        title,
        header,
        sections,
        number,
        branch: branch.to_owned(),
        discussion: discussion.clone(),
        checks,
        pushes,
        threads,
    }
}

/// Git calls that run at the same time while a page renders.
const WORKERS: usize = 8;

/// `f` of every item, in order, on up to [`WORKERS`] threads.
fn parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if items.len() < 2 {
        return items.iter().map(f).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut results: Vec<(usize, R)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WORKERS.min(items.len()))
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(item) = items.get(index) else { break };
                        done.push((index, f(item)));
                    }
                    done
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

/// The push timeline as a Markdown list.
fn pushes_markdown(tr: &Strings, pushes: &[PushEntry]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## {}\n", tr.discussion_pushes);
    for push in pushes {
        let at = if push.at.is_empty() {
            String::new()
        } else {
            format!("{} · ", push.at)
        };
        match &push.rewrote {
            Some(old) => {
                let _ = writeln!(
                    out,
                    "- {at}**{}** `{old}` → `{}` ({})",
                    tr.discussion_rewritten, push.commit, push.what
                );
                if !push.range_diff.is_empty() {
                    out.push_str("\n  ```\n");
                    for line in &push.range_diff {
                        let _ = writeln!(out, "  {line}");
                    }
                    out.push_str("  ```\n\n");
                }
            }
            None => {
                let _ = writeln!(out, "- {at}`{}` ({})", push.commit, push.what);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_name_renames_deletions_and_hunks() {
        let diff = "\
diff --git a/src/a.rs b/src/b.rs
similarity index 90%
rename from src/a.rs
rename to src/b.rs
index 1..2 100644
--- a/src/a.rs
+++ b/src/b.rs
@@ -3,0 +4,2 @@ fn x()
+one
+two
@@ -10 +12 @@
-old
+new
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
--- a/gone.rs
+++ /dev/null
@@ -1,3 +0,0 @@
";
        let files = parse_diff(diff);
        let a = &files["src/a.rs"];
        assert_eq!(a.new_path.as_deref(), Some("src/b.rs"));
        assert_eq!(a.hunks.len(), 2);
        assert_eq!(files["gone.rs"].new_path, None);
    }

    #[test]
    fn lines_follow_insertions_and_stop_at_changes() {
        let hunks = [
            // Two lines inserted after line 3.
            Hunk {
                old_start: 3,
                old_len: 0,
                new_start: 4,
                new_len: 2,
            },
            // Line 10 replaced.
            Hunk {
                old_start: 10,
                old_len: 1,
                new_start: 12,
                new_len: 1,
            },
        ];
        assert_eq!(map_line(&hunks, 2), Mapped::Same(2));
        assert_eq!(map_line(&hunks, 3), Mapped::Same(3));
        assert_eq!(map_line(&hunks, 4), Mapped::Moved(6));
        assert_eq!(map_line(&hunks, 10), Mapped::Changed(12));
        assert_eq!(map_line(&hunks, 11), Mapped::Moved(13));
        // Lines removed above move a line up.
        let removed = [Hunk {
            old_start: 1,
            old_len: 3,
            new_start: 0,
            new_len: 0,
        }];
        assert_eq!(map_line(&removed, 5), Mapped::Moved(2));
    }

    #[test]
    fn a_lost_commit_is_found_by_its_line_text_nearest_the_old_line() {
        let hunk = "@@ -1,2 +1,3 @@\n fn a() {}\n+    let x = 1;";
        assert_eq!(hunk_anchor(hunk), Some("    let x = 1;"));
        let text = "let x = 1;\nfn a() {}\n\n\n\n  let x = 1;\n";
        assert_eq!(find_line(text, "    let x = 1;", 5), Some(6));
        assert_eq!(find_line(text, "    let x = 1;", 1), Some(1));
        assert_eq!(find_line(text, "missing", 1), None);
    }

    #[test]
    fn the_page_puts_failed_checks_first_and_falls_back_to_the_pipeline() {
        use crate::ext::Review;
        let tr = &crate::i18n::EN;
        let dir = tempfile::tempdir().unwrap();
        let check = |name: &str, state| Check {
            name: name.into(),
            state,
            url: None,
        };
        let discussion = Discussion {
            title: "Fix".into(),
            state: BranchState::Open,
            target: Some("main".into()),
            conflicts: true,
            reviews: vec![Review {
                author: "cy".into(),
                verdict: Verdict::Changes,
            }],
            checks: vec![
                check("a", CheckState::Success),
                check("b", CheckState::Failed),
                check("c", CheckState::Running),
            ],
            ..Discussion::default()
        };
        let page = render(
            tr,
            &Environment::Local,
            dir.path(),
            None,
            "fix",
            4,
            &discussion,
            None,
        );
        assert_eq!(page.header, "!4 Fix · open · ✗ Checks 1/3");
        assert_eq!(
            page.sections[1].kind,
            SectionKind::Checks(CheckState::Failed)
        );
        let names: Vec<&str> = page.sections[1].markdown.lines().skip(2).collect();
        assert_eq!(names, ["- ✗ b", "- ● c", "- ✓ a"]);
        let overview = &page.sections[0].markdown;
        assert!(overview.contains("`fix` → `main` · open"), "{overview}");
        assert!(
            overview.contains("1 passed, 1 failed, 1 running or waiting"),
            "{overview}"
        );
        assert!(
            overview.contains("✗ **cy** requested changes"),
            "{overview}"
        );
        assert!(overview.contains(tr.change_conflicts), "{overview}");

        let pipeline = Pipeline {
            status: PipelineStatus::Success,
            url: Some("p".into()),
        };
        let bare = Discussion {
            title: "Fix".into(),
            ..Discussion::default()
        };
        let page = render(
            tr,
            &Environment::Local,
            dir.path(),
            None,
            "fix",
            4,
            &bare,
            Some(&pipeline),
        );
        assert_eq!(
            page.sections[1].markdown,
            "## Checks\n\n- ✓ [Pipeline](p)\n"
        );
    }

    #[test]
    fn unix_times_read_as_iso() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_793_282_531), "2026-10-29T14:02:11Z");
    }

    #[test]
    fn only_hex_names_reach_git() {
        assert!(valid_commit("0123abcd"));
        assert!(!valid_commit("--output=x"));
        assert!(!valid_commit("HEAD~1"));
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn comments_follow_their_line_through_an_amend_and_a_lost_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q"]);
        git(repo, &["checkout", "-q", "-b", "fix"]);
        std::fs::write(repo.join("a.rs"), "fn a() {}\nlet x = 1;\nlet y = 2;\n").unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-qm", "one"]);
        let first = git(repo, &["rev-parse", "HEAD"]);
        // Rewritten: two lines above the commented one, `y` changed.
        std::fs::write(
            repo.join("a.rs"),
            "// a\n// b\nfn a() {}\nlet x = 1;\nlet y = 3;\n",
        )
        .unwrap();
        git(repo, &["commit", "-qa", "--amend", "-m", "one"]);
        let thread = |line, commit: Option<&str>, hunk: Option<&str>| Thread {
            path: "a.rs".into(),
            line: Some(line),
            commit: commit.map(str::to_owned),
            hunk: hunk.map(str::to_owned),
            comments: vec![Comment {
                author: "bob".into(),
                body: format!("on {line}"),
                at: None,
            }],
            ..Thread::default()
        };
        let discussion = Discussion {
            title: "Fix".into(),
            threads: vec![
                thread(2, Some(&first), None),
                thread(3, Some(&first), None),
                // A commit nobody fetched: found by the hunk's last line.
                thread(
                    2,
                    Some("0123456789abcdef"),
                    Some("@@ -1 +1,2 @@\n fn a() {}\n+let x = 1;"),
                ),
            ],
            ..Discussion::default()
        };
        let rendered = render(
            &crate::i18n::EN,
            &Environment::Local,
            repo,
            None,
            "fix",
            3,
            &discussion,
            None,
        );
        assert_eq!(rendered.title, "!3 Fix");
        let labels: Vec<&str> = rendered.sections.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels[0], "Overview");
        assert!(labels.contains(&"a.rs:4"), "{labels:?}");
        let page = rendered.markdown();
        assert!(page.contains("`a.rs` line 4"), "{page}");
        assert!(page.contains("Moved from line 2"), "{page}");
        assert!(page.contains("Line 3 was changed"), "{page}");
        assert!(page.contains(">    4  let x = 1;"), "{page}");
        assert!(page.contains("Found by its text, was line 2"), "{page}");
        // The amend is in the reflog, and range-diff names the rewrite.
        assert!(page.contains("amend"), "{page}");
        assert!(page.contains("rewritten"), "{page}");
    }
}
