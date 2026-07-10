use crate::constants::{DEFAULT_OLLAMA_PORT, DEFAULT_TEMPERATURE, LEGACY_DEFAULT_MAX_TOKENS};
use crate::models::ReasoningLevel;
use crate::runtime::{PolicyOverride, SafetyMode};
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

    /// Web tool (`web_search` / `web_fetch`) backend selection.
    #[serde(default)]
    pub web: WebConfig,

    /// TUI appearance preferences (`[ui]` table).
    #[serde(default)]
    pub ui: UiConfig,

    /// Non-interactive mode configuration
    #[serde(default)]
    pub non_interactive: NonInteractiveConfig,

    /// MCP server configurations
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// When unset or true, MCP tools are DEFERRED: instead of advertising
    /// every server's tools on every request, the model gets one
    /// `tool_search` tool that returns matching schemas and promotes them
    /// to direct advertisement. Bounds the always-on tool surface.
    /// `Option` so the derived `Config::default()` and the serde default
    /// agree (both `None` = on) and saved configs don't freeze the value.
    /// Per-server override: `defer = false` on the server entry. Read via
    /// [`Config::mcp_deferral_enabled`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_defer_tools: Option<bool>,

    /// User overrides + custom OpenAI-compatible providers. Keys are
    /// provider names; matching a built-in registry entry overrides its
    /// defaults, anything else defines a fully custom provider.
    /// Example:
    /// ```toml
    /// [providers.groq]
    /// api_key_env = "MY_GROQ_KEY"  # override default GROQ_API_KEY
    ///
    /// [providers.my-vllm]
    /// base_url = "http://192.168.1.42:8000/v1"
    /// api_key_env = "VLLM_KEY"
    /// compat = "openai-effort"
    /// ```
    #[serde(default)]
    pub providers: HashMap<String, UserProviderConfig>,

    /// Per-model reasoning preferences keyed by full model ID
    /// (`provider/name`). Set when the user runs `/reasoning <level>` or
    /// Alt+T cycles while using a specific model — the new value sticks
    /// for that model until changed. Falls back to
    /// `default_model.reasoning` when no entry exists.
    /// Example:
    /// ```toml
    /// [reasoning_per_model]
    /// "<provider>/<model>" = "high"
    /// "ollama/qwen3-coder:30b" = "low"
    /// ```
    #[serde(default)]
    pub reasoning_per_model: HashMap<String, ReasoningLevel>,

    /// Per-model Ollama `num_ctx` override set via `/context <n>`/`max`. Beats
    /// auto-fit; cleared by `/context auto`. Keyed by model id.
    ///
    /// Example:
    /// ```toml
    /// [ollama_num_ctx_per_model]
    /// "ollama/ornith:9b" = 131072
    /// ```
    #[serde(default)]
    pub ollama_num_ctx_per_model: HashMap<String, u32>,

    /// Named model profiles that agents/plugins can request without
    /// hardcoding a concrete provider model. Values are full model IDs.
    /// Example:
    /// ```toml
    /// [model_profiles]
    /// fast = "ollama/qwen3-coder:14b"
    /// large-context = "openai/<model>"
    /// tool-strong = "anthropic/<model>"
    /// vision = "gemini/gemini-2.5-pro"
    /// cheap = "groq/llama-3.3-70b-versatile"
    /// ```
    #[serde(default)]
    pub model_profiles: HashMap<String, String>,

    /// Runtime safety policy. Defaults to `Ask` so mutations / shell /
    /// network actions require approval out of the box; users opt into
    /// `Auto` (LLM-vetted) or `FullAccess` deliberately.
    #[serde(default)]
    pub safety: SafetyConfig,

    /// Durable semantic memory settings.
    #[serde(default)]
    pub memory: MemoryConfig,

    /// `mermaidd` background-daemon settings (task scheduler).
    #[serde(default)]
    pub daemon: DaemonConfig,

    /// Context-compaction settings.
    #[serde(default)]
    pub compaction: CompactionConfig,

    /// Computer-use (desktop control) preferences.
    #[serde(default)]
    pub computer_use: ComputerUseConfig,

    /// Subagent (`agent` tool) settings: drive timeout and user-defined
    /// agent types.
    #[serde(default)]
    pub agents: AgentsConfig,

    /// Runtime-only prompt customizations supplied by CLI flags. These are
    /// deliberately skipped when saving config so one-off agent personas do
    /// not pollute the user's persistent Mermaid settings.
    #[serde(skip)]
    pub prompt: PromptConfig,
}

impl Config {
    /// Effective value of [`Config::mcp_defer_tools`]: unset means ON.
    pub fn mcp_deferral_enabled(&self) -> bool {
        self.mcp_defer_tools.unwrap_or(true)
    }
}

/// TUI appearance preferences.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiConfig {
    /// Color theme the TUI renders with. Switched live via `/theme`.
    #[serde(default)]
    pub theme: ThemeChoice,
}

/// Which built-in color theme the TUI renders with. A typed enum (not a
/// free string) so a typo in config.toml is a clear deserialize error and
/// the reducer's match stays exhaustive when a theme is added.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
}

impl ThemeChoice {
    /// The lowercase config-file spelling (`/theme` echo + persistence).
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeChoice::Dark => "dark",
            ThemeChoice::Light => "light",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PromptConfig {
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
}

impl PromptConfig {
    pub fn render_system_prompt(&self, default_prompt: &str) -> String {
        let mut rendered = self
            .system_prompt
            .as_deref()
            .unwrap_or(default_prompt)
            .trim_end()
            .to_string();

        for extra in &self.append_system_prompt {
            let extra = extra.trim();
            if extra.is_empty() {
                continue;
            }
            if !rendered.is_empty() {
                rendered.push_str("\n\n");
            }
            rendered.push_str(extra);
        }

        rendered
    }

    pub fn is_customized(&self) -> bool {
        self.system_prompt.is_some() || !self.append_system_prompt.is_empty()
    }
}

/// Whether model-driven shell commands may reach the network. `Deny` engages
/// the Linux seccomp network kill-switch (`--no-network`); a no-op on other
/// platforms. Default `Allow` preserves today's behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    #[default]
    Allow,
    Deny,
}

/// Where model-driven shell commands may write. `Project` engages Linux
/// Landlock write-confinement (`--confine-fs`): writes are allowed only beneath
/// the project directory, the system temp directory, and `/dev`; reads and
/// execution stay unrestricted. Best-effort (no-op on kernels without Landlock
/// and on other platforms). Default `Unrestricted` preserves today's behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemPolicy {
    #[default]
    Unrestricted,
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    pub mode: SafetyMode,
    pub checkpoint_on_mutation: bool,
    /// Network access policy for shell commands. `Deny` installs the OS
    /// network kill-switch on Linux. See [`NetworkPolicy`].
    #[serde(default)]
    pub network: NetworkPolicy,
    /// Filesystem write policy for shell commands. `Project` confines writes
    /// to the project/temp/`/dev` directories on Linux. See
    /// [`FilesystemPolicy`].
    #[serde(default)]
    pub filesystem: FilesystemPolicy,
    #[serde(default)]
    pub overrides: Vec<PolicyOverride>,
    /// Model id the `Auto`-mode safety classifier uses to vet borderline
    /// actions. `None` ⇒ vet with the session's active model. Set this to
    /// point the vet at a cheaper/faster model than the one driving the work.
    #[serde(default)]
    pub auto_classifier_model: Option<String>,
    /// Headless escape hatch: when true, non-replayable tools (web/mcp/
    /// subagent/computer_use) are allowed to PROCEED on an `Ask` decision in a
    /// headless run (no approval UI) instead of being blocked. Default `false`
    /// — `mermaid run` in `ask` mode otherwise refuses these. Set via
    /// `--allow-untrusted-tools` or config for CI that needs them.
    #[serde(default)]
    pub allow_untrusted_headless_tools: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            // Safe-by-default: the first run prompts for approval on
            // mutations / shell / network rather than silently auto-allowing
            // everything. FullAccess remains available via config.
            mode: SafetyMode::Ask,
            checkpoint_on_mutation: true,
            network: NetworkPolicy::default(),
            filesystem: FilesystemPolicy::default(),
            overrides: Vec::new(),
            auto_classifier_model: None,
            allow_untrusted_headless_tools: false,
        }
    }
}

/// `mermaidd` background-daemon settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// How many daemon-queued tasks may execute concurrently. Each task is a
    /// full agent run holding a model context, so the default is strictly
    /// serial — honest for a single local GPU. Raise it when the daemon's
    /// tasks target cloud providers (or a box with VRAM to spare).
    pub max_concurrent_tasks: usize,
    /// Wall-clock budget per daemon task, in minutes. `None` keeps the
    /// headless runner's built-in 20-minute deadline; set it to give queued
    /// batch work a shorter (or longer) leash. A task over budget is failed
    /// with a timeout report.
    pub task_timeout_minutes: Option<u64>,
    /// Days to retain finished runtime rows (terminal tasks, stale sessions,
    /// finished tool runs, old compactions, …) before the startup GC prunes
    /// them. Active data is never pruned regardless of this value.
    pub retention_days: i64,
    /// Days to retain `outcomes` reward rows — the self-improving-loop training
    /// corpus. Deliberately longer than `retention_days` so a large training
    /// history survives the shorter task/session window; each outcome's
    /// denormalized context keeps it usable after its task row is pruned.
    pub outcomes_retention_days: i64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            max_concurrent_tasks: 1,
            task_timeout_minutes: None,
            retention_days: 30,
            outcomes_retention_days: 180,
        }
    }
}

/// Durable semantic memory settings (v0.10.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Master switch for agent memory (the tool, the always-loaded index, and
    /// the slash commands). On by default.
    pub enabled: bool,
    /// Byte cap on the always-loaded memory index before it's truncated.
    pub index_cap_bytes: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            index_cap_bytes: crate::constants::MAX_MEMORY_INDEX_BYTES,
        }
    }
}

/// Context-compaction settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    /// Cap on consecutive auto-compact-and-continue recoveries after a
    /// context-window truncation, before the run stops and shows the manual
    /// levers (`/context max`, `/context offload on`). The counter resets
    /// whenever the run makes progress, so this bounds only no-progress
    /// thrashing on a too-small window. `0` means uncapped.
    ///
    /// Example:
    /// ```toml
    /// [compaction]
    /// max_truncation_recoveries = 0  # never give up on its own
    /// ```
    pub max_truncation_recoveries: u8,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            max_truncation_recoveries: crate::constants::COMPACTION_MAX_TRUNCATION_RECOVERIES,
        }
    }
}

/// Computer-use (desktop control) preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ComputerUseConfig {
    /// After a successful click / type_text / press_key, auto-capture the
    /// focused window and attach it inline so the model can verify the result.
    /// On by default (non-breaking); set false to cut the per-action capture
    /// cost + image tokens when visual feedback isn't needed. The model can
    /// still call `screenshot` explicitly.
    pub auto_screenshot: bool,
}

impl Default for ComputerUseConfig {
    fn default() -> Self {
        Self {
            auto_screenshot: true,
        }
    }
}

/// Subagent (`agent` tool) settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsConfig {
    /// Hard ceiling on one subagent drive's wall-clock runtime, in seconds.
    /// `0` falls back to the built-in default (1200 = 20 minutes).
    pub timeout_secs: u64,
    /// User-defined agent types for the `agent` tool's `type` arg, keyed by
    /// type name. A custom name shadows a built-in (`general`, `explore`),
    /// so `[agents.types.explore]` retunes the built-in Explore.
    /// ```toml
    /// [agents.types.scout]
    /// tools = ["read_file", "execute_command"]  # omit for the full child set
    /// safety = "read_only"    # ceiling — the child never runs looser
    /// preamble = "You are a scout: find and report, fast."
    /// model = "ollama/qwen3:8b"  # default model; per-call `model` arg wins
    /// ```
    pub types: HashMap<String, AgentTypeConfig>,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 1200,
            types: HashMap::new(),
        }
    }
}

/// One user-defined agent type (see [`AgentsConfig::types`]). Every field is
/// optional; an empty table behaves like the built-in `general` type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentTypeConfig {
    /// Tool names the child registry is filtered to. Valid names:
    /// `read_file`, `write_file`, `apply_patch`, `delete_file`,
    /// `create_directory`, `execute_command`, `web_search`, `web_fetch`,
    /// `mcp`. Omit for the full child set.
    pub tools: Option<Vec<String>>,
    /// Safety ceiling (canonical mode name: `read_only`/`ask`/`auto`/
    /// `full_access`). The child runs at the LESS permissive of the parent's
    /// live mode and this ceiling.
    pub safety: Option<String>,
    /// Extra system-prompt block appended after the child's subagent
    /// contract.
    pub preamble: Option<String>,
    /// Default model id for this type (e.g. `"ollama/qwen3:8b"`); a per-call
    /// `model` arg wins over it.
    pub model: Option<String>,
}

/// User-supplied remote provider configuration. All fields are optional for a
/// built-in provider; fully custom OpenAI-compatible providers require a base
/// URL and API-key environment variable.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct UserProviderConfig {
    /// Override the provider API base URL (None = built-in default; required
    /// for fully custom providers).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Env var name to read the API key from (None = use the built-in
    /// registry default like `GROQ_API_KEY`; required for fully custom
    /// providers).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Extra HTTP headers sent on every request to this provider.
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
    /// Extra HTTP headers whose VALUES come from environment variables
    /// (map is header name -> env var name), resolved at request-build time so
    /// a secret header (e.g. a gateway token) never has to live in config.toml.
    /// A missing env var is skipped.
    #[serde(default)]
    pub env_headers: HashMap<String, String>,
    /// For fully custom providers (no built-in registry entry), declares
    /// which OpenAI-compatible shape the endpoint speaks. Ignored when
    /// the provider name matches a built-in registry entry. Values:
    /// `"openai"` (no reasoning), `"openai-effort"` (`reasoning_effort`
    /// field), `"openrouter"` (nested `reasoning: {effort}` object).
    #[serde(default)]
    pub compat: Option<String>,
    /// Optional preferred model — surfaced by `mermaid status` and used
    /// as the default when the user picks this provider with no model
    /// suffix.
    #[serde(default)]
    pub default_model: Option<String>,
}

/// MCP server configuration
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Command to execute (e.g., "npx", "node", "python")
    pub command: String,
    /// Command-line arguments
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for the server process
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// If non-empty, only these tool names are exposed to the model.
    #[serde(default)]
    pub enabled_tools: Vec<String>,
    /// Tool names hidden from the model. Takes precedence over `enabled_tools`.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    /// Per-server deferral override: `Some(false)` always advertises this
    /// server's tools directly (skips `tool_search`); `Some(true)` defers
    /// even when the global `mcp_defer_tools` is off; `None` follows the
    /// global setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer: Option<bool>,
}

impl McpServerConfig {
    /// Whether `tool_name` should be exposed to the model: hidden when listed in
    /// `disabled_tools` (which wins), else allowed when `enabled_tools` is empty
    /// (allow-all) or names it.
    pub fn tool_allowed(&self, tool_name: &str) -> bool {
        if self.disabled_tools.iter().any(|t| t == tool_name) {
            return false;
        }
        self.enabled_tools.is_empty() || self.enabled_tools.iter().any(|t| t == tool_name)
    }
}

/// Mask a header/env map for `Debug`: keys are kept (so you can still see which
/// vars are set) but values are never rendered — they hold secrets like API keys
/// and `Authorization` tokens (#F12). A `BTreeMap` keeps the output deterministic.
fn debug_masked_map(
    map: &HashMap<String, String>,
) -> std::collections::BTreeMap<&str, &'static str> {
    map.keys().map(|k| (k.as_str(), "[REDACTED]")).collect()
}

// Manual `Debug` for the secret-bearing config structs so a `{:?}` (into
// tracing, a panic, or an error) cannot dump provider keys / Authorization
// headers / MCP env secrets. `Config` keeps its derived `Debug`, which now
// recurses through these redacting impls (#F12).
impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("command", &self.command)
            // args may carry an inline secret (e.g. `--api-key=sk-...`).
            .field(
                "args",
                &self
                    .args
                    .iter()
                    .map(|a| crate::utils::redact_secrets(a))
                    .collect::<Vec<_>>(),
            )
            .field("env", &debug_masked_map(&self.env))
            // Tool allow/deny lists are plain tool names, not secrets.
            .field("enabled_tools", &self.enabled_tools)
            .field("disabled_tools", &self.disabled_tools)
            .finish()
    }
}

impl std::fmt::Debug for UserProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserProviderConfig")
            .field("base_url", &self.base_url)
            .field("api_key_env", &self.api_key_env)
            .field("extra_headers", &debug_masked_map(&self.extra_headers))
            // Values are env var NAMES (not secrets), so render them.
            .field("env_headers", &self.env_headers)
            .field("compat", &self.compat)
            .field("default_model", &self.default_model)
            .finish()
    }
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
    /// Default reasoning depth used for new sessions when no `--reasoning`
    /// flag is given. Each adapter snaps this onto the closest level the
    /// model actually supports via `nearest_effort()`.
    pub reasoning: ReasoningLevel,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            provider: String::new(),
            name: String::new(),
            temperature: DEFAULT_TEMPERATURE,
            // 0 = AUTO: the model-scaled output budget (adapters omit the cap so
            // the provider decides, or size it to the context window). A positive
            // value set by the user is an explicit hard cap.
            max_tokens: 0,
            reasoning: ReasoningLevel::default(),
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
    /// Allow Ollama to offload the model/KV cache to system RAM when it doesn't
    /// fit VRAM. **Disabled by default**: RAM offload is 5–20× slower, so by
    /// default Mermaid auto-fits `num_ctx` to VRAM (keeping the model on the
    /// GPU). Enable to trade speed for a larger context window. Toggle in-app
    /// with `/context offload on|off`.
    pub allow_ram_offload: bool,
    /// Optional hard cap on the auto-fitted context window (in tokens). `None`
    /// lets auto-fit use the full memory budget up to the model's max; set this
    /// to bound it (e.g. to leave VRAM headroom for other apps).
    pub max_auto_num_ctx: Option<usize>,
    /// Start `ollama serve` automatically when the configured server is local
    /// (loopback) and not running — the user should never have to leave
    /// mermaid to start Ollama. Disable if you manage the server yourself
    /// (e.g. systemd with custom flags). Never applies to remote hosts.
    pub auto_start: bool,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            host: String::from("localhost"),
            port: DEFAULT_OLLAMA_PORT,
            num_gpu: None,            // Let Ollama auto-detect
            num_thread: None,         // Let Ollama auto-detect
            num_ctx: None,            // Use model default (overrides auto-fit)
            numa: None,               // Auto-detect
            allow_ram_offload: false, // VRAM-only by default (RAM is slow)
            max_auto_num_ctx: None,   // No cap; auto-fit to the memory budget
            auto_start: true,         // A dead local server is mermaid's problem
        }
    }
}

/// Backend for the `web_fetch` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FetchBackend {
    /// Fetch the URL directly from this machine and convert it to markdown.
    /// No API key, no third party — works for any user with network access.
    #[default]
    Native,
    /// Route through Ollama Cloud's `/api/web_fetch` (needs `OLLAMA_API_KEY`).
    Ollama,
}

/// Backend for the `web_search` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchBackend {
    /// Zero-config default: Ollama Cloud when `OLLAMA_API_KEY` is set, otherwise
    /// an auto-managed local SearXNG container (mermaid starts it on the first
    /// search and tears it down on exit). The user configures nothing.
    #[default]
    Auto,
    /// Ollama Cloud's `/api/web_search` (needs `OLLAMA_API_KEY`).
    Ollama,
    /// A self-hosted SearXNG instance queried at `searxng_url` — keyless.
    Searxng,
}

/// Web tool backend configuration.
///
/// ```toml
/// [web]
/// fetch_backend = "native"   # or "ollama"
/// search_backend = "auto"    # or "ollama" / "searxng"
/// searxng_url = "http://localhost:8080"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    /// Backend for `web_fetch`. `native` (default) fetches the URL from this
    /// machine and needs no key; `ollama` uses Ollama Cloud.
    pub fetch_backend: FetchBackend,
    /// Backend for `web_search`. `auto` (default) uses Ollama Cloud when
    /// `OLLAMA_API_KEY` is set and otherwise auto-manages a local SearXNG
    /// container. `ollama` forces Ollama Cloud; `searxng` forces a self-hosted
    /// instance at `searxng_url`.
    pub search_backend: SearchBackend,
    /// SearXNG base URL, used when `search_backend = "searxng"` (your own
    /// instance). The instance must have the JSON output format enabled
    /// (`search.formats` includes `json`). The `auto` managed instance ignores
    /// this and picks its own port.
    pub searxng_url: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            fetch_backend: FetchBackend::Native,
            search_backend: SearchBackend::Auto,
            searxng_url: String::from("http://localhost:8080"),
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
            // 0 = AUTO (see `ModelSettings::max_tokens`).
            max_tokens: 0,
            no_execute: false,
        }
    }
}

/// One source of configuration in the layered merge. Declaration order IS
/// precedence: every later layer's table is deep-merged over the earlier ones,
/// so `Defaults < User < Project < Session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigLayer {
    /// Built-in defaults (`Config::default()`); the implicit base — an empty
    /// table deserializes to it, so no explicit table is ever built for it.
    Defaults = 0,
    /// The user's `~/.config/mermaid/config.toml` — the only layer persists
    /// write to.
    User = 1,
    /// A repo's `<git-root>/.mermaid/config.toml` (sanitized + tighten-only;
    /// populated by the project-config loader).
    Project = 2,
    /// This invocation's CLI flags: `-c KEY=VALUE` plus the dedicated flags
    /// (`--no-network`, `--confine-fs`, `--sandbox`, `run --max-tokens`,
    /// `run --allow-untrusted-tools`).
    Session = 3,
}

impl ConfigLayer {
    /// Human name used in unknown-key warnings ("in user config (…)").
    fn name(self) -> &'static str {
        match self {
            ConfigLayer::Defaults => "defaults",
            ConfigLayer::User => "user config",
            ConfigLayer::Project => "project config",
            ConfigLayer::Session => "session flags",
        }
    }
}

/// One layer's raw table plus where it came from (for warning attribution).
#[derive(Debug, Clone)]
pub(crate) struct LayerSource {
    /// Which precedence slot this table occupies.
    pub layer: ConfigLayer,
    /// Human-readable origin (file path or "command line") for warnings.
    pub origin: String,
    /// The layer's raw parsed TOML, merged verbatim (already sanitized for
    /// the project layer).
    pub table: toml::Table,
}

/// The per-invocation config overrides carried by CLI flags — the `Session`
/// layer's inputs. Built from the parsed CLI by `Cli::session_flags()`.
#[derive(Debug, Clone, Default)]
pub struct SessionFlags {
    /// Repeatable `-c KEY=VALUE` overrides, applied first (dedicated flags
    /// deep-set on top, so a flag beats a contradictory `-c`).
    pub overrides: Vec<String>,
    /// `--no-network` or `--sandbox` → `safety.network = "deny"`.
    pub deny_network: bool,
    /// `--confine-fs` or `--sandbox` → `safety.filesystem = "project"`.
    pub confine_fs: bool,
    /// `run --max-tokens <n>` → `default_model.max_tokens`.
    pub max_tokens: Option<usize>,
    /// `run --allow-untrusted-tools` → `safety.allow_untrusted_headless_tools`.
    pub allow_untrusted_tools: bool,
}

impl SessionFlags {
    /// Render the flags as the `Session` layer's raw table. `-c` overrides go
    /// in first; the dedicated flags deep-set on top of them, preserving the
    /// historical ordering where `--no-network` beats `-c safety.network=allow`.
    pub(crate) fn to_table(&self) -> Result<toml::Table> {
        let mut table = toml::Table::new();
        apply_cli_overrides(&mut table, &self.overrides)?;
        if self.deny_network {
            deep_set_segments(
                &mut table,
                &["safety", "network"],
                toml::Value::String("deny".into()),
            )?;
        }
        if self.confine_fs {
            deep_set_segments(
                &mut table,
                &["safety", "filesystem"],
                toml::Value::String("project".into()),
            )?;
        }
        if let Some(n) = self.max_tokens {
            deep_set_segments(
                &mut table,
                &["default_model", "max_tokens"],
                toml::Value::Integer(n as i64),
            )?;
        }
        if self.allow_untrusted_tools {
            deep_set_segments(
                &mut table,
                &["safety", "allow_untrusted_headless_tools"],
                toml::Value::Boolean(true),
            )?;
        }
        Ok(table)
    }
}

/// Load the user-scope configuration (defaults + the user file, no project or
/// session layers). This is the view persistence baselines, the daemon, and
/// runtime re-reads use — anything that must not observe another repo's
/// project config or a one-off CLI flag.
pub fn load_config() -> Result<Config> {
    let config_path = get_config_path()?;
    let mut table = read_config_table(&config_path)?;
    migrate_legacy_max_tokens(&mut table);
    Ok(finalize_config(table)?.0)
}

/// A completed layered load: the merged config plus the messages the startup
/// path surfaces.
pub struct LayeredLoad {
    /// The merged, typed configuration.
    pub config: Config,
    /// Layer-attributed unknown-key and project-sanitizer warnings.
    pub warnings: Vec<String>,
    /// Informational lines (e.g. "using project config …").
    pub notices: Vec<String>,
}

/// Load the full layered configuration:
/// defaults < user file < project file < session flags.
/// `cwd` locates the project layer (`<git-root>/.mermaid/config.toml`,
/// sanitized + safety-clamped); pass `None` to skip it (daemon, tests).
pub fn load_layered_config(
    cwd: Option<&std::path::Path>,
    flags: &SessionFlags,
) -> Result<LayeredLoad> {
    let config_path = get_config_path()?;
    let mut user_table = read_config_table(&config_path)?;
    migrate_legacy_max_tokens(&mut user_table);
    let mut layers = vec![LayerSource {
        layer: ConfigLayer::User,
        origin: config_path.display().to_string(),
        table: user_table.clone(),
    }];
    let mut sanitizer_warnings = Vec::new();
    let mut notices = Vec::new();
    if let Some(cwd) = cwd {
        // The tighten-only safety clamp compares against the user-scope
        // (defaults + user file) values.
        let base_safety = finalize_config(user_table)?.0.safety;
        let (layer, warnings, notice) =
            super::project_config::load_project_layer(cwd, &base_safety);
        sanitizer_warnings.extend(warnings);
        notices.extend(notice);
        if let Some(layer) = layer {
            layers.push(layer);
        }
    }
    layers.push(LayerSource {
        layer: ConfigLayer::Session,
        origin: "command line".to_string(),
        table: flags.to_table()?,
    });
    let (config, unknown_key_warnings) = merge_layers(layers)?;
    // Sanitizer warnings first: they explain keys that will also be absent
    // from the merged result.
    sanitizer_warnings.extend(unknown_key_warnings);
    Ok(LayeredLoad {
        config,
        warnings: sanitizer_warnings,
        notices,
    })
}

/// The project-scoped view (defaults + user + project, NO session flags) for
/// runtime re-reads keyed to a workdir — e.g. the memory settings consulted
/// per operation. Never fails and never prints; warnings/notices were already
/// surfaced by the startup load.
pub fn load_project_scoped_config(cwd: &std::path::Path) -> Config {
    fn load(cwd: &std::path::Path) -> Result<Config> {
        let config_path = get_config_path()?;
        let mut user_table = read_config_table(&config_path)?;
        migrate_legacy_max_tokens(&mut user_table);
        let base_safety = finalize_config(user_table.clone())?.0.safety;
        let mut layers = vec![LayerSource {
            layer: ConfigLayer::User,
            origin: config_path.display().to_string(),
            table: user_table,
        }];
        let (layer, _warnings, _notice) =
            super::project_config::load_project_layer(cwd, &base_safety);
        if let Some(layer) = layer {
            layers.push(layer);
        }
        Ok(merge_layers(layers)?.0)
    }
    load(cwd).unwrap_or_default()
}

/// Like [`load_config`] (user scope, no session flags) but never fails: on a
/// malformed config, warn on stderr (secret-redacted, #F13) and fall back to
/// defaults (#111). For standalone subcommands that only read user settings.
pub fn load_config_or_warn() -> Config {
    load_config().unwrap_or_else(|e| {
        eprintln!(
            "mermaid: {}",
            crate::utils::redact_secrets(&format!("{e:#}"))
        );
        Config::default()
    })
}

/// Read and parse one layer's TOML file; a missing file is an empty table.
pub(crate) fn read_config_table(path: &std::path::Path) -> Result<toml::Table> {
    if !path.exists() {
        return Ok(toml::Table::new());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str::<toml::Table>(&raw).with_context(|| {
        format!(
            "Failed to parse {}. Run 'mermaid init' to regenerate.",
            path.display()
        )
    })
}

/// Deep-merge the layers in order (later wins) and deserialize the result
/// once. Unknown-key warnings are collected per layer so each names the file
/// (or flag set) that actually contains the typo.
pub(crate) fn merge_layers(layers: Vec<LayerSource>) -> Result<(Config, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut merged = toml::Table::new();
    for layer in layers {
        collect_layer_warnings(&layer, &mut warnings);
        deep_merge(&mut merged, layer.table);
    }
    let (config, _) = finalize_config(merged)?;
    Ok((config, warnings))
}

/// Run one layer's table through `serde_ignored` purely for warning
/// attribution. A layer that fails to deserialize on its own contributes no
/// warnings — the authoritative merged deserialize in `merge_layers` surfaces
/// any real error (and a later layer may legitimately fix an earlier one's
/// value).
fn collect_layer_warnings(layer: &LayerSource, warnings: &mut Vec<String>) {
    let mut ignored = Vec::new();
    let result: Result<Config, _> =
        serde_ignored::deserialize(toml::Value::Table(layer.table.clone()), |path| {
            ignored.push(path.to_string())
        });
    if result.is_ok() {
        for path in ignored {
            warnings.push(format!(
                "unknown config key '{path}' in {} ({}) — check for a typo",
                layer.layer.name(),
                layer.origin
            ));
        }
    }
}

/// Recursively merge `overlay` into `base`: tables merge key-by-key, while
/// scalars and arrays replace wholesale (arrays are atomic values here — an
/// element-wise merge could never express removing an entry). A kind conflict
/// (table over scalar or vice versa) resolves to the overlay's value.
fn deep_merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base_table)), toml::Value::Table(overlay_table)) => {
                deep_merge(base_table, overlay_table);
            },
            (_, value) => {
                base.insert(key, value);
            },
        }
    }
}

/// One-time migration for the AUTO output-budget change. Existing config files
/// froze the old `default_model.max_tokens = 4096` default to disk (`save_config`
/// serializes every field), which would otherwise pin the stale cap forever.
/// Coerce that legacy value to `0` (AUTO) so upgraded users get the model-scaled
/// budget. Applied to the on-disk table *before* CLI overrides, so an explicit
/// `-c default_model.max_tokens=4096` still wins. The only unpreserved case is a
/// user who hand-wrote exactly `4096` in config.toml — an unusual deliberate
/// value, and AUTO is the better default regardless.
fn migrate_legacy_max_tokens(table: &mut toml::Table) {
    if let Some(dm) = table
        .get_mut("default_model")
        .and_then(|v| v.as_table_mut())
        && dm.get("max_tokens").and_then(|v| v.as_integer())
            == Some(LEGACY_DEFAULT_MAX_TOKENS as i64)
    {
        dm.insert("max_tokens".to_string(), toml::Value::Integer(0));
    }
}

/// Deserialize a (possibly merged) config `Table` into `Config`, collecting the
/// dotted paths of any keys `Config` doesn't recognize so the caller can warn.
/// An empty table yields `Config::default()` (every field is `#[serde(default)]`).
fn finalize_config(table: toml::Table) -> Result<(Config, Vec<String>)> {
    let mut ignored = Vec::new();
    let config: Config = serde_ignored::deserialize(toml::Value::Table(table), |path| {
        ignored.push(path.to_string());
    })
    .context("Failed to interpret configuration. Run 'mermaid init' to regenerate.")?;
    Ok((config, ignored))
}

/// Apply repeatable `-c KEY=VALUE` overrides onto a config table. `KEY` is a
/// dotted path (`default_model.model`); `VALUE` is parsed as a TOML scalar so
/// `true`/`3`/`"x"` keep their types, with a bare word treated as a string.
fn apply_cli_overrides(table: &mut toml::Table, overrides: &[String]) -> Result<()> {
    for raw in overrides {
        let (key, val) = raw
            .split_once('=')
            .with_context(|| format!("invalid -c override '{raw}' (expected KEY=VALUE)"))?;
        let key = key.trim();
        if key.is_empty() {
            anyhow::bail!("invalid -c override '{raw}' (empty key)");
        }
        deep_set(table, key, parse_override_value(val.trim()))?;
    }
    Ok(())
}

/// Parse an override value as a standalone TOML value, falling back to a plain
/// string when it isn't valid TOML on its own (e.g. `ollama/qwen`).
fn parse_override_value(s: &str) -> toml::Value {
    toml::from_str::<toml::Table>(&format!("x = {s}"))
        .ok()
        .and_then(|t| t.get("x").cloned())
        .unwrap_or_else(|| toml::Value::String(s.to_string()))
}

/// Set a dotted `key` path in `table` to `value`, creating intermediate
/// tables. Dotted-path parsing means a `-c` override cannot address a map key
/// that itself contains a dot (e.g. a `reasoning_per_model` model id) — a
/// documented syntax limitation; internal persists use
/// [`deep_set_segments`] directly and are immune.
fn deep_set(table: &mut toml::Table, key: &str, value: toml::Value) -> Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    deep_set_segments(table, &parts, value).with_context(|| format!("cannot set '{key}'"))
}

/// Set a pre-split `path` in `table` to `value`, creating intermediate tables.
/// Segments are literal keys — a segment containing a dot addresses exactly
/// that key (which dotted parsing cannot express).
fn deep_set_segments(table: &mut toml::Table, path: &[&str], value: toml::Value) -> Result<()> {
    let Some((leaf, parents)) = path.split_last() else {
        anyhow::bail!("empty config key path");
    };
    let mut cur = table;
    for part in parents {
        let next = cur
            .entry((*part).to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        cur = next
            .as_table_mut()
            .with_context(|| format!("'{part}' is not a table"))?;
    }
    cur.insert((*leaf).to_string(), value);
    Ok(())
}

/// Remove a pre-split `path` from `table`. Returns whether a value was
/// actually removed. Never creates intermediate tables; a missing parent
/// simply means there was nothing to remove.
pub(crate) fn deep_remove_segments(table: &mut toml::Table, path: &[&str]) -> bool {
    let Some((leaf, parents)) = path.split_last() else {
        return false;
    };
    let mut cur = table;
    for part in parents {
        match cur.get_mut(*part).and_then(|v| v.as_table_mut()) {
            Some(next) => cur = next,
            None => return false,
        }
    }
    cur.remove(*leaf).is_some()
}

/// Like [`load_layered_config`] but never fails — the startup entry point.
/// On success, prints notices and layer-attributed warnings to stderr. On a
/// malformed layer, warns (secret-redacted, #F13) and degrades: the session
/// flags are re-applied over bare defaults so `--no-network`/`-c` survive a
/// corrupt user file rather than being silently dropped with it.
pub fn load_layered_config_or_warn(cwd: Option<&std::path::Path>, flags: &SessionFlags) -> Config {
    match load_layered_config(cwd, flags) {
        Ok(load) => {
            for notice in &load.notices {
                eprintln!("mermaid: {notice}");
            }
            for warning in &load.warnings {
                eprintln!("mermaid: warning: {warning}");
            }
            load.config
        },
        Err(e) => {
            // A TOML parse error renders the offending source line, which can be
            // a secret-bearing one (`extra_headers`/`env`/`api_key_env`); scrub
            // credential-shaped content before it reaches stderr (#F13).
            eprintln!(
                "mermaid: {}",
                crate::utils::redact_secrets(&format!("{e:#}"))
            );
            flags
                .to_table()
                .ok()
                .and_then(|table| finalize_config(table).ok())
                .map(|(config, _)| config)
                .unwrap_or_default()
        },
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

/// Save a full configuration to file. Private on purpose: serializing the
/// whole typed `Config` freezes every default (and would freeze merged
/// project/session values) into the file, so the only legitimate callers are
/// `init_config` (writing pristine defaults to an absent file) and tests.
/// Runtime persistence goes through [`update_user_config_key`] /
/// [`remove_user_config_key`], which rewrite only their own keys.
fn save_config(config: &Config, path: Option<PathBuf>) -> Result<()> {
    let path = if let Some(p) = path {
        p
    } else {
        get_config_dir()?.join("config.toml")
    };
    write_config_bytes(&path, toml::to_string_pretty(config)?.as_bytes())
}

/// Write raw config bytes atomically and owner-only.
///
/// The config can carry literal secrets — `mcp_servers[].env`,
/// `mcp_servers[].args`, and `providers[].extra_headers` all accept inline
/// credential values — so it must not be left world-readable, and a crash
/// mid-write must not truncate it. Write atomically (temp → fsync → rename),
/// creating the temp 0600 on Unix so the renamed file is never even briefly
/// world-readable (this also tightens a pre-existing config, since the new
/// file replaces the old one). Windows relies on the per-user profile ACL.
fn write_config_bytes(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    crate::runtime::write_atomic_with_mode(path, bytes, 0o600)
        .with_context(|| format!("Failed to write config to {}", path.display()))?;
    #[cfg(not(unix))]
    crate::runtime::write_atomic(path, bytes)
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

/// Serializes the read-modify-write persistence path. The `persist_*` helpers
/// run as concurrent detached tasks (dispatched by the effect runner) that all
/// load → mutate → save the same file; without a lock two quick toggles
/// (`/model` then Alt+T) can interleave their loads and lose one write. Held
/// only across the synchronous fs work — never across an `.await`.
static PERSIST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Read the raw USER config table, apply `mutate`, and write it back — under
/// `PERSIST_LOCK` so concurrent persists can't clobber each other. Operating
/// on the raw table (never the merged typed `Config`) means a persist rewrites
/// only its own keys: unknown keys survive, defaults are not frozen in, and
/// project-layer or session-flag values can never leak into the user file.
/// A malformed file propagates the parse error rather than being overwritten
/// with defaults (#111).
fn update_user_config_table(mutate: impl FnOnce(&mut toml::Table) -> Result<()>) -> Result<()> {
    update_user_config_table_at(&get_config_path()?, mutate)
}

/// [`update_user_config_table`] against an explicit path (test seam).
fn update_user_config_table_at(
    path: &std::path::Path,
    mutate: impl FnOnce(&mut toml::Table) -> Result<()>,
) -> Result<()> {
    let _guard = PERSIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut table = read_config_table(path)?;
    // Converge the on-disk legacy output cap while we're rewriting anyway.
    migrate_legacy_max_tokens(&mut table);
    mutate(&mut table)?;
    write_config_bytes(path, toml::to_string_pretty(&table)?.as_bytes())
}

/// Set one key (pre-split path segments, so map keys containing dots — e.g.
/// `reasoning_per_model."ollama/qwen3:8b"` — address correctly) in the USER
/// config file, leaving every other key untouched.
pub fn update_user_config_key(path: &[&str], value: toml::Value) -> Result<()> {
    update_user_config_table(|table| deep_set_segments(table, path, value))
}

/// Remove one key (pre-split path segments) from the USER config file.
/// Returns whether the key existed.
pub fn remove_user_config_key(path: &[&str]) -> Result<bool> {
    let mut removed = false;
    update_user_config_table(|table| {
        removed = deep_remove_segments(table, path);
        Ok(())
    })?;
    Ok(removed)
}

/// Persist the last used model to the user config file.
pub fn persist_last_model(model: &str) -> Result<()> {
    update_user_config_key(&["last_used_model"], toml::Value::String(model.to_string()))
}

/// Persist the TUI theme choice (`/theme dark|light`).
pub fn persist_ui_theme(theme: ThemeChoice) -> Result<()> {
    update_user_config_key(
        &["ui", "theme"],
        toml::Value::String(theme.as_str().to_string()),
    )
}

/// Persist the user's default reasoning level. Used by the `/reasoning` slash
/// command and the Alt+T cycle handler so the choice survives across sessions.
pub fn persist_default_reasoning(level: ReasoningLevel) -> Result<()> {
    update_user_config_key(
        &["default_model", "reasoning"],
        toml::Value::try_from(level)?,
    )
}

/// Persist a reasoning level for a specific model ID
/// (e.g. `<provider>/<model>`). The TUI calls this from Alt+T,
/// `/reasoning <level>`, and the does-not-support-thinking auto-snap so
/// the choice sticks per-model rather than bleeding into other models on
/// next session start.
pub fn persist_reasoning_for_model(model_id: &str, level: ReasoningLevel) -> Result<()> {
    update_user_config_key(
        &["reasoning_per_model", model_id],
        toml::Value::try_from(level)?,
    )
}

/// Persist (or clear) a per-model Ollama `num_ctx` override. `Some(n)` sets it,
/// `None` removes the entry (returning that model to auto-fit).
pub fn persist_ollama_num_ctx_for_model(model_id: &str, num_ctx: Option<u32>) -> Result<()> {
    match num_ctx {
        Some(n) => update_user_config_key(
            &["ollama_num_ctx_per_model", model_id],
            toml::Value::Integer(i64::from(n)),
        ),
        None => remove_user_config_key(&["ollama_num_ctx_per_model", model_id]).map(|_| ()),
    }
}

/// Persist the Ollama RAM-offload toggle (`/context offload on|off`).
pub fn persist_ollama_allow_ram_offload(enabled: bool) -> Result<()> {
    update_user_config_key(
        &["ollama", "allow_ram_offload"],
        toml::Value::Boolean(enabled),
    )
}

/// Resolve which model to use: CLI arg > last_used > default_model > any available
pub async fn resolve_model_id(cli_model: Option<&str>, config: &Config) -> anyhow::Result<String> {
    if let Some(model) = cli_model {
        if let Some(resolved) = resolve_model_profile_alias(model, config)? {
            return Ok(resolved);
        }
        return Ok(model.to_string());
    }
    if let Some(last_model) = &config.last_used_model {
        if let Some(resolved) = resolve_model_profile_alias(last_model, config)? {
            return Ok(resolved);
        }
        return Ok(last_model.clone());
    }
    if !config.default_model.provider.is_empty() && !config.default_model.name.is_empty() {
        return Ok(format!(
            "{}/{}",
            config.default_model.provider, config.default_model.name
        ));
    }
    let available = crate::ollama::require_any_model(config).await?;
    // `require_any_model` already errors on empty, so this `.first()` is
    // never `None` in practice. Use `.first()` over `[0]` so the precondition
    // is enforced by the type system instead of by a comment.
    let first = available
        .first()
        .ok_or_else(|| anyhow::anyhow!("require_any_model returned empty list"))?;
    Ok(format!("ollama/{}", first))
}

fn resolve_model_profile_alias(requested: &str, config: &Config) -> anyhow::Result<Option<String>> {
    let profile = requested.strip_prefix("profile:").unwrap_or(requested);
    if let Some(model) = config.model_profiles.get(profile) {
        anyhow::ensure!(
            !model.trim().is_empty(),
            "model profile `{}` is configured with an empty model id",
            profile
        );
        return Ok(Some(model.clone()));
    }
    if requested.starts_with("profile:") {
        anyhow::bail!(
            "model profile `{}` is not configured; add it under [model_profiles]",
            profile
        );
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_default_max_tokens_migrates_to_auto() {
        // The frozen pre-AUTO default (4096) on disk is coerced to 0 = AUTO…
        let mut table: toml::Table =
            toml::from_str("[default_model]\nmax_tokens = 4096\n").unwrap();
        migrate_legacy_max_tokens(&mut table);
        let (config, _) = finalize_config(table).unwrap();
        assert_eq!(config.default_model.max_tokens, 0);

        // …while any other explicit cap is preserved.
        let mut table: toml::Table =
            toml::from_str("[default_model]\nmax_tokens = 8192\n").unwrap();
        migrate_legacy_max_tokens(&mut table);
        let (config, _) = finalize_config(table).unwrap();
        assert_eq!(config.default_model.max_tokens, 8192);

        // A config without the key is untouched (stays the 0 default).
        let mut table = toml::Table::new();
        migrate_legacy_max_tokens(&mut table);
        let (config, _) = finalize_config(table).unwrap();
        assert_eq!(config.default_model.max_tokens, 0);
    }

    #[test]
    fn ui_theme_deserializes_defaults_and_rejects_typos() {
        let config: Config = toml::from_str("[ui]\ntheme = \"light\"\n").unwrap();
        assert_eq!(config.ui.theme, ThemeChoice::Light);
        // Absent → dark, both from an empty file and from Config::default().
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.ui.theme, ThemeChoice::Dark);
        assert_eq!(Config::default().ui.theme, ThemeChoice::Dark);
        // Typos are a clear deserialize error, not a silent fallback.
        assert!(toml::from_str::<Config>("[ui]\ntheme = \"solarized\"\n").is_err());
    }

    #[test]
    fn finalize_config_flags_unknown_keys() {
        let table: toml::Table =
            toml::from_str("unknown_top = 1\n[default_model]\nmax_tokens = 512\nbogus = true\n")
                .unwrap();
        let (config, ignored) = finalize_config(table).expect("finalizes despite unknown keys");
        assert_eq!(config.default_model.max_tokens, 512);
        assert!(
            ignored.iter().any(|p| p == "unknown_top"),
            "got {ignored:?}"
        );
        assert!(
            ignored.iter().any(|p| p.contains("bogus")),
            "got {ignored:?}"
        );
    }

    #[test]
    fn cli_overrides_beat_file_and_create_nested_tables() {
        // Override beats the file value...
        let mut table: toml::Table = toml::from_str("[default_model]\nmax_tokens = 100\n").unwrap();
        apply_cli_overrides(&mut table, &["default_model.max_tokens=8192".to_string()]).unwrap();
        let (config, ignored) = finalize_config(table).unwrap();
        assert_eq!(config.default_model.max_tokens, 8192);
        assert!(ignored.is_empty());
        // ...and creates a section absent from the file.
        let mut empty = toml::Table::new();
        apply_cli_overrides(&mut empty, &["default_model.max_tokens=256".to_string()]).unwrap();
        assert_eq!(
            finalize_config(empty).unwrap().0.default_model.max_tokens,
            256
        );
    }

    #[test]
    fn parse_override_value_keeps_toml_types_with_string_fallback() {
        assert_eq!(parse_override_value("true"), toml::Value::Boolean(true));
        assert_eq!(parse_override_value("42"), toml::Value::Integer(42));
        assert_eq!(
            parse_override_value("ollama/qwen"),
            toml::Value::String("ollama/qwen".to_string())
        );
    }

    #[test]
    fn cli_override_invalid_format_errors() {
        let mut table = toml::Table::new();
        assert!(apply_cli_overrides(&mut table, &["noequalssign".to_string()]).is_err());
        assert!(apply_cli_overrides(&mut table, &["=novalue".to_string()]).is_err());
    }

    #[test]
    fn deep_merge_recurses_tables_and_replaces_scalars_and_arrays() {
        let mut base: toml::Table = toml::from_str(
            "top = 1\n[ollama]\nhost = \"localhost\"\nport = 11434\n[safety]\noverrides = [\"a\", \"b\"]\n",
        )
        .unwrap();
        let overlay: toml::Table =
            toml::from_str("[ollama]\nhost = \"gpu-box\"\n[safety]\noverrides = [\"c\"]\n")
                .unwrap();
        deep_merge(&mut base, overlay);
        // Sibling keys inside a merged table survive...
        assert_eq!(base["ollama"]["port"].as_integer(), Some(11434));
        // ...the overlaid scalar wins...
        assert_eq!(base["ollama"]["host"].as_str(), Some("gpu-box"));
        // ...arrays replace wholesale (no concat)...
        assert_eq!(base["safety"]["overrides"].as_array().unwrap().len(), 1);
        // ...and untouched top-level keys survive.
        assert_eq!(base["top"].as_integer(), Some(1));
    }

    #[test]
    fn deep_merge_overlay_wins_on_kind_conflict() {
        // Scalar over table and table over scalar both resolve to the overlay.
        let mut base: toml::Table = toml::from_str("[a]\nx = 1\nb = 2\n").unwrap();
        let overlay: toml::Table = toml::from_str("a = 5\n[b]\ny = 3\n").unwrap();
        deep_merge(&mut base, overlay);
        assert_eq!(base["a"].as_integer(), Some(5));
        assert_eq!(base["b"]["y"].as_integer(), Some(3));
    }

    #[test]
    fn merge_layers_precedence_and_layer_attributed_warnings() {
        let user: toml::Table = toml::from_str(
            "last_used_model = \"ollama/a\"\nuser_typo = 1\n[default_model]\nmax_tokens = 100\n",
        )
        .unwrap();
        let session: toml::Table =
            toml::from_str("last_used_model = \"ollama/b\"\nsession_typo = 2\n").unwrap();
        let (config, warnings) = merge_layers(vec![
            LayerSource {
                layer: ConfigLayer::User,
                origin: "/tmp/user.toml".to_string(),
                table: user,
            },
            LayerSource {
                layer: ConfigLayer::Session,
                origin: "command line".to_string(),
                table: session,
            },
        ])
        .expect("merges");
        // Later layer wins; earlier layer's untouched keys survive.
        assert_eq!(config.last_used_model.as_deref(), Some("ollama/b"));
        assert_eq!(config.default_model.max_tokens, 100);
        // Each unknown key names its own layer + origin.
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("user_typo") && w.contains("user config (/tmp/user.toml)")),
            "got {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("session_typo") && w.contains("session flags")),
            "got {warnings:?}"
        );
    }

    #[test]
    fn session_flags_table_maps_each_flag() {
        let flags = SessionFlags {
            overrides: vec!["web.searxng_url=\"http://x:1\"".to_string()],
            deny_network: true,
            confine_fs: true,
            max_tokens: Some(512),
            allow_untrusted_tools: true,
        };
        let (config, _) = finalize_config(flags.to_table().unwrap()).unwrap();
        assert_eq!(config.safety.network, NetworkPolicy::Deny);
        assert_eq!(config.safety.filesystem, FilesystemPolicy::Project);
        assert_eq!(config.default_model.max_tokens, 512);
        assert!(config.safety.allow_untrusted_headless_tools);
        assert_eq!(config.web.searxng_url, "http://x:1");
    }

    #[test]
    fn session_dedicated_flags_beat_dash_c() {
        // `--no-network` wins over a contradictory `-c safety.network=allow`
        // (the dedicated flags deep-set after the -c overrides).
        let flags = SessionFlags {
            overrides: vec!["safety.network=allow".to_string()],
            deny_network: true,
            ..Default::default()
        };
        let (config, _) = finalize_config(flags.to_table().unwrap()).unwrap();
        assert_eq!(config.safety.network, NetworkPolicy::Deny);
    }

    #[test]
    fn corrupt_layer_yields_no_warnings_but_merged_error_surfaces() {
        // A layer that doesn't deserialize on its own contributes no warnings…
        let bad: toml::Table = toml::from_str("[safety]\nmode = 42\n").unwrap();
        let mut warnings = Vec::new();
        collect_layer_warnings(
            &LayerSource {
                layer: ConfigLayer::User,
                origin: "x".to_string(),
                table: bad.clone(),
            },
            &mut warnings,
        );
        assert!(warnings.is_empty());
        // …and the merged deserialize is what errors…
        assert!(
            merge_layers(vec![LayerSource {
                layer: ConfigLayer::User,
                origin: "x".to_string(),
                table: bad.clone(),
            }])
            .is_err()
        );
        // …unless a later layer fixes the value (session repairing a bad file).
        let fix: toml::Table = toml::from_str("[safety]\nmode = \"ask\"\n").unwrap();
        let (config, _) = merge_layers(vec![
            LayerSource {
                layer: ConfigLayer::User,
                origin: "x".to_string(),
                table: bad,
            },
            LayerSource {
                layer: ConfigLayer::Session,
                origin: "command line".to_string(),
                table: fix,
            },
        ])
        .expect("later layer repairs the earlier one");
        assert_eq!(config.safety.mode, SafetyMode::Ask);
    }

    #[test]
    fn project_layer_beats_user_and_loses_to_session() {
        let user: toml::Table = toml::from_str("last_used_model = \"ollama/user\"\n").unwrap();
        let project: toml::Table = toml::from_str(
            "last_used_model = \"ollama/project\"\n[default_model]\nreasoning = \"low\"\n",
        )
        .unwrap();
        let session: toml::Table =
            toml::from_str("last_used_model = \"ollama/session\"\n").unwrap();
        let (config, _) = merge_layers(vec![
            LayerSource {
                layer: ConfigLayer::User,
                origin: "user".to_string(),
                table: user,
            },
            LayerSource {
                layer: ConfigLayer::Project,
                origin: "project".to_string(),
                table: project,
            },
            LayerSource {
                layer: ConfigLayer::Session,
                origin: "command line".to_string(),
                table: session,
            },
        ])
        .expect("merges");
        // Session beats project beats user for the contested key…
        assert_eq!(config.last_used_model.as_deref(), Some("ollama/session"));
        // …while the project's uncontested key lands.
        assert_eq!(config.default_model.reasoning, ReasoningLevel::Low);
    }

    #[test]
    fn session_flags_survive_corrupt_user_layer_fallback() {
        // The or_warn fallback re-applies the session flags over bare defaults;
        // pin the exact expression it uses.
        let flags = SessionFlags {
            deny_network: true,
            ..Default::default()
        };
        let config = flags
            .to_table()
            .ok()
            .and_then(|table| finalize_config(table).ok())
            .map(|(config, _)| config)
            .unwrap_or_default();
        assert_eq!(config.safety.network, NetworkPolicy::Deny);
    }

    #[test]
    fn deep_set_segments_addresses_keys_containing_dots() {
        // A model id with dots must be ONE key, which dotted parsing cannot
        // express — the latent bug the segment API fixes.
        let mut table = toml::Table::new();
        deep_set_segments(
            &mut table,
            &["reasoning_per_model", "gemini/gemini-2.5-pro"],
            toml::Value::String("high".to_string()),
        )
        .unwrap();
        let (config, ignored) = finalize_config(table).unwrap();
        assert!(ignored.is_empty(), "got {ignored:?}");
        assert_eq!(
            config.reasoning_per_model.get("gemini/gemini-2.5-pro"),
            Some(&ReasoningLevel::High)
        );
    }

    #[test]
    fn deep_remove_segments_removes_leaf_only() {
        let mut table: toml::Table =
            toml::from_str("[ollama_num_ctx_per_model]\n\"ollama/a\" = 1\n\"ollama/b\" = 2\n")
                .unwrap();
        assert!(deep_remove_segments(
            &mut table,
            &["ollama_num_ctx_per_model", "ollama/a"]
        ));
        // Sibling survives; parent table survives; missing keys report false.
        assert_eq!(
            table["ollama_num_ctx_per_model"]["ollama/b"].as_integer(),
            Some(2)
        );
        assert!(!deep_remove_segments(
            &mut table,
            &["ollama_num_ctx_per_model", "ollama/a"]
        ));
        assert!(!deep_remove_segments(&mut table, &["nope", "x"]));
    }

    #[test]
    fn update_user_config_table_preserves_unknown_keys() {
        let dir = std::env::temp_dir().join("mermaid_test_config_targeted_persist");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");
        // A file with an unknown key (maybe from a newer mermaid) and one known
        // setting the persist must not disturb.
        std::fs::write(
            &path,
            "future_key = \"kept\"\nlast_used_model = \"ollama/old\"\n\n[ollama]\nport = 12345\n",
        )
        .expect("seed");

        update_user_config_table_at(&path, |table| {
            deep_set_segments(
                table,
                &["last_used_model"],
                toml::Value::String("ollama/new".to_string()),
            )
        })
        .expect("persist");

        let blob = std::fs::read_to_string(&path).expect("read back");
        let table: toml::Table = toml::from_str(&blob).expect("parse back");
        // The targeted key changed…
        assert_eq!(table["last_used_model"].as_str(), Some("ollama/new"));
        // …the unknown key survived (typed round-trips would have dropped it)…
        assert_eq!(table["future_key"].as_str(), Some("kept"));
        // …and no defaults were frozen in (only the keys that were there).
        assert!(!blob.contains("safety"), "defaults must not be frozen in");
        assert_eq!(table["ollama"]["port"].as_integer(), Some(12345));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_tool_allowed_honors_enabled_and_disabled() {
        // Default (both empty) allows everything.
        let cfg = McpServerConfig::default();
        assert!(cfg.tool_allowed("anything"));
        // enabled_tools acts as an allowlist.
        let cfg = McpServerConfig {
            enabled_tools: vec!["read".into(), "search".into()],
            ..Default::default()
        };
        assert!(cfg.tool_allowed("read"));
        assert!(!cfg.tool_allowed("write"));
        // disabled_tools wins over enabled_tools.
        let cfg = McpServerConfig {
            enabled_tools: vec!["read".into(), "write".into()],
            disabled_tools: vec!["write".into()],
            ..Default::default()
        };
        assert!(cfg.tool_allowed("read"));
        assert!(!cfg.tool_allowed("write"));
    }

    /// Configs persisted before Step 4 don't have a `reasoning` field on
    /// `[default_model]`. Loading them must succeed and yield the
    /// `Medium` default — otherwise existing user configs break on
    /// upgrade.
    #[test]
    fn model_settings_deserializes_without_reasoning_field() {
        let toml_blob = r#"
            provider = "ollama"
            name = "qwen3-coder:30b"
            temperature = 0.7
            max_tokens = 4096
        "#;
        let settings: ModelSettings = toml::from_str(toml_blob).expect("backward compat");
        assert_eq!(settings.reasoning, ReasoningLevel::Medium);
        assert_eq!(settings.provider, "ollama");
    }

    #[test]
    fn model_settings_round_trips_reasoning_high() {
        let original = ModelSettings {
            provider: "anthropic".to_string(),
            name: "claude-sonnet-4-6".to_string(),
            temperature: 0.5,
            max_tokens: 8192,
            reasoning: ReasoningLevel::High,
        };
        let toml_blob = toml::to_string(&original).expect("serialize");
        let back: ModelSettings = toml::from_str(&toml_blob).expect("deserialize");
        assert_eq!(back.reasoning, ReasoningLevel::High);
        assert_eq!(back.name, "claude-sonnet-4-6");
    }

    #[test]
    fn agents_config_defaults_and_parses_custom_types() {
        // Absent section → defaults (20-minute timeout, no custom types).
        let config: Config = toml::from_str("").expect("empty config parses");
        assert_eq!(config.agents.timeout_secs, 1200);
        assert!(config.agents.types.is_empty());

        let config: Config = toml::from_str(
            r#"
[agents]
timeout_secs = 300

[agents.types.scout]
tools = ["read_file", "execute_command"]
safety = "read_only"
preamble = "You are a scout."
model = "ollama/qwen3:8b"
"#,
        )
        .expect("agents section parses");
        assert_eq!(config.agents.timeout_secs, 300);
        let scout = &config.agents.types["scout"];
        assert_eq!(
            scout.tools.as_deref(),
            Some(&["read_file".to_string(), "execute_command".to_string()][..])
        );
        assert_eq!(scout.safety.as_deref(), Some("read_only"));
        assert_eq!(scout.model.as_deref(), Some("ollama/qwen3:8b"));
    }

    #[test]
    fn configured_model_profile_resolves_explicit_alias() {
        let mut config = Config::default();
        config
            .model_profiles
            .insert("fast".to_string(), "ollama/qwen3-coder:14b".to_string());
        assert_eq!(
            resolve_model_profile_alias("fast", &config).unwrap(),
            Some("ollama/qwen3-coder:14b".to_string())
        );
        assert_eq!(
            resolve_model_profile_alias("profile:fast", &config).unwrap(),
            Some("ollama/qwen3-coder:14b".to_string())
        );
    }

    #[test]
    fn profile_prefix_requires_configuration() {
        let config = Config::default();
        assert!(resolve_model_profile_alias("profile:vision", &config).is_err());
        assert_eq!(
            resolve_model_profile_alias("vision", &config).unwrap(),
            None
        );
    }

    /// `persist_default_reasoning` writes to the real config path, so
    /// this test goes through `save_config(_, Some(path))` directly to
    /// avoid clobbering the user's actual `~/.config/mermaid/config.toml`.
    /// Uses `std::env::temp_dir` (matching the pattern in
    /// `session::conversation` and `utils::logger`) — no external
    /// `tempfile` crate dependency.
    #[test]
    fn save_and_reload_preserves_reasoning_field() {
        let dir = std::env::temp_dir().join("mermaid_test_config_reasoning");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");

        let mut cfg = Config::default();
        cfg.default_model.provider = "ollama".to_string();
        cfg.default_model.name = "qwen3-coder:30b".to_string();
        cfg.default_model.reasoning = ReasoningLevel::Low;

        save_config(&cfg, Some(path.clone())).expect("save");

        let blob = std::fs::read_to_string(&path).expect("read");
        let loaded: Config = toml::from_str(&blob).expect("parse back");
        assert_eq!(loaded.default_model.reasoning, ReasoningLevel::Low);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Per-model entries serialize as a TOML table with quoted keys (the
    /// model IDs contain `/`). This test verifies the round-trip works
    /// through both serialization and deserialization, matching what
    /// `persist_reasoning_for_model` would produce in real use.
    #[test]
    fn save_and_reload_preserves_reasoning_per_model_table() {
        let dir = std::env::temp_dir().join("mermaid_test_config_per_model_reasoning");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");

        let mut cfg = Config::default();
        cfg.reasoning_per_model.insert(
            "anthropic/claude-sonnet-4-6".to_string(),
            ReasoningLevel::High,
        );
        cfg.reasoning_per_model
            .insert("ollama/qwen3-coder:30b".to_string(), ReasoningLevel::Low);

        save_config(&cfg, Some(path.clone())).expect("save");

        let blob = std::fs::read_to_string(&path).expect("read");
        let loaded: Config = toml::from_str(&blob).expect("parse back");
        assert_eq!(
            loaded
                .reasoning_per_model
                .get("anthropic/claude-sonnet-4-6"),
            Some(&ReasoningLevel::High)
        );
        assert_eq!(
            loaded.reasoning_per_model.get("ollama/qwen3-coder:30b"),
            Some(&ReasoningLevel::Low)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/context <n>` overrides round-trip through the per-model TOML table, and
    /// the offload toggle persists on `[ollama]`.
    #[test]
    fn save_and_reload_preserves_ollama_context_overrides() {
        let dir = std::env::temp_dir().join("mermaid_test_config_ollama_ctx");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");

        let mut cfg = Config::default();
        cfg.ollama_num_ctx_per_model
            .insert("ollama/ornith:9b".to_string(), 131_072);
        cfg.ollama.allow_ram_offload = true;
        cfg.ollama.max_auto_num_ctx = Some(65_536);

        save_config(&cfg, Some(path.clone())).expect("save");
        let blob = std::fs::read_to_string(&path).expect("read");
        let loaded: Config = toml::from_str(&blob).expect("parse back");

        assert_eq!(
            loaded.ollama_num_ctx_per_model.get("ollama/ornith:9b"),
            Some(&131_072)
        );
        assert!(loaded.ollama.allow_ram_offload);
        assert_eq!(loaded.ollama.max_auto_num_ctx, Some(65_536));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Older configs have neither the per-model num_ctx table nor the new
    /// `[ollama]` keys; loading must default cleanly (empty map, offload off).
    #[test]
    fn config_deserializes_without_ollama_context_keys() {
        let toml_blob = r#"
[ollama]
host = "localhost"
port = 11434
"#;
        let cfg: Config = toml::from_str(toml_blob).expect("parse");
        assert!(cfg.ollama_num_ctx_per_model.is_empty());
        assert!(!cfg.ollama.allow_ram_offload);
        assert_eq!(cfg.ollama.max_auto_num_ctx, None);
        // Configs from before the auto-start knob default it ON — reviving a
        // dead local server is the out-of-the-box behavior.
        assert!(cfg.ollama.auto_start);
    }

    /// Configs from before Step 5b don't have a `reasoning_per_model`
    /// section. Loading them must succeed with an empty map — otherwise
    /// upgrade breaks every existing user.
    #[test]
    fn config_deserializes_without_reasoning_per_model() {
        let toml_blob = r#"
            last_used_model = "ollama/qwen3-coder:30b"

            [default_model]
            provider = "ollama"
            name = "qwen3-coder:30b"
            temperature = 0.7
            max_tokens = 4096
        "#;
        let cfg: Config = toml::from_str(toml_blob).expect("backward compat");
        assert!(cfg.reasoning_per_model.is_empty());
        assert!(!cfg.prompt.is_customized());
    }

    /// Config holds inline-secret-capable fields (`mcp_servers[].env`, `args`,
    /// `providers[].extra_headers`), so it must be written owner-only rather
    /// than inheriting a world-readable umask.
    #[cfg(unix)]
    #[test]
    fn save_config_writes_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("mermaid_test_config_perms");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");
        // Pre-create a world-readable file to prove we also tighten existing.
        std::fs::write(&path, "stale").expect("seed");
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));

        save_config(&Config::default(), Some(path.clone())).expect("save");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must be written owner-only");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_defaults_computer_use_auto_screenshot_on() {
        // An empty/legacy config must keep the auto-screenshot behavior (#98).
        let cfg: Config = toml::from_str("").expect("empty config");
        assert!(cfg.computer_use.auto_screenshot);
    }

    #[test]
    fn prompt_config_replaces_and_appends_without_persisting() {
        let mut cfg = Config::default();
        cfg.prompt.system_prompt = Some("base".to_string());
        cfg.prompt
            .append_system_prompt
            .push("extra instructions".to_string());

        assert_eq!(
            cfg.prompt.render_system_prompt("default"),
            "base\n\nextra instructions"
        );

        let blob = toml::to_string(&cfg).expect("serialize");
        assert!(!blob.contains("extra instructions"));
        let loaded: Config = toml::from_str(&blob).expect("deserialize");
        assert!(!loaded.prompt.is_customized());
    }
}
