use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use purgery_core::ServerConfig;
use purgery_server::process_once_raw;
use std::fs;

#[derive(Parser)]
#[command(
    name = "purgery-server",
    about = "Purgery server: process staged uploads and move files to final storage",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Process one batch of ready runs and exit
    ProcessOnce {
        /// Path to server configuration TOML
        #[arg(long)]
        config: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::ProcessOnce { config } => {
            let config_content = fs::read_to_string(&config)
                .with_context(|| format!("failed to read server config: {config}"))?;
            let server_config = ServerConfig::from_toml(&config_content)
                .with_context(|| "failed to parse server config")?;
            process_once_raw(&server_config)?;
        }
    }
    Ok(())
}
