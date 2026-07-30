use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aj_apply_patch_eval::analysis::{analyze_records, render_markdown};
use aj_apply_patch_eval::docker::{
    copy_worker, fixture_worker, probe_worker, snapshot_worker, tool_worker, verify_worker,
};
use aj_apply_patch_eval::pilot_analysis::{analyze_pilot_records, render_pilot_markdown};
use aj_apply_patch_eval::planning::plan_main;
use aj_apply_patch_eval::runner::{RunOptions, freeze_model_selection, run, run_preflight};
use aj_apply_patch_eval::schedule::{FrozenPlan, SchedulePhase, freeze_plan};
use aj_apply_patch_eval::suite::committed_manifest;
use aj_apply_patch_eval::worker::run_worker;
use aj_conf::Config;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "aj-apply-patch-eval")]
#[command(about = "Run, freeze, and analyze the apply-patch description evaluation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate the immutable image and mandatory isolation probes.
    Preflight {
        #[arg(long)]
        image: String,
    },
    /// Run adjacent trial pairs from one frozen phase.
    Run {
        #[arg(long, value_enum)]
        phase: Phase,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        records: PathBuf,
        #[arg(long)]
        artifact_dir: PathBuf,
        #[arg(long)]
        image: String,
        #[arg(long)]
        max_cost_usd: f64,
        #[arg(long)]
        max_trials: u64,
        #[arg(long)]
        timeout_seconds: u64,
        #[arg(long)]
        max_model_responses: u32,
    },
    /// Freeze a deterministic task universe and non-live schedule.
    Freeze {
        #[arg(long)]
        seed: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        universe_per_archetype: u32,
        #[arg(long, default_value = "openai-codex")]
        provider: String,
        #[arg(long, default_value = "gpt-5.6-sol")]
        model: String,
        #[arg(long, default_value = "low")]
        reasoning: String,
    },
    /// Blind the complete pilot and freeze the confirmatory main schedule.
    PlanMain {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        records: PathBuf,
        #[arg(long)]
        output_plan: PathBuf,
        #[arg(long)]
        output_report: PathBuf,
    },
    /// Analyze the excluded pilot descriptively after main planning is frozen.
    AnalyzePilot {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        records: PathBuf,
        #[arg(long)]
        output_json: PathBuf,
        #[arg(long)]
        output_markdown: PathBuf,
    },
    /// Analyze exactly the frozen confirmatory main schedule.
    Analyze {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        records: PathBuf,
        #[arg(long)]
        output_json: PathBuf,
        #[arg(long)]
        output_markdown: PathBuf,
    },
    #[command(name = "__worker", hide = true)]
    Worker,
    #[command(name = "__tool-worker", hide = true)]
    ToolWorker,
    #[command(name = "__fixture-worker", hide = true)]
    FixtureWorker,
    #[command(name = "__snapshot-worker", hide = true)]
    SnapshotWorker,
    #[command(name = "__verify-worker", hide = true)]
    VerifyWorker,
    #[command(name = "__copy-worker", hide = true)]
    CopyWorker,
    #[command(name = "__probe-worker", hide = true)]
    ProbeWorker,
    #[command(name = "__volume-keeper", hide = true)]
    VolumeKeeper,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Phase {
    Smoke,
    Pilot,
    Main,
}

impl From<Phase> for SchedulePhase {
    fn from(phase: Phase) -> Self {
        match phase {
            Phase::Smoke => Self::Smoke,
            Phase::Pilot => Self::Pilot,
            Phase::Main => Self::Main,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    match Cli::parse().command {
        Command::Preflight { image } => run_preflight(&image).await?,
        Command::Run {
            phase,
            plan,
            records,
            artifact_dir,
            image,
            max_cost_usd,
            max_trials,
            timeout_seconds,
            max_model_responses,
        } => {
            load_parent_environment();
            run(RunOptions {
                phase: phase.into(),
                plan,
                records,
                artifact_dir,
                image,
                max_cost_usd,
                max_trials,
                timeout: Duration::from_secs(timeout_seconds),
                max_model_responses,
            })
            .await?;
        }
        Command::Freeze {
            seed,
            output,
            universe_per_archetype,
            provider,
            model,
            reasoning,
        } => {
            let manifest = committed_manifest()?;
            let model = freeze_model_selection(&provider, &model, &reasoning)?;
            let plan = freeze_plan(&manifest, &seed, universe_per_archetype, model)?;
            write(&output, &serde_json::to_vec_pretty(&plan)?)?;
        }
        Command::PlanMain {
            plan,
            records,
            output_plan,
            output_report,
        } => {
            let unplanned = read_plan(&plan)?;
            let outcome = plan_main(&unplanned, &records)?;
            write(&output_report, &serde_json::to_vec_pretty(&outcome.report)?)?;
            let planned = outcome.planned_plan.ok_or_else(|| {
                std::io::Error::other(
                    "planner is inconclusive because the frozen universe is insufficient",
                )
            })?;
            write(&output_plan, &serde_json::to_vec_pretty(&planned)?)?;
        }
        Command::AnalyzePilot {
            plan,
            records,
            output_json,
            output_markdown,
        } => {
            let plan = read_plan(&plan)?;
            let report = analyze_pilot_records(&plan, &records)?;
            write(&output_json, &serde_json::to_vec_pretty(&report)?)?;
            write(&output_markdown, render_pilot_markdown(&report).as_bytes())?;
        }
        Command::Analyze {
            plan,
            records,
            output_json,
            output_markdown,
        } => {
            let plan = read_plan(&plan)?;
            let report = analyze_records(&plan, &records)?;
            write(&output_json, &serde_json::to_vec_pretty(&report)?)?;
            write(&output_markdown, render_markdown(&report).as_bytes())?;
        }
        Command::Worker => run_worker().await?,
        Command::ToolWorker => tool_worker().await?,
        Command::FixtureWorker => fixture_worker().await?,
        Command::SnapshotWorker => snapshot_worker().await?,
        Command::VerifyWorker => verify_worker().await?,
        Command::CopyWorker => copy_worker().await?,
        Command::ProbeWorker => probe_worker().await?,
        Command::VolumeKeeper => std::future::pending::<()>().await,
    }
    Ok(())
}

fn load_parent_environment() {
    if let Ok(path) = Config::get_dotenv_file_path() {
        dotenv::from_path(path).ok();
    }
    dotenv::dotenv().ok();
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn read_plan(path: &Path) -> Result<FrozenPlan, Box<dyn Error>> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
