# Remote control and VM provisioning

## Status: draft, phase 2 complete

Companion document: `docs/remote-control-implementation.md`, the manual
for the implementing agent.

## 1. Overview

This feature makes an aj session controllable from another aj process,
and grows that into a small distributed system:

- A **session host** serves one or more live sessions over a control
  port. Every attached client, local or remote, sees the full event
  stream and can submit prompts, steer, cancel, and change settings.
- A **gateway** is an aj process that aggregates many session hosts
  behind one address, multiplexes their sessions to clients, and can
  provision new hosts by spinning up VMs (via ember) that run aj.
- A **client** is the aj TUI attached over the network instead of
  in-process. A sidebar lists all reachable sessions with status
  indicators and switches between them instantly.

The design leans on what aj already has. The `AgentEvent` bus is the
single source of UI truth, its JSON shape is test-pinned, session
resume already works by replaying the on-disk log through the same
reducer that renders live events. Remote control is, at its core, that
same event stream and that same reducer with a network in between.

One principle governs everything: **the local TUI and remote clients
are peers.** Both attach to a session host through the same interface,
both submit through the same command path, and conflicts resolve
through the queueing semantics that already exist (busy agent, message
queues, steering). There is no "owner" of a session beyond the host
process itself.

## 2. Goals and non-goals

Goals:

- Remote-control protocol: attach to a session, receive everything,
  submit anything the local UI could.
- Correct catch-up: a client that connects mid-session, or reconnects
  after a drop, converges to the same durable-derived state as a
  client with an uninterrupted connection. (Transient-only artifacts,
  such as in-flight streaming text, may differ momentarily and are
  superseded as activity concludes. Section 11 defines the precise
  equivalence that tests enforce.)
- Multi-session hosts: one aj process can hold several live sessions.
- Gateway mode: one address for many hosts, one unified stream, plus
  VM provisioning with ember as the first backend.
- Session sidebar in the TUI: list, status glyphs, instant switching.
- A testing story strong enough that the protocol can be trusted:
  reducer-equivalence tests over the scripted provider, with fault
  injection.

Non-goals:

- Transport encryption. Tailscale's WireGuard mesh provides it, aj
  never terminates TLS itself. Connection identity, however, is in
  scope: the control port is remote code execution, and section 6.11
  defines its protection layers. What stays out of scope is
  per-connection human re-authentication (Tailscale SSH check-mode
  parity), which tailscale offers only for SSH, see 6.11 for the
  posture and the tunnel fallback.
- Tool approvals or any client-to-agent mid-turn decision channel.
  Status is working / idle, nothing more (the Shelley model).
- A web UI. The transport choice (HTTP + SSE) keeps that door open but
  nothing here builds toward it.
- Keyless VMs via an LLM credential proxy. v1 ships API keys to the VM
  over the trusted network. A proxy mode is a clean later upgrade.
- Off-host reachability of ember guests. v1 requires the gateway to
  run on the ember host, where guest IPs are directly dialable.

## 3. What we build on

Facts about the current codebase that the design depends on:

- `AgentEvent` (`aj-agent/src/events.rs`) serializes as internally
  tagged snake_case JSON. The shape is test-pinned and documented as a
  contract (print mode `--format json` emits it today). The event
  envelope carries no sequence numbers (message payloads have their
  own timestamps, but nothing orders events on the wire).
- Events only derive `Serialize`. Payload types mostly round-trip
  already. Adding deserialization is mechanical, with two known warts:
  the `Arc<[UserContent]>` fields need custom deserializers matching
  the existing custom serializer, and `AgentMessage::id` is
  serde-skipped (in-memory only). That id is not cosmetic: the reducer
  stores it on transcript entries and the branch feature resolves
  branch targets through it. The wire therefore carries the log entry
  id on durable frames and the client codec backfills the decoded
  message id from it, exactly as local resume backfills ids from log
  entries.
- `aj_session::replay()` projects the persisted log onto the same
  event stream a live run produces, and the `aj-app` chat reducer
  (`reduce`) folds either into `ChatState`. Local resume is exactly
  replay-then-reduce. Tool-detail `body_ref` compaction is resolved
  during projection, so projected events are self-contained.
- Persistence is a bus subscriber (`persistence_listener`), one log
  entry per `MessageEnd` (plus `SubAgentSpawn` roots). The log is
  append-only JSONL, entries form a tree via `parent_id`, and
  `linearize(head)` produces the model-facing view. Non-punctuation
  entries (settings, spawn roots) buffer in memory until the next
  punctuating append, and a torn tail is truncated on resume, so the
  on-disk suffix is not crash-stable. The protocol accounts for this
  (epoch lifetime, section 6.5).
- `aj-app` is frontend-agnostic by CI-enforced rule (no vaxis
  dependency) and composes session state: `SessionCore`, the turn
  driver (`Turns`, `drive_turn`), the queues (which live in
  `aj-agent`), and the reducer.
- One process currently holds one live session. Switching tears down
  and rebuilds the world. Lifting this is part of the work (section 5).
- The scripted provider (`aj_models::scripted`, `--scripted`) replays
  canned model streams through the whole real pipeline. It is the test
  substrate for everything here.
- Exactly three event kinds carry cumulative snapshots where the last
  one wins: `MessageUpdate`, `ToolExecutionUpdate`, `TaskOutput`.
  Only these are safe to lose or coalesce. Other non-durable events
  (`ToolExecutionEnd`, `SubAgentEnd`, `TaskStart`/`TaskEnd`, notices,
  lifecycle brackets) are one-shot: the reducer renders tool results
  only from `ToolExecutionEnd` (the durable tool-result `MessageEnd`
  is structural framing), and a lost `SubAgentEnd` wedges a sub-agent
  box forever. The protocol's reliability classes (section 6.4) are
  built around this distinction.
- Settings changes today write log entries directly (no bus event) and
  the TUI shows a local notice. The host synthesizes the corresponding
  wire frames (section 5).

## 4. Roles and topology

Three roles, one protocol:

- **Session host**: `aj serve --listen <addr>` (headless) or
  `aj --listen <addr>` (interactive TUI plus embedded server). A host
  process serves the sessions of exactly one working directory, the
  one it was started in. Each host persists a stable random `host_id`
  in the project's session store (a `host-id` file in
  `~/.aj/sessions/<project>/`, next to the logs it identifies), never
  in user-global state or the working directory itself. The id names
  the session store, not the process: session ids are unique within a
  store, which is what makes `<host_id>:<session_id>` globally unique
  (section 6.2). The default listen address for a bare `--listen` is
  `127.0.0.1:6161`.
- **Gateway**: `aj gateway --listen <addr> [--config <file>]`.
  Speaks the client side of the protocol toward hosts and the server
  side toward clients. Implements the same session-facing API as a
  host, so clients cannot and need not distinguish the two except by
  advertised capabilities. Adds host and VM management endpoints.
- **Client**: `aj connect <url>`. The normal TUI, rendering from
  events received over the wire, submitting via commands.

A gateway can front another gateway without special cases, because it
only depends on the session-facing API. Not a v1 target, but nothing
should preclude it.

## 5. The session host

This is the main internal refactor. A new layer in `aj-app`, the
**session host**, owns N live sessions and exposes one attachment
interface used by both the local TUI and the network server.

Responsibilities:

- Session lifecycle: create, materialize on demand (attach or command
  to a known-on-disk session), keep live, tear down cleanly on
  shutdown. Idle eviction is out of scope for v1, sessions stay live
  until the host exits.
- Single-writer safety: materializing a session takes an advisory lock
  on the session, held in a lock file beside the log (the log itself is
  created lazily, so there is not always a file to lock), released on
  teardown. The lock is taken before anything reads or repairs the log,
  because resume truncates a torn tail and repair appends tool results,
  so a refused materialization must not have touched the file. A second process (host
  or plain interactive aj) that hits the lock refuses to materialize
  that session (surfaced as a 409 over the wire). This is what keeps
  `aj --listen` and a gateway-spawned `aj serve` in the same directory
  from corrupting a shared log.
- Per session: the `SessionCore` (agent, log, queues, task registry),
  a turn driver replacing the TUI drive loop's turn arm (spawning
  turns, wakes, compaction continuations, reaping), and an event
  fan-out that tags events and forwards them to attached clients.
- Sequence assignment: durable events are tagged with their log
  entry's append index at the append site, and the tag travels with
  the event to the fan-out. The fan-out must never infer the index by
  reading log length at consume time, that races concurrent sub-agent
  appends. Per session, **live** durable frames are delivered to any
  given stream in strictly increasing seq order, and the server never
  delivers a live durable frame at or below the stream's backfill
  boundary (this is what absorbs durable events that were in flight in
  the fan-out when an attach was served). Backfill blocks follow
  projection order instead, see section 6.5.
- The command surface: submit (prompt-or-queue, mirroring today's
  `handle_submit` semantics), steer/promote, cancel (with the existing
  sub-agent-to-main cascade), compact, settings changes (host applies
  them, writes the settings log entries, and synthesizes the
  corresponding wire frames, both the projected notice event and a
  refreshed `state` frame). A settings entry that lands before its
  thread's first message projects no notice, so the durable frame the
  host publishes for it would have nothing to regenerate from. The host
  publishes the confirmation untagged in that case: live clients still
  see it, and it is a transient notice like any other, which is what
  keeps the pre-first-prompt settings gesture from going silent. Also
  task kill, head/branch switch, and session create.
- Head switching: refused with a conflict while a turn is running or
  background tasks are live, mirroring the local busy refusal (a
  mid-turn head switch would let the running turn persist onto the
  wrong branch). On success the host clears that session's pending
  message queues, mints a new epoch, and emits a `reset` frame.
- Emitting the host-level state the protocol needs: the session list
  with per-session status, queue snapshots on every queue mutation
  (today only drains emit `QueueUpdate`, the host emits them on
  enqueue too), per-session `state` frames (section 6.3), and
  epoch/seq bookkeeping.
- Shutdown (SIGTERM, or the interactive host quitting): cancel running
  turns through the existing abort path (which leaves transcripts
  consistent), quiesce background tasks, flush log buffers, close
  client streams. Remote clients of a departed host simply lose the
  connection, through a gateway the sessions surface as unreachable.
- A turn's fatal error belongs to its session, not to the host. It
  surfaces as an error frame on that session's stream and the session
  stays live. A host serving several sessions must not exit because one
  of them hit a disk failure, and the local TUI attached to it should
  show the error rather than quit.

The local TUI becomes the first client of this interface, attached
in-process through direct handles and channels, not through HTTP. It
keeps its `ChatState` and reducer exactly as today, what changes is
that world mutations route through host commands instead of touching
`SessionCore` directly. Remote commands arrive at the same methods.
This is what makes "peers" true by construction rather than by
discipline.

The host layer must not depend on the TUI (the existing `aj-app` rule
already enforces this) and must be usable without any terminal at all,
that is what `aj serve` is.

## 6. Wire protocol

### 6.1 Transport

HTTP. Commands are JSON POSTs, effects arrive on the stream. Streaming
is SSE, one `data: <json>` frame per line. Reads are plain GETs. No
WebSockets, no JSON-RPC, no correlation ids.

All routes live under `/v1/`. `GET /v1/hello` returns protocol
version, capability list, app version, `host_id`, and the working
directory (a gateway omits the working directory and advertises its
own capabilities). It is the reachability and identity probe.

SSE streams send a heartbeat frame every 30 seconds when otherwise
idle. Clients treat a silent stream (no frame for ~60s) as dead and
reconnect with backoff.

Error responses carry `{code, message}`. The vocabulary: 400 malformed
request, 404 unknown session/task/entry, 409 conflict (busy refusal, lock
conflict, or a request the host cannot serve such as a model it has no
credentials for), 500 the host failed internally, 503 upstream host
unreachable (gateway only).

### 6.2 Sessions and addressing

A session is addressed by an opaque string id. On a host this is the
existing session id (the timestamp filename stem). A gateway
namespaces ids as `<host_id>:<session_id>` (colon, which is valid in a
URL path segment) and treats them as opaque in its own API. Clients
never parse session ids. Cross-host uniqueness rests on `host_id`,
which is why it names the session store (section 4).

### 6.3 Frames

`GET /v1/events` opens the SSE stream. Each frame is one JSON object,
internally tagged with `kind`:

- `event`: `{kind, session, epoch, seq?, entry_id?, event}` where
  `event` is a serialized `AgentEvent`. `seq` and `entry_id` are
  present if and only if the event is durable (section 6.4). On the
  receive side the nested event decodes through a wire wrapper that
  distinguishes known from unknown event types, the unknown case
  retaining the raw JSON (section 6.10). The envelope's epoch and
  cursor semantics apply regardless of whether the nested event type
  is known.
- `state`: `{kind, session, epoch, working, settings, last_seq}`.
  Structured per-session state: whether the session's **main agent** has
  a turn in flight, the active settings (model identity, thinking,
  thinking display, speed, verbosity), and the current durable
  high-water mark. Sent at the start
  of an attach (before backfill) and whenever any of it changes. This is
  how a mid-session joiner learns the active model and seeds its
  lifecycle spinner, neither of which is derivable from projected events.
  It is also the authoritative carrier of restored settings: a client
  that wants a "restored session" notice renders it locally from this
  frame on first attach, the host publishes no restore notices, so
  nothing can duplicate on reconnect.
  The change trigger is `working` or `settings`, not `last_seq`: every
  durable frame already carries its own position, and `list` carries it
  for sessions a client has not attached, so re-emitting `state` per
  append would double the frame count for nothing.
  `working` is authoritative for the main agent and applies on every
  `state` frame, which is what self-heals a spinner left running by an
  `AgentEnd` a client missed. It says nothing about sub-agents: their
  liveness comes from lifecycle events, from the sub boxes in the
  transcript, and from the host concluding known-idle subs after
  `caught_up`. Scoping it to the main agent is what keeps an on-change
  re-emission from clearing a running background sub-agent's mark, which
  a single session-wide bool would do.
- `caught_up`: `{kind, session, epoch, last_seq}`. Ends a backfill
  (section 6.5). Only meaningful inside an attach block the client
  requested: a client must not commit a cursor from a `caught_up` it did
  not ask for, or it would claim entries it never applied.
- `list`: `{kind, sessions: [...]}`. The full session list with
  per-session status (section 6.8). Cumulative, the latest frame
  supersedes all earlier ones.
- `reset`: `{kind, session}`. Continuity for this session is broken
  (head switch, or a gateway lost and regained its host). The client
  must re-attach the session (section 6.5). Its cursor stays valid to
  offer, the server decides whether it can resume from it.
- `heartbeat`: `{kind}`.

Session-scoped frames carry their session id in a top-level `session`
field. That is a load-bearing convention, not a style choice: it is
what lets a gateway rewrite session ids even in frame kinds it does
not understand (section 6.10).

Unknown frame kinds, unknown keys in a frame, and unknown `event`
types must be ignored by clients (section 6.10).

### 6.4 Reliability classes

Every frame is in exactly one class:

- **Durable** event frames correspond to persisted log entries: the
  `MessageEnd` that triggers persistence, the `SubAgentStart` that
  writes the spawn root, the `CompactionEnd` whose checkpoint entry the
  compaction path appends itself, and the notices the projection derives
  from settings entries (which the host synthesizes live, since no bus
  event exists for those). Durable frames carry `seq` (the entry's
  1-based append position, so `0` reads as "nothing durable yet") and
  `entry_id`. They are exactly what backfill can regenerate. Seqs are
  strictly monotone per session but **not contiguous**: some entries
  project no event (system-prompt roots, seed settings). Clients must
  not do gap detection on seq, continuity comes from the stream being
  FIFO plus explicit `reset` signals.

  At most one frame per log entry is durable. An entry can project
  several events (a tool-result entry projects a tool bracket around its
  `MessageEnd`, an assistant entry projects a trailing `UsageUpdate`) and
  only one of them carries the tag, in both live flow and backfill, so
  that "the cursor is at seq N" stays a statement about entries.

  An entry whose event the host emits itself, rather than deriving it
  from the bus, must be tagged at the append site and reach the fan-out
  while the append still holds the log, otherwise a concurrent
  sub-agent append lands in between and the seqs stop being monotone.
- **Lossy** frames are the three cumulative-snapshot events:
  `MessageUpdate` (keyed by agent id), `ToolExecutionUpdate` (keyed by
  call id), `TaskOutput` (keyed by task id), plus the `list`, `state`,
  and `vms` frame kinds (each its own key). Lossy frames may be
  coalesced or dropped under pressure, correctness never depends on
  them, the newest one or a later durable frame supersedes. New
  capability-gated frame kinds declare their class when introduced.
- **Reliable-transient** frames are everything else: tool
  start/end, sub-agent end, task start/end, notices, warnings,
  errors, lifecycle brackets, usage updates, queue updates,
  compaction progress, `caught_up`, `reset`. Not replayable, but
  one-shot: losing one wedges or corrupts client state (a lost
  `ToolExecutionEnd` is an unrendered tool result, a lost
  `SubAgentEnd` is a spinner that never stops). These must be
  delivered in order or the client must be evicted (section 6.9),
  never silently dropped.

### 6.5 Attach and catch-up

Per session the host maintains:

- **seq**: section 6.4. The client tracks two positions: the last durable
  seq it has **applied**, which is what the cursor invariant below
  compares against, and the last it has **committed**, which is what it
  offers on re-attach. An applied seq is committed once a later durable
  frame or a `caught_up` arrives, because a log entry can project a
  trailing untagged event (an assistant entry's `UsageUpdate`) and a
  connection that drops in between would otherwise leave the client
  claiming an entry it only partly applied. Offering an older cursor is
  always safe: the server serves one more entry and idempotent
  application absorbs it. A `last_seq` merely observed in `list` frames
  for a session the client never attached is glyph data, never a cursor,
  offering it would silently skip that session's entire history.
- **epoch**: an opaque token minted fresh every time a session is
  materialized, and replaced whenever the linearized history changes
  in a way that is not a pure append (head switch to a different
  branch). Epochs are never persisted. A host restart therefore always
  invalidates cursors, which is exactly right: the log tail is not
  crash-stable (buffered non-punctuation entries, torn-tail
  truncation), so post-restart seqs may not mean what they meant
  before. Within one epoch, "durable events after seq N" is
  well-defined.

Attaching is done through the stream itself, there is no separate
snapshot request. The stream request names sessions to attach, each
with an optional cursor: `GET /v1/events?session=<id>[@<epoch>:<seq>]`
(repeatable). For each named session the server, atomically with
respect to that session's event flow: registers the subscription,
projects the durable suffix, and emits in order on the stream: a
`state` frame, the backfill (projected events for entries after the
cursor, or from the beginning when the cursor is absent or its epoch
does not match), and `caught_up`. Live frames for that session follow.
Because subscription and projection are atomic and the stream is FIFO,
no durable event can be missed or delivered below the backfill
boundary within a connection: there is no client-side
buffer-and-reconcile dance. Reliable-transient frames already in
flight in the fan-out at attach time can still arrive after
`caught_up` and overlap what the backfill projected, which is one of
the reasons application must be idempotent (below) on first attach as
well as on re-attach. Lossy frames in flight at attach time are
**dropped** rather than delivered, because a cumulative snapshot
delivered after the durable frame that superseded it resurrects stale
transient state: a `MessageUpdate` for a message the backfill already
finalized would paint a second, unfinalized copy of it. Dropping them
costs at most one coalescing tick of streaming text, which the next
live snapshot restores. A cursor beyond the session's current
`last_seq` is treated as an epoch mismatch (full backfill).

Sessions not named in the request still produce live durable and
reliable-transient frames (it is a unified stream), just without
backfill. Since lossy frames are droppable by definition, the server
may suppress them for sessions a client has not attached, which keeps
host-wide streaming churn from pressuring every client's queue.
Changing the attach set means reopening the stream with new
parameters, reconnection is the normal mode, not an exception.

Backfill projection rules, which differ from dead-log replay:

- The host projects from the full log so server-side context (tool
  name maps, usage running totals) is complete, and emits only events
  for entries after the cursor.
- A **running** sub-agent's bracket is never force-closed, at the cursor
  boundary or at end of log. The real `SubAgentEnd` arrives live later.
  A **finished** sub-agent's bracket still is, because a sub-agent's
  conclusion is not persisted and reconstructing it from the run's last
  assistant message is the only way a finished box gets concluded from a
  log at all. The host therefore tells the projection which sub-agents
  are still running. A sub-agent whose bracket the projection leaves
  open because it is running is not concluded; every other sub-agent the
  host knows to be idle is concluded after `caught_up`, which is what
  unwedges a box whose `SubAgentEnd` fell into a client's disconnected
  window even when no durable entry follows its cursor.
- A sub-agent thread that is open at the cursor boundary gets its
  `SubAgentStart` re-synthesized so the suffix is well-bracketed.
  Synthesized bracketing frames carry no `seq` or `entry_id` (their
  spawn root is at or below the cursor, tagging them durable would
  make the cursor invariant drop them). They are bracketing glue: the
  client ensures the sub's box exists, reusing it when it already
  does, and reads the run the glue opens as in progress.
- Background sub-agents interleave with their parent, so bracket state
  is per run. Closing a run because the next entry belongs to another
  agent would fabricate a conclusion for a live sub and re-open its
  bracket without its task or settings.
- Within a backfill, event order follows the projection (thread
  bracketing), not global seq order. The client treats the backfill
  block atomically: it advances its cursor to `caught_up.last_seq`
  once, not per frame. During live flow the cursor advances per
  durable frame.

Client application rules:

- **Epoch adoption**: the attach block of an attach the client itself
  requested establishes the session's current epoch: the client adopts
  the epoch carried by that block's opening `state` frame and applies
  the block under it. This is what lets a client whose cursor went
  stale (head switch, host restart) accept the full backfill served
  under the new epoch instead of filtering it out.
- **Epoch filter**: every session-scoped `event`, `state`, and
  `caught_up` frame carries `epoch`, and outside an attach block the
  client drops any frame whose epoch differs from the session's
  current one. This is what keeps a stale in-flight frame from an
  abandoned branch out of the new branch's transcript.
- **Cursor invariant**: within an epoch, a durable frame whose seq is at
  or below the last applied seq is dropped as a duplicate, at any time,
  not just during a specific phase. This is a de-duplication
  optimization, not the correctness mechanism. Correctness rests on
  idempotent application, below, because the invariant cannot protect an
  entry's trailing untagged events: a re-served entry whose durable frame
  is dropped still applies its `UsageUpdate` and its tool bracket.
  Adopting a different epoch resets the cursor bookkeeping, since seqs
  from the old epoch say nothing about the new one.
- **Re-attach reconciliation**: when a client re-attaches a session it
  already has state for (after a drop, an eviction, or a `reset`), the
  suffix will re-project things the client partially saw live. Before
  applying the backfill the client quiesces its transient-derived
  in-flight state for that session. Quiesce clears transient *detail*
  and never durable identity or structure: the unfinalized streaming
  assistant entry goes, a running tool cell keeps its `call_id`, tool
  name and arguments but loses the partial result painted by
  `ToolExecutionUpdate`, a running sub-agent box keeps its status and
  child transcript but loses its activity detail, and the
  compaction-in-progress indicator is cleared. Dropping a running tool
  cell outright would lose its arguments for good, because a tool that
  has not finished has no log entry and so no backfill can regenerate
  it. Concluding or dropping a running sub-agent box would either show a
  finished box for a live sub or orphan its child transcript. The host
  is authoritative for concluding sub-agent boxes (the projection rules
  above).

  Application must be idempotent on durable identity, for every
  durable-derived effect and not only the obvious ones: a projected tool
  start for a `call_id` the client already renders updates that cell in
  place, a re-synthesized `SubAgentStart` for a known sub reuses its box
  and marks that run in progress, a re-served transcript row for a known
  message id updates in place, a re-served compaction checkpoint or
  settings notice updates its existing row rather than appending a second
  one. That last pair has no identity on the event itself, so the client
  hands the frame's `entry_id` to its reducer. (This is a deliberate
  hardening of the reducer, which today appends unconditionally.) A
  re-synthesized start marks the run in progress because that is its whole
  purpose: it brackets the events that follow it, and those events only
  land on a running box. A box's report is refreshed from the sub's own
  conclusions only while it is running, so a client that re-attached during
  a continuation would otherwise keep the previous run's report for good. A
  durable `SubAgentStart` names a spawn root instead, and a spawn root is
  minted once per run, so re-serving one for a box the client already holds
  never resurrects its conclusion. After `caught_up` the client refetches
  the task table and the pending-message queues (section 6.7), because
  neither task events nor queue updates are replayable. A client may
  instead discard the session's state and rebuild from a full backfill,
  which must produce the same result, and is the natural choice when it
  has no state yet.
- On `reset`, the client re-attaches (reopens the stream naming the
  session, offering its cursor). The server serves an incremental
  suffix if the epoch still matches, or a full backfill if not.

### 6.6 Commands

Commands are JSON POSTs. Effects are observable on the stream.
Mutations against a specific session return 202 on acceptance.
`POST /v1/sessions` returns 200 with the new session's id. Commands
that today act on "the viewed agent" locally take an optional agent
target (defaulting to the main agent), the remote client resolves its
viewed agent to that parameter:

| Command | Body | Semantics |
|---|---|---|
| `POST /v1/sessions` | optional settings, optional first prompt | Create a session in the host's working directory. Settings resolution per section 8. |
| `.../{id}/prompt` | text or content blocks | Exactly `handle_submit`: run a turn if idle, queue follow-up if busy. |
| `.../{id}/steer` | text, optional agent | Queue steering (or promote pending follow-up when text is empty), as today. |
| `.../{id}/cancel` | optional agent | Cancel the running turn, with the existing foreground-sub-agent-cancels-main cascade. |
| `.../{id}/queue` | op: remove (optional agent) or clear | Withdraw or clear pending queued messages. Withdrawal returns the text, which is what makes the client's dequeue-into-the-editor gesture work. One agent holds at most one coalesced pending message, so there is no index to address. |
| `.../{id}/compact` | optional instructions | Manual compaction. |
| `.../{id}/settings` | model / thinking / thinking display / speed / verbosity changes | Host applies, logs, and emits the synthesized frames. |
| `.../{id}/head` | target entry id | Switch the session head. 409 while working or tasks live. Clears queues, new epoch, `reset` frame. |
| `.../{id}/tasks/{task_id}/kill` | — | Kill a background task. |

Gateway-only additions are in section 7. Session creation through a
gateway takes a target host parameter, because hosts are bound to
working directories.

Request bodies are the `aj-wire` types, which are the source of truth
once landed. Two shapes the spec pins because getting them wrong is
easy: a model change travels as the same (api, url, name) triple that
CLI and env selection use, never as a catalog object (the host
resolves the triple against its own catalog and credentials), and
thinking display is a live-only axis, applied and broadcast via the
`state` frame but not written to the log, matching the local behavior
where it reseeds from config on resume.

### 6.7 Reads

State that is not on the event stream, or that a mid-session joiner
needs on demand:

- `GET /v1/sessions` — the same payload as a `list` frame. Includes
  every session of the host's working directory, on-disk ones as well
  as live ones, with a liveness flag. Attaching or commanding a
  non-live session materializes it (lock permitting). A read never
  does, with one exception: the tree read has to parse the log, so it
  materializes like a command. This is the discovery surface, there is
  no separate on-disk listing.
- `GET /v1/sessions/{id}/tasks` — background task table
  (`TaskRegistry` snapshot, with wall-clock timestamps in the wire
  model, the in-memory form uses `Instant`). Clients replace their
  task table with this after `caught_up`, and ignore `TaskOutput` for
  unknown task ids in the interim.
- `GET /v1/sessions/{id}/tasks/{task_id}` — one task's detailed read:
  status, output tails, byte totals, and the agent report where
  applicable. This is what backs the task-output overlay in connect
  mode, the spill file on the host's disk is not reachable remotely.
- `GET /v1/sessions/{id}/queue` — pending steering and follow-up
  messages.
- `GET /v1/sessions/{id}/tree` — the session branch tree, for the
  tree view and head switching.

### 6.8 Status model

Per-session status in `list` frames and `GET /v1/sessions`:

- `live`: materialized in the host, vs on-disk only.
- `working`: the main agent has a turn in flight (section 6.3). Live
  background sub-agents surface through `tasks`, not here.
- `queued`: counts of pending steering / follow-up messages.
- `tasks`: count of live background tasks.
- `last_seq` and a last-activity timestamp. For a session that is not
  live, `last_seq` is derived from the store rather than reported as
  zero, otherwise the unseen-output glyph below could never fire for a
  session the client has not attached, which is most of them.
- `unreachable` (gateway only): the owning host connection is down.

There is deliberately no "needs attention" bit on the server. A client
derives it: a session that is idle and whose `last_seq` is beyond what
the user has viewed has unseen output, that is the sidebar glyph.
Attention is client-relative state and stays client-side.

`list` frames are lossy-coalescible (section 6.4), and the host
debounces them: `last_seq` churn during a busy turn must not produce a
frame per event, a short coalescing tick bounds the rate. Producing
one must also be cheap, because session events are the frequent
trigger: a refresh does no disk I/O for live sessions (the host
already holds their `last_seq` and status in memory, reading the log
back to recount entries is never correct), and the on-disk remainder
is served from caches invalidated by directory change, never by
session events. Per-file work on refresh (format sniffing, entry
counting) caches on facts that actually change (a file's format
never does). Violating this turns the list publisher into a
steady-state I/O storm: a debounced refresh that rescans a large
session directory reads hundreds of gigabytes over a working day.

### 6.9 Flow control

The agent must never block on a slow client, and reliable frames must
never be silently dropped for a connected client.

- Per attached client, the server keeps a bounded outbound queue. The
  bound governs **live fan-out only**: an attach block (the `state`
  frame, backfill, `caught_up`) is producer-paced, generated and
  written at the pace the client reads it under ordinary HTTP
  backpressure, and is never preloaded into the bounded queue. A big
  resumed session must not evict its own client before the first
  frame is read. Live frames arriving during a backfill queue under
  the bound as usual, and overflow there still evicts.
- Lossy frames are coalesced by their key: a newer snapshot for the
  same key **replaces the queued older one's payload semantics by
  dropping the old frame and enqueueing the new one at the tail**,
  never by in-place substitution (in-place substitution would reorder
  content across a queued durable boundary, briefly painting turn N+1
  text into turn N's bubble). The host additionally coalesces
  streaming updates on a short tick before fan-out, which bounds
  steady-state frame rate.
- Durable and reliable-transient frames are never dropped. If a
  client's bounded queue overflows with them, the server disconnects
  that client. Eviction over buffering, as in Shelley. Recovery is the
  ordinary re-attach with cursor plus reconciliation (section 6.5),
  which is exactly what makes eviction safe.
- The fan-out consumes a channel, never the bus directly, so network
  activity cannot stall or fail a turn. The one inline bus listener is
  the persisting forwarder, which has to be inline: it tags a durable
  event at the append site and hands it on while the append still holds
  the log, which is what makes seqs monotone (section 6.4). Its send is
  non-blocking and a closed receiver is ignored, and **that channel must
  stay unbounded**. Bounding it would put a blocking send under the log
  lock and stall every append in the session, which is exactly what this
  rule exists to prevent. Flow control belongs on the per-client queues,
  downstream of the fan-out.

### 6.10 Compatibility

Both ends of every connection are aj, but versions will skew,
especially with long-lived VMs. Rules:

- `GET /v1/hello` carries `protocol` (integer, starts at 1) and
  `capabilities` (list of strings). The protocol integer only bumps on
  breaking changes, which we intend never to make. Everything else is
  additive and capability-advertised.
- Servers ignore unknown JSON fields in requests. Clients ignore
  unknown fields in frames and responses.
- Unknown tolerance lives at the wire boundary, never in the domain
  enum. `AgentEvent` stays closed (every consumer relies on its
  exhaustive matches and its `agent_id()` contract, which an unknown
  variant cannot honor) and gains strict deserialization of its known
  variants. `aj-wire` wraps decoding for events and for frames: a
  known type decodes strictly (a malformed known event is an error,
  not a downgrade to unknown), an unknown type retains its tag and
  complete raw JSON. Unknown events and frames never reach the local
  event bus or the reducer.
- Client handling of unknowns: an unknown nested event is skipped
  before the reducer, but its envelope still applies, the epoch
  filter runs and a durable unknown event advances the cursor
  (otherwise every reconnect would refetch an event the client will
  never understand). Unknown top-level frame kinds may be discarded
  by endpoint clients outright.
- Gateway handling of unknowns: forward, don't filter. The retained
  raw JSON is re-emitted unchanged except for the session-id rewrite,
  which the top-level `session` convention (section 6.3) makes
  possible without understanding the frame. This is what lets an
  older gateway sit between newer hosts and newer clients.
- New endpoints, frame kinds, and event types arrive with a capability
  string. Probing an endpoint (404 vs 2xx) is a valid fallback check.
- The pinned-shape tests in `events.rs` extend to round-trip tests:
  serialize-deserialize must be identity on the wire-visible parts,
  and decoding must survive forward-compat fixtures (extra fields,
  unknown variants, unknown frame kinds).

### 6.11 Securing the control port

An attached client can run arbitrary commands through the agent, so
the control port is remote code execution and gets SSH-grade
treatment. The protocol itself stays credential-free, protection is
layered around it:

- **Bind discipline.** Hosts and gateways bind loopback or a tailnet
  interface address, never a public interface. The bare `--listen`
  default is loopback. The VM unit's `0.0.0.0` bind is the exception
  and is safe because ember guest networks are host-private (the
  guest IP is unreachable off the VM host).
- **Tailnet policy.** A tailnet's default policy allows every device
  to reach every port of every other device, so deployments must
  restrict the control port with a grant/ACL (deny-by-default once
  defined, enforced on the receiving node): only the owner's devices
  may reach port 6161 on aj hosts and gateways. One trap the sample
  policy must not fall into: tailscale rules are additive accepts,
  there is no deny rule, so restricting the port is impossible while
  a default allow-all rule remains. The sample therefore shows a
  least-privilege replacement, not an addition: aj machines carry a
  tag (e.g. `tag:aj-host`), the owner's devices get their broad
  access spelled out explicitly, and the control port on the tag is
  granted only to the owner, carrying the aj app capability the
  identity gate checks. The reference systemd units (section 7.4)
  ship with that sample. On a strictly single-user tailnet with no
  shared or third-party nodes, allow-all already means "only my
  devices" and the policy change is defense in depth, the identity
  gate is the layer doing the practical work there.
- **Identity gate.** The server verifies every connection's peer
  against the local tailscale daemon: a whois lookup on the remote
  address (tailscaled's local API, what `tailscale whois` wraps)
  resolves the peer's machine, user, tags, and granted capabilities.
  A connection is accepted only when the lookup resolves and the
  peer's user is in the configured allowlist or carries the aj app
  capability granted in the tailnet policy file. Rejections are 403
  and every accepted connection is logged with its resolved identity.
  This is defense in depth against policy misconfiguration and gives
  every action an attributable identity. The gate has three modes,
  configured per process with `--auth <local|tailscale|open>` or
  `AJ_AUTH` (flag wins): `local` (default, loopback peers only),
  `tailscale` (the whois gate), and `open` (explicit opt-out, for the
  host-private VM bind). In `tailscale` mode the allowlist is given
  with repeatable `--allow <login>` or comma-separated `AJ_ALLOW`,
  where a login is the tailnet login name exactly as whois reports it
  (e.g. `alice@github`). Tagged nodes have no login and are accepted
  only via the app capability, whose key is
  `github.com/aljoscha/aj/cap/control`. A connection passes with
  either an allowlisted login or the capability. Serving a
  non-loopback address in `local` mode refuses to
  start rather than silently serving unauthenticated.

What this deliberately does not provide is per-connection human
re-authentication. Tailscale's check mode (browser SSO re-auth on a
`checkPeriod`) exists only for Tailscale SSH, not for generic TCP.
The posture is device-level trust, the same as key-based SSH: tailnet
node-key expiry re-authenticates devices periodically, and the
identity gate names who did what. Whoever wants check-mode semantics
today binds the host to loopback and tunnels the control port through
Tailscale SSH with `action: check`, which composes cleanly with
everything here.

## 7. The gateway

### 7.1 Aggregation

The gateway keeps one **control connection** per enrolled host (its
`/v1/events` stream without session attachments, for the list and
status), from which it maintains the merged directory: namespaced
session ids, merged `list` frames, `unreachable` marking.

Client event streams are **spliced**: for each client stream that
attaches sessions, the gateway opens the corresponding upstream
streams with the client's own cursors and forwards frames with ids
rewritten. The gateway holds no session logs and no cursors of its
own, correctness is inherited from the host protocol. Commands and
reads are proxied to the owning host (503 with a clear code when it is
unreachable).

When a gateway-to-host connection drops, downstream continuity for
that host's sessions is broken even though client connections stayed
up. The gateway must emit `reset` frames for the affected sessions
(and mark them unreachable in the list until the host returns).
Clients re-attach with their cursors as usual, which resumes
incrementally when the host's epochs survived and fully when they did
not. The same mechanism covers the case where a host evicts a slow
gateway.

Enrollment: static host addresses from the gateway config file, plus
dynamic enrollment (VMs it provisions, or explicit
`POST /v1/hosts {address}`). `GET /v1/hosts` lists, `DELETE
/v1/hosts/{id}` removes. Enrolled hosts persist in gateway state so
restarts recover the full set.

Gateway configuration is a TOML file (`--config <file>`, defaulting to
`~/.aj/gateway.toml`): a list of static host addresses, and a
provisioner section selecting the backend with its backend-specific
settings (for local-process: the workspace root to create session dirs
under and optionally the `aj` binary to spawn, for ember: the golden
VM name, default resources, and the VM user). Runtime state (dynamic
enrollments, VM records) lives under `~/.aj/gateway/`. A session host
needs no configuration file of its own: the listen address and the
identity-gate settings (section 6.11), given as flags or environment,
plus its auto-minted per-working-directory `host_id`, is all there
is, the rest is its normal local aj environment.

### 7.2 Provisioning

A backend trait with a deliberately small contract: provision a new
host (returns an address to dial and an id), destroy it, list what
exists. Two implementations:

- **local-process** (first, and permanently useful): spawns `aj serve`
  processes on the gateway's own machine, one workspace dir each. It
  exists for tests and for "many sessions on this box without VMs",
  and it exercises every gateway code path the ember backend needs.
- **ember**: section 7.3.

The wire surface (gateway capability):

- `POST /v1/vms` — multipart: a JSON part with parameters (name,
  resources, optional workspace setup such as a git URL to clone,
  optional project env content, see section 8) and an optional profile
  bundle part (`.tar.gz`, section 8). Returns 202 with a VM id
  immediately, provisioning runs async (it takes seconds to minutes).
- Progress and outcome surface as VM state (`provisioning`, `ready`,
  `failed` with a message, `destroyed`) via `GET /v1/vms` and a lossy
  cumulative `vms` frame kind on the event stream. On `ready` the VM's
  host is enrolled and its sessions appear in the directory.
- `DELETE /v1/vms/{id}` — destroy (destroys the VM, removes the
  enrollment).

### 7.3 The ember backend

Ember is CLI-only by design, the backend shells out and parses
`--format json` where available (the manual flags ember behavior as
"re-verify against the installed ember" since it is a separate
project):

- One-time setup (documented, not automated by aj): build a golden
  image with `ember image build` from a Dockerfile that bakes the aj
  binary, the `aj-serve.service` unit, and sshd. Create and stop a
  golden VM from it.
- Provision: `ember vm cp <golden> <name>` (CoW fork, with explicit
  modest resources, ember's defaults are sized for dev VMs), read
  `guest_ip` from `ember vm inspect --format json` (it can read as
  pending briefly after start), wait for ssh, push the profile bundle
  and run workspace setup via `ember cp` / `ember exec`, start the
  unit, poll `GET /v1/hello` until it answers (that poll, with backoff
  and a timeout, is the readiness protocol, ember has no readiness
  signal), enroll.
- Destroy: `ember vm rm --force`.

Known constraints, accepted for v1: ember needs root on Linux (the
gateway runs as root or with sudoers rules for `ember`), and guest IPs
are only dialable from the ember host (the gateway runs there).

### 7.4 Process supervision

Everything long-running runs under systemd:

- In the VM: `aj-serve.service`, `Restart=on-failure`, journal
  logging, `WorkingDirectory=` the workspace dir, and
  `AJ_LISTEN=0.0.0.0:6161` in the unit environment with the identity
  gate in `open` mode (the guest network is host-private, section
  6.11). 6161 is the fixed control-port convention between image and
  provisioner, the provisioner dials `guest_ip:6161`. The unit is
  baked into the golden image but not enabled at boot, the
  provisioner starts it after the profile bundle lands (the bundle
  must exist before the first session is created).
- On the gateway machine: a documented unit for `aj gateway`. The repo
  ships both unit files as reference material with the docs, aj does
  not install them.

## 8. Configuration provenance

Which files feed a remotely-created session. Three scopes, each with a
natural home:

- **Workspace-scoped** (repo AGENTS.md/CLAUDE.md, project skills and
  `.aj//.agents//.claude` dirs): read from the workspace on the
  machine where the session runs. The VM's checkout is the source,
  nothing is shipped. One deliberate exception: a project `.env` is
  never in the checkout (it is secrets), so a fresh clone has none.
  The provision parameters may carry project env content explicitly
  for that reason, otherwise provisioned sessions simply run without
  project env vars.
- **User-scoped agent behavior**: follows the user who provisions,
  and the user's files live on the **client** machine. The client
  therefore assembles the **profile bundle** and sends it with the
  provision request (section 7.2), the gateway stores it only for the
  duration of provisioning and ships it into the VM. Contents, as a
  `.tar.gz` with a fixed layout: `agents.md` (the global instructions
  file), `skills/` (user skill directories, shipped as real files
  because skills are read from disk at runtime), `config.toml` (the
  agent-behavior subset only: model, thinking, speed, verbosity,
  disabled tools/skills, compaction settings, image handling),
  `models.json`, and `env` (key-value lines destined for
  `~/.aj/.env`). The provisioner unpacks it into the VM user's home
  per that fixed mapping, overwriting whatever the image had (the
  golden image ships none of these). Secrets ride the trusted network,
  v1 accepts that, the keyless-proxy alternative is a recorded
  non-goal.
- **UX-only** (theme, keybindings, `show_*` toggles, syntax
  highlighting, transcript compactness): never leaves the client.
  This falls out of the architecture, rendering happens where the
  human is.

The bundle is a snapshot, not a sync. Re-provisioning or an explicit
re-push refreshes it. When attaching to a host that we did not
provision, the host's own local files apply, provenance rules only
govern hosts we create.

Orthogonal to file provenance, **per-session inference settings
follow the creator**. The host supplies the environment (workspace,
skills, keys, catalog, tool availability), but the model, thinking
level, thinking display, speed, and verbosity of a session belong to
whoever creates it. "Belong" is per axis and hinges on whether the
creator actually stated a value: a setting is stated when it comes
from a CLI flag, an environment variable, or an entry written in the
client's config file. The built-in fallback a config resolves to when
nothing is written is not a preference and does not travel. The
create command sends only stated axes, and stated axes are strict:
a value the host's model cannot serve fails the create with a clear
error naming the supported values, never a silent clamp or
substitution. A stated model must likewise be servable by the host
(present in its catalog, with credentials). Unstated axes are the
host's to default, and its defaulting is model-aware: its own
configured default when the chosen model supports it, otherwise a
supported value. That is not reinterpretation, a default is the
host's to choose precisely because nobody asked for anything. This
split is what lets a stock client create a session on a host serving
a narrow-vocabulary model (a scripted host supports only thinking
`off`) while still refusing loudly when a human really asked for
`xhigh`. After creation the settings command mutates the axes, from
any client, as peers, under the same strictness. Thinking display
sits here rather than in the UX scope because it changes what the
provider is asked to emit, not just what is rendered.

## 9. Client TUI

### 9.1 Connect mode

`aj connect <url>` runs the TUI against a remote server. The reducer,
transcript, overlays, and keybindings are the ones that exist, fed by
decoded frames instead of the in-process channel, submitting via
commands instead of `Turns`. Two client-side model changes this
requires, beyond transport: queue state must live in the client model
fed from `QueueUpdate` frames and the queue read (locally the view
re-reads a live handle at draw time, which does not exist remotely),
and the task table must accept replacement from the tasks read
(section 6.7). Footer settings state comes from `state` frames.

Two things the in-process client used to reach around the protocol
for are resolved by contract now: thinking display is a real settings
axis (command, `state` frame, creator-follows rule, live-only, see
sections 6.6 and 8), and restore notices are client-rendered from the
attach `state` frame, the host publishes none (section 6.3).

Session selection: bare `aj connect <url>` attaches the host's most
recently modified session, and creates one when the host has none.
`aj connect <url> --new` forces creation, and an optional session-id
argument attaches a specific one. This is why session creation is
part of phase 2, a fresh `aj serve` would otherwise be unreachable.

Not every local action can work over the wire, and the boundary is
explicit rather than discovered. Supported in connect mode from phase
2: prompt, steer, cancel, queue withdraw/clear, settings including
model switch and thinking display, compaction, task kill, and the
task-output overlay (backed by the per-task read, section 6.7). The
exit usage banner renders from the client's own event-derived
accounting rather than a host read. Deferred to phase 3 alongside the
sidebar: the session tree view and branching UX (the tree read and
head command exist, the interaction wiring does not). Not supported
over the wire in v1: HTML export and the session-info overlay, both
read host-local files, run them on the host. An unsupported action in
connect mode surfaces a clear notice, it never silently does nothing.

Connection state (connected, reconnecting, catching up) is surfaced in
the footer/status line.

### 9.2 The sidebar

A persistent sidebar (toggleable, hidden by default in plain local
single-session use) lists sessions:

- Against a host or gateway: all sessions from the `list` frames, with
  glyphs for working / idle / unseen-output / unreachable, plus modest
  metadata (age, preview text if cheaply available).
- Switching focus is instant and stateful: the client keeps one
  `ChatState` per session it has attached, live frames keep arriving
  for background sessions over the same unified stream (durable frames
  at minimum, so unseen-output tracking works), and switching is a
  view swap, not a rebuild. Catch-up for a session happens lazily on
  first focus.
- Local single-session mode keeps working exactly as today, the
  sidebar simply has one entry. Creating a new session from the
  sidebar goes through the create command (choosing a host when
  connected to a gateway).

Keyboard model, exact layout, and glyph choices are left to
implementation taste within existing TUI conventions, with one
requirement: new interactions (sidebar toggle and focus, session
switching, remote session creation) are `AjAction`s riding the
existing keybinding system, so they get default chords and user
overrides like every other action.

## 10. Crate layout

- **`aj-wire`** (new): the protocol crate. Frame types, wire versions
  of non-event payloads (list entries, task summaries, tree, hello,
  queue, VM state), the known/unknown decode wrappers for events and
  frames (the unknown case retaining raw JSON for gateway
  forwarding), and entry-id backfill into decoded message ids. Strict
  deserialization of known `AgentEvent` variants lives with the enum
  in `aj-agent`, `aj-wire` owns everything version-skew. No I/O, no
  HTTP types. Both `aj-app` and anything else can depend on it.
- **`aj-app`**: gains the session-host layer (section 5), the reducer
  hardening (idempotent application, quiesce, canonical form for
  tests, section 11), and the client-side session directory / cursor
  bookkeeping. Stays free of TUI and HTTP dependencies.
- **`aj`** (binary): the HTTP server (axum or similar) over the host
  layer, the HTTP client, the `serve` / `connect` / `gateway`
  subcommands and `--listen` flag, the sidebar and connect-mode TUI.
  The gateway's aggregation and provisioning logic may start here and
  move to its own crate if it grows past taste.

The HTTP dependency stays out of `aj-app` for the same reason vaxis
does: the app layer is transport-agnostic, servers and clients are
frontends.

## 11. Testing strategy

Tests come first in each phase (the implementation manual is explicit
about ordering). The layers:

1. **Wire tests**: strict round-trip identity for every known event
   variant and frame kind, pinned JSON fixtures both directions,
   extra unknown fields ignored on known types, malformed known
   events fail decoding rather than degrading to unknown, unknown
   event types and frame kinds decode into the raw-retaining wrappers
   and re-serialize unchanged, durable unknown events keep their
   envelope so cursor progression works, entry-id backfill into
   decoded message ids.
2. **Reducer equivalence** (the core correctness property). The
   comparison operates on a **canonical form** of `ChatState`: a
   projection defined in test support that covers transcript entries
   (including message ids), sub-agent boxes (status, conclusion,
   report), task table, queue state, footer accounting and usage, and
   lifecycle, while excluding wall-clock fields (`Instant`s and
   timings) and client-relative state. `ChatState` is not comparable
   today (no `PartialEq`, and the reducer stamps `Instant::now()`),
   the canonical form is how equality becomes writable. The harness:
   run a scripted-provider session on an in-process host, attach a
   client through the real HTTP stack over loopback, fold received
   frames, and assert canonical-form equality with a locally-reduced
   `ChatState`, comparing at quiescent points (turn ends, session
   end). Then the adversarial variant: inject disconnects at random
   frame boundaries (seeded, shrinking-friendly), forcing re-attach
   with cursors each time, and assert canonical-form convergence at
   quiescence. Transient-only artifacts (notices from dropped
   windows, in-flight streaming text) are excluded from the fault
   variant's comparison by the canonical form's definition.
3. **Reducer hardening units**: idempotent re-application (projected
   tool start for a known call id, re-synthesized `SubAgentStart` for
   a known sub), quiesce behavior, epoch filtering, cursor invariant.
4. **Multi-session host**: concurrent scripted sessions on one host,
   list frame correctness and debouncing, status transitions, session
   materialization and lock conflicts, per-session cursor isolation.
5. **Gateway**: two in-process hosts behind a gateway, id
   namespacing, list merging, command routing, splice forwarding,
   host kill/restart with `reset` emission, `unreachable` surfacing,
   and incremental-vs-full resume after the host returns.
6. **Provisioning**: the local-process backend runs the full
   provision-enroll-create-prompt-destroy cycle in CI (it spawns real
   `aj serve` binaries with the scripted provider), including bundle
   delivery. The ember backend gets a manual integration test (root +
   KVM required), marked ignored, run on the target machine.
7. **TUI**: sidebar rendering and switching via the existing TUI test
   support, connect-mode smoke test against an in-process scripted
   host.

Named cases the reviews identified as the sharp edges, each gets a
test: attach cut between a tool's `ToolExecutionEnd` and its durable
`MessageEnd`, reconnect while a tool and a sub-agent are running
(duplicate-cell and stuck-spinner hazards), attach mid-sub-run (no
force-closed bracket, no spurious failed conclusion), reconnect where
zero durable entries follow the cursor but an open sub concluded in
the gap, head switch refused while busy, stale-epoch frames dropped,
task-table refetch after `caught_up`, queue enqueue visibility on a
second client, slow-client eviction and recovery, settings visibility
for a mid-session joiner, seq non-contiguity tolerated, and the
identity gate (loopback-only refusal in `local` mode, accept and
reject paths in `tailscale` mode against a faked whois resolver).

## 12. Phasing

Each phase lands green (fmt, check, clippy, tests) and committed
before the next begins.

- **Phase 0, wire foundations**: `aj-wire` crate, strict
  `Deserialize` for known `AgentEvent` variants (in `aj-agent`),
  the known/unknown event and frame wrappers with raw retention (in
  `aj-wire`), entry-id backfill, frame types, fixtures. No behavior
  change anywhere.
- **Phase 1, the session host and reducer hardening**: the host layer
  in `aj-app`, multi-session capable, seq/epoch bookkeeping, session
  locks, TUI rerouted through it in-process. Reducer idempotency,
  quiesce, canonical form. No network. Externally invisible, all
  existing tests keep passing.
- **Phase 2, single-session remote**: HTTP server and client,
  `aj serve`, `aj --listen` (flag and `AJ_LISTEN` environment
  variable), `aj connect` viewing/controlling one session (selection
  rule and action matrix per section 9.1), session creation over the
  wire, the per-task read, the full attach/catch-up protocol, the
  connection identity gate (section 6.11), reducer-equivalence
  harness including fault injection.
- **Phase 3, multiplexing**: unified stream with many sessions, the
  sidebar, session switching, the tree view and branching UX over the
  wire, the gateway with aggregation over static hosts plus the
  `/v1/hosts` enrollment endpoints and persisted enrollment state.
- **Phase 4, provisioning**: backend trait, local-process backend
  (with the CI cycle test), profile bundle, the `/v1/vms` endpoints
  and `vms` frames, ember backend, systemd unit files and setup docs.

## 13. Open questions and accepted risks

- Backfill cost: a full backfill projects the whole log, and sub-heavy
  logs make that expensive. The local resume used to defer sub-thread
  projection for this reason and the wire has no deferred variant, so
  the local frontend gave the deferral up when it became a client. v1
  accepts that deliberately: deferral would break the "cursor =
  applied prefix of one seq space" invariant that catch-up, dedup, and
  re-attach reconciliation rest on, turning the cursor into per-thread
  state and multiplying the sharp-edge cases in the protocol's riskiest
  layer. The cost is paid once at first attach, steady-state
  re-attaches are incremental via cursors, and the sidebar attaches
  lazily. A thread-scoped backfill is cleanly additive later (a
  capability) if a real session hurts.
- Suffix projection interacts with interleaved background-sub logs
  (the projection re-opens brackets with synthesized fallback starts).
  The reconciliation rules cover the client side, but this area is
  flagged for extra test attention rather than declared fully solved
  on paper.
- Epoch bookkeeping interacts with session branching, whose spec is
  itself young. The definitions here (epoch per materialization,
  bumped on non-append change, head switch refused while busy) are the
  invariants to preserve, mechanics may adjust as branching settles.
- Frame volume for image-heavy sessions may want compression on the
  SSE response. Deferred until measured.
