# Configuration

Config file: `~/.config/mermaid/config.toml` (Linux) or the platform equivalent
via the `directories` crate. Run `mermaid init` to create one.

Two environment variables relocate Mermaid's own directories wholesale, on
every platform: `MERMAID_CONFIG_DIR` (the directory holding `config.toml`) and
`MERMAID_DATA_DIR` (the runtime store: checkpoints, process records, the
daemon socket). They exist for sandboxed test runs, CI, and portable installs
— on Windows the platform locations are known folders that `HOME`/
`XDG_CONFIG_HOME` cannot move, so these are the only reliable override. An
empty value means unset.

## Layers

Configuration is assembled from layers, later layers winning key-by-key through
one recursive TOML deep-merge (tables merge; scalars and arrays replace):

1. built-in defaults
2. the user config file above
3. the project config: `<git-root>/.mermaid/config.toml`, located from the working directory
   (`-p /repo` adopts that repo's project config)
4. session flags: repeatable `-c key.path=value` plus the dedicated flags (`--no-network`,
   `--confine-fs`, `--sandbox`, `run --max-tokens`, `run --allow-untrusted-tools`), with a
   dedicated flag beating a contradictory `-c`

Unknown-key warnings name the layer they came from, and in-app settings changes (`/model`,
Alt+T, `/context`, `mermaid add`) rewrite only their own keys in the user file — unrecognized
keys in the file survive, and defaults are never frozen in.

## Project config

A repo can commit shared defaults in `.mermaid/config.toml` — model choice, profiles,
per-model reasoning, and UX knobs. Loading needs no trust ceremony because safety is
structural:

- Only these top-level sections are honored: `default_model`, `model_aliases`,
  `reasoning_per_model`, `ollama`, `ollama_num_ctx_per_model`, `compaction`,
  `computer_use`, `memory`, `non_interactive`, `ui`, and a `safety` subset. Anything else —
  including `web` (selects egress routing), `mcp_servers` (spawns commands), `providers`
  (redirects traffic/credentials), `agents`, and `daemon` — is ignored with a warning.
  `ollama.host`/`port` are also denied inside the otherwise-allowed `ollama` table.
- `safety.mode`, `safety.network`, and `safety.filesystem` are clamped tighten-only against
  your user config: a project can turn the sandbox on or drop to `read_only`, but can never
  loosen what you configured. Session flags (you, at the keyboard) still override everything.
- Startup prints `mermaid: using project config <path> (<n> keys)` whenever the layer
  contributes.

## Schema

Important fields in the current config schema:

```toml
# Last model picked via `--model` — used by bare `mermaid` on next start
last_used_model = "ollama/qwen3-coder:30b"

[default_model]
provider = "ollama"
name = "qwen3-coder:30b"
temperature = 0.7
max_tokens = 0        # 0 = auto (model-scaled output budget); positive = hard cap
reasoning = "medium"  # none | minimal | low | medium | high | xhigh | max

[ollama]
host = "localhost"
port = 11434
# Start `ollama serve` automatically when the local server isn't running
# (loopback hosts only; the revived server binds to exactly this host:port).
# Only paths that USE Ollama start it (chat, the startup model check); the
# read-only verbs (`mermaid list` / `models` / `status` / `doctor`) observe
# and never start anything — a stopped server still lists its installed
# models, read from Ollama's on-disk store (honoring OLLAMA_MODELS), and
# labeled "starts automatically on use". Disable autostart here — or with
# MERMAID_OLLAMA_AUTOSTART=0 in the environment — if you manage Ollama
# yourself (custom bind address, containers, CI).
auto_start = true
# cloud_api_key = "your-key"  # for :cloud models
# num_gpu = 10
# num_thread = 8
# num_ctx = 8192
# numa = false

[safety]
# Approval policy. Default is "ask": prompt before mutations / shell / network
# actions. "auto" runs an LLM classifier that vets each borderline action
# against your stated intent — aligned actions run automatically, risky ones
# escalate to an approval prompt. "full_access" auto-runs everything local;
# write-shaped MCP tools (no read-only annotation) are still vetted against
# your intent per `external_writes` below. "read_only" blocks mutations and
# requires one-shot approval for each web request because URLs and queries are
# externally observable. Set allow_readonly_web = true only when unattended
# web egress in read_only sessions is intentional; project config cannot set it.
# Change it live with Shift+Tab or `/safety <mode>` (session-scoped; this
# value is the persistent default each session starts from).
mode = "ask"
checkpoint_on_mutation = true
# network = "allow"       # "deny" is a global shell + web egress kill-switch
# allow_readonly_web = false
# Enforcement floor for write-shaped MCP tools (send / deploy / delete-remote
# — anything without a server-advertised readOnlyHint): safety mode alone
# never authorizes an external side effect. "allow" restores unconditional
# runs, "auto" (default) vets against your intent, "ask" always prompts,
# "deny" blocks.
# external_writes = "auto"
# Same levels for machine-scoped package operations (npm -g, cargo install,
# pip install, brew/apt/winget installs): they change the machine, not the
# project, so even full_access vets them. Project-local installs
# (npm install, cargo add) are untouched.
# system_installs = "auto"
# Model the "auto" classifier uses to vet actions. Omit to vet with the
# session's active model; set a smaller/faster model to cut latency and cost.
# auto_classifier_model = "<provider>/<small-fast-model>"

[exec]
# Foreground commands run on a pseudo-terminal by default (openpty on Unix,
# ConPTY on Windows): the child sees a real console, so spinner/progress
# tools behave, and on Unix /dev/tty resolves to the captured pty instead of
# writing over the TUI. ANSI escapes are stripped from what the model sees;
# a child that reads stdin hangs to its timeout (use mode="background" for
# interactive daemons). Set false to use pipes.
# pty = true

[ui]
# TUI color theme: "dark" (default) or "light". Switch live with
# `/theme dark|light` (persists here). Setting the NO_COLOR environment
# variable (any non-empty value) disables colors entirely, regardless of
# this value.
theme = "dark"

[non_interactive]
# Run behavior is controlled by CLI flags:
#   mermaid run "prompt" --format json --max-tokens 4096 --no-execute
# These fields remain in the schema for compatibility but are not the
# source of truth for `mermaid run`.
output_format = "text"
max_tokens = 0        # 0 = auto (model-scaled); positive = hard cap
no_execute = false

# Durable agent memory (the `memory` tool, the always-loaded index, and
# /remember & friends). On by default.
[memory]
enabled = true
# index_cap_bytes = 8000   # byte cap on the always-loaded memory index

[compaction]
# Cap on consecutive auto compact-and-continue recoveries after a
# context-window truncation, before the run stops and shows the manual
# levers (`/context max`, `/context offload on`). 0 = uncapped.
max_truncation_recoveries = 3
# Compact automatically when the window crosses the threshold. false leaves
# compaction entirely to `/compact`.
# auto_enabled = true
# auto_threshold_percent = 85        # clamped to 1..=100
# tail_turns = 2                     # user turns kept verbatim (min 1)
# tail_token_budget = 8000           # token ceiling on that tail
# tool_output_max_chars = 2000       # per-message cap in the summarizer excerpt
# summary_max_tokens = 8000          # ceiling on the checkpoint produced
# summarizer_input_token_budget = 64000
# min_response_reserve_tokens = 4000 # window held back for the reply
# max_response_reserve_tokens = 20000
# Both token budgets scale DOWN automatically on a small context window, so
# these are caps rather than demands. Nonsense values are clamped, not
# rejected: 0 falls back to the default and swapped reserve bounds are ordered.

# Subagents (the `agent` tool). Built-in types: `general` (full tool access
# at your safety mode) and `explore` (read-only reconnaissance). Define more
# below; a custom name shadows a built-in, so `[agents.types.explore]`
# retunes the built-in. Callers pick a type with the tool's `type` arg,
# override the model per call with `model`, and continue a prior child with
# `agent_id` (from the `[agent_id: …]` trailer on each result).
[agents]
# Wall-clock ceiling per subagent drive, in seconds. 0 = built-in default
# (1200 = 20 minutes).
timeout_secs = 1200

# Example user-defined type. Every field is optional.
# [agents.types.scout]
# tools = ["read_file", "execute_command"]  # omit for the full child set
# safety = "read_only"    # ceiling — the child never runs looser than this
# preamble = "You are a scout: find and report, fast."
# model = "ollama/qwen3:8b"   # default model for this type; per-call `model` wins
# isolation = "worktree"  # private git checkout; per-call `isolation` wins

# Per-model reasoning preferences (remembered across sessions)
[reasoning_per_model]
# "<provider>/<model>" = "high"
"ollama/qwen3-coder:30b" = "low"

# Optional named config overlays selected per invocation with
# `--profile <name>`. A profile's values beat this file's top-level values
# but lose to a repo's project config and to `-c` overrides.
#   mermaid --profile work doctor
# [profiles.work.default_model]
# temperature = 0.2
# [profiles.work]
# last_used_model = "anthropic/<model>"

# Optional model-id aliases. A request for `--model fast` or
# `--model alias:fast` resolves through this table when present.
[model_aliases]
fast = "ollama/qwen3-coder:14b"
# large-context = "openai/<model>"
# tool-strong = "anthropic/<model>"
# vision = "gemini/<model>"
# cheap = "groq/<model>"

# Remote providers — override env-var name, base URL, or extra headers
[providers.anthropic]
# api_key_env = "MY_ANTHROPIC_KEY"  # default: ANTHROPIC_API_KEY

[providers.gemini]
# api_key_env = "MY_GOOGLE_KEY"  # default: GOOGLE_API_KEY; GEMINI_API_KEY is accepted as a legacy fallback

[providers.meta]
# api_key_env = "MY_META_KEY"  # default: MODEL_API_KEY
# base_url = "https://api.meta.ai/v1"

[providers.groq]
# api_key_env = "MY_GROQ_KEY"    # default: GROQ_API_KEY
# base_url = "https://api.groq.com/openai/v1"

[providers.grok]
# api_key_env = "MY_XAI_KEY"  # default: XAI_API_KEY
# base_url = "https://api.x.ai/v1"
# extra_headers = { "X-Custom-Header" = "value" }

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

## System prompt

System prompt customization is runtime-only and is not saved to config:

```bash
mermaid --append-system-prompt "Prefer minimal diffs"
mermaid --append-system-prompt-file ./extra-instructions.md
mermaid --system-prompt "You are a focused code reviewer."
mermaid --system-prompt-file ./replacement-system-prompt.md
```

## Web tool backends

Web routing is user-controlled under `[web]` in the user config or a session `-c` override.
Project config cannot select a backend or destination:

```toml
[web]
fetch_backend = "native"    # "native" (default, in-process, no key) or "ollama"
search_backend = "auto"     # "auto" (default) | "ollama" | "searxng"
allow_ollama_search_fallback = false  # opt-in: cloud search where "auto" has no bundle
searxng_url = "http://localhost:8080"
```

- **`web_fetch` defaults to `native`**: it fetches directly from your machine without ambient proxy variables, rejects URL userinfo and non-global destinations at every redirect, refuses HTTPS downgrade redirects, decodes supported text charsets, and routes extraction by MIME. HTML/XHTML uses readability; Markdown and plain text are preserved; JSON and XML are rendered as data. Set `fetch_backend = "ollama"` to explicitly route through Ollama Cloud's server-side fetch instead (useful for JS-heavy pages and bot walls; needs `OLLAMA_API_KEY`). Ollama's API does not disclose its target redirect chain or final URL, so Mermaid labels final provenance as unknown and treats target-hop enforcement as provider-managed rather than fabricating a final URL.
- **`web_search` defaults to sovereign `auto`**: Mermaid downloads and runs a self-contained local [SearXNG](https://github.com/searxng/searxng) bundle on the first search on supported Linux/macOS targets, then health-checks and reuses it. Merely setting `OLLAMA_API_KEY` never changes the route. Select `search_backend = "ollama"` explicitly for Ollama Cloud, or `"searxng"` for your own instance at `searxng_url` (including on Windows; the instance must have `json` in `search.formats`). Windows managed search remains unavailable because no supported bundle exists — set `allow_ollama_search_fallback = true` (with `OLLAMA_API_KEY`) to let `auto` fall back to Ollama Cloud there. The fallback engages only where the bundle is unsupported (a viable managed bundle always wins), and the startup notice discloses the off-machine egress when it does.
- Native fetches retain requested/final URL, status, MIME, charset, backend, extraction mode, source/extracted sizes, and truncation provenance; cloud fetches retain the requested URL and explicitly mark final provenance unavailable. The complete model-visible result is capped at 30 KB. Decoded chunks are charged at the transport boundary against a 64 MiB per-turn budget, with 16 MiB per response, eight downloads globally, two per origin, two blocking extractors, and four concurrent search queries. Batch search preserves input order and reports per-query partial failures structurally. Snapshots are session/task scoped and the process-wide cache is capped at four entries and 32 MiB.

## Provider notes

Model names are whatever the vendor currently ships — Mermaid passes them through, so use any
model id from your provider's docs. See the env-var table in the
[README](../README.md#remote-providers).

Meta Muse Spark uses Meta's Responses API so encrypted reasoning state survives
Mermaid's model/tool loop without Meta retaining the response server-side.
Mermaid requests automatic reasoning summaries for the existing reasoning panel
and keeps the encrypted continuation only in private local session data.

```bash
export MODEL_API_KEY="your-meta-api-key"
mermaid --model meta/muse-spark-1.1 --reasoning high
```

Cloudflare Workers AI needs both `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` (the account id is spliced into the account-scoped endpoint URL); alternatively set `[providers.cloudflare].base_url` to a full account-scoped URL or an AI Gateway endpoint. Example model: `cloudflare/@cf/zai-org/glm-5.2`.

Grok (xAI) uses `XAI_API_KEY` (create one at https://console.x.ai) at `https://api.x.ai/v1` — OpenAI-compatible Chat Completions. Models like `grok-4.6`/`grok-4` support vision (`jpg/jpeg`/`png`, 20MiB per image, unlimited images) and tool calling. Both `grok/<model>` and `xai/<model>` prefixes work and resolve to the same endpoint. Example: `mermaid --model grok/grok-4.6`.

Ollama Cloud models authenticate via `OLLAMA_API_KEY`. Native `web_fetch` and managed/self-hosted SearXNG do not require it. Cloud web routing is never inferred from the key: set `fetch_backend = "ollama"` or `search_backend = "ollama"` explicitly, or opt into `allow_ollama_search_fallback` for platforms without a managed bundle (see [Web tool backends](#web-tool-backends)). Use `mermaid cloud-setup` from your shell to set the key for cloud models; `/cloud-setup` in the TUI points back to that shell command.

When a cloud provider call fails, the error shown in the TUI ends with a `(request-id: ..., cf-ray: ...)` line when the provider's response carried those headers — quote it when reporting the failure to the provider (or in a Mermaid issue), it lets them find the exact request.

## API keys

API keys resolve in strict precedence order: **environment variables always win**; the OS keyring (macOS Keychain, Windows Credential Manager, Linux Secret Service) fills the gap when no env var is set. Store a key with `mermaid login <provider>` (hidden input; `mermaid login` alone lists every provider's key status) and remove it with `mermaid logout <provider>`. A per-provider `api_key_env` override in config is authoritative — when set, neither the default env var nor the keyring is consulted. On headless Linux without a Secret Service the keyring quietly reports no keys (env vars keep working); set `MERMAID_NO_KEYRING=1` to disable keyring lookups entirely. `doctor`, `mermaid login`, and `mermaid feedback` report each key's source as `env`, `keyring`, or `none` — never the value.
