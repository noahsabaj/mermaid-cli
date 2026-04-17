# Mermaid

An open-source AI coding assistant with computer use for the terminal. Multi-provider — Ollama (local), Anthropic, Gemini, OpenAI, Groq, OpenRouter, and any OpenAI-compatible endpoint — with native tool calling, subagents, desktop control, and a clean TUI.

## Features

- **Multi-Provider** — Ollama (local/cloud), Anthropic Claude, Google Gemini, OpenAI, Groq, OpenRouter, Cerebras, DeepInfra, Together, plus fully-custom OpenAI-compatible endpoints
- **Native Tool Calling** — read, write, edit, execute commands, search the web, manage MCP servers
- **Computer Use** — screenshot, click, type, scroll — full desktop control via vision models
- **Subagents** — spawn parallel autonomous agents for independent tasks
- **Agent Loop** — model calls tools autonomously, sees results, and continues until done
- **Image Paste** — Ctrl+V to attach images for vision models (X11/Wayland/macOS/Windows)
- **Reasoning Levels** — seven tiers (`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`); cycle with Alt+T or set via `/reasoning`; persisted per-model
- **MERMAID.md** — auto-loaded project-level instructions; edits take effect on the next turn
- **MCP Servers** — stdio JSON-RPC client with a built-in registry of 16 popular servers (`mermaid add <name>`)
- **Session Persistence** — conversations auto-save and resume with `--continue`
- **Message Queuing** — type while the model generates, messages send in order
- **Non-Interactive Mode** — script with `mermaid run "prompt"` for CI/automation

## Quick Start

```bash
# Install from crates.io
cargo install mermaid-cli

# Or from source
git clone https://github.com/noahsabaj/mermaid-cli.git
cd mermaid-cli
cargo install --path .
```

Local inference requires [Ollama](https://ollama.com) (models auto-pull if not found locally). Cloud providers are optional — see [Remote Providers](#remote-providers) below.

### Computer Use Dependencies (optional)

For desktop control via screenshot/click/type tools:

```bash
# Linux / X11
sudo apt install scrot xdotool

# Linux / Wayland
sudo apt install grim ydotool wtype

# Screenshot downscaling (optional, for high-res displays)
sudo apt install imagemagick
```

macOS and Windows are supported through `screencapture`/`pngpaste` and PowerShell respectively. See `src/agents/computer_use.rs` for the full platform matrix.

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
mermaid list                                    # List available models across providers
mermaid status                                  # Check Ollama, MCP, and provider config
mermaid init                                    # Create default config file
mermaid run "fix the tests"                     # Non-interactive mode
mermaid run "explain main.rs" -f json           # JSON output
mermaid add <name>                              # Add an MCP server (e.g., context7, git)
mermaid remove <name>                           # Remove a configured MCP server
mermaid mcp                                     # List configured MCP servers
```

`mermaid add <name>` resolves the name through a built-in registry of 16 popular MCP servers (context7, playwright, memory, git, fetch, time, filesystem, notion, slack, postgres, brave-search, supabase, perplexity, docker, sequential-thinking, everything), prompts for any required env vars, validates by spawning the server, and saves it to `~/.config/mermaid/config.toml`.

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| Enter | Send message (or queue while the model is generating) |
| Esc | Stop generation / clear input / dismiss command palette |
| Ctrl+C | Quit (auto-saves the session) |
| Alt+T | Cycle reasoning level: `None → Low → Medium → High → Max → None` |
| Ctrl+V | Paste image or text from clipboard |
| Ctrl+O | Preview attached image |
| Ctrl+Click | Open image from chat history |
| `/` | Open slash-command palette (filter-as-you-type) |
| Tab | In palette: complete highlighted command name |
| Up/Down | Navigate input history; palette navigation; scroll chat |
| Page Up/Down | Scroll chat |
| Mouse Wheel | Scroll chat |

## Slash Commands

Type `/` to open the command palette (shows all commands with live filter); type `/<name>` to invoke directly.

| Command | Description |
|---------|-------------|
| `/model <name>` | Switch model; auto-pulls Ollama models if needed |
| `/reasoning <level>` | Set reasoning: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `/clear` | Clear chat history and model context for this session |
| `/save [name]` | Save the current conversation |
| `/load [id]` | Load a saved conversation by id |
| `/list` | List saved conversations |
| `/cloud-setup` | Show Ollama Cloud API-key setup instructions |
| `/help` (`/h`) | Show all commands |
| `/quit` (`/q`) | Exit |

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
| `execute_command` | Run any command — terminal, GUI apps, scripts, servers |
| `web_search` | Search the web (Ollama Cloud) |
| `web_fetch` | Fetch URL content as markdown (Ollama Cloud) |
| `agent` | Spawn autonomous sub-agent for parallel tasks |
| `screenshot` | Capture the screen (fullscreen, focused window, monitor, region, or window by title) |
| `list_windows` | List visible window titles (discovery for window-mode screenshots) |
| `click` | Click at screen coordinates (auto-screenshot after) |
| `type_text` | Type text at cursor position (auto-screenshot after) |
| `press_key` | Press key combos (ctrl+s, alt+tab, etc.) |
| `scroll` | Scroll up or down |
| `mouse_move` | Move mouse cursor without clicking |

MCP servers contribute additional tools under the `mcp__<server>__<tool>` prefix when configured.

## Project Instructions (MERMAID.md)

Create a `MERMAID.md` at your project root with conventions, tool versions, naming patterns, and run commands — Mermaid loads it automatically at session start and auto-reloads when the file changes (one `stat` per turn, no filesystem watcher). The walk stops at the `.git` root or `$HOME`.

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

## Configuration

Config file: `~/.config/mermaid/config.toml` (Linux) or platform equivalent via `directories` crate.

Run `mermaid init` to create a default config. Full surface:

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
# cloud_api_key = "your-key"  # for :cloud models + web_search/web_fetch
# num_gpu = 10
# num_ctx = 8192

[non_interactive]
output_format = "text"
max_tokens = 4096
no_execute = false

# Per-model reasoning preferences (remembered across sessions)
[reasoning_per_model]
"anthropic/claude-opus-4-7" = "high"
"ollama/qwen3-coder:30b" = "low"

# Remote providers — override env-var name, base URL, or extra headers
[providers.anthropic]
# api_key_env = "MY_ANTHROPIC_KEY"  # default: ANTHROPIC_API_KEY

[providers.gemini]
# api_key_env = "MY_GOOGLE_KEY"  # default: GOOGLE_API_KEY

[providers.groq]
# api_key_env = "MY_GROQ_KEY"    # default: GROQ_API_KEY

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

## Remote Providers

Set the appropriate environment variable (or override via `[providers.<name>].api_key_env` in config):

| Provider | Env var | Example model |
|----------|---------|---------------|
| Anthropic | `ANTHROPIC_API_KEY` | `anthropic/claude-opus-4-7` |
| Google Gemini | `GOOGLE_API_KEY` | `gemini/gemini-3.1-pro-preview` |
| OpenAI | `OPENAI_API_KEY` | `openai/gpt-5` |
| Groq | `GROQ_API_KEY` | `groq/qwen-qwq-32b` |
| OpenRouter | `OPENROUTER_API_KEY` | `openrouter/anthropic/claude-3.7-sonnet` |
| Cerebras | `CEREBRAS_API_KEY` | `cerebras/gpt-oss-120b` |
| DeepInfra | `DEEPINFRA_API_KEY` | `deepinfra/deepseek-ai/DeepSeek-R1` |
| Together | `TOGETHER_API_KEY` | `together/deepseek-ai/DeepSeek-R1` |
| Ollama Cloud | `OLLAMA_API_KEY` | `ollama/kimi-k2-thinking:cloud` |

Web search and web fetch tools require an Ollama Cloud API key — set `OLLAMA_API_KEY` or `cloud_api_key` under `[ollama]`. Use `/cloud-setup` in the TUI for the full instructions.

## License

MIT OR Apache-2.0

Built with [Ratatui](https://github.com/ratatui-org/ratatui) and [Ollama](https://ollama.com). Inspired by [Aider](https://github.com/paul-gauthier/aider) and [Claude Code](https://github.com/anthropics/claude-code).
