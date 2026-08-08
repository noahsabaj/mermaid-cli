//! `press_key` — single key or key combination at current focus.
//! Keys: simple names (`"Return"`, `"Escape"`, `"Tab"`, `"F5"`,
//! …) or modifier chains (`"ctrl+shift+t"`, `"alt+Tab"`). Uses
//! xdotool/wtype/ydotool syntax pass-through; the model is
//! expected to know the platform convention.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::providers::ctx::ExecContext;
use mermaid_domain::{ToolDefinition, ToolOutcome};
use mermaid_model::constants::POST_KEY_DELAY_MS;

use super::super::ToolExecutor;
use super::computer_use_success;
use super::driver::ComputerUseDriver;

pub struct PressKeyTool {
    driver: Arc<ComputerUseDriver>,
}

impl PressKeyTool {
    pub fn new(driver: Arc<ComputerUseDriver>) -> Self {
        Self { driver }
    }
}

#[async_trait]
impl ToolExecutor for PressKeyTool {
    fn name(&self) -> &'static str {
        "press_key"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "press_key".to_string(),
            description: "Press a key or chord: 'Return', 'Escape', 'Tab', 'F5', 'ctrl+shift+t', \
                 'alt+Tab', etc. Uses xdotool/wtype naming. ALWAYS click the target \
                 window first. Auto-captures the focused window afterwards."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "key": { "type": "string" } },
                "required": ["key"]
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
            "press_key",
            mermaid_runtime::ToolCategory::ComputerUse,
            "computer-use: press_key".to_string(),
            &args,
        )
        .await
        {
            return blocked;
        }
        let key = match args.get("key").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return ToolOutcome::error(
                    "press_key requires `key` string",
                    started.elapsed().as_secs_f64(),
                );
            },
        };

        let res = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::cancelled(),
            r = self.driver.press_key(&key, &ctx.token) => r,
        };
        if let Err(e) = res {
            return ToolOutcome::error(
                format!("press_key failed: {}", e),
                started.elapsed().as_secs_f64(),
            );
        }

        tokio::time::sleep(std::time::Duration::from_millis(POST_KEY_DELAY_MS)).await;
        let base_msg = format!("Pressed: {}", key);

        let (summary, image) = match super::emit_auto_screenshot(
            &self.driver,
            &ctx,
            "press_key auto-screenshot",
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
            computer_use_success("press_key", args, out, started.elapsed().as_secs_f64());
        if let Some(image) = image {
            outcome = outcome.with_images(vec![image]);
        }
        outcome
    }
}
