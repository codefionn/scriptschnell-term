//! The Windows sandbox behind the `cmdterm-sandbox` launcher.
//!
//! The command runs with a write-restricted copy of the user's token. Its
//! restricting SIDs are a capability SID, the logon SID and Everyone, so a
//! write succeeds only where one of them is granted access, while reads are
//! checked as usual. The capability SID is derived from the `--profile`
//! name, so each project has its own and cannot write another's paths. The
//! legacy name `cmdterm-sandbox` is used without a profile, and a launch
//! with a profile removes the legacy SID's explicit entries from the paths
//! it touches. The launcher grants the SID modify rights (without the right
//! to delete any child of a directory) on each writable path, inherited by
//! everything below it, and keeps the grant for later runs. `--read-only`
//! paths get an inherited deny entry for the SID, so the sandbox cannot
//! write, delete or re-permission them. TEMP and TMP point to
//! `%TEMP%\cmdterm-sandbox\PROFILE`. An entry is written only when the
//! path's ACL lacks it. A path that cannot be changed is reported and
//! skipped. The command and its children run in a job object that closes
//! with the launcher, so nothing outlives the terminal, and that keeps it
//! from the user's other windows and from logging off, system parameter and
//! display changes and desktop switching (`ui.rs`). The clipboard stays
//! open. The command runs on a desktop of its own (`desktop.rs`); when it
//! cannot connect to that desktop, it runs again on the user's, with a
//! warning.
//! Container engine pipes reach the sandbox through relays the launcher
//! serves (`relay.rs`).

#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, GENERIC_ALL, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, LocalFree,
    SetHandleInformation,
};
use windows_sys::Win32::Security::Authorization::{
    DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE,
    REVOKE_ACCESS, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACCESS_DENIED_ACE, ACE_HEADER, ACL, ACL_REVISION, AddAccessAllowedAce,
    CopySid, CreateRestrictedToken, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
    DISABLE_MAX_PRIVILEGE, DeriveCapabilitySidsFromName, EqualSid, GetAce, GetLengthSid,
    GetTokenInformation, INHERITED_ACE, InitializeAcl, NO_INHERITANCE, PSID, SECURITY_MAX_SID_SIZE,
    SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT, SetTokenInformation,
    TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS,
    TOKEN_QUERY, TOKEN_USER, TokenDefaultDacl, TokenGroups, TokenUser, WRITE_RESTRICTED,
    WinLocalSystemSid, WinWorldSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA,
    WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, SE_GROUP_LOGON_ID,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess,
    GetExitCodeProcess, INFINITE, OpenProcessToken, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOW, WaitForSingleObject,
};

use crate::SandboxLaunch;

mod appcontainer;
mod desktop;
mod relay;
mod ui;

// The flags in `ui.rs` are winnt.h's.
const _: () = assert!(
    ui::HANDLES == JOB_OBJECT_UILIMIT_HANDLES
        && ui::READCLIPBOARD == JOB_OBJECT_UILIMIT_READCLIPBOARD
        && ui::WRITECLIPBOARD == JOB_OBJECT_UILIMIT_WRITECLIPBOARD
        && ui::SYSTEMPARAMETERS == JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
        && ui::DISPLAYSETTINGS == JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
        && ui::GLOBALATOMS == JOB_OBJECT_UILIMIT_GLOBALATOMS
        && ui::DESKTOP == JOB_OBJECT_UILIMIT_DESKTOP
        && ui::EXITWINDOWS == JOB_OBJECT_UILIMIT_EXITWINDOWS
);

/// The name the capability SID is derived from when there is no
/// `--profile`, and the folder of the temporary directories.
const LEGACY_PROFILE: &str = "cmdterm-sandbox";

/// Rights the sandbox gets on writable paths: Windows' own Modify right,
/// which is everything but changing permissions or ownership. It leaves out
/// `FILE_DELETE_CHILD`, so a child that denies `DELETE` stays.
const MODIFY: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

/// Rights denied on read-only paths: every way to change them.
const WRITE_DENIED: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | FILE_DELETE_CHILD
    | WRITE_DAC
    | WRITE_OWNER;

/// Rights denied on hidden paths.
const ALL_DENIED: u32 = FILE_ALL_ACCESS;

/// How the command is confined.
enum Isolation {
    /// A write-restricted token, with this capability SID as one of its
    /// restricting SIDs.
    Token(Handle, Sid),
    /// An AppContainer with this SID.
    AppContainer(Sid),
}

impl Isolation {
    /// The SID the sandbox's writable paths are granted to.
    fn sid(&self) -> &Sid {
        match self {
            Self::Token(_, sid) | Self::AppContainer(sid) => sid,
        }
    }
}

/// Runs `launch` in the sandbox and returns its exit code. With
/// `--appcontainer` it tries an AppContainer first and falls back to the
/// restricted token, saying why.
pub fn launch_sandboxed(launch: &SandboxLaunch) -> Result<u32, String> {
    let profile = launch
        .profile
        .as_deref()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name != LEGACY_PROFILE);
    if let Some(name) = &profile
        && (name.is_empty() || name.contains(['\\', '/', ':', '\0']) || name.starts_with('.'))
    {
        return Err(format!("{name:?} is not a usable profile name"));
    }
    // The legacy SID's entries come off every path this launch touches, so
    // a project does not keep what the shared SID was once granted.
    let legacy = match &profile {
        Some(_) => Some(capability_sid(LEGACY_PROFILE)?),
        None => None,
    };
    let legacy = legacy.as_ref();
    let root = std::env::temp_dir().join(LEGACY_PROFILE);
    let temp = match &profile {
        Some(name) => root.join(name),
        None => root.clone(),
    };
    std::fs::create_dir_all(&temp).map_err(|e| format!("{}: {e}", temp.display()))?;
    if let Some(legacy) = legacy {
        warn(set_access(&root, &[], Some(legacy)));
    }
    let writable: Vec<&Path> = launch
        .writable
        .iter()
        .map(|p| p.as_path())
        .chain([temp.as_path()])
        .filter(|p| p.exists())
        .collect();
    let appcontainer = launch.appcontainer.as_ref().and_then(|name| {
        let name = name.to_string_lossy();
        match appcontainer::prepare(&name, &writable, launch, legacy) {
            Ok(sid) => Some(Isolation::AppContainer(sid)),
            Err(reason) => {
                eprintln!(
                    "cmdterm-sandbox: not using the AppContainer {name}: {reason}; \
                     using the restricted token instead"
                );
                None
            }
        }
    });
    let isolation = match appcontainer {
        Some(isolation) => isolation,
        None => {
            let capability = capability_sid(profile.as_deref().unwrap_or(LEGACY_PROFILE))?;
            grant_writable(&capability, &writable, legacy);
            deny_writes(&capability, &launch.read_only, legacy);
            Isolation::Token(Handle(restricted_token(&capability)?), capability)
        }
    };
    let job = Handle(kill_on_close_job()?);
    if let Some(share) = launch.cpu {
        crate::cpu::windows::cap_job(job.0, share);
    }

    // The port hub is on the host's loopback, and the sandbox shares it, so
    // the launcher reports the ports of the job's processes itself.
    if let Some((port, token)) = &launch.ports_tcp {
        let job = job.0 as usize;
        let listeners = Box::new(move || appcontainer::job_listeners(job as HANDLE));
        let hub = ([127, 0, 0, 1], *port).into();
        if let Err(error) =
            crate::ports::run_bridge_tcp(hub, token.clone(), Vec::new(), listeners, false)
        {
            eprintln!(
                "cmdterm-sandbox: port hub 127.0.0.1:{port}: {error}; \
                 running without port forwarding"
            );
        }
    }

    // Relayed pipes come first, so the command's own variables win.
    let user = process_user()?;
    let mut vars = Vec::new();
    for (var, address) in &launch.pipes {
        match relay::start(address, &[&user, isolation.sid()]) {
            Ok(Some(relayed)) => vars.push((var.clone(), relayed.into())),
            Ok(None) => {}
            Err(error) => eprintln!("cmdterm-sandbox: {error}"),
        }
    }
    vars.extend(launch.vars.iter().cloned());
    vars.push(("TEMP".into(), temp.clone().into()));
    vars.push(("TMP".into(), temp.into()));
    let environment = environment_block(&launch.unset, &vars);
    let command_line = command_line(&launch.program, &launch.args);
    let cwd = launch.cwd.as_deref().map(|cwd| wide(cwd.as_os_str()));

    // The console's Ctrl+C belongs to the command. Ignoring it with a null
    // handler would be inherited, so a handler that swallows it is used.
    unsafe extern "system" fn swallow(_: u32) -> i32 {
        1
    }
    unsafe {
        SetConsoleCtrlHandler(Some(swallow), 1);
    }

    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags = STARTF_USESTDHANDLES;
    unsafe {
        startup.hStdInput = inheritable(GetStdHandle(STD_INPUT_HANDLE));
        startup.hStdOutput = inheritable(GetStdHandle(STD_OUTPUT_HANDLE));
        startup.hStdError = inheritable(GetStdHandle(STD_ERROR_HANDLE));
    }
    let run = |startup: &STARTUPINFOW| {
        run_contained(
            &isolation,
            &job,
            command_line.clone(),
            environment.clone(),
            cwd.as_deref(),
            startup,
        )
        .map_err(|error| format!("cannot start {}: {error}", launch.program.display()))
    };

    // A desktop of its own for the restricted token (`desktop.rs`). An
    // AppContainer runs at low integrity, which already keeps its input
    // from the user's windows, and would need the desktop labeled for it.
    let desktop = match &isolation {
        Isolation::Token(_, capability) => match desktop::Desktop::create(&user, capability) {
            Ok(desktop) => Some(desktop),
            Err(error) => {
                shared_desktop_warning(&root, &error);
                None
            }
        },
        Isolation::AppContainer(_) => None,
    };
    if let Some(mut desktop) = desktop {
        let mut own = startup;
        own.lpDesktop = desktop.startup_name();
        match run(&own) {
            Ok(code) if !ui::failed_on_desktop(code) => return Ok(code),
            Ok(code) => shared_desktop_warning(
                &root,
                &format!("the command could not connect to it (exit code {code:#x})"),
            ),
            Err(error) => shared_desktop_warning(&root, &error),
        }
    }
    run(&startup)
}

/// Tells the user, once per temporary directory root, that sandboxes run
/// on the user's own desktop, where they can send input to other windows.
fn shared_desktop_warning(root: &Path, reason: &str) {
    let marker = root.join("shared-desktop-warned");
    if marker.exists() {
        return;
    }
    let _ = std::fs::write(&marker, reason);
    eprintln!(
        "cmdterm-sandbox: not using a desktop of its own: {reason}; the command shares \
         your desktop and can send keystrokes to your windows"
    );
}

/// Starts the command suspended with `startup`, puts it in `job`, runs it
/// and returns its exit code.
fn run_contained(
    isolation: &Isolation,
    job: &Handle,
    mut command_line: Vec<u16>,
    mut environment: Vec<u16>,
    cwd: Option<&[u16]>,
    startup: &STARTUPINFOW,
) -> Result<u32, String> {
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    match isolation {
        Isolation::Token(token, _) => {
            let created = unsafe {
                CreateProcessAsUserW(
                    token.0,
                    null(),
                    command_line.as_mut_ptr(),
                    null(),
                    null(),
                    1,
                    CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                    environment.as_mut_ptr().cast(),
                    cwd.map_or(null(), |cwd| cwd.as_ptr()),
                    startup,
                    &mut info,
                )
            };
            if created == 0 {
                return Err(last_error());
            }
        }
        Isolation::AppContainer(sid) => unsafe {
            appcontainer::create_process(
                sid,
                &mut command_line,
                &mut environment,
                cwd,
                startup,
                &mut info,
            )?;
        },
    }
    let process = Handle(info.hProcess);
    let thread = Handle(info.hThread);
    unsafe {
        if AssignProcessToJobObject(job.0, process.0) == 0 {
            let error = last_error();
            windows_sys::Win32::System::Threading::TerminateProcess(process.0, 1);
            return Err(format!("cannot contain the process: {error}"));
        }
        ResumeThread(thread.0);
        WaitForSingleObject(process.0, INFINITE);
        let mut code = 1;
        GetExitCodeProcess(process.0, &mut code);
        Ok(code)
    }
}

/// Closes the handle when dropped.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// A SID in a buffer aligned for `SID`.
struct Sid(Vec<u32>);

impl Sid {
    fn as_psid(&self) -> PSID {
        self.0.as_ptr() as PSID
    }

    /// Copies the SID at `sid`.
    unsafe fn copy(sid: PSID) -> Result<Self, String> {
        unsafe {
            let len = GetLengthSid(sid);
            let mut buf = vec![0u32; (len as usize).div_ceil(4)];
            if CopySid(len, buf.as_mut_ptr().cast(), sid) == 0 {
                return Err(format!("cannot copy a SID: {}", last_error()));
            }
            Ok(Self(buf))
        }
    }

    fn well_known(kind: i32) -> Result<Self, String> {
        let mut buf = vec![0u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(4)];
        let mut len = SECURITY_MAX_SID_SIZE;
        let ok = unsafe { CreateWellKnownSid(kind, null_mut(), buf.as_mut_ptr().cast(), &mut len) };
        if ok == 0 {
            return Err(format!("cannot build a well-known SID: {}", last_error()));
        }
        Ok(Self(buf))
    }
}

/// The capability SID derived from `name`.
fn capability_sid(name: &str) -> Result<Sid, String> {
    let name = wide(OsStr::new(name));
    let mut groups: *mut PSID = null_mut();
    let mut group_count = 0;
    let mut sids: *mut PSID = null_mut();
    let mut sid_count = 0;
    unsafe {
        if DeriveCapabilitySidsFromName(
            name.as_ptr(),
            &mut groups,
            &mut group_count,
            &mut sids,
            &mut sid_count,
        ) == 0
        {
            return Err(format!("cannot derive the sandbox SID: {}", last_error()));
        }
        let sid = if sid_count > 0 {
            Sid::copy(*sids)
        } else {
            Err("no capability SID derived".to_owned())
        };
        for i in 0..group_count as usize {
            LocalFree(*groups.add(i));
        }
        for i in 0..sid_count as usize {
            LocalFree(*sids.add(i));
        }
        LocalFree(groups.cast());
        LocalFree(sids.cast());
        sid
    }
}

/// One entry `set_access` wants in an ACL.
struct Rule<'a> {
    sid: &'a Sid,
    rights: u32,
    /// A deny entry instead of an allow entry.
    deny: bool,
}

/// Prints `result`'s error as a warning. A path that cannot be changed does
/// not stop the launch.
fn warn(result: Result<(), String>) {
    if let Err(error) = result {
        eprintln!("cmdterm-sandbox: {error}");
    }
}

/// Grants `sid` modify rights on each of `paths`, warning about failures.
fn grant_writable(sid: &Sid, paths: &[&Path], legacy: Option<&Sid>) {
    let rule = [Rule {
        sid,
        rights: MODIFY,
        deny: false,
    }];
    for path in paths {
        warn(set_access(path, &rule, legacy));
    }
}

/// Denies `sid` every write on each of `paths` that exists, warning about
/// failures. The deny is added again when a host program replaced a file.
fn deny_writes(sid: &Sid, paths: &[PathBuf], legacy: Option<&Sid>) {
    let rule = [Rule {
        sid,
        rights: WRITE_DENIED,
        deny: true,
    }];
    for path in paths.iter().filter(|p| p.exists()) {
        warn(set_access(path, &rule, legacy));
    }
}

/// Makes the ACL of `path` hold every rule, and drops the explicit entries
/// of `legacy`. The ACL is read once and written once, and not at all when
/// it already holds everything: writing propagates through the whole tree
/// below a directory. New entries are inherited by what is below a
/// directory.
fn set_access(path: &Path, rules: &[Rule], legacy: Option<&Sid>) -> Result<(), String> {
    let name = wide(path.as_os_str());
    let fail = |what: &str, code: u32| format!("cannot {what} {}: error {code}", path.display());
    unsafe {
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor = null_mut();
        let code = GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        );
        if code != ERROR_SUCCESS {
            return Err(fail("read the permissions of", code));
        }
        // Grants of older versions carried FILE_DELETE_CHILD, which lets the
        // sandbox delete a read-only file through its parent. GRANT_ACCESS
        // would keep it, so such a grant is replaced with SET_ACCESS.
        let wider = |rule: &&Rule| {
            !rule.deny
                && rule.rights & FILE_DELETE_CHILD == 0
                && has_entry(dacl, rule.sid, false, FILE_DELETE_CHILD, true)
        };
        let missing: Vec<(&Rule, bool)> = rules
            .iter()
            .map(|rule| (rule, wider(&rule)))
            .filter(|(rule, wider)| {
                *wider || !has_entry(dacl, rule.sid, rule.deny, rule.rights, false)
            })
            .collect();
        let stale = legacy.filter(|sid| {
            has_entry(dacl, sid, false, 0, true) || has_entry(dacl, sid, true, 0, true)
        });
        let mut code = ERROR_SUCCESS;
        if !missing.is_empty() || stale.is_some() {
            let inheritance = if path.is_dir() {
                SUB_CONTAINERS_AND_OBJECTS_INHERIT
            } else {
                NO_INHERITANCE
            };
            let mut access: Vec<EXPLICIT_ACCESS_W> = missing
                .iter()
                .map(|(rule, wider)| {
                    let mode = match (rule.deny, wider) {
                        (true, _) => DENY_ACCESS,
                        (false, true) => SET_ACCESS,
                        (false, false) => GRANT_ACCESS,
                    };
                    explicit_access(rule.sid, rule.rights, mode, inheritance)
                })
                .collect();
            if let Some(sid) = stale {
                access.push(explicit_access(sid, 0, REVOKE_ACCESS, NO_INHERITANCE));
            }
            let mut updated: *mut ACL = null_mut();
            code = SetEntriesInAclW(access.len() as u32, access.as_ptr(), dacl, &mut updated);
            if code == ERROR_SUCCESS {
                // Without a protection flag the new entries propagate to
                // everything below that inherits.
                code = SetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    updated,
                    null(),
                );
                LocalFree(updated.cast());
            }
        }
        LocalFree(descriptor);
        if code != ERROR_SUCCESS {
            return Err(fail("change the sandbox's access to", code));
        }
    }
    Ok(())
}

fn explicit_access(sid: &Sid, rights: u32, mode: i32, inheritance: u32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: rights,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.as_psid().cast(),
        },
    }
}

/// True when `dacl` has an allow entry (or a deny entry, with `deny`) for
/// `sid` that covers all of `rights`. An entry with fewer rights, such as a
/// read grant, does not count. With `explicit`, only entries that were not
/// inherited count.
unsafe fn has_entry(dacl: *const ACL, sid: &Sid, deny: bool, rights: u32, explicit: bool) -> bool {
    if dacl.is_null() {
        return false;
    }
    let wanted = if deny {
        ACCESS_DENIED_ACE_TYPE
    } else {
        ACCESS_ALLOWED_ACE_TYPE
    };
    unsafe {
        for index in 0..(*dacl).AceCount as u32 {
            let mut ace: *mut c_void = null_mut();
            if GetAce(dacl, index, &mut ace) == 0 {
                continue;
            }
            let header = &*(ace as *const ACE_HEADER);
            if u32::from(header.AceType) != wanted {
                continue;
            }
            if explicit && u32::from(header.AceFlags) & INHERITED_ACE != 0 {
                continue;
            }
            // Both ACE types start with the header and a mask, then the SID.
            let (mask, ace_sid) = if deny {
                let ace = ace as *const ACCESS_DENIED_ACE;
                ((*ace).Mask, std::ptr::addr_of!((*ace).SidStart) as PSID)
            } else {
                let ace = ace as *const ACCESS_ALLOWED_ACE;
                ((*ace).Mask, std::ptr::addr_of!((*ace).SidStart) as PSID)
            };
            if mask & rights == rights && EqualSid(ace_sid, sid.as_psid()) != 0 {
                return true;
            }
        }
    }
    false
}

/// A write-restricted primary token whose new objects the sandbox, the
/// user and SYSTEM may use.
fn restricted_token(capability: &Sid) -> Result<HANDLE, String> {
    unsafe {
        let mut own = null_mut();
        let access = TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT;
        if OpenProcessToken(GetCurrentProcess(), access, &mut own) == 0 {
            return Err(format!("cannot open the process token: {}", last_error()));
        }
        let own = Handle(own);
        let user = token_info(own.0, TokenUser)?;
        let user = Sid::copy((*(user.as_ptr() as *const TOKEN_USER)).User.Sid)?;
        let groups = token_info(own.0, TokenGroups)?;
        let groups = &*(groups.as_ptr() as *const TOKEN_GROUPS);
        let groups = std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize);
        let logon = groups
            .iter()
            .find(|g| g.Attributes & SE_GROUP_LOGON_ID as u32 == SE_GROUP_LOGON_ID as u32)
            .map(|g| Sid::copy(g.Sid))
            .transpose()?;
        let everyone = Sid::well_known(WinWorldSid)?;
        let system = Sid::well_known(WinLocalSystemSid)?;

        let mut restricting = vec![capability, &everyone];
        restricting.extend(logon.as_ref());
        let restricting: Vec<SID_AND_ATTRIBUTES> = restricting
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid.as_psid(),
                Attributes: 0,
            })
            .collect();
        let mut token = null_mut();
        if CreateRestrictedToken(
            own.0,
            DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
            0,
            null(),
            0,
            null(),
            restricting.len() as u32,
            restricting.as_ptr(),
            &mut token,
        ) == 0
        {
            return Err(format!("cannot restrict the token: {}", last_error()));
        }
        let token = Handle(token);

        let mut owners = vec![&user, capability, &system];
        owners.extend(logon.as_ref());
        let size = size_of::<ACL>()
            + owners
                .iter()
                .map(|sid| size_of::<ACCESS_ALLOWED_ACE>() + GetLengthSid(sid.as_psid()) as usize)
                .sum::<usize>();
        let mut acl = vec![0u32; size.div_ceil(4)];
        let acl_ptr: *mut ACL = acl.as_mut_ptr().cast();
        if InitializeAcl(acl_ptr, size as u32, ACL_REVISION) == 0 {
            return Err(format!("cannot build the default ACL: {}", last_error()));
        }
        for sid in owners {
            if AddAccessAllowedAce(acl_ptr, ACL_REVISION, GENERIC_ALL, sid.as_psid()) == 0 {
                return Err(format!("cannot build the default ACL: {}", last_error()));
            }
        }
        let default = TOKEN_DEFAULT_DACL {
            DefaultDacl: acl_ptr,
        };
        if SetTokenInformation(
            token.0,
            TokenDefaultDacl,
            std::ptr::addr_of!(default).cast(),
            size_of::<TOKEN_DEFAULT_DACL>() as u32,
        ) == 0
        {
            return Err(format!("cannot set the default ACL: {}", last_error()));
        }
        let raw = token.0;
        std::mem::forget(token);
        Ok(raw)
    }
}

/// The SID of the user this process runs as.
fn process_user() -> Result<Sid, String> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(format!("cannot open the process token: {}", last_error()));
        }
        let token = Handle(token);
        let user = token_info(token.0, TokenUser)?;
        Sid::copy((*(user.as_ptr() as *const TOKEN_USER)).User.Sid)
    }
}

/// One `GetTokenInformation` class, in a buffer aligned for its struct.
unsafe fn token_info(token: HANDLE, class: i32) -> Result<Vec<u64>, String> {
    unsafe {
        let mut len = 0;
        GetTokenInformation(token, class, null_mut(), 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
            return Err(format!("cannot query the token: {}", last_error()));
        }
        Ok(buf)
    }
}

fn kill_on_close_job() -> Result<HANDLE, String> {
    unsafe {
        let job = CreateJobObjectW(null(), null());
        if job.is_null() {
            return Err(format!("cannot create a job: {}", last_error()));
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            let error = last_error();
            CloseHandle(job);
            return Err(format!("cannot configure the job: {error}"));
        }
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: ui::job_ui_limits(),
        };
        if SetInformationJobObject(
            job,
            JobObjectBasicUIRestrictions,
            std::ptr::addr_of!(ui).cast(),
            size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        ) == 0
        {
            eprintln!(
                "cmdterm-sandbox: cannot limit the job's access to the desktop: {}",
                last_error()
            );
        }
        Ok(job)
    }
}

/// Marks a standard handle inheritable so the command gets it.
unsafe fn inheritable(handle: HANDLE) -> HANDLE {
    unsafe {
        SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
    }
    handle
}

pub(crate) fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}

pub(crate) fn last_error() -> String {
    std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32).to_string()
}

/// This process's environment without `unset`, with `vars` on top, as a
/// sorted `KEY=VALUE\0...\0\0` block. Windows variable names ignore case.
pub(crate) fn environment_block(unset: &[OsString], vars: &[(OsString, OsString)]) -> Vec<u16> {
    let upper = |key: &OsStr| key.to_string_lossy().to_uppercase();
    let mut entries: Vec<(String, OsString, OsString)> = Vec::new();
    let all = std::env::vars_os().chain(vars.iter().cloned());
    for (key, value) in all {
        let name = upper(&key);
        if unset.iter().any(|u| upper(u) == name) {
            continue;
        }
        entries.retain(|(other, _, _)| *other != name);
        entries.push((name, key, value));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut block = Vec::new();
    for (_, key, value) in entries {
        block.extend(key.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// `program` and `args` quoted the way `CommandLineToArgvW` splits them.
pub(crate) fn command_line(program: &OsStr, args: &[OsString]) -> Vec<u16> {
    let mut line = Vec::new();
    for (i, arg) in std::iter::once(program)
        .chain(args.iter().map(|a| a.as_os_str()))
        .enumerate()
    {
        if i > 0 {
            line.push(u16::from(b' '));
        }
        quote(arg, &mut line);
    }
    line.push(0);
    line
}

fn quote(arg: &OsStr, out: &mut Vec<u16>) {
    let units: Vec<u16> = arg.encode_wide().collect();
    let special = |u: &u16| b" \t\n\"".iter().any(|c| *u == u16::from(*c));
    if !units.is_empty() && !units.iter().any(special) {
        out.extend(units);
        return;
    }
    out.push(u16::from(b'"'));
    let mut backslashes = 0;
    for unit in units {
        if unit == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        let doubled = if unit == u16::from(b'"') {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        out.extend(std::iter::repeat_n(u16::from(b'\\'), doubled));
        out.push(unit);
        backslashes = 0;
    }
    out.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    out.push(u16::from(b'"'));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(arg: &str) -> String {
        let mut out = Vec::new();
        quote(OsStr::new(arg), &mut out);
        String::from_utf16_lossy(&out)
    }

    #[test]
    fn arguments_are_quoted_for_the_c_runtime() {
        assert_eq!(quoted("a"), "a");
        assert_eq!(quoted(""), r#""""#);
        assert_eq!(quoted("b c"), r#""b c""#);
        assert_eq!(quoted(r#"x"y"#), r#""x\"y""#);
        assert_eq!(quoted(r"d\ e\"), r#""d\ e\\""#);
    }
}
