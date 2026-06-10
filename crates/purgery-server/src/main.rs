use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use purgery_core::{ColorMode, LogFormat, LogLevel, Nickname, RunId, ServerConfig};
use purgery_server::{
    begin_run, bootstrap, finish_run, heartbeat_run, prepare_run, process_once_raw,
    read_run_status, resolve_destinations, run_gc, server_check,
};
use std::fs;

fn load_server_config(config_path: &str) -> Result<ServerConfig> {
    let config_content = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read server config: {config_path}"))?;
    ServerConfig::from_toml(&config_content).with_context(|| "failed to parse server config")
}

fn find_config() -> Result<String> {
    if let Ok(path) = std::env::var("PURGERY_SERVER_CONFIG_PATH") {
        if !path.is_empty() {
            return Ok(path);
        }
    }
    if let Ok(xdg_home) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg_home.is_empty() {
            let xdg_path = format!("{xdg_home}/purgery/server.toml");
            if fs::metadata(&xdg_path).is_ok() {
                return Ok(xdg_path);
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            let user_path = format!("{home}/.config/purgery/server.toml");
            if fs::metadata(&user_path).is_ok() {
                return Ok(user_path);
            }
        }
    }
    let etc_path = "/etc/purgery/server.toml".to_string();
    if fs::metadata(&etc_path).is_ok() {
        return Ok(etc_path);
    }
    anyhow::bail!(
        "no server config found; use --config, $PURGERY_SERVER_CONFIG_PATH, \
         $XDG_CONFIG_HOME/purgery/server.toml, ~/.config/purgery/server.toml, \
         or /etc/purgery/server.toml"
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

    /// Log level override (error, warn, info, debug, trace)
    #[arg(long, global = true)]
    log_level: Option<String>,
    /// Log format override (pretty, compact, json)
    #[arg(long, global = true)]
    log_format: Option<String>,
    /// Color mode override (auto, always, never)
    #[arg(long, global = true)]
    color: Option<String>,
    /// Suppress all logs except errors (conflicts with --verbose and --log-level)
    #[arg(long, global = true, conflicts_with_all = &["verbose", "log_level"])]
    quiet: bool,
    /// Enable verbose (debug) logging
    #[arg(long, global = true, conflicts_with_all = &["quiet", "log_level"])]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Recover processing runs, process ready runs, and exit
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
    /// Check server configuration and dependencies (side-effect-free)
    Check,
    /// Bootstrap server directories (creates root and purgery_root)
    Bootstrap,
    /// Run garbage collection on expired incoming runs
    Gc,
    /// Validate run plan and return transfer destinations (passthrough + purgatory roots)
    PrepareRun {
        #[arg(long)]
        nickname: String,
        #[arg(long)]
        run_id: String,
    },
    /// Send heartbeat for an incoming run
    HeartbeatRun {
        #[arg(long)]
        nickname: String,
        #[arg(long)]
        run_id: String,
    },
    /// Resolve final storage destinations for pure passthrough groups (side-effect-free)
    /// Reads run config from stdin.
    ResolveDestinations {
        #[arg(long)]
        nickname: String,
    },
}

/// Apply CLI logging overrides on top of a base config.
fn apply_cli_overrides(log_cfg: &mut purgery_core::LoggingConfig, cli: &Cli) -> Result<()> {
    if cli.quiet {
        log_cfg.level = LogLevel::Error;
    }
    if cli.verbose {
        log_cfg.level = LogLevel::Debug;
    }
    if let Some(ref level) = cli.log_level {
        log_cfg.level = level
            .parse::<LogLevel>()
            .map_err(|e| anyhow::anyhow!("invalid log level: {e}"))?;
    }
    if let Some(ref fmt) = cli.log_format {
        log_cfg.format = fmt
            .parse::<LogFormat>()
            .map_err(|e| anyhow::anyhow!("invalid log format: {e}"))?;
    }
    if let Some(ref color) = cli.color {
        log_cfg.color = color
            .parse::<ColorMode>()
            .map_err(|e| anyhow::anyhow!("invalid color mode: {e}"))?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve config path before dispatch so we can load config once.
    let config_path = cli.config.as_deref().unwrap_or("");
    let path = if config_path.is_empty() {
        find_config()?
    } else {
        config_path.to_owned()
    };
    let server_config = load_server_config(&path)?;

    // Merge logging: config defaults + CLI overrides, then init.
    let mut log_cfg = server_config.logging.clone();
    apply_cli_overrides(&mut log_cfg, &cli)?;
    purgery_core::init_logging(&log_cfg)
        .map_err(|e| anyhow::anyhow!("failed to initialize logging: {e}"))?;

    match cli.command {
        Command::ProcessOnce => {
            server_check(&server_config)?;
            process_once_raw(&server_config)?;
        }
        Command::BeginRun { nickname, run_id } => {
            let nickname = Nickname::new(nickname).with_context(|| "invalid nickname")?;
            let run_id = RunId::new(run_id).with_context(|| "invalid run ID")?;
            let response = begin_run(&server_config, &nickname, &run_id)?;
            print!("{response}");
        }
        Command::FinishRun { nickname, run_id } => {
            let nickname = Nickname::new(nickname).with_context(|| "invalid nickname")?;
            let run_id = RunId::new(run_id).with_context(|| "invalid run ID")?;
            finish_run(&server_config, &nickname, &run_id)?;
        }
        Command::Status { nickname, run_id } => {
            let nickname = Nickname::new(nickname).with_context(|| "invalid nickname")?;
            let run_id = RunId::new(run_id).with_context(|| "invalid run ID")?;
            let status = read_run_status(&server_config, &nickname, &run_id)?;
            print!("{}", status.to_toml()?);
        }
        Command::Check => {
            server_check(&server_config)?;
        }
        Command::Bootstrap => {
            bootstrap(&server_config)?;
        }
        Command::Gc => {
            run_gc(&server_config)?;
        }
        Command::PrepareRun { nickname, run_id } => {
            let nickname = Nickname::new(nickname).with_context(|| "invalid nickname")?;
            let run_id = RunId::new(run_id).with_context(|| "invalid run ID")?;
            let response = prepare_run(&server_config, &nickname, &run_id)?;
            print!("{response}");
        }
        Command::HeartbeatRun { nickname, run_id } => {
            let nickname = Nickname::new(nickname).with_context(|| "invalid nickname")?;
            let run_id = RunId::new(run_id).with_context(|| "invalid run ID")?;
            heartbeat_run(&server_config, &nickname, &run_id)?;
        }
        Command::ResolveDestinations { nickname } => {
            let nickname = Nickname::new(nickname).with_context(|| "invalid nickname")?;
            use std::io::Read;
            let mut stdin_content = String::new();
            std::io::stdin()
                .read_to_string(&mut stdin_content)
                .with_context(|| "failed to read stdin")?;
            let run_config = purgery_core::RunConfig::from_toml(&stdin_content)
                .with_context(|| "failed to parse run config")?;
            let response = resolve_destinations(&server_config, &nickname, &run_config)?;
            print!("{response}");
        }
    }

    Ok(())
}
