use anyhow::Result;
use clap::{Parser, Subcommand};
use colored::Colorize;
use std::path::PathBuf;

use mermaid::{
    app::{init_config, load_config, Config},
    context::{ContextLoader, LoaderConfig},
    models::{ModelFactory, ProjectContext},
    tui::{run_ui, App},
};

#[derive(Parser, Debug)]
#[command(name = "mermaid")]
#[command(version = "0.1.0")]
#[command(about = "🧜‍♀️ An open-source, model-agnostic AI pair programmer", long_about = None)]
struct Cli {
    /// Model to use (e.g., ollama/codellama, openai/gpt-4, anthropic/claude-3)
    #[arg(short, long)]
    model: Option<String>,

    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,

    /// Project directory (defaults to current directory)
    #[arg(short, long)]
    path: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize configuration
    Init,
    /// List available models
    List,
    /// Start a chat session (default)
    Chat,
    /// Show version information
    Version,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Setup logging
    let log_level = if cli.verbose {
        "debug".to_string()
    } else {
        std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string())
    };

    tracing_subscriber::fmt()
        .with_env_filter(log_level)
        .init();

    // Handle commands
    match cli.command {
        Some(Commands::Init) => {
            println!("🧜‍♀️ Initializing Mermaid configuration...");
            init_config()?;
            println!("✓ Configuration initialized successfully!");
            return Ok(());
        }
        Some(Commands::List) => {
            println!("🧜‍♀️ Available models:");
            let models = ModelFactory::list_available().await?;
            for model in models {
                println!("  • {}", model.green());
            }
            return Ok(());
        }
        Some(Commands::Version) => {
            println!("🧜‍♀️ Mermaid v{}", env!("CARGO_PKG_VERSION"));
            println!("   An open-source, model-agnostic AI pair programmer");
            return Ok(());
        }
        Some(Commands::Chat) | None => {
            // Continue to chat interface
        }
    }

    // Load configuration
    let config = if let Some(config_path) = cli.config {
        let toml_str = std::fs::read_to_string(&config_path)?;
        toml::from_str::<Config>(&toml_str)?
    } else {
        match load_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("⚠️  Failed to load config: {}. Using defaults.", e);
                Config::default()
            }
        }
    };

    // Determine model to use
    let model_id = if let Some(model) = cli.model {
        model
    } else {
        format!("{}/{}", config.default_model.provider, config.default_model.name)
    };

    println!("🧜‍♀️ Starting Mermaid with model: {}", model_id.green());

    // Create model instance
    let model = match ModelFactory::create(&model_id).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("❌ Failed to initialize model: {}", e);
            eprintln!("   Make sure the model is available and properly configured.");
            std::process::exit(1);
        }
    };

    // Validate model connection
    if !model.validate_connection().await? {
        eprintln!("⚠️  Warning: Model connection could not be validated");
        eprintln!("   The model may not work correctly.");
    }

    // Load project context
    let project_path = cli.path.unwrap_or_else(|| PathBuf::from("."));
    println!("📂 Loading project context from: {}", project_path.display());

    let loader_config = LoaderConfig {
        max_file_size: config.context.max_file_size,
        max_files: config.context.max_files,
        max_context_tokens: config.context.max_context_tokens,
        priority_extensions: vec![],
        ignore_patterns: config.context.exclude_patterns.clone(),
    };

    let context_loader = ContextLoader::with_config(loader_config)?;
    let context = match context_loader.load_context(&project_path) {
        Ok(ctx) => {
            println!(
                "✓ Loaded {} files ({} tokens)",
                ctx.files.len(),
                ctx.token_count
            );
            ctx
        }
        Err(e) => {
            eprintln!("⚠️  Failed to load project context: {}", e);
            eprintln!("   Continuing without project context.");
            ProjectContext::new(project_path.to_string_lossy().to_string())
        }
    };

    // Create app instance
    let app = App::new(model, context);

    // Print welcome message
    println!();
    println!("🧜‍♀️ Welcome to Mermaid!");
    println!("   • Press 'i' to enter insert mode and type your message");
    println!("   • Press 'Enter' to send your message");
    println!("   • Press 'Esc' to return to normal mode");
    println!("   • Press ':' for commands (:help for list)");
    println!("   • Press 'Ctrl+C' to quit");
    println!();

    // Run the TUI
    run_ui(app).await?;

    println!("👋 Goodbye!");
    Ok(())
}
