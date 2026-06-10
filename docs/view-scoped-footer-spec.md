# View-scoped footer & per-agent settings — spec

Status: implemented.

## Motivation

The footer always renders the **main agent's** state, regardless of
which agent the chat view is observing:

- The model + thinking-effort line is a single global string, set at
  startup and refreshed by the `/model` / `/thinking` selectors from
  the shared `RunConfigSnapshot`
  (`src/aj/src/modes/interactive.rs`, `refresh_footer_model`).
- The context-occupancy indicator is fed by one `FooterData`
  snapshot in the event pump that deliberately drops every
  non-main `TurnUsage`
  (`src/aj/src/modes/interactive/event_pump.rs`, the
  `TurnUsage` arm).
- `EventPump::set_active_view` switches the transcript, the spinner,
  and the editor's `agent N` marker — but never touches the footer.

Sub-agents are full `Agent` instances with their own context-window
occupancy and their own settings: each spawn snapshots the parent's
bundle into the child (model, thinking effort, speed), and
`AgentEvent::SubAgentStart` reports that snapshot as `AgentSettings`.
But two pieces of the binary still treat settings as session-global:

- The per-turn stamping in the submit handler applies the shared
  `RunConfigSnapshot` to **whatever agent** the turn targets, so a
  sub-agent continuation silently overwrites the sub-agent's own
  settings with the global config.
- The `/model` / `/thinking` selectors always read and write the
  shared snapshot, with no way to address a sub-agent.

This spec makes settings genuinely per-agent and the footer
view-scoped:

1. **Sub-agents keep their settings.** A continuation turn runs with
   the sub-agent's own settings unless the user explicitly changed
   them for that agent.
2. **Selectors target the viewed agent.** `/model` and `/thinking`
   opened while a sub-agent view is active read from and apply to
   that sub-agent.
3. **The footer describes the viewed agent**: its model + effort
   line and its context occupancy.

## Display contract

When the chat view's active agent is `id`, the footer shows:

- **Model line**: `"<model-id> <thinking>"` for `id`, using the same
  `"off"`/`"low"`/.../`"max"` vocabulary as `AgentSettings.thinking`.
  Semantics are uniform across views: the line shows **what the next
  turn on this agent runs with** — staged changes appear
  immediately, an in-flight turn keeps the config it captured (same
  wording the run-config docs use for main today).
- **Context usage**: `tokens/window (p%)` for `id` — numerator from
  `id`'s last `TurnUsage`, denominator from `id`'s model's context
  window. `?` before the first usage event; suppressed entirely when
  the window is unknown (existing `context_window == 0` semantics).
- **Unchanged, view-independent parts**: cwd and the aggregate
  `N agents (alt+a)` indicator.
- **Speed is not displayed.** It is tracked in the per-agent
  settings snapshot (it rides along in `AgentSettings`) but the
  footer ignores it, same as today.

The editor's thinking border tint follows the same view-scoped
thinking value as the footer line, so the two cues never disagree.
The `agent N` top-bar marker stays as-is.

## Settings contract

Every agent owns its settings (model bundle, thinking effort,
speed):

- **Main** is driven by the shell-lifetime `RunConfigSnapshot`,
  exactly as today: selectors stage into it, every main turn stamps
  it onto the main agent, choices persist to `config.toml` and to
  the session log's user thread.
- **A sub-agent** is born with the parent's bundle (spawn-time
  inheritance in `SessionContextWrapper::spawn_agent`, unchanged)
  and **keeps those settings for its whole life**. Continuation
  turns no longer stamp the global run config onto sub targets. The
  only way a sub-agent's settings change is the user explicitly
  changing them while viewing that agent.
- **Per-sub-agent changes** are staged loop-side (the loop must
  never lock an agent's `TokioMutex`) and applied at the sub-agent's
  next turn start — mirroring the main-agent mid-turn semantics:
  accepted and displayed immediately, effective next turn.
- A sub-view change does **not** touch `config.toml` (that records
  the user's session default, which is main's concern) and does
  **not** touch main's run config. It is recorded in the session log
  on the **sub-agent's thread** (the settings entry kinds are
  already thread-scoped; `append_model_change` /
  `append_thinking_change` take a `ThreadFilter`).

## Design

### 1. Per-agent footer state (`footer_data.rs`)

`FooterData` becomes a per-agent store:

```rust
/// Displayable state for one agent: its settings identity plus the
/// context-occupancy pair.
struct AgentFooter {
    /// Next-turn settings (provider, model_id, thinking, speed).
    /// Speed is carried but not rendered.
    settings: AgentSettings,
    /// Context window of `settings`' model, in tokens. Zero means
    /// unknown and suppresses the indicator.
    context_window: u64,
    /// Prompt size of the agent's most recent turn (turn_input +
    /// cache read + cache write), `None` until the first TurnUsage.
    last_turn_context_tokens: Option<u64>,
}

pub struct FooterData {
    /// Keyed by agent. The Main entry always exists (seeded at
    /// construction); Sub entries are created on SubAgentStart and
    /// kept for the pump's lifetime so finished sub-agents (still
    /// selectable in the picker) render their final state.
    agents: HashMap<AgentId, AgentFooter>,
}
```

API (all keyed by `AgentId`):

- `new(main_settings: AgentSettings, main_window: u64)` — seeds the
  Main entry.
- `note_settings(id, settings, context_window)` — insert-or-replace
  the settings identity, preserving `last_turn_context_tokens` (a
  model swap doesn't erase what the last prompt cost).
- `record_turn_usage(id, usage)` — fold a `TurnUsage` into `id`'s
  entry; creates the entry with empty settings if it's somehow
  missing (defensive, e.g. event order in tests).
- `context_usage(id) -> ContextUsage` — view for the footer; falls
  back to the Main entry when `id` has none.
- `model_line(id) -> Option<String>` — `"<model_id> <thinking>"`;
  `None` when the entry's `model_id` is empty (legacy replayed
  spawns carry `fallback_settings()` with empty provider/model — we
  drop the line rather than render garbage). Falls back to Main when
  `id` has no entry.
- `settings(id) -> Option<&AgentSettings>` — read-back for selector
  pre-selection and the editor border tint.

The existing `format_footer_model` in `interactive.rs` folds into
`model_line` (string-based formatting; callers pass
`thinking_level_name(...)` where they hold a `ThinkingConfig`).

This store is the binary's single source of truth for "what does
agent `id` currently run with" — the selectors read it, the footer
renders it, and the staging sites write it. It is display *and*
identity state, but deliberately holds only strings/scalars; the
live handles (provider `Arc`s, `StreamOptions`) stay in the run
config and the override map (§3).

### 2. Pump wiring (`event_pump.rs`)

`EventPump::new` gains the main settings seed and a catalog handle:

```rust
pub fn new(
    theme: ChatTheme,
    render_settings: RenderSettings,
    main_settings: AgentSettings,
    main_context_window: u64,
    catalog: Arc<Vec<ModelInfo>>,
) -> Self
```

- `sync_footer` reads the active view (`ChatView::active`) and
  pushes both the model line (`Footer::set_model`) and the context
  usage for that agent. This centralizes all footer model-line
  writes in the pump; `refresh_footer_model` and the direct
  `footer.set_model(..)` at startup go away (cwd stays where it is).
- `TurnUsage { agent_id, usage }`: always
  `footer_data.record_turn_usage(*agent_id, usage)`; call
  `sync_footer` when `*agent_id` is the active view. The
  per-transcript dim usage lines are unchanged.
- `SubAgentStart { child, settings, .. }`: in addition to
  `ensure_sub_box`, seed the child's entry:
  `footer_data.note_settings(child, settings, resolve_window(settings))`.
- `set_active_view`: additionally `sync_footer`. (Callers handle the
  border tint, see §6.)
- New `note_agent_settings(tui, id, settings, context_window)`:
  called by the binary whenever an agent's next-turn settings change
  (selector confirms for main and subs alike, resume reconciliation
  — see §3/§5). Syncs the footer when `id` is the active view.
- New getter `agent_settings(id)` delegating to
  `FooterData::settings` for selector pre-selection and border tint.
- `set_context_window` is absorbed into `note_agent_settings`.

**Window resolution** for settings known only as
`(provider, model_id)` strings (`resolve_window`):

1. Catalog scan: `catalog.iter().find(|m| m.provider == p && m.id == id)`
   → `context_window`. The catalog is the authoritative source and
   is already loaded once at startup (`Shell::model_catalog`).
2. Miss, but `(provider, model_id)` equals the Main entry's →
   Main's window. This covers scripted runs and `--model-url`
   bundles that aren't in the catalog: sub-agents inherit the
   parent's bundle, so the identity match is exact in practice.
3. Otherwise `0` → indicator suppressed for that view.

Call sites that hold a resolved `ModelInfo` (selector confirms) pass
`info.context_window` directly and skip resolution.

The context window is deliberately *not* added to
`AgentEvent::SubAgentStart` or the on-disk `SubAgentSpawn` entry: it
is derivable from the catalog via the settings keys, and resolving it
at display time means live and replayed events take the same path
and pick up catalog updates.

### 3. Per-agent run configuration (`interactive.rs`, `session.rs`)

Main keeps the shell-lifetime `Arc<Mutex<RunConfigSnapshot>>`
unchanged. Sub-agents get a per-world override map:

```rust
/// Loop-side staged settings for one sub-agent. Each axis is
/// `Some(..)` only if the user changed it for this agent; axes left
/// `None` keep whatever the agent itself holds (its spawn-time
/// inheritance). The outer/inner Option split on `thinking` matters:
/// `Some(None)` means "explicitly set to off".
struct SubAgentOverrides {
    /// Full bundle swap from a `/model` confirm: provider handle,
    /// model info, stream options, and the `(provider, id)` key.
    bundle: Option<(Arc<dyn Provider>, Arc<ModelInfo>, StreamOptions, (String, String))>,
    thinking: Option<Option<ThinkingConfig>>,
    speed: Option<Option<Speed>>,
}

/// On `SessionWorld` (sub ids are per-session):
sub_overrides: Arc<std::sync::Mutex<HashMap<usize, SubAgentOverrides>>>,
```

The submit handler's turn task receives both handles and applies at
turn start, under the turn's own agent lock (uncontended, as today):

- `Main`: stamp the full `RunConfigSnapshot` (unchanged).
- `Sub(n)`: apply only the axes present in `sub_overrides[n]`, if
  the entry exists; otherwise stamp **nothing** — the agent runs
  with its own settings. Overrides are kept (not drained) and
  re-applied idempotently each turn; the entry *is* the user's
  standing choice for that agent.

This is the behavioral core of "subs keep their settings": removing
the unconditional global stamp for sub targets.

`aj-agent` needs no changes: spawn-time inheritance, the
`SubAgentStart` settings snapshot, and the registry already provide
everything the binary needs.

### 4. Selectors target the viewed agent

Both the open paths (`SlashAction::OpenThinkingSelector` /
`OpenModelSelector`) and the confirm arms in
`handle_selector_outcome` resolve the **target agent** as
`world.pump.active_view(..)` at open time, and carry it through the
`OpenSelector::Thinking` / `OpenSelector::Model` state so the
confirm applies to the agent the user was looking at, even if the
view changed while the overlay was up.

**Open paths**: read the current value for pre-selection from the
target's footer settings (`pump.agent_settings(target)`): the model
selector pre-selects `(provider, model_id)`, the thinking selector
parses the thinking string back to `Option<ThinkingConfig>`. For
`Main` this is equivalent to today's run-config read (the Main
footer entry mirrors the run config by construction).

**`/thinking` confirm**, target `Sub(n)`:

- Stage: `sub_overrides[n].thinking = Some(level)`.
- Display: `pump.note_agent_settings(tui, target, updated, window)`
  where `updated` is the target's current footer settings with the
  thinking string replaced (window unchanged).
- Border tint: `apply_editor_border_for_thinking(..)` — the user is
  viewing this agent, so the cue updates exactly like the main flow.
- Log: `append_thinking_change(ThreadFilter::subagent(n), name)`.
- **No** `config.toml` persistence.
- Notice: `Thinking effort set to <name> for agent <n>.`

**`/model` confirm**, target `Sub(n)`:

- Resolve the bundle via `from_model_info(auth, info, speed)` where
  `speed` is the target's effective speed (override if staged, else
  parsed from the target's footer settings string).
- Stage: `sub_overrides[n].bundle = Some(..)`.
- Display: `note_agent_settings` with the new
  `(provider, model_id)`, the preserved thinking string, and
  `info.context_window` as the window.
- Log: `append_model_change(ThreadFilter::subagent(n), ..)`.
- **No** `config.toml` persistence.
- Notice: `Model set to <name> (<prov>/<id>) for agent <n>.`

**Confirm with target `Main`** keeps today's flow (run-config
staging, `config.toml` persist, `ThreadFilter::USER` log entry,
border tint), with the footer refresh routed through
`pump.note_agent_settings(tui, AgentId::Main, ..)` instead of
`refresh_footer_model` + `set_context_window`.

A non-promptable target (a resumed sub-agent with no live handle and
no override map effect) is still allowed: the staged override and
log entry are harmless, but to keep the UX honest the confirm arm
checks `resolve_agent(target, ..)` and, when the target can't be
prompted, surfaces the existing "This agent can't be prompted."
notice instead of staging.

Thinking-level validation (`validate_thinking_level`) runs against
the **target's** model: the staged bundle override's `ModelInfo` if
present, else a catalog lookup by the target's settings keys, else
skip validation (same lenient posture as scripted mode).

### 5. Resume / replay

Replay already synthesizes `SubAgentStart` with the recorded
`AgentSettings` (or `fallback_settings()` for legacy logs) and
per-agent `TurnUsage` events; pumping them through
`EventPump::handle` populates the per-agent entries like a live run.

Mid-thread settings entries (`ModelChange` / `ThinkingChange` /
`SpeedChange` on a sub-agent thread, written by §4) are projected by
replay as plain notices, which don't carry structure the pump can
fold. To keep resumed footer entries accurate, `SessionWorld::install`
reconciles after replay: for each sub-agent thread present in the
log, linearize it and call
`pump.note_agent_settings(tui, Sub(n), conversation.settings()…, resolve_window(..))`.
`Conversation::settings()` already folds the spawn snapshot plus any
later change entries, last-wins. (Axes the log doesn't record map to
the `fallback_settings()` defaults, matching what replay showed.)

Resumed sub-agents are not promptable (the registry starts empty),
so no override restoration is needed — the reconciled footer entry
is purely informational, like the rest of a resumed sub transcript.

Legacy fallback settings have empty provider/model: window
resolution yields 0 and `model_line` yields `None`, so a resumed
legacy sub view shows no indicator and no model line rather than
wrong data.

### 6. Editor border tint

The picker-confirm path (`AgentPickerOutcome::Confirmed`) and the
session-reset path that re-applies the agent marker also re-apply
the border: parse the viewed agent's thinking string
(`pump.agent_settings(id)`) back to `Option<ThinkingConfig>` and
call `apply_editor_border_for_thinking`. The thinking-string parse
already exists in the resume-restore path; extract it next to
`thinking_level_name` as its inverse so all sites share it.

## Out of scope

- Displaying speed anywhere, and a `/speed` command (the override
  struct carries the axis so a future command slots in without
  reshaping anything).
- Per-spawn settings parameters on the `agent` tool (the model
  choosing a different bundle for its sub-agent).
- Showing settings in the agent picker rows or sub-agent box headers
  (possible follow-up once this state exists per agent).
- Restoring promptability + overrides for resumed sub-agents.

## Testing

`footer_data.rs` unit tests (rework existing ones onto the keyed
API):

- per-agent fold: usage for `Sub(1)` doesn't touch Main's numerator
  and vice versa; last-wins per agent.
- `note_settings` preserves the existing numerator.
- `context_usage` / `model_line` fall back to Main for unknown ids.
- `model_line` is `None` for empty `model_id`.

`event_pump.rs` tests:

- Main usage drives the footer while main is viewed (existing test,
  unchanged behavior).
- Sub usage leaves the footer alone **while main is viewed**
  (existing test, reworded rationale).
- `SubAgentStart` + switch to the sub view: footer shows the sub's
  model line and `?/<resolved window>`; its `TurnUsage` then updates
  the indicator live; switching back restores main's line and usage.
- Window resolution: catalog hit; catalog miss with identity match
  → main's window; full miss → indicator suppressed.
- `note_agent_settings(Main, ..)` while a sub view is active doesn't
  repaint; the new line shows after switching back to main.
- `note_agent_settings` for the viewed sub repaints immediately.
- A replayed event sequence (`SubAgentStart` with recorded settings,
  then `TurnUsage(Sub)`) populates the sub view's footer on resume.

`interactive.rs` / integration tests (scripted provider):

- A sub-agent continuation turn runs with the sub-agent's own
  settings after a main-view `/model` change (no global stamp).
- `/thinking` confirmed while viewing a sub stages an override that
  the sub's next turn applies, updates the footer immediately,
  appends a `ThinkingChange` on the sub thread, and leaves
  `config.toml` and main's run config untouched.
- `/model` confirmed while viewing a sub: same shape, including the
  sub-thread `ModelChange` entry and the preserved effective speed.
- Selector open on a sub view pre-selects the sub's current
  model / thinking.
- `SessionWorld::install` reconciliation: a log with a sub thread
  containing a spawn snapshot plus a later `ThinkingChange` yields a
  footer entry with the changed level.

## Stages

1. **`footer_data.rs` rework** — per-agent store + unit tests. Pure
   data, no wiring.
2. **Pump + display wiring** — `EventPump::new` signature, event
   arms, `set_active_view` sync, `note_agent_settings`, window
   resolution, and the `interactive.rs` / `session.rs` display call
   sites (removing `refresh_footer_model` / `format_footer_model` /
   `set_context_window`). Pump tests.
3. **Per-agent run config** — `SubAgentOverrides` map on
   `SessionWorld`, submit-handler stamping change (no global stamp
   for sub targets). Integration test for settings retention.
4. **View-targeted selectors** — target threading through
   `OpenSelector`, per-target confirm arms (staging, logging,
   notices, validation), pre-selection from footer settings, border
   tint on view switch, install-time reconciliation.
