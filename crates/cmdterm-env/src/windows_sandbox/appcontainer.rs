//! The experimental AppContainer mode of the launcher.
//!
//! With `--appcontainer NAME` the command runs in an AppContainer instead of
//! under a write-restricted token. The profile `NAME` is created on first
//! use (`cmdterm-sandbox-<project id>`), and its SID gets modify rights on
//! the writable paths and read and execute rights on the `--read` ones. It
//! is denied writes on the `--read-only` paths, and all access to the
//! `--hide` paths that lie inside a path it may read or write, since a deny
//! entry is the only way to take back what a parent grant gives. A hidden
//! path elsewhere is left alone: the AppContainer cannot read the user
//! profile without a grant anyway.
//! The process starts with `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`
//! and no capabilities, so it has no `internetClient` and the only network
//! it reaches is the loopback addresses a loopback exemption allows: the
//! host's filtering proxy and nothing else. The exemption is machine-wide
//! state that needs an administrator the first time. When the profile or
//! the exemption fails, [`prepare`] returns the reason and the launcher
//! falls back to the restricted token. A path that cannot be granted or
//! denied is reported and skipped.
//!
//! The AppContainer shares the host's loopback, so the launcher runs the
//! bridge of the port protocol itself (`ports.rs`), outside the container.
//! It finds the sandbox's listeners in the TCP tables, limited to the
//! processes of the job ([`job_listeners`]).

use std::collections::BTreeSet;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, HANDLE};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6TABLE_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
    TCP_TABLE_OWNER_PID_LISTENER,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    EqualSid, FreeSid, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
use windows_sys::Win32::System::JobObjects::{
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JobObjectBasicProcessIdList, QueryInformationJobObject,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, STARTUPINFOEXW, STARTUPINFOW,
    UpdateProcThreadAttribute,
};

use super::{
    ALL_DENIED, Rule, Sid, deny_writes, grant_writable, last_error, set_access, warn, wide,
};
use crate::{SandboxLaunch, sandbox::path_is_inside};

/// Rights on `--read` paths: read and run, nothing else.
const READ_EXECUTE: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`.
const ALREADY_EXISTS: i32 = 0x8007_00B7_u32 as i32;

/// Sets up the AppContainer `name` and returns its SID: the profile, the
/// loopback exemption, the grants on `writable` and `launch.readable` paths
/// that exist, and the denies on `launch.read_only` and `launch.hidden`.
pub(super) fn prepare(
    name: &str,
    writable: &[&Path],
    launch: &SandboxLaunch,
    legacy: Option<&Sid>,
) -> Result<Sid, String> {
    let sid = profile_sid(name)?;
    exempt_loopback(&sid)?;
    grant_writable(&sid, writable, legacy);
    let read = [Rule {
        sid: &sid,
        rights: READ_EXECUTE,
        deny: false,
    }];
    let readable: Vec<&Path> = (launch.readable.iter())
        .map(PathBuf::as_path)
        .filter(|path| path.exists())
        .collect();
    for path in &readable {
        warn(set_access(path, &read, legacy));
    }
    deny_writes(&sid, &launch.read_only, legacy);
    let hide = [Rule {
        sid: &sid,
        rights: ALL_DENIED,
        deny: true,
    }];
    let granted: Vec<&Path> = writable.iter().chain(&readable).copied().collect();
    for path in &launch.hidden {
        let inside = granted.iter().any(|root| path_is_inside(path, root));
        if inside && path.exists() {
            warn(set_access(path, &hide, legacy));
        }
    }
    Ok(sid)
}

/// The SID of the profile `name`, which is created when it does not exist.
fn profile_sid(name: &str) -> Result<Sid, String> {
    let name = wide(std::ffi::OsStr::new(name));
    let description = wide(std::ffi::OsStr::new("cmdterm sandbox"));
    let mut sid: PSID = null_mut();
    unsafe {
        let mut result = CreateAppContainerProfile(
            name.as_ptr(),
            description.as_ptr(),
            description.as_ptr(),
            null(),
            0,
            &mut sid,
        );
        if result == ALREADY_EXISTS {
            result = DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid);
        }
        if result != 0 || sid.is_null() {
            return Err(format!(
                "cannot create the AppContainer profile: HRESULT {:#010x}",
                result as u32
            ));
        }
        let copy = Sid::copy(sid);
        FreeSid(sid);
        copy
    }
}

type GetConfig = unsafe extern "system" fn(*mut u32, *mut *mut SID_AND_ATTRIBUTES) -> u32;
type SetConfig = unsafe extern "system" fn(u32, *const SID_AND_ATTRIBUTES) -> u32;

/// `NetworkIsolationGetAppContainerConfig` and `...SetAppContainerConfig`,
/// looked up when first needed. Imported statically, they would keep every
/// binary that links this crate from starting where `firewallapi.dll` lacks
/// them, such as on Wine, although the mode is off by default.
fn network_isolation() -> Result<(GetConfig, SetConfig), String> {
    let library = wide(std::ffi::OsStr::new("firewallapi.dll"));
    // SAFETY: the names are NUL-terminated, and the two functions have
    // these signatures in every `firewallapi.dll` that exports them. The
    // library stays loaded for the life of the process.
    unsafe {
        let module = LoadLibraryW(library.as_ptr());
        if module.is_null() {
            return Err(format!("cannot load firewallapi.dll: {}", last_error()));
        }
        let get = GetProcAddress(
            module,
            c"NetworkIsolationGetAppContainerConfig".as_ptr().cast(),
        );
        let set = GetProcAddress(
            module,
            c"NetworkIsolationSetAppContainerConfig".as_ptr().cast(),
        );
        match (get, set) {
            (Some(get), Some(set)) => Ok((
                std::mem::transmute::<unsafe extern "system" fn() -> isize, GetConfig>(get),
                std::mem::transmute::<unsafe extern "system" fn() -> isize, SetConfig>(set),
            )),
            _ => Err("firewallapi.dll has no loopback exemptions".to_owned()),
        }
    }
}

/// Lets the AppContainer reach the loopback addresses, which the proxy and
/// the port hub listen on. The list is machine-wide and replaced as a
/// whole, so the SID is added to the current entries. Setting it needs an
/// administrator, and a SID that an earlier run added needs nothing.
fn exempt_loopback(sid: &Sid) -> Result<(), String> {
    let (get_config, set_config) = network_isolation()?;
    unsafe {
        let mut count = 0;
        let mut list: *mut SID_AND_ATTRIBUTES = null_mut();
        let code = get_config(&mut count, &mut list);
        if code != 0 {
            return Err(format!("cannot read the loopback exemptions: error {code}"));
        }
        // The list is not freed: the launcher is short-lived and the
        // documentation of its allocator is unclear.
        let mut entries: Vec<SID_AND_ATTRIBUTES> = if list.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(list, count as usize).to_vec()
        };
        if entries
            .iter()
            .any(|entry| EqualSid(entry.Sid, sid.as_psid()) != 0)
        {
            return Ok(());
        }
        entries.push(SID_AND_ATTRIBUTES {
            Sid: sid.as_psid(),
            Attributes: 0,
        });
        match set_config(entries.len() as u32, entries.as_ptr()) {
            0 => Ok(()),
            ERROR_ACCESS_DENIED => Err(
                "the loopback exemption needs an administrator once (error 5, access denied)"
                    .to_owned(),
            ),
            code => Err(format!("cannot add the loopback exemption: error {code}")),
        }
    }
}

/// Starts the suspended command in the AppContainer `sid`, with no
/// capabilities. `startup` carries the standard handles.
pub(super) unsafe fn create_process(
    sid: &Sid,
    command_line: &mut [u16],
    environment: &mut [u16],
    cwd: Option<&[u16]>,
    startup: &STARTUPINFOW,
    info: &mut PROCESS_INFORMATION,
) -> Result<(), String> {
    unsafe {
        let mut size = 0usize;
        // The first call only reports the size, and fails by design.
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
        let mut list = vec![0u64; size.div_ceil(8)];
        let list_ptr: *mut c_void = list.as_mut_ptr().cast();
        if InitializeProcThreadAttributeList(list_ptr, 1, 0, &mut size) == 0 {
            return Err(format!("cannot prepare the process: {}", last_error()));
        }
        let mut capabilities = SECURITY_CAPABILITIES {
            AppContainerSid: sid.as_psid(),
            Capabilities: null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };
        let updated = UpdateProcThreadAttribute(
            list_ptr,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            std::ptr::addr_of_mut!(capabilities).cast(),
            size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null(),
        );
        if updated == 0 {
            let error = last_error();
            DeleteProcThreadAttributeList(list_ptr);
            return Err(format!("cannot set the AppContainer: {error}"));
        }
        let mut extended: STARTUPINFOEXW = std::mem::zeroed();
        extended.StartupInfo = *startup;
        extended.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        extended.lpAttributeList = list_ptr;
        let created = CreateProcessW(
            null(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_mut_ptr().cast(),
            cwd.map_or(null(), |cwd| cwd.as_ptr()),
            std::ptr::addr_of!(extended.StartupInfo),
            info,
        );
        let error = (created == 0).then(last_error);
        DeleteProcThreadAttributeList(list_ptr);
        match error {
            None => Ok(()),
            Some(error) => Err(error),
        }
    }
}

/// The TCP ports that processes of `job` listen on, on loopback or every
/// address. Empty when the job cannot be read, such as after it closed.
pub(super) fn job_listeners(job: HANDLE) -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();
    let pids = job_pids(job);
    if pids.is_empty() {
        return ports;
    }
    if let Some(table) = tcp_table(u32::from(AF_INET)) {
        let table = table.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
        unsafe {
            let rows =
                std::slice::from_raw_parts((*table).table.as_ptr(), (*table).dwNumEntries as usize);
            for row in rows {
                // The address is in network order: 127.0.0.1 reads as 0x0100007F.
                let local = row.dwLocalAddr == 0 || row.dwLocalAddr == 0x0100_007F;
                if local && pids.contains(&row.dwOwningPid) {
                    ports.insert(u16::from_be(row.dwLocalPort as u16));
                }
            }
        }
    }
    if let Some(table) = tcp_table(u32::from(AF_INET6)) {
        let table = table.as_ptr().cast::<MIB_TCP6TABLE_OWNER_PID>();
        unsafe {
            let rows =
                std::slice::from_raw_parts((*table).table.as_ptr(), (*table).dwNumEntries as usize);
            for row in rows {
                let mut loopback = [0u8; 16];
                loopback[15] = 1;
                let local = row.ucLocalAddr == [0u8; 16] || row.ucLocalAddr == loopback;
                if local && pids.contains(&row.dwOwningPid) {
                    ports.insert(u16::from_be(row.dwLocalPort as u16));
                }
            }
        }
    }
    ports
}

/// The ids of the processes in `job`, up to 1024.
fn job_pids(job: HANDLE) -> Vec<u32> {
    const MAX: usize = 1024;
    // A u64 buffer is aligned for the list, which starts with two u32.
    let mut buffer = vec![0u64; 1 + MAX];
    unsafe {
        let list = buffer
            .as_mut_ptr()
            .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
        let ok = QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            list.cast(),
            (buffer.len() * 8) as u32,
            null_mut(),
        );
        if ok == 0 {
            return Vec::new();
        }
        let count = ((*list).NumberOfProcessIdsInList as usize).min(MAX);
        let first = std::ptr::addr_of!((*list).ProcessIdList).cast::<usize>();
        (0..count).map(|i| *first.add(i) as u32).collect()
    }
}

/// The listener table of an address family, in a buffer aligned for the
/// `MIB_TCP*TABLE_OWNER_PID` structures.
fn tcp_table(family: u32) -> Option<Vec<u64>> {
    let mut size = 0u32;
    for _ in 0..4 {
        let mut buffer = vec![0u64; (size as usize).div_ceil(8).max(1)];
        let code = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr().cast(),
                &mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        match code {
            0 => return Some(buffer),
            // The table grew between the calls, or this is the first call.
            ERROR_INSUFFICIENT_BUFFER => {}
            _ => return None,
        }
    }
    None
}
