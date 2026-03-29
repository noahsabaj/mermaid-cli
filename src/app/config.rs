use crate::constants::{DEFAULT_MAX_TOKENS, DEFAULT_OLLAMA_PORT, DEFAULT_TEMPERATURE};
use crate::prompts;
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Main configuration structure
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Last used model (persisted between sessions)
    #[serde(default)]
    pub last_used_model: Option<String>,

    /// Default model configuration
    #[serde(default)]
    pub default_model: ModelSettings,

    /// Ollama configuration
    #[serde(default)]
    pub ollama: OllamaConfig,

    /// OpenAI configuration (for direct OpenAI API access)
    #[serde(default)]
    pub openai: OpenAIConfig,

    /// Anthropic configuration (for direct Anthropic API access)
    #[serde(default)]
    pub anthropic: AnthropicConfig,

    /// UI configuration
    #[serde(default)]
    pub ui: UIConfig,

    /// Operation mode configuration
    #[serde(default)]
    pub mode: ModeConfig,

    /// Behavior configuration (auto-install, etc.)
    #[serde(default)]
    pub behavior: BehaviorConfig,

    /// Non-interactive mode configuration
    #[serde(default)]
    pub non_interactive: NonInteractiveConfig,
}

/// Default model settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
        prompts::get_system_prompt()
    }
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            provider: String::new(),
            name: String::new(),
            temperature: DEFAULT_TEMPERATURE,
            max_tokens: DEFAULT_MAX_TOKENS,
            system_prompt: Some(Self::default_system_prompt()),
        }
    }
}

/// Ollama configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OllamaConfig {
    /// Ollama server host
    pub host: String,
    /// Ollama server port
    pub port: u16,
    /// Ollama cloud API key (for :cloud models)
    /// Set this to use Ollama's cloud inference service
    /// Get your key at: https://ollama.com/cloud
    pub cloud_api_key: Option<String>,
    /// Number of GPU layers to offload (None = auto, 0 = CPU only, positive = specific count)
    /// Lower values free up VRAM for larger models at the cost of speed
    pub num_gpu: Option<i32>,
    /// Number of CPU threads for processing offloaded layers
    /// Higher values improve CPU inference speed for large models
    pub num_thread: Option<i32>,
    /// Context window size (number of tokens)
    /// Larger values allow longer conversations but use more memory
    pub num_ctx: Option<i32>,
    /// Enable NUMA optimization for multi-CPU systems
    pub numa: Option<bool>,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            host: String::from("localhost"),
            port: DEFAULT_OLLAMA_PORT,
            cloud_api_key: None,
            num_gpu: None,    // Let Ollama auto-detect
            num_thread: None, // Let Ollama auto-detect
            num_ctx: None,    // Use model default
            numa: None,       // Auto-detect
        }
    }
}

/// OpenAI configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
#[serde(default)]
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
#[serde(default)]
pub struct UIConfig {
    /// Color theme
    pub theme: String,
}

impl Default for UIConfig {
    fn default() -> Self {
        Self {
            theme: String::from("dark"),
        }
    }
}

/// Operation mode configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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

/// Behavior configuration (formerly CLI flags)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    /// Automatically install missing Ollama models
    pub auto_install_models: bool,
    /// Preferred backend (ollama only)
    pub backend: String,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            auto_install_models: true,
            backend: String::from("auto"),
        }
    }
}

/// Non-interactive mode configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NonInteractiveConfig {
    /// Output format (text, json, markdown)
    pub output_format: String,
    /// Maximum tokens to generate
    pub max_tokens: usize,
    /// Don't execute agent actions (dry run)
    pub no_execute: bool,
}

impl Default for NonInteractiveConfig {
    fn default() -> Self {
        Self {
            output_format: String::from("text"),
            max_tokens: DEFAULT_MAX_TOKENS,
            no_execute: false,
        }
    }
}

/// Load configuration from single config file
/// Priority: config file > defaults (that's it - no merging, no env vars)
pub fn load_config() -> Result<Config> {
    let config_path = get_config_path()?;

    if config_path.exists() {
        let toml_str = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        let config: Config = toml::from_str(&toml_str)
            .with_context(|| format!("Failed to parse {}. Run 'mermaid init' to regenerate.", config_path.display()))?;
        Ok(config)
    } else {
        Ok(Config::default())
    }
}

/// Get the path to the single config file
pub fn get_config_path() -> Result<PathBuf> {
    Ok(get_config_dir()?.join("config.toml"))
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
    let config_file = get_config_path()?;

    if config_file.exists() {
        println!("Configuration already exists at: {}", config_file.display());
    } else {
        let default_config = Config::default();
        save_config(&default_config, Some(config_file.clone()))?;
        println!("Created configuration at: {}", config_file.display());
    }

    Ok(())
}

/// Persist the last used model to config file
pub fn persist_last_model(model: &str) -> Result<()> {
    let mut config = load_config().unwrap_or_default();
    config.last_used_model = Some(model.to_string());
    save_config(&config, None)
}
