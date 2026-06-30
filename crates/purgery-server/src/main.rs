use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use purgery_core::{
    ColorMode, LogFormat, LogLevel, Nickname, ProtocolErrorResponse, RunId, ServerConfig,
};
use purgery_server::{
    begin_run, bootstrap, finish_run, heartbeat_run, prepare_run, process_once_raw,
    process_run_target, read_run_status, run_gc, run_state, server_check, version_response,
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
    about = "Purgery server: processes uploaded files through transform pipelines and reports run status",
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
    /// Print protocol and package version information
    Version,
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
    /// Bootstrap server internal directories under work_dir
    Bootstrap,
    /// Collect expired server work state across all request phases
    Gc,
    /// Validate an incoming transform run plan
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
    /// Report run phase without requiring terminal status
    RunState {
        #[arg(long)]
        nickname: String,
        #[arg(long)]
        run_id: String,
    },
    /// Claim/process the target run, recover if abandoned, or no-op if actively processed or terminal (long-running)
    ProcessRun {
        #[arg(long)]
        nickname: String,
        #[arg(long)]
        run_id: String,
    },
}

/// Returns `true` when the subcommand is a client-driven protocol command
/// that should emit machine-readable error envelopes on failure.
fn is_protocol_command(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::BeginRun { .. }
            | Command::PrepareRun { .. }
            | Command::FinishRun { .. }
            | Command::HeartbeatRun { .. }
            | Command::RunState { .. }
            | Command::Status { .. }
            | Command::ProcessRun { .. }
    )
}

/// Return the wire-format name of a subcommand for protocol error envelopes.
fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::BeginRun { .. } => "begin-run",
        Command::PrepareRun { .. } => "prepare-run",
        Command::FinishRun { .. } => "finish-run",
        Command::HeartbeatRun { .. } => "heartbeat-run",
        Command::RunState { .. } => "run-state",
        Command::Status { .. } => "status",
        Command::ProcessRun { .. } => "process-run",
        Command::Version => "version",
        Command::ProcessOnce => "process-once",
        Command::Check => "check",
        Command::Bootstrap => "bootstrap",
        Command::Gc => "gc",
    }
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

/// Print a machine-readable protocol error envelope to stdout and
/// exit with code 1.  The client parses this instead of scraping stderr.
fn exit_with_protocol_error(command: &str, err: anyhow::Error, code: &str) -> ! {
    let envelope = ProtocolErrorResponse {
        protocol_version: purgery_core::PROTOCOL_VERSION,
        purgery_version: purgery_core::current_purgery_version().to_string(),
        command: command.to_owned(),
        ok: false,
        error: purgery_core::ProtocolErrorDetail {
            code: code.to_owned(),
            message: format!("{err:#}"),
        },
    };
    if let Ok(toml_str) = toml::to_string(&envelope) {
        print!("{toml_str}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
    std::process::exit(1);
}

/// Run a protocol command closure; on error, emit the machine-readable
/// error envelope and exit instead of letting `?` propagate.
fn run_protocol(command: &str, code: &str, f: impl FnOnce() -> Result<()>) {
    if let Err(e) = f() {
        exit_with_protocol_error(command, e, code);
    }
}

/// Attempt to discover, load, and prepare the server config.
/// Returns `(ServerConfig, LoggingConfig)` on success; returns an
/// `anyhow::Error` on any failure (config not found, unreadable,
/// parse error, CLI override parse failure).
fn load_config_and_logging(cli: &Cli) -> Result<(ServerConfig, purgery_core::LoggingConfig)> {
    let config_path = cli.config.as_deref().unwrap_or("");
    let path = if config_path.is_empty() {
        find_config()?
    } else {
        config_path.to_owned()
    };
    let server_config = load_server_config(&path)?;
    let mut log_cfg = server_config.logging.clone();
    apply_cli_overrides(&mut log_cfg, cli)?;
    Ok((server_config, log_cfg))
}

/// Return the domain error code for a protocol command
/// (excluding `invalid_request` which is handled by the
/// `validate_nickname_and_run_id` helper).
fn protocol_error_code(command: &str) -> &'static str {
    match command {
        "prepare-run" => "run_plan_invalid",
        _ => "server_error",
    }
}

/// Parse a nickname and run ID pair, exiting with a protocol error
/// envelope under `invalid_request` if either is malformed.
fn validate_nickname_and_run_id(
    command: &str,
    nickname_str: &str,
    run_id_str: &str,
) -> (Nickname, RunId) {
    let nickname = match Nickname::new(nickname_str.to_owned()) {
        Ok(n) => n,
        Err(e) => exit_with_protocol_error(
            command,
            anyhow::anyhow!("invalid nickname: {e}"),
            "invalid_request",
        ),
    };
    let run_id = match RunId::new(run_id_str.to_owned()) {
        Ok(r) => r,
        Err(e) => exit_with_protocol_error(
            command,
            anyhow::anyhow!("invalid run ID: {e}"),
            "invalid_request",
        ),
    };
    (nickname, run_id)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // The `version` subcommand does not require config loading.
    if matches!(cli.command, Command::Version) {
        print!("{}", version_response());
        return Ok(());
    }

    let cmd_name = command_name(&cli.command);
    let is_protocol = is_protocol_command(&cli.command);

    // Attempt config loading.  For protocol commands, failures emit a
    // machine-readable error envelope so the client does not need to
    // scrape stderr for Purgery-level error messages.
    let (server_config, log_cfg) = match load_config_and_logging(&cli) {
        Ok(pair) => pair,
        Err(e) => {
            if is_protocol {
                exit_with_protocol_error(cmd_name, e, "server_config_invalid");
            }
            return Err(e);
        }
    };

    // Logging init failure is always a hard error, but for protocol
    // commands we still emit the envelope first.
    if let Err(e) = purgery_core::init_logging(&log_cfg) {
        let err = anyhow::anyhow!("failed to initialize logging: {e}");
        if is_protocol {
            exit_with_protocol_error(cmd_name, err, "server_config_invalid");
        }
        return Err(err);
    }

    match cli.command {
        Command::Version => {
            unreachable!("Version is handled before config loading")
        }
        Command::ProcessOnce => {
            server_check(&server_config)?;
            process_once_raw(&server_config)?;
        }
        Command::BeginRun {
            ref nickname,
            ref run_id,
        } => {
            let (nickname, run_id) = validate_nickname_and_run_id("begin-run", nickname, run_id);
            run_protocol("begin-run", "server_error", || {
                let response = begin_run(&server_config, &nickname, &run_id)?;
                print!("{response}");
                Ok(())
            });
        }
        Command::FinishRun {
            ref nickname,
            ref run_id,
        } => {
            let (nickname, run_id) = validate_nickname_and_run_id("finish-run", nickname, run_id);
            run_protocol("finish-run", "server_error", || {
                finish_run(&server_config, &nickname, &run_id)?;
                Ok(())
            });
        }
        Command::Status {
            ref nickname,
            ref run_id,
        } => {
            let (nickname, run_id) = validate_nickname_and_run_id("status", nickname, run_id);
            run_protocol("status", "server_error", || {
                let status = read_run_status(&server_config, &nickname, &run_id)?;
                print!("{}", status.to_toml()?);
                Ok(())
            });
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
        Command::PrepareRun {
            ref nickname,
            ref run_id,
        } => {
            let (nickname, run_id) = validate_nickname_and_run_id("prepare-run", nickname, run_id);
            run_protocol("prepare-run", protocol_error_code("prepare-run"), || {
                let response = prepare_run(&server_config, &nickname, &run_id)?;
                print!("{response}");
                Ok(())
            });
        }
        Command::HeartbeatRun {
            ref nickname,
            ref run_id,
        } => {
            let (nickname, run_id) =
                validate_nickname_and_run_id("heartbeat-run", nickname, run_id);
            run_protocol("heartbeat-run", "server_error", || {
                heartbeat_run(&server_config, &nickname, &run_id)?;
                Ok(())
            });
        }
        Command::RunState {
            ref nickname,
            ref run_id,
        } => {
            let (nickname, run_id) = validate_nickname_and_run_id("run-state", nickname, run_id);
            run_protocol("run-state", "server_error", || {
                let response = run_state(&server_config, &nickname, &run_id)?;
                print!(
                    "{}",
                    toml::to_string(&response).with_context(|| "failed to serialize run state")?
                );
                Ok(())
            });
        }
        Command::ProcessRun {
            ref nickname,
            ref run_id,
        } => {
            let (nickname, run_id) = validate_nickname_and_run_id("process-run", nickname, run_id);
            run_protocol("process-run", "server_error", || {
                server_check(&server_config)?;
                let response = process_run_target(&server_config, &nickname, &run_id)?;
                println!(
                    "{}",
                    toml::to_string(&response)
                        .with_context(|| "failed to serialize process-run response")?
                );
                Ok(())
            });
        }
    }
    Ok(())
}
