//! `press_key` — single key or key combination at current focus.
//! Keys: simple names (`"Return"`, `"Escape"`, `"Tab"`, `"F5"`,
//! …) or modifier chains (`"ctrl+shift+t"`, `"alt+Tab"`). Uses
//! xdotool/wtype/ydotool syntax pass-through; the model is
//! expected to know the platform convention.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::constants::POST_KEY_DELAY_MS;
use crate::domain::{ToolDefinition, ToolOutcome};
use crate::providers::ctx::{ExecContext, ProgressEvent};

use super::super::ToolExecutor;
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
        if let Err(o) = self.driver.ensure_alive() {
            return o;
        }
        let key = match args.get("key").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => {
                return ToolOutcome::Error {
                    error: "press_key requires `key` string".to_string(),
                    duration_secs: started.elapsed().as_secs_f64(),
                };
            },
        };

        let res = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::Cancelled,
            r = self.driver.press_key(&key, &ctx.token) => r,
        };
        if let Err(e) = res {
            return ToolOutcome::Error {
                error: format!("press_key failed: {}", e),
                duration_secs: started.elapsed().as_secs_f64(),
            };
        }

        tokio::time::sleep(std::time::Duration::from_millis(POST_KEY_DELAY_MS)).await;
        let base_msg = format!("Pressed: {}", key);

        let (summary, image) = match self.driver.capture_focused_for_autoshot(&ctx.token).await {
            Some((s, b64)) => (Some(s), Some(b64)),
            None => (None, None),
        };

        if let Some(b64) = &image
            && let Ok(bytes) =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
        {
            let _ = ctx
                .progress
                .send(ProgressEvent::Artifact {
                    mime: "image/png".to_string(),
                    data: bytes,
                    caption: Some("press_key auto-screenshot".to_string()),
                })
                .await;
        }

        let out = match &summary {
            Some(s) => format!("{}\n[auto-screenshot: {}]", base_msg, s),
            None => base_msg,
        };
        ToolOutcome::Finished {
            output: out,
            images: image.map(|b| vec![b]),
            duration_secs: started.elapsed().as_secs_f64(),
        }
    }
}
