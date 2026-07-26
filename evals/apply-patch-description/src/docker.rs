//! Docker containment, one-shot helpers, and hidden guest entry points.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aj_agent::TaskRegistry;
use aj_agent::events::AgentId;
use aj_agent::tool::{
    SpawnMode, SpawnResult, StartedTask, TaskKind, TaskOutputSource, TodoItem, ToolContext,
    ToolDetails,
};
use aj_models::types::UserContent;
use aj_tools::tools::bash::BashInput;
use aj_tools::{BuiltinToolOptions, builtin_tools_for_model};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

use crate::fixtures::{CommandResult, hidden_behavior_script, materialize, verify_candidate};
use crate::protocol::{
    FixtureWorkerInput, FixtureWorkerOutput, GitArtifacts, ProbeInput, ProbeResult, ProtocolError,
    SnapshotWorkerInput, SnapshotWorkerOutput, ToolOutcomeWire, ToolWorkerInput, VerifyWorkerInput,
    VerifyWorkerOutput, read_frame, write_frame,
};
use crate::snapshot::{IgnorePrefixes, capture, delta};
use crate::suite::committed_manifest;

const WORKSPACE: &str = "/workspace";
const VOLUME_BYTES: &str = "67108864";
const DOCKER_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(300);
pub const SOURCE_PROVENANCE_LABEL: &str = "org.aj.eval.source-provenance";
// `Vec<u8>` uses JSON arrays on the framed helper protocol. Keep each raw Git
// artifact small enough that worst-case byte encoding still fits the 16 MiB frame.
const MAX_GIT_ARTIFACT_BYTES: usize = 1024 * 1024;

/// Docker command, containment, or helper protocol failure.
#[derive(Debug)]
pub struct DockerError(pub String);

impl fmt::Display for DockerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DockerError {}

impl From<std::io::Error> for DockerError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<ProtocolError> for DockerError {
    fn from(error: ProtocolError) -> Self {
        Self(error.to_string())
    }
}

/// Size-bounded named tmpfs volume kept mounted for the trial lifetime.
pub struct FixtureVolume {
    name: String,
    keeper: String,
    cleaned: bool,
}

/// Immutable image identity and the source state embedded at build time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageIdentity {
    pub id: String,
    pub source_provenance: String,
}

impl FixtureVolume {
    pub async fn create(image: &str, label: &str) -> Result<Self, DockerError> {
        let name = unique_name(&format!("volume-{label}"));
        let keeper = unique_name(&format!("keeper-{label}"));
        checked(Command::new("docker").args([
            "volume",
            "create",
            "--driver",
            "local",
            "--opt",
            "type=tmpfs",
            "--opt",
            "device=tmpfs",
            "--opt",
            &format!("o=size={VOLUME_BYTES},mode=0755"),
            &name,
        ]))
        .await?;

        let mut command = Command::new("docker");
        command.args([
            "run",
            "-d",
            "--name",
            &keeper,
            "--network",
            "none",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--pids-limit",
            "16",
            "--memory",
            "64m",
            "--cpus",
            "0.10",
            "--mount",
            &format!("type=volume,src={name},dst={WORKSPACE}"),
            image,
            "__volume-keeper",
        ]);
        let output = match command_output(&mut command, "start volume keeper").await {
            Ok(output) => output,
            Err(error) => {
                let cleanup = cleanup_named_container(&keeper, true).await;
                let volume_cleanup =
                    checked(Command::new("docker").args(["volume", "rm", "-f", &name])).await;
                return Err(combine_cleanup_errors(error, [cleanup, volume_cleanup]));
            }
        };
        if !output.status.success() {
            let start_error = command_error("start volume keeper", &output);
            let cleanup = cleanup_named_container(&keeper, true).await;
            let volume_cleanup =
                checked(Command::new("docker").args(["volume", "rm", "-f", &name])).await;
            return Err(combine_cleanup_errors(
                start_error,
                [cleanup, volume_cleanup],
            ));
        }
        if String::from_utf8_lossy(&output.stdout).trim().is_empty() {
            let error = DockerError("Docker returned an empty keeper id".into());
            let cleanup = cleanup_named_container(&keeper, true).await;
            let volume_cleanup =
                checked(Command::new("docker").args(["volume", "rm", "-f", &name])).await;
            return Err(combine_cleanup_errors(error, [cleanup, volume_cleanup]));
        }
        Ok(Self {
            name,
            keeper,
            cleaned: false,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Stops the keeper, confirms its removal, and removes the tmpfs volume.
    pub async fn cleanup(&mut self) -> Result<(), DockerError> {
        if self.cleaned {
            return Ok(());
        }
        let container = cleanup_named_container(&self.keeper, true).await;
        let volume = checked(Command::new("docker").args(["volume", "rm", "-f", &self.name])).await;
        match (container, volume) {
            (Ok(()), Ok(())) => {
                self.cleaned = true;
                Ok(())
            }
            (container, volume) => Err(combine_cleanup_errors(
                DockerError("fixture volume cleanup failed".into()),
                [container, volume],
            )),
        }
    }
}

impl Drop for FixtureVolume {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let _ = std::process::Command::new("timeout")
            .args(["30", "docker", "kill", &self.keeper])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::process::Command::new("timeout")
            .args(["30", "docker", "wait", &self.keeper])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::process::Command::new("timeout")
            .args(["30", "docker", "rm", "-f", &self.keeper])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::process::Command::new("timeout")
            .args(["30", "docker", "volume", "rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Rejects mutable tags and returns Docker's immutable id and source label.
pub async fn validate_image(image: &str) -> Result<ImageIdentity, DockerError> {
    let digest = immutable_digest(image).ok_or_else(|| {
        DockerError("image must be immutable: use sha256:<64 hex> or a name@sha256:<64 hex>".into())
    })?;
    let mut command = Command::new("docker");
    command.args([
        "image",
        "inspect",
        "--format",
        &format!("{{{{.Id}}}}\t{{{{index .Config.Labels \"{SOURCE_PROVENANCE_LABEL}\"}}}}"),
        image,
    ]);
    let output = command_output(&mut command, "inspect image").await?;
    if !output.status.success() {
        return Err(command_error("inspect image", &output));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (id, source_provenance) = text
        .trim()
        .split_once('\t')
        .ok_or_else(|| DockerError("Docker image inspection omitted source provenance".into()))?;
    if source_provenance.is_empty() || source_provenance == "<no value>" {
        return Err(DockerError(format!(
            "image is missing required {SOURCE_PROVENANCE_LABEL} provenance label"
        )));
    }
    if image.starts_with("sha256:") && id != format!("sha256:{digest}") {
        return Err(DockerError(
            "Docker image id does not match requested digest".into(),
        ));
    }
    Ok(ImageIdentity {
        id: id.into(),
        source_provenance: source_provenance.into(),
    })
}

fn immutable_digest(image: &str) -> Option<&str> {
    let digest = image
        .strip_prefix("sha256:")
        .or_else(|| image.rsplit_once("@sha256:").map(|(_, digest)| digest))?;
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(digest)
}

fn unique_name(label: &str) -> String {
    let safe_label = label
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .take(20)
        .collect::<String>();
    format!("aj-eval-{safe_label}-{:032x}", rand::random::<u128>())
}

fn validate_container_name(name: &str) -> Result<(), DockerError> {
    if name.len() > 63
        || name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(DockerError(format!(
            "invalid evaluator container name: {name}"
        )));
    }
    Ok(())
}

/// A fully specified Docker invocation whose unique name is known before spawn.
pub struct ContainerCommand {
    name: String,
    command: Command,
}

impl ContainerCommand {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn spawn(mut self) -> Result<RunningContainer, DockerError> {
        validate_container_name(&self.name)?;
        let child = self.command.spawn()?;
        Ok(RunningContainer {
            name: self.name,
            child: Some(child),
            cleaned: false,
        })
    }
}

/// An attached container that must be explicitly reaped and removed.
pub struct RunningContainer {
    name: String,
    child: Option<Child>,
    cleaned: bool,
}

impl RunningContainer {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.as_mut()?.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    /// Waits for normal exit or kills the container, then waits and removes it.
    pub async fn finish(mut self, terminate: bool) -> Result<ExitStatus, DockerError> {
        let mut errors = Vec::new();
        if terminate && let Err(error) = kill_if_running(&self.name).await {
            errors.push(error.to_string());
        }
        let status = match self.child.as_mut() {
            Some(child) => match tokio::time::timeout(DOCKER_COMMAND_TIMEOUT, child.wait()).await {
                Err(_) => {
                    errors.push("wait for attached Docker CLI timed out".into());
                    None
                }
                Ok(result) => match result {
                    Ok(status) => Some(status),
                    Err(error) => {
                        errors.push(format!("wait for attached Docker CLI failed: {error}"));
                        None
                    }
                },
            },
            None => None,
        };
        self.child = None;
        if let Err(error) = wait_container(&self.name).await {
            errors.push(error.to_string());
        }
        if let Err(error) = remove_container(&self.name).await {
            errors.push(error.to_string());
        }
        if errors.is_empty() {
            self.cleaned = true;
            status.ok_or_else(|| DockerError("attached Docker CLI had no exit status".into()))
        } else {
            Err(DockerError(errors.join("; ")))
        }
    }
}

impl Drop for RunningContainer {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        for _ in 0..20 {
            let inspect = std::process::Command::new("timeout")
                .args(["30", "docker", "container", "inspect", &self.name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if inspect.is_ok_and(|status| status.success()) {
                break;
            }
            if self
                .child
                .as_mut()
                .and_then(|child| child.try_wait().ok())
                .flatten()
                .is_some()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::process::Command::new("timeout")
            .args(["30", "docker", "kill", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::process::Command::new("timeout")
            .args(["30", "docker", "wait", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::process::Command::new("timeout")
            .args(["30", "docker", "rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

fn contained_command(image: &str, command_name: &str) -> ContainerCommand {
    let name = unique_name(command_name.trim_start_matches('_'));
    let mut command = Command::new("docker");
    command.args([
        "run",
        "--name",
        &name,
        "-i",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--pids-limit",
        "128",
        "--memory",
        "1g",
        "--cpus",
        "1.0",
        "--ulimit",
        "nofile=1024:1024",
        "--ulimit",
        "nproc=128:128",
        "--ulimit",
        "fsize=65536:65536",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,noexec,size=256m,mode=1777",
        "--tmpfs",
        "/home/aj:rw,nosuid,nodev,noexec,size=32m,mode=0700",
        "--env",
        "HOME=/home/aj",
        "--env",
        "TMPDIR=/tmp",
        "--env",
        "PYTHONDONTWRITEBYTECODE=1",
        "--workdir",
        WORKSPACE,
        image,
        command_name,
    ]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    ContainerCommand { name, command }
}

fn mount(command: &mut Command, volume: &FixtureVolume, destination: &str, read_only: bool) {
    let suffix = if read_only { ",readonly" } else { "" };
    command.arg("--mount").arg(format!(
        "type=volume,src={},dst={destination}{suffix}",
        volume.name()
    ));
}

/// Starts a paid worker with an attached, framed stdin/stdout channel.
pub fn worker_command(image: &str, volume: &FixtureVolume) -> ContainerCommand {
    let mut command = contained_command(image, "__worker");
    // Docker options must precede the image and hidden command.
    insert_mount_before_image(&mut command, volume, WORKSPACE, false);
    command
}

fn insert_mount_before_image(
    command: &mut ContainerCommand,
    volume: &FixtureVolume,
    destination: &str,
    read_only: bool,
) {
    let args = command
        .command
        .as_std()
        .get_args()
        .map(OsString::from)
        .collect::<Vec<_>>();
    let image_position = args.len() - 2;
    let mut rebuilt = Command::new("docker");
    rebuilt.args(&args[..image_position]);
    mount(&mut rebuilt, volume, destination, read_only);
    rebuilt.args(&args[image_position..]);
    rebuilt.stdin(Stdio::piped());
    rebuilt.stdout(Stdio::piped());
    rebuilt.stderr(Stdio::piped());
    command.command = rebuilt;
}

/// Runs a framed one-shot helper with one fixture volume.
pub async fn run_helper<I, O>(
    image: &str,
    hidden_command: &str,
    volume: &FixtureVolume,
    read_only: bool,
    input: &I,
) -> Result<O, DockerError>
where
    I: Serialize,
    O: DeserializeOwned,
{
    let mut command = contained_command(image, hidden_command);
    insert_mount_before_image(&mut command, volume, WORKSPACE, read_only);
    run_framed(command, input, None).await
}

/// Runs a one-shot helper and tears down its container when the trial cancels.
pub async fn run_helper_cancellable<I, O>(
    image: &str,
    hidden_command: &str,
    volume: &FixtureVolume,
    read_only: bool,
    input: &I,
    cancel: &CancellationToken,
) -> Result<O, DockerError>
where
    I: Serialize,
    O: DeserializeOwned,
{
    let mut command = contained_command(image, hidden_command);
    insert_mount_before_image(&mut command, volume, WORKSPACE, read_only);
    run_framed(command, input, Some(cancel)).await
}

/// Clones one volume into another without exposing either to the host filesystem.
pub async fn copy_volume(
    image: &str,
    source: &FixtureVolume,
    destination: &FixtureVolume,
) -> Result<(), DockerError> {
    let mut command = contained_command(image, "__copy-worker");
    insert_mount_before_image(&mut command, source, "/source", true);
    insert_mount_before_image(&mut command, destination, "/destination", false);
    run_framed::<_, ()>(command, &(), None).await
}

/// Clones a volume and tears down the helper if the parent cancels.
pub async fn copy_volume_cancellable(
    image: &str,
    source: &FixtureVolume,
    destination: &FixtureVolume,
    cancel: &CancellationToken,
) -> Result<(), DockerError> {
    let mut command = contained_command(image, "__copy-worker");
    insert_mount_before_image(&mut command, source, "/source", true);
    insert_mount_before_image(&mut command, destination, "/destination", false);
    run_framed::<_, ()>(command, &(), Some(cancel)).await
}

async fn run_framed<I, O>(
    command: ContainerCommand,
    input: &I,
    cancel: Option<&CancellationToken>,
) -> Result<O, DockerError>
where
    I: Serialize,
    O: DeserializeOwned,
{
    let mut child = command.spawn()?;
    let mut stdin = match child.take_stdin() {
        Some(stdin) => stdin,
        None => {
            let cleanup = child.finish(true).await;
            return Err(combine_cleanup_errors(
                DockerError("Docker helper has no stdin".into()),
                [cleanup.map(|_| ())],
            ));
        }
    };
    if let Err(error) = write_frame(&mut stdin, input).await {
        let cleanup = child.finish(true).await;
        return Err(combine_cleanup_errors(error.into(), [cleanup.map(|_| ())]));
    }
    if let Err(error) = stdin.shutdown().await {
        let cleanup = child.finish(true).await;
        return Err(combine_cleanup_errors(error.into(), [cleanup.map(|_| ())]));
    }
    drop(stdin);
    let mut stdout = match child.take_stdout() {
        Some(stdout) => stdout,
        None => {
            let cleanup = child.finish(true).await;
            return Err(combine_cleanup_errors(
                DockerError("Docker helper has no stdout".into()),
                [cleanup.map(|_| ())],
            ));
        }
    };
    let result = if let Some(cancel) = cancel {
        tokio::select! {
            result = read_frame(&mut stdout) => result,
            () = cancel.cancelled() => {
                let cleanup = child.finish(true).await;
                return Err(combine_cleanup_errors(
                    DockerError("Docker helper cancelled".into()),
                    [cleanup.map(|_| ())],
                ));
            }
            () = tokio::time::sleep(HELPER_TIMEOUT) => {
                let cleanup = child.finish(true).await;
                return Err(combine_cleanup_errors(
                    DockerError(format!("Docker helper timed out after {} seconds", HELPER_TIMEOUT.as_secs())),
                    [cleanup.map(|_| ())],
                ));
            }
        }
    } else {
        match tokio::time::timeout(HELPER_TIMEOUT, read_frame(&mut stdout)).await {
            Ok(result) => result,
            Err(_) => {
                let cleanup = child.finish(true).await;
                return Err(combine_cleanup_errors(
                    DockerError(format!(
                        "Docker helper timed out after {} seconds",
                        HELPER_TIMEOUT.as_secs()
                    )),
                    [cleanup.map(|_| ())],
                ));
            }
        }
    };
    let terminate = !matches!(result, Ok(Some(_)));
    let cleanup = child.finish(terminate).await;
    let result = match result {
        Ok(Some(result)) => result,
        Ok(None) => {
            return Err(combine_cleanup_errors(
                DockerError("Docker helper returned no frame".into()),
                [cleanup.map(|_| ())],
            ));
        }
        Err(error) => {
            return Err(combine_cleanup_errors(error.into(), [cleanup.map(|_| ())]));
        }
    };
    let status = cleanup?;
    if !status.success() {
        return Err(DockerError(format!("run helper failed with {status}")));
    }
    Ok(result)
}

async fn checked(command: &mut Command) -> Result<(), DockerError> {
    let output = command_output(command, "Docker command").await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("Docker command", &output))
    }
}

async fn command_output(
    command: &mut Command,
    action: &str,
) -> Result<std::process::Output, DockerError> {
    tokio::time::timeout(DOCKER_COMMAND_TIMEOUT, command.output())
        .await
        .map_err(|_| DockerError(format!("{action} timed out")))?
        .map_err(Into::into)
}

async fn kill_if_running(name: &str) -> Result<(), DockerError> {
    validate_container_name(name)?;
    let mut command = Command::new("docker");
    command.args([
        "container",
        "inspect",
        "--format",
        "{{.State.Running}}",
        name,
    ]);
    let output = command_output(&mut command, "inspect container before kill").await?;
    if !output.status.success() {
        return Err(command_error("inspect container before kill", &output));
    }
    if String::from_utf8_lossy(&output.stdout).trim() == "true" {
        checked(Command::new("docker").args(["kill", name])).await?;
    }
    Ok(())
}

async fn wait_container(name: &str) -> Result<(), DockerError> {
    validate_container_name(name)?;
    checked(Command::new("docker").args(["wait", name])).await
}

async fn remove_container(name: &str) -> Result<(), DockerError> {
    validate_container_name(name)?;
    checked(Command::new("docker").args(["rm", "-f", name])).await
}

async fn cleanup_named_container(name: &str, terminate: bool) -> Result<(), DockerError> {
    validate_container_name(name)?;
    let mut command = Command::new("docker");
    command.args([
        "container",
        "inspect",
        "--format",
        "{{.State.Running}}",
        name,
    ]);
    let inspect = command_output(&mut command, "inspect container for cleanup").await?;
    if !inspect.status.success() {
        let stderr = String::from_utf8_lossy(&inspect.stderr);
        if stderr.contains("No such object") || stderr.contains("No such container") {
            return Ok(());
        }
        return Err(command_error("inspect container for cleanup", &inspect));
    }
    if terminate && String::from_utf8_lossy(&inspect.stdout).trim() == "true" {
        checked(Command::new("docker").args(["kill", name])).await?;
    }
    wait_container(name).await?;
    remove_container(name).await
}

fn combine_cleanup_errors<const N: usize>(
    primary: DockerError,
    cleanup: [Result<(), DockerError>; N],
) -> DockerError {
    let errors = cleanup
        .into_iter()
        .filter_map(Result::err)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if errors.is_empty() {
        primary
    } else {
        DockerError(format!("{primary}; cleanup failed: {}", errors.join("; ")))
    }
}

fn command_error(action: &str, output: &std::process::Output) -> DockerError {
    DockerError(format!(
        "{action} failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Mandatory isolation checks run before a live provider can be called.
pub async fn preflight(image: &str) -> Result<(), DockerError> {
    tokio::time::timeout(PREFLIGHT_TIMEOUT, preflight_inner(image))
        .await
        .map_err(|_| DockerError("preflight timed out".into()))?
}

async fn preflight_inner(image: &str) -> Result<(), DockerError> {
    validate_image(image).await?;
    let mut fixture = FixtureVolume::create(image, "preflight").await?;
    let host_probe = std::env::temp_dir().join(format!(
        "aj-eval-host-probe-{:032x}",
        rand::random::<u128>()
    ));
    fs::write(&host_probe, b"host-only")?;
    let result = async {
        let manifest = committed_manifest().map_err(|error| DockerError(error.to_string()))?;
        let universe = crate::schedule::freeze_universe(&manifest, "docker-preflight-v1", 5)
            .map_err(|error| DockerError(error.to_string()))?;
        let instance = universe
            .instances
            .first()
            .cloned()
            .ok_or_else(|| DockerError("preflight task universe is empty".into()))?;
        let initialized: FixtureWorkerOutput = run_helper(
            image,
            "__fixture-worker",
            &fixture,
            false,
            &FixtureWorkerInput { instance },
        )
        .await?;
        if initialized.baseline_commit.len() != 40
            || !initialized
                .baseline_commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(DockerError(
                "fixture helper did not produce a baseline Git commit".into(),
            ));
        }
        let initialized_output =
            snapshot_helper(image, &fixture, true, Some(&initialized.baseline_commit)).await?;
        let initialized_snapshot = initialized_output.snapshot;
        if initialized_snapshot.root_hash != initialized.fixture.baseline_revision {
            return Err(DockerError(
                "Git initialization changed model-visible fixture state".into(),
            ));
        }
        if !initialized_output
            .git
            .is_some_and(|git| git.diff.is_empty() && git.status.is_empty())
        {
            return Err(DockerError(
                "baseline Git commit differs from the materialized fixture".into(),
            ));
        }

        let probe: ProbeResult = run_helper(
            image,
            "__probe-worker",
            &fixture,
            false,
            &ProbeInput {
                host_path: host_probe.to_string_lossy().into_owned(),
            },
        )
        .await?;
        if !probe.absolute_write_blocked
            || !probe.parent_write_blocked
            || !probe.host_path_unreadable
            || !probe.credential_environment_absent
            || !probe.network_blocked
        {
            return Err(DockerError(format!("isolation probe failed: {probe:?}")));
        }

        let before = snapshot_helper(image, &fixture, false, None)
            .await?
            .snapshot;
        for path in ["/escape", "../escape"] {
            let result: ToolOutcomeWire = run_helper(
                image,
                "__tool-worker",
                &fixture,
                false,
                &ToolWorkerInput {
                    name: "apply_patch".into(),
                    arguments: serde_json::json!({
                        "patchText": format!(
                            "*** Begin Patch\n*** Add File: {path}\n+escape\n*** End Patch\n"
                        )
                    }),
                },
            )
            .await?;
            if !result.is_error {
                return Err(DockerError(format!(
                    "apply_patch path escape was accepted: {path}"
                )));
            }
        }
        let after = snapshot_helper(image, &fixture, false, None)
            .await?
            .snapshot;
        if before != after {
            return Err(DockerError(
                "apply_patch path probe mutated the fixture volume".into(),
            ));
        }

        let background = ToolWorkerInput {
            name: "bash".into(),
            arguments: serde_json::json!({
                "command": "sleep 30 >/tmp/background-probe 2>&1 &",
                "description": "probe descendant cleanup",
                "run_in_background": false,
                "timeout": 5
            }),
        };
        let started = Instant::now();
        let _: ToolOutcomeWire =
            run_helper(image, "__tool-worker", &fixture, false, &background).await?;
        if started.elapsed() > Duration::from_secs(10) {
            return Err(DockerError(
                "shell background descendant prevented prompt container teardown".into(),
            ));
        }

        let rejected: ToolOutcomeWire = run_helper(
            image,
            "__tool-worker",
            &fixture,
            false,
            &ToolWorkerInput {
                name: "bash".into(),
                arguments: serde_json::json!({
                    "command": "sleep 1",
                    "description": "probe API background rejection",
                    "run_in_background": true,
                    "timeout": 5
                }),
            },
        )
        .await?;
        if !rejected.is_error || !wire_text(&rejected).contains("background") {
            return Err(DockerError("background bash API was not rejected".into()));
        }
        never_framing_helper_probe(image, &fixture).await?;
        cancelled_container_probe(image, &fixture, &initialized.baseline_commit).await
    }
    .await;
    let host_cleanup = fs::remove_file(&host_probe).map_err(DockerError::from);
    let fixture_cleanup = fixture.cleanup().await;
    match result {
        Ok(()) => {
            host_cleanup?;
            fixture_cleanup
        }
        Err(error) => Err(combine_cleanup_errors(
            error,
            [host_cleanup, fixture_cleanup],
        )),
    }
}

async fn never_framing_helper_probe(
    image: &str,
    fixture: &FixtureVolume,
) -> Result<(), DockerError> {
    let mut command = contained_command(image, "__volume-keeper");
    insert_mount_before_image(&mut command, fixture, WORKSPACE, true);
    let name = command.name().to_string();
    let error = run_framed::<_, ()>(command, &(), None)
        .await
        .expect_err("volume keeper never writes a frame");
    if !error.to_string().contains("timed out") {
        return Err(DockerError(format!(
            "never-framing helper failed for the wrong reason: {error}"
        )));
    }
    if !container_absent(&name).await? {
        return Err(DockerError(format!(
            "timed-out helper still exists after cleanup: {name}"
        )));
    }
    Ok(())
}

async fn snapshot_helper(
    image: &str,
    fixture: &FixtureVolume,
    include_git_artifacts: bool,
    baseline_commit: Option<&str>,
) -> Result<SnapshotWorkerOutput, DockerError> {
    run_helper(
        image,
        "__snapshot-worker",
        fixture,
        true,
        &SnapshotWorkerInput {
            include_git_artifacts,
            baseline_commit: baseline_commit.map(str::to_string),
        },
    )
    .await
}

async fn cancelled_container_probe(
    image: &str,
    fixture: &FixtureVolume,
    baseline_commit: &str,
) -> Result<(), DockerError> {
    let mut command = contained_command(image, "__tool-worker");
    insert_mount_before_image(&mut command, fixture, WORKSPACE, false);
    let mut container = command.spawn()?;
    let name = container.name().to_string();
    let operation = async {
        let mut stdin = container
            .take_stdin()
            .ok_or_else(|| DockerError("lifecycle probe has no stdin".into()))?;
        write_frame(
            &mut stdin,
            &ToolWorkerInput {
                name: "bash".into(),
                arguments: serde_json::json!({
                    "command": "printf started > lifecycle-probe; sleep 2; printf delayed >> lifecycle-probe",
                    "description": "probe attached-container cancellation",
                    "run_in_background": false,
                    "timeout": 10
                }),
            },
        )
        .await?;
        drop(stdin);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = snapshot_helper(image, fixture, false, None).await?.snapshot;
            if snapshot
                .entries
                .iter()
                .any(|entry| entry.path == "lifecycle-probe")
            {
                return Ok::<(), DockerError>(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(DockerError(
                    "cancelled-container probe never started in the guest".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    .await;
    let cleanup = container.finish(true).await.map(|_| ());
    if let Err(error) = operation {
        return Err(combine_cleanup_errors(error, [cleanup]));
    }
    cleanup?;
    if !container_absent(&name).await? {
        return Err(DockerError(format!(
            "cancelled container still exists after cleanup: {name}"
        )));
    }
    let after_kill = snapshot_helper(image, fixture, false, None).await?.snapshot;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let after_delay = snapshot_helper(image, fixture, false, None).await?.snapshot;
    if after_kill != after_delay {
        return Err(DockerError(
            "cancelled guest performed a delayed fixture mutation".into(),
        ));
    }
    let committed: ToolOutcomeWire = run_helper(
        image,
        "__tool-worker",
        fixture,
        false,
        &ToolWorkerInput {
            name: "bash".into(),
            arguments: serde_json::json!({
                "command": "git add lifecycle-probe && git commit -m lifecycle-probe",
                "description": "prove diff uses the trusted baseline commit",
                "run_in_background": false,
                "timeout": 10
            }),
        },
    )
    .await?;
    if committed.is_error {
        return Err(DockerError(format!(
            "cannot prepare committed Git-artifact probe: {}",
            wire_text(&committed)
        )));
    }
    let git = snapshot_helper(image, fixture, true, Some(baseline_commit))
        .await?
        .git
        .ok_or_else(|| DockerError("Git artifact helper returned no artifacts".into()))?;
    if git.diff.is_empty() || !git.status.is_empty() {
        return Err(DockerError(
            "Git artifacts do not reconstruct a clean commit made after the trusted baseline"
                .into(),
        ));
    }
    Ok(())
}

async fn container_absent(name: &str) -> Result<bool, DockerError> {
    validate_container_name(name)?;
    let mut command = Command::new("docker");
    command.args(["container", "inspect", name]);
    let output = command_output(&mut command, "inspect container absence").await?;
    if output.status.success() {
        return Ok(false);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such object") || stderr.contains("No such container") {
        Ok(true)
    } else {
        Err(command_error("check cancelled container absence", &output))
    }
}

/// Returns all text blocks in a tool outcome.
pub fn wire_text(outcome: &ToolOutcomeWire) -> String {
    outcome
        .content
        .iter()
        .filter_map(|content| match content {
            UserContent::Text(text) => Some(text.text.as_str()),
            UserContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Hidden fixture helper.
pub async fn fixture_worker() -> Result<(), DockerError> {
    let input: FixtureWorkerInput = read_stdin().await?;
    let fixture = materialize(&input.instance, Path::new(WORKSPACE))
        .map_err(|error| DockerError(error.to_string()))?;
    initialize_git_repository().await?;
    let baseline_commit = git_stdout(&["rev-parse", "HEAD"], &[0]).await?;
    write_stdout(&FixtureWorkerOutput {
        fixture,
        baseline_commit: String::from_utf8_lossy(&baseline_commit).trim().to_string(),
    })
    .await
}

/// Hidden snapshot helper.
pub async fn snapshot_worker() -> Result<(), DockerError> {
    let input: SnapshotWorkerInput = read_stdin().await?;
    let manifest = committed_manifest().map_err(|error| DockerError(error.to_string()))?;
    let ignores =
        IgnorePrefixes::from_manifest(&manifest).map_err(|error| DockerError(error.to_string()))?;
    let snapshot =
        capture(Path::new(WORKSPACE), &ignores).map_err(|error| DockerError(error.to_string()))?;
    let git = if input.include_git_artifacts {
        let baseline = input.baseline_commit.as_deref().ok_or_else(|| {
            DockerError("Git artifact capture requires the trusted baseline commit".into())
        })?;
        Some(capture_git_artifacts(baseline).await?)
    } else {
        None
    };
    write_stdout(&SnapshotWorkerOutput { snapshot, git }).await
}

async fn initialize_git_repository() -> Result<(), DockerError> {
    git_stdout(&["init", "--quiet"], &[0]).await?;
    git_stdout(&["config", "user.name", "AJ Eval"], &[0]).await?;
    git_stdout(&["config", "user.email", "eval@localhost"], &[0]).await?;
    git_stdout(&["add", "--all"], &[0]).await?;
    let mut command = Command::new("git");
    command
        .args([
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "initial fixture",
        ])
        .current_dir(WORKSPACE)
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z");
    let output = command_output(&mut command, "initialize fixture Git commit").await?;
    if !output.status.success() {
        return Err(command_error("initialize fixture Git commit", &output));
    }
    Ok(())
}

async fn capture_git_artifacts(baseline_commit: &str) -> Result<GitArtifacts, DockerError> {
    if baseline_commit.len() != 40 || !baseline_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(DockerError("invalid trusted baseline commit".into()));
    }
    let status = git_stdout_readonly(&["status", "--porcelain=v1", "-z"], &[0]).await?;
    let mut diff = git_stdout_readonly(
        &["diff", "--binary", "--no-ext-diff", baseline_commit, "--"],
        &[0],
    )
    .await?;
    for entry in status
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        if entry.starts_with(b"?? ") {
            let path = std::str::from_utf8(&entry[3..]).map_err(|_| {
                DockerError("Git status contains a non-UTF-8 untracked path".into())
            })?;
            let untracked = git_stdout_readonly(
                &[
                    "diff",
                    "--binary",
                    "--no-ext-diff",
                    "--no-index",
                    "/dev/null",
                    path,
                ],
                &[0, 1],
            )
            .await?;
            append_bounded(&mut diff, &untracked, "Git diff")?;
        }
    }
    if status.len() > MAX_GIT_ARTIFACT_BYTES {
        return Err(DockerError(format!(
            "Git status exceeds {MAX_GIT_ARTIFACT_BYTES} bytes"
        )));
    }
    Ok(GitArtifacts { diff, status })
}

async fn git_stdout(arguments: &[&str], accepted: &[i32]) -> Result<Vec<u8>, DockerError> {
    git_stdout_with_optional_locks(arguments, accepted, true).await
}

async fn git_stdout_readonly(arguments: &[&str], accepted: &[i32]) -> Result<Vec<u8>, DockerError> {
    git_stdout_with_optional_locks(arguments, accepted, false).await
}

async fn git_stdout_with_optional_locks(
    arguments: &[&str],
    accepted: &[i32],
    optional_locks: bool,
) -> Result<Vec<u8>, DockerError> {
    let mut command = Command::new("git");
    command.args(arguments).current_dir(WORKSPACE);
    if !optional_locks {
        command.env("GIT_OPTIONAL_LOCKS", "0");
    }
    let output = command_output(&mut command, "run trusted Git command").await?;
    if !output
        .status
        .code()
        .is_some_and(|code| accepted.contains(&code))
    {
        return Err(command_error("run trusted Git command", &output));
    }
    if output.stdout.len() > MAX_GIT_ARTIFACT_BYTES {
        return Err(DockerError(format!(
            "Git command output exceeds {MAX_GIT_ARTIFACT_BYTES} bytes"
        )));
    }
    Ok(output.stdout)
}

fn append_bounded(target: &mut Vec<u8>, bytes: &[u8], label: &str) -> Result<(), DockerError> {
    if target.len().saturating_add(bytes.len()) > MAX_GIT_ARTIFACT_BYTES {
        return Err(DockerError(format!(
            "{label} exceeds {MAX_GIT_ARTIFACT_BYTES} bytes"
        )));
    }
    target.extend_from_slice(bytes);
    Ok(())
}

/// Hidden original-production-tool helper.
pub async fn tool_worker() -> Result<(), DockerError> {
    let input: ToolWorkerInput = read_stdin().await?;
    let outcome = execute_tool(input).await;
    write_stdout(&outcome).await
}

async fn execute_tool(input: ToolWorkerInput) -> ToolOutcomeWire {
    if input.name == "bash"
        && serde_json::from_value::<BashInput>(input.arguments.clone())
            .is_ok_and(|input| input.run_in_background)
    {
        return error_wire("background bash is disabled for this evaluation");
    }
    let tools = builtin_tools_for_model(
        &BuiltinToolOptions {
            image_auto_resize: true,
            bash_rtk: false,
        },
        &["agent".into()],
        Some("gpt"),
    );
    let Some(tool) = tools.into_iter().find(|tool| tool.name == input.name) else {
        return error_wire("tool is not allowed in the one-shot worker");
    };
    if !matches!(input.name.as_str(), "apply_patch" | "bash" | "read_file") {
        return error_wire("tool is not brokered as an effectful operation");
    }
    let mut context = MinimalToolContext::new(PathBuf::from(WORKSPACE));
    match (tool.func)(&mut context, input.arguments).await {
        Ok(outcome) => ToolOutcomeWire {
            content: outcome.content,
            details: serde_json::to_value(outcome.details).unwrap_or_else(
                |error| serde_json::json!({"serialization_error": error.to_string()}),
            ),
            is_error: outcome.is_error,
        },
        Err(error) => error_wire(&format!("tool execution failed: {error}")),
    }
}

fn error_wire(message: &str) -> ToolOutcomeWire {
    ToolOutcomeWire {
        content: vec![UserContent::text(message)],
        details: serde_json::json!({"kind": "text", "summary": "error", "body": message}),
        is_error: true,
    }
}

/// Hidden verifier helper.
pub async fn verify_worker() -> Result<(), DockerError> {
    let input: VerifyWorkerInput = read_stdin().await?;
    let authoritative = Path::new("/tmp/authoritative");
    let candidate_before = Path::new("/tmp/candidate-before");
    fs::create_dir(authoritative)?;
    fs::create_dir(candidate_before)?;
    let fixture = materialize(&input.instance, authoritative)
        .map_err(|error| DockerError(error.to_string()))?;
    let manifest = committed_manifest().map_err(|error| DockerError(error.to_string()))?;
    let ignores =
        IgnorePrefixes::from_manifest(&manifest).map_err(|error| DockerError(error.to_string()))?;
    let before =
        capture(Path::new(WORKSPACE), &ignores).map_err(|error| DockerError(error.to_string()))?;
    copy_tree(Path::new(WORKSPACE), candidate_before)?;
    let command_result = match &fixture.visible_check {
        Some(request) => Some(run_visible_check(&request.argv).await?),
        None => None,
    };
    let hidden_result = match hidden_behavior_script(&input.instance)
        .map_err(|error| DockerError(error.to_string()))?
    {
        Some(script) => Some(run_hidden_check(&script).await?),
        None => None,
    };
    let after =
        capture(Path::new(WORKSPACE), &ignores).map_err(|error| DockerError(error.to_string()))?;
    let mutations = delta(&before, &after);
    let mut report = verify_candidate(
        &input.instance,
        &fixture,
        authoritative,
        candidate_before,
        command_result.as_ref(),
        hidden_result.as_ref(),
    )
    .map_err(|error| DockerError(error.to_string()))?;
    if !mutations.paths.is_empty() {
        report.passed = false;
        report
            .reasons
            .push("the verifier command mutated candidate state".into());
    }
    write_stdout(&VerifyWorkerOutput {
        report,
        command_result,
        before,
        after,
        mutations,
    })
    .await
}

async fn run_visible_check(argv: &[String]) -> Result<CommandResult, DockerError> {
    let (program, arguments) = argv
        .split_first()
        .ok_or_else(|| DockerError("verifier argv is empty".into()))?;
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(program)
            .args(arguments)
            .current_dir(WORKSPACE)
            .output(),
    )
    .await
    .map_err(|_| DockerError("visible verifier timed out".into()))??;
    Ok(CommandResult {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

async fn run_hidden_check(script: &str) -> Result<CommandResult, DockerError> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("python3")
            .args(["-I", "-B", "-c", script])
            .current_dir(WORKSPACE)
            .output(),
    )
    .await
    .map_err(|_| DockerError("hidden verifier timed out".into()))??;
    Ok(CommandResult {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Hidden in-container isolation probe.
pub async fn probe_worker() -> Result<(), DockerError> {
    let input: ProbeInput = read_stdin().await?;
    let absolute_write_blocked = fs::write("/escape", b"escape").is_err();
    let parent_write_blocked = fs::write("../escape", b"escape").is_err();
    let host_path_unreadable = fs::read(&input.host_path).is_err();
    let credential_environment_absent = std::env::vars().all(|(name, _)| {
        let upper = name.to_ascii_uppercase();
        !upper.contains("API_KEY")
            && !upper.contains("ACCESS_TOKEN")
            && !upper.contains("AUTH_TOKEN")
            && name != "MODEL_URL"
    });
    let network_blocked = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect("1.1.1.1:53"),
    )
    .await
    .map_or(true, |result| result.is_err());
    let mut open_fds = fs::read_dir("/proc/self/fd")?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<i32>().ok())
        .collect::<Vec<_>>();
    open_fds.sort_unstable();
    write_stdout(&ProbeResult {
        absolute_write_blocked,
        parent_write_blocked,
        host_path_unreadable,
        credential_environment_absent,
        network_blocked,
        open_fds,
    })
    .await
}

/// Hidden volume copy helper.
pub async fn copy_worker() -> Result<(), DockerError> {
    let _: () = read_stdin().await?;
    copy_tree(Path::new("/source"), Path::new("/destination"))?;
    write_stdout(&()).await
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), DockerError> {
    if fs::read_dir(destination)?.next().is_some() {
        return Err(DockerError("copy destination is not empty".into()));
    }
    copy_directory(source, destination)
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), DockerError> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_dir() {
            fs::create_dir(&destination_path)?;
            fs::set_permissions(
                &destination_path,
                fs::Permissions::from_mode(metadata.permissions().mode()),
            )?;
            copy_directory(&source_path, &destination_path)?;
        } else if metadata.file_type().is_file() {
            fs::copy(&source_path, &destination_path)?;
            fs::set_permissions(
                &destination_path,
                fs::Permissions::from_mode(metadata.permissions().mode()),
            )?;
        } else if metadata.file_type().is_symlink() {
            symlink(fs::read_link(&source_path)?, &destination_path)?;
        } else {
            return Err(DockerError(format!(
                "unsupported copy entry: {}",
                source_path.display()
            )));
        }
    }
    Ok(())
}

async fn read_stdin<T: DeserializeOwned>() -> Result<T, DockerError> {
    read_frame(&mut tokio::io::stdin())
        .await?
        .ok_or_else(|| DockerError("helper received no input frame".into()))
}

async fn write_stdout<T: Serialize>(value: &T) -> Result<(), DockerError> {
    write_frame(&mut tokio::io::stdout(), value)
        .await
        .map_err(Into::into)
}

/// Minimal context for a one-shot original production tool closure.
struct MinimalToolContext {
    working_directory: PathBuf,
    todos: Vec<TodoItem>,
    cancellation: CancellationToken,
    registry: TaskRegistry,
}

impl MinimalToolContext {
    fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            todos: Vec::new(),
            cancellation: CancellationToken::new(),
            registry: TaskRegistry::default(),
        }
    }
}

impl ToolContext for MinimalToolContext {
    fn working_directory(&self) -> PathBuf {
        self.working_directory.clone()
    }

    fn get_todo_list(&self) -> Vec<TodoItem> {
        self.todos.clone()
    }

    fn set_todo_list(&mut self, todos: Vec<TodoItem>) {
        self.todos = todos;
    }

    fn spawn_agent<'a>(
        &'a mut self,
        _task: String,
        _mode: SpawnMode,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<SpawnResult, aj_agent::BoxError>> + Send + 'a>,
    > {
        Box::pin(async { Err("sub-agents are disabled in tool workers".into()) })
    }

    fn emit_update<'a>(
        &'a mut self,
        _partial: ToolDetails,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    fn task_registry(&self) -> TaskRegistry {
        self.registry.clone()
    }

    fn agent_id(&self) -> AgentId {
        AgentId::Main
    }

    fn start_background_task(
        &mut self,
        _kind: TaskKind,
        _label: String,
        _output: Arc<dyn TaskOutputSource>,
    ) -> StartedTask {
        panic!("background tasks are rejected before one-shot execution")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_contained_command_has_a_valid_unique_parent_name_and_no_auto_remove() {
        let first = contained_command("sha256:image", "__worker");
        let second = contained_command("sha256:image", "__worker");
        assert_ne!(first.name(), second.name());
        validate_container_name(first.name()).unwrap();
        let arguments = first
            .command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair[0] == "--name" && pair[1] == first.name() })
        );
        assert!(!arguments.iter().any(|argument| argument == "--rm"));
    }

    #[test]
    fn tool_outcome_wire_round_trips() {
        let value = ToolOutcomeWire {
            content: vec![UserContent::text("ok")],
            details: serde_json::json!({"kind": "text", "body": "ok"}),
            is_error: false,
        };
        let bytes = serde_json::to_vec(&value).unwrap();
        let decoded: ToolOutcomeWire = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(wire_text(&decoded), "ok");
        assert!(!decoded.is_error);
    }

    #[tokio::test]
    async fn background_bash_is_rejected_before_execution() {
        let outcome = execute_tool(ToolWorkerInput {
            name: "bash".into(),
            arguments: serde_json::json!({
                "command": "touch should-not-exist",
                "description": "test",
                "run_in_background": true,
                "timeout": 1
            }),
        })
        .await;
        assert!(outcome.is_error);
        assert!(wire_text(&outcome).contains("background"));
    }

    #[tokio::test]
    #[ignore = "requires AJ_EVAL_IMAGE pointing at an immutable built evaluator image"]
    async fn docker_isolation_contract() {
        let image = std::env::var("AJ_EVAL_IMAGE").expect("AJ_EVAL_IMAGE");
        preflight(&image).await.unwrap();
    }
}
