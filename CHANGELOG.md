# Changelog

All notable changes to Mermaid CLI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/noahsabaj/mermaid-cli/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/noahsabaj/mermaid-cli/releases/tag/v0.1.0