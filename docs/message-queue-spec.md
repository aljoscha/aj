# Queued messages — steering & follow-up — spec

Status: proposed.

Let the user type a message while an agent is working. The message is
**queued** and shown above the editor, signalling it will be sent when
the turn is done. The same gesture set also lets the user **steer** a
running turn — inject a more urgent message right after the next tool
call — and pull a queued message back into the editor to edit it.

This implements the two queues sketched in `docs/aj-next-plan.md` §1.9
("Steering and follow-up queues") and finally wires the
already-defined-but-unemitted `AgentEvent::QueueUpdate` event
(`src/aj-agent/src/events.rs`). It applies to the main agent **and** to
sub-agents (which are fully promptable per
`docs/subagent-steering-spec.md`): everything is keyed by `AgentId` and
targets the agent the chat view is observing.

This document is the implementation contract; stages are at the end.

---

## 1. Background: what exists today

### 1.1 The protocol is half-built

`AgentEvent::QueueUpdate { agent_id, steering, follow_up }`
(`events.rs:291`) is defined with a frozen JSON wire shape but is
**never emitted**. The only references are a match-arm label
(`lib.rs:3199`) and a no-op pump arm (`event_pump.rs:709`). The `Agent`
has no queue fields and no `steer`/`follow_up` methods. §1.9 of the plan
specifies the intended semantics:

- **Steering** drains at each turn boundary — after the assistant's tool
  batch completes, before the next inference. Injected with full prior
  context.
- **Follow-up** drains when the agent would otherwise exit the loop;
  continues the run instead of returning from `prompt`.
- Both queues use interior mutability because the user mutates them from
  a different code path than the one driving `prompt`. `QueueUpdate`
  fires on every change with full snapshots.

### 1.2 The notice queue is the precedent

`TaskRegistry` (`src/aj-agent/src/tool.rs`) is an `Arc`-backed shared
registry with a `Mutex` inside, mutated off the agent lock by detached
task drivers and drained by the agent at fixed points while it holds
`&mut self`. It is injected into the `Agent` via `set_task_registry`
(`lib.rs:329`), shared with sub-agents in `spawn_agent`
(`lib.rs:2426`), and its drivers emit `Task*` bus events off-lock. The
message queues are built the same way; the steering/follow-up drain is a
direct analog of `drain_task_notices` (`lib.rs:1345`).

### 1.3 The drain points

Steering must be injected mid-turn, so it drains inside `execute_turn`
(`lib.rs:848`) at the point that already drains task notices: **after
the tool batch, before the next inference** (`lib.rs:1318`) — "right
after the next tool call".

Follow-up (and any end-of-turn steering left over) is delivered through
the **wake mechanism** the background-tasks work already built
(`docs/background-tasks-spec.md`): when a turn finishes, the binary
re-enters an idle agent to drain pending work without waiting for the
user. The turn-completion wake trigger (`interactive.rs:1095`) and
`spawn_wake_turn` (`interactive.rs:1970`) drive `Agent::wake`
(`lib.rs:621`), which drains at the run-top point beside
`drain_task_notices` (`lib.rs:702`) and is a no-op when there is nothing
to drain. Reusing wake — rather than a second in-loop drain at loop
exit — means follow-up delivery is immune to the
`should_stop_after_turn` break (`lib.rs:1314`, which exits before the
in-loop drain) and to a message enqueued in the window between loop exit
and `AgentEnd`: both land at the single idle-transition point the wake
trigger already guards.

### 1.4 Submit, busy-gate, and the editor

- The submit handler (`interactive.rs:1736`) resolves the target as
  `world.pump.active_view(...)`, refuses the submit while that agent is
  busy (`turn_cancels.contains_key(target) || pump.is_running(target)`),
  otherwise spawns a turn task that calls `agent.prompt(text, cancel)`.
  It never re-locks the agent on the loop.
- `sync_editor_enabled` (`interactive.rs:2006`) disables the editor's
  submit while the active view is busy via `set_editor_submit_enabled`
  (`event_pump.rs:1246`).
- The editor produces submitted text on Enter; the host drains it with
  `take_submitted_prompt` (`event_pump.rs:1238`) after every input
  event.
- `Up` and `Ctrl+P` are both the `tui.editor.cursorUp` action; the
  editor navigates prompt history only when the buffer is empty or the
  cursor is on the first visual line while already browsing
  (`editor.rs:3417`).
- **Alt+Enter is currently a newline.** `is_newline_event`
  (`keys.rs:149`) treats `Enter+ALT` as a byte-form newline fallback for
  terminals that deliver Shift+Enter as `\x1b\r`. See §5.4.
- Host chords (`alt+t`, `alt+o`, `ctrl+v`, `ctrl+o`, `ctrl+r`) are
  intercepted in the input arm *before* `shell.tui.handle_input`
  (`interactive.rs:1302`–`1458`); this spec adds two more.

### 1.5 View scoping is already a pattern

The loader, footer, and editor marker are all rendered for
`EventPump::active_view` and re-synced on `set_active_view`
(`event_pump.rs:408`/`417`); per-agent state is keyed by `AgentId`
(`docs/view-scoped-footer-spec.md`). The pending-message box follows
the same model.

---

## 2. UX contract

At most **one pending message per agent**, with one **kind**
(`Follow-up` or `Steering`). It renders in a box directly above the
editor, for the active view only.

**Composing while the viewed agent is busy:**

- **Enter** appends the editor's text (newline-joined) to the pending
  message and clears the editor. If nothing is pending it creates a
  `Follow-up`. Enter never changes an existing message's kind.
- **Alt+Enter** appends the editor's text and sets the kind to
  `Steering` (promoting an existing `Follow-up`). With an empty editor
  and a pending `Follow-up`, Alt+Enter just promotes it to `Steering`
  (no text added). With nothing pending and text in the editor, it
  creates a `Steering` message directly.
- Kind only escalates (Follow-up → Steering), never the reverse. To
  demote, yank and resubmit (below).
- Empty submits (whitespace only) with nothing pending are no-ops, as
  today.

**Editing a queued message:**

- **Up / Ctrl+P** with an **empty** editor and a pending message yanks
  it: the text moves back into the editor (cursor at end) and the queue
  entry is removed. The yank **forgets the kind** — a subsequent Enter
  re-queues it as Follow-up, Alt+Enter as Steering (decision: reset and
  choose on resubmit). If the message was already drained a moment
  earlier, the yank finds nothing and falls through to normal
  history-up. With a non-empty editor, Up behaves normally (in-editor
  movement / history), so a half-typed draft is never clobbered.

**When the message is sent:**

- A `Follow-up` is sent once the turn ends: the agent re-enters via the
  wake path and runs it as a fresh turn (§3.3). The busy-gate stays set
  across the wake, so the user can keep queueing.
- A `Steering` message is injected right after the current tool batch
  finishes, before the next inference. If the turn ends before any tool
  batch consumes it, it is delivered alongside follow-ups by the wake
  (so it is never lost), just without the mid-turn urgency.
- Injected messages are ordinary user messages: persisted via the
  normal `MessageEnd`, rendered in the transcript like any prompt.

**Idle agent:** Enter and Alt+Enter both start a turn immediately with
the editor text (decision: idle Alt+Enter is a normal prompt — there is
no turn to steer). Queue boxes only ever appear while an agent is busy.

**Display:** the box shows the kind, a one-line hint, and the message
text capped at a small number of lines with a `+N more lines` indicator
(decision: cap with indicator). Hints:

- Follow-up: `queued · sends when the turn ends · ↑ edit · alt+↵ steer`
- Steering: `steering · sends at the next tool call · ↑ edit`

(The `alt+↵ steer` hint is omitted for a steering message, which cannot
escalate further.)

---

## 3. Design — agent layer (`aj-agent`)

### 3.1 The `MessageQueues` handle

A shared, `Arc`-backed, `Clone` handle modeled on `TaskRegistry`:

```rust
/// One agent's pending messages. At most one of the two is non-empty
/// under the TUI's coalescing invariant (§4.2); the Vec shape matches
/// the QueueUpdate wire contract and leaves room for other producers.
#[derive(Default)]
struct AgentQueues {
    steering: Vec<String>,
    follow_up: Vec<String>,
}

pub struct MessageQueues {
    inner: Arc<Mutex<HashMap<AgentId, AgentQueues>>>,
}
```

The handle carries **no event bus** — mirroring `TaskRegistry`, which
is also pure state (only its per-task driver holds a bus). All methods
are `&self`, synchronous, and never held across an `.await`. Emission
of `QueueUpdate` is split by who changed the queue: the **agent** emits
it after it drains (it is mid-turn, holding its own bus), and the
**TUI** re-syncs its own box directly after it enqueues (it holds
`&mut Tui` in the input arm). See §3.4.

**Mutators** (called synchronously from the TUI input arm):

- `append_follow_up(agent, text)` — newline-join `text` onto the agent's
  pending message; create a Follow-up if none exists; never change an
  existing kind.
- `append_steering(agent, text)` — same, but set kind to Steering
  (move any pending Follow-up content into the steering slot first).
- `promote(agent)` — move a pending Follow-up into the steering slot
  with no new text (empty-editor Alt+Enter).
- `take_pending(agent) -> Option<String>` — remove and return the
  pending message's text regardless of kind (yank); `None` if nothing
  pending.
- `clear(agent)` — drop the agent's pending message (§6.1).

**Drain** (called from the agent's turn driver while holding
`&mut self` — the in-loop steering point and the run-top wake drain,
§3.3):

- `drain_steering(agent) -> Vec<String>`
- `drain_follow_up(agent) -> Vec<String>`

Each removes and returns the relevant slot. The agent appends the
drained text as user messages and then emits one `QueueUpdate`
announcing the post-drain state.

**Reads** (no emit): `snapshot(agent) -> QueueSnapshot { kind, text }`
and `has_pending(agent) -> bool` for the TUI box and the wake guard;
`event_messages(agent) -> (Vec<AgentMessage>, Vec<AgentMessage>)` for
the agent's `QueueUpdate` payload.

> NOTE: the kind is derived — Steering if the steering slot is
> non-empty, else Follow-up if the follow-up slot is non-empty, else
> none. The TUI keeps at most one slot populated, so the derivation is
> unambiguous. We keep the two-Vec shape (rather than a single
> `Option<(Kind, String)>`) only to match the frozen `QueueUpdate` wire
> contract; the TUI coalesces to one entry.

### 3.2 Agent field, injection, sub-agent sharing

`Agent` gains `message_queues: MessageQueues` plus
`set_message_queues(...)`, defaulting to a standalone empty handle so
print mode and tests are untouched (mirrors `set_task_registry`).
`spawn_agent` (`lib.rs:2426`) adds
`sub_agent.set_message_queues(self.message_queues.clone())` next to the
task-registry share, so a sub-agent drains its own `AgentId`-keyed
slot.

### 3.3 Drain wiring

**In-loop steering** at `lib.rs:1318` (alongside `drain_task_notices`):
drain `self.message_queues.drain_steering(self.agent_id)` and append
each as a user message via the existing `MessageStart`/`MessageEnd`
bracket (no `<task-notification>` wrapper — these are real user
messages), then fall through to the existing `continue` so the injected
message is in context for the next inference. This is the only drain
that must live in the loop; it is what makes steering urgent.

**Follow-up (and end-of-turn steering) via wake.** `Agent::wake`
(`lib.rs:621`) and the run-top drain in `run_top_level_turn_inner`
(`lib.rs:702`) gain a queued-message drain beside the notice drain:
`drain_steering` then `drain_follow_up` for `self.agent_id`, each
appended as a user message. `wake`'s "nothing to do" guard
(`lib.rs:626`) becomes
`has_notices(id) || message_queues.has_pending(id)`. The binary's
turn-completion wake trigger (`interactive.rs:1095`) — and any other
wake trigger — gains the same `|| has_pending(id)` condition, so a
finished turn with a pending follow-up re-enters the agent exactly the
way a finished turn with a task notice does today.

A follow-up is therefore delivered as a fresh top-level run (new
`AgentStart`/`AgentEnd`), not as a continuation inside the original
`prompt` call as §1.9 first sketched. This is invisible to the user ("a
message sent after the turn finished") and is strictly more robust than
an in-loop loop-exit drain — see §1.3. The wake turn is spawned through
the same `turn_cancels` machinery as a user submit, so the busy-gate and
the editor's queue-routing mode stay correct while it runs and the user
can keep queueing.

### 3.4 `QueueUpdate` ownership and race-freedom

The agent emits `QueueUpdate` after it drains (§3.3). The TUI treats it
purely as a "something changed for this agent, re-sync" trigger and
**always re-reads `queues.snapshot(active_view)`** rather than trusting
the event payload. For its own enqueues the TUI re-syncs the box
directly (it holds `&mut Tui`), so no event round-trip is needed there.

This is race-free without ordering guarantees between the TUI task
(enqueues) and the turn task (drains): the handle's `Mutex` is the
single source of truth, and a stale event can never clobber newer state
because the handler re-reads the live snapshot. The event payload
remains a correct snapshot for out-of-process consumers
(`aj --format json`); persistence ignores `QueueUpdate` (it is
transient, like notices).

---

## 4. Design — TUI layer (`aj`, `aj-tui`)

### 4.1 The pending-message box

A new `PendingMessage` component (`components/pending_message.rs`) holds
the current `(kind, text)` and renders the box from §2 with the
line-cap + `+N more lines` indicator (reuse the truncation helper the
editor uses for its `↑ N more` indicator). It renders nothing when
empty.

It lives in a new layout slot `Pending`, inserted **between `Status` and
`Editor`** (`layout.rs`): add the variant, update the `SlotIndex::idx`
match (the single source of truth for slot order), and `insert_child` it
in `build_layout`. The `Status` container (a show/hide box already
hugging the editor) is the precedent.

### 4.2 Coalescing invariant

The TUI keeps the queue to **one message, one kind** per agent: Enter
appends without changing kind, Alt+Enter appends and escalates to
Steering, yank empties it. This is the only producer in v1, so the
"at most one slot populated" invariant in §3.1 holds.

### 4.3 Re-syncing the box

A `sync_pending(tui, queues, active_view)` reconciler renders the box
from `queues.snapshot(active_view)` (or hides it when empty). It is
idempotent and called from:

- the input arm after any TUI-originated mutation,
- the pump's `QueueUpdate` arm (`event_pump.rs:709`), but only when the
  event's `agent_id == active_view` (the box shows the active view only;
  other agents' changes are ignored until the user switches to them),
- `set_active_view` (view switch shows the newly viewed agent's
  pending message).

This mirrors `sync_loader` / `sync_footer` / `sync_editor_enabled`.

### 4.4 Submit routing (the core change)

The editor's submit stays **enabled while busy**: drop the busy-disable
from `sync_editor_enabled` (`disable_submit` keeps existing for any
genuinely-disabled state, of which there are none today). The submit
handler (`interactive.rs:1736`) branches on the busy-gate instead of
refusing:

- **Idle** (not busy): unchanged — spawn the turn task and
  `agent.prompt(...)`.
- **Busy**: `queues.append_follow_up(active, text)`, record the text in
  the editor's history, clear the editor, `sync_pending`. Do not spawn a
  turn.

Factor the idle turn-spawn into a small `spawn_turn(target, text, ...)`
helper so the idle Alt+Enter path (§4.5) reuses it.

### 4.5 Alt+Enter = steering submit (host intercept)

Add `ACTION_SUBMIT_STEERING` (default `alt+enter`) and intercept it in
the input arm **before** `shell.tui.handle_input` (same pattern as the
`alt+t`/`alt+o` chords), consuming the event so the editor never sees it
(no newline). On match, read `editor.expanded_text().trim()`, then:

- **Idle**: `spawn_turn(active, text)` if non-empty — a normal prompt.
- **Busy, text non-empty**: `queues.append_steering(active, text)`;
  record history; clear editor.
- **Busy, text empty, Follow-up pending**: `queues.promote(active)`.
- **Busy, text empty, nothing pending**: no-op.

Then `sync_pending` and `request_render` (we bypassed `handle_input`, so
we must request the paint ourselves, like the clipboard chord).

### 4.6 Up / Ctrl+P yank (host intercept)

Before `shell.tui.handle_input`, and only when no overlay/login modal is
up, if the event matches `tui.editor.cursorUp`, the editor is empty
(`editor.text().is_empty()`), and `queues.has_pending(active)`:
`take_pending(active)`, `editor.set_text(text)` (cursor lands at end),
consume the event, `sync_pending`, `request_render`. Otherwise fall
through to the editor's normal history-up. Restricting to an empty
editor keeps the yank from hijacking cursor movement inside a draft and
keeps "every up that would otherwise go to history" yanking instead.

We also expose the yank as a named action `ACTION_DEQUEUE` (default
`alt+up`) that yanks **regardless** of editor contents — prepending the
queued text to whatever is currently typed (blank-line joined). This is
the unambiguous alternative for users who would rather not rely on the
empty-editor guard, and it is the chord referenced by the box's `↑ edit`
hint when the editor is non-empty.

### 4.7 Keybindings summary

| Action | Default | Where handled |
|---|---|---|
| Submit / append follow-up | `enter` | editor → submit handler routes |
| Submit / append steering | `alt+enter` (`ACTION_SUBMIT_STEERING`) | host intercept |
| Yank queued message (editor empty) | `up` / `ctrl+p` (`tui.editor.cursorUp`) | host intercept when editor empty + pending |
| Yank queued message (always) | `alt+up` (`ACTION_DEQUEUE`) | host intercept, prepends to current draft |
| Newline | `shift+enter`, `\`+Enter | editor (unchanged) |

All overridable via `config.toml` keybindings.

---

## 5. Cross-cutting

### 5.1 Sub-agents

Everything is keyed by `AgentId` and targets `active_view`, so steering
and follow-up work for any agent the user is viewing — including a
sub-agent mid-run (its `execute_turn` drains `self.agent_id`'s slots
whether it is running its initial spawn inside the parent's tool call or
a standalone continuation). The shared handle (§3.2) and the shared bus
(sub-agents inherit the parent's bus) make this fall out without
sub-agent-specific code.

### 5.2 Persistence & replay

`QueueUpdate` is transient and not persisted (the persistence listener
ignores it, like notices). A drained message persists as a normal user
`MessageEnd`. Replay is unaffected: a resumed session has empty queues.

### 5.3 Cancellation

Per §1.9, cancelling a turn does **not** silently drop the queues. In
the branch where the per-view cancel actually cancels the viewed agent's
turn (`interactive.rs:1255`), it also yanks any pending message for that
agent back into the editor (the §4.6 yank composed onto the cancel
path): interrupting the agent leaves the user holding exactly what they
had lined up, ready to edit or resend, rather than discarding it. The
quit-arm and exit branches (idle viewed agent, nothing running) are
unchanged. `clear` (§6.1) remains a separate operation for dropping a
pending message without restoring it.

### 5.4 The Alt+Enter / newline tradeoff

Repurposing Alt+Enter removes its newline-fallback role **in the prompt
editor only** (the host consumes it before the editor sees it; other
single-line inputs are unaffected, and `is_newline_event` is left
untouched). Newline entry remains via **Shift+Enter** and the
**`\`+Enter** workaround, and `ACTION_SUBMIT_STEERING` is rebindable for
anyone in a terminal that can only express newline via Alt+Enter. We
accept this because Alt+Enter is the requested gesture and Shift+Enter
is the canonical newline. (Alternative — a different steering chord —
rejected to honor the requested UX.)

---

## 6. Out of scope

- A `/queue` command or any way to inspect/reorder a multi-entry queue:
  the TUI coalesces to one message per kind. The Vec shape keeps the
  door open for multi-producer queues without a reshape.
- Drain-mode configuration (`"all"` vs `"one-at-a-time"` from §1.9):
  immaterial while the TUI keeps one entry per kind. The agent drain
  loops over the Vec, so a future multi-entry producer works without
  changes.
- Queue mutation by tools or the model (only the user enqueues), and
  queuing anything but plain text — command-palette actions are not
  queueable; the enqueue API takes a string.
- Showing another agent's pending message while not viewing it (the box
  is view-scoped, like the footer).
- Routing input typed during a blocking non-LLM operation (e.g. context
  compaction) through these queues: no such state exists today, but the
  handle is the natural place to buffer and flush it when one does.

### 6.1 `clear` gesture

The `clear` handle method is part of the API (it satisfies §1.9's
`clear_*` and is the natural binding for a future "drop the queued
message" gesture). v1 ships the method but binds no dedicated chord —
yank-then-clear-the-editor already removes a queued message. A
double-Esc (or similar) binding is a small follow-up; called out here so
the method's existence is intentional, not dead code.

---

## 7. Testing

**`aj-agent` (`MessageQueues` + drain):**

- `append_follow_up` then `append_steering` coalesces to one steering
  message (newline-joined); `promote` moves follow-up → steering with no
  text; `take_pending` returns the text and empties the slot.
- Every mutator and drain emits exactly one `QueueUpdate` carrying the
  post-change snapshot; per-`AgentId` isolation (a `Sub(1)` change
  leaves `Main` untouched).
- Scripted-provider turn: a steering message enqueued during a tool
  batch is injected as a user message after the batch and before the
  next inference. A follow-up enqueued mid-turn is delivered by a wake
  run after the turn ends (its own `AgentStart`/`AgentEnd`), as is a
  steering message left pending when the turn ends without a tool call.
  `wake` is a no-op when both queues and the notice queue are empty, and
  a follow-up enqueued during a `should_stop_after_turn` stop is still
  delivered by the wake.

**`aj` / `aj-tui` (pump + interactive, scripted):**

- Busy submit (Enter) appends to the follow-up box instead of spawning a
  turn; a second submit newline-appends; the box renders the cap +
  `+N more lines` for long text.
- Alt+Enter on a busy view enqueues/escalates to steering; the
  empty-editor Alt+Enter promotes an existing follow-up; idle Alt+Enter
  starts a normal turn.
- Up / Ctrl+P with an empty editor and a pending message yanks it into
  the editor and empties the queue; with a non-empty editor it does not;
  after a drain it falls through to history-up.
- View scoping: a `QueueUpdate` for a non-active agent does not paint the
  box; switching to that view shows its pending message; switching away
  hides it.
- The editor's submit is no longer disabled while busy.

---

## 8. Stages

1. **`MessageQueues` + agent drain** — the handle (state, mutators,
   drain, `QueueUpdate` emission), the `Agent` field + injection +
   sub-agent share, the in-loop steering drain in `execute_turn`, and
   the wake-path drain (`Agent::wake` guard + the run-top
   `drain_steering`/`drain_follow_up`). Agent-layer unit/scripted tests.
   No TUI yet.
2. **`PendingMessage` component + slot** — the component, the `Pending`
   slot in `layout.rs`, and `sync_pending` driven by the pump's
   `QueueUpdate` arm and `set_active_view`. Pump tests.
3. **Submit routing + Alt+Enter + yank + cancel-restore** —
   busy-routes-to-queue in the submit handler, `spawn_turn` extraction,
   the host intercepts and the `ACTION_SUBMIT_STEERING` / `ACTION_DEQUEUE`
   bindings, dropping the busy-disable, the `|| has_pending(id)` clause
   on the binary's wake trigger (§3.3), and the yank-on-cancel (§5.3).
   Interactive tests.
