# Mermaid

An open-source AI coding assistant for the terminal. Local/cloud models via Ollama, native tool calling, and a clean TUI.

## Features

- **Local & Cloud Models** via Ollama - run locally or use cloud models with no hardware cost
- **Native Tool Calling** - read, write, edit, execute commands, search the web, manage git
- **Image Paste** - Ctrl+V to attach images for vision models (X11/Wayland)
- **Thinking Mode** - toggle extended reasoning with Alt+T
- **Session Persistence** - conversations auto-save and resume with `--continue`
- **Message Queuing** - type while the model generates, messages send in order

## Quick Start

```bash
# Install
cargo install mermaid-cli

# Or from source
git clone https://github.com/noahsabaj/mermaid-cli.git
cd mermaid-cli
cargo install --path .
```

```bash
mermaid                        # Start fresh session
mermaid --continue             # Resume last session
mermaid --model qwen3-coder:30b  # Use specific model
mermaid list                   # List available models
```

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| Enter | Send message |
| Esc | Stop generation / clear input |
| Ctrl+C | Quit |
| Alt+T | Toggle thinking mode |
| Ctrl+V | Paste image or text from clipboard |
| Ctrl+O | Preview attached image |
| Ctrl+Click | Open image from chat history |
| Up/Down | Navigate input history / scroll chat |
| Page Up/Down | Scroll chat |

## Commands

Type `:` followed by a command:

| Command | Description |
|---------|-------------|
| `:model <name>` | Switch model |
| `:clear` | Clear chat history |
| `:save` | Save conversation |
| `:load` | Load conversation |
| `:list` | List saved conversations |
| `:help` | Show all commands |
| `:quit` | Exit |

## Tools

The model uses these autonomously:

| Tool | Description |
|------|-------------|
| `read_file` | Read files (text, PDF, images) |
| `write_file` | Create or overwrite files |
| `edit_file` | Targeted text replacement with diff |
| `delete_file` | Delete files (with backup) |
| `create_directory` | Create directories |
| `execute_command` | Run shell commands |
| `git_status` | Check repository status |
| `git_diff` | View file changes |
| `git_commit` | Create commits |
| `web_search` | Search the web |
| `web_fetch` | Fetch URL content as markdown |

## Configuration

Config file: `~/.config/mermaid/config.toml`

```toml
last_used_model = "ollama/qwen3-coder:30b"

[ollama]
host = "localhost"
port = 11434
# cloud_api_key = "your-key"
```

## Cloud & Web Search

Access cloud models and web search via Ollama Cloud:

```bash
export OLLAMA_API_KEY=your-key
mermaid --model kimi-k2.5:cloud
```

## License

MIT OR Apache-2.0

Built with [Ratatui](https://github.com/ratatui-org/ratatui) and [Ollama](https://ollama.ai). Inspired by [Aider](https://github.com/paul-gauthier/aider) and [Claude Code](https://github.com/anthropics/claude-code).
