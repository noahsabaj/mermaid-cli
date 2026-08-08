use anyhow::Result;

/// Whether the Ollama Cloud key is configured — i.e. the `OLLAMA_API_KEY`
/// environment variable is set to a non-empty value. The key is never read from
/// or written to `config.toml` (#88).
pub fn is_cloud_configured() -> bool {
    get_cloud_api_key().is_some()
}

/// Interactive Ollama Cloud setup. Mermaid resolves the cloud key from the
/// `OLLAMA_API_KEY` environment variable only — it is never written to
/// `config.toml` (#88), matching how every other provider key is handled. So
/// this explains how to set the variable rather than prompting for and saving a
/// secret to disk. Returns whether the key is currently configured.
pub fn setup_cloud_interactive() -> Result<bool> {
    println!("\n=== Ollama Cloud Setup ===\n");
    println!("Ollama Cloud runs large models on datacenter-grade hardware.");
    println!("Cloud models use the :cloud suffix (e.g., kimi-k2-thinking:cloud).\n");
    println!("Mermaid reads the key from the OLLAMA_API_KEY environment variable");
    println!("and never writes it to disk. To configure it:\n");
    println!("  1. Get an API key at https://ollama.com/cloud");
    println!("  2. Export it in your shell:\n");
    println!("       export OLLAMA_API_KEY=<your-key>\n");
    println!("  3. To persist it, add that line to your shell rc (e.g. ~/.bashrc");
    println!("     or ~/.zshrc), then start a new shell.\n");

    if is_cloud_configured() {
        println!("OLLAMA_API_KEY is set — cloud models are available.\n");
    } else {
        println!("OLLAMA_API_KEY is not set yet; cloud models stay unavailable");
        println!("until you set it and re-run mermaid.\n");
    }
    Ok(is_cloud_configured())
}

/// Get the Ollama Cloud API key: the `OLLAMA_API_KEY` environment variable,
/// falling back to the OS keyring (`mermaid login ollama`).
///
/// Never persisted to config files (#88); the keyring is the only at-rest
/// store and it is the OS's. Empty values are treated as unset.
pub fn get_cloud_api_key() -> Option<String> {
    mermaid_model::utils::resolve_provider_key("ollama", "OLLAMA_API_KEY", None)
}

/// Check if a model name requires cloud access.
pub fn is_cloud_model(model_name: &str) -> bool {
    model_name.ends_with(":cloud")
}

/// Prompt the user to configure cloud access if a cloud model is requested
/// without `OLLAMA_API_KEY` set.
pub fn prompt_cloud_setup_if_needed(model_name: &str) -> Result<bool> {
    if !is_cloud_model(model_name) {
        return Ok(true); // Not a cloud model, proceed.
    }

    if is_cloud_configured() {
        return Ok(true); // Already configured, proceed.
    }

    println!("\nCloud model requested but OLLAMA_API_KEY is not set.");
    println!("   Model: {model_name}\n");

    setup_cloud_interactive()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_cloud_model_detects_suffix() {
        assert!(is_cloud_model("kimi-k2-thinking:cloud"));
        assert!(!is_cloud_model("qwen3-coder:30b"));
        // Only the exact ":cloud" suffix counts (a "-cloud" tag does not).
        assert!(!is_cloud_model("qwen3-coder:480b-cloud"));
    }

    #[test]
    fn get_cloud_api_key_resolves_from_env_only() {
        // #88: the key comes from the environment, never from config on disk.
        temp_env::with_vars([("OLLAMA_API_KEY", Some("sk-test"))], || {
            assert_eq!(get_cloud_api_key().as_deref(), Some("sk-test"));
            assert!(is_cloud_configured());
        });
        temp_env::with_vars([("OLLAMA_API_KEY", None::<&str>)], || {
            assert_eq!(get_cloud_api_key(), None);
            assert!(!is_cloud_configured());
        });
        // Empty is treated as unset.
        temp_env::with_vars([("OLLAMA_API_KEY", Some(""))], || {
            assert_eq!(get_cloud_api_key(), None);
        });
    }
}
