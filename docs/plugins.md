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

At startup mermaid discovers skills from three places — project (`<git-root>/.mermaid/skills/<name>/SKILL.md`, shared with your team), user (`~/.config/mermaid/skills/<name>/SKILL.md`, all your projects), and enabled plugins (declared in the plugin manifest's `skills` list) — and injects a compact index (name, description, path) into the system prompt. Skills in Claude Code's and Codex's directories load too: `.claude/skills/` and `.agents/skills/` in the project, `~/.claude/skills/` and `~/.agents/skills/` for the user (see [Files from other tools](#files-from-other-tools)). Same-named skills dedupe with project > user > plugin precedence, and Mermaid's own directory wins over `.claude/`, which wins over `.agents/`. When a task matches a description, the model reads the full `SKILL.md` with `read_file`, so activation is visible in the transcript and idle skills cost almost nothing per request. The index caps at 64 skills / 8 KiB; edits to skill files are picked up on the next session start. `mermaid doctor` reports the discovered count.

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

## Files from other tools

Mermaid reads the files Claude Code and the `.agents/` convention use, so you keep your
instructions, skills, commands and agents when you move to Mermaid or use both. No plugin is
needed.

- **Instructions.** `CLAUDE.md` (or `.claude/CLAUDE.md`) loads in place of `AGENTS.md`, only when
  the directory has no `AGENTS.md`. A project with both keeps them in step itself, so Mermaid
  does not send the same rules twice. `MERMAID.md` still loads last and wins on conflict.
- **Directories.** Mermaid looks in these, highest precedence first: in the project (the git
  root), `.mermaid/`, `.claude/`, `.agents/`; for the user, `~/.config/mermaid/`, `~/.claude/`,
  `~/.agents/`. Each may hold `skills/`, `commands/` and `agents/`. A project entry wins over a
  user entry, and within a scope a higher directory wins on the same name, so `.mermaid/` wins
  over `.claude/`.
- **Prompt commands: `commands/**/*.md`.** The file stem is the command name (`fix-issue.md` is
  `/fix-issue`; subdirectories only group files), unless the frontmatter sets `name:`. The
  `description:` shows in the `/` palette and `/help`, tagged `(project)` or `(user)`. The body is
  the prompt: `$ARGUMENTS` is replaced with everything typed after the command, `$1`, `$2`, ...
  with each word, and with neither the arguments are appended. This is also how you add your own
  commands: put a markdown file in `.mermaid/commands/` or `~/.config/mermaid/commands/`.
  Claude Code's `!` shell lines and `@` file references are sent to the model as written; Mermaid
  does not run them. A file named like a built-in command is skipped with a warning. A command
  file wins over a same-named plugin command.
- **Agent types: `agents/*.md`.** Claude Code's subagent format: `name:` (default the file stem),
  `description:` (shown to the model in the `agent` tool's description), `tools:`, and the body
  as the system prompt. Claude Code tool names map to Mermaid's: `Read` to `read_file`; `Grep`,
  `Glob` and `LS` to `read_file` plus `execute_command` for searching; `Edit` and `MultiEdit` to
  `edit_file`, `apply_patch` and `write_file`; `Write` to `write_file` and `create_directory`;
  `Bash` to `execute_command`; `WebFetch`, `WebSearch` and `mcp__*` to `web_fetch`, `web_search`
  and `mcp`. Mermaid's own tool names work too. An agent whose tools can't write (no `Edit`,
  `Write`, `Bash` or other write tool) gets a `read_only` safety ceiling, so its search shell
  can't change files; set `safety:` to choose another ceiling. `model:` is used only in Mermaid's
  `provider/model` form; Claude Code aliases such as `sonnet` or `inherit` use the session model.
  `isolation:` works as in config. A same-named `[agents.types.<name>]` in your config wins, and
  a project file cannot redefine the built-in `general` or `explore` types (a cloned repository
  must not loosen them); your user-level files can.

Like skills, these load at session start; restart to pick up edits.
