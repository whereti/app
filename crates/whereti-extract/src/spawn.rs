//! Starting a worker process (see `worker`), with a restricted token.
//!
//! The worker's own lockdown (no privileges, Low integrity) stops it from
//! writing anything, but it could still read whatever its parent can, and
//! the service's parent is SYSTEM. So the worker starts with a restricted
//! copy of its parent's token: no privilege but passing through folders,
//! every group but Everyone and Users only able to deny, and every access
//! must also be allowed to Everyone, Users, RESTRICTED or the logon
//! session. One user's files (a profile, ProgramData\whereti) are out of
//! its reach; what every user may read (Windows, Program Files) still is,
//! so Windows' OCR, picture and PDF code loads. It needs nothing else: the
//! document comes on stdin, and any file it needs as a handle.
//!
//! The process starts suspended and runs only once it is in its job.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    DuplicateHandle, SetHandleInformation, DUPLICATE_SAME_ACCESS, GENERIC_ALL, HANDLE, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::{
    AddAccessAllowedAce, AddMandatoryAce, CreateRestrictedToken, CreateWellKnownSid, DuplicateTokenEx, EqualSid, GetLengthSid, GetTokenInformation,
    InitializeAcl, InitializeSecurityDescriptor, SecurityImpersonation, SetSecurityDescriptorDacl, SetSecurityDescriptorSacl, SetTokenInformation,
    TokenDefaultDacl, TokenGroups, TokenPrimary, TokenUser, WinBuiltinUsersSid, WinLocalSystemSid, WinLowLabelSid, WinRestrictedCodeSid, WinWorldSid,
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, DISABLE_MAX_PRIVILEGE, SECURITY_ATTRIBUTES,
    SECURITY_DESCRIPTOR, SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, SYSTEM_MANDATORY_LABEL_ACE, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER,
    WELL_KNOWN_SID_TYPE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::{
    SECURITY_DESCRIPTOR_REVISION, SE_GROUP_INTEGRITY, SE_GROUP_LOGON_ID, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcessToken, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW, CREATE_SUSPENDED, EXTENDED_STARTUPINFO_PRESENT,
    INFINITE, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

/// A started worker: its process and our ends of its pipes.
pub(crate) struct Child {
    process: OwnedHandle,
    pub(crate) stdin: Option<File>,
    pub(crate) stdout: Option<File>,
    pub(crate) stderr: Option<File>,
}

impl Child {
    pub(crate) fn kill(&self) {
        // SAFETY: our handle to the process.
        unsafe { TerminateProcess(self.process.as_raw_handle() as HANDLE, 1) };
    }

    /// Waits at most `timeout`; `true` when it has ended.
    pub(crate) fn wait_for(&self, timeout: Duration) -> bool {
        let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
        // SAFETY: our handle to the process.
        unsafe { WaitForSingleObject(self.process.as_raw_handle() as HANDLE, ms) == WAIT_OBJECT_0 }
    }

    /// Waits for the end; its exit code.
    pub(crate) fn exit_code(&self) -> Option<i32> {
        let mut code = 0u32;
        // SAFETY: our handle to the process.
        let ok = unsafe {
            WaitForSingleObject(self.process.as_raw_handle() as HANDLE, INFINITE);
            GetExitCodeProcess(self.process.as_raw_handle() as HANDLE, &mut code)
        };
        (ok != 0).then_some(code as i32)
    }
}

/// Starts `exe` with `args`, suspended; puts it in `job` (see
/// `worker::Job`), duplicates `give` into it, then lets it run. Returns
/// the child and the values `give` has in it. `restricted`: with the
/// restricted token (else the parent's, as it is). `stderr`: keep its
/// errors (else they go nowhere).
pub(crate) fn spawn(
    exe: &Path,
    args: &[&OsStr],
    restricted: bool,
    stderr: bool,
    job: &crate::worker::Job,
    give: &[RawHandle],
) -> io::Result<(Child, Vec<u64>)> {
    let (in_read, in_write) = pipe()?;
    let (out_read, out_write) = pipe()?;
    let err = if stderr { Some(pipe()?) } else { None };
    let child_ends: Vec<HANDLE> =
        [&in_read, &out_write].into_iter().chain(err.as_ref().map(|e| &e.1)).map(|h| h.as_raw_handle() as HANDLE).collect();

    let token = if restricted { Some(restricted_token()?) } else { None };
    // a restricted token without the logon session (the service's) cannot
    // open the window station it would inherit: it gets one of its own
    let mut desktop: Option<Vec<u16>> = match &token {
        Some((_, logon)) if !*logon || OWN_DESKTOP.load(std::sync::atomic::Ordering::Relaxed) => Some(worker_desktop()?),
        _ => None,
    };

    let mut cmd: Vec<u16> = Vec::new();
    quote(exe.as_os_str(), &mut cmd);
    for a in args {
        cmd.push(b' ' as u16);
        quote(a, &mut cmd);
    }
    cmd.push(0);
    let app: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let dir: Option<Vec<u16>> = exe.parent().filter(|d| !d.as_os_str().is_empty()).map(|d| d.as_os_str().encode_wide().chain([0]).collect());

    // only the child's ends of its pipes are inherited, nothing else
    let mut size = 0usize;
    // SAFETY: a size query.
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
    let mut attrs = vec![0u64; size.div_ceil(8)];
    let list = attrs.as_mut_ptr().cast();
    // SAFETY: a buffer of the size asked for.
    if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
        return Err(os_error("attribute list"));
    }
    struct List(*mut core::ffi::c_void);
    impl Drop for List {
        fn drop(&mut self) {
            // SAFETY: a list we initialised.
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _list = List(list);
    // SAFETY: the handles live until CreateProcess returns.
    let ok = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            child_ends.as_ptr().cast(),
            child_ends.len() * std::mem::size_of::<HANDLE>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        return Err(os_error("handle list"));
    }
    for &h in &child_ends {
        // SAFETY: our own pipe handles; inheritable only until the child has them
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }
    // SAFETY: plain data, filled below.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = in_read.as_raw_handle() as HANDLE;
    si.StartupInfo.hStdOutput = out_write.as_raw_handle() as HANDLE;
    // no stderr: a null handle, which the worker's writes treat as nowhere
    si.StartupInfo.hStdError = err.as_ref().map_or(std::ptr::null_mut(), |e| e.1.as_raw_handle() as HANDLE);
    si.lpAttributeList = list;
    if let Some(d) = desktop.as_mut() {
        si.StartupInfo.lpDesktop = d.as_mut_ptr();
    }
    let flags = CREATE_SUSPENDED | CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS | EXTENDED_STARTUPINFO_PRESENT;
    // SAFETY: plain data, filled by CreateProcess.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let dir_ptr = dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
    let si_ptr = &si as *const STARTUPINFOEXW as *const STARTUPINFOW;
    // SAFETY: NUL-terminated strings, a valid startup info and attribute list.
    let ok = unsafe {
        match &token {
            Some((t, _)) => CreateProcessAsUserW(
                t.as_raw_handle() as HANDLE,
                app.as_ptr(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                std::ptr::null(),
                dir_ptr,
                si_ptr,
                &mut pi,
            ),
            None => CreateProcessW(app.as_ptr(), cmd.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 1, flags, std::ptr::null(), dir_ptr, si_ptr, &mut pi),
        }
    };
    let started = os_error("start the process");
    for &h in &child_ends {
        // SAFETY: as above
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
    }
    if ok == 0 {
        return Err(started);
    }
    // SAFETY: handles CreateProcess gave us, owned from here.
    let (process, thread) = unsafe { (OwnedHandle::from_raw_handle(pi.hProcess as RawHandle), OwnedHandle::from_raw_handle(pi.hThread as RawHandle)) };
    drop((in_read, out_write));
    let child = Child {
        process,
        stdin: Some(File::from(in_write)),
        stdout: Some(File::from(out_read)),
        stderr: err.map(|(read, write)| {
            drop(write);
            File::from(read)
        }),
    };
    if let Err(e) = job.assign(child.process.as_raw_handle()) {
        child.kill();
        return Err(io::Error::new(e.kind(), format!("job object: {e}")));
    }
    let mut given = Vec::new();
    for &h in give {
        let mut target: HANDLE = std::ptr::null_mut();
        // SAFETY: valid handles: ours, and the child process we just made
        let ok = unsafe {
            DuplicateHandle(GetCurrentProcess(), h as HANDLE, child.process.as_raw_handle() as HANDLE, &mut target, 0, 0, DUPLICATE_SAME_ACCESS)
        };
        if ok == 0 {
            let e = os_error("give a handle");
            child.kill();
            return Err(e);
        }
        given.push(target as usize as u64);
    }
    // SAFETY: the suspended main thread of our child.
    if unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) } == u32::MAX {
        let e = os_error("resume");
        child.kill();
        return Err(e);
    }
    Ok((child, given))
}

/// The last error of this thread, saying what failed.
fn os_error(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// A pipe, neither end inheritable: (read, write).
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: std::ptr::null_mut(), bInheritHandle: 0 };
    let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: two out-pointers, plain attributes.
    if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
        return Err(os_error("pipe"));
    }
    // SAFETY: fresh handles, owned from here.
    Ok(unsafe { (OwnedHandle::from_raw_handle(r as RawHandle), OwnedHandle::from_raw_handle(w as RawHandle)) })
}

/// Appends `arg` quoted as the C runtime reads a command line back.
fn quote(arg: &OsStr, out: &mut Vec<u16>) {
    let s: Vec<u16> = arg.encode_wide().collect();
    let (quote, slash, space, tab) = (b'"' as u16, b'\\' as u16, b' ' as u16, b'\t' as u16);
    if !s.is_empty() && !s.iter().any(|&c| c == space || c == tab || c == quote) {
        out.extend(s);
        return;
    }
    out.push(quote);
    let mut slashes = 0;
    for c in s {
        if c == slash {
            slashes += 1;
        } else {
            if c == quote {
                // the slashes before a quote, and the quote, are escaped
                out.extend(std::iter::repeat_n(slash, slashes + 1));
            }
            slashes = 0;
        }
        out.push(c);
    }
    out.extend(std::iter::repeat_n(slash, slashes));
    out.push(quote);
}

/// A well-known SID, in a buffer of its own.
fn well_known(kind: WELL_KNOWN_SID_TYPE) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut size = buf.len() as u32;
    // SAFETY: a buffer of the given size.
    if unsafe { CreateWellKnownSid(kind, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size) } == 0 {
        return Err(os_error("well-known SID"));
    }
    buf.truncate(size as usize);
    Ok(buf)
}

/// A token's information of one class, in an 8-aligned buffer.
fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
    let mut needed = 0u32;
    // SAFETY: a size query, then a buffer of that size.
    unsafe { GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut needed) };
    let mut buf = vec![0u64; (needed as usize).div_ceil(8) + 1];
    // SAFETY: the buffer holds at least `needed` bytes.
    if unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), needed, &mut needed) } == 0 {
        return Err(os_error("token information"));
    }
    Ok(buf)
}

/// An ACL allowing everything to each of `sids`, 8-aligned.
fn allow_all(sids: &[*mut core::ffi::c_void]) -> io::Result<Vec<u64>> {
    let ace = std::mem::size_of::<ACCESS_ALLOWED_ACE>() - 4;
    // SAFETY: valid SIDs.
    let size = std::mem::size_of::<ACL>() + sids.iter().map(|&s| ace + unsafe { GetLengthSid(s) } as usize).sum::<usize>();
    let size = size.next_multiple_of(4);
    let mut buf = vec![0u64; size.div_ceil(8)];
    let acl: *mut ACL = buf.as_mut_ptr().cast();
    // SAFETY: a buffer of `size` bytes, valid SIDs.
    unsafe {
        if InitializeAcl(acl, size as u32, ACL_REVISION) == 0 {
            return Err(os_error("ACL"));
        }
        for &sid in sids {
            if AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, sid) == 0 {
                return Err(os_error("ACL entry"));
            }
        }
    }
    Ok(buf)
}

/// For the tests: restricted workers get a window station of their own
/// even where the logon session's would do (the service's case, tried as
/// a user).
#[doc(hidden)]
pub static OWN_DESKTOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `window station\desktop` for restricted workers, made once by this
/// process and kept while it runs: only its user, SYSTEM and RESTRICTED may
/// use them. (user32, which Windows' PDF renderer loads, needs a desktop.)
fn worker_desktop() -> io::Result<Vec<u16>> {
    use windows_sys::Win32::System::StationsAndDesktops::{
        CreateDesktopW, CreateWindowStationW, GetProcessWindowStation, GetUserObjectInformationW, SetProcessWindowStation, UOI_NAME,
    };
    static MADE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(name) = made.as_ref() {
        return Ok(name.encode_utf16().chain([0]).collect());
    }
    let user = token_info_of_self(TokenUser)?;
    let mut restricted = well_known(WinRestrictedCodeSid)?;
    let mut system = well_known(WinLocalSystemSid)?;
    // SAFETY: TOKEN_USER as Windows wrote it.
    let user_sid = unsafe { (*(user.as_ptr() as *const TOKEN_USER)).User.Sid };
    let mut acl = allow_all(&[user_sid, restricted.as_mut_ptr().cast(), system.as_mut_ptr().cast()])?;
    // SAFETY: plain data, initialised by Windows below.
    let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let psd = (&mut sd as *mut SECURITY_DESCRIPTOR).cast();
    // labelled Low, as the worker is once locked down (an object without a
    // label counts as Medium, which a Low process may not use)
    let mut low = well_known(WinLowLabelSid)?;
    let low_sid: *mut core::ffi::c_void = low.as_mut_ptr().cast();
    // SAFETY: a valid SID.
    let size = (std::mem::size_of::<ACL>() + std::mem::size_of::<SYSTEM_MANDATORY_LABEL_ACE>() - 4 + unsafe { GetLengthSid(low_sid) } as usize).next_multiple_of(4);
    let mut sacl = vec![0u64; size.div_ceil(8)];
    // SAFETY: a descriptor pointing at our ACLs, each in a buffer of its size.
    unsafe {
        if InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION) == 0
            || SetSecurityDescriptorDacl(psd, 1, acl.as_mut_ptr().cast(), 0) == 0
            || InitializeAcl(sacl.as_mut_ptr().cast(), size as u32, ACL_REVISION) == 0
            || AddMandatoryAce(sacl.as_mut_ptr().cast(), ACL_REVISION, 0, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP, low_sid) == 0
            || SetSecurityDescriptorSacl(psd, 1, sacl.as_ptr().cast(), 0) == 0
        {
            return Err(os_error("desktop security"));
        }
    }
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: psd, bInheritHandle: 0 };
    let desk: Vec<u16> = "Default".encode_utf16().chain([0]).collect();
    // SAFETY: NUL-terminated names, valid attributes; the process's own
    // window station is set back at once. The handles stay open (never
    // closed) so the window station and desktop live as long as we do.
    let station = unsafe {
        // no name: Windows names it after the logon session (a name of our
        // own needs more rights as a user). A service's logon session has
        // one already, which it may not open like this: then a name of ours.
        let mut ws = CreateWindowStationW(std::ptr::null(), 0, GENERIC_ALL, &sa);
        if ws.is_null() {
            let own: Vec<u16> = format!("whereti-reader-{}", std::process::id()).encode_utf16().chain([0]).collect();
            ws = CreateWindowStationW(own.as_ptr(), 0, GENERIC_ALL, &sa);
        }
        if ws.is_null() {
            return Err(os_error("window station"));
        }
        let mut name = [0u16; 256];
        let mut needed = 0u32;
        if GetUserObjectInformationW(ws, UOI_NAME, name.as_mut_ptr().cast(), (name.len() * 2) as u32, &mut needed) == 0 {
            return Err(os_error("window station name"));
        }
        let ours = GetProcessWindowStation();
        if SetProcessWindowStation(ws) == 0 {
            return Err(os_error("switch window station"));
        }
        let d = CreateDesktopW(desk.as_ptr(), std::ptr::null(), std::ptr::null(), 0, GENERIC_ALL, &sa);
        let made_err = os_error("desktop");
        SetProcessWindowStation(ours);
        if d.is_null() {
            return Err(made_err);
        }
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        String::from_utf16_lossy(&name[..len])
    };
    let name = format!("{station}\\Default");
    *made = Some(name.clone());
    Ok(name.encode_utf16().chain([0]).collect())
}

/// This process's token information of one class.
fn token_info_of_self(class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
    let mut raw: HANDLE = std::ptr::null_mut();
    // SAFETY: our own token, closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(os_error("open token"));
    }
    // SAFETY: a handle we just opened.
    let token = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
    token_info(token.as_raw_handle() as HANDLE, class)
}

/// The restricted copy of this process's token (see the module comment),
/// and whether it has a logon session's SID (its window station's key).
fn restricted_token() -> io::Result<(OwnedHandle, bool)> {
    let mut raw: HANDLE = std::ptr::null_mut();
    // SAFETY: our own token, owned below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT, &mut raw) } == 0 {
        return Err(os_error("open token"));
    }
    // SAFETY: a handle we just opened.
    let token = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
    let groups = token_info(raw, TokenGroups)?;
    let user = token_info(raw, TokenUser)?;
    // SAFETY: TOKEN_USER and TOKEN_GROUPS as Windows wrote them in these buffers.
    let (user_sid, groups) = unsafe {
        let user = &*(user.as_ptr() as *const TOKEN_USER);
        let groups = &*(groups.as_ptr() as *const TOKEN_GROUPS);
        (user.User.Sid, std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize))
    };
    let mut everyone = well_known(WinWorldSid)?;
    let mut users = well_known(WinBuiltinUsersSid)?;
    let mut restricted = well_known(WinRestrictedCodeSid)?;
    let mut system = well_known(WinLocalSystemSid)?;
    let (everyone, users, restricted, system) =
        (everyone.as_mut_ptr().cast(), users.as_mut_ptr().cast(), restricted.as_mut_ptr().cast(), system.as_mut_ptr().cast());

    let mut deny_only = Vec::new();
    let mut logon = false;
    let mut allowed = vec![everyone, users, restricted];
    for g in groups {
        if g.Attributes & SE_GROUP_INTEGRITY as u32 != 0 {
            continue;
        }
        // the logon session: its window station and desktop
        if g.Attributes & SE_GROUP_LOGON_ID as u32 == SE_GROUP_LOGON_ID as u32 {
            allowed.push(g.Sid);
            logon = true;
            continue;
        }
        // SAFETY: valid SIDs.
        if unsafe { EqualSid(g.Sid, everyone) != 0 || EqualSid(g.Sid, users) != 0 } {
            continue;
        }
        deny_only.push(SID_AND_ATTRIBUTES { Sid: g.Sid, Attributes: 0 });
    }
    let restricting: Vec<SID_AND_ATTRIBUTES> = allowed.iter().map(|&s| SID_AND_ATTRIBUTES { Sid: s, Attributes: 0 }).collect();
    let mut new: HANDLE = std::ptr::null_mut();
    // SAFETY: valid lists of valid SIDs.
    let ok = unsafe {
        CreateRestrictedToken(
            raw,
            DISABLE_MAX_PRIVILEGE,
            deny_only.len() as u32,
            deny_only.as_ptr(),
            0,
            std::ptr::null(),
            restricting.len() as u32,
            restricting.as_ptr(),
            &mut new,
        )
    };
    if ok == 0 {
        return Err(os_error("restricted token"));
    }
    // SAFETY: the handle CreateRestrictedToken made.
    let new = unsafe { OwnedHandle::from_raw_handle(new as RawHandle) };
    drop(token);

    // The worker must open its own process and token (its lockdown, COM):
    // what it makes, and its token itself, allow its user and RESTRICTED.
    let mut acl = allow_all(&[user_sid, restricted, system])?;
    let dacl = TOKEN_DEFAULT_DACL { DefaultDacl: acl.as_mut_ptr().cast() };
    // SAFETY: a valid ACL for a token we own.
    if unsafe { SetTokenInformation(new.as_raw_handle() as HANDLE, TokenDefaultDacl, (&dacl as *const TOKEN_DEFAULT_DACL).cast(), std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32) } == 0 {
        return Err(os_error("default DACL"));
    }
    // SAFETY: plain data, initialised by Windows below.
    let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let psd = (&mut sd as *mut SECURITY_DESCRIPTOR).cast();
    // SAFETY: a descriptor pointing at our ACL.
    unsafe {
        if InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION) == 0 || SetSecurityDescriptorDacl(psd, 1, acl.as_ptr().cast(), 0) == 0 {
            return Err(os_error("token security"));
        }
    }
    // the token object itself gets that DACL as it is copied (its handle
    // may not change the DACL of the one CreateRestrictedToken made)
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: psd, bInheritHandle: 0 };
    let mut copy: HANDLE = std::ptr::null_mut();
    // SAFETY: a token we own, valid attributes.
    if unsafe { DuplicateTokenEx(new.as_raw_handle() as HANDLE, 0, &sa, SecurityImpersonation, TokenPrimary, &mut copy) } == 0 {
        return Err(os_error("copy the token"));
    }
    // SAFETY: the handle DuplicateTokenEx made.
    Ok((unsafe { OwnedHandle::from_raw_handle(copy as RawHandle) }, logon))
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn arguments_are_quoted_like_the_c_runtime_reads_them() {
        let q = |s: &str| {
            let mut out = Vec::new();
            quote(std::ffi::OsStr::new(s), &mut out);
            String::from_utf16(&out).unwrap()
        };
        assert_eq!(q("plain"), "plain");
        assert_eq!(q(""), "\"\"");
        assert_eq!(q(r"C:\Program Files\x"), r#""C:\Program Files\x""#);
        assert_eq!(q(r"a\ b\"), r#""a\ b\\""#);
        assert_eq!(q(r#"say "hi""#), r#""say \"hi\"""#);
    }
}
