//! CPU limits for terminals, so agents and the builds they start leave the
//! machine usable.
//!
//! A terminal starts through [`SCRIPT`], a POSIX script that runs on the
//! machine the terminal's processes run on, also over SSH and on remote
//! machines. Where `systemd-run --user` works, the terminal gets a scope of
//! its own in [`SLICE`]. The slice is capped by a [`Share`] of the machine's
//! cores, and every scope in it gets an equal part of that by default,
//! however many processes it starts. Both can change while the terminals
//! run: [`set_share`] moves the slice's cap, [`set_weight`] lowers or
//! raises one scope's part. Without systemd, Linux pins the terminal to its
//! share of the cores with `taskset` when it starts, and
//! [`set_autogroup_nice`] stands in for the weight on this machine.
//! Elsewhere the terminal runs unlimited.
//!
//! On machines with efficiency and performance cores ([`Topology`]), the
//! slice and each scope also get the cores they may run on
//! (`AllowedCPUs`), so the app can keep terminals the user does not watch
//! on the efficiency cores and a specific performance core free.
//!
//! On Windows, job objects do the same without a script (`windows.rs`).
//! A scriptschnell-term-remote server applies the limit itself to the terminals it
//! starts ([`CpuLimit::to_spec`]) and answers [`REMOTE_PROGRAM`] for the
//! changes after, so this works on Windows servers too.

#[cfg(windows)]
pub(crate) mod windows;

use std::ffi::OsString;
use std::time::Duration;

use crate::Environment;

/// The systemd slice terminals run in, under the user manager's
/// `app.slice`. All app instances on a machine share it, so the cap holds
/// for all of their terminals together.
pub const SLICE: &str = "app-scriptschnell_term.slice";

/// The variable the script sets to [`SLICE`] when the terminal runs in
/// it, so `control run` keeps its own scopes inside the cap.
pub const SLICE_VAR: &str = "SCRIPTSCHNELL_TERM_CPU_SLICE";

/// The weight of a scope that nothing lowered: systemd's default.
pub const DEFAULT_WEIGHT: u32 = 100;

/// The program of a scriptschnell-term-remote channel that changes CPU limits on the
/// server instead of running anything, with the arguments [`answer`]
/// takes. Older servers fail to start it, which leaves their limits alone.
pub const REMOTE_PROGRAM: &str = "cmdterm-remote:cpu";

/// Sets `a` to the cores share `r` and `p` gives on this machine, of `n`,
/// and `q` to the slice's `CPUQuota`, empty for no cap.
macro_rules! share_sh {
    () => {
        r#"n=$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 1)
a=$((n - r)); h=$((n * p / 100)); [ "$h" -lt "$a" ] && a=$h; [ "$a" -ge 1 ] || a=1
q=; [ "$a" -lt "$n" ] && q=$((a * 100))%
"#
    };
}

/// Starts `"$@"` limited. Arguments: the scope's unit name, the
/// [`Share`]'s cores to keep free and percent, the starting weight, `--`,
/// then the command. Without a command it starts `$SHELL`. systemd 254
/// and later expand `${NAME}` in the command of `systemd-run`, which would
/// change a shell script passed to `sh -c`, so the script turns that off
/// where `systemd-run` knows `--expand-environment`.
pub const SCRIPT: &str = concat!(
    r#"u=$1 r=$2 p=$3 w=$4; shift 5
[ "$#" -gt 0 ] || set -- "${SHELL:-/bin/sh}"
"#,
    share_sh!(),
    r#"if [ "$(uname -s)" = Linux ] && command -v systemd-run >/dev/null 2>&1 &&
  systemctl --user set-property --runtime app-scriptschnell_term.slice "CPUQuota=$q" >/dev/null 2>&1; then
  SCRIPTSCHNELL_TERM_CPU_SLICE=app-scriptschnell_term.slice; export SCRIPTSCHNELL_TERM_CPU_SLICE
  x=; systemd-run --help 2>/dev/null | grep -q -e --expand-environment && x=--expand-environment=no
  exec systemd-run --user --scope --quiet --collect --slice=app-scriptschnell_term.slice \
    --unit="$u" -p "CPUWeight=$w" -p "IOWeight=$w" $x -- "$@"
fi
if [ -n "$q" ] && [ "$(uname -s)" = Linux ] && command -v taskset >/dev/null 2>&1; then
  exec taskset -c "$((n - a))-$((n - 1))" "$@"
fi
exec "$@""#
);

/// Moves the slice's cap to the [`Share`] in its first two arguments, and
/// its cores to the list in the third when there is one; an empty list
/// lets it use every core.
const SHARE_SCRIPT: &str = concat!(
    "r=$1 p=$2\n",
    share_sh!(),
    r#"if [ "$#" -ge 3 ]; then
  exec systemctl --user set-property --runtime app-scriptschnell_term.slice "CPUQuota=$q" "AllowedCPUs=$3"
else
  exec systemctl --user set-property --runtime app-scriptschnell_term.slice "CPUQuota=$q"
fi"#
);

/// Sets the weight of scope `$1` to `$2` and, with a third argument, its
/// cores to the list in it, empty for every core. Where the user manager
/// has no cpuset controller, `taskset` moves the scope's processes
/// instead.
const SCOPE_SCRIPT: &str = r#"u=$1 w=$2
if [ "$#" -ge 3 ]; then
  systemctl --user set-property --runtime "$u" "CPUWeight=$w" "IOWeight=$w" "AllowedCPUs=$3" || exit 1
else
  systemctl --user set-property --runtime "$u" "CPUWeight=$w" "IOWeight=$w" || exit 1
fi
[ "$#" -ge 3 ] || exit 0
l=$3; [ -n "$l" ] || l=$(cat /sys/devices/system/cpu/online 2>/dev/null) || exit 0
g=/sys/fs/cgroup$(systemctl --user show --value -p ControlGroup "$u" 2>/dev/null)
[ -r "$g/cgroup.procs" ] || exit 0
grep -qw cpuset "$g/cgroup.controllers" 2>/dev/null && exit 0
command -v taskset >/dev/null 2>&1 || exit 0
while read -r pid; do taskset -apc "$l" "$pid" >/dev/null 2>&1; done < "$g/cgroup.procs"
exit 0"#;

/// Prints the machine's online cores and, where the kernel tells them
/// apart, its efficiency cores or every core's capacity.
const TOPOLOGY_SCRIPT: &str = r#"[ "$(uname -s)" = Linux ] || exit 0
d=/sys/devices/system/cpu
echo "online $(cat $d/online)"
if [ -r /sys/devices/cpu_atom/cpus ] && [ -r /sys/devices/cpu_core/cpus ]; then
  echo "efficiency $(cat /sys/devices/cpu_atom/cpus)"
  exit 0
fi
for c in $d/cpu[0-9]*; do
  [ -r "$c/cpu_capacity" ] && echo "capacity ${c##*/cpu} $(cat "$c/cpu_capacity")"
done
exit 0"#;

/// How much of a machine's CPU its terminals may use together: all cores
/// but `reserve`, and at most `percent` of them, at least one core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share {
    pub reserve: u32,
    pub percent: u32,
}

impl Share {
    /// Every core: no cap.
    pub const ALL: Self = Self {
        reserve: 0,
        percent: 100,
    };

    /// The cores this share gives on a machine with `cores` of them.
    pub fn cores(self, cores: u32) -> u32 {
        let part = cores * self.percent.min(100) / 100;
        cores.saturating_sub(self.reserve).min(part).max(1)
    }
}

/// The cores of a machine, telling its efficiency cores from its
/// performance cores.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Topology {
    /// Every online core, ascending.
    pub online: Vec<u32>,
    /// The efficiency cores among them, ascending; empty where all cores
    /// are alike.
    pub efficiency: Vec<u32>,
}

impl Topology {
    /// Reads the output of [`TOPOLOGY_SCRIPT`]. Without per-core types
    /// it takes the cores below the highest capacity as efficiency cores,
    /// as on ARM's big.LITTLE and on hybrid x86 with newer kernels.
    fn parse(text: &str) -> Self {
        let mut online = Vec::new();
        let mut efficiency = None;
        let mut capacities = Vec::new();
        for line in text.lines() {
            let mut words = line.split_whitespace();
            match (words.next(), words.next(), words.next()) {
                (Some("online"), Some(list), _) => online = parse_list(list),
                (Some("efficiency"), Some(list), _) => efficiency = Some(parse_list(list)),
                (Some("capacity"), Some(cpu), Some(capacity)) => {
                    if let (Ok(cpu), Ok(capacity)) = (cpu.parse::<u32>(), capacity.parse::<u32>()) {
                        capacities.push((cpu, capacity));
                    }
                }
                _ => {}
            }
        }
        let efficiency = efficiency.unwrap_or_else(|| {
            let top = capacities.iter().map(|(_, capacity)| *capacity).max();
            capacities
                .iter()
                .filter(|(_, capacity)| Some(*capacity) < top)
                .map(|(cpu, _)| *cpu)
                .collect()
        });
        let efficiency = efficiency
            .into_iter()
            .filter(|cpu| online.contains(cpu))
            .collect::<Vec<_>>();
        let mut topology = Self { online, efficiency };
        topology.efficiency.sort_unstable();
        topology
    }

    /// The topology in the form [`Self::parse`] reads.
    pub fn to_text(&self) -> String {
        format!(
            "online {}\nefficiency {}\n",
            format_list(&self.online),
            format_list(&self.efficiency)
        )
    }

    /// Whether the machine has both kinds of cores.
    pub fn hybrid(&self) -> bool {
        !self.efficiency.is_empty() && self.efficiency.len() < self.online.len()
    }

    /// The `reserve` cores a [`Share`] keeps free: performance cores
    /// first, from the lowest, so the rest of the machine keeps a fast
    /// core, and at most all cores but one.
    pub fn free(&self, reserve: u32) -> Vec<u32> {
        let count = (reserve as usize).min(self.online.len().saturating_sub(1));
        let performance = self
            .online
            .iter()
            .filter(|cpu| !self.efficiency.contains(cpu));
        performance
            .chain(&self.efficiency)
            .take(count)
            .copied()
            .collect()
    }

    /// The cores a terminal may run on under `share`: all but the free
    /// ones when `fast`, else the efficiency cores. `None` where the
    /// machine has only one kind of core and the kernel places alone.
    pub fn cores(&self, share: Share, fast: bool) -> Option<Vec<u32>> {
        if !self.hybrid() {
            return None;
        }
        let free = self.free(share.reserve);
        let pick = |from: &[u32]| -> Vec<u32> {
            from.iter()
                .filter(|cpu| !free.contains(cpu))
                .copied()
                .collect()
        };
        let cores = if fast {
            Vec::new()
        } else {
            pick(&self.efficiency)
        };
        Some(if cores.is_empty() {
            pick(&self.online)
        } else {
            cores
        })
    }
}

/// Reads a kernel CPU list such as `0-3,8,10-11`.
pub fn parse_list(list: &str) -> Vec<u32> {
    let mut cpus = Vec::new();
    for part in list.trim().split(',').filter(|part| !part.is_empty()) {
        let (start, end) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(start), Ok(end)) = (start.parse::<u32>(), end.parse::<u32>()) {
            cpus.extend(start..=end.min(start.saturating_add(4096)));
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    cpus
}

/// Writes ascending cores as a kernel CPU list, such as `0-3,8`.
pub fn format_list(cpus: &[u32]) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index < cpus.len() {
        let start = cpus[index];
        let mut end = start;
        while index + 1 < cpus.len() && cpus[index + 1] == end + 1 {
            index += 1;
            end += 1;
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}-{end}"));
        }
        index += 1;
    }
    out
}

/// The cores of the machine `env` runs on. Blocking. `None` where it
/// cannot tell, such as on macOS or when the machine does not answer.
pub fn topology(env: &Environment) -> Option<Topology> {
    if !applies(env) {
        return None;
    }
    let text = operate(env, TOPOLOGY_SCRIPT, &[], "topology")?;
    let topology = Topology::parse(&text);
    (!topology.online.is_empty()).then_some(topology)
}

/// How one terminal starts limited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuLimit {
    /// The terminal's scope, without `.scope`.
    pub unit: String,
    /// The slice's cap the terminal sets when it starts.
    pub share: Share,
    /// The scope's CPU and IO weight at start, 1 to 10000.
    pub weight: u32,
}

impl CpuLimit {
    /// A limit whose scope is named after `id`, such as a terminal's remote
    /// session, so the app finds it again after a restart. Characters a
    /// unit name cannot hold become `_`.
    pub fn new(id: &str, share: Share, weight: u32) -> Self {
        let id: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Self {
            unit: format!("scriptschnell-term-{id}"),
            share,
            weight,
        }
    }

    /// Arguments to `sh` that run the command after them limited.
    pub fn sh_args(&self) -> [OsString; 8] {
        [
            "-c".into(),
            SCRIPT.into(),
            "cmdterm-cpu".into(),
            self.unit.clone().into(),
            self.share.reserve.to_string().into(),
            self.share.percent.to_string().into(),
            self.weight.clamp(1, 10_000).to_string().into(),
            "--".into(),
        ]
    }

    /// The limit as one word, `unit:reserve:percent:weight`, for a
    /// scriptschnell-term-remote server to apply to the terminal it starts.
    pub fn to_spec(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.unit, self.share.reserve, self.share.percent, self.weight
        )
    }

    /// Reads [`Self::to_spec`].
    pub fn from_spec(spec: &str) -> Option<Self> {
        let mut parts = spec.split(':');
        let unit = parts.next().filter(|unit| !unit.is_empty())?;
        let mut number = || parts.next()?.parse::<u32>().ok();
        let (reserve, percent, weight) = (number()?, number()?, number()?);
        let limit = Self::new(
            unit.strip_prefix("scriptschnell-term-")?,
            Share { reserve, percent },
            weight,
        );
        (limit.unit == unit).then_some(limit)
    }

    /// [`Self::sh_args`] after `sh`, quoted for a remote shell, with a
    /// space at the end.
    pub(crate) fn quoted(&self) -> String {
        let mut out = String::from("sh");
        for arg in self.sh_args() {
            out.push(' ');
            out.push_str(&crate::quote(&arg.to_string_lossy()));
        }
        out.push(' ');
        out
    }
}

/// Whether limits apply to commands in `env` at all: on Unix through
/// [`SCRIPT`], on Windows through job objects, on remote machines through
/// their server.
pub fn applies(env: &Environment) -> bool {
    match env {
        Environment::Local => cfg!(any(unix, windows)),
        Environment::Sandbox(target) => applies(&target.host),
        Environment::Container(target) => applies(&target.host),
        _ => true,
    }
}

/// Whether `env` runs commands on this machine under Windows, where the
/// limit goes to the PTY's job instead of a script.
pub(crate) fn by_job(env: &Environment) -> bool {
    cfg!(windows) && matches!(env, Environment::Local)
}

/// Whether `env` is a scriptschnell-term-remote machine, whose server applies the
/// limit.
pub(crate) fn by_server(env: &Environment) -> bool {
    matches!(env, Environment::Remote(_) | Environment::Vm(_))
}

/// Runs `script` with `args` in `sh` on `env`, and returns what it printed.
fn run_sh(env: &Environment, script: &str, args: &[String]) -> Option<String> {
    let output = env
        .command("sh")
        .args(["-c", script, "cmdterm-cpu"])
        .args(args)
        .output_within(Duration::from_secs(10))
        .ok()
        .filter(|output| output.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Asks the server of `env` to run [`answer`] with `args`.
fn ask_server(env: &Environment, args: &[String]) -> Option<String> {
    let output = env
        .command(REMOTE_PROGRAM)
        .args(args)
        .output_within(Duration::from_secs(10))
        .ok()
        .filter(|output| output.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs a CPU operation on the machine of `env`: here, through its server
/// or through `sh`. `script` and `args` are the `sh` form; `op` is what
/// [`answer`] takes.
fn operate(env: &Environment, script: &str, args: &[String], op: &str) -> Option<String> {
    let base = env.base();
    if matches!(base, Environment::Local) {
        return local(op, args);
    }
    if by_server(base) {
        let mut all = vec![op.to_owned()];
        all.extend(args.iter().cloned());
        return ask_server(base, &all);
    }
    run_sh(base, script, args)
}

/// Runs a CPU operation on this machine, as [`answer`] describes.
#[cfg(windows)]
fn local(op: &str, args: &[String]) -> Option<String> {
    let number = |index: usize| args.get(index)?.parse::<u32>().ok();
    let cores = |index: usize| args.get(index).map(|list| parse_list(list));
    match op {
        "topology" => windows::topology().map(|topology| topology.to_text()),
        "share" => {
            let share = Share {
                reserve: number(0)?,
                percent: number(1)?,
            };
            windows::set_share(share, cores(2).as_deref()).then(String::new)
        }
        "scope" => {
            let unit = args.first()?.strip_suffix(".scope")?;
            windows::set_scope(unit, number(1)?, cores(2).as_deref()).then(String::new)
        }
        _ => None,
    }
}

/// Runs a CPU operation on this machine, as [`answer`] describes.
#[cfg(not(windows))]
fn local(op: &str, args: &[String]) -> Option<String> {
    let valid_cores = |list: &str| {
        list.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b',' | b'-'))
    };
    let valid_unit = |unit: &str| {
        unit.strip_prefix("scriptschnell-term-")
            .and_then(|rest| rest.strip_suffix(".scope"))
            .is_some_and(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            })
    };
    let script = match op {
        "topology" => {
            if !args.is_empty() {
                return None;
            }
            TOPOLOGY_SCRIPT
        }
        "share" => {
            if !(2..=3).contains(&args.len())
                || args[0].parse::<u32>().is_err()
                || args[1].parse::<u32>().is_err()
                || args.get(2).is_some_and(|list| !valid_cores(list))
            {
                return None;
            }
            SHARE_SCRIPT
        }
        "scope" => {
            if !(2..=3).contains(&args.len())
                || !valid_unit(&args[0])
                || args[1].parse::<u32>().is_err()
                || args.get(2).is_some_and(|list| !valid_cores(list))
            {
                return None;
            }
            SCOPE_SCRIPT
        }
        _ => return None,
    };
    run_sh(&Environment::Local, script, args)
}

/// Answers a [`REMOTE_PROGRAM`] channel on a scriptschnell-term-remote server: the
/// exit code and output of `topology`, `share RESERVE PERCENT [CORES]` or
/// `scope UNIT.scope WEIGHT [CORES]` on this machine.
pub fn answer(args: &[String]) -> (i32, String) {
    let Some((op, rest)) = args.split_first() else {
        return (2, String::new());
    };
    match local(op, rest) {
        Some(text) => (0, text),
        None => (1, String::new()),
    }
}

/// The weight systemd gives a process at nice level `nice`, from the
/// kernel's table of nice weights, where nice 0 is 1024 and the default
/// weight 100.
pub fn weight_for_nice(nice: u8) -> u32 {
    const WEIGHTS: [u32; 20] = [
        1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
    ];
    let weight = WEIGHTS[usize::from(nice.min(19))];
    ((weight * DEFAULT_WEIGHT + 512) / 1024).max(1)
}

/// Moves the cap of the slice on the machine of `env` to `share`, for the
/// terminals running there and those that start later, and its cores to
/// `cores` when given, empty for every core. Blocking. False where
/// nothing manages terminals there.
pub fn set_share(env: &Environment, share: Share, cores: Option<&[u32]>) -> bool {
    if matches!(env, Environment::Container(_)) || !applies(env) {
        return false;
    }
    let mut args = vec![share.reserve.to_string(), share.percent.to_string()];
    args.extend(cores.map(format_list));
    operate(env, SHARE_SCRIPT, &args, "share").is_some()
}

/// Sets the CPU and IO weight of the scope `unit` that a terminal of `env`
/// runs in, and its cores to `cores` when given, empty for every core.
/// Blocking. False where nothing manages the terminal, such as in a
/// devcontainer, where the scope would have to be inside.
pub fn set_scope(env: &Environment, unit: &str, weight: u32, cores: Option<&[u32]>) -> bool {
    if matches!(env, Environment::Container(_)) || !applies(env) {
        return false;
    }
    let mut args = vec![format!("{unit}.scope"), weight.clamp(1, 10_000).to_string()];
    args.extend(cores.map(format_list));
    operate(env, SCOPE_SCRIPT, &args, "scope").is_some()
}

/// Moves the processes `pids` on this machine, with all their threads, to
/// `cores`, where systemd does not manage the terminal they run in. Their
/// children inherit it.
pub fn pin_local(pids: &[i32], cores: &[u32]) {
    if cores.is_empty() || !cfg!(target_os = "linux") {
        return;
    }
    let list = format_list(cores);
    for pid in pids {
        let _ = Environment::Local
            .command("taskset")
            .args(["-apc", &list, &pid.to_string()])
            .output_within(Duration::from_secs(5));
    }
}

/// Sets the nice level of the autogroup `pid` belongs to: its session's,
/// as the kernel groups a terminal's processes without cgroups. Unlike a
/// process's own nice level, a user may raise it back to 0. Linux only,
/// and only where the kernel groups by session.
pub fn set_autogroup_nice(pid: i32, nice: u8) -> std::io::Result<()> {
    std::fs::write(format!("/proc/{pid}/autogroup"), nice.min(19).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_levels_map_to_systemd_weights() {
        assert_eq!(weight_for_nice(0), 100);
        assert_eq!(weight_for_nice(5), 33);
        assert_eq!(weight_for_nice(10), 11);
        assert_eq!(weight_for_nice(19), 1);
        assert_eq!(weight_for_nice(40), 1);
    }

    #[test]
    fn unit_names_hold_only_safe_characters() {
        let share = Share {
            reserve: 2,
            percent: 50,
        };
        let limit = CpuLimit::new("a1/b.c d", share, 11);
        assert_eq!(limit.unit, "scriptschnell-term-a1_b_c_d");
        let args = limit.sh_args();
        assert_eq!(args[3], "scriptschnell-term-a1_b_c_d");
        assert_eq!(args[4..], ["2", "50", "11", "--"]);
    }

    #[test]
    fn limits_travel_to_servers_as_one_word() {
        let limit = CpuLimit::new(
            "s1",
            Share {
                reserve: 1,
                percent: 50,
            },
            11,
        );
        assert_eq!(limit.to_spec(), "scriptschnell-term-s1:1:50:11");
        assert_eq!(CpuLimit::from_spec(&limit.to_spec()), Some(limit));
        assert_eq!(CpuLimit::from_spec("other:1:50:11"), None);
        assert_eq!(CpuLimit::from_spec("scriptschnell-term-s1:1:x:11"), None);
        let topology = Topology::parse("online 0-19\nefficiency 12-19\n");
        assert_eq!(Topology::parse(&topology.to_text()), topology);
        assert_eq!(answer(&[]).0, 2);
        assert_eq!(answer(&["fly".into()]).0, 1);
        assert_eq!(
            answer(&["share".into(), "a[$(id)]".into(), "50".into()]).0,
            1
        );
        assert_eq!(
            answer(&[
                "share".into(),
                "1".into(),
                "50".into(),
                "0 -p ExecStart=x".into()
            ])
            .0,
            1
        );
        assert_eq!(
            answer(&["scope".into(), "other.scope".into(), "10".into()]).0,
            1
        );
    }

    #[test]
    fn cpu_lists_read_and_write_the_kernel_format() {
        assert_eq!(parse_list("0-3,8,10-11\n"), [0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(parse_list(""), Vec::<u32>::new());
        assert_eq!(format_list(&[0, 1, 2, 3, 8, 10, 11]), "0-3,8,10-11");
        assert_eq!(format_list(&[]), "");
    }

    #[test]
    fn topologies_tell_efficiency_cores_apart() {
        // Intel: 6 performance cores with two threads, 8 efficiency cores.
        let intel = Topology::parse("online 0-19\nefficiency 12-19\n");
        assert!(intel.hybrid());
        assert_eq!(intel.efficiency, (12..20).collect::<Vec<_>>());
        // ARM: four little cores below the big ones.
        let mut arm = String::from("online 0-7\n");
        for cpu in 0..8 {
            let capacity = if cpu < 4 { 446 } else { 1024 };
            arm.push_str(&format!("capacity {cpu} {capacity}\n"));
        }
        let arm = Topology::parse(&arm);
        assert_eq!(arm.efficiency, [0, 1, 2, 3]);
        // All cores alike: the kernel places alone.
        let even = Topology::parse("online 0-3\ncapacity 0 1024\ncapacity 1 1024\n");
        assert!(!even.hybrid());
        let balanced = Share {
            reserve: 1,
            percent: 100,
        };
        assert_eq!(even.cores(balanced, true), None);
    }

    #[test]
    fn hidden_terminals_get_efficiency_cores_and_shown_ones_the_rest() {
        let intel = Topology::parse("online 0-19\nefficiency 12-19\n");
        let balanced = Share {
            reserve: 1,
            percent: 100,
        };
        // The free core is the first performance core.
        assert_eq!(intel.free(1), [0]);
        assert_eq!(
            intel.cores(balanced, true).unwrap(),
            (1..20).collect::<Vec<_>>()
        );
        assert_eq!(
            intel.cores(balanced, false).unwrap(),
            (12..20).collect::<Vec<_>>()
        );
        assert_eq!(
            intel.cores(Share::ALL, true).unwrap(),
            (0..20).collect::<Vec<_>>()
        );
        // More cores kept free than performance cores reach into the
        // efficiency cores, and one core always stays.
        assert_eq!(intel.free(13), (0..13).collect::<Vec<_>>());
        assert_eq!(intel.free(99).len(), 19);
        let all_but_one = Share {
            reserve: 99,
            percent: 100,
        };
        assert_eq!(intel.cores(all_but_one, false).unwrap(), [19]);
    }

    #[test]
    fn shares_keep_at_least_one_core() {
        let one = Share {
            reserve: 1,
            percent: 100,
        };
        let half = Share {
            reserve: 1,
            percent: 50,
        };
        assert_eq!(Share::ALL.cores(16), 16);
        assert_eq!(one.cores(16), 15);
        assert_eq!(one.cores(1), 1);
        assert_eq!(half.cores(16), 8);
        assert_eq!(half.cores(2), 1);
        assert_eq!(half.cores(3), 1);
    }

    /// The script computes the same cap as [`Share::cores`].
    #[cfg(target_os = "linux")]
    #[test]
    fn script_and_share_agree_on_the_cap() {
        for (reserve, percent) in [(0, 100), (1, 100), (1, 50), (64, 100)] {
            let script = concat!("r=$1 p=$2\n", share_sh!(), "echo \"$n $a $q\"");
            let output = std::process::Command::new("sh")
                .args([
                    "-c",
                    script,
                    "sh",
                    &reserve.to_string(),
                    &percent.to_string(),
                ])
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&output.stdout);
            let mut words = text.split_whitespace();
            let n: u32 = words.next().unwrap().parse().unwrap();
            let a: u32 = words.next().unwrap().parse().unwrap();
            let share = Share { reserve, percent };
            assert_eq!(a, share.cores(n), "{reserve} {percent}: {text}");
            let quota = words.next();
            assert_eq!(quota.is_some(), a < n, "{text}");
        }
    }

    /// The script runs the command it was given, and `$SHELL` without one.
    /// Without the user's bus it cannot reach systemd, so the test leaves
    /// the real slice alone.
    #[cfg(target_os = "linux")]
    #[test]
    fn script_runs_the_command_without_systemd() {
        let run = |command: &[&str]| {
            let output = std::process::Command::new("sh")
                .args(CpuLimit::new("test", Share::ALL, 100).sh_args())
                .args(command)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("SHELL", "/bin/echo")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        assert_eq!(run(&["sh", "-c", "echo ran"]), "ran");
        assert_eq!(run(&[]), "");
    }

    /// A command reaches `systemd-run` as it is, with `${...}` that
    /// systemd would expand, and `--expand-environment=no` only where
    /// `systemd-run` knows it. Fake `systemctl` and `systemd-run` print
    /// what they get instead of touching the real slice.
    #[cfg(target_os = "linux")]
    #[test]
    fn script_keeps_systemd_from_expanding_the_command() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        script("systemctl", "exit 0");
        let run = |help: &str| {
            script(
                "systemd-run",
                &format!(
                    "[ \"$1\" = --help ] && {{ echo '{help}'; exit 0; }}\n\
                     for a; do printf '%s\\n' \"$a\"; done"
                ),
            );
            let output = std::process::Command::new("sh")
                .args(CpuLimit::new("test", Share::ALL, 100).sh_args())
                .args(["sh", "-c", "l=${l%%@@*}"])
                .env_clear()
                .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        let new = run("  --expand-environment=BOOL    Control expansion");
        assert!(new.contains("\n--expand-environment=no\n--\nsh\n-c\nl=${l%%@@*}\n"), "{new}");
        let old = run("  --scope    Run as scope");
        assert!(!old.contains("--expand-environment"), "{old}");
        assert!(old.contains("\n--\nsh\n-c\nl=${l%%@@*}\n"), "{old}");
    }
}
