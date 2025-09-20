use anyhow::Result;
use clap::Parser;

use mermaid::{cli::Cli, runtime::Orchestrator};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Set up logging if verbose
    if cli.verbose {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .init();
    }

    // Create and run the orchestrator
    let orchestrator = Orchestrator::new(cli)?;
    orchestrator.run().await
}