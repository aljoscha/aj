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

Spec calls for one atomic commit. Splitting in two: §2.3a does the
UI rendering swap (this commit window); §2.3b does the persistence
swap (`view.add_*` removal, log access via `Arc<Mutex<>>`). The
split is safe — no double-rendering and no dead `bus.emit` calls
either way. The plan author flagged atomicity as concerning because
of those two failure modes; staging the two halves around them
preserves the same end state.

- [x] §2.3a: `EventBridgeListener` drives UI rendering off the bus.
      Added new bus events `StreamChunk { agent_id, channel:
      StreamChannel, action: StreamAction }` (text/thinking
      streaming, bridging shape that folds into `MessageUpdate` in
      §2.4) and `TurnUsage { agent_id, usage }` (per-turn
      `TokenUsage` snapshot, folds into `AssistantMessage.usage` on
      `MessageEnd` in §2.4). `Agent::execute_turn` now emits these
      alongside the existing `Notice` / `Warning` / `Error` /
      `ToolExecutionStart` / `ToolExecutionEnd` /
      `SubAgentStart` / `SubAgentEnd` / `StreamRetry` events.
      `EventBridgeListener` (in `src/aj/src/event_bridge.rs`) holds a
      main `Box<dyn AjUi>` plus a `HashMap<usize, Box<dyn AjUi>>` of
      lazily-created `SubAgentCli`s and dispatches each event onto
      the appropriate renderer. Sub-agents share the parent's bus
      (per `docs/aj-next-plan.md` §1.6) via a new
      `Agent::set_bus(EventBus)` setter called inside
      `SessionContextWrapper::spawn_agent`, so a single registration
      on the parent covers every event in the session.
      `aj_tools::bridge::render_details_via_ui` was promoted to `pub`
      so the bridge listener can reuse the same
      `ToolDetails`-to-`AjUi` projection that the in-tree
      `BridgedTool` uses today, keeping the rendered output byte-
      identical. Inside `execute_turn`, the matching `self.ui.*`
      calls were removed: `display_error` (overload retry / parse
      error / tool-use parse error / protocol error),
      `agent_text_*` / `agent_thinking_*`, `display_token_usage`,
      and the tail `display_user_output` invocation. The
      `Agent::run` and `Agent::run_single_turn` paths still call
      `self.ui.*` for things outside the turn loop (startup notices,
      sandbox warning, conversation history replay, end-of-session
      usage summary) — those move to the binary in §2.5. The
      `AgentEvent::agent_id` accessor and the `event_protocol_tests`
      labels learned about the new `StreamChunk` / `TurnUsage`
      variants; the locked sequences pick up `TurnUsage` after every
      assistant turn.
- [x] §2.3b: Persistence listener drives `ConversationLog` writes
      off the bus. Adds a `MessagePersisted { agent_id, kind }`
      bridging event (in `aj_agent::events`) carrying a
      `PersistedMessageKind { Assistant | ToolResult | UserOutput }`
      payload — one variant per `view.add_*` call the agent used to
      make inline. The agent emits one event per write; the listener
      writes one JSONL line per emit. Wraps log access in
      `Arc<tokio::sync::Mutex<ConversationLog>>` so the agent and
      the persistence listener can share it; the agent only locks
      it briefly (linearization for inference, `latest_leaf` after
      `MessagePersisted::Assistant` to recover the freshly-appended
      `EntryId` for sub-agent anchoring) and never holds the lock
      across a `bus.emit` so it can't deadlock against the
      listener's own `lock().await`. The listener factory lives in
      `aj_agent::persistence::persistence_listener(log)` so the
      `aj` binary, the future `aj-next` binary, and tests can all
      share the same projection logic without the binary needing to
      reach into `aj-session` directly. `Agent::run`,
      `Agent::run_single_turn`, `Agent::execute_turn`, and
      `Agent::execute_tool` swap their `&mut ConversationLog`
      parameter for the shared handle; `SessionContextWrapper` does
      the same so sub-agent spawns thread the handle through to
      `sub_agent.run_single_turn`. `Agent::execute_turn` no longer
      calls `view.add_*` at all — assistant messages, the
      synthesized tool-result user message, and the legacy
      `UserOutput::ToolError` records (today the only freestanding
      user_output the agent writes) all flow out as
      `MessagePersisted` events. The legacy `make_view` helper goes
      away with them. `aj/src/main.rs` registers the listener
      alongside the existing `EventBridgeListener` (rendering)
      before any turn runs; the persistence listener returning
      `Err` aborts the run with `TurnError::Fatal`, preserving
      today's "log never more than one event behind reality"
      durability guarantee. The `event_protocol_tests` module
      registers the persistence listener in tandem with the
      recording listener and locks in the new event sequence;
      `EventLabel` grew a `MessagePersisted { agent_id, kind }`
      arm tracking only the kind discriminator so test assertions
      stay legible. Four new tests in `aj_agent::persistence::tests`
      cover the happy paths (assistant / tool-result / user-output
      writes), the empty-thread error path, and the no-op behaviour
      for non-persistence events.

### §2.4 Flip `Agent::run` to bus-only

Spec calls for one atomic commit. Splitting in two: §2.4a switches
the agent's tool driving to the new `tool::ToolDefinition` /
`tool::ToolContext` path so tool execution stops touching `AjUi`;
§2.4b strips `self.ui` and the `Arc<TokioMutex<ConversationLog>>`
parameter from the agent's outer surface and moves the readline
loop, history display, and log management to the binary. The split
mirrors §2.3a/§2.3b — §2.4a is a focused, well-tested refactor of
the tool execution path; §2.4b is the larger ownership move that
materializes the "agent is bus-only" end state.

- [x] §2.4a: Switch the agent off the legacy tool trait. The
      `tool_definitions` collection becomes
      `HashMap<String, aj_agent::tool::ErasedToolDefinition>`;
      `Agent::execute_tool` drives tools via the new trait,
      returning `ToolOutcome { content, details, is_error }`.
      `SessionContextWrapper` implements `tool::ToolContext`
      (working dir, todos, sub-agent spawn, cancellation token,
      `emit_update` no-op for now). `RecordingAjUi`, `BridgedTool`,
      `LegacyContextBridge`, the `tool_details_for_legacy` helper,
      the legacy `TurnContext` / `ToolTurnContext` impls, and the
      entire `legacy_tool` module went away.
      `aj-tools::get_builtin_tools()` returns the new
      `ErasedToolDefinition` directly via blanket `Into` impls.
      `aj-tools::testing` collapsed to just `DummyToolContext`
      (legacy `DummySessionContext` / `DummyTurnContext` /
      `DummyPermissionHandler` were dead after the migration).

      `tool::ErasedToolDefinition` gained `Clone` (the closure
      moved from `Box<...>` to `Arc<...>`) so the agent can clone
      the parent's tool list per-spawn for sub-agents without
      re-running `get_builtin_tools()`. The agent's tool execution
      loop now constructs a `ToolOutcome` for both the success
      path and the synthesized error paths (parse failure /
      bubbled-up `Err`); the wire `tool_result` block carries the
      text-flattened `outcome.content`, the bus's
      `ToolExecutionEnd` event carries `outcome.details`
      verbatim, and `MessagePersisted::UserOutput::ToolError`
      still rides alongside synthesized errors so the on-disk
      shape (37/37 freestanding `user_output` entries on disk are
      `ToolError` per the §2.0 reconnaissance) keeps matching.

      `Agent` gained a `cancellation: CancellationToken` field
      surfaced through `ToolContext::cancellation`; sub-agents
      receive a `child_token()` derived from the parent's per
      `docs/aj-next-plan.md` §1.6 so a future `Agent::cancel()`
      reaches the whole hierarchy. The `aj` binary's
      `EventBridgeListener` keeps using
      `aj_tools::bridge::render_details_via_ui` to project
      `ToolDetails` onto the legacy `AjUi`. Self-tests update
      `event_protocol_tests::PingTool` to the new trait shape and
      lock the same end-to-end event sequence the §2.3 commits
      already verified.

      The agent retains `self.ui` for run-level orchestration
      (notices, history display, readline loop, usage summary)
      and still takes `Arc<TokioMutex<ConversationLog>>` as a
      parameter — those both move out in §2.4b.
- [x] §2.4b: Remove `self.ui` and the `ConversationLog` parameter
      from `Agent`. The agent's `UI: AjUi` generic is gone — the
      struct is now plain `Agent` and the binary owns the renderer.
      The agent maintains an in-memory `Vec<MessageParam>` transcript;
      [`Agent::seed_messages`] (called once on startup) replaces the
      previous reach into [`ConversationLog::linearize`]. The
      readline loop, history display, repair-on-resume, sub-agent-
      counter seeding, system-prompt resolution, sandbox warning,
      `Model: <name>, at <url>` notice, `Context:` notice, and
      end-of-session usage summary all moved to `aj/src/main.rs`.
      The agent's API surface is now a turn-level
      [`Agent::run_turn(prompt: Option<String>)`] for the top-level
      flow and the simplified [`Agent::run_single_turn(prompt)`] for
      sub-agents (via `ToolContext::spawn_agent`).

      `aj-agent` no longer depends on `aj-session`. The persistence
      listener factory moved from `aj_agent::persistence::persistence_listener`
      to `aj_session::persistence_listener`; `aj-session` picked up
      an `aj-agent` dependency (per the §1 dependency graph) so it
      can build the listener directly. The old
      `Agent::repair_interrupted_tool_uses` helper moved to
      `aj_session::repair_interrupted_tool_uses` since it operates
      on a [`ConversationLog`]; the binary calls it on resume after
      linearizing the user thread, before seeding the agent's
      transcript. `Agent::resolve_system_prompt(&mut log)` was
      replaced by [`Agent::assemble_system_prompt`] (compute) +
      [`Agent::set_assembled_system_prompt`] (push); the binary
      decides whether to reuse the persisted prompt or freeze a
      fresh one and then pushes the final string into the agent.

      [`PersistedMessageKind::User { content }`] is new — emitted by
      both [`Agent::run_turn`] (top-level user input) and
      [`Agent::run_single_turn`] (sub-agent prompt). The session-
      side persistence listener now also observes
      [`AgentEvent::SubAgentStart`] to capture the parent thread's
      current head: the sub-agent's first
      [`AgentEvent::MessagePersisted`] (whose own thread is empty)
      anchors at that captured head; subsequent sub-agent writes
      follow the chain via
      [`ConversationLog::latest_leaf`](aj_session::ConversationLog).
      The agent no longer needs to hand a `parent_head: EntryId`
      through to the sub-agent — a benefit of the
      `AgentId`-tagged event stream — so
      [`SessionContextWrapper::spawn_agent`] dropped its log /
      parent-head fields entirely.

      The agent's `event_protocol_tests` were reworked to run in
      isolation: they no longer construct a [`ConversationLog`] or
      register a persistence listener (the agent doesn't depend on
      `aj-session`), they assert the event sequence directly off the
      bus. A new `run_turn_emits_user_message_persisted_event` test
      pins the new [`PersistedMessageKind::User`] event to the
      top-level flow. The session-side persistence-listener tests
      live in `aj_session::listener::tests` and cover the new
      sub-agent first-entry anchoring path on top of the existing
      assistant / tool-result / user-output writes.

### §2.5 Split agent loop from input loop

Spec calls for one atomic commit. Splitting in two: §2.5a does the
public API rename (this commit window); §2.5b flips
[`aj_session::replay`] to emit
[`aj_agent::events::AgentEvent`]s so the binary's resume-time
history rendering can route through the same event-bridge listener
that drives live runs. The split is safe — the API rename has no
on-the-wire or persistence implications, and the replay flip is a
self-contained additive change to `aj-session` that the binary
adopts independently.

- [x] §2.5a: `Agent::prompt(message)` / `Agent::continue_run()`
      API. The legacy `Agent::run_turn(prompt: Option<String>)`
      entry point split into two clearly-named entry points:
      [`Agent::prompt`] (append a fresh user-role text message,
      then run one assistant turn) and [`Agent::continue_run`]
      (run one assistant turn against the existing transcript
      without appending anything). The shared bracket-the-run-with-
      `AgentStart`/`AgentEnd` body lives in private
      `run_top_level_turn` / `run_top_level_turn_inner`.

      [`Agent::continue_run`] enforces the wire-layer precondition
      that the transcript must end in a user-role message
      (otherwise the model API errors with an obscure 4xx); a
      mismatched last role or an empty transcript surfaces as
      [`TurnError::Fatal`] without firing any inference. The binary
      already inspects `agent.messages().last()` to decide which
      entry point to call (per `docs/aj-next-plan.md` §2.5
      bullet 3 — "resumes from transcript when the last message is
      user/tool-result"), so the precondition acts as a
      defense-in-depth check rather than a runtime gate.

      `aj/src/main.rs`'s turn loop swapped its
      `Option<String>`-shaped branch for the typed split: a fresh
      readline result drives `agent.prompt(text).await`; the
      "continue without prompting after a user/tool-result last
      message" arm drives `agent.continue_run().await`. Same
      observable behaviour, clearer call sites. The recoverable-
      error retry path keeps the existing `force_user_input` flag
      so a turn that errored never silently re-fires inference
      against the same broken request.

      `aj_agent::event_protocol_tests` picked up three new
      [`Agent::continue_run`] tests on top of the renamed
      [`Agent::prompt`] coverage: the success path (drives one
      assistant turn from a seeded user-role last message and
      emits no `MessagePersisted::User`), the assistant-last
      rejection path, and the empty-transcript rejection path. The
      `ScriptedModel` deliberately runs out of canned scripts in
      the rejection tests, so a regression that skips the
      precondition check would panic with "ScriptedModel
      exhausted" instead of returning the expected
      [`TurnError::Fatal`].

- [x] §2.5b: Flip [`aj_session::replay`] to
      `Iterator<Item = AgentEvent>`. The cycle that blocked this
      in §2.0(a) (`aj-agent` depending on `aj-session`) was
      removed in §2.4b — `aj-session` now depends on `aj-agent`
      and the replay module constructs `AgentEvent`s directly.
      The mapping (in `src/aj-session/src/replay.rs`):

      - [`ConversationEntryKind::SystemPrompt`]: model-facing
        metadata, no event.
      - Assistant-role [`Message`]: one
        [`AgentEvent::StreamChunk`] `Start`/`Stop` pair on
        [`StreamChannel::Thinking`] for each `ThinkingBlock` /
        `RedactedThinkingBlock` (latter rendered as
        `[Redacted thinking: ...]`), then one pair on
        [`StreamChannel::Text`] for the joined visible text. Each
        `ToolUseBlock` updates an internal `tool_use_id ↦
        tool_name` map used to label the matching
        `ToolResultBlock` in the next user message.
      - User-role [`Message`]: one [`AgentEvent::ToolExecutionEnd`]
        for each `ToolResultBlock` keyed by `tool_use_id`
        (carrying the looked-up tool name; falls back to
        `"tool"` for orphaned legacy entries) plus, if the
        message also carried free text, a
        [`StreamChannel::User`] `Start`/`Stop` pair so the
        renderer paints the user-input pane.
      - [`UserOutput`]: each variant maps onto the closest live
        equivalent — [`AgentEvent::Notice`] / [`AgentEvent::Error`]
        for textual notices, [`AgentEvent::ToolExecutionEnd`] (with
        the matching [`ToolDetails`] variant) for tool-flavoured
        outputs, [`AgentEvent::TurnUsage`] for `TokenUsage`
        snapshots. [`UserOutput::TokenUsageSummary`] is
        end-of-session presentational state the binary renders
        separately on shutdown, so it has no replay event.

      A new [`StreamChannel::User`] variant was added to
      [`aj_agent::events`] for the replayed user-input pair (live
      user prompts arrive through the readline loop and never
      flow through the bus, so the channel exists exclusively for
      replay). [`event_bridge::EventBridgeListener`] in the `aj`
      binary picks up the new channel and routes its actions
      through `user_text_start` / `user_text_update` /
      `user_text_stop`, mirroring the existing Text/Thinking
      handling. The agent's `event_protocol_tests` `EventLabel`
      mapping learned the new channel name (`"user"`) so future
      regressions show up in the locked sequence labels.

      `Agent::events::AgentId` derivation in `replay()`: User
      threads emit as [`AgentId::Main`]; Subagent threads emit
      tagged with their `agent_id` as [`AgentId::Sub(n)`] so the
      bridge listener's per-agent renderer cache routes
      sub-agent activity to a fresh [`SubAgentCli`] just like a
      live run does. Meta entries (the system-prompt root) emit
      no events — they're structural framing the binary projects
      elsewhere (it freezes the prompt and hands it to the
      agent).

      The binary's [`display_conversation_history`] retired in
      favour of the new pipeline:
      `aj/src/main.rs::run_session` now keeps a clone of the
      [`Listener`] subscribed to the agent's bus and drives
      [`replay`] events through the same closure directly (not
      through the bus, which would re-fire the persistence
      listener and double-write every entry). The
      `"--- End of conversation history ---"` notice still
      bookends the replay so the user has a clear visual
      separation between historical and live output. Replay now
      runs *after* `repair_interrupted_tool_uses`, so the
      synthesized `tool_result` message a recovery walk inserts
      shows up in the user's resume display alongside everything
      else (previously the synthesis happened after rendering
      and the user never saw it).

      Tests: `replay_projects_assistant_thinking_text_and_tool_results`
      pins the seven-event sequence for a turn with
      thinking/text/tool_use/tool_result;
      `replay_skips_system_prompt_and_handles_empty_log` covers
      the meta-only case; `replay_projects_user_output_tool_error_with_is_error_flag`
      pins the legacy `UserOutput::ToolError` projection (the
      predominant on-disk shape per §2.0 reconnaissance);
      `replay_routes_subagent_entries_to_sub_agent_id` covers
      sub-agent-tagged events;
      `replay_falls_back_when_tool_use_id_is_not_tracked` covers
      the truncated-log case where a tool_result block has no
      preceding tool_use entry.

### §2.6 Cleanup

- [x] Delete `RecordingAjUi`. Already gone in §2.4a (alongside the
      `BridgedTool` / `LegacyContextBridge` removal); recorded here
      for tracking-doc completeness.
- [x] Delete `aj-ui` crate; absorb types into `aj-agent`. The `AjUi`
      trait, `AjUiAskPermission` blanket impl, and `Box<dyn AjUi>`
      forwarding impl are gone with the crate. The data shapes that
      were riding on the trait's signatures moved into a new
      `aj_agent::types` module: `TokenUsage` (carried on
      `AgentEvent::TurnUsage`), `UserOutput` (carried on
      `PersistedMessageKind::UserOutput` and on the on-disk
      `ConversationEntryKind::UserOutput` until the §3 walker rewrites
      them), and the structured-totals pair `SubAgentUsage` /
      `UsageSummary` (used by the binary's end-of-session summary).
      `aj-session`, `aj-tools`, and `aj` all dropped their `aj-ui`
      dependency; the workspace `Cargo.toml` no longer lists it.

      The `aj_tools::bridge::render_details_via_ui` rendering helper
      moved out of `aj-tools` (which is now wire-only — tool
      implementations + `get_builtin_tools()`) into the binary's
      `event_bridge.rs`. It now takes a concrete `&AjCliCommon`
      instead of `&mut dyn AjUi`; the binary calls
      `aj_tools::tools::todo::format_todo_list` directly for the
      `Todos` arm.
- [x] Replace `AjCli` trait impl with a plain `Renderer` struct.
      `AjCli` keeps its `{ history, common }` shape but the entire
      `impl AjUi for AjCli` block is gone — the struct exposes
      inherent methods directly (`display_notice`, `display_warning`,
      `display_error`, `display_token_usage_summary`,
      `agent_text_stop`, `display_tool_result_diff`,
      `get_user_input`). A new `AjCli::renderer()` returns a clone
      of the inner `AjCliCommon` for listeners that need to render
      without pulling in the readline loop.

      `SubAgentCli` (the per-sub-agent trait wrapper) is deleted
      outright: the bus listener now holds an `AjCliCommon` directly
      for the main agent and a `HashMap<usize, AjCliCommon>` of
      lazily-constructed renderers for each `Sub(n)`. The sub-agent
      flavour reuses `AjCliCommon::new(Some(format!("(sub agent
      {n})")), false, false)` — the same prefix and compact output
      profile the deleted wrapper produced — so rendered output for
      nested transcripts is byte-identical.

      `EventBridgeListener::new` now takes an `AjCliCommon` (cheaper
      than the prior `Box<dyn AjUi>`); `main.rs` builds it via
      `EventBridgeListener::new(ui.renderer())`. The two test
      binaries (`bin/test_diff.rs`, `bin/test_markdown.rs`) drive
      `AjCli`'s inherent methods directly without the trait import.
      The existing `cli_common::AjCliCommon` keeps its
      `ask_permission` helper as a concrete CLI utility (the trait
      `AjUiAskPermission` is gone but the inherent permission
      prompt stays available for future interactive needs — see §1.7
      "Permissions: dropped").

## Phase 1 — `aj-next` (§4)

- [x] Crate scaffold (`src/aj-next/`). Adds the `aj-next` package
      to the workspace with the §4 dependency set (`aj-agent`,
      `aj-conf`, `aj-models`, `aj-session`, `aj-tools`, `aj-tui`)
      plus the basic glue deps (`anyhow`, `clap`, `dotenv`,
      `serde_json`, `tokio`, `tracing`, `tracing-subscriber`).
      Lays out the directory tree exactly as `docs/aj-next-plan.md`
      §4 prescribes:

      ```
      src/aj-next/
        Cargo.toml
        src/
          main.rs            — env + arg parsing + mode dispatch
          lib.rs             — module declarations, SYSTEM_PROMPT
          cli.rs             — module declarations
          cli/
            args.rs          — clap-derived `Args` (mirrors legacy
                               `aj` flags + `--print` / `--format`),
                               `Command` (list-threads/continue/
                               models), `ModelsCommand`
            file_args.rs     — passthrough stub for `@file` expansion
          config.rs          — module declarations
          config/
            keybindings.rs   — empty stub
            theme.rs         — empty stub
            slash_commands.rs — empty stub
          persistence.rs     — empty stub
          modes.rs           — module declarations
          modes/
            print.rs         — `run(args)` stub returning a clear
                               "not yet implemented" error
            interactive.rs   — `InteractiveMode { from_args, run }`
                               stub with `bail!`
            interactive/
              layout.rs      — empty stub
              event_pump.rs  — empty stub
              footer_data.rs — empty stub
              keys.rs        — empty stub
              editor_ext.rs  — empty stub
              components.rs  — module declarations
              components/
                assistant_message.rs, user_message.rs,
                tool_execution.rs, bash_execution.rs, diff.rs,
                loader_status.rs, footer.rs, header.rs,
                model_selector.rs, thinking_selector.rs,
                session_selector.rs — empty stubs
      ```

      `main.rs` parses CLI args via [`clap::Parser`], loads
      `~/.aj/.env` (then a project `.env`) via the same precedence
      the legacy binary uses, and dispatches to the appropriate
      mode: `--print` selects `modes::print::run`, otherwise
      `InteractiveMode::from_args` + `run`. `list-threads`,
      `continue`, and `models update` route through their own
      stub handlers in `main.rs` so the dispatch surface is
      stable for the next steps to plug into.

      `SYSTEM_PROMPT` is shared with the legacy binary via
      `include_str!("../../aj/SYSTEM_PROMPT.md")` so both
      binaries embed the exact same bytes during the transition.
      The file moves into this crate at the §5 cutover.

      Per the workspace lint policy (`mod_module_files = "warn"`)
      every directory uses the sibling `foo.rs` + `foo/` layout
      rather than `foo/mod.rs`. The three `async fn` stubs that
      will actually `await` once filled in (`modes::print::run`,
      `InteractiveMode::run`, `handle_models_command`) carry a
      narrow `#[allow(clippy::unused_async)]` with a comment
      naming the await they'll grow.

      `cargo build -p aj-next`, `cargo test -p aj-next`, and
      `cargo clippy -p aj-next --all-targets` all pass clean (the
      only remaining warnings are pre-existing in `aj-agent`).
      `cargo run -p aj-next -- --help` prints the full clap help
      tree end-to-end.
- [x] Print mode (text, JSONL).
      Implemented in `src/aj-next/src/modes/print.rs`. The `print::run`
      flow mirrors the legacy `aj` binary's session setup (load
      `config.toml`, resolve model args with CLI > env > config
      precedence, build the agent + filtered tool list, open a fresh
      [`ConversationLog`]) but skips the readline loop: a single
      [`Agent::prompt`] runs the supplied prompt to completion, then
      either the JSONL listener has already streamed every event or
      the text-mode finalizer walks back through `agent.messages()`
      to print the last assistant message's visible text blocks.

      Per `docs/aj-next-plan.md` §4.2 the same agent core drives both
      output formats; the only difference is which subscriber sees
      the bus. JSON mode registers `json_event_listener`
      ([`listener_from_sync`]-wrapped, awaited inline by the bus) so
      events appear on stdout in emit order with one JSONL line per
      event; text mode registers no extra listener and relies on the
      transcript walk after `prompt` returns. Persistence runs
      alongside both via [`aj_session::persistence_listener`] so
      `aj-next --print "do X"` leaves a resumable thread on disk.

      JSON shape: the print-mode contract piggybacks on the new
      [`Serialize`] derive on [`AgentEvent`] (and supporting types
      [`AgentId`], [`StreamChannel`], [`StreamAction`],
      [`PersistedMessageKind`]). The discriminator is `"type"` for
      event variants and `"kind"` for nested enums, all
      `snake_case` per the existing serde convention. The deferred
      `MessageStart` / `MessageUpdate` / `MessageEnd` variants are
      `#[serde(skip)]`-marked because they carry an
      [`AssistantMessageEvent`] payload that doesn't yet derive
      `Serialize` and the agent doesn't emit them today; the JSONL
      listener catches the (unreachable-in-practice) serialize error,
      logs a one-liner to stderr, and continues. The shape is locked
      by `agent_event_serializes_with_internally_tagged_snake_case_shape`
      in `aj-agent::events::tests`.

      Validation: `aj-next --print` without a prompt errors with
      "requires a prompt argument"; `aj-next --print continue ...`
      bails with a clear "not yet supported" message (resume + print
      lands alongside the interactive resume work). `cargo build`,
      `cargo fmt`, `cargo test`, and `cargo clippy -p aj-next
      --all-targets` all pass clean.
- [x] Interactive TUI: layout slots, event pump, components.
      **Status:** functional MVP landed. The interactive mode wires
      end-to-end: `aj-next` (no flags) opens a [`Tui`] with a five-slot
      layout (Header / Chat / Status / Editor / Footer), subscribes a
      persistence listener and a channel-style event listener on the
      agent's bus, replays any persisted thread through the same event
      pump that handles live events, and enters a `tokio::select!` loop
      over `tui.next_event()` and the bus channel. Editor submits spawn
      `agent.prompt(text)` on a tokio task; the editor's
      `disable_submit` flag gates a second submission until the run
      ends.

      **`aj-agent`:** new [`Agent::subscribe_channel`] returns
      `(SubscriptionHandle, UnboundedReceiver<AgentEvent>)`. The
      forwarder is built on top of [`Agent::subscribe`] +
      [`listener_from_sync`] and never blocks on a slow renderer
      (the `send` returns immediately, and dropping the receiver is
      treated as a benign hangup, not a fatal listener error).

      **Theme:** `aj-next/src/config/theme.rs` builds the shared
      [`MarkdownTheme`], [`EditorTheme`], and [`SelectListTheme`]
      from `aj_tui::style` closures so every component renders
      against the same palette. `Selectors and theming` will swap
      these for a configurable palette layer.

      **Layout:** `aj-next/src/modes/interactive/layout.rs` exports
      a [`SlotIndex`] enum and a [`build_layout`] helper that
      attaches the five layout slots in canonical order and focuses
      the editor.

      **Components** (in `aj-next/src/modes/interactive/components/`):

      - `assistant_message.rs` — wraps two
        [`Markdown`](aj_tui::components::markdown::Markdown) widgets
        (text + dim/italic thinking) with append-delta and replace-
        snapshot setters so streaming `StreamChunk` events update
        in place.
      - `user_message.rs` — `> `-prefixed italic markdown widget for
        typed prompts and replayed user-thread entries.
      - `tool_execution.rs` — generic per-tool component with a
        status glyph (`…` running, `✓` ok, `✗` error), a `name(args)`
        header, and a body switched on the [`ToolDetails`] variant.
        Diff/Bash/Todos/SubAgentReport/Json all flow through this
        component.
      - `bash_execution.rs` — body-line builder for the
        [`ToolDetails::Bash`] variant (stdout + dim "STDERR:"
        header + colored exit-code row + truncation/spillover
        marker). Used by `tool_execution.rs`.
      - `diff.rs` — unified-diff renderer with a 3-line context
        window, dim equal lines, red/green +/-, and a dim "…" hunk
        separator. Used by `tool_execution.rs`.
      - `loader_status.rs` — wraps [`aj_tui::components::loader::Loader`]
        with active/idle gating so the status slot collapses to
        zero rows between turns.
      - `header.rs` / `footer.rs` — minimal stub dim text rows
        (thread id + notice; model + cwd) populated from
        [`Agent::env`] / [`Agent::model`] on startup. Richer
        per-turn refresh lands alongside `footer_data.rs` in a
        follow-up.

      **Event pump** (`aj-next/src/modes/interactive/event_pump.rs`):
      [`EventPump::handle`] receives one [`AgentEvent`] and
      mutates the layout. Lifecycle (`AgentStart`/`AgentEnd`)
      toggles the loader; streaming `StreamChunk` events update
      the in-flight assistant component; `ToolExecutionStart`/
      `ToolExecutionUpdate`/`ToolExecutionEnd` route by `call_id`
      to the matching `ToolExecutionComponent`; persisted user
      messages add `UserMessageComponent`s to the chat slot;
      notices/warnings/errors append styled `Text` rows.
      Sub-agent grouping, `TurnEnd` / `TurnUsage` / `QueueUpdate`
      consumption, and the unified `MessageStart`/`MessageUpdate`/
      `MessageEnd` variants land in follow-up steps; the variant
      arms are present so the exhaustiveness check stays alive.

      Twenty-three new unit tests cover the components and the
      layout helper. `cargo build`, `cargo test -p aj-next`,
      `cargo fmt`, and `cargo clippy -p aj-next --all-targets`
      all pass clean (the only remaining warnings are pre-existing
      in `aj-agent`).
- [ ] Selectors (model/thinking/session) and theming.

  Splitting along the same lines as §2.3a/b / §2.4a/b / §2.5a/b: each
  selector is a self-contained unit with its own commit window, and
  the theming refactor is an independent concern. Sub-bullets:

  - [x] Phase 1.4a: Slash-command infrastructure + selector overlay
        framework + thinking selector. Adds the slash-command
        catalog, dispatcher, autocomplete provider, and the first
        end-to-end selector flow.

        **`aj-models`:** [`ThinkingConfig`] gained `PartialEq + Eq`
        so the slash dispatcher's `SlashAction` enum can derive
        equality (the test suite asserts on dispatched actions
        directly).

        **`aj-agent`:** new [`Agent::default_thinking`] getter and
        [`Agent::set_default_thinking`] setter so the selector can
        read the current level on open and apply the chosen level
        on confirm. The setter takes effect on the next inference;
        in-flight turns continue with whatever they were already
        configured for.

        **`aj-next/config/slash_commands.rs`:** new module owning
        the slash-command catalog ([`BUILTIN_COMMANDS`]) and the
        submit-time dispatcher ([`dispatch`]). `BUILTIN_COMMANDS`
        carries six entries today (`/thinking`, `/model`,
        `/session`, `/clear`, `/help`, `/quit`); each has a
        description and optional argument hint surfaced in the
        editor's autocomplete pop-up. [`build_autocomplete_provider`]
        constructs a [`CombinedAutocompleteProvider`] from the
        catalog so typing `/` opens the suggestion list and
        `/thinking m` proposes `medium` and `max` via the inline
        argument completer. [`dispatch`] returns a [`SlashAction`]
        — `OpenThinkingSelector`, `SetThinking(level)`,
        `NotYetImplemented { command, message }`, `Quit`,
        `Unknown { input }`, `InvalidArgument { command, message }`
        — that the host applies. The thinking-level catalog
        ([`THINKING_LEVELS`]) is shared with the selector
        component so descriptions stay in sync.

        **`aj-next/.../components/thinking_selector.rs`:** new
        [`ThinkingSelectorComponent`] wrapping
        [`aj_tui::components::select_list::SelectList`]. The
        component pre-selects the active level on open (marked
        `(current)` so a no-op confirm is obvious), routes
        Enter/Esc through the SelectList's `on_select` / `on_cancel`
        callbacks into a shared `Arc<Mutex<Option<Outcome>>>` slot,
        and exposes [`outcome_handle()`] so the host can poll the
        outcome after each input event. Five unit tests cover
        the highlight-current-on-open, Enter-confirms, Esc-cancels,
        arrow-key-navigation, and off-is-selectable paths.

        **`aj-next/modes/interactive.rs`:** wires the slash
        catalogue into the editor on startup
        ([`Editor::set_autocomplete_provider`]) and intercepts
        `/...`-prefixed editor submissions in the main loop,
        before the normal `agent.prompt(...)` branch. A
        `OpenSelector` enum tracks the active overlay and the
        host polls its outcome handle after every TUI input
        event; on confirm the host hides the overlay via
        [`Tui::hide_overlay`] (which restores focus to the
        editor) and applies the result, on cancel it just hides
        and stays put. `/quit` breaks the main loop;
        `NotYetImplemented` and `Unknown` flow through
        [`AgentEvent::Notice`] so the existing event pump
        renders a dim status row in the chat scrollback. Slash
        dispatch runs even mid-turn so `/quit` always works;
        [`Agent::set_default_thinking`] takes a brief
        `TokioMutex` lock alongside the agent's
        prompt-running task without blocking the bus.

        Seven new unit tests in `slash_commands::tests` cover
        the dispatcher (quit, unknown, no-arg-opens-selector,
        valid levels round-trip, invalid arguments, deferred
        commands, level catalog round-trip, prefix completer).
        `cargo build`, `cargo test -p aj-next`, `cargo fmt`,
        and `cargo clippy -p aj-next --all-targets` all pass
        clean.

  - [x] Phase 1.4b: Model selector. Drives off
        [`aj_models::registry::ModelRegistry`] with an inline fuzzy
        filter (matches `id`, `provider`, and `name`), preserves the
        registry's catalog order on ties, and applies the chosen
        model via a new [`Agent::set_model`] setter.

        **`aj-agent`:** new [`Agent::set_model`] setter mirroring
        [`Agent::set_default_thinking`]. Takes effect on the next
        inference; an in-flight turn keeps running against whatever
        handle it was already using.

        **`aj-next/config/slash_commands.rs`:** `/model` no longer
        returns [`SlashAction::NotYetImplemented`]; instead it
        returns [`SlashAction::OpenModelSelector { initial_query }`]
        with no args (`initial_query == None`) or with a fuzzy
        pre-fill (`Some("sonnet")` for `/model sonnet`). The argument
        completer on the editor pop-up uses
        [`aj_tui::fuzzy::FuzzyMatcher`] against a `"provider id name"`
        haystack so typing `/model sonn` proposes every model whose
        searchable blob fuzzy-matches the query. A new
        [`load_model_catalog`] helper snapshots
        [`ModelRegistry::load`] once at startup so the autocomplete
        provider and the selector overlay share a single load
        rather than reading the JSON catalog twice.

        **`aj-next/.../components/model_selector.rs`:** new
        [`ModelSelectorComponent`] wrapping an
        [`aj_tui::components::text_input::TextInput`] (live search
        box) above an [`aj_tui::components::select_list::SelectList`]
        (results). The component owns the catalog and rebuilds the
        inner list on every text change via a reusable
        [`FuzzyMatcher`] (empty query → catalog order; non-empty →
        highest-score-first with catalog-order tiebreak). The
        currently-active model is pre-selected on open and marked
        `(current)` so a no-op confirm is visually obvious;
        Enter/Esc are intercepted at the component level so they
        route to the outcome slot without relying on either
        sub-component's callbacks. Navigation keys
        (Up/Down/PgUp/PgDn) flow to the [`SelectList`]; everything
        else flows to the [`TextInput`]. A small
        [`ModelIdentity`] / [`ModelIdentityRef`] trait pair keeps
        the component decoupled from the wire-level [`Model`]
        trait. Eight unit tests cover open-pre-selects-current,
        Enter-confirms-highlighted, Esc-cancels, down-then-Enter,
        live-filtering-on-typing, initial-query-pre-fills,
        empty-catalog-shows-no-match, and unknown-current-falls-back-
        to-first-row.

        **`aj-next/modes/interactive.rs`:** captures
        `current_model_key: Arc<Mutex<(String, String)>>` at startup
        from `ModelArgs.api` and the resolved
        `model.model_name()`, and threads it (alongside the
        snapshotted `model_catalog: Arc<Vec<ModelInfo>>`) through
        [`handle_slash_command`] and [`handle_selector_outcome`].
        [`OpenSelector`] grew a [`Model { handle, outcome }`]
        variant. On [`SlashAction::OpenModelSelector`] the host
        clones the catalog into a fresh
        [`ModelSelectorComponent`], shows it as an 80-column
        centred overlay (wider than the thinking overlay because
        ids like `claude-sonnet-4-20250514` need room), and polls
        the outcome handle after every input event. On a confirmed
        outcome the host rebuilds an [`aj_models::create_model`]
        handle from the picked [`ModelInfo`] (provider → `api`,
        base_url → `url`, id → `model_name`, speed left unset on
        selector-driven swaps), calls [`Agent::set_model`] under
        the existing [`TokioMutex<Agent>`] lock, updates
        `current_model_key` so a re-open pre-selects the new row,
        and refreshes the footer's model line with the freshly-
        active `model_name @ model_url` pair so the user has
        immediate visual confirmation of the swap.

        Speed (`Anthropic fast-inference-2025-10-02` beta) is
        intentionally not preserved across selector-driven swaps —
        users who want it pass `--speed fast` at startup. A
        follow-up could surface it as part of the selector if
        that turns out to matter in practice.

        `cargo build`, `cargo test -p aj-next`, `cargo fmt`, and
        `cargo clippy -p aj-next --all-targets` all pass clean (the
        only remaining warnings are pre-existing in `aj-agent`).

  - [x] Phase 1.4c: Session selector. Drives off
        [`aj_session::ConversationPersistence::list_thread_previews`]
        (a new richer-metadata loader added alongside the existing
        [`list_threads`] cheap path). On select, the host swaps the
        active log + agent over to the chosen thread without
        restarting the binary.

        **`aj-session`:** new [`ThreadPreview`] struct carrying
        `thread_id`, `modified: DateTime<Utc>`, `size_bytes`,
        `message_count`, and `first_user_message: Option<String>`
        — the data the interactive overlay needs to render
        recognisable rows. New
        [`ConversationPersistence::list_thread_previews(on_progress)`]
        walks the same threads-dir [`list_threads`] does, but for
        each file opens the JSONL and counts `Message` entries
        while capturing the first user-role textual block. The
        progress callback fires once per file (with `(loaded,
        total)`) so a future "Loading X/Y" indicator can wire
        directly to it without changing the signature. Pre-refactor
        files are filtered up-front so `total` reflects only
        rows the selector will actually display. Tests cover the
        happy path (count + preview), the empty-dir / pre-refactor
        edge cases, the "no user text yet" case, and the progress
        callback order.

        **`aj-next/config/slash_commands.rs`:** `/session` no
        longer returns [`SlashAction::NotYetImplemented`]; it now
        returns [`SlashAction::OpenSessionSelector { initial_query }`]
        — with `None` for the bare `/session` form, and
        `Some("fix bug")` for `/session fix bug` so the overlay
        opens already filtered. `BUILTIN_COMMANDS`' `/session`
        entry grew a `[search]` argument hint to match.

        **`aj-next/.../components/session_selector.rs`:** new
        [`SessionSelectorComponent`] wrapping a
        [`aj_tui::components::text_input::TextInput`] (live
        fuzzy search) above an
        [`aj_tui::components::select_list::SelectList`] (results).
        Mirrors the model selector's design: the component owns
        the catalog and rebuilds the inner list on every text
        change via a reusable [`aj_tui::fuzzy::FuzzyMatcher`]
        scoring against a `"<first_user_message> <thread_id>"`
        haystack. The currently-active thread is pre-selected on
        open and tagged `(current)` so a no-op confirm is
        visually obvious. Each row's primary column carries the
        first user message (truncated to 80 chars with a trailing
        `…`); the description column carries `<count> msgs ·
        <age>` where age is a coarse `now / 5m / 3h / 2d / 4w /
        6mo / 2y` bucket relative to a single `now` snapshot
        taken at construction time. `SelectList`'s default
        32-char primary column was widened via
        [`SelectListLayout::max_primary_column_width`] so long
        previews don't get truncated below my own ellipsis cap.
        Eleven unit tests cover open-pre-selects-current,
        Enter-commits-highlighted, Esc-cancels, down-then-Enter,
        live-filtering-on-typing, initial-query-pre-fills,
        empty-catalog-no-match, no-user-message-placeholder,
        ellipsis-on-very-long-previews, and the age-bucket
        boundaries.

        **`aj-next/modes/interactive.rs`:** wires the swap. The
        `_persistence_handle` binding (previously underscore-
        prefixed and immutable) became a plain `mut
        persistence_handle: SubscriptionHandle` so a successful
        swap can replace it with a fresh subscription tied to the
        new log; same shape for the `Arc<TokioMutex<ConversationLog>>`
        binding, which now lives behind `let mut log = …`.
        [`OpenSelector`] grew a [`Session { handle, outcome }`]
        variant; [`handle_slash_command`] picks up `log` +
        `conversation_persistence` parameters so it can load
        previews and pre-select the current thread on
        [`SlashAction::OpenSessionSelector`]. The overlay is
        anchored at center with `width: 80` (matches the model
        selector — long preview text benefits from the room).

        On a confirmed outcome, a new [`perform_thread_swap`]
        function does the in-process equivalent of `aj-next
        continue <id>`: it resumes the new log,
        [`repair_interrupted_tool_uses`]es it, seeds the agent's
        transcript / sub-agent counter / system prompt, drops
        the old persistence subscription in favour of a fresh one
        tied to the new log, clears the chat container, builds a
        new [`EventPump`], replays the new log through it, and
        refreshes the header. The dance is bracketed so the
        in-memory state (agent transcript + Arc) only flips
        after every fallible step has succeeded — a
        [`ConversationLog::resume`] error leaves the previous
        thread fully active. A no-op short-circuit catches the
        "user picked the row that's already current" case so the
        scrollback doesn't briefly blank out.

        Slash-command dispatch is guaranteed to be between turns
        (the editor's `disable_submit` flag blocks all submit
        events mid-turn), so the swap path doesn't need to cancel
        an in-flight `agent.prompt(...)` task.

  - [ ] Phase 1.4d: Configurable theme palette. Replace the
        hard-coded closures in `config/theme.rs` with a
        JSON-loaded palette (`vars` + `colors` semantic tokens),
        wire fs-watcher hot-reload, and surface the active level
        through the editor's border colour so `thinking` and
        `bash` modes are visually distinct.

        Splitting along the lines we've used for prior multi-part
        Phase 1 steps: this is large enough to benefit from a
        commit per axis. Sub-bullets:

    - [x] Phase 1.4d.i: JSON-loaded palette + named themes. The
          `config/theme.rs` builders now consume a [`Theme`]
          struct holding precomputed ANSI escapes per semantic
          token; the previous hard-coded closures are gone.

          **Schema:** a `vars`
          map of reusable hex / 256-color values plus a `colors`
          map keyed by 45 foreground and 6 background semantic
          tokens. Values are either explicit (`#rrggbb` hex,
          integer 0–255 for the 256-color palette, or `""` for
          terminal default) or var-references resolved
          transitively (`mdHeading: "yellow"` → `vars.yellow:
          "#9a7326"`). Cyclic refs are detected and surfaced as
          [`ThemeError::VarCycle`]; missing required tokens
          surface as [`ThemeError::MissingColor`].

          **Bundled themes:** `dark.json` and `light.json` ship
          embedded via [`include_str!`] under
          `src/aj-next/src/config/theme/`. [`Theme::bundled_dark`]
          / [`Theme::bundled_light`] return them directly with no
          fs hit; [`Theme::load(name)`] consults
          `~/.aj/themes/<name>.json` first (so a user file can
          override a bundled name), falls back to the bundled
          catalog, then falls back to dark on any parse error so
          the binary always comes up with a working palette.
          [`Theme::available`] lists bundled names plus every
          `*.json` under the user themes dir for future
          `/theme`-selector discovery.

          **Color modes:** [`ColorMode::detect`] looks at
          `$COLORTERM`, `$WT_SESSION`, `$TERM`, and
          `$TERM_PROGRAM` to pick truecolor vs 256-color. Hex
          values are downsampled to the xterm 6x6x6 cube or
          24-step grayscale ramp on 256-color terminals using
          BT.601 luma weighting;
          colors with low channel spread fall through to the
          grayscale ramp only when it's empirically closer than
          the cube pick, so a deliberate cyan / red tint isn't
          flattened to gray on limited terminals. All distance
          math is integer-only (no `as` casts) so the workspace's
          `clippy::as_conversions` lint stays satisfied.

          **Builders:** [`markdown_theme`], [`editor_theme`], and
          [`select_list_theme`] take `&Theme` and produce the
          `MarkdownTheme` / `EditorTheme` / `SelectListTheme`
          structs from `aj-tui`. The mapping uses the
          semantic tokens defined in the palette schema
          (`accent`, `muted`, `mdHeading`, `mdLink`, …). The
          editor's `border_color` uses
          [`ThemeColor::BorderMuted`] as the resting tint;
          [`aj_tui::editor_component::EditorComponent::set_border_color`]
          can swap it per-frame once the thinking-level wiring
          lands in 1.4d.iii.

          **Config integration:** [`aj_conf::Config`] grew a
          `theme: Option<String>` field consulted by
          `InteractiveMode::run` on startup; absent or unset
          defaults to `"dark"`. The interactive mode builds the
          [`Theme`] once and threads `&Theme` through
          [`build_layout`], [`EventPump::new`],
          [`handle_slash_command`], [`handle_selector_outcome`],
          and [`perform_thread_swap`].

          **Workspace deps:** `aj-next` picked up workspace deps
          on `serde` (for the `Deserialize` derives) and
          `thiserror` (for [`ThemeError`]).

          13 unit tests cover dark / light parsing, the
          truecolor and 256-color encoding paths, integer color
          values, var-ref resolution + cycle detection, missing-
          token reporting, the fall-back-to-dark path, the
          builder-produced theme closures, and the `rgb_to_256`
          neutral-vs-saturated dispatch. `cargo build`, `cargo
          test -p aj-next`, `cargo fmt`, and `cargo clippy -p
          aj-next --all-targets` all pass clean.

    - [x] Phase 1.4d.ii: fs-watcher hot-reload of the active theme
          file. The interactive mode now holds a single
          [`ThemeHandle`] (an `Arc<RwLock<Theme>>`) and threads it
          through every theme builder; a runtime swap re-points the
          inner [`Theme`] without rebuilding any component — every
          closure handed out by [`ThemeHandle::fg_closure`] /
          [`bg_closure`] resolves through the shared lock on each
          call, so changes are visible on the next render. The
          previous `&Theme`-shaped builder API is gone (all
          `markdown_theme` / `editor_theme` / `select_list_theme`
          call sites take `&ThemeHandle` now).

          **Watcher** (`config/theme.rs`): [`watch_user_theme`]
          (and the testable
          [`watch_user_theme_in_dir`](crate::config::theme::watch_user_theme_in_dir))
          spawns a `notify`-backed directory watcher over
          `~/.aj/themes/`, filters events to the requested filename,
          debounces 100 ms (matches a typical editor's
          tempfile+rename burst), re-reads the file, and emits a
          freshly-parsed [`Theme`] through an
          [`UnboundedReceiver<Theme>`]. Parse errors during mid-save
          invalid-JSON snapshots are swallowed silently and logged
          at `debug` — the running palette stays put until the next
          quiescent valid save. The directory watch (rather than a
          file-level watch) survives tempfile+rename saves which
          would otherwise drop the file-level watch's inode.

          Returns `None` for: bundled theme names with no user
          override (the file doesn't exist), missing `$HOME` (no
          themes dir), or notify-backend failures (resource
          exhaustion). The interactive mode tolerates `None` by
          folding the `recv_theme` future into `std::future::pending`,
          so the `tokio::select!` arm becomes a no-op rather than
          spinning.

          **Integration** (`modes/interactive.rs`):
          [`InteractiveMode::run`] now wraps the loaded `Theme` in a
          `ThemeHandle` at startup, kicks off the watcher (storing
          its [`ThemeWatcherGuard`] until shutdown), and adds a
          fourth `tokio::select!` arm: on each delivered theme it
          calls [`ThemeHandle::replace`] to swap the palette,
          [`Tui::invalidate`] to drop every component's cached
          render output, [`Tui::request_render`] to schedule the
          next frame, and emits an [`AgentEvent::Notice`] confirming
          the swap through the existing event pump so the user gets
          a chat-line confirmation. The new
          [`Component::invalidate`] plumbing in `aj-tui` walks the
          root + every overlay; on the next render the closures
          resolve through the new palette automatically.

          **Workspace deps:** `notify = "8.2"` added to the
          workspace and pulled into `aj-next` (runtime dep).
          `tempfile` added as a dev-dep of `aj-next` for the
          watcher integration tests.

          Six new unit tests in `config::theme::tests` cover:
          `ThemeHandle::fg_closure` paints with the new palette
          after `replace`; `ThemeHandle::name` tracks replacement;
          `watch_user_theme_in_dir` returns `None` for missing
          files; an end-to-end "write seed → start watcher → edit
          → assert delivered theme" test with a 5-second budget;
          unrelated filenames in the same directory are filtered
          out; mid-save invalid-JSON snapshots are swallowed and a
          subsequent valid save recovers. Total `aj-next` unit
          tests: 81 (up from 75). `cargo build`, `cargo test
          --workspace`, `cargo fmt`, and `cargo clippy -p aj-next
          --all-targets` all pass clean (the only remaining
          warnings are pre-existing in `aj-agent`).

    - [x] Phase 1.4d.iii: Surface the active thinking level and
          bash mode through the editor's border color. Hook the
          editor's [`set_border_color`] off the agent's
          `default_thinking` setter and any future bash-mode
          toggle so `thinking` / `bash` modes are visually
          distinct.

          **`config/theme.rs`:** new helpers
          [`editor_border_color_for_thinking`] and
          [`editor_border_color_for_bash_mode`] return an
          `Arc<dyn Fn(&str) -> String>` closure painted with the
          appropriate semantic token. The thinking helper takes
          an `Option<&ThinkingConfig>` and routes via a private
          `thinking_color_token` mapping that's locked by a
          dedicated unit test:

          | Level | Token |
          |---|---|
          | `None` (off) | `ThinkingOff` |
          | `Low` | `ThinkingLow` |
          | `Medium` | `ThinkingMedium` |
          | `High` | `ThinkingHigh` |
          | `XHigh` / `Max` | `ThinkingXhigh` |

          `XHigh` and `Max` share the highest tint because the
          theme schema (and the bundled `dark` / `light` palettes)
          tops out at `thinkingXhigh` — both represent "the
          strongest reasoning the active model supports" and the
          visual cue is the same intent. The bash-mode helper
          paints with the dedicated `BashMode` token (a vivid
          green/yellow in the bundled themes) so a future
          bash-mode toggle is instantly distinct from any
          thinking tint.

          Both closures resolve through the shared
          [`ThemeHandle`] on each call so a runtime theme reload
          (the fs-watcher arm of the main select loop) reskins
          the border automatically without re-invoking the
          helper. A `editor_border_color_picks_up_theme_replace`
          unit test pins that invariant.

          **`modes/interactive.rs`:** new
          `apply_editor_border_for_thinking` private helper
          looks up the editor slot via
          `Tui::get_mut_as::<Editor>(SlotIndex::Editor.idx())`
          and calls [`EditorComponent::set_border_color`] (the
          trait method on `aj-tui`'s editor) with the freshly-
          built closure. Wired into three call sites:

          - On startup, immediately after [`build_layout`],
            using the agent's initial `default_thinking()` so
            the TUI comes up with the correct tint.
          - In the `SlashAction::SetThinking` arm of
            [`handle_slash_command`] (the `/thinking <level>`
            short-form path that skips the overlay), after
            [`Agent::set_default_thinking`] commits the change.
          - In the [`ThinkingSelectorOutcome::Confirmed`] arm
            of [`handle_selector_outcome`] (the `/thinking`
            overlay confirm path), again after
            [`Agent::set_default_thinking`].

          Both update paths drop the brief
          `TokioMutex<Agent>` lock before touching the TUI so
          the `set_border_color` call doesn't hold the agent
          lock across an editor render-cache invalidation; the
          closure itself is constructed against a `&ThemeHandle`
          borrow that lives independently of the agent.

          The `perform_thread_swap` path doesn't update the
          border: a session swap leaves the agent's default
          thinking level untouched, so the existing closure
          stays correct. Bash mode wiring is reserved for a
          future commit alongside the `!`-prefix bash-mode
          toggle on the editor; the helper exists today so the
          mode → token mapping lives in one file once the
          toggle lands.

          Five new unit tests in `config::theme::tests` cover:
          the `thinking_color_token` mapping for every
          `ThinkingConfig` variant plus `None`, the
          paint-with-level-token happy path (`medium` →
          `#81a2be`), the off-thinking dark-gray fallback
          (`None` → `thinkingOff` → `#505050`), the
          hot-reload-through-replace invariant, and the
          bash-mode green tint (`#b5bd68`). Total `aj-next`
          unit tests: 86 (up from 81). `cargo build`, `cargo
          test -p aj-next`, `cargo fmt`, and `cargo clippy -p
          aj-next --all-targets` all pass clean (the only
          remaining warnings are pre-existing in `aj-agent`).

## Phase 2 — Cutover (§5)

- [ ] Behavioral parity verification for daily flows. Decomposed
      into focused sub-bullets, each landing as its own commit:
  - [x] Phase 2.1a: Wire `aj-next list-threads` — replace the stub
        in `main.rs::handle_list_threads` with a port of the
        legacy `aj`'s implementation. Drives off
        [`ConversationPersistence::list_threads`] and prints
        `<thread_id> (modified: <utc>, <size>)` per row, matching
        the legacy format byte-for-byte so users with scripts /
        muscle memory don't break. The empty case prints
        `"No conversation threads found for this project."` —
        same string the legacy binary uses. `cargo build`,
        `cargo test -p aj-next`, `cargo fmt`, and `cargo clippy
        -p aj-next --all-targets` all pass clean.
  - [x] Phase 2.1b: Wire `aj-next models update` — replaces the
        stub in `main.rs::handle_models_command` with a body that
        calls [`aj_models::refresh::refresh_user_cache`] and
        prints `summary.one_line()`. Required for `/model`
        selector freshness — the catalog never updates today.
        Live `cargo run -p aj-next -- models update` against an
        empty `~/.aj/models.json` populated 64 models from
        `https://models.dev/api.json` and printed the stable
        `added N models, removed M, price changes on K (total:
        T, written to <path>)` summary line, so any scripts
        watching for that line keep working. `cargo build`,
        `cargo test -p aj-next`, `cargo fmt`, and `cargo clippy
        -p aj-next --all-targets` all pass clean.
  - [x] Phase 2.1c: Startup `Context:` notice + sandbox warning.
        Ports the legacy [`display_context`] flow plus the
        `AJ_DISABLE_SANDBOX_WARNING`-gated sandbox notice into the
        `aj-next` interactive mode. Both emit through the existing
        event pump as [`AgentEvent::Notice`] / [`AgentEvent::Warning`]
        events, so the pump's pre-existing notice/warning arms paint
        them as dim chat-scrollback rows just above the editor
        without bypassing the rendering pipeline. The notices fire
        *after* the resume-time replay loop so a resumed thread's
        historical content stays on top of the scrollback, and the
        Context/sandbox rows sit closest to the active editor where
        they're easy to spot at startup.

        **`config/theme.rs` (unchanged):** no theme work — the
        warning routing through `AgentEvent::Warning` picks up the
        existing yellow style closure the pump uses for slash-
        command status feedback.

        **`modes/interactive.rs`:**
        - New private helper [`build_context_notice(env)`] mirrors
          legacy [`display_context`] byte-for-byte: emits
          `"Context: (none)"` for an empty `context_files` vector,
          otherwise a multi-line listing with one row per file
          formatted as `  - <tildified path> (<kind label>)`. The
          tildification routes through [`aj_conf::display_path`] so
          paths under `$HOME` collapse to `~/...` just like in the
          legacy binary.
        - New private helper [`warning_event(text)`] mirrors the
          existing [`notice_event(text)`] helper but constructs an
          [`AgentEvent::Warning`] (yellow style) instead of a
          notice.
        - New private helper [`sandbox_warning_enabled()`] returns
          `true` iff `AJ_DISABLE_SANDBOX_WARNING` is unset in the
          environment, exactly matching the legacy
          `std::env::var(...).is_err()` semantics — setting the var
          to *any* value (including the empty string) suppresses
          the warning.
        - New `const SANDBOX_WARNING` carries the exact 234-char
          warning text the legacy binary emits, lifted verbatim
          (the line-continuation indentation is collapsed by
          `rustc` so the at-runtime string is identical to the
          legacy version — verified with a one-shot `rustc` build).
        - Wired into [`InteractiveMode::run`] right after the
          replay loop (line 296-ish) and before the
          persistence-listener registration: one
          `pump.handle(&mut tui, &notice_event(&build_context_notice(...)))`
          call followed by a conditional
          `pump.handle(&mut tui, &warning_event(SANDBOX_WARNING))`
          gated on `sandbox_warning_enabled()`.

        Session swaps via `/session` deliberately don't re-emit
        these notices: working directory is fixed for the run so
        the context files don't change, and a freshly-swapped
        thread's replay shouldn't be cluttered with redundant
        startup banners.

        Five new unit tests in `modes::interactive::tests` cover:
        the empty-`context_files` rendering, the populated case
        (asserts on the live `$HOME` to keep the tildified-path
        assertion stable across machines), the
        `AJ_DISABLE_SANDBOX_WARNING` gate (unset / set / empty
        string), and the two event wrappers'
        [`AgentId::Main`] tagging. The env-var test
        save/restores the pre-existing value so it doesn't
        disturb other tests in the same process.

        `cargo build`, `cargo test -p aj-next`, `cargo fmt`, and
        `cargo clippy -p aj-next --all-targets` all pass clean
        (the only remaining warnings are pre-existing in
        `aj-agent`). Total `aj-next` unit tests: 91 (up from 86).
  - [x] Phase 2.1d: End-of-session usage summary + resume hint.
        Ports `display_usage_summary` + the `Thread: <id> (resume
        with: aj-next continue <id>)` line from the legacy `aj`
        binary so users can track cost and find their thread id
        on exit.

        **`aj-next/.../interactive/shutdown.rs`:** new module
        owning the end-of-session banner. Three public surfaces:

        - [`build_usage_summary(agent)`] — read the agent's
          [`Agent::accumulated_usage`] and [`Agent::sub_agent_usage`]
          and project them onto a structured [`UsageSummary`] (a
          [`SubAgentUsage`] row per agent, sub-agents sorted by
          `agent_id` for determinism, plus a `total_usage` row
          summing them). Cache-creation / cache-read counts on the
          wire [`Usage`] are `Option<u64>`; absent values flatten
          to `0` in the summary's `u64` fields. The thin
          [`build_usage_summary`] wrapper reads the parts off the
          agent; the heavy lifter
          [`build_usage_summary_from_parts`] takes a `&Usage` plus
          a `&HashMap<usize, Usage>` so unit tests can exercise
          the projection without spinning up a live `Agent`.
        - [`format_usage_summary(summary)`] — render a
          [`UsageSummary`] into the canonical multi-line block
          (`Main Agent - …` row, `Sub-agent <n> - …` rows in id
          order, trailing `TOTAL - …` row). The row shape `Input:
          A | Output: B | Cache Creation: C | Cache Read: D`
          mirrors `aj/src/cli_common.rs::format_usage_summary`
          byte-for-byte so users scripting against either binary
          see the same numbers in the same positions. No trailing
          newline — the caller (the print helper) adds one.
        - [`print_usage_summary`] / [`print_resume_hint`] —
          stdout-bound display helpers wrapping the formatters in
          [`aj_tui::style::dim`] (ANSI dim attribute) to match the
          legacy `console::style(…).dim()` presentation. Each adds
          one trailing newline so the usage block, the resume
          hint, and the user's shell prompt sit on visually
          separate rows.

        **`aj-next/modes/interactive.rs`:** the end of
        [`InteractiveMode::run`] now does, after
        [`Tui::stop`]:

        1. Lock the agent, call [`build_usage_summary`], call
           [`print_usage_summary`].
        2. Lock the log, snapshot
           `latest_leaf(ThreadFilter::USER).is_some()` plus the
           `thread_id`. If the leaf exists,
           [`print_resume_hint`] the id.

        Reading the agent + log behind their `TokioMutex`es on
        the shutdown path is deadlock-free: the in-flight
        `agent.prompt(...)` task has been aborted by then, the
        event-channel forwarder lives on its own task and
        doesn't touch these mutexes, and the persistence
        listener is no-op-and-quick when the bus is quiet.

        The "is there a leaf?" check covers both the "user
        submitted at least one prompt this session" and "we
        resumed a thread that already had content" paths in one
        shot since the persistence listener writes user
        messages inline before each `prompt` returns — the
        legacy binary's `sent_any_input` boolean would have
        ended up tracking the same state, but reading the log's
        leaf is simpler and survives `/session` swaps without
        extra bookkeeping. A fresh thread where the user quits
        before typing anything still has no leaf, so they get
        the usage banner (which will be all zeros) but no
        resume hint pointing at an effectively-empty thread.

        The shutdown banner is printed *after* [`Tui::stop`] so
        the bytes land in the user's regular shell scrollback
        rather than the alternate-screen TUI buffer that gets
        cleared on exit — same end-of-session behaviour the
        legacy binary produces.

        Print mode (`--print`) does not get this banner: a
        one-shot `aj-next --print "do X"` prints the assistant's
        answer to stdout and exits, leaving the user free to
        compose them. Adding a usage line under the answer would
        contaminate scripted output where the caller's already
        piped stdout into another process. The interactive
        binary is the only one that surfaces this — same call
        the legacy binary makes.

        Five new unit tests in `shutdown::tests` cover:
        the main-only happy path (no sub-agents → total equals
        main), the multi-sub-agent path with out-of-order
        `HashMap` inserts (asserts on sorted `agent_id` output
        and on the summed totals), the format renders the
        main-only block byte-for-byte against the legacy shape,
        the format renders sub-agent rows in order, and the
        resume-hint formatter round-trips its thread id.
        `cargo build`, `cargo test -p aj-next`, `cargo fmt`,
        and `cargo clippy -p aj-next --all-targets` all pass
        clean (the only remaining warnings are pre-existing in
        `aj-agent`). Total `aj-next` unit tests: 96 (up from 91).
  - [ ] Phase 2.1e: Prompt history bootstrap from project JSONL
        thread logs. Port the prompt-history extractor so the
        editor's up-arrow surfaces cross-session history.
  - [ ] Phase 2.1f: Per-turn `TurnUsage` rendering. Fill the
        placeholder arm in `event_pump.rs` with a dim status line
        matching the legacy `display_token_usage`.
  - [ ] Phase 2.1g (nice-to-have): `aj-next continue --print`.
        Replace the explicit bail in `print::run` with a resume
        flow that replays history through the JSONL sink and
        runs the supplied prompt against the resumed transcript.
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
