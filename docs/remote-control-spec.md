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
  Status is working or idle, nothing more.
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

The session host owns N live sessions and exposes one attachment
interface used by both the local TUI (in-process, through direct
handles) and the network server. Both reach the same command methods.
The host layer depends on no terminal, which is what `aj serve` is.

- **Lifecycle.** Sessions are materialized on demand (an attach or
  command naming a session known on disk) and torn down on shutdown. A
  live session that is quiescent (no turn in flight, no live background
  tasks, no queued messages) and has no attached client is released
  after an idle grace period: the driver is joined, log buffers are
  flushed, the lock is released, and the epoch dies. The next attach or
  command re-materializes it with a fresh epoch and a full backfill.
  Only log-durable state survives a release, as on a process restart.
  Attachment is the retention signal (section 8.2). A created session
  with nothing durable on disk is exempt and stays live for the host's
  lifetime.
- **Single-writer safety.** Materializing a session takes an advisory
  lock (a file in the store's `locks/` directory) before anything reads
  or repairs the log, so a refused materialization has not touched the
  file. A second process that hits the lock refuses to materialize that
  session: HTTP 409 `locked`, or the `error` frame of section 5.5 on an
  attach. The lock file records its holder (pid and host id) and the
  refusal names it.
- **Sequence assignment.** Durable events are tagged with their log
  entry's append index at the append site, never inferred from log
  length at consume time. Per session, **live** durable frames reach any
  given stream in strictly increasing seq order, and never at or below
  the stream's backfill boundary. Backfill blocks follow projection
  order (section 5.5).
- **Commands.** Section 5.6. Settings changes are applied by the host,
  written as settings log entries, and published as the projected notice
  event plus a refreshed `state` frame. A settings entry that lands
  before its thread's first message projects no notice, so its
  confirmation is published untagged.
- **Head switching.** Refused with 409 `conflict` while a turn is
  running or background tasks are live. On success the host clears the
  session's pending queues, mints a new epoch, and emits `reset`.
- **Host-level state.** The session directory (section 5.8), a
  `QueueUpdate` on every queue mutation (enqueue as well as drain),
  per-session `state` frames (section 5.3), and epoch/seq bookkeeping.
- **Shutdown** (SIGTERM, or the interactive host quitting): running
  turns are cancelled through the ordinary abort path, background tasks
  quiesced, log buffers flushed, client streams closed. Sessions wind down
  concurrently. The process gives orderly teardown one 30-second grace,
  then exits nonzero with a diagnostic if cleanup is unfinished. Another
  stop signal during teardown exits immediately. Forced exit can lose
  buffered work and abandon subprocess cleanup. In-process session cleanup
  retains writer locks until its tasks and persistence listeners finish,
  without its own host-wide deadline. The gateway uses the same process
  exit policy.
- **Persistence failure.** A conversation-log write error is terminal
  for that materialization: the log refuses every later write. The host
  publishes one `error` frame with code `persistence_failed` on every
  stream attached to the session, detaches the session from those
  streams, and tears it down. The stream stays open and a later attach
  or command re-materializes the session from disk. If the failure hit a
  session's first publication there is no log to reopen: the message
  says the submitted message was not recorded, and the id answers
  `unknown_session` from then on. Fatal turn failures that do not
  compromise persistence stay scoped to the turn.

## 5. Wire protocol

### 5.1 Transport

HTTP. Commands are JSON POSTs, reads are GETs, effects arrive on an SSE
stream as one `data: <json>` frame per line. No WebSockets, no JSON-RPC,
no correlation ids. All routes live under `/v1/`. Unknown query
parameters are ignored.

`GET /v1/hello` is the reachability and identity probe: `{protocol,
capabilities, app_version, host_id, working_directory?, name?}`. A
gateway omits `working_directory` and `name`, which is how a client
tells the two roles apart (section 5.10 for its capabilities).

A host's `name` is display metadata, never an address, and may collide.
The host states it at startup (`--name` or `AJ_NAME`, else its working
directory abbreviated to `~` under home), and there is no rename over
the wire. A name is one trimmed line of at most 80 bytes with no control
characters, which a reader enforces itself. Absent means the reader
falls back to the id.

SSE streams send a `heartbeat` frame after 30 seconds of idleness, and
clients treat 60 seconds of silence as a dead stream. Error responses
carry `{code, message}` (section 5.6 for the envelope rules):

| Status | Code | Meaning |
|---|---|---|
| 400 | `invalid_request` | Malformed request: bad JSON, unknown field, bad cursor, a body naming two settings axes, an empty prompt, a session named twice in one attach. |
| 403 | `forbidden` | The identity gate refused the peer (section 5.11). The message never says why. |
| 404 | `unknown_session`, `unknown_task`, `unknown_entry` | The named thing does not exist. A syntactically invalid session id is `unknown_session`. |
| 404 | `unknown_endpoint` | No such route on this peer. The answer a capability probe reads (section 5.10). |
| 409 | `conflict` | Well formed but refused by session state: a turn running, tasks live, a running mark with no owning turn or task. |
| 409 | `locked` | A rival writer holds the session's lock (section 4). |
| 409 | `unsupported` | Well formed and unconflicting, but this host cannot serve it: a model it has no credentials for, a settings change for an agent that is not live. |
| 500 | `internal` | Host or gateway failure. |
| 503 | `host_unreachable` | Gateway only: the owning host's control connection is down. |

A gateway adds its enrollment codes (section 6.1) and relays a host
refusal whose body is not an envelope as `host_refused`.

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

- `event`: `{kind, session, epoch, seq?, entry_id?, event}` where
  `event` is a serialized `AgentEvent`. `seq` and `entry_id` are present
  if and only if the event is durable (section 5.4). The envelope's
  semantics apply whether or not the nested event type is known to the
  receiver.
- `state`: `{kind, session, epoch, working, settings, last_seq}`.
  `working` says whether the session's **main agent** has a turn in
  flight, `settings` is the active `AgentSettings` (`provider`,
  `model_id`, `thinking`, `thinking_display`, `speed`, `verbosity`),
  `last_seq` is the durable high-water mark. Sent at the start of every
  attach block, before the backfill, and whenever `working` or
  `settings` changes, never for `last_seq` alone. The host publishes no
  "restored session" notice, a client renders one from the first
  attach's `state`. `working` applies on every `state` frame and
  self-heals a spinner left running by a missed `AgentEnd`. It says
  nothing about sub-agents, whose liveness comes from lifecycle events.
- `caught_up`: `{kind, session, epoch, last_seq}`. Ends a backfill
  (section 5.5). A client must not commit a cursor from a `caught_up` it
  did not ask for.
- `list`: `{kind, sessions: [...], hosts?: [...]}`. The full session
  directory (section 5.8). `hosts` is present only from a gateway
  (section 6.1). Cumulative, the latest frame supersedes all earlier
  ones.
- `error`: `{kind, session, epoch?, code, message}`.
  The error envelope (section 5.6) as a session-scoped stream frame.
  Every per-session resolution failure travels this way with its own
  code (`unknown_session`, `locked`, `persistence_failed`, ...). Only
  failures of the request itself (a session named twice, a shut-down
  server, a malformed cursor) fail the request.
- `reset`: `{kind, session}`. Continuity for this session is broken
  (head switch, or a gateway lost or regained its host). The client
  re-attaches, offering its cursor (section 5.5).
- `heartbeat`: `{kind}`.

Session-scoped frames carry their session id in a top-level `session`
field, which is what lets a gateway rewrite ids in frame kinds it does
not understand. Unknown frame kinds, unknown keys, and unknown `event`
types must be ignored by clients (section 5.10).

### 5.4 Reliability classes

Every frame is in exactly one class:

- **Durable** event frames correspond to persisted log entries: the
  `MessageEnd` that triggers persistence, the `SubAgentStart` that
  writes the spawn root, the `CompactionEnd` whose checkpoint entry the
  compaction path appends, and the notices the projection derives from
  notice-producing state entries. Durable frames carry `seq` (the
  entry's 1-based append position, so `0` reads as nothing durable yet)
  and `entry_id`. They are exactly what backfill can regenerate. Seqs
  are strictly monotone per session but **not contiguous**, so clients
  must not do gap detection: continuity comes from the stream being FIFO
  plus explicit `reset`.

  At most one frame per log entry is durable. An entry can project
  several events (a tool-result entry projects a tool bracket around its
  `MessageEnd`, assistant entries project a trailing `UsageUpdate`,
  priced compactions project a trailing `CompactionUsageUpdate`) and
  only one of them carries the tag, in live flow and backfill alike. A
  `CompactionUsageUpdate` carries the checkpoint entry id in its payload
  even though it carries no durable tag.
- **Lossy** frames are the three cumulative-snapshot events,
  `MessageUpdate` (keyed by agent id), `ToolExecutionUpdate` (keyed by
  call id), `TaskOutput` (keyed by task id), plus the `list` and `state`
  frame kinds (each its own key). They may be coalesced or dropped under
  pressure, the newest one or a later durable frame supersedes. New
  frame kinds declare their class when introduced.
- **Reliable-transient** frames are everything else: tool start/end,
  sub-agent end, task start/end, notices, warnings, errors, lifecycle
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
  persisted, so a host restart always invalidates cursors, which is
  right: the log tail is not crash-stable (buffered non-punctuation
  entries, torn-tail truncation). Within one epoch, "durable events
  after seq N" is well-defined.

Attaching is done through the stream itself, there is no separate
snapshot request. The stream request names sessions to attach, each with
an optional cursor: `GET /v1/events?session=<id>[@<epoch>:<seq>]`
(repeatable). The cursor is split off at the first `@`, so session ids
never contain one, and the cursor splits at its last `:`, so an epoch
may contain a colon. Attaching nothing is legal: that stream carries
`list` frames and heartbeats only.

A stream request never fails wholesale over one bad session: each named
session either gets its attach block or a session-scoped `error` frame
carrying the failure's own code (section 5.3), and the rest are served.
Any error frame ends that attachment, and the dropped cursor costs only
a full backfill later. For each resolvable session the server,
atomically with respect to that session's event flow, registers the
subscription, projects the durable suffix, and emits in order on the
stream: a `state` frame, the backfill (projected events for entries
after the cursor, or from the beginning when the cursor is absent or its
epoch does not match), and `caught_up`. Live frames for that session
follow. Because subscription and projection are atomic and the stream is
FIFO, no durable event can be missed or delivered below the backfill
boundary within a connection. Reliable-transient frames in flight at
attach time can still arrive after `caught_up` and overlap the backfill,
so application must be idempotent (below) on first attach as well as on
re-attach. Lossy frames in flight at attach time are **dropped**, since
a stale snapshot after the durable frame that superseded it would
resurrect transient state. A cursor beyond the session's current
`last_seq` is treated as an epoch mismatch.

Sessions not named in the request produce nothing on the stream except
their rows in `list` frames, so a client is never evicted over traffic
it did not ask for. Changing the attach set means reopening the stream
with new parameters.

Backfill projection rules, which differ from dead-log replay:

- The host projects from the full log so server-side context (tool name
  maps, usage running totals) is complete, and emits only events for
  entries after the cursor.
- A **running** sub-agent's bracket is never force-closed, at the cursor
  boundary or at end of log. The real `SubAgentEnd` arrives live later.
  A **finished** sub-agent's bracket still is, because a conclusion is
  not persisted and the run's last assistant message is the only source
  for it. The host tells the projection which sub-agents are running,
  and concludes every sub-agent it knows to be idle after `caught_up`,
  which unwedges a box whose `SubAgentEnd` fell into a disconnected
  window even when no durable entry follows the cursor.
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

- **Epoch adoption**: an attach block the client itself requested
  establishes the session's current epoch: the client adopts the epoch
  of the block's opening `state` frame and applies the block under it,
  which is how a stale cursor (head switch, host restart) accepts the
  full backfill served under the new epoch.
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
  conclusion. After `caught_up` the client refetches the task table and
  the pending-message queues (section 5.7), because neither is
  replayable. A client may instead discard the session's state and
  rebuild from a full backfill, which must produce the same result.
- On `reset`, the client re-attaches, offering its cursor. The server
  serves an incremental suffix if the epoch still matches, or a full
  backfill if not. A `reset` received mid-attach-block abandons the
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
  - `persistence_failed`: re-ask immediately when the frame folds. The host
    treats the failed materialization as absent and rebuilds from disk.

### 5.6 Commands

Commands are JSON POSTs. Effects are observable on the stream or the
corresponding read. Mutations against a specific session return 202 on
acceptance. `POST /v1/sessions`
returns 200 with the new session's id. Commands that act on "the viewed
agent" locally take an optional `agent` field (default: the main agent).

| Command | Body | Semantics |
|---|---|---|
| `POST /v1/sessions` | `{host?, settings?, prompt?, tag?}` | Create a session in the host's working directory. `settings` per section 7. `prompt` is `{text}` or `{content: [...]}`. Settings, prompt and tag are validated before a log exists, so a refusal leaves nothing behind. The tag and first prompt are applied afterwards under the session's own lock and are best-effort: one that does not land still answers 200 `{id}` plus an `incomplete` string carrying the host's words for what did not stick, so the client retags rather than creating a second session. A first prompt whose persistence fails does so on the stream, as `persistence_failed`, after the id was answered. A minted session is never deleted to make an error tidier. |
| `.../{id}/prompt` | `{text}` or `{content}`, optional `agent` | Run a turn if the agent is idle, queue a follow-up if busy. |
| `.../{id}/steer` | `{text, agent?}` | Queue steering, or promote the pending follow-up when `text` is empty. |
| `.../{id}/cancel` | `{agent?}` | Cancel the targeted agent through the mechanism that owns its run: its driven turn, a detached sub-agent's background task, or the foreground-sub-agent-cancels-main cascade. A running mark with no owning turn or task is 409 `conflict`. An idle or completed target is accepted. |
| `.../{id}/queue` | `{op: "remove", agent?}` or `{op: "clear"}` | Withdraw one agent's pending message, or clear the session's queues. A withdrawal answers 200 `{text?}` with the text it took, which is what makes the dequeue-into-the-editor gesture work. One agent holds at most one coalesced pending message, so there is no index. A clear answers 202. |
| `.../{id}/compact` | `{instructions?}` | Manual compaction. |
| `.../{id}/settings` | exactly one of `model`, `thinking`, `thinking_display`, `speed`, `verbosity`, optional `agent` | Host applies, logs, and publishes the synthesized frames. Naming zero or two axes is 400. A remote change never persists to the host's config files. |
| `.../{id}/env` | `{key, value}` | Set one session environment value, including the empty string. Null or an absent `value` removes the key from the map, not from the inherited process environment. Idle-only: 409 `conflict` while a turn or background task is live. Validates before mutation and persists the full resulting active-branch map. Capability `session_env` (section 5.10). |
| `.../{id}/tag` | `{tag}`, empty or absent clears | Set the session's tag (section 5.8): one trimmed line, length-capped. Materializes like any command so the session lock covers the sidecar write. |
| `.../{id}/archive` | `{archived: bool}`, absent reads false | Set or clear the archived bit (section 5.8). Materializes like any command, so a rival's lock refuses it. Nothing else refuses it: a session working through a turn takes it and goes on working. Capability `archive` (section 5.10). |
| `.../{id}/head` | `{entry}` or `{before: <entry_id>}` | Switch the session head. 409 `conflict` while working or tasks live. Clears queues, new epoch, `reset` frame. `before` resolves the named entry to its parent server-side, atomically with the switch. An unknown entry is 404 `unknown_entry`, an entry with no parent is refused. Exactly one of the two fields. |
| `.../{id}/tasks/{task_id}/kill` | `{}` (absent or blank is equivalent) | Kill a background task. Any field or non-object value is refused before the task is touched. |

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

### 5.7 Reads

- `GET /v1/sessions`: the same payload as a `list` frame, `{sessions,
  hosts?}`. Includes every session of the host's working directory,
  on-disk as well as live, with a liveness flag. Attaching or commanding
  a non-live session materializes it (lock permitting). A read never
  does, except for the tree and environment reads, which parse the log
  and materialize like a command. This is the discovery surface, there is
  no separate on-disk listing.
- `GET /v1/sessions/{id}/tasks`: `{tasks: [{id, owner, call_id, kind,
  label, status, started_at}]}`, the background task table with
  wall-clock timestamps. Clients replace their task table with this
  after `caught_up`, and ignore `TaskOutput` for unknown task ids.
- `GET /v1/sessions/{id}/tasks/{task_id}`: `{id, status, stdout_tail,
  stderr_tail, stdout_total_bytes, stderr_total_bytes, report?}`. This
  backs the task-output overlay in connect mode, the spill file on the
  host's disk is not reachable remotely.
- `GET /v1/sessions/{id}/queue`: `{queues: [{agent_id, steering,
  follow_up}]}`, the pending messages per agent.
- `GET /v1/sessions/{id}/tree`: `{segments, head?}`, the
  segment-collapsed branch tree for the tree view and head switching.
  `head` is the current head entry id, absent only while the log has no
  head, and not derivable from the segments.
- `GET /v1/sessions/{id}/env`: a JSON object mapping strings to strings,
  the full environment map selected by the active branch. Values are
  unredacted on the trusted control port. Export-only redaction does not
  apply. This reads the session overlay, not the host process environment.
  Capability `session_env` (section 5.10).

### 5.8 Status model

Per-session row fields in `list` frames and `GET /v1/sessions`:

- `id`: the session id (section 5.2).
- `live`: materialized in the host, vs on-disk only.
- `working`: the main agent has a turn in flight (section 5.3). Live
  background sub-agents surface through `tasks`, not here.
- `queued`: `{steering, follow_up}` counts of pending messages.
- `tasks`: count of live background tasks.
- `last_activity`: host-clock timestamp on every row. For a live row it
  is the last durable event the host observed, and a release hands that
  stamp to the cold row. The log's mtime stands in only where the host
  has no answer, so a restarted host can disagree with its predecessor.
- `last_seq`: only on live rows. A cold row never carries one: an exact
  entry count costs a read of the whole file, and a list-observed
  position is never a cursor anyway (section 5.5).
- `tag`: optional user-set label, display metadata, never an id.
  Session-scoped, so it lives in a sidecar (`meta/<session id>.tag`, one
  line of UTF-8) that a head switch cannot move.
- `archived`: user-set bit, display metadata with no lifecycle meaning:
  an archived session keeps its log, its lock, and any turn it is
  running. Stored as a sidecar (`meta/<session id>.archived`) whose
  existence is the whole bit. Absent reads false. Changed only by the
  archive command, deliberately with no create-time field.
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

`list` frames are lossy (section 5.4) and debounced on a short tick, so
`last_seq` churn never produces a frame per event. A snapshot a
subscriber has already accepted (queued or delivered) is not sent to it
again.

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
- Durable and reliable-transient frames are never dropped. If a client's
  bounded queue overflows with them, the server disconnects that client.
  Recovery is the ordinary re-attach with cursor plus reconciliation
  (section 5.5), which is what makes eviction safe.

### 5.10 Compatibility

Both ends of every connection are aj, but versions skew. Rules:

- `GET /v1/hello` carries `protocol` and `capabilities`. Protocol 1 is
  the generation whose servers ignore unknown request fields, protocol 2
  the strict-command generation this document describes. The exact
  version check is the boundary: a protocol-2 client sends no create or
  command after a protocol-1 hello, and a gateway opens no link to a
  host whose checked hello failed. Mixed generations are unavailable
  rather than degraded. Hosts and gateways roll before clients.
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
- New endpoints, frame kinds, and event types arrive with a capability
  string. Protocol 1 implies the whole section 5 surface at first
  release, and capabilities exist for what came after. The registry:

  | Capability | Covers | Advertised by |
  |---|---|---|
  | `archive` | `POST /v1/sessions/{id}/archive` | hosts |
  | `session_env` | `GET` and `POST /v1/sessions/{id}/env` | hosts |
  | `compaction_usage` | the `compaction_usage_update` event, including its checkpoint entry id | hosts |

  A capability is self-description, never a gate: probing an endpoint
  (404 `unknown_endpoint` vs 2xx) is a valid fallback check. A gateway's
  `hello` advertises nothing, since it cannot answer for hosts that need
  not agree, so a client attempts the route through it and reads the
  refusal.

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

## 6. The gateway

### 6.1 Aggregation

The gateway keeps one **control connection** per enrolled host (its
`/v1/events` stream without attachments) and maintains the merged
directory from it: namespaced ids, merged `list` frames, `unreachable`
marking. Client event streams are **spliced**: for each attached session
the gateway opens the upstream stream with the client's own cursor and
forwards frames with ids rewritten. It holds no session logs and no
cursors of its own. Commands and reads are proxied to the owning host
with method, query, and body carried unread. An unreachable owning host
is 503 `host_unreachable`.

`reset` is emitted on two edges, for exactly the sessions a client
attached: the gateway lost its link to the host, and the link came back.
The gateway never re-opens an upstream attachment itself, since only the
client has a current cursor. While the link is down the host's rows are
marked `unreachable`. A client that attaches sessions on an unreachable
host is **not** refused: those sessions contribute no upstream while
every other host's sessions on that stream are served. A host believed
reachable that does not answer an attach is the ordinary 503. Removing
an enrollment closes the upstream connections, removes the host's
sessions from the merged list, and ends its splices with `reset`. The
client's re-attach is then refused per session with an `error` frame,
its stream and other hosts' sessions untouched.

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
| `POST /v1/hosts` | `{address}`, `<host>:<port>` or an `http(s)://` URL | Enroll a host dynamically. Completes a checked protocol-2 hello before adding or recording it. Answers 200 with the host's row. 409 `already_enrolled` for an address already enrolled, 409 `duplicate_host` for an id another enrollment holds, 409 `unusable_host_id` for an id that cannot namespace sessions. |
| `DELETE /v1/hosts/{id}` | | Withdraw a dynamic enrollment. 204. 404 `unknown_host`, 409 `static_host` for a configured host, which is removed from the file instead. |

Static host addresses come from the config file (`--config <file>`,
default `~/.aj/gateway.toml`, which need not exist). Dynamic enrollments
and the gateway's own id live under `~/.aj/gateway/`. An unreachable or
incompatible hello leaves a configured or remembered enrollment in place
but disconnected: nothing is marked connected, published as reachable,
or routed to until a protocol-2 hello succeeds.

### 6.2 Process supervision

Reference systemd units for `aj serve` and `aj gateway` ship in
`deploy/`, and aj does not install them. 6161 is the control-port
convention, and a host is configured entirely by `--listen`, `--auth`,
`--allow`, `--name` and their environment equivalents.

## 7. Session settings follow the creator

The host supplies the environment (workspace, skills, keys, catalog,
tool availability), but the model, thinking level, thinking display,
speed, and verbosity of a session belong to whoever creates it, per
axis, where the creator stated a value through a CLI flag, an
environment variable, or an entry written in the client's config file. A
built-in fallback is not a preference and does not travel. The create
command sends only stated axes, and stated axes are strict: a value the
host's model cannot serve fails the create with an error naming the
supported values, never a silent clamp or substitution. A stated model
must be servable by the host (present in its catalog, with credentials).
Unstated axes are the host's to default, model-aware: its own configured
default when the chosen model supports it, otherwise a supported value.
After creation the settings command mutates the axes, from any client,
as peers, under the same strictness. Thinking display is an inference
setting because it changes what the provider is asked to emit.

The session environment overlay is separate from inference settings. Any
client can read or edit it after creation through the environment routes
(section 5.6 and 5.7). An edit changes only the session's active branch,
never the host process environment or config files. Keys must be nonempty
and contain neither `=` nor NUL, and values must not contain NUL. Unknown
request fields are refused before mutation. Removal exposes any inherited
process value rather than masking it. The environment read reports only
entries remaining in the overlay.

Remote launch/create environment input is not part of the creation wire
contract. `CreateSessionRequest` and `SessionSettings` carry no environment
map, and a client asked to send one on a remote create refuses rather than
dropping it. Creation-wire support is separate scope.

## 8. Client TUI

### 8.1 Connect mode

`aj connect <url>` runs the TUI against a remote server. Queue state is
fed from `QueueUpdate` frames and the queue read, the task table from
the tasks read (section 5.7), footer settings and the restore notice
from `state` frames.

Session selection asks for one of three things:

- Bare `aj connect <url>` attaches the host's most recently active
  unarchived session, and creates one when the host offers none.
- `aj connect <url> <session-id> [input...]` attaches that session
  whatever its archived bit says. Launch input follows the id.
- `aj connect <url> --new [input...]` creates a session. Under `--new`
  every positional after the url is launch input, so "create this named
  session" is not expressible.

`--tag <label>` names the session a run creates. `--host <id>` names the
host a create through a multi-host gateway is for (section 5.6): an id,
or a prefix only one host answers to, resolved client-side. A value that
fits none or several is refused with the candidates listed before the
terminal is taken over. A run that attaches instead of creating reports
that `--host` and `--tag` had nothing to point at.

The boundary of what works over the wire is explicit. Supported: prompt,
steer, cancel, queue withdraw and clear, settings including model switch
and thinking display, compaction, task kill, the task-output overlay,
tagging, archiving, environment overlay reads and edits, the tree view and
head switching, and session
creation and switching. The prompt recall ring holds this run's own
submissions only. The exit usage banner renders from the client's own
event-derived accounting. Refused, each with a notice naming why: the
session-info overlay and HTML export (host-local files no endpoint
serves), prompt-history search (this client's own store), and the usage
overlay and credential management (this client's credential store). An
unsupported action never silently does nothing.

Connection state (connected, reconnecting, catching up) is surfaced in
the footer.

### 8.2 The sidebar

A persistent sidebar lists the directory's sessions grouped by host,
with glyphs for working, unseen output, and unreachable. Rows carry no
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
- **Every new interaction is an `AjAction`** on the existing keybinding
  system (sidebar toggle and focus, session switching, remote creation,
  tagging, archiving and the reveal, folding a host's rows), with a
  default chord and user overrides. Pointer gestures dispatch the same
  actions.

A host's group header reads the host's name (section 5.1), else its id,
else its address, and the create-flow host selector labels hosts the
same way. Rows show the session's tag where one is set, else an
id-derived label.

## 9. Testing

The core correctness property is **reducer equivalence**: a client that
connects mid-session, or reconnects after drops, converges to the same
durable-derived state as a client with an uninterrupted connection. The
comparison operates on a **canonical form** of `ChatState`: a projection
covering transcript entries (including message ids), sub-agent boxes
(status, conclusion, report), task table, queue state, footer accounting
and usage, and lifecycle, excluding wall-clock fields and
client-relative state. The harness runs a scripted-provider session on
an in-process host, attaches a client through the real HTTP stack over
loopback, and asserts canonical-form equality with a locally-reduced
`ChatState` at quiescent points. The adversarial variant injects seeded
disconnects at random frame boundaries, forcing re-attach with cursors,
and asserts convergence at quiescence.

The fault variant compares the canonical form's **convergent tier**,
which masks exactly the rows no durable entry backs: reliable-transient
frames are not replayable (section 5.4), so a client disconnected across
a notice's window legitimately never has it. A row with a durable origin
is compared in both tiers, whatever its kind, the line is durability,
not row type. The no-fault comparison uses the full form, notices
included, and the fault sweep includes a scripted turn that emits a
notice, so the masking is load-bearing.

Sharp edges, each pinned by a test: attach cut between a tool's
`ToolExecutionEnd` and its durable `MessageEnd`, reconnect while a tool
and a sub-agent are running, attach mid-sub-run, reconnect where zero
durable entries follow the cursor but an open sub concluded in the gap,
head switch refused while busy, stale-epoch frames dropped, task-table
refetch after `caught_up`, queue enqueue visibility on a second client,
slow-client eviction and recovery, settings visibility for a mid-session
joiner, seq non-contiguity, per-session attach refusal while the same
stream's other sessions serve (through a gateway included), every
gateway `reset` edge (host lost, host returned, enrollment withdrawn,
identity replaced) scoped to the attaching client's sessions,
incompatible peers neither recorded nor routed to, the identity gate in
`local` and `tailscale` modes, closed JSON command schemas through every
route, and misspelled commands that neither mint nor clear, queue,
cancel, compact, kill, or dispatch inference.

## 10. Accepted limits and banked work

- A full backfill projects the whole log. Deferring sub-thread
  projection would break the "cursor = applied prefix of one seq space"
  invariant, so the cost is paid once at first attach. A thread-scoped
  backfill is cleanly additive as a capability if a real session hurts.
- `list` frames are cumulative over the whole store (about 60 KB at 400
  sessions). Debouncing bounds the rate, not the size. A row cap or
  delta encoding is the follow-up if a store gets big enough to hurt.
- A resumed sub-agent's report text is bracket-scoped: which child's
  report survives a resume depends on how their log lines interleaved.
  Accepted while it misleads nobody, per-run scoping is the named fix.
- Banked, wanted: previews in connect mode. The directory contract
  forbids content reads per row, so the shape is an on-demand per-session
  preview read for visible rows.
- Banked, wanted: prompt history across hosts, as a capped, user-paced
  history read on the host behind its own endpoint and capability
  string, with the client merging sources and a gateway merging per-host
  reads.
- Banked, wanted: provider usage over a connection. Resolving an expired
  OAuth credential writes it back and the reset consumes a credit, so
  the host owns both ends: a user-paced usage read plus a separate reset
  action, each behind its own endpoint and capability string. Credential
  management is deliberately not banked: writing a host's credentials
  from a remote client is its own question.
