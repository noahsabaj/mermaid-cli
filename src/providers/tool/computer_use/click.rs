//! `click` — mouse click at model-space `(x, y)`. `screenshot_id`
//! selects which capture the coords refer to (None = latest). After
//! clicking, auto-captures the focused window so the model can
//! verify the result inline.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::domain::{ToolDefinition, ToolOutcome};
use crate::providers::ctx::ExecContext;
use mermaid_model::constants::POST_CLICK_DELAY_MS;

use super::super::ToolExecutor;
use super::computer_use_success;
use super::driver::ComputerUseDriver;

pub struct ClickTool {
    driver: Arc<ComputerUseDriver>,
}

impl ClickTool {
    pub fn new(driver: Arc<ComputerUseDriver>) -> Self {
        Self { driver }
    }
}

#[async_trait]
impl ToolExecutor for ClickTool {
    fn name(&self) -> &'static str {
        "click"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "click".to_string(),
            description:
                "Click at model-space (x, y). Pass `screenshot_id` to lock coordinates to a \
                 specific past screenshot; omit for the most recent. Auto-captures the \
                 focused window afterwards so the result is visible inline."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "button": { "type": "string", "enum": ["left", "middle", "right"], "default": "left" },
                    "screenshot_id": { "type": "integer" }
                },
                "required": ["x", "y"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: ExecContext) -> ToolOutcome {
        let started = Instant::now();
        if let Err(error) = self.driver.ensure_alive_async().await {
            return ToolOutcome::error(error, started.elapsed().as_secs_f64());
        }
        if let Some(blocked) = super::super::policy_gate::gate_external(
            &ctx,
            "click",
            mermaid_runtime::ToolCategory::ComputerUse,
            "computer-use: click".to_string(),
            &args,
        )
        .await
        {
            return blocked;
        }

        let x = args.get("x").and_then(|v| v.as_i64()).map(|n| n as i32);
        let y = args.get("y").and_then(|v| v.as_i64()).map(|n| n as i32);
        let (x, y) = match (x, y) {
            (Some(x), Some(y)) => (x, y),
            _ => {
                return ToolOutcome::error(
                    "click requires integer `x` and `y`",
                    started.elapsed().as_secs_f64(),
                );
            },
        };
        let button = args
            .get("button")
            .and_then(|v| v.as_str())
            .unwrap_or("left")
            .to_string();
        let screenshot_id = args.get("screenshot_id").and_then(|v| v.as_u64());

        let (sx, sy) = match self.driver.scale_coords(x, y, screenshot_id) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::error(e, started.elapsed().as_secs_f64()),
        };

        let click_res = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            r = self.driver.click(sx, sy, &button, &ctx.token) => r,
        };
        if let Err(e) = click_res {
            return ToolOutcome::error(
                format!("click failed: {}", e),
                started.elapsed().as_secs_f64(),
            );
        }

        // Let the WM process focus change + UI update before the
        // auto-screenshot captures the result.
        tokio::time::sleep(std::time::Duration::from_millis(POST_CLICK_DELAY_MS)).await;

        let mut msg = format!(
            "Clicked {} at ({}, {}) [screen: ({}, {})]",
            button, x, y, sx, sy
        );
        if let Some(warning) = self.driver.check_cursor_landed(sx, sy).await {
            msg.push('\n');
            msg.push_str(&warning);
        }

        let (summary, image) =
            match super::emit_auto_screenshot(&self.driver, &ctx, "click auto-screenshot").await {
                Some((s, b64)) => (Some(s), Some(b64)),
                None => (None, None),
            };

        let final_output = match &summary {
            Some(s) => format!("{}\n[auto-screenshot: {}]", msg, s),
            None => msg,
        };
        let mut outcome =
            computer_use_success("click", args, final_output, started.elapsed().as_secs_f64());
        if let Some(image) = image {
            outcome = outcome.with_images(vec![image]);
        }
        outcome
    }
}
