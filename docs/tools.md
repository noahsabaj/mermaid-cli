# Tool details

The tool table lives in the [README](../README.md#tools). This covers the behavior behind it.

## MCP tools

MCP servers contribute additional tools under the `mcp__<server>__<tool>` prefix when configured. Names and schemas are sanitized to provider-safe form at startup (charset `[A-Za-z0-9_-]`, 64-char cap, `$ref` inlining and other schema normalization); `enabled_tools`/`disabled_tools` filters keep matching the RAW tool names the server itself advertises.

Servers start concurrently at launch, each bounded by a 60-second timeout, and report ready/errored individually.

By default MCP tools are **deferred**: instead of advertising every server's tools on every request, the model gets one `tool_search` tool that searches deferred tool names/descriptions and promotes matches to direct advertisement for the rest of the session — deferred schemas don't count against `/context` until promoted. Opt out globally with `mcp_defer_tools = false` at the top level of config, or per server with `defer = false` on its `[mcp_servers.<name>]` entry.

## Web tools

`web_fetch` is registered natively with no key. `web_search` is registered when the selected backend is viable; the managed default is omitted with an actionable diagnostic on unsupported platforms.

Inspect an existing `web_fetch` snapshot with Unicode-caseless `pattern` matching, or page through it with stable `start_line`/`line_count` continuation, without refetching.

Backend selection, redirect and provenance rules, and the transfer budgets are in
[configuration.md](configuration.md#web-tool-backends).

## Computer use

Computer-use tools are advertised only in interactive TUI sessions when a usable GUI backend is detected.

| Platform | Screenshot | Click / type / scroll | Clipboard image paste |
| --- | --- | --- | --- |
| Linux / X11 | `scrot` | `xdotool` | `xclip` |
| Linux / Wayland | `grim` | `ydotool`, `wtype` | `wl-clipboard` |
| macOS | `screencapture` | not yet ported | `pngpaste` / `osascript` |
| Windows | not wired yet | not wired yet | PowerShell |

```bash
sudo apt install scrot xdotool xclip                 # X11
sudo apt install grim ydotool wtype wl-clipboard     # Wayland
sudo apt install imagemagick                         # optional: screenshot downscaling
```

See `src/providers/tool/computer_use/` for the implementation matrix.

## Inline approvals

In `ask` mode — and on an `auto` escalation — a gated action pauses and prompts inline
(`1` Yes · `2` Yes, don't ask again · `3`/Esc No). The agent waits for your answer instead of
erroring out.
