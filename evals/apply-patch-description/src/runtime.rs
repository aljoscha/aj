//! Typed live-run records, accounting, and pair admission policy.

use std::collections::BTreeMap;

use aj_agent::events::AgentEvent;
use aj_agent::message::AgentMessageKind;
use aj_models::types::{AssistantContent, ErrorCategory, Message, StopReason, Usage, UserContent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fixtures::{CommandResult, VerificationReport};
use crate::snapshot::{FilesystemSnapshot, SnapshotDelta};

/// Maximum fresh isolated attempts allowed for one scheduled pair.
pub const MAX_PAIR_ATTEMPTS: usize = 32;

/// Ordered terminal taxonomy for one trial.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    RunnerInternal,
    InfrastructureFailed,
    Cancelled,
    TimedOut,
    TurnLimit,
    ModelFailed,
    VerifierFailed,
    Passed,
}

impl TerminalStatus {
    /// Whether the trial belongs in the intent-to-treat denominator.
    pub fn valid(self) -> bool {
        !matches!(
            self,
            Self::RunnerInternal | Self::InfrastructureFailed | Self::Cancelled
        )
    }
}

/// Worker-side conclusion before the independent verifier runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTerminal {
    Completed,
    TurnLimit,
    Cancelled,
    ModelFailed,
    RunnerInternal,
}

/// Exhaustive classification of an attempted patch call.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchClassification {
    SchemaError,
    PartialApplication,
    Success,
    FormatError,
    Rejected,
    ApplicationError,
}

impl PatchClassification {
    pub fn failed(self) -> bool {
        self != Self::Success
    }
}

/// Attribution assigned to one snapshot-bracketed tool invocation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationAttribution {
    ApplyPatch,
    NonPatchTool,
    NoMutation,
    BetweenBoundaries,
}

/// Ordered parent-owned mutation record.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MutationLedgerEntry {
    pub sequence: u64,
    pub request_id: Option<u64>,
    pub tool: Option<String>,
    pub arguments_sha256: Option<String>,
    pub attribution: MutationAttribution,
    pub delta: SnapshotDelta,
}

/// Parent-owned record for one model-requested patch call.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PatchCallRecord {
    pub sequence: u64,
    pub request_id: u64,
    pub arguments_sha256: String,
    pub invoked: bool,
    pub is_error: bool,
    pub result_text: String,
    pub classification: PatchClassification,
    pub delta: SnapshotDelta,
}

/// Exact parent-received outcome from one brokered production tool closure.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolOutcomeRecord {
    pub request_id: u64,
    pub tool: String,
    pub content: Vec<UserContent>,
    pub details: Value,
    pub is_error: bool,
}

/// Usage-field provenance that cannot be recovered from normalized `Usage`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UsageFieldPresence {
    pub input: Option<bool>,
    pub output: Option<bool>,
    pub cache_read: Option<bool>,
    pub cache_write: Option<bool>,
    pub source: String,
}

/// Cache regime derived from provider-reported normalized usage.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStratum {
    ZeroRead,
    PositiveRead,
    UnknownRead,
}

/// Cost range used when cache-write tokens are unavailable.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CacheWriteSensitivity {
    pub lower_aj_recorded_catalog_cost: f64,
    pub upper_aj_recorded_catalog_cost: f64,
    pub upper_assumed_cache_write_tokens: u64,
}

/// Fixed limits enforced or requested for one trial.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeLimits {
    pub wall_timeout_seconds: u64,
    pub max_provider_requests: u32,
    pub max_model_responses: u32,
    pub provider_output_token_ceiling: u64,
    pub aggregate_observed_output_token_ceiling: u64,
}

/// Frozen evaluator source state used to build and run this trial.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceProvenance {
    pub head: String,
    pub dirty: bool,
    pub worktree_hash: Option<String>,
}

impl SourceProvenance {
    /// Formats a revision without representing a dirty tree as clean `HEAD`.
    pub fn revision_label(&self) -> String {
        match &self.worktree_hash {
            Some(hash) if self.dirty => format!("{}+dirty.{hash}", self.head),
            _ => self.head.clone(),
        }
    }
}

/// Structured terminal error reported by the production provider.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderErrorRecord {
    pub category: ErrorCategory,
    pub message: String,
    pub retry_after_ms: Option<u64>,
    pub http_status: Option<u16>,
}

/// Event accounting collected inline by the worker.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct WorkerMetrics {
    pub usage: Usage,
    pub model_responses: u64,
    pub tool_rounds: u64,
    pub total_tool_calls: u64,
    pub tool_calls_by_name: BTreeMap<String, u64>,
    pub apply_patch_attempts: u64,
    pub apply_patch_failures: u64,
    pub recovery_rounds: u64,
    pub stream_retries: u64,
    pub final_assistant_text: String,
    pub provider_errors: Vec<String>,
    pub transcript_wire_messages: Vec<Message>,
}

/// Typed result returned by the disposable agent worker.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkerResult {
    pub terminal: WorkerTerminal,
    pub error: Option<String>,
    pub metrics: WorkerMetrics,
    pub registry_quiescent: bool,
}

/// Independent verifier artifacts.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VerifierRecord {
    pub report: VerificationReport,
    pub command_result: Option<CommandResult>,
    pub before_root_hash: String,
    pub after_root_hash: String,
    pub mutations: SnapshotDelta,
}

/// Complete typed runtime payload stored in `TrialRecord::runtime`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeRecord {
    pub terminal_status: TerminalStatus,
    pub valid: bool,
    pub task_passed: bool,
    pub sessions_with_patch_failure: bool,
    pub edit_bypass: bool,
    pub aj_recorded_catalog_cost: f64,
    pub model_responses: u64,
    pub provider_requests: u32,
    pub usage: Usage,
    pub usage_field_presence: UsageFieldPresence,
    pub cache_stratum: CacheStratum,
    pub cache_write_sensitivity: CacheWriteSensitivity,
    pub limits: RuntimeLimits,
    pub first_response_aj_recorded_catalog_cost: Option<f64>,
    pub tool_rounds: u64,
    pub total_tool_calls: u64,
    pub tool_calls_by_name: BTreeMap<String, u64>,
    pub apply_patch_attempts: u64,
    pub successful_patch_calls: u64,
    pub recovery_rounds: u64,
    pub stream_retries: u64,
    pub duration_millis: u64,
    pub final_assistant_text: String,
    pub prompt: String,
    pub verifier_command: Option<Vec<String>>,
    pub patch_calls: Vec<PatchCallRecord>,
    pub tool_outcomes: Vec<ToolOutcomeRecord>,
    pub mutation_ledger: Vec<MutationLedgerEntry>,
    pub baseline_root_hash: Option<String>,
    pub final_snapshot: Option<FilesystemSnapshot>,
    pub final_snapshot_blob: Option<String>,
    pub final_delta: Option<SnapshotDelta>,
    pub baseline_commit: Option<String>,
    pub final_diff_blob: Option<String>,
    pub final_status_blob: Option<String>,
    pub changed_paths: Vec<String>,
    pub verifier: Option<VerifierRecord>,
    pub payload_hashes: Vec<String>,
    pub normalized_model_context_hashes: Vec<String>,
    pub normalized_first_request_hash: Option<String>,
    pub system_prompt_hash: String,
    pub cache_key_hash: String,
    pub transcript_wire_messages: Vec<Message>,
    pub conversation_jsonl_blob: Option<String>,
    pub provider_errors: Vec<String>,
    pub provider_error_details: Vec<ProviderErrorRecord>,
    pub worker_error: Option<String>,
    pub containment_cleanup_confirmed: bool,
    pub isolation_contract: String,
    pub evaluator_api_limitations: Vec<String>,
    pub image_id: String,
    pub source_provenance: SourceProvenance,
    pub utc_date: String,
    pub conservative_catalog_pair_reserve: f64,
    pub final_assistant_text_blob: Option<String>,
}

impl RuntimeRecord {
    /// Whether this record can participate in a durable completed pair.
    pub fn completion_eligible(&self) -> bool {
        self.valid
            && self.valid == self.terminal_status.valid()
            && self.task_passed == (self.terminal_status == TerminalStatus::Passed)
            && self.stream_retries == 0
            && self.provider_errors.is_empty()
            && self.provider_error_details.is_empty()
    }
}

/// Mutable worker-side event collector. The event bus invokes this inline.
#[derive(Debug, Default)]
pub struct EventCollector {
    metrics: WorkerMetrics,
    patch_failure_seen: bool,
}

impl EventCollector {
    pub fn observe(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageEnd { message, .. } => {
                if let AgentMessageKind::Wire(wire) = &message.kind {
                    self.metrics.transcript_wire_messages.push(wire.clone());
                    if let Message::Assistant(assistant) = wire {
                        self.metrics.usage.accumulate(&assistant.usage);
                        self.metrics.model_responses += 1;
                        if assistant
                            .content
                            .iter()
                            .any(|block| matches!(block, AssistantContent::ToolCall(_)))
                        {
                            self.metrics.tool_rounds += 1;
                        }
                        if self.patch_failure_seen {
                            self.metrics.recovery_rounds += 1;
                        }
                        self.metrics.final_assistant_text = assistant
                            .content
                            .iter()
                            .filter_map(|block| match block {
                                AssistantContent::Text(text) => Some(text.text.as_str()),
                                _ => None,
                            })
                            .collect::<String>();
                        if matches!(
                            assistant.stop_reason,
                            StopReason::Error | StopReason::Aborted
                        ) && let Some(error) = &assistant.error
                        {
                            self.metrics.provider_errors.push(error.message.clone());
                        }
                    }
                }
            }
            AgentEvent::ToolExecutionStart { tool, .. } => {
                self.metrics.total_tool_calls += 1;
                *self
                    .metrics
                    .tool_calls_by_name
                    .entry(tool.clone())
                    .or_default() += 1;
                if tool == "apply_patch" {
                    self.metrics.apply_patch_attempts += 1;
                }
            }
            AgentEvent::ToolExecutionEnd { tool, is_error, .. }
                if tool == "apply_patch" && *is_error =>
            {
                self.metrics.apply_patch_failures += 1;
                self.patch_failure_seen = true;
            }
            AgentEvent::StreamRetry { error, .. } => {
                self.metrics.stream_retries += 1;
                self.metrics.provider_errors.push(error.clone());
            }
            _ => {}
        }
    }

    pub fn finish(self) -> WorkerMetrics {
        self.metrics
    }
}

/// Pair-only admission decision used before starting either member.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionDecision {
    Admit,
    TrialLimit,
    Budget,
}

pub fn admit_pair(
    started_trials: u64,
    max_trials: u64,
    spent: f64,
    max_cost: f64,
    pair_reserve: f64,
) -> AdmissionDecision {
    if started_trials.saturating_add(2) > max_trials {
        AdmissionDecision::TrialLimit
    } else if spent + pair_reserve > max_cost {
        AdmissionDecision::Budget
    } else {
        AdmissionDecision::Admit
    }
}

/// Parses the analyzer's required projection from a full runtime record.
pub fn analyzer_projection(
    record: &RuntimeRecord,
) -> Result<crate::analysis::RuntimeMetrics, String> {
    let value: Value = serde_json::to_value(record).map_err(|error| error.to_string())?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

#[cfg(test)]
use crate::snapshot::{EntryKind, SnapshotEntry, delta};

#[cfg(test)]
fn completed_snapshot_fixture(path: &str, hash: &str, root_hash: &str) -> FilesystemSnapshot {
    FilesystemSnapshot {
        entries: vec![SnapshotEntry {
            path: path.into(),
            kind: EntryKind::File,
            unix_mode: 0o644,
            file_length: Some(1),
            file_sha256: Some(hash.into()),
            symlink_target: None,
            symlink_target_sha256: None,
        }],
        root_hash: root_hash.into(),
    }
}

#[cfg(test)]
pub(crate) fn completed_runtime_fixture() -> RuntimeRecord {
    let baseline = completed_snapshot_fixture("file", "before", "baseline");
    let final_snapshot = completed_snapshot_fixture("file", "after", "final");
    RuntimeRecord {
        terminal_status: TerminalStatus::Passed,
        valid: true,
        task_passed: true,
        sessions_with_patch_failure: false,
        edit_bypass: false,
        aj_recorded_catalog_cost: 0.25,
        model_responses: 2,
        provider_requests: 2,
        usage: Usage::default(),
        usage_field_presence: UsageFieldPresence {
            input: Some(true),
            output: Some(true),
            cache_read: Some(true),
            cache_write: None,
            source: "test".into(),
        },
        cache_stratum: CacheStratum::ZeroRead,
        cache_write_sensitivity: CacheWriteSensitivity {
            lower_aj_recorded_catalog_cost: 0.25,
            upper_aj_recorded_catalog_cost: 0.3,
            upper_assumed_cache_write_tokens: 10,
        },
        limits: RuntimeLimits {
            wall_timeout_seconds: 10,
            max_provider_requests: 2,
            max_model_responses: 2,
            provider_output_token_ceiling: 100,
            aggregate_observed_output_token_ceiling: 100,
        },
        first_response_aj_recorded_catalog_cost: Some(0.1),
        tool_rounds: 1,
        total_tool_calls: 1,
        tool_calls_by_name: BTreeMap::from([("apply_patch".into(), 1)]),
        apply_patch_attempts: 1,
        successful_patch_calls: 1,
        recovery_rounds: 0,
        stream_retries: 0,
        duration_millis: 10,
        final_assistant_text: "done".into(),
        prompt: "change file".into(),
        verifier_command: None,
        patch_calls: Vec::new(),
        tool_outcomes: Vec::new(),
        mutation_ledger: Vec::new(),
        baseline_root_hash: Some(baseline.root_hash.clone()),
        final_snapshot: Some(final_snapshot.clone()),
        final_snapshot_blob: Some("blob".into()),
        final_delta: Some(delta(&baseline, &final_snapshot)),
        baseline_commit: Some("commit".into()),
        final_diff_blob: Some("diff".into()),
        final_status_blob: Some("status".into()),
        changed_paths: vec!["file".into()],
        verifier: None,
        payload_hashes: Vec::new(),
        normalized_model_context_hashes: vec!["context".into()],
        normalized_first_request_hash: Some("context".into()),
        system_prompt_hash: "system".into(),
        cache_key_hash: "cache".into(),
        transcript_wire_messages: Vec::new(),
        conversation_jsonl_blob: Some("transcript".into()),
        provider_errors: Vec::new(),
        provider_error_details: Vec::new(),
        worker_error: None,
        containment_cleanup_confirmed: true,
        isolation_contract: "test".into(),
        evaluator_api_limitations: Vec::new(),
        image_id: "sha256:image".into(),
        source_provenance: SourceProvenance {
            head: "head".into(),
            dirty: true,
            worktree_hash: Some("dirty".into()),
        },
        utc_date: "2026-07-24".into(),
        conservative_catalog_pair_reserve: 1.0,
        final_assistant_text_blob: Some("final-text".into()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aj_agent::events::AgentId;
    use aj_agent::message::AgentMessage;
    use aj_models::types::{AssistantError, AssistantMessage, ErrorCategory, UsageCost};

    use super::*;
    use crate::snapshot::{EntryKind, SnapshotEntry, delta};

    fn assistant(reason: StopReason, input: u64) -> AgentEvent {
        let mut message = AssistantMessage::empty();
        message.stop_reason = reason;
        message.usage = Usage {
            input,
            output: 1,
            total_tokens: input + 1,
            cost: UsageCost {
                total: 0.01,
                ..UsageCost::default()
            },
            ..Usage::default()
        };
        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
            message.error = Some(AssistantError::new(ErrorCategory::Transient, "failed"));
        }
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::Assistant(message)),
        }
    }

    #[test]
    fn event_accounting_includes_success_error_aborted_and_retry_usage() {
        let mut collector = EventCollector::default();
        collector.observe(&assistant(StopReason::Stop, 2));
        collector.observe(&assistant(StopReason::Error, 3));
        collector.observe(&assistant(StopReason::Aborted, 5));
        collector.observe(&AgentEvent::StreamRetry {
            agent_id: AgentId::Main,
            attempt: 1,
            delay: Duration::ZERO,
            error: "retry".into(),
        });
        let metrics = collector.finish();
        assert_eq!(metrics.model_responses, 3);
        assert_eq!(metrics.usage.input, 10);
        assert_eq!(metrics.stream_retries, 1);
        assert_eq!(metrics.provider_errors.len(), 3);
    }

    #[test]
    fn pair_admission_never_splits_a_pair() {
        assert_eq!(admit_pair(0, 2, 0.0, 1.0, 1.0), AdmissionDecision::Admit);
        assert_eq!(
            admit_pair(2, 3, 0.0, 1.0, 0.1),
            AdmissionDecision::TrialLimit
        );
        assert_eq!(admit_pair(0, 2, 0.9, 1.0, 0.2), AdmissionDecision::Budget);
    }

    #[test]
    fn dirty_source_revision_is_never_formatted_as_clean_head() {
        let clean = SourceProvenance {
            head: "abc".into(),
            dirty: false,
            worktree_hash: None,
        };
        let dirty = SourceProvenance {
            head: "abc".into(),
            dirty: true,
            worktree_hash: Some("def".into()),
        };
        assert_eq!(clean.revision_label(), "abc");
        assert_eq!(dirty.revision_label(), "abc+dirty.def");
        assert_ne!(dirty.revision_label(), dirty.head);
    }

    fn snapshot(path: &str, hash: &str, root_hash: &str) -> FilesystemSnapshot {
        FilesystemSnapshot {
            entries: vec![SnapshotEntry {
                path: path.into(),
                kind: EntryKind::File,
                unix_mode: 0o644,
                file_length: Some(1),
                file_sha256: Some(hash.into()),
                symlink_target: None,
                symlink_target_sha256: None,
            }],
            root_hash: root_hash.into(),
        }
    }

    fn runtime_record() -> RuntimeRecord {
        let baseline = snapshot("file", "before", "baseline");
        let final_snapshot = snapshot("file", "after", "final");
        RuntimeRecord {
            terminal_status: TerminalStatus::Passed,
            valid: true,
            task_passed: true,
            sessions_with_patch_failure: false,
            edit_bypass: false,
            aj_recorded_catalog_cost: 0.25,
            model_responses: 2,
            provider_requests: 2,
            usage: Usage::default(),
            usage_field_presence: UsageFieldPresence {
                input: Some(true),
                output: Some(true),
                cache_read: Some(true),
                cache_write: None,
                source: "test".into(),
            },
            cache_stratum: CacheStratum::ZeroRead,
            cache_write_sensitivity: CacheWriteSensitivity {
                lower_aj_recorded_catalog_cost: 0.25,
                upper_aj_recorded_catalog_cost: 0.3,
                upper_assumed_cache_write_tokens: 10,
            },
            limits: RuntimeLimits {
                wall_timeout_seconds: 10,
                max_provider_requests: 2,
                max_model_responses: 2,
                provider_output_token_ceiling: 100,
                aggregate_observed_output_token_ceiling: 100,
            },
            first_response_aj_recorded_catalog_cost: Some(0.1),
            tool_rounds: 1,
            total_tool_calls: 1,
            tool_calls_by_name: BTreeMap::from([("apply_patch".into(), 1)]),
            apply_patch_attempts: 1,
            successful_patch_calls: 1,
            recovery_rounds: 0,
            stream_retries: 0,
            duration_millis: 10,
            final_assistant_text: "done".into(),
            prompt: "change file".into(),
            verifier_command: None,
            patch_calls: Vec::new(),
            tool_outcomes: Vec::new(),
            mutation_ledger: Vec::new(),
            baseline_root_hash: Some(baseline.root_hash.clone()),
            final_snapshot: Some(final_snapshot.clone()),
            final_snapshot_blob: Some("blob".into()),
            final_delta: Some(delta(&baseline, &final_snapshot)),
            baseline_commit: Some("commit".into()),
            final_diff_blob: Some("diff".into()),
            final_status_blob: Some("status".into()),
            changed_paths: vec!["file".into()],
            verifier: None,
            payload_hashes: Vec::new(),
            normalized_model_context_hashes: vec!["context".into()],
            normalized_first_request_hash: Some("context".into()),
            system_prompt_hash: "system".into(),
            cache_key_hash: "cache".into(),
            transcript_wire_messages: Vec::new(),
            conversation_jsonl_blob: Some("transcript".into()),
            provider_errors: Vec::new(),
            provider_error_details: Vec::new(),
            worker_error: None,
            containment_cleanup_confirmed: true,
            isolation_contract: "test".into(),
            evaluator_api_limitations: Vec::new(),
            image_id: "sha256:image".into(),
            source_provenance: SourceProvenance {
                head: "head".into(),
                dirty: true,
                worktree_hash: Some("dirty".into()),
            },
            utc_date: "2026-07-24".into(),
            conservative_catalog_pair_reserve: 1.0,
            final_assistant_text_blob: Some("final-text".into()),
        }
    }

    #[test]
    fn completion_eligibility_requires_consistent_clean_runtime() {
        let mut record = runtime_record();
        assert!(record.completion_eligible());
        record.stream_retries = 1;
        assert!(!record.completion_eligible());
        record.stream_retries = 0;
        record.terminal_status = TerminalStatus::InfrastructureFailed;
        assert!(!record.completion_eligible());
        record.terminal_status = TerminalStatus::Passed;
        record.task_passed = false;
        assert!(!record.completion_eligible());
    }

    #[test]
    fn full_runtime_record_parses_through_analyzer_projection() {
        let record = runtime_record();
        assert!(record.baseline_commit.is_some());
        assert!(record.final_diff_blob.is_some());
        assert!(record.final_status_blob.is_some());
        assert_eq!(record.changed_paths, ["file"]);
        assert!(record.conversation_jsonl_blob.is_some());
        let projection = analyzer_projection(&record).unwrap();
        assert!(projection.task_passed);
        assert_eq!(projection.aj_recorded_catalog_cost, 0.25);
        assert_eq!(projection.model_responses, 2);
    }

    #[test]
    fn verifier_mutations_do_not_replace_agent_delta() {
        let mut record = runtime_record();
        let verifier_after = snapshot("verifier-output", "output", "verifier-after");
        let final_snapshot = record.final_snapshot.as_ref().unwrap();
        let verifier_mutations = delta(final_snapshot, &verifier_after);
        let final_root_hash = final_snapshot.root_hash.clone();
        record.verifier = Some(VerifierRecord {
            report: VerificationReport {
                passed: false,
                reasons: vec!["verifier mutated state".into()],
                changed_path_allowlist: crate::fixtures::ChangedPathAllowlistResult {
                    passed: false,
                    allowed_paths: vec!["file".into()],
                    changed_paths: vec!["file".into()],
                    disallowed_paths: Vec::new(),
                },
                visible_check: crate::fixtures::VisibleCheckMetadata {
                    request: None,
                    outcome: crate::fixtures::VisibleCheckOutcome::NotRequired,
                    result: None,
                },
                hidden_check: crate::fixtures::HiddenCheckMetadata {
                    contract_passed: false,
                    behavior_result: None,
                },
            },
            command_result: None,
            before_root_hash: final_root_hash,
            after_root_hash: verifier_after.root_hash,
            mutations: verifier_mutations,
        });
        assert_eq!(record.final_delta.as_ref().unwrap().paths[0].path, "file");
        assert!(
            record
                .verifier
                .unwrap()
                .mutations
                .paths
                .iter()
                .any(|change| change.path == "verifier-output")
        );
    }
}
