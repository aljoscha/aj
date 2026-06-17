# Parallel tool execution — spec

Run the tool calls of a single assistant turn **concurrently** when the
tools opt in, instead of strictly one at a time. The model already
emits multiple `tool_use` blocks in one message; today the agent runs
them serially. This wires up the dormant `ExecutionMode` enum so that
read-only / side-effect-light tools (`read_file`, `agent`, the `task_*`
tools, todos) run in parallel, while state-mutating tools (`bash`,
`write_file`, `edit_file`, `edit_file_multi`) stay serial and act as
ordering barriers.

The headline win is **parallel foreground sub-agents**: two `agent`
tool calls in one batch each spawn and drive a child concurrently,
instead of the first blocking the second.

This document is the implementation contract. It is split into stages
at the end for orchestration.

---

## 1. Background: what happens today

### 1.1 Tool batches run strictly serially

`Agent::execute_turn` (`src/aj-agent/src/lib.rs`) collects the
`tool_use` blocks off the finalized assistant message
(`lib.rs:1295`) and drains them through a sequential loop
(`lib.rs:1340`):

```rust
let mut pending = tool_calls.into_iter();
while let Some((tool_id, tool_name, tool_input)) = pending.next() {
    // emit ToolExecutionStart, before-hook,
    // select!(cancel | execute_tool), after-hook,
    // finalize_tool_result   ← pushes the tool_result to the transcript
}
```

Each `execute_tool(...).await` completes and its `ToolResult` is
appended before the next call starts. Result ordering falls out of
the serial loop: `[assistant(calls...), result_0, result_1, ...]` in
emission order.

### 1.2 `ExecutionMode` exists but is never consulted

`ExecutionMode { Sequential, Parallel }` (`tool.rs:42`, default
`Parallel`) is declared by the tool trait
(`ToolDefinition::execution_mode`, `tool.rs:607`) and stored on
`ErasedToolDefinition` (`tool.rs:648`, `660`). Tools override it:
`bash`, `write_file`, `edit_file`, `edit_file_multi` return
`Sequential`; everything else keeps the `Parallel` default. **No code
in `aj-agent` reads `execution_mode`** — the batch loop is
unconditionally serial. The enum is dead-but-declared metadata.

### 1.3 The `&mut SessionState` borrow is what serializes foreground tools

`execute_tool` takes `&mut self` (`lib.rs:1785`) so it can build the
`SessionContextWrapper` around `session_ctx: &mut self.session_state`
(`lib.rs:1812`). Because each tool call needs an exclusive `&mut`
borrow of the agent's `SessionState`, two tool futures cannot be
in flight at once — this is the borrow-checker-level wall.

This is already documented as the reason background tasks exist: the
`start_background_task` contract notes that "ids are allocated by the
registry, not by session state, so detached drivers never need the
`&mut SessionState` borrow that makes foreground tools block"
(`tool.rs:741`).

Everything *else* the wrapper needs is already cheaply cloneable and
concurrency-safe: `provider` (`Arc<dyn Provider>`, `Send + Sync`,
independent stream per call), `parent_bus` (`EventBus::emit` snapshots
listeners under a short lock and awaits them outside it), and the
sibling registries `task_registry` / `sub_agent_registry` /
`message_queues` (all `Arc<Mutex<…>>` wrappers whose locks are never
held across `.await`).

### 1.4 What `SessionState` actually holds

```rust
pub struct SessionState {
    working_directory: PathBuf,          // read-only after construction
    todo_list: Vec<TodoItem>,            // read/write by todo tools
    turn_counter: usize,                 // loop-private (mutated by execute_turn)
    accumulated_usage: Usage,            // loop-private
    sub_agent_counter: usize,            // bumped by spawn_agent
    sub_agent_usage: HashMap<usize, Usage>, // written by spawn_agent / notice drain
}
```

The fields tools touch through `ToolContext` are `working_directory`
(read), `todo_list` (read/write), `sub_agent_counter` (bump via
`next_sub_agent_id`), and `sub_agent_usage` (insert via
`record_sub_agent_usage`). `turn_counter` and `accumulated_usage` are
mutated only by the run loop under `&mut self`.

---

## 2. Design

Three pieces: (a) make the tool-visible session state a shared,
internally-synchronized handle so `execute_tool` can take `&self`;
(b) partition each batch into contiguous concurrency groups and run
each group through a bounded concurrent pool, finalizing results in
original order; (c) warn the model, on the `agent` tool, that parallel
agents share the filesystem.

### 2.1 `SessionState` becomes a cheaply-cloneable handle

Convert `SessionState` into a `#[derive(Clone)]` handle wrapping
`Arc<std::sync::Mutex<SessionStateInner>>`, mirroring `TaskRegistry`,
`SubAgentRegistry`, and `MessageQueues` exactly. All current fields
move into `SessionStateInner`. Every accessor takes `&self` and locks
internally for the duration of a short, non-`await` map/field
operation.

- `working_directory(&self) -> PathBuf` — unchanged signature (already
  returns owned).
- `get_todo_list(&self) -> Vec<TodoItem>` / `set_todo_list(&self, …)` —
  `set_*` drops its `&mut`.
- `next_sub_agent_id(&self) -> usize`, `seed_sub_agent_counter(&self, …)`,
  `record_sub_agent_usage(&self, …)` — all drop their `&mut`.
- `turn_counter(&self) -> usize` plus a new `bump_turn_counter(&self)`
  to replace the direct `self.session_state.turn_counter += 1`
  (`lib.rs:1015`).
- `accumulated_usage(&self) -> Usage` — **returns owned** (was `&Usage`);
  `Usage` is a handful of `u64`s, cloning is trivial. Add
  `accumulate_usage(&self, delta: &Usage)` to replace the direct
  `accumulate_usage(&mut self.session_state.accumulated_usage, …)`
  (`lib.rs:1336`) and the field reads at `lib.rs:1319-1326`.
- `sub_agent_usage(&self) -> HashMap<usize, Usage>` — **returns owned**
  (was `&HashMap`).

`std::sync::Mutex` (not `tokio::sync::Mutex`): accessors never await
while holding the lock, matching the three sibling registries. The
lock is per-handle and held only for trivial ops, so contention is a
non-issue.

`Agent` keeps a `SessionState` field; its public delegating accessors
(`Agent::accumulated_usage`, `Agent::sub_agent_usage`, `lib.rs:432`,
`440`) change their return types from `&T` to owned `T`. The one
external caller, `build_usage_summary_from_parts` in
`src/aj/src/modes/interactive/shutdown.rs:31`, takes the values
by reference at the call site, so adapt it to bind the owned values
first (or change the helper to take by value). Tests in `lib.rs` that
call `agent.sub_agent_usage()` adapt to the owned return.

> NOTE: This puts `turn_counter` / `accumulated_usage` behind the same
> lock even though only the loop touches them. That is deliberate — one
> uniform handle matching the sibling registries is a cleaner boundary
> than splitting "loop-private" from "tool-visible" session state into
> two types, and the lock cost is negligible.

### 2.2 The wrapper holds a handle, `execute_tool` takes `&self`

`SessionContextWrapper` changes its `session_ctx: &'a mut SessionState`
field to a cloned `SessionState` handle (owned, like the registry
fields it already holds). The wrapper keeps its other `&'a` shared
borrows (`env`, `disabled_tools`) — multiple concurrent wrappers
holding shared `&self.env` borrows is fine.

`execute_tool` changes to `&self`:

```rust
async fn execute_tool(&self, call_id: &str, tool_name: &str,
                      tool_input: serde_json::Value) -> Result<ToolOutcome, anyhow::Error>
```

It builds the wrapper from `&self` (clone the `SessionState` handle and
the registry handles, share `&self.env` / `&self.disabled_tools`). N
such futures can be alive at once because each holds only shared `&self`
borrows plus owned handle clones.

**The `ToolContext` trait does not change.** Its `set_todo_list`,
`spawn_agent`, `emit_update`, and `start_background_task` methods keep
`&mut self`: each concurrent tool call gets its **own** wrapper
instance, so `&mut wrapper` is exclusive per-call even when many run
concurrently. Tool implementations are untouched. Inside
`spawn_agent`, `self.session_ctx.next_sub_agent_id()` becomes
`self.session_state.next_sub_agent_id()` against the handle's `&self`
accessor.

### 2.3 Contiguous grouping + bounded concurrent execution

Replace the serial drain in `execute_turn` with: partition → run each
group concurrently (capped) → finalize in order.

**Partition.** Walk the ordered tool calls, look up each tool's
`execution_mode` on its `ErasedToolDefinition`, and coalesce into
contiguous groups: a maximal run of `Parallel` tools is one group; each
`Sequential` tool is its own singleton group. Order across groups is
preserved, so a `Sequential` tool is a barrier between the parallel
runs around it.

```
calls:  [read, read, bash, agent, agent]
modes:  [ Par,  Par,  Seq,  Par,   Par ]
groups: [ {read, read} ] [ {bash} ] [ {agent, agent} ]
```

A tool whose name is unknown or whose input failed to parse is treated
as `Sequential` (conservative: it becomes its own barrier group; the
error outcome is synthesized as today).

**Execute.** For each group in order, run its calls through a bounded
concurrent pool that **preserves order**: build one future per call and
drive them with `futures::stream::iter(...).buffered(cap)` (`buffered`,
not `buffer_unordered`, so collected outcomes stay in call order).
Collect the group's outcomes, then finalize each in order. Because
groups run one at a time and a group is fully finalized before the next
starts, a `Sequential` singleton never overlaps anything, and overall
transcript order equals original call order regardless of which futures
finish first.

The per-call future (`run_tool_call(&self, …) -> ToolRunResult`)
performs the side-effect-free half of today's loop body and returns the
outcome without touching the transcript:

1. emit `ToolExecutionStart`,
2. consult the before-tool-call hook (may rewrite input or
   short-circuit),
3. `tokio::select! { biased; cancel.cancelled() => aborted, res = execute_tool(...) => res }`,
4. consult the after-tool-call hook (skipped on abort),
5. return `{ call_id, tool_name, outcome, aborted }`.

`finalize_tool_result` stays `&mut self` and is called from the loop,
in order, after each group's outcomes are collected — it builds the
`ToolResultMessage`, pushes it to `self.transcript`, and emits
`MessageStart` / `MessageEnd` / `ToolExecutionEnd`.

**Cap.** A single tunable read once at agent construction (or via a
small helper), `max_tool_concurrency()`:
`AJ_MAX_TOOL_CONCURRENCY` parsed as a positive integer if set, else a
built-in default of **8**. Groups run sequentially, so this caps
per-turn concurrency. Nested sub-agents each cap their own turn — same
per-agent model as the reference implementation, no global pool.

> NOTE: `buffered(cap)` keeps up to `cap` futures in flight and yields
> results in input order, which gives us bounded concurrency and
> deterministic transcript ordering in one combinator. We do not need
> `FuturesUnordered` or a hand-rolled pool.

### 2.4 Cancellation

The transcript invariant is unchanged: every `tool_use` gets a matching
`tool_result`, in original order, and the turn returns
`TurnError::Aborted`.

Within a group, each `run_tool_call` already races `execute_tool`
against the cancel token. Once cancel fires, in-flight calls drop their
`execute_tool` future and return a cancelled outcome, and any calls
`buffered` starts afterward see the biased `select!` short-circuit
immediately (they never run `execute_tool`). So a cancelled group still
yields one outcome per call, in order.

After finalizing a group, if any call in it aborted, synthesize
cancelled outcomes for **all calls in every remaining group**, in
order — emitting each one's `ToolExecutionStart` then
`finalize_tool_result` with `cancelled_tool_outcome(...)`, exactly as
the current pending-drain does (`lib.rs:1452-1476`) — then return
`TurnError::Aborted`.

### 2.5 Warn that parallel agents share the filesystem

Add a note to the `agent` tool description
(`src/aj-tools/src/tools/agent.rs`) so the spawning model understands
the hazard it now controls:

> When you launch multiple agents in parallel (multiple `agent` calls
> in one message, or several background agents), they run concurrently
> against the **same working directory and filesystem**. There is no
> isolation or locking between them: if two agents write or edit the
> same files, or use the same scratch paths, they will clobber each
> other. Only parallelize agents whose work is independent, give each
> agent its own scratch paths, and keep conflicting edits to a single
> agent (or run them sequentially).

Also fix the stale module comment in `agent.rs:23-26`, which claims
parallel batching already happens "because each one gets a fresh
sub-agent" — that becomes true with this change, so the comment should
describe the actual mechanism (contiguous `Parallel` grouping) rather
than asserting nothing interleaves on shared state.

This is the only behavioral guard. Per the design discussion we
**accept** that concurrent agents can clobber each other and rely on
prompt-level guidance rather than a runtime mutation lock or per-agent
worktrees. (This hazard already exists today via background agents,
which run `bash` concurrently with no cross-agent lock; this change
makes it reachable from foreground batches too.)

### 2.6 Update the `ExecutionMode` doc

The enum doc (`tool.rs:35`, `44`) currently describes all-or-nothing
semantics ("Forces serial execution of the whole batch when any tool in
the batch picks this mode"). Rewrite it to describe contiguous
grouping: a `Sequential` tool runs alone and acts as a barrier; a
maximal contiguous run of `Parallel` tools executes concurrently.

---

## 3. Non-goals

- **Input-dependent concurrency classification.** The reference
  implementation classifies each `bash` invocation as parallel-safe iff
  its parsed command is read-only. We keep the static per-tool
  assignment (`bash` is always `Sequential`). A command classifier is a
  deliberate future enhancement, not in scope.
- **Cross-agent mutation safety.** No global filesystem lock, no
  per-agent worktree/cwd sandbox. See §2.5.
- **System-prompt batching guidance.** Anthropic requests multiple tool
  calls per response by default, so parallelism is reachable without
  prompt changes. Explicitly steering the model to batch independent
  calls is a possible follow-up, out of scope here.
- **Parallelizing across turns or across agents from one scheduler.**
  Each agent caps and schedules its own turn; sub-agent concurrency
  continues to come from separate `Agent` handles on their own tokio
  tasks.

---

## 4. Testing

Unit (in `aj-agent`, `#[cfg(test)]`):

- **Grouping.** A pure partition helper over `(name, mode)` lists:
  `[Par, Par, Seq, Par]` → `[[0,1],[2],[3]]`; all-`Par` → one group;
  all-`Seq` → N singletons; unknown/parse-error tool → singleton.
- **Ordering under skew.** Two `Parallel` tool calls where the first
  future resolves slower than the second; assert the transcript lands
  `result_0` before `result_1` (original order), proving `buffered`
  ordering.
- **Actual concurrency.** Two `Parallel` calls that each wait on a
  shared `tokio::sync::Barrier`/`Notify` requiring both to be in flight
  to complete; the turn finishes only if they truly overlap (a serial
  runner would deadlock/time out).
- **Barrier.** `[read(Par), bash(Seq), read(Par)]` — assert the `bash`
  call does not overlap either read (e.g. a shared "currently running"
  counter never exceeds 1 while bash runs) and results stay ordered.
- **Cap.** With `AJ_MAX_TOOL_CONCURRENCY=2` and a group of 4
  instrumented Parallel calls, peak in-flight count is 2.
- **Cancellation.** Cancel mid-group: every call (in the cancelled
  group and all later groups) gets a `tool_result`, in order, none
  dangling, and the turn returns `Aborted`.
- **Parallel foreground agents.** Two `agent` calls in one batch each
  run a child to completion concurrently; both reports land, in order;
  `sub_agent_usage` records both.

Integration / regression:

- Existing serial-behavior tests (single tool call, sequential
  `bash`/edit batches) still pass unchanged.

---

## 5. Touch points

- `src/aj-agent/src/lib.rs`
  - `SessionState` → handle (`Arc<Mutex<SessionStateInner>>`), `&self`
    accessors, owned returns for `accumulated_usage` / `sub_agent_usage`,
    `bump_turn_counter`, `accumulate_usage`.
  - `SessionContextWrapper.session_ctx` → owned `SessionState` handle;
    `spawn_agent` body uses the handle.
  - `execute_tool` → `&self`.
  - `execute_turn` batch loop → partition + `buffered(cap)` +
    ordered finalize + cancellation drain. Extract `run_tool_call`
    (`&self`) for the side-effect-free half.
  - `max_tool_concurrency()` helper (env `AJ_MAX_TOOL_CONCURRENCY`,
    default 8).
  - `Agent::accumulated_usage` / `Agent::sub_agent_usage` return owned.
- `src/aj-agent/src/tool.rs` — `ExecutionMode` doc rewrite (contiguous
  grouping). Grouping reads `ErasedToolDefinition::execution_mode`,
  which already exists.
- `src/aj-tools/src/tools/agent.rs` — description warning; fix stale
  module comment.
- `src/aj/src/modes/interactive/shutdown.rs` — adapt to owned
  `accumulated_usage` / `sub_agent_usage` returns.

`futures` is already a dependency of `aj-agent` (`Cargo.toml:11`), so
`stream::iter(...).buffered(...)` needs no new crate.

---

## 6. Stages (for orchestration)

1. **Session-state handle.** Convert `SessionState` to the
   internally-locked handle with `&self` accessors; update the
   wrapper field, `spawn_agent`, the loop's counter/usage call sites,
   the public `Agent` accessors, and `shutdown.rs`. `execute_tool`
   becomes `&self`. No behavior change yet — the loop is still serial.
   Build + existing tests green.
2. **Grouping + concurrent execution.** Add the partition helper and
   `max_tool_concurrency()`, extract `run_tool_call`, and rewrite the
   batch loop to group → `buffered(cap)` → ordered finalize, including
   the cancellation drain over remaining groups. Add the unit/regression
   tests in §4.
3. **Prompt + docs.** `agent` tool warning, `agent.rs` module comment,
   `ExecutionMode` doc rewrite.

Stage 1 is the structural refactor and the only intricate borrow work;
stage 2 is the feature; stage 3 is text. Stages 2 and 3 depend on 1.
