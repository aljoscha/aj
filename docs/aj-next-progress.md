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

- [x] Per-tool migration off `&mut dyn AjUi` onto `&mut dyn ToolContext`.
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
  - [x] `grep` (Text). Migrated to `aj_agent::tool::ToolDefinition`;
        returns `ToolOutcome { content, details: ToolDetails::Text {
        summary, body }, is_error }` and no longer touches `AjUi`.
        `summary` is the search-root path made relative to the working
        directory (matching `read_file` / `ls` / `glob` conventions)
        followed by a ` pattern=<regex>` marker, plus an optional
        ` include=<glob>` marker when the call narrows the file set,
        so a collapsed view captures both the target and the search
        parameters. `body` is the formatted listing of matching files
        (or a "no matches" notice). Recoverable errors (path-not-absolute,
        missing path, not-a-directory, invalid regex, invalid include
        glob, walker IO failures, metadata failures) come back as
        `is_error: true` outcomes instead of bubbled `Err`s; files the
        searcher can't read (binary, permission errors) are silently
        skipped to match historical behavior. Wired into
        `get_builtin_tools()` via `bridge::legacy_adapt(GrepTool)`.
        `bin/grep.rs` updated to drive the new shape via
        `DummyToolContext`. Eight unit tests cover the happy paths
        (regex match, include filter, no matches) and recoverable
        errors (relative path, missing path, file path, invalid regex,
        invalid include glob).
  - [x] `agent` (Text / SubAgentReport). Migrated to
        `aj_agent::tool::ToolDefinition`; returns
        `ToolOutcome { content, details: ToolDetails::SubAgentReport
        { agent_id, task, report }, is_error }` and no longer touches
        `AjUi`. The wire `content` carries the sub-agent's final
        assistant text verbatim so the parent model still reads the
        report on the wire; `details` carries the structured
        `(agent_id, task, report)` triple the renderer / persistence
        listener uses to group nested transcripts under this tool
        call. `agent_id` comes from the new
        `aj_agent::tool::SpawnedAgent` struct returned by
        `ToolContext::spawn_agent` (and the legacy
        `SessionContext::spawn_agent`); the in-tree implementation in
        `SessionContextWrapper` constructs it from the freshly-allocated
        `next_sub_agent_id()` value. Spawn failures still propagate as
        `Err` so the agent runtime keeps synthesizing a generic error
        tool_result on bubble-up, matching the legacy contract.
        `LegacyContextBridge::spawn_agent` and the dummy session /
        tool contexts in `aj-tools::testing` were updated to thread
        the new return type through. Wired into `get_builtin_tools()`
        via `bridge::legacy_adapt(AgentTool)`. Three unit tests cover
        the success path (round-trips wire content + structured
        `SubAgentReport`), the optional `description` input (kept for
        schema compat, doesn't affect the outcome), and the
        spawn-failure bubble-up path.
  - [x] `todo_read` (Todos). Migrated to `aj_agent::tool::ToolDefinition`;
        returns `ToolOutcome { content, details: ToolDetails::Todos {
        items }, is_error }` and no longer touches `AjUi`. The wire
        `content` carries the canonical text rendering of the list
        (status symbols `-`/`›`/`✓`, priority suffix, and the
        `"TODO list is empty."` marker for empty sessions) so the LLM
        reads the full snapshot without needing to know about the
        structured variant. `details::Todos { items }` carries the
        same snapshot in structured form for the renderer /
        persistence listener. Wired into `get_builtin_tools()` via
        `bridge::legacy_adapt(TodoReadTool)`. The bridge's
        `ToolDetails::Todos` arm now reuses the public
        `format_todo_list` helper from `tools::todo` so the legacy
        CLI display matches what the pre-migration code produced.
        `format_todo_list` was promoted from a module-private helper
        to a `pub` function so the bridge can share the rendering
        without coupling to internal todo state.
  - [x] `todo_write` (Todos / Text). Migrated to
        `aj_agent::tool::ToolDefinition`; returns `ToolOutcome { content,
        details: ToolDetails::Todos { items }, is_error }` and no
        longer touches `AjUi`. The wire `content` carries
        `"Updated todo list with N items.\n<formatted list>"` so the
        model still sees both the confirmation and the post-update
        rendering; `details::Todos { items }` carries the structured
        snapshot for the UI / persistence layer. The "more than one
        in-progress" validation now comes back as an `is_error: true`
        outcome with `details::Text { summary, body }` — same
        recoverable-error shape as `read_file`/`ls`/`glob`/`grep`
        validation failures — instead of bubbling an `Err`. The
        session's todo list is left untouched on validation failure.
        Wired into `get_builtin_tools()` via
        `bridge::legacy_adapt(TodoWriteTool)`. Five unit tests cover
        the empty / populated read paths, list replacement, the
        empty-list write, and the multi-in-progress validation
        rejection.
  - [x] `write_file` (Diff). Migrated to `aj_agent::tool::ToolDefinition`;
        returns `ToolOutcome { content, details: ToolDetails::Diff {
        path, before, after }, is_error }` and no longer touches `AjUi`.
        On success the wire `content` is the legacy
        `"Successfully {created|overwrote} file '{path}'"` summary so
        the model still sees a deterministic confirmation; the
        structured `Diff` carries the prior bytes as `before` (empty
        string for fresh files, which the renderer surfaces as an
        all-`+` creation diff) and the freshly-written bytes as
        `after`. Recoverable errors (path-not-absolute, IO write
        failure) come back as `is_error: true` outcomes with
        `details::Text { summary, body }` — same shape as
        `read_file`/`ls`/`glob`/`grep`/`todo_write` validation
        failures — instead of bubbling an `Err`. `execution_mode()`
        is overridden to `ExecutionMode::Sequential` per
        `docs/aj-next-plan.md` §1.3 so a batch containing it
        serializes around any other in-flight tool calls. Wired into
        `get_builtin_tools()` via `bridge::legacy_adapt(WriteFileTool)`;
        the bridge's existing `ToolDetails::Diff` arm renders through
        `display_tool_result_diff`, which handles the empty-`before`
        creation case naturally. Five unit tests cover the create /
        overwrite happy paths, the relative-path and write-failure
        recoverable errors, and lock in the `Sequential` execution
        mode.
  - [x] `edit_file` (Diff). Migrated to `aj_agent::tool::ToolDefinition`;
        returns `ToolOutcome { content, details: ToolDetails::Diff {
        path, before, after }, is_error }` and no longer touches `AjUi`.
        On success the wire `content` is the legacy `"Successfully
        replaced '<old>' with '<new>' in file '<path>'"` summary so the
        model still sees a deterministic confirmation; the structured
        `Diff` carries the file's prior bytes as `before` and the post-
        replacement bytes as `after`. Recoverable errors (path-not-
        absolute, file-not-found, read-failure, zero matches, multiple
        matches without `replace_all`, write-failure) come back as
        `is_error: true` outcomes with `details::Text { summary, body }`
        — same shape as `read_file`/`ls`/`glob`/`grep`/`todo_write`/
        `write_file` validation failures — instead of bubbling an `Err`,
        and the file is left untouched on every recoverable error path
        (the disk write only runs once validation has fully passed).
        `execution_mode()` is overridden to `ExecutionMode::Sequential`
        per `docs/aj-next-plan.md` §1.3 so a batch containing it
        serializes around any other in-flight tool calls. Wired into
        `get_builtin_tools()` via `bridge::legacy_adapt(EditFileTool)`;
        the bridge's existing `ToolDetails::Diff` arm renders through
        `display_tool_result_diff`. Seven unit tests cover the single-
        occurrence happy path, `replace_all` over multiple occurrences,
        the relative-path / missing-file / no-match / multi-match-without-
        `replace_all` recoverable errors, and lock in the `Sequential`
        execution mode.
  - [x] `edit_file_multi` (Diff). Migrated to `aj_agent::tool::ToolDefinition`;
        returns `ToolOutcome { content, details: ToolDetails::Diff {
        path, before, after }, is_error }` and no longer touches `AjUi`.
        On success the wire `content` is the legacy `"Successfully
        applied N edits to file '<path>':\n<per-edit summaries>"` so the
        model still sees a deterministic confirmation listing each
        edit's `old_string` → `new_string` mapping; the structured
        `Diff` carries the file's prior bytes as `before` and the
        post-batch bytes (after every edit applied) as `after`. Edits
        are still applied sequentially against the in-memory copy of
        the file — each edit's `old_string` is matched against the
        result of all prior edits, preserving the documented
        "subsequent edits work on the state of the file after the
        previous edit" contract. Recoverable errors (path-not-absolute,
        file-not-found, read-failure, zero matches at any step,
        multiple matches without `replace_all` at any step,
        write-failure) come back as `is_error: true` outcomes with
        `details::Text { summary, body }` — same shape as `read_file`/
        `ls`/`glob`/`grep`/`todo_write`/`write_file`/`edit_file`
        validation failures — instead of bubbling an `Err`, and the
        file is left untouched on every recoverable error path
        (the disk write only runs once every edit has fully validated,
        upholding the documented "all edits applied atomically"
        guarantee). `execution_mode()` is overridden to
        `ExecutionMode::Sequential` per `docs/aj-next-plan.md` §1.3 so
        a batch containing it serializes around any other in-flight
        tool calls. Wired into `get_builtin_tools()` via
        `bridge::legacy_adapt(EditFileMultiTool)`; the bridge's
        existing `ToolDetails::Diff` arm renders through
        `display_tool_result_diff`. Eight unit tests cover multiple
        independent edits in source order, edits whose `old_string`
        depends on a prior edit's `new_string` output, single
        `replace_all` happy path, the relative-path / missing-file /
        ambiguous-match recoverable errors, the mid-batch validation
        failure (which must leave the file untouched), and lock in
        the `Sequential` execution mode.
  - [x] `bash` (Bash). Migrated to `aj_agent::tool::ToolDefinition`;
        returns `ToolOutcome { content, details: ToolDetails::Bash {
        command, stdout, stderr, exit_code, truncated, full_output_path },
        is_error }` and no longer touches `AjUi`. The wire `content`
        keeps the legacy text shape (stdout, then `STDERR:` block, then
        a `Command failed with exit code: N` / `Command terminated by
        signal` / `Command cancelled` / `Command timed out after N
        seconds` trailer, plus an `[Output truncated; full output at
        <path>]` marker when truncation occurred) so existing prompts
        keep behaving the same way; the structured `Bash` payload
        carries the same data plus a separate stdout/stderr split for
        the renderer.

        Per `docs/aj-next-plan.md` §1.2: each stream is capped in
        memory at a 16 KiB rolling tail (`STREAM_CAP_BYTES`) using a
        `Vec<u8>` that evicts from the front once full. Both reader
        tasks tee every byte into a `tempfile::NamedTempFile`
        (prefix `aj-bash-`, suffix `.log`) so the spill always holds
        the full uncaptured output. At completion we either `keep()`
        the spill (`truncated == true`) and surface its path through
        `full_output_path`, or drop the `NamedTempFile` so its `Drop`
        unlinks the file. The wire content is additionally hard-capped
        at the legacy 35 000-char ceiling (`WIRE_CAP_BYTES`) as
        belt-and-braces — in practice the per-stream caps keep us well
        under it.

        Per §1.3 / §1.8: the child is spawned with `process_group(0)`
        so it leads its own group, and on cancel / timeout we shell
        out to `kill -9 -- -<pgid>` to SIGKILL the entire descendant
        tree (a plain `Child::kill` would leak grandchildren the shell
        forked). `stdin: null` keeps any command that reads from stdin
        from hanging on the agent's terminal. The implementation
        races `child.wait`, `cancellation.cancelled()`, a
        `sleep_until(timeout_at)` arm, and a periodic `sleep(100ms)`
        wakeup in a `tokio::select!` with `biased` ordering so
        cancellation always wins. Cancellation and timeout produce
        `is_error: true` outcomes with `exit_code: None`; non-zero
        natural exits stay `is_error: false` to match legacy behavior
        (the wire content already conveys the failure to the model).

        Per §1.3 / §6 ("tool-update streaming"): while the child runs
        the loop calls `ctx.emit_update(snapshot)` on a 100 ms leading-
        edge debounce (~10 events/s). Each snapshot is a
        `ToolDetails::Bash` partial with the in-flight stdout/stderr
        tails and the `truncated` flag set as eviction occurs;
        `exit_code` and `full_output_path` are left `None` until the
        final outcome.

        `execution_mode()` is overridden to `ExecutionMode::Sequential`
        because bash runs arbitrary commands. Wired into
        `get_builtin_tools()` via `bridge::legacy_adapt(BashTool)`; the
        bridge's existing `ToolDetails::Bash` arm renders through
        `display_tool_result` / `display_tool_error` so the legacy CLI
        keeps printing tool output until §2.3.

        Reader tasks share a `RecordingCtx` test helper that records
        every `emit_update` snapshot for assertion; the legacy
        `DummyToolContext` is reused for the simpler paths. Ten unit
        tests cover the happy paths (echo to stdout, stderr capture
        under its own header, working-directory plumbing), the failure
        paths (non-zero exit code stays `is_error: false`,
        missing-binary surfacing as exit 127), the truncation /
        spill-file path, cancellation and timeout (both must kill the
        process and return `is_error: true` with `exit_code: None`),
        the leading-edge `emit_update` debounce firing at least once
        during execution, and lock in the `Sequential` execution mode.

        `aj-tools` gained a runtime `tempfile` dependency (it was a
        dev-dep before) for the `NamedTempFile` spill helper.

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
