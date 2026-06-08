use clap::{Parser, Subcommand};

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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::ProcessOnce { config } => {
            eprintln!("not yet implemented: purgery-server process-once --config {config}");
            Ok(())
        }
    }
}
