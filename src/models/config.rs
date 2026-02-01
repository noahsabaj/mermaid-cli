/// Unified configuration system for models and backends
///
/// Replaces the fragmented app::Config + models::ModelConfig split
/// with a single, coherent, backend-agnostic configuration structure.

use crate::prompts;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Unified model configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Model identifier (provider/model or just model name)
    /// Examples: "ollama/qwen3-coder:30b", "qwen3-coder:30b", "gpt-4"
    pub model: String,

    /// Temperature (0.0-2.0, controls randomness)
    #[serde(default = "default_temperature")]
    pub temperature: f32,

    /// Maximum tokens to generate
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,

    /// Top-p sampling (0.0-1.0)
    pub top_p: Option<f32>,

    /// Frequency penalty (-2.0 to 2.0)
    pub frequency_penalty: Option<f32>,

    /// Presence penalty (-2.0 to 2.0)
    pub presence_penalty: Option<f32>,

    /// System prompt override (None = use default)
    pub system_prompt: Option<String>,

    /// Backend-specific options (provider name -> key/value pairs)
    /// Example: {"ollama": {"num_gpu": "10", "num_ctx": "8192"}}
    #[serde(default)]
    pub backend_options: HashMap<String, HashMap<String, String>>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            model: "ollama/tinyllama".to_string(),
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            top_p: Some(default_top_p()),
            frequency_penalty: None,
            presence_penalty: None,
            system_prompt: Some(prompts::get_system_prompt()),
            backend_options: HashMap::new(),
        }
    }
}

impl ModelConfig {
    /// Get a backend-specific option
    pub fn get_backend_option(&self, backend: &str, key: &str) -> Option<&String> {
        self.backend_options.get(backend)?.get(key)
    }

    /// Get backend option as integer
    pub fn get_backend_option_i32(&self, backend: &str, key: &str) -> Option<i32> {
        self.get_backend_option(backend, key)?
            .parse::<i32>()
            .ok()
    }

    /// Get backend option as boolean
    pub fn get_backend_option_bool(&self, backend: &str, key: &str) -> Option<bool> {
        self.get_backend_option(backend, key)?
            .parse::<bool>()
            .ok()
    }

    /// Set a backend-specific option
    pub fn set_backend_option(&mut self, backend: String, key: String, value: String) {
        self.backend_options
            .entry(backend)
            .or_insert_with(HashMap::new)
            .insert(key, value);
    }

    /// Extract Ollama-specific options
    pub fn ollama_options(&self) -> OllamaOptions {
        OllamaOptions {
            num_gpu: self.get_backend_option_i32("ollama", "num_gpu"),
            num_thread: self.get_backend_option_i32("ollama", "num_thread"),
            num_ctx: self.get_backend_option_i32("ollama", "num_ctx"),
            numa: self.get_backend_option_bool("ollama", "numa"),
            cloud_api_key: self.get_backend_option("ollama", "cloud_api_key").cloned(),
        }
    }

    /// Create config with Plan Mode instructions appended to system prompt
    pub fn with_plan_mode() -> Self {
        let mut config = Self::default();
        config.system_prompt = Some(prompts::get_system_prompt_with_plan_mode());
        config
    }
}

/// Ollama-specific options (extracted from backend_options)
#[derive(Debug, Clone, Default)]
pub struct OllamaOptions {
    pub num_gpu: Option<i32>,
    pub num_thread: Option<i32>,
    pub num_ctx: Option<i32>,
    pub numa: Option<bool>,
    pub cloud_api_key: Option<String>,
}

/// Backend connection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    /// Ollama server URL (default: http://localhost:11434)
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,

    /// Connection timeout in seconds
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,

    /// Request timeout in seconds
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,

    /// Max idle connections per host
    #[serde(default = "default_max_idle")]
    pub max_idle_per_host: usize,

    /// Health check interval in seconds
    #[serde(default = "default_health_check_interval")]
    pub health_check_interval_secs: u64,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            ollama_url: default_ollama_url(),
            timeout_secs: default_timeout(),
            request_timeout_secs: default_request_timeout(),
            max_idle_per_host: default_max_idle(),
            health_check_interval_secs: default_health_check_interval(),
        }
    }
}

// Default value functions
fn default_temperature() -> f32 {
    0.7
}

fn default_max_tokens() -> usize {
    4096
}

fn default_top_p() -> f32 {
    1.0
}

fn default_ollama_url() -> String {
    std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string())
}

fn default_timeout() -> u64 {
    10
}

fn default_request_timeout() -> u64 {
    120
}

fn default_max_idle() -> usize {
    10
}

fn default_health_check_interval() -> u64 {
    30
}
