use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use aj_apply_patch_eval::analysis::{analyze_records, render_markdown};
use aj_apply_patch_eval::schedule::freeze_plan;
use aj_apply_patch_eval::suite::committed_manifest;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "aj-apply-patch-eval")]
#[command(about = "Freeze and analyze the apply-patch description evaluation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Freeze a deterministic task universe and non-live schedule.
    Freeze {
        #[arg(long)]
        seed: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        universe_per_archetype: u32,
    },
    /// Analyze complete pairs from an append-only record stream.
    Analyze {
        #[arg(long)]
        records: PathBuf,
        #[arg(long)]
        output_json: PathBuf,
        #[arg(long)]
        output_markdown: PathBuf,
    },
}

fn main() -> Result<(), Box<dyn Error>> {
    match Cli::parse().command {
        Command::Freeze {
            seed,
            output,
            universe_per_archetype,
        } => {
            let manifest = committed_manifest()?;
            let plan = freeze_plan(&manifest, &seed, universe_per_archetype)?;
            write(&output, &serde_json::to_vec_pretty(&plan)?)?;
        }
        Command::Analyze {
            records,
            output_json,
            output_markdown,
        } => {
            let report = analyze_records(&records)?;
            write(&output_json, &serde_json::to_vec_pretty(&report)?)?;
            write(&output_markdown, render_markdown(&report).as_bytes())?;
        }
    }
    Ok(())
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}
