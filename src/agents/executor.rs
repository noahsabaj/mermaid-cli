use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

use crate::agents::ActionResult;
use crate::constants::{COMMAND_MAX_TIMEOUT_SECS, COMMAND_TIMEOUT_SECS};

/// Execute a shell command and capture output
///
/// Returns ActionResult directly - errors are captured in ActionResult::Error.
/// `timeout_secs` overrides the default 30-second timeout (capped at 300s).
pub async fn execute_command(
    command: &str,
    working_dir: Option<&str>,
    timeout_secs: Option<u64>,
) -> ActionResult {
    // Security checks
    if contains_dangerous_command(command) {
        return ActionResult::Error {
            error: format!("Dangerous command blocked: {}", command),
        };
    }

    // Parse the command
    let shell = if cfg!(target_os = "windows") {
        "cmd"
    } else {
        "sh"
    };

    let shell_arg = if cfg!(target_os = "windows") {
        "/C"
    } else {
        "-c"
    };

    // Create the command
    let mut cmd = Command::new(shell);
    cmd.arg(shell_arg)
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Set working directory if specified
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    // Execute with timeout (default COMMAND_TIMEOUT_SECS, max COMMAND_MAX_TIMEOUT_SECS)
    let secs = timeout_secs
        .unwrap_or(COMMAND_TIMEOUT_SECS)
        .min(COMMAND_MAX_TIMEOUT_SECS);
    let timeout_duration = Duration::from_secs(secs);

    match timeout(timeout_duration, run_command(cmd)).await {
        Ok(Ok(output)) => ActionResult::Success { output },
        Ok(Err(e)) => ActionResult::Error {
            error: format!("Command failed: {}", e),
        },
        Err(_) => ActionResult::Error {
            error: format!(
                "Command timed out after {} seconds. If this is a server or long-running process, it is likely still running in the background.",
                timeout_duration.as_secs()
            ),
        },
    }
}

/// Run the command and capture output
///
/// Reads stdout and stderr concurrently to prevent deadlock when the child
/// process fills one pipe's buffer while we're blocked reading the other.
async fn run_command(mut cmd: Command) -> Result<String> {
    let mut child = cmd
        .spawn()
        .context("Failed to execute command. Is the shell available?")?;

    let stdout = child
        .stdout
        .take()
        .context("Command process stdout stream not available. This is likely a bug.")?;
    let stderr = child
        .stderr
        .take()
        .context("Command process stderr stream not available. This is likely a bug.")?;

    // Read stdout and stderr concurrently to avoid deadlock
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        let mut output = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            output.push_str(&line);
            output.push('\n');
        }
        output
    });

    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut errors = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            errors.push_str(&line);
            errors.push('\n');
        }
        errors
    });

    let output = stdout_task.await.unwrap_or_default();
    let errors = stderr_task.await.unwrap_or_default();

    let status = child
        .wait()
        .await
        .context("Failed to wait for command to complete. Process may have crashed.")?;

    // Combine output and errors
    let mut full_output = output;
    if !errors.is_empty() {
        full_output.push_str("\n--- stderr ---\n");
        full_output.push_str(&errors);
    }

    if !status.success() {
        full_output.push_str(&format!(
            "\n--- Command exited with status: {} ---",
            status.code().unwrap_or(-1)
        ));
    }

    Ok(full_output)
}

/// Check if a command contains dangerous operations
///
/// This is a **defense-in-depth** blocklist, NOT a security boundary.
/// The AI model is the trusted actor making tool calls. This blocklist
/// catches obvious destructive patterns that are almost never intentional,
/// but it cannot prevent all dangerous commands (encoded, obfuscated, or
/// via scripting languages). The real security boundary is the user's
/// decision to run Mermaid with filesystem/shell access.
fn contains_dangerous_command(command: &str) -> bool {
    let dangerous_patterns = [
        "rm -rf /",
        "rm -rf /*",
        "dd if=/dev/zero of=/",
        "dd if=/dev/random of=/",
        "dd if=/dev/urandom of=/",
        "mkfs.",
        "format c:",
        "> /dev/sda",
        "chmod -R 777 /",
        "chmod -R 000 /",
        ":(){ :|:& };:", // Fork bomb (with spaces)
        ":(){ :|:&};:",  // Fork bomb (no space before colon)
        "curl | bash",
        "curl | sh",
        "wget | bash",
        "wget | sh",
        "nc -l",             // Netcat listener
        "ncat -l",           // Nmap netcat listener
        "socat tcp-listen:", // Socat listener
    ];

    let lower_command = command.to_lowercase();

    for pattern in &dangerous_patterns {
        if lower_command.contains(pattern) {
            return true;
        }
    }

    // Check for attempts to delete/modify system directories
    // Use word-boundary-aware matching to avoid false positives like
    // ".mermaid" containing "rm" or "2>/dev/null" matching "/dev"
    let system_dir_patterns = [
        ("/etc", false),
        ("/usr", false),
        ("/boot", false),
        ("/proc", false),
        ("/sys", false),
        ("/dev/", true), // Trailing slash: /dev/sda, /dev/null won't false-positive on "/dev" alone
        ("/home", false),
        ("C:\\Windows", false),
        ("C:\\Program Files", false),
        ("C:\\Users", false),
    ];

    // Only match "rm" or "del" as standalone command words, not substrings
    let has_rm_command = lower_command.starts_with("rm ")
        || lower_command.contains(" rm ")
        || lower_command.contains(";rm ")
        || lower_command.contains("&rm ")
        || lower_command.contains("|rm ");
    let has_del_command = lower_command.starts_with("del ")
        || lower_command.contains(" del ")
        || lower_command.contains(";del ")
        || lower_command.contains("&del ");

    if has_rm_command || has_del_command {
        for (dir, require_trailing) in &system_dir_patterns {
            if *require_trailing {
                // For /dev/, check the dir appears as a target, not in redirects like 2>/dev/null
                if command.contains(dir)
                    && !command.contains(&format!("{}null", dir))
                    && !command.contains(&format!("{}zero", dir))
                {
                    return true;
                }
            } else if command.contains(dir) {
                return true;
            }
        }

        // Check for home directory via shell expansion (~, $HOME)
        // Match ~ only as standalone word (preceded by space), not as suffix like "file~"
        if command.contains(" ~/")
            || command.ends_with(" ~")
            || command.contains(" ~ ")
            || command.contains("$HOME")
        {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_safe_command() {
        let result = execute_command("echo 'Hello, Mermaid!'", None, None).await;

        match result {
            ActionResult::Success { output } => {
                assert!(output.contains("Hello, Mermaid!"));
            },
            _ => panic!("Expected success"),
        }
    }

    #[tokio::test]
    async fn test_dangerous_command_blocked() {
        let result = execute_command("rm -rf /", None, None).await;

        match result {
            ActionResult::Error { error } => {
                assert!(error.contains("Dangerous command blocked"));
            },
            _ => panic!("Expected error"),
        }
    }

    #[test]
    fn test_dangerous_command_detection() {
        assert!(contains_dangerous_command("rm -rf /"));
        assert!(contains_dangerous_command("format c:"));
        assert!(contains_dangerous_command(":(){ :|:& };:"));
        assert!(contains_dangerous_command(":(){ :|:&};:")); // No space variant
        assert!(!contains_dangerous_command("ls -la"));
        assert!(!contains_dangerous_command("cargo build"));

        // Netcat/socat listeners
        assert!(contains_dangerous_command("ncat -l 8080"));
        assert!(contains_dangerous_command("socat tcp-listen:8080 -"));

        // dd to devices
        assert!(contains_dangerous_command("dd if=/dev/random of=/dev/sda"));
        assert!(contains_dangerous_command("dd if=/dev/urandom of=/dev/sdb"));

        // False positives that should NOT be blocked
        assert!(!contains_dangerous_command(
            r#"find . -type f ! -path "./.git/*" ! -path "./.mermaid/*" 2>/dev/null"#
        ));
        assert!(!contains_dangerous_command("ls /tmp 2>/dev/null"));

        // Actual dangerous system dir commands that SHOULD be blocked
        assert!(contains_dangerous_command("rm -rf /etc/passwd"));
        assert!(contains_dangerous_command("rm /usr/bin/something"));

        // Home directory protection
        assert!(contains_dangerous_command("rm -rf ~"));
        assert!(contains_dangerous_command("rm -rf ~/"));
        assert!(contains_dangerous_command("rm -rf ~/Documents"));
        assert!(contains_dangerous_command("rm -rf $HOME"));
        assert!(contains_dangerous_command("rm -rf $HOME/Documents"));
        assert!(contains_dangerous_command("rm -rf /home/user"));
        assert!(contains_dangerous_command("rm /home/user/file.txt"));

        // Home dir false positives that should NOT be blocked
        assert!(!contains_dangerous_command("rm file~")); // backup file suffix
        assert!(!contains_dangerous_command("rm backup~")); // backup file suffix
        assert!(!contains_dangerous_command("ls ~/Documents")); // ls is not rm
    }
}
