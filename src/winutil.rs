//! Service-side Windows helpers: sessions, launching the helper as SYSTEM in
//! the console session, job objects, SendSAS, pipe security.

use std::ffi::c_void;

use anyhow::{bail, Context, Result};
use windows::core::{s, w, HSTRING, PWSTR};
use windows::Win32::Foundation::{LocalFree, BOOL, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation,
    TokenPrimary, TokenSessionId, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_ACCESS_MASK,
    TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
    TOKEN_QUERY,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_INFORMATION, STARTUPINFOW,
};

pub use nya_server_core::win::{computer_name, is_elevated, Handle};

/// Session attached to the physical console, or None while switching.
pub fn active_console_session() -> Option<u32> {
    let s = unsafe { WTSGetActiveConsoleSessionId() };
    (s != 0xFFFF_FFFF).then_some(s)
}

/// A job object that kills its processes when the service exits.
pub fn kill_on_close_job() -> Result<Handle> {
    unsafe {
        let job = Handle(CreateJobObjectW(None, None)?);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )?;
        Ok(job)
    }
}

/// Launch `cmdline` in `session` with a copy of our (SYSTEM) token, on
/// `winsta0\default`. Returns the process handle.
pub fn spawn_in_session(session: u32, cmdline: &str, job: Option<&Handle>) -> Result<Handle> {
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_SESSIONID | TOKEN_ADJUST_DEFAULT,
            &mut own,
        )
        .context("OpenProcessToken")?;
        let own = Handle(own);
        let mut dup = HANDLE::default();
        DuplicateTokenEx(own.0, TOKEN_ACCESS_MASK(0x02000000), None, SecurityImpersonation, TokenPrimary, &mut dup)
            .context("DuplicateTokenEx")?;
        let dup = Handle(dup);
        SetTokenInformation(dup.0, TokenSessionId, &session as *const u32 as *const c_void, 4)
            .context("SetTokenInformation(TokenSessionId)")?;

        let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut cmd: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessAsUserW(
            dup.0,
            None,
            PWSTR(cmd.as_mut_ptr()),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            None,
            None,
            &si,
            &mut pi,
        )
        .context("CreateProcessAsUserW")?;
        let _thread = Handle(pi.hThread);
        let process = Handle(pi.hProcess);
        if let Some(job) = job {
            if let Err(e) = AssignProcessToJobObject(job.0, process.0) {
                tracing::warn!("AssignProcessToJobObject: {e}");
            }
        }
        Ok(process)
    }
}

/// Ctrl+Alt+Del. Requires running as a service (or SYSTEM) and the
/// `SoftwareSASGeneration` policy, which `install` sets.
pub fn send_sas() -> Result<()> {
    unsafe {
        let lib = LoadLibraryW(w!("sas.dll")).context("load sas.dll")?;
        let Some(f) = GetProcAddress(lib, s!("SendSAS")) else { bail!("SendSAS not found") };
        let f: unsafe extern "system" fn(BOOL) = std::mem::transmute(f);
        f(BOOL(0));
    }
    Ok(())
}

/// Only SYSTEM (the helper pipe).
pub const SYSTEM_ONLY: &str = "D:P(A;;GA;;;SY)";

/// Security attributes from an SDDL string (for named pipes).
pub struct PipeSa {
    pub sa: SECURITY_ATTRIBUTES,
    sd: PSECURITY_DESCRIPTOR,
}

unsafe impl Send for PipeSa {}

impl PipeSa {
    pub fn new(sddl: &str) -> Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(sddl), SDDL_REVISION_1, &mut sd, None)
                .context("security descriptor")?;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: BOOL(0),
        };
        Ok(Self { sa, sd })
    }
}

impl Drop for PipeSa {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(HLOCAL(self.sd.0));
        }
    }
}

/// Is the client of this pipe an elevated administrator (or SYSTEM)? Must be
/// called after reading from the pipe. Uses the client's own token, so a
/// non-elevated admin (UAC filtered token) is not an administrator here.
pub fn pipe_client_is_admin(pipe: &impl std::os::windows::io::AsRawHandle) -> bool {
    use windows::Win32::Security::{
        CheckTokenMembership, CreateWellKnownSid, RevertToSelf, WinBuiltinAdministratorsSid, PSID,
        SECURITY_MAX_SID_SIZE,
    };
    use windows::Win32::System::Pipes::ImpersonateNamedPipeClient;
    let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut len = sid.len() as u32;
    let psid = PSID(sid.as_mut_ptr() as *mut c_void);
    unsafe {
        if CreateWellKnownSid(WinBuiltinAdministratorsSid, None, psid, &mut len).is_err() {
            return false;
        }
        if let Err(e) = ImpersonateNamedPipeClient(HANDLE(pipe.as_raw_handle())) {
            tracing::warn!("ImpersonateNamedPipeClient: {e}");
            return false;
        }
        let mut member = BOOL(0);
        let ok = CheckTokenMembership(None, psid, &mut member).is_ok() && member.as_bool();
        if RevertToSelf().is_err() {
            // Never keep running as somebody else.
            std::process::abort();
        }
        ok
    }
}

/// Wait up to `ms` for a process to exit, then terminate it.
pub fn wait_or_kill(process: &Handle, ms: u32) {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
    unsafe {
        if WaitForSingleObject(process.0, ms) != WAIT_OBJECT_0 {
            tracing::warn!("helper did not exit in time; terminating");
            let _ = TerminateProcess(process.0, 1);
            let _ = WaitForSingleObject(process.0, 2000);
        }
    }
}

/// Token of the user logged on at the console (service mode). `None` in
/// standalone mode (no privilege) or when nobody is logged on.
pub fn console_user_token() -> Option<Handle> {
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    let session = active_console_session()?;
    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(session, &mut token).ok()? };
    Some(Handle(token))
}

/// Folder for files received from the client: the console user's
/// `Downloads\NyaRemoteControl` (or the current user's in standalone mode).
pub fn receive_dir() -> std::path::PathBuf {
    let token = console_user_token();
    nya_win::shell::receive_dir(token.as_ref().map(|t| t.0))
        .or_else(|| nya_win::shell::receive_dir(None))
        .unwrap_or_else(|| std::env::temp_dir().join("NyaRemoteControl"))
}
