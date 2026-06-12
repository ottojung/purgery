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
    about = "Purgery client: sync files to server and clean up imported files",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Parser)]
enum Command {
    Sync(SyncArgs),
}

#[derive(Parser)]
struct SyncArgs {
    #[arg(
        long,
        help = "Comma-separated list of postprocess steps to apply on the server"
    )]
    postprocess: Option<String>,

    #[arg(long, help = "Delete source files after successful import")]
    delete_after_import: bool,

    #[arg(
        long,
        default_value = "/tmp/purgery-client",
        help = "Directory for client state files"
    )]
    state_dir: String,

    source: String,

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
