//! `mouse_move` — move the cursor without clicking. Same
//! `screenshot_id` coord-translation semantics as `click`.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::domain::{ToolDefinition, ToolOutcome};
use crate::providers::ctx::ExecContext;

use super::super::ToolExecutor;
use super::driver::ComputerUseDriver;

pub struct MouseMoveTool {
    driver: Arc<ComputerUseDriver>,
}

impl MouseMoveTool {
    pub fn new(driver: Arc<ComputerUseDriver>) -> Self {
        Self { driver }
    }
}

#[async_trait]
impl ToolExecutor for MouseMoveTool {
    fn name(&self) -> &'static str {
        "mouse_move"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "mouse_move".to_string(),
            description: "Move the mouse cursor to model-space (x, y) without clicking. Same \
                 `screenshot_id` semantics as click: pass the id from the screenshot \
                 whose coordinates you're using, or omit to use the most recent."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "screenshot_id": { "type": "integer" }
                },
                "required": ["x", "y"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: ExecContext) -> ToolOutcome {
        let started = Instant::now();
        if let Err(o) = self.driver.ensure_alive() {
            return o;
        }

        let x = args.get("x").and_then(|v| v.as_i64()).map(|n| n as i32);
        let y = args.get("y").and_then(|v| v.as_i64()).map(|n| n as i32);
        let (x, y) = match (x, y) {
            (Some(x), Some(y)) => (x, y),
            _ => {
                return ToolOutcome::Error {
                    error: "mouse_move requires integer `x` and `y`".to_string(),
                    duration_secs: started.elapsed().as_secs_f64(),
                };
            },
        };
        let screenshot_id = args.get("screenshot_id").and_then(|v| v.as_u64());
        let (sx, sy) = match self.driver.scale_coords(x, y, screenshot_id) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutcome::Error {
                    error: e,
                    duration_secs: started.elapsed().as_secs_f64(),
                };
            },
        };

        let res = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::Cancelled,
            r = self.driver.mouse_move(sx, sy, &ctx.token) => r,
        };
        if let Err(e) = res {
            return ToolOutcome::Error {
                error: format!("mouse_move failed: {}", e),
                duration_secs: started.elapsed().as_secs_f64(),
            };
        }

        ToolOutcome::Finished {
            output: format!("Moved to ({}, {}) [screen: ({}, {})]", x, y, sx, sy),
            images: None,
            duration_secs: started.elapsed().as_secs_f64(),
        }
    }
}
