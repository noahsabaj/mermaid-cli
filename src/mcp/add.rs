//! MCP server add/remove commands.
//!
//! `mermaid add NAME` — resolve, prompt for env vars, validate, save config
//! `mermaid remove NAME` — remove from config

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::io::{self, Write};

use crate::app::{Config, McpServerConfig, load_config, save_config};

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
    // Check if already configured
    let mut config = load_config()?;
    if config.mcp_servers.contains_key(name) {
        print!("'{}' is already configured. Overwrite? [y/N]: ", name);
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // Raw command server: skip the whole registry resolution chain.
    if let Some(cmd) = command {
        return add_command_server(config, name, cmd, command_args, env_pairs).await;
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

    // Save to config
    config.mcp_servers.insert(name.to_string(), server_config);
    save_config(&config, None)?;

    let config_path = crate::app::get_config_dir()?.join("config.toml");
    println!(
        "\nSaved to {}\nThe '{}' tools will be available next time you start mermaid.",
        config_path.display(),
        name
    );

    Ok(())
}

/// Register a raw command MCP server (no registry resolution): validate by
/// spawning `command args...` exactly, then save it verbatim to config.
async fn add_command_server(
    mut config: Config,
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
    config.mcp_servers.insert(name.to_string(), server_config);
    save_config(&config, None)?;

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
    let mut config = load_config()?;

    if config.mcp_servers.remove(name).is_some() {
        save_config(&config, None)?;
        println!("Removed MCP server '{}' from config.", name);
    } else {
        println!("MCP server '{}' is not configured.", name);
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
}
