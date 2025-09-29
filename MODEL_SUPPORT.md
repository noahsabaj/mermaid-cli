# Model Support Status

## Current Reality (September 2025)

### ✅ TESTED & WORKING
**Ollama Local Models Only**

Mermaid has been developed and tested exclusively with:
- Ollama locally installed models
- Models running on localhost
- No API key required

**Tested Models**:
- deepseek-coder
- codellama
- llama3
- Other local Ollama models

### 🔧 THEORETICALLY SUPPORTED (Untested)

The codebase includes:
- LiteLLM proxy integration (Python proxy server)
- Unified model interface for 100+ providers
- API key configuration in .env

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

### Ollama Setup
```bash
# Install Ollama
curl -fsSL https://ollama.com/install.sh | sh

# Pull a model
ollama pull deepseek-coder:33b

# Run Mermaid
mermaid --model ollama/deepseek-coder:33b
```

That's it. No API keys, no proxy, no configuration.

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

## Why the Disconnect?

The code was architected to support multiple providers via LiteLLM proxy, but:
- Only Ollama has been tested in practice
- API provider testing requires actual API keys ($$$)
- Ollama is the primary use case (free, local, private)

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
**Use Ollama only**. Don't try API models unless you want to debug.

### For Developers
If you want API support:
1. Get API keys for testing
2. Test each provider manually
3. Fix issues as they arise
4. Update token limits
5. Document what works

Until then: **Ollama only, everything else is theoretical**

## Token Counting Note

The outdated model names (gpt-4, claude-3) in tokenizer.rs:
- Used only for token count approximations
- Don't affect Ollama functionality
- Ollama models use default GPT-3.5 tokenizer
- Good enough for CLI use case

**Not a priority to update unless adding real API support**

---

*Last Updated*: September 29, 2025
*Status*: Ollama-only, API support untested