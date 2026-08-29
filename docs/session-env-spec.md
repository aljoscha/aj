# Session-scoped environment

## Motivation

A session's tool subshells inherit the host process environment, so
anything identity-like (`BEADS_ACTOR` for the workshop's bench model)
must live in the `aj serve` process, which dedicates the host to one
identity and leaks it across every session the host serves. This spec
gives each session its own environment map, stated at creation,
persisted in the session log, restored on resume and materialize, and
overlaid on tool subshells. Identity rides the session instead of the
process.

Create-only in v1: no mutation surface and no settings-command axis.
A session's env is fixed at birth and outlives host restarts.

## 1. Semantics

The session env is a map of environment variable names to values.

**The identity invariant.** A session's env is exactly what its
create was explicitly given, or what its own log records. It is never
inherited from a host's process environment, base run config, or
config file. This is enforced at one place: env is an explicit
parameter of session creation, not a defaulting axis of
`RunConfigSnapshot` / `base_run_config`. The invariant is what makes
the feature safe on shared hosts: interactive runs embed a server
under `--listen`, and a launch env that rode the host's base config
would stamp the launcher's identity onto sessions remote peers
create.

**Log scope.** Session env is immutable log-level creation metadata,
not user-thread or branch state. Selecting, reconstructing, or
switching a conversation head cannot select, replace, or clear it.
Every branch of an env-bearing session executes with the same map,
including one rooted directly at the system prompt before any
inference-setting entry. In v1 only creation records the map.

Consequences, each ruled below:

- A parked session re-materialized after the bench changed occupants
  keeps *its* recorded env, never the new occupant's.
- A resume of a session whose log records no env runs with no env,
  even when the resuming invocation stated `--env`. Stamping the
  resumer's identity onto an old session is the misattribution this
  feature removes. Such a run says so instead of silently dropping
  the flag (section 5).
- `aj serve` and `aj gateway` refuse `--env` at startup: by the
  invariant there is no path from a serve process to a session's env,
  so accepting the flag would be a lie in the grammar.
- Per-session env never appears in `config.toml` (brief non-goal): a
  config default would stamp one identity onto every session, the
  process-env failure mode with extra steps.

**Layering.** In a tool subshell, the session env is applied above
the inherited host process environment (which already includes the
`~/.aj/.env` and project `.env` loads, `src/aj/src/main.rs:104-111`)
and below the tool's fixed determinism overrides (`TERM=dumb`,
`NO_COLOR=1`, `AGENT=aj`, ..., `bash.rs`). A session env entry
shadows a host process variable of the same name. A session env entry
named like a fixed override loses to it inside tool subshells, by
design and without a refusal: the overrides exist so captured output
stays parseable, and that is not the creator's to break. No key is
reserved or rejected on collision grounds.

**Extent.** The overlay applies to tool subshells: the bash tool's
child, foreground and background alike (one build site serves both,
`BashTool::execute` builds and spawns before the background branch),
and the same subshells run by sub-agents. Host-side helper processes
(the `rtk` formatter, `pgrep` liveness probes) are not tool subshells
and keep plain process inheritance. The frozen system prompt's
`<env>` block (`AgentEnv`, workspace context) is unrelated to this
map and unchanged, and the name collision is noted here so nobody
wires one into the other.

**Validation.** Syntactic only, at the boundary that accepts the map
(host create handler, CLI flag parse): a key must be non-empty and
contain neither `=` nor NUL, a value must not contain NUL. A create
carrying an invalid map is refused before a log exists with an error
naming the offending key, per the existing rule that a refused create
leaves nothing behind. Values are otherwise arbitrary, keys are
case-sensitive, and an empty map is valid (applied trivially,
recorded, echoed).

## 2. Wire

### 2.1 Create

`CreateSessionRequest` gains a top-level field, sibling of `tag` and
`prompt`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub env: Option<BTreeMap<String, String>>,
```

Env is a property of the creation, not an inference setting, so it
deliberately does not join the wire `SessionSettings`: that struct is
`#[serde(flatten)]`-embedded in `SettingsRequest`, the runtime
mutation route, and placing env there would either open the mutation
surface v1 excludes or force refusal logic into the settings path.
Top-level placement makes create-only true by construction. A
settings request that carries an `env` key anyway is an unknown field
to every host and is ignored under 6.10 like any other, the standing
unknown-field posture rather than a new hole, and no built-in client
offers such a surface.

`BTreeMap` for deterministic serialization. The host applies the map
in full or refuses the create (section 1 validation), never a partial
apply.

### 2.2 The answer says what happened

Servers ignore unknown request fields (spec 6.10), so a host that
predates this feature mints the session *without* env and cannot say
so. An identity that fails silently is the exact harm this feature
exists to end, so the create's answer must carry the fact.

`SessionCreated` gains:

```rust
/// The env keys the create applied, present exactly when the request
/// stated an env map (an empty stated map echoes as an empty list).
/// Absent when the request stated none, and absent from hosts that
/// predate session env, which is what lets a creator distinguish
/// "applied" from "silently dropped by an old host".
#[serde(default, skip_serializing_if = "Option::is_none")]
pub env_keys: Option<Vec<String>>,
```

Keys only: the creator already knows the values, and the echo's job
is the fact of application, legible in operator output. Sorted (map
order).

The decode table for a creator that stated env:

| Answer | Meaning |
| --- | --- |
| 200, `env_keys` present | The host applied the map (all of it). |
| 200, `env_keys` absent | The host predates session env. The session exists without identity. |
| 400 naming a key | A knowing host refused an invalid map. Nothing was minted. |

Client rule (strictness across version skew, extending spec section
8's stated-axes-are-strict posture to the one case the host cannot
enforce): a built-in client that stated env and receives an answer
without `env_keys` reports it loudly. The TUI renders a prominent
notice naming the host as predating session env. Unattended callers
(the workshop's wake script) treat it as failure and name the minted,
identityless session they are abandoning. A minted session is never
deleted to make the error tidier, per the existing create contract.

This is attempt-and-read, the doctrinal fallback: no pre-gating, no
hello round-trip in the create path, and it works identically through
gateways, where the client only ever sees the gateway's hello.

### 2.3 Capability string

Hosts advertise `session_env` in `GET /v1/hello` capabilities,
extending the 6.10 registry (new surface past the baseline). Gateways
do not advertise it, a gateway cannot answer for its hosts (6.10). It
is self-description, never a gate, and aj's own client code does not
consult it: the `env_keys` echo is the normative signal. It exists so
operator tooling can ask "is this bench binary new enough" without
minting a session, which the workshop's cutover check wants.

### 2.4 Gateway

Nothing to build. The gateway's create route already edits the body
as a `RawObject`, reading `host` and carrying every other field
unread upstream, and namespaces only `id` on the way back (spec 6.10,
`gateway/server.rs::create_session`). `env` rides through raw, and
`env_keys` rides back inside the answer the gateway does not decode.
This holds for older gateways by the same contract. The work is one
pinning test: a create with env through the gateway reaches the host
with the map intact and the echo intact on the way back.

### 2.5 Spec doc amendments

`docs/remote-control-spec.md` is amended with the range that lands
the wire change: the 6.6 create row gains env among the optional
creation properties with the echo contract, and the 6.10 capability
registry gains `session_env`.

## 3. On-disk format (`aj-session`)

One new variant on `ConversationEntryKind`, beside the four settings
variants:

```rust
/// The complete environment fixed for this session at creation.
/// V1 writes it once as log-level creation metadata when creation
/// stated a map. An empty map is distinct from no record.
EnvChange { env: BTreeMap<String, String> },
```

Serialized as `"type": "env_change"` under the existing tagged
scheme. The creation record is `ThreadKind::Meta`, has no `agent_id`,
and names the system-prompt root as its parent. It is never a legal
conversation head. Non-punctuation, like the settings seeds: a
created session that never receives a message leaves no file.

**Transactional first publication.** A new log does not expose its
target file one line at a time. On the first punctuation append,
`ConversationLog` assembles the pending system prompt, optional
`EnvChange`, inference-setting seeds, and punctuation into one complete
initial image at a same-directory staging path outside the session-log
namespace. After the complete image is written and flushed, it is
atomically installed at the canonical session path without replacing
an existing file.

Pending records are not drained until installation succeeds. A
surfaced write, flush, or install error leaves the canonical path absent
or unchanged and keeps the fresh log's complete pending prefix for a
same-object retry. A process failure before install may leave only an
ignored staging artifact. After install the canonical image has a
punctuation record after `EnvChange`. `flush_pending` remains a no-op
before first punctuation. Later appends and the existing power-loss
contract are unchanged.

- **Seed**: `freeze_and_seed` appends one `EnvChange` immediately after
  the system-prompt root and before the user-thread inference-setting
  seeds, exactly when the create stated env. An absent map writes no
  entry, so `None` stays distinct from `Some({})`.
- **Extraction**: `LogSnapshot` and `ConversationLog` expose
  `session_env()`, which reads the log-level creation record
  independently of the active head. `None` means a legacy log or an
  env-less create. `aj-session`'s `SessionSettings` remains
  inference-only, and `Conversation::settings()` cannot state session
  identity because a `Conversation` is head-filtered.
- **Compaction**: cannot affect `session_env()`, because extraction
  uses neither message projection nor a head-linearized conversation.
- **Branching**: existing head targets remain valid, including the
  system-prompt root and inference-setting entries before the first
  message. `head_switch` restores branch-local inference settings but
  neither reapplies nor clears the session env.
- **Replay**: the log-level creation record always projects nothing.
  V1 has no post-message env change to announce.
- **Export**: the `/export` artifact embeds every log entry verbatim
  today, and a self-contained HTML file is built to leave the
  machine. The `ExportEntry` serializer (which already normalizes
  tool-result details) redacts `EnvChange` values, keys kept, each
  value replaced by a fixed `"[redacted]"` marker. The contract:
  an export names the session's env keys and never carries a value.

The log itself stores full values. Restore needs them, and the log
lives on the host's disk in the same trust domain as the transcript,
which already carries whatever secrets pass through tool output.

## 4. Application

- **Carrier**: session creation passes env explicitly
  (`SessionSpec::Create` gains it, `SessionHost::create_with` /
  `mint` take it through the same signatures that carry settings,
  prompt, and tag). `RunConfigSnapshot` stays inference-only, per the
  section 1 invariant.
- **Agent state**: the `Agent`'s session state holds
  `session_env: BTreeMap<String, String>` (empty when none), set at
  `SessionCore::build`: from the create's map on Create, from
  `PreparedLog::session_env` on Resume. Remote materialize goes through
  the same log-level extraction, so parked sessions restore their env
  across host restarts. Head switching restores only branch-local
  inference settings and never mutates session env.
- **Tool seam**: `ToolContext` gains a `session_env()` accessor,
  backed by the session state through `SessionContextWrapper` like
  `working_directory()`. `BashTool::execute` applies it at the single
  child-build site, between process inheritance and the fixed
  overrides. Foreground and background bash are the same spawned
  child, so both are covered at that one site.
- **Sub-agents**: `spawn_agent` copies the parent's session env onto
  the child agent with one explicit setter, alongside the existing
  thinking/speed/block-images inheritance lines. Sub-agent subshells
  then see the same overlay through the same tool seam.

## 5. CLI and local paths

`--env KEY=VALUE`, repeatable, global (like `--tag`, so it reads the
same on either side of a subcommand and reaches `connect`):

- Parse splits at the first `=`. An argument without `=` is refused.
  A key stated twice in one invocation is refused (stating one
  identity twice is a bug, not a preference). Key/value validation
  as in section 1. No environment-variable binding, deliberately
  (the `--api-key` precedent): a session env stated by an exported
  variable in the launching shell would be process-level identity
  sneaking back in.
- **Launch rule**: the launch env applies to every create the
  invocation performs, and only to creates. This is one uniform
  sentence across modes: the startup create of a local run, an
  in-TUI new-session create (local or connected, host picker
  included), a `connect --new` create, and a print-mode fresh run
  all carry it. For the connected in-TUI create this deviates from
  how launch *settings* behave (those are not re-sent, the host
  defaults them): env is identity, and a `connect --new --env` run
  whose user opens a second session must not silently mint an
  identityless one. The deviation is deliberate and this line is its
  record.
- **Local TUI**: the parsed map is carried in the interactive state
  and passed explicitly at each create call site
  (`Control::create` gains an env parameter, the Local arm hands it
  to `create_with`, the Remote arm puts it on the wire request).
  It is not baked into the composed host's base config, per the
  invariant.
- **Resume**: on any resume or materialize the log-level creation
  record wins independently of the selected conversation head. A log
  that records env restores it, and a log that records none resumes
  with none. No branch-local settings fold or launch env may substitute
  for that record. The launch env stays armed for creates the run
  performs later (an in-TUI new session), so a resume does not consume
  or apply it. Where a run's primary gesture creates nothing
  (`aj continue --env`, a bare `aj connect --env` that attached), the
  run says the resumed or attached session keeps its own env and that
  `--env` names creations, mirroring how a `--tag` that named nothing
  is handled. No backfill entry is written.
- **`aj serve` / `aj gateway`**: refuse `--env` at startup with an
  error saying session env is stated per create.
- **Print mode**: a fresh `aj -p` run seeds and applies env like the
  interactive create path. `aj -p continue` follows the resume rule,
  notices to stderr.

## 6. Observability

- **Create answer**: `env_keys` (section 2.2). The workshop wake
  script prints them, which is the at-a-glance verification for the
  unattended path.
- **Session info** (`/info`): the digest gains an Env section under
  Settings listing keys and values. The surface is local-only today
  (spec 9.1) and reads the same log the operator could `cat`, so
  values are shown: verifying identity at a glance is the use case,
  and `BEADS_ACTOR` redacted to a key name verifies nothing.
  `SessionStats` carries the log-level `session_env` separately from
  its branch-local inference settings.
- **Export**: keys only, values redacted (section 3).
- **Non-goals, v1**: no env in the sessions directory rows, `state`
  frames, or any remote read. When session info learns to answer
  over the wire, the Env section rides along and the redaction
  question is decided there, where the trust domain changes.

## 7. Back-compat

- New binary, old log: no `EnvChange` entry, `session_env()` is `None`,
  session runs with no env. No migration.
- Old binary, new log: under the process-crash and surfaced-error
  contract, transactional first publication leaves either no canonical
  file or a complete initial image in which the unknown
  `"type": "env_change"` entry is followed by the first punctuation.
  The old reader therefore reports the interior unknown entry as corrupt
  and leaves the file unchanged. Its general rule still treats an
  unknown final entry as a torn tail and truncates it, but the v1 writer
  does not expose `EnvChange` in that position.
- New client, old host: section 2.2. The echo's absence is the
  signal, the client is loud, nothing pre-gates.
- Old client, new host: never states env, never sees `env_keys`
  (skip-serialized), and ignores it if it ever did (6.10).
- Old gateway between new ends: raw pass-through both directions
  (section 2.4).

## 8. Testing

Mutation-anchored, per the workshop standard: each guarantee has a
test that goes red when the wiring line is deleted, not only when a
helper-built component is driven directly.

- **Wire**: round-trip of `env` on the create request and `env_keys`
  on the answer, plus the forward-compat fixture (env-bearing create
  decoded by the old shape ignores it, answer without `env_keys`
  decodes as `None`).
- **Gateway**: an env-bearing create through the real gateway route
  reaches the host body-intact and the echo returns intact.
  Assert at the upstream-body boundary, not only on the client's
  view.
- **Host**: create with env, drive the real agent, the bash tool
  observes the variables (assert in the child's output, the boundary
  where the harm lands). Invalid map refuses with 400 and leaves no
  session and no log file. Echo lists exactly the stated keys.
  A create without env answers without `env_keys`.
- **Layering**: a session env entry shadows a host process variable,
  and a fixed override (`AGENT`) beats a session entry of the same
  name.
- **Sub-agents and background**: a sub-agent's bash child and a
  background bash task observe the env. The sub-agent test must run
  through the real `spawn_agent` path so deleting the inheritance
  setter goes red.
- **Persistence**: create with env, restart the host (rebuild
  `SessionHost` over the same store), materialize, bash still
  observes the env. Resume under a *different* launch `--env` keeps
  the log's env (the occupant-change case, the reason persistence
  exists). Legacy log resumed with `--env` runs env-less and
  notices.
- **First publication**: an owning subprocess drives the production
  first-punctuation path through deterministic checkpoints after the
  staged image is written and flushed and around no-clobber install.
  Kill and wait at each checkpoint. The canonical path is always absent
  or contains punctuation after `EnvChange`. Injected write, flush, and
  install errors keep the complete pending prefix; retrying a
  punctuation append on the same `ConversationLog` publishes the
  original env once. A frozen pre-`EnvChange` codec fixture first proves
  it truncates a manually constructed final unknown entry, then proves
  it refuses the published interior unknown entry without changing
  bytes. Direct target writes or draining pending records before install
  must make these tests red.
- **Head independence**: through a real `SessionHost`, create with env,
  switch successfully to the system-prompt root, and send a prompt whose
  user-thread ancestry omits every inference-setting seed. Before that
  prompt the switch leaves the canonical path absent. `session_env()`
  and the real bash child still observe the creation map, including
  after host restart and materialization on that branch. Deriving env
  from `Conversation::settings()`, clearing it on head switch, or
  rejecting the root head must make this test red.
- **Local paths**: launch `--env` reaches the startup create's seed
  entry and an in-TUI new session's seed entry. `aj serve --env`
  refuses to start. Duplicate-key and missing-`=` parses refuse.
- **Export/replay**: export artifact contains the keys and the
  redaction marker and not the values (assert on the decoded
  embedded entries). Seed entry projects no replay notice.
- **Client echo check**: against a scripted answer lacking
  `env_keys`, the connect path surfaces the predates-session-env
  notice.

## 9. Acceptance (end to end)

On a bench: a create carrying `{"BEADS_ACTOR":"judd"}` against a host
whose process env says nothing (or says another seat) yields a
session whose bash tool prints `BEADS_ACTOR=judd`, including from a
sub-agent's subshell, and still prints it after the host process is
restarted and the session is materialized by a new prompt. The wake
script then carries the actor per create and the bench drop-ins are
deleted (tracked separately, outside this repo).
