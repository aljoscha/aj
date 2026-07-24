//! The [`AgentEvent`] reducer: folds one event into the [`ChatState`].
//!
//! One arm per event variant. Routing is by the event's agent id into
//! that agent's transcript and render bookkeeping. The reducer takes
//! the event by value so payloads (the assistant `partial`, tool
//! `content`) move into the model instead of being cloned. Persistence
//! is a separate bus subscriber, so nothing downstream needs the event
//! intact.

use std::sync::Arc;
use std::time::Instant;

use aj_agent::events::{AgentEvent, AgentId, SubAgentConclusion};
use aj_agent::message::{AgentMessageKind, TaskNotification};
use aj_agent::tool::{TaskStatus, ToolDetails};
use aj_models::streaming::AssistantMessageEvent;
use aj_models::types::{
    AssistantContent, AssistantMessage, ErrorCategory, Message, StopReason, UserContent,
    UserMessage,
};
use serde_json::Value;

use crate::chat::model::{
    AssistantEntry, ChatState, CompactionEntry, EntryId, EntryKind, NoticeEntry, NoticeLevel,
    SubAgentEntry, SubAgentStatus, TaskInfo, TaskNotificationEntry, ToolEntry, ToolStatus,
    TurnUsageEntry, UserEntry, joined_user_text,
};
use crate::session::AgentLifecycle;

/// Whether the reduced event changed renderable state. The host turns
/// `Redraw(true)` into a redraw request. `Redraw(false)` means the
/// event was a pure no-op (or bookkeeping-only) for the view.
#[must_use]
pub struct Redraw(pub bool);

/// Fold `event` into the model, updating the shared lifecycle sets
/// alongside it.
pub fn reduce(state: &mut ChatState, lifecycle: &mut AgentLifecycle, event: AgentEvent) -> Redraw {
    match event {
        // ---- Lifecycle ----------------------------------------------------
        //
        // `lifecycle` is the literal set of in-flight agents:
        // `AgentStart` marks running, `AgentEnd` marks idle, no agent's
        // lifecycle touches another's entry. Spinners and counts derive
        // from it view-side.
        AgentEvent::AgentStart { agent_id } => {
            lifecycle.mark_running(agent_id);
            // A continuation re-prompt emits no `SubAgentStart`, so
            // `AgentStart(Sub n)` is what flips a re-prompted box back
            // to `Running`. Defensive: skip when the box doesn't exist
            // yet.
            if let AgentId::Sub(n) = agent_id
                && let Some(b) = state.sub_box_mut(n)
            {
                b.status = SubAgentStatus::Running;
                // A continuation re-run (the box already has an end recorded)
                // restarts the clock so the runtime times the new run, not the
                // wall-clock since first spawn. The initial run has no end yet,
                // so its `started_at` is left untouched.
                if b.finished_at.is_some() {
                    b.started_at = Instant::now();
                    b.finished_at = None;
                }
            }
            Redraw(true)
        }
        AgentEvent::AgentEnd { agent_id, .. } => {
            lifecycle.mark_idle(agent_id);
            // Each agent owns its streaming bookkeeping, so an agent's
            // end clears only its own entry. The main agent's pending
            // `agent` tool call (whose body is a sub-agent run) is
            // unaffected.
            if let Some(render) = state.render.get_mut(&agent_id) {
                render.current_assistant = None;
                render.tool_index.clear();
            }
            // On the live path the trailing `SubAgentEnd` carries the
            // real conclusion and sets the final
            // `Truncated`/`Failed`/`Done` status. `conclude_sub_box`
            // touches a still-running box only, so an `AgentEnd`
            // delivered after it can't clobber that conclusion.
            if let AgentId::Sub(n) = agent_id {
                state.conclude_sub_box(n);
            }
            Redraw(true)
        }
        AgentEvent::TurnStart { agent_id } => {
            // Each new turn opens a fresh assistant entry. NOTE: a turn
            // that aborts or errors emits no `TurnEnd`, so nothing here
            // may assume balanced brackets.
            if let Some(render) = state.render.get_mut(&agent_id) {
                render.current_assistant = None;
            }
            Redraw(false)
        }
        AgentEvent::TurnEnd { .. } => {
            // The model is built incrementally from the message / tool
            // / usage events, so the finalized snapshot carried here is
            // not needed. The arm stays explicit so the exhaustiveness
            // check flags new variants.
            Redraw(false)
        }

        // ---- Message lifecycle ----------------------------------------------
        AgentEvent::MessageStart { .. } => {
            // The authoritative payload lands on `MessageEnd`
            // (user/tool-result), or the assistant entry is
            // materialized lazily by the first painting
            // `MessageUpdate` / by `MessageEnd` on the replay path.
            Redraw(false)
        }
        AgentEvent::MessageUpdate {
            agent_id, event, ..
        } => reduce_message_update(state, agent_id, event),
        AgentEvent::MessageEnd { agent_id, message } => {
            // The user and task-notification arms consume the id, so
            // read it under a borrow here and skip the copy for the
            // other arms. We cannot hoist the read into the arm itself:
            // the by-value match below moves `message.kind`, and `id()`
            // borrows the whole message.
            let message_id = match &message.kind {
                AgentMessageKind::Wire(Message::User(_))
                | AgentMessageKind::TaskNotification(_) => message.id().to_string(),
                _ => String::new(),
            };
            match message.kind {
                AgentMessageKind::Wire(Message::User(user)) => {
                    reduce_user_end(state, agent_id, user, message_id)
                }
                AgentMessageKind::Wire(Message::Assistant(assistant)) => {
                    reduce_assistant_end(state, agent_id, assistant)
                }
                AgentMessageKind::Wire(Message::ToolResult(_)) => {
                    // Tool results render through the dedicated
                    // `ToolExecutionEnd` event (which carries the
                    // structured `ToolDetails`). The unified
                    // `MessageEnd { ToolResult }` is structural framing.
                    Redraw(false)
                }
                AgentMessageKind::TaskNotification(notification) => {
                    reduce_task_notification_end(state, agent_id, notification, message_id)
                }
            }
        }

        // ---- Tool execution -------------------------------------------------
        //
        // The parent's `agent` tool call is represented by the
        // sub-agent box, not a tool cell, so its events are skipped to
        // avoid duplicating the report.
        AgentEvent::ToolExecutionStart {
            agent_id,
            call_id,
            tool,
            args,
        } => {
            if tool == "agent" {
                return Redraw(false);
            }
            // A sub-agent's tool start is its latest live activity. Main's
            // own tool calls are not sub-agent activity, and gating on
            // `Sub(n)` excludes them (the parent's `agent` tool is already
            // skipped above).
            if let AgentId::Sub(n) = agent_id
                && let Some(b) = state.sub_box_mut(n)
            {
                b.latest_activity = Some(tool.clone());
            }
            append_tool_entry(state, agent_id, call_id, tool, args);
            Redraw(true)
        }
        AgentEvent::ToolExecutionUpdate {
            agent_id,
            call_id,
            tool,
            partial,
            content,
            ..
        } => {
            if tool == "agent" {
                return Redraw(false);
            }
            let Some(&id) = state
                .render
                .get(&agent_id)
                .and_then(|r| r.tool_index.get(&call_id))
            else {
                // No mapped cell: a stale update after the owner's
                // `AgentEnd` wiped the index. Drop it.
                return Redraw(false);
            };
            if let Some(entry) = state.tool_entry_mut(agent_id, id) {
                entry.details = Some(partial);
                entry.content = content;
            }
            Redraw(true)
        }
        AgentEvent::ToolExecutionEnd {
            agent_id,
            call_id,
            tool,
            result,
            content,
            is_error,
        } => {
            if tool == "agent" {
                return Redraw(false);
            }
            // If we never saw `ToolExecutionStart` (replay path), build
            // the cell now so the result is visible. Args aren't
            // available on the End event, so it renders with an empty
            // object. The build-on-miss branch must replicate the live
            // path's bookkeeping (record `tool_index`, clear
            // `current_assistant`) so a subsequent assistant text chunk
            // opens a fresh entry *after* the tool rather than reusing
            // a pre-tool one.
            let id = match state
                .render
                .get(&agent_id)
                .and_then(|r| r.tool_index.get(&call_id))
            {
                Some(&id) => id,
                None => append_tool_entry(
                    state,
                    agent_id,
                    call_id.clone(),
                    tool,
                    Value::Object(Default::default()),
                ),
            };
            // A bash result carrying a task id is a background launch.
            // Record the cell so a `TaskStart` arriving after the
            // owner's `AgentEnd` can still find it. A fast task can
            // already have reached `TaskEnd`, which froze the cell
            // around the final `TaskOutput` snapshot. The launch
            // result's empty snapshot must not clobber that.
            let mut frozen = false;
            if let ToolDetails::Bash {
                task_id: Some(task_id),
                ..
            } = &result
            {
                state.pending_task_cells.insert(call_id.clone(), id);
                frozen = state
                    .tasks
                    .get(task_id)
                    .is_some_and(|info| info.status.is_terminal());
            }
            if let Some(entry) = state.tool_entry_mut(agent_id, id) {
                entry.status = ToolStatus::Done { is_error };
                if !frozen {
                    entry.details = Some(result);
                    entry.content = content;
                }
            }
            Redraw(true)
        }

        // ---- Notices --------------------------------------------------------
        AgentEvent::Notice { agent_id, text } => {
            append_notice(state, agent_id, NoticeLevel::Info, text);
            Redraw(true)
        }
        AgentEvent::Warning { agent_id, text } => {
            append_notice(state, agent_id, NoticeLevel::Warning, text);
            Redraw(true)
        }
        AgentEvent::Error { agent_id, text } => {
            append_notice(state, agent_id, NoticeLevel::Error, text);
            Redraw(true)
        }
        AgentEvent::StreamRetry {
            agent_id,
            attempt,
            delay,
            ..
        } => {
            // The failed attempt's error already rendered in-band from
            // its `MessageEnd`, so this line carries only the retry
            // cadence.
            let text = format!(
                "Retrying inference (attempt {attempt}, in {}ms)…",
                delay.as_millis()
            );
            append_notice(state, agent_id, NoticeLevel::Warning, text);
            Redraw(true)
        }

        // ---- Per-turn token usage --------------------------------------------
        AgentEvent::UsageUpdate { agent_id, usage } => {
            // Every agent's usage folds into its own footer entry. The
            // rendered footer tracks the viewed agent, so views repaint
            // it only when `agent_id == active_view`.
            state.footers.record_turn_usage(agent_id, &usage);
            state
                .transcripts
                .entry(agent_id)
                .or_default()
                .append(EntryKind::TurnUsage(TurnUsageEntry { agent_id, usage }));
            Redraw(true)
        }

        // ---- Compaction lifecycle ----------------------------------------------
        //
        // Compaction is host-orchestrated and does not bracket itself
        // with `AgentStart`/`AgentEnd`, so busy-ness is tracked through
        // the separate `compacting` set. The phase label is model
        // state (`compaction_phase`) the view reads for its spinner.
        AgentEvent::CompactionStart { agent_id, .. } => {
            lifecycle.mark_compacting(agent_id);
            state.compaction_phase.remove(&agent_id);
            Redraw(true)
        }
        AgentEvent::CompactionProgress {
            agent_id, phase, ..
        } => {
            state.compaction_phase.insert(agent_id, phase);
            Redraw(true)
        }
        AgentEvent::CompactionEnd {
            agent_id,
            tokens_before,
            tokens_after,
            summary,
            error,
            ..
        } => {
            lifecycle.clear_compacting(agent_id);
            state.compaction_phase.remove(&agent_id);
            // The `summary`/`error` pair encodes the terminal outcome:
            // an error is a failure, a summary is a success, neither is
            // a cancellation that wrote nothing.
            if let Some(err) = error {
                append_notice(
                    state,
                    agent_id,
                    NoticeLevel::Warning,
                    format!("Compaction failed: {err}"),
                );
            } else if let Some(summary) = summary {
                state
                    .transcripts
                    .entry(agent_id)
                    .or_default()
                    .append(EntryKind::Compaction(CompactionEntry {
                        tokens_before,
                        tokens_after,
                        summary,
                    }));
                // No `UsageUpdate` follows a compaction, so refresh the
                // footer occupancy directly to the post-compaction
                // estimate.
                state.footers.set_context_tokens(agent_id, tokens_after);
            } else {
                append_notice(
                    state,
                    agent_id,
                    NoticeLevel::Info,
                    "Compaction canceled.".to_string(),
                );
            }
            Redraw(true)
        }

        // ---- Sub-agent boxes ------------------------------------------------
        AgentEvent::SubAgentStart {
            parent,
            child,
            task,
            background,
            settings,
        } => {
            if let AgentId::Sub(n) = child {
                // Ensure the child's transcript and the parent-side box
                // (initially `Running`). The footer count and the box's
                // re-run status come from the paired `AgentStart(Sub
                // n)`, not from here.
                state.transcripts.entry(child).or_default();
                if !state.sub_boxes.contains_key(&n) {
                    let id =
                        state
                            .transcripts
                            .entry(parent)
                            .or_default()
                            .append(EntryKind::SubAgent(SubAgentEntry {
                                child: n,
                                task,
                                status: SubAgentStatus::Running,
                                report: None,
                                started_at: Instant::now(),
                                finished_at: None,
                                background,
                                latest_activity: None,
                            }));
                    state.sub_boxes.insert(n, (parent, id));
                }
                // Seed the child's footer entry with its spawn-time
                // settings so its view shows a model line and (when
                // resolvable) a context window.
                let window = state.resolve_window(&settings);
                state.footers.note_settings(child, settings, window);
            }
            Redraw(true)
        }
        AgentEvent::SubAgentEnd {
            child,
            report,
            conclusion,
            ..
        } => {
            if let AgentId::Sub(n) = child
                && let Some(b) = state.sub_box_mut(n)
            {
                b.status = match conclusion {
                    SubAgentConclusion::Completed => SubAgentStatus::Done,
                    SubAgentConclusion::Truncated => SubAgentStatus::Truncated,
                    SubAgentConclusion::Failed => SubAgentStatus::Failed,
                };
                b.report = Some(report);
                b.finished_at = Some(Instant::now());
            }
            Redraw(true)
        }

        // ---- Background tasks -------------------------------------------------
        //
        // Transient events: persistence ignores them and replay never
        // synthesizes them, so everything here is live-session-only
        // state. A resumed transcript shows the persisted launch cell
        // (with its task-id badge from the `ToolDetails::Bash` payload)
        // while this map starts empty, so nothing below may assume an
        // entry exists for a cell that carries a badge.
        AgentEvent::TaskStart {
            agent_id,
            task_id,
            call_id,
            kind,
            label,
        } => {
            // Resolve the launch cell: from the live `tool_index` when
            // the owner's turn is still running, else from the linkage
            // recorded at `ToolExecutionEnd`. `TaskStart` is unordered
            // relative to that result and may even trail the owner's
            // `AgentEnd`, which wipes `tool_index`. Both lookups key
            // on the launching `call_id`, so an agent-kind task (whose
            // `agent` tool has no cell, it renders as a sub-agent box)
            // misses both and keeps `cell = None`.
            let pending = state.pending_task_cells.remove(&call_id);
            let cell = state
                .render
                .get(&agent_id)
                .and_then(|r| r.tool_index.get(&call_id))
                .copied()
                .or(pending);
            if let Some(cell_id) = cell
                && let Some(entry) = state.tool_entry_mut(agent_id, cell_id)
            {
                entry.task = Some(task_id);
            }
            state.tasks.insert(
                task_id,
                TaskInfo {
                    kind,
                    label,
                    owner: agent_id,
                    call_id,
                    status: TaskStatus::Running,
                    started_at: Instant::now(),
                    finished_at: None,
                    cell,
                },
            );
            Redraw(true)
        }
        AgentEvent::TaskOutput {
            task_id, partial, ..
        } => {
            // Frozen after `TaskEnd`: a straggling snapshot must not
            // reopen the cell past its terminal badge.
            let frozen = state
                .tasks
                .get(&task_id)
                .is_none_or(|info| info.status.is_terminal());
            if frozen {
                return Redraw(false);
            }
            let Some((owner, cell)) = state.task_cell(task_id) else {
                return Redraw(false);
            };
            if let Some(entry) = state.tool_entry_mut(owner, cell) {
                // `TaskOutput` carries no wire content, only the
                // cumulative `ToolDetails` snapshot.
                entry.details = Some(partial);
            }
            Redraw(true)
        }
        AgentEvent::TaskEnd {
            task_id, status, ..
        } => {
            // A `TaskEnd` for an untracked task (impossible live,
            // conceivable on weird replays) is inert. The cell's badge
            // status is read from this entry, so freezing it here is
            // what stops further `TaskOutput` from landing.
            if let Some(info) = state.tasks.get_mut(&task_id) {
                info.status = status;
                info.finished_at = Some(Instant::now());
            }
            Redraw(true)
        }

        // ---- Queue snapshots ---------------------------------------------------
        AgentEvent::QueueUpdate { .. } => {
            // Redraw ping only: the view re-reads the live
            // `MessageQueues` snapshot rather than trusting the
            // payload, which keeps the pending box correct even if a UI
            // enqueue raced the drain.
            Redraw(true)
        }
    }
}

/// Handle an assistant streaming update.
fn reduce_message_update(
    state: &mut ChatState,
    agent_id: AgentId,
    event: AssistantMessageEvent,
) -> Redraw {
    // Early-out for events that don't paint into the assistant entry,
    // BEFORE materializing it. Tool calls render through the dedicated
    // `ToolExecution*` events. `Start` / `Done` / `Error` are
    // lifecycle markers whose bookends are the matching `MessageStart`
    // / `MessageEnd`. Returning here keeps tool-use-only turns from
    // materializing an empty assistant entry.
    let partial = match event {
        AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. } => partial,
        AssistantMessageEvent::ToolCallStart { .. }
        | AssistantMessageEvent::ToolCallDelta { .. }
        | AssistantMessageEvent::ToolCallEnd { .. }
        | AssistantMessageEvent::Start { .. }
        | AssistantMessageEvent::Done { .. }
        | AssistantMessageEvent::Error { .. } => return Redraw(false),
    };

    // `partial` is the cumulative message-so-far, so we replace the
    // stored snapshot wholesale. This is simpler than a per-block
    // open/append/close state machine and self-heals a dropped delta.
    // In particular a `ThinkingEnd` whose `content` field is empty
    // (the shape the agent emits when the provider finalizes a
    // thinking block without an authoritative snapshot) cannot wipe
    // anything: only the cumulative `partial` is consulted.
    match state
        .render
        .get(&agent_id)
        .and_then(|r| r.current_assistant)
    {
        Some(id) => {
            if let Some(EntryKind::Assistant(entry)) = state
                .transcripts
                .get_mut(&agent_id)
                .and_then(|t| t.get_mut(id))
                .map(|e| &mut e.kind)
            {
                entry.message = partial;
            }
        }
        None => {
            let id = state
                .transcripts
                .entry(agent_id)
                .or_default()
                .append(EntryKind::Assistant(AssistantEntry {
                    message: partial,
                    finalized: false,
                }));
            state.render.entry(agent_id).or_default().current_assistant = Some(id);
        }
    }
    Redraw(true)
}

/// Handle `MessageEnd { User }`: append the authoritative payload.
/// The rendering path for both live user prompts and replayed user
/// threads.
fn reduce_user_end(
    state: &mut ChatState,
    agent_id: AgentId,
    user: UserMessage,
    message_id: String,
) -> Redraw {
    let text = joined_user_text(&user.content);
    if text.is_empty() {
        return Redraw(false);
    }
    state
        .transcripts
        .entry(agent_id)
        .or_default()
        .append(EntryKind::User(UserEntry {
            message_id,
            content: user.content,
        }));
    Redraw(true)
}

/// Handle `MessageEnd { TaskNotification }`: append the typed
/// task-completion notice entry, which the view renders as an
/// outcome-tinted bubble rather than a user prompt.
fn reduce_task_notification_end(
    state: &mut ChatState,
    agent_id: AgentId,
    notification: TaskNotification,
    message_id: String,
) -> Redraw {
    state
        .transcripts
        .entry(agent_id)
        .or_default()
        .append(EntryKind::TaskNotification(TaskNotificationEntry {
            message_id,
            label: notification.label,
            kind: notification.kind,
            outcome: notification.outcome,
            body: notification.body,
        }));
    Redraw(true)
}

/// Handle `MessageEnd { Assistant }`: finalize the in-flight entry (or
/// materialize it on the replay path), then surface any in-band error.
fn reduce_assistant_end(
    state: &mut ChatState,
    agent_id: AgentId,
    assistant: AssistantMessage,
) -> Redraw {
    // A failed turn carries its error in-band on the finalized
    // assistant message. We render it here, on `MessageEnd`, so it
    // lands in transcript order right after the turn's partial content
    // and tool calls rather than out-of-band from the turn's return
    // value.
    //
    // Cancellations are confirmed on the turn-completion path (a
    // cancel can return without an in-band aborted `MessageEnd`), so
    // we skip every abort shape here to avoid a duplicate notice.
    let is_abort = matches!(assistant.stop_reason, StopReason::Aborted)
        || assistant
            .error
            .as_ref()
            .is_some_and(|e| e.category == ErrorCategory::Aborted);
    let error_line = if is_abort {
        None
    } else if let Some(err) = &assistant.error {
        Some(format!("Error: {}", err.message))
    } else if matches!(assistant.stop_reason, StopReason::Error) {
        Some("Error: the model stream failed".to_string())
    } else {
        None
    };

    // Two cases share this path:
    //
    // 1. Live streaming already opened the entry through the painting
    //    `MessageUpdate`s. The finalized snapshot replaces the last
    //    partial (self-healing any dropped update) and unbinds the
    //    streaming target.
    //
    // 2. Replay emits `MessageStart` + `MessageEnd` with no
    //    `MessageUpdate` in between, so no entry exists. We
    //    materialize one from the finalized message, but only when the
    //    payload carries at least one Text / Thinking block:
    //    tool-use-only turns render entirely through the tool cells,
    //    and an empty assistant entry would add a spurious gap.
    let has_renderable = assistant
        .content
        .iter()
        .any(|b| matches!(b, AssistantContent::Text(_) | AssistantContent::Thinking(_)));
    // The sub-agent box renders its report, sourced from the sub's latest
    // assistant conclusion, so capture that text before `assistant` moves
    // into the transcript entry below. Only sub-agents need it, so skip the
    // allocation on the Main path. Concatenating the Text blocks mirrors
    // replay's report capture (latest assistant text wins).
    let conclusion: Option<String> = matches!(agent_id, AgentId::Sub(_)).then(|| {
        assistant
            .content
            .iter()
            .filter_map(|b| match b {
                AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect()
    });
    let current = state
        .render
        .get_mut(&agent_id)
        .and_then(|r| r.current_assistant.take());
    let mut changed = false;
    match current {
        Some(id) => {
            if let Some(EntryKind::Assistant(entry)) = state
                .transcripts
                .get_mut(&agent_id)
                .and_then(|t| t.get_mut(id))
                .map(|e| &mut e.kind)
            {
                entry.message = assistant;
                entry.finalized = true;
                changed = true;
            }
        }
        None if has_renderable => {
            state
                .transcripts
                .entry(agent_id)
                .or_default()
                .append(EntryKind::Assistant(AssistantEntry {
                    message: assistant,
                    finalized: true,
                }));
            changed = true;
        }
        None => {}
    }
    // A sub-agent's box renders its report, not the sub's transcript, so keep
    // the report fresh from the sub's latest conclusion while it runs. A
    // continuation or steering re-run completes through `AgentEnd(Sub n)`,
    // which carries no report, so without this a re-run box would keep
    // showing the first run's conclusion.
    //
    // We refresh only while the box is `Running`. A resumed box is already
    // `Done` with its report set by `SubAgentEnd`, and observing it replays
    // the sub's `MessageEnd`s through here to materialize its transcript.
    // Gating on `Running` keeps that materialize a pure read, so a
    // materialized box renders identically to a freshly resumed one. This
    // matters for a tool-concluding or interleaved sub whose thread-order
    // last assistant differs from replay's bracket-order report. A live
    // re-run flips the box back to `Running` via `AgentStart(Sub n)` before
    // its `MessageEnd`s, so the live path still refreshes.
    //
    // The report tracks the last assistant text even when empty, mirroring
    // replay's `capture_sub_report`, so a tool-concluding sub shows a thin
    // box. We keep the last non-empty `latest_activity` line though.
    if let AgentId::Sub(n) = agent_id
        && let Some(conclusion) = conclusion
        && let Some(b) = state.sub_box_mut(n)
        && b.status == SubAgentStatus::Running
    {
        if !conclusion.is_empty() {
            b.latest_activity = Some(one_line(&conclusion));
        }
        b.report = Some(conclusion);
        changed = true;
    }
    if let Some(line) = error_line {
        append_notice(state, agent_id, NoticeLevel::Error, line);
        changed = true;
    }
    Redraw(changed)
}

/// Append a running tool cell to `agent_id`'s transcript and record
/// the live bookkeeping. Shared by `ToolExecutionStart` and the
/// build-on-miss branch of `ToolExecutionEnd`.
fn append_tool_entry(
    state: &mut ChatState,
    agent_id: AgentId,
    call_id: String,
    tool: String,
    args: Value,
) -> EntryId {
    let header_only = state.header_only_for(agent_id);
    let id = state
        .transcripts
        .entry(agent_id)
        .or_default()
        .append(EntryKind::Tool(ToolEntry {
            call_id: call_id.clone(),
            tool,
            args,
            status: ToolStatus::Running,
            details: None,
            content: Arc::from(Vec::<UserContent>::new()),
            task: None,
            header_only,
        }));
    let render = state.render.entry(agent_id).or_default();
    render.tool_index.insert(call_id, id);
    // A tool call that arrives mid-turn means the assistant message
    // that emitted it is finished as far as the stream is concerned.
    // Drop the streaming target so post-tool assistant text opens a
    // fresh entry *after* the tool.
    render.current_assistant = None;
    id
}

/// Append a notice row to `agent_id`'s transcript.
fn append_notice(state: &mut ChatState, agent_id: AgentId, level: NoticeLevel, text: String) {
    state
        .transcripts
        .entry(agent_id)
        .or_default()
        .append(EntryKind::Notice(NoticeEntry { level, text }));
}

/// Collapse `s`'s runs of whitespace into single spaces, yielding one line.
/// Used for the sub-agent box's single-line latest-activity string.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::model::AgentEntry;

    use std::sync::Mutex;
    use std::time::Duration;

    use aj_agent::bus::listener_from_sync;
    use aj_agent::events::{AgentSettings, CompactionPhase, CompactionReason};
    use aj_agent::message::{AgentMessage, TaskNotification, TaskNotificationKind, TaskOutcome};
    use aj_agent::tool::{TaskKind, ToolDetails};
    use aj_agent::types::TokenUsage;
    use aj_models::registry::ModelInfo;
    use aj_models::types::{
        AssistantError, TextContent, ThinkingContent, ToolCall, Usage, UserMessage,
    };
    use aj_session::ConversationPersistence;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use crate::chat::model::Entry;
    use crate::test_support::{build_test_agent, scripted_run_config};

    fn main_settings() -> AgentSettings {
        AgentSettings {
            provider: "anthropic".into(),
            model_id: "claude-main".into(),
            thinking: "off".into(),
            speed: "standard".into(),
            verbosity: "default".into(),
        }
    }

    fn state_with_catalog(catalog: Vec<ModelInfo>) -> ChatState {
        // 200k matches the canonical Sonnet window so incidental
        // window expectations don't need a synthetic value.
        ChatState::new(main_settings(), 200_000, Arc::new(catalog))
    }

    fn state() -> ChatState {
        state_with_catalog(Vec::new())
    }

    /// Dispatch `event`, discarding the redraw signal.
    fn apply(state: &mut ChatState, life: &mut AgentLifecycle, event: AgentEvent) {
        let _ = reduce(state, life, event);
    }

    fn entries(state: &ChatState, id: AgentId) -> &[Entry] {
        state
            .transcript(id)
            .map(|t| t.entries())
            .unwrap_or_default()
    }

    /// Assistant partial with the scripted-provider identity and the
    /// given content blocks. The reducer stores this snapshot
    /// wholesale, so tests hand it the cumulative content the real
    /// streaming accumulator would carry.
    fn partial_with(content: Vec<AssistantContent>) -> AssistantMessage {
        AssistantMessage {
            content,
            api: "scripted".into(),
            provider: "scripted".into(),
            model: "scripted".into(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        }
    }

    fn empty_partial() -> AssistantMessage {
        partial_with(Vec::new())
    }

    fn text_partial(text: &str) -> AssistantMessage {
        partial_with(vec![AssistantContent::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })])
    }

    fn thinking_partial(thinking: &str) -> AssistantMessage {
        partial_with(vec![AssistantContent::Thinking(ThinkingContent {
            thinking: thinking.into(),
            thinking_signature: None,
            redacted: false,
        })])
    }

    fn message_update(event: AssistantMessageEvent) -> AgentEvent {
        AgentEvent::MessageUpdate {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::Assistant(empty_partial())),
            event,
        }
    }

    fn assistant_message_end(message: AssistantMessage) -> AgentEvent {
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::Assistant(message)),
        }
    }

    fn user_message_end(text: &str) -> AgentEvent {
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::User(UserMessage::text(text))),
        }
    }

    /// A `MessageEnd { TaskNotification }` for a bash task with `label`
    /// and `outcome`, the shape `Agent::drain_task_notices` emits.
    fn task_notification_end(label: &str, outcome: TaskOutcome) -> AgentEvent {
        let body = match outcome {
            TaskOutcome::Succeeded => "exit code 0".to_string(),
            TaskOutcome::Failed { code: Some(c) } => format!("exit code {c}"),
            TaskOutcome::Failed { code: None } => "killed by signal".to_string(),
            TaskOutcome::Killed => "killed".to_string(),
        };
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::task_notification(TaskNotification::new(
                label.to_string(),
                TaskNotificationKind::Bash,
                outcome,
                body,
            )),
        }
    }

    fn errored_assistant_message_end(
        category: ErrorCategory,
        message: &str,
        stop: StopReason,
    ) -> AgentEvent {
        let mut m = empty_partial();
        m.stop_reason = stop;
        m.error = Some(AssistantError::new(category, message));
        assistant_message_end(m)
    }

    fn assistant_text(entry: &Entry) -> &str {
        match &entry.kind {
            EntryKind::Assistant(a) => match &a.message.content[..] {
                [AssistantContent::Text(t)] => &t.text,
                other => panic!("expected one text block, got {other:?}"),
            },
            other => panic!("expected assistant entry, got {other:?}"),
        }
    }

    fn count_kind(state: &ChatState, id: AgentId, pred: fn(&EntryKind) -> bool) -> usize {
        entries(state, id).iter().filter(|e| pred(&e.kind)).count()
    }

    fn sub_settings(provider: &str, model_id: &str) -> AgentSettings {
        AgentSettings {
            provider: provider.into(),
            model_id: model_id.into(),
            thinking: "off".into(),
            speed: "standard".into(),
            verbosity: "default".into(),
        }
    }

    fn sub_agent_start(n: usize, provider: &str, model_id: &str) -> AgentEvent {
        AgentEvent::SubAgentStart {
            parent: AgentId::Main,
            child: AgentId::Sub(n),
            task: format!("task {n}"),
            background: false,
            settings: sub_settings(provider, model_id),
        }
    }

    /// A sub-agent assistant `MessageEnd` carrying a single text block, the
    /// shape a sub-agent's concluding report takes on the live path.
    fn sub_assistant_end(n: usize, text: &str) -> AgentEvent {
        AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(n),
            message: AgentMessage::wire(Message::Assistant(text_partial(text))),
        }
    }

    /// A sub-agent assistant `MessageEnd` whose only content is a tool call,
    /// the shape a tool-concluding sub-agent's final turn takes: no Text
    /// blocks, so its concluding report is empty.
    fn sub_tool_only_end(n: usize, call_id: &str, tool: &str) -> AgentEvent {
        AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(n),
            message: AgentMessage::wire(Message::Assistant(partial_with(vec![
                AssistantContent::ToolCall(ToolCall {
                    id: call_id.into(),
                    name: tool.into(),
                    arguments: serde_json::json!({}),
                }),
            ]))),
        }
    }

    fn bash_task_details(stdout: &str, task_id: Option<usize>) -> ToolDetails {
        ToolDetails::Bash {
            command: "sleep 5".into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: None,
            truncated: false,
            full_output_path: None,
            stdout_truncation: None,
            stderr_truncation: None,
            task_id,
        }
    }

    fn tool_start(agent_id: AgentId, call_id: &str, tool: &str) -> AgentEvent {
        AgentEvent::ToolExecutionStart {
            agent_id,
            call_id: call_id.into(),
            tool: tool.into(),
            args: serde_json::json!({}),
        }
    }

    fn tool_end(agent_id: AgentId, call_id: &str, tool: &str, result: ToolDetails) -> AgentEvent {
        AgentEvent::ToolExecutionEnd {
            agent_id,
            call_id: call_id.into(),
            tool: tool.into(),
            result,
            content: Arc::from(Vec::<UserContent>::new()),
            is_error: false,
        }
    }

    fn token_usage(turn: [u64; 4]) -> TokenUsage {
        TokenUsage {
            accumulated_input: 0,
            turn_input: turn[0],
            accumulated_output: 0,
            turn_output: turn[1],
            accumulated_cache_write: 0,
            turn_cache_write: turn[2],
            accumulated_cache_read: 0,
            turn_cache_read: turn[3],
        }
    }

    #[test]
    fn streaming_materializes_one_entry_and_snapshot_replaces() {
        let mut s = state();
        let mut life = AgentLifecycle::default();

        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: empty_partial(),
            }),
        );
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "Hel".into(),
                partial: text_partial("Hel"),
            }),
        );
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "lo".into(),
                partial: text_partial("Hello"),
            }),
        );

        let rows = entries(&s, AgentId::Main);
        assert_eq!(rows.len(), 1, "streaming builds exactly one entry");
        assert_eq!(assistant_text(&rows[0]), "Hello");
        match &rows[0].kind {
            EntryKind::Assistant(a) => assert!(!a.finalized),
            other => panic!("unexpected kind: {other:?}"),
        }

        // MessageEnd finalizes with the authoritative snapshot and
        // clears the streaming target, so a next turn opens fresh.
        apply(
            &mut s,
            &mut life,
            assistant_message_end(text_partial("Hello!")),
        );
        let rows = entries(&s, AgentId::Main);
        assert_eq!(rows.len(), 1);
        assert_eq!(assistant_text(&rows[0]), "Hello!");
        match &rows[0].kind {
            EntryKind::Assistant(a) => assert!(a.finalized),
            other => panic!("unexpected kind: {other:?}"),
        }
        assert_eq!(
            s.render
                .get(&AgentId::Main)
                .and_then(|r| r.current_assistant),
            None,
        );
    }

    #[test]
    fn tool_use_only_turn_leaves_no_assistant_entry() {
        // A turn where the model emitted only a `tool_use` block must
        // not materialize an empty assistant entry: none of the
        // tool-call `MessageUpdate`s paint, and the `MessageEnd`
        // payload carries no Text / Thinking block.
        let mut s = state();
        let mut life = AgentLifecycle::default();

        apply(&mut s, &mut life, user_message_end("please run a tool"));
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: empty_partial(),
            }),
        );
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"cmd\":\"ls\"}".into(),
                partial: empty_partial(),
            }),
        );
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"cmd": "ls"}),
                },
                partial: empty_partial(),
            }),
        );
        let mut tool_only = empty_partial();
        tool_only.content = vec![AssistantContent::ToolCall(ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"cmd": "ls"}),
        })];
        tool_only.stop_reason = StopReason::ToolUse;
        apply(&mut s, &mut life, assistant_message_end(tool_only));
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Main, "call-1", "bash"),
        );
        apply(
            &mut s,
            &mut life,
            tool_end(
                AgentId::Main,
                "call-1",
                "bash",
                ToolDetails::Text {
                    summary: "bash".into(),
                    body: "ok".into(),
                },
            ),
        );

        assert_eq!(
            count_kind(&s, AgentId::Main, |k| matches!(k, EntryKind::Assistant(_))),
            0,
            "tool-use-only turn must not leave an assistant entry",
        );
        assert_eq!(
            count_kind(&s, AgentId::Main, |k| matches!(k, EntryKind::Tool(_))),
            1,
        );
    }

    #[test]
    fn thinking_stream_survives_empty_snapshot_stop_event() {
        // The agent emits `ThinkingEnd` with an empty `content` field
        // when the provider finalizes a thinking block without an
        // authoritative snapshot. The reducer must consult only the
        // cumulative `partial`, so the accumulated thinking survives.
        let mut s = state();
        let mut life = AgentLifecycle::default();

        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: empty_partial(),
            }),
        );
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "first let me reason about the".into(),
                partial: thinking_partial("first let me reason about the"),
            }),
        );
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: " inputs carefully".into(),
                partial: thinking_partial("first let me reason about the inputs carefully"),
            }),
        );
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: String::new(),
                partial: thinking_partial("first let me reason about the inputs carefully"),
            }),
        );

        let rows = entries(&s, AgentId::Main);
        assert_eq!(rows.len(), 1);
        match &rows[0].kind {
            EntryKind::Assistant(a) => match &a.message.content[..] {
                [AssistantContent::Thinking(t)] => {
                    assert_eq!(t.thinking, "first let me reason about the inputs carefully");
                }
                other => panic!("expected one thinking block, got {other:?}"),
            },
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn terminal_error_message_end_appends_inband_error_row() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            errored_assistant_message_end(
                ErrorCategory::Auth,
                "anthropic provider: no credentials for provider \"anthropic\"",
                StopReason::Error,
            ),
        );
        let rows = entries(&s, AgentId::Main);
        assert_eq!(rows.len(), 1, "exactly one error row");
        match &rows[0].kind {
            EntryKind::Notice(n) => {
                assert_eq!(n.level, NoticeLevel::Error);
                assert!(
                    n.text
                        .starts_with("Error: anthropic provider: no credentials")
                );
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn retryable_terminal_error_renders_in_band() {
        // Rendering keys off "any non-abort error", not
        // retryable-vs-not, so an exhausted retryable attempt's error
        // still shows.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            errored_assistant_message_end(
                ErrorCategory::Transient,
                "stream ended without a terminal event",
                StopReason::Error,
            ),
        );
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::Notice(n) => {
                assert_eq!(n.text, "Error: stream ended without a terminal event");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn aborted_message_end_appends_no_error_row() {
        // Cancellations are confirmed on the turn-completion path, so
        // an aborted `MessageEnd` must not append an error row (that
        // would duplicate the cancel notice). Both abort shapes are
        // skipped: the aborted stop reason and the aborted category.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            errored_assistant_message_end(
                ErrorCategory::Aborted,
                "turn aborted by client",
                StopReason::Aborted,
            ),
        );
        let mut aborted_category_only = empty_partial();
        aborted_category_only.error = Some(AssistantError::new(
            ErrorCategory::Aborted,
            "aborted mid-flight",
        ));
        apply(
            &mut s,
            &mut life,
            assistant_message_end(aborted_category_only),
        );
        assert!(
            entries(&s, AgentId::Main).is_empty(),
            "aborted MessageEnd must append nothing",
        );
    }

    #[test]
    fn errored_stop_without_detail_renders_generic_line() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let mut m = empty_partial();
        m.stop_reason = StopReason::Error;
        apply(&mut s, &mut life, assistant_message_end(m));
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::Notice(n) => {
                assert_eq!(n.level, NoticeLevel::Error);
                assert_eq!(n.text, "Error: the model stream failed");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn user_message_end_skips_empty_and_notification_appends_typed_entry() {
        let mut s = state();
        let mut life = AgentLifecycle::default();

        apply(&mut s, &mut life, user_message_end(""));
        assert!(entries(&s, AgentId::Main).is_empty(), "empty text skipped");

        apply(&mut s, &mut life, user_message_end("hello"));
        apply(
            &mut s,
            &mut life,
            task_notification_end("cargo build", TaskOutcome::Failed { code: Some(1) }),
        );

        let rows = entries(&s, AgentId::Main);
        assert_eq!(rows.len(), 2);
        match &rows[0].kind {
            EntryKind::User(u) => assert_eq!(u.joined_text(), "hello"),
            other => panic!("unexpected kind: {other:?}"),
        }
        // The notice lands as a typed `TaskNotification` entry, not a
        // user prompt, so navigation and export can branch on it.
        match &rows[1].kind {
            EntryKind::TaskNotification(n) => {
                assert_eq!(n.label, "cargo build");
                assert_eq!(n.kind, TaskNotificationKind::Bash);
                assert_eq!(n.outcome, TaskOutcome::Failed { code: Some(1) });
                assert_eq!(n.body, "exit code 1");
            }
            other => panic!("expected TaskNotification, got {other:?}"),
        }
    }

    #[test]
    fn user_message_end_stores_message_id() {
        let mut s = state();
        let mut life = AgentLifecycle::default();

        // Read the minted id off the event before reducing consumes it.
        let event = user_message_end("hello");
        let AgentEvent::MessageEnd { message, .. } = &event else {
            panic!("user_message_end builds a MessageEnd event");
        };
        let expected_id = message.id().to_string();
        assert!(
            !expected_id.is_empty(),
            "a live message mints a non-empty id"
        );

        apply(&mut s, &mut life, event);

        let rows = entries(&s, AgentId::Main);
        assert_eq!(rows.len(), 1);
        match &rows[0].kind {
            EntryKind::User(u) => assert_eq!(u.message_id, expected_id),
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn tool_start_clears_current_assistant_so_post_tool_text_opens_fresh() {
        let mut s = state();
        let mut life = AgentLifecycle::default();

        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "let me think".into(),
                partial: thinking_partial("let me think"),
            }),
        );
        apply(&mut s, &mut life, tool_start(AgentId::Main, "c1", "bash"));
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::TextDelta {
                content_index: 1,
                delta: "done".into(),
                partial: text_partial("done"),
            }),
        );

        let rows = entries(&s, AgentId::Main);
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0].kind, EntryKind::Assistant(_)));
        assert!(matches!(rows[1].kind, EntryKind::Tool(_)));
        assert!(matches!(rows[2].kind, EntryKind::Assistant(_)));
        assert_eq!(assistant_text(&rows[2]), "done");
    }

    #[test]
    fn replay_tool_end_without_start_builds_cell_and_does_not_steal_next_assistant() {
        // Replay emits `ToolExecutionEnd` with no `Start`. The
        // build-on-miss branch must create the cell AND clear
        // `current_assistant`, otherwise the next assistant message
        // would attach to the pre-tool entry and render above the tool.
        let mut s = state();
        let mut life = AgentLifecycle::default();

        apply(&mut s, &mut life, user_message_end("please run a tool"));
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "let me think".into(),
                partial: thinking_partial("let me think"),
            }),
        );
        apply(
            &mut s,
            &mut life,
            tool_end(
                AgentId::Main,
                "call-1",
                "bash",
                ToolDetails::Text {
                    summary: "bash".into(),
                    body: "hello from aj".into(),
                },
            ),
        );
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "Done.".into(),
                partial: text_partial("Done."),
            }),
        );

        let rows = entries(&s, AgentId::Main);
        assert_eq!(rows.len(), 4);
        match &rows[2].kind {
            EntryKind::Tool(t) => {
                assert_eq!(t.status, ToolStatus::Done { is_error: false });
                // Args are unavailable on the End event.
                assert_eq!(t.args, serde_json::json!({}));
            }
            other => panic!("expected the tool cell third, got {other:?}"),
        }
        assert_eq!(
            assistant_text(&rows[3]),
            "Done.",
            "post-tool text must open a fresh entry after the tool",
        );
    }

    #[test]
    fn agent_tool_is_skipped_and_sub_agent_events_route_into_sub_transcript() {
        let mut s = state();
        let mut life = AgentLifecycle::default();

        // Main fires the `agent` tool: no cell anywhere.
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Main, "c-agent", "agent"),
        );
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );
        // The sub-agent's own tool routes into its transcript.
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Sub(1), "c-bash", "bash"),
        );
        apply(
            &mut s,
            &mut life,
            tool_end(
                AgentId::Sub(1),
                "c-bash",
                "bash",
                ToolDetails::Text {
                    summary: String::new(),
                    body: "ok".into(),
                },
            ),
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                report: "done".into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
        );

        assert_eq!(
            count_kind(&s, AgentId::Main, |k| matches!(k, EntryKind::Tool(_))),
            0,
            "the `agent` tool call must not create a cell",
        );
        assert_eq!(
            count_kind(&s, AgentId::Main, |k| matches!(k, EntryKind::SubAgent(_))),
            1,
            "the box represents the sub-agent in the parent transcript",
        );
        let sub_rows = entries(&s, AgentId::Sub(1));
        assert_eq!(sub_rows.len(), 1);
        match &sub_rows[0].kind {
            EntryKind::Tool(t) => {
                assert!(t.header_only, "sub tools are header-only while unobserved");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::SubAgent(b) => {
                assert_eq!(b.status, SubAgentStatus::Done);
                assert_eq!(b.report.as_deref(), Some("done"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn set_active_view_reconciles_header_only_hints() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Sub(1), "c1", "read_file"),
        );

        let header_only = |s: &ChatState| match &entries(s, AgentId::Sub(1))[0].kind {
            EntryKind::Tool(t) => t.header_only,
            other => panic!("unexpected kind: {other:?}"),
        };
        assert!(header_only(&s), "collected on Main: header-only");

        s.set_active_view(AgentId::Sub(1));
        assert!(!header_only(&s), "observing the sub expands its tools");

        // A tool collected while observing arrives with a full body.
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Sub(1), "c2", "read_file"),
        );
        match &entries(&s, AgentId::Sub(1))[1].kind {
            EntryKind::Tool(t) => assert!(!t.header_only),
            other => panic!("unexpected kind: {other:?}"),
        }

        s.set_active_view(AgentId::Main);
        assert!(header_only(&s), "returning to Main re-collapses");
    }

    #[test]
    fn continuation_agent_start_reruns_box_and_agent_end_finishes_it() {
        // A continuation re-prompt emits no `SubAgentStart`/`End`, so
        // the box status is driven purely by `AgentStart(Sub n)` and
        // `AgentEnd(Sub n)`.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );

        let box_status = |s: &mut ChatState| s.sub_box_mut(1).expect("box").status;

        for _ in 0..2 {
            apply(
                &mut s,
                &mut life,
                AgentEvent::AgentStart {
                    agent_id: AgentId::Sub(1),
                },
            );
            assert_eq!(box_status(&mut s), SubAgentStatus::Running);
            apply(
                &mut s,
                &mut life,
                AgentEvent::AgentEnd {
                    agent_id: AgentId::Sub(1),
                    messages: Vec::new(),
                },
            );
            assert_eq!(box_status(&mut s), SubAgentStatus::Done);
        }
    }

    #[test]
    fn sub_agent_end_conclusion_drives_the_box_status() {
        // Each conclusion maps to a distinct box status. On the live path
        // `AgentEnd(Sub n)` fires first and marks the still-running box
        // `Done`; the trailing `SubAgentEnd` carries the conclusion and
        // must set the final `Truncated`/`Failed` without being clobbered.
        for (conclusion, expected) in [
            (SubAgentConclusion::Completed, SubAgentStatus::Done),
            (SubAgentConclusion::Truncated, SubAgentStatus::Truncated),
            (SubAgentConclusion::Failed, SubAgentStatus::Failed),
        ] {
            let mut s = state();
            let mut life = AgentLifecycle::default();
            apply(
                &mut s,
                &mut life,
                sub_agent_start(1, "scripted", "scripted"),
            );
            apply(
                &mut s,
                &mut life,
                AgentEvent::AgentStart {
                    agent_id: AgentId::Sub(1),
                },
            );
            apply(
                &mut s,
                &mut life,
                AgentEvent::AgentEnd {
                    agent_id: AgentId::Sub(1),
                    messages: Vec::new(),
                },
            );
            apply(
                &mut s,
                &mut life,
                AgentEvent::SubAgentEnd {
                    parent: AgentId::Main,
                    child: AgentId::Sub(1),
                    report: "r".into(),
                    conclusion,
                },
            );
            assert_eq!(
                s.sub_box_mut(1).expect("box").status,
                expected,
                "conclusion {conclusion:?} maps to {expected:?}",
            );
        }
    }

    #[test]
    fn agent_end_does_not_clobber_a_concluded_box() {
        // The `AgentEnd` guard matters only if the events reorder so that
        // `SubAgentEnd` (carrying the conclusion) lands before `AgentEnd`.
        // Deliver them that way and assert the conclusion survives.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                report: "r".into(),
                conclusion: SubAgentConclusion::Failed,
            },
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );
        assert_eq!(
            s.sub_box_mut(1).expect("box").status,
            SubAgentStatus::Failed,
        );
    }

    #[test]
    fn continuation_refreshes_the_box_report_from_the_latest_conclusion() {
        // The box renders `report`. A continuation re-run completes through
        // `AgentEnd(Sub n)`, which carries no report, so the report is kept
        // fresh from the sub's latest assistant conclusion. Without that the
        // box would keep showing the first run's report.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );

        // First run: concludes and ends via `SubAgentEnd`.
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        apply(&mut s, &mut life, sub_assistant_end(1, "first result"));
        apply(
            &mut s,
            &mut life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                report: "first result".into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );
        assert_eq!(
            s.sub_box_mut(1).expect("box").report.as_deref(),
            Some("first result"),
        );

        // Continuation re-run: a fresh conclusion, then `AgentEnd` with NO
        // `SubAgentEnd`.
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        apply(&mut s, &mut life, sub_assistant_end(1, "second result"));
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );

        let b = s.sub_box_mut(1).expect("box");
        assert_eq!(b.status, SubAgentStatus::Done);
        assert_eq!(
            b.report.as_deref(),
            Some("second result"),
            "the box report tracks the latest run's conclusion",
        );
    }

    #[test]
    fn continuation_with_a_tool_concluding_run_blanks_the_box_report() {
        // A tool-concluding (or aborted) final turn carries no Text blocks, so
        // its concluding report is empty. The box report must track that empty
        // text rather than keep the previous run's conclusion, because replay's
        // `capture_sub_report` overwrites unconditionally and would show an
        // empty report on a later resume. Diverging here would break parity.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );

        // First run: a prose conclusion, ends via `SubAgentEnd`.
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        apply(&mut s, &mut life, sub_assistant_end(1, "first result"));
        apply(
            &mut s,
            &mut life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                report: "first result".into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );
        assert_eq!(
            s.sub_box_mut(1).expect("box").report.as_deref(),
            Some("first result"),
        );

        // Continuation re-run whose final assistant turn is tool-only, then
        // `AgentEnd` with no `SubAgentEnd`.
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        apply(&mut s, &mut life, sub_tool_only_end(1, "c1", "read_file"));
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );

        let b = s.sub_box_mut(1).expect("box");
        assert_eq!(b.status, SubAgentStatus::Done);
        assert_eq!(
            b.report.as_deref(),
            Some(""),
            "the box report tracks the empty conclusion, not the prior run",
        );
        // The empty turn does not blank the last meaningful activity line.
        assert_eq!(b.latest_activity.as_deref(), Some("first result"));
    }

    #[test]
    fn materialize_does_not_change_a_done_box_report() {
        // Observing a resumed sub materializes its transcript by replaying the
        // sub's `MessageEnd`s through the reducer. Those bare `MessageEnd`s
        // arrive with no preceding `AgentStart(Sub n)`, so the box stays
        // `Done`. The report was set authoritatively by `SubAgentEnd` on the
        // deferred drain and must survive the replay untouched, otherwise a
        // tool-concluding or interleaved sub's box would flip to the
        // thread-order last assistant text and break eager/lazy parity.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        apply(&mut s, &mut life, sub_assistant_end(1, "resume value"));
        apply(
            &mut s,
            &mut life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                report: "resume value".into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
        );
        {
            let b = s.sub_box_mut(1).expect("box");
            assert_eq!(b.status, SubAgentStatus::Done);
            assert_eq!(b.report.as_deref(), Some("resume value"));
        }

        // Materialize: a bare sub `MessageEnd` with no `AgentStart` before it.
        apply(&mut s, &mut life, sub_assistant_end(1, "materialized text"));

        let b = s.sub_box_mut(1).expect("box");
        assert_eq!(
            b.status,
            SubAgentStatus::Done,
            "materialize leaves the box Done",
        );
        assert_eq!(
            b.report.as_deref(),
            Some("resume value"),
            "materialize is a pure read: the Done box report is not rewritten",
        );
    }

    #[test]
    fn sub_activity_updates_latest_activity() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );
        {
            let b = s.sub_box_mut(1).expect("box");
            assert_eq!(b.latest_activity, None);
        }

        // A sub assistant conclusion sets the collapsed one-line activity.
        apply(
            &mut s,
            &mut life,
            sub_assistant_end(1, "line one\n  line two"),
        );
        {
            let b = s.sub_box_mut(1).expect("box");
            assert_eq!(b.latest_activity.as_deref(), Some("line one line two"));
        }

        // A sub tool start sets the tool name.
        apply(&mut s, &mut life, tool_start(AgentId::Sub(1), "c1", "grep"));
        let b = s.sub_box_mut(1).expect("box");
        assert_eq!(b.latest_activity.as_deref(), Some("grep"));
    }

    fn agent_row(s: &ChatState, id: AgentId) -> AgentEntry {
        s.agents()
            .into_iter()
            .find(|a| a.id == id)
            .expect("agent row present")
    }

    #[test]
    fn sub_agent_end_freezes_the_runtime() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );

        // Running: no end recorded, and the snapshot reports a (ticking) runtime.
        assert!(s.sub_box_mut(1).expect("box").finished_at.is_none());
        assert!(
            agent_row(&s, AgentId::Sub(1)).runtime.is_some(),
            "a running sub has a runtime"
        );

        apply(
            &mut s,
            &mut life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                report: "done".into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
        );
        // Ended: the end is stamped and the reported runtime no longer moves.
        assert!(s.sub_box_mut(1).expect("box").finished_at.is_some());
        let r1 = agent_row(&s, AgentId::Sub(1)).runtime;
        let r2 = agent_row(&s, AgentId::Sub(1)).runtime;
        assert_eq!(r1, r2, "a finished sub's runtime is frozen");
    }

    #[test]
    fn continuation_rerun_restarts_the_runtime_clock() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );
        let spawn_start = s.sub_box_mut(1).expect("box").started_at;

        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );
        assert!(s.sub_box_mut(1).expect("box").finished_at.is_some());

        // Idle, then a continuation re-run. The clock restarts (start advances,
        // end cleared) so the runtime times the new run, not the idle gap.
        std::thread::sleep(std::time::Duration::from_millis(2));
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        let b = s.sub_box_mut(1).expect("box");
        assert!(b.finished_at.is_none(), "re-run clears the frozen end");
        assert!(b.started_at > spawn_start, "re-run resets the start");
    }

    #[test]
    fn agents_flag_background_subs_from_the_spawn_event() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        // Sub 1 spawned blocking (foreground), sub 2 spawned background.
        // The flag comes from each sub's own `SubAgentStart`, so it does
        // not depend on the transient task registry and survives a resume.
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(2),
                task: "task 2".into(),
                background: true,
                settings: sub_settings("scripted", "scripted"),
            },
        );
        assert!(!agent_row(&s, AgentId::Sub(1)).background, "blocking sub");
        assert!(agent_row(&s, AgentId::Sub(2)).background, "background sub");

        // The classification survives the sub finishing.
        apply(
            &mut s,
            &mut life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(2),
                report: "done".into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
        );
        assert!(
            agent_row(&s, AgentId::Sub(2)).background,
            "still background after it ends"
        );
    }

    #[test]
    fn agents_main_row_has_no_runtime_or_background() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );
        let main = agent_row(&s, AgentId::Main);
        assert_eq!(main.runtime, None);
        assert!(!main.background);
    }

    #[test]
    fn task_output_tails_cell_via_snapshot_after_owner_agent_end_and_task_end_freezes() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        apply(&mut s, &mut life, tool_start(AgentId::Main, "c1", "bash"));
        apply(
            &mut s,
            &mut life,
            tool_end(AgentId::Main, "c1", "bash", bash_task_details("", Some(1))),
        );
        // The detached driver races the tool future's return, so
        // `TaskStart` can land on either side of `ToolExecutionEnd`.
        // This test feeds the post-result order.
        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskStart {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                kind: TaskKind::Bash {
                    command: "sleep 5".into(),
                },
                label: "sleep 5".into(),
            },
        );

        let cell_details = |s: &ChatState| -> String {
            match &entries(s, AgentId::Main)[0].kind {
                EntryKind::Tool(t) => match t.details.as_ref().expect("details") {
                    ToolDetails::Bash { stdout, .. } => stdout.clone(),
                    other => panic!("expected bash details, got {other:?}"),
                },
                other => panic!("unexpected kind: {other:?}"),
            }
        };
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::Tool(t) => assert_eq!(t.task, Some(1), "cell carries the task badge"),
            other => panic!("unexpected kind: {other:?}"),
        }

        // The owning turn ends. This wipes the agent's tool_index, so
        // subsequent routing exercises the TaskStart snapshot.
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskOutput {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                partial: bash_task_details("LIVETAIL", Some(1)),
            },
        );
        assert_eq!(cell_details(&s), "LIVETAIL");

        // TaskEnd freezes the task. A straggling snapshot no longer
        // lands.
        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskEnd {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                status: TaskStatus::Exited(Some(0)),
                label: "sleep 5".into(),
            },
        );
        let info = s.tasks().get(&1).expect("tracked task");
        assert_eq!(info.status, TaskStatus::Exited(Some(0)));
        assert!(info.finished_at.is_some(), "TaskEnd freezes the runtime");

        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskOutput {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                partial: bash_task_details("AFTERFREEZE", Some(1)),
            },
        );
        assert_eq!(cell_details(&s), "LIVETAIL", "frozen cell keeps its tail");
    }

    #[test]
    fn task_start_after_owner_agent_end_still_finds_the_launch_cell() {
        // The detached driver emits `TaskStart` and in the extreme it
        // lands only after the owner's `AgentEnd` already wiped the
        // `tool_index`. The linkage recorded at `ToolExecutionEnd` is
        // what keeps the badge and the live tail working then.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        apply(&mut s, &mut life, tool_start(AgentId::Main, "c1", "bash"));
        apply(
            &mut s,
            &mut life,
            tool_end(AgentId::Main, "c1", "bash", bash_task_details("", Some(1))),
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskStart {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                kind: TaskKind::Bash {
                    command: "sleep 5".into(),
                },
                label: "sleep 5".into(),
            },
        );

        // The cell got its task linkage despite the wiped index.
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::Tool(t) => assert_eq!(t.task, Some(1), "cell carries the task badge"),
            other => panic!("unexpected kind: {other:?}"),
        }
        // Remove-on-consume: the fallback entry is claimed exactly
        // once.
        assert!(
            s.pending_task_cells.is_empty(),
            "consumed linkage is removed"
        );

        // Live tail still lands in the launch cell.
        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskOutput {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                partial: bash_task_details("LIVETAIL", Some(1)),
            },
        );
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::Tool(t) => match t.details.as_ref().expect("details") {
                ToolDetails::Bash { stdout, .. } => assert_eq!(stdout, "LIVETAIL"),
                other => panic!("expected bash details, got {other:?}"),
            },
            other => panic!("unexpected kind: {other:?}"),
        }

        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskEnd {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                status: TaskStatus::Exited(Some(0)),
                label: "sleep 5".into(),
            },
        );
        let info = s.tasks().get(&1).expect("tracked task");
        assert_eq!(info.status, TaskStatus::Exited(Some(0)));
    }

    #[test]
    fn agent_task_start_does_not_claim_a_replayed_bash_cell() {
        // Task ids restart at 1 per session world, so a resumed
        // session's first background task can collide with a task id
        // in a replayed launch cell. The `agent` tool has no cell (it
        // renders as a sub-agent box), so a background agent task's
        // `TaskStart` misses the live `tool_index`, and the fallback,
        // keyed by the launching `call_id`, must not hand it the
        // stale bash cell either.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        // Replay: the persisted launch result arrives without a
        // preceding Start and seeds the fallback map with the old
        // session's task id.
        apply(
            &mut s,
            &mut life,
            tool_end(
                AgentId::Main,
                "c-resumed",
                "bash",
                bash_task_details("", Some(1)),
            ),
        );
        // Live: a background agent spawn mints task id 1 in the
        // fresh registry.
        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskStart {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c-agent".into(),
                kind: TaskKind::Agent {
                    agent_id: 1,
                    task: "explore".into(),
                },
                label: "agent 1".into(),
            },
        );

        // The agent task tracks no cell, and the replayed bash cell
        // stays unclaimed.
        let info = s.tasks().get(&1).expect("tracked task");
        assert_eq!(info.cell, None, "agent tasks have no launch cell");
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::Tool(t) => assert_eq!(t.task, None, "replayed cell stays unclaimed"),
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn resumed_launch_cell_and_unknown_task_end_are_inert() {
        // Resume fidelity: task events are transient, so replay only
        // delivers the persisted launch result. The cell carries its
        // badge in the Bash payload while the model tracks no task.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            tool_end(
                AgentId::Main,
                "c-resumed",
                "bash",
                bash_task_details("", Some(7)),
            ),
        );
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::Tool(t) => {
                assert_eq!(t.task, None, "no TaskStart, no tracked badge");
                match t.details.as_ref().expect("details") {
                    ToolDetails::Bash { task_id, .. } => assert_eq!(*task_id, Some(7)),
                    other => panic!("expected bash details, got {other:?}"),
                }
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        assert!(s.tasks().is_empty());
        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskEnd {
                agent_id: AgentId::Main,
                task_id: 7,
                call_id: "c-resumed".into(),
                status: TaskStatus::Exited(Some(0)),
                label: "sleep 5".into(),
            },
        );
        assert!(s.tasks().is_empty(), "unknown TaskEnd tracks nothing");
    }

    #[test]
    fn agent_end_clears_only_that_agents_render_bookkeeping() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );

        // In-flight state on both agents.
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Main, "c-main", "bash"),
        );
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hi".into(),
                partial: text_partial("hi"),
            }),
        );
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Sub(1), "c-sub", "bash"),
        );

        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );

        let sub = s.render.get(&AgentId::Sub(1)).expect("sub render");
        assert!(sub.tool_index.is_empty());
        assert_eq!(sub.current_assistant, None);
        let main = s.render.get(&AgentId::Main).expect("main render");
        assert_eq!(main.tool_index.len(), 1, "main bookkeeping untouched");
        assert!(main.current_assistant.is_some());
    }

    #[test]
    fn lifecycle_sets_track_agent_and_compaction_events() {
        let mut s = state();
        let mut life = AgentLifecycle::default();

        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        assert!(life.is_running(AgentId::Main));
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );
        assert!(!life.is_running(AgentId::Main));

        apply(
            &mut s,
            &mut life,
            AgentEvent::CompactionStart {
                agent_id: AgentId::Main,
                reason: CompactionReason::Manual,
            },
        );
        assert!(life.is_compacting(AgentId::Main));
        assert_eq!(s.compaction_phase(AgentId::Main), None, "starting phase");

        apply(
            &mut s,
            &mut life,
            AgentEvent::CompactionProgress {
                agent_id: AgentId::Main,
                reason: CompactionReason::Manual,
                phase: CompactionPhase::Saving,
            },
        );
        assert_eq!(
            s.compaction_phase(AgentId::Main),
            Some(CompactionPhase::Saving),
        );

        apply(
            &mut s,
            &mut life,
            AgentEvent::CompactionEnd {
                agent_id: AgentId::Main,
                reason: CompactionReason::Manual,
                tokens_before: 1_200,
                tokens_after: 300,
                summary: Some("did stuff".into()),
                error: None,
            },
        );
        assert!(!life.is_compacting(AgentId::Main));
        assert_eq!(s.compaction_phase(AgentId::Main), None);
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::Compaction(c) => {
                assert_eq!(c.tokens_before, 1_200);
                assert_eq!(c.tokens_after, 300);
                assert_eq!(c.summary, "did stuff");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        // No UsageUpdate follows a compaction, so the footer numerator
        // is refreshed directly.
        assert_eq!(s.footers().context_usage(AgentId::Main).tokens, Some(300));
    }

    #[test]
    fn compaction_end_failure_and_cancel_render_notices() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            AgentEvent::CompactionEnd {
                agent_id: AgentId::Main,
                reason: CompactionReason::Manual,
                tokens_before: 0,
                tokens_after: 0,
                summary: None,
                error: Some("summarizer failed".into()),
            },
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::CompactionEnd {
                agent_id: AgentId::Main,
                reason: CompactionReason::Manual,
                tokens_before: 0,
                tokens_after: 0,
                summary: None,
                error: None,
            },
        );
        let rows = entries(&s, AgentId::Main);
        match &rows[0].kind {
            EntryKind::Notice(n) => {
                assert_eq!(n.level, NoticeLevel::Warning);
                assert_eq!(n.text, "Compaction failed: summarizer failed");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        match &rows[1].kind {
            EntryKind::Notice(n) => {
                assert_eq!(n.level, NoticeLevel::Info);
                assert_eq!(n.text, "Compaction canceled.");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn stream_retry_appends_warning_with_cadence_line() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            AgentEvent::StreamRetry {
                agent_id: AgentId::Main,
                attempt: 3,
                delay: Duration::from_millis(250),
                error: "overloaded".into(),
            },
        );
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::Notice(n) => {
                assert_eq!(n.level, NoticeLevel::Warning);
                assert_eq!(n.text, "Retrying inference (attempt 3, in 250ms)…");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn notice_warning_error_append_leveled_rows() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: "info".into(),
            },
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::Warning {
                agent_id: AgentId::Main,
                text: "warn".into(),
            },
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::Error {
                agent_id: AgentId::Main,
                text: "err".into(),
            },
        );
        let levels: Vec<NoticeLevel> = entries(&s, AgentId::Main)
            .iter()
            .map(|e| match &e.kind {
                EntryKind::Notice(n) => n.level,
                other => panic!("unexpected kind: {other:?}"),
            })
            .collect();
        assert_eq!(
            levels,
            vec![NoticeLevel::Info, NoticeLevel::Warning, NoticeLevel::Error],
        );
    }

    #[test]
    fn usage_update_appends_structured_row_and_folds_footer() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: token_usage([1_000, 999, 50, 200]),
            },
        );
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::TurnUsage(u) => {
                assert_eq!(u.agent_id, AgentId::Main);
                assert_eq!(
                    u.line(),
                    "Token Usage - Input: 0+1000 | Output: 0+999 | Cache Creation: 0+50 | Cache Read: 0+200",
                );
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        // Occupancy numerator: turn_input + cache_read + cache_write.
        assert_eq!(s.footers().context_usage(AgentId::Main).tokens, Some(1_250),);
    }

    #[test]
    fn sub_agent_start_resolves_window_via_catalog_main_identity_and_miss() {
        let catalog_model = ModelInfo {
            id: "gpt-sub".into(),
            name: "gpt-sub".into(),
            family: None,
            api: "anthropic-messages".into(),
            provider: "openai".into(),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            reasoning_options: Vec::new(),
            supports_verbosity: false,
            input: vec![aj_models::registry::InputModality::Text],
            cost: aj_models::registry::ModelCost::default(),
            context_window: 400_000,
            max_tokens: 100,
        };
        let mut s = state_with_catalog(vec![catalog_model]);
        let mut life = AgentLifecycle::default();

        // Catalog hit.
        apply(&mut s, &mut life, sub_agent_start(1, "openai", "gpt-sub"));
        assert_eq!(
            s.footers().context_usage(AgentId::Sub(1)).context_window,
            400_000,
        );
        // Catalog miss with a Main-identity match: Main's window.
        apply(
            &mut s,
            &mut life,
            sub_agent_start(2, "anthropic", "claude-main"),
        );
        assert_eq!(
            s.footers().context_usage(AgentId::Sub(2)).context_window,
            200_000,
        );
        // Full miss: 0 suppresses the indicator.
        apply(&mut s, &mut life, sub_agent_start(3, "mystery", "unknown"));
        assert_eq!(s.footers().context_usage(AgentId::Sub(3)).context_window, 0,);
    }

    #[test]
    fn queue_update_pings_redraw_and_turn_events_do_not() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let redraw = reduce(
            &mut s,
            &mut life,
            AgentEvent::QueueUpdate {
                agent_id: AgentId::Main,
                steering: Vec::new(),
                follow_up: Vec::new(),
            },
        );
        assert!(redraw.0, "QueueUpdate is a redraw ping");
        assert!(entries(&s, AgentId::Main).is_empty(), "no entry appended");

        let redraw = reduce(
            &mut s,
            &mut life,
            AgentEvent::TurnStart {
                agent_id: AgentId::Main,
            },
        );
        assert!(!redraw.0, "TurnStart is bookkeeping only");
    }

    /// End-to-end: drive a real scripted agent, record its bus events,
    /// and feed the stream through `reduce`, so the reducer is
    /// exercised against the exact event shapes the agent emits
    /// (including the accumulator-built `partial` snapshots).
    #[tokio::test]
    async fn scripted_turn_reduces_to_user_and_finalized_assistant_entries() {
        use crate::test_support::finalized_text_message;

        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("hello from scripted")]);
        let (mut agent, _log, _persistence) = build_test_agent(&persistence, &run_config);

        let recorded: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            sink.lock().unwrap().push(event.clone());
        }));
        agent
            .prompt("hi".into(), CancellationToken::new())
            .await
            .expect("scripted turn");

        let mut s = ChatState::new(
            AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            200_000,
            Arc::new(Vec::new()),
        );
        let mut life = AgentLifecycle::default();
        for event in recorded.lock().unwrap().drain(..) {
            let _ = reduce(&mut s, &mut life, event);
        }

        assert!(!life.is_running(AgentId::Main), "turn settled idle");
        let rows = entries(&s, AgentId::Main);
        let user = rows
            .iter()
            .find_map(|e| match &e.kind {
                EntryKind::User(u) => Some(u),
                _ => None,
            })
            .expect("a user entry");
        assert_eq!(user.joined_text(), "hi");
        let assistants: Vec<&AssistantEntry> = rows
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Assistant(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(assistants.len(), 1, "one assistant entry for one turn");
        assert!(assistants[0].finalized);
        match &assistants[0].message.content[..] {
            [AssistantContent::Text(t)] => assert_eq!(t.text, "hello from scripted"),
            other => panic!("expected one text block, got {other:?}"),
        }
    }
}
