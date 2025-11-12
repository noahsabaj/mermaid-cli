use anyhow::Result;
use std::path::Path;

use crate::app::load_config;
use crate::context::ContextLoader;
use crate::models::{MessageRole, ModelFactory};
use crate::session::SessionState;
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
        Some("refresh") | Some("r") => handle_refresh(app).await,
        Some("save") => handle_save(app, parts.get(1).copied()),
        Some("load") => handle_load(app, parts.get(1).copied()),
        Some("list") => handle_list(app),
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

        app.set_status(format!("Switching to model: {}...", model_id));

        // Try to create the new model
        let config = match load_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                app.set_status(format!("Failed to load config: {}", e));
                return;
            },
        };

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
                app.set_status(format!("Switched to model: {}", model_id));

                // Save the model preference to session
                let mut session = SessionState::load().unwrap_or_default();
                session.set_model(model_id);
                let _ = session.save();
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

/// Refresh file context from disk
async fn handle_refresh(app: &mut App) {
    match ContextLoader::new() {
        Ok(loader) => match loader.load(Path::new(".")).await {
            Ok(new_context) => {
                app.context.files = new_context.files;
                app.context.token_count = new_context.token_count;
                app.set_status(format!(
                    "Refreshed: {} files, ~{} tokens",
                    app.context.files.len(),
                    app.context.token_count
                ));
            },
            Err(e) => {
                app.set_status(format!("Failed to refresh: {}", e));
            },
        },
        Err(e) => {
            app.set_status(format!("Failed to create loader: {}", e));
        },
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

/// Show help message
fn handle_help(app: &mut App) {
    app.add_message(
        MessageRole::System,
        "Commands:\n\
         :quit/:q - Quit the application\n\
         :clear - Clear chat history\n\
         :model [name] - Switch model or show current\n\
         :sidebar/:sb - Toggle file sidebar\n\
         :refresh/:r - Refresh file context from disk\n\
         :save [name] - Save current conversation\n\
         :load [name] - Load a conversation\n\
         :list - List saved conversations\n\
         :stats/:diag - Toggle hardware diagnostics\n\
         :help/:h - Show this help\n\
         \n\
         Keys:\n\
         i - Enter insert mode (type messages)\n\
         Esc - Return to normal mode / Close diagnostics\n\
         : - Enter command mode\n\
         Tab - Toggle sidebar\n\
         F2 - Toggle hardware diagnostics\n\
         Ctrl+C - Quit"
            .to_string(),
    );
}
