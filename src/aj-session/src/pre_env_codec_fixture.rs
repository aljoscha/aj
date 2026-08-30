//! Frozen pre-session-env decoder from commit
//! `3fdb897feeb8ab48486fc71cf7800e211c61d899`.
//!
//! The entry enum and delayed-final-line recovery below preserve the decoder
//! that shipped immediately before `EnvChange`. Compatibility tests use this
//! code rather than asking the current decoder to approximate an older binary.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use aj_agent::events::AgentSettings;
use aj_agent::message::AgentMessage;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::compaction::CompactionDetails;
use crate::log::{EntryId, ThreadKind};

#[derive(Debug, Deserialize)]
struct ConversationEntry {
    #[allow(dead_code)]
    id: EntryId,
    #[serde(default)]
    #[allow(dead_code)]
    parent_id: Option<EntryId>,
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: Option<DateTime<Utc>>,
    #[allow(dead_code)]
    thread: ThreadKind,
    #[serde(default)]
    #[allow(dead_code)]
    agent_id: Option<usize>,
    #[serde(flatten)]
    #[allow(dead_code)]
    entry: ConversationEntryKind,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum ConversationEntryKind {
    Message {
        message: AgentMessage,
    },
    SystemPrompt {
        text: String,
    },
    ModelChange {
        provider: String,
        model_id: String,
    },
    ThinkingChange {
        level: String,
    },
    SpeedChange {
        speed: String,
    },
    VerbosityChange {
        verbosity: String,
    },
    SubAgentSpawn {
        task: String,
        #[serde(default)]
        background: bool,
        settings: AgentSettings,
    },
    Compaction {
        summary: String,
        first_kept_entry_id: EntryId,
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<CompactionDetails>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<aj_models::types::Usage>,
    },
}

/// Run the frozen decoder's final-line recovery against `path`.
///
/// An unknown interior record is corruption and leaves bytes untouched. An
/// unknown final record is treated as a torn tail and truncated from disk.
pub(crate) fn resume(path: &Path) -> Result<(), String> {
    let file = File::open(path).map_err(|err| err.to_string())?;
    let snapshot_len = file.metadata().map_err(|err| err.to_string())?.len();
    let mut reader = BufReader::new(file.take(snapshot_len));
    let mut pending_line = String::new();
    let mut current_line = String::new();
    let mut pending_line_number = None;
    let mut pending_line_start = None;
    let mut physical_line_number = 0;
    let mut next_line_start = 0_u64;
    let mut corruption = None;

    loop {
        current_line.clear();
        let current_line_start = next_line_start;
        let bytes_read = reader
            .read_line(&mut current_line)
            .map_err(|err| err.to_string())?;
        if bytes_read == 0 {
            break;
        }
        next_line_start += u64::try_from(bytes_read).expect("line length fits u64");
        physical_line_number += 1;
        if corruption.is_some() {
            continue;
        }
        if current_line.ends_with('\n') {
            current_line.pop();
            if current_line.ends_with('\r') {
                current_line.pop();
            }
        }
        if current_line.trim().is_empty() {
            continue;
        }
        if let Some(line_number) = pending_line_number
            && let Err(err) = serde_json::from_str::<ConversationEntry>(&pending_line)
        {
            corruption = Some((line_number, err));
            continue;
        }
        std::mem::swap(&mut pending_line, &mut current_line);
        pending_line_number = Some(physical_line_number);
        pending_line_start = Some(current_line_start);
    }

    if let Some((line_number, err)) = corruption {
        return Err(format!("line {line_number}: {err}"));
    }
    let mut truncate_to = None;
    if pending_line_number.is_some()
        && serde_json::from_str::<ConversationEntry>(&pending_line).is_err()
    {
        truncate_to = pending_line_start;
    }
    drop(reader);
    if let Some(len) = truncate_to {
        OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|file| file.set_len(len))
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}
