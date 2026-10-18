//! Fused remote machines: the sidebar shows the projects a trusted remote
//! machine shares next to this machine's own, so switching to a worktree
//! over there is the same as switching to one here.
//!
//! `remote.fused` in `settings.json` lists the machines. A worker thread asks
//! each one for its projects (`cmdterm_remote::projects`) at startup, when
//! the list changes, and every [`REFRESH`] after that. The UI thread adds the
//! projects it has not seen as ordinary remote projects marked
//! [`Project::fused`], and removes fused ones the machine stopped listing.
//! They are saved like any other project, so they are back on the next start
//! before the machine answers, and stay while it is unreachable.
//!
//! Laptops lose their network for hours, and wake up on another one. So an
//! ask that gets no answer within [`ASK_TIMEOUT`] is given up, a machine that
//! did not answer is asked again after [`RETRY`], doubling up to [`REFRESH`],
//! and every machine is asked at once when this one wakes from sleep.
//!
//! [`Project::fused`]: crate::state::Project::fused

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use cmdterm_env::{Environment, RemoteTarget};
use cmdterm_remote::projects::{self, SharedProject};
use schnellui::RedrawSignal;

use crate::ext::Event;
use crate::machines::Shared;
use crate::state::FusedMachine;
use crate::vcs::VcsKind;
use crate::workspace::{Workspace, project_storage_id};

/// How often a machine that answered is asked again.
const REFRESH: Duration = Duration::from_secs(300);

/// How long one ask may take. A connection that died while this machine
/// slept can hang until TCP gives up on it.
const ASK_TIMEOUT: Duration = Duration::from_secs(45);

/// How long the worker waits before it asks a machine that did not answer
/// again. Each further miss doubles it, up to [`REFRESH`].
const RETRY: Duration = Duration::from_secs(15);

/// How often the worker compares the clocks to notice a sleep.
const TICK: Duration = Duration::from_secs(30);

/// The monotonic clock stops while this machine sleeps and the wall clock
/// does not. A gap of more than this between them is a sleep.
const SLEPT: Duration = Duration::from_secs(60);

/// What one machine answered last.
#[derive(Clone, Debug)]
pub struct Listing {
    pub address: String,
    pub projects: Result<Vec<SharedProject>, String>,
}

/// The worker that asks fused machines for their projects.
#[derive(Default)]
pub struct Fusion {
    /// The addresses the worker asks, read before each round.
    machines: Arc<Mutex<Vec<String>>>,
    /// The latest answer of every fused machine.
    found: Shared<Vec<Listing>>,
    /// Asks every machine now. `None` until the first machine is fused.
    kick: Option<mpsc::Sender<()>>,
}

/// When the worker asks one machine next.
struct Due {
    address: String,
    at: Instant,
    /// Asks in a row that got no answer.
    misses: u32,
}

impl Fusion {
    /// Asks `addresses` now, starting the worker if it is not running.
    fn ask(&mut self, addresses: Vec<String>, wake: Arc<OnceLock<RedrawSignal>>) {
        let empty = addresses.is_empty();
        if let Ok(mut machines) = self.machines.lock() {
            *machines = addresses;
        }
        if let Some(kick) = &self.kick
            && kick.send(()).is_ok()
        {
            return;
        }
        if empty {
            return;
        }
        let (kick, rounds) = mpsc::channel();
        self.kick = Some(kick);
        let machines = self.machines.clone();
        let found = self.found.clone();
        let _ = std::thread::Builder::new()
            .name("cmdterm-fusion".into())
            .spawn(move || work(&machines, &found, &rounds, &wake));
    }

    /// True once after the worker brought new answers. Allocation-free,
    /// called per poll.
    pub fn changed(&self) -> bool {
        self.found.changed()
    }

    /// Whether the last ask of the machine at `address` got no answer.
    fn unreachable(&self, address: &str) -> bool {
        self.found
            .get()
            .iter()
            .any(|listing| listing.address == address && listing.projects.is_err())
    }
}

/// The worker's loop: asks each machine when it is due, and all of them
/// after a kick or a sleep.
fn work(
    machines: &Mutex<Vec<String>>,
    found: &Shared<Vec<Listing>>,
    rounds: &mpsc::Receiver<()>,
    wake: &OnceLock<RedrawSignal>,
) {
    let mut due: Vec<Due> = Vec::new();
    let mut latest: Vec<Listing> = Vec::new();
    let mut everyone = true;
    loop {
        let addresses = machines.lock().map(|m| m.clone()).unwrap_or_default();
        let now = Instant::now();
        due.retain(|machine| addresses.contains(&machine.address));
        for address in &addresses {
            if !due.iter().any(|machine| &machine.address == address) {
                due.push(Due {
                    address: address.clone(),
                    at: now,
                    misses: 0,
                });
            }
        }
        if everyone {
            for machine in &mut due {
                machine.at = now;
                machine.misses = 0;
            }
        }
        let asked: Vec<String> = due
            .iter()
            .filter(|machine| machine.at <= now)
            .map(|machine| machine.address.clone())
            .collect();
        let before = latest.len();
        latest.retain(|listing| addresses.contains(&listing.address));
        if !asked.is_empty() || latest.len() != before {
            for listing in ask_all(&asked) {
                if let Some(machine) = due.iter_mut().find(|m| m.address == listing.address) {
                    let now = Instant::now();
                    if listing.projects.is_ok() {
                        machine.misses = 0;
                        machine.at = now + REFRESH;
                    } else {
                        machine.at = now + retry_after(machine.misses);
                        machine.misses = machine.misses.saturating_add(1);
                    }
                }
                match latest.iter_mut().find(|l| l.address == listing.address) {
                    Some(slot) => *slot = listing,
                    None => latest.push(listing),
                }
            }
            found.set(latest.clone());
            if let Some(signal) = wake.get() {
                signal.request_redraw();
            }
        }
        let next = due.iter().map(|machine| machine.at).min();
        let wait = next.map_or(TICK, |at| {
            at.saturating_duration_since(Instant::now()).min(TICK)
        });
        let (mono, wall) = (Instant::now(), SystemTime::now());
        everyone = match rounds.recv_timeout(wait) {
            Ok(()) => true,
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Kicks that came in meanwhile are this round.
        while rounds.try_recv().is_ok() {}
        let wall = SystemTime::now().duration_since(wall).unwrap_or_default();
        everyone |= slept(wall, mono.elapsed());
    }
}

/// How long to wait before asking a machine again that missed `misses`
/// asks before this one.
fn retry_after(misses: u32) -> Duration {
    RETRY.saturating_mul(1 << misses.min(5)).min(REFRESH)
}

/// Whether this machine slept while `wall` passed on the wall clock and
/// `mono` on the monotonic one.
fn slept(wall: Duration, mono: Duration) -> bool {
    wall > mono + SLEPT
}

/// Asks every machine at once, so one that does not answer holds up no
/// other.
fn ask_all(addresses: &[String]) -> Vec<Listing> {
    std::thread::scope(|scope| {
        let asks: Vec<_> = addresses
            .iter()
            .map(|address| {
                scope.spawn(move || Listing {
                    address: address.clone(),
                    projects: projects::fetch(&environment(address), ASK_TIMEOUT)
                        .map_err(|error| error.to_string()),
                })
            })
            .collect();
        asks.into_iter().filter_map(|ask| ask.join().ok()).collect()
    })
}

/// The environment of a fused machine's projects: its host, never a VM.
fn environment(address: &str) -> Environment {
    Environment::Remote(RemoteTarget::new(address))
}

impl Workspace {
    /// Asks the fused machines in the settings for their projects. Called
    /// at startup and after the list changes.
    pub fn fuse_from_settings(&self) {
        let wake = self.redraw_handle();
        let mut inner = self.borrow_mut();
        let addresses = inner
            .settings
            .remote
            .fused
            .iter()
            .map(|machine| machine.address.clone())
            .collect();
        inner.fusion.ask(addresses, wake);
    }

    /// Whether the sidebar shows the projects of the machine at `address`.
    pub fn is_fused(&self, address: &str) -> bool {
        let address = RemoteTarget::new(address).address;
        self.borrow()
            .settings
            .remote
            .fused
            .iter()
            .any(|machine| machine.address == address)
    }

    /// Whether the machine at `address` is fused and did not answer the
    /// last time it was asked. Its projects stay in the sidebar meanwhile.
    pub fn fused_unreachable(&self, address: &str) -> bool {
        let address = RemoteTarget::new(address).address;
        self.is_fused(&address) && self.borrow().fusion.unreachable(&address)
    }

    /// The switch on a trusted machine's row: shows its projects in the
    /// sidebar, or takes the ones it showed away again.
    pub fn set_fused(&self, address: &str, on: bool) {
        let address = RemoteTarget::new(address).address;
        let gone: Vec<u64> = {
            let mut inner = self.borrow_mut();
            let fused = &mut inner.settings.remote.fused;
            if on {
                if !fused.iter().any(|machine| machine.address == address) {
                    fused.push(FusedMachine {
                        address: address.clone(),
                        hidden: Vec::new(),
                    });
                }
                Vec::new()
            } else {
                fused.retain(|machine| machine.address != address);
                let env = environment(&address);
                inner
                    .state
                    .projects
                    .iter()
                    .filter(|project| project.fused && project.env == env)
                    .map(|project| project.id)
                    .collect()
            }
        };
        self.save_settings();
        for id in gone {
            self.drop_project(id);
        }
        self.fuse_from_settings();
        let tr = self.tr();
        if on {
            self.toast_done((tr.toast_fused)(&address));
        } else {
            self.toast_done((tr.toast_unfused)(&address));
        }
        self.borrow_mut().dirty.settings = true;
    }

    /// Keeps a fused project the user removes out of the sidebar, so the
    /// next answer of its machine does not bring it back.
    pub(crate) fn hide_fused(&self, id: u64) {
        let hidden = {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            let Some(project) = inner.state.project(id).filter(|p| p.fused) else {
                return;
            };
            let Environment::Remote(target) = &project.env else {
                return;
            };
            let Some(machine) = inner
                .settings
                .remote
                .fused
                .iter_mut()
                .find(|machine| machine.address == target.address)
            else {
                return;
            };
            if machine.hidden.contains(&project.root) {
                return;
            }
            machine.hidden.push(project.root.clone());
            true
        };
        if hidden {
            self.save_settings();
        }
    }

    /// Brings the sidebar in line with what the fused machines answered.
    /// A machine that did not answer keeps its projects. The shown project
    /// stays until the user leaves it.
    pub(crate) fn apply_fusion(&self) {
        let listings = self.borrow().fusion.found.get();
        let mut added = Vec::new();
        let mut gone = Vec::new();
        {
            let mut inner = self.borrow_mut();
            let inner = &mut *inner;
            // The machines page tells which machines do not answer.
            inner.dirty.settings |= inner.settings_ui.open;
            let active = inner.state.active_project;
            for listing in &listings {
                let Ok(shared) = &listing.projects else {
                    continue;
                };
                // Unfused while the worker asked.
                let Some(machine) = inner
                    .settings
                    .remote
                    .fused
                    .iter()
                    .find(|machine| machine.address == listing.address)
                else {
                    continue;
                };
                let env = environment(&listing.address);
                let wanted = |root: &std::path::Path| {
                    !machine.hidden.iter().any(|hidden| hidden == root)
                        && shared.iter().any(|project| project.root == root)
                };
                for project in shared {
                    if !wanted(&project.root) {
                        continue;
                    }
                    let Some(vcs) = vcs_kind(&project.vcs) else {
                        continue;
                    };
                    let known = inner
                        .state
                        .projects
                        .iter()
                        .any(|p| !p.is_global() && p.env == env && p.root == project.root);
                    if known {
                        continue;
                    }
                    let id = inner.state.add_project(
                        project.name.clone(),
                        env.clone(),
                        project.root.clone(),
                        vcs,
                        None,
                    );
                    if let Some(new) = inner.state.project_mut(id) {
                        new.fused = true;
                    }
                    added.push(id);
                }
                gone.extend(
                    inner
                        .state
                        .projects
                        .iter()
                        .filter(|p| p.fused && p.env == env && Some(p.id) != active)
                        .filter(|p| !wanted(&p.root))
                        .map(|p| p.id),
                );
            }
        }
        for id in gone {
            self.drop_project(id);
        }
        if added.is_empty() {
            return;
        }
        for &id in &added {
            let event = {
                let inner = self.borrow();
                let Some(project) = inner.state.project(id) else {
                    continue;
                };
                Event::ProjectAdded {
                    project_id: project_storage_id(&project.name, &project.env, &project.root),
                    name: project.name.clone(),
                    root: project.root.clone(),
                    env: project.env.label().to_owned(),
                }
            };
            self.emit(&event);
            self.reread_project(id);
        }
        {
            let mut inner = self.borrow_mut();
            inner.tasks.look_again();
            inner.dirty.sidebar = true;
        }
        self.save();
    }

    /// Sets what the worker found, as if it had asked. For tests.
    #[cfg(test)]
    pub fn fake_fusion(&self, listings: Vec<Listing>) {
        self.borrow().fusion.found.set(listings);
    }
}

/// The VCS of a shared project, as `state.json` names it.
fn vcs_kind(name: &str) -> Option<VcsKind> {
    serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{SettingsStore, StateStore};
    use std::path::{Path, PathBuf};

    /// Nothing listens there, so the VCS worker's reads fail fast.
    const ADDRESS: &str = "127.0.0.1:1";

    fn shared(name: &str, root: &str) -> SharedProject {
        SharedProject {
            name: name.into(),
            root: PathBuf::from(root),
            vcs: "git".into(),
        }
    }

    fn answer(ws: &Workspace, projects: Vec<SharedProject>) {
        ws.fake_fusion(vec![Listing {
            address: ADDRESS.into(),
            projects: Ok(projects),
        }]);
        assert!(ws.borrow().fusion.changed());
        ws.apply_fusion();
    }

    fn fused_roots(ws: &Workspace) -> Vec<PathBuf> {
        ws.borrow()
            .state
            .projects
            .iter()
            .filter(|p| p.fused)
            .map(|p| p.root.clone())
            .collect()
    }

    #[test]
    fn fused_machines_add_hide_and_drop_their_projects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(
            StateStore::new(root.join("state.json")),
            SettingsStore::new(root.join("settings.json")),
            &root.join("extensions"),
        );
        ws.borrow_mut().settings.remote.fused.push(FusedMachine {
            address: ADDRESS.into(),
            hidden: Vec::new(),
        });
        assert!(ws.is_fused("127.0.0.1:1"));

        // Shared projects show as remote projects on the machine's host.
        answer(
            &ws,
            vec![shared("app", "/srv/app"), shared("lib", "/srv/lib")],
        );
        assert_eq!(
            fused_roots(&ws),
            [Path::new("/srv/app"), Path::new("/srv/lib")]
        );
        let app = ws.borrow().state.projects[0].clone();
        assert_eq!(app.env, environment(ADDRESS));
        let saved = StateStore::new(root.join("state.json")).load().unwrap();
        assert!(saved.projects.iter().all(|p| p.fused));

        // A machine that does not answer keeps its projects.
        ws.fake_fusion(vec![Listing {
            address: ADDRESS.into(),
            projects: Err("refused".into()),
        }]);
        ws.apply_fusion();
        assert_eq!(fused_roots(&ws).len(), 2);
        assert!(ws.fused_unreachable(ADDRESS));
        answer(
            &ws,
            vec![shared("app", "/srv/app"), shared("lib", "/srv/lib")],
        );
        assert!(!ws.fused_unreachable(ADDRESS));

        // Removed by the user, a project stays out of the sidebar.
        let lib = ws.borrow().state.projects[1].id;
        ws.remove_project(lib);
        answer(
            &ws,
            vec![shared("app", "/srv/app"), shared("lib", "/srv/lib")],
        );
        assert_eq!(fused_roots(&ws), [Path::new("/srv/app")]);
        let settings = SettingsStore::new(root.join("settings.json"))
            .load()
            .unwrap();
        assert_eq!(settings.remote.fused[0].hidden, [Path::new("/srv/lib")]);

        // One the machine stopped listing goes, unless it is shown.
        ws.borrow_mut().state.active_project = None;
        answer(&ws, vec![shared("new", "/srv/new")]);
        assert_eq!(fused_roots(&ws), [Path::new("/srv/new")]);

        // A project the user added at the same place is never taken away.
        ws.borrow_mut().state.add_project(
            "mine".into(),
            environment(ADDRESS),
            "/srv/mine".into(),
            VcsKind::Git,
            None,
        );
        ws.borrow_mut().state.active_project = None;
        answer(&ws, vec![shared("mine", "/srv/mine")]);
        answer(&ws, Vec::new());
        let names: Vec<_> = ws
            .borrow()
            .state
            .projects
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(names, ["mine"]);

        // Unfusing takes the machine's projects away.
        ws.borrow_mut().state.active_project = None;
        answer(&ws, vec![shared("app", "/srv/app")]);
        ws.set_fused(ADDRESS, false);
        assert!(fused_roots(&ws).is_empty());
        assert!(!ws.is_fused(ADDRESS));
    }

    #[test]
    fn missed_asks_back_off_and_sleeps_are_noticed() {
        let waits: Vec<_> = (0..8).map(|misses| retry_after(misses).as_secs()).collect();
        assert_eq!(waits, [15, 30, 60, 120, 240, 300, 300, 300]);
        assert_eq!(retry_after(u32::MAX), REFRESH);

        let second = Duration::from_secs(1);
        assert!(!slept(TICK, TICK));
        // Wall clock adjustments of a few seconds are no sleep.
        assert!(!slept(TICK + 5 * second, TICK));
        assert!(slept(Duration::from_secs(3600), TICK));
        // A wall clock set back is no sleep either.
        assert!(!slept(Duration::ZERO, TICK));
    }
}
