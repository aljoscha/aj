# Context Compaction Spec

Status: proposed. This document specifies client-side context
compaction for `aj`: a manual `/compact` command, automatic
threshold-driven compaction, and reactive recovery from a
context-overflow error. Compaction replaces the earlier part of a
thread with an LLM-generated structured summary while keeping a recent
tail of messages verbatim, so a long-running session stays within the
model's context window without losing the thread of work.

The design is host-orchestrated: the binary (`aj`) owns the policy and
the end-to-end flow, the on-disk log is the source of truth for the
compaction boundary, and the agent runtime gains two small, generic
mechanisms (a bus-silent completion and a transcript reseed). This
mirrors the existing split where the binary owns the `ConversationLog`
and appends settings entries directly while the agent owns the live
transcript and emits events.

## 1. Goals and non-goals

Goals:

- Summarize-and-continue: when context grows past a threshold, replace
  the old prefix with a summary and keep recent messages verbatim.
- Three triggers: manual (`/compact [instructions]`), automatic
  (occupancy crosses a configured fraction of the window), and reactive
  (a turn fails with a context-overflow error → compact and retry once).
- Durable and resumable: a compaction is recorded in the session log so
  resuming a compacted thread reconstructs exactly the post-compaction
  context, and the full pre-compaction history remains on disk.
- Iterative summaries: a second compaction folds new history into the
  previous summary rather than summarizing a summary.
- Available in both the interactive TUI and the headless `--print`
  path (the reactive overflow trigger fires under `--print`).

Non-goals:

- Surfacing a provider's *server-side* compaction (e.g. Anthropic's
  `compaction` stop reason). The unified `StopReason`
  (`aj-models/src/types.rs:163`) has no `Compaction` variant and this
  spec does not add one. Client-side summarize-and-reseed is the path
  that fits aj's abstractions.
- Branch summarization (summarizing a forked thread that rejoins its
  parent). The log is already a parent-linked tree, so this can be added
  later with the same machinery; it is out of scope here.

## 2. Where each piece lives (crate boundaries)

```
aj-session::compaction   pure planning: token estimation, cut-point
                         selection, conversation serialization, summary
                         prompt templates, file-op extraction. Operates
                         on `&[ConversationEntry]` / `&[Message]`. No
                         provider, no I/O. Fully unit-testable.

aj-session::log          `ConversationEntryKind::Compaction` variant,
                         `append_compaction(...)`, and compaction-aware
                         projection in `Conversation::agent_messages` /
                         `messages`.

aj-session::replay       replay arm mapping a `Compaction` entry onto
                         renderer events.

aj-agent                 generic mechanisms only: `Agent::complete_oneshot`
                         (bus-silent completion) and `Agent::reseed_transcript`;
                         the `CompactionStart` / `CompactionEnd` events.

aj-conf                  `auto_compact` + `compact_threshold` config
                         options; `ValueKind::Number`.

aj (binary)              orchestration (the `compaction` host module),
                         the `/compact` command, the threshold and
                         overflow triggers, interactive wiring (spawned
                         task, footer accessor, notices), print-mode
                         recovery.
```

Rationale: the cut-point and summary-prompt logic operates over log
entries (to produce a durable `EntryId` anchor) and is pure, so it lives
in `aj-session` alongside the log it reasons about. The summarizer
*inference* needs the agent's provider/model/auth, which are private to
the `Agent`; rather than expose them, the agent gains one generic
"complete this prompt out of band" method. Policy (thresholds, when to
fire, the user-facing command) lives in the binary, which already owns
the log handle and the config.

## 3. Data model

### 3.1 `ConversationEntryKind::Compaction`

Add a variant to `ConversationEntryKind` (`aj-session/src/log.rs:106`):

```rust
/// A compaction checkpoint: the thread's history before
/// `first_kept_entry_id` was summarized into `summary`. Projection
/// (`Conversation::agent_messages` / `messages`) replaces that prefix
/// with a single synthetic summary message and keeps everything from
/// `first_kept_entry_id` onward verbatim. The summarized entries stay
/// on disk — compaction changes only the projection, never deletes
/// lines.
Compaction {
    /// LLM-generated structured summary that stands in for the
    /// summarized prefix.
    summary: String,
    /// First retained entry. Everything strictly before it on this
    /// thread (back to the previous compaction boundary, or the
    /// thread root) is represented by `summary`.
    first_kept_entry_id: EntryId,
    /// Estimated context tokens before this compaction ran. Carried
    /// for the UI ("freed ~N tokens") and telemetry; not used by
    /// projection.
    tokens_before: u64,
    /// Files read / modified in the summarized range, surfaced so the
    /// model knows what was touched without parsing the prose. `None`
    /// when extraction found nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    details: Option<CompactionDetails>,
},
```

`CompactionDetails` lives in `aj-session::compaction` (§4.6) and is
serialized verbatim onto the entry.

### 3.2 Durability: `is_punctuation`

A `Compaction` entry must be durable on its own — if a session is
compacted and then abandoned before the next message, the compaction
must still be on disk so the next resume sees the reduced context.
Therefore `ConversationEntryKind::is_punctuation`
(`aj-session/src/log.rs:163`) returns `true` for `Compaction`, flushing
it (and any buffered non-punctuation entries) immediately, exactly like
a `Message`.

### 3.3 Compaction-aware projection

`Conversation::agent_messages` (`aj-session/src/log.rs:301`) and
`messages` (`:288`) are the only readers that turn the linearized entry
chain into the message list the agent/provider consume. Both become
compaction-aware with one shared helper. Algorithm over the linearized
(chronological) `entries`:

1. Find the index `c` of the **last** `Compaction` entry. If none, the
   projection is unchanged (today's filter-map over `Message` entries).
2. Read `first_kept_entry_id = K` from entry `c`; find `K`'s index `k`.
   (If `K` is missing — a corrupt/edited log — fall back to projecting
   from `c+1`, dropping nothing extra, and log a warning.)
3. Emit a single synthetic **summary message** built from entry `c`'s
   `summary` (§3.4), then project every `Message` entry at index `>= k`,
   skipping the `Compaction` marker itself (it is not a `Message`, so
   the existing filter naturally skips it).

Why "last compaction wins": a second compaction's `summary` is generated
with the previous summary as input (the update prompt, §4.5), so it
already subsumes it, and its `first_kept_entry_id` points past the
previous boundary. Taking only the latest compaction's summary plus its
retained tail therefore reconstructs the full reduced context.

Why this slices correctly on disk: a `Compaction` entry is appended with
`parent_id` = the thread's current head, so it sits *after* the retained
tail in append order, and subsequent turns chain off it. The retained
tail `[K .. head]` lands between `K` and the marker; post-compaction
turns land after the marker; everything before `K` is dropped. The
walk is linear and needs no special tree traversal.

This shared helper is the single seam where compaction affects what the
model sees; `transcript_to_messages` (`aj-agent/src/projection.rs:26`)
stays a trivial clone-and-collect because the agent's transcript is
*already* the reduced projection after a reseed (§6.2).

### 3.4 Summary wire representation

The synthetic summary message is a plain user-role wire message wrapped
in a fixed prefix/suffix so the model reads it as context rather than an
instruction:

```
The conversation history before this point was compacted into the
following summary:

<summary>
{summary}
</summary>
```

It is built as `AgentMessage::wire(Message::User(UserMessage::text(...)))`.
We deliberately do **not** add a new `AgentMessageKind` variant: the
summary is a normal user message on the wire, and the renderer
distinguishes it via the replay path (§8), not via the transcript shape.
The `COMPACTION_SUMMARY_PREFIX` / `COMPACTION_SUMMARY_SUFFIX` constants
live in `aj-session::compaction` and are used by the projection helper.

NOTE: framing the summary as a user message matches the established
pattern for injected context and keeps `transform_messages` happy (it is
just text). An assistant-role summary would be demoted/dropped across
cross-model replay by transform rule 2/5.

### 3.5 `ConversationLog::append_compaction`

The binary's log-append surface is settings-only today
(`append_model_change` / `append_thinking_change` / `append_speed_change`,
`log.rs:774-817`). Add a sibling for compaction:

```rust
/// Record a compaction checkpoint on `filter`'s thread, anchored at
/// the thread's current leaf. Punctuation: flushes immediately
/// (see `ConversationEntryKind::is_punctuation`). `first_kept_entry_id`
/// must be an existing entry on the same thread.
pub fn append_compaction(
    &mut self,
    filter: ThreadFilter,
    summary: String,
    first_kept_entry_id: EntryId,
    tokens_before: u64,
    details: Option<CompactionDetails>,
) -> Result<EntryId, ConversationError>;
```

It anchors at `latest_leaf(filter)` (falling back to the system-prompt
root) exactly like `append_settings_entry` (`log.rs:852`), validates
that `first_kept_entry_id` exists, and appends a `Compaction` entry.

## 4. Compaction planning library (`aj-session::compaction`)

A new module of pure functions. No provider, no async, no I/O — it takes
entries/messages and returns a plan. This is where the bulk of the
unit tests live.

### 4.1 Token estimation

Two sources, in priority order, matching how aj already measures
occupancy:

- **Authoritative**: the most recent assistant message's `usage`
  (`input + cache_read + cache_write`) — the real prompt size the
  provider reported. This is the same numerator the footer uses
  (`footer_data.rs:113`).
- **Heuristic fallback** for messages with no usage (or trailing
  messages after the last assistant turn): `ceil(chars / 4)`, counting
  text/thinking/tool-call argument characters, with a fixed
  `ESTIMATED_IMAGE_CHARS` charge per image block.

```rust
/// Estimate context tokens for a single wire message (heuristic).
pub fn estimate_message_tokens(message: &Message) -> u64;

/// Estimate the context tokens a linearized message list occupies,
/// preferring the last assistant `usage` and estimating only the
/// trailing messages after it.
pub fn estimate_context_tokens(messages: &[Message]) -> ContextEstimate;

/// Compaction-aware occupancy for a whole `Conversation`.
pub fn estimate_conversation_context(conversation: &Conversation) -> ContextEstimate;
```

The usage anchor needs one correction once compaction enters the
picture: a retained assistant message's `usage` measures the prompt as
it was *when that turn ran*, including history that a later compaction
has since summarized away. So immediately after a compaction (no real
turn has run since), the most recent assistant `usage` over-reports by
the entire summarized prefix. `estimate_conversation_context` guards
against this: when a `Compaction` is the most recent entry among
{compaction, assistant message}, it estimates the projected messages
(summary + retained tail) purely heuristically instead of anchoring on
the stale usage; otherwise it defers to `estimate_context_tokens` over
the projection. Both compaction `tokens_before` / `tokens_after` and the
resumed footer occupancy go through it, so the reported numbers match
what the next turn actually sends.

### 4.2 The trigger predicate

```rust
/// Whether occupancy has crossed the configured fraction of the window.
/// `threshold` is a fraction in (0, 1]; `context_tokens` and
/// `context_window` are absolute token counts.
pub fn should_compact(context_tokens: u64, context_window: u64, threshold: f64) -> bool {
    context_window > 0 && (context_tokens as f64) > (context_window as f64) * threshold
}
```

The numerator at runtime is the footer occupancy
(`turn_input + turn_cache_read + turn_cache_write`); the denominator is
`agent.model_info().context_window`. Default `threshold` is `0.85`
(§9).

### 4.3 Cut-point selection

Given the linearized entries and a `keep_recent_tokens` budget, choose
the first retained entry. Constraints, mirroring the reference design:

- **Valid cut points** are user- or assistant-message starts (and a
  prior compaction's summary boundary). A `tool_result` is **never** a
  cut point: keeping a `tool_result` whose `tool_call` was summarized
  away would orphan it on the wire and providers reject that. (The
  transform layer synthesizes results for orphaned *calls* but cannot
  un-orphan a *result*.)
- **Keep-recent tail**: walk backward from the head accumulating
  estimated tokens until `keep_recent_tokens` is reached, then snap the
  cut to the nearest valid cut point at or before that position.
- **Boundary start**: when a previous `Compaction` exists, the
  summarized range starts at that compaction's `first_kept_entry_id`
  (not the thread root) and its `summary` is fed to the update prompt
  (§4.5).
- **Split turn**: if the chosen cut lands inside a turn (the cut point
  is an assistant message mid-turn, not a turn-starting user message),
  the turn's prefix is summarized separately and appended to the main
  summary under a "Turn Context (split turn)" heading, so the retained
  suffix still has the turn's setup.

```rust
pub struct CutPoint {
    pub first_kept_entry_id: EntryId,
    pub first_kept_index: usize,
    pub turn_start_index: Option<usize>, // Some when the cut splits a turn
}

pub fn find_cut_point(
    entries: &[ConversationEntry],
    boundary_start: usize,
    keep_recent_tokens: u64,
) -> Option<CutPoint>;
```

`keep_recent_tokens` is a fixed token budget (`compact_keep_recent`,
default `20_000`), not a fraction of the window: the summarized range
depends only on how much recent context we want to retain, independent
of the model. With a 0.85 trigger and a small recent tail plus a short
summary, post-compaction occupancy lands well under the threshold,
leaving headroom so the next turn does not immediately re-trigger.

### 4.4 Conversation serialization

The summarizer is fed a *flattened text* transcript, never the raw
message list, so the request can't trip provider tool-call/tool-result
pairing rules and never re-sends image bytes:

```rust
/// Render a linearized message list into a plain-text transcript
/// (role-labeled, tool calls and results inlined as text, images noted
/// as placeholders) for embedding in the summarizer prompt.
pub fn serialize_conversation(messages: &[Message]) -> String;
```

### 4.5 Summary prompts

Three prompt builders plus a shared system prompt, all pure string
construction. The prompts ask for a structured checkpoint with stable
section headings (goal, constraints/preferences, progress
done/in-progress/blocked, key decisions, next steps, critical context)
and stress preserving exact file paths, identifiers, and error
messages.

```rust
pub const SUMMARIZATION_SYSTEM_PROMPT: &str;

/// First compaction on a thread.
pub fn initial_summary_prompt(conversation_text: &str, custom: Option<&str>) -> String;

/// Subsequent compaction: fold new history into the previous summary.
pub fn update_summary_prompt(conversation_text: &str, previous_summary: &str, custom: Option<&str>) -> String;

/// Summarize a split-turn prefix.
pub fn turn_prefix_summary_prompt(conversation_text: &str) -> String;
```

`custom` carries the optional `/compact <instructions>` focus text.

### 4.6 File-op extraction and `CompactionDetails`

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Scan summarized messages for file operations, keyed off the builtin
/// tool names (`read_file`, `edit_file`, `edit_file_multi`,
/// `write_file`) and their `path` arguments. Carries forward a prior
/// compaction's details so the running lists don't lose earlier files.
pub fn extract_file_ops(messages: &[Message], previous: Option<&CompactionDetails>) -> CompactionDetails;
```

The resolved read/modified lists are appended to the summary text (so
the model sees them) *and* stored on the entry's `details` (so the next
compaction carries them forward). Tool names are passed in / kept as
module constants to avoid a hard dependency on `aj-tools`.

### 4.7 The plan type

```rust
/// Everything the host needs to run one compaction, computed purely
/// from the linearized log.
pub struct CompactionPlan {
    pub first_kept_entry_id: EntryId,
    pub messages_to_summarize: Vec<Message>,
    pub turn_prefix_messages: Vec<Message>, // empty unless split turn
    pub previous_summary: Option<String>,
    pub tokens_before: u64,
    pub file_ops: CompactionDetails,
}

/// Build a plan, or `None` when compaction is not applicable (nothing to
/// summarize, or the last entry is already a `Compaction`).
pub fn prepare_compaction(
    conversation: &Conversation,
    keep_recent_tokens: u64,
) -> Option<CompactionPlan>;
```

`prepare_compaction` finds the previous compaction boundary, computes
`tokens_before`, runs `find_cut_point`, and collects the message ranges.
It does **not** call the model — summary generation is the host's job
(§7.1), because it needs the provider.

## 5. (reserved)

## 6. Agent runtime additions (`aj-agent`)

The agent gains two generic mechanisms and two events. Nothing
compaction-specific (no prompts, no thresholds) lives in `aj-agent`.

### 6.1 `Agent::complete_oneshot`

```rust
/// Run a single, bus-silent completion against the agent's provider and
/// return the concatenated assistant text. Does not touch the
/// transcript, emits no `Message*` / `TurnUsage` events (so persistence
/// never sees it), and does not accumulate usage. Honors the supplied
/// cancellation token.
///
/// Used for out-of-band model calls — today, generating a compaction
/// summary. `max_tokens` caps the response; `system_prompt` and the
/// single user `text` define the request.
pub async fn complete_oneshot(
    &self,
    system_prompt: &str,
    text: String,
    max_tokens: u64,
    cancel: CancellationToken,
) -> Result<String, TurnError>;
```

It builds a `Context { system_prompt, messages: [user(text)], tools: [] }`
and drives `provider.stream_simple` with the agent's `model_info` and a
clone of `stream_options` (output capped at `max_tokens`, thinking left
at the agent's default unless the summary budget is too small), collects
the streamed text, and returns it. Because it takes `&self` and emits
nothing, the binary can call it while holding the agent lock between
turns without disturbing transcript or persistence.

NOTE: it is bus-silent by design. We do *not* want a summarizer turn to
produce a `MessageEnd` (which the persistence listener would write to the
log) or a `TurnUsage` (which would move the footer occupancy). The
summary's only durable record is the `Compaction` log entry.

### 6.2 `Agent::reseed_transcript`

`seed_session` (`lib.rs:440`) is contracted as call-once-before-first-turn
and also sets the system prompt and sub-agent counter. Compaction needs
to replace the transcript on a *live, shared, between-turns* agent.
Add a focused method:

```rust
/// Replace the in-memory transcript wholesale. Contract: call only
/// while no turn is in flight (the caller holds the agent lock and is
/// not inside `prompt` / `wake` / `continue_run`). Used by host-driven
/// compaction to install the reduced post-compaction projection; the
/// durable record is the log's `Compaction` entry, from which an
/// identical transcript is reconstructed on resume.
pub fn reseed_transcript(&mut self, transcript: Vec<AgentMessage>);
```

The system prompt and sub-agent counter are untouched (compaction
changes neither).

### 6.3 Compaction events

Add to `AgentEvent` (`aj-agent/src/events.rs`), both carrying `agent_id`:

```rust
/// Compaction has started for this agent. Renderers show a
/// "compacting…" indicator. Transient — not persisted.
CompactionStart { agent_id: AgentId, reason: CompactionReason },

/// Compaction finished. `tokens_before` / `tokens_after` are the
/// estimated occupancy on either side (for a "freed ~N tokens"
/// notice). `summary` is the generated text so the renderer can show
/// a compaction-summary row live (resume gets it from the log via
/// replay, §8). Transient — not persisted; the `Compaction` log
/// entry is the durable record. `error` is set when compaction failed
/// (e.g. summarizer error) and nothing was written.
CompactionEnd {
    agent_id: AgentId,
    reason: CompactionReason,
    tokens_before: u64,
    tokens_after: u64,
    summary: Option<String>,
    error: Option<String>,
},
```

`CompactionReason` is `Manual | Threshold | Overflow` (serde
`snake_case`). These are emitted by the host (it owns orchestration) via
`Agent`-exposed bus access; since the binary already holds the agent, it
emits them by calling a small `Agent::emit_event`-style passthrough, or
by routing through the existing pump as synthetic events. Concretely we
reuse the pump path (`world.pump.handle(tui, &event)`) for the live
indicator and add `CompactionStart`/`CompactionEnd` arms to the pump and
to `--format json` output.

NOTE: keeping these events serializable preserves the `--format json`
contract (the locked
`agent_event_serializes_with_internally_tagged_snake_case_shape` test in
`aj-agent::events` gets new cases).

## 7. Host orchestration (`aj`)

A new `aj::compaction` module ties the pieces together. It is the only
place that calls both the planning library and the model.

### 7.1 The end-to-end flow

`run_compaction` is the shared core for all three triggers:

```rust
/// Plan, summarize, persist, and reseed. Returns the outcome for the
/// caller to render. Locks `log` and `agent` as needed; assumes no
/// turn is in flight.
pub async fn run_compaction(
    agent: &Arc<TokioMutex<Agent>>,
    log: &Arc<TokioMutex<ConversationLog>>,
    reason: CompactionReason,
    custom_instructions: Option<&str>,
    keep_recent_tokens: u64, // fixed recent-tail budget to keep verbatim
    cancel: CancellationToken,
) -> CompactionOutcome;
```

Steps:

1. **Linearize** the user thread: lock `log`, `latest_leaf(USER)` →
   `linearize(head, USER)` → `Conversation`. Compute occupancy
   `tokens_before`.
2. **Plan**: `prepare_compaction(&conversation, keep_recent_tokens)`.
   `None` → outcome `NothingToDo` (session too small, or already
   compacted). Release the log lock before the network call.
3. **Summarize**: build the prompt (`update_summary_prompt` when
   `plan.previous_summary` is set, else `initial_summary_prompt`;
   `serialize_conversation(&plan.messages_to_summarize)` for the body;
   `custom_instructions` threaded in). When `plan.turn_prefix_messages`
   is non-empty, generate the turn-prefix summary in a second
   `complete_oneshot` call and append it. Append the file-op lists.
   `max_tokens` for the summary = `min(model.max_tokens, SUMMARY_OUTPUT_CAP)`.
4. **Persist**: lock `log`, `append_compaction(USER, summary,
   plan.first_kept_entry_id, plan.tokens_before, Some(plan.file_ops))`.
5. **Reseed**: re-linearize from the new head → `agent_messages()`
   (now compaction-aware) → lock `agent`, `reseed_transcript(...)`.
6. Compute `tokens_after` (occupancy of the reseeded projection) and
   return `Compacted { tokens_before, tokens_after, summary }`.

Cancellation (`cancel`) is selected against the `complete_oneshot`
calls; an abort before step 4 leaves the log untouched (no partial
compaction is ever persisted). A summarizer error returns
`Failed { error }` and likewise writes nothing.

### 7.2 Manual `/compact`

Command surface (`aj/src/config/commands.rs`):

- Add `CommandAction::Compact` (`commands.rs:202`).
- Add a `COMMANDS` entry (`commands.rs:56`), category `"session"`,
  name `"compact"`, description "Summarize earlier context to free up
  the window."

`/compact` accepts optional free-form focus instructions
(`/compact focus on the auth refactor`). Commands today are
zero-argument (`commands.rs:52`); compaction is the first that takes a
tail argument, so the interactive command parser passes the remainder of
the input line to the handler as `custom_instructions`. (This is a small
extension to the command dispatch, not a new parser — the palette still
lists `compact` with no args; typed `/compact <text>` carries the tail.)

Handling in `handle_command` (`interactive.rs:3170`): like the
session-changing commands, `/compact` is **refused mid-turn** via the
`turn_running` guard with `session_busy_notice("compact")`
(`interactive.rs:224`). Otherwise it triggers a compaction run (§7.5).

### 7.3 Automatic (threshold) trigger

After each top-level turn completes, the binary already learns occupancy
from the `TurnUsage` event folded into the footer
(`footer_data.rs:102`). In the interactive turn-completion arm
(`interactive.rs:1102`), after the turn is removed from `turns` and the
agent is marked idle:

- If `config.auto_compact` is enabled, no compaction is already in
  flight, the just-finished turn was not aborted/errored, and
  `should_compact(occupancy, window, threshold)` holds, spawn a
  compaction run with `reason = Threshold` *before* draining the wake
  triggers / accepting the next prompt.

The occupancy numerator needs a read accessor: add
`AgentFooters::last_turn_context_tokens(id) -> Option<u64>` (or expose it
via `EventPump`), since today only the composite `context_usage` view is
public (`footer_data.rs:119`). The denominator is
`agent.model_info().context_window`, already seeded into the footer at
`session.rs:473`.

Auto-compaction never fires *inside* a turn (aj's `execute_turn` loops
over tool batches without returning to the binary). A single very long
tool-using turn that overruns the window is handled by the reactive path
(§7.4), not the threshold path.

### 7.4 Reactive (overflow) trigger

When a turn returns `TurnError::Recoverable` and the last assistant
message is a context overflow, compact and retry once.

Mechanics, leaning on existing behavior:

- The agent emits `MessageEnd` for the overflow error assistant message
  (`lib.rs:1063`) — so it is persisted to the log — but does **not**
  push it onto the in-memory transcript (`lib.rs:1085-1163` only pushes
  on abort). Detection therefore reads the log: after a `Recoverable`
  turn, lock `log`, linearize, and call
  `aj_models::errors::last_turn_is_context_overflow(&conversation.messages(), Some(window))`.
- Gate on a once-per-turn flag (`overflow_recovery_attempted`) so a
  second overflow surfaces the error instead of looping.
- Run `run_compaction(reason = Overflow)`. The reseeded transcript's
  tail ends with the persisted error assistant message; trim a trailing
  `Error`/`Aborted` assistant message during this reseed so the
  transcript ends in a user/tool-result message. This is consistent with
  the wire, because `transform_messages` rule 5
  (`aj-models/src/transform.rs:328`) already drops `Error`/`Aborted`
  assistant messages and their tool results before inference — the model
  never sees the failed turn regardless.
- Re-drive inference with `Agent::continue_run` (`lib.rs:689`), which
  requires the transcript to end in user/tool-result — satisfied by the
  trim above.

If the retry overflows again, surface the original recoverable error
("context overflow recovery failed; reduce context or switch to a
larger-context model"). Recovery is capped at one attempt.

### 7.5 Interactive integration

The summarizer is a multi-second network call, so compaction runs as a
spawned task (like a turn), never inline in `handle_command` (which is
awaited on the UI event loop). Add `spawn_compaction`, mirroring
`spawn_prompt_turn` (`interactive.rs:2159`):

- It is tracked alongside `turns` so the loop treats a compaction as an
  in-flight operation: new prompts/turns and overlapping compactions are
  refused while it runs (reusing the `turn_running` notion —
  `!turns.is_empty()` plus a compaction-in-flight flag).
- The task: emit `CompactionStart`, call `run_compaction(...)` (which
  locks `log`/`agent` internally and does the reseed), emit
  `CompactionEnd`, and return the outcome.
- A cancel token (Esc) is wired the same way as a turn's, so a long
  summary can be aborted; an aborted compaction writes nothing.
- On completion the loop renders a notice via the existing
  `notice_event` path (`interactive.rs:2498`): "Compacted: summarized N
  messages, context ~X% → ~Y%." A failure renders a warning. The
  `CompactionStart`/`End` events also drive a live "compacting…"
  indicator in the pump and a compaction-summary row.

Why not reuse `seed_session`: its contract is once-only on a fresh agent
(`lib.rs:438`); `reseed_transcript` (§6.2) is the live-agent path.

### 7.6 Print mode

Print mode (`aj/src/modes/print.rs`) is one-shot, so the threshold path
(between top-level turns) does not apply. It supports the **reactive**
path only: after `agent.prompt_with_content` returns `Recoverable`
(`print.rs:485`), if `config.auto_compact` and the last turn overflowed,
run `run_compaction(Overflow)` and retry once with `continue_run`,
then print the final assistant text. In `--format json` a
`CompactionStart`/`End` pair is emitted to the JSONL stream around the
recovery so consumers see it.

### 7.7 `aj compact` CLI subcommand

Not implemented. A headless one-shot `aj compact [session_id]` would
build an agent for a resolved session and run a single
`run_compaction(Manual)` without sending a turn, but it duplicates print
mode's agent-construction path for marginal value. Compaction is reached
through the interactive `/compact` command, the automatic threshold
trigger, and the reactive overflow path (including under `--print`),
which together cover the intended use. Revisit if a scripted "compact
this session" entry point is needed.

## 8. Replay

`replay` (`aj-session/src/replay.rs`) maps each persisted entry onto
renderer events so a resumed session looks like a live one. The
`Compaction` arm of `ReplayState::project_entry` emits:

- `CompactionEnd { reason: Manual, tokens_before, tokens_after, summary:
  None, error: None }`, where `tokens_after` is
  `estimate_conversation_context` of the user thread linearized up to the
  compaction (the reduced projection's occupancy). This mirrors the live
  path: a compaction emits no `TurnUsage` and the retained tail's usage
  is stale, so without it a resumed footer would keep showing the
  pre-compaction occupancy. `summary` is omitted to keep replay events
  lean — the durable record is the log's compaction entry.

Crucially, replay's other arms are unaffected: the summarized prefix
entries are still in the log and `replay` still walks them in order, so
the *scrollback* shows the full historical conversation, while the
*model context* (rebuilt via `agent_messages`) is the reduced
projection. Replay is about what the user sees; projection is about what
the model sees. They intentionally diverge after a compaction. The
replay `Compaction` row marks the boundary in the scrollback.

The replay usage accumulator (`ReplayState::usage_accumulators`,
`replay.rs:118`) is unchanged: a `Compaction` entry carries no `usage`.

## 9. Configuration

Three persisted options (durable, like every other setting — no
session-only toggle), added to `Config` (`aj-conf/src/lib.rs`),
`Default`, and `Config::OPTIONS`:

```rust
/// Whether the agent automatically compacts context when occupancy
/// crosses `compact_threshold`. Defaults to `true`. Also gates the
/// reactive (overflow) recovery path.
pub auto_compact: bool,            // default true

/// Fraction of the model's context window at which auto-compaction
/// fires (Claude-style "context left until auto-compact"). Defaults
/// to 0.85. Clamped to (0, 1].
pub compact_threshold: f64,        // default 0.85

/// Approximate tokens of recent conversation kept verbatim after a
/// compaction; everything older is summarized. A fixed budget, not a
/// fraction of the window. Defaults to 20_000.
pub compact_keep_recent: u64,      // default 20_000
```

`auto_compact` uses `ValueKind::Bool` and `bool_item`. `compact_threshold`
needs a numeric kind, which the schema lacks today
(`ValueKind` is `String | Bool | Enum | StringList`, `lib.rs:565`).
Add:

```rust
ValueKind::Number,   // Display: "number"
```

with a `display`/`apply_toml`/`to_toml` path: `apply_toml` parses a TOML
float (accepting integers too), validates the `(0, 1]` range for
`compact_threshold` and returns a `toml::de::Error` otherwise;
`to_toml` emits the float only when it differs from the default
(a `number_item(value, default)` helper mirroring `bool_item`,
`lib.rs:696`). The drift test
`test_options_table_matches_config_fields` covers the new fields.

NOTE: `ValueKind::Number` is added properly (parsed, range-validated,
round-tripped) rather than smuggling a number through a string — see the
project guidance on not downgrading designs.

## 10. Testing

Pure planning (`aj-session::compaction`), the bulk of coverage:

- Token estimation: usage-preferred vs heuristic fallback; image charge.
- `should_compact`: boundary at exactly the threshold; zero window.
- `find_cut_point`: never cuts on a `tool_result`; keeps ~the requested
  tail; snaps to a turn start; split-turn detection; honors a prior
  compaction's `boundary_start`.
- `serialize_conversation`: tool calls/results inlined; images noted.
- `extract_file_ops`: read/edit/write tools picked up; previous details
  carried forward.
- `prepare_compaction`: `None` when too small / already compacted;
  correct `messages_to_summarize` and `first_kept_entry_id`.

Log + projection (`aj-session::log`):

- `append_compaction` flushes immediately (punctuation) and round-trips
  through `resume`.
- `agent_messages` / `messages` drop the pre-`first_kept` prefix, inject
  the wrapped summary, and keep the tail; "last compaction wins" across
  two compaction entries; missing `first_kept_entry_id` falls back
  safely.

Replay: a `Compaction` entry yields the summary row; the prefix entries
still replay into scrollback.

Agent (`aj-agent`): `complete_oneshot` emits no `Message*`/`TurnUsage`
and leaves the transcript untouched (scripted provider);
`reseed_transcript` replaces the transcript. Event serialization cases
for `CompactionStart`/`End`.

Host (`aj`): a scripted end-to-end `run_compaction` over a seeded log
(summary from the scripted provider) producing a `Compaction` entry and
a reduced reseed; overflow detection + trailing-error trim +
`continue_run` recovery.

Config: parse/round-trip `compact_threshold`; range rejection; drift
test.

## 11. Phasing

1. **Data model + projection.** `ConversationEntryKind::Compaction`,
   `append_compaction`, `is_punctuation`, compaction-aware
   `agent_messages`/`messages`, replay arm. Pure, well-tested; no
   behavior change until something writes a `Compaction` entry.
2. **Planning library.** `aj-session::compaction` end to end (estimation,
   cut-point, serialize, prompts, file-ops, `prepare_compaction`).
3. **Agent mechanisms.** `complete_oneshot`, `reseed_transcript`,
   `CompactionStart`/`End` events.
4. **Host orchestration + manual path.** `aj::compaction::run_compaction`,
   `/compact` command (+ tail-argument dispatch), interactive
   `spawn_compaction` + notice + footer accessor. End-to-end manual
   compaction.
5. **Auto + reactive triggers.** Threshold check in the turn-completion
   arm; overflow detection + retry-once in interactive and print mode;
   config (`auto_compact`, `compact_threshold`, `ValueKind::Number`).
6. **Polish.** Live "compacting…" indicator + compaction-summary row
   rendering; footer wording near the threshold ("context until
   auto-compact").

## 12. Open questions / flagged tradeoffs

- **In-loop proactive compaction.** This spec compacts *between*
  top-level turns plus reactive overflow recovery — matching the
  reference design and respecting aj's agent/log/bus split. It cannot
  proactively compact mid-turn during a long tool-call chain; that case
  costs one wasted overflow round-trip before recovery. Moving a
  compaction checkpoint inside `execute_turn` would avoid the wasted
  round-trip but pulls summarization/persistence concerns into
  `aj-agent` and needs a new event + anchor mechanism. Deferred; revisit
  if long single-turn overflows prove common.
- **Keep-recent budget.** The recent tail kept verbatim is a fixed token
  budget (`compact_keep_recent`, default 20_000) rather than a fraction
  of the window, so the summarized range depends only on retention, not
  on the model's window size. It is a user-tunable `ValueKind::Number`
  option alongside `compact_threshold`.
- **Summarizer model.** The summary is generated with the session's
  active model. A cheaper/faster dedicated summarizer model could be a
  later option; for now "one model per session" keeps auth and
  configuration simple.
