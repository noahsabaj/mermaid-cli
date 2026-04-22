//! `screenshot` tool — capture the display (or a slice of it) and
//! return the image for the model to reason about.
//!
//! Accepted modes: `fullscreen` (default), `focused`, `monitor`,
//! `region`, `window`. Coordinate metadata is registered in the
//! driver's `ScreenshotRegistry` so later `click` / `mouse_move`
//! calls can quote `screenshot_id` to lock their coordinates to this
//! specific capture.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use crate::domain::{ToolDefinition, ToolOutcome};
use crate::providers::ctx::{ExecContext, ProgressEvent};

use super::super::ToolExecutor;
use super::driver::{ComputerUseDriver, ScreenshotSpec};

pub struct ScreenshotTool {
    driver: Arc<ComputerUseDriver>,
}

impl ScreenshotTool {
    pub fn new(driver: Arc<ComputerUseDriver>) -> Self {
        Self { driver }
    }
}

#[async_trait]
impl ToolExecutor for ScreenshotTool {
    fn name(&self) -> &'static str {
        "screenshot"
    }

    fn schema(&self) -> ToolDefinition {
        ToolDefinition {
            name: "screenshot".to_string(),
            description: "Capture the display and return it as a base64 PNG plus a screenshot id. \
                 Modes: 'fullscreen' (default), 'focused' (active window, X11/macOS only), \
                 'monitor' (pass `monitor` with an output name from xrandr), 'region' \
                 (pass `region` as 'X,Y,WIDTHxHEIGHT'), 'window' (pass `window` with a \
                 title substring, X11 only). The returned id can be passed as `screenshot_id` \
                 on later `click` / `mouse_move` calls so coordinates are translated using \
                 the right scale + offset."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["fullscreen", "focused", "monitor", "region", "window"], "default": "fullscreen" },
                    "monitor": { "type": "string", "description": "Output name from xrandr --query (required for mode='monitor')" },
                    "region": { "type": "string", "description": "'X,Y,WIDTHxHEIGHT' (required for mode='region')" },
                    "window": { "type": "string", "description": "Window title substring (required for mode='window', X11 only)" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: ExecContext) -> ToolOutcome {
        let started = Instant::now();

        if let Err(outcome) = self.driver.ensure_alive() {
            return outcome;
        }

        let spec = match parse_spec(&args) {
            Ok(s) => s,
            Err(e) => {
                return ToolOutcome::Error {
                    error: e,
                    duration_secs: started.elapsed().as_secs_f64(),
                };
            },
        };

        let result = tokio::select! {
            biased;
            _ = ctx.token.cancelled() => return ToolOutcome::Cancelled,
            r = self.driver.capture(spec, &ctx.token) => r,
        };

        let cap = match result {
            Ok(c) => c,
            Err(e) => {
                return ToolOutcome::Error {
                    error: format!("screenshot capture failed: {}", e),
                    duration_secs: started.elapsed().as_secs_f64(),
                };
            },
        };

        // Emit an inline preview via the progress channel. The reducer
        // routes `image/*` artifacts onto the active assistant
        // message's `images` for immediate chat display; the final
        // outcome carries the same base64 so conversation save/load
        // keeps the screenshot in history. Dedupe between the two
        // paths is owned by the chat widget (Commit 6).
        let _ = ctx
            .progress
            .send(ProgressEvent::Artifact {
                mime: "image/png".to_string(),
                data: cap.raw_bytes,
                caption: Some(format!("screenshot #{}", cap.id)),
            })
            .await;

        ToolOutcome::Finished {
            output: cap.summary,
            images: Some(vec![cap.base64_png]),
            duration_secs: started.elapsed().as_secs_f64(),
        }
    }
}

fn parse_spec(args: &Value) -> Result<ScreenshotSpec, String> {
    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("fullscreen")
        .to_lowercase();
    match mode.as_str() {
        "fullscreen" | "" => Ok(ScreenshotSpec::Fullscreen),
        "focused" => Ok(ScreenshotSpec::Focused),
        "monitor" => {
            let name = args
                .get("monitor")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    "Monitor name required for mode='monitor'. Use `xrandr --query` to list."
                        .to_string()
                })?
                .to_string();
            Ok(ScreenshotSpec::Monitor(name))
        },
        "region" => {
            let region = args.get("region").and_then(|v| v.as_str()).ok_or_else(|| {
                "Region required for mode='region'. Format: 'X,Y,WIDTHxHEIGHT'".to_string()
            })?;
            parse_region(region)
        },
        "window" => {
            let title = args
                .get("window")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    "Window title required for mode='window'. Use list_windows first.".to_string()
                })?
                .to_string();
            Ok(ScreenshotSpec::Window(title))
        },
        other => Err(format!(
            "Unknown screenshot mode '{}'. Valid: fullscreen|focused|monitor|region|window",
            other
        )),
    }
}

fn parse_region(s: &str) -> Result<ScreenshotSpec, String> {
    // 'X,Y,WIDTHxHEIGHT' e.g. '100,50,800x600'
    let parts: Vec<&str> = s.splitn(3, ',').collect();
    if parts.len() != 3 {
        return Err(format!(
            "Invalid region '{}'. Expected 'X,Y,WIDTHxHEIGHT' (e.g. '100,50,800x600').",
            s
        ));
    }
    let x: i32 = parts[0]
        .parse()
        .map_err(|_| format!("Invalid X in region '{}'", s))?;
    let y: i32 = parts[1]
        .parse()
        .map_err(|_| format!("Invalid Y in region '{}'", s))?;
    let (w, h) = parts[2]
        .split_once('x')
        .ok_or_else(|| format!("Invalid WIDTHxHEIGHT in region '{}'", s))?;
    let width: u32 = w
        .parse()
        .map_err(|_| format!("Invalid width in region '{}'", s))?;
    let height: u32 = h
        .parse()
        .map_err(|_| format!("Invalid height in region '{}'", s))?;
    Ok(ScreenshotSpec::Region(x, y, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec_defaults_to_fullscreen() {
        let s = parse_spec(&serde_json::json!({})).unwrap();
        assert!(matches!(s, ScreenshotSpec::Fullscreen));
    }

    #[test]
    fn parse_spec_region_parses_valid() {
        let s =
            parse_spec(&serde_json::json!({"mode": "region", "region": "10,20,800x600"})).unwrap();
        match s {
            ScreenshotSpec::Region(x, y, w, h) => {
                assert_eq!((x, y, w, h), (10, 20, 800, 600));
            },
            _ => panic!("expected Region"),
        }
    }

    #[test]
    fn parse_spec_region_rejects_malformed() {
        let err = parse_spec(&serde_json::json!({"mode": "region", "region": "oops"})).unwrap_err();
        assert!(err.contains("Expected"));
    }

    #[test]
    fn parse_spec_monitor_requires_name() {
        let err = parse_spec(&serde_json::json!({"mode": "monitor"})).unwrap_err();
        assert!(err.contains("Monitor name"));
    }

    #[test]
    fn parse_spec_mode_is_case_insensitive() {
        let s = parse_spec(&serde_json::json!({"mode": "FullScreen"})).unwrap();
        assert!(matches!(s, ScreenshotSpec::Fullscreen));
    }
}
