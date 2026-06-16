//! Pure compaction planning: token estimation, cut-point selection,
//! conversation serialization, summary prompt templates, and file-op
//! extraction.
//!
//! Everything here operates on `&[ConversationEntry]` / `&[Message]`
//! and returns a plan; there is no provider, no async, and no I/O. The
//! host (`aj`) calls [`prepare_compaction`] to compute a
//! [`CompactionPlan`], generates the summary against its model, and
//! records the result with `ConversationLog::append_compaction`. See
//! `docs/compaction-spec.md` §4.

use serde::{Deserialize, Serialize};

use aj_agent::message::AgentMessage;
use aj_models::types::{AssistantContent, Message, UserContent, UserMessage};

use crate::log::{Conversation, ConversationEntry, ConversationEntryKind, EntryId};

/// Files touched in a summarized range, surfaced so the model knows
/// what was read or modified without parsing the summary prose. The
/// lists are carried forward across successive compactions.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CompactionDetails {
    #[serde(default)]
    pub read_files: Vec<String>,
    #[serde(default)]
    pub modified_files: Vec<String>,
}

/// Estimated context occupancy of a message list.
#[derive(Clone, Debug)]
pub struct ContextEstimate {
    pub tokens: u64,
    /// Index of the message that supplied authoritative usage, if any.
    pub last_usage_index: Option<usize>,
}

/// The first retained entry chosen by [`find_cut_point`].
#[derive(Clone, Debug)]
pub struct CutPoint {
    pub first_kept_entry_id: EntryId,
    pub first_kept_index: usize,
    /// `Some(turn_start)` when the cut splits a turn (cut lands on an
    /// assistant message, with an earlier user turn-start in range).
    pub turn_start_index: Option<usize>,
}

/// Everything the host needs to run one compaction, computed purely
/// from the linearized log.
#[derive(Clone, Debug)]
pub struct CompactionPlan {
    pub first_kept_entry_id: EntryId,
    pub messages_to_summarize: Vec<Message>,
    /// Prefix of a split turn, summarized separately. Empty unless the
    /// cut landed mid-turn.
    pub turn_prefix_messages: Vec<Message>,
    pub previous_summary: Option<String>,
    pub tokens_before: u64,
    pub file_ops: CompactionDetails,
}

/// Prefix wrapping the synthetic summary message so the model reads it
/// as context rather than an instruction. Paired with
/// [`COMPACTION_SUMMARY_SUFFIX`].
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

/// Suffix closing the synthetic summary message.
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Heuristic token charge for one image block. The character-based
/// estimator divides accumulated characters by 4, so an image is
/// charged `ESTIMATED_IMAGE_TOKENS * 4` characters.
const ESTIMATED_IMAGE_TOKENS: u64 = 1200; // ~4800 chars / 4

/// Builtin tool names that read a file at `arguments["path"]`.
const READ_TOOLS: &[&str] = &["read_file"];

/// Builtin tool names that modify a file at `arguments["path"]`.
const EDIT_TOOLS: &[&str] = &["edit_file", "edit_file_multi", "write_file"];

/// Upper bound on the characters of a single tool result embedded in the
/// summarizer transcript. A large result (a file dump, a long command
/// output) is truncated to this many characters so one tool call can't
/// blow up the summarization request.
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Shared system prompt for the out-of-band summarizer call.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// Format instruction for the first compaction on a thread. The
/// transcript is placed before this block so the leading sentence can
/// refer to "the messages above". Stable section headings plus the
/// directive to preserve exact identifiers make the output a checkpoint
/// another model can continue from.
const INITIAL_SUMMARY_INSTRUCTION: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another model will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by the user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, identifiers, and error messages.";

/// Format instruction for a follow-up compaction: fold the new messages
/// into the previous summary. The explicit merge rules keep
/// still-relevant information from being dropped as history is folded
/// forward.
const UPDATE_SUMMARY_INSTRUCTION: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed
- UPDATE \"Next Steps\" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, identifiers, and error messages.";

/// Build the synthetic user-role message that stands in for compacted
/// history (the wrapped summary the model reads as context).
pub fn summary_message(summary: &str) -> AgentMessage {
    let text = format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}");
    AgentMessage::wire(Message::User(UserMessage::text(&text)))
}

/// Sum the heuristic character charge of user/tool-result content:
/// text-block characters plus a fixed charge per image.
#[allow(clippy::as_conversions)]
fn user_content_chars(content: &[UserContent]) -> u64 {
    let mut chars: u64 = 0;
    for block in content {
        match block {
            UserContent::Text(t) => chars += t.text.chars().count() as u64,
            UserContent::Image(_) => chars += ESTIMATED_IMAGE_TOKENS * 4,
        }
    }
    chars
}

/// Estimate context tokens for a single wire message via the
/// character heuristic (`ceil(chars / 4)`), charging a fixed amount
/// per image block.
#[allow(clippy::as_conversions)]
pub fn estimate_message_tokens(message: &Message) -> u64 {
    let chars = match message {
        Message::User(u) => user_content_chars(&u.content),
        Message::ToolResult(tr) => user_content_chars(&tr.content),
        Message::Assistant(a) => {
            let mut chars: u64 = 0;
            for block in &a.content {
                match block {
                    AssistantContent::Text(t) => chars += t.text.chars().count() as u64,
                    AssistantContent::Thinking(th) => chars += th.thinking.chars().count() as u64,
                    AssistantContent::ToolCall(tc) => {
                        chars += tc.name.len() as u64;
                        chars += serde_json::to_string(&tc.arguments)
                            .map(|s| s.len())
                            .unwrap_or(0) as u64;
                    }
                }
            }
            chars
        }
    };
    chars.div_ceil(4)
}

/// Estimate the context tokens a linearized message list occupies.
///
/// Prefers the most recent assistant `usage`
/// (`input + cache_read + cache_write`) as the authoritative prompt
/// size — the same numerator the footer uses — and adds the heuristic
/// estimate of only the messages trailing it. With no usage anywhere
/// it estimates the whole list heuristically.
pub fn estimate_context_tokens(messages: &[Message]) -> ContextEstimate {
    let last_usage = messages.iter().enumerate().rev().find_map(|(i, m)| {
        if let Message::Assistant(a) = m {
            let base = a.usage.input + a.usage.cache_read + a.usage.cache_write;
            if base > 0 {
                return Some((i, base));
            }
        }
        None
    });

    match last_usage {
        Some((i, base)) => {
            let trailing: u64 = messages[i + 1..].iter().map(estimate_message_tokens).sum();
            ContextEstimate {
                tokens: base + trailing,
                last_usage_index: Some(i),
            }
        }
        None => ContextEstimate {
            tokens: messages.iter().map(estimate_message_tokens).sum(),
            last_usage_index: None,
        },
    }
}

/// Whether the most-recent-assistant-`usage` anchor would over-report
/// occupancy for this entry path.
///
/// The anchor is stale exactly when a `Compaction` is the most recent
/// entry among {compaction, assistant message}: every retained
/// assistant message then predates the summary, so its `usage` still
/// reflects the old, pre-compaction prompt — the full summarized prefix
/// included — rather than the reduced projection that will actually be
/// sent next. Once a real assistant turn runs after the compaction,
/// that turn's `usage` measures the reduced context and the anchor is
/// trustworthy again.
fn usage_anchor_is_stale(entries: &[ConversationEntry]) -> bool {
    for entry in entries.iter().rev() {
        match &entry.entry {
            ConversationEntryKind::Compaction { .. } => return true,
            ConversationEntryKind::Message { message }
                if matches!(message.as_wire(), Some(Message::Assistant(_))) =>
            {
                return false;
            }
            _ => {}
        }
    }
    false
}

/// Estimate context occupancy for a linearized conversation, honoring
/// compaction.
///
/// Like [`estimate_context_tokens`] but compaction-aware. When a
/// compaction sits at the head (no assistant turn has run since it),
/// the retained tail's most recent assistant `usage` predates the
/// summary and would over-report by the entire summarized prefix; we
/// then estimate the projected messages (summary plus retained tail)
/// purely heuristically. Otherwise we defer to the usage-anchored
/// estimate over the projection, which is authoritative.
///
/// Both compaction `tokens_before` / `tokens_after` and the resumed
/// footer occupancy go through here, so the reported numbers match what
/// the next turn will actually send.
pub fn estimate_conversation_context(conversation: &Conversation) -> ContextEstimate {
    let messages = conversation.messages();
    if usage_anchor_is_stale(conversation.entries()) {
        ContextEstimate {
            tokens: messages.iter().map(estimate_message_tokens).sum(),
            last_usage_index: None,
        }
    } else {
        estimate_context_tokens(&messages)
    }
}

/// Whether occupancy has crossed the configured fraction of the window.
/// `threshold` is a fraction in (0, 1]; `context_tokens` and
/// `context_window` are absolute token counts.
#[allow(clippy::as_conversions)]
pub fn should_compact(context_tokens: u64, context_window: u64, threshold: f64) -> bool {
    context_window > 0 && (context_tokens as f64) > (context_window as f64) * threshold
}

/// Borrow the wire [`Message`] an entry carries, if it is a `Message`
/// entry. Settings / compaction / system-prompt entries yield `None`.
fn entry_wire(entry: &ConversationEntry) -> Option<&Message> {
    match &entry.entry {
        ConversationEntryKind::Message { message } => message.as_wire(),
        _ => None,
    }
}

fn is_user_message(entry: &ConversationEntry) -> bool {
    matches!(entry_wire(entry), Some(Message::User(_)))
}

fn is_assistant_message(entry: &ConversationEntry) -> bool {
    matches!(entry_wire(entry), Some(Message::Assistant(_)))
}

/// Walk backward from `cut_index` (exclusive) down to `boundary_start`
/// (inclusive) for the nearest user message — the turn that the cut
/// lands inside.
fn find_turn_start(
    entries: &[ConversationEntry],
    cut_index: usize,
    boundary_start: usize,
) -> Option<usize> {
    let mut i = cut_index;
    while i > boundary_start {
        i -= 1;
        if is_user_message(&entries[i]) {
            return Some(i);
        }
    }
    None
}

/// Choose the first retained entry given a `keep_recent_tokens` budget.
///
/// Valid cut points are user- or assistant-message starts in
/// `boundary_start..entries.len()`; a `tool_result` is never a cut
/// point, because keeping a result whose call was summarized away would
/// orphan it on the wire. The walk accumulates estimated tokens from
/// the head backward until the budget is reached, then snaps the cut to
/// the nearest valid cut point at or after that position. When the cut
/// lands on an assistant message mid-turn, `turn_start_index` carries
/// the turn's user start so the host can summarize the prefix
/// separately.
pub fn find_cut_point(
    entries: &[ConversationEntry],
    boundary_start: usize,
    keep_recent_tokens: u64,
) -> Option<CutPoint> {
    let valid: Vec<usize> = (boundary_start..entries.len())
        .filter(|&i| is_user_message(&entries[i]) || is_assistant_message(&entries[i]))
        .collect();
    if valid.is_empty() {
        return None;
    }

    // Accumulate tokens from the head backward; once the keep-recent
    // budget is reached, snap to the first valid cut point at or after
    // the position we stopped at (a `tool_result` there is skipped
    // forward to the next user/assistant message).
    let mut acc: u64 = 0;
    let mut cut_index: Option<usize> = None;
    let mut i = entries.len();
    while i > boundary_start {
        i -= 1;
        if let Some(m) = entry_wire(&entries[i]) {
            acc += estimate_message_tokens(m);
            if acc >= keep_recent_tokens {
                let snapped = valid
                    .iter()
                    .copied()
                    .find(|&v| v >= i)
                    .unwrap_or_else(|| *valid.last().expect("valid is non-empty"));
                cut_index = Some(snapped);
                break;
            }
        }
    }
    // The whole range fits within the budget: keep everything from the
    // earliest valid cut point (the host then sees an empty summary
    // range and declines to compact).
    let cut_index = cut_index.unwrap_or(valid[0]);

    let turn_start_index = if is_user_message(&entries[cut_index]) {
        None
    } else {
        find_turn_start(entries, cut_index, boundary_start)
    };

    Some(CutPoint {
        first_kept_entry_id: entries[cut_index].id.clone(),
        first_kept_index: cut_index,
        turn_start_index,
    })
}

/// Render a linearized message list into a plain-text transcript
/// (role-labeled, tool calls and results inlined as text, images noted
/// as placeholders) for embedding in a summarizer prompt. Feeding the
/// summarizer flattened text instead of the raw message list keeps the
/// request from tripping provider tool-call/tool-result pairing rules
/// and avoids re-sending image bytes.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(messages.len());
    for message in messages {
        match message {
            Message::User(u) => {
                let mut s = String::from("User:");
                append_user_content(&mut s, &u.content);
                parts.push(s);
            }
            Message::Assistant(a) => {
                let mut s = String::from("Assistant:");
                for block in &a.content {
                    match block {
                        AssistantContent::Text(t) => {
                            s.push('\n');
                            s.push_str(&t.text);
                        }
                        AssistantContent::Thinking(th) => {
                            s.push('\n');
                            s.push_str(&th.thinking);
                        }
                        AssistantContent::ToolCall(tc) => {
                            let args = serde_json::to_string(&tc.arguments).unwrap_or_default();
                            s.push_str(&format!("\n[tool call: {} {}]", tc.name, args));
                        }
                    }
                }
                parts.push(s);
            }
            Message::ToolResult(tr) => {
                let mut body = String::new();
                append_user_content(&mut body, &tr.content);
                let mut s = format!("Tool result ({}):", tr.tool_call_id);
                s.push_str(&truncate_for_summary(&body, TOOL_RESULT_MAX_CHARS));
                parts.push(s);
            }
        }
    }
    parts.join("\n")
}

/// Truncate `text` to `max_chars` characters, appending a marker noting
/// how many were dropped. Operates on `char` boundaries so multi-byte
/// text is never split mid-character.
fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!(
        "{kept}\n\n[... {} more characters truncated]",
        total - max_chars
    )
}

/// Append user/tool-result content blocks to `s`, one per line, noting
/// images as `[image: <mime>]` placeholders.
fn append_user_content(s: &mut String, content: &[UserContent]) {
    for block in content {
        match block {
            UserContent::Text(t) => {
                s.push('\n');
                s.push_str(&t.text);
            }
            UserContent::Image(img) => {
                s.push_str(&format!("\n[image: {}]", img.mime_type));
            }
        }
    }
}

/// Append the optional `/compact <instructions>` focus text.
fn append_custom_focus(prompt: &mut String, custom: Option<&str>) {
    if let Some(custom) = custom {
        prompt.push_str("\n\nAdditional focus: ");
        prompt.push_str(custom);
    }
}

/// Build the prompt for the first compaction on a thread. The
/// transcript comes first so the trailing instruction can refer to "the
/// messages above".
pub fn initial_summary_prompt(conversation_text: &str, custom: Option<&str>) -> String {
    let mut prompt = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{INITIAL_SUMMARY_INSTRUCTION}"
    );
    append_custom_focus(&mut prompt, custom);
    prompt
}

/// Build the prompt for a subsequent compaction: fold new messages into
/// the previous summary, preserving everything still relevant. The
/// transcript comes first so the trailing instruction can refer to "the
/// messages above".
pub fn update_summary_prompt(
    conversation_text: &str,
    previous_summary: &str,
    custom: Option<&str>,
) -> String {
    let mut prompt = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n<previous-summary>\n{previous_summary}\n</previous-summary>\n\n{UPDATE_SUMMARY_INSTRUCTION}"
    );
    append_custom_focus(&mut prompt, custom);
    prompt
}

/// Build the prompt for summarizing a split-turn prefix into a short
/// summary that gives the retained suffix its setup.
pub fn turn_prefix_summary_prompt(conversation_text: &str) -> String {
    format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\nThis is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\nWhat the user asked for at the start of this turn.\n\n## Early Progress\nKey decisions and work done in the summarized prefix.\n\n## Context for Suffix\nInformation the retained recent work depends on. Preserve exact file paths, function names, and error messages.\n\nBe concise. Focus on what's needed to understand the kept suffix."
    )
}

/// Scan summarized messages for builtin file operations and return the
/// resolved read/modified lists, carrying forward a prior compaction's
/// details so the running lists don't lose earlier files.
pub fn extract_file_ops(
    messages: &[Message],
    previous: Option<&CompactionDetails>,
) -> CompactionDetails {
    let mut read_files = previous.map(|p| p.read_files.clone()).unwrap_or_default();
    let mut modified_files = previous
        .map(|p| p.modified_files.clone())
        .unwrap_or_default();

    for message in messages {
        let Message::Assistant(a) = message else {
            continue;
        };
        for block in &a.content {
            let AssistantContent::ToolCall(tc) = block else {
                continue;
            };
            let Some(path) = tc.arguments.get("path").and_then(|v| v.as_str()) else {
                continue;
            };
            if READ_TOOLS.contains(&tc.name.as_str()) {
                read_files.push(path.to_string());
            } else if EDIT_TOOLS.contains(&tc.name.as_str()) {
                modified_files.push(path.to_string());
            }
        }
    }

    read_files.sort();
    read_files.dedup();
    modified_files.sort();
    modified_files.dedup();

    CompactionDetails {
        read_files,
        modified_files,
    }
}

/// Build a plan, or `None` when compaction is not applicable (empty
/// view, the last entry is already a `Compaction`, no valid cut point,
/// or nothing to summarize).
///
/// The summarized range starts at the previous compaction's
/// `first_kept_entry_id` when one exists (so a second compaction folds
/// only new history into the previous summary), else at the thread
/// root. This does not call the model — the host generates the summary
/// from the returned ranges.
pub fn prepare_compaction(
    conversation: &Conversation,
    keep_recent_tokens: u64,
) -> Option<CompactionPlan> {
    let entries = conversation.entries();
    if entries.is_empty() {
        return None;
    }
    if matches!(
        entries.last().map(|e| &e.entry),
        Some(ConversationEntryKind::Compaction { .. })
    ) {
        return None;
    }

    // Locate the previous compaction (the last one before the head).
    // Its `first_kept_entry_id` bounds the new summarized range and its
    // `summary` / `details` feed forward.
    let prev_compaction = entries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(c, e)| match &e.entry {
            ConversationEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                details,
                ..
            } => Some((
                c,
                summary.clone(),
                first_kept_entry_id.clone(),
                details.clone(),
            )),
            _ => None,
        });

    let (boundary_start, previous_summary, previous_details) = match prev_compaction {
        Some((c, summary, first_kept, details)) => {
            let boundary = entries
                .iter()
                .position(|e| e.id == first_kept)
                .unwrap_or(c + 1);
            (boundary, Some(summary), details)
        }
        None => (0, None, None),
    };

    // `tokens_before` is the current (compaction-aware) occupancy.
    let tokens_before = estimate_conversation_context(conversation).tokens;

    let cut = find_cut_point(entries, boundary_start, keep_recent_tokens)?;
    let history_end = cut.turn_start_index.unwrap_or(cut.first_kept_index);

    let messages_to_summarize: Vec<Message> = entries[boundary_start..history_end]
        .iter()
        .filter_map(|e| match &e.entry {
            ConversationEntryKind::Message { message } => message.as_wire().cloned(),
            _ => None,
        })
        .collect();

    // Nothing accumulated before the cut: the session is too small to
    // benefit from a summary, so decline rather than write an empty one.
    if messages_to_summarize.is_empty() {
        return None;
    }

    let turn_prefix_messages: Vec<Message> = match cut.turn_start_index {
        Some(turn_start) => entries[turn_start..cut.first_kept_index]
            .iter()
            .filter_map(|e| match &e.entry {
                ConversationEntryKind::Message { message } => message.as_wire().cloned(),
                _ => None,
            })
            .collect(),
        None => Vec::new(),
    };

    let mut all_summarized = messages_to_summarize.clone();
    all_summarized.extend(turn_prefix_messages.iter().cloned());
    let file_ops = extract_file_ops(&all_summarized, previous_details.as_ref());

    Some(CompactionPlan {
        first_kept_entry_id: cut.first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        previous_summary,
        tokens_before,
        file_ops,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{ConversationEntry, ConversationEntryKind, ThreadKind};
    use aj_models::types::{
        AssistantContent, AssistantMessage, TextContent, ToolCall, ToolResultMessage, Usage,
        UserContent, UserMessage,
    };
    use serde_json::json;

    fn msg_entry(id: &str, message: Message) -> ConversationEntry {
        ConversationEntry {
            id: id.to_string(),
            parent_id: None,
            timestamp: None,
            thread: ThreadKind::User,
            agent_id: None,
            entry: ConversationEntryKind::Message {
                message: AgentMessage::wire(message),
            },
        }
    }

    fn user(text: &str) -> Message {
        Message::User(UserMessage::text(text))
    }

    fn assistant_text(text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            ..AssistantMessage::empty()
        })
    }

    /// Assistant message whose `usage` reports a prompt of `base`
    /// tokens (all as plain `input`).
    fn assistant_with_usage(text: &str, base: u64) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            usage: Usage {
                input: base,
                ..Usage::default()
            },
            ..AssistantMessage::empty()
        })
    }

    fn compaction_entry(id: &str, first_kept: &str, summary: &str) -> ConversationEntry {
        ConversationEntry {
            id: id.to_string(),
            parent_id: None,
            timestamp: None,
            thread: ThreadKind::User,
            agent_id: None,
            entry: ConversationEntryKind::Compaction {
                summary: summary.to_string(),
                first_kept_entry_id: first_kept.to_string(),
                tokens_before: 0,
                details: None,
            },
        }
    }

    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: args,
            })],
            ..AssistantMessage::empty()
        })
    }

    fn tool_result(id: &str, name: &str, body: &str) -> Message {
        Message::ToolResult(ToolResultMessage::text(id, name, body, false))
    }

    /// A two-turn conversation: a large first turn, then a tool-using
    /// second turn (user, tool_call, tool_result, assistant).
    fn sample_entries() -> Vec<ConversationEntry> {
        vec![
            msg_entry("0", user(&"x".repeat(400))),           // 100 tokens
            msg_entry("1", assistant_text(&"y".repeat(400))), // 100 tokens
            msg_entry("2", user("hi")),
            msg_entry("3", tool_call("c1", "read_file", json!({"path": "/tmp/x"}))),
            msg_entry("4", tool_result("c1", "read_file", "ok")),
            msg_entry("5", assistant_text("done")),
        ]
    }

    #[test]
    fn estimate_context_prefers_usage_and_adds_trailing() {
        let assistant = Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::text("hello")],
            usage: Usage {
                input: 1000,
                cache_read: 200,
                cache_write: 50,
                ..Usage::default()
            },
            ..AssistantMessage::empty()
        });
        // 40-char trailing user message -> ceil(40 / 4) = 10 tokens.
        let trailing = user(&"a".repeat(40));
        let est = estimate_context_tokens(&[user("hi"), assistant, trailing]);
        assert_eq!(est.last_usage_index, Some(1));
        assert_eq!(est.tokens, 1250 + 10);
    }

    #[test]
    fn estimate_context_falls_back_to_heuristic_without_usage() {
        // Both messages have zero usage, so the whole list is estimated.
        let est = estimate_context_tokens(&[user("aaaa"), assistant_text("bbbb")]);
        assert_eq!(est.last_usage_index, None);
        assert_eq!(est.tokens, 2);
    }

    #[test]
    fn estimate_conversation_context_ignores_stale_usage_after_compaction() {
        // A compaction at the head: the retained tail's assistant still
        // carries the pre-compaction 100k usage. That anchor is stale
        // (it counts the now-summarized prefix), so the estimate must
        // fall back to the heuristic over the projection (summary +
        // short retained tail), not report ~100k.
        let entries = vec![
            msg_entry("0", user("old request")),
            msg_entry("1", assistant_with_usage("old reply", 100_000)),
            msg_entry("2", user("recent request")),
            msg_entry("3", assistant_with_usage("recent reply", 100_000)),
            compaction_entry("4", "2", "SUMMARY"),
        ];
        let conv = Conversation::from_entries("t".to_string(), entries);
        let est = estimate_conversation_context(&conv);
        assert_eq!(est.last_usage_index, None);
        assert!(
            est.tokens < 1_000,
            "expected a small heuristic, got {}",
            est.tokens
        );
    }

    #[test]
    fn estimate_conversation_context_uses_usage_after_post_compaction_turn() {
        // A real turn ran after the compaction; its 5k usage measures
        // the reduced context and is authoritative again.
        let entries = vec![
            msg_entry("0", user("old request")),
            msg_entry("1", assistant_with_usage("old reply", 100_000)),
            compaction_entry("2", "3", "SUMMARY"),
            msg_entry("3", user("new request")),
            msg_entry("4", assistant_with_usage("new reply", 5_000)),
        ];
        let conv = Conversation::from_entries("t".to_string(), entries);
        let est = estimate_conversation_context(&conv);
        assert!(est.last_usage_index.is_some());
        assert!(
            (5_000..6_000).contains(&est.tokens),
            "expected ~5k usage anchor, got {}",
            est.tokens
        );
    }

    #[test]
    fn should_compact_threshold_and_zero_window() {
        // Strictly greater: exactly at the threshold does not trigger.
        assert!(!should_compact(85, 100, 0.85));
        assert!(should_compact(86, 100, 0.85));
        // A zero window never triggers.
        assert!(!should_compact(100, 0, 0.85));
    }

    #[test]
    fn find_cut_point_snaps_off_tool_result_to_turn_start() {
        let entries = sample_entries();
        // Small budget: the backward walk reaches the budget at the
        // tool_result (index 4), which snaps forward to the assistant
        // at index 5.
        let cut = find_cut_point(&entries, 0, 2).expect("cut point");
        assert_eq!(cut.first_kept_index, 5);
        assert_eq!(cut.first_kept_entry_id, "5");
        // The chosen index is never a tool_result.
        assert!(!matches!(
            entry_wire(&entries[cut.first_kept_index]),
            Some(Message::ToolResult(_))
        ));
        // Index 5 is mid-turn (assistant), so the cut splits turn 2; its
        // user turn-start is index 2.
        assert_eq!(cut.turn_start_index, Some(2));
    }

    #[test]
    fn find_cut_point_snaps_to_user_turn_start() {
        let entries = sample_entries();
        // Budget reaches the user at index 2 exactly; a user message is
        // itself a turn start, so the cut does not split a turn.
        let cut = find_cut_point(&entries, 0, 10).expect("cut point");
        assert_eq!(cut.first_kept_index, 2);
        assert_eq!(cut.turn_start_index, None);
    }

    #[test]
    fn extract_file_ops_reads_edits_and_carries_previous() {
        let messages = vec![
            tool_call("c1", "read_file", json!({"path": "/a.rs"})),
            tool_call("c2", "edit_file", json!({"path": "/b.rs"})),
            tool_call("c3", "write_file", json!({"path": "/c.rs"})),
            tool_call("c4", "edit_file_multi", json!({"path": "/b.rs"})),
        ];
        let previous = CompactionDetails {
            read_files: vec!["/old_read.rs".into()],
            modified_files: vec!["/old_mod.rs".into()],
        };
        let ops = extract_file_ops(&messages, Some(&previous));
        assert_eq!(
            ops.read_files,
            vec!["/a.rs".to_string(), "/old_read.rs".to_string()]
        );
        assert_eq!(
            ops.modified_files,
            vec![
                "/b.rs".to_string(),
                "/c.rs".to_string(),
                "/old_mod.rs".to_string(),
            ]
        );
    }

    #[test]
    fn serialize_conversation_inlines_tool_calls_and_results() {
        let messages = vec![
            user("hello"),
            tool_call("c1", "read_file", json!({"path": "/x"})),
            tool_result("c1", "read_file", "file body"),
        ];
        let text = serialize_conversation(&messages);
        assert!(text.contains("User:"));
        assert!(text.contains("hello"));
        assert!(text.contains("[tool call: read_file"));
        assert!(text.contains("/x"));
        assert!(text.contains("Tool result (c1):"));
        assert!(text.contains("file body"));
    }

    #[test]
    fn serialize_conversation_notes_images() {
        let img = Message::User(UserMessage::new(vec![UserContent::image(
            "base64data",
            "image/png",
        )]));
        let text = serialize_conversation(&[img]);
        assert!(text.contains("[image: image/png]"));
    }

    #[test]
    fn serialize_conversation_truncates_large_tool_results() {
        let big = "z".repeat(10_000);
        let text = serialize_conversation(&[tool_result("c1", "read_file", &big)]);
        assert!(text.contains("more characters truncated"));
        // The body is bounded near the cap, well under the original size.
        assert!(text.chars().count() < TOOL_RESULT_MAX_CHARS + 200);
    }
}
