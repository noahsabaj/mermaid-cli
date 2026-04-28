use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const SERVICE_NAME: &str = "mermaidd.service";
const DEFAULT_TCP_ADDR: &str = "127.0.0.1:39871";
const DAEMON_BIN_ENV: &str = "MERMAID_DAEMON_BIN";

fn desktop_token_path() -> Result<PathBuf, String> {
    let dir = mermaid_cli::runtime::data_dir()
        .map_err(|err| err.to_string())?
        .join("desktop");
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    Ok(dir.join("daemon.token"))
}

fn load_or_create_desktop_token() -> Result<String, String> {
    let path = desktop_token_path()?;
    if let Ok(value) = std::fs::read_to_string(&path) {
        let token = value.trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
    }

    let (token, _) = mermaid_cli::runtime::generate_pairing_token().map_err(|err| err.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    use std::io::Write;
    let mut file = options.open(&path).map_err(|err| err.to_string())?;
    file.write_all(token.as_bytes())
        .map_err(|err| err.to_string())?;
    Ok(token)
}

fn request_daemon(body: Value) -> Result<Value, String> {
    mermaid_cli::runtime::request_daemon_json(body).map_err(|err| err.to_string())
}

fn request_daemon_authed(mut body: Value) -> Result<Value, String> {
    let token = load_or_create_desktop_token()?;
    body["auth"] = json!({ "token": token });
    request_daemon(body)
}

fn bootstrap_desktop_token() -> Result<Value, String> {
    let token = load_or_create_desktop_token()?;
    request_daemon(json!({
        "command": "desktop_bootstrap",
        "token_hash": mermaid_cli::runtime::hash_pairing_token(&token),
        "label": "Mermaid Desktop",
    }))
}

fn desktop_runtime_client() -> Result<mermaid_cli::runtime::RuntimeClient, String> {
    bootstrap_desktop_token()?;
    Ok(mermaid_cli::runtime::RuntimeClient::daemon())
}

fn desktop_runtime_client_authed() -> Result<mermaid_cli::runtime::RuntimeClient, String> {
    bootstrap_desktop_token()?;
    Ok(mermaid_cli::runtime::RuntimeClient::daemon_with_token(
        load_or_create_desktop_token()?,
    ))
}

fn to_value<T: Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|err| err.to_string())
}

#[tauri::command]
fn daemon_health() -> Result<Value, String> {
    to_value(
        mermaid_cli::runtime::RuntimeClient::daemon()
            .health()
            .map_err(|err| err.to_string())?
            .value,
    )
}

#[tauri::command]
fn daemon_service(action: String) -> Result<Value, String> {
    if matches!(action.as_str(), "start" | "restart") {
        ensure_daemon_service_installed()?;
    }
    let args: &[&str] = match action.as_str() {
        "start" => &["--user", "start", SERVICE_NAME],
        "stop" => &["--user", "stop", SERVICE_NAME],
        "restart" => &["--user", "restart", SERVICE_NAME],
        "status" => &["--user", "status", SERVICE_NAME, "--no-pager"],
        other => return Err(format!("unsupported daemon service action: {other}")),
    };
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|err| err.to_string())?;
    Ok(json!({
        "ok": output.status.success(),
        "status": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
        "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
    }))
}

fn ensure_daemon_service_installed() -> Result<(), String> {
    let unit_path = daemon_service_path()?;
    if unit_path.exists() {
        return Ok(());
    }

    let daemon_path = resolve_mermaidd_path()?;
    if !daemon_path.exists() {
        return Err(format!(
            "Could not install {SERVICE_NAME}: resolved daemon binary does not exist at {}. Build it with `cargo build --bin mermaidd` or set {DAEMON_BIN_ENV}.",
            daemon_path.display()
        ));
    }
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    std::fs::write(&unit_path, render_daemon_unit(&daemon_path))
        .map_err(|err| format!("failed to write {}: {err}", unit_path.display()))?;

    let reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output()
        .map_err(|err| err.to_string())?;
    if !reload.status.success() {
        return Err(format!(
            "Installed {}, but `systemctl --user daemon-reload` failed:\n{}",
            unit_path.display(),
            String::from_utf8_lossy(&reload.stderr)
        ));
    }
    Ok(())
}

fn daemon_service_path() -> Result<PathBuf, String> {
    Ok(systemd_user_dir()?.join(SERVICE_NAME))
}

fn systemd_user_dir() -> Result<PathBuf, String> {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(config_home).join("systemd/user"));
    }
    let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) else {
        return Err("could not resolve HOME or XDG_CONFIG_HOME for systemd user unit path".to_string());
    };
    Ok(PathBuf::from(home).join(".config/systemd/user"))
}

fn resolve_mermaidd_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(DAEMON_BIN_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let sibling = parent.join("mermaidd");
            if sibling.exists() {
                return Ok(sibling);
            }
        }
        let mut candidates = Vec::new();
        for ancestor in current_exe.ancestors() {
            #[cfg(debug_assertions)]
            {
                candidates.push(ancestor.join("target/debug/mermaidd"));
                candidates.push(ancestor.join("target/release/mermaidd"));
            }
            #[cfg(not(debug_assertions))]
            {
                candidates.push(ancestor.join("target/release/mermaidd"));
                candidates.push(ancestor.join("target/debug/mermaidd"));
            }
        }
        if let Some(path) = candidates.into_iter().find(|path| path.exists()) {
            return Ok(path);
        }
    }

    if let Some(path) = find_on_path("mermaidd") {
        return Ok(path);
    }

    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join(".cargo/bin/mermaidd"));
    }

    Ok(PathBuf::from("mermaidd"))
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(binary))
        .find(|path| path.exists())
}

fn render_daemon_unit(exec_start: &std::path::Path) -> String {
    format!(
        "[Unit]\n\
         Description=Mermaid local AI runtime daemon\n\
         Documentation=https://github.com/noahsabaj/mermaid-cli#readme\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         Environment=MERMAID_DAEMON_TCP_ADDR={}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        quote_systemd_exec_path(exec_start),
        DEFAULT_TCP_ADDR
    )
}

fn quote_systemd_exec_path(path: &std::path::Path) -> String {
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

#[tauri::command]
fn desktop_dashboard() -> Result<Value, String> {
    to_value(desktop_runtime_client()?.dashboard().map_err(|err| err.to_string())?.value)
}

#[tauri::command]
fn desktop_diagnostics() -> Result<Value, String> {
    to_value(desktop_runtime_client()?.diagnostics().map_err(|err| err.to_string())?.value)
}

#[tauri::command]
fn desktop_hygiene_preview() -> Result<Value, String> {
    to_value(desktop_runtime_client()?.hygiene_preview().map_err(|err| err.to_string())?.value)
}

#[tauri::command]
fn desktop_hygiene_archive() -> Result<Value, String> {
    to_value(
        desktop_runtime_client_authed()?
            .hygiene_archive()
            .map_err(|err| err.to_string())?
            .value,
    )
}

#[tauri::command]
fn desktop_task_detail(id: String) -> Result<Value, String> {
    to_value(desktop_runtime_client()?.task_detail(&id).map_err(|err| err.to_string())?.value)
}

#[tauri::command]
fn desktop_approval_detail(id: String) -> Result<Value, String> {
    to_value(
        desktop_runtime_client()?
            .approval_detail(&id)
            .map_err(|err| err.to_string())?
            .value,
    )
}

#[tauri::command]
fn desktop_checkpoint_detail(id: String) -> Result<Value, String> {
    to_value(
        desktop_runtime_client()?
            .checkpoint_detail(&id)
            .map_err(|err| err.to_string())?
            .value,
    )
}

#[tauri::command]
fn run_task(prompt: String, project_path: Option<String>, model_id: Option<String>) -> Result<Value, String> {
    request_daemon_authed(json!({
        "command": "run",
        "prompt": prompt,
        "project_path": project_path.unwrap_or_default(),
        "model_id": model_id.unwrap_or_default(),
    }))
}

#[tauri::command]
fn approve(id: String) -> Result<Value, String> {
    to_value(desktop_runtime_client_authed()?.approve(&id).map_err(|err| err.to_string())?)
}

#[tauri::command]
fn deny(id: String) -> Result<Value, String> {
    to_value(desktop_runtime_client_authed()?.deny(&id).map_err(|err| err.to_string())?)
}

#[tauri::command]
fn restore_checkpoint(id: String) -> Result<Value, String> {
    to_value(
        desktop_runtime_client_authed()?
            .restore_checkpoint(&id)
            .map_err(|err| err.to_string())?,
    )
}

#[tauri::command]
fn set_safety_mode(mode: String) -> Result<Value, String> {
    desktop_runtime_client_authed()?
        .set_safety_mode(&mode)
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn stop_process(id: String) -> Result<Value, String> {
    let response = desktop_runtime_client_authed()?
        .stop_process(&id)
        .map_err(|err| err.to_string())?;
    Ok(json!({"ok": response.ok, "item": response.item.clone(), "process": response.item}))
}

#[tauri::command]
fn restart_process(id: String) -> Result<Value, String> {
    let response = desktop_runtime_client_authed()?
        .restart_process(&id)
        .map_err(|err| err.to_string())?;
    Ok(json!({"ok": response.ok, "item": response.item.clone(), "process": response.item}))
}

#[tauri::command]
fn open_process(id: String) -> Result<Value, String> {
    to_value(
        desktop_runtime_client_authed()?
            .open_process(&id)
            .map_err(|err| err.to_string())?,
    )
}

#[tauri::command]
fn process_logs(id: String, tail_bytes: Option<u64>) -> Result<Value, String> {
    to_value(
        desktop_runtime_client()?
            .process_log(&id, tail_bytes)
            .map_err(|err| err.to_string())?,
    )
}

#[tauri::command]
fn memory_edit(id: String, value: String) -> Result<Value, String> {
    let response = desktop_runtime_client_authed()?
        .edit_memory(&id, &value, "desktop")
        .map_err(|err| err.to_string())?;
    Ok(json!({"ok": true, "item": response.value.clone(), "memory": response.value}))
}

#[tauri::command]
fn forget(id: String) -> Result<Value, String> {
    desktop_runtime_client_authed()?
        .forget_memory(&id)
        .map_err(|err| err.to_string())?;
    Ok(json!({"ok": true}))
}

#[tauri::command]
fn plugin_preview(path: String) -> Result<Value, String> {
    request_daemon(json!({ "command": "plugin_preview", "path": path }))
}

#[tauri::command]
fn plugin_install(path: String) -> Result<Value, String> {
    request_daemon_authed(json!({ "command": "plugin_install", "path": path }))
}

#[tauri::command]
fn set_plugin_enabled(id: String, enabled: bool) -> Result<Value, String> {
    desktop_runtime_client_authed()?
        .set_plugin_enabled(&id, enabled)
        .map_err(|err| err.to_string())?;
    Ok(json!({"ok": true}))
}

#[tauri::command]
fn create_pairing(label: Option<String>) -> Result<Value, String> {
    request_daemon_authed(json!({
        "command": "pair",
        "label": label.unwrap_or_else(|| "Remote client".to_string()),
    }))
}

#[cfg(target_os = "linux")]
fn install_linux_webkit_workarounds() {
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // WebKitGTK's GBM/DMABUF renderer can abort on some Mint/Ubuntu GPU
        // stacks before the window is created. Setting the documented fallback
        // keeps development and packaged builds on the stable software path.
        unsafe {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn install_linux_webkit_workarounds() {}

fn main() {
    install_linux_webkit_workarounds();

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            daemon_health,
            daemon_service,
            desktop_dashboard,
            desktop_diagnostics,
            desktop_hygiene_preview,
            desktop_hygiene_archive,
            desktop_task_detail,
            desktop_approval_detail,
            desktop_checkpoint_detail,
            run_task,
            approve,
            deny,
            restore_checkpoint,
            set_safety_mode,
            stop_process,
            restart_process,
            open_process,
            process_logs,
            memory_edit,
            forget,
            plugin_preview,
            plugin_install,
            set_plugin_enabled,
            create_pairing,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Mermaid desktop");
}
