//! JSONL as the format a session is exported to and imported from.
//!
//! A session is one file, `ID.jsonl`, with one entry per line in append
//! order. Its label and archive marker are sidecars beside it:
//! `meta/ID.tag` holds the label on one line, and `meta/ID.archived` marks
//! the session archived by existing. [`Directory`] is that layout.
//!
//! This module reads and writes the format and knows nothing of where
//! sessions are kept. `aj export-session` and `aj import-session` end here.
//! Session files that were never imported and still sit in a project's
//! sessions directory are [`crate::legacy`]'s.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::log::{validate_recorded_session_env, validate_recorded_system_prompt};
use crate::sqlite::{SessionAnnotations, SqliteStore};
use crate::{EntryId, LogEntry, LogEntryKind, ProjectSessions, SessionError, ThreadKind};

/// The extension of the sidecar holding a session's label.
pub(crate) const TAG_SIDECAR: &str = "tag";

/// The extension of the sidecar whose existence archives a session.
pub(crate) const ARCHIVED_SIDECAR: &str = "archived";

/// What importing a JSONL file did about an undelimited final line, which
/// is what an interrupted append left behind. The file itself is not
/// changed. [`crate::SessionLog::resume_reporting`] returns it for the
/// opening surface's recovery notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailRepair {
    /// The final entry was complete and valid but lacked its framing
    /// newline. It was imported.
    CompletedFraming,
    /// The final line was an incomplete entry prefix of `bytes` bytes. It
    /// was left out.
    RemovedIncompleteEntry { bytes: u64 },
}

/// A directory of session files with their sidecars.
#[derive(Clone, Copy)]
pub(crate) struct Directory<'a>(pub(crate) &'a Path);

impl Directory<'_> {
    pub(crate) fn session_path(&self, session_id: &str) -> PathBuf {
        self.0.join(format!("{session_id}.jsonl"))
    }

    /// The directory holding the sidecars, `meta/` beside the session files.
    ///
    /// One directory read finds every sidecar without a `stat` per session.
    pub(crate) fn meta_dir(&self) -> PathBuf {
        self.0.join("meta")
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

    /// The label the session's sidecar holds, `Ok(None)` when it has none.
    pub(crate) fn read_tag(&self, session_id: &str) -> Result<Option<String>, SessionError> {
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

    /// Whether the session's archive marker exists.
    pub(crate) fn read_archived(&self, session_id: &str) -> Result<bool, SessionError> {
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

    /// The label and archive marker the session's sidecars hold.
    pub(crate) fn annotations(&self, session_id: &str) -> Result<SessionAnnotations, SessionError> {
        Ok(SessionAnnotations {
            tag: self.read_tag(session_id)?,
            archived: self.read_archived(session_id)?,
        })
    }

    /// Set or clear the session's label sidecar.
    ///
    /// The body lands in a temporary file in the same directory and is
    /// renamed over the target. Readers see the old tag or the new one, never
    /// a torn line.
    ///
    /// `tag` is expected to have been through [`crate::tag::normalize_tag`]
    /// already, so this writes what it is given. An id the grammar rejects
    /// is an error, not a quiet no-op.
    pub(crate) fn write_tag(
        &self,
        session_id: &str,
        tag: Option<&str>,
    ) -> Result<(), SessionError> {
        let Some(path) = self.sidecar_path(session_id, TAG_SIDECAR) else {
            return Err(SessionError::InvalidSessionId(session_id.to_string()));
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

    /// Create or remove the session's archive marker.
    ///
    /// The existence of the marker is the bit, so there is no content to
    /// tear: the create and the remove each publish the whole answer in one
    /// operation, and a reader sees one state or the other. An id the
    /// grammar rejects is an error, not a quiet no-op.
    pub(crate) fn write_archived(
        &self,
        session_id: &str,
        archived: bool,
    ) -> Result<(), SessionError> {
        let Some(path) = self.sidecar_path(session_id, ARCHIVED_SIDECAR) else {
            return Err(SessionError::InvalidSessionId(session_id.to_string()));
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

    /// A session's file and sidecars, read without changing them (see
    /// [`read`] for `tolerant`).
    pub(crate) fn source(&self, session_id: &str, tolerant: bool) -> Result<Source, SessionError> {
        if !crate::id::is_valid_session_id(session_id) {
            return Err(SessionError::InvalidSessionId(session_id.to_string()));
        }
        let path = self.session_path(session_id);
        let (entries, tail_repair) = read(&path, tolerant)?;
        Ok(Source {
            entries,
            modified: fs::metadata(&path)?.modified()?.into(),
            annotations: self.annotations(session_id)?,
            tail_repair,
        })
    }
}

/// One session as its file and sidecars hold it.
pub(crate) struct Source {
    pub(crate) entries: Vec<LogEntry>,
    modified: DateTime<Utc>,
    annotations: SessionAnnotations,
    pub(crate) tail_repair: Option<TailRepair>,
}

impl Source {
    pub(crate) fn import_into(
        &self,
        store: &mut SqliteStore,
        id: &str,
    ) -> Result<(), SessionError> {
        import_entries(store, id, &self.entries, self.modified, &self.annotations)
    }

    /// Store the session as `id` in `store`'s database, in one transaction.
    pub(crate) fn import(&self, store: &ProjectSessions, id: &str) -> Result<(), SessionError> {
        self.import_into(&mut store.sqlite_writer()?, id)
    }

    pub(crate) fn nonempty(self) -> Result<Self, SessionError> {
        if self.entries.is_empty() {
            return Err(SessionError::MigrationConflict(
                "the source has no entries".into(),
            ));
        }
        Ok(self)
    }
}

/// Store `entries` as session `id` in one transaction. The store files
/// them as it files entries appended one at a time.
pub(crate) fn import_entries(
    store: &mut SqliteStore,
    id: &str,
    entries: &[LogEntry],
    modified: DateTime<Utc>,
    annotations: &SessionAnnotations,
) -> Result<(), SessionError> {
    let entries: Vec<&LogEntry> = entries.iter().collect();
    store.create_session(id, &entries, modified, annotations)?;
    Ok(())
}

impl ProjectSessions {
    /// Write a session as a JSONL file: `directory/ID.jsonl` plus its
    /// `meta/` label and archive sidecars. Never replaces an existing file.
    /// This is history interchange, not a backup: external tool-output
    /// files are not copied.
    pub fn export_jsonl(
        &self,
        session_id: &str,
        directory: &Path,
    ) -> Result<PathBuf, SessionError> {
        use std::io::Write;
        let entries = self.read_entries(session_id)?;
        let (tag, archived) = (self.read_tag(session_id)?, self.read_archived(session_id)?);
        fs::create_dir_all(directory)?;
        let target = Directory(directory);
        let path = target.session_path(session_id);
        let mut file = std::io::BufWriter::new(File::create_new(&path)?);
        let write = (|| -> Result<(), SessionError> {
            for entry in &entries {
                serde_json::to_writer(&mut file, entry)?;
                file.write_all(b"\n")?;
            }
            Ok(file.into_inner().map_err(|e| e.into_error())?.sync_all()?)
        })();
        if write.is_err() {
            // A partial file would block a retry and could be imported.
            let _ = fs::remove_file(&path);
        }
        write?;
        if tag.is_some() {
            target.write_tag(session_id, tag.as_deref())?;
        }
        if archived {
            target.write_archived(session_id, true)?;
        }
        Ok(path)
    }

    /// Import a JSONL session file, with any `meta/` sidecars beside it, into
    /// this project's SQLite store under the file's session ID. The file is
    /// left untouched. An ID this project already uses is refused. Rename the
    /// file to import a copy.
    pub fn import_jsonl(&self, file: &Path) -> Result<String, SessionError> {
        let id = file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|_| file.extension().is_some_and(|e| e == "jsonl"))
            .ok_or_else(|| SessionError::InvalidSessionId(file.display().to_string()))?;
        let directory = file.parent().unwrap_or_else(|| Path::new("."));
        let source = Directory(directory).source(id, false)?.nonempty()?;
        if self.in_database(id)? || crate::legacy::holds(self, id).unwrap_or(false) {
            return Err(SessionError::MigrationConflict(format!(
                "this project already has a session {id}"
            )));
        }
        source.import(self, id)?;
        Ok(id.to_owned())
    }
}

/// Read a session's JSONL file in append order. Writes nothing.
///
/// Every delimited line must be a valid entry, otherwise the file is
/// [`SessionError::Corrupt`]. The final line may lack its newline,
/// which is what an interrupted append left behind. A `tolerant` read
/// adopts such a line when it is a complete entry, drops it when it is
/// an incomplete JSON prefix, and reports which. It also keeps the first
/// occurrence of a duplicated entry id. A strict read refuses both. An
/// undelimited final line that is neither shape, such as a complete
/// value that is not an entry, is [`SessionError::UnsafeTail`].
pub(crate) fn read(
    path: &Path,
    tolerant: bool,
) -> Result<(Vec<LogEntry>, Option<TailRepair>), SessionError> {
    let file = File::open(&path)?;
    // The captured length defines this resume's boundary. Appends after this
    // snapshot belong to a later resume.
    let snapshot_len = file.metadata()?.len();
    let mut reader = BufReader::new(file.take(snapshot_len));
    let mut physical_line_number = 0_u64;
    let mut corruption = None;

    let mut entries: HashMap<EntryId, LogEntry> = HashMap::new();
    let mut order: Vec<EntryId> = Vec::new();

    // A duplicated line (a hand-edited log, or one line written
    // twice) would otherwise give one entry two positions and
    // project its message twice. `append` rejects a duplicate id
    // loudly. A tolerant read keeps the first ordinary occurrence and
    // carries on, since the rest of the file is still usable. Creation
    // identity is stricter: coalescing a reused root or legacy Meta EnvChange id
    // could erase the physical identity entry before its layout is
    // validated, so any duplicate involving either is corruption.
    let adopt = |order: &mut Vec<EntryId>,
                 entries: &mut HashMap<EntryId, LogEntry>,
                 entry: LogEntry,
                 line_number: u64|
     -> Result<(), SessionError> {
        if let Some(existing) = entries.get(&entry.id) {
            if !tolerant {
                return Err(SessionError::Corrupt(format!(
                    "{}:line {line_number}: duplicate entry id {}",
                    path.display(),
                    entry.id,
                )));
            }
            let protected = |entry: &LogEntry| {
                matches!(entry.entry, LogEntryKind::SystemPrompt { .. })
                    || (entry.thread == ThreadKind::Meta
                        && matches!(entry.entry, LogEntryKind::EnvChange { .. }))
            };
            let is_creation_identity =
                order.first() == Some(&entry.id) || protected(existing) || protected(&entry);
            if is_creation_identity {
                return Err(SessionError::Corrupt(format!(
                    "{}:line {line_number}: duplicate creation identity entry id {}",
                    path.display(),
                    entry.id,
                )));
            }
            tracing::warn!(
                "duplicate entry id {} in {}: keeping the first occurrence",
                entry.id,
                path.display()
            );
            return Ok(());
        }
        order.push(entry.id.clone());
        entries.insert(entry.id.clone(), entry);
        Ok(())
    };

    // Decode one delimited physical line: blank lines are tolerated, a
    // line that is not UTF-8 or not a current entry is corruption.
    let process_delimited = |entries: &mut HashMap<EntryId, LogEntry>,
                             order: &mut Vec<EntryId>,
                             line: &[u8],
                             line_number: u64|
     -> Result<(), SessionError> {
        let Ok(text) = std::str::from_utf8(line) else {
            return Err(SessionError::Corrupt(format!(
                "{}:line {line_number}: line is not valid UTF-8",
                path.display()
            )));
        };
        let text = text.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            return Ok(());
        }
        match serde_json::from_str::<LogEntry>(text) {
            Ok(entry) => adopt(order, entries, entry, line_number),
            Err(err) => Err(SessionError::Corrupt(format!(
                "{}:line {line_number}: {err}",
                path.display()
            ))),
        }
    };

    // One line of lookahead: a line is decoded once a later line proves
    // it is not final, so the final line is judged separately below.
    let mut final_line: Option<(Vec<u8>, u64)> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        physical_line_number += 1;
        if let Some((previous, line_number)) = final_line.take()
            && corruption.is_none()
            && let Err(err) = process_delimited(&mut entries, &mut order, &previous, line_number)
        {
            corruption = Some(err);
        }
        final_line = Some((line.clone(), physical_line_number));
    }

    let unread_snapshot_bytes = reader.get_ref().limit();
    if unread_snapshot_bytes != 0 {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!(
                "session log shrank while reading {}: {unread_snapshot_bytes} snapshot bytes missing",
                path.display()
            ),
        )));
    }

    // Interior corruption always wins over any tail outcome.
    if let Some(err) = corruption {
        return Err(err);
    }

    let mut repair = None;
    match final_line {
        None => {}
        Some((line, line_number)) if line.ends_with(b"\n") => {
            // An interrupted write cannot have produced its framing
            // newline, so a malformed delimited final line is corruption
            // like any interior line.
            process_delimited(&mut entries, &mut order, &line, line_number)?;
        }
        Some((tail, line_number)) => {
            // Only appends write this file and each append is one entry
            // plus its newline, so an undelimited final line is the
            // interrupted append's partial entry. The parser decides
            // between the two shapes a cut can leave: a complete entry
            // (the cut fell between the entry and its newline) or a
            // strict prefix of one (the parser ran out of input). The cut
            // may fall inside a multi-byte character, which is still a
            // prefix. Anything else was not written by an append.
            let text = match std::str::from_utf8(&tail) {
                Ok(text) => text,
                Err(err) if err.error_len().is_none() => {
                    std::str::from_utf8(&tail[..err.valid_up_to()]).expect("validated prefix")
                }
                Err(_) => {
                    return Err(SessionError::UnsafeTail {
                        reason: "the unterminated final line is not valid UTF-8".to_string(),
                    });
                }
            };
            match serde_json::from_str::<LogEntry>(text) {
                Ok(entry) => {
                    adopt(&mut order, &mut entries, entry, line_number)?;
                    repair = Some(TailRepair::CompletedFraming);
                }
                Err(err) if err.is_eof() => {
                    tracing::warn!(
                        "leaving out the interrupted final entry of {}: {} bytes",
                        path.display(),
                        tail.len(),
                    );
                    repair = Some(TailRepair::RemovedIncompleteEntry {
                        bytes: u64::try_from(tail.len()).expect("line length fits u64"),
                    });
                }
                Err(err) => {
                    return Err(SessionError::UnsafeTail {
                        reason: format!("the unterminated final line is not an entry: {err}"),
                    });
                }
            }
        }
    }

    validate_recorded_system_prompt(&order, &entries)?;
    validate_recorded_session_env(&order, &entries)?;

    if !tolerant && repair.is_some() {
        return Err(SessionError::UnsafeTail {
            reason: "the final entry's write was interrupted".into(),
        });
    }

    // Backfill each message entry's in-memory id from its entry id, as
    // a read from the database does. Old 8-hex files simply adopt their
    // entry id as the message id.
    let entries = order
        .into_iter()
        .filter_map(|id| {
            let mut entry = entries.remove(&id)?;
            if let LogEntryKind::Message { message } = &mut entry.entry {
                message.set_id(id);
            }
            Some(entry)
        })
        .collect();
    Ok((entries, repair))
}
