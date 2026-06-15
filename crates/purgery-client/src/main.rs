#[cfg(not(unix))]
compile_error!("Purgery is Unix-only — it requires rsync, SSH, and Unix filesystem semantics");

use anyhow::Result;
use clap::Parser;
use purgery_core::{ColorMode, LogFormat, LogLevel, LoggingConfig};

mod classify;
mod cleanup;
mod run;
mod runner;
mod split;

#[derive(Parser)]
#[command(
    name = "purgery-client",
    about = "Purgery client: rsync-style one-shot import",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    /// Quiet: set log level to error (conflicts with --verbose and --log-level)
    #[arg(long, global = true, conflicts_with_all = &["verbose", "log_level"])]
    quiet: bool,

    /// Verbose: set log level to debug (conflicts with --quiet and --log-level)
    #[arg(long, global = true, conflicts_with_all = &["quiet", "log_level"])]
    verbose: bool,

    /// Log level: error, warn, info, debug, trace (conflicts with --quiet and --verbose)
    #[arg(long, global = true, value_enum, conflicts_with_all = &["quiet", "verbose"])]
    log_level: Option<LogLevelArg>,

    /// Log format: pretty, compact, json
    #[arg(long, global = true, value_enum)]
    log_format: Option<LogFormatArg>,

    /// Color mode: auto, always, never
    #[arg(long, global = true, value_enum)]
    color: Option<ColorModeArg>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Parser)]
enum Command {
    /// Sync a local source entry to a remote destination
    Sync(SyncArgs),
}

#[derive(Parser)]
struct SyncArgs {
    /// Transform to run on the server
    #[arg(long = "transform", short = 'p')]
    transform: Option<String>,

    /// Delete source files after successful import (required with --transform)
    #[arg(long)]
    delete_after_import: bool,

    /// Split pattern using rsync-style syntax: select matching source entries
    /// and process each individually
    #[arg(long)]
    split: Option<String>,

    /// Directory for client state files (default: XDG_STATE_HOME/purgery)
    #[arg(long)]
    state_dir: Option<String>,

    /// Command to invoke on the remote server (default: purgery-server)
    #[arg(long, default_value = "purgery-server")]
    server_command: String,

    /// Local source path (file, directory, or symlink)
    #[arg(allow_hyphen_values = true)]
    source: String,

    /// Destination in rsync style: USER@HOST:DESTINATION
    #[arg(allow_hyphen_values = true)]
    destination: String,
}

#[derive(clap::ValueEnum, Clone)]
enum LogLevelArg {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<LogLevelArg> for LogLevel {
    fn from(arg: LogLevelArg) -> Self {
        match arg {
            LogLevelArg::Error => LogLevel::Error,
            LogLevelArg::Warn => LogLevel::Warn,
            LogLevelArg::Info => LogLevel::Info,
            LogLevelArg::Debug => LogLevel::Debug,
            LogLevelArg::Trace => LogLevel::Trace,
        }
    }
}

#[derive(clap::ValueEnum, Clone)]
enum LogFormatArg {
    Pretty,
    Compact,
    Json,
}

impl From<LogFormatArg> for LogFormat {
    fn from(arg: LogFormatArg) -> Self {
        match arg {
            LogFormatArg::Pretty => LogFormat::Pretty,
            LogFormatArg::Compact => LogFormat::Compact,
            LogFormatArg::Json => LogFormat::Json,
        }
    }
}

#[derive(clap::ValueEnum, Clone)]
enum ColorModeArg {
    Auto,
    Always,
    Never,
}

impl From<ColorModeArg> for ColorMode {
    fn from(arg: ColorModeArg) -> Self {
        match arg {
            ColorModeArg::Auto => ColorMode::Auto,
            ColorModeArg::Always => ColorMode::Always,
            ColorModeArg::Never => ColorMode::Never,
        }
    }
}

fn build_logging_config(cli: &Cli) -> LoggingConfig {
    let level = if cli.quiet {
        LogLevel::Error
    } else if cli.verbose {
        LogLevel::Debug
    } else {
        cli.log_level
            .clone()
            .map(Into::into)
            .unwrap_or(LogLevel::Info)
    };
    let format = cli.log_format.clone().map(Into::into).unwrap_or_default();
    let color = cli.color.clone().map(Into::into).unwrap_or_default();
    LoggingConfig {
        level,
        format,
        color,
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let logging_config = build_logging_config(&cli);
    purgery_core::init_logging(&logging_config)
        .map_err(|e| anyhow::anyhow!("failed to initialize logging: {e}"))?;

    match cli.command {
        Command::Sync(args) => {
            run::run_sync(&args)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_verbose_before_subcommand() {
        let args = Cli::try_parse_from([
            "purgery-client",
            "--verbose",
            "sync",
            "--",
            "/src",
            "host:dest",
        ])
        .unwrap();
        let cfg = build_logging_config(&args);
        assert!(matches!(cfg.level, LogLevel::Debug));
    }

    #[test]
    fn cli_verbose_after_subcommand() {
        let args = Cli::try_parse_from([
            "purgery-client",
            "sync",
            "--verbose",
            "--",
            "/src",
            "host:dest",
        ])
        .unwrap();
        let cfg = build_logging_config(&args);
        assert!(matches!(cfg.level, LogLevel::Debug));
    }

    #[test]
    fn cli_parses_sync_basic() {
        let args = Cli::try_parse_from([
            "purgery-client",
            "sync",
            "--",
            "/home/user/Videos",
            "user@host:/dest",
        ])
        .unwrap();
        match args.command {
            Command::Sync(s) => {
                assert_eq!(s.source, "/home/user/Videos");
                assert_eq!(s.destination, "user@host:/dest");
                assert!(!s.delete_after_import);
                assert!(s.transform.is_none());
            }
        }
    }

    #[test]
    fn cli_parses_sync_with_transform() {
        let args = Cli::try_parse_from([
            "purgery-client",
            "sync",
            "--transform",
            "compress",
            "--delete-after-import",
            "--",
            "/src",
            "host:dest",
        ])
        .unwrap();
        match args.command {
            Command::Sync(s) => {
                assert_eq!(s.transform, Some("compress".into()));
                assert!(s.delete_after_import);
            }
        }
    }

    #[test]
    fn cli_parses_logging_flags() {
        let args = Cli::try_parse_from([
            "purgery-client",
            "--log-level",
            "debug",
            "--log-format",
            "json",
            "--color",
            "never",
            "sync",
            "--",
            "/src",
            "host:dest",
        ])
        .unwrap();
        assert!(args.log_level.is_some());
        let cfg = build_logging_config(&args);
        assert!(matches!(cfg.level, LogLevel::Debug));
        assert!(matches!(cfg.format, LogFormat::Json));
        assert!(matches!(cfg.color, ColorMode::Never));
    }

    #[test]
    fn cli_quiet_conflicts_with_verbose() {
        let result = Cli::try_parse_from([
            "purgery-client",
            "--quiet",
            "--verbose",
            "sync",
            "--",
            "/src",
            "host:dest",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_quiet_conflicts_with_log_level() {
        let result = Cli::try_parse_from([
            "purgery-client",
            "--quiet",
            "--log-level",
            "debug",
            "sync",
            "--",
            "/src",
            "host:dest",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_verbose_conflicts_with_log_level() {
        let result = Cli::try_parse_from([
            "purgery-client",
            "--verbose",
            "--log-level",
            "warn",
            "sync",
            "--",
            "/src",
            "host:dest",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_quiet_sets_error_level() {
        let args = Cli::try_parse_from([
            "purgery-client",
            "--quiet",
            "sync",
            "--",
            "/src",
            "host:dest",
        ])
        .unwrap();
        let cfg = build_logging_config(&args);
        assert!(matches!(cfg.level, LogLevel::Error));
    }

    #[test]
    fn cli_verbose_sets_debug_level() {
        let args = Cli::try_parse_from([
            "purgery-client",
            "--verbose",
            "sync",
            "--",
            "/src",
            "host:dest",
        ])
        .unwrap();
        let cfg = build_logging_config(&args);
        assert!(matches!(cfg.level, LogLevel::Debug));
    }

    #[test]
    fn cli_default_log_level_is_info() {
        let args =
            Cli::try_parse_from(["purgery-client", "sync", "--", "/src", "host:dest"]).unwrap();
        let cfg = build_logging_config(&args);
        assert!(matches!(cfg.level, LogLevel::Info));
    }

    #[test]
    fn cli_rejects_multiple_transform_flags() {
        let result = Cli::try_parse_from([
            "purgery-client",
            "sync",
            "--transform",
            "a",
            "--transform",
            "b",
            "--delete-after-import",
            "--",
            "/src",
            "host:dest",
        ]);

        assert!(result.is_err());
    }
}
