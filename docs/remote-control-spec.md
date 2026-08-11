# Remote control and VM provisioning

## Status: draft, phase 3 in progress

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
  shutdown. Materialization has a symmetric release: a live session
  that is quiescent (no turn in flight, no live background tasks, no
  queued messages, queues are memory-only and must not be evicted
  away) and has no attached clients is de-materialized after an idle
  grace period: the turn driver is joined, log buffers are flushed,
  the lock is released, and the epoch dies with the materialization
  (section 6.5). The next attach or command re-materializes with a
  fresh epoch and a full backfill, which the protocol absorbs by
  design, so eviction needs no wire surface of its own. Release costs
  exactly what a process restart plus resume costs, because it is
  one: everything scoped to a materialization dies with it, live
  sub-agent handles (continuing a concluded sub or adjusting per-sub
  settings is refused after re-materialization, as after any resume),
  the background task table, and per-materialization usage
  accounting. Only log-durable state survives, which is the same
  contract resume has always had. One session is exempt from release:
  a created-but-never-durable session (nothing punctuated to disk
  yet) stays live for the host's lifetime, releasing it would orphan
  its id (no file, nothing to re-materialize from), and force-
  flushing it to disk would litter the store with empty sessions.
  Local aj has the same semantics, an unprompted session is
  process-lifetime state that vanishes on exit. The accumulation this
  permits is bounded by deliberate client action, and its lock blocks
  nobody (there is no file for another process to want). If it ever
  matters, the answer is explicit deletion, not release. The grace
  period is implementation taste, but long enough that switching away
  and back does not thrash resume. Eviction serializes with
  materialization per session id, a command arriving mid-teardown
  waits and re-materializes. Attachment is the retention signal:
  remote clients detach by reopening the stream without naming the
  session, and a client keeps a small bounded working set of
  background sessions attached (section 9.2), detaching what falls
  out of it. Retained locks inside the working set are use, not a
  leak. Visiting, though, is not retention: browsing fifty sessions
  must not leave fifty live drivers and fifty held locks behind.
  Without release-on-idle a
  long-lived host monotonically accumulates every session it ever
  touched, holding locks other processes in the same directory need.
- Single-writer safety: materializing a session takes an advisory lock
  on the session, held in a lock file beside the log (the log itself is
  created lazily, so there is not always a file to lock), released on
  teardown. The lock is taken before anything reads or repairs the log,
  because resume truncates a torn tail and repair appends tool results,
  so a refused materialization must not have touched the file. A second process (host
  or plain interactive aj) that hits the lock refuses to materialize
  that session (surfaced as a 409 over the wire). The lock file
  records its holder (pid and host id), and the refusal carries that,
  a user facing "session in use" needs to know which process to go
  quit or detach. This is what keeps
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

Opaque to clients does not mean unvalidated at the server. A host
turns session ids into store filenames and a gateway turns them into
upstream URLs, so a wire-supplied id is
validated syntactically at the boundary (its own id grammar, and
categorically nothing containing a path separator or `..`) and
rejected with 404 before it reaches any path or URL construction or
store lookup. Membership in an enumeration is not a substitute: it
happens to be safe, but it couples path safety to how a lookup is
implemented. Nor is a URL builder: one measured builder silently
drops `.` and `..` segments instead of escaping them, which turned a
crafted id into a different upstream *route* (the create route,
minting a session instead of answering about one) rather than a bad
session lookup. Validation happens before construction, on both roles.

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
  application absorbs it. A `last_seq` merely observed in a `list` frame
  is never a cursor, offering it would silently skip everything the
  client has not applied. Only live rows carry one at all, and attention
  for the rest rides the list's activity stamps (section 6.8).
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

Sessions not named in the request produce nothing on the stream
except their rows in `list` frames. Session-scoped frames flow only
for attached sessions: an unattached session's events would apply to
no client state (and their seqs must not be used as cursors, above),
while its reliable-transient frames are undroppable by class and
would count against the client's bounded queue, so a client could be
evicted over traffic it never asked for. Attention for unattached
sessions rides the list's activity stamps (section 6.8). Changing the
attach set means reopening the stream with new parameters,
reconnection is the normal mode, not an exception.

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
| `POST /v1/sessions` | optional settings, optional first prompt, optional tag | Create a session in the host's working directory. Settings resolution per section 8. Creating the session is what either happens or does not: the settings, the prompt and the tag are validated before a log exists, so a refusal leaves nothing behind. The tag and the first prompt are applied afterwards, under the session's own lock, and are best-effort: one that does not land still answers 200 with the id, plus an `incomplete` field carrying the host's words for what did not stick, so the client retags rather than creating a second session. A minted session is never deleted to make an error tidier. |
| `.../{id}/prompt` | text or content blocks | Exactly `handle_submit`: run a turn if idle, queue follow-up if busy. |
| `.../{id}/steer` | text, optional agent | Queue steering (or promote pending follow-up when text is empty), as today. |
| `.../{id}/cancel` | optional agent | Cancel the running turn, with the existing foreground-sub-agent-cancels-main cascade. |
| `.../{id}/queue` | op: remove (optional agent) or clear | Withdraw or clear pending queued messages. Withdrawal returns the text, which is what makes the client's dequeue-into-the-editor gesture work. One agent holds at most one coalesced pending message, so there is no index to address. |
| `.../{id}/compact` | optional instructions | Manual compaction. |
| `.../{id}/settings` | model / thinking / thinking display / speed / verbosity changes | Host applies, logs, and emits the synthesized frames. |
| `.../{id}/tag` | tag string, empty clears | Set the session's tag: session-scoped display metadata (section 6.8), a single trimmed line, length-capped. Materializes like any command so the session lock covers the sidecar write. |
| `.../{id}/head` | target: an entry id, or `{before: <entry_id>}` | Switch the session head. 409 while working or tasks live. Clears queues, new epoch, `reset` frame. The `before` shape resolves any named entry to its parent server-side, atomically with the switch. An unknown entry is 404, an entry with no parent is refused. Its consumer is the branch-from-a-message gesture (replace the message rather than append after it), but the contract is deliberately not restricted to user-thread entries: every head `before` can reach is reachable through the plain entry shape already, so a restriction would police nothing. |
| `.../{id}/tasks/{task_id}/kill` | — | Kill a background task. |

Gateway-only additions are in section 7. Session creation through a
gateway takes a target host, because hosts are bound to working
directories: the create body carries an optional `host` field naming
an enrolled host, in the same vocabulary the directory rows' `host`
field uses (section 6.8), which is how the sidebar can fill it. On a
gateway the field is required unless exactly one host is enrolled,
then it defaults to that host, ambiguity is refused with a clear
error, never guessed. On a plain host the field is absent or equals
the host's own id, anything else is refused, a host cannot create
elsewhere. No capability string, the field ships inside the
protocol-1 baseline (section 6.10). The response names the session in
the answering server's id vocabulary, namespaced through a gateway.

Request bodies are the `aj-wire` types, which are the source of truth
once landed. Errors cross the wire as a small envelope, not bare
prose: `{code, message, ...fields}`. The `message` is the human
sentence, produced where the facts live and always sufficient on its
own. An unknown `code` renders as its `message` verbatim, so codes
are additive (section 6.10), and a capable client composes its own
wording from the code's fields and can act on the kind, the same
division the rest of the protocol uses, the wire carries structure
and rendering happens where the human is. An error that references a
session carries it in a top-level `session` field, the same
convention frames use (section 6.3), so a gateway rewrites error
bodies with the machinery it already has, without understanding the
code. Codes arrive error-by-error, when a client needs to
distinguish or re-render one, there is no big-bang migration, an
envelope with only a `message` is a complete error.

Two shapes the spec pins because getting them wrong is
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
  tree view and head switching. Carries the session's current head
  entry id (absent only while the log has no head): the overlay needs it
  for active-row pre-selection and for treating a switch to the
  current tip as a no-op, and it is not derivable from the segments,
  a head can sit mid-segment.

### 6.8 Status model

Per-session status in `list` frames and `GET /v1/sessions`:

- `live`: materialized in the host, vs on-disk only.
- `working`: the main agent has a turn in flight (section 6.3). Live
  background sub-agents surface through `tasks`, not here.
- `queued`: counts of pending steering / follow-up messages.
- `tasks`: count of live background tasks.
- a last-activity timestamp (host clock) on every row, and `last_seq`
  only on live rows, where the host holds it in memory. A cold row
  never carries a derived `last_seq`: an exact entry count is O(file
  bytes) to compute (nothing in the log records it), the protocol
  forbids using it as a cursor anyway (section 6.5), and seqs are not
  even stable across materializations. Deriving it made every host
  start read the entire store, gigabytes before first paint. The
  activity timestamp carries the same signal for a stat. The host's
  own answer is the authority wherever it has one: a live row's stamp
  is the last durable event it observed, and a release hands that same
  stamp to the cold row and pins it against the file it left behind,
  so the row does not move over a liveness flip in either direction.
  The log's mtime stands in only where the host has no answer, which
  is every session on a host that just started.
  The two are not interchangeable, which is why the host's wins. The
  mtime answers when bytes landed: it runs a little ahead of a durable
  event the host has not observed yet, and a long way behind one whose
  entry was buffered, since a release's teardown flush writes those a
  whole idle grace after the work that produced them. A row stamped
  from that flush would announce unseen output on a session that
  produced none. The guarantee is therefore process-local: a host that
  restarts falls back to the mtime and can differ from what the
  previous host published, as can two hosts listing one store.
- `tag`: optional user-set label, display metadata, never an id. A
  tag is session-scoped, deliberately not branch-scoped, so it lives
  beside the log rather than in it: a sidecar file
  (`meta/<session id>.tag` in the store, single line of UTF-8,
  written atomically) that a head switch cannot move. Untagged
  sessions have no file. Set at create (create body / launch flag) or
  by the tag command (section 6.6).
- `host`: which enrolled host a row belongs to, filled by a gateway
  and absent from a plain host's rows. Clients group by it and must
  not derive it from the id, ids are opaque (section 6.2).
- `unreachable` (gateway only): the owning host connection is down.

There is deliberately no "needs attention" bit on the server. A client
derives it from seqs, not stamps: a session is unseen when the last
durable seq the client has evidence of (applied frames while
attached, `last_seq` on live rows while not) exceeds the seq the user
had viewed. Stamps are the wrong instrument here, a viewed stamp
recorded from a debounced row predates output the user just watched
and lights the glyph on it. This is also what section 6.5's "glyph
data, never a cursor" was always pointing at: a list-observed
`last_seq` now literally is glyph data. Unseen **latches** client-side: once
derived from a live row it holds until the user views the session,
so the session going cold afterwards (its row loses `last_seq` by
design) does not clear it. The one hole is a session whose entire
unseen window fell inside a client disconnect and that went cold
before reconnect, it shows no glyph until opened. Accepted:
attention is ephemeral client-relative state, it does not survive a
client restart either, and re-deriving it from cold stores is the
entry-count reading this design already refused. A session the user
has never viewed reads as having nothing unseen: the glyph answers
"did this move since I last looked", and with no last look the
question is vacuous. The loud alternative would light every row of a
store on first connect, drowning the signal the glyph exists to
carry. Attention is client-relative state and stays client-side.

`list` frames are lossy-coalescible (section 6.4), and the host
debounces them: `last_seq` churn during a busy turn must not produce a
frame per event, a short coalescing tick bounds the rate. Producing
one touches memory only. The host is the single writer of its working
directory's session store (section 5), so cold sessions do not change
behind its back and their metadata needs no continual freshness. The
event-triggered refresh path composes the frame from the live
sessions' in-memory state plus a cold-session cache and performs no
filesystem work at all, not even stats. The cold cache is (re)built
at enumeration points, which are rare and externally paced: host
startup, an explicit `GET /v1/sessions`, a stream attach. The host's
own structural changes reach the directory without enumeration: a
created session appears through the live map, and a release writes
the session's final row into the cold cache from the driver's own
state (deletion, should it ever exist, would remove the row the same
way). A concurrent writer
in the same directory is a conflict the session locks exist to
surface, not a workload to poll for, its sessions cannot be served by
this host anyway, and its activity becomes visible at the next
enumeration point. Reading a log to produce a directory row is never
correct, live or cold: a live session's facts are in memory, and a
cold row needs only enumeration metadata plus two small cached reads,
the format sniff (first line, cached against the file it was taken
from so a settled store re-sniffs nothing, keyed on the file rather
than the path alone so a sniff that landed on a log another process
was midway through writing recovers) and the tag sidecar where one
exists (section 6.8). Enumeration is therefore readdir,
stats and a sniff per file that moved, cheap enough to run
synchronously at startup.
This contract has teeth on both axes. The polling variant of this
design read hundreds of gigabytes a day re-deriving an unchanged
400-file directory, and deriving a cold `last_seq` once, at startup,
still put the whole store's bytes in front of the first frame.
A refresh whose payload a subscriber has already
been sent is not sent to it again, `list` is cumulative and an
unchanged snapshot carries no information. The comparison is per
subscriber and against what was actually delivered: a freshly
attached subscriber has been sent nothing, and a snapshot a client's
bounded queue dropped was not delivered either. A single host-global
"last published" memory could keep neither promise, it would starve
a fresh subscriber on a quiet host and strand a client whose queue
dropped a frame.

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
  possible without understanding the frame. Unchanged means
  structurally: JSON key order is not significant and byte identity
  is not required. An unknown frame with no top-level `session` field
  is host-scoped and forwarded as is. This is what lets an
  older gateway sit between newer hosts and newer clients.
- The same discipline governs a gateway editing a request or response
  body (create's `host` field going up, the session id coming back)
  **and any wire object a gateway re-emits under its own name, the
  merged directory's rows included**: parse only the top level, keep
  every other value as raw JSON, edit
  the named fields, re-emit structurally unchanged. A typed decode
  and re-encode would silently drop a newer client's fields or refuse
  a body an older gateway cannot parse, reintroducing exactly the
  version ceiling this section exists to prevent. A row merge is not
  exempt because it is a merge: the gateway reads the fields it
  routes on and rewrites the ones it owns (id, `host`,
  `unreachable`), everything else passes through raw. This applies to
  every body-editing route, present and future.
- New endpoints, frame kinds, and event types arrive with a capability
  string. New means relative to a released baseline: protocol 1
  implies the whole section 6 surface as it stands at first release,
  and capabilities exist for what comes after, which is why `hello`
  advertises none today. Probing an endpoint (404 vs 2xx) is a valid
  fallback check.
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
and mark them unreachable in the list while it still has their rows.
Across a gateway restart it does not: the gateway stores no rows,
deliberately, so a host that is down when the gateway comes back has
no sessions to mark. The signal survives anyway because a gateway's
`list` frames carry the enrolled hosts with their reachability
alongside the rows (additive, gateway-only), and a client renders an
unreachable host it holds no rows for as an empty group rather than
as nothing. Honest absence beats a cached directory: persisted rows
could name sessions that no longer exist, and a stale "maybe" is
worse than a clear "unreachable, contents unknown".
Clients re-attach with their cursors as usual, which resumes
incrementally when the host's epochs survived and fully when they did
not. The same mechanism covers the case where a host evicts a slow
gateway. Removing an enrollment is active teardown, not bookkeeping:
the upstream connections close, the host's sessions leave the merged
list, and its splices end. Leaving them would serve a directory that
contradicts the enrollment set.

The gateway does not re-open that upstream itself. Resuming one needs a
*current* cursor, and the client's cursor advances as it applies the
frames the gateway forwarded, so tracking one would give the gateway
per-session cursor state this section denies it, and put a second,
subtly different cursor authority in the system. `reset` plus the
client's own re-attach is the whole mechanism, which is why `reset` is
emitted on two edges: the host was lost, and the host came back. The
control connection is the reachability oracle for the second, since it
redials on its own and its return is what makes an upstream attach
succeed again.

A client that attaches sessions on a host the gateway currently cannot
reach is **not** refused: those sessions contribute no upstream and
stay `unreachable` in the list, which is the signal that they carry
nothing, while the sessions of every other host on that stream are
served normally. Failing the whole stream would punish those. Nor does
the gateway spin on `reset` for them, it waits for the host's return
edge. A host the gateway believes is reachable and which then does not
answer the attach is the ordinary 503 instead: nothing has told the
client those sessions are unreachable, so a stream that carried them
silently would leave it watching frames that never come.

Flow control on a client stream is section 6.9's, with the gateway as
the server: a bounded queue per client, lossy frames coalesced by key
and dropped at the bound, durable and reliable-transient frames never
dropped, and a client the gateway cannot keep up with evicted. One
difference in mechanism, none in rule: on a host the attach block is
paced by its own producer, on a gateway the block arrives over the
upstream connection, so pacing it means not reading that connection
until the client reads. Measuring a block against the bound instead
would evict the very client that asked for it, and the re-attach would
do the same again.

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
VM name, default resources, and the VM user). Runtime state (the
gateway's own id, dynamic enrollments, VM records) lives under
`~/.aj/gateway/`. The id is there because `hello` carries one for a
gateway as much as for a host (section 6.1) and a gateway has no
session store to name it. A session host
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
accounting rather than a host read. One stated limitation of that
choice: the accounting counts what the client observed under its
current epoch, so spend on a branch abandoned by a head switch is not
re-derivable after the reset and the banner under-reports it.
Accepted, an exit banner does not earn host-authoritative usage on
the wire, and such a read is the named fix if it ever matters.
Not supported
over the wire in v1: HTML export and the session-info overlay, both
read host-local files, run them on the host. An unsupported action in
connect mode surfaces a clear notice, it never silently does nothing.

Connection state (connected, reconnecting, catching up) is surfaced in
the footer/status line.

### 9.2 The sidebar

A persistent sidebar lists sessions. It shows itself by default
whenever the directory offers more than one session and stays hidden
otherwise (a lone local session has nothing to choose between), and
an explicit toggle wins over the default for the rest of the process.

- Against a host or gateway: all sessions from the `list` frames, with
  glyphs for working / idle / unseen-output / unreachable, plus modest
  metadata (age, preview text if cheaply available).
- Switching focus is instant and stateful: the client keeps one
  `ChatState` per session it has attached, live frames keep arriving
  for background sessions over the same unified stream (attachment is
  what makes them arrive at all, section 6.5, and it is what section 5
  counts as use), and switching is a view swap, not a rebuild. A
  session the client has not attached shows only its `list` row, which
  is where its unseen-output glyph comes from (section 6.8). Attaching
  is what a first focus does, and the attach block it earns is that
  session's catch-up, so a session the user never opens is never
  projected.
- Background attachment is a bounded working set, LRU over focus. The
  bound is an implementation constant with its tradeoff stated: large
  enough that juggling a handful of sessions never pays a re-attach,
  small enough that a browse through the store does not pile up live
  drivers and locks on the host (section 5). The focused session is
  never detached. A session that falls out of the set is detached,
  the host releases it after its idle grace, its lock frees, and its
  row keeps carrying the attention signal like any unattached
  session. Re-focusing it costs an ordinary re-attach, incremental
  when its epoch survived, a full backfill when the host released it.
  Detaching may keep or drop the client-side `ChatState`, dropping
  is always safe because re-attach reconciliation absorbs a rebuild
  (section 6.5).
- The strip is an orientation instrument, not a store browser, the
  session selector is the browser. Rows order by last activity,
  newest first, client-side (ties broken by id), the view windows
  around the focused row so focus is always visible, and the stepping
  chords walk the displayed order. Focusing a cold session
  materializes it, that is what focusing means. The working set is
  legible: rows distinguish attached sessions from merely listed
  ones, so what the client holds open (and the locks that retention
  implies, section 5) is visible rather than inferred.
- Switching and creating are never refused because a turn is running:
  a background session keeps folding and its turn completes, that is
  the point of the swap model. The busy refusal that remains is the
  host's own head-switch 409 (section 6.6), which is about mutating a
  busy session's history, not about looking elsewhere.
- Local use keeps working exactly as today: with one session the
  strip stays hidden and nothing changes. Creating a new session from
  the sidebar goes through the create command (choosing a host when
  connected to a gateway).

Keyboard model, exact layout, and glyph choices are left to
implementation taste within existing TUI conventions, with one
requirement: new interactions (sidebar toggle and focus, session
switching, remote session creation, session tagging) are `AjAction`s
riding the existing keybinding system, so they get default chords and
user overrides like every other action. Pointer gestures are a second
trigger for the same actions, never a separate behavior: a click on a
sidebar row dispatches the switch action that the chord dispatches.

Rows show the session's tag where one is set (section 6.8), falling
back to the id-derived label. A tag is set at launch (`--tag` on `aj`
and on `aj connect --new`, riding the create command's tag field) or
on the focused session through the tag action.

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
- `list` frames are cumulative over the whole store, so their size
  grows with session count (~60 KB at 400 sessions). Per-subscriber
  suppression and durable-event pacing bound the rate, not the size.
  A row cap or delta encoding is the follow-up if a real store gets
  big enough to hurt.
- Banked, wanted: previews in connect mode. Rows deliberately carry
  no preview text (section 9.2's "if cheaply available" resolved to
  "not over the wire"), but the selector is poorer for it remotely.
  The enumeration contract (section 6.8) forbids content reads per
  row, so the design is one of two shapes, decided when picked up:
  fold preview capture into the same fingerprint-cached head read
  that sniffs the format (enumeration-paced, amortized free), or an
  on-demand per-session preview read issued by the selector for
  visible rows (user-paced, no standing cost).
- Banked, wanted: prompt history across hosts. The overlay scans
  only the local store's logs, so prompts typed into remote sessions
  are invisible locally, and the client trends toward a disposable
  terminal (eventually a web UI), so user-scoped state cannot live
  only client-side, which also rules out a client journal as the
  fix. Candidate shape: a capped, user-paced history read on the
  host (the same scan the local overlay runs, behind an endpoint,
  with a capability string per section 6.10), the client merges
  sources and dedupes, a gateway merges per-host reads naturally.
