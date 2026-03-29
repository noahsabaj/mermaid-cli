use anyhow::Result;

use crate::app::{load_config, persist_last_model};
use crate::models::{MessageRole, ModelFactory};
use crate::ollama;
use crate::tui::App;

/// Handle slash commands (e.g., :model, :save, :load, :clear, etc.)
///
/// Commands are executed synchronously and update app state directly.
/// Returns Ok(()) on success, or an error if command execution fails.
pub async fn handle_command(app: &mut App, command: &str) -> Result<()> {
    let parts: Vec<&str> = command.split_whitespace().collect();

    match parts.first().copied() {
        Some("quit") | Some("q") => handle_quit(app),
        Some("clear") => handle_clear(app),
        Some("model") => handle_model(app, parts.get(1).copied()).await,
        Some("refresh") | Some("r") => {
            app.set_status("Not needed - the model explores the codebase via tools");
        },
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

/// Quit the application
fn handle_quit(app: &mut App) {
    app.auto_save_conversation();
    app.quit();
}

/// Clear chat history
fn handle_clear(app: &mut App) {
    app.session_state.messages.clear();
    app.set_status("Chat cleared");
}

/// Switch model or show current model
async fn handle_model(app: &mut App, model_name: Option<&str>) {
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
                2. Run :cloud-setup to configure interactively\n\
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

        // Check if model needs to be pulled (only for ollama models)
        let bare_model = model_id.strip_prefix("ollama/").unwrap_or(&model_id);
        if (model_id.starts_with("ollama/") || !model_id.contains('/'))
            && let Ok(models) = ollama::list_models_async().await {
                let model_exists = models.iter().any(|m| {
                    m == bare_model
                        || (!bare_model.contains(':')
                            && *m == format!("{}:latest", bare_model))
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

                    match pull_model_http(bare_model, &config.ollama.host, config.ollama.port).await
                    {
                        Ok(()) => {
                            app.add_message(
                                MessageRole::System,
                                format!("Model '{}' pulled successfully.", bare_model),
                            );
                        }
                        Err(e) => {
                            app.set_status(format!("Failed to pull model: {}", e));
                            app.add_message(
                                MessageRole::System,
                                format!("Failed to pull model '{}': {}", bare_model, e),
                            );
                            return;
                        }
                    }
                }
            }

        app.set_status(format!("Switching to model: {}...", model_id));

        // Create new model asynchronously
        let model_id_clone = model_id.clone();
        let new_model = tokio::task::spawn(async move {
            ModelFactory::create(&model_id_clone, Some(&config)).await
        });

        match new_model.await {
            Ok(Ok(model)) => {
                // Update the model and model name
                *app.model_state.model.write().await = model;
                app.model_state.model_name = model_id.clone();
                app.model_state.model_id = model_id.clone();

                // Persist the model choice to config
                if let Err(e) = persist_last_model(&model_id) {
                    app.set_status(format!("Switched to {} (failed to save: {})", model_id, e));
                } else {
                    app.set_status(format!("Switched to model: {}", model_id));
                }
            },
            Ok(Err(e)) => {
                app.set_status(format!("Failed to switch model: {}", e));
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
                                "Available conversations:\n{}\n\nUse :load <id> to load a specific conversation",
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

/// Setup Ollama cloud interactively
fn handle_cloud_setup(app: &mut App) {
    app.add_message(
        MessageRole::System,
        "Ollama Cloud Setup\n\n\
        To configure Ollama Cloud, you have two options:\n\n\
        1. Exit Mermaid and run the setup in your terminal:\n\
           mermaid (then type :cloud-setup)\n\n\
        2. Manually configure:\n\
           a) Get API key from: https://ollama.com/cloud\n\
           b) Add to ~/.config/mermaid/config.toml:\n\
              [ollama]\n\
              cloud_api_key = \"your_key_here\"\n\
           c) OR set environment variable:\n\
              export OLLAMA_API_KEY=your_key_here\n\n\
        After configuration, you can use cloud models:\n\
        - :model kimi-k2-thinking:cloud\n\
        - :model qwen3-coder:480b-cloud\n\
        - :model deepseek-v3.1:671b-cloud"
            .to_string(),
    );
}

/// Pull a model from Ollama registry using the HTTP API.
///
/// Uses POST /api/pull with stream: false. Blocks until pull completes.
/// No global timeout — pulls can take a long time for large models.
async fn pull_model_http(model_name: &str, host: &str, port: u16) -> anyhow::Result<()> {
    let url = format!("http://{}:{}/api/pull", host, port);

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()?;

    let response = client
        .post(&url)
        .json(&serde_json::json!({
            "model": model_name,
            "stream": false
        }))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {}: {}", status, body);
    }

    Ok(())
}

/// Show help message
fn handle_help(app: &mut App) {
    app.add_message(
        MessageRole::System,
        "COMMANDS:\n\
         :model [name] - Switch model (auto-pulls if needed) or show current\n\
         :clear - Clear chat history\n\
         :save [name] - Save current conversation\n\
         :load [name] - Load a conversation\n\
         :list - List saved conversations\n\
         :cloud-setup - Configure Ollama Cloud API key\n\
         :quit/:q - Quit the application\n\
         :help/:h - Show this help\n\
         \n\
         KEYBOARD:\n\
         Enter - Send message\n\
         Esc - Stop generation / clear input\n\
         Ctrl+C - Quit\n\
         Alt+T - Toggle thinking mode\n\
         Ctrl+V - Paste image or text from clipboard\n\
         Ctrl+O - Preview attached image\n\
         Ctrl+Click - Open image from chat history\n\
         Up/Down - Navigate input history or scroll chat\n\
         Page Up/Down - Scroll chat\n\
         Mouse Wheel - Scroll chat\n\
         Left/Right - Move cursor in input\n\
         Home/End - Jump to start/end of input"
            .to_string(),
    );
}
