#[cfg(not(unix))]
compile_error!("Purgery is Unix-only — it requires rsync, SSH, and Unix filesystem semantics");

use anyhow::Result;
use clap::Parser;

mod classify;
mod cleanup;
mod run;
mod ssh;
mod transfer;

#[derive(Parser)]
#[command(
    name = "purgery-client",
    about = "Purgery client: rsync-style one-shot import",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Parser)]
enum Command {
    /// Sync a local source tree to a remote server
    Sync(SyncArgs),
}

#[derive(Parser)]
struct SyncArgs {
    /// Postprocess step to run on the server (repeatable)
    #[arg(long = "postprocess", short = 'p')]
    postprocess: Vec<String>,

    /// Delete source files after successful import (required with --postprocess)
    #[arg(long)]
    delete_after_import: bool,

    /// Directory for client state files (default: XDG_STATE_HOME/purgery)
    #[arg(long)]
    state_dir: Option<String>,

    /// Command to invoke on the remote server (default: purgery-server)
    #[arg(long, default_value = "purgery-server")]
    server_command: String,

    /// Local source path
    #[arg(allow_hyphen_values = true)]
    source: String,

    /// Destination in rsync style: USER@HOST:DESTINATION
    #[arg(allow_hyphen_values = true)]
    destination: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Sync(args) => {
            run::run_sync(&args)?;
        }
    }
    Ok(())
}
