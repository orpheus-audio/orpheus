//! `orpheusctl`: command-line client for any Orpheus node.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Base URL of the node to talk to.
    #[arg(long, default_value = "http://localhost:7480", env = "ORPHEUS_NODE")]
    node: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the node's ID, name and version.
    Status,
}

#[derive(Deserialize)]
struct Status {
    id: String,
    name: String,
    version: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let base = cli.node.trim_end_matches('/');
    match cli.command {
        Command::Status => {
            let url = format!("{base}/x-orpheus/v1/status");
            let s: Status = ureq::get(&url)
                .call()
                .and_then(|mut r| r.body_mut().read_json())
                .with_context(|| format!("querying {url}"))?;
            println!("{}  {}  (orpheusd {})", s.id, s.name, s.version);
        }
    }
    Ok(())
}
