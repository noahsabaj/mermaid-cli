//! The configuration value.
//!
//! Pure serde types and their defaults. Loading, layering, merging and
//! persisting them stays in `src/app/config.rs`.
//!
//! `State.settings: Config` is correct MVU, not a violation — the reducer
//! MUTATES it (`/plan config` edits `settings.plan` live and emits
//! `Cmd::PersistPlanConfig`), and `SessionHeader.config` is the `--replay`
//! seed. The bug was only that the type was DEFINED in the application layer,
//! which made `domain -> app` a cycle: `CompactionConfig::policy()` returns a
//! `domain::CompactionPolicy`. Defining `Config` here dissolves that edge by
//! construction rather than inverting it.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use mermaid_model::constants::{DEFAULT_OLLAMA_PORT, DEFAULT_TEMPERATURE};
use mermaid_model::models::ReasoningLevel;
use mermaid_runtime::{PolicyOverride, SafetyMode};

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
            Self::Dark => "dark",
            Self::Light => "light",
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
    /// even `full_access` routes MCP writes through the intent classifier
    /// (aligned runs silently, off-task escalates). `allow` restores the old
    /// unconditional-allow behavior.
    #[serde(default)]
    pub external_writes: mermaid_runtime::FloorLevel,
    /// Enforcement floor for machine-scoped package operations (`npm -g`,
    /// `cargo install`, `pip install`, `brew`/`apt`/`winget` installs) —
    /// same levels and default as `external_writes`. They mutate the
    /// MACHINE, not the project (outside checkpoint reach), so even
    /// `full_access` vets them. Project-local installs (`npm install`,
    /// `cargo add`) are untouched.
    #[serde(default)]
    pub system_installs: mermaid_runtime::FloorLevel,
    /// Model id the `Auto`-mode safety classifier uses to vet borderline
    /// actions. `None` ⇒ vet with the session's active model. Set this to
    /// point the vet at a cheaper/faster model than the one driving the work.
    #[serde(default)]
    pub auto_classifier_model: Option<String>,
    /// Headless escape hatch: when true, non-replayable tools (web/mcp/
    /// `subagent/computer_use`) are allowed to PROCEED on an `Ask` decision in a
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
            external_writes: mermaid_runtime::FloorLevel::default(),
            system_installs: mermaid_runtime::FloorLevel::default(),
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
            scratchpad_retention_days: mermaid_model::constants::SCRATCHPAD_RETENTION_DAYS as i64,
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
            Self::Allow => "allow",
            Self::Auto => "auto",
            Self::Ask => "ask",
            Self::Deny => "deny",
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
    /// `skip_serializing` keeps "unset" meaningful in saved configs (the
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
    pub reasoning: Option<mermaid_model::models::ReasoningLevel>,
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
            index_cap_bytes: mermaid_model::constants::MAX_MEMORY_INDEX_BYTES,
        }
    }
}

/// Context-compaction settings.
///
/// Every field maps onto a [`crate::CompactionPolicy`] knob that was
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
        let policy = crate::CompactionPolicy::default();
        Self {
            max_truncation_recoveries:
                mermaid_model::constants::COMPACTION_MAX_TRUNCATION_RECOVERIES,
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
    pub fn policy(&self) -> crate::CompactionPolicy {
        let defaults = crate::CompactionPolicy::default();
        let min_reserve = self.min_response_reserve_tokens;
        let max_reserve = self.max_response_reserve_tokens;
        crate::CompactionPolicy {
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
    /// After a successful click / `type_text` / `press_key`, auto-capture the
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
                let parsed = url::Url::parse(url)
                    .map_err(|e| anyhow::anyhow!("invalid MCP server url '{url}': {e}"))?;
                let host = parsed.host_str().unwrap_or("");
                match parsed.scheme() {
                    "https" => Ok(TransportKind::Http),
                    "http" if mermaid_model::utils::classify_host(host).is_loopback() => {
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
                    .map(|a| mermaid_model::utils::redact_secrets(a))
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
    pub fn name(self) -> &'static str {
        match self {
            Self::Defaults => "defaults",
            Self::User => "user config",
            Self::Profile => "config profile",
            Self::Project => "project config",
            Self::Session => "session flags",
        }
    }
}

/// One layer's raw table plus where it came from (for warning attribution).
#[derive(Debug, Clone)]
pub struct LayerSource {
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
