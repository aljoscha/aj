//! Trusted parent for live paired evaluation trials.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use aj_models::auth::AuthStorage;
use aj_models::openai::responses::responses_reasoning_effort;
use aj_models::provider::Provider;
use aj_models::registry::{ModelInfo, ModelRegistry, calculate_cost, validate_thinking_level};
use aj_models::streaming::AssistantMessageEvent;
use aj_models::types::{
    AssistantContent, Context, ErrorCategory, Message, OnPayload, SimpleStreamOptions,
    StreamOptions, ThinkingLevel, ToolDefinition, ToolResultMessage, Usage, UserMessage,
};
use aj_tools::tools::apply_patch::ApplyPatchInput;
use aj_tools::tools::bash::BashInput;
use aj_tools::{BuiltinToolOptions, builtin_tools_for_model};
use futures::StreamExt;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::artifacts::{
    ArtifactLog, BlobStore, PairCompletionIdentity, PairKey, RecordedDescription, TrialIdentity,
    TrialMetadata, TrialRecord, completed_pair, scan,
};
use crate::descriptions::{DescriptionVariant, load};
use crate::docker::{
    DockerError, FixtureVolume, copy_volume_cancellable, preflight, run_helper,
    run_helper_cancellable, validate_image, wire_text, worker_command,
};
use crate::fixtures::GeneratedFixture;
use crate::planning::validate_pilot_evidence;
use crate::protocol::{
    FixtureWorkerInput, FixtureWorkerOutput, GitArtifacts, MAX_FRAME_BYTES, ParentResponse,
    SnapshotWorkerInput, SnapshotWorkerOutput, ToolOutcomeWire, ToolWorkerInput, VerifyWorkerInput,
    VerifyWorkerOutput, WorkerInit, WorkerModel, WorkerRequest, read_frame, write_frame,
};
use crate::runtime::{
    AdmissionDecision, CacheStratum, CacheWriteSensitivity, MutationAttribution,
    MutationLedgerEntry, PatchCallRecord, PatchClassification, ProviderErrorRecord, RuntimeLimits,
    RuntimeRecord, SourceProvenance, TerminalStatus, ToolOutcomeRecord, UsageFieldPresence,
    VerifierRecord, WorkerMetrics, WorkerResult, WorkerTerminal, admit_pair,
};
use crate::schedule::{
    FrozenModelSelection, FrozenPlan, MainPlanning, PairScheduleRecord, SchedulePhase,
    TaskInstance, TrialScheduleRecord, validate_frozen_plan,
};
use crate::snapshot::{FilesystemSnapshot, SnapshotDelta, delta};
use crate::worker::initial_context;
use crate::{hash_framed, sha256_hex, suite};

const PROVIDER_ID: &str = "openai-codex";
#[cfg(test)]
const MODEL_ID: &str = "gpt-5.6-sol";
const PREFLIGHT_TOTAL_TIMEOUT: Duration = Duration::from_secs(360);

/// Parent-run configuration from the public `run` command.
pub struct RunOptions {
    pub phase: SchedulePhase,
    pub plan: PathBuf,
    pub records: PathBuf,
    pub artifact_dir: PathBuf,
    pub image: String,
    pub max_cost_usd: f64,
    pub max_trials: u64,
    pub timeout: Duration,
    pub max_model_responses: u32,
}

/// Live runner failure. No provider call is made after this is returned.
#[derive(Debug)]
pub struct RunnerError(pub String);

impl fmt::Display for RunnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RunnerError {}

impl From<std::io::Error> for RunnerError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<DockerError> for RunnerError {
    fn from(error: DockerError) -> Self {
        Self(error.to_string())
    }
}

struct TrustedModel {
    provider: Arc<dyn Provider>,
    model: Arc<ModelInfo>,
    stream_options: StreamOptions,
    catalog_hash: String,
    reasoning: ThinkingLevel,
}

#[derive(Clone)]
struct FrozenRunState {
    utc_date: String,
    image_id: String,
    source: SourceProvenance,
}

/// Runs mandatory isolation and image checks without making a provider call.
pub async fn run_preflight(image: &str) -> Result<(), RunnerError> {
    tokio::time::timeout(PREFLIGHT_TOTAL_TIMEOUT, async {
        let source = source_provenance().await?;
        let identity = validate_image(image).await?;
        if identity.source_provenance != source.revision_label() {
            return Err(RunnerError(format!(
                "image source provenance {} does not match host source provenance {}",
                identity.source_provenance,
                source.revision_label()
            )));
        }
        preflight(image).await.map_err(Into::into)
    })
    .await
    .map_err(|_| RunnerError("preflight exceeded its overall deadline".into()))?
}

/// Resolves and freezes a local model catalog entry without loading credentials.
pub fn freeze_model_selection(
    provider: &str,
    model_id: &str,
    reasoning: &str,
) -> Result<FrozenModelSelection, RunnerError> {
    let reasoning = parse_reasoning(reasoning)?;
    let registry = ModelRegistry::load();
    freeze_model_selection_from_registry(&registry, provider, model_id, reasoning)
}

fn freeze_model_selection_from_registry(
    registry: &ModelRegistry,
    provider: &str,
    model_id: &str,
    reasoning: ThinkingLevel,
) -> Result<FrozenModelSelection, RunnerError> {
    let model = registry.get(provider, model_id).ok_or_else(|| {
        RunnerError(format!(
            "model {provider}/{model_id} is not in the registry"
        ))
    })?;
    validate_thinking_level(model, &reasoning).map_err(RunnerError)?;
    if !matches!(
        model.api.as_str(),
        "openai-codex-responses" | "openai-responses"
    ) {
        return Err(RunnerError(format!(
            "model {provider}/{model_id} uses unsupported evaluation API {}",
            model.api
        )));
    }
    let tools = expected_tools(DescriptionVariant::Current, model.family.as_deref());
    if !tools.iter().any(|tool| tool.name == "apply_patch") {
        return Err(RunnerError(format!(
            "model {provider}/{model_id} uses a tool family without apply_patch"
        )));
    }
    let model_capability_hash = serde_json::to_vec(model)
        .map(|bytes| hash_framed(b"aj-apply-patch-eval-model-capability-v1", &[&bytes]))
        .map_err(|error| RunnerError(format!("cannot hash model capability: {error}")))?;
    let tool_catalog_hash = tool_catalog_hash(&tools)?;
    let mut selection = FrozenModelSelection {
        provider: provider.into(),
        model: model_id.into(),
        reasoning: reasoning.as_str().into(),
        catalog_hash: registry_hash(registry)?,
        catalog_source: registry.source_label().into(),
        catalog_updated_at: registry.updated_at,
        model_capability_hash,
        family: model.family.clone(),
        api: model.api.clone(),
        context_window: model.context_window,
        max_tokens: model.max_tokens,
        tool_catalog_hash,
        selection_hash: String::new(),
    };
    let bytes = serde_json::to_vec(&selection)
        .map_err(|error| RunnerError(format!("cannot hash model selection: {error}")))?;
    selection.selection_hash = hash_framed(b"aj-apply-patch-eval-model-selection-v1", &[&bytes]);
    Ok(selection)
}

fn parse_reasoning(value: &str) -> Result<ThinkingLevel, RunnerError> {
    match value {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::XHigh),
        "max" => Ok(ThinkingLevel::Max),
        _ => Err(RunnerError(format!(
            "unsupported reasoning level {value}. Expected off, minimal, low, medium, high, xhigh, or max"
        ))),
    }
}

fn frozen_model(plan: &FrozenPlan) -> Result<&FrozenModelSelection, RunnerError> {
    plan.model
        .as_ref()
        .ok_or_else(|| RunnerError("frozen plan has no model selection. Re-run freeze".into()))
}

/// Executes complete adjacent pairs from one frozen phase.
pub async fn run(options: RunOptions) -> Result<(), RunnerError> {
    if options.max_trials == 0 || options.max_trials % 2 != 0 {
        return Err(RunnerError(
            "--max-trials must be a positive even number".into(),
        ));
    }
    if options.max_cost_usd <= 0.0 || !options.max_cost_usd.is_finite() {
        return Err(RunnerError(
            "--max-cost-usd must be finite and positive".into(),
        ));
    }
    if options.max_model_responses == 0 {
        return Err(RunnerError("--max-model-responses must be positive".into()));
    }
    let source = source_provenance().await?;
    let image = validate_image(&options.image).await?;
    if image.source_provenance != source.revision_label() {
        return Err(RunnerError(format!(
            "image source provenance {} does not match host source provenance {}",
            image.source_provenance,
            source.revision_label()
        )));
    }
    run_preflight(&options.image).await?;
    let plan = load_plan(&options.plan)?;
    let state = scan(&options.records).map_err(|error| RunnerError(error.to_string()))?;
    let run_state = FrozenRunState {
        utc_date: frozen_utc_date(&plan, &state, options.phase)?,
        image_id: image.id,
        source,
    };
    match options.phase {
        SchedulePhase::Smoke => {
            require_unplanned_phase(&plan, "smoke")?;
        }
        SchedulePhase::Pilot => {
            require_unplanned_phase(&plan, "pilot")?;
            require_complete_pairs(
                &plan.schedule.smoke,
                &state,
                &plan.schedule.schedule_hash,
                "pilot phase requires every smoke pair",
            )?;
        }
        SchedulePhase::Main => {
            plan.require_planned_main()
                .map_err(|error| RunnerError(error.to_string()))?;
            validate_pilot_evidence(&plan, &options.records)
                .map_err(|error| RunnerError(error.to_string()))?;
        }
    }
    validate_resume_before_resolution(&plan, &state, &options, &run_state)?;
    let (model, catalog_hash, reasoning) = resolve_model_metadata(&plan)?;
    unpaid_request_preflight(&model, reasoning, &run_state.utc_date)?;
    let pairs = phase_pairs(&plan, options.phase);
    let incomplete = pairs
        .iter()
        .filter(|pair| {
            !state.complete_pairs.contains(&PairKey {
                run_id: pair.run_id.clone(),
                pair_id: pair.pair_id.clone(),
            })
        })
        .count();
    let pair_reserve = pair_reserve(options.phase, &plan, &model, options.max_model_responses)?;
    let mut spent = recorded_spend(&state, options.phase);
    if options.phase == SchedulePhase::Main
        && spent + pair_reserve * usize_as_f64(incomplete) > options.max_cost_usd
    {
        return Err(RunnerError(format!(
            "main run budget cannot reserve {pair_reserve:.6} USD for each of {incomplete} incomplete pairs"
        )));
    }
    if incomplete == 0
        || admit_pair(
            0,
            options.max_trials,
            spent,
            options.max_cost_usd,
            pair_reserve,
        ) != AdmissionDecision::Admit
    {
        return Ok(());
    }
    let trusted =
        match resolve_trusted_model(Arc::clone(&model), catalog_hash.clone(), reasoning).await {
            Ok(trusted) => trusted,
            Err(error) => {
                persist_resolution_failure(
                    &options,
                    &plan,
                    &state,
                    &run_state,
                    (&model, &catalog_hash, reasoning),
                    &error,
                )?;
                return Err(error);
            }
        };
    let revision = run_state.source.revision_label();
    let descriptions = recorded_descriptions();
    validate_resume_context(&plan, &state, &options, &trusted, &run_state)?;

    let blobs = BlobStore::new(options.artifact_dir.join("blobs"))
        .map_err(|error| RunnerError(error.to_string()))?;
    let mut log =
        ArtifactLog::open(&options.records).map_err(|error| RunnerError(error.to_string()))?;
    let run_cancel = CancellationToken::new();
    let signal_cancel = run_cancel.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let mut started_trials = 0_u64;
    for pair in pairs {
        let key = PairKey {
            run_id: pair.run_id.clone(),
            pair_id: pair.pair_id.clone(),
        };
        if state.complete_pairs.contains(&key) {
            continue;
        }
        match admit_pair(
            started_trials,
            options.max_trials,
            spent,
            options.max_cost_usd,
            pair_reserve,
        ) {
            AdmissionDecision::Admit => {}
            AdmissionDecision::TrialLimit | AdmissionDecision::Budget => break,
        }

        let instance = find_instance(&plan, pair)?;
        let prior_attempts = state
            .trials_by_hash
            .values()
            .filter(|record| {
                record.identity.run_id == pair.run_id && record.identity.pair_id == pair.pair_id
            })
            .map(|record| record.identity.attempt_id.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let attempts_allowed = 2_usize.saturating_sub(prior_attempts);
        if attempts_allowed == 0 {
            break;
        }
        let mut completed = false;
        for retry in 0..attempts_allowed {
            if retry > 0 {
                if run_cancel.is_cancelled()
                    || admit_pair(
                        started_trials,
                        options.max_trials,
                        spent,
                        options.max_cost_usd,
                        pair_reserve,
                    ) != AdmissionDecision::Admit
                {
                    break;
                }
            }
            let attempt_id = format!("{:032x}", rand::random::<u128>());
            let mut records = Vec::new();
            let mut retry_pair = false;
            let mut first_context_hash: Option<String> = None;
            for scheduled in &pair.trials {
                started_trials += 1;
                let mut execution = run_trial(
                    &options,
                    &trusted,
                    instance,
                    scheduled,
                    &attempt_id,
                    &blobs,
                    run_cancel.clone(),
                    &run_state,
                )
                .await?;
                if execution.valid && execution.provider_requests > 0 {
                    match (
                        first_context_hash.as_ref(),
                        execution.normalized_first_request_hash.as_ref(),
                    ) {
                        (None, Some(hash)) => first_context_hash = Some(hash.clone()),
                        (Some(first), Some(hash)) if first == hash => {}
                        (Some(_), Some(_)) => mark_runner_internal(
                            &mut execution,
                            "paired normalized first-request hashes differ",
                        ),
                        (_, None) => mark_runner_internal(
                            &mut execution,
                            "trial with a provider request has no normalized first-request hash",
                        ),
                    }
                }
                spent += execution.aj_recorded_catalog_cost;
                let metadata = TrialMetadata {
                    task_seed: instance.task_seed.clone(),
                    current_description: descriptions[0].clone(),
                    compact_description: descriptions[1].clone(),
                    aj_revision: revision.clone(),
                    suite_revision: plan.universe.suite_revision.clone(),
                    model_catalog_hash: trusted.catalog_hash.clone(),
                    provider: trusted.model.provider.clone(),
                    model: trusted.model.id.clone(),
                    reasoning_effort: trusted.reasoning.as_str().into(),
                    tool_catalog_hash: frozen_model(&plan)?.tool_catalog_hash.clone(),
                    fixture_revision: execution.baseline_root_hash.clone().unwrap_or_default(),
                };
                let record = TrialRecord::new(
                    trial_identity(&plan, pair, scheduled, &attempt_id),
                    metadata,
                    serde_json::to_value(&execution)
                        .map_err(|error| RunnerError(error.to_string()))?,
                )
                .map_err(|error| RunnerError(error.to_string()))?;
                log.append_trial(&record)
                    .map_err(|error| RunnerError(error.to_string()))?;
                if execution.terminal_status == TerminalStatus::RunnerInternal {
                    return Err(RunnerError(format!(
                        "runner_internal in trial {}",
                        scheduled.trial_identity_hash
                    )));
                }
                retry_pair |= matches!(
                    execution.terminal_status,
                    TerminalStatus::InfrastructureFailed | TerminalStatus::Cancelled
                );
                records.push(record);
            }
            if records.iter().all(|record| {
                serde_json::from_value::<RuntimeRecord>(record.runtime.clone())
                    .is_ok_and(|runtime| runtime.valid)
            }) {
                log.complete_pair(
                    PairCompletionIdentity {
                        run_id: pair.run_id.clone(),
                        pair_id: pair.pair_id.clone(),
                        attempt_id: attempt_id.clone(),
                        task_id: pair.task_id.clone(),
                        instance_hash: pair.instance_hash.clone(),
                        schedule_hash: plan.schedule.schedule_hash.clone(),
                        phase: pair.phase,
                    },
                    [
                        records[0].record_hash.clone(),
                        records[1].record_hash.clone(),
                    ],
                )
                .map_err(|error| RunnerError(error.to_string()))?;
                completed = true;
                break;
            }
            if !retry_pair || retry + 1 == attempts_allowed || run_cancel.is_cancelled() {
                break;
            }
        }
        if !completed {
            break;
        }
    }
    signal_task.abort();
    Ok(())
}

fn mark_runner_internal(runtime: &mut RuntimeRecord, error: &str) {
    runtime.terminal_status = TerminalStatus::RunnerInternal;
    runtime.valid = false;
    runtime.task_passed = false;
    runtime.worker_error = Some(match runtime.worker_error.take() {
        Some(existing) => format!("{existing}; {error}"),
        None => error.to_string(),
    });
}

fn load_plan(path: &Path) -> Result<FrozenPlan, RunnerError> {
    let bytes = std::fs::read(path)?;
    let plan: FrozenPlan = serde_json::from_slice(&bytes)
        .map_err(|error| RunnerError(format!("invalid frozen plan: {error}")))?;
    let committed = suite::committed_manifest().map_err(|error| RunnerError(error.to_string()))?;
    if plan.manifest != committed
        || plan.descriptions
            != [
                load(DescriptionVariant::Current),
                load(DescriptionVariant::CompactV1),
            ]
    {
        return Err(RunnerError(
            "frozen plan does not match the committed suite and descriptions".into(),
        ));
    }
    validate_frozen_plan(&plan).map_err(|error| RunnerError(error.to_string()))?;
    Ok(plan)
}

fn require_unplanned_phase(plan: &FrozenPlan, phase: &str) -> Result<(), RunnerError> {
    if matches!(plan.planning, MainPlanning::Unplanned) && plan.schedule.main.is_empty() {
        Ok(())
    } else {
        Err(RunnerError(format!(
            "{phase} phase requires the original unplanned frozen plan"
        )))
    }
}

fn frozen_utc_date(
    plan: &FrozenPlan,
    state: &crate::artifacts::ResumeState,
    phase: SchedulePhase,
) -> Result<String, RunnerError> {
    if phase == SchedulePhase::Main {
        return plan
            .require_planned_main()
            .map(|planning| planning.pilot_evidence.runtime_context.utc_date.clone())
            .map_err(|error| RunnerError(error.to_string()));
    }
    let pair_ids = match phase {
        SchedulePhase::Smoke => plan
            .schedule
            .smoke
            .iter()
            .map(|pair| pair.pair_id.as_str())
            .collect::<BTreeSet<_>>(),
        SchedulePhase::Pilot => plan
            .schedule
            .smoke
            .iter()
            .chain(&plan.schedule.pilot)
            .map(|pair| pair.pair_id.as_str())
            .collect::<BTreeSet<_>>(),
        SchedulePhase::Main => unreachable!(),
    };
    for trial in state.trials_by_hash.values().filter(|trial| {
        trial.identity.run_id == plan.schedule.run_id
            && pair_ids.contains(trial.identity.pair_id.as_str())
    }) {
        if let Some(date) = trial.runtime.get("utc_date").and_then(Value::as_str) {
            return Ok(date.into());
        }
    }
    Ok(chrono::Utc::now().format("%Y-%m-%d").to_string())
}

fn require_complete_pairs(
    pairs: &[PairScheduleRecord],
    state: &crate::artifacts::ResumeState,
    schedule_hash: &str,
    message: &str,
) -> Result<(), RunnerError> {
    for pair in pairs {
        let completed = completed_pair(state, schedule_hash, pair).map_err(|error| {
            RunnerError(format!(
                "{message} in the same record stream: {}: {error}",
                pair.pair_id
            ))
        })?;
        for trial in completed.trials {
            if !trial
                .runtime
                .get("valid")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Err(RunnerError(format!(
                    "completed excluded pair {} contains an invalid trial",
                    pair.pair_id
                )));
            }
        }
    }
    Ok(())
}

fn validate_resume_before_resolution(
    plan: &FrozenPlan,
    state: &crate::artifacts::ResumeState,
    options: &RunOptions,
    run_state: &FrozenRunState,
) -> Result<(), RunnerError> {
    let frozen = frozen_model(plan)?;
    if state
        .trials_by_hash
        .values()
        .any(|trial| trial.identity.run_id != plan.schedule.run_id)
        || state
            .completion_markers
            .values()
            .any(|marker| marker.identity.run_id != plan.schedule.run_id)
    {
        return Err(RunnerError(
            "record stream contains a different frozen run. Use separate artifacts per model and plan"
                .into(),
        ));
    }
    let pairs = match options.phase {
        SchedulePhase::Pilot => plan
            .schedule
            .smoke
            .iter()
            .chain(&plan.schedule.pilot)
            .collect::<Vec<_>>(),
        _ => phase_pairs(plan, options.phase).iter().collect(),
    };
    let pair_ids = pairs
        .iter()
        .map(|pair| pair.pair_id.as_str())
        .collect::<BTreeSet<_>>();
    let expected_limits = RuntimeLimits {
        wall_timeout_seconds: options.timeout.as_secs(),
        max_provider_requests: options.max_model_responses,
        max_model_responses: options.max_model_responses,
        provider_output_token_ceiling: frozen.max_tokens,
        aggregate_observed_output_token_ceiling: frozen
            .max_tokens
            .saturating_mul(u64::from(options.max_model_responses)),
    };
    let tools = expected_tools(DescriptionVariant::Current, frozen.family.as_deref());
    let context = initial_context(&run_state.utc_date, "resume-context", &tools);
    let system_prompt_hash = sha256_hex(
        context
            .system_prompt
            .as_deref()
            .unwrap_or_default()
            .as_bytes(),
    );
    let descriptions = recorded_descriptions();
    let revision = run_state.source.revision_label();

    for pair in &pairs {
        let key = PairKey {
            run_id: pair.run_id.clone(),
            pair_id: pair.pair_id.clone(),
        };
        if state.completion_markers.contains_key(&key) {
            completed_pair(state, &plan.schedule.schedule_hash, pair)
                .map_err(|error| RunnerError(error.to_string()))?;
        }
        let attempt_count = state
            .trials_by_hash
            .values()
            .filter(|trial| {
                trial.identity.run_id == pair.run_id && trial.identity.pair_id == pair.pair_id
            })
            .map(|trial| trial.identity.attempt_id.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        if attempt_count > 2 {
            return Err(RunnerError(format!(
                "resumed pair {} exceeds the two-attempt limit",
                pair.pair_id
            )));
        }
    }

    for trial in state.trials_by_hash.values().filter(|trial| {
        trial.identity.run_id == plan.schedule.run_id
            && pair_ids.contains(trial.identity.pair_id.as_str())
    }) {
        let pair = pairs
            .iter()
            .find(|pair| pair.pair_id == trial.identity.pair_id)
            .expect("pair id came from the frozen phase set");
        let scheduled = pair
            .trials
            .iter()
            .find(|scheduled| {
                scheduled.order_index == trial.identity.order_index
                    && scheduled.variant == trial.identity.variant
            })
            .ok_or_else(|| {
                RunnerError(format!(
                    "resumed pair {} has a trial outside its frozen slots",
                    pair.pair_id
                ))
            })?;
        if trial.identity.task_id != scheduled.task_id
            || trial.identity.instance_hash != scheduled.instance_hash
            || trial.identity.archetype_id != pair.archetype_id
            || trial.identity.schedule_hash != plan.schedule.schedule_hash
            || trial.identity.phase != scheduled.phase
            || trial.identity.repetition != scheduled.archetype_repetition
        {
            return Err(RunnerError(format!(
                "resumed pair {} has a mixed frozen trial identity",
                pair.pair_id
            )));
        }
        let runtime: RuntimeRecord =
            serde_json::from_value(trial.runtime.clone()).map_err(|error| {
                RunnerError(format!("cannot validate resumed runtime context: {error}"))
            })?;
        let unresolved = runtime.terminal_status == TerminalStatus::InfrastructureFailed
            && runtime.provider_requests == 0;
        if runtime.image_id != run_state.image_id
            || runtime.source_provenance != run_state.source
            || runtime.utc_date != run_state.utc_date
            || runtime.limits != expected_limits
            || (!unresolved
                && !runtime.system_prompt_hash.is_empty()
                && runtime.system_prompt_hash != system_prompt_hash)
            || trial.metadata.current_description != descriptions[0]
            || trial.metadata.compact_description != descriptions[1]
            || trial.metadata.aj_revision != revision
            || trial.metadata.suite_revision != plan.universe.suite_revision
            || trial.metadata.model_catalog_hash != frozen.catalog_hash
            || trial.metadata.provider != frozen.provider
            || trial.metadata.model != frozen.model
            || trial.metadata.reasoning_effort != frozen.reasoning
            || trial.metadata.tool_catalog_hash != frozen.tool_catalog_hash
        {
            return Err(RunnerError(format!(
                "resumed pair {} has a different immutable runtime context",
                trial.identity.pair_id
            )));
        }
    }
    Ok(())
}

fn persist_resolution_failure(
    options: &RunOptions,
    plan: &FrozenPlan,
    state: &crate::artifacts::ResumeState,
    run_state: &FrozenRunState,
    resolved: (&ModelInfo, &str, ThinkingLevel),
    error: &RunnerError,
) -> Result<(), RunnerError> {
    let Some(pair) = phase_pairs(plan, options.phase).iter().find(|pair| {
        !state.complete_pairs.contains(&PairKey {
            run_id: pair.run_id.clone(),
            pair_id: pair.pair_id.clone(),
        })
    }) else {
        return Ok(());
    };
    let attempts = state
        .trials_by_hash
        .values()
        .filter(|trial| {
            trial.identity.run_id == pair.run_id && trial.identity.pair_id == pair.pair_id
        })
        .map(|trial| trial.identity.attempt_id.as_str())
        .collect::<BTreeSet<_>>();
    if attempts.len() >= 2 {
        return Ok(());
    }
    let blobs = BlobStore::new(options.artifact_dir.join("blobs"))
        .map_err(|error| RunnerError(error.to_string()))?;
    let (resolved_model, catalog_hash, _) = resolved;
    let max_tokens = resolved_model.max_tokens;
    let catalog_hash = catalog_hash.to_string();
    let provider = resolved_model.provider.clone();
    let model = resolved_model.id.clone();
    let reserve = conservative_catalog_pair_reserve(resolved_model, options.max_model_responses);
    let limits = RuntimeLimits {
        wall_timeout_seconds: options.timeout.as_secs(),
        max_provider_requests: options.max_model_responses,
        max_model_responses: options.max_model_responses,
        provider_output_token_ceiling: max_tokens,
        aggregate_observed_output_token_ceiling: max_tokens
            .saturating_mul(u64::from(options.max_model_responses)),
    };
    let attempt_id = format!("{:032x}", rand::random::<u128>());
    let scheduled = &pair.trials[0];
    let mut runtime = setup_failure_runtime(
        format!("model or credential resolution failed: {error}"),
        opaque_cache_key(&rand::random::<[u8; 32]>()),
        limits,
        run_state,
        &blobs,
        reserve,
        true,
    );
    if runtime.conversation_jsonl_blob.is_none() {
        return Err(RunnerError(runtime.worker_error.clone().unwrap_or_else(
            || "cannot store credential failure artifact".into(),
        )));
    }
    runtime.terminal_status = TerminalStatus::InfrastructureFailed;
    let descriptions = recorded_descriptions();
    let record = TrialRecord::new(
        trial_identity(plan, pair, scheduled, &attempt_id),
        TrialMetadata {
            task_seed: find_instance(plan, pair)?.task_seed.clone(),
            current_description: descriptions[0].clone(),
            compact_description: descriptions[1].clone(),
            aj_revision: run_state.source.revision_label(),
            suite_revision: plan.universe.suite_revision.clone(),
            model_catalog_hash: catalog_hash,
            provider,
            model,
            reasoning_effort: frozen_model(plan)?.reasoning.clone(),
            tool_catalog_hash: frozen_model(plan)?.tool_catalog_hash.clone(),
            fixture_revision: String::new(),
        },
        serde_json::to_value(runtime).map_err(|error| RunnerError(error.to_string()))?,
    )
    .map_err(|error| RunnerError(error.to_string()))?;
    ArtifactLog::open(&options.records)
        .map_err(|error| RunnerError(error.to_string()))?
        .append_trial(&record)
        .map_err(|error| RunnerError(error.to_string()))
}

fn resolve_model_metadata(
    plan: &FrozenPlan,
) -> Result<(Arc<ModelInfo>, String, ThinkingLevel), RunnerError> {
    let frozen = frozen_model(plan)?;
    let reasoning = parse_reasoning(&frozen.reasoning)?;
    let registry = ModelRegistry::load();
    let current = freeze_model_selection_from_registry(
        &registry,
        &frozen.provider,
        &frozen.model,
        reasoning,
    )?;
    if &current != frozen {
        return Err(RunnerError(
            "local model catalog or capability identity differs from the frozen plan".into(),
        ));
    }
    let model = registry
        .get(&frozen.provider, &frozen.model)
        .cloned()
        .ok_or_else(|| RunnerError("frozen model disappeared from the local registry".into()))?;
    Ok((Arc::new(model), frozen.catalog_hash.clone(), reasoning))
}

async fn resolve_trusted_model(
    model: Arc<ModelInfo>,
    catalog_hash: String,
    reasoning: ThinkingLevel,
) -> Result<TrustedModel, RunnerError> {
    let auth = AuthStorage::at_default_path().map_err(|error| RunnerError(error.to_string()))?;
    if auth
        .get_api_key(&model.provider)
        .await
        .map_err(|error| RunnerError(error.to_string()))?
        .is_none()
    {
        return Err(RunnerError(format!(
            "no credentials resolved for {}",
            model.provider
        )));
    }
    let resolved = aj_app::model::from_model_info(&auth, (*model).clone(), None)
        .map_err(|error| RunnerError(error.to_string()))?;
    Ok(TrustedModel {
        provider: resolved.provider,
        model: resolved.model_info,
        stream_options: resolved.stream_options,
        catalog_hash,
        reasoning,
    })
}

fn unpaid_request_preflight(
    model: &ModelInfo,
    reasoning: ThinkingLevel,
    date: &str,
) -> Result<(), RunnerError> {
    validate_thinking_level(model, &reasoning).map_err(RunnerError)?;
    let prompt = "Unpaid request-construction preflight.";
    let current_tools = expected_tools(DescriptionVariant::Current, model.family.as_deref());
    let compact_tools = expected_tools(DescriptionVariant::CompactV1, model.family.as_deref());
    let current = initial_context(date, prompt, &current_tools);
    let compact = initial_context(date, prompt, &compact_tools);
    validate_context_pair(&current, &compact)?;
    let system_prompt = current.system_prompt.as_deref().unwrap_or_default();
    if !system_prompt.contains("/workspace") || !system_prompt.contains(date) {
        return Err(RunnerError(
            "unpaid context lacks the canonical workspace or frozen UTC date".into(),
        ));
    }

    let current_payload = request_shape(
        model,
        reasoning,
        &current,
        "preflight-cache-key",
        model.max_tokens,
    );
    let compact_payload = request_shape(
        model,
        reasoning,
        &compact,
        "preflight-cache-key",
        model.max_tokens,
    );
    if normalized_request_hash(&current_payload, "preflight-cache-key")?
        != normalized_request_hash(&compact_payload, "preflight-cache-key")?
    {
        return Err(RunnerError(
            "unpaid provider request contexts differ outside apply_patch.description".into(),
        ));
    }
    if !payload_has_reasoning(&current_payload, model, reasoning) {
        return Err(RunnerError(
            "unpaid provider request does not serialize frozen reasoning".into(),
        ));
    }
    Ok(())
}

fn validate_context_pair(current: &Context, compact: &Context) -> Result<(), RunnerError> {
    let mut normalized_current = serde_json::to_value(current)
        .map_err(|error| RunnerError(format!("cannot serialize current context: {error}")))?;
    let mut normalized_compact = serde_json::to_value(compact)
        .map_err(|error| RunnerError(format!("cannot serialize compact context: {error}")))?;
    normalize_apply_patch_description(&mut normalized_current)?;
    normalize_apply_patch_description(&mut normalized_compact)?;
    if normalized_current != normalized_compact {
        return Err(RunnerError(
            "provider contexts differ outside apply_patch.description".into(),
        ));
    }
    let current_description = apply_patch_description(current)?;
    let compact_description = apply_patch_description(compact)?;
    if current_description != load(DescriptionVariant::Current).content
        || compact_description != load(DescriptionVariant::CompactV1).content
        || current_description == compact_description
    {
        return Err(RunnerError(
            "provider contexts do not contain the two frozen apply_patch descriptions".into(),
        ));
    }
    Ok(())
}

fn apply_patch_description(context: &Context) -> Result<&str, RunnerError> {
    context
        .tools
        .iter()
        .find(|tool| tool.name == "apply_patch")
        .map(|tool| tool.description.as_str())
        .ok_or_else(|| RunnerError("provider context has no apply_patch tool".into()))
}

fn request_shape(
    model: &ModelInfo,
    reasoning: ThinkingLevel,
    context: &Context,
    cache_key: &str,
    max_tokens: u64,
) -> Value {
    let options = StreamOptions {
        session_id: Some(cache_key.to_string()),
        max_tokens: Some(max_tokens),
        ..StreamOptions::default()
    };
    let mut payload = serde_json::json!({
        "model": model.id,
        "provider": model.provider,
        "prompt_cache_key": cache_key,
        "max_output_tokens": max_tokens,
        "stream_options": options,
        "context": context,
    });
    if let Some(effort) = responses_reasoning_effort(model, &reasoning) {
        payload["reasoning"] = serde_json::json!({"effort": effort});
    }
    payload
}

fn payload_has_reasoning(payload: &Value, model: &ModelInfo, reasoning: ThinkingLevel) -> bool {
    let actual = payload.pointer("/reasoning/effort");
    match responses_reasoning_effort(model, &reasoning) {
        Some(expected) => serde_json::to_value(expected).ok().as_ref() == actual,
        None => payload.get("reasoning").is_none(),
    }
}

fn normalized_request_hash(payload: &Value, session_id: &str) -> Result<String, RunnerError> {
    let mut normalized = payload.clone();
    if normalized.get("prompt_cache_key").and_then(Value::as_str) != Some(session_id) {
        return Err(RunnerError(
            "serialized request has an unexpected prompt cache key".into(),
        ));
    }
    if let Some(cache_key) = normalized.get_mut("prompt_cache_key") {
        *cache_key = Value::String("<opaque-cache-key>".into());
    } else {
        return Err(RunnerError(
            "serialized request has no prompt cache key".into(),
        ));
    }
    normalize_apply_patch_description(&mut normalized)?;
    serde_json::to_vec(&normalized)
        .map(|bytes| hash_framed(b"aj-apply-patch-eval-normalized-context-v1", &[&bytes]))
        .map_err(|error| RunnerError(error.to_string()))
}

fn normalize_apply_patch_description(value: &mut Value) -> Result<(), RunnerError> {
    let tools_value = if value.get("context").is_some() {
        value.pointer_mut("/context/tools")
    } else {
        value.get_mut("tools")
    };
    let tools = tools_value
        .and_then(Value::as_array_mut)
        .ok_or_else(|| RunnerError("serialized context has no tools array".into()))?;
    let tool = tools
        .iter_mut()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("apply_patch"))
        .ok_or_else(|| RunnerError("serialized context has no apply_patch tool".into()))?;
    let description = tool
        .get_mut("description")
        .ok_or_else(|| RunnerError("apply_patch tool has no description".into()))?;
    *description = Value::String("<apply-patch-description>".into());
    Ok(())
}

fn tool_catalog_hash(tools: &[ToolDefinition]) -> Result<String, RunnerError> {
    let mut value = serde_json::json!({"tools": tools});
    normalize_apply_patch_description(&mut value)?;
    serde_json::to_vec(&value)
        .map(|bytes| hash_framed(b"aj-apply-patch-eval-tool-catalog-v1", &[&bytes]))
        .map_err(|error| RunnerError(format!("cannot hash tool catalog: {error}")))
}

fn registry_hash(registry: &ModelRegistry) -> Result<String, RunnerError> {
    let models = registry
        .providers()
        .into_iter()
        .map(|provider| registry.models(provider))
        .collect::<Vec<_>>();
    serde_json::to_vec(&(registry.source_label(), registry.updated_at, models))
        .map(|bytes| sha256_hex(&bytes))
        .map_err(|error| RunnerError(error.to_string()))
}

fn recorded_descriptions() -> [RecordedDescription; 2] {
    let current = load(DescriptionVariant::Current);
    let compact = load(DescriptionVariant::CompactV1);
    [current, compact].map(|description| RecordedDescription {
        sha256: description.sha256,
        byte_length: description.byte_length,
    })
}

async fn source_provenance() -> Result<SourceProvenance, RunnerError> {
    let head = git_output(&["rev-parse", "HEAD"]).await?;
    let status = git_output_bytes(&["status", "--porcelain=v1", "-z"]).await?;
    if status.is_empty() {
        return Ok(SourceProvenance {
            head: String::from_utf8_lossy(&head).trim().to_string(),
            dirty: false,
            worktree_hash: None,
        });
    }
    let diff = git_output_bytes(&["diff", "--binary", "HEAD"]).await?;
    let untracked_paths =
        git_output_bytes(&["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    let mut untracked = Vec::new();
    for path in untracked_paths
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path_text = std::str::from_utf8(path)
            .map_err(|_| RunnerError("source provenance has a non-UTF-8 path".into()))?;
        let metadata = std::fs::symlink_metadata(path_text)?;
        let bytes = if metadata.file_type().is_symlink() {
            std::fs::read_link(path_text)?
                .to_string_lossy()
                .as_bytes()
                .to_vec()
        } else {
            std::fs::read(path_text)?
        };
        let path_length = u64::try_from(path.len())
            .map_err(|_| RunnerError("source provenance path length exceeds u64".into()))?;
        let byte_length = u64::try_from(bytes.len())
            .map_err(|_| RunnerError("source provenance file length exceeds u64".into()))?;
        untracked.extend_from_slice(&path_length.to_be_bytes());
        untracked.extend_from_slice(path);
        untracked.extend_from_slice(&byte_length.to_be_bytes());
        untracked.extend_from_slice(&bytes);
    }
    Ok(SourceProvenance {
        head: String::from_utf8_lossy(&head).trim().to_string(),
        dirty: true,
        worktree_hash: Some(hash_framed(
            b"aj-apply-patch-eval-source-worktree-v1",
            &[&status, &diff, &untracked],
        )),
    })
}

async fn git_output(arguments: &[&str]) -> Result<Vec<u8>, RunnerError> {
    git_output_bytes(arguments).await
}

async fn git_output_bytes(arguments: &[&str]) -> Result<Vec<u8>, RunnerError> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("git").args(arguments).output(),
    )
    .await
    .map_err(|_| RunnerError(format!("git {} timed out", arguments.join(" "))))??;
    if !output.status.success() {
        return Err(RunnerError(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn phase_pairs(plan: &FrozenPlan, phase: SchedulePhase) -> &[PairScheduleRecord] {
    match phase {
        SchedulePhase::Smoke => &plan.schedule.smoke,
        SchedulePhase::Pilot => &plan.schedule.pilot,
        SchedulePhase::Main => &plan.schedule.main,
    }
}

fn find_instance<'a>(
    plan: &'a FrozenPlan,
    pair: &PairScheduleRecord,
) -> Result<&'a TaskInstance, RunnerError> {
    plan.universe
        .instances
        .iter()
        .find(|instance| instance.instance_hash == pair.instance_hash)
        .ok_or_else(|| RunnerError("scheduled task instance is missing".into()))
}

fn trial_identity(
    plan: &FrozenPlan,
    pair: &PairScheduleRecord,
    trial: &TrialScheduleRecord,
    attempt_id: &str,
) -> TrialIdentity {
    TrialIdentity {
        run_id: trial.run_id.clone(),
        pair_id: trial.pair_id.clone(),
        attempt_id: attempt_id.into(),
        task_id: trial.task_id.clone(),
        instance_hash: trial.instance_hash.clone(),
        archetype_id: pair.archetype_id.clone(),
        schedule_hash: plan.schedule.schedule_hash.clone(),
        phase: trial.phase,
        repetition: trial.archetype_repetition,
        variant: trial.variant,
        order_index: trial.order_index,
    }
}

fn recorded_spend(state: &crate::artifacts::ResumeState, phase: SchedulePhase) -> f64 {
    state
        .trials_by_hash
        .values()
        .filter(|record| record.identity.phase == phase)
        .filter_map(|record| {
            record
                .runtime
                .get("aj_recorded_catalog_cost")
                .and_then(Value::as_f64)
        })
        .sum()
}

fn pair_reserve(
    phase: SchedulePhase,
    plan: &FrozenPlan,
    model: &ModelInfo,
    max_requests: u32,
) -> Result<f64, RunnerError> {
    let catalog = conservative_catalog_pair_reserve(model, max_requests);
    if phase != SchedulePhase::Main {
        return Ok(catalog);
    }
    let planned = plan
        .require_planned_main()
        .map_err(|error| RunnerError(error.to_string()))?;
    if (planned.conservative_catalog_pair_reserve - catalog).abs() > 1e-9 {
        return Err(RunnerError(
            "current catalog safety reserve differs from the pilot-frozen reserve".into(),
        ));
    }
    Ok(catalog)
}

fn conservative_catalog_pair_reserve(model: &ModelInfo, max_requests: u32) -> f64 {
    let mut input_rate = model.cost.input;
    let mut output_rate = model.cost.output;
    let mut cache_read_rate = model.cost.cache_read;
    let mut cache_write_rate = model.cost.cache_write;
    for tier in &model.cost.tiers {
        input_rate = input_rate.max(tier.input);
        output_rate = output_rate.max(tier.output);
        cache_read_rate = cache_read_rate.max(tier.cache_read);
        cache_write_rate = cache_write_rate.max(tier.cache_write);
    }
    let input = u64_as_f64(model.context_window);
    let output = u64_as_f64(model.max_tokens);
    let per_request = (input * (input_rate + cache_read_rate + cache_write_rate)
        + output * output_rate)
        / 1_000_000.0;
    2.0 * f64::from(max_requests) * per_request
}

fn validate_resume_context(
    plan: &FrozenPlan,
    state: &crate::artifacts::ResumeState,
    options: &RunOptions,
    trusted: &TrustedModel,
    run_state: &FrozenRunState,
) -> Result<(), RunnerError> {
    for pair in phase_pairs(plan, options.phase) {
        let key = PairKey {
            run_id: pair.run_id.clone(),
            pair_id: pair.pair_id.clone(),
        };
        if let Some(marker) = state.completion_markers.get(&key)
            && (marker.identity.schedule_hash != plan.schedule.schedule_hash
                || marker.identity.phase != options.phase)
        {
            return Err(RunnerError(format!(
                "completed pair {} has a mixed immutable phase identity",
                pair.pair_id
            )));
        }
    }
    let pair_ids = match options.phase {
        SchedulePhase::Pilot => plan
            .schedule
            .smoke
            .iter()
            .chain(&plan.schedule.pilot)
            .map(|pair| pair.pair_id.as_str())
            .collect::<BTreeSet<_>>(),
        _ => phase_pairs(plan, options.phase)
            .iter()
            .map(|pair| pair.pair_id.as_str())
            .collect::<BTreeSet<_>>(),
    };
    let limits = RuntimeLimits {
        wall_timeout_seconds: options.timeout.as_secs(),
        max_provider_requests: options.max_model_responses,
        max_model_responses: options.max_model_responses,
        provider_output_token_ceiling: trusted.model.max_tokens,
        aggregate_observed_output_token_ceiling: trusted
            .model
            .max_tokens
            .saturating_mul(u64::from(options.max_model_responses)),
    };
    let tools = expected_tools(DescriptionVariant::Current, trusted.model.family.as_deref());
    let context = initial_context(&run_state.utc_date, "resume-context", &tools);
    let system_prompt_hash = sha256_hex(
        context
            .system_prompt
            .as_deref()
            .unwrap_or_default()
            .as_bytes(),
    );
    if options.phase == SchedulePhase::Main {
        let frozen = &plan
            .require_planned_main()
            .map_err(|error| RunnerError(error.to_string()))?
            .pilot_evidence
            .runtime_context;
        let reserve =
            conservative_catalog_pair_reserve(&trusted.model, options.max_model_responses);
        if frozen.image_id != run_state.image_id
            || frozen.source_provenance != run_state.source
            || frozen.utc_date != run_state.utc_date
            || frozen.limits != limits
            || frozen.system_prompt_hash != system_prompt_hash
            || frozen.aj_revision != run_state.source.revision_label()
            || frozen.model_catalog_hash != trusted.catalog_hash
            || frozen.provider != trusted.model.provider
            || frozen.model != trusted.model.id
            || frozen.reasoning_effort != trusted.reasoning.as_str()
            || frozen.tool_catalog_hash != frozen_model(plan)?.tool_catalog_hash
            || (frozen.conservative_catalog_pair_reserve - reserve).abs() > 1e-9
        {
            return Err(RunnerError(
                "main runtime context differs from the exact pilot context".into(),
            ));
        }
    }
    for trial in state.trials_by_hash.values().filter(|trial| {
        trial.identity.run_id == plan.schedule.run_id
            && pair_ids.contains(trial.identity.pair_id.as_str())
    }) {
        let runtime: RuntimeRecord =
            serde_json::from_value(trial.runtime.clone()).map_err(|error| {
                RunnerError(format!("cannot validate resumed runtime context: {error}"))
            })?;
        let unresolved = runtime.terminal_status == TerminalStatus::InfrastructureFailed
            && runtime.model_responses == 0;
        let system_matches = runtime.system_prompt_hash.is_empty()
            || runtime.system_prompt_hash == system_prompt_hash;
        if runtime.image_id != run_state.image_id
            || runtime.source_provenance != run_state.source
            || runtime.utc_date != run_state.utc_date
            || runtime.limits.wall_timeout_seconds != limits.wall_timeout_seconds
            || runtime.limits.max_provider_requests != limits.max_provider_requests
            || runtime.limits.max_model_responses != limits.max_model_responses
            || (!unresolved && runtime.limits != limits)
            || (!unresolved && !system_matches)
            || (!unresolved && trial.metadata.model_catalog_hash != trusted.catalog_hash)
            || (!unresolved && trial.metadata.provider != trusted.model.provider)
            || (!unresolved && trial.metadata.model != trusted.model.id)
            || trial.metadata.reasoning_effort != trusted.reasoning.as_str()
            || trial.metadata.tool_catalog_hash != frozen_model(plan)?.tool_catalog_hash
        {
            return Err(RunnerError(format!(
                "resumed pair {} has a different immutable runtime context",
                trial.identity.pair_id
            )));
        }
    }
    Ok(())
}

#[allow(clippy::as_conversions)]
fn usize_as_f64(value: usize) -> f64 {
    value as f64
}

#[allow(clippy::as_conversions)]
fn u64_as_f64(value: u64) -> f64 {
    value as f64
}

struct TrialExecution {
    fixture: GeneratedFixture,
    baseline: FilesystemSnapshot,
    final_snapshot: FilesystemSnapshot,
    final_snapshot_valid: bool,
    final_delta: SnapshotDelta,
    worker: Option<WorkerResult>,
    parent_metrics: WorkerMetrics,
    mutation_ledger: Vec<MutationLedgerEntry>,
    patch_calls: Vec<PatchCallRecord>,
    tool_outcomes: Vec<ToolOutcomeRecord>,
    edit_bypass: bool,
    payload_hashes: Vec<String>,
    provider_errors: Vec<String>,
    provider_error_details: Vec<ProviderErrorRecord>,
    provider_infrastructure_failed: bool,
    first_paid_request_validated: bool,
    internal_error: Option<String>,
    timed_out: bool,
    cancelled: bool,
    duration: Duration,
    verifier: Option<VerifyWorkerOutput>,
    cache_key: String,
    baseline_commit: Option<String>,
    git_artifacts: Option<GitArtifacts>,
    normalized_model_context_hashes: Vec<String>,
    provider_requests: u32,
    containment_cleanup_confirmed: bool,
    system_prompt_hash: String,
}

async fn run_trial(
    options: &RunOptions,
    trusted: &TrustedModel,
    instance: &TaskInstance,
    scheduled: &TrialScheduleRecord,
    _attempt_id: &str,
    blobs: &BlobStore,
    run_cancel: CancellationToken,
    run_state: &FrozenRunState,
) -> Result<RuntimeRecord, RunnerError> {
    let started = Instant::now();
    let trial_cancel = CancellationToken::new();
    let deadline_expired = Arc::new(AtomicBool::new(false));
    let deadline_task = {
        let trial_cancel = trial_cancel.clone();
        let deadline_expired = Arc::clone(&deadline_expired);
        let run_cancel = run_cancel.clone();
        let timeout = options.timeout;
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(timeout) => {
                    deadline_expired.store(true, Ordering::SeqCst);
                    trial_cancel.cancel();
                }
                () = run_cancel.cancelled() => trial_cancel.cancel(),
            }
        })
    };
    let result = run_trial_inner(
        options,
        trusted,
        instance,
        scheduled,
        _attempt_id,
        blobs,
        trial_cancel,
        run_state,
    )
    .await;
    deadline_task.abort();
    let mut runtime = result?;
    runtime.duration_millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    apply_deadline_outcome(
        &mut runtime,
        deadline_expired.load(Ordering::SeqCst),
        options.timeout,
    );
    Ok(runtime)
}

fn apply_deadline_outcome(runtime: &mut RuntimeRecord, expired: bool, timeout: Duration) {
    if !expired {
        return;
    }
    runtime.duration_millis = runtime
        .duration_millis
        .max(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX));
    if matches!(
        runtime.terminal_status,
        TerminalStatus::RunnerInternal | TerminalStatus::InfrastructureFailed
    ) {
        return;
    }
    if !runtime.containment_cleanup_confirmed {
        mark_runner_internal(
            runtime,
            "whole-trial deadline expired and containment cleanup was not confirmed",
        );
        return;
    }
    if runtime.normalized_first_request_hash.is_none() {
        runtime.terminal_status = TerminalStatus::InfrastructureFailed;
        runtime.valid = false;
    } else {
        runtime.terminal_status = TerminalStatus::TimedOut;
        runtime.valid = true;
    }
    runtime.task_passed = false;
}

async fn run_trial_inner(
    options: &RunOptions,
    trusted: &TrustedModel,
    instance: &TaskInstance,
    scheduled: &TrialScheduleRecord,
    _attempt_id: &str,
    blobs: &BlobStore,
    run_cancel: CancellationToken,
    run_state: &FrozenRunState,
) -> Result<RuntimeRecord, RunnerError> {
    let cache_key = opaque_cache_key(&rand::random::<[u8; 32]>());
    let max_output_tokens = trusted.model.max_tokens;
    let catalog_pair_reserve =
        conservative_catalog_pair_reserve(&trusted.model, options.max_model_responses);
    let runtime_limits = RuntimeLimits {
        wall_timeout_seconds: options.timeout.as_secs(),
        max_provider_requests: options.max_model_responses,
        max_model_responses: options.max_model_responses,
        provider_output_token_ceiling: max_output_tokens,
        aggregate_observed_output_token_ceiling: max_output_tokens
            .saturating_mul(u64::from(options.max_model_responses)),
    };
    let mut volume = match FixtureVolume::create(&options.image, &instance.archetype_id).await {
        Ok(volume) => volume,
        Err(error) => {
            return Ok(setup_failure_runtime(
                format!("fixture volume creation failed: {error}"),
                cache_key,
                runtime_limits,
                run_state,
                blobs,
                catalog_pair_reserve,
                false,
            ));
        }
    };
    if run_cancel.is_cancelled() {
        let cleanup = volume.cleanup().await;
        let cleanup_confirmed = cleanup.is_ok();
        return Ok(setup_cancelled_runtime(
            setup_error_with_cleanup("fixture volume creation", "trial cancelled", cleanup),
            cache_key,
            runtime_limits,
            run_state,
            blobs,
            catalog_pair_reserve,
            cleanup_confirmed,
        ));
    }
    let fixture_output: FixtureWorkerOutput = match run_helper_cancellable(
        &options.image,
        "__fixture-worker",
        &volume,
        false,
        &FixtureWorkerInput {
            instance: instance.clone(),
        },
        &run_cancel,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            let cleanup = volume.cleanup().await;
            let cleanup_confirmed = cleanup.is_ok();
            let message = setup_error_with_cleanup("fixture materialization", &error, cleanup);
            return Ok(if clean_helper_cancellation(&error, &run_cancel) {
                setup_cancelled_runtime(
                    message,
                    cache_key,
                    runtime_limits,
                    run_state,
                    blobs,
                    catalog_pair_reserve,
                    cleanup_confirmed,
                )
            } else {
                setup_failure_runtime(
                    message,
                    cache_key,
                    runtime_limits,
                    run_state,
                    blobs,
                    catalog_pair_reserve,
                    cleanup_confirmed,
                )
            });
        }
    };
    let baseline_output = match snapshot_volume(
        &options.image,
        &volume,
        true,
        Some(&fixture_output.baseline_commit),
        Some(&run_cancel),
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            let cleanup = volume.cleanup().await;
            let cleanup_confirmed = cleanup.is_ok();
            let message = setup_error_with_cleanup("baseline snapshot", &error, cleanup);
            return Ok(if clean_helper_cancellation(&error, &run_cancel) {
                setup_cancelled_runtime(
                    message,
                    cache_key,
                    runtime_limits,
                    run_state,
                    blobs,
                    catalog_pair_reserve,
                    cleanup_confirmed,
                )
            } else {
                setup_failure_runtime(
                    message,
                    cache_key,
                    runtime_limits,
                    run_state,
                    blobs,
                    catalog_pair_reserve,
                    cleanup_confirmed,
                )
            });
        }
    };
    let baseline = baseline_output.snapshot;
    if !baseline_output
        .git
        .as_ref()
        .is_some_and(|git| git.diff.is_empty() && git.status.is_empty())
    {
        let cleanup = volume.cleanup().await;
        let cleanup_confirmed = cleanup.is_ok();
        return Ok(setup_failure_runtime(
            setup_error_with_cleanup(
                "baseline Git validation",
                "initial commit differs from the materialized worktree",
                cleanup,
            ),
            cache_key,
            runtime_limits,
            run_state,
            blobs,
            catalog_pair_reserve,
            cleanup_confirmed,
        ));
    }
    if baseline.root_hash != fixture_output.fixture.baseline_revision {
        let cleanup = volume.cleanup().await;
        let cleanup_confirmed = cleanup.is_ok();
        let error = match cleanup {
            Ok(()) => "fixture helper and trusted snapshot helper disagree".into(),
            Err(cleanup) => format!(
                "fixture helper and trusted snapshot helper disagree; cleanup failed: {cleanup}"
            ),
        };
        return Ok(setup_failure_runtime(
            error,
            cache_key,
            runtime_limits,
            run_state,
            blobs,
            catalog_pair_reserve,
            cleanup_confirmed,
        ));
    }
    let init = WorkerInit {
        model: worker_model(&trusted.model),
        reasoning: trusted.reasoning,
        variant: scheduled.variant,
        prompt: fixture_output.fixture.prompt.clone(),
        session_id: cache_key.clone(),
        utc_date: run_state.utc_date.clone(),
        max_model_responses: options.max_model_responses,
        max_output_tokens,
    };
    let started = Instant::now();
    let mut execution = execute_container(
        options,
        trusted,
        &volume,
        init,
        baseline.clone(),
        fixture_output.fixture.clone(),
        cache_key,
        run_cancel.clone(),
    )
    .await;
    execution.duration = started.elapsed();
    let baseline_commit = fixture_output.baseline_commit;
    execution.baseline_commit = Some(baseline_commit.clone());

    if execution.containment_cleanup_confirmed && !execution.cancelled && !execution.timed_out {
        match snapshot_volume(
            &options.image,
            &volume,
            true,
            Some(&baseline_commit),
            Some(&run_cancel),
        )
        .await
        {
            Ok(final_output) => {
                let final_snapshot = final_output.snapshot;
                execution.record_between_boundary(final_snapshot.clone());
                execution.final_delta = delta(&execution.baseline, &final_snapshot);
                execution.final_snapshot = final_snapshot;
                execution.final_snapshot_valid = true;
                execution.git_artifacts = final_output.git;
            }
            Err(error) if clean_helper_cancellation(&error, &run_cancel) => {
                execution.cancelled = true;
                execution.final_delta = delta(&execution.baseline, &execution.final_snapshot);
            }
            Err(error) => {
                execution.internal_error = Some(format!("final snapshot failed: {error}"));
                execution.final_delta = delta(&execution.baseline, &execution.final_snapshot);
            }
        }
    } else if !execution.containment_cleanup_confirmed {
        execution.internal_error = Some(match execution.internal_error.take() {
            Some(error) => format!("{error}; containment cleanup was not confirmed"),
            None => "containment cleanup was not confirmed".into(),
        });
    }

    if run_cancel.is_cancelled() {
        execution.cancelled = true;
    }
    if execution.internal_error.is_none()
        && execution.final_snapshot_valid
        && !execution.cancelled
        && !execution.timed_out
    {
        match verify_final_state(&options.image, &volume, instance, &run_cancel).await {
            Ok(verifier) => {
                if verifier.before.root_hash != execution.final_snapshot.root_hash {
                    execution.internal_error =
                        Some("verifier clone differs from final agent state".into());
                }
                execution.verifier = Some(verifier);
            }
            Err(error) if clean_helper_cancellation(&error, &run_cancel) => {
                execution.cancelled = true;
            }
            Err(error) => {
                execution.containment_cleanup_confirmed = false;
                execution.internal_error = Some(format!("verifier isolation failed: {error}"));
            }
        }
    }
    if let Err(error) = volume.cleanup().await {
        execution.internal_error = Some(format!("fixture volume cleanup failed: {error}"));
        execution.containment_cleanup_confirmed = false;
        execution.final_snapshot_valid = false;
        execution.git_artifacts = None;
        execution.verifier = None;
    }
    finish_runtime(
        execution,
        trusted,
        blobs,
        runtime_limits,
        run_state,
        catalog_pair_reserve,
    )
}

async fn verify_final_state(
    image: &str,
    volume: &FixtureVolume,
    instance: &TaskInstance,
    cancel: &CancellationToken,
) -> Result<VerifyWorkerOutput, RunnerError> {
    let mut verifier_volume = FixtureVolume::create(image, "verifier").await?;
    let result = async {
        copy_volume_cancellable(image, volume, &verifier_volume, cancel).await?;
        run_helper_cancellable(
            image,
            "__verify-worker",
            &verifier_volume,
            true,
            &VerifyWorkerInput {
                instance: instance.clone(),
            },
            cancel,
        )
        .await
        .map_err(RunnerError::from)
    }
    .await;
    let cleanup = verifier_volume.cleanup().await.map_err(RunnerError::from);
    match result {
        Ok(value) => {
            cleanup?;
            Ok(value)
        }
        Err(error) => {
            if let Err(cleanup) = cleanup {
                Err(RunnerError(format!(
                    "{error}; verifier cleanup failed: {cleanup}"
                )))
            } else {
                Err(error)
            }
        }
    }
}

fn worker_model(model: &ModelInfo) -> WorkerModel {
    WorkerModel {
        id: model.id.clone(),
        name: model.name.clone(),
        family: model.family.clone(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        reasoning: model.reasoning,
        reasoning_options: model.reasoning_options.clone(),
        supports_verbosity: model.supports_verbosity,
        input: model.input.clone(),
        cost: model.cost.clone(),
        context_window: model.context_window,
        max_tokens: model.max_tokens,
    }
}

async fn snapshot_volume(
    image: &str,
    volume: &FixtureVolume,
    include_git_artifacts: bool,
    baseline_commit: Option<&str>,
    cancel: Option<&CancellationToken>,
) -> Result<SnapshotWorkerOutput, RunnerError> {
    let input = SnapshotWorkerInput {
        include_git_artifacts,
        baseline_commit: baseline_commit.map(str::to_string),
    };
    match cancel {
        Some(cancel) => {
            run_helper_cancellable(image, "__snapshot-worker", volume, true, &input, cancel).await
        }
        None => run_helper(image, "__snapshot-worker", volume, true, &input).await,
    }
    .map_err(Into::into)
}

fn opaque_cache_key(nonce: &[u8; 32]) -> String {
    hash_framed(b"aj-apply-patch-eval-opaque-cache-key-v1", &[nonce])
}

fn setup_error_with_cleanup(
    stage: &str,
    error: impl fmt::Display,
    cleanup: Result<(), DockerError>,
) -> String {
    match cleanup {
        Ok(()) => format!("{stage} failed: {error}"),
        Err(cleanup) => format!("{stage} failed: {error}; cleanup failed: {cleanup}"),
    }
}

fn clean_helper_cancellation(error: &impl fmt::Display, cancel: &CancellationToken) -> bool {
    cancel.is_cancelled() && error.to_string() == "Docker helper cancelled"
}

fn benign_broker_cancellation(error: &RunnerError, cancel: &CancellationToken) -> bool {
    clean_helper_cancellation(error, cancel)
        || cancel.is_cancelled()
            && error.to_string() == "provider request did not terminate after cancellation"
}

fn setup_failure_runtime(
    mut error: String,
    cache_key: String,
    limits: RuntimeLimits,
    run_state: &FrozenRunState,
    blobs: &BlobStore,
    catalog_pair_reserve: f64,
    containment_cleanup_confirmed: bool,
) -> RuntimeRecord {
    let conversation_jsonl_blob = match blobs.put(&[]) {
        Ok(hash) => Some(hash),
        Err(blob_error) => {
            error.push_str(&format!(
                "; cannot store empty conversation JSONL: {blob_error}"
            ));
            None
        }
    };
    RuntimeRecord {
        terminal_status: TerminalStatus::RunnerInternal,
        valid: false,
        task_passed: false,
        sessions_with_patch_failure: false,
        edit_bypass: false,
        aj_recorded_catalog_cost: 0.0,
        model_responses: 0,
        provider_requests: 0,
        usage: aj_models::types::Usage::default(),
        usage_field_presence: UsageFieldPresence {
            input: None,
            output: None,
            cache_read: None,
            cache_write: None,
            source: "no provider request completed".into(),
        },
        cache_stratum: CacheStratum::UnknownRead,
        cache_write_sensitivity: CacheWriteSensitivity {
            lower_aj_recorded_catalog_cost: 0.0,
            upper_aj_recorded_catalog_cost: 0.0,
            upper_assumed_cache_write_tokens: 0,
        },
        limits,
        first_response_aj_recorded_catalog_cost: None,
        tool_rounds: 0,
        total_tool_calls: 0,
        tool_calls_by_name: Default::default(),
        apply_patch_attempts: 0,
        successful_patch_calls: 0,
        recovery_rounds: 0,
        stream_retries: 0,
        duration_millis: 0,
        final_assistant_text: String::new(),
        prompt: String::new(),
        verifier_command: None,
        patch_calls: Vec::new(),
        tool_outcomes: Vec::new(),
        mutation_ledger: Vec::new(),
        baseline_root_hash: None,
        final_snapshot: None,
        final_snapshot_blob: None,
        final_delta: None,
        baseline_commit: None,
        final_diff_blob: None,
        final_status_blob: None,
        changed_paths: Vec::new(),
        verifier: None,
        payload_hashes: Vec::new(),
        normalized_model_context_hashes: Vec::new(),
        normalized_first_request_hash: None,
        system_prompt_hash: String::new(),
        cache_key_hash: sha256_hex(cache_key.as_bytes()),
        transcript_wire_messages: Vec::new(),
        conversation_jsonl_blob,
        provider_errors: Vec::new(),
        provider_error_details: Vec::new(),
        worker_error: Some(error),
        containment_cleanup_confirmed,
        isolation_contract: "docker_attach_v2: setup failed before a confirmatory final snapshot"
            .into(),
        evaluator_api_limitations: Vec::new(),
        image_id: run_state.image_id.clone(),
        source_provenance: run_state.source.clone(),
        utc_date: run_state.utc_date.clone(),
        conservative_catalog_pair_reserve: catalog_pair_reserve,
        final_assistant_text_blob: None,
    }
}

fn setup_cancelled_runtime(
    error: String,
    cache_key: String,
    limits: RuntimeLimits,
    run_state: &FrozenRunState,
    blobs: &BlobStore,
    catalog_pair_reserve: f64,
    containment_cleanup_confirmed: bool,
) -> RuntimeRecord {
    let mut runtime = setup_failure_runtime(
        error,
        cache_key,
        limits,
        run_state,
        blobs,
        catalog_pair_reserve,
        containment_cleanup_confirmed,
    );
    if containment_cleanup_confirmed && runtime.conversation_jsonl_blob.is_some() {
        runtime.terminal_status = TerminalStatus::Cancelled;
    }
    runtime
}

async fn execute_container(
    options: &RunOptions,
    trusted: &TrustedModel,
    volume: &FixtureVolume,
    init: WorkerInit,
    baseline: FilesystemSnapshot,
    fixture: GeneratedFixture,
    cache_key: String,
    run_cancel: CancellationToken,
) -> TrialExecution {
    let expected_tools = expected_tools(init.variant, trusted.model.family.as_deref());
    let expected_context = initial_context(&init.utc_date, &init.prompt, &expected_tools);
    let mut execution = TrialExecution::new(fixture, baseline, cache_key);
    execution.system_prompt_hash = sha256_hex(
        expected_context
            .system_prompt
            .as_deref()
            .unwrap_or_default()
            .as_bytes(),
    );
    if run_cancel.is_cancelled() {
        execution.cancelled = true;
        return execution;
    }
    let command = worker_command(&options.image, volume);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            execution.internal_error = Some(format!("cannot start worker: {error}"));
            return execution;
        }
    };
    let mut stdin = match child.take_stdin() {
        Some(stdin) => stdin,
        None => {
            execution.internal_error = Some("worker has no attached stdin".into());
            if let Err(error) = child.finish(true).await {
                execution.containment_cleanup_confirmed = false;
                execution.internal_error = Some(format!(
                    "worker has no attached stdin; container cleanup failed: {error}"
                ));
            }
            return execution;
        }
    };
    let mut stdout = match child.take_stdout() {
        Some(stdout) => stdout,
        None => {
            execution.internal_error = Some("worker has no attached stdout".into());
            if let Err(error) = child.finish(true).await {
                execution.containment_cleanup_confirmed = false;
                execution.internal_error = Some(format!(
                    "worker has no attached stdout; container cleanup failed: {error}"
                ));
            }
            return execution;
        }
    };
    let mut stderr = child.take_stderr();
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        if let Some(stderr) = &mut stderr {
            let _ = stderr.read_to_end(&mut bytes).await;
        }
        bytes
    });
    if let Err(error) = write_frame(&mut stdin, &init).await {
        execution.internal_error = Some(format!("cannot initialize worker: {error}"));
        if let Err(cleanup) = child.finish(true).await {
            execution.containment_cleanup_confirmed = false;
            execution.internal_error = Some(format!(
                "cannot initialize worker: {error}; container cleanup failed: {cleanup}"
            ));
        }
        stderr_task.abort();
        return execution;
    }

    enum DriveOutcome {
        Finished(Result<(), RunnerError>),
        TimedOut(Result<(), RunnerError>),
        Cancelled(Result<(), RunnerError>),
    }
    let provider_cancel = CancellationToken::new();
    let drive = {
        let mut broker = Broker {
            image: &options.image,
            volume,
            provider: Arc::clone(&trusted.provider),
            model: Arc::clone(&trusted.model),
            trusted_options: trusted.stream_options.clone(),
            session_id: init.session_id,
            max_requests: options.max_model_responses,
            max_output_tokens: init.max_output_tokens,
            expected_tools,
            expected_system_prompt: expected_context.system_prompt.unwrap_or_default(),
            expected_prompt: init.prompt,
            reasoning: trusted.reasoning,
            provider_cancel: provider_cancel.clone(),
            execution: &mut execution,
        };
        let broker_future = broker.run(&mut stdout, &mut stdin);
        tokio::pin!(broker_future);
        let timeout = tokio::time::sleep(options.timeout);
        tokio::pin!(timeout);
        tokio::select! {
            result = &mut broker_future => DriveOutcome::Finished(result),
            () = &mut timeout => {
                provider_cancel.cancel();
                DriveOutcome::TimedOut(drain_cancelled_broker(&mut broker_future).await)
            },
            () = run_cancel.cancelled() => {
                provider_cancel.cancel();
                DriveOutcome::Cancelled(drain_cancelled_broker(&mut broker_future).await)
            },
        }
    };
    let kill = !matches!(drive, DriveOutcome::Finished(Ok(())));
    match &drive {
        DriveOutcome::TimedOut(result) => {
            execution.timed_out = true;
            if let Err(error) = result
                && !benign_broker_cancellation(error, &provider_cancel)
            {
                execution.internal_error = Some(error.to_string());
            }
        }
        DriveOutcome::Cancelled(result) => {
            execution.cancelled = true;
            if let Err(error) = result
                && !benign_broker_cancellation(error, &provider_cancel)
            {
                execution.internal_error = Some(error.to_string());
            }
        }
        DriveOutcome::Finished(Err(error)) => {
            execution.internal_error = Some(error.to_string());
        }
        DriveOutcome::Finished(Ok(())) => {}
    }
    drop(stdin);
    match child.finish(kill).await {
        Ok(status) if status.success() || kill => {}
        Ok(status) => execution.internal_error = Some(format!("worker exited with {status}")),
        Err(error) => {
            execution.containment_cleanup_confirmed = false;
            execution.internal_error = Some(format!("worker container cleanup failed: {error}"));
        }
    }
    if let Ok(stderr) = stderr_task.await
        && !stderr.is_empty()
        && execution.internal_error.is_some()
    {
        execution.provider_errors.push(format!(
            "worker stderr: {}",
            String::from_utf8_lossy(&stderr)
        ));
    }
    execution
}

async fn drain_cancelled_broker<F>(future: &mut F) -> Result<(), RunnerError>
where
    F: std::future::Future<Output = Result<(), RunnerError>> + Unpin,
{
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .map_err(|_| RunnerError("provider request did not terminate after cancellation".into()))?
}

fn expected_tools(variant: DescriptionVariant, family: Option<&str>) -> Vec<ToolDefinition> {
    let disabled = vec!["agent".to_string()];
    let mut tools = builtin_tools_for_model(
        &BuiltinToolOptions {
            image_auto_resize: true,
            bash_rtk: false,
        },
        &disabled,
        family,
    );
    if let Some(tool) = tools.iter_mut().find(|tool| tool.name == "apply_patch") {
        tool.description = load(variant).content;
    }
    tools
        .into_iter()
        .map(|tool| ToolDefinition {
            name: tool.name,
            description: tool.description,
            parameters: tool.input_schema,
        })
        .collect()
}

struct Broker<'a> {
    image: &'a str,
    volume: &'a FixtureVolume,
    provider: Arc<dyn Provider>,
    model: Arc<ModelInfo>,
    trusted_options: StreamOptions,
    session_id: String,
    max_requests: u32,
    max_output_tokens: u64,
    expected_tools: Vec<ToolDefinition>,
    expected_system_prompt: String,
    expected_prompt: String,
    reasoning: ThinkingLevel,
    provider_cancel: CancellationToken,
    execution: &'a mut TrialExecution,
}

impl Broker<'_> {
    async fn run(
        &mut self,
        stdout: &mut tokio::process::ChildStdout,
        stdin: &mut tokio::process::ChildStdin,
    ) -> Result<(), RunnerError> {
        loop {
            let request: WorkerRequest = read_frame(stdout)
                .await
                .map_err(|error| RunnerError(error.to_string()))?
                .ok_or_else(|| RunnerError("worker closed without a result".into()))?;
            match request {
                WorkerRequest::Provider {
                    id,
                    context,
                    observed_reasoning,
                } => {
                    self.provider_request(id, context, observed_reasoning, stdin)
                        .await?;
                }
                WorkerRequest::Tool {
                    id,
                    name,
                    arguments,
                } => self.tool_request(id, name, arguments, stdin).await?,
                WorkerRequest::Finished { result } => {
                    self.execution.worker = Some(result);
                    return Ok(());
                }
            }
        }
    }

    async fn provider_request(
        &mut self,
        id: u64,
        context: Context,
        observed_reasoning: ThinkingLevel,
        stdin: &mut tokio::process::ChildStdin,
    ) -> Result<(), RunnerError> {
        self.snapshot_boundary().await?;
        self.execution.provider_requests = self.execution.provider_requests.saturating_add(1);
        let request_number = self.execution.provider_requests;
        if observed_reasoning != self.reasoning {
            return Err(RunnerError("worker requested non-frozen reasoning".into()));
        }
        if request_number > self.max_requests {
            self.provider_cancel.cancel();
            return Err(RunnerError(
                "parent provider request limit reached before a paid call".into(),
            ));
        }
        if serde_json::to_value(&context.tools).ok()
            != serde_json::to_value(&self.expected_tools).ok()
        {
            return Err(RunnerError(
                "worker provider context has unexpected tool descriptions or schemas".into(),
            ));
        }
        if context.system_prompt.as_deref() != Some(&self.expected_system_prompt)
            || context.messages.first().and_then(|message| match message {
                Message::User(user) => user.content.first().and_then(|content| match content {
                    aj_models::types::UserContent::Text(text) => Some(text.text.as_str()),
                    aj_models::types::UserContent::Image(_) => None,
                }),
                _ => None,
            }) != Some(&self.expected_prompt)
        {
            return Err(RunnerError(
                "worker provider context has an unexpected system prompt or first message".into(),
            ));
        }

        let payloads = Arc::new(Mutex::new(Vec::<Result<(Value, String), String>>::new()));
        let captured = Arc::clone(&payloads);
        let expected_tools = self.expected_tools.clone();
        let expected_session = self.session_id.clone();
        let model = Arc::clone(&self.model);
        let payload_cancel = self.provider_cancel.clone();
        let reasoning = self.reasoning;
        let mut options = self.trusted_options.clone();
        options.session_id = Some(self.session_id.clone());
        options.max_tokens = Some(self.max_output_tokens);
        options.on_payload = Some(OnPayload::new(move |payload| {
            let validation = validate_payload(
                payload,
                &expected_tools,
                &expected_session,
                &model,
                reasoning,
            )
            .and_then(|()| {
                normalized_request_hash(payload, &expected_session)
                    .map(|hash| (payload.clone(), hash))
            })
            .map_err(|error| error.to_string());
            if validation.is_err() {
                payload_cancel.cancel();
            }
            captured
                .lock()
                .expect("payload capture mutex poisoned")
                .push(validation);
        }));
        options.cancel = Some(self.provider_cancel.clone());
        let simple = SimpleStreamOptions {
            base: options,
            reasoning: self.reasoning,
        };
        let mut stream = self.provider.stream_simple(&self.model, &context, &simple);
        let mut events = Vec::new();
        let mut buffered_bytes = 0_usize;
        while let Some(event) = stream.next().await {
            let terminal = event.is_terminal();
            let event_bytes = serde_json::to_vec(&event)
                .map_err(|error| RunnerError(format!("cannot size provider event: {error}")))?
                .len();
            buffered_bytes = buffered_bytes
                .checked_add(event_bytes)
                .ok_or_else(|| RunnerError("provider event buffer size overflow".into()))?;
            if buffered_bytes > MAX_FRAME_BYTES * 4 || events.len() >= 100_000 {
                self.provider_cancel.cancel();
                while stream.next().await.is_some() {}
                return Err(RunnerError(
                    "provider event buffer exceeded its fixed bound".into(),
                ));
            }
            events.push(event);
            if terminal {
                break;
            }
        }
        if !events
            .last()
            .is_some_and(AssistantMessageEvent::is_terminal)
        {
            return Err(RunnerError("provider stream had no terminal event".into()));
        }
        if let Some(terminal) = events.last() {
            self.execution.account_provider_terminal(terminal.partial());
            let aggregate_ceiling = self
                .max_output_tokens
                .saturating_mul(u64::from(self.max_requests));
            if self.execution.parent_metrics.usage.output > aggregate_ceiling {
                self.provider_cancel.cancel();
                return Err(RunnerError(format!(
                    "aggregate observed output usage {} exceeds cap {}",
                    self.execution.parent_metrics.usage.output, aggregate_ceiling
                )));
            }
        }
        let payload_results = payloads
            .lock()
            .expect("payload capture mutex poisoned")
            .clone();
        let mut payloads = Vec::new();
        for payload in payload_results {
            let (payload, normalized_hash) = payload.map_err(RunnerError)?;
            self.execution.payload_hashes.push(
                serde_json::to_vec(&payload)
                    .map(|bytes| sha256_hex(&bytes))
                    .map_err(|error| RunnerError(error.to_string()))?,
            );
            self.execution
                .normalized_model_context_hashes
                .push(normalized_hash);
            payloads.push(payload);
        }
        if !payloads.is_empty() {
            self.execution.first_paid_request_validated = true;
        }
        for event in events {
            write_frame(stdin, &ParentResponse::ProviderEvent { id, event })
                .await
                .map_err(|error| RunnerError(error.to_string()))?;
        }
        self.snapshot_boundary().await
    }

    async fn tool_request(
        &mut self,
        id: u64,
        name: String,
        arguments: Value,
        stdin: &mut tokio::process::ChildStdin,
    ) -> Result<(), RunnerError> {
        let before = match snapshot_volume(
            self.image,
            self.volume,
            false,
            None,
            Some(&self.provider_cancel),
        )
        .await
        {
            Ok(output) => output.snapshot,
            Err(error) => {
                if !clean_helper_cancellation(&error, &self.provider_cancel) {
                    self.execution.containment_cleanup_confirmed = false;
                }
                return Err(error);
            }
        };
        self.execution.record_between_boundary(before.clone());
        let arguments_bytes =
            serde_json::to_vec(&arguments).map_err(|error| RunnerError(error.to_string()))?;
        let arguments_sha256 = sha256_hex(&arguments_bytes);
        let mut invoked = true;
        let outcome = if name == "apply_patch"
            && serde_json::from_value::<ApplyPatchInput>(arguments.clone()).is_err()
        {
            invoked = false;
            tool_error("apply_patch input did not match ApplyPatchInput")
        } else if name == "bash"
            && serde_json::from_value::<BashInput>(arguments.clone())
                .is_ok_and(|input| input.run_in_background)
        {
            invoked = false;
            tool_error("background bash is disabled for this evaluation")
        } else {
            match run_helper_cancellable(
                self.image,
                "__tool-worker",
                self.volume,
                false,
                &ToolWorkerInput {
                    name: name.clone(),
                    arguments: arguments.clone(),
                },
                &self.provider_cancel,
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    if !clean_helper_cancellation(&error, &self.provider_cancel) {
                        self.execution.containment_cleanup_confirmed = false;
                    }
                    return Err(error.into());
                }
            }
        };
        let after = match snapshot_volume(
            self.image,
            self.volume,
            false,
            None,
            Some(&self.provider_cancel),
        )
        .await
        {
            Ok(output) => output.snapshot,
            Err(error) => {
                if !clean_helper_cancellation(&error, &self.provider_cancel) {
                    self.execution.containment_cleanup_confirmed = false;
                }
                return Err(error);
            }
        };
        let tool_delta = delta(&before, &after);
        let changed = !tool_delta.paths.is_empty();
        let attribution = if changed && name == "apply_patch" {
            MutationAttribution::ApplyPatch
        } else if changed {
            self.execution.edit_bypass = true;
            MutationAttribution::NonPatchTool
        } else {
            MutationAttribution::NoMutation
        };
        self.execution.push_mutation(
            Some(id),
            Some(name.clone()),
            Some(arguments_sha256.clone()),
            attribution,
            tool_delta.clone(),
        );
        if name == "apply_patch" {
            let result_text = wire_text(&outcome);
            let classification = classify_patch(invoked, &outcome, &result_text, changed);
            let sequence = u64::try_from(self.execution.patch_calls.len() + 1)
                .map_err(|_| RunnerError("patch call sequence overflow".into()))?;
            self.execution.patch_calls.push(PatchCallRecord {
                sequence,
                request_id: id,
                arguments_sha256,
                invoked,
                is_error: outcome.is_error,
                result_text,
                classification,
                delta: tool_delta,
            });
            if classification.failed() {
                self.execution.parent_metrics.apply_patch_failures += 1;
            }
        }
        self.execution.final_snapshot = after;
        self.execution.tool_outcomes.push(ToolOutcomeRecord {
            request_id: id,
            tool: name.clone(),
            content: outcome.content.clone(),
            details: outcome.details.clone(),
            is_error: outcome.is_error,
        });
        if name == "apply_patch" {
            self.execution.parent_metrics.apply_patch_attempts += 1;
        }
        self.execution
            .parent_metrics
            .transcript_wire_messages
            .push(Message::ToolResult(ToolResultMessage {
                tool_call_id: id.to_string(),
                tool_name: name,
                content: outcome.content.clone(),
                details: Some(outcome.details.clone()),
                is_error: outcome.is_error,
                timestamp: 0,
            }));
        write_frame(stdin, &ParentResponse::ToolResult { id, outcome })
            .await
            .map_err(|error| RunnerError(error.to_string()))
    }

    async fn snapshot_boundary(&mut self) -> Result<(), RunnerError> {
        let snapshot = match snapshot_volume(
            self.image,
            self.volume,
            false,
            None,
            Some(&self.provider_cancel),
        )
        .await
        {
            Ok(output) => output.snapshot,
            Err(error) => {
                if !clean_helper_cancellation(&error, &self.provider_cancel) {
                    self.execution.containment_cleanup_confirmed = false;
                }
                return Err(error);
            }
        };
        self.execution.record_between_boundary(snapshot);
        Ok(())
    }
}

fn validate_payload(
    payload: &Value,
    expected_tools: &[ToolDefinition],
    session_id: &str,
    model: &ModelInfo,
    reasoning: ThinkingLevel,
) -> Result<(), RunnerError> {
    if !payload_has_reasoning(payload, model, reasoning) {
        return Err(RunnerError(
            "captured paid request does not send frozen reasoning effort".into(),
        ));
    }
    if payload.get("prompt_cache_key").and_then(Value::as_str) != Some(session_id) {
        return Err(RunnerError(
            "captured paid request does not use the trusted trial cache key".into(),
        ));
    }
    let tools = payload
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| RunnerError("captured paid request has no tools array".into()))?;
    if tools.len() != expected_tools.len() {
        return Err(RunnerError(
            "captured paid request tool count differs".into(),
        ));
    }
    for (actual, expected) in tools.iter().zip(expected_tools) {
        if actual.get("name").and_then(Value::as_str) != Some(&expected.name)
            || actual.get("description").and_then(Value::as_str) != Some(&expected.description)
            || actual.get("parameters") != Some(&expected.parameters)
        {
            return Err(RunnerError(format!(
                "captured paid request tool differs at {}",
                expected.name
            )));
        }
    }
    Ok(())
}

fn tool_error(message: &str) -> ToolOutcomeWire {
    ToolOutcomeWire {
        content: vec![aj_models::types::UserContent::text(message)],
        details: serde_json::json!({"kind": "text", "summary": "error", "body": message}),
        is_error: true,
    }
}

fn classify_patch(
    invoked: bool,
    outcome: &ToolOutcomeWire,
    result: &str,
    changed: bool,
) -> PatchClassification {
    if !invoked {
        PatchClassification::SchemaError
    } else if outcome.is_error && changed {
        PatchClassification::PartialApplication
    } else if !outcome.is_error {
        PatchClassification::Success
    } else if result.starts_with("apply_patch verification failed:") {
        PatchClassification::FormatError
    } else if result.starts_with("patch rejected:") {
        PatchClassification::Rejected
    } else {
        PatchClassification::ApplicationError
    }
}

impl TrialExecution {
    fn new(fixture: GeneratedFixture, baseline: FilesystemSnapshot, cache_key: String) -> Self {
        let final_delta = delta(&baseline, &baseline);
        let mut parent_metrics = WorkerMetrics::default();
        parent_metrics
            .transcript_wire_messages
            .push(Message::User(UserMessage::text(fixture.prompt.clone())));
        Self {
            fixture,
            baseline: baseline.clone(),
            final_snapshot: baseline.clone(),
            final_snapshot_valid: false,
            final_delta,
            worker: None,
            parent_metrics,
            mutation_ledger: Vec::new(),
            patch_calls: Vec::new(),
            tool_outcomes: Vec::new(),
            edit_bypass: false,
            payload_hashes: Vec::new(),
            provider_errors: Vec::new(),
            provider_error_details: Vec::new(),
            provider_infrastructure_failed: false,
            first_paid_request_validated: false,
            internal_error: None,
            timed_out: false,
            cancelled: false,
            duration: Duration::ZERO,
            verifier: None,
            cache_key,
            baseline_commit: None,
            git_artifacts: None,
            normalized_model_context_hashes: Vec::new(),
            provider_requests: 0,
            containment_cleanup_confirmed: true,
            system_prompt_hash: String::new(),
        }
    }

    fn push_mutation(
        &mut self,
        request_id: Option<u64>,
        tool: Option<String>,
        arguments_sha256: Option<String>,
        attribution: MutationAttribution,
        change: SnapshotDelta,
    ) {
        let sequence = u64::try_from(self.mutation_ledger.len() + 1).unwrap_or(u64::MAX);
        self.mutation_ledger.push(MutationLedgerEntry {
            sequence,
            request_id,
            tool,
            arguments_sha256,
            attribution,
            delta: change,
        });
    }

    fn record_between_boundary(&mut self, snapshot: FilesystemSnapshot) {
        let change = delta(&self.final_snapshot, &snapshot);
        if !change.paths.is_empty() {
            self.edit_bypass = true;
            self.push_mutation(
                None,
                None,
                None,
                MutationAttribution::BetweenBoundaries,
                change,
            );
        }
        self.final_snapshot = snapshot.clone();
    }

    fn account_provider_terminal(&mut self, assistant: &aj_models::types::AssistantMessage) {
        if self
            .patch_calls
            .iter()
            .any(|patch| patch.classification.failed())
        {
            self.parent_metrics.recovery_rounds += 1;
        }
        self.parent_metrics.usage.accumulate(&assistant.usage);
        self.parent_metrics.model_responses += 1;
        let tool_calls = assistant
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !tool_calls.is_empty() {
            self.parent_metrics.tool_rounds += 1;
        }
        self.parent_metrics.total_tool_calls += u64::try_from(tool_calls.len()).unwrap_or(u64::MAX);
        for call in tool_calls {
            *self
                .parent_metrics
                .tool_calls_by_name
                .entry(call.name.clone())
                .or_default() += 1;
        }
        self.parent_metrics.final_assistant_text = assistant
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        if let Some(error) = &assistant.error {
            self.provider_errors.push(error.message.clone());
            self.provider_error_details.push(ProviderErrorRecord {
                category: error.category,
                message: error.message.clone(),
                retry_after_ms: error.retry_after_ms,
                http_status: error.http_status,
            });
            self.parent_metrics
                .provider_errors
                .push(error.message.clone());
            self.provider_infrastructure_failed = matches!(
                error.category,
                ErrorCategory::RateLimit
                    | ErrorCategory::Auth
                    | ErrorCategory::Overloaded
                    | ErrorCategory::Transient
            );
        } else {
            self.provider_infrastructure_failed = false;
        }
        self.parent_metrics
            .transcript_wire_messages
            .push(Message::Assistant(assistant.clone()));
    }
}

fn finish_runtime(
    mut execution: TrialExecution,
    trusted: &TrustedModel,
    blobs: &BlobStore,
    limits: RuntimeLimits,
    run_state: &FrozenRunState,
    catalog_pair_reserve: f64,
) -> Result<RuntimeRecord, RunnerError> {
    if let Some(worker) = &execution.worker
        && !usage_bits_equal(&worker.metrics.usage, &execution.parent_metrics.usage)
    {
        execution.internal_error = Some("worker and parent usage accounting disagree".into());
    }
    let completed_provider_request = execution
        .parent_metrics
        .transcript_wire_messages
        .iter()
        .any(
            |message| matches!(message, Message::Assistant(assistant) if assistant.error.is_none()),
        );
    if completed_provider_request && !execution.first_paid_request_validated {
        execution.internal_error = Some("no captured paid request was validated".into());
    }
    let sessions_with_patch_failure = execution
        .patch_calls
        .iter()
        .any(|patch| patch.classification.failed());
    let usage = execution.parent_metrics.usage.clone();
    let mut sensitivity_usage = usage.clone();
    sensitivity_usage.cache_write = sensitivity_usage.cache_write.saturating_add(usage.input);
    calculate_cost(&trusted.model.cost, &mut sensitivity_usage);
    let cache_stratum = if usage.cache_read > 0 {
        CacheStratum::PositiveRead
    } else {
        CacheStratum::UnknownRead
    };
    let transcript = execution.worker.as_ref().map_or_else(
        || execution.parent_metrics.transcript_wire_messages.clone(),
        |worker| worker.metrics.transcript_wire_messages.clone(),
    );
    let final_snapshot_blob = if execution.final_snapshot_valid {
        match serde_json::to_vec(&execution.final_snapshot) {
            Ok(bytes) => store_blob(
                blobs,
                &bytes,
                "final snapshot",
                &mut execution.internal_error,
            ),
            Err(error) => {
                execution.internal_error =
                    Some(format!("cannot serialize final snapshot: {error}"));
                None
            }
        }
    } else {
        None
    };
    let conversation_jsonl_blob = match conversation_jsonl(&transcript) {
        Ok(bytes) => store_blob(
            blobs,
            &bytes,
            "conversation JSONL",
            &mut execution.internal_error,
        ),
        Err(error) => {
            execution.internal_error = Some(error.to_string());
            None
        }
    };
    let final_assistant_text_blob = store_blob(
        blobs,
        execution.parent_metrics.final_assistant_text.as_bytes(),
        "final assistant text",
        &mut execution.internal_error,
    );
    let (final_diff_blob, final_status_blob) =
        execution
            .git_artifacts
            .as_ref()
            .map_or((None, None), |git| {
                (
                    store_blob(
                        blobs,
                        &git.diff,
                        "final Git diff",
                        &mut execution.internal_error,
                    ),
                    store_blob(
                        blobs,
                        &git.status,
                        "final Git status",
                        &mut execution.internal_error,
                    ),
                )
            });
    let worker_terminal = execution.worker.as_ref().map(|worker| worker.terminal);
    let verifier_passed = execution
        .verifier
        .as_ref()
        .is_some_and(|verifier| verifier.report.passed);
    let status = if execution.internal_error.is_some()
        || worker_terminal == Some(WorkerTerminal::RunnerInternal)
    {
        TerminalStatus::RunnerInternal
    } else if worker_terminal == Some(WorkerTerminal::ModelFailed)
        && execution.provider_infrastructure_failed
    {
        TerminalStatus::InfrastructureFailed
    } else if execution.cancelled || worker_terminal == Some(WorkerTerminal::Cancelled) {
        TerminalStatus::Cancelled
    } else if execution.timed_out {
        TerminalStatus::TimedOut
    } else if worker_terminal == Some(WorkerTerminal::TurnLimit) {
        TerminalStatus::TurnLimit
    } else if worker_terminal == Some(WorkerTerminal::ModelFailed) || worker_terminal.is_none() {
        TerminalStatus::ModelFailed
    } else if !verifier_passed {
        TerminalStatus::VerifierFailed
    } else {
        TerminalStatus::Passed
    };
    let valid = status.valid();
    let task_passed = status == TerminalStatus::Passed;
    let verifier = execution.verifier.map(|verifier| VerifierRecord {
        report: verifier.report,
        command_result: verifier.command_result,
        before_root_hash: verifier.before.root_hash,
        after_root_hash: verifier.after.root_hash,
        mutations: verifier.mutations,
    });
    let metrics = execution
        .worker
        .as_ref()
        .map_or(&execution.parent_metrics, |worker| &worker.metrics);
    let stream_retries = metrics.stream_retries;
    let total_tool_calls = metrics.total_tool_calls;
    let tool_calls_by_name = metrics.tool_calls_by_name.clone();
    let normalized_first_request_hash = execution.normalized_model_context_hashes.first().cloned();
    let changed_paths = if execution.final_snapshot_valid {
        execution
            .final_delta
            .paths
            .iter()
            .map(|change| change.path.clone())
            .collect()
    } else {
        Vec::new()
    };
    let final_snapshot = execution
        .final_snapshot_valid
        .then_some(execution.final_snapshot.clone());
    let final_delta = execution
        .final_snapshot_valid
        .then_some(execution.final_delta.clone());
    let mut evaluator_api_limitations = vec![
        "The production serialized payload callback runs after provider request construction. The equivalent typed request is validated before credentials, and an invalid production payload is cancelled and drained before the trial fails closed.".into(),
        "Normalized Usage does not preserve cache-read or cache-write field presence. Zero cache-read usage has unknown presence, and the cache-write sensitivity range spans zero through reported input tokens.".into(),
    ];
    if trusted.model.provider == PROVIDER_ID {
        evaluator_api_limitations.push(
            "The public Codex provider omits StreamOptions.max_tokens from its wire request. The catalog or server maximum is therefore the per-request output ceiling. The parent also enforces request count and fails closed after completed normalized usage first exceeds the aggregate observed output-token ceiling. Tokens emitted before completed usage is reported cannot be interrupted by that observed-usage check.".into(),
        );
    }
    Ok(RuntimeRecord {
        terminal_status: status,
        valid,
        task_passed,
        sessions_with_patch_failure,
        edit_bypass: execution.edit_bypass,
        aj_recorded_catalog_cost: usage.cost.total,
        model_responses: execution.parent_metrics.model_responses,
        provider_requests: execution.provider_requests,
        usage: usage.clone(),
        usage_field_presence: UsageFieldPresence {
            input: None,
            output: None,
            cache_read: None,
            cache_write: None,
            source: "normalized provider usage does not expose raw field presence".into(),
        },
        cache_stratum,
        cache_write_sensitivity: CacheWriteSensitivity {
            lower_aj_recorded_catalog_cost: usage.cost.total,
            upper_aj_recorded_catalog_cost: sensitivity_usage.cost.total,
            upper_assumed_cache_write_tokens: usage.input,
        },
        limits,
        first_response_aj_recorded_catalog_cost: execution
            .parent_metrics
            .transcript_wire_messages
            .iter()
            .find_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant.usage.cost.total),
                _ => None,
            }),
        tool_rounds: execution.parent_metrics.tool_rounds,
        total_tool_calls,
        tool_calls_by_name,
        apply_patch_attempts: u64::try_from(execution.patch_calls.len()).unwrap_or(u64::MAX),
        successful_patch_calls: u64::try_from(
            execution
                .patch_calls
                .iter()
                .filter(|patch| patch.classification == PatchClassification::Success)
                .count(),
        )
        .unwrap_or(u64::MAX),
        recovery_rounds: execution.parent_metrics.recovery_rounds,
        stream_retries,
        duration_millis: u64::try_from(execution.duration.as_millis()).unwrap_or(u64::MAX),
        final_assistant_text: execution.parent_metrics.final_assistant_text,
        prompt: execution.fixture.prompt,
        verifier_command: execution
            .fixture
            .visible_check
            .map(|command| command.argv),
        patch_calls: execution.patch_calls,
        tool_outcomes: execution.tool_outcomes,
        mutation_ledger: execution.mutation_ledger,
        baseline_root_hash: Some(execution.baseline.root_hash.clone()),
        final_snapshot,
        final_snapshot_blob,
        final_delta,
        baseline_commit: execution.baseline_commit,
        final_diff_blob,
        final_status_blob,
        changed_paths,
        verifier,
        payload_hashes: execution.payload_hashes,
        normalized_model_context_hashes: execution.normalized_model_context_hashes,
        normalized_first_request_hash,
        system_prompt_hash: execution.system_prompt_hash,
        cache_key_hash: sha256_hex(execution.cache_key.as_bytes()),
        transcript_wire_messages: transcript,
        conversation_jsonl_blob,
        provider_errors: execution.provider_errors,
        provider_error_details: execution.provider_error_details,
        worker_error: execution
            .internal_error
            .or_else(|| execution.worker.and_then(|worker| worker.error)),
        containment_cleanup_confirmed: execution.containment_cleanup_confirmed,
        isolation_contract: "docker_attach_v2: parent-named containers, explicit kill/wait/rm, network=none, read_only_root, cap_drop=all, no_new_privileges, bounded_tmpfs_volume, fixed_pids_memory_cpu_ulimits".into(),
        evaluator_api_limitations,
        image_id: run_state.image_id.clone(),
        source_provenance: run_state.source.clone(),
        utc_date: run_state.utc_date.clone(),
        conservative_catalog_pair_reserve: catalog_pair_reserve,
        final_assistant_text_blob,
    })
}

fn usage_bits_equal(left: &Usage, right: &Usage) -> bool {
    let left_costs = [
        left.cost.input,
        left.cost.output,
        left.cost.cache_read,
        left.cost.cache_write,
        left.cost.total,
    ];
    let right_costs = [
        right.cost.input,
        right.cost.output,
        right.cost.cache_read,
        right.cost.cache_write,
        right.cost.total,
    ];
    left.input == right.input
        && left.output == right.output
        && left.cache_read == right.cache_read
        && left.cache_write == right.cache_write
        && left.total_tokens == right.total_tokens
        && left_costs.iter().all(|value| value.is_finite())
        && right_costs.iter().all(|value| value.is_finite())
        && left_costs
            .iter()
            .zip(right_costs)
            .all(|(left, right)| left.to_bits() == right.to_bits())
}

fn store_blob(
    blobs: &BlobStore,
    bytes: &[u8],
    label: &str,
    internal_error: &mut Option<String>,
) -> Option<String> {
    match blobs.put(bytes) {
        Ok(hash) => Some(hash),
        Err(error) => {
            *internal_error = Some(format!("cannot store {label}: {error}"));
            None
        }
    }
}

fn conversation_jsonl(messages: &[Message]) -> Result<Vec<u8>, RunnerError> {
    let mut bytes = Vec::new();
    for message in messages {
        serde_json::to_writer(&mut bytes, message).map_err(|error| {
            RunnerError(format!("cannot serialize conversation JSONL: {error}"))
        })?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{freeze_plan, freeze_schedule, test_model_selection};
    use crate::suite::committed_manifest;

    fn model_plan(seed: &str) -> FrozenPlan {
        let model = freeze_model_selection(PROVIDER_ID, MODEL_ID, "low").unwrap();
        freeze_plan(&committed_manifest().unwrap(), seed, 6, model).unwrap()
    }

    fn timeout_runtime(provider_requests: u32, cleanup_confirmed: bool) -> RuntimeRecord {
        let temp = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(temp.path()).unwrap();
        let run_state = FrozenRunState {
            utc_date: "2026-07-24".into(),
            image_id: "sha256:image".into(),
            source: SourceProvenance {
                head: "head".into(),
                dirty: false,
                worktree_hash: None,
            },
        };
        let mut runtime = setup_failure_runtime(
            "cancelled".into(),
            "cache".into(),
            RuntimeLimits {
                wall_timeout_seconds: 10,
                max_provider_requests: 2,
                max_model_responses: 2,
                provider_output_token_ceiling: 100,
                aggregate_observed_output_token_ceiling: 200,
            },
            &run_state,
            &blobs,
            1.0,
            cleanup_confirmed,
        );
        runtime.terminal_status = TerminalStatus::Cancelled;
        runtime.provider_requests = provider_requests;
        runtime.usage.output = 7;
        runtime.usage.total_tokens = 7;
        runtime.aj_recorded_catalog_cost = 0.25;
        runtime
    }

    #[test]
    fn excluded_phases_require_unplanned_state_and_complete_smoke() {
        let manifest = committed_manifest().unwrap();
        let plan = freeze_plan(&manifest, "phase-state", 6, test_model_selection()).unwrap();
        require_unplanned_phase(&plan, "smoke").unwrap();
        let error = require_complete_pairs(
            &plan.schedule.smoke,
            &crate::artifacts::ResumeState::default(),
            &plan.schedule.schedule_hash,
            "pilot requires smoke",
        )
        .unwrap_err();
        assert!(error.to_string().contains("pilot requires smoke"));

        let mut invalid = plan.clone();
        invalid.schedule = freeze_schedule(
            &manifest,
            &invalid.universe,
            &invalid.model.as_ref().unwrap().selection_hash,
            1,
        )
        .unwrap();
        assert!(require_unplanned_phase(&invalid, "pilot").is_err());
    }

    #[test]
    fn whole_trial_deadline_preserves_usage_and_requires_cleanup() {
        let mut before_request = timeout_runtime(0, true);
        apply_deadline_outcome(&mut before_request, true, Duration::from_secs(10));
        assert_eq!(
            before_request.terminal_status,
            TerminalStatus::InfrastructureFailed
        );
        assert!(!before_request.valid);

        let mut paid = timeout_runtime(1, true);
        paid.normalized_first_request_hash = Some("captured".into());
        apply_deadline_outcome(&mut paid, true, Duration::from_secs(10));
        assert_eq!(paid.terminal_status, TerminalStatus::TimedOut);
        assert!(paid.valid);
        assert_eq!(paid.usage.output, 7);
        assert_eq!(paid.aj_recorded_catalog_cost, 0.25);

        let mut cleanup_failed = timeout_runtime(1, false);
        cleanup_failed.normalized_first_request_hash = Some("captured".into());
        apply_deadline_outcome(&mut cleanup_failed, true, Duration::from_secs(10));
        assert_eq!(
            cleanup_failed.terminal_status,
            TerminalStatus::RunnerInternal
        );
        assert!(!cleanup_failed.valid);

        let mut runner_failure = timeout_runtime(1, true);
        runner_failure.normalized_first_request_hash = Some("captured".into());
        mark_runner_internal(&mut runner_failure, "broken final artifact");
        apply_deadline_outcome(&mut runner_failure, true, Duration::from_secs(10));
        assert_eq!(
            runner_failure.terminal_status,
            TerminalStatus::RunnerInternal
        );

        let mut provider_failure = timeout_runtime(1, true);
        provider_failure.normalized_first_request_hash = Some("captured".into());
        provider_failure.terminal_status = TerminalStatus::InfrastructureFailed;
        apply_deadline_outcome(&mut provider_failure, true, Duration::from_secs(10));
        assert_eq!(
            provider_failure.terminal_status,
            TerminalStatus::InfrastructureFailed
        );
        assert!(!provider_failure.valid);

        let temp = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(temp.path()).unwrap();
        let cancelled = setup_cancelled_runtime(
            "cancelled during setup".into(),
            "cache".into(),
            RuntimeLimits {
                wall_timeout_seconds: 10,
                max_provider_requests: 2,
                max_model_responses: 2,
                provider_output_token_ceiling: 100,
                aggregate_observed_output_token_ceiling: 200,
            },
            &FrozenRunState {
                utc_date: "2026-07-24".into(),
                image_id: "sha256:image".into(),
                source: SourceProvenance {
                    head: "head".into(),
                    dirty: false,
                    worktree_hash: None,
                },
            },
            &blobs,
            1.0,
            true,
        );
        assert_eq!(cancelled.terminal_status, TerminalStatus::Cancelled);
        assert!(!cancelled.valid);

        let corrupt = tempfile::tempdir().unwrap();
        std::fs::write(corrupt.path().join(sha256_hex(&[])), b"corrupt").unwrap();
        let corrupt_blobs = BlobStore::new(corrupt.path()).unwrap();
        let artifact_failure = setup_cancelled_runtime(
            "cancelled during setup".into(),
            "cache".into(),
            cancelled.limits,
            &FrozenRunState {
                utc_date: "2026-07-24".into(),
                image_id: "sha256:image".into(),
                source: SourceProvenance {
                    head: "head".into(),
                    dirty: false,
                    worktree_hash: None,
                },
            },
            &corrupt_blobs,
            1.0,
            true,
        );
        assert_eq!(
            artifact_failure.terminal_status,
            TerminalStatus::RunnerInternal
        );

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(benign_broker_cancellation(
            &RunnerError("provider request did not terminate after cancellation".into()),
            &cancellation
        ));
    }

    #[test]
    fn usage_reconciliation_compares_every_cost_bit() {
        let mut left = Usage::default();
        let mut right = Usage::default();
        left.input = 1;
        right.input = 1;
        assert!(usage_bits_equal(&left, &right));

        right.cost.cache_write = -0.0;
        assert!(!usage_bits_equal(&left, &right));
        right.cost.cache_write = 0.0;
        right.cost.total = f64::NAN;
        assert!(!usage_bits_equal(&left, &right));
    }

    #[test]
    fn freeze_rejects_unknown_models_and_reasoning_levels() {
        assert!(freeze_model_selection("missing", "missing", "low").is_err());
        assert!(freeze_model_selection(PROVIDER_ID, MODEL_ID, "turbo").is_err());
        let frozen = freeze_model_selection(PROVIDER_ID, MODEL_ID, "low").unwrap();
        assert_eq!(frozen.provider, PROVIDER_ID);
        assert_eq!(frozen.model, MODEL_ID);
        assert_eq!(frozen.reasoning, "low");
        assert!(!frozen.tool_catalog_hash.is_empty());
    }

    #[test]
    fn reasoning_off_matches_responses_wire_contract() {
        let registry = ModelRegistry::load();
        let non_reasoning = registry.get("openai", "gpt-4.1").unwrap();
        let reasoning = registry.get("openai", "gpt-5.4").unwrap();
        let context = Context::new("test");
        let non_reasoning_payload = request_shape(
            non_reasoning,
            ThinkingLevel::Off,
            &context,
            "cache",
            non_reasoning.max_tokens,
        );
        let reasoning_payload = request_shape(
            reasoning,
            ThinkingLevel::Off,
            &context,
            "cache",
            reasoning.max_tokens,
        );
        assert!(non_reasoning_payload.get("reasoning").is_none());
        assert_eq!(
            reasoning_payload
                .pointer("/reasoning/effort")
                .and_then(Value::as_str),
            Some("none")
        );
        assert!(payload_has_reasoning(
            &non_reasoning_payload,
            non_reasoning,
            ThinkingLevel::Off
        ));
        assert!(payload_has_reasoning(
            &reasoning_payload,
            reasoning,
            ThinkingLevel::Off
        ));
    }

    #[test]
    fn incompatible_resume_is_rejected_before_resolution_failure_can_append() {
        let plan = model_plan("resume-before-credentials");
        let pair = &plan.schedule.smoke[0];
        let scheduled = &pair.trials[0];
        let frozen = plan.model.as_ref().unwrap();
        let options = RunOptions {
            phase: SchedulePhase::Smoke,
            plan: PathBuf::new(),
            records: PathBuf::new(),
            artifact_dir: PathBuf::new(),
            image: "sha256:image".into(),
            max_cost_usd: 1.0,
            max_trials: 2,
            timeout: Duration::from_secs(10),
            max_model_responses: 2,
        };
        let run_state = FrozenRunState {
            utc_date: "2026-07-24".into(),
            image_id: "sha256:image".into(),
            source: SourceProvenance {
                head: "head".into(),
                dirty: false,
                worktree_hash: None,
            },
        };
        let mut runtime = timeout_runtime(0, true);
        runtime.terminal_status = TerminalStatus::InfrastructureFailed;
        runtime.image_id = run_state.image_id.clone();
        runtime.source_provenance = run_state.source.clone();
        runtime.utc_date = run_state.utc_date.clone();
        runtime.limits = RuntimeLimits {
            wall_timeout_seconds: 10,
            max_provider_requests: 2,
            max_model_responses: 2,
            provider_output_token_ceiling: frozen.max_tokens,
            aggregate_observed_output_token_ceiling: frozen.max_tokens.saturating_mul(2),
        };
        let descriptions = recorded_descriptions();
        let record = TrialRecord::new(
            trial_identity(&plan, pair, scheduled, "attempt"),
            TrialMetadata {
                task_seed: find_instance(&plan, pair).unwrap().task_seed.clone(),
                current_description: descriptions[0].clone(),
                compact_description: descriptions[1].clone(),
                aj_revision: run_state.source.revision_label(),
                suite_revision: plan.universe.suite_revision.clone(),
                model_catalog_hash: frozen.catalog_hash.clone(),
                provider: frozen.provider.clone(),
                model: frozen.model.clone(),
                reasoning_effort: frozen.reasoning.clone(),
                tool_catalog_hash: frozen.tool_catalog_hash.clone(),
                fixture_revision: String::new(),
            },
            serde_json::to_value(runtime).unwrap(),
        )
        .unwrap();
        let mut state = crate::artifacts::ResumeState::default();
        state
            .trials_by_hash
            .insert(record.record_hash.clone(), record.clone());
        validate_resume_before_resolution(&plan, &state, &options, &run_state).unwrap();

        let mut incompatible_runtime = record.runtime.clone();
        incompatible_runtime["image_id"] = Value::String("sha256:other".into());
        let incompatible = TrialRecord::new(
            record.identity.clone(),
            record.metadata.clone(),
            incompatible_runtime,
        )
        .unwrap();
        state.trials_by_hash.clear();
        state
            .trials_by_hash
            .insert(incompatible.record_hash.clone(), incompatible);
        assert!(validate_resume_before_resolution(&plan, &state, &options, &run_state).is_err());
    }

    #[test]
    fn opaque_cache_keys_are_unique_lowercase_hex_and_treatment_blind() {
        let first = opaque_cache_key(&[1; 32]);
        let second = opaque_cache_key(&[2; 32]);
        assert_ne!(first, second);
        for key in [&first, &second] {
            assert!(key.len() <= 64);
            assert!(
                key.bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
            assert!(!key.contains("current"));
            assert!(!key.contains("compact"));
        }
    }

    #[test]
    fn unpaid_contexts_differ_only_in_frozen_description() {
        let plan = model_plan("unpaid-context");
        let (model, _, reasoning) = resolve_model_metadata(&plan).unwrap();
        let current_tools = expected_tools(DescriptionVariant::Current, model.family.as_deref());
        let compact_tools = expected_tools(DescriptionVariant::CompactV1, model.family.as_deref());
        let current = initial_context("2026-07-24", "prompt", &current_tools);
        let compact = initial_context("2026-07-24", "prompt", &compact_tools);
        validate_context_pair(&current, &compact).unwrap();
        assert_eq!(current.system_prompt, compact.system_prompt);
        assert_eq!(
            serde_json::to_value(&current.messages).unwrap(),
            serde_json::to_value(&compact.messages).unwrap()
        );
        unpaid_request_preflight(&model, reasoning, "2026-07-24").unwrap();
    }

    #[test]
    fn normalized_pair_request_hashes_ignore_only_cache_key_and_description() {
        let plan = model_plan("normalized-pair");
        let (model, _, reasoning) = resolve_model_metadata(&plan).unwrap();
        let current = initial_context(
            "2026-07-24",
            "prompt",
            &expected_tools(DescriptionVariant::Current, model.family.as_deref()),
        );
        let compact = initial_context(
            "2026-07-24",
            "prompt",
            &expected_tools(DescriptionVariant::CompactV1, model.family.as_deref()),
        );
        let current = request_shape(&model, reasoning, &current, "same", 100);
        let compact = request_shape(&model, reasoning, &compact, "same", 100);
        assert_eq!(
            normalized_request_hash(&current, "same").unwrap(),
            normalized_request_hash(&compact, "same").unwrap()
        );
        let mut changed = compact;
        changed["context"]["messages"][0]["content"][0]["text"] = Value::String("other".into());
        assert_ne!(
            normalized_request_hash(&current, "same").unwrap(),
            normalized_request_hash(&changed, "same").unwrap()
        );
    }

    #[test]
    fn normalized_serialized_payload_detects_stable_input_and_option_changes() {
        let plan = model_plan("serialized-payload");
        let (model, _, _) = resolve_model_metadata(&plan).unwrap();
        let payload = |variant, cache: &str| {
            let context = initial_context(
                "2026-07-24",
                "prompt",
                &expected_tools(variant, model.family.as_deref()),
            );
            serde_json::json!({
                "model": model.id,
                "instructions": context.system_prompt,
                "input": context.messages,
                "tools": context.tools,
                "reasoning": {"effort": "low"},
                "prompt_cache_key": cache,
                "store": false
            })
        };
        let current = payload(DescriptionVariant::Current, "cache-a");
        let compact = payload(DescriptionVariant::CompactV1, "cache-b");
        assert_eq!(
            normalized_request_hash(&current, "cache-a").unwrap(),
            normalized_request_hash(&compact, "cache-b").unwrap()
        );

        let mut changed_input = compact.clone();
        changed_input["input"][0]["content"][0]["text"] = Value::String("changed".into());
        assert_ne!(
            normalized_request_hash(&current, "cache-a").unwrap(),
            normalized_request_hash(&changed_input, "cache-b").unwrap()
        );
        let mut changed_option = compact;
        changed_option["store"] = Value::Bool(true);
        assert_ne!(
            normalized_request_hash(&current, "cache-a").unwrap(),
            normalized_request_hash(&changed_option, "cache-b").unwrap()
        );
    }

    #[test]
    fn conversation_artifact_is_one_message_per_jsonl_line() {
        let messages = vec![
            Message::User(UserMessage::text("one")),
            Message::User(UserMessage::text("two")),
        ];
        let bytes = conversation_jsonl(&messages).unwrap();
        assert!(bytes.ends_with(b"\n"));
        let lines = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        for line in lines {
            serde_json::from_slice::<Message>(line).unwrap();
        }
        assert!(serde_json::from_slice::<Vec<Message>>(&bytes).is_err());
    }

    #[test]
    fn patch_classification_is_ordered_and_exhaustive() {
        let ok = ToolOutcomeWire {
            content: vec![],
            details: Value::Null,
            is_error: false,
        };
        let error = ToolOutcomeWire {
            is_error: true,
            ..ok.clone()
        };
        assert_eq!(
            classify_patch(false, &error, "", false),
            PatchClassification::SchemaError
        );
        assert_eq!(
            classify_patch(true, &error, "patch rejected: x", true),
            PatchClassification::PartialApplication
        );
        assert_eq!(
            classify_patch(true, &ok, "", false),
            PatchClassification::Success
        );
        assert_eq!(
            classify_patch(true, &error, "apply_patch verification failed: x", false),
            PatchClassification::FormatError
        );
        assert_eq!(
            classify_patch(true, &error, "patch rejected: x", false),
            PatchClassification::Rejected
        );
        assert_eq!(
            classify_patch(true, &error, "other", false),
            PatchClassification::ApplicationError
        );
    }
}
