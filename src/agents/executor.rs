use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

use crate::agents::ActionResult;

/// Execute a shell command and capture output
pub async fn execute_command(
    command: &str,
    working_dir: Option<&str>,
) -> Result<ActionResult> {
    // Security checks
    if contains_dangerous_command(command) {
        return Ok(ActionResult::Error {
            error: format!("Dangerous command blocked: {}", command),
        });
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

    // Execute with timeout
    let timeout_duration = Duration::from_secs(30);

    match timeout(timeout_duration, run_command(cmd)).await {
        Ok(Ok(output)) => Ok(ActionResult::Success { output }),
        Ok(Err(e)) => Ok(ActionResult::Error {
            error: format!("Command failed: {}", e),
        }),
        Err(_) => Ok(ActionResult::Error {
            error: format!("Command timed out after {} seconds", timeout_duration.as_secs()),
        }),
    }
}

/// Run the command and stream output
async fn run_command(mut cmd: Command) -> Result<String> {
    let mut child = cmd.spawn()
        .context("Failed to execute command. Is the shell available?")?;

    let stdout = child.stdout.take()
        .context("Command process stdout stream not available. This is likely a bug.")?;
    let stderr = child.stderr.take()
        .context("Command process stderr stream not available. This is likely a bug.")?;

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut output = String::new();
    let mut errors = String::new();

    // Read stdout
    while let Some(line) = stdout_reader
        .next_line()
        .await
        .context("Error reading command output. The process may have terminated unexpectedly.")?
    {
        output.push_str(&line);
        output.push('\n');
    }

    // Read stderr
    while let Some(line) = stderr_reader
        .next_line()
        .await
        .context("Error reading command error output. The process may have terminated unexpectedly.")?
    {
        errors.push_str(&line);
        errors.push('\n');
    }

    let status = child.wait().await
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
fn contains_dangerous_command(command: &str) -> bool {
    let dangerous_patterns = [
        "rm -rf /",
        "rm -rf /*",
        "dd if=/dev/zero of=/",
        "mkfs.",
        "format c:",
        "> /dev/sda",
        "chmod -R 777 /",
        "chmod -R 000 /",
        ":(){ :|:& };:", // Fork bomb
        "curl | bash",
        "wget | sh",
        "nc -l", // Netcat listener
    ];

    let lower_command = command.to_lowercase();

    for pattern in &dangerous_patterns {
        if lower_command.contains(pattern) {
            return true;
        }
    }

    // Check for attempts to modify system directories
    let system_dirs = [
        "/etc",
        "/usr",
        "/boot",
        "/proc",
        "/sys",
        "/dev",
        "C:\\Windows",
        "C:\\Program Files",
    ];

    for dir in &system_dirs {
        if command.contains(dir) && (command.contains("rm") || command.contains("del")) {
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
        let result = execute_command("echo 'Hello, Mermaid!'", None)
            .await
            .unwrap();

        match result {
            ActionResult::Success { output } => {
                assert!(output.contains("Hello, Mermaid!"));
            }
            _ => panic!("Expected success"),
        }
    }

    #[tokio::test]
    async fn test_dangerous_command_blocked() {
        let result = execute_command("rm -rf /", None).await.unwrap();

        match result {
            ActionResult::Error { error } => {
                assert!(error.contains("Dangerous command blocked"));
            }
            _ => panic!("Expected error"),
        }
    }

    #[test]
    fn test_dangerous_command_detection() {
        assert!(contains_dangerous_command("rm -rf /"));
        assert!(contains_dangerous_command("format c:"));
        assert!(contains_dangerous_command(":(){ :|:& };:"));
        assert!(!contains_dangerous_command("ls -la"));
        assert!(!contains_dangerous_command("cargo build"));
    }
}