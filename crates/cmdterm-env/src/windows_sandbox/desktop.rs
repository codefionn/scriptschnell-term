//! A desktop of its own for each sandboxed command, in the interactive
//! window station, which the user never switches to. Windows the command
//! opens land there, out of sight, and `SendInput` and hooks from there do
//! not reach the user's windows. The launcher stays on the user's desktop
//! and keeps the pseudoconsole: the command attaches to the launcher's
//! console as before, since a console belongs to the session, not to a
//! desktop. openai/codex's Windows sandbox runs commands on a private
//! desktop the same way (`desktop.rs` and `conpty/mod.rs` there).
//!
//! Only the user and the sandbox's capability SID get an entry. The user
//! gets every right, the capability SID what `ui::DESKTOP_PARTICIPANT`
//! lists: the write-restricted token passes an access check only when one
//! of its restricting SIDs is granted too, so another project's sandbox,
//! with another capability SID, cannot open this desktop.

use std::ptr::null;

use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, GetLengthSid, InitializeAcl,
    InitializeSecurityDescriptor, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    SetSecurityDescriptorDacl,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW, DESKTOP_ENUMERATE,
    DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READOBJECTS,
    DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, HDESK,
};
use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;

use super::{Sid, last_error, ui, wide};

// The rights in `ui.rs` are winuser.h's.
const _: () = assert!(
    ui::DESKTOP_READOBJECTS == DESKTOP_READOBJECTS
        && ui::DESKTOP_CREATEWINDOW == DESKTOP_CREATEWINDOW
        && ui::DESKTOP_CREATEMENU == DESKTOP_CREATEMENU
        && ui::DESKTOP_HOOKCONTROL == DESKTOP_HOOKCONTROL
        && ui::DESKTOP_JOURNALRECORD == DESKTOP_JOURNALRECORD
        && ui::DESKTOP_JOURNALPLAYBACK == DESKTOP_JOURNALPLAYBACK
        && ui::DESKTOP_ENUMERATE == DESKTOP_ENUMERATE
        && ui::DESKTOP_WRITEOBJECTS == DESKTOP_WRITEOBJECTS
        && ui::DESKTOP_SWITCHDESKTOP == DESKTOP_SWITCHDESKTOP
        && ui::DELETE == windows_sys::Win32::Storage::FileSystem::DELETE
        && ui::READ_CONTROL == windows_sys::Win32::Storage::FileSystem::READ_CONTROL
        && ui::WRITE_DAC == windows_sys::Win32::Storage::FileSystem::WRITE_DAC
        && ui::WRITE_OWNER == windows_sys::Win32::Storage::FileSystem::WRITE_OWNER
);

/// A desktop the launcher created, closed on drop. Windows destroys it
/// once no process and no handle uses it any more.
pub(super) struct Desktop {
    handle: HDESK,
    /// `WinSta0\NAME`, NUL-terminated, for `STARTUPINFO.lpDesktop`.
    startup: Vec<u16>,
}

impl Desktop {
    /// A new desktop that `user` owns and `capability` may use.
    pub(super) fn create(user: &Sid, capability: &Sid) -> Result<Self, String> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos() as u64);
        let name = ui::desktop_name(std::process::id(), nonce);
        let entries = [
            (user, ui::DESKTOP_ALL),
            (capability, ui::DESKTOP_PARTICIPANT),
        ];
        // SAFETY: the ACL buffer is sized for both entries and outlives the
        // security descriptor, which outlives `CreateDesktopW`; the SIDs
        // are valid for the call.
        unsafe {
            let size = size_of::<ACL>()
                + (entries.iter())
                    .map(|(sid, _)| {
                        size_of::<ACCESS_ALLOWED_ACE>() + GetLengthSid(sid.as_psid()) as usize
                    })
                    .sum::<usize>();
            let mut acl = vec![0u32; size.div_ceil(4)];
            let acl_ptr: *mut ACL = acl.as_mut_ptr().cast();
            if InitializeAcl(acl_ptr, size as u32, ACL_REVISION) == 0 {
                return Err(format!("cannot build the desktop's ACL: {}", last_error()));
            }
            for (sid, rights) in entries {
                if AddAccessAllowedAce(acl_ptr, ACL_REVISION, rights, sid.as_psid()) == 0 {
                    return Err(format!("cannot build the desktop's ACL: {}", last_error()));
                }
            }
            let mut descriptor: SECURITY_DESCRIPTOR = std::mem::zeroed();
            let descriptor_ptr = (&raw mut descriptor).cast();
            if InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) == 0
                || SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) == 0
            {
                return Err(format!(
                    "cannot describe the desktop's access: {}",
                    last_error()
                ));
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor_ptr,
                bInheritHandle: 0,
            };
            let wide_name = wide(name.as_ref());
            let handle = CreateDesktopW(
                wide_name.as_ptr(),
                null(),
                null(),
                0,
                ui::DESKTOP_ALL,
                &attributes,
            );
            if handle.is_null() {
                return Err(format!("cannot create a desktop: {}", last_error()));
            }
            Ok(Self {
                handle,
                startup: wide(ui::startup_desktop(&name).as_ref()),
            })
        }
    }

    /// The desktop for `STARTUPINFO.lpDesktop`, valid while `self` lives.
    pub(super) fn startup_name(&mut self) -> *mut u16 {
        self.startup.as_mut_ptr()
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateDesktopW` and is owned here.
        unsafe {
            CloseDesktop(self.handle);
        }
    }
}
