//! Analysis of complete durable pairs with typed phase-2 runtime metrics.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::artifacts::{TrialRecord, scan};
use crate::descriptions::DescriptionVariant;
use crate::schedule::SchedulePhase;
use crate::statistics::{
    BinaryPair, BinaryStratum, BootstrapConfig, BootstrapSummary, EfficiencyPair,
    EfficiencyStratum, RiskDifferenceBounds, paired_relative_change_bootstrap,
    paired_risk_difference_bounds,
};
use crate::suite::committed_manifest;

/// Error raised when durable records cannot support the declared analysis.
#[derive(Debug)]
pub struct AnalysisError(pub String);

impl fmt::Display for AnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AnalysisError {}

/// Required phase-2 runtime metrics consumed by the foundational analyzer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RuntimeMetrics {
    pub task_passed: bool,
    pub sessions_with_patch_failure: bool,
    pub edit_bypass: bool,
    pub aj_recorded_catalog_cost: f64,
    pub model_responses: u64,
}

/// One binary endpoint's weighted paired result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BinaryEndpointSummary {
    pub endpoint: String,
    pub bounds: RiskDifferenceBounds,
}

/// One efficiency endpoint's weighted paired result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EfficiencyEndpointSummary {
    pub endpoint: String,
    pub current_mean: f64,
    pub compact_mean: f64,
    pub bootstrap: BootstrapSummary,
}

/// Available confirmatory summaries. Decision logic remains a later live phase.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AnalysisReport {
    pub complete_pairs: usize,
    pub binary: Vec<BinaryEndpointSummary>,
    pub efficiency: Vec<EfficiencyEndpointSummary>,
    pub statistical_contract: String,
    pub cost_limitation: String,
}

/// Analyzes all complete markers in a durable record stream.
pub fn analyze_records(path: &Path) -> Result<AnalysisReport, AnalysisError> {
    let state = scan(path).map_err(|error| AnalysisError(error.to_string()))?;
    if state.completion_markers.is_empty() {
        return Err(AnalysisError("records contain no complete pairs".into()));
    }
    let manifest = committed_manifest().map_err(|error| AnalysisError(error.to_string()))?;
    let mut pairs_by_archetype: BTreeMap<String, Vec<(RuntimeMetrics, RuntimeMetrics)>> =
        BTreeMap::new();
    let mut complete_pairs = 0;
    let mut analysis_identity: Option<(&str, &str, &TrialRecord)> = None;
    for marker in state.completion_markers.values() {
        if marker.identity.phase != SchedulePhase::Main {
            continue;
        }
        complete_pairs += 1;
        let first = state
            .trials_by_hash
            .get(&marker.trial_record_hashes[0])
            .ok_or_else(|| AnalysisError("verified marker lost its first trial".into()))?;
        let second = state
            .trials_by_hash
            .get(&marker.trial_record_hashes[1])
            .ok_or_else(|| AnalysisError("verified marker lost its second trial".into()))?;
        if let Some((run_id, schedule_hash, baseline)) = analysis_identity {
            if marker.identity.run_id != run_id
                || marker.identity.schedule_hash != schedule_hash
                || !same_analysis_context(first, baseline)
            {
                return Err(AnalysisError(
                    "complete main pairs mix run, schedule, model, or frozen-description identities"
                        .into(),
                ));
            }
        } else {
            analysis_identity = Some((
                &marker.identity.run_id,
                &marker.identity.schedule_hash,
                first,
            ));
        }
        let (current, compact) = variants(first, second)?;
        let current_metrics = metrics(current)?;
        let compact_metrics = metrics(compact)?;
        pairs_by_archetype
            .entry(current.identity.archetype_id.clone())
            .or_default()
            .push((current_metrics, compact_metrics));
    }

    if complete_pairs == 0 {
        return Err(AnalysisError(
            "records contain no complete main pairs".into(),
        ));
    }

    for archetype in &manifest.archetypes {
        if !pairs_by_archetype.contains_key(&archetype.id) {
            return Err(AnalysisError(format!(
                "required archetype {} has no complete pair",
                archetype.id
            )));
        }
    }
    if let Some(unexpected) = pairs_by_archetype.keys().find(|id| {
        !manifest
            .archetypes
            .iter()
            .any(|archetype| &archetype.id == *id)
    }) {
        return Err(AnalysisError(format!(
            "records contain unknown archetype {unexpected}"
        )));
    }

    let binary_fields: [(&str, fn(&RuntimeMetrics) -> bool); 3] = [
        ("task_passed", runtime_task_passed),
        ("sessions_with_patch_failure", runtime_patch_failure),
        ("edit_bypass", runtime_edit_bypass),
    ];
    let binary = binary_fields
        .into_iter()
        .map(|(name, field)| {
            let strata = manifest
                .archetypes
                .iter()
                .map(|archetype| BinaryStratum {
                    archetype_id: archetype.id.clone(),
                    weight: manifest_weight(archetype),
                    pairs: pairs_by_archetype[&archetype.id]
                        .iter()
                        .map(|(current, compact)| BinaryPair {
                            current: field(current),
                            compact: field(compact),
                        })
                        .collect(),
                })
                .collect::<Vec<_>>();
            paired_risk_difference_bounds(&strata, 0.05)
                .map(|bounds| BinaryEndpointSummary {
                    endpoint: name.into(),
                    bounds,
                })
                .map_err(|error| AnalysisError(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let bootstrap_config = BootstrapConfig::default();
    let efficiency_fields: [(&str, fn(&RuntimeMetrics) -> f64); 2] = [
        ("aj_recorded_catalog_cost", runtime_cost),
        ("model_responses", runtime_responses),
    ];
    let efficiency = efficiency_fields
        .into_iter()
        .map(|(name, field)| {
            let strata = manifest
                .archetypes
                .iter()
                .map(|archetype| EfficiencyStratum {
                    archetype_id: archetype.id.clone(),
                    weight: manifest_weight(archetype),
                    pairs: pairs_by_archetype[&archetype.id]
                        .iter()
                        .map(|(current, compact)| EfficiencyPair {
                            current: field(current),
                            compact: field(compact),
                        })
                        .collect(),
                })
                .collect::<Vec<_>>();
            let current_mean = weighted_mean(&strata, |pair| pair.current);
            let compact_mean = weighted_mean(&strata, |pair| pair.compact);
            paired_relative_change_bootstrap(&strata, &bootstrap_config)
                .map(|bootstrap| EfficiencyEndpointSummary {
                    endpoint: name.into(),
                    current_mean,
                    compact_mean,
                    bootstrap,
                })
                .map_err(|error| AnalysisError(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(AnalysisReport {
        complete_pairs,
        binary,
        efficiency,
        statistical_contract: "One-sided 95% fixed-archetype paired Hoeffding bounds. These distribution-free bounds are conservative and retain uncertainty at all-zero and all-pass outcomes.".into(),
        cost_limitation: "AJ-recorded catalog cost is not billed cost. Missing provider cache-write usage is recorded as zero by current AJ accounting.".into(),
    })
}

fn same_analysis_context(left: &TrialRecord, right: &TrialRecord) -> bool {
    left.metadata.current_description == right.metadata.current_description
        && left.metadata.compact_description == right.metadata.compact_description
        && left.metadata.aj_revision == right.metadata.aj_revision
        && left.metadata.suite_revision == right.metadata.suite_revision
        && left.metadata.model_catalog_hash == right.metadata.model_catalog_hash
        && left.metadata.provider == right.metadata.provider
        && left.metadata.model == right.metadata.model
        && left.metadata.reasoning_effort == right.metadata.reasoning_effort
}

fn manifest_weight(archetype: &crate::suite::ArchetypeManifest) -> f64 {
    f64::from(archetype.weight.numerator) / f64::from(archetype.weight.denominator)
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
    serde_json::from_value(trial.runtime.clone()).map_err(|error| {
        AnalysisError(format!(
            "trial {} is missing required runtime metrics: {error}",
            trial.record_hash
        ))
    })
}

fn weighted_mean(strata: &[EfficiencyStratum], field: impl Fn(&EfficiencyPair) -> f64) -> f64 {
    strata
        .iter()
        .map(|stratum| {
            stratum.weight * stratum.pairs.iter().map(&field).sum::<f64>()
                / usize_as_f64(stratum.pairs.len())
        })
        .sum()
}

fn runtime_task_passed(runtime: &RuntimeMetrics) -> bool {
    runtime.task_passed
}

fn runtime_patch_failure(runtime: &RuntimeMetrics) -> bool {
    runtime.sessions_with_patch_failure
}

fn runtime_edit_bypass(runtime: &RuntimeMetrics) -> bool {
    runtime.edit_bypass
}

fn runtime_cost(runtime: &RuntimeMetrics) -> f64 {
    runtime.aj_recorded_catalog_cost
}

fn runtime_responses(runtime: &RuntimeMetrics) -> f64 {
    u64_as_f64(runtime.model_responses)
}

#[allow(clippy::as_conversions)]
fn usize_as_f64(value: usize) -> f64 {
    value as f64
}

#[allow(clippy::as_conversions)]
fn u64_as_f64(value: u64) -> f64 {
    value as f64
}

/// Renders a compact Markdown report.
pub fn render_markdown(report: &AnalysisReport) -> String {
    let mut output = format!(
        "# Apply patch description evaluation\n\nComplete pairs: {}\n\n",
        report.complete_pairs
    );
    output.push_str("## Binary guardrails\n\n| Endpoint | Effect | Lower | Upper |\n| --- | ---: | ---: | ---: |\n");
    for endpoint in &report.binary {
        output.push_str(&format!(
            "| {} | {:.4} | {:.4} | {:.4} |\n",
            endpoint.endpoint,
            endpoint.bounds.estimate,
            endpoint.bounds.lower,
            endpoint.bounds.upper
        ));
    }
    output.push_str("\n## Efficiency\n\n| Endpoint | Current mean | Compact mean | Relative change | 95% upper | 97.5% upper |\n| --- | ---: | ---: | ---: | ---: | ---: |\n");
    for endpoint in &report.efficiency {
        output.push_str(&format!(
            "| {} | {:.6} | {:.6} | {:.4} | {:.4} | {:.4} |\n",
            endpoint.endpoint,
            endpoint.current_mean,
            endpoint.compact_mean,
            endpoint.bootstrap.relative_change,
            endpoint.bootstrap.upper_95,
            endpoint.bootstrap.upper_97_5
        ));
    }
    output.push_str(&format!(
        "\n## Contracts and limitations\n\n{}\n\n{}\n",
        report.statistical_contract, report.cost_limitation
    ));
    output
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::artifacts::{
        ArtifactLog, PairCompletionIdentity, RecordedDescription, TrialIdentity, TrialMetadata,
        TrialRecord,
    };
    use crate::suite::committed_manifest;

    #[test]
    fn missing_runtime_fields_fail_clearly() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let archetype = &committed_manifest().unwrap().archetypes[0].id;
        let make_identity = |variant, order_index| TrialIdentity {
            run_id: "run".into(),
            pair_id: "pair".into(),
            attempt_id: "attempt".into(),
            task_id: "task".into(),
            instance_hash: "instance".into(),
            archetype_id: archetype.clone(),
            schedule_hash: "schedule".into(),
            phase: SchedulePhase::Main,
            repetition: 0,
            variant,
            order_index,
        };
        let metadata = || TrialMetadata {
            task_seed: "seed".into(),
            current_description: RecordedDescription {
                sha256: "current".into(),
                byte_length: 100,
            },
            compact_description: RecordedDescription {
                sha256: "compact".into(),
                byte_length: 50,
            },
            aj_revision: "aj".into(),
            suite_revision: "suite".into(),
            model_catalog_hash: "catalog".into(),
            provider: "provider".into(),
            model: "model".into(),
            reasoning_effort: "low".into(),
            fixture_revision: "fixture".into(),
        };
        let first = TrialRecord::new(
            make_identity(DescriptionVariant::Current, 0),
            metadata(),
            json!({}),
        )
        .unwrap();
        let second = TrialRecord::new(
            make_identity(DescriptionVariant::CompactV1, 1),
            metadata(),
            json!({}),
        )
        .unwrap();
        let mut log = ArtifactLog::open(&path).unwrap();
        log.append_trial(&first).unwrap();
        log.append_trial(&second).unwrap();
        log.complete_pair(
            PairCompletionIdentity {
                run_id: "run".into(),
                pair_id: "pair".into(),
                attempt_id: "attempt".into(),
                task_id: "task".into(),
                instance_hash: "instance".into(),
                schedule_hash: "schedule".into(),
                phase: SchedulePhase::Main,
            },
            [first.record_hash, second.record_hash],
        )
        .unwrap();
        let error = analyze_records(&path).unwrap_err().to_string();
        assert!(error.contains("required archetype") || error.contains("required runtime"));
    }
}
