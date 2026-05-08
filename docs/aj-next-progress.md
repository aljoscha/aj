# aj-next Plan Implementation Progress

Tracking file for `docs/aj-next-plan.md`. Each item maps to a step
in §2 (Phase 0), §4 (Phase 1), or §5 (Phase 2). Use `git log` for
the authoritative state; this file is the bridge between the plan
and the git history.

> **Relationship to `models-progress.md`.** This file is the
> commit-by-commit decomposition of `models-progress.md` Phase 6
> (steps 16–18). The aj-next §2 rollout ends with `aj-agent`
> migrated to the unified types and streaming protocol, which is
> exactly what models-spec step 16 requires. Once §2.4–§2.6 land,
> tick 16/17/18 in `models-progress.md`. Until then, **this is
> the active tracking file**; pick the next unchecked item from
> here.

## Phase 0 — refactor the core (§2)

### §2.0 Preparation

- [x] Reconnaissance: inspect on-disk thread files in `~/.aj/threads/`
      to choose between serde rename and rewriting walker for
      `UserOutput` → `ToolDetails` migration. **Decision: rewriting
      walker, no `schema_version` header.** See "Reconnaissance
      findings" below.
- [x] (b) Move contract types into `aj-agent`: new `events`, `tool`,
      and `message` modules. Types are defined but not yet wired.
- [x] (c) Flip `aj-tools`'s dependency to `aj-agent` instead of
      `aj-ui`. Tool implementations + `get_builtin_tools()` only.
      Legacy contract types (`ToolDefinition`, `ToolResult`,
      `ErasedToolDefinition`, `SessionContext`, `TurnContext`) moved
      to `aj_agent::legacy_tool`; `ErasedToolDefinition` is now
      `Clone` (Arc-shared closure) so sub-agents inherit the parent
      toolset without re-calling `get_builtin_tools()`. `TodoItem` /
      `TodoPriority` / `TodoStatus` are canonicalized in
      `aj_agent::tool` and re-exported from `aj_tools::tools::todo`.
- [x] (a) Extract `aj-session` crate from today's
      `aj_models::conversation::*`, with a replay module returning
      an iterator. **Status:** the new `aj-session` crate carries
      `ConversationLog`, `ConversationView`, `ConversationPersistence`,
      `ConversationEntry`, `ConversationEntryKind`, `ThreadKind`,
      `ThreadFilter`, `EntryId`, `ConversationError`, plus the
      read-only `Conversation` view (refactored to expose
      `messages()` so the wire-only `Model` trait now takes
      `&[MessageParam]`). The legacy `Model` trait stays in
      `aj-models` but no longer references any persistence type, so
      `aj-models` dropped its `aj-ui` dependency and is now wire-only.
      `aj-agent` (and `aj`) imports the persistence types from
      `aj-session`. The `replay()` function returns
      `impl Iterator<Item = &ConversationEntry>` for now: the
      target shape is `Iterator<Item = AgentEvent>` per §1.1, but
      `aj-agent` still depends on `aj-session` for `ConversationLog`
      during §2.1–§2.3, so a `aj-session → aj-agent` dep would form
      a cycle. The signature flips once §2.4 lands and the cycle
      breaks; the iterator-driven call-site shape stays unchanged.

### §2.1 Agent emits events alongside `AjUi` calls

- [x] Agent gains a private bus and emits `AgentEvent` parallel to
      every `self.ui.foo(...)`. Tests subscribe; no production
      subscribers yet.
      Implemented in `aj-agent::bus` (`EventBus`, `Listener`,
      `SubscriptionHandle`, `listener_from_sync`). `Agent` exposes
      `subscribe(...)` and tags every event with an `AgentId`;
      sub-agents get an `AgentId::Sub(n)` via `set_agent_id`. Bus
      emits land alongside the existing UI calls for: lifecycle
      (`AgentStart`, `AgentEnd`, `TurnStart`); diagnostics (`Notice`,
      `Warning`, `Error`); streaming retries (`StreamRetry`); tool
      execution (`ToolExecutionStart`, `ToolExecutionEnd` with
      `ToolDetails::Text` for now); and sub-agent correlation
      (`SubAgentStart`, `SubAgentEnd`).
      `MessageStart`/`MessageUpdate`/`MessageEnd`, the full
      `TurnEnd` payload, `ToolExecutionUpdate`, and `QueueUpdate`
      events are deferred to §2.2/§2.4 once the unified message
      types and per-tool `ToolOutcome` migrations land — the variant
      shapes already require `AssistantMessage` /
      `AssistantMessageEvent` from `aj-models::types`/`streaming`,
      and bridging them through the legacy `MessageParam` flow
      would be throwaway work. Two `event_protocol_tests` lock the
      sequence we *do* emit today.

### §2.2 Refactor tools to `ToolContext` + `ToolOutcome`

- [ ] Per-tool migration off `&mut dyn AjUi` onto `&mut dyn ToolContext`.
  - [x] `read_file` (Text). Migrated to `aj_agent::tool::ToolDefinition`;
        returns `ToolOutcome { content, details: ToolDetails::Text { summary,
        body }, is_error }` and no longer touches `AjUi`. Plumbed through
        `aj-tools::bridge::legacy_adapt` so the agent's legacy
        `ErasedToolDefinition` collection can drive it during the
        migration window. Bridge pieces: `LegacyContextBridge` adapts
        `&mut dyn legacy::SessionContext` to `&mut dyn ToolContext` (no-op
        `emit_update`, never-fired `cancellation`); `BridgedTool<T>`
        renders `ToolDetails` via the legacy `AjUi` so the CLI keeps
        printing tool results until §2.3 wires the bus to rendering.
        Legacy `ToolResult` grew `details: Option<ToolDetails>` and
        `is_error: bool` so migrated tools surface their structured
        payload through the existing agent runtime; `Agent::execute_turn`
        now reads both fields when emitting `ToolExecutionEnd` and
        computing the wire `is_error`. Errors (path-not-absolute,
        file-not-found) come back as `is_error: true` outcomes instead
        of bubbled `Err`s. `DummyToolContext` added to
        `aj-tools::testing` for unit-testing migrated tools.
  - [x] `ls` (Text). Migrated to `aj_agent::tool::ToolDefinition`;
        returns `ToolOutcome { content, details: ToolDetails::Text {
        summary, body }, is_error }` and no longer touches `AjUi`.
        `summary` is the directory path made relative to the working
        directory (matching `read_file`'s convention) with optional
        ` (recursive)` and ` ignore=[...]` markers describing the
        request mode; `body` is the formatted listing. Recoverable
        errors (path-not-absolute, missing path, not-a-directory,
        invalid glob pattern, walker IO failure) come back as
        `is_error: true` outcomes instead of bubbled `Err`s. Wired
        into `get_builtin_tools()` via `bridge::legacy_adapt(LsTool)`.
        `bin/ls.rs` updated to drive the new shape via
        `DummyToolContext`. Seven unit tests cover the happy paths
        (non-recursive, recursive, ignore patterns) and recoverable
        errors (relative path, missing path, file path, invalid glob).
  - [x] `glob` (Text). Migrated to `aj_agent::tool::ToolDefinition`;
        returns `ToolOutcome { content, details: ToolDetails::Text {
        summary, body }, is_error }` and no longer touches `AjUi`.
        `summary` is the search-root path made relative to the working
        directory (matching `read_file` / `ls` conventions) followed
        by a ` pattern=<glob>` marker so a collapsed view captures
        both the target and the search pattern. `body` is the
        formatted listing of matched entries (or a "no entries"
        notice). Recoverable errors (path-not-absolute, missing path,
        not-a-directory, invalid glob pattern, walker IO failures,
        relative-path computation failures, metadata failures) come
        back as `is_error: true` outcomes instead of bubbled `Err`s.
        Wired into `get_builtin_tools()` via `bridge::legacy_adapt(GlobTool)`.
        `bin/glob.rs` updated to drive the new shape via
        `DummyToolContext`. Seven unit tests cover the happy paths
        (extension match, recursive `**/`, no matches) and recoverable
        errors (relative path, missing path, file path, invalid glob).
  - [ ] `grep` (Text)
  - [ ] `agent` (Text / SubAgentReport)
  - [ ] `todo_read` (Todos + Text)
  - [ ] `todo_write` (Todos + Text)
  - [ ] `write_file` (Diff)
  - [ ] `edit_file` (Diff)
  - [ ] `edit_file_multi` (Diff)
  - [ ] `bash` (Bash)

### §2.3 Drive the legacy CLI off the bus

- [ ] Atomic swap: add `EventBridgeListener` + persistence listener,
      delete `self.ui.*` and `view.add_*` calls inside
      `Agent::execute_turn` in one commit.

### §2.4 Flip `Agent::run` to bus-only

- [ ] Remove `self.ui` and `&mut ConversationLog` parameter from
      `Agent`. The bus is the only output; `aj-session` is no longer
      reached from `aj-agent`.

### §2.5 Split agent loop from input loop

- [ ] `Agent::prompt` / `Agent::continue_run` API; binary owns
      readline loop, `ConversationLog`, persistence + renderer
      listeners, and replay.

### §2.6 Cleanup

- [ ] Delete `RecordingAjUi`.
- [ ] Delete `aj-ui` crate; absorb types into `aj-agent`.
- [ ] Replace `AjCli` trait impl with a plain `Renderer` struct.

## Phase 1 — `aj-next` (§4)

- [ ] Crate scaffold (`src/aj-next/`).
- [ ] Print mode (text, JSONL).
- [ ] Interactive TUI: layout slots, event pump, components.
- [ ] Selectors (model/thinking/session) and theming.

## Phase 2 — Cutover (§5)

- [ ] Behavioral parity verification for daily flows.
- [ ] Rename `aj-next` → `aj`, delete legacy `aj` crate.
- [ ] Drop `rustyline`, `termimad`, `console`.
- [ ] Remove `AjCli`, `AjCliCommon`, `cli_sub_agent`, `prompt_history`.
- [ ] Update `README.md` and `AGENTS.md`.

---

## Reconnaissance findings — `UserOutput` → `ToolDetails` migration

Empirical sample taken from `~/.aj/threads/aj/` and
`~/.aj/threads/aj-2/` (240 thread files, ~12k entries):

| Entry `type` | Count |
|---|---|
| `message` | 11,779 |
| `system_prompt` | 120 |
| `user_output` | 37 |

Subagent framing exists in real data (thread `subagent`, 381
entries spread across `agent_id` ∈ {1,2,3}); 120 thread files have
the `meta` `system_prompt` root.

`message` content blocks observed: `text`, `thinking`, `tool_use`,
`tool_result`. Roles: `user`, `assistant`.

**Of the 37 `user_output` entries, all 37 are `ToolError`.** Zero
`Notice`, `Error`, `ToolResult`, `ToolResultDiff`, `TokenUsage`, or
`TokenUsageSummary` entries appear on disk. This is a code
consequence, not coincidence: `Agent::execute_tool`'s
`recording_ui.take_recorded_outputs()` call is commented out
(`src/aj-agent/src/lib.rs:758-760`), so every successful tool path
returns `user_outputs: Vec::new()`. Only the agent-side error
construction (`UserOutput::ToolError` at `lib.rs:493` and `:518`)
ever reaches the log.

On-disk shape quirk: the outer entry tag is snake_case
(`"type": "user_output"`, from `#[serde(tag = "type",
rename_all = "snake_case")]` on `ConversationEntryKind`), but the
inner `UserOutput` variant uses serde defaults, so the variant
discriminator is PascalCase (`"ToolError": {...}`).

### Why a rewriting walker (not a serde rename)

1. `ToolError {tool_name, input, error}` → `ToolDetails::Text
   {summary, body}` with `is_error: true` is a **field-name
   rewrite**, not a tag rename. `#[serde(rename = "...")]` on
   variants and fields can't bridge `tool_name`/`input`/`error` to
   `summary`/`body`.
2. The §3 "discard `Notice`/`Error`/`TokenUsage*`" rules are no-ops
   for actual data — none exist on disk. The walker stays trivial
   for the `UserOutput` portion: 37 `ToolError` entries to rewrite,
   nothing to discard.
3. The `aj-models` rewrite already needs a one-shot
   `MessageParam` → unified `Message` walker (per
   `models-spec.md`). Adding the `UserOutput` rewrite to the same
   pass costs nothing.
4. After the walker runs we own a single, clean on-disk format
   with no lingering serde compat shims in the Rust types.

### Why no `schema_version` header

- Every existing entry is identifiable by structural pattern
  (`type` discriminator + variant body) without a version field.
- The migration is one-shot and one-way (per §3); a header would
  only matter if we expected multiple format generations
  coexisting, which is explicitly out of scope.
- `.bak` siblings cover the rollback story.

### Walker shape (for §2.0(a) when it lands)

The walker lives in `aj-session::migrate::walk_threads_dir(path)`
and runs once on first launch of the new binary against
`~/.aj/threads/<project>/*.jsonl`:

- For each `ConversationEntry` with `entry: UserOutput(ToolError {
  tool_name, input, error })`: emit `entry: ToolDetails(Text {
  summary: format!("{tool_name}: error"), body: Some(error) })`
  with `is_error: true`. Anchor on the same `parent_id`; preserve
  `id`, `timestamp`, `thread`, `agent_id`. The preceding `tool_use`
  block is reachable via `parent_id` if a future format wants the
  `call_id` correlation.
- `MessageParam` content blocks → unified `Message` (handled by
  the same pass once `aj-models` lands its rewrite; out of scope
  for §2.0(a)).
- Each migrated file gets a `.bak` sibling. Skip files that
  already have a `.bak` sibling (idempotent rerun).
