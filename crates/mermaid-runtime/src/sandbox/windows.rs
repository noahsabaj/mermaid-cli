//! Windows AppContainer and Job Object sandbox backend.
//!
//! Enforces network kill-switch and filesystem write-confinement via
//! native Windows AppContainers (LowBox tokens) and Job Objects.
//!
//! An AppContainer denies by default on both axes, so each half of the
//! policy that is NOT requested has to be granted back explicitly:
//!
//! - network: the three well-known network capability SIDs are attached to
//!   the container unless `deny_network` is set (without them a
//!   `--confine-fs`-only run could not reach the network at all);
//! - writes: with no `allowed_writes` the container is granted the current
//!   directory and the temp directory (without that a `--no-network`-only
//!   run could not write its own project). An AppContainer cannot be told
//!   "writes unconfined", so this is the documented floor.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use anyhow::Context as _;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::Isolation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Threading::*;

use super::SandboxPolicy;

/// Convert an `OsStr` / `Path` to a null-terminated UTF-16 vector for Win32 API calls.
fn to_wide_null(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// Convert a string slice to a null-terminated UTF-16 vector.
fn str_to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// RAII wrapper for an AppContainer profile and its `PSID`.
struct AppContainerProfileGuard {
    name_w: Vec<u16>,
    sid: PSID,
}

impl AppContainerProfileGuard {
    fn create(name: &str) -> anyhow::Result<Self> {
        let name_w = str_to_wide_null(name);
        let display_name_w = str_to_wide_null("Mermaid Sandbox");
        let desc_w = str_to_wide_null("Mermaid Sandboxed Process");
        let mut psid: PSID = null_mut();

        unsafe {
            let _ = DeleteAppContainerProfile(name_w.as_ptr());
        }

        let hr = unsafe {
            CreateAppContainerProfile(
                name_w.as_ptr(),
                display_name_w.as_ptr(),
                desc_w.as_ptr(),
                null(),
                0,
                &mut psid,
            )
        };
        if hr != 0 || psid.is_null() {
            anyhow::bail!("CreateAppContainerProfile failed (HRESULT {hr:#x})");
        }
        Ok(Self { name_w, sid: psid })
    }

    fn as_psid(&self) -> PSID {
        self.sid
    }
}

impl Drop for AppContainerProfileGuard {
    fn drop(&mut self) {
        unsafe {
            if !self.sid.is_null() {
                FreeSid(self.sid);
            }
            let _ = DeleteAppContainerProfile(self.name_w.as_ptr());
        }
    }
}

/// RAII wrapper for a Win32 `HANDLE`.
struct AutoHandle(HANDLE);

impl AutoHandle {
    fn new(h: HANDLE) -> Self {
        Self(h)
    }

    fn is_valid(&self) -> bool {
        !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for AutoHandle {
    fn drop(&mut self) {
        if self.is_valid() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// RAII guard that revokes one container SID's grant on a directory when
/// dropped.
///
/// It revokes the SID, not "restores the previous DACL": every sandboxed
/// command grants its own container SID on the same project root, and two
/// commands overlap whenever a background command is running. Restoring a
/// DACL captured before the second grant would strip the second command's
/// access mid-run (which is exactly how it surfaced: a test's redirect into
/// a granted directory failed with access denied while a sibling test's
/// guard dropped). Each container SID is unique to its run, so revoking it
/// touches nothing another command added.
struct DaclGuard {
    path_w: Vec<u16>,
    /// The container SID, copied: the guard may outlive the profile that
    /// produced it.
    sid: Vec<u8>,
}

impl Drop for DaclGuard {
    fn drop(&mut self) {
        let _ = edit_path_dacl(
            &mut self.path_w,
            self.sid.as_mut_ptr().cast(),
            REVOKE_ACCESS,
            0,
        );
    }
}

/// Read `path`'s DACL, apply one `EXPLICIT_ACCESS` entry for `sid`
/// (`GRANT_ACCESS` with `access_mask`, or `REVOKE_ACCESS`), and write it
/// back. Reads the DACL at call time rather than reusing an earlier copy, so
/// concurrent edits by other commands' guards compose instead of clobbering.
fn edit_path_dacl(
    path_w: &mut [u16],
    sid: PSID,
    mode: ACCESS_MODE,
    access_mask: u32,
) -> anyhow::Result<()> {
    let mut old_dacl: *mut ACL = null_mut();
    let mut sec_desc: PSECURITY_DESCRIPTOR = null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old_dacl,
            null_mut(),
            &mut sec_desc,
        )
    };
    anyhow::ensure!(
        status == ERROR_SUCCESS,
        "GetNamedSecurityInfoW failed: {status}"
    );

    let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
    ea.grfAccessPermissions = access_mask;
    ea.grfAccessMode = mode;
    ea.grfInheritance = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
    ea.Trustee.pMultipleTrustee = null_mut();
    ea.Trustee.MultipleTrusteeOperation = 0;
    ea.Trustee.TrusteeForm = TRUSTEE_IS_SID;
    ea.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
    ea.Trustee.ptstrName = sid as _;

    let mut new_dacl: *mut ACL = null_mut();
    let status = unsafe { SetEntriesInAclW(1, &ea, old_dacl, &mut new_dacl) };
    if status != ERROR_SUCCESS {
        unsafe {
            LocalFree(sec_desc as _);
        }
        anyhow::bail!("SetEntriesInAclW failed: {status}");
    }
    let status = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_dacl,
            null_mut(),
        )
    };
    unsafe {
        LocalFree(new_dacl as _);
        LocalFree(sec_desc as _);
    }
    anyhow::ensure!(
        status == ERROR_SUCCESS,
        "SetNamedSecurityInfoW failed: {status}"
    );
    Ok(())
}

/// Byte copy of a SID, so a guard can name its trustee after the profile
/// that produced the SID is gone.
fn copy_sid(sid: PSID) -> Vec<u8> {
    let len = unsafe { GetLengthSid(sid) } as usize;
    let mut buf = vec![0u8; len];
    unsafe {
        CopySid(len as u32, buf.as_mut_ptr().cast(), sid);
    }
    buf
}

/// Grant filesystem permissions to `sid` on `path`; the guard revokes them.
fn grant_path_access(
    path: &Path,
    sid: PSID,
    access_mask: u32,
) -> anyhow::Result<Option<DaclGuard>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut path_w = to_wide_null(path.as_os_str());
    edit_path_dacl(&mut path_w, sid, GRANT_ACCESS, access_mask)
        .with_context(|| format!("granting the sandbox access to {}", path.display()))?;
    Ok(Some(DaclGuard {
        path_w,
        sid: copy_sid(sid),
    }))
}

/// Quote a single command line argument according to standard Windows rules.
pub(crate) fn quote_windows_arg(arg: &OsStr) -> String {
    let s = arg.to_string_lossy();
    if s.is_empty() {
        return "\"\"".to_string();
    }
    if !s.contains([' ', '\t', '\n', '\x0b', '\"']) {
        return s.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in s.chars() {
        if c == '\\' {
            backslashes += 1;
        } else if c == '"' {
            for _ in 0..(backslashes * 2 + 1) {
                out.push('\\');
            }
            backslashes = 0;
            out.push('"');
        } else {
            for _ in 0..backslashes {
                out.push('\\');
            }
            backslashes = 0;
            out.push(c);
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Format an `argv` array into a single Windows command line string.
pub(crate) fn build_windows_cmdline(argv: &[OsString]) -> String {
    argv.iter()
        .map(|a| quote_windows_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Enable loopback exemption for the given AppContainer SID via FirewallAPI.dll.
fn enable_loopback_exemption(sid: PSID) {
    type FnNetworkIsolationSetAppContainerConfig =
        unsafe extern "system" fn(u32, *const SID_AND_ATTRIBUTES) -> u32;

    let dll_name = str_to_wide_null("FirewallAPI.dll");
    let h_mod = unsafe { LoadLibraryW(dll_name.as_ptr()) };
    if h_mod.is_null() {
        return;
    }
    let proc_name = c"NetworkIsolationSetAppContainerConfig";
    let proc = unsafe { GetProcAddress(h_mod, proc_name.as_ptr() as _) };
    if let Some(func) = proc {
        let set_config: FnNetworkIsolationSetAppContainerConfig =
            unsafe { std::mem::transmute(func) };
        let item = SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: 0,
        };
        unsafe {
            let _ = set_config(1, &item);
        }
    }
    unsafe {
        CloseHandle(h_mod as _);
    }
}

/// Check if Windows AppContainer support is available on this system.
pub(crate) fn appcontainer_available() -> bool {
    let name = "mermaid-probe";
    let name_w = str_to_wide_null(name);
    let mut psid: PSID = null_mut();
    let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name_w.as_ptr(), &mut psid) };
    if hr == 0 && !psid.is_null() {
        unsafe {
            FreeSid(psid);
        }
        true
    } else {
        false
    }
}

const WRITE_PERMISSIONS: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

/// Proc thread attribute identifiers.
const PROC_THREAD_ATTRIBUTE_HANDLE_LIST_ID: usize = 0x00020002;
const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES_ID: usize = 0x00020009;

/// The roots a container may write when the policy confines nothing: the
/// working directory and the temp directory. An AppContainer has no "writes
/// unconfined" setting -- it can only be granted paths -- so a policy that
/// asks for the network kill-switch alone still has to name what the child
/// may write, and these two are what an unconfined command would touch.
fn implicit_write_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    roots.push(std::env::temp_dir());
    roots
}

fn write_roots(policy: &SandboxPolicy) -> Vec<PathBuf> {
    if policy.allowed_writes.is_empty() {
        implicit_write_roots()
    } else {
        policy.allowed_writes.clone()
    }
}

fn setup_dacl_guards(policy: &SandboxPolicy, app_sid: PSID) -> anyhow::Result<Vec<DaclGuard>> {
    let mut guards = Vec::new();
    for dir in write_roots(policy) {
        if let Some(guard) = grant_path_access(&dir, app_sid, WRITE_PERMISSIONS)? {
            guards.push(guard);
        }
    }
    Ok(guards)
}

/// `SE_GROUP_ENABLED` from `winnt.h`. `windows-sys` files it under
/// `Win32_System_SystemServices`, a feature that drags in thousands of
/// unrelated constants for one flag; the value has been 0x4 since NT.
const SE_GROUP_ENABLED: u32 = 0x0000_0004;

/// A well-known capability SID, owned, for a `SECURITY_CAPABILITIES` list.
struct CapabilitySid {
    buf: Vec<u8>,
}

impl CapabilitySid {
    fn well_known(kind: WELL_KNOWN_SID_TYPE) -> anyhow::Result<Self> {
        let mut buf = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut len = buf.len() as u32;
        let ok = unsafe { CreateWellKnownSid(kind, null_mut(), buf.as_mut_ptr().cast(), &mut len) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            anyhow::bail!("CreateWellKnownSid({kind}) failed (error code {err})");
        }
        buf.truncate(len as usize);
        Ok(Self { buf })
    }

    fn as_psid(&self) -> PSID {
        self.buf.as_ptr().cast_mut().cast()
    }
}

/// The capability SIDs that let an AppContainer open network sockets:
/// internet client, internet client+server, private-network client+server.
/// A container without them has no network at all (the loopback exemption
/// aside), which is the kill-switch; a container with all three has what a
/// normal process has.
const NETWORK_CAPABILITIES: [WELL_KNOWN_SID_TYPE; 3] = [
    WinCapabilityInternetClientSid,
    WinCapabilityInternetClientServerSid,
    WinCapabilityPrivateNetworkClientServerSid,
];

/// Empty under `deny_network`; the three network capabilities otherwise.
fn network_capabilities(policy: &SandboxPolicy) -> anyhow::Result<Vec<CapabilitySid>> {
    if policy.deny_network {
        return Ok(Vec::new());
    }
    NETWORK_CAPABILITIES
        .iter()
        .map(|kind| CapabilitySid::well_known(*kind))
        .collect()
}

fn collect_stdio_handles(app_sid: PSID) -> (Vec<HANDLE>, HANDLE, HANDLE, HANDLE) {
    let h_stdin = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let h_stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let h_stderr = unsafe { GetStdHandle(STD_ERROR_HANDLE) };

    let mut handles = Vec::new();
    for &h in &[h_stdin, h_stdout, h_stderr] {
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            }
            grant_handle_access(h, app_sid);
            handles.push(h);
        }
    }
    (handles, h_stdin, h_stdout, h_stderr)
}

fn grant_handle_access(h: HANDLE, sid: PSID) {
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return;
    }
    unsafe {
        let mut p_old_dacl: *mut ACL = null_mut();
        let mut p_sec_desc: PSECURITY_DESCRIPTOR = null_mut();
        let status = GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut p_old_dacl,
            null_mut(),
            &mut p_sec_desc,
        );
        if status == ERROR_SUCCESS {
            let mut ea: EXPLICIT_ACCESS_W = std::mem::zeroed();
            ea.grfAccessPermissions = GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE;
            ea.grfAccessMode = GRANT_ACCESS;
            ea.grfInheritance = 0;
            ea.Trustee.TrusteeForm = TRUSTEE_IS_SID;
            ea.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
            ea.Trustee.ptstrName = sid as _;

            let mut p_new_dacl: *mut ACL = null_mut();
            if SetEntriesInAclW(1, &ea, p_old_dacl, &mut p_new_dacl) == ERROR_SUCCESS {
                let _ = SetSecurityInfo(
                    h,
                    SE_KERNEL_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    p_new_dacl,
                    null_mut(),
                );
                LocalFree(p_new_dacl as _);
            }
            if !p_sec_desc.is_null() {
                LocalFree(p_sec_desc as _);
            }
        }
    }
}

fn setup_proc_attributes(
    sec_cap: &mut SECURITY_CAPABILITIES,
    handles: &mut [HANDLE],
) -> anyhow::Result<(Vec<u8>, LPPROC_THREAD_ATTRIBUTE_LIST)> {
    let mut attr_size: usize = 0;
    unsafe {
        let _ = InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut attr_size);
    }
    let mut attr_buf = vec![0u8; attr_size];
    let attr_list = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    let ok = unsafe { InitializeProcThreadAttributeList(attr_list, 2, 0, &mut attr_size) };
    anyhow::ensure!(ok != 0, "InitializeProcThreadAttributeList failed");

    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES_ID,
            sec_cap as *mut _ as _,
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null_mut(),
        )
    };
    anyhow::ensure!(ok != 0, "UpdateProcThreadAttribute (sec cap) failed");

    if !handles.is_empty() {
        let ok = unsafe {
            UpdateProcThreadAttribute(
                attr_list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST_ID,
                handles.as_mut_ptr() as _,
                std::mem::size_of_val(handles),
                null_mut(),
                null_mut(),
            )
        };
        anyhow::ensure!(ok != 0, "UpdateProcThreadAttribute (handles) failed");
    }

    Ok((attr_buf, attr_list))
}

/// Resolve a program name to an absolute executable path using PATH and System32.
fn resolve_executable(program: &OsStr) -> PathBuf {
    let path = Path::new(program);
    if path.is_absolute() || path.components().count() > 1 {
        return path.to_path_buf();
    }
    let exts = [".exe", ".cmd", ".bat", ""];
    let prog_str = program.to_string_lossy();
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for ext in exts {
                let candidate = dir.join(format!("{prog_str}{ext}"));
                if candidate.is_file() && candidate.metadata().map(|m| m.len() > 0).unwrap_or(false)
                {
                    return candidate;
                }
            }
        }
    }
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        let sys32 = PathBuf::from(system_root).join("System32");
        for ext in exts {
            let candidate = sys32.join(format!("{prog_str}{ext}"));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    path.to_path_buf()
}

fn spawn_sandboxed_child(
    argv: &[OsString],
    startup_info_ex: &mut STARTUPINFOEXW,
    h_job: &AutoHandle,
) -> anyhow::Result<i32> {
    let mut resolved_argv = argv.to_vec();
    if let Some(prog) = argv.first() {
        resolved_argv[0] = resolve_executable(prog).into_os_string();
    }
    let cmdline = build_windows_cmdline(&resolved_argv);
    let mut cmdline_w = str_to_wide_null(&cmdline);
    let mut proc_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let created = unsafe {
        CreateProcessW(
            null(),
            cmdline_w.as_mut_ptr(),
            null(),
            null(),
            TRUE,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
            null(),
            null(),
            &startup_info_ex.StartupInfo,
            &mut proc_info,
        )
    };
    if created == 0 {
        let err = unsafe { GetLastError() };
        anyhow::bail!("CreateProcessW failed for '{cmdline}' (error code {err})");
    }

    let h_proc = AutoHandle::new(proc_info.hProcess);
    let h_thread = AutoHandle::new(proc_info.hThread);

    let assigned = unsafe { AssignProcessToJobObject(h_job.raw(), h_proc.raw()) };
    if assigned == 0 {
        let err = unsafe { GetLastError() };
        anyhow::bail!("AssignProcessToJobObject failed (error code {err})");
    }

    unsafe {
        ResumeThread(h_thread.raw());
    }
    drop(h_thread);

    unsafe {
        WaitForSingleObject(h_proc.raw(), INFINITE);
    }

    let mut exit_code: u32 = 0;
    unsafe {
        GetExitCodeProcess(h_proc.raw(), &mut exit_code);
    }

    Ok(exit_code as i32)
}

/// Run a command inside an ephemeral AppContainer and Job Object.
pub(crate) fn run_in_appcontainer(
    policy: &SandboxPolicy,
    argv: &[OsString],
) -> anyhow::Result<i32> {
    anyhow::ensure!(!argv.is_empty(), "cannot run empty argv");

    let nonce: u64 = {
        let mut b = [0u8; 8];
        let _ = getrandom::fill(&mut b);
        u64::from_ne_bytes(b)
    };
    let container_name = format!("mermaid-sandbox-{}-{:016x}", std::process::id(), nonce);
    let app_sid =
        AppContainerProfileGuard::create(&container_name).context("create AppContainer profile")?;

    enable_loopback_exemption(app_sid.as_psid());

    let _dacl_guards = setup_dacl_guards(policy, app_sid.as_psid())?;

    let capabilities = network_capabilities(policy)?;
    let mut capability_attrs: Vec<SID_AND_ATTRIBUTES> = capabilities
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.as_psid(),
            Attributes: SE_GROUP_ENABLED,
        })
        .collect();
    let mut sec_cap: SECURITY_CAPABILITIES = unsafe { std::mem::zeroed() };
    sec_cap.AppContainerSid = app_sid.as_psid();
    sec_cap.Capabilities = if capability_attrs.is_empty() {
        null_mut()
    } else {
        capability_attrs.as_mut_ptr()
    };
    sec_cap.CapabilityCount = capability_attrs.len() as u32;

    let (mut handles, h_stdin, h_stdout, h_stderr) = collect_stdio_handles(app_sid.as_psid());
    let (_attr_buf, attr_list) = setup_proc_attributes(&mut sec_cap, &mut handles)?;

    let mut startup_info_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup_info_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup_info_ex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup_info_ex.StartupInfo.hStdInput = h_stdin;
    startup_info_ex.StartupInfo.hStdOutput = h_stdout;
    startup_info_ex.StartupInfo.hStdError = h_stderr;
    startup_info_ex.lpAttributeList = attr_list;

    let h_job = AutoHandle::new(unsafe { CreateJobObjectW(null(), null()) });
    anyhow::ensure!(h_job.is_valid(), "CreateJobObjectW failed");

    let mut limit_info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limit_info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = unsafe {
        SetInformationJobObject(
            h_job.raw(),
            JobObjectExtendedLimitInformation,
            &limit_info as *const _ as _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    anyhow::ensure!(ok != 0, "SetInformationJobObject failed");

    let res = spawn_sandboxed_child(argv, &mut startup_info_ex, &h_job);
    unsafe {
        DeleteProcThreadAttributeList(attr_list);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_windows_arg_handles_plain_and_spaces() {
        assert_eq!(quote_windows_arg(OsStr::new("hello")), "hello");
        assert_eq!(
            quote_windows_arg(OsStr::new("hello world")),
            "\"hello world\""
        );
        assert_eq!(quote_windows_arg(OsStr::new("")), "\"\"");
    }

    #[test]
    fn quote_windows_arg_escapes_quotes_and_backslashes() {
        assert_eq!(
            quote_windows_arg(OsStr::new("say \"hello\"")),
            "\"say \\\"hello\\\"\""
        );
        assert_eq!(
            quote_windows_arg(OsStr::new(r"C:\Program Files\App\")),
            "\"C:\\Program Files\\App\\\\\""
        );
    }

    #[test]
    fn build_windows_cmdline_formats_multiple_args() {
        let argv = vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from("echo hello world"),
        ];
        assert_eq!(
            build_windows_cmdline(&argv),
            "cmd.exe /c \"echo hello world\""
        );
    }

    #[test]
    fn appcontainer_available_returns_true_on_modern_windows() {
        assert!(appcontainer_available());
    }

    #[test]
    fn appcontainer_executes_simple_command() {
        let policy = SandboxPolicy::default();
        let argv = vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from("echo 1"),
        ];
        let exit_code = run_in_appcontainer(&policy, &argv).expect("run in appcontainer");
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn appcontainer_enforces_write_confinement() {
        let base = std::env::temp_dir().join(format!(
            "mermaid-appcontainer-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let allowed = base.join("allowed");
        let outside = base.join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let policy = SandboxPolicy {
            deny_network: false,
            allowed_writes: vec![allowed.clone()],
        };

        let in_file = allowed.join("in.txt");
        let inside_argv = vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from(format!("echo hello > {}", in_file.display())),
        ];
        let code = run_in_appcontainer(&policy, &inside_argv).expect("inside write");
        assert_eq!(code, 0);
        assert!(in_file.exists());

        let out_file = outside.join("out.txt");
        let outside_argv = vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from(format!("echo hello > {}", out_file.display())),
        ];
        let code = run_in_appcontainer(&policy, &outside_argv).expect("outside write");
        assert_ne!(code, 0);
        assert!(!out_file.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `deny_network` strips every capability; otherwise the three network
    /// capabilities are attached, each a valid SID.
    #[test]
    fn network_capabilities_follow_the_policy() {
        let denied = network_capabilities(&SandboxPolicy {
            deny_network: true,
            allowed_writes: Vec::new(),
        })
        .expect("capabilities");
        assert!(denied.is_empty());

        let allowed = network_capabilities(&SandboxPolicy::default()).expect("capabilities");
        assert_eq!(allowed.len(), NETWORK_CAPABILITIES.len());
        for sid in &allowed {
            assert_ne!(unsafe { IsValidSid(sid.as_psid()) }, 0);
        }
    }

    /// A policy that names no write roots is `--no-network` alone: the child
    /// must still be able to write where an unconfined command would, so the
    /// working directory and the temp directory are granted. Asserted on the
    /// root list rather than by running a child: the grant propagates
    /// inheritable ACEs down the whole working directory, which in a test is
    /// the checkout (and its `target/`) while other test binaries are busy in
    /// it -- slow, and it raced once in CI. The grant mechanism itself is
    /// exercised by `appcontainer_enforces_write_confinement`.
    #[test]
    fn a_policy_without_write_roots_grants_cwd_and_temp() {
        let policy = SandboxPolicy {
            deny_network: true,
            allowed_writes: Vec::new(),
        };
        let roots = write_roots(&policy);
        assert!(roots.contains(&std::env::temp_dir()), "{roots:?}");
        assert!(
            roots.contains(&std::env::current_dir().unwrap()),
            "{roots:?}"
        );
        let explicit = SandboxPolicy {
            deny_network: true,
            allowed_writes: vec![std::env::temp_dir()],
        };
        assert_eq!(write_roots(&explicit), vec![std::env::temp_dir()]);
    }

    /// With write confinement alone the network stays reachable. The probe is
    /// System32's `curl.exe` by full path: every AppContainer can read
    /// System32, while PATH on a CI runner finds Git for Windows' copy first
    /// (access denied) and the runner's Python exits STATUS_DLL_NOT_FOUND
    /// before running a line,
    /// with stderr captured into the granted directory. That directory is a
    /// fresh one under temp, not temp itself: grants restore the previous DACL
    /// on drop, so two tests granting the same directory concurrently wipe
    /// each other's grant. A connect may fail for lack of a route on the
    /// runner; it must not fail with `Permission denied` / `WSAEACCES`
    /// (10013), the capability-omission signature.
    #[test]
    fn appcontainer_with_write_confinement_alone_keeps_the_network() {
        let dir = std::env::temp_dir().join(format!(
            "mermaid-appcontainer-net-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let policy = SandboxPolicy {
            deny_network: false,
            allowed_writes: vec![dir.clone()],
        };
        let out_path = dir.join("probe.txt");
        // One file for stdout and stderr of the whole group, plus a trailer
        // saying how curl exited, so a failure names its cause: "not
        // recognized" (curl not found), "Access is denied" (could not start
        // it), a curl message (it ran), or no file at all (the redirect into
        // the granted directory failed).
        let argv = vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from(format!(
                // `cd /d` first: the child inherits the test's working directory
                // (the checkout), which the container cannot read, and cmd will
                // run builtins but not launch an external program from a
                // directory it cannot see ("The current directory is invalid").
                // In production the working directory is the project root, which
                // the policy grants. No quotes around the paths: the argv quoter
                // escapes inner quotes as `\"`, which cmd.exe does not
                // understand, and the temp path has no spaces.
                "(cd /d {} && %SystemRoot%\\System32\\curl.exe -sS --max-time 5 -o NUL http://1.1.1.1/ && echo [curl ok] || echo [curl failed]) > {} 2>&1",
                dir.display(),
                out_path.display()
            )),
        ];
        let code = run_in_appcontainer(&policy, &argv).expect("run in appcontainer");
        let output = std::fs::read_to_string(&out_path);
        let _ = std::fs::remove_dir_all(&dir);
        let output =
            output.unwrap_or_else(|err| panic!("no probe output file (exit {code}): {err}"));
        assert!(
            output.contains("[curl ok]") || output.contains("curl:"),
            "the probe did not run (exit {code}): {output}"
        );
        assert!(
            !output.contains("Permission denied") && !output.contains("10013"),
            "connect refused by capability omission (exit {code}): {output}"
        );
    }

    /// Two commands granting the same directory: dropping the first guard
    /// must leave the second's grant in place. The previous guard restored
    /// the DACL it had captured before the second grant existed.
    #[test]
    fn dropping_one_grant_keeps_a_concurrent_grant_on_the_same_directory() {
        fn sid_string(sid: PSID) -> String {
            let mut w: *mut u16 = null_mut();
            assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut w) }, 0);
            let mut len = 0;
            while unsafe { *w.add(len) } != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(w, len) });
            unsafe {
                LocalFree(w.cast());
            }
            s
        }
        fn dacl_sddl(path: &Path) -> String {
            let path_w = to_wide_null(path.as_os_str());
            let mut sd: PSECURITY_DESCRIPTOR = null_mut();
            let rc = unsafe {
                GetNamedSecurityInfoW(
                    path_w.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    &mut sd,
                )
            };
            assert_eq!(rc, ERROR_SUCCESS);
            let mut w: *mut u16 = null_mut();
            assert_ne!(
                unsafe {
                    ConvertSecurityDescriptorToStringSecurityDescriptorW(
                        sd,
                        SDDL_REVISION_1,
                        DACL_SECURITY_INFORMATION,
                        &mut w,
                        null_mut(),
                    )
                },
                0
            );
            let mut len = 0;
            while unsafe { *w.add(len) } != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(w, len) });
            unsafe {
                LocalFree(w.cast());
                LocalFree(sd.cast());
            }
            s
        }

        let dir = std::env::temp_dir().join(format!(
            "mermaid-appcontainer-grants-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let first = AppContainerProfileGuard::create(&format!(
            "mermaid-test-grant-a-{}",
            std::process::id()
        ))
        .unwrap();
        let second = AppContainerProfileGuard::create(&format!(
            "mermaid-test-grant-b-{}",
            std::process::id()
        ))
        .unwrap();
        let first_sid = sid_string(first.as_psid());
        let second_sid = sid_string(second.as_psid());

        let guard_a = grant_path_access(&dir, first.as_psid(), WRITE_PERMISSIONS).unwrap();
        let guard_b = grant_path_access(&dir, second.as_psid(), WRITE_PERMISSIONS).unwrap();
        let both = dacl_sddl(&dir);
        assert!(
            both.contains(&first_sid) && both.contains(&second_sid),
            "{both}"
        );

        drop(guard_a);
        let after = dacl_sddl(&dir);
        assert!(
            !after.contains(&first_sid),
            "first grant survived its guard: {after}"
        );
        assert!(
            after.contains(&second_sid),
            "second grant was stripped: {after}"
        );

        drop(guard_b);
        let none = dacl_sddl(&dir);
        assert!(!none.contains(&second_sid), "{none}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Under `deny_network` the same curl probe must fail to connect. The
    /// Python probe this replaces exited STATUS_DLL_NOT_FOUND inside the
    /// container before running a line, so its `code != 0` passed without
    /// testing the kill-switch. `curl: (7)` is the could-not-connect class;
    /// the text after it reads `Permission denied` for WSAEACCES on the
    /// runners seen so far, but curl's wording is not asserted.
    #[test]
    fn appcontainer_enforces_network_isolation() {
        let dir = std::env::temp_dir().join(format!(
            "mermaid-appcontainer-deny-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let policy = SandboxPolicy {
            deny_network: true,
            allowed_writes: vec![dir.clone()],
        };
        let out_path = dir.join("probe.txt");
        let argv = vec![
            OsString::from("cmd.exe"),
            OsString::from("/c"),
            OsString::from(format!(
                "(cd /d {} && %SystemRoot%\\System32\\curl.exe -sS --max-time 5 -o NUL http://1.1.1.1/ && echo [curl ok] || echo [curl failed]) > {} 2>&1",
                dir.display(),
                out_path.display()
            )),
        ];
        let code = run_in_appcontainer(&policy, &argv).expect("run in appcontainer");
        let output = std::fs::read_to_string(&out_path);
        let _ = std::fs::remove_dir_all(&dir);
        let output =
            output.unwrap_or_else(|err| panic!("no probe output file (exit {code}): {err}"));
        assert!(
            output.contains("[curl failed]") && output.contains("curl: (7)"),
            "the connect was not refused (exit {code}): {output}"
        );
    }
}
