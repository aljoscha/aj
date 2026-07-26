//! Blinded pilot reduction and deterministic main-schedule planning.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::artifacts::{PairKey, TrialRecord, scan};
use crate::hash_framed;
use crate::runtime::{RuntimeLimits, SourceProvenance};
use crate::schedule::{
    FrozenPlan, MainPlanning, PairScheduleRecord, ScheduleError, SchedulePhase,
    finalize_main_schedule, planned_main_pair_ids, validate_frozen_plan,
};
use crate::statistics::{
    BlindedEfficiencyPair, BlindedPlannerInput, BlindedPlannerStratum, PairedEventCounts,
    PlannerConfig, PlanningConclusion, SamplePlan, plan_sample, validate_planner,
};

const ONE_SIDED_95_Z: f64 = 1.644_853_626_951_472_2;

/// Pilot reduction or planning failure.
#[derive(Debug)]
pub struct PlanningError(pub String);

impl fmt::Display for PlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PlanningError {}

impl From<ScheduleError> for PlanningError {
    fn from(error: ScheduleError) -> Self {
        Self(error.to_string())
    }
}

/// Required runtime projection. Treatment labels never enter the planner.
#[derive(Clone, Debug, Deserialize)]
struct PilotRuntimeMetrics {
    valid: bool,
    task_passed: bool,
    sessions_with_patch_failure: bool,
    edit_bypass: bool,
    aj_recorded_catalog_cost: f64,
    model_responses: u64,
    image_id: String,
    source_provenance: SourceProvenance,
    utc_date: String,
    limits: RuntimeLimits,
    system_prompt_hash: String,
    conservative_catalog_pair_reserve: f64,
    normalized_first_request_hash: Option<String>,
}

/// Frozen runtime controls shared by every excluded trial used for planning.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FrozenPilotRuntimeContext {
    pub image_id: String,
    pub source_provenance: SourceProvenance,
    pub utc_date: String,
    pub limits: RuntimeLimits,
    pub system_prompt_hash: String,
    pub aj_revision: String,
    pub model_catalog_hash: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: String,
    pub conservative_catalog_pair_reserve: f64,
}

/// Cryptographic commitment to the exact excluded completion stream.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PilotEvidenceDigest {
    pub unplanned_schedule_hash: String,
    pub completion_stream_hash: String,
    pub completion_marker_hashes: Vec<String>,
    pub trial_record_hashes: Vec<String>,
    pub runtime_context: FrozenPilotRuntimeContext,
}

/// Label-free pilot data and pooled event tables frozen into the report.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BlindedPilotSummary {
    pub pair_count: u32,
    pub task_failure: PairedEventCounts,
    pub sessions_with_patch_failure: PairedEventCounts,
    pub edit_bypass: PairedEventCounts,
    pub planner_input: BlindedPlannerInput,
}

/// Frozen cost observations used for main-run admission control.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FrozenReserveInputs {
    pub blinded_pair_costs: Vec<f64>,
    pub mean_pair_cost: f64,
    pub sample_standard_deviation: f64,
    pub one_sided_95_pair_cost: f64,
}

/// Typed planning record embedded in a finalized frozen plan.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MainPlanningRecord {
    pub schema_version: u32,
    pub blinded_pilot_hash: String,
    pub pilot_evidence: PilotEvidenceDigest,
    pub blinded_pilot_summary: BlindedPilotSummary,
    pub planner_config: PlannerConfig,
    pub planner_config_hash: String,
    pub planner_version: String,
    pub simulation_seed: String,
    pub sample_plan: SamplePlan,
    pub recommended_pairs_per_archetype: u32,
    pub selected_main_pair_ids: Vec<String>,
    pub frozen_reserve_inputs: Option<FrozenReserveInputs>,
    pub conservative_catalog_pair_reserve: f64,
    pub planning_report_hash: String,
    pub planning_hash: String,
}

impl MainPlanningRecord {
    /// Freezes a recommended sample and its selected deterministic pair IDs.
    pub fn new(
        blinded_pilot_summary: BlindedPilotSummary,
        pilot_evidence: PilotEvidenceDigest,
        planner_config: PlannerConfig,
        sample_plan: SamplePlan,
        selected_main_pair_ids: Vec<String>,
        frozen_reserve_inputs: Option<FrozenReserveInputs>,
        planning_report_hash: String,
        simulation_seed: String,
    ) -> Result<Self, ScheduleError> {
        let recommended_pairs_per_archetype = sample_plan.pairs_per_archetype.ok_or_else(|| {
            ScheduleError("cannot freeze an inconclusive main recommendation".into())
        })?;
        let blinded_pilot_hash =
            hash_serialized(b"blinded-pilot-summary-v1", &blinded_pilot_summary)?;
        let planner_config_hash = hash_serialized(b"planner-config-v1", &planner_config)?;
        let planner_version = planner_config.planner_version.clone();
        let mut record = Self {
            schema_version: 1,
            blinded_pilot_hash,
            conservative_catalog_pair_reserve: pilot_evidence
                .runtime_context
                .conservative_catalog_pair_reserve,
            pilot_evidence,
            blinded_pilot_summary,
            planner_config,
            planner_config_hash,
            planner_version,
            simulation_seed,
            sample_plan,
            recommended_pairs_per_archetype,
            selected_main_pair_ids,
            frozen_reserve_inputs,
            planning_report_hash,
            planning_hash: String::new(),
        };
        let mut unhashed = record.clone();
        unhashed.planning_hash.clear();
        record.planning_hash = hash_serialized(b"main-planning-record-v1", &unhashed)?;
        record.validate()?;
        Ok(record)
    }

    /// Verifies all content hashes in the frozen planning record.
    pub fn validate(&self) -> Result<(), ScheduleError> {
        validate_planner(
            &self.planner_config,
            &self.blinded_pilot_summary.planner_input,
        )
        .map_err(|error| ScheduleError(error.to_string()))?;
        let recomputed = plan_sample(
            &self.planner_config,
            &self.blinded_pilot_summary.planner_input,
            &self.simulation_seed,
        )
        .map_err(|error| ScheduleError(error.to_string()))?;
        let summary = &self.blinded_pilot_summary;
        let pooled_task = pooled_counts(&summary.planner_input, |stratum| stratum.task_failure);
        let pooled_patch = pooled_counts(&summary.planner_input, |stratum| stratum.patch_failure);
        let pooled_bypass = pooled_counts(&summary.planner_input, |stratum| stratum.edit_bypass);
        if self.schema_version != 1
            || self.planner_version != self.planner_config.planner_version
            || self.planner_config_hash
                != hash_serialized(b"planner-config-v1", &self.planner_config)?
            || self.blinded_pilot_hash
                != hash_serialized(b"blinded-pilot-summary-v1", &self.blinded_pilot_summary)?
            || u64::from(summary.pair_count) != pooled_task.pairs()
            || summary.task_failure != pooled_task
            || summary.sessions_with_patch_failure != pooled_patch
            || summary.edit_bypass != pooled_bypass
            || self.sample_plan.conclusion != PlanningConclusion::Recommended
            || self.sample_plan != recomputed
            || self.sample_plan.pairs_per_archetype != Some(self.recommended_pairs_per_archetype)
            || self.sample_plan.target_power != self.planner_config.target_power
            || self.sample_plan.practical_cap != self.planner_config.maximum_pairs_per_archetype
            || self.recommended_pairs_per_archetype == 0
            || self.recommended_pairs_per_archetype
                > self.planner_config.maximum_pairs_per_archetype
            || self.selected_main_pair_ids.is_empty()
            || self.selected_main_pair_ids.len()
                != usize::try_from(self.recommended_pairs_per_archetype)
                    .unwrap()
                    .saturating_mul(summary.planner_input.strata.len())
            || self.planning_report_hash.is_empty()
            || self.simulation_seed.is_empty()
            || self.pilot_evidence.unplanned_schedule_hash.is_empty()
            || self.pilot_evidence.completion_stream_hash.is_empty()
            || self.conservative_catalog_pair_reserve
                != self
                    .pilot_evidence
                    .runtime_context
                    .conservative_catalog_pair_reserve
            || !self.conservative_catalog_pair_reserve.is_finite()
            || self.conservative_catalog_pair_reserve <= 0.0
            || self
                .sample_plan
                .endpoint_requirements
                .iter()
                .any(|endpoint| {
                    endpoint.required_pairs_per_archetype.is_none()
                        || endpoint.achieved_power_lower_bound < self.sample_plan.target_power
                })
        {
            return Err(ScheduleError("invalid frozen main planning record".into()));
        }
        let mut unhashed = self.clone();
        unhashed.planning_hash.clear();
        let expected = hash_serialized(b"main-planning-record-v1", &unhashed)?;
        if self.planning_hash != expected {
            return Err(ScheduleError("main planning hash mismatch".into()));
        }
        Ok(())
    }
}

/// Serializable output of the blinded planner command.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PlanningReport {
    pub schema_version: u32,
    pub run_id: String,
    pub unplanned_schedule_hash: String,
    pub blinded_pilot_hash: String,
    pub pilot_evidence: PilotEvidenceDigest,
    pub blinded_pilot_summary: BlindedPilotSummary,
    pub planner_config: PlannerConfig,
    pub planner_config_hash: String,
    pub planner_version: String,
    pub simulation_seed: String,
    pub sample_plan: SamplePlan,
    pub selected_main_pair_ids: Vec<String>,
    pub frozen_reserve_inputs: Option<FrozenReserveInputs>,
    pub conservative_catalog_pair_reserve: f64,
    pub report_hash: String,
}

/// A report is always returned. The plan is absent when the universe is insufficient.
pub struct PlanningOutcome {
    pub planned_plan: Option<FrozenPlan>,
    pub report: PlanningReport,
}

/// Reduces the complete pilot and plans the main schedule with production controls.
pub fn plan_main(plan: &FrozenPlan, records: &Path) -> Result<PlanningOutcome, PlanningError> {
    let mut config = PlannerConfig::default();
    config.maximum_pairs_per_archetype = config
        .maximum_pairs_per_archetype
        .min(plan.universe.instances_per_archetype.saturating_sub(4));
    plan_main_with_config(plan, records, config)
}

/// Reduces the complete pilot and plans with explicit deterministic controls.
pub fn plan_main_with_config(
    plan: &FrozenPlan,
    records: &Path,
    config: PlannerConfig,
) -> Result<PlanningOutcome, PlanningError> {
    if !matches!(plan.planning, MainPlanning::Unplanned) || !plan.schedule.main.is_empty() {
        return Err(PlanningError(
            "plan-main requires an unplanned frozen plan".into(),
        ));
    }
    validate_frozen_plan(plan)?;
    let (summary, reserve, evidence) = reduce_pilot(plan, records)?;
    let blinded_pilot_hash = hash_serialized(b"blinded-pilot-summary-v1", &summary)?;
    let planner_config_hash = hash_serialized(b"planner-config-v1", &config)?;
    let planner_seed = hash_framed(
        b"sample-planner-seed-v1",
        &[
            plan.universe.run_seed.as_bytes(),
            blinded_pilot_hash.as_bytes(),
            planner_config_hash.as_bytes(),
        ],
    );
    let sample_plan = plan_sample(&config, &summary.planner_input, &planner_seed)
        .map_err(|error| PlanningError(error.to_string()))?;
    let selected_main_pair_ids = match sample_plan.pairs_per_archetype {
        Some(repetitions) => planned_main_pair_ids(plan, repetitions)?,
        None => Vec::new(),
    };
    let mut report = PlanningReport {
        schema_version: 1,
        run_id: plan.schedule.run_id.clone(),
        unplanned_schedule_hash: plan.schedule.schedule_hash.clone(),
        blinded_pilot_hash: blinded_pilot_hash.clone(),
        pilot_evidence: evidence.clone(),
        blinded_pilot_summary: summary.clone(),
        planner_config: config.clone(),
        planner_config_hash: planner_config_hash.clone(),
        planner_version: config.planner_version.clone(),
        simulation_seed: planner_seed.clone(),
        sample_plan: sample_plan.clone(),
        selected_main_pair_ids: selected_main_pair_ids.clone(),
        frozen_reserve_inputs: reserve.clone(),
        conservative_catalog_pair_reserve: evidence
            .runtime_context
            .conservative_catalog_pair_reserve,
        report_hash: String::new(),
    };
    report.report_hash = report_hash(&report)?;
    let planned_plan =
        if let Some(recommended_pairs_per_archetype) = sample_plan.pairs_per_archetype {
            debug_assert_eq!(
                sample_plan.pairs_per_archetype,
                Some(recommended_pairs_per_archetype)
            );
            let record = MainPlanningRecord::new(
                summary,
                evidence,
                config,
                sample_plan,
                selected_main_pair_ids,
                reserve,
                report.report_hash.clone(),
                planner_seed,
            )?;
            Some(finalize_main_schedule(plan, record)?)
        } else {
            None
        };
    Ok(PlanningOutcome {
        planned_plan,
        report,
    })
}

/// Recomputes the exact excluded stream committed by a planned main record.
pub fn validate_pilot_evidence(plan: &FrozenPlan, records: &Path) -> Result<(), PlanningError> {
    let planning = plan.require_planned_main()?;
    let mut unplanned = plan.clone();
    unplanned.schedule.main.clear();
    unplanned.schedule.planning_hash = None;
    unplanned.schedule.schedule_hash = planning.pilot_evidence.unplanned_schedule_hash.clone();
    unplanned.planning = MainPlanning::Unplanned;
    let (summary, reserve, evidence) = reduce_pilot(&unplanned, records)?;
    if summary != planning.blinded_pilot_summary
        || reserve != planning.frozen_reserve_inputs
        || evidence != planning.pilot_evidence
    {
        return Err(PlanningError(
            "record stream does not match the pilot evidence committed by planning".into(),
        ));
    }
    Ok(())
}

fn reduce_pilot(
    plan: &FrozenPlan,
    records: &Path,
) -> Result<
    (
        BlindedPilotSummary,
        Option<FrozenReserveInputs>,
        PilotEvidenceDigest,
    ),
    PlanningError,
> {
    let state = scan(records).map_err(|error| PlanningError(error.to_string()))?;
    let expected = plan
        .schedule
        .smoke
        .iter()
        .chain(&plan.schedule.pilot)
        .map(|pair| (pair.pair_id.as_str(), pair))
        .collect::<BTreeMap<_, _>>();
    for marker in state.completion_markers.values().filter(|marker| {
        matches!(
            marker.identity.phase,
            SchedulePhase::Smoke | SchedulePhase::Pilot
        )
    }) {
        if marker.identity.run_id != plan.schedule.run_id
            || marker.identity.schedule_hash != plan.schedule.schedule_hash
            || !expected.contains_key(marker.identity.pair_id.as_str())
        {
            return Err(PlanningError(
                "records contain an extra or mixed-schedule smoke/pilot completion".into(),
            ));
        }
    }
    let mut strata = plan
        .manifest
        .archetypes
        .iter()
        .map(|archetype| {
            (
                archetype.id.clone(),
                BlindedPlannerStratum {
                    archetype_id: archetype.id.clone(),
                    task_failure: PairedEventCounts::default(),
                    patch_failure: PairedEventCounts::default(),
                    edit_bypass: PairedEventCounts::default(),
                    efficiency: Vec::new(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut pair_costs = Vec::new();
    let mut runtime_context = None;
    let mut marker_hashes = Vec::new();
    let mut trial_hashes = Vec::new();
    for pair in plan.schedule.smoke.iter().chain(&plan.schedule.pilot) {
        let marker = state
            .completion_markers
            .get(&PairKey {
                run_id: pair.run_id.clone(),
                pair_id: pair.pair_id.clone(),
            })
            .ok_or_else(|| PlanningError(format!("missing complete pair {}", pair.pair_id)))?;
        validate_marker_identity(plan, pair, marker)?;
        let first = state
            .trials_by_hash
            .get(&marker.trial_record_hashes[0])
            .ok_or_else(|| PlanningError("completion marker lost its first trial".into()))?;
        let second = state
            .trials_by_hash
            .get(&marker.trial_record_hashes[1])
            .ok_or_else(|| PlanningError("completion marker lost its second trial".into()))?;
        validate_scheduled_trials(pair, first, second)?;
        let first_metrics = runtime_metrics(first)?;
        let second_metrics = runtime_metrics(second)?;
        if first_metrics.normalized_first_request_hash.is_none()
            || first_metrics.normalized_first_request_hash
                != second_metrics.normalized_first_request_hash
        {
            return Err(PlanningError(
                "excluded pair has unequal actual first-request payload hashes".into(),
            ));
        }
        for (trial, metrics) in [(first, &first_metrics), (second, &second_metrics)] {
            let context = runtime_context_for(trial, metrics);
            if runtime_context
                .as_ref()
                .is_some_and(|expected| expected != &context)
            {
                return Err(PlanningError(
                    "excluded trials mix image, source, date, limits, model, or controls".into(),
                ));
            }
            runtime_context.get_or_insert(context);
        }
        marker_hashes.push(marker.record_hash.clone());
        trial_hashes.extend(marker.trial_record_hashes.iter().cloned());
        if pair.phase == SchedulePhase::Pilot {
            let (blinded_first, blinded_second) =
                if blinded_swap(&plan.universe.run_seed, &pair.pair_id) {
                    (&second_metrics, &first_metrics)
                } else {
                    (&first_metrics, &second_metrics)
                };
            let stratum = strata
                .get_mut(&pair.archetype_id)
                .ok_or_else(|| PlanningError("pilot has an unknown archetype".into()))?;
            add_event(
                &mut stratum.task_failure,
                !blinded_first.task_passed,
                !blinded_second.task_passed,
            );
            add_event(
                &mut stratum.patch_failure,
                blinded_first.sessions_with_patch_failure,
                blinded_second.sessions_with_patch_failure,
            );
            add_event(
                &mut stratum.edit_bypass,
                blinded_first.edit_bypass,
                blinded_second.edit_bypass,
            );
            stratum.efficiency.push(BlindedEfficiencyPair {
                first_cost: blinded_first.aj_recorded_catalog_cost,
                second_cost: blinded_second.aj_recorded_catalog_cost,
                first_responses: blinded_first.model_responses,
                second_responses: blinded_second.model_responses,
            });
            pair_costs.push(
                first_metrics.aj_recorded_catalog_cost + second_metrics.aj_recorded_catalog_cost,
            );
        }
    }
    let planner_input = BlindedPlannerInput {
        strata: strata.into_values().collect(),
    };
    let summary = BlindedPilotSummary {
        pair_count: u32::try_from(plan.schedule.pilot.len()).unwrap(),
        task_failure: pooled_counts(&planner_input, |stratum| stratum.task_failure),
        sessions_with_patch_failure: pooled_counts(&planner_input, |stratum| stratum.patch_failure),
        edit_bypass: pooled_counts(&planner_input, |stratum| stratum.edit_bypass),
        planner_input,
    };
    marker_hashes.sort();
    trial_hashes.sort();
    let stream_material = serde_json::to_vec(&(&marker_hashes, &trial_hashes))
        .map_err(|error| PlanningError(format!("cannot hash pilot evidence: {error}")))?;
    let evidence = PilotEvidenceDigest {
        unplanned_schedule_hash: plan.schedule.schedule_hash.clone(),
        completion_stream_hash: hash_framed(
            b"apply-patch-pilot-completion-stream-v1",
            &[&stream_material],
        ),
        completion_marker_hashes: marker_hashes,
        trial_record_hashes: trial_hashes,
        runtime_context: runtime_context
            .ok_or_else(|| PlanningError("pilot evidence has no runtime context".into()))?,
    };
    Ok((summary, reserve_inputs(pair_costs), evidence))
}

fn runtime_context_for(
    trial: &TrialRecord,
    runtime: &PilotRuntimeMetrics,
) -> FrozenPilotRuntimeContext {
    FrozenPilotRuntimeContext {
        image_id: runtime.image_id.clone(),
        source_provenance: runtime.source_provenance.clone(),
        utc_date: runtime.utc_date.clone(),
        limits: runtime.limits.clone(),
        system_prompt_hash: runtime.system_prompt_hash.clone(),
        aj_revision: trial.metadata.aj_revision.clone(),
        model_catalog_hash: trial.metadata.model_catalog_hash.clone(),
        provider: trial.metadata.provider.clone(),
        model: trial.metadata.model.clone(),
        reasoning_effort: trial.metadata.reasoning_effort.clone(),
        conservative_catalog_pair_reserve: runtime.conservative_catalog_pair_reserve,
    }
}

fn validate_marker_identity(
    plan: &FrozenPlan,
    pair: &PairScheduleRecord,
    marker: &crate::artifacts::PairCompleteRecord,
) -> Result<(), PlanningError> {
    if marker.identity.run_id != pair.run_id
        || marker.identity.pair_id != pair.pair_id
        || marker.identity.task_id != pair.task_id
        || marker.identity.instance_hash != pair.instance_hash
        || marker.identity.phase != pair.phase
        || marker.identity.schedule_hash != plan.schedule.schedule_hash
    {
        return Err(PlanningError(
            "completion marker does not match the frozen smoke/pilot schedule".into(),
        ));
    }
    Ok(())
}

fn validate_scheduled_trials(
    pair: &PairScheduleRecord,
    first: &TrialRecord,
    second: &TrialRecord,
) -> Result<(), PlanningError> {
    for (trial, scheduled) in [first, second].into_iter().zip(&pair.trials) {
        if trial.identity.run_id != scheduled.run_id
            || trial.identity.pair_id != scheduled.pair_id
            || trial.identity.task_id != scheduled.task_id
            || trial.identity.instance_hash != scheduled.instance_hash
            || trial.identity.archetype_id != pair.archetype_id
            || trial.identity.phase != scheduled.phase
            || trial.identity.repetition != scheduled.archetype_repetition
            || trial.identity.variant != scheduled.variant
            || trial.identity.order_index != scheduled.order_index
        {
            return Err(PlanningError(
                "durable trial does not match its frozen schedule identity".into(),
            ));
        }
    }
    Ok(())
}

fn runtime_metrics(trial: &TrialRecord) -> Result<PilotRuntimeMetrics, PlanningError> {
    let metrics: PilotRuntimeMetrics = serde_json::from_value(trial.runtime.clone())
        .map_err(|error| PlanningError(format!("invalid pilot runtime projection: {error}")))?;
    if !metrics.valid
        || !metrics.aj_recorded_catalog_cost.is_finite()
        || metrics.aj_recorded_catalog_cost <= 0.0
        || metrics.model_responses == 0
    {
        return Err(PlanningError(
            "pilot contains invalid required metrics".into(),
        ));
    }
    Ok(metrics)
}

fn blinded_swap(run_seed: &str, pair_id: &str) -> bool {
    let hash = hash_framed(
        b"pilot-treatment-orientation-v1",
        &[run_seed.as_bytes(), pair_id.as_bytes()],
    );
    u8::from_str_radix(&hash[..2], 16).expect("SHA-256 hex prefix is valid") & 1 == 1
}

fn add_event(counts: &mut PairedEventCounts, first: bool, second: bool) {
    match (first, second) {
        (false, false) => counts.neither += 1,
        (true, false) => counts.first_only += 1,
        (false, true) => counts.second_only += 1,
        (true, true) => counts.both += 1,
    }
}

fn pooled_counts(
    input: &BlindedPlannerInput,
    field: impl Fn(&BlindedPlannerStratum) -> PairedEventCounts,
) -> PairedEventCounts {
    input
        .strata
        .iter()
        .map(field)
        .fold(PairedEventCounts::default(), |mut total, counts| {
            total.neither += counts.neither;
            total.first_only += counts.first_only;
            total.second_only += counts.second_only;
            total.both += counts.both;
            total
        })
}

fn reserve_inputs(costs: Vec<f64>) -> Option<FrozenReserveInputs> {
    if costs.len() < 2 {
        return None;
    }
    let count = usize_as_f64(costs.len());
    let mean = costs.iter().sum::<f64>() / count;
    let variance = costs.iter().map(|cost| (cost - mean).powi(2)).sum::<f64>() / (count - 1.0);
    let standard_deviation = variance.sqrt();
    Some(FrozenReserveInputs {
        blinded_pair_costs: costs,
        mean_pair_cost: mean,
        sample_standard_deviation: standard_deviation,
        one_sided_95_pair_cost: mean + ONE_SIDED_95_Z * standard_deviation / count.sqrt(),
    })
}

fn report_hash(report: &PlanningReport) -> Result<String, PlanningError> {
    let mut unhashed = report.clone();
    unhashed.report_hash.clear();
    hash_serialized(b"main-planning-report-v1", &unhashed).map_err(Into::into)
}

fn hash_serialized(domain: &[u8], value: &impl Serialize) -> Result<String, ScheduleError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ScheduleError(format!("cannot hash planning data: {error}")))?;
    Ok(hash_framed(domain, &[&bytes]))
}

#[allow(clippy::as_conversions)]
fn usize_as_f64(value: usize) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::artifacts::{
        ArtifactLog, PairCompletionIdentity, RecordedDescription, TrialIdentity, TrialMetadata,
        TrialRecord,
    };
    use crate::descriptions::{DescriptionVariant, load};
    use crate::schedule::{PairScheduleRecord, freeze_plan};
    use crate::suite::committed_manifest;

    fn metadata(plan: &FrozenPlan) -> TrialMetadata {
        let recorded = |variant| {
            let value = load(variant);
            RecordedDescription {
                sha256: value.sha256,
                byte_length: value.byte_length,
            }
        };
        TrialMetadata {
            task_seed: "seed".into(),
            current_description: recorded(DescriptionVariant::Current),
            compact_description: recorded(DescriptionVariant::CompactV1),
            aj_revision: "head".into(),
            suite_revision: plan.universe.suite_revision.clone(),
            model_catalog_hash: "catalog".into(),
            provider: "openai-codex".into(),
            model: "gpt-5.6-sol".into(),
            reasoning_effort: "low".into(),
            fixture_revision: "fixture".into(),
        }
    }

    fn runtime(valid: bool, cost: f64) -> serde_json::Value {
        json!({
            "valid": valid,
            "task_passed": true,
            "sessions_with_patch_failure": false,
            "edit_bypass": false,
            "aj_recorded_catalog_cost": cost,
            "model_responses": 2,
            "image_id": "sha256:image",
            "source_provenance": {"head":"head","dirty":false,"worktree_hash":null},
            "utc_date": "2026-07-24",
            "limits": {
                "wall_timeout_seconds": 600,
                "max_provider_requests": 12,
                "max_model_responses": 12,
                "provider_output_token_ceiling": 128000,
                "aggregate_observed_output_token_ceiling": 1536000
            },
            "system_prompt_hash": "system",
            "conservative_catalog_pair_reserve": 100.0,
            "normalized_first_request_hash": "payload"
        })
    }

    fn append_pair(log: &mut ArtifactLog, plan: &FrozenPlan, pair: &PairScheduleRecord) {
        let attempt = format!("attempt-{}", pair.pair_id);
        let records = pair.trials.each_ref().map(|scheduled| {
            TrialRecord::new(
                TrialIdentity {
                    run_id: pair.run_id.clone(),
                    pair_id: pair.pair_id.clone(),
                    attempt_id: attempt.clone(),
                    task_id: pair.task_id.clone(),
                    instance_hash: pair.instance_hash.clone(),
                    archetype_id: pair.archetype_id.clone(),
                    schedule_hash: plan.schedule.schedule_hash.clone(),
                    phase: pair.phase,
                    repetition: scheduled.archetype_repetition,
                    variant: scheduled.variant,
                    order_index: scheduled.order_index,
                },
                metadata(plan),
                runtime(true, 1.0 + f64::from(scheduled.order_index)),
            )
            .unwrap()
        });
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
    fn reducer_uses_only_marker_referenced_attempts_and_binds_exact_stream() {
        let manifest = committed_manifest().unwrap();
        let plan = freeze_plan(&manifest, "planning-evidence", 6).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let mut log = ArtifactLog::open(&path).unwrap();
        let pair = &plan.schedule.pilot[0];
        let abandoned = TrialRecord::new(
            TrialIdentity {
                run_id: pair.run_id.clone(),
                pair_id: pair.pair_id.clone(),
                attempt_id: "abandoned".into(),
                task_id: pair.task_id.clone(),
                instance_hash: pair.instance_hash.clone(),
                archetype_id: pair.archetype_id.clone(),
                schedule_hash: plan.schedule.schedule_hash.clone(),
                phase: pair.phase,
                repetition: pair.archetype_repetition,
                variant: pair.trials[0].variant,
                order_index: 0,
            },
            metadata(&plan),
            runtime(false, 0.0),
        )
        .unwrap();
        log.append_trial(&abandoned).unwrap();
        for pair in plan.schedule.smoke.iter().chain(&plan.schedule.pilot) {
            append_pair(&mut log, &plan, pair);
        }
        drop(log);

        let outcome = plan_main_with_config(
            &plan,
            &path,
            PlannerConfig {
                simulation_replicates: 100,
                maximum_pairs_per_archetype: 1,
                ..PlannerConfig::default()
            },
        )
        .unwrap();
        assert!(outcome.planned_plan.is_none());
        assert_eq!(outcome.report.pilot_evidence.trial_record_hashes.len(), 128);
        assert!(
            !outcome
                .report
                .pilot_evidence
                .trial_record_hashes
                .contains(&abandoned.record_hash)
        );
        assert_eq!(
            outcome.report.pilot_evidence.unplanned_schedule_hash,
            plan.schedule.schedule_hash
        );
    }

    #[test]
    fn planning_record_rejects_a_claimed_one_pair_fake_plan() {
        let pilot = BlindedPlannerInput {
            strata: (0..16)
                .map(|index| BlindedPlannerStratum {
                    archetype_id: format!("a{index}"),
                    task_failure: PairedEventCounts {
                        neither: 3,
                        ..PairedEventCounts::default()
                    },
                    patch_failure: PairedEventCounts {
                        neither: 3,
                        ..PairedEventCounts::default()
                    },
                    edit_bypass: PairedEventCounts {
                        neither: 3,
                        ..PairedEventCounts::default()
                    },
                    efficiency: vec![
                        BlindedEfficiencyPair {
                            first_cost: 1.0,
                            second_cost: 1.0,
                            first_responses: 2,
                            second_responses: 2,
                        };
                        3
                    ],
                })
                .collect(),
        };
        let summary = BlindedPilotSummary {
            pair_count: 48,
            task_failure: PairedEventCounts {
                neither: 48,
                ..PairedEventCounts::default()
            },
            sessions_with_patch_failure: PairedEventCounts {
                neither: 48,
                ..PairedEventCounts::default()
            },
            edit_bypass: PairedEventCounts {
                neither: 48,
                ..PairedEventCounts::default()
            },
            planner_input: pilot,
        };
        let config = PlannerConfig {
            simulation_replicates: 100,
            maximum_pairs_per_archetype: 508,
            ..PlannerConfig::default()
        };
        let fake = SamplePlan {
            conclusion: PlanningConclusion::Recommended,
            pairs_per_archetype: Some(1),
            limiting_endpoint: Some("fake".into()),
            endpoint_requirements: Vec::new(),
            target_power: 0.8,
            practical_cap: 508,
        };
        let evidence = PilotEvidenceDigest {
            unplanned_schedule_hash: "schedule".into(),
            completion_stream_hash: "stream".into(),
            completion_marker_hashes: vec!["marker".into()],
            trial_record_hashes: vec!["trial".into()],
            runtime_context: FrozenPilotRuntimeContext {
                image_id: "image".into(),
                source_provenance: SourceProvenance {
                    head: "head".into(),
                    dirty: false,
                    worktree_hash: None,
                },
                utc_date: "2026-07-24".into(),
                limits: RuntimeLimits {
                    wall_timeout_seconds: 1,
                    max_provider_requests: 1,
                    max_model_responses: 1,
                    provider_output_token_ceiling: 1,
                    aggregate_observed_output_token_ceiling: 1,
                },
                system_prompt_hash: "system".into(),
                aj_revision: "head".into(),
                model_catalog_hash: "catalog".into(),
                provider: "provider".into(),
                model: "model".into(),
                reasoning_effort: "low".into(),
                conservative_catalog_pair_reserve: 1.0,
            },
        };
        assert!(
            MainPlanningRecord::new(
                summary,
                evidence,
                config,
                fake,
                vec!["pair".into(); 16],
                None,
                "report".into(),
                "seed".into(),
            )
            .is_err()
        );
    }
}
