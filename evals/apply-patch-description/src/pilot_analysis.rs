//! Descriptive analysis of the excluded pilot after main planning is frozen.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::analysis::{RuntimeMetrics, validate_pilot_runtime_context};
use crate::artifacts::{TrialRecord, completed_pair, scan};
use crate::descriptions::DescriptionVariant;
use crate::hash_framed;
use crate::planning::{FrozenPilotRuntimeContext, PlanningReport, plan_main};
use crate::runtime::MAX_PAIR_ATTEMPTS;
use crate::schedule::{FrozenPlan, PairScheduleRecord, SchedulePhase, validate_frozen_plan};

const PILOT_PAIRS: usize = 48;
const ARCHETYPES: usize = 16;
const PAIRS_PER_ARCHETYPE: usize = 3;

/// Error raised when durable records cannot support a descriptive pilot report.
#[derive(Debug)]
pub struct PilotAnalysisError(pub String);

impl fmt::Display for PilotAnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PilotAnalysisError {}

/// Hash and length of one frozen treatment description.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PilotDescriptionIdentity {
    pub variant: DescriptionVariant,
    pub sha256: String,
    pub byte_length: u64,
}

/// Frozen model controls for the pilot estimand.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PilotModelIdentity {
    pub provider: String,
    pub model: String,
    pub reasoning: String,
    pub model_selection_hash: String,
    pub model_catalog_hash: String,
    pub model_capability_hash: String,
    pub tool_catalog_hash: String,
}

/// Frozen hashes that bind the report to planning and pilot evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PilotReportIdentities {
    pub run_id: String,
    pub universe_hash: String,
    pub unplanned_schedule_hash: String,
    pub planning_report_hash: String,
    pub blinded_pilot_hash: String,
    pub pilot_completion_stream_hash: String,
}

/// Expected and observed excluded-phase counts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PilotCompleteness {
    pub expected_smoke_pairs: usize,
    pub observed_smoke_pairs: usize,
    pub expected_pilot_pairs: usize,
    pub observed_pilot_pairs: usize,
    pub expected_archetypes: usize,
    pub observed_archetypes: usize,
    pub expected_pairs_per_archetype: usize,
    pub observed_pairs_per_archetype: BTreeMap<String, usize>,
    pub report_sample_pairs: usize,
    pub report_sample_sessions: usize,
    pub smoke_pairs_in_report_sample: usize,
}

/// Treatment order counts after restoring the treatment labels.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PilotOrderCounts {
    pub ab_current_then_compact_v1: usize,
    pub ba_compact_v1_then_current: usize,
}

/// Immutable evidence identity for one marker-referenced pilot pair.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PilotPairEvidenceIdentity {
    pub pair_id: String,
    pub pair_identity_hash: String,
    pub archetype_id: String,
    pub archetype_repetition: u32,
    pub task_id: String,
    pub instance_hash: String,
    pub attempt_id: String,
    pub treatment_order: String,
    pub completion_marker_hash: String,
    pub current_trial_record_hash: String,
    pub compact_v1_trial_record_hash: String,
}

/// Event count and observed rate for one treatment variant.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct BinaryVariantDescription {
    pub count: u64,
    pub observations: u64,
    pub rate: f64,
}

/// Paired event table with treatment labels restored.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PairedDiscordanceDescription {
    pub neither: u64,
    pub current_only: u64,
    pub compact_v1_only: u64,
    pub both: u64,
}

/// Descriptive summary for one binary endpoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PilotBinaryEndpoint {
    pub endpoint: String,
    pub current: BinaryVariantDescription,
    pub compact_v1: BinaryVariantDescription,
    pub paired_discordance: PairedDiscordanceDescription,
    pub observed_difference_compact_v1_minus_current: f64,
}

/// Deterministic distribution of observed values.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PilotDistribution {
    pub count: usize,
    pub minimum: f64,
    pub mean: f64,
    pub median: f64,
    pub p95: f64,
    pub maximum: f64,
}

/// Descriptive summary for one continuous endpoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PilotContinuousEndpoint {
    pub endpoint: String,
    pub current: PilotDistribution,
    pub compact_v1: PilotDistribution,
    pub absolute_mean_difference_compact_v1_minus_current: f64,
    pub relative_mean_difference_compact_v1_minus_current: Option<f64>,
    pub within_pair_difference_compact_v1_minus_current: PilotDistribution,
}

/// Overall or archetype-specific descriptive endpoint summaries.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PilotDescriptiveSummary {
    pub pair_count: usize,
    pub binary_outcomes: Vec<PilotBinaryEndpoint>,
    pub continuous_endpoints: Vec<PilotContinuousEndpoint>,
}

/// Structurally separate non-shipping report for the frozen pilot.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExploratoryPilotReport {
    pub schema_version: u32,
    pub analysis_kind: String,
    pub shipping_eligible: bool,
    pub identities: PilotReportIdentities,
    pub provenance: FrozenPilotRuntimeContext,
    pub model: PilotModelIdentity,
    pub descriptions: [PilotDescriptionIdentity; 2],
    pub completeness: PilotCompleteness,
    pub treatment_order_counts: PilotOrderCounts,
    pub pair_evidence: Vec<PilotPairEvidenceIdentity>,
    pub overall: PilotDescriptiveSummary,
    pub by_archetype: BTreeMap<String, PilotDescriptiveSummary>,
    pub report_hash: String,
}

#[derive(Clone)]
struct PilotPair {
    evidence: PilotPairEvidenceIdentity,
    current: RuntimeMetrics,
    compact: RuntimeMetrics,
}

/// Analyzes exactly the 48 marker-referenced pilot pairs after planning is frozen.
pub fn analyze_pilot_records(
    plan: &FrozenPlan,
    planning_report: &PlanningReport,
    records: &Path,
) -> Result<ExploratoryPilotReport, PilotAnalysisError> {
    validate_frozen_plan(plan).map_err(error)?;

    // We compare the complete blinded planning result before projecting labels.
    let recomputed = plan_main(plan, records).map_err(error)?;
    if recomputed.report != *planning_report {
        return Err(PilotAnalysisError(
            "planning report does not exactly match deterministic blinded planning".into(),
        ));
    }
    validate_pilot_shape(plan)?;

    let state = scan(records).map_err(error)?;
    validate_stream_scope(
        plan,
        &state,
        &planning_report.pilot_evidence.runtime_context,
    )?;
    let observed_smoke_pairs = state
        .completion_markers
        .values()
        .filter(|marker| marker.identity.phase == SchedulePhase::Smoke)
        .count();
    let observed_pilot_pairs = state
        .completion_markers
        .values()
        .filter(|marker| marker.identity.phase == SchedulePhase::Pilot)
        .count();

    let mut pairs = Vec::with_capacity(PILOT_PAIRS);
    for scheduled in &plan.schedule.pilot {
        let completed = completed_pair(
            &state,
            &planning_report.pilot_evidence.unplanned_schedule_hash,
            scheduled,
        )
        .map_err(error)?;
        let [first, second] = completed.trials;
        let (current_trial, compact_trial, treatment_order) = normalize_treatments(first, second)?;
        let current = metrics(current_trial)?;
        let compact = metrics(compact_trial)?;
        validate_pilot_runtime_context(
            current_trial,
            &current,
            &planning_report.pilot_evidence.runtime_context,
        )
        .map_err(error)?;
        validate_pilot_runtime_context(
            compact_trial,
            &compact,
            &planning_report.pilot_evidence.runtime_context,
        )
        .map_err(error)?;
        if !payloads_equivalent(&current, &compact) {
            return Err(PilotAnalysisError(
                "pilot pair has unequal actual first-request payload hashes".into(),
            ));
        }
        pairs.push(PilotPair {
            evidence: PilotPairEvidenceIdentity {
                pair_id: scheduled.pair_id.clone(),
                pair_identity_hash: scheduled.pair_identity_hash.clone(),
                archetype_id: scheduled.archetype_id.clone(),
                archetype_repetition: scheduled.archetype_repetition,
                task_id: scheduled.task_id.clone(),
                instance_hash: scheduled.instance_hash.clone(),
                attempt_id: completed.marker.identity.attempt_id.clone(),
                treatment_order: treatment_order.into(),
                completion_marker_hash: completed.marker.record_hash.clone(),
                current_trial_record_hash: current_trial.record_hash.clone(),
                compact_v1_trial_record_hash: compact_trial.record_hash.clone(),
            },
            current,
            compact,
        });
    }
    pairs.sort_by(|left, right| left.evidence.pair_id.cmp(&right.evidence.pair_id));

    let mut by_archetype = BTreeMap::new();
    let mut observed_pairs_per_archetype = BTreeMap::new();
    for archetype in &plan.manifest.archetypes {
        let stratum = pairs
            .iter()
            .filter(|pair| pair.evidence.archetype_id == archetype.id)
            .collect::<Vec<_>>();
        observed_pairs_per_archetype.insert(archetype.id.clone(), stratum.len());
        by_archetype.insert(archetype.id.clone(), summarize(&stratum));
    }
    if observed_pairs_per_archetype
        .values()
        .any(|count| *count != PAIRS_PER_ARCHETYPE)
    {
        return Err(PilotAnalysisError(
            "pilot report requires exactly three pairs per archetype".into(),
        ));
    }

    let model = plan
        .model
        .as_ref()
        .ok_or_else(|| PilotAnalysisError("planned pilot has no model selection".into()))?;
    let ab = pairs
        .iter()
        .filter(|pair| pair.evidence.treatment_order == "current_then_compact_v1")
        .count();
    let descriptions = plan
        .descriptions
        .each_ref()
        .map(|description| PilotDescriptionIdentity {
            variant: description.variant,
            sha256: description.sha256.clone(),
            byte_length: description.byte_length,
        });
    let all = pairs.iter().collect::<Vec<_>>();
    let mut report = ExploratoryPilotReport {
        schema_version: 2,
        analysis_kind: "exploratory_pilot_descriptive".into(),
        shipping_eligible: false,
        identities: PilotReportIdentities {
            run_id: plan.schedule.run_id.clone(),
            universe_hash: plan.universe.universe_hash.clone(),
            unplanned_schedule_hash: planning_report.unplanned_schedule_hash.clone(),
            planning_report_hash: planning_report.report_hash.clone(),
            blinded_pilot_hash: planning_report.blinded_pilot_hash.clone(),
            pilot_completion_stream_hash: planning_report
                .pilot_evidence
                .completion_stream_hash
                .clone(),
        },
        provenance: planning_report.pilot_evidence.runtime_context.clone(),
        model: PilotModelIdentity {
            provider: model.provider.clone(),
            model: model.model.clone(),
            reasoning: model.reasoning.clone(),
            model_selection_hash: model.selection_hash.clone(),
            model_catalog_hash: model.catalog_hash.clone(),
            model_capability_hash: model.model_capability_hash.clone(),
            tool_catalog_hash: model.tool_catalog_hash.clone(),
        },
        descriptions,
        completeness: PilotCompleteness {
            expected_smoke_pairs: plan.schedule.smoke.len(),
            observed_smoke_pairs,
            expected_pilot_pairs: PILOT_PAIRS,
            observed_pilot_pairs,
            expected_archetypes: ARCHETYPES,
            observed_archetypes: observed_pairs_per_archetype.len(),
            expected_pairs_per_archetype: PAIRS_PER_ARCHETYPE,
            observed_pairs_per_archetype,
            report_sample_pairs: pairs.len(),
            report_sample_sessions: pairs.len() * 2,
            smoke_pairs_in_report_sample: 0,
        },
        treatment_order_counts: PilotOrderCounts {
            ab_current_then_compact_v1: ab,
            ba_compact_v1_then_current: pairs.len() - ab,
        },
        pair_evidence: pairs.iter().map(|pair| pair.evidence.clone()).collect(),
        overall: summarize(&all),
        by_archetype,
        report_hash: String::new(),
    };
    report.report_hash = compute_report_hash(&report)?;
    Ok(report)
}

fn validate_pilot_shape(plan: &FrozenPlan) -> Result<(), PilotAnalysisError> {
    if plan.manifest.archetypes.len() != ARCHETYPES || plan.schedule.pilot.len() != PILOT_PAIRS {
        return Err(PilotAnalysisError(
            "pilot report requires exactly 48 pairs across 16 archetypes".into(),
        ));
    }
    let mut counts = BTreeMap::<&str, usize>::new();
    for pair in &plan.schedule.pilot {
        *counts.entry(&pair.archetype_id).or_default() += 1;
    }
    if plan
        .manifest
        .archetypes
        .iter()
        .any(|archetype| counts.get(archetype.id.as_str()) != Some(&PAIRS_PER_ARCHETYPE))
    {
        return Err(PilotAnalysisError(
            "pilot report requires exactly three pairs per archetype".into(),
        ));
    }
    Ok(())
}

fn validate_stream_scope(
    plan: &FrozenPlan,
    state: &crate::artifacts::ResumeState,
    context: &FrozenPilotRuntimeContext,
) -> Result<(), PilotAnalysisError> {
    if state.truncated_final_line {
        return Err(PilotAnalysisError(
            "records stream has a truncated final line".into(),
        ));
    }
    let excluded = plan
        .schedule
        .smoke
        .iter()
        .chain(&plan.schedule.pilot)
        .map(|pair| (pair.pair_id.as_str(), pair))
        .collect::<BTreeMap<_, _>>();
    for marker in state.completion_markers.values() {
        if marker.identity.phase == SchedulePhase::Main {
            continue;
        }
        if marker.identity.run_id != plan.schedule.run_id {
            return Err(PilotAnalysisError(
                "records mix different frozen runs or model selections".into(),
            ));
        }
        let expected = match marker.identity.phase {
            SchedulePhase::Smoke | SchedulePhase::Pilot => {
                if marker.identity.schedule_hash != plan.schedule.schedule_hash {
                    return Err(PilotAnalysisError(
                        "records mix smoke or pilot schedule hashes".into(),
                    ));
                }
                excluded.get(marker.identity.pair_id.as_str())
            }
            SchedulePhase::Main => continue,
        };
        if expected.is_none_or(|pair| pair.phase != marker.identity.phase) {
            return Err(PilotAnalysisError(
                "records contain an extra completion marker".into(),
            ));
        }
    }

    let mut attempts = BTreeMap::<(&str, &str), BTreeSet<&str>>::new();
    for trial in state.trials_by_hash.values() {
        if trial.identity.phase == SchedulePhase::Main {
            continue;
        }
        if trial.identity.run_id != plan.schedule.run_id {
            return Err(PilotAnalysisError(
                "records mix different frozen runs or model selections".into(),
            ));
        }
        let expected = match trial.identity.phase {
            SchedulePhase::Smoke | SchedulePhase::Pilot => {
                if trial.identity.schedule_hash != plan.schedule.schedule_hash {
                    return Err(PilotAnalysisError(
                        "records mix smoke or pilot schedule hashes".into(),
                    ));
                }
                excluded.get(trial.identity.pair_id.as_str())
            }
            SchedulePhase::Main => continue,
        }
        .ok_or_else(|| PilotAnalysisError("records contain an unscheduled trial".into()))?;
        validate_trial_slot(expected, trial)?;
        let runtime = parse_metrics(trial)?;
        validate_pilot_runtime_context(trial, &runtime, context).map_err(error)?;
        attempts
            .entry((
                trial.identity.pair_id.as_str(),
                phase_name(trial.identity.phase),
            ))
            .or_default()
            .insert(trial.identity.attempt_id.as_str());
    }
    if attempts
        .values()
        .any(|pair_attempts| pair_attempts.len() > MAX_PAIR_ATTEMPTS)
    {
        return Err(PilotAnalysisError(format!(
            "records exceed the frozen {MAX_PAIR_ATTEMPTS}-attempt limit"
        )));
    }
    Ok(())
}

fn validate_trial_slot(
    pair: &PairScheduleRecord,
    trial: &TrialRecord,
) -> Result<(), PilotAnalysisError> {
    let scheduled = pair
        .trials
        .iter()
        .find(|scheduled| {
            scheduled.order_index == trial.identity.order_index
                && scheduled.variant == trial.identity.variant
        })
        .ok_or_else(|| PilotAnalysisError("trial occupies no frozen pair slot".into()))?;
    if trial.identity.pair_id != scheduled.pair_id
        || trial.identity.task_id != scheduled.task_id
        || trial.identity.instance_hash != scheduled.instance_hash
        || trial.identity.archetype_id != pair.archetype_id
        || trial.identity.phase != scheduled.phase
        || trial.identity.repetition != scheduled.archetype_repetition
    {
        return Err(PilotAnalysisError(
            "trial does not match its exact frozen schedule identity".into(),
        ));
    }
    Ok(())
}

fn phase_name(phase: SchedulePhase) -> &'static str {
    match phase {
        SchedulePhase::Smoke => "smoke",
        SchedulePhase::Pilot => "pilot",
        SchedulePhase::Main => "main",
    }
}

fn normalize_treatments<'a>(
    first: &'a TrialRecord,
    second: &'a TrialRecord,
) -> Result<(&'a TrialRecord, &'a TrialRecord, &'static str), PilotAnalysisError> {
    match (first.identity.variant, second.identity.variant) {
        (DescriptionVariant::Current, DescriptionVariant::CompactV1) => {
            Ok((first, second, "current_then_compact_v1"))
        }
        (DescriptionVariant::CompactV1, DescriptionVariant::Current) => {
            Ok((second, first, "compact_v1_then_current"))
        }
        _ => Err(PilotAnalysisError(
            "complete pilot pair does not contain both variants".into(),
        )),
    }
}

fn parse_metrics(trial: &TrialRecord) -> Result<RuntimeMetrics, PilotAnalysisError> {
    serde_json::from_value(trial.runtime.clone()).map_err(|parse_error| {
        PilotAnalysisError(format!(
            "trial {} is missing required runtime metrics: {parse_error}",
            trial.record_hash
        ))
    })
}

fn metrics(trial: &TrialRecord) -> Result<RuntimeMetrics, PilotAnalysisError> {
    let runtime = parse_metrics(trial)?;
    if !runtime.valid
        || !runtime.aj_recorded_catalog_cost.is_finite()
        || runtime.aj_recorded_catalog_cost < 0.0
    {
        return Err(PilotAnalysisError(
            "completed pilot trial has invalid required metrics".into(),
        ));
    }
    Ok(runtime)
}

fn payloads_equivalent(current: &RuntimeMetrics, compact: &RuntimeMetrics) -> bool {
    if current.provider_requests == 0 && compact.provider_requests == 0 {
        return true;
    }
    current.provider_requests > 0
        && compact.provider_requests > 0
        && current.normalized_first_request_hash.is_some()
        && current.normalized_first_request_hash == compact.normalized_first_request_hash
}

fn summarize(pairs: &[&PilotPair]) -> PilotDescriptiveSummary {
    let binary_fields: [(&str, fn(&RuntimeMetrics) -> bool); 3] = [
        ("task_success", |runtime| runtime.task_passed),
        ("sessions_with_patch_failure", |runtime| {
            runtime.sessions_with_patch_failure
        }),
        ("edit_bypass", |runtime| runtime.edit_bypass),
    ];
    let continuous_fields: [(&str, fn(&RuntimeMetrics) -> f64); 8] = [
        ("model_responses", |runtime| {
            u64_as_f64(runtime.model_responses)
        }),
        ("input_tokens", |runtime| u64_as_f64(runtime.usage.input)),
        ("output_tokens", |runtime| u64_as_f64(runtime.usage.output)),
        ("cache_read_tokens", |runtime| {
            u64_as_f64(runtime.usage.cache_read)
        }),
        ("cache_write_tokens", |runtime| {
            u64_as_f64(runtime.usage.cache_write)
        }),
        ("total_tokens", |runtime| {
            u64_as_f64(runtime.usage.total_tokens)
        }),
        ("latency_millis", |runtime| {
            u64_as_f64(runtime.duration_millis)
        }),
        ("aj_recorded_catalog_cost", |runtime| {
            runtime.aj_recorded_catalog_cost
        }),
    ];
    PilotDescriptiveSummary {
        pair_count: pairs.len(),
        binary_outcomes: binary_fields
            .into_iter()
            .map(|(endpoint, field)| binary_summary(endpoint, pairs, field))
            .collect(),
        continuous_endpoints: continuous_fields
            .into_iter()
            .map(|(endpoint, field)| continuous_summary(endpoint, pairs, field))
            .collect(),
    }
}

fn binary_summary(
    endpoint: &str,
    pairs: &[&PilotPair],
    field: fn(&RuntimeMetrics) -> bool,
) -> PilotBinaryEndpoint {
    let mut discordance = PairedDiscordanceDescription {
        neither: 0,
        current_only: 0,
        compact_v1_only: 0,
        both: 0,
    };
    for pair in pairs {
        match (field(&pair.current), field(&pair.compact)) {
            (false, false) => discordance.neither += 1,
            (true, false) => discordance.current_only += 1,
            (false, true) => discordance.compact_v1_only += 1,
            (true, true) => discordance.both += 1,
        }
    }
    let observations = u64::try_from(pairs.len()).expect("pilot pair count fits u64");
    let current_count = discordance.current_only + discordance.both;
    let compact_count = discordance.compact_v1_only + discordance.both;
    let current = BinaryVariantDescription {
        count: current_count,
        observations,
        rate: ratio(current_count, observations),
    };
    let compact_v1 = BinaryVariantDescription {
        count: compact_count,
        observations,
        rate: ratio(compact_count, observations),
    };
    PilotBinaryEndpoint {
        endpoint: endpoint.into(),
        current,
        compact_v1,
        paired_discordance: discordance,
        observed_difference_compact_v1_minus_current: compact_v1.rate - current.rate,
    }
}

fn continuous_summary(
    endpoint: &str,
    pairs: &[&PilotPair],
    field: fn(&RuntimeMetrics) -> f64,
) -> PilotContinuousEndpoint {
    let current_values = pairs
        .iter()
        .map(|pair| field(&pair.current))
        .collect::<Vec<_>>();
    let compact_values = pairs
        .iter()
        .map(|pair| field(&pair.compact))
        .collect::<Vec<_>>();
    let differences = current_values
        .iter()
        .zip(&compact_values)
        .map(|(current, compact)| compact - current)
        .collect::<Vec<_>>();
    let current = distribution(&current_values);
    let compact_v1 = distribution(&compact_values);
    PilotContinuousEndpoint {
        endpoint: endpoint.into(),
        current,
        compact_v1,
        absolute_mean_difference_compact_v1_minus_current: compact_v1.mean - current.mean,
        relative_mean_difference_compact_v1_minus_current: (current.mean != 0.0)
            .then_some(compact_v1.mean / current.mean - 1.0),
        within_pair_difference_compact_v1_minus_current: distribution(&differences),
    }
}

fn distribution(values: &[f64]) -> PilotDistribution {
    debug_assert!(!values.is_empty());
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    let median = if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    };
    PilotDistribution {
        count: sorted.len(),
        minimum: sorted[0],
        mean: sorted.iter().sum::<f64>() / usize_as_f64(sorted.len()),
        median,
        p95: nearest_rank(&sorted, 0.95),
        maximum: sorted[sorted.len() - 1],
    }
}

fn nearest_rank(sorted: &[f64], quantile: f64) -> f64 {
    let rank = (quantile * usize_as_f64(sorted.len())).ceil();
    let index = positive_f64_as_usize(rank)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index]
}

fn compute_report_hash(report: &ExploratoryPilotReport) -> Result<String, PilotAnalysisError> {
    let mut unhashed = report.clone();
    unhashed.report_hash.clear();
    let bytes = serde_json::to_vec(&unhashed)
        .map_err(|serialize_error| PilotAnalysisError(serialize_error.to_string()))?;
    Ok(hash_framed(
        b"apply-patch-exploratory-pilot-report-v2",
        &[&bytes],
    ))
}

/// Renders the deterministic descriptive Markdown report.
pub fn render_pilot_markdown(report: &ExploratoryPilotReport) -> String {
    let mut output = String::from(
        "Exploratory pilot report. Not eligible for a shipping decision.\n\n# Apply patch description exploratory pilot\n\n",
    );
    output.push_str(&format!(
        "Analysis kind: `{}`\n\nReport hash: `{}`\n\nPilot pairs: `{}`. Smoke pairs in report sample: `0`.\n\n",
        report.analysis_kind, report.report_hash, report.completeness.report_sample_pairs
    ));
    output.push_str("## Frozen identities\n\n");
    output.push_str(&format!(
        "- Run: `{}`\n- Universe: `{}`\n- Unplanned schedule: `{}`\n- Planning report: `{}`\n- Pilot evidence: `{}`\n- Provider/model/reasoning: `{}` / `{}` / `{}`\n\n",
        report.identities.run_id,
        report.identities.universe_hash,
        report.identities.unplanned_schedule_hash,
        report.identities.planning_report_hash,
        report.identities.pilot_completion_stream_hash,
        report.model.provider,
        report.model.model,
        report.model.reasoning,
    ));
    output.push_str(&format!(
        "Treatment order: AB `{}`, BA `{}`.\n\n",
        report.treatment_order_counts.ab_current_then_compact_v1,
        report.treatment_order_counts.ba_compact_v1_then_current
    ));
    output.push_str("## Overall binary outcomes\n\n| Endpoint | Current count / rate | Compact count / rate | Compact minus current | Current only | Compact only |\n| --- | ---: | ---: | ---: | ---: | ---: |\n");
    for endpoint in &report.overall.binary_outcomes {
        output.push_str(&format!(
            "| {} | {} / {:.4} | {} / {:.4} | {:.4} | {} | {} |\n",
            endpoint.endpoint,
            endpoint.current.count,
            endpoint.current.rate,
            endpoint.compact_v1.count,
            endpoint.compact_v1.rate,
            endpoint.observed_difference_compact_v1_minus_current,
            endpoint.paired_discordance.current_only,
            endpoint.paired_discordance.compact_v1_only,
        ));
    }
    output.push_str("\n## Overall continuous endpoints\n\n| Endpoint | Current mean / median / p95 | Compact mean / median / p95 | Absolute mean difference | Relative mean difference | Within-pair difference mean / median / p95 |\n| --- | ---: | ---: | ---: | ---: | ---: |\n");
    for endpoint in &report.overall.continuous_endpoints {
        let relative = endpoint
            .relative_mean_difference_compact_v1_minus_current
            .map_or_else(|| "undefined".into(), |value| format!("{value:.6}"));
        output.push_str(&format!(
            "| {} | {:.6} / {:.6} / {:.6} | {:.6} / {:.6} / {:.6} | {:.6} | {} | {:.6} / {:.6} / {:.6} |\n",
            endpoint.endpoint,
            endpoint.current.mean,
            endpoint.current.median,
            endpoint.current.p95,
            endpoint.compact_v1.mean,
            endpoint.compact_v1.median,
            endpoint.compact_v1.p95,
            endpoint.absolute_mean_difference_compact_v1_minus_current,
            relative,
            endpoint.within_pair_difference_compact_v1_minus_current.mean,
            endpoint.within_pair_difference_compact_v1_minus_current.median,
            endpoint.within_pair_difference_compact_v1_minus_current.p95,
        ));
    }
    output.push_str("\n## Per-archetype descriptions\n\n");
    for (archetype, summary) in &report.by_archetype {
        output.push_str(&format!(
            "- `{archetype}`: {} pairs. Binary: `{}`. Continuous: `{}`.\n",
            summary.pair_count,
            serde_json::to_string(&summary.binary_outcomes).expect("report serialization succeeds"),
            serde_json::to_string(&summary.continuous_endpoints)
                .expect("report serialization succeeds"),
        ));
    }
    output.push_str("\n## Pair evidence identities\n\n");
    for pair in &report.pair_evidence {
        output.push_str(&format!(
            "- `{}` `{}` `{}` marker `{}` current `{}` compact `{}`\n",
            pair.pair_id,
            pair.archetype_id,
            pair.treatment_order,
            pair.completion_marker_hash,
            pair.current_trial_record_hash,
            pair.compact_v1_trial_record_hash,
        ));
    }
    output
}

fn error(error: impl fmt::Display) -> PilotAnalysisError {
    PilotAnalysisError(error.to_string())
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    u64_as_f64(numerator) / u64_as_f64(denominator)
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
    use std::fs;
    use std::sync::OnceLock;

    use serde_json::{Value, json};

    use super::*;
    use crate::artifacts::{
        ArtifactLog, ArtifactRecord, PairCompletionIdentity, RecordedDescription, TrialIdentity,
        TrialMetadata, TrialRecord,
    };
    use crate::planning::plan_main;
    use crate::schedule::{PairScheduleRecord, freeze_plan, test_model_selection};
    use crate::suite::committed_manifest;

    struct Fixture {
        plan_json: Vec<u8>,
        planning_report_json: Vec<u8>,
        records: Vec<u8>,
        abandoned_hash: String,
    }

    static FIXTURE: OnceLock<Fixture> = OnceLock::new();

    fn fixture() -> &'static Fixture {
        FIXTURE.get_or_init(build_fixture)
    }

    fn build_fixture() -> Fixture {
        let manifest = committed_manifest().unwrap();
        let plan = freeze_plan(
            &manifest,
            "exploratory-pilot-report-tests",
            6,
            test_model_selection(),
        )
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let records_path = temp.path().join("records.jsonl");
        let mut log = ArtifactLog::open(&records_path).unwrap();
        let pilot = &plan.schedule.pilot[0];
        let abandoned = trial(&plan, pilot, &pilot.trials[0], "abandoned", None);
        let abandoned_hash = abandoned.record_hash.clone();
        log.append_trial(&abandoned).unwrap();
        for pair in plan.schedule.smoke.iter().chain(&plan.schedule.pilot) {
            append_pair(&mut log, &plan, pair);
        }
        drop(log);
        let outcome = plan_main(&plan, &records_path).unwrap();
        Fixture {
            plan_json: serde_json::to_vec(&plan).unwrap(),
            planning_report_json: serde_json::to_vec(&outcome.report).unwrap(),
            records: fs::read(records_path).unwrap(),
            abandoned_hash,
        }
    }

    fn write_fixture() -> (
        tempfile::TempDir,
        FrozenPlan,
        PlanningReport,
        std::path::PathBuf,
    ) {
        let fixture = fixture();
        let temp = tempfile::tempdir().unwrap();
        let records = temp.path().join("records.jsonl");
        fs::write(&records, &fixture.records).unwrap();
        let plan = serde_json::from_slice(&fixture.plan_json).unwrap();
        let planning_report = serde_json::from_slice(&fixture.planning_report_json).unwrap();
        (temp, plan, planning_report, records)
    }

    fn metadata(plan: &FrozenPlan, pair: &PairScheduleRecord) -> TrialMetadata {
        let model = plan.model.as_ref().unwrap();
        let instance = plan
            .universe
            .instances
            .iter()
            .find(|instance| instance.instance_hash == pair.instance_hash)
            .unwrap();
        TrialMetadata {
            task_seed: instance.task_seed.clone(),
            current_description: RecordedDescription {
                sha256: plan.descriptions[0].sha256.clone(),
                byte_length: plan.descriptions[0].byte_length,
            },
            compact_description: RecordedDescription {
                sha256: plan.descriptions[1].sha256.clone(),
                byte_length: plan.descriptions[1].byte_length,
            },
            aj_revision: "test-head".into(),
            suite_revision: plan.universe.suite_revision.clone(),
            model_catalog_hash: model.catalog_hash.clone(),
            provider: model.provider.clone(),
            model: model.model.clone(),
            reasoning_effort: model.reasoning.clone(),
            tool_catalog_hash: model.tool_catalog_hash.clone(),
            fixture_revision: "fixture-v1".into(),
        }
    }

    fn runtime(
        plan: &FrozenPlan,
        pair: &PairScheduleRecord,
        scheduled: &crate::schedule::TrialScheduleRecord,
    ) -> Value {
        let mut value = serde_json::to_value(crate::runtime::completed_runtime_fixture()).unwrap();
        let object = value.as_object_mut().unwrap();
        let current = scheduled.variant == DescriptionVariant::Current;
        let repetition = scheduled.archetype_repetition;
        let swap = {
            let hash = hash_framed(
                b"pilot-treatment-orientation-v1",
                &[plan.universe.run_seed.as_bytes(), pair.pair_id.as_bytes()],
            );
            u8::from_str_radix(&hash[..2], 16).unwrap() & 1 == 1
        };
        let blinded_first = (scheduled.order_index == 0) != swap;
        let offset: f64 = if repetition.is_multiple_of(2) {
            0.1
        } else {
            -0.1
        };
        let first_cost = 10.0 + offset;
        let second_cost = 10.0 - offset;
        let first_responses = if offset.is_sign_positive() { 9 } else { 11 };
        let second_responses = if offset.is_sign_positive() { 11 } else { 9 };
        let task_passed = !current || repetition != 0;
        object.insert(
            "terminal_status".into(),
            json!(if task_passed {
                "passed"
            } else {
                "verifier_failed"
            }),
        );
        object.insert("task_passed".into(), json!(task_passed));
        object.insert(
            "sessions_with_patch_failure".into(),
            json!(current && repetition == 1),
        );
        object.insert("edit_bypass".into(), json!(!current && repetition == 2));
        object.insert(
            "aj_recorded_catalog_cost".into(),
            json!(if blinded_first {
                first_cost
            } else {
                second_cost
            }),
        );
        let responses = if blinded_first {
            first_responses
        } else {
            second_responses
        };
        object.insert("model_responses".into(), json!(responses));
        object.insert("provider_requests".into(), json!(responses));
        object.get_mut("limits").unwrap()["max_provider_requests"] = json!(12);
        object.get_mut("limits").unwrap()["max_model_responses"] = json!(12);
        object.insert(
            "duration_millis".into(),
            json!(if current { 20 } else { 10 }),
        );
        object.insert(
            "normalized_first_request_hash".into(),
            json!("same-payload"),
        );
        object.insert("system_prompt_hash".into(), json!("system"));
        object.insert("conservative_catalog_pair_reserve".into(), json!(100.0));
        object.insert("baseline_root_hash".into(), json!("fixture-v1"));
        object.insert("image_id".into(), json!("sha256:test-image"));
        object.insert(
            "source_provenance".into(),
            json!({"head":"test-head","dirty":false,"worktree_hash":null}),
        );
        object.insert("utc_date".into(), json!("2026-07-24"));
        object.insert(
            "usage".into(),
            json!({
                "input": if current { 100 } else { 80 },
                "output": if current { 20 } else { 10 },
                "cache_read": if current { 5 } else { 4 },
                "cache_write": 0,
                "total_tokens": if current { 125 } else { 94 },
                "cost": {"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0}
            }),
        );
        value
    }

    fn trial(
        plan: &FrozenPlan,
        pair: &PairScheduleRecord,
        scheduled: &crate::schedule::TrialScheduleRecord,
        attempt: &str,
        metadata_override: Option<TrialMetadata>,
    ) -> TrialRecord {
        TrialRecord::new(
            TrialIdentity {
                run_id: pair.run_id.clone(),
                pair_id: pair.pair_id.clone(),
                attempt_id: attempt.into(),
                task_id: pair.task_id.clone(),
                instance_hash: pair.instance_hash.clone(),
                archetype_id: pair.archetype_id.clone(),
                schedule_hash: plan.schedule.schedule_hash.clone(),
                phase: pair.phase,
                repetition: scheduled.archetype_repetition,
                variant: scheduled.variant,
                order_index: scheduled.order_index,
            },
            metadata_override.unwrap_or_else(|| metadata(plan, pair)),
            runtime(plan, pair, scheduled),
        )
        .unwrap()
    }

    fn append_pair(log: &mut ArtifactLog, plan: &FrozenPlan, pair: &PairScheduleRecord) {
        let attempt = format!("attempt-{}", pair.pair_id);
        let records = pair
            .trials
            .each_ref()
            .map(|scheduled| trial(plan, pair, scheduled, &attempt, None));
        for record in &records {
            log.append_trial(record).unwrap();
        }
        log.complete_pair(
            PairCompletionIdentity {
                run_id: pair.run_id.clone(),
                pair_id: pair.pair_id.clone(),
                attempt_id: attempt,
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
        .unwrap();
    }

    #[test]
    fn report_is_exact_descriptive_deterministic_and_excludes_unmarked_and_smoke() {
        let (_temp, plan, planning_report, records) = write_fixture();
        let first = analyze_pilot_records(&plan, &planning_report, &records).unwrap();
        let second = analyze_pilot_records(&plan, &planning_report, &records).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.report_hash, compute_report_hash(&first).unwrap());
        assert_eq!(first.analysis_kind, "exploratory_pilot_descriptive");
        assert_eq!(
            planning_report.sample_plan.conclusion,
            crate::statistics::PlanningConclusion::InconclusiveInsufficientUniverse
        );
        assert_eq!(
            first.identities.planning_report_hash,
            planning_report.report_hash
        );
        assert!(!first.shipping_eligible);
        assert_eq!(first.completeness.observed_pilot_pairs, 48);
        assert_eq!(first.completeness.report_sample_pairs, 48);
        assert_eq!(first.completeness.report_sample_sessions, 96);
        assert_eq!(first.completeness.smoke_pairs_in_report_sample, 0);
        assert_eq!(first.overall.pair_count, 48);
        assert_eq!(first.pair_evidence.len(), 48);
        assert!(
            first
                .completeness
                .observed_pairs_per_archetype
                .values()
                .all(|count| *count == 3)
        );
        assert_eq!(first.by_archetype.len(), 16);
        assert!(
            first
                .by_archetype
                .values()
                .all(|summary| summary.pair_count == 3)
        );
        assert_eq!(
            first.treatment_order_counts.ab_current_then_compact_v1
                + first.treatment_order_counts.ba_compact_v1_then_current,
            48
        );
        assert!(first.treatment_order_counts.ab_current_then_compact_v1 > 0);
        assert!(first.treatment_order_counts.ba_compact_v1_then_current > 0);
        assert!(
            first
                .pair_evidence
                .iter()
                .all(|pair| pair.current_trial_record_hash != fixture().abandoned_hash)
        );
        let input_tokens = first
            .overall
            .continuous_endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint == "input_tokens")
            .unwrap();
        assert_eq!(input_tokens.current.mean, 100.0);
        assert_eq!(input_tokens.compact_v1.mean, 80.0);
        assert_eq!(
            input_tokens.absolute_mean_difference_compact_v1_minus_current,
            -20.0
        );
        assert_eq!(
            input_tokens
                .within_pair_difference_compact_v1_minus_current
                .mean,
            -20.0
        );
        let markdown = render_pilot_markdown(&first);
        assert!(
            markdown.starts_with("Exploratory pilot report. Not eligible for a shipping decision.")
        );
        assert_eq!(markdown, render_pilot_markdown(&second));
    }

    #[test]
    fn serialized_report_matches_the_exact_descriptive_schema() {
        let (_temp, plan, planning_report, records) = write_fixture();
        let value =
            serde_json::to_value(analyze_pilot_records(&plan, &planning_report, &records).unwrap())
                .unwrap();
        assert_descriptive_schema(&value, "root").unwrap();

        for forbidden in [
            "shipping_decision",
            "confidence_bounds",
            "p_values",
            "achieved_power",
            "threshold_passed",
            "sample_recommendation",
            "unexpected_key",
        ] {
            let mut contaminated = value.clone();
            contaminated
                .as_object_mut()
                .unwrap()
                .insert(forbidden.into(), Value::Null);
            assert!(assert_descriptive_schema(&contaminated, "root").is_err());
        }
    }

    fn assert_descriptive_schema(value: &Value, path: &str) -> Result<(), String> {
        match value {
            Value::Object(object) if path == "root.by_archetype" => {
                for value in object.values() {
                    assert_descriptive_schema(value, "root.by_archetype.*")?;
                }
            }
            Value::Object(object) if path == "root.completeness.observed_pairs_per_archetype" => {
                if object.values().any(|value| !value.is_number()) {
                    return Err(format!("non-numeric archetype count at {path}"));
                }
            }
            Value::Object(object) => {
                let expected = allowed_schema_keys(path)
                    .ok_or_else(|| format!("unexpected object at {path}"))?;
                let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
                let expected = expected.iter().copied().collect::<BTreeSet<_>>();
                if actual != expected {
                    return Err(format!(
                        "schema keys differ at {path}: actual {actual:?}, expected {expected:?}"
                    ));
                }
                for (key, value) in object {
                    let child = format!("{path}.{key}");
                    assert_descriptive_schema(value, &child)?;
                }
            }
            Value::Array(values) => {
                let child = format!("{path}[]");
                for value in values {
                    assert_descriptive_schema(value, &child)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn allowed_schema_keys(path: &str) -> Option<&'static [&'static str]> {
        match path {
            "root" => Some(&[
                "schema_version",
                "analysis_kind",
                "shipping_eligible",
                "identities",
                "provenance",
                "model",
                "descriptions",
                "completeness",
                "treatment_order_counts",
                "pair_evidence",
                "overall",
                "by_archetype",
                "report_hash",
            ]),
            "root.identities" => Some(&[
                "run_id",
                "universe_hash",
                "unplanned_schedule_hash",
                "planning_report_hash",
                "blinded_pilot_hash",
                "pilot_completion_stream_hash",
            ]),
            "root.provenance" => Some(&[
                "image_id",
                "source_provenance",
                "utc_date",
                "limits",
                "system_prompt_hash",
                "aj_revision",
                "model_catalog_hash",
                "provider",
                "model",
                "reasoning_effort",
                "tool_catalog_hash",
                "suite_revision",
                "current_description",
                "compact_description",
                "conservative_catalog_pair_reserve",
            ]),
            "root.provenance.source_provenance" => Some(&["head", "dirty", "worktree_hash"]),
            "root.provenance.limits" => Some(&[
                "wall_timeout_seconds",
                "max_provider_requests",
                "max_model_responses",
                "provider_output_token_ceiling",
                "aggregate_observed_output_token_ceiling",
            ]),
            "root.provenance.current_description" | "root.provenance.compact_description" => {
                Some(&["sha256", "byte_length"])
            }
            "root.model" => Some(&[
                "provider",
                "model",
                "reasoning",
                "model_selection_hash",
                "model_catalog_hash",
                "model_capability_hash",
                "tool_catalog_hash",
            ]),
            "root.descriptions[]" => Some(&["variant", "sha256", "byte_length"]),
            "root.completeness" => Some(&[
                "expected_smoke_pairs",
                "observed_smoke_pairs",
                "expected_pilot_pairs",
                "observed_pilot_pairs",
                "expected_archetypes",
                "observed_archetypes",
                "expected_pairs_per_archetype",
                "observed_pairs_per_archetype",
                "report_sample_pairs",
                "report_sample_sessions",
                "smoke_pairs_in_report_sample",
            ]),
            "root.treatment_order_counts" => {
                Some(&["ab_current_then_compact_v1", "ba_compact_v1_then_current"])
            }
            "root.pair_evidence[]" => Some(&[
                "pair_id",
                "pair_identity_hash",
                "archetype_id",
                "archetype_repetition",
                "task_id",
                "instance_hash",
                "attempt_id",
                "treatment_order",
                "completion_marker_hash",
                "current_trial_record_hash",
                "compact_v1_trial_record_hash",
            ]),
            "root.overall" | "root.by_archetype.*" => {
                Some(&["pair_count", "binary_outcomes", "continuous_endpoints"])
            }
            "root.overall.binary_outcomes[]" | "root.by_archetype.*.binary_outcomes[]" => Some(&[
                "endpoint",
                "current",
                "compact_v1",
                "paired_discordance",
                "observed_difference_compact_v1_minus_current",
            ]),
            "root.overall.binary_outcomes[].current"
            | "root.overall.binary_outcomes[].compact_v1"
            | "root.by_archetype.*.binary_outcomes[].current"
            | "root.by_archetype.*.binary_outcomes[].compact_v1" => {
                Some(&["count", "observations", "rate"])
            }
            "root.overall.binary_outcomes[].paired_discordance"
            | "root.by_archetype.*.binary_outcomes[].paired_discordance" => {
                Some(&["neither", "current_only", "compact_v1_only", "both"])
            }
            "root.overall.continuous_endpoints[]"
            | "root.by_archetype.*.continuous_endpoints[]" => Some(&[
                "endpoint",
                "current",
                "compact_v1",
                "absolute_mean_difference_compact_v1_minus_current",
                "relative_mean_difference_compact_v1_minus_current",
                "within_pair_difference_compact_v1_minus_current",
            ]),
            "root.overall.continuous_endpoints[].current"
            | "root.overall.continuous_endpoints[].compact_v1"
            | "root.overall.continuous_endpoints[].within_pair_difference_compact_v1_minus_current"
            | "root.by_archetype.*.continuous_endpoints[].current"
            | "root.by_archetype.*.continuous_endpoints[].compact_v1"
            | "root.by_archetype.*.continuous_endpoints[].within_pair_difference_compact_v1_minus_current" => {
                Some(&["count", "minimum", "mean", "median", "p95", "maximum"])
            }
            _ => None,
        }
    }

    #[test]
    fn planning_report_must_match_every_recomputed_field() {
        let (_temp, plan, mut planning_report, records) = write_fixture();
        planning_report.sample_plan.practical_cap += 1;
        let error = analyze_pilot_records(&plan, &planning_report, &records).unwrap_err();
        assert!(error.to_string().contains("does not exactly match"));
    }

    #[test]
    fn partial_main_markers_do_not_enter_or_block_the_pilot_report() {
        let (_temp, plan, planning_report, records) = write_fixture();
        let pair = &plan.schedule.pilot[0];
        let source = trial(&plan, pair, &pair.trials[0], "main-source", None);
        let main_records = [
            (DescriptionVariant::Current, 0),
            (DescriptionVariant::CompactV1, 1),
        ]
        .map(|(variant, order_index)| {
            let mut identity = source.identity.clone();
            identity.pair_id = "partial-main-pair".into();
            identity.attempt_id = "partial-main-attempt".into();
            identity.schedule_hash = "planned-main-schedule".into();
            identity.phase = SchedulePhase::Main;
            identity.variant = variant;
            identity.order_index = order_index;
            TrialRecord::new(identity, source.metadata.clone(), source.runtime.clone()).unwrap()
        });
        let mut log = ArtifactLog::open(&records).unwrap();
        for record in &main_records {
            log.append_trial(record).unwrap();
        }
        log.complete_pair(
            PairCompletionIdentity {
                run_id: plan.schedule.run_id.clone(),
                pair_id: "partial-main-pair".into(),
                attempt_id: "partial-main-attempt".into(),
                task_id: source.identity.task_id.clone(),
                instance_hash: source.identity.instance_hash.clone(),
                schedule_hash: "planned-main-schedule".into(),
                phase: SchedulePhase::Main,
            },
            [
                main_records[0].record_hash.clone(),
                main_records[1].record_hash.clone(),
            ],
        )
        .unwrap();
        drop(log);

        let report = analyze_pilot_records(&plan, &planning_report, &records).unwrap();
        assert_eq!(report.completeness.report_sample_pairs, PILOT_PAIRS);
        assert_eq!(report.pair_evidence.len(), PILOT_PAIRS);
    }

    #[test]
    fn missing_pilot_marker_is_rejected() {
        let (temp, plan, planning_report, records) = write_fixture();
        let mut lines = fs::read_to_string(&records)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let marker = lines
            .iter()
            .rposition(|line| line.contains("\"record_type\":\"pair_complete\""))
            .unwrap();
        lines.remove(marker);
        fs::write(&records, format!("{}\n", lines.join("\n"))).unwrap();
        let error = analyze_pilot_records(&plan, &planning_report, &records).unwrap_err();
        assert!(error.to_string().contains("missing complete pair"));
        drop(temp);
    }

    #[test]
    fn mixed_run_and_abandoned_context_contamination_are_rejected() {
        let (_temp, plan, planning_report, records) = write_fixture();
        let pair = &plan.schedule.pilot[0];
        let mut foreign = trial(&plan, pair, &pair.trials[0], "foreign", None);
        foreign.identity.run_id = "foreign-run".into();
        foreign = TrialRecord::new(foreign.identity, foreign.metadata, foreign.runtime).unwrap();
        ArtifactLog::open(&records)
            .unwrap()
            .append_trial(&foreign)
            .unwrap();
        assert!(
            analyze_pilot_records(&plan, &planning_report, &records)
                .unwrap_err()
                .to_string()
                .contains("mix different frozen runs")
        );

        let (_temp, plan, planning_report, records) = write_fixture();
        let pair = &plan.schedule.pilot[0];
        let mut contaminated_metadata = metadata(&plan, pair);
        contaminated_metadata.provider = "wrong-provider".into();
        let contaminated = trial(
            &plan,
            pair,
            &pair.trials[0],
            "context-contamination",
            Some(contaminated_metadata),
        );
        ArtifactLog::open(&records)
            .unwrap()
            .append_trial(&contaminated)
            .unwrap();
        assert!(
            analyze_pilot_records(&plan, &planning_report, &records)
                .unwrap_err()
                .to_string()
                .contains("runtime context frozen by the pilot")
        );
    }
    #[test]
    fn truncated_final_record_is_rejected() {
        let (_temp, plan, planning_report, records) = write_fixture();
        let mut bytes = fs::read(&records).unwrap();
        bytes.extend_from_slice(b"{\"record_type\":\"trial\"");
        fs::write(&records, bytes).unwrap();

        assert!(
            analyze_pilot_records(&plan, &planning_report, &records)
                .unwrap_err()
                .to_string()
                .contains("truncated final line")
        );
    }

    #[test]
    fn provider_contamination_in_marked_evidence_is_rejected() {
        let (_temp, plan, planning_report, records) = write_fixture();
        let mut stream = fs::read_to_string(&records)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<ArtifactRecord>(line).unwrap())
            .collect::<Vec<_>>();
        let marker_index = stream
            .iter()
            .position(|record| {
                matches!(
                    record,
                    ArtifactRecord::PairComplete(marker)
                        if marker.identity.phase == SchedulePhase::Pilot
                )
            })
            .unwrap();
        let old_hash = match &stream[marker_index] {
            ArtifactRecord::PairComplete(marker) => marker.trial_record_hashes[0].clone(),
            ArtifactRecord::Trial(_) => unreachable!(),
        };
        let trial_index = stream
            .iter()
            .position(|record| {
                matches!(record, ArtifactRecord::Trial(trial) if trial.record_hash == old_hash)
            })
            .unwrap();
        let replacement = match &stream[trial_index] {
            ArtifactRecord::Trial(trial) => {
                let mut runtime = trial.runtime.clone();
                runtime["provider_errors"] = json!(["contaminated"]);
                TrialRecord::new(trial.identity.clone(), trial.metadata.clone(), runtime).unwrap()
            }
            ArtifactRecord::PairComplete(_) => unreachable!(),
        };
        let replacement_hash = replacement.record_hash.clone();
        stream[trial_index] = ArtifactRecord::Trial(replacement);
        match &mut stream[marker_index] {
            ArtifactRecord::PairComplete(marker) => {
                marker.trial_record_hashes[0] = replacement_hash;
                let material = serde_json::to_vec(&(
                    marker.schema_version,
                    &marker.identity,
                    &marker.trial_record_hashes,
                ))
                .unwrap();
                marker.record_hash =
                    hash_framed(b"aj-apply-patch-eval-pair-marker-v1", &[&material]);
            }
            ArtifactRecord::Trial(_) => unreachable!(),
        }
        let bytes = stream
            .iter()
            .map(|record| format!("{}\n", serde_json::to_string(record).unwrap()))
            .collect::<String>();
        fs::write(&records, bytes).unwrap();

        assert!(
            analyze_pilot_records(&plan, &planning_report, &records)
                .unwrap_err()
                .to_string()
                .contains("provider-contaminated")
        );
    }
}
