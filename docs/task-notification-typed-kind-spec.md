# Task notifications as a first-class transcript kind

## Problem

When a background task (detached bash command or background sub-agent)
finishes, `Agent::drain_task_notices` injects a completion notice into the
conversation as a normal `Message::User` whose text is wrapped in
`<task-notification>...</task-notification>` tags. The tag does double duty:

1. Model-facing: the literal tag text reaches the LLM verbatim and is the only
   cue telling it "this turn was harness-injected, not a user reply."
2. Local marker: frontends sniff the string to special-case rendering and
   routing.

The second job is a wart. It is stringly-typed and sniffed independently in
several places, so any site that forgets to check treats a notice as a real
user prompt. Concrete symptoms:

- Transcript-focus Tab/Shift+Tab navigation treats notices as focus stops, and
  they become copyable/branchable like real prompts.
- HTML export renders notices as ordinary user bubbles and counts them in user
  stats.
- Print mode and compaction turn-boundary detection also fail to distinguish
  them.

## Approach

Represent a task notification as a typed transcript entry rather than a tagged
user message. The tag becomes a projection-only detail that only the model ever
sees. Every local consumer branches on the variant, so navigation, export,
prompt history, and rendering get correct behavior, most of it for free.

This uses the forward-compat seam that `AgentMessageKind` already documents: an
agent-only transcript entry becomes a new enum variant, and the single
projection choke point decides how it reaches the provider.

Backward compatibility is explicitly out of scope. Notice lines written before
this change are `role:"user"` with `<task-notification>` body text. They
deserialize as plain user messages and render as such on resume. We accept that
rather than add a migration pass.

## 1. Data model (`aj-agent`)

Add a second variant to `AgentMessageKind` in `aj-agent/src/message.rs`:

```rust
#[serde(untagged)]
pub enum AgentMessageKind {
    Wire(Message),
    TaskNotification(TaskNotification),
}
```

`TaskNotification` carries the structure we keep for rich rendering:

```rust
pub struct TaskNotification {
    // On-disk discriminator so untagged deserialization picks this over a
    // wire `Message`. A unit enum that (de)serializes to exactly
    // "task_notification".
    #[serde(rename = "role")]
    tag: NotificationTag,
    /// Command line (bash) or task description (agent).
    pub label: String,
    /// What kind of work ran, for icon/label selection.
    pub kind: TaskNotificationKind, // Bash | Agent
    /// Terminal outcome.
    pub outcome: TaskOutcome,       // Succeeded | Failed { code: Option<i32> } | Killed
    /// Pre-rendered notice body: exit status + output tail (bash) or the
    /// report (agent). This is the only text projected to the model.
    pub body: String,
}
```

`TaskOutcome` and `TaskNotificationKind` are small, self-contained serializable
enums. We do not derive serde onto the runtime `TaskStatus` / `TaskKind` in
`tool.rs`, which carry process handles and agent ids that do not belong on disk.
Map the terminal `TaskStatus` to `TaskOutcome` at construction:

- `Exited(Some(0))` -> `Succeeded`
- `Exited(Some(n))` -> `Failed { code: Some(n) }`
- `Exited(None)` -> `Failed { code: None }` (signal-killed process)
- `Killed` -> `Killed`

Agent-backed tasks complete or fail: map their completed/failed status onto
`Succeeded` / `Failed { code: None }` the same way.

### Serde disambiguation (the one subtle part)

`AgentMessageKind` is `#[serde(untagged)]`, so serde tries variants top to
bottom. `Wire` must stay first. `Message` is `#[serde(tag = "role")]`, so a
line with `role:"task_notification"` fails to deserialize as `Wire` (unknown
role) and falls through to `TaskNotification`. The required `tag` field (a unit
enum accepting only `"task_notification"`) makes `TaskNotification` reject a
`role:"user"` line, so both directions disambiguate on `role`.

Required tests:
- Round-trip a `TaskNotification` entry through JSON.
- A `role:"user"` line never parses as `TaskNotification`.
- A `role:"task_notification"` line never parses as `Wire`.

## 2. Single projection (`aj-agent`)

Rename the two projection methods on `AgentMessage` so the stored-vs-projected
distinction is explicit:

- `as_wire()` -> `as_stored_wire(&self) -> Option<&Message>`: the wire message
  this entry *literally stores*; `None` for agent-only kinds (task
  notifications) that have no stored wire form. Every current `as_wire` caller
  keeps this one; `None`-for-notifications is the answer they want (a notice is
  not a tool result to repair, not a user prompt to recall, etc.).
- new `to_projected_wire(&self) -> Option<Message>`: the wire message the
  *provider receives*, synthesizing the user message with task-notification
  framing for a notification; `None` only for future kinds that never project.

Keep the model-facing text byte-identical to today so model behavior does not
change:

```rust
Message::User(UserMessage::text(format!("{OPEN}\n{body}\n{CLOSE}")))
```

Only two choke points switch to `to_projected_wire`, plus compaction (see §4):

- `aj-agent/src/projection.rs::transcript_to_messages` (agent to provider).
  Required: without this the wake breaks, the model would wake with no idea why.
- `aj-session/src/log.rs::messages()` (occupancy estimation and resume-facing
  wire view).

`agent_messages()` / `projected_agent_messages()` keep returning
`AgentMessage`s, so notifications survive resume seeding as the typed kind and
project through `to_projected_wire` on the next turn.

## 3. Creation (`aj-agent`)

`Agent::drain_task_notices` (`lib.rs`) stops formatting a tagged string. It
builds `AgentMessage` with the `TaskNotification` variant from the queued
`TaskNotice` (`label`, `kind`, `outcome` derived from `status`, `body`).
Everything else (sub-agent usage folding, `MessageStart` / `MessageEnd`
emission) is unchanged, so persistence and the event bus keep working.

## 4. Compaction, decision B2 (`aj-session/src/compaction.rs`)

A task notification is a turn boundary: the conversation shape is `notice`
followed by the assistant wake response, a coherent unit we do not want to split
at a cut.

Add helpers:

```rust
fn is_task_notification(entry: &ConversationEntry) -> bool { /* via AgentMessageKind */ }
fn is_turn_start(entry: &ConversationEntry) -> bool {
    is_user_message(entry) || is_task_notification(entry)
}
```

Use `is_turn_start` wherever `find_cut_point` / `find_turn_start` currently use
`is_user_message` (the `valid` cut-point set, the `turn_start_index` check, the
backward walk). `is_assistant_message` is untouched. Effect: a cut can land on a
notice and keep the notice-plus-wake-response intact, and a kept wake response
snaps its turn start back to its notice rather than to the prior real prompt.

Token accounting and summary input must include notices:

- The backward token walk in `find_cut_point` must count a notice's projected
  tokens. Route it through an owned projection (`to_projected_wire`) instead of
  the borrowing accessor, so a notice contributes to the keep-recent budget.
- `messages_to_summarize` and `turn_prefix_messages` currently extract via
  `expand_message(message.clone()).as_wire().cloned()`. Switch to
  `to_projected_wire` so a notice's text feeds the summary.

The occupancy path (`estimate_conversation_context` -> `messages()`) already
counts notices once `messages()` uses `to_projected_wire`.

## 5. Session layer, mostly free (`aj-session`)

- `prompt_history.rs`: delete the tag sniff and the `TASK_NOTIFICATION_OPEN_TAG`
  import. `PromptHead::is_user_prompt` keys on `role == "user"`, so a
  `role:"task_notification"` line is excluded automatically. Keep the existing
  test intent: notices do not appear in Up/Down recall.
- `replay.rs`, `repair.rs`, `stats.rs`, `tree.rs`, `listener.rs`: no logic
  change. They go through `as_stored_wire`, which yields `None` for
  notifications, the correct "not a stored wire message" answer. Add a replay
  test that a notification entry round-trips into the transcript as the typed
  kind, not as a user entry.

## 6. View model and frontends

### View model (`aj-app/src/chat/model.rs`)

Add `EntryKind::TaskNotification(TaskNotificationEntry)` with `message_id`,
`label`, `kind`, `outcome`, `body`. The existing `EntryKind::Notice` is an
unrelated UI-notice concept (level + text); leave it alone. Remove
`UserEntry.collapsible`; `UserEntry` returns to `message_id` + `content`.

### Reducer (`aj-app/src/chat/reducer.rs`)

Add a `MessageEnd` dispatch arm for `AgentMessageKind::TaskNotification` that
appends the new entry (and include it in the `message_id` match so it gets a
stable entry id). Delete the `collapsible` logic and the
`TASK_NOTIFICATION_OPEN_TAG` import from `reduce_user_end`. Rewrite the
`user_message_end_skips_empty_and_marks_task_notifications_collapsible` test to
assert the notification arm produces a `TaskNotification` entry.

### aj-next transcript (`aj-next/src/transcript.rs`)

Add a `build_task_notification` renderer keyed on the new entry kind. Move the
fold logic out of `build_user_bubble` (rename `USER_COLLAPSED_LINES`
accordingly). Keep `sanitize_terminal_output` on the notice body since it embeds
captured task output.

Bubble background reflects outcome, matching how tool cells reflect status:

- `Succeeded` -> `styles.tool_success_bg`
- `Failed` -> `styles.tool_error_bg`
- `Killed` -> `styles.tool_error_bg`

(Two-tone: success vs did-not-succeed. Easily split later if we want a neutral
kill tint.) Style the text and fold-hint spans consistently with tool cells
rather than the user tint. Update the `bubble.rs` module doc that says "Tool
cells and user-message bubbles share the exact same surface" to include task
notifications.

`user_message_indices` and the focus-border predicate match only
`EntryKind::User(_)`, so notifications stop being Tab stops and stop being
copyable/branchable with no change there. This fixes the navigation symptom. Add
a Tab-nav test asserting a notification is skipped.

### aj old frontend (`aj/src/modes/interactive/...`)

`event_pump.rs`: replace the tag sniff with a match on
`EntryKind::TaskNotification`, building the collapsible component. Apply the
analogous outcome-tint styling if the component distinguishes a background;
otherwise leave its surface as is. `editor_ext.rs`: update the
`bootstrap_ignores_task_notification_messages` test to the new on-disk shape.

## 7. HTML export (`aj-app/src/export.rs` + `assets/export/template.js`)

`ExportEntry::serialize` falls through to raw `entry.serialize` for non
tool-result messages, so a notification serializes with `role:"task_notification"`
and its structured fields with no Rust change. Confirm `derive_title` selects
the first `Message::User` via the stored-wire accessor, so it skips
notifications; add an assertion.

In `template.js`: add a `renderTaskNotification` branch that renders a distinct
block from `label` / `outcome` / `body` (styled like tool/agent output, not a
User bubble), and exclude `role === 'task_notification'` from the user stats
counter. This fixes the export symptom and lets export show, e.g., "task <label>
succeeded" rather than raw prose.

## 8. Cleanup

Demote `TASK_NOTIFICATION_OPEN_TAG` / `TASK_NOTIFICATION_CLOSE_TAG` in
`aj-agent/src/tool.rs` to `pub(crate)`. After the refactor only
`to_projected_wire` references them. Update their doc from "frontends key
rendering off the open tag" to "framing used when projecting a notification onto
the wire."

## 9. Tests to add or update

- `aj-agent`: serde round-trip + disambiguation (§1); projection of a notice to
  a framed user message (§2); `drain_task_notices` produces the typed variant;
  update existing lib tests that assert the tagged-string shape.
- `aj-session`: prompt-history exclusion; replay round-trip as typed kind;
  compaction B2 (notice is a valid cut point and a turn start, its tokens count,
  its text feeds the summary).
- `aj-app`: reducer arm produces a `TaskNotification` entry; export serializes
  `role:"task_notification"` and `derive_title` skips it.
- `aj-next`: Tab-nav skips a notification; the notification bubble uses the
  outcome tint.
- `aj`: `editor_ext` bootstrap ignores notifications under the new shape.

## Method rename summary

`AgentMessage::as_wire` becomes `as_stored_wire` (borrow, literal stored wire
message). A new `to_projected_wire` (owned, provider-facing projection) is the
single place a notification becomes a wire message. This rename touches every
`as_wire` call site (mechanical).
