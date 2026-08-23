# Remote control: implementation manual

This is the working manual and prompt for the agent implementing the
remote-control and VM-provisioning feature in this repository. Your
task is to implement `docs/remote-control-spec.md`, phase by phase.

Read, in this order, before writing anything: the spec in full,
`CLAUDE.md` (build commands, code style, commit conventions), and the
"Map of the existing code" below. This document adds what the spec
deliberately leaves out: where things live in the code today, the
order of work, and the rules of engagement.

## The working loop

The spec is the source of truth and the implementation derives from
it, never the other way around. When reality disagrees with the spec,
the loop is:

1. **Trivial factual drift** (a file moved, a function was renamed, a
   line reference is stale): fix the stale reference in the docs in
   the same commit and mention it in your phase report. No pause
   needed.
2. **Anything design-level** (a protocol rule doesn't survive contact
   with the code, an assumption about existing behavior is wrong, a
   phase's approach won't work as described, a test the spec requires
   cannot be written as specified): **stop working on that thread**.
   Write a short report: what you were doing, what the spec says, what
   you found instead, evidence (code, failing test, trace), and one or
   two options if you see them. Then hand back. The spec gets amended
   through review, and you continue implementing against the amended
   spec. Do not silently deviate, and do not redesign unilaterally,
   even when your workaround seems obviously right.

## Rules of engagement

- **Tests first, per phase.** Before writing implementation code for a
  phase, write the tests that will prove the phase works (they fail or
  don't compile at first, that is the point). The spec's section 11
  names the test layers and the named sharp-edge cases, this manual
  maps them to phases. It is fine to adjust tests as understanding
  improves, it is not fine to write the implementation first and
  backfill tests that mirror it.
- **Test quality is a first-class deliverable.** The protocol is only
  trustworthy through the reducer-equivalence harness and the
  sharp-edge cases, treat them as the product. Never weaken an
  assertion, shrink a comparison, or skip a named case to get green.
  If a required test cannot pass, that is a design finding, report it
  (working loop, point 2).
- One phase per commit series, green before moving on: `cargo fmt`,
  `cargo check`, `cargo clippy --workspace --all-targets`,
  `cargo test`. Follow the commit conventions in CLAUDE.md.
- Follow the repo's code style rules (CLAUDE.md): import grouping,
  error-handling boundaries (typed errors at library seams, no
  `anyhow` in library signatures), comment discipline.
- Do not refactor beyond what the phase needs. Flag tempting cleanups
  in your report instead.

## Per-phase review pipeline

A phase is not done when its tests pass, it is done when it has
survived adversarial review. After the phase's work is committed:

1. Run **two adversarial review agents in parallel**. They are
   read-only: they may read, search, build, and run tests, but must
   not edit files (two parallel reviewers editing would clobber each
   other). Each prompt must state the intended behavior, the spec
   sections in scope, and the commit range to attack, and must say:
   assume there are bugs, find them and show how things break. A
   finding needs evidence (a concrete failure scenario, a breaking
   input or frame trace, a failing test), not a vague concern. Give
   the two reviewers different lenses so they don't produce the same
   report, for this project the natural split is one on correctness,
   concurrency, and protocol edge cases (catch-up, epochs, flow
   control, races), the other on spec conformance, interface
   contracts, and test coverage. If the change under review edited
   the spec or the manual, both prompts must name those paragraphs
   as targets: prose written by the same hands as the code inherits
   the code's blind spots, and self-written spec deltas have been
   the falsified paragraphs more than once, whoever held the pen.
   Cross-references in a delta are claims, not decoration: a reviewer
   greps the cited section and confirms it says what the citation
   claims, a phantom citation has already carried a wrong rule past
   two reviewers who deferred to it.
2. Run a **fix pass** that takes both reports, triages them,
   integrates the valid findings, and amends the phase's commits.
   Adversarial review produces false positives, rejecting a finding
   with a short justification is expected, applying everything
   blindly is not. Findings that imply a design or scope change are
   not fixed in the pipeline, they go through the working loop
   (point 2) instead.
3. If the reviewers found serious issues, run one more review pass
   over the fixes, then stop. Don't loop endlessly.

The phase report (see Reporting) includes the review outcome: findings
accepted, findings rejected and why.

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
  `replay_deferring_subs`, `project_thread`, `project_suffix`. Backfill
  is a suffix-filtered projection with the live-log deviations of spec
  6.5 (a running sub-agent's bracket is not force-closed,
  re-synthesized `SubAgentStart` at the cursor boundary). Study
  `close_finished_runs` and the fallback-`SubAgentStart` re-open path
  first, both are dead-log heuristics that must not leak into live
  backfill unchanged.
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
  task table need client-model paths: the model trusts `QueueUpdate`
  frames and accepts task-table replacement from the tasks read, and
  the views read that model (`aj/src/pending.rs`) rather than live
  handles, so one path serves both modes.
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
strict round-trip identity tests, extra-unknown-fields-ignored and
malformed-known-event-fails cases, wrapper fixtures (unknown event
`type` and unknown frame `kind` decode into the raw-retaining
wrappers and re-serialize unchanged, durable unknown events keep
their envelope for cursor progression), and entry-id backfill into
decoded message ids.

Then: strict `Deserialize` for known `AgentEvent` variants in
`aj-agent` (the enum stays closed, no catch-all variant, spec 6.10),
and in `aj-wire` the frame types (spec 6.3), the known/unknown decode
wrappers for events and frames, and the wire models for list entries
/ task summaries / tree / hello / queue / VM state. Handle the two
known warts deliberately: custom deserializers for the
`Arc<[UserContent]>` fields, and the message-id backfill.

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
non-contiguity, identity-gate accept and reject paths).

Then: the HTTP server over the host layer, the HTTP client, `aj
serve`, `--listen` (flag and `AJ_LISTEN` environment variable),
`aj connect` (single focused session, with the selection rule and the
supported-action matrix of spec 9.1: unsupported actions notice, they
never silently no-op), session creation over the wire, the per-task
read behind the task-output overlay, hello and capability handshake,
heartbeats, bounded per-client queues with coalescing and eviction
(the bound governs live fan-out only, attach blocks are
producer-paced, spec 6.9), and the connection identity gate (spec
6.11, flag/env names and the capability key are pinned there). Put
the gate's peer lookup behind a small trait so tests can fake the
whois resolver, the real implementation queries tailscaled's local
API (the unix socket that `tailscale whois --json` wraps). Session
create sends only stated settings axes per spec section 8: the config
schema's `Option` fields already distinguish written entries from
built-in fallbacks, so statedness must be captured before the
defaulting layer resolves them.

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
per-session `ChatState` switching, the tree view and branching UX
over the wire, session-id validation at the wire boundary (spec 6.2,
required before any single-id lookup may touch a path directly, and
what unblocks `ColdSessions::contains` answering a one-id membership
question without enumerating the store), `aj gateway` with static
host config and the `/v1/hosts` enrollment endpoints (with persisted
enrollment state), id namespacing, control connections and splice
forwarding, `unreachable` surfacing.

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

## Banked polish

Deferred small items live in `docs/banked-ux.md`, each carrying the
ruling that settles it, so a batch round needs no new decisions. Two
larger banked features (remote previews, cross-host prompt history)
are recorded with their design constraints in spec section 13.

## Status: paused before phase 4, daily-driving

Phases 0 through 3 are complete and accepted. Phase 4 (provisioning)
is deferred, not abandoned: the operating model for now is pet VMs
with long-running `aj serve` hosts behind a gateway, driven daily to
sand off rough edges. Phase 4 resumes on call, its section below is
current and its kickoff constraints stand (re-verify ember behavior
against the installed binary, the section 6.11/8 secrets posture as
implementation constraints).

Daily driving is deployment, so the before-deployment items are due
now, in order:

1. In-flight work completes: the 11.2 convergent tier, the
   `aborted_session_resume` flake investigation. Landed: the tier
   merged with its spec prose, and the flake was diagnosed and fixed
   (the TODO note it had planted went with it).
2. The targeted vacuity sweep (below), rescheduled from
   before-deployment to before-daily-driving, same scope.
3. The banked polish batch (`docs/banked-ux.md`).
4. Reference systemd unit and a short setup note for a long-running
   `aj serve` on a pet VM, pulled forward from phase 4's deliverables
   because it is the daily-drive setup. Docs and a unit file only, no
   provisioning code. Landed: `deploy/aj-serve.service`,
   `deploy/aj-gateway.service`, `docs/pet-vm-setup.md`.
5. Security posture before anything listens beyond loopback: hosts
   and gateway run the identity gate per spec 6.11 (`--auth
   tailscale` with an allowlist, or stay loopback behind SSH), and
   the tailnet policy gets drafted against the real tailnet. This
   one is a designer-and-owner task, not an implementer task.

Rough edges found while daily driving follow the standard loop:
reported, ruled, turned into tasks with the working loop and review
pipeline unchanged. The banked features (remote previews, cross-host
prompt history, the render-loop cost, spec section 13) get
re-prioritized by actual use.

The `locked` row bit (spec 6.5, 6.8) is published: a refused acquire
sets it, a won one clears it, and an enumeration point sweeps the lock
directory to re-establish it. Two things it does not have yet. No
client waits on its edge, so a locked refusal still does not rejoin on
its own. And nothing refreshes it between enumeration points, which on
an idle host never come: measured live, a host with a connected client
and nobody typing enumerates nothing at all, so a rival that lets go
stays published as holding the session. What bounds that is still
under design, and the bit is a hint either way, so the escape hatch is
what it always was, attempting the session and reading the answer.

## Before deployment

Between phase 4 landing and deployment, run the targeted vacuity
sweep: most of the suite predates the self-asserting-fixture rule and
is unaudited against the one failure mode passing tests conceal.
Scope by harm, not count: the catch-up/equivalence layer, flow
control and eviction, locks and release, the identity gate and id
validation, the gateway's reset edges, and anything
security-adjacent. Per test, mutation-check the property it names.
The selection question is "if this silently broke, what would it
cost and who would notice".

## Reporting

At the end of each phase, produce a short report: what landed, test
inventory, any doc-drift fixes (working loop, point 1), any surfaced
assumptions or debt worth a follow-up. Design-level findings follow
the working loop, point 2: they stop the affected work and go back for
spec amendment, they are not decided unilaterally.
