# Changelog

All notable changes to Mermaid CLI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.1] - 2026-04-26

Runtime hardening, typed tool output, token accounting, and context
compaction on top of the v0.7 architecture.

### Added

- **Manual and automatic context compaction.** `/compact [instructions]`
  now creates a model-visible checkpoint, archives the removed raw
  messages under `.mermaid/compactions/`, and replaces old history
  with a structured handoff plus the most recent turns. Mermaid also
  auto-compacts near the model's context limit and retries once after
  provider context-limit errors.
- **Typed tool-result metadata.** Tool outcomes now carry structured
  status, duration, line counts, byte counts, result counts, artifacts,
  and tool-specific metadata. The TUI renders friendly summaries such
  as read/write line counts, web-search result counts, command exit
  status, and background process details without scraping model-facing
  text.
- **Runtime metadata layer.** `domain::runtime` tracks lifecycle
  signals, provider capability snapshots, managed background
  processes, tool metadata, and a lightweight runtime timeline.
- **Background command registration.** `execute_command` can run
  long-lived commands in background mode, capture startup logs, detect
  local URLs, and register PID/log metadata for Mermaid to display and
  persist.
- **Subagent tool.** The `agent` tool can spawn autonomous child agents
  using the active model/provider, with depth limits and a child tool
  registry that excludes unsafe/self-recursive tools.
- **Computer-use tools.** The v0.7 tool registry now includes the
  screenshot, click, mouse-move, keypress, type-text, scroll, and
  window-list computer-use tools.
- **Chat image artifacts.** Tool-produced images can be attached to
  assistant messages, rendered in the chat history, and opened from the
  TUI.
- **Lifecycle signal handling.** SIGINT, SIGTERM, and SIGHUP now flow
  through reducer messages so Mermaid can restore the terminal and save
  state consistently.
- **Context and usage slash commands.** `/usage` and `/context` report
  provider token usage, session totals, estimated prompt budget, model
  context capacity, and recent compaction metadata.

### Changed

- **v0.6 runtime deleted.** The `MERMAID_V7=1` opt-in is gone; the
  v0.7 architecture is now the only code path. `src/tui/`,
  `src/runtime/`, `src/agents/`, `src/models/backend.rs`, and
  `src/models/retry.rs` are all removed. Net ~8,000 LOC of old code
  gone from the tree.
- Non-interactive `mermaid run <prompt>` now runs on the v0.7 reducer
  + effect runner (same as interactive); output shape matches the
  v0.6 `NonInteractiveResult` so scripts keep working.
- Slash commands, diff helpers, action value types, MCP manager
  accessor, and the web search client all moved out of the v0.6
  namespace into `src/domain/`, `src/render/`, `src/mcp/`, and
  `src/providers/tool/`. No behaviour changes — just no longer
  reaching back into deleted modules.
- Token accounting now distinguishes provider-reported usage from
  local estimates. The footer shows current context usage separately
  from last API usage and cumulative session totals, avoiding the old
  inflated "session tokens" display.
- Model/provider requests now use a stream bridge shared across
  providers, making cancellation and done/usage events more uniform.
- Terminal teardown now restores raw mode, mouse capture, bracketed
  paste, and the alternate screen before asynchronous shutdown work
  drains.

### Fixed

- Ctrl+C from an idle, empty TUI exits and restores the user's terminal
  reliably instead of requiring repeated keypresses or leaking terminal
  escape sequences back into the shell.
- Cancelled turns now drain through `TurnCancelled`, preventing stale
  provider/tool events from leaving the reducer stuck in `Cancelling`.
- Tool cancellation now returns typed cancelled outcomes instead of
  relying on textual placeholders.
- Stale screenshots are evicted from outgoing model requests while the
  latest relevant image remains available in chat history.
- Gemini API key resolution now documents and preserves the
  `GEMINI_API_KEY` legacy fallback alongside `GOOGLE_API_KEY`.

### Removed

- Two integration test files that exercised the v0.6 runtime
  (`tests/agent_loop_tests.rs`, `tests/tui_behavior_tests.rs`). The
  reducer + effect parity suites (`tests/reducer_flows.rs`,
  `tests/effect_cancel.rs`) cover the equivalents.

### Added (free, via the new architecture)

- MCP servers initialize automatically at startup via
  `Cmd::InitMcpServers`. v0.6 only init'd in the interactive path;
  non-interactive invocations now get MCP tools too.
- `manager_ref::wait_ready()` — if a tool call races startup, it
  parks briefly for init to complete instead of immediately
  erroring.
- `--record <file>` now records structured reducer input events,
  including lifecycle and compaction events, for replay/debugging.

### Docs

- Updated the architecture, adding-tools, replay-debugging, and README
  docs for the v0.7-only runtime, typed metadata, background commands,
  computer-use path changes, and current provider key behavior.

### Tests

- Added regression coverage for terminal-mode restoration on Ctrl+C,
  context compaction planning/replacement, slash-command parsing,
  compact event rendering, token-status rendering, background command
  metadata, and subagent/tool registry behavior.

## [0.7.0] - 2026-04-21

The Architecture Release. This is a big-bang rewrite of Mermaid's
runtime on the Elm/MVU pattern: one pure reducer, effects as data,
structured concurrency per turn. External behaviour is intended to
match v0.6; several whole classes of bug that v0.6 let slip become
impossible to express against the new types.

The new path ships behind `MERMAID_V7=1` for the v0.7.0 release so
the v0.6 runtime keeps running by default during the migration
window. Flipping the default happens in a follow-up once the v7
path has been exercised against real sessions.

### Added

- **Pure reducer** (`src/domain/reducer.rs`) — `fn update(State, Msg)
  -> (State, Vec<Cmd>)`. Synchronous. Stale events filter by embedded
  `TurnId` before any state transition. Tool-result completeness is
  type-enforced (`Vec<Option<ToolOutcome>>` can't advance to the
  follow-up call until every slot is `Some(_)`). Exhaustive match on
  `Msg`; clippy catches any missing variant.
- **Effect runner** (`src/effect/`) — the single place in the
  codebase where tokio tasks spawn. Owns per-turn `TurnScope`
  (`CancellationToken` + `JoinSet`) so cancellation is a signal, not
  a poll. Retry/tracing middleware (from v0.6's
  `src/models/retry.rs`) now wraps any adapter uniformly.
- **`ModelProvider` + `ToolExecutor` traits** (`src/providers/`) —
  the adapter surface. Four model providers (Ollama, Anthropic,
  Gemini, OpenAI-compat) + six built-in tools (read_file,
  write_file, edit_file, delete_file, create_directory,
  execute_command) all implement these. MCP dispatch lives at
  `tool::McpToolProxy`.
- **`StreamContext` + `ExecContext`** — typed per-call contexts
  carrying the turn's cancellation token. Providers and tools that
  ignore the token don't get past code review; the type signature
  makes the race explicit.
- **Pure view function** (`src/render/`) — `fn render(&State, &mut
  Frame)`. Never mutates state, never performs I/O, never holds a
  `&mut App`. Testable against ratatui's `TestBackend` without a
  runtime or terminal.
- **Single event loop** (`src/app/run.rs`) — one `tokio::select!`
  over crossterm `EventStream` + effect-result mpsc + tick timer.
  Replaces v0.6's two competing event loops. Behind
  `MERMAID_V7=1`.
- **`TerminalGuard`** — raw-mode/alt-screen setup with panic-safe
  teardown. A panic mid-render restores the shell.
- **Recorder / Replay** (`src/app/recorder.rs`) — JSONL msg logs.
  Reducer is event-sourced by design, so record is one line per
  reducer input and replay is a fold. Regression tests as flat
  files; bug reports as replay logs.

### Changed

- `ExecuteCommandTool` now races subprocess wait against the turn's
  cancellation token. Ctrl+C during a long-running build aborts
  within microseconds (plus SIGKILL travel) instead of waiting for
  the 300-second timeout. Structural fix for the v0.6 "20-press
  Ctrl+C" report — tokens can't be forgotten; they're in the type.
- Retry middleware moves from `src/models/retry.rs` (deleted in
  follow-up) to `src/effect/middleware.rs`. Behaviour identical:
  3 attempts, 500ms→3s exponential backoff, retry on 5xx / 429 /
  `ConnectionFailed`.

### Docs

- `docs/architecture.md` — full tour of the new design + invariants.
- `docs/adding_tools.md` — one-file tool recipe.
- `docs/adding_providers.md` — adapter recipe.
- `docs/replay_debugging.md` — record/replay usage.

### Tests

- 558 tests pass: 516 library, 42 integration.
- `tests/reducer_flows.rs` — 15 multi-message flow tests (stale
  events, tool-outcome completeness, cancel, quit, slash commands).
- `tests/effect_cancel.rs` — 5 real-tokio tests (Ctrl+C aborts a
  `sleep 60` within 300ms; bounded shutdown).
- Ratatui `TestBackend` renderer tests (5).

### Not yet in v0.7.0

- Default binary path still runs v0.6 runtime. Flip happens in a
  follow-up release once v7 parity is verified against real
  sessions.
- Subagent dispatch, MCP startup, and several modals (conversation
  load, /cloud-setup, model list) still route through v0.6 code.
  Reducer has the `Msg` vocabulary; implementations mechanical.

## [0.6.0] - 2026-04-16

Major release: multi-provider adapter support. Mermaid is no longer
Ollama-only — direct integrations for Anthropic Claude, Google Gemini, and
the full OpenAI-compatible long tail (OpenAI, Groq, OpenRouter, Cerebras,
DeepInfra, Together). Plus a new slash-command palette, auto-loaded
MERMAID.md project instructions, MCP spec bump, and a security update.

### Added

- **Anthropic adapter** (`src/models/adapters/anthropic.rs`) — bespoke
  Messages API support: `2023-06-01` version pin, adaptive + legacy
  thinking formats dispatched per model, typed SSE streaming, `thinking`
  signature round-trip for multi-turn extended thinking, `cache_control:
  ephemeral` on system prompts + last tool for prompt caching, vision
  (base64 images), tool translation to Anthropic's flat `{type: "custom"}`
  shape. Supports Claude Opus 4.7 (`xhigh` effort tier), Sonnet 4.6,
  Opus 4.6, Sonnet 4.5, Opus 4.5, Haiku 4.5.
- **Gemini adapter** (`src/models/adapters/gemini.rs`) — per-method
  endpoints (`:generateContent` / `:streamGenerateContent?alt=sse`),
  `user`/`model` role convention, `functionResponse` merge for tool
  results, per-model thinking dispatch (Gemini 3 `thinkingLevel` enum,
  Gemini 2.5 Pro/Flash/Flash-Lite `thinkingBudget` with correct floors,
  2.0 omits `thinkingConfig`), `thought: true` reasoning parts, inline
  base64 images for vision. Curated list: `gemini-pro-latest`,
  `gemini-flash-latest`, `gemini-3.1-pro-preview`, `gemini-3-flash-preview`,
  `gemini-3.1-flash-lite-preview`, `gemini-2.5-pro/flash/flash-lite`.
- **OpenAI-compatible adapter** (`src/models/adapters/openai_compat.rs`)
  — single `/chat/completions` adapter with per-provider quirks encoded
  in `ProviderProfile`. Built-in registry: OpenAI, Groq, OpenRouter,
  Cerebras, DeepInfra, Together. Three reasoning strategies (`Effort`,
  `OpenRouterShape`, `None`) and three extraction strategies
  (`DeltaContentField`, `InlineThinkTags`, `None`). Streaming tool-call
  accumulator handles OpenAI's chunked `delta.tool_calls` pattern.
  OpenRouter `X-OpenRouter-Title` canonical header.
- **Custom OpenAI-compatible providers** — users can add any
  `/chat/completions` endpoint via `[providers.<name>]` in `config.toml`
  with `base_url`, `api_key_env`, and `compat = "openai" |
  "openai-effort" | "openrouter"`.
- **`ReasoningLevel` enum** (`src/models/reasoning.rs`) — seven tiers
  (`None`, `Minimal`, `Low`, `Medium`, `High`, `XHigh`, `Max`) with rank
  ordering; `XHigh` sits between `High` and `Max`. `nearest_effort()`
  snaps user choice onto the model's advertised `ReasoningCapability`.
  Per-model persistence via `[reasoning_per_model]` in config.
- **`--reasoning <level>` CLI flag** overrides config-default for this
  session.
- **Typed streaming** (`src/models/stream.rs`) — `StreamEvent` enum
  (`Text`, `Reasoning`, `ToolCall`, `Done`) replaces the legacy text-only
  callback. Adapters emit typed events; consumers route them without
  marker-sniffing.
- **`ModelCapabilities`** (`src/models/capabilities.rs`) — per-model
  `supports_tools`/`supports_vision`/`supports_reasoning`/`max_context_tokens`
  advertised by each adapter.
- **MERMAID.md project instructions** (`src/app/instructions.rs`) —
  walks UP from cwd to the git root or `$HOME`, loads the nearest
  `MERMAID.md`, auto-reloads on mtime change before every model call
  (one stat per turn). 10k-token cap with truncation marker. Injected
  via `ModelConfig::dynamic_system_suffix`; Anthropic gets a separate
  cache block (static base stays warm across project switches).
- **Slash-command palette** (`src/tui/widgets/slash_palette.rs`,
  `src/tui/slash_commands.rs`) — type `/` to open a filter-as-you-type
  list of all commands. Up/Down navigates, Tab completes, Enter
  dispatches, Esc dismisses. Centralized `COMMAND_REGISTRY` so `/help`
  auto-updates with new commands.
- **`/reasoning <level>` slash command** — per-model persisted reasoning
  depth. Alt+T cycles `None → Low → Medium → High → Max → None`
  (Minimal + XHigh reachable only via `/reasoning`, treated as
  specialist tiers).
- **MCP 2025-11-25 protocol** — bumped from 2025-03-26. New content
  block types: `audio`, `resource_link`, `resource` (embedded).
  Audio flows through the image attachment channel; resource links
  render as text so the model can follow up with another tool call.
- **`postgres-mcp` (uvx)** replacing deprecated
  `@modelcontextprotocol/server-postgres` — crystaldba community
  maintainer. Env var renamed `DATABASE_URL` → `DATABASE_URI`.
- **`@zencoderai/slack-mcp-server`** replacing deprecated
  `@modelcontextprotocol/server-slack` — Zencoder is the official
  handoff maintainer.
- **`@brave/brave-search-mcp-server`** replacing deprecated
  `@modelcontextprotocol/server-brave-search` — Brave is now the
  first-party maintainer.
- **Graceful MCP shutdown** (`src/mcp/transport.rs`) — close stdin →
  2s wait → SIGTERM → 1s wait → SIGKILL. Replaces the previous
  straight-to-SIGKILL path.
- **UTF-8-safe byte-buffer draining** (`src/utils/ndjson.rs`,
  `src/utils/sse.rs`) — NDJSON line splitter and SSE event splitter
  that buffer raw bytes and decode only complete frames. Protects
  against TCP-chunk-inside-codepoint corruption on both Ollama NDJSON
  and OpenAI-compat SSE streams.
- **API-key resolution** (`src/utils/auth.rs`) — uniform env-var lookup
  with optional `[providers.<name>].api_key_env` override.

### Changed

- **`/` slash-command prefix** replaces the legacy `:` colon prefix.
  All commands now live under `/`; the palette only opens for `/`.
- **Tokio bumped to 1.44** (resolves to 1.49 via caret) — closes
  RUSTSEC-2025-0023 (broadcast channel unsoundness).
- **toml 0.9 → 1.1** (major version bump), clap 4.5 → 4.6, bytes 1.8 →
  1.11, regex 1.11 → 1.12.
- **Removed `dotenvy`** — unused dead dependency.
- **Added `temp-env`** dev-dep — replaces `unsafe { env::set_var /
  remove_var }` in `backend.rs` + `auth.rs` tests; safer under
  `--test-threads > 1`.
- **MSRV pinned to 1.91** in `Cargo.toml rust-version`, `.clippy.toml`,
  and `flake.nix`. `str::floor_char_boundary` (used for UTF-8-safe
  slicing) stabilized in Rust 1.91.
- **Ollama `gpt-oss` dispatch** — sends `think: "low"|"medium"|"high"`
  (string enum) instead of the bool other Ollama models expect. Advertised
  as `Levels([None, Low, Medium, High])` in capabilities so XHigh/Max
  snap correctly via `nearest_effort`.
- **OpenRouter header** — `X-Title` → `X-OpenRouter-Title` (the new
  canonical name; old still accepted for backward compat).

### Fixed

- Anthropic `Max` effort now gated per-model — Sonnet 4.5 / Opus 4.5 /
  Haiku 4.5 snap to `"high"` since they don't accept `"max"` per the
  2026-04 effort documentation.
- Gemini 3 model IDs refreshed — `gemini-3-pro` (shut down 2026-03),
  `gemini-3-flash`, `gemini-3-flash-lite` replaced with the current
  `-preview` variants and `-latest` aliases.
- MCP registry entries for `slack`, `postgres`, and `brave-search` swapped
  to current maintained alternatives (originals deprecated upstream).

### Removed

- `src/tui/stream_handler.rs` — replaced by the typed `StreamEvent` path
  and `TuiObserver` in `loop_coordinator.rs`.
- `.env.example` — `dotenvy` dependency removed, no `.env` loading path.

## [0.5.1] - 2026-04-12

### Added
- MCP (Model Context Protocol) client integration for connecting to external tool servers
- `mermaid add <name>` / `mermaid remove <name>` / `mermaid mcp` commands for MCP server management
- Built-in registry of 17 popular MCP servers (context7, github, playwright, memory, postgres, etc.)
- Enhanced computer use: window-aware screenshots (`mode: "window"`), `list_windows` tool, auto-screenshot after click/type/key actions
- 42 new tests across agent loop, session persistence, non-interactive mode, and stream handler

### Changed
- MCP servers now initialize in background (TUI renders immediately instead of blocking startup)
- MCP tools become available to the model as soon as servers are ready, even mid-agent-loop
- Centralized model configuration into `ModelConfig::from_app_config()` (internal refactor)
- Nix flake: switched from nightly Rust to stable 1.87.0, removed OpenSSL 1.1 dependency

### Fixed
- Ollama URL normalization with paths (`http://host/v1` no longer appends port after the path)
- Token tracking now counts total tokens (prompt + completion) instead of completion-only
- `ActionResult` images field properly propagated for screenshot tool results
- Command timeout treated as success (process continues running in background)

### Removed
- Sync `list_models()` from Ollama detector (replaced by async-only `list_models_async()`)
- Unused fields from MCP `ResolvedServer` struct

## [0.5.0] - 2026-03-15

### Added
- Ollama Cloud setup flow (`:cloud-setup` command and interactive API key configuration)
- Claude Code-style subagents for parallel task execution (`agent` tool)
- TUI stream event system (typed `StreamEvent` enum replacing string-based protocol)
- Image paste status widgets and attachment management UI
- Web search and web fetch via Ollama Cloud API (`web_search`, `web_fetch` tools)

### Changed
- Consolidated ModelFactory into `backend.rs`, removed `factory.rs`
- Architectural cleanup across TUI state management (split App into focused state modules)
- Consolidated git tools into bash `execute_command`, removed `git2` dependency
- Removed vestigial LiteLLM infrastructure

## [0.4.1] - 2026-02-10

### Fixed
- Full codebase review: 53 clippy warnings fixed
- Security and correctness improvements across all modules

## [0.4.0] - 2026-02-08

### Changed
- Codebase review, dead code removal, and architectural cleanup

### Fixed
- Gate crates.io publish behind `PUBLISH_TO_CRATES_IO` environment variable

## [0.3.0] - 2026-01-20

### Added
- Proper agent loop for native Ollama tool calling (replaces text-based action blocks)
- Thinking mode toggle with Alt+T (for models that support extended reasoning)
- Message queuing — type while model generates, messages send in order
- Session persistence — start fresh by default, `--continue` to resume last conversation
- `--sessions` flag to pick a previous conversation to resume
- Model persistence — last-used model saved to config
- Image paste support with vision model integration (Ctrl+V)
- `edit_file` tool for targeted text replacement with diff display
- Web search via Ollama Cloud API (replaced SearXNG)
- `web_fetch` tool for fetching URL content as markdown
- Bracketed paste support for multi-line input
- Markdown table rendering in chat
- Auto-pull models from Ollama when not found locally
- Non-interactive mode: `mermaid run "prompt"` with JSON/text/markdown output
- `delete_file` and `create_directory` tools
- Computer use tools: `screenshot`, `click`, `type_text`, `press_key`, `scroll`, `mouse_move`
- File-based logging (writes to `~/.mermaid/mermaid.log` instead of corrupting TUI)
- Esc/Ctrl+C to interrupt queued message processing

### Changed
- Simplified to Ollama-only backend (removed vLLM, proxy, model router)
- Overhauled system prompt for tool-calling models
- Upgraded to Rust 2024 edition
- Upgraded ratatui from 0.29 to 0.30 with new idioms
- Rewrote README for accuracy and simplicity
- Unified tool calling to native Ollama API format
- Used Ollama's real token counts (removed tiktoken dependency)

### Fixed
- Char-boundary safe string slicing throughout (prevents UTF-8 panics)
- False positive in dangerous command detection (`.mermaid` matching `rm`)
- Cursor position drifts on wrapped input lines
- Cursor jumps to column 1 when typing space
- Timestamp overlapping long user messages
- Streaming timeout for large operations (removed global HTTP timeout)
- Windows duplicate input from key release events

## [0.2.1] - 2025-12-18

### Added
- NixOS support via `flake.nix` and `flake.lock`
- `delete_file` and `create_directory` tools for agent
- `.vs/` to `.gitignore`

### Changed
- Organized project root directory structure:
  - Moved scripts to `scripts/`
  - Moved infrastructure files to `infra/`
- Updated README to accurately reflect available tools (renamed `run_command` to `execute_command`)

### Fixed
- CLI argument conflict: `prompt` now uses `-P` short flag, `path` retains `-p`
- `resume` argument `conflicts_with` validation logic
- Compilation errors due to relative paths in `include_str!` macros

## [0.2.0] - 2025-11-16

### Added
- Native Ollama tool calling support with JSON Schema tool definitions
  - 9 tools: read_file, write_file, run_command, git_status, git_diff, git_commit, web_search, list_directory, get_file_info
  - Structured tool definitions with detailed parameter descriptions
  - Tool calls parsed from streaming chunks in real-time
- Enhanced input widget UI matching Claude Code aesthetics
  - Always-visible "> " prompt prefix
  - Full-width input bar with top/bottom borders only
  - Proper text wrapping with 2-space indentation on continuation lines
  - Blank line after "Thinking..." marker for better visual spacing
- Model compatibility framework for tool calling detection

### Changed
- Migrated from text-based action blocks to Ollama native tool calling API
- Completely rewrote system prompt (76% reduction: 353 to 86 lines)
- Tool definitions now provide comprehensive usage guidance
- Cleaner, more maintainable architecture with dedicated tools module
- Updated all backend adapters to support tool_calls in responses
- Stream handler now accumulates tool calls from streaming chunks

### Removed
- Legacy text-based parsers (parser.rs, extractor.rs, segmenter.rs)
- Verbose system prompt with action block examples
- Text-based action block parsing (temporarily, will be restored as fallback)

### Fixed
- Cursor positioning now accounts for "> " prefix and border changes
- Text wrapping alignment issues with continuation lines
- Input widget rendering for empty input states

### Breaking Changes
- Models without native Ollama tool calling support will not execute actions
- Next release (v0.2.1) will restore text-based fallback for universal compatibility
- Compatible models: llama3.1, llama3.2, qwen2.5-coder, mistral-nemo, firefunction-v2

## [0.1.1] - 2025-09-27

### Added
- Test helper functions for better test coverage
  - `path_exists` function in filesystem module for path validation
  - `current_branch` function in git module for branch detection

### Fixed
- Test compilation errors in filesystem and git modules
- Clippy configuration to allow reasonable nesting depth

### Changed
- Adjusted CI/CD workflow clippy strictness to warnings level

## [0.1.0] - 2025-09-27

### Added
- Initial release of Mermaid CLI
- Model-agnostic AI pair programmer with support for 100+ LLM providers via LiteLLM proxy
- Terminal User Interface (TUI) built with Ratatui
  - Real-time streaming responses
  - Syntax highlighting for code
  - Project sidebar with file tree
  - Markdown rendering support
- Agentic capabilities
  - File operations (read, write, create, delete)
  - Git integration (diff, status, commit)
  - Shell command execution
  - Project context awareness
- Configuration system
  - Global config at ~/.config/mermaid/config.toml
  - Project-specific config support
  - Environment variable configuration
- LiteLLM proxy integration
  - Support for OpenAI, Anthropic, Google, Ollama, and 90+ more providers
  - Unified API interface
  - Docker/Podman containerization
- Project context loading
  - Automatic project structure analysis
  - Token counting and management
  - Respects .gitignore patterns
- GitHub Actions CI/CD workflows
  - Automated testing and linting
  - Multi-platform release builds (Linux, macOS, Windows)
  - Security vulnerability scanning
  - Code formatting enforcement
- Dual licensing (MIT OR Apache-2.0)

### Infrastructure
- Rust 2021 edition
- Comprehensive test suite
- rustfmt and clippy configuration
- Docker compose setup for LiteLLM proxy

[Unreleased]: https://github.com/noahsabaj/mermaid-cli/compare/v0.7.1...HEAD
[0.7.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/noahsabaj/mermaid-cli/releases/tag/v0.1.0
