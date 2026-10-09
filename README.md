# Mermaid

An open-source AI coding assistant for the terminal. Multi-provider — Ollama (local), Anthropic, Gemini, Meta, OpenAI, Groq, OpenRouter, and any OpenAI-compatible endpoint — with native tool calling, subagents, and a clean TUI.

## Features

- **Native tool calling** — read, write, edit, delete, run commands, search the web, spawn subagents, call MCP tools
- **Subagents** — spawn parallel autonomous agents; built-in `general` and read-only `explore` types, per-call model override, continuation handles
- **Worktree isolation** — give a writing subagent its own git checkout, seeded with your uncommitted state. Its changes land as one patch, serialized against other children, so parallel writers report a conflict instead of interleaving
- **Safety modes** — `read_only`/`ask`/`auto`/`full_access`, cycled live with Shift+Tab; `auto` is classifier-backed, and gated actions prompt inline rather than erroring out
- **Checkpoints** — shadow-git snapshots before mutations; inspect with `/checkpoints`, roll back with `/restore <id>`
- **Durable memory** — the agent remembers facts across sessions; a compact index auto-loads into every prompt
- **Project instructions and skills** — auto-loads `AGENTS.md` (or `CLAUDE.md`) and `MERMAID.md`, plus task-specific playbooks loaded on demand
- **MCP servers** — stdio JSON-RPC client with a built-in registry of popular servers
- **Sessions** — conversations auto-save; `--continue` reopens the last one here, `--resume` opens a picker, double-Esc forks the timeline at an earlier message
- **Context compaction** — automatic checkpoint-and-continue when the window fills; manual `/compact [focus]`
- **Image paste** — Ctrl+V attaches images for vision models on X11, Wayland, macOS, and Windows
- **Reasoning levels** — cycled with Alt+T, persisted per model
- **Record and replay** — `--record` captures every reducer input; `--replay` reconstructs the session offline, deterministically
- **Non-interactive mode** — script with `mermaid run "prompt"` for CI and automation

## Install

No Rust or cargo required — the installer downloads a prebuilt binary for your platform from the latest [GitHub Release](https://github.com/noahsabaj/mermaid-cli/releases), verifies its checksum, and puts `mermaid` on your PATH.

**macOS / Linux**

```bash
curl -fsSL https://noahsabaj.github.io/mermaid-cli/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://noahsabaj.github.io/mermaid-cli/install.ps1 | iex
```

Run `mermaid` to start, `mermaid update` for the newest version. (`MERMAID_INSTALL_DIR` changes the location; `MERMAID_VERSION=vX.Y.Z` pins a release.)

**Or install with a package manager**

```bash
# Homebrew (macOS / Linux)
brew install noahsabaj/mermaid/mermaid

# Scoop (Windows)
scoop bucket add mermaid https://github.com/noahsabaj/scoop-mermaid
scoop install mermaid
```

```powershell
# WinGet (Windows) — pending review on the official winget-pkgs repo
winget install NoahSabaj.Mermaid
```

All three are bumped on every release.

With the Rust toolchain, `cargo install mermaid-cli` works too, though crates.io can lag the newest tag. Every release also attaches prebuilt binaries and Linux `.deb`/`.rpm` packages.

Mermaid needs one model backend, either kind. [Ollama](https://ollama.com) covers local inference (models auto-pull) but is **not** required — a provider API key alone is enough, see [Remote providers](#remote-providers). Name a remote model once with `mermaid --model anthropic/<model>` and Mermaid remembers it.

## First 10 minutes

```bash
mermaid doctor                         # Check model, tools, safety, and project instructions
mermaid                                # Start the full-screen terminal coding agent
```

Then ask for normal coding-agent work:

- "read the repo and tell me where the test runner lives"
- "find the bug in this failing test and fix it"
- "review the current branch for regressions"

Inside the TUI, use `/help` for grouped commands, `/doctor` for the session readiness report, `/context` to inspect prompt budget, `/compact [focus]` to create a handoff checkpoint, and Esc to interrupt the agent loop.

## Usage

```bash
mermaid                                    # Start fresh session
mermaid --continue                         # Resume the most recent session in this directory
mermaid --model anthropic/<model>          # Pick a model (see Remote providers below)
mermaid run "fix the tests"                # Non-interactive mode
mermaid add <name>                         # Add an MCP server from the built-in registry (e.g., context7, git)
```

Every flag, keyboard shortcut, and slash command: [docs/cli-reference.md](docs/cli-reference.md). `mermaid --help` and `/help` list them too.

Remote MCP servers that sign in with OAuth (Linear, Notion, Sentry, Atlassian and others) work too: `mermaid add linear --url https://mcp.linear.app/mcp` opens the browser to sign in, and `mermaid mcp login <name>` signs in again later. Tokens go to the OS keyring and refresh on their own. See [docs/tools.md](docs/tools.md#remote-servers-and-sign-in).

## Tools

The model calls these autonomously:

| Tool | Description |
|------|-------------|
| `read_file` | Read files (text, PDF, images) |
| `write_file` | Create or overwrite files (timestamped backup) |
| `edit_file` | Single-location search-and-replace; refuses an ambiguous match |
| `apply_patch` | Multi-hunk, context-anchored edits with a diff (fuzzy-tolerant) |
| `delete_file` | Delete files (timestamped backup) |
| `create_directory` | Create directories |
| `execute_command` | Run shell commands; background mode tracks PID, log, and URL |
| `background_process` | Read new output from, wait on, or stop a process `execute_command` left running |
| `memory` | Durable cross-session memory (project, shared, or global scope) |
| `web_search` | Search the web (managed local SearXNG by default) |
| `web_fetch` | Fetch a URL into a bounded session snapshot (in-process, no key) |
| `agent` | Spawn an autonomous subagent for parallel tasks |
| `task_create`, `task_update`, `task_list` | The live task checklist (`/todos`) |
| `ask_user_question` | Multiple-choice questions when a decision is the user's to make |
| `context_archive` | Search or read back the session's full history, including what compaction removed |
| `compact_context` | Checkpoint the context now instead of waiting for the automatic threshold |

MCP servers contribute tools under the `mcp__<server>__<tool>` prefix, **deferred** by default: one `tool_search` tool promotes matches for the rest of the session, so unpromoted schemas never count against `/context`. Opt out with `mcp_defer_tools = false`.

## Safety

Approval policy and OS confinement are independent. The policy (`read_only`, `ask`, `auto`, `full_access`) decides what needs your say-so; the sandbox (`--no-network`, `--confine-fs`, or both with `--sandbox`) decides what the kernel permits regardless, and fails closed. See [docs/sandbox.md](docs/sandbox.md).

## Project instructions

Create an `AGENTS.md` (the cross-tool open standard) and/or a `MERMAID.md` (mermaid-specific) at your project root with conventions, tool versions, naming patterns, and run commands. Both load from the nearest matching directory — `AGENTS.md` first, then `MERMAID.md`, so MERMAID.md overrides on conflict. They auto-reload when the files change, and the walk stops at the `.git` root or `$HOME`. This repo's own [AGENTS.md](AGENTS.md) is a worked example.

Coming from Claude Code? A `CLAUDE.md` loads when there is no `AGENTS.md`, and the skills, commands and agents in `.claude/` and `.agents/` (project and `~/`) load too. Put your own prompt commands in `.mermaid/commands/` as markdown. See [docs/plugins.md](docs/plugins.md#files-from-other-tools).

## Configuration

Config lives at `~/.config/mermaid/config.toml`; `mermaid init` creates one. A repo can commit shared defaults in `.mermaid/config.toml`, which can tighten safety but never loosen it. Layers merge key-by-key, later winning: built-in defaults, user config, project config, then session flags (`-c key.path=value`).

```toml
[default_model]
provider = "ollama"
name = "qwen3-coder:30b"
reasoning = "medium"   # none | minimal | low | medium | high | xhigh | max

[safety]
mode = "auto"           # read_only | ask | auto | full_access
checkpoint_on_mutation = true
```

The annotated full schema — safety enforcement floors, compaction budgets, subagent types, profiles, model aliases, provider overrides, web backends — is in [docs/configuration.md](docs/configuration.md).

## Remote providers

Set the appropriate environment variable, or override it with `[providers.<name>].api_key_env`. Model names are whatever the vendor currently ships; Mermaid passes them through.

| Provider | Env var | Model format |
|----------|---------|--------------|
| Anthropic | `ANTHROPIC_API_KEY` | `anthropic/<model>` |
| Google Gemini | `GOOGLE_API_KEY` (`GEMINI_API_KEY` legacy fallback) | `gemini/<model>` |
| Meta | `MODEL_API_KEY` | `meta/<model>` |
| OpenAI | `OPENAI_API_KEY` | `openai/<model>` |
| Groq | `GROQ_API_KEY` | `groq/<model>` |
| OpenRouter | `OPENROUTER_API_KEY` | `openrouter/<vendor>/<model>` |
| Cerebras | `CEREBRAS_API_KEY` | `cerebras/<model>` |
| DeepInfra | `DEEPINFRA_API_KEY` | `deepinfra/<vendor>/<model>` |
| Together | `TOGETHER_API_KEY` | `together/<vendor>/<model>` |
| NVIDIA NIM | `NVIDIA_API_KEY` | `nvidia/<vendor>/<model>` |
| Cloudflare Workers AI | `CLOUDFLARE_API_TOKEN` + `CLOUDFLARE_ACCOUNT_ID` | `cloudflare/@cf/<vendor>/<model>` |
| Grok (xAI) | `XAI_API_KEY` | `grok/<model>` (`xai/<model>` alias) |
| Ollama Cloud | `OLLAMA_API_KEY` | `ollama/<model>:cloud` |

Or store a key in the OS keyring with `mermaid login <provider>`; environment variables still win. Details: [API keys](docs/configuration.md#api-keys) and [provider notes](docs/configuration.md#provider-notes).

## Architecture

Mermaid's runtime is an Elm/MVU pattern: one pure reducer (`fn update(State, Msg) -> (State, Vec<Cmd>)`), effects as data, structured concurrency per turn. Duplicate error display, 20-press Ctrl+C during tool execution, stale stream events corrupting a new turn — whole classes of bug are statically impossible against those types.

## Documentation

- [docs/cli-reference.md](docs/cli-reference.md) — every flag, shortcut, and slash command
- [docs/configuration.md](docs/configuration.md) — full config schema, layering, providers, web backends
- [docs/tools.md](docs/tools.md) — MCP naming and deferral, web tools, inline approvals
- [docs/sandbox.md](docs/sandbox.md) — OS confinement, per platform
- [docs/plugins.md](docs/plugins.md) — skills, hooks, and plugin bundles
- [docs/runtime.md](docs/runtime.md) — the optional `mermaidd` service, logging, diagnostics
- [docs/development.md](docs/development.md) — pre-PR gate, CI matrix, snapshot suites
- [AGENTS.md](AGENTS.md) — contributor and agent guardrails

## License

MIT OR Apache-2.0

Built with [Ratatui](https://github.com/ratatui-org/ratatui) and [Ollama](https://ollama.com). Inspired by [Aider](https://github.com/paul-gauthier/aider) and [Claude Code](https://github.com/anthropics/claude-code).
