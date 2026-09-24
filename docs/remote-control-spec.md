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
  or command re-materializes the session from disk, recovering the saved
  prefix even if the first write left an empty or incomplete log. Creation
  settings that never reached disk cannot be recovered. If saving failed
  before opening the log, the message directs the user to start a new
  session. An id with no log answers `unknown_session`. Fatal turn failures
  that do not compromise persistence stay scoped to the turn.

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

- `event`: `{kind, session, epoch, seq?, entry_id?, branch_settings?, event}` where
  `event` is a serialized `AgentEvent`. `seq` and `entry_id` are present
  if and only if the event is durable (section 5.4). The envelope's
  semantics apply whether or not the nested event type is known to the
  receiver. Durable main-thread user-message ends carry `branch_settings`:
  the recorded model (`{api, name}`), thinking, speed, verbosity, their independent
  `oracle_model`, `oracle_thinking`, `oracle_speed`, and `oracle_verbosity`
  counterparts, and provider account pins at that message's parent.
  Unrecorded inference axes are omitted,
  distinct from explicit `off` or `default`. An absent provider pin follows the
  provider's current default. This is historical context, not a prediction of
  the host's runtime fallback or validation. It contains no environment values,
  credentials, or live-only thinking display. Live delivery and backfill supply
  the same context, including across compaction. Older hosts may omit the field.
- `state`: `{kind, session, epoch, working, settings, oracle_settings?, last_seq}`.
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
  `last_seq` is the durable high-water mark. Sent at the start of every
  attach block, before the backfill, and whenever `working` or
  `settings` or `oracle_settings` changes, never for `last_seq` alone. The host publishes no
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
  compaction path appends, and the notices or `SubAgentSettings` the projection derives from
  notice-producing state entries. Durable frames carry `seq` (the
  entry's 1-based append position, so `0` reads as nothing durable yet)
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
acceptance. An optional JSON body `{incomplete: <message>}` reports a change
that applied but whose requested save or log record failed. An empty body or
absent `incomplete` means complete acceptance. The caller can report partial
success without correlating an event-stream warning with its request.
`POST /v1/sessions`
returns 200 with the new session's id. Commands that act on "the viewed
agent" locally take an optional `agent` field (default: the main agent).

| Command | Body | Semantics |
|---|---|---|
| `POST /v1/sessions` | `{host?, settings?, prompt?, tag?, env?}` | Create a session in the host's working directory. `settings` and the initial string-to-string `env` overlay per section 7. `prompt` is `{text}` or `{content: [...]}`. Settings, prompt, tag and env are validated before a log exists, so a refusal leaves nothing behind. The tag and first prompt are applied afterwards under the session's own lock and are best-effort: one that does not land still answers 200 `{id}` plus an `incomplete` string carrying the host's words for what did not stick, so the client retags rather than creating a second session. A first prompt whose persistence fails does so on the stream, as `persistence_failed`, after the id was answered. A minted session is never deleted to make an error tidier. |
| `.../{id}/prompt` | `{text}` or `{content}`, optional `agent` | Run a turn if the agent is idle, queue a follow-up if busy. |
| `.../{id}/steer` | `{text, agent?}` | Queue steering, or promote the pending follow-up when `text` is empty. |
| `.../{id}/cancel` | `{agent?}` | Cancel the targeted agent through the mechanism that owns its run: its driven turn, a detached sub-agent's background task, or the foreground-sub-agent-cancels-main cascade. A running mark with no owning turn or task is 409 `conflict`. An idle or completed target is accepted. |
| `.../{id}/queue` | `{op: "remove", agent?}` or `{op: "clear"}` | Withdraw one agent's pending message, or clear the session's queues. A withdrawal answers 200 `{text?}` with the text it took, which is what makes the dequeue-into-the-editor gesture work. One agent holds at most one coalesced pending message, so there is no index. A clear answers 202. |
| `.../{id}/compact` | `{instructions?}` | Manual compaction. |
| `.../{id}/settings` | exactly one of `model`, `thinking`, `thinking_display`, `speed`, `verbosity`, `oracle_model`, `oracle_thinking`, `oracle_speed`, `oracle_verbosity`, optional `agent`, `persist` | Host applies, logs, and publishes the synthesized frames. Naming zero or two axes is 400. `account` is rejected, including null, and must use the account route. `persist` defaults to `none`, or is `user`, `project_set`, or `project_clear`, and also writes the value into that host config layer. Oracle axes reject sub-agent targets. Persistence is main-agent only. Capability `host_config`. |
| `.../{id}/account` | `{provider, account?}` | Select an account for this session and provider from the host-local auth store. Missing or null `account` resets to Provider default, `""` pins the unnamed account, any other string pins that exact label. Does not change the provider's auth-store default. Read the result through `accounts`. Capability `session_accounts` (section 5.10). |
| `.../{id}/env` | `{key, value}` | Set one session environment value, including the empty string. Null or an absent `value` removes the key from the map, not from the inherited process environment. Idle-only: 409 `conflict` while a turn or background task is live. Validates before mutation and persists the full resulting active-branch map. Capability `session_env` (section 5.10). |
| `.../{id}/tag` | `{tag}`, empty or absent clears | Set the session's tag (section 5.8): one trimmed line, length-capped. Materializes like any command so the session lock covers the sidecar write. |
| `.../{id}/archive` | `{archived: bool}`, absent reads false | Set or clear the archived bit (section 5.8). Materializes like any command, so a rival's lock refuses it. Nothing else refuses it: a session working through a turn takes it and goes on working. Capability `archive` (section 5.10). |
| `.../{id}/head` | `{entry, changes?}` or `{before: <entry_id>, changes?}` | Switch the session head. 409 `conflict` while working or tasks live. Clears queues, new epoch, `reset` frame. `before` resolves the named entry to its parent server-side, atomically with the switch. An unknown entry is 404 `unknown_entry`, an entry with no parent is refused. Exactly one target. Optional `changes` applies session-scoped overrides to its inherited baseline (see below). |
| `.../{id}/tasks/{task_id}/kill` | `{}` (absent or blank is equivalent) | Kill a background task. Any field or non-object value is refused before the task is touched. |

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

Capability `branch_settings` covers head overrides. Empty
`changes` is omitted on the wire so ordinary head requests remain compatible.
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
Capability `host_config` covers the second: `GET` and `POST
/v1/sessions/{id}/config`, plus `persist` on the settings command for the
common "apply and save" gesture. The session id routes to the owning host
and must name a session in its store. Reading does not materialize it.

A read returns `HostConfig`: the `user` layer and the `effective` merge as
schema-keyed string maps in the editor's vocabulary (`<unset>` for absent
optionals, comma-separated name lists), `project_keys`, `has_project`, and
the host's `models`, `tools`, and `skills` catalogs. Nothing from the auth
store, environment, or arbitrary config-file keys. Connections are trusted,
so complete model and catalog URLs round-trip without masking. The
standalone model and thinking selectors read the same model catalog through
`GET /v1/sessions/{id}/models`, a model array under `host_config`. This read
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

Capability `host_skills` adds `GET /v1/sessions/{id}/skills`, returning
discovered `{name, description, path, enabled, disable_model_invocation}`
metadata, and `POST /v1/sessions/{id}/skills` with `{name, disable}`.
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
- `GET /v1/sessions/{id}/tasks`: `{tasks: [{id, owner, call_id, kind,
  label, status, started_at}]}`, the background task table with
  wall-clock timestamps. Clients replace their task table with this
  after `caught_up`, and ignore `TaskOutput` for unknown task ids.
- `GET /v1/sessions/{id}/tasks/{task_id}`: `{id, status, stdout_tail,
  stderr_tail, stdout_total_bytes, stderr_total_bytes, report?}`. Rolling
  tails and an optional agent report, not the complete output.
- `GET /v1/sessions/{id}/tasks/{task_id}/output?offset=N`: `TaskOutput` in
  `aj-wire`, `{id, status, offset, total_bytes, bytes}`. Reads the retained
  task's full interleaved spill output at the required unsigned byte offset.
  `bytes` is a JSON byte array preserving invalid UTF-8 and split code points,
  capped at `TASK_OUTPUT_CHUNK_BYTES` (65536). `total_bytes` is the file length
  captured before reading, and a read never crosses that captured length.
  Advance by `bytes.length` to continue. At that length the result is empty,
  and a running task may append afterward. Status is sampled before the file
  read, so it can lag completion. An offset beyond the captured length is
  400 `invalid_request`. Unknown tasks, including cold-session tasks, are
  404 `unknown_task`. Missing or unreadable spill output is 409 `unsupported`
  with a clear explanation, never a tail substituted for full output.
  File paths come only from the session task registry, not the request.
  Reads use bounded allocation and blocking-pool file I/O. Running and completed
  tasks are readable while retained by the live registry, with no persistent
  task archive. Capability `task_output` (section 5.10).
- `GET /v1/sessions/{id}/queue`: `{queues: [{agent_id, steering,
  follow_up}]}`, the pending messages per agent.
- `GET /v1/sessions/{id}/tree`: `{segments, head?}`, the
  segment-collapsed branch tree for the tree view and head switching.
  `head` is the current head entry id, absent only while the log has no
  head, and not derivable from the segments.
- `GET /v1/sessions/{id}/info`: the session's aggregate log facts, encoded as
  `SessionInfo` in `aj-wire`: identity and host-local file path, timestamps,
  file size, message and tool counts, usage and its provider/model/account
  breakdown, compaction usage, recorded settings, and session environment.
  Counts and usage span all threads and branches. Settings and environment
  reflect the active user branch. Environment values are unredacted, with
  `null` meaning unrecorded and `{}` meaning recorded empty. The response's
  `session_id` is the host-local log identity, not a gateway routing id.
  Clients render the facts and fetch only when the info overlay opens.
  Capability `session_info` (section 5.10).
- `GET /v1/sessions/{id}/export`: `SessionExport` in `aj-wire`, `{html}`.
  The host renders the complete log with the ordinary HTML exporter, including
  all threads and branches and its export redaction and tool-detail resolution.
  Rendering runs on the blocking pool, not the serving loop. The endpoint writes
  no host export file. Clients save the returned document to
  `~/.aj/exports/aj-session-<id>.html` on the client, using the requested session
  id (including a gateway prefix), and report that local path in the notice.
  Capability `session_export` (section 5.10). Clients attempt the endpoint
  without capability gating and explain when the host does not support it.
- `GET /v1/sessions/{id}/usage`: host provider-account plan usage, not the
  session's token totals. Returns `ProviderUsageReport` in `aj-wire`, with
  `statuses` sorted by provider and exact account label, and `reset_providers`
  naming providers with a configured reset adapter. Each status contains
  `provider_id`, `provider_name`, nullable `account`, and `outcome`: `Usage`
  (windows, notes, optional reset credits), `Unsupported` (reason),
  `NotConfigured`, `NoSource`, or `Error` (message). Enums use serde's externally
  tagged representation.
  `provider_name` is the host display label for the credential kind: the
  registered OAuth name for subscriptions, or the friendly API-provider name
  for API keys. Unknown providers fall back to their ID. This additive field
  defaults to an empty string when absent. Clients display `provider_id` when
  it is empty, without inferring a name from human-readable status text.
  Naming alone never refreshes a token.
  A runtime credential override collapses its provider to one unlabeled row.
  The host reads credentials and performs any OAuth refresh and writeback.
  Capability `provider_usage` (section 5.10).
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
  the ordinary error envelope. Capability `provider_usage_reset` (section 5.10).
  Both endpoints validate the session before touching host credentials and do
  not materialize it. The session address selects exactly one host through the
  gateway's wildcard route, with no gateway-wide aggregation. No provider keys,
  bearer tokens, or credential contents travel in either request or report.
- `GET /v1/previews?session=<id>` (repeatable): the session browser's
  `SessionPreviews` in `aj-wire`, `{previews, incomplete}`. Each preview
  carries the full first user text block, message count, creation and
  last-message timestamps, file modification time and size, tag, and archive
  bit, describing the whole log rather than the active branch. The read uses
  the ordinary preview scanner without materializing any session. An id the
  host does not have, cannot read, or could never hold is left out rather than
  refusing the batch. A gateway splits a batch by owning host, reads each host
  once, returns previews under the ids the client asked with, and names a host
  it could not read once in `incomplete` `[{host, message}]`, keeping the other
  hosts' rows. Owning hosts are read concurrently within one batch deadline,
  so stalled hosts do not delay healthy reads behind timeout waves.
  Capability `session_previews` (section 5.10).
- `GET /v1/sessions/{id}/prompt-history`: submitted prompts from the focused
  session's workspace on its owning host. A gateway forwards this read to that
  host, not to the client's workspace.
- `GET /v1/prompt-history`: submitted prompts from every workspace in the host's
  sessions store. A gateway reads adopted, connected hosts concurrently and merges
  their replies. Other enrolled hosts are named partial failures without receiving
  a history request. Both history endpoints return `PromptHistory` in `aj-wire`:
  `{prompts: [{text, project, timestamp}], incomplete: [{host, message}]}`.
  Text is the full trimmed prompt, joining user text blocks with newlines.
  Only top-level user-thread messages contribute, across all branches and
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
  their state. Capability `prompt_history` (section 5.10).
- `GET /v1/sessions/{id}/prompt-history/stream` and
  `GET /v1/prompt-history/stream`: finite SSE reads with the same scope and
  ranking as the JSON resources. Named `snapshot` events carry complete
  replacement `PromptHistory` values, coalesced as files finish scanning.
  A terminal `complete` event carries the final value, even when empty or
  unchanged. A terminal `error` event carries `{code, message}` and leaves
  earlier results available. EOF without either terminal event is a failed
  read. There is no reconnect or replay. Dropping the body cancels the read.
  Gateways forward Workspace bodies without buffering. All-scope reads merge
  each host's latest snapshot as it arrives, ordered by host label and identity
  to break equal-time ties independently of arrival order. Slow hosts do not
  block healthy results. Failed or timed-out hosts retain their last available
  prompts and contribute a named failure. Capability `prompt_history_stream`.
- `GET /v1/sessions/{id}/env`: a JSON object mapping strings to strings,
  the full environment map selected by the active branch. Values are
  unredacted on the trusted control port. Export-only redaction does not
  apply. This reads the session overlay, not the host process environment.
  Capability `session_env` (section 5.10).
- `GET /v1/sessions/{id}/env/before/{entry}`: the same map at the named entry's
  parent, with the same target validation as `head.before`. An unknown entry is
  404 `unknown_entry`, and a parentless entry or invalid parent head is refused.
  This read can materialize a session and is available while work is live. It
  changes neither the head nor runtime state. The entry is one URL-encoded path
  segment. A host without this resource returns `unknown_endpoint`, rather than
  ignoring a target query and returning the live map. Capability
  `transcript_settings` (section 5.10).
- `GET /v1/sessions/{id}/accounts?provider=...`: `{provider, selected,
  default, accounts, override_active, source}` from the host-local auth store.
  `provider` is URL-encoded by clients. Omitting it selects the session's
  current main-model provider, returned in `provider`. `selected` is the session
  pin, or null for Provider default. `default` is the provider's default
  account label, or null when none exists. `accounts` contains exact labels,
  including `""` for the unnamed account. `override_active` reports whether a
  credential override is active, and `source` describes the credential source
  without exposing a secret. No keys, tokens, or credential contents travel.
  Capability `session_accounts` (section 5.10).

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
  the generation whose servers ignore unknown request fields. Protocol 2
  requires strict commands. Protocol 3 also combines committed compaction and
  its optional usage in one durable `compaction_end` event. Removing the
  separate usage event changes semantics, so this requires a protocol bump
  rather than additive decoding. The exact version check is the boundary:
  a protocol-3 client sends no create or command after a mismatched hello,
  and a gateway opens no link to a host whose checked hello failed. Mixed generations are
  unavailable rather than degraded. Hosts and gateways roll before clients.
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
  | `branch_settings` | `head.changes` | hosts |
  | `transcript_settings` | user-message `branch_settings` and `GET /v1/sessions/{id}/env/before/{entry}` | hosts |
  | `session_env` | `GET` and `POST /v1/sessions/{id}/env` | hosts |
  | `session_export` | `GET /v1/sessions/{id}/export` | hosts |
  | `task_output` | `GET /v1/sessions/{id}/tasks/{task_id}/output?offset=N` | hosts |
  | `provider_usage` | `GET /v1/sessions/{id}/usage` | hosts |
  | `provider_usage_reset` | `POST /v1/sessions/{id}/usage/reset` | hosts |
  | `session_info` | `GET /v1/sessions/{id}/info` | hosts |
  | `session_previews` | `GET /v1/previews` | hosts and gateways |
  | `prompt_history` | Workspace and All prompt-history reads | hosts and gateways |
  | `prompt_history_stream` | finite SSE Workspace and All prompt-history reads | hosts and gateways |
  | `credentials` | `GET` and `POST /v1/sessions/{id}/credentials` (section 5.12) | hosts |
  | `session_accounts` | `GET /v1/sessions/{id}/accounts`, `POST /v1/sessions/{id}/account`, and creation `settings.account` | hosts |
  | `host_config` | `GET` and `POST /v1/sessions/{id}/config`, `GET /v1/sessions/{id}/models`, and `settings.persist` | hosts |
  | `host_skills` | `GET` and `POST /v1/sessions/{id}/skills` | hosts |
  | `compaction_usage` | optional cumulative `usage` on durable `compaction_end`, identified by the frame's `entry_id` | hosts |

  A capability is self-description, never a gate: probing an endpoint
  (404 `unknown_endpoint` vs 2xx) is a valid fallback check. A gateway's
  `hello` advertises only its own features, not those of hosts that need
  not agree. A client attempts the route through it and reads the refusal.

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
nor takes its writer lock. Capability `credentials`.

- `GET` returns `CredentialOverview`: the host's OAuth providers (id and
  display name), one safe status row per provider account (`provider_id`,
  `provider_name`, optional label, default and configured flags, summary,
  optional detail), and a `stored` map from provider id to `{kind:"bare"}` or
  `{kind:"accounts", default, accounts:[labels...]}`. No keys or tokens are
  returned and the read never refreshes anything. `provider_name` follows the
  usage report naming and empty-string compatibility rules (section 5.7).
  The summary describes
  method/source without repeating the provider name. A runtime override has
  its own API-key row and does not hide stored bare or labeled credentials.
  Rows are sorted by provider ID and account label, with the runtime override
  before stored credentials for that provider.
- `POST` takes one `CredentialMutation`, tagged by `action`:
  `store {provider, target, credentials}`, `logout_bare {provider}`,
  `logout {provider, account_label}`, `set_default {provider,
  account_label}`, `logout_with_new_default {provider, account_label,
  new_default}`, or `logout_all {provider, expected_accounts}`. Unknown
  fields are refused before anything is written. The host applies the store's
  own exact-label, storage-shape and lock-time checks. The answer is tagged by
  `outcome`: `applied`, `removing_default {provider, account_label}` (the
  frontend offers a replacement default or removal of the whole set), or
  `failed {code, message}`.

Login is the client's work up to the last step. The browser is where the user
sits, so the client runs the provider's OAuth flow itself, exactly as a local
run does, and then sends the finished credentials as a `store` mutation. Its
`target` is `{kind:"new", label}` (`null` for a provider's first, bare
credential, a string to add an account) or `{kind:"replace", label}` (`null`
for the bare credential, a string for that exact account). A duplicate label
is refused on the client before the browser opens. Cancelling during
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
| `POST /v1/hosts` | `{address}`, `<host>:<port>` or an `http(s)://` URL | Enroll a host dynamically. Completes a checked protocol-3 hello before adding or recording it. Answers 200 with the host's row. 409 `already_enrolled` for an address already enrolled, 409 `duplicate_host` for an id another enrollment holds, 409 `unusable_host_id` for an id that cannot namespace sessions. |
| `DELETE /v1/hosts/{id}` | | Withdraw a dynamic enrollment. 204. 404 `unknown_host`, 409 `static_host` for a configured host, which is removed from the file instead. |

Static host addresses come from the config file (`--config <file>`,
default `~/.aj/gateway.toml`, which need not exist). Dynamic enrollments
and the gateway's own id live under `~/.aj/gateway/`. An unreachable or
incompatible hello leaves a configured or remembered enrollment in place
but disconnected: nothing is marked connected, published as reachable,
or routed to until a protocol-3 hello succeeds.

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
as peers, with host-owned model resolution. Model edits preserve the current
effort, and the next inference validates the combination. Thinking display is an inference
setting because it changes what the provider is asked to emit.

Oracle is an advisory child with its own model, thinking effort, speed, and
verbosity. Its `oracle_*` settings use the same vocabularies and precedence as
main's settings, with independent built-in, user, and project defaults. An
Oracle model selection requires both `api` and `name` and may include a URL
override. There is no partial model selection or follow-main mode. Oracle
thinking accepts `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`.
For verbosity, `default` means the Oracle provider's default, not main's value.

Oracle calls use the same inputs and delivery modes as the `agent` tool. Calls
block by default. With `run_in_background: true`, the tool returns the task id
immediately and the report arrives as a task completion notice. Background
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
apply to consultations in the next main turn. Main edits never change Oracle's
choices, and a retained child keeps its own bundle. Session selectors change
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

Account selection attempts the host's endpoint and shows the ordinary
unsupported-endpoint notice if the peer lacks it. It does not fall back to
reading or modifying the client's auth store. Credential pickers and the
`/auth` overlay follow the same rule through section 5.12: they read the
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
Unsupported endpoints produce a notice rather than a capability pre-gate.
The up-arrow ring is independent, retaining local bootstrap behavior and only
this run's submissions over a connection.

The usage overlay captures its Control connection and complete focused session
address when opened. It renders that host's reports, offers eligible provider
accounts with nonzero credits and a host reset adapter, confirms the exact
account, and retains its target and idempotency key on retry. Completion offers
a user-paced refresh against the same captured address, even if focus changes.
It attempts both endpoints without capability pre-gating and renders a clear
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
them with previews from their owning hosts, read in small batches in
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
on the same durable-derived conversation as an uninterrupted client. This
includes transcript content and identity, sub-agent status and reports,
tasks, queued messages, settings, usage and lifecycle. Wall-clock timings
and client-local view choices need not agree.

Exercise that promise through the real host and HTTP client, including
seeded disconnects and cursor-based reattachment. Keep independent expected
conversation outcomes so two equally broken clients cannot satisfy a
comparison. Tests should observe stable application and peer boundaries,
not internal indexes or incidental counts of frames and transcript rows.

A disconnected client cannot recover a reliable-transient notice whose only
delivery happened while it was away (section 5.4), or unfinished streaming
text superseded by a durable message. Recovery comparisons exclude those
artifacts, but keep every durable-backed row. Uninterrupted clients must
agree on transient notices too. Fault scenarios must actually interrupt
durable history and include a notice raised while the client is away.

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
