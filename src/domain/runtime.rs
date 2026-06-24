//! Runtime metadata shared by the reducer, recorder, and renderer.
//!
//! These types deliberately carry facts rather than presentation
//! strings. Tool output still contains the provider-facing text that
//! goes back into the model, while this module holds the metadata the
//! UI and future commands can consume without scraping that text.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// External lifecycle signal observed by the app shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSignal {
    Interrupt,
    Terminate,
    Hangup,
}

impl RuntimeSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeSignal::Interrupt => "interrupt",
            RuntimeSignal::Terminate => "terminate",
            RuntimeSignal::Hangup => "hangup",
        }
    }
}

/// Runtime event recorded in state for observability / replay tooling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeTimelineEvent {
    pub kind: RuntimeTimelineKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTimelineKind {
    Signal,
    Process,
    Tool,
    Provider,
}

/// Normalized provider capability snapshot exposed in app state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilitySnapshot {
    pub provider: String,
    pub model: String,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub reasoning: String,
    pub max_context_tokens: Option<usize>,
}

impl ProviderCapabilitySnapshot {
    /// Conservative static snapshot used before a provider has been
    /// resolved. This is intentionally cheap and side-effect free so
    /// the reducer can update it on `/model` without touching network
    /// or credential state.
    pub fn from_model_id(model_id: &str) -> Self {
        let (provider, model) = match model_id.split_once('/') {
            Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
                (provider.to_ascii_lowercase(), model.to_string())
            },
            _ => ("ollama".to_string(), model_id.to_string()),
        };

        let (supports_tools, supports_vision, reasoning) = match provider.as_str() {
            "anthropic" => (true, true, "adaptive".to_string()),
            "gemini" => (true, true, "thinking_level".to_string()),
            "ollama" => (true, false, "binary".to_string()),
            _ => (true, false, "effort".to_string()),
        };

        let max_context_tokens = infer_static_context_window(&provider, &model);

        Self {
            provider,
            model,
            supports_tools,
            supports_vision,
            reasoning,
            max_context_tokens,
        }
    }
}

fn infer_static_context_window(provider: &str, model: &str) -> Option<usize> {
    let model = model.to_ascii_lowercase();
    match provider {
        "anthropic" => Some(200_000),
        "gemini" => Some(1_000_000),
        "openai" if model.contains("gpt-4.1") || model.contains("gpt-5") => Some(400_000),
        "openrouter" if model.contains("claude") => Some(200_000),
        _ => None,
    }
}

pub fn infer_static_context_window_for_model_id(model_id: &str) -> Option<usize> {
    let (provider, model) = match model_id.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (provider.to_ascii_lowercase(), model.to_string())
        },
        _ => ("ollama".to_string(), model_id.to_string()),
    };
    infer_static_context_window(&provider, &model)
}

/// Background process status tracked by Mermaid after launching a
/// command in `execute_command(mode="background")`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedProcessStatus {
    Running,
    Exited,
    Unknown,
}

/// Registry record for a background process Mermaid started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedProcess {
    pub id: String,
    pub pid: u32,
    pub command: String,
    pub cwd: Option<String>,
    pub log_path: String,
    pub detected_url: Option<String>,
    pub status: ManagedProcessStatus,
}

/// Structured metadata extracted from a completed tool run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolRunMetadata {
    #[serde(default)]
    pub detail: ToolMetadata,
    pub line_count: Option<usize>,
    pub byte_count: Option<usize>,
    pub result_count: Option<usize>,
    pub duration_secs: Option<f64>,
    pub process: Option<ManagedProcess>,
    /// User-facing display diff for file mutations. This is captured
    /// at tool execution time so whole-file writes can compare against
    /// the pre-write contents even after the file has been overwritten.
    #[serde(default)]
    pub display_diff: Option<String>,
    #[serde(default)]
    pub diff_truncated: bool,
    #[serde(default)]
    pub artifacts: Vec<ToolArtifact>,
}

/// Tool outcome status independent of how the result is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Success,
    Error,
    Cancelled,
}

/// Typed metadata produced by a specific tool implementation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolMetadata {
    #[default]
    None,
    ReadFile {
        paths: Vec<String>,
        line_count: usize,
        byte_count: usize,
        truncated: bool,
    },
    WriteFile {
        path: String,
        line_count: usize,
        byte_count: usize,
        created: Option<bool>,
    },
    EditFile {
        path: String,
        replacements: usize,
    },
    DeleteFile {
        path: String,
    },
    CreateDirectory {
        path: String,
    },
    WebSearch {
        queries: Vec<String>,
        requested_count: usize,
        result_count: usize,
        sources: Vec<String>,
    },
    WebFetch {
        url: String,
        title: Option<String>,
        line_count: usize,
        byte_count: usize,
    },
    ExecuteCommand {
        command: String,
        working_dir: Option<String>,
        exit_code: Option<i32>,
        timed_out: bool,
        background: bool,
        stdout_lines: usize,
        stderr_lines: usize,
        detected_urls: Vec<String>,
        pid: Option<u32>,
        log_path: Option<String>,
    },
    ComputerUse {
        action: String,
        params: Value,
    },
    Mcp {
        server: String,
        tool: String,
    },
    Subagent {
        model_id: String,
    },
    Custom {
        name: String,
        data: Value,
    },
}

/// Non-text artifact produced by a tool. Images are base64 strings to
/// match the existing chat-message storage format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolArtifact {
    Image { data: String },
    File { path: String },
    Log { path: String },
}

/// Runtime state that is not part of the chat transcript sent to a
/// model, but is useful for UI, slash commands, and debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    pub provider_capabilities: ProviderCapabilitySnapshot,
    #[serde(default)]
    pub processes: Vec<ManagedProcess>,
    #[serde(default)]
    pub timeline: Vec<RuntimeTimelineEvent>,
    /// Estimated token cost of the built-in tool schemas the effect runner
    /// appends to every model request during dispatch. The reducer's
    /// `/context` preview builds an MCP-only request and can't see these, so
    /// the runner reports the figure via `Msg::BuiltinToolSchemaTokens` and
    /// `/context` folds it in to match what dispatch actually decides.
    #[serde(default)]
    pub builtin_tool_schema_tokens: usize,
}

impl RuntimeState {
    pub fn new(model_id: &str) -> Self {
        Self {
            provider_capabilities: ProviderCapabilitySnapshot::from_model_id(model_id),
            processes: Vec::new(),
            timeline: Vec::new(),
            builtin_tool_schema_tokens: 0,
        }
    }

    pub fn set_model(&mut self, model_id: &str) {
        self.provider_capabilities = ProviderCapabilitySnapshot::from_model_id(model_id);
        self.timeline.push(RuntimeTimelineEvent {
            kind: RuntimeTimelineKind::Provider,
            message: format!("model set to {}", model_id),
        });
    }

    pub fn record_signal(&mut self, signal: RuntimeSignal) {
        self.timeline.push(RuntimeTimelineEvent {
            kind: RuntimeTimelineKind::Signal,
            message: format!("received {}", signal.as_str()),
        });
    }

    pub fn register_process(&mut self, process: ManagedProcess) {
        if let Some(existing) = self.processes.iter_mut().find(|p| p.pid == process.pid) {
            *existing = process.clone();
        } else {
            self.processes.push(process.clone());
        }
        self.timeline.push(RuntimeTimelineEvent {
            kind: RuntimeTimelineKind::Process,
            message: format!("registered process {} ({})", process.pid, process.command),
        });
    }
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new("ollama/unknown")
    }
}
