# Model Persistence Design

## Problem

1. Mermaid defaults to `tinyllama` on every startup, ignoring the model used in the previous session
2. First-time users get `tinyllama` auto-installed without consent
3. Users want their model choice remembered between sessions

## Design Decisions

- **Single config file**: All state in `~/.config/mermaid/config.toml` (no separate session file)
- **Persist immediately**: Save model choice as soon as user switches
- **No auto-install**: Never download models without explicit user action
- **Inform and exit**: If no valid model available, print helpful message and exit (no TUI)

## Startup Flow

```
mermaid starts
    |
    +-> Load config.toml
    |       +-> Get last_used_model (may be None)
    |
    +-> Check CLI --model flag
    |       +-> If set, override last_used_model
    |
    +-> Determine effective model
    |       +-> CLI flag > config last_used_model > None
    |       +-> If None and no models installed -> inform & exit
    |
    +-> Validate model exists locally
    |       +-> If not found -> inform & exit
    |
    +-> If --model was used, persist to config.toml
    |
    +-> Launch TUI with validated model
```

## Config Structure

```toml
# Existing structure unchanged
[default_model]
provider = "ollama"
name = "glm-4.7-flash"

# New field at root level
last_used_model = "ollama/glm-4.7-flash"
```

**Precedence:**
1. CLI `--model` flag (highest)
2. `last_used_model` from config
3. `default_model.provider/name` combo (fallback)
4. If all None -> require model to exist

## Exit Messages

**No models installed:**
```
No Ollama models found.

To get started:
  1. Browse models at https://ollama.com/library
  2. Install one with: ollama pull <model-name>
  3. Run mermaid again

Example: ollama pull qwen3:8b
```

**Last-used model not found:**
```
Model 'glm-4.7-flash' not found.

To fix:
  - Install it: ollama pull glm-4.7-flash
  - Or use a different model: mermaid --model <other-model>

Available models: qwen3:8b, llama3:8b, mistral:7b
```

## Code Changes

| File | Changes |
|------|---------|
| `src/app/config.rs` | Add `last_used_model: Option<String>` to `Config`; remove `tinyllama` default |
| `src/ollama/installer.rs` | Remove auto-install logic; convert to validation-only |
| `src/main.rs` | Add early model validation before TUI; handle exit cases |
| `src/tui/command_handler.rs` | On `/model` switch, persist to config |
| `src/tui/app.rs` | When model changes via picker, persist to config |

**New helper:**
```rust
// In src/app/config.rs
pub fn persist_last_model(model: &str) -> Result<()> {
    let mut config = load_config()?;
    config.last_used_model = Some(model.to_string());
    save_config(&config, None)
}
```
