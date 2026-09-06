# CLI and TUI reference

## Command line

```bash
mermaid                                         # Start fresh session
mermaid --continue                              # Resume the most recent session in this directory
mermaid --resume                                # Pick a past session from a searchable list
mermaid --model ollama/qwen3-coder:30b          # Ollama local (any installed model — `mermaid list`)
mermaid --model anthropic/<model>               # Anthropic (requires ANTHROPIC_API_KEY)
mermaid --model gemini/<model>                  # Gemini (requires GOOGLE_API_KEY)
mermaid --model openai/<model>                  # OpenAI (requires OPENAI_API_KEY)
mermaid --model groq/<model>                    # Groq (requires GROQ_API_KEY)
mermaid --model grok/grok-4.6                   # Grok (xAI) (requires XAI_API_KEY; alias xai/<model>)
mermaid --model nvidia/z-ai/glm-5.2             # NVIDIA NIM (requires NVIDIA_API_KEY)
mermaid --model cloudflare/@cf/zai-org/glm-5.2  # Cloudflare Workers AI (CLOUDFLARE_API_TOKEN + CLOUDFLARE_ACCOUNT_ID)
mermaid --reasoning high                        # Override default reasoning depth
mermaid --path /path/to/project                  # Run against a specific project directory
mermaid --record /tmp/session.jsonl              # Record reducer events for replay/debugging
mermaid --replay /tmp/session.jsonl              # Reconstruct a recorded session (headless, deterministic)
mermaid --append-system-prompt "Prefer small diffs" # Add one-off runtime instructions
mermaid --system-prompt-file ./prompt.md         # Replace the default prompt for one run
mermaid run --plan "refactor the auth flow"      # Headless plan mode: read-only run that delivers a plan file
mermaid run --plan --plan-autoaccept "..."       # ...and continue straight into implementation
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
                                                # providers, Gemini, Ollama, and Anthropic
                                                # (output_config.format on current models).
mermaid --resume <id> run "and now the tests"   # Continue a saved session headless (id from
                                                #   ndjson session_started/result, json result,
                                                #   or the `session:` line on stderr)
mermaid --continue run "keep going"             # Resume this directory's most recent session
mermaid --no-network run "audit the code"       # Deny web tools everywhere; shell network on Linux/macOS
mermaid --sandbox run "refactor this"           # Also confine writes to the project (Linux/macOS)
mermaid add <name>                              # Add an MCP server (e.g., context7, git)
mermaid remove <name>                           # Remove a configured MCP server
mermaid mcp                                     # List configured MCP servers
```

`mermaid tasks`, `mermaid processes`, `mermaid plugin`, and the other durable-runtime verbs are
documented in [runtime.md](runtime.md).

## Keyboard shortcuts

| Key | Action |
|-----|--------|
| Enter | Send message (or queue while the model is generating) |
| Esc | Stop generation / dismiss command palette or attachment focus |
| Esc Esc | (idle) Rewind: fork the session at an earlier message — original preserved, composer pre-filled |
| Ctrl+C | Quit (auto-saves the session) |
| Ctrl+D | Quit when the input box is empty (auto-saves the session) |
| Ctrl+B | While tools are running, send the foreground command to the background (it keeps running as a `/processes` entry) |
| Alt+T | Cycle reasoning level: `None → Minimal → Low → Medium → High → XHigh → Max → None` |
| Shift+Tab | Cycle safety mode: `plan → read_only → ask → auto → full_access → plan` (session-scoped). `plan` is read-only exploration plus an approvable plan file |
| Ctrl+V | Paste image or text from clipboard (a copied image *file* pastes as the image) |
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

### Message queuing and mid-run steering

Type while the agent works. Queued messages are delivered at the next tool boundary within the
run, so the model course-corrects mid-task, or at run end when no boundary follows.

### Image paste

Ctrl+V attaches images for vision models on X11, Wayland, macOS, and Windows. All three
clipboard shapes work on every backend: a raster copy (screenshot tools, "Copy image"), an
encoded blob with no raster form (GIMP, Figma — `PNG` with no `CF_BITMAP`), and a file reference
from a file manager's Copy (Explorer / Finder / Nautilus). File-reference pastes accept
png/jpeg/gif/webp/bmp/tiff up to 32 MB.

The paste reads the clipboard through the platform's own helper:

| Platform | Helper |
| --- | --- |
| Linux / X11 | `xclip` |
| Linux / Wayland | `wl-clipboard` (`wl-paste`) |
| macOS | `pngpaste` / `osascript` |
| Windows | PowerShell |

```bash
sudo apt install xclip           # X11
sudo apt install wl-clipboard    # Wayland
```

### @-mentions

Type `@` in the composer to fuzzy-pick a project file (gitignore-aware); the path lands in your
prompt as text the agent reads with its tools.

## Slash commands

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

- `/model` — open the model picker: every model this machine can reach, local Ollama models grouped first, the active one marked, type to filter (↑↓ navigate · Enter switch · Esc cancel). Rows drop the provider their group heading already names — `mistralai/mistral-large-2-instruct` under `nvidia` — and the footer shows the highlighted row's full id, the string `/model <name>` and `--model` take. `/model <name>` switches directly; either way an Ollama model auto-pulls if needed
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

- `/safety [plan|read_only|ask|auto|full_access]` (alias `/permission`) — show or set the session safety mode; Shift+Tab cycles it
- `/plan [off|show|config]` — enter/leave plan mode (Shift+Tab cycles into it too; `off` returns to the configured `[safety] mode`), show the plan file, or open the plan settings picker (`/config` opens the same picker). Per-category permissions (builds, web, memory, task tools), plan-phase model and reasoning overrides, and approval behavior all live in `/plan config`. Approving a plan seeds the live task checklist from its Tasks section and can start implementation in place, in a cleared context, or hand off to a fork or fresh session on a different model — plan on a frontier model, execute locally
- `/approvals`, `/approve <id>`, `/deny <id>`
- `/checkpoint <path...>`, `/checkpoints`, `/restore <id>`

Integrations:

- `/plugins`, `/cloud-setup`

Advanced runtime:

- `/tasks`, `/task <id>`, `/pause <id>`, `/resume <id>`
- `/processes`, `/logs <id>`, `/stop <id>`, `/restart <id>`, `/open <target>`, `/ports`

Reasoning choices persist per-model: set `/reasoning high` on one model and `/reasoning low` on another, and each is remembered independently across sessions.
