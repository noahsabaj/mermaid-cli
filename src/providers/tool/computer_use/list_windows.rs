//! `list_windows` — enumerate visible window titles. X11 only;
//! Wayland's wlr-protocols ecosystem has no portable window
//! enumeration primitive.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::domain::{ToolDefinition, ToolOutcome};
use crate::providers::ctx::ExecContext;

use super::super::ToolExecutor;
use super::driver::ComputerUseDriver;

pub struct ListWindowsTool {
    driver: Arc<ComputerUseDriver>,
}

impl ListWindowsTool {
    pub fn new(driver: Arc<ComputerUseDriver>) -> Self {
        Self { driver }
    }
}

#[async_trait]
impl ToolExecutor for ListWindowsTool {
    fn name(&self) -> &'static str {
        "list_windows"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_windows".to_string(),
            description: "List visible window titles. X11 only. On Wayland, use screenshot mode \
                 'fullscreen' or 'monitor' instead — window enumeration is not portable \
                 across compositors."
                .to_string(),
            input_schema: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    async fn execute(&self, _args: Value, ctx: ExecContext) -> ToolOutcome {
        let started = Instant::now();
        if let Err(o) = self.driver.ensure_alive() {
            return o;
        }

        let windows = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::Cancelled,
            r = self.driver.list_windows(&ctx.token) => match r {
                Ok(w) => w,
                Err(e) => return ToolOutcome::Error {
                    error: format!("list_windows failed: {}", e),
                    duration_secs: started.elapsed().as_secs_f64(),
                },
            },
        };

        let output = if windows.is_empty() {
            "No visible windows found.".to_string()
        } else {
            let list = windows
                .iter()
                .map(|w| format!("  - {}", w))
                .collect::<Vec<_>>()
                .join("\n");
            format!("Visible windows ({}):\n{}", windows.len(), list)
        };

        ToolOutcome::Finished {
            output,
            images: None,
            duration_secs: started.elapsed().as_secs_f64(),
        }
    }
}
