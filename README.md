# Mermaid

An open-source AI coding assistant for the terminal. Multi-provider — Ollama (local), Anthropic, Gemini, Meta, OpenAI, Groq, OpenRouter, and any OpenAI-compatible endpoint — with native tool calling, subagents, computer use, and a clean TUI.

## Features

- **Native tool calling** — read, write, edit, delete, run commands, search the web, spawn subagents, call MCP tools
- **Computer use** — screenshot, click, type, press keys, scroll, move the mouse, list windows
- **Subagents** — spawn parallel autonomous agents; built-in `general` and read-only `explore` types, per-call model override, continuation handles
- **Worktree isolation** — give a writing subagent its own git checkout, seeded with your uncommitted state. Its changes land as one patch, serialized against other children, so parallel writers report a conflict instead of interleaving
- **Safety modes** — `plan`/`read_only`/`ask`/`auto`/`full_access`, cycled live with Shift+Tab; `auto` is classifier-backed, and gated actions prompt inline rather than erroring out
- **Plan mode** — a hard read-only state where the agent explores and authors a plan file you approve before anything changes; `mermaid run --plan` does it headless
- **Checkpoints** — shadow-git snapshots before mutations; inspect with `/checkpoints`, roll back with `/restore <id>`
- **Durable memory** — the agent remembers facts across sessions; a compact index auto-loads into every prompt
- **Project instructions and skills** — auto-loads `AGENTS.md` and `MERMAID.md`, plus task-specific playbooks loaded on demand
- **MCP servers** — stdio JSON-RPC client with a built-in registry of 16 popular servers
- **Sessions** — conversations auto-save; `--continue` reopens the last one here, `--resume` opens a picker, double-Esc forks the timeline at an earlier message
- **Context compaction** — automatic checkpoint-and-continue when the window fills; manual `/compact [focus]`
- **Image paste** — Ctrl+V attaches images for vision models on X11, Wayland, macOS, and Windows
- **Reasoning levels** — seven tiers, cycled with Alt+T, persisted per model
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
mermaid --resume                           # Pick a past session from a searchable list
mermaid --model anthropic/<model>          # Pick a model (see Remote providers below)
mermaid --reasoning high                   # Override default reasoning depth
mermaid --path /path/to/project            # Run against a specific project directory
mermaid list                               # List available models across providers
mermaid doctor                             # First-run readiness check
mermaid init                               # Create default config file
mermaid add <name>                         # Add an MCP server (e.g., context7, git)
mermaid pr create                          # Open a PR/MR from the current branch (wraps gh/glab)

mermaid run "fix the tests"                # Non-interactive mode
mermaid run "explain main.rs" -f json      # JSON output (or -f ndjson to stream events)
mermaid run --plan "refactor the auth"     # Headless plan mode: read-only, delivers a plan file
mermaid --sandbox run "refactor this"      # Confine writes to the project, deny network
```

Every flag, structured output, headless session resume, and record/replay: [docs/cli-reference.md](docs/cli-reference.md).

`mermaid add <name>` resolves the name through a registry of 16 popular MCP servers (context7, playwright, git, postgres, notion, slack, and more), prompts for required env vars, and validates by spawning the server.

## Keyboard shortcuts

| Key | Action |
|-----|--------|
| Enter | Send message (or queue while the model is generating) |
| Esc | Stop generation / dismiss palette |
| Esc Esc | (idle) Rewind: fork the session at an earlier message |
| Ctrl+C | Quit (auto-saves the session) |
| Alt+T | Cycle reasoning level |
| Shift+Tab | Cycle safety mode: `plan → read_only → ask → auto → full_access` |
| Ctrl+V | Paste image or text from clipboard |
| Ctrl+O | Compose the prompt in `$VISUAL`/`$EDITOR` |
| `/` | Open the slash-command palette |
| `@` | Open the fuzzy file picker |

The [full tables](docs/cli-reference.md#keyboard-shortcuts) add selection, background processes, and every slash command; `/help` groups them in the TUI.

## Tools

The model calls these autonomously:

| Tool | Description |
|------|-------------|
| `read_file` | Read files (text, PDF, images) |
| `write_file` | Create or overwrite files (timestamped backup) |
| `apply_patch` | Multi-hunk, context-anchored edits with a diff (fuzzy-tolerant) |
| `delete_file` | Delete files (timestamped backup) |
| `create_directory` | Create directories |
| `execute_command` | Run shell commands; background mode tracks PID, log, and URL |
| `memory` | Durable cross-session memory (project, shared, or global scope) |
| `web_search` | Search the web (managed local SearXNG by default) |
| `web_fetch` | Fetch a URL into a bounded session snapshot (in-process, no key) |
| `agent` | Spawn an autonomous subagent for parallel tasks |

Plus seven computer-use tools — `screenshot`, `list_windows`, `click`, `type_text`, `press_key`, `scroll`, `mouse_move` — advertised only in interactive sessions with a usable GUI backend. Linux/X11 and Linux/Wayland are full-control; macOS does screenshots and clipboard paste but not click/type/scroll; Windows is not wired yet. Helpers and the full matrix: [docs/tools.md](docs/tools.md).

MCP servers contribute tools under the `mcp__<server>__<tool>` prefix, **deferred** by default: one `tool_search` tool promotes matches for the rest of the session, so unpromoted schemas never count against `/context`. Opt out with `mcp_defer_tools = false`.

## Safety

Approval policy and OS confinement are independent. The policy (`plan`, `read_only`, `ask`, `auto`, `full_access`) decides what needs your say-so; the sandbox decides what the kernel permits regardless:

- `--no-network` — blocks web tools everywhere, and stops model-run commands from reaching the network
- `--confine-fs` — write-class filesystem access only beneath the project root, cwd, and temp
- `--sandbox` — both at once

Enforcement is seccomp-BPF plus Landlock on Linux, Seatbelt on macOS, AppContainer plus Job Objects on Windows. It fails closed: unappliable confinement exits 126 rather than running unconfined. See [docs/sandbox.md](docs/sandbox.md).

## Project instructions

Create an `AGENTS.md` (the cross-tool open standard) and/or a `MERMAID.md` (mermaid-specific) at your project root with conventions, tool versions, naming patterns, and run commands. Both load from the nearest matching directory — `AGENTS.md` first, then `MERMAID.md`, so MERMAID.md overrides on conflict. They auto-reload when the files change, and the walk stops at the `.git` root or `$HOME`. This repo's own [AGENTS.md](AGENTS.md) is a worked example.

## Configuration

Config lives at `~/.config/mermaid/config.toml`; `mermaid init` creates one. A repo can commit shared defaults in `.mermaid/config.toml`, which can tighten safety but never loosen it. Layers merge key-by-key, later winning: built-in defaults, user config, project config, then session flags (`-c key.path=value`).

```toml
[default_model]
provider = "ollama"
name = "qwen3-coder:30b"
reasoning = "medium"   # none | minimal | low | medium | high | xhigh | max

[safety]
mode = "ask"           # plan | read_only | ask | auto | full_access
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

Environment variables always win; the OS keyring fills the gap when none is set. Store a key with `mermaid login <provider>` and remove it with `mermaid logout <provider>`. Keys are reported as `env`, `keyring`, or `none` — never by value. Per-provider details are in [docs/configuration.md](docs/configuration.md#provider-notes).

## Architecture

Mermaid's runtime is an Elm/MVU pattern: one pure reducer (`fn update(State, Msg) -> (State, Vec<Cmd>)`), effects as data, structured concurrency per turn. Duplicate error display, 20-press Ctrl+C during tool execution, stale stream events corrupting a new turn — whole classes of bug are statically impossible against those types.

## Documentation

- [docs/cli-reference.md](docs/cli-reference.md) — every flag, shortcut, and slash command
- [docs/configuration.md](docs/configuration.md) — full config schema, layering, providers, web backends
- [docs/tools.md](docs/tools.md) — MCP naming and deferral, web tools, computer-use backends
- [docs/sandbox.md](docs/sandbox.md) — OS confinement, per platform
- [docs/plugins.md](docs/plugins.md) — skills, hooks, and plugin bundles
- [docs/runtime.md](docs/runtime.md) — the optional `mermaidd` service, logging, diagnostics
- [docs/development.md](docs/development.md) — pre-PR gate, CI matrix, snapshot suites
- [AGENTS.md](AGENTS.md) — contributor and agent guardrails

## Development

```
just check    # cargo fmt --check + clippy -D warnings + guards + cargo nextest run
```

That is the exact pre-PR gate, and what CI's blocking jobs run.

## License

MIT OR Apache-2.0

Built with [Ratatui](https://github.com/ratatui-org/ratatui) and [Ollama](https://ollama.com). Inspired by [Aider](https://github.com/paul-gauthier/aider) and [Claude Code](https://github.com/anthropics/claude-code).
