//! Recurring extension tasks from `term.task`: when each is due, running
//! the due ones, and the timer that wakes an idle window for them.
//!
//! The last run of every task, per project, is saved in `state.json`, so a
//! daily task runs once a day across restarts. [`Workspace::poll`] compares
//! the next due time with the clock, which allocates nothing; everything
//! else here is cold.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::ext::{TaskInfo, TaskProject, TaskWorktree};
use schnellui::RedrawSignal;

use crate::workspace::{Inner, Workspace};

/// How long a due task waits for its project's first VCS snapshot, which
/// lists the worktrees, before it is looked at again.
const SNAPSHOT_RETRY: u64 = 60;

/// The longest the timer sleeps between two looks at the clock, so a task
/// that came due while the machine was suspended runs soon after it wakes.
const TIMER_STEP: Duration = Duration::from_secs(60);

/// Seconds since the Unix epoch.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// When the next task is due and the timer that wakes the window then.
#[derive(Default)]
pub(crate) struct Schedule {
    /// Unix seconds when [`Workspace::run_due_tasks`] should look again.
    /// `Some(0)` asks for a look on the next poll.
    due: Option<u64>,
    /// The extension host's tasks revision the schedule was made from.
    seen: Option<u64>,
    timer: Option<mpsc::Sender<Option<u64>>>,
}

impl Schedule {
    /// Whether the next poll should run [`Workspace::run_due_tasks`]: a
    /// task is due, or the tasks changed. Allocation-free.
    pub(crate) fn wants_a_look(&self, revision: u64, now: u64) -> bool {
        self.seen != Some(revision) || self.due.is_some_and(|due| now >= due)
    }

    /// Makes the next poll look at every task again, such as after a
    /// project was added or the extensions were reloaded.
    pub(crate) fn look_again(&mut self) {
        self.seen = None;
    }

    fn set(&mut self, due: Option<u64>, wake: &Arc<OnceLock<RedrawSignal>>) {
        self.due = due;
        if self.timer.is_none() && due.is_none() {
            return;
        }
        let timer = self.timer.get_or_insert_with(|| spawn_timer(wake.clone()));
        if timer.send(due).is_err() {
            self.timer = None;
        }
    }
}

/// Wakes the window once the deadline it was last sent has passed.
fn spawn_timer(wake: Arc<OnceLock<RedrawSignal>>) -> mpsc::Sender<Option<u64>> {
    let (sender, receiver) = mpsc::channel::<Option<u64>>();
    let _ = std::thread::Builder::new()
        .name("task-timer".into())
        .spawn(move || {
            let mut deadline: Option<u64> = None;
            loop {
                let next = match deadline {
                    None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    Some(due) => {
                        let left = Duration::from_secs(due.saturating_sub(unix_now()));
                        receiver.recv_timeout(left.min(TIMER_STEP))
                    }
                };
                match next {
                    Ok(due) => deadline = due,
                    Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {
                        if deadline.is_some_and(|due| unix_now() >= due) {
                            deadline = None;
                            if let Some(signal) = wake.get() {
                                signal.request_redraw();
                            }
                        }
                    }
                }
            }
        });
    sender
}

/// One run of one task: its id, the project it runs for and the key its
/// last run is saved under.
struct Run {
    task: String,
    project: Option<TaskProject>,
    key: String,
}

impl Workspace {
    /// Runs every task that is due and sets the timer for the next one.
    pub(crate) fn run_due_tasks(&self) {
        let now = unix_now();
        let mut runs = Vec::new();
        let mut next: Option<u64> = None;
        {
            let mut inner = self.borrow_mut();
            let revision = inner.ext.tasks_revision();
            inner.tasks.seen = Some(revision);
            let inner = &*inner;
            for task in inner.ext.tasks() {
                for (key, project) in targets(inner, task) {
                    let due = inner
                        .state
                        .task_runs
                        .get(&key)
                        .map_or(now, |last| last.saturating_add(task.every));
                    let due = if due > now {
                        due
                    } else {
                        match project {
                            Some(None) => now + SNAPSHOT_RETRY,
                            project => {
                                runs.push(Run {
                                    task: task.id.clone(),
                                    project: project.flatten(),
                                    key,
                                });
                                now + task.every
                            }
                        }
                    };
                    next = Some(next.map_or(due, |next| next.min(due)));
                }
            }
        }
        self.run_tasks(runs, now);
        let wake = self.redraw_handle();
        self.borrow_mut().tasks.set(next, &wake);
    }

    /// Runs task `id` now for every project it matches, whether due or
    /// not, as the palette's Run task does. The period starts again.
    pub fn run_task_now(&self, id: &str) {
        let now = unix_now();
        let runs = {
            let inner = self.borrow();
            let Some(task) = inner.ext.tasks().iter().find(|task| task.id == id) else {
                return;
            };
            targets(&inner, task)
                .into_iter()
                .filter_map(|(key, project)| match project {
                    // Its worktrees are not known yet.
                    Some(None) => None,
                    project => Some(Run {
                        task: task.id.clone(),
                        project: project.flatten(),
                        key,
                    }),
                })
                .collect()
        };
        self.run_tasks(runs, now);
        self.borrow_mut().tasks.look_again();
    }

    /// Calls each run's function and saves when it ran. The time is saved
    /// first, so a task that fails waits for its next period.
    fn run_tasks(&self, runs: Vec<Run>, now: u64) {
        if runs.is_empty() {
            return;
        }
        for run in runs {
            self.borrow_mut().state.task_runs.insert(run.key, now);
            let result = self
                .borrow_mut()
                .ext
                .run_task(&run.task, run.project.as_ref());
            if let Err(error) = result {
                self.notify(error.to_string());
            }
        }
        self.save();
    }
}

/// What `task` runs for, with the key of its last run: once without a
/// project, or once per project it matches. A matching project whose
/// worktrees are not known yet comes back as `Some(None)`.
#[allow(clippy::option_option)]
fn targets(inner: &Inner, task: &TaskInfo) -> Vec<(String, Option<Option<TaskProject>>)> {
    if task.projects.is_none() {
        return vec![(task.id.clone(), None)];
    }
    inner
        .state
        .projects
        .iter()
        .filter(|project| task.matches(&project.name, &project.root))
        .map(|project| {
            let key = format!("{}@{}", task.id, project.id);
            let found = inner
                .snapshots
                .get(&project.id)
                .map(|snapshot| TaskProject {
                    name: project.name.clone(),
                    id: crate::workspace::project_storage_id(
                        &project.name,
                        &project.env,
                        &project.root,
                    ),
                    root: project.root.clone(),
                    env: project.env.label().to_owned(),
                    worktrees: snapshot
                        .worktrees
                        .iter()
                        .map(|worktree| TaskWorktree {
                            path: worktree.path.clone(),
                            branch: worktree.branch.clone(),
                            runtime: crate::runtime::runtime_name(
                                project
                                    .worktree(&worktree.path)
                                    .map(|config| config.runtime)
                                    .unwrap_or_default(),
                            )
                            .to_owned(),
                        })
                        .collect(),
                });
            (key, Some(found))
        })
        .collect()
}
