mod compress;
mod pack;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "onelf", about = "Single-binary packaging tool", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Pack {
        directory: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        command: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Pack {
            directory,
            output,
            command,
        } => {
            println!("Packing {} -> {}", directory.display(), output.display());
            println!("Command: {}", command);
        }
    }
}
