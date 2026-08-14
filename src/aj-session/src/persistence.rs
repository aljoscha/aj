//! Project-level discovery of conversation session files.
//!
//! [`ConversationPersistence`] is the owner of a project's sessions
//! directory. It lists existing sessions (for `aj list-sessions` and
//! `aj continue`) and resolves a session id to its on-disk path so
//! [`crate::log::ConversationLog`] can open / create the right file.

use aj_models::types::{Message, UserContent};
use chrono::{DateTime, NaiveDateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use crate::log::{ConversationEntry, ConversationEntryKind, ConversationError};

/// The extension of the sidecar holding a session's label.
const TAG_SIDECAR: &str = "tag";

/// The extension of the sidecar whose existence archives a session.
const ARCHIVED_SIDECAR: &str = "archived";

/// Handles persistence operations for conversations, including listing
/// existing session files and resolving their paths.
#[derive(Clone)]
pub struct ConversationPersistence {
    sessions_dir: PathBuf,
}

impl ConversationPersistence {
    /// Create a new [ConversationPersistence] instance with the given
    /// sessions directory.
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    pub fn sessions_dir(&self) -> &std::path::Path {
        &self.sessions_dir
    }

    pub(crate) fn session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.jsonl"))
    }

    /// The directory holding session sidecars, `meta/` under the store.
    ///
    /// The location is fixed by spec 6.8. Its payoff is that one directory read
    /// finds every sidecar without walking the logs, which is what makes an
    /// untagged store cost nothing and a tagged one cost one read per label
    /// rather than a `stat` per session.
    fn meta_dir(&self) -> PathBuf {
        self.sessions_dir.join("meta")
    }

    /// A session's sidecar path under `extension`, or `None` for an id the
    /// grammar rejects.
    ///
    /// An invalid id must not become a path (see [`crate::id`]), and these are
    /// write paths, so the check is not optional.
    fn sidecar_path(&self, session_id: &str, extension: &str) -> Option<PathBuf> {
        crate::id::is_valid_session_id(session_id)
            .then(|| self.meta_dir().join(format!("{session_id}.{extension}")))
    }

    /// The session's tag, `Ok(None)` when it has none.
    ///
    /// One open and one read of a small file. Callers that list a whole store
    /// go through [`Self::enumerate_tags`] instead, which finds the sidecars
    /// that exist in a single directory read.
    pub fn read_tag(&self, session_id: &str) -> Result<Option<String>, ConversationError> {
        let Some(path) = self.sidecar_path(session_id, TAG_SIDECAR) else {
            return Ok(None);
        };
        match fs::read_to_string(&path) {
            Ok(contents) => Ok(crate::tag::tag_from_sidecar(&contents)),
            // No sidecar is the untagged case, not a failure.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Set or clear the session's tag. `None` removes the sidecar.
    ///
    /// The write is atomic: the body lands in a temporary file in the same
    /// directory and is renamed over the target, so a reader sees either the
    /// old tag or the new one and never a torn line. Callers hold the session's
    /// lock (spec 6.6), which is what orders two writers.
    ///
    /// `tag` is expected to have been through [`crate::tag::normalize_tag`]
    /// already, so this writes what it is given.
    ///
    /// An id the grammar rejects is an error rather than a quiet no-op. It
    /// cannot name a sidecar in this store, and a caller that took the `Ok`
    /// for a write would go on to publish a label the store does not hold:
    /// the tag command sets the session's row on the strength of this
    /// returning.
    pub fn write_tag(&self, session_id: &str, tag: Option<&str>) -> Result<(), ConversationError> {
        let Some(path) = self.sidecar_path(session_id, TAG_SIDECAR) else {
            return Err(ConversationError::InvalidSessionId(session_id.to_string()));
        };
        let Some(tag) = tag else {
            match fs::remove_file(&path) {
                Ok(()) => return Ok(()),
                // Already untagged, which is what the caller asked for.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(err.into()),
            }
        };
        fs::create_dir_all(self.meta_dir())?;
        let mut temp = tempfile::Builder::new()
            .prefix(".tag-")
            .tempfile_in(self.meta_dir())?;
        std::io::Write::write_all(&mut temp, tag.as_bytes())?;
        std::io::Write::write_all(&mut temp, b"\n")?;
        temp.persist(&path).map_err(|err| err.error)?;
        Ok(())
    }

    /// Whether the session is archived.
    ///
    /// One `stat` and no read at all: the sidecar's existence is the whole
    /// answer. Callers that list a whole store go through
    /// [`Self::enumerate_archived`] instead, which finds the sidecars that
    /// exist in a single directory read.
    pub fn read_archived(&self, session_id: &str) -> Result<bool, ConversationError> {
        let Some(path) = self.sidecar_path(session_id, ARCHIVED_SIDECAR) else {
            return Ok(false);
        };
        match fs::metadata(&path) {
            // A directory under the name is not a sidecar, which is the answer
            // an enumeration gives for it too.
            Ok(metadata) => Ok(metadata.is_file()),
            // No sidecar is the unarchived case, not a failure.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// Set or clear the session's archived bit by creating or removing its
    /// sidecar.
    ///
    /// The existence of the file is the bit, so there is no content to tear:
    /// the create and the remove each publish the whole answer in one
    /// operation, and a reader sees one state or the other. Callers hold the
    /// session's lock, which is what orders two writers.
    ///
    /// An id the grammar rejects is an error rather than a quiet no-op, for
    /// the reason [`Self::write_tag`] gives: the archive command sets the
    /// session's row on the strength of this returning.
    pub fn write_archived(
        &self,
        session_id: &str,
        archived: bool,
    ) -> Result<(), ConversationError> {
        let Some(path) = self.sidecar_path(session_id, ARCHIVED_SIDECAR) else {
            return Err(ConversationError::InvalidSessionId(session_id.to_string()));
        };
        if !archived {
            return match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                // Already unarchived, which is what the caller asked for.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                // A directory under the sidecar's name is not the bit either
                // (see [`Self::read_archived`]), so the session already reads
                // as unarchived and the remove had nothing to do. Raising here
                // would refuse the state the store is in, and go on refusing
                // it.
                Err(_) if fs::metadata(&path).is_ok_and(|meta| !meta.is_file()) => Ok(()),
                Err(err) => Err(err.into()),
            };
        }
        fs::create_dir_all(self.meta_dir())?;
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(_) => Ok(()),
            // Already archived, and the file holds nothing to bring up to
            // date, so this leaves it exactly as it is.
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Every tag sidecar in the store, with the fingerprint of the file it was
    /// read from.
    ///
    /// One directory read of `meta/` plus one `stat` per sidecar, and no
    /// sidecar contents at all: the label itself is read by
    /// [`Self::read_tag`], once per fingerprint this reports. An untagged
    /// store has no `meta/` directory and costs a single failed `read_dir`,
    /// which is what makes the untagged case free (spec 6.8): a caller cannot
    /// ask per session without paying a `stat` per session.
    pub fn enumerate_tags(&self) -> Result<Vec<SidecarMetadata>, ConversationError> {
        self.enumerate_sidecars(TAG_SIDECAR)
    }

    /// Every archived sidecar in the store, with the fingerprint of the file
    /// it was found at.
    ///
    /// The same shape as [`Self::enumerate_tags`], for the axis whose answer is
    /// the file's existence: one directory read for the whole listing and no
    /// sidecar opened at all. A store with no `meta/` directory pays a single
    /// failed `read_dir`, and one that has the directory pays a read of it per
    /// axis, never a `stat` per session.
    pub fn enumerate_archived(&self) -> Result<Vec<SidecarMetadata>, ConversationError> {
        self.enumerate_sidecars(ARCHIVED_SIDECAR)
    }

    /// The sidecars under `extension` the store holds.
    ///
    /// One directory read per axis, which is what keeps a listing off the
    /// per-session `stat` a caller asking session by session would pay.
    fn enumerate_sidecars(
        &self,
        extension: &str,
    ) -> Result<Vec<SidecarMetadata>, ConversationError> {
        let meta = self.meta_dir();
        let entries = match fs::read_dir(&meta) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut sidecars = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some(extension) {
                continue;
            }
            let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !crate::id::is_valid_session_id(session_id) {
                continue;
            }
            let Ok(metadata) = fs::metadata(&path) else {
                // Vanished since the directory read.
                continue;
            };
            // A directory named like a sidecar is not one, and offering it
            // would cost a failed open at every enumeration for the life of
            // the store: the read fails, so nothing is cached, so it is tried
            // again next time.
            if !metadata.is_file() {
                continue;
            }
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            sidecars.push(SidecarMetadata {
                session_id: session_id.to_string(),
                modified_at: modified.into(),
                size_bytes: metadata.len(),
            });
        }
        Ok(sidecars)
    }

    /// The label of every tagged session in the store, keyed by session id.
    ///
    /// Driven by [`Self::enumerate_tags`], so the cost is one directory read
    /// plus one small read per sidecar that exists. An untagged store has no
    /// `meta/` directory and pays a single failed `read_dir` with no
    /// per-session read at all (spec 6.8), which is why this cannot be a loop
    /// over the sessions asking each for its tag.
    ///
    /// A label that cannot be read is dropped rather than raised: a listing
    /// carries labels as display metadata, and one unreadable sidecar must not
    /// cost the caller its rows.
    fn tags_by_session(&self) -> HashMap<String, String> {
        let sidecars = match self.enumerate_tags() {
            Ok(sidecars) => sidecars,
            Err(err) => {
                tracing::debug!(
                    "could not enumerate tag sidecars in {}: {err}",
                    self.meta_dir().display()
                );
                return HashMap::new();
            }
        };
        sidecars
            .into_iter()
            .filter_map(|sidecar| {
                let tag = self.read_tag(&sidecar.session_id).ok().flatten()?;
                Some((sidecar.session_id, tag))
            })
            .collect()
    }

    /// The id of every archived session in the store.
    ///
    /// Driven by [`Self::enumerate_archived`], so the cost is one directory
    /// read and nothing per session: the listing is the answer, since the
    /// sidecar's existence is the bit.
    ///
    /// A directory that cannot be read reports nothing archived rather than
    /// raising, for the reason [`Self::tags_by_session`] gives: the bit is
    /// display metadata and must not cost a listing its rows.
    fn archived_sessions(&self) -> HashSet<String> {
        match self.enumerate_archived() {
            Ok(sidecars) => sidecars
                .into_iter()
                .map(|sidecar| sidecar.session_id)
                .collect(),
            Err(err) => {
                tracing::debug!(
                    "could not enumerate archived sidecars in {}: {err}",
                    self.meta_dir().display()
                );
                HashSet::new()
            }
        }
    }

    /// Get metadata about all conversation sessions, sorted by creation
    /// time (latest first).
    ///
    /// Files whose first line does not parse as the new
    /// [ConversationEntry] shape (e.g. pre-refactor sessions) are skipped
    /// with a `tracing::info!` note, and so is a file that cannot be read at
    /// all: one bad file must not fail the listing. A file whose stem is not
    /// a session id is skipped before that, by
    /// [`Self::enumerate_sessions`].
    pub fn list_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
        let mut sessions = self.enumerate_sessions()?;
        sessions.retain(|metadata| {
            let path = self.session_path(&metadata.session_id);
            match self.is_current_format(&metadata.session_id) {
                Some(true) => true,
                Some(false) => {
                    tracing::info!(
                        "skipping pre-refactor session file {} (old on-disk format)",
                        path.display()
                    );
                    false
                }
                None => {
                    tracing::warn!("skipping unreadable session file {}", path.display());
                    false
                }
            }
        });
        Ok(sessions)
    }

    /// Every `.jsonl` file in the sessions directory whose stem is a session
    /// id, latest first, with the facts a `stat` yields.
    ///
    /// A stem the grammar rejects (see [`crate::id`]) is skipped: this store
    /// refuses that id at every later lookup, so listing it would put a row in
    /// every directory that nothing can resolve.
    ///
    /// No file is opened, so this says nothing about a log's format:
    /// [`Self::is_current_format`] is that gate, applied separately. The split
    /// is what lets a caller that refreshes a listing often cache the gate's
    /// verdict, which is a read, while re-running the enumeration, which is
    /// not.
    ///
    /// A file that vanishes or turns unreadable between the directory read and
    /// its `stat` is skipped rather than failing the enumeration: a listing
    /// must not break over one file, least of all one that is no longer there.
    pub fn enumerate_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.sessions_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // A stem the grammar rejects is not a session this store can be
            // asked about later, so listing it would put a row in every
            // directory that no attach could ever resolve.
            if !crate::id::is_valid_session_id(session_id) {
                continue;
            }
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(err) => {
                    // Vanished or turned unreadable since the directory read.
                    tracing::debug!("skipping session file {}: {err}", path.display());
                    continue;
                }
            };
            if !metadata.is_file() {
                continue;
            }
            let Ok(modified) = metadata.modified() else {
                tracing::debug!(
                    "skipping session file {}: no modification time",
                    path.display()
                );
                continue;
            };
            sessions.push(SessionMetadata::new(
                session_id.to_string(),
                modified.into(),
                metadata.len(),
            ));
        }

        // Filenames are timestamps, so reverse-lexicographic is latest first.
        sessions.sort_by(|left, right| right.session_id.cmp(&left.session_id));
        Ok(sessions)
    }

    /// The `stat` facts for one session's log, `Ok(None)` when the store
    /// holds no log under that id.
    ///
    /// The single-id form of [`Self::enumerate_sessions`], for the membership
    /// question a lookup asks. It costs one `stat` rather than a directory
    /// read, which is what keeps "is this id one of mine" off the size of the
    /// store.
    ///
    /// An id the grammar rejects answers `Ok(None)` without touching the
    /// filesystem: it cannot name a log in this store, and turning it into a
    /// path is exactly what must not happen (see [`crate::id`]).
    ///
    /// Absence and failure are separate answers. A store this process cannot
    /// read is not a store without the session in it, and a caller that
    /// conflated the two would report a session it holds a directory row for
    /// as gone.
    pub fn session_metadata(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionMetadata>, ConversationError> {
        if !crate::id::is_valid_session_id(session_id) {
            return Ok(None);
        }
        let path = self.session_path(session_id);
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(ConversationError::Io(err)),
        };
        if !metadata.is_file() {
            return Ok(None);
        }
        let modified = metadata.modified().map_err(ConversationError::Io)?;
        Ok(Some(SessionMetadata::new(
            session_id.to_string(),
            modified.into(),
            metadata.len(),
        )))
    }

    /// Whether `session_id`'s log is in the current on-disk format, or `None`
    /// when the log could not be read at all.
    ///
    /// An empty log counts as current (it was just created and nothing has been
    /// written yet). Otherwise its first non-empty line must parse as a
    /// [`ConversationEntry`]. The line is read as bytes, so a log that is not
    /// valid UTF-8 earns a verdict (it is not the current format) rather than a
    /// read failure.
    ///
    /// The `None` case is separate because a caller that caches the verdict
    /// must not cache it: the format is a durable property of the log's
    /// content, while a failure to open the file says nothing about the log and
    /// can clear on its own. An id the grammar rejects reads as `None` too: it
    /// names no log here, and it must not be turned into a path to find that
    /// out (see [`crate::id`]).
    pub fn is_current_format(&self, session_id: &str) -> Option<bool> {
        if !crate::id::is_valid_session_id(session_id) {
            return None;
        }
        let path = self.session_path(session_id);
        let file = File::open(&path).ok()?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                // An empty file is fine.
                Ok(0) => return Some(true),
                Ok(_) => {
                    let trimmed = line.trim_ascii();
                    if trimmed.is_empty() {
                        continue;
                    }
                    return Some(serde_json::from_slice::<ConversationEntry>(trimmed).is_ok());
                }
                Err(_) => return None,
            }
        }
    }

    /// Get the latest conversation session ID, if any exist.
    pub fn get_latest_session_id(&self) -> Result<Option<String>, ConversationError> {
        let sessions = self.list_sessions()?;
        Ok(sessions.first().map(|t| t.session_id.clone()))
    }

    /// List sessions with rich per-session previews — first user
    /// message, message count, modified time, file size.
    ///
    /// Walks the sessions directory in the same latest-first order
    /// as [`Self::list_sessions`], but for each file opens the JSONL
    /// and scans line by line to count `Message` entries and capture
    /// the first user-role textual block. `on_progress(loaded, total)`
    /// fires once per file as previews complete so a caller showing
    /// a "Loading X/Y" indicator can update incrementally. Files
    /// whose first line does not parse as the new [`ConversationEntry`]
    /// shape are skipped (consistent with [`Self::list_sessions`]).
    ///
    /// Note on streaming: this function returns the previews in one
    /// `Vec` after every file has been scanned. The callback is the
    /// streaming surface for progress reporting; callers that want to
    /// render rows as they are scanned (rather than blocking on the
    /// full walk) use [`Self::list_session_previews_streaming`]
    /// instead.
    pub fn list_session_previews(
        &self,
        mut on_progress: impl FnMut(usize, usize),
    ) -> Result<Vec<SessionPreview>, ConversationError> {
        let candidates = self.preview_candidates()?;
        let mut tags = self.tags_by_session();
        let archived = self.archived_sessions();
        let total = candidates.len();
        let mut previews = Vec::with_capacity(total);
        for (i, (session_id, path)) in candidates.into_iter().enumerate() {
            if let Some(mut preview) = read_preview(session_id, &path, &|| false) {
                preview.tag = tags.remove(&preview.session_id);
                preview.archived = archived.contains(&preview.session_id);
                previews.push(preview);
            }
            // Tick progress for every file, including the pre-refactor
            // ones that produced no row, so the counter reaches `total`.
            on_progress(i + 1, total);
        }
        Ok(previews)
    }

    /// Stream per-session previews to `emit`, one file's preview per
    /// call, in the same latest-first order as
    /// [`Self::list_session_previews`]. Each call carries a
    /// single-element batch so a UI rendering the list incrementally
    /// can append rows as the scan progresses rather than blocking on
    /// the whole walk.
    ///
    /// Mirrors the failure tolerance of [`Self::list_session_previews`]:
    /// a pre-refactor or unreadable file is skipped (no row emitted),
    /// and a missing or unreadable sessions directory emits nothing.
    /// `cancel` is polled between files and periodically within a file so
    /// the scan, which runs on the blocking pool and can't be aborted,
    /// bails promptly once the consumer (the selector overlay) goes away.
    /// It should be sticky: once it returns true the scan stops, and a file
    /// interrupted mid-read is dropped rather than emitted as a partial row.
    /// Pass `&|| false` for an uninterruptible scan.
    pub fn list_session_previews_streaming(
        &self,
        cancel: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(Vec<SessionPreview>),
    ) {
        let candidates = match self.preview_candidates() {
            Ok(c) => c,
            Err(err) => {
                tracing::debug!(
                    "could not enumerate sessions dir {}: {err}",
                    self.sessions_dir.display()
                );
                return;
            }
        };
        // Read once up front rather than per file: both axes are a single
        // directory read of `meta/` (see [`Self::tags_by_session`] and
        // [`Self::archived_sessions`]), and the walk they annotate is the same
        // snapshot of the store.
        let mut tags = self.tags_by_session();
        let archived = self.archived_sessions();
        for (session_id, path) in candidates {
            if cancel() {
                break;
            }
            if let Some(mut preview) = read_preview(session_id, &path, cancel) {
                // A mid-file cancel leaves `read_preview` with a partial
                // count, so re-check before emitting: a sticky `cancel` is
                // true here and we drop the partial rather than show a row
                // with a truncated message count.
                if cancel() {
                    break;
                }
                preview.tag = tags.remove(&preview.session_id);
                preview.archived = archived.contains(&preview.session_id);
                emit(vec![preview]);
            }
        }
    }

    /// Enumerate the session files worth previewing, newest-first.
    ///
    /// Every `.jsonl` file is a candidate. The current-format check runs
    /// inline in the per-file walk ([`read_session_preview_file`]), so
    /// each file is opened once rather than once to check the format and
    /// again to read the preview. A pre-refactor file is dropped during
    /// that walk, so the progress total counts it but no row appears for
    /// it, and the counter still reaches the total.
    fn preview_candidates(&self) -> Result<Vec<(String, PathBuf)>, ConversationError> {
        Ok(self
            .enumerate_sessions()?
            .into_iter()
            .map(|metadata| {
                let path = self.session_path(&metadata.session_id);
                (metadata.session_id, path)
            })
            .collect())
    }
}

/// Read a preview for `path`.
///
/// `Ok(None)` means a pre-refactor file (its first non-empty line is not
/// the current [`ConversationEntry`] shape). It is dropped from the
/// listing, matching the format gate [`ConversationPersistence::list_sessions`]
/// applies. A read error (the file vanished or became unreadable between
/// enumeration and the open) also drops it, the same way `list_sessions`
/// does, so the two listings stay consistent.
fn read_preview(
    session_id: String,
    path: &std::path::Path,
    cancel: &dyn Fn() -> bool,
) -> Option<SessionPreview> {
    match read_session_preview_file(&session_id, path, cancel) {
        Ok(Some(preview)) => Some(preview),
        Ok(None) => {
            tracing::info!(
                "skipping pre-refactor session file {} (old on-disk format)",
                path.display()
            );
            None
        }
        Err(err) => {
            tracing::warn!("skipping unreadable session file {}: {err}", path.display());
            None
        }
    }
}

/// A session sidecar the store holds, with the file state it was found at.
///
/// The fingerprint is what lets a caller cache what it derived from the file: a
/// sidecar whose modification time and size have not moved holds the same
/// answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarMetadata {
    pub session_id: String,
    pub modified_at: DateTime<Utc>,
    pub size_bytes: u64,
}

/// Metadata about a conversation session.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    pub session_id: String,
    pub modified_at: DateTime<Utc>,
    /// File size in bytes. Paired with `modified_at` it fingerprints the
    /// file, which is what lets a caller cache anything it derived from
    /// the file's contents (see
    /// [`ConversationPersistence::is_current_format`]).
    pub size_bytes: u64,
}

impl SessionMetadata {
    /// Assemble a row from what a `stat` of the session's log yields.
    pub fn new(session_id: String, modified_at: DateTime<Utc>, size_bytes: u64) -> Self {
        Self {
            session_id,
            modified_at,
            size_bytes,
        }
    }

    /// The modification time, formatted for the `list-sessions` output.
    ///
    /// Derived on demand rather than held: a listing that only fingerprints
    /// files (the session host's directory refresh) enumerates the whole store
    /// on a timer and never renders a row.
    pub fn modified_display(&self) -> String {
        self.modified_at.format("%Y-%m-%d %H:%M:%S UTC").to_string()
    }

    /// The file size, formatted for the `list-sessions` output. Size stands in
    /// for conversation length.
    pub fn size_display(&self) -> String {
        if self.size_bytes < 1024 {
            format!("{}B", self.size_bytes)
        } else if self.size_bytes < 1024 * 1024 {
            format!("{}KB", self.size_bytes / 1024)
        } else {
            format!("{}MB", self.size_bytes / (1024 * 1024))
        }
    }
}

/// Richer per-session snapshot used by the interactive session
/// selector overlay.
///
/// Unlike [`SessionMetadata`] (which is purely a filesystem-stat
/// payload), [`SessionPreview`] opens the JSONL and walks far enough
/// to count `Message` entries and capture the first user-role text
/// block. Producing one preview is therefore O(file size) per
/// session; [`ConversationPersistence::list_session_previews`] streams
/// progress through a callback so a UI rendering the list can show
/// a `Loading X/Y` indicator while the walk completes.
#[derive(Debug, Clone)]
pub struct SessionPreview {
    /// Filename stem of the session file (e.g.
    /// `2025-05-11-14-22-03-512`).
    pub session_id: String,
    /// Modification time read from the file system. Held as a
    /// real [`DateTime`] (not a pre-formatted string) so the
    /// renderer can choose whatever date/age formatting it likes.
    pub modified: DateTime<Utc>,
    /// Session creation time. Parsed from `session_id` (which is
    /// minted as a millisecond-precision UTC timestamp on
    /// [`crate::log::ConversationLog::create`]). Falls back to
    /// `modified` when the id doesn't parse, so hand-renamed files or a
    /// future filename format this build doesn't recognise still
    /// produce a structurally complete row.
    pub created_at: DateTime<Utc>,
    /// Time of the most recently appended message-kind entry.
    /// Captured during the JSONL walk in
    /// [`read_session_preview_file`] as the largest
    /// [`ConversationEntry::timestamp`] seen on a
    /// [`ConversationEntryKind::Message`] entry, so out-of-order
    /// writes (e.g. a tool result that completes after a streaming
    /// assistant message finalised) still resolve to the true
    /// most-recent message rather than the last line of the file.
    /// Falls back to `modified` when no entry carries a timestamp
    /// (logs predating the timestamping work) or no message-kind
    /// entry has been appended yet.
    pub last_message_at: DateTime<Utc>,
    /// On-disk size in bytes. Cheap to surface from the
    /// `fs::metadata` we already had to call.
    pub size_bytes: u64,
    /// Number of [`ConversationEntryKind::Message`] entries in the
    /// log. User, assistant, and tool_result messages all
    /// contribute; non-message entries (`SystemPrompt`) are
    /// skipped.
    pub message_count: usize,
    /// First user-role textual content block in the file, if any.
    /// `None` for a freshly-minted session that hasn't yet seen a
    /// user prompt. The string carries the verbatim text — the
    /// renderer applies its own truncation policy.
    pub first_user_message: Option<String>,
    /// The label the session carries, `None` when it has none.
    ///
    /// It lives in a tag sidecar rather than in the log, so the listing
    /// fills it from one read of the sidecar directory (see
    /// [`ConversationPersistence::list_session_previews_streaming`]) and the
    /// per-file walk leaves it unset.
    pub tag: Option<String>,
    /// Whether the session is archived.
    ///
    /// Its sidecar's existence is the bit, so the listing fills it from the
    /// same directory read the labels come from and the per-file walk leaves
    /// it false.
    pub archived: bool,
}

/// Open `path`, walk every JSONL line, and assemble a
/// [`SessionPreview`].
///
/// Returns `Ok(None)` when the first non-empty line does not parse as a
/// [`ConversationEntry`], i.e. a pre-refactor file the listing should
/// drop. This is the current-format gate applied inline so the file is
/// opened once (the standalone [`ConversationPersistence::is_current_format`]
/// check stays for `list_sessions`, which doesn't otherwise read the
/// file). A later line that fails to parse is skipped (matching the
/// resume-time tolerance for truncated trailing lines). The walk is
/// one-pass: we read every line so `message_count` is accurate, but we
/// stop updating `first_user_message` once we have one.
fn read_session_preview_file(
    session_id: &str,
    path: &std::path::Path,
    cancel: &dyn Fn() -> bool,
) -> Result<Option<SessionPreview>, ConversationError> {
    let metadata = fs::metadata(path)?;
    let modified = metadata
        .modified()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now());
    let size_bytes = metadata.len();

    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut message_count = 0usize;
    let mut first_user_message: Option<String> = None;
    // Track the largest message-kind timestamp seen so far. Tracking
    // the max (not the last) lets the field tolerate out-of-order
    // writes: a tool result that lands after a streaming assistant
    // message finalised, for example.
    let mut last_message_at: Option<DateTime<Utc>> = None;
    let mut seen_first_entry = false;

    for (lineno, line_res) in reader.lines().enumerate() {
        // Cooperative cancellation: this runs on the blocking pool, so we
        // poll `cancel` and stop reading once the consumer is gone. We may
        // break with a partial count; the streaming caller re-checks
        // `cancel` before emitting and drops it, so no truncated row shows.
        if lineno % crate::SCAN_CANCEL_CHECK_LINES == 0 && cancel() {
            break;
        }
        // A best-effort `Ok(_)`-only path: an IO error mid-file
        // shouldn't mask the entries we already accumulated. Same
        // policy as the resume tolerance for truncated lines.
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let entry = match serde_json::from_str::<ConversationEntry>(&line) {
            Ok(entry) => entry,
            Err(_) if !seen_first_entry => {
                // First non-empty line isn't the current entry shape: a
                // pre-refactor file. Skip the whole file.
                return Ok(None);
            }
            // A later torn/garbage line: skip it, keep what we have.
            Err(_) => continue,
        };
        seen_first_entry = true;
        if let ConversationEntryKind::Message { message: msg } = &entry.entry {
            message_count += 1;
            if first_user_message.is_none() {
                if let Some(Message::User(u)) = msg.as_stored_wire() {
                    if let Some(text) = first_user_text(&u.content) {
                        first_user_message = Some(text);
                    }
                }
            }
            if let Some(ts) = entry.timestamp {
                last_message_at = Some(match last_message_at {
                    Some(prev) if prev >= ts => prev,
                    _ => ts,
                });
            }
        }
    }

    // Creation time: derived from the filename stem rather than
    // a per-entry timestamp so a session with no appended messages
    // (a freshly-minted log) still has a meaningful "created"
    // marker for the selector. Fall back to the file mtime if the
    // stem doesn't parse.
    let created_at = parse_session_id_created_at(session_id).unwrap_or(modified);
    // `last_message_at` falls back to the file mtime for two cases:
    // logs predating the per-entry timestamping work (every entry
    // has `timestamp: None`) and freshly-minted sessions with no
    // message-kind entries yet. The fallback matches the value the
    // selector would have rendered as `modified` under the older
    // single-field design.
    let last_message_at = last_message_at.unwrap_or(modified);

    Ok(Some(SessionPreview {
        session_id: session_id.to_string(),
        modified,
        created_at,
        last_message_at,
        size_bytes,
        message_count,
        first_user_message,
        // Not in the log: the listing fills these from the sidecars.
        tag: None,
        archived: false,
    }))
}

/// Parse a session id minted by [`crate::log::ConversationLog::create`]
/// back into the UTC instant it represents.
///
/// The mint format is `%Y-%m-%d-%H-%M-%S-%3f` with an optional
/// `_<N>` collision suffix appended when two `create`s land in the
/// same millisecond. This parser strips the suffix and reads the
/// stem against the same `chrono` format string the minter uses, so
/// the round-trip is exact.
///
/// Returns `None` for any stem that doesn't conform, such as
/// hand-renamed files or future format changes. The caller
/// falls back to file mtime in that case so the row still renders.
pub(crate) fn parse_session_id_created_at(session_id: &str) -> Option<DateTime<Utc>> {
    // Strip a trailing `_<digits>` collision suffix. The mint side
    // never embeds an underscore in the timestamp portion so an
    // underscore unambiguously marks the suffix boundary; we still
    // require the suffix to be all digits to avoid misclassifying
    // an unexpected stem shape as a collision.
    let stem = match session_id.rsplit_once('_') {
        Some((prefix, suffix))
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) =>
        {
            prefix
        }
        _ => session_id,
    };
    NaiveDateTime::parse_from_str(stem, "%Y-%m-%d-%H-%M-%S-%3f")
        .ok()
        .map(|naive| naive.and_utc())
}

/// Return the text from the first [`UserContent::Text`] block in
/// `content`, if any. Used by [`read_session_preview_file`] to
/// capture the user-input preview.
fn first_user_text(content: &[UserContent]) -> Option<String> {
    content.iter().find_map(|b| match b {
        UserContent::Text(t) => {
            let trimmed = t.text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use aj_agent::message::AgentMessage;
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, TextContent, ToolCall, ToolResultMessage,
        UserMessage,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::log::{ConversationLog, ConversationView};

    /// A tag round-trips through the sidecar, and clearing removes it.
    #[test]
    fn a_tag_round_trips_and_clears() {
        let (_dir, persistence) = fixture();
        let id = "2024-01-01-00-00-00";

        assert_eq!(persistence.read_tag(id).expect("read"), None, "untagged");

        persistence.write_tag(id, Some("fix-auth")).expect("write");
        assert_eq!(
            persistence.read_tag(id).expect("read"),
            Some("fix-auth".to_string()),
        );

        persistence.write_tag(id, Some("renamed")).expect("rewrite");
        assert_eq!(
            persistence.read_tag(id).expect("read"),
            Some("renamed".to_string()),
            "a second write replaces the first",
        );

        persistence.write_tag(id, None).expect("clear");
        assert_eq!(persistence.read_tag(id).expect("read"), None);
        persistence
            .write_tag(id, None)
            .expect("clearing an untagged session is not an error");
    }

    /// The sidecar lands where spec 6.8 puts it, `meta/<session id>.tag`, so
    /// nothing of it appears in the store directory the logs live in and the
    /// session enumeration cannot see it.
    #[test]
    fn a_sidecar_lives_under_meta() {
        let (dir, persistence) = fixture();
        let id = "2024-01-01-00-00-00";
        persistence.write_tag(id, Some("fix-auth")).expect("write");

        assert!(
            dir.path().join("meta").join(format!("{id}.tag")).is_file(),
            "the sidecar is at the path the spec names",
        );
        let top: Vec<std::ffi::OsString> = std::fs::read_dir(dir.path())
            .expect("store directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(
            top,
            vec![std::ffi::OsString::from("meta")],
            "nothing landed beside the logs",
        );
        assert!(
            persistence
                .enumerate_sessions()
                .expect("enumerate")
                .is_empty(),
            "a store with only a tag holds no sessions",
        );
    }

    /// An id the grammar rejects never becomes a path, on the write side as
    /// much as the read side. The write says so rather than reporting a
    /// success it did not have: a caller that believed it would put a label on
    /// a row the store cannot carry.
    #[test]
    fn an_invalid_id_touches_no_file() {
        let (dir, persistence) = fixture();
        for id in ["../escape", "with/slash", ""] {
            assert!(
                matches!(
                    persistence.write_tag(id, Some("nope")),
                    Err(ConversationError::InvalidSessionId(named)) if named == id,
                ),
                "{id:?} is refused rather than silently dropped",
            );
            // Clearing is refused on the same terms: it is a write too.
            assert!(persistence.write_tag(id, None).is_err());
            assert_eq!(persistence.read_tag(id).expect("read"), None);
            assert!(
                matches!(
                    persistence.write_archived(id, true),
                    Err(ConversationError::InvalidSessionId(named)) if named == id,
                ),
                "{id:?} is refused by the archived write as well",
            );
            assert!(persistence.write_archived(id, false).is_err());
            assert!(!persistence.read_archived(id).expect("read"));
        }
        assert!(
            !dir.path().join("meta").exists(),
            "no sidecar directory was created for a rejected id",
        );
    }

    /// The write is atomic: the tag arrives whole or not at all. A reader
    /// looking at the directory mid-write sees the previous tag, never a
    /// partial line, because the body is written elsewhere and renamed in.
    #[test]
    fn a_tag_is_never_read_half_written() {
        let (dir, persistence) = fixture();
        let id = "2024-01-01-00-00-00";
        persistence.write_tag(id, Some("before")).expect("write");

        let target = dir.path().join("meta").join(format!("{id}.tag"));
        let before = std::fs::metadata(&target).expect("the sidecar exists");
        persistence
            .write_tag(id, Some("after-a-much-longer-tag"))
            .expect("rewrite");

        // A rename replaces the inode rather than growing the old file, which
        // is what makes a concurrent reader see one whole tag or the other.
        let after = std::fs::metadata(&target).expect("still there");
        assert_ne!(
            inode(&before),
            inode(&after),
            "the rewrite landed as a rename, not an in-place write",
        );
        assert_eq!(
            persistence.read_tag(id).expect("read"),
            Some("after-a-much-longer-tag".to_string()),
        );
        // And nothing is left behind in the directory.
        let strays: Vec<_> = std::fs::read_dir(dir.path().join("meta"))
            .expect("meta")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .filter(|name| name != &std::ffi::OsString::from(format!("{id}.tag")))
            .collect();
        assert!(
            strays.is_empty(),
            "temporary files were cleaned up: {strays:?}"
        );
    }

    #[cfg(unix)]
    fn inode(metadata: &std::fs::Metadata) -> u64 {
        std::os::unix::fs::MetadataExt::ino(metadata)
    }

    /// Enumerating tags is one directory read, and an untagged store does not
    /// even have the directory.
    #[test]
    fn enumerating_tags_finds_the_sidecars_that_exist() {
        let (_dir, persistence) = fixture();
        assert!(
            persistence.enumerate_tags().expect("enumerate").is_empty(),
            "an untagged store has no meta directory and no tags",
        );

        persistence
            .write_tag("2024-01-01-00-00-00", Some("first"))
            .expect("write");
        persistence
            .write_tag("2024-01-02-00-00-00", Some("second"))
            .expect("write");

        let mut found: Vec<(String, u64)> = persistence
            .enumerate_tags()
            .expect("enumerate")
            .into_iter()
            .map(|tag| (tag.session_id, tag.size_bytes))
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                ("2024-01-01-00-00-00".to_string(), 6),
                ("2024-01-02-00-00-00".to_string(), 7),
            ],
            "each sidecar reports its own file state",
        );
    }

    /// A file in `meta/` that is not a tag sidecar is ignored, so a stray does
    /// not become a row's label. A directory named like one is not a sidecar
    /// either: offering it would cost a failed open at every enumeration for
    /// the life of the store, since a read that fails is deliberately not
    /// cached.
    #[test]
    fn enumerating_tags_ignores_what_is_not_a_sidecar() {
        let (dir, persistence) = fixture();
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).expect("meta");
        std::fs::write(meta.join("notes.txt"), "not a tag").expect("write");
        std::fs::write(meta.join("with slash.tag"), "invalid id").expect("write");
        std::fs::create_dir_all(meta.join("2024-01-01-00-00-00.tag")).expect("a directory");

        assert!(persistence.enumerate_tags().expect("enumerate").is_empty());
        assert!(
            persistence.read_tag("2024-01-01-00-00-00").is_err(),
            "and reading it is the failure the listing spares the caller",
        );
    }

    /// The archived bit round-trips through the sidecar's existence, and
    /// clearing removes the file. Setting it twice is the same answer, and so
    /// is clearing a session that was never archived: the caller states the
    /// bit it wants rather than toggling one.
    #[test]
    fn an_archived_bit_round_trips_and_clears() {
        let (dir, persistence) = fixture();
        let id = "2024-01-01-00-00-00";

        assert!(
            !persistence.read_archived(id).expect("read"),
            "a session with no sidecar is not archived",
        );

        persistence.write_archived(id, true).expect("archive");
        assert!(persistence.read_archived(id).expect("read"));
        assert!(
            dir.path()
                .join("meta")
                .join(format!("{id}.archived"))
                .is_file(),
            "the sidecar lands in meta/, beside the label's and out of the \
             directory the logs live in",
        );

        let sidecar = dir.path().join("meta").join(format!("{id}.archived"));
        let written_at = std::fs::metadata(&sidecar)
            .and_then(|meta| meta.modified())
            .expect("the sidecar's timestamp");
        std::thread::sleep(std::time::Duration::from_millis(10));
        persistence
            .write_archived(id, true)
            .expect("archiving an archived session is not an error");
        assert!(persistence.read_archived(id).expect("read"));
        assert_eq!(
            std::fs::metadata(&sidecar)
                .and_then(|meta| meta.modified())
                .expect("the sidecar's timestamp"),
            written_at,
            "and it leaves the file alone rather than rewriting it",
        );

        persistence.write_archived(id, false).expect("unarchive");
        assert!(!persistence.read_archived(id).expect("read"));
        assert!(
            !dir.path()
                .join("meta")
                .join(format!("{id}.archived"))
                .exists(),
            "unarchiving removes the sidecar rather than emptying it",
        );
        persistence
            .write_archived(id, false)
            .expect("unarchiving an unarchived session is not an error");
    }

    /// The two sidecars are two axes over one directory: archiving keeps the
    /// label, and neither listing reports the other's files. A session is
    /// archived by its own sidecar and by nothing else.
    #[test]
    fn the_archived_bit_and_the_label_are_independent() {
        let (_dir, persistence) = fixture();
        let tagged = "2024-01-01-00-00-00";
        let archived = "2024-01-02-00-00-00";
        persistence
            .write_tag(tagged, Some("fix-auth"))
            .expect("tag");
        persistence.write_archived(tagged, true).expect("archive");
        persistence.write_archived(archived, true).expect("archive");

        assert_eq!(
            persistence.read_tag(tagged).expect("read"),
            Some("fix-auth".to_string()),
            "archiving a session leaves its label alone",
        );
        assert!(
            !persistence
                .read_archived("2024-01-03-00-00-00")
                .expect("read"),
            "and a session nobody archived is not archived",
        );

        let ids = |sidecars: Vec<SidecarMetadata>| -> Vec<String> {
            let mut ids: Vec<String> = sidecars
                .into_iter()
                .map(|sidecar| sidecar.session_id)
                .collect();
            ids.sort();
            ids
        };
        assert_eq!(
            ids(persistence.enumerate_tags().expect("enumerate")),
            vec![tagged.to_string()],
            "the label listing reports the tag sidecars and nothing else",
        );
        assert_eq!(
            ids(persistence.enumerate_archived().expect("enumerate")),
            vec![tagged.to_string(), archived.to_string()],
            "and the archived listing reports the archived sidecars",
        );
    }

    /// Enumerating the archived sessions is one directory read that opens no
    /// file, and a store with nothing archived does not even have the
    /// directory. A stray in `meta/` is not the bit, a directory named like a
    /// sidecar least of all: it cannot be removed by an unarchive, so reading
    /// it as archived would strand the session.
    #[test]
    fn enumerating_archived_finds_the_sidecars_that_exist() {
        let (dir, persistence) = fixture();
        assert!(
            persistence
                .enumerate_archived()
                .expect("enumerate")
                .is_empty(),
            "an unarchived store has no meta directory and nothing archived",
        );

        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).expect("meta");
        std::fs::write(meta.join("notes.txt"), "not a sidecar").expect("write");
        std::fs::write(meta.join("with slash.archived"), "invalid id").expect("write");
        std::fs::create_dir_all(meta.join("2024-01-01-00-00-00.archived")).expect("a directory");

        assert!(
            persistence
                .enumerate_archived()
                .expect("enumerate")
                .is_empty(),
            "none of those is an archived session",
        );
        assert!(
            !persistence
                .read_archived("2024-01-01-00-00-00")
                .expect("read"),
            "and a directory under the name is not the bit either",
        );
        persistence
            .write_archived("2024-01-01-00-00-00", false)
            .expect("unarchiving what the store already reads as unarchived");

        persistence
            .write_archived("2024-01-02-00-00-00", true)
            .expect("archive");
        let found = persistence.enumerate_archived().expect("enumerate");
        assert_eq!(
            found
                .iter()
                .map(|sidecar| sidecar.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["2024-01-02-00-00-00"],
        );
        assert_eq!(
            found[0].size_bytes, 0,
            "the sidecar carries no content: its existence is the answer",
        );
    }

    /// Build a `ConversationPersistence` against a fresh temp dir.
    fn fixture() -> (TempDir, ConversationPersistence) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        (dir, persistence)
    }

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::User(UserMessage::text(text)))
    }

    fn assistant_text(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            ..AssistantMessage::empty()
        }))
    }

    /// Append one user-text message and one assistant-text message
    /// via the high-level [`ConversationView::add_message`] path.
    fn append_user_then_assistant(log: &mut ConversationLog, u: &str, a: &str) {
        let mut view = ConversationView::user(log);
        view.add_message(user_msg(u)).expect("append user");
        view.add_message(assistant_text(a))
            .expect("append assistant");
    }

    #[test]
    fn list_session_previews_returns_empty_when_dir_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing");
        let persistence = ConversationPersistence::new(path);
        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert!(previews.is_empty());
    }

    #[test]
    fn list_session_previews_captures_first_user_message_and_count() {
        let (_dir, persistence) = fixture();

        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello world", "hi there");

        // A second message on the user thread so the count crosses 2.
        let mut view = ConversationView::user(&mut log);
        view.add_message(user_msg("follow-up"))
            .expect("append second user");

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        assert_eq!(p.session_id, log.session_id());
        assert_eq!(p.message_count, 3);
        assert_eq!(p.first_user_message.as_deref(), Some("hello world"));
        assert!(p.size_bytes > 0);
    }

    /// A file whose stem is not a session id is not a session: the store can
    /// never be asked about it again (the grammar rejects the id at every
    /// lookup), so a row for it would be a directory entry nothing resolves.
    #[test]
    fn enumeration_skips_a_stem_that_is_not_a_session_id() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let real = log.session_id().to_string();
        drop(log);
        for stem in ["not a session", "sneaky.name", "héllo"] {
            std::fs::write(
                persistence.sessions_dir().join(format!("{stem}.jsonl")),
                "{}\n",
            )
            .expect("write a stray log");
        }

        let listed: Vec<String> = persistence
            .enumerate_sessions()
            .expect("enumerate")
            .into_iter()
            .map(|metadata| metadata.session_id)
            .collect();
        assert_eq!(listed, vec![real]);
    }

    /// The single-id membership primitive: one `stat`, and nothing at all for
    /// an id the grammar rejects.
    ///
    /// The traversal case puts a real, readable log where the id points, so
    /// the refusal has to come from the grammar rather than from the file not
    /// being there.
    #[test]
    fn session_metadata_answers_one_id_and_refuses_a_traversal() {
        // A store one level down, so the traversal target below still lives
        // inside the temp dir.
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let session_id = log.session_id().to_string();
        let size = std::fs::metadata(log.path()).expect("stat").len();
        drop(log);

        let metadata = persistence
            .session_metadata(&session_id)
            .expect("the stat succeeded")
            .expect("the log is there");
        assert_eq!(metadata.session_id, session_id);
        assert_eq!(metadata.size_bytes, size);

        assert!(
            persistence
                .session_metadata("2026-01-01-00-00-00-000")
                .expect("a missing log is not a failure")
                .is_none(),
        );

        // A readable log one directory up from the store. Only the grammar
        // stands between the id that names it and a `stat` that resolves.
        std::fs::create_dir_all(dir.path().join("elsewhere")).expect("sibling dir");
        std::fs::write(dir.path().join("elsewhere/reachable.jsonl"), "{}\n")
            .expect("a log outside the store");
        assert!(
            dir.path().join("elsewhere/reachable.jsonl").is_file(),
            "the traversal target is really there",
        );
        for id in ["../elsewhere/reachable", "..", ""] {
            assert!(
                persistence
                    .session_metadata(id)
                    .expect("a rejected id is not a failure")
                    .is_none(),
                "{id:?} resolved to a file",
            );
        }
    }

    /// The format sniff reads bytes, not text, so a log that is not valid
    /// UTF-8 earns a verdict rather than a read failure. That matters because
    /// the caller caches the verdict against the file and treats a failure as
    /// "try again next time", which for a file that will never decode is every
    /// enumeration for the life of the host.
    #[test]
    fn the_format_sniff_reads_bytes_and_always_reaches_a_verdict() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let session_id = log.session_id().to_string();
        let path = log.path().to_path_buf();
        drop(log);

        // The first two bytes of a three-byte character, as a crash mid-append
        // leaves behind. The sniff reads the first line, so it is unaffected.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen the log");
        std::io::Write::write_all(&mut file, &[b'{', 0xe2, 0x82]).expect("append a torn line");
        drop(file);
        assert_eq!(persistence.is_current_format(&session_id), Some(true));

        // A whole log of invalid bytes is a verdict too, not a read failure.
        let blob = persistence
            .sessions_dir()
            .join("2000-01-01-00-00-00-000.jsonl");
        std::fs::write(&blob, [0xff, 0xfe, 0xff]).expect("write a non-utf8 log");
        assert_eq!(
            persistence.is_current_format("2000-01-01-00-00-00-000"),
            Some(false),
        );
    }

    /// The enumeration is the cheap half of a listing: it stats but never
    /// opens, so a pre-refactor file is enumerated like any other and only
    /// `list_sessions` (through the format gate) drops it.
    #[test]
    fn enumeration_keeps_what_the_format_gate_drops() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let session_id = log.session_id().to_string();
        drop(log);
        let old = persistence
            .sessions_dir()
            .join("2000-01-01-00-00-00-000.jsonl");
        std::fs::write(&old, "not json at all\n").expect("write a pre-refactor file");
        // Neither an unrelated extension nor a directory is a session.
        std::fs::write(persistence.sessions_dir().join("host-id"), "id\n").expect("write");
        std::fs::create_dir(persistence.sessions_dir().join("nested.jsonl")).expect("mkdir");

        let enumerated: Vec<String> = persistence
            .enumerate_sessions()
            .expect("enumerate")
            .into_iter()
            .map(|metadata| metadata.session_id)
            .collect();
        assert_eq!(
            enumerated,
            vec![session_id.clone(), "2000-01-01-00-00-00-000".to_string()],
            "both logs are enumerated, latest first",
        );

        let listed: Vec<String> = persistence
            .list_sessions()
            .expect("list")
            .into_iter()
            .map(|metadata| metadata.session_id)
            .collect();
        assert_eq!(
            listed,
            vec![session_id.clone()],
            "the gate drops the old one"
        );
        assert_eq!(persistence.is_current_format(&session_id), Some(true));
        assert_eq!(
            persistence.is_current_format("2000-01-01-00-00-00-000"),
            Some(false),
        );
        assert_eq!(
            persistence.is_current_format("no-such-session"),
            None,
            "a log that cannot be opened has no format verdict",
        );
    }

    /// The enumeration never opens a log, which is what a caller refreshing a
    /// listing on a timer depends on: an unreadable log is still enumerated,
    /// and only the gate (which does open it) has no verdict for it.
    #[cfg(unix)]
    #[test]
    fn enumeration_opens_no_log() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let session_id = log.session_id().to_string();
        let path = log.path().to_path_buf();
        drop(log);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("drop the read bit");
        if File::open(&path).is_ok() {
            // Root ignores the permission bits, so there is nothing to prove
            // here. Skipping beats asserting something that cannot fail.
            return;
        }

        let enumerated: Vec<String> = persistence
            .enumerate_sessions()
            .expect("enumerate")
            .into_iter()
            .map(|metadata| metadata.session_id)
            .collect();
        assert_eq!(
            enumerated,
            vec![session_id.clone()],
            "a log nothing can open is still enumerated, so nothing opened it",
        );
        assert_eq!(persistence.is_current_format(&session_id), None);
        assert!(persistence.list_sessions().expect("list").is_empty());

        // And the verdict comes back the moment the file is readable again,
        // with no change to its size or modification time.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("restore the read bit");
        assert_eq!(persistence.is_current_format(&session_id), Some(true));
    }

    #[test]
    fn list_session_previews_emits_progress_callback_per_file() {
        let (_dir, persistence) = fixture();
        for i in 0..3 {
            let mut log = ConversationLog::create(&persistence).expect("create");
            append_user_then_assistant(&mut log, &format!("prompt {i}"), &format!("reply {i}"));
            // Tiny sleep so the millisecond-resolution mint sees a
            // fresh timestamp for each file.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let progress = RefCell::new(Vec::<(usize, usize)>::new());
        let previews = persistence
            .list_session_previews(|loaded, total| progress.borrow_mut().push((loaded, total)))
            .expect("list");
        assert_eq!(previews.len(), 3);
        let p = progress.into_inner();
        assert_eq!(p, vec![(1, 3), (2, 3), (3, 3)]);
    }

    #[test]
    fn list_session_previews_streaming_matches_batched_order() {
        let (_dir, persistence) = fixture();
        for i in 0..3 {
            let mut log = ConversationLog::create(&persistence).expect("create");
            append_user_then_assistant(&mut log, &format!("prompt {i}"), &format!("reply {i}"));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        // Each emit carries exactly one file's preview, in the same
        // newest-first order the batched listing produces.
        let mut batches = Vec::new();
        persistence.list_session_previews_streaming(&|| false, &mut |b| batches.push(b));
        assert!(
            batches.iter().all(|b| b.len() == 1),
            "expected one preview per batch, got {:?}",
            batches.iter().map(Vec::len).collect::<Vec<_>>()
        );
        let streamed: Vec<String> = batches
            .into_iter()
            .flatten()
            .map(|p| p.session_id)
            .collect();
        let batched: Vec<String> = persistence
            .list_session_previews(|_, _| {})
            .expect("list")
            .into_iter()
            .map(|p| p.session_id)
            .collect();
        assert_eq!(streamed, batched);
    }

    /// A preview carries the label its session's sidecar holds, and only that
    /// session's: the listing is one source for both the row and its label.
    #[test]
    fn previews_carry_the_labels_the_sidecars_hold() {
        let (_dir, persistence) = fixture();
        let mut ids = Vec::new();
        for i in 0..3 {
            let mut log = ConversationLog::create(&persistence).expect("create");
            append_user_then_assistant(&mut log, &format!("prompt {i}"), &format!("reply {i}"));
            ids.push(log.session_id().to_string());
            // Tiny sleep so the millisecond-resolution mint sees a fresh
            // timestamp for each file.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        persistence
            .write_tag(&ids[1], Some("fix-auth"))
            .expect("write");

        let labelled = |previews: Vec<SessionPreview>| -> Vec<(String, Option<String>)> {
            let mut rows: Vec<(String, Option<String>)> = previews
                .into_iter()
                .map(|preview| (preview.session_id, preview.tag))
                .collect();
            rows.sort();
            rows
        };
        let expected = vec![
            (ids[0].clone(), None),
            (ids[1].clone(), Some("fix-auth".to_string())),
            (ids[2].clone(), None),
        ];
        assert_eq!(
            labelled(persistence.list_session_previews(|_, _| {}).expect("list")),
            expected,
        );

        let mut streamed = Vec::new();
        persistence.list_session_previews_streaming(&|| false, &mut |batch| {
            streamed.extend(batch);
        });
        assert_eq!(
            labelled(streamed),
            expected,
            "the streaming listing labels its rows the same way",
        );
    }

    /// A preview carries the archived bit off the sidecar directory, on both
    /// listings. It is the only place a local listing can learn the bit: the
    /// per-file walk reads the log, which never held it.
    #[test]
    fn previews_carry_the_archived_bit() {
        let (_dir, persistence) = fixture();
        let mut ids = Vec::new();
        for i in 0..2 {
            let mut log = ConversationLog::create(&persistence).expect("create");
            append_user_then_assistant(&mut log, &format!("prompt {i}"), &format!("reply {i}"));
            ids.push(log.session_id().to_string());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        persistence.write_archived(&ids[1], true).expect("archive");

        let filed = |previews: Vec<SessionPreview>| -> Vec<(String, bool)> {
            let mut rows: Vec<(String, bool)> = previews
                .into_iter()
                .map(|preview| (preview.session_id, preview.archived))
                .collect();
            rows.sort();
            rows
        };
        let expected = vec![(ids[0].clone(), false), (ids[1].clone(), true)];
        assert_eq!(
            filed(persistence.list_session_previews(|_, _| {}).expect("list")),
            expected,
        );

        let mut streamed = Vec::new();
        persistence.list_session_previews_streaming(&|| false, &mut |batch| {
            streamed.extend(batch);
        });
        assert_eq!(
            filed(streamed),
            expected,
            "the streaming listing files its rows the same way",
        );
    }

    /// The labels come off one read of the sidecar directory, not off a
    /// per-session question. A sidecar whose session has no log is therefore
    /// still found, which a listing that asked each session for its tag could
    /// not do, and which is what keeps an untagged store free (spec 6.8).
    #[test]
    fn the_label_map_is_driven_by_the_sidecar_directory() {
        let (_dir, persistence) = fixture();
        assert!(
            persistence.tags_by_session().is_empty(),
            "an untagged store has no sidecar directory to read",
        );

        persistence
            .write_tag("2024-01-01-00-00-00", Some("no-log-here"))
            .expect("write");
        assert_eq!(
            persistence.tags_by_session().get("2024-01-01-00-00-00"),
            Some(&"no-log-here".to_string()),
        );
    }

    /// A sidecar body that reads as no label at all leaves the session
    /// unlabelled rather than failing the listing, matching how the sidecar
    /// read tolerates a hand-edited file.
    #[test]
    fn an_unusable_sidecar_leaves_the_session_unlabelled() {
        let (dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "prompt", "reply");
        let id = log.session_id().to_string();
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).expect("meta");
        std::fs::write(meta.join(format!("{id}.tag")), "\n").expect("write");

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].tag, None);
    }

    #[test]
    fn list_session_previews_streaming_stops_when_cancelled() {
        let (_dir, persistence) = fixture();
        for i in 0..3 {
            let mut log = ConversationLog::create(&persistence).expect("create");
            append_user_then_assistant(&mut log, &format!("prompt {i}"), &format!("reply {i}"));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        // A predicate that trips after the first file leaves the walk: the
        // between-files check must break before reading the rest.
        let seen = std::cell::Cell::new(0usize);
        let mut batches = Vec::new();
        persistence.list_session_previews_streaming(&|| seen.get() > 0, &mut |b| {
            seen.set(seen.get() + 1);
            batches.push(b);
        });
        assert_eq!(batches.len(), 1, "cancel should stop after the first file");
    }

    #[test]
    fn read_session_preview_file_stops_mid_file() {
        // A single file larger than the in-file poll interval, so the check
        // inside the read loop (not the between-files check) is what bails.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("2024-01-01-00-00-00.jsonl");
        let n = crate::SCAN_CANCEL_CHECK_LINES * 3;
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&path).unwrap();
            for i in 0..n {
                let line = serde_json::to_string(&serde_json::json!({
                    "id": format!("{i:08}"),
                    "thread": "user",
                    "type": "message",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": format!("p{i}")}],
                        "timestamp": 0,
                    },
                }))
                .unwrap();
                writeln!(file, "{line}").unwrap();
            }
        }

        let full = read_session_preview_file("2024-01-01-00-00-00", &path, &|| false)
            .expect("read")
            .expect("valid first line");
        assert_eq!(full.message_count, n);

        // Sticky predicate: false at the line-0 poll, true from line-1024 on.
        let calls = std::cell::Cell::new(0usize);
        let cancel = || {
            let c = calls.get();
            calls.set(c + 1);
            c > 0
        };
        let partial = read_session_preview_file("2024-01-01-00-00-00", &path, &cancel)
            .expect("read")
            .expect("valid first line");
        assert_eq!(
            partial.message_count,
            crate::SCAN_CANCEL_CHECK_LINES,
            "in-file cancel bails at the 1024-line poll, not after the full read"
        );
    }

    #[test]
    fn list_session_previews_streaming_missing_dir_emits_nothing() {
        let dir = TempDir::new().unwrap();
        let persistence = ConversationPersistence::new(dir.path().join("missing"));
        let mut batches = Vec::new();
        persistence.list_session_previews_streaming(&|| false, &mut |b| batches.push(b));
        assert!(batches.is_empty());
    }

    #[test]
    fn list_session_previews_ignores_non_user_first_messages() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        // First message is a tool_result (not a user prompt). The
        // preview should leave `first_user_message` at `None`.
        let mut view = ConversationView::user(&mut log);
        view.add_message(AgentMessage::wire(Message::ToolResult(
            ToolResultMessage::text("x", "ping", "ok", false),
        )))
        .expect("append");

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        assert!(previews[0].first_user_message.is_none());
        assert_eq!(previews[0].message_count, 1);
    }

    #[test]
    fn list_session_previews_skips_pre_refactor_files() {
        let (_dir, persistence) = fixture();
        let bogus = persistence.sessions_dir.join("old.jsonl");
        std::fs::write(&bogus, "not json at all\n").expect("write");

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert!(previews.is_empty(), "got {previews:?}");
    }

    #[test]
    fn list_session_previews_keeps_valid_alongside_pre_refactor_and_counts_all_files() {
        // A pre-refactor file is dropped from the rows but still counts
        // toward the progress total (it's walked in the same single pass
        // as the valid files), so the loaded counter reaches the total
        // even though fewer rows appear.
        let (_dir, persistence) = fixture();
        let sessions_dir = persistence.sessions_dir().to_path_buf();
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        std::fs::write(sessions_dir.join("old.jsonl"), "not json at all\n").expect("write old");

        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");

        let progress = RefCell::new(Vec::<(usize, usize)>::new());
        let previews = persistence
            .list_session_previews(|loaded, total| progress.borrow_mut().push((loaded, total)))
            .expect("list");

        assert_eq!(previews.len(), 1, "only the valid session yields a row");
        assert_eq!(previews[0].session_id, log.session_id());
        let progress = progress.into_inner();
        assert_eq!(progress.last(), Some(&(2, 2)), "both files tick progress");
    }

    #[test]
    fn read_session_preview_file_tolerates_a_torn_later_line() {
        // The first line gates the format (a valid entry here), so a
        // garbage line *after* it is skipped rather than dropping the
        // whole file, matching the resume truncated-line tolerance.
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let path = log.path().to_path_buf();
        let session_id = log.session_id().to_string();
        drop(log);

        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .expect("read log")
            .lines()
            .map(str::to_string)
            .collect();
        // Insert garbage after the first valid line.
        lines.insert(1, "}{ this is not json".to_string());
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("rewrite");

        let preview = read_session_preview_file(&session_id, &path, &|| false)
            .expect("read")
            .expect("a valid first line keeps the file");
        // The two messages survive. Only the torn line is skipped.
        assert_eq!(preview.message_count, 2);
        assert_eq!(preview.first_user_message.as_deref(), Some("hello"));
    }

    #[test]
    fn parse_session_id_created_at_round_trips_minted_id() {
        let parsed = super::parse_session_id_created_at("2025-05-11-14-22-03-512")
            .expect("known-good stem parses");
        let expected = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_milli_opt(14, 22, 3, 512)
            .unwrap()
            .and_utc();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_session_id_created_at_strips_collision_suffix() {
        let suffix = super::parse_session_id_created_at("2025-05-11-14-22-03-512_3")
            .expect("suffixed stem parses");
        let bare = super::parse_session_id_created_at("2025-05-11-14-22-03-512")
            .expect("bare stem parses");
        assert_eq!(suffix, bare);
    }

    #[test]
    fn parse_session_id_created_at_returns_none_for_unrecognised_stem() {
        assert!(super::parse_session_id_created_at("custom-name").is_none());
        assert!(super::parse_session_id_created_at("").is_none());
        assert!(super::parse_session_id_created_at("2025-05-11-14-22-03-512_abc").is_none());
    }

    #[test]
    fn list_session_previews_populates_created_at_from_session_id() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hi", "ok");
        let session_id = log.session_id().to_string();

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        let expected =
            super::parse_session_id_created_at(&session_id).expect("freshly-minted id parses");
        assert_eq!(p.created_at, expected);
    }

    #[test]
    fn list_session_previews_counts_tool_result_entries() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: "tu-1".into(),
                    name: "ping".into(),
                    arguments: serde_json::json!({}),
                })],
                ..AssistantMessage::empty()
            })))
            .expect("a");
            view.add_message(AgentMessage::wire(Message::ToolResult(
                ToolResultMessage::text("tu-1", "ping", "ok", false),
            )))
            .expect("tr");
        }

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        // Three wire-level messages: user, assistant, tool_result.
        assert_eq!(previews[0].message_count, 3);
    }

    #[test]
    fn list_session_previews_falls_back_to_modified_when_no_message_entries() {
        // Legacy on-disk shape: a session file containing only a
        // SystemPrompt entry, with no `Message` entries. New code
        // can't produce this layout (the system prompt buffers and
        // never flushes alone), but files written by older builds
        // still exist on users' disks and the preview walk must
        // render them gracefully. The fallback under test:
        // `last_message_at` defaults to the file mtime when no
        // Message-kind entry contributed a timestamp.
        let (_dir, persistence) = fixture();
        let sessions_dir = persistence.sessions_dir().to_path_buf();
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");

        let session_id = "2024-01-01-00-00-00-000";
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        let line = serde_json::json!({
            "id": "00000000",
            "timestamp": "2024-01-01T00:00:00Z",
            "thread": "meta",
            "type": "system_prompt",
            "text": "legacy abandoned-session prompt",
        });
        std::fs::write(&path, format!("{line}\n")).expect("write legacy file");

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        assert_eq!(p.message_count, 0);
        assert_eq!(p.last_message_at, p.modified);
    }

    #[test]
    fn list_session_previews_uses_largest_message_timestamp() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "world");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut view = ConversationView::user(&mut log);
        view.add_message(user_msg("follow-up"))
            .expect("append user2");

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        let min_expected = p.created_at + chrono::Duration::milliseconds(10);
        assert!(
            p.last_message_at >= min_expected,
            "last_message_at = {}, expected >= {}",
            p.last_message_at,
            min_expected
        );
    }
}
