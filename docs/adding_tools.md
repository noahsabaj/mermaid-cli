# Adding a tool

One file, one `impl`, one registry entry. The architecture handles the rest — cancellation, progress, identity, and workdir all ride inside `ExecContext` rather than being plumbed through parameters.

## 1. Pick a name

The name is what the model sees when it decides to call your tool. Keep it `snake_case`, short, and action-first: `read_file`, `web_search`, `execute_command`. MCP tools get the `mcp__<server>__<name>` prefix automatically; don't replicate that here.

## 2. Implement `ToolExecutor`

```rust
// src/providers/tool/my_tool.rs
use async_trait::async_trait;

use crate::domain::ToolOutcome;
use crate::providers::ctx::ExecContext;
use crate::providers::tool::ToolExecutor;

pub struct MyTool;

#[async_trait]
impl ToolExecutor for MyTool {
    fn name(&self) -> &'static str {
        "my_tool"
    }

    async fn execute(&self, args: serde_json::Value, ctx: ExecContext) -> ToolOutcome {
        let Some(input) = args.get("input").and_then(|v| v.as_str()) else {
            return ToolOutcome::Error {
                error: "my_tool requires 'input' (string)".to_string(),
                duration_secs: 0.0,
            };
        };

        let start = std::time::Instant::now();

        // Race any meaningful await against ctx.token. This is
        // the whole contract for cancellation — if your code blocks
        // for more than a few ms without select!-ing the token,
        // Ctrl+C won't abort cleanly.
        tokio::select! {
            biased;
            _ = ctx.token.cancelled() => ToolOutcome::Cancelled,
            result = do_the_work(input, &ctx.workdir) => match result {
                Ok(output) => ToolOutcome::Finished {
                    output,
                    images: None,
                    duration_secs: start.elapsed().as_secs_f64(),
                },
                Err(e) => ToolOutcome::Error {
                    error: format!("my_tool: {}", e),
                    duration_secs: start.elapsed().as_secs_f64(),
                },
            },
        }
    }
}

async fn do_the_work(input: &str, workdir: &std::path::Path) -> anyhow::Result<String> {
    // Your actual tool body.
    Ok(format!("processed {} in {}", input, workdir.display()))
}
```

## 3. Register it

Either in the default registry or a custom one the effect runner is wired with.

```rust
// src/providers/tool/mod.rs (abridged)
impl Default for ToolRegistry {
    fn default() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(filesystem::ReadFileTool));
        r.register(Arc::new(filesystem::WriteFileTool));
        r.register(Arc::new(my_tool::MyTool));  // ← new line
        r
    }
}
```

## 4. Describe it to the model

The registry routes calls; the model needs to know the name + schema. Add a `ToolDefinition` alongside the existing ones (same file, adjacent spot) so the outgoing `ChatRequest::tools` includes your tool.

## Dos and don'ts

**Do** emit progress events through `ctx.progress` for multi-step work. The reducer doesn't act on them today, but the renderer will surface them in a follow-up commit and recorded sessions include them.

**Don't** call `tokio::time::sleep` or any other long await without racing it against `ctx.token.cancelled()`. Users expect Ctrl+C to abort; forgetting the select race is the structural bug the old architecture let slip repeatedly.

**Do** return `ToolOutcome::Error` for expected failures (bad input, missing file, permission denied). The error message lands in the tool-result message the model sees next turn — make it useful.

**Don't** hand back paths outside `ctx.workdir` unless the tool explicitly accepts absolute paths. Relative args should resolve against the working directory so the behavior matches user expectations.

**Do** write a unit test. `crate::providers::ctx::test_exec_context(turn, call_id, workdir)` gives you a pre-built `ExecContext` + receiver you can assert against without needing a runtime.

## Testing checklist

- Happy path: valid args, success outcome.
- Missing args: returns `Error`.
- Cancellation: pre-cancel the token, call execute, assert `Cancelled`.
- Workdir handling: relative paths resolve against `ctx.workdir`.
