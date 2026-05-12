# `aj-next` plan — event-driven core + new TUI

Companion to `docs/models-spec.md`. This plan assumes the wire-layer
rewrite from that spec has already landed: `aj-models` exposes the
unified `Message`, `AssistantMessage`, `AssistantMessageEvent`,
`Usage`, etc. Anything that talks about the wire is defined there;
this doc covers everything above it.

The shape splits cleanly into three layers:

- **Wire layer** (`aj-models`): provider SDKs and unified message
  types. Knows nothing about persistence or UI.
- **Agent layer** (`aj-agent`): the `Agent` runtime, the typed
  `AgentEvent` stream, the tool trait, and an `AgentMessage`
  extension that adds agent-only entries on top of wire `Message`s.
  Knows nothing about persistence or UI.
- **Session layer** (`aj-session`, new): the on-disk thread format,
  `ConversationLog`, replay, and thread navigation. Bridges the
  wire and agent types into JSONL files. Not part of the runtime.

The agent is a state machine that emits typed events; everything
else (TUI, plain CLI, RPC server, persistence, tests) subscribes.
Persistence is just one subscriber, living in its own crate so
multiple frontends can share it without pulling in a UI.

The plan has three pieces:

1. **Phase 0** — split out `aj-session`, refactor `aj-agent` and
   `aj-tools`. The existing `aj` binary keeps working throughout
   via a thin event-to-CLI bridge.
2. **Phase 1** — build `aj-next` on top of the new core. Print mode
   first, then interactive TUI, then selectors and theming.
3. **Phase 2** — cutover. `aj-next` becomes `aj`; `aj-ui` and the
   legacy CLI go away.

---

## 1. The new core shape

Dependency graph after Phase 0:

```
aj-models  ←  aj-agent  ←  aj-tools
               ↑              ↑
               └─  aj-session  ─┘
                       ↑
                 ┌─────┴─────┐
                 aj         aj-next
```

| Crate | Today | After Phase 0 |
|---|---|---|
| `aj-models` | wire + persistence + UI dep | wire only (per `models-spec.md`) |
| `aj-agent` | `Agent<UI: AjUi>`, depends on tools/UI | runtime + contract types (events, tool trait, `ToolDetails`, `AgentMessage`). Depends only on `aj-models`. No UI generic |
| `aj-session` (new) | — | on-disk format, `ConversationLog`, `ConversationView`, `ConversationPersistence`, replay. Depends on `aj-models` + `aj-agent` |
| `aj-tools` | depends on `aj-ui` | tool implementations only; depends on `aj-agent` |
| `aj-ui` | the `AjUi` trait | deleted; types absorbed by `aj-agent` |
| `aj` | rustyline CLI implementing `AjUi` | same binary; UI is now an event subscriber over `AjCli` rendering helpers |
| `aj-next` (new) | — | TUI binary; same core dependencies as `aj`, plus `aj-tui` |

The agent's only reach into `aj-tools` today is `get_builtin_tools()`
during sub-agent spawn. That moves up: the binary builds the tool
list once and passes it to `Agent::new`; sub-agents reuse it minus
the `agent` tool. After Phase 2, `aj` is deleted and `aj-next` is
renamed.

### 1.0 Terminology

| Term | Meaning |
|---|---|
| **Inference** | One streaming LLM call (one `Provider::stream` invocation). |
| **Turn** | One assistant-message cycle, ending when the model emits `Done` (or `Error`/`Aborted`). A turn typically runs several inferences if it contains tool calls. |
| **Agent loop** | The driver inside `Agent::prompt` that runs turns until exit. With steering and follow-up queues a single `prompt` call can span multiple turns. |
| **Input loop** | The binary's outer loop: read user input, call `agent.prompt`, repeat. Lives in the binary, not in `aj-agent`. |

### 1.1 Event stream

`AgentEvent` is intentionally a small set of variants, with the
fine-grained streaming detail surfaced through a nested
`AssistantMessageEvent` (defined in `aj-models`) rather than lifted
into top-level variants. This keeps the agent crate decoupled from
the wire streaming protocol — the agent re-emits whatever the
provider produced.

The variants, grouped:

- **Lifecycle.** `AgentStart`, `AgentEnd { messages }`, `TurnStart`,
  `TurnEnd { message, tool_results }`.
- **Message lifecycle.** `MessageStart { message }`,
  `MessageUpdate { message, event: AssistantMessageEvent }` (only
  during assistant streaming), `MessageEnd { message }`.
  Emitted for user, assistant, and tool-result messages alike;
  `MessageUpdate` only fires for assistant streaming and carries the
  full `partial: AssistantMessage` snapshot inside `event` (see
  `models-spec.md` §2).
- **Tool execution lifecycle.** `ToolExecutionStart { call_id, tool, args }`,
  `ToolExecutionUpdate { call_id, tool, args, partial: ToolDetails }`,
  `ToolExecutionEnd { call_id, tool, result: ToolDetails, is_error }`.
- **Sub-agents.** `SubAgentStart { parent, child, task }`,
  `SubAgentEnd { parent, child, report }` for correlation.
- **Notices.** `Notice { text }`, `Warning { text }`, `Error { text }`,
  `StreamRetry { attempt, delay, error }`. Transient; not persisted.
- **Queue snapshots.** `QueueUpdate { steering, follow_up }` whenever
  either queue changes; full snapshots so the TUI's pending-messages
  indicator renders from a single event.

Every event carries an `agent_id: AgentId` (`Main` or `Sub(usize)`)
so listeners can route sub-agent output into nested transcripts.

The agent loop calls `aj-models::stream_simple`, gets back an
`AssistantMessageEventStream`, and re-emits each event inside
`MessageUpdate`. The `partial` snapshot inside the event already
carries the running text/thinking/tool-call state; the agent doesn't
need to track it separately.

### 1.2 Tool outcomes — `ToolDetails`

Tools today format strings and call `ui.display_tool_result*`. After:
they return structured details, the listener formats them.

```rust
pub struct ToolOutcome {
    /// Content sent back to the model as the tool_result message.
    /// Maps directly onto aj_models::ToolResultMessage.content.
    pub content: Vec<UserContent>,
    /// Structured payload for UI rendering. Tool-specific.
    pub details: ToolDetails,
    pub is_error: bool,
}
```

`ToolDetails` is a closed enum with one variant per rendering shape,
not per tool:

- `Text { summary, body }` — default for `read_file`, `ls`, `glob`,
  `grep`, `agent`, `todo_*`, anything without a dedicated variant.
- `Diff { path, before, after }` — `write_file`, `edit_file`,
  `edit_file_multi`. Listener renders a syntax-highlighted unified
  diff.
- `Bash { command, stdout, stderr, exit_code, truncated, full_output_path }` —
  `bash`. `stdout`/`stderr` carry a bounded rolling tail; if output
  exceeded the cap, the implementation spills the full stream to a
  temp file and surfaces its path here.
- `SubAgentReport { agent_id, task, report }` — the `agent` tool.
- `Todos { items }` — `todo_read`, `todo_write`.
- `Json(Value)` — escape hatch for tools that don't warrant their
  own variant.

`ToolDetails` is `Clone + Send + Sync` and serializable, so it round-
trips through the conversation log alongside the wire `Message`.

**Why a closed enum, not tool-owned rendering.** A reasonable
alternative is to give each `ToolDefinition` a `render_call` /
`render_result` method that returns UI components. We don't:

- The closed enum round-trips through `aj-session` for free — same
  shape on disk, in `--json` print mode, and in replay. Trait-object
  rendering would force tool details to be `serde_json::Value` on
  disk and typed only inside each tool, losing the static guarantee
  at the persistence boundary.
- Listeners stay decoupled from `aj-tui`. If tools owned rendering,
  either `aj-agent` would have to know about `aj_tui::Component` (or
  every frontend would need its own per-tool registry).
- We have 11 builtin tools and no third-party tool extension story
  today. Adding a variant + updating listener `match` arms is bounded.

Trade-off: a new tool with novel rendering needs a new variant.
Escape hatch is `Json(Value)`. A listener-side per-tool renderer
registry can be added in `aj-next` later (keyed by tool name,
without touching this enum).

### 1.3 Tool trait

The trait surface:

- An associated `Input: JsonSchema + DeserializeOwned + Send`.
- `name()`, `description()`, `input_schema()` (default-derived).
- `execute(ctx, input) -> impl Future<Output = Result<ToolOutcome>>`.
- An optional `execution_mode()` returning `Sequential | Parallel`,
  default `Parallel`.

The execution context (`ToolContext`) gives tools:

- `working_directory()`.
- `get_todo_list()` / `set_todo_list(...)`.
- `spawn_agent(task) -> impl Future<Output = Result<String>>` — the
  child runs on the parent's bus and returns its final assistant
  text.
- `emit_update(partial: ToolDetails)` for `ToolExecutionUpdate`
  events. Tools that produce many updates self-throttle (~10/s,
  100ms leading-edge debounce). Tools with potentially-large output
  use bounded buffers and spill to a temp file once the cap is hit.
- `cancellation()` — a `CancellationToken` tools must honor for
  long-running work or external processes (see §1.8).

There is no `&mut dyn AjUi` parameter and no `display_tool_result*`
calls. Tools no longer know there's a UI.

**Per-tool execution mode.** Default is `Parallel`. The agent's
batch-execution rule (§1.5) is: if any tool in a batch is
`Sequential`, the whole batch runs sequentially. Builtin
classification:

| Sequential | Parallel |
|---|---|
| `bash`, `write_file`, `edit_file`, `edit_file_multi` | `read_file`, `ls`, `glob`, `grep`, `agent`, `todo_read`, `todo_write` |

The four sequential tools either mutate the filesystem or run
arbitrary commands; serializing avoids interleaved writes and
output-channel contention.

### 1.4 Agent surface

`Agent` is a runtime handle. The public surface, by responsibility:

- **Construction.** `Agent::new(env, system_prompt, tools, disabled_tools, model, default_thinking)`.
  System prompt is fully assembled by the caller; the agent stores
  it but doesn't persist it (that's `aj-session`'s job). Sub-agents
  share the parent's bus (§1.6).
- **Subscription.** `subscribe(listener)` registers an async closure
  invoked for each event in registration order. Returns a handle
  that drops the subscription. Listeners are awaited inline: a
  listener that returns `Err` fails the run with `TurnError::Fatal`.
  This is the same mechanism for both durable (persistence) and
  best-effort (rendering) consumers — see "hook vs subscriber"
  below.
- **Channel sugar.** `subscribe_channel() -> mpsc::UnboundedReceiver<AgentEvent>`
  is built on top of `subscribe`: it registers a non-blocking-send
  listener and returns the receiver. The TUI uses this; the agent
  is never blocked by a slow renderer.
- **Hooks.** Single-slot setters (`Option<Box<...>>`):
  - `set_after_tool_call(hook)` — invoked after a tool returns,
    before the corresponding `ToolExecutionEnd` and `ToolResult`
    message events fire. Can override `content`, `details`,
    `is_error`, or `terminate`. Use case: redaction, auto-truncation.
  - `set_should_stop_after_turn(hook)` (deferred to v1.1; mentioned
    here so we don't re-litigate the API shape later).
  - `set_before_tool_call(hook)` (permission gating, deferred).
- **Transcript management.** `seed_messages(messages)` replaces the
  current in-memory transcript on resume. Doesn't emit events; the
  caller renders history through `aj-session::ConversationLog::replay`.
  `messages()` returns a slice for projection / persistence.
- **Run.** `prompt(message_or_messages)` runs one user turn (or a
  batch of seed messages). `continue_run()` resumes from the
  existing transcript when the last message is a user or tool-result
  message — used after a recoverable error or abort, to retry
  without re-injecting a prompt.
- **Cancel.** `cancel()` is idempotent; it triggers the agent's
  `CancellationToken`. See §1.8.
- **State accessors.** `current_turn()`, `accumulated_usage()`,
  plus a small `state()` snapshot (`is_streaming`,
  `streaming_message`, `pending_tool_calls`, `last_error`) for the
  TUI.

**Hook vs subscriber pattern.** There is a single `subscribe`
mechanism with awaited listeners; persistence is just one of those
listeners and earns its durability guarantee from "the agent awaits
each listener inline before moving on". Persistence is therefore
one of the listeners the binary registers at startup, returning
`Err` if the on-disk write fails (which fails the prompt).
Rendering listeners use the channel sugar so they never block.

The result is the same "at-most one event behind reality" guarantee
today's code provides via inline `view.add_*` calls, with no special
hook slot.

**`prompt` does not take a `&mut ConversationLog`.** The agent
doesn't know there's a log. The binary owns the log and registers
a persistence listener. This is what makes the agent equally
embeddable from a TUI, CLI, RPC server, or test harness.

### 1.5 Tool-call execution

Per turn, after the assistant message finalizes, the agent collects
its `ToolCall` content blocks into a batch and runs them according
to the per-tool `execution_mode`:

- **Sequential batch.** Any tool in the batch with `Sequential`
  mode forces serial execution: prepare → execute → finalize → emit
  end, one call at a time, in source order.
- **Parallel batch.** All tools `Parallel`: prepare each call
  sequentially (validation, `before_tool_call` hook), then drive
  all `execute` futures concurrently. `ToolExecutionEnd` events
  fire in completion order; the resulting `ToolResultMessage`s are
  appended in source order so the wire transcript stays
  deterministic.

Errors during validation, the (future) before-hook, or `execute`
itself produce a synthesized `ToolDetails::Text` error result with
`is_error: true`. If every tool in the batch returns
`terminate: true`, the agent ends the turn and emits `AgentEnd`
without a follow-up inference.

### 1.6 Sub-agents

The `agent` tool calls `ctx.spawn_agent(task)`; tools never
construct a child `Agent` directly. The implementation lives in
`aj-agent` and:

- Allocates a fresh `AgentId::Sub(n)` from the parent's session
  state.
- Builds the child's tool list as the parent's minus `agent` (no
  recursion).
- Clones the parent's bus, model handle, and system prompt.
- Derives the child's cancellation token from
  `parent.cancel.child_token()` so `parent.cancel()` reaches the
  child recursively.
- Awaits `child.prompt(task).await`, rolls the child's accumulated
  usage into the parent's accounting, and returns the child's last
  assistant text.

Bus sharing means every event the child emits reaches every
listener on the parent's bus, tagged with `Sub(n)`. Listeners
group nested transcripts by id — the persistence listener writes
sub-agent events under `ThreadKind::Subagent` framing in the same
JSONL file (§3); the TUI groups them under the parent's
`ToolExecutionComponent`.

### 1.7 Permissions

Dropped. The current `AjUi::ask_permission` is dead code (defined
but never called). A tool that decides something is unsafe returns
`is_error: true`. The `set_before_tool_call` slot is reserved for
v1.1 if we want gating later — same shape as `set_after_tool_call`.

### 1.8 Cancellation

`Agent::cancel()` is idempotent and triggers the agent's
`CancellationToken`. The token is awaited at two checkpoints inside
the turn loop:

1. **Streaming inference.** The provider's `AssistantMessageEventStream`
   is `select!`-ed against `cancel.cancelled()`. On cancel the
   in-flight HTTP request is dropped, the provider synthesizes
   `Error { reason: Aborted }` per `models-spec.md` §2, and the
   loop returns `TurnError::Aborted`.
2. **Each tool execution.** The future returned by `ToolDefinition::execute`
   is `select!`-ed against the token. On cancel, the future is
   dropped.

Tools observing a long-running body (network I/O, sub-process)
should also `select!` against `ctx.cancellation()` internally.
External processes (`bash`) **must** kill their process group on
cancel — drop alone leaks the child.

**Persistence on cancellation.** Listeners run synchronously for
every event emitted *before* the abort. The agent emits
`AssistantMessage { stop_reason: Aborted, ... }` and synthesizes
`is_error: true` tool-result messages for any in-flight tool calls
before returning, so the persisted log stays internally consistent
— no dangling tool calls without matching results. This eliminates
the cancel-path branch from today's `repair_interrupted_tool_uses`
walker (which still handles the process-kill case where persistence
didn't get a chance to run).

`Agent::prompt` returns `Err(TurnError::Aborted)` on cancel; the
binary's main loop treats it as "continue to next user input"
rather than fatal.

### 1.9 Steering and follow-up queues

While `prompt` is running, the user may want to inject another
message — either to redirect mid-task ("wait, do this instead") or
to schedule work after the current task ("then also clean up the
temp files"). Two queues handle these:

- **Steering** (`steer(message)`): drained at each turn boundary
  (after the assistant turn completes its tool batch, before the
  next inference). Injected messages run with the full prior
  context.
- **Follow-up** (`follow_up(message)`): drained when the agent
  would otherwise exit the loop. Continues the run instead of
  returning from `prompt`.

Both queues take `&self` (interior mutability) because the user
calls them from a different code path than the one driving
`prompt`. A `std::sync::Mutex` is enough — contention is brief.

Queue API: `steer`, `follow_up`, `pending_steering`,
`pending_follow_ups`, `clear_steering`, `clear_follow_ups`.
`QueueUpdate` events fire on every change carrying full snapshots
of both queues.

**Drain mode.** Two modes are supported: `"all"` (drain every
queued message into the next turn at once) and `"one-at-a-time"`
(inject the head of the queue, run a turn, then check again).
We default to `"one-at-a-time"` — interleaving multiple steering
messages across multiple turns gives the model a chance to react
before the next is injected. Configurable via `AgentOptions`.

**Cancellation interaction.** `Agent::cancel()` does not clear the
queues. `clear_steering` / `clear_follow_ups` are separate
operations; the TUI's "double-Esc" cancel can bind both.

---

## 2. Phase 0 — refactor the core

Six commits. The legacy `aj` CLI keeps working throughout.

### 2.0 Preparation

Three structural moves, in order:

- **(a) Extract `aj-session`.** New crate, takes today's
  `aj_models::conversation::*` (`ConversationLog`, `ConversationView`,
  `ConversationPersistence`, `ConversationEntry`,
  `ConversationEntryKind`, `ThreadKind`, `ThreadFilter`, `EntryId`,
  `ConversationError`). Adds a replay module returning
  `impl Iterator<Item = AgentEvent>`. Depends on `aj-models` and
  `aj-agent`.
- **(b) Move contract types into `aj-agent`.** New modules:
  `events` (`AgentEvent`, `AgentId`, queue types), `tool`
  (`ToolDefinition`, `ToolContext`, `ToolOutcome`, `ToolDetails`,
  `ErasedToolDefinition`), `message` (`AgentMessage`,
  `AgentMessageKind`).
- **(c) Flip `aj-tools`'s dependency** to `aj-agent` instead of
  `aj-ui`. Tool implementations + `get_builtin_tools()` only.

`AjUi` stays alive in `aj-ui` through these moves; the bus runs
alongside it for several commits before deletion.

**Reconnaissance step:** look at real on-disk thread files in
`~/.aj/threads/` and decide whether the `UserOutput` → `ToolDetails`
migration is a serde rename or a rewriting walker (see §3).

Commits, in order: `aj-agent: introduce events/tool/message
contract modules` → `aj-tools: depend on aj-agent` → `aj-session:
extract conversation persistence into a new crate`.

### 2.1 Agent emits events alongside `AjUi` calls

`Agent` gains a private bus. Every `self.ui.foo(...)` call gets a
parallel `bus.emit(AgentEvent::Foo {...})`. **No production
subscribers yet** — only tests subscribe. CLI behavior is byte-
identical. A `RecordingListener` test snapshots the event sequence
for a known prompt to lock the protocol.

Commit: `agent: emit AgentEvents through an internal bus`.

### 2.2 Refactor tools to `ToolContext` + `ToolOutcome`

Per tool: replace `&mut dyn AjUi` with `&mut dyn ToolContext`,
return `ToolOutcome { content, details, is_error }`, drop the
trailing `ui.display_tool_result*` call. Pick a `ToolDetails`
variant per the table:

| Tool | Variant |
|---|---|
| `read_file`, `ls`, `glob`, `grep`, `agent` (text result), `todo_read`/`todo_write` (still also emit `Todos`) | `Text` |
| `write_file`, `edit_file`, `edit_file_multi` | `Diff` |
| `bash` | `Bash` |
| `agent` (when run as sub-agent reporter) | `SubAgentReport` |
| `todo_read`, `todo_write` | `Todos` |

The agent consumes `ToolOutcome` directly: emit `ToolExecutionEnd`,
project `content` onto a `ToolResultMessage` for the wire. The
agent does not persist `details` itself — that's the persistence
listener's job.

Commits, one per tool: `tools/<name>: return ToolOutcome with structured details`.

### 2.3 Drive the legacy CLI off the bus

**Atomic swap.** In one commit:

1. Add an `EventBridgeListener` in `aj` that subscribes to the bus
   and calls into existing `AjCli` rendering helpers.
2. Add a persistence listener that translates each event into a
   `ConversationEntry` and appends through `aj-session`.
3. Delete the `self.ui.*` and `view.add_*` calls inside
   `Agent::execute_turn`.

Splitting (3) out would render and persist every event twice for
one commit; splitting it merged would leave `bus.emit` as dead
code. The atomicity matters.

After this commit `Agent::execute_turn` is bus-only.
`Agent::run` and `Agent::run_single_turn` still own the input loop
and call `self.ui.*` for things outside the turn loop (startup
notices, model header, sandbox warning, readline, end-of-session
usage summary). Those move to the binary in §2.5.

The persistence listener returns `Err` on disk failure → fails the
run with `TurnError::Fatal`, preserving today's durability.
The renderer uses the channel sugar (`subscribe_channel` →
`tokio::spawn`) so a slow render never blocks the agent.

Commit: `aj: bridge AgentEvents to AjCli rendering and persistence`.

### 2.4 Flip `Agent::run` to bus-only

Remove `self.ui` and the `&mut ConversationLog` parameter from
`Agent`. The direct `view.add_*` / trait calls inside `Agent::run`
go away; the bus is the only output. `aj-session` is no longer
reached from `aj-agent`.

Commit: `agent: remove AjUi and ConversationLog dependencies`.

### 2.5 Split agent loop from input loop

After this commit:

- `Agent::prompt(message)` runs one turn; `Agent::continue_run()`
  resumes from transcript when the last message is user/tool-result.
- The `aj` binary owns its own readline loop and its own
  `ConversationLog`. On startup it: opens or creates a log,
  registers a persistence listener and a renderer listener,
  replays persisted history through the renderer, calls
  `agent.seed_messages(log.messages())`, then enters the input
  loop calling `agent.prompt(...)`.
- Replay lives on `aj_session::ConversationLog::replay()`, not on
  `Agent`. The agent never reads the log.

Commit: `agent: expose prompt/continue API; move input loop and persistence to the binary`.

### 2.6 Cleanup

- Delete `RecordingAjUi`.
- Delete the `aj-ui` crate from the workspace. `AjUi`,
  `UserOutput`, `AjUiAskPermission`, `Box<dyn AjUi>` impls go with
  it.
- Replace `AjCli` (the trait impl) with a plain `Renderer` struct
  (`pub struct AjCli { common, history }`).

Commit: `aj-ui: delete; replace AjCli with a plain renderer`.

End state of Phase 0: `aj` works exactly as today; each crate has a
single job; tools return structured details; the workspace is ready
for `aj-next` without throwaway bridges.

---

## 3. Conversation log migration

`UserOutput` today carries three unrelated kinds of payload:

| Old `UserOutput` variant | New home |
|---|---|
| `ToolResult`, `ToolResultDiff`, `ToolError` | `ToolDetails` (persisted alongside the tool-result message in `aj-session`) |
| `Notice`, `Error` | transient `AgentEvent::{Notice,Error}` — not persisted |
| `TokenUsage`, `TokenUsageSummary` | recomputed from `AssistantMessage.usage` per `models-spec.md` §1.2; not persisted as a standalone entry |

So `ToolDetails` only replaces the tool-result subset of
`UserOutput`. The rest become events or live on the wire `Message`.
Tool details ride on `ToolResultMessage.details` (see `models-spec.md`
§1.2), notices/errors are transient, usage rides on each assistant
message.

**Migration walker.** `aj-session` ships
`migrate::walk_threads_dir(path)` that runs on first launch of the
new binary against `~/.aj/threads/`:

- `ConversationEntryKind::UserOutput(ToolResult/Diff/Error)` →
  `ConversationEntryKind::ToolDetails(...)` keyed by the most recent
  preceding tool_use id.
- `ConversationEntryKind::UserOutput(Notice/Error)` → discarded
  (they were always presentational).
- `ConversationEntryKind::UserOutput(TokenUsage*)` → discarded
  (recomputed from `AssistantMessage.usage`).
- `MessageParam` content blocks → unified `Message` per
  `models-spec.md` (this part runs as a separate one-shot pass
  alongside the `aj-models` rewrite).

Each migrated file gets a `.bak` sibling. Migration is one-way; no
downgrade. If a `schema_version` field becomes warranted after the
§2.0 reconnaissance, add it at the log header and gate the walker
on it.

---

## 4. `aj-next` (Phase 1)

Crate layout:

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
    persistence.rs           # registers a persistence listener via Agent::subscribe
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

Dependencies: `aj-agent`, `aj-tools`, `aj-session`, `aj-tui`,
`aj-conf`, `aj-models`.

`InteractiveMode::run` is a `tokio::select!` over: `tui.next_event()`
for keyboard / mouse / render-throttle ticks; the event-pump receiver
from `agent.subscribe_channel()`; `footer_data.tick()` for
git-branch refreshes.

On startup, `InteractiveMode` opens or creates a `ConversationLog`
through `aj-session`, registers a synchronous persistence listener
on the agent (`agent.subscribe(persistence_handler)`), drives
`log.replay()` through the same event pump that handles live
events, then calls `agent.seed_messages(log.messages())` before
entering the main select loop.

When the editor submits, `InteractiveMode` calls
`agent.prompt(message)` on a spawned task and observes events as
they flow.

### 4.1 Sub-agent rendering

The event pump groups events by `agent_id`. Events arriving between
`SubAgentStart` and `SubAgentEnd` for `Sub(n)` are routed into the
parent's `ToolExecutionComponent`. Collapsible by default, expand
with `Ctrl+O`.

### 4.2 Print mode

Same `aj-next` binary, no TUI. Subscribes to the same bus, writes
plain text to stdout. With `--json`, dumps each event as a JSONL
line. Same code path lets us script the agent or embed it in a
parent process.

### 4.3 Settings persistence and selectors

The `/settings` dialog, the `/model` selector, and the thinking-level
selector are all thin views over an in-memory `SettingsManager` owned
by `aj-conf`. The file
format is TOML, building on the existing `aj-conf` `Config` loader
that already reads `~/.aj/config.toml`.

- **On-disk layout.** Global `~/.aj/config.toml`, project
  `<cwd>/.aj/config.toml`. Effective settings are a deep merge with
  project overriding global. (Project-scope is new — today only the
  global file exists; the loader gains a project-scope read.)
- **Load.** `SettingsManager` is constructed at startup and holds
  the parsed global + project structs plus the merged view. It
  replaces / wraps the current `aj-conf::Config::load`.
- **Re-read on dialog open.** Opening the `/settings`, `/model`,
  or thinking dialog re-reads both TOML files from disk before
  populating the view. We prefer this fresher read (over relying on
  an in-memory snapshot until an explicit `/reload`) so a user
  editing the file in another window — or a concurrent `aj`
  process — doesn't get clobbered by a stale snapshot when the
  user toggles an unrelated row.
- **Session state vs persisted default.** The session keeps its own
  active model and thinking level (whatever it started with, plus
  any in-session switches). The persisted defaults in `config.toml`
  are a separate piece of state: they're what *new* sessions pick
  up at startup. On open, the `/model` and thinking selectors
  highlight the **session's active value** as the current choice
  (so the dialog reflects "what this session is using right now"),
  while the list of available entries and any other displayed
  metadata come from the freshly re-read TOML. Picking an entry
  (a) writes the new default to disk and (b) switches the current
  session over to it. Not picking anything leaves both untouched.
- **Write policy.** No Save button. Each row's callback calls a
  typed `SettingsManager::set_*` setter, which:
  1. mutates the in-memory scoped struct,
  2. marks the field modified,
  3. enqueues a serialized write task.
  The write task takes a **file lock** (`fs2` / `fd-lock` advisory
  lock on the TOML file), re-reads the on-disk file as a
  `toml_edit::DocumentMut`, patches only the fields modified in
  this session into the document (preserving comments, key order,
  and unknown keys), and writes the result back atomically
  (tempfile + rename in the same directory). This means:
  - Unknown / future settings keys round-trip untouched.
  - Hand-written comments and formatting are preserved.
  - Concurrent edits to *other* fields by another process or editor
    are preserved.
  - Writes are serialized through an internal queue so two rapid
    toggles can't race.
- **Save queue.** A single background task per scope drains the
  write queue; setters are fire-and-forget from the UI thread. A
  `flush()` awaits the queue (used on shutdown and from tests).
- **Model selector persistence.** Selecting a model writes
  `default_provider` + `default_model` to global `config.toml`
  immediately. There is no session-only mode in the UI. A future `--models` CLI scope flag can restrict the picker without
  changing this — picking still persists the default.
- **Thinking-level persistence.** Setting the thinking level (via
  the `/settings` submenu, the cycle hotkey, or the standalone
  selector) writes `default_thinking_level` (the existing
  `thinking` key in `config.toml`) to global config, after clamping
  to the current model's supported levels. Skip the write only when
  the new level is `Off` and the active model doesn't support
  thinking, so we don't stomp the user's preferred level while
  temporarily on a non-reasoning model.
- **Session-only fields.** A small set of fields are deliberately
  session-only and never persisted: auto-compaction toggle, anything
  else flagged as transient. These live on the session, not on
  `SettingsManager`.
- **Reload command.** `/reload` still exists and forces a full
  re-read plus re-resolution of derived state (resource loader,
  theme, keybindings); the per-dialog re-read is a narrower path.

Crate placement: `SettingsManager`, the TOML schema types, and the
file-lock/RMW machinery live in `aj-conf`. `aj-conf` gains a
`toml_edit` dependency for format-preserving writes (it already
depends on `toml` for reads). The selector components in `aj-next`
depend on `aj-conf` for read/write and on `aj-models` for the model
list.

### 4.4 Thinking-block rendering

The assistant message component owns rendering for the
[`StreamChannel::Thinking`] channel. Two display modes, both fed by
the same cumulative thinking snapshot the event pump pushes into
the component:

- **Expanded** (default). The thinking snapshot is rendered as
  italic markdown using the `thinkingText` theme color. The
  widget's [`DefaultTextStyle`] sets `italic: true` and
  `color = theme.fg_closure(ThemeColor::ThinkingText)` so a theme
  reload reskins the block without rebuilding the widget. This is
  the verbose mode — the user reads the model's reasoning prose
  inline above the visible answer.
- **Collapsed**. The thinking snapshot is replaced by a single
  italic line in the same `thinkingText` color reading `Thinking…`
  (one-line `Text` component, `padding_x = 1`, `padding_y = 0`).
  This stands in for the entire thinking block regardless of how
  long the streaming snapshot is. The collapsed line never
  animates and carries no spinner — the global working indicator
  is the only motion the user sees.

Both modes use the same `AssistantMessageComponent` instance and
the same buffer, so flipping between them is a render-time
decision that takes effect on the next paint. Live streaming and
replay are visually identical for a given setting: the component
makes no distinction between an in-flight `StreamChunk` snapshot
and a replayed terminal snapshot.

**Spacing rule.** When the component renders both a thinking
block (expanded or collapsed) and a visible text block, a single
blank row separates them. When only thinking is present (e.g. a
turn that emits thinking and then a tool call, with no visible
text yet), no trailing row is emitted — the chat container's
auto-spacer handles the gap before the next sibling (the tool
execution component). This matches the existing per-message
layout and avoids a stray blank row between thinking and a
following tool call.

**Persistent setting.** A new `hide_thinking_block: bool` field
on `aj-conf::Config` (TOML key `hide_thinking_block`, default
`false`) determines the startup mode. The setting is global only
(no project override). When `SettingsManager` lands in §4.3 the
field migrates onto it without changing the TOML key or the
default; the field is documented in the `Config` struct so the
shape is preserved across the SettingsManager transition.

**Runtime toggle.** `Ctrl+T` toggles the mode for the active
session. The handler:

1. Flips the in-memory `EventPump::hide_thinking_block` flag.
2. Walks the chat container's children and calls
   `AssistantMessageComponent::set_hide_thinking_block(hide)` on
   every assistant message, in-flight or finalized, so the next
   render reflects the new mode for both the current streaming
   turn and every historical message in scrollback.
3. Invalidates the TUI to flush any cached render bytes that
   captured the previous mode.
4. Surfaces a chat notice (`Thinking blocks: hidden` or
   `Thinking blocks: visible`) so the user gets immediate
   feedback that the keystroke was consumed.

Toggling does not persist to disk in this commit — persistence
lands together with the rest of the §4.3 settings machinery, at
which point this toggle becomes a `SettingsManager::set_*` call
that round-trips through the same RMW pipeline as the other
settings. Until then, the runtime toggle is session-only and the
`config.toml` value is read once at startup. The `/settings`
dialog (§4.3) will surface the same flag as a row, matching the
keybinding behavior.

**Print mode.** Print text mode continues to suppress thinking
entirely (per `modes/print.rs`); JSON mode continues to emit it
as part of the event stream. The `hide_thinking_block` setting
has no effect outside the interactive TUI.

**Theme integration.** Both bundled themes (`dark`, `light`)
already ship a `thinkingText` color; the assistant message
component pulls it through the `ChatTheme` bundle so the same
closure feeds the expanded-mode markdown style and the
collapsed-mode `Text` widget. User themes that don't define
`thinkingText` fall back to the bundled theme value via the
existing JSON-loader fallback.

---

## 5. Phase 2 — cutover

1. Verify `aj-next` reaches behavioral parity for the daily flow:
   `aj` (no args), `aj continue [id]`, `aj list-threads`, replay,
   sub-agents, model switching.
2. Rename the binary `aj-next` → `aj`. Delete the old `aj` crate.
3. Drop `rustyline`, `termimad`, `console` from the workspace.
4. Remove `AjCli`, `AjCliCommon`, `cli_sub_agent`, `prompt_history`
   (port history bootstrap into `aj-next`'s editor wiring).
5. Update `README.md` and `AGENTS.md` to point at `aj-next` paths.

`aj-session` survives the cutover unchanged.

---

## 6. Decisions in effect

- **Crate split:** `aj-ui` deleted; contract types live in
  `aj-agent`; persistence in `aj-session`; `aj-models` is wire-only
  (per `models-spec.md`).
- **No persistence dependency in `aj-agent`:** `Agent::prompt`
  takes no log argument. The binary registers a listener via
  `agent.subscribe(...)`.
- **Single subscription mechanism:** `subscribe(listener)` with
  awaited async listeners in registration order. A listener
  returning `Err` fails the run. Channel-based subscribers are
  built on top via `subscribe_channel`. Persistence is just one
  listener — its inline-and-fail semantics give us the same
  "at-most one event behind reality" guarantee today's code has.
- **Hooks now (single-slot, `Option<Box<...>>`):** `set_after_tool_call`.
  Reserved for later: `set_before_tool_call`, `set_should_stop_after_turn`.
- **Permissions:** dropped. Tools return `is_error: true` to refuse.
- **Tool execution:** parallel by default; per-tool opt-out
  (`Sequential` for `bash`, `write_file`, `edit_file`,
  `edit_file_multi`). A batch with any sequential tool runs serially.
- **Tool-update streaming:** `ctx.emit_update` lands in §2.2 with
  only `bash` actually using it; cumulative snapshots, not deltas;
  ~100ms leading-edge debounce; bounded buffers with temp-file
  spillover for unbounded output.
- **Persistence policy:** the persistence listener writes only
  terminal events: user messages, finalized assistant messages,
  tool-result messages with their `ToolDetails`, frozen system
  prompt. Streaming `MessageUpdate` and `ToolExecutionUpdate`
  events flow on the bus but never hit disk. UI listeners see all
  events.
- **Log-format migration:** owned by `aj-session`. Three-way split
  of old `UserOutput` (tool details / transient / per-message
  usage), one-shot walker, `.bak` per file. One-way; no downgrade.
- **Sub-agents:** share the parent's bus tagged with `AgentId::Sub(n)`.
  Cancellation is hierarchical via `child_token()`. Persistence
  listener writes them under `ThreadKind::Subagent` framing in the
  same JSONL file.
- **Steering and follow-up queues:** default `"one-at-a-time"`
  drain mode; `"all"` available via `AgentOptions`.
  `QueueUpdate` events carry full snapshots.
- **Continuation:** `Agent::continue_run()` resumes from the
  current transcript when the last message is a user or tool-result
  message. Used after recoverable errors or aborts to retry without
  re-injecting a prompt.

---

## 7. Deferred capabilities

Capabilities we deliberately skip in v1, listed so their absence is
intentional rather than accidental:

- **`shouldStopAfterTurn` hook.** "Stop after this turn, e.g.
  before the context window fills up." API shape reserved as
  `Agent::set_should_stop_after_turn`.
- **`beforeToolCall` hook.** Pre-execute gating (permission /
  policy). API shape reserved as `Agent::set_before_tool_call`.
- **`transformContext` / `convertToLlm` hooks.** Host-supplied
  rewrites of the `AgentMessage[]` → `Message[]` projection. We
  bake the projection into the agent (matches our single binary).
  If context-window-aware truncation lands later it can become a
  hook.
- **Per-call `getApiKey` resolver.** Resolving the provider key
  per inference so OAuth tokens can refresh during long tool
  executions. Today our model handle holds the key. Worth wiring
  through `aj-models`' auth layer once the OAuth flows from
  `models-spec.md` §9 land.
- **Session forking (`fork(entry_id)`).** Branching a thread at an
  entry. `aj-session`'s on-disk format already keys entries by
  `EntryId` and tracks `parent_id`, so nothing here precludes
  forking later.
- **Extension framework.** A plugin surface that lets third parties
  inject tools and event handlers. Out of scope; the closed
  `ToolDetails` enum is the explicit constraint that motivates
  this.
