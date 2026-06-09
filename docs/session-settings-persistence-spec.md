# Session settings persistence & restore

## Motivation

Today the conversation log persists messages and the frozen system
prompt, but not the model or thinking effort the session ran with.
Resuming a session (`aj continue`, interactive `/resume`) builds the
agent off the *current* run config — CLI flags, env, `config.toml` —
so a session that was running `openai/gpt-x` quietly continues on
whatever the user's current default happens to be.

This spec makes the session log the source of truth for the session's
model, thinking effort, and speed: changes are recorded as log
entries, and on resume the recorded settings take precedence over the
current defaults.

Assistant messages already carry `api` / `provider` / `model`
(`aj_models::types::AssistantMessage`), and they are persisted
verbatim. That gives us per-turn model attribution for free and a
restore fallback for logs that predate this work.

## 1. On-disk format (`aj-session`)

Three new variants on `ConversationEntryKind`:

```rust
/// The active model changed (or was first recorded). `provider` and
/// `model_id` key into the model catalog.
ModelChange { provider: String, model_id: String },

/// The active thinking effort changed (or was first recorded).
/// `level` is one of "off", "low", "medium", "high", "xhigh", "max"
/// (the same vocabulary `thinking_level_name` renders). Stored as a
/// string so the on-disk format stays stable if the effort enum
/// evolves; unknown values are ignored on restore with a notice.
ThinkingChange { level: String },

/// The active speed changed (or was first recorded). `speed` is
/// "standard" or "fast" (the `ConfigSpeed` Display vocabulary);
/// unknown values are ignored on restore with a notice.
SpeedChange { speed: String },
```

Serialized with the existing `#[serde(tag = "type")]` scheme as
`"type": "model_change"` / `"type": "thinking_change"` /
`"type": "speed_change"` lines.

Framing:

- **Thread**: the thread whose settings they describe —
  `ThreadKind::User` for the main conversation, appended at the user
  thread's current leaf, and `ThreadKind::Subagent` (with `agent_id`)
  for sub-agent threads (see §2.4). Settings entries are part of the
  thread's timeline, so a linearize from any head sees exactly the
  settings that were active on that path, and each thread's settings
  are independent.
- **Punctuation**: `is_punctuation() == false`. Settings entries
  buffer in `pending_writes` like the system prompt, so a session
  where the user only opens the TUI and flips the model — but never
  submits — still leaves no file on disk.

Read-side consumers checked against the new variants:

- `Conversation::messages()` / `agent_messages()` / `message_count()`
  filter on `Message { .. }` — unaffected.
- `repair_interrupted_tool_uses` walks `Message` entries with a
  let-else — unaffected.
- `replay` renders settings entries as notices (see §2.5).
- `SessionPreview` walk matches `Message` — unaffected.

### `SessionSettings` extraction

New on `Conversation` (the linearized read-only view):

```rust
pub struct SessionSettings {
    /// Last (provider, model_id) recorded on this path: the most
    /// recent ModelChange entry, falling back to the most recent
    /// assistant message's (provider, model) for logs that predate
    /// settings entries.
    pub model: Option<(String, String)>,
    /// Last recorded thinking level string, from the most recent
    /// ThinkingChange entry. `None` means "nothing recorded"
    /// (inherit the current default) — distinct from `Some("off")`.
    pub thinking: Option<String>,
    /// Last recorded speed string, from the most recent SpeedChange
    /// entry. `None` means "nothing recorded".
    pub speed: Option<String>,
}

impl Conversation {
    pub fn settings(&self) -> SessionSettings { ... }
}
```

One forward scan over `entries()`, keeping the last value seen per
axis. A `ModelChange` entry and an assistant message both update the
model; whichever comes later on the path wins.

## 2. Writing entries

The persistence listener keeps exclusive ownership of *message*
writes. Main-thread settings entries are appended by the binary,
which already holds the log handle — they originate from binary-level
state (`RunConfigSnapshot`), not from agent activity. Sub-agent
settings entries are the one exception: they are written by the
persistence listener off `AgentEvent::SubAgentStart` (§2.4), since
spawns happen inside the agent. The module docs in
`aj-session/src/listener.rs` are updated to state this split.

Append path: new `ConversationLog` helpers
(`append_model_change` / `append_thinking_change` /
`append_speed_change`) taking a `ThreadFilter`, appending at that
thread's `latest_leaf` (anchoring at the system-prompt root for
fresh logs, mirroring `ConversationView::parent_for_next_append`).

### Speed becomes explicit run-config state

Today speed is resolved once at startup (`--speed` > `config.speed`)
and baked into `StreamOptions` headers; `RunConfigSnapshot` doesn't
track it, and the `/model` selector silently drops it on swap
(passing `None` to `from_model_info`). To make speed recordable and
restorable, `RunConfigSnapshot` gains a `speed: Option<Speed>` field,
and every bundle rebuild (`/model` swap, restore) passes `cfg.speed`
into `from_model_info` instead of `None`. This fixes the existing
drop-on-swap quirk as a side effect: `--speed fast` now survives a
`/model` pick (degrading silently on providers that ignore it, as
today).

Write sites:

1. **Session creation** (interactive `SessionWorld::build` Create
   path, and print mode's fresh-log path): immediately after
   `set_system_prompt`, record one `ModelChange`, one
   `ThinkingChange`, and one `SpeedChange` reflecting the active run
   config. This pins the session's initial settings so a resume
   restores them even if the global default changes in between and
   the session never saw a switch. Buffered like the system prompt —
   no file until the first real message.
2. **`/model` confirm** (`handle_selector_outcome`, Model arm): after
   staging the swap into `RunConfigSnapshot`, lock `world.log` and
   append a `ModelChange`. The existing `config.toml` persistence
   (global default) stays as-is — a switch updates both the session
   record and the user default.
3. **`/thinking` confirm** (Thinking arm): same, appending a
   `ThinkingChange` with `thinking_level_name(&level)`.
4. **Sub-agent spawn**: see §2.4.

There is no runtime speed switcher today, so the seed entry is the
only `SpeedChange` writer; a future `/speed` command appends one the
same way the `/model` and `/thinking` arms do.

Mid-turn changes are fine: the entry lands between two messages on
the user thread and the next `MessageEnd` chains onto it via
`latest_leaf`. This also means the log records the change at the
moment the user made it, even though the in-flight turn keeps its
old settings — matching what the next inference will actually use.

### 2.4 Sub-agent threads

Sub-agent threads get the same initial settings record as the main
thread, so per-sub-agent model/effort settings can be layered on
later without a format change, and so the log is self-describing
about what each sub-agent actually ran with.

Sub-agents are spawned inside the agent, so the binary isn't the
writer here. Instead:

- `AgentEvent::SubAgentStart` gains the child's bundle identity:
  `provider: String`, `model_id: String`, `thinking: String`, and
  `speed: String` (same vocabularies as the entry kinds). The agent
  populates these from the child's bundle when it emits the event.
  Today these always mirror the parent's bundle; when per-sub-agent
  settings land, the same fields carry the sub-agent-specific values
  with no event change.
- The persistence listener, on `SubAgentStart`, appends the
  `ModelChange` / `ThinkingChange` / `SpeedChange` triple on the
  sub-agent's thread, anchored at the captured parent head — before
  any of the sub-agent's messages. The first `MessageEnd` for that
  sub-agent then chains onto the settings entries via
  `latest_leaf(ThreadFilter::subagent(n))`, so the pending-anchor
  map is consumed at `SubAgentStart` time rather than at the first
  message.

Restore-side, sub-agent settings entries are recorded but not acted
on today: the registry starts empty on resume (resumed sub-agents
are not re-promptable), and fresh sub-agents take the main bundle.
When per-sub-agent settings ship, restore can read these entries.

Main-thread restore is unaffected: `Conversation::settings()`
operates on a `ThreadFilter::USER` linearization, which excludes
sub-agent entries.

### 2.5 Replay rendering

`replay` projects a settings entry to one `AgentEvent::Notice`
(e.g. `Model set to <prov>/<id>.`, `Thinking effort set to high.`,
`Speed set to fast.`) **only when at least one `Message` entry
precedes it on the same thread**. This renders mid-session switches
in resumed scrollback — mirroring the notice the selector showed
live — while keeping the initial seed entries (session creation,
sub-agent spawn) silent, since those never produced a visible notice
live either.

This also requires two small adjustments to replay's sub-agent
bracketing, since a sub-thread's first entries are now settings
entries rather than the task message:

- `bracket_subagent` / `subagent_task` take the task from the
  run's first `Message` entry instead of its first entry.
- The synthesized `AgentEvent::SubAgentStart` populates the new
  `provider` / `model_id` / `thinking` fields from the sub-thread's
  settings entries (falling back to the first sub-assistant
  message's `provider`/`model` and an empty level for legacy logs).

## 3. Restore on resume

### Precedence

CLI flags (`--model-api` / `--model-name`, and the `MODEL_API` /
`MODEL_NAME` env vars clap merges into them) apply to **new sessions
only**. On *any* resume — startup `aj continue`, interactive
`/resume` — the session log's recorded settings win:

1. The session log's recorded settings.
2. Current run config (CLI flags > env > `config.toml` > defaults) —
   only reached when the log records nothing for an axis, i.e.
   legacy logs with no assistant turns (model) or no recorded
   thinking/speed change.

A mid-process `/resume` likewise restores from the target session's
log, even after a mid-process `/model` pick — restoring the session
as it was is the point of this feature.

### Interactive (`SessionWorld::build`)

`build` gains a restore context:

```rust
pub struct RestoreContext {
    pub registry: Arc<ModelRegistry>,
    pub auth: AuthStorage,
}
```

passed as `Option<&RestoreContext>` (`None` in scripted mode and in
unit tests, which disables restoration). On the Resume path, after
linearizing the user thread:

1. `let settings = conversation.settings();`
2. **Speed**: if `settings.speed` is `Some(s)`, parse it back to
   `Option<Speed>` (unknown strings → keep current, notice) and
   overwrite `run_config.speed`. Resolved first because the model
   bundle rebuild below stamps speed headers.
3. **Model**: if `settings.model` is `Some((prov, id))` and
   `(prov, id) != run_config.model_key`:
   - `registry.get(prov, id)` →
     `crate::model::from_model_info(auth, info, run_config.speed)`,
     then overwrite `provider` / `model_info` / `stream_options` /
     `model_key` in `RunConfigSnapshot` and re-apply
     `apply_thinking_display` (it's stamped per-bundle in
     `build_run_config`). Notice: `Restored model <name>
     (<prov>/<id>) from session.`
   - Catalog miss or `from_model_info` error: keep the current
     bundle. Notice: `Session used <prov>/<id>, which is not
     available; continuing with <current>.`
   - If the model is unchanged but the restored speed differs from
     the one baked into the current `stream_options`, rebuild the
     bundle for the current `model_info` with the restored speed so
     the headers match.
   - Auth is *not* checked here — key resolution is deliberately
     lazy (see `aj/src/model.rs`), so an uncredentialed restored
     provider surfaces at the next turn and the user can `/login`.
4. **Thinking**: if `settings.thinking` is `Some(level_str)`, parse
   it back to `Option<ThinkingConfig>` (the inverse of
   `thinking_level_name`; unknown strings → keep current, notice).
   Validate against the restored model via
   `aj_models::registry::validate_thinking_level`; on rejection keep
   the current run-config level and emit a notice (clamping rules are
   provider-specific, so we don't guess a substitute rung). On
   success overwrite `run_config.thinking`.
5. The agent for the world is then built from the (possibly updated)
   snapshot, exactly as today.

`SessionWorld` grows `pub restore_notices: Vec<String>`; callers
(`InteractiveMode::run` startup and `build_next_world`) append them
to the notices they already pump onto the chat scrollback.

Because restoration mutates the shared `RunConfigSnapshot`, the
restored settings persist for subsequent turns, `/model` pre-select,
and later `/new` sessions — same semantics as a manual pick. Callers
refresh the dependent UI after `install`:

- `refresh_footer_model(...)` with the restored id + level,
- `apply_editor_border_for_thinking(...)`,
- the context-window denominator is already seeded from the world's
  agent (`EventPump::new(.., context_window)`).

### Print mode (`print::run`)

The `continue` path currently resolves the model before opening the
log. Reorder: resolve the `ConversationLog` first, compute
`conversation.settings()`, then pick the model/speed with the same
precedence (session > `args.model_*`/`--speed`/config; flags only
apply to fresh runs). The restored-model notices go to stderr like
the existing config diagnostics. The fresh-log path appends the
three initial settings entries like the interactive path.

## 4. Back-compat

- **New binary, old log**: no settings entries → model falls back to
  the last assistant message's `(provider, model)`; thinking and
  speed fall back to the current defaults. No migration needed.
- **Old binary, new log**: serde rejects the unknown `type` tag, so
  resume reports the log as corrupt (unless the entry happens to be
  the torn-tail line). Same forward-compat posture as any other
  entry-kind addition; acceptable.

## 5. Testing

- `aj-session` unit tests:
  - settings entries round-trip through resume; buffered (no file)
    until first punctuation; appear in their thread's linearize and
    are skipped by `messages()` / `message_count()`.
  - `Conversation::settings()`: last-wins ordering, assistant-message
    fallback, `None` vs `Some("off")` for thinking, branch-awareness
    (settings entry on one path not visible from a head on another),
    sub-agent entries excluded from the user-thread scan.
  - persistence listener: `SubAgentStart` writes the sub-thread
    settings triple anchored at the parent head; the sub-agent's
    first message chains onto them.
  - replay: seed entries (pre-message) are silent; mid-session
    entries emit one `Notice` each; sub-agent task extraction and
    `SubAgentStart` field population work with settings entries
    leading the sub-thread, and with legacy logs lacking them.
- `aj` integration tests (scripted provider where possible):
  - fresh session writes the three initial entries.
  - `/model` / `/thinking` confirms append entries; `/model` swap
    preserves the active speed.
  - `SessionWorld::build` resume restores run config (including
    speed headers) and reports notices; catalog-miss fallback keeps
    the current model; restore applies even when the process was
    started with `--model-name` / `--speed`.
- print mode: `aj --print continue` uses the session's recorded
  model even when flags are passed; flags apply on fresh runs.
