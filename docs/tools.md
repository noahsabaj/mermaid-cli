# Tool details

The tool table lives in the [README](../README.md#tools). This covers the behavior behind it.

## Core tools

Always registered: `read_file`, `write_file`, `edit_file`, `apply_patch`, `delete_file`,
`create_directory`, `execute_command`, `background_process`, `memory`, `agent`, the checklist trio (`task_create`,
`task_update`, `task_list`), `ask_user_question`, and the context pair (`context_archive`,
`compact_context`, below).
`web_search` and `web_fetch` register when their backend is viable (below); MCP tools when a
server is configured.

Editing: `edit_file` is for one location -- `target_content` must match once (or set
`allow_multiple`), with matching that degrades in steps from exact through trailing-whitespace,
full-trim and Unicode normalisation before refusing. `apply_patch` is for multi-hunk and
new-file work and takes a unified diff, range headers included. Both write atomically beneath
the resolved root, snapshot a checkpoint for `/restore`, and replay through the approval queue.

On Anthropic, the model gets Anthropic's own text editor and bash tools, the definitions Claude
is trained on, in place of Mermaid's schemas for the same work: the text editor stands in for
`read_file`, `write_file` and `edit_file`, and `bash` sits beside `execute_command`, which keeps
its timeout and background mode. Each native call is rewritten onto the Mermaid tool it stands
for before anything runs (`view` is a line-numbered `read_file`, `str_replace` and `insert` are
`edit_file`, `bash` is `execute_command` with the longest foreground timeout), so the policy
gate, the read-only sandbox, checkpoints and approvals see the same tool they always do, and
history sends the call back to the model as it wrote it. `bash` is not offered on Windows,
where commands run under PowerShell. A model that refuses these tools gets Mermaid's schemas
from then on; `[tools] provider_native = false` always sends Mermaid's. OpenAI's `apply_patch`
and `shell` tools exist only in the Responses API, which Mermaid does not use for OpenAI;
Mermaid's own `apply_patch` already takes the same patch format.

Pictures: `read_file` returns a PNG, JPEG, GIF or WebP file (known by its first bytes) as an
image beside a one-line `[image/png, 48213 bytes]` result, up to 3.75 MiB. Any tool's images
(an MCP tool's screenshot, too) travel with its result: inside the `tool_result` on Anthropic,
and as a user turn right after the run of results on the providers that take images only from
the user. Only the newest three images in a conversation are sent.

Background processes: `execute_command` with `mode="background"` (or a foreground command the
user moves to the background with Ctrl+B) returns an id such as `bg-1234`. `background_process`
takes that id. `read` returns the output written since the last read, at most the newest 32 KiB,
and whether the process still runs. `wait` blocks until the process exits, a `pattern` appears in
new output, or `timeout_secs` passes (default 300, at most 3600; Esc ends it early), then returns
the same as `read`. `stop` ends the process tree. `list` shows the session's processes. The tool
reaches only processes this Mermaid process started for the calling session, never a raw pid, so
it needs no approval in any safety mode. Exit codes are not recorded. The user's `/logs` and
`/stop` work on the same processes.

Computer: with `computer = true` under `[tools]`, the `computer` tool takes screenshots and drives
the mouse and keyboard of the user's real screen. It is off by default because every screenshot
goes to the model's provider. Its actions are those of Anthropic's computer toolset: `screenshot`,
`zoom`, `cursor_position`, `left_click`, `right_click`, `middle_click`, `double_click`,
`triple_click`, `left_click_drag`, `mouse_move`, `left_mouse_down`, `left_mouse_up`, `scroll`,
`type`, `key`, `hold_key` and `wait`. On Anthropic, Claude gets the toolset itself
(`computer_toolset_20260801`), rewritten onto the tool as it arrives, with `provider_native`;
other vision models call the tool directly. A screenshot is fitted to 1568 px on its long edge and
1.15 megapixels, and coordinates are pixels of the last screenshot, scaled back to the primary
screen. Input actions wait 0.5 s before they return. Calls in one message run in order, and after
one fails the rest return "Not executed: an earlier computer action in this turn failed." without
running. Screenshots, `zoom`, `cursor_position` and `wait` run in every safety mode; the other
actions are gated as external access, so `read_only` blocks them. The gate decides once for all
the input actions of one model message: `ask` shows the whole batch in one prompt, and `auto`
gives the safety check the batch and the last screenshot, so its model must accept pictures. A
headless run in `ask` refuses input unless `--allow-untrusted-tools` is set. If the user moves the
mouse while Mermaid works, the next input action returns "Not executed: the user moved the mouse"
and input stays stopped until the user sends a message.
Windows and macOS use xcap and enigo (macOS asks the terminal for Screen Recording and
Accessibility). Linux needs an X11 session; Wayland is not supported yet.

Paths outside the project (absolute, or traversing out of it) resolve to where they point and
are gated as external access: `read_only` denies, `ask` prompts with a per-directory
"don't ask again", `auto` classifies, `full_access` allows. See the README's [Safety](../README.md#safety) section.

## Context tools

Compaction summarizes older history out of the working context, but nothing is deleted: every
message stays in the session's `.mermaid/conversations/<id>.jsonl` log. `context_archive`
searches that log (case-insensitive `query`), reads any message back whole (`message`, paged
with `char_offset` past 32,000 characters), or lists it. Messages are numbered in the order they
happened and marked with the compaction that removed them, so the checkpoint summary is never
the only copy of anything.

`compact_context` lets the model checkpoint when it judges its context noisy, with an optional
`focus` for the handoff; it runs before the model's next call. The automatic trigger
(`[compaction] auto_threshold_percent`, 85% by default) stays as the safety net. On a provider
that compacts server-side (Anthropic), the provider handles that automatic trigger instead
(`[compaction] provider_native`); `compact_context` and `/compact` still run Mermaid's own.

## MCP tools

MCP servers contribute additional tools under the `mcp__<server>__<tool>` prefix when configured. Names and schemas are sanitized to provider-safe form at startup (charset `[A-Za-z0-9_-]`, 64-char cap, `$ref` inlining and other schema normalization); `enabled_tools`/`disabled_tools` filters keep matching the RAW tool names the server itself advertises.

Servers start concurrently at launch and report ready/errored individually.

Mermaid speaks MCP 2026-07-28 and still works with servers that only speak 2025-11-25. Each server
gets a `server/discover` request first. A server that answers it speaks 2026-07-28: there is no
`initialize` handshake or session, and every request carries the protocol version, client info and
capabilities in `_meta`. Any other answer (an error, an HTTP `4xx`, or silence from a stdio
server for 20 seconds) means 2025-11-25, and Mermaid runs the `initialize` handshake instead. On a
2026-07-28 HTTP server, calls also carry the `Mcp-Method`, `Mcp-Name` and `Mcp-Param-*` headers;
a tool whose `x-mcp-header` annotations are invalid is left out with a warning. Mermaid declares
no client capabilities (no sampling, elicitation or roots), so a tool call that asks for that
kind of input fails with an error.

By default MCP tools are **deferred**: instead of advertising every server's tools on every request, the model gets one `tool_search` tool that searches deferred tool names/descriptions and promotes matches to direct advertisement for the rest of the session — deferred schemas don't count against `/context` until promoted. Opt out globally with `mcp_defer_tools = false` at the top level of config, or per server with `defer = false` on its `[mcp_servers.<name>]` entry.

### Remote servers and sign-in

A remote server is an `[mcp_servers.<name>]` entry with a `url` (Streamable HTTP). Add one with
`mermaid add <name> --url <URL>`. If the server needs an OAuth sign-in, Mermaid opens the browser
there and then; `mermaid mcp login <name>` signs in again later, and `mermaid mcp logout <name>`
deletes the stored tokens. The flow follows the MCP authorization spec (2026-07-28):

- Discovery from the server's `401` challenge or its `/.well-known/oauth-protected-resource`
  metadata, then the authorization server's RFC 8414 or OpenID Connect metadata. The metadata
  must name the issuer it was fetched for, and the server must support PKCE `S256`.
- The client, in the spec's order: a client you registered yourself (`oauth.client_id`), then
  Mermaid's Client ID Metadata Document
  (`https://noahsabaj.github.io/mermaid-cli/oauth/client-metadata.json`), then Dynamic Client
  Registration as a native app.
- The browser redirects to `http://127.0.0.1:<port>/callback`. On a remote shell, sign in in any
  browser and paste the address it ends on into the terminal. Mermaid checks `state` and the
  `iss` of the response before it redeems the code.
- Tokens are requested for the server's `resource` (RFC 8707) and stored in the OS keyring
  (service `mermaid`, account `mcp-oauth:<name>`), bound to the server's `url`. They refresh
  when they expire or when the server answers `401`. When a refresh fails, or the server answers
  `403 insufficient_scope`, the server does not start and says to run `mermaid mcp login <name>`;
  that sign-in asks for the new scopes as well as the old ones.

A config that sends its own `Authorization` header (`headers` or `env_headers`) turns this off.
For a server that does not let Mermaid register itself, register an OAuth app there and name it
in config:

```toml
[mcp_servers.github]
url = "https://api.githubcopilot.com/mcp/"

[mcp_servers.github.oauth]
client_id = "Iv1.0123456789abcdef"
client_secret_env = "GITHUB_MCP_CLIENT_SECRET"  # only for a confidential app
callback_port = 8765                            # the app's redirect: http://127.0.0.1:8765/callback
# scopes = ["repo"]                             # replaces the scopes the server suggests
```

`mermaid add <name> --url <URL> --client-id <ID> --client-secret-env <VAR> --callback-port <PORT>`
writes the same table.

## Web tools

`web_fetch` is registered natively with no key. `web_search` is registered when the selected backend is viable; the managed default is omitted with an actionable diagnostic on unsupported platforms.

HTML is reduced to its main content by a readability pass. When that guesses wrong (a table,
a nav-hosted index, attribute data), `raw: true` returns the page source as served instead;
it needs the native fetch backend.

Inspect an existing `web_fetch` snapshot with Unicode-caseless `pattern` matching, or page through it with stable `start_line`/`line_count` continuation, without refetching.

Backend selection and redirect rules are in
[configuration.md](configuration.md#web-tool-backends).

## Inline approvals

In `ask` mode — and on an `auto` escalation — a gated action pauses and prompts inline
(`1` Yes · `2` Yes, don't ask again · `3`/Esc No). The agent waits for your answer instead of
erroring out.
