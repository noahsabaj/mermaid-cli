//! MCP server add/remove commands.
//!
//! `mermaid add NAME` — resolve, prompt for env vars, validate, save config
//! `mermaid remove NAME` — remove from config

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::io::{self, Write};

use crate::app::{load_config, remove_user_config_key, update_user_config_key};
use mermaid_domain::McpServerConfig;

use super::registry;

/// Add an MCP server by name.
///
/// Resolution chain: built-in registry → convention → npm search.
/// Prompts for required env vars, validates by spawning the server,
/// then saves to config.toml. When `command` is set, skips registry
/// resolution and registers that raw command verbatim instead.
pub async fn add_server(
    name: &str,
    assume_yes: bool,
    command: Option<String>,
    command_args: Vec<String>,
    env_pairs: Vec<String>,
) -> Result<()> {
    if !confirm_overwrite(name)? {
        return Ok(());
    }

    // Raw command server: skip the whole registry resolution chain.
    if let Some(cmd) = command {
        return add_command_server(name, cmd, command_args, env_pairs).await;
    }

    println!("\nResolving '{}'...", name);

    // Resolve the server package via A → B → C. A non-registry result requires
    // explicit confirmation (or --yes) before it is returned (#10).
    let resolved = registry::resolve(name, assume_yes).await?;

    // Prompt for required environment variables
    let mut env = HashMap::new();
    if !resolved.env_vars.is_empty() {
        println!("\nThis server requires:");
        for (var_name, description) in &resolved.env_vars {
            println!("  {}: {}", var_name, description);
        }
        println!();

        for (var_name, _description) in &resolved.env_vars {
            // Check if already set in environment
            if let Ok(existing) = std::env::var(var_name)
                && !existing.is_empty()
            {
                print!(
                    "Enter {} [press Enter to use existing from environment]: ",
                    var_name
                );
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                let input = input.trim();
                if input.is_empty() {
                    // Use environment value — don't store in config
                    // (it will be inherited from the environment at runtime)
                    continue;
                }
                env.insert(var_name.clone(), input.to_string());
            } else {
                print!("Enter {}: ", var_name);
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                let input = input.trim();
                if input.is_empty() {
                    return Err(anyhow!(
                        "Required environment variable '{}' not provided. Setup cancelled.",
                        var_name
                    ));
                }
                env.insert(var_name.clone(), input.to_string());
            }
        }
    }

    // Validate by spawning the server
    println!("\nValidating server (this may take a moment on first run)...");
    let tool_names = registry::validate_server(
        &resolved.command,
        &resolved.package,
        &resolved.extra_args,
        &env,
    )
    .await?;

    if tool_names.is_empty() {
        println!("Warning: Server responded but reported 0 tools.");
    } else {
        println!("Server ready: {} tool(s) available", tool_names.len());
        // Show first few tool names
        let preview: Vec<&str> = tool_names.iter().map(|s| s.as_str()).take(5).collect();
        let suffix = if tool_names.len() > 5 {
            format!(", ... ({} more)", tool_names.len() - 5)
        } else {
            String::new()
        };
        println!("  {}{}", preview.join(", "), suffix);
    }

    // Build config entry — launcher-specific flags (`-y` for npx, none for uvx).
    let mut args: Vec<String> = match resolved.command.as_str() {
        "npx" => vec!["-y".to_string(), resolved.package.clone()],
        _ => vec![resolved.package.clone()],
    };
    args.extend(resolved.extra_args);

    let server_config = McpServerConfig {
        command: resolved.command.clone(),
        args,
        env,
        ..Default::default()
    };

    // Save to config: rewrite only this server's key, so unknown keys and the
    // rest of the user file survive untouched.
    save_server(name, &server_config)?;

    let config_path = crate::app::get_config_dir()?.join("config.toml");
    println!(
        "\nSaved to {}\nThe '{}' tools will be available next time you start mermaid.",
        config_path.display(),
        name
    );

    Ok(())
}

/// If `name` is already configured, prompt before overwriting. Returns
/// `false` (after printing) when the user declines.
fn confirm_overwrite(name: &str) -> Result<bool> {
    let config = load_config()?;
    if config.mcp_servers.contains_key(name) {
        print!("'{}' is already configured. Overwrite? [y/N]: ", name);
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(false);
        }
    }
    Ok(true)
}

/// Register a remote Streamable HTTP MCP server: validate by connecting to
/// `url` (initialize + list tools + end session), then save it to config.
///
/// `header_pairs` are literal `'Name: Value'` headers; `env_header_pairs` are
/// `Header=ENV_VAR` mappings resolved from the environment at request time.
pub async fn add_http_server(
    name: &str,
    url: String,
    header_pairs: Vec<String>,
    env_header_pairs: Vec<String>,
) -> Result<()> {
    if !confirm_overwrite(name)? {
        return Ok(());
    }

    let server_config = McpServerConfig {
        url: Some(url.clone()),
        headers: parse_header_pairs(&header_pairs)?,
        env_headers: parse_env_header_pairs(&env_header_pairs)?,
        ..Default::default()
    };

    println!("\nValidating '{name}' ({url})...");
    match registry::validate_http(&server_config).await {
        Ok(tools) => println!("Server ready: {} tool(s) available", tools.len()),
        Err(e) => return Err(anyhow!("Server '{name}' failed to start: {e}")),
    }

    save_server(name, &server_config)?;

    let config_path = crate::app::get_config_dir()?.join("config.toml");
    println!(
        "\nSaved to {}\nThe '{name}' tools will be available next time you start mermaid.",
        config_path.display()
    );
    Ok(())
}

/// Parse repeatable `'Name: Value'` header pairs into a map.
fn parse_header_pairs(pairs: &[String]) -> Result<HashMap<String, String>> {
    let mut headers = HashMap::new();
    for pair in pairs {
        let (name, value) = pair
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid --header (expected 'Name: Value')"))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("invalid --header (empty header name)"));
        }
        headers.insert(name.to_string(), value.trim().to_string());
    }
    Ok(headers)
}

/// Parse repeatable `Header=ENV_VAR` pairs into a map (header name -> env var
/// name; the env var is read at request time, so no secret lands in config).
fn parse_env_header_pairs(pairs: &[String]) -> Result<HashMap<String, String>> {
    let mut env_headers = HashMap::new();
    for pair in pairs {
        let (header, var) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid --env-header '{pair}' (expected Header=ENV_VAR)"))?;
        let (header, var) = (header.trim(), var.trim());
        if header.is_empty() || var.is_empty() {
            return Err(anyhow!(
                "invalid --env-header '{pair}' (expected Header=ENV_VAR)"
            ));
        }
        env_headers.insert(header.to_string(), var.to_string());
    }
    Ok(env_headers)
}

/// Persist one MCP server entry into the user config file.
fn save_server(name: &str, server_config: &McpServerConfig) -> Result<()> {
    update_user_config_key(
        &["mcp_servers", name],
        toml::Value::try_from(server_config)?,
    )
}

/// Register a raw command MCP server (no registry resolution): validate by
/// spawning `command args...` exactly, then save it verbatim to config.
async fn add_command_server(
    name: &str,
    command: String,
    args: Vec<String>,
    env_pairs: Vec<String>,
) -> Result<()> {
    let env = parse_env_pairs(&env_pairs)?;

    println!("\nValidating '{name}' ({command})...");
    match registry::validate_argv(&command, &args, &env).await {
        Ok(tools) => println!("Server ready: {} tool(s) available", tools.len()),
        Err(e) => return Err(anyhow!("Server '{name}' failed to start: {e}")),
    }

    let server_config = McpServerConfig {
        command,
        args,
        env,
        ..Default::default()
    };
    save_server(name, &server_config)?;

    let config_path = crate::app::get_config_dir()?.join("config.toml");
    println!(
        "\nSaved to {}\nThe '{name}' tools will be available next time you start mermaid.",
        config_path.display()
    );
    Ok(())
}

/// Parse repeatable `KEY=VALUE` env pairs into a map.
fn parse_env_pairs(pairs: &[String]) -> Result<HashMap<String, String>> {
    let mut env = HashMap::new();
    for pair in pairs {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("invalid --env '{pair}' (expected KEY=VALUE)"))?;
        env.insert(k.trim().to_string(), v.to_string());
    }
    Ok(env)
}

/// Remove an MCP server from the config.
pub async fn remove_server(name: &str) -> Result<()> {
    if remove_user_config_key(&["mcp_servers", name])? {
        println!("Removed MCP server '{}' from config.", name);
    } else {
        println!("MCP server '{}' is not configured.", name);
        let config = load_config()?;
        if !config.mcp_servers.is_empty() {
            println!(
                "Configured servers: {}",
                config
                    .mcp_servers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_pairs_splits_on_first_equals() {
        let env = parse_env_pairs(&["A=1".to_string(), "B=x=y".to_string()]).unwrap();
        assert_eq!(env.get("A").map(String::as_str), Some("1"));
        // Only the first '=' splits, so the value may itself contain '='.
        assert_eq!(env.get("B").map(String::as_str), Some("x=y"));
    }

    #[test]
    fn parse_env_pairs_rejects_missing_equals() {
        assert!(parse_env_pairs(&["NOEQUALS".to_string()]).is_err());
    }

    #[test]
    fn parse_header_pairs_splits_on_first_colon() {
        let headers = parse_header_pairs(&[
            "Authorization: Bearer abc:def".to_string(),
            "X-Plain:v".to_string(),
        ])
        .unwrap();
        // Only the first ':' splits — bearer tokens may contain colons.
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer abc:def")
        );
        assert_eq!(headers.get("X-Plain").map(String::as_str), Some("v"));
    }

    #[test]
    fn parse_header_pairs_rejects_malformed_without_echoing_value() {
        let err = parse_header_pairs(&["no-colon-secret".to_string()]).unwrap_err();
        // Header values can be secrets; the error must not quote the input.
        assert!(!err.to_string().contains("no-colon-secret"), "{err}");
        assert!(parse_header_pairs(&[": value-only".to_string()]).is_err());
    }

    #[test]
    fn parse_env_header_pairs_maps_header_to_var_name() {
        let map = parse_env_header_pairs(&["Authorization=MY_TOKEN".to_string()]).unwrap();
        assert_eq!(
            map.get("Authorization").map(String::as_str),
            Some("MY_TOKEN")
        );
        assert!(parse_env_header_pairs(&["NoEquals".to_string()]).is_err());
        assert!(parse_env_header_pairs(&["=VAR".to_string()]).is_err());
        assert!(parse_env_header_pairs(&["Header=".to_string()]).is_err());
    }
}
