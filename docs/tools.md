# Tool details

The tool table lives in the [README](../README.md#tools). This covers the behavior behind it.

## Core tools

Always registered: `read_file`, `write_file`, `edit_file`, `apply_patch`, `delete_file`,
`create_directory`, `execute_command`, `memory`, `agent`, the checklist trio (`task_create`,
`task_update`, `task_list`), `ask_user_question`, and `enter_plan_mode`/`exit_plan_mode`.
`web_search` and `web_fetch` register when their backend is viable (below); MCP tools when a
server is configured.

Editing: `edit_file` is for one location -- `target_content` must match once (or set
`allow_multiple`), with matching that degrades in steps from exact through trailing-whitespace,
full-trim and Unicode normalisation before refusing. `apply_patch` is for multi-hunk and
new-file work and takes a unified diff, range headers included. Both write atomically beneath
the resolved root, snapshot a checkpoint for `/undo`, and replay through the approval queue.

Paths outside the project (absolute, or traversing out of it) resolve to where they point and
are gated as external access: `read_only` and `plan` deny, `ask` prompts with a per-directory
"don't ask again", `auto` classifies, `full_access` allows. See the README's [Safety](../README.md#safety) section.

## MCP tools

MCP servers contribute additional tools under the `mcp__<server>__<tool>` prefix when configured. Names and schemas are sanitized to provider-safe form at startup (charset `[A-Za-z0-9_-]`, 64-char cap, `$ref` inlining and other schema normalization); `enabled_tools`/`disabled_tools` filters keep matching the RAW tool names the server itself advertises.

Servers start concurrently at launch, each bounded by a 60-second timeout, and report ready/errored individually.

By default MCP tools are **deferred**: instead of advertising every server's tools on every request, the model gets one `tool_search` tool that searches deferred tool names/descriptions and promotes matches to direct advertisement for the rest of the session — deferred schemas don't count against `/context` until promoted. Opt out globally with `mcp_defer_tools = false` at the top level of config, or per server with `defer = false` on its `[mcp_servers.<name>]` entry.

## Web tools

`web_fetch` is registered natively with no key. `web_search` is registered when the selected backend is viable; the managed default is omitted with an actionable diagnostic on unsupported platforms.

Inspect an existing `web_fetch` snapshot with Unicode-caseless `pattern` matching, or page through it with stable `start_line`/`line_count` continuation, without refetching.

Backend selection, redirect and provenance rules, and the transfer budgets are in
[configuration.md](configuration.md#web-tool-backends).

## Inline approvals

In `ask` mode — and on an `auto` escalation — a gated action pauses and prompts inline
(`1` Yes · `2` Yes, don't ask again · `3`/Esc No). The agent waits for your answer instead of
erroring out.
