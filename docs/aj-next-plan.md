# `aj-next` plan — event-driven core + new TUI

This replaces the previous "keep `AjUi` and bridge into the TUI" plan.
The new shape: the agent is a pure state machine that emits a typed
event stream; consumers (a TUI, a plain-text CLI, an RPC server, log
persistence, tests) subscribe.

The plan is in three large pieces:

1. **Phase 0 — refactor `aj-agent` and `aj-tools` to the event-driven
   shape.** This is foundation work; the existing `aj` binary keeps
   working by riding a thin event-to-CLI bridge.
2. **Phase 1 — build `aj-next` on top of the new core.** Print mode
   first, then interactive TUI, then selectors and theming.
3. **Phase 2 — cutover.** `aj-next` becomes `aj`; `aj-ui` and the
   legacy CLI go away.

Phase 0 lands first and ships the workspace in a state where the
existing `aj` binary still works end-to-end. Phase 1 then layers the
new TUI without touching the now-stable agent core.

---

## 1. The new core shape

Two crates change shape; one is born; one dies. The dependency
direction between `aj-agent` and `aj-tools` flips: tools depend on the
agent core, not the other way around. The contract types (events,
tool trait, tool outcome) live in the same package as the `Agent`
runtime, and tool implementations sit a layer up.

| Crate | Today | After Phase 0 |
|-------|-------|---------------|
| `aj-agent` | Owns `Agent<UI: AjUi>`; pushes through `dyn AjUi`. Depends on `aj-tools`. | Owns the **agent contract**: `AgentEvent`, `AgentId`, `ToolDefinition`, `ToolContext`, `ToolOutcome`, `ToolDetails`, `TokenUsage`, `UsageSummary`, `StopReason`, plus the `Agent` runtime. No UI generic; emits events on a bus via `Agent::subscribe`. Does **not** depend on `aj-tools`. |
| `aj-tools` | Defines `ToolDefinition` trait + 11 tool implementations. Depends on `aj-ui`. | Tool implementations only. Depends on `aj-agent` for the trait + types. Each tool returns `ToolOutcome { model_output, details, is_error }`. |
| `aj-ui` | Defines `AjUi` trait + `UserOutput` + `TokenUsage` types. | Deleted. Its types either move into `aj-agent` (events, usage) or vanish (`AjUi`, `UserOutput`, `AjUiAskPermission`). |
| `aj` | Rustyline CLI implementing `AjUi`. | Same binary, but its UI is now an event listener that calls into `AjCli`'s rendering helpers. |
| `aj-next` | — | New crate with the TUI. Subscribes to the same event bus. |

The only place `aj-agent` reaches into `aj-tools` today is sub-agent
spawn (it calls `get_builtin_tools()` to assemble the child's tool
list). That gets pushed up: the binary builds the tool list once and
passes it to `Agent::new`; sub-agents reuse the same list (minus the
`agent` tool to prevent recursion).

After Phase 2 `aj` is deleted and `aj-next` is renamed.

### 1.1 The event enum

Lives in `aj-agent`. Every variant carries an `agent_id: AgentId` so
listeners can route sub-agent output (or, in v2, parallel agent fan-
out) into nested transcripts.

```rust
#[derive(Clone, Debug)]
pub enum AgentEvent {
    // Lifecycle
    AgentStart   { agent_id: AgentId, model: ModelInfo, thinking: ThinkingLevel },
    AgentEnd     { agent_id: AgentId },
    TurnStart    { agent_id: AgentId, turn: usize },
    TurnEnd      { agent_id: AgentId, turn: usize, usage: TokenUsage },

    // User input (emitted when a user message is accepted)
    UserMessage  { agent_id: AgentId, text: String },

    // Assistant streaming
    AssistantMessageStart { agent_id: AgentId },
    AssistantTextStart    { agent_id: AgentId, text: String },
    AssistantTextDelta    { agent_id: AgentId, diff: String },
    AssistantTextStop     { agent_id: AgentId, text: String },
    AssistantThinkingStart{ agent_id: AgentId, text: String },
    AssistantThinkingDelta{ agent_id: AgentId, diff: String },
    AssistantThinkingStop { agent_id: AgentId },
    AssistantMessageEnd   { agent_id: AgentId, stop_reason: StopReason },

    // Tool execution
    ToolExecutionStart  { agent_id: AgentId, call_id: String, tool: String,
                          input: serde_json::Value },
    ToolExecutionUpdate { agent_id: AgentId, call_id: String, partial: ToolDetails },
    ToolExecutionEnd    { agent_id: AgentId, call_id: String, outcome: ToolOutcome },

    // Sub-agent lifecycle (correlation aid)
    SubAgentStart { parent: AgentId, child: AgentId, task: String },
    SubAgentEnd   { parent: AgentId, child: AgentId, report: Option<String> },

    // Notices / errors / retries
    Notice  { agent_id: AgentId, message: String },
    Warning { agent_id: AgentId, message: String },
    Error   { agent_id: AgentId, message: String },
    StreamRetry { agent_id: AgentId, attempt: usize, delay: Duration, error: String },

    // Final summary, emitted once at the end of a `prompt()` call.
    UsageSummary(UsageSummary),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AgentId { Main, Sub(usize) }
```

`StopReason` mirrors today's stop-reason values (end_turn, tool_use,
max_tokens, error, aborted).

### 1.2 Tool outcomes — structured details

Today: tools format strings and call `ui.display_tool_result*`.
After: tools return structured details and the listener formats them.

```rust
#[derive(Clone, Debug)]
pub struct ToolOutcome {
    /// What goes back to the model as the tool_result content.
    pub model_output: String,
    /// Structured payload for UI rendering. Tool-specific.
    pub details: ToolDetails,
    /// Whether this is an error result (model still sees it,
    /// flagged with the provider's error marker, e.g. `is_error: true`).
    pub is_error: bool,
}

#[derive(Clone, Debug)]
pub enum ToolDetails {
    /// Plain summary + optional collapsible body. Default for read_file,
    /// ls, glob, grep, agent, todo_*, anything that doesn't have a
    /// dedicated variant.
    Text { summary: String, body: Option<String> },

    /// File mutation. Listener renders a syntax-highlighted unified diff.
    Diff { path: String, before: String, after: String },

    /// Bash command output.
    Bash {
        command: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
        truncated: bool,
    },

    /// Sub-agent report. The listener can collapse / expand the report.
    SubAgentReport { agent_id: usize, task: String, report: String },

    /// Todo list snapshot.
    Todos { items: Vec<TodoItem> },

    /// Escape hatch for tools that haven't been given a dedicated variant.
    /// `model_output` already covers the model-facing string; `Json` is
    /// for the UI.
    Json(serde_json::Value),
}
```

`ToolDetails` is `Clone + Send + Sync` and serializable, so it round-
trips through the conversation log.

### 1.3 Tool trait

```rust
pub trait ToolDefinition {
    type Input: JsonSchema + DeserializeOwned + Send;

    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;

    fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> impl Future<Output = Result<ToolOutcome, anyhow::Error>> + Send;

    fn input_schema(&self) -> Value { derive_schema::<Self::Input>() }
}

pub trait ToolContext: Send {
    fn working_directory(&self) -> PathBuf;

    fn get_todo_list(&self) -> Vec<TodoItem>;
    fn set_todo_list(&mut self, todos: Vec<TodoItem>);

    /// Spawn a sub-agent and await its final assistant text. The agent
    /// emits `SubAgentStart` / nested events / `SubAgentEnd` on the same
    /// bus this tool is contributing to.
    fn spawn_agent(&mut self, task: String)
        -> Pin<Box<dyn Future<Output = Result<String, anyhow::Error>> + Send + '_>>;

    /// Push a partial-result update for the in-flight tool call. The
    /// agent loop wraps it in a `ToolExecutionUpdate` event. Cumulative
    /// snapshots, not deltas.
    fn emit_update(&mut self, partial: ToolDetails);

    /// Cancellation token; tools that block must honor it.
    fn cancellation(&self) -> &CancellationToken;
}
```

The `&mut dyn AjUi` parameter and all `display_tool_result*` calls are
gone. Tools no longer know there's a UI.

### 1.4 Agent shape

```rust
pub struct Agent {
    env: AgentEnv,
    system_prompt: &'static str,
    assembled_system_prompt: Option<String>,
    tool_definitions: HashMap<String, ErasedToolDefinition>,
    tools: Vec<Tool>,
    disabled_tools: Vec<String>,
    model: Arc<dyn Model>,
    session_state: SessionState,
    default_thinking: Option<ThinkingConfig>,

    // New
    bus: AgentEventBus,
    cancel: CancellationToken,
}

impl Agent {
    pub fn new(env, system_prompt, tools, disabled_tools, model, default_thinking) -> Self;

    /// Subscribe to events. Returns an unbounded receiver. Multiple
    /// listeners can subscribe; each gets its own receiver. Sub-agents
    /// inherit the same bus.
    pub fn subscribe(&mut self) -> mpsc::UnboundedReceiver<AgentEvent>;

    /// Run one user turn: appends the user message, drives the model
    /// loop until end_turn or error. Events flow on the bus throughout.
    /// Returns when the bus has nothing left to emit for this prompt.
    pub async fn prompt(
        &mut self,
        log: &mut ConversationLog,
        message: String,
    ) -> Result<(), TurnError>;

    /// Re-emit events for an already-persisted log. Doesn't call the
    /// model. Useful on resume.
    pub fn replay(&mut self, log: &ConversationLog);

    /// Cancel the in-flight prompt. Idempotent.
    pub fn cancel(&self);

    pub fn current_turn(&self) -> usize;
    pub fn accumulated_usage(&self) -> &Usage;
}
```

`AgentEventBus` is a small `Vec<UnboundedSender<AgentEvent>>` with
prune-on-emit (drop closed senders). Listeners are awaited or polled
out of band.

### 1.5 Replay

`Agent::replay(log)` walks the persisted conversation entries and emits
the same `AgentEvent`s the original turn produced. The log already
captures user/assistant messages and (after Phase 0) tool details, so
this is mechanical. Critically, the **same listener code** that handles
live events also handles replay — no `display_conversation_history`
special case.

For the conversation log itself: today's `ConversationEntryKind`
includes `UserOutput`. We rename it to `ToolDetails` and persist the
`ToolDetails` produced by the new tool trait. The old `UserOutput` enum
is migrated 1:1 (Notice/Error/ToolResult/ToolResultDiff/ToolError/
TokenUsage/TokenUsageSummary). On-disk JSONL files written before
the migration need a small `serde` rename or a one-shot migration
walker; details in §3.

### 1.6 Sub-agents

Sub-agents share the parent's event bus. The `agent` tool spawns a
child `Agent` with `agent_id = AgentId::Sub(n)` and connects its bus
into the parent's. Every event the child emits carries `Sub(n)` so
listeners can render nested transcripts. The TUI groups sub-agent
events under the parent's `ToolExecutionComponent`.

There's no separate `get_subagent_ui` factory anymore. The bus is the
abstraction; routing happens listener-side.

### 1.7 Permissions

Dropped. The current `AjUi::ask_permission` is dead code (defined and
forwarded but never called by `aj-agent` or any tool), and the new TUI
won't ship per-tool permission gating either. If a tool decides
something is unsafe it returns `is_error: true` from `execute`, and
that's the entire surface.

If we want gating later, the hook to add is a
`Agent::set_before_tool_call` callback that the binary can register.
Not in v1.

---

## 2. Phase 0 — refactor the core

Six small commits, each shippable independently. The existing `aj` CLI
keeps working throughout.

### 2.0 — preparation

- Move the contract types into `aj-agent`. New module
  `aj_agent::events` defines `AgentEvent`, `AgentId`, `StopReason`,
  `TokenUsage`, `UsageSummary`, `SubAgentUsage` (the last three move
  here from `aj-ui`). New module `aj_agent::tool` defines
  `ToolDefinition`, `ToolContext`, `ToolOutcome`, `ToolDetails`,
  `ErasedToolDefinition`. No trait, no impls beyond
  `Clone + Debug + serde`.
- Flip the `aj-tools` dependency: it now depends on `aj-agent` instead
  of `aj-ui`. The trait + erased wrapper move out of `aj-tools`; only
  the 11 tool implementations and `get_builtin_tools()` stay.
- The legacy `AjUi` trait stays in `aj-ui` for now so the refactor
  steps below can land incrementally without breaking the binary.
- Look at a few real on-disk thread files in `~/.aj/threads/` to
  decide whether the planned `UserOutput` → `ToolDetails` serde rename
  is sufficient or whether we need a `schema_version` header.

Commit: `agent: introduce events and tool contract modules; flip tool dep direction`.

### 2.1 — agent emits events alongside AjUi calls

`aj-agent::Agent` gains a private `AgentEventBus`. Every `self.ui.foo(...)`
call gets a parallel `bus.emit(AgentEvent::Foo {...})`. No behavior
change to the CLI yet; we just have a working bus alongside the
existing trait calls.

This step is the biggest read-and-mirror pass. Tests: a `RecordingBus`
listener captures all events for a known prompt; we snapshot the
sequence to lock the protocol.

Commit: `agent: emit AgentEvents through an internal bus`.

### 2.2 — refactor tools to ToolContext + ToolOutcome

For each of the 11 tools (read_file, write_file, edit_file,
edit_file_multi, ls, glob, grep, bash, agent, todo_read, todo_write):

1. Replace `&mut dyn AjUi` with `&mut dyn ToolContext`.
2. Replace the trailing `ui.display_tool_result*` call with
   `Ok(ToolOutcome { model_output, details, is_error: false })`.
3. Per tool, pick the right `ToolDetails` variant:

   | Tool | Variant |
   |------|---------|
   | `read_file` | `Text { summary: "<path><:lines>", body: Some(formatted) }` |
   | `write_file` | `Diff { path, before: "", after }` |
   | `edit_file` | `Diff { path, before, after }` |
   | `edit_file_multi` | `Diff { path, before, after }` (final diff) |
   | `ls` | `Text { summary, body }` |
   | `glob` | `Text { summary, body }` |
   | `grep` | `Text { summary, body }` |
   | `bash` | `Bash { command, stdout, stderr, exit_code, truncated }` |
   | `agent` | `SubAgentReport { agent_id, task, report }` |
   | `todo_read` | `Todos { items }` |
   | `todo_write` | `Todos { items }` |

In `Agent::execute_tool`, we now consume `ToolOutcome` directly:

- Emit `ToolExecutionEnd { outcome }` on the bus.
- Stash `outcome.details` in the conversation log entry that today
  holds `UserOutput::ToolResult{,Diff}`.
- Use `outcome.model_output` as the tool_result content sent back to
  the model.

Tests: per-tool unit tests assert the right `ToolDetails` variant on
known inputs. The existing tool-behavior tests (file contents, exit
codes, etc.) still apply.

Commits, one per tool: `tools/<name>: return ToolOutcome with structured details`.

### 2.3 — drive the legacy CLI off the event bus

`aj` adds a small `EventBridgeListener` that subscribes to the bus and
calls into the existing `AjCli` rendering helpers:

```rust
async fn run_event_bridge(
    cli: &mut AjCli,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
) {
    while let Some(event) = events.recv().await {
        match event {
            AgentEvent::Notice { message, .. }   => cli.display_notice(&message),
            AgentEvent::AssistantTextStart{..}   => cli.agent_text_start(""),
            AgentEvent::AssistantTextDelta{diff,..} => cli.agent_text_update(&diff),
            AgentEvent::AssistantTextStop{text,..}  => cli.agent_text_stop(&text),
            AgentEvent::ToolExecutionEnd{outcome, ..} => render_outcome(cli, outcome),
            // …
        }
    }
}
```

`render_outcome` switches on `ToolDetails` and calls
`display_tool_result` / `display_tool_result_diff` exactly like today.

`Agent::run` and `Agent::run_single_turn` still exist and still own
the user-input loop for the legacy CLI; they just route through the
bus internally. The bridge listener runs on a `tokio::spawn`.

Commit: `aj: bridge AgentEvents to AjCli rendering`.

### 2.4 — flip Agent::run to bus-only

Now that the bridge works, remove `self.ui` from `Agent`. The trait
calls inside `Agent::run` go away; the bus is the only output channel.
`AjUi` stops being used by the agent crate.

`Agent::run` keeps its current shape (it takes a small input provider
trait — see §2.5 — for `get_user_input`) but no longer pushes UI calls.

Commit: `agent: remove AjUi dependency, route everything through the bus`.

### 2.5 — split agent loop from input loop

Today `Agent::run` owns both the model loop and the readline loop.
After this commit:

- `Agent::prompt(log, message)` runs one user turn.
- The legacy `aj` binary owns its own readline loop:

  ```rust
  loop {
      let line = match cli.read_line()? { Some(l) => l, None => break };
      agent.prompt(&mut log, line).await?;
  }
  ```

- `Agent::replay(log)` re-emits the historical events on resume.

This is what unblocks `aj-next`: the same `Agent::prompt` API powers
both the legacy CLI and the new TUI.

Commit: `agent: expose prompt/replay/cancel API; move readline to the binary`.

### 2.6 — cleanup

- Delete `RecordingAjUi` (unused).
- Delete `aj-ui` from the workspace. Remove its entry from the
  workspace `Cargo.toml`. The `AjUi` trait, `UserOutput`,
  `AjUiAskPermission`, and the `Box<dyn AjUi>` impls go with it.
- The `aj` binary's `AjCli` becomes a small `Renderer` struct
  (`pub struct AjCli { common: AjCliCommon, history: Arc<Mutex<…>> }`)
  with plain rendering methods. No trait.

Commit: `aj-ui: delete; replace AjCli with a plain renderer struct`.

End state of Phase 0:

- `aj` binary still works exactly as it does today.
- `aj-agent` exposes a clean event-driven API.
- All tools return structured details.
- The workspace is ready for `aj-next` without any throwaway bridge.

---

## 3. Conversation log migration

The existing log writes `UserOutput` entries as JSONL lines. The new
log writes `ToolDetails` plus a few new `AgentEvent`-derived entries.

Approach: rename `UserOutput` → `ToolDetails` with a `#[serde(rename
= "UserOutput")]` shim on the persistence side for one release, and
ship a one-shot migration walker that rewrites old logs in
`~/.aj/threads/` to the new shape. The variant names stay close
enough that the rename is the bulk of the diff.

If the on-disk shape is messier than expected, we add a
`schema_version` field at the log header and gate the migration on it.
Defer the decision until we look at actual log files in §2.0.

---

## 4. `aj-next` (Phase 1)

Same crate layout as the previous plan, but the `ui_bridge.rs`
disappears — there's nothing to bridge anymore.

```
src/aj-next/
  Cargo.toml
  src/
    main.rs
    lib.rs
    cli/
      args.rs
      file_args.rs
    config/
      mod.rs
      keybindings.rs
      theme.rs
      slash_commands.rs
    modes/
      mod.rs
      print.rs
      interactive/
        mod.rs                 # InteractiveMode
        layout.rs              # named slots
        event_pump.rs          # AgentEvent → component updates
        components/
          assistant_message.rs
          user_message.rs
          tool_execution.rs
          bash_execution.rs
          diff.rs
          loader_status.rs
          footer.rs
          header.rs
          model_selector.rs
          thinking_selector.rs
          session_selector.rs
        footer_data.rs
        keys.rs
        editor_ext.rs
```

`InteractiveMode::run` is a `tokio::select!` over:

- `tui.next_event()` for keyboard / mouse / render-throttle ticks.
- `agent_events.recv()` for `AgentEvent`s.
- `footer_data.tick()` for git-branch refreshes.

When the editor submits, `InteractiveMode` calls
`agent.prompt(&mut log, line).await` on a spawned task and observes
events as they flow.

### 4.1 Sub-agent rendering

The event pump groups events by `agent_id`. For `Sub(n)` events that
arrive between `SubAgentStart` and `SubAgentEnd`, the pump routes them
into the parent's `ToolExecutionComponent` (the `agent` tool's
component is now itself a transcript fragment). Collapsible by
default, expand with `Ctrl+O`.

### 4.2 Print mode

Same `aj-next` binary, no TUI. Subscribes to the same bus and writes a
plain-text rendering to stdout. With `--json`, dumps each event as a
JSONL line. This is the same code that lets us script the agent or
embed it in a parent process.

---

## 5. Phase 2 — cutover

1. Verify `aj-next` reaches behavioral parity for the daily flow:
   `aj` (no args), `aj continue [id]`, `aj list-threads`, replay,
   sub-agents, model switching.
2. Rename the binary `aj-next` → `aj`. Delete the `aj` crate.
3. Drop `rustyline`, `termimad`, `console` from the workspace.
4. Remove `AjCli`, `AjCliCommon`, `cli_sub_agent`, `prompt_history`
   (the latter ports its history-bootstrap logic into `aj-next`'s
   editor wiring).
5. Update `README.md` and `AGENTS.md` to point at `aj-next` paths.

---

## 6. Decisions in effect

- **Crate rename**: `aj-ui` is deleted. Contract types (events, tool
  trait, tool outcome, usage) move to `aj-agent`. Tool implementations
  in `aj-tools` start depending on `aj-agent`.
- **Permission system**: dropped entirely. No `before_tool_call` hook,
  no confirm dialog, no permission channel. A tool that wants to
  refuse returns `is_error: true`.
- **Event bus delivery**: per-subscriber unbounded `mpsc`. Each
  subscriber gets its own receiver; the bus prunes closed senders on
  emit. Back-pressure is handled inside each listener (the TUI's
  render throttle coalesces frames).
- **Tool-update streaming**: `emit_update` lands in §2.2 with only
  `bash` actually using it; the other 10 tools return a single final
  `ToolOutcome`. Cumulative snapshots, not deltas.
- **Log-format migration**: serde rename + one-shot walker that
  rewrites old logs in `~/.aj/threads/` to the new shape. Add a
  schema_version header only if the rename alone proves insufficient
  once we look at real on-disk files in §2.0.
- **User-facing notices**: agent-side notices become
  `AgentEvent::Notice` / `Warning` events. The TUI renders them as
  small banner components; the legacy CLI bridge prints them via
  `display_notice` / `display_warning`.
- **Sub-agents**: share the parent's event bus. Events carry
  `AgentId::Sub(n)`. Listeners group nested transcripts by id.
