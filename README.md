# Mermaid

An open-source AI coding assistant for the terminal. Local/cloud models with Ollama, native tool calling, and a clean TUI.

## Features

- **Local Models**: Run 100% locally with Ollama - your code never leaves your machine
- **Cloud Models**: Run models via Ollama Cloud - use the most powerful models available today with no hardware cost
- **Native Tool Calling**: Ollama's tool calling API for structured, reliable actions
- **True Agency**: Read, write, edit, execute commands, search the web, and manage git
- **Real-time Streaming**: See responses as they're generated
- **Session Persistence**: Conversations auto-save and can be resumed
- **Thinking Mode**: Toggle extended thinking for complex reasoning (Alt+T)
- **Message Queuing**: Type while the model generates - messages queue up and send in order
- **Operation Modes**: Normal, Accept Edits, Plan Mode, and Bypass All - cycle with Shift+Tab

## Quick Start

### Prerequisites

- [Rust](https://rustup.rs/) (for building)
- [Ollama](https://ollama.ai/install.sh) (for running models)

### Install

```bash
# From crates.io
cargo install mermaid-cli

# Or from source
git clone https://github.com/noahsabaj/mermaid-cli.git
cd mermaid-cli
cargo install --path .
```

### Run

```bash
# Start fresh session
mermaid

# Resume last session
mermaid --continue

# Use specific model
mermaid --model qwen3-coder:30b

# List available models
mermaid list
```

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| Enter | Send message |
| Esc | Stop generation / Clear input |
| Ctrl+C | Quit |
| Alt+T | Toggle thinking mode |
| Shift+Tab | Cycle operation mode |
| Up/Down | Scroll chat or navigate history |
| Page Up/Down | Scroll chat |
| Mouse Wheel | Scroll chat |

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

## Configuration

Config file: `~/.config/mermaid/config.toml`

```toml
# Last used model (auto-saved)
last_used_model = "ollama/qwen3-coder:30b"

[ollama]
host = "localhost"
port = 11434
# cloud_api_key = "your-key"  # For Ollama Cloud models

[context]
max_files = 100
max_file_size = 1048576  # 1MB
```

## Available Tools

The model can use these tools autonomously:

| Tool | Description |
|------|-------------|
| `read_file` | Read any file (text, PDF, images) |
| `write_file` | Create or overwrite entire files |
| `edit_file` | Targeted edits by replacing specific text (shows diff) |
| `delete_file` | Delete files (with backup) |
| `create_directory` | Create directories |
| `execute_command` | Run shell commands |
| `git_status` | Check repository status |
| `git_diff` | View file changes |
| `git_commit` | Create commits |
| `web_search` | Search the web for current information |
| `web_fetch` | Fetch a URL and return content as markdown |

## Model Compatibility

Any Ollama model with tool calling support works. Models without tool calling can run inference in mermaid, but cannot use tools (Bash, Read, Write, Web Search, etc.)

### Ollama Cloud

Access large models via Ollama Cloud:

```bash
# Configure API key
export OLLAMA_API_KEY=your-key

# Use cloud models
mermaid --model kimi-k2.5:cloud
```

## Web Search

Web search uses the Ollama Cloud API. Set your API key to enable:

```bash
export OLLAMA_API_KEY=your-key
```

## Development

```bash
cargo build          # Build
cargo test           # Test
cargo install --path . --force  # Install locally
```

## License

MIT OR Apache-2.0

## Acknowledgments

Built with [Ratatui](https://github.com/ratatui-org/ratatui) and [Ollama](https://ollama.ai). Inspired by [Aider](https://github.com/paul-gauthier/aider) and [Claude Code](https://github.com/anthropics/claude-code).
