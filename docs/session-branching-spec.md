# Spec: session branching

## Status: proposal (not started)

Branch a conversation at any earlier user message, resume onto the branch you
last worked on, and navigate between branches in a session tree view. All of it
for `aj-next`.

The on-disk substrate already supports this. Every `ConversationEntry` carries
an `id` and a `parent_id`, so a session file is a tree, and
`ConversationLog::linearize(head, filter)` reconstructs any one path
(`src/aj-session/src/log.rs`). Nothing creates sibling branches deliberately
today. They only arise from two concurrent writers (documented on
`ConversationLog`), and the append-order `replay` would interleave them. This
spec makes branches first-class: an explicit head, ids that reach the UI,
path-aware replay, and the UX on top.

## Goal and scope

**In scope:**

- Message ids: every `AgentMessage` gets a unique id at creation, and the log
  adopts it as the entry id for `Message` entries.
- An explicit user-thread head on `ConversationLog` that all appends anchor at,
  replacing the implicit "most recently appended" (`latest_leaf`) convention.
- Path-aware replay: resume renders only the active path, not every entry in
  append order.
- The `b` shortcut in transcript focus mode: prefill the editor with the
  focused user message, and on submit start a new branch at that point.
- A session tree overlay, reached from the command palette, that renders the
  branch structure and switches the active branch.

**Out of scope (see Non-goals):**

- Branching at assistant or tool messages.
- LLM summaries of the abandoned branch injected into the new one.
- Forking a branch into a separate session file.
- Garbage collection of abandoned branches.

## Design overview

Four ideas, each small:

1. **Messages own their ids.** A 128-bit random token minted in the
   `AgentMessage` constructor, before the message is seen by any consumer. The
   id flows through the `AgentEvent` bus into `ChatState` and through the
   persistence listener onto disk, where it *is* the entry id. One id end to
   end, no correlation layer between the UI and the log.
2. **The log owns an explicit head.** `ConversationLog` tracks the user-thread
   head that the next append anchors at. Appends advance it, branching and
   branch switching set it. Persistence of the head is implicit: the next
   appended message records it via `parent_id`, and resume recovers it via
   `latest_leaf` because the most recently appended entry is always on the
   branch that was last written to.
3. **Replay follows the path.** Resume and rebuild project only the linearized
   chain from the head, plus the sub-agent threads anchored on it.
4. **Branch creation is just a head move plus a normal turn.** `b` prefills the
   editor and arms a branch anchor. Submit moves the head to the anchor's
   parent, rebuilds session state through the existing session-switch
   machinery, and runs the prompt as an ordinary turn. The tree view is the
   same head move without the prompt.

## Part 1: message ids

### `AgentMessage.id`

`AgentMessage` (`src/aj-agent/src/message.rs`) gains an `id: String`, a 32-hex
random token (128 bits, `format!("{:032x}", rand::random::<u128>())`). Minted
inside the `AgentMessage` constructor (`AgentMessage::wire` and siblings), not
at call sites. That single choke point matters: messages are constructed in
more places than the obvious ones (user prompts, steering and follow-ups,
assistant finalization including the aborted/error paths, tool results, task
notices, sub-agent task prompts, repair's synthesized tool results, the
synthetic compaction summary), and a per-site enumeration would miss some.

NOTE: the `MessageStart` placeholder for a streaming assistant turn is a
separate `AgentMessage` construction from the final one at stream end, so
Start and End of one turn carry different ids. Only `MessageEnd` ids are
consumed anywhere, but this is a landmine if ids ever spread to more
`ChatState` entry kinds, so it is recorded here.

Why 128 bits when entry ids are 32 today: `ConversationLog::mint_id` can check
candidates against the in-memory index, so 32 bits suffice there. Message ids
are minted in `aj-agent` with no access to the log, so entropy replaces the
collision check. At 128 bits the birthday bound is negligible even across
concurrent writers. `mint_id` (used for non-message entries) widens to the same
32-hex format so all new ids are uniform. `EntryId` stays a `String`, and old
8-hex ids coexist with new 32-hex ids in the same file because parent links
copy strings verbatim.

### Serialization: the id is in-memory only

`AgentMessage` is `#[serde(transparent)]` over the wire enum, and its bare
wire-JSON shape is a locked on-disk contract. The `id` field is therefore
`#[serde(skip)]`: it is never serialized, anywhere. Consequences, all
deliberate:

- On disk, the entry id is the single source of truth for `Message` entries
  (no redundant second copy inside the message).
- Print mode's `AgentEvent` JSONL does not carry message ids. The id contract
  ends at the process boundary.
- Any `AgentMessage` deserialized outside the log's backfill path (preview
  scans, prompt history) has an empty id. The contract is: ids are guaranteed
  non-empty only for messages minted in-process or loaded through
  `ConversationLog::resume`. Today no other deserializer consumes ids, and
  any future one must go through the log.

### The log adopts message ids as entry ids

For `ConversationEntryKind::Message` entries, the adoption lives in the log's
own append path (`append` / `ConversationView::add_message`): when the payload
carries a message with a non-empty id, that id becomes the entry id, otherwise
one is minted. Putting the adoption in the append path rather than in the
persistence listener keeps every writer consistent. Repair also appends
messages, and a listener-only adoption would leave repair entries with ids
diverging from their in-memory messages. Non-message entries (settings,
compaction, spawn roots, system prompt) keep log-minted ids.

On load, `ConversationLog::resume` backfills `AgentMessage.id` from the entry
id, so replay, reseeding, and the reducer see ids on old files for free. Old
files need no migration: their entry ids simply become the message ids.

`append` keeps a uniqueness check and returns `InvalidAppend` on a duplicate
id. A collision is a 2^-64-ish event, erroring loudly beats silently diverging.

### Ids reach `ChatState`

`UserEntry` in the chat model (`src/aj-app/src/chat/model.rs`) gains
`message_id: String`, copied from the `AgentMessage` in the `MessageEnd` event
by the reducer. Within the TUI it is always non-empty: live messages mint ids,
replayed messages are backfilled. Other entry kinds don't need ids yet, we add
them when a feature wants them.

This is the piece that makes `b` a direct lookup. There is no ordinal
correlation and no temporal coupling on the persistence listener: the id exists
before the event is emitted, so it does not matter which subscriber runs first.

## Part 2: the explicit head

### `ConversationLog::head`

`ConversationLog` gains a user-thread head:

- `head: Option<EntryId>`. `None` only for a fresh log with no user-thread
  entries: the next append then anchors at the system-prompt meta entry when
  one exists (the existing `parent_for_next_append` fallback), or becomes the
  file root.
- Initialized to `None` on `create` and to `latest_leaf(ThreadFilter::USER)` on
  `resume` (or to an explicit override, see Part 4).
- Advanced by every user-thread append: messages via the persistence listener,
  settings entries, compaction entries, repair.
- `set_head(EntryId)` requires the entry to exist and to be either a
  user-thread entry (`ThreadKind::User`) or the system-prompt meta entry, and
  errors otherwise. Existence alone is not enough: a sub-agent entry as head
  would chain the main conversation onto a sub thread, which `append`'s own
  checks would not catch (they validate the new entry's thread, not the
  parent's).

All current `latest_leaf(USER)`-as-head call sites switch to `head`. The full
list, since missing one silently writes to the wrong branch:

- `persistence_listener` (`src/aj-session/src/listener.rs`), which reads it
  twice: the `MessageEnd` persist and the `SubAgentStart` parent anchor.
- `append_settings_entry` and `append_compaction` (`log.rs`).
- `run_compaction`'s planning read (`src/aj-app/src/compaction.rs`), which
  linearizes the conversation to plan `first_kept_entry_id`. Left on
  `latest_leaf`, a `/compact` after a branch switch would summarize the
  abandoned branch and attach the result to the active one, replacing the
  active history with the wrong summary. Its second read, the post-append
  reseed, moves too: it happens to be equivalent today (the just-appended
  compaction entry is both the newest leaf and the head, under one lock),
  but leaving it on `latest_leaf` would make the equivalence load-bearing.
- `repair_interrupted_tool_uses` (`repair.rs`).
- `prepare_log` (`src/aj-app/src/session_setup.rs`).
- The export path's `leaf_id` (`src/aj-app/src/export.rs`).
- `SessionStats::settings` (`stats.rs`), which is already path-scoped and
  would otherwise report the wrong branch's model/thinking in session info.

`ConversationView::user` takes the log's head instead of a caller-computed one
and advances it on append. This is a simplification on its own: one source of
truth instead of an append-recency scan recomputed at eight sites.

`latest_leaf` remains as the resume-time initializer, for head-agnostic
emptiness checks (resume-hint eligibility in `print.rs` and the `aj-next`
splash), and for sub-agent threads. Sub-agent threads keep their
`latest_leaf(subagent(n))` anchoring: they are linear per `agent_id`, branching does not apply to them,
and giving each a head field would be bookkeeping without a consumer.

### Head persistence, and the one thing we accept losing

There is no persisted head pointer. The head is recorded implicitly by the
next append's `parent_id`, and resume recovers it through `latest_leaf`. This
is correct for every flow that flushes a punctuation entry: branch-and-submit,
or a branch switch followed by any message.

The accepted gap: switch branches in the tree view, then quit having written
nothing (or only buffered non-punctuation entries, e.g. a settings change,
which are already discarded on quit today). On resume the head falls back to
the most recently written branch. We accept this over a persisted pointer (a
sidecar field breaks the single append-only file model) and over writing a
head-move marker entry on every switch. If the gap ever bothers us, a marker
entry is a pure extension: nothing in this design would change.

## Part 3: path-aware replay

`replay` (`src/aj-session/src/replay.rs`) walks the file in append order and
projects every entry. With sibling branches on disk that interleaves all of
them into the scrollback. This is already wrong for the concurrent-writer case
and becomes unacceptable with deliberate branches.

Fix: compute the active path set by walking parent pointers from the head, and
include a sub-agent thread when its *first entry's* parent is in the path set.
Keying on the first entry rather than on a `SubAgentSpawn` root matters:
legacy logs have sub threads without spawn roots, leading directly with the
task user message, and both shapes anchor their first entry at the spawning
assistant message on the user thread. No transitive closure is needed: spawned
agents have the `agent` tool removed from their toolset, so sub threads always
anchor on the user thread.

Replay then iterates append order as today but skips entries outside the
included set. Filtering append order (rather than emitting the linearized path
directly) preserves the original interleaving of main-thread and sub-agent
events, which the reducer's ordering assumptions rely on.

`replay_deferring_subs` and `project_thread` get the same treatment. Session
previews (`persistence.rs`) keep their whole-file view, which is fine:
previews describe the session, not a path. `Conversation` and the agent seed
are already path-scoped via `linearize`.

## Part 4: rebuilding onto a different head

Branch creation and branch switching both reduce to "rebuild this session with
an explicit head". We reuse the session-switch machinery, which already
rebuilds core, agent seed, and `ChatState` from a log correctly.

### Mechanics

- `SessionSpec::Resume` gains an optional `head: Option<EntryId>` override,
  where `None` means "default", i.e. `latest_leaf`. `None` never means "branch
  to the file root": branching at a root entry is refused up front (see
  Part 5), so every branch target is a real id.
- `prepare_log` installs the override as the log's head *before* repair runs,
  then linearizes and repairs as today. Ordering matters: repair must anchor
  its synthesized tool results at the branch path's tip, not at the abandoned
  branch's, otherwise a branch created below a dangling tool_call is seeded
  with the dangle intact. The override is apply-or-fail: a stale or invalid
  requested head (truncated file, hand-edited log) fails the whole build
  instead of silently resuming the default head, so a successful build
  guarantees the requested head is installed.
- The `aj-next` session loop gains an exit variant carrying the same session
  id, the target head, and an optional pending prompt
  (`SessionExit::Branch { head, prompt }` or equivalent). The outer loop in
  `run()` (`src/aj-next/src/interactive.rs`) rebuilds via
  `build_next_session` / `install_next_session` exactly like a switch, then
  feeds the pending prompt into the fresh world as if the user had submitted
  it.
- `TranscriptView::reset_to_tail` runs on install, as with a session switch,
  so cached render surfaces keyed by chat `EntryId` don't collide.
- Cosmetics: the rebuild shows a branch-specific notice (not "Switched to
  session {same id}") and does not add a duplicate entry to the
  quit-banner's completed-sessions list. The usage overlay resets like a
  switch does. Not ideal mid-session, but consistent, and fixing usage
  carry-over is orthogonal.

### Buffered writes and flush ordering

`ConversationLog` gains `flush_pending`, which drains `pending_writes` to
disk. It is a no-op when the log has never materialized a file (`file` is
`None`, the abandoned-empty-session property stays intact). Branch operations
require a materialized log and are refused otherwise. This is unreachable
through the UX (branching needs a persisted user message, the tree view needs
persisted entries), so the refusal is a guard, not a flow.

The flush runs in the *outer* loop, after turn and background-task shutdown
and immediately before the rebuild. Not inside `drive()`: background sub-agents
are shut down only after `drive` returns, and one racing shutdown can still
append a non-punctuation entry that a too-early flush would strand in the
dropped log instance. The re-resume's durability comes from
shutdown-before-build plus this flush. Keep that order.

The flushed entries chain from the old head and belong to the abandoned
branch. That is correct: they were made in that branch's context and must not
be lost by the re-resume, but they also must not follow the user to the new
branch.

### Settings and compaction are path-scoped

Settings restore already folds `Conversation::settings()` over the linearized
path, so after branching the session carries the settings as of the branch
point. Branching is "go back in time", and that includes model/thinking
changes made on the abandoned tail. Same for compaction: a compaction entry
applies only when it sits on the active path, so a branch created before the
boundary sees full pre-compaction history through the normal projection.

### Guards and failure handling

- **Mid-turn:** branch submit and tree-view switching are refused with a
  notice while `world.turns` is non-empty (the same global check the
  session-changing commands use, deliberately not per-view). The refusal
  keeps the armed anchor and the editor text. A follow-up queued to a
  turn-owning agent is covered by this check (a non-empty queue at turn end
  immediately spawns a wake turn). A follow-up queued to a *background*
  sub-agent is not, that case is caught by the background-task guard below.
- **Background tasks:** the `turns` check does not cover detached bash tasks
  and background sub-agents, and the rebuild's shutdown would kill them
  silently, which is surprising for a gesture the user reads as "edit and
  resend". Branch submit, tree-view switching, session switching, and
  starting a new session therefore all refuse outright while background work
  runs. The refusal is a toast that names the blocked action and carries a
  short remedy: cancel the running turn with Ctrl+C, or stop background
  tasks from the agent picker.
- **The prompt is never lost and never missubmitted.** The armed submit
  records the prompt to prompt history before breaking out of `drive()` (the
  normal drive-loop submit site is bypassed). A requested head override is
  apply-or-fail: if it cannot be installed (stale id: truncated file,
  hand-edited log), `prepare_log` fails the build, and `build_next_session`'s
  existing fallback machinery resumes the previous session on its default
  head with a failure notice. The same happens when the build fails for any
  other reason. Either way the pending prompt is *not* fed into the fallback
  world. It is restored into the editor with a notice explaining the branch
  failed, and it is never submitted against the wrong head.

## Part 5: the `b` shortcut

Mirrors the `y` copy shortcut layer for layer:

1. `ACTION_BRANCH_MESSAGE = "aj.transcript.branch_message"` with default chord
   `b` and description "Branch from the focused message" in `AJ_KEYBINDINGS`
   (`src/aj-app/src/keybindings.rs`).
2. `AjAction::BranchMessage` (`src/aj-app/src/actions.rs`), capture phase.
3. Keymap gate `in_transcript_focus` (`src/aj-next/src/keymap.rs`), so `b`
   types normally in the editor. Mirror the existing
   `copy_message_matches_on_y_only_in_transcript_focus` test.
4. Dispatch in the shell's `on_action`: read the focused user entry's
   `message_id` and `joined_text()` (a `focused_message_id()` sibling of
   `focused_message_text()` on `TranscriptView`), prefill the editor with
   `TextArea::set_text`, focus the editor, and arm a pending branch anchor
   (the message id) in a shell slot, following the parked-slot pattern the
   other host actions use.

**Main view only.** `b` is inert when `active_view` is a sub-agent view.
Focus mode works in those views and their task prompts land as `UserEntry`
rows, but a sub-agent user message is not a branch point: its parent chain
lives on a sub thread, and anchoring the user-thread head there would splice
the main conversation onto a sub-agent thread (ending in an unanswered
spawning tool_call). `set_head`'s thread validation is the backstop, the view
gate is the UX.

**Root refusal.** If the anchor entry's `parent_id` is `None` (the first
message of an ancient file with no system prompt and no seeded settings), the
branch is refused with a notice. A second root entry is invalid in the log,
and there is nothing meaningful to branch from. On files written by current
code this cannot happen: the first user message's parent is the last seeded
settings entry (system prompt and initial settings chain ahead of it on the
user thread).

The focus-border hint extends to "y to copy · b to branch", both labels
resolved from the keybinding data via `default_action_shortcut`, never
hardcoded.

### Armed-anchor lifecycle

- While armed, the footer shows a small indicator ("branching from message",
  plus the truncated prompt text) so submit-behavior is never a surprise.
- Arming overwrites the editor content, the same contract as prompt-history
  recall. Pressing `b` on another message re-arms to the new anchor and
  replaces the content again.
- Esc in the editor cancels the armed anchor and shows a short notice (the
  editor text stays, it is the user's to keep or clear). When the
  autocomplete popup is open, Esc closes the popup first and the anchor
  second, matching the existing Esc priority chain.
- The anchor is cleared on any session install (`SessionExit::New`, `Switch`,
  and the branch exit itself). The shell and its slots survive session
  rebuilds, so without the explicit clear a stale anchor could resolve
  against a different session's log, and with legacy 8-hex ids even resolve
  to a *wrong* entry rather than failing.
- While armed, steering (Alt+Enter) and queue-dequeue (Alt+Up) are refused
  with a notice pointing at Esc. Steering would silently consume the branch
  prompt as steering text for the branch being abandoned, dequeueing would
  splice queued text into it. Both are incoherent with an armed anchor.
- Submit with an empty (post-trim) editor while armed is refused and keeps
  the anchor. The head must not move for a prompt that would be dropped.
- Submitting with an anchor armed while a turn runs is refused with a notice
  (see Part 4 guards). Arming itself is always allowed.

### Submit

Submit with an armed anchor: resolve the message id in the log, take that
entry's `parent_id` as the new head (on current files that parent is a real
entry, typically the previous assistant message or a settings entry), and
break out with the branch exit carrying head plus the edited prompt. The
rebuilt session's transcript ends just before the branched-from message, and
the submitted turn's user message persists with `parent_id` equal to the new
head, which is what creates the sibling branch on disk.

## Part 6: the session tree view

A new palette command, `CommandAction::OpenSessionTree` ("session tree"). The
view itself opens read-only at any time, selection is subject to the Part 4
guards.

### Tree model

Built on demand from the log, in `aj-session`:

- A children index over user-thread entries (`HashMap<EntryId, Vec<EntryId>>`
  from parent pointers, children in append order). Sub-agent entries are
  excluded so spawn roots don't read as branch points.
- A **virtual root** anchors the index: user-thread entries whose parent is
  the system-prompt meta entry (or `None`) are its children. Forks at the
  very first user message are real (branching there anchors siblings at the
  seeded settings chain, which is itself user-thread and linear, so those
  forks appear on the settings entry, but a hand-edited or ancient file can
  fork at the meta root too), and without the virtual root they would exist
  in no node's child list.
- The displayed structure collapses linear runs: a **segment** is the chain
  from a fork child (or the virtual root) down to the next fork or leaf.
  Nodes in the view are segments, not entries. A **fork** is a node with two
  or more user-thread children. A **leaf** is one with none.

### Rendering

One row per segment, composed like the session selector's rows:

- Ascii connectors `├─` / `└─` / `│` for structure. Indentation increases only
  below forks, so a mostly-linear session stays flat.
- The row label is the segment's first user message, one line, truncated
  (reuse `truncate_chars`, ~60 chars). That message is the divergence point,
  which is exactly what distinguishes branches from each other. A segment
  with no user message (possible: a settings entry appended at a fork after
  a tree-view switch, or repair-synthesized tool results at a branch point)
  falls back to the first message of any kind, then to a dim kind
  placeholder like "(settings)".
- Leaf segments get a dim suffix: the *segment's* message count and the
  relative age of its last entry (reuse `format_age`).
- The **active path** is the chain from the virtual root to the current head.
  Segments on it are marked and sorted first among siblings, so the current
  branch reads as a straight line from the top. Nothing below the head is
  marked.

A session with no forks still opens and shows its single segment row. An
unpersisted session shows an empty list with a placeholder notice.

v1 builds this on `FilterableSelect` inside an `OverlayWindow` (the
session-selector shape, `src/aj-next/src/session_selector.rs`), with the tree
art baked into row labels. If the filter interaction fights the tree layout in
practice we promote it to a small custom widget, same overlay plumbing.

### Selection

Enter on a segment parks a branch request for the drive loop: head = the
segment's last entry, no prompt. Same rebuild path as Part 4.

One vintage-specific edge: on a file with a system prompt but no seeded
settings chain, branching at the first user message sets the head to the
system-prompt *meta* entry (Part 5's root refusal only fires on a `None`
parent). That head is in no segment, so the active path is empty, nothing is
marked, and any selection moves the head. Determinate and acceptable, just
stated here so nobody "fixes" it.

The head is normally a segment's last entry, but not always: the branch flow
moves the head first and runs the prompt second, so a failure between the two
(refused spawn, provider error) legitimately leaves the head mid-segment.
Selection is therefore a no-op close only when the head already *is* the
selected segment's last entry. Selecting the active segment with a mid-segment
head fast-forwards to the segment's end, which is also the recovery gesture
for exactly that failure state. Escape closes without changes.

## Non-goals, for the record

- **Branching at assistant messages.** The anchor resolution generalizes (any
  user-thread entry id works as a head), but the UX is scoped to user
  messages. Extending it later means storing ids on more `ChatState` entry
  kinds.
- **Branch summaries.** Summarizing the abandoned branch into a context
  message on the new one composes with this design as another entry kind on
  the path. Out of scope for v1.
- **Forking to a new session file.** Copying the active path into a fresh
  session is a different feature with different tradeoffs (clean file vs
  shared history). Nothing here blocks it.
- **Branch GC.** Abandoned branches stay in the file and remain reachable
  through the tree view. Append-only is the contract.

## Phasing

Each phase lands independently and keeps all tests green.

1. **Message ids.** `AgentMessage.id` (`serde(skip)`), minting in the
   constructor, entry-id adoption in the log's append path, backfill on
   resume, `mint_id` widened, `UserEntry.message_id` in the reducer.
2. **Explicit head + path-aware replay.** The `head` field, `set_head` with
   thread validation, all eight anchor/consumer sites moved off
   `latest_leaf(USER)` (including `run_compaction` planning and
   `stats.settings`), resume override in `prepare_log` with
   repair-after-override ordering, path-filtered replay. This fixes the
   existing concurrent-writer interleaving bug before any UI exists.
3. **`b` and the branch submit flow.** Action, keymap gate, Main-view gate,
   prefill, armed-anchor lifecycle (footer indicator, Esc, session-install
   clearing, steer/dequeue refusal), root refusal, branch exit + rebuild +
   prompt handoff with the never-lose-never-missubmit contract,
   `flush_pending` and its ordering, background-task confirm.
4. **Session tree view.** Children index, virtual root, and segment model in
   `aj-session`, the overlay, selection-driven branch switch with
   fast-forward semantics.

## Testing

- `aj-session`: appends anchor at `head` and advance it. `set_head` +
  subsequent append creates a sibling on disk. `set_head` rejects sub-agent
  entries and missing ids. Resume with a head override linearizes the right
  path, a stale override fails the build with an error. Repair with a head
  override anchors its synthesized results at the override (pin the
  ordering).
  Path-aware replay excludes sibling branches, includes sub-agent threads
  anchored on the path, and includes legacy sub threads without spawn roots
  (pin the concurrent-writer interleave fix). Message-id backfill on
  old-format files. Duplicate-id append errors. Adoption in the append path
  covers repair's writes, not just the listener's. `flush_pending` drains in
  order, no-ops on an unmaterialized log. Segment/children-index model:
  forks, leaves, linear runs, the virtual root, root forks, message-free
  segments.
- `aj-app`: reducer stores `message_id` from `MessageEnd`. Settings restore
  after a branch reflects the branch point, not the abandoned tail.
  Compaction planned after a branch switch summarizes the active path
  (pin the `run_compaction` head move).
- `aj-next`: keymap test for `b` gated to transcript focus (mirror the `y`
  test). `b` inert in sub-agent views. Focus-hint label renders both
  shortcuts from the binding data. Armed-anchor lifecycle: arm, re-arm, Esc
  cancel (popup-first priority), cleared on session install, mid-turn submit
  refusal keeps anchor and text, empty-submit refusal, steer/dequeue refusal
  while armed. Root-entry branch refusal on an ancient-file fixture. Branch
  failure restores the prompt into the editor and records it in history.
  Tree rows: connector art, active-first ordering, truncation, label
  fallbacks, single-segment session, mid-segment head fast-forward.
- End to end (testkit): branch at an earlier message, submit, verify the new
  path renders alone and the file contains both branches. Switch back via the
  tree view and verify transcript and agent seed match the original branch.
  Branch with a running background task requires the confirm step.
