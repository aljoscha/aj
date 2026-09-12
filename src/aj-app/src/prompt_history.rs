//! Prompt-history wire conversion and merging, using the session collector's
//! ordering and deduplication rules.

use aj_session::prompt_history::{RecordedPrompt, retain_recent};
use aj_wire::{HistoryPrompt, PROMPT_HISTORY_LIMIT, PromptHistory};

pub fn to_wire(prompt: RecordedPrompt) -> HistoryPrompt {
    HistoryPrompt {
        text: prompt.entry.text,
        project: prompt.entry.project,
        timestamp: prompt.timestamp,
    }
}

/// Merge a host's reply without discarding healthy prompts when another host
/// failed. Equal timestamps keep `history` before `incoming`.
pub fn merge(history: &mut PromptHistory, incoming: PromptHistory) {
    let mut prompts = std::mem::take(&mut history.prompts)
        .into_iter()
        .chain(incoming.prompts)
        .map(|p| RecordedPrompt {
            entry: aj_session::PromptEntry {
                text: p.text,
                project: p.project,
            },
            timestamp: p.timestamp,
        })
        .collect();
    retain_recent(&mut prompts, PROMPT_HISTORY_LIMIT);
    history.prompts = prompts.into_iter().map(to_wire).collect();
    history.incomplete.extend(incoming.incomplete);
}
