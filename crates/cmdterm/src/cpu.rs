//! CPU modes: how much of a machine's CPU the app's terminals may use, so
//! agents and the builds they start leave it usable.
//!
//! Every terminal starts in a systemd scope of its own where the machine
//! has `systemd-run --user` ([`cmdterm_env::cpu`]), whatever the mode, so
//! switching modes reaches the terminals that already run: the mode moves
//! the cap of the slice they share and the weight of each scope. Agents in
//! hidden terminals get the mode's background nice level as their weight;
//! shown terminals and terminals without an agent keep the default.
//!
//! On machines with efficiency and performance cores, hidden terminals run
//! on the efficiency cores. Shown ones may also use the performance cores,
//! except in quiet mode, and except the cores a mode keeps free, which are
//! performance cores. The changes run on one worker thread, in the order
//! the app makes them; it asks each machine for its cores once.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::mpsc::{Sender, channel};

use cmdterm_env::cpu::{self, DEFAULT_WEIGHT};
use cmdterm_env::{CpuLimit, CpuShare, Environment};

use crate::state::{CpuMode, CpuModes};

impl CpuMode {
    pub const ALL: [Self; 3] = [Self::Off, Self::Balanced, Self::Quiet];

    /// What the mode does when settings change nothing: cores kept free,
    /// percent of the cores at most, and the nice level of agents in
    /// hidden terminals.
    fn builtin(self) -> (u32, u32, u8) {
        match self {
            Self::Off => (0, 100, 0),
            Self::Balanced => (1, 100, 10),
            Self::Quiet => (1, 50, 19),
        }
    }

    /// The mode after this one, for the status bar's switch.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Balanced,
            Self::Balanced => Self::Quiet,
            Self::Quiet => Self::Off,
        }
    }

    pub fn name(self, tr: &crate::i18n::Strings) -> &'static str {
        match self {
            Self::Off => tr.cpu_off,
            Self::Balanced => tr.cpu_balanced,
            Self::Quiet => tr.cpu_quiet,
        }
    }
}

/// Which [`CpuModes`] entry applies to a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// This machine, its sandboxes, devcontainers and local VMs.
    Local,
    /// SSH hosts, remote machines and extensions' environments.
    Remote,
}

impl Place {
    pub fn of(env: &Environment) -> Self {
        match env.base() {
            Environment::Local | Environment::Vm(_) => Self::Local,
            _ => Self::Remote,
        }
    }
}

impl CpuModes {
    pub fn of(&self, place: Place) -> CpuMode {
        match place {
            Place::Local => self.local,
            Place::Remote => self.remote,
        }
    }

    pub fn set(&mut self, place: Place, mode: CpuMode) {
        match place {
            Place::Local => self.local = mode,
            Place::Remote => self.remote = mode,
        }
    }

    /// What `mode` does with the changes these settings make to it:
    /// cores kept free, percent of the cores at most, and the nice level
    /// of agents in hidden terminals. Off stays off.
    fn tuned(&self, mode: CpuMode) -> (u32, u32, u8) {
        let (free, percent, nice) = mode.builtin();
        let tuning = match mode {
            CpuMode::Off => return (free, percent, nice),
            CpuMode::Balanced => &self.balanced,
            CpuMode::Quiet => &self.quiet,
        };
        (
            tuning.free_cores.unwrap_or(free),
            tuning.max_percent.unwrap_or(percent).clamp(1, 100),
            tuning.background_nice.unwrap_or(nice).min(19),
        )
    }

    /// The cores the machine's terminals may use together under `mode`.
    pub fn share(&self, mode: CpuMode) -> CpuShare {
        let (reserve, percent, _) = self.tuned(mode);
        CpuShare { reserve, percent }
    }

    /// The nice level of agents in hidden terminals under `mode`; 0
    /// leaves them alone.
    pub fn background_nice(&self, mode: CpuMode) -> u8 {
        self.tuned(mode).2
    }

    /// Whether shown terminals may use the performance cores under `mode`,
    /// on machines that have efficiency cores too.
    pub fn performance_cores(&self, mode: CpuMode) -> bool {
        match mode {
            CpuMode::Off => true,
            CpuMode::Balanced => self.balanced.performance_cores.unwrap_or(true),
            CpuMode::Quiet => self.quiet.performance_cores.unwrap_or(false),
        }
    }

    /// `mode`'s name with what it does, such as "balanced (keeps 1 core
    /// free, hidden agents at nice 10)".
    pub fn describe(&self, mode: CpuMode, tr: &crate::i18n::Strings) -> String {
        let (free, percent, nice) = self.tuned(mode);
        format!(
            "{} ({})",
            mode.name(tr),
            (tr.cpu_detail)(free, percent, nice)
        )
    }
}

/// A terminal's scope, as the app keeps it to change its weight and cores.
pub struct CpuControl {
    pub env: Environment,
    pub place: Place,
    unit: String,
    /// Whether the scope may exist already, with another weight, such as
    /// for a remote session the terminal attached to again.
    attached: bool,
    /// What was last sent; `None` before the first update.
    placed: Cell<Option<Placement>>,
}

/// What decides a scope's weight and cores, to send a change only when it
/// changes.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Placement {
    weight: u32,
    share: CpuShare,
    fast: bool,
    efficiency_cores: bool,
}

impl CpuControl {
    /// The limit a new terminal in `env` starts with under `modes`, and
    /// the control that changes it later. `id` names its scope; `attached`
    /// says the scope may exist already with another weight. `None` where
    /// limits do not apply, such as on Windows.
    pub fn start(
        env: &Environment,
        modes: &CpuModes,
        id: &str,
        attached: bool,
    ) -> Option<(CpuLimit, Self)> {
        if !cpu::applies(env) {
            return None;
        }
        let place = Place::of(env);
        let limit = CpuLimit::new(id, modes.share(modes.of(place)), DEFAULT_WEIGHT);
        let control = Self {
            env: env.clone(),
            place,
            unit: limit.unit.clone(),
            attached,
            placed: Cell::new(None),
        };
        Some((limit, control))
    }

    /// Gives the scope the weight and cores `modes` ask for at its place:
    /// a lower weight for an agent in a hidden terminal (`agent` and not
    /// `shown`), the efficiency cores unless `shown` and the mode lets it
    /// use the performance cores. Allocation-free while nothing changes;
    /// `pids` are the processes that stand in for the scope where systemd
    /// does not manage it.
    pub fn update(
        &self,
        modes: &CpuModes,
        shown: bool,
        agent: bool,
        pids: impl FnOnce() -> Vec<i32>,
    ) {
        let mode = modes.of(self.place);
        let nice = if agent && !shown {
            modes.background_nice(mode)
        } else {
            0
        };
        let placement = Placement {
            weight: cpu::weight_for_nice(nice),
            share: modes.share(mode),
            fast: shown && modes.performance_cores(mode),
            efficiency_cores: modes.efficiency_cores,
        };
        let first = match self.placed.replace(Some(placement)) {
            Some(placed) if placed == placement => return,
            placed => placed.is_none(),
        };
        send(Job::Scope {
            env: self.env.clone(),
            unit: self.unit.clone(),
            placement,
            nice,
            // A new scope starts at the default weight on every core.
            unknown: first && self.attached,
            pids: if self.env.base().is_local() {
                pids()
            } else {
                Vec::new()
            },
        });
    }
}

/// Moves the cap of the slice on the machine of `env` to `share`, and on
/// machines with efficiency cores keeps the free ones out of it, off the
/// UI thread.
pub fn apply_share(env: Environment, share: CpuShare, efficiency_cores: bool) {
    send(Job::Share {
        env,
        share,
        efficiency_cores,
    });
}

enum Job {
    Share {
        env: Environment,
        share: CpuShare,
        efficiency_cores: bool,
    },
    Scope {
        env: Environment,
        unit: String,
        placement: Placement,
        nice: u8,
        /// Whether the scope's weight and cores are unknown, so the worker
        /// sets them even when they look unchanged.
        unknown: bool,
        pids: Vec<i32>,
    },
}

/// Queues `job` for the worker, which starts with the first job.
fn send(job: Job) {
    static WORKER: Mutex<Option<Sender<Job>>> = Mutex::new(None);
    let Ok(mut worker) = WORKER.lock() else {
        return;
    };
    let job = match worker.as_ref() {
        Some(tx) => match tx.send(job) {
            Ok(()) => return,
            Err(failed) => failed.0,
        },
        None => job,
    };
    let (tx, rx) = channel::<Job>();
    let spawned = std::thread::Builder::new()
        .name("cmdterm-cpu".into())
        .spawn(move || {
            let mut worker = Worker::default();
            for job in rx {
                worker.run(job);
            }
        });
    if spawned.is_ok() && tx.send(job).is_ok() {
        *worker = Some(tx);
    }
}

/// What the worker knows about the machines and scopes it changed.
#[derive(Default)]
struct Worker {
    /// Each machine's cores, asked once; `None` where it could not tell.
    topologies: HashMap<Environment, Option<cpu::Topology>>,
    /// The weight and cores last set on each scope. A scope missing here
    /// has the default weight and every core.
    scopes: HashMap<String, (u32, Option<Vec<u32>>)>,
}

impl Worker {
    fn topology(&mut self, env: &Environment) -> Option<&cpu::Topology> {
        let base = env.base();
        if !self.topologies.contains_key(base) {
            let topology = cpu::topology(base);
            self.topologies.insert(base.clone(), topology);
        }
        self.topologies.get(base)?.as_ref()
    }

    /// The cores to give a scope or the slice: `None` leaves them alone,
    /// on machines whose cores are all alike, and an empty list allows
    /// every core again where placing is turned off.
    fn cores(
        &mut self,
        env: &Environment,
        share: CpuShare,
        fast: bool,
        efficiency_cores: bool,
    ) -> Option<Vec<u32>> {
        let topology = self.topology(env)?;
        if !topology.hybrid() {
            return None;
        }
        if !efficiency_cores {
            return Some(Vec::new());
        }
        topology.cores(share, fast)
    }

    fn run(&mut self, job: Job) {
        match job {
            Job::Share {
                env,
                share,
                efficiency_cores,
            } => {
                if matches!(env, Environment::Container(_)) {
                    return;
                }
                let cores = self.cores(&env, share, true, efficiency_cores);
                cpu::set_share(&env, share, cores.as_deref());
            }
            Job::Scope {
                env,
                unit,
                placement,
                nice,
                unknown,
                pids,
            } => {
                let cores = if matches!(env, Environment::Container(_)) {
                    None
                } else {
                    self.cores(
                        &env,
                        placement.share,
                        placement.fast,
                        placement.efficiency_cores,
                    )
                };
                let wanted = (placement.weight, cores);
                let current = self.scopes.get(&unit);
                let unchanged = current.map_or(
                    wanted.0 == DEFAULT_WEIGHT && wanted.1.is_none(),
                    |current| *current == wanted,
                );
                if unchanged && !unknown {
                    return;
                }
                let (weight, cores) = &wanted;
                if !cpu::set_scope(&env, &unit, *weight, cores.as_deref())
                    && cfg!(target_os = "linux")
                {
                    for pid in &pids {
                        let _ = cpu::set_autogroup_nice(*pid, nice);
                    }
                    if let Some(cores) = cores {
                        cpu::pin_local(&pids, cores);
                    }
                }
                // Terminals come and go; the map only needs the live ones.
                if self.scopes.len() > 4096 {
                    self.scopes.clear();
                }
                self.scopes.insert(unit, wanted);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_keep_cores_free_and_lower_hidden_agents() {
        let modes = CpuModes::default();
        assert_eq!(modes.share(CpuMode::Off), CpuShare::ALL);
        assert_eq!(modes.share(CpuMode::Balanced).cores(16), 15);
        assert_eq!(modes.share(CpuMode::Quiet).cores(16), 8);
        assert_eq!(modes.share(CpuMode::Quiet).cores(1), 1);
        let weight = |mode| cpu::weight_for_nice(modes.background_nice(mode));
        assert_eq!(weight(CpuMode::Off), 100);
        assert_eq!(weight(CpuMode::Balanced), 11);
        assert_eq!(weight(CpuMode::Quiet), 1);
        assert_eq!(CpuMode::Quiet.next(), CpuMode::Off);
    }

    #[test]
    fn settings_tune_what_a_mode_does() {
        let modes: CpuModes = serde_json::from_str(
            r#"{"quiet": {"max_percent": 25}, "balanced": {"free_cores": 4, "background_nice": 40}}"#,
        )
        .unwrap();
        assert_eq!(modes.share(CpuMode::Quiet).cores(16), 4);
        assert_eq!(modes.background_nice(CpuMode::Quiet), 19);
        assert_eq!(modes.share(CpuMode::Balanced).cores(16), 12);
        assert_eq!(modes.background_nice(CpuMode::Balanced), 19);
        assert_eq!(modes.share(CpuMode::Off), CpuShare::ALL);
        let tr = &crate::i18n::EN;
        assert_eq!(
            modes.describe(CpuMode::Quiet, tr),
            "quiet (keeps 1 core free, uses at most 25% CPU, background agents at nice 19)"
        );
        assert_eq!(modes.describe(CpuMode::Off, tr), "off (no limit)");
        // Untouched modes stay out of settings.json.
        let saved = serde_json::to_string(&CpuModes::default()).unwrap();
        assert_eq!(saved, r#"{"local":"balanced","remote":"off"}"#);
        assert!(modes.performance_cores(CpuMode::Off));
        assert!(modes.performance_cores(CpuMode::Balanced));
        assert!(!modes.performance_cores(CpuMode::Quiet));
    }

    #[test]
    fn places_follow_the_machine_under_a_runtime() {
        assert_eq!(Place::of(&Environment::Local), Place::Local);
        assert_eq!(Place::of(&Environment::ssh("box")), Place::Remote);
        let modes = CpuModes::default();
        assert_eq!(modes.of(Place::Local), CpuMode::Balanced);
        assert_eq!(modes.of(Place::Remote), CpuMode::Off);
    }

    #[test]
    fn settings_read_modes_by_name() {
        let modes: CpuModes = serde_json::from_str(r#"{"remote": "quiet"}"#).unwrap();
        assert_eq!(modes.local, CpuMode::Balanced);
        assert_eq!(modes.remote, CpuMode::Quiet);
    }
}
