//! Bounded length-prefixed protocol used over Docker attach streams.

use std::fmt;

use aj_models::streaming::AssistantMessageEvent;
use aj_models::types::{Context, ThinkingLevel, UserContent};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::descriptions::DescriptionVariant;
use crate::fixtures::{CommandResult, GeneratedFixture, VerificationReport};
use crate::runtime::WorkerResult;
use crate::schedule::TaskInstance;
use crate::snapshot::{FilesystemSnapshot, SnapshotDelta};

/// Largest accepted frame. Context and image-bearing tool results must fit this cap.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Framing, serialization, or protocol error.
#[derive(Debug)]
pub struct ProtocolError(pub String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

impl From<std::io::Error> for ProtocolError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

/// Sanitized model metadata needed to construct the guest `Agent`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkerModel {
    pub id: String,
    pub name: String,
    pub family: Option<String>,
    pub api: String,
    pub provider: String,
    pub reasoning: bool,
    pub reasoning_options: Vec<aj_models::registry::ReasoningOption>,
    pub supports_verbosity: bool,
    pub input: Vec<aj_models::registry::InputModality>,
    pub cost: aj_models::registry::ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
}

/// Trusted parent configuration sent before the worker starts its agent.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkerInit {
    pub model: WorkerModel,
    pub reasoning: ThinkingLevel,
    pub variant: DescriptionVariant,
    pub prompt: String,
    pub session_id: String,
    pub utc_date: String,
    pub max_model_responses: u32,
    pub max_output_tokens: u64,
}

/// Requests emitted by the trial worker on its attached stdout.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerRequest {
    Provider {
        id: u64,
        context: Context,
        observed_reasoning: ThinkingLevel,
    },
    Tool {
        id: u64,
        name: String,
        arguments: Value,
    },
    Finished {
        result: WorkerResult,
    },
}

/// Responses sent by the trusted parent on the worker's attached stdin.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParentResponse {
    ProviderEvent {
        id: u64,
        event: AssistantMessageEvent,
    },
    ToolResult {
        id: u64,
        outcome: ToolOutcomeWire,
    },
    Failure {
        id: u64,
        error: String,
    },
}

impl ParentResponse {
    /// Correlation id used by the guest's response dispatcher.
    pub fn id(&self) -> u64 {
        match self {
            Self::ProviderEvent { id, .. }
            | Self::ToolResult { id, .. }
            | Self::Failure { id, .. } => *id,
        }
    }
}

/// Serializable projection of `ToolOutcome` across the process boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolOutcomeWire {
    pub content: Vec<UserContent>,
    pub details: Value,
    pub is_error: bool,
}

/// Input to a one-shot production tool container.
#[derive(Debug, Deserialize, Serialize)]
pub struct ToolWorkerInput {
    pub name: String,
    pub arguments: Value,
}

/// Output from fixture materialization.
#[derive(Debug, Deserialize, Serialize)]
pub struct FixtureWorkerOutput {
    pub fixture: GeneratedFixture,
    pub baseline_commit: String,
}

/// Input to fixture materialization.
#[derive(Debug, Deserialize, Serialize)]
pub struct FixtureWorkerInput {
    pub instance: TaskInstance,
}

/// Input to the trusted snapshot and Git-artifact helper.
#[derive(Debug, Deserialize, Serialize)]
pub struct SnapshotWorkerInput {
    pub include_git_artifacts: bool,
    pub baseline_commit: Option<String>,
}

/// Bounded reconstructable Git state captured from the fixture repository.
#[derive(Debug, Deserialize, Serialize)]
pub struct GitArtifacts {
    pub diff: Vec<u8>,
    pub status: Vec<u8>,
}

/// Snapshot helper output with optional final Git artifacts.
#[derive(Debug, Deserialize, Serialize)]
pub struct SnapshotWorkerOutput {
    pub snapshot: FilesystemSnapshot,
    pub git: Option<GitArtifacts>,
}

/// Input to the isolated verifier.
#[derive(Debug, Deserialize, Serialize)]
pub struct VerifyWorkerInput {
    pub instance: TaskInstance,
}

/// Isolated verifier output, with verifier state kept separate from agent state.
#[derive(Debug, Deserialize, Serialize)]
pub struct VerifyWorkerOutput {
    pub report: VerificationReport,
    pub command_result: Option<CommandResult>,
    pub before: FilesystemSnapshot,
    pub after: FilesystemSnapshot,
    pub mutations: SnapshotDelta,
}

/// Result of mandatory in-guest isolation probes.
#[derive(Debug, Deserialize, Serialize)]
pub struct ProbeResult {
    pub absolute_write_blocked: bool,
    pub parent_write_blocked: bool,
    pub host_path_unreadable: bool,
    pub credential_environment_absent: bool,
    pub network_blocked: bool,
    pub open_fds: Vec<i32>,
}

/// Host-only path that must remain invisible inside an isolation probe.
#[derive(Debug, Deserialize, Serialize)]
pub struct ProbeInput {
    pub host_path: String,
}

/// Writes one bounded JSON frame with a big-endian u32 length prefix.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ProtocolError(format!("cannot serialize protocol frame: {error}")))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError(format!(
            "protocol frame is {} bytes, limit is {MAX_FRAME_BYTES}",
            bytes.len()
        )));
    }
    let length = u32::try_from(bytes.len())
        .map_err(|_| ProtocolError("protocol frame length exceeds u32".into()))?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one bounded JSON frame. EOF before a prefix is clean, truncation is not.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>, ProtocolError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut prefix = [0_u8; 4];
    match reader.read(&mut prefix[..1]).await {
        Ok(0) => return Ok(None),
        Ok(1) => {}
        Ok(_) => unreachable!("one-byte read returned more than one byte"),
        Err(error) => return Err(error.into()),
    }
    reader
        .read_exact(&mut prefix[1..])
        .await
        .map_err(|error| ProtocolError(format!("truncated protocol length prefix: {error}")))?;
    let length = usize::try_from(u32::from_be_bytes(prefix))
        .map_err(|_| ProtocolError("protocol frame length does not fit usize".into()))?;
    if length > MAX_FRAME_BYTES {
        return Err(ProtocolError(format!(
            "protocol frame declares {length} bytes, limit is {MAX_FRAME_BYTES}"
        )));
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|error| ProtocolError(format!("truncated protocol payload: {error}")))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| ProtocolError(format!("invalid protocol JSON: {error}")))
}

#[cfg(test)]
mod tests {
    use aj_models::types::{AssistantMessage, StopReason, Usage, UsageCost};
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;
    use crate::runtime::{WorkerMetrics, WorkerTerminal};

    fn fractional_usage() -> Usage {
        Usage {
            input: 297,
            output: 39,
            cache_read: 2560,
            cache_write: 73,
            total_tokens: 2969,
            cost: UsageCost {
                input: (5.0 / 1_000_000.0) * 297.0,
                output: (30.0 / 1_000_000.0) * 39.0,
                cache_read: (0.5 / 1_000_000.0) * 2560.0,
                cache_write: (6.25 / 1_000_000.0) * 73.0,
                total: 0.00439125,
            },
        }
    }

    fn assert_usage_bits_eq(actual: &Usage, expected: &Usage) {
        assert_eq!(actual.input, expected.input);
        assert_eq!(actual.output, expected.output);
        assert_eq!(actual.cache_read, expected.cache_read);
        assert_eq!(actual.cache_write, expected.cache_write);
        assert_eq!(actual.total_tokens, expected.total_tokens);
        for (actual, expected) in [
            (actual.cost.input, expected.cost.input),
            (actual.cost.output, expected.cost.output),
            (actual.cost.cache_read, expected.cost.cache_read),
            (actual.cost.cache_write, expected.cost.cache_write),
            (actual.cost.total, expected.cost.total),
        ] {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
    }

    #[tokio::test]
    async fn frames_round_trip_without_cross_request_confusion() {
        let (mut left, mut right) = duplex(1024);
        let writer = tokio::spawn(async move {
            write_frame(
                &mut left,
                &ParentResponse::Failure {
                    id: 9,
                    error: "first".into(),
                },
            )
            .await
            .unwrap();
            write_frame(
                &mut left,
                &ParentResponse::Failure {
                    id: 3,
                    error: "second".into(),
                },
            )
            .await
            .unwrap();
        });
        let first: ParentResponse = read_frame(&mut right).await.unwrap().unwrap();
        let second: ParentResponse = read_frame(&mut right).await.unwrap().unwrap();
        assert_eq!((first.id(), second.id()), (9, 3));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn parent_frames_preserve_usage_cost_bits() {
        let usage = fractional_usage();
        let response = ParentResponse::ProviderEvent {
            id: 1,
            event: AssistantMessageEvent::Start {
                partial: AssistantMessage {
                    content: Vec::new(),
                    api: "openai-codex-responses".into(),
                    provider: "openai-codex".into(),
                    model: "gpt-5.6-sol".into(),
                    account: None,
                    response_id: None,
                    usage: usage.clone(),
                    stop_reason: StopReason::Stop,
                    error: None,
                    timestamp: 0,
                },
            },
        };
        let (mut left, mut right) = duplex(4096);
        write_frame(&mut left, &response).await.unwrap();
        let decoded: ParentResponse = read_frame(&mut right).await.unwrap().unwrap();
        let ParentResponse::ProviderEvent {
            event: AssistantMessageEvent::Start { partial },
            ..
        } = decoded
        else {
            panic!("unexpected protocol response");
        };
        assert_usage_bits_eq(&partial.usage, &usage);
    }

    #[tokio::test]
    async fn worker_frames_preserve_usage_cost_bits() {
        let usage = fractional_usage();
        let request = WorkerRequest::Finished {
            result: WorkerResult {
                terminal: WorkerTerminal::Completed,
                error: None,
                metrics: WorkerMetrics {
                    usage: usage.clone(),
                    ..WorkerMetrics::default()
                },
                registry_quiescent: true,
            },
        };
        let (mut left, mut right) = duplex(4096);
        write_frame(&mut left, &request).await.unwrap();
        let decoded: WorkerRequest = read_frame(&mut right).await.unwrap().unwrap();
        let WorkerRequest::Finished { result } = decoded else {
            panic!("unexpected protocol request");
        };
        assert_usage_bits_eq(&result.metrics.usage, &usage);
    }

    #[tokio::test]
    async fn rejects_oversize_and_truncated_frames() {
        let (mut left, mut right) = duplex(64);
        left.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        assert!(read_frame::<_, Value>(&mut right).await.is_err());

        let (mut left, mut right) = duplex(64);
        left.write_all(&8_u32.to_be_bytes()).await.unwrap();
        left.write_all(b"{}").await.unwrap();
        drop(left);
        let error = read_frame::<_, Value>(&mut right).await.unwrap_err();
        assert!(error.to_string().contains("truncated protocol payload"));

        let (mut left, mut right) = duplex(64);
        left.write_all(&[0, 0]).await.unwrap();
        drop(left);
        let error = read_frame::<_, Value>(&mut right).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("truncated protocol length prefix")
        );
    }

    #[tokio::test]
    async fn writer_enforces_frame_limit() {
        let (mut left, _right) = duplex(8);
        let value = "x".repeat(MAX_FRAME_BYTES + 1);
        assert!(write_frame(&mut left, &value).await.is_err());
    }
}
