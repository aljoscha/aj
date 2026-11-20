# Remote control protocol

This document is the authoritative contract for the wire protocol, the
session host, the gateway, and the connected client. A change that
alters behaviour it describes updates the relevant section in the same
change. Implementation detail belongs to module documentation.

## 1. Overview

A **session host** serves the live sessions of one working directory
over a control port. A **gateway** aggregates many hosts behind one
address and serves the same session-facing API. A **client** is the aj
TUI attached over the network instead of in-process, with a sidebar over
every reachable session. The wire carries the `AgentEvent` stream that
drives the local UI, and a client folds it through the same reducer that
local resume uses.

**The local TUI and remote clients are peers.** Both attach to a session
host through the same interface, both submit through the same command
path, and conflicts resolve through the agent's queueing semantics (busy
agent, message queues, steering). There is no owner of a session beyond
the host process itself.

## 2. Non-goals

- Transport encryption. aj never terminates TLS. The control port is
  protected by bind discipline and a connection identity gate (section
  5.11), not by credentials in the protocol.
- Per-connection human re-authentication. The posture is device-level
  trust, as with key-based SSH. Whoever wants check-mode semantics binds
  the host to loopback and tunnels the port through Tailscale SSH with
  `action: check`.
- Tool approvals or any client-to-agent mid-turn decision channel.
  Runtime status reports activity and turn outcomes, not approval requests.
- A web UI. HTTP plus SSE leaves that door open, nothing here builds
  toward it.
- Provisioning hosts or VMs. A gateway enrolls hosts that exist.

## 3. Roles and topology

- **Session host**: `aj serve` (headless) or `aj --listen[=<addr>]`
  (interactive TUI plus embedded server). The address comes from
  `--listen` or `AJ_LISTEN` and defaults to `127.0.0.1:6161`. A host
  serves the sessions of exactly one working directory, the one it was
  started in, and persists a stable random `host_id` in that store (a
  `host-id` file in `~/.aj/sessions/<project>/`). The id names the
  store, not the process, which is what makes `<host_id>:<session_id>`
  globally unique (section 5.2).
- **Gateway**: `aj gateway [--config <file>]`, listening on the same
  `--listen` / `AJ_LISTEN` address. Serves the same session-facing API
  as a host, proxying to the hosts behind it, and adds host enrollment
  endpoints (section 6.1). Clients distinguish the two only by `hello`.
- **Client**: `aj connect <url>`. The normal TUI, rendering from frames
  received over the wire and submitting via commands.

A gateway depends only on the session-facing API, so nothing precludes a
gateway fronting another gateway.

## 4. The session host

The host opens and writes sessions in one SQLite database. A legacy JSONL
session is listed from its file and imported when it is first resumed.
Session discovery and lookups are scoped to the host's project. Explicit
all-project prompt-history queries read across projects. See
[session storage](session-storage.md) for the layout, commit guarantees,
import rules, and what a reopen reads.

The session host owns N live sessions and exposes one attachment
interface used by both the local TUI (in-process, through direct
handles) and the network server. Both reach the same command methods.
The host layer depends on no terminal, which is what `aj serve` is.

- **Lifecycle.** Sessions are materialized on demand (an attach or
  command naming a session known on disk) and torn down on shutdown. A
  live session that is quiescent (no turn in flight, no live background
  tasks, no queued messages) and has no attached client is released
  after an idle grace period: the driver is joined, the lock is
  released, and the epoch dies. The next attach or
  command re-materializes it with a fresh epoch and a fresh backfill.
  Only log-durable state survives a release, as on a process restart.
  Attachment is the retention signal (section 8.2). A created session
  with nothing durable on disk is exempt and stays live for the host's
  lifetime.
- **Single-writer safety.** Materializing a session takes an advisory
  lock (a file in the store's `locks/` directory) before anything reads
  or repairs the log, so a refused materialization has not touched the
  session. A second process that hits the lock refuses to materialize that
  session: HTTP 409 `locked`, or the `error` frame of section 5.5 on an
  attach. The lock file records its holder (pid and host id) and the
  refusal names it.
- **Sequence assignment.** Durable events are tagged with their log
  entry's append index at the append site, never inferred from log
  length at consume time. A hosted session's log has one writer. It
  appends an entry and hands what the entry publishes to the session's
  publisher under one lock, and the publisher learns of entries from
  nowhere else. So per session, **live** durable frames reach any
  given stream in strictly increasing seq order, and never at or below
  the stream's backfill boundary. Backfill blocks follow projection
  order (section 5.5).
- **Commands.** Section 5.6. Settings changes are applied by the host,
  written as settings log entries, and published as the projected notice
  event plus a refreshed `state` frame. A settings entry that lands
  before its thread's first message projects no notice, so its
  confirmation is published untagged.
- **Head switching.** Refused with 409 `conflict` unless the session
  is quiesced: no turn, no agent run, no Code Mode cell and no background
  task of any agent runs, no message is queued, and no task notification or
  restored pending work waits to be delivered. The refusal says what is in
  the way and what clears it: stopping the task, the `continue` action, or
  withdrawing the queued message. So nothing runs or waits on a branch the
  session is not on, and a switch carries nothing over. A switch stores
  nothing, so a host restart does not remember it unless something was done
  after it: the session then opens on the thread last appended to. A
  switch to an earlier entry starts the draft of a branch, which the first
  prompt there creates ([session storage](session-storage.md)). A switch
  happens whole or not at all. Whatever can refuse it is checked before
  the head moves, and a refused switch leaves the session as it was. The
  one step that can fail afterwards is the write of what the switch
  records on the branch it lands on: the changes it was given, a paused
  goal, the answer to a tool call left open there. If that write fails, the
  host ends the session with a `persistence_failed` error, as after a
  storage failure, and the client's next attach opens it on the branch the
  store has. No client is left on a branch the host is not on. On
  success the host drops the finished task entries of the branch it left,
  mints a new epoch, and emits `reset`. A sub-agent the new branch does not
  show is no longer prompted: a command that names it is refused with 409
  `conflict`. One it shows is prompted as before
  ([sub-agent steering](subagent-steering-spec.md), section 4).
- **Host-level state.** The session directory (section 5.8), a
  `QueueUpdate` on every queue mutation a client can see (a message left
  waiting behind a running turn, a withdrawal, a drain), per-session `state`
  frames (section 5.3), and epoch/seq bookkeeping. A prompt to an idle agent
  is pending only until its turn takes it and is not announced as queued
  ([queued messages](message-queue-spec.md), section 2).
- **Shutdown** (SIGTERM, or the interactive host quitting) leaves each
  session as a host that died at that moment leaves it. A command the
  session is in the middle of ends first. Then the session is closed for
  writing, its turns are stopped where they stand, background agent runs
  and Code Mode cells end, and client streams are closed. Nothing is
  written after the close: no aborted message, no cancelled tool result, no end of
  a turn. A client sees its stream end, as when the host is lost.
  Recorded `bash` commands keep running, and the next open of the session
  settles what was left and takes them up
  ([background tasks](background-tasks-spec.md), section 5). An idle
  release, a storage failure and a dropped session stop the same way.
  Sessions wind down
  concurrently. The process gives orderly teardown one 30-second grace,
  then exits nonzero with a diagnostic if cleanup is unfinished. Another
  stop signal during teardown exits immediately. Forced exit can abandon
  subprocess cleanup. Every write is committed before it is acknowledged
  and a session is closed between writes, so no exit loses an acknowledged
  write. In-process session cleanup
  retains writer locks until its tasks have let go,
  without its own host-wide deadline. The gateway uses the same process
  exit policy.
- **Persistence failure.** A session-log write error is terminal
  for that materialization: the log refuses every later write. The host
  publishes one `error` frame with code `persistence_failed` on every
  stream attached to the session, detaches the session from those
  streams, and tears it down. The stream stays open and a later attach
  or command re-materializes the session from disk, recovering the saved
  prefix even if the first write left an empty or incomplete log. Creation
  settings that never reached disk cannot be recovered. If saving failed
  before opening the log, the message directs the user to start a new
  session. An id with no log answers `unknown_session`. Fatal turn failures
  that do not compromise persistence stay scoped to the turn.
- **Mid-turn compaction.** With host config `auto_compact_during_turn = true`
  (default `false`) and `auto_compact = true`, Main checks provider-reported
  input occupancy against `compact_threshold` after a full tool batch and
  before its next inference. Compaction settings are read at each check, so
  settings edits apply within a running turn, including enabling or disabling
  mid-turn compaction. A compaction already underway keeps its settings.
  Streaming responses are never interrupted.
  Compaction uses the ordinary compaction events and keeps the same turn active,
  with `working` remaining true through successful compaction and continuation.
  Failed compaction preserves the original history and stops continuation,
  including overflow recovery, without automatically retrying compaction.
  Queued input and task notifications remain pending until an explicit start, such
  as a prompt, manual compaction, or goal resume. The hold is in-memory only.
  The failure is recorded as a display-only log entry,
  so reconnect and session replay show the same transcript warning. Replaying
  a failure does not reinstate the hold or change model context.
  Post-turn threshold triggering is unchanged.

## 5. Wire protocol

### 5.1 Transport

HTTP. Commands are JSON POSTs, reads are GETs, effects arrive on an SSE
stream as one `data: <json>` frame per line. No WebSockets, no JSON-RPC,
no correlation ids. All routes live under `/v1/`. Unknown query
parameters are ignored.

`GET /v1/hello` is the reachability and identity probe: `{protocol,
app_version, host_id, working_directory?, name?}`. A gateway omits
`working_directory` and `name`, which is how a client tells the two
roles apart. `app_version` is display text naming the build, the cargo
version and the git revision it was built from, as in
`0.1.0 (8cff8cc1abcd)`. Clients show it and never parse or compare it.
Compatibility is `protocol`'s job.

A host's `name` is display metadata, never an address, and may collide.
The host states it at startup (`--name` or `AJ_NAME`, else its working
directory abbreviated to `~` under home), and there is no rename over
the wire. A name is one trimmed line of at most 80 bytes with no control
characters, which a reader enforces itself. Absent means the reader
falls back to the id.

SSE streams send a `heartbeat` frame after 30 seconds of idleness, and
clients treat 60 seconds of silence as a dead stream. A host sends no
heartbeat on a stream while an attach block it owes that stream is
incomplete (section 5.5). Error responses
carry `{code, message}` (section 5.6 for the envelope rules):

| Status | Code | Meaning |
|---|---|---|
| 400 | `invalid_request` | Malformed request: bad JSON, unknown field, bad cursor, a body naming two settings axes, an empty prompt, a session named twice in one attach. |
| 403 | `forbidden` | The identity gate refused the peer (section 5.11). The message never says why. |
| 404 | `unknown_session`, `unknown_task`, `unknown_entry` | The named thing does not exist. A syntactically invalid session id is `unknown_session`. |
| 404 | `unknown_endpoint` | No such route on this peer. A client reads it as the peer lacking the feature (section 5.10). |
| 409 | `conflict` | Well formed but refused by session state: a turn running, tasks live, a running mark with no owning turn or task. |
| 409 | `locked` | A rival writer holds the session's lock (section 4). |
| 409 | `unsupported` | Well formed and unconflicting, but this host cannot serve it: a model it has no credentials for, a settings change for an agent that is not live. |
| 500 | `internal` | Host or gateway failure. |
| 503 | `host_unreachable` | Gateway only, on a proxied command, read or create: the owning host's control connection is down or the host did not answer in time. On the event stream the same code is a per-session `error` frame instead (section 5.3). |

A gateway adds its enrollment codes (section 6.1).

### 5.2 Sessions and addressing

A session is addressed by an opaque string id. On a host this is the
store's session id (the timestamp filename stem). A gateway namespaces
ids as `<host_id>:<session_id>` and treats them as opaque in its own
API. Clients never parse session ids.

Opaque to clients does not mean unvalidated at the server. A host turns
session ids into store filenames and a gateway turns them into upstream
URLs, so a wire-supplied id is validated syntactically at the boundary
(the store's id grammar: non-empty, length-capped, ASCII alphanumerics
plus `-` and `_`) and rejected with 404 `unknown_session` before it
reaches any path or URL construction or store lookup, on both roles.

### 5.3 Frames

`GET /v1/events` opens the SSE stream. Each frame is one JSON object,
internally tagged with `kind`:

- `event`: `{kind, session, epoch, seq?, entry_id?, branch_settings?, event}` where
  `event` is a serialized `AgentEvent`, except that a `message_update`
  omits the in-process event's `message`: it repeats `event.partial`,
  so the receiver rebuilds it as that partial wrapped as an assistant
  message. `seq` and `entry_id` are present
  if and only if the event is durable (section 5.4). The envelope's
  semantics apply whether or not the nested event type is known to the
  receiver. Durable main-thread user-message ends carry `branch_settings`:
  the recorded model (`{api, name}`), thinking, speed, verbosity, their independent
  `oracle_model`, `oracle_thinking`, and `oracle_verbosity`
  counterparts, and provider account pins at that message's parent.
  Unrecorded inference axes are omitted,
  distinct from explicit `off` or `default`. An absent provider pin follows the
  provider's current default. This is historical context, not a prediction of
  the host's runtime fallback or validation. It contains no environment values,
  credentials, or live-only thinking display. Live delivery and backfill supply
  the same context, including across compaction. Older hosts may omit the field.
- `state`: `{kind, session, epoch, opens_block?, working, turn_outcome?, turn_interrupted?, settings, oracle_settings?, goal?}`.
  `opens_block: true` marks the `state` that opens an attach block
  (section 5.5), absent reads false. It is the only thing that tells a
  block's opening from an on-change `state`, so a client reads it off the
  frame and never infers it from having asked.
  `turn_outcome` optionally reports how Main's last driven turn ended:
  `completed`, `cancelled`, `failed`, or `waiting` (an explicit yield).
  It is null while Main is working, absent or null when unknown, and not
  restored from the log. Attach openings carry the current outcome so a
  yielded goal remains distinguishable from one continuing between turns.
  A client may announce outcomes of turns it observed starting live, but must
  not announce catch-up or its post-`caught_up` repair as new completion.
  Tool failures alone are not failed turns, and waiting does not imply a need
  for user action.
  `turn_interrupted: true` says the process that ran Main's last turn ended
  before the turn did, absent reads false. It is read from the turn state
  stored with the thread the selected branch ends in, so it survives a
  host restart and holds until a turn starts there. A client shows a notice for it after a block that
  rebuilt its transcript.
  `working` says whether the session's **main agent** has a turn in
  flight, `settings` is the active `AgentSettings` (`provider`,
  `model_id`, `thinking`, `thinking_display`, `speed`, `verbosity`, `context_window`),
  `oracle_settings` carries the independently staged Oracle bundle in the same
  `AgentSettings` shape. Hosts with Oracle support always supply it. Older hosts
  may omit it.
  Both identities are staged for the next main turn, including runtime fallbacks,
  unlike the recorded facts in session-info and `branch_settings`.
  `context_window` is the capacity in tokens of the host's resolved model bundle.
  Clients use it for context occupancy and warning thresholds, never a client
  catalog lookup. Missing or zero capacity suppresses the indicator. Subagent
  spawn settings carry the same metadata, retained in the session log for replay.
  Subagent model and thinking changes publish `sub_agent_settings` events with
  `child`, confirmation `text`, and flattened `AgentSettings`. The snapshot
  replaces that child's footer settings without clearing its measured usage.
  Replay and synthesized continuation starts preserve these changes. Main's
  footer settings come from `state`, not historical settings entries.
  Sent at the start of every attach block, before the backfill and marked
  `opens_block`, and unmarked whenever `working`, `settings`,
  `oracle_settings`, or `goal` changes. The host publishes no
  "restored session" notice, a client renders one from the first
  attach's `state`. `working` applies on every `state` frame and
  self-heals a spinner left running by a missed `AgentEnd`. It says
  nothing about sub-agents, whose liveness comes from lifecycle events.
- `caught_up`: `{kind, session, epoch, last_seq, history_start, tasks,
  queues}`. Ends a backfill (section 5.5). `history_start` is present on a
  fresh backfill only: the log position of the first entry the block
  covered. Above `1`, earlier history exists and the client fetches it in
  pages (`GET .../history`, section 6). A client must not commit a cursor, or take
  the tables, from a `caught_up` it did not ask for. `tasks` is the
  session's background task table, `[{id, owner, call_id, kind, label,
  status, started_at}]` with wall-clock `started_at`, running and
  retained finished tasks alike. `queues` is the pending messages per
  agent, `[{agent_id, steering, follow_up}]`, main first, listing only
  agents with something pending. Neither is in the log, so a backfill
  cannot regenerate them and the block carries them here instead. The
  client replaces its task table and queues with them, and ignores
  `TaskOutput` for task ids the table does not list.
- `list`: `{kind, sessions: [...], hosts?: [...]}`. The full session
  directory (section 5.8). `hosts` is present only from a gateway
  (section 6.1). Cumulative, the latest frame supersedes all earlier
  ones.
- `error`: `{kind, session, code, message}`.
  The error envelope (section 5.6) as a session-scoped stream frame.
  Every per-session resolution failure travels this way with its own
  code (`unknown_session`, `locked`, `persistence_failed`,
  `host_unreachable`, ...). Only failures of the request itself (a
  session named twice, a shut-down server, a malformed cursor) fail the
  request. `host_unreachable` comes only from a gateway: the session's
  host cannot be reached now, which says nothing about the session
  itself, and the gateway announces the host's return with `reset`
  (section 6.1).
- `reset`: `{kind, session}`. Continuity for this session is broken
  (head switch, or a gateway lost or regained its host). The client
  re-attaches, offering its cursor (section 5.5).
- `heartbeat`: `{kind}`.

Session-scoped frames carry their session id in a top-level `session`
field, which is what lets a gateway rewrite ids in frame kinds it does
not understand. Unknown frame kinds, unknown keys, and unknown `event`
types must be ignored by clients (section 5.10).

`MessageEnd.message` also accepts an audit-only message variant:
`{role: "tool_activity", cell_id: string, message: Message}`. The `cell_id`
identifies the execution cell issuing the nested call. The inner message is an
ordinary assistant message containing a single tool call, or its ordinary
tool-result message. Call ids match the existing `ToolExecutionStart/End`
events, which render the activity without creating an assistant message. These
entries are durable, non-user-editable, and never enter model context, including
after resume or compaction. Replay regenerates tool events and retains the
audit wrapper on `MessageEnd`, without inference usage updates. Print JSONL
uses the same message shape.

Tool calls may carry `is_raw: true` to identify verbatim source in their string
`arguments`. Absent or false means JSON arguments, including a JSON string.
Clients preserve this discriminator when storing or forwarding messages.

Completed Code Mode scripts can emit a durable state event:

```json
{"type":"code_mode_store","agent_id":"main","writes":{"key":{"value":42}}}
```

`writes` replaces the named keys on that agent's branch. The event
is emitted after log persistence on the host wire and is reproduced on replay.
Print JSONL also carries it. It is not a model message or rendered chat activity
and does not affect usage. Clients that do not use this state can ignore it.
Committed values survive session resume and compaction. Live JavaScript cells
are not restored. Script errors retain prior writes from that script, while
cancellation before accepted completion discards its pending writes.

Task kinds on `TaskStart` and `caught_up.tasks` include
`{"code_mode": {"cell_id": "..."}}`, alongside `bash` and `agent`.
Task notifications accept `kind: "code_mode"` alongside `"bash"` and
`"agent"`. A Code Mode task represents a yielded JavaScript cell retained
until its final output is collected or it is stopped. `Running` means the
cell is **open**, not necessarily computing: completed-but-uncollected cells
remain open. Its label is `Code Mode cell ID (open until collected)`.
Clients include these cells in the task picker and footer without treating
them as sub-agents. The existing task kill endpoint stops open cells, including
ones awaiting collection. No separate cell endpoint is required.
For Code Mode, `TaskRead.report` contains append-only observed textual output,
not a guarantee of completion. The output endpoint reads those observations
even though a cell has no process spill file. Uncollected output remains in
the evaluator until `exec` or `wait` observes it.

### 5.4 Reliability classes

Every frame is in exactly one class:

- **Durable** event frames correspond to persisted log entries: the
  `MessageEnd` that triggers persistence, the `SubAgentStart` that
  writes the spawn root, the `CompactionEnd` for a checkpoint or recorded
  failure, and the notices or `SubAgentSettings` the projection derives from
  notice-producing state entries. Durable frames carry `seq` (the
  entry's 1-based position, so `0` reads as nothing durable yet)
  and `entry_id`. They are exactly what backfill can regenerate. Seqs
  are strictly monotone per session but **not contiguous**, so clients
  must not do gap detection: continuity comes from the stream being FIFO
  plus explicit `reset`.

  At most one frame per log entry is durable. An entry can project
  several events (a tool-result entry projects a tool bracket around its
  `MessageEnd`, assistant entries project a trailing `UsageUpdate`) and
  only one of them carries the tag, in live flow and backfill alike.
  A committed compaction projects one durable `CompactionEnd` containing
  optional cumulative `TokenUsage` in `usage`. Its envelope's `entry_id`
  identifies both summary and spend, so cursor filtering treats them as one
  event. Backfill regenerates the same event. Legacy log entries without usage
  omit that field and leave spend unknown, not zero.
  A failed compaction projects one durable `CompactionEnd` with `error` and
  without summary or usage. Its identity deduplicates the transcript warning.
  Cancellation writes no entry and its `CompactionEnd` remains transient.
- **Lossy** frames are the three cumulative-snapshot events,
  `MessageUpdate` (keyed by agent id), `ToolExecutionUpdate` (keyed by
  call id), `TaskOutput` (keyed by task id), plus the `list` frame kind
  and the on-change `state` frame (each its own key). A `state` marked
  `opens_block` is part of its block and reliable-transient. They may be
  coalesced or dropped under pressure, the newest one or a later durable
  frame supersedes. New frame kinds declare their class when introduced.
- **Reliable-transient** frames are everything else: tool start/end,
  task start/end, notices, warnings, errors, lifecycle
  brackets, usage updates, queue updates, compaction progress,
  `caught_up`, `reset`, and the `error` frame. Not replayable, and
  losing one wedges or corrupts client state, so these are delivered in
  order or the client is evicted (section 5.9), never silently dropped.

An event type the receiver does not know classifies as reliable, the
safe side: a needless delivery rather than a dropped one-shot.

### 5.5 Attach and catch-up

Per session the host maintains:

- **seq**: section 5.4. The client tracks the last durable seq it has
  **applied**, which the cursor invariant below compares against, and
  the last it has **committed**, which it offers on re-attach. An
  applied seq is committed once a later durable frame or a `caught_up`
  arrives, because an entry can project a trailing untagged event and a
  drop in between would leave the client claiming an entry it only
  partly applied. Offering an older cursor is always safe: idempotent
  application absorbs the extra entry. A `last_seq` observed in a `list`
  frame is never a cursor.
- **epoch**: an opaque token minted fresh every time a session is
  materialized, and replaced whenever the linearized history changes in
  a way that is not a pure append (head switch). Epochs are never
  persisted, so a host restart always invalidates cursors. Within one epoch, "durable events
  after seq N" is well-defined.

Attaching is done through the stream itself, there is no separate
snapshot request. The stream request names sessions to attach, each with
an optional cursor: `GET /v1/events?session=<id>[@<epoch>:<seq>]`
(repeatable). The cursor is split off at the first `@`, so session ids
never contain one, and the cursor splits at its last `:`, so an epoch
may contain a colon. Attaching nothing is legal: that stream carries
`list` frames and heartbeats only. A host also takes `list=none`, which
opens a stream that is never sent a `list` frame, for a consumer that
gets its directory elsewhere (section 6.1). Any other `list` value is
400 `invalid_request`.

A stream request never fails wholesale over one bad session: each named
session either gets its attach block or a session-scoped `error` frame
carrying the failure's own code (section 5.3), and the rest are served.
A `reset` for the session may supersede either at any point, and asks
for a re-attach as always. An error frame ends that attachment, and
except for `host_unreachable` the client drops its epoch and cursor with
it, which costs only a fresh backfill later. For each resolvable session
the server, atomically with respect to that session's event flow,
registers the subscription, projects the durable suffix, and emits in
order on the stream: a `state` frame marked `opens_block`, the backfill
(projected events for entries after the cursor, or the recent tail when
the cursor is absent or its epoch does not match), and `caught_up`. The
recent tail starts at a main-agent message a few hundred entries before
the tip of the selected branch, or at the beginning of a shorter one. It is what a reader sees first, not
the session's whole history. Live frames for that session
follow. Because subscription and projection are atomic and the stream is
FIFO, no durable event can be missed or delivered below the backfill
boundary within a connection. Reliable-transient frames in flight at
attach time can still arrive after `caught_up` and overlap the backfill,
so application must be idempotent (below) on first attach as well as on
re-attach. Lossy frames in flight at attach time are **dropped**, since
a stale snapshot after the durable frame that superseded it would
resurrect transient state. A cursor beyond the session's current
`last_seq` is treated as an epoch mismatch.

The task table and queues on `caught_up` are read after the
subscription is registered, and the host changes either before it
publishes the event announcing the change. So every change is either in
those tables or announced by a `TaskStart`, `TaskEnd` or `QueueUpdate`
that follows `caught_up`. The one change without an event of its own is a
prompt to an idle agent, which is pending until its turn takes it. A
client that attaches in between gets it in `queues`, and the turn's drain
announces the empty queue. Applying such an event over tables that
already reflect it changes nothing: a task event updates a known task in
place, and a `QueueUpdate` replaces its agent's queue.

A client recognizes a block by its opening frame alone and needs no
record of what it asked for: a `state` marked `opens_block` opens one,
the matching `caught_up` ends it, and an unmarked `state` never does.
Because every named session is answered, a client waits for that answer
with no deadline of its own. A block that stops arriving is a stream
failure: a host sends no `heartbeat` on a stream while any block it owes
that stream is incomplete, so a stalled block turns into stream silence
and the ordinary stream-loss recovery re-attaches. A block only waits on
its session's log lock, which appends hold briefly, and on the client
reading it.

Sessions not named in the request produce nothing on the stream except
their rows in `list` frames, so a client is never evicted over traffic
it did not ask for. Changing the attach set means reopening the stream
with new parameters.

Backfill projection rules, which differ from dead-log replay:

- The host projects from the full log so server-side context (tool name
  maps, usage running totals) is complete, and emits only events for
  entries after the cursor.
- A sub-agent run's end is an entry of the log. Its `SubAgentEnd` is
  durable, carries the entry's `seq`, and is served to a cursor below
  it like any other entry. A sub-agent prompted again after that
  brackets nothing: its box stays concluded.
- A run without an end entry is running, was recorded before those
  existed, or was dropped with its parent's cancelled turn. A run its
  process abandoned gets an end entry from the next open, concluded as
  failed with a report that says it was interrupted. If the host knows a
  run without one is **running**, its bracket is left open and the real
  `SubAgentEnd` arrives live later. Otherwise the projection closes it
  once, after the run's last message, with an untagged `SubAgentEnd`
  derived from that message. A run that ended at or below the cursor
  projects no bracket.
- The host tells the projection which sub-agents are running, states
  each one's lifecycle after the backfill, and concludes every other
  sub-agent the projection walked after `caught_up`.
- A sub-agent thread open at the cursor boundary gets its
  `SubAgentStart` re-synthesized so the suffix is well-bracketed. Such
  frames carry no `seq` or `entry_id` (their spawn root is at or below
  the cursor). The client ensures the sub's box exists, reusing it when
  it already does, and reads the run as in progress. Bracket state is
  per run, since background sub-agents interleave with their parent.
- Within a backfill, event order follows the projection (thread
  bracketing), not global seq order. The client treats the backfill
  block atomically: it advances its cursor to `caught_up.last_seq` once,
  not per frame. During live flow the cursor advances per durable frame.

Client application rules:

- **Epoch adoption**: an attach block establishes the session's current
  epoch: the client adopts the epoch of the block's opening `state` frame
  and applies the block under it,
  which is how a stale cursor (head switch, host restart) accepts the
  fresh backfill served under the new epoch.
- **Epoch filter**: outside an attach block the client drops any
  session-scoped frame whose epoch differs from the session's current
  one, which keeps a stale in-flight frame from an abandoned branch out
  of the new branch's transcript.
- **Cursor invariant**: within an epoch, a durable frame whose seq is at
  or below the last applied seq is dropped as a duplicate, at any time.
  This is a de-duplication optimization, not the correctness mechanism:
  it cannot protect an entry's trailing untagged events, since a
  re-served entry whose durable frame is dropped still applies its
  `UsageUpdate` and its tool bracket. Adopting a different epoch resets
  the cursor bookkeeping.
- **Re-attach reconciliation**: when a client re-attaches a session it
  already has state for, the suffix re-projects things it partially saw
  live. Before applying the backfill the client quiesces its transient
  in-flight state for that session. Quiesce clears transient *detail*
  and never durable identity or structure: the unfinalized streaming
  assistant entry goes, a running tool cell keeps its `call_id`, tool
  name and arguments but loses the partial result painted by
  `ToolExecutionUpdate` (a tool that has not finished has no log entry,
  so no backfill can regenerate its arguments), a running sub-agent box
  keeps its status and child transcript but loses its activity detail,
  and the compaction-in-progress indicator is cleared. The host is
  authoritative for concluding sub-agent boxes.

  Application must be idempotent on durable identity, for every
  durable-derived effect: a projected tool start for a `call_id` the
  client already renders updates that cell in place, a re-synthesized
  `SubAgentStart` for a known sub reuses its box and marks that run in
  progress, a re-served transcript row for a known message id updates in
  place, a re-served compaction checkpoint or state notice updates its
  existing row rather than appending a second one. That last pair has no
  identity on the event itself, so the client hands the frame's
  `entry_id` to its reducer. A durable `SubAgentStart` names a spawn
  root, minted once per run, so re-serving one never resurrects a
  conclusion. On `caught_up` the client replaces its task table and
  pending-message queues with the ones the frame carries, because
  neither is replayable. This holds for every session in the attach
  set, focused or not. A client may instead discard the session's state
  and rebuild from a fresh backfill plus the history pages before it,
  which must produce the same transcript.
- On `reset`, the client re-attaches, offering its cursor. The server
  serves an incremental suffix if the epoch still matches, or a fresh
  backfill of the recent tail if not. A `reset` received mid-attach-block abandons the
  block: the cursor only advances at `caught_up`, which never came, so
  discarding the partial application is safe. A re-attach that fails
  because the session no longer exists fails as a session-scoped `error`
  frame, never as the stream.
- **Re-asking after a refusal.** An attach refusal ends the attempt, never
  starting a timer or a retry loop. Recovery depends on its code:
  - `locked`: keep the session selected and its cached transcript available,
    show that another process holds it, and offer explicit retry by selecting
    it again, including from the sidebar or session selector. Directory updates
    and `reset` frames do not retry it. Omit it from subsequent stream opens
    until the user selects it, so reconnecting or following another session
    cannot silently acquire it in the background.
  - `unknown_session`, and any code the client does not know: re-ask when the
    session's row is absent from one folded `list` and present in the next.
    These transitions are set-wide and include the first list after a refusal
    if the client held no rows. Explicit selection also retries.
  - `persistence_failed`: for a session the client was following, re-ask
    once, immediately, when the frame folds. The host treats the failed
    materialization as absent and rebuilds from disk, so the user sees what
    was saved. A re-ask answered with `persistence_failed` again settles
    like any refusal and waits for the user to select the session.
  - `host_unreachable`: keep the epoch and the cursor, and the cached
    transcript, and show that the host is unreachable and the session
    resumes when it returns. Re-ask on the gateway's `reset` for the
    session, so the host's return costs an incremental catch-up. Explicit
    selection also retries.

### Goals

Hosts advertise `goals`. Clients attempt the command and show an unsupported
notice if the host lacks the endpoint, including through a gateway.
`POST /v1/sessions/{id}/goal` accepts `{ "action": { "op": "get" } }`.
Every action returns HTTP 200 `{ "goal": <snapshot or null> }`.
Actions are `get`, `create` (`objective`, optional `token_budget`), `edit`
(`objective`), `set_budget` (`token_budget`), `replace` (`objective`, optional
`token_budget`), `pause`, `resume`, `clear`, `complete`, and `block`. Unknown
fields are rejected. Goals belong only to Main.

Requests may include `expected_goal_id` beside `action`. The host refuses a
mismatched or missing current goal with 409 `conflict` before applying the
action. Clients use this field for actions on a displayed goal, so delayed
edits cannot change a replacement. `replace` requires `expected_goal_id` and
validates the new objective and budget before replacing anything. It creates
a new identity with zero usage in one operation. `create` cannot replace an
unfinished goal. Model tools cannot replace, edit, change budgets, resume or
clear goals.

Every `state` frame carries the current branch-local `goal`, or null when
cleared. An absent field decodes as no goal for compatibility. A snapshot has
`id`, `objective`, `status`, `token_budget`, `tokens_used`, and
`time_used_seconds`. Status is `active`, `paused`, `blocked`,
`budget_limited`, `usage_limited`, or `complete`.

Goal state and usage persist on the user branch. A change of the goal's
objective, budget or status is committed before it takes effect: before a
`state` frame shows it, before the model is told and before pursuit follows
it. A goal command whose commit is refused is refused, and the goal stays
as it was. A status the host itself decides on, after a turn that failed or
a spent budget for example, is committed the same way. If that commit is
refused, clients get one error, the goal stays as it was except that the
host starts no further goal turn, and the host tries again. The host
records usage with each of these records and when a Main turn ends, so a
crash or a shutdown mid-turn loses at most that turn's accounting. `state`
frames carry current usage between those records. Reopening a session or
switching heads does not automatically pursue a restored goal. Explicit
resume is required. Cancelling Main pauses pursuit. Reconnecting to a still
live session does not pause it. Goal commands do not cancel a turn or its tools.
Each idle-driven goal turn records internal context with a display-only `notice`
field. The transcript renders it as a dim "Continuing goal." row, live and on
replay, rather than as editable user input. Ordinary goal-state context stays
hidden. The notice is not included in model input.
The hosted Main agent can call `wait` alone to yield. In a batch with other
tool calls, all results are returned to the next inference normally. The
mixed-batch wait request is discarded, not deferred to a later step.
A standalone wait ends the turn with a transient `agent_end` carrying
`waiting: true` (an absent field means false). The agent is idle and the goal
remains active, but no synthetic goal turn starts until new input arrives.
Queued input and task notifications, including ones already queued at
yield, retain their ordinary wake paths. A new turn clears the wait. Explicit user
goal creation, replacement, objective editing or resume also releases it.
Running background processes alone do not suppress goal continuation. The wait
is not persisted, and the tool is unavailable to sub-agents and print mode.
Explicit pause, block and completion close goal accounting, whether requested
by the user or the model. Later reporting and descendant results are outside
that subtotal. Clearing a goal also stops its accounting and continuations.
Creation, replacement and resume are accepted during work. They admit subsequent
Main inference into the goal without charging inference already in flight.
Descendants belong to the goal that admitted their work, not a replacement.
`edit` steers a changed active objective into the current turn at its next
inference opportunity. Editing preserves identity,
budget and usage. Active, paused, blocked and usage-limited states are preserved.
Editing a complete or budget-limited goal requests activation, but an exhausted
budget keeps it budget-limited. Tool mutations from an inference preceding a
user goal change are refused until the model has sampled current goal context.

`set_budget` accepts a positive integer, or null/omission for unlimited. It
preserves identity, objective and usage, and does not reactivate stopped work.
Lowering an active goal's budget to or below its usage makes it budget-limited.
An exhausted goal can resume after the user increases or removes its budget.
The token budget is soft: usage includes
uncached input, output, cache writes, and descendant agents, but not cache
reads. A running turn can exceed the budget. No time budget is supported.
Three goal turns with `bash` tool errors and no successful Main tool calls stop
pursuit as blocked. Any successful Main tool call resets that audit, including
`get_goal`. Text-only turns do not. A normally completed command with a nonzero
exit code is not a tool error. Resuming or changing the objective starts a fresh
audit.
Print mode refuses goal tools without a session host.
Disabling `update_goal` prevents starting or resuming pursuit, since the model
must be able to conclude the objective. Active pursuit pauses before its next
turn if that tool becomes disabled. Usage is checkpointed and best-effort: it
counts what providers reported, and usage an interrupted turn never checkpointed
is not recovered.

The command palette's Goal entry opens a management window over the same live
session state as the footer. Actions use Control against the session captured
at open, never host files. The window has no separate fetched goal snapshot.
Creation has a multiline objective, an optional positive token budget (blank
means unlimited), and an explicit Start goal action. Existing goals offer
objective and budget editing, Pause, Resume, Clear, and a new-goal flow.
Replacing unfinished work asks for confirmation. Submitting an objective edit
that resumes completed work says so. Exhausted goals offer budget adjustment
rather than a Resume action that must fail. Streamed updates keep status and
usage current without replacing editor drafts. Failed requests retain open
drafts for correction or retry, and request errors remain visible after the
window closes. Successful actions clear the action filter. Management shows a
wrapped objective preview. The objective editor shows the full text, and Escape
closes it without making changes.
Entering an idle saved session with a resumable paused, blocked
or usage-limited goal offers Resume or leave stopped, without prompting again
on live pauses or reconnects.
The zero-argument `goal` palette action opens the goal window. The footer reads
live snapshots and shows one compact goal phrase per status. Active pursuit
shows budget usage, or elapsed goal time without a budget. Paused, stalled
(blocked) and usage-limited goals show the control hint. A budget-limited goal
shows its usage against the budget. A complete goal shows tokens used when it had a
budget, otherwise elapsed goal time.
Elapsed goal time includes waiting while the goal is active. It advances
locally between snapshots even while Main is idle or another agent is viewed.
Stopped goals and unsettled connections show the host's recorded time. This
display clock never changes persisted usage.
The goal is right-aligned and reserves its width, so narrow terminals truncate
the usual fields before it. Only a goal wider than the whole footer is omitted.

### 5.6 Commands

Commands are JSON POSTs. Effects are observable on the stream or the
corresponding read. Mutations against a specific session return 202 on
acceptance. An optional JSON body `{incomplete: <message>}` reports a change
that applied but whose requested save or log entry failed. An empty body or
absent `incomplete` means complete acceptance. The caller can report partial
success without correlating an event-stream warning with its request.
`POST /v1/sessions`
returns 200 with the new session's id. Commands that act on "the viewed
agent" locally take an optional `agent` field (default: the main agent).

| Command | Body | Semantics |
|---|---|---|
| `POST /v1/sessions` | `{host?, settings?, prompt?, tag?, env?}` | Create a session in the host's working directory. `settings` and the initial string-to-string `env` overlay per section 7. `prompt` is `{text}` or `{content: [...]}`. Settings, prompt, tag and env are validated before a log exists, so a refusal leaves nothing behind. The tag is part of the creation and is stored with the session's first input. The first prompt is applied afterwards under the session's own lock: one the session refuses still answers 200 `{id}` plus an `incomplete` string carrying the host's words for what did not stick, so the client prompts again rather than creating a second session. A first prompt whose save fails is one the session refuses: nothing is stored and the session is still there to prompt. A minted session is never deleted to make an error tidier. |
| `.../{id}/prompt` | `{text}` or `{content}`, optional `agent` | The prompt is saved as the agent's pending input before the answer, and a refused save refuses the command. An idle agent then starts a turn that takes it, with any image content. Behind a running turn it waits as a follow-up, and image content is 409 `conflict` there. A prompt to an idle agent that already has a pending message joins that message. A sub-agent is named by any client of any host process that has the session. One the selected branch shows no run of is 409 `conflict`, here and for `steer` and `settings`. |
| `.../{id}/steer` | `{text, agent?}` | Queue steering, or promote the pending follow-up when `text` is empty. |
| `.../{id}/cancel` | `{agent?}` | Interrupt the targeted agent's current turn. A foreground child stays assigned while its parent delegation remains pending. Prompts, queued follow-ups, and task notifications can start its next turn, just as for the main agent. Its final report completes the original call. A detached background child is stopped through its task, just like task kill. Cancelling a parent ends its foreground assignments, but never stops detached background work. A running mark with no owning turn, assignment, or task is 409 `conflict`. An idle or completed target is accepted. |
| `.../{id}/kill-agent` | `{agent}` | Explicitly terminate a sub-agent assignment, or a running independent continuation. The main agent is not a kill target. |
| `.../{id}/queue` | `{op: "remove", agent?}` or `{op: "clear"}` | Withdraw one agent's pending message, or clear the session's queues. A withdrawal answers 200 `{text?}` with the text it took, which is what makes the dequeue-into-the-editor gesture work. A pending message with image content, which a restart or a failed turn can leave, is not withdrawn: 409 `conflict`. A clear discards it. One agent holds at most one coalesced pending message, so there is no index. A clear answers 202. |
| `.../{id}/compact` | `{instructions?}` | Manual compaction. |
| `.../{id}/continue` | `{}` | Resume the main agent's interrupted turn without a new user message. A turn is interrupted when the selected branch does not end on a completed assistant message, as after a crash, a restart, a cancellation or a provider error. The model is told the turn was interrupted and that a tool call whose result was not saved may or may not have run. It also resumes each sub-agent whose own turn, one after its run, was cut short by a host that ended, the same way. A sub-agent run that was cut short is not resumed: it was closed at the open, and its parent told ([sub-agent steering](subagent-steering-spec.md), section 4). With no interrupted turn, it delivers the main agent's pending queued message and task notifications instead. 409 `conflict` while the main agent's turn is running and nothing was resumed, or when the last turn finished and nothing is pending. |
| `.../{id}/settings` | exactly one of `model`, `thinking`, `thinking_display`, `speed`, `verbosity`, `oracle_model`, `oracle_thinking`, `oracle_verbosity`, optional `agent`, `persist` | Host applies, logs, and publishes the synthesized frames. Naming zero or two axes is 400. `account` is rejected, including null, and must use the account route. `persist` defaults to `none`, or is `user`, `project_set`, or `project_clear`, and also writes the value into that host config layer. Oracle axes reject sub-agent targets. Persistence is main-agent only. |
| `.../{id}/account` | `{provider, account?}` | Select an account for this session and provider from the host-local auth store. Missing or null `account` resets to Provider default, `""` pins the unnamed account, any other string pins that exact label. Does not change the provider's auth-store default. Read the result through `accounts`. |
| `.../{id}/env` | `{key, value}` | Set one session environment value, including the empty string. Null or an absent `value` removes the key from the map, not from the inherited process environment. Idle-only: 409 `conflict` while a turn or background task is live. Validates before mutation and persists the full resulting active-branch map. |
| `.../{id}/tag` | `{tag}`, empty or absent clears | Set the session's tag (section 5.8): one trimmed line, length-capped. Materializes like any command so the session lock covers the metadata write. |
| `.../{id}/archive` | `{archived: bool}`, absent reads false | Set or clear the archived bit (section 5.8). Materializes like any command, so a rival's lock refuses it. Nothing else refuses it: a session working through a turn takes it and goes on working. |
| `.../{id}/head` | `{entry, changes?}` or `{before: <entry_id>, changes?}` | Switch the session head. 409 `conflict` unless the session is quiesced (section 4). New epoch, `reset` frame. `before` resolves the named entry to its parent server-side, atomically with the switch. An unknown entry is 404 `unknown_entry`, an entry with no parent is refused. Exactly one target. Optional `changes` applies session-scoped overrides to its inherited baseline (see below). |
| `.../{id}/tasks/{task_id}/kill` | `{}` (absent or blank is equivalent) | Kill a background task. Any field or non-object value is refused before the task is touched. |

`agent_start` marks a running turn and `agent_end` marks it idle. `round_start`
and `round_end` bracket each round inside it. A round that aborts or fails has
no `round_end`. The `assignment_pending` field of `agent_start` and `agent_end` is an optional boolean, defaulting to false,
that says the foreground caller still owns the assignment. It does not change
turn activity or inhibit ordinary wakes. `sub_agent_end` carries the final
assignment conclusion. Assigned children stay in the active agents picker
between turns, without a separate interrupted state. Attach repairs ownership
and current turn activity from the host's live state.
Ctrl+C interrupts the viewed child only. Ctrl+K in the picker explicitly kills
a running or assigned sub-agent, including a foreground assignment. The
main row is not killable, and background bash task kills use the task endpoint.

Head `changes` is a closed object with optional `settings`, `accounts`, and
`env` objects. Settings may override model, thinking, speed, and verbosity
together. The host rejects `settings.account` (the accounts map owns pins) and
`thinking_display` (live-only). Accounts maps provider names to exact labels,
with null removing a pin and `""` pinning the unnamed account. Env maps keys to
values, with null removing a session overlay key and `""` retaining an empty
value. Omitted fields inherit the target baseline. The host validates and
prepares the changes before switching. These changes never update config files
or auth defaults.

Branch preparation is client-owned. The editor combines the selected message's
recorded `branch_settings` with explicit choices, leaving unrecorded axes
unspecified rather than borrowing the live branch's values. No host request is
needed to arm a branch or stage session-only inference settings. Saving a
staged axis into the host's config is an ordinary config edit (section 5.6).
The environment editor reads the selected branch point only when opened
(section 5.7). Submission resolves
inheritance and validates the requested combination on the host.

Empty `changes` is omitted on the wire so ordinary head requests remain
compatible.
Nonempty changes must travel to the server: an older server's closed head schema
rejects them rather than silently switching without the requested overrides.

The create body's `host` field names an enrolled host in the vocabulary
the directory rows' `host` field uses (section 5.8). On a gateway it is
required unless exactly one host is enrolled: ambiguity is 409
`ambiguous_host`, no host is 409 `no_host_enrolled`. On a plain host it
is absent or the host's own id. The response names the session in the
answering server's id vocabulary, namespaced through a gateway.

Request bodies are the `aj-wire` types, which are the source of truth.
Their JSON is flat: a prompt's body is `{text: ...}` with `agent` beside
it, never `{input: {text: ...}}`. A model change travels as the `{api,
url?, name}` triple that CLI and env selection use, never as a catalog
object, and the host resolves it against its own catalog and
credentials. Thinking display is a live-only axis: broadcast via
`state`, not written to the log, so a resumed session reseeds it from
the host's config.

Errors cross the wire as a small envelope: `{code, message, ...fields}`.
`message` is a human sentence sufficient on its own, and an unknown
`code` renders as its `message` verbatim, so codes are additive. An
error that references a session carries it in a top-level `session`
field, so a gateway rewrites error bodies without understanding the
code.

#### Host config and skills

Two things can hold a setting: the running session, which the settings
command changes, and the host's config files, which seed every new session.
`GET` and `POST /v1/sessions/{id}/config` read and edit the second, plus
`persist` on the settings command for the common "apply and save" gesture. The session id routes to the owning host
and must name a session in its store. Reading does not materialize it.

A read returns `HostConfig`: the `user` layer and the `effective` merge as
schema-keyed string maps in the editor's vocabulary (`<unset>` for absent
optionals, comma-separated name lists), `project_keys`, `has_project`, and
the host's `models`, `tools`, and `skills` catalogs. Model choices include only
providers with configured host credentials (stored credentials, environment
keys, or runtime API-key overrides). Custom models from the host's
`~/.aj/models-custom.json` are included when no authentication is required, or when
their configured `custom_auth.api_key_env` variable is nonempty. Credentials are not validated or refreshed
for this read. No credential values or arbitrary config-file keys are returned.
Connections are trusted, so complete model and catalog URLs round-trip without masking. The
standalone model and thinking selectors read the same model catalog through
`GET /v1/sessions/{id}/models`, a model array. This read
does not discover skills or read config layers. Selectors show a notice if
it fails, never the client's catalog.

A write is one `ConfigEdit`, `{key, value, persist}`, into the layer
`persist` names: `user` or `project_set` with a string value, `project_clear`
with none. Unknown keys, malformed values and presentation keys are refused
before anything changes, as is a project edit on a host without a
repository. The inference axes (`model` as `provider/id`, `thinking`,
`thinking_display`, `speed`, `verbosity`) are accepted in the editor's
vocabulary and land exactly as a persisted settings command would land them,
without touching any session. That is how an editor saves an axis while a
branch draft is open: the config is written at once and the session effect
waits for the branch. A save succeeds or fails as a whole and the host's
effective config only ever reflects what reached disk, so a same-value retry
after a failed save writes again.

`GET /v1/sessions/{id}/skills` returns discovered `{name, description,
path, enabled, disable_model_invocation}` metadata, and `POST
/v1/sessions/{id}/skills` takes `{name, disable}`.
Discovery uses the host's working directory and user skill roots. A toggle
needs a discovered name and edits `disabled_skills` in the host's user config
for new sessions. Running sessions keep their prompt's skill listing.

The settings and skills editors capture the session and host they opened on
and stay responsive while host reads run in the background. Settings starts
with editable client presentation rows and a host-loading notice. Host rows
arrive below them without replacing client edits or moving the selection.
Skills starts with a loading placeholder. A reply fills only the window that
asked for it. Saves also run off the input loop, with one outstanding edit per
window. The chosen value stays visible and the user can navigate or close
while saving. A definite validation or endpoint refusal restores the previous
value and leaves the window editable. An unconfirmed save pauses further edits
and asks the user to reopen the window to refresh. A save that failed after
the session applied the change is reported in the notice.
Branch choices are staged immediately, independently of saving their defaults.
An unsupported endpoint shows a notice and never falls back to reading or
writing the client's config.

Theme, transcript rendering, image rendering, frame statistics, sidebar width,
and keybindings belong to the client. Their settings remain available when
host discovery fails. The editor reads and writes them in the client's own
layers and applies their live rendering effect locally. Over a connection,
host-side rows carry `*`, with a footer naming the opening host and explaining
that unmarked settings belong to the client. Local runs need no ownership
marker. A project edit without a repository on its owning side reports that
side's limitation. Neither side's values fall back to the other's.

### 5.7 Reads

- `GET /v1/sessions`: the same payload as a `list` frame, `{sessions,
  hosts?}`. Includes every session of the host's working directory,
  on-disk as well as live, with a liveness flag. Attaching or commanding
  a non-live session materializes it (lock permitting). A read never
  does, except for the tree, environment, account, and session-info reads, which parse the log
  and materialize like a command. This is the discovery surface, there is
  no separate on-disk listing.
- `GET /v1/sessions/{id}/tasks/{task_id}/output?offset=N`: `TaskOutput` in
  `aj-wire`, `{id, status, offset, total_bytes, bytes}`. Reads the retained
  task's full interleaved spill output, or a Code Mode cell's append-only
  observed textual output, at the required unsigned byte offset.
  `bytes` is a JSON byte array preserving invalid UTF-8 and split code points,
  capped at `TASK_OUTPUT_CHUNK_BYTES` (65536). `total_bytes` is the output length
  captured before reading, and a read never crosses that captured length.
  Advance by `bytes.length` to continue. At that length the result is empty,
  and a running task may append afterward. Status is sampled before the file
  read, so it can lag completion. An offset beyond the captured length is
  400 `invalid_request`. Unknown tasks, including cold-session tasks, are
  404 `unknown_task`. Missing or unreadable spill output is 409 `unsupported`
  with a clear explanation, never a tail substituted for full output.
  Code Mode observations are read from their retained in-memory snapshot.
  File paths come only from the session task registry, not the request.
  Reads use bounded allocation and blocking-pool file I/O. Running and completed
  tasks are readable while retained by the live registry. A running `bash`
  task is recorded and taken up again by the next open of the session. A
  finished task is not kept across a release or restart.
- `GET /v1/sessions/{id}/tree`: `{segments, head?}`, the
  segment-collapsed branch tree for the tree view and head switching.
  `head` is the current head entry id, absent only while the log has no
  head, and not derivable from the segments.
- `GET /v1/sessions/{id}/info`: the session's aggregate log facts, encoded as
  `SessionInfo` in `aj-wire`: identity, the project it is stored under, the
  host-local path of the database that stores it, timestamps, the summed size of the session's
  stored entries, message and tool counts, usage and its provider/model/account/served-speed
  breakdown, compaction usage, recorded settings, and session environment.
  Counts and usage span all threads and branches. Settings and environment
  reflect the active user branch. Environment values are unredacted, with
  `null` meaning unrecorded and `{}` meaning recorded empty. The response's
  `session_id` is the host-local log identity, not a gateway routing id.
  Clients render the facts and fetch only when the info overlay opens.
  Response usage carries optional `served_speed`, canonicalized from server
  reports rather than requested settings. A missing report is unknown. Usage
  buckets carry their homogeneous reported mode in `usage.served_speed`, while
  mixed or unknown totals carry none. Clients label missing modes `unknown` and
  keep the requested settings distinct from the reported usage.
- `GET /v1/sessions/{id}/history?before=N&limit=M[&agent=K]`: `HistoryPage`
  in `aj-wire`, `{epoch, agent, start, end, frames}`. One page of one
  agent's history before log position `before`: the main agent's, or
  sub-agent `K`'s. History is paged per agent because each transcript is
  read on its own. The frames are the `event` frames a backfill would have
  carried for that agent for entries `start..end`, where `end` is `before`
  clamped to the log and `start` is one of that agent's messages at least
  `limit` entries earlier, or `1` when it has none. The main agent's pages
  count the entries the selected branch shows, a sub-agent's count its own. The host caps `limit`
  at 4000. A sub-agent's box (its start and conclusion) belongs to its
  parent's page, and what the sub-agent did belongs to its own. A run that
  continues past `end`, or that the host knows to be running or assigned,
  is left open, as in a backfill.

  A fresh backfill covers every agent from `history_start`, so that is
  where each agent's history starts until a page moves it. The client
  folds a page on its own and puts the resulting entries in front of that
  agent's transcript, then asks again with `before = start` until `start`
  is `1`. It drops a page whose `epoch` is not the one it is attached under
  or whose `end` is not that agent's current start, so a late or repeated
  answer is harmless. A page changes no cursor, usage total, task table or
  lifecycle state. Reading and projecting run on the blocking pool.
- `GET /v1/sessions/{id}/export`: `SessionExport` in `aj-wire`, `{html}`.
  The host renders the complete log with the ordinary HTML exporter, including
  all threads and branches and its export redaction and tool-detail resolution.
  Rendering runs on the blocking pool, not the serving loop. The endpoint writes
  no host export file. Clients save the returned document to
  `~/.aj/exports/aj-session-<id>.html` on the client, using the requested session
  id (including a gateway prefix), and report that local path in the notice.
  Clients attempt the endpoint and explain when the host does not support it.
- `GET /v1/sessions/{id}/usage`: host provider-account plan usage, not the
  session's token totals. Returns `ProviderUsageReport` in `aj-wire`, with
  `statuses` sorted by provider and exact account label, and `reset_providers`
  naming providers with a configured reset adapter. Each status contains
  `provider_id`, `provider_name`, nullable `account`, `is_default`, and `outcome`: `Usage`
  (windows, details, notes, optional reset credits), `Unsupported` (reason),
  `NotConfigured`, `NoSource`, or `Error` (message). Enums use serde's externally
  tagged representation.
  `details` is an ordered array of `{label, value}` string pairs for labeled
  facts such as credit balances. This additive field defaults to an empty array
  when absent. Labels and values render independently, without parsing display
  strings. `notes` remains freeform text.
  `provider_name` is the host display label for the credential kind: the
  registered OAuth name for subscriptions, or the friendly API-provider name
  for API keys. Unknown providers fall back to their ID. This additive field
  defaults to an empty string when absent. Clients display `provider_id` when
  it is empty, without inferring a name from human-readable status text.
  Naming alone never refreshes a token.
  `is_default` marks the exact stored account that was the provider default
  when accounts were discovered. It defaults to false when absent and is false
  for provider-level sources, including runtime overrides. Clients show it as
  `provider default`, not as a session's explicit account selection.
  A runtime credential override collapses its provider to one unlabeled row.
  The host reads credentials and performs any OAuth refresh and writeback.
- `POST /v1/sessions/{id}/usage/reset`: `{target, idempotency_key}`. The target
  is copied unchanged from a report's `reset_credits.target`, containing
  `provider_id`, nullable exact `account`, and `upstream_account_id`. It is an
  identity claim, not a bearer credential or authorization. The host resolves
  its own credentials and the provider revalidates the upstream identity before
  spending. Account selection changes cannot retarget an offer. An unknown or
  replaced account refuses as stale rather than falling back to a default.
  The nonblank idempotency key identifies one confirmed attempt and is reused
  unchanged on retries, including after an ambiguous transport failure.
  A provider response is `{"Ok":"Reset"}`, `{"Ok":"AlreadyRedeemed"}`,
  `{"Ok":"NothingToReset"}`, or `{"Ok":"NoCredit"}`. Provider failures are
  `{"Err":"StaleTarget"}` (refresh required) or `{"Err":{"Error":"message"}}`
  (retryable). These are HTTP 200 results. Session and transport refusals use
  the ordinary error envelope.
  Both endpoints validate the session before touching host credentials and do
  not materialize it. The session address selects exactly one host through the
  gateway's wildcard route, with no gateway-wide aggregation. No provider keys,
  bearer tokens, or credential contents travel in either request or report.
- `GET /v1/previews?session=<id>` (repeatable): the session browser's
  `SessionPreviews` in `aj-wire`, `{previews, incomplete}`. Each preview
  carries the full first nonempty main-thread user text block in
  `first_user_message`, falling back to the first nonempty goal objective when
  no such message exists.
  Internal context and delegated task prompts are not user input. This fallback
  does not count as a message.
  Previews also carry message count, creation and
  last-message timestamps, file modification time and size, tag, and archive
  bit, describing the whole log rather than the active branch. The read uses
  the ordinary preview scanner without materializing any session. An id the
  host does not have, cannot read, or could never hold is left out rather than
  refusing the batch. A gateway splits a batch by owning host, reads each host
  once, returns previews under the ids the client asked with, and names a host
  it could not read once in `incomplete` `[{host, message}]`, keeping the other
  hosts' rows. Owning hosts are read concurrently within one batch deadline,
  so stalled hosts do not delay healthy reads behind timeout waves.
- `GET /v1/sessions/{id}/prompt-history/stream`: submitted prompts from the
  focused session's workspace on its owning host. A gateway forwards this read
  to that host, not to the client's workspace.
- `GET /v1/prompt-history/stream`: submitted prompts from every workspace in the
  host's sessions store. A gateway reads adopted, connected hosts concurrently
  and merges their replies. Other enrolled hosts are named partial failures
  without receiving a history request. Both history endpoints produce
  `PromptHistory` values in `aj-wire`:
  `{prompts: [{text, project, timestamp}], incomplete: [{host, message}]}`.
  Text is the full trimmed prompt, joining user text blocks with newlines.
  Only top-level main-thread messages contribute, across all branches and
  archived sessions. Assistant, tool-result, sub-agent, and task-notification
  content is excluded. Corrupt and non-UTF-8 lines and unreadable files are
  skipped independently, as in the ordinary prompt extractor.
  `project` is the sessions-store workspace directory label in All scope and
  null in Workspace scope. `timestamp` is the persisted entry's UTC timestamp,
  falling back to a positive user-message millisecond timestamp, session-id
  creation time, file modification time, then the Unix epoch. Results are
  newest first; prompts sharing a timestamp keep their log order, later lines
  first. Exact equality of trimmed text deduplicates to the newest occurrence,
  including its workspace label. The limit is 2000 distinct prompts globally,
  applied after ranking and deduplication, not by truncating a directory walk.
  A gateway keeps healthy results when another host fails or lacks the endpoint
  and names failures in `incomplete`. Reads do not materialize sessions or alter
  their state.

  Both are finite SSE reads. Named `snapshot` events carry complete
  replacement `PromptHistory` values, coalesced as files finish scanning:
  the first file's at once, later ones at most about ten times a second.
  A terminal `complete` event carries the final value, even when empty or
  unchanged. A terminal `error` event carries `{code, message}` and leaves
  earlier results available. EOF without either terminal event is a failed
  read. There is no reconnect or replay. Dropping the body cancels the read.
  Gateways forward Workspace bodies without buffering. All-scope reads merge
  each host's latest snapshot as it arrives, ordered by host label and identity
  to break equal-time ties independently of arrival order. Slow hosts do not
  block healthy results. Failed or timed-out hosts retain their last available
  prompts and contribute a named failure.
- `GET /v1/sessions/{id}/env`: a JSON object mapping strings to strings,
  the full environment map selected by the active branch. Values are
  unredacted on the trusted control port. Export-only redaction does not
  apply. This reads the session overlay, not the host process environment.
- `GET /v1/sessions/{id}/env/before/{entry}`: the same map at the named entry's
  parent, with the same target validation as `head.before`. An unknown entry is
  404 `unknown_entry`, and a parentless entry or invalid parent head is refused.
  This read can materialize a session and is available while work is live. It
  changes neither the head nor runtime state. The entry is one URL-encoded path
  segment. A host without this resource returns `unknown_endpoint`, rather than
  ignoring a target query and returning the live map.
- `GET /v1/sessions/{id}/accounts?provider=...`: `{provider, selected,
  default, accounts, override_active, source}` from the host-local auth store.
  `provider` is URL-encoded by clients. Omitting it selects the session's
  current main-model provider, returned in `provider`. `selected` is the session
  pin, or null for Provider default. `default` is the provider's default
  account label, or null when none exists. `accounts` contains exact labels,
  including `""` for the unnamed account. `override_active` reports whether a
  credential override is active, and `source` describes the credential source
  without exposing a secret. No keys, tokens, or credential contents travel.

### 5.8 Status model

Per-session row fields in `list` frames and `GET /v1/sessions`:

- `id`: the session id (section 5.2).
- `live`: materialized in the host, vs on-disk only.
- `working`: any agent turn, compaction, or background task is running in
  the session. A main agent yielding while its background work continues
  leaves this true. Unlike `state.working` (section 5.3), this is session-wide
  activity, not Main's turn state. It does not mean user input is required
  when false.
- `last_activity`: host-clock timestamp on every row. For a live row it
  is the last durable event the host observed, and a release hands that
  stamp to the cold row. Where the host has no answer, the time the
  session's stored row was last updated stands in, or the file's
  modification time for a JSONL session not yet imported. A restarted
  host can therefore disagree with its predecessor.
- `last_seq`: only on live rows. A cold row never carries one. A
  list-observed position is never a cursor (section 5.5).
- `tag`: optional user-set label, display metadata, never an id.
  Session-scoped and independent of branch history. SQLite stores it on the
  session's row. A session without an entry yet has no row: its label and
  archived bit live with the host that holds it and are stored with its first
  entry. A JSONL session not yet imported uses a sidecar
  (`meta/<session id>.tag`, one line of UTF-8).
  A head switch cannot move the label.
- `archived`: user-set bit, display metadata with no lifecycle meaning:
  an archived session keeps its log, its lock, and any turn it is
  running. Stored on the session's SQLite row, or for a JSONL session not yet
  imported as a sidecar (`meta/<session id>.archived`) whose existence is the
  bit. Absent reads false.
  Changed only by the archive command, deliberately with no create-time field.
- `locked`: the session's advisory lock (section 4) is held by a writer
  other than the publishing host. A session live in this host is never
  `locked` on its own rows: the bit names a rival. A hint, never a gate:
  the lock is the only authority and the bit may lag it in either
  direction, so a client acts by attempting and reading the answer.
  Absent reads false. A gateway relays it untouched.
- `host`: which enrolled host the row belongs to, filled by a gateway
  and absent from a plain host's rows. Clients group by it and must not
  derive it from the id.
- `unreachable`: gateway only, the owning host's connection is down.
  Absent reads false.

There is deliberately no "needs attention" bit on the server. A client
derives it from seqs, not stamps: a session is unseen when the last
durable seq the client has evidence of (applied frames while attached,
`last_seq` on live rows while not) exceeds the seq the user had viewed.
Unseen **latches** client-side until the user views the session, so the
row going cold (losing `last_seq`) does not clear it. A session whose
entire unseen window fell inside a disconnect and went cold before
reconnect shows no glyph until opened, and a never-viewed session reads
as having nothing unseen.

`list` frames are lossy (section 5.4). A host queues an initial directory
for a new subscriber on attach, without waiting for the refresh tick.
Ongoing updates are debounced on a one-second
tick, so `last_seq` churn never produces a frame per event. A host
recomposes the directory only when something a row shows may have
changed, which transient events such as streaming deltas never do. A
row, and with it a sidebar's working flag and unseen glyph, may trail
the host by up to a second, while an attached session's `state` frames
travel at once. A snapshot a subscriber has
already accepted (queued or delivered) is not sent to it again.

### 5.9 Flow control

The agent must never block on a slow client, and reliable frames must
never be silently dropped for a connected client.

- Per attached client, the server keeps a bounded outbound queue. The
  bound governs **live fan-out only**: an attach block (`state`,
  backfill, `caught_up`) is producer-paced, written at the pace the
  client reads under ordinary HTTP backpressure, and never preloaded
  into the bounded queue, so a big resumed session cannot evict its own
  client before the first frame is read. Live frames arriving during a
  backfill queue under the bound as usual, and overflow there evicts.
- Lossy frames are coalesced by their key: a newer snapshot for the same
  key drops the queued older frame and enqueues the new one at the tail,
  never by in-place substitution, which would reorder content across a
  queued durable boundary. A lossy frame that meets a full queue with no
  older frame to replace is dropped.
- A host's HTTP stream paces lossy frames: after writing one it may wait
  about 50 ms before reading again, and same-key snapshots arriving
  meanwhile coalesce in the queue as above. A fast client therefore
  receives a streaming reply as a handful of snapshots rather than one
  per provider delta. Queued reliable frames suppress further pacing so
  their preceding snapshots drain in FIFO order.
  A reliable frame arriving during a wait pays at most that one pacing
  window, not one per snapshot key ahead of it. Closed queues drain without
  starting another wait. Attach blocks hold no lossy frames
  and are unaffected. A gateway's downstream inherits the pacing of its
  upstream streams.
- Durable and reliable-transient frames are never dropped. If a client's
  bounded queue overflows with them, the server disconnects that client.
  Recovery is the ordinary re-attach with cursor plus reconciliation
  (section 5.5), which is what makes eviction safe.

### 5.10 Compatibility

Both ends of every connection are aj, but versions skew. Rules:

- `GET /v1/hello` carries `protocol`, and the current protocol is 4.
  Peers check it for exact equality: a client sends no create or command
  after a mismatched hello, and a gateway opens no link to a host whose
  checked hello failed. Mixed generations are unavailable rather than
  degraded. Hosts and gateways roll before clients.
- Every protocol-defined JSON command has a closed schema. The component
  that owns the command's effect rejects unknown fields recursively
  (nested settings, model selections, prompt content included) before
  any effect, as 400 `invalid_request` with a generic message that names
  no offending field. Clients do not retry after stripping a field. A
  command with no fields has an empty-object schema: an absent body,
  ASCII whitespace, and `{}` are equivalent, and any field or non-object
  value is malformed. Read and event-attachment queries keep their own
  rules.
- New command fields roll receiver first. Once every receiver knows an
  optional field, clients may send it. A stale receiver refuses it
  instead of executing a smaller command.
- Observation decoding is additive. Clients ignore unknown fields in
  hello, responses, error envelopes, reads, directory rows, frames, and
  known events. A malformed known type is an error rather than a
  downgrade to unknown, while an unknown type retains its tag and raw
  JSON. An unknown nested event is skipped before the reducer, but its
  envelope still applies: the epoch filter runs and a durable unknown
  event advances the cursor. Unknown top-level frame kinds may be
  discarded by endpoint clients outright.
- A gateway forwards unknowns, it does not filter. Retained raw JSON is
  re-emitted structurally unchanged (key order is not significant)
  except for the session-id rewrite, which the top-level `session`
  convention (section 5.3) makes possible without understanding the
  frame. An unknown frame with no `session` field is host-scoped and
  forwarded as is.
- Strictness belongs to the effect owner, not every HTTP hop. A gateway
  keeps per-session methods, queries, and body bytes opaque and lets the
  destination host validate them. Its create route rewrites only `host`,
  a directory row merge rewrites only `id`, `host`, and `unreachable`,
  and a host's refusal travels back intact apart from the session-id
  rewrite. Gateway-owned commands such as enrollment are strict at the
  gateway.
- A peer does not advertise which endpoints it serves. A client attempts
  a feature's route and, when the peer answers 404 `unknown_endpoint`,
  shows the user a clear notice that this host lacks the feature. It
  does not hide or disable features up front. The answer comes from
  whoever owns the route, so it holds the same for a host reached
  directly and one reached through a gateway.

### 5.11 Securing the control port

An attached client can run arbitrary commands through the agent, so the
control port is remote code execution. The protocol itself is
credential-free, protection is layered around it:

- **Bind discipline.** Hosts and gateways bind loopback or a tailnet
  interface address, never a public interface. The bare `--listen`
  default is loopback.
- **Tailnet policy.** The deployment restricts the control port to the
  owner's devices with a tailnet grant, and grants the aj control
  capability to the identities that may connect.
- **Identity gate.** Configured per process with `--auth
  <local|tailscale|open>` or `AJ_AUTH` (flag wins). `local` (default)
  accepts loopback peers only, and serving a non-loopback address in
  `local` mode refuses to start rather than serving unauthenticated.
  `tailscale` verifies every connection's peer against the local
  tailscale daemon (a whois lookup on the remote address) and accepts it
  only when the lookup resolves and the peer's login is in the allowlist
  or the peer carries the capability
  `github.com/aljoscha/aj/cap/control` granted in the tailnet policy.
  The allowlist is given with repeatable `--allow <login>` or
  comma-separated `AJ_ALLOW`, where a login is the tailnet login name
  exactly as whois reports it (e.g. `alice@github`). Tagged nodes have
  no login and are accepted only via the capability. `open` accepts
  everyone and belongs only to a network that is private by
  construction. Rejections are 403 `forbidden` with a message that does
  not say why, and every accepted connection is logged with its resolved
  identity. The gate runs before routing, so an unauthorized peer cannot
  probe which endpoints exist.

### 5.12 Host credentials

`/v1/sessions/{id}/credentials` addresses the credential store of the
session's owning host, so a gateway routes it like any session request. The
session is only an address: the host checks it exists and neither resumes it
nor takes its writer lock.

- `GET` returns `CredentialOverview`: the host's OAuth providers (id and
  display name), one safe status row per provider account (`provider_id`,
  `provider_name`, optional label, default and configured flags, summary,
  optional detail), and a `stored` map from provider id to
  `{default, accounts:[labels...]}`, where the empty label is the unnamed
  account. No keys or tokens are returned and the read never refreshes
  anything. `provider_name` follows the
  usage report naming and empty-string compatibility rules (section 5.7).
  The summary describes
  method/source without repeating the provider name. A runtime override has
  its own API-key row and does not hide stored accounts.
  Rows are sorted by provider ID and account label, with the runtime override
  before stored credentials for that provider.
- `POST` takes one `CredentialMutation`, tagged by `action`:
  `store {provider, target, credentials}`, `logout {provider,
  account_label}`, `set_default {provider, account_label}`,
  `logout_with_new_default {provider, account_label, new_default}`, or `logout_all {provider, expected_accounts}`. Unknown
  fields are refused before anything is written. The host applies the store's
  own exact-label and lock-time checks. The answer is tagged by
  `outcome`: `applied`, `removing_default {provider, account_label}` (the
  frontend offers a replacement default or removal of the whole set), or
  `failed {code, message}`.

Login is the client's work up to the last step. The browser is where the user
sits, so the client runs the provider's OAuth flow itself, exactly as a local
run does, and then sends the finished credentials as a `store` mutation. Its
`target` is `{kind:"new", label}` or `{kind:"replace", label}`. For `new`,
`null` is a provider's first login, stored as the unnamed account and refused
if the provider gained a credential meanwhile, and a string adds that account.
For `replace`, the string names the exact account, with `""` for the unnamed
account. A duplicate label is refused on the client before the browser opens. Cancelling during
authorization stores nothing anywhere. A transport failure after the store
request left is an uncertain write: the client says so and never retries on
its own.

## 6. The gateway

### 6.1 Aggregation

The gateway keeps one **control connection** per enrolled host (its
`/v1/events` stream without attachments) and maintains the merged
directory from it: namespaced ids, merged `list` frames, `unreachable`
marking. Client event streams are **spliced**: for each attached session
the gateway opens the upstream stream with the client's own cursor and
`list=none`, since its directory comes from the control connection, and
forwards frames with ids rewritten. It holds no session logs and no
cursors of its own. Commands and reads are proxied to the owning host
with method, query, and body carried unread. A proxied request to an
unreachable owning host is 503 `host_unreachable`.

A client stream's response head never waits on a host. Each host's
sessions are served on their own: the gateway dials that host's
upstream, bounded by its upstream timeout, while the other hosts are
served, and every named session gets its block or an `error` frame. A
session whose host cannot be reached gets `host_unreachable`: the
gateway holds no link to the host, or the dial failed, timed out, or
was refused. One hung host therefore delays and fails nothing but its
own sessions.

`reset` is emitted for exactly the sessions a client attached on a
host, when continuity for them breaks: the upstream ended (including
one that fell silent mid-block), the gateway's link to the host came
back after being down, or the host was withdrawn. After a
`host_unreachable` the next reading of that host as reachable is its
return. The gateway writes every frame about one host's sessions on a
client stream from one place, into that stream's FIFO queue, so a
`host_unreachable` always precedes the `reset` announcing that host's
return. The gateway never re-opens an upstream attachment itself, since
only the client has a current cursor. While the link is down the host's
rows are marked `unreachable`. Removing an enrollment closes the
upstream connections, removes the host's sessions from the merged list,
and ends its splices with `reset`, including one still dialing. The
client's re-attach is then refused per session with `unknown_session`,
its stream and other hosts' sessions untouched.

Typed upstream requests share a gateway-owned HTTP connection pool. Idle
pooled connections may remain until ordinary pool eviction after withdrawal,
but do not keep a subscription or reconnect a withdrawn host. Request and
stream deadlines are unchanged by pooling. The bounded forwarding transport
is separate from the transport for long-lived event streams.

A gateway's `list` frames and `GET /v1/sessions` carry the enrolled
hosts beside the rows in `hosts`: `{id?, address?, name?,
working_directory?, unreachable}`. A host is named by exactly one of
`id` and `address`, `address` only while the gateway has never learned
the id, since it invents none. `name` and `working_directory` come from
the host's latest hello. A client renders an unreachable host it holds
no rows for as an empty group, labelled by name, else id, else address.

The gateway stores no session rows across a restart, only learned host
ids and names: a host that is down when the gateway comes back has no
rows, and its `hosts` entry is the signal. A different id at an enrolled
address is resolved per enrollment kind. A **configured** enrollment
names an address, so a new id is a withdrawal of the old identity
followed by fresh contact: `reset` for its attached sessions, its rows
leave, and the state file adopts the new id before it is published. A
**dynamic** enrollment names the host it shook hands with, so a
different id is refused and the error says to withdraw and re-enroll.

Flow control on a client stream is section 5.9's with the gateway as the
server: the attach block is paced by not reading the upstream connection
until the client reads.

Enrollment routes, strict at the gateway (section 5.10):

| Route | Body | Semantics |
|---|---|---|
| `GET /v1/hosts` | | `{hosts: [{id?, address, source, connected, sessions, error?}]}`. `source` is `config` or `dynamic`. |
| `POST /v1/hosts` | `{address}`, `<host>:<port>` or an `http(s)://` URL | Enroll a host dynamically. Completes a checked hello before adding or recording it. Answers 200 with the host's row. 409 `already_enrolled` for an address already enrolled, 409 `duplicate_host` for an id another enrollment holds, 409 `unusable_host_id` for an id that cannot namespace sessions. |
| `DELETE /v1/hosts/{id}` | | Withdraw a dynamic enrollment. 204. 404 `unknown_host`, 409 `static_host` for a configured host, which is removed from the file instead. |

Static host addresses come from the config file (`--config <file>`,
default `~/.aj/gateway.toml`, which need not exist). Dynamic enrollments
and the gateway's own id live under `~/.aj/gateway/`. An unreachable or
incompatible hello leaves a configured or remembered enrollment in place
but disconnected: nothing is marked connected, published as reachable,
or routed to until a checked hello succeeds.

### 6.2 Process supervision

Reference systemd units for `aj serve` and `aj gateway` ship in
`deploy/`, and aj does not install them. 6161 is the control-port
convention, and a host is configured entirely by `--listen`, `--auth`,
`--allow`, `--name` and their environment equivalents.

These options are available on local interactive launches, `continue`, and
`serve`. `gateway` accepts `--listen`, `--auth`, and `--allow`, but not
`--name`. `connect` accepts session-creation options (`--model-api`,
`--model-url`, `--model-name`, `--account`, `--default-account`, `--speed`,
`--thinking`, `--tag`, and `--env`), not server options. `--api-key` is
local-only. `continue` also accepts `--env` for later in-TUI creates,
but not `--tag`: startup only resumes an existing session. Utility commands
accept none of these options. Shared options may precede or follow the
relevant subcommand.

## 7. Session settings follow the creator

The host supplies the environment (workspace, skills, keys, catalog,
tool availability), but the model, thinking level, thinking display,
speed, and verbosity of a session belong to whoever creates it, per
axis, where the creator stated a value through a CLI flag, an
environment variable, or an entry written in the client's config file. A
built-in fallback is not a preference and does not travel. The create
command sends only stated axes, and stated axes are strict: a value the
host's model cannot serve fails the create with an error naming the
supported values, except for speed's explicit model-specific fallback below. A stated model
must be servable by the host (present in its catalog, with credentials).
Unstated axes are the host's to default, model-aware: its own configured
default when the chosen model supports it, otherwise a supported value.
After creation the settings command mutates the axes, from any client,
as peers, with host-owned model resolution. Model edits preserve the current
effort, and the next inference validates the combination. Thinking display is an inference
setting because it changes what the provider is asked to emit.

Oracle is an advisory child with its own model, thinking effort, and verbosity.
Its `oracle_*` settings use the same vocabularies and precedence as main's
settings, with independent built-in, user, and project defaults. Speed is one
shared preference for Main and Oracle, not an `oracle_*` setting. An
Oracle model selection requires both `api` and `name` and may include a URL
override. There is no partial model selection or follow-main mode. Oracle
thinking accepts `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`.
For verbosity, `default` uses the selected model's catalog default, then the
server default, gated by `supports_verbosity`. Explicit verbosity overrides
the catalog default. Settings and session entries keep `default` unresolved
so model changes follow their own catalog defaults, without live catalog
discovery. Oracle uses its own model's default, not main's value.

Speed supports `standard`, `fast`, `ultrafast`, `flex`, and model-advertised
named identifiers. `default` leaves the choice to each model's advertised
catalog default. Explicit `standard` requests ordinary processing even if a
provider project or catalog defaults to a premium mode. Supported choices and
available prices travel with model metadata. A selected preference is retained
in the session's entries, while each model independently resolves unsupported
modes to standard speed. The host reports fallback notices. Catalog support
does not guarantee account eligibility, and a server rejection is surfaced
rather than retried at a different price. Flex does not trigger an automatic
fallback to standard capacity.

A speed edit targeting Main applies the shared preference to Main and Oracle.
An edit targeting a promptable child records a thread-local override. Speed
changes apply at the next turn, not to in-flight requests. `oracle_speed` is
not accepted in create, settings, or branch-edit requests. Historical Oracle
speed records remain readable but do not override the shared preference.

Oracle calls use the same inputs and delivery modes as the `agent` tool. Calls
block by default. With `run_in_background: true`, the tool returns the task id
immediately and the report arrives as a task notification. Background
Oracle children use ordinary task cancellation and shutdown ownership.

A creator sends only stated Oracle choices. The host resolves omitted choices
from its Oracle defaults and defaults unstated effort against the selected
Oracle model. Explicit creation and branch choices are validated together.
Both roles obtain a complete model selection from their effective configuration.
Changing only a provider or URL leaves the other components unchanged. A connected
creator sends the complete selection when any model component was stated. With
none stated, the host uses its own defaults. An unavailable configured model is
an error, never a request to pick another catalog entry. Attaching to an existing
session does not validate the client's model defaults against the host catalog.

After creation, each model or inference edit uses the same per-axis application
path as main. A model edit does not require changing the saved effort first.
The next inference validates that model/effort combination.

Oracle settings are recorded on the selected user branch and restored on resume.
New sessions and cold resumes start from Oracle's effective configuration, then
recorded axes replace those defaults. A head switch restores recorded axes over
the current runtime choices, just as for main. Unrecorded historical axes remain
unknown in session-info and branch metadata. Live selectors use `oracle_settings`
from state frames, while armed branch selectors use only historical metadata.
Each main turn captures its Oracle bundle at turn start. Edits during that turn
apply to consultations in the next main turn. Main model, effort, and verbosity edits never change Oracle's choices. Speed
edits apply to both roles, and a retained child keeps its own bundle. Session selectors change
only the current session.
Settings-window edits also save the chosen default on the host, whether the
client is local or connected. Explicit model saves record both provider and name,
including a selection equal to the built-in default. Endpoint defaults, like
main's `model_url`, take effect on host restart.

Main and Oracle both require resolvable model defaults. Invalid model or endpoint
configuration fails startup for either one. Both restore recorded settings over
their configured defaults using the same rules. If a recorded model is no longer
available, restoration retains that agent's current resolved model and reports
the fallback.

Credentials resolve lazily for both models. Known missing credentials for a
separate Oracle provider are reported against the host's store during attach and
print startup. Login takes effect without rebuilding the session. A failed
consultation returns a tool error through the ordinary child lifecycle.

Sub-agent spawn events carry `tool_name`, the originating tool, and the spawn
record preserves it for replay and reconnect. The transcript identifies Oracle
children as `agent(oracle) N`, as does the agent switcher, where the origin is
also searchable. An absent or empty origin remains an ordinary `agent N`, as
does the `agent` tool.

They also carry `call_id`, the id of the parent's tool call that spawned the
sub-agent, and the spawn record preserves it. The field is optional: a peer
or a record that predates it sends none, and a receiver treats an absent
`call_id` as unknown. A host that reopens a session uses it to answer a
spawning call its last process left without a result: with the sub-agent's
report when its run ended, and otherwise with the news that the sub-agent
was interrupted. A call whose spawn record names no `call_id` gets the
generic interrupted result.

Creation may also supply `settings.account: {name: ...}` for the initial
provider. An absent or null outer `account` leaves the choice unchanged.
An object with absent or null `name` explicitly resets to Provider default,
`{name: ""}` pins the unnamed account, and `{name: "label"}` pins that exact
provider-local label. Unknown nested fields are rejected. This choice is
validated by the host with the other creation settings before minting.
After creation, selection uses the account route rather than the inference
settings route. Account choices are session-scoped and provider-local,
never changes to config or auth-store defaults. There is no account footer
field and no new config setting. Login, logout, and auth default management
change the host's store through the credentials route (section 5.12). The
account route carries selection and non-secret account metadata only.

The session environment overlay is separate from inference settings. Any
client can read or edit it after creation through the environment routes
(section 5.6 and 5.7). An edit changes only the session's active branch,
never the host process environment or config files. Keys must be nonempty
and contain neither `=` nor NUL, and values must not contain NUL. Unknown
request fields are refused before mutation. Removal exposes any inherited
process value rather than masking it. The environment read reports only
entries remaining in the overlay.

The optional top-level `env` on `CreateSessionRequest` supplies the initial
branch environment. It is separate from inference `SessionSettings` and is
validated before minting. Absence records no overlay, and an explicit empty
map records an empty overlay. `--env KEY=VALUE` supplies this map to every
create the invocation performs, including connected in-TUI new sessions.
Attaching or resuming does not apply the launch map to an existing session.

A current-protocol receiver that lacks the field refuses it before minting under
the closed-schema rule (section 5.10). The client surfaces the refusal and
never retries with env removed. Mismatched protocol versions are refused at hello.
The ordinary successful create response needs no env echo or proof exchange.
Gateways forward the map to the owning host without interpreting it.

## 8. Client TUI

Settings, skills, tool/skill toggles, and session environment lists share picker
navigation: Home/End select the first/last filtered row, PageUp/PageDown move by
the visible list height, and Up/Down move one row. A left click only selects.
Enter performs the row's edit or opens its submenu. Navigation remains available
while a save is pending, without bypassing the edit guard. Keyboard focus stays
in search, with Ctrl+A/Ctrl+E moving within the query. Description panels, blank
list space, and scrollbars do not select or edit settings.

### 8.1 Connect mode

`aj connect <url>` runs the TUI against a remote server. Queue state and
the task table come from `caught_up` (section 5.3) and the live
`QueueUpdate`, `TaskStart` and `TaskEnd` events after it, footer settings
and the restore notice from `state` frames.

Session selection asks for one of three things:

- Bare `aj connect <url>` attaches the host's most recently active
  unarchived session. If none exists, it fails and suggests `--new`.
  Local `aj continue` follows the same resume-only rule and suggests `aj`
  when no unarchived session exists. Naming a nonexistent session fails too.
- `aj connect <url> <session-id> [input...]` attaches that session
  whatever its archived bit says. Launch input follows the id.
- `aj connect <url> --new [input...]` creates a session. Under `--new`
  every positional after the url is launch input, so "create this named
  session" is not expressible.

Explicit `--model-api`, `--model-name`, `--model-url`, `--thinking`,
`--speed`, `--account`, and `--default-account` select settings for a new
session only. They require `connect --new` in remote mode and are accepted
on either side of the subcommand. Bare connect rejects these flags before
contacting the host. Environment and config values remain creation defaults,
not explicit flags, and do not change an attached session. Use interactive
settings to change existing sessions.
`--api-key` is local-only and is rejected on connect. Configure credentials
on the host instead.

Local new interactive and print sessions accept the same settings flags,
including `--api-key`. Explicit settings flags are rejected on `continue`,
`serve`, `gateway`, and utility subcommands. Host defaults come from config
and environment. Resume still accepts prompt text and file attachments.

`--tag <label>` names the session a run creates. `--host <id>` names the
host a create through a multi-host gateway is for (section 5.6): an id,
or a prefix only one host answers to, resolved client-side. A value that
fits none or several is refused with the candidates listed before the
terminal is taken over. A run that attaches instead of creating reports
that `--host` and `--tag` had nothing to point at.

The boundary of what works over the wire is explicit. Supported: prompt,
steer, cancel, queue withdraw and clear, settings including model switch
and thinking display, compaction, task kill, the task-output overlay,
tagging, archiving, environment overlay reads and edits, session account
reads and selection, credential status, login, logout and provider defaults,
HTML export, the session-info and provider-usage overlays (including confirmed
host reset-credit consumption), the tree view and
head switching, and session
creation and switching. The prompt recall ring holds this run's own
submissions only. On exit, the client prints the focused session's id and
an `aj connect <url> <session>` command using the connected endpoint and
the complete id, including its host prefix through a gateway. URLs with
userinfo, query parameters, fragments, or control characters are replaced
by `"$AJ_CONNECT_URL"`, with an instruction to set it to the same connection
URL. No usage summary is printed. An
unsupported action never silently does nothing.

Clipboard images and image-file paths pasted by a terminal drop become
client-local editor attachments. Submission prepares image content on the
client and sends bytes through the existing prompt `content` field. The
host never opens the client's path. Preparation or submission failures keep
the draft in the editor. Hosts and gateways accept request bodies up to 32 MiB,
including JSON and base64 overhead. Image preparation also precedes a requested
branch change, and its bytes stay with the pending branch prompt. The host saves
an accepted image prompt with its bytes. A client shows and takes back queued
text only, so busy-agent image submissions are refused without discarding the
draft.

Account selection attempts the host's endpoint and shows the ordinary
unsupported-endpoint notice if the peer lacks it. It does not fall back to
reading or modifying the client's auth store. Credential pickers and the
auth status overlay follow the same rule through section 5.12: they read the
host's store, name the opening host, and keep its session address across
focus changes. Login runs the OAuth flow on the client, opening the browser
here when one is available, and stores the result on the host. Cancelling
before the store closes the dialog with nothing written. A store request
whose answer is lost is reported as unconfirmed, including a gateway's
`host_unreachable` response. The notice asks the user to reopen auth status
before retrying. Logout and provider-default changes also run off the input
loop, including the status read after logout. Session switches do not wait
for these writes, which retain their opening host. No write is retried
automatically.

Prompt-history search uses Control in local, direct, and gateway modes. Opening
or switching scope starts a user-paced read off the input and render loops.
Local and HTTP scans publish coalesced provisional snapshots as files are read,
ending with a complete bounded list or partial results with named failures.
The overlay remains interactive while loading. Search covers the full prompt.
A left click selects a row without closing the overlay or moving keyboard focus
from search. Enter recalls the selection into the editor without submitting.
Home/End select the first/last filtered result. PageUp/PageDown move selection
and viewport by the visible list height, clamping at either end. Ctrl+A/Ctrl+E
move within the search text instead. While the query is empty and the user has
not edited, selected a row, or scrolled, background snapshots select
the youngest prompt at the top. After interaction, they retain the selected
prompt and its screen row while it survives. Clearing the query does not re-enable
automatic following. Replacing the scope starts a fresh list with the query retained.
If the selection is offscreen, they retain the top visible prompt instead. Removed anchors fall
back to the nearest remaining rank. Changing the query selects the best match
again. Updates paint at most ten times per second, skipping unchanged
snapshots without delaying the first results.
Closing or changing scope cancels the outstanding client read. No history cache
or journal is persisted, and no history read belongs to directory or sidebar polling.
Unsupported endpoints produce a notice (section 5.10).
The up-arrow ring is independent, retaining local bootstrap behavior and only
this run's submissions over a connection.

The usage overlay captures its Control connection and complete focused session
address when opened. It renders that host's reports, offers eligible provider
accounts with nonzero credits and a host reset adapter, confirms the exact
account, and retains its target and idempotency key on retry. Completion offers
a user-paced refresh against the same captured address, even if focus changes.
It attempts both endpoints and renders a clear
notice on `unknown_endpoint`, with no fallback to client credentials.
The report supports line scrolling, Home/End, and viewport-sized pages with
the same context overlap as read-only content overlays. Provider, confirmation,
and retry menus use picker navigation and select-only clicks. Clicking never
spends a credit or retries a reset. Enter performs the selected menu action.

Task output follows new output on open. Keyboard arrows scroll document lines,
and PageUp/PageDown use the visible body height with the read-only page overlap.
Keyboard scrolling or Home pauses following without discarding the reading
position when more output arrives. End immediately reveals the latest output
and resumes following.

Connection state (connected, reconnecting, catching up) is surfaced in
the footer.

The same session browser opens locally, directly connected, and through a
gateway. It starts with selectable directory rows and progressively enriches
exactly those rows with previews from their owning hosts, read in small batches in
directory order so the top of the list fills first and the rest keeps landing
behind it. An arriving preview fills in its own row and leaves the highlight
and the scroll where the user put them.
A left click selects a row without resuming it or moving keyboard focus from
search. Enter confirms the selected session. Blank space and the scrollbar do
not select rows. Home/End and PageUp/PageDown navigate the filtered list with
the same behavior as prompt history.
It reads the complete list, including archived sessions, so prompt-text search
does not depend on which rows have been visible. Search covers the full first user text block even when
the displayed preview is truncated, plus tags, ids, and host labels. Loading
is shown while the search corpus is incomplete. Failed reads leave the basic
rows selectable, with a notice and an incomplete-search indicator. Reopening
refreshes previews. Closing cancels remaining reads. Preview reads belong to
this user gesture, never to periodic directory enumeration or the sidebar.
Rows place the current-session marker (`▌`), tag, and aligned metadata columns
before the preview. Session state is shown in every mode. Gateway rows also
name their host. Metadata keeps its inline labels, without separator dots
or a table header, and yields space to a recognizable prompt on narrow screens.
The preview uses the remaining width and clips at the right edge.
The current-session marker is independent of the keyboard selection highlight.
Archived rows strike through all their text without adding a column or changing
their colors and selection highlight.

A reachable row reported as `locked` shows `L` for a session in use by another
process, with `in use` in the state column. The compact indication remains
recognizable when metadata is clipped. It does not replace the current-session
marker or prevent selection and attachment attempts, even after a locked refusal.

### 8.2 The sidebar

A persistent sidebar lists the directory's sessions grouped by host,
with glyphs for working, unseen output, in use elsewhere (`L`), and unreachable.
Unreachable takes precedence over a stale lock indication. Rows carry no
preview text: nothing in the directory contract reads log content per
row. The promises the host relies on:

- **The attach set is retention.** The client keeps one `ChatState` per
  attached session, live frames keep arriving for background sessions
  over the same stream, and switching focus is a view swap. An
  unattached session shows only its `list` row (section 5.8). A first
  focus attaches, so a session the user never opens is never projected.
- **Background attachment is a bounded working set**, LRU over focus, so
  a browse through the store does not pile up live drivers and locks on
  the host (section 4). A session that falls out of the set is detached
  by reopening the stream without it, and the host releases it after its
  idle grace. Re-focusing is an ordinary re-attach. Detaching may keep
  or drop the client-side `ChatState`, dropping is always safe.
- **The focused session is never detached.**
- **Archived rows leave the LRU.** The set holds no archived session but
  the one the user is in, enforced whenever the bit or the focus moves,
  so the host can release a session its user said they were done with.
  Archived rows also leave the display, with the focused row and
  attached rows as the only exemptions, behind an explicit reveal that
  is client state and never sent anywhere.
- **Switching and creating are never refused because a turn is
  running.** Head switching and environment editing are idle-only and can
  return the host's 409.
- **Creating through a multi-host gateway names the host explicitly**
  through a selector over the directory's hosts. A confirm that names no
  host creates nothing.
- **The archive action** sends the opposite of the bit the client holds,
  always an explicit direction, and records an accepted answer ahead of
  the peer's next `list`. A peer that does not serve the route leaves a
  notice, read off the command's `unknown_endpoint` error.
- **Session and sidebar commands are `AjAction`s** on the existing keybinding
  system (sidebar toggle and focus, session switching, remote creation,
  tagging, archiving and the reveal, folding a host's rows), with a
  default chord and user overrides. Pointer gestures dispatch the same
  actions.

A host's group header reads the host's name (section 5.1), else its id,
else its address, and the create-flow host selector labels hosts the
same way. Rows show the session's tag where one is set, else an
id-derived label.

The sidebar's separator becomes a heavy line with a horizontal resize cursor
on hover and during a drag. Dragging adjusts this client's width without saving
it or sending a command to the host. An explicit sidebar-width setting takes
precedence over the dragged width. The gesture reserves space for the transcript
and does not select text or activate session rows.

## 9. Testing

A client that connects mid-session or reconnects after drops must converge
on the same durable-derived transcript as an uninterrupted client. This
includes transcript content and identity, sub-agent status and reports,
tasks, queued messages, settings, usage and lifecycle. Wall-clock timings
and client-local view choices need not agree.

Exercise that promise through the real host and HTTP client, including
seeded disconnects and cursor-based reattachment. Keep independent expected
transcript outcomes so two equally broken clients cannot satisfy a
comparison. Tests should observe stable application and peer boundaries,
not internal indexes or incidental counts of frames and transcript rows.

A disconnected client cannot recover a reliable-transient notice whose only
delivery happened while it was away (section 5.4), or unfinished streaming
text superseded by a durable message. Recovery comparisons exclude those
artifacts, but keep every durable-backed row. Uninterrupted clients must
agree on transient notices too. Fault scenarios must actually interrupt
durable history and include a notice raised while the client is away.

## 10. Accepted limits

- A fresh backfill and a history page each walk the session's state
  records and the messages the host read for their range, and emit only
  their range. What a SQLite host reads is described in
  [session storage](session-storage.md). A background sub-agent's nested
  tool activity that crosses a page boundary can show as two cells.
- `list` frames are cumulative over the whole store (about 60 KB at 400
  sessions). Debouncing bounds the rate, not the size. A row cap or
  delta encoding is the follow-up if a store gets big enough to hurt.
- A sub-agent run without a stored end record whose turn is not stored as
  running, one from before end records or one dropped with its parent's
  cancelled turn, is closed on reopen from its last message: the report is
  its last assistant text, and the conclusion follows that message's stop
  reason.
