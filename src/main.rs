mod nix;

use anyhow::ensure;
use clap::{Parser, Subcommand};
use nix_compute_protocol::model;

#[derive(Debug, Parser)]
#[command(
    name = "nix-compute",
    version,
    about = "Build and inspect reproducible compute tasks from locked Nix Flakes"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Inspect {
        selector: String,
        #[arg(long)]
        target: Option<String>,
    },
    Validate {
        selector: String,
        #[arg(long)]
        target: Option<String>,
    },
    Build {
        selector: String,
        #[arg(long)]
        target: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Inspect { selector, target } | Command::Validate { selector, target } => {
            let (selector, _) = nix::Selector::parse(&selector)?.snapshot()?;
            let job = selector.job()?;
            if let Some(name) = target {
                ensure!(job.targets.contains_key(&name), "unknown target {name}");
                println!(
                    "{}",
                    serde_json::to_string_pretty(&selector.target(&name)?)?
                );
            } else {
                println!("{}", serde_json::to_string_pretty(&job)?);
            }
        }
        Command::Build { selector, target } => {
            let (selector, _) = nix::Selector::parse(&selector)?.snapshot()?;
            let job = selector.job()?;
            let name = if let Some(name) = target {
                ensure!(job.targets.contains_key(&name), "unknown target {name}");
                name
            } else {
                ensure!(
                    job.targets.len() == 1,
                    "--target is required; declared targets: {}",
                    job.targets.keys().cloned().collect::<Vec<_>>().join(", ")
                );
                job.targets.keys().next().unwrap().clone()
            };
            selector.target(&name)?.validate()?;
            let path = selector.build(&format!("computeTasks.\"{}\".\"{name}\"", selector.job))?;
            model::Task::load(std::path::Path::new(&path))?;
            println!("{path}");
        }
    }
    Ok(())
}
