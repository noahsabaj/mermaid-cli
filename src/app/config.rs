use crate::constants::{DEFAULT_MAX_TOKENS, DEFAULT_OLLAMA_PORT, DEFAULT_TEMPERATURE};
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

    /// Non-interactive mode configuration
    #[serde(default)]
    pub non_interactive: NonInteractiveConfig,

    /// MCP server configurations
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    pub mode: SafetyMode,
    pub checkpoint_on_mutation: bool,
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

/// User-supplied OpenAI-compatible provider configuration. All fields are
/// optional — when matching a built-in registry entry, only the supplied
/// fields override; the rest fall back to the registry defaults. For
/// fully custom providers, `base_url` and `api_key_env` are required.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct UserProviderConfig {
    /// Override base URL for `/chat/completions` (None = use built-in
    /// registry default; required for fully custom providers).
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
#[derive(Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Command to execute (e.g., "npx", "node", "python")
    pub command: String,
    /// Command-line arguments
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for the server process
    #[serde(default)]
    pub env: HashMap<String, String>,
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
            .finish()
    }
}

impl std::fmt::Debug for UserProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserProviderConfig")
            .field("base_url", &self.base_url)
            .field("api_key_env", &self.api_key_env)
            .field("extra_headers", &debug_masked_map(&self.extra_headers))
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
            max_tokens: DEFAULT_MAX_TOKENS,
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
            max_tokens: DEFAULT_MAX_TOKENS,
            no_execute: false,
        }
    }
}

/// Load configuration from the single config file.
/// Priority: CLI `-c` overrides > config file > defaults.
pub fn load_config() -> Result<Config> {
    Ok(load_config_with_overrides(&[])?.0)
}

/// Load the config, applying repeatable `-c KEY=VALUE` CLI overrides on top of
/// the file before deserializing. Returns the config plus the dotted paths of
/// any unknown/ignored keys (typo detection), which the startup path warns
/// about.
///
/// This is the seed the future layered-config engine extends: it already folds
/// (file → CLI overrides) through one `toml::Table` before a single deserialize,
/// so adding user/project layers becomes a matter of merging more tables here.
pub fn load_config_with_overrides(overrides: &[String]) -> Result<(Config, Vec<String>)> {
    let config_path = get_config_path()?;
    let mut table = if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        toml::from_str::<toml::Table>(&raw).with_context(|| {
            format!(
                "Failed to parse {}. Run 'mermaid init' to regenerate.",
                config_path.display()
            )
        })?
    } else {
        toml::Table::new()
    };
    apply_cli_overrides(&mut table, overrides)?;
    finalize_config(table)
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

/// Set a dotted `key` path in `table` to `value`, creating intermediate tables.
fn deep_set(table: &mut toml::Table, key: &str, value: toml::Value) -> Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    let mut cur = table;
    for part in &parts[..parts.len() - 1] {
        let next = cur
            .entry((*part).to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        cur = next
            .as_table_mut()
            .with_context(|| format!("cannot set '{key}': '{part}' is not a table"))?;
    }
    cur.insert(parts[parts.len() - 1].to_string(), value);
    Ok(())
}

/// Like [`load_config`] but never fails: on a malformed config, warn on stderr
/// and fall back to defaults (#111); on success, warn about any unknown keys.
pub fn load_config_or_warn() -> Config {
    load_config_or_warn_with_overrides(&[])
}

/// [`load_config_or_warn`] plus CLI `-c` overrides — the startup entry point.
/// Unknown top-level or nested keys are reported (typo detection) but tolerated.
pub fn load_config_or_warn_with_overrides(overrides: &[String]) -> Config {
    match load_config_with_overrides(overrides) {
        Ok((config, ignored)) => {
            for path in &ignored {
                eprintln!(
                    "mermaid: warning: unknown config key '{path}' (ignored — check for a typo)"
                );
            }
            config
        },
        Err(e) => {
            // A TOML parse error renders the offending source line, which can be
            // a secret-bearing one (`extra_headers`/`env`/`api_key_env`); scrub
            // credential-shaped content before it reaches stderr (#F13).
            eprintln!(
                "mermaid: {}",
                crate::utils::redact_secrets(&format!("{e:#}"))
            );
            Config::default()
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

/// Save configuration to file
pub fn save_config(config: &Config, path: Option<PathBuf>) -> Result<()> {
    let path = if let Some(p) = path {
        p
    } else {
        get_config_dir()?.join("config.toml")
    };

    let toml_string = toml::to_string_pretty(config)?;

    // The config can carry literal secrets — `mcp_servers[].env`,
    // `mcp_servers[].args`, and `providers[].extra_headers` all accept inline
    // credential values — so it must not be left world-readable, and a crash
    // mid-write must not truncate it. Write atomically (temp → fsync → rename),
    // creating the temp 0600 on Unix so the renamed file is never even briefly
    // world-readable (this also tightens a pre-existing config, since the new
    // file replaces the old one). Windows relies on the per-user profile ACL.
    #[cfg(unix)]
    crate::runtime::write_atomic_with_mode(&path, toml_string.as_bytes(), 0o600)
        .with_context(|| format!("Failed to write config to {}", path.display()))?;
    #[cfg(not(unix))]
    crate::runtime::write_atomic(&path, toml_string.as_bytes())
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

/// Load the config, apply `mutate`, and save it back — under `PERSIST_LOCK` so
/// concurrent persists can't clobber each other. On a malformed config the error
/// propagates (the caller drops it) rather than overwriting the file with
/// defaults (#111).
fn update_config(mutate: impl FnOnce(&mut Config)) -> Result<()> {
    let _guard = PERSIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut config = load_config()?;
    mutate(&mut config);
    save_config(&config, None)
}

/// Persist the last used model to config file.
pub fn persist_last_model(model: &str) -> Result<()> {
    update_config(|config| config.last_used_model = Some(model.to_string()))
}

/// Persist the user's default reasoning level to config file. Used by the
/// `/reasoning` slash command and the Alt+T cycle handler so the choice survives
/// across sessions.
pub fn persist_default_reasoning(level: ReasoningLevel) -> Result<()> {
    update_config(|config| config.default_model.reasoning = level)
}

/// Persist a reasoning level for a specific model ID
/// (e.g. `<provider>/<model>`). The TUI calls this from Alt+T,
/// `/reasoning <level>`, and the does-not-support-thinking auto-snap so
/// the choice sticks per-model rather than bleeding into other models on
/// next session start.
pub fn persist_reasoning_for_model(model_id: &str, level: ReasoningLevel) -> Result<()> {
    update_config(|config| {
        config
            .reasoning_per_model
            .insert(model_id.to_string(), level);
    })
}

/// Persist (or clear) a per-model Ollama `num_ctx` override. `Some(n)` sets it,
/// `None` removes the entry (returning that model to auto-fit).
pub fn persist_ollama_num_ctx_for_model(model_id: &str, num_ctx: Option<u32>) -> Result<()> {
    update_config(|config| match num_ctx {
        Some(n) => {
            config
                .ollama_num_ctx_per_model
                .insert(model_id.to_string(), n);
        },
        None => {
            config.ollama_num_ctx_per_model.remove(model_id);
        },
    })
}

/// Persist the Ollama RAM-offload toggle (`/context offload on|off`).
pub fn persist_ollama_allow_ram_offload(enabled: bool) -> Result<()> {
    update_config(|config| config.ollama.allow_ram_offload = enabled)
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
