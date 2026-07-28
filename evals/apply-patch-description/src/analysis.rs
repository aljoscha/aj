//! Complete confirmatory analysis of the frozen main schedule.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::artifacts::{TrialRecord, completed_pair, scan};
use crate::descriptions::DescriptionVariant;
use crate::planning::{FrozenPilotRuntimeContext, validate_pilot_evidence};
use crate::runtime::{PatchClassification, RuntimeLimits, SourceProvenance, TerminalStatus};
use crate::schedule::{FrozenPlan, PairScheduleRecord, SchedulePhase, validate_frozen_plan};
use crate::statistics::{
    BinaryPair, BinaryStratum, BootstrapConfig, BootstrapSummary, EfficiencyPair,
    EfficiencyStratum, RiskDifferenceBounds, paired_relative_change_bootstrap,
    paired_risk_difference_bounds,
};

/// Error raised when durable records cannot support confirmatory analysis.
#[derive(Debug)]
pub struct AnalysisError(pub String);

impl fmt::Display for AnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AnalysisError {}

/// Required metrics and optional diagnostics projected from a runtime record.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RuntimeMetrics {
    pub valid: bool,
    pub task_passed: bool,
    pub sessions_with_patch_failure: bool,
    pub edit_bypass: bool,
    pub aj_recorded_catalog_cost: f64,
    pub model_responses: u64,
    pub provider_requests: u32,
    pub image_id: String,
    pub source_provenance: SourceProvenance,
    pub utc_date: String,
    pub limits: RuntimeLimits,
    pub system_prompt_hash: String,
    pub terminal_status: TerminalStatus,
    pub usage: TokenUsageProjection,
    pub duration_millis: u64,
    pub tool_rounds: u64,
    pub total_tool_calls: u64,
    pub tool_calls_by_name: BTreeMap<String, u64>,
    pub recovery_rounds: u64,
    pub patch_calls: Vec<PatchDiagnosticProjection>,
    pub final_assistant_text: String,
    pub final_assistant_text_blob: Option<String>,
    pub normalized_first_request_hash: Option<String>,
    pub conservative_catalog_pair_reserve: f64,
    #[serde(default)]
    pub first_response_aj_recorded_catalog_cost: Option<f64>,
    #[serde(default)]
    pub apply_patch_attempts: Option<u64>,
    #[serde(default)]
    pub successful_patch_calls: Option<u64>,
    #[serde(default)]
    pub cache_stratum: Option<String>,
    #[serde(default)]
    pub cache_write_sensitivity: Option<CacheWriteSensitivityProjection>,
}

/// Provider-reported token categories used by descriptive distributions.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TokenUsageProjection {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
}

/// Patch classification projected without retaining full call artifacts.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PatchDiagnosticProjection {
    pub classification: PatchClassification,
}

/// Optional cache-write cost range projected without depending on runtime types.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CacheWriteSensitivityProjection {
    pub lower_aj_recorded_catalog_cost: f64,
    pub upper_aj_recorded_catalog_cost: f64,
}

/// Frozen identities that bind the report to the confirmatory design.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AnalysisIdentities {
    pub run_id: String,
    pub universe_hash: String,
    pub schedule_hash: String,
    pub planning_hash: String,
    pub planning_report_hash: String,
}

/// Main-sample completeness against the frozen schedule.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletenessSummary {
    pub expected_pairs: usize,
    pub complete_pairs: usize,
    pub complete: bool,
}

/// One binary endpoint's bound and guardrail decision.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BinaryEndpointSummary {
    pub endpoint: String,
    pub margin: f64,
    pub bound_direction: String,
    pub bounds: RiskDifferenceBounds,
    pub guardrail_passed: bool,
    pub material_harm: bool,
}

/// Distribution diagnostics for one variant.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct DistributionSummary {
    pub mean: f64,
    pub median: f64,
    pub p95: f64,
}

/// One efficiency endpoint's paired estimate and diagnostics.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EfficiencyEndpointSummary {
    pub endpoint: String,
    pub current: DistributionSummary,
    pub compact: DistributionSummary,
    pub absolute_change: f64,
    pub bootstrap: BootstrapSummary,
    pub establishes_benefit: bool,
    pub passes_non_degradation: bool,
}

/// Variant-specific estimated catalog cost per successful task.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct CostPerSuccessfulTask {
    pub current: Option<f64>,
    pub compact: Option<f64>,
}

/// Call-level patch failure diagnostic when runtime fields are present.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct CallPatchFailureDiagnostic {
    pub current_attempts: u64,
    pub current_failures: u64,
    pub current_rate: Option<f64>,
    pub compact_attempts: u64,
    pub compact_failures: u64,
    pub compact_rate: Option<f64>,
}

/// Cache-write sensitivity range by variant.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct CacheWriteSensitivitySummary {
    pub current_lower_mean: f64,
    pub current_upper_mean: f64,
    pub compact_lower_mean: f64,
    pub compact_upper_mean: f64,
    pub relative_change_lower: Option<f64>,
    pub relative_change_upper: Option<f64>,
}

/// Aggregate diagnostic counts and distributions for one variant.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VariantDiagnostics {
    pub distributions: BTreeMap<String, DistributionSummary>,
    pub tool_calls_by_name: BTreeMap<String, u64>,
    pub patch_classifications: BTreeMap<String, u64>,
    pub task_successes: u64,
    pub edit_bypass_sessions: u64,
    pub session_failures: BTreeMap<String, u64>,
    pub final_assistant_text_count: u64,
    pub final_assistant_text_nonempty: u64,
    pub final_assistant_text_bytes: u64,
    pub final_assistant_text_blob_refs: Vec<String>,
}

/// Descriptive diagnostics that never override the shipping rule.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DiagnosticSummary {
    pub cost_per_successful_task: CostPerSuccessfulTask,
    pub first_response_cost: Option<BTreeMap<String, DistributionSummary>>,
    pub call_patch_failure: Option<CallPatchFailureDiagnostic>,
    pub session_patch_failure: BTreeMap<String, f64>,
    pub cache_strata: Option<BTreeMap<String, BTreeMap<String, u64>>>,
    pub cache_write_sensitivity: Option<CacheWriteSensitivitySummary>,
    pub variants: BTreeMap<String, VariantDiagnostics>,
}

/// Final confirmatory disposition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShippingDecision {
    ShipCompactV1,
    RetainCurrent,
    Inconclusive,
}

/// Decision and deterministic supporting reasons.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecisionSummary {
    pub decision: ShippingDecision,
    pub reasons: Vec<String>,
}

/// Complete machine-readable confirmatory report.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AnalysisReport {
    pub identities: AnalysisIdentities,
    pub completeness: CompletenessSummary,
    pub decision: DecisionSummary,
    pub binary: Vec<BinaryEndpointSummary>,
    pub efficiency: Vec<EfficiencyEndpointSummary>,
    pub diagnostics: DiagnosticSummary,
    pub statistical_contract: String,
    pub cost_limitation: String,
    pub sample_plan: crate::statistics::SamplePlan,
}

/// Analyzes exactly the frozen main schedule with 100,000 bootstrap replicates.
pub fn analyze_records(plan: &FrozenPlan, records: &Path) -> Result<AnalysisReport, AnalysisError> {
    analyze_records_with_config(plan, records, BootstrapConfig::default())
}

/// Analyzes the frozen main schedule with explicit bootstrap controls.
pub fn analyze_records_with_config(
    plan: &FrozenPlan,
    records: &Path,
    bootstrap_config: BootstrapConfig,
) -> Result<AnalysisReport, AnalysisError> {
    validate_frozen_plan(plan).map_err(|error| AnalysisError(error.to_string()))?;
    let planning = plan
        .require_planned_main()
        .map_err(|error| AnalysisError(error.to_string()))?;
    planning
        .validate()
        .map_err(|error| AnalysisError(error.to_string()))?;
    validate_pilot_evidence(plan, records).map_err(|error| AnalysisError(error.to_string()))?;
    if plan.schedule.planning_hash.as_deref() != Some(&planning.planning_hash) {
        return Err(AnalysisError(
            "schedule is not bound to the frozen planning hash".into(),
        ));
    }

    let state = scan(records).map_err(|error| AnalysisError(error.to_string()))?;
    if state
        .trials_by_hash
        .values()
        .any(|trial| trial.identity.run_id != plan.schedule.run_id)
        || state
            .completion_markers
            .values()
            .any(|marker| marker.identity.run_id != plan.schedule.run_id)
    {
        return Err(AnalysisError(
            "records mix different frozen runs or model selections".into(),
        ));
    }
    let expected = plan
        .schedule
        .main
        .iter()
        .map(|pair| (pair.pair_id.as_str(), pair))
        .collect::<BTreeMap<_, _>>();
    for marker in state
        .completion_markers
        .values()
        .filter(|marker| marker.identity.phase == SchedulePhase::Main)
    {
        if marker.identity.run_id != plan.schedule.run_id
            || marker.identity.schedule_hash != plan.schedule.schedule_hash
            || !expected.contains_key(marker.identity.pair_id.as_str())
        {
            return Err(AnalysisError(
                "records contain a main pair from a different run, schedule, or planning record"
                    .into(),
            ));
        }
    }

    let pilot_context = &planning.pilot_evidence.runtime_context;
    let mut attempts_by_pair = BTreeMap::<&str, BTreeSet<&str>>::new();
    for trial in state.trials_by_hash.values().filter(|trial| {
        trial.identity.run_id == plan.schedule.run_id && trial.identity.phase == SchedulePhase::Main
    }) {
        let pair = expected
            .get(trial.identity.pair_id.as_str())
            .ok_or_else(|| AnalysisError("records contain an unplanned main trial".into()))?;
        validate_trial(&plan.schedule.schedule_hash, pair, trial)?;
        let runtime = parse_metrics(trial)?;
        validate_pilot_runtime_context(trial, &runtime, pilot_context)?;
        attempts_by_pair
            .entry(&trial.identity.pair_id)
            .or_default()
            .insert(&trial.identity.attempt_id);
    }
    if attempts_by_pair.values().any(|attempts| attempts.len() > 2) {
        return Err(AnalysisError(
            "main records exceed the frozen two-attempt limit".into(),
        ));
    }

    let mut pairs_by_archetype: BTreeMap<String, Vec<(RuntimeMetrics, RuntimeMetrics)>> =
        BTreeMap::new();
    let mut baseline: Option<&TrialRecord> = None;
    for pair in &plan.schedule.main {
        let completed = completed_pair(&state, &plan.schedule.schedule_hash, pair)
            .map_err(|error| AnalysisError(error.to_string()))?;
        let [first, second] = completed.trials;
        validate_frozen_context(plan, first)?;
        validate_frozen_context(plan, second)?;
        if let Some(reference) = baseline {
            if !same_analysis_context(first, reference)
                || !same_analysis_context(second, reference)
                || !same_analysis_context(first, second)
            {
                return Err(AnalysisError(
                    "main trials mix image, source, date, limits, model, or frozen controls".into(),
                ));
            }
        } else {
            baseline = Some(first);
        }
        let (current, compact) = variants(first, second)?;
        let current_metrics = metrics(current)?;
        let compact_metrics = metrics(compact)?;
        validate_pilot_runtime_context(current, &current_metrics, pilot_context)?;
        validate_pilot_runtime_context(compact, &compact_metrics, pilot_context)?;
        if !current_metrics.valid || !compact_metrics.valid {
            return Err(AnalysisError(
                "a completed main pair contains an invalid trial".into(),
            ));
        }
        if !payloads_equivalent(&current_metrics, &compact_metrics) {
            return Err(AnalysisError(
                "paired actual first-request payload hashes differ".into(),
            ));
        }
        pairs_by_archetype
            .entry(pair.archetype_id.clone())
            .or_default()
            .push((current_metrics, compact_metrics));
    }
    let expected_repetitions = planning.recommended_pairs_per_archetype;
    for archetype in &plan.manifest.archetypes {
        let observed = pairs_by_archetype.get(&archetype.id).map_or(0, Vec::len);
        if observed != usize::try_from(expected_repetitions).unwrap() {
            return Err(AnalysisError(format!(
                "archetype {} has {observed} complete pairs, expected {expected_repetitions}",
                archetype.id
            )));
        }
    }

    let binary = binary_summaries(plan, &pairs_by_archetype)?;
    let efficiency = efficiency_summaries(plan, &pairs_by_archetype, &bootstrap_config)?;
    let decision = decide(&binary, &efficiency);
    let diagnostics = diagnostics(&pairs_by_archetype);
    Ok(AnalysisReport {
        identities: AnalysisIdentities {
            run_id: plan.schedule.run_id.clone(),
            universe_hash: plan.universe.universe_hash.clone(),
            schedule_hash: plan.schedule.schedule_hash.clone(),
            planning_hash: planning.planning_hash.clone(),
            planning_report_hash: planning.planning_report_hash.clone(),
        },
        completeness: CompletenessSummary {
            expected_pairs: plan.schedule.main.len(),
            complete_pairs: plan.schedule.main.len(),
            complete: true,
        },
        decision,
        binary,
        efficiency,
        diagnostics,
        statistical_contract: format!(
            "One-sided 95% bounds invert the frozen fixed-archetype paired multinomial profile-score test for compact-v1 minus current. Efficiency uses nearest-rank quantiles from {} seeded pair-preserving bootstrap replicates.",
            bootstrap_config.replicates
        ),
        cost_limitation: "AJ-recorded catalog cost is not billed cost. Missing provider cache-write usage enters current AJ accounting as zero, so the cache-write sensitivity range is diagnostic and is reported beside the cost decision.".into(),
        sample_plan: planning.sample_plan.clone(),
    })
}

fn validate_trial(
    schedule_hash: &str,
    pair: &PairScheduleRecord,
    trial: &TrialRecord,
) -> Result<(), AnalysisError> {
    let scheduled = pair
        .trials
        .iter()
        .find(|scheduled| {
            scheduled.order_index == trial.identity.order_index
                && scheduled.variant == trial.identity.variant
        })
        .ok_or_else(|| AnalysisError("main trial occupies no frozen pair slot".into()))?;
    if trial.identity.run_id != scheduled.run_id
        || trial.identity.pair_id != scheduled.pair_id
        || trial.identity.task_id != scheduled.task_id
        || trial.identity.instance_hash != scheduled.instance_hash
        || trial.identity.archetype_id != pair.archetype_id
        || trial.identity.schedule_hash != schedule_hash
        || trial.identity.phase != scheduled.phase
        || trial.identity.repetition != scheduled.archetype_repetition
    {
        return Err(AnalysisError(
            "main trial does not match its exact frozen schedule identity".into(),
        ));
    }
    Ok(())
}

fn validate_pilot_runtime_context(
    trial: &TrialRecord,
    runtime: &RuntimeMetrics,
    frozen: &FrozenPilotRuntimeContext,
) -> Result<(), AnalysisError> {
    let unresolved = runtime.terminal_status == TerminalStatus::InfrastructureFailed
        && runtime.provider_requests == 0;
    let reserve_matches = (runtime.conservative_catalog_pair_reserve
        - frozen.conservative_catalog_pair_reserve)
        .abs()
        <= 1e-9;
    if runtime.image_id != frozen.image_id
        || runtime.source_provenance != frozen.source_provenance
        || runtime.utc_date != frozen.utc_date
        || runtime.limits != frozen.limits
        || (!unresolved && runtime.system_prompt_hash != frozen.system_prompt_hash)
        || (unresolved
            && !runtime.system_prompt_hash.is_empty()
            && runtime.system_prompt_hash != frozen.system_prompt_hash)
        || (!unresolved && !reserve_matches)
        || (unresolved && runtime.conservative_catalog_pair_reserve != 0.0 && !reserve_matches)
        || trial.metadata.aj_revision != frozen.aj_revision
        || trial.metadata.model_catalog_hash != frozen.model_catalog_hash
        || trial.metadata.provider != frozen.provider
        || trial.metadata.model != frozen.model
        || trial.metadata.reasoning_effort != frozen.reasoning_effort
        || trial.metadata.tool_catalog_hash != frozen.tool_catalog_hash
        || trial.metadata.suite_revision != frozen.suite_revision
        || trial.metadata.current_description != frozen.current_description
        || trial.metadata.compact_description != frozen.compact_description
    {
        return Err(AnalysisError(
            "main trial differs from the runtime context frozen by the pilot".into(),
        ));
    }
    Ok(())
}

fn validate_frozen_context(plan: &FrozenPlan, trial: &TrialRecord) -> Result<(), AnalysisError> {
    let current = plan
        .descriptions
        .iter()
        .find(|description| description.variant == DescriptionVariant::Current)
        .ok_or_else(|| AnalysisError("plan lost the current description".into()))?;
    let compact = plan
        .descriptions
        .iter()
        .find(|description| description.variant == DescriptionVariant::CompactV1)
        .ok_or_else(|| AnalysisError("plan lost the compact description".into()))?;
    if trial.metadata.suite_revision != plan.universe.suite_revision
        || trial.metadata.current_description.sha256 != current.sha256
        || trial.metadata.current_description.byte_length != current.byte_length
        || trial.metadata.compact_description.sha256 != compact.sha256
        || trial.metadata.compact_description.byte_length != compact.byte_length
    {
        return Err(AnalysisError(
            "main trial does not match frozen suite or description identities".into(),
        ));
    }
    Ok(())
}

fn same_analysis_context(left: &TrialRecord, right: &TrialRecord) -> bool {
    let runtime_matches = serde_json::from_value::<RuntimeMetrics>(left.runtime.clone())
        .ok()
        .zip(serde_json::from_value::<RuntimeMetrics>(right.runtime.clone()).ok())
        .is_some_and(|(left, right)| {
            left.image_id == right.image_id
                && left.source_provenance == right.source_provenance
                && left.utc_date == right.utc_date
                && left.limits == right.limits
                && left.system_prompt_hash == right.system_prompt_hash
                && left.conservative_catalog_pair_reserve == right.conservative_catalog_pair_reserve
        });
    runtime_matches
        && left.metadata.current_description == right.metadata.current_description
        && left.metadata.compact_description == right.metadata.compact_description
        && left.metadata.aj_revision == right.metadata.aj_revision
        && left.metadata.suite_revision == right.metadata.suite_revision
        && left.metadata.model_catalog_hash == right.metadata.model_catalog_hash
        && left.metadata.provider == right.metadata.provider
        && left.metadata.model == right.metadata.model
        && left.metadata.reasoning_effort == right.metadata.reasoning_effort
        && left.metadata.tool_catalog_hash == right.metadata.tool_catalog_hash
}

fn payloads_equivalent(current: &RuntimeMetrics, compact: &RuntimeMetrics) -> bool {
    let current_valid =
        current.provider_requests == 0 || current.normalized_first_request_hash.is_some();
    let compact_valid =
        compact.provider_requests == 0 || compact.normalized_first_request_hash.is_some();
    current_valid
        && compact_valid
        && (current.provider_requests == 0
            || compact.provider_requests == 0
            || current.normalized_first_request_hash == compact.normalized_first_request_hash)
}

fn variants<'a>(
    first: &'a TrialRecord,
    second: &'a TrialRecord,
) -> Result<(&'a TrialRecord, &'a TrialRecord), AnalysisError> {
    match (first.identity.variant, second.identity.variant) {
        (DescriptionVariant::Current, DescriptionVariant::CompactV1) => Ok((first, second)),
        (DescriptionVariant::CompactV1, DescriptionVariant::Current) => Ok((second, first)),
        _ => Err(AnalysisError(
            "complete pair does not contain both variants".into(),
        )),
    }
}

fn metrics(trial: &TrialRecord) -> Result<RuntimeMetrics, AnalysisError> {
    let metrics = parse_metrics(trial)?;
    if !metrics.aj_recorded_catalog_cost.is_finite() || metrics.aj_recorded_catalog_cost < 0.0 {
        return Err(AnalysisError(
            "main trial has invalid catalog-cost metrics".into(),
        ));
    }
    Ok(metrics)
}

fn parse_metrics(trial: &TrialRecord) -> Result<RuntimeMetrics, AnalysisError> {
    serde_json::from_value(trial.runtime.clone()).map_err(|error| {
        AnalysisError(format!(
            "trial {} is missing required runtime metrics: {error}",
            trial.record_hash
        ))
    })
}

fn binary_summaries(
    plan: &FrozenPlan,
    pairs: &BTreeMap<String, Vec<(RuntimeMetrics, RuntimeMetrics)>>,
) -> Result<Vec<BinaryEndpointSummary>, AnalysisError> {
    let fields: [(&str, f64, bool, fn(&RuntimeMetrics) -> bool); 3] = [
        ("task_success", -0.05, false, |runtime| runtime.task_passed),
        ("sessions_with_patch_failure", 0.03, true, |runtime| {
            runtime.sessions_with_patch_failure
        }),
        ("edit_bypass", 0.02, true, |runtime| runtime.edit_bypass),
    ];
    fields
        .into_iter()
        .map(|(endpoint, margin, upper, field)| {
            let strata = plan
                .manifest
                .archetypes
                .iter()
                .map(|archetype| BinaryStratum {
                    archetype_id: archetype.id.clone(),
                    weight: manifest_weight(archetype),
                    pairs: pairs[&archetype.id]
                        .iter()
                        .map(|(current, compact)| BinaryPair {
                            current: field(current),
                            compact: field(compact),
                        })
                        .collect(),
                })
                .collect::<Vec<_>>();
            let bounds = paired_risk_difference_bounds(&strata, 0.05)
                .map_err(|error| AnalysisError(error.to_string()))?;
            Ok(BinaryEndpointSummary {
                endpoint: endpoint.into(),
                margin,
                bound_direction: if upper { "upper" } else { "lower" }.into(),
                guardrail_passed: if upper {
                    bounds.upper < margin
                } else {
                    bounds.lower > margin
                },
                material_harm: if upper {
                    bounds.lower > margin
                } else {
                    bounds.upper < margin
                },
                bounds,
            })
        })
        .collect()
}

fn efficiency_summaries(
    plan: &FrozenPlan,
    pairs: &BTreeMap<String, Vec<(RuntimeMetrics, RuntimeMetrics)>>,
    bootstrap_config: &BootstrapConfig,
) -> Result<Vec<EfficiencyEndpointSummary>, AnalysisError> {
    let fields: [(&str, fn(&RuntimeMetrics) -> f64); 2] = [
        ("aj_recorded_catalog_cost", |runtime| {
            runtime.aj_recorded_catalog_cost
        }),
        ("model_responses", |runtime| {
            u64_as_f64(runtime.model_responses)
        }),
    ];
    fields
        .into_iter()
        .map(|(endpoint, field)| {
            let strata = efficiency_strata(plan, pairs, field);
            let bootstrap = paired_relative_change_bootstrap(&strata, bootstrap_config)
                .map_err(|error| AnalysisError(error.to_string()))?;
            let (current, compact) = variant_values(pairs, field);
            Ok(EfficiencyEndpointSummary {
                endpoint: endpoint.into(),
                current: distribution(&current),
                compact: distribution(&compact),
                absolute_change: distribution(&compact).mean - distribution(&current).mean,
                establishes_benefit: bootstrap.defined
                    && bootstrap.bounds_defined
                    && bootstrap.relative_change <= -0.05
                    && bootstrap.upper_97_5 < 0.0,
                passes_non_degradation: bootstrap.defined
                    && bootstrap.bounds_defined
                    && bootstrap.upper_95 < 0.02,
                bootstrap,
            })
        })
        .collect()
}

fn efficiency_strata(
    plan: &FrozenPlan,
    pairs: &BTreeMap<String, Vec<(RuntimeMetrics, RuntimeMetrics)>>,
    field: fn(&RuntimeMetrics) -> f64,
) -> Vec<EfficiencyStratum> {
    plan.manifest
        .archetypes
        .iter()
        .map(|archetype| EfficiencyStratum {
            archetype_id: archetype.id.clone(),
            weight: manifest_weight(archetype),
            pairs: pairs[&archetype.id]
                .iter()
                .map(|(current, compact)| EfficiencyPair {
                    current: field(current),
                    compact: field(compact),
                })
                .collect(),
        })
        .collect()
}

/// Applies the frozen shipping rule to complete endpoint summaries.
pub fn decide(
    binary: &[BinaryEndpointSummary],
    efficiency: &[EfficiencyEndpointSummary],
) -> DecisionSummary {
    let all_guardrails = binary.iter().all(|endpoint| endpoint.guardrail_passed);
    let cost = efficiency
        .iter()
        .find(|endpoint| endpoint.endpoint == "aj_recorded_catalog_cost");
    let responses = efficiency
        .iter()
        .find(|endpoint| endpoint.endpoint == "model_responses");
    let cost_case = cost.is_some_and(|endpoint| endpoint.establishes_benefit)
        && responses.is_some_and(|endpoint| endpoint.passes_non_degradation);
    let response_case = responses.is_some_and(|endpoint| endpoint.establishes_benefit)
        && cost.is_some_and(|endpoint| endpoint.passes_non_degradation);
    if all_guardrails && (cost_case || response_case) {
        let benefit = if cost_case {
            "AJ-recorded catalog cost"
        } else {
            "model responses"
        };
        return DecisionSummary {
            decision: ShippingDecision::ShipCompactV1,
            reasons: vec![
                "all three binary guardrails pass".into(),
                format!("{benefit} establishes worthwhile efficiency benefit"),
                "the other efficiency endpoint passes non-degradation".into(),
            ],
        };
    }
    let material = binary
        .iter()
        .filter(|endpoint| endpoint.material_harm)
        .map(|endpoint| endpoint.endpoint.clone())
        .collect::<Vec<_>>();
    let efficiency_harm = efficiency
        .iter()
        .filter(|endpoint| endpoint.bootstrap.bounds_defined && endpoint.bootstrap.lower_95 > 0.02)
        .map(|endpoint| endpoint.endpoint.clone())
        .collect::<Vec<_>>();
    if !material.is_empty() || !efficiency_harm.is_empty() {
        let mut reasons = material
            .into_iter()
            .map(|endpoint| format!("{endpoint} shows material guardrail harm"))
            .collect::<Vec<_>>();
        reasons.extend(
            efficiency_harm
                .into_iter()
                .map(|endpoint| format!("{endpoint} shows efficiency degradation above 2%")),
        );
        return DecisionSummary {
            decision: ShippingDecision::RetainCurrent,
            reasons,
        };
    }
    let mut reasons = Vec::new();
    for endpoint in binary.iter().filter(|endpoint| !endpoint.guardrail_passed) {
        reasons.push(format!(
            "{} guardrail is not established",
            endpoint.endpoint
        ));
    }
    if !cost_case && !response_case {
        reasons.push("neither efficiency case is established".into());
    }
    for endpoint in efficiency
        .iter()
        .filter(|endpoint| !endpoint.bootstrap.defined)
    {
        reasons.push(format!(
            "{} relative change is undefined because the current weighted mean is zero",
            endpoint.endpoint
        ));
    }
    for endpoint in efficiency
        .iter()
        .filter(|endpoint| endpoint.bootstrap.defined && !endpoint.bootstrap.bounds_defined)
    {
        reasons.push(format!(
            "{} bootstrap bounds are unbounded because a resample has no current observations",
            endpoint.endpoint
        ));
    }
    DecisionSummary {
        decision: ShippingDecision::Inconclusive,
        reasons,
    }
}

fn diagnostics(
    pairs: &BTreeMap<String, Vec<(RuntimeMetrics, RuntimeMetrics)>>,
) -> DiagnosticSummary {
    let all = pairs.values().flatten().collect::<Vec<_>>();
    let current_cost = all
        .iter()
        .map(|(current, _)| current.aj_recorded_catalog_cost)
        .collect::<Vec<_>>();
    let compact_cost = all
        .iter()
        .map(|(_, compact)| compact.aj_recorded_catalog_cost)
        .collect::<Vec<_>>();
    let current_successes = all
        .iter()
        .filter(|(current, _)| current.task_passed)
        .count();
    let compact_successes = all
        .iter()
        .filter(|(_, compact)| compact.task_passed)
        .count();
    let first_response = optional_distributions(
        all.iter()
            .map(|(current, compact)| {
                (
                    current.first_response_aj_recorded_catalog_cost,
                    compact.first_response_aj_recorded_catalog_cost,
                )
            })
            .collect(),
    );
    let call_patch_failure = call_patch_diagnostic(&all);
    let current_session_failure = mean_bool(
        all.iter()
            .map(|(current, _)| current.sessions_with_patch_failure),
    );
    let compact_session_failure = mean_bool(
        all.iter()
            .map(|(_, compact)| compact.sessions_with_patch_failure),
    );
    DiagnosticSummary {
        cost_per_successful_task: CostPerSuccessfulTask {
            current: ratio_sum(&current_cost, current_successes),
            compact: ratio_sum(&compact_cost, compact_successes),
        },
        first_response_cost: first_response,
        call_patch_failure,
        session_patch_failure: BTreeMap::from([
            ("current".into(), current_session_failure),
            ("compact_v1".into(), compact_session_failure),
        ]),
        cache_strata: cache_strata(&all),
        cache_write_sensitivity: sensitivity(&all),
        variants: BTreeMap::from([
            ("current".into(), variant_diagnostics(&all, false)),
            ("compact_v1".into(), variant_diagnostics(&all, true)),
        ]),
    }
}

fn variant_diagnostics(
    all: &[&(RuntimeMetrics, RuntimeMetrics)],
    compact: bool,
) -> VariantDiagnostics {
    let values = all
        .iter()
        .map(|pair| if compact { &pair.1 } else { &pair.0 })
        .collect::<Vec<_>>();
    let metric = |field: fn(&RuntimeMetrics) -> f64| {
        distribution(
            &values
                .iter()
                .map(|runtime| field(runtime))
                .collect::<Vec<_>>(),
        )
    };
    let mut distributions = BTreeMap::from([
        ("input_tokens".into(), metric(|r| u64_as_f64(r.usage.input))),
        (
            "output_tokens".into(),
            metric(|r| u64_as_f64(r.usage.output)),
        ),
        (
            "cache_read_tokens".into(),
            metric(|r| u64_as_f64(r.usage.cache_read)),
        ),
        (
            "cache_write_tokens".into(),
            metric(|r| u64_as_f64(r.usage.cache_write)),
        ),
        (
            "total_tokens".into(),
            metric(|r| u64_as_f64(r.usage.total_tokens)),
        ),
        (
            "duration_millis".into(),
            metric(|r| u64_as_f64(r.duration_millis)),
        ),
        ("tool_rounds".into(), metric(|r| u64_as_f64(r.tool_rounds))),
        (
            "total_tool_calls".into(),
            metric(|r| u64_as_f64(r.total_tool_calls)),
        ),
        (
            "recovery_rounds".into(),
            metric(|r| u64_as_f64(r.recovery_rounds)),
        ),
    ]);
    let tool_names = values
        .iter()
        .flat_map(|runtime| runtime.tool_calls_by_name.keys().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    for name in tool_names {
        let per_session = values
            .iter()
            .map(|runtime| u64_as_f64(runtime.tool_calls_by_name.get(&name).copied().unwrap_or(0)))
            .collect::<Vec<_>>();
        distributions.insert(format!("tool_calls:{name}"), distribution(&per_session));
    }
    let mut tool_calls_by_name = BTreeMap::new();
    let mut patch_classifications = BTreeMap::new();
    let mut session_failures = BTreeMap::new();
    let mut blob_refs = std::collections::BTreeSet::new();
    for runtime in &values {
        for (name, count) in &runtime.tool_calls_by_name {
            *tool_calls_by_name.entry(name.clone()).or_default() += count;
        }
        for call in &runtime.patch_calls {
            *patch_classifications
                .entry(patch_classification_name(call.classification).into())
                .or_default() += 1;
        }
        if runtime.terminal_status != TerminalStatus::Passed {
            *session_failures
                .entry(terminal_status_name(runtime.terminal_status).into())
                .or_default() += 1;
        }
        if let Some(reference) = &runtime.final_assistant_text_blob {
            blob_refs.insert(reference.clone());
        }
    }
    VariantDiagnostics {
        distributions,
        tool_calls_by_name,
        patch_classifications,
        task_successes: u64::try_from(values.iter().filter(|r| r.task_passed).count()).unwrap(),
        edit_bypass_sessions: u64::try_from(values.iter().filter(|r| r.edit_bypass).count())
            .unwrap(),
        session_failures,
        final_assistant_text_count: u64::try_from(values.len()).unwrap(),
        final_assistant_text_nonempty: u64::try_from(
            values
                .iter()
                .filter(|r| !r.final_assistant_text.is_empty())
                .count(),
        )
        .unwrap(),
        final_assistant_text_bytes: values.iter().fold(0_u64, |total, runtime| {
            total.saturating_add(
                u64::try_from(runtime.final_assistant_text.len()).unwrap_or(u64::MAX),
            )
        }),
        final_assistant_text_blob_refs: blob_refs.into_iter().collect(),
    }
}

fn patch_classification_name(classification: PatchClassification) -> &'static str {
    match classification {
        PatchClassification::SchemaError => "schema_error",
        PatchClassification::PartialApplication => "partial_application",
        PatchClassification::Success => "success",
        PatchClassification::FormatError => "format_error",
        PatchClassification::Rejected => "rejected",
        PatchClassification::ApplicationError => "application_error",
    }
}

fn terminal_status_name(status: TerminalStatus) -> &'static str {
    match status {
        TerminalStatus::RunnerInternal => "runner_internal",
        TerminalStatus::InfrastructureFailed => "infrastructure_failed",
        TerminalStatus::Cancelled => "cancelled",
        TerminalStatus::TimedOut => "timed_out",
        TerminalStatus::TurnLimit => "turn_limit",
        TerminalStatus::ModelFailed => "model_failed",
        TerminalStatus::VerifierFailed => "verifier_failed",
        TerminalStatus::Passed => "passed",
    }
}

fn call_patch_diagnostic(
    all: &[&(RuntimeMetrics, RuntimeMetrics)],
) -> Option<CallPatchFailureDiagnostic> {
    let aggregate = |compact: bool| {
        all.iter()
            .try_fold((0_u64, 0_u64), |(attempts, failures), pair| {
                let runtime = if compact { &pair.1 } else { &pair.0 };
                let trial_attempts = runtime.apply_patch_attempts?;
                let successes = runtime.successful_patch_calls?;
                (successes <= trial_attempts).then_some((
                    attempts + trial_attempts,
                    failures + trial_attempts - successes,
                ))
            })
    };
    let (current_attempts, current_failures) = aggregate(false)?;
    let (compact_attempts, compact_failures) = aggregate(true)?;
    Some(CallPatchFailureDiagnostic {
        current_attempts,
        current_failures,
        current_rate: rate(current_failures, current_attempts),
        compact_attempts,
        compact_failures,
        compact_rate: rate(compact_failures, compact_attempts),
    })
}

fn cache_strata(
    all: &[&(RuntimeMetrics, RuntimeMetrics)],
) -> Option<BTreeMap<String, BTreeMap<String, u64>>> {
    let mut result = BTreeMap::from([
        ("current".into(), BTreeMap::new()),
        ("compact_v1".into(), BTreeMap::new()),
    ]);
    for pair in all {
        for (variant, runtime) in [("current", &pair.0), ("compact_v1", &pair.1)] {
            let stratum = runtime.cache_stratum.as_ref()?;
            *result
                .get_mut(variant)
                .unwrap()
                .entry(stratum.clone())
                .or_default() += 1;
        }
    }
    Some(result)
}

fn sensitivity(all: &[&(RuntimeMetrics, RuntimeMetrics)]) -> Option<CacheWriteSensitivitySummary> {
    let values = |compact: bool| {
        all.iter()
            .map(|pair| {
                let runtime = if compact { &pair.1 } else { &pair.0 };
                runtime.cache_write_sensitivity.as_ref().map(|range| {
                    (
                        range.lower_aj_recorded_catalog_cost,
                        range.upper_aj_recorded_catalog_cost,
                    )
                })
            })
            .collect::<Option<Vec<_>>>()
    };
    let current = values(false)?;
    let compact = values(true)?;
    Some(CacheWriteSensitivitySummary {
        current_lower_mean: mean(current.iter().map(|range| range.0)),
        current_upper_mean: mean(current.iter().map(|range| range.1)),
        compact_lower_mean: mean(compact.iter().map(|range| range.0)),
        compact_upper_mean: mean(compact.iter().map(|range| range.1)),
        relative_change_lower: relative_sensitivity(
            mean(compact.iter().map(|range| range.0)),
            mean(current.iter().map(|range| range.1)),
        ),
        relative_change_upper: relative_sensitivity(
            mean(compact.iter().map(|range| range.1)),
            mean(current.iter().map(|range| range.0)),
        ),
    })
}

fn relative_sensitivity(compact: f64, current: f64) -> Option<f64> {
    (current > 0.0).then_some(compact / current - 1.0)
}

fn optional_distributions(
    values: Vec<(Option<f64>, Option<f64>)>,
) -> Option<BTreeMap<String, DistributionSummary>> {
    let current = values
        .iter()
        .map(|pair| pair.0)
        .collect::<Option<Vec<_>>>()?;
    let compact = values
        .iter()
        .map(|pair| pair.1)
        .collect::<Option<Vec<_>>>()?;
    Some(BTreeMap::from([
        ("current".into(), distribution(&current)),
        ("compact_v1".into(), distribution(&compact)),
    ]))
}

fn variant_values(
    pairs: &BTreeMap<String, Vec<(RuntimeMetrics, RuntimeMetrics)>>,
    field: fn(&RuntimeMetrics) -> f64,
) -> (Vec<f64>, Vec<f64>) {
    let all = pairs.values().flatten();
    let current = all.clone().map(|(current, _)| field(current)).collect();
    let compact = all.map(|(_, compact)| field(compact)).collect();
    (current, compact)
}

fn distribution(values: &[f64]) -> DistributionSummary {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    let median = if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    };
    DistributionSummary {
        mean: sorted.iter().sum::<f64>() / usize_as_f64(sorted.len()),
        median,
        p95: nearest_rank(&sorted, 0.95),
    }
}

fn manifest_weight(archetype: &crate::suite::ArchetypeManifest) -> f64 {
    f64::from(archetype.weight.numerator) / f64::from(archetype.weight.denominator)
}

fn ratio_sum(values: &[f64], denominator: usize) -> Option<f64> {
    (denominator > 0).then(|| values.iter().sum::<f64>() / usize_as_f64(denominator))
}

fn rate(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| u64_as_f64(numerator) / u64_as_f64(denominator))
}

fn mean_bool(values: impl Iterator<Item = bool>) -> f64 {
    let values = values.collect::<Vec<_>>();
    usize_as_f64(values.iter().filter(|value| **value).count()) / usize_as_f64(values.len())
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let values = values.collect::<Vec<_>>();
    values.iter().sum::<f64>() / usize_as_f64(values.len())
}

fn nearest_rank(sorted: &[f64], quantile: f64) -> f64 {
    let rank = (quantile * usize_as_f64(sorted.len())).ceil();
    let index = positive_f64_as_usize(rank)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index]
}

/// Renders the complete deterministic Markdown report.
pub fn render_markdown(report: &AnalysisReport) -> String {
    let mut output = format!(
        "# Apply patch description evaluation\n\nDecision: `{}`\n\nComplete main pairs: {}/{}\n\n",
        decision_name(report.decision.decision),
        report.completeness.complete_pairs,
        report.completeness.expected_pairs
    );
    output.push_str("## Frozen identities\n\n");
    output.push_str(&format!(
        "- Run: `{}`\n- Universe: `{}`\n- Schedule: `{}`\n- Planning: `{}`\n- Planning report: `{}`\n\n",
        report.identities.run_id,
        report.identities.universe_hash,
        report.identities.schedule_hash,
        report.identities.planning_hash,
        report.identities.planning_report_hash
    ));
    output.push_str("## Decision reasons\n\n");
    for reason in &report.decision.reasons {
        output.push_str(&format!("- {reason}\n"));
    }
    output.push_str("\n## Binary guardrails\n\n| Endpoint | Effect | Lower | Upper | Margin | Pass |\n| --- | ---: | ---: | ---: | ---: | --- |\n");
    for endpoint in &report.binary {
        output.push_str(&format!(
            "| {} | {:.4} | {:.4} | {:.4} | {:.4} | {} |\n",
            endpoint.endpoint,
            endpoint.bounds.estimate,
            endpoint.bounds.lower,
            endpoint.bounds.upper,
            endpoint.margin,
            endpoint.guardrail_passed
        ));
    }
    output.push_str("\n## Efficiency and tails\n\n| Endpoint | Current mean / median / p95 | Compact mean / median / p95 | Absolute | Relative | Upper 95% | Upper 97.5% |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for endpoint in &report.efficiency {
        if !endpoint.bootstrap.defined {
            output.push_str(&format!(
                "| {} | {:.6} / {:.6} / {:.6} | {:.6} / {:.6} / {:.6} | {:.6} | undefined | undefined | undefined |\n",
                endpoint.endpoint,
                endpoint.current.mean,
                endpoint.current.median,
                endpoint.current.p95,
                endpoint.compact.mean,
                endpoint.compact.median,
                endpoint.compact.p95,
                endpoint.absolute_change,
            ));
            continue;
        }
        if !endpoint.bootstrap.bounds_defined {
            output.push_str(&format!(
                "| {} | {:.6} / {:.6} / {:.6} | {:.6} / {:.6} / {:.6} | {:.6} | {:.4} | unbounded | unbounded |\n",
                endpoint.endpoint,
                endpoint.current.mean,
                endpoint.current.median,
                endpoint.current.p95,
                endpoint.compact.mean,
                endpoint.compact.median,
                endpoint.compact.p95,
                endpoint.absolute_change,
                endpoint.bootstrap.relative_change,
            ));
            continue;
        }
        output.push_str(&format!(
            "| {} | {:.6} / {:.6} / {:.6} | {:.6} / {:.6} / {:.6} | {:.6} | {:.4} | {:.4} | {:.4} |\n",
            endpoint.endpoint,
            endpoint.current.mean,
            endpoint.current.median,
            endpoint.current.p95,
            endpoint.compact.mean,
            endpoint.compact.median,
            endpoint.compact.p95,
            endpoint.absolute_change,
            endpoint.bootstrap.relative_change,
            endpoint.bootstrap.upper_95,
            endpoint.bootstrap.upper_97_5
        ));
    }
    output.push_str(&format!(
        "\n## Diagnostics\n\nCall-level patch failure: `{}`\n\nCache strata: `{}`\n\nCache-write sensitivity: `{}`\n\nCost per successful task: `{}`\n\nVariant aggregates and artifact references: `{}`\n\n## Cost limitation\n\n{}\n\n## Statistical contract\n\n{}\n\n## Sample plan\n\nPairs per archetype: `{}`. Limiting endpoint: `{}`.\n",
        json_or_unavailable(&report.diagnostics.call_patch_failure),
        json_or_unavailable(&report.diagnostics.cache_strata),
        json_or_unavailable(&report.diagnostics.cache_write_sensitivity),
        serde_json::to_string(&report.diagnostics.cost_per_successful_task).unwrap(),
        serde_json::to_string(&report.diagnostics.variants).unwrap(),
        report.cost_limitation,
        report.statistical_contract,
        report.sample_plan.pairs_per_archetype.map_or_else(|| "unavailable".into(), |value| value.to_string()),
        report.sample_plan.limiting_endpoint.as_deref().unwrap_or("unavailable")
    ));
    output
}

fn decision_name(decision: ShippingDecision) -> &'static str {
    match decision {
        ShippingDecision::ShipCompactV1 => "ship_compact_v1",
        ShippingDecision::RetainCurrent => "retain_current",
        ShippingDecision::Inconclusive => "inconclusive",
    }
}

fn json_or_unavailable(value: &impl Serialize) -> String {
    match serde_json::to_value(value).unwrap() {
        serde_json::Value::Null => "unavailable".into(),
        value => serde_json::to_string(&value).unwrap(),
    }
}

#[allow(clippy::as_conversions)]
fn usize_as_f64(value: usize) -> f64 {
    value as f64
}

#[allow(clippy::as_conversions)]
fn u64_as_f64(value: u64) -> f64 {
    value as f64
}

#[allow(clippy::as_conversions)]
fn positive_f64_as_usize(value: f64) -> usize {
    debug_assert!(value.is_finite() && value >= 0.0 && value <= usize::MAX as f64);
    value as usize
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::artifacts::{RecordedDescription, TrialIdentity, TrialMetadata, TrialRecord};

    fn bounds(estimate: f64, lower: f64, upper: f64) -> RiskDifferenceBounds {
        RiskDifferenceBounds {
            estimate,
            lower,
            upper,
            alpha: 0.05,
        }
    }

    fn binary(pass: bool, harm: bool) -> Vec<BinaryEndpointSummary> {
        [
            ("task_success", -0.05, "lower"),
            ("sessions_with_patch_failure", 0.03, "upper"),
            ("edit_bypass", 0.02, "upper"),
        ]
        .into_iter()
        .map(|(endpoint, margin, direction)| BinaryEndpointSummary {
            endpoint: endpoint.into(),
            margin,
            bound_direction: direction.into(),
            bounds: bounds(0.0, -0.01, 0.01),
            guardrail_passed: pass,
            material_harm: harm,
        })
        .collect()
    }

    fn efficiency(
        benefit: bool,
        non_degradation: bool,
        harm: bool,
    ) -> Vec<EfficiencyEndpointSummary> {
        ["aj_recorded_catalog_cost", "model_responses"]
            .into_iter()
            .enumerate()
            .map(|(index, endpoint)| EfficiencyEndpointSummary {
                endpoint: endpoint.into(),
                current: DistributionSummary {
                    mean: 10.0,
                    median: 10.0,
                    p95: 10.0,
                },
                compact: DistributionSummary {
                    mean: 9.0,
                    median: 9.0,
                    p95: 9.0,
                },
                absolute_change: -1.0,
                bootstrap: BootstrapSummary {
                    defined: true,
                    bounds_defined: true,
                    relative_change: if benefit && index == 0 { -0.1 } else { 0.0 },
                    lower_95: if harm { 0.03 } else { -0.02 },
                    lower_97_5: -0.03,
                    upper_95: if non_degradation { 0.01 } else { 0.03 },
                    upper_97_5: if benefit && index == 0 { -0.01 } else { 0.02 },
                    replicates: 100,
                },
                establishes_benefit: benefit && index == 0,
                passes_non_degradation: non_degradation,
            })
            .collect()
    }

    #[test]
    fn every_decision_branch_is_explicit() {
        assert_eq!(
            decide(&binary(true, false), &efficiency(true, true, false)).decision,
            ShippingDecision::ShipCompactV1
        );
        assert_eq!(
            decide(&binary(false, true), &efficiency(false, false, false)).decision,
            ShippingDecision::RetainCurrent
        );
        assert_eq!(
            decide(&binary(true, false), &efficiency(false, false, true)).decision,
            ShippingDecision::RetainCurrent
        );
        assert_eq!(
            decide(&binary(false, false), &efficiency(false, false, false)).decision,
            ShippingDecision::Inconclusive
        );
    }

    #[test]
    fn response_endpoint_can_establish_benefit() {
        let mut endpoints = efficiency(false, true, false);
        endpoints[1].establishes_benefit = true;
        assert_eq!(
            decide(&binary(true, false), &endpoints).decision,
            ShippingDecision::ShipCompactV1
        );
    }

    #[test]
    fn main_runtime_must_match_the_exact_pilot_context() {
        let current = RecordedDescription {
            sha256: "current".into(),
            byte_length: 100,
        };
        let compact = RecordedDescription {
            sha256: "compact".into(),
            byte_length: 50,
        };
        let source = SourceProvenance {
            head: "head".into(),
            dirty: false,
            worktree_hash: None,
        };
        let limits = RuntimeLimits {
            wall_timeout_seconds: 600,
            max_provider_requests: 12,
            max_model_responses: 12,
            provider_output_token_ceiling: 128_000,
            aggregate_observed_output_token_ceiling: 1_536_000,
        };
        let frozen = FrozenPilotRuntimeContext {
            image_id: "sha256:image-a".into(),
            source_provenance: source.clone(),
            utc_date: "2026-07-24".into(),
            limits: limits.clone(),
            system_prompt_hash: "system".into(),
            aj_revision: "head".into(),
            model_catalog_hash: "catalog".into(),
            provider: "openai-codex".into(),
            model: "gpt-5.6-sol".into(),
            reasoning_effort: "low".into(),
            tool_catalog_hash: "tools".into(),
            suite_revision: "suite".into(),
            current_description: current.clone(),
            compact_description: compact.clone(),
            conservative_catalog_pair_reserve: 100.0,
        };
        let trial = TrialRecord::new(
            TrialIdentity {
                run_id: "run".into(),
                pair_id: "pair".into(),
                attempt_id: "attempt".into(),
                task_id: "task".into(),
                instance_hash: "instance".into(),
                archetype_id: "insertion".into(),
                schedule_hash: "schedule".into(),
                phase: SchedulePhase::Main,
                repetition: 0,
                variant: DescriptionVariant::Current,
                order_index: 0,
            },
            TrialMetadata {
                task_seed: "seed".into(),
                current_description: current,
                compact_description: compact,
                aj_revision: "head".into(),
                suite_revision: "suite".into(),
                model_catalog_hash: "catalog".into(),
                provider: "openai-codex".into(),
                model: "gpt-5.6-sol".into(),
                reasoning_effort: "low".into(),
                tool_catalog_hash: "tools".into(),
                fixture_revision: "fixture".into(),
            },
            json!({}),
        )
        .unwrap();
        let runtime_json = json!({
            "valid": true,
            "task_passed": true,
            "sessions_with_patch_failure": false,
            "edit_bypass": false,
            "aj_recorded_catalog_cost": 1.0,
            "model_responses": 1,
            "provider_requests": 1,
            "image_id": "sha256:image-a",
            "source_provenance": source,
            "utc_date": "2026-07-24",
            "limits": limits,
            "system_prompt_hash": "system",
            "terminal_status": "passed",
            "usage": {"input":1,"output":1,"cache_read":0,"cache_write":0,"total_tokens":2},
            "duration_millis": 1,
            "tool_rounds": 1,
            "total_tool_calls": 1,
            "tool_calls_by_name": {"apply_patch":1},
            "recovery_rounds": 0,
            "patch_calls": [],
            "final_assistant_text": "done",
            "final_assistant_text_blob": null,
            "normalized_first_request_hash": "payload",
            "conservative_catalog_pair_reserve": 100.0
        });
        let runtime: RuntimeMetrics = serde_json::from_value(runtime_json).unwrap();
        validate_pilot_runtime_context(&trial, &runtime, &frozen).unwrap();

        let mut changed = runtime;
        changed.image_id = "sha256:image-b".into();
        assert!(validate_pilot_runtime_context(&trial, &changed, &frozen).is_err());
    }
}
