//! Host-driven context compaction: plan, summarize, persist, reseed.
//!
//! The pure planning lives in `aj_session::compaction`. This module is
//! the orchestration behind manual compaction and the automatic threshold
//! and overflow triggers. It locks the log to plan, runs a bus-silent
//! summarizer inference on the agent, records a compaction entry, and
//! reseeds the agent's transcript from the post-compaction projection.

use aj_agent::events::{AgentEvent, AgentId, CompactionPhase, CompactionReason};
use aj_agent::message::AgentMessage;
use aj_agent::{Agent, TurnError};
use aj_models::types::{AssistantContent, AssistantMessage, Message, StopReason, Usage};
use aj_session::compaction as planning;
use aj_session::{Journal, TaggedEvent, ThreadFilter};
use tokio_util::sync::CancellationToken;

/// Upper bound on summary output tokens, clamped against the model's
/// own `max_tokens`.
const SUMMARY_OUTPUT_CAP: u64 = 8192;

/// Accept only a completed, nonempty text summary before replacing history.
fn summary_text(message: &AssistantMessage) -> Result<String, &'static str> {
    if message.stop_reason == StopReason::Length {
        return Err("Compaction summary reached the output limit. History was not changed.");
    }
    if message.stop_reason != StopReason::Stop
        || message
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_)))
    {
        return Err(
            "Compaction did not produce a completed text summary. History was not changed.",
        );
    }
    let mut out = String::new();
    for block in &message.content {
        if let AssistantContent::Text(t) = block {
            out.push_str(&t.text);
        }
    }
    if out.trim().is_empty() {
        return Err("Compaction produced an empty summary. History was not changed.");
    }
    Ok(out)
}

/// Outcome of a compaction run. Progress is also emitted on the event bus.
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
    /// Compaction failed. History was not replaced, and the error was emitted
    /// for persistence and display.
    Failed(String),
}

/// A failed compaction is not a failed inference of a turn. The turn
/// driver must not classify its retained assistant message for overflow retry.
#[derive(Debug, thiserror::Error)]
#[error("compaction failed: {0}")]
pub(crate) struct CompactionFailure(String);

impl CompactionOutcome {
    /// Stop the driven turn on failure or cancellation. A no-op permits
    /// continuation, but callers must not retry compaction without new work.
    pub(crate) fn into_turn_result(self) -> Result<(), TurnError> {
        match self {
            Self::Compacted { .. } | Self::NothingToDo => Ok(()),
            Self::Canceled => Err(TurnError::Aborted),
            Self::Failed(error) => Err(TurnError::Recoverable(Box::new(CompactionFailure(error)))),
        }
    }
}

/// Plan, summarize, persist, and reseed in one shot. The agent must be
/// idle or paused in its before-continuation hook, with all foreground
/// message events persisted. Locks the log only around the pure planning and
/// the final persist+reseed, never across the summarizer network call, so a
/// long summary doesn't block log writers. Cancellation is honored by
/// the `complete_oneshot` calls; an abort before the persist step leaves
/// the log untouched. Failures append a display-only error, not a checkpoint.
///
/// The checkpoint and the successful `CompactionEnd` that stands for it are
/// one write to `journal`, the session's. The bus gets that event after the
/// write. Failure events are recorded by the journal as the bus commits
/// them. Cancellation records nothing.
pub async fn run_compaction(
    agent: &mut Agent,
    journal: &Journal,
    reason: CompactionReason,
    custom_instructions: Option<&str>,
    keep_recent_tokens: u64,
    cancel: CancellationToken,
) -> CompactionOutcome {
    // Plan under the log lock, then drop it before any network call. We
    // plan before announcing the compaction so a no-op never shows the
    // "Compacting…" spinner or disturbs the footer. A missing head and a
    // `None` plan both collapse to "nothing to do".
    let plan = {
        let mut log_guard = journal.lock().await;
        match log_guard.head().cloned() {
            // The plan chooses the user messages to keep among all of the
            // branch's, which a reopened log reads only now.
            Some(head) => log_guard.log_mut().hold_user_messages().map(|()| {
                let branch = log_guard.linearize(&head, ThreadFilter::MAIN);
                planning::prepare_compaction(&branch, keep_recent_tokens)
            }),
            None => Ok(None),
        }
    };
    let plan = match plan {
        Ok(Some(plan)) => plan,
        Ok(None) => return finish_nothing(agent, reason, keep_recent_tokens).await,
        Err(error) => return finish_failed(agent, reason, 0, error.to_string()).await,
    };

    match agent.admit_goal_inference(cancel.clone()).await {
        Ok(()) => {}
        Err(TurnError::Aborted) => return finish_canceled(agent, reason, plan.tokens_before).await,
        Err(err) => return finish_failed(agent, reason, plan.tokens_before, err.to_string()).await,
    }

    // Best-effort UI signal; a failed emit must not abort the compaction.
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

    let messages_text = planning::serialize_messages(&plan.messages_to_summarize);
    let prompt = match plan.previous_summary.as_deref() {
        Some(previous) => {
            planning::update_summary_prompt(&messages_text, previous, custom_instructions)
        }
        None => planning::initial_summary_prompt(&messages_text, custom_instructions),
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
            match summary_text(&message) {
                Ok(text) => text,
                Err(error) => {
                    return finish_failed(agent, reason, plan.tokens_before, error.to_string())
                        .await;
                }
            }
        }
        Err(TurnError::Aborted) => return finish_canceled(agent, reason, plan.tokens_before).await,
        Err(err) => return finish_failed(agent, reason, plan.tokens_before, err.to_string()).await,
    };

    // A cut that split a turn leaves the retained suffix without its
    // turn setup; summarize the prefix separately and append it.
    if !plan.turn_prefix_messages.is_empty() {
        let prefix_text = planning::serialize_messages(&plan.turn_prefix_messages);
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
                match summary_text(&message) {
                    Ok(text) => text,
                    Err(error) => {
                        return finish_failed(agent, reason, plan.tokens_before, error.to_string())
                            .await;
                    }
                }
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
    // post-compaction projection, and account the summarizer's spend, all in
    // one write. The write also sends the `CompactionEnd` to the session's
    // stream, so no later append can be published before the checkpoint.
    emit_progress(agent, reason, CompactionPhase::Saving).await;
    let tokens_before = plan.tokens_before;
    let committed = journal
        .record(|log| {
            // Cancellation before the commit boundary leaves history untouched.
            // Once appended, the checkpoint is published and the agent reseeded
            // in this same step.
            if cancel.is_cancelled() {
                return Ok((None, Vec::new()));
            }
            let checkpoint = log.append_compaction(
                ThreadFilter::MAIN,
                summary.clone(),
                plan.first_kept_entry_id.clone(),
                plan.retained_user_entry_ids.clone(),
                tokens_before,
                Some(plan.file_ops.clone()),
                summarizer_usage.clone(),
            )?;
            let head = log.head().cloned().expect("head exists after append");
            let branch = log.linearize(&head, ThreadFilter::MAIN);
            let mut messages = branch.agent_messages();
            // Drop a trailing failed-assistant message. The log records the
            // failed turn (it was emitted to the bus), but the wire layer
            // never shows Error/Aborted assistants to the model, and the
            // reactive overflow path needs the reseeded transcript to end in
            // a user/tool-result message so `continue_turn` can re-drive
            // inference against the reduced context.
            trim_trailing_failed_assistant(&mut messages);
            // The just-appended compaction sits at the head, so the
            // retained tail's assistant `usage` is stale; the
            // compaction-aware estimate uses the projection's heuristic
            // size instead, which is what the next turn will actually send.
            let after = planning::estimate_branch_context(&branch).tokens;
            agent.reseed_transcript(messages);
            let summary = summary.clone();
            let end =
                agent.account_compaction(&summarizer_usage, reason, tokens_before, after, summary);
            let event = TaggedEvent {
                entry: Some(checkpoint),
                event: end.event().clone(),
                branch_settings: None,
            };
            Ok((Some((after, end)), vec![event]))
        })
        .await;
    let (tokens_after, end) = match committed {
        Ok(Some(committed)) => committed,
        Ok(None) => return finish_canceled(agent, reason, tokens_before).await,
        Err(err) => return finish_failed(agent, reason, tokens_before, err.to_string()).await,
    };
    // The event is committed and on the session's stream. This delivers it
    // to the bus, outside the log lock.
    if let Err(err) = agent.publish_compaction(end).await {
        tracing::warn!("failed to emit committed compaction: {err}");
    }

    CompactionOutcome::Compacted {
        tokens_before,
        tokens_after,
    }
}

/// Drop trailing failed-assistant messages (an overflow's error message
/// or an aborted turn) so the reseeded transcript ends in a
/// user/tool-result message — the precondition `continue_turn` enforces
/// for reactive recovery. We only trim a failed assistant that carries
/// no tool calls, so we never orphan a tool result that references it;
/// a partially-executed tool round ends in its tool-result messages
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
/// only loses a UI label, so it must not abort the compaction.
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
/// untouched (nothing changed). Manual compaction gets a
/// one-line notice explaining why. Automatic triggers stay silent so a
/// threshold that keeps firing without summarizable history can't spam
/// the transcript.
async fn finish_nothing(
    agent: &Agent,
    reason: CompactionReason,
    keep_recent_tokens: u64,
) -> CompactionOutcome {
    if matches!(reason, CompactionReason::Manual) {
        let text = format!(
            "Nothing to compact: the branch already fits within the \
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

/// Emit a failing `CompactionEnd` for persistence and display, without
/// replacing any history, and report the failure to the turn driver.
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
            usage: None,
            summary: None,
            error: Some(error.clone()),
        })
        .await
    {
        tracing::warn!("failed to emit CompactionEnd: {err}");
    }
    CompactionOutcome::Failed(error)
}

/// Emit a terminal `CompactionEnd` for a cancelled compaction. `summary` and
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
            usage: None,
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
    use std::sync::Arc;

    use super::*;
    use aj_models::types::{AssistantMessage, ToolCall, UserMessage};

    struct RecordingProvider {
        inner: Arc<dyn aj_models::provider::Provider>,
        requests: Arc<std::sync::Mutex<Vec<aj_models::types::Context>>>,
    }

    impl aj_models::provider::Provider for RecordingProvider {
        fn stream(
            &self,
            model: &aj_models::registry::ModelInfo,
            context: &aj_models::types::Context,
            options: &aj_models::types::StreamOptions,
        ) -> aj_models::streaming::AssistantMessageEventStream {
            self.requests.lock().unwrap().push(context.clone());
            self.inner.stream(model, context, options)
        }

        fn stream_simple(
            &self,
            model: &aj_models::registry::ModelInfo,
            context: &aj_models::types::Context,
            options: &aj_models::types::SimpleStreamOptions,
        ) -> aj_models::streaming::AssistantMessageEventStream {
            self.requests.lock().unwrap().push(context.clone());
            self.inner.stream_simple(model, context, options)
        }
    }

    #[tokio::test]
    async fn compaction_keeps_original_requests_across_repeated_compaction_and_session_resume() {
        use crate::session::{SessionCore, SessionEntry, SessionSpec};
        use crate::test_support::{build_test_agent, finalized_text_message, scripted_run_config};
        use aj_agent::bus::listener_from_sync;
        use aj_agent::queue::MessageQueues;
        use aj_models::types::UserContent;
        use aj_session::{LogEntryKind, ProjectSessions};

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("evidence.txt");
        std::fs::write(&path, "evidence").unwrap();
        let mut read = finalized_text_message("");
        read.stop_reason = StopReason::ToolUse;
        read.content = vec![AssistantContent::ToolCall(ToolCall {
            is_raw: false,
            id: "read".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": path}),
        })];
        // Large assistant work pushes requests out of the recent tail. Neither
        // generated checkpoint repeats the request text, so only retention can save it.
        let run_config = scripted_run_config(vec![
            finalized_text_message(&"old work ".repeat(3000)),
            read,
            finalized_text_message(&"more work ".repeat(3000)),
            finalized_text_message("recent reply"),
            finalized_text_message("CHECKPOINT_ONE"),
            finalized_text_message(&"new work ".repeat(3000)),
            finalized_text_message("recent reply"),
            finalized_text_message("CHECKPOINT_TWO"),
            finalized_text_message("resumed reply"),
        ]);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let mut config = run_config.lock().unwrap();
            config.main.provider = Arc::new(RecordingProvider {
                inner: Arc::clone(&config.main.provider),
                requests: Arc::clone(&requests),
            });
        }
        let store = ProjectSessions::new(dir.path().join("sessions"));
        let (mut agent, log, journal) = build_test_agent(&store, &run_config);
        let queues = MessageQueues::default();
        agent.set_message_queues(queues.clone());
        let _steering = agent.subscribe(listener_from_sync(move |event| {
            if matches!(event, AgentEvent::ToolExecutionEnd { .. }) {
                queues.append_steering(AgentId::Main, "STEERING_CONSTRAINT");
            }
        }));
        agent
            .prompt_with_content(
                vec![
                    UserContent::text("ORIGINAL_REQUEST"),
                    UserContent::image("original-pixels", "image/png"),
                ],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        agent
            .prompt("ORIGINAL_REQUEST".into(), CancellationToken::new())
            .await
            .unwrap();
        agent
            .prompt("continue".into(), CancellationToken::new())
            .await
            .unwrap();

        let mut prior_selected = Vec::new();
        for round in 0..2 {
            if round == 1 {
                agent
                    .prompt("continue".into(), CancellationToken::new())
                    .await
                    .unwrap();
                agent
                    .prompt("continue".into(), CancellationToken::new())
                    .await
                    .unwrap();
            }
            let outcome = run_compaction(
                &mut agent,
                &journal,
                CompactionReason::Manual,
                None,
                4000,
                CancellationToken::new(),
            )
            .await;
            assert!(
                matches!(outcome, CompactionOutcome::Compacted { .. }),
                "{outcome:?}"
            );
            let guard = log.lock().await;
            let branch = guard.linearize(guard.head().unwrap(), ThreadFilter::MAIN);
            let LogEntryKind::Compaction {
                retained_user_entry_ids,
                first_kept_entry_id,
                ..
            } = &branch.entries().last().unwrap().entry
            else {
                panic!("checkpoint missing")
            };
            assert!(
                retained_user_entry_ids.len() >= 3,
                "both original requests and steering were outside the tail"
            );
            for id in &prior_selected {
                assert!(
                    retained_user_entry_ids.contains(id),
                    "compaction alone must not age out a user request"
                );
            }
            prior_selected = retained_user_entry_ids.clone();
            let cut = branch
                .entries()
                .iter()
                .position(|entry| &entry.id == first_kept_entry_id)
                .unwrap();
            assert!(
                branch.entries()[cut..]
                    .iter()
                    .filter_map(|entry| match &entry.entry {
                        LogEntryKind::Message { message } =>
                            Some(serde_json::to_string(message).unwrap()),
                        _ => None,
                    })
                    .all(|text| !text.contains("ORIGINAL_REQUEST")
                        && !text.contains("STEERING_CONSTRAINT"))
            );
        }
        let live = serde_json::to_value(agent.messages()).unwrap();
        let session_id = log.lock().await.session_id().to_string();
        drop(journal);
        drop(agent);
        drop(log);

        // Resuming under a different budget must not recompute a checkpoint's selection.
        let config = aj_conf::Config {
            compact_keep_recent: 1,
            ..Default::default()
        };
        let snapshot = run_config.lock().unwrap().clone();
        let (core, _) = SessionCore::build(
            &config,
            snapshot,
            &store,
            &SessionSpec::Resume {
                session_id,
                entry: SessionEntry::Startup,
            },
            None,
        )
        .unwrap();
        let (mut resumed, _log, _persistence) = core.into_test_agent();
        assert_eq!(serde_json::to_value(resumed.messages()).unwrap(), live);
        resumed
            .prompt("next".into(), CancellationToken::new())
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        let next = &requests.last().unwrap().messages;
        let texts: Vec<_> = next
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => match user.content.first() {
                    Some(UserContent::Text(text)) => Some(text.text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            texts
                .iter()
                .filter(|&&text| text == "ORIGINAL_REQUEST")
                .count(),
            2
        );
        assert_eq!(
            texts
                .iter()
                .filter(|&&text| text == "STEERING_CONSTRAINT")
                .count(),
            1
        );
        assert_eq!(
            texts.iter().filter(|&&text| text == "continue").count(),
            3,
            "equal text is not a duplicate message"
        );
        assert_eq!(texts[0], planning::RETAINED_USER_CONTEXT);
        let summary_index = texts
            .iter()
            .position(|text| text.contains("CHECKPOINT_TWO"))
            .unwrap();
        assert!(texts[..summary_index].contains(&"STEERING_CONSTRAINT"));
        assert!(!texts.iter().any(|text| text.contains("CHECKPOINT_ONE")));
        let images: Vec<_> = next
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(&user.content),
                _ => None,
            })
            .flatten()
            .filter_map(|content| match content {
                UserContent::Image(image) => Some(image.data.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(images, ["original-pixels"]);
    }

    /// Exercise a first-turn cut through real tools, durable projection, resume,
    /// and the next provider request. The retained tool result must keep its call.
    #[tokio::test]
    async fn compaction_of_a_long_first_turn_preserves_the_tail_on_resume_and_continuation() {
        use crate::test_support::{build_test_agent, finalized_text_message, scripted_run_config};
        use aj_session::ProjectSessions;

        let dir = tempfile::TempDir::new().unwrap();
        let old_path = dir.path().join("old.txt");
        let recent_path = dir.path().join("recent.txt");
        std::fs::write(&old_path, format!("OLD_EVIDENCE {}", "x".repeat(8000))).unwrap();
        std::fs::write(&recent_path, "RECENT_EVIDENCE").unwrap();
        let read = |id: &str, path: &std::path::Path| {
            let mut message = finalized_text_message("");
            message.stop_reason = StopReason::ToolUse;
            message.content = vec![AssistantContent::ToolCall(ToolCall {
                is_raw: false,
                id: id.to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": path}),
            })];
            message
        };
        let run_config = scripted_run_config(vec![
            read("old", &old_path),
            read("recent", &recent_path),
            finalized_text_message("kept reply"),
            finalized_text_message("CHECKPOINT"),
            finalized_text_message("continued"),
        ]);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let mut config = run_config.lock().unwrap();
            config.main.provider = Arc::new(RecordingProvider {
                inner: Arc::clone(&config.main.provider),
                requests: Arc::clone(&requests),
            });
        }
        let store = ProjectSessions::new(dir.path().join("sessions"));
        let (mut agent, log, journal) = build_test_agent(&store, &run_config);
        agent
            .prompt("inspect both files".into(), CancellationToken::new())
            .await
            .unwrap();
        let before = serde_json::to_string(agent.messages()).unwrap();
        assert!(before.contains("OLD_EVIDENCE") && before.contains("RECENT_EVIDENCE"));

        let outcome = run_compaction(
            &mut agent,
            &journal,
            CompactionReason::Manual,
            Some("focus on the evidence"),
            100,
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(outcome, CompactionOutcome::Compacted { .. }),
            "{outcome:?}"
        );
        let compacted = serde_json::to_value(agent.messages()).unwrap();
        let reduced = compacted.to_string();
        assert!(reduced.contains("CHECKPOINT") && reduced.contains("RECENT_EVIDENCE"));
        assert!(!reduced.contains("OLD_EVIDENCE"));
        assert!(reduced.contains("kept reply"));
        assert!(reduced.len() < before.len());

        let session_id = log.lock().await.session_id().to_string();
        // Whoever drives a turn stores its end. A second log would
        // otherwise store it, and fence this one out.
        let turn = log.lock().await.turn_state();
        turn.set(AgentId::Main, aj_session::TurnState::Idle)
            .unwrap();
        let resumed = aj_session::SessionLog::resume(&store, &session_id).unwrap();
        let branch = resumed.linearize(resumed.head().unwrap(), ThreadFilter::MAIN);
        assert_eq!(
            serde_json::to_value(branch.agent_messages()).unwrap(),
            compacted
        );

        agent
            .prompt("continue".into(), CancellationToken::new())
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            5,
            "one summary call, not an empty history call plus a prefix call"
        );
        let summary_request = &requests[3];
        assert!(summary_request.tools.is_empty());
        let summary_input = serde_json::to_string(&summary_request.messages).unwrap();
        assert!(
            summary_input.contains("inspect both files") && summary_input.contains("OLD_EVIDENCE")
        );
        assert!(summary_input.contains("focus on the evidence"));
        assert!(!summary_input.contains("RECENT_EVIDENCE"));
        let next = &requests[4].messages;
        assert!(
            next.windows(2).any(|pair| matches!(pair,
                [Message::Assistant(a), Message::ToolResult(result)]
                    if result.tool_call_id == "recent" && a.content.iter().any(|block|
                        matches!(block, AssistantContent::ToolCall(call) if call.id == "recent"))
            )),
            "the retained result needs its original call on the next request"
        );
        assert!(
            !serde_json::to_string(next)
                .unwrap()
                .contains("OLD_EVIDENCE")
        );
    }

    #[tokio::test]
    async fn invalid_compaction_summaries_leave_live_and_durable_history_unchanged() {
        use crate::test_support::{build_test_agent, finalized_text_message, scripted_run_config};
        use aj_agent::bus::listener_from_sync;
        use aj_session::ProjectSessions;

        let mut length_limited = finalized_text_message("incomplete checkpoint");
        length_limited.stop_reason = StopReason::Length;
        let mut tool_use = finalized_text_message("not a checkpoint");
        tool_use.stop_reason = StopReason::ToolUse;
        tool_use.content.push(tool_call());
        let mut unexpected_tool_call = tool_use.clone();
        unexpected_tool_call.stop_reason = StopReason::Stop;
        let mut thinking_only = finalized_text_message("");
        thinking_only.content = vec![AssistantContent::Thinking(
            aj_models::types::ThinkingContent {
                thinking: "summary deliberation".into(),
                thinking_signature: None,
                redacted: false,
            },
        )];

        for invalid in [
            finalized_text_message(" \n\t"),
            thinking_only,
            length_limited,
            tool_use,
            unexpected_tool_call,
        ] {
            for fail_prefix in [false, true] {
                let dir = tempfile::TempDir::new().unwrap();
                let store = ProjectSessions::new(dir.path().to_path_buf());
                let mut script = vec![
                    finalized_text_message("first answer"),
                    finalized_text_message(&"recent work ".repeat(400)),
                ];
                if fail_prefix {
                    script.push(finalized_text_message("valid history summary"));
                }
                script.push(invalid.clone());
                if !fail_prefix {
                    script.push(finalized_text_message("valid prefix summary"));
                }
                let run_config = scripted_run_config(script);
                let (mut agent, log, journal) = build_test_agent(&store, &run_config);
                agent
                    .prompt("first question".into(), CancellationToken::new())
                    .await
                    .unwrap();
                agent
                    .prompt("second question".into(), CancellationToken::new())
                    .await
                    .unwrap();
                let live_before = serde_json::to_value(agent.messages()).unwrap();
                let path = {
                    let guard = log.lock().await;
                    guard.path().to_path_buf()
                };
                let durable_before = std::fs::read(&path).unwrap();
                let ends = Arc::new(std::sync::Mutex::new(Vec::new()));
                let recorded = Arc::clone(&ends);
                let _events = agent.subscribe(listener_from_sync(move |event| {
                    if let AgentEvent::CompactionEnd { .. } = event {
                        recorded.lock().unwrap().push(event.clone());
                    }
                }));

                let outcome = run_compaction(
                    &mut agent,
                    &journal,
                    CompactionReason::Manual,
                    None,
                    100,
                    CancellationToken::new(),
                )
                .await;
                assert!(
                    matches!(outcome, CompactionOutcome::Failed(_)),
                    "{outcome:?}"
                );
                assert_eq!(serde_json::to_value(agent.messages()).unwrap(), live_before);
                let guard = log.lock().await;
                let branch = guard.linearize(guard.head().unwrap(), ThreadFilter::MAIN);
                assert_eq!(
                    serde_json::to_value(branch.agent_messages()).unwrap(),
                    live_before
                );
                assert!(std::fs::read(&path).unwrap().starts_with(&durable_before));
                assert!(matches!(
                    ends.lock().unwrap().as_slice(),
                    [AgentEvent::CompactionEnd {
                        summary: None,
                        error: Some(_),
                        usage: None,
                        ..
                    }]
                ));
            }
        }
    }

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
            is_raw: false,
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

    /// The checkpoint's append and the `CompactionEnd` that stands for it
    /// are one step under the log lock. A racing append that queues for the
    /// lock right behind the checkpoint takes the next position, and must
    /// reach the stream after the checkpoint's event.
    ///
    /// The racer models a background sub-agent's commit. It is queued while
    /// the lock is held elsewhere, so it gets the lock the moment the
    /// compaction's write lets go of it.
    #[tokio::test]
    async fn a_racing_append_is_published_after_the_checkpoint_it_follows() {
        use aj_agent::events::AgentEvent;
        use aj_session::{Committed, LogEntryKind, ProjectSessions, ThreadFilter};
        use tempfile::TempDir;
        use tokio_util::sync::CancellationToken;

        use crate::test_support::{build_test_agent, finalized_text_message, scripted_run_config};

        let dir = TempDir::new().expect("tempdir");
        let store = ProjectSessions::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![
            finalized_text_message(&"first answer ".repeat(400)),
            finalized_text_message("second answer"),
            finalized_text_message("SUMMARY"),
        ]);
        let (mut agent, log, journal) = build_test_agent(&store, &run_config);
        let mut rx = journal.stream();

        // When the compaction is about to save, hold the log for a few polls and
        // queue the racer. The compaction's write queues first.
        let racer_log = Arc::clone(&log);
        let racer_journal = journal.clone();
        let racer = Arc::new(std::sync::Mutex::new(None));
        let racer_slot = Arc::clone(&racer);
        let _racer = agent.subscribe(Arc::new(move |event: &AgentEvent| {
            let saving = matches!(
                event,
                AgentEvent::CompactionProgress {
                    phase: CompactionPhase::Saving,
                    ..
                }
            );
            let log = Arc::clone(&racer_log);
            let journal = racer_journal.clone();
            let slot = Arc::clone(&racer_slot);
            Box::pin(async move {
                if !saving {
                    return Ok(());
                }
                let held = log.lock_owned().await;
                tokio::spawn(async move {
                    for _ in 0..5 {
                        tokio::task::yield_now().await;
                    }
                    drop(held);
                });
                let racing = tokio::spawn(async move {
                    journal
                        .lock()
                        .await
                        .record(|log| {
                            let entry = log.append_thinking_change(ThreadFilter::MAIN, "high")?;
                            Ok((entry.seq, Vec::new()))
                        })
                        .await
                        .expect("the racing append")
                });
                *slot.lock().expect("racer slot poisoned") = Some(racing);
                Ok(())
            })
        }));

        agent
            .prompt("first question".to_string(), CancellationToken::new())
            .await
            .expect("first turn");
        // The large first answer leaves one complete turn to summarize.
        agent
            .prompt("second question".to_string(), CancellationToken::new())
            .await
            .expect("second turn");

        let outcome = run_compaction(
            &mut agent,
            &journal,
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
        let racing = racer
            .lock()
            .expect("racer slot poisoned")
            .take()
            .expect("the compaction announced its save");
        let raced = racing.await.expect("racer task");

        let mut positions = Vec::new();
        let mut compaction_end = None;
        while let Ok(item) = rx.try_recv() {
            match item {
                Committed::Event(tagged) => {
                    let Some(entry) = tagged.entry else { continue };
                    if matches!(tagged.event, AgentEvent::CompactionEnd { .. }) {
                        compaction_end = Some(entry.clone());
                    }
                    positions.push(entry.seq);
                }
                Committed::Position(seq) => positions.push(seq),
            }
        }
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "positions must reach the stream in increasing order, got {positions:?}"
        );
        let compaction_end = compaction_end.expect("the compaction end is tagged");
        assert_eq!(
            raced,
            compaction_end.seq + 1,
            "the racer must have queued right behind the checkpoint"
        );
        assert_eq!(positions.last().copied(), Some(raced));
        let guard = log.lock().await;
        let checkpoint = guard
            .entries_in_order()
            .into_iter()
            .find(|entry| entry.id == compaction_end.id)
            .expect("the tagged entry is in the log");
        assert!(
            matches!(checkpoint.entry, LogEntryKind::Compaction { .. }),
            "the compaction end is tagged with its checkpoint"
        );
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
        use aj_session::{LogEntryKind, ProjectSessions};
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
        let store = ProjectSessions::new(dir.path().to_path_buf());
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
            guard.main.model_info = Arc::new(aj_models::registry::ModelInfo {
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
        let (mut agent, log, journal) = build_test_agent(&store, &run_config);

        agent
            .prompt("first question".to_string(), CancellationToken::new())
            .await
            .expect("first turn");
        agent
            .prompt("second question".to_string(), CancellationToken::new())
            .await
            .expect("second turn");

        let outcome = run_compaction(
            &mut agent,
            &journal,
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
                LogEntryKind::Compaction { usage, summary, .. } => {
                    Some((usage.clone(), summary.clone()))
                }
                _ => None,
            })
            .expect("a compaction checkpoint was written");
        let (usage, summary) = entry;

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

    /// Cancellation after the checkpoint commit cannot erase its spend or
    /// take back its event, which the stream got with the commit.
    #[tokio::test]
    async fn committed_accounting_and_publication_survive_listener_cancellation() {
        use aj_agent::events::AgentEvent;
        use aj_models::types::Usage;
        use aj_session::{Committed, LogEntryKind, ProjectSessions};
        use tempfile::TempDir;
        use tokio_util::sync::CancellationToken;

        use crate::test_support::{build_test_agent, finalized_text_message, scripted_run_config};

        let dir = TempDir::new().expect("tempdir");
        let store = ProjectSessions::new(dir.path().to_path_buf());
        let mut summary = finalized_text_message("SUMMARY");
        summary.usage = Usage {
            input: 7,
            output: 11,
            cache_write: 13,
            cache_read: 17,
            total_tokens: 48,
            ..Usage::default()
        };
        let run_config = scripted_run_config(vec![
            finalized_text_message(&"first answer ".repeat(400)),
            finalized_text_message("second answer"),
            summary,
        ]);
        let (mut agent, log, journal) = build_test_agent(&store, &run_config);

        let listener_entered = Arc::new(tokio::sync::Notify::new());
        let entered = Arc::clone(&listener_entered);
        let _gate = agent.subscribe(Arc::new(move |event| {
            let entered = Arc::clone(&entered);
            let hold = matches!(
                event,
                AgentEvent::CompactionEnd {
                    usage: Some(_),
                    summary: Some(_),
                    ..
                }
            );
            Box::pin(async move {
                if hold {
                    entered.notify_one();
                    std::future::pending().await
                } else {
                    Ok(())
                }
            })
        }));
        let mut rx = journal.stream();

        agent
            .prompt("first question".to_string(), CancellationToken::new())
            .await
            .expect("first turn");
        agent
            .prompt("second question".to_string(), CancellationToken::new())
            .await
            .expect("second turn");
        while rx.try_recv().is_ok() {}

        let mut compaction = Box::pin(run_compaction(
            &mut agent,
            &journal,
            CompactionReason::Manual,
            None,
            100,
            CancellationToken::new(),
        ));
        tokio::select! {
            () = listener_entered.notified() => {}
            outcome = &mut compaction => panic!("listener did not hold compaction: {outcome:?}"),
        }
        drop(compaction);

        let accumulated = agent.accumulated_usage();
        assert_eq!(accumulated.input, 7);
        assert_eq!(accumulated.output, 11);
        assert_eq!(accumulated.cache_write, 13);
        assert_eq!(accumulated.cache_read, 17);
        let log_guard = log.lock().await;
        let checkpoint_usage = log_guard
            .entries_in_order()
            .into_iter()
            .find_map(|entry| match &entry.entry {
                LogEntryKind::Compaction { usage, .. } => Some(usage.clone()),
                _ => None,
            })
            .expect("committed checkpoint usage");
        assert_eq!(checkpoint_usage.input, 7);
        assert_eq!(checkpoint_usage.output, 11);
        assert_eq!(checkpoint_usage.cache_write, 13);
        assert_eq!(checkpoint_usage.cache_read, 17);
        drop(log_guard);

        let mut published = Vec::new();
        while let Ok(item) = rx.try_recv() {
            if let Committed::Event(tagged) = item
                && matches!(tagged.event, AgentEvent::CompactionEnd { .. })
            {
                published.push(tagged.entry.is_some());
            }
        }
        assert_eq!(
            published,
            vec![true],
            "the checkpoint's event is published once, tagged, with the commit"
        );
    }
}
