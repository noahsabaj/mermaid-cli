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
        Some("refresh") | Some("r") => handle_refresh(app).await,
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

/// Refresh command - no longer needed as LLM explores via tools
async fn handle_refresh(app: &mut App) {
    app.set_status("Context refresh not needed - LLM explores codebase via tools");
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

/// Show help message
fn handle_help(app: &mut App) {
    app.add_message(
        MessageRole::System,
        "COMMANDS:\n\
         :quit/:q - Quit the application\n\
         :clear - Clear chat history\n\
         :model [name] - Switch model or show current\n\
         :cloud-setup - Configure Ollama Cloud API key\n\
         :refresh/:r - Refresh file context from disk\n\
         :save [name] - Save current conversation\n\
         :load [name] - Load a conversation\n\
         :list - List saved conversations\n\
         :help/:h - Show this help\n\
         \n\
         OPERATION MODES (Shift+Tab to cycle):\n\
         Normal - Confirms all operations (default)\n\
         Accept Edits - Auto-accepts file edits only\n\
         Plan Mode - Preview actions without execution\n\
         Bypass All - Auto-accepts everything (use with caution)\n\
         \n\
         INPUT & NAVIGATION:\n\
         Enter - Submit message or execute command\n\
         Esc - Cancel generation/plan or clear input\n\
         Up/Down - Navigate input history or scroll chat\n\
         Left/Right - Move cursor in input\n\
         Home/End - Jump to start/end of input\n\
         Page Up/Down - Scroll chat\n\
         Mouse Wheel - Scroll chat\n\
         \n\
         ACTION CONFIRMATION (when prompted):\n\
         Alt+Y - Approve action\n\
         Alt+N - Reject action\n\
         Alt+A - Always approve similar actions\n\
         Alt+P - Toggle action preview\n\
         \n\
         PLAN APPROVAL (when plan is ready):\n\
         Y - Approve and execute plan\n\
         N - Cancel plan\n\
         \n\
         OTHER:\n\
         Ctrl+C - Quit application"
            .to_string(),
    );
}
