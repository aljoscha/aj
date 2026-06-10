# Background tasks — spec

Let the model run `bash` commands and sub-agents **in the background**:
the tool call returns immediately with a task id, the work continues on
a detached task, and the model observes it through a small family of
`task_*` tools (incremental output, wait, kill) plus automatic
completion notices injected into its transcript. When a task finishes
while the owning agent is idle, the notice **wakes** it — a turn is
triggered so the agent reacts to the result without polling and
without waiting for the user. The user observes background work
through the existing footer / agent-picker surfaces, extended to
cover bash tasks, with live output in the transcript.

This builds directly on the retention + concurrency foundation from
`docs/subagent-steering-spec.md` (§7 there sketches background spawn as
"an additive mode on top"; this is that mode) and reuses the rendering
machinery from `docs/subagent-observability-spec.md`.

This document is the implementation contract. It is split into stages
at the end for orchestration.

---

## 1. Background: what happens today

### 1.1 Both tools block the turn

- `bash` (`src/aj-tools/src/tools/bash.rs`): spawns `bash -c` in a
  fresh process group, drains stdout/stderr through two reader tasks
  into per-stream rolling tails (`StreamState`) teed into a spill file
  (`SpillState`), and `select!`s on cancellation / timeout / child
  exit. `execute` does not return until one of those fires. Progress
  snapshots go through `ToolContext::emit_update`, which is a no-op in
  production (lib.rs:1857 — sync trait method vs async bus).
- `agent` (`src/aj-tools/src/tools/agent.rs`): delegates to
  `ToolContext::spawn_agent`, which retains the child in the shared
  `SubAgentRegistry`, locks the handle, and runs `run_single_turn` to
  completion before returning `SpawnedAgent { agent_id, report }`. The
  parent is suspended at that `.await` for the child's whole run.

### 1.2 The pieces background execution needs already exist

- **Retention**: `SubAgentRegistry` (lib.rs:1518) keeps live
  `SharedAgent` handles past the spawning tool call; the binary
  resolves them to drive continuations.
- **Concurrent turns**: the binary's `turns: JoinSet` +
  `turn_cancels: HashMap<AgentId, CancellationToken>` already run
  multiple agents' turns concurrently, serialized per agent.
- **Event routing**: sub-agents share the parent's bus tagged with
  `Sub(n)`; the pump's `running_agents` set drives the per-view
  spinner, the footer aggregate, and per-box status. Detached tasks
  can emit on the bus without the `emit_update` sync problem — they
  run on their own tokio tasks and can `.await` the emit.
- **Output buffering**: bash's rolling tail + spill file is exactly
  the shape a background job's output buffer needs; it only lacks
  cursored reads.

### 1.3 What's missing

- Nothing survives a tool call except sub-agent handles: there is no
  job registry, no task ids, no way to read a process's output after
  the originating call returned.
- No mechanism injects content into an agent's transcript between
  turns or between tool batches — the `QueueUpdate` event exists but
  nothing emits it.
- Every `tool_use` must get its `tool_result` in the same batch, so a
  background task's *final* outcome cannot ride the original tool
  result; it has to arrive later through another channel.

---

## 2. Goals / non-goals

**Goals**

1. `bash` and `agent` accept `background: true`; the call returns
   quickly with a task id while the work continues detached.
2. A background bash launch waits a short window (~2s) before
   returning: commands that die instantly (typo, missing binary, bound
   port) surface their failure inline instead of costing a poll
   round-trip; commands that finish inside the window return the
   normal foreground result.
3. The model can read **incremental** output (`task_output`), block
   until something finishes (`task_wait`), kill a task (`task_kill`),
   and list tasks (`task_list`).
4. When a task finishes, a **completion notice** is injected into the
   owning agent's transcript at the next natural point (before the
   next inference step, or at the start of the next turn) so the model
   never has to busy-poll and cannot forget a task.
5. **Auto-wake**: a notice arriving while the owning agent is idle
   triggers a turn on that agent (main *and* sub-agents alike) — the
   agent reads the result and keeps working. Waking without polling is
   what makes backgrounding genuine parallelism rather than deferred
   reads.
6. Background tasks survive turn end but **not process exit**: they
   are killed on shutdown.
7. The user sees background bash tasks in the footer and the alt+a
   picker, with live output in the transcript (the same treatment
   sub-agents already get).

**Non-goals (this iteration)**

- **Detach-from-process**: no nohup-style tasks that outlive aj. Exit
  kills everything.
- **Resurrecting tasks on `/resume`**: tasks are process-scoped.
  Persisted launch results and notices stand alone in the transcript.
- **Live transcript reads of a running background agent** through
  `task_output`: a running agent holds its own `TokioMutex`, so the
  tool reports status only until the report lands. The TUI observes
  running agents through events, as today.
- Implementing batch-parallel tool execution (the unread
  `ExecutionMode` deviation in `execute_turn`). Backgrounding
  sidesteps most of the need; the deviation stays on record.

---

## 3. Locked decisions

1. **A `background` parameter on the existing tools**, not new
   `bash_background` / `agent_background` tools. The decision is made
   at call time; near-duplicate tool descriptions would just burn
   context.
2. **One unified task id space** for bash tasks and background agent
   spawns, minted per session (`#1`, `#2`, …). One concept — "things
   running in the background" — one set of `task_*` tools. Agent-backed
   tasks additionally reference their `Sub(n)` id in metadata.
3. **The original tool result is the "started" result.** The real
   outcome arrives later via `task_output` / `task_wait` / the
   completion notice. This keeps the tool_use/tool_result batch
   invariant untouched.
4. **Completion notices are injected user-role messages**, drained at
   two points: the top of `prompt()` and after each tool batch in
   `execute_turn` (i.e. immediately before every inference step). They
   are persisted like any other message so resumed transcripts read
   coherently.
5. **Notices wake idle owners.** The binary reacts to `TaskEnd` (and
   to turn completion with notices still queued) by driving a wake
   turn on the owner through the same per-agent turn machinery user
   prompts use. A wake turn is an ordinary turn: gated per-agent,
   cancellable per-view, visible in spinner/footer. Sub-agent wakes
   run in the sub-agent's own chat; the user inspects what they came
   up with by switching to that view (steering-spec semantics).
6. **Cursor-based incremental reads.** Each task keeps one
   model-facing cursor; `task_output` returns only output since the
   last read. The notice for a finished bash task carries the unread
   tail (capped by the standard bash budgets), advancing the cursor —
   it behaves like an implicit final `task_output`, so short commands
   need no extra round-trip.
7. **Tasks hang off a session-level cancellation root**, not the
   per-turn token. The registry owns the root; the binary cancels it
   on shutdown. `task_kill` and the TUI cancel per-task child tokens.
8. **Task lifecycle events** (`TaskStart` / `TaskOutput` / `TaskEnd`)
   are new bus variants, transient like `ToolExecutionUpdate` (not
   persisted). They carry the originating `call_id` so the TUI updates
   the existing bash tool cell in place — no new box type for live
   output.
9. **Sub-agents may start background bash tasks** (they inherit the
   toolset); ownership and notices are scoped to the spawning agent.
   Background *agent* spawns remain main-only because the `agent` tool
   stays filtered out of sub-agent toolsets.

---

## 4. Design

### 4.1 Task registry (`aj-agent`)

A cheaply-cloneable shared registry, sibling of `SubAgentRegistry`:

```rust
pub type TaskId = usize;

#[derive(Clone, Copy, ...)]
pub enum TaskStatus {
    Running,
    /// Process exited (code is `None` when signal-killed), or the
    /// agent-backed run completed/failed.
    Exited(Option<i32>),
    /// Killed via `task_kill`, the TUI, or shutdown.
    Killed,
}

pub enum TaskKind {
    Bash { command: String },
    Agent { agent_id: usize, task: String },
}

pub struct TaskEntry {
    pub id: TaskId,
    pub owner: AgentId,
    pub kind: TaskKind,
    pub label: String,          // display label (command / description)
    pub status: TaskStatus,
    pub started_at: Instant,
    cancel: CancellationToken,  // child of the registry root
    output: Arc<dyn TaskOutputSource>,
    cursor: TaskCursor,         // model-facing read position
}

#[derive(Clone, Default)]
pub struct TaskRegistry {
    inner: Arc<StdMutex<RegistryInner>>,  // entries + notices + next id
    root_cancel: CancellationToken,
}
```

Key operations (all lock-only, never held across `.await`):

- `register(owner, kind, label, output) -> StartedTask` — mints the
  id, inserts the entry as `Running`, returns the id plus a child
  cancel token for the driver.
- `read(id) -> TaskRead` — status + everything since the entry's
  cursor (advances it). See §4.4.
- `kill(id)` — cancels the entry's token; the driver flips status.
- `snapshot() -> Vec<TaskSummary>` — for `task_list` and the picker.
- `push_notice(notice)` / `drain_notices(owner) -> Vec<TaskNotice>` /
  `has_notices(owner) -> bool` — per-owner completion-notice queue
  (§4.6); `has_notices` is what the binary's wake triggers poll
  (§4.7).
- `shutdown()` — cancels `root_cancel`; callers then await driver
  quiescence (§4.8).

Output is type-erased behind a small trait so `aj-agent` stays free of
process details while `aj-tools` keeps the buffering implementation:

```rust
pub trait TaskOutputSource: Send + Sync {
    /// Output accumulated since `cursor`, advancing it. Returns raw
    /// (sanitized) text per stream plus totals; the *caller* applies
    /// display truncation. If the cursor has fallen behind the rolling
    /// tail's eviction horizon the read reports the elided byte count
    /// and the spill path.
    fn read_new(&self, cursor: &mut TaskCursor) -> TaskRead;
}
```

Wiring mirrors `SubAgentRegistry`: the binary creates one
`TaskRegistry`, injects it on the main agent
(`set_task_registry`), and the agent threads a clone through
`SessionContextWrapper` into every tool call and every spawned
sub-agent (so sub-agent-owned bash tasks land in the same map the
binary observes). Print mode and tests that never inject one get a
default registry whose tasks die with the process — same pattern,
same caveats as `SubAgentRegistry`.

### 4.2 `ToolContext` additions

```rust
pub trait ToolContext: Send {
    // ... existing methods ...

    /// Shared task registry handle (for the task_* tools).
    fn task_registry(&self) -> TaskRegistry;

    /// Register a background task and get the plumbing its detached
    /// driver needs: the task id, a cancel token (child of the
    /// registry root, NOT of the per-turn token), and an event sink.
    fn start_background_task(
        &mut self,
        kind: TaskKind,
        label: String,
        output: Arc<dyn TaskOutputSource>,
    ) -> StartedTask;
}

pub struct StartedTask {
    pub id: TaskId,
    pub cancel: CancellationToken,
    pub events: TaskEventSink,
}
```

`TaskEventSink` is how a detached driver reaches the bus and the
notice queue without holding the whole agent:

```rust
pub struct TaskEventSink { /* bus clone, owner, task_id, call_id */ }

impl TaskEventSink {
    /// Emit `TaskOutput` (drivers self-throttle, ~10/s).
    pub async fn output(&self, partial: ToolDetails);
    /// Flip the registry status, emit `TaskEnd`, queue the notice.
    pub async fn finished(&self, status: TaskStatus, notice: TaskNotice);
}
```

The sink captures the `call_id` of the originating tool call so
`TaskStart` / `TaskOutput` / `TaskEnd` correlate with the transcript
cell (§4.9). Because drivers are plain tokio tasks, emits are properly
`.await`ed — the `emit_update` sync/async impasse does not apply here.
Bus listener errors are logged and swallowed by the sink: a task
finishing outside any turn has no turn to abort, and persistence never
subscribes to task events anyway.

NOTE: `start_background_task` allocates ids from the registry, not
from `SessionState`, so detached drivers never need the `&mut
SessionState` borrow that makes foreground tools block today.

### 4.3 `bash` background mode

`BashInput` gains:

```rust
/// Run the command in the background. The call returns once the
/// launch window passes (or earlier if the command finishes); use
/// task_output / task_wait to follow it. `timeout` is ignored in
/// background mode — the task runs until it exits or is killed.
#[serde(default)]
pub background: bool,
```

Execution in background mode:

1. Spawn the child and reader tasks exactly as today (`process_group`,
   `StreamState`, spill file). The spill file is **always persisted**
   for background tasks — the process outlives the tool call and the
   full transcript must stay reachable for the TUI and for
   cursor-eviction markers.
2. Call `start_background_task`; move child + states + sink into a
   detached driver task that `select!`s on the task cancel token vs
   `child.wait()`, emits throttled `TaskOutput` snapshots, kills the
   process group on cancellation, and ends with
   `events.finished(status, notice)`.
3. The tool-side waits up to `LAUNCH_WINDOW` (2s) for the driver to
   report a terminal status (a `watch` channel on the entry):
   - **Finished inside the window**: return the normal
     foreground-shaped outcome (same wire content, same
     `ToolDetails::Bash`); the entry is finalized with its cursor at
     the end and the notice suppressed — from the model's perspective
     this was a foreground call.
   - **Still running**: return the started outcome. Wire content:

     ```
     Started background task #3: cargo build --release
     <output captured during the launch window>
     Still running. Use task_output(3) to read new output, task_wait
     to block until it finishes. You will be notified when it
     completes.
     ```

     The launch-window output advances the task cursor (the model has
     seen it). `TaskStart` is emitted at this point, not at spawn, so
     the TUI only tracks tasks that actually went to the background.

`ToolDetails::Bash` gains `task_id: Option<TaskId>` (serde-defaulted
`None`, preserving the persisted shape) so renderers can badge the
cell and correlate task events. Foreground behavior, including
`execution_mode() == Sequential`, is unchanged.

### 4.4 `task_output`, `task_wait`, `task_kill`, `task_list` (`aj-tools`)

All four are thin wrappers over the registry; default
`ExecutionMode::Parallel`.

- **`task_output { id }`** — status plus new-since-cursor output.
  For bash tasks the raw segments from `read_new` are run through
  `truncate_tail` with the standard bash budgets and the existing
  marker phrasing; if the cursor fell behind the rolling tail an
  `[N bytes elided. Full output at <spill path>]` marker leads the
  stream. Details: `ToolDetails::Bash` with `task_id` set, `exit_code`
  populated once terminal. For agent tasks: status while running, the
  final report once done (`ToolDetails::Text`). Unknown id → `is_error`
  outcome listing live ids.
- **`task_wait { ids, timeout: u64 = 120 }`** — resolves when **any**
  listed task reaches a terminal status, or when the timeout fires,
  whichever is first; returns immediately if one already terminal.
  Must `select!` on `ctx.cancellation()` so Ctrl+C cancels the wait
  (not the tasks). The result reports every listed task's status and
  folds in an implicit `task_output` for the finished ones. A timeout
  is **not** an error (`is_error: false`) — it's a normal "still
  running, here's where things stand" report, so the model doesn't
  misread patience as failure. Waiting on all of several tasks is just
  calling `task_wait` again with the survivors.
- **`task_kill { id }`** — cancels the task token, awaits the terminal
  status (bounded), returns the final unread tail. Killing an
  agent-backed task cancels the child's run token; the sub-agent's
  handle stays in `SubAgentRegistry` (it's re-promptable, per the
  steering spec).
- **`task_list {}`** — id, kind, label, status, runtime, unread
  line/byte counts for every task this session. `ToolDetails::Text`.

Completion races are benign: a task may finish between the started
result and the first `task_output`; the read simply reports the
terminal status. If a notice for that task is still queued it is
dropped at drain time when its content has already been consumed via
an explicit read (the entry tracks whether the cursor reached EOF
before the drain).

### 4.5 `agent` background mode

`AgentInput` gains the same `background: bool` (default `false`).
`ToolContext::spawn_agent` grows a mode so the wrapper can implement
both shapes (signature change; the trait has few implementors):

```rust
fn spawn_agent(&mut self, task: String, mode: SpawnMode)
    -> ... Future<Output = anyhow::Result<SpawnResult>>;

pub enum SpawnMode { Blocking, Background }
pub enum SpawnResult {
    Completed(SpawnedAgent),                       // Blocking
    Started { agent_id: usize, task_id: TaskId },  // Background
}
```

Background path inside `SessionContextWrapper::spawn_agent`: build,
configure, and retain the sub-agent exactly as today (same
`SubAgentStart` emit, same registry insert), then instead of locking
and running inline:

1. `start_background_task(TaskKind::Agent { .. })` — the task cancel
   token becomes the child's run cancellation (so `task_kill`, the
   TUI, and shutdown reach the run; the parent's turn token explicitly
   does **not**, because outliving the parent's turn is the point).
2. `tokio::spawn` a driver that locks the `SharedAgent`, runs
   `run_single_turn(task)`, then emits `SubAgentEnd` (unchanged
   contract: emitted regardless of success) and
   `events.finished(status, notice)` with the report (or error) as the
   notice body. The driver also records the child's accumulated usage
   on the task entry; the owner folds it into
   `SessionState.sub_agent_usage` when the notice is drained — the
   drain points hold `&mut self`, so usage accounting needs no shared
   mutability.
3. Return `Started`. The tool result is
   `ToolDetails::Text { summary: "agent 2 started in background (task #5)" }`
   with matching wire content plus the "you will be notified" hint.
   The rich rendering needs no new variant — `SubAgentStart` already
   creates the box, and the report reaches the transcript via the
   notice.

The pump's gating from the steering spec applies untouched: the
background run emits `AgentStart(Sub n)` / `AgentEnd(Sub n)`, so the
sub-agent shows as running in the picker, user submits to it are
gated, and the footer counts it.

### 4.6 Completion notices

`TaskNotice` carries owner, task id, kind, label, terminal status, and
a pre-rendered body (for bash: exit status plus the unread tail,
truncated with the standard budgets and markers; for agents: the
report verbatim).

Drain points, both holding `&mut self` on the owning agent:

1. **Top of `run_top_level_turn_inner`**, before the prompt append —
   notices that arrived while the agent was idle land *before* the
   user's new message, in arrival order.
2. **In `execute_turn` after a tool batch completes**, before the
   `continue` that triggers the next inference — notices that arrived
   mid-run reach the model at the first opportunity.

Each drained notice becomes one user-role wire message:

```
[background task #3 finished: cargo build --release — exit code 0]
<unread output tail, if any>
```

appended to the transcript with the same `MessageStart` / `MessageEnd`
emission the prompt path uses (lib.rs:651-675), so it renders and
persists like any other message. Injecting user-role messages adjacent
to tool results is the same wire shape the failed-turn re-prompt path
already produces; providers accept consecutive user-role messages.
Sub-agent drain points are identical (`run_single_turn_inner` shares
`execute_turn`), so a sub-agent that backgrounded a command hears
about it inside its own run or on its next continuation.

Notices arriving while the owner is idle (or surviving past a turn's
end) do not wait for the user — they wake the owner (§4.7).

### 4.7 Auto-wake

The agent runtime cannot start its own turns — the binary owns turn
driving (per-agent `turns: JoinSet` + `turn_cancels` gating from the
steering spec). So wake is split between a new entry point on `Agent`
and two triggers in the binary.

**`Agent::wake(cancel) -> Result<WakeOutcome, TurnError>`** — drains
the owner's notice queue; if nothing was drained (a concurrent prompt
or wake already consumed it) it returns `WakeOutcome::Empty` without
emitting any event. Otherwise it appends the drained notice messages
(same `MessageStart`/`MessageEnd` emission as §4.6), brackets the run
with `AgentStart`/`AgentEnd` like every other top-level run, and
drives `execute_turn`. This is `continue_run` minus the
ends-in-user-message precondition — the drained notices *are* the
user-role messages that make the transcript valid for inference.

**Binary triggers**, both funneling into the same spawn path user
submits use (resolve handle → gate → insert `turn_cancels` → spawn
onto `turns`):

1. **On `TaskEnd`**: if the owner is idle (`!turn_cancels.contains` &&
   `!pump.is_running(owner)`), spawn a wake turn on it. If the owner
   is busy, do nothing — the mid-run drain point or trigger 2 picks
   the notice up.
2. **On turn completion** (the `JoinSet` arm): if
   `registry.has_notices(id)` for the agent that just finished, spawn
   a wake turn on it. This closes the race where a task finishes
   after the last mid-run drain point but before the turn ends.

Because wake turns ride the normal machinery, everything composes
without special cases: per-agent single-turn gating (a user submit
and a wake can't overlap), Ctrl+C on the viewed agent cancels its
wake turn, the editor disables while the viewed agent is woken, and a
wake turn that itself backgrounds more tasks just continues the
cycle. `WakeOutcome::Empty` makes the triggers idempotent — both may
fire for the same notice and the second wake is a cheap no-op (the
binary skips spawning when the gate is closed anyway).

Sub-agent wakes need a live handle; owners always have one because
task ownership is only acquired by a live run in this process
(resumed, handle-less sub-agents cannot have started tasks).

Wake turns apply the current run-config and count usage like any
other turn. In print mode there is no trigger loop: the run ends when
the main turn (and any `task_wait`s the model issued) complete, and
remaining tasks are killed at exit — noted in the tool description so
the model waits explicitly before finishing there.

### 4.8 Cancellation & lifecycle

Token hierarchy:

- `TaskRegistry.root_cancel` — session-scoped, cancelled by the binary
  on shutdown.
- per-task child tokens — cancelled by `task_kill`, the picker's kill
  action, or the root.
- The per-turn token never reaches task drivers. Cancelling a turn
  cancels an in-flight `task_wait` (it selects on the turn token) but
  not the tasks themselves.

Shutdown: the binary calls `registry.shutdown()` before exit, then
awaits driver quiescence with a short bounded grace (drivers respond
promptly: `kill -9 -- -<pgid>` + reap, or a cancelled child run). The
Ctrl+C quit-arming flow from the steering spec §4.5 extends naturally:
"N agents / M tasks still running — press Ctrl+C again to quit". If aj
is killed outright the process-group children leak — same class of
limitation as today's foreground bash, recorded as known.

Double-forking daemons escape the process group and our kill — known
limitation, called out in the bash tool description so the model
prefers proper foreground supervision (`background: true` on the
supervisor) over `nohup`-style detachment.

### 4.9 TUI

- **Pump**: track `tasks: BTreeMap<TaskId, TaskInfo>` from `TaskStart`
  / `TaskEnd` (`TaskInfo`: kind, label, owner, call_id, status).
  Agent-backed tasks are *not* double-counted: the footer's agent
  count already covers them via `running_agents`, so the task
  indicator counts only `TaskKind::Bash` entries.
- **Footer**: generalize `AgentActivity` / `render_agent_activity` to
  `"2 agents, 1 task (alt+a)"` (each part shown only when nonzero).
- **Picker**: the alt+a picker grows task entries beneath the agent
  entries — status glyph, label (command tail), runtime. Selecting a
  bash-task entry jumps the chat view to the owning agent's transcript
  scrolled to the task's cell. A kill action on the selected task
  (`ctrl+k`) fires `registry.kill(id)`.
- **Live output**: `TaskOutput { call_id, partial }` updates the
  existing bash tool cell for the originating call in place — the cell
  keeps live-tailing after `ToolExecutionEnd`, then freezes on
  `TaskEnd` with a final status badge. No new box type; the
  `ToolDetails::Bash.task_id` badge marks the cell as a background
  task. (The full scrollback is always one keypress away via the spill
  path shown in the cell.)
- **Persistence/resume**: task events are transient. A resumed
  transcript shows the persisted launch cell (launch-window snapshot +
  task id badge) and any persisted notices; the registry starts empty.

### 4.10 Events

New `AgentEvent` variants, all carrying the owner's `agent_id` plus
`task_id` and the originating `call_id`:

```rust
TaskStart   { agent_id, task_id, call_id, kind, label },
TaskOutput  { agent_id, task_id, call_id, partial: ToolDetails },
TaskEnd     { agent_id, task_id, call_id, status, label },
```

Transient (persistence ignores them); fully serializable so
`aj --format json` ships them like every other variant. `TaskOutput`
reuses `ToolDetails::Bash` snapshots — same shape `emit_update`
produces today, so renderers share one code path.

---

## 5. Edge cases

- **Command dies inside the launch window** → foreground-shaped result,
  no task, no notice, no `TaskStart`. The typo/`command not found`
  case costs zero extra round-trips.
- **Task finishes between "started" result and first read** → reads
  report the terminal status; the queued notice is dropped at drain
  time iff the model already consumed everything (cursor at EOF on a
  terminal task).
- **Cursor falls behind the rolling tail** (chatty process, infrequent
  reads) → the read leads with an elision marker and the spill path;
  totals stay exact because `StreamState` already tracks them.
- **`task_wait` timeout** → normal (non-error) status report.
- **`task_wait` cancelled by Ctrl+C** → the wait's tool call resolves
  as the standard cancelled outcome; tasks unaffected.
- **Unknown / already-terminal ids** → `is_error` outcome listing
  live ids (unknown), or an immediate normal report (terminal).
- **Sub-agent-owned task finishes after the sub-agent's run ended** →
  trigger 1 wakes the sub-agent in its own chat; the user inspects the
  result by switching views. No escalation to Main.
- **Task finishes while the owner's turn is ending** (after the last
  mid-run drain point) → trigger 2 catches it on turn completion.
- **Wake races a user submit** → per-agent gating serializes them;
  whichever runs first drains the queue, and the loser's wake is an
  `Empty` no-op (or the prompt path's drain delivers the notices
  before the user's message).
- **Turn aborts mid-batch with notices pending** → the drain point is
  skipped with the rest of the loop; trigger 2 fires on the aborted
  turn's completion, so the notices still wake the owner. If the user
  cancelled deliberately and also wants the tasks gone, that's
  `task_kill` / the picker's kill — cancelling a turn has never
  implied killing its children.
- **User quits with notices queued** → the TUI already showed
  `TaskEnd`; shutdown kills the task tree; queued notices die with the
  process (they are only persisted once drained into a transcript).
- **Print mode** → works (registry defaults), but tasks pending when
  the run ends are killed at exit; the model should `task_wait` before
  finishing. Worth one line in the tool description.
- **Notice content vs. context budget** → notice bodies use the same
  per-stream budgets as bash results; a misbehaving firehose costs at
  most one bash-result's worth of context per notice.

---

## 6. Touch list

- `src/aj-agent/src/tool.rs`: `TaskId`, `TaskStatus`, `TaskKind`,
  `TaskOutputSource`, `TaskCursor`/`TaskRead`, `StartedTask`,
  `TaskEventSink`, `SpawnMode`/`SpawnResult`; `ToolContext` methods;
  `ToolDetails::Bash.task_id`.
- `src/aj-agent/src/lib.rs`: `TaskRegistry` + notice queue;
  `Agent::set_task_registry`; wrapper plumbing; background path in
  `spawn_agent`; drain points in `run_top_level_turn_inner` /
  `execute_turn`; `Agent::wake` + `WakeOutcome`; usage folding at
  drain.
- `src/aj-agent/src/events.rs`: `TaskStart` / `TaskOutput` / `TaskEnd`.
- `src/aj-tools/src/tools/bash.rs`: `background` input, driver
  extraction (the run loop becomes shared between foreground and
  detached modes), `TaskOutputSource` impl over `StreamState`, launch
  window.
- `src/aj-tools/src/tools/agent.rs`: `background` input, started
  outcome.
- `src/aj-tools/src/tools/`: new `task.rs` with the four `task_*`
  tools.
- `src/aj/src/modes/interactive.rs`: registry injection; wake
  triggers (on `TaskEnd` in the bus arm, on turn completion in the
  `JoinSet` arm); shutdown ordering (`registry.shutdown()` + grace
  before `turns.shutdown()`); Ctrl+C arming text.
- `src/aj/src/modes/interactive/event_pump.rs` + `components/footer.rs`
  + `components/agent_picker.rs` + chat-view cell updates: §4.9.

---

## 7. Resolved decisions

1. **Sub-agent notices stay in the sub-agent's chat** and wake the
   sub-agent there — no escalation to Main. The user inspects the
   woken sub-agent's output by switching to its view; if its
   conclusions matter to Main, the user (or a future report-to-parent
   action) carries them over.
2. **Auto-wake is in scope** (§4.7). Waking without polling is the
   point of backgrounding; without it a notice can rot until the next
   user prompt.
3. **No dedicated full-screen task view this iteration.** The inline
   live cell plus the always-persisted spill file is the v1
   observation surface; we revisit after test-driving (a `ChatView`
   active-id generalization to `Agent|Task` is the sketched path if
   the inline cell proves too cramped).

---

## 8. Staged implementation plan

**Stage A — registry + protocol (aj-agent).** `TaskRegistry`, ids,
statuses, notice queue, `ToolContext` additions, `Task*` events,
`ToolDetails::Bash.task_id`, drain points (drain logic testable with a
scripted provider: queue a notice, assert the injected user message
and its `MessageStart`/`MessageEnd` pair). No tool uses it yet.

**Stage B — background bash + read/wait/kill/list + wake.** Extract
the bash driver, implement the launch window, `TaskOutputSource` over
`StreamState` (always-persisted spill), the four `task_*` tools,
`Agent::wake`, and the binary's two wake triggers. End-to-end: start,
poll, wait, kill, notice-on-completion, idle owner wakes and reacts.

**Stage C — background agent spawn.** `SpawnMode` plumbing, detached
run driver, usage folding at drain, started outcome. Verify the
steering-spec gating (submit-to-running-sub refused) holds for
background initial runs, and that wake works for sub-agent owners in
their own chats.

**Stage D — TUI.** Pump task tracking, footer aggregate, picker
entries + kill action, live cell updates off `TaskOutput`, frozen
badge off `TaskEnd`.

**Stage E — lifecycle polish.** Shutdown ordering + grace, Ctrl+C
arming text, print-mode kill-at-exit, tool-description notes
(daemons, print mode), docs touch-up.

---

## 9. Testing

- **aj-agent**: registry insert/read/kill/snapshot; cursor semantics
  incl. eviction marker; notice drain at both injection points (idle →
  prompt-start; mid-run → post-batch) with persistence-shaped
  `MessageEnd`s; suppressed notice after cursor-at-EOF read;
  `Agent::wake` drains-then-runs with full event bracketing and
  returns `Empty` (no events) on an empty queue; task ids monotonic;
  shutdown cancels all task tokens.
- **aj-tools**: launch-window fast-exit returns foreground shape;
  started shape carries id + window output; `task_output` incremental
  reads (twice → second read empty), truncation markers, unknown id;
  `task_wait` any-of / timeout-not-error / turn-cancel; `task_kill`
  kills the process group (no surviving grandchildren); background
  ignores `timeout`.
- **agent tool**: background spawn returns started; report arrives as
  a notice; `SubAgentEnd` still emitted; usage folded at drain;
  killing the task cancels the child's run.
- **aj (pump/UI/wake)**: footer counts agents + bash tasks separately;
  cell live-updates off `TaskOutput` and freezes on `TaskEnd`; picker
  shows and kills tasks; resume renders persisted launch cells without
  a registry; `TaskEnd` with an idle owner spawns a wake turn; a busy
  owner defers to the turn-completion trigger; wake vs. user-submit
  gating; Ctrl+C cancels a viewed wake turn.
