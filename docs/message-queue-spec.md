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

### 1.3 The drain points already exist

In `execute_turn` (`lib.rs:848`), the loop already drains task notices at
exactly the two boundaries we need:

- **After the tool batch, before the next inference** (`lib.rs:1318`,
  next to `drain_task_notices`) → the **steering** drain point ("right
  after the next tool call").
- **At loop exit** (the `else { break }`, `lib.rs:1326`) → the
  **follow-up** drain point ("sent once the turn is done"), and a
  steering fallback for a turn that ends without any tool call.

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
  terminals that deliver Shift+Enter as `\x1b\r`. See §5.5.
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

- A `Follow-up` is injected as a user message when the turn would
  otherwise end, and the run continues into a new turn (one `prompt`
  call, one `AgentStart`/`AgentEnd` bracket — the busy-gate stays set
  throughout).
- A `Steering` message is injected right after the current tool batch
  finishes, before the next inference; if the turn ends with no tool
  call, it is injected at loop exit (so it is never lost).
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
#[derive(Default, Clone)]
struct AgentQueues {
    steering: Vec<UserMessage>,
    follow_up: Vec<UserMessage>,
}

pub struct MessageQueues {
    inner: Arc<Mutex<HashMap<AgentId, AgentQueues>>>,
    bus: EventBus, // emits QueueUpdate after every change
}
```

`bus` is a clone of the session bus, obtained at construction the way
`TaskRegistry`'s drivers receive one. Every mutator mutates under the
lock, drops the lock, then emits
`QueueUpdate { agent_id, steering, follow_up }` for that agent — the
snapshot is taken under the lock so the emitted payload is internally
consistent. The lock is never held across the `await` on `emit`.

**Mutators** (called from the TUI input arm, which is async):

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

**Drain** (called from `execute_turn` while holding `&mut self`):

- `drain_steering(agent) -> Vec<UserMessage>`
- `drain_follow_up(agent) -> Vec<UserMessage>`

Each removes and returns the relevant slot and emits `QueueUpdate`.

**Reads** (no emit): `snapshot(agent) -> QueueSnapshot { kind, text }`
and `has_pending(agent) -> bool`, used by the TUI to render the box.

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

### 3.3 Drain wiring in `execute_turn`

- **Steering** at `lib.rs:1318` (alongside `drain_task_notices`): drain
  `self.message_queues.drain_steering(self.agent_id)`, append each as a
  user message via the existing `MessageStart`/`MessageEnd` bracket (no
  `<task-notification>` wrapper — these are real user messages), then
  fall through to the existing `continue`.
- **Loop exit** at the `else { break }` (`lib.rs:1326`): before breaking,
  drain steering first (the no-tool-call fallback), then follow-up; if
  anything was drained, append the messages and `continue` instead of
  breaking. Draining steering before follow-up keeps the "more urgent"
  ordering when both somehow exist.

Both drains stay inside the same `prompt` call, so the binary's
`turn_cancels` entry — and thus the busy-gate and the editor's
queue-routing mode — persist across the whole multi-message run with no
extra bookkeeping.

### 3.4 `QueueUpdate` is a trigger, not a payload

`QueueUpdate` is emitted on every mutation and every drain. The TUI
treats it purely as a "something changed for this agent, re-sync"
trigger and **always re-reads `queues.snapshot(active_view)`** rather
than trusting the event payload. This makes the design race-free without
ordering guarantees between the TUI task (mutations) and the turn task
(drains): the handle's `Mutex` is the single source of truth, and a
stale event can never clobber newer state because the handler re-reads
the live snapshot. The payload remains correct for out-of-process
consumers (`aj --format json`); persistence ignores `QueueUpdate` (it is
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

### 4.7 Keybindings summary

| Action | Default | Where handled |
|---|---|---|
| Submit / append follow-up | `enter` | editor → submit handler routes |
| Submit / append steering | `alt+enter` (`ACTION_SUBMIT_STEERING`) | host intercept |
| Yank queued message | `up` / `ctrl+p` (`tui.editor.cursorUp`) | host intercept when editor empty + pending |
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

Per §1.9, cancelling a turn (Ctrl+C) does **not** clear the queues —
the user may want the queued message to drive the next run. `clear`
(§6.1) is a separate, explicit operation.

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
- Queue mutation by tools or the model (only the user enqueues).
- Showing another agent's pending message while not viewing it (the box
  is view-scoped, like the footer).

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
  next inference; a follow-up enqueued mid-turn is injected at loop exit
  and the run continues in the same `AgentStart`/`AgentEnd` bracket; a
  steering message on a no-tool-call turn is injected at loop exit.

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
   sub-agent share, and the two drain points in `execute_turn`.
   Agent-layer unit/scripted tests. No TUI yet.
2. **`PendingMessage` component + slot** — the component, the `Pending`
   slot in `layout.rs`, and `sync_pending` driven by the pump's
   `QueueUpdate` arm and `set_active_view`. Pump tests.
3. **Submit routing + Alt+Enter + yank** — busy-routes-to-queue in the
   submit handler, `spawn_turn` extraction, the two host intercepts and
   the `ACTION_SUBMIT_STEERING` binding, drop the busy-disable.
   Interactive tests.
