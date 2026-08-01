# Remote control: implementation manual

This is the working manual for the agent implementing
`docs/remote-control-spec.md`. Read the spec first, in full. This
document adds what the spec deliberately leaves out: where things live
in the code today, the order of work, and the rules of engagement.

## Rules of engagement

- **Tests first, per phase.** Before writing implementation code for a
  phase, write the tests that will prove the phase works (they fail or
  don't compile at first, that is the point). The spec's section 11
  names the test layers and the named sharp-edge cases, this manual
  maps them to phases. It is fine to adjust tests as understanding
  improves, it is not fine to write the implementation first and
  backfill tests that mirror it.
- **The spec is the contract, but not scripture.** When reality
  disagrees with the spec (an assumption about existing code is wrong,
  a protocol detail doesn't survive contact), do not silently deviate.
  Update the spec in the same commit and call the change out
  prominently in your report, so it can be reviewed as a design
  decision, not discovered as drift.
- One phase per commit series, green before moving on: `cargo fmt`,
  `cargo check`, `cargo clippy --workspace --all-targets`,
  `cargo test`. Follow the commit conventions in CLAUDE.md.
- Follow the repo's code style rules (CLAUDE.md): import grouping,
  error-handling boundaries (typed errors at library seams, no
  `anyhow` in library signatures), comment discipline.
- Do not refactor beyond what the phase needs. Flag tempting cleanups
  in your report instead.

## Map of the existing code

Verified starting points (line references drift, re-check):

- `src/aj-agent/src/events.rs` — `AgentEvent`, internally tagged
  snake_case serde, shape-pinning tests at the bottom. The custom
  `Arc<[UserContent]>` serializer is here (`serialize_content_arc`).
- `src/aj-agent/src/message.rs` — `AgentMessage`, whose `id` is
  `#[serde(skip)]`. The log adopts message ids as entry ids and
  resume backfills them. The wire codec must do the same backfill
  from the frame's `entry_id` (spec 6.3), otherwise branching breaks
  for remote clients (the reducer stores `message_id` on entries and
  branch-target resolution matches on it).
- `src/aj-agent/src/bus.rs` — `EventBus`, inline-awaited listeners.
  `Agent::subscribe_channel` (`lib.rs`) is the channel form. Network
  fan-out must use the channel form, never an inline listener (an
  inline listener's stall or error becomes a fatal turn error).
- `src/aj-agent/src/queue.rs` — `MessageQueues`. Enqueue does not
  emit events today, only the agent's drain emits `QueueUpdate`. The
  host layer adds enqueue-side emission (spec section 5).
- `src/aj-agent/src/lib.rs` — `Agent`, `TaskRegistry` (its
  `TaskSummary` holds `Instant`, the wire model needs wall-clock),
  `SubAgentRegistry`.
- `src/aj-session/src/log.rs` — `ConversationLog`, append-only entry
  tree, `linearize`, heads, and the insertion-order `order` vec (the
  natural seq source). Note: non-punctuation entries (settings, spawn
  roots) buffer in `pending_writes` until the next punctuating
  append, and resume truncates a torn tail. This is why epochs are
  per-materialization and never persisted (spec 6.5).
- **Seq assignment**: the fan-out is a channel consumer, so it sees
  events later than the inline persistence listener and must not read
  log length at consume time (races concurrent sub-agent appends).
  Capture the append index at the append site and carry it to the
  fan-out. The workable mapping is `MessageEnd` to its entry via the
  in-memory `AgentMessage` id plus listener registration order
  (persistence subscribes before fan-out). If a cleaner seam exists
  (e.g. the persistence listener reporting appends to the host),
  prefer it, the constraint is spec section 5.
- **The attach path**: the log lives behind an async mutex and full
  backfills are expensive. Do not hold the log lock while projecting,
  that stalls the session's next append for the whole projection. The
  workable shape: under the lock, snapshot what projection needs and
  note `last_seq`, register the subscriber, then project outside the
  lock and filter live durable frames at or below `last_seq` for that
  stream (spec section 5's backfill-boundary rule).
- `src/aj-session/src/replay.rs` — `replay`,
  `replay_deferring_subs`, `project_thread`. Backfill is a
  suffix-filtered projection with the live-log deviations of spec 6.5
  (no force-closing of open sub brackets, re-synthesized
  `SubAgentStart` at the cursor boundary). Study `close_open_sub` and
  the fallback-`SubAgentStart` re-open path first, both are dead-log
  heuristics that must not leak into live backfill unchanged.
- `src/aj-session/src/listener.rs` — `persistence_listener`. Durable
  events are the ones this listener acts on, plus the settings and
  compaction projections that the host synthesizes live (no bus event
  exists for settings changes today, they are applied in
  `src/aj-app/src/settings.rs` which writes log entries directly).
- `src/aj-app/src/session.rs`, `session_setup.rs` — `SessionCore`
  and its composition. `src/aj-app/src/turn.rs` — `TurnStart`,
  `drive_turn`, `Turns`. These are the ingredients the host layer
  (spec section 5) composes per session.
- `src/aj-app/src/chat.rs` + `chat/` — `ChatState` and `reduce`.
  Phase 1 hardens this: idempotent application by durable identity
  (tool cells by `call_id`, sub boxes by agent id), the re-attach
  quiesce operation, epoch/cursor filtering (wherever the client-side
  fold lives), and the canonical form for test comparison (spec 11.2,
  note `ChatState` has no `PartialEq` and stores `Instant`s, the
  canonical form is the comparable projection). Queue state and the
  task table need client-model paths that do not exist today: locally
  the view re-reads live handles at draw time (`aj/src/pending.rs`),
  remotely the model must trust `QueueUpdate` frames and accept task
  table replacement from the tasks read.
- `src/aj-app/src/cli/args.rs` — CLI args including `--scripted`.
  The scripted provider itself is `aj_models::scripted`
  (`src/aj-models/src/scripted.rs`), `src/aj-app/src/scripted.rs` is
  the resolver.
- `src/aj-app/src/print.rs` — print mode's `json_event_listener` and
  replay-then-live streaming, the closest existing relative of the
  event stream server.
- `src/aj/src/interactive.rs` — the drive loop (`run`, `drive`,
  `handle_submit`, `handle_steer`, `cancel_viewed_turn`,
  session-switch and branch handling, including the busy refusals the
  host must mirror). Phase 1 reroutes this through the host layer. It
  is a very large file, read the drive loop and the handlers before
  touching anything.
- `src/aj/src/session_selector.rs` — renders a session list already,
  reference for the sidebar's data needs.
- `scripts/check-no-tui-dep.sh` — the CI rule keeping `aj-app` free
  of vaxis. `aj-app` must also stay free of HTTP dependencies, wire
  types go in the new `aj-wire` crate.

Ember integration (phase 4): ember is driven via its CLI,
`ember vm cp` (fork), `ember vm inspect --format json`
(`network.guest_ip`), `ember cp`, `ember exec`, `ember vm rm`. Root
required on Linux. No readiness signal: poll `/v1/hello`. Mutating
commands print human text, use exit codes plus a follow-up `inspect`.
Ember is a separate project, re-verify all of this against the
installed ember before building on it.

## Phase order and per-phase instructions

### Phase 0: `aj-wire`

Tests first: pinned JSON fixtures for every `AgentEvent` variant
(reuse/extend the existing shape tests as the source of truth),
round-trip identity tests, forward-compat fixtures (unknown event
`type`, unknown frame `kind`, extra fields) that must decode, and
entry-id backfill into decoded message ids.

Then: the crate with frame types (spec 6.3), wire models for list
entries / task summaries / tree / hello / queue / VM state, and
`Deserialize` for `AgentEvent`. Handle the two known warts
deliberately: custom deserializers for the `Arc<[UserContent]>`
fields, and the message-id backfill. The unknown-tolerant decode path
is a requirement, not an option.

Acceptance: workspace green, no behavior change, fixtures committed.

### Phase 1: the session host and reducer hardening

Tests first, two groups. Host: `aj-app` integration tests over the
scripted provider covering session creation, concurrent turns, fan-out
tagging, seq assignment against the log (including concurrent
sub-agent appends), epoch stability under appends and epoch change on
head switch, head-switch busy refusal and queue clearing, session
locks, queue-mutation emissions, synthesized settings frames, command
semantics (prompt-or-queue, steer, cancel cascade, agent-scoped
targets). Reducer: the hardening units of spec 11.3 (idempotent
re-application, quiesce, epoch filter, cursor invariant) plus the
canonical form itself.

Then: the host layer per spec section 5, the reducer hardening, and
the TUI reroute. Keep the reroute mechanical: the drive loop keeps
rendering and input, world mutations go through host commands, the
event arm consumes the host's tagged stream for the focused session.
Expect hidden one-session-per-process assumptions to surface (the
branch-switch-as-world-rebuild path is a known one), fix them at the
root, and note each in your report.

Acceptance: all pre-existing tests pass, new host and reducer tests
pass, the interactive TUI behaves identically in manual smoke use.

### Phase 2: single-session remote

Tests first: the reducer-equivalence harness (spec 11.2). Build it as
a reusable test facility, it is the backbone for everything after:
in-process host, real HTTP over loopback, client fold, canonical-form
comparison at quiescent points, then the seeded fault-injection
variant (disconnect at random frame boundaries, re-attach with
cursors, assert convergence). Cover the named sharp-edge cases from
spec section 11 that apply to a single session (attach cut between
tool end and its durable message, reconnect with a running tool and
sub, mid-sub-run attach, zero-suffix reconnect with an open sub
concluded in the gap, stale-epoch drops, task refetch after
`caught_up`, settings visibility for a mid-session joiner, seq
non-contiguity).

Then: the HTTP server over the host layer, the HTTP client, `aj
serve`, `--listen` (flag and `AJ_LISTEN` environment variable),
`aj connect` (single focused session), hello and capability handshake,
heartbeats, bounded per-client queues with coalescing and eviction.

Acceptance: equivalence harness green including fault injection, a
human can run `aj serve` in one terminal and `aj connect` in another
and work normally, including killing and restarting the client
mid-turn.

### Phase 3: multiplexing

Tests first: multi-session and gateway tests (spec 11.4, 11.5),
including list debouncing, lock conflicts, gateway `reset` emission on
host flap with incremental-vs-full resume, queue enqueue visibility on
a second client, and slow-client eviction with recovery. Sidebar tests
with the TUI test support.

Then: unified stream fan-out for many sessions, the sidebar and
per-session `ChatState` switching, session creation over the wire,
`aj gateway` with static host config and the `/v1/hosts` enrollment
endpoints (with persisted enrollment state), id namespacing, control
connections and splice forwarding, `unreachable` surfacing.

Acceptance: gateway tests green, manual: two `aj serve` hosts, one
gateway, one client, switch between sessions on both hosts with
correct glyphs and instant switching.

### Phase 4: provisioning

Tests first: the local-process backend cycle test (provision with
bundle → enroll → create session → scripted prompt → destroy) running
in CI, plus bundle assembly/unpack round-trip tests. The ember backend
test is `#[ignore]`-style, manual, root+KVM.

Then: the backend trait, the local-process backend, the `/v1/vms`
endpoints and `vms` frames, the profile bundle (client-side assembly,
wire carriage on the provision request, VM-side unpack per the fixed
layout of spec section 8), the ember backend, reference systemd units,
and a setup document for building the golden image.

Acceptance: CI cycle test green, manual ember run on the target
machine documented in the report (what was run, what happened).

## Reporting

At the end of each phase, produce a short report: what landed, test
inventory, any spec deviations (with the spec already updated), any
surfaced assumptions or debt worth a follow-up. Findings that imply a
design or scope change stop the work and go back for discussion, they
are not decided unilaterally.
