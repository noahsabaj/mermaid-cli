# Mermaid

An open-source AI coding assistant with computer use for the terminal. Multi-provider — Ollama (local), Anthropic, Gemini, Meta, OpenAI, Groq, OpenRouter, and any OpenAI-compatible endpoint — with native tool calling, subagents, computer-use tools, and a clean TUI.

## Features

- **Multi-Provider** — Ollama (local/cloud), Anthropic Claude, Google Gemini, Meta Muse, OpenAI, Groq, OpenRouter, Cerebras, DeepInfra, Together, NVIDIA NIM, Cloudflare Workers AI, plus fully-custom OpenAI-compatible endpoints
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
- **Rewind & Fork** — double-Esc when idle picks an earlier message and forks the session there (new session id, lineage recorded, original untouched); edit the pre-filled prompt and resend to branch the timeline. Files are not rewound, but checkpoints taken after the fork point are surfaced so `/restore <id>` can roll them back
- **Context Compaction** — automatic checkpoint-and-continue when the window fills (or the model truncates mid-run); manual `/compact [focus]` for handoffs
- **Record & Replay** — `--record` captures every reducer input; `--replay` reconstructs the session offline, deterministically, with a built-in purity check
- **@-Mentions** — type `@` in the composer to fuzzy-pick a project file (gitignore-aware); the path lands in your prompt as text the agent reads with its tools
- **Message Queuing & Mid-Run Steering** — type while the agent works; queued messages are delivered at the next tool boundary within the run (the model course-corrects mid-task), or at run end when no boundary follows
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
mermaid --model nvidia/z-ai/glm-5.2             # NVIDIA NIM (requires NVIDIA_API_KEY)
mermaid --model cloudflare/@cf/zai-org/glm-5.2  # Cloudflare Workers AI (CLOUDFLARE_API_TOKEN + CLOUDFLARE_ACCOUNT_ID)
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
mermaid run "explain main.rs" -f json           # JSON output (single typed object)
mermaid run "explain main.rs" -f ndjson         # Streaming NDJSON events (SDK/scripting)
mermaid run "summarize this repo" --output-schema schema.json -f json
                                                # Structured output: the agentic loop runs
                                                # normally, then one formatting turn reshapes
                                                # the answer to your JSON Schema (validated
                                                # client-side; result carries structured_output).
                                                # Native constrained output on OpenAI-compatible
                                                # providers, Gemini, and Ollama; prompt-driven +
                                                # validation on Anthropic.
mermaid --resume <id> run "and now the tests"   # Continue a saved session headless (id from
                                                #   ndjson session_started/result, json result,
                                                #   or the `session:` line on stderr)
mermaid --continue run "keep going"             # Resume this directory's most recent session
mermaid --no-network run "audit the code"       # Deny network to shell commands (Linux seccomp)
mermaid --sandbox run "refactor this"           # Also confine writes to the project (Linux Landlock)
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
| Esc Esc | (idle) Rewind: fork the session at an earlier message — original preserved, composer pre-filled |
| Ctrl+C | Quit (auto-saves the session) |
| Ctrl+D | Quit when the input box is empty (auto-saves the session) |
| Ctrl+B | While tools are running, send the foreground command to the background (it keeps running as a `/processes` entry) |
| Alt+T | Cycle reasoning level: `None → Minimal → Low → Medium → High → XHigh → Max → None` |
| Shift+Tab | Cycle safety mode: `read_only → ask → auto → full_access → read_only` (session-scoped) |
| Ctrl+V | Paste image or text from clipboard |
| Ctrl+O | Compose the prompt in `$VISUAL`/`$EDITOR` (TUI suspends, resumes on save-quit) |
| Ctrl+Click | Open image from chat history |
| Drag | Select chat text (highlights; does not copy) |
| Ctrl+Shift+C | Copy the selected chat text to the clipboard |
| Shift+Drag | Native terminal selection (bypasses Mermaid's mouse capture — useful for selecting across the whole window, including the input box and status bar) |
| `/` | Open slash-command palette (filter-as-you-type) |
| `@` | Open the fuzzy file picker (at the start of a word); type to filter, Tab/Enter inserts `@path`, Esc dismisses |
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
- `/theme [dark|light]` — switch the color theme (persisted); `NO_COLOR` disables colors entirely
- `/editor` — compose the prompt in `$VISUAL`/`$EDITOR` (Ctrl+O keeps the current draft)
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
| `apply_patch` | Multi-hunk, context-anchored file edits with a diff (fuzzy-tolerant) |
| `delete_file` | Delete files (timestamped backup) |
| `create_directory` | Create directories |
| `execute_command` | Run shell commands; background mode registers PID/log/URL metadata for GUI apps and dev servers |
| `memory` | Manage durable cross-session memory (remember / update / forget facts; project, shared, or global scope) |
| `web_search` | Search the web (zero-config: managed local SearXNG, or Ollama Cloud) |
| `web_fetch` | Fetch a URL as markdown (native in-process by default, no key); optional `pattern` finds case-insensitive substring matches with surrounding context instead of returning the whole page |
| `agent` | Spawn autonomous sub-agent for parallel tasks |
| `screenshot` | Capture the screen (fullscreen, focused window, monitor, region, or window by title) |
| `list_windows` | List visible window titles (X11-only discovery for window-mode screenshots) |
| `click` | Click at screen coordinates (auto-screenshot after) |
| `type_text` | Type text at cursor position (auto-screenshot after) |
| `press_key` | Press key combos (ctrl+s, alt+tab, etc.) |
| `scroll` | Scroll up or down |
| `mouse_move` | Move mouse cursor without clicking |

MCP servers contribute additional tools under the `mcp__<server>__<tool>` prefix when configured. Names and schemas are sanitized to provider-safe form at startup (charset `[A-Za-z0-9_-]`, 64-char cap, `$ref` inlining and other schema normalization); `enabled_tools`/`disabled_tools` filters keep matching the RAW tool names the server itself advertises. `web_fetch` (native) and `web_search` (see [Web tool backends](#web-tool-backends)) are both registered by default with no configuration. Computer-use tools are advertised only in interactive TUI sessions when a usable GUI backend is detected.

By default MCP tools are **deferred**: instead of advertising every server's tools on every request, the model gets one `tool_search` tool that searches deferred tool names/descriptions and promotes matches to direct advertisement for the rest of the session — deferred schemas don't count against `/context` until promoted. Servers start concurrently at launch, each bounded by a 60-second timeout, and report ready/errored individually. Opt out globally with `mcp_defer_tools = false` at the top level of config, or per server with `defer = false` on its `[mcp_servers.<name>]` entry.

### Web tool backends

The web tools work out of the box with no configuration, and are backend-pluggable under `[web]`:

```toml
[web]
fetch_backend = "native"    # "native" (default, in-process, no key) or "ollama"
search_backend = "auto"     # "auto" (default) | "ollama" | "searxng"
searxng_url = "http://localhost:8080"
```

- **`web_fetch` defaults to `native`**: it fetches the URL directly from your machine and converts the HTML to markdown — no API key, no third party. Set `fetch_backend = "ollama"` to route through Ollama Cloud's server-side fetch instead (handles JS-heavy pages and bot-walls better; needs `OLLAMA_API_KEY`).
- **`web_search` defaults to `auto`**, which just works with zero setup: if `OLLAMA_API_KEY` is set it uses Ollama Cloud; otherwise mermaid **downloads and runs a self-contained local [SearXNG](https://github.com/searxng/searxng) bundle** on your first search and tears it down when it exits — no Docker, no Podman, nothing to install. The bundle (a portable Python plus the [Granian](https://github.com/emmett-framework/granian) server and SearXNG) is fetched once from [mermaid-searxng](https://github.com/noahsabaj/mermaid-searxng), sha256-verified, and cached under your data dir; after that startup is a couple of seconds. Force a backend with `search_backend = "ollama"` (Ollama Cloud) or `"searxng"` (your own instance at `searxng_url`, which must have `json` in its `search.formats`).

## Project Instructions

Create an `AGENTS.md` (the cross-tool open standard) and/or a `MERMAID.md` (mermaid-specific) at your project root with conventions, tool versions, naming patterns, and run commands. Both are loaded from the nearest matching directory — `AGENTS.md` first, then `MERMAID.md`, so MERMAID.md overrides on conflict. They auto-reload when the files change (one `stat` per turn, no filesystem watcher). The walk stops at the `.git` root or `$HOME`.

## Skills

Skills are task-specific playbooks the agent loads on demand (progressive disclosure). Each skill is a directory holding a `SKILL.md` with Claude Code-compatible frontmatter:

```markdown
---
name: deploy
description: Cut a release — version bump, changelog, tag, publish
---

Step-by-step instructions the model follows when this skill applies...
```

At startup mermaid discovers skills from three places — project (`<git-root>/.mermaid/skills/<name>/SKILL.md`, shared with your team), user (`~/.config/mermaid/skills/<name>/SKILL.md`, all your projects), and enabled plugins (declared in the plugin manifest's `skills` list) — and injects a compact index (name, description, path) into the system prompt. Same-named skills dedupe with project > user > plugin precedence. When a task matches a description, the model reads the full `SKILL.md` with `read_file`, so activation is visible in the transcript and idle skills cost almost nothing per request. The index caps at 64 skills / 8 KiB; edits to skill files are picked up on the next session start. `mermaid doctor` reports the discovered count.

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

Attach to a running (or still-queued) daemon task's live event stream with `mermaid task <id> --follow`: an ack line, then NDJSON `RunEvent` lines (text deltas, tool starts/finishes, errors) until the terminal `result`, at which point the command exits. The daemon's whole control protocol is a typed request enum (`subscribe_task` included) — unknown commands and malformed fields answer with a precise parse error instead of a silent drop.

The CLI can inspect and manage the same store with `mermaid tasks`, `mermaid task <id>`, `mermaid cancel <task-id>`, `mermaid approvals`, `mermaid approve <id>`, `mermaid deny <id>`, `mermaid tool-runs`, `mermaid checkpoints`, `mermaid restore <id>`, `mermaid plugin list`, `mermaid plugin install <path-or-github>`, `mermaid plugin audit <path>`, `mermaid models`, `mermaid model-info <model>`, `mermaid processes`, `mermaid logs <process>`, `mermaid stop <process>`, `mermaid restart <process>`, `mermaid open <target>`, `mermaid ports`, `mermaid pair`, and `mermaid daemon`. Installing a plugin from a Git URL (rather than a local path) requires an explicit full URL and `MERMAID_ALLOW_PLUGIN_FETCH=1`, since fetching and later running remote plugin code is a privileged operation.

Daemon `run` requests are queued and executed by a scheduler bounded by `[daemon] max_concurrent_tasks` (default 1, so a burst of background tasks never runs more than one agent loop against your GPU at a time; raise it for cloud-provider workloads). Requests may carry `"priority": "low"|"normal"|"high"` — higher priority claims first, FIFO within a level — and the queue is durable across daemon restarts. Cancel a queued or running task with `mermaid cancel <task-id>`; running tasks unwind gracefully (the same teardown as Esc in the TUI). `[daemon] task_timeout_minutes` bounds each task's wall clock. Finished tasks and the daemon's training-signal `outcomes` are garbage-collected on startup by `[daemon] retention_days` (default 30) and `[daemon] outcomes_retention_days` (default 180, longer so training history outlives the task rows it came from).

On Linux, install a per-user systemd unit with `mermaid daemon install --start`. The installer writes `~/.config/systemd/user/mermaidd.service`, points `ExecStart` at the discovered `mermaidd` binary, reloads systemd's user manager, and optionally enables/starts the service. Use `mermaid daemon status`, `mermaid daemon logs [-f]`, `mermaid daemon restart`, `mermaid daemon stop`, `mermaid daemon uninstall`, or `mermaid daemon print-unit` for day-to-day service management. Set `MERMAID_DAEMON_BIN=/absolute/path/to/mermaidd` before installing if the background-service binary is not next to `mermaid` or on `PATH`.

### Logging and diagnostics

Mermaid logs to `~/.mermaid/mermaid.log` (owner-only, secret-redacted, 10 MiB rotation), scoped by `RUST_LOG` / `--verbose`. Independently, an always-on in-memory ring captures the last ~2000 trace events from mermaid's own crates at TRACE level (dependencies capped at INFO) regardless of `RUST_LOG` — so a bug that already happened is diagnosable without reproducing it under elevated logging.

`mermaid feedback` writes a local diagnostic bundle to the current directory (mode 0600): the doctor report, a names-and-booleans config summary (provider keys are reported as present/absent — never values), recent session ids, the trace ring, and the log tail. `--stdout` prints instead; `--format json` emits machine-readable output. **Nothing is uploaded** — the ring redacts secrets at capture, the log at write, and the whole rendered bundle passes a final redaction sweep; review the file before sharing it in a bug report.

On Windows, `mermaidd.exe` serves the same JSONL control surface over a named pipe at `\\.\pipe\mermaidd-<your-user-SID>` instead of a Unix socket. The pipe carries an explicit owner-only security descriptor (LocalSystem + your user, nothing else) and rejects remote pipe clients, mirroring the `0600` socket + peer-uid check on Unix; the same pairing-token rules apply on top. There is no service installer yet — start `mermaidd.exe` directly or wire it into Task Scheduler; `mermaid daemon install` remains Linux/systemd-only.

Release builds keep the existing `.tar.gz`/`.zip` archives and add Linux `.deb`/`.rpm` artifacts for x86_64 and aarch64. The distro packages install `mermaid`, `mermaidd`, docs, and a reference systemd user unit at `/usr/lib/systemd/user/mermaidd.service`; they do not auto-enable or start the daemon.

### Plugin hooks

Enabled plugins' hooks receive lifecycle events as JSON on stdin (`MERMAID_HOOK_EVENT` names the
event). Most events are observe-only, but on **`before_tool_use`** a hook can gate the call by
printing one JSON object on stdout (Claude Code-compatible), or deny by exiting with code 2
(stderr becomes the reason):

```sh
#!/bin/sh
# deny-etc-writes: block any tool call whose arguments mention /etc
payload=$(cat)
case "$payload" in
  *'/etc'*) cat <<'EOF'
{"hookSpecificOutput": {"hookEventName": "PreToolUse",
  "permissionDecision": "deny",
  "permissionDecisionReason": "writes under /etc are not allowed here"}}
EOF
  ;;
esac
```

The response may also carry `updatedInput` (a full replacement tool-arguments object — still
vetted by the safety policy exactly like the original) and `additionalContext` (a string surfaced
to the model on its next request). The legacy `{"decision": "block", "reason": "..."}` shape is
accepted too. Across plugins: the first deny wins, the last `updatedInput` wins, and context
strings concatenate. Failure semantics are asymmetric by design: an explicit deny always denies,
while infrastructure failures (unparseable output, a timeout, a crash) log a warning and allow —
a buggy hook must not lock you out of every tool call.

### Plugin bundles: MCP servers, commands, agent types

Beyond skills and hooks, an enabled plugin can contribute three more asset kinds, each a list of
plugin-relative paths in `plugin.toml`:

- **`mcp = ["servers.toml"]`** — each file's `[servers.<name>]` tables are MCP server configs
  (same shape as `[mcp_servers.<name>]` in your config). They start with your own servers at
  session startup and flow through tool deferral like any other server. A `./`-relative
  `command` resolves inside the plugin directory (containment enforced); anything else is
  PATH-looked-up. A same-named server in your config wins with a warning. Enabling a plugin that
  declares MCP servers grants command execution — the same trust boundary as hooks.
- **`prompts = ["deploy.md"]`** — markdown prompt commands with the skills frontmatter dialect
  (`name:`/`description:`; the name falls back to the file stem, validated `[a-z0-9-]+`). They
  appear in the `/` palette tagged `(plugin:<name>)` and in `/help`; running `/deploy prod`
  substitutes `prod` for `$ARGUMENTS` (or appends the args when the token is absent) and submits
  the expansion as a normal prompt — the transcript shows the expanded text, so recordings
  replay without the plugin. Built-in commands always win over a same-named prompt.
- **`agents = ["types.toml"]`** — each file's `[types.<name>]` tables are agent types (same
  shape as `[agents.types.<name>]`): the model can spawn them via the `agent` tool. Your config's
  same-named type wins with a warning.

Like skills, bundle changes are picked up on the next session start.

## Configuration

Config file: `~/.config/mermaid/config.toml` (Linux) or platform equivalent via `directories` crate.

Configuration is assembled from layers, later layers winning key-by-key through one recursive
TOML deep-merge (tables merge; scalars and arrays replace):

1. built-in defaults
2. the user config file above
3. the project config: `<git-root>/.mermaid/config.toml`, located from the working directory
   (`-p /repo` adopts that repo's project config)
4. session flags: repeatable `-c key.path=value` plus the dedicated flags (`--no-network`,
   `--confine-fs`, `--sandbox`, `run --max-tokens`, `run --allow-untrusted-tools`), with a
   dedicated flag beating a contradictory `-c`

Unknown-key warnings name the layer they came from, and in-app settings changes (`/model`,
Alt+T, `/context`, `mermaid add`) rewrite only their own keys in the user file — unrecognized
keys in the file survive, and defaults are never frozen in.

### Project config

A repo can commit shared defaults in `.mermaid/config.toml` — model choice, profiles,
per-model reasoning, web/UX knobs. Loading needs no trust ceremony because safety is
structural:

- Only these top-level sections are honored: `default_model`, `model_aliases`,
  `reasoning_per_model`, `ollama`, `ollama_num_ctx_per_model`, `web`, `compaction`,
  `computer_use`, `memory`, `non_interactive`, and a `safety` subset. Anything else —
  `mcp_servers` (spawns commands), `providers` (redirects traffic/credentials), `agents`,
  `daemon` — is ignored with a warning, as are `web.searxng_url` and `ollama.host`/`port`
  (traffic-redirect vectors) inside otherwise-allowed tables.
- `safety.mode`, `safety.network`, and `safety.filesystem` are clamped tighten-only against
  your user config: a project can turn the sandbox on or drop to `read_only`, but can never
  loosen what you configured. Session flags (you, at the keyboard) still override everything.
- Startup prints `mermaid: using project config <path> (<n> keys)` whenever the layer
  contributes.

Run `mermaid init` to create a default config. Important fields in the current config schema:

```toml
# Last model picked via `--model` — used by bare `mermaid` on next start
last_used_model = "ollama/qwen3-coder:30b"

[default_model]
provider = "ollama"
name = "qwen3-coder:30b"
temperature = 0.7
max_tokens = 0        # 0 = auto (model-scaled output budget); positive = hard cap
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

[exec]
# Foreground commands run on a pseudo-terminal by default (Unix): the child
# sees a real tty, so spinner/progress tools behave and /dev/tty resolves to
# the captured pty instead of writing over the TUI. ANSI escapes are stripped
# from what the model sees; a child that reads stdin hangs to its timeout
# (use mode="background" for interactive daemons). Set false to use pipes.
# pty = true

[ui]
# TUI color theme: "dark" (default) or "light". Switch live with
# `/theme dark|light` (persists here). Setting the NO_COLOR environment
# variable (any non-empty value) disables colors entirely, regardless of
# this value.
theme = "dark"

[non_interactive]
# Run behavior is controlled by CLI flags:
#   mermaid run "prompt" --format json --max-tokens 4096 --no-execute
# These fields remain in the schema for compatibility but are not the
# source of truth for `mermaid run`.
output_format = "text"
max_tokens = 0        # 0 = auto (model-scaled); positive = hard cap
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

# Optional named config overlays selected per invocation with
# `--profile <name>`. A profile's values beat this file's top-level values
# but lose to a repo's project config and to `-c` overrides.
#   mermaid --profile work doctor
# [profiles.work.default_model]
# temperature = 0.2
# [profiles.work]
# last_used_model = "anthropic/<model>"

# Optional model-id aliases. A request for `--model fast` or
# `--model alias:fast` resolves through this table when present.
[model_aliases]
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

[providers.meta]
# api_key_env = "MY_META_KEY"  # default: MODEL_API_KEY
# base_url = "https://api.meta.ai/v1"

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
| Meta | `MODEL_API_KEY` | `meta/<model>` |
| OpenAI | `OPENAI_API_KEY` | `openai/<model>` |
| Groq | `GROQ_API_KEY` | `groq/<model>` |
| OpenRouter | `OPENROUTER_API_KEY` | `openrouter/<vendor>/<model>` |
| Cerebras | `CEREBRAS_API_KEY` | `cerebras/<model>` |
| DeepInfra | `DEEPINFRA_API_KEY` | `deepinfra/<vendor>/<model>` |
| Together | `TOGETHER_API_KEY` | `together/<vendor>/<model>` |
| NVIDIA NIM | `NVIDIA_API_KEY` | `nvidia/<vendor>/<model>` |
| Cloudflare Workers AI | `CLOUDFLARE_API_TOKEN` + `CLOUDFLARE_ACCOUNT_ID` | `cloudflare/@cf/<vendor>/<model>` |
| Ollama Cloud | `OLLAMA_API_KEY` | `ollama/<model>:cloud` |

Meta Muse Spark uses Meta's Responses API so encrypted reasoning state survives
Mermaid's model/tool loop without Meta retaining the response server-side.
Mermaid requests automatic reasoning summaries for the existing reasoning panel
and keeps the encrypted continuation only in private local session data.

```bash
export MODEL_API_KEY="your-meta-api-key"
mermaid --model meta/muse-spark-1.1 --reasoning high
```

Cloudflare Workers AI needs both `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` (the account id is spliced into the account-scoped endpoint URL); alternatively set `[providers.cloudflare].base_url` to a full account-scoped URL or an AI Gateway endpoint. Example model: `cloudflare/@cf/zai-org/glm-5.2`.

API keys resolve in strict precedence order: **environment variables always win**; the OS keyring (macOS Keychain, Windows Credential Manager, Linux Secret Service) fills the gap when no env var is set. Store a key with `mermaid login <provider>` (hidden input; `mermaid login` alone lists every provider's key status) and remove it with `mermaid logout <provider>`. A per-provider `api_key_env` override in config is authoritative — when set, neither the default env var nor the keyring is consulted. On headless Linux without a Secret Service the keyring quietly reports no keys (env vars keep working); set `MERMAID_NO_KEYRING=1` to disable keyring lookups entirely. `doctor`, `mermaid login`, and `mermaid feedback` report each key's source as `env`, `keyring`, or `none` — never the value.

When a cloud provider call fails, the error shown in the TUI ends with a `(request-id: ..., cf-ray: ...)` line when the provider's response carried those headers — quote it when reporting the failure to the provider (or in a Mermaid issue), it lets them find the exact request.

Ollama Cloud models authenticate via `OLLAMA_API_KEY`. The web tools don't require it: `web_fetch` is native, and `web_search` defaults to `auto` — Ollama Cloud when the key is set, otherwise a mermaid-managed local SearXNG (see [Web tool backends](#web-tool-backends)). Use `mermaid cloud-setup` from your shell to set the key for cloud models; `/cloud-setup` in the TUI points back to that shell command.

## Development

Contributor guardrails live in [`AGENTS.md`](AGENTS.md) (MVU purity, the no-emoji
rule, no back-compat shims). The one-command pre-PR gate — exactly what CI runs —
is:

```
just check    # cargo fmt --check + clippy -D warnings + cargo nextest run
```

Or run the pieces directly if you don't have [`just`](https://github.com/casey/just)
/ [`cargo-nextest`](https://nexte.st):

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace   # or: cargo test --workspace
```

CI additionally runs two dependency-free source guards (`.github/scripts/`): no
emoji/pictographs in source, and `src/domain` stays a pure MVU core (no I/O, no
wall clock).

The TUI has a snapshot suite (`src/render/snapshots.rs`, unix-only) that pins
full rendered frames for curated scenes at 80x24 and 120x40. It runs as part of
the normal test suite; a mismatch panics with a diff and writes a gitignored
`.snap.new` sibling. Review and accept deliberate visual changes with
`just snapshots` (`cargo insta review`) and commit the updated `.snap` files in
the same PR as the style change.

## License

MIT OR Apache-2.0

Built with [Ratatui](https://github.com/ratatui-org/ratatui) and [Ollama](https://ollama.com). Inspired by [Aider](https://github.com/paul-gauthier/aider) and [Claude Code](https://github.com/anthropics/claude-code).
