//! The [`AgentEvent`] reducer: folds one event into the [`ChatState`].
//!
//! One arm per event variant. Routing is by the event's agent id into
//! that agent's transcript and render bookkeeping. The reducer takes
//! the event by value so payloads (the assistant `partial`, tool
//! `content`) move into the model instead of being cloned. Persistence
//! is a separate bus subscriber, so nothing downstream needs the event
//! intact.

use std::collections::btree_map::Entry as BTreeMapEntry;
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
use aj_session::EntryId as LogEntryId;
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
///
/// `entry` is the log entry the event derives from: `Some` for a durable
/// frame (the wire envelope's `entry_id`, spec 6.4), `None` for a
/// locally emitted event, for a live non-durable one, and for every
/// event of a dead-log replay.
///
/// It is the durable identity of the effects whose event carries none of
/// its own: a compaction checkpoint's summary row and a projected
/// settings notice. Handing it in is what lets a re-served backfill
/// update those rows in place instead of appending a second one, which
/// the cursor invariant cannot do for them (spec 6.5). It also tells a
/// `SubAgentStart` that names a spawn root from the entry-less bracketing
/// glue a backfill synthesizes for a run in progress. Passing `None`
/// for an event that is in fact durable is safe when the fold starts
/// from fresh state, which is what local resume does, and grows
/// duplicate rows under re-application.
pub fn reduce(
    state: &mut ChatState,
    lifecycle: &mut AgentLifecycle,
    event: AgentEvent,
    entry: Option<&LogEntryId>,
) -> Redraw {
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
            // to `Running`.
            if let AgentId::Sub(n) = agent_id {
                state.reopen_sub_box(n);
            }
            Redraw(true)
        }
        AgentEvent::AgentEnd { agent_id, .. } => {
            lifecycle.mark_idle(agent_id);
            // Each agent owns its streaming bookkeeping, so an agent's
            // end clears only its own entry. The main agent's pending
            // `agent` tool call (whose body is a sub-agent run) is
            // unaffected. The durable-identity indexes stay: a
            // re-applied event for a cell or message this agent already
            // rendered has to find it, whichever turn it came from.
            if let Some(render) = state.render.get_mut(&agent_id) {
                render.current_assistant = None;
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
            // The arms below consume `message.kind`, and `id()` borrows
            // the whole message, so read the durable id up front. It is
            // also the message's log entry id, which the log adopts on
            // append and the wire codec backfills on decode, so these
            // arms need no separate `entry`. Tool results render through
            // `ToolExecutionEnd` and get no entry of their own, so they
            // need no identity.
            let message_id = match &message.kind {
                AgentMessageKind::Wire(Message::User(_) | Message::Assistant(_))
                | AgentMessageKind::TaskNotification(_) => durable_id(message.id()),
                AgentMessageKind::Wire(Message::ToolResult(_)) => None,
            };
            match message.kind {
                AgentMessageKind::Wire(Message::User(user)) => {
                    reduce_user_end(state, agent_id, user, message_id)
                }
                AgentMessageKind::Wire(Message::Assistant(assistant)) => {
                    reduce_assistant_end(state, agent_id, assistant, message_id)
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
            // A `call_id` we already render is the same call: a
            // re-projected start refreshes the cell rather than adding a
            // second one. Status and result stay untouched, so a start
            // re-delivered after the call finished cannot un-finish it.
            if let Some(id) = indexed_tool(state, agent_id, &call_id)
                && let Some(cell) = state.tool_entry_mut(agent_id, id)
            {
                cell.tool = tool;
                cell.args = args;
            } else {
                append_tool_entry(state, agent_id, call_id, tool, args);
            }
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
            let Some(id) = indexed_tool(state, agent_id, &call_id) else {
                // No cell for this call in this transcript: a snapshot
                // for a call this client never saw open. The call's end
                // builds the cell from its result, so only this partial
                // view of it is lost.
                return Redraw(false);
            };
            // A cumulative snapshot paints a running cell only. Once the
            // call concluded, `ToolExecutionEnd` put the authoritative
            // result there, and a snapshot arriving late (one that was in
            // flight at an attach boundary, or that raced the result)
            // must not overwrite it with a partial. Correctness never
            // depends on a lossy frame (spec 6.4), so dropping it is the
            // only safe reading. `TaskOutput` freezes the same way.
            match state.tool_entry_mut(agent_id, id) {
                Some(cell) if cell.status == ToolStatus::Running => {
                    cell.details = Some(partial);
                    cell.content = content;
                    Redraw(true)
                }
                _ => Redraw(false),
            }
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
            let id = match indexed_tool(state, agent_id, &call_id) {
                Some(id) => id,
                None => append_tool_entry(
                    state,
                    agent_id,
                    call_id.clone(),
                    tool,
                    Value::Object(Default::default()),
                ),
            };
            // A bash result carrying a task id is a background launch, and a
            // tracked task's *structured* body belongs to its `TaskOutput`
            // snapshots rather than to the launch result: the launch's own
            // snapshot is empty, so re-projecting this bracket would blank
            // whatever the task has streamed since (`TaskEnd` never repaints,
            // and the next snapshot is not guaranteed either). An untracked
            // task (a resumed cell, or a client that joined after the launch)
            // has nobody else to paint it, so the result lands.
            //
            // The wire content is never contested: `TaskOutput` carries none,
            // so no snapshot can have painted it, and it is the durable result
            // the model saw. Freezing it too would make the cell's content
            // depend on whether `TaskStart` won its race with this event,
            // which is a race the launching tool documents as either-order.
            let mut frozen = false;
            if let ToolDetails::Bash {
                task_id: Some(task_id),
                ..
            } = &result
            {
                frozen = state.tasks.contains_key(task_id);
            }
            if let Some(cell) = state.tool_entry_mut(agent_id, id) {
                cell.status = ToolStatus::Done { is_error };
                cell.content = content;
                if !frozen {
                    cell.details = Some(result);
                }
            }
            Redraw(true)
        }

        // ---- Notices --------------------------------------------------------
        AgentEvent::Notice { agent_id, text } => {
            // The projected notice of a settings entry is durable, and
            // `entry` is the only identity it has: a re-served suffix
            // updates the row it already produced instead of appending a
            // second one. A locally raised notice carries none and
            // appends.
            record_notice(
                state,
                agent_id,
                NoticeLevel::Info,
                text,
                entry.map(String::as_str),
            );
            Redraw(true)
        }
        AgentEvent::Warning { agent_id, text } => {
            record_notice(state, agent_id, NoticeLevel::Warning, text, None);
            Redraw(true)
        }
        AgentEvent::Error { agent_id, text } => {
            record_notice(state, agent_id, NoticeLevel::Error, text, None);
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
            record_notice(state, agent_id, NoticeLevel::Warning, text, None);
            Redraw(true)
        }

        // ---- Per-turn token usage --------------------------------------------
        AgentEvent::UsageUpdate { agent_id, usage } => {
            // Every agent's usage folds into its own footer entry. The
            // rendered footer tracks the viewed agent, so views repaint
            // it only when `agent_id == active_view`.
            state.footers.record_turn_usage(agent_id, &usage);
            // The row belongs to the assistant message it follows, which
            // is this row's durable identity, so a re-applied update
            // overwrites its row instead of adding one.
            let after = state
                .render
                .get(&agent_id)
                .and_then(|r| r.last_finalized_assistant.clone());
            let existing = after
                .as_deref()
                .and_then(|after| indexed_row(state, agent_id, after, usage_origin));
            match existing {
                Some(id) => {
                    if let Some(EntryKind::TurnUsage(row)) = state
                        .transcripts
                        .get_mut(&agent_id)
                        .and_then(|t| t.get_mut(id))
                        .map(|e| &mut e.kind)
                    {
                        row.usage = usage;
                    }
                }
                None => {
                    state
                        .transcripts
                        .entry(agent_id)
                        .or_default()
                        .append(EntryKind::TurnUsage(TurnUsageEntry {
                            agent_id,
                            usage,
                            after_message_id: after,
                        }));
                }
            }
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
                // The failure and cancel branches append no log entry, so
                // they have no durable identity to key on and none is
                // needed: no entry exists for a backfill to regenerate
                // them from, and the frame that carries them is
                // reliable-transient, delivered exactly once.
                record_notice(
                    state,
                    agent_id,
                    NoticeLevel::Warning,
                    format!("Compaction failed: {err}"),
                    None,
                );
            } else if let Some(summary) = summary {
                // A successful compaction appends its checkpoint entry,
                // and `CompactionEnd` carries no identity of its own, so
                // that entry is the row's key. The cursor invariant is not
                // enough on its own: it is a de-duplication optimization
                // (spec 6.5), and a client that offers an older cursor or
                // re-attaches under a fresh epoch is served the entry
                // again.
                let existing =
                    entry.and_then(|entry| indexed_row(state, agent_id, entry, compaction_origin));
                match existing {
                    Some(id) => {
                        if let Some(EntryKind::Compaction(row)) =
                            entry_kind_mut(state, agent_id, id)
                        {
                            row.tokens_before = tokens_before;
                            row.tokens_after = tokens_after;
                            row.summary = summary;
                        }
                    }
                    None => {
                        state.transcripts.entry(agent_id).or_default().append(
                            EntryKind::Compaction(CompactionEntry {
                                tokens_before,
                                tokens_after,
                                summary,
                                entry: entry.cloned(),
                            }),
                        );
                    }
                }
                // No `UsageUpdate` follows a compaction, so refresh the
                // footer occupancy directly to the post-compaction
                // estimate.
                state.footers.set_context_tokens(agent_id, tokens_after);
            } else {
                record_notice(
                    state,
                    agent_id,
                    NoticeLevel::Info,
                    "Compaction canceled.".to_string(),
                    None,
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
                // (initially `Running`). The footer count comes from the
                // paired `AgentStart(Sub n)`, not from here.
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
                } else if entry.is_none() {
                    // A start that carries no log entry is not a spawn root:
                    // it is the bracketing glue a projection synthesizes for
                    // a run whose own start fell below the cursor, so that
                    // run is in progress and the events after it belong to
                    // it. Re-opening the box is what lets those events land,
                    // the report refresh on the sub's conclusions firing only
                    // on a `Running` box. Without it a client re-attaching
                    // during a continuation keeps the previous run's report
                    // for good (spec 6.5).
                    //
                    // A durable start names a spawn root, and a root is
                    // minted once per run, so re-serving one for a box we
                    // already hold leaves its conclusion alone.
                    state.reopen_sub_box(n);
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
            // The launch cell is the owner's tool cell for the launching
            // `call_id`. `TaskStart` is unordered relative to the
            // launch's own `ToolExecutionEnd`, but either side of that
            // race leaves the cell in `tool_index`, which outlives the
            // turn, so one lookup covers both. An agent-kind task has no
            // cell at all (its `agent` tool renders as a sub-agent box),
            // so it misses and stays unbadged.
            if let Some(id) = indexed_tool(state, agent_id, &call_id)
                && let Some(cell) = state.tool_entry_mut(agent_id, id)
            {
                cell.task = Some(task_id);
            }
            match state.tasks.entry(task_id) {
                BTreeMapEntry::Vacant(slot) => {
                    slot.insert(TaskInfo {
                        kind,
                        label,
                        owner: agent_id,
                        call_id,
                        status: TaskStatus::Running,
                        started_at: Instant::now(),
                        finished_at: None,
                    });
                }
                // A re-applied start refreshes what the event carries and
                // nothing else: overwriting the whole entry would
                // un-finish a task that already ended and restart its
                // runtime clock.
                BTreeMapEntry::Occupied(mut slot) => {
                    let info = slot.get_mut();
                    info.kind = kind;
                    info.label = label;
                    info.owner = agent_id;
                    info.call_id = call_id;
                }
            }
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
            let Some((owner, id)) = state.task_cell(task_id) else {
                return Redraw(false);
            };
            if let Some(cell) = state.tool_entry_mut(owner, id) {
                // `TaskOutput` carries no wire content, only the
                // cumulative `ToolDetails` snapshot.
                cell.details = Some(partial);
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
                    // The streaming envelope mints a fresh id per event,
                    // so a partial has no durable identity yet. The
                    // finalizing `MessageEnd` fills it in.
                    message_id: None,
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
    message_id: Option<String>,
) -> Redraw {
    let text = joined_user_text(&user.content);
    if text.is_empty() {
        return Redraw(false);
    }
    if let Some(id) = indexed_message(state, agent_id, message_id.as_deref())
        && let Some(EntryKind::User(entry)) = entry_kind_mut(state, agent_id, id)
    {
        entry.content = user.content;
        return Redraw(true);
    }
    let id = state
        .transcripts
        .entry(agent_id)
        .or_default()
        .append(EntryKind::User(UserEntry {
            message_id: message_id.clone(),
            content: user.content,
        }));
    remember_message(state, agent_id, message_id.as_deref(), id);
    Redraw(true)
}

/// Handle `MessageEnd { TaskNotification }`: append the typed
/// task-completion notice entry, which the view renders as an
/// outcome-tinted bubble rather than a user prompt.
fn reduce_task_notification_end(
    state: &mut ChatState,
    agent_id: AgentId,
    notification: TaskNotification,
    message_id: Option<String>,
) -> Redraw {
    if let Some(id) = indexed_message(state, agent_id, message_id.as_deref())
        && let Some(EntryKind::TaskNotification(entry)) = entry_kind_mut(state, agent_id, id)
    {
        entry.label = notification.label;
        entry.kind = notification.kind;
        entry.outcome = notification.outcome;
        entry.body = notification.body;
        return Redraw(true);
    }
    let id = state
        .transcripts
        .entry(agent_id)
        .or_default()
        .append(EntryKind::TaskNotification(TaskNotificationEntry {
            message_id: message_id.clone(),
            label: notification.label,
            kind: notification.kind,
            outcome: notification.outcome,
            body: notification.body,
        }));
    remember_message(state, agent_id, message_id.as_deref(), id);
    Redraw(true)
}

/// The entry `message_id` already produced in `agent_id`'s transcript,
/// when this client rendered that message before.
fn indexed_message(
    state: &ChatState,
    agent_id: AgentId,
    message_id: Option<&str>,
) -> Option<EntryId> {
    state
        .render
        .get(&agent_id)?
        .message_index
        .get(message_id?)
        .copied()
}

/// Record `id` as the entry for durable `message_id`.
fn remember_message(
    state: &mut ChatState,
    agent_id: AgentId,
    message_id: Option<&str>,
    id: EntryId,
) {
    let Some(message_id) = message_id else {
        return;
    };
    state
        .render
        .entry(agent_id)
        .or_default()
        .message_index
        .insert(message_id.to_string(), id);
}

/// The cell a tool event belongs to in `agent_id`'s transcript.
///
/// A non-empty `call_id` is durable identity and resolves through the
/// index, which outlives the turn, so a re-projected bracket finds the
/// cell it already produced. The id is assumed unique for the session,
/// which holds because providers mint one per call.
///
/// An empty `call_id` is not an identity: the OpenAI adapter builds a
/// `ToolCall` with an empty id and fills it in only when the wire delta
/// carries one, so indexing it would collapse every id-less call in the
/// session onto a single cell. Such a call is instead correlated only
/// while it runs, which is enough for live flow (its update and its
/// result arrive before it concludes) and is all that can be done: a
/// re-served bracket for an id-less call has nothing to match on and
/// renders a second cell.
fn indexed_tool(state: &ChatState, agent_id: AgentId, call_id: &str) -> Option<EntryId> {
    if call_id.is_empty() {
        return running_unidentified_cell(state, agent_id);
    }
    state
        .render
        .get(&agent_id)?
        .tool_index
        .get(call_id)
        .copied()
}

/// The most recent still-running cell with no `call_id`.
fn running_unidentified_cell(state: &ChatState, agent_id: AgentId) -> Option<EntryId> {
    state
        .transcript(agent_id)?
        .entries()
        .iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            EntryKind::Tool(cell)
                if cell.call_id.is_empty() && cell.status == ToolStatus::Running =>
            {
                Some(entry.id)
            }
            _ => None,
        })
}

/// The durable identity of a wire id, or `None` when it has none.
///
/// An empty id is never an identity: several messages can carry one (a
/// message deserialized outside the log's backfill path), so keying on it
/// would alias unrelated rows.
fn durable_id(id: &str) -> Option<String> {
    (!id.is_empty()).then(|| id.to_string())
}

/// The row in `agent_id`'s transcript that durable identity `origin`
/// already produced, matched through `key`.
///
/// A reverse scan rather than a third index: the row a re-applied event
/// looks for sits at or near the tail (a usage row follows the message it
/// reports on, a notice follows the entry that raised it), so the walk
/// settles in a step or two on the live path, and a miss costs one pass
/// over a transcript small enough that views walk it entry by entry every
/// frame.
fn indexed_row(
    state: &ChatState,
    agent_id: AgentId,
    origin: &str,
    key: impl Fn(&EntryKind) -> Option<&str>,
) -> Option<EntryId> {
    state
        .transcript(agent_id)?
        .entries()
        .iter()
        .rev()
        .find(|entry| key(&entry.kind) == Some(origin))
        .map(|entry| entry.id)
}

/// Durable identity of a usage row: the assistant message it reports on.
fn usage_origin(kind: &EntryKind) -> Option<&str> {
    match kind {
        EntryKind::TurnUsage(row) => row.after_message_id.as_deref(),
        _ => None,
    }
}

/// Durable identity of a notice row: the log entry or message it derives
/// from.
fn notice_origin(kind: &EntryKind) -> Option<&str> {
    match kind {
        EntryKind::Notice(row) => row.entry.as_deref(),
        _ => None,
    }
}

/// Durable identity of a compaction row: its checkpoint log entry.
fn compaction_origin(kind: &EntryKind) -> Option<&str> {
    match kind {
        EntryKind::Compaction(row) => row.entry.as_deref(),
        _ => None,
    }
}

/// Mutable payload of entry `id` in `agent_id`'s transcript.
fn entry_kind_mut(state: &mut ChatState, agent_id: AgentId, id: EntryId) -> Option<&mut EntryKind> {
    state
        .transcripts
        .get_mut(&agent_id)
        .and_then(|t| t.get_mut(id))
        .map(|e| &mut e.kind)
}

/// Handle `MessageEnd { Assistant }`: finalize the in-flight entry (or
/// materialize it on the replay path), then surface any in-band error.
fn reduce_assistant_end(
    state: &mut ChatState,
    agent_id: AgentId,
    assistant: AssistantMessage,
    message_id: Option<String>,
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
    // Durable identity wins over the streaming slot: a re-applied
    // `MessageEnd` updates the entry it already produced. The streaming
    // slot is only consulted (and released) when this message is new to
    // us, so a re-application cannot steal another turn's in-flight
    // entry.
    let target = match indexed_message(state, agent_id, message_id.as_deref()) {
        Some(id) => Some(id),
        None => state
            .render
            .get_mut(&agent_id)
            .and_then(|r| r.current_assistant.take()),
    };
    // Defensive: a target that no longer resolves to an assistant entry
    // falls through to the append below, so a durable message always
    // lands somewhere renderable. Nothing produces that state today
    // (quiesce drops only unfinalized entries, and it clears the
    // streaming slot that names them in the same pass), so this is a
    // guard against a future index leak, not a live case.
    let target = target.filter(|&id| {
        matches!(
            state.transcript(agent_id).and_then(|t| t.get(id)),
            Some(entry) if matches!(entry.kind, EntryKind::Assistant(_))
        )
    });
    let mut changed = false;
    let mut landed = None;
    match target {
        Some(id) => {
            if let Some(EntryKind::Assistant(entry)) = entry_kind_mut(state, agent_id, id) {
                entry.message = assistant;
                entry.finalized = true;
                entry.message_id = message_id.clone();
                changed = true;
                landed = Some(id);
            }
        }
        None if has_renderable => {
            let id = state
                .transcripts
                .entry(agent_id)
                .or_default()
                .append(EntryKind::Assistant(AssistantEntry {
                    message_id: message_id.clone(),
                    message: assistant,
                    finalized: true,
                }));
            changed = true;
            landed = Some(id);
        }
        None => {}
    }
    if let Some(id) = landed {
        remember_message(state, agent_id, message_id.as_deref(), id);
    }
    // Recorded even when the message rendered no entry (a tool-use-only
    // turn): the trailing `UsageUpdate` still reports on this message
    // and keys its row off this id.
    state
        .render
        .entry(agent_id)
        .or_default()
        .last_finalized_assistant = message_id.clone();
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
    // its `MessageEnd`s, so the live path still refreshes, and a backfill
    // does the same through the re-synthesized `SubAgentStart` that opens
    // the run's bracket.
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
        // The notice derives from this assistant message, so that
        // message is its durable identity and a re-served suffix updates
        // the row it already produced. Keying it directly on the message
        // rather than through `message_index` is what covers the
        // content-less error shape, which renders no assistant entry at
        // all and so never enters that index.
        record_notice(
            state,
            agent_id,
            NoticeLevel::Error,
            line,
            message_id.as_deref(),
        );
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
    if !call_id.is_empty() {
        render.tool_index.insert(call_id, id);
    }
    // A tool call that arrives mid-turn means the assistant message
    // that emitted it is finished as far as the stream is concerned.
    // Drop the streaming target so post-tool assistant text opens a
    // fresh entry *after* the tool.
    render.current_assistant = None;
    id
}

/// Append a notice row, or update in place the row that `origin` already
/// produced.
///
/// `origin` is the durable identity the notice derives from: the settings
/// log entry behind a projected notice, the assistant message behind an
/// in-band error line. `None` is "no durable identity" and appends
/// unconditionally, which is what every locally raised notice carries.
fn record_notice(
    state: &mut ChatState,
    agent_id: AgentId,
    level: NoticeLevel,
    text: String,
    origin: Option<&str>,
) {
    if let Some(origin) = origin
        && let Some(id) = indexed_row(state, agent_id, origin, notice_origin)
        && let Some(EntryKind::Notice(row)) = entry_kind_mut(state, agent_id, id)
    {
        row.level = level;
        row.text = text;
        return;
    }
    state
        .transcripts
        .entry(agent_id)
        .or_default()
        .append(EntryKind::Notice(NoticeEntry {
            level,
            text,
            entry: origin.map(str::to_string),
        }));
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
    use crate::test_support::{
        CanonicalState, assert_no_dangling, build_test_agent, dangling_entry_ids,
        scripted_run_config,
    };

    fn main_settings() -> AgentSettings {
        AgentSettings {
            provider: "anthropic".into(),
            model_id: "claude-main".into(),
            thinking: "off".into(),
            thinking_display: "default".into(),
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
        let _ = reduce(state, life, event, None);
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
            thinking_display: "default".into(),
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
        tool_start_with_args(agent_id, call_id, tool, serde_json::json!({}))
    }

    fn tool_start_with_args(
        agent_id: AgentId,
        call_id: &str,
        tool: &str,
        args: Value,
    ) -> AgentEvent {
        AgentEvent::ToolExecutionStart {
            agent_id,
            call_id: call_id.into(),
            tool: tool.into(),
            args,
        }
    }

    /// A `ToolExecutionUpdate`: the lossy cumulative snapshot a running
    /// tool paints its cell with.
    fn tool_update(
        agent_id: AgentId,
        call_id: &str,
        tool: &str,
        partial: ToolDetails,
    ) -> AgentEvent {
        AgentEvent::ToolExecutionUpdate {
            agent_id,
            call_id: call_id.into(),
            tool: tool.into(),
            args: serde_json::json!({}),
            partial,
            content: Arc::from(Vec::<UserContent>::new()),
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
            EntryKind::User(u) => assert_eq!(u.message_id.as_deref(), Some(expected_id.as_str())),
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
    fn a_resynthesized_sub_agent_start_reopens_a_concluded_box() {
        // The state a client holds once a run of `Sub(1)` has finished: box
        // `Done`, carrying that run's report.
        let concluded = || {
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
            apply(&mut s, &mut life, sub_assistant_end(1, "first result"));
            apply(
                &mut s,
                &mut life,
                AgentEvent::SubAgentEnd {
                    parent: AgentId::Main,
                    child: AgentId::Sub(1),
                    report: "first result".into(),
                    conclusion: SubAgentConclusion::Completed,
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
            (s, life)
        };

        // A re-attach during a continuation: the backfill carries no
        // `AgentStart(Sub 1)` (nothing persists one), so the entry-less
        // start it synthesizes to bracket the continuation's entries is the
        // only signal that the run is under way.
        let (mut s, mut life) = concluded();
        let _ = reduce(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
            None,
        );
        {
            let b = s.sub_box_mut(1).expect("box");
            assert_eq!(
                b.status,
                SubAgentStatus::Running,
                "the glue marks the run it brackets in progress",
            );
            assert!(b.finished_at.is_none(), "and the runtime clock runs again");
        }
        // Which is what lets the bracketed entries refresh the report.
        apply(&mut s, &mut life, sub_assistant_end(1, "second result"));
        assert_eq!(
            s.sub_box_mut(1).expect("box").report.as_deref(),
            Some("second result"),
        );

        // A durable start names the spawn root of the run this box already
        // stands for, so re-serving it is a pure read.
        let (mut s, mut life) = concluded();
        let root = "e-spawn-root".to_string();
        let _ = reduce(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
            Some(&root),
        );
        let b = s.sub_box_mut(1).expect("box");
        assert_eq!(
            b.status,
            SubAgentStatus::Done,
            "a re-served spawn root does not resurrect a concluded box",
        );
        assert!(b.finished_at.is_some(), "its runtime clock stays frozen");
        assert_eq!(b.report.as_deref(), Some("first result"));
    }

    #[test]
    fn a_terminal_conclusion_is_not_reopened() {
        // Both re-open paths, a continuation's `AgentStart(Sub n)` and a
        // backfill's entry-less `SubAgentStart`, are refused by a box whose
        // conclusion is terminal. Re-opening one would let the next
        // `AgentEnd(Sub n)` conclude it `Done` and rewrite a failure into a
        // success.
        for conclusion in [SubAgentConclusion::Failed, SubAgentConclusion::Truncated] {
            for reopening in [
                AgentEvent::AgentStart {
                    agent_id: AgentId::Sub(1),
                },
                sub_agent_start(1, "scripted", "scripted"),
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
                    AgentEvent::SubAgentEnd {
                        parent: AgentId::Main,
                        child: AgentId::Sub(1),
                        report: "how it ended".into(),
                        conclusion,
                    },
                );
                let concluded = s.sub_box_mut(1).expect("box").status;

                apply(&mut s, &mut life, reopening);
                apply(&mut s, &mut life, sub_assistant_end(1, "a later line"));
                apply(
                    &mut s,
                    &mut life,
                    AgentEvent::AgentEnd {
                        agent_id: AgentId::Sub(1),
                        messages: Vec::new(),
                    },
                );

                let b = s.sub_box_mut(1).expect("box");
                assert_eq!(b.status, concluded, "the conclusion stands");
                assert_eq!(
                    b.report.as_deref(),
                    Some("how it ended"),
                    "and so does the report it came with",
                );
            }
        }
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

        // The owning turn ends. A background task outlives it, so its
        // output still has to route into the launch cell.
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
        // lands only after the owner's turn is over. The badge and the
        // live tail have to keep working across that boundary.
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

        // The cell got its task linkage.
        match &entries(&s, AgentId::Main)[0].kind {
            EntryKind::Tool(t) => assert_eq!(t.task, Some(1), "cell carries the task badge"),
            other => panic!("unexpected kind: {other:?}"),
        }
        // The linkage is the surviving `tool_index` entry, not a
        // snapshot: the index is what outlives the turn.
        assert_eq!(
            s.task_cell(1),
            s.render
                .get(&AgentId::Main)
                .and_then(|r| r.tool_index.get("c1").copied())
                .map(|cell| (AgentId::Main, cell)),
            "the task routes through the owner's tool index",
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
        assert!(s.tasks().contains_key(&1), "the task is tracked");
        assert_eq!(s.task_cell(1), None, "agent tasks have no launch cell");
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
        assert_eq!(sub.current_assistant, None, "streaming is per-turn");
        assert_eq!(
            sub.tool_index.len(),
            1,
            "the durable-identity index outlives the turn",
        );
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
            None,
        );
        assert!(redraw.0, "QueueUpdate is a redraw ping");
        assert!(entries(&s, AgentId::Main).is_empty(), "no entry appended");

        let redraw = reduce(
            &mut s,
            &mut life,
            AgentEvent::TurnStart {
                agent_id: AgentId::Main,
            },
            None,
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
                thinking_display: "default".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            200_000,
            Arc::new(Vec::new()),
        );
        let mut life = AgentLifecycle::default();
        for event in recorded.lock().unwrap().drain(..) {
            let _ = reduce(&mut s, &mut life, event, None);
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

    // ---- Idempotent re-application (spec 6.5) ---------------------------
    //
    // A re-attach backfill re-projects entries the client already saw, so
    // applying a projected event twice has to leave the same state. The
    // canonical form is the oracle: it is what the phase-2 equivalence
    // harness compares, so a difference these tests miss is one that
    // harness misses too.

    fn canon(state: &ChatState, life: &AgentLifecycle) -> CanonicalState {
        CanonicalState::of_reduced(state, life)
    }

    /// The single tool cell in `id`'s transcript.
    fn only_tool(state: &ChatState, id: AgentId) -> &ToolEntry {
        let tools: Vec<&ToolEntry> = entries(state, id)
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Tool(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 1, "expected exactly one tool cell");
        tools[0]
    }

    #[test]
    fn reapplied_tool_start_updates_the_cell_in_place() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let start = tool_start(AgentId::Main, "c1", "bash");
        apply(&mut s, &mut life, start.clone());
        let before = canon(&s, &life);

        apply(&mut s, &mut life, start);

        assert_eq!(canon(&s, &life), before, "re-applied start changed state");
        let cell = only_tool(&s, AgentId::Main);
        assert_eq!(cell.call_id, "c1");
        assert_eq!(
            cell.status,
            ToolStatus::Running,
            "the cell the index names is the one that is still running",
        );
        assert_no_dangling(&s);
    }

    #[test]
    fn reapplied_tool_end_after_agent_end_does_not_duplicate_the_cell() {
        // The turn that ran the tool is over. A backfill re-projects the
        // whole bracket, and the cell it belongs to has to be found even
        // though nothing about the call is in flight any more.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let start = tool_start(AgentId::Main, "c1", "bash");
        let end = tool_end(
            AgentId::Main,
            "c1",
            "bash",
            bash_task_details("output", None),
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        apply(&mut s, &mut life, start.clone());
        apply(&mut s, &mut life, end.clone());
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );
        let before = canon(&s, &life);

        apply(&mut s, &mut life, start);
        apply(&mut s, &mut life, end);

        assert_eq!(canon(&s, &life), before, "re-applied bracket changed state");
        assert_eq!(
            only_tool(&s, AgentId::Main).status,
            ToolStatus::Done { is_error: false },
        );
        assert_no_dangling(&s);
    }

    #[test]
    fn reapplied_user_message_end_updates_the_row_in_place() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let user = user_message_end("hello");
        apply(&mut s, &mut life, user.clone());
        let before = canon(&s, &life);

        apply(&mut s, &mut life, user);

        assert_eq!(canon(&s, &life), before);
        assert_eq!(
            count_kind(&s, AgentId::Main, |k| matches!(k, EntryKind::User(_))),
            1
        );
        assert_no_dangling(&s);
    }

    #[test]
    fn reapplied_assistant_message_end_updates_the_row_in_place() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        // The live shape: streaming opens the entry, `MessageEnd`
        // finalizes it and stamps its durable id.
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "answer".into(),
                partial: text_partial("answer"),
            }),
        );
        let assistant = assistant_message_end(text_partial("answer"));
        apply(&mut s, &mut life, assistant.clone());
        let before = canon(&s, &life);

        apply(&mut s, &mut life, assistant);

        assert_eq!(canon(&s, &life), before);
        let assistants: Vec<&AssistantEntry> = entries(&s, AgentId::Main)
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Assistant(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(assistants.len(), 1, "one row for one message");
        assert!(assistants[0].finalized);
        assert!(
            assistants[0].message_id.is_some(),
            "the finalized row carries its durable id",
        );
        assert_no_dangling(&s);
    }

    #[test]
    fn reapplied_task_notification_end_updates_the_row_in_place() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let notice = task_notification_end("cargo build", TaskOutcome::Succeeded);
        apply(&mut s, &mut life, notice.clone());
        let before = canon(&s, &life);

        apply(&mut s, &mut life, notice);

        assert_eq!(canon(&s, &life), before);
        assert_eq!(
            count_kind(&s, AgentId::Main, |k| matches!(
                k,
                EntryKind::TaskNotification(_)
            )),
            1,
        );
        assert_no_dangling(&s);
    }

    #[test]
    fn reapplied_usage_update_overwrites_the_row_for_its_message() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let assistant = assistant_message_end(text_partial("answer"));
        apply(&mut s, &mut life, assistant.clone());
        apply(
            &mut s,
            &mut life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: token_usage([100, 10, 0, 0]),
            },
        );

        // The re-projected suffix carries the same message and its
        // trailing usage. The numbers stand in for a projection whose
        // accumulators read differently: the last application wins.
        apply(&mut s, &mut life, assistant);
        apply(
            &mut s,
            &mut life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: token_usage([200, 20, 0, 0]),
            },
        );

        let rows: Vec<&TurnUsageEntry> = entries(&s, AgentId::Main)
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::TurnUsage(u) => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), 1, "one usage row for one message");
        assert_eq!(rows[0].usage.turn_input, 200, "the later value wins");
        assert_eq!(rows[0].usage.turn_output, 20);
        assert_eq!(
            s.footers().context_usage(AgentId::Main).tokens,
            Some(200),
            "the footer follows the row",
        );
        assert_no_dangling(&s);
    }

    #[test]
    fn resynthesized_sub_agent_start_reuses_the_box_and_keeps_its_footer() {
        // A suffix whose sub-agent thread is open at the cursor boundary
        // gets its `SubAgentStart` re-synthesized from the run's
        // remembered task, run mode and settings. The box and the
        // footer it seeded have to survive that.
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
        let start = sub_agent_start(1, "openai", "gpt-sub");
        apply(&mut s, &mut life, start.clone());
        apply(
            &mut s,
            &mut life,
            AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        apply(&mut s, &mut life, sub_assistant_end(1, "working on it"));
        apply(
            &mut s,
            &mut life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Sub(1),
                usage: token_usage([1_000, 10, 0, 0]),
            },
        );
        let before = canon(&s, &life);
        let box_id = s.sub_boxes.get(&1).copied().expect("sub box recorded");

        apply(&mut s, &mut life, start);

        assert_eq!(
            canon(&s, &life),
            before,
            "re-synthesized start changed state"
        );
        assert_eq!(
            s.sub_boxes.get(&1).copied(),
            Some(box_id),
            "the box is reused, not re-appended",
        );
        assert_eq!(
            s.footers().model_line(AgentId::Sub(1)).as_deref(),
            Some("gpt-sub off"),
            "the sub keeps its model line",
        );
        assert_eq!(
            s.footers().context_usage(AgentId::Sub(1)),
            crate::footer::ContextUsage {
                tokens: Some(1_000),
                context_window: 400_000,
            },
            "and its occupancy accounting",
        );
        assert_no_dangling(&s);
    }

    /// Stamp a fixed durable id on a `MessageEnd`, so a test can re-apply
    /// the same message with a different payload. Live messages mint a
    /// fresh id each time, which is exactly what a re-served suffix does
    /// not do.
    fn with_id(event: AgentEvent, id: &str) -> AgentEvent {
        let AgentEvent::MessageEnd {
            agent_id,
            mut message,
        } = event
        else {
            panic!("with_id expects a MessageEnd");
        };
        message.set_id(id.to_string());
        AgentEvent::MessageEnd { agent_id, message }
    }

    /// An assistant `MessageEnd` whose only content is a tool call: a
    /// tool-use-only turn, which renders no assistant entry.
    fn tool_use_only_end(call_id: &str) -> AgentEvent {
        assistant_message_end(partial_with(vec![AssistantContent::ToolCall(ToolCall {
            id: call_id.into(),
            name: "todo_read".into(),
            arguments: serde_json::json!({}),
        })]))
    }

    fn compaction_end(summary: &str, tokens_after: u64) -> AgentEvent {
        AgentEvent::CompactionEnd {
            agent_id: AgentId::Main,
            reason: CompactionReason::Manual,
            tokens_before: 1_000,
            tokens_after,
            summary: Some(summary.to_string()),
            error: None,
        }
    }

    fn notices(state: &ChatState, id: AgentId) -> Vec<(NoticeLevel, String)> {
        entries(state, id)
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Notice(n) => Some((n.level, n.text.clone())),
                _ => None,
            })
            .collect()
    }

    fn usage_rows(state: &ChatState, id: AgentId) -> Vec<&TurnUsageEntry> {
        entries(state, id)
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::TurnUsage(u) => Some(u),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_reapplied_tool_start_alone_does_not_unfinish_a_done_cell() {
        // The suffix re-projects the whole bracket, but a client can see
        // the start again with the end still to come (the two are
        // separate frames). The start refreshes tool and args only.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let start = tool_start_with_args(
            AgentId::Main,
            "c1",
            "bash",
            serde_json::json!({"command": "ls"}),
        );
        apply(&mut s, &mut life, start.clone());
        apply(
            &mut s,
            &mut life,
            tool_end(
                AgentId::Main,
                "c1",
                "bash",
                bash_task_details("the result", None),
            ),
        );
        let before = canon(&s, &life);

        apply(&mut s, &mut life, start);

        assert_eq!(canon(&s, &life), before, "the start alone changed state");
        let cell = only_tool(&s, AgentId::Main);
        assert_eq!(
            cell.status,
            ToolStatus::Done { is_error: false },
            "a re-served start must not un-finish the call",
        );
        assert!(cell.details.is_some(), "and must not drop its result");
    }

    #[test]
    fn a_reapplied_message_end_with_a_changed_payload_updates_the_row() {
        // The suffix can carry a payload that differs from what the live
        // frame carried (a projection reads the persisted form), so the
        // row has to be rewritten, not merely left alone.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            with_id(user_message_end("hello"), "m-user"),
        );
        apply(
            &mut s,
            &mut life,
            with_id(assistant_message_end(text_partial("first")), "m-assistant"),
        );
        apply(
            &mut s,
            &mut life,
            with_id(
                task_notification_end("cargo build", TaskOutcome::Succeeded),
                "m-notice",
            ),
        );

        apply(
            &mut s,
            &mut life,
            with_id(user_message_end("hello, edited"), "m-user"),
        );
        apply(
            &mut s,
            &mut life,
            with_id(
                assistant_message_end(text_partial("revised")),
                "m-assistant",
            ),
        );
        apply(
            &mut s,
            &mut life,
            with_id(
                task_notification_end("cargo test", TaskOutcome::Killed),
                "m-notice",
            ),
        );

        let rows = entries(&s, AgentId::Main);
        assert_eq!(rows.len(), 3, "three messages, three rows");
        match &rows[0].kind {
            EntryKind::User(u) => assert_eq!(u.joined_text(), "hello, edited"),
            other => panic!("unexpected kind: {other:?}"),
        }
        assert_eq!(assistant_text(&rows[1]), "revised");
        match &rows[2].kind {
            EntryKind::TaskNotification(n) => {
                assert_eq!(n.label, "cargo test");
                assert_eq!(n.outcome, TaskOutcome::Killed);
                assert_eq!(n.body, "killed");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        assert_no_dangling(&s);
    }

    #[test]
    fn a_tool_use_only_turn_still_anchors_its_usage_row() {
        // The message renders no entry, so nothing but
        // `last_finalized_assistant` can key its trailing usage row. A
        // re-served entry has to overwrite that row rather than add one.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let message = with_id(tool_use_only_end("c1"), "m-tools");
        apply(&mut s, &mut life, message.clone());
        assert_eq!(
            s.render
                .get(&AgentId::Main)
                .and_then(|r| r.last_finalized_assistant.as_deref()),
            Some("m-tools"),
            "the anchor is recorded even with no row to show",
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: token_usage([100, 10, 0, 0]),
            },
        );

        apply(&mut s, &mut life, message);
        apply(
            &mut s,
            &mut life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: token_usage([200, 20, 0, 0]),
            },
        );

        let rows = usage_rows(&s, AgentId::Main);
        assert_eq!(rows.len(), 1, "one usage row for one message");
        assert_eq!(rows[0].after_message_id.as_deref(), Some("m-tools"));
        assert_eq!(rows[0].usage.turn_input, 200, "the later value wins");
    }

    #[test]
    fn a_late_tool_update_cannot_repaint_a_concluded_cell() {
        // A cumulative snapshot is lossy, so correctness never depends on
        // one (spec 6.4). One that arrives after the call's authoritative
        // result must therefore be dropped, not painted.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let authoritative = ToolDetails::Text {
            summary: "todo_read".into(),
            body: "AUTHORITATIVE RESULT".into(),
        };
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Main, "c1", "todo_read"),
        );
        apply(
            &mut s,
            &mut life,
            tool_end(AgentId::Main, "c1", "todo_read", authoritative.clone()),
        );
        let before = canon(&s, &life);

        let redraw = reduce(
            &mut s,
            &mut life,
            tool_update(
                AgentId::Main,
                "c1",
                "todo_read",
                ToolDetails::Text {
                    summary: "todo_read".into(),
                    body: "stale partial".into(),
                },
            ),
            None,
        );

        assert!(!redraw.0, "a dropped snapshot is not a redraw");
        assert_eq!(canon(&s, &life), before, "the stale snapshot landed");
        let cell = only_tool(&s, AgentId::Main);
        assert_eq!(cell.status, ToolStatus::Done { is_error: false });
        assert_eq!(
            cell.details
                .as_ref()
                .map(|d| serde_json::to_value(d).unwrap()),
            Some(serde_json::to_value(&authoritative).unwrap()),
            "the result stands",
        );
    }

    #[test]
    fn a_reprojected_background_launch_keeps_the_live_task_output() {
        // A background launch's cell is `Done` the moment the spawn
        // returns, and its body then belongs to the task's snapshots. A
        // re-projected bracket must not blank what the task streamed.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let launch = tool_end(AgentId::Main, "c1", "bash", bash_task_details("", Some(1)));
        apply(&mut s, &mut life, tool_start(AgentId::Main, "c1", "bash"));
        apply(&mut s, &mut life, launch.clone());
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
        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskOutput {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c1".into(),
                partial: bash_task_details("half way there\n", Some(1)),
            },
        );

        // The re-attach suffix re-projects the launch bracket.
        apply(&mut s, &mut life, tool_start(AgentId::Main, "c1", "bash"));
        apply(&mut s, &mut life, launch);

        match only_tool(&s, AgentId::Main)
            .details
            .as_ref()
            .expect("details")
        {
            ToolDetails::Bash { stdout, .. } => {
                assert_eq!(stdout, "half way there\n", "the live tail survives")
            }
            other => panic!("expected bash details, got {other:?}"),
        }
    }

    #[test]
    fn a_background_launch_s_wire_content_lands_whichever_order_task_start_arrives_in() {
        // The launching tool's driver races its own return, so `TaskStart`
        // lands on either side of the launch's `ToolExecutionEnd`. The
        // structured body is contested (the task's snapshots own it) but the
        // wire content is not, so the cell must read the same either way.
        let launch = AgentEvent::ToolExecutionEnd {
            agent_id: AgentId::Main,
            call_id: "c1".into(),
            tool: "bash".into(),
            result: bash_task_details("", Some(1)),
            content: Arc::from(vec![UserContent::text("Started background task #1")]),
            is_error: false,
        };
        let start = AgentEvent::TaskStart {
            agent_id: AgentId::Main,
            task_id: 1,
            call_id: "c1".into(),
            kind: TaskKind::Bash {
                command: "sleep 5".into(),
            },
            label: "sleep 5".into(),
        };

        let content_of = |events: Vec<AgentEvent>| {
            let mut s = state();
            let mut life = AgentLifecycle::default();
            apply(&mut s, &mut life, tool_start(AgentId::Main, "c1", "bash"));
            for event in events {
                apply(&mut s, &mut life, event);
            }
            joined_user_text(&only_tool(&s, AgentId::Main).content)
        };

        assert_eq!(
            content_of(vec![start.clone(), launch.clone()]),
            content_of(vec![launch, start]),
        );
    }

    #[test]
    fn a_task_start_cannot_paint_another_agents_cell_with_the_same_call_id() {
        // Call ids are unique per provider run, not per session, so two
        // agents can hold the same one, and an `EntryId` is a per
        // transcript counter, so the same id names an unrelated entry in
        // another transcript. A task's launch cell therefore has to be
        // resolved in the owner's own transcript.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );
        // The sub's own first row, so its "dup" cell lands on the same
        // entry id as Main's unrelated `read_file` cell below.
        apply(&mut s, &mut life, sub_assistant_end(1, "on it"));
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Sub(1), "dup", "bash"),
        );
        apply(
            &mut s,
            &mut life,
            tool_end(
                AgentId::Sub(1),
                "dup",
                "bash",
                bash_task_details("", Some(1)),
            ),
        );
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Main, "c-read", "read_file"),
        );
        apply(
            &mut s,
            &mut life,
            tool_end(
                AgentId::Main,
                "c-read",
                "read_file",
                ToolDetails::Text {
                    summary: "read_file".into(),
                    body: "file contents".into(),
                },
            ),
        );
        // The hazard only exists while the two ids collide, so the setup
        // asserts that it does.
        let sub_cell = s
            .render
            .get(&AgentId::Sub(1))
            .and_then(|r| r.tool_index.get("dup").copied())
            .expect("the sub's launch cell");
        let main_read = s
            .render
            .get(&AgentId::Main)
            .and_then(|r| r.tool_index.get("c-read").copied())
            .expect("main's read cell");
        assert_eq!(
            sub_cell, main_read,
            "the two transcripts really do share this entry id",
        );

        // Main launches nothing under "dup", so its task resolves no cell.
        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskStart {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "dup".into(),
                kind: TaskKind::Bash {
                    command: "sleep 5".into(),
                },
                label: "sleep 5".into(),
            },
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskOutput {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "dup".into(),
                partial: bash_task_details("TASK TAIL", Some(1)),
            },
        );

        assert_eq!(s.task_cell(1), None, "no cell in Main's transcript");
        let main_cell = only_tool(&s, AgentId::Main);
        assert_eq!(main_cell.tool, "read_file");
        assert_eq!(main_cell.task, None, "the unrelated cell keeps no badge");
        match main_cell.details.as_ref().expect("details") {
            ToolDetails::Text { body, .. } => assert_eq!(body, "file contents"),
            other => panic!("expected text details, got {other:?}"),
        }
    }

    #[test]
    fn a_reapplied_task_start_does_not_resurrect_a_finished_task() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let start = AgentEvent::TaskStart {
            agent_id: AgentId::Main,
            task_id: 1,
            call_id: "c1".into(),
            kind: TaskKind::Bash {
                command: "sleep 5".into(),
            },
            label: "sleep 5".into(),
        };
        apply(&mut s, &mut life, start.clone());
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
        let (started_at, finished_at) = {
            let info = s.tasks().get(&1).expect("tracked task");
            (info.started_at, info.finished_at)
        };

        apply(&mut s, &mut life, start);

        let info = s.tasks().get(&1).expect("tracked task");
        assert_eq!(
            info.status,
            TaskStatus::Exited(Some(0)),
            "a re-applied start must not un-finish the task",
        );
        assert_eq!(info.finished_at, finished_at, "nor unfreeze its runtime");
        assert_eq!(info.started_at, started_at, "nor restart its clock");
    }

    #[test]
    fn a_redelivered_agent_bracket_does_not_rewrite_a_terminal_sub_conclusion() {
        // `AgentStart(Sub n)` re-opens a box for a continuation re-run,
        // which is a `Done` box. Re-opening a failed one would let the
        // paired `AgentEnd` conclude it `Done` and lose the failure.
        for (conclusion, expected) in [
            (SubAgentConclusion::Failed, SubAgentStatus::Failed),
            (SubAgentConclusion::Truncated, SubAgentStatus::Truncated),
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
                AgentEvent::SubAgentEnd {
                    parent: AgentId::Main,
                    child: AgentId::Sub(1),
                    report: "it broke".to_string(),
                    conclusion,
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
            let before = canon(&s, &life);

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

            let b = s.sub_box_mut(1).expect("the box");
            assert_eq!(b.status, expected, "a terminal conclusion is terminal");
            assert_eq!(b.report.as_deref(), Some("it broke"));
            assert!(b.finished_at.is_some(), "and stays frozen");
            assert_eq!(
                canon(&s, &life),
                before,
                "the re-delivered bracket changed state",
            );
        }
    }

    #[test]
    fn an_empty_call_id_does_not_collapse_tool_cells() {
        // The OpenAI adapter builds a `ToolCall` with an empty id and
        // fills it only when the wire delta carries one. The index now
        // outlives the turn, so keying on an empty id would make every
        // id-less call in the session share one cell. The bracket of one
        // such call still has to correlate, which it does through the
        // cell that is still running.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        for tool in ["bash", "read_file"] {
            apply(&mut s, &mut life, tool_start(AgentId::Main, "", tool));
            apply(
                &mut s,
                &mut life,
                tool_end(
                    AgentId::Main,
                    "",
                    tool,
                    ToolDetails::Text {
                        summary: tool.into(),
                        body: format!("{tool} output"),
                    },
                ),
            );
        }

        let cells: Vec<&ToolEntry> = entries(&s, AgentId::Main)
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Tool(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(cells.len(), 2, "one cell per call");
        assert_eq!(cells[0].tool, "bash");
        assert_eq!(cells[1].tool, "read_file");
        assert!(
            s.render
                .get(&AgentId::Main)
                .is_none_or(|r| r.tool_index.is_empty()),
            "an id with no identity is not indexed",
        );
        assert_no_dangling(&s);
    }

    #[test]
    fn a_reserved_compaction_checkpoint_updates_its_row() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let entry = "e-checkpoint".to_string();
        let _ = reduce(
            &mut s,
            &mut life,
            compaction_end("the summary", 400),
            Some(&entry),
        );
        let _ = reduce(
            &mut s,
            &mut life,
            compaction_end("the summary, reprojected", 400),
            Some(&entry),
        );

        let rows: Vec<&CompactionEntry> = entries(&s, AgentId::Main)
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Compaction(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), 1, "one row per checkpoint entry");
        assert_eq!(rows[0].summary, "the summary, reprojected");
        assert_eq!(rows[0].entry.as_deref(), Some("e-checkpoint"));

        // A compaction with no durable identity (nothing re-serves it)
        // keeps appending.
        let _ = reduce(&mut s, &mut life, compaction_end("local", 300), None);
        let _ = reduce(&mut s, &mut life, compaction_end("local", 300), None);
        assert_eq!(
            count_kind(&s, AgentId::Main, |k| matches!(k, EntryKind::Compaction(_))),
            3,
        );
    }

    #[test]
    fn a_reserved_settings_notice_updates_its_row() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let entry = "e-model-change".to_string();
        let notice = |text: &str| AgentEvent::Notice {
            agent_id: AgentId::Main,
            text: text.to_string(),
        };
        let _ = reduce(
            &mut s,
            &mut life,
            notice("Model set to openai/gpt-5."),
            Some(&entry),
        );
        // The re-served text stands in for a projection that renders the
        // same entry differently: the row is rewritten, not just left
        // undisturbed.
        let _ = reduce(
            &mut s,
            &mut life,
            notice("Model set to openai/gpt-5 (reprojected)."),
            Some(&entry),
        );
        assert_eq!(
            notices(&s, AgentId::Main),
            vec![(
                NoticeLevel::Info,
                "Model set to openai/gpt-5 (reprojected).".to_string()
            )],
            "one row per settings entry, updated in place",
        );

        // A notice with no durable origin still appends: every locally
        // raised one is a distinct line.
        let _ = reduce(&mut s, &mut life, notice("Restored settings."), None);
        let _ = reduce(&mut s, &mut life, notice("Restored settings."), None);
        assert_eq!(notices(&s, AgentId::Main).len(), 3);
    }

    #[test]
    fn a_reapplied_errored_assistant_end_does_not_duplicate_its_error_notice() {
        // Two shapes: one that renders an assistant row (so the message is
        // in `message_index`) and one that renders none, which is why the
        // notice keys on the message rather than on its row.
        for content in [Vec::new(), text_partial("partial answer").content] {
            let mut s = state();
            let mut life = AgentLifecycle::default();
            let failed = |text: &str| {
                let mut message = partial_with(content.clone());
                message.stop_reason = StopReason::Error;
                message.error = Some(AssistantError::new(ErrorCategory::InvalidRequest, text));
                with_id(assistant_message_end(message), "m-failed")
            };

            apply(&mut s, &mut life, failed("boom"));
            let once = canon(&s, &life);
            apply(&mut s, &mut life, failed("boom"));
            assert_eq!(canon(&s, &life), once, "the re-application drifted");

            // A re-served error line is rewritten rather than duplicated.
            apply(&mut s, &mut life, failed("boom, reprojected"));
            assert_eq!(
                notices(&s, AgentId::Main),
                vec![(NoticeLevel::Error, "Error: boom, reprojected".to_string())],
                "one notice per failed message (content blocks: {})",
                content.len(),
            );
            assert_no_dangling(&s);
        }
    }

    // ---- Quiesce (spec 6.5's re-attach reconciliation) ------------------

    #[test]
    fn quiesce_drops_the_streaming_assistant_entry_and_keeps_finalized_ones() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "first".into(),
                partial: text_partial("first"),
            }),
        );
        apply(
            &mut s,
            &mut life,
            assistant_message_end(text_partial("first")),
        );
        // A second turn is mid-stream when the connection drops.
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "half".into(),
                partial: text_partial("half"),
            }),
        );
        assert_eq!(
            count_kind(&s, AgentId::Main, |k| matches!(k, EntryKind::Assistant(_))),
            2,
        );

        s.quiesce(&mut life);

        let assistants: Vec<&AssistantEntry> = entries(&s, AgentId::Main)
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Assistant(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(assistants.len(), 1, "only the finalized row survives");
        assert_eq!(assistant_text(&entries(&s, AgentId::Main)[0]), "first");
        assert_eq!(
            s.render
                .get(&AgentId::Main)
                .and_then(|r| r.current_assistant),
            None,
        );
        assert_no_dangling(&s);
    }

    #[test]
    fn quiesce_keeps_a_running_tool_cell_and_clears_only_its_partial_result() {
        // A tool that has not finished has no log entry, so no backfill
        // can regenerate its cell: dropping it would lose the call's
        // arguments for good. Only the partial result the lossy
        // `ToolExecutionUpdate` painted goes.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Main, "c-done", "bash"),
        );
        apply(
            &mut s,
            &mut life,
            tool_end(
                AgentId::Main,
                "c-done",
                "bash",
                bash_task_details("finished", None),
            ),
        );
        apply(
            &mut s,
            &mut life,
            tool_start_with_args(
                AgentId::Main,
                "c-running",
                "bash",
                serde_json::json!({"command": "sleep 5"}),
            ),
        );
        apply(
            &mut s,
            &mut life,
            tool_update(
                AgentId::Main,
                "c-running",
                "bash",
                bash_task_details("half way there\n", None),
            ),
        );

        s.quiesce(&mut life);

        let cells: Vec<&ToolEntry> = entries(&s, AgentId::Main)
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Tool(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(cells.len(), 2, "both cells survive");
        assert_eq!(cells[1].call_id, "c-running");
        assert_eq!(cells[1].status, ToolStatus::Running);
        assert_eq!(
            cells[1].args,
            serde_json::json!({"command": "sleep 5"}),
            "the running call keeps its arguments",
        );
        assert!(
            cells[1].details.is_none(),
            "and loses the partial result a snapshot painted",
        );
        let render = s.render.get(&AgentId::Main).expect("main render");
        assert!(
            render.tool_index.contains_key("c-running"),
            "the surviving cell keeps its index entry",
        );
        assert!(render.tool_index.contains_key("c-done"));
        assert_no_dangling(&s);

        // The call concludes live, into the cell that was kept.
        apply(
            &mut s,
            &mut life,
            tool_end(
                AgentId::Main,
                "c-running",
                "bash",
                bash_task_details("late result", None),
            ),
        );
        let rebuilt: Vec<&ToolEntry> = entries(&s, AgentId::Main)
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Tool(t) if t.call_id == "c-running" => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(rebuilt.len(), 1, "exactly one cell for the call");
        assert_eq!(rebuilt[0].status, ToolStatus::Done { is_error: false });
        assert_eq!(
            rebuilt[0].args,
            serde_json::json!({"command": "sleep 5"}),
            "still with its arguments",
        );
        assert_no_dangling(&s);
    }

    #[test]
    fn quiesce_clears_sub_box_activity_but_keeps_it_running() {
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
        apply(&mut s, &mut life, sub_assistant_end(1, "still going"));
        {
            let b = s.sub_box_mut(1).expect("box");
            assert_eq!(b.latest_activity.as_deref(), Some("still going"));
        }
        let sub_entries = entries(&s, AgentId::Sub(1)).len();
        assert!(sub_entries > 0, "the sub built a transcript");

        s.quiesce(&mut life);

        let b = s.sub_box_mut(1).expect("the box survives");
        assert_eq!(b.status, SubAgentStatus::Running, "the host concludes it");
        assert_eq!(b.latest_activity, None, "transient detail is dropped");
        assert_eq!(b.report.as_deref(), Some("still going"), "the report stays");
        assert_eq!(
            entries(&s, AgentId::Sub(1)).len(),
            sub_entries,
            "the child transcript is untouched",
        );
        assert!(
            life.is_running(AgentId::Sub(1)),
            "quiesce does not touch the running set",
        );
        assert_no_dangling(&s);
    }

    #[test]
    fn quiesce_keeps_the_last_finalized_assistant_anchor() {
        // The anchor is what the trailing `UsageUpdate` of a re-served
        // entry keys its row on, and the cursor invariant drops that
        // entry's own durable frame, so clearing the anchor here would
        // grow a second usage row on every re-attach.
        let mut s = state();
        let mut life = AgentLifecycle::default();
        let message = with_id(assistant_message_end(text_partial("answer")), "m-answer");
        apply(&mut s, &mut life, message);
        apply(
            &mut s,
            &mut life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: token_usage([100, 10, 0, 0]),
            },
        );

        s.quiesce(&mut life);

        assert_eq!(
            s.render
                .get(&AgentId::Main)
                .and_then(|r| r.last_finalized_assistant.as_deref()),
            Some("m-answer"),
            "quiesce keeps durable identity",
        );
        // The suffix re-serves that entry. Its durable frame is dropped as
        // a duplicate, so the trailing usage update is all that lands, and
        // the anchor is the only thing that can route it to its row.
        apply(
            &mut s,
            &mut life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: token_usage([200, 20, 0, 0]),
            },
        );
        let rows = usage_rows(&s, AgentId::Main);
        assert_eq!(rows.len(), 1, "one usage row for one message");
        assert_eq!(rows[0].after_message_id.as_deref(), Some("m-answer"));
        assert_eq!(rows[0].usage.turn_input, 200);
    }

    #[test]
    fn quiesce_clears_the_compaction_indicator() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            AgentEvent::CompactionStart {
                agent_id: AgentId::Main,
                reason: CompactionReason::Manual,
            },
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::CompactionProgress {
                agent_id: AgentId::Main,
                reason: CompactionReason::Manual,
                phase: CompactionPhase::Saving,
            },
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::CompactionStart {
                agent_id: AgentId::Sub(1),
                reason: CompactionReason::Threshold,
            },
        );

        s.quiesce(&mut life);

        assert_eq!(s.compaction_phase(AgentId::Main), None);
        assert!(!life.is_compacting(AgentId::Main));
        assert!(
            !life.is_compacting(AgentId::Sub(1)),
            "every agent's mark is cleared, not just the viewed one",
        );
        assert!(life.compacting_agents().is_empty());
    }

    #[test]
    fn quiesce_leaves_no_dangling_entry_ids() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        // A sub-agent box, a finalized message, and a running bash cell a
        // background task routes through: every index that names an entry
        // has to still resolve afterwards.
        apply(
            &mut s,
            &mut life,
            sub_agent_start(1, "scripted", "scripted"),
        );
        apply(&mut s, &mut life, user_message_end("run something"));
        apply(
            &mut s,
            &mut life,
            tool_start(AgentId::Main, "c-bash", "bash"),
        );
        apply(
            &mut s,
            &mut life,
            AgentEvent::TaskStart {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "c-bash".into(),
                kind: TaskKind::Bash {
                    command: "sleep 5".into(),
                },
                label: "sleep 5".into(),
            },
        );
        // A second turn is mid-stream when the connection drops, so
        // quiesce has an entry to actually drop.
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "half".into(),
                partial: text_partial("half"),
            }),
        );
        let cell = s.task_cell(1).expect("the launch cell");

        s.quiesce(&mut life);

        assert_eq!(
            s.task_cell(1),
            Some(cell),
            "the running launch cell, and the task's route to it, survive",
        );
        assert_no_dangling(&s);
    }

    /// The dangling check is only worth running if it can fail. Nothing
    /// the reducer does produces a dangling id (which is why quiesce needs
    /// no index pruning), so the state is built by hand: an unfinalized
    /// assistant entry recorded in `message_index`, which quiesce then
    /// drops.
    #[test]
    fn the_dangling_check_fires_on_a_stale_index_entry() {
        let mut s = state();
        let mut life = AgentLifecycle::default();
        apply(
            &mut s,
            &mut life,
            message_update(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "half".into(),
                partial: text_partial("half"),
            }),
        );
        let streaming = s
            .render
            .get(&AgentId::Main)
            .and_then(|r| r.current_assistant)
            .expect("the streaming entry");
        s.render
            .entry(AgentId::Main)
            .or_default()
            .message_index
            .insert("m-never-finalized".to_string(), streaming);
        assert!(
            dangling_entry_ids(&s).is_empty(),
            "the index resolves before the entry goes"
        );

        s.quiesce(&mut life);

        assert_eq!(
            dangling_entry_ids(&s),
            vec!["message_index[m-never-finalized] for Main dangles".to_string()],
            "the checker names the stale index entry",
        );
    }

    // ---- The canonical form itself --------------------------------------

    #[test]
    fn canonical_form_ignores_instants_display_flags_and_active_view() {
        // Both states fold the same events, so their durable content
        // matches while every wall-clock stamp differs.
        let events = vec![
            sub_agent_start(1, "scripted", "scripted"),
            AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
            sub_assistant_end(1, "done here"),
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                report: "done here".to_string(),
                conclusion: SubAgentConclusion::Completed,
            },
            user_message_end("hello"),
            assistant_message_end(text_partial("hi")),
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: token_usage([10, 1, 0, 0]),
            },
        ];
        let fold = |events: Vec<AgentEvent>| {
            let mut s = state();
            let mut life = AgentLifecycle::default();
            for event in events {
                let _ = reduce(&mut s, &mut life, event, None);
            }
            (s, life)
        };
        let (first, first_life) = fold(events.clone());
        std::thread::sleep(Duration::from_millis(2));
        let (mut second, second_life) = fold(events);

        let started = |state: &ChatState| match &entries(state, AgentId::Main)[0].kind {
            EntryKind::SubAgent(b) => b.started_at,
            other => panic!("expected the sub box first, got {other:?}"),
        };
        assert_ne!(
            started(&first),
            started(&second),
            "the two folds really do carry different instants",
        );

        second.show_thinking_block = !second.show_thinking_block;
        second.show_token_usage = !second.show_token_usage;
        second.compact_transcript = !second.compact_transcript;
        second.tools_expanded = !second.tools_expanded;
        second.set_active_view(AgentId::Sub(1));

        assert_eq!(canon(&first, &first_life), canon(&second, &second_life));
    }

    #[test]
    fn canonical_form_separates_states_that_differ_only_in_hidden_settings() {
        // The footer's model line renders model id plus thinking, so a
        // difference in provider, speed or verbosity would be invisible to
        // an oracle built on that string. Spec 6.3's `state` frame carries
        // all four, and settings visibility for a mid-session joiner is a
        // named sharp edge (spec 11), so the oracle carries the snapshot.
        let life = AgentLifecycle::default();
        let reference = canon(&state(), &life);
        let mutations: [fn(&mut AgentSettings); 3] = [
            |s| s.provider = "openai".into(),
            |s| s.speed = "fast".into(),
            |s| s.verbosity = "high".into(),
        ];
        for mutate in mutations {
            let mut settings = main_settings();
            mutate(&mut settings);
            let variant = ChatState::new(settings.clone(), 200_000, Arc::new(Vec::new()));
            assert_ne!(
                canon(&variant, &life),
                reference,
                "{settings:?} reads as the same state",
            );
        }
    }

    #[test]
    fn canonical_form_separates_states_that_differ_in_covered_fields() {
        // One base fold, then one variant per covered field. Each has to
        // read as a different state, otherwise the oracle is blind to
        // that field.
        let base = || {
            let mut s = state();
            let mut life = AgentLifecycle::default();
            apply(
                &mut s,
                &mut life,
                sub_agent_start(1, "scripted", "scripted"),
            );
            apply(&mut s, &mut life, tool_start(AgentId::Main, "c1", "bash"));
            (s, life)
        };
        let (s, life) = base();
        let reference = canon(&s, &life);

        let (mut tool_done, mut tool_done_life) = base();
        apply(
            &mut tool_done,
            &mut tool_done_life,
            tool_end(AgentId::Main, "c1", "bash", bash_task_details("out", None)),
        );
        assert_ne!(
            canon(&tool_done, &tool_done_life),
            reference,
            "tool status is covered",
        );

        let (mut concluded, mut concluded_life) = base();
        apply(
            &mut concluded,
            &mut concluded_life,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                report: "failed hard".to_string(),
                conclusion: SubAgentConclusion::Failed,
            },
        );
        assert_ne!(
            canon(&concluded, &concluded_life),
            reference,
            "a sub-agent's conclusion is covered",
        );

        let (mut used, mut used_life) = base();
        apply(
            &mut used,
            &mut used_life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: token_usage([10, 1, 0, 0]),
            },
        );
        let used_canon = canon(&used, &used_life);
        assert_ne!(used_canon, reference, "a usage row is covered");
        let (mut used_more, mut used_more_life) = base();
        apply(
            &mut used_more,
            &mut used_more_life,
            AgentEvent::UsageUpdate {
                agent_id: AgentId::Main,
                usage: token_usage([20, 1, 0, 0]),
            },
        );
        assert_ne!(
            canon(&used_more, &used_more_life),
            used_canon,
            "and so is what a usage row reports",
        );

        let (mut noticed, mut noticed_life) = base();
        apply(
            &mut noticed,
            &mut noticed_life,
            AgentEvent::Warning {
                agent_id: AgentId::Main,
                text: "heads up".into(),
            },
        );
        assert_ne!(
            canon(&noticed, &noticed_life),
            reference,
            "a notice is covered",
        );

        // Two folds of the same text under different message ids are
        // different states: branch targets resolve through that id.
        let mut left = state();
        let mut left_life = AgentLifecycle::default();
        apply(&mut left, &mut left_life, user_message_end("hello"));
        let mut right = state();
        let mut right_life = AgentLifecycle::default();
        apply(&mut right, &mut right_life, user_message_end("hello"));
        assert_ne!(
            canon(&left, &left_life),
            canon(&right, &right_life),
            "a message id is covered",
        );
    }
}
