//! Host-driven context compaction: plan, summarize, persist, reseed.
//!
//! The pure planning lives in `aj_session::compaction`; this module is
//! the orchestration that the manual `/compact` command, the `compact`
//! CLI subcommand, and the auto/overflow triggers all call. It locks
//! the log to plan, runs a bus-silent summarizer inference on the
//! agent, records a compaction entry, and reseeds the agent's
//! transcript from the post-compaction projection.

use std::sync::Arc;

use aj_agent::events::{AgentEvent, AgentId, CompactionPhase, CompactionReason};
use aj_agent::message::AgentMessage;
use aj_agent::{Agent, TurnError};
use aj_models::types::{AssistantContent, AssistantMessage, Message, StopReason, Usage};
use aj_session::compaction as planning;
use aj_session::{AppendHandoff, ConversationLog, ThreadFilter};
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

/// Upper bound on summary output tokens, clamped against the model's
/// own `max_tokens`.
const SUMMARY_OUTPUT_CAP: u64 = 8192;

/// The text blocks of a summarizer response, concatenated.
fn assistant_text(message: &AssistantMessage) -> String {
    let mut out = String::new();
    for block in &message.content {
        if let AssistantContent::Text(t) = block {
            out.push_str(&t.text);
        }
    }
    out
}

/// Outcome of a compaction run, for callers that render text (the CLI)
/// rather than relying on the emitted events (the TUI).
#[derive(Debug)]
pub enum CompactionOutcome {
    /// Compaction ran. Token counts are estimated occupancy.
    Compacted {
        tokens_before: u64,
        tokens_after: u64,
    },
    /// Nothing to compact (session too small or already compacted).
    NothingToDo,
    /// Compaction was cancelled before anything was persisted.
    Canceled,
    /// Compaction failed (summarizer error); nothing was written.
    Failed(String),
}

/// Plan, summarize, persist, and reseed in one shot. Assumes no turn is
/// in flight (the caller holds the agent lock; `agent` is already
/// borrowed mutably). Locks `log` only around the pure planning and the
/// final persist+reseed, never across the summarizer network call, so a
/// long summary doesn't block log writers. Cancellation is honored by
/// the `complete_oneshot` calls; an abort before the persist step leaves
/// the log untouched.
///
/// `handoff` carries the checkpoint's log identity to whoever tags the
/// `CompactionEnd` this emits (see [`AppendHandoff`]). A run that
/// persists nothing files nothing, so its `CompactionEnd` stays
/// untagged.
pub async fn run_compaction(
    agent: &mut Agent,
    log: &Arc<TokioMutex<ConversationLog>>,
    handoff: &AppendHandoff,
    reason: CompactionReason,
    custom_instructions: Option<&str>,
    keep_recent_tokens: u64,
    cancel: CancellationToken,
) -> CompactionOutcome {
    // Plan under the log lock, then drop it before any network call. We
    // plan before announcing the run so a no-op never shows the
    // "Compacting…" spinner or disturbs the footer. A missing head and a
    // `None` plan both collapse to "nothing to do".
    let plan = {
        let log_guard = log.lock().await;
        match log_guard.head().cloned() {
            Some(head) => {
                let conversation = log_guard.linearize(&head, ThreadFilter::USER);
                planning::prepare_compaction(&conversation, keep_recent_tokens)
            }
            None => None,
        }
    };
    let Some(plan) = plan else {
        return finish_nothing(agent, reason, keep_recent_tokens).await;
    };

    // Best-effort UI signal; a failed emit must not abort the run.
    if let Err(err) = agent
        .emit_event(AgentEvent::CompactionStart {
            agent_id: AgentId::Main,
            reason,
        })
        .await
    {
        tracing::warn!("failed to emit CompactionStart: {err}");
    }

    // Output budgets are clamped against the model's own cap; a model
    // that reports 0 (unknown) keeps the desired budget unclamped.
    let model_max_tokens = agent.model_info().max_tokens;
    let summary_max_tokens = clamp_output_budget(SUMMARY_OUTPUT_CAP, model_max_tokens);

    let conversation_text = planning::serialize_conversation(&plan.messages_to_summarize);
    let prompt = match plan.previous_summary.as_deref() {
        Some(previous) => {
            planning::update_summary_prompt(&conversation_text, previous, custom_instructions)
        }
        None => planning::initial_summary_prompt(&conversation_text, custom_instructions),
    };

    // The summarizer's spend is out-of-band: its exchange never becomes
    // a message entry, so this accumulator is the only record of it, and
    // it rides along on the compaction entry written below.
    let mut summarizer_usage = Usage::default();

    emit_progress(agent, reason, CompactionPhase::Summarizing).await;
    let mut summary = match agent
        .complete_oneshot(
            planning::SUMMARIZATION_SYSTEM_PROMPT,
            prompt,
            summary_max_tokens,
            cancel.clone(),
        )
        .await
    {
        Ok(message) => {
            summarizer_usage.accumulate(&message.usage);
            assistant_text(&message)
        }
        Err(TurnError::Aborted) => return finish_canceled(agent, reason, plan.tokens_before).await,
        Err(err) => return finish_failed(agent, reason, plan.tokens_before, err.to_string()).await,
    };

    // A cut that split a turn leaves the retained suffix without its
    // turn setup; summarize the prefix separately and append it.
    if !plan.turn_prefix_messages.is_empty() {
        let prefix_text = planning::serialize_conversation(&plan.turn_prefix_messages);
        let prefix_prompt = planning::turn_prefix_summary_prompt(&prefix_text);
        let prefix_max_tokens = clamp_output_budget(SUMMARY_OUTPUT_CAP / 2, model_max_tokens);
        emit_progress(agent, reason, CompactionPhase::SummarizingTurnPrefix).await;
        let prefix = match agent
            .complete_oneshot(
                planning::SUMMARIZATION_SYSTEM_PROMPT,
                prefix_prompt,
                prefix_max_tokens,
                cancel.clone(),
            )
            .await
        {
            Ok(message) => {
                summarizer_usage.accumulate(&message.usage);
                assistant_text(&message)
            }
            Err(TurnError::Aborted) => {
                return finish_canceled(agent, reason, plan.tokens_before).await;
            }
            Err(err) => {
                return finish_failed(agent, reason, plan.tokens_before, err.to_string()).await;
            }
        };
        summary = format!("{summary}\n\n---\n\n**Turn context (split turn):**\n\n{prefix}");
    }

    // Surface the touched-files lists in the prose so the model sees
    // them without parsing the structured sections.
    summary.push_str(&format_file_ops(&plan.file_ops));

    // Persist the checkpoint, reseed the live transcript from the
    // post-compaction projection, and emit the `CompactionEnd` for the
    // checkpoint, all under one log guard. `log` and `agent` are distinct
    // locks, so holding the log guard while reseeding the agent is safe.
    emit_progress(agent, reason, CompactionPhase::Saving).await;
    let tokens_after = {
        let mut log_guard = log.lock().await;
        let checkpoint = match log_guard.append_compaction(
            ThreadFilter::USER,
            summary.clone(),
            plan.first_kept_entry_id.clone(),
            plan.tokens_before,
            Some(plan.file_ops.clone()),
            Some(summarizer_usage),
        ) {
            Ok(entry) => entry,
            Err(err) => {
                drop(log_guard);
                return finish_failed(agent, reason, plan.tokens_before, err.to_string()).await;
            }
        };
        let head = log_guard.head().cloned().expect("head exists after append");
        let conversation = log_guard.linearize(&head, ThreadFilter::USER);
        let mut messages = conversation.agent_messages();
        // Drop a trailing failed-assistant message. The log records the
        // failed turn (it was emitted to the bus), but the wire layer
        // never shows Error/Aborted assistants to the model, and the
        // reactive overflow path needs the reseeded transcript to end in
        // a user/tool-result message so `continue_run` can re-drive
        // inference against the reduced context.
        trim_trailing_failed_assistant(&mut messages);
        // The just-appended compaction sits at the head, so the
        // retained tail's assistant `usage` is stale; the
        // compaction-aware estimate uses the projection's heuristic
        // size instead, which is what the next turn will actually send.
        let after = planning::estimate_conversation_context(&conversation).tokens;
        agent.reseed_transcript(messages);

        handoff.file(checkpoint);
        // The emit happens under the guard on purpose: a durable append
        // landing between the checkpoint and its event (a background
        // sub-agent's `MessageEnd`) would make the forwarded seqs
        // non-monotone, which spec section 5 forbids. A listener must
        // therefore not take the log lock for `CompactionEnd`.
        if let Err(err) = agent
            .emit_event(AgentEvent::CompactionEnd {
                agent_id: AgentId::Main,
                reason,
                tokens_before: plan.tokens_before,
                tokens_after: after,
                summary: Some(summary),
                error: None,
            })
            .await
        {
            tracing::warn!("failed to emit CompactionEnd: {err}");
        }
        after
    };

    CompactionOutcome::Compacted {
        tokens_before: plan.tokens_before,
        tokens_after,
    }
}

/// Drop trailing failed-assistant messages (an overflow's error turn
/// or an aborted turn) so the reseeded transcript ends in a
/// user/tool-result message — the precondition `continue_run` enforces
/// for reactive recovery. We only trim a failed assistant that carries
/// no tool calls, so we never orphan a tool result that references it;
/// a partially-executed tool turn ends in its tool-result messages
/// rather than the assistant and so is left untouched.
fn trim_trailing_failed_assistant(messages: &mut Vec<AgentMessage>) {
    while let Some(last) = messages.last() {
        let trim = matches!(
            last.as_stored_wire(),
            Some(Message::Assistant(a))
                if matches!(a.stop_reason, StopReason::Error | StopReason::Aborted)
                    && !a
                        .content
                        .iter()
                        .any(|c| matches!(c, AssistantContent::ToolCall(_)))
        );
        if trim {
            messages.pop();
        } else {
            break;
        }
    }
}

/// Emit a best-effort [`AgentEvent::CompactionProgress`]; a failed emit
/// only loses a UI label, so it must not abort the run.
async fn emit_progress(agent: &Agent, reason: CompactionReason, phase: CompactionPhase) {
    if let Err(err) = agent
        .emit_event(AgentEvent::CompactionProgress {
            agent_id: AgentId::Main,
            reason,
            phase,
        })
        .await
    {
        tracing::warn!("failed to emit CompactionProgress: {err}");
    }
}

/// Clamp a desired output budget against the model's `max_tokens`. A
/// model that reports 0 (unknown) keeps `desired` unclamped, since
/// clamping to 0 would starve the summarizer.
fn clamp_output_budget(desired: u64, model_max_tokens: u64) -> u64 {
    if model_max_tokens == 0 {
        desired
    } else {
        desired.min(model_max_tokens)
    }
}

/// Render the touched-files block appended to the summary. Empty when
/// no files were touched; otherwise a `## Files Touched` section with
/// only the non-empty lists.
fn format_file_ops(file_ops: &planning::CompactionDetails) -> String {
    if file_ops.read_files.is_empty() && file_ops.modified_files.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n## Files Touched\n");
    if !file_ops.read_files.is_empty() {
        out.push_str(&format!("Read: {}\n", file_ops.read_files.join(", ")));
    }
    if !file_ops.modified_files.is_empty() {
        out.push_str(&format!(
            "Modified: {}\n",
            file_ops.modified_files.join(", ")
        ));
    }
    out
}

/// Report that there was nothing to compact, leaving the footer
/// untouched (nothing changed). A user-initiated `/compact` gets a
/// one-line notice explaining why; automatic triggers stay silent so a
/// threshold that keeps firing without summarizable history can't spam
/// the transcript.
async fn finish_nothing(
    agent: &Agent,
    reason: CompactionReason,
    keep_recent_tokens: u64,
) -> CompactionOutcome {
    if matches!(reason, CompactionReason::Manual) {
        let text = format!(
            "Nothing to compact: the conversation already fits within the \
             recent-context budget (~{keep_recent_tokens} tokens)."
        );
        if let Err(err) = agent
            .emit_event(AgentEvent::Notice {
                agent_id: AgentId::Main,
                text,
            })
            .await
        {
            tracing::warn!("failed to emit nothing-to-compact notice: {err}");
        }
    }
    CompactionOutcome::NothingToDo
}

/// Emit a failing `CompactionEnd` (nothing was persisted) and report
/// the failure.
async fn finish_failed(
    agent: &Agent,
    reason: CompactionReason,
    tokens_before: u64,
    error: String,
) -> CompactionOutcome {
    if let Err(err) = agent
        .emit_event(AgentEvent::CompactionEnd {
            agent_id: AgentId::Main,
            reason,
            tokens_before,
            tokens_after: 0,
            summary: None,
            error: Some(error.clone()),
        })
        .await
    {
        tracing::warn!("failed to emit CompactionEnd: {err}");
    }
    CompactionOutcome::Failed(error)
}

/// Emit a terminal `CompactionEnd` for a cancelled run. `summary` and
/// `error` are both `None` — the event's documented "ended without
/// writing" shape — so a renderer stops the in-progress indicator and
/// shows a neutral notice rather than reading it as success or failure.
async fn finish_canceled(
    agent: &Agent,
    reason: CompactionReason,
    tokens_before: u64,
) -> CompactionOutcome {
    if let Err(err) = agent
        .emit_event(AgentEvent::CompactionEnd {
            agent_id: AgentId::Main,
            reason,
            tokens_before,
            tokens_after: 0,
            summary: None,
            error: None,
        })
        .await
    {
        tracing::warn!("failed to emit CompactionEnd (canceled): {err}");
    }
    CompactionOutcome::Canceled
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_models::types::{AssistantMessage, ToolCall, UserMessage};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::User(UserMessage::text(text)))
    }

    fn assistant(stop: StopReason, content: Vec<AssistantContent>) -> AgentMessage {
        let mut a = AssistantMessage::empty();
        a.stop_reason = stop;
        a.content = content;
        AgentMessage::wire(Message::Assistant(a))
    }

    fn tool_call() -> AssistantContent {
        AssistantContent::ToolCall(ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: serde_json::Value::Null,
        })
    }

    #[test]
    fn trims_trailing_error_assistant() {
        let mut msgs = vec![user("hi"), assistant(StopReason::Error, vec![])];
        trim_trailing_failed_assistant(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].as_stored_wire(), Some(Message::User(_))));
    }

    #[test]
    fn trims_consecutive_failed_assistants() {
        let mut msgs = vec![
            user("hi"),
            assistant(StopReason::Aborted, vec![]),
            assistant(StopReason::Error, vec![]),
        ];
        trim_trailing_failed_assistant(&mut msgs);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn keeps_failed_assistant_with_tool_calls() {
        // A partially-executed turn ends in its tool-result messages, not
        // the assistant; trimming the assistant would orphan results.
        let mut msgs = vec![
            user("hi"),
            assistant(StopReason::Aborted, vec![tool_call()]),
        ];
        trim_trailing_failed_assistant(&mut msgs);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn keeps_successful_trailing_assistant() {
        let mut msgs = vec![user("hi"), assistant(StopReason::Stop, vec![])];
        trim_trailing_failed_assistant(&mut msgs);
        assert_eq!(msgs.len(), 2);
    }

    /// What the sink observed across one compaction, plus whether a
    /// concurrent durable append managed to land inside the checkpoint's
    /// window.
    struct RaceOutcome {
        /// Whether the racing append found the log lock free while
        /// `CompactionEnd` was being delivered.
        interleaved: bool,
        /// The durable positions the sink saw, in delivery order.
        durable: Vec<u64>,
        /// The position `CompactionEnd` was tagged with.
        compaction_end: Option<u64>,
    }

    /// Drive two turns and then a compaction, with the tagging forwarder
    /// installed and a listener that models a concurrent durable append
    /// landing while `CompactionEnd` is delivered.
    ///
    /// The racer uses `try_lock` deliberately: a real concurrent appender
    /// (a background sub-agent's `MessageEnd` through the same forwarder)
    /// gets its append in exactly when the log lock is free at that
    /// moment, and blocks until after the event otherwise.
    async fn compaction_with_a_racing_append() -> RaceOutcome {
        use std::sync::Mutex as StdMutex;

        use aj_agent::events::AgentEvent;
        use aj_session::{
            AppendHandoff, ConversationEntryKind, ConversationPersistence, TaggedEvent, ThreadKind,
            persisting_forwarder,
        };
        use tempfile::TempDir;
        use tokio_util::sync::CancellationToken;

        use crate::test_support::{build_test_agent, finalized_text_message, scripted_run_config};

        let dir = TempDir::new().expect("tempdir");
        let store = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![
            finalized_text_message("first answer"),
            finalized_text_message("second answer"),
            finalized_text_message("SUMMARY"),
        ]);
        let (mut agent, log, persistence) = build_test_agent(&store, &run_config);
        // The forwarder replaces the plain persistence listener: it is the
        // one that tags, and two persisting listeners would append every
        // message twice.
        drop(persistence);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let interleaved = Arc::new(StdMutex::new(false));

        // Subscribed before the forwarder, so a durable append it manages
        // to make reaches the sink before `CompactionEnd` does.
        let racer_log = Arc::clone(&log);
        let racer_sink = tx.clone();
        let racer_flag = Arc::clone(&interleaved);
        let _racer = agent.subscribe(Arc::new(move |event: &AgentEvent| {
            let log = Arc::clone(&racer_log);
            let sink = racer_sink.clone();
            let flag = Arc::clone(&racer_flag);
            let is_compaction_end = matches!(event, AgentEvent::CompactionEnd { .. });
            Box::pin(async move {
                if !is_compaction_end {
                    return Ok(());
                }
                let Ok(mut guard) = log.try_lock() else {
                    return Ok(());
                };
                *flag.lock().expect("flag mutex poisoned") = true;
                let message = user("interleaved");
                let parent = guard.head().cloned().expect("the log has a head");
                let entry = guard
                    .append(
                        Some(parent),
                        ThreadKind::User,
                        None,
                        ConversationEntryKind::Message {
                            message: message.clone(),
                        },
                    )
                    .expect("append the interleaved message");
                let _ = sink.send(TaggedEvent {
                    entry: Some(entry),
                    event: AgentEvent::MessageEnd {
                        agent_id: AgentId::Main,
                        message,
                    },
                });
                Ok(())
            })
        }));

        let handoff = AppendHandoff::default();
        let _forwarder =
            agent.subscribe(persisting_forwarder(Arc::clone(&log), handoff.clone(), tx));

        agent
            .prompt("first question".to_string(), CancellationToken::new())
            .await
            .expect("first turn");
        // A long second prompt so the keep-recent cut leaves the first
        // turn to summarize.
        agent
            .prompt(
                format!("second question {}", "X".repeat(2000)),
                CancellationToken::new(),
            )
            .await
            .expect("second turn");

        let outcome = run_compaction(
            &mut agent,
            &log,
            &handoff,
            CompactionReason::Manual,
            None,
            100,
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(outcome, CompactionOutcome::Compacted { .. }),
            "expected a compaction, got {outcome:?}"
        );

        let mut durable = Vec::new();
        let mut compaction_end = None;
        while let Ok(tagged) = rx.try_recv() {
            let Some(entry) = tagged.entry else { continue };
            if matches!(tagged.event, AgentEvent::CompactionEnd { .. }) {
                compaction_end = Some(entry.seq);
            }
            durable.push(entry.seq);
        }
        RaceOutcome {
            interleaved: *interleaved.lock().expect("flag mutex poisoned"),
            durable,
            compaction_end,
        }
    }

    /// A compaction's checkpoint carries what the summarizer spent,
    /// summed over every call the compaction made.
    ///
    /// A cut that lands mid-turn costs a second call for the turn
    /// prefix, and both are the user's money. The scripted model is
    /// given real rates because the default scripted `ModelCost` is all
    /// zeros, against which a dropped dollar figure reads as correct.
    #[tokio::test]
    async fn the_checkpoint_records_what_every_summarizer_call_spent() {
        use aj_session::{ConversationEntryKind, ConversationPersistence};
        use tempfile::TempDir;
        use tokio_util::sync::CancellationToken;

        use crate::test_support::{build_test_agent, finalized_text_message, scripted_run_config};

        fn priced(text: &str, input: u64, output: u64) -> aj_models::types::AssistantMessage {
            let mut m = finalized_text_message(text);
            m.usage.input = input;
            m.usage.output = output;
            m
        }

        let dir = TempDir::new().expect("tempdir");
        let store = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![
            finalized_text_message("first answer"),
            // Long enough to blow the keep-recent budget on its own, so
            // the cut snaps to this assistant message and lands inside
            // the turn its user prompt started.
            finalized_text_message(&format!("second answer {}", "X".repeat(4000))),
            priced("SUMMARY", 40_000, 900),
            priced("PREFIX", 5_000, 100),
        ]);
        {
            let mut guard = run_config.lock().expect("run config mutex poisoned");
            guard.model_info = Arc::new(aj_models::registry::ModelInfo {
                cost: aj_models::registry::ModelCost {
                    input: 3.0,
                    output: 15.0,
                    cache_read: 0.3,
                    cache_write: 3.75,
                    tiers: Vec::new(),
                },
                ..crate::test_support::scripted_model_info()
            });
        }
        let (mut agent, log, _persistence) = build_test_agent(&store, &run_config);

        agent
            .prompt("first question".to_string(), CancellationToken::new())
            .await
            .expect("first turn");
        agent
            .prompt("second question".to_string(), CancellationToken::new())
            .await
            .expect("second turn");

        let handoff = AppendHandoff::default();
        let outcome = run_compaction(
            &mut agent,
            &log,
            &handoff,
            CompactionReason::Manual,
            None,
            100,
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(outcome, CompactionOutcome::Compacted { .. }),
            "expected a compaction, got {outcome:?}"
        );

        let guard = log.lock().await;
        let entry = guard
            .entries_in_order()
            .into_iter()
            .find_map(|e| match &e.entry {
                ConversationEntryKind::Compaction { usage, summary, .. } => {
                    Some((usage.clone(), summary.clone()))
                }
                _ => None,
            })
            .expect("a compaction checkpoint was written");
        let (usage, summary) = entry;
        let usage = usage.expect("the checkpoint records the summarizer's usage");

        assert!(
            summary.contains("PREFIX"),
            "the fixture must reach the split-turn path, which is the second \
             summarizer call, or this test only measures the first: {summary}"
        );
        assert_eq!(
            usage.total_tokens, 46_000,
            "40900 for the summary call plus 5100 for the turn prefix"
        );
        // (45000 * 3.0 + 1000 * 15.0) / 1e6
        let expected = 0.135 + 0.015;
        assert!(
            (usage.cost.total - expected).abs() < 1e-9,
            "the checkpoint totals both calls' dollars: got {} expected {expected}",
            usage.cost.total
        );
    }

    /// The checkpoint's append and the `CompactionEnd` that stands for it
    /// have to be one atomic step. Emitting after dropping the log guard
    /// lets another durable append take a higher position and reach the
    /// sink first.
    #[tokio::test]
    async fn compaction_end_is_emitted_under_the_log_guard() {
        let outcome = compaction_with_a_racing_append().await;
        assert!(
            !outcome.interleaved,
            "no durable append may land between the checkpoint and its event"
        );
    }

    /// Spec section 5: live durable frames reach a stream in strictly
    /// increasing seq order.
    #[tokio::test]
    async fn durable_seqs_reach_the_sink_in_increasing_order() {
        let outcome = compaction_with_a_racing_append().await;
        assert!(
            outcome.durable.windows(2).all(|pair| pair[0] < pair[1]),
            "durable seqs must strictly increase, got {:?}",
            outcome.durable
        );
        assert_eq!(
            outcome.compaction_end,
            outcome.durable.last().copied(),
            "the checkpoint is the log's newest entry when its event is sent"
        );
    }
}
