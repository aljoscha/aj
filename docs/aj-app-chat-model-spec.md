# Spec C: transcript model + `AgentEvent` reducer

## Status: implemented

Companion to `docs/aj-next-vaxis-plan.md`. This spec defines the one true
interface for `aj-next`'s interactive rendering: a backend-neutral **chat model**
(`ChatState`) plus an **`AgentEvent` reducer** that applies each event to that
model.

The reducer is the state update function for the chat domain. It takes the
current `ChatState` plus an `AgentEvent`, mutates the model, and reports whether
the UI should redraw. It is not a vaxis widget or component.

It lives in `aj-app` and is consumed only by `aj-next`. `aj`'s imperative
`EventPump` is untouched (see the option-2 decision in the plan). The reducer is
the analogue of `EventPump::handle`, but it mutates data, not aj-tui components,
so it is unit-testable with no terminal.

## Why a model + reducer

In `aj-tui` the pump reaches into the live component tree, addressing widgets by
container index and mutating them in place. In a retained cell framework
(`vxfw`), the natural shape is inverted: the app owns a data model, and widgets
draw themselves from it. So `aj-next` keeps a `ChatState`, the reducer applies
`AgentEvent`s to it, and the chat view is a `vxfw` scroll container whose source
builds one widget per model entry. This makes the state layer pure and testable,
and keeps widget internals out of the event path.

The reducer is pure domain logic with no TUI dependency, which is why it belongs
in `aj-app` rather than in `aj-next`. It is the `aj-next` analogue of
`EventPump::handle`, but it updates transcript data instead of live widgets. It
costs the same to write either place, and `aj-app` gives it a shared home and
terminal-free tests.

## The boundary: what the reducer owns vs what the view owns

- **Reducer owns:** the transcript entries, per-agent streaming bookkeeping, the
  background-task table, footer accounting, and the display flags. It answers
  "what is there to render."
- **View owns:** turning the model into `vxfw` widgets, layout, styling, scroll
  position and follow-tail, and expand/collapse rendering driven by the display
  flags. It answers "how it looks."

The reducer never constructs a widget and never reads a `vaxis` type. The view
never inspects an `AgentEvent`.

## Model types

Backend-neutral. No `aj-tui`, no `vaxis`. Entries are keyed by a stable
`EntryId`, never a container index.

```
pub struct ChatState {
    /// Per-agent transcripts. Main always present; a Sub(n) transcript is
    /// created on SubAgentStart. A sub-agent's entries live in its own
    /// transcript; the parent transcript holds a SubAgent entry pointing at it.
    transcripts: HashMap<AgentId, Transcript>,

    /// Which agent's transcript the chat view currently shows (Main, or a
    /// sub-agent viewed in full).
    active_view: AgentId,

    /// Per-agent streaming bookkeeping (the pump's AgentRender, de-index-ed).
    render: HashMap<AgentId, AgentRender>,

    /// Background tasks, keyed by TaskId (the pump's TaskInfo, de-index-ed).
    tasks: BTreeMap<TaskId, TaskInfo>,

    /// Per-agent footer store (from Spec D: AgentFooters moves to aj-app).
    footers: AgentFooters,

    /// Model catalog, for resolving a settings bundle's context window.
    catalog: Arc<Vec<ModelInfo>>,

    /// Display flags the view reads at draw time. Flipping one is a redraw,
    /// not a walk of components (the pump walks; the view just re-reads).
    hide_thinking_block: bool,
    tools_expanded: bool,
    show_image_in_terminal: bool,
}

pub struct Transcript {
    entries: Vec<Entry>,     // append-only within a session
    next_id: u64,            // mints EntryId
}

pub struct Entry {
    pub id: EntryId,         // opaque, stable for the session
    pub kind: EntryKind,
}

pub enum EntryKind {
    User(UserEntry),
    Assistant(AssistantEntry),
    Tool(ToolEntry),
    SubAgent(SubAgentEntry),
    Compaction(CompactionEntry),
    Notice(NoticeEntry),
    TurnUsage(TurnUsageEntry),   // { agent_id, usage }, the (sub agent N)
                                 // prefix needs the emitting agent and line()
                                 // keeps formatting out of the view
}
```

`EntryId` is an opaque per-transcript id (a `u64` counter). Containers only
append within a session, so an index would also be stable, but an opaque id keeps
the model honest against any future reordering and reads clearly in the
bookkeeping maps.

Entry payloads:

```
pub struct UserEntry {
    pub content: Vec<UserContent>,
    /// True for harness task-completion notices (text starts with the
    /// task-notification tag). The view renders these collapsible under the
    /// tools-expand flag instead of dumping the whole notice.
    pub collapsible: bool,
}

pub struct AssistantEntry {
    /// The latest AssistantMessage snapshot. MessageUpdate carries a cumulative
    /// `partial: AssistantMessage`, so the reducer stores the snapshot rather
    /// than replaying deltas. On MessageEnd this is the finalized message.
    pub message: AssistantMessage,
    pub finalized: bool,
}

pub struct ToolEntry {
    pub call_id: String,
    pub tool: String,
    pub args: serde_json::Value,     // empty object on the replay/build-on-miss path
    pub status: ToolStatus,          // Running | Done { is_error }
    pub details: Option<ToolDetails>, // None until the first partial arrives
    pub content: Arc<[UserContent]>,  // finalized wire content (for inline images),
                                      // the Arc the events carry, a refcount bump
                                      // instead of a deep copy of image bytes
    /// Set when a background task attaches to this cell (bash/agent task badge).
    pub task: Option<TaskId>,
    /// Sub-agent tools render header-only unless the sub is being observed.
    /// A display hint the view reads; the reducer sets it from active_view.
    pub header_only: bool,
}

pub struct SubAgentEntry {
    pub child: usize,                // the n in Sub(n); transcript key is Sub(child)
    pub task: String,
    pub status: SubAgentStatus,      // Running | Done
}

pub struct CompactionEntry { pub tokens_before: u64, pub tokens_after: u64, pub summary: String }

pub struct NoticeEntry { pub level: NoticeLevel, pub text: String }  // Info | Warning | Error
// Retries render as warnings, matching the pump's yellow row, so there is no
// separate Retry level.

pub struct AgentRender {
    /// The in-flight assistant entry for this agent, or None between turns.
    current_assistant: Option<EntryId>,
    /// call_id -> the tool entry it maps to, in this agent's transcript.
    tool_index: HashMap<String, EntryId>,
}

pub struct TaskInfo {
    pub kind: TaskKind,
    pub label: String,
    pub owner: AgentId,
    pub call_id: String,
    pub status: TaskStatus,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    /// The launch cell in the owner's transcript, snapshotted at TaskStart so
    /// it survives the owner's AgentEnd clearing its tool_index. None when the
    /// launching call has no cell (the agent tool renders as a sub-agent box).
    cell: Option<EntryId>,
}
```

## Lifecycle sets live in `SessionCore`

Per Spec D, `running_agents` and `compacting` move into `SessionCore` (they are
orchestration truth read by the turn primitives and the quit-arm logic, not just
view state). The reducer updates them, so its signature takes them alongside the
model:

```
pub fn reduce(state: &mut ChatState, lifecycle: &mut AgentLifecycle, event: AgentEvent) -> Redraw;
```

The event is taken by value: `aj-next` owns each `AgentEvent` off
`subscribe_channel`, so the reducer moves payloads (the assistant `partial`, tool
`content`) into the model instead of cloning them. Persistence is a separate bus
subscriber, so the reducer does not need to leave the event intact for anyone
else.

where `AgentLifecycle { running_agents: HashSet<AgentId>, compacting: HashSet<AgentId> }`
is the type `SessionCore` owns. `Redraw` is a simple "did anything change"
signal the host turns into `app.request_redraw()`. Both types are in `aj-app`, so
this coupling is intra-crate. `aj` does not use the reducer: its pump updates
`SessionCore`'s sets directly.

## The reduction rules

One arm per `AgentEvent` variant. These preserve the domain rules currently
encoded in `EventPump::handle` and the message/tool handlers. Routing is by
`event.agent_id()` into `transcripts[agent_id]` and `render[agent_id]`.

| Event | Model mutation |
|---|---|
| `AgentStart` | `lifecycle.running_agents.insert(id)`. If `Sub(n)`, set its `SubAgentEntry` status to `Running` (a continuation re-prompt has no `SubAgentStart`, so this is what re-runs a box). |
| `AgentEnd` | `lifecycle.running_agents.remove(id)`. Clear `render[id]` (`current_assistant = None`, `tool_index.clear()`). If `Sub(n)`, set its box status `Done`. |
| `TurnStart` | `render[id].current_assistant = None` (next assistant opens fresh). |
| `MessageStart` | No-op. The authoritative payload lands on `MessageEnd` (user/tool-result) or the assistant entry is materialized lazily by the first painting `MessageUpdate` / by `MessageEnd` on the replay path. |
| `MessageUpdate` | Assistant streaming only. Ignore non-painting events (tool-call deltas, `Start`/`Done`/`Error`). For a painting event, ensure the current assistant entry exists (creating it and setting `current_assistant`), then replace its stored `AssistantMessage` snapshot with `event`'s cumulative `partial`. |
| `MessageEnd` | Dispatch on message kind. **User:** append a `UserEntry` (skip empty; set `collapsible` when the text starts with the task-notification tag). **Assistant:** finalize `current_assistant` (materializing it on the replay path if it was never opened and the payload has a Text/Thinking block), set `finalized`, clear `current_assistant`. Then, unless the message is an abort (`StopReason::Aborted` or an `Aborted`-category error), append a `Notice{Error}` when the message carries an error, or a generic "the model stream failed" error when `stop_reason == Error`. **ToolResult:** no-op (structural framing; the result renders from `ToolExecutionEnd`). |
| `ToolExecutionStart` | Skip when `tool == "agent"` (the sub-agent box represents it). Otherwise append a `ToolEntry{status: Running}`, record `render[id].tool_index[call_id] = entry`, set `header_only` from whether this sub is being observed, and clear `current_assistant` (a mid-turn tool call ends the assistant block). |
| `ToolExecutionUpdate` | Skip `agent`. Update the mapped `ToolEntry`'s `details` (+ `content`) with the cumulative partial. |
| `ToolExecutionEnd` | Skip `agent`. Build the entry on miss (replay path: no Start seen, args empty), then set `status: Done{is_error}`, `details`, `content`. The build-on-miss branch replicates the live bookkeeping (record `tool_index`, clear `current_assistant`). |
| `Notice` / `Warning` / `Error` | Append `NoticeEntry` with level `Info` / `Warning` / `Error`. |
| `StreamRetry` | Append `NoticeEntry{Warning}` with the retry cadence line (the failed attempt's error already rendered from its `MessageEnd`). |
| `UsageUpdate` | Append or update a `TurnUsage` row keyed to the preceding assistant or compaction checkpoint. Assistant usage folds into `footers.record_turn_usage(id, usage)`. Compaction usage advances cumulative spend but does not replace context occupancy with the summarizer's prompt size. |
| `CompactionStart` | `lifecycle.compacting.insert(id)`; the view labels the spinner "Compacting context...". |
| `CompactionProgress` | Record the phase in `ChatState.compaction_phase` for the view's spinner label (`Summarizing` / `SummarizingTurnPrefix` / `Saving`). No transcript entry. |
| `CompactionEnd` | `lifecycle.compacting.remove(id)`. On `error`, append `Notice{Warning}` "Compaction failed: ...". On `summary` (success), append `CompactionEntry`, install its durable id as the following usage origin, and call `footers.set_context_tokens(id, tokens_after)`. On neither, append `Notice{Info}` "Compaction canceled." |
| `SubAgentStart` | Ensure the `Sub(n)` transcript and a `SubAgentEntry{Running}` in the parent (the box). Seed `footers.note_settings(child, settings, resolve_window(settings))`. The running status and footer count derive from the paired `AgentStart(Sub n)`. |
| `SubAgentEnd` | Set the `Sub(n)` box status to `Done`. |
| `TaskStart` | Snapshot the launch cell (`render[owner].tool_index[call_id]`) now, since the owner's `AgentEnd` will clear it. Insert `TaskInfo{Running, cell}`. |
| `TaskOutput` | Resolve the task's cell (live `tool_index` by `call_id`, else the snapshot) and update that `ToolEntry`'s partial. |
| `TaskEnd` | Set the `TaskInfo` status and `finished_at`; update the cell's `ToolEntry` to the finished state (badge). |
| `QueueUpdate` | The view re-reads the live queue snapshot (see `aj_agent::queue`) for the pending-message box when `id == active_view`, rather than trusting the payload (guards against a UI enqueue racing the drain). |
| `TurnEnd` | No-op. The transcript is built incrementally from the message/usage events; the finalized snapshot is not needed. Arm stays explicit for exhaustiveness. |

Notes on the subtle rules, preserved verbatim in intent:

- **`MessageUpdate` stores a snapshot, not deltas.** This does not change
  streaming. The agent emits one `MessageUpdate` per provider chunk exactly as it
  does for `aj`, so the text grows chunk-by-chunk and each update triggers a
  redraw (coalesced by the frame throttle). The only difference is bookkeeping:
  each `MessageUpdate` already carries a cumulative `partial: AssistantMessage`,
  so the reducer replaces the stored snapshot instead of appending a delta. Both
  render the full text-so-far after every chunk. The snapshot is simpler than the
  pump's per-block `open/append/close` state machine and self-heals a dropped
  delta (the pump reaches for this only on `TextEnd`). `aj-next` receives owned
  events off `subscribe_channel`, so `reduce` takes the event by value and moves
  the `partial` in, avoiding a per-chunk clone.
- **Terminal error appends a row; abort does not.** A failed turn carries its
  error in-band on the finalized assistant message. The reducer appends an error
  notice on `MessageEnd`, except for aborts (confirmed on the turn-completion
  path) to avoid a duplicate notice.
- **The `agent` tool is invisible as a tool.** Its `ToolExecution*` events are
  skipped; the `SubAgentEntry` box is its representation.
- **Sub-agent header-only.** A sub-agent's tool entries render header-only unless
  that sub is the active full view. The reducer records the hint; the view honors
  it and flips it when `active_view` changes.
- **Task cells survive the turn.** Background tasks outlive their turn, so the
  launch cell is snapshotted at `TaskStart` and events route by the snapshot once
  the owner's `tool_index` is cleared.

## Replay uses the same reducer

`aj_session::replay` emits the same `AgentEvent`s (a `MessageStart` + `MessageEnd`
pair with no `MessageUpdate` for finalized messages, `ToolExecutionEnd` with no
`Start`). The reducer already handles both paths (the "materialize on
`MessageEnd`" and "build on miss for `ToolExecutionEnd`" branches), so `aj-next`
replays history by feeding replayed events through the same `reduce` call it uses
for live events. No separate replay path.

## What the view does with the model

The `aj-next` chat view (a later spec) renders `transcripts[active_view]` into a
`vxfw` scroll container, one widget per `Entry`, reading the display flags:

- `AssistantEntry`: render text and thinking blocks from the stored
  `AssistantMessage`. `hide_thinking_block` collapses thinking to a one-line
  placeholder. This is a draw-time decision, so toggling the flag needs only a
  redraw, not a walk of entries.
- `ToolEntry`: header plus body; `tools_expanded` and `header_only` control the
  body. `show_image_in_terminal` gates inline image rendering from `content`.
- `SubAgentEntry`: the box; when it is the `active_view`, the view renders the
  `Sub(n)` transcript in full instead.

Spinner, footer, and pending-message box read `lifecycle`, `footers`, and the
live queue respectively.

## Testing

Unit tests over `reduce` with no terminal, porting the behavioral coverage from
`event_pump.rs`'s tests (which are extensive):

- streaming text/thinking builds one assistant entry; a tool-use-only turn leaves
  no empty assistant entry;
- terminal error appends an error notice, abort does not, an errored stop without
  detail renders the generic line;
- sub-agent events route into the `Sub(n)` transcript and the `agent` tool is
  skipped;
- background task output tails the launch cell and `TaskEnd` freezes it;
- a resumed launch cell (replay, no task tracking) renders its badge from the
  `ToolDetails::Bash` payload;
- footer usage accounting per agent (from the moved `AgentFooters` tests).

## Relationship to `aj`'s pump

`aj` keeps `EventPump::handle`. The reducer re-expresses the same domain rules
against data. The overlap is semantic, not shared code, and resolves if `aj` is
ever retired or refactored onto the model. Until then, a change to the
`AgentEvent` vocabulary touches both, which is unavoidable and rare.

## Decisions

- **C-1. `EntryId`. Resolved: opaque `u64` per transcript.** Cheap and
  future-proof against any later reordering, and it reads clearly in the
  bookkeeping maps.
- **C-2. Assistant snapshot vs deltas. Resolved: snapshot.** Store the cumulative
  `AssistantMessage` from each `MessageUpdate`, moved in by value. Simpler,
  authoritative, self-heals a dropped delta, and no per-chunk clone. Streaming
  cadence is unchanged: one `MessageUpdate` per provider chunk, one throttled
  redraw.
- **C-3. Lifecycle-set ownership. Resolved: in `SessionCore`.** `reduce` takes
  `AgentLifecycle` by `&mut`, so `running_agents` / `compacting` are one source
  of truth shared with the turn primitives and the quit-arm logic.
- **C-4. Pending queues. Resolved: re-read live.** The view re-reads the live
  `MessageQueues` snapshot on `QueueUpdate`, matching the pump and guarding
  against a UI enqueue racing the drain.
