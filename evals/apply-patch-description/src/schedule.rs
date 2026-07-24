//! Frozen task universe and domain-separated paired schedules.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::descriptions::{DescriptionVariant, FrozenDescription, load};
use crate::hash_framed;
use crate::rng::CounterRng;
use crate::suite::{
    ArchetypeManifest, ParameterKind, SuiteManifest, TaskParameters, UncommonTextLane,
    suite_revision,
};

/// Error returned while freezing a universe or schedule.
#[derive(Debug)]
pub struct ScheduleError(pub String);

impl fmt::Display for ScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ScheduleError {}

/// A generated task identity whose fixture can be materialized in phase 2.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskInstance {
    pub task_id: String,
    pub archetype_id: String,
    pub task_seed: String,
    pub universe_index: u32,
    pub parameters: TaskParameters,
    pub suite_revision: String,
    pub instance_hash: String,
}

/// Maximum frozen set from which all evaluation phases select.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FrozenUniverse {
    pub schema_version: u32,
    pub run_seed: String,
    pub suite_revision: String,
    pub instances_per_archetype: u32,
    pub instances: Vec<TaskInstance>,
    pub universe_hash: String,
}

/// Excluded or confirmatory schedule phase.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulePhase {
    Smoke,
    Pilot,
    Main,
}

impl SchedulePhase {
    fn domain(self) -> &'static [u8] {
        match self {
            Self::Smoke => b"smoke",
            Self::Pilot => b"pilot",
            Self::Main => b"main",
        }
    }
}

/// One scheduled trial, including fields used to validate a resumed attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrialScheduleRecord {
    pub run_id: String,
    pub pair_id: String,
    pub task_id: String,
    pub instance_hash: String,
    pub phase: SchedulePhase,
    pub phase_repetition: u32,
    pub archetype_repetition: u32,
    pub variant: DescriptionVariant,
    pub order_index: u8,
    pub trial_identity_hash: String,
}

/// Adjacent two-trial unit. Only these records are globally shuffled.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PairScheduleRecord {
    pub run_id: String,
    pub pair_id: String,
    pub archetype_id: String,
    pub task_id: String,
    pub instance_hash: String,
    pub phase: SchedulePhase,
    pub phase_repetition: u32,
    pub archetype_repetition: u32,
    pub uncommon_text_lane: Option<UncommonTextLane>,
    pub trials: [TrialScheduleRecord; 2],
    pub pair_identity_hash: String,
}

/// Complete pair-only execution schedule.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FrozenSchedule {
    pub schema_version: u32,
    pub run_id: String,
    pub run_seed: String,
    pub suite_revision: String,
    pub universe_hash: String,
    pub smoke: Vec<PairScheduleRecord>,
    pub pilot: Vec<PairScheduleRecord>,
    pub main: Vec<PairScheduleRecord>,
    pub schedule_hash: String,
}

/// Serialized output of the non-live `freeze` command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FrozenPlan {
    pub manifest: SuiteManifest,
    pub descriptions: [FrozenDescription; 2],
    pub universe: FrozenUniverse,
    pub schedule: FrozenSchedule,
}

/// Creates the deterministic maximum task universe.
pub fn freeze_universe(
    manifest: &SuiteManifest,
    run_seed: &str,
    instances_per_archetype: u32,
) -> Result<FrozenUniverse, ScheduleError> {
    if instances_per_archetype < 5 {
        return Err(ScheduleError(
            "universe-per-archetype must admit one smoke, three pilot, and one main pair".into(),
        ));
    }
    let revision = suite_revision(manifest).map_err(|error| ScheduleError(error.to_string()))?;
    let mut instances = Vec::new();
    for archetype in &manifest.archetypes {
        for universe_index in 0..instances_per_archetype {
            instances.push(make_instance(
                archetype,
                run_seed,
                &revision,
                universe_index,
            )?);
        }
    }
    let material = serde_json::to_vec(&instances)
        .map_err(|error| ScheduleError(format!("cannot serialize universe: {error}")))?;
    let universe_hash = hash_framed(
        b"aj-apply-patch-eval-universe-v1",
        &[revision.as_bytes(), run_seed.as_bytes(), &material],
    );
    Ok(FrozenUniverse {
        schema_version: 1,
        run_seed: run_seed.into(),
        suite_revision: revision,
        instances_per_archetype,
        instances,
        universe_hash,
    })
}

/// Selects non-overlapping smoke, pilot, and main pairs.
pub fn freeze_schedule(
    manifest: &SuiteManifest,
    universe: &FrozenUniverse,
    main_repetitions: u32,
) -> Result<FrozenSchedule, ScheduleError> {
    validate_universe(manifest, universe)?;
    if main_repetitions == 0 || main_repetitions + 4 > universe.instances_per_archetype {
        return Err(ScheduleError(
            "main repetitions do not fit the frozen universe".into(),
        ));
    }
    let run_id = hash_framed(
        b"aj-apply-patch-eval-run-id-v1",
        &[
            universe.run_seed.as_bytes(),
            universe.suite_revision.as_bytes(),
            universe.universe_hash.as_bytes(),
        ],
    );
    let mut used = vec![
        vec![false; usize::try_from(universe.instances_per_archetype).unwrap()];
        manifest.archetypes.len()
    ];
    let smoke = select_phase(
        manifest,
        universe,
        &run_id,
        SchedulePhase::Smoke,
        1,
        0,
        &mut used,
    )?;
    let pilot = select_phase(
        manifest,
        universe,
        &run_id,
        SchedulePhase::Pilot,
        3,
        1,
        &mut used,
    )?;
    let main = select_phase(
        manifest,
        universe,
        &run_id,
        SchedulePhase::Main,
        main_repetitions,
        4,
        &mut used,
    )?;

    let material = serde_json::to_vec(&(&smoke, &pilot, &main))
        .map_err(|error| ScheduleError(format!("cannot serialize schedule: {error}")))?;
    let schedule_hash = hash_framed(
        b"aj-apply-patch-eval-schedule-v1",
        &[
            run_id.as_bytes(),
            universe.universe_hash.as_bytes(),
            &material,
        ],
    );
    Ok(FrozenSchedule {
        schema_version: 1,
        run_id,
        run_seed: universe.run_seed.clone(),
        suite_revision: universe.suite_revision.clone(),
        universe_hash: universe.universe_hash.clone(),
        smoke,
        pilot,
        main,
        schedule_hash,
    })
}

/// Verifies every deterministic instance identity and the universe root hash.
pub fn validate_universe(
    manifest: &SuiteManifest,
    universe: &FrozenUniverse,
) -> Result<(), ScheduleError> {
    let revision = suite_revision(manifest).map_err(|error| ScheduleError(error.to_string()))?;
    if universe.schema_version != 1 || universe.suite_revision != revision {
        return Err(ScheduleError(
            "universe schema or suite revision mismatch".into(),
        ));
    }
    if universe.instances_per_archetype < 5 {
        return Err(ScheduleError("universe is too small for all phases".into()));
    }
    let expected_count = usize::try_from(universe.instances_per_archetype)
        .expect("u32 fits usize")
        .checked_mul(manifest.archetypes.len())
        .ok_or_else(|| ScheduleError("universe size overflow".into()))?;
    if universe.instances.len() != expected_count {
        return Err(ScheduleError(
            "universe has an unexpected instance count".into(),
        ));
    }
    let mut position = 0;
    for archetype in &manifest.archetypes {
        for universe_index in 0..universe.instances_per_archetype {
            let expected = make_instance(
                archetype,
                &universe.run_seed,
                &universe.suite_revision,
                universe_index,
            )?;
            if universe.instances[position] != expected {
                return Err(ScheduleError(format!(
                    "invalid universe instance {}/{universe_index}",
                    archetype.id
                )));
            }
            position += 1;
        }
    }
    let material = serde_json::to_vec(&universe.instances)
        .map_err(|error| ScheduleError(format!("cannot serialize universe: {error}")))?;
    let expected_hash = hash_framed(
        b"aj-apply-patch-eval-universe-v1",
        &[revision.as_bytes(), universe.run_seed.as_bytes(), &material],
    );
    if universe.universe_hash != expected_hash {
        return Err(ScheduleError("universe hash mismatch".into()));
    }
    Ok(())
}

/// Verifies the self-contained identity hash of one generated task instance.
pub fn validate_task_instance_identity(instance: &TaskInstance) -> Result<(), ScheduleError> {
    let material = serde_json::to_vec(&(
        &instance.task_id,
        &instance.archetype_id,
        &instance.task_seed,
        instance.universe_index,
        &instance.parameters,
        &instance.suite_revision,
    ))
    .map_err(|error| ScheduleError(format!("cannot serialize task identity: {error}")))?;
    let expected = hash_framed(b"aj-apply-patch-eval-instance-v1", &[&material]);
    if instance.instance_hash != expected {
        return Err(ScheduleError("task instance identity hash mismatch".into()));
    }
    Ok(())
}

/// Regenerates and verifies all schedule identities and hashes.
pub fn validate_schedule(
    manifest: &SuiteManifest,
    universe: &FrozenUniverse,
    schedule: &FrozenSchedule,
) -> Result<(), ScheduleError> {
    let expected = freeze_schedule(
        manifest,
        universe,
        u32::try_from(schedule.main.len() / manifest.archetypes.len())
            .map_err(|_| ScheduleError("main schedule size exceeds u32".into()))?,
    )?;
    if schedule != &expected {
        return Err(ScheduleError("schedule identities or hash mismatch".into()));
    }
    Ok(())
}

/// Freezes a universe and uses all remaining instances for the main schedule.
pub fn freeze_plan(
    manifest: &SuiteManifest,
    run_seed: &str,
    instances_per_archetype: u32,
) -> Result<FrozenPlan, ScheduleError> {
    let universe = freeze_universe(manifest, run_seed, instances_per_archetype)?;
    let schedule = freeze_schedule(manifest, &universe, instances_per_archetype - 4)?;
    Ok(FrozenPlan {
        manifest: manifest.clone(),
        descriptions: [
            load(DescriptionVariant::Current),
            load(DescriptionVariant::CompactV1),
        ],
        universe,
        schedule,
    })
}

fn select_phase(
    manifest: &SuiteManifest,
    universe: &FrozenUniverse,
    run_id: &str,
    phase: SchedulePhase,
    repetitions: u32,
    repetition_offset: u32,
    used: &mut [Vec<bool>],
) -> Result<Vec<PairScheduleRecord>, ScheduleError> {
    let mut pairs = Vec::new();
    for (archetype_position, archetype) in manifest.archetypes.iter().enumerate() {
        let mut permutation = (0..universe.instances_per_archetype).collect::<Vec<_>>();
        CounterRng::new(
            b"phase-instance-permutation-v1",
            &[
                universe.run_seed.as_bytes(),
                phase.domain(),
                archetype.id.as_bytes(),
            ],
        )
        .shuffle(&mut permutation);
        let mut selected = Vec::new();
        for phase_repetition in 0..repetitions {
            let archetype_repetition = repetition_offset + phase_repetition;
            let selected_index = permutation.iter().copied().find(|index| {
                !used[archetype_position][usize::try_from(*index).expect("u32 fits usize")]
                    && !selected.contains(index)
                    && (archetype.id != "uncommon-text"
                        || instance_lane(universe, &archetype.id, *index)
                            == Some(lane_for_repetition(archetype_repetition)))
            });
            if let Some(index) = selected_index {
                selected.push(index);
            }
        }
        if selected.len() != usize::try_from(repetitions).unwrap() {
            return Err(ScheduleError(format!(
                "not enough unused {} instances",
                archetype.id
            )));
        }
        let first_is_compact = CounterRng::new(
            b"archetype-first-order-v1",
            &[universe.run_seed.as_bytes(), archetype.id.as_bytes()],
        )
        .boolean();
        for (phase_repetition, universe_index) in selected.into_iter().enumerate() {
            used[archetype_position][usize::try_from(universe_index).unwrap()] = true;
            let instance = find_instance(universe, &archetype.id, universe_index)?;
            let archetype_repetition = repetition_offset
                + u32::try_from(phase_repetition).expect("phase repetition fits u32");
            let compact_first = first_is_compact ^ (archetype_repetition % 2 == 1);
            pairs.push(make_pair(
                run_id,
                phase,
                u32::try_from(phase_repetition).unwrap(),
                archetype_repetition,
                instance,
                compact_first,
            )?);
        }
    }
    CounterRng::new(
        b"phase-pair-shuffle-v1",
        &[
            universe.run_seed.as_bytes(),
            phase.domain(),
            universe.universe_hash.as_bytes(),
        ],
    )
    .shuffle(&mut pairs);
    Ok(pairs)
}

fn instance_lane(
    universe: &FrozenUniverse,
    archetype_id: &str,
    universe_index: u32,
) -> Option<UncommonTextLane> {
    find_instance(universe, archetype_id, universe_index)
        .ok()
        .and_then(|instance| match &instance.parameters {
            TaskParameters::UncommonText { lane, .. } => Some(*lane),
            _ => None,
        })
}

fn lane_for_repetition(repetition: u32) -> UncommonTextLane {
    if repetition % 2 == 0 {
        UncommonTextLane::ConflictMarkers
    } else {
        UncommonTextLane::Crlf
    }
}

fn find_instance<'a>(
    universe: &'a FrozenUniverse,
    archetype_id: &str,
    universe_index: u32,
) -> Result<&'a TaskInstance, ScheduleError> {
    universe
        .instances
        .iter()
        .find(|instance| {
            instance.archetype_id == archetype_id && instance.universe_index == universe_index
        })
        .ok_or_else(|| {
            ScheduleError(format!(
                "missing universe instance {archetype_id}/{universe_index}"
            ))
        })
}

fn make_pair(
    run_id: &str,
    phase: SchedulePhase,
    phase_repetition: u32,
    archetype_repetition: u32,
    instance: &TaskInstance,
    compact_first: bool,
) -> Result<PairScheduleRecord, ScheduleError> {
    let pair_id = hash_framed(
        b"aj-apply-patch-eval-pair-id-v1",
        &[
            run_id.as_bytes(),
            phase.domain(),
            instance.instance_hash.as_bytes(),
            &phase_repetition.to_be_bytes(),
        ],
    );
    let variants = if compact_first {
        [DescriptionVariant::CompactV1, DescriptionVariant::Current]
    } else {
        [DescriptionVariant::Current, DescriptionVariant::CompactV1]
    };
    let make_trial = |variant, order_index| {
        let trial_identity_hash = hash_framed(
            b"aj-apply-patch-eval-trial-identity-v1",
            &[
                run_id.as_bytes(),
                pair_id.as_bytes(),
                instance.instance_hash.as_bytes(),
                &[order_index],
            ],
        );
        TrialScheduleRecord {
            run_id: run_id.into(),
            pair_id: pair_id.clone(),
            task_id: instance.task_id.clone(),
            instance_hash: instance.instance_hash.clone(),
            phase,
            phase_repetition,
            archetype_repetition,
            variant,
            order_index,
            trial_identity_hash,
        }
    };
    let trials = [make_trial(variants[0], 0), make_trial(variants[1], 1)];
    let uncommon_text_lane = match &instance.parameters {
        TaskParameters::UncommonText { lane, .. } => Some(*lane),
        _ => None,
    };
    let pair_material = serde_json::to_vec(&trials)
        .map_err(|error| ScheduleError(format!("cannot serialize pair: {error}")))?;
    let pair_identity_hash = hash_framed(
        b"aj-apply-patch-eval-pair-identity-v1",
        &[pair_id.as_bytes(), &pair_material],
    );
    Ok(PairScheduleRecord {
        run_id: run_id.into(),
        pair_id,
        archetype_id: instance.archetype_id.clone(),
        task_id: instance.task_id.clone(),
        instance_hash: instance.instance_hash.clone(),
        phase,
        phase_repetition,
        archetype_repetition,
        uncommon_text_lane,
        trials,
        pair_identity_hash,
    })
}

fn make_instance(
    archetype: &ArchetypeManifest,
    run_seed: &str,
    revision: &str,
    universe_index: u32,
) -> Result<TaskInstance, ScheduleError> {
    let task_seed = hash_framed(
        b"aj-apply-patch-eval-task-seed-v1",
        &[
            run_seed.as_bytes(),
            revision.as_bytes(),
            archetype.id.as_bytes(),
            &universe_index.to_be_bytes(),
        ],
    );
    let token = &task_seed[..12];
    let parameters = parameters(archetype.parameter_kind, token, universe_index);
    let task_id = format!("{}-{universe_index:04}-{token}", archetype.id);
    let material = serde_json::to_vec(&(
        &task_id,
        &archetype.id,
        &task_seed,
        universe_index,
        &parameters,
        revision,
    ))
    .map_err(|error| ScheduleError(format!("cannot serialize task identity: {error}")))?;
    let instance_hash = hash_framed(b"aj-apply-patch-eval-instance-v1", &[&material]);
    Ok(TaskInstance {
        task_id,
        archetype_id: archetype.id.clone(),
        task_seed,
        universe_index,
        parameters,
        suite_revision: revision.into(),
        instance_hash,
    })
}

fn parameters(kind: ParameterKind, token: &str, index: u32) -> TaskParameters {
    let path = |stem: &str, extension: &str| format!("src/{stem}_{token}.{extension}");
    let number = 3 + index % 7;
    match kind {
        ParameterKind::UniqueReplacement => TaskParameters::UniqueReplacement {
            path: path("record", "txt"),
            old: format!("old_{token}"),
            new: format!("new_{token}"),
            retry_count: number,
        },
        ParameterKind::MultilineEdit => TaskParameters::MultilineEdit {
            path: path("service", "py"),
            symbol: format!("service_{token}"),
            boundary: i32::try_from(number).unwrap(),
            increment: i32::try_from(number + 2).unwrap(),
        },
        ParameterKind::Insertion => TaskParameters::Insertion {
            path: path("list", "txt"),
            anchor: format!("anchor_{token}"),
            value: format!("value_{token}"),
        },
        ParameterKind::Removal => TaskParameters::Removal {
            path: path("options", "toml"),
            key: format!("obsolete_{token}"),
            retained_value: number,
        },
        ParameterKind::IndentationSensitive => TaskParameters::IndentationSensitive {
            path: path("config", "yaml"),
            section: format!("section_{token}"),
            timeout: number * 10,
        },
        ParameterKind::NearbyChanges => TaskParameters::NearbyChanges {
            path: path("nearby", "py"),
            first: format!("first_{token}"),
            second: format!("second_{token}"),
            amount: i32::try_from(number).unwrap(),
        },
        ParameterKind::TwoRelatedSourceFiles => TaskParameters::TwoRelatedSourceFiles {
            model_path: path("model", "py"),
            view_path: path("view", "py"),
            symbol: format!("Record{token}"),
            default_limit: number * 5,
        },
        ParameterKind::SourcePlusTest => TaskParameters::SourcePlusTest {
            source_path: path("math", "py"),
            test_path: format!("tests/math_{token}_test.py"),
            symbol: format!("calculate_{token}"),
            boundary: i32::try_from(number).unwrap(),
        },
        ParameterKind::ThreeFileConfiguration => TaskParameters::ThreeFileConfiguration {
            paths: [
                format!("config/base_{token}.toml"),
                format!("config/dev_{token}.toml"),
                format!("config/prod_{token}.toml"),
            ],
            key: format!("setting_{token}"),
            values: [number * 10, number * 10 + 1, number * 10 + 2],
        },
        ParameterKind::AddFile => TaskParameters::AddFile {
            path: path("generated", "txt"),
            content_token: token.into(),
            number,
        },
        ParameterKind::DeleteFile => TaskParameters::DeleteFile {
            path: path("obsolete", "txt"),
            number,
        },
        ParameterKind::RenameWithContent => TaskParameters::RenameWithContent {
            old_path: path("old", "py"),
            new_path: path("new", "py"),
            old_symbol: format!("legacy_{token}"),
            symbol: format!("renamed_{token}"),
            multiplier: number,
        },
        ParameterKind::RepeatedBlocks => TaskParameters::RepeatedBlocks {
            path: path("blocks", "txt"),
            target_label: format!("target_{token}"),
            old_limit: number,
            new_limit: number + 4,
        },
        ParameterKind::RepeatedMethods => TaskParameters::RepeatedMethods {
            path: path("methods", "py"),
            target_type: format!("Type{token}"),
            method: "render".into(),
            suffix: number,
        },
        ParameterKind::EndOfFile => TaskParameters::EndOfFile {
            path: path("tail", "txt"),
            value: format!("tail_{token}"),
        },
        ParameterKind::UncommonText => TaskParameters::UncommonText {
            path: path("uncommon", "txt"),
            lane: if index % 2 == 0 {
                UncommonTextLane::ConflictMarkers
            } else {
                UncommonTextLane::Crlf
            },
            token: token.into(),
            marker_width: u8::try_from(7 + index % 3).unwrap(),
            number,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;
    use crate::suite::committed_manifest;

    #[test]
    fn phases_are_deterministic_disjoint_and_pair_only() {
        let manifest = committed_manifest().unwrap();
        let first = freeze_plan(&manifest, "schedule-seed", 7).unwrap();
        let second = freeze_plan(&manifest, "schedule-seed", 7).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.schedule.smoke.len(), 16);
        assert_eq!(first.schedule.pilot.len(), 48);
        assert_eq!(first.schedule.main.len(), 48);

        let mut task_ids = HashSet::new();
        for pair in first
            .schedule
            .smoke
            .iter()
            .chain(&first.schedule.pilot)
            .chain(&first.schedule.main)
        {
            assert!(task_ids.insert(&pair.task_id));
            assert_eq!(pair.trials[0].pair_id, pair.trials[1].pair_id);
            assert_ne!(pair.trials[0].variant, pair.trials[1].variant);
            assert_eq!(
                [pair.trials[0].order_index, pair.trials[1].order_index],
                [0, 1]
            );
        }
    }

    #[test]
    fn order_and_uncommon_lane_alternate_within_each_archetype() {
        let manifest = committed_manifest().unwrap();
        let plan = freeze_plan(&manifest, "alternation", 8).unwrap();
        for phase in [
            &plan.schedule.smoke,
            &plan.schedule.pilot,
            &plan.schedule.main,
        ] {
            let mut by_archetype: HashMap<&str, Vec<&PairScheduleRecord>> = HashMap::new();
            for pair in phase {
                by_archetype
                    .entry(&pair.archetype_id)
                    .or_default()
                    .push(pair);
            }
            for pairs in by_archetype.values_mut() {
                pairs.sort_by_key(|pair| pair.phase_repetition);
                for adjacent in pairs.windows(2) {
                    assert_ne!(adjacent[0].trials[0].variant, adjacent[1].trials[0].variant);
                }
            }
            let uncommon = &by_archetype["uncommon-text"];
            for pair in uncommon {
                let expected = if pair.archetype_repetition % 2 == 0 {
                    UncommonTextLane::ConflictMarkers
                } else {
                    UncommonTextLane::Crlf
                };
                assert_eq!(pair.uncommon_text_lane, Some(expected));
            }
        }
    }

    #[test]
    fn identities_change_with_seed_and_are_self_consistent() {
        let manifest = committed_manifest().unwrap();
        let first = freeze_plan(&manifest, "one", 5).unwrap();
        let other = freeze_plan(&manifest, "two", 5).unwrap();
        assert_ne!(first.universe.universe_hash, other.universe.universe_hash);
        assert_ne!(first.schedule.schedule_hash, other.schedule.schedule_hash);
        for instance in &first.universe.instances {
            let archetype = manifest
                .archetypes
                .iter()
                .find(|item| item.id == instance.archetype_id)
                .unwrap();
            assert_eq!(instance.parameters.kind(), archetype.parameter_kind);
        }
        validate_universe(&manifest, &first.universe).unwrap();
        validate_schedule(&manifest, &first.universe, &first.schedule).unwrap();
        let mut corrupted = first.universe.clone();
        corrupted.instances[0].task_seed = "corrupt".into();
        assert!(validate_universe(&manifest, &corrupted).is_err());
    }
}
