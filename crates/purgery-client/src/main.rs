use clap::{Parser, Subcommand};

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

#[derive(Subcommand)]
enum Command {
    /// Sync local files to the server, process, and clean up confirmed imports
    SyncAndCleanup {
        /// Path to client configuration TOML
        #[arg(long)]
        config: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::SyncAndCleanup { config } => {
            eprintln!("not yet implemented: purgery-client sync-and-cleanup --config {config}");
            Ok(())
        }
    }
}
