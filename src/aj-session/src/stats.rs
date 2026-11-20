//! Aggregate summary of a session.
//!
//! [`SessionStats`] is the read-only digest behind a "session info" view:
//! identity (id, project, the database that stores it), timing, message
//! counts broken out by kind, a per-tool call breakdown, aggregate token
//! usage and dollar cost, a usage breakdown per provider, model, account,
//! and served speed, and the settings the session is running with. It is
//! computed in one pass over every entry of the session, so the message,
//! tool-call, and usage totals include sub-agent activity and branches the
//! user moved off. A log that holds only part of its session reads the
//! rest from the database for this.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use aj_models::types::{AssistantContent, Message, Usage};
use chrono::{DateTime, Utc};

use crate::log::{LogEntryKind, SessionLog, SessionSettings, ThreadFilter};
use crate::persistence::parse_session_id_created_at;

/// Usage and response counts for one provider, model, account, and served speed.
#[derive(Debug, Clone)]
pub struct UsageBucket {
    /// Provider recorded on the responses.
    pub provider: String,
    /// Model recorded on the responses.
    pub model: String,
    /// Account label recorded on the responses, when available.
    pub account: Option<String>,
    /// Accumulated token usage and recorded cost for the responses.
    /// `served_speed` identifies this homogeneous bucket, not the requested speed.
    pub usage: Usage,
    /// Number of assistant responses in this bucket.
    pub responses: usize,
    /// Responses that carried tokens but no recorded cost.
    pub unpriced_responses: usize,
}

fn compare_usage_buckets(a: &UsageBucket, b: &UsageBucket) -> Ordering {
    b.usage
        .cost
        .total
        .total_cmp(&a.usage.cost.total)
        .then_with(|| b.usage.total_tokens.cmp(&a.usage.total_tokens))
        .then_with(|| a.provider.cmp(&b.provider))
        .then_with(|| a.model.cmp(&b.model))
        .then_with(|| a.account.cmp(&b.account))
        .then_with(|| a.usage.served_speed.cmp(&b.usage.served_speed))
}

/// A read-only digest of a [`SessionLog`].
///
/// All counts span every thread of the session (the main thread plus
/// any sub-agent threads), so they describe the whole session rather than
/// a single thread. `settings` is the exception: it reflects the user
/// thread's current values.
///
/// Counts are session-level. They include entries the projected
/// thread no longer shows, such as messages a compaction summarized
/// away or a branch the user moved off. They describe what the store
/// holds, not what is currently in the model's context.
#[derive(Debug, Clone)]
pub struct SessionStats {
    /// The id the session is listed under (`aj list-sessions`).
    pub session_id: String,
    /// The database that stores the session, shared with other sessions.
    pub path: PathBuf,
    /// The project the session is stored under.
    pub project: String,
    /// Creation time parsed from the `session_id` stem. `None` when the
    /// id is not a minted timestamp.
    pub created_at: Option<DateTime<Utc>>,
    /// Timestamp of the most recent message entry, i.e. the last time the
    /// session saw activity. `None` for a log with no timestamped messages.
    pub last_activity: Option<DateTime<Utc>>,
    /// Summed length of the session's stored entries. `None` when the
    /// session is not stored yet (a fresh log whose only entries are still
    /// in memory).
    pub size_bytes: Option<u64>,
    /// Every entry in the session: messages, state entries, the system
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
    /// Aggregate token usage and dollar cost for the session: every
    /// assistant message in the file, plus the summarizer spend recorded
    /// on its compaction entries. Like the other counts this spans all
    /// threads and branches, so it reflects total spend on the session
    /// rather than the cost of the currently projected thread. The
    /// cost figures are the per-response amounts recorded when each
    /// response arrived, so a model whose pricing was unknown
    /// contributes zero and a non-trivial token count can still report a
    /// zero cost.
    pub usage: Usage,
    /// Assistant-response usage grouped by provider, model, optional account,
    /// and served speed recorded on each response. Buckets span every thread and
    /// branch, including sub-agent threads. Compaction spend is excluded
    /// because its entries identify no provider or model. Responses without
    /// an account label or served speed remain in distinct `None` buckets.
    /// Buckets are sorted by cost descending, then tokens descending, then the
    /// full provider, model, account, and served speed key ascending.
    pub usage_breakdown: Vec<UsageBucket>,
    /// The share of `usage` spent on compaction summaries rather than on
    /// the thread itself, summed from the compaction entries that
    /// recorded any.
    pub compaction_usage: Usage,
    /// Model, thinking, speed, and verbosity currently recorded on the user
    /// thread.
    pub settings: SessionSettings,
    /// Complete environment on the active user branch. `None` differs from a
    /// recorded empty map.
    pub session_env: Option<BTreeMap<String, String>>,
}

impl SessionLog {
    /// Compute the aggregate [`SessionStats`] for this log.
    ///
    /// One pass over every entry in every thread, so message and
    /// tool-call totals include sub-agent activity. `settings` is read
    /// from the main thread's current head via [`BranchView::settings`],
    /// so it reflects the active branch.
    ///
    /// [`BranchView::settings`]: crate::log::BranchView::settings
    ///
    /// A log that does not hold its whole session reads it for this, which
    /// is blocking database I/O. A caller that must not hold the log for
    /// that long reads [`Self::whole`] itself and passes it to
    /// [`Self::stats_over`].
    pub fn stats(&self) -> Result<SessionStats, crate::log::SessionError> {
        Ok(self.stats_over(&self.whole().read()?))
    }

    /// [`Self::stats`] over `whole`, this log's whole session.
    pub fn stats_over(&self, whole: &crate::log::LogSnapshot) -> SessionStats {
        let mut user_messages = 0;
        let mut assistant_messages = 0;
        let mut tool_results = 0;
        let mut tool_calls = 0;
        let mut subagents = 0;
        let mut compactions = 0;
        let mut total_entries = 0;
        let mut usage = Usage::default();
        let mut usage_buckets = HashMap::new();
        let mut compaction_usage = Usage::default();
        let mut last_activity: Option<DateTime<Utc>> = None;
        let mut per_tool: HashMap<String, usize> = HashMap::new();

        for entry in whole.entries_in_order() {
            total_entries += 1;
            match &entry.entry {
                LogEntryKind::Message { message } => {
                    if let Some(ts) = entry.timestamp {
                        last_activity = Some(last_activity.map_or(ts, |cur| cur.max(ts)));
                    }
                    match message.as_stored_wire() {
                        Some(Message::User(_)) => user_messages += 1,
                        Some(Message::Assistant(a)) => {
                            assistant_messages += 1;
                            usage.accumulate(&a.usage);
                            let account = a.account.clone();
                            let key = (
                                a.provider.clone(),
                                a.model.clone(),
                                account.clone(),
                                a.usage.served_speed.clone(),
                            );
                            let bucket = usage_buckets.entry(key).or_insert_with(|| UsageBucket {
                                provider: a.provider.clone(),
                                model: a.model.clone(),
                                account,
                                usage: Usage {
                                    served_speed: a.usage.served_speed.clone(),
                                    ..Usage::default()
                                },
                                responses: 0,
                                unpriced_responses: 0,
                            });
                            bucket.usage.accumulate(&a.usage);
                            bucket.responses += 1;
                            if a.usage.total_tokens > 0 && a.usage.cost.total == 0.0 {
                                bucket.unpriced_responses += 1;
                            }
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
                LogEntryKind::SubAgentSpawn { .. } => subagents += 1,
                LogEntryKind::Compaction {
                    usage: entry_usage, ..
                } => {
                    compactions += 1;
                    usage.accumulate(entry_usage);
                    compaction_usage.accumulate(entry_usage);
                }
                _ => {}
            }
        }

        let mut tool_call_counts: Vec<(String, usize)> = per_tool.into_iter().collect();
        tool_call_counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let mut usage_breakdown: Vec<UsageBucket> = usage_buckets.into_values().collect();
        usage_breakdown.sort_by(compare_usage_buckets);

        let settings = whole
            .head()
            .map(|head| whole.linearize(head, ThreadFilter::MAIN).settings())
            .unwrap_or_default();

        SessionStats {
            session_id: self.session_id().to_string(),
            path: self.path().to_path_buf(),
            project: self.project_id().to_owned(),
            created_at: parse_session_id_created_at(self.session_id()),
            last_activity,
            size_bytes: self.stored_bytes().ok().flatten(),
            total_entries,
            user_messages,
            assistant_messages,
            tool_results,
            tool_calls,
            tool_call_counts,
            subagents,
            compactions,
            usage,
            usage_breakdown,
            compaction_usage,
            settings,
            session_env: whole.session_env().cloned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use aj_agent::events::AgentSettings;
    use aj_agent::message::AgentMessage;
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, StopReason, TextContent, ToolCall,
        ToolResultMessage, Usage, UserContent, UserMessage,
    };
    use serde_json::json;

    use crate::log::{
        AgentLog, LogEntryKind, SessionLog, SessionSettings, ThreadFilter, ThreadKind,
    };
    use crate::persistence::ProjectSessions;

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
                is_raw: false,
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
            account: None,
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

    /// Build a small main thread and assert the digest counts
    /// each message kind, every tool call, and ranks the per-tool breakdown.
    #[test]
    fn stats_count_messages_and_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).unwrap();

        let mut head = AgentLog::main(&mut log);
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

        let stats = log.stats().unwrap();
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
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let log = SessionLog::create(&persistence).unwrap();
        let stats = log.stats().unwrap();
        assert_eq!(stats.user_messages, 0);
        assert_eq!(stats.assistant_messages, 0);
        assert_eq!(stats.tool_results, 0);
        assert_eq!(stats.tool_calls, 0);
        assert!(stats.tool_call_counts.is_empty());
        assert!(stats.last_activity.is_none());
        assert_eq!(stats.session_env, None);
        assert!(log.latest_leaf(ThreadFilter::MAIN).is_none());
    }

    #[test]
    fn stats_preserves_an_explicitly_empty_session_env() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).unwrap();
        log.set_system_prompt("p".into()).expect("root");
        log.append_env_change(BTreeMap::new()).expect("empty env");

        assert_eq!(log.stats().unwrap().session_env, Some(BTreeMap::new()));
    }

    #[test]
    fn stats_reads_session_env_from_active_branch() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).unwrap();
        let expected = BTreeMap::from([("BEADS_ACTOR".to_string(), "session-actor".to_string())]);
        let root = log.set_system_prompt("p".into()).expect("root");
        let env = log.append_env_change(expected.clone()).expect("env");
        assert_eq!(log.stats().unwrap().session_env, Some(expected.clone()));
        log.set_head(root.id).expect("root head");

        let stats = log.stats().unwrap();
        assert_eq!(stats.settings, SessionSettings::default());
        assert_eq!(stats.session_env, None);
        log.set_head(env.id).expect("env head");
        assert_eq!(log.stats().unwrap().session_env, Some(expected));
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
        assistant_for("anthropic", "claude-test", usage)
    }

    fn assistant_for(provider: &str, model: &str, usage: Usage) -> Message {
        assistant_for_account(provider, model, None, usage)
    }

    fn assistant_for_account(
        provider: &str,
        model: &str,
        account: Option<&str>,
        usage: Usage,
    ) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(text("ok"))],
            api: "test".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            account: account.map(str::to_string),
            response_id: None,
            usage,
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        })
    }

    fn measured_usage(tokens: [u64; 4], costs: [f64; 5]) -> Usage {
        measured_usage_with_total(tokens, tokens.iter().sum(), costs)
    }

    fn measured_usage_with_total(tokens: [u64; 4], total_tokens: u64, costs: [f64; 5]) -> Usage {
        Usage {
            input: tokens[0],
            output: tokens[1],
            cache_read: tokens[2],
            cache_write: tokens[3],
            total_tokens,
            served_speed: None,
            cost: aj_models::types::UsageCost {
                input: costs[0],
                output: costs[1],
                cache_read: costs[2],
                cache_write: costs[3],
                total: costs[4],
            },
        }
    }

    fn spawn_settings() -> AgentSettings {
        AgentSettings {
            context_window: 0,
            provider: "zeta".to_string(),
            model_id: "tie-z".to_string(),
            thinking: "off".to_string(),
            thinking_display: String::new(),
            speed: "standard".to_string(),
            verbosity: "default".to_string(),
        }
    }

    /// Build a log holding one assistant message and one compaction
    /// checkpoint carrying `usage`, the shape a compacted session has on
    /// disk. Returns the log and the guard owning its directory.
    fn log_with_compaction(usage: Usage) -> (tempfile::TempDir, SessionLog) {
        let dir = tempfile::tempdir().unwrap();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).unwrap();
        let first_kept = {
            let mut head = AgentLog::main(&mut log);
            head.add_message(AgentMessage::wire(user("hi"))).unwrap();
            head.add_message(AgentMessage::wire(assistant_with_usage(100, 50, 0.10)))
                .unwrap()
                .id
        };
        log.append_compaction(
            ThreadFilter::MAIN,
            "summary".into(),
            first_kept,
            Vec::new(),
            1_000,
            None,
            usage,
        )
        .unwrap();
        (dir, log)
    }

    /// A compaction's own spend reaches both the session total and its
    /// own subtotal.
    ///
    /// The summarizer exchange is never a message entry, so this entry
    /// is the only place the spend exists: a fold that misses it loses
    /// the money rather than misplacing it.
    #[test]
    fn stats_folds_compaction_spend_into_the_total_and_its_subtotal() {
        let mut usage = Usage {
            input: 40_000,
            output: 900,
            total_tokens: 40_900,
            served_speed: Some("fast".into()),
            ..Usage::default()
        };
        usage.cost.total = 0.25;
        let (_dir, log) = log_with_compaction(usage);

        let stats = log.stats().unwrap();
        assert_eq!(stats.compactions, 1, "the fixture must record a compaction");
        // The total first: a reader who checks one number checks this one.
        assert!(
            (stats.usage.cost.total - 0.35).abs() < 1e-9,
            "session total is the turn's $0.10 plus the summarizer's $0.25, got {}",
            stats.usage.cost.total
        );
        assert_eq!(
            stats.usage.total_tokens, 41_050,
            "the turn's 150 tokens plus the summarizer's 40900"
        );
        // The four counts and not only the total. A fold that adds the
        // summarizer's `total_tokens` and dollars while leaving the counts
        // behind keeps this total right and stops the overlay's four token
        // rows from summing to it, which is ws-6w5's defect arriving through
        // the compaction door.
        assert_eq!(
            stats.usage.input, 40_100,
            "the turn's 100 input plus the summarizer's 40000"
        );
        assert_eq!(
            stats.usage.output, 950,
            "the turn's 50 output plus the summarizer's 900"
        );
        assert!(
            (stats.compaction_usage.cost.total - 0.25).abs() < 1e-9,
            "the compaction line reports the summarizer's share, got {}",
            stats.compaction_usage.cost.total
        );
        assert_eq!(stats.compaction_usage.total_tokens, 40_900);
        assert_eq!(stats.compaction_usage.served_speed, None);
        assert_eq!(stats.usage.served_speed, None);
        assert_eq!(
            stats.usage_breakdown.len(),
            1,
            "compaction spend never creates a usage bucket"
        );
        let bucket = &stats.usage_breakdown[0];
        assert_eq!(
            (
                bucket.provider.as_str(),
                bucket.model.as_str(),
                bucket.account.as_deref(),
                bucket.usage.total_tokens,
                bucket.usage.cost.total,
                bucket.responses,
            ),
            ("anthropic", "claude-test", None, 150, 0.10, 1),
            "the assistant response remains its own 150-token, $0.10 bucket"
        );
    }

    /// The digest sums token usage and dollar cost across every assistant
    /// message in the file.
    #[test]
    fn stats_aggregate_usage_across_assistant_messages() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).unwrap();

        let mut head = AgentLog::main(&mut log);
        head.add_message(AgentMessage::wire(user("hi"))).unwrap();
        head.add_message(AgentMessage::wire(assistant_with_usage(100, 50, 0.10)))
            .unwrap();
        let mut second = Usage {
            input: 200,
            output: 80,
            total_tokens: 280,
            ..Usage::default()
        };
        second.cost.total = 0.25;
        head.add_message(AgentMessage::wire(assistant_for(
            "anthropic",
            "claude-test",
            second,
        )))
        .unwrap();

        let stats = log.stats().unwrap();
        assert_eq!(stats.usage.input, 300);
        assert_eq!(stats.usage.output, 130);
        assert_eq!(stats.usage.total_tokens, 430);
        assert!((stats.usage.cost.total - 0.35).abs() < 1e-9);
    }

    /// Usage buckets preserve each response's identity across user branches
    /// and sub-agent threads, aggregate every usage dimension, count unpriced
    /// responses, and follow the ruled stable order.
    #[test]
    fn stats_breaks_usage_down_by_recorded_response_identity_across_the_whole_log() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).unwrap();

        let root = log
            .append(
                None,
                ThreadKind::Main,
                None,
                LogEntryKind::Message {
                    message: AgentMessage::wire(user("root")),
                },
            )
            .unwrap()
            .id;

        // This branch is abandoned below. Its two responses must remain in
        // the file-level buckets even though settings follows another head.
        let abandoned_high = log
            .append(
                Some(root.clone()),
                ThreadKind::Main,
                None,
                LogEntryKind::Message {
                    message: AgentMessage::wire(assistant_for(
                        "anthropic",
                        "high",
                        measured_usage([0, 20, 30, 50], [0.0, 1.0, 1.0, 3.0, 5.0]),
                    )),
                },
            )
            .unwrap()
            .id;
        log.append(
            Some(abandoned_high),
            ThreadKind::Main,
            None,
            LogEntryKind::Message {
                message: AgentMessage::wire(assistant_for(
                    "beta",
                    "high",
                    measured_usage([100, 100, 100, 100], [0.1, 0.2, 0.3, 1.4, 2.0]),
                )),
            },
        )
        .unwrap();

        log.set_head(root.clone()).unwrap();
        let active_high = log
            .append(
                Some(root),
                ThreadKind::Main,
                None,
                LogEntryKind::Message {
                    message: AgentMessage::wire(assistant_for(
                        "anthropic",
                        "high",
                        measured_usage_with_total([0, 0, 0, 0], 50, [1.0, 1.0, 1.0, 1.0, 0.0]),
                    )),
                },
            )
            .unwrap()
            .id;
        let zero_token = log
            .append(
                Some(active_high),
                ThreadKind::Main,
                None,
                LogEntryKind::Message {
                    message: AgentMessage::wire(assistant_for(
                        "anthropic",
                        "high",
                        Usage::default(),
                    )),
                },
            )
            .unwrap()
            .id;
        let personal = log
            .append(
                Some(zero_token),
                ThreadKind::Main,
                None,
                LogEntryKind::Message {
                    message: AgentMessage::wire(assistant_for_account(
                        "anthropic",
                        "high",
                        Some("personal"),
                        measured_usage([10, 20, 30, 40], [0.4, 0.6, 1.0, 2.0, 4.0]),
                    )),
                },
            )
            .unwrap()
            .id;
        let work = log
            .append(
                Some(personal),
                ThreadKind::Main,
                None,
                LogEntryKind::Message {
                    message: AgentMessage::wire(assistant_for_account(
                        "anthropic",
                        "high",
                        Some("work"),
                        measured_usage([20, 20, 20, 20], [0.3, 0.5, 0.7, 1.5, 3.0]),
                    )),
                },
            )
            .unwrap()
            .id;
        let active_head = log
            .append(
                Some(work),
                ThreadKind::Main,
                None,
                LogEntryKind::Message {
                    message: AgentMessage::wire(assistant_for(
                        "alpha",
                        "tie-a",
                        measured_usage([75, 75, 75, 75], [0.2, 0.4, 0.6, 0.8, 2.0]),
                    )),
                },
            )
            .unwrap()
            .id;

        let spawn = log
            .append_subagent_spawn(
                7,
                active_head,
                "measure",
                "agent",
                None,
                true,
                &spawn_settings(),
            )
            .unwrap();
        let subagent_tie = log
            .append(
                Some(spawn.id),
                ThreadKind::Sub,
                Some(7),
                LogEntryKind::Message {
                    message: AgentMessage::wire(assistant_for(
                        "alpha",
                        "tie-z",
                        measured_usage([60, 70, 80, 90], [0.5, 0.5, 0.5, 0.5, 2.0]),
                    )),
                },
            )
            .unwrap()
            .id;
        log.append(
            Some(subagent_tie),
            ThreadKind::Sub,
            Some(7),
            LogEntryKind::Message {
                message: AgentMessage::wire(assistant_for(
                    "zeta",
                    "tie-a",
                    measured_usage([90, 80, 70, 60], [0.8, 0.6, 0.4, 0.2, 2.0]),
                )),
            },
        )
        .unwrap();

        let stats = log.stats().unwrap();
        assert_eq!(
            stats.usage_breakdown.len(),
            7,
            "account labels must keep one provider/model pair in distinct usage rows"
        );
        let actual: Vec<_> = stats
            .usage_breakdown
            .iter()
            .map(|bucket| {
                (
                    bucket.provider.as_str(),
                    bucket.model.as_str(),
                    bucket.account.as_deref(),
                    (
                        bucket.usage.input,
                        bucket.usage.output,
                        bucket.usage.cache_read,
                        bucket.usage.cache_write,
                        bucket.usage.total_tokens,
                    ),
                    (
                        bucket.usage.cost.input,
                        bucket.usage.cost.output,
                        bucket.usage.cost.cache_read,
                        bucket.usage.cost.cache_write,
                        bucket.usage.cost.total,
                    ),
                    bucket.responses,
                    bucket.unpriced_responses,
                )
            })
            .collect();
        assert_eq!(
            actual,
            vec![
                (
                    "anthropic",
                    "high",
                    None,
                    (0, 20, 30, 50, 150),
                    (1.0, 2.0, 2.0, 4.0, 5.0),
                    3,
                    1,
                ),
                (
                    "anthropic",
                    "high",
                    Some("personal"),
                    (10, 20, 30, 40, 100),
                    (0.4, 0.6, 1.0, 2.0, 4.0),
                    1,
                    0,
                ),
                (
                    "anthropic",
                    "high",
                    Some("work"),
                    (20, 20, 20, 20, 80),
                    (0.3, 0.5, 0.7, 1.5, 3.0),
                    1,
                    0,
                ),
                (
                    "beta",
                    "high",
                    None,
                    (100, 100, 100, 100, 400),
                    (0.1, 0.2, 0.3, 1.4, 2.0),
                    1,
                    0,
                ),
                (
                    "alpha",
                    "tie-a",
                    None,
                    (75, 75, 75, 75, 300),
                    (0.2, 0.4, 0.6, 0.8, 2.0),
                    1,
                    0,
                ),
                (
                    "alpha",
                    "tie-z",
                    None,
                    (60, 70, 80, 90, 300),
                    (0.5, 0.5, 0.5, 0.5, 2.0),
                    1,
                    0,
                ),
                (
                    "zeta",
                    "tie-a",
                    None,
                    (90, 80, 70, 60, 300),
                    (0.8, 0.6, 0.4, 0.2, 2.0),
                    1,
                    0,
                ),
            ],
        );
        assert_eq!(stats.assistant_messages, 9);
        assert_eq!(stats.usage.total_tokens, 1_630);
        assert!((stats.usage.cost.total - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn resumed_stats_group_by_served_speed_not_requested_speed() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).unwrap();
        log.set_system_prompt("p".into()).unwrap();
        log.append_speed_change(ThreadFilter::MAIN, "fast").unwrap();

        // The first response was requested fast but actually served standard.
        // Equal bucket totals also exercise the served-speed ordering tie-break.
        for (speed, tokens, cost) in [
            (
                Some("standard"),
                [10, 20, 30, 40],
                [0.1, 0.2, 0.3, 0.4, 1.0],
            ),
            (Some("fast"), [5, 10, 15, 20], [0.05, 0.1, 0.15, 0.2, 0.5]),
            (None, [10, 20, 30, 40], [0.1, 0.2, 0.3, 0.4, 1.0]),
            (Some("fast"), [5, 10, 15, 20], [0.05, 0.1, 0.15, 0.2, 0.5]),
        ] {
            let mut usage = measured_usage(tokens, cost);
            usage.served_speed = speed.map(str::to_string);
            AgentLog::main(&mut log)
                .add_message(AgentMessage::wire(assistant_for_account(
                    "provider",
                    "model",
                    Some("work"),
                    usage,
                )))
                .unwrap();
        }
        let id = log.session_id().to_string();
        let stored_speeds: Vec<_> = persistence
            .read_entries(&id)
            .unwrap()
            .iter()
            .map(|entry| serde_json::to_value(entry).unwrap())
            .filter_map(|entry| entry.pointer("/message/usage").cloned())
            .map(|usage| usage.get("served_speed").cloned())
            .collect();
        assert_eq!(
            stored_speeds,
            vec![
                Some(json!("standard")),
                Some(json!("fast")),
                None,
                Some(json!("fast"))
            ],
            "the unknown response must omit the field, as legacy records do",
        );
        drop(log);

        let resumed = SessionLog::resume(&persistence, &id).unwrap();
        let stats = resumed.stats().unwrap();
        assert_eq!(stats.settings.speed.as_deref(), Some("fast"));
        let buckets: Vec<_> = stats
            .usage_breakdown
            .iter()
            .map(|bucket| {
                assert_eq!(bucket.provider, "provider");
                assert_eq!(bucket.model, "model");
                assert_eq!(bucket.account.as_deref(), Some("work"));
                assert_eq!(
                    (
                        bucket.usage.input,
                        bucket.usage.output,
                        bucket.usage.cache_read,
                        bucket.usage.cache_write,
                        bucket.usage.total_tokens
                    ),
                    (10, 20, 30, 40, 100)
                );
                assert_eq!(
                    (
                        bucket.usage.cost.input,
                        bucket.usage.cost.output,
                        bucket.usage.cost.cache_read,
                        bucket.usage.cost.cache_write,
                        bucket.usage.cost.total
                    ),
                    (0.1, 0.2, 0.3, 0.4, 1.0)
                );
                (
                    bucket.usage.served_speed.as_deref(),
                    bucket.responses,
                    bucket.unpriced_responses,
                )
            })
            .collect();
        assert_eq!(
            buckets,
            vec![(None, 1, 0), (Some("fast"), 2, 0), (Some("standard"), 1, 0)]
        );
        assert_eq!(stats.assistant_messages, 4);
        assert_eq!(
            (
                stats.usage.input,
                stats.usage.output,
                stats.usage.cache_read,
                stats.usage.cache_write,
                stats.usage.total_tokens
            ),
            (30, 60, 90, 120, 300)
        );
        assert_eq!(stats.usage.cost.total, 3.0);
        assert_eq!(stats.usage.served_speed, None);
    }

    #[test]
    fn usage_bucket_order_uses_the_optional_account_before_served_speed() {
        let bucket = |account: Option<&str>| super::UsageBucket {
            provider: "same-provider".to_string(),
            model: "same-model".to_string(),
            account: account.map(str::to_string),
            usage: measured_usage([25, 25, 25, 25], [0.0, 0.0, 0.0, 1.0, 1.0]),
            responses: 1,
            unpriced_responses: 0,
        };
        let mut buckets = [bucket(Some("zeta")), bucket(Some("alpha")), bucket(None)];

        buckets.sort_by(super::compare_usage_buckets);

        assert_eq!(
            buckets
                .iter()
                .map(|bucket| bucket.account.as_deref())
                .collect::<Vec<_>>(),
            vec![None, Some("alpha"), Some("zeta")],
        );
    }
}
