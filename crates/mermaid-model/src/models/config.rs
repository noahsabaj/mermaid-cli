//! Unified configuration system for models and backends
//!
//! Replaces the fragmented `app::Config` + `models::ModelConfig` split
//! with a single, coherent, backend-agnostic configuration structure.

use crate::constants::DEFAULT_TEMPERATURE;
use crate::models::reasoning::ReasoningLevel;
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
    /// Example: {"ollama": {"`num_gpu"`: "10", "`num_ctx"`: "8192"}}
    #[serde(default)]
    pub backend_options: HashMap<String, HashMap<String, String>>,

    /// `mermaid run --output-schema` formatting turn: the JSON Schema the
    /// response must conform to. Adapters map it to their native constrained
    /// output (OpenAI-compat `response_format`, Gemini `responseJsonSchema`,
    /// Ollama `format`); Anthropic has no native shape and relies on the
    /// prompt + client-side validation. Runtime-only, never persisted.
    #[serde(skip)]
    pub output_schema: Option<serde_json::Value>,

    /// Tool definitions the model sees, already translated into
    /// OpenAI-compatible `{type: "function", function: {name,
    /// description, parameters}}` shape. Runtime-only. Populated by
    /// provider wrappers from `ChatRequest.tools` — adapters iterate
    /// this directly, no internal registry.
    #[serde(skip)]
    pub tools: Vec<serde_json::Value>,

    /// The model's real context window from cache-first live discovery,
    /// copied off `ChatRequest.resolved_context_window` by provider
    /// wrappers. Runtime-only; `None` = unknown (no window clamp).
    #[serde(skip)]
    pub resolved_context_window: Option<usize>,

    /// The model's real per-response output ceiling, copied off
    /// `ChatRequest.resolved_max_output`. Runtime-only; `None` = unknown
    /// (adapters that require `max_tokens` fall back to a floor).
    #[serde(skip)]
    pub resolved_max_output: Option<usize>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            // Intentionally empty — every real construction goes through
            // a provider wrapper that sets `model` immediately.
            model: String::new(),
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            // No prompt by default. Every production path builds this from
            // `ChatRequest.system_prompt` (see each `providers::model::*`
            // wrapper's `build_model_config`), so reaching up into `prompts`
            // here only ever produced a multi-KB string that was immediately
            // overwritten — and made the model layer depend on the app layer's
            // prompt text to do it.
            system_prompt: None,
            dynamic_system_suffix: None,
            reasoning: ReasoningLevel::default(),
            hide_reasoning_trace: false,
            backend_options: HashMap::new(),
            tools: Vec::new(),
            resolved_context_window: None,
            resolved_max_output: None,
            output_schema: None,
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
                Some(format!("{s}\n\n---\n\n{suffix}"))
            },
            (Some(s), _) if !s.is_empty() => Some(s.to_string()),
            (_, Some(suffix)) if !suffix.is_empty() => Some(suffix.to_string()),
            _ => None,
        }
    }

    /// Extract Ollama-specific options
    pub fn ollama_options(&self) -> OllamaOptions {
        OllamaOptions {
            num_gpu: self.get_backend_option_i32("ollama", "num_gpu"),
            num_thread: self.get_backend_option_i32("ollama", "num_thread"),
            num_ctx: self.get_backend_option_i32("ollama", "num_ctx"),
            num_predict: self.get_backend_option_i32("ollama", "num_predict"),
            numa: self.get_backend_option_bool("ollama", "numa"),
        }
    }
}

/// Ollama-specific options (extracted from `backend_options`)
#[derive(Debug, Clone, Default)]
pub struct OllamaOptions {
    pub num_gpu: Option<i32>,
    pub num_thread: Option<i32>,
    pub num_ctx: Option<i32>,
    /// Output token cap (`num_predict`). Ollama left output unbounded, so a
    /// small `num_ctx` was the only stop condition. Derived in
    /// `build_model_config` (see `ollama_sizing::default_ollama_num_predict`):
    /// AUTO gets the full window room, an explicit `max_tokens` is exact.
    pub num_predict: Option<i32>,
    pub numa: Option<bool>,
}

/// Backend connection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    /// Ollama server URL (default: <http://localhost:11434>)
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,

    /// Connection timeout in seconds
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,

    /// Max idle connections per host
    #[serde(default = "default_max_idle")]
    pub max_idle_per_host: usize,

    /// Auto-start a dead *local* Ollama server on connection failure
    /// (`ollama::ensure_running`). Sourced from
    /// `app::Config.ollama.auto_start`; only ever acts on loopback URLs.
    #[serde(default = "default_ollama_autostart")]
    pub ollama_autostart: bool,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            ollama_url: default_ollama_url(),
            timeout_secs: default_timeout(),
            max_idle_per_host: default_max_idle(),
            ollama_autostart: default_ollama_autostart(),
        }
    }
}

// Default value functions
fn default_temperature() -> f32 {
    DEFAULT_TEMPERATURE
}

fn default_max_tokens() -> usize {
    // 0 = AUTO: adapters size the output budget to the model (see
    // `adapters::output_budget`). A positive value is an explicit hard cap.
    0
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

fn default_ollama_autostart() -> bool {
    true
}

fn default_max_idle() -> usize {
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialized `BackendConfig`s from before the autostart knob lack the
    /// key — it must default ON (reviving a dead local server is the
    /// out-of-the-box behavior).
    #[test]
    fn backend_config_defaults_autostart_on_when_key_absent() {
        let cfg: BackendConfig = serde_json::from_str(
            r#"{"ollama_url":"http://localhost:11434","timeout_secs":5,"max_idle_per_host":2}"#,
        )
        .expect("parse");
        assert!(cfg.ollama_autostart);
        assert!(BackendConfig::default().ollama_autostart);
    }
}
