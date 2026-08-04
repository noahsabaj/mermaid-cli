#[cfg(any(unix, windows))]
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use sha2::{Digest, Sha256};

use crate::data_dir;

pub const DAEMON_TOKEN_ENV: &str = "MERMAID_DAEMON_TOKEN";

/// Default lifetime for a freshly minted pairing token. Tokens expire so a
/// leaked or forgotten token can't be replayed indefinitely; `--ttl-days 0`
/// opts out for long-lived automation.
pub const DEFAULT_PAIRING_TTL_DAYS: i64 = 30;

/// RFC3339 expiry `ttl_days` from now, or `None` when `ttl_days <= 0`
/// (never expires). Shared by the daemon `pair` command and the local CLI so
/// both honor the same TTL semantics.
pub fn pairing_expiry_from_now(ttl_days: i64) -> Option<String> {
    (ttl_days > 0).then(|| (chrono::Utc::now() + chrono::Duration::days(ttl_days)).to_rfc3339())
}

/// Clamp a *client-supplied* pairing TTL so a daemon socket caller can't mint a
/// never-expiring token by sending `ttl_days <= 0`: non-positive input becomes
/// the default TTL, positive values pass through. The local `mermaid pair` CLI
/// deliberately does **not** call this — its `--ttl-days 0` "never expires"
/// opt-out is an owner-only choice with no privilege boundary (#65).
pub fn clamp_pairing_ttl_days(ttl_days: i64) -> i64 {
    if ttl_days <= 0 {
        DEFAULT_PAIRING_TTL_DAYS
    } else {
        ttl_days
    }
}

pub fn daemon_socket_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("mermaidd.sock"))
}

pub fn generate_pairing_token() -> Result<(String, String)> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|err| anyhow::anyhow!("failed to generate pairing token: {}", err))?;
    let token = format!("mermaid_{}", general_purpose::URL_SAFE_NO_PAD.encode(bytes));
    let hash = hash_pairing_token(&token);
    Ok((token, hash))
}

pub fn hash_pairing_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    crate::hex_lower(&digest)
}

pub fn request_daemon_json(mut body: serde_json::Value) -> Result<serde_json::Value> {
    if body.get("auth").is_none()
        && let Ok(token) = std::env::var(DAEMON_TOKEN_ENV)
        && !token.trim().is_empty()
    {
        body["auth"] = serde_json::json!({ "token": token });
    }
    request_daemon_text(&body.to_string())
}

/// Write one request line and read back the single JSON response line. The
/// unix-socket and Windows named-pipe transports differ only in how the stream
/// is opened, so the wire exchange itself lives here once.
///
/// The response is exactly one JSON line followed by `\n`, written before the
/// server closes its end — so `read_line` completes on the newline and never
/// reaches the post-close read (which Windows surfaces as a `BrokenPipe` error
/// rather than a unix-style clean EOF).
#[cfg(any(unix, windows))]
fn daemon_exchange<S: Read + Write>(mut stream: S, line: &str) -> Result<serde_json::Value> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut response = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut response)?;
    let value: serde_json::Value =
        serde_json::from_str(response.trim()).context("daemon returned invalid JSON")?;
    if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        anyhow::bail!(
            "{}",
            value
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("daemon request failed")
        );
    }
    Ok(value)
}

pub fn request_daemon_text(line: &str) -> Result<serde_json::Value> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let socket = daemon_socket_path()?;
        let stream = UnixStream::connect(&socket)
            .with_context(|| format!("failed to connect to {}", socket.display()))?;
        daemon_exchange(stream, line)
    }

    #[cfg(windows)]
    {
        let pipe_name = daemon_pipe_name()?;
        let stream = open_daemon_pipe(&pipe_name)?;
        daemon_exchange(stream, line)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = line;
        anyhow::bail!("daemon IPC supports Unix sockets and Windows named pipes only")
    }
}

/// Open a STREAMING daemon connection: send one JSON line, return a
/// line-iterator over the responses. Used by `subscribe_task`, whose
/// connection stays open (ack line, then NDJSON events until the terminal
/// `result`) — `request_daemon_json` reads exactly one line and closes.
/// `auth.token` is injected from `MERMAID_DAEMON_TOKEN` like the one-shot
/// path.
pub fn subscribe_daemon_lines(
    mut body: serde_json::Value,
) -> Result<impl Iterator<Item = Result<String>>> {
    if body.get("auth").is_none()
        && let Ok(token) = std::env::var(DAEMON_TOKEN_ENV)
        && !token.trim().is_empty()
    {
        body["auth"] = serde_json::json!({ "token": token });
    }
    let line = body.to_string();

    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let socket = daemon_socket_path()?;
        let mut stream = UnixStream::connect(&socket)
            .with_context(|| format!("failed to connect to {}", socket.display()))?;
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let reader = BufReader::new(stream);
        Ok(reader.lines().map(|l| l.map_err(anyhow::Error::from)))
    }

    #[cfg(windows)]
    {
        let pipe_name = daemon_pipe_name()?;
        let mut stream = open_daemon_pipe(&pipe_name)?;
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let reader = BufReader::new(stream);
        Ok(reader.lines().map(|l| l.map_err(anyhow::Error::from)))
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = line;
        anyhow::bail!("daemon IPC supports Unix sockets and Windows named pipes only");
        #[allow(unreachable_code)]
        Ok(std::iter::empty().map(|(): ()| unreachable!()))
    }
}

/// Name of the per-user daemon control pipe for `sid`. Namespaced by the
/// user's SID so two users on one machine get distinct pipes (the analog of
/// the unix socket living in a per-user data dir) — the ACL from
/// [`pipe_sddl`] then enforces that separation, rather than merely naming it.
pub fn pipe_name_for_sid(sid: &str) -> String {
    format!(r"\\.\pipe\mermaidd-{sid}")
}

/// SDDL for the daemon pipe's DACL: protected (`P`, no inherited ACEs),
/// granting `GA` (generic all) to `SY` (LocalSystem) and to the owning user's
/// SID — and to no one else, since a DACL denies anything it doesn't grant.
/// This is the named-pipe analog of the 0600 unix socket + uid peer check
/// (#66). Remote access is separately refused via
/// `PIPE_REJECT_REMOTE_CLIENTS` on the server, not the DACL.
pub fn pipe_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})")
}

/// String SID (`S-1-5-21-…`) of the user this process runs as, read from the
/// process token. Both ends derive the pipe name from it, and the server bakes
/// it into the pipe ACL.
#[cfg(windows)]
pub fn current_user_sid() -> Result<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            anyhow::bail!("OpenProcessToken failed (error {})", GetLastError());
        }
        // Everything after the token opens runs in a closure so the handle is
        // closed on every path — success or bail.
        let result = (|| {
            let mut needed: u32 = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            anyhow::ensure!(
                needed > 0,
                "GetTokenInformation sizing call failed (error {})",
                GetLastError()
            );
            let mut buf = vec![0_u8; needed as usize];
            if GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) == 0
            {
                anyhow::bail!("GetTokenInformation failed (error {})", GetLastError());
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid_w: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid_w) == 0 {
                anyhow::bail!("ConvertSidToStringSidW failed (error {})", GetLastError());
            }
            let mut len = 0_usize;
            while *sid_w.add(len) != 0 {
                len += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(sid_w, len));
            LocalFree(sid_w.cast());
            Ok(sid)
        })();
        CloseHandle(token);
        result
    }
}

/// Control-pipe name for the current user (see [`pipe_name_for_sid`]).
#[cfg(windows)]
pub fn daemon_pipe_name() -> Result<String> {
    Ok(pipe_name_for_sid(&current_user_sid()?))
}

/// Owner-only pipe security for the daemon's listener. Owns the
/// `LocalAlloc`ed security descriptor built from [`pipe_sddl`]; hand
/// [`Self::attributes_ptr`] to `ServerOptions::create_with_security_attributes_raw`
/// while this guard is alive.
#[cfg(windows)]
pub struct PipeSecurity {
    descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
    attributes: windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
}

#[cfg(windows)]
impl PipeSecurity {
    pub fn owner_only() -> Result<Self> {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

        let sddl = pipe_sddl(&current_user_sid()?);
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            anyhow::bail!(
                "failed to build pipe security descriptor from `{}` (error {})",
                sddl,
                unsafe { GetLastError() }
            );
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        Ok(Self {
            descriptor,
            attributes,
        })
    }

    /// Pointer for `create_with_security_attributes_raw`. Taken fresh per call
    /// so moving the guard between calls stays sound; the descriptor it points
    /// at is heap-allocated and lives until the guard drops.
    pub fn attributes_ptr(&mut self) -> *mut core::ffi::c_void {
        (&raw mut self.attributes).cast()
    }
}

#[cfg(windows)]
impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.descriptor.cast());
        }
    }
}

/// Open the daemon control pipe as an ordinary duplex file handle, retrying
/// briefly on `ERROR_PIPE_BUSY` (all server instances momentarily taken — the
/// server stands up the next instance right after each accept, so busy windows
/// are tiny).
#[cfg(windows)]
fn open_daemon_pipe(pipe_name: &str) -> Result<std::fs::File> {
    const ATTEMPTS: u32 = 5;
    for attempt in 1..=ATTEMPTS {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe_name)
        {
            Ok(file) => return Ok(file),
            Err(err)
                if err.raw_os_error()
                    == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32)
                    && attempt < ATTEMPTS =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
            },
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed to connect to {pipe_name} (is mermaidd running?)")
                });
            },
        }
    }
    anyhow::bail!("daemon pipe {pipe_name} stayed busy after {ATTEMPTS} attempts")
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn pairing_token_hash_is_stable_and_not_plaintext() {
        let hash = hash_pairing_token("mermaid_test");
        assert_eq!(hash, hash_pairing_token("mermaid_test"));
        assert_ne!(hash, "mermaid_test");
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn generated_pairing_token_hash_matches_token() {
        let (token, hash) = generate_pairing_token().expect("token");
        assert!(token.starts_with("mermaid_"));
        assert_eq!(hash, hash_pairing_token(&token));
    }

    #[test]
    fn clamp_pairing_ttl_days_forces_expiry_for_non_positive() {
        assert_eq!(clamp_pairing_ttl_days(0), DEFAULT_PAIRING_TTL_DAYS);
        assert_eq!(clamp_pairing_ttl_days(-5), DEFAULT_PAIRING_TTL_DAYS);
        assert_eq!(clamp_pairing_ttl_days(7), 7);
        // The #65 property: a clamped non-positive ttl yields a NON-NULL expiry,
        // exactly as the daemon `pair` handler composes the two helpers.
        assert!(pairing_expiry_from_now(clamp_pairing_ttl_days(0)).is_some());
        assert!(pairing_expiry_from_now(clamp_pairing_ttl_days(-1)).is_some());
    }

    #[test]
    fn pipe_name_and_sddl_embed_the_sid() {
        let sid = "S-1-5-21-1-2-3-1000";
        assert_eq!(
            super::pipe_name_for_sid(sid),
            r"\\.\pipe\mermaidd-S-1-5-21-1-2-3-1000"
        );
        let sddl = super::pipe_sddl(sid);
        // Protected DACL granting only LocalSystem + the owner: exactly two
        // allow-ACEs, no deny/inherit clutter for the parser to misread.
        assert_eq!(sddl, "D:P(A;;GA;;;SY)(A;;GA;;;S-1-5-21-1-2-3-1000)");
    }

    // Windows-only: exercises the real token→SID→SDDL→descriptor FFI chain on
    // the Windows CI runner — the part a Linux build can't validate at all.
    #[cfg(windows)]
    #[test]
    fn current_user_sid_and_pipe_security_resolve() {
        let sid = super::current_user_sid().expect("current_user_sid");
        assert!(sid.starts_with("S-1-"), "unexpected SID shape: {sid}");
        let mut security = super::PipeSecurity::owner_only().expect("PipeSecurity");
        assert!(!security.attributes_ptr().is_null());
        assert!(super::daemon_pipe_name().expect("pipe name").contains(&sid));
    }
}
