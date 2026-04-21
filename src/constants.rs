//! Constants module to avoid magic numbers in the codebase

// Network Configuration
pub const DEFAULT_OLLAMA_PORT: u16 = 11434;

/// Computer-use tool names. Adapters strip these from the outgoing
/// tool list when the request is for a subagent, since subagents
/// shouldn't be wiggling the user's mouse. Single source of truth
/// so the adapter filter + the tool registry don't drift.
pub const GUI_TOOL_NAMES: &[&str] = &[
    "screenshot",
    "list_windows",
    "click",
    "type_text",
    "press_key",
    "scroll",
    "mouse_move",
];

// Timeouts
pub const COMMAND_TIMEOUT_SECS: u64 = 30;
pub const COMMAND_MAX_TIMEOUT_SECS: u64 = 300;

// UI Configuration
pub const UI_POLL_INTERVAL_MS: u64 = 50;
pub const UI_MOUSE_SCROLL_LINES: u16 = 3;
pub const UI_ERROR_LOG_MAX_SIZE: usize = 50;

// Default Model Configuration
pub const DEFAULT_TEMPERATURE: f32 = 0.7;
pub const DEFAULT_MAX_TOKENS: usize = 4096;

// Context Management
/// Maximum context tokens for managed message history
pub const MAX_CONTEXT_TOKENS: usize = 75_000;
/// Tokens reserved for the model's response within the context window
pub const CONTEXT_RESERVE_TOKENS: usize = 4_000;

// Web Content
/// Maximum characters to keep when truncating fetched web content
pub const WEB_CONTENT_MAX_CHARS: usize = 5_000;

/// Aggregate cap on the formatted output of `execute_web_searches`.
/// Per-result content is already truncated to `WEB_CONTENT_MAX_CHARS`, so
/// at the default 5 results this caps the total at ~25 KB plus headers and
/// the sources block. The aggregate cap protects against many-results-of-
/// medium-size cases where individual truncation alone isn't enough.
pub const WEB_SEARCH_AGGREGATE_MAX_CHARS: usize = 30_000;

/// Maximum characters allowed in the streaming response buffer.
/// Prevents unbounded memory growth from runaway model responses.
pub const MAX_RESPONSE_CHARS: usize = 400_000;

// UI Cache
/// Maximum entries in the markdown parse cache before eviction
pub const MARKDOWN_CACHE_MAX_ENTRIES: usize = 200;

// Subagent Configuration
/// Maximum number of concurrent subagents that can be spawned by a single parent call
pub const MAX_CONCURRENT_AGENTS: usize = 10;

// Computer Use
/// Maximum width for screenshots sent to models (pixels)
pub const SCREENSHOT_MAX_WIDTH: u32 = 1280;

// Computer-use timing — empirically tuned for typical desktop response.
// Slow systems (high-load WMs, remote X displays) may need higher values;
// the right place to make these tunable later is via env vars on
// `app::Config`, alongside the rest of the configurable surface.
/// Delay after `xdotool windowactivate --sync` so the window manager has
/// time to actually move focus before the next action.
pub const WINDOW_FOCUS_DELAY_MS: u64 = 200;
/// Delay after a click for the window manager to process focus + UI update,
/// before we take the auto-screenshot the model uses to decide its next move.
pub const POST_CLICK_DELAY_MS: u64 = 500;
/// Delay after typing for the target application to settle (validate input,
/// re-render) before the auto-screenshot.
pub const POST_TYPE_DELAY_MS: u64 = 500;
/// Delay after a key press / shortcut for the target app to react before
/// the auto-screenshot.
pub const POST_KEY_DELAY_MS: u64 = 500;
/// Delay between simulated keystrokes when typing text. The previous
/// 12ms default lost characters on slow Electron / web targets; 25ms
/// is a safer default that still types ~40 chars/sec.
pub const TYPE_KEY_DELAY_MS: u64 = 25;
/// Maximum scroll wheel ticks in a single `scroll` call. Clamps the
/// model's `amount` parameter so a runaway model can't request a
/// million ticks (would exceed `ARG_MAX` building xdotool argv).
pub const MAX_SCROLL_AMOUNT: i32 = 100;
/// Capacity of the per-screenshot metadata ring buffer in
/// `computer_use`. Each call to `screenshot` registers metadata under
/// a new id; older entries are LRU-evicted past this cap. Sized to
/// cover any realistic agentic loop without unbounded growth.
pub const SCREENSHOT_REGISTRY_CAPACITY: usize = 16;
/// Maximum number of screenshot images retained in the per-call
/// message history sent to the model. Older messages keep their text
/// content (with a placeholder noting where the image was elided) so
/// the model knows what was dropped from context.
pub const MAX_RETAINED_SCREENSHOTS: usize = 3;

// MERMAID.md project instructions (Step 5h)
/// Maximum bytes loaded from MERMAID.md before truncation. ~10k
/// tokens at 4 chars/token. Files larger than this likely have
/// repository-wide notes that don't all need to live in the system
/// prompt; truncate with a marker so the user knows.
pub const MAX_INSTRUCTIONS_BYTES: usize = 40_000;
/// Marker appended to MERMAID.md content when the file exceeds the
/// byte cap. The model sees this so it knows context was elided.
pub const INSTRUCTIONS_TRUNCATION_MARKER: &str =
    "\n\n[MERMAID.md truncated — exceeds 10k token cap]";
