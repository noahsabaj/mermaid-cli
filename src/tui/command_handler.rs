use anyhow::Result;
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;

use crate::app::{load_config, persist_last_model, persist_reasoning_for_model};
use crate::models::{MessageRole, ModelFactory, ReasoningLevel};
use crate::ollama;
use crate::tui::App;
use crate::utils::drain_complete_lines;

use super::render::render_ui;

/// Convenience alias so the long terminal type doesn't litter every signature.
type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

/// Handle slash commands (e.g., /model, /save, /load, /clear, etc.)
///
/// Commands are executed synchronously and update app state directly.
/// `terminal` is threaded through so long-running commands (model pull)
/// can render live progress instead of freezing the TUI for minutes.
/// Returns Ok(()) on success, or an error if command execution fails.
pub async fn handle_command(
    app: &mut App,
    terminal: &mut TuiTerminal,
    command: &str,
) -> Result<()> {
    let parts: Vec<&str> = command.split_whitespace().collect();

    match parts.first().copied() {
        Some("quit") | Some("q") => handle_quit(app),
        Some("clear") => handle_clear(app),
        Some("model") => handle_model(app, terminal, parts.get(1).copied()).await,
        Some("reasoning") => handle_reasoning(app, parts.get(1).copied()),
        Some("save") => handle_save(app, parts.get(1).copied()),
        Some("load") => handle_load(app, parts.get(1).copied()),
        Some("list") => handle_list(app),
        Some("cloud-setup") => handle_cloud_setup(app),
        Some("help") | Some("h") => handle_help(app),
        _ => {
            app.set_status(format!("Unknown command: {}", command));
        },
    }

    Ok(())
}

/// Show or set the reasoning level. Mirrors `handle_model`'s show/set
/// pattern. Persists on set so the choice survives restarts.
fn handle_reasoning(app: &mut App, level_arg: Option<&str>) {
    let Some(level_str) = level_arg else {
        app.set_status(format!(
            "Current reasoning level: {}",
            app.model_state.base_config.reasoning.as_str()
        ));
        return;
    };

    let parsed = match level_str.to_lowercase().as_str() {
        "none" => Some(ReasoningLevel::None),
        "minimal" => Some(ReasoningLevel::Minimal),
        "low" => Some(ReasoningLevel::Low),
        "medium" => Some(ReasoningLevel::Medium),
        "high" => Some(ReasoningLevel::High),
        "max" => Some(ReasoningLevel::Max),
        "xhigh" => Some(ReasoningLevel::XHigh),
        _ => None,
    };

    let Some(level) = parsed else {
        app.set_status(format!(
            "Unknown reasoning level: '{}' (valid: none, minimal, low, medium, high, max, xhigh)",
            level_str
        ));
        return;
    };

    app.model_state.set_reasoning(level);
    match persist_reasoning_for_model(&app.model_state.model_id, level) {
        Ok(()) => app.set_status(format!(
            "Reasoning set to: {} for {}",
            level.as_str(),
            app.model_state.model_id
        )),
        Err(e) => app.set_status(format!(
            "Reasoning set to: {} (failed to persist: {})",
            level.as_str(),
            e
        )),
    }
}

/// Quit the application
fn handle_quit(app: &mut App) {
    app.auto_save_conversation();
    app.quit();
}

/// Clear chat history
fn handle_clear(app: &mut App) {
    app.session_state.messages.clear();
    app.ui_state.markdown_cache.clear();
    app.set_status("Chat cleared");
}

/// Switch model or show current model
async fn handle_model(app: &mut App, terminal: &mut TuiTerminal, model_name: Option<&str>) {
    if let Some(model_name) = model_name {
        // Parse the model name (could be provider/model or just model)
        let model_id = if model_name.contains('/') {
            model_name.to_string()
        } else {
            // Assume ollama if no provider specified
            format!("ollama/{}", model_name)
        };

        // Check if this is a cloud model and if cloud is configured
        if ollama::is_cloud_model(&model_id) && !ollama::is_cloud_configured() {
            app.add_message(
                MessageRole::System,
                "Cloud model requested but Ollama Cloud is not configured.\n\n\
                To use cloud models:\n\
                1. Get an API key from https://ollama.com/cloud\n\
                2. Run /cloud-setup to configure interactively\n\
                   OR\n\
                3. Set environment variable: export OLLAMA_API_KEY=your_key\n\
                   OR\n\
                4. Add to config: ~/.config/mermaid/config.toml\n\
                   [ollama]\n\
                   cloud_api_key = \"your_key\"\n\n\
                Available cloud models:\n\
                - kimi-k2-thinking:cloud\n\
                - qwen3-coder:480b-cloud\n\
                - deepseek-v3.1:671b-cloud\n\
                - gpt-oss:120b-cloud"
                    .to_string(),
            );
            return;
        }

        // Try to create the new model
        let config = match load_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                app.set_status(format!("Failed to load config: {}", e));
                return;
            },
        };

        // Check if model needs to be pulled (only for ollama models).
        // Uses HTTP /api/tags via ModelFactory so the user's configured
        // host/port is honored (unlike the retired CLI-parsing path).
        let bare_model = model_id.strip_prefix("ollama/").unwrap_or(&model_id);
        if (model_id.starts_with("ollama/") || !model_id.contains('/'))
            && let Ok(models) = ModelFactory::from_config(&config)
                .list_models("ollama")
                .await
        {
            let model_exists = models.iter().any(|m| {
                m == bare_model
                    || (!bare_model.contains(':') && *m == format!("{}:latest", bare_model))
            });

            if !model_exists {
                app.set_status(format!("Pulling model: {}...", bare_model));
                app.add_message(
                    MessageRole::System,
                    format!(
                        "Model '{}' not found locally. Pulling from registry...",
                        bare_model
                    ),
                );

                match pull_model_http(
                    app,
                    terminal,
                    bare_model,
                    &config.ollama.host,
                    config.ollama.port,
                )
                .await
                {
                    Ok(()) => {
                        app.add_message(
                            MessageRole::System,
                            format!("Model '{}' pulled successfully.", bare_model),
                        );
                    },
                    Err(e) => {
                        app.set_status(format!("Failed to pull model: {}", e));
                        app.add_message(
                            MessageRole::System,
                            format!("Failed to pull model '{}': {}", bare_model, e),
                        );
                        return;
                    },
                }
            }
        }

        app.set_status(format!("Switching to model: {}...", model_id));

        // Create new model
        let new_model = ModelFactory::create(&model_id, Some(&config)).await;

        match new_model {
            Ok(model) => {
                // Snapshot the new model's reasoning capability BEFORE
                // moving it into the RwLock — used by the status-bar
                // snap-divergence indicator (Step 5b).
                let new_supported_reasoning = model.capabilities().supports_reasoning.clone();

                // Update the model and model name
                *app.model_state.model.write().await = model;
                app.model_state.model_name = model_id.clone();
                app.model_state.model_id = model_id.clone();
                app.model_state.supported_reasoning = new_supported_reasoning;

                // Reset capability flags for the new model — they'll be
                // re-detected from the model's responses. Temperature
                // and max_tokens are preserved from the current session.
                app.model_state.vision_supported = None;

                // Load the per-model reasoning preference for the new
                // model (Step 5b). Falls back to the global default if no
                // per-model entry exists. This is what makes /model
                // switches feel right: switching to Sonnet gives you back
                // the `high` you set last time, switching to Ollama gives
                // you back the `low` you set last time.
                let new_reasoning = config
                    .reasoning_per_model
                    .get(&model_id)
                    .copied()
                    .unwrap_or(config.default_model.reasoning);
                app.model_state.set_reasoning(new_reasoning);

                // Persist the model choice to config
                if let Err(e) = persist_last_model(&model_id) {
                    app.set_status(format!("Switched to {} (failed to save: {})", model_id, e));
                } else {
                    app.set_status(format!("Switched to model: {}", model_id));
                }
            },
            Err(e) => {
                app.set_status(format!("Failed to switch model: {}", e));
            },
        }
    } else {
        app.set_status(format!("Current model: {}", app.model_state.model_name));
    }
}

/// Save current conversation
fn handle_save(app: &mut App, name: Option<&str>) {
    if let Err(e) = app.save_conversation() {
        app.set_status(format!("Failed to save: {}", e));
    } else {
        app.set_status(if let Some(name) = name {
            format!("Conversation saved as: {}", name)
        } else {
            "Conversation saved".to_string()
        });
    }
}

/// Load a conversation by name or show selector
fn handle_load(app: &mut App, name: Option<&str>) {
    if let Some(ref manager) = app.session_state.conversation_manager {
        if let Some(name) = name {
            // Load specific conversation
            match manager.load_conversation(name) {
                Ok(conv) => {
                    app.load_conversation(conv);
                },
                Err(e) => {
                    app.set_status(format!("Failed to load: {}", e));
                },
            }
        } else {
            // Show list of available conversations
            match manager.list_conversations() {
                Ok(conversations) => {
                    if conversations.is_empty() {
                        app.set_status("No saved conversations found");
                    } else {
                        let list = conversations
                            .iter()
                            .map(|c| c.summary())
                            .collect::<Vec<_>>()
                            .join("\n");
                        app.add_message(
                            MessageRole::System,
                            format!(
                                "Available conversations:\n{}\n\nUse /load <id> to load a specific conversation",
                                list
                            ),
                        );
                    }
                },
                Err(e) => {
                    app.set_status(format!("Failed to list conversations: {}", e));
                },
            }
        }
    }
}

/// List saved conversations
fn handle_list(app: &mut App) {
    if let Some(ref manager) = app.session_state.conversation_manager {
        match manager.list_conversations() {
            Ok(conversations) => {
                if conversations.is_empty() {
                    app.set_status("No saved conversations in this directory");
                } else {
                    let list = conversations
                        .iter()
                        .map(|c| c.summary())
                        .collect::<Vec<_>>()
                        .join("\n");
                    app.add_message(
                        MessageRole::System,
                        format!("Saved conversations:\n{}", list),
                    );
                }
            },
            Err(e) => {
                app.set_status(format!("Failed to list conversations: {}", e));
            },
        }
    }
}

/// Show Ollama Cloud setup instructions
///
/// Interactive stdin-based setup is not available from inside the TUI (stdin
/// is owned by crossterm raw mode), so we surface the two paths that work
/// from the current session: an environment variable or a config edit.
fn handle_cloud_setup(app: &mut App) {
    app.add_message(
        MessageRole::System,
        "Ollama Cloud Setup\n\n\
        Option 1 \u{2014} environment variable (session-scoped):\n\
          export OLLAMA_API_KEY=your_key_here\n\
          (set before launching Mermaid; takes effect on next start)\n\n\
        Option 2 \u{2014} config file (persisted):\n\
          Edit ~/.config/mermaid/config.toml and add:\n\
            [ollama]\n\
            cloud_api_key = \"your_key_here\"\n\n\
        Get an API key at: https://ollama.com/cloud\n\n\
        After setup, switch to a cloud model with /model <name>:cloud\n\
        (e.g., /model kimi-k2-thinking:cloud, /model qwen3-coder:480b-cloud,\n\
        /model deepseek-v3.1:671b-cloud)."
            .to_string(),
    );
}

/// Pull a model from Ollama registry using the streaming HTTP API.
///
/// Sends `{"stream": true}` so progress arrives as NDJSON chunks rather than
/// blocking on a single multi-minute response. Each chunk is parsed and used
/// to update the TUI status line, and the terminal is redrawn between chunks
/// so the user sees live progress instead of a frozen UI.
///
/// Errors from the server (`{"error": "..."}`) propagate as `Err`; otherwise
/// the function returns once the stream completes (the final chunk usually
/// has `{"status": "success"}`).
async fn pull_model_http(
    app: &mut App,
    terminal: &mut TuiTerminal,
    model_name: &str,
    host: &str,
    port: u16,
) -> anyhow::Result<()> {
    let url = format!("http://{}:{}/api/pull", host, port);

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()?;

    let response = client
        .post(&url)
        .json(&serde_json::json!({
            "model": model_name,
            "stream": true
        }))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {}: {}", status, body);
    }

    // Drain NDJSON chunks. Use the shared `drain_complete_lines` helper so
    // we get UTF-8-safe parsing even when TCP chunks split codepoints
    // (Ollama statuses are ASCII today, but the helper costs nothing extra).
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result?;
        buf.extend_from_slice(&chunk);

        for line in drain_complete_lines(&mut buf) {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: serde_json::Value = serde_json::from_str(&line)
                .map_err(|e| anyhow::anyhow!("invalid pull progress JSON: {} ({})", e, line))?;

            // Server-side error (bad model name, registry unreachable, etc.)
            // surfaces in the first chunk now instead of after a full request
            // round-trip.
            if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
                anyhow::bail!("{}", err);
            }

            let status = parsed
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pulling");

            // For download chunks Ollama emits `completed`/`total` byte counts.
            // Render them as a percentage + human-readable size if both fields
            // are present; otherwise just show the status.
            let display = match (
                parsed.get("completed").and_then(|v| v.as_u64()),
                parsed.get("total").and_then(|v| v.as_u64()),
            ) {
                (Some(done), Some(total)) if total > 0 => {
                    let pct = (done as f64 / total as f64 * 100.0) as u64;
                    format!(
                        "Pulling {}: {} {}% ({} / {})",
                        model_name,
                        status,
                        pct,
                        format_bytes(done),
                        format_bytes(total),
                    )
                },
                _ => format!("Pulling {}: {}", model_name, status),
            };
            app.set_status(display);

            // Render between chunks so the user actually sees the updates
            // — `handle_command` is awaited synchronously from the main
            // event loop, which won't redraw until we return.
            terminal.draw(|f| render_ui(f, app))?;
        }
    }

    Ok(())
}

/// Format a byte count as a short human-readable string (e.g. "1.4 GB").
fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{} B", n)
    }
}

/// Show help message. The COMMANDS section is rendered from the static
/// `COMMAND_REGISTRY` (single source of truth) so adding a new command
/// updates this output automatically.
fn handle_help(app: &mut App) {
    let mut output = String::from("COMMANDS:\n");
    for cmd in crate::tui::slash_commands::COMMAND_REGISTRY {
        let name_with_args = match cmd.arg_hint {
            Some(hint) => format!("/{} {}", cmd.name, hint),
            None => format!("/{}", cmd.name),
        };
        let alias_suffix = if cmd.aliases.is_empty() {
            String::new()
        } else {
            let aliases: Vec<String> = cmd.aliases.iter().map(|a| format!("/{}", a)).collect();
            format!(" (alias: {})", aliases.join(", "))
        };
        output.push_str(&format!(
            "  {} - {}{}\n",
            name_with_args, cmd.description, alias_suffix
        ));
    }
    output.push_str(
        "\nKEYBOARD:\n\
         Enter - Send message\n\
         Esc - Stop generation / clear input\n\
         Ctrl+C - Quit\n\
         Alt+T - Cycle reasoning level (none → low → medium → high → max → none)\n\
         Ctrl+V - Paste image or text from clipboard\n\
         Ctrl+O - Preview attached image\n\
         Ctrl+Click - Open image from chat history\n\
         Up/Down - Navigate input history or scroll chat\n\
         Page Up/Down - Scroll chat\n\
         Mouse Wheel - Scroll chat\n\
         Left/Right - Move cursor in input\n\
         Home/End - Jump to start/end of input",
    );
    app.add_message(MessageRole::System, output);
}

#[cfg(test)]
mod tests {
    use super::format_bytes;

    #[test]
    fn format_bytes_scales_correctly() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MB");
        assert_eq!(
            format_bytes(3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024),
            "3.5 GB"
        );
    }
}
