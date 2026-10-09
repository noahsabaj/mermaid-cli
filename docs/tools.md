# Tool details

The tool table lives in the [README](../README.md#tools). This covers the behavior behind it.

## Core tools

Always registered: `read_file`, `write_file`, `edit_file`, `apply_patch`, `delete_file`,
`create_directory`, `execute_command`, `memory`, `agent`, the checklist trio (`task_create`,
`task_update`, `task_list`), `ask_user_question`, and the context pair (`context_archive`,
`compact_context`, below).
`web_search` and `web_fetch` register when their backend is viable (below); MCP tools when a
server is configured, and `list_mcp_resources`/`read_mcp_resource` while a server serving
resources is ready.

Editing: `edit_file` is for one location -- `target_content` must match once (or set
`allow_multiple`), with matching that degrades in steps from exact through trailing-whitespace,
full-trim and Unicode normalisation before refusing. `apply_patch` is for multi-hunk and
new-file work and takes a unified diff, range headers included. Both write atomically beneath
the resolved root, snapshot a checkpoint for `/undo`, and replay through the approval queue.

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

Servers start concurrently at launch, each bounded by a 60-second timeout, and report ready/errored individually.

By default MCP tools are **deferred**: instead of advertising every server's tools on every request, the model gets one `tool_search` tool that searches deferred tool names/descriptions and promotes matches to direct advertisement for the rest of the session — deferred schemas don't count against `/context` until promoted. Opt out globally with `mcp_defer_tools = false` at the top level of config, or per server with `defer = false` on its `[mcp_servers.<name>]` entry.

### Resources

A server that declares the `resources` capability exposes read-only data (files, records, documents) beside its tools. While at least one such server is ready the model gets two built-in tools, never deferred behind `tool_search`:

- `list_mcp_resources` returns each resource's `server`, `uri`, `name`, `description` and `mimeType` as JSON, for every resources-capable server or only the one named by `server`. A server whose listing fails reports its error in place of its resources.
- `read_mcp_resource` takes `server` and `uri` and returns the text content. An image is attached the way MCP tool images are; other binary content is summarized by MIME type and size rather than sent as base64.

Output is capped at 100,000 characters, keeping the head and the tail. Both tools pass the same policy gate as MCP tool calls: `read_only` blocks them, `ask` prompts each time (MCP access is never allowlisted), and `auto` classifies. Reading is read-only by protocol, so the external-writes floor that applies to write-shaped MCP tools does not.

### Prompts

A server that declares `prompts` contributes each of its prompts to the `/` palette as `/mcp__<server>__<prompt>` (sanitized like tool names, lowercased), tagged `(mcp:<server>)` like a plugin's prompt commands. Typed words map onto the prompt's declared arguments in order; double quotes group words, and the last argument takes whatever is left. A missing required argument prints a usage line. Otherwise Mermaid fetches the prompt (`prompts/get`) and sends its text as your message, so the transcript shows the text itself; non-text parts are left out with a note. A server's prompts leave the palette when it errors or stops. `/doctor` lists each ready server's tool and prompt counts and whether it serves resources.

## Web tools

`web_fetch` is registered natively with no key. `web_search` is registered when the selected backend is viable; the managed default is omitted with an actionable diagnostic on unsupported platforms.

HTML is reduced to its main content by a readability pass. When that guesses wrong (a table,
a nav-hosted index, attribute data), `raw: true` returns the page source as served instead;
it needs the native fetch backend.

Inspect an existing `web_fetch` snapshot with Unicode-caseless `pattern` matching, or page through it with stable `start_line`/`line_count` continuation, without refetching.

Backend selection, redirect and provenance rules, and the transfer budgets are in
[configuration.md](configuration.md#web-tool-backends).

## Inline approvals

In `ask` mode — and on an `auto` escalation — a gated action pauses and prompts inline
(`1` Yes · `2` Yes, don't ask again · `3`/Esc No). The agent waits for your answer instead of
erroring out.
