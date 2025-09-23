# 🧜‍♀️ Mermaid - Open Source AI Pair Programmer

An open-source, model-agnostic AI pair programmer CLI that provides an interactive chat interface with full agentic coding capabilities. Built with a unified interface supporting **100+ LLM providers** through LiteLLM proxy - from local Ollama models to every major cloud API.

## ✨ Features

- 🌐 **100+ Model Support**: Single interface for OpenAI, Anthropic, Google, Groq, Ollama, Azure, Cohere, Mistral, and 90+ more
- 🔄 **Hot-Swappable Models**: Switch between any provider/model mid-session without losing context
- 📂 **Project Aware**: Automatically loads and understands your entire project context
- 🛠️ **True Agency**: Can read, write, execute commands, and manage git
- 🔒 **Privacy First**: Run 100% locally with Ollama - your code never leaves your machine
- 💬 **Interactive TUI**: Beautiful terminal interface with syntax highlighting
- ⚡ **Real-time Streaming**: See responses as they're generated
- 🎯 **Smart Context**: Respects .gitignore and intelligently manages token limits
- 🐳 **Rootless Containers**: Secure Podman/Docker deployment with no daemon overhead

## 🚀 Quick Start

### Prerequisites

- Rust toolchain (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- Podman (`sudo apt-get install podman`) or Docker
- Optional: Ollama for local models (`curl -fsSL https://ollama.ai/install.sh | sh`)

### Installation

```bash
# Clone the repository
git clone https://github.com/noahsabaj/mermaid-cli.git
cd mermaid

# Copy environment template and add your API keys
cp .env.example .env
# Edit .env with your favorite editor and add API keys

# Start the LiteLLM proxy server (runs in background)
./start_litellm.sh

# Build and install Mermaid
cargo build --release
cargo install --path .
```

### Basic Usage

```bash
# Use with any of 100+ models
mermaid --model ollama/tinyllama         # Local tiny model
mermaid --model ollama/deepseek-coder:33b # Local large model
mermaid --model openai/gpt-4o            # OpenAI
mermaid --model anthropic/claude-3-sonnet # Anthropic
mermaid --model groq/llama3-70b          # Groq (fast!)
mermaid --model google/gemini-pro        # Google

# List all available models from proxy
mermaid list

# Check proxy status
./start_litellm.sh status

# View proxy logs
./start_litellm.sh logs
```

## 🎮 Interactive Commands

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

## 🔧 Configuration

### Environment Variables (`.env` file)
The primary configuration is through environment variables. Copy `.env.example` to `.env`:

```bash
# LiteLLM Proxy URL (default: http://localhost:4000)
LITELLM_PROXY_URL=http://localhost:4000

# API Keys - Add only the ones you need
OPENAI_API_KEY=sk-...
ANTHROPIC_API_KEY=sk-ant-api03-...
GROQ_API_KEY=gsk_...
GOOGLE_API_KEY=...
AZURE_API_KEY=...
COHERE_API_KEY=...
MISTRAL_API_KEY=...
# ... 90+ more providers supported

# Default model (optional)
MERMAID_DEFAULT_MODEL=ollama/tinyllama
```

### Application Configuration
Located at `~/.config/mermaid/config.toml`:

```toml
[default_model]
name = "ollama/deepseek-coder:33b"  # provider/model format
temperature = 0.7
max_tokens = 4096

[litellm]
proxy_url = "http://localhost:4000"  # Override env var if needed

[ui]
theme = "dark"
show_sidebar = true

[context]
max_files = 100
max_context_tokens = 75000
```

### Project Configuration
Create `.mermaid/config.toml` in your project root to override global settings.

## 🤝 Supported Providers (100+)

All providers are accessed through the unified LiteLLM proxy using the format `provider/model`:

### Local Models (Privacy-First)
- **Ollama**: `ollama/tinyllama`, `ollama/deepseek-coder:33b`, `ollama/codellama`, `ollama/mistral`
- **LlamaCPP**: `llamacpp/model-name`
- **Local AI**: `localai/model-name`

### Major Cloud Providers
- **OpenAI**: `openai/gpt-4o`, `openai/gpt-4-turbo`, `openai/gpt-3.5-turbo`
- **Anthropic**: `anthropic/claude-3-opus`, `anthropic/claude-3-sonnet`, `anthropic/claude-3-haiku`
- **Google**: `google/gemini-pro`, `google/gemini-pro-vision`, `google/palm-2`
- **Azure**: `azure/your-deployment-name`

### Fast Inference Providers
- **Groq**: `groq/llama3-70b`, `groq/mixtral-8x7b` (Ultra-fast inference)
- **Together AI**: `together/llama-2-70b`, `together/codellama-34b`
- **Anyscale**: `anyscale/llama-2-70b-chat`
- **DeepInfra**: `deepinfra/llama-2-70b`, `deepinfra/codellama-34b`

### Specialized Providers
- **Cohere**: `cohere/command`, `cohere/command-light`
- **Mistral**: `mistral/mistral-large`, `mistral/mistral-medium`
- **Perplexity**: `perplexity/llama-3-sonar-large`, `perplexity/codellama-70b`
- **Replicate**: `replicate/llama-2-70b`, `replicate/vicuna-13b`
- **Hugging Face**: `huggingface/bigscience/bloom`, `huggingface/codegen`

### Setup Instructions

1. **Add API Keys**: Edit `.env` file with your provider keys
2. **Start Proxy**: Run `./start_litellm.sh`
3. **For Ollama**: Models are auto-detected if Ollama is running
4. **Test Connection**: `curl http://localhost:4000/models`

## 💡 Example Workflows

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

## 🎨 Features in Action

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

## 🛠️ Development

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
┌─────────────┐     ┌──────────────┐     ┌─────────────────┐
│   Mermaid   │────▶│ LiteLLM Proxy│────▶│ 100+ Providers  │
│     CLI     │     │  (Port 4000) │     │ (Unified API)   │
└─────────────┘     └──────────────┘     └─────────────────┘
       │                                          │
       │                                   ┌──────┴──────┐
       ▼                                   ▼             ▼
  ┌─────────┐                      ┌─────────┐   ┌─────────┐
  │  Local  │                      │  Cloud  │   │  Fast   │
  │ Context │                      │  APIs   │   │Inference│
  └─────────┘                      └─────────┘   └─────────┘
                                   OpenAI        Groq
                                   Anthropic     Together
                                   Google        Anyscale
                                   Azure         DeepInfra
```

**Key Components:**

- `models/unified.rs` - Single unified model implementation using LiteLLM proxy
- `models/factory.rs` - Model factory that queries available models from proxy
- `agents/` - File system, command execution, git operations
- `context/` - Project analysis and context loading
- `tui/` - Terminal user interface with Ratatui
- `app/` - Configuration and application state

**Why LiteLLM Proxy?**
- Single API format (OpenAI-compatible) for all providers
- Automatic retry, fallback, and load balancing
- Built-in caching and rate limiting
- No need to maintain individual provider SDKs

## 📊 Comparison

| Feature | Mermaid | Aider | Claude Code | GitHub Copilot |
|---------|---------|-------|-------------|----------------|
| Open Source | ✅ | ✅ | ❌ | ❌ |
| Local Models | ✅ | ✅ | ❌ | ❌ |
| Model Providers | 100+ | ~10 | Claude only | OpenAI only |
| Unified Interface | ✅ LiteLLM | ❌ | ❌ | ❌ |
| Privacy | ✅ Full | ✅ Full | ❌ | ❌ |
| File Operations | ✅ | ✅ | ✅ | Limited |
| Command Execution | ✅ | ✅ | ✅ | ❌ |
| Git Integration | ✅ | ✅ | ✅ | ✅ |
| Streaming UI | ✅ | ✅ | ✅ | N/A |
| Rootless Containers | ✅ Podman | ❌ | ❌ | ❌ |
| Cost | Free* | Free* | $20/mo | $10/mo |

*Free with local models, API costs apply for cloud models

## 🤔 FAQ

### Can I use this with my proprietary code?
Yes! With local models (Ollama), your code never leaves your machine.

### Does it work offline?
Yes, with Ollama and local models.

### Can I add support for other models?
Mermaid already supports 100+ models through LiteLLM proxy! If your provider isn't supported, you can:
1. Request it in the LiteLLM repo (they add new providers regularly)
2. Use the OpenAI-compatible endpoint if your provider supports it
3. Run models locally with Ollama for complete control

## 📜 License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

## 🙏 Acknowledgments

- Built with [Ratatui](https://github.com/ratatui-org/ratatui) for the TUI
- Uses [Ollama](https://ollama.ai) for local model support
- Inspired by [Aider](https://github.com/paul-gauthier/aider) and Claude Code

## 🗺️ Roadmap

- [ ] Multi-agent collaboration
- [ ] Plugin system
- [ ] VSCode extension
- [ ] Model fine-tuning support
- [ ] Semantic code search
- [ ] Code review mode

## 💬 Community

- GitHub Issues: [Report bugs or request features](https://github.com/noahsabaj/mermaid-cli/issues)

---

**Note**: This project is under active development. Expect breaking changes until v1.0.

Made with 🧜‍♀️ by the open source community
