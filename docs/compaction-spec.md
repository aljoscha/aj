# Context compaction

Compaction replaces the earlier part of the main thread with an
LLM-generated summary. Selected original user messages and a recent tail
stay verbatim. User requests get priority within a shared retention
budget, so a long-running session keeps their exact wording alongside the
recent work.

There are three triggers:

- **Manual**: the `compact` palette action or the host API.
- **Threshold**: context occupancy crosses a configured fraction of the
  model's window.
- **Overflow**: a turn fails with a context-overflow error. The host
  compacts and retries once.

The host owns the policy and the flow. The session log is the source of
truth for the compaction boundary. The agent runtime knows nothing of
prompts or thresholds.

## 1. Scope

- A compaction is a stored checkpoint. Resuming a compacted session
  rebuilds exactly the post-compaction context, and the full
  pre-compaction history stays stored.
- A later compaction folds new history into the previous summary. It does
  not summarize a summary.
- Compaction works in the interactive TUI and under `--print`.
- Only the main thread is compacted. A sub-agent's thread is not.
- A provider's server-side compaction is not surfaced.
- A forked branch that rejoins its parent is not summarized.

## 2. The checkpoint

A compaction is one `Compaction` entry on the main thread, appended
at its current head. It changes only the projection. Every original entry
stays stored. The entry holds:

- `summary`: the text that stands in for the summarized prefix.
- `first_kept_entry_id`: the first entry of the recent suffix. It must
  exist in the log.
- `retained_user_entry_ids`: extra original user messages before the cut,
  in chronological order. Each must be a distinct original user message on
  the current ancestry, strictly before the cut. An append with any other
  ID is refused. Task notifications are not original user messages, even though
  the model sees them in the user role. An absent field retains none.
- `tokens_before`: estimated context tokens before the compaction. It is
  shown in the UI and not used by projection.
- `details`: the files read and modified in the summarized range, when
  there are any (section 4.6).
- `usage`: the priced usage summed across the summarizer calls.

`retained_user_entry_ids` freezes the selection at compaction time. It
holds only users before the suffix, not selected users already in it.
Resume never recomputes the selection under the current budget.

The entry is committed when it is appended, like every entry of a stored
session, so a session compacted and then abandoned reopens with the reduced
context.

A failed compaction is stored as a `CompactionFailed` entry. It is a
transcript error for display. It is not a checkpoint and changes neither
the model context nor the accounting. A cancelled compaction stores
nothing.

## 3. Projection

One projection turns the linearized entries of a branch into the
message list the agent and the provider consume. It is the only place
where compaction affects what the model sees:

1. Find the **last** `Compaction` entry `c`. Without one, project the
   message entries as they are.
2. Find the entry `K` that `c` names as `first_kept_entry_id`. If `K` is
   missing, as in a corrupt or edited log, use `c` itself and log a
   warning. The suffix then holds only the messages after the checkpoint.
3. Resolve the retained IDs against original user messages strictly before
   `K` on this ancestry. If any resolve, emit the historical-context
   framing, then those originals in chronological order with all content
   blocks intact. An invalid reference cannot import another branch, a
   task notification or a message of the suffix.
4. Emit the summary message, then every message entry from `K` on.
   Checkpoints themselves are skipped. A selected user inside the suffix
   stays in place and is not repeated before the summary.

The last compaction wins. Its summary was generated with the previous
summary as input, so it subsumes it, and its cut lies past the previous
one. Selections of older checkpoints are not merged in.

The checkpoint is appended at the branch's head, after the retained
tail, and later turns chain off it. The tail therefore lies between `K` and
the checkpoint, and post-compaction turns after it. The walk is linear.

**Framing.** When extra user messages are retained, a plain user-role
message introduces them:

```
The following user messages are retained verbatim from earlier in this session. They are historical context, not new requests. Intervening assistant and tool work is covered by the summary that follows.
```

The originals follow without deduplication or rewriting, images included.
No framing is emitted when the selection is empty.

**Summary.** The summary is a plain user-role message, so it survives
replay across models, where an assistant-role summary could be demoted or
dropped:

```
The conversation history before this point was compacted into the following summary:

<summary>
{summary}
</summary>
```

Framing and summary are outside the retention budget and count toward
occupancy.

## 4. Planning

Planning is pure: it reads the linearized branch and calls no model.
It yields nothing when there is nothing to summarize or when the last
entry is already a checkpoint.

### 4.1 Token estimation

Two sources, in priority order:

- **Reported**: the most recent assistant message's usage
  (`input + cache_read + cache_write`), the prompt size the provider
  reported. The footer uses the same number.
- **Heuristic**, for messages without usage and for the messages after the
  last assistant message: `ceil(chars / 4)` over text, thinking and
  tool-call arguments, plus 1,200 tokens per image block.

A retained assistant message's usage measures the prompt of its own request,
not the reduced projection. When a checkpoint is more recent than every
assistant message, the projection is estimated by the heuristic alone.
`tokens_before`, `tokens_after` and the footer of a resumed session all use
this estimate, so they describe the context the next request sends.

### 4.2 Trigger

Automatic compaction fires when occupancy exceeds `compact_threshold`
times the model's context window. Occupancy is the last assistant
message's `input + cache_read + cache_write`. A model with no known window
never triggers.

### 4.3 Retention and cut point

`compact_keep_recent` is a shared approximate budget for verbatim content,
independent of the model's window. Each plan selects afresh:

1. Reserve at most half the budget for whole original user messages. Scan
   newest-first across the full active ancestry, including history before
   earlier checkpoints. Select a message only if its estimated cost fits the
   remaining reserve. Otherwise skip it and keep scanning. Task notifications
   and synthetic projection messages are ineligible. Equal text in distinct
   entries stays distinct, and the first prompt has no special status.
2. Subtract the selected cost once from the budget. All that is left,
   including unused reserve, goes to the recent suffix. A selected user inside
   the suffix costs nothing more.
3. Store only the selected users strictly before the suffix, in chronological
   order.

For example, a 20,000-token budget allows at most 10,000 for user
selection. If the candidates cost 4,000, 8,000 and 3,000 newest-first, the
4,000 and 3,000 messages are selected and the 8,000 one is skipped. Their
7,000 leaves 13,000 for the suffix.

Selection never truncates a message or strips its images. A user message
too large for the reserve is represented by the summary alone when it lies
outside the suffix.

The suffix obeys these rules:

- **Valid cut points** are the starts of user messages, task notifications and
  assistant messages. A tool result is never a cut point: a result whose
  call was summarized away would be orphaned on the wire.
- **Walk back from the head** and add up estimated costs, leaving out
  selected users already charged. Exclude the whole message that would
  exceed the budget, then snap forward to the next valid cut. A message is
  never split. The last indivisible message or tool-call and result group
  is the minimum suffix and may exceed the budget.
- **Boundary start.** After an earlier compaction, the range to summarize
  starts at that checkpoint's `first_kept_entry_id`, and its summary feeds
  the update prompt. This bounds the suffix and the new summary input, not
  user selection.
- **Split turn.** When the cut lands on an assistant message inside a turn,
  the turn's prefix is summarized separately and appended to the summary,
  so the suffix keeps the turn's setup. When that prefix is the only new
  history, it is the main summary input instead. A long first turn, or a
  turn that spans successive checkpoints, then compacts in one call.

The budget is not a hard cap on occupancy: framing, summary and the minimum
suffix can exceed it.

### 4.4 Summarizer input

The summarizer gets a flattened text transcript, never the raw messages:
roles are labelled, tool calls and results are inlined as text, and images
are placeholders. The request therefore cannot break tool-call pairing
rules and sends no image bytes.

User messages in the summarized range stay in the summarizer input even
when they are selected for verbatim retention. The summary then still
covers them if a later checkpoint does not select them. History summarized
before is carried by the previous summary and not fed in again.

### 4.5 Summary requirements

There are three prompts: a first summary, an update of a previous summary,
and a split-turn prefix. They share one system prompt. The summarizer
writes a continuation checkpoint, not a reply. The messages are
source material, not instructions for it.

A summary covers:

- Active tasks, unfinished requests, and applicable constraints and
  preferences.
- Current state and evidence: changes, important commands or tests and
  their observed outcomes, unresolved failures, and unverified work.
- Decisions and their reasons, including failed or rejected approaches
  worth avoiding.
- Remaining work, blockers, and the exact paths, symbols, identifiers and
  error messages needed to continue.

These are coverage requirements, not a schema. Headings and their order are
free, and empty sections are omitted. Plans and assumptions stay distinct
from observed facts and completed work. The summary invents no next steps
and claims no unsupported success.

The update prompt asks for a current checkpoint, not a work log. Facts,
constraints and unfinished requests that still hold carry forward even when
new messages do not repeat them. Newer corrections replace older claims.
The split-turn prompt covers only the supplied prefix and forbids
reconstructing the suffix it cannot see.

The host API can pass free-form focus instructions to the first and update
prompts.

Each summarizer response must finish normally, with text that is not only
whitespace and with no tool calls. An empty, thinking-only, length-limited
or tool-use response fails the compaction. This checks completion, not
quality. The output budget is `min(model max tokens, 8192)` for the summary
and half of that cap for a split-turn prefix.

### 4.6 File lists

Planning scans the summarized messages for `read_file`, `edit_file` and
`write_file` calls and collects their paths, added to the lists of the
previous checkpoint. The lists are appended to the summary text, so the
model sees them, and stored in the entry's `details`, so the next
compaction carries them forward.

## 5. Running a compaction

### 5.1 Steps

The agent must be idle, or paused between a tool batch and the next
inference, with all of its message events stored. The log is locked for
planning and again for storing, never across a summarizer call.

1. **Plan.** Nothing to do ends here, before any event, so a no-op shows no
   indicator. A manual run shows a notice.
2. **Summarize.** Emit `CompactionStart`. Call the model outside the
   session: the exchange touches no transcript, emits no message or
   usage event and is not stored. A split turn takes a second call, whose
   text is appended under `**Turn context (split turn):**`. Append the file
   lists.
3. **Store.** If the compaction was cancelled, stop without writing. Otherwise
   append the checkpoint.
4. **Reseed.** Replace the agent's transcript with the projection from the
   new head. A trailing failed or aborted assistant message without tool
   calls is dropped, so the transcript ends on a user message or a tool
   result and the turn can continue from it.
5. **Publish.** Account the summed summarizer usage once and emit one
   successful `CompactionEnd` with `tokens_after` and the cumulative usage.
   The event carries the checkpoint's durable position, so a client accepts
   or discards the checkpoint and its spend together.

Steps 3 to 5 are one write under the log lock, which hands the event to the
session's publisher before it lets go. No concurrent append can be published
at a higher position first, and once the checkpoint is appended, reseed and
publication always finish. The agent's own listeners get the event after the
lock is released.

### 5.2 Outcomes

- **Compacted**, with `tokens_before` and `tokens_after`.
- **Nothing to do**: nothing to summarize, or already compacted.
- **Cancelled**: the cancel fired before the append. The log is untouched
  and nothing is stored.
- **Failed**: the summarizer or the append failed. No checkpoint is
  written and the live transcript is unchanged. The failure is stored for
  display.

`CompactionEnd` encodes the outcome. A summary without an error is a
success. An error is a failure. Neither is a cancellation.
`CompactionStart` and `CompactionProgress` (summarizing, summarizing the
turn prefix, saving) are not stored.

Usage that was spent is never lost: it is folded into the agent's
accumulator before any listener runs, so a cancellation or a listener
failure cannot erase it.

### 5.3 Automatic compaction

The turn driver runs one turn and its compaction continuations until they
settle. The hosted session and `--print` share it. It reads the policy at
each compaction boundary, so a settings change affects the next check. One
cancel token covers the whole sequence.

- **Overflow recovery, once.** A turn that fails with a context overflow is
  followed by an overflow compaction, and the turn continues. A second
  overflow in the same sequence is not retried: the driver emits a warning
  and returns the error with guidance.
- **After a turn.** When occupancy crossed the threshold, the driver
  compacts and returns. It starts no further turn.
- **During a turn.** After a completed tool batch and before the next
  inference, the driver compacts when occupancy crossed the threshold, and
  the turn continues on the reduced transcript.

Any other error or abort is returned unchanged. A failed or cancelled
compaction ends the sequence with an error.

Pending work is not the driver's job. The host starts a wake turn when an
agent goes idle with a task notification or a queued message pending
([background tasks](background-tasks-spec.md), section 4). A threshold
compaction therefore runs before the wake, and the pending work runs against
the reduced context. After a failed compaction the host holds automatic
wakes of the main agent until the user starts a turn.

### 5.4 Policy per caller

- **Main agent, hosted.** Overflow recovery and the after-turn threshold
  follow `auto_compact`. The during-turn threshold needs `auto_compact` and
  `auto_compact_during_turn`.
- **Sub-agent continuation.** No compaction of any kind.
- **Print mode.** Overflow recovery follows `auto_compact`. There is no
  after-turn threshold, because the run is one-shot. The during-turn
  threshold is as for the hosted main agent. Overflow recovery runs before
  background tasks are torn down, so the retried turn can still use tools.
  With `--format json` the compaction events reach the output stream.

### 5.5 Manual compaction

The `compact` palette action takes no arguments. The host API accepts
optional focus instructions. Manual compaction targets the main agent and
is refused while it is busy, which the interactive client shows as a
notice. There is no `aj compact` subcommand.

## 6. Replay

A reopened session replays a `Compaction` entry as one successful
`CompactionEnd`. The entry does not record the trigger, so the replayed reason
is always manual. `tokens_after` is the estimate of the main thread up to the
checkpoint, which keeps a reopened footer from showing pre-compaction
occupancy. A reopen that does not hold the whole session shows an estimate for
an older compaction. The event's usage is the replayed total before the
checkpoint plus the checkpoint's own. A `CompactionFailed` entry replays as a
`CompactionEnd` with its error.

The summarized entries are still in the log, and replay walks them in
order. The scrollback shows the full branch with a compaction row at
the boundary, while the model context is the reduced projection.

The checkpoint's usage row is keyed by the event's durable ID. Serving an
older cursor again, or rebuilding from a fresh epoch, updates that row. It
neither duplicates it nor overwrites later assistant usage.

## 7. Configuration

| Option | Default | Meaning |
|---|---|---|
| `auto_compact` | `true` | Compact when occupancy crosses `compact_threshold` after a turn. Also gates overflow recovery. |
| `auto_compact_during_turn` | `false` | Also compact the main agent between a tool batch and the next inference. Requires `auto_compact`. |
| `compact_threshold` | `0.85` | Fraction of the model's context window at which automatic compaction fires. Must be in `(0, 1]`. A value outside the range is rejected when the config is parsed. |
| `compact_keep_recent` | `20_000` | Shared approximate verbatim budget in tokens. At most half selects original user messages. The rest keeps a tool-safe recent suffix. Summary and framing are outside it. |

## 8. Limits

- `compact_keep_recent` is a fixed budget, not a fraction of the model's
  window. The minimum indivisible suffix may exceed it.
- The summary is generated with the session's active model. There is no
  separate summarizer model.
- With `auto_compact_during_turn` off, a long tool-call chain inside one
  turn is compacted only after it overflows, which costs one failed
  request before recovery.
