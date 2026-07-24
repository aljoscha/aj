//! Typed suite manifest, task parameters, and suite revision validation.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::descriptions::{DescriptionVariant, load};
use crate::hash_framed;

const MANIFEST_BYTES: &[u8] = include_bytes!("../suite/manifest.json");

/// Error returned when a committed suite contract is invalid.
#[derive(Debug)]
pub struct SuiteError(pub String);

impl fmt::Display for SuiteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SuiteError {}

/// Exact rational weight used by an archetype.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RationalWeight {
    pub numerator: u32,
    pub denominator: u32,
}

/// Structural task class from the frozen suite profile.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskClass {
    CommonSingleFile,
    MultiFile,
    FileOperation,
    AmbiguousContext,
    FileBoundary,
    UncommonText,
}

/// The parameter contract associated with an archetype generator.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParameterKind {
    UniqueReplacement,
    MultilineEdit,
    Insertion,
    Removal,
    IndentationSensitive,
    NearbyChanges,
    TwoRelatedSourceFiles,
    SourcePlusTest,
    ThreeFileConfiguration,
    AddFile,
    DeleteFile,
    RenameWithContent,
    RepeatedBlocks,
    RepeatedMethods,
    EndOfFile,
    UncommonText,
}

/// One frozen archetype definition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArchetypeManifest {
    pub id: String,
    pub class: TaskClass,
    pub weight: RationalWeight,
    pub generator_id: String,
    pub verifier_id: String,
    pub allowlist_templates: Vec<String>,
    pub visible_check: bool,
    pub multiple_valid_diffs: bool,
    pub parameter_kind: ParameterKind,
}

/// Complete suite manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SuiteManifest {
    pub schema_version: u32,
    pub ignored_prefixes: Vec<String>,
    pub archetypes: Vec<ArchetypeManifest>,
}

/// Alternating uncommon-text fixture lane.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UncommonTextLane {
    ConflictMarkers,
    Crlf,
}

/// Seeded inputs required by each of the 16 fixture generators.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TaskParameters {
    UniqueReplacement {
        path: String,
        old: String,
        new: String,
        retry_count: u32,
    },
    MultilineEdit {
        path: String,
        symbol: String,
        boundary: i32,
        increment: i32,
    },
    Insertion {
        path: String,
        anchor: String,
        value: String,
    },
    Removal {
        path: String,
        key: String,
        retained_value: u32,
    },
    IndentationSensitive {
        path: String,
        section: String,
        timeout: u32,
    },
    NearbyChanges {
        path: String,
        first: String,
        second: String,
        amount: i32,
    },
    TwoRelatedSourceFiles {
        model_path: String,
        view_path: String,
        symbol: String,
        default_limit: u32,
    },
    SourcePlusTest {
        source_path: String,
        test_path: String,
        symbol: String,
        boundary: i32,
    },
    ThreeFileConfiguration {
        paths: [String; 3],
        key: String,
        values: [u32; 3],
    },
    AddFile {
        path: String,
        content_token: String,
        number: u32,
    },
    DeleteFile {
        path: String,
        number: u32,
    },
    RenameWithContent {
        old_path: String,
        new_path: String,
        old_symbol: String,
        symbol: String,
        multiplier: u32,
    },
    RepeatedBlocks {
        path: String,
        target_label: String,
        old_limit: u32,
        new_limit: u32,
    },
    RepeatedMethods {
        path: String,
        target_type: String,
        method: String,
        suffix: u32,
    },
    EndOfFile {
        path: String,
        value: String,
    },
    UncommonText {
        path: String,
        lane: UncommonTextLane,
        token: String,
        marker_width: u8,
        number: u32,
    },
}

impl TaskParameters {
    /// Returns the manifest parameter kind for this value.
    pub fn kind(&self) -> ParameterKind {
        match self {
            Self::UniqueReplacement { .. } => ParameterKind::UniqueReplacement,
            Self::MultilineEdit { .. } => ParameterKind::MultilineEdit,
            Self::Insertion { .. } => ParameterKind::Insertion,
            Self::Removal { .. } => ParameterKind::Removal,
            Self::IndentationSensitive { .. } => ParameterKind::IndentationSensitive,
            Self::NearbyChanges { .. } => ParameterKind::NearbyChanges,
            Self::TwoRelatedSourceFiles { .. } => ParameterKind::TwoRelatedSourceFiles,
            Self::SourcePlusTest { .. } => ParameterKind::SourcePlusTest,
            Self::ThreeFileConfiguration { .. } => ParameterKind::ThreeFileConfiguration,
            Self::AddFile { .. } => ParameterKind::AddFile,
            Self::DeleteFile { .. } => ParameterKind::DeleteFile,
            Self::RenameWithContent { .. } => ParameterKind::RenameWithContent,
            Self::RepeatedBlocks { .. } => ParameterKind::RepeatedBlocks,
            Self::RepeatedMethods { .. } => ParameterKind::RepeatedMethods,
            Self::EndOfFile { .. } => ParameterKind::EndOfFile,
            Self::UncommonText { .. } => ParameterKind::UncommonText,
        }
    }
}

/// Loads and validates the committed manifest.
pub fn committed_manifest() -> Result<SuiteManifest, SuiteError> {
    let manifest = serde_json::from_slice(MANIFEST_BYTES)
        .map_err(|error| SuiteError(format!("invalid manifest JSON: {error}")))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Validates all frozen suite-profile invariants.
pub fn validate_manifest(manifest: &SuiteManifest) -> Result<(), SuiteError> {
    if manifest.schema_version != 1 {
        return Err(SuiteError("schema_version must be 1".into()));
    }
    if manifest.archetypes.len() != EXPECTED.len() {
        return Err(SuiteError(
            "manifest must contain exactly 16 archetypes".into(),
        ));
    }
    validate_normalized_prefixes(&manifest.ignored_prefixes)?;

    let mut ids = HashSet::new();
    for (archetype, expected) in manifest.archetypes.iter().zip(EXPECTED) {
        if archetype.id != expected.0
            || archetype.class != expected.1
            || archetype.parameter_kind != expected.2
        {
            return Err(SuiteError(format!(
                "archetype {} does not match the frozen profile",
                archetype.id
            )));
        }
        if !ids.insert(&archetype.id) {
            return Err(SuiteError(format!(
                "duplicate archetype ID {}",
                archetype.id
            )));
        }
        if archetype.weight
            != (RationalWeight {
                numerator: 1,
                denominator: 16,
            })
        {
            return Err(SuiteError(format!(
                "{} must have weight 1/16",
                archetype.id
            )));
        }
        if archetype.generator_id != format!("{}-v1", archetype.id)
            || archetype.verifier_id != format!("{}-v1", archetype.id)
        {
            return Err(SuiteError(format!(
                "{} has an unfrozen generator or verifier",
                archetype.id
            )));
        }
        if archetype.allowlist_templates.is_empty() {
            return Err(SuiteError(format!(
                "{} has an empty allowlist",
                archetype.id
            )));
        }
        validate_normalized_prefixes(&archetype.allowlist_templates)?;
    }
    if manifest
        .archetypes
        .iter()
        .filter(|item| item.visible_check)
        .count()
        < 8
    {
        return Err(SuiteError(
            "at least eight archetypes need a visible check".into(),
        ));
    }
    if manifest
        .archetypes
        .iter()
        .filter(|item| item.multiple_valid_diffs)
        .count()
        < 4
    {
        return Err(SuiteError(
            "at least four archetypes need multiple valid diffs".into(),
        ));
    }
    Ok(())
}

fn validate_normalized_prefixes(paths: &[String]) -> Result<(), SuiteError> {
    for path in paths {
        if path.is_empty()
            || path.starts_with('/')
            || path.ends_with('/')
            || path
                .split('/')
                .any(|component| component.is_empty() || component == "." || component == "..")
            || path.contains('\\')
        {
            return Err(SuiteError(format!(
                "path is not a normalized relative path: {path}"
            )));
        }
    }
    Ok(())
}

/// Computes the canonical revision over typed canonical JSON and both descriptions.
pub fn suite_revision(manifest: &SuiteManifest) -> Result<String, SuiteError> {
    validate_manifest(manifest)?;
    let canonical = serde_json::to_vec(manifest)
        .map_err(|error| SuiteError(format!("cannot serialize manifest: {error}")))?;
    Ok(hash_framed(
        b"aj-apply-patch-eval-suite-revision-v1",
        &[
            &canonical,
            load(DescriptionVariant::Current).content.as_bytes(),
            load(DescriptionVariant::CompactV1).content.as_bytes(),
        ],
    ))
}

const EXPECTED: &[(&str, TaskClass, ParameterKind)] = &[
    (
        "unique-replacement",
        TaskClass::CommonSingleFile,
        ParameterKind::UniqueReplacement,
    ),
    (
        "multiline-edit",
        TaskClass::CommonSingleFile,
        ParameterKind::MultilineEdit,
    ),
    (
        "insertion",
        TaskClass::CommonSingleFile,
        ParameterKind::Insertion,
    ),
    (
        "removal",
        TaskClass::CommonSingleFile,
        ParameterKind::Removal,
    ),
    (
        "indentation-sensitive",
        TaskClass::CommonSingleFile,
        ParameterKind::IndentationSensitive,
    ),
    (
        "nearby-changes",
        TaskClass::CommonSingleFile,
        ParameterKind::NearbyChanges,
    ),
    (
        "two-related-source-files",
        TaskClass::MultiFile,
        ParameterKind::TwoRelatedSourceFiles,
    ),
    (
        "source-plus-test",
        TaskClass::MultiFile,
        ParameterKind::SourcePlusTest,
    ),
    (
        "three-file-configuration",
        TaskClass::MultiFile,
        ParameterKind::ThreeFileConfiguration,
    ),
    ("add-file", TaskClass::FileOperation, ParameterKind::AddFile),
    (
        "delete-file",
        TaskClass::FileOperation,
        ParameterKind::DeleteFile,
    ),
    (
        "rename-with-content",
        TaskClass::FileOperation,
        ParameterKind::RenameWithContent,
    ),
    (
        "repeated-blocks",
        TaskClass::AmbiguousContext,
        ParameterKind::RepeatedBlocks,
    ),
    (
        "repeated-methods",
        TaskClass::AmbiguousContext,
        ParameterKind::RepeatedMethods,
    ),
    (
        "end-of-file",
        TaskClass::FileBoundary,
        ParameterKind::EndOfFile,
    ),
    (
        "uncommon-text",
        TaskClass::UncommonText,
        ParameterKind::UncommonText,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_manifest_satisfies_frozen_profile() {
        let manifest = committed_manifest().unwrap();
        let counts = [
            (TaskClass::CommonSingleFile, 6),
            (TaskClass::MultiFile, 3),
            (TaskClass::FileOperation, 3),
            (TaskClass::AmbiguousContext, 2),
            (TaskClass::FileBoundary, 1),
            (TaskClass::UncommonText, 1),
        ];
        for (class, expected) in counts {
            assert_eq!(
                manifest
                    .archetypes
                    .iter()
                    .filter(|item| item.class == class)
                    .count(),
                expected
            );
        }
        assert_eq!(suite_revision(&manifest).unwrap().len(), 64);
    }

    #[test]
    fn rejects_non_normalized_allowlist() {
        let mut manifest = committed_manifest().unwrap();
        manifest.archetypes[0].allowlist_templates = vec!["../escape".into()];
        assert!(
            validate_manifest(&manifest)
                .unwrap_err()
                .to_string()
                .contains("normalized")
        );
    }
}
