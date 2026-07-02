#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
#[cfg(any(target_os = "linux", test))]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

use super::DaemonCommand;

// These describe the Linux systemd user service. Their only consumers are the
// `cfg(target_os = "linux")` / `cfg(test)` helpers below, so on Windows/macOS
// non-test builds they are retained but unused — allow that without disturbing
// the Linux build, where they are genuinely live.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const SERVICE_NAME: &str = "mermaidd.service";
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const DAEMON_BIN_ENV: &str = "MERMAID_DAEMON_BIN";

#[cfg(target_os = "linux")]
pub fn handle_daemon_command(command: &DaemonCommand) -> Result<()> {
    match command {
        DaemonCommand::Install { start, force } => install_service(*start, *force),
        DaemonCommand::Uninstall => uninstall_service(),
        DaemonCommand::Start => systemctl_service("start"),
        DaemonCommand::Stop => systemctl_service("stop"),
        DaemonCommand::Restart => systemctl_service("restart"),
        DaemonCommand::Status => {
            let ok = run_inherited_allow_failure("systemctl", &systemctl_status_args())?;
            if !ok {
                eprintln!(
                    "systemctl status returned non-zero; the background service may be inactive."
                );
            }
            Ok(())
        },
        DaemonCommand::Logs { follow, lines } => {
            run_inherited("journalctl", &journalctl_args(*follow, *lines))
        },
        DaemonCommand::PrintUnit => {
            let daemon_path = resolve_mermaidd_path();
            print!("{}", render_unit(&daemon_path));
            Ok(())
        },
    }
}

#[cfg(not(target_os = "linux"))]
pub fn handle_daemon_command(_command: &DaemonCommand) -> Result<()> {
    anyhow::bail!("`mermaid daemon` service management is currently supported on Linux only")
}

#[cfg(target_os = "linux")]
fn install_service(start: bool, force: bool) -> Result<()> {
    let daemon_path = resolve_mermaidd_path();
    if !daemon_path.exists() {
        eprintln!(
            "warning: resolved background-service binary does not exist yet: {}",
            daemon_path.display()
        );
        eprintln!(
            "         set {}=/absolute/path/to/mermaidd or install mermaidd before starting the service.",
            DAEMON_BIN_ENV
        );
    }

    let unit_path = service_path()?;
    if unit_path.exists() && !force {
        anyhow::bail!(
            "service unit already exists at {}; rerun with --force to overwrite it",
            unit_path.display()
        );
    }
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&unit_path, render_unit(&daemon_path))
        .with_context(|| format!("writing {}", unit_path.display()))?;

    run_inherited("systemctl", &systemctl_daemon_reload_args())?;
    println!("Installed {}", unit_path.display());
    if start {
        run_inherited("systemctl", &systemctl_enable_now_args())?;
        println!("Enabled and started {}", SERVICE_NAME);
    } else {
        println!("Start it with: mermaid daemon start");
        println!(
            "Enable at login with: systemctl --user enable {}",
            SERVICE_NAME
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_service() -> Result<()> {
    let ok = run_inherited_allow_failure("systemctl", &systemctl_disable_now_args())?;
    if !ok {
        eprintln!("warning: systemctl disable --now returned non-zero; continuing uninstall.");
    }

    let unit_path = service_path()?;
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("removing {}", unit_path.display()))?;
        println!("Removed {}", unit_path.display());
    } else {
        println!("No user service unit found at {}", unit_path.display());
    }
    run_inherited("systemctl", &systemctl_daemon_reload_args())?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemctl_service(action: &str) -> Result<()> {
    run_inherited("systemctl", &systemctl_service_args(action))
}

#[cfg(any(target_os = "linux", test))]
pub fn render_unit(exec_start: &Path) -> String {
    // No `Environment=MERMAID_DAEMON_TCP_ADDR=...`: the daemon binds TCP only
    // when MERMAID_DAEMON_ENABLE_TCP=1, so setting the address alone did nothing
    // and misleadingly implied the service listens on TCP by default. Keep this
    // byte-for-byte in sync with `packaging/systemd/mermaidd.service` (a test
    // enforces it).
    format!(
        "[Unit]\n\
         Description=Mermaid local AI background service\n\
         Documentation=https://github.com/noahsabaj/mermaid-cli#readme\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        quote_systemd_exec_path(exec_start),
    )
}

#[cfg(target_os = "linux")]
fn service_path() -> Result<PathBuf> {
    Ok(systemd_user_dir()?.join(SERVICE_NAME))
}

#[cfg(target_os = "linux")]
fn systemd_user_dir() -> Result<PathBuf> {
    systemd_user_dir_from(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

#[cfg(any(target_os = "linux", test))]
fn systemd_user_dir_from(config_home: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(config_home) = config_home.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(config_home.join("systemd/user"));
    }
    let Some(home) = home.filter(|path| !path.as_os_str().is_empty()) else {
        anyhow::bail!("could not resolve HOME or XDG_CONFIG_HOME for systemd user unit path");
    };
    Ok(home.join(".config/systemd/user"))
}

#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn resolve_mermaidd_path() -> PathBuf {
    if let Some(path) = std::env::var_os(DAEMON_BIN_ENV).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        let sibling = parent.join(daemon_binary_name());
        if sibling.exists() {
            return sibling;
        }
    }

    if let Ok(path) = which::which(daemon_binary_name()) {
        return path;
    }

    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home)
            .join(".cargo/bin")
            .join(daemon_binary_name());
    }

    PathBuf::from(daemon_binary_name())
}

#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn daemon_binary_name() -> &'static str {
    #[cfg(windows)]
    {
        "mermaidd.exe"
    }
    #[cfg(not(windows))]
    {
        "mermaidd"
    }
}

#[cfg(any(target_os = "linux", test))]
fn quote_systemd_exec_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if raw
        .chars()
        .all(|ch| !ch.is_whitespace() && ch != '"' && ch != '\\')
    {
        return raw.into_owned();
    }

    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('"');
    for ch in raw.chars() {
        if ch == '"' || ch == '\\' {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

#[cfg(any(target_os = "linux", test))]
fn systemctl_service_args(action: &str) -> Vec<String> {
    vec!["--user".into(), action.into(), SERVICE_NAME.into()]
}

#[cfg(any(target_os = "linux", test))]
fn systemctl_status_args() -> Vec<String> {
    vec![
        "--user".into(),
        "status".into(),
        SERVICE_NAME.into(),
        "--no-pager".into(),
    ]
}

#[cfg(target_os = "linux")]
fn systemctl_daemon_reload_args() -> Vec<String> {
    vec!["--user".into(), "daemon-reload".into()]
}

#[cfg(target_os = "linux")]
fn systemctl_enable_now_args() -> Vec<String> {
    vec![
        "--user".into(),
        "enable".into(),
        "--now".into(),
        SERVICE_NAME.into(),
    ]
}

#[cfg(target_os = "linux")]
fn systemctl_disable_now_args() -> Vec<String> {
    vec![
        "--user".into(),
        "disable".into(),
        "--now".into(),
        SERVICE_NAME.into(),
    ]
}

#[cfg(any(target_os = "linux", test))]
fn journalctl_args(follow: bool, lines: usize) -> Vec<String> {
    let mut args = vec![
        "--user-unit".into(),
        SERVICE_NAME.into(),
        "-n".into(),
        lines.to_string(),
        "--no-pager".into(),
    ];
    if follow {
        args.push("-f".into());
    }
    args
}

#[cfg(target_os = "linux")]
fn run_inherited(program: &str, args: &[String]) -> Result<()> {
    let status = command_with_inherited_stdio(program, args)
        .status()
        .with_context(|| format!("running {}", format_command(program, args)))?;
    if !status.success() {
        anyhow::bail!(
            "{} exited with status {}",
            format_command(program, args),
            status
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_inherited_allow_failure(program: &str, args: &[String]) -> Result<bool> {
    let status = command_with_inherited_stdio(program, args)
        .status()
        .with_context(|| format!("running {}", format_command(program, args)))?;
    Ok(status.success())
}

#[cfg(target_os = "linux")]
fn command_with_inherited_stdio(program: &str, args: &[String]) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}

#[cfg(target_os = "linux")]
fn format_command(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_unit_has_exec_path_and_no_dead_tcp_env() {
        let unit = render_unit(Path::new("/usr/bin/mermaidd"));
        assert!(unit.contains("ExecStart=/usr/bin/mermaidd"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        // The TCP-address env var did nothing without MERMAID_DAEMON_ENABLE_TCP,
        // so it must not appear (it implied the daemon listens on TCP).
        assert!(!unit.contains("MERMAID_DAEMON_TCP_ADDR"));
    }

    #[test]
    fn packaged_unit_matches_render_unit() {
        // The static `packaging/systemd/mermaidd.service` and the CLI-generated
        // unit are two sources of truth that had already drifted (description +
        // the dead env line). Pin them byte-for-byte so they can't diverge again.
        let packaged = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/systemd/mermaidd.service"
        ));
        assert_eq!(packaged, render_unit(Path::new("/usr/bin/mermaidd")));
    }

    #[test]
    fn render_unit_quotes_paths_with_spaces() {
        let unit = render_unit(Path::new("/home/user/bin dir/mermaid\"d"));
        assert!(unit.contains("ExecStart=\"/home/user/bin dir/mermaid\\\"d\""));
    }

    #[test]
    fn systemd_user_dir_prefers_xdg_config_home() {
        let path = systemd_user_dir_from(
            Some(PathBuf::from("/tmp/config")),
            Some(PathBuf::from("/home/tester")),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("/tmp/config/systemd/user"));
    }

    #[test]
    fn systemd_user_dir_falls_back_to_home_config() {
        let path = systemd_user_dir_from(None, Some(PathBuf::from("/home/tester"))).unwrap();
        assert_eq!(path, PathBuf::from("/home/tester/.config/systemd/user"));
    }

    #[test]
    fn service_command_args_target_mermaidd_unit() {
        assert_eq!(
            systemctl_service_args("restart"),
            vec!["--user", "restart", "mermaidd.service"]
        );
        assert_eq!(
            systemctl_status_args(),
            vec!["--user", "status", "mermaidd.service", "--no-pager"]
        );
    }

    #[test]
    fn journalctl_args_support_follow_and_line_count() {
        assert_eq!(
            journalctl_args(true, 25),
            vec![
                "--user-unit",
                "mermaidd.service",
                "-n",
                "25",
                "--no-pager",
                "-f"
            ]
        );
    }
}
