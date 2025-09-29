# Model Support Status

## Current Reality (September 2025)

### ✅ TESTED & WORKING
**Ollama Models (Direct Connection)**

Mermaid now uses **direct connection** to Ollama:
- No LiteLLM proxy needed
- No API keys required
- No .env file needed
- No Podman/Docker required
- Fastest and simplest setup

**How it works:**
```
Your request → OllamaDirectModel → Ollama API (localhost:11434)
```

**Tested Models**:
- qwen3-coder:30b
- deepseek-coder
- codellama
- llama3
- Any Ollama model

**Usage:**
```bash
mermaid --model ollama/qwen3-coder:30b
mermaid --model ollama/deepseek-coder:33b
```

### 🔧 THEORETICALLY SUPPORTED (Untested)
**API Models (via LiteLLM Proxy)**

The codebase includes:
- LiteLLM proxy integration (Python proxy server)
- Support for 100+ providers (OpenAI, Anthropic, etc.)
- API key configuration in .env
- Podman/Docker requirement

**How it works:**
```
Your request → UnifiedModel → LiteLLM Proxy → Provider API
```

**But**: These have NOT been tested and may not work.

### ⚠️ KNOWN ISSUES

1. **Outdated Model Names** (src/utils/tokenizer.rs:125-146)
   - gpt-4, gpt-3.5-turbo, claude-3 references
   - These are years old
   - Only used for token counting approximations
   - Doesn't affect Ollama functionality

2. **LiteLLM Proxy** (docker-compose.yml)
   - Included but untested
   - May or may not work with actual APIs
   - Designed for multi-provider support
   - **Not needed for Ollama**

3. **API Configuration** (.env.example)
   - Shows OPENAI_API_KEY, ANTHROPIC_API_KEY, etc.
   - **Not tested, may not work**
   - Left in for future expansion

## What Actually Works

### Ollama Setup (Direct Connection - NEW!)
```bash
# 1. Install Ollama
curl -fsSL https://ollama.com/install.sh | sh

# 2. Start Ollama (if not auto-started)
ollama serve

# 3. Pull a model
ollama pull qwen3-coder:30b

# 4. Run Mermaid - that's it!
mermaid --model ollama/qwen3-coder:30b
```

**No API keys, no proxy, no .env file, no Podman/Docker!**

The new direct connection:
- Starts in < 1 second (no proxy startup wait)
- Works 100% offline
- Zero configuration required
- Tested and reliable

## Future Plans

### To Actually Support API Models
Would require:
1. Testing with real API keys
2. Verifying LiteLLM proxy works
3. Testing each provider (OpenAI, Anthropic, etc.)
4. Updating model token limits
5. Handling API rate limits
6. Error handling for API failures

**Estimate**: 20-30 hours of testing and fixes

### Current Focus
- Ollama local models only
- No API dependencies
- Simple, works offline
- Privacy-focused

## Architecture Change (September 2025)

### OLD Architecture (Before)
```
ALL models → LiteLLM Proxy → Provider
```
**Problems:**
- Ollama required proxy (unnecessary)
- Proxy needed .env file
- Proxy needed Podman/Docker
- Slower startup (proxy wait)
- More complex setup

### NEW Architecture (Now)
```
ollama/* models  → Direct to Ollama API (localhost:11434)
other/* models   → LiteLLM Proxy → Provider API
```
**Benefits:**
- Ollama works out-of-the-box
- No proxy for Ollama = faster
- Simpler setup for 90% use case
- Tested code separated from untested

### Why the Change?

The code was originally architected to support all providers via LiteLLM proxy, but:
- Only Ollama has been tested in practice
- API provider testing requires actual API keys ($$$)
- Ollama is the primary use case (free, local, private)
- Proxy added unnecessary complexity for Ollama users

**Solution:** Separate direct Ollama path from proxy path

## Documentation Accuracy

### Files Claiming Multi-Provider Support
- CLAUDE.md - Documents LiteLLM architecture
- src/models/unified.rs - Has multi-provider code
- docker-compose.yml - Includes LiteLLM proxy
- .env.example - Shows all API keys

**Reality**: Architecture exists, testing doesn't.

### What to Believe
- **Ollama works**: 100% tested and working
- **APIs might work**: Architecture there, untested
- **LiteLLM proxy**: Included, untested

## Recommendation

### For Users
**Use Ollama models** - they work perfectly with the new direct connection:
```bash
mermaid --model ollama/qwen3-coder:30b
mermaid --model ollama/deepseek-coder:33b
```

**For API models** (OpenAI, Anthropic, etc.):
- Expect issues - untested
- Requires LiteLLM proxy setup
- Requires .env with API keys
- Requires Podman/Docker
- File issues if you try and it doesn't work

### For Developers
If you want to add/test API support:
1. Get API keys for testing
2. Test each provider manually
3. Fix issues as they arise
4. Update token limits
5. Document what works

**Current state:**
- Ollama: Production-ready, tested, fast
- API models: Theoretical, untested, may have issues

## Token Counting Note

The outdated model names (gpt-4, claude-3) in tokenizer.rs:
- Used only for token count approximations
- Don't affect Ollama functionality
- Ollama models use default GPT-3.5 tokenizer
- Good enough for CLI use case

**Not a priority to update unless adding real API support**

---

*Last Updated*: September 29, 2025
*Status*: Ollama direct connection (working), API support via proxy (untested)
*Architecture*: Dual-path - Direct Ollama + Proxy for APIs