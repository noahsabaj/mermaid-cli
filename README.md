# Mermaid

An open-source AI coding assistant with computer use for the terminal. Multi-provider — Ollama (local), Anthropic, Gemini, OpenAI, Groq, OpenRouter, and any OpenAI-compatible endpoint — with native tool calling, subagents, computer-use tools, and a clean TUI.

## Features

- **Multi-Provider** — Ollama (local/cloud), Anthropic Claude, Google Gemini, OpenAI, Groq, OpenRouter, Cerebras, DeepInfra, Together, plus fully-custom OpenAI-compatible endpoints
- **Native Tool Calling** — read, write, edit, delete, create directories, execute commands, search the web, spawn subagents, and call configured MCP tools
- **Computer Use** — screenshot, click, type, press keys, scroll, move the mouse, and list windows on supported interactive GUI backends
- **Subagents** — spawn parallel autonomous agents for independent tasks; built-in `general` and read-only `explore` types (plus user-defined ones), per-call model override, and continuation handles to follow up with a child that kept its context
- **Agent Loop** — model calls tools autonomously, sees results, and continues until done
- **Image Paste** — Ctrl+V to attach images for vision models (X11/Wayland/macOS/Windows)
- **Reasoning Levels** — seven tiers (`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`); cycle with Alt+T or set via `/reasoning`; persisted per-model
- **Safety Modes** — `read_only`/`ask`/`auto`/`full_access`; `auto` is classifier-backed (an LLM vets each borderline action against your intent, auto-running aligned ones and escalating risky ones); cycle live with Shift+Tab or `/safety`
- **Inline approvals** — in `ask` mode (and `auto` escalations) a gated action pauses and prompts inline (`1` Yes · `2` Yes, don't ask again · `3`/Esc No); the agent waits for your answer instead of erroring out
- **Checkpoints** — shadow-git snapshots before mutations (`checkpoint_on_mutation`, on by default); inspect with `/checkpoints`, roll back with `/restore <id>`
- **Project Instructions** — auto-loads `AGENTS.md` and `MERMAID.md` (MERMAID.md wins on conflict); edits take effect on the next turn
- **Durable Memory** — the agent remembers facts across sessions (`memory` tool + `/remember`, `/memory`, `/forget`); a compact index auto-loads into every prompt
- **MCP Servers** — stdio JSON-RPC client with a built-in registry of 16 popular servers (`mermaid add <name>`)
- **Session Persistence** — conversations auto-save; `--continue` reopens the last one in the current directory, `--resume` opens a searchable picker of past sessions
- **Context Compaction** — automatic checkpoint-and-continue when the window fills (or the model truncates mid-run); manual `/compact [focus]` for handoffs
- **Record & Replay** — `--record` captures every reducer input; `--replay` reconstructs the session offline, deterministically, with a built-in purity check
- **Message Queuing** — type while the model generates, messages send in order
- **Non-Interactive Mode** — script with `mermaid run "prompt"` for CI/automation

### Architecture

Mermaid's runtime is an Elm/MVU pattern: one pure reducer (`fn update(State, Msg) -> (State, Vec<Cmd>)`), effects as data, structured concurrency per turn. Whole classes of bug the old architecture let slip — duplicate error display, 20-press Ctrl+C during tool execution, stale stream events corrupting a new turn — are statically impossible against the new types.

Read [`docs/architecture.md`](docs/architecture.md) for the full tour. The [adding a tool](docs/adding_tools.md) and [adding a provider](docs/adding_providers.md) recipes are one file each; [`docs/replay_debugging.md`](docs/replay_debugging.md) covers record/replay for reproducing bugs.

## Get started

No Rust or cargo required — the installer downloads a prebuilt binary for your platform from the latest [GitHub Release](https://github.com/noahsabaj/mermaid-cli/releases), verifies its checksum, and puts `mermaid` on your PATH.

**macOS / Linux**

```bash
curl -fsSL https://noahsabaj.github.io/mermaid-cli/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://noahsabaj.github.io/mermaid-cli/install.ps1 | iex
```

Then run `mermaid` to start, and `mermaid update` whenever you want the newest version. (Set `MERMAID_INSTALL_DIR` to change the install location, or `MERMAID_VERSION=vX.Y.Z` to pin a specific release.)

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

All three are bumped automatically on every release; upgrade with `mermaid update` or your package manager.

<details>
<summary>Install with cargo instead (needs the Rust toolchain)</summary>

```bash
cargo install mermaid-cli                                      # from crates.io
cargo install --git https://github.com/noahsabaj/mermaid-cli   # latest from git
```

Prebuilt binaries (plus `.deb`/`.rpm`) are attached to every release; the crates.io release can lag the newest tag.

</details>

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
mermaid --continue                              # Resume the most recent session in this directory
mermaid --resume                                # Pick a past session from a searchable list
mermaid --model ollama/qwen3-coder:30b          # Ollama local (any installed model — `mermaid list`)
mermaid --model anthropic/<model>               # Anthropic (requires ANTHROPIC_API_KEY)
mermaid --model gemini/<model>                  # Gemini (requires GOOGLE_API_KEY)
mermaid --model openai/<model>                  # OpenAI (requires OPENAI_API_KEY)
mermaid --model groq/<model>                    # Groq (requires GROQ_API_KEY)
mermaid --reasoning high                        # Override default reasoning depth
mermaid --path /path/to/project                  # Run against a specific project directory
mermaid --record /tmp/session.jsonl              # Record reducer events for replay/debugging
mermaid --replay /tmp/session.jsonl              # Reconstruct a recorded session (headless, deterministic)
mermaid --append-system-prompt "Prefer small diffs" # Add one-off runtime instructions
mermaid --system-prompt-file ./prompt.md         # Replace the default prompt for one run
mermaid list                                    # List available models across providers
mermaid doctor                                  # First-run readiness check
mermaid status                                  # Lower-level Ollama, MCP, and provider config
mermaid update                                  # Update to the latest release (or use brew/scoop)
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
| Ctrl+D | Quit when the input box is empty (auto-saves the session) |
| Ctrl+B | While tools are running, send the foreground command to the background (it keeps running as a `/processes` entry) |
| Alt+T | Cycle reasoning level: `None → Minimal → Low → Medium → High → XHigh → Max → None` |
| Shift+Tab | Cycle safety mode: `read_only → ask → auto → full_access → read_only` (session-scoped) |
| Ctrl+V | Paste image or text from clipboard |
| Ctrl+Click | Open image from chat history |
| Drag | Select chat text (highlights; does not copy) |
| Ctrl+Shift+C | Copy the selected chat text to the clipboard |
| Shift+Drag | Native terminal selection (bypasses Mermaid's mouse capture — useful for selecting across the whole window, including the input box and status bar) |
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

Durable memory:

- `/memory` (alias `/memories`) — list the durable facts Mermaid has saved across sessions
- `/remember <fact>` — save a fact to durable memory
- `/forget <name>` — delete a saved memory by name
- `/consolidate-memory` (aliases `/memory-consolidate`, `/prune-memory`) — merge duplicates and prune stale memories

Safety and recovery:

- `/safety [read_only|ask|auto|full_access]` (alias `/permission`) — show or set the session safety mode; Shift+Tab cycles it
- `/approvals`, `/approve <id>`, `/deny <id>`
- `/checkpoint <path...>`, `/checkpoints`, `/restore <id>`

Integrations:

- `/plugins`, `/cloud-setup`

Advanced runtime:

- `/tasks`, `/task <id>`, `/pause <id>`, `/resume <id>`
- `/processes`, `/logs <id>`, `/stop <id>`, `/restart <id>`, `/open <target>`, `/ports`

Reasoning choices persist per-model: set `/reasoning high` on one model and `/reasoning low` on another, and each is remembered independently across sessions.

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
| `memory` | Manage durable cross-session memory (remember / update / forget facts; project, shared, or global scope) |
| `web_search` | Search the web (zero-config: managed local SearXNG, or Ollama Cloud) |
| `web_fetch` | Fetch a URL as markdown (native in-process by default, no key) |
| `agent` | Spawn autonomous sub-agent for parallel tasks |
| `screenshot` | Capture the screen (fullscreen, focused window, monitor, region, or window by title) |
| `list_windows` | List visible window titles (X11-only discovery for window-mode screenshots) |
| `click` | Click at screen coordinates (auto-screenshot after) |
| `type_text` | Type text at cursor position (auto-screenshot after) |
| `press_key` | Press key combos (ctrl+s, alt+tab, etc.) |
| `scroll` | Scroll up or down |
| `mouse_move` | Move mouse cursor without clicking |

MCP servers contribute additional tools under the `mcp__<server>__<tool>` prefix when configured. `web_fetch` (native) and `web_search` (see [Web tool backends](#web-tool-backends)) are both registered by default with no configuration. Computer-use tools are advertised only in interactive TUI sessions when a usable GUI backend is detected.

### Web tool backends

The web tools work out of the box with no configuration, and are backend-pluggable under `[web]`:

```toml
[web]
fetch_backend = "native"    # "native" (default, in-process, no key) or "ollama"
search_backend = "auto"     # "auto" (default) | "ollama" | "searxng"
searxng_url = "http://localhost:8080"
```

- **`web_fetch` defaults to `native`**: it fetches the URL directly from your machine and converts the HTML to markdown — no API key, no third party. Set `fetch_backend = "ollama"` to route through Ollama Cloud's server-side fetch instead (handles JS-heavy pages and bot-walls better; needs `OLLAMA_API_KEY`).
- **`web_search` defaults to `auto`**, which just works with zero setup: if `OLLAMA_API_KEY` is set it uses Ollama Cloud; otherwise mermaid **auto-starts and manages a local [SearXNG](https://github.com/searxng/searxng) container** (via podman or docker) on your first search and tears it down when it exits — you install and configure nothing. The first search pulls the SearXNG image once; after that startup is a few seconds. Force a backend with `search_backend = "ollama"` (Ollama Cloud) or `"searxng"` (your own instance at `searxng_url`, which must have `json` in its `search.formats`).

## Project Instructions

Create an `AGENTS.md` (the cross-tool open standard) and/or a `MERMAID.md` (mermaid-specific) at your project root with conventions, tool versions, naming patterns, and run commands. Both are loaded from the nearest matching directory — `AGENTS.md` first, then `MERMAID.md`, so MERMAID.md overrides on conflict. They auto-reload when the files change (one `stat` per turn, no filesystem watcher). The walk stops at the `.git` root or `$HOME`.

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

The CLI can inspect and manage the same store with `mermaid tasks`, `mermaid task <id>`, `mermaid cancel <task-id>`, `mermaid approvals`, `mermaid approve <id>`, `mermaid deny <id>`, `mermaid tool-runs`, `mermaid checkpoints`, `mermaid restore <id>`, `mermaid plugin list`, `mermaid plugin install <path-or-github>`, `mermaid plugin audit <path>`, `mermaid models`, `mermaid model-info <model>`, `mermaid processes`, `mermaid logs <process>`, `mermaid stop <process>`, `mermaid restart <process>`, `mermaid open <target>`, `mermaid ports`, `mermaid pair`, and `mermaid daemon`. Installing a plugin from a Git URL (rather than a local path) requires an explicit full URL and `MERMAID_ALLOW_PLUGIN_FETCH=1`, since fetching and later running remote plugin code is a privileged operation.

Daemon `run` requests are queued and executed by a scheduler bounded by `[daemon] max_concurrent_tasks` (default 1, so a burst of background tasks never runs more than one agent loop against your GPU at a time; raise it for cloud-provider workloads). Requests may carry `"priority": "low"|"normal"|"high"` — higher priority claims first, FIFO within a level — and the queue is durable across daemon restarts. Cancel a queued or running task with `mermaid cancel <task-id>`; running tasks unwind gracefully (the same teardown as Esc in the TUI). `[daemon] task_timeout_minutes` bounds each task's wall clock.

On Linux, install a per-user systemd unit with `mermaid daemon install --start`. The installer writes `~/.config/systemd/user/mermaidd.service`, points `ExecStart` at the discovered `mermaidd` binary, reloads systemd's user manager, and optionally enables/starts the service. Use `mermaid daemon status`, `mermaid daemon logs [-f]`, `mermaid daemon restart`, `mermaid daemon stop`, `mermaid daemon uninstall`, or `mermaid daemon print-unit` for day-to-day service management. Set `MERMAID_DAEMON_BIN=/absolute/path/to/mermaidd` before installing if the background-service binary is not next to `mermaid` or on `PATH`.

On Windows, `mermaidd.exe` serves the same JSONL control surface over a named pipe at `\\.\pipe\mermaidd-<your-user-SID>` instead of a Unix socket. The pipe carries an explicit owner-only security descriptor (LocalSystem + your user, nothing else) and rejects remote pipe clients, mirroring the `0600` socket + peer-uid check on Unix; the same pairing-token rules apply on top. There is no service installer yet — start `mermaidd.exe` directly or wire it into Task Scheduler; `mermaid daemon install` remains Linux/systemd-only.

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
# Start `ollama serve` automatically when the local server isn't running
# (loopback hosts only; the revived server binds to exactly this host:port).
# Only paths that USE Ollama start it (chat, the startup model check); the
# read-only verbs (`mermaid list` / `models` / `status` / `doctor`) observe
# and never start anything. Disable here — or with
# MERMAID_OLLAMA_AUTOSTART=0 in the environment — if you manage Ollama
# yourself (custom bind address, containers, CI).
auto_start = true
# cloud_api_key = "your-key"  # for :cloud models
# num_gpu = 10
# num_thread = 8
# num_ctx = 8192
# numa = false

[safety]
# Approval policy. Default is "ask": prompt before mutations / shell / network
# actions. "auto" runs an LLM classifier that vets each borderline action
# against your stated intent — aligned actions run automatically, risky ones
# escalate to an approval prompt. "full_access" auto-runs everything (the
# legacy default); "read_only" blocks all mutations but keeps reads flowing —
# including web_search / web_fetch, which are reads of the public web (the
# SSRF guard on internal hosts applies in every mode). Change it live with
# Shift+Tab or `/safety <mode>` (session-scoped; this value is the persistent
# default each session starts from).
mode = "ask"
checkpoint_on_mutation = true
# Model the "auto" classifier uses to vet actions. Omit to vet with the
# session's active model; set a smaller/faster model to cut latency and cost.
# auto_classifier_model = "<provider>/<small-fast-model>"

[non_interactive]
# Run behavior is controlled by CLI flags:
#   mermaid run "prompt" --format json --max-tokens 4096 --no-execute
# These fields remain in the schema for compatibility but are not the
# source of truth for `mermaid run`.
output_format = "text"
max_tokens = 4096
no_execute = false

# Durable agent memory (the `memory` tool, the always-loaded index, and
# /remember & friends). On by default.
[memory]
enabled = true
# index_cap_bytes = 8000   # byte cap on the always-loaded memory index

[compaction]
# Cap on consecutive auto compact-and-continue recoveries after a
# context-window truncation, before the run stops and shows the manual
# levers (`/context max`, `/context offload on`). 0 = uncapped.
max_truncation_recoveries = 3

# Subagents (the `agent` tool). Built-in types: `general` (full tool access
# at your safety mode) and `explore` (read-only reconnaissance). Define more
# below; a custom name shadows a built-in, so `[agents.types.explore]`
# retunes the built-in. Callers pick a type with the tool's `type` arg,
# override the model per call with `model`, and continue a prior child with
# `agent_id` (from the `[agent_id: …]` trailer on each result).
[agents]
# Wall-clock ceiling per subagent drive, in seconds. 0 = built-in default
# (1200 = 20 minutes).
timeout_secs = 1200

# Example user-defined type. Every field is optional.
# [agents.types.scout]
# tools = ["read_file", "execute_command"]  # omit for the full child set
# safety = "read_only"    # ceiling — the child never runs looser than this
# preamble = "You are a scout: find and report, fast."
# model = "ollama/qwen3:8b"   # default model for this type; per-call `model` wins

# Per-model reasoning preferences (remembered across sessions)
[reasoning_per_model]
# "<provider>/<model>" = "high"
"ollama/qwen3-coder:30b" = "low"

# Optional agent/plugin model profiles. A request for `--model fast` or
# `--model profile:fast` resolves through this table when present.
[model_profiles]
fast = "ollama/qwen3-coder:14b"
# large-context = "openai/<model>"
# tool-strong = "anthropic/<model>"
# vision = "gemini/<model>"
# cheap = "groq/<model>"

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

Set the appropriate environment variable (or override via `[providers.<name>].api_key_env` in config). Model names are whatever the vendor currently ships — Mermaid passes them through, so use any model id from your provider's docs in the format below:

| Provider | Env var | Model format |
|----------|---------|--------------|
| Anthropic | `ANTHROPIC_API_KEY` | `anthropic/<model>` |
| Google Gemini | `GOOGLE_API_KEY` (`GEMINI_API_KEY` legacy fallback) | `gemini/<model>` |
| OpenAI | `OPENAI_API_KEY` | `openai/<model>` |
| Groq | `GROQ_API_KEY` | `groq/<model>` |
| OpenRouter | `OPENROUTER_API_KEY` | `openrouter/<vendor>/<model>` |
| Cerebras | `CEREBRAS_API_KEY` | `cerebras/<model>` |
| DeepInfra | `DEEPINFRA_API_KEY` | `deepinfra/<vendor>/<model>` |
| Together | `TOGETHER_API_KEY` | `together/<vendor>/<model>` |
| Ollama Cloud | `OLLAMA_API_KEY` | `ollama/<model>:cloud` |

Ollama Cloud models authenticate via `OLLAMA_API_KEY`. The web tools don't require it: `web_fetch` is native, and `web_search` defaults to `auto` — Ollama Cloud when the key is set, otherwise a mermaid-managed local SearXNG (see [Web tool backends](#web-tool-backends)). Use `mermaid cloud-setup` from your shell to set the key for cloud models; `/cloud-setup` in the TUI points back to that shell command.

## License

MIT OR Apache-2.0

Built with [Ratatui](https://github.com/ratatui-org/ratatui) and [Ollama](https://ollama.com). Inspired by [Aider](https://github.com/paul-gauthier/aider) and [Claude Code](https://github.com/anthropics/claude-code).
