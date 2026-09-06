//! Constants module to avoid magic numbers in the codebase

// Network Configuration
pub const DEFAULT_OLLAMA_PORT: u16 = 11434;

// Timeouts
pub const COMMAND_TIMEOUT_SECS: u64 = 30;
pub const COMMAND_MAX_TIMEOUT_SECS: u64 = 300;

// UI Configuration
pub const UI_MOUSE_SCROLL_LINES: u16 = 3;
/// How long a first Ctrl+C keeps exit armed: a second press inside this
/// window exits, after it the press re-arms instead. Long enough to read the
/// "press ctrl+c again to exit" hint, short enough that a stray press doesn't
/// leave the app one keystroke from quitting minutes later.
pub const UI_EXIT_CONFIRM_WINDOW_SECS: i64 = 3;

// Default Model Configuration
pub const DEFAULT_TEMPERATURE: f32 = 0.7;
/// The pre-AUTO default output cap (the config default was 4096 until the
/// model-scaled budget landed; `0` = AUTO is the default now). Kept ONLY so
/// config loading can recognize the frozen legacy value on disk and migrate it
/// to AUTO — never used to cap a request.
pub const LEGACY_DEFAULT_MAX_TOKENS: usize = 4096;
/// Meta's documented Muse Spark context and per-response output ceilings.
/// Static-but-documented like the OpenAI gpt catalog rows: Meta's `/v1/models`
/// exposes no limit metadata (Model schema is `id/object/created/owned_by`/
/// metadata — verified against the API reference 2026-07-09), so live limits
/// discovery has nothing to read. Consumed by the muse-spark catalog row
/// (context) and `MetaProvider` capabilities (both).
pub const META_MUSE_SPARK_CONTEXT_WINDOW: usize = 1_048_576;
pub const META_MUSE_SPARK_MAX_OUTPUT_TOKENS: usize = 131_072;

// Context Management
/// Auto-compact once the fully-enriched request reaches this percentage
/// of the model's known context window.
pub const COMPACTION_AUTO_THRESHOLD_PERCENT: u8 = 85;
/// Default number of recent user turns preserved verbatim after compaction.
pub const COMPACTION_TAIL_TURNS: usize = 2;
/// Maximum estimated tokens to preserve as the recent tail.
pub const COMPACTION_TAIL_TOKEN_BUDGET: usize = 8_000;
/// Maximum characters of old tool output included in the summarization prompt.
pub const COMPACTION_TOOL_OUTPUT_MAX_CHARS: usize = 2_000;
/// Maximum tokens requested from the compaction summarizer.
pub const COMPACTION_SUMMARY_MAX_TOKENS: usize = 8_000;
/// Maximum estimated input tokens sent to the summarizer.
pub const COMPACTION_SUMMARIZER_INPUT_TOKEN_BUDGET: usize = 64_000;
/// Minimum response reserve when deciding whether the next request fits.
pub const COMPACTION_MIN_RESPONSE_RESERVE_TOKENS: usize = 4_000;
/// Maximum response reserve when deciding whether the next request fits.
pub const COMPACTION_MAX_RESPONSE_RESERVE_TOKENS: usize = 20_000;
/// Default cap on consecutive auto-compact-and-continue recoveries after a
/// context-window truncation, before the run stops and shows the manual levers.
/// The counter resets whenever the run makes progress; `0` means uncapped.
pub const COMPACTION_MAX_TRUNCATION_RECOVERIES: u8 = 3;
/// Cap on consecutive auto-continuations after a response hits the provider's
/// per-response OUTPUT cap (window room to spare). Each continuation resumes
/// the reply in a fresh turn; the cap bounds a model that restarts or
/// re-truncates instead of finishing. Reset whenever a turn ends another way.
pub const MAX_OUTPUT_CONTINUATIONS: u32 = 4;

// Ollama auto-sizing
// Mermaid probes an Ollama model's real context window (`/api/show`) and sizes
// `num_ctx`/`num_predict` automatically so users never touch Ollama config. See
// `src/models/adapters/ollama_sizing.rs`.
/// Conservative `num_ctx` used when memory can't be detected (and as the auto
/// fallback). Comfortably above the compaction response reserve so auto-compaction
/// stays sane on the smaller probed window.
pub const DEFAULT_OLLAMA_MAX_AUTO_NUM_CTX: usize = 32_768;
/// Floor for the auto-fit `num_ctx` (never applied above the model's own max).
/// Equal to Ollama's own default so flooring is never worse than today.
pub const OLLAMA_MIN_AUTO_NUM_CTX: usize = 4_096;
/// Auto-fit `num_ctx` is rounded down to a multiple of this for clean values.
pub const OLLAMA_NUM_CTX_ROUNDING: usize = 1_024;
/// Bytes per KV-cache element. fp16 (2 bytes); KV-cache quantization is not
/// modeled yet (a later refinement).
pub const OLLAMA_KV_DTYPE_BYTES: usize = 2;
/// Fraction of the memory budget (VRAM, or system RAM when offload is allowed)
/// usable for model weights + KV cache; the remainder is headroom for compute
/// buffers and other processes.
pub const OLLAMA_MEMORY_BUDGET_FRACTION: f64 = 0.85;
/// Floor for `num_predict` so a small `num_ctx` can't starve the answer.
pub const OLLAMA_MIN_NUM_PREDICT: usize = 512;
/// Tokens held back from `num_ctx` when capping `num_predict`, so the prompt +
/// output estimate doesn't bump exactly against the window.
pub const OLLAMA_NUM_PREDICT_MARGIN: usize = 256;
/// Wall-clock cap for the best-effort `nvidia-smi` VRAM probe. It returns in
/// tens of ms normally; a wedged driver must not stall model dispatch.
pub const NVIDIA_SMI_TIMEOUT_SECS: u64 = 3;
/// Per-request timeout for the `/api/show` + `/api/tags` capability probe. The
/// chat client has no global timeout (streaming), so the probe sets its own so a
/// slow/hung Ollama never stalls the turn.
pub const OLLAMA_PROBE_TIMEOUT_SECS: u64 = 3;
/// How long a cached `provider_probes` row stays valid — shared by the Ollama
/// `/api/show` probe and the per-provider limits probes (`limits_probe`).
/// Model dimensions are static per id; the TTL just lets re-pulled/updated
/// models refresh.
pub const PROVIDER_PROBE_TTL_DAYS: i64 = 30;

// Web Content
/// Maximum characters to keep when truncating fetched web content
pub const WEB_CONTENT_MAX_CHARS: usize = 5_000;

/// Byte-exact aggregate cap on the complete formatted output of `web_search`.
/// Per-result content is already truncated to `WEB_CONTENT_MAX_CHARS`, so
/// at the default 5 results this caps the total at ~25 KB plus headers and
/// the sources block. The aggregate cap protects against many-results-of-
/// medium-size cases where individual truncation alone isn't enough.
pub const WEB_SEARCH_AGGREGATE_MAX_BYTES: usize = 30_000;

/// Maximum characters allowed in the streaming response buffer.
/// Prevents unbounded memory growth from runaway model responses.
pub const MAX_RESPONSE_CHARS: usize = 400_000;

/// Largest file `apply_patch` will read to apply a patch against. Deliberately
/// well above `MAX_RESPONSE_CHARS` (which caps model-visible output) so a
/// legitimately large source file stays patchable; a file past this is refused
/// rather than patched from a partial read.
pub const MAX_PATCH_FILE_BYTES: usize = 5 * 1024 * 1024;

// Tool execution limits
/// Maximum bytes of combined stdout/stderr captured from a single
/// `execute_command` invocation. Past this the capture stops and a
/// truncation marker is appended — prevents a chatty or newline-less
/// command (`cat /dev/urandom`, `yes`) from exhausting memory.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;
/// Upper bound on the number of parallel tool calls a single streaming model
/// response may accumulate. A delta whose `index` exceeds this is dropped
/// rather than used to grow an allocation proportional to an untrusted
/// integer (guards against a crafted stream OOM-ing the daemon).
pub const MAX_TOOL_CALLS: usize = 256;

// Bound-before-allocate frame caps (Cause 2)
// Every one of these guards a buffer whose size is driven by an untrusted
// peer (an MCP server, a daemon client, a model provider). They're sized
// generously — well above any legitimate payload — because their only job is
// to stop a peer that streams bytes *without* a delimiter from growing a
// buffer without bound. A legitimate large payload is delimited and so is
// never anywhere near these.
/// Max bytes in a single MCP JSON-RPC line before the frame is dropped and the
/// reader resyncs to the next newline. MCP tool results can be large (a server
/// returning a file), hence the generous ceiling.
pub const MAX_MCP_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Max bytes in a single daemon control-command line. Commands are short JSON
/// control messages; anything larger is malformed or hostile.
pub const MAX_DAEMON_COMMAND_BYTES: usize = 1024 * 1024;
/// Wall-clock ceiling on a single daemon control connection. A client that
/// opens a connection and never sends a complete command line would otherwise
/// park the handler task (and hold its fd) forever; bound it so a slow or stuck
/// peer can't leak connections toward fd exhaustion.
pub const DAEMON_CONNECTION_TIMEOUT_SECS: u64 = 30;
/// Max bytes accumulated in an SSE reassembly buffer without a complete event
/// boundary. A provider that streams bytes but never emits the `\n\n` event
/// separator would otherwise grow the buffer unbounded.
pub const MAX_SSE_BUFFER_BYTES: usize = 8 * 1024 * 1024;
/// Max bytes buffered for one streaming tool call's arguments. Tool arguments
/// (e.g. a file's contents for a `write_file`) can be large, so this is
/// generous; past it we stop appending so a crafted stream can't grow the
/// buffer without bound.
pub const MAX_TOOL_ARG_BYTES: usize = 4 * 1024 * 1024;
/// Max number of `queries[]` honored in a single `web_search` call, and of
/// `paths[]` in a single `read_file` call. Bounds the fan-out a single tool
/// call can request.
pub const MAX_BATCH_TOOL_ITEMS: usize = 32;
/// Max bytes read from an Ollama `web_search`/`web_fetch` HTTP response body
/// before the read is aborted. The body is JSON we fully buffer to parse; this
/// stops a compromised or misconfigured endpoint from returning a multi-GB
/// body that `Response::json` would buffer unbounded.
pub const MAX_WEB_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Aggregate decoded web bytes accepted across all calls in one model turn.
pub const MAX_WEB_TURN_BYTES: usize = 64 * 1024 * 1024;
/// Maximum web response downloads in flight process-wide across all backends.
pub const MAX_WEB_DOWNLOAD_CONCURRENCY: usize = 8;
/// Maximum web response downloads in flight for one transport origin.
pub const MAX_WEB_PER_ORIGIN_CONCURRENCY: usize = 2;
/// Maximum CPU-heavy extraction/snapshot jobs in flight process-wide.
pub const MAX_WEB_EXTRACTION_CONCURRENCY: usize = 2;
/// Maximum concurrent queries inside one batched `web_search` call.
pub const MAX_WEB_SEARCH_CONCURRENCY: usize = 4;

// UI Cache
/// Maximum entries in the markdown parse cache before eviction
pub const MARKDOWN_CACHE_MAX_ENTRIES: usize = 200;

// Tool-returned images
/// Maximum number of tool-returned images (MCP tools can answer with image
/// content) retained in the per-call message history sent to the model.
/// Older messages keep their text content, with a placeholder noting where
/// the image was elided, so the model knows what was dropped from context.
pub const MAX_RETAINED_TOOL_IMAGES: usize = 3;

// Project instructions (Step 5h)
/// Maximum bytes loaded from project instruction files before truncation. ~10k
/// tokens at 4 chars/token. Files larger than this likely have
/// repository-wide notes that don't all need to live in the system
/// prompt; truncate with a marker so the user knows.
pub const MAX_INSTRUCTIONS_BYTES: usize = 40_000;
/// Marker appended to project-instruction content when it exceeds the
/// byte cap. The model sees this so it knows context was elided.
pub const INSTRUCTIONS_TRUNCATION_MARKER: &str =
    "\n\n[Project instructions truncated - exceeds 10k token cap]";

// Durable semantic memory (v0.10.0)
/// Max bytes of the always-loaded memory INDEX (name + description + path per
/// fact, all scopes). ~2k tokens at 4 chars/token. The index is terse; if it
/// overflows, that's a signal to run `/consolidate-memory`.
pub const MAX_MEMORY_INDEX_BYTES: usize = 8_000;
/// Marker appended to the memory index when it exceeds the byte cap.
pub const MEMORY_INDEX_TRUNCATION_MARKER: &str =
    "\n\n[Memory index truncated - too many entries; run /consolidate-memory]";

/// Days a session scratchpad survives before the daemon's hygiene sweep
/// reclaims it. Lives here rather than in `session::scratchpad` because
/// `Config`'s default reads it, and `Config` sits below the module that does
/// the sweeping.
pub const SCRATCHPAD_RETENTION_DAYS: u64 = 7;
