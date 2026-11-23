//! The rule groups of the Seatbelt profile that `sandbox.rs` builds for
//! `sandbox-exec` on macOS, and the probe that leaves out a group this
//! machine's `sandbox-exec` rejects.
//!
//! The profile allows what it does not deny (`(allow default)`), so each
//! group denies one way past the sandbox. `sandbox-exec` refuses a whole
//! profile when it does not know one operation or filter in it, and macOS
//! versions differ in what they know. A [`Probe`] runs `sandbox-exec` with
//! the full profile and `/usr/bin/true`, on a thread of its own, started at
//! app start ([`warm_seatbelt_probe`]) or by the first sandbox on a host.
//! When that fails but the profile without any group loads, it tries each
//! group alone, keeps the ones that load, and drops the rest. The dropped
//! groups are reported once through [`take_sandbox_warnings`]. A host
//! where even the bare profile fails, or where `sandbox-exec` cannot run,
//! keeps every group, so the sandbox's own error reaches the user.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::Environment;

/// A set of rule groups.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Rules(u8);

impl Rules {
    /// Programs that start or script other programs on the sandbox's
    /// behalf, which then run outside it: the `lsopen`, `appleevent-send`
    /// and `job-creation` checks that LaunchServices (`open`), Apple
    /// Events (`osascript`) and launchd (`launchctl submit`, `load`,
    /// `bootstrap`) make on a sandboxed caller, and the `launchctl`
    /// binary, whose `setenv` and `kickstart` change what launchd starts
    /// without creating a job.
    pub(crate) const LAUNCH: Self = Self(1);
    /// The mach services behind those: LaunchServices, Apple Events, login
    /// items (`sharedfilelistd`, `smd`). Denied as well, in case a service
    /// skips its check. `com.apple.lsd.mapdb` only maps the LaunchServices
    /// database for reading, and Anthropic's sandbox-runtime keeps it open
    /// in its own allow list. DNS (`mDNSResponder`), the keychain and TLS
    /// trust (`SecurityServer`, `trustd`, `ocspd`), user lookups
    /// (`opendirectoryd`), `notifyd`, `logd` and `diagnosticd` stay open,
    /// because command line tools need them.
    pub(crate) const MACH: Self = Self(2);
    /// The window server, through which a program can post keyboard and
    /// mouse events to the user's windows when the app holds the
    /// Accessibility permission. GUI programs started in a sandbox cannot
    /// open a window without it.
    pub(crate) const WINDOW_SERVER: Self = Self(4);
    /// Unix sockets need an allow rule, whether the network is on or not,
    /// the way a bubblewrap sandbox reaches only the sockets it can see.
    /// The system's DNS and syslog sockets stay open, and so do sockets in
    /// the shared temporary directories, where programs in the sandbox put
    /// their own. launchd's listeners there (the SSH agent, XQuartz's
    /// display) and tmux servers, which run commands for whoever connects,
    /// are denied. `sandbox.rs` adds the writable, hidden and passed-in
    /// paths after these.
    pub(crate) const UNIX_SOCKETS: Self = Self(8);
    /// Processes outside the sandbox stay out of view: their open files,
    /// working directories and code signatures. Listing process ids stays
    /// allowed.
    pub(crate) const PROCESS_INFO: Self = Self(16);
    /// No process's arguments and environment, where another project's
    /// sandbox keeps its proxy password. Seatbelt names these sysctls by
    /// their path, `kern.procargs2` and the process id, so it cannot tell
    /// the sandbox's own processes: `ps` and `pgrep -f` see no arguments,
    /// even of those.
    pub(crate) const PROCARGS: Self = Self(32);

    pub(crate) const NONE: Self = Self(0);
    pub(crate) const ALL: Self = Self(63);

    /// Every group with its name, for warnings, and its profile text.
    const GROUPS: [(Self, &'static str, &'static str); 6] = [
        (
            Self::LAUNCH,
            "launch",
            "(deny lsopen)\n\
             (deny appleevent-send)\n\
             (deny job-creation)\n\
             (deny process-exec (literal \"/bin/launchctl\"))\n",
        ),
        (
            Self::MACH,
            "mach-services",
            "(deny mach-lookup\n  \
               (global-name \"com.apple.coreservices.launchservicesd\")\n  \
               (global-name \"com.apple.CoreServices.coreservicesd\")\n  \
               (global-name \"com.apple.coreservices.quarantine-resolver\")\n  \
               (global-name \"com.apple.coreservices.appleevents\")\n  \
               (global-name-prefix \"com.apple.coreservices.sharedfilelistd\")\n  \
               (global-name \"com.apple.xpc.smd\")\n  \
               (global-name-prefix \"com.apple.lsd.\"))\n\
             (allow mach-lookup (global-name \"com.apple.lsd.mapdb\"))\n",
        ),
        (
            Self::WINDOW_SERVER,
            "window-server",
            "(deny mach-lookup\n  \
               (global-name \"com.apple.windowserver.active\")\n  \
               (global-name \"com.apple.windowserver\"))\n",
        ),
        (
            Self::UNIX_SOCKETS,
            "unix-sockets",
            r##"(deny network-outbound (remote unix-socket))
(allow network-outbound
  (remote unix-socket (subpath "/private/var/run/mDNSResponder"))
  (remote unix-socket (subpath "/private/var/run/syslog"))
  (remote unix-socket (subpath "/private/tmp"))
  (remote unix-socket (subpath "/private/var/folders")))
(deny network-outbound
  (remote unix-socket (path-regex #"^/private/tmp/com\.apple\.launchd\."))
  (remote unix-socket (path-regex #"^/private/tmp/tmux-")))
"##,
        ),
        (
            Self::PROCESS_INFO,
            "process-info",
            "(deny process-info*)\n\
             (allow process-info* (target same-sandbox))\n\
             (allow process-info-listpids)\n",
        ),
        (
            Self::PROCARGS,
            "process-arguments",
            "(deny sysctl-read (sysctl-name-prefix \"kern.procargs\"))\n",
        ),
    ];

    pub(crate) fn has(self, group: Self) -> bool {
        self.0 & group.0 == group.0
    }

    fn with(self, group: Self) -> Self {
        Self(self.0 | group.0)
    }

    fn without(self, group: Self) -> Self {
        Self(self.0 & !group.0)
    }

    /// The profile text of the groups in this set.
    pub(crate) fn push_text(self, profile: &mut String) {
        for (group, _, text) in Self::GROUPS {
            if self.has(group) {
                profile.push_str(text);
            }
        }
    }

    /// The names of the groups in this set.
    fn names(self) -> Vec<&'static str> {
        (Self::GROUPS.iter())
            .filter(|(group, _, _)| self.has(*group))
            .map(|(_, name, _)| *name)
            .collect()
    }
}

/// The answers of `probe` for each of `sets`, asked in parallel.
fn batch(probe: &(impl Fn(Rules) -> Option<bool> + Sync), sets: &[Rules]) -> Vec<Option<bool>> {
    std::thread::scope(|scope| {
        let asked: Vec<_> = (sets.iter())
            .map(|rules| scope.spawn(move || probe(*rules)))
            .collect();
        (asked.into_iter())
            .map(|answer| answer.join().ok().flatten())
            .collect()
    })
}

/// The groups a `sandbox-exec` accepts, found with `probe`, which answers
/// whether a profile with the given groups loads, or `None` when it could
/// not tell. See the module documentation. The questions of each step are
/// asked at once, so a slow `sandbox-exec` costs about one timeout per
/// step: four steps, and more only when groups clash.
pub(crate) fn settle(probe: impl Fn(Rules) -> Option<bool> + Sync) -> Rules {
    if batch(&probe, &[Rules::ALL, Rules::NONE]) != [Some(false), Some(true)] {
        return Rules::ALL;
    }
    let groups: Vec<Rules> = Rules::GROUPS.iter().map(|(group, _, _)| *group).collect();
    let alone = batch(&probe, &groups);
    let mut kept = (groups.iter().zip(alone))
        .filter(|(_, answer)| *answer == Some(true))
        .fold(Rules::NONE, |kept, (group, _)| kept.with(*group));
    if probe(kept) == Some(true) {
        return kept;
    }
    // Groups that load alone may still clash. The latest group whose
    // removal lets the rest load goes; failing that, groups go from the
    // last until the profile loads.
    let latest_first: Vec<Rules> = (groups.iter().rev().copied())
        .filter(|group| kept.has(*group))
        .collect();
    let without: Vec<Rules> = latest_first.iter().map(|g| kept.without(*g)).collect();
    if let Some(at) = batch(&probe, &without)
        .iter()
        .position(|answer| *answer == Some(true))
    {
        return without[at];
    }
    for group in latest_first {
        kept = kept.without(group);
        if kept == Rules::NONE || probe(kept) == Some(true) {
            break;
        }
    }
    kept
}

/// How long a sandbox that starts while its host's probe still runs waits
/// for it. After that it, and every later one, uses all groups until the
/// probe is done.
const WAIT: Duration = Duration::from_secs(2);

/// One probe of a host, running on a thread of its own.
pub(crate) struct Probe {
    result: Mutex<Option<Rules>>,
    done: Condvar,
    /// Set once a wait ran out, so that later sandboxes do not wait again.
    gave_up: AtomicBool,
}

impl Probe {
    /// Starts `settle` with `probe` on a new thread. Dropped groups become
    /// a [`SandboxWarning`] when it ends.
    pub(crate) fn start(
        probe: impl Fn(Rules) -> Option<bool> + Send + Sync + 'static,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            result: Mutex::new(None),
            done: Condvar::new(),
            gave_up: AtomicBool::new(false),
        });
        let running = this.clone();
        let started = std::thread::Builder::new()
            .name("cmdterm-seatbelt-probe".into())
            .spawn(move || {
                let rules = settle(probe);
                let dropped = Rules::ALL.without(rules);
                if dropped != Rules::NONE {
                    warn(SandboxWarning::SeatbeltRulesDropped(dropped.names()));
                }
                running.finish(rules);
            });
        if started.is_err() {
            this.finish(Rules::ALL);
        }
        this
    }

    fn finish(&self, rules: Rules) {
        if let Ok(mut result) = self.result.lock() {
            *result = Some(rules);
        }
        self.done.notify_all();
    }

    /// The probe's answer. Before it is done, the first caller waits up
    /// to `wait` for it; callers after a wait that ran out get every group
    /// at once.
    pub(crate) fn rules(&self, wait: Duration) -> Rules {
        let Ok(result) = self.result.lock() else {
            return Rules::ALL;
        };
        if let Some(rules) = *result {
            return rules;
        }
        if self.gave_up.load(Ordering::Acquire) {
            return Rules::ALL;
        }
        let waited = self
            .done
            .wait_timeout_while(result, wait, |result| result.is_none());
        match waited {
            Ok((result, _)) if result.is_some() => result.unwrap_or(Rules::ALL),
            _ => {
                self.gave_up.store(true, Ordering::Release);
                Rules::ALL
            }
        }
    }
}

/// The probe of `host`, started on first use.
fn probe_of(host: &Environment) -> Option<Arc<Probe>> {
    static PROBES: Mutex<Vec<(Environment, Arc<Probe>)>> = Mutex::new(Vec::new());
    let mut probes = PROBES.lock().ok()?;
    if let Some((_, probe)) = probes.iter().find(|(known, _)| known == host) {
        return Some(probe.clone());
    }
    let target = host.clone();
    let probe = Probe::start(move |rules| {
        let args = probe_policy().seatbelt_args(rules);
        let check = (target.command("/usr/bin/sandbox-exec"))
            .args(args)
            .arg("/usr/bin/true");
        let output = check.output_within(Duration::from_secs(10)).ok()?;
        Some(output.status.success())
    });
    probes.push((host.clone(), probe.clone()));
    Some(probe)
}

/// The groups `host`'s `sandbox-exec` accepts. The first call per host
/// starts the probe and waits for it at most two seconds; see [`Probe`].
pub(crate) fn accepted(host: &Environment) -> Rules {
    probe_of(host).map_or(Rules::ALL, |probe| probe.rules(WAIT))
}

/// Starts the probe of this machine on macOS, so it is done before the
/// first sandboxed terminal opens. Does not block. Elsewhere it does
/// nothing.
pub fn warm_seatbelt_probe() {
    if cfg!(target_os = "macos") {
        let _ = probe_of(&Environment::Local);
    }
}

/// A policy with one path of each kind the profile names, so a probe sees
/// every rule shape the real profiles have. The paths need not exist.
pub(crate) fn probe_policy() -> crate::SandboxPolicy {
    let dir = std::path::PathBuf::from("/private/tmp/cmdterm-probe");
    let mut policy = crate::SandboxPolicy::local(vec![dir.join("work")], &[], false);
    policy.kind = crate::SandboxKind::Seatbelt;
    policy.hidden_dirs.push(dir.join("work/hidden"));
    policy.hidden_files.push(dir.join("work/secret"));
    policy.read_only.push(dir.join("work/hooks"));
    policy.readable.push(dir.join("work/hidden/docs"));
    policy.sockets.push(dir.join("control.sock"));
    policy
}

/// Something about the sandboxes of this process that the user should
/// hear about once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxWarning {
    /// `sandbox-exec` rejected these rule groups of the Seatbelt profile,
    /// so sandboxes on that host run without them.
    SeatbeltRulesDropped(Vec<&'static str>),
    /// Terminals in Windows sandboxes start `with` instead of the shell
    /// `shell`, which runs on the MSYS runtime and cannot start there.
    ShellReplaced { shell: String, with: &'static str },
    /// Claude Code started in a Windows sandbox. Node gives each child pipes
    /// it creates by name, and the default access list of such a pipe lets
    /// only the user write to it, so every program Claude Code starts there
    /// fails with `EPERM`: its hooks, shell commands and tools.
    ClaudeWithoutGitBash,
}

static PENDING: AtomicBool = AtomicBool::new(false);
static WARNINGS: Mutex<Vec<SandboxWarning>> = Mutex::new(Vec::new());

pub(crate) fn warn(warning: SandboxWarning) {
    if let Ok(mut warnings) = WARNINGS.lock() {
        warnings.push(warning);
        PENDING.store(true, Ordering::Release);
    }
}

/// True when [`take_sandbox_warnings`] has something. Allocation-free, for
/// a check on every poll.
pub fn sandbox_warnings_pending() -> bool {
    PENDING.load(Ordering::Acquire)
}

/// The warnings since the last call.
pub fn take_sandbox_warnings() -> Vec<SandboxWarning> {
    PENDING.store(false, Ordering::Release);
    WARNINGS
        .lock()
        .map(|mut warnings| std::mem::take(&mut *warnings))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `sandbox-exec` that rejects every profile with one of `rejected`.
    fn rejecting(rejected: Rules) -> impl Fn(Rules) -> Option<bool> + Send + Sync + Copy {
        move |rules: Rules| Some(rules.0 & rejected.0 == 0)
    }

    #[test]
    fn a_profile_that_loads_keeps_every_group() {
        assert_eq!(settle(rejecting(Rules::NONE)), Rules::ALL);
        // No answer, or a bare profile that fails too, teaches nothing.
        assert_eq!(settle(|_| None), Rules::ALL);
        assert_eq!(settle(|_| Some(false)), Rules::ALL);
    }

    #[test]
    fn rejected_groups_are_dropped_and_the_rest_kept() {
        let kept = settle(rejecting(Rules::WINDOW_SERVER.with(Rules::PROCARGS)));
        assert_eq!(
            kept,
            Rules::ALL
                .without(Rules::WINDOW_SERVER)
                .without(Rules::PROCARGS)
        );
        assert_eq!(
            Rules::ALL.without(kept).names(),
            ["window-server", "process-arguments"]
        );
    }

    #[test]
    fn of_two_groups_that_clash_the_later_one_goes() {
        // Each loads alone, but not launch and unix sockets together.
        let clash =
            |rules: Rules| Some(!(rules.has(Rules::LAUNCH) && rules.has(Rules::UNIX_SOCKETS)));
        assert_eq!(settle(clash), Rules::ALL.without(Rules::UNIX_SOCKETS));
    }

    #[test]
    fn a_slow_probe_holds_up_only_the_first_sandbox_and_only_briefly() {
        let release = Arc::new(AtomicBool::new(false));
        let gate = release.clone();
        let probe = Probe::start(move |rules| {
            while !gate.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            rejecting(Rules::WINDOW_SERVER)(rules)
        });
        let started = std::time::Instant::now();
        assert_eq!(probe.rules(Duration::from_millis(50)), Rules::ALL);
        assert!(started.elapsed() < Duration::from_secs(1));
        // Later sandboxes do not wait again.
        let again = std::time::Instant::now();
        assert_eq!(probe.rules(Duration::from_secs(30)), Rules::ALL);
        assert!(again.elapsed() < Duration::from_millis(500));
        release.store(true, Ordering::Release);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while probe.rules(Duration::ZERO) == Rules::ALL && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            probe.rules(Duration::ZERO),
            Rules::ALL.without(Rules::WINDOW_SERVER)
        );
    }

    #[test]
    fn a_quick_probe_is_waited_for() {
        let probe = Probe::start(rejecting(Rules::PROCARGS));
        assert_eq!(
            probe.rules(Duration::from_secs(10)),
            Rules::ALL.without(Rules::PROCARGS)
        );
    }

    #[test]
    fn every_group_has_text_and_the_full_set_has_them_all() {
        let mut all = String::new();
        Rules::ALL.push_text(&mut all);
        for (group, name, text) in Rules::GROUPS {
            assert!(all.contains(text), "{name}");
            let mut alone = String::new();
            group.push_text(&mut alone);
            assert_eq!(alone, text);
        }
        assert!(all.contains("com.apple.windowserver.active"));
    }

    #[test]
    fn warnings_are_taken_once() {
        warn(SandboxWarning::SeatbeltRulesDropped(vec!["launch"]));
        assert!(sandbox_warnings_pending());
        assert!(
            take_sandbox_warnings().contains(&SandboxWarning::SeatbeltRulesDropped(vec!["launch"]))
        );
        assert!(!sandbox_warnings_pending());
    }
}
