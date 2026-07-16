# Lazy sub-agent history on resume

Status: proposed.

## Motivation

Resuming a large session is slow and memory-heavy. After
`ConversationLog::resume` the whole log is already in memory, so the
remaining cost is the projection and reduction pass that turns the log
into the chat model:

```rust
for event in replay(&log) {
    reduce(&mut chat, &mut core.lifecycle, event);
}
```

Earlier profiling put replay projection at roughly 510ms and the reduce
at roughly 60ms on the sample session, and in that session about 416 of
442 MiB of history is sub-agent threads. Projection is expensive because
it clones every message and tool payload into an `AgentEvent`, and reduce
then clones each again into a per-agent `Transcript`. So the dominant
resume cost is building sub-agent transcripts that the user is usually
not looking at.

This spec defers that work. At resume we build the main transcript and
the sub-agent boxes but not the sub-agent transcripts. A sub-agent's
full history is reconstructed on demand, when the user observes it.

Scope: the `aj-next` TUI. The live event path is unchanged (see
Non-goals). Print mode and HTML export are unaffected because they do
not go through this reduce pass.

## The key invariant

Replay always closes every sub-agent bracket. `close_open_sub`
(`src/aj-session/src/replay.rs`) emits a `SubAgentEnd` for a run even
when the log has no explicit terminal record, for example a background
sub-agent that was still running when the session was last closed. So
after a resume every sub-agent box is in the `Done` state.

A sub-agent box is therefore in the `Running` state only during a live
run in the current session, and a live run always has its transcript
present because the streaming events build it as they arrive. This is
the invariant that lets the box avoid ever touching a resumed
sub-agent's transcript:

- `Done` box: renders from the report, which is metadata on the box
  entry. No transcript needed.
- `Running` box: live only, transcript always present.

The only consumer that needs a resumed sub-agent's full transcript is
Observe, an explicit async user action. Crucially, a resumed sub-agent
has no live handle (the agent registry only holds sub-agents spawned
this session), so no live turn can ever be spawned against it, and the
only way to target it at all is to Observe it first. That means the
render path never has to reach into the log, and no live event ever
addresses a still-deferred sub-agent (see Why no live hook is needed).

## What the box shows

Today the collapsed box (`build_subagent_box`,
`src/aj-next/src/subagent_box.rs`) composites the child transcript and
tail-windows it to a fixed number of rows. This spec replaces that body:

- `Done`: the status glyph, `agent N`, the task, and the sub-agent's
  report (or a snippet of it). The report is already captured by replay
  as `SubAgentEnd { report }` and stored on the box entry by the
  reducer, but is currently not rendered. This spec renders it.
- `Running` (live only): the status glyph, `agent N`, the task, a
  running indicator, and a one-line latest-activity string.

This is a deliberate behavior change. The box no longer shows a live
scrolling preview of the sub-agent's work. It shows the sub-agent's
conclusion (its report) once done, and while running it shows a running
indicator and a single latest-activity line so the user still sees that
the sub-agent is making progress and roughly what it is doing. The full
conversation is one Observe away, exactly as today. The behavior is now
consistent between a sub-agent that ran in this session and one loaded
from disk, which the tail-window preview was not once a session was
resumed.

The latest-activity line is a single `String` field on `SubAgentEntry`
updated by the reducer from live events (the last assistant line, or the
current tool label). It is not read from the transcript, so it
introduces no transcript dependency and no lazy-load concern. It stays
`None` on a resumed box, which is always `Done` and so shows its report
instead.

The running indicator is a wall-clock spinner, matching the status
loader's cadence, so it keeps animating between sub-agent events rather
than freezing (a frozen glyph reads as stalled). Three pieces drive it.
The glyph is picked from a fixed frame set by
`started_at.elapsed() / interval % frames`. The status loader arms its
redraw tick while any sub-agent runs, not only while the viewed agent is
busy, so a background sub-agent viewed from Main still gets a periodic
redraw. And a `Running` box bypasses the transcript render cache, so each
redraw rebuilds it with a fresh frame. The reducer still updates the
box's `latest_activity` line on live events, which the bypassing box
picks up on its next rebuild.

The box always shows the latest run's conclusion. Report is written by
`SubAgentEnd` (initial run and replay), but a continuation or steering
re-run of a live sub-agent completes through `AgentEnd(Sub n)`, which
today flips status to `Done` without refreshing `report`. Since the box
now renders `report`, the reducer must refresh the box's shown
conclusion on that `AgentEnd(Sub n)` completion too, sourced from the
sub-agent's concluding assistant text (its live transcript is present
for a sub that just ran). Otherwise a re-run box would show the previous
run's conclusion. This affects the pure-live path as much as resume, so
it is a reducer fix, not a resume-only concern.

## Design

### Resume: defer sub content in replay

The expense is projection, not the reduce, so filtering replay's output
after the fact would not help. Replay itself must not project sub-agent
content.

Add a deferred-sub mode to `Replay` (a constructor such as
`replay_deferring_subs(&log)` or a flag on the existing `Replay`). In
this mode, while a sub-agent bracket is open replay:

- still emits `SubAgentStart` (so the reducer creates the box, the empty
  child transcript slot, and the child footer seed) and `SubAgentEnd`
  (so the box gets its `Done` status and report),
- does not project or emit the sub-agent's `MessageStart`/`MessageEnd`,
  `ToolExecutionStart`/`Update`/`End`, `UsageUpdate`, or `Notice`
  events,
- still tracks the report the way it does today, by remembering the
  latest sub-agent assistant text and reading it at bracket close. This
  is cheap because it reads one message rather than cloning every
  sub-agent message into an event.

The deferred mode is defined as the full replay state machine with the
sub-agent content projection gated off. It runs the same
`bracket_subagent` and `close_open_sub` logic, and it updates the
per-bracket report from sub-agent assistant text exactly as full replay
does. The only difference is that for sub-agent entries it does not push
the projected `MessageStart`/`End`, `ToolExecution*`, `UsageUpdate`, or
`Notice` events onto the output. This is a contract, not an
optimization detail: deferred replay must emit the identical sequence of
`SubAgentStart`/`SubAgentEnd` events, with identical reports, that full
replay emits. Only the sub-agent content events are withheld. A test
asserts this equality (see Testing).

This requires a small refactor, not a boolean gate, and the spec calls
it out because the naive implementation is silently wrong. Today the
report is captured as a side effect inside `project_assistant`, right
next to the message and usage clones that must be dropped for the perf
win. Simply skipping `project_entry` for sub-agent entries would also
skip the report capture, so every resumed box would render an empty
report. The report capture (the `AssistantContent::Text` fold that
overwrites, not accumulates, the open bracket's report, cleared on
bracket open) must be extracted into a helper that both the full and
deferred paths call, so the report advances without the clones. The
same applies to any per-bracket state `close_open_sub` reads. Deferred
mode advances that state and withholds only the event pushes.

The main thread, `CompactionEnd` (which is USER-thread only), and the
sub-agent bracket events are projected exactly as before. Walking the
log by index is still O(entries), which is unavoidable and already
streamed, but the per-sub-agent projection clones are gone. That is the
bulk of the 510ms and the sub-agent transcript memory.

`replay(&log)` keeps its current full behavior for the callers that need
every event, namely print mode's JSON dump and `derive_title` in HTML
export. Only the two `aj-next` resume drains
(`src/aj-next/src/interactive.rs`) use the deferred mode.

The resume drain records which sub-agent indices were deferred. A
sub-agent is "deferred" until its transcript is materialized. The
simplest home for this is a set the interactive host owns alongside
`chat`, for example `deferred_subs: HashSet<usize>`, seeded from the
`SubAgentStart` events seen during the deferred drain.

### Observe: materialize on demand

Observe is handled at `AgentPickerOutcome::Observe(id)`
(`src/aj-next/src/interactive.rs`), which today just calls
`chat.set_active_view(id)`. This runs in the async event loop, so it can
lock the log.

The Observe handler (`apply_picker_outcome`) becomes async so it can
lock the log. Its call site is already async.

When `id` is `AgentId::Sub(n)` and `n` is still deferred, materialize
before switching the view. The log is locked only for the read, then
released so a concurrent live turn's persistence is not blocked behind
the projection and reduce:

1. take an owned copy of the thread under a short lock, and drop the
   lock:
   ```text
   let conv = {
       let log = core.log.lock().await;
       let head = log.latest_leaf(ThreadFilter::subagent(n))?;
       log.linearize(&head, ThreadFilter::subagent(n))
   };
   ```
   `linearize` walks the sub-agent's own `parent_id` chain and returns
   an owned `Conversation`, so it is correct even for background
   sub-agents whose entries interleave with the parent's, and nothing
   else needs the lock afterward. Holding the lock across the whole
   materialize would stall live streaming on a large sub-agent, since
   persistence appends under the same lock inline on each emit,
2. project `conv` into that sub-agent's events with a new
   `project_thread(&conv, AgentId::Sub(n))` helper in `replay.rs` that
   reuses the existing `project_assistant` / `project_tool_result` /
   `project_user` logic with a fresh projection state scoped to the
   thread. It emits the sub-agent's `MessageStart`/`End`,
   `ToolExecution*`, `UsageUpdate`, and `Notice` events, but not
   `SubAgentStart`/`End` because the box already exists,
3. `reduce` those events into `chat`, which fills
   `transcripts[Sub(n)]`, refines the sub-agent footer occupancy from
   the replayed `UsageUpdate`s, and sets `header_only` on its tool
   entries via the same rule the reducer uses live,
4. remove `n` from `deferred_subs`,
5. `chat.set_active_view(Sub(n))`, which reconciles `header_only`
   against the now-active view exactly as today.

Materialization is idempotent and cached by `deferred_subs` membership,
so re-observing a sub-agent does no work.

`project_thread` has one precondition it can rely on: a sub-agent thread
never contains a `Compaction` entry (compaction runs on the USER thread
only), so single-thread projection needs no `log` handle for the
compaction estimate that full replay computes. Parity with full replay
holds because the usage accumulator and the settings-notice gate are
keyed per `AgentId`, so a fresh state scoped to `Sub(n)` reproduces the
same `UsageUpdate` sequence and `Notice` gating that full replay
produces for that sub-agent.

### Why no live hook is needed

An earlier draft materialized a deferred sub-agent before reducing any
live event addressed to it, to guard against a re-run appending to an
empty transcript. That hook is unnecessary and would be racy, so it is
not part of this design.

It is unnecessary because a resumed sub-agent cannot receive a live
event while still deferred. Spawning or waking a turn against a
sub-agent requires a live handle from the agent registry
(`resolve_agent`), which resume never populates, so `spawn_turn` refuses
a resumed target. The only host path that targets a sub-agent submits
to the current `active_view`, and making a sub-agent the active view
goes through Observe, which materializes it first. So by the time any
live event could be addressed to a sub-agent, it is already
materialized and out of `deferred_subs`.

It would be racy because persistence appends each event to the log
inline on emit, before the frontend drains it. A hook that re-read the
log with `linearize` on the first live event would pick up entries the
live drain is about to reduce, applying the new turn twice. Avoiding the
hook sidesteps this, and keeps `drain_events` synchronous.

### The box fingerprint

`subagent_fingerprint` (`src/aj-next/src/transcript.rs`) currently hashes
the child transcript so the box re-renders when the child changes. With
the box rendering from metadata, the fingerprint changes to hash the box
entry's state: status, the report, the latest-activity string, and the
background flag. The report and activity strings are short, so the
fingerprint hashes their full value rather than a length proxy, otherwise
a same-length activity transition (for example `bash` to `grep`) would
leave a stale line. Changing both transcript readers together is
load-bearing: `build_subagent_box` and `subagent_fingerprint` are the
only two readers of a sub-agent's transcript in the render path, and the
box currently renders the child transcript, so a box left unchanged would
render blank for a deferred `Done` sub-agent. A `Done` box no longer
depends on the transcript at all. A `Running` box bypasses this
fingerprint cache entirely (its glyph animates on the wall-clock), so it
rebuilds on every redraw and always reflects its latest metadata.

## Non-goals

- The live projection path is unchanged. Live sub-agents still build
  their transcripts as events stream in, so observing a running
  sub-agent still shows it working in real time. Making the live path
  lazy as well (materialize only on observe, live and resumed) is a
  possible later step but is out of scope here, because it would turn
  observing a running sub-agent into a snapshot rather than a live view.
  The box body and the `latest_activity` field do change on the live
  path, since the box render is shared, but no transcript building
  changes live.
- The old `aj` interactive TUI is out of scope and keeps its current
  eager behavior. It has its own event pump and sub-agent box and does
  not consume `aj-app`'s `reduce` / `ChatState`, so the shared `aj-app`
  changes here (the `latest_activity` string on `SubAgentEntry`, the
  continuation-report refresh, and the new `replay_deferring_subs`
  constructor) are additive for it and render only in `aj-next`.
- Print mode and HTML export are unaffected. Print uses full `replay`,
  export's body reads `ConversationLog::entries_in_order` directly and
  its title uses full `replay` (`derive_title`), so both still render
  full sub-agent threads.

## Aborted or partial sessions

Resuming a session that was killed while sub-agents were still running
must keep working. It does, for the same reasons it works today.

- A torn final JSON line from the abort is repaired by the streaming
  resume in `ConversationLog::resume` before replay runs, so the log
  loads with the last partial record dropped.
- Replay closes every open sub-agent bracket at end of log
  (`close_open_sub`), so a sub-agent that never actually finished still
  produces a `SubAgentEnd`. The resumed box is `Done` with a report
  taken from the last assistant text that was flushed, which may be
  partial or empty. This is the current behavior, and the deferred mode
  preserves it because it runs the same bracketing and report tracking.
- A background sub-agent whose entries interleave with the parent's in
  append order can open and close its bracket more than once during
  replay. This is pre-existing behavior. The box ends `Done` with the
  report from the last close, whether or not projection is deferred.
- Observe materializes from `linearize(ThreadFilter::subagent(n))`,
  which walks the sub-agent's own `parent_id` chain and so gathers the
  full history regardless of interleaving or how many brackets replay
  opened. So even for an aborted background sub-agent, observing it
  shows its complete flushed history, matching the eager path.
- A sub-agent aborted immediately after spawn, with no messages, has
  only its `SubAgentSpawn` entry. It resumes as a `Done` box with an
  empty report and an empty transcript on observe, the same as today.

Resume never auto-restarts a resumed sub-agent, and a resumed sub-agent
has no live handle to re-run against (see Why no live hook is needed),
so an aborted sub-agent stays `Done`. To interact with it the user
Observes it, which materializes its flushed history.

## What does not change

- Agent picker and `ChatState::agents()` read box metadata (task,
  status, runtime, background), never a child transcript, so they work
  unchanged against deferred sub-agents.
- Session stats count `SubAgentSpawn` entries in the log
  (`src/aj-session/src/stats.rs`), unaffected.
- Compaction operates on the USER thread only, unaffected.
- The usage overlay is sourced from provider usage reports, not local
  footers, so deferring sub-agent `UsageUpdate` events does not change
  it.

## Touch points

- `src/aj-session/src/replay.rs`: the deferred-sub replay mode, the
  extracted report-capture helper shared by full and deferred paths,
  and a `project_thread` helper for single-thread materialization.
- `src/aj-next/src/interactive.rs`: both resume drains use the deferred
  mode and seed `deferred_subs`; `apply_picker_outcome` becomes async
  and materializes on Observe. `drain_events` stays synchronous (no live
  hook). `deferred_subs` is carried in the session bundle (`World` and
  `NextSession`) and replaced together with `chat` in
  `install_next_session`, and reseeded by each drain.
- `src/aj-next/src/subagent_box.rs`: box body renders from metadata
  (report or running indicator plus latest-activity), not the child
  transcript.
- `src/aj-next/src/transcript.rs`: `subagent_fingerprint` keys on box
  metadata by full value.
- `src/aj-app/src/chat/model.rs` and `reducer.rs`: the `latest_activity`
  string on `SubAgentEntry`, its live update, and the continuation
  `AgentEnd(Sub n)` report refresh. No change to how transcripts are
  stored. These are consumed only by `aj-next`.

## Testing

- Deferred replay unit test: the deferred mode emits main events plus
  `SubAgentStart`/`SubAgentEnd` and emits no sub-agent content events.
  Assert the `SubAgentEnd` report is non-empty and byte-equal to the
  report full `replay` produces for the same log. The non-empty
  assertion specifically guards the report-capture refactor, whose naive
  form yields an empty report.
- `project_thread` parity: for a given sub-agent, the events it emits
  match the sub-agent-tagged events that full `replay` emits for that
  same thread (same order, same payloads), so a materialized transcript
  equals the eager one. Include a background sub-agent whose entries
  interleave with the parent's, not just a foreground one, since parity
  relies on the sub-agent's append order matching its `parent_id` chain.
- Full-replay callers unchanged: a test that print mode and export still
  see a sub-agent's messages. `src/aj/tests/replay_parity.rs` exercises
  the old `aj` event pump through full `replay` and gives no coverage of
  the deferred mode, so a regression that flipped `replay` to deferred
  would pass it. This new test closes that gap.
- Resume integration: resume a session with sub-agents. Assert the boxes
  are present with the right task, `Done` status, and report, and that
  `transcripts[Sub(n)]` is empty until observed. Observe one and assert
  its transcript, footer occupancy, and tool `header_only` flags match
  what the eager path produces.
- Re-observe idempotency: resume, observe `Sub(n)`, switch to Main,
  observe `Sub(n)` again. Assert the second Observe does no materialize
  work (`n` already out of `deferred_subs`) and the transcript is intact.
- `header_only` reconcile: after materialize, switch the active view
  away from the sub-agent and back, asserting the tool cells' 
  `header_only` flags flip exactly as the eager path leaves them.
- Legacy-log seeding: a log with no `SubAgentSpawn` entry (the replay
  fallback synthesizes `SubAgentStart` at the sub-agent's first
  message). Assert the box is still seeded into `deferred_subs` and
  materializes on Observe.
- Session-switch survival: resume a session with deferred sub-agents,
  switch to another session, and assert the new session's
  `deferred_subs` reflects its own sub-agents and the old set did not
  leak (indices restart per session).
- Aborted-session resume: build a log that ends mid sub-agent run (no
  terminal record for the sub-agent, and a torn final line). Assert it
  resumes with the sub-agent box present and `Done`, its report equal to
  the last flushed assistant text, and that observing it materializes
  the full flushed history. Include a background sub-agent whose entries
  interleave with the parent's.
- Continuation-report refresh (live, not resume-specific): run a
  sub-agent, observe it, submit a steering continuation, let it finish
  through `AgentEnd(Sub n)`, switch to Main, and assert the box shows the
  continuation's conclusion, not the first run's.
- `latest_activity` reducer test: assert the field is updated from live
  sub-agent events (last assistant line, current tool label) and drives
  a `Running` box redraw.
- Box render snapshots: `Done` box shows the report, `Running` box shows
  the running indicator and latest-activity. The existing
  `subagent_box` tests assert the composited child transcript and will
  break under the metadata box, so they are rewritten as part of this
  change, not treated as regressions.
- The Observe picker test
  (`agent_picker_observe_switches_the_view`) is rewritten once
  `apply_picker_outcome` is async and materializing.
- Perf smoke on the sample session: resume wall time and peak RSS before
  and after.

## Performance expectations

On the sample session, deferring sub-agent projection removes most of
the roughly 510ms projection time and the roughly 416 MiB of sub-agent
transcript clones, since sub-agents are about 94% of that history. The
main transcript, boxes, picker, stats, and export are unchanged.
Observing a sub-agent pays a one-time cost bounded by that one
sub-agent's size, then caches.

## Risks and open questions

- Behavior change to the collapsed box (no live scrolling preview). This
  is the deliberate simplification that removes all draw-time log access
  and makes the box consistent across live and resumed sub-agents. It
  needs sign-off, which this spec assumes.
- Tool-concluding sub-agents render a thin box body. The report captures
  the last assistant text only, so a sub-agent whose final action was a
  tool call resumes with an empty report and a box that shows just task
  and status. The tail-window used to show that trailing tool work. How
  common this is depends on whether sub-agents usually end with a prose
  report, which they typically do because the `agent` tool result is the
  final assistant text. If it turns out common, the report capture can
  be widened to a last-activity string (last tool label when no trailing
  prose), at the cost of a small change to what the report means.
- The running indicator animates on a wall-clock even while a background
  sub-agent runs and Main is viewed: the status loader arms its redraw
  tick whenever any sub-agent runs, and a `Running` box bypasses the
  render cache so each redraw rebuilds it with a fresh frame.
- Resumed sub-agent runtime shows as roughly zero, because
  `started_at`/`finished_at` are stamped at reduce time and
  `SubAgentStart`/`SubAgentEnd` fire back to back on resume. This is
  pre-existing and not changed here.
