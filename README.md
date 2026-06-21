# Mermaid

An open-source AI coding assistant with computer use for the terminal. Multi-provider — Ollama (local), Anthropic, Gemini, OpenAI, Groq, OpenRouter, and any OpenAI-compatible endpoint — with native tool calling, subagents, computer-use tools, and a clean TUI.

## Features

- **Multi-Provider** — Ollama (local/cloud), Anthropic Claude, Google Gemini, OpenAI, Groq, OpenRouter, Cerebras, DeepInfra, Together, plus fully-custom OpenAI-compatible endpoints
- **Native Tool Calling** — read, write, edit, delete, create directories, execute commands, search the web, spawn subagents, and call configured MCP tools
- **Computer Use** — screenshot, click, type, press keys, scroll, move the mouse, and list windows on supported interactive GUI backends
- **Subagents** — spawn parallel autonomous agents for independent tasks
- **Agent Loop** — model calls tools autonomously, sees results, and continues until done
- **Image Paste** — Ctrl+V to attach images for vision models (X11/Wayland/macOS/Windows)
- **Reasoning Levels** — seven tiers (`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`); cycle with Alt+T or set via `/reasoning`; persisted per-model
- **Project Instructions** — auto-loads `MERMAID.md`, `AGENTS.md`, `CLAUDE.md`, and `GEMINI.md`; edits take effect on the next turn
- **MCP Servers** — stdio JSON-RPC client with a built-in registry of 16 popular servers (`mermaid add <name>`)
- **Session Persistence** — conversations auto-save and resume with `--continue`
- **Message Queuing** — type while the model generates, messages send in order
- **Non-Interactive Mode** — script with `mermaid run "prompt"` for CI/automation

### Architecture

Mermaid's runtime is an Elm/MVU pattern: one pure reducer (`fn update(State, Msg) -> (State, Vec<Cmd>)`), effects as data, structured concurrency per turn. Whole classes of bug the old architecture let slip — duplicate error display, 20-press Ctrl+C during tool execution, stale stream events corrupting a new turn — are statically impossible against the new types.

Read [`docs/architecture.md`](docs/architecture.md) for the full tour. The [adding a tool](docs/adding_tools.md) and [adding a provider](docs/adding_providers.md) recipes are one file each; [`docs/replay_debugging.md`](docs/replay_debugging.md) covers record/replay for reproducing bugs.

## Quick Start

Prebuilt binaries for Linux, macOS, and Windows (plus `.deb`/`.rpm`) are attached to every [GitHub Release](https://github.com/noahsabaj/mermaid-cli/releases).

```bash
# Build and install the latest release from git
cargo install --git https://github.com/noahsabaj/mermaid-cli

# Or from a local clone
git clone https://github.com/noahsabaj/mermaid-cli.git
cd mermaid-cli
cargo install --path .

# From crates.io
cargo install mermaid-cli
```

> The crates.io release can lag the newest tag. For the latest version, use the GitHub Release binaries or the `--git` install above.

Local inference requires [Ollama](https://ollama.com) (models auto-pull if not found locally). Cloud providers are optional — see [Remote Providers](#remote-providers) below.

### First 10 Minutes

```bash
mermaid doctor                         # Check model, tools, safety, and project instructions
mermaid                                # Start the full-screen terminal coding agent
```

Then ask Mermaid to do normal coding-agent work:

- "read the repo and tell me where the test runner lives"
- "find the bug in this failing test and fix it"
- "add this small feature and run the relevant tests"
- "review the current branch for regressions"

Inside the TUI, use `/help` for grouped commands, `/doctor` for the current session readiness report, `/context` to inspect prompt budget and compaction status, `/compact [focus]` to create a handoff checkpoint, and Esc to interrupt the current agent loop.

### Computer Use Dependencies (optional)

For full Linux GUI control via screenshot/click/type tools:

```bash
# Linux / X11
sudo apt install scrot xdotool xclip

# Linux / Wayland
sudo apt install grim ydotool wtype wl-clipboard

# Screenshot downscaling (optional, for high-res displays)
sudo apt install imagemagick
```

Computer-use registration is backend-gated: Linux/X11 and Linux/Wayland are the current full-control backends. macOS currently supports screenshot capture through `screencapture` plus clipboard image paste through `pngpaste`/`osascript`; click/type/scroll are not yet ported there. Windows clipboard paste uses PowerShell, but the computer-use backend is not wired yet. See `src/providers/tool/computer_use/` for the implementation matrix.

## Usage

```bash
mermaid                                         # Start fresh session
mermaid --continue                              # Resume last session
mermaid --sessions                              # Pick a previous session to resume
mermaid --model ollama/qwen3-coder:30b          # Ollama local
mermaid --model anthropic/claude-opus-4-7       # Anthropic (requires ANTHROPIC_API_KEY)
mermaid --model gemini/gemini-3.1-pro-preview   # Gemini (requires GOOGLE_API_KEY)
mermaid --model openai/gpt-5                    # OpenAI (requires OPENAI_API_KEY)
mermaid --model groq/qwen-qwq-32b               # Groq (requires GROQ_API_KEY)
mermaid --reasoning high                        # Override default reasoning depth
mermaid --path /path/to/project                  # Run against a specific project directory
mermaid --record /tmp/session.jsonl              # Record reducer events for replay/debugging
mermaid --append-system-prompt "Prefer small diffs" # Add one-off runtime instructions
mermaid --system-prompt-file ./prompt.md         # Replace the default prompt for one run
mermaid list                                    # List available models across providers
mermaid doctor                                  # First-run readiness check
mermaid status                                  # Lower-level Ollama, MCP, and provider config
mermaid self-test                               # Fast deterministic Mermaid self-test
mermaid init                                    # Create default config file
mermaid cloud-setup                             # Configure Ollama Cloud API key
mermaid run "fix the tests"                     # Non-interactive mode
mermaid run "explain main.rs" -f json           # JSON output
mermaid add <name>                              # Add an MCP server (e.g., context7, git)
mermaid remove <name>                           # Remove a configured MCP server
mermaid mcp                                     # List configured MCP servers
mermaid pr create                               # Open a PR/MR from the current branch (wraps gh/glab)
```

`mermaid add <name>` resolves the name through a built-in registry of 16 popular MCP servers (context7, playwright, memory, git, fetch, time, filesystem, notion, slack, postgres, brave-search, supabase, perplexity, docker, sequential-thinking, everything), prompts for any required env vars, validates by spawning the server, and saves it to `~/.config/mermaid/config.toml`.

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| Enter | Send message (or queue while the model is generating) |
| Esc | Stop generation / dismiss command palette or attachment focus |
| Ctrl+C | Quit (auto-saves the session) |
| Alt+T | Cycle reasoning level: `None → Minimal → Low → Medium → High → XHigh → Max → None` |
| Ctrl+V | Paste image or text from clipboard |
| Ctrl+Click | Open image from chat history |
| `/` | Open slash-command palette (filter-as-you-type) |
| Tab | In palette: complete highlighted command name |
| Up/Down | Navigate input history; palette and conversation-list navigation |
| Mouse Wheel | Scroll chat |

## Slash Commands

Type `/` to open the command palette (shows all commands with live filter); type `/<name>` to invoke directly. `/help` shows the same commands grouped in the TUI.

Everyday:

- `/doctor` — show current model, safety, prompt, instruction, and tool readiness
- `/clear`, `/save [name]`, `/load [id]`, `/list` — manage the conversation
- `/cancel [id]` — cancel the active turn or a durable task
- `/handoff [id]`, `/report [id]` — write a current-context report or inspect a task report
- `/help` (`/h`), `/quit` (`/q`)

Model and context:

- `/model <name>` — switch model; auto-pulls Ollama models if needed
- `/reasoning <level>` — set reasoning: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`
- `/visible-reasoning [on|off|toggle]` — show or hide reasoning blocks in the transcript
- `/usage`, `/context`, `/compact [instructions]`
- `/model-info <model>`

Safety and recovery:

- `/approvals`, `/approve <id>`, `/deny <id>`
- `/checkpoint <path...>`, `/checkpoints`, `/restore <id>`

Integrations:

- `/memory`, `/memory edit <id> <value>`, `/remember <key> <value>`, `/forget <id>`
- `/plugins`, `/cloud-setup`

Advanced runtime:

- `/tasks`, `/task <id>`, `/pause <id>`, `/resume <id>`
- `/processes`, `/logs <id>`, `/stop <id>`, `/restart <id>`, `/open <target>`, `/ports`

Reasoning choices persist per-model: setting `/reasoning high` on Claude Opus 4.7 and `/reasoning low` on Ollama is remembered across sessions.

## Tools

The model uses these autonomously via native tool calling:

| Tool | Description |
|------|-------------|
| `read_file` | Read files (text, PDF, images) |
| `write_file` | Create or overwrite files (timestamped backup if file exists) |
| `edit_file` | Targeted text replacement with diff |
| `delete_file` | Delete files (timestamped backup) |
| `create_directory` | Create directories |
| `execute_command` | Run shell commands; background mode registers PID/log/URL metadata for GUI apps and dev servers |
| `web_search` | Search the web (Ollama Cloud) |
| `web_fetch` | Fetch URL content as markdown (Ollama Cloud) |
| `agent` | Spawn autonomous sub-agent for parallel tasks |
| `screenshot` | Capture the screen (fullscreen, focused window, monitor, region, or window by title) |
| `list_windows` | List visible window titles (X11-only discovery for window-mode screenshots) |
| `click` | Click at screen coordinates (auto-screenshot after) |
| `type_text` | Type text at cursor position (auto-screenshot after) |
| `press_key` | Press key combos (ctrl+s, alt+tab, etc.) |
| `scroll` | Scroll up or down |
| `mouse_move` | Move mouse cursor without clicking |

MCP servers contribute additional tools under the `mcp__<server>__<tool>` prefix when configured. Web tools are registered only when `OLLAMA_API_KEY` is set in the environment. Computer-use tools are advertised only in interactive TUI sessions when a usable GUI backend is detected.

## Project Instructions

Create a `MERMAID.md` at your project root with conventions, tool versions, naming patterns, and run commands. Mermaid also reads interoperable `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, and `.mermaid/memory/memory.jsonl` from the same nearest matching directory, then auto-reloads when those files change (one `stat` per turn, no filesystem watcher). The walk stops at the `.git` root or `$HOME`.

```markdown
# Project: foo-service

## Conventions
- snake_case for functions, PascalCase for types
- No `unwrap()` outside of tests
- Run `cargo nextest run` for tests (not `cargo test`)

## Build
- `just dev` — dev server on :8080
```

File size is capped at ~10k tokens; oversized content is truncated with a marker so the model knows context was elided.

## Runtime And Background Service

The CLI/TUI is the primary Mermaid app. `mermaidd` is optional advanced infrastructure for durable runtime state, remote attach, and long-running process ownership; normal chat, `mermaid run`, and `mermaid self-test` work without the user service.

`mermaidd` stores durable runtime state in `~/.local/share/mermaid/runtime.sqlite3` and exposes a local Unix-socket JSONL control surface at `~/.local/share/mermaid/mermaidd.sock`. The socket is created mode `0600` and the data dir `0700`, so only your user can reach it. A localhost TCP listener on `127.0.0.1:39871` is **off by default** — enable it with `MERMAID_DAEMON_ENABLE_TCP=1`. Mutating Unix-socket JSON commands require a pairing token; when TCP is enabled, every command (including health) requires a token. Create one with `mermaid pair --label <device>` and pass it as `MERMAID_DAEMON_TOKEN` or `auth.token`.

The CLI can inspect and manage the same store with `mermaid tasks`, `mermaid task <id>`, `mermaid approvals`, `mermaid approve <id>`, `mermaid deny <id>`, `mermaid tool-runs`, `mermaid checkpoints`, `mermaid restore <id>`, `mermaid memory`, `mermaid remember`, `mermaid memory-edit <id> <value>`, `mermaid forget`, `mermaid plugin list`, `mermaid plugin install <path-or-github>`, `mermaid plugin audit <path>`, `mermaid models`, `mermaid model-info <model>`, `mermaid processes`, `mermaid logs <process>`, `mermaid stop <process>`, `mermaid restart <process>`, `mermaid open <target>`, `mermaid ports`, `mermaid pair`, and `mermaid daemon`. Installing a plugin from a Git URL (rather than a local path) requires an explicit full URL and `MERMAID_ALLOW_PLUGIN_FETCH=1`, since fetching and later running remote plugin code is a privileged operation.

On Linux, install a per-user systemd unit with `mermaid daemon install --start`. The installer writes `~/.config/systemd/user/mermaidd.service`, points `ExecStart` at the discovered `mermaidd` binary, reloads systemd's user manager, and optionally enables/starts the service. Use `mermaid daemon status`, `mermaid daemon logs [-f]`, `mermaid daemon restart`, `mermaid daemon stop`, `mermaid daemon uninstall`, or `mermaid daemon print-unit` for day-to-day service management. Set `MERMAID_DAEMON_BIN=/absolute/path/to/mermaidd` before installing if the background-service binary is not next to `mermaid` or on `PATH`.

Release builds keep the existing `.tar.gz`/`.zip` archives and add Linux `.deb`/`.rpm` artifacts for x86_64 and aarch64. The distro packages install `mermaid`, `mermaidd`, docs, and a reference systemd user unit at `/usr/lib/systemd/user/mermaidd.service`; they do not auto-enable or start the daemon.

## Configuration

Config file: `~/.config/mermaid/config.toml` (Linux) or platform equivalent via `directories` crate.

Run `mermaid init` to create a default config. Important fields in the current config schema:

```toml
# Last model picked via `--model` — used by bare `mermaid` on next start
last_used_model = "ollama/qwen3-coder:30b"

[default_model]
provider = "ollama"
name = "qwen3-coder:30b"
temperature = 0.7
max_tokens = 4096
reasoning = "medium"  # none | minimal | low | medium | high | xhigh | max

[ollama]
host = "localhost"
port = 11434
# cloud_api_key = "your-key"  # for :cloud models
# num_gpu = 10
# num_thread = 8
# num_ctx = 8192
# numa = false

[safety]
# Approval policy. Default is "ask": prompt before mutations / shell / network
# actions. Set "full_access" to auto-run everything (the pre-0.8 default),
# "auto_review" to auto-allow low-risk and ask for the rest, or "read_only".
mode = "ask"
checkpoint_on_mutation = true

[non_interactive]
# Current v0.8 run behavior is controlled by CLI flags:
#   mermaid run "prompt" --format json --max-tokens 4096 --no-execute
# These fields remain in the schema for compatibility but are not the
# source of truth for `mermaid run`.
output_format = "text"
max_tokens = 4096
no_execute = false

# Per-model reasoning preferences (remembered across sessions)
[reasoning_per_model]
"anthropic/claude-opus-4-7" = "high"
"ollama/qwen3-coder:30b" = "low"

# Optional agent/plugin model profiles. A request for `--model fast` or
# `--model profile:fast` resolves through this table when present.
[model_profiles]
fast = "ollama/qwen3-coder:14b"
large-context = "openai/gpt-5"
tool-strong = "anthropic/claude-opus-4-7"
vision = "gemini/gemini-3.1-pro-preview"
cheap = "groq/qwen-qwq-32b"

# Remote providers — override env-var name, base URL, or extra headers
[providers.anthropic]
# api_key_env = "MY_ANTHROPIC_KEY"  # default: ANTHROPIC_API_KEY

[providers.gemini]
# api_key_env = "MY_GOOGLE_KEY"  # default: GOOGLE_API_KEY; GEMINI_API_KEY is accepted as a legacy fallback

[providers.groq]
# api_key_env = "MY_GROQ_KEY"    # default: GROQ_API_KEY
# base_url = "https://api.groq.com/openai/v1"
# extra_headers = { "X-Custom-Header" = "value" }

# Custom OpenAI-compatible provider (e.g., self-hosted vLLM)
[providers.my-vllm]
base_url = "http://192.168.1.42:8000/v1"
api_key_env = "VLLM_KEY"
compat = "openai-effort"   # openai | openai-effort | openrouter
# default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"

# MCP servers — usually managed via `mermaid add <name>`
[mcp_servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
```

System prompt customization is runtime-only and is not saved to config:

```bash
mermaid --append-system-prompt "Prefer minimal diffs"
mermaid --append-system-prompt-file ./extra-instructions.md
mermaid --system-prompt "You are a focused code reviewer."
mermaid --system-prompt-file ./replacement-system-prompt.md
```

## Remote Providers

Set the appropriate environment variable (or override via `[providers.<name>].api_key_env` in config):

| Provider | Env var | Example model |
|----------|---------|---------------|
| Anthropic | `ANTHROPIC_API_KEY` | `anthropic/claude-opus-4-7` |
| Google Gemini | `GOOGLE_API_KEY` (`GEMINI_API_KEY` legacy fallback) | `gemini/gemini-3.1-pro-preview` |
| OpenAI | `OPENAI_API_KEY` | `openai/gpt-5` |
| Groq | `GROQ_API_KEY` | `groq/qwen-qwq-32b` |
| OpenRouter | `OPENROUTER_API_KEY` | `openrouter/anthropic/claude-3.7-sonnet` |
| Cerebras | `CEREBRAS_API_KEY` | `cerebras/gpt-oss-120b` |
| DeepInfra | `DEEPINFRA_API_KEY` | `deepinfra/deepseek-ai/DeepSeek-R1` |
| Together | `TOGETHER_API_KEY` | `together/deepseek-ai/DeepSeek-R1` |
| Ollama Cloud | `OLLAMA_API_KEY` | `ollama/kimi-k2-thinking:cloud` |

Ollama Cloud models use `OLLAMA_API_KEY` or `cloud_api_key` under `[ollama]`. Web search and web fetch tool registration currently requires `OLLAMA_API_KEY` in the environment. Use `mermaid cloud-setup` from your shell to save the config key for cloud models; `/cloud-setup` in the TUI points back to that shell command.

## License

MIT OR Apache-2.0

Built with [Ratatui](https://github.com/ratatui-org/ratatui) and [Ollama](https://ollama.com). Inspired by [Aider](https://github.com/paul-gauthier/aider) and [Claude Code](https://github.com/anthropics/claude-code).
