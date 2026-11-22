//! Runs VCS and forge queries off the UI thread.
//!
//! The UI sends [`Job`]s and drains [`Reply`]s after the worker wakes it. The
//! worker also re-polls the watched project every [`POLL`], so worktree status
//! stays current without UI timers. A reply that matches the previous one for a
//! project is dropped here, so an unchanged poll never touches the UI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::forge::{ChangeRequest, ForgeClient, ForgeHints, ForgeRepo, Pipeline};
use crate::vcs::{KnownRepo, Repository, Status, VcsError, Worktree};
use cmdterm_env::Environment;

const POLL: Duration = Duration::from_secs(5);
/// Forge APIs are rate limited; their data changes slowly.
const FORGE_POLL: Duration = Duration::from_secs(60);

pub enum Job {
    /// Find the repository at `path`; `input` is the text the user typed.
    /// `jj` asks Jujutsu first.
    Discover {
        input: String,
        env: Environment,
        path: PathBuf,
        jj: bool,
    },
    /// Find the peer of a VPN location, then the repository there.
    /// `ssh_program` replaces `ssh` when the peer is reached over SSH.
    DiscoverVpn {
        input: String,
        location: Box<crate::vpn::VpnLocation>,
        scan: crate::vpn::ScanConfig,
        ssh_program: Option<String>,
        jj: bool,
    },
    /// Clone `url` into a new directory under `parent`, on a thread of its
    /// own so a long clone does not hold up polling, then discover it.
    Clone {
        input: String,
        env: Environment,
        url: String,
        parent: PathBuf,
        devcontainer: bool,
        jj: bool,
    },
    /// Find the folder of a global session; `input` is the text the user
    /// typed.
    OpenGlobal {
        input: String,
        env: Environment,
        path: PathBuf,
    },
    /// Stop polling the watched project, when a global session is shown.
    Unwatch,
    /// Start polling `project` (replaces the previous watch) and refresh now.
    /// `jj` reads it with Jujutsu where it has a jj repository; `known` is
    /// the git repository recorded for it, see [`crate::vcs::open`].
    Watch {
        project: u64,
        env: Environment,
        root: PathBuf,
        jj: bool,
        known: Option<KnownRepo>,
    },
    /// Refresh `project` once without changing the watch, so the rail can
    /// list the worktrees of projects that are not selected.
    Refresh {
        project: u64,
        env: Environment,
        root: PathBuf,
        jj: bool,
        known: Option<KnownRepo>,
    },
    /// Add a worktree at `path`. `branch` is checked out when it exists and
    /// created at `from` otherwise; `None` leaves the worktree detached at
    /// `from`. `from` defaults to the main worktree's revision and may only
    /// be given for a new branch or a detached worktree.
    AddWorktree {
        project: u64,
        repo: Repository,
        path: PathBuf,
        branch: Option<String>,
        from: Option<String>,
    },
    /// Add a worktree at `source`'s checked-out revision. `branch` is checked
    /// out when it exists and created otherwise; `None` leaves it detached.
    AddWorktreeFrom {
        project: u64,
        repo: Repository,
        source: PathBuf,
        path: PathBuf,
        branch: Option<String>,
    },
    RemoveWorktree {
        project: u64,
        repo: Repository,
        path: PathBuf,
    },
    /// Fetch, pull or push in the worktree `path`, on a thread of its own
    /// so a slow remote does not hold up polling.
    Remote {
        project: u64,
        repo: Repository,
        path: PathBuf,
        op: RemoteOp,
    },
}

/// What [`Job::Remote`] does with a worktree's remotes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteOp {
    /// Fetch from every remote.
    Fetch,
    /// Bring the checked-out branch up to date with its upstream.
    Pull,
    /// Push the checked-out branch to the named remote, or to the one it
    /// pushes to by default.
    Push(Option<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub repo: Repository,
    pub worktrees: Vec<Worktree>,
    /// Status per worktree, parallel to `worktrees`.
    pub status: Vec<Option<Status>>,
    pub forge: Option<ForgeRepo>,
    pub change_requests: Vec<ChangeRequest>,
    /// Latest pipeline per worktree, parallel to `worktrees`.
    pub pipelines: Vec<Option<Pipeline>>,
    /// Web page of the forge project; known without a token.
    pub forge_url: Option<String>,
    /// URL of `origin`, else of the first remote.
    pub remote_url: Option<String>,
    /// Names of the remotes (Mercurial: paths), `origin` first.
    pub remotes: Vec<String>,
    /// Worktrees whose `.git` no longer leads to the repository's metadata,
    /// see [`Repository::replaced_dot_git`].
    pub replaced: Vec<PathBuf>,
}

pub enum Reply {
    Discovered {
        input: String,
        env: Environment,
        repo: Repository,
        /// Run the main worktree's terminals in its devcontainer.
        devcontainer: bool,
    },
    DiscoverFailed {
        input: String,
        message: String,
    },
    /// The absolute folder of the global session asked for with `input`.
    GlobalOpened {
        input: String,
        env: Environment,
        root: PathBuf,
    },
    Snapshot {
        project: u64,
        snapshot: Box<Snapshot>,
    },
    Error {
        project: u64,
        message: String,
    },
    WorktreeAdded {
        project: u64,
        path: PathBuf,
    },
    /// A [`Job::Remote`] ended: with the remote a push went to, or with
    /// the line that says why it failed.
    RemoteDone {
        project: u64,
        path: PathBuf,
        op: RemoteOp,
        result: Result<Option<String>, String>,
    },
}

pub struct VcsWorker {
    jobs: Sender<Job>,
    replies: Receiver<Reply>,
}

impl VcsWorker {
    /// `wake` runs on the worker thread after each reply is queued.
    pub fn spawn(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (jobs, job_rx) = mpsc::channel();
        let (reply_tx, replies) = mpsc::channel();
        std::thread::Builder::new()
            .name("cmdterm-vcs".into())
            .spawn(move || run(job_rx, reply_tx, Arc::new(wake)))
            .expect("spawn vcs worker");
        Self { jobs, replies }
    }

    pub fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    pub fn try_recv(&self) -> Option<Reply> {
        self.replies.try_recv().ok()
    }
}

struct Watched {
    project: u64,
    env: Environment,
    root: PathBuf,
    jj: bool,
    known: Option<KnownRepo>,
}

#[derive(Default)]
struct ForgeCache {
    fetched: Option<Instant>,
    branch_requests: Vec<ChangeRequest>,
    /// Latest pipeline per branch.
    branch_pipelines: HashMap<String, Pipeline>,
    /// Browser URL of the detected forge repo, rebuilt when the repo changes.
    web_url: Option<(ForgeRepo, String)>,
}

fn run(jobs: Receiver<Job>, replies: Sender<Reply>, wake: Arc<dyn Fn() + Send + Sync>) {
    let mut watched: Option<Watched> = None;
    let mut last: HashMap<u64, Snapshot> = HashMap::new();
    let mut forge_cache: HashMap<u64, ForgeCache> = HashMap::new();
    let mut next_poll = Instant::now() + POLL;
    let send = |reply: Reply| {
        if replies.send(reply).is_ok() {
            wake();
        }
    };
    loop {
        let timeout = next_poll.saturating_duration_since(Instant::now());
        let job = match jobs.recv_timeout(timeout) {
            Ok(job) => Some(job),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let target = match job {
            Some(Job::DiscoverVpn {
                input,
                location,
                scan,
                ssh_program,
                jj,
            }) => {
                match location.resolve(&scan) {
                    Ok(mut env) => {
                        if let (Environment::Ssh(target), Some(program)) = (&mut env, ssh_program) {
                            target.program = Some(program);
                        }
                        match crate::vcs::discover_with(&env, &location.path, jj) {
                            Ok(repo) => send(Reply::Discovered {
                                input,
                                env,
                                repo,
                                devcontainer: false,
                            }),
                            Err(error) => send(Reply::DiscoverFailed {
                                input,
                                message: error.to_string(),
                            }),
                        }
                    }
                    Err(message) => send(Reply::DiscoverFailed { input, message }),
                }
                None
            }
            Some(Job::Discover {
                input,
                env,
                path,
                jj,
            }) => {
                match crate::vcs::discover_with(&env, &path, jj) {
                    Ok(repo) => send(Reply::Discovered {
                        input,
                        env,
                        repo,
                        devcontainer: false,
                    }),
                    Err(error) => send(Reply::DiscoverFailed {
                        input,
                        message: error.to_string(),
                    }),
                }
                None
            }
            Some(Job::Clone {
                input,
                env,
                url,
                parent,
                devcontainer,
                jj,
            }) => {
                let replies = replies.clone();
                let wake = wake.clone();
                let _ = std::thread::Builder::new()
                    .name("cmdterm-clone".into())
                    .spawn(move || {
                        let reply = match crate::vcs::clone_with(&env, &url, &parent, jj) {
                            Ok(repo) => Reply::Discovered {
                                input,
                                env,
                                repo,
                                devcontainer,
                            },
                            Err(error) => Reply::DiscoverFailed {
                                input,
                                message: error.to_string(),
                            },
                        };
                        if replies.send(reply).is_ok() {
                            wake();
                        }
                    });
                None
            }
            Some(Job::OpenGlobal { input, env, path }) => {
                send(match crate::vcs::resolve_folder(&env, &path) {
                    Ok(root) => Reply::GlobalOpened { input, env, root },
                    Err(error) => Reply::DiscoverFailed {
                        input,
                        message: error.to_string(),
                    },
                });
                None
            }
            Some(Job::Unwatch) => {
                watched = None;
                None
            }
            Some(Job::Watch {
                project,
                env,
                root,
                jj,
                known,
            }) => {
                watched = Some(Watched {
                    project,
                    env: env.clone(),
                    root: root.clone(),
                    jj,
                    known: known.clone(),
                });
                Some((project, env, root, jj, known))
            }
            Some(Job::Refresh {
                project,
                env,
                root,
                jj,
                known,
            }) => Some((project, env, root, jj, known)),
            Some(Job::AddWorktree {
                project,
                repo,
                path,
                branch,
                from,
            }) => {
                let exists = branch.as_ref().is_some_and(|branch| {
                    repo.branches()
                        .is_ok_and(|branches| branches.contains(branch))
                });
                let added = match (&branch, &from) {
                    (Some(branch), Some(_)) if exists => Err(VcsError::InvalidArgument(format!(
                        "branch {branch} exists; a start revision only applies to a new branch"
                    ))),
                    (Some(branch), None) => repo.add_worktree(&path, branch, !exists),
                    (branch, Some(from)) => repo.add_worktree_at(&path, from, branch.as_deref()),
                    (None, None) => repo.add_worktree_from(&repo.root, &path, None),
                };
                match added {
                    Ok(worktree) => send(Reply::WorktreeAdded {
                        project,
                        path: worktree.path,
                    }),
                    Err(error) => send(Reply::Error {
                        project,
                        message: format!("could not add worktree: {error}"),
                    }),
                }
                Some((
                    project,
                    repo.env.clone(),
                    repo.root.clone(),
                    jj(&repo),
                    repo.known(),
                ))
            }
            Some(Job::AddWorktreeFrom {
                project,
                repo,
                source,
                path,
                branch,
            }) => {
                let exists = branch.as_ref().is_some_and(|branch| {
                    repo.branches()
                        .is_ok_and(|branches| branches.contains(branch))
                });
                let added = match &branch {
                    Some(branch) if exists => repo.add_worktree(&path, branch, false),
                    _ => repo.add_worktree_from(&source, &path, branch.as_deref()),
                };
                match added {
                    Ok(worktree) => send(Reply::WorktreeAdded {
                        project,
                        path: worktree.path,
                    }),
                    Err(error) => send(Reply::Error {
                        project,
                        message: format!("could not add worktree: {error}"),
                    }),
                }
                Some((
                    project,
                    repo.env.clone(),
                    repo.root.clone(),
                    jj(&repo),
                    repo.known(),
                ))
            }
            Some(Job::RemoveWorktree {
                project,
                repo,
                path,
            }) => {
                match repo.remove_worktree(&path, false) {
                    Ok(()) if cfg!(windows) && repo.env.is_local() => remove_left_dir(&path),
                    Ok(()) => {}
                    Err(error) => send(Reply::Error {
                        project,
                        message: format!("could not remove worktree: {error}"),
                    }),
                }
                Some((
                    project,
                    repo.env.clone(),
                    repo.root.clone(),
                    jj(&repo),
                    repo.known(),
                ))
            }
            Some(Job::Remote {
                project,
                repo,
                path,
                op,
            }) => {
                let replies = replies.clone();
                let wake = wake.clone();
                let _ = std::thread::Builder::new()
                    .name("cmdterm-vcs-remote".into())
                    .spawn(move || {
                        let result = match &op {
                            RemoteOp::Fetch => repo.fetch(&path, None).map(|()| None),
                            RemoteOp::Pull => repo.pull(&path).map(|()| None),
                            RemoteOp::Push(remote) => repo.push(&path, remote.as_deref()).map(Some),
                        };
                        let reply = Reply::RemoteDone {
                            project,
                            path,
                            op,
                            result: result.map_err(|error| crate::vcs::reason(&error)),
                        };
                        if replies.send(reply).is_ok() {
                            wake();
                        }
                    });
                None
            }
            None => {
                next_poll = Instant::now() + POLL;
                watched.as_ref().map(|w| {
                    let known = w.known.clone();
                    (w.project, w.env.clone(), w.root.clone(), w.jj, known)
                })
            }
        };
        let Some((project, env, root, jj, known)) = target else {
            continue;
        };
        let forge = forge_cache.entry(project).or_default();
        match snapshot(&env, &root, jj, known.as_ref(), forge) {
            Ok(snapshot) => {
                if last.get(&project) != Some(&snapshot) {
                    last.insert(project, snapshot.clone());
                    send(Reply::Snapshot {
                        project,
                        snapshot: Box::new(snapshot),
                    });
                }
            }
            Err(message) => {
                last.remove(&project);
                send(Reply::Error { project, message });
            }
        }
    }
}

/// Whether the refresh after a worktree job asks Jujutsu: it does when the
/// repository the job ran on is one.
fn jj(repo: &Repository) -> bool {
    repo.kind == crate::vcs::VcsKind::Jujutsu
}

/// Removes the empty directory a removed worktree may leave on Windows,
/// which keeps a directory while it is the working directory of a process,
/// here a shell of the worktree that is still ending. Gives up after two
/// seconds; a directory that is not empty stays.
fn remove_left_dir(path: &Path) {
    for _ in 0..20 {
        if !path.exists() || std::fs::remove_dir(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn snapshot(
    env: &Environment,
    root: &Path,
    jj: bool,
    known: Option<&KnownRepo>,
    forge: &mut ForgeCache,
) -> Result<Snapshot, String> {
    let repo = crate::vcs::open(env, root, jj, known).map_err(|error| error.to_string())?;
    let worktrees = repo.worktrees().map_err(|error| error.to_string())?;
    let replaced = repo.replaced_dot_git();
    let status = worktrees
        .iter()
        .map(|worktree| {
            if worktree.bare || worktree.prunable {
                None
            } else {
                repo.status(&worktree.path).ok()
            }
        })
        .collect();
    let hints = ForgeHints::default();
    let mut remotes = repo.remotes().unwrap_or_default();
    // Prefer `origin`, else the first remote a forge recognizes.
    remotes.sort_by_key(|remote| remote.name != "origin");
    let remote_url = remotes.first().map(|remote| remote.url.clone());
    let remote_names = remotes.iter().map(|remote| remote.name.clone()).collect();
    let forge_repo = remotes
        .iter()
        .find_map(|remote| crate::forge::detect(&remote.url, &hints));
    if let Some(forge_repo) = &forge_repo {
        let stale = forge
            .fetched
            .is_none_or(|fetched| fetched.elapsed() >= FORGE_POLL);
        if stale {
            forge.fetched = Some(Instant::now());
            let token = crate::forge::token_from_env(forge_repo.kind, &forge_repo.host);
            if token.is_none() {
                // Data from token-backed calls goes away with the token.
                forge.branch_requests.clear();
                forge.branch_pipelines.clear();
            } else {
                let client = ForgeClient::new(forge_repo.clone(), token);
                if let Ok(requests) = client.open_change_requests() {
                    forge.branch_requests = requests;
                }
                for branch in worktrees.iter().filter_map(|w| w.branch.as_deref()) {
                    // An error keeps the previous value for that branch.
                    match client.branch_pipeline(branch) {
                        Ok(Some(pipeline)) => {
                            forge.branch_pipelines.insert(branch.to_owned(), pipeline);
                        }
                        Ok(None) => {
                            forge.branch_pipelines.remove(branch);
                        }
                        Err(_) => {}
                    }
                }
                forge.branch_pipelines.retain(|branch, _| {
                    worktrees
                        .iter()
                        .any(|w| w.branch.as_deref() == Some(branch.as_str()))
                });
            }
        }
    }
    let pipelines = worktrees
        .iter()
        .map(|worktree| {
            let branch = worktree.branch.as_deref()?;
            forge.branch_pipelines.get(branch).cloned()
        })
        .collect();
    let forge_url = match &forge_repo {
        Some(repo) => {
            if forge
                .web_url
                .as_ref()
                .is_none_or(|(cached, _)| cached != repo)
            {
                let url = ForgeClient::new(repo.clone(), None).web_url();
                forge.web_url = Some((repo.clone(), url));
            }
            forge.web_url.as_ref().map(|(_, url)| url.clone())
        }
        None => None,
    };
    Ok(Snapshot {
        repo,
        worktrees,
        status,
        forge: forge_repo,
        change_requests: forge.branch_requests.clone(),
        pipelines,
        forge_url,
        remote_url,
        remotes: remote_names,
        replaced,
    })
}
