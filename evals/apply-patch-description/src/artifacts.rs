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
use crate::schedule::SchedulePhase;
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
            schema_version: 1,
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
            b"aj-apply-patch-eval-trial-record-v1",
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
            if trial.schema_version != 1 || trial.record_hash != trial.computed_hash()? {
                return Err(ArtifactError(format!("invalid trial hash at line {line}")));
            }
            if trial.identity.order_index > 1 {
                return Err(ArtifactError(format!("invalid trial order at line {line}")));
            }
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

/// Writer that flushes and syncs every complete JSONL line.
pub struct ArtifactLog {
    path: PathBuf,
    file: File,
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
        Ok(Self { path, file })
    }

    /// Appends and synchronizes one immutable trial record.
    pub fn append_trial(&mut self, trial: &TrialRecord) -> Result<(), ArtifactError> {
        if trial.record_hash != trial.computed_hash()? {
            return Err(ArtifactError(
                "refusing to append a trial with an invalid hash".into(),
            ));
        }
        self.append(&ArtifactRecord::Trial(trial.clone()))
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
            json!({}),
        )
        .unwrap();
        let second = TrialRecord::new(
            identity(DescriptionVariant::CompactV1, 1, "attempt-2"),
            metadata(),
            json!({}),
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
            json!({}),
        )
        .unwrap();
        let second = TrialRecord::new(
            identity(DescriptionVariant::CompactV1, 1, "attempt"),
            metadata(),
            json!({}),
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
    fn blob_store_round_trips_binary_content() {
        let temp = tempfile::tempdir().unwrap();
        let store = BlobStore::new(temp.path()).unwrap();
        let bytes = [0, 255, b'\r', b'\n'];
        let hash = store.put(&bytes).unwrap();
        assert_eq!(store.put(&bytes).unwrap(), hash);
        assert_eq!(store.get(&hash).unwrap(), bytes);
    }
}
