use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aj_apply_patch_eval::analysis::{analyze_records, render_markdown};
use aj_apply_patch_eval::docker::{
    copy_worker, fixture_worker, probe_worker, snapshot_worker, tool_worker, verify_worker,
};
use aj_apply_patch_eval::pilot_analysis::{analyze_pilot_records, render_pilot_markdown};
use aj_apply_patch_eval::planning::{PlanningReport, plan_main};
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
        planning_report: PathBuf,
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
            let outputs = validate_output_paths(
                &[plan.as_path(), records.as_path()],
                &[output_plan.as_path(), output_report.as_path()],
            )?;
            let output_plan = &outputs[0];
            let output_report = &outputs[1];
            let unplanned = read_plan(&plan)?;
            let outcome = plan_main(&unplanned, &records)?;
            let report_bytes = serde_json::to_vec_pretty(&outcome.report)?;
            write(output_report, &report_bytes)?;
            let planned = outcome.planned_plan.ok_or_else(|| {
                io::Error::other(
                    "planner is inconclusive because the frozen universe is insufficient",
                )
            })?;
            write(output_plan, &serde_json::to_vec_pretty(&planned)?)?;
        }
        Command::AnalyzePilot {
            plan,
            planning_report,
            records,
            output_json,
            output_markdown,
        } => {
            let outputs = validate_output_paths(
                &[plan.as_path(), planning_report.as_path(), records.as_path()],
                &[output_json.as_path(), output_markdown.as_path()],
            )?;
            let output_json = &outputs[0];
            let output_markdown = &outputs[1];
            let plan = read_plan(&plan)?;
            let planning_report = read_planning_report(&planning_report)?;
            let report = analyze_pilot_records(&plan, &planning_report, &records)?;
            let json = serde_json::to_vec_pretty(&report)?;
            let markdown = render_pilot_markdown(&report);
            write(output_json, &json)?;
            write(output_markdown, markdown.as_bytes())?;
        }
        Command::Analyze {
            plan,
            records,
            output_json,
            output_markdown,
        } => {
            let outputs = validate_output_paths(
                &[plan.as_path(), records.as_path()],
                &[output_json.as_path(), output_markdown.as_path()],
            )?;
            let output_json = &outputs[0];
            let output_markdown = &outputs[1];
            let plan = read_plan(&plan)?;
            let report = analyze_records(&plan, &records)?;
            let json = serde_json::to_vec_pretty(&report)?;
            let markdown = render_markdown(&report);
            write(output_json, &json)?;
            write(output_markdown, markdown.as_bytes())?;
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

static NEXT_OUTPUT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_before_publish(path, bytes, || {})
}

fn write_before_publish(
    path: &Path,
    bytes: &[u8],
    before_publish: impl FnOnce(),
) -> io::Result<()> {
    match existing_output(path, bytes)? {
        Some(true) => return Ok(()),
        Some(false) => return Err(output_exists_with_different_bytes(path)),
        None => {}
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("output path has no file name"))?
        .to_string_lossy();
    let temporary_id = NEXT_OUTPUT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        temporary_id
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        before_publish();
        match fs::hard_link(&temporary, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                match existing_output(path, bytes)? {
                    Some(true) => Ok(()),
                    Some(false) => Err(output_exists_with_different_bytes(path)),
                    None => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    })();
    let cleanup = fs::remove_file(&temporary);
    if result.is_ok() {
        cleanup?;
        File::open(parent)?.sync_all()?;
    } else {
        let _ = cleanup;
    }
    result
}

fn existing_output(path: &Path, bytes: &[u8]) -> io::Result<Option<bool>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let flags = nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_NONBLOCK;
        options.custom_flags(flags.bits());
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "output path is not a regular file: {}",
            path.display()
        )));
    }
    let mut existing = Vec::new();
    file.read_to_end(&mut existing)?;
    Ok(Some(existing == bytes))
}

fn output_exists_with_different_bytes(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "output path already exists with different bytes: {}",
            path.display()
        ),
    )
}

fn validate_output_paths(inputs: &[&Path], outputs: &[&Path]) -> io::Result<Vec<PathBuf>> {
    let input_identities = inputs
        .iter()
        .map(|path| path_identity(path, false))
        .collect::<io::Result<Vec<_>>>()?;
    let mut output_identities = Vec::with_capacity(outputs.len());
    for output in outputs {
        let identity = path_identity(output, true)?;
        if input_identities
            .iter()
            .chain(&output_identities)
            .any(|protected| identity.aliases(protected))
        {
            return Err(io::Error::other(format!(
                "output path aliases an input or another output: {}",
                output.display()
            )));
        }
        output_identities.push(identity);
    }
    Ok(output_identities
        .into_iter()
        .map(|identity| identity.resolved)
        .collect())
}

struct PathIdentity {
    resolved: PathBuf,
    file_id: Option<FileId>,
}

impl PathIdentity {
    fn aliases(&self, other: &Self) -> bool {
        self.resolved == other.resolved
            || self
                .file_id
                .zip(other.file_id)
                .is_some_and(|(left, right)| left == right)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct FileId {
    device: u64,
    inode: u64,
}

fn path_identity(path: &Path, output: bool) -> io::Result<PathIdentity> {
    let absolute = absolute_path(path)?;
    if output {
        match fs::symlink_metadata(&absolute) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::other(format!(
                    "output path is a symbolic link: {}",
                    path.display()
                )));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(io::Error::other(format!(
                    "output path is not a regular file: {}",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let resolved = if output {
        resolve_allow_missing(&absolute)?
    } else {
        absolute.canonicalize()?
    };
    let file_id = fs::metadata(&absolute)
        .ok()
        .and_then(|metadata| file_id(&metadata));
    Ok(PathIdentity { resolved, file_id })
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn resolve_allow_missing(path: &Path) -> io::Result<PathBuf> {
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                for component in suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "path has no existing ancestor")
                })?;
                suffix.push(name.to_os_string());
                existing = existing.parent().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "path has no existing ancestor")
                })?;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn file_id(metadata: &fs::Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;

    Some(FileId {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn file_id(_metadata: &fs::Metadata) -> Option<FileId> {
    None
}

fn read_plan(path: &Path) -> Result<FrozenPlan, Box<dyn Error>> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn read_planning_report(path: &Path) -> Result<PlanningReport, Box<dyn Error>> {
    let bytes = fs::read(path)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let report: PlanningReport = serde_json::from_slice(&bytes)?;
    if serde_json::to_value(&report)? != value {
        return Err(
            io::Error::other("planning report contains fields outside its frozen schema").into(),
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_boundary_rejects_lexical_aliases_and_preserves_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input.json");
        fs::write(&input, b"input").unwrap();
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        let lexical_alias = child.join("..").join("input.json");
        assert!(validate_output_paths(&[&input], &[&lexical_alias]).is_err());

        let output = temp.path().join("output.json");
        fs::write(&output, b"old").unwrap();
        let validated = validate_output_paths(&[&input], &[&output]).unwrap();
        assert!(write(&validated[0], b"new").is_err());
        assert_eq!(fs::read(&output).unwrap(), b"old");
        write(&validated[0], b"old").unwrap();
        assert_eq!(fs::read(output).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn output_boundary_resolves_symlinks_before_parent_components() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let protected = temp.path().join("protected");
        let child = protected.join("child");
        let safe = temp.path().join("safe");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir(&safe).unwrap();
        let input = protected.join("input.json");
        fs::write(&input, b"input").unwrap();
        symlink(&child, safe.join("link")).unwrap();

        let traversal = safe.join("link").join("..").join("input.json");
        assert!(validate_output_paths(&[&input], &[&traversal]).is_err());

        let unresolved = safe.join("link").join("..").join("output.json");
        let validated = validate_output_paths(&[&input], &[&unresolved]).unwrap();
        assert_eq!(validated, [protected.join("output.json")]);
    }

    #[cfg(unix)]
    #[test]
    fn output_boundary_rejects_symbolic_and_hard_link_aliases() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input.json");
        fs::write(&input, b"input").unwrap();

        let symbolic = temp.path().join("symbolic.json");
        symlink(&input, &symbolic).unwrap();
        assert!(validate_output_paths(&[&input], &[&symbolic]).is_err());
        assert!(write(&symbolic, b"replacement").is_err());
        assert_eq!(fs::read(&input).unwrap(), b"input");

        let hard = temp.path().join("hard.json");
        fs::hard_link(&input, &hard).unwrap();
        assert!(validate_output_paths(&[&input], &[&hard]).is_err());

        let first = temp.path().join("first.json");
        fs::write(&first, b"old").unwrap();
        let second = temp.path().join("second.json");
        fs::hard_link(&first, &second).unwrap();
        assert!(validate_output_paths(&[&input], &[&first, &second]).is_err());
    }

    #[test]
    fn output_publication_never_replaces_a_racing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output.json");
        let result = write_before_publish(&output, b"generated", || {
            fs::write(&output, b"racing writer").unwrap();
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(output).unwrap(), b"racing writer");
    }
}
