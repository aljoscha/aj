# Sub-agent steering — spec

Turn the sub-agent view from **observe-only** into a fully interactive
view: when the user switches the chat view to a sub-agent and submits a
prompt, the prompt goes to *that* sub-agent, and its turn runs
concurrently with whatever the main agent (or other sub-agents) are
doing. The sub-agent view becomes the main view in every respect except
which agent it is wired to.

This supersedes the "steering a sub-agent from the picker
(observe-only)" non-goal in `docs/subagent-observability-spec.md`; the
rendering, picker, footer and box machinery built there are reused
unchanged.

This document is the implementation contract. It is split into stages
at the end for orchestration.

---

## 1. Background: what happens today

### 1.1 Sub-agents are ephemeral

`SessionContextWrapper::spawn_agent` (`src/aj-agent/src/lib.rs`) builds
a fresh `Agent` on the stack, shares the parent's bus + a child
cancellation token, runs it to completion via `run_single_turn`, reads
its usage, emits `SubAgentEnd`, and returns `SpawnedAgent { agent_id,
report }`. The `Agent` is a local: once `spawn_agent` returns it is
**dropped**. Nothing — not `SessionState`, not the parent, not the
binary — retains a handle. The only survivors are the report string
(handed to the parent model as the `agent` tool result), the persisted
sub-thread on disk, and the recorded usage.

The whole sub-agent run happens *inside* the parent's tool call:
`execute_turn` → `execute_tool` → `spawn_agent().await`. While it runs,
the parent agent is suspended at that `.await`, and the binary's spawned
turn task holds the main agent's `TokioMutex` the entire time.

### 1.2 The binary drives exactly one turn at a time

`src/aj/src/modes/interactive.rs`:

- One `agent: Arc<TokioMutex<Agent>>` (the main agent).
- `running_task: Option<JoinHandle<Result<(), TurnError>>>` holds the
  single in-flight turn; `current_turn_cancel: Option<CancellationToken>`
  is the binary's clone of that turn's cancel token.
- The submit handler (interactive.rs:1232-1337) ignores submits while
  `running_task.is_some()`, otherwise spawns a task that locks the agent
  and calls `a.prompt(text, turn_cancel)`. It **always** targets the
  main agent — `pump.active_view(...)` is never consulted.
- The editor's submit is disabled globally while a turn runs
  (`set_editor_submit_enabled(false)`).
- Ctrl+C (interactive.rs:809-822) cancels the single turn if one runs,
  else exits.
- The `select!` (interactive.rs:664-720) awaits the single
  `running_task` via a `task_done` future that pends forever when no
  task is running.

### 1.3 The pump already tracks "who is running", with caveats

`src/aj/src/modes/interactive/event_pump.rs`:

- `running_agents: HashSet<AgentId>` is driven by `AgentStart` /
  `AgentEnd` and gates the single global loader/spinner (start on
  empty→nonempty, stop on nonempty→empty).
- `running_sub_agents: usize` is driven by `SubAgentStart` /
  `SubAgentEnd` and gates the footer `N agent(s) (alt+a)` indicator.
- Two heuristics exist purely because today sub-agents only ever run
  *inside* the main turn: `AgentStart{Main}` clears the running set
  (resync), and `AgentEnd{Main}` is treated as the authoritative "all
  activity stopped" and zeroes both `running_agents` and
  `running_sub_agents`.

Those two heuristics are **wrong** once a sub-agent can run
independently of the main turn (a main turn can start or end while a
sub-agent's own turn is still running).

### 1.4 Persistence already chains continuations

`src/aj-session/src/listener.rs`: for `AgentId::Sub(n)` the listener
anchors a `MessageEnd` on `latest_leaf(subagent(n))` if that thread has
entries, falling back to the `SubAgentStart` anchor only for the *first*
write. A re-prompt emits no new `SubAgentStart`, so its messages chain
onto the existing sub-thread leaf automatically — exactly the "keep
adding to the same on-disk sub-thread" behavior we want. No listener
change is required for continuation.

### 1.5 Concurrency model already in place

`EventBus::emit` (`src/aj-agent/src/bus.rs`) snapshots the listener list
under a short lock, releases it, then awaits each listener. Concurrent
emits from two agent tasks run their listener futures concurrently; the
persistence listener serializes real writes behind the log's
`TokioMutex`, and the channel-forward listener does a lock-free
`mpsc::send`. The binary consumes that single mpsc on its main loop, so
**all** UI/pump mutation stays single-threaded and serialized; the only
true concurrency is between agent turn tasks on distinct `Agent`
mutexes. This is the property that makes concurrent turns tractable.

---

## 2. Goals / non-goals

**Goals**

1. Submitting in the sub-agent view sends the prompt to that sub-agent
   and runs its turn.
2. Main and sub-agent turns (and multiple sub-agent turns) run
   **concurrently**; each agent still runs at most one turn at a time.
3. Continuations append to the sub-agent's existing on-disk sub-thread.
4. The working **spinner is scoped to the viewed agent** — it shows the
   activity of whichever agent the chat view is on, not global activity.
5. The architecture is a clean foundation for the future
   **background-spawn** flow (main spawns a sub-agent, keeps going, and
   is later notified of the result).

**Non-goals (this iteration)**

- **Option A semantics**: a sub-agent continuation is an independent
  conversation. The parent model is **not** automatically updated with
  anything the sub-agent produces after its original `agent` tool call
  returned. (A future "send report to parent" action is out of scope.)
- **Re-prompting resumed sub-agents**: only sub-agents spawned in the
  *current process* are re-promptable (they have a live handle).
  Sub-agents reconstructed by `/resume` render but are not re-promptable
  until we add transcript rehydration (§8, open question).
- **Non-blocking spawn**: the initial `agent` tool call still blocks the
  parent and returns the first report inline (unchanged contract). Only
  *user-initiated continuations* are concurrent. Background spawn is
  designed-for but not built (§7).
- Nested sub-agents (still one level; the `agent` tool stays filtered
  out of sub-agent toolsets).

---

## 3. Locked decisions

1. **Option A.** Continuations are side conversations; the parent model
   is untouched. The editor marker stays `agent N` (already shipped) —
   it now means "you are talking to agent N", which is accurate.
2. **Per-agent single turn, cross-agent concurrent.** Each `Agent` has
   its own `TokioMutex`; a turn locks only its own agent. Two turns on
   the same agent can never overlap (one transcript, `&mut self`).
3. **Retain sub-agents in a shared registry.** Sub-agents become
   long-lived `Arc<TokioMutex<Agent>>` held in a registry shared between
   the main `Agent` (which inserts on spawn) and the binary (which
   resolves them to drive continuations).
4. **The pump's running set becomes the single source of truth** for
   "what is running", driven by `AgentStart`/`AgentEnd` per `AgentId`.
   The per-view spinner, the aggregate footer count, and per-box status
   all derive from it. The Main-authoritative-clear and
   AgentStart{Main}-clear heuristics are removed.
5. **UI stays single-threaded.** Concurrency lives only in agent turn
   tasks; everything reaches the UI through the existing serialized
   mpsc event channel.

---

## 4. Design

### 4.1 Sub-agent registry (`aj-agent`)

Introduce a small, cheaply-cloneable shared registry in `aj-agent`:

```rust
/// A live, re-promptable agent handle.
pub type SharedAgent = Arc<tokio::sync::Mutex<Agent>>;

/// Registry of retained sub-agents, keyed by their `Sub(n)` index.
/// Cloneable; all clones share one map. The inner `std::sync::Mutex`
/// is held only to get/insert a handle (never across `.await`).
#[derive(Clone, Default)]
pub struct SubAgentRegistry {
    inner: Arc<std::sync::Mutex<BTreeMap<usize, SharedAgent>>>,
}

impl SubAgentRegistry {
    pub fn insert(&self, n: usize, agent: SharedAgent);
    pub fn get(&self, n: usize) -> Option<SharedAgent>;
    pub fn ids(&self) -> Vec<usize>;
}
```

Wiring:

- `Agent` gains a `sub_agent_registry: SubAgentRegistry` field
  (default-empty) plus `set_sub_agent_registry(&mut self, reg)`. Only
  the main agent ever needs one set; sub-agents can't spawn (the `agent`
  tool is filtered out), so they never read it.
- `SessionContextWrapper` (built in `Agent::execute_tool`,
  lib.rs:1479-1496) gets a `sub_agent_registry: SubAgentRegistry` field,
  cloned from `self.sub_agent_registry`.
- `spawn_agent` (lib.rs:1625-1735): after building `sub_agent` and
  setting its id/bus/cancellation as today, **retain then run**:

  ```rust
  let shared: SharedAgent = Arc::new(TokioMutex::new(sub_agent));
  self.sub_agent_registry.insert(agent_id, Arc::clone(&shared));
  let (result, usage) = {
      let mut guard = shared.lock().await;
      let result = guard.run_single_turn(task).await;
      let usage = guard.session_state.accumulated_usage.clone();
      (result, usage)
  };
  // ... record usage, emit SubAgentEnd, return SpawnedAgent as today.
  ```

  The handle stays in the registry after the initial run. The parent's
  tool result is still the first report — the `agent` tool contract is
  unchanged.

Notes:
- The registry is injected by the **binary** so the binary and the main
  agent share one map. Callers that never set one (print mode, tests)
  get the default-empty registry; sub-agents are simply retained for the
  lifetime of the main `Agent` and dropped with it — harmless for
  one-shot runs.
- Flat keyspace: `Sub(n)` is global per session and sub-agents don't
  nest, so a `BTreeMap<usize, _>` suffices.

### 4.2 Resolving and driving any agent (binary)

Replace the single-turn machinery in `InteractiveMode::run` with a
per-agent one:

- Keep `agent: Arc<TokioMutex<Agent>>` for the main agent. Add the
  shared `registry: SubAgentRegistry` (the same instance set on the main
  agent). A small resolver:

  ```rust
  fn resolve_agent(
      id: AgentId,
      main: &Arc<TokioMutex<Agent>>,
      registry: &SubAgentRegistry,
  ) -> Option<SharedAgent> {
      match id {
          AgentId::Main => Some(Arc::clone(main)),
          AgentId::Sub(n) => registry.get(n),
      }
  }
  ```

- Replace `running_task` / `current_turn_cancel` with:
  - `turns: JoinSet<(AgentId, Result<(), TurnError>)>` — the set of
    in-flight turn tasks. `JoinSet` keeps panic detection (`join_next`
    yields `Err(JoinError)`), gives completion-as-they-finish, and
    `abort_all` on shutdown.
  - `turn_cancels: HashMap<AgentId, CancellationToken>` — the binary's
    clone of each in-flight turn's cancel token. Its key set is exactly
    "agents the binary is currently driving".

- The `select!` arm that awaited `task_done` now awaits the `JoinSet`:

  ```rust
  joined = join_next_or_pending(&mut turns) => {
      match joined {
          Ok((id, result)) => {
              turn_cancels.remove(&id);
              pump.mark_idle(&mut tui, id);     // reconcile (idempotent)
              // Main-turn completion bounds every *nested* initial
              // spawn it started: those sub-agents are done (or their
              // run was dropped on cancel). Drain any still marked
              // running that the binary is NOT independently driving
              // (∉ turn_cancels) so a leaked sub-agent can't pin the
              // footer/spinner. Independent continuations are in
              // turn_cancels and survive. This is the concurrency-safe
              // replacement for the old "Main AgentEnd drains all subs".
              if id == AgentId::Main {
                  for sub in pump.running_agents() {
                      if matches!(sub, AgentId::Sub(_)) && !turn_cancels.contains_key(&sub) {
                          pump.mark_idle(&mut tui, sub);
                      }
                  }
              }
              sync_editor_enabled(&mut tui, &pump, &turn_cancels, active_view);
              match result {
                  Ok(()) => {}
                  Err(TurnError::Aborted) => notice "turn cancelled" (scoped to id),
                  Err(TurnError::Recoverable(e)) => Error event for id,
                  Err(TurnError::Fatal(e)) => { run_result = Err(e); break; }
              }
          }
          Err(join_err) => { run_result = Err(panicked); break; }
      }
  }
  ```

  `join_next_or_pending` pends forever when the set is empty (mirrors the
  current `task_done` helper). The Main-completion drain above is where
  the old CPU-pegging leak fix (a dropped `AgentEnd(Sub n)` leaving the
  loader's animation pump alive) now lives — concurrency-safe because it
  only drains sub-agents the binary isn't driving.

### 4.3 Submit routing

In the submit handler (interactive.rs:1232-1337), after the slash-command
branch:

```rust
let target = pump.active_view(&mut tui);

// Per-agent single-turn gate: refuse if the target is already busy,
// whether the binary is driving it or it is running its initial spawn
// inside the main turn.
if turn_cancels.contains_key(&target) || pump.is_running(target) {
    continue;
}

let Some(handle) = resolve_agent(target, &agent, &registry) else {
    // Sub-agent has no live handle (e.g. resumed). Surface a notice.
    pump.handle(&mut tui, &notice_event("This agent can't be prompted."));
    continue;
};

// Clear editor + history as today, then disable submit only if the
// target is the active view (it always is here, but keep it explicit).
let turn_cancel = CancellationToken::new();
turn_cancels.insert(target, turn_cancel.clone());
let run_config_for_turn = Arc::clone(&run_config);
turns.spawn(async move {
    let mut a = handle.lock().await;
    { /* apply run_config: set_provider + set_default_thinking */ }
    let result = a.prompt(trimmed, turn_cancel).await;
    (target, result)
});
sync_editor_enabled(&mut tui, &pump, &turn_cancels, target);
```

Notes:
- `prompt` appends the user message and runs, emitting
  `AgentStart`/`AgentEnd` and `MessageStart/End` tagged with `target`'s
  id — so rendering and persistence route automatically.
- Run-config (model/thinking) is applied to the target before each turn,
  same as main, so continuations honor the currently-selected model.
- Slash commands still operate on the **main** agent / session; they are
  not retargeted by the active view (a `/model` change applies session
  wide via run-config). This keeps session-level commands unsurprising.

### 4.4 Editor enable/disable per active view

The editor's submit is enabled iff the **active view's** agent is idle:

```rust
fn sync_editor_enabled(tui, pump, turn_cancels, active: AgentId) {
    let busy = turn_cancels.contains_key(&active) || pump.is_running(active);
    set_editor_submit_enabled(tui, !busy);
}
```

Add `EventPump::is_running(&self, id: AgentId) -> bool` reading the
running-agent set (§4.6). Recompute by calling `sync_editor_enabled`:

- after `set_active_view` (view switch),
- after spawning a turn,
- after a turn completes (JoinSet arm),
- after every `pump.handle(event)` in the bus arm (cheap; catches a
  sub-agent's initial-run `AgentStart`/`AgentEnd` while that sub is the
  active view).

`pump.set_active_view` keeps repainting the transcript; the binary calls
`sync_editor_enabled` right after so the marker + editability match the
new view.

### 4.5 Cancellation (Ctrl+C)

Ctrl+C acts on **the agent you are viewing** — consistent with the
per-view spinner, editor-enable, and submit routing. Generalize the
Ctrl+C arm (interactive.rs:809-822). Priority order:

1. Overlay up → close (unchanged).
2. Login dialog up → cancel login (unchanged).
3. **Viewed agent has a running turn** → cancel just that turn. If the
   binary is driving it (`turn_cancels` contains the active view), fire
   that token. If instead the viewed agent is a sub-agent running its
   *initial* spawn (running per `pump.is_running` but not in
   `turn_cancels`, because that run is owned by the main turn), fire the
   main turn's token (`turn_cancels[Main]`); the child token cascades.
4. **Viewed agent idle, but some other agent is running** → do **not**
   cancel anything. Show `N agent(s) still running — press Ctrl+C again
   to quit` and arm a transient `quit_armed` flag. A second Ctrl+C while
   still in this state exits; shutdown's `turns.abort_all()` tears the
   background turns down then. `quit_armed` is cleared on any other input
   event or state change. (Background sub-agents are thus never
   cancelled by a Ctrl+C aimed at a different view — to cancel one, switch
   to its view and Ctrl+C there.)
5. **Viewed agent idle, nothing running anywhere** → exit immediately
   (today's behavior).

`Ctrl+D` exits unconditionally (unchanged). On shutdown, `turns
.abort_all()` replaces the single `running_task.abort()`.

### 4.6 Per-view spinner, footer, and status (pump)

Make `running_agents: HashSet<AgentId>` the single source of truth for
"what is running", driven by `AgentStart`/`AgentEnd` per `AgentId`, and
derive three things from it.

`AgentStart{id}` inserts `id`; `AgentEnd{id}` removes it. **Drop** the
two heuristics in `handle` that only held under the old nested model:
the `AgentStart{Main}` resync-clear and the `AgentEnd{Main}`
authoritative drain. Under concurrency a main turn can legitimately
start or end while a sub-agent turn runs, so neither may touch other
agents' entries.

**Spinner — scoped to the viewed agent.** The single `Loader` in the
`Status` slot (idx 2, between Chat and Editor) reflects *the agent
currently being viewed*, not global activity: active iff `active_view ∈
running_agents`. Replace the old empty↔nonempty start/stop calls with a
single recompute:

```rust
fn sync_loader(&self, tui: &mut Tui) {
    let active = self.active_view(tui);          // reads ChatView::active()
    let should_run = self.running_agents.contains(&active);
    self.with_loader(tui, |l| match (should_run, l.is_active()) {
        (true, false) => l.start(),
        (false, true) => l.stop(),
        _ => {}                                   // no-op: don't reset the animation
    });
}
```

Call `sync_loader` after every `AgentStart`/`AgentEnd` and from
`set_active_view` (so switching views immediately reflects the new
agent's state). The `(_, _) => {}` arm matters: `Loader::start` resets
the frame clock, so we only call it on a genuine idle→running edge to
avoid animation jitter on unrelated events.

This per-view scoping also subsumes the deleted `AgentEnd{Main}`
heuristic for the spinner: when viewing Main the spinner is on iff Main
is running, so a leaked sub-agent entry can no longer pin the Main
view's spinner.

**Footer — aggregate.** The `N agent(s) (alt+a)` indicator stays an
aggregate so the user still sees background activity while viewing an
idle agent. Derive it from the set: `running = running_agents.iter()
.filter(|a| matches!(a, Sub(_))).count()`; show when `> 0`. Delete the
separate `running_sub_agents` counter and its `SubAgentStart`/
`SubAgentEnd` increments. (The initial spawn already emits
`AgentStart(Sub n)` inside `run_single_turn`, so the count covers both
initial runs and continuations.)

**Per-box status.** Drive `SubAgentBox` status from `AgentStart(Sub n)`
→ `Running` and `AgentEnd(Sub n)` → `Done` — this is what flips a
re-prompted box back to `Running` then `Done`, since continuations emit
no `SubAgentStart`/`SubAgentEnd`. `SubAgentStart` still creates the box
+ persistence anchor + initial `Running`; `SubAgentEnd` still carries
the report.

Add three accessors/mutators:
- `pub fn is_running(&self, id: AgentId) -> bool` — membership in the set.
- `pub fn running_agents(&self) -> Vec<AgentId>` — snapshot, so the
  binary can reconcile leaked nested subs on Main-turn completion (§4.2).
- `pub fn mark_idle(&mut self, tui, id)` — removes `id` from the set,
  sets a `Sub(n)` box to `Done`, then `sync_loader` + `sync_agent_indicator`.
  Idempotent w.r.t. the `AgentEnd` the pump already processed.

The pump keeps `running_agents` as the **literal** truth from
`AgentStart`/`AgentEnd`; it does not itself special-case Main. Leak
protection (a dropped `AgentEnd(Sub n)` from a cancelled initial spawn)
is the binary's job via the Main-completion drain in §4.2 — the binary
is the only place that knows which running subs are independent
continuations (in `turn_cancels`) versus nested initial spawns. For the
spinner specifically, per-view scoping already prevents a leaked sub
from pinning the Main view.

### 4.7 Persistence

No listener change for continuations (§1.4): a re-prompt chains onto
`latest_leaf(subagent(n))`. Add a test (§9) that re-prompting a
sub-agent appends to its existing sub-thread (the new user message's
parent is the prior sub-thread leaf, not the parent-anchor).

---

## 5. Edge cases

- **Submit to a busy agent.** Gated in §4.3 (both `turn_cancels` and
  `pump.is_running`). Same "ignore second submit" behavior as today,
  now per-agent.
- **Start race** (submit accepted, `AgentStart` not yet emitted). The
  `turn_cancels` key is inserted synchronously at spawn, so the gate and
  `sync_editor_enabled` see the agent as busy immediately, before the
  bus catches up.
- **Continuation after a replyless failed turn.** If a turn fails before
  producing a retained assistant message, the agent's in-memory
  transcript ends on a `User` message (the errored assistant is emitted
  as a `MessageEnd` for persistence but is *not* pushed to the
  transcript; only `Aborted` partials are pushed — lib.rs:909-937 vs the
  `return` at :1001). Re-prompting then appends another `User` message.
  A sub-agent is a full `Agent` reusing the identical
  `prompt` → transcript → `transcript_to_messages` → provider path, so
  this behaves **exactly** as re-prompting the main agent after a failed
  turn does today (which works in practice). No sub-agent-specific
  handling. If we ever want to harden consecutive same-role user turns
  on the wire, that belongs in `transform_messages` for every agent —
  out of scope here.
- **Sub-agent with no live handle** (resumed session). `resolve_agent`
  returns `None`; surface a notice and keep the view read-only.
- **Main ends while viewing Main with a sub continuation running.** The
  spinner stops (you are viewing Main, now idle); the footer still shows
  the running sub; switching to that sub shows its spinner. Correct
  under per-view scoping (§4.6).
- **Quit with background turns running.** `turns.abort_all()` on
  shutdown; end-of-session summary reads the main agent as today.

---

## 6. Touch list

- `src/aj-agent/src/lib.rs`: `SubAgentRegistry` + `SharedAgent`;
  `Agent.sub_agent_registry` + setter; `SessionContextWrapper`
  registry field; retain-then-run in `spawn_agent`.
- `src/aj/src/modes/interactive.rs`: registry injection on the main
  agent; `turns: JoinSet` + `turn_cancels` replacing
  `running_task`/`current_turn_cancel`; `resolve_agent`,
  `join_next_or_pending`, `sync_editor_enabled`; submit routing;
  generalized Ctrl+C; shutdown `abort_all`.
- `src/aj/src/modes/interactive/event_pump.rs`: drop `running_sub_agents`
  and the two heuristics; derive the aggregate footer from
  `running_agents`; per-view spinner via `sync_loader` (called on
  `AgentStart`/`AgentEnd` and from `set_active_view`); `is_running` +
  `mark_idle`; status from `AgentStart`/`AgentEnd(Sub n)`.
- No change to `agent_picker.rs`, `subagent_box.rs` rendering,
  `footer.rs` API, `listener.rs`, or the wire transform.

---

## 7. Forward compatibility: background spawn

The future flow — main spawns a sub-agent, **keeps going**, and is later
notified of the result — drops out of this design as a localized change,
*not* a rewrite:

- Sub-agents are already retained, individually addressable, bus-shared,
  and driven by the same per-agent turn machinery.
- Background spawn = a non-blocking spawn mode in `spawn_agent`: insert
  the sub-agent into the registry, spawn its initial run as a turn task
  (the binary's `turns`/`turn_cancels` machinery, or an agent-internal
  task), and return a "started" tool result to the parent immediately
  instead of blocking on the report.
- Notification = on completion, emit an event the binary surfaces (the
  existing `SubAgentEnd` already carries the report); a later iteration
  can optionally feed that back into the parent as a new user/tool
  message to "wake" it. That parent-feedback step is the only genuinely
  new mechanism, and Option A deliberately leaves the parent untouched
  until we build it.

So this iteration's retention + concurrency is the foundation; background
spawn is an additive mode on top.

---

## 8. Resolved decisions

1. **Ctrl+C is per-view** (§4.5): it cancels the *viewed* agent's turn if
   one is running, otherwise — if other agents are still running — arms a
   `press Ctrl+C again to quit` guard (without cancelling them), and
   otherwise exits. Background sub-agents are never cancelled by a Ctrl+C
   aimed at another view.
2. **Resumed sub-agents are observe-only** this iteration (no live
   handle; no rehydration). Re-prompting them is future work.
3. **Replyless failed-turn continuation** (§5): **no special handling**.
   A sub-agent re-prompt shares the main agent's `prompt`/transcript/
   wire path, so it behaves identically to re-prompting the main agent
   after a failed turn — which works today. Any future hardening of
   consecutive same-role user turns belongs in the shared wire transform.

---

## 9. Staged implementation plan

**Stage A — retention (aj-agent).** Add `SharedAgent`,
`SubAgentRegistry`, the `Agent` field + setter, the
`SessionContextWrapper` field, and retain-then-run in `spawn_agent`.
No behavior change yet (registry unused by the binary). Unit test:
after a spawn, the registry holds the sub-agent and its transcript ends
on the report.

**Stage B — pump concurrency + per-view spinner.** Make
`running_agents` authoritative: remove `running_sub_agents` and the Main
heuristics, derive the aggregate footer from the set, add `sync_loader`
(per-view spinner), `is_running` + `mark_idle`, and drive box status
from `AgentStart`/`AgentEnd(Sub n)`. Adapt the existing pump tests (the
"Main AgentEnd authoritative" / "AgentStart{Main} resync" tests change
meaning) and add: concurrent overlap (main + sub running together);
footer count tracks the set; the spinner follows the viewed agent and
toggles on `set_active_view`.

**Stage C — per-agent drive loop (binary).** Replace
`running_task`/`current_turn_cancel` with `turns: JoinSet` +
`turn_cancels`; add `resolve_agent`, `join_next_or_pending`,
`sync_editor_enabled`; inject the registry on the main agent. No new
user-facing path yet — main still submits to main, but through the new
machinery. Verify no regression to a single main turn (cancel, errors,
panic, shutdown).

**Stage D — submit routing + editability + Ctrl+C.** Route submit to
`pump.active_view`, gate per-agent, wire `sync_editor_enabled` at the
four recompute points, and generalize Ctrl+C. This is the stage that
turns on the feature.

**Stage E — polish + tests.** Resumed-sub notice; end-to-end test that
submitting in a sub-view runs the sub-agent's turn concurrently with a
main turn and persists onto the existing sub-thread.

---

## 10. Testing

- **aj-agent**: registry retention after spawn; transcript shape.
- **aj-session**: re-prompt chains onto the existing sub-thread leaf
  (parent of the continuation's first user message is the prior
  sub-thread leaf, not the original parent anchor).
- **event_pump**: aggregate footer count derives from the running set;
  the spinner follows the viewed agent (on iff `active_view` is running)
  and toggles across `set_active_view`; box status flips
  `Running`→`Done`→`Running`→`Done` across a continuation;
  `is_running`/`mark_idle` behavior.
- **interactive (integration)**: submit in a sub-view drives that
  sub-agent; concurrent main + sub turns both render and persist;
  per-agent submit gating; Ctrl+C scoping; resumed-sub and failed-init
  guards.
