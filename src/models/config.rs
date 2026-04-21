//! Unified configuration system for models and backends
//!
//! Replaces the fragmented app::Config + models::ModelConfig split
//! with a single, coherent, backend-agnostic configuration structure.

use crate::constants::{DEFAULT_MAX_TOKENS, DEFAULT_TEMPERATURE};
use crate::models::reasoning::ReasoningLevel;
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

    /// System prompt override (None = use default)
    pub system_prompt: Option<String>,

    /// Project-specific instructions appended to the system prompt
    /// (Step 5h: MERMAID.md content). Runtime-only — never persisted.
    /// On Anthropic, this gets its own `cache_control` block so the
    /// static base stays cached even when the dynamic suffix changes.
    /// On other adapters, it's concatenated onto the system prompt
    /// with a `---` separator.
    #[serde(skip)]
    pub dynamic_system_suffix: Option<String>,

    /// Requested reasoning depth. Adapters map this to provider-native
    /// shapes via `nearest_effort()` against `ModelCapabilities
    /// ::supports_reasoning`. Defaults to `Medium` — the OpenAI / Anthropic
    /// / Gemini default and the level that produces useful chain-of-thought
    /// without burning excessive latency for routine prompts.
    #[serde(default)]
    pub reasoning: ReasoningLevel,

    /// Hide reasoning traces from the user-facing stream while still
    /// allowing the model to reason server-side. Maps to Ollama's
    /// `--hidethinking` semantics and Anthropic's `thinking.display:
    /// "hidden"`. Internal plumbing; the reducer currently never
    /// sets this (no UI toggle) but the adapter pipeline honors it
    /// when a future toggle lands.
    #[serde(default)]
    pub hide_reasoning_trace: bool,

    /// Backend-specific options (provider name -> key/value pairs)
    /// Example: {"ollama": {"num_gpu": "10", "num_ctx": "8192"}}
    #[serde(default)]
    pub backend_options: HashMap<String, HashMap<String, String>>,

    /// Tool definitions the model sees, already translated into
    /// OpenAI-compatible `{type: "function", function: {name,
    /// description, parameters}}` shape. Runtime-only. Populated by
    /// provider wrappers from `ChatRequest.tools` — adapters iterate
    /// this directly, no internal registry.
    #[serde(skip)]
    pub tools: Vec<serde_json::Value>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            // Intentionally empty — every real construction goes through
            // a provider wrapper that sets `model` immediately.
            model: String::new(),
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            system_prompt: Some(prompts::get_system_prompt()),
            dynamic_system_suffix: None,
            reasoning: ReasoningLevel::default(),
            hide_reasoning_trace: false,
            backend_options: HashMap::new(),
            tools: Vec::new(),
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
        self.get_backend_option(backend, key)?.parse::<i32>().ok()
    }

    /// Get backend option as boolean
    pub fn get_backend_option_bool(&self, backend: &str, key: &str) -> Option<bool> {
        self.get_backend_option(backend, key)?.parse::<bool>().ok()
    }

    /// Set a backend-specific option
    pub fn set_backend_option(&mut self, backend: String, key: String, value: String) {
        self.backend_options
            .entry(backend)
            .or_default()
            .insert(key, value);
    }

    /// Build the system-prompt string for adapters that don't support
    /// per-block cache control (Gemini, OpenAI-compat, Ollama). Joins
    /// the static base and the dynamic suffix (MERMAID.md content)
    /// with a `---` separator. Anthropic's adapter doesn't use this
    /// helper — it emits two separately-cached typed-text blocks.
    ///
    /// Returns `None` only when both fields are empty/unset.
    pub fn combined_system_prompt(&self) -> Option<String> {
        match (
            self.system_prompt.as_deref(),
            self.dynamic_system_suffix.as_deref(),
        ) {
            (Some(s), Some(suffix)) if !s.is_empty() && !suffix.is_empty() => {
                Some(format!("{}\n\n---\n\n{}", s, suffix))
            },
            (Some(s), _) if !s.is_empty() => Some(s.to_string()),
            (_, Some(suffix)) if !suffix.is_empty() => Some(suffix.to_string()),
            _ => None,
        }
    }

    /// Build a ModelConfig from user-facing app Config for a given model ID.
    ///
    /// Centralizes the wiring of temperature, max_tokens, reasoning level,
    /// and Ollama hardware options that was previously scattered across
    /// orchestrator.rs and model.rs.
    ///
    /// Reasoning resolution: per-model preference
    /// (`config.reasoning_per_model[model_id]`) wins, then falls back to
    /// the global `default_model.reasoning`. Set per-model via Alt+T or
    /// `/reasoning <level>` while using the model in question.
    pub fn from_app_config(config: &crate::app::Config, model_id: &str) -> Self {
        let reasoning = config
            .reasoning_per_model
            .get(model_id)
            .copied()
            .unwrap_or(config.default_model.reasoning);
        let mut mc = Self {
            model: model_id.to_string(),
            temperature: config.default_model.temperature,
            max_tokens: config.default_model.max_tokens,
            reasoning,
            ..Self::default()
        };
        if let Some(v) = config.ollama.num_gpu {
            mc.set_backend_option("ollama".into(), "num_gpu".into(), v.to_string());
        }
        if let Some(v) = config.ollama.num_ctx {
            mc.set_backend_option("ollama".into(), "num_ctx".into(), v.to_string());
        }
        if let Some(v) = config.ollama.num_thread {
            mc.set_backend_option("ollama".into(), "num_thread".into(), v.to_string());
        }
        if let Some(v) = config.ollama.numa {
            mc.set_backend_option("ollama".into(), "numa".into(), v.to_string());
        }
        mc
    }

    /// Extract Ollama-specific options
    pub fn ollama_options(&self) -> OllamaOptions {
        OllamaOptions {
            num_gpu: self.get_backend_option_i32("ollama", "num_gpu"),
            num_thread: self.get_backend_option_i32("ollama", "num_thread"),
            num_ctx: self.get_backend_option_i32("ollama", "num_ctx"),
            numa: self.get_backend_option_bool("ollama", "numa"),
        }
    }
}

/// Ollama-specific options (extracted from backend_options)
#[derive(Debug, Clone, Default)]
pub struct OllamaOptions {
    pub num_gpu: Option<i32>,
    pub num_thread: Option<i32>,
    pub num_ctx: Option<i32>,
    pub numa: Option<bool>,
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

    /// Max idle connections per host
    #[serde(default = "default_max_idle")]
    pub max_idle_per_host: usize,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            ollama_url: default_ollama_url(),
            timeout_secs: default_timeout(),
            max_idle_per_host: default_max_idle(),
        }
    }
}

// Default value functions
fn default_temperature() -> f32 {
    DEFAULT_TEMPERATURE
}

fn default_max_tokens() -> usize {
    DEFAULT_MAX_TOKENS
}

fn default_ollama_url() -> String {
    // Real callers always go through `the `providers::factory::ProviderFactory` path`,
    // which reads `app::Config.ollama.host/port` (the single documented config
    // path). This default only fires when constructing `BackendConfig::default`
    // directly (no app config supplied) — primarily tests. Keep it static so
    // the precedence is unambiguous; a `MERMAID_OLLAMA_HOST` env override
    // would belong on `app::Config` loading instead, where it can be
    // documented and surfaced in `mermaid status`.
    "http://localhost:11434".to_string()
}

fn default_timeout() -> u64 {
    10
}

fn default_max_idle() -> usize {
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step 4 wires `default_model.reasoning` from app config into the
    /// per-call ModelConfig. Without this, the user's config-file choice
    /// would silently revert to `Medium` on every session bootstrap.
    #[test]
    fn from_app_config_propagates_reasoning_from_settings() {
        let mut cfg = crate::app::Config::default();
        cfg.default_model.reasoning = ReasoningLevel::High;

        let mc = ModelConfig::from_app_config(&cfg, "ollama/qwen3-coder:30b");
        assert_eq!(mc.reasoning, ReasoningLevel::High);
        assert_eq!(mc.model, "ollama/qwen3-coder:30b");
    }

    #[test]
    fn from_app_config_uses_medium_default_when_unset() {
        let cfg = crate::app::Config::default();
        let mc = ModelConfig::from_app_config(&cfg, "ollama/qwen3-coder:30b");
        assert_eq!(mc.reasoning, ReasoningLevel::Medium);
    }

    /// Per-model preference wins over the global default. This is the
    /// Step 5b semantic: setting `/reasoning high` on Sonnet sticks for
    /// Sonnet without affecting other models.
    #[test]
    fn from_app_config_uses_per_model_preference() {
        let mut cfg = crate::app::Config::default();
        cfg.default_model.reasoning = ReasoningLevel::Low;
        cfg.reasoning_per_model.insert(
            "anthropic/claude-sonnet-4-6".to_string(),
            ReasoningLevel::High,
        );

        let mc_per_model = ModelConfig::from_app_config(&cfg, "anthropic/claude-sonnet-4-6");
        assert_eq!(mc_per_model.reasoning, ReasoningLevel::High);

        // Falls back to default for other models.
        let mc_default = ModelConfig::from_app_config(&cfg, "ollama/foo");
        assert_eq!(mc_default.reasoning, ReasoningLevel::Low);
    }
}
