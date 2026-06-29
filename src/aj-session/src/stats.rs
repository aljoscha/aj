//! Aggregate summary of a conversation log.
//!
//! [`SessionStats`] is the read-only digest behind a "session info" view:
//! identity (id, on-disk path), timing, message counts broken out by kind,
//! a per-tool call breakdown, aggregate token usage and dollar cost, and
//! the settings the session is running with. It is computed in one pass
//! over every entry across all threads, so the message, tool-call, and
//! usage totals include sub-agent activity.

use std::collections::HashMap;
use std::path::PathBuf;

use aj_models::types::{AssistantContent, Message, Usage};
use chrono::{DateTime, Utc};

use crate::log::{ConversationEntryKind, ConversationLog, SessionSettings, ThreadFilter};
use crate::persistence::parse_session_id_created_at;

/// A read-only digest of a [`ConversationLog`].
///
/// All counts span every thread in the file (the user conversation plus
/// any sub-agent threads), so they describe the whole session rather than
/// a single thread. `settings` is the exception: it reflects the user
/// thread's current values.
///
/// Counts are file-level. They include entries the projected conversation
/// no longer shows, such as messages a compaction summarized away or a
/// branch the user moved off. They describe what the log holds on disk,
/// not what is currently in the model's context.
#[derive(Debug, Clone)]
pub struct SessionStats {
    /// The id the session is listed under (`aj list-sessions`).
    pub session_id: String,
    /// The on-disk JSONL file backing the session.
    pub path: PathBuf,
    /// Creation time parsed from the `session_id` stem. `None` when the
    /// id is not a minted timestamp.
    pub created_at: Option<DateTime<Utc>>,
    /// Timestamp of the most recent message entry, i.e. the last time the
    /// session saw activity. `None` for a log with no timestamped messages.
    pub last_activity: Option<DateTime<Utc>>,
    /// Size of the backing file. `None` when the file does not exist yet
    /// (a fresh log whose only entries are still buffered in memory).
    pub size_bytes: Option<u64>,
    /// Every entry in the file: messages, settings records, the system
    /// prompt, sub-agent roots, and compaction checkpoints.
    pub total_entries: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_results: usize,
    /// Total tool calls, counted as `tool_call` blocks inside assistant
    /// messages (one assistant message can carry several).
    pub tool_calls: usize,
    /// Per-tool call counts, sorted most-used first and ties broken by
    /// name so the order is stable across runs.
    pub tool_call_counts: Vec<(String, usize)>,
    /// Sub-agents spawned in this session.
    pub subagents: usize,
    /// Compaction checkpoints recorded in this session.
    pub compactions: usize,
    /// Aggregate token usage and dollar cost, summed over every assistant
    /// message in the file. Like the other counts this spans all threads
    /// and branches, so it reflects total spend on the session rather than
    /// the cost of the currently projected conversation. The cost figures
    /// are the per-response amounts recorded when each response arrived, so
    /// a model whose pricing was unknown contributes zero and a non-trivial
    /// token count can still report a zero cost.
    pub usage: Usage,
    /// Model / thinking / speed currently recorded on the user thread.
    pub settings: SessionSettings,
}

impl ConversationLog {
    /// Compute the aggregate [`SessionStats`] for this log.
    ///
    /// One pass over every entry in every thread, so message and
    /// tool-call totals include sub-agent activity. `settings` is read
    /// from the user thread's latest leaf via [`Conversation::settings`].
    ///
    /// [`Conversation::settings`]: crate::log::Conversation::settings
    pub fn stats(&self) -> SessionStats {
        let mut user_messages = 0;
        let mut assistant_messages = 0;
        let mut tool_results = 0;
        let mut tool_calls = 0;
        let mut subagents = 0;
        let mut compactions = 0;
        let mut total_entries = 0;
        let mut usage = Usage::default();
        let mut last_activity: Option<DateTime<Utc>> = None;
        let mut per_tool: HashMap<String, usize> = HashMap::new();

        for entry in self.entries_in_order() {
            total_entries += 1;
            match &entry.entry {
                ConversationEntryKind::Message { message } => {
                    if let Some(ts) = entry.timestamp {
                        last_activity = Some(last_activity.map_or(ts, |cur| cur.max(ts)));
                    }
                    match message.as_wire() {
                        Some(Message::User(_)) => user_messages += 1,
                        Some(Message::Assistant(a)) => {
                            assistant_messages += 1;
                            usage.accumulate(&a.usage);
                            for content in &a.content {
                                if let AssistantContent::ToolCall(call) = content {
                                    tool_calls += 1;
                                    *per_tool.entry(call.name.clone()).or_insert(0) += 1;
                                }
                            }
                        }
                        Some(Message::ToolResult(_)) => tool_results += 1,
                        None => {}
                    }
                }
                ConversationEntryKind::SubAgentSpawn { .. } => subagents += 1,
                ConversationEntryKind::Compaction { .. } => compactions += 1,
                _ => {}
            }
        }

        let mut tool_call_counts: Vec<(String, usize)> = per_tool.into_iter().collect();
        tool_call_counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let settings = self
            .latest_leaf(ThreadFilter::USER)
            .map(|head| self.linearize(&head, ThreadFilter::USER).settings())
            .unwrap_or_default();

        SessionStats {
            session_id: self.session_id().to_string(),
            path: self.path().to_path_buf(),
            created_at: parse_session_id_created_at(self.session_id()),
            last_activity,
            size_bytes: std::fs::metadata(self.path()).ok().map(|m| m.len()),
            total_entries,
            user_messages,
            assistant_messages,
            tool_results,
            tool_calls,
            tool_call_counts,
            subagents,
            compactions,
            usage,
            settings,
        }
    }
}

#[cfg(test)]
mod tests {
    use aj_agent::message::AgentMessage;
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, StopReason, TextContent, ToolCall,
        ToolResultMessage, Usage, UserContent, UserMessage,
    };
    use serde_json::json;

    use crate::log::{ConversationLog, ConversationView, ThreadFilter};
    use crate::persistence::ConversationPersistence;

    fn text(body: &str) -> TextContent {
        TextContent {
            text: body.to_string(),
            text_signature: None,
        }
    }

    fn user(body: &str) -> Message {
        Message::User(UserMessage {
            content: vec![UserContent::Text(text(body))],
            timestamp: 0,
        })
    }

    fn assistant_with_calls(calls: &[&str]) -> Message {
        let mut content = vec![AssistantContent::Text(text("ok"))];
        for (i, name) in calls.iter().enumerate() {
            content.push(AssistantContent::ToolCall(ToolCall {
                id: format!("call-{i}"),
                name: name.to_string(),
                arguments: json!({}),
            }));
        }
        Message::Assistant(AssistantMessage {
            content,
            api: "test".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-test".to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        })
    }

    fn tool_result(id: &str, name: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            content: vec![UserContent::Text(text("done"))],
            details: None,
            is_error: false,
            timestamp: 0,
        })
    }

    /// Build a small user-thread conversation and assert the digest counts
    /// each message kind, every tool call, and ranks the per-tool breakdown.
    #[test]
    fn stats_count_messages_and_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let mut log = ConversationLog::create(&persistence).unwrap();

        let mut head = ConversationView::user(&mut log, None);
        head.add_message(AgentMessage::wire(user("hi"))).unwrap();
        head.add_message(AgentMessage::wire(assistant_with_calls(&[
            "read_file",
            "read_file",
            "Bash",
        ])))
        .unwrap();
        head.add_message(AgentMessage::wire(tool_result("call-0", "read_file")))
            .unwrap();
        head.add_message(AgentMessage::wire(tool_result("call-1", "read_file")))
            .unwrap();
        head.add_message(AgentMessage::wire(tool_result("call-2", "Bash")))
            .unwrap();
        head.add_message(AgentMessage::wire(assistant_with_calls(&["read_file"])))
            .unwrap();

        let stats = log.stats();
        assert_eq!(stats.user_messages, 1);
        assert_eq!(stats.assistant_messages, 2);
        assert_eq!(stats.tool_results, 3);
        assert_eq!(stats.tool_calls, 4);
        // read_file (3) outranks Bash (1). Ties would break by name.
        assert_eq!(
            stats.tool_call_counts,
            vec![("read_file".to_string(), 3), ("Bash".to_string(), 1)]
        );
        assert_eq!(stats.session_id, log.session_id());
        assert_eq!(stats.path, log.path());
        assert!(stats.last_activity.is_some());
    }

    #[test]
    fn stats_empty_log_is_all_zero() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let log = ConversationLog::create(&persistence).unwrap();
        let stats = log.stats();
        assert_eq!(stats.user_messages, 0);
        assert_eq!(stats.assistant_messages, 0);
        assert_eq!(stats.tool_results, 0);
        assert_eq!(stats.tool_calls, 0);
        assert!(stats.tool_call_counts.is_empty());
        assert!(stats.last_activity.is_none());
        assert!(log.latest_leaf(ThreadFilter::USER).is_none());
    }

    /// Build an assistant message carrying explicit token usage and a
    /// total dollar cost, used to exercise the per-session aggregation.
    fn assistant_with_usage(input: u64, output: u64, cost_total: f64) -> Message {
        let mut usage = Usage {
            input,
            output,
            total_tokens: input + output,
            ..Usage::default()
        };
        usage.cost.total = cost_total;
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(text("ok"))],
            api: "test".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-test".to_string(),
            response_id: None,
            usage,
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        })
    }

    /// The digest sums token usage and dollar cost across every assistant
    /// message in the file.
    #[test]
    fn stats_aggregate_usage_across_assistant_messages() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let mut log = ConversationLog::create(&persistence).unwrap();

        let mut head = ConversationView::user(&mut log, None);
        head.add_message(AgentMessage::wire(user("hi"))).unwrap();
        head.add_message(AgentMessage::wire(assistant_with_usage(100, 50, 0.10)))
            .unwrap();
        head.add_message(AgentMessage::wire(assistant_with_usage(200, 80, 0.25)))
            .unwrap();

        let stats = log.stats();
        assert_eq!(stats.usage.input, 300);
        assert_eq!(stats.usage.output, 130);
        assert_eq!(stats.usage.total_tokens, 430);
        assert!((stats.usage.cost.total - 0.35).abs() < 1e-9);
    }
}
