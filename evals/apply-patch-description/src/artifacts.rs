//! Durable append-only trial artifacts and content-addressed blobs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::descriptions::DescriptionVariant;
use crate::runtime::RuntimeRecord;
use crate::schedule::{PairScheduleRecord, SchedulePhase, TrialScheduleRecord};
use crate::{hash_framed, sha256_hex};

static NEXT_BLOB_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// Artifact contract or durability error.
#[derive(Debug)]
pub struct ArtifactError(pub String);

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ArtifactError {}

impl From<io::Error> for ArtifactError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

/// Stable fields shared by a trial record and its completion marker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrialIdentity {
    pub run_id: String,
    pub pair_id: String,
    pub attempt_id: String,
    pub task_id: String,
    pub instance_hash: String,
    pub archetype_id: String,
    pub schedule_hash: String,
    pub phase: SchedulePhase,
    pub repetition: u32,
    pub variant: DescriptionVariant,
    pub order_index: u8,
}

/// Hash and byte length recorded for one frozen description.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordedDescription {
    pub sha256: String,
    pub byte_length: u64,
}

/// Typed immutable context surrounding the extensible runtime payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrialMetadata {
    pub task_seed: String,
    pub current_description: RecordedDescription,
    pub compact_description: RecordedDescription,
    pub aj_revision: String,
    pub suite_revision: String,
    pub model_catalog_hash: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: String,
    #[serde(default)]
    pub tool_catalog_hash: String,
    pub fixture_revision: String,
}

/// Append-only trial envelope. Runtime fields remain extensible until phase 2.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TrialRecord {
    pub schema_version: u32,
    pub identity: TrialIdentity,
    pub metadata: TrialMetadata,
    pub runtime: Value,
    pub record_hash: String,
}

impl TrialRecord {
    /// Constructs a record with a hash over its typed identity and runtime payload.
    pub fn new(
        identity: TrialIdentity,
        metadata: TrialMetadata,
        runtime: Value,
    ) -> Result<Self, ArtifactError> {
        let mut record = Self {
            schema_version: 2,
            identity,
            metadata,
            runtime,
            record_hash: String::new(),
        };
        record.record_hash = record.computed_hash()?;
        Ok(record)
    }

    fn computed_hash(&self) -> Result<String, ArtifactError> {
        let bytes = serde_json::to_vec(&(
            self.schema_version,
            &self.identity,
            &self.metadata,
            &self.runtime,
        ))
        .map_err(|error| ArtifactError(format!("cannot hash trial record: {error}")))?;
        Ok(hash_framed(
            b"aj-apply-patch-eval-trial-record-v2",
            &[&bytes],
        ))
    }
}

/// Pair identity copied into a durable completion marker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PairCompletionIdentity {
    pub run_id: String,
    pub pair_id: String,
    pub attempt_id: String,
    pub task_id: String,
    pub instance_hash: String,
    pub schedule_hash: String,
    pub phase: SchedulePhase,
}

/// Marker written only after two referenced valid trial lines are durable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PairCompleteRecord {
    pub schema_version: u32,
    pub identity: PairCompletionIdentity,
    pub trial_record_hashes: [String; 2],
    pub record_hash: String,
}

impl PairCompleteRecord {
    fn computed_hash(&self) -> Result<String, ArtifactError> {
        let bytes = serde_json::to_vec(&(
            self.schema_version,
            &self.identity,
            &self.trial_record_hashes,
        ))
        .map_err(|error| ArtifactError(format!("cannot hash completion marker: {error}")))?;
        Ok(hash_framed(
            b"aj-apply-patch-eval-pair-marker-v1",
            &[&bytes],
        ))
    }
}

/// A typed line in the artifact stream.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub enum ArtifactRecord {
    Trial(TrialRecord),
    PairComplete(PairCompleteRecord),
}

/// Schedule pair key skipped during resume only after a valid marker.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PairKey {
    pub run_id: String,
    pub pair_id: String,
}

/// Verified state recovered from an artifact stream.
#[derive(Clone, Debug, Default)]
pub struct ResumeState {
    pub trials_by_hash: BTreeMap<String, TrialRecord>,
    pub complete_pairs: BTreeSet<PairKey>,
    pub completion_markers: BTreeMap<PairKey, PairCompleteRecord>,
    pub truncated_final_line: bool,
    trial_record_order: Vec<String>,
    trial_identities: BTreeSet<(String, String, String, u8)>,
}

/// Marker and ordered trials proven to match one exact frozen pair.
#[derive(Debug)]
pub struct CompletedPair<'a> {
    pub marker: &'a PairCompleteRecord,
    pub trials: [&'a TrialRecord; 2],
}

/// An unmarked durable pair attempt that can be completed without rerunning it.
#[derive(Debug)]
pub struct RecoverablePair {
    pub identity: PairCompletionIdentity,
    pub trial_record_hashes: [String; 2],
}

/// Returns the number of distinct attempts recorded for one pair.
pub fn pair_attempt_count(state: &ResumeState, run_id: &str, pair_id: &str) -> usize {
    state
        .trials_by_hash
        .values()
        .filter(|trial| trial.identity.run_id == run_id && trial.identity.pair_id == pair_id)
        .map(|trial| trial.identity.attempt_id.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

/// Finds the earliest complete valid attempt that lacks a completion marker.
pub fn recoverable_pair(
    state: &ResumeState,
    schedule_hash: &str,
    pair: &PairScheduleRecord,
) -> Result<Option<RecoverablePair>, ArtifactError> {
    let key = PairKey {
        run_id: pair.run_id.clone(),
        pair_id: pair.pair_id.clone(),
    };
    if state.complete_pairs.contains(&key) {
        return Ok(None);
    }

    let mut attempts = BTreeMap::<&str, [Option<&TrialRecord>; 2]>::new();
    for hash in &state.trial_record_order {
        let trial = &state.trials_by_hash[hash];
        if trial.identity.run_id != pair.run_id || trial.identity.pair_id != pair.pair_id {
            continue;
        }
        let scheduled = &pair.trials[usize::from(trial.identity.order_index)];
        if !trial_matches_slot(trial, schedule_hash, pair, scheduled) {
            return Err(ArtifactError(format!(
                "unmarked trial for pair {} is outside its frozen slot",
                pair.pair_id
            )));
        }
        if validate_completed_trial(trial, 0).is_err() {
            continue;
        }
        let attempt = attempts
            .entry(&trial.identity.attempt_id)
            .or_insert([None, None]);
        attempt[usize::from(trial.identity.order_index)] = Some(trial);
        if let [Some(first), Some(second)] = *attempt {
            if first.metadata != second.metadata {
                return Err(ArtifactError(format!(
                    "unmarked pair attempt {} has mixed metadata",
                    pair.pair_id
                )));
            }
            return Ok(Some(RecoverablePair {
                identity: PairCompletionIdentity {
                    run_id: pair.run_id.clone(),
                    pair_id: pair.pair_id.clone(),
                    attempt_id: first.identity.attempt_id.clone(),
                    task_id: pair.task_id.clone(),
                    instance_hash: pair.instance_hash.clone(),
                    schedule_hash: schedule_hash.to_string(),
                    phase: pair.phase,
                },
                trial_record_hashes: [first.record_hash.clone(), second.record_hash.clone()],
            }));
        }
    }
    Ok(None)
}

/// Resolves a completion marker and validates its full frozen pair identity.
pub fn completed_pair<'a>(
    state: &'a ResumeState,
    schedule_hash: &str,
    pair: &PairScheduleRecord,
) -> Result<CompletedPair<'a>, ArtifactError> {
    let marker = state
        .completion_markers
        .get(&PairKey {
            run_id: pair.run_id.clone(),
            pair_id: pair.pair_id.clone(),
        })
        .ok_or_else(|| ArtifactError(format!("missing complete pair {}", pair.pair_id)))?;
    if marker.identity.run_id != pair.run_id
        || marker.identity.pair_id != pair.pair_id
        || marker.identity.task_id != pair.task_id
        || marker.identity.instance_hash != pair.instance_hash
        || marker.identity.schedule_hash != schedule_hash
        || marker.identity.phase != pair.phase
    {
        return Err(ArtifactError(format!(
            "completion marker {} does not match its frozen pair",
            pair.pair_id
        )));
    }
    let trials = marker.trial_record_hashes.each_ref().map(|hash| {
        state
            .trials_by_hash
            .get(hash)
            .expect("scan validated marker trial references")
    });
    for (trial, scheduled) in trials.iter().zip(&pair.trials) {
        if !trial_matches_slot(trial, schedule_hash, pair, scheduled) {
            return Err(ArtifactError(format!(
                "completion marker {} references a trial outside its frozen slot",
                pair.pair_id
            )));
        }
    }
    Ok(CompletedPair { marker, trials })
}

fn trial_matches_slot(
    trial: &TrialRecord,
    schedule_hash: &str,
    pair: &PairScheduleRecord,
    scheduled: &TrialScheduleRecord,
) -> bool {
    trial.identity.run_id == scheduled.run_id
        && trial.identity.pair_id == scheduled.pair_id
        && trial.identity.task_id == scheduled.task_id
        && trial.identity.instance_hash == scheduled.instance_hash
        && trial.identity.archetype_id == pair.archetype_id
        && trial.identity.schedule_hash == schedule_hash
        && trial.identity.phase == scheduled.phase
        && trial.identity.repetition == scheduled.archetype_repetition
        && trial.identity.variant == scheduled.variant
        && trial.identity.order_index == scheduled.order_index
}

/// Scans JSONL, ignoring at most one incomplete final line.
pub fn scan(path: &Path) -> Result<ResumeState, ArtifactError> {
    if !path.exists() {
        return Ok(ResumeState::default());
    }
    let bytes = fs::read(path)?;
    let mut state = ResumeState::default();
    let complete_bytes = match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(last_newline) if last_newline + 1 < bytes.len() => {
            state.truncated_final_line = true;
            &bytes[..=last_newline]
        }
        Some(_) => bytes.as_slice(),
        None if bytes.is_empty() => bytes.as_slice(),
        None => {
            state.truncated_final_line = true;
            &bytes[..0]
        }
    };
    for (line_index, line) in complete_bytes.split(|byte| *byte == b'\n').enumerate() {
        if line.is_empty() {
            if line_index + 1 == complete_bytes.split(|byte| *byte == b'\n').count() {
                continue;
            }
            return Err(ArtifactError(format!(
                "empty artifact line {}",
                line_index + 1
            )));
        }
        let record: ArtifactRecord = serde_json::from_slice(line).map_err(|error| {
            ArtifactError(format!("corrupt artifact line {}: {error}", line_index + 1))
        })?;
        apply_record(&mut state, record, line_index + 1)?;
    }
    Ok(state)
}

fn apply_record(
    state: &mut ResumeState,
    record: ArtifactRecord,
    line: usize,
) -> Result<(), ArtifactError> {
    match record {
        ArtifactRecord::Trial(trial) => {
            if trial.schema_version != 2 {
                return Err(ArtifactError(format!(
                    "unsupported trial schema version {} at line {line}",
                    trial.schema_version
                )));
            }
            if trial.record_hash != trial.computed_hash()? {
                return Err(ArtifactError(format!("invalid trial hash at line {line}")));
            }
            if trial.identity.order_index > 1 {
                return Err(ArtifactError(format!("invalid trial order at line {line}")));
            }
            let logical_identity = logical_trial_identity(&trial);
            if !state.trial_identities.insert(logical_identity) {
                return Err(ArtifactError(format!(
                    "duplicate logical trial identity at line {line}"
                )));
            }
            state.trial_record_order.push(trial.record_hash.clone());
            if state
                .trials_by_hash
                .insert(trial.record_hash.clone(), trial)
                .is_some()
            {
                return Err(ArtifactError(format!(
                    "duplicate trial record at line {line}"
                )));
            }
        }
        ArtifactRecord::PairComplete(marker) => {
            validate_marker(state, &marker, line)?;
            let key = PairKey {
                run_id: marker.identity.run_id.clone(),
                pair_id: marker.identity.pair_id.clone(),
            };
            if !state.complete_pairs.insert(key) {
                return Err(ArtifactError(format!(
                    "duplicate pair completion at line {line}"
                )));
            }
            state.completion_markers.insert(
                PairKey {
                    run_id: marker.identity.run_id.clone(),
                    pair_id: marker.identity.pair_id.clone(),
                },
                marker,
            );
        }
    }
    Ok(())
}

fn validate_marker(
    state: &ResumeState,
    marker: &PairCompleteRecord,
    line: usize,
) -> Result<(), ArtifactError> {
    if marker.schema_version != 1 || marker.record_hash != marker.computed_hash()? {
        return Err(ArtifactError(format!(
            "invalid pair marker hash at line {line}"
        )));
    }
    if marker.trial_record_hashes[0] == marker.trial_record_hashes[1] {
        return Err(ArtifactError(format!(
            "pair marker repeats a trial at line {line}"
        )));
    }
    let first = state
        .trials_by_hash
        .get(&marker.trial_record_hashes[0])
        .ok_or_else(|| {
            ArtifactError(format!(
                "pair marker references a missing trial at line {line}"
            ))
        })?;
    let second = state
        .trials_by_hash
        .get(&marker.trial_record_hashes[1])
        .ok_or_else(|| {
            ArtifactError(format!(
                "pair marker references a missing trial at line {line}"
            ))
        })?;
    for trial in [first, second] {
        validate_completed_trial(trial, line)?;
        let identity = &trial.identity;
        if identity.run_id != marker.identity.run_id
            || identity.pair_id != marker.identity.pair_id
            || identity.attempt_id != marker.identity.attempt_id
            || identity.task_id != marker.identity.task_id
            || identity.instance_hash != marker.identity.instance_hash
            || identity.schedule_hash != marker.identity.schedule_hash
            || identity.phase != marker.identity.phase
        {
            return Err(ArtifactError(format!(
                "pair marker identity mismatch at line {line}"
            )));
        }
    }
    if first.identity.order_index != 0
        || second.identity.order_index != 1
        || first.identity.variant == second.identity.variant
        || first.identity.archetype_id != second.identity.archetype_id
        || first.identity.repetition != second.identity.repetition
        || first.metadata != second.metadata
    {
        return Err(ArtifactError(format!(
            "pair marker does not reference an ordered AB/BA pair at line {line}"
        )));
    }
    Ok(())
}

fn logical_trial_identity(trial: &TrialRecord) -> (String, String, String, u8) {
    (
        trial.identity.run_id.clone(),
        trial.identity.pair_id.clone(),
        trial.identity.attempt_id.clone(),
        trial.identity.order_index,
    )
}

fn validate_completed_trial(trial: &TrialRecord, line: usize) -> Result<(), ArtifactError> {
    let runtime: RuntimeRecord =
        serde_json::from_value(trial.runtime.clone()).map_err(|error| {
            ArtifactError(format!(
                "pair completion references an invalid runtime at line {line}: {error}"
            ))
        })?;
    if runtime.completion_eligible() {
        Ok(())
    } else {
        Err(ArtifactError(format!(
            "pair completion references an invalid or provider-contaminated trial at line {line}"
        )))
    }
}

/// Writer that flushes and syncs every complete JSONL line.
pub struct ArtifactLog {
    path: PathBuf,
    file: File,
    trial_identities: BTreeSet<(String, String, String, u8)>,
}

impl ArtifactLog {
    /// Opens an append-only stream after validating all existing records.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ArtifactError> {
        let path = path.into();
        let state = scan(&path)?;
        if state.truncated_final_line {
            return Err(ArtifactError(
                "artifact has a truncated final line and cannot be appended without rewriting history".into(),
            ));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file,
            trial_identities: state.trial_identities,
        })
    }

    /// Appends and synchronizes one immutable trial record.
    pub fn append_trial(&mut self, trial: &TrialRecord) -> Result<(), ArtifactError> {
        if trial.record_hash != trial.computed_hash()? {
            return Err(ArtifactError(
                "refusing to append a trial with an invalid hash".into(),
            ));
        }
        let identity = logical_trial_identity(trial);
        if self.trial_identities.contains(&identity) {
            return Err(ArtifactError(
                "refusing to append a duplicate logical trial identity".into(),
            ));
        }
        self.append(&ArtifactRecord::Trial(trial.clone()))?;
        self.trial_identities.insert(identity);
        Ok(())
    }

    /// Validates two durable trials, then appends and synchronizes their marker.
    pub fn complete_pair(
        &mut self,
        identity: PairCompletionIdentity,
        trial_record_hashes: [String; 2],
    ) -> Result<PairCompleteRecord, ArtifactError> {
        self.file.flush()?;
        self.file.sync_data()?;
        let state = scan(&self.path)?;
        let mut marker = PairCompleteRecord {
            schema_version: 1,
            identity,
            trial_record_hashes,
            record_hash: String::new(),
        };
        marker.record_hash = marker.computed_hash()?;
        validate_marker(&state, &marker, 0)?;
        let key = PairKey {
            run_id: marker.identity.run_id.clone(),
            pair_id: marker.identity.pair_id.clone(),
        };
        if state.complete_pairs.contains(&key) {
            return Err(ArtifactError("pair is already complete".into()));
        }
        self.append(&ArtifactRecord::PairComplete(marker.clone()))?;
        Ok(marker)
    }

    fn append(&mut self, record: &ArtifactRecord) -> Result<(), ArtifactError> {
        serde_json::to_writer(&mut self.file, record)
            .map_err(|error| ArtifactError(format!("cannot serialize artifact record: {error}")))?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file.sync_data()?;
        Ok(())
    }
}

/// Content-addressed storage rooted at a caller-owned artifact directory.
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Creates or opens a blob store.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, ArtifactError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Stores bytes under their SHA-256 and returns the hash.
    pub fn put(&self, bytes: &[u8]) -> Result<String, ArtifactError> {
        let hash = sha256_hex(bytes);
        let path = self.root.join(&hash);
        if path.exists() {
            if fs::read(&path)? != bytes {
                return Err(ArtifactError(format!(
                    "blob hash collision or corruption: {hash}"
                )));
            }
            return Ok(hash);
        }
        let temporary_id = NEXT_BLOB_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let temporary = self
            .root
            .join(format!(".{hash}.{}.{temporary_id}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        match fs::hard_link(&temporary, &path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if fs::read(&path)? != bytes {
                    fs::remove_file(&temporary)?;
                    return Err(ArtifactError(format!(
                        "blob hash collision or corruption: {hash}"
                    )));
                }
            }
            Err(error) => {
                fs::remove_file(&temporary)?;
                return Err(error.into());
            }
        }
        fs::remove_file(&temporary)?;
        File::open(&self.root)?.sync_all()?;
        Ok(hash)
    }

    /// Reads and verifies a blob.
    pub fn get(&self, hash: &str) -> Result<Vec<u8>, ArtifactError> {
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ArtifactError("invalid blob hash".into()));
        }
        let mut bytes = Vec::new();
        File::open(self.root.join(hash))?.read_to_end(&mut bytes)?;
        if sha256_hex(&bytes) != hash {
            return Err(ArtifactError(format!("corrupt blob: {hash}")));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::runtime::MAX_PAIR_ATTEMPTS;
    use crate::schedule::{PairScheduleRecord, TrialScheduleRecord};

    fn identity(variant: DescriptionVariant, order_index: u8, attempt: &str) -> TrialIdentity {
        TrialIdentity {
            run_id: "run".into(),
            pair_id: "pair".into(),
            attempt_id: attempt.into(),
            task_id: "task".into(),
            instance_hash: "instance".into(),
            archetype_id: "insertion".into(),
            schedule_hash: "schedule".into(),
            phase: SchedulePhase::Main,
            repetition: 0,
            variant,
            order_index,
        }
    }

    fn completion(attempt: &str) -> PairCompletionIdentity {
        PairCompletionIdentity {
            run_id: "run".into(),
            pair_id: "pair".into(),
            attempt_id: attempt.into(),
            task_id: "task".into(),
            instance_hash: "instance".into(),
            schedule_hash: "schedule".into(),
            phase: SchedulePhase::Main,
        }
    }

    fn scheduled_pair() -> PairScheduleRecord {
        let trial = |variant, order_index| TrialScheduleRecord {
            run_id: "run".into(),
            pair_id: "pair".into(),
            task_id: "task".into(),
            instance_hash: "instance".into(),
            phase: SchedulePhase::Main,
            phase_repetition: 0,
            archetype_repetition: 0,
            variant,
            order_index,
            trial_identity_hash: format!("trial-{order_index}"),
        };
        PairScheduleRecord {
            run_id: "run".into(),
            pair_id: "pair".into(),
            archetype_id: "insertion".into(),
            task_id: "task".into(),
            instance_hash: "instance".into(),
            phase: SchedulePhase::Main,
            phase_repetition: 0,
            archetype_repetition: 0,
            uncommon_text_lane: None,
            trials: [
                trial(DescriptionVariant::Current, 0),
                trial(DescriptionVariant::CompactV1, 1),
            ],
            pair_identity_hash: "pair-identity".into(),
        }
    }

    fn clean_runtime() -> Value {
        serde_json::to_value(crate::runtime::completed_runtime_fixture()).unwrap()
    }

    fn metadata() -> TrialMetadata {
        TrialMetadata {
            task_seed: "task-seed".into(),
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
            tool_catalog_hash: "tools".into(),
            fixture_revision: "fixture".into(),
        }
    }

    #[test]
    fn incomplete_attempt_is_retained_and_retry_can_complete() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let abandoned = TrialRecord::new(
            identity(DescriptionVariant::Current, 0, "attempt-1"),
            metadata(),
            json!({}),
        )
        .unwrap();
        let mut log = ArtifactLog::open(&path).unwrap();
        log.append_trial(&abandoned).unwrap();
        assert!(scan(&path).unwrap().complete_pairs.is_empty());

        let first = TrialRecord::new(
            identity(DescriptionVariant::Current, 0, "attempt-2"),
            metadata(),
            clean_runtime(),
        )
        .unwrap();
        let second = TrialRecord::new(
            identity(DescriptionVariant::CompactV1, 1, "attempt-2"),
            metadata(),
            clean_runtime(),
        )
        .unwrap();
        log.append_trial(&first).unwrap();
        log.append_trial(&second).unwrap();
        log.complete_pair(
            completion("attempt-2"),
            [first.record_hash, second.record_hash],
        )
        .unwrap();
        let state = scan(&path).unwrap();
        assert_eq!(state.trials_by_hash.len(), 3);
        assert_eq!(state.complete_pairs.len(), 1);
    }

    #[test]
    fn old_trial_schema_has_a_precise_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let record = TrialRecord::new(
            identity(DescriptionVariant::Current, 0, "old"),
            metadata(),
            json!({}),
        )
        .unwrap();
        let mut value = serde_json::to_value(ArtifactRecord::Trial(record)).unwrap();
        value["schema_version"] = json!(1);
        value["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("tool_catalog_hash");
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&value).unwrap()),
        )
        .unwrap();
        let error = scan(&path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported trial schema version 1")
        );
    }

    #[test]
    fn scanner_tolerates_only_a_truncated_final_line() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let trial = TrialRecord::new(
            identity(DescriptionVariant::Current, 0, "attempt"),
            metadata(),
            json!({}),
        )
        .unwrap();
        let line = serde_json::to_vec(&ArtifactRecord::Trial(trial)).unwrap();
        fs::write(&path, [line.as_slice(), b"\n{\"record_type\":"].concat()).unwrap();
        let state = scan(&path).unwrap();
        assert!(state.truncated_final_line);
        assert_eq!(state.trials_by_hash.len(), 1);

        fs::write(&path, b"not-json\n{\"partial\":").unwrap();
        assert!(scan(&path).unwrap_err().to_string().contains("line 1"));
    }

    #[test]
    fn rejects_invalid_hash_identity_and_duplicate_completion() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let mut invalid = TrialRecord::new(
            identity(DescriptionVariant::Current, 0, "attempt"),
            metadata(),
            json!({}),
        )
        .unwrap();
        invalid.record_hash = "bad".into();
        let mut bytes = serde_json::to_vec(&ArtifactRecord::Trial(invalid)).unwrap();
        bytes.push(b'\n');
        fs::write(&path, bytes).unwrap();
        assert!(
            scan(&path)
                .unwrap_err()
                .to_string()
                .contains("invalid trial hash")
        );

        fs::remove_file(&path).unwrap();
        let mut log = ArtifactLog::open(&path).unwrap();
        let first = TrialRecord::new(
            identity(DescriptionVariant::Current, 0, "attempt"),
            metadata(),
            clean_runtime(),
        )
        .unwrap();
        let second = TrialRecord::new(
            identity(DescriptionVariant::CompactV1, 1, "attempt"),
            metadata(),
            clean_runtime(),
        )
        .unwrap();
        log.append_trial(&first).unwrap();
        log.append_trial(&second).unwrap();
        let hashes = [first.record_hash, second.record_hash];
        log.complete_pair(completion("attempt"), hashes.clone())
            .unwrap();
        assert!(
            log.complete_pair(completion("attempt"), hashes)
                .unwrap_err()
                .to_string()
                .contains("already complete")
        );
    }

    #[test]
    fn completed_pair_rejects_a_marker_with_the_wrong_task_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let mut first_identity = identity(DescriptionVariant::Current, 0, "attempt");
        first_identity.task_id = "wrong-task".into();
        let mut second_identity = identity(DescriptionVariant::CompactV1, 1, "attempt");
        second_identity.task_id = "wrong-task".into();
        let first = TrialRecord::new(first_identity, metadata(), clean_runtime()).unwrap();
        let second = TrialRecord::new(second_identity, metadata(), clean_runtime()).unwrap();
        let mut log = ArtifactLog::open(&path).unwrap();
        log.append_trial(&first).unwrap();
        log.append_trial(&second).unwrap();
        let mut wrong = completion("attempt");
        wrong.task_id = "wrong-task".into();
        log.complete_pair(
            wrong,
            [first.record_hash.clone(), second.record_hash.clone()],
        )
        .unwrap();

        let state = scan(&path).unwrap();
        let error = completed_pair(&state, "schedule", &scheduled_pair()).unwrap_err();
        assert!(error.to_string().contains("does not match its frozen pair"));
    }

    #[test]
    fn scanner_rejects_reused_logical_trial_slots() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let first = TrialRecord::new(
            identity(DescriptionVariant::Current, 0, "attempt"),
            metadata(),
            json!({"valid": false}),
        )
        .unwrap();
        let second = TrialRecord::new(
            first.identity.clone(),
            metadata(),
            json!({"valid": false, "different": true}),
        )
        .unwrap();
        let mut log = ArtifactLog::open(&path).unwrap();
        log.append_trial(&first).unwrap();
        assert!(
            log.append_trial(&second)
                .unwrap_err()
                .to_string()
                .contains("duplicate logical trial identity")
        );
        drop(log);
        assert_eq!(scan(&path).unwrap().trials_by_hash.len(), 1);
    }

    #[test]
    fn marked_trials_reject_provider_contamination_and_missing_fields() {
        for contaminated in [
            json!({
                "valid": true,
                "stream_retries": 1,
                "provider_errors": [],
                "provider_error_details": []
            }),
            json!({
                "valid": true,
                "stream_retries": 0,
                "provider_errors": ["retryable"],
                "provider_error_details": []
            }),
            json!({
                "valid": true,
                "stream_retries": 0,
                "provider_errors": [],
                "provider_error_details": [{"message": "retryable"}]
            }),
            json!({"valid": true}),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("records.jsonl");
            let first = TrialRecord::new(
                identity(DescriptionVariant::Current, 0, "attempt"),
                metadata(),
                contaminated,
            )
            .unwrap();
            let second = TrialRecord::new(
                identity(DescriptionVariant::CompactV1, 1, "attempt"),
                metadata(),
                clean_runtime(),
            )
            .unwrap();
            let mut marker = PairCompleteRecord {
                schema_version: 1,
                identity: completion("attempt"),
                trial_record_hashes: [first.record_hash.clone(), second.record_hash.clone()],
                record_hash: String::new(),
            };
            marker.record_hash = marker.computed_hash().unwrap();
            let records = [
                ArtifactRecord::Trial(first),
                ArtifactRecord::Trial(second),
                ArtifactRecord::PairComplete(marker),
            ];
            let bytes = records
                .iter()
                .map(|record| format!("{}\n", serde_json::to_string(record).unwrap()))
                .collect::<String>();
            fs::write(&path, bytes).unwrap();

            assert!(scan(&path).unwrap_err().to_string().contains("invalid"));
        }
    }

    #[test]
    fn marked_trials_reject_inconsistent_or_excessive_usage() {
        let mut inconsistent = clean_runtime();
        inconsistent["usage"]["input"] = json!(1);

        let mut excessive = clean_runtime();
        excessive["usage"]["output"] = json!(201);
        excessive["usage"]["total_tokens"] = json!(201);

        for malformed in [inconsistent, excessive] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("records.jsonl");
            let first = TrialRecord::new(
                identity(DescriptionVariant::Current, 0, "attempt"),
                metadata(),
                malformed,
            )
            .unwrap();
            let second = TrialRecord::new(
                identity(DescriptionVariant::CompactV1, 1, "attempt"),
                metadata(),
                clean_runtime(),
            )
            .unwrap();
            let mut log = ArtifactLog::open(&path).unwrap();
            log.append_trial(&first).unwrap();
            log.append_trial(&second).unwrap();
            assert!(
                log.complete_pair(
                    completion("attempt"),
                    [first.record_hash.clone(), second.record_hash.clone()],
                )
                .unwrap_err()
                .to_string()
                .contains("invalid or provider-contaminated")
            );
        }
    }

    #[test]
    fn recovers_the_final_unmarked_complete_attempt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let mut log = ArtifactLog::open(&path).unwrap();
        for attempt in 0..MAX_PAIR_ATTEMPTS - 1 {
            let abandoned = TrialRecord::new(
                identity(
                    DescriptionVariant::Current,
                    0,
                    &format!("attempt-{attempt}"),
                ),
                metadata(),
                json!({"valid": false}),
            )
            .unwrap();
            log.append_trial(&abandoned).unwrap();
        }
        let attempt = format!("attempt-{}", MAX_PAIR_ATTEMPTS - 1);
        let first = TrialRecord::new(
            identity(DescriptionVariant::Current, 0, &attempt),
            metadata(),
            clean_runtime(),
        )
        .unwrap();
        let second = TrialRecord::new(
            identity(DescriptionVariant::CompactV1, 1, &attempt),
            metadata(),
            clean_runtime(),
        )
        .unwrap();
        log.append_trial(&first).unwrap();
        log.append_trial(&second).unwrap();
        drop(log);

        let state = scan(&path).unwrap();
        assert_eq!(pair_attempt_count(&state, "run", "pair"), MAX_PAIR_ATTEMPTS);
        let recoverable = recoverable_pair(&state, "schedule", &scheduled_pair())
            .unwrap()
            .unwrap();
        assert_eq!(recoverable.identity.attempt_id, attempt);
        ArtifactLog::open(&path)
            .unwrap()
            .complete_pair(recoverable.identity, recoverable.trial_record_hashes)
            .unwrap();
        assert!(completed_pair(&scan(&path).unwrap(), "schedule", &scheduled_pair()).is_ok());
    }

    #[test]
    fn blob_store_round_trips_binary_content() {
        let temp = tempfile::tempdir().unwrap();
        let store = BlobStore::new(temp.path()).unwrap();
        let bytes = [0, 255, b'\r', b'\n'];
        let hash = store.put(&bytes).unwrap();
        assert_eq!(store.put(&bytes).unwrap(), hash);
        assert_eq!(store.get(&hash).unwrap(), bytes);
    }
}
