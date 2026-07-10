# Changelog

All notable changes to Mermaid CLI will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Plugin bundles: MCP servers, prompt commands, and agent types.** An
  enabled plugin's `plugin.toml` can now declare `mcp` (TOML files of
  `[servers.<name>]` configs, started with your own servers and flowing
  through tool deferral), `prompts` (markdown slash commands with
  `$ARGUMENTS` substitution, shown in the palette as `(plugin:<name>)` and
  in `/help`), and `agents` (TOML files of `[types.<name>]` agent types).
  Config-defined entries always shadow plugin ones with a warning;
  built-in slash commands can never be shadowed; `./`-relative MCP
  commands resolve inside the plugin directory with containment. Same
  restart-to-refresh policy as skills.


- **Kill background agents: `/agents` command + `agent` tool kill action.**
  `/agents` lists every detached (Ctrl+B backgrounded) subagent with its id,
  description, live activity, elapsed time, and token count; `/agents kill
  <id>` cancels one and `/agents kill all` cancels every one. The model can
  manage its own children too: the `agent` tool now takes `action: "kill"`
  plus an `agent_id` (killing an already-finished child evicts it from the
  continuation cache instead). A killed child unwinds orderly, posts a
  "cancelled" note with its billed token spend folded into the session
  totals, and — unlike a normally-finished background agent — does not queue
  its partial report for the model. Closes the follow-up from the agent
  backgrounding work below.

- **OS-keyring API keys + `mermaid login`.** `mermaid login <provider>`
  stores a provider API key in the OS keyring (macOS Keychain, Windows
  Credential Manager, Linux Secret Service); `mermaid login` lists every
  provider's key status; `mermaid logout <provider>` removes a stored key.
  Environment variables keep absolute precedence, and a per-provider
  `api_key_env` override remains authoritative (no keyring fallback).
  `doctor` and `mermaid feedback` now report each key's source (`env`,
  `keyring`, `none`). `MERMAID_NO_KEYRING=1` disables keyring lookups.

- **Foreground commands run on a PTY (Unix).** `execute_command` children
  now see a real terminal: `isatty` is true, spinner-heavy tools emit sane
  progress instead of dumping escape garbage, and `/dev/tty` resolves to the
  CAPTURED pty — a `sudo`-style prompt lands in the tool output instead of
  painting over the TUI. ANSI sequences are stripped from what the model
  sees and PTY `\r\n` line endings are normalized; the tee log keeps raw
  bytes so backgrounded (`Ctrl+B`) tails render colors. The same sandbox
  launcher, secret-env scrubbing, timeout, cancel, and detach semantics
  apply on both paths. Opt out with `[exec] pty = false`; any pre-spawn PTY
  failure falls back to pipes automatically. One semantic change: a child
  that reads stdin now hangs until its timeout instead of hitting instant
  EOF (mitigated by `GIT_TERMINAL_PROMPT=0` and the command timeout).

- **Live agent panel + agent backgrounding.** Running `agent` tools now get
  one stable panel row each under the status spinner — description, the
  child's current tool or phase, elapsed time, and a live token count — so
  parallel agents are visible from the moment they start instead of only
  when they finish. Ctrl+B now genuinely works for agents: it detaches every
  running subagent from the turn (the model gets an immediate "moved to
  background" outcome), the children keep running with their rows marked
  `bg`, and each report is delivered to the conversation through the
  queued-message path when it finishes, with its token spend still folded
  into the session totals. The `ctrl+b to background` hint only renders when
  something running can actually background (shell commands or agents).

- **Named config profiles.** Define `[profiles.<name>]` overlays in your
  user config and select one per invocation with the global
  `--profile <name>` flag. Profile values beat the user file but lose to a
  repo's project config (its tighten-only safety clamp still wins) and to
  `-c` overrides. Unknown profile names error listing what is defined;
  `doctor` shows the active profile; persists never touch `[profiles.*]`.

- **`mermaid run --output-schema <FILE>`.** Structured output for headless
  runs: the agentic loop runs completely normally, then one extra formatting
  turn (no tools) reshapes the final answer to the given JSON Schema —
  natively enforced on OpenAI-compatible providers (`response_format`),
  Gemini (`responseJsonSchema`), and Ollama (`format`); prompt-driven on
  Anthropic. The response is validated client-side either way: valid output
  lands in the result's `structured_output` field, and any failure keeps the
  text answer and records an `output_schema:` error instead of returning
  nothing.

- **Task checklist (todo tracking).** New `task_create` / `task_update` /
  `task_list` tools let the model plan multi-step work as an id-addressed
  checklist: batch creation and differential batch updates in single calls,
  an optional `explanation` surfaced to the user on scope pivots, and
  soft-corrective notes (never rejections) when discipline slips (two tasks
  in_progress, pending-to-completed jumps). The checklist renders live under
  the status line — the spinner headline becomes the active task's present
  tense form, completed rows collapse to strikethrough with a per-task cost
  suffix (elapsed time + tokens), long lists window with an overflow footer —
  and Ctrl+T toggles a one-line `Next:` collapsed view. The list persists
  with the session (resume restores it), clears on rewind/fork and `/clear`,
  and subagents get isolated checklists for free. Beyond the tools: a
  bounded evidence trail records what actually ran while each task was in
  progress (visible in `task_list` and `/todos`); a staleness nudge tells
  the model when an in_progress task has gone untouched for five model
  calls; the gated `task_completed` plugin hook can veto a completion
  (flipping the task back to in_progress with the reason); `/todos`
  (`add`/`rm`/`done`/`clear`) lets the user edit the checklist directly,
  with the model notified on its next request; and headless runs emit an
  additive `tasks_updated` NDJSON event carrying the full snapshot with
  cost attribution.

- **Light theme, `/theme`, and a `[ui]` config table.** The TUI now ships a
  light palette alongside the default dark one. `/theme dark|light` switches
  live and persists as `ui.theme`; every previously hardcoded widget color
  (role markers, diff backgrounds, modal text, the queued-message band) now
  routes through the theme.

- **`NO_COLOR` support.** Setting `NO_COLOR` (any non-empty value, per
  no-color.org) renders the whole TUI in the terminal's own default colors.
  Layout, glyphs, and bold/dim structure are unchanged.

- **Compose in `$EDITOR`.** Ctrl+O (or `/editor`) suspends the TUI, opens
  the current input draft in `$VISUAL`/`$EDITOR`, and loads the result back
  into the composer on save-quit. Works mid-run (it only edits the draft);
  recordings capture the returned text, so `--replay` never launches an
  editor.

- **`web_fetch` find-in-page.** New optional `pattern` argument: instead of
  returning the whole page, the tool returns each match (plain
  case-insensitive substring, per line) with `context_lines` of surrounding
  context (default 2), line-numbered and merged into blocks, capped at 20
  blocks with a `(+N more matches)` tail. Matching runs on the full page
  before the output cap, so tail matches on long pages are found.

- **Provider request ids in error messages.** Errors built from a provider's
  HTTP response now capture `x-request-id`/`request-id`/
  `anthropic-request-id` and `cf-ray` and append them as a
  `(request-id: ..., cf-ray: ...)` line to the user-facing message — quote
  it when reporting provider failures. Log output is unchanged.

- **Mid-run steering.** Messages typed while the agent is working are now
  delivered at the next tool boundary WITHIN the run — committed as user
  messages right after the tool results, so the very next model call sees
  them and course-corrects mid-task. Previously queued input waited for the
  whole run to end. Messages queued mid-stream with no later tool boundary
  still deliver at run end; images attached to queued messages ride along.

- **Checkpoints anchored to the conversation.** File checkpoints now record
  which session and message position produced them. Rewinding with
  double-Esc reports the checkpoints the discarded timeline left behind and
  names the one to `/restore` to roll files back to the fork point — rewind
  itself still never touches files.

- **Deferred MCP tools via `tool_search`.** MCP tools are no longer all
  advertised on every request: the model gets one `tool_search` tool that
  searches the deferred pool (names + descriptions) and promotes matches to
  direct advertisement for the rest of the session. Bounds the always-on
  tool surface (and the prompt tokens it costs) no matter how many servers
  are configured. Opt out with `mcp_defer_tools = false` globally or
  `defer = false` per server.

- **Concurrent, timeout-bounded MCP startup.** Servers now start in
  parallel, each with a 60s wall-clock bound, and report Ready/Errored
  individually as they resolve — one hung server no longer delays (or
  blocks) the rest. A timed-out server reports `startup timed out after
  60s` instead of hanging startup.

- **Provider-safe MCP tool names and schemas.** Tool names are sanitized at
  ingestion (charset `[A-Za-z0-9_-]`, 64-char cap with a stable hash suffix
  on overflow/collision — server names containing `__` no longer break
  routing) and input schemas are normalized for strict provider validators
  (local `$ref` inlined, `$defs`/`$schema` dropped, `const` to `enum`,
  nullable `anyOf` flattened, draft-4 boolean exclusive bounds dropped).
  `enabled_tools`/`disabled_tools` filters keep matching the raw names the
  server advertises.

- **Meta Muse Spark 1.1 now uses the Responses API with reasoning continuity.**
  `meta/muse-spark-1.1` authenticates with `MODEL_API_KEY`, streams text,
  automatic reasoning summaries, tool calls, usage, and multimodal inputs from
  `POST /v1/responses`. Mermaid uses Meta's stateless encrypted-replay mode
  (`store: false`) so reasoning survives tool turns and saved-session resume
  without server-managed response state; the opaque continuation is protected
  by existing private-session permissions and never rendered or written to
  diagnostic logs. Other providers keep their existing endpoints and behavior.
  One migration note: the per-message `thinking_signature` field became the
  provider-neutral `provider_continuation`, so Anthropic extended-thinking
  sessions saved before this change resume without their replayed thinking
  blocks (new turns re-establish them; no request errors).

- **Rewind and fork with double-Esc.** Pressing Esc twice within a second
  while idle opens a picker of the session's earlier user messages (newest
  first). Selecting one FORKS the session at that point: the original is
  saved and preserved (it keeps appearing in the `--resume` picker, now with
  its lineage recorded via `forked_from`/`parent_session`), a new session id
  takes over with the history before that message, and the composer is
  pre-filled with the message itself — its pasted images re-staged — so you
  edit and resend to branch the timeline. Busy Esc is unchanged (still
  cancels); a first idle Esc shows an `esc again to rewind` hint on the
  input box. The whole flow is key-driven and pure, so record/replay work
  unchanged.

- **@-mention fuzzy file picker.** Typing `@` at the start of a word in the
  composer opens a fuzzy picker over the project's files (ripgrep's
  gitignore-aware walker — hidden files, `.git`, and ignored paths excluded;
  capped at 20k entries) ranked by nucleo, the matcher Helix uses. Up/Down
  navigate, Tab or Enter inserts the relative path as plain text
  (`@src/foo.rs `) — the model reads it with its own tools, so mentions
  survive persistence, compaction, replay, and every provider. Esc dismisses
  for the current token; typing reopens. `user@host` never triggers, and the
  slash palette keeps `/` input.

- **`mermaid feedback` + an always-on TRACE ring.** A new in-memory ring
  captures the last ~2000 trace events from mermaid's crates at TRACE level
  (dependencies capped at INFO) regardless of `RUST_LOG`, with secrets
  redacted at capture — so a bug that already happened is diagnosable without
  a reproduce-under-logging round trip. `mermaid feedback` bundles the doctor
  report, a names-and-booleans config summary (provider keys reported as
  present/absent, never values), recent session ids, the trace ring, and the
  log tail into a local `mermaid-feedback-<ts>.md` (mode 0600; `--stdout` /
  `--format json` available). Nothing is uploaded; the rendered bundle passes
  a final whole-document redaction sweep. `RUST_LOG` now scopes only the file
  log layer.

- **Render snapshot suite.** Nine curated TUI scenes (idle, transcript,
  streaming, tool execution with a queued message, approval modal, question
  modal, conversation picker, slash palette, system notice + compaction
  checkpoint) are now pinned as full-frame `insta` snapshots at 80x24 and
  120x40, with a determinism self-check. Any unintended visual drift fails CI
  with a diff; deliberate changes are reviewed with `just snapshots` and the
  updated `.snap` files ride in the same PR.

- **Context windows and output caps are now discovered live, not hardcoded.**
  Anthropic and Gemini turns fetch the model's real limits from their models
  endpoints (`max_input_tokens`/`max_tokens`, `inputTokenLimit`/
  `outputTokenLimit`) — cache-first in `provider_probes` with the same 30-day
  TTL the Ollama probe uses, one same-host GET per (provider, model) on a
  miss. Claude turns now see their real 1M-token windows and 128K output
  ceilings instead of the rotted 200K/64K pins (auto-compaction was firing
  ~5x too early), the stale per-family output table is deleted outright, and
  `mermaid model-info` reports the discovered numbers with a `probed`
  confidence. GPT-5.6 (1.5M) joins the static catalog — OpenAI's `/v1/models`
  exposes no limits, so OpenAI stays static-but-corrected. Edge case: an
  Anthropic-compatible gateway id the Models API 404s on falls back to a
  conservative 8192 output floor on AUTO; set an explicit `max_tokens` to
  override.
- **Mermaid learns output caps from provider 400s and retries.** When a
  provider rejects a turn naming the model's real per-response ceiling
  (Ollama Cloud's `exceeds model's maximum output tokens (N)` — the
  minimax-m3 incident — or the OpenAI-style `max_tokens is too large`), the
  cap is persisted to the limits cache, the request is clamped, and the turn
  retries once instead of dying. Every later turn sizes below the learned cap
  up front. The parser is deliberately strict: context-limit wordings never
  match, so a window can't be mislearned as an output cap.

- **Skills: SKILL.md playbooks with progressive disclosure.** Mermaid now
  discovers task-specific playbooks at startup — project
  (`<git-root>/.mermaid/skills/<name>/SKILL.md`), user
  (`~/.config/mermaid/skills/`), and enabled plugins (the manifest's `skills`
  list, containment-checked like hooks) — and injects a compact index (name,
  one-line description, path) into the system prompt. Same-named skills dedupe
  with project > user > plugin precedence; the index caps at 64 skills / 8 KiB
  with an overflow line. The model activates a skill by reading its SKILL.md
  with the existing policy-gated `read_file`, so activation is visible in the
  transcript and idle skills cost no per-request tool schema. Headless runs
  and subagents load the same index; `mermaid doctor` reports the count.

- **Plugin hooks can now gate tool calls.** On `before_tool_use`, an enabled
  plugin hook may deny the call, rewrite its arguments, or inject context for
  the model's next request via a Claude Code-compatible JSON response on
  stdout (`permissionDecision` allow/deny/ask, the legacy `decision: block`
  shape, plus mermaid's `updatedInput` and `additionalContext` extensions);
  exiting with code 2 also denies, with stderr as the reason. First deny wins
  across plugins, the last rewrite wins, context strings concatenate, and a
  rewritten call is still vetted by the safety policy. Intent fails closed
  (explicit denials always deny) while infrastructure fails open (a hook that
  times out or prints garbage logs a warning and allows) — a buggy hook can't
  lock you out of every tool. All other events remain observe-only.

- **`mermaid run` is session-addressable.** Every headless run now surfaces
  its session id — a new `session_id` field on the ndjson `session_started`
  and `result` lines and the json result (protocol stays v1; the fields are
  additive), plus a `session: <id>` line on stderr for the text/markdown
  formats — and accepts `--resume <id>` / `--continue` to seed the run from a
  saved session, appending to the same session file so repeated resumes chain.
  Headless runs also stamp git branch/SHA/CLI-version provenance like the
  interactive path, a run that ends in a provider error still persists its
  session (the emitted id never dangles), and a script that names a missing
  session or asks for `--continue` with none saved gets a hard error instead
  of a silent fresh session.

### Changed

- **`model_profiles` renamed to `[model_aliases]`** (and the `profile:`
  model-id prefix to `alias:`) to free the "profile" name for the config
  overlays above. Bare alias names still resolve unchanged.

### Fixed

- **Existing `[model_profiles]` config tables migrate automatically.** The
  rename to `[model_aliases]` left older config files warning about an
  unknown key; loads now migrate the table in memory (warning gone
  immediately) and the next settings persist renames it on disk.


- **The status line no longer flickers raw subagent text.** Every child
  stream chunk used to overwrite the status line (garbage fragments flying
  past at stream speed); children now report only stable activity — their
  current tool, coarse phase changes, and a token count throttled to twice a
  second. The run token counter also keeps climbing during agent turns
  (children's live counts ride on top) instead of freezing until the tools
  return.

- **Token accounting normalized; one honest set of meters.** The footer
  showed three token counters measuring three different things: `context`
  and `last api` were the same number rendered twice, and `session` was a
  naive sum of every API call's full total — the re-sent conversation input
  (and cache reads) counted again on every call, so a short agentic run
  read "1.2M". The footer now shows only the context gauge
  (`context: 24.5k / 1M (2%)`); the cumulative sum lives in `/usage`,
  labeled as what it is (all API calls, subagents included, input/output/
  cache broken out). The run summary ("Worked for 6m · used N tokens") now
  counts real provider-reported output tokens across the run — parent
  turns, subagents, and mid-run compactions — falling back to the old
  chars/4 estimate (marked `~`) only when a provider reports no usage.
  Underneath, `TokenUsage` stores only disjoint components and derives all
  totals, fixing two real unit bugs: each adapter previously invented its
  own `total_tokens` (Anthropic's included cache reads, OpenAI's didn't),
  and OpenAI-compat/Meta double-counted reasoning tokens in output totals
  (wire `completion_tokens` already includes them) — OpenAI-visible output
  numbers shrink accordingly.
- **Rate-limit errors now say what actually happened.** A provider 429 used
  to surface as the inscrutable "retry after None"; it now shows the
  provider's own reason from the response body (e.g. Cloudflare's "you have
  used up your daily free allocation of 10,000 neurons") — the difference
  between "wait a moment" and "upgrade your plan" — plus the server's
  retry-after when sent. 429 retries also got their own backoff schedule
  (~2s then ~5s instead of the 5xx 500ms→1s): retrying inside the same rate
  bucket always lost. `Retry-After` headers were already honored and still
  win when present.
- **Cloudflare Workers AI models now report their real context window.**
  Cloudflare's OpenAI-compatible `/models` lists bare ids with no limit
  metadata, so the footer gauge read "context: … / unknown" and compaction
  sizing flew blind. Live limits discovery now queries the account's
  `models/search` endpoint instead (openrouter format first — context window
  plus output cap for marketplace models — falling back to the full-catalog
  default format for everything else). AI Gateway base-url overrides keep
  the previous generic behavior.
- **The full repaint no longer kills the TUI.** The post-shell-command
  repaint (and Ctrl+L) queried the terminal for the cursor position, and the
  reply raced the input reader thread — the first shell command the model
  ran could exit the app with "The cursor position could not be read within
  a normal duration". The repaint now clears and redraws without ever
  querying the terminal.
- **Headless runs no longer truncate auto-continued replies.** `mermaid run
  -p … --output text/markdown/json` returned only the final continuation
  segment; it now joins the whole chain, echo-trimmed.
- **Image clicks resolve by their stable `[Image #N]` number.** Ctrl+Click
  previously mapped through the display position, which the transcript
  stitch can shift; the global number now wins, with the positional pair as
  fallback for pre-numbering sessions.
- **A run that ended at the auto-continue cap no longer starts the next run
  with an empty continuation budget** (the counter now resets on submit,
  like its truncation/empty-turn siblings).
- **Shell commands can no longer grab the terminal.** Model-run commands now
  start in their own session (`setsid`) with no controlling terminal, so a
  child that opens `/dev/tty` — a `sudo` password prompt, an ssh passphrase
  read — fails instantly instead of painting its prompt over the TUI and
  hanging until the command timeout. Git is additionally told never to prompt
  (`GIT_TERMINAL_PROMPT=0`) in foreground and background commands alike.
  Esc-cancel, timeout tree-kill, and Ctrl+B backgrounding semantics are
  unchanged; no behavior change on Windows.
- **The TUI now recovers from stray writes to the terminal.** ratatui only
  repaints cells it knows changed, so bytes another process wrote directly to
  the tty (e.g. a child that opened `/dev/tty`) used to persist as ghost
  characters that typing couldn't dislodge. The screen now fully repaints
  after each shell command finishes, and Ctrl+L forces an immediate repaint
  at any time.
- **Ctrl+Shift+C no longer quits — it copies the selection.** Mermaid now
  negotiates the kitty keyboard protocol (disambiguated key encoding) where
  the terminal supports it, so Ctrl+Shift+C arrives distinct from Ctrl+C and
  triggers the existing drag-select copy instead of the quit path; Esc
  reporting also becomes unambiguous. On terminals without the protocol the
  two chords are physically identical on the wire — there the new press-twice
  behavior below keeps a stray copy attempt harmless.

### Changed

- **Ctrl+C now requires a second press to exit.** The first Ctrl+C does the
  useful thing — interrupts a running turn (like Esc) or clears typed input —
  and shows "press ctrl+c again to exit" for 3 seconds; a second press inside
  the window exits. Ctrl+D on empty input and `/quit` still exit immediately.

### Added

- **Auto-continued replies are now seamless.** A reply that crosses the
  model's per-response output cap reads as ONE uninterrupted assistant
  bubble: the transcript stitches the continuation into its bubble (a code
  fence cut open at the seam renders as a single intact block), a
  conservative overlap trim drops the resume-echo some models emit, and the
  in-flight continuation streams into the same bubble live. The "continuing"
  system note is gone from view and — once its one request has gone out —
  retired from history entirely, so it can never steer a later, unrelated
  turn (the stalled-turn retry nudge gets the same treatment). Canonical
  history still stores the true wire-level messages (provider-correct,
  thinking-signature-safe); the stitch is display-only.
- **Data-driven model-capability catalog.** Thinking wire shape (anthropic
  adaptive-vs-budget, gemini level-vs-budget floors, gpt-oss think-string),
  temperature support, effort-tier ceiling, vision markers, static context
  windows, and documented output ceilings now live in one first-match-wins
  const table (`src/models/catalog.rs`) instead of model-name string gates
  scattered across four adapters and the domain. Every known model's behavior
  is pinned unchanged by table-driven tests; Ollama's `/api/show` probe stays
  authoritative for local models. Two deliberate accuracy fixes: matching is
  uniformly case-insensitive, and gpt-5/gpt-4.1's 400k static window now
  applies through any provider (previously `openai/` only).
- **Cloudflare Workers AI is now a built-in provider.** Reach Cloudflare-hosted
  models through their OpenAI-compatible endpoint with
  `mermaid --model cloudflare/@cf/<vendor>/<model>` — for example
  `cloudflare/@cf/zai-org/glm-5.2` (GLM-5.2). Set `CLOUDFLARE_API_TOKEN` and
  `CLOUDFLARE_ACCOUNT_ID` (the account id is spliced into the endpoint URL), or
  point `[providers.cloudflare].base_url` at a full account-scoped URL / AI
  Gateway endpoint. Exposes the reasoning-level selector and streams GLM-5.2's
  thinking trace. Discovery surfaces (`doctor`, the best-effort `/models` probe)
  use the same account-scoped URL and report a missing account id instead of
  probing a placeholder; a setup missing both env vars gets one error naming
  both.
- **Project-local config.** A repo can now commit `.mermaid/config.toml` at its
  git root; it layers between your user config and session flags. Safe by
  construction, with no trust prompt: security-sensitive keys (`mcp_servers`,
  `providers`, `agents`, `daemon`, `last_used_model`, `web.searxng_url`,
  `ollama.host`/`port`, and most of `safety`) are stripped with a warning, and
  the allowed `safety.mode`/`network`/`filesystem` can only TIGHTEN your user
  settings — a cloned repo can pick models and UX defaults but can never spawn
  commands, redirect prompt traffic, or relax approvals. Startup prints a
  one-line notice whenever a project config contributes keys, and runtime
  memory-setting re-reads honor the project layer too.

- **Layered config engine.** Configuration now merges as ordered layers —
  built-in defaults < `~/.config/mermaid/config.toml` < session flags (`-c`
  plus `--no-network`/`--confine-fs`/`--sandbox`, `run --max-tokens`,
  `run --allow-untrusted-tools`) — through one recursive TOML deep-merge and a
  single typed deserialize. Unknown-key warnings name the layer that contains
  the typo, and in-app settings changes now rewrite only their own keys in the
  user file: unrecognized keys survive persists, defaults are no longer frozen
  into the file, and per-model entries whose ids contain dots
  (`gemini/gemini-2.5-pro`) persist correctly. A corrupt config file degrades
  to defaults while the session flags still apply.
- **Long responses auto-continue across the model's per-response output cap.**
  When a reply is cut by the provider's per-response output ceiling with
  context-window room to spare, mermaid now continues it in a fresh turn (the
  committed partial rides in history and a note nudges the model to resume, not
  restart), bounded per run so a re-truncating model can't loop. A mid-reasoning
  cutoff or a capped run still stops with the accurate output-limit message. So
  an answer that "wants 40000 tokens" completes even on providers that cap
  single responses lower.
- **Live model-limit discovery — `Context: unknown` gets real numbers.** Most
  OpenAI-compatible providers attach the model's context window and output
  ceiling to their `/models` metadata (OpenRouter, Cloudflare, …); mermaid
  previously threw that data away. It's now parsed, cached across sessions in
  `provider_probes`, and refreshed into the live capability snapshot — so the
  status bar shows a real window for remote models, proactive auto-compaction
  works for them, the truncation classifier gets real windows, and
  `mermaid model-info` gains an `Output limit:` line. Anthropic models report
  their documented window/ceiling statically.
- **Truncation is now diagnosed correctly — no more false "Context window
  full".** A `length` stop is classified from the response usage: hitting the
  per-response output cap (window still has room — the common case on remote
  providers) now stops with an accurate message naming the real limit, instead
  of misreporting a full window and looping through futile conversation
  compactions that couldn't help. A genuinely full window still compacts and
  continues exactly as before.
- **Model-scaled output budget — the hardcoded 4096-token cap is gone.**
  `default_model.max_tokens` now defaults to `0` = **auto**: OpenAI-compatible
  providers and Gemini get no cap at all (the provider applies the model's own
  per-response maximum), Ollama's `num_predict` gets the full room the context
  window leaves after the prompt, and Anthropic (which requires `max_tokens`)
  gets the model's documented output ceiling clamped to the window. Reasoning
  models like GLM-5.2 can finally emit tens of thousands of thinking tokens
  without tripping a stale 2024-era limit. A positive `max_tokens` remains an
  explicit hard cap (cost control); existing config files carrying the frozen
  legacy `4096` are migrated to auto on load. Compaction's response reserve is
  now reasoning-aware instead of mirroring the send-cap.
- **Optional filesystem write-confinement for shell commands (Linux).**
  `mermaid --confine-fs …` (or `safety.filesystem = "project"`) confines
  model-run shell commands with a Landlock ruleset: writes are allowed only
  beneath the project directory, the system temp directory, and `/dev`; reads
  and execution stay unrestricted. A failure matching the denial signature
  while confinement is active is reported with a clear "filesystem sandbox"
  explanation and the `denied_by_sandbox` marker. `mermaid --sandbox …` is
  shorthand for `--no-network --confine-fs`. Best-effort by design: kernels
  without Landlock (pre-5.13) and other platforms degrade to a warned no-op.
  Off by default.
- **Optional network kill-switch for shell commands (Linux).** `mermaid
  --no-network …` (or `safety.network = "deny"`) confines model-run shell
  commands with a seccomp-BPF filter that denies internet sockets
  (`AF_INET`/`AF_INET6`) while leaving `AF_UNIX` and local IPC working, so a
  sandboxed command can't reach the network but ordinary local work still runs.
  A blocked attempt is reported with a clear "blocked by the network sandbox"
  message and a `denied_by_sandbox` marker instead of a confusing crash. Applied
  via a hidden `__sandbox-exec` re-exec launcher; a no-op on macOS/Windows
  (Seatbelt/AppContainer are follow-ups). Off by default.
- **Typed NDJSON event stream for `mermaid run`.** `mermaid run --format ndjson`
  streams the run lifecycle as one JSON object per line — `session_started`,
  `text`/`reasoning` deltas, `tool_started`/`tool_finished`, `approval_required`,
  `turn_done`, and a terminal `result` — so `mermaid run` can be driven as an
  SDK/subprocess. The event shape is a stable, versioned contract
  (`protocol_version`) pinned by a golden serialization test; `--format json`
  now emits that same typed `result` object.
- **Startup process hardening + per-turn timing.** On Linux, Mermaid now disables
  core dumps (`RLIMIT_CORE=0`) and ptrace attachment (`PR_SET_DUMPABLE=0`) at
  startup, so a crash can't leave a core file carrying secrets and the process
  isn't trivially attachable. Each model turn also emits a structured timing
  event (turn id, model, elapsed) to the log / diagnostic bundle.
- **Faster session listing + session provenance.** Each saved session now writes
  a tiny `<id>.meta` sidecar, so listing sessions (`/list`) reads metadata
  instead of parsing every transcript (older sessions fall back to a full read).
  Sessions also record their creation environment — git branch, git SHA, and CLI
  version — plus `forked_from` / `parent_session` lineage fields (ready for a
  future fork/rewind).
- **TUI: attention bell, keyboard scrolling, and a shortcut list.** The terminal
  title now reflects run state (`mermaid · working`, else the conversation
  title), and Mermaid rings the bell when a run finishes or an approval is
  waiting while the terminal is unfocused. PageUp/PageDown scroll the transcript
  (Shift+Up/Down by a line, End jumps back to the newest message), and `/help`
  now lists the keyboard shortcuts.
- **Providers & MCP: keyless local servers, env-sourced headers, per-server tool
  filters, raw command servers.** Local OpenAI-compatible endpoints (llama.cpp,
  vLLM, LM Studio) now work with no API key when the base URL is loopback/LAN;
  a missing key for a public provider gives an actionable "get a key at …" hint.
  Providers can source secret headers from env vars (`[providers.x] env_headers`)
  instead of writing them into config. Each MCP server accepts `enabled_tools` /
  `disabled_tools` to scope which of its tools reach the model. And `mermaid add
  --command <cmd> --arg … --env K=V` registers an arbitrary command MCP server
  without going through the package registry. (Also fixes a latent bug where a
  built-in provider's static analytics headers were dropped.)
- **Config: typo warnings, `-c` overrides, and stdin prompts.** Unknown keys in
  `config.toml` now warn with their dotted path instead of being silently
  ignored. A repeatable `-c key.path=value` flag overrides any config value for
  one invocation (`mermaid -c default_model.max_tokens=8192 run "…"`; the value
  is parsed as TOML). And `mermaid run` reads its prompt from stdin when given
  `-` or no prompt (`echo "explain this" | mermaid run -`); piped stdin
  alongside an explicit prompt is appended as a fenced block.
- **Sharper prompt + searchable memory.** The system prompt gained a `## Web`
  section (browse for time-sensitive or externally-verifiable facts, prefer
  primary sources, cite inline) and a memory "signal gate" (save a fact only if
  a future session would act better for it). The `memory` tool gained a `search`
  action that substring-matches across fact names, descriptions, and bodies — a
  targeted read that stays available in every safety mode.
- **`apply_patch` — robust, multi-file editing.** A new patch-based editor
  (`*** Begin Patch … *** End Patch` with Add / Update / Delete / Move hunks and
  optional `@@` context anchors) backed by a graduated fuzzy matcher that
  tolerates whitespace and curly-quote drift, so an edit no longer fails on a
  stray trailing space. It edits several files in one call under a single
  checkpoint, and warns when a hunk matched fuzzily. Replaces `edit_file` (the
  old single exact-match replacement is removed).
- **The daemon now schedules its tasks instead of stampeding.** `run` requests
  enqueue; a scheduler executes queued tasks bounded by
  `[daemon] max_concurrent_tasks` (default 1 — one agent run at a time, honest
  for a single local GPU), ordered by priority (`run` accepts
  `"priority": "low"|"normal"|"high"`) then FIFO. The queue is durable: tasks
  submitted while the daemon is down or busy survive restarts and drain
  automatically.
- **Daemon tasks can be cancelled.** New `cancel_task` daemon command and
  `mermaid cancel <task-id>` CLI: a running task gets the same graceful
  teardown as pressing Esc in the TUI (current tool's process tree killed,
  turn unwound, status `cancelled` persisted), with a hard stop if it doesn't
  unwind within a grace window; a queued task is cancelled before it starts.
- **Per-task wall-clock budgets.** `[daemon] task_timeout_minutes` bounds each
  daemon task's runtime (unset keeps the existing 20-minute headless default),
  so an unattended run's worst case is bounded.
- **The agent can ask you structured multiple-choice questions.** A new
  `ask_user_question` tool lets the model pause mid-run and pose 1–4 questions in
  an interactive terminal modal — single-select, multi-select, rank, or typed
  inputs (text/number/date/path) — each with an "Other" free-text escape,
  optional side-by-side previews (including diffs), and a "remember this answer"
  toggle that persists settled preferences across sessions. Answers flow back as
  the tool result so the run continues with your decision; headless runs with no
  TTY proceed with best judgment instead of blocking.

- **NVIDIA NIM is now a built-in provider.** Reach NVIDIA-hosted models through
  their OpenAI-compatible endpoint with `mermaid --model nvidia/<model>` and an
  `NVIDIA_API_KEY` — for example `nvidia/z-ai/glm-5.2` (GLM-5.2). Reasoning-model
  traces (streamed as `delta.reasoning_content`) are surfaced, and tool calling
  works as with any built-in provider.

- **Mermaid warns when you attach an image to a model that can't see it.** Some
  Ollama models have no vision capability, so a pasted image is silently ignored
  — which looks exactly like a bug. Mermaid now probes the model's advertised
  `vision` capability (Ollama `/api/show`) the moment you paste an image (and on
  `/model` switch), and posts a one-line notice *before* you send if the model
  can't see it, so you can switch to a vision-capable model instead of wasting a
  turn. The notice is shown once per model per session; a per-turn re-check backs
  it up for a fast paste-then-send. This also makes `/doctor` report Ollama
  vision support accurately instead of always claiming "no".

- **Pasted images are now inline `[Image #N]` tokens in the prompt.** Instead of
  a separate `[Image #1] (PNG, 1KB)  (↑ to select)` bar floating above the input
  box, Ctrl+V splices an inline `[Image #N]` pill into the message text at the
  cursor — you can type around it and it deletes as a unit (Backspace on the pill
  removes both the token and the image). `N` is a **stable, conversation-global**
  number (it keeps climbing across messages and survives `--resume`/`--continue`),
  so "in image #16 you can see…" is unambiguous for you and the model; the
  submitted text carries the tokens so the model can correlate each image with
  its reference, and the transcript shows the same number. This also retires the
  attachment-focus bar entirely, so the up-arrow always steps through prompt
  history with no contention.

- **`read_only` safety mode now permits `web_search` and `web_fetch`.**
  Searching and fetching the public web are reads — reading is what
  read-only mode is for — so they no longer die with "blocks mutations and
  control actions". The SSRF guard (refusing internal / loopback / metadata
  hosts) lives in the web tools and applies in every mode, and an operator
  `Deny` override on the `web` category still outranks the carve-out.
  Anything that *acts* on the network keeps the `network` category and stays
  blocked.

- **`web_search`'s managed backend is now sovereign — no Docker or Podman.**
  The default `auto` backend (when `OLLAMA_API_KEY` is unset) no longer runs a
  SearXNG container. Instead the first search downloads a self-contained,
  sha256-verified bundle — a portable CPython plus the Granian server and
  SearXNG — from [mermaid-searxng](https://github.com/noahsabaj/mermaid-searxng),
  unpacks it under the data dir, and runs Granian bound to loopback, reaped on
  mermaid exit. No container runtime, no VM, nothing to install; the bundle is
  fetched once and cached. Forcing your own instance (`search_backend =
  "searxng"` / `searxng_url`) or Ollama Cloud (`OLLAMA_API_KEY`) is unchanged.

- **The Ollama auto-start is no longer silent.** At the moment mermaid
  commits to spawning `ollama serve`, one line — "Starting the local Ollama
  server (it stays running after mermaid exits)…" — now reaches the user:
  as a system line in the TUI transcript (via a new out-of-band
  `StreamEvent::Status` → `Msg::TransientStatus` path, recorded/replayed
  like any other Msg), on stderr for headless `mermaid run` (stdout stays
  clean for the response payload), and on stderr for the startup model
  check. The line fires only when a spawn actually happens — never when the
  server was already up, never on remote URLs, never from the read-only
  verbs — closing the latency-feedback, discoverability, and consent gaps
  of a revival that can otherwise hide up to ~15s behind a generic spinner
  and leave behind a detached server with no breadcrumb.

### Changed

- **Tool-call transcript labels distinguish creating a file from changing one.**
  A `write_file` that overwrites an existing file now reads `Update`, not
  `Write` — `Write` is reserved for a genuinely new file — and targeted
  `edit_file` calls read `Update` too. The vocabulary is now
  `Write` / `Update` / `Delete` (previously `Write` / `Edit` / `Delete`),
  matching Claude Code, so it's clear at a glance whether a call created,
  modified, or removed a file. The create-vs-modify distinction comes from the
  `created` flag the write tool already records, so it's accurate even when the
  model rewrites a whole file with `write_file` instead of `edit_file`.

- **File-diff summaries read as words, like Claude Code.** The `⎿` line under a
  `Write` / `Update` now reads `Added 49 lines, took 25ms` or
  `Added 7 lines, removed 1 line, took 25ms` (and `Removed 9 lines, took 25ms`),
  replacing the terse `Success, +49 -0, took 25ms`. Empty clauses are dropped and
  `line`/`lines` agree with the count; the timing is kept, the redundant
  `Success,` prefix removed.

- **`apply_patch` reads as `Update`, and `Success` is dropped from result lines.**
  An `apply_patch` call now shows `● Update(<file>)` — or `Write` / `Delete` by
  operation, `Update(N files)` for a multi-file patch — instead of the model's raw
  `Apply patch()` with empty parens, matching the `Write` / `Update` / `Delete`
  vocabulary. Separately, the redundant `Success` prefix is gone from every result
  line (a failure renders differently, so a plain success needs no label): e.g.
  `3 lines read, took 1.2s`, and a delete shows just `took 35ms`.

- **`mermaid list` and `mermaid models` no longer start Ollama.** All four
  read-only verbs (`list` / `models` / `status` / `doctor`) now enumerate
  with auto-start hard-off: observing state never mutates it, so a
  cloud-model user who deliberately stopped Ollama to free VRAM can run any
  of them without resurrecting the daemon. A dead server is reported
  honestly ("Ollama is installed but not running — local models can't be
  listed") instead of the misleading "No Ollama models installed locally."
  Auto-start remains on the paths that actually use Ollama: chat and the
  startup model check.

### Fixed

- **Read-only shell commands prefixed with `cd` are no longer blocked.** A
  `cd DIR && <read>` command (e.g. `cd repo && git status`) classified as a
  mutation because `cd` wasn't a recognized read-only head, so read_only mode
  blocked the whole compound command. `cd`/`pushd`/`popd`/`dirs` (plus
  `base64`/`seq`) now classify as read-only, and the read-only git allowlist
  gained the pure-read subcommands `rev-list`/`merge-base`/`show-ref`/
  `for-each-ref`/`name-rev`/`show-branch`/`count-objects`/`version`. The
  worst-segment rule still catches any real mutation in a later segment.
- **The model no longer believes it's still in `read_only` after switching to a
  looser mode.** A mutation denied in read_only left a `blocked by policy:
  read-only safety mode …` tool-result in the conversation, which was re-sent
  every turn; after switching to `full_access` the model trusted those stale
  errors over the (correct, live) mode and kept refusing edits — or claimed the
  runtime was "still read-only." Superseded read-only denials are now rewritten
  to a past-tense note once the live mode is looser, and loosening the mode past
  such a denial surfaces a one-line confirmation.
- **Command, tool, and web output truncation keeps the tail.** Output past the
  cap is truncated in the middle (head + tail with an elision marker) instead of
  head-only, so a failing command's actual error — which lives at the end — is
  no longer discarded before the model sees it.
- **Sessions saved mid-tool-call resume cleanly.** `tool_use`/`tool_result`
  pairing is normalized before every request and when a session is loaded, so a
  session persisted while a tool was in flight no longer resumes into an
  unrecoverable provider 400.
- **OpenAI-compatible vision is reported truthfully.** `supports_vision` is now
  derived from the model id instead of hardcoded `false`, so `/doctor` and
  `/model` no longer under-report vision for capable models and the no-vision
  warning fires for genuinely text-only models.
- **The approval and question brokers recover from a poisoned lock instead of
  cascading panics.** They took `Mutex::lock().unwrap()`, so a panic while
  holding the (tiny, synchronous) lock would poison it and every subsequent
  access would panic in turn. They now recover the guard on poison, matching the
  pattern already used elsewhere in the codebase.
- **The streaming relay task can no longer leak on cancel.** Each provider's
  stream bridge spawned a relay task whose handle was dropped (not aborted) when
  a turn was cancelled; parked on a full downstream sink, it could outlive the
  turn. It now rides an abort-on-drop guard (promoted to a shared util), so a
  cancelled turn aborts it — the same structural task ownership the effect runner
  already used internally.
- **`temperature = 0.0` is now honored.** Setting an explicit `0.0`
  (deterministic / greedy decoding) was silently overridden with the `0.7`
  default — a leftover `> 0.0` guard treated a deliberate zero as "unset." It now
  passes through verbatim.
- **Calling a stopped MCP server now returns a clear error.** After
  `stop_server`, a later `call_tool` hit the dead server and surfaced a
  broken-pipe transport error instead of saying the server was stopped. The
  client is now flagged on shutdown and the manager returns a clean
  "MCP server '…' has been stopped" message.
- **The queued-message FIFO is now bounded.** Prompts typed while a turn is in
  flight are queued and auto-submitted when it finishes; holding Enter through a
  long turn could grow that queue without limit. It's now capped at 32, dropping
  the oldest with a warning.
- **A misbehaving Ollama stream can't exhaust memory.** The newline-delimited
  JSON reassembly buffer had no cap, so an endpoint that streamed bytes without
  ever sending a newline would grow it until OOM. It now enforces the same 8 MiB
  reassembly cap the SSE (Anthropic/Gemini/OpenAI) streams already had.
- **The config file is now written atomically.** `save_config` truncated the
  config in place, so a crash, kill, or disk-full mid-write could leave an empty
  or half-written `config.toml` — losing your settings (and inline secrets). It
  now writes via a temp file + fsync + atomic rename, created `0o600` on Unix so
  the secret-bearing config is never even briefly world-readable.
- **Failed background saves now report what went wrong instead of vanishing.**
  Conversation saves, the compaction archive's conversation write, model /
  reasoning / Ollama-preference persistence, the `--record` replay log, the
  daemon's TCP hint file, and the `ask_user_question` "remember this answer"
  store all dropped their error on failure (some logged a bare "failed" with no
  detail). Each now logs the underlying error, and the answer-prefs write is
  atomic so a crash mid-save can't truncate it.
- **Cancelling or resetting a turn no longer leaves a dead question modal on
  screen.** An `ask_user_question` prompt parked mid-turn survived `/load`,
  `/clear`, Ctrl+C, and quit — the tool task behind it was torn down, so the
  modal could never be answered. All four paths now clear it (and the stale
  running-tool indicator) alongside the pending approval they already dropped.
- **A context-limit compaction no longer silently drops your turn.** When a
  provider rejected a request mid-stream for length, Mermaid compacted the
  conversation but then ended the turn instead of resuming — abandoning the work
  you asked for. It now resumes the request after compacting, exactly like a
  truncation recovery. Relatedly, a system note posted *during* a compaction
  (e.g. an MCP server error) is now inserted before a pending tool call rather
  than wedged between a `tool_use` and its `tool_result`, which some providers
  reject on the next request.
- **The daemon reaps orphaned background-command logs on startup.** Ctrl+B-
  detached commands leave a tee log (capped at 64 MiB each) in the private temp
  dir; across many restarts with backgrounded processes these accumulated
  forever. The daemon's startup recovery now sweeps `mermaid-bg-*.log` files
  older than `[daemon] retention_days` (a live detached process keeps its log
  fresh, so an old mtime means the writer is long gone).
- **The daemon's `outcomes` and finished-`tasks` tables no longer grow without
  bound.** The startup GC now prunes terminal tasks (with their events) and the
  append-only `outcomes` reward table, which #148's durable queue would otherwise
  keep — with their full prompts — forever. `outcomes` (the self-improving-loop
  training corpus) is retained on its own, deliberately longer window so a large
  training history survives the shorter task/session retention, and each outcome
  is stamped with its task's context so it stays usable after that task is
  pruned. New `[daemon] retention_days` (default 30) and
  `[daemon] outcomes_retention_days` (default 180) tune the two windows.
- **A daemon task that produces an empty response is recorded as a failure, not
  a success.** A run that returned no error but also no text was mapped to
  `Completed` and stamped a `task_terminal` success/1.0 into the `outcomes`
  training corpus — a false positive the self-improving loop would learn from. It
  is now a `Failed` task with a clear report, so the reward signal reflects that
  nothing was produced.
- **Pasting an image and immediately pressing Enter no longer drops the image.**
  Ctrl+V reads the clipboard asynchronously, so a fast paste-then-Enter could
  submit the message before the image arrived — sending it with no image (and
  leaking a stray `[Image #N]` into the next prompt). Enter now waits for any
  in-flight clipboard read to land, then submits with the image included. The
  read result rides a dedicated internal message so an empty or failed read
  still releases the held submit instead of wedging it, and a normal terminal
  paste is never mistaken for a Ctrl+V read.

### Security

- **The debug log and conversation store are owner-only and redacted.**
  `~/.mermaid/mermaid.log` and the saved conversation/compaction transcripts are
  now written `0600` and scrubbed of credential-shaped strings, so a `read_file`
  of `.env` or an API error echoing a key can't sit in cleartext in a
  world-readable file.
- **Concurrent same-path file edits are serialized.** A per-path async lock
  prevents two file-mutating tool calls in one turn from silently clobbering
  each other (last-writer-wins) or racing a read-modify-write.
- **`web_fetch` now pins DNS resolution against rebinding.** The native fetch
  resolved a URL's host and vetted the addresses, then let the HTTP client
  re-resolve at connect time — a TOCTOU a hostile DNS server could exploit to
  pass the pre-check with a public IP, then connect to `127.0.0.1` /
  `169.254.169.254`. A custom resolver now vets the resolved addresses at connect
  time, for the initial host and every redirect hop, failing closed on any
  internal address — so the connection binds to exactly what was vetted.
- **`open_url` (background-mode browser launch) now validates the URL scheme and
  no longer routes through a shell on Windows.** The model-supplied `open_url`
  was passed unvalidated — and on Windows via `cmd /C start`, so shell
  metacharacters (`& | > ^`) in the URL could execute arbitrary commands, while a
  `file:` / `javascript:` URL could reach the desktop handler on any OS. It is
  now rejected unless it is `http`/`https`, and launched on Windows via
  `rundll32` (a real executable, single argv — no shell re-parse). Loopback URLs
  stay allowed so opening a just-started local dev server still works.

### Infrastructure

- **Repo hygiene: `AGENTS.md`, a `justfile`, CI source guards, nextest, and
  CHANGELOG-driven releases.** A root `AGENTS.md` encodes the real invariants
  (MVU purity, no emojis, no back-compat shims) and a `justfile` provides the
  one-command pre-PR gate (`just check`). CI gained two dependency-free guards —
  no emoji/pictographs in source, and `src/domain` stays a pure MVU core (no
  I/O, no wall clock) — plus a daemon-integration + `self-test` job, and switched
  the test runner to `cargo-nextest` (retries auto-heal the Windows cancellation
  flake). Releases now build their notes from the curated CHANGELOG section
  (failing if the tag has none) and smoke-test each built binary (`version` +
  `self-test`) and the published install script.

## [0.16.0] - 2026-07-04

### Added

- **Mermaid now starts Ollama itself.** When a request to a *local* (loopback)
  Ollama URL is refused — cold boot after a reboot, server crashed mid-session
  — mermaid finds the `ollama` binary (PATH or the platform's default install
  locations), launches `ollama serve` detached (it survives mermaid exiting
  and ignores the TUI's Ctrl+C), waits for it to come up, and retries the
  request. No more leaving mermaid to run `ollama serve` by hand. Applies to
  chat, `mermaid models`/`list`, and the startup model check, for local and
  `:cloud` models alike; remote hosts are never touched, and the diagnostics
  (`mermaid status` / `doctor`) deliberately observe without healing — they
  now report "installed but not running" instead of conflating it with "no
  models". "Is Ollama installed" checks share the autostart's binary
  discovery, so a fresh install whose PATH hasn't reached the current shell
  is still found. If auto-start can't help (e.g. Ollama isn't installed), the
  connection error says exactly that and where to get it. Opt out with
  `auto_start = false` under `[ollama]`, or `MERMAID_OLLAMA_AUTOSTART=0` in
  the environment (containers/CI).

- **`mermaidd` now runs on Windows.** The daemon serves its JSONL control
  protocol over a named pipe (`\\.\pipe\mermaidd-<user-SID>`) locked to the
  owning user with an explicit security descriptor — the named-pipe analog of
  the `0600` Unix socket + peer-uid check — with remote pipe clients rejected
  and the first pipe instance doubling as the single-daemon guard. The
  `mermaid` CLI and the optional localhost TCP listener work unchanged.
  Service install (`mermaid daemon install`) remains systemd/Linux-only; on
  Windows start `mermaidd.exe` manually or via Task Scheduler.

- The runtime store now records **outcomes** — a durable, append-only table of
  verifiable results and reward/preference signals attached to a task (and
  optionally a specific tool run). Each outcome carries a `kind` (e.g.
  `task_terminal`, `test`, `preference`), a graded `label`, an optional scalar
  `reward`, and a `source` marking provenance (`verifier`/`user`/`model`/
  `system`) — the enrichment that turns the trajectory log into training data.
  `mermaidd` records a `task_terminal` outcome when a daemon-run task finishes.

## [0.15.1] - 2026-07-02

### Added

- The `--resume` picker can now delete a session: press `Del` on a row to
  remove its saved conversation (with a `y`/N confirm).

### Fixed

- Empty sessions are no longer saved: running `mermaid` and closing it without
  sending anything leaves no conversation file, so it can't clutter the
  `--resume` picker or be reached by `--continue`. Pre-existing empty session
  files are also filtered out of both resume paths.
- The `--resume` picker now scrolls properly: the mouse wheel scrolls the
  viewport (it previously moved the selection, because the alternate screen was
  translating the wheel into arrow-key sequences), and the list follows the
  selection when you arrow past the bottom or top instead of clipping.
- `--resume`/`--continue` now restore the full session state, not just the
  transcript: the safety mode and the token/context meters (the `context: …`
  and `session: …` figures in the status bar) are saved with the conversation
  and hydrated on resume, instead of resetting to `n/a` / `0` / the
  config-default safety mode. Safety-mode changes (`Shift+Tab`, `/safety`) now
  persist immediately; conversations saved before this fall back to defaults.

## [0.15.0] - 2026-07-02

### Added

- Web search and fetch now work out of the box with **zero configuration**, and
  are backend-pluggable via `[web]` config. `web_fetch` defaults to a **native**
  in-process backend (fetch the URL directly, convert HTML to markdown) — no
  key, no third party. `web_search` defaults to **`auto`**: Ollama Cloud when
  `OLLAMA_API_KEY` is set, otherwise mermaid **auto-starts and manages a local
  SearXNG container** (via podman/docker) on the first search and tears it down
  when it exits — you install and configure nothing. The first search pulls the
  SearXNG image once. Force a backend with `fetch_backend = "ollama"` or
  `search_backend = "ollama"`/`"searxng"` (your own instance at `searxng_url`,
  which must have the JSON format enabled).
- `mermaid --resume` opens a searchable picker of this directory's past
  conversations, styled like the main TUI (type to filter; each row shows the
  title and a `relative-time · branch · size` meta line). It replaces the old
  bordered `--sessions` picker (renamed for Claude Code parity). `--continue`
  is unchanged — it reopens the most recent conversation in the directory.
- Conversations now record the git branch they were worked on (shown in the
  `--resume` picker). Sessions saved before this backfill their branch on the
  next save; non-git directories simply omit it.
- Agent types for the `agent` tool. Built-in `general` (full tool access at
  your safety mode) and `explore` (read-only reconnaissance: reads +
  read-only commands, cannot mutate regardless of the parent's mode), plus
  user-defined types under `[agents.types]` in config — each a tool filter, a
  safety ceiling (the child runs at the *less* permissive of the parent's
  live mode and the ceiling, so a type can only tighten), a system-prompt
  preamble, and an optional default model. A custom name shadows a built-in,
  so `[agents.types.explore]` retunes the built-in. Pick a type with the
  tool's new `type` arg.
- Per-call subagent model override: the `agent` tool's new `model` arg (and a
  type's `model` default) runs a child on a different model than the parent —
  e.g. a cheap/fast model for search-and-summarize fan-out, the session model
  for synthesis. Priority: per-call `model` > type default > session model.
- Subagent continuation handles. Every `agent` result ends with an
  `[agent_id: …]` trailer; passing that id back as the new `agent_id` arg
  restores the child's conversation context and seeds the prompt as its next
  message, so a follow-up reuses what the child already learned instead of
  re-exploring. The most recent children (bounded cache) are retained;
  timed-out and errored children are kept too, so "continue aN: what did you
  find so far?" works.
- Configurable subagent timeout via `[agents] timeout_secs` (default 1200 =
  20 minutes), replacing the previously hard-coded ceiling.
- Live subagent visibility: while an `agent` call runs, the status line now
  shows the child's current activity ("Running tools: Agent explore crates ·
  read_file…") instead of an opaque spinner — the child's tool starts/finishes
  and latest text were already streamed to the parent but silently dropped by
  the reducer. Completed subagent rows also report what the child cost and
  which model ran it ("Success, 12.3k tokens · ollama/…, took 62s").
- Subagent report contract: a child session's system prompt now states that
  its final message is returned verbatim to the parent as the tool result and
  that nobody can answer questions — so children end with self-contained
  reports instead of "Want me to continue?".
- Subagents can now actually use MCP tools: the child's server entries are
  seeded Ready from the process-global MCP manager (shared with the parent —
  no per-child server processes), so `mcp__` tools are advertised to the
  child. Previously the registry carried the proxy but the tools were never
  advertised, making the documented capability dead in practice.

### Changed

- `web_fetch` now defaults to the native in-process backend instead of Ollama
  Cloud, so it works with no API key. Set `[web] fetch_backend = "ollama"` to
  keep the previous server-side behavior.
- The `--sessions` flag is renamed to `--resume` to match `claude --resume`.
  No deprecation alias — mermaid has no released users yet.

### Fixed

- Weak models no longer hit `unknown tool: web_search` when the web tools
  aren't configured. The system prompt now tells the model to call only tools
  present in its actual tool list, and the Ollama adapter no longer strips
  registered web tools by `OLLAMA_API_KEY` presence — which would otherwise
  have hidden the new keyless native `web_fetch`.
- A completing subagent no longer kills the parent's MCP servers: the child
  `EffectRunner`'s shutdown reaped the process-global MCP manager, so the
  first subagent to finish terminated every MCP server for the rest of the
  session. Child runners now leave the shared manager alone; only the
  top-level runner reaps it on exit.
- Subagent token usage now counts: the child session's provider usage rolls
  up into the parent's session totals and the end-of-run "used N tokens"
  summary (it was silently excluded — invisible spend on paid APIs).
- The system prompt advertised a nonexistent `subagent` tool; the registered
  tool is `agent`. It also now notes that subagent fan-out works in
  `read_only` (children inherit read-only), so models explore in parallel
  instead of assuming the spawn is blocked.
- `read_only` no longer blocks spawning subagents (user-reported): the
  `agent` tool now spawns in every safety mode, because the child inherits
  the parent's live safety mode and each child tool call is re-gated
  individually — a `read_only` child can fan out parallel exploration but
  still can't mutate anything. Operator `Deny` overrides on the subagent
  category/tool and the destructive-prompt hard-deny still block the spawn.

## [0.14.2] - 2026-07-02

### Fixed

- `read_only` no longer blanket-blocks `awk` (user-reported): the ubiquitous
  read-only idioms (`awk '{print $1}'`, field/pattern extraction, `-F`/`-v`)
  now classify as reads, so a pipeline like `… | awk -F/ '{print $1}' | sort`
  runs. `awk` that writes a file (`print > f`), runs a command (`system()`,
  `| "cmd"`), edits in place (gawk `-i inplace`), or loads an external program
  (`-f script.awk`) still classifies as a mutation and stays gated. (A bare
  `>` comparison like `awk '$1 > 5'` is conservatively treated as a write —
  indistinguishable from a redirect without a full awk parser.)

## [0.14.1] - 2026-07-02

### Security

- Closed a shell-classifier bypass found in a full audit: `yq -i` /
  `--inplace` rewrites a file in place but was rated read-only by its
  command name, so it auto-ran in `read_only` and `auto` modes. It (and
  `date -s`/`--set`, which sets the system clock) now classify as mutations.
  Their read-only invocations (`yq . f`, `date`, `date -d …`) are unaffected.

### Fixed

- `read_only` mode no longer blocks genuinely read-only commands
  (user-reported): redirects to the null-device family (`2>/dev/null` and
  friends) count as reads instead of writes; a glued separator
  (`ls 2>/dev/null; echo done`) no longer hard-denies the whole chain as a
  "sensitive `/dev/` write"; and `command -v NAME` — the POSIX binary-exists
  test, which executes nothing — classifies as the lookup it is. Redirects
  to real files, real devices (`/dev/sda`), and sensitive paths (`/etc/…`,
  `~/.ssh/…`) stay blocked, with regression tests pinning both directions.
- The read-only command allowlist gained the common pure-read tools it was
  missing, so they stop needing approval / stop being blocked in
  `read_only`: process/system inspection (`ps`, `groups`, `nproc`, `uptime`,
  `free`, `tty`, `arch`, `vmstat`, `ls{cpu,blk,usb,pci}`), binary/file
  inspection (`xxd`, `od`, `hexdump`, `strings`, `nm`, `objdump`, `readelf`,
  `size`), text tools (`nl`, `tac`, `rev`, `comm`, `join`, `paste`, `fold`,
  `fmt`, `expand`, `unexpand`, `[`), and the remaining checksum families
  (`b2sum`, `sha224/384/512sum`). Tools that can mutate (`strip`, `ldd`,
  `sed`, `awk`) were deliberately left off.

## [0.14.0] - 2026-07-02

### Security

- Daemon: the legacy plaintext socket commands were removed — every mutating
  command now goes through the token-gated JSON surface, so a local process
  can no longer bypass pairing.
- The safety gate's "don't ask again" allowlist no longer matches a command
  that contains a command substitution (an allowlisted prefix can't smuggle a
  `$(…)` payload), and shadow-git checkpoint snapshots skip absolute and `..`
  manifest entries — a crafted entry could previously truncate the very file
  being checkpointed via a self-copy.

### Added

- **`--replay <file>` — deterministic session replay.** A `--record` log now
  replays back through the pure reducer: `mermaid --replay session.jsonl`
  reconstructs the session headless (no model calls, no tool execution, no
  config reads — the log embeds its own config snapshot) and prints the
  transcript plus a determinism verdict. Every replay folds the log twice and
  exits non-zero if the folds diverge, making it a standing canary for
  reducer purity bugs.
- Recording format v1: recordings now start with a self-contained session
  header (config, model, cwd, `--continue` seed) and store every reducer
  input as a full serde round-trip — pasted images and tool artifacts ride
  as base64 and replay bit-exactly. Older (headerless, lossy) recordings are
  not readable; re-record with this version.
- Replay verifies against the live session, not just itself: recordings are
  sealed on clean exit with a fingerprint of the final session state, and
  `--replay` reports whether its fold reproduces the recorded outcome
  (`live match: yes / no / unknown`).
- Recordings no longer store the 60 Hz `Tick` stream (a documented reducer
  no-op, pinned by test) — hours-long recordings shrink from megabytes of
  ticks to just the meaningful inputs, with zero replay fidelity loss.

### Changed

- The reducer is now fully clock-pure: conversation mutations (message
  commits, compaction records, `/clear`'s fresh conversation id) derive
  every timestamp from the injected per-tick clock instead of reading the
  wall clock mid-update. Same recorded log in, same state out — the property
  `--replay` verifies and `tests/replay_determinism.rs` pins in CI.
- CI now builds, lints, and tests the full workspace — the runtime crate's
  test suite (daemon storage, checkpoints, policy, plugins) was silently
  excluded before.
- Dead-code sweep: the unused status-banner subsystem and a set of orphaned
  helpers/wrappers were removed (−544 lines), and the three divergent
  compact-count formatters were unified into one.

### Fixed

- **Cancelling (or quitting) mid-tool-execution no longer poisons the next
  turn.** Orphaned tool calls are sealed with cancelled placeholders, so the
  follow-up request can't be rejected for a dangling tool call; a message
  queued mid-turn no longer leaks across `/load` or `/clear` into the wrong
  conversation; and a mid-turn system notice can no longer split an
  assistant's tool call from its result (another next-turn rejection).
- **Headless runs finally see your project.** `mermaid run` and daemon tasks
  now load `AGENTS.md`/`MERMAID.md` project instructions and durable memory,
  matching interactive sessions; subagents load them synchronously instead of
  racing their first model call.
- OpenAI-compatible providers: assistant tool calls are wire-conformant
  (typed `function`, stringified arguments) — strict endpoints no longer
  reject the second turn — and image attachments are actually sent to
  vision models.
- MCP: a hung server can no longer wedge a turn (tool calls time out after
  5 minutes), and servers with paginated tool lists advertise all of their
  tools instead of just the first page.
- Repeated OS signals are all handled — previously the SIGINT/SIGTERM/SIGHUP
  handlers fired once and went quiet, so a second Ctrl+C from outside the
  TUI did nothing.
- Daemon: the accept loop survives transient connection errors instead of
  exiting, idle connections time out, and plugin hooks can no longer
  deadlock on large stdin payloads.
- Wide (CJK) characters no longer overflow truncated status lines, and
  concurrent config saves can no longer interleave and corrupt the file.
- A draw error during shutdown no longer skips MCP child cleanup and
  pending session saves.
- Release pipeline: the publish workflow verifies the tag matches the crate
  version, changelog extraction works from shallow checkouts, and the
  packaged systemd unit is generated from the same source as
  `mermaid daemon install` (with a drift-guard test).
- Clipboard operations can no longer hang Mermaid. Every clipboard subprocess
  (`wl-paste`/`wl-copy`, `xclip`, `pbpaste`/`pbcopy`, `osascript`, PowerShell)
  now runs under a kill-on-timeout deadline, so a frozen selection owner or a
  stale display connection surfaces as a visible paste/copy error within
  seconds — instead of a paste that silently never lands, a permanently leaked
  blocking thread, and a stuck child process that could stall shutdown.

## [0.13.0] - 2026-06-30

### Security

- **Fixed a critical sandbox bypass.** A destructive command hidden inside a
  command substitution (`$(…)` / backticks / `<(…)`), or obfuscated with `${IFS}`
  word-splitting or interior `..`, could be classified as read-only and auto-run
  with no approval in `read_only`, `ask`, and `auto` modes. The policy gate now
  recurses into substitutions and normalizes these forms, and fails safe when a
  command is nested too deep to fully analyze — so a hidden `rm -rf /` can no
  longer ride a benign-looking outer command. The gate is shell-aware end to end,
  so flag reordering, glued operators, and quoting can't downgrade a command's risk.
- Approval replay is confined through the same symlink-safe path checks
  (`openat2`) as the live path, and re-verifies a command isn't destructive before
  re-running it.
- Secrets are redacted more thoroughly (key-name-aware, more token formats), the
  config file is written `0600`, MCP child processes start from a clean
  environment, and terminal escape sequences in tool output are neutralized.
- MCP: package names are validated (no argument injection via a leading dash),
  and a provider `base_url` override that would send your API key to a
  non-loopback host must use HTTPS and warns you which host will receive the key.

### Fixed

- **A stalled turn no longer ends the run silently.** When the model spends a turn
  "thinking" but produces no reply and no actions, Mermaid auto-retries the
  request once (nudging the model) instead of leaving you at a finished timer with
  no output; if it's still empty, you get a clear hint instead of silence.
- An abnormally-closed model stream is surfaced as an error instead of being
  mistaken for a complete (empty) response — across all providers.
- Project instructions: `MERMAID.md` keeps its precedence even when the combined
  `AGENTS.md` + `MERMAID.md` exceed the size cap, a single unreadable instruction
  file no longer drops the others, and Windows home-directory resolution is fixed.
- Checkpoint restore is memory-bounded (one file at a time) and rollback is
  crash-safe — a failed restore can be rolled back in full, including non-empty
  directory subtrees.
- Assorted robustness fixes: idempotent daemon fallbacks, ownership-scoped task
  reconciliation (won't clobber a live session's task), deterministic MCP tool
  ordering, and per-model provider capability handling for current models.

## [0.12.2] - 2026-06-29

### Added

- A full-width gray highlight band behind your submitted prompts (Claude-Code
  style), keeping the `>` marker, so your messages stand out in the transcript.
- An end-of-run indicator: when an agentic run finishes, a dim "Worked for {time}
  · used {N} tokens" line appears where the spinner was — so a completed run has
  closure and you can see how long it took. It's display-only (never sent back to
  the model).

### Removed

- The chat transcript scrollbar. The transcript now spans the full pane width
  (the reserved right-hand gutter column is reclaimed); scrolling is unchanged.
- The per-turn "Reasoning hidden" placeholder line. With reasoning hidden (the
  default), turns now collapse silently instead of printing a
  `Reasoning hidden (/visible-reasoning on to show)` notice on every reasoning
  turn. `/visible-reasoning on` still reveals the thinking.

### Fixed

- **Markdown tables now render aligned instead of mangled.** Table lines are
  flagged preformatted so they're no longer word-wrapped (which collapsed their
  column padding), and tables wider than the terminal size their columns to fit
  and wrap long cell text within the column — nothing is lost and no row overflows.
- **Cloud (`:cloud`) Ollama models now use their full context window.** They run on
  Ollama's servers, not your local GPU, so Mermaid no longer VRAM-clamps them —
  e.g. `minimax-m3:cloud` uses its full ~524k-token window instead of being
  auto-fit down to your GPU (which it never touches). An explicit `/context <n>`
  still caps it if you want.
- Manual `/compact` on a conversation with too little history to summarize now
  shows a calm "Nothing to compact" note instead of a misleading
  "Compaction failed: Invalid request" error. Genuine compaction failures still
  report as failures.

## [0.12.1] - 2026-06-29

### Added

- **Automatic Ollama context sizing — you never touch Ollama config.** Mermaid
  probes an Ollama model's real context window + architecture dimensions
  (`/api/show`, cached in `provider_probes`) and auto-fits `num_ctx` to your GPU's
  VRAM so the model stays on the GPU. CPU/RAM offload is 5–20× slower, so it's off
  by default; the new `[ollama]` config keys `allow_ram_offload` (default `false`)
  and `max_auto_num_ctx` tune this. The status bar and `mermaid model-info` now
  report the real window instead of "unknown", and auto-compaction works for
  Ollama for the first time (it was silently disabled when the context limit was
  unknown).
- **Ollama context auto-converges to the real GPU fit.** Auto-fit is an estimate,
  so Mermaid now checks where the model actually loaded after each turn
  (`/api/ps`). If it spilled into CPU/RAM while offload is off, Mermaid shrinks
  `num_ctx` to the largest window that clears the measured overflow and reloads at
  it next turn, repeating until the model is fully resident on the GPU — or warning
  you once when even the minimum window can't fit (e.g. the weights alone exceed
  your free VRAM). `/context` reports the fitted window as `auto (GPU-fit)`, and
  `/context <n>` / `/context offload on` still override it.
- **Mermaid auto-compacts and continues when the context window fills.** On a small
  window (e.g. a local model auto-fit to a modest GPU), a response that hit the
  window mid-turn used to stop with a hint. Mermaid now compacts the conversation
  and resumes the run automatically, bounded by a per-run cap that resets whenever
  the run makes progress (so it only ever stops genuine no-progress thrashing). A
  new `[compaction]` config key, `max_truncation_recoveries`, tunes the cap
  (default `3`; `0` = uncapped).

### Changed

- Removed emojis from all user-facing output; status messages, warnings, and
  indicators now use plain-text markers.

### Fixed

- **Ollama responses no longer truncate early.** `max_tokens` is now forwarded to
  Ollama as `num_predict` (plus reasoning-aware headroom), bounded by the context
  window. Previously it was never sent, so a reasoning model would stop only when
  the tiny default window filled (`done_reason=length`).
- **The live token counter and spinner track the whole run.** The counter no longer
  sits at `0` during the thinking phase — it climbs as tokens stream — and the
  spinner plus its elapsed/token counters now persist across every tool step of an
  agentic run instead of resetting at each model call, so a long multi-step run
  shows one continuous, growing total.
- **Wrapped Markdown keeps its left margin.** Long assistant paragraphs no longer
  flush to column 0 when they wrap, and a wrapped bullet or numbered list item now
  hangs under its marker text instead of snapping back to the message gutter.

## [0.12.0] - 2026-06-28

### Security

- **Daemon, checkpoint, and storage hardening (review axis 3).**
  - Approval replay is now single-shot — a *denied* approval can no longer be
    resurrected as approved, and a stored action can't be replayed N times.
  - `restore_checkpoint` confines every restored path to the checkpoint's
    recorded project root; a tampered manifest can no longer write or delete
    files outside it (absolute paths and `..` escapes are rejected). The
    approval-replay exec path gets the same containment.
  - Pairing tokens are matched in constant time (no SQL `=` timing channel); the
    unauthenticated `pairings` socket command that exposed token hashes is
    removed; `logs` now requires the pairing token; daemon snapshots redact token
    hashes.
  - On Windows the data dir (SQLite DB with token hashes + transcripts) is locked
    to the current user via `icacls` instead of inheriting default ACLs.
  - Checkpoint shadow-git commands run with hooks disabled, and checkpoint /
    plugin manifests are written atomically.

### Fixed

- **Headless `mermaid run` output is no longer corrupted by a subagent.** A
  subagent's runner emitted an OSC 2 terminal-title escape (`\x1b]2;…`) into
  stdout even in headless mode (it didn't inherit the parent's title
  suppression), producing invalid `--format json` — and stray bytes in
  `text`/`markdown` — whenever the `agent` tool ran.
- **`mermaid run ""` now errors instead of silently doing nothing.** An empty or
  whitespace-only prompt is rejected at parse time (`prompt cannot be empty`,
  exit 2) rather than producing no output with a success exit code.
- **`mermaidd` no longer starts (or clobbers) the daemon when probed.** It
  ignored all arguments and went straight to binding the control socket —
  removing any existing one first — so `mermaidd --version`/`--help`/a typo would
  boot a foreground daemon, and doing so while the managed daemon was running
  would unlink its socket and orphan it. `mermaidd` now answers
  `--version`/`--help`, rejects unknown arguments (exit 2), and refuses to start
  when a live daemon already holds the socket (only a stale socket is removed).
- **Provider-adapter correctness (review axis 4).**
  - Truncation (`max_tokens`) and content-filter / safety refusals are no longer
    silently treated as a clean finish: a `⚠ truncated` note now appears, and a
    refusal that produced no usable content ends the turn with a clear error
    (Gemini's streaming path now matches its non-streaming behavior, applied
    across all adapters).
  - Anthropic streams cut mid-message (a proxy `Connection: close` without
    `message_stop`) no longer drop a fully-streamed tool call.
  - 429s now honor the server's `Retry-After` (capped at 60s) instead of a fixed
    ~1.5s backoff, surface as a typed rate-limit error, and every retry backoff
    is jittered to avoid synchronized retries.
  - OpenAI cached input tokens are no longer double-counted in the input total.
  - OpenAI-compat non-streaming responses strip inline `<think>` tags; the
    temperature is clamped to 0–2 for OpenAI-compat and Ollama.
- **Concurrency, runtime & MCP hardening (review axis 5).**
  - A slow or hung **plugin hook no longer freezes the app**: hooks now run off
    the event loop (`spawn_blocking`) and are killed if they overrun a 30s
    bound, instead of a synchronous `child.wait()` with no timeout.
  - **MCP servers are now gracefully shut down on exit** (stdin-EOF → terminate →
    kill ladder) instead of being orphaned, and `/mcp` stop actually kills the
    server's child rather than only updating the UI.
  - A flaky MCP server no longer slowly leaks request slots — the pending-request
    map entry is removed on timeout/error.
  - A cancelled foreground command now tree-kills its process group, so a
    grandchild it forked (`sh -c "server &"`) isn't orphaned.
  - Restarting a managed process waits (bounded) for the old PID to exit before
    respawning, avoiding a port clash with its predecessor.

### Changed

- **BREAKING — pairing tokens now expire.** New tokens default to a 30-day TTL.
  `mermaid pair` becomes `mermaid pair create [--label L] [--ttl-days N]`
  (`--ttl-days 0` = never expires), plus `mermaid pair list` and `mermaid pair
  revoke <id>`. Existing tokens get a 30-day grace window from first upgrade.
- **BREAKING — plugins install disabled.** `mermaid plugin install` no longer
  auto-enables a plugin; run `mermaid plugin enable <id>` (which now prints the
  plugin's declared capabilities) to activate its hooks. The manifest
  `permissions` field is renamed `capabilities` and documented as advisory
  disclosure, not a sandbox.
- **Provider `base_url` now requires HTTPS for non-local hosts.** A custom or
  overridden provider endpoint on plain `http://` to a public host is refused (it
  would send the API key in cleartext); `http://localhost` and private hosts stay
  allowed for local model servers (Ollama, vLLM).

## [0.11.1] - 2026-06-23

### Fixed

- **Status line no longer bleeds off-screen.** A long `Running tools: <cmd> …
  (esc to interrupt …)` now splits onto two rows when it doesn't fit and
  truncates each row to the terminal width — nothing overflows, including
  unbreakable file paths. The reserved height is stable and capped so the input
  box can't be evicted on a short terminal.
- **Esc never exits.** A second Esc while a turn was already cancelling used to
  quit mermaid (and could leave a backgrounded process holding the terminal). Esc
  now only cancels; only Ctrl+C / Ctrl+D / `/quit` exit.
- **Diff backgrounds fill the whole row.** Tab-indented diffs no longer show a
  ragged "staircase" — tabs are expanded so the red/green bar spans the full
  width, and tab indentation is now visible.
- **Quieter tool execution.** Live tool output (build lines, pids, streamed file
  contents) no longer flickers a transient line above the input; the status line
  names the running tool and full output stays in the transcript.
- **Ollama cloud models work on first use.** `mermaid --model <name>:cloud` no
  longer fails at startup trying to `ollama pull` a cloud model — cloud models
  are served by the daemon and skip the local pull.
- **Markdown loose-list bodies hang-indent** under their bullet instead of
  dropping flush to the left margin.
- **Installer takes PATH precedence** and warns when another `mermaid` (e.g. a
  stale `cargo install`) earlier on PATH would shadow the install.

### Added

- **Homebrew + Scoop + WinGet.** `brew install noahsabaj/mermaid/mermaid`,
  `scoop install mermaid`, and `winget install NoahSabaj.Mermaid` (once accepted
  upstream). All three are bumped automatically by the release pipeline.

## [0.11.0] - 2026-06-22

### Added

- **Install without cargo.** One-line installers download a prebuilt binary for
  your platform from the latest GitHub Release, verify it against `SHA256SUMS`,
  and put `mermaid` + `mermaidd` on your PATH — no Rust toolchain needed:
  - macOS/Linux: `curl -fsSL https://noahsabaj.github.io/mermaid-cli/install.sh | sh`
  - Windows: `irm https://noahsabaj.github.io/mermaid-cli/install.ps1 | iex`

  Honor `MERMAID_VERSION` (pin a release), `MERMAID_INSTALL_DIR`, and
  `MERMAID_NO_MODIFY_PATH`. The scripts are served from GitHub Pages and stay
  canonical in the repo.
- **`mermaid update`.** Checks GitHub Releases for a newer version and updates
  in place by re-running the platform install script (`--check` to only report,
  `--force` to reinstall). Reuses the existing HTTP client — no new
  dependencies. On Windows the installer renames the running `mermaid.exe` aside
  so an in-place update succeeds.

## [0.10.2] - 2026-06-22

### Added

- **Background processes on Windows.** `execute_command` `mode="background"`
  previously errored with "not supported on Windows yet"; it now works — the
  command is spawned detached (`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`)
  with output redirected to a log file, and `/processes`, `/logs <id>`, and
  `/stop <id>` function (process liveness via `tasklist`, stop via
  `taskkill /T /F` tree-kill).
- **Ctrl+B sends a running foreground command to the background.** While an
  `execute_command` runs in the foreground, press `Ctrl+B` to detach it — it
  keeps running as a `/processes` entry, tail-able via `/logs`, instead of
  blocking the turn (the "oops, I ran the dev server in foreground" rescue, as
  in Claude Code). Its output is teed to a log file so it stays viewable. The
  status line advertises the shortcut while tools run. A backgrounded process is
  session-scoped (it stops when Mermaid exits).

### Changed

- **System prompt steers agents off blocking commands.** For smoke checks,
  prefer a finite command (a build, a one-shot test run, `--version`) over a
  dev server or watcher (which never exit and block until the 30s foreground
  timeout); use `mode="background"` for servers/daemons/watchers.

## [0.10.1] - 2026-06-22

### Fixed

- **"Running tools…" looked frozen.** The tool-execution status line was stuck
  at `0s` and didn't say what was running, so a slow tool call (e.g. an agent
  running `npm run dev` as a smoke test — a dev server that blocks until
  `execute_command`'s 30s timeout) looked hung. `TurnState::ExecutingTools` was
  the only active turn state without a `started` timestamp, so the elapsed clock
  was hard-coded to 0 (Generating/Compacting tick normally). It now carries a
  start time and counts up, and the status line **names the in-flight tool**
  (e.g. `Running tools: Bash npm run dev`, with `+N more` when several run in
  parallel). Not a hang — `Esc` always aborted it.

## [0.10.0] - 2026-06-22

Headline: **durable, agent-managed memory** — Mermaid now remembers facts across
sessions in plain Markdown files it reads and maintains itself. Alongside it, a
batch of TUI upgrades (richer markdown, drag-select + copy, a chat scrollbar,
inline approval prompts with an arrow-key picker, the version in the footer) and
a round of paste/safety/diff polish.

### Added

- **Durable agent memory.** Mermaid keeps long-term semantic memory as plain
  Markdown files — one atomic fact per file — across three scopes: global,
  project-private (the default; machine-local, not committed), and
  project-shared (opt-in, committed to `.mermaid/memory/`). An auto-derived
  index is always in context; the agent reads a fact on demand via `read_file`
  and maintains memory itself through a `memory` tool (`remember` / `update` /
  `forget`), ungated in every safety mode except read-only. Manual controls:
  `/memory` lists, `/remember <fact>` saves, `/forget <name>` deletes, and
  `/consolidate-memory` runs a model-assisted, checkpoint-reversible **prune**
  of duplicate/stale facts (prune-only by design — stored facts are never
  rewritten, which avoids semantic drift). Secrets, tokens, and PII are never
  stored. The index is generated from the files, so it can't drift from them;
  no database, vectors, or embeddings.
- **Version in the status footer.** The footer's second line now reads
  `mermaid vX.Y.Z · safety: <mode> · reasoning: <level>` — the version tracks
  the crate version automatically.
- **Chat scrollbar.** The transcript now shows a scrollbar (ratatui 0.30's
  `Scrollbar`) in a reserved right-hand gutter whenever it overflows the
  viewport, so you can see scroll position at a glance. Dropped the unused
  `palette` and `macros` ratatui features for a leaner build; kept
  `scrolling-regions`.
- **Richer markdown rendering.** Assistant markdown is now theme-aware
  (headings, lists, blockquotes, links, tables all use the active theme's
  palette instead of hardcoded ANSI colors), fenced code blocks get **in-house
  syntax highlighting** (keywords / strings / line-comments via a small
  language-agnostic lexer, no new dependency) on the theme's code background,
  code-block **indentation is preserved** (code lines are no longer word-wrapped
  into a collapsed paragraph; lines wider than the viewport soft-wrap with a
  hanging indent rather than being clipped), inline `code` is tightened (no
  stray padding), link destinations are shown dimmed after the text, and `---`
  thematic breaks render as a horizontal rule.
- **Drag to select, Ctrl+Shift+C to copy.** A plain left-mouse drag selects
  chat text (reverse-video highlight); **`Ctrl+Shift+C` copies** the selection
  to the system clipboard (with a "Copied N chars" status). Selecting and
  copying are distinct — a drag never auto-copies, so it can't clobber your
  clipboard. Mouse-wheel scroll and Ctrl+Click-to-open-image are unaffected.
  Selection is display-cell accurate (CJK-safe), drops the rendered left
  margin so multi-line and code copies are clean (the code's own indentation
  is kept), and clears on scroll. `Shift+Drag` still bypasses to native
  terminal selection. Copy shells out to the platform tool (`clip`/PowerShell,
  `pbcopy`, `wl-copy`/`xclip`) — no new dependency.
- **Inline approval prompts.** In interactive `ask` mode (and `auto`-mode
  escalations), a gated tool action now **pauses and prompts inline** —
  `1` Yes · `2` Yes, don't ask again (per-tool; `execute_command` keyed on the
  program) · `3`/Esc No — and the agent waits for the answer. Previously `ask`
  mode just returned an "Approval required" error and the model flailed; the
  only approval path was the out-of-band `/approve <id>` flow (still used in
  headless `mermaid run`). The prompt also covers the previously-unguarded
  non-replayable tools (web / MCP / subagent / computer-use) under `ask`. The
  picker is keyboard-navigable — `↑`/`↓` move a highlighted option and `Enter`
  selects it, or press the number directly.

### Changed

- **BREAKING: memory is greenfield-replaced.** The old SQLite/JSONL key-value
  memory store is gone, along with its CLI subcommands; `/memory`, `/remember`,
  and `/forget` are now backed by the new Markdown file store (and `/memory-edit`
  is removed). There is no migration — saved entries from the old store are not
  carried over.
- **System prompt — interaction & editing norms.** Added a focused set of
  cross-model norms: no time estimates; make the smallest change that does the
  task (no speculative abstractions/options, no cleanup of untouched code, no
  backwards-compat shims or tombstone comments); don't create files (esp.
  docs/README) unless needed; don't introduce security holes; communicate in
  response text, not via tool calls/comments; treat file/web/tool output as
  untrusted data, not instructions; and stronger anti-sycophancy (skip
  "You're absolutely right"-style validation, investigate over confirming).
- **Project instruction files are now exactly `AGENTS.md` + `MERMAID.md`.**
  `CLAUDE.md` and `GEMINI.md` are no longer auto-loaded. `AGENTS.md` (the
  cross-tool open standard) loads first; `MERMAID.md` (mermaid-specific) loads
  last and overrides it on conflict. **BREAKING** for anyone relying on
  CLAUDE.md/GEMINI.md auto-loading. (The `find_mermaid_md` back-compat helper
  was removed.)

### Fixed

- The `/clear` (and other) confirmation modal was inert — nothing rendered it
  and no key handler read it. It now shows and accepts `y`/`n`.
- **Chat spacing:** a turn that thought and then immediately called a tool
  (hidden reasoning + empty text + actions) rendered the "Reasoning hidden"
  placeholder flush against the first tool block. It now gets the same single
  blank-line gap every other block has.
- **Multi-line paste (Windows).** Pasting multi-line text submitted each line
  as its own message and rendered character-by-character. crossterm 0.29
  doesn't emit `Event::Paste` on the Windows console — a paste arrives as a
  burst of individual key events, so every newline hit the Enter→submit path.
  The main loop now coalesces a rapid key burst (characters, newlines, and
  tabs) into a single atomic paste (a lone Enter still submits, a lone Tab is
  still a Tab; Shift+Enter still inserts a newline), and the input box renders
  embedded newlines as real rows.
- **System prompt** refreshed: it now documents the safety/permission modes
  (and how to behave when an action is gated — explain, don't spam retries),
  the tool set, and the in-session controls.
- **Pasted text scrambled on Windows.** A paste whose burst split into coalesced
  `Paste` chunks plus stray `Char` key events (uppercase letters) came out
  reordered — e.g. `Review … Define … Report …` became `RDReview … efine …
  eport …`. Paste now inserts at the cursor and advances it, exactly like
  typing, so the result stays in order however the burst splits.
- **The model couldn't see the live safety mode.** After switching modes
  mid-session (e.g. `read_only` → `full_access`), the agent kept refusing
  actions based on a stale gate error. The current mode is now surfaced in the
  prompt each turn (the same field the policy gate enforces), so it stops
  guessing from old errors.
- **Slash palette hid hyphenated commands.** Typing a command's first word
  (e.g. `/consolidate`) matched nothing until the hyphen was typed; plain
  prefix matching now surfaces `consolidate-memory`, `cloud-setup`, etc.
- **Duplicate `/compact` indicator.** Removed the redundant gray "Compacting
  context…" status line; the live blue indicator and the completion receipt
  remain.
- **Diff header clutter.** File diffs no longer print the `---`/`+++`/`@@`
  unified-diff headers above the change — just the success line and the colored
  body.

### Security

- Bumped `quinn-proto` 0.11.14 → 0.11.15 for RUSTSEC-2026-0185 (a remote
  memory-exhaustion DoS in out-of-order QUIC stream reassembly).

## [0.9.0] - 2026-06-21

Headline: a **classifier-backed Auto safety mode** (as in Claude Code / Codex)
plus in-session mode switching. The minor bump reflects the breaking rename of
the `AutoReview` mode.

### Added

- **Auto safety mode** — a classifier-backed permission mode (as in Claude Code
  and Codex). Under `[safety] mode = "auto"`, borderline actions (shell /
  network / external tools) are vetted by an LLM against your stated intent:
  aligned actions run automatically, risky or off-task ones escalate to an
  approval prompt. Reads and file edits still auto-run (with checkpoints), and
  destructive patterns stay hard-denied by the rule engine. The classifier
  defaults to the session model, overridable via `[safety] auto_classifier_model`;
  any classifier error or timeout fails safe (escalate), never silently allows.
- **In-session safety switching** — `Shift+Tab` cycles `read_only → ask → auto →
  full_access`, and `/safety [mode]` (alias `/permission`) shows or sets it.
  Both are session-scoped (the `[safety] mode` config value remains the
  persistent default), and the status footer now always shows the active mode.

### Changed

- **BREAKING:** the `AutoReview` safety mode is renamed to `Auto`, and its
  behavior changed from rule-based ("ask for everything risky") to
  classifier-backed. Config files with `[safety] mode = "auto_review"` must
  change the value to `"auto"` — the old string is no longer accepted.
- Bumped dependencies: `rusqlite` 0.39 → 0.40, `sha2` 0.10 → 0.11, `getrandom`
  0.3 → 0.4.

### Fixed

- Loosened a flaky timing assertion in the cancellation/timeout integration
  test (`execute_command_timeout_honored`) that intermittently failed on loaded
  CI runners; it now measures the real timeout behavior with a generous ceiling.

### CI

- Bumped pinned GitHub Actions: `actions/checkout` → v7, `actions/download-artifact`
  → v8, `actions/upload-artifact` → v7, `softprops/action-gh-release` → v3.

## [0.8.1] - 2026-06-21

Documentation, release-pipeline, and supply-chain fixes on top of 0.8.0. This
is the first 0.8.x release published to crates.io (the workspace split had
broken crates.io publishing after 0.7.1).

### Changed

- **README** corrected against the code: repointed the install instructions
  (GitHub Release binaries / `cargo install --git`, since crates.io can lag),
  fixed the Alt+T reasoning cycle (all seven levels) and the daemon defaults
  (TCP off by default via `MERMAID_DAEMON_ENABLE_TCP`, socket `0600` / data dir
  `0700`), documented `mermaid pr create`, the `MERMAID_ALLOW_PLUGIN_FETCH`
  plugin opt-in, and the `[safety]` config section.

### Fixed

- **Restored crates.io publishing.** `mermaid-runtime` is now publishable and
  the release workflow publishes it before the `mermaid-cli` binary crate
  (the path dependency now carries a version requirement).

### Security

- Pinned every GitHub Action to a commit SHA and added a Dependabot config to
  keep them current; the release workflow now attaches a `SHA256SUMS` file to
  each GitHub Release so downloaded binaries can be verified.

## [0.8.0] - 2026-06-21

Security-hardening release: the full-codebase review's critical/high findings
are fixed, dependency CVEs are patched, and the safety defaults are now
safe-by-default. Also adds Git-host PR creation.

### Added

- **`mermaid pr create`.** Create a pull/merge request from the current
  branch via the host's own CLI (`gh` for GitHub, `glab` for GitLab),
  reusing its existing authentication. Auto-detects the host from the
  `origin` remote (overridable with `--provider`), and supports `--title`,
  `--body`, `--summary <file>` (attach a review summary), `--base`,
  `--draft`, and `--web`. (#2)

### Changed

- **BREAKING: default safety mode is now `Ask` (was `FullAccess`).** A fresh
  install prompts for approval on mutations / shell / network actions instead
  of auto-running them. Set `[safety] mode = "full_access"` in config to
  restore the old behavior.
- **BREAKING: the daemon TCP control listener is off by default.** Opt in with
  `MERMAID_DAEMON_ENABLE_TCP=1` (the old `MERMAID_DAEMON_DISABLE_TCP` toggle is
  gone), and auth is now required for every TCP command including `health`.
- **BREAKING: installing a plugin from a Git URL now requires
  `MERMAID_ALLOW_PLUGIN_FETCH=1`** and no longer auto-expands a bare
  `owner/repo` into a GitHub URL; the clone runs with repo hooks and external
  transports disabled.
- Shell-command risk classification was rewritten to tokenize the command:
  unknown commands now require approval instead of being treated as read-only,
  and network/interpreter commands (`curl`, `wget`, `ssh`, `python -c`, …) are
  classified as network/process actions.

### Security

- The safety policy is now enforced for **every** dangerous tool. Previously
  `web_*`, `mcp`, `subagent`, and the computer-use tools bypassed it entirely,
  so `ReadOnly` silently failed to block them; a single gate now covers them.
- Provider API keys and the daemon token are scrubbed from the environment of
  commands spawned by `execute_command`, MCP servers, and plugin hooks.
- Filesystem path containment now resolves through the canonical
  nearest-existing ancestor (closes symlink-follow / TOCTOU and
  symlinked-parent-on-create escapes) and fails closed.
- Daemon control socket is created `0600` and the data dir `0700`; the
  conversation `/load` id is validated against the generated format.
- Bounded the previously-unbounded command output capture and the streamed
  tool-call index (anti-OOM/DoS).
- Session, compaction-archive, and checkpoint writes are now atomic
  (temp + fsync + rename); SQLite opens with WAL + `busy_timeout`.
- Patched **12 RUSTSEC advisories** via dependency updates: `aws-lc-sys`
  0.37 → 0.41, `rustls-webpki` → 0.103.13, plus `bytes`, `quinn-proto`, `time`.

### Fixed

- Streaming `Done` no longer races ahead of buffered tool calls (the
  intermittent "model forgot to call the tool" bug).
- Token estimates now count assistant tool-call argument bytes, fixing
  systematic under-compaction that could overflow the provider context.
- Anthropic: drop assistant `thinking` blocks that lack a signature (they
  caused a 400 on the next turn). Gemini: a safety/recitation-blocked response
  is surfaced as a structured error instead of a misleading parse failure.
- Compaction now persists the archive before overwriting the (message-stripped)
  conversation, so a failed archive write can no longer lose messages.

## [0.7.1] - 2026-04-26

Runtime hardening, typed tool output, token accounting, and context
compaction on top of the v0.7 architecture.

### Added

- **Manual and automatic context compaction.** `/compact [instructions]`
  now creates a model-visible checkpoint, archives the removed raw
  messages under `.mermaid/compactions/`, and replaces old history
  with a structured handoff plus the most recent turns. Mermaid also
  auto-compacts near the model's context limit and retries once after
  provider context-limit errors.
- **Typed tool-result metadata.** Tool outcomes now carry structured
  status, duration, line counts, byte counts, result counts, artifacts,
  and tool-specific metadata. The TUI renders friendly summaries such
  as read/write line counts, web-search result counts, command exit
  status, and background process details without scraping model-facing
  text.
- **Runtime metadata layer.** `domain::runtime` tracks lifecycle
  signals, provider capability snapshots, managed background
  processes, tool metadata, and a lightweight runtime timeline.
- **Background command registration.** `execute_command` can run
  long-lived commands in background mode, capture startup logs, detect
  local URLs, and register PID/log metadata for Mermaid to display and
  persist.
- **Subagent tool.** The `agent` tool can spawn autonomous child agents
  using the active model/provider, with depth limits and a child tool
  registry that excludes unsafe/self-recursive tools.
- **Computer-use tools.** The v0.7 tool registry now includes the
  screenshot, click, mouse-move, keypress, type-text, scroll, and
  window-list computer-use tools.
- **Chat image artifacts.** Tool-produced images can be attached to
  assistant messages, rendered in the chat history, and opened from the
  TUI.
- **Lifecycle signal handling.** SIGINT, SIGTERM, and SIGHUP now flow
  through reducer messages so Mermaid can restore the terminal and save
  state consistently.
- **Context and usage slash commands.** `/usage` and `/context` report
  provider token usage, session totals, estimated prompt budget, model
  context capacity, and recent compaction metadata.

### Changed

- **v0.6 runtime deleted.** The `MERMAID_V7=1` opt-in is gone; the
  v0.7 architecture is now the only code path. `src/tui/`,
  `src/runtime/`, `src/agents/`, `src/models/backend.rs`, and
  `src/models/retry.rs` are all removed. Net ~8,000 LOC of old code
  gone from the tree.
- Non-interactive `mermaid run <prompt>` now runs on the v0.7 reducer
  + effect runner (same as interactive); output shape matches the
  v0.6 `NonInteractiveResult` so scripts keep working.
- Slash commands, diff helpers, action value types, MCP manager
  accessor, and the web search client all moved out of the v0.6
  namespace into `src/domain/`, `src/render/`, `src/mcp/`, and
  `src/providers/tool/`. No behaviour changes — just no longer
  reaching back into deleted modules.
- Token accounting now distinguishes provider-reported usage from
  local estimates. The footer shows current context usage separately
  from last API usage and cumulative session totals, avoiding the old
  inflated "session tokens" display.
- Model/provider requests now use a stream bridge shared across
  providers, making cancellation and done/usage events more uniform.
- Terminal teardown now restores raw mode, mouse capture, bracketed
  paste, and the alternate screen before asynchronous shutdown work
  drains.

### Fixed

- Ctrl+C from an idle, empty TUI exits and restores the user's terminal
  reliably instead of requiring repeated keypresses or leaking terminal
  escape sequences back into the shell.
- Cancelled turns now drain through `TurnCancelled`, preventing stale
  provider/tool events from leaving the reducer stuck in `Cancelling`.
- Tool cancellation now returns typed cancelled outcomes instead of
  relying on textual placeholders.
- Stale screenshots are evicted from outgoing model requests while the
  latest relevant image remains available in chat history.
- Gemini API key resolution now documents and preserves the
  `GEMINI_API_KEY` legacy fallback alongside `GOOGLE_API_KEY`.

### Removed

- Two integration test files that exercised the v0.6 runtime
  (`tests/agent_loop_tests.rs`, `tests/tui_behavior_tests.rs`). The
  reducer + effect parity suites (`tests/reducer_flows.rs`,
  `tests/effect_cancel.rs`) cover the equivalents.

### Added (free, via the new architecture)

- MCP servers initialize automatically at startup via
  `Cmd::InitMcpServers`. v0.6 only init'd in the interactive path;
  non-interactive invocations now get MCP tools too.
- `manager_ref::wait_ready()` — if a tool call races startup, it
  parks briefly for init to complete instead of immediately
  erroring.
- `--record <file>` now records structured reducer input events,
  including lifecycle and compaction events, for replay/debugging.

### Docs

- Updated the architecture, adding-tools, replay-debugging, and README
  docs for the v0.7-only runtime, typed metadata, background commands,
  computer-use path changes, and current provider key behavior.

### Tests

- Added regression coverage for terminal-mode restoration on Ctrl+C,
  context compaction planning/replacement, slash-command parsing,
  compact event rendering, token-status rendering, background command
  metadata, and subagent/tool registry behavior.

## [0.7.0] - 2026-04-21

The Architecture Release. This is a big-bang rewrite of Mermaid's
runtime on the Elm/MVU pattern: one pure reducer, effects as data,
structured concurrency per turn. External behaviour is intended to
match v0.6; several whole classes of bug that v0.6 let slip become
impossible to express against the new types.

The new path ships behind `MERMAID_V7=1` for the v0.7.0 release so
the v0.6 runtime keeps running by default during the migration
window. Flipping the default happens in a follow-up once the v7
path has been exercised against real sessions.

### Added

- **Pure reducer** (`src/domain/reducer.rs`) — `fn update(State, Msg)
  -> (State, Vec<Cmd>)`. Synchronous. Stale events filter by embedded
  `TurnId` before any state transition. Tool-result completeness is
  type-enforced (`Vec<Option<ToolOutcome>>` can't advance to the
  follow-up call until every slot is `Some(_)`). Exhaustive match on
  `Msg`; clippy catches any missing variant.
- **Effect runner** (`src/effect/`) — the single place in the
  codebase where tokio tasks spawn. Owns per-turn `TurnScope`
  (`CancellationToken` + `JoinSet`) so cancellation is a signal, not
  a poll. Retry/tracing middleware (from v0.6's
  `src/models/retry.rs`) now wraps any adapter uniformly.
- **`ModelProvider` + `ToolExecutor` traits** (`src/providers/`) —
  the adapter surface. Four model providers (Ollama, Anthropic,
  Gemini, OpenAI-compat) + six built-in tools (read_file,
  write_file, edit_file, delete_file, create_directory,
  execute_command) all implement these. MCP dispatch lives at
  `tool::McpToolProxy`.
- **`StreamContext` + `ExecContext`** — typed per-call contexts
  carrying the turn's cancellation token. Providers and tools that
  ignore the token don't get past code review; the type signature
  makes the race explicit.
- **Pure view function** (`src/render/`) — `fn render(&State, &mut
  Frame)`. Never mutates state, never performs I/O, never holds a
  `&mut App`. Testable against ratatui's `TestBackend` without a
  runtime or terminal.
- **Single event loop** (`src/app/run.rs`) — one `tokio::select!`
  over crossterm `EventStream` + effect-result mpsc + tick timer.
  Replaces v0.6's two competing event loops. Behind
  `MERMAID_V7=1`.
- **`TerminalGuard`** — raw-mode/alt-screen setup with panic-safe
  teardown. A panic mid-render restores the shell.
- **Recorder / Replay** (`src/app/recorder.rs`) — JSONL msg logs.
  Reducer is event-sourced by design, so record is one line per
  reducer input and replay is a fold. Regression tests as flat
  files; bug reports as replay logs.

### Changed

- `ExecuteCommandTool` now races subprocess wait against the turn's
  cancellation token. Ctrl+C during a long-running build aborts
  within microseconds (plus SIGKILL travel) instead of waiting for
  the 300-second timeout. Structural fix for the v0.6 "20-press
  Ctrl+C" report — tokens can't be forgotten; they're in the type.
- Retry middleware moves from `src/models/retry.rs` (deleted in
  follow-up) to `src/effect/middleware.rs`. Behaviour identical:
  3 attempts, 500ms→3s exponential backoff, retry on 5xx / 429 /
  `ConnectionFailed`.

### Docs

- `docs/architecture.md` — full tour of the new design + invariants.
- `docs/adding_tools.md` — one-file tool recipe.
- `docs/adding_providers.md` — adapter recipe.
- `docs/replay_debugging.md` — record/replay usage.

### Tests

- 558 tests pass: 516 library, 42 integration.
- `tests/reducer_flows.rs` — 15 multi-message flow tests (stale
  events, tool-outcome completeness, cancel, quit, slash commands).
- `tests/effect_cancel.rs` — 5 real-tokio tests (Ctrl+C aborts a
  `sleep 60` within 300ms; bounded shutdown).
- Ratatui `TestBackend` renderer tests (5).

### Not yet in v0.7.0

- Default binary path still runs v0.6 runtime. Flip happens in a
  follow-up release once v7 parity is verified against real
  sessions.
- Subagent dispatch, MCP startup, and several modals (conversation
  load, /cloud-setup, model list) still route through v0.6 code.
  Reducer has the `Msg` vocabulary; implementations mechanical.

## [0.6.0] - 2026-04-16

Major release: multi-provider adapter support. Mermaid is no longer
Ollama-only — direct integrations for Anthropic Claude, Google Gemini, and
the full OpenAI-compatible long tail (OpenAI, Groq, OpenRouter, Cerebras,
DeepInfra, Together). Plus a new slash-command palette, auto-loaded
MERMAID.md project instructions, MCP spec bump, and a security update.

### Added

- **Anthropic adapter** (`src/models/adapters/anthropic.rs`) — bespoke
  Messages API support: `2023-06-01` version pin, adaptive + legacy
  thinking formats dispatched per model, typed SSE streaming, `thinking`
  signature round-trip for multi-turn extended thinking, `cache_control:
  ephemeral` on system prompts + last tool for prompt caching, vision
  (base64 images), tool translation to Anthropic's flat `{type: "custom"}`
  shape. Supports Claude Opus 4.7 (`xhigh` effort tier), Sonnet 4.6,
  Opus 4.6, Sonnet 4.5, Opus 4.5, Haiku 4.5.
- **Gemini adapter** (`src/models/adapters/gemini.rs`) — per-method
  endpoints (`:generateContent` / `:streamGenerateContent?alt=sse`),
  `user`/`model` role convention, `functionResponse` merge for tool
  results, per-model thinking dispatch (Gemini 3 `thinkingLevel` enum,
  Gemini 2.5 Pro/Flash/Flash-Lite `thinkingBudget` with correct floors,
  2.0 omits `thinkingConfig`), `thought: true` reasoning parts, inline
  base64 images for vision. Curated list: `gemini-pro-latest`,
  `gemini-flash-latest`, `gemini-3.1-pro-preview`, `gemini-3-flash-preview`,
  `gemini-3.1-flash-lite-preview`, `gemini-2.5-pro/flash/flash-lite`.
- **OpenAI-compatible adapter** (`src/models/adapters/openai_compat.rs`)
  — single `/chat/completions` adapter with per-provider quirks encoded
  in `ProviderProfile`. Built-in registry: OpenAI, Groq, OpenRouter,
  Cerebras, DeepInfra, Together. Three reasoning strategies (`Effort`,
  `OpenRouterShape`, `None`) and three extraction strategies
  (`DeltaContentField`, `InlineThinkTags`, `None`). Streaming tool-call
  accumulator handles OpenAI's chunked `delta.tool_calls` pattern.
  OpenRouter `X-OpenRouter-Title` canonical header.
- **Custom OpenAI-compatible providers** — users can add any
  `/chat/completions` endpoint via `[providers.<name>]` in `config.toml`
  with `base_url`, `api_key_env`, and `compat = "openai" |
  "openai-effort" | "openrouter"`.
- **`ReasoningLevel` enum** (`src/models/reasoning.rs`) — seven tiers
  (`None`, `Minimal`, `Low`, `Medium`, `High`, `XHigh`, `Max`) with rank
  ordering; `XHigh` sits between `High` and `Max`. `nearest_effort()`
  snaps user choice onto the model's advertised `ReasoningCapability`.
  Per-model persistence via `[reasoning_per_model]` in config.
- **`--reasoning <level>` CLI flag** overrides config-default for this
  session.
- **Typed streaming** (`src/models/stream.rs`) — `StreamEvent` enum
  (`Text`, `Reasoning`, `ToolCall`, `Done`) replaces the legacy text-only
  callback. Adapters emit typed events; consumers route them without
  marker-sniffing.
- **`ModelCapabilities`** (`src/models/capabilities.rs`) — per-model
  `supports_tools`/`supports_vision`/`supports_reasoning`/`max_context_tokens`
  advertised by each adapter.
- **MERMAID.md project instructions** (`src/app/instructions.rs`) —
  walks UP from cwd to the git root or `$HOME`, loads the nearest
  `MERMAID.md`, auto-reloads on mtime change before every model call
  (one stat per turn). 10k-token cap with truncation marker. Injected
  via `ModelConfig::dynamic_system_suffix`; Anthropic gets a separate
  cache block (static base stays warm across project switches).
- **Slash-command palette** (`src/tui/widgets/slash_palette.rs`,
  `src/tui/slash_commands.rs`) — type `/` to open a filter-as-you-type
  list of all commands. Up/Down navigates, Tab completes, Enter
  dispatches, Esc dismisses. Centralized `COMMAND_REGISTRY` so `/help`
  auto-updates with new commands.
- **`/reasoning <level>` slash command** — per-model persisted reasoning
  depth. Alt+T cycles `None → Low → Medium → High → Max → None`
  (Minimal + XHigh reachable only via `/reasoning`, treated as
  specialist tiers).
- **MCP 2025-11-25 protocol** — bumped from 2025-03-26. New content
  block types: `audio`, `resource_link`, `resource` (embedded).
  Audio flows through the image attachment channel; resource links
  render as text so the model can follow up with another tool call.
- **`postgres-mcp` (uvx)** replacing deprecated
  `@modelcontextprotocol/server-postgres` — crystaldba community
  maintainer. Env var renamed `DATABASE_URL` → `DATABASE_URI`.
- **`@zencoderai/slack-mcp-server`** replacing deprecated
  `@modelcontextprotocol/server-slack` — Zencoder is the official
  handoff maintainer.
- **`@brave/brave-search-mcp-server`** replacing deprecated
  `@modelcontextprotocol/server-brave-search` — Brave is now the
  first-party maintainer.
- **Graceful MCP shutdown** (`src/mcp/transport.rs`) — close stdin →
  2s wait → SIGTERM → 1s wait → SIGKILL. Replaces the previous
  straight-to-SIGKILL path.
- **UTF-8-safe byte-buffer draining** (`src/utils/ndjson.rs`,
  `src/utils/sse.rs`) — NDJSON line splitter and SSE event splitter
  that buffer raw bytes and decode only complete frames. Protects
  against TCP-chunk-inside-codepoint corruption on both Ollama NDJSON
  and OpenAI-compat SSE streams.
- **API-key resolution** (`src/utils/auth.rs`) — uniform env-var lookup
  with optional `[providers.<name>].api_key_env` override.

### Changed

- **`/` slash-command prefix** replaces the legacy `:` colon prefix.
  All commands now live under `/`; the palette only opens for `/`.
- **Tokio bumped to 1.44** (resolves to 1.49 via caret) — closes
  RUSTSEC-2025-0023 (broadcast channel unsoundness).
- **toml 0.9 → 1.1** (major version bump), clap 4.5 → 4.6, bytes 1.8 →
  1.11, regex 1.11 → 1.12.
- **Removed `dotenvy`** — unused dead dependency.
- **Added `temp-env`** dev-dep — replaces `unsafe { env::set_var /
  remove_var }` in `backend.rs` + `auth.rs` tests; safer under
  `--test-threads > 1`.
- **MSRV pinned to 1.91** in `Cargo.toml rust-version`, `.clippy.toml`,
  and `flake.nix`. `str::floor_char_boundary` (used for UTF-8-safe
  slicing) stabilized in Rust 1.91.
- **Ollama `gpt-oss` dispatch** — sends `think: "low"|"medium"|"high"`
  (string enum) instead of the bool other Ollama models expect. Advertised
  as `Levels([None, Low, Medium, High])` in capabilities so XHigh/Max
  snap correctly via `nearest_effort`.
- **OpenRouter header** — `X-Title` → `X-OpenRouter-Title` (the new
  canonical name; old still accepted for backward compat).

### Fixed

- Anthropic `Max` effort now gated per-model — Sonnet 4.5 / Opus 4.5 /
  Haiku 4.5 snap to `"high"` since they don't accept `"max"` per the
  2026-04 effort documentation.
- Gemini 3 model IDs refreshed — `gemini-3-pro` (shut down 2026-03),
  `gemini-3-flash`, `gemini-3-flash-lite` replaced with the current
  `-preview` variants and `-latest` aliases.
- MCP registry entries for `slack`, `postgres`, and `brave-search` swapped
  to current maintained alternatives (originals deprecated upstream).

### Removed

- `src/tui/stream_handler.rs` — replaced by the typed `StreamEvent` path
  and `TuiObserver` in `loop_coordinator.rs`.
- `.env.example` — `dotenvy` dependency removed, no `.env` loading path.

## [0.5.1] - 2026-04-12

### Added
- MCP (Model Context Protocol) client integration for connecting to external tool servers
- `mermaid add <name>` / `mermaid remove <name>` / `mermaid mcp` commands for MCP server management
- Built-in registry of 17 popular MCP servers (context7, github, playwright, memory, postgres, etc.)
- Enhanced computer use: window-aware screenshots (`mode: "window"`), `list_windows` tool, auto-screenshot after click/type/key actions
- 42 new tests across agent loop, session persistence, non-interactive mode, and stream handler

### Changed
- MCP servers now initialize in background (TUI renders immediately instead of blocking startup)
- MCP tools become available to the model as soon as servers are ready, even mid-agent-loop
- Centralized model configuration into `ModelConfig::from_app_config()` (internal refactor)
- Nix flake: switched from nightly Rust to stable 1.87.0, removed OpenSSL 1.1 dependency

### Fixed
- Ollama URL normalization with paths (`http://host/v1` no longer appends port after the path)
- Token tracking now counts total tokens (prompt + completion) instead of completion-only
- `ActionResult` images field properly propagated for screenshot tool results
- Command timeout treated as success (process continues running in background)

### Removed
- Sync `list_models()` from Ollama detector (replaced by async-only `list_models_async()`)
- Unused fields from MCP `ResolvedServer` struct

## [0.5.0] - 2026-03-15

### Added
- Ollama Cloud setup flow (`:cloud-setup` command and interactive API key configuration)
- Claude Code-style subagents for parallel task execution (`agent` tool)
- TUI stream event system (typed `StreamEvent` enum replacing string-based protocol)
- Image paste status widgets and attachment management UI
- Web search and web fetch via Ollama Cloud API (`web_search`, `web_fetch` tools)

### Changed
- Consolidated ModelFactory into `backend.rs`, removed `factory.rs`
- Architectural cleanup across TUI state management (split App into focused state modules)
- Consolidated git tools into bash `execute_command`, removed `git2` dependency
- Removed vestigial LiteLLM infrastructure

## [0.4.1] - 2026-02-10

### Fixed
- Full codebase review: 53 clippy warnings fixed
- Security and correctness improvements across all modules

## [0.4.0] - 2026-02-08

### Changed
- Codebase review, dead code removal, and architectural cleanup

### Fixed
- Gate crates.io publish behind `PUBLISH_TO_CRATES_IO` environment variable

## [0.3.0] - 2026-01-20

### Added
- Proper agent loop for native Ollama tool calling (replaces text-based action blocks)
- Thinking mode toggle with Alt+T (for models that support extended reasoning)
- Message queuing — type while model generates, messages send in order
- Session persistence — start fresh by default, `--continue` to resume last conversation
- `--sessions` flag to pick a previous conversation to resume
- Model persistence — last-used model saved to config
- Image paste support with vision model integration (Ctrl+V)
- `edit_file` tool for targeted text replacement with diff display
- Web search via Ollama Cloud API (replaced SearXNG)
- `web_fetch` tool for fetching URL content as markdown
- Bracketed paste support for multi-line input
- Markdown table rendering in chat
- Auto-pull models from Ollama when not found locally
- Non-interactive mode: `mermaid run "prompt"` with JSON/text/markdown output
- `delete_file` and `create_directory` tools
- Computer use tools: `screenshot`, `click`, `type_text`, `press_key`, `scroll`, `mouse_move`
- File-based logging (writes to `~/.mermaid/mermaid.log` instead of corrupting TUI)
- Esc/Ctrl+C to interrupt queued message processing

### Changed
- Simplified to Ollama-only backend (removed vLLM, proxy, model router)
- Overhauled system prompt for tool-calling models
- Upgraded to Rust 2024 edition
- Upgraded ratatui from 0.29 to 0.30 with new idioms
- Rewrote README for accuracy and simplicity
- Unified tool calling to native Ollama API format
- Used Ollama's real token counts (removed tiktoken dependency)

### Fixed
- Char-boundary safe string slicing throughout (prevents UTF-8 panics)
- False positive in dangerous command detection (`.mermaid` matching `rm`)
- Cursor position drifts on wrapped input lines
- Cursor jumps to column 1 when typing space
- Timestamp overlapping long user messages
- Streaming timeout for large operations (removed global HTTP timeout)
- Windows duplicate input from key release events

## [0.2.1] - 2025-12-18

### Added
- NixOS support via `flake.nix` and `flake.lock`
- `delete_file` and `create_directory` tools for agent
- `.vs/` to `.gitignore`

### Changed
- Organized project root directory structure:
  - Moved scripts to `scripts/`
  - Moved infrastructure files to `infra/`
- Updated README to accurately reflect available tools (renamed `run_command` to `execute_command`)

### Fixed
- CLI argument conflict: `prompt` now uses `-P` short flag, `path` retains `-p`
- `resume` argument `conflicts_with` validation logic
- Compilation errors due to relative paths in `include_str!` macros

## [0.2.0] - 2025-11-16

### Added
- Native Ollama tool calling support with JSON Schema tool definitions
  - 9 tools: read_file, write_file, run_command, git_status, git_diff, git_commit, web_search, list_directory, get_file_info
  - Structured tool definitions with detailed parameter descriptions
  - Tool calls parsed from streaming chunks in real-time
- Enhanced input widget UI matching Claude Code aesthetics
  - Always-visible "> " prompt prefix
  - Full-width input bar with top/bottom borders only
  - Proper text wrapping with 2-space indentation on continuation lines
  - Blank line after "Thinking..." marker for better visual spacing
- Model compatibility framework for tool calling detection

### Changed
- Migrated from text-based action blocks to Ollama native tool calling API
- Completely rewrote system prompt (76% reduction: 353 to 86 lines)
- Tool definitions now provide comprehensive usage guidance
- Cleaner, more maintainable architecture with dedicated tools module
- Updated all backend adapters to support tool_calls in responses
- Stream handler now accumulates tool calls from streaming chunks

### Removed
- Legacy text-based parsers (parser.rs, extractor.rs, segmenter.rs)
- Verbose system prompt with action block examples
- Text-based action block parsing (temporarily, will be restored as fallback)

### Fixed
- Cursor positioning now accounts for "> " prefix and border changes
- Text wrapping alignment issues with continuation lines
- Input widget rendering for empty input states

### Breaking Changes
- Models without native Ollama tool calling support will not execute actions
- Next release (v0.2.1) will restore text-based fallback for universal compatibility
- Compatible models: llama3.1, llama3.2, qwen2.5-coder, mistral-nemo, firefunction-v2

## [0.1.1] - 2025-09-27

### Added
- Test helper functions for better test coverage
  - `path_exists` function in filesystem module for path validation
  - `current_branch` function in git module for branch detection

### Fixed
- Test compilation errors in filesystem and git modules
- Clippy configuration to allow reasonable nesting depth

### Changed
- Adjusted CI/CD workflow clippy strictness to warnings level

## [0.1.0] - 2025-09-27

### Added
- Initial release of Mermaid CLI
- Model-agnostic AI pair programmer with support for 100+ LLM providers via LiteLLM proxy
- Terminal User Interface (TUI) built with Ratatui
  - Real-time streaming responses
  - Syntax highlighting for code
  - Project sidebar with file tree
  - Markdown rendering support
- Agentic capabilities
  - File operations (read, write, create, delete)
  - Git integration (diff, status, commit)
  - Shell command execution
  - Project context awareness
- Configuration system
  - Global config at ~/.config/mermaid/config.toml
  - Project-specific config support
  - Environment variable configuration
- LiteLLM proxy integration
  - Support for OpenAI, Anthropic, Google, Ollama, and 90+ more providers
  - Unified API interface
  - Docker/Podman containerization
- Project context loading
  - Automatic project structure analysis
  - Token counting and management
  - Respects .gitignore patterns
- GitHub Actions CI/CD workflows
  - Automated testing and linting
  - Multi-platform release builds (Linux, macOS, Windows)
  - Security vulnerability scanning
  - Code formatting enforcement
- Dual licensing (MIT OR Apache-2.0)

### Infrastructure
- Rust 2021 edition
- Comprehensive test suite
- rustfmt and clippy configuration
- Docker compose setup for LiteLLM proxy

[Unreleased]: https://github.com/noahsabaj/mermaid-cli/compare/v0.16.0...HEAD
[0.16.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.15.1...v0.16.0
[0.15.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.15.0...v0.15.1
[0.15.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.14.2...v0.15.0
[0.14.2]: https://github.com/noahsabaj/mermaid-cli/compare/v0.14.1...v0.14.2
[0.14.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.14.0...v0.14.1
[0.14.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.12.2...v0.13.0
[0.12.2]: https://github.com/noahsabaj/mermaid-cli/compare/v0.12.1...v0.12.2
[0.12.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.11.1...v0.12.0
[0.11.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.10.2...v0.11.0
[0.10.2]: https://github.com/noahsabaj/mermaid-cli/compare/v0.10.1...v0.10.2
[0.10.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/noahsabaj/mermaid-cli/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/noahsabaj/mermaid-cli/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/noahsabaj/mermaid-cli/releases/tag/v0.1.0
