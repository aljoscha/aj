//! Project-level discovery of conversation thread files.
//!
//! [`ConversationPersistence`] is the owner of a project's threads
//! directory. It lists existing threads (for `aj list-threads` and
//! `aj continue`) and resolves a thread id to its on-disk path so
//! [`crate::log::ConversationLog`] can open / create the right file.

use chrono::{DateTime, Utc};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use crate::log::{ConversationEntry, ConversationError};

/// Handles persistence operations for conversations, including listing
/// existing thread files and resolving their paths.
#[derive(Clone)]
pub struct ConversationPersistence {
    threads_dir: PathBuf,
}

impl ConversationPersistence {
    /// Create a new [ConversationPersistence] instance with the given
    /// threads directory.
    pub fn new(threads_dir: PathBuf) -> Self {
        Self { threads_dir }
    }

    pub fn threads_dir(&self) -> &std::path::Path {
        &self.threads_dir
    }

    pub(crate) fn thread_path(&self, thread_id: &str) -> PathBuf {
        self.threads_dir.join(format!("{thread_id}.jsonl"))
    }

    /// Get metadata about all conversation threads, sorted by creation
    /// time (latest first).
    ///
    /// Files whose first line does not parse as the new
    /// [ConversationEntry] shape (e.g. pre-refactor threads) are skipped
    /// with a `tracing::info!` note.
    pub fn list_threads(&self) -> Result<Vec<ThreadMetadata>, ConversationError> {
        if !self.threads_dir.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&self.threads_dir)?;
        let mut thread_files = Vec::new();

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                if let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) {
                    thread_files.push(file_stem.to_string());
                }
            }
        }

        // Sort by filename (a timestamp), latest first.
        thread_files.sort_by(|a, b| b.cmp(a));

        let mut threads = Vec::new();

        for thread_id in thread_files {
            let path = self.thread_path(&thread_id);

            if !Self::looks_like_new_format(&path) {
                tracing::info!(
                    "skipping pre-refactor thread file {} (old on-disk format)",
                    path.display()
                );
                continue;
            }

            let metadata = fs::metadata(&path)?;
            let modified = metadata.modified()?;
            let modified_str = DateTime::<Utc>::from(modified)
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string();

            // Use file size as proxy for conversation length.
            let file_size = metadata.len();
            let size_display = if file_size < 1024 {
                format!("{file_size}B")
            } else if file_size < 1024 * 1024 {
                format!("{}KB", file_size / 1024)
            } else {
                format!("{}MB", file_size / (1024 * 1024))
            };

            threads.push(ThreadMetadata {
                thread_id,
                modified: modified_str,
                size_display,
            });
        }

        Ok(threads)
    }

    /// Empty files are considered new-format (they were just created and
    /// nothing has been written yet). Otherwise the first non-empty line
    /// must parse as a [ConversationEntry].
    fn looks_like_new_format(path: &std::path::Path) -> bool {
        let Ok(file) = File::open(path) else {
            return false;
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return true, // empty file is fine
                Ok(_) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    return serde_json::from_str::<ConversationEntry>(line.trim_end()).is_ok();
                }
                Err(_) => return false,
            }
        }
    }

    /// Get the latest conversation thread ID, if any exist.
    pub fn get_latest_thread_id(&self) -> Result<Option<String>, ConversationError> {
        let threads = self.list_threads()?;
        Ok(threads.first().map(|t| t.thread_id.clone()))
    }
}

/// Metadata about a conversation thread.
#[derive(Debug, Clone)]
pub struct ThreadMetadata {
    pub thread_id: String,
    pub modified: String,
    pub size_display: String,
}
