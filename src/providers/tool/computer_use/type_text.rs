//! `type_text` — simulate keyboard input at the current focus. Pairs
//! with click (always click first so keystrokes go to the right
//! window) and auto-captures the focused window afterwards.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::constants::POST_TYPE_DELAY_MS;
use crate::domain::{ToolDefinition, ToolOutcome};
use crate::providers::ctx::ExecContext;

use super::super::ToolExecutor;
use super::computer_use_success;
use super::driver::ComputerUseDriver;

pub struct TypeTextTool {
    driver: Arc<ComputerUseDriver>,
}

impl TypeTextTool {
    pub fn new(driver: Arc<ComputerUseDriver>) -> Self {
        Self { driver }
    }
}

#[async_trait]
impl ToolExecutor for TypeTextTool {
    fn name(&self) -> &'static str {
        "type_text"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "type_text".to_string(),
            description:
                "Type literal text at the current keyboard focus. ALWAYS click the target \
                 window first — keystrokes go to whichever window currently has focus. \
                 Auto-captures the focused window afterwards."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
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
            "type_text",
            crate::runtime::ToolCategory::ComputerUse,
            "computer-use: type_text".to_string(),
            &args,
        )
        .await
        {
            return blocked;
        }
        let text = match args.get("text").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return ToolOutcome::error(
                    "type_text requires `text` string",
                    started.elapsed().as_secs_f64(),
                );
            },
        };

        let res = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            r = self.driver.type_text(&text, &ctx.token) => r,
        };
        if let Err(e) = res {
            return ToolOutcome::error(
                format!("type_text failed: {}", e),
                started.elapsed().as_secs_f64(),
            );
        }

        tokio::time::sleep(std::time::Duration::from_millis(POST_TYPE_DELAY_MS)).await;

        let typed_preview: String = text.chars().take(50).collect();
        let base_msg = format!("Typed: {}", typed_preview);

        let (summary, image) = match super::emit_auto_screenshot(
            &self.driver,
            &ctx,
            "type_text auto-screenshot",
        )
        .await
        {
            Some((s, b64)) => (Some(s), Some(b64)),
            None => (None, None),
        };

        let out = match &summary {
            Some(s) => format!("{}\n[auto-screenshot: {}]", base_msg, s),
            None => base_msg,
        };
        let mut outcome =
            computer_use_success("type_text", args, out, started.elapsed().as_secs_f64());
        if let Some(image) = image {
            outcome = outcome.with_images(vec![image]);
        }
        outcome
    }
}
