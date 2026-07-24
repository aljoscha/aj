//! Binary-safe, non-following filesystem snapshots and deltas.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::suite::SuiteManifest;
use crate::{frame, sha256_hex};

/// Error returned when repository state cannot be represented safely.
#[derive(Debug)]
pub struct SnapshotError(pub String);

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SnapshotError {}

impl From<io::Error> for SnapshotError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

/// Validated normalized path prefixes omitted from a snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IgnorePrefixes(Vec<String>);

impl IgnorePrefixes {
    /// Validates manifest-declared ignore prefixes.
    pub fn from_manifest(manifest: &SuiteManifest) -> Result<Self, SnapshotError> {
        Self::new(manifest.ignored_prefixes.clone())
    }

    fn new(prefixes: Vec<String>) -> Result<Self, SnapshotError> {
        let mut seen = BTreeSet::new();
        for prefix in &prefixes {
            validate_relative(prefix)?;
            if !seen.insert(prefix) {
                return Err(SnapshotError(format!("duplicate ignore prefix: {prefix}")));
            }
        }
        Ok(Self(prefixes))
    }

    fn ignores(&self, path: &str) -> bool {
        self.0.iter().any(|prefix| {
            path == prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|tail| tail.starts_with('/'))
        })
    }
}

/// Kind of a captured filesystem entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
}

/// One binary-safe filesystem entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SnapshotEntry {
    pub path: String,
    pub kind: EntryKind,
    pub unix_mode: u32,
    pub file_length: Option<u64>,
    pub file_sha256: Option<String>,
    pub symlink_target: Option<String>,
    pub symlink_target_sha256: Option<String>,
}

/// Ordered repository snapshot and its explicitly framed root hash.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FilesystemSnapshot {
    pub entries: Vec<SnapshotEntry>,
    pub root_hash: String,
}

/// Type of change between two snapshots.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Deleted,
    Modified,
    Type,
    Mode,
}

/// Changes to one normalized path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PathChange {
    pub path: String,
    pub changes: Vec<ChangeKind>,
}

/// Deterministic delta between two roots.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SnapshotDelta {
    pub before_root_hash: String,
    pub after_root_hash: String,
    pub paths: Vec<PathChange>,
}

/// Captures regular files, directories, and symlinks without following links.
pub fn capture(root: &Path, ignores: &IgnorePrefixes) -> Result<FilesystemSnapshot, SnapshotError> {
    let root_metadata = fs::symlink_metadata(root)?;
    if !root_metadata.file_type().is_dir() || root_metadata.file_type().is_symlink() {
        return Err(SnapshotError(
            "snapshot root must be a real directory".into(),
        ));
    }
    let mut entries = Vec::new();
    let mut hard_links = HashMap::new();
    walk(root, root, ignores, &mut hard_links, &mut entries)?;
    entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    let root_hash = root_hash(&entries);
    Ok(FilesystemSnapshot { entries, root_hash })
}

fn walk(
    root: &Path,
    directory: &Path,
    ignores: &IgnorePrefixes,
    hard_links: &mut HashMap<(u64, u64), String>,
    entries: &mut Vec<SnapshotEntry>,
) -> Result<(), SnapshotError> {
    for child in fs::read_dir(directory)? {
        let child = child?;
        let path = child.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| SnapshotError("filesystem path escaped snapshot root".into()))?;
        let normalized = normalized_path(relative)?;
        if ignores.ignores(&normalized) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();
        let entry = if file_type.is_dir() {
            SnapshotEntry {
                path: normalized.clone(),
                kind: EntryKind::Directory,
                unix_mode: mode(&metadata),
                file_length: None,
                file_sha256: None,
                symlink_target: None,
                symlink_target_sha256: None,
            }
        } else if file_type.is_file() {
            if metadata.nlink() > 1 {
                let key = (metadata.dev(), metadata.ino());
                let first = hard_links.entry(key).or_insert_with(|| normalized.clone());
                return Err(SnapshotError(format!(
                    "hard link is unsupported: {normalized} shares storage with {first}"
                )));
            }
            SnapshotEntry {
                path: normalized.clone(),
                kind: EntryKind::File,
                unix_mode: mode(&metadata),
                file_length: Some(metadata.len()),
                file_sha256: Some(hash_file(&path)?),
                symlink_target: None,
                symlink_target_sha256: None,
            }
        } else if file_type.is_symlink() {
            let target = fs::read_link(&path)?;
            let target_bytes = target.as_os_str().as_bytes();
            let target_text = std::str::from_utf8(target_bytes)
                .map_err(|_| SnapshotError(format!("non-UTF8 symlink target at {normalized}")))?;
            validate_symlink_target(relative, &target)?;
            SnapshotEntry {
                path: normalized,
                kind: EntryKind::Symlink,
                unix_mode: mode(&metadata),
                file_length: None,
                file_sha256: None,
                symlink_target: Some(target_text.into()),
                symlink_target_sha256: Some(sha256_hex(target_bytes)),
            }
        } else {
            return Err(SnapshotError(format!(
                "unsupported filesystem kind at {normalized}"
            )));
        };
        entries.push(entry);
        if file_type.is_dir() {
            walk(root, &path, ignores, hard_links, entries)?;
        }
    }
    Ok(())
}

fn normalized_path(path: &Path) -> Result<String, SnapshotError> {
    let text = path
        .to_str()
        .ok_or_else(|| SnapshotError("snapshot path is not UTF-8".into()))?;
    validate_relative(text)?;
    Ok(text.into())
}

fn validate_relative(path: &str) -> Result<(), SnapshotError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(SnapshotError(format!(
            "path is not normalized and relative: {path}"
        )));
    }
    Ok(())
}

fn validate_symlink_target(link_path: &Path, target: &Path) -> Result<(), SnapshotError> {
    if target.is_absolute() {
        return Err(SnapshotError(format!(
            "absolute symlink target is a path escape: {}",
            target.display()
        )));
    }
    let mut depth = link_path
        .parent()
        .map_or(0, |parent| parent.components().count());
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir => {
                return Err(SnapshotError("symlink target escapes snapshot root".into()));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(SnapshotError("symlink target escapes snapshot root".into()));
            }
        }
    }
    Ok(())
}

fn mode(metadata: &Metadata) -> u32 {
    metadata.mode() & 0o7777
}

fn hash_file(path: &Path) -> Result<String, SnapshotError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn root_hash(entries: &[SnapshotEntry]) -> String {
    let mut hasher = Sha256::new();
    frame(&mut hasher, b"aj-apply-patch-eval-snapshot-v1");
    for entry in entries {
        frame(&mut hasher, entry.path.as_bytes());
        frame(&mut hasher, format!("{:?}", entry.kind).as_bytes());
        frame(&mut hasher, &entry.unix_mode.to_be_bytes());
        frame_option(
            &mut hasher,
            entry
                .file_length
                .map(|length| length.to_be_bytes())
                .as_ref()
                .map(<[u8; 8]>::as_slice),
        );
        frame_option(&mut hasher, entry.file_sha256.as_deref().map(str::as_bytes));
        frame_option(
            &mut hasher,
            entry.symlink_target.as_deref().map(str::as_bytes),
        );
        frame_option(
            &mut hasher,
            entry.symlink_target_sha256.as_deref().map(str::as_bytes),
        );
    }
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn frame_option(hasher: &mut Sha256, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            frame(hasher, b"some");
            frame(hasher, value);
        }
        None => frame(hasher, b"none"),
    }
}

/// Computes added, deleted, modified, type, and mode changes.
pub fn delta(before: &FilesystemSnapshot, after: &FilesystemSnapshot) -> SnapshotDelta {
    let before_by_path = before
        .entries
        .iter()
        .map(|entry| (&entry.path, entry))
        .collect::<BTreeMap<_, _>>();
    let after_by_path = after
        .entries
        .iter()
        .map(|entry| (&entry.path, entry))
        .collect::<BTreeMap<_, _>>();
    let paths = before_by_path
        .keys()
        .chain(after_by_path.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut changed = Vec::new();
    for path in paths {
        let changes = match (before_by_path.get(path), after_by_path.get(path)) {
            (None, Some(_)) => vec![ChangeKind::Added],
            (Some(_), None) => vec![ChangeKind::Deleted],
            (Some(before_entry), Some(after_entry)) => compare_entries(before_entry, after_entry),
            (None, None) => unreachable!(),
        };
        if !changes.is_empty() {
            changed.push(PathChange {
                path: path.clone(),
                changes,
            });
        }
    }
    SnapshotDelta {
        before_root_hash: before.root_hash.clone(),
        after_root_hash: after.root_hash.clone(),
        paths: changed,
    }
}

fn compare_entries(before: &SnapshotEntry, after: &SnapshotEntry) -> Vec<ChangeKind> {
    let mut changes = Vec::new();
    if before.kind != after.kind {
        changes.push(ChangeKind::Type);
    } else if before.file_length != after.file_length
        || before.file_sha256 != after.file_sha256
        || before.symlink_target != after.symlink_target
        || before.symlink_target_sha256 != after.symlink_target_sha256
    {
        changes.push(ChangeKind::Modified);
    }
    if before.unix_mode != after.unix_mode {
        changes.push(ChangeKind::Mode);
    }
    changes
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;

    use super::*;

    #[test]
    fn preserves_binary_crlf_symlink_and_executable_metadata() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("binary"), [0, 255, 1]).unwrap();
        fs::write(temp.path().join("crlf"), b"a\r\nb\r\n").unwrap();
        fs::write(temp.path().join("run"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(temp.path().join("run"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("binary", temp.path().join("link")).unwrap();

        let snapshot = capture(temp.path(), &IgnorePrefixes::new(vec![]).unwrap()).unwrap();
        let binary = snapshot
            .entries
            .iter()
            .find(|entry| entry.path == "binary")
            .unwrap();
        assert_eq!(binary.file_length, Some(3));
        let crlf = snapshot
            .entries
            .iter()
            .find(|entry| entry.path == "crlf")
            .unwrap();
        assert_eq!(crlf.file_length, Some(6));
        let link = snapshot
            .entries
            .iter()
            .find(|entry| entry.path == "link")
            .unwrap();
        assert_eq!(link.symlink_target.as_deref(), Some("binary"));
        let executable = snapshot
            .entries
            .iter()
            .find(|entry| entry.path == "run")
            .unwrap();
        assert_eq!(executable.unix_mode, 0o755);
    }

    #[test]
    fn reports_all_delta_categories() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("deleted"), b"x").unwrap();
        fs::write(temp.path().join("modified"), b"before").unwrap();
        fs::write(temp.path().join("mode"), b"x").unwrap();
        fs::write(temp.path().join("type"), b"x").unwrap();
        let ignores = IgnorePrefixes::new(vec![]).unwrap();
        let before = capture(temp.path(), &ignores).unwrap();

        fs::remove_file(temp.path().join("deleted")).unwrap();
        fs::write(temp.path().join("added"), b"x").unwrap();
        fs::write(temp.path().join("modified"), b"after").unwrap();
        fs::set_permissions(temp.path().join("mode"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(temp.path().join("type")).unwrap();
        fs::create_dir(temp.path().join("type")).unwrap();
        let changes = delta(&before, &capture(temp.path(), &ignores).unwrap());
        let find = |path| {
            changes
                .paths
                .iter()
                .find(|change| change.path == path)
                .unwrap()
        };
        assert_eq!(find("added").changes, [ChangeKind::Added]);
        assert_eq!(find("deleted").changes, [ChangeKind::Deleted]);
        assert_eq!(find("modified").changes, [ChangeKind::Modified]);
        assert_eq!(find("type").changes, [ChangeKind::Type, ChangeKind::Mode]);
        assert_eq!(find("mode").changes, [ChangeKind::Mode]);
    }

    #[test]
    fn ignores_only_component_prefixes() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("target")).unwrap();
        fs::write(temp.path().join("target/output"), b"ignored").unwrap();
        fs::write(temp.path().join("targeted"), b"kept").unwrap();
        let snapshot = capture(
            temp.path(),
            &IgnorePrefixes::new(vec!["target".into()]).unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].path, "targeted");
        assert!(IgnorePrefixes::new(vec!["../outside".into()]).is_err());
    }

    #[test]
    fn rejects_symlink_escape_and_hard_links() {
        let temp = tempfile::tempdir().unwrap();
        symlink("../outside", temp.path().join("escape")).unwrap();
        assert!(capture(temp.path(), &IgnorePrefixes::new(vec![]).unwrap()).is_err());
        fs::remove_file(temp.path().join("escape")).unwrap();
        fs::write(temp.path().join("first"), b"x").unwrap();
        fs::hard_link(temp.path().join("first"), temp.path().join("second")).unwrap();
        assert!(
            capture(temp.path(), &IgnorePrefixes::new(vec![]).unwrap())
                .unwrap_err()
                .to_string()
                .contains("hard link")
        );
    }

    #[test]
    fn rejects_non_utf8_paths_and_unsupported_kinds() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(OsString::from_vec(vec![0xff])), b"x").unwrap();
        assert!(capture(temp.path(), &IgnorePrefixes::new(vec![]).unwrap()).is_err());
        fs::remove_file(temp.path().join(OsString::from_vec(vec![0xff]))).unwrap();

        let _listener = UnixListener::bind(temp.path().join("socket")).unwrap();
        assert!(
            capture(temp.path(), &IgnorePrefixes::new(vec![]).unwrap())
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
    }
}
