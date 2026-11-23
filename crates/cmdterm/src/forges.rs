//! Forge accounts and the change requests of agent worktrees.
//!
//! Extensions register forges with `term.forge` (see `ext/README.md`). The
//! app keeps their accounts: the user signs in on an extension's page of the
//! settings, the token goes into the system's secret storage, and the app
//! adds it to the `term.http` requests the extension sends to that host.
//! Extensions never see a token.
//!
//! The rail's Agents tab lists the worktrees where agent sessions ran. The
//! ones whose agent finished are the only ones the app asks forges about,
//! in one `check` call per forge with every repository and branch, at most
//! every [`CHECK_EVERY`]. A branch once found merged is not asked about
//! again. Idle polls compare one `Instant` and allocate nothing.
//!
//! Before a repository whose branches all have a change request already is
//! asked about again, `git ls-remote` reads the default branch and those
//! branches over the git protocol, which no forge counts against its API
//! limits. While none of them moved, the forge is not asked, for up to
//! [`REFRESH_EVERY`]; that catches what moves no ref, such as a change
//! request closed without a merge.
//!
//! Agents read the change request of a worktree through the control socket
//! (`control/changes.rs`), from what these checks found. A worktree an
//! agent asks about is watched for a while: its branch joins the checks,
//! its repository's refs are read with `git ls-remote` on every check, and
//! its discussion is asked for again when they moved, when its change
//! request changed, or [`REFRESH_EVERY`] after it was asked last.
//!
//! Every rendered change request page is also written to the cache
//! directory, one file per worktree. The next time a worktree's page is
//! asked for, after a restart too, the saved page shows at once while the
//! forge is asked again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use cmdterm_env::os_proxy::ProxySettings;
use std::time::{Duration, Instant};

use schnellui::RedrawSignal;

use crate::discussion::{Page, Rendered};
use crate::ext::{
    BranchReport, BranchState, Callback, Discussion, DiscussionRequest, ForgeCheck, ForgeInfo,
    HttpRequest, HttpResponse,
};
use crate::i18n::Strings;
use crate::state::{AgentRecord, AgentSession};
use crate::term::Attention;
use crate::workspace::{Inner, LiveWorktree, Workspace};

/// How often finished agents' branches are looked up.
const CHECK_EVERY: Duration = Duration::from_secs(180);
/// The wait after an agent finished before its branch is looked up, so a
/// burst of state changes ends in one check.
const SOON: Duration = Duration::from_secs(10);
/// The shortest time between two checks of the same forge.
const MIN_SPACING: Duration = Duration::from_secs(30);
/// The first check after the start waits for the projects' snapshots.
const FIRST_CHECK: Duration = Duration::from_secs(5);
/// The longest a repository whose refs did not move goes without asking
/// its forge.
const REFRESH_EVERY: Duration = Duration::from_secs(30 * 60);
/// How long a `git ls-remote` run may take before checks stop waiting for it.
const GATE_TIMEOUT: Duration = Duration::from_secs(60);
/// Requests that may run at the same time, across extensions.
const MAX_IN_FLIGHT: usize = 16;
/// The secret storage service tokens are kept under.
const SERVICE: &str = "scriptschnell-term-forge";
/// The app's own view of a change request's discussion. Its key is
/// `<project id>:<worktree path>`.
pub const DISCUSSION_VIEW: &str = "scriptschnell-term.discussion";

/// Where an agent worktree stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentGroup {
    /// The agent finished its turn or waits for the user, and the branch
    /// is not merged.
    Finished,
    /// The agent is working.
    Working,
    /// The branch's change request is merged.
    Merged,
    /// An agent session that ended before the worktree's last one, or any
    /// that ended in a main worktree.
    Earlier,
}

/// One agent of the Agents tab: a live one in a terminal, or an agent
/// session that ended in a worktree.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRow {
    pub project: u64,
    pub project_name: String,
    pub path: PathBuf,
    /// The agent session's title, else the branch, else the folder.
    pub title: String,
    pub branch: Option<String>,
    pub attention: Attention,
    pub message: String,
    pub group: AgentGroup,
    /// The change request of the branch, as a forge last reported it.
    pub change: Option<BranchReport>,
    /// Whether the forge can show the change request's discussion.
    pub discussions: bool,
    /// The ended agent session a click resumes. `None` while an agent
    /// runs in the worktree, and for a merged branch.
    pub resume: Option<AgentSession>,
    /// The terminal the agent runs in; `None` without a live agent.
    pub panel: Option<u64>,
    /// When an earlier session was seen last, in seconds since the Unix
    /// epoch; 0 for the other rows.
    pub seen: u64,
    /// The forge and account the branch's remote belongs to, with the
    /// remote's host, path and URL.
    remote: Option<RemoteKey>,
}

#[derive(Clone, Debug, PartialEq)]
struct RemoteKey {
    forge: String,
    account: String,
    host: String,
    path: String,
    url: String,
}

/// Where an account's sign-in stands, for its row in the settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountState {
    SignedOut,
    /// The token is being checked and stored.
    SigningIn,
    /// Signed in on the settings page, as this user (empty when the forge
    /// cannot name the owner).
    SignedIn(String),
    /// A token from one of the forge's variables.
    FromEnv(String),
    Failed(String),
}

enum Done {
    Http(Option<Callback>, HttpResponse),
    /// A token read from the secret storage at start.
    Loaded(String, Option<String>),
    SignedIn {
        key: String,
        token: String,
        result: Result<String, String>,
    },
    /// A change request page rendered on its thread, by view key.
    Discussion(String, Arc<Rendered>),
    /// The checks of a due check, after `git ls-remote` read the refs of
    /// the repositories in `gates`.
    Gated {
        batches: BTreeMap<String, Vec<ForgeCheck>>,
        /// Per `(account, path)`: a hash of the refs, `None` when
        /// `ls-remote` failed, and whether the forge is skipped while they
        /// did not move.
        refs: Vec<Refs>,
    },
}

/// What `ls-remote` read of one repository: `(account, path)`, the hash
/// of its refs, and whether its forge is skipped while they did not move.
type Refs = ((String, String), Option<u64>, bool);

/// A repository whose refs are read before its forge is asked.
struct Gate {
    account: String,
    path: String,
    /// Whether the forge is skipped while the refs did not move. Off for
    /// repositories read only because an agent watches one of their
    /// worktrees.
    filter: bool,
    env: cmdterm_env::Environment,
    cwd: PathBuf,
    /// The git repository recorded for the project at `cwd`.
    known: Option<crate::vcs::KnownRepo>,
    url: String,
    branches: Vec<String>,
}

/// What `ls-remote` saw of a repository when its forge was asked last.
struct Seen {
    refs: u64,
    asked: Instant,
}

/// A discussion the app asked a forge for, by request id.
struct Ask {
    key: String,
    env: cmdterm_env::Environment,
    worktree: PathBuf,
    /// The git repository recorded for the project.
    known: Option<crate::vcs::KnownRepo>,
    branch: String,
    number: u64,
    /// The built-in client's pipeline of the branch.
    pipeline: Option<crate::forge::Pipeline>,
}

/// Forge accounts, the last reports of branches, and when to check next.
pub struct Forges {
    /// Tokens by account key, `<forge id>/<account>`.
    tokens: HashMap<String, String>,
    /// Sign-ins that are running or failed, by account key.
    trouble: HashMap<String, AccountState>,
    /// Token fields of the settings, by account key.
    pub typed: HashMap<String, String>,
    /// A token field rebuilt the form while the user types in it, so the
    /// settings keep focus on that field.
    pub typing: bool,
    /// `(account, path, branch)` to what the forge reported last.
    reports: HashMap<(String, String, String), BranchReport>,
    next_check: Instant,
    /// Per forge: when it was asked last and when it may be asked again.
    asked: HashMap<String, (Instant, Instant)>,
    /// Whether the saved accounts' tokens were read.
    loaded: bool,
    in_flight: Arc<AtomicUsize>,
    done: (Sender<Done>, Receiver<Done>),
    wake: Arc<OnceLock<RedrawSignal>>,
    /// Whether tokens go to the system's secret storage. Off in tests.
    keyring: bool,
    /// Per `(account, path)`, the refs when the forge was asked last.
    seen: HashMap<(String, String), Seen>,
    /// When the running `ls-remote` gate started.
    gating: Option<Instant>,
    /// Whether checks read refs with `git ls-remote` first. Off in tests,
    /// which check synchronously.
    gate: bool,
    /// Discussion views by key.
    pages: HashMap<String, Page>,
    /// Views that wait for their worktree, its branch's change request or
    /// the saved tokens, whatever page they show meanwhile.
    waiting: HashSet<String>,
    /// Where rendered pages are saved, one file per view key.
    page_dir: PathBuf,
    /// Saved tokens still being read from the secret storage.
    tokens_pending: usize,
    /// The add-account fields of the Accounts page by forge id: what the
    /// user typed and why it was refused.
    pub drafts: HashMap<String, (String, Option<String>)>,
    /// Discussions asked for and not answered, by request id.
    asks: HashMap<String, Ask>,
    next_ask: u64,
    /// Whether what the control socket reports of change requests changed
    /// since it was published.
    stale: bool,
    /// When the discussion of each watched worktree was asked for last, by
    /// view key.
    watch_asked: HashMap<String, Instant>,
}

impl Forges {
    pub fn new(wake: Arc<OnceLock<RedrawSignal>>, page_dir: PathBuf) -> Self {
        Self {
            tokens: HashMap::new(),
            trouble: HashMap::new(),
            typed: HashMap::new(),
            typing: false,
            reports: HashMap::new(),
            next_check: Instant::now() + FIRST_CHECK,
            asked: HashMap::new(),
            loaded: false,
            in_flight: Arc::default(),
            done: mpsc::channel(),
            wake,
            keyring: !cfg!(test),
            seen: HashMap::new(),
            gating: None,
            gate: !cfg!(test),
            pages: HashMap::new(),
            waiting: HashSet::new(),
            page_dir,
            tokens_pending: 0,
            drafts: HashMap::new(),
            asks: HashMap::new(),
            next_ask: 0,
            stale: true,
            watch_asked: HashMap::new(),
        }
    }

    /// Publishes the change requests to the control socket on the next poll.
    pub(crate) fn changed(&mut self) {
        self.stale = true;
    }

    /// Checks soon, after an agent finished.
    pub fn soon(&mut self) {
        self.next_check = self.next_check.min(Instant::now() + SOON);
    }

    /// Checks on the next poll, for a page someone waits for.
    fn now(&mut self) {
        self.next_check = Instant::now();
        if let Some(signal) = self.wake.get() {
            signal.request_redraw();
        }
    }

    /// The token for `account` of `forge`: the one the user signed in with,
    /// else for accounts on the forge's first host the first of its
    /// variables that is set.
    fn token(&self, forge: &ForgeInfo, account: &str) -> Option<String> {
        if let Some(token) = self.tokens.get(&account_key(&forge.id, account)) {
            return Some(token.clone());
        }
        env_token(forge, account).map(|(_, token)| token)
    }

    /// Where the sign-in of `account` on `forge` stands.
    pub fn account_state(
        &self,
        forge: &ForgeInfo,
        account: &str,
        accounts: &BTreeMap<String, String>,
    ) -> AccountState {
        let key = account_key(&forge.id, account);
        if let Some(state) = self.trouble.get(&key) {
            return state.clone();
        }
        if let Some(user) = accounts.get(&key) {
            return AccountState::SignedIn(user.clone());
        }
        match env_token(forge, account) {
            Some((var, _)) => AccountState::FromEnv(var),
            None => AccountState::SignedOut,
        }
    }

    /// The branch report of `account`, `path` and `branch`.
    fn report(&self, account: &str, path: &str, branch: &str) -> Option<&BranchReport> {
        // Keys own their strings; a lookup in this cold path builds one.
        self.reports
            .get(&(account.to_owned(), path.to_owned(), branch.to_owned()))
    }

    fn send(&self, done: Done) {
        let _ = self.done.0.send(done);
        if let Some(signal) = self.wake.get() {
            signal.request_redraw();
        }
    }

    /// Sends `request` on a thread of its own, with `auth` as a header.
    fn spawn_http(&self, request: HttpRequest, auth: Option<(String, String)>) {
        let callback = request.done;
        if self.in_flight.load(Ordering::Relaxed) >= MAX_IN_FLIGHT {
            self.send(Done::Http(
                callback,
                failed(format!("more than {MAX_IN_FLIGHT} requests at once")),
            ));
            return;
        }
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        let (sender, wake, in_flight) = (
            self.done.0.clone(),
            Arc::clone(&self.wake),
            Arc::clone(&self.in_flight),
        );
        let spawned = std::thread::Builder::new()
            .name("forge-http".into())
            .spawn(move || {
                let response = send_http(&agent(), &request, auth.as_ref());
                in_flight.fetch_sub(1, Ordering::Relaxed);
                let _ = sender.send(Done::Http(callback, response));
                if let Some(signal) = wake.get() {
                    signal.request_redraw();
                }
            });
        if let Err(error) = spawned {
            self.in_flight.fetch_sub(1, Ordering::Relaxed);
            self.send(Done::Http(callback, failed(error.to_string())));
        }
    }
}

/// A hash of what `git ls-remote` lists for the default branch and the
/// branches of `gate`; `None` when it fails, such as for a repository that
/// needs a password.
fn ls_remote(gate: &Gate) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let repo = crate::vcs::open(&gate.env, &gate.cwd, false, gate.known.as_ref()).ok()?;
    let mut command = repo
        .git_command(&repo.root)
        .ok()?
        .var("SSH_ASKPASS_REQUIRE", "never")
        .args(["ls-remote", "--", gate.url.as_str(), "HEAD"]);
    for branch in &gate.branches {
        command = command.arg(format!("refs/heads/{branch}"));
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    output.stdout.hash(&mut hasher);
    Some(hasher.finish())
}

/// `<forge id>/<account>`, such as `github.github/github.com/acme`: names
/// an account in the settings and the secret storage.
pub fn account_key(forge: &str, account: &str) -> String {
    format!("{forge}/{account}")
}

/// The first of `forge`'s variables that holds a token, for the accounts on
/// the host of its first account.
fn env_token(forge: &ForgeInfo, account: &str) -> Option<(String, String)> {
    let host = crate::ext::account_host(account);
    if forge
        .hosts
        .first()
        .is_none_or(|first| crate::ext::account_host(first) != host)
    {
        return None;
    }
    forge.env.iter().find_map(|var| {
        std::env::var(var)
            .ok()
            .filter(|token| !token.is_empty())
            .map(|token| (var.clone(), token))
    })
}

fn failed(error: impl Into<String>) -> HttpResponse {
    HttpResponse {
        error: Some(error.into()),
        ..HttpResponse::default()
    }
}

/// The header `forge` sends `token` in.
fn auth_header(forge: &ForgeInfo, token: &str) -> (String, String) {
    let mut value = forge.auth_value.replace("{token}", token);
    if value.contains("{basic}") {
        value = value.replace(
            "{basic}",
            &crate::forge::base64(format!(":{token}").as_bytes()),
        );
    }
    if value.contains("{base64}") {
        value = value.replace("{base64}", &crate::forge::base64(token.as_bytes()));
    }
    (forge.auth_header.clone(), value)
}

/// Whether a token for `account` may go to `url`: the account's host or
/// one of its subdomains, such as `api.github.com` for `github.com`.
fn may_send_token(account: &str, url: &str) -> bool {
    let Some(host) = crate::ext::url_host(url) else {
        return false;
    };
    let account = account.split(['/', ':']).next().unwrap_or(account);
    host == account
        || host
            .strip_suffix(account)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// The agent for forge requests. It is built again when the proxy changes,
/// and kept otherwise, so its connections are reused.
fn agent() -> ureq::Agent {
    static AGENT: Mutex<Option<(Option<ProxySettings>, ureq::Agent)>> = Mutex::new(None);
    let proxy = cmdterm_env::os_proxy::current();
    let mut cached = AGENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((settings, agent)) = &*cached
        && *settings == proxy
    {
        return agent.clone();
    }
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_secs(20)))
        .proxy(
            proxy
                .as_ref()
                .and_then(|p| cmdterm_update::proxy::build(&p.server, &p.no_proxy)),
        )
        .tls_config(cmdterm_update::system_tls())
        .http_status_as_error(false)
        .user_agent(concat!("scriptschnell-term/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    *cached = Some((proxy, agent.clone()));
    agent
}

fn send_http(
    agent: &ureq::Agent,
    request: &HttpRequest,
    auth: Option<&(String, String)>,
) -> HttpResponse {
    let mut builder = ureq::http::Request::builder()
        .method(request.method.as_str())
        .uri(request.url.as_str())
        .header("Accept", "application/json");
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    if let Some((name, value)) = auth {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let body = request.body.clone().unwrap_or_default();
    let built = match builder.body(body) {
        Ok(built) => built,
        Err(error) => return failed(error.to_string()),
    };
    let mut response = match agent.run(built) {
        Ok(response) => response,
        Err(error) => return failed(error.to_string()),
    };
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            Some((
                name.as_str().to_ascii_lowercase(),
                value.to_str().ok()?.to_owned(),
            ))
        })
        .collect();
    match response
        .body_mut()
        .with_config()
        .limit(crate::ext::HTTP_BODY_LIMIT)
        .read_to_string()
    {
        Ok(body) => HttpResponse {
            status,
            headers,
            body,
            error: None,
        },
        Err(error) => HttpResponse {
            status,
            headers,
            body: String::new(),
            error: Some(error.to_string()),
        },
    }
}

/// The value at a dotted `path` of a JSON answer, as text.
fn json_field(body: &str, path: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(body).ok()?;
    for part in path.split('.').filter(|p| !p.is_empty()) {
        value = value.get_mut(part)?.take();
    }
    match value {
        serde_json::Value::String(text) => Some(text),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Opens `url` in the system's browser, where the user is signed in to the
/// forge.
pub(crate) fn open_in_system_browser(url: &str) {
    let program = crate::open_url::SYSTEM_OPENER;
    let command = cmdterm_env::Environment::Local.command(program).arg(url);
    // On a thread, so the opener is waited for without holding up a frame.
    let _ = std::thread::Builder::new()
        .name("open-url".into())
        .spawn(move || command.output());
}

impl Inner {
    /// The forge and account the remote of project `project` belongs to,
    /// with the remote's host, path and URL.
    fn remote_key(&self, project: u64) -> Option<RemoteKey> {
        let url = self.snapshots.get(&project)?.remote_url.as_deref()?;
        let (host, path) = crate::forge::split_remote(url)?;
        self.ext.forges().iter().find_map(|forge| {
            Some(RemoteKey {
                forge: forge.id.clone(),
                account: forge.account_for(&host, &path)?.to_owned(),
                host: host.clone(),
                path: path.clone(),
                url: url.to_owned(),
            })
        })
    }

    /// The id of the forge the active project's remote belongs to, with
    /// the project's name, for the Accounts page to list it first.
    pub(crate) fn active_forge(&self) -> Option<(String, String)> {
        let project = self.state.active()?;
        let key = self.remote_key(project.id)?;
        Some((key.forge, project.name.clone()))
    }

    /// The change request of `branch` of project `project`, as its forge
    /// last reported it, else from the built-in forge client's open change
    /// requests. `None` while neither knows the branch.
    fn branch_change(&self, project: u64, key: &RemoteKey, branch: &str) -> Option<BranchReport> {
        if let Some(report) = self.forges.report(&key.account, &key.path, branch) {
            return Some(report.clone());
        }
        let request = self
            .snapshots
            .get(&project)?
            .change_requests
            .iter()
            .find(|r| r.source_branch == branch)?;
        Some(BranchReport {
            account: key.account.clone(),
            path: key.path.clone(),
            branch: branch.to_owned(),
            state: BranchState::Open,
            number: Some(request.number),
            url: Some(request.url.clone()),
            title: Some(request.title.clone()),
        })
    }

    /// One row per agent, in project order: every agent running in a
    /// terminal, and for each worktree but the main one without a live
    /// agent, its last agent session when it has a saved agent title or
    /// session. After a worktree's rows come its ended sessions no row
    /// stands for, in the Earlier group.
    pub fn agent_rows(&self) -> Vec<AgentRow> {
        let forges = self.ext.forges();
        let mut rows = Vec::new();
        for project in &self.state.projects {
            let snapshot = self.snapshots.get(&project.id);
            let remote = self.remote_key(project.id);
            let mut add = |path: &PathBuf, branch: Option<&str>, main: bool| {
                let agents = self
                    .live_get(project.id, path)
                    .map(LiveWorktree::agents)
                    .unwrap_or_default();
                let config = project.worktree(path);
                // Ended sessions, the last one first.
                let mut ended = config
                    .map_or(&[][..], |c| c.agent_sessions.as_slice())
                    .iter()
                    .rev()
                    .filter(|r| !self.agent_open(project.id, path, &r.resume()))
                    .peekable();
                let saved = config
                    .and_then(|w| w.agent_title())
                    .or_else(|| ended.peek().and_then(|r| r.title.as_deref()));
                let fallback = || match (saved, branch) {
                    (Some(title), _) => title.to_owned(),
                    (None, Some(branch)) => branch.to_owned(),
                    (None, None) => crate::search::folder_name(path),
                };
                let listed =
                    !agents.is_empty() || (!main && (saved.is_some() || ended.peek().is_some()));
                if listed {
                    let change = branch.and_then(|branch| {
                        self.branch_change(project.id, remote.as_ref()?, branch)
                    });
                    let merged = change
                        .as_ref()
                        .is_some_and(|c| c.state == BranchState::Merged);
                    let group = |attention: Attention| {
                        if merged {
                            AgentGroup::Merged
                        } else if attention == Attention::Busy {
                            AgentGroup::Working
                        } else {
                            AgentGroup::Finished
                        }
                    };
                    let discussions = remote.as_ref().is_some_and(|key| {
                        forges.iter().any(|f| f.id == key.forge && f.discussions)
                    });
                    let row = |title: String, attention: Attention, message: String| AgentRow {
                        project: project.id,
                        project_name: project.name.clone(),
                        path: path.clone(),
                        title,
                        branch: branch.map(str::to_owned),
                        attention,
                        message,
                        group: group(attention),
                        change: change.clone(),
                        discussions,
                        resume: None,
                        panel: None,
                        seen: 0,
                        remote: remote.clone(),
                    };
                    let single = agents.len() == 1;
                    for agent in &agents {
                        // A lone agent may go by the worktree's saved title;
                        // several go by their own, else their tab's label.
                        let title = if !agent.title.is_empty() {
                            agent.title.clone()
                        } else if single {
                            fallback()
                        } else {
                            agent.label.clone()
                        };
                        rows.push(AgentRow {
                            panel: Some(agent.panel),
                            ..row(title, agent.attention, agent.message.clone())
                        });
                    }
                    if agents.is_empty() {
                        // Without a live agent the row stands for the last
                        // ended session, which a click resumes.
                        let resume = ended.next().map(AgentRecord::resume);
                        rows.push(AgentRow {
                            resume: resume.filter(|_| !merged),
                            ..row(fallback(), Attention::None, String::new())
                        });
                    }
                }
                for record in ended {
                    let title = record
                        .title
                        .clone()
                        .or_else(|| {
                            let program = crate::agents::program_of(&record.resume())?;
                            Some(crate::agents::Agent::find(program)?.name().to_owned())
                        })
                        .unwrap_or_else(|| record.agent.clone());
                    rows.push(AgentRow {
                        project: project.id,
                        project_name: project.name.clone(),
                        path: path.clone(),
                        title,
                        branch: branch.map(str::to_owned),
                        attention: Attention::None,
                        message: String::new(),
                        group: AgentGroup::Earlier,
                        change: None,
                        discussions: false,
                        resume: Some(record.resume()),
                        panel: None,
                        seen: record.seen,
                        remote: None,
                    });
                }
            };
            if project.is_global() {
                add(&project.root, None, true);
            } else if let Some(snapshot) = snapshot {
                for worktree in snapshot.worktrees.iter().filter(|w| !w.bare) {
                    add(&worktree.path, worktree.branch.as_deref(), worktree.is_main);
                }
            } else {
                for config in &project.worktrees {
                    add(&config.path, None, config.path == project.root);
                }
            }
        }
        rows
    }
}

impl Workspace {
    /// Applies finished forge requests and asks the forges about finished
    /// agents when a check is due. One `try_recv` and one clock read while
    /// nothing happens.
    pub fn poll_forges(&self) {
        loop {
            let done = self.borrow().forges.done.1.try_recv();
            let Ok(done) = done else { break };
            self.contain(|| self.forge_done(done));
        }
        let due = Instant::now() >= self.borrow().forges.next_check;
        if due {
            self.check_forges();
        }
        let (watches, publish) = {
            let inner = self.borrow();
            inner.control_ref().map_or((false, false), |control| {
                (
                    control.has_new_watches(),
                    inner.forges.stale || control.changes_wanted(),
                )
            })
        };
        if watches {
            self.start_watches();
        }
        if publish {
            self.publish_changes();
        }
    }

    fn forge_done(&self, done: Done) {
        match done {
            Done::Http(Some(callback), response) => {
                let outcome = self.borrow_mut().ext.complete_http(callback, &response);
                if let Err(error) = outcome {
                    self.notify(error.to_string());
                }
            }
            Done::Http(None, _) => {}
            Done::Loaded(key, token) => {
                let mut inner = self.borrow_mut();
                if let Some(token) = token {
                    inner.forges.tokens.insert(key, token);
                    inner.forges.soon();
                }
                inner.forges.tokens_pending = inner.forges.tokens_pending.saturating_sub(1);
                if inner.forges.tokens_pending == 0 {
                    drop(inner);
                    self.ask_waiting_discussions();
                }
            }
            Done::SignedIn { key, token, result } => {
                let mut inner = self.borrow_mut();
                inner.dirty.settings = true;
                let tr = inner.tr;
                let account = key.split_once('/').map_or(key.as_str(), |(_, a)| a);
                match &result {
                    Ok(user) => inner.toast(false, (tr.toast_signed_in)(account, user)),
                    Err(error) => inner.toast(true, (tr.toast_sign_in_failed)(account, error)),
                }
                match result {
                    Ok(user) => {
                        inner.forges.trouble.remove(&key);
                        inner.forges.tokens.insert(key.clone(), token);
                        inner.settings.forge_accounts.insert(key, user);
                        inner.forges.soon();
                        drop(inner);
                        self.save_settings();
                    }
                    Err(error) => {
                        inner
                            .forges
                            .trouble
                            .insert(key, AccountState::Failed(error));
                    }
                }
            }
            Done::Gated { batches, refs } => self.ask_forges(batches, refs),
            Done::Discussion(key, rendered) => {
                let mut inner = self.borrow_mut();
                inner
                    .forges
                    .pages
                    .insert(key.clone(), Page::Ready(rendered));
                inner.forges.stale = true;
                drop(inner);
                self.refresh_discussion(&key);
            }
        }
    }
}

impl Inner {
    /// Reads the tokens of the saved accounts from the secret storage, on a
    /// thread, since a locked keyring asks the user first.
    fn load_forge_tokens(&mut self) {
        self.forges.loaded = true;
        if !self.forges.keyring || self.settings.forge_accounts.is_empty() {
            return;
        }
        let keys: Vec<String> = self.settings.forge_accounts.keys().cloned().collect();
        let (sender, wake) = (self.forges.done.0.clone(), Arc::clone(&self.forges.wake));
        self.forges.tokens_pending = keys.len();
        let started = std::thread::Builder::new()
            .name("forge-tokens".into())
            .spawn(move || {
                for key in keys {
                    let token = cmdterm_env::os_secrets::lookup(SERVICE, &key)
                        .ok()
                        .flatten();
                    let _ = sender.send(Done::Loaded(key, token));
                }
                if let Some(signal) = wake.get() {
                    signal.request_redraw();
                }
            });
        if started.is_err() {
            self.forges.tokens_pending = 0;
        }
    }

    /// Whether every saved token was read, or will never be.
    fn tokens_read(&self) -> bool {
        self.forges.loaded && self.forges.tokens_pending == 0
    }
}

impl Workspace {
    /// Asks every forge about the branches of finished agents that are not
    /// merged yet, one `check` call per forge.
    fn check_forges(&self) {
        if !self.borrow().forges.loaded {
            self.borrow_mut().load_forge_tokens();
        }
        let now = Instant::now();
        let (batches, gates) = {
            let mut inner = self.borrow_mut();
            inner.forges.next_check = now + CHECK_EVERY;
            if inner
                .forges
                .gating
                .is_some_and(|since| now.duration_since(since) < GATE_TIMEOUT)
            {
                return;
            }
            // The branches of finished agents, and those of change request
            // pages that wait for their branch's change request.
            let mut branches: Vec<(u64, String, RemoteKey, bool)> = Vec::new();
            for row in inner.agent_rows() {
                let (Some(branch), Some(remote)) = (row.branch, row.remote) else {
                    continue;
                };
                // Several agents of one worktree share its branch.
                if row.group != AgentGroup::Finished
                    || branches
                        .iter()
                        .any(|(project, name, ..)| *project == row.project && *name == branch)
                {
                    continue;
                }
                let known = row
                    .change
                    .as_ref()
                    .is_some_and(|c| c.state != BranchState::None);
                branches.push((row.project, branch, remote, known));
            }
            for key in &inner.forges.waiting {
                let Some((project, path)) = key
                    .split_once(':')
                    .and_then(|(id, path)| Some((id.parse::<u64>().ok()?, PathBuf::from(path))))
                else {
                    continue;
                };
                let branch = inner
                    .snapshots
                    .get(&project)
                    .and_then(|s| s.worktrees.iter().find(|w| w.path == path))
                    .and_then(|w| w.branch.clone());
                let (Some(branch), Some(remote)) = (branch, inner.remote_key(project)) else {
                    continue;
                };
                if !branches
                    .iter()
                    .any(|(p, b, _, _)| *p == project && *b == branch)
                {
                    branches.push((project, branch, remote, false));
                }
            }
            // The branches of worktrees agents watch, whose repositories'
            // refs are read on every check.
            let watched = inner.control_ref().map(|c| c.watched()).unwrap_or_default();
            let mut watched_repos: Vec<(String, String)> = Vec::new();
            for (project, path) in watched {
                let branch = inner
                    .snapshots
                    .get(&project)
                    .and_then(|s| s.worktrees.iter().find(|w| w.path == path))
                    .and_then(|w| w.branch.clone());
                let (Some(branch), Some(remote)) = (branch, inner.remote_key(project)) else {
                    continue;
                };
                let repo = (remote.account.clone(), remote.path.clone());
                if !watched_repos.contains(&repo) {
                    watched_repos.push(repo);
                }
                if !branches
                    .iter()
                    .any(|(p, b, _, _)| *p == project && *b == branch)
                {
                    let known = inner
                        .branch_change(project, &remote, &branch)
                        .is_some_and(|c| c.state != BranchState::None);
                    branches.push((project, branch, remote, known));
                }
            }
            let mut batches: BTreeMap<String, Vec<ForgeCheck>> = BTreeMap::new();
            let mut gates: Vec<Gate> = Vec::new();
            // Repositories with a branch without a change request skip the
            // gate: opening one moves no ref ls-remote reads.
            let mut ungated: Vec<(String, String)> = Vec::new();
            for (project, branch, remote, known) in branches {
                let repo = (remote.account.clone(), remote.path.clone());
                if !known && !ungated.contains(&repo) {
                    ungated.push(repo.clone());
                }
                if inner.forges.gate {
                    match gates
                        .iter_mut()
                        .find(|g| g.account == repo.0 && g.path == repo.1)
                    {
                        Some(gate) => gate.branches.push(branch.clone()),
                        None => {
                            if let Some(project) = inner.state.project(project) {
                                gates.push(Gate {
                                    account: repo.0.clone(),
                                    path: repo.1.clone(),
                                    filter: true,
                                    env: project.env.clone(),
                                    cwd: project.root.clone(),
                                    known: project.known_repo(),
                                    url: remote.url.clone(),
                                    branches: vec![branch.clone()],
                                });
                            }
                        }
                    }
                }
                let Some(forge) = inner.ext.forges().iter().find(|f| f.id == remote.forge) else {
                    continue;
                };
                let signed_in = inner.forges.token(forge, &remote.account).is_some();
                let checks = batches.entry(remote.forge.clone()).or_default();
                match checks
                    .iter_mut()
                    .find(|c| c.account == remote.account && c.path == remote.path)
                {
                    Some(check) => {
                        if !check.branches.contains(&branch) {
                            check.branches.push(branch);
                        }
                    }
                    None => checks.push(ForgeCheck {
                        account: remote.account,
                        host: remote.host,
                        path: remote.path,
                        remote: remote.url,
                        branches: vec![branch],
                        signed_in,
                    }),
                }
            }
            // A forge asked too recently is asked as soon as it may be.
            let mut retry = inner.forges.next_check;
            batches.retain(|forge, _| match inner.forges.asked.get(forge) {
                Some((last, hold)) => {
                    let may = (*hold).max(*last + MIN_SPACING);
                    if now < may {
                        retry = retry.min(may);
                    }
                    now >= may
                }
                None => true,
            });
            inner.forges.next_check = retry;
            for forge in batches.keys() {
                inner.forges.asked.insert(forge.clone(), (now, now));
            }
            gates.retain_mut(|g| {
                let repo = (g.account.clone(), g.path.clone());
                g.filter = !ungated.contains(&repo) && inner.forges.seen.contains_key(&repo);
                (g.filter || watched_repos.contains(&repo))
                    && batches
                        .values()
                        .flatten()
                        .any(|c| c.account == g.account && c.path == g.path)
            });
            (batches, gates)
        };
        if gates.is_empty() {
            self.ask_forges(batches, Vec::new());
            return;
        }
        let mut inner = self.borrow_mut();
        inner.forges.gating = Some(now);
        let (sender, wake) = (inner.forges.done.0.clone(), Arc::clone(&inner.forges.wake));
        drop(inner);
        let _ = std::thread::Builder::new()
            .name("forge-ls-remote".into())
            .spawn(move || {
                let refs = gates
                    .iter()
                    .map(|g| ((g.account.clone(), g.path.clone()), ls_remote(g), g.filter))
                    .collect();
                let _ = sender.send(Done::Gated { batches, refs });
                if let Some(signal) = wake.get() {
                    signal.request_redraw();
                }
            });
    }

    /// Asks every forge in `batches`, but not about repositories whose refs
    /// in `refs` did not move since their forge was asked last, unless
    /// that was [`REFRESH_EVERY`] ago or the repository was read only for
    /// a watched worktree. Then asks again for the discussions of watched
    /// worktrees that are due.
    fn ask_forges(&self, mut batches: BTreeMap<String, Vec<ForgeCheck>>, refs: Vec<Refs>) {
        let now = Instant::now();
        // The forges that may be asked now; discussions keep to them too.
        let allowed: Vec<String> = batches.keys().cloned().collect();
        let moved: Vec<(String, String)>;
        {
            let mut inner = self.borrow_mut();
            inner.forges.gating = None;
            let unmoved: Vec<(String, String)> = refs
                .iter()
                .filter(|(_, _, filter)| *filter)
                .filter_map(|(repo, hash, _)| {
                    let seen = inner.forges.seen.get(repo)?;
                    let same = hash.is_some_and(|h| h == seen.refs);
                    (same && now.duration_since(seen.asked) < REFRESH_EVERY).then(|| repo.clone())
                })
                .collect();
            // A hash of 0 stands for refs not read yet.
            moved = refs
                .iter()
                .filter_map(|(repo, hash, _)| {
                    let before = inner.forges.seen.get(repo)?.refs;
                    (before != 0 && hash.is_some_and(|h| h != before)).then(|| repo.clone())
                })
                .collect();
            for checks in batches.values_mut() {
                checks.retain(|c| {
                    !unmoved
                        .iter()
                        .any(|(account, path)| *account == c.account && *path == c.path)
                });
            }
            batches.retain(|_, checks| !checks.is_empty());
            let hashes: HashMap<_, _> = refs
                .into_iter()
                .map(|(repo, hash, _)| (repo, hash))
                .collect();
            for check in batches.values().flatten() {
                let repo = (check.account.clone(), check.path.clone());
                // A repository seen for the first time is read on the next
                // check, so its hash belongs to this ask.
                let refs = hashes.get(&repo).copied().flatten().unwrap_or(0);
                inner.forges.seen.insert(repo, Seen { refs, asked: now });
            }
        }
        for (forge, checks) in batches {
            let outcome = self.borrow_mut().ext.forge_check(&forge, &checks);
            if let Err(error) = outcome {
                self.notify(error.to_string());
            }
        }
        self.ask_watched_discussions(|forge, repo, key, inner| {
            allowed.iter().any(|f| f == forge)
                && (moved.contains(repo)
                    || inner
                        .forges
                        .watch_asked
                        .get(key)
                        .is_none_or(|at| now.duration_since(*at) >= REFRESH_EVERY))
        });
    }

    /// Sends an extension's `term.http` request, with the account's token
    /// when it names one.
    pub(crate) fn forge_http(&self, request: HttpRequest) {
        let inner = self.borrow();
        let mut auth = None;
        if let Some((forge_id, host)) = &request.account {
            let Some(forge) = inner.ext.forges().iter().find(|f| &f.id == forge_id) else {
                let error = format!("term.http: no forge '{forge_id}' is registered");
                inner.forges.send(Done::Http(request.done, failed(error)));
                return;
            };
            if !forge.hosts.contains(host) && !forge.aliases.iter().any(|(_, a)| a == host) {
                let error = format!("term.http: '{host}' is not an account of '{forge_id}'");
                inner.forges.send(Done::Http(request.done, failed(error)));
                return;
            }
            if !may_send_token(host, &request.url) {
                let site = crate::ext::account_host(host);
                let error = format!("term.http: the token of {host} only goes to {site}");
                inner.forges.send(Done::Http(request.done, failed(error)));
                return;
            }
            auth = inner
                .forges
                .token(forge, host)
                .map(|token| auth_header(forge, &token));
        }
        inner.forges.spawn_http(request, auth);
    }

    /// Takes what a forge's `check` found.
    pub(crate) fn forge_report(
        &self,
        forge: &str,
        results: Vec<BranchReport>,
        retry_after: Option<u64>,
    ) {
        let mut inner = self.borrow_mut();
        let mut changed: Vec<(String, String, String)> = Vec::new();
        for report in results {
            let key = (
                report.account.clone(),
                report.path.clone(),
                report.branch.clone(),
            );
            if inner.forges.reports.get(&key) != Some(&report) {
                inner.forges.reports.insert(key.clone(), report);
                changed.push(key);
            }
        }
        if let Some(seconds) = retry_after {
            let hold = Instant::now() + Duration::from_secs(seconds.min(24 * 60 * 60));
            let entry = inner
                .forges
                .asked
                .entry(forge.to_owned())
                .or_insert((Instant::now(), hold));
            entry.1 = hold;
        }
        if !changed.is_empty() {
            inner.dirty.sidebar = true;
            inner.forges.stale = true;
            drop(inner);
            self.ask_waiting_discussions();
            self.ask_watched_discussions(|_, (account, path), key, inner| {
                let Some((project, worktree)) = parse_key(key) else {
                    return false;
                };
                let branch = (inner.snapshots.get(&project))
                    .and_then(|s| s.worktrees.iter().find(|w| w.path == worktree))
                    .and_then(|w| w.branch.as_deref());
                branch.is_some_and(|branch| {
                    (changed.iter()).any(|(a, p, b)| a == account && p == path && b == branch)
                })
            });
        }
    }

    /// Opens the page where the user creates a token for `account`.
    pub fn open_forge_token_page(&self, forge: &str, account: &str) {
        let url = self.borrow_mut().ext.forge_token_url(forge, account);
        match url {
            Ok(Some(url)) => open_in_system_browser(&url),
            Ok(None) => {}
            Err(error) => self.toast_failed(error.to_string()),
        }
    }

    /// Keeps what the user typed into the token field of an account.
    pub fn set_forge_token_input(&self, forge: &str, account: &str, token: &str) {
        let mut inner = self.borrow_mut();
        let token = token.trim().to_owned();
        let was_empty = inner
            .forges
            .typed
            .get(&account_key(forge, account))
            .is_none_or(String::is_empty);
        // Sign in is on only with a token, so the form rebuilds when the
        // field turns empty or not, and keeps the field focused.
        if was_empty != token.is_empty() {
            inner.forges.typing = true;
            // Only one field holds the settings' focus ref.
            inner.settings_ui.finding_page = false;
            inner.settings_ui.finding_extension = false;
            inner.dirty.settings = true;
        }
        inner
            .forges
            .typed
            .insert(account_key(forge, account), token);
    }

    /// Checks the typed token with the forge's `whoami` URL, then keeps it in
    /// the secret storage and the account in the settings.
    pub fn forge_sign_in(&self, forge_id: &str, account: &str) {
        let key = account_key(forge_id, account);
        let token = self
            .borrow_mut()
            .forges
            .typed
            .remove(&key)
            .unwrap_or_default();
        let tr = self.tr();
        if token.is_empty() {
            let message = tr.forge_token_missing.to_owned();
            let mut inner = self.borrow_mut();
            inner.toast(true, (tr.toast_sign_in_failed)(account, &message));
            inner
                .forges
                .trouble
                .insert(key, AccountState::Failed(message));
            inner.dirty.settings = true;
            return;
        }
        let whoami = self.borrow_mut().ext.forge_whoami(forge_id, account);
        let whoami = match whoami {
            Ok(whoami) => whoami,
            Err(error) => {
                self.toast_failed((tr.toast_sign_in_failed)(account, &error.to_string()));
                return;
            }
        };
        let mut inner = self.borrow_mut();
        let Some(forge) = inner
            .ext
            .forges()
            .iter()
            .find(|f| f.id == forge_id)
            .cloned()
        else {
            return;
        };
        if let Some((url, _)) = &whoami
            && !may_send_token(account, url)
            && !url.starts_with("https://")
        {
            let error = format!("the token of {account} only goes over https");
            inner.toast(true, (tr.toast_sign_in_failed)(account, &error));
            inner
                .forges
                .trouble
                .insert(key, AccountState::Failed(error));
            inner.dirty.settings = true;
            return;
        }
        if let Some((url, _)) = &whoami {
            let host = crate::ext::url_host(url).unwrap_or_default();
            if !inner.ext.allows_host(&forge.extension, &host) {
                let error = format!("{host} is not one of the hosts of {}", forge.extension);
                inner.toast(true, (tr.toast_sign_in_failed)(account, &error));
                inner
                    .forges
                    .trouble
                    .insert(key, AccountState::Failed(error));
                inner.dirty.settings = true;
                return;
            }
        }
        inner
            .forges
            .trouble
            .insert(key.clone(), AccountState::SigningIn);
        inner.dirty.settings = true;
        let (sender, wake, keyring) = (
            inner.forges.done.0.clone(),
            Arc::clone(&inner.forges.wake),
            inner.forges.keyring,
        );
        let title = forge.title.clone();
        drop(inner);
        let _ = std::thread::Builder::new()
            .name("forge-sign-in".into())
            .spawn(move || {
                let result = verify(&agent(), &forge, &token, whoami).and_then(|user| {
                    if keyring {
                        let label = format!("scriptschnell-term: {title} ({key})");
                        cmdterm_env::os_secrets::store(&label, SERVICE, &key, &token)
                            .map_err(|error| error.to_string())?;
                    }
                    Ok(user)
                });
                let _ = sender.send(Done::SignedIn { key, token, result });
                if let Some(signal) = wake.get() {
                    signal.request_redraw();
                }
            });
    }

    /// The Sign out button: forgets the account's token and says so.
    pub fn forge_sign_out(&self, forge: &str, account: &str) {
        self.forget_forge_token(forge, account);
        self.toast_done((self.tr().toast_signed_out)(account));
    }

    /// Forgets the account's token, here and in the secret storage.
    fn forget_forge_token(&self, forge: &str, account: &str) {
        let key = account_key(forge, account);
        let keyring = {
            let mut inner = self.borrow_mut();
            inner.forges.tokens.remove(&key);
            inner.forges.trouble.remove(&key);
            inner.settings.forge_accounts.remove(&key);
            inner.dirty.settings = true;
            inner.forges.keyring
        };
        self.save_settings();
        if keyring {
            let _ = std::thread::Builder::new()
                .name("forge-sign-out".into())
                .spawn(move || cmdterm_env::os_secrets::remove(SERVICE, &key));
        }
    }

    /// Keeps what the user typed into the add-account field of `forge`. No
    /// rebuild: the field shows it already.
    pub fn set_forge_account_draft(&self, forge: &str, text: &str) {
        let mut inner = self.borrow_mut();
        let draft = inner.forges.drafts.entry(forge.to_owned()).or_default();
        draft.0 = text.to_owned();
        draft.1 = None;
    }

    /// Adds the typed account to `forge`. One that is no account, or one
    /// the forge has already, stays in the field with the reason under it.
    pub fn add_forge_account(&self, forge_id: &str) {
        let hosts = {
            let mut inner = self.borrow_mut();
            let tr = inner.tr;
            let Some(forge) = inner.ext.forges().iter().find(|f| f.id == forge_id) else {
                return;
            };
            let mut hosts = forge.hosts.clone();
            let draft = inner.forges.drafts.entry(forge_id.to_owned()).or_default();
            let typed = draft.0.trim().to_owned();
            if typed.is_empty() {
                return;
            }
            inner.dirty.settings = true;
            let error = match crate::ext::forge_account(&typed) {
                None => Some((tr.forge_not_an_account)(&typed)),
                Some(account) if hosts.contains(&account) => Some((tr.list_duplicate)(&typed)),
                Some(account) => {
                    hosts.push(account);
                    None
                }
            };
            if let Some(error) = error {
                inner.toast(true, error.clone());
                inner
                    .forges
                    .drafts
                    .entry(forge_id.to_owned())
                    .or_default()
                    .1 = Some(error);
                return;
            }
            inner.forges.drafts.remove(forge_id);
            let added = hosts.last().map_or(typed.as_str(), String::as_str);
            inner.toast(false, (tr.toast_account_added)(added));
            hosts
        };
        self.set_forge_hosts(forge_id, hosts);
    }

    /// Removes `account` from `forge` and signs out of it, so its token does
    /// not stay in the secret storage.
    pub fn remove_forge_account(&self, forge_id: &str, account: &str) {
        let hosts = {
            let inner = self.borrow();
            let Some(forge) = inner.ext.forges().iter().find(|f| f.id == forge_id) else {
                return;
            };
            let mut hosts = forge.hosts.clone();
            hosts.retain(|host| host != account);
            hosts
        };
        self.forget_forge_token(forge_id, account);
        self.set_forge_hosts(forge_id, hosts);
        self.toast_done((self.tr().toast_account_removed)(account));
    }

    /// Gives `forge` the accounts `hosts`, saves them and checks again.
    fn set_forge_hosts(&self, forge_id: &str, hosts: Vec<String>) {
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            let default = (inner.ext.forges().iter())
                .find(|f| f.id == forge_id)
                .is_some_and(|f| f.default_hosts == hosts);
            if default {
                inner.settings.forge_hosts.remove(forge_id);
            } else {
                inner
                    .settings
                    .forge_hosts
                    .insert(forge_id.to_owned(), hosts);
            }
            inner.ext.set_forge_hosts(&inner.settings.forge_hosts);
            inner.forges.next_check = Instant::now();
            inner.forges.stale = true;
            inner.dirty.settings = true;
            inner.dirty.sidebar = true;
        }
        self.save_settings();
    }

    /// Stores a token as if the user had signed in, without the secret
    /// storage or a request.
    #[cfg(test)]
    pub fn sign_in_forge_for_test(&self, forge: &str, host: &str, token: &str, user: &str) {
        self.forge_done(Done::SignedIn {
            key: account_key(forge, host),
            token: token.to_owned(),
            result: Ok(user.to_owned()),
        });
    }

    /// Ends a sign-in as if the forge refused the token.
    #[cfg(test)]
    pub fn fail_forge_sign_in_for_test(&self, forge: &str, account: &str, error: &str) {
        let done = Done::SignedIn {
            key: account_key(forge, account),
            token: String::new(),
            result: Err(error.to_owned()),
        };
        self.borrow().forges.send(done);
    }

    /// Makes the next poll check the forges.
    #[cfg(test)]
    pub fn check_forges_now(&self) {
        let mut inner = self.borrow_mut();
        inner.forges.next_check = Instant::now();
        inner.forges.asked.clear();
    }

    /// The Markdown of a rendered discussion view.
    #[cfg(test)]
    pub fn discussion_for_test(&self, key: &str) -> Option<String> {
        match self.borrow().forges.pages.get(key)? {
            Page::Ready(rendered) => Some(rendered.markdown()),
            other => Some(format!("{other:?}")),
        }
    }

    /// The `Authorization` header a request to `account` of `forge` gets.
    #[cfg(test)]
    pub fn forge_auth_for_test(&self, forge: &str, account: &str) -> Option<(String, String)> {
        let inner = self.borrow();
        let info = inner.ext.forges().iter().find(|f| f.id == forge)?;
        let token = inner.forges.token(info, account)?;
        Some(auth_header(info, &token))
    }
}

/// The name of the token's owner from `whoami`, or an empty name when the
/// forge has no such URL. An answer outside 2xx refuses the token.
fn verify(
    agent: &ureq::Agent,
    forge: &ForgeInfo,
    token: &str,
    whoami: Option<(String, Option<String>)>,
) -> Result<String, String> {
    let Some((url, field)) = whoami else {
        return Ok(String::new());
    };
    let request = HttpRequest {
        method: "GET".into(),
        url,
        headers: Vec::new(),
        body: None,
        account: None,
        done: None,
    };
    let response = send_http(agent, &request, Some(&auth_header(forge, token)));
    if let Some(error) = response.error {
        return Err(error);
    }
    match response.status {
        200..=299 => Ok(field
            .and_then(|field| json_field(&response.body, &field))
            .unwrap_or_default()),
        401 | 403 => Err(format!(
            "the forge refused the token (HTTP {})",
            response.status
        )),
        code => Err(format!("HTTP {code}")),
    }
}

/// The key of the discussion view of worktree `path` of project `project`.
pub fn discussion_key(project: u64, path: &std::path::Path) -> String {
    format!("{project}:{}", path.display())
}

/// The project and worktree of a discussion view key.
fn parse_key(key: &str) -> Option<(u64, PathBuf)> {
    let (id, path) = key.split_once(':')?;
    Some((id.parse().ok()?, PathBuf::from(path)))
}

/// What the control socket reports of the change request of `worktree`.
fn change_report(
    project: &str,
    worktree: &Path,
    branch: Option<&str>,
    remote: Option<&RemoteKey>,
    report: Option<&BranchReport>,
    page: Option<&Rendered>,
) -> serde_json::Value {
    use serde_json::{Value, json};
    let state = match (branch, remote, report) {
        (None, ..) => "no_branch",
        (_, None, _) => "unsupported",
        (_, _, None) => "unknown",
        (_, _, Some(report)) => match report.state {
            BranchState::None => "none",
            BranchState::Open => "open",
            BranchState::Merged => "merged",
            BranchState::Closed => "closed",
        },
    };
    let mut out = json!({
        "project": project,
        "worktree": worktree,
        "branch": branch,
        "forge": remote.map(|r| r.forge.as_str()),
        "state": state,
    });
    if let Some(report) = report {
        out["number"] = json!(report.number);
        out["title"] = json!(report.title);
        out["url"] = json!(report.url);
    }
    let Some(page) = page else { return out };
    let check = |state: crate::ext::CheckState| match state {
        crate::ext::CheckState::Success => "success",
        crate::ext::CheckState::Skipped => "skipped",
        crate::ext::CheckState::Canceled => "canceled",
        crate::ext::CheckState::Pending => "pending",
        crate::ext::CheckState::Running => "running",
        crate::ext::CheckState::Failed => "failed",
    };
    let discussion = &page.discussion;
    let comments =
        || (discussion.comments.iter()).chain(discussion.threads.iter().flat_map(|t| &t.comments));
    let last_activity = comments()
        .filter_map(|c| c.at.as_deref())
        .chain(discussion.pushes.iter().filter_map(|p| p.at.as_deref()))
        .max();
    let reviews: Vec<Value> = (discussion.reviews.iter())
        .map(|review| {
            let verdict = match review.verdict {
                crate::ext::Verdict::Pending => "pending",
                crate::ext::Verdict::Commented => "commented",
                crate::ext::Verdict::Approved => "approved",
                crate::ext::Verdict::Changes => "changes_requested",
            };
            json!({ "author": review.author, "verdict": verdict })
        })
        .collect();
    let checks: Vec<Value> = (page.checks.iter())
        .map(|c| json!({ "name": c.name, "state": check(c.state), "url": c.url }))
        .collect();
    out["details"] = json!({
        "draft": discussion.draft,
        "conflicts": discussion.conflicts,
        "target": discussion.target,
        "author": discussion.author,
        "reviews": reviews,
        // The worst state of all checks.
        "checks_state": page.checks.iter().map(|c| c.state).max().map(check),
        "checks": checks,
        "head": discussion.pushes.last().map(|p| p.commit.as_str()),
        "comments": comments().count(),
        "open_threads": discussion.threads.iter().filter(|t| !t.resolved).count(),
        "last_activity": last_activity,
    });
    out
}

impl Inner {
    /// The change request of every worktree, as the control socket reports
    /// it to agents.
    fn change_entries(&self) -> Vec<crate::control::ChangeEntry> {
        let mut entries = Vec::new();
        for project in &self.state.projects {
            let Some(snapshot) = self.snapshots.get(&project.id) else {
                continue;
            };
            if project.is_global() {
                continue;
            }
            let remote = self.remote_key(project.id);
            for worktree in snapshot.worktrees.iter().filter(|w| !w.bare) {
                let branch = worktree.branch.as_deref();
                let report = branch
                    .and_then(|branch| self.branch_change(project.id, remote.as_ref()?, branch));
                let key = discussion_key(project.id, &worktree.path);
                // Only a page of the branch's current change request.
                let page = match self.forges.pages.get(&key) {
                    Some(Page::Ready(page))
                        if Some(page.branch.as_str()) == branch
                            && report.as_ref().and_then(|r| r.number) == Some(page.number) =>
                    {
                        Some(Arc::clone(page))
                    }
                    _ => None,
                };
                let value = change_report(
                    &project.name,
                    &worktree.path,
                    branch,
                    remote.as_ref(),
                    report.as_ref(),
                    page.as_deref(),
                );
                entries.push(crate::control::ChangeEntry {
                    project: project.id,
                    worktree: worktree.path.clone(),
                    branch: worktree.branch.clone(),
                    report: value,
                    page,
                });
            }
        }
        entries
    }
}

impl Workspace {
    /// Hands the control socket the change request of every worktree.
    fn publish_changes(&self) {
        let (control, entries) = {
            let mut inner = self.borrow_mut();
            inner.forges.stale = false;
            (inner.running_control(), inner.change_entries())
        };
        if let Some(control) = control {
            control.publish_changes(entries);
        }
    }

    /// Starts the watches agents asked for since the last poll: the next
    /// poll checks the forges, and each worktree's discussion is asked for.
    fn start_watches(&self) {
        let Some(control) = self.borrow().running_control() else {
            return;
        };
        let fresh = control.take_new_watches();
        if fresh.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut inner = self.borrow_mut();
        inner.forges.now();
        for (project, path) in fresh {
            let key = discussion_key(project, &path);
            inner.forges.watch_asked.insert(key.clone(), now);
            inner.ask_discussion(&key);
        }
    }

    /// Asks again for the discussion of each watched worktree with a change
    /// request that `due` picks, given its forge, repository and view key.
    fn ask_watched_discussions(&self, due: impl Fn(&str, &(String, String), &str, &Inner) -> bool) {
        let Some(control) = self.borrow().running_control() else {
            return;
        };
        let now = Instant::now();
        for (project, path) in control.watched() {
            let key = discussion_key(project, &path);
            let mut inner = self.borrow_mut();
            let Some(remote) = inner.remote_key(project) else {
                continue;
            };
            let numbered = (inner.snapshots.get(&project))
                .and_then(|s| s.worktrees.iter().find(|w| w.path == path))
                .and_then(|w| w.branch.as_deref())
                .and_then(|branch| inner.branch_change(project, &remote, branch))
                .is_some_and(|change| change.number.is_some());
            let repo = (remote.account.clone(), remote.path.clone());
            if !numbered || !due(&remote.forge, &repo, &key, &inner) {
                continue;
            }
            inner.forges.watch_asked.insert(key.clone(), now);
            inner.ask_discussion(&key);
        }
    }
}

impl Inner {
    /// Asks the forge for the discussion of the change request behind view
    /// key `key`. A page shown already, or saved by an earlier run, stays
    /// until the answer comes.
    pub(crate) fn ask_discussion(&mut self, key: &str) {
        if !self.forges.pages.contains_key(key)
            && let Some(saved) = load_page(&self.forges.page_dir, key, self.tr)
        {
            self.forges.pages.insert(key.to_owned(), Page::Ready(saved));
        }
        self.forges.waiting.remove(key);
        let set = |inner: &mut Self, page: Page| {
            if page == Page::Waiting {
                inner.forges.waiting.insert(key.to_owned());
            }
            if !matches!(inner.forges.pages.get(key), Some(Page::Ready(_))) {
                inner.forges.pages.insert(key.to_owned(), page);
            }
        };
        let Some((project, path)) = key
            .split_once(':')
            .and_then(|(id, path)| Some((id.parse::<u64>().ok()?, PathBuf::from(path))))
        else {
            return set(self, Page::Unsupported);
        };
        let Some((env, known)) = (self.state.project(project))
            .map(|project| (project.env.clone(), project.known_repo()))
        else {
            return set(self, Page::Unsupported);
        };
        // Every worktree's change request, not only those the Agents tab
        // lists: an explore panel shows the page for any worktree.
        let Some(worktree) = self
            .snapshots
            .get(&project)
            .and_then(|s| s.worktrees.iter().find(|w| w.path == path))
        else {
            return set(self, Page::Waiting);
        };
        // The saved page of a branch that is no longer checked out goes.
        let saved = match self.forges.pages.get(key) {
            Some(Page::Ready(page)) => Some((page.branch.as_str(), page.number)),
            _ => None,
        };
        let saved = match saved {
            Some((shown, number)) if worktree.branch.as_deref() == Some(shown) => Some(number),
            Some(_) => {
                self.forges.pages.remove(key);
                None
            }
            None => None,
        };
        let Some(branch) = worktree.branch.clone() else {
            return set(self, Page::NoChange);
        };
        let Some(remote) = self.remote_key(project) else {
            return set(self, Page::Unsupported);
        };
        let discussions = (self.ext.forges().iter()).any(|f| f.id == remote.forge && f.discussions);
        if !discussions {
            return set(self, Page::Unsupported);
        }
        let number = match (self.branch_change(project, &remote, &branch), saved) {
            (Some(change), _) => match change.number {
                Some(number) => number,
                None => return set(self, Page::NoChange),
            },
            // Until the forge reports the branch, the saved page names its
            // change request.
            (None, Some(number)) => number,
            (None, None) => {
                // The next check asks the forge about this branch.
                self.forges.now();
                return set(self, Page::Waiting);
            }
        };
        let pipeline = self.snapshots.get(&project).and_then(|snapshot| {
            let index = snapshot.worktrees.iter().position(|w| w.path == path)?;
            snapshot.pipelines.get(index)?.clone()
        });
        if !self.tokens_read() {
            // Asked again once the saved tokens are read.
            if !self.forges.loaded {
                self.load_forge_tokens();
            }
            if !self.tokens_read() {
                return set(self, Page::Waiting);
            }
        }
        let signed_in = self
            .ext
            .forges()
            .iter()
            .find(|f| f.id == remote.forge)
            .is_some_and(|forge| self.forges.token(forge, &remote.account).is_some());
        self.forges.next_ask += 1;
        let id = self.forges.next_ask.to_string();
        let request = DiscussionRequest {
            id: id.clone(),
            account: remote.account,
            host: remote.host,
            path: remote.path,
            remote: remote.url,
            branch: branch.clone(),
            number,
            signed_in,
        };
        set(self, Page::Loading);
        match self.ext.forge_discussion(&remote.forge, &request) {
            Ok(true) => {
                self.forges.asks.insert(
                    id,
                    Ask {
                        key: key.to_owned(),
                        env,
                        worktree: path,
                        known,
                        branch,
                        number,
                        pipeline,
                    },
                );
            }
            Ok(false) => {
                self.forges.pages.insert(key.to_owned(), Page::Unsupported);
            }
            Err(error) => {
                self.forges
                    .pages
                    .insert(key.to_owned(), Page::Failed(error.to_string()));
                self.push_notice(error.to_string());
            }
        }
    }

    /// Shows the discussion of view key `key` in `panel`, asking the forge
    /// first when nothing was asked yet, such as for a panel restored from
    /// before the explore panel showed change requests.
    pub(crate) fn render_discussion(&mut self, panel: &crate::files::FilesPanel, key: &str) {
        let page = self.change_page(key);
        let title = key.split_once(':').map_or_else(
            || key.to_owned(),
            |(_, path)| crate::search::folder_name(std::path::Path::new(path)),
        );
        match page_message(self.tr, &page) {
            Some(message) => panel.show_message(&title, message),
            None => {
                if let Page::Ready(rendered) = &page {
                    panel.show_markdown(&rendered.title, &rendered.markdown());
                }
            }
        }
    }

    /// The change request page of view key `key`, asking the forge first
    /// when nothing was asked yet.
    pub(crate) fn change_page(&mut self, key: &str) -> Page {
        if !self.forges.pages.contains_key(key) {
            self.ask_discussion(key);
        }
        self.forges.pages.get(key).cloned().unwrap_or(Page::Waiting)
    }
}

/// A rendered page as saved in the cache directory.
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedPage<P> {
    version: u32,
    key: String,
    /// The language its strings are in.
    language: String,
    page: P,
}

/// Bumped when [`Rendered`] changes shape, so older files are not read.
const SAVED_VERSION: u32 = 1;

/// The file the page of view key `key` is saved in.
fn page_path(dir: &Path, key: &str) -> PathBuf {
    // FNV-1a: the key holds a path, which is no file name.
    let hash = (key.bytes()).fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    dir.join(format!("{hash:016x}.json"))
}

/// The page saved for view key `key`, when it is in the language of `tr`.
fn load_page(dir: &Path, key: &str, tr: &Strings) -> Option<Arc<Rendered>> {
    let text = std::fs::read(page_path(dir, key)).ok()?;
    let saved: SavedPage<Rendered> = serde_json::from_slice(&text).ok()?;
    let language = crate::i18n::Lang::of(tr).id();
    (saved.version == SAVED_VERSION && saved.key == key && saved.language == language)
        .then(|| Arc::new(saved.page))
}

/// Saves `page` for view key `key`. A failure only costs the next start
/// its early page.
fn save_page(dir: &Path, key: &str, tr: &Strings, page: &Rendered) {
    let saved = SavedPage {
        version: SAVED_VERSION,
        key: key.to_owned(),
        language: crate::i18n::Lang::of(tr).id().to_owned(),
        page,
    };
    let Ok(text) = serde_json::to_vec(&saved) else {
        return;
    };
    let path = page_path(dir, key);
    let partial = path.with_extension("json.part");
    let _ = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&partial, text))
        .and_then(|()| std::fs::rename(&partial, &path));
}

/// The message a page that is not ready shows instead.
pub(crate) fn page_message(tr: &Strings, page: &Page) -> Option<String> {
    match page {
        Page::Ready(_) => None,
        Page::Waiting => Some(tr.discussion_waiting.to_owned()),
        Page::NoChange => Some(tr.discussion_no_change.to_owned()),
        Page::Loading => Some(tr.discussion_loading.to_owned()),
        Page::Unsupported => Some(tr.discussion_unsupported.to_owned()),
        Page::Failed(error) => Some((tr.discussion_failed)(error)),
    }
}

impl Workspace {
    /// Shows the change request of worktree `path` of project `project` on
    /// the change request page of an explore panel of that worktree.
    pub fn open_discussion(&self, project: u64, path: &std::path::Path) {
        self.activate_worktree(project, path);
        let key = discussion_key(project, path);
        self.borrow_mut().ask_discussion(&key);
        self.open_change();
    }

    /// Pushes the change request page of view key `key` to the panels that
    /// show it: discussion views and the explore panels of its worktree.
    pub(crate) fn refresh_discussion(&self, key: &str) {
        self.refresh_views(Some(DISCUSSION_VIEW), Some(key));
        let Some((project, path)) = key
            .split_once(':')
            .and_then(|(id, path)| Some((id.parse::<u64>().ok()?, PathBuf::from(path))))
        else {
            return;
        };
        let (panels, page, tr) = {
            let inner = self.borrow();
            let panels: Vec<_> = inner
                .live_get(project, &path)
                .map(|live| {
                    live.panels
                        .iter()
                        .filter_map(|(_, panel)| match panel {
                            crate::workspace::LivePanel::Explore(explore) => Some(explore.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let page = inner.forges.pages.get(key).cloned();
            (panels, page, inner.tr)
        };
        let Some(page) = page else { return };
        for explore in panels {
            explore.set_change(&page, tr);
        }
    }

    /// Takes a forge's answer to a discussion request: renders it on a
    /// thread, where git places its threads on the worktree's head.
    pub(crate) fn forge_discussion_done(&self, id: &str, discussion: Result<Discussion, String>) {
        let mut inner = self.borrow_mut();
        let Some(ask) = inner.forges.asks.remove(id) else {
            return;
        };
        let discussion = match discussion {
            Ok(discussion) => discussion,
            Err(error) => {
                let ready = matches!(inner.forges.pages.get(&ask.key), Some(Page::Ready(_)));
                if ready {
                    let text = (inner.tr.discussion_failed)(&error);
                    inner.push_notice(text);
                } else {
                    inner
                        .forges
                        .pages
                        .insert(ask.key.clone(), Page::Failed(error));
                }
                drop(inner);
                self.refresh_discussion(&ask.key);
                return;
            }
        };
        let (tr, sender, wake) = (
            inner.tr,
            inner.forges.done.0.clone(),
            Arc::clone(&inner.forges.wake),
        );
        let page_dir = inner.forges.page_dir.clone();
        drop(inner);
        let _ = std::thread::Builder::new()
            .name("discussion".into())
            .spawn(move || {
                let rendered = crate::discussion::render(
                    tr,
                    &ask.env,
                    &ask.worktree,
                    ask.known.as_ref(),
                    &ask.branch,
                    ask.number,
                    &discussion,
                    ask.pipeline.as_ref(),
                );
                save_page(&page_dir, &ask.key, tr, &rendered);
                let _ = sender.send(Done::Discussion(ask.key, Arc::new(rendered)));
                if let Some(signal) = wake.get() {
                    signal.request_redraw();
                }
            });
    }

    /// Asks again for the discussions that waited for a repository or a
    /// branch's change request.
    pub(crate) fn ask_waiting_discussions(&self) {
        let waiting: Vec<String> = self.borrow().forges.waiting.iter().cloned().collect();
        for key in waiting {
            let page = {
                let mut inner = self.borrow_mut();
                inner.ask_discussion(&key);
                inner.forges.pages.get(&key).cloned()
            };
            if page != Some(Page::Waiting) {
                self.refresh_discussion(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forge(value: &str) -> ForgeInfo {
        ForgeInfo {
            id: "github.github".into(),
            hosts: vec!["github.com".into()],
            auth_header: "Authorization".into(),
            auth_value: value.into(),
            ..ForgeInfo::default()
        }
    }

    #[test]
    fn tokens_only_go_to_their_host_and_its_subdomains() {
        assert!(may_send_token(
            "github.com",
            "https://api.github.com/graphql"
        ));
        assert!(may_send_token("github.com", "https://github.com/x"));
        assert!(may_send_token("git.corp", "https://user@git.corp:8443/api"));
        assert!(!may_send_token("github.com", "https://evilgithub.com/"));
        assert!(!may_send_token(
            "github.com",
            "https://github.com.evil.org/"
        ));
        assert!(!may_send_token("github.com", "ftp://github.com/"));
    }

    #[test]
    fn auth_values_fill_in_the_token() {
        assert_eq!(auth_header(&forge("Bearer {token}"), "abc").1, "Bearer abc");
        // Azure DevOps: Basic with an empty user name.
        assert_eq!(
            auth_header(&forge("Basic {basic}"), "secret").1,
            "Basic OnNlY3JldA=="
        );
        // Bitbucket: Basic with the `email:token` the user pasted.
        assert_eq!(
            auth_header(&forge("Basic {base64}"), "a@b.c:t").1,
            "Basic YUBiLmM6dA=="
        );
    }

    #[test]
    fn whoami_fields_are_dotted_paths() {
        let body = r#"{"login":"octo","authenticatedUser":{"providerDisplayName":"Octo Cat"}}"#;
        assert_eq!(json_field(body, "login").as_deref(), Some("octo"));
        assert_eq!(
            json_field(body, "authenticatedUser.providerDisplayName").as_deref(),
            Some("Octo Cat")
        );
        assert_eq!(json_field(body, "missing"), None);
    }

    #[test]
    fn aliases_map_remote_hosts_to_accounts() {
        let mut info = forge("Basic {basic}");
        info.hosts = vec!["dev.azure.com".into()];
        info.aliases = vec![
            ("*.visualstudio.com".into(), "dev.azure.com".into()),
            ("ssh.dev.azure.com".into(), "dev.azure.com".into()),
        ];
        let path = "org/project/_git/repo";
        assert_eq!(
            info.account_for("dev.azure.com", path),
            Some("dev.azure.com")
        );
        assert_eq!(
            info.account_for("ssh.dev.azure.com", "v3/org/project/repo"),
            Some("dev.azure.com")
        );
        assert_eq!(
            info.account_for("org.visualstudio.com", "project/_git/repo"),
            Some("dev.azure.com")
        );
        assert_eq!(info.account_for("visualstudio.com", path), None);
        assert_eq!(info.account_for("github.com", path), None);
    }

    #[test]
    fn accounts_with_a_path_take_the_repositories_under_it() {
        let mut info = forge("Bearer {token}");
        info.hosts = vec![
            "github.com".into(),
            "github.com/acme".into(),
            "github.com/acme/team".into(),
            "git.corp/ops".into(),
        ];
        assert_eq!(info.account_for("github.com", "octo/x"), Some("github.com"));
        assert_eq!(
            info.account_for("github.com", "Acme/x"),
            Some("github.com/acme")
        );
        assert_eq!(
            info.account_for("github.com", "acme/team/x"),
            Some("github.com/acme/team")
        );
        // A segment, not a string prefix.
        assert_eq!(
            info.account_for("github.com", "acmeco/x"),
            Some("github.com")
        );
        assert_eq!(info.account_for("git.corp", "ops/x"), Some("git.corp/ops"));
        assert_eq!(info.account_for("git.corp", "dev/x"), None);
    }

    #[test]
    fn aliases_rewrite_paths_for_accounts_with_a_path() {
        let mut info = forge("Basic {basic}");
        info.hosts = vec![
            "dev.azure.com/contoso".into(),
            "dev.azure.com/fabrikam".into(),
        ];
        info.aliases = vec![
            ("*.visualstudio.com".into(), "dev.azure.com/*".into()),
            (
                "*.visualstudio.com/defaultcollection".into(),
                "dev.azure.com/*".into(),
            ),
            ("ssh.dev.azure.com".into(), "dev.azure.com".into()),
            ("ssh.dev.azure.com/v3".into(), "dev.azure.com".into()),
        ];
        assert_eq!(
            info.account_for("dev.azure.com", "contoso/p/_git/r"),
            Some("dev.azure.com/contoso")
        );
        assert_eq!(
            info.account_for("ssh.dev.azure.com", "v3/fabrikam/p/r"),
            Some("dev.azure.com/fabrikam")
        );
        assert_eq!(
            info.account_for("contoso.visualstudio.com", "DefaultCollection/p/_git/r"),
            Some("dev.azure.com/contoso")
        );
        assert_eq!(
            info.account_for("fabrikam.visualstudio.com", "p/_git/r"),
            Some("dev.azure.com/fabrikam")
        );
        assert_eq!(info.account_for("dev.azure.com", "other/p/_git/r"), None);
    }

    #[test]
    fn env_tokens_cover_every_account_on_the_first_host() {
        let mut info = forge("Bearer {token}");
        info.hosts = vec![
            "github.com/acme".into(),
            "github.com".into(),
            "ghe.corp".into(),
        ];
        // Set wherever tests run.
        info.env = vec!["PATH".into()];
        assert!(env_token(&info, "github.com").is_some());
        assert!(env_token(&info, "github.com/acme").is_some());
        assert!(env_token(&info, "ghe.corp").is_none());
    }

    #[test]
    fn ls_remote_hashes_change_only_when_a_watched_ref_moves() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&["branch", "fix"]);
        git(&["branch", "other"]);
        let gate = Gate {
            account: "a".into(),
            path: "p".into(),
            filter: true,
            env: cmdterm_env::Environment::Local,
            cwd: dir.path().to_owned(),
            known: None,
            url: dir.path().display().to_string(),
            branches: vec!["fix".into()],
        };
        let first = ls_remote(&gate).unwrap();
        git(&["commit", "-q", "--allow-empty", "-m", "two"]);
        git(&["checkout", "-q", "other"]);
        git(&["commit", "-q", "--allow-empty", "-m", "elsewhere"]);
        git(&["checkout", "-q", "main"]);
        // main moved: HEAD is watched.
        let second = ls_remote(&gate).unwrap();
        assert_ne!(first, second);
        // `other` is not watched.
        git(&["branch", "-f", "other", "main"]);
        assert_eq!(ls_remote(&gate), Some(second));
        let missing = Gate {
            url: dir.path().join("nothing").display().to_string(),
            ..gate
        };
        assert_eq!(ls_remote(&missing), None);
    }
}
