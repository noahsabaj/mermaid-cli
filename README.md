# Mermaid - Open Source AI Pair Programmer

An open-source AI pair programmer CLI that provides an interactive chat interface with full agentic coding capabilities. Uses local Ollama models for fast, private coding assistance.

## Features

- **Native Tool Calling**: Ollama's tool calling API for structured, reliable actions (v0.2.0+)
- **Local Model Support**: Use Ollama for fast, private code assistance
- **Multiple Local Models**: Switch between different Ollama models mid-session without losing context
- **Project Aware**: Automatically loads and understands your entire project context
- **True Agency**: Can read, write, execute commands, and manage git
- **Privacy First**: Run 100% locally with Ollama - your code never leaves your machine
- **Interactive TUI**: Beautiful terminal interface with Claude Code-inspired aesthetics
- **Real-time Streaming**: See responses as they're generated
- **Smart Context**: Respects .gitignore and intelligently manages token limits
- **Web Search**: Integrated local Searxng for documentation and current information
- **Rootless Containers**: Secure Podman deployment with no daemon overhead

## Quick Start

### Prerequisites

- **Rust toolchain** (required for building from source)
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  source $HOME/.cargo/env
  ```

- **Ollama** (required for running local AI models)
  ```bash
  curl -fsSL https://ollama.ai/install.sh | sh
  ```

- **Podman** (optional, for web search via Searxng)
  ```bash
  # Ubuntu/Debian/Linux Mint
  sudo apt-get update && sudo apt-get install -y podman podman-compose
  ```

### Installation

#### Quick Install (Recommended)

**If you already have Rust:**

```bash
cargo install mermaid-cli
```

**If starting from scratch (installs everything):**

```bash
curl -fsSL https://raw.githubusercontent.com/noahsabaj/mermaid-cli/main/scripts/install.sh | bash
```

This one-liner installs:
- Rust (if needed)
- Ollama (if needed)
- Mermaid CLI from crates.io
- llama3.1:8b model (4.7GB, tool calling compatible)
- Configures your PATH

After installation, just run:

```bash
mermaid
```

#### Manual Install (Advanced)

Step-by-step installation with full control:

```bash
# 1. Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# 2. Install Ollama
curl -fsSL https://ollama.ai/install.sh | sh

# 3. Install Mermaid from crates.io
cargo install mermaid-cli

# 4. Download a compatible model
ollama pull llama3.1:8b

# 5. Run Mermaid
mermaid
```

### Updating Mermaid

To update to the latest version:

```bash
# Update from crates.io
cargo install mermaid-cli --force

# Or use the one-liner installer (also updates Ollama if needed)
curl -fsSL https://raw.githubusercontent.com/noahsabaj/mermaid-cli/main/scripts/install.sh | bash
```

### Basic Usage

```bash
# Start Mermaid with default model
mermaid

# Use a specific model
mermaid --model llama3.1:8b

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

## Model Compatibility

Mermaid uses **Ollama** for local model support with native tool calling (v0.2.0+).

### Verified Compatible Models (Tool Calling Support)

Models with native Ollama tool calling support that can execute file operations, commands, and git actions:

**Recommended for Coding:**
- `llama3.1:8b` - Fast, excellent tool calling (4.7GB)
- `llama3.1:70b` - Best quality, slower (40GB)
- `qwen2.5-coder:7b` - Optimized for code (4.7GB)
- `qwen2.5-coder:14b` - Excellent coding (9.0GB)
- `qwen2.5-coder:32b` - Elite coding (19GB)
- `mistral-nemo:12b` - Balanced performance (7.1GB)

**Other Compatible Models:**
- `llama3.2:1b` - Ultra-fast, limited capabilities (1.3GB)
- `llama3.2:3b` - Fast, decent quality (2.0GB)
- `firefunction-v2:70b` - Specialized for function calling (40GB)

### Models Without Tool Calling

These models can chat but cannot execute actions (coming in v0.2.1 with text fallback):
- `deepseek-coder:33b` - Excellent for code, no tool support
- `codellama` - Good for code, no tool support
- `tinyllama` - Ultra-fast, no tool support
- Most other Ollama models

### Installing Models

```bash
# Install a compatible model
ollama pull llama3.1:8b

# List installed models
ollama list

# Use with Mermaid
mermaid --model llama3.1:8b
```

### Cloud Models (Ollama Cloud)

Access massive models on datacenter hardware:
- `qwen3-coder:480b-cloud` - 480B params, elite coding
- `kimi-k2-thinking:cloud` - 1T params, advanced reasoning
- `deepseek-v3.1:671b-cloud` - 671B params, largest

**Note:** Cloud models require an API key from [ollama.com/cloud](https://ollama.com/cloud)

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

### Agent Capabilities (Native Tool Calling)

Mermaid uses Ollama's native tool calling API for structured, reliable actions:

**Available Tools:**
- `read_file` - Read any file (text, PDF, images with vision models)
- `write_file` - Create or update files in the project
- `delete_file` - Delete a file from the project directory
- `create_directory` - Create a new directory
- `execute_command` - Execute shell commands and see output
- `git_status` - Check git working tree status
- `git_diff` - View changes in files
- `git_commit` - Create commits with proper messages
- `web_search` - Search the web via local Searxng

**How It Works:**
1. Model receives tool definitions as JSON Schema
2. Model calls tools when needed (structured function calls)
3. Mermaid executes the tool and returns results
4. Model continues with the context of results
5. All tool calls are shown in the UI with clear summaries

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
