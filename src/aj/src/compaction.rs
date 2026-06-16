//! Host-driven context compaction: plan, summarize, persist, reseed.
//!
//! The pure planning lives in `aj_session::compaction`; this module is
//! the orchestration that the manual `/compact` command, the `compact`
//! CLI subcommand, and the auto/overflow triggers all call. It locks
//! the log to plan, runs a bus-silent summarizer inference on the
//! agent, records a compaction entry, and reseeds the agent's
//! transcript from the post-compaction projection. See
//! `docs/compaction-spec.md` §7.1.

use std::sync::Arc;

use aj_agent::Agent;
use aj_agent::events::{AgentEvent, AgentId, CompactionReason};
use aj_agent::message::AgentMessage;
use aj_models::types::{AssistantContent, Message, StopReason};
use aj_session::compaction as planning;
use aj_session::{ConversationLog, ThreadFilter};
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

/// Upper bound on summary output tokens, clamped against the model's
/// own `max_tokens`.
const SUMMARY_OUTPUT_CAP: u64 = 8192;

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
    /// Compaction failed (summarizer error or cancellation); nothing
    /// was written.
    Failed(String),
}

/// Plan, summarize, persist, and reseed in one shot. Assumes no turn is
/// in flight (the caller holds the agent lock; `agent` is already
/// borrowed mutably). Locks `log` only around the pure planning and the
/// final persist+reseed, never across the summarizer network call, so a
/// long summary doesn't block log writers. Cancellation is honored by
/// the `complete_oneshot` calls; an abort before the persist step leaves
/// the log untouched.
pub async fn run_compaction(
    agent: &mut Agent,
    log: &Arc<TokioMutex<ConversationLog>>,
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
        match log_guard.latest_leaf(ThreadFilter::USER) {
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

    let mut summary = match agent
        .complete_oneshot(
            planning::SUMMARIZATION_SYSTEM_PROMPT,
            prompt,
            summary_max_tokens,
            cancel.clone(),
        )
        .await
    {
        Ok(text) => text,
        Err(err) => return finish_failed(agent, reason, plan.tokens_before, err.to_string()).await,
    };

    // A cut that split a turn leaves the retained suffix without its
    // turn setup; summarize the prefix separately and append it.
    if !plan.turn_prefix_messages.is_empty() {
        let prefix_text = planning::serialize_conversation(&plan.turn_prefix_messages);
        let prefix_prompt = planning::turn_prefix_summary_prompt(&prefix_text);
        let prefix_max_tokens = clamp_output_budget(SUMMARY_OUTPUT_CAP / 2, model_max_tokens);
        let prefix = match agent
            .complete_oneshot(
                planning::SUMMARIZATION_SYSTEM_PROMPT,
                prefix_prompt,
                prefix_max_tokens,
                cancel.clone(),
            )
            .await
        {
            Ok(text) => text,
            Err(err) => {
                return finish_failed(agent, reason, plan.tokens_before, err.to_string()).await;
            }
        };
        summary = format!("{summary}\n\n---\n\n**Turn context (split turn):**\n\n{prefix}");
    }

    // Surface the touched-files lists in the prose so the model sees
    // them without parsing the structured sections.
    summary.push_str(&format_file_ops(&plan.file_ops));

    // Persist the checkpoint and reseed the live transcript from the
    // post-compaction projection. `log` and `agent` are distinct locks,
    // so holding the log guard while reseeding the agent is safe.
    let tokens_after = {
        let mut log_guard = log.lock().await;
        if let Err(err) = log_guard.append_compaction(
            ThreadFilter::USER,
            summary.clone(),
            plan.first_kept_entry_id.clone(),
            plan.tokens_before,
            Some(plan.file_ops.clone()),
        ) {
            drop(log_guard);
            return finish_failed(agent, reason, plan.tokens_before, err.to_string()).await;
        }
        let head = log_guard
            .latest_leaf(ThreadFilter::USER)
            .expect("head exists after append");
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
        after
    };

    if let Err(err) = agent
        .emit_event(AgentEvent::CompactionEnd {
            agent_id: AgentId::Main,
            reason,
            tokens_before: plan.tokens_before,
            tokens_after,
            summary: Some(summary),
            error: None,
        })
        .await
    {
        tracing::warn!("failed to emit CompactionEnd: {err}");
    }

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
            last.as_wire(),
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
        assert!(matches!(msgs[0].as_wire(), Some(Message::User(_))));
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
}
