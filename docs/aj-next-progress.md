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
- [x] Selectors (model/thinking/session) and theming.

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

  - [x] Phase 1.4d: Configurable theme palette. Replace the
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

  - [x] Phase 1.4e: Bigger session picker + richer per-thread
        metadata. Adds creation time and last-message time to the
        row metadata, widens the overlay so the extra fields have
        room, and bumps the visible-row count so more threads fit
        on screen at once. The session selector keeps its current
        single-line row model — multi-line rows would force a
        generic [`aj_tui::components::select_list::SelectList`]
        scroll-model refactor that ripples into the thinking and
        model selectors as well, and is deliberately deferred.

        **`aj-session`:** [`ThreadPreview`] grew two
        `DateTime<Utc>` fields:

        - `created_at` — parsed from `thread_id` once at preview
          build time. The thread id is minted in
          [`ConversationLog::create`] as `%Y-%m-%d-%H-%M-%S-%3f`,
          optionally suffixed `_<N>` for intra-millisecond
          collisions; the parser strips a trailing `_<digits>`
          suffix before parsing the stem. Falls back to `modified`
          if the result doesn't parse (placeholder previews,
          hand-renamed files, future format changes).
        - `last_message_at` — captured during the JSONL walk in
          [`read_thread_preview_file`] as the largest
          `ConversationEntry.timestamp` seen on a `Message`-kind
          entry. Falls back to `modified` if no entry carries a
          timestamp (the field is `Option<DateTime<Utc>>` so logs
          predating the timestamping work can have `None` on
          every line).

        [`ThreadPreview::placeholder`] populates both new fields
        from the same fallback logic so a corrupt thread file
        still produces a structurally complete row. `modified`
        stays as-is — it remains the cheap fallback and is still
        what [`ThreadMetadata`] / the `list-threads` CLI surface.

        New unit tests in `persistence::tests` (six total):

        - `parse_thread_id_created_at_round_trips_minted_id` —
          a known stem parses back to the corresponding UTC
          instant (millisecond precision preserved).
        - `parse_thread_id_created_at_strips_collision_suffix` —
          `..._3` suffix is stripped cleanly before parse.
        - `parse_thread_id_created_at_returns_none_for_unrecognised_stem` —
          hand-renamed / future-format ids fall through to
          `None`; an `_abc` (non-digit) suffix isn't a collision
          marker.
        - `list_thread_previews_populates_created_at_from_thread_id` —
          a freshly-minted thread's `created_at` equals the
          instant its id encodes.
        - `list_thread_previews_falls_back_to_modified_for_last_message_at_when_no_entries` —
          a log with only a `SystemPrompt` entry (no `Message`-
          kind entries) sets `last_message_at == modified`.
        - `list_thread_previews_uses_largest_message_timestamp` —
          tracking the max (not the last) timestamp tolerates
          out-of-order writes.

        **`aj/.../components/session_selector.rs`:** four layout /
        rendering changes.

        - **Overlay sizing.** The [`OverlayOptions`] in the
          [`SlashAction::OpenSessionSelector`] arm of
          [`handle_slash_command`] now uses
          `width: Some(SizeValue::Percent(80.0))` with
          `min_width: Some(80)` and
          `max_height: Some(SizeValue::Percent(80.0))`. Wide
          terminals get a wide picker; narrow terminals degrade
          gracefully because [`resolve_overlay_layout`] clamps
          width to the available terminal width after the
          `min_width` floor is applied, and `max_height` keeps
          the picker from overflowing on short terminals.
        - **More visible rows.** `MAX_VISIBLE_ROWS: 8 → 16`. With
          a richer per-row description the user wants more rows
          on screen at once. Thinking and model selectors stay at
          8 — their catalogs are small and they don't carry the
          same information density.
        - **Description triplet.** `format_secondary` now
          produces `<count> msgs · created <D> · last <age>`:

          - `<count> msgs` keeps the singular / plural grammar
            (`1 msg`, `42 msgs`).
          - `created <D>` is an adaptive absolute format off
            `created_at` (new helper `format_created`):
            clock-only (`14:22`) for threads created on the same
            calendar day as the selector's `now` snapshot; month
            + day (`May 8`) for same calendar year; month + day
            + year (`May 8 2024`) for older threads.
          - `last <age>` reuses the existing `format_age` coarse
            buckets (`now / 5m / 3h / 2d / 4w / 6mo / 2y`), but
            driven off `last_message_at` instead of `modified`.

          Example row description:
          `42 msgs · created May 8 · last 5m`. If a too-narrow
          terminal can't fit the full triplet, `SelectList`'s
          existing description-end truncation kicks in — we
          don't build a custom collapse strategy.
        - **Primary column.** `PREVIEW_MAX_CHARS: 80 → 60`. The
          existing `primary_column_layout` computes
          `max_primary_column_width` as `PREVIEW_MAX_CHARS + 12`
          (preview + ` (current)` + inter-column gap) so the
          column cap follows the constant automatically. Long
          previews continue to receive a trailing `…` from
          [`truncate_for_display`], and the `(current)` suffix
          continues to fit because the layout cap already reserved
          space for it.

        Search and sort policy are unchanged: the haystack stays
        `"<first_user_message> <thread_id>"` (the thread id
        encodes the creation date, so date-prefix searches still
        work), and [`ConversationPersistence::list_thread_previews`]
        keeps returning latest-first by filename stem. Per-row
        load-progress indication and sort / group toggles in the
        overlay remain out of scope; the `on_progress` callback
        is already plumbed for the former when it becomes
        warranted.

        New unit tests in `session_selector::tests` (seven total):

        - `format_created_uses_clock_for_same_day` /
          `_uses_month_day_for_same_year` /
          `_uses_year_for_older_threads` lock the three
          `format_created` branches at deterministic synthetic
          timestamps.
        - `description_carries_msg_count_created_and_last` pins
          the full description string for a same-day thread.
        - `description_singular_msg_word_for_one_message` keeps
          the `1 msg` singular grammar.
        - `description_uses_last_message_at_not_modified_for_age`
          — a preview whose file mtime is recent but whose final
          message timestamp is hours old still renders `last 3h`
          rather than `last now`.
        - `pre_selected_current_thread_stays_visible_at_max_visible_rows_bump`
          — `MAX_VISIBLE_ROWS` bump doesn't regress
          highlight-current-on-open: with 12 threads in the
          catalog and the current pre-selection in the middle,
          the row is visible without scrolling.

        No callers of [`ThreadPreview`] outside the selector
        inspect the new fields, so the data-model change lands
        as an additive extension without touching the
        `list-threads` CLI's [`ThreadMetadata`] path. `cargo
        build`, `cargo test --workspace`, `cargo fmt`, and
        `cargo clippy -p aj-session -p aj --all-targets` all
        pass clean (the only remaining warnings are pre-existing
        in `aj-agent`).

## Phase 2 — Cutover (§5)

- [x] Behavioral parity verification for daily flows. Decomposed
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
  - [x] Phase 2.1e: Prompt history bootstrap from project JSONL
        thread logs so the editor's up-arrow surfaces
        cross-session history.

        **`aj-next/.../interactive/editor_ext.rs`:** new
        [`PromptHistory`] type and [`DEFAULT_MAX_ENTRIES`] (= 200)
        constant that bootstraps an in-memory prompt history from
        the project's JSONL thread logs and installs it into
        [`aj_tui::components::editor::Editor`]. Three public
        surfaces:

        - [`PromptHistory::bootstrap(persistence, max)`] walks
          every `*.jsonl` file in the project's threads directory
          (sorted lex = chronological since filenames are
          timestamps), parses each line, and pushes the text
          content of every top-level user-role [`MessageParam`]
          onto an internal `VecDeque<String>` capped at `max`.
          Subagent threads, assistant messages, and tool-result-
          only user messages are skipped; only "prompts the human
          typed" land in the ring. Robustness contract: a single
          unreadable file, an invalid-UTF-8 line, or an
          unparseable JSON line is skipped without aborting the
          rest of the walk — the in-memory shape survives
          paste-corrupted scratch threads where the legacy
          `~/.aj/history.txt` would have truncated.
        - [`PromptHistory::install(editor)`] pushes every retained
          entry through
          [`aj_tui::components::editor::Editor::add_to_history`]
          in oldest-first order so pressing Up surfaces the most
          recent prompt. The editor's own [`Editor::HISTORY_LIMIT`]
          (100) and consecutive-duplicate dedup apply naturally as
          entries land — over-supplying with 200 entries gives the
          editor's ring the right "100 most recent" semantics.
        - [`PromptHistory::len`], [`is_empty`], and [`iter`] for
          test introspection.

        Live submissions update the same ring through the existing
        `editor.add_to_history(&trimmed)` call in the submit branch
        of the main loop, so no separate "record" path lives in
        this module — the editor itself is the system of record
        after bootstrap.

        Why this lives in-memory rather than on a separate
        `history.txt`: the legacy binary's flat history file was
        brittle to non-UTF-8 content (a single bad paste truncated
        every entry past the corruption point on the next submit)
        and to concurrent-process clobber (two `aj` processes
        running side by side each rewrote the whole file; last
        writer wins). The JSONL conversation log we already
        maintain is per-line robust to arbitrary bytes via
        `serde_json` escaping, and each thread file is owned by
        exactly one running process — so it's the natural source
        of truth.

        **`aj-next/modes/interactive.rs`:** new bootstrap call
        right after the slash-command autocomplete provider
        install: [`PromptHistory::bootstrap`] reads
        `~/.aj/threads/<project>/` once at startup,
        [`install`] pushes everything into the freshly-built
        editor's ring before the main loop begins. The
        [`ConversationPersistence`] handle that drove the
        `/session` selector now also drives the prompt-history
        walker — no extra I/O setup. Two `aj-next` processes in
        side-by-side terminals each see their own newly-submitted
        prompts in their own ring, plus every other process's
        prompts on next launch (since both write to the JSONL log
        independently).

        Twelve new unit tests in `editor_ext::tests` cover
        chronological-order assembly, multi-file
        consolidation, invalid-UTF-8 / unparseable-JSON line skip,
        null-byte / multiline prompt round-trip, consecutive-only
        dedup, cap-at-max trimming, subagent / assistant /
        tool-result skip rules, missing-dir empty return, and a
        round-trip-through-editor test that uses
        [`Editor::handle_input`] with [`Key::up()`] to walk the
        installed ring and verify the most-recent-first surface
        order. Total `aj-next` unit tests: 107 (up from 96).
        `cargo build`, `cargo test -p aj-next`, `cargo fmt`, and
        `cargo clippy -p aj-next --all-targets` all pass clean
        (the only remaining warnings are pre-existing in
        `aj-agent`).
  - [x] Phase 2.1f: Per-turn `TurnUsage` rendering. Fills the
        placeholder arm in `event_pump.rs` with a dim
        `Token Usage - …` chat row that matches the legacy
        `display_token_usage` line byte-for-byte (modulo ANSI dim
        wrapping). The new private helper
        [`format_turn_usage_line(agent_id, usage)`] in
        `event_pump.rs` ports the `format_tokens(acc, turn)`
        closure from `aj/src/cli_common.rs::format_token_usage`
        verbatim: render `acc+turn` when the turn delta is
        nonzero, plain `acc` when it's zero (so cache hits don't
        spam `+0` columns). The main-agent row is unprefixed; a
        `(sub agent N)` tag is prepended for `AgentId::Sub(n)` to
        match the legacy `AjCliCommon::prefix` convention so the
        scrollback stays readable when sub-agents share the
        parent's bus.

        Wiring: the placeholder `TurnUsage` arm under "events
        whose UI work isn't yet wired" in `EventPump::handle`
        moved out into its own arm calling a new
        [`EventPump::append_turn_usage`] helper. The helper
        constructs the line, wraps it in
        [`aj_tui::style::dim`], and pushes it onto the chat
        container via the existing
        [`EventPump::push_chat_child`] path so the row sits in
        the same scrollback as assistant text, tool execution
        blocks, and notices — exactly where the legacy renderer
        printed it.

        Four new unit tests in `event_pump::tests`:

        - `format_turn_usage_line_emits_acc_plus_turn_for_main_agent`
          pins the first-turn shape where `accumulated == turn`,
          asserting every column gets the `acc+turn` rendering.
        - `format_turn_usage_line_drops_turn_part_when_turn_is_zero`
          locks the cache-hit case: a turn that contributes zero
          new tokens must render bare `acc` columns, never
          `acc+0`.
        - `format_turn_usage_line_prefixes_sub_agent_id` covers
          the `(sub agent N)` prefix for sub-agents.
        - `turn_usage_event_appends_one_chat_row` is the
          end-to-end check: dispatch a `TurnUsage` event through
          a real [`EventPump`] against a freshly-built layout,
          assert the chat container grew by exactly one row, and
          verify the row's rendered output carries the formatted
          line wrapped in the ANSI dim escape sequence.

        Total `aj-next` unit tests: 111 (up from 107).
        `cargo build`, `cargo test -p aj-next`, `cargo fmt`, and
        `cargo clippy -p aj-next --all-targets` all pass clean
        (the only remaining warnings are pre-existing in
        `aj-agent`).
  - [x] Phase 2.1g (nice-to-have): `aj-next continue --print`.
        Replaces the explicit bail in `print::run` with a resume
        flow that runs the supplied prompt against the resumed
        transcript.

        **`aj-next/cli/args.rs`:** the `Continue` subcommand grew a
        positional `prompt: Vec<String>` after the optional
        `thread_id`. Clap's greedy single-then-rest positional
        consumption routes the first positional into `thread_id`
        and joins the rest into `prompt` — so
        `aj-next --print continue ID hello world` parses as
        `thread_id=Some("ID"), prompt=["hello","world"]`. With no
        thread id the parser treats the first positional as
        `thread_id` (it always fills first), so the ambiguous
        case `aj-next --print continue hello` resolves to a
        "thread `hello` not found" error rather than silently
        treating it as a prompt — users who want "latest +
        prompt" should pass the thread id explicitly (typically
        from `aj-next list-threads`).

        **`aj-next/modes/print.rs`:**
        - `collect_prompt_text` now consults both `args.prompt`
          (top-level positional, populated for the no-subcommand
          path) and `Continue.prompt` (subcommand positional,
          populated for the resume path). Clap keeps them disjoint;
          the helper picks whichever is non-empty and joins with
          spaces. Both empty is still the same "requires a prompt
          argument" error as before.
        - `run` now dispatches on a `resume_request:
          Option<Option<String>>` — `None` for the fresh-thread
          path, `Some(Some(id))` for an explicit resume, and
          `Some(None)` for "resume latest" (which `bail!`s when
          the project has no threads to resume; print mode is
          one-shot so silently falling through to a fresh thread
          would surprise scripts).
        - When resuming, the run mirrors the interactive resume
          path exactly: reuse the persisted system prompt
          (cache-warm; matches the bytes the model saw on the
          prior turn, so prompt-cache hits aren't invalidated),
          seed the sub-agent counter from the log's
          `max_agent_id`, run [`repair_interrupted_tool_uses`]
          (which synthesizes tool_results for any dangling
          `tool_use` ids and warns if it did), re-linearize the
          user thread, then seed the agent's transcript via
          `Agent::seed_messages`.
        - In JSON format mode the persisted history is drained
          through the JSONL sink in [`aj_session::replay`] order
          **before** subscribing any listeners to the bus, so
          consumers see the full event trace (historical and
          live) in emit order without double-firing the
          persistence listener — the disk events are already
          there. Text mode skips the historical events: callers
          piping `aj-next --print "..."` into another process
          want a clean final answer, not the prior conversation
          re-stamped.

        End-to-end smoke (`aj-next --print --format json continue
        <thread-id> "Q"` against a real thread file): the binary
        emits the historical `stream_chunk` user/thinking events
        and `tool_execution_end` rows for the persisted tool
        calls, then the live `agent_start` / `message_persisted`
        (user prompt) / `turn_start` events, and surfaces the
        expected `agent run failed (recoverable)` error when the
        model endpoint is unreachable — confirming the resume +
        replay + prompt + persistence path all run in the right
        order. The `repair_interrupted_tool_uses` warning fires
        from the resume path so users see "resuming past N
        interrupted tool call(s); synthesizing error results"
        before the JSON stream starts.

        Five new unit tests in `print::tests` cover
        `collect_prompt_text`: top-level prompt for the
        no-subcommand path, the no-prompt error, continue +
        explicit prompt → join with spaces, continue + thread id
        but no prompt → error, and the lone-continue-positional
        ambiguity (treated as thread id). Total `aj-next` unit
        tests: 116 (up from 111). `cargo build`, `cargo test -p
        aj-next`, `cargo fmt`, and `cargo clippy -p aj-next
        --all-targets` all pass clean (the only remaining
        warnings are pre-existing in `aj-agent`).
- [x] Rename `aj-next` → `aj`, delete legacy `aj` crate. The
      legacy `src/aj/` crate (rustyline readline loop, `AjCli` /
      `AjCliCommon` / `cli_sub_agent` / `prompt_history` /
      `event_bridge` modules, plus the `test_diff` and
      `test_markdown` helper binaries) is gone. `src/aj-next/`
      was renamed to `src/aj/` and its `Cargo.toml` was retitled
      (`name = "aj"`, `[[bin]] name = "aj"`) so `cargo run -p aj`
      and `cargo install --path src/aj` produce a binary called
      `aj`. The workspace `Cargo.toml` now lists `src/aj` exactly
      once (the renamed crate).

      `SYSTEM_PROMPT.md` moved alongside the rename so the
      `include_str!` path in `src/aj/src/lib.rs` collapses from
      `../../aj/SYSTEM_PROMPT.md` to `../SYSTEM_PROMPT.md`. All
      user-facing strings referencing the old binary name were
      updated: `cli/args.rs` (`#[command(name = "aj")]`, the
      `aj continue ID prompt...` help text), the resume hint in
      `modes/interactive/shutdown.rs` (`Thread: <id> (resume
      with: aj continue <id>)`), `print::run`'s error messages
      (`aj --print requires a prompt argument`,
      `aj --print does not accept this subcommand`, etc.), the
      `/clear` slash-command "restart `aj`" notice, and the
      scratch-dir prefix in `editor_ext` tests
      (`aj-prompt-history-...`). Doc comments in
      `lib.rs`/`main.rs`/`cli.rs`/`config/*` were also retitled.
      References to the spec file (`docs/aj-next-plan.md`) stay
      intact since they're filename pointers, not binary-name
      pointers.

      `cargo build`, `cargo fmt`, `cargo test --workspace`, and
      `cargo clippy -p aj --all-targets` all pass clean (the
      only remaining warnings are pre-existing in `aj-agent`).
      `cargo run -p aj -- --help` prints `Usage: aj ...`; `cargo
      run -p aj -- list-threads` lists threads exactly like the
      legacy binary did. Total `aj` unit tests: 116 (renamed
      from the prior `aj-next` count).
- [x] Drop `rustyline`, `termimad`, `console`. `rustyline` was
      already dropped alongside the legacy `aj` crate rename. The
      remaining two go in this commit: `termimad = "0.33.0"` came
      off `src/aj-tools/Cargo.toml` (its lone usage was
      `format_todo_list`'s strike-through styling, replaced with an
      inline `\x1b[9m{content}\x1b[29m` SGR pair matching the
      `aj_tui::style::strikethrough` convention — turns off only the
      strikethrough attribute instead of resetting all colors), and
      `console = "0.15"` came off the workspace `Cargo.toml` (no
      Rust source referenced it; it was a stranded leftover from the
      legacy CLI). `cargo build` and `cargo test --workspace` pass
      clean; `cargo clippy --workspace --all-targets` produces only
      the pre-existing `clone_on_ref_ptr` warnings in `aj-agent` /
      `aj-models` / `aj-tools`. Neither crate appears in `Cargo.lock`
      anymore (no transitive consumers).
- [x] Remove `AjCli`, `AjCliCommon`, `cli_sub_agent`, `prompt_history`.
      The legacy types and modules were already deleted alongside the
      `aj` crate rename (see the §5 step 2 entry above): `AjCli`,
      `AjCliCommon`, `SubAgentCli`, `cli_sub_agent`, and the legacy
      `prompt_history` module all came out with the `src/aj/`
      directory removal. The prompt-history bootstrap was ported into
      the new binary's editor wiring as Phase 2.1e
      ([`PromptHistory`] in `src/aj/src/modes/interactive/editor_ext.rs`).
      This commit cleans up two stale doc comments that still
      referenced the removed types — the [`StreamAction`] doc in
      `src/aj-agent/src/events.rs` rewrites the "mirrors the legacy
      `agent_text_*` callback shape so the bridge listener can drive
      the existing `AjCli` rendering helpers" framing into a
      self-contained description of `Start`/`Update`/`Stop`'s
      semantics, and the [`DEFAULT_MAX_ENTRIES`] doc in
      `editor_ext.rs` drops the "same effective behaviour the legacy
      binary produced with `rustyline`'s history file" trailer in
      favour of just stating that the editor's own cap keeps the
      most recent entries. No code changes. `cargo build`, `cargo
      test -p aj -p aj-agent`, and `cargo fmt` pass clean (139 tests
      across the two crates).
- [x] Update `README.md` and `AGENTS.md`. `README.md` was
      expanded from the legacy three-line install blurb to cover
      the daily flows: interactive vs `--print` mode, the
      non-conversational subcommands (`list-threads`, `continue`,
      `models update`), the slash-command catalog
      (`/model`, `/thinking`, `/session`, `/clear`, `/help`,
      `/quit`), the `~/.aj/` layout (`.env`, `config.toml`,
      `models.json`, `themes/`, `threads/`), the model-selection
      precedence (CLI flags → env vars → `config.toml` → defaults
      with the correct kebab-case `--model-api` / `--model-url`
      / `--model-name` flag names), and the post-Phase-0 crate
      graph (`aj-models` ← `aj-agent` ← `aj-tools` ← `aj-session`
      ← `aj`, plus `aj-tui` / `aj-conf` / the two provider SDKs).

      `CLAUDE.md` (which `AGENTS.md` symlinks to) was updated to
      match: the stale `cargo run -p aj --bin test_diff` line
      came out (the `test_diff` / `test_markdown` helper binaries
      were deleted with the legacy `aj` crate rename), the model
      flags were corrected from snake_case to kebab-case, the
      lint command was changed from `cargo clippy` to
      `cargo clippy --workspace --all-targets` (matches the
      command the progress doc cites under every recent commit),
      and a new Architecture section documents the crate graph,
      the per-crate responsibilities, and the `AgentEvent` bus
      subscription model (`Agent::prompt` doesn't take a
      `&ConversationLog`; the binary registers a persistence
      listener). The Configuration & Runtime section now lists
      every file under `~/.aj/` and spells out the model-selection
      precedence chain.

      No code changes. `cargo build`, `cargo fmt`, and `cargo
      test --workspace` all pass clean (no source touched).

---

## Phase 6 — `Provider` trait + unified types migration (§ models-progress 16–18)

> **Bridge to `docs/models-progress.md`.** Steps 16/17/18 there
> describe the same migration: switch `aj-agent`'s inference loop
> off the legacy `aj_models::Model` trait, the
> `aj_models::streaming::StreamingEvent` enum, and the
> `aj_models::messages::*` wire types, and onto the unified
> [`aj_models::provider::Provider`] /
> [`aj_models::streaming::AssistantMessageEvent`] /
> [`aj_models::types::*`] surface specified in
> `docs/models-spec.md`. The Phase 0–2 work above intentionally
> left this migration for last because the binary, persistence
> layer, and test harness all needed to be built (and stabilized
> on real on-disk data) first. With cutover complete, the legacy
> wire surface is the only thing standing between us and the
> models-spec end state.
>
> **This is the active section.** Phase 0/1/2 above are done and
> intentionally not re-litigated; pick the next unchecked item
> below. Once 6.9 lands, tick steps 16/17/18 in
> `models-progress.md`.

### Goals

After Phase 6 lands:

- `aj-models::lib` carries only `pub mod` declarations and the
  shared error type. No `Model` trait, no `create_model`, no
  `ModelArgs`, no `Model`-shaped re-exports.
- `aj-models::streaming` carries only the unified
  `AssistantMessageEvent` / `AssistantMessageEventStream` types
  (plus the `DoneReason` / `ErrorReason` helpers that ride on
  them). The legacy `StreamingEvent` enum is gone.
- `aj-models::messages` is gone; its types either move into a
  renamed `aj-models::wire` module or are absorbed into
  `aj-models::types`. Decision deferred to step 6.9 — see the
  "Architectural choices" subsection.
- `aj-models::{anthropic,openai}::legacy` modules are deleted.
  The new `aj-models::anthropic::provider` and the three OpenAI
  provider impls (`openai::completions`, `openai::responses`,
  `openai::codex`) operate on `aj_models::types::*` end-to-end —
  no `crate::messages::` imports.
- `aj-agent::Agent` holds an `Arc<dyn Provider>` plus an
  `Arc<ModelInfo>` instead of an `Arc<dyn Model>`. The inference
  loop consumes `AssistantMessageEvent` (with `partial:
  AssistantMessage` snapshots on every event, per the unified
  protocol) rather than walking `StreamingEvent` deltas. Per
  `docs/models-spec.md` §2 every event already carries a
  coherent `partial: AssistantMessage` snapshot, so the agent
  doesn't have to maintain its own delta accumulator. The
  transcript stays serialized as `Vec<MessageParam>` (so on-disk
  JSONL is unchanged) and projects to
  `Vec<aj_models::types::Message>` once per inference.
- The binary's three `create_model(ModelArgs { ... })` call sites
  (`aj/src/modes/interactive.rs` startup and `/model` swap,
  `aj/src/modes/print.rs` startup) build the provider handle via
  `aj_models::registry::ModelRegistry::load()` plus
  `aj_models::provider::provider_for(&model_info.api)`.
- `ScriptedModel` is replaced by a `ScriptedProvider` implementing
  the new `Provider` trait. The demos catalog (`scripted/demos.rs`)
  ports over; `aj-agent::event_protocol_tests`,
  `aj/tests/replay_parity.rs`, and the `--scripted` CLI flag all
  drive the new provider.

### Out of scope

- **On-disk persistence format change.** `aj-session` continues to
  write `ConversationEntryKind::Message(MessageParam)` and
  `ConversationEntryKind::ToolResult { content, details }` JSONL
  exactly as today. The agent's in-memory transcript shape stays
  `Vec<MessageParam>` for ergonomics and on-disk compat. A future
  migration could replace the transcript with the tagged-union
  `Vec<aj_models::types::Message>` shape plus a one-shot walker
  like the one in `aj-session::migrate`, but that's a separate
  piece of work decoupled from the provider flip.
- **`AgentMessage` extension layer.** A future iteration may
  want a custom-message extension surface so frontends can stash
  UI-only rows (bash-execution display, compaction summaries,
  branch summaries) in the transcript without sending them to the
  model. We don't need this in Phase 6; the projection helper
  introduced in step 6.5 makes it trivial to add later by growing
  `convert_to_llm` into a user-injectable hook.
- **`transformContext` / `prepareNextTurn` / `getApiKey` hooks.**
  Reserved for later — `docs/aj-next-plan.md` §7 already flags
  them as deferred. Same shape applies: each can become an
  optional callback on `AgentOptions` without changing the inference
  loop's contract.

### Decomposition (atomic commits, in dependency order)

Each step is independent: the binary and its test suite stay
working between steps, and no step needs the next one to be useful.
The order is bottom-up: clean up the providers, build the scripted
impl, swap the agent's internals, swap the binary's call sites,
then delete the corpses. Audit details (line numbers, call sites,
import paths) are summarized inline; full audit is in the
session notes for the planning commit.

- [x] **6.1: Anthropic provider off `crate::messages`.** Already
       complete on arrival at Phase 6 — the Anthropic provider was
       written against `aj_models::types::*` from the initial
       implementation (commit `130d40e`). The provider's
       `convert_messages`, `convert_user_message`,
       `convert_assistant_message`, etc. take and return unified
       `aj_models::types::Message` / `AssistantMessage` /
       `UserMessage` shapes; the only `MessageParam` /
       `ContentBlockParam` / `Role` imports in
       `aj-models/src/anthropic/provider.rs` come from the
       `anthropic_sdk::messages` wire crate, not from
       `crate::messages`. No `crate::messages::*` references exist
       anywhere in the provider file (`rg "crate::messages"
       src/aj-models/src/anthropic/provider.rs` returns empty). The
       round-trip suite (`cargo test -p aj-models --test roundtrip`,
       50 tests) passes clean. The plan's audit referenced the
       SDK-side `anthropic_sdk::messages::MessageParam` import; that
       boundary is the right shape (`Provider` trait surface uses
       `types::*`, internals convert to/from the SDK's wire types)
       and matches `docs/models-spec.md` §6.2. The conversion
       helpers the plan suggested (`message_param_to_message` /
       `message_to_message_param`) aren't needed because the
       provider works on the unified types directly without any
       crate-level `MessageParam` intermediary.

- [x] **6.2: OpenAI providers off `crate::messages`.** Same story
       as 6.1. All three OpenAI providers
       (`openai::completions`, `openai::responses`,
       `openai::codex`) and `transform.rs` were written against
       `aj_models::types::*` from their initial implementations;
       `rg "crate::messages" src/aj-models/src/{openai,transform.rs}`
       returns matches only inside `openai::legacy` (the legacy
       `Model` trait impl, removed in step 6.9). The round-trip
       suites for chat completions, responses, codex responses, and
       cross-provider transforms all pass unchanged (covered by
       `cargo test -p aj-models --test roundtrip`, 50 tests
       total).

- [ ] **6.3: `ScriptedProvider`.** New
       `aj-models::scripted::provider` module implementing the
       `Provider` trait against `AssistantMessageEventStream`.
       Scripts continue to be authored as `Vec<ScriptStep>` but
       each step now carries an `AssistantMessageEvent` instead
       of a `StreamingEvent`; the step's `delay` semantics stay
       the same (pre-emit sleep). The demo helpers in
       `scripted/demos.rs` (`text_only`, `thinking_then_text`,
       `tool_use_then_result`, etc.) port over one by one. The
       provider also gains a `Vec<AssistantMessage>` "final
       message script" mode where it auto-generates the
       `Start`/`*Start`/`*Delta`/`*End`/`Done` event sequence
       from a static `AssistantMessage` (deltas are chunked off
       the message's text blocks at a configurable token size).
       Saves test authors from writing per-token deltas when
       they only care about the agent's behaviour on the
       finalized message — useful for the bulk of
       `event_protocol_tests`'s scripts, which set the message
       and don't care about live streaming shape.

       Legacy `ScriptedModel` stays alive in parallel for now;
       only its dependents migrate in step 6.8. The two impls
       share the demo authoring vocabulary
       (`Script::with_text_only(...)`, etc.) so we can lift
       most demos without rewriting them.

       Sub-progress:
   - [x] 6.3.i. Provider core, builder vocabulary, and
         `from_messages` auto-generation mode. Lands as
         `src/aj-models/src/scripted/provider.rs`. New types:
         [`ProviderScript`], [`ProviderScriptStep`],
         [`ExhaustedBehavior`] (mirrors the legacy variant),
         [`ScriptedProvider`] implementing the unified
         [`Provider`] trait, and [`ScriptBuilder`] threading a
         running `AssistantMessage` partial through every emitted
         event. Constructors: `new(scripts)` (defaults to
         `ExhaustedBehavior::EndTurn`), `from_event_vecs(vecs)`,
         and `from_messages(messages, chunk_size, chunk_delay)` —
         the latter is the "final message script" mode the plan
         calls out, walking each static `AssistantMessage`'s
         content blocks and synthesising the
         `Start`/`*Start`/`*Delta`/`*End`/`Done` (or `Error`)
         triplet automatically. `chunk_size == 0` short-circuits
         to a single delta per block, the path
         `event_protocol_tests` will want once it migrates in
         6.8. Builder methods: `start()`, `text_block(...)`,
         `thinking_block(...)`, `tool_call_block(...)`,
         `done(reason)`, `error(reason, error)`, each with a
         `_chunked` variant accepting a per-call `chunk_size` /
         `chunk_delay` override; `delay(d)` schedules a one-shot
         pre-emit delay for the next pushed step. Tool-call
         deltas chunk the serialized JSON so the agent's
         partial-JSON parser path stays exercised, with the
         final `ToolCallEnd` carrying the fully parsed
         arguments. `StopReason` → terminal-event mapping
         covers `Stop`/`Length`/`ToolUse` → `Done` and
         `Error`/`Aborted` → `Error` with the corresponding
         `ErrorReason`. The exhausted-`EndTurn` fallback stamps
         the requested model's `api` / `provider` / `id` onto
         the synthetic `Done` so attribution survives even when
         the queue is empty (matches the agent's expectation
         that every `AssistantMessage` carries identity fields).
         Seventeen unit tests cover: `split_chunks`'s zero /
         empty / multi-byte cases, in-order replay,
         `from_messages` event sequences (text only, chunked
         text, thinking + tool call, `Error` mapping, `Aborted`
         mapping), exhausted-EndTurn synthetic Done,
         exhausted-Panic abort, builder partial threading,
         builder Done / Error payload propagation, per-script
         dispatch across multiple `stream()` calls, the
         `stream_simple` → `stream` delegation, and tool-call
         delta chunk concatenation. The legacy `ScriptedModel`
         in `scripted.rs` stays alive untouched. `cargo build`,
         `cargo fmt`, `cargo test -p aj-models`, and `cargo
         clippy -p aj-models --all-targets` all pass (only
         pre-existing `clone_on_ref_ptr` warnings in
         `oauth/anthropic.rs` remain).
   - [x] 6.3.ii. Port the 9 named demos from
         `scripted/demos.rs` (`thinking-basic`, `thinking-long`,
         `thinking-then-tool`, `interleaved`, `tool-error`,
         `protocol-error`, `tool-use-parse-error`,
         `streaming-text`, `multi-tool`) into a new
         `scripted::provider::demos` module using
         `ScriptBuilder`. Add a `lookup` / `catalog` / `names`
         API matching the legacy one so step 6.8 can swap the
         `--scripted` CLI flag's resolver without touching the
         binary call site. Add a `catalog_demos_are_well_formed`
         test mirroring the legacy one (every demo non-empty,
         last step is `Done` or `Error`).

         Lands as `src/aj-models/src/scripted/provider/demos.rs`,
         registered via `pub mod demos;` inside `provider.rs`.
         Each demo builds its per-inference scripts via
         [`ScriptBuilder`] threaded through a small `builder()`
         helper that stamps the `scripted` / `scripted` /
         `scripted` identity tuple and the default `TEXT_CHUNK = 8`
         / `CHUNK_MS = 25` chunking onto the running partial. Text
         blocks ride on the builder defaults; thinking blocks use
         the `_chunked` variants to drop the chunk size to
         `THINKING_CHUNK = 6` (smaller chunks stress the
         collapsible-thinking widget's live update path);
         tool-call blocks use `tool_call_block_chunked(..., 0,
         Duration::ZERO)` to emit a single delta carrying the
         full serialized arguments (matches the legacy demos'
         "single tool_use event" shape so the rendering compares
         apples-to-apples). Section breaks between thinking →
         text or text → tool_use ride on `builder().delay(...)`
         which schedules a one-shot 200 ms pre-emit delay on the
         next step.

         Translation rules between protocols:
         - Legacy `StopReason::EndTurn` → [`DoneReason::Stop`].
         - Legacy `StopReason::ToolUse` → [`DoneReason::ToolUse`].
         - Legacy `StreamingEvent::ProtocolError { error }` mid-
           stream + `FinalizedMessage` → terminal
           [`AssistantMessageEvent::Error`] with
           [`ErrorReason::Error`] and an
           [`AssistantError`]`(ErrorCategory::Transient, ...)`
           payload. The unified protocol's terminal-event
           contract means the demo's preamble text comes through
           in the partial captured on the Error event; the
           agent's retry layer in 6.5 will treat the transient
           category as recoverable (same end-user effect as the
           legacy non-terminal protocol error).
         - Legacy `StreamingEvent::ToolUseParseError` → a regular
           tool-call block with `Value::Null` arguments. The
           unified protocol carries no dedicated parse-error
           event; the bash tool's input-schema validation rejects
           the null payload at execution time and the agent
           synthesizes an `is_error: true` tool result (same
           rendering path as the legacy demo).

         Four tests cover the module:
         `catalog_demos_are_well_formed` (every demo non-empty,
         every script ends in `Done` or `Error` — covers all
         scripts within a multi-script demo, not just the last,
         which is slightly stronger than the legacy test);
         `lookup_returns_some_for_known_names`;
         `lookup_returns_none_for_unknown`;
         `catalog_names_match_legacy_demo_set` (asserts the new
         catalog has the exact same name vector as
         `scripted::demos::names()` so a future drift between
         the two surfaces shows up immediately — relevant until
         6.8 swaps the binary resolver and 6.9 deletes the
         legacy library). `cargo build`, `cargo fmt`, `cargo
         test -p aj-models`, and `cargo clippy -p aj-models
         --all-targets` all pass clean (only the pre-existing
         `clone_on_ref_ptr` warnings in `oauth/anthropic.rs`
         remain).

- [x] **6.4: `LegacyProviderAdapter` (internal compat shim).**
       New `aj-models::compat` module (gated on `#[doc(hidden)]`
       and not re-exported from the crate root) housing a
       `LegacyProviderAdapter` that wraps an `Arc<dyn Model>` and
       implements `Provider`. The adapter transcodes the
       legacy `StreamingEvent` stream into
       `AssistantMessageEvent`s by holding an
       `AssistantMessage` builder it clones into each event's
       `partial:` slot (the legacy stream doesn't carry coherent
       partials, so the adapter synthesizes them).

       Lands as `src/aj-models/src/compat.rs`, registered via
       `#[doc(hidden)] pub mod compat;` in `lib.rs`. Public
       surface: [`LegacyProviderAdapter::new(Arc<dyn Model>)`]
       and the [`Provider`] impl; everything else is module-
       private.

       **Stream-side transcoder.** `Transcoder` holds the running
       `AssistantMessage` partial, the cumulative legacy `Usage`
       (re-mapped to unified `Usage` on every update so every
       emitted snapshot stays coherent), the currently-open
       content-block index, plus `started`/`terminated` flags.
       `ensure_started` lazily emits the opening
       [`AssistantMessageEvent::Start`] the first time any event
       arrives — providers that skip `MessageStart` (notably the
       legacy OpenAI Chat Completions path, which only emits
       `TextStart` / `TextUpdate`) still produce a well-formed
       prelude.

       **Per-event mapping (actual `StreamingEvent` surface).**
       - `MessageStart { message }` seeds `response_id` from the
         legacy message id and pre-seeds `partial.usage` from the
         start-event usage (Anthropic stamps a non-zero usage
         here). Emits [`Start`] via `ensure_started`.
       - `UsageUpdate { usage }` applies the cumulative delta
         via `LegacyUsage::apply_delta` and re-projects onto
         `partial.usage`. No on-wire event — matches the spec's
         "usage rides on subsequent partials" contract.
       - `TextStart` / `TextUpdate` / `TextStop` →
         [`TextStart`] / [`TextDelta`] / [`TextEnd`], maintaining
         a single open block index. `TextStart` with non-empty
         text also emits an immediate [`TextDelta`] so the
         consumer sees both the open event and the seed bytes
         (Anthropic streams the first token inside the start
         event). `TextStop` reconciles the block's accumulated
         text against the provider's authoritative final string.
       - `ThinkingStart` / `ThinkingUpdate` / `ThinkingStop` →
         the [`ThinkingStart`] / [`ThinkingDelta`] / [`ThinkingEnd`]
         analogues. `ThinkingStop` carries no final text in the
         legacy protocol; the transcoder pulls the reconciled
         text from `partial.content[idx]` so the unified
         [`ThinkingEnd::content`] field stays populated.
       - `FinalizedMessage { message }` → walks the message's
         content blocks and synthesises
         [`ToolCallStart`] / [`ToolCallEnd`] for every
         `ToolUseBlock` (the legacy protocol has no streaming
         tool-call events, so every tool call surfaces here),
         then emits [`Done`] with the rebuilt
         [`AssistantMessage`]. The final message's identity
         fields (`api` / `provider` / `model`) come from the
         caller's `ModelInfo` — the legacy `Message.model` is
         only consulted for `response_id`. Stop-reason mapping
         lives in [`map_legacy_stop_reason`]:
         `EndTurn`/`StopSequence`/`PauseTurn`/`Compaction`/`None`
         → `(Stop, DoneReason::Stop)`; `MaxTokens` → `Length`;
         `ToolUse` → `ToolUse`; `Refusal` and
         `ModelContextWindowExceeded` map to
         `(Stop, DoneReason::Stop)` since the legacy agent
         already treated them as ordinary finalized messages.
       - `Error { error }` → terminal
         [`AssistantMessageEvent::Error`] with category from
         [`classify_legacy_api_error`] (AuthenticationError /
         PermissionError → `Auth`; RateLimitError → `RateLimit`;
         OverloadedError → `Overloaded`; GatewayTimeoutError →
         `Transient`; InvalidRequestError / NotFoundError /
         BillingError → `InvalidRequest`; ApiError → `Unknown`).
       - `ParseError` / `ProtocolError` → terminal Error with
         `Transient` category. Legacy semantics were non-fatal
         diagnostics, but the unified protocol only has terminal
         Error; surfacing as Transient lets the agent's retry
         layer treat them as recoverable (same end-user effect as
         the legacy "log and keep streaming" path, since the
         next inference re-issues the call).
       - `ToolUseParseError { id, name, error, raw_data }` →
         synthesises [`ToolCallStart`] / [`ToolCallEnd`] with
         `arguments: Value::Null`. The agent's per-tool
         input-schema validation rejects the null payload at
         execution time and synthesises an `is_error: true`
         tool_result, matching the legacy agent's "resurrect
         tool_use, paired error result" behaviour without
         needing a dedicated unified event variant.

       **Unterminated streams.** When the legacy stream closes
       without `FinalizedMessage` or `Error`, the driver
       synthesises a terminal [`AssistantMessageEvent::Error`]
       with `Transient` category so consumers awaiting
       `result()` always see a typed event. Matches the spec
       contract that the stream terminates with exactly one of
       `Done` or `Error`.

       **Unified → legacy projection.** Used to feed the wrapped
       `Model::run_inference_streaming`:
       - `Message::User` → `MessageParam { role: User }` with
         `UserContent::Text` → `TextBlock` and
         `UserContent::Image` → `ImageBlock { source: Base64 }`.
       - `Message::Assistant` → `MessageParam { role: Assistant }`
         with `AssistantContent::Text` → `TextBlock` (carrying
         `text_signature`), `AssistantContent::Thinking` →
         `ThinkingBlock` / `RedactedThinkingBlock` (selected by
         the `redacted` flag, signature on
         `thinking_signature`), `AssistantContent::ToolCall` →
         `ToolUseBlock`.
       - `Message::ToolResult` → user-role `MessageParam`
         carrying a single `ToolResultBlock` whose `content` is
         `ToolResultContent::Blocks(...)` projected from
         `UserContent`. Symmetric with the wire format the
         legacy providers already round-trip on input.
       - Tools project field-by-field (`name`,
         `description`, `parameters` → `input_schema`,
         `r#type: None`).
       - `SimpleStreamOptions::reasoning` → `ThinkingConfig` via
         [`map_thinking_level_to_legacy`]: Minimal / Low → Low;
         Medium → Medium; High → High; XHigh → XHigh. The
         legacy enum has no Minimal rung; mapping to Low keeps
         thinking enabled on Anthropic for the smallest-rung
         case.

       **Identity stamping.** The wrapped `Model::model_name()` /
       `model_url()` are intentionally ignored: the adapter
       stamps `api` / `provider` / `model` on every emitted
       partial / terminal message from the caller's `ModelInfo`,
       matching how the concrete providers behave. `response_id`
       is sourced from the legacy message id when non-empty.

       **Spawned task.** The legacy `run_inference_streaming` is
       async and returns an owned stream; we drive it from a
       spawned tokio task so [`Provider::stream`] can return
       synchronously like the real provider impls do. The setup
       call's `Result<_, ModelError>` is surfaced as an
       immediate terminal `Error { Transient }` on the consumer
       stream so callers always see a well-formed stream.

       Ten unit tests cover the surface:
       - `finalized_text_only_message_emits_start_and_done` pins
         the Start → Done sequence on a single-text-block
         finalized message, including stamped identity / usage /
         response_id round-trip.
       - `finalized_tool_use_synthesises_tool_call_events`
         verifies the `ToolCallStart` / `ToolCallEnd`
         synthesis path and the `DoneReason::ToolUse` /
         `StopReason::ToolUse` projection.
       - `streaming_text_deltas_round_trip` exercises
         `TextStart` / `TextUpdate` / `TextStop` and asserts the
         partial's running text matches the diffs, plus the
         terminal `Done` reconciles to the provider's
         authoritative final text.
       - `legacy_api_error_maps_to_unified_error` confirms an
         `OverloadedError` lands on
         `AssistantMessageEvent::Error` with
         `ErrorCategory::Overloaded`.
       - `tool_use_parse_error_synthesises_null_tool_call`
         covers the parse-error recovery path.
       - `unterminated_legacy_stream_synthesises_transient_error`
         pins the safety-net error when the wrapped model
         closes the stream without a terminal event.
       - `stream_simple_passes_thinking_to_legacy_model` is a
         recording-model test confirming the
         `ThinkingLevel` → `ThinkingConfig` projection reaches
         the wrapped model.
       - `tool_result_message_projects_to_user_role_tool_result_block`
         pins the structural shape of the unified → legacy
         projection for tool results.
       - `thinking_round_trip_preserves_signature` exercises
         both projection directions for thinking content,
         making sure `thinking_signature` round-trips.
       - `tool_call_projection_drops_caller_field` pins the
         caller-field handling on both sides of the projection.

       `cargo build`, `cargo test -p aj-models compat`,
       `cargo test --workspace`, `cargo fmt`, and
       `cargo clippy -p aj-models --all-targets` all pass clean
       (only pre-existing `clone_on_ref_ptr` warnings in
       `oauth/anthropic.rs` remain — none in `compat.rs`).

       The adapter is the bridge that lets step 6.5 (Agent
       runs on `Provider`) land before steps 6.6–6.8 (binary
       call sites + scripted + sub-agents) finish migrating. By
       step 6.9 every adapter call site is gone and the module
       is deleted along with the legacy types it bridges.

- [x] **6.5: `Agent` inference loop on `Provider`.** Replace
       `Agent::model: Arc<dyn Model>` with
       `Agent::provider: Arc<dyn Provider>` plus
       `Agent::model_info: Arc<ModelInfo>`. The inference loop in
       `aj-agent/src/lib.rs` (roughly lines 579–733 today)
       rewrites to consume `AssistantMessageEvent` and project
       tool calls off the finalized `AssistantMessage.content`
       (filtering for `ToolCall` blocks). Per inference, the
       agent calls a new private helper
       `agent::projection::transcript_to_messages(transcript:
       &[MessageParam]) -> Vec<aj_models::types::Message>` to
       project the transcript, then builds a `Context` and
       `StreamOptions` and calls `provider.stream_simple(...)`.

       The streaming loop replaces today's `StreamingEvent`
       match (lib.rs:579-733) with the unified match:
       - `Start { partial }` — initialize the in-progress
         `AssistantMessage`. No on-bus event yet; the existing
         `StreamChunk { channel: ..., action: Start }` events
         fire on the first `*Start` event for each channel.
       - `TextStart` / `TextDelta` / `TextEnd` — emit
         `StreamChunk { channel: Text, action: ... }`.
       - `ThinkingStart` / `ThinkingDelta` / `ThinkingEnd` —
         emit `StreamChunk { channel: Thinking, action: ... }`.
       - `ToolCallStart` / `ToolCallDelta` / `ToolCallEnd` —
         update an in-flight `ToolDetails` snapshot per call
         (live argument echo). The existing `ToolExecutionStart`
         event fires on the first delta, `ToolExecutionUpdate`
         on subsequent ones, `ToolExecutionEnd` on the End.
       - `Done { reason, message }` — finalize. Build a
         `MessageParam` from `message`'s blocks, append it to
         the transcript, emit `MessagePersisted::Assistant`,
         then drive the tool batch off `message.content`'s
         `ToolCall` blocks.
       - `Error { reason, error }` — `TurnError::Recoverable`
         or `TurnError::Aborted` depending on `reason`.

       `Agent::new(...)` keeps its public signature (still takes
       `Arc<dyn Model>`) but immediately wraps it in
       `LegacyProviderAdapter` from step 6.4, plus constructs a
       synthetic `ModelInfo` from `model.model_name()` /
       `model.model_url()` so the new internal state is
       complete. `Agent::set_model(...)` does the same. This
       lets the binary and tests keep passing legacy-shaped
       handles for one more step.

       Per-turn `TurnUsage` events continue to fire from the
       finalized message's `usage` field (which the unified
       `AssistantMessage` carries directly per the unified
       protocol).

       Tests: `aj-agent::event_protocol_tests` continues to
       drive `ScriptedModel` through the adapter. The locked
       event sequences must not regress — that's the contract
       the existing `EventLabel`-stamped snapshot tests
       enforce, and it's the strongest signal that the
       provider flip didn't shift any observable agent
       behaviour.

       **Implementation notes.** Lands as a single commit. The
       `Agent` struct gains `provider: Arc<dyn Provider>` and
       `model_info: Arc<ModelInfo>` alongside the existing
       `model: Arc<dyn Model>` field (the legacy slot stays
       alive through step 6.7 because `Agent::new` /
       `Agent::set_model` still take `Arc<dyn Model>` and
       `SessionContextWrapper::spawn_agent` threads the legacy
       handle into the sub-agent's `Agent::new`). `Agent::new`
       and `Agent::set_model` wrap the legacy handle in
       [`LegacyProviderAdapter`] (from step 6.4) and synthesise a
       minimal [`ModelInfo`] via a new `synthetic_model_info`
       helper that stamps `"legacy"` onto the api/provider slots
       and pulls `id` / `base_url` off
       [`Model::model_name`] / [`Model::model_url`].
       `Agent::model()` and `Agent::env()` accessors keep their
       legacy signatures; the field rename to
       `model_info()` lands in 6.7.

       **Inference loop rewrite.** The old `run_inference_streaming`
       returned `Pin<Box<dyn Stream<Item = StreamingEvent>>>` after
       awaiting `Model::run_inference_streaming(...)`. The new
       implementation is synchronous — it builds a
       [`Context`] (system prompt + projected `Vec<Message>` +
       projected `Vec<ToolDefinition>`) and a
       [`SimpleStreamOptions`] (default base + the legacy
       `ThinkingConfig` mapped to a unified [`ThinkingLevel`] via
       `thinking_config_to_level`), then calls
       [`Provider::stream_simple`] directly and returns the
       resulting [`AssistantMessageEventStream`]. The unified
       event protocol's events drive the new match in
       `execute_turn`:

       - `Start { partial }` is a no-op (the renderer opens its
         assistant slot on the first content-block event below).
       - `TextStart` / `TextDelta` / `TextEnd` map to
         `StreamChunk { Text, Start | Update | Stop }`. The
         unified protocol carries no text on `TextStart` itself
         (initial bytes ride on a follow-up `TextDelta`), so
         `Start { snapshot }` always carries an empty string and
         the renderer appends as it goes.
       - `ThinkingStart` / `ThinkingDelta` / `ThinkingEnd` map to
         `StreamChunk { Thinking, ... }` analogously, with the
         `ThinkingEnd { content }` payload riding on the
         `Stop { snapshot }` (a forward-looking improvement on
         the legacy `ThinkingStop` which always emitted an empty
         snapshot — the renderer ignores the value anyway).
       - `ToolCallStart` / `ToolCallDelta` / `ToolCallEnd` are
         currently no-ops: tool calls are collected off the
         finalized `Done { message }` payload below, then driven
         by the existing per-tool `ToolExecutionStart` /
         `ToolExecutionEnd` bracket. The match arm exists so the
         hooks can attach later without touching the loop shape.
       - `Done { message }` captures the finalized
         [`AssistantMessage`] and breaks out of the streaming
         loop.
       - `Error { reason, error }` captures the error-shaped
         [`AssistantMessage`] (with `stop_reason: Error` /
         `Aborted` and `error: Some(...)`) and breaks out.

       **Error / retry handling.** On a terminal `Error`, the
       loop reads `error.error.category`. If it's
       [`ErrorCategory::Overloaded`], the existing
       `create_retry_strategy` exponential backoff kicks in
       (matching the legacy `is_overloaded()` retry path).
       Other categories — including the legacy
       `RateLimit`/`Transient`/`InvalidRequest`/etc. paths,
       client `Aborted`, and the synthesized "stream ended
       without a terminal event" Transient — surface as
       `TurnError::Recoverable(anyhow!(error.message))` so the
       binary keeps the session alive and the user can
       re-prompt. The `StreamRetry` event payload is unchanged.

       **Tool calls + transcript shape.** Tool calls are
       collected off `response.content` by filtering for
       [`AssistantContent::ToolCall`] variants. The tuple shape
       `(id, name, arguments)` matches the legacy code so the
       rest of the execution loop is byte-for-byte identical.
       The finalized [`AssistantMessage`] is re-projected onto
       the agent's [`MessageParam`] transcript via the new
       `crate::projection::assistant_message_to_message_param`
       helper before the `MessagePersisted::Assistant` event
       fires — preserving the existing JSONL on-disk format
       without forcing a `Vec<Message>` migration of the
       agent's transcript. Per-turn usage rides on
       `response.usage` (unified [`Usage`]) and is mapped to
       the legacy [`aj_models::messages::Usage`] via a new
       `usage_unified_to_legacy` helper so
       `session_state.accumulated_usage.add_usage(...)` and the
       `TurnUsage` event payload keep their existing shapes.

       **Tool input parse errors.** The legacy `StreamingEvent::ToolUseParseError`
       path that synthesised a `ContentBlock::ToolUseBlock { input: Value::Null }`
       and pre-populated `tool_use_parse_failures` is gone. The
       [`LegacyProviderAdapter`] from step 6.4 already emits a
       full [`AssistantContent::ToolCall`] block with
       `arguments: Value::Null` for the parse-failure case, so
       the agent's existing tool-execution error path
       (deserializer rejects null → tool returns `Err` →
       `MessagePersisted::UserOutput::ToolError` + synthetic
       error outcome) covers the same end-to-end behaviour
       without a dedicated upstream parse-error variant. The
       `tool_use_parse_errors` / `tool_use_parse_failures`
       state and the inline `if let Some(parse_err) =
       tool_use_parse_failures.remove(&tool_id) { ... }` branch
       both drop.

       **Transcript projection.** New `aj-agent::projection`
       module (private to the crate) ships three helpers:
       `transcript_to_messages(transcript: &[MessageParam]) ->
       Vec<Message>` walks each [`MessageParam`] and splits
       user-role entries into per-block
       [`Message::ToolResult`] rows (one per
       [`ContentBlockParam::ToolResultBlock`]) plus a coalesced
       [`Message::User`] for the remaining text / image blocks;
       `assistant_message_to_message_param(&AssistantMessage) ->
       MessageParam` is the inverse projection for finalized
       assistant messages; `usage_unified_to_legacy(&Usage) ->
       LegacyUsage` maps the per-turn counters. Eight unit
       tests cover text round-trip, assistant text projection,
       tool_use / tool_result split, mixed text-then-tool-result
       ordering, signed and redacted thinking round-trip, image
       block projection, and usage mapping.

       **Test contract preserved.** All five
       `event_protocol_tests` (`run_single_turn_with_tool_call_emits_locked_protocol`,
       `tool_result_persistence_event_carries_structured_details_by_id`,
       `run_single_turn_brackets_with_agent_lifecycle`,
       `prompt_emits_user_message_persisted_event`,
       `continue_run_drives_existing_transcript_without_appending`)
       continue to drive `ScriptedModel` (legacy) through the
       [`LegacyProviderAdapter`] wrap in `Agent::new`. The
       locked event sequences pass unchanged, which is the
       strongest evidence the provider flip didn't shift any
       observable agent behaviour. `cargo build`, `cargo fmt`,
       `cargo test --workspace`, and `cargo clippy -p aj-agent
       --all-targets` all pass clean (only pre-existing
       `clone_on_ref_ptr` warnings in `bus.rs` and
       `oauth/anthropic.rs` remain — none in the touched
       files).

- [x] **6.6: Binary builds `Provider` directly.** Replace the
       three `create_model(ModelArgs {...})` call sites (two in
       `aj/src/modes/interactive.rs` — startup line 142, /model
       swap line 1033 — and one in `aj/src/modes/print.rs` line
       147) with a unified `aj/src/model.rs` helper that:
       1. Loads `ModelRegistry::load()` once at startup.
       2. Resolves `(api, model_name)` against the registry to
          a concrete `ModelInfo`. If `model_name` is `None`,
          picks the registry's default for the api.
       3. Calls `aj_models::provider::provider_for(&model_info.api)`
          to obtain a `Box<dyn Provider>` and wraps it in an
          `Arc`.
       4. Reads the appropriate API key via
          `aj_models::auth::find_env_keys(&model_info.provider)`
          and stamps it onto `StreamOptions::api_key` along with
          any `Speed`-driven beta headers.
       5. Hands `(provider, model_info, options)` to
          `Agent::new(...)` via a new constructor variant
          `Agent::with_provider(...)` that takes the unified
          shape.

       Both legacy `Agent::new(Arc<dyn Model>, ...)` and the
       new `Agent::with_provider(...)` stay alive for one more
       commit; step 6.7 drops the legacy one. The `--scripted`
       flag stays on `Arc<dyn Model>` for now — step 6.8 ports
       it onto `ScriptedProvider` directly.

       **Implementation notes.** Lands as the binary-side helper
       at `src/aj/src/model.rs` (new module, registered in
       `lib.rs`) plus the matching wiring in
       `aj/src/modes/{interactive,print}.rs` and the runtime
       additions in `aj-agent/src/lib.rs`. The helper exposes:
       [`ResolvedModel`] (the `(Arc<dyn Provider>, Arc<ModelInfo>,
       StreamOptions)` bundle ready to plug into the agent),
       `resolve(&registry, provider_id, model_id, url, speed)`
       for the CLI / config / env path, and
       `from_model_info(model_info, speed)` for the `/model`
       selector swap path (which already has the catalog row in
       hand). Default provider is `"anthropic"` when nothing in
       CLI / env / config picks one; default model is the first
       entry the registry lists for that provider (catalog order
       preserved by `ModelRegistry::from_catalog_with_overrides`,
       so a future "preferred default per provider" hook lands
       in §4.3 SettingsManager without touching the helper).

       **Speed → beta header.** The fast-inference Anthropic
       beta (`fast-mode-2026-02-01`) is stamped onto
       [`StreamOptions::headers`] when `--speed fast` / config
       `speed = "fast"` is set *and* the resolved
       `model_info.api` is `anthropic-messages`. Other APIs
       silently ignore the speed knob — same end-user effect as
       the legacy `AnthropicModel::new`'s 3-line `if speed.is_some()
       { client.with_beta(...) }` block, just relocated to the
       binary so the provider crate stays generic. New
       [`AnthropicProvider::build_client`] reads
       [`StreamOptions::headers`] for any `anthropic-beta`
       entries (comma-split, case-insensitive, empty-trim) and
       appends them to the SDK client's beta list before the
       request goes out. Six new unit tests in
       `aj-models::anthropic::provider::tests` pin the parsing
       logic (`extra_betas_from_headers_*`).

       **Agent runtime additions.** [`Agent::with_provider`]
       (alongside the existing legacy [`Agent::new`]) takes the
       `(provider, model_info, stream_options, thinking)` quad
       and routes through a shared private `build` constructor
       so tool flattening + default-thinking projection lives in
       one place. `Agent::model: Arc<dyn Model>` becomes
       `Option<Arc<dyn Model>>`, populated only on the legacy
       path; [`Agent::model_info`] is the new accessor that's
       always populated and is what the TUI footer / startup
       banner now read off (previously
       `agent.model().model_name()` / `.model_url()`).
       [`Agent::set_provider`] is the registry-driven swap
       counterpart of `Agent::set_model`. The inference loop's
       `run_inference_streaming` now seeds
       [`SimpleStreamOptions::base`] from `self.stream_options`
       instead of `StreamOptions::default()`, so the resolved
       api_key and headers reach every provider call.
       [`SessionContextWrapper`] gains
       `provider: Arc<dyn Provider>` /
       `model_info: Arc<ModelInfo>` /
       `stream_options: StreamOptions` so sub-agents spawned
       from a `with_provider` parent reuse the same triple via
       [`Agent::with_provider`]; legacy parents continue
       spawning through [`Agent::new`] so the `--scripted`
       path and the event-protocol tests stay byte-for-byte
       unchanged.

       **Binary call sites.** `interactive.rs` startup splits
       on `args.scripted`: scripted runs go through
       `scripted::resolve_or_explain` + `Agent::new` (legacy);
       real runs go through `ModelRegistry::load` +
       `model::resolve` + `Agent::with_provider`. The `/model`
       selector's swap path calls `model::from_model_info(info)`
       + `agent.set_provider(...)`. `print.rs` mirrors the same
       split. The TUI footer reads `id @ base_url` off
       `agent.model_info()` so both paths render identically.

       **Tests.** 11 new unit tests in `model::tests`:
       `pick_model_returns_explicit_match`,
       `pick_model_falls_back_to_first_listed`,
       `pick_model_errors_for_unknown_provider`,
       `pick_model_errors_for_unknown_model_in_known_provider`,
       `apply_speed_headers_skips_when_speed_unset`,
       `apply_speed_headers_skips_standard_speed`,
       `apply_speed_headers_stamps_fast_mode_beta_on_anthropic`,
       `apply_speed_headers_is_noop_for_non_anthropic_providers`,
       `add_beta_header_creates_when_missing`,
       `add_beta_header_appends_to_existing_value`,
       `add_beta_header_skips_duplicate`. Plus 6 new tests in
       `aj-models::anthropic::provider::tests` for the header
       parsing path. The existing
       `event_protocol_tests::run_single_turn_*` and
       `aj/tests/replay_parity.rs` suites continue to drive
       `ScriptedModel` through `Agent::new`, locked event
       sequences pass unchanged — strongest signal the
       legacy-shaped scripted path is untouched. `cargo build`,
       `cargo fmt`, `cargo test --workspace`, and
       `cargo clippy -p aj-agent --all-targets` all pass (only
       pre-existing `clone_on_ref_ptr` warnings in `bus.rs` and
       `oauth/anthropic.rs` remain).

       Catches resolved: `Speed`-driven beta header plumbing
       moved out of the legacy `AnthropicModel::new` into the
       binary's `model::apply_speed_headers` helper, threaded
       through `StreamOptions::headers`, and consumed by
       `AnthropicProvider::build_client`'s new
       `extra_betas_from_headers` parser.

- [x] **6.7: `Agent::new` drops `Arc<dyn Model>`.** Removed the
       legacy `Agent::new(model: Arc<dyn Model>, ...)` constructor;
       `Agent::with_provider(...)` is the only way to build an
       agent. Removed the `Agent::model: Option<Arc<dyn Model>>`
       field, the `Agent::model()` accessor, and the
       `Agent::set_model(Arc<dyn Model>)` mutator. Folded the
       former `Self::build` helper directly into `with_provider`
       since the dual-path indirection is gone. The legacy doc
       refs on `Agent::model_info()` were cleaned up to no longer
       reference the deleted constructor.

       Internal `LegacyProviderAdapter` usage inside `Agent` is
       gone — the agent never wraps a legacy handle anymore. The
       `synthetic_model_info` helper that used to live in
       `aj-agent::lib.rs` migrated to `aj-models::compat` as a
       public free function so the two remaining call sites
       (`aj/src/scripted.rs` and the test fixtures in
       `aj-agent::event_protocol_tests` /
       `aj/tests/replay_parity.rs`) can wrap a `ScriptedModel`
       themselves and feed the resulting `(Provider, ModelInfo)`
       pair into `Agent::with_provider`. The now-unused
       `LegacyProviderAdapter::inner()` accessor went with it.

       `SessionContextWrapper`'s `model: Option<Arc<dyn Model>>`
       slot dropped; sub-agents always share the parent's
       `provider` + `model_info` + `stream_options` triple per
       `docs/aj-next-plan.md` §1.6. The `if let Some(model) = ...`
       branch in `spawn_agent` collapsed to a single
       `Agent::with_provider` call.

       `aj/src/scripted.rs::resolve_or_explain` now returns a
       new `ResolvedScriptedModel { provider, model_info }` struct
       (mirroring the shape `crate::model::resolve` returns for
       real providers) so the `--scripted` flag plumbs the same
       `(Provider, ModelInfo, StreamOptions)` shape as the
       registry-driven path. The two binary call sites
       (`interactive.rs`, `print.rs`) and the replay-parity
       integration test all flow through `Agent::with_provider`
       now.

       After this step the only remaining callers of the legacy
       `Model` trait are `ScriptedModel` itself, the
       `LegacyProviderAdapter::new(...)` wrap at the three call
       sites listed above, and the legacy `Anthropic`/`OpenAI`
       `Model` impl blocks (still spliced into `aj-models` for
       6.8 to replace with `ScriptedProvider` and 6.9 to delete).
       `cargo build`, `cargo fmt`, `cargo test --workspace`, and
       `cargo clippy --workspace --all-targets` all pass clean
       (pre-existing `clone_on_ref_ptr` warnings in `bus.rs`,
       `bash.rs`, `oauth/anthropic.rs`, and the agent's
       `event_protocol_tests` remain — none in the touched
       files). End-to-end `aj --print --scripted streaming-text`
       runs the canned demo to completion, confirming the
       provider-only path works for the scripted flow.

- [ ] **6.8: Tests + `--scripted` flag onto `ScriptedProvider`.**
       Migrate every test fixture that built `ScriptedModel`
       scripts (`aj-agent::event_protocol_tests`,
       `aj/tests/replay_parity.rs`, the scripted-demo eyeballing
       tests) onto `ScriptedProvider` + `AssistantMessageEvent`
       script steps. The `aj/src/scripted.rs` resolver returns
       `(Arc<dyn Provider>, Arc<ModelInfo>)` instead of
       `Arc<dyn Model>`; the `--scripted` CLI flag plumbs the
       pair through to `Agent::with_provider(...)` exactly like
       6.6 does for real providers.

       Watchpoints:
       - `replay_parity.rs` builds a `ScriptedModel` with one
         fixed `StreamingEvent::FinalizedMessage` carrying a
         single tool-use block. Under the new shape this becomes
         a single `Done { message }` event. The test's
         `ExhaustedBehavior::Panic` semantics carry over.
       - The `--scripted` CLI flag's demo library (`text_only`,
         `tool_use_demo`, `thinking_demo`, etc. in
         `scripted/demos.rs`) ports each demo to the new event
         shape. Most demos already specify deltas, so the
         translation is mechanical: `MessageStart` → `Start`,
         per-block deltas split into `*Start` / `*Delta` /
         `*End` per the unified protocol.

       After this step, **no production or test code calls
       `aj_models::Model` / `aj_models::create_model` /
       `aj_models::messages::*` / `aj_models::streaming::StreamingEvent`
       directly.** The legacy types are reachable only through
       `aj-models`'s own internal modules (which 6.9 deletes).

       Sub-progress:
   - [x] 6.8.i. `--scripted` CLI resolver onto `ScriptedProvider`.
         Rewrote `src/aj/src/scripted.rs::resolve_or_explain` to
         look the demo name up in
         [`aj_models::scripted::provider::demos`] (the new event-
         shape catalog landed in 6.3.ii) and return a
         [`ScriptedProvider`] preloaded with that demo's scripts.
         The provider defaults to [`ExhaustedBehavior::EndTurn`]
         so demos that run out of script (the user keeps chatting)
         exit cleanly without panicking the inference task — same
         semantics as the legacy resolver. The
         [`ResolvedScriptedModel`] return shape is unchanged
         (`{ provider: Arc<dyn Provider>, model_info: Arc<ModelInfo> }`),
         so the two binary call sites (`interactive.rs`,
         `print.rs`) didn't need an update.

         The synthesized [`ModelInfo`] is now built inline by a
         private `scripted_model_info(name)` helper instead of
         going through [`aj_models::compat::synthetic_model_info`]
         — the compat helper hardcoded `api: "legacy"` /
         `provider: "legacy"`, whereas the new identity matches
         what `ScriptedProvider` stamps onto every emitted
         [`AssistantMessage`] partial (`api: "scripted"` /
         `provider: "scripted"`). The model id is namespaced by
         demo name (`scripted/<demo>`) so the TUI footer's
         `format!("{} @ {}", info.id, info.base_url)` rendering
         shows which canned flow is running — matches the legacy
         resolver's `with_name(format!("scripted/{name}"))`
         behaviour.

         The `--scripted` flag's doc-comment in
         `src/aj/src/cli/args.rs` had a stale paragraph claiming
         the binary "registers a [`ScriptedModel`] in its place";
         updated to point at [`ScriptedProvider`] and
         [`AssistantMessageEvent`] instead.

         After this commit `aj/src/scripted.rs` no longer touches
         the legacy `Model` trait, `LegacyProviderAdapter`, or
         `synthetic_model_info`; those references move out of the
         binary entirely. Two call sites still use the compat
         helpers — `aj-agent::event_protocol_tests` (6.8.ii) and
         `aj/tests/replay_parity.rs` (6.8.iii) — and both go away
         in their respective sub-steps. `cargo build`, `cargo
         test --workspace`, `cargo fmt`, and `cargo clippy
         --workspace --all-targets` all pass clean (only the
         pre-existing `clone_on_ref_ptr` warnings in `bus.rs`,
         `bash.rs`, `oauth/anthropic.rs`, and the agent's
         `event_protocol_tests` remain — none in the touched
         files). End-to-end `cargo run -p aj -- --scripted
         streaming-text --print "hi"` exercises the new path
         live and runs the canned demo to completion.
   - [ ] 6.8.ii. `aj-agent::event_protocol_tests` onto
         `ScriptedProvider`. Replace the
         `ScriptedModel::from_event_vecs(...)` builder used in
         every test in
         `src/aj-agent/src/lib.rs::event_protocol_tests` with
         `ScriptedProvider::from_messages(...)` (the unified
         "final message script" mode) or
         `from_event_vecs(...)` for tests that author per-event
         deltas. Drop the `Arc<dyn Model> → LegacyProviderAdapter
         → synthetic_model_info` plumbing from `build_agent`; the
         test helper builds a `ScriptedProvider` and `ModelInfo`
         directly. Locked event sequences should match
         byte-for-byte across the migration (the
         `LegacyProviderAdapter`'s transcoder produces the same
         event shape `ScriptedProvider` emits natively).
   - [ ] 6.8.iii. `aj/tests/replay_parity.rs` onto
         `ScriptedProvider`. Same migration shape as 6.8.ii: drop
         the `ScriptedModel`-driven `one_tool_use_script` helper
         and rebuild it on `ScriptedProvider::from_messages`
         (a one-block tool-use `AssistantMessage` with
         `stop_reason: ToolUse`). After this, the integration
         test no longer references `aj_models::Model` /
         `aj_models::compat::*` / `aj_models::streaming::StreamingEvent`
         and the `aj_models::scripted::ScriptedModel` /
         `aj_models::scripted::Script` types — the only remaining
         caller of those is `aj-models` itself, which 6.9 deletes.

- [ ] **6.9: Delete legacy.** With every call site migrated,
       delete in one commit:
       - The `Model` trait, `create_model`, `ModelArgs`, and
         `ModelError` (if no other crate uses it) from
         `aj-models::lib`.
       - `aj-models::anthropic::legacy` and
         `aj-models::openai::legacy` modules and their
         `pub mod legacy;` declarations in `anthropic.rs` /
         `openai.rs`. The `AnthropicModel` / `OpenAiModel`
         re-exports go with them.
       - `aj-models::streaming::StreamingEvent` enum. The
         supporting types (`DoneReason` / `ErrorReason`) stay —
         they're used by the unified protocol.
       - The legacy `ScriptedModel` impl in
         `aj-models::scripted` (its `impl Model` block, the
         `StreamingEvent`-based `ScriptStep` shape, and the
         legacy demo helpers). The `ScriptedProvider` from 6.3
         keeps the demo catalog under its new event shape.
       - The internal `LegacyProviderAdapter` from 6.4 and the
         `aj-models::compat` module.
       - `aj-models::messages` module. **Decision deferred to
         this step:** either rename it to `aj-models::wire`
         (preserving `MessageParam` / `ContentBlockParam` / etc.
         as the on-disk + agent-transcript currency, separate
         from the tagged-union `aj-models::types::Message`),
         or absorb the types into `aj-models::types` under a
         new `wire` submodule. Recommended: rename to `wire` —
         it keeps the role-of-the-types clear (on-disk + agent
         transcript) and avoids a single module owning two
         distinct conceptual shapes. Update every import path
         across `aj-agent`, `aj-session`, `aj-tools`, and `aj`
         in the same commit; the audit identified the exact
         lines.
       - Doc comments referencing removed types in
         `aj/src/cli/args.rs:53,60`,
         `aj/src/scripted.rs:7,13`,
         `aj/src/modes/interactive/components/model_selector.rs:247,263`,
         `aj-agent/src/events.rs:286,323`,
         `aj-agent/src/types.rs:26`,
         `aj/src/modes/interactive/event_pump.rs:884`.

       Then tick steps 16/17/18 in `models-progress.md`.

### Architectural choices (decided up front)

- **`Agent` transcript stays `Vec<MessageParam>`.** Keeps the
  on-disk JSONL format unchanged (`aj-session` writes
  `ConversationEntryKind::Message(MessageParam)` directly). The
  agent owns a private
  `convert_to_llm(transcript: &[MessageParam]) -> Vec<types::Message>`
  helper called once per inference. Future work can replace
  this with a richer `AgentMessage` enum and a user-injectable
  `convert_to_llm` hook without disturbing the
  6.x steps. The projection is mechanical:
  `MessageParam { role: User, content }` →
  `Message::User(UserMessage { ... })` filtering tool-result
  blocks out into one `Message::ToolResult` per block;
  `MessageParam { role: Assistant, content }` →
  `Message::Assistant(AssistantMessage { ... })`.

- **Provider lookup is per-`Agent`, not per-inference.** The
  agent holds an `Arc<dyn Provider>` constructed once at startup
  (or once per `/model` swap). A per-inference lookup indirection
  (`(model, context, options) -> EventStream` closure) is
  flexible but overkill for our current capabilities — we don't
  have an in-process model registry that resolves providers per
  inference, and reusing the constructed provider avoids
  re-allocating the underlying SDK client on every turn. If we
  ever need OAuth-refresh-per-call (per `models-spec.md` §9.4),
  it can ride through `StreamOptions::api_key` without
  re-architecting the provider handle.

- **Tool results stay folded into the wire `MessageParam`
  flow.** `aj-session` already round-trips
  `ConversationEntryKind::ToolResult { content, details }` with
  the structured `ToolDetails` map (per the Resume Fidelity
  follow-up steps 1–6 below). The provider-facing projection
  in 6.5 surfaces these as `aj_models::types::Message::ToolResult`
  entries — one per `tool_use_id` — when building the per-
  inference `Context`. The agent's own state shape doesn't
  change; the projection helper does the encoding. This matches
  the unified-types contract in `docs/models-spec.md` §1.2,
  where `ToolResultMessage` is a top-level message variant
  rather than a content block inside a user message.

- **`StreamingEvent` → `AssistantMessageEvent` mapping (only
  used inside `LegacyProviderAdapter`).** Detailed in step 6.4.
  The adapter's transcoder is the only place the legacy
  protocol meets the new one; once 6.7 / 6.8 land, the adapter
  becomes unreachable from production code and 6.9 deletes it.

- **`messages` → `wire`, not `messages` → `types`.** Keeping
  `MessageParam` (flat `{ role, content }`) separate from
  `types::Message` (tagged union of three message kinds)
  matches the natural division: `MessageParam` is the wire
  *for our persisted transcript*, while `types::Message` is
  the wire *for provider inference*. Conflating them today
  would force a JSONL format migration that's deliberately
  out of scope for Phase 6.

### Empirical references

- A legacy-type audit (line numbers, call sites, import paths
  for every `aj_models::Model`, `create_model`, `StreamingEvent`,
  `messages::*`, and `legacy::*` reference across the workspace)
  was captured during the planning step. The line-number
  pointers in each sub-step above were drawn from that audit.
- The streaming-event protocol and per-event `partial:
  AssistantMessage` contract are defined in
  `docs/models-spec.md` §2.
- The provider trait contract is defined in
  `docs/models-spec.md` §5.

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

## Resume fidelity follow-up — persist `ToolDetails` inline on tool results

A user-reported continue-session bug (chat scrollback rendered tools
without their structured body) traced to three distinct gaps. Two of
them landed already as `tui: clear current_assistant when replay
synthesizes a tool component` and `session: replay emits
ToolExecutionStart with captured tool_use args`. The third — the
biggest — is captured here so it's picked up when we get to
`docs/aj-next-plan.md` §3 ("Conversation log migration") and §1.2
("`ToolDetails` persistence").

### Symptom

On a resumed thread, a tool call that ran live as e.g.

```
✓ bash(command="echo \"hi\"", description="Test")
$ echo "hi"
hi
[exit 0]
```

renders on resume as a flat `ToolDetails::Text` shape:

```
✓ bash(command="echo \"hi\"", description="Test")   <-- args restored
  bash                                              <-- summary == tool name
  hi                                                <-- body == raw tool_result text
```

Same for `edit_file` / `write_file` (no diff visible — body shown as
plain text) and `todo_write` (no structured list — body is the
LLM-facing summary text).

### Root cause

The agent emits `AgentEvent::ToolExecutionEnd { result: ToolDetails }`
on the live bus, and the renderer uses the structured variant
(`ToolDetails::Bash { command, stdout, stderr, exit_code, truncated,
... }`, `ToolDetails::Diff { path, before, after }`,
`ToolDetails::Todos { items }`, etc.) to drive its specialised body
renderers (`bash_execution.rs::render_bash_body`,
`diff.rs::render_unified_diff`, the todo list view).

`ToolDetails` is **not** persisted. The on-disk format only stores
the wire `tool_result.content` (text-flattened
`ContentBlockParam::ToolResultBlock`). On resume the replay walker
can't reconstruct the structured variant, so it folds everything into
`ToolDetails::Text { summary: tool_name, body: result_text }` and the
renderer falls back to plain-text rendering of the body.

Acknowledged in code at `src/aj-agent/src/lib.rs` (search for "the
structured details land via a future `MessagePersisted::ToolDetails`
once the on-disk format catches up"). Captured in the plan in §3 as
the `ToolDetails`-replaces-`UserOutput::ToolResult*` migration row
but tied to the broader rewriting walker.

### Design: ride `details` inline on the tool-result record

The shape we want:

- Single on-disk record per tool call: `(assistant message with
  toolCall content blocks) → (toolResult message with both
  LLM-facing `content` and structured `details`)`.
- The persisted tool-result record carries `content` (text/images
  for the LLM) **and** `details` (structured rendering payload) on
  the same wire record. Both are persisted on the same JSONL line.
- **No separate "ToolDetails" entry kind, no separate
  start/end persistence events.** The agent only ever writes one
  record per tool call. The in-progress visual state is a
  live-stream-only concept driven by the event bus; resumed sessions
  skip it entirely (the tool is already finished by the time replay
  runs).
- Replay walks the linearized message list synchronously and uses an
  in-flight `Map<tool_use_id, ToolExecutionComponent>` to wire the
  tool result back to the component spawned for the matching
  `tool_use` block on the preceding assistant message.

That maps onto our types as: store the `ToolDetails` on the same
persistence record that already carries the wire `tool_result.content`,
not as a sidecar entry. The wire type
`ContentBlockParam::ToolResultBlock` is shared with the
provider SDK (Anthropic / OpenAI), so we can't add unknown fields to
it directly without filtering on the wire-out path. Two clean
shapes for where the details field rides:

- **Option A (lightweight, preferred):** widen
  `PersistedMessageKind::ToolResult` to
  `ToolResult { content: Vec<ContentBlockParam>, details:
  HashMap<String, (ToolDetails, bool /* is_error */)> }`, keyed by
  `tool_use_id`. The agent already emits this event once per turn
  with all tool results batched into one message (matching how
  `aj-models` packs the wire response), so the details map sits at
  the same level. Persistence listener writes both halves on the
  same `ConversationEntry`. Replay reads `details` directly off the
  entry and projects `ToolExecutionEnd { result: details.clone() }`
  for each tool_use_id, no second pass or correlation pointer
  needed.
- **Option B (per-block):** introduce a
  persistence-only `PersistedToolResultBlock` that wraps
  `ContentBlockParam::ToolResultBlock` with an extra
  `details: Option<ToolDetails>` field. `ConversationEntry`
  flattens its persistence content blocks, but a custom
  `Serialize`/`Deserialize` boundary translates between the
  persistence shape and the wire shape when projecting
  `Conversation::messages()` for the provider call. Slightly more
  type surface, but each persisted tool_result is self-contained
  (no separate details map to maintain).

Both keep the wire-out path clean (no extra fields leak to the
provider SDK) and both make the on-disk record the single source of
truth for resume rendering. Pick Option A unless the field-soup of
piggy-backing on top of `PersistedMessageKind::ToolResult` gets
unwieldy; the implementer can flip to Option B inside §3 without
disturbing replay or the agent loop.

### Implementation plan when §3 lands

Decomposed into self-contained commits, each one a step from the
end-to-end design above. Pick the next unchecked box.

- [x] **Step 1: Widen `PersistedMessageKind::ToolResult` to carry
      structured details.** The agent's batched tool-result
      persistence event now ships a
      `details: HashMap<String, ToolDetails>` field keyed by
      `tool_use_id`, alongside the existing wire `content`. The
      agent's tool-execution loop in `src/aj-agent/src/lib.rs`
      accumulates a per-call mirror of `tool_result_contents`
      during the batch loop and ships it on the
      `MessagePersisted::ToolResult` event at the end of the turn.

      The persistence listener in `aj-session::listener` destructures
      the new field but does not yet route it onto disk (an
      explicit `let _ = details;` plus a comment pointing at step 2);
      this is the layered scaffolding the design intended — the
      producer side ships the structured payload first, the
      on-disk shape catches up next, replay catches up after that.

      Pin-shape JSON test in `src/aj-agent/src/events.rs` now
      asserts:
      - The empty `details` map is skipped on serialize
        (no shape change for legacy emitters / the rare
        details-less batch).
      - A populated map surfaces as a JSON object whose values are
        the existing internally-tagged `ToolDetails` shape (e.g.
        `{"kind": "text", "summary": "...", "body": "..."}`).

      New unit test
      `tool_result_persistence_event_carries_structured_details_by_id`
      in `src/aj-agent/src/lib.rs` drives a full tool-call turn
      against a scripted model + the existing `PingTool` fixture
      and asserts:
      - Exactly one `MessagePersisted::ToolResult` event fires per
        turn.
      - Its `content` carries the wire `ToolResultBlock` keyed by
        the expected `tool_use_id`.
      - Its `details` map has a matching entry keyed by the same
        `tool_use_id`, carrying the structured `ToolDetails::Text`
        payload `PingTool` returned.

      Other downstream consumers of `PersistedMessageKind::ToolResult`
      (`aj-agent`'s own `event_label`, `aj/src/modes/interactive/event_pump.rs`)
      already used `{ .. }` patterns and were unaffected by the
      widening. `cargo build`, `cargo test --workspace`, `cargo
      fmt`, and `cargo clippy -p aj-agent -p aj-session
      --all-targets` all pass clean (only the pre-existing
      `clone_on_ref_ptr` warnings remain, matching the surrounding
      test fixtures' style).

- [x] **Step 2: On-disk persistence — new entry shape +
      `add_tool_result`.** Introduces a way to ride
      `details: HashMap<String, ToolDetails>` on the on-disk record
      alongside the wire `Vec<ContentBlockParam>`. Landed Option A
      from the design exploration: a new
      [`ConversationEntryKind::ToolResult { content, details }`]
      variant on the persisted entry plus a matching
      [`ConversationView::add_tool_result(content, details)`]
      method. The on-disk schema is additive — legacy logs omit the
      variant entirely, and a `ToolResult` entry with an empty
      `details` map serializes to `{"type":"tool_result","content":[...]}`
      with no `details` key thanks to
      `#[serde(default, skip_serializing_if = "HashMap::is_empty")]`,
      so the wire shape stays compact for callers that don't have
      structured details to ship.

      `Conversation` projects [`ToolResult`] entries onto user-role
      [`MessageParam`]s through a new private
      [`message_param_for`] helper so the wire layer continues to
      see one user message per tool batch regardless of which
      on-disk encoding (legacy `Message` or structured `ToolResult`)
      the log uses. [`messages()`] and [`last_message()`] now
      return owned [`MessageParam`] values because the projection
      synthesizes a new [`MessageParam`] on the fly for `ToolResult`
      entries; the two call sites in the binary
      (`aj/src/modes/print.rs::run` and
      `aj/src/modes/interactive.rs::{run, perform_thread_swap}`)
      dropped their `.into_iter().cloned().collect()` adapters
      accordingly. [`message_count()`] counts `ToolResult` entries
      alongside `Message` entries.
      [`last_user_message()`] / [`last_assistant_message()`] are
      unchanged — both filter on `TextBlock` content, which
      `ToolResult` entries never carry, so the existing semantics
      hold.

      [`repair_interrupted_tool_uses`] now treats `tool_use_ids` on
      `ToolResult` entries as resolved alongside the legacy
      [`Role::User`] tool-result message path so a thread written
      with structured tool results doesn't re-synthesize
      "interrupted tool call" markers on resume.
      [`read_thread_preview_file`] counts `ToolResult` entries
      toward `message_count` and bumps `last_message_at` off their
      timestamps so the session selector overlay's row stats stay
      accurate when a session uses the new variant.
      [`aj_session::replay`] picks up an additional match arm that
      projects `ToolResult` entries via the existing
      [`project_user`] helper (text-only synthesis off the wire
      content); step 4 upgrades this projection to read the
      persisted structured `details` payload directly.

      Step 3 (persistence listener dispatch) flips
      `let _ = details;` in
      `aj_session::listener::persist` over to a call into the new
      `add_tool_result` method so the agent's batched
      [`PersistedMessageKind::ToolResult`] event actually lands on
      disk as a `ConversationEntryKind::ToolResult` entry. Until
      then the variant is reachable through tests and the public
      API (a follow-up could already write `ToolResult` entries
      via [`ConversationView::add_tool_result`] from outside the
      listener), but the live agent run keeps writing tool batches
      through the legacy [`add_user_message`] path so the on-disk
      shape for new logs hasn't changed yet.

      Four new tests pin the contract:
      - `add_tool_result_writes_structured_entry_visible_through_messages_projection`
        (in `aj-session::log::tests`) round-trips a populated
        `details` payload through write → resume → linearize → read
        and asserts the entry shape survives, the wire projection
        renders as a user-role message, and `message_count` /
        `last_message` see the new variant.
      - `add_tool_result_with_empty_details_skips_field_on_disk`
        (in `aj-session::log::tests`) asserts the empty-`details`
        case never writes `"details"` on the JSONL line and that
        the on-disk shape deserializes back with an empty map by
        default.
      - `repair_recognises_tool_use_ids_resolved_by_structured_tool_result_entries`
        (in `aj-session::repair::tests`) verifies the repair
        walker treats `tool_use_ids` resolved by structured
        `ToolResult` entries the same as legacy `Message`-encoded
        tool results.
      - `replay_projects_structured_tool_result_entries_as_user_thread_events`
        (in `aj-session::replay::tests`) pins the step-2 bridging
        shape: `ToolExecutionStart` carrying the tool name + args
        captured from the preceding assistant turn, followed by a
        `ToolExecutionEnd` with a `ToolDetails::Text` fallback
        body. Step 4's upgrade will tighten the assertion to read
        the persisted `details` map.
      - `list_thread_previews_counts_structured_tool_result_entries_as_messages`
        (in `aj-session::persistence::tests`) verifies the
        per-thread preview's `message_count` includes
        `ToolResult` entries.

      `cargo build`, `cargo test --workspace`, `cargo fmt`, and
      `cargo clippy --workspace --all-targets` all pass clean (only
      the pre-existing `clone_on_ref_ptr` warnings remain in
      `aj-agent` / `aj-models` / `aj-tools`).

- [x] **Step 3: Persistence listener dispatch.** Replaced the
      `let _ = details;` placeholder in `aj_session::listener::persist`
      with a `view.add_tool_result(content, details)?` call so the
      agent's batched [`PersistedMessageKind::ToolResult`] event
      lands on disk as a [`ConversationEntryKind::ToolResult`]
      entry carrying both halves — the wire `tool_result` content
      blocks for the model, and the structured per-call
      [`ToolDetails`] payload (keyed by `tool_use_id`) for the
      renderer. New logs written by a `MessagePersisted::ToolResult`
      emit now use the structured on-disk shape; legacy logs
      written as plain user-role `Message` entries continue to
      work because [`Conversation::messages`] and the replay
      walker accept both encodings (see step 2). Step 4 still
      tightens the replay projection to read the persisted
      `details` directly off the entry.

      New unit test in `aj_session::listener::tests`,
      `tool_result_kind_writes_structured_tool_result_entry`,
      drives a `PersistedMessageKind::ToolResult` event through a
      bus-subscribed listener and asserts the freshly-appended
      entry is a `ConversationEntryKind::ToolResult` with the
      expected wire content + the `tool_use_id ↦
      ToolDetails::Text` mapping, and that the wire projection
      still surfaces it as one user-role message per tool batch
      so the model continues to see the same shape on the next
      inference. `cargo build`, `cargo test --workspace`, `cargo
      fmt`, and `cargo clippy --workspace --all-targets` all pass
      clean (only the pre-existing `clone_on_ref_ptr` warnings
      remain in `aj-agent` / `aj-models` / `aj-tools`).

- [x] **Step 4: Replay reads `details` (with legacy fallback).**
      [`aj_session::replay::ReplayState::project_user`] now takes
      an `Option<&HashMap<String, ToolDetails>>` alongside the wire
      `content` slice and, for each [`ToolResultBlock`], uses
      `details.and_then(|map| map.get(tool_use_id)).cloned()` as the
      payload for the synthesized [`AgentEvent::ToolExecutionEnd`]
      event. When `details` is `None` (legacy
      [`ConversationEntryKind::Message`] user-role entries) or the
      map lacks an entry for a given `tool_use_id` (e.g. the
      repair-on-resume walker's empty-map writes, or a future
      partial-details producer), the projection falls back to the
      pre-Step-4 text-only [`ToolDetails::Text { summary: tool_name,
      body: content.text() }`] synthesis so the renderer still has
      something to paint. The synthetic [`ToolExecutionStart`]
      continues to ship the args header captured from the preceding
      assistant turn's `tool_use` block; new logs with structured
      details just get a richer body on the matching End event.

      Three tests cover the new behaviour in
      `aj-session::replay::tests`:
      - `replay_projects_persisted_tool_details_on_resume` writes a
        structured tool-result entry with two
        distinct [`ToolDetails`] variants ([`Diff`] for an
        edit_file-style call, [`Bash`] for a bash-style call) in a
        single batch and asserts each End event surfaces the
        persisted payload verbatim — pinning the
        tool_use_id-keyed lookup so the map isn't collapsed to its
        first or last entry.
      - `replay_falls_back_to_text_for_tool_use_ids_missing_from_details`
        writes an entry with a partially-populated `details` map
        (one `tool_use_id` has a [`Diff`] payload, the other has
        nothing) and asserts the missing entry falls through to
        the text-only synthesis with the wire body and the
        resolved tool name as the summary.
      - `replay_projects_structured_tool_result_entries_as_user_thread_events`
        (renamed comment, same body) keeps pinning the empty-map
        fallback path used by the repair walker.

      The legacy [`Role::User`] tool-result message path passes
      `None` as the `details` argument from
      [`ReplayState::project_entry`], so its byte-for-byte
      behaviour is preserved — the existing
      `replay_projects_assistant_thinking_text_and_tool_results`
      and `replay_falls_back_when_tool_use_id_is_not_tracked`
      tests continue to pass without modification.

      `cargo build`, `cargo test -p aj-session`, `cargo fmt`, and
      `cargo clippy -p aj-session --all-targets` all pass clean
      (only the pre-existing `clone_on_ref_ptr` warnings remain
      in `aj-agent`).

- [x] **Step 5: End-to-end replay-vs-live parity test.** A test in
      `src/aj/` that runs a tool-call turn live (bash, edit_file,
      todo_write fixtures), kills the process between persistence
      and the next inference, resumes from the on-disk log, and
      asserts the rendered scrollback bytes match the live ones
      for each of the three tool kinds. Without this guard the
      three rendering paths drift silently across future
      refactors.

      Implemented as the integration test
      `src/aj/tests/replay_parity.rs`. Three `#[tokio::test]`
      cases (`replay_renders_bash_tool_identically_to_live`,
      `replay_renders_edit_file_tool_identically_to_live`,
      `replay_renders_todo_write_tool_identically_to_live`) share
      a small `drive_live_turn` harness that:

      1. Builds a temp `threads_dir` and working dir, plus a
         scripted model that returns exactly one inference whose
         finalized message carries a single tool_use block for
         the target tool. `ExhaustedBehavior::Panic` traps any
         path that would let the agent reach a second inference.
      2. Registers three bus listeners in order: the production
         `persistence_listener`, an event-capture listener that
         pushes every observed `AgentEvent` into a `Vec`, and a
         kill-switch listener that returns `Err` the first time
         it sees `MessagePersisted::ToolResult`. The earlier
         listeners run first per `EventBus::emit`'s
         registration-order contract, so the disk write and the
         capture both complete before the kill switch trips —
         exactly mirroring "process dies after the tool result
         persists but before the next inference starts".
      3. Drives `agent.prompt(...)` and asserts it returns
         `Err(TurnError::Fatal(_))`.

      The test then renders two chat scrollbacks against the
      same `build_layout`-built `Tui` (with a headless
      `StubTerminal` that drops every write) and the same
      `bundled_dark` theme:

      - **Live:** drive the captured `AgentEvent`s through a
        fresh `EventPump`, skipping `TurnUsage` events (the
        agent emits per-turn usage on the bus but does not
        persist it, so replay can't re-emit a matching event;
        the divergence is unrelated to tool rendering and the
        Step 5 test exists to guard the three tool render paths
        specifically — restoring usage persistence later turns
        the filter into a no-op).
      - **Replay:** `ConversationLog::resume` the same file,
        drive `replay(&log)` events through a second fresh
        `EventPump`, and compare the chat container's
        `Container::render(width)` output line-for-line.

      Tool inputs are chosen so each fixture exercises a
      distinct `ToolDetails` variant on the wire:

      | Fixture | Tool | Variant exercised | Notes |
      |---|---|---|---|
      | `replay_renders_bash_tool_identically_to_live` | `bash` | `ToolDetails::Bash` | runs `echo hello` against the working tempdir |
      | `replay_renders_edit_file_tool_identically_to_live` | `edit_file` | `ToolDetails::Diff` | seeds `sample.txt` with `"hello world\n"` and swaps `hello`→`goodbye` |
      | `replay_renders_todo_write_tool_identically_to_live` | `todo_write` | `ToolDetails::Todos` | writes a two-item list (`in-progress` / `todo`) |

      Comparison is byte-for-byte on the `Vec<String>` returned
      by `Container::render` — ANSI escapes and all. A
      side-by-side `diff_lines` helper feeds the panic message
      when a future refactor regresses one of the renderers so
      the failure mode is debuggable from CI output alone, no
      re-run with extra println!s needed.

      `cargo test -p aj --test replay_parity` passes (3 tests).
      `cargo build`, `cargo test --workspace`, `cargo fmt`, and
      `cargo clippy -p aj --all-targets` all pass clean (only
      the pre-existing `clone_on_ref_ptr` warnings remain in
      `aj-agent`).

- [x] **Step 6: Migration walker for legacy
      `UserOutput::ToolError`.** New `aj_session::migrate` module
      ships a one-shot `walk_threads_dir(path) -> MigrationSummary`
      invoked once from `src/aj/src/main.rs::main` before the mode
      dispatch (so interactive, `--print`, `continue`, and
      `list-threads` all see the migrated shape). The walker is
      idempotent — files with a sibling `<stem>.jsonl.bak` are
      skipped on subsequent passes — and a no-op for users without
      legacy data (no `.bak` sibling is created on a clean file).

      Per-entry behaviour, refined to handle the transitional shape
      modern logs took during the §3 ramp:

      - Walk back the `parent_id` chain to the closest assistant
        `ConversationEntryKind::Message` and match the legacy
        entry's `tool_name` against its `ToolUseBlock`s. The first
        unclaimed-and-unresolved candidate becomes the
        `tool_use_id` for the migration.
      - **Rewrite** the legacy entry as a structured
        `ConversationEntryKind::ToolResult { content, details }`
        when no existing `ToolResult` entry covers that
        `tool_use_id`: `content` carries one
        `ContentBlockParam::ToolResultBlock { tool_use_id, content:
        error_text, is_error: true }` so the wire transcript stays
        well-formed for the next inference, and `details` carries a
        single-entry `HashMap<String, ToolDetails::Text { summary:
        format!("{tool_name}: error"), body: error }>` keyed by the
        same `tool_use_id` so the renderer's structured-payload
        path lights up on resume. `id`, `parent_id`, `timestamp`,
        `thread`, and `agent_id` are preserved so subsequent
        entries (whose `parent_id` references this one) keep
        linking cleanly.
      - **Drop** the legacy entry when a `ConversationEntryKind::ToolResult`
        (or legacy user-role `ConversationEntryKind::Message`
        carrying `ToolResultBlock` content) in the same file already
        carries the matching `tool_use_id`. That case is the
        transitional double-write the agent emitted during the §3
        ramp: the structured entry has the canonical payload, and
        the freestanding error is redundant.
      - **Preserve** the legacy entry verbatim when no preceding
        `tool_use` block can be located in the parent chain
        (orphan errors from very early thread shapes). The
        renderer's legacy `UserOutput::ToolError` handler keeps
        surfacing them; a file containing only orphans is left
        untouched (no `.bak`, no rewrite) since orphans aren't a
        semantic change.

      File-level rewrite is atomic via a three-step ordering
      (`fs::copy` original → `.bak`; write `.tmp`; `fsync`;
      atomic same-directory `rename` `.tmp` → original) so every
      crash point leaves a recoverable state. The `.bak` carries
      the pre-migration bytes verbatim — covered by a unit test
      that greps the backup for the legacy `"ToolError"` tag.

      Eight unit tests in `aj_session::migrate::tests` cover:
      `walk_is_noop_when_threads_dir_missing` (missing dir →
      default summary, no error); `walk_skips_files_with_no_legacy_entries`
      (clean file → no `.bak`, no counters bumped);
      `walk_rewrites_legacy_tool_error_as_structured_tool_result`
      (happy path: rewrite, `.bak` preserves original, structured
      shape round-trips through `ConversationLog::resume` →
      `linearize`); `walk_drops_legacy_tool_error_when_structured_result_already_covers_it`
      (transitional double-write dedup); `walk_preserves_orphan_tool_error_without_preceding_tool_use`
      (orphan path: no `.bak`, no rewrite, `files_migrated == 0`);
      `walk_is_idempotent` (second pass observes `.bak` and skips);
      `walk_handles_batch_of_errors_sharing_one_assistant_turn`
      (two errors share one assistant turn with two `ToolUseBlock`s
      → distinct `tool_use_id`s claimed in source order); and
      `walk_preserves_parent_chain_after_rewrite` (entries
      following the rewritten one still resolve their `parent_id`
      and `linearize` walks past).

      Startup wiring (`src/aj/src/main.rs::run_migration`) reads
      the threads dir via `Config::get_threads_dir_path()` and
      demotes both resolution and walker errors to
      `tracing::warn!` so a migration failure never blocks a
      session — the legacy entries simply keep rendering through
      the renderer's fallback path until the issue is resolved. A
      `tracing::info!` one-line summary fires when at least one
      file or entry changed, with explicit `files_migrated`,
      `entries_rewritten`, `entries_dropped`, `entries_orphaned`,
      and `files_skipped` counters.

      `cargo build`, `cargo test --workspace`, `cargo fmt`, and
      `cargo clippy -p aj-session --all-targets` all pass clean
      (only the pre-existing `clone_on_ref_ptr` warnings remain
      in `aj-agent`).

### Original implementation-plan prose (preserved for reference)

1. **Wire event variant.** Widen `PersistedMessageKind::ToolResult`
   (Option A) or introduce `PersistedToolResultBlock` (Option B) to
   carry the structured details. The agent emits this from the
   tool-execution loop in `Agent::run` (`src/aj-agent/src/lib.rs`)
   alongside the existing wire `content`. Carry the same
   `tool_use_id` the bus events use so replay can correlate the
   details back to the persisted block. No new
   `PersistedMessageKind::ToolDetails` variant — the existing
   `ToolResult` event simply grows the field.

2. **On-disk persistence.** Update
   `aj_session::log::ConversationView::add_user_message` (or a new
   `add_tool_result` method specialized for this shape) to accept
   the details map alongside the content blocks and serialize both
   into the same `ConversationEntryKind::Message` JSONL line. The
   on-disk schema is additive: legacy logs simply omit the field
   (`#[serde(default, skip_serializing_if = "...")]`) and replay
   falls through to today's text-only synthesis path. **No
   rewriting migration walker is needed for the new field** — the
   walker described in §3 is still wanted for the legacy
   `UserOutput::ToolError` rewrites, but a separate purpose.

3. **Persistence listener.** Extend
   `aj_session::listener::persist` to route the widened
   `PersistedMessageKind::ToolResult` through the new `add_*`
   method.

4. **Replay.** Update `aj_session::replay::ReplayState` to read the
   details field off each persisted tool_result and project
   `AgentEvent::ToolExecutionEnd { result: details, is_error }`.
   When the field is absent (legacy logs), keep today's fallback:
   `ToolDetails::Text { summary: tool_name, body: result_text }`.
   The synthetic `ToolExecutionStart` emitted today by
   `project_user` continues to provide the args header; new logs
   with details just get a richer body on top.

5. **Migration walker (for legacy `UserOutput::ToolError` only).**
   The rewriting walker sketched in §3 still applies to the legacy
   `UserOutput::ToolError` entries (37/37 of the on-disk
   `user_output` entries per the §2.0 reconnaissance). Rewrite each
   as a persisted tool_result with an empty wire `content` and
   `details = ToolDetails::Text { summary: format!("{tool_name}:
   error"), body: error }`, anchored on the same `parent_id`. Per-
   tool reconstruction for *successful* legacy tool results isn't
   needed because there are zero `UserOutput::ToolResult*` entries
   on disk; pre-§3 successes only existed in the wire
   `tool_result.content`, which replay already projects to
   `ToolDetails::Text` correctly.

6. **Tests.**
   - `aj-session::replay`: a persisted entry with `details` emits a
     structured `ToolExecutionEnd`; an entry without `details`
     falls through to today's text-only synthesis (regression
     against legacy logs).
   - `aj-session::listener`: `MessagePersisted::ToolResult` with
     details writes both halves on the same JSONL line.
   - `aj-session::migrate`: legacy `UserOutput::ToolError` is
     rewritten as a structured tool_result with `is_error: true`;
     idempotent on rerun.
   - `aj`: an end-to-end replay test (live run → kill → resume) that
     pins the rendered scrollback against the live one for bash,
     edit_file, and todo_write fixtures. Without this test the
     three rendering paths drift silently across the migration.

### Architectural pointers

These are second-order design points worth honouring when §3 lands
even though they aren't strictly required by the fidelity bug:

- **Render the assistant's text/thinking as one component, then walk
  its toolCalls and append a tool component per call.** Don't try to
  interleave tool components inside the assistant message's content
  rendering. The natural turn boundary (model stops generating after a
  toolUse stop reason) keeps the order correct: the next assistant
  message lands after the toolResult. Today's `EventPump` already
  follows this shape via the streaming event sequence; the §3 work
  shouldn't disturb it.
- **Tool result `content` always carries a useful text representation
  for the LLM.** The `details` field is for the renderer; the LLM
  still needs words to reason over. Don't shrink the wire content to
  an empty string just because the structured `details` has the
  "real" data — the next turn's prompt depends on the LLM seeing
  something meaningful in `tool_result.content`.
- **Snapshot full state for stateful tools (todos, plan-mode state)
  in `details`.** Persist the full `todos: Vec<TodoItem>` snapshot in
  each tool_result's `details` and reconstruct in-memory state on
  resume by walking the message branch and replaying the latest
  snapshot. Our `ToolDetails::Todos { items: Vec<TodoItem> }` already
  has the right shape; just make sure resumes actually use it as the
  rehydration source rather than re-running the tool.
- **Persistence is just a listener on the agent's event bus.** Our
  current §2.4b wiring already has this shape — keep it that way
  through §3. Don't pass the log into `Agent::run`; let the binary
  own the log and register the persistence listener.
- **Persist eagerly.** Don't defer the first flush until the first
  assistant message lands — that loses data on a pre-response crash.
  Our current implementation persists each `MessagePersisted` event
  as it arrives; keep that behaviour through §3. The cost (one
  fsync per JSONL line) is well below the agent's other latencies.
- **`ToolDetails` per-tool vs. closed enum.** An alternative shape is
  to let each tool ship its own details type and `render_result`
  closure, with the renderer registry looking up the right pair by
  tool name. Our closed `enum ToolDetails { Text, Bash, Diff, Todos,
  Json, SubAgentReport }` is simpler and is fine while the variant
  set is small. If we start adding lots of one-off tools with bespoke
  rendering, consider migrating to a `dyn ToolRenderer` registry to
  avoid the god-type pattern. Not blocking for §3.
- **Avoid duplicating the wire body in `details`.** A leaner shape
  would keep only metadata (truncation flags, full-output path) in
  `ToolDetails::Bash`, with the actual stdout living in the wire
  `content[0].text`. Our current `ToolDetails::Bash` duplicates
  stdout in both places, which is wasteful but harmless. Worth
  tidying as part of §3 if a re-shape lands.

### Out of scope for the immediate two commits

The reorder fix (commit 1) and the args-on-replay fix (commit 2) are
deliberately scoped to the **replay + renderer** side and don't
touch the on-disk format. They make a resumed session look exactly
like the live one *as far as the persisted bytes allow*. Closing the
remaining gap requires the `details`-inline-on-tool-result change
above and should land together with the §3 migration walker (for the
legacy `UserOutput::ToolError` rewrites) so a single binary release
covers both the new persistence path and the legacy cleanup.
