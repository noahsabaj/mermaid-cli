# Skills, hooks, and plugin bundles

## Skills

Skills are task-specific playbooks the agent loads on demand (progressive disclosure). Each skill is a directory holding a `SKILL.md` with Claude Code-compatible frontmatter:

```markdown
---
name: deploy
description: Cut a release — version bump, changelog, tag, publish
---

Step-by-step instructions the model follows when this skill applies...
```

At startup mermaid discovers skills from three places — project (`<git-root>/.mermaid/skills/<name>/SKILL.md`, shared with your team), user (`~/.config/mermaid/skills/<name>/SKILL.md`, all your projects), and enabled plugins (declared in the plugin manifest's `skills` list) — and injects a compact index (name, description, path) into the system prompt. Same-named skills dedupe with project > user > plugin precedence. When a task matches a description, the model reads the full `SKILL.md` with `read_file`, so activation is visible in the transcript and idle skills cost almost nothing per request. The index caps at 64 skills / 8 KiB; edits to skill files are picked up on the next session start. `mermaid doctor` reports the discovered count.

File size is capped at ~10k tokens; oversized content is truncated with a marker so the model knows context was elided.

## Plugin hooks

Enabled plugins' hooks receive lifecycle events as JSON on stdin (`MERMAID_HOOK_EVENT` names the
event). Most events are observe-only, but on **`before_tool_use`** a hook can gate the call by
printing one JSON object on stdout (Claude Code-compatible), or deny by exiting with code 2
(stderr becomes the reason):

```sh
#!/bin/sh
# deny-etc-writes: block any tool call whose arguments mention /etc
payload=$(cat)
case "$payload" in
  *'/etc'*) cat <<'EOF'
{"hookSpecificOutput": {"hookEventName": "PreToolUse",
  "permissionDecision": "deny",
  "permissionDecisionReason": "writes under /etc are not allowed here"}}
EOF
  ;;
esac
```

The response may also carry `updatedInput` (a full replacement tool-arguments object — still
vetted by the safety policy exactly like the original) and `additionalContext` (a string surfaced
to the model on its next request). The legacy `{"decision": "block", "reason": "..."}` shape is
accepted too. Across plugins: the first deny wins, the last `updatedInput` wins, and context
strings concatenate. Failure semantics are asymmetric by design: an explicit deny always denies,
while infrastructure failures (unparseable output, a timeout, a crash) log a warning and allow —
a buggy hook must not lock you out of every tool call.

## Plugin bundles: MCP servers, commands, agent types

Beyond skills and hooks, an enabled plugin can contribute three more asset kinds, each a list of
plugin-relative paths in `plugin.toml`:

- **`mcp = ["servers.toml"]`** — each file's `[servers.<name>]` tables are MCP server configs
  (same shape as `[mcp_servers.<name>]` in your config). They start with your own servers at
  session startup and flow through tool deferral like any other server. A `./`-relative
  `command` resolves inside the plugin directory (containment enforced); anything else is
  PATH-looked-up. A same-named server in your config wins with a warning. Enabling a plugin that
  declares MCP servers grants command execution — the same trust boundary as hooks.
- **`prompts = ["deploy.md"]`** — markdown prompt commands with the skills frontmatter dialect
  (`name:`/`description:`; the name falls back to the file stem, validated `[a-z0-9-]+`). They
  appear in the `/` palette tagged `(plugin:<name>)` and in `/help`; running `/deploy prod`
  substitutes `prod` for `$ARGUMENTS` (or appends the args when the token is absent) and submits
  the expansion as a normal prompt — the transcript shows the expanded text, so recordings
  replay without the plugin. Built-in commands always win over a same-named prompt.
- **`agents = ["types.toml"]`** — each file's `[types.<name>]` tables are agent types (same
  shape as `[agents.types.<name>]`): the model can spawn them via the `agent` tool. Your config's
  same-named type wins with a warning.

Like skills, bundle changes are picked up on the next session start.
