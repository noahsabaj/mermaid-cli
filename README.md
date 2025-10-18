# Mermaid - Open Source AI Pair Programmer

An open-source AI pair programmer CLI that provides an interactive chat interface with full agentic coding capabilities. Uses local Ollama models for fast, private coding assistance.

## Features

- **Local Model Support**: Use Ollama for fast, private code assistance
- **Multiple Local Models**: Switch between different Ollama models mid-session without losing context
- **Project Aware**: Automatically loads and understands your entire project context
- **True Agency**: Can read, write, execute commands, and manage git
- **Privacy First**: Run 100% locally with Ollama - your code never leaves your machine
- **Interactive TUI**: Beautiful terminal interface with syntax highlighting
- **Real-time Streaming**: See responses as they're generated
- **Smart Context**: Respects .gitignore and intelligently manages token limits
- **Rootless Containers**: Secure Podman/Docker deployment with no daemon overhead

## Quick Start

### Prerequisites

- Rust toolchain (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- Podman (`sudo apt-get install podman`) or Docker
- Ollama for local models (`curl -fsSL https://ollama.ai/install.sh | sh`)

### Installation

```bash
# Clone the repository
git clone https://github.com/noahsabaj/mermaid-cli.git
cd mermaid

# Build and install Mermaid
cargo build --release
cargo install --path .
```

### Basic Usage

```bash
# Use with any local Ollama model
mermaid --model ollama/tinyllama         # Tiny model (fast)
mermaid --model ollama/deepseek-coder:33b # Large model (best quality)
mermaid --model ollama/qwen3-coder:30b   # Excellent at coding

# List available models
mermaid list
```

## Interactive Commands

Once in the chat interface:

- **`i`** - Enter insert mode (type your message)
- **`Enter`** - Send message (in insert mode)
- **`Esc`** - Return to normal mode
- **`:`** - Enter command mode
- **`Tab`** - Toggle file sidebar
- **`Ctrl+C`** - Quit

### Command Mode

- `:help` - Show all commands
- `:model <name>` - Switch to a different model
- `:clear` - Clear chat history
- `:sidebar` - Toggle file tree
- `:quit` - Exit Mermaid

## Configuration

### Environment Variables (`.env` file)
Set your default model configuration:

```bash
MERMAID_DEFAULT_MODEL=ollama/tinyllama
```

### Application Configuration
Located at `~/.config/mermaid/config.toml`:

```toml
[default_model]
name = "ollama/deepseek-coder:33b"  # provider/model format
temperature = 0.7
max_tokens = 4096

[ui]
theme = "dark"
show_sidebar = true

[context]
max_files = 100
max_context_tokens = 75000
```

### Project Configuration
Create `.mermaid/config.toml` in your project root to override global settings.

## Supported Models

Mermaid uses **Ollama** for local model support. Available models include:
- `ollama/tinyllama` - Tiny, ultra-fast model for testing
- `ollama/deepseek-coder:33b` - Best for coding tasks
- `ollama/codellama` - Specialized code model
- `ollama/mistral` - Balanced performance
- `ollama/qwen3-coder:30b` - Excellent coding capabilities

Install any model with: `ollama pull model-name`

## Example Workflows

### Code Generation
```
You: Create a REST API endpoint for user authentication

Mermaid: I'll create a REST API endpoint for user authentication. Let me set up a basic auth endpoint with JWT tokens.

[Creates files, shows code, explains implementation]
```

### Code Review
```
You: Review my changes in src/main.rs

Mermaid: I'll review the changes in src/main.rs. Let me check the diff first.

[Analyzes code, suggests improvements, identifies issues]
```

### Debugging
```
You: The tests are failing, can you help?

Mermaid: I'll help you debug the failing tests. Let me first run them to see the errors.

[Runs tests, analyzes errors, fixes issues]
```

### Refactoring
```
You: Refactor this function to use async/await

Mermaid: I'll refactor this function to use async/await pattern.

[Shows original code, explains changes, implements refactoring]
```

## Features in Action

### Agent Capabilities

Mermaid can perform various actions by parsing special blocks in its responses:

- **File Operations**: Create, read, update, delete files
- **Command Execution**: Run shell commands and see output
- **Git Operations**: Check status, view diffs, commit changes

### Project Context

Mermaid automatically:
- Scans your project directory
- Respects `.gitignore` patterns
- Loads relevant source files
- Understands project structure (Cargo.toml, package.json, etc.)
- Manages token limits intelligently

## Development

### Building from Source

```bash
# Clone the repository
git clone https://github.com/noahsabaj/mermaid-cli.git
cd mermaid

# Build debug version
cargo build

# Run tests
cargo test

# Build optimized release
cargo build --release
```

### Architecture

```
┌─────────────┐     ┌──────────────┐
│   Mermaid   │────▶│    Ollama    │
│     CLI     │     │  Local Server│
└─────────────┘     └──────────────┘
│                          │
└──────────┬───────────────┘
           ▼
       ┌─────────┐
       │  Local  │
       │ Context │
       └─────────┘
```

**Key Components:**

- `models/ollama_direct.rs` - Direct Ollama connection
- `agents/` - File system, command execution, git operations
- `context/` - Project analysis and context loading
- `tui/` - Terminal user interface with Ratatui
- `app/` - Configuration and application state

**Privacy First:**
- All processing happens locally
- Your code never leaves your machine
- No external API calls or cloud dependencies

## Comparison

| Feature | Mermaid | Aider | Claude Code | GitHub Copilot |
|---------|---------|-------|-------------|----------------|
| Open Source | Yes | Yes | No | No |
| Local Models Only | Yes | Yes | No | No |
| Model Support | Ollama | Multiple | Claude only | OpenAI only |
| Privacy | Full | Full | No | No |
| File Operations | Yes | Yes | Yes | Limited |
| Command Execution | Yes | Yes | Yes | No |
| Git Integration | Yes | Yes | Yes | Yes |
| Streaming UI | Yes | Yes | Yes | N/A |
| Rootless Containers | Yes (Podman) | No | No | No |
| Cost | Completely Free | Completely Free | $20/mo | $10/mo |

## FAQ

### Can I use this with my proprietary code?
Yes! With local models (Ollama), your code never leaves your machine.

### Does it work offline?
Yes, with Ollama and local models.

### Can I add support for other models?
Mermaid uses Ollama for model support. To use additional models:
1. Pull the model with Ollama: `ollama pull model-name`
2. Use it with Mermaid: `mermaid --model ollama/model-name`
3. Check available models at [ollama.ai](https://ollama.ai)

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

## Acknowledgments

- Built with [Ratatui](https://github.com/ratatui-org/ratatui) for the TUI
- Uses [Ollama](https://ollama.ai) for local model support
- Inspired by [Aider](https://github.com/paul-gauthier/aider), [Gemini-CLI](https://github.com/google-gemini/gemini-cli), and Claude Code

## Community

- GitHub Issues: [Report bugs or request features](https://github.com/noahsabaj/mermaid-cli/issues)

---

**Note**: This project is under active development. Expect breaking changes until v1.0.

Made with love by the open source community
