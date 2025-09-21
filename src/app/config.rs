use anyhow::{Context, Result};
use directories::ProjectDirs;
use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use crate::constants::{DEFAULT_LITELLM_PROXY_URL, DEFAULT_OLLAMA_PORT};

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Default model configuration
    #[serde(default)]
    pub default_model: ModelSettings,

    /// LiteLLM proxy configuration
    #[serde(default)]
    pub litellm: LiteLLMConfig,

    /// Ollama configuration
    #[serde(default)]
    pub ollama: OllamaConfig,

    /// OpenAI configuration
    #[serde(default)]
    pub openai: OpenAIConfig,

    /// Anthropic configuration
    #[serde(default)]
    pub anthropic: AnthropicConfig,

    /// UI configuration
    #[serde(default)]
    pub ui: UIConfig,

    /// Context loader configuration
    #[serde(default)]
    pub context: ContextConfig,

    /// Operation mode configuration
    #[serde(default)]
    pub mode: ModeConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_model: ModelSettings::default(),
            litellm: LiteLLMConfig::default(),
            ollama: OllamaConfig::default(),
            openai: OpenAIConfig::default(),
            anthropic: AnthropicConfig::default(),
            ui: UIConfig::default(),
            context: ContextConfig::default(),
            mode: ModeConfig::default(),
        }
    }
}

/// Default model settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSettings {
    /// Model provider (ollama, openai, anthropic)
    pub provider: String,
    /// Model name
    pub name: String,
    /// Temperature for generation
    pub temperature: f32,
    /// Maximum tokens to generate
    pub max_tokens: usize,
    /// System prompt
    pub system_prompt: Option<String>,
}

impl ModelSettings {
    /// Default system prompt that teaches models how to use Mermaid's action blocks
    pub fn default_system_prompt() -> String {
        r#"You are Mermaid, an AI pair programmer assistant that can read, write, and execute code.

## IMPORTANT: Action Blocks

You have the ability to perform actions by using special action blocks in your responses. These blocks will be automatically parsed and executed.

### File Operations

To write or create a file, use:
```
[FILE_WRITE: path/to/file.rs]
fn main() {
    println!("Hello, world!");
}
[/FILE_WRITE]
```

To read a file, use:
```
[FILE_READ: path/to/file.rs]
[/FILE_READ]
```

### Shell Commands

To execute shell commands, use:
```
[COMMAND: cargo test]
[/COMMAND]
```

Or with a specific working directory:
```
[COMMAND: cargo build --release dir=/path/to/project]
[/COMMAND]
```

### Git Operations

To see git status:
```
[GIT_STATUS]
```

To see git diff:
```
[GIT_DIFF]
```

## Guidelines

1. When asked to create or modify files, ALWAYS use the [FILE_WRITE:] action block
2. When asked to run commands, ALWAYS use the [COMMAND:] action block
3. Be precise with file paths - use the exact paths shown in the project context
4. After writing files, consider running relevant tests or build commands
5. Explain what you're doing before and after each action block

Remember: You're not just showing code examples - you can actually create, modify, and execute files!"#.to_string()
    }
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            provider: String::from("ollama"),
            name: String::from("tinyllama"),
            temperature: 0.7,
            max_tokens: 4096,
            system_prompt: Some(Self::default_system_prompt()),
        }
    }
}

/// LiteLLM proxy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiteLLMConfig {
    /// Proxy server URL
    pub proxy_url: String,
    /// Master key for authentication
    pub master_key: Option<String>,
}

impl Default for LiteLLMConfig {
    fn default() -> Self {
        Self {
            proxy_url: DEFAULT_LITELLM_PROXY_URL.to_string(),
            master_key: None,
        }
    }
}

/// Ollama configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaConfig {
    /// Ollama server host
    pub host: String,
    /// Ollama server port
    pub port: u16,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            host: String::from("localhost"),
            port: DEFAULT_OLLAMA_PORT,
        }
    }
}

/// OpenAI configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIConfig {
    /// Environment variable containing API key
    pub api_key_env: String,
    /// Organization ID (optional)
    pub organization: Option<String>,
}

impl Default for OpenAIConfig {
    fn default() -> Self {
        Self {
            api_key_env: String::from("OPENAI_API_KEY"),
            organization: None,
        }
    }
}

/// Anthropic configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicConfig {
    /// Environment variable containing API key
    pub api_key_env: String,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key_env: String::from("ANTHROPIC_API_KEY"),
        }
    }
}

/// UI configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UIConfig {
    /// Color theme
    pub theme: String,
    /// Syntax highlighting theme
    pub syntax_theme: String,
    /// Show line numbers in code blocks
    pub show_line_numbers: bool,
    /// Show file sidebar by default
    pub show_sidebar: bool,
}

impl Default for UIConfig {
    fn default() -> Self {
        Self {
            theme: String::from("dark"),
            syntax_theme: String::from("monokai"),
            show_line_numbers: true,
            show_sidebar: true,
        }
    }
}

/// Context loader configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Maximum file size to load (in bytes)
    pub max_file_size: usize,
    /// Maximum number of files to include
    pub max_files: usize,
    /// Maximum total context size in tokens
    pub max_context_tokens: usize,
    /// Auto-include these file patterns
    pub include_patterns: Vec<String>,
    /// Always exclude these patterns
    pub exclude_patterns: Vec<String>,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_file_size: 1024 * 1024, // 1MB
            max_files: 100,
            max_context_tokens: 50000,
            include_patterns: vec![],
            exclude_patterns: vec![String::from("*.log"), String::from("*.tmp")],
        }
    }
}

/// Operation mode configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeConfig {
    /// Default operation mode (normal, accept_edits, plan_mode, bypass_all)
    pub default_mode: String,
    /// Remember mode between sessions
    pub remember_mode: bool,
    /// Auto-commit in AcceptEdits mode
    pub auto_commit_on_accept: bool,
    /// Require double confirmation for destructive operations in BypassAll mode
    pub require_destructive_confirmation: bool,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            default_mode: String::from("normal"),
            remember_mode: false,
            auto_commit_on_accept: false,
            require_destructive_confirmation: true,
        }
    }
}

/// Load configuration from multiple sources
pub fn load_config() -> Result<Config> {
    // Get config directories
    let config_dir = get_config_dir()?;
    let global_config = config_dir.join("config.toml");
    let local_config = PathBuf::from(".mermaid/config.toml");

    // Build figment configuration
    let mut figment = Figment::from(Serialized::defaults(Config::default()));

    // Add global config if it exists
    if global_config.exists() {
        figment = figment.merge(Toml::file(&global_config));
    }

    // Add local config if it exists
    if local_config.exists() {
        figment = figment.merge(Toml::file(&local_config));
    }

    // Add environment variables (MERMAID_ prefix)
    figment = figment.merge(Env::prefixed("MERMAID_"));

    // Extract and return config
    figment
        .extract()
        .context("Failed to load configuration. Check that config files are valid TOML format.")
}

/// Get the configuration directory
pub fn get_config_dir() -> Result<PathBuf> {
    if let Some(proj_dirs) = ProjectDirs::from("", "", "mermaid") {
        let config_dir = proj_dirs.config_dir();
        std::fs::create_dir_all(config_dir)?;
        Ok(config_dir.to_path_buf())
    } else {
        // Fallback to home directory
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .context("Could not determine home directory")?;
        let config_dir = PathBuf::from(home).join(".config").join("mermaid");
        std::fs::create_dir_all(&config_dir)?;
        Ok(config_dir)
    }
}

/// Save configuration to file
pub fn save_config(config: &Config, path: Option<PathBuf>) -> Result<()> {
    let path = if let Some(p) = path {
        p
    } else {
        get_config_dir()?.join("config.toml")
    };

    let toml_string = toml::to_string_pretty(config)?;
    std::fs::write(&path, toml_string)
        .with_context(|| format!("Failed to write config to {}", path.display()))?;

    Ok(())
}

/// Create a default configuration file if it doesn't exist
pub fn init_config() -> Result<()> {
    let config_dir = get_config_dir()?;
    let config_file = config_dir.join("config.toml");

    if !config_file.exists() {
        let default_config = Config::default();
        save_config(&default_config, Some(config_file.clone()))?;
        println!("Created default configuration at: {}", config_file.display());
    }

    // Create example local config
    let local_example = PathBuf::from(".mermaid/config.toml.example");
    if !local_example.exists() {
        if let Some(parent) = local_example.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let example_config = r#"# Mermaid Project Configuration
# This file overrides global settings for this project

[default_model]
provider = "ollama"
name = "tinyllama"
temperature = 0.7
max_tokens = 4096

[context]
max_files = 150
max_context_tokens = 75000
include_patterns = ["src/**/*.rs", "Cargo.toml"]
"#;
        std::fs::write(&local_example, example_config)?;
        println!("Created example configuration at: {}", local_example.display());
    }

    Ok(())
}