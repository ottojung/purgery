use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use purgery_core::{Nickname, RunId, ServerConfig};
use purgery_server::{begin_run, finish_run, process_once_raw, read_run_status, server_check};
use std::fs;

fn load_server_config(config_path: &str) -> Result<ServerConfig> {
    let config_content = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read server config: {config_path}"))?;
    ServerConfig::from_toml(&config_content).with_context(|| "failed to parse server config")
}

fn find_config() -> Result<String> {
    if let Ok(path) = std::env::var("PURGERY_CONFIG") {
        if !path.is_empty() {
            return Ok(path);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let user_path = format!("{home}/.config/purgery/server.toml");
    if fs::metadata(&user_path).is_ok() {
        return Ok(user_path);
    }
    let etc_path = "/etc/purgery/server.toml".to_string();
    if fs::metadata(&etc_path).is_ok() {
        return Ok(etc_path);
    }
    anyhow::bail!(
        "no server config found; use --config, $PURGERY_CONFIG, ~/.config/purgery/server.toml, or /etc/purgery/server.toml"
    )
}

#[derive(Parser)]
#[command(
    name = "purgery-server",
    about = "Purgery server: process staged uploads and move files to final storage",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    /// Path to server configuration TOML
    #[arg(long, global = true)]
    config: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Process one batch of ready runs and exit
    ProcessOnce,
    /// Begin a new run: create incoming directory and print paths
    BeginRun {
        #[arg(long)]
        nickname: String,
        #[arg(long)]
        run_id: String,
    },
    /// Finish a run: move from incoming to ready
    FinishRun {
        #[arg(long)]
        nickname: String,
        #[arg(long)]
        run_id: String,
    },
    /// Read run status from done or failed
    Status {
        #[arg(long)]
        nickname: String,
        #[arg(long)]
        run_id: String,
    },
    /// Check server configuration and dependencies
    Check,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let call_with_config = |f: &dyn Fn(&ServerConfig) -> Result<()>| -> Result<()> {
        let config_path = cli.config.as_deref().unwrap_or("");
        let path = if config_path.is_empty() {
            find_config()?
        } else {
            config_path.to_owned()
        };
        let server_config = load_server_config(&path)?;
        f(&server_config)
    };

    match cli.command {
        Command::ProcessOnce => {
            call_with_config(&|config| {
                server_check(config)?;
                process_once_raw(config)
            })?;
        }
        Command::BeginRun { nickname, run_id } => {
            let nickname = Nickname::new(nickname).with_context(|| "invalid nickname")?;
            let run_id = RunId::new(run_id).with_context(|| "invalid run ID")?;
            call_with_config(&|config| {
                let response = begin_run(config, &nickname, &run_id)?;
                print!("{response}");
                Ok(())
            })?;
        }
        Command::FinishRun { nickname, run_id } => {
            let nickname = Nickname::new(nickname).with_context(|| "invalid nickname")?;
            let run_id = RunId::new(run_id).with_context(|| "invalid run ID")?;
            call_with_config(&|config| finish_run(config, &nickname, &run_id))?;
        }
        Command::Status { nickname, run_id } => {
            let nickname = Nickname::new(nickname).with_context(|| "invalid nickname")?;
            let run_id = RunId::new(run_id).with_context(|| "invalid run ID")?;
            call_with_config(&|config| {
                let status = read_run_status(config, &nickname, &run_id)?;
                print!("{}", status.to_toml()?);
                Ok(())
            })?;
        }
        Command::Check => {
            call_with_config(&|config| server_check(config))?;
        }
    }

    Ok(())
}
