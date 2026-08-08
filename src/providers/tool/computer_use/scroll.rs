//! `scroll` — wheel scroll in "up" or "down" direction. Amount is
//! clamped to `[1, MAX_SCROLL_AMOUNT]` so a runaway model can't
//! request a million ticks (would blow ARG_MAX building xdotool's
//! argv on the X11 path).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::providers::ctx::ExecContext;
use mermaid_domain::{ToolDefinition, ToolOutcome};
use mermaid_model::constants::MAX_SCROLL_AMOUNT;

use super::super::ToolExecutor;
use super::computer_use_success;
use super::driver::ComputerUseDriver;

pub struct ScrollTool {
    driver: Arc<ComputerUseDriver>,
}

impl ScrollTool {
    pub fn new(driver: Arc<ComputerUseDriver>) -> Self {
        Self { driver }
    }
}

#[async_trait]
impl ToolExecutor for ScrollTool {
    fn name(&self) -> &'static str {
        "scroll"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "scroll".to_string(),
            description: "Scroll the focused window. `direction` is 'up' or 'down'; `amount` is \
                 clamped to 1..=100 wheel ticks. Scrolls in the currently-focused window; \
                 click on the scroll target first if it isn't focused."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "direction": { "type": "string", "enum": ["up", "down"] },
                    "amount": { "type": "integer", "minimum": 1, "maximum": MAX_SCROLL_AMOUNT }
                },
                "required": ["direction", "amount"]
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
            "scroll",
            mermaid_runtime::ToolCategory::ComputerUse,
            "computer-use: scroll".to_string(),
            &args,
        )
        .await
        {
            return blocked;
        }

        let direction = args
            .get("direction")
            .and_then(|v| v.as_str())
            .unwrap_or("down")
            .to_string();
        if direction != "up" && direction != "down" {
            return ToolOutcome::error(
                format!(
                    "scroll: direction must be 'up' or 'down', got '{}'",
                    direction
                ),
                started.elapsed().as_secs_f64(),
            );
        }
        let requested = args
            .get("amount")
            .and_then(|v| v.as_i64())
            .map(|n| n as i32)
            .unwrap_or(3);
        let amount = requested.clamp(1, MAX_SCROLL_AMOUNT);

        let res = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            r = self.driver.scroll(&direction, amount, &ctx.token) => r,
        };
        if let Err(e) = res {
            return ToolOutcome::error(
                format!("scroll failed: {}", e),
                started.elapsed().as_secs_f64(),
            );
        }

        let clamp_note = if requested != amount {
            format!(" (clamped from {} to {})", requested, amount)
        } else {
            String::new()
        };
        computer_use_success(
            "scroll",
            args,
            format!("Scrolled {} by {}{}", direction, amount, clamp_note),
            started.elapsed().as_secs_f64(),
        )
    }
}
