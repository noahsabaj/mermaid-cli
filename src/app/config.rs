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

    /// Named model-id aliases that agents/plugins can request without
    /// hardcoding a concrete provider model. Values are full model IDs.
    /// (Distinct from `[profiles.<name>]`, which are whole-config overlays
    /// selected with `--profile`.) Example:
    /// ```toml
    /// [model_aliases]
    /// fast = "ollama/qwen3-coder:14b"
    /// large-context = "openai/<model>"
    /// tool-strong = "anthropic/<model>"
    /// vision = "gemini/gemini-2.5-pro"
    /// cheap = "groq/llama-3.3-70b-versatile"
    /// ```
    #[serde(default)]
    pub model_aliases: HashMap<String, String>,

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

    /// Foreground `execute_command` behavior.
    #[serde(default)]
    pub exec: ExecConfig,

    /// Plan-mode behavior (`/plan`, `/safety plan`, Shift+Tab).
    #[serde(default)]
    pub plan: PlanConfig,

    /// Subagent (`agent` tool) settings: drive timeout and user-defined
    /// agent types.
    #[serde(default)]
    pub agents: AgentsConfig,

    /// Runtime-only prompt customizations supplied by CLI flags. These are
    /// deliberately skipped when saving config so one-off agent personas do
    /// not pollute the user's persistent Mermaid settings.
    #[serde(skip)]
    pub prompt: PromptConfig,

    /// The `--profile <name>` overlay active this session, for `doctor` and
    /// startup notices. Runtime-only (`skip`): never persisted, and
    /// `[profiles.*]` itself is excised before deserialization ever sees it.
    #[serde(skip)]
    pub active_profile: Option<String>,
}

impl Config {
    /// Effective value of [`Config::mcp_defer_tools`]: unset means ON.
    pub fn mcp_deferral_enabled(&self) -> bool {
        self.mcp_defer_tools.unwrap_or(true)
    }
}

/// Foreground `execute_command` behavior (`[exec]` table).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecConfig {
    /// Run foreground commands on a pseudo-terminal (openpty on Unix,
    /// ConPTY on Windows). On a PTY, `tty`/`isatty` report a terminal,
    /// spinner-heavy tools emit sane progress, and on Unix `/dev/tty`
    /// resolves to the CAPTURED pty instead of scribbling over the TUI.
    /// `Option` so the
    /// derived default and the serde default agree (both `None` = on) and
    /// saved configs don't freeze the value. `pty = false` restores pipes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty: Option<bool>,
}

impl ExecConfig {
    /// Effective value of [`ExecConfig::pty`]: unset means ON.
    pub fn pty_enabled(&self) -> bool {
        self.pty.unwrap_or(true)
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
        self.append_extras(self.base_prompt(default_prompt))
    }

    /// The base prompt before any `append_system_prompt` extras: the user's
    /// override when set, else `default_prompt`.
    ///
    /// Split out so callers that REWRITE the base (plan mode splices whole
    /// sections out of it) can do so before the extras are appended. Rewriting
    /// the rendered string instead let a section splice run past the end of
    /// the base and delete the user's appended instructions.
    pub fn base_prompt<'a>(&'a self, default_prompt: &'a str) -> &'a str {
        self.system_prompt.as_deref().unwrap_or(default_prompt)
    }

    /// Append the configured extras to an already-chosen base.
    pub fn append_extras(&self, base: &str) -> String {
        let mut rendered = base.trim_end().to_string();

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

/// Whether model-driven actions may reach the network. `Deny` removes web
/// capabilities and engages the shell-command network kill-switch where the
/// OS sandbox supports it. Default `Allow` preserves explicit network use.
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
    /// Network access policy for every model-driven network action. `Deny`
    /// also installs the shell-command OS kill-switch where supported.
    #[serde(default)]
    pub network: NetworkPolicy,
    /// Filesystem write policy for shell commands. `Project` confines writes
    /// to the project/temp/`/dev` directories on Linux. See
    /// [`FilesystemPolicy`].
    #[serde(default)]
    pub filesystem: FilesystemPolicy,
    #[serde(default)]
    pub overrides: Vec<PolicyOverride>,
    /// Enforcement floor for write-shaped MCP tools (no server-advertised
    /// `readOnlyHint`): `allow` | `auto` | `ask` | `deny`. Safety mode alone
    /// never authorizes an external side effect — with the default `auto`,
    /// even full_access routes MCP writes through the intent classifier
    /// (aligned runs silently, off-task escalates). `allow` restores the old
    /// unconditional-allow behavior.
    #[serde(default)]
    pub external_writes: crate::runtime::FloorLevel,
    /// Enforcement floor for machine-scoped package operations (`npm -g`,
    /// `cargo install`, `pip install`, `brew`/`apt`/`winget` installs) —
    /// same levels and default as `external_writes`. They mutate the
    /// MACHINE, not the project (outside checkpoint reach), so even
    /// full_access vets them. Project-local installs (`npm install`,
    /// `cargo add`) are untouched.
    #[serde(default)]
    pub system_installs: crate::runtime::FloorLevel,
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
    /// Explicit user/session opt-in allowing public web reads to proceed in
    /// `read_only` mode. Without it, each request requires one-shot approval;
    /// project configuration is not permitted to enable this capability.
    #[serde(default)]
    pub allow_readonly_web: bool,
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
            external_writes: crate::runtime::FloorLevel::default(),
            system_installs: crate::runtime::FloorLevel::default(),
            auto_classifier_model: None,
            allow_untrusted_headless_tools: false,
            allow_readonly_web: false,
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
    /// Days to retain unlocked per-session scratch directories before the
    /// daemon's startup sweep reaps them. Sessions whose owning process is
    /// still alive are never reaped regardless of age. Interactive sessions
    /// sweep with the built-in default; this knob only tunes mermaidd.
    pub scratchpad_retention_days: i64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            max_concurrent_tasks: 1,
            task_timeout_minutes: None,
            retention_days: 30,
            outcomes_retention_days: 180,
            scratchpad_retention_days: crate::session::scratchpad::RETENTION_DAYS as i64,
        }
    }
}

/// What approval does once granted, when the user has pinned it in config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPostApprove {
    /// Approval immediately auto-submits "Implement the plan."
    Start,
    /// Approval finalizes the plan and returns to the idle prompt.
    Wait,
}

/// Permission level for one plan-mode category. Mirrors the safety-mode
/// ladder so the picker reads familiarly: `allow` runs, `auto` is vetted by
/// the Auto classifier, `ask` raises the approval modal, `deny` blocks with
/// the plan-flavored teaching denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPermLevel {
    Allow,
    Auto,
    Ask,
    Deny,
}

impl PlanPermLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanPermLevel::Allow => "allow",
            PlanPermLevel::Auto => "auto",
            PlanPermLevel::Ask => "ask",
            PlanPermLevel::Deny => "deny",
        }
    }
}

/// Per-category permission profile applied while a plan is being drafted.
/// The read-only floor stays the base; these levels decide how far each
/// carve-out opens. The plan file itself is not a category — being able to
/// author the plan IS plan mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlanPermissions {
    /// Known-safe build/test commands (`is_plan_safe_build_command`).
    pub builds: PlanPermLevel,
    /// `web_search` / `web_fetch` (GET-shaped reads).
    pub web: PlanPermLevel,
    /// Durable memory writes.
    pub memory: PlanPermLevel,
    /// The checklist writers (`task_create` / `task_update`). Only `allow`
    /// unblocks them — `auto`/`ask` collapse to `deny` (they are ungated
    /// tools with no approval path, and the checklist is seeded from the
    /// approved plan anyway).
    pub tasks: PlanPermLevel,
}

impl Default for PlanPermissions {
    fn default() -> Self {
        Self {
            builds: PlanPermLevel::Allow,
            // Planning inherits the ReadOnly web posture: every externally
            // observable URL/query needs one-shot approval unless the user
            // explicitly opens this category in `/plan config`.
            web: PlanPermLevel::Ask,
            memory: PlanPermLevel::Allow,
            tasks: PlanPermLevel::Deny,
        }
    }
}

impl PlanPermissions {
    /// The top-level picker presets; `None` when the current values match
    /// none of them (the picker shows "custom").
    pub fn preset_name(&self) -> Option<&'static str> {
        if *self == Self::default() {
            Some("default")
        } else if *self == Self::strict() {
            Some("strict")
        } else if *self == Self::open() {
            Some("open")
        } else {
            None
        }
    }

    /// Everything denied: pure read-only exploration plus the plan file.
    pub fn strict() -> Self {
        Self {
            builds: PlanPermLevel::Deny,
            web: PlanPermLevel::Deny,
            memory: PlanPermLevel::Deny,
            tasks: PlanPermLevel::Deny,
        }
    }

    /// Everything allowed (the working tree stays read-only regardless).
    pub fn open() -> Self {
        Self {
            builds: PlanPermLevel::Allow,
            web: PlanPermLevel::Allow,
            memory: PlanPermLevel::Allow,
            tasks: PlanPermLevel::Allow,
        }
    }
}

/// Plan-mode settings (`[plan]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlanConfig {
    /// When true, `exit_plan_mode` skips the approval dialog entirely: the
    /// plan is approved the moment the model presents it. Default false —
    /// the dialog is the point of plan mode.
    pub auto_approve: bool,
    /// Pin what approval does. Unset (default) the dialog offers both
    /// "Approve and start" and "Approve and wait" every time; set, it
    /// collapses to a single Approve option with this behavior. Option +
    /// skip_serializing keeps "unset" meaningful in saved configs (the
    /// freeze-defaults rule).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_approve: Option<PlanPostApprove>,
    /// Per-category permission profile while planning. Edited live in the
    /// `/plan config` picker; the reducer threads the LIVE values onto each
    /// tool dispatch (the startup `Config` snapshot in `ExecContext` would
    /// go stale).
    pub permissions: PlanPermissions,
    /// Plan-phase model override: entering plan mode swaps the session to
    /// this model and leaving restores the previous one — plan on a frontier
    /// model, execute locally (or invert for privacy). Unset = plan with
    /// whatever is running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Plan-phase reasoning override, same swap/restore contract as `model`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<crate::models::ReasoningLevel>,
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
///
/// Every field maps onto a [`crate::domain::CompactionPolicy`] knob that was
/// previously a hard-coded constant. Values are sanitized on the way out (see
/// [`CompactionConfig::policy`]) rather than validated on the way in: a bad
/// number should degrade to the nearest sane one, not refuse to start the app.
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

    /// Compact automatically when the context crosses the threshold below.
    /// `false` leaves compaction entirely to `/compact` — the provider's own
    /// context limit then becomes the only backstop.
    pub auto_enabled: bool,

    /// Window fill (percent) at which auto-compaction triggers. Clamped to
    /// `1..=100`; a value of 100 effectively means "only when the response
    /// reserve no longer fits".
    pub auto_threshold_percent: u8,

    /// How many trailing user turns survive compaction verbatim. Clamped to at
    /// least 1 — a compaction that preserved no turn would hand the model a
    /// summary with no live thread to continue.
    pub tail_turns: usize,

    /// Token ceiling on that preserved tail. When the last `tail_turns` exceed
    /// it, older turns are dropped from the tail until it fits.
    pub tail_token_budget: usize,

    /// Per-message character cap applied to tool output inside the summarizer's
    /// history excerpt (prose gets 4x this). Keeps one enormous tool result
    /// from crowding out the rest of the conversation.
    pub tool_output_max_chars: usize,

    /// Ceiling on the checkpoint the summarizer may produce. Scaled DOWN
    /// automatically for small context windows (see
    /// `CompactionPolicy::summary_output_tokens`), so this is a cap and not a
    /// demand.
    pub summary_max_tokens: usize,

    /// Ceiling on the summarizer's input (prompt scaffold plus history
    /// excerpt). Also scaled down to fit a small window.
    pub summarizer_input_token_budget: usize,

    /// Floor and ceiling on the window room held back for the model's reply
    /// when deciding whether the context counts as "full". Swapped values are
    /// corrected rather than rejected.
    pub min_response_reserve_tokens: usize,
    pub max_response_reserve_tokens: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        let policy = crate::domain::CompactionPolicy::default();
        Self {
            max_truncation_recoveries: crate::constants::COMPACTION_MAX_TRUNCATION_RECOVERIES,
            auto_enabled: policy.auto_enabled,
            auto_threshold_percent: policy.auto_threshold_percent,
            tail_turns: policy.tail_turns,
            tail_token_budget: policy.tail_token_budget,
            tool_output_max_chars: policy.tool_output_max_chars,
            summary_max_tokens: policy.summary_max_tokens,
            summarizer_input_token_budget: policy.summarizer_input_token_budget,
            min_response_reserve_tokens: policy.min_response_reserve_tokens,
            max_response_reserve_tokens: policy.max_response_reserve_tokens,
        }
    }
}

impl CompactionConfig {
    /// The live policy, with every value clamped into a range compaction can
    /// actually operate in.
    ///
    /// Sanitizing here rather than at load time means a hand-edited config can
    /// never put the app in a state where compaction silently cannot run — the
    /// failure mode that motivated it is a `min_response_reserve` above
    /// `max_response_reserve`, which would make `response_reserve` return the
    /// smaller *maximum* and quietly under-reserve on every turn.
    pub fn policy(&self) -> crate::domain::CompactionPolicy {
        let defaults = crate::domain::CompactionPolicy::default();
        let min_reserve = self.min_response_reserve_tokens;
        let max_reserve = self.max_response_reserve_tokens;
        crate::domain::CompactionPolicy {
            auto_enabled: self.auto_enabled,
            auto_threshold_percent: self.auto_threshold_percent.clamp(1, 100),
            tail_turns: self.tail_turns.max(1),
            // A zero budget would drop the whole tail; fall back to the default
            // rather than produce a checkpoint with nothing after it.
            tail_token_budget: nonzero_or(self.tail_token_budget, defaults.tail_token_budget),
            tool_output_max_chars: nonzero_or(
                self.tool_output_max_chars,
                defaults.tool_output_max_chars,
            ),
            summary_max_tokens: nonzero_or(self.summary_max_tokens, defaults.summary_max_tokens),
            summarizer_input_token_budget: nonzero_or(
                self.summarizer_input_token_budget,
                defaults.summarizer_input_token_budget,
            ),
            // Order the pair rather than trusting it: swapped bounds are the
            // easy hand-edit mistake, and silently inverting the reserve is
            // worse than ignoring the user's intent about which is which.
            min_response_reserve_tokens: min_reserve.min(max_reserve),
            max_response_reserve_tokens: min_reserve.max(max_reserve),
        }
    }
}

/// `value` unless it is zero, in which case `fallback`.
fn nonzero_or(value: usize, fallback: usize) -> usize {
    if value == 0 { fallback } else { value }
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
    /// Where this type's children write: `shared` (default) puts them in the
    /// session's directory, `worktree` gives each its own git checkout whose
    /// changes are applied to the project only when it finishes. A per-call
    /// `isolation` arg wins over it.
    ///
    /// Isolate a type you fan out with; leave a type shared when its writes
    /// need to be visible to the parent immediately.
    pub isolation: Option<String>,
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
    /// Optional preferred model for this provider (a bare model id like
    /// `claude-x`; a `vendor/model` id is fine too). Used as the startup
    /// model when nothing else pins one — no `--model`, no
    /// `last_used_model`, no `[default_model]`, and no local Ollama — which
    /// is what lets a machine with only a provider key run bare `mermaid`.
    #[serde(default)]
    pub default_model: Option<String>,
}

/// MCP server configuration
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Command to execute (e.g., "npx", "node", "python"). Empty = unset;
    /// exactly one of `command` / `url` must be set (see [`Self::transport_kind`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// Command-line arguments
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for the server process
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Streamable HTTP endpoint URL for a remote MCP server. Presence selects
    /// the HTTP transport; mutually exclusive with `command`. Must never
    /// serialize as a bare `None` — toml errors on unsupported None values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Literal HTTP headers sent on every request to `url` (e.g. an
    /// `Authorization` token). Values are secrets: redacted in `Debug`.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// HTTP headers whose VALUES come from environment variables (map is
    /// header name -> env var name), resolved at request-build time so a
    /// secret header never has to live in config.toml. A missing env var is
    /// skipped. Same semantics as `UserProviderConfig::env_headers`.
    #[serde(default)]
    pub env_headers: HashMap<String, String>,
    /// Allow `url` to resolve to private/link-local addresses. Off by default:
    /// plugin bundles ship MCP configs, and a malicious bundle must not be
    /// able to point a server entry at 169.254.169.254 or the LAN.
    #[serde(default)]
    pub allow_private_network: bool,
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

/// Which transport an [`McpServerConfig`] selects: a spawned child process
/// (stdio) or a remote Streamable HTTP endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Stdio,
    Http,
}

impl McpServerConfig {
    /// Resolve which transport this config selects, enforcing the invariants:
    /// exactly one of `command` / `url` set, and an HTTP url must be `https`
    /// anywhere or `http` to a loopback host only (plaintext to a routable
    /// host would leak `Authorization` headers in cleartext).
    pub fn transport_kind(&self) -> Result<TransportKind> {
        match (&self.url, self.command.is_empty()) {
            (Some(_), false) => Err(anyhow::anyhow!(
                "MCP server config sets both `command` and `url`; they are mutually exclusive"
            )),
            (None, true) => Err(anyhow::anyhow!(
                "MCP server config sets neither `command` nor `url`"
            )),
            (None, false) => Ok(TransportKind::Stdio),
            (Some(url), true) => {
                let parsed = reqwest::Url::parse(url)
                    .map_err(|e| anyhow::anyhow!("invalid MCP server url '{url}': {e}"))?;
                let host = parsed.host_str().unwrap_or("");
                match parsed.scheme() {
                    "https" => Ok(TransportKind::Http),
                    "http" if crate::utils::classify_host(host).is_loopback() => {
                        Ok(TransportKind::Http)
                    },
                    "http" => Err(anyhow::anyhow!(
                        "MCP server url '{url}' uses plaintext http to a non-loopback host; \
                         use https (auth headers would travel in cleartext)"
                    )),
                    other => Err(anyhow::anyhow!(
                        "MCP server url '{url}' has unsupported scheme '{other}' \
                         (expected https, or http to loopback)"
                    )),
                }
            },
        }
    }

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
            .field("url", &self.url)
            // Literal header values are secrets (Authorization tokens).
            .field("headers", &debug_masked_map(&self.headers))
            // Values are env var NAMES (not secrets), so render them.
            .field("env_headers", &self.env_headers)
            .field("allow_private_network", &self.allow_private_network)
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
    /// Sovereign zero-config default: an auto-managed local SearXNG process on
    /// platforms with a published bundle. It never selects a cloud backend
    /// merely because a credential exists.
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
    /// Backend for `web_search`. `auto` (default) auto-manages a local SearXNG
    /// process where a bundle is supported. `ollama` explicitly selects Ollama
    /// Cloud; `searxng` selects a self-hosted instance at `searxng_url`.
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
/// so `Defaults < User < Profile < Project < Session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigLayer {
    /// Built-in defaults (`Config::default()`); the implicit base — an empty
    /// table deserializes to it, so no explicit table is ever built for it.
    Defaults = 0,
    /// The user's `~/.config/mermaid/config.toml` — the only layer persists
    /// write to.
    User = 1,
    /// A named overlay from the user file's `[profiles.<name>]`, selected
    /// with `--profile <name>`. Sits BELOW Project so a repo's tighten-only
    /// safety clamp still wins over a profile's choices.
    Profile = 2,
    /// A repo's `<git-root>/.mermaid/config.toml` (sanitized + tighten-only;
    /// populated by the project-config loader).
    Project = 3,
    /// This invocation's CLI flags: `-c KEY=VALUE` plus the dedicated flags
    /// (`--no-network`, `--confine-fs`, `--sandbox`, `run --max-tokens`,
    /// `run --allow-untrusted-tools`).
    Session = 4,
}

impl ConfigLayer {
    /// Human name used in unknown-key warnings ("in user config (…)").
    fn name(self) -> &'static str {
        match self {
            ConfigLayer::Defaults => "defaults",
            ConfigLayer::User => "user config",
            ConfigLayer::Profile => "config profile",
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
    /// `--profile <name>`: select a `[profiles.<name>]` overlay from the user
    /// config file. NOT rendered into `to_table` — profiles are their own
    /// layer, resolved by `load_layered_config`.
    pub profile: Option<String>,
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

/// Remove the `profiles` table from a raw user-config table and return it
/// (empty when absent). `[profiles.<name>]` overlays must NEVER reach
/// `Config` deserialization — they are a container of layer tables, not
/// config keys — so every user-file read excises them before
/// `finalize_config` (which would otherwise warn about unknown keys) and
/// before any safety baseline is computed.
fn take_profiles(table: &mut toml::Table) -> toml::Table {
    match table.remove("profiles") {
        Some(toml::Value::Table(profiles)) => profiles,
        // A non-table `profiles` key is malformed; drop it (the profile
        // lookup errors clearly when one was requested).
        _ => toml::Table::new(),
    }
}

/// Resolve `--profile <name>` against the user file's excised `[profiles.*]`
/// table: the named overlay as a `Profile` layer, or a hard error naming the
/// available profiles (sorted).
fn resolve_profile_layer(
    profiles: &toml::Table,
    name: &str,
    config_path: &std::path::Path,
) -> Result<LayerSource> {
    match profiles.get(name) {
        Some(toml::Value::Table(overlay)) => Ok(LayerSource {
            layer: ConfigLayer::Profile,
            origin: format!("profile:{} ({})", name, config_path.display()),
            table: overlay.clone(),
        }),
        Some(_) => anyhow::bail!(
            "config profile '{}' is not a table; define it as [profiles.{}] in {}",
            name,
            name,
            config_path.display()
        ),
        None => {
            let mut available: Vec<&str> = profiles.keys().map(String::as_str).collect();
            available.sort_unstable();
            if available.is_empty() {
                anyhow::bail!(
                    "no config profiles defined; add [profiles.{}] to {}",
                    name,
                    config_path.display()
                );
            }
            anyhow::bail!(
                "unknown config profile '{}'; available: {}",
                name,
                available.join(", ")
            )
        },
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
    migrate_legacy_model_profiles(&mut table);
    let _ = take_profiles(&mut table);
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
    migrate_legacy_model_profiles(&mut user_table);
    // Excise [profiles.*] BEFORE anything deserializes the user table (the
    // safety baseline below and finalize_config's unknown-key scan).
    let profiles = take_profiles(&mut user_table);
    let mut layers = vec![LayerSource {
        layer: ConfigLayer::User,
        origin: config_path.display().to_string(),
        table: user_table.clone(),
    }];
    let mut sanitizer_warnings = Vec::new();
    let mut notices = Vec::new();
    if let Some(name) = flags.profile.as_deref() {
        let layer = resolve_profile_layer(&profiles, name, &config_path)?;
        notices.push(format!(
            "using config profile '{}' (from {})",
            name,
            config_path.display()
        ));
        layers.push(layer);
    }
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
    let (mut config, unknown_key_warnings) = merge_layers(layers)?;
    config.active_profile = flags.profile.clone();
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
        migrate_legacy_model_profiles(&mut user_table);
        let _ = take_profiles(&mut user_table);
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

/// Migrate the pre-profiles `[model_profiles]` table to its new name,
/// `[model_aliases]` (the `profile` name now belongs to `--profile` config
/// overlays). Runs wherever `migrate_legacy_max_tokens` runs: config loads
/// stop warning immediately, and the next persist converges the file on
/// disk. A file that somehow has BOTH tables keeps `model_aliases`.
fn migrate_legacy_model_profiles(table: &mut toml::Table) {
    if table.contains_key("model_aliases") {
        table.remove("model_profiles");
        return;
    }
    if let Some(profiles) = table.remove("model_profiles") {
        table.insert("model_aliases".to_string(), profiles);
    }
}

/// Deserialize a (possibly merged) config `Table` into `Config`, collecting the
/// dotted paths of any keys `Config` doesn't recognize so the caller can warn.
/// An empty table yields `Config::default()` (every field is `#[serde(default)]`).
fn finalize_config(table: toml::Table) -> Result<(Config, Vec<String>)> {
    let mut ignored = Vec::new();
    let mut config: Config = serde_ignored::deserialize(toml::Value::Table(table), |path| {
        ignored.push(path.to_string());
    })
    .context("Failed to interpret configuration. Run 'mermaid init' to regenerate.")?;
    // `plan` is a live session mode, not a persistent default: entering it
    // allocates a plan file, which config loading has no session to do it for.
    // `safety.mode = "plan"` would otherwise start a session that reports
    // "planning" with no plan to write. Fall back to the default and let
    // `/plan`, `/safety plan`, or Shift+Tab do the real thing. It is also what
    // `mode_after_plan` reads, so this must never be `plan` itself.
    if config.safety.mode.is_planning() {
        config.safety.mode = SafetyConfig::default().mode;
        ignored.push(
            "safety.mode (plan is entered with /plan or Shift+Tab, not configured)".to_string(),
        );
    }
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
/// `mcp_servers[].args`, `mcp_servers[].headers`, and
/// `providers[].extra_headers` all accept inline credential values — so it
/// must not be left world-readable, and a crash
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
    migrate_legacy_model_profiles(&mut table);
    mutate(&mut table)?;
    write_config_bytes(path, toml::to_string_pretty(&table)?.as_bytes())
}

/// Set one key (pre-split path segments, so map keys containing dots — e.g.
/// `reasoning_per_model."ollama/qwen3:8b"` — address correctly) in the USER
/// config file, leaving every other key untouched.
pub fn update_user_config_key(path: &[&str], value: toml::Value) -> Result<()> {
    update_user_config_table(|table| deep_set_segments(table, path, value))
}

/// Persist the whole `[plan]` table (the `/plan config` picker). Values the
/// user set through the picker are explicit choices, so writing them —
/// including ones that currently match defaults — is correct; unset Options
/// stay absent via `skip_serializing_if`.
pub fn persist_plan_config(plan: &PlanConfig) -> Result<()> {
    update_user_config_key(&["plan"], toml::Value::try_from(plan)?)
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

/// Resolve which model to use: CLI arg > last_used > `[default_model]` > a
/// local Ollama model > a configured provider's `default_model`.
pub async fn resolve_model_id(cli_model: Option<&str>, config: &Config) -> anyhow::Result<String> {
    if let Some(model) = cli_model {
        if let Some(resolved) = resolve_model_alias(model, config)? {
            return Ok(resolved);
        }
        return Ok(model.to_string());
    }
    if let Some(last_model) = &config.last_used_model {
        if let Some(resolved) = resolve_model_alias(last_model, config)? {
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
    // Nothing pinned. Ollama is Mermaid's default backend, not a prerequisite:
    // prefer a local model when one is installed, then a remote provider the
    // user has given an explicit `default_model`, and only then give up — with
    // a message that offers both routes instead of demanding an Ollama install
    // from someone who set `ANTHROPIC_API_KEY` and never wanted local models.
    let local = crate::ollama::local_models(config).await;
    if let Some(first) = local.as_ref().and_then(|models| models.first()) {
        return Ok(format!("ollama/{}", first));
    }
    if let Some(model_id) = configured_provider_default_model(config) {
        return Ok(model_id);
    }
    Err(no_model_configured_error(config, local.is_some()))
}

/// A `[providers.<name>].default_model` belonging to a provider whose API key
/// resolves right now. It is a model id the user typed themselves, so using it
/// as the startup default requires no guess about which models a vendor
/// currently ships — Mermaid never invents model names.
fn configured_provider_default_model(config: &Config) -> Option<String> {
    for provider in crate::providers::configured_remote_providers(config) {
        let model = config
            .providers
            .get(&provider.name)
            .and_then(|entry| entry.default_model.as_deref())
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let Some(model) = model else { continue };
        // The field holds a bare model name, but an id that already carries
        // its provider prefix (or an OpenRouter-style `vendor/model`) must not
        // be double-prefixed into `openrouter/openrouter/...`.
        if model.starts_with(&format!("{}/", provider.name)) {
            return Some(model.to_string());
        }
        return Some(format!("{}/{}", provider.name, model));
    }
    None
}

/// The startup error for "no model is configured yet".
///
/// Ollama is one of two ways to get a model, so this never tells a user who
/// already has a provider key that they must install it. `ollama_installed`
/// distinguishes "install Ollama" from "you have Ollama, pull a model".
fn no_model_configured_error(config: &Config, ollama_installed: bool) -> anyhow::Error {
    let providers = crate::providers::configured_remote_providers(config);
    let mut lines = vec!["No model configured yet.".to_string(), String::new()];

    if let Some(first) = providers.first() {
        let names: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
        lines.push(format!("Remote providers ready: {}", names.join(", ")));
        lines.push("Name a model to use one, e.g.:".to_string());
        lines.push(format!("    mermaid --model {}/<model>", first.name));
        lines.push(
            "Mermaid remembers the last model you used, so --model is a one-time step; \
             `mermaid list` shows what is available."
                .to_string(),
        );
        lines.push(String::new());
        lines.push("Or pin one in config.toml:".to_string());
        lines.push(format!("    [providers.{}]", first.name));
        lines.push("    default_model = \"<model>\"".to_string());
    } else {
        lines.push(
            "For a remote model, set a provider key (ANTHROPIC_API_KEY, OPENAI_API_KEY,"
                .to_string(),
        );
        lines.push("GOOGLE_API_KEY, GROQ_API_KEY, OPENROUTER_API_KEY, …) and name a".to_string());
        lines.push("model: mermaid --model anthropic/<model>".to_string());
    }

    lines.push(String::new());
    if ollama_installed {
        lines.push("For a local model, pull one first: ollama pull qwen3:8b".to_string());
    } else {
        lines.push(
            "For local models, install Ollama (https://ollama.com/download), then: \
             ollama pull qwen3:8b"
                .to_string(),
        );
    }
    lines.push("`mermaid doctor` reports what is and isn't ready.".to_string());

    anyhow::anyhow!(lines.join("\n"))
}

fn resolve_model_alias(requested: &str, config: &Config) -> anyhow::Result<Option<String>> {
    let alias = requested.strip_prefix("alias:").unwrap_or(requested);
    if let Some(model) = config.model_aliases.get(alias) {
        anyhow::ensure!(
            !model.trim().is_empty(),
            "model alias `{}` is configured with an empty model id",
            alias
        );
        return Ok(Some(model.clone()));
    }
    if requested.starts_with("alias:") {
        anyhow::bail!(
            "model alias `{}` is not configured; add it under [model_aliases]",
            alias
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
        migrate_legacy_model_profiles(&mut table);
        let (config, _) = finalize_config(table).unwrap();
        assert_eq!(config.default_model.max_tokens, 0);

        // …while any other explicit cap is preserved.
        let mut table: toml::Table =
            toml::from_str("[default_model]\nmax_tokens = 8192\n").unwrap();
        migrate_legacy_max_tokens(&mut table);
        migrate_legacy_model_profiles(&mut table);
        let (config, _) = finalize_config(table).unwrap();
        assert_eq!(config.default_model.max_tokens, 8192);

        // A config without the key is untouched (stays the 0 default).
        let mut table = toml::Table::new();
        migrate_legacy_max_tokens(&mut table);
        migrate_legacy_model_profiles(&mut table);
        let (config, _) = finalize_config(table).unwrap();
        assert_eq!(config.default_model.max_tokens, 0);
    }

    #[test]
    fn legacy_model_profiles_table_migrates_to_model_aliases() {
        // Loads stop warning immediately...
        let mut table: toml::Table =
            toml::from_str("[model_profiles]\nfast = \"ollama/qwen3:8b\"\n").unwrap();
        migrate_legacy_model_profiles(&mut table);
        let (config, ignored) = finalize_config(table).unwrap();
        assert_eq!(config.model_aliases["fast"], "ollama/qwen3:8b");
        assert!(ignored.is_empty(), "no unknown-key warning: {ignored:?}");
        // ...and a file with BOTH keeps the new table.
        let mut table: toml::Table =
            toml::from_str("[model_profiles]\nfast = \"old\"\n[model_aliases]\nfast = \"new\"\n")
                .unwrap();
        migrate_legacy_model_profiles(&mut table);
        let (config, ignored) = finalize_config(table).unwrap();
        assert_eq!(config.model_aliases["fast"], "new");
        assert!(ignored.is_empty());
        // ...and the persist path rewrites the key on disk.
        let dir = std::env::temp_dir().join("mermaid_test_model_profiles_migrate");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[model_profiles]\nfast = \"ollama/x\"\n").unwrap();
        update_user_config_table_at(&path, |_| Ok(())).unwrap();
        let blob = std::fs::read_to_string(&path).unwrap();
        assert!(blob.contains("[model_aliases]"), "{blob}");
        assert!(!blob.contains("model_profiles"), "{blob}");
        let _ = std::fs::remove_dir_all(&dir);
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
    fn take_profiles_excises_and_tolerates_absence() {
        let mut table: toml::Table =
            toml::from_str("[profiles.fast.default_model]\ntemperature = 0.1\n").unwrap();
        let profiles = take_profiles(&mut table);
        assert!(table.is_empty(), "profiles must be excised: {table:?}");
        assert!(profiles.contains_key("fast"));
        // Absent -> empty, table untouched.
        let mut table: toml::Table = toml::from_str("last_used_model = \"x\"\n").unwrap();
        assert!(take_profiles(&mut table).is_empty());
        assert_eq!(table.len(), 1);
        // Malformed (non-table) -> dropped, empty result.
        let mut table: toml::Table = toml::from_str("profiles = 3\n").unwrap();
        assert!(take_profiles(&mut table).is_empty());
        assert!(table.is_empty());
    }

    #[test]
    fn resolve_profile_layer_errors_name_available_profiles() {
        let profiles: toml::Table = toml::from_str("[work]\n[fast]\n").unwrap();
        let path = std::path::Path::new("/tmp/config.toml");
        let err = resolve_profile_layer(&profiles, "nope", path).unwrap_err();
        assert!(err.to_string().contains("available: fast, work"), "{err}");
        // No profiles at all -> a distinct, actionable error.
        let err = resolve_profile_layer(&toml::Table::new(), "work", path).unwrap_err();
        assert!(
            err.to_string().contains("no config profiles defined"),
            "{err}"
        );
        // Non-table profile value -> hard error.
        let profiles: toml::Table = toml::from_str("work = 1\n").unwrap();
        let err = resolve_profile_layer(&profiles, "work", path).unwrap_err();
        assert!(err.to_string().contains("not a table"), "{err}");
        // Hit -> Profile layer with attributing origin.
        let profiles: toml::Table =
            toml::from_str("[work.default_model]\ntemperature = 0.2\n").unwrap();
        let layer = resolve_profile_layer(&profiles, "work", path).unwrap();
        assert_eq!(layer.layer, ConfigLayer::Profile);
        assert!(layer.origin.contains("profile:work"));
    }

    #[test]
    fn profile_layer_beats_user_loses_to_project_and_session() {
        let user: toml::Table = toml::from_str(
            "last_used_model = \"ollama/user\"\n[default_model]\ntemperature = 0.9\nmax_tokens = 100\n",
        )
        .unwrap();
        let profile: toml::Table = toml::from_str(
            "last_used_model = \"ollama/profile\"\n[default_model]\ntemperature = 0.1\nprofile_typo = 1\n",
        )
        .unwrap();
        let project: toml::Table = toml::from_str("[default_model]\ntemperature = 0.5\n").unwrap();
        let session: toml::Table =
            toml::from_str("last_used_model = \"ollama/session\"\n").unwrap();
        let (config, warnings) = merge_layers(vec![
            LayerSource {
                layer: ConfigLayer::User,
                origin: "/tmp/user.toml".to_string(),
                table: user,
            },
            LayerSource {
                layer: ConfigLayer::Profile,
                origin: "profile:work (/tmp/user.toml)".to_string(),
                table: profile,
            },
            LayerSource {
                layer: ConfigLayer::Project,
                origin: "/repo/.mermaid/config.toml".to_string(),
                table: project,
            },
            LayerSource {
                layer: ConfigLayer::Session,
                origin: "command line".to_string(),
                table: session,
            },
        ])
        .expect("merges");
        // Project beats profile; session beats everything; profile beats user
        // where later layers are silent.
        assert_eq!(config.default_model.temperature, 0.5);
        assert_eq!(config.last_used_model.as_deref(), Some("ollama/session"));
        assert_eq!(config.default_model.max_tokens, 100);
        // Unknown keys inside the profile attribute to it.
        assert!(
            warnings.iter().any(|w| w.contains("profile_typo")
                && w.contains("config profile (profile:work (/tmp/user.toml))")),
            "got {warnings:?}"
        );
    }

    #[test]
    fn persists_never_touch_profile_tables() {
        let dir = std::env::temp_dir().join("mermaid_test_profiles_persist");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[profiles.fast.default_model]\ntemperature = 0.1\n\n[safety]\nmode = \"ask\"\n",
        )
        .expect("seed");

        update_user_config_table_at(&path, |table| {
            deep_set_segments(
                table,
                &["safety", "mode"],
                toml::Value::String("auto".to_string()),
            )
        })
        .expect("persist");

        let table: toml::Table =
            toml::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("parse");
        assert_eq!(table["safety"]["mode"].as_str(), Some("auto"));
        // The overlay table survives persists byte-for-byte semantically.
        assert_eq!(
            table["profiles"]["fast"]["default_model"]["temperature"].as_float(),
            Some(0.1)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_flags_table_maps_each_flag() {
        let flags = SessionFlags {
            overrides: vec!["web.searxng_url=\"http://x:1\"".to_string()],
            deny_network: true,
            confine_fs: true,
            max_tokens: Some(512),
            allow_untrusted_tools: true,
            profile: None,
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

    #[test]
    fn mcp_transport_kind_requires_exactly_one_of_command_and_url() {
        // command-only → stdio.
        let cfg = McpServerConfig {
            command: "npx".to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.transport_kind().unwrap(), TransportKind::Stdio);
        // url-only → http.
        let cfg = McpServerConfig {
            url: Some("https://example.com/mcp".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.transport_kind().unwrap(), TransportKind::Http);
        // Both set → error.
        let cfg = McpServerConfig {
            command: "npx".to_string(),
            url: Some("https://example.com/mcp".to_string()),
            ..Default::default()
        };
        assert!(
            cfg.transport_kind()
                .unwrap_err()
                .to_string()
                .contains("mutually exclusive")
        );
        // Neither set → error.
        let cfg = McpServerConfig::default();
        assert!(
            cfg.transport_kind()
                .unwrap_err()
                .to_string()
                .contains("neither")
        );
    }

    #[test]
    fn mcp_transport_kind_gates_url_scheme() {
        let with_url = |url: &str| McpServerConfig {
            url: Some(url.to_string()),
            ..Default::default()
        };
        // https anywhere is fine; http only to loopback (plaintext to a
        // routable host would leak auth headers).
        assert!(
            with_url("https://mcp.example.com/x")
                .transport_kind()
                .is_ok()
        );
        assert!(
            with_url("http://localhost:8080/mcp")
                .transport_kind()
                .is_ok()
        );
        assert!(
            with_url("http://127.0.0.1:8080/mcp")
                .transport_kind()
                .is_ok()
        );
        assert!(with_url("http://192.168.1.5/mcp").transport_kind().is_err());
        assert!(with_url("ftp://example.com/mcp").transport_kind().is_err());
        assert!(with_url("not a url").transport_kind().is_err());
    }

    #[test]
    fn mcp_server_config_debug_masks_header_values() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer sk-secret".to_string());
        let mut env_headers = HashMap::new();
        env_headers.insert("X-Api-Key".to_string(), "MY_TOKEN_VAR".to_string());
        let cfg = McpServerConfig {
            url: Some("https://example.com/mcp".to_string()),
            headers,
            env_headers,
            ..Default::default()
        };
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("sk-secret"), "{rendered}");
        assert!(rendered.contains("Authorization"), "{rendered}");
        // env_headers values are env var NAMES, safe to render.
        assert!(rendered.contains("MY_TOKEN_VAR"), "{rendered}");
    }

    #[test]
    fn mcp_url_config_round_trips_through_toml_without_command() {
        // `mermaid add --url` persists via toml::Value::try_from; a bare None
        // url or a forced empty `command` key would break that round-trip.
        let cfg = McpServerConfig {
            url: Some("https://example.com/mcp".to_string()),
            ..Default::default()
        };
        let blob = toml::to_string(&toml::Value::try_from(&cfg).unwrap()).unwrap();
        assert!(
            !blob.contains("command"),
            "empty command must be omitted: {blob}"
        );
        let back: McpServerConfig = toml::from_str(&blob).unwrap();
        assert_eq!(back.url.as_deref(), Some("https://example.com/mcp"));
        assert!(back.command.is_empty());
        // And a stdio config must not serialize a `url` key at all.
        let cfg = McpServerConfig {
            command: "npx".to_string(),
            ..Default::default()
        };
        let blob = toml::to_string(&toml::Value::try_from(&cfg).unwrap()).unwrap();
        assert!(!blob.contains("url"), "{blob}");
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
    fn configured_model_alias_resolves_explicit_prefix() {
        let mut config = Config::default();
        config
            .model_aliases
            .insert("fast".to_string(), "ollama/qwen3-coder:14b".to_string());
        assert_eq!(
            resolve_model_alias("fast", &config).unwrap(),
            Some("ollama/qwen3-coder:14b".to_string())
        );
        assert_eq!(
            resolve_model_alias("alias:fast", &config).unwrap(),
            Some("ollama/qwen3-coder:14b".to_string())
        );
    }

    #[test]
    fn alias_prefix_requires_configuration() {
        let config = Config::default();
        assert!(resolve_model_alias("alias:vision", &config).is_err());
        assert_eq!(resolve_model_alias("vision", &config).unwrap(), None);
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
    /// `headers`, `providers[].extra_headers`), so it must be written
    /// owner-only rather than inheriting a world-readable umask.
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

    /// An absent `[compaction]` section must reproduce the constants exactly —
    /// making the policy configurable must not change anyone's behavior.
    #[test]
    fn absent_compaction_section_matches_the_built_in_policy() {
        let c: Config = toml::from_str("").expect("empty config parses");
        assert_eq!(
            c.compaction.policy(),
            crate::domain::CompactionPolicy::default(),
        );
    }

    #[test]
    fn compaction_settings_reach_the_policy() {
        let c: Config = toml::from_str(
            "[compaction]\n\
             auto_enabled = false\n\
             auto_threshold_percent = 60\n\
             tail_turns = 5\n\
             tail_token_budget = 12000\n\
             summary_max_tokens = 3000\n",
        )
        .expect("compaction section parses");
        let policy = c.compaction.policy();
        assert!(!policy.auto_enabled);
        assert_eq!(policy.auto_threshold_percent, 60);
        assert_eq!(policy.tail_turns, 5);
        assert_eq!(policy.tail_token_budget, 12_000);
        assert_eq!(policy.summary_max_tokens, 3_000);
        // Unset keys keep their defaults rather than zeroing out.
        let defaults = crate::domain::CompactionPolicy::default();
        assert_eq!(policy.tool_output_max_chars, defaults.tool_output_max_chars);
    }

    /// A hand-edited config degrades to the nearest workable value rather than
    /// putting compaction in a state where it silently cannot run.
    #[test]
    fn nonsense_compaction_settings_are_clamped() {
        let c: Config = toml::from_str(
            "[compaction]\n\
             auto_threshold_percent = 250\n\
             tail_turns = 0\n\
             tail_token_budget = 0\n\
             summary_max_tokens = 0\n\
             summarizer_input_token_budget = 0\n\
             tool_output_max_chars = 0\n\
             min_response_reserve_tokens = 50000\n\
             max_response_reserve_tokens = 1000\n",
        )
        .expect("config parses");
        let policy = c.compaction.policy();
        let defaults = crate::domain::CompactionPolicy::default();

        assert_eq!(policy.auto_threshold_percent, 100, "percent clamps to 100");
        assert_eq!(
            policy.tail_turns, 1,
            "a checkpoint needs a live turn after it"
        );
        // Zero would mean "no budget at all"; fall back rather than disable.
        assert_eq!(policy.tail_token_budget, defaults.tail_token_budget);
        assert_eq!(policy.summary_max_tokens, defaults.summary_max_tokens);
        assert_eq!(
            policy.summarizer_input_token_budget,
            defaults.summarizer_input_token_budget
        );
        assert_eq!(policy.tool_output_max_chars, defaults.tool_output_max_chars);

        // Swapped reserve bounds are ordered, not obeyed: `response_reserve`
        // clamps with `.max(min).min(max)`, so an inverted pair would return
        // the smaller value and under-reserve on every single turn.
        assert_eq!(policy.min_response_reserve_tokens, 1_000);
        assert_eq!(policy.max_response_reserve_tokens, 50_000);
        assert!(policy.min_response_reserve_tokens <= policy.max_response_reserve_tokens);
    }

    /// `auto_threshold_percent = 0` would compact on every single turn, before
    /// there is anything to compact.
    #[test]
    fn zero_compaction_threshold_clamps_up() {
        let c: Config =
            toml::from_str("[compaction]\nauto_threshold_percent = 0\n").expect("parses");
        assert_eq!(c.compaction.policy().auto_threshold_percent, 1);
    }

    #[test]
    fn plan_config_defaults_parse_and_do_not_freeze() {
        // Absent section: dialog on, nothing pinned.
        let c: Config = toml::from_str("").expect("empty config parses");
        assert!(!c.plan.auto_approve);
        assert!(c.plan.post_approve.is_none());
        // Explicit values parse.
        let c: Config = toml::from_str("[plan]\nauto_approve = true\npost_approve = \"start\"\n")
            .expect("plan section parses");
        assert!(c.plan.auto_approve);
        assert_eq!(c.plan.post_approve, Some(PlanPostApprove::Start));
        assert_eq!(
            toml::from_str::<Config>("[plan]\npost_approve = \"wait\"\n")
                .expect("wait parses")
                .plan
                .post_approve,
            Some(PlanPostApprove::Wait)
        );
        // The unset pin is never frozen into a saved config (Option +
        // skip_serializing_if), so a future default change still reaches
        // existing files.
        let blob = toml::to_string(&Config::default()).expect("serialize");
        assert!(!blob.contains("post_approve"));
    }

    /// Config with one remote provider carrying an explicit `default_model`.
    fn config_with_provider_default(provider: &str, model: &str) -> Config {
        let mut config = Config::default();
        config.providers.insert(
            provider.to_string(),
            UserProviderConfig {
                default_model: Some(model.to_string()),
                ..Default::default()
            },
        );
        config
    }

    /// The whole point of the Ollama-optional path: a machine whose only
    /// backend is Anthropic must resolve a model without Ollama in the picture.
    #[test]
    fn provider_default_model_resolves_without_ollama() {
        let config = config_with_provider_default("anthropic", "claude-x");
        temp_env::with_vars([("ANTHROPIC_API_KEY", Some("sk-test"))], || {
            assert_eq!(
                configured_provider_default_model(&config).as_deref(),
                Some("anthropic/claude-x")
            );
        });
    }

    /// An unconfigured provider's `default_model` is not a usable default —
    /// building it would fail on the missing key at the first request.
    #[test]
    fn provider_default_model_ignored_without_a_key() {
        let config = config_with_provider_default("anthropic", "claude-x");
        temp_env::with_vars([("ANTHROPIC_API_KEY", None::<&str>)], || {
            // The keyring is the machine's, so only assert the env-var half:
            // with no key in the environment there is nothing to prefer.
            if crate::utils::provider_key_source("anthropic", "ANTHROPIC_API_KEY", None) == "none" {
                assert_eq!(configured_provider_default_model(&config), None);
            }
        });
    }

    /// OpenRouter ids are `vendor/model`, which must be prefixed once, not
    /// twice — and an id that already names its provider is left alone.
    #[test]
    fn provider_default_model_is_prefixed_exactly_once() {
        temp_env::with_vars([("OPENROUTER_API_KEY", Some("sk-test"))], || {
            let vendor_model = config_with_provider_default("openrouter", "z-ai/glm-5.2");
            assert_eq!(
                configured_provider_default_model(&vendor_model).as_deref(),
                Some("openrouter/z-ai/glm-5.2")
            );
            let already_prefixed =
                config_with_provider_default("openrouter", "openrouter/z-ai/glm-5.2");
            assert_eq!(
                configured_provider_default_model(&already_prefixed).as_deref(),
                Some("openrouter/z-ai/glm-5.2")
            );
        });
    }

    /// The regression this replaced: startup used to end at "Ollama is not
    /// installed", which reads as "Mermaid needs Ollama". With a provider key
    /// present the message must be about naming a model, not about Ollama.
    #[test]
    fn missing_model_error_does_not_demand_ollama_when_a_provider_is_ready() {
        let config = Config::default();
        temp_env::with_vars([("ANTHROPIC_API_KEY", Some("sk-test"))], || {
            let msg = no_model_configured_error(&config, false).to_string();
            assert!(msg.contains("anthropic"), "{msg}");
            assert!(msg.contains("mermaid --model anthropic/<model>"), "{msg}");
            assert!(msg.contains("[providers.anthropic]"), "{msg}");
            // Ollama may still be mentioned as the local option, but never as
            // a prerequisite for running Mermaid at all.
            assert!(!msg.contains("Ollama is not installed"), "{msg}");
        });
    }

    /// Run `f` with every built-in provider's key env var unset, so a key in
    /// the developer's own shell can't change what the message says.
    fn with_no_provider_keys<T>(f: impl FnOnce() -> T) -> T {
        let cleared: Vec<(&str, Option<&str>)> = [
            crate::providers::model::anthropic::DEFAULT_API_KEY_ENV,
            crate::providers::model::gemini::DEFAULT_API_KEY_ENV,
            crate::providers::model::gemini::LEGACY_API_KEY_ENV,
            crate::providers::model::meta::DEFAULT_API_KEY_ENV,
        ]
        .iter()
        .map(|env| (*env, None))
        .chain(
            crate::models::PROVIDER_REGISTRY
                .iter()
                .map(|profile| (profile.api_key_env, None)),
        )
        .collect();
        temp_env::with_vars(cleared, f)
    }

    /// With nothing configured at all, both routes are offered — the remote
    /// one first, since it needs no install.
    #[test]
    fn missing_model_error_offers_both_routes_when_nothing_is_configured() {
        with_no_provider_keys(|| {
            let msg = no_model_configured_error(&Config::default(), false).to_string();
            assert!(msg.contains("https://ollama.com/download"), "{msg}");
            // A keyring login would legitimately name a provider instead; only
            // assert the no-provider wording when there really is none.
            if !msg.contains("Remote providers ready") {
                assert!(msg.contains("ANTHROPIC_API_KEY"), "{msg}");
            }
        });
    }

    /// End-to-end through `resolve_model_id` itself: nothing pinned, no local
    /// model reachable, one configured provider — Mermaid starts on that
    /// provider instead of erroring out about Ollama.
    #[test]
    fn resolve_model_id_falls_back_to_a_configured_provider() {
        let mut config = config_with_provider_default("anthropic", "claude-x");
        // Point at a dead port with autostart off, so "no local model" holds
        // whether or not this machine has Ollama installed.
        config.ollama.host = "http://127.0.0.1".to_string();
        config.ollama.port = 1;
        config.ollama.auto_start = false;
        temp_env::with_vars([("ANTHROPIC_API_KEY", Some("sk-test"))], || {
            let runtime = tokio::runtime::Runtime::new().expect("runtime");
            let resolved = runtime
                .block_on(resolve_model_id(None, &config))
                .expect("a configured provider is enough to resolve a model");
            assert_eq!(resolved, "anthropic/claude-x");
        });
    }

    /// An installed-but-empty Ollama needs a pull, not another install.
    #[test]
    fn missing_model_error_says_pull_when_ollama_is_installed() {
        let msg = no_model_configured_error(&Config::default(), true).to_string();
        assert!(msg.contains("ollama pull qwen3:8b"), "{msg}");
        assert!(!msg.contains("https://ollama.com/download"), "{msg}");
    }
}
