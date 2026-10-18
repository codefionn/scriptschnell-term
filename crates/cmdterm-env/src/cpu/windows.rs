//! CPU limits on Windows, with job objects.
//!
//! Every limited terminal starts suspended, joins the app's slice job and
//! then a job of its own, nested in it, and only then runs, so nothing it
//! starts escapes. The slice job carries the hard cap of the [`Share`]
//! (`JobObjectCpuRateControlInformation`). A terminal's job carries what
//! systemd's weight and `AllowedCPUs` do on Linux: a lower priority class
//! and scheduling class for agents in hidden terminals, and an affinity for
//! the cores it may run on. The jobs have no names, so sandboxed processes
//! cannot open them; this process keeps them in [`SCOPES`] by unit name.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
    JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_AFFINITY,
    JOB_OBJECT_LIMIT_PRIORITY_CLASS, JOB_OBJECT_LIMIT_SCHEDULING_CLASS,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JobObjectBasicLimitInformation,
    JobObjectBasicProcessIdList, JobObjectCpuRateControlInformation, QueryInformationJobObject,
    SetInformationJobObject,
};
use windows_sys::Win32::System::SystemInformation::{
    CpuSetInformation, GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetProcessAffinityMask, IDLE_PRIORITY_CLASS,
    NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_SET_INFORMATION, SetPriorityClass,
};

use super::{CpuLimit, DEFAULT_WEIGHT, Share, Topology};

/// A job object, closed on drop.
pub(crate) struct Job {
    handle: HANDLE,
    /// The basic limits last set, so a change keeps the others.
    limits: Mutex<Limits>,
}

// SAFETY: a job handle is a process-wide token any thread may use, and the
// calls made through it are thread-safe.
unsafe impl Send for Job {}
// SAFETY: as above.
unsafe impl Sync for Job {}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateJobObjectW` and is owned here.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Limits {
    /// The priority class all processes of the job run at, once lowered.
    priority: Option<u32>,
    /// The scheduling class, 0 to 9, once lowered.
    scheduling: Option<u32>,
    /// The cores the job's processes may run on, once restricted.
    affinity: Option<usize>,
}

impl Job {
    fn new() -> Option<Self> {
        // SAFETY: null security attributes and name create an unnamed job
        // with the default security.
        let handle = unsafe { CreateJobObjectW(null(), null()) };
        (!handle.is_null()).then(|| Self {
            handle,
            limits: Mutex::new(Limits::default()),
        })
    }

    /// Sets the job's basic limits to `limits`. Without the privilege a
    /// priority class needs on some systems, it sets the processes' own
    /// priority classes instead, which their children inherit.
    fn set_limits(&self, limits: Limits) -> bool {
        let mut info = JOBOBJECT_BASIC_LIMIT_INFORMATION::default();
        if let Some(scheduling) = limits.scheduling {
            info.LimitFlags |= JOB_OBJECT_LIMIT_SCHEDULING_CLASS;
            info.SchedulingClass = scheduling;
        }
        if let Some(affinity) = limits.affinity {
            info.LimitFlags |= JOB_OBJECT_LIMIT_AFFINITY;
            info.Affinity = affinity;
        }
        let without_priority = info;
        if let Some(priority) = limits.priority {
            info.LimitFlags |= JOB_OBJECT_LIMIT_PRIORITY_CLASS;
            info.PriorityClass = priority;
        }
        let set = |info: &JOBOBJECT_BASIC_LIMIT_INFORMATION| {
            // SAFETY: `info` is a live struct of the size passed.
            unsafe {
                SetInformationJobObject(
                    self.handle,
                    JobObjectBasicLimitInformation,
                    std::ptr::from_ref(info).cast(),
                    size_of::<JOBOBJECT_BASIC_LIMIT_INFORMATION>() as u32,
                ) != 0
            }
        };
        let ok = set(&info)
            || (set(&without_priority) && {
                if let Some(priority) = limits.priority {
                    self.set_process_priorities(priority);
                }
                true
            });
        if ok && let Ok(mut current) = self.limits.lock() {
            *current = limits;
        }
        ok
    }

    fn set_process_priorities(&self, priority: u32) {
        for pid in self.pids() {
            // SAFETY: `OpenProcess` returns null or a handle closed below.
            unsafe {
                let process = OpenProcess(PROCESS_SET_INFORMATION, 0, pid);
                if !process.is_null() {
                    SetPriorityClass(process, priority);
                    CloseHandle(process);
                }
            }
        }
    }

    /// The ids of the job's processes, up to 1024.
    fn pids(&self) -> Vec<u32> {
        const MAX: usize = 1024;
        // A u64 buffer is aligned for the list, which starts with two u32.
        let mut buffer = vec![0u64; 1 + MAX / 2];
        let list = buffer
            .as_mut_ptr()
            .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
        // SAFETY: the buffer is as large as the size passed and aligned for
        // the list; the kernel fills in at most that much.
        unsafe {
            if QueryInformationJobObject(
                self.handle,
                JobObjectBasicProcessIdList,
                list.cast(),
                (buffer.len() * 8) as u32,
                null_mut(),
            ) == 0
            {
                return Vec::new();
            }
            let count = ((*list).NumberOfProcessIdsInList as usize).min(MAX);
            std::slice::from_raw_parts((*list).ProcessIdList.as_ptr(), count)
                .iter()
                .map(|pid| *pid as u32)
                .collect()
        }
    }
}

/// The job all limited terminals of this process run in, with the cap.
fn slice() -> Option<&'static Job> {
    static SLICE: OnceLock<Option<Job>> = OnceLock::new();
    SLICE.get_or_init(Job::new).as_ref()
}

/// Each terminal's job by its unit name, while the terminal holds it.
static SCOPES: Mutex<Option<HashMap<String, Weak<Job>>>> = Mutex::new(None);

/// A job for a terminal that starts under `limit`, registered under its
/// unit, with the slice's cap set to the limit's share. The terminal keeps
/// it while it runs.
pub(crate) fn scope(limit: &CpuLimit) -> Option<Arc<Job>> {
    let job = Arc::new(Job::new()?);
    let mut limits = Limits::default();
    if limit.weight < DEFAULT_WEIGHT {
        (limits.priority, limits.scheduling) = classes(limit.weight);
    }
    job.set_limits(limits);
    set_share(limit.share, None);
    if let Ok(mut scopes) = SCOPES.lock() {
        let scopes = scopes.get_or_insert_with(HashMap::new);
        scopes.retain(|_, job| job.strong_count() > 0);
        scopes.insert(limit.unit.clone(), Arc::downgrade(&job));
    }
    Some(job)
}

/// Puts the suspended `process` into the slice and then into `job`,
/// nested in it. False when Windows refused, such as before Windows 8,
/// which cannot nest jobs.
pub(crate) fn contain(process: HANDLE, job: &Job) -> bool {
    // SAFETY: both handles are valid; a failed call changes nothing.
    unsafe {
        if let Some(slice) = slice() {
            AssignProcessToJobObject(slice.handle, process);
        }
        AssignProcessToJobObject(job.handle, process) != 0
    }
}

/// The priority class and scheduling class that stand in for `weight`:
/// below normal for an agent at nice 10, idle from nice 15.
fn classes(weight: u32) -> (Option<u32>, Option<u32>) {
    if weight >= DEFAULT_WEIGHT {
        (Some(NORMAL_PRIORITY_CLASS), Some(5))
    } else if weight >= 10 {
        (Some(BELOW_NORMAL_PRIORITY_CLASS), Some(3))
    } else {
        (Some(IDLE_PRIORITY_CLASS), Some(1))
    }
}

/// The cores of this machine, from `GetSystemCpuSetInformation`: those
/// below the highest efficiency class are efficiency cores. Machines with
/// more than one processor group count all cores but place none.
pub(crate) fn topology() -> Option<Topology> {
    let mut length = 0u32;
    // SAFETY: a null buffer only asks for the length.
    unsafe {
        GetSystemCpuSetInformation(null_mut(), 0, &mut length, null_mut(), 0);
    }
    if length == 0 {
        return None;
    }
    let mut buffer = vec![0u64; (length as usize).div_ceil(8)];
    // SAFETY: the buffer holds `length` bytes, aligned for the entries.
    let ok = unsafe {
        GetSystemCpuSetInformation(
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
            null_mut(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    let bytes = buffer.as_ptr().cast::<u8>();
    let mut cores = Vec::new();
    let mut offset = 0usize;
    while offset + size_of::<u32>() * 2 <= length as usize {
        // SAFETY: each entry starts with its size and type, and the kernel
        // wrote `length` bytes of entries; the entry is read unaligned.
        let entry = unsafe {
            std::ptr::read_unaligned(bytes.add(offset).cast::<SYSTEM_CPU_SET_INFORMATION>())
        };
        if entry.Size == 0 {
            break;
        }
        if entry.Type == CpuSetInformation {
            // SAFETY: the type says the union holds a CPU set.
            let set = unsafe { entry.Anonymous.CpuSet };
            cores.push((set.Group, set.LogicalProcessorIndex, set.EfficiencyClass));
        }
        offset += entry.Size as usize;
    }
    if cores.is_empty() {
        return None;
    }
    if cores.iter().any(|(group, ..)| *group > 0) {
        return Some(Topology {
            online: (0..cores.len() as u32).collect(),
            efficiency: Vec::new(),
        });
    }
    let top = cores.iter().map(|(.., class)| *class).max();
    let mut online: Vec<u32> = cores
        .iter()
        .map(|(_, index, _)| u32::from(*index))
        .collect();
    let mut efficiency: Vec<u32> = cores
        .iter()
        .filter(|(.., class)| Some(*class) < top)
        .map(|(_, index, _)| u32::from(*index))
        .collect();
    online.sort_unstable();
    efficiency.sort_unstable();
    Some(Topology { online, efficiency })
}

/// The affinity mask of `cores`, or every core this process may use for
/// an empty list. `None` for cores past the first 64, which a job's basic
/// limits cannot name.
fn mask(cores: &[u32]) -> Option<usize> {
    if cores.is_empty() {
        let (mut process, mut system) = (0usize, 0usize);
        // SAFETY: the out pointers are live locals.
        let ok = unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) };
        return (ok != 0).then_some(process);
    }
    let mut mask = 0usize;
    for core in cores {
        mask |= 1usize.checked_shl(*core)?;
    }
    Some(mask)
}

/// Sets the hard cap of the slice to `share`. The cores a mode keeps free
/// are kept out of each terminal's job instead, so `_cores` goes unused.
pub(crate) fn set_share(share: Share, _cores: Option<&[u32]>) -> bool {
    slice().is_some_and(|slice| cap(slice.handle, share))
}

/// Caps `job` at `share` of this machine's cores, or lifts its cap for
/// every core.
fn cap(job: HANDLE, share: Share) -> bool {
    let cores = topology().map_or(1, |topology| topology.online.len() as u32);
    let allowed = share.cores(cores);
    let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
    if allowed < cores {
        info.ControlFlags =
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        // In hundredths of a percent of all cores.
        info.Anonymous.CpuRate = (allowed * 10_000 / cores.max(1)).max(1);
    }
    // SAFETY: `info` is a live struct of the size passed.
    unsafe {
        SetInformationJobObject(
            job,
            JobObjectCpuRateControlInformation,
            std::ptr::from_ref(&info).cast(),
            size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
        ) != 0
    }
}

/// Sets the weight and cores of the terminal whose job is `unit`. False
/// when this process started no such terminal or it ended.
pub(crate) fn set_scope(unit: &str, weight: u32, cores: Option<&[u32]>) -> bool {
    let job = SCOPES
        .lock()
        .ok()
        .and_then(|scopes| scopes.as_ref()?.get(unit)?.upgrade());
    let Some(job) = job else {
        return false;
    };
    let mut limits = job.limits.lock().map(|limits| *limits).unwrap_or_default();
    // Once lowered, the classes stay set, at normal when raised again, so
    // the processes that were lowered come back up.
    if weight < DEFAULT_WEIGHT || limits.priority.is_some() {
        (limits.priority, limits.scheduling) = classes(weight);
    }
    if let Some(cores) = cores
        && (!cores.is_empty() || limits.affinity.is_some())
    {
        limits.affinity = mask(cores);
    }
    job.set_limits(limits)
}

/// Gives a sandbox launcher's own job the cap of `share`, as it cannot
/// join the slice: a job with UI limits cannot nest.
pub(crate) fn cap_job(job: HANDLE, share: Share) {
    if share != Share::ALL {
        cap(job, share);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A limited terminal starts in its jobs, and its weight and cores
    /// change while it runs.
    #[test]
    fn limited_terminals_run_in_jobs_that_change() {
        let topology = topology().expect("the machine's cores");
        assert!(!topology.online.is_empty());
        let limit = CpuLimit::new(
            "wintest",
            Share {
                reserve: 1,
                percent: 100,
            },
            DEFAULT_WEIGHT,
        );
        let pty = crate::pty::Pty::open(crate::pty::PtySize::default()).unwrap();
        let mut command = crate::pty::CommandBuilder::new("cmd.exe");
        command.args(["/c", "ping -n 2 127.0.0.1 >nul"]);
        command.limit_cpu(Some(limit.clone()));
        let mut child = pty.spawn(&command).unwrap();
        assert!(set_scope(&limit.unit, 11, Some(&topology.online[..1])));
        let job = SCOPES.lock().unwrap().as_ref().unwrap()[&limit.unit]
            .upgrade()
            .unwrap();
        assert!(job.pids().contains(&child.pid()));
        assert!(set_scope(&limit.unit, DEFAULT_WEIGHT, Some(&[])));
        // Wine has no CPU rate control, so only Windows checks the cap.
        let _ = set_share(Share::ALL, None);
        assert!(child.wait().is_ok());
        assert!(!set_scope("nothing", DEFAULT_WEIGHT, None));
    }
}
