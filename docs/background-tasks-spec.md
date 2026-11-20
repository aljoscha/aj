# Background tasks

The model can run `bash` commands and sub-agents in the background. The tool
call returns at once with a task ID. The work continues on a detached driver.
The model observes it with `task_output` and `task_stop`, and receives a
task notification in its transcript when the task ends. A background `bash`
command is a process of its own that outlives `aj` and is found again by the
next host that opens the session (section 7). A task notification that arrives
while the owning agent is idle wakes it: the host starts a turn so the agent
reacts without polling and without waiting for the user.

The user sees background work in the footer and the agent picker, with live
output in the transcript.

Code Mode cells use the same registry and task IDs. See
[Code Mode](code-mode.md).

## 1. Contract

1. **A `run_in_background` parameter** on `bash` and `agent` selects the mode.
   There are no separate background tools.
2. **Two tools** make up the rest of the model-facing surface: `task_output`
   and `task_stop`. There is no list tool and no multi-task wait. IDs arrive
   in started results and in task notifications.
3. **One ID space** covers bash tasks, background agent runs and Code Mode
   cells. IDs are minted per session from 1 and shown as `#1`, `#2`. The
   session stores the highest ID it minted, so a restart does not use one
   twice. An agent-backed task also names its `Sub(n)` ID.
4. **The original tool result is the "started" result.** It returns at once. A
   command that dies right after launch surfaces through its task
   notification.
5. **A task notification is always delivered**, even when the model already
   read the output. It is a typed transcript message that the model sees as a
   user message wrapped in `<task-notification>`. The tag marks it as injected
   by the harness, so the model does not take it for a reply from the user. It
   is stored like any other message.
6. **A task notification wakes an idle owner** (section 4).
7. **Reads are stateless.** `task_output` returns the status and the current
   rolling tail. Repeated calls return overlapping content. The output file is
   the full output, and the model reads it in slices with `read_file` and
   `offset` / `limit`.
8. **Tasks hang off a session-level cancellation root**, not the turn token. A
   task outlives the turn that started it. A background `bash` command also
   outlives the session and the process: only `task_stop`, the user's kill and
   its own exit end it. A background agent run and a Code Mode cell end with
   the session.
9. **Task events are not stored.** `TaskStart`, `TaskOutput` and `TaskEnd`
   carry the `call_id` of the launching tool call, so a frontend updates that
   call's cell in place.
10. **Sub-agents may start background bash tasks.** Background agent spawns
    are for the main agent only, because the `agent` tool is filtered out of a
    sub-agent's tools.
11. **A task belongs to the agent that started it.** The notification goes to
    that agent, and only that agent can read or stop the task with the tools
    (section 2.3).
12. **One registry per session** holds the tasks of the main agent and every
    sub-agent. It knows nothing of processes or of SQLite: a tool supplies the
    output source, and the session supplies the store that records running
    work.

## 2. Tools

### 2.1 `bash` with `run_in_background: true`

The command runs detached from the `aj` process, under a shell wrapper in a
session and process group of its own. The wrapper writes the command's output
to one file and its exit status to another. A foreground command uses the
same runner, and ends with the host (section 2.4).

- **Record first.** The wrapper runs nothing until the host tells it to, and
  the host does so only once the task record is saved
  ([principles](principles.md), rule 5). A refused record fails the call, and
  a host that dies first leaves a wrapper that exits. Either way no command
  runs without a record.
- **Files.** Output and exit files are named `aj-bash-XXXXXX.log` and
  `.exit` ([session storage](session-storage.md) says where). The exit file
  is removed once the task's end is saved.
- **One stream.** Stdout and stderr share the output file in the order the
  command wrote them. Tails, byte totals and truncation markers are of the
  combined stream and are reported as stdout. `STDERR:` does not appear.
- **Exit status** is the shell's: a command killed by signal `N` ends with
  exit code `128 + N`. If the wrapper itself is killed outright, the process
  that started it writes the status it reaped. No other process can, and then
  the task is lost: its process is gone and left no exit status (section 7).
- **Watching.** The driver reads the output file as it grows and emits
  throttled `TaskOutput`. While the file does not grow the driver looks at it
  less often, down to once a second, so output that resumes after a quiet
  spell shows within a second. It learns of the end from the wrapper it
  reaps. For a task it did not start it waits on a descriptor of the
  wrapper's process on Linux, and elsewhere looks at the exit file and the
  pid once a second. A pid is the task only with the recorded start time, so
  a reused pid is not mistaken for it.
- **Kill.** `SIGTERM` to the process group, then `SIGKILL` after 2 seconds to
  whatever is left. Nothing is signalled unless the wrapper is still the
  recorded process.
- **Descendants.** The task ends when the command does. A process the command
  left behind keeps running and is not stopped. A double-forking daemon
  escapes the process group and therefore a kill.
- `timeout` is ignored. The task runs until it exits or is stopped.
- **Without a task record** (print mode, or a registry with no store) the task
  cannot be found again, so shutdown kills it, and so does its driver being
  dropped.

The started result:

```
Started background task #3: cargo build --release
Output is being written to <output path>, read it with read_file
(supports offset/limit). You will be notified when it completes.
```

### 2.2 `agent` with `run_in_background: true`

The sub-agent is built, announced with `SubAgentStart` and retained as in a
blocking spawn. Then:

1. A task of kind `Agent` is registered. Its cancel token is the run's
   cancellation. `task_stop`, the picker's kill, a cancel aimed at that
   sub-agent and shutdown reach the run. Cancelling the parent's turn does
   not. Before the run starts, the notification its parent gets if the host
   ends first is saved (section 7). A refused save fails the call.
2. A detached driver runs the sub-agent's turn, saves the task
   notification with the report or the error as its body, emits `SubAgentEnd`,
   and finishes the task. A run that completed has exit code 0 and one that
   failed has code 1. A run that shutdown stops gets neither the event nor a
   task notification (section 5).
3. The tool result reads `agent 2 started in background (task #5)` followed by
   the "you will be notified" hint.

The run emits `AgentStart(Sub n)` and `AgentEnd(Sub n)`, so the sub-agent
shows as running and the footer counts it.

### 2.3 `task_output` and `task_stop`

Both may overlap a sequential tool call, so a read or a stop is never
blocked by the work it controls.

**`task_output { id, block = true, timeout = 30 }`**. With `block`, the call
returns when the task reaches a terminal status or the timeout passes. A
timeout is not an error. It is a normal "still running" report. Cancelling
the turn cancels the wait, not the task. With `block: false` the call reports
at once.

For a bash task the result is the status, the rolling tail with the standard
bash truncation budgets and markers, and the output path. For an agent task
it is the status while the run is live and the final report once it is done.

**`task_stop { id }`** cancels the task's token, waits up to 5 seconds for a
terminal status and returns the final status and output tail. Stopping a
finished task is not an error. Stopping an agent task cancels the run. The
sub-agent stays retained and can still be prompted.

**Authorization.** IDs are session-wide, so an agent can name a task it did
not start. Both tools act only for the agent that owns the task, whether the
caller is a parent, a child or a sibling. A task therefore becomes
unreachable through the tools once its owner finishes. Its output stays
readable in the owner's transcript, and the picker lists and kills tasks of
any owner.

The two refusals are error results: `No background task with id {id}.` and
`Background task #{id} belongs to another agent.` Existence is not hidden.
Both append the caller's own live task IDs so the model can correct itself.

### 2.4 `bash` in the foreground

A foreground call runs its command through the same wrapper and files, and
waits for it. What differs from a background task:

- **No task and no task record.** The command takes no task ID, has no registry
  entry and no task events, and brings no task notification. Nothing is
  written to the session store for it. The call's tool result is its account:
  the output tail with the standard budgets and markers, and `Command failed
  with exit code: N` for a nonzero status. Live output reaches the UI as
  `ToolExecutionUpdate` snapshots.
- **It ends with the host.** The host holds the write end of the wrapper's
  stdin pipe for as long as the call lives. A watcher in the wrapper reads
  that pipe and sends `SIGKILL` to the command's process group when it closes.
  The system closes it when the host process ends, however it ends, so no
  cleanup code has to run ([principles](principles.md), rule 5). The watcher
  ignores `SIGTERM`, so a polite stop does not disarm it.
- **Files.** They are removed as soon as the result is built. The output file
  stays when the result names it: as the full output of a truncated tail, or
  because a process the command left behind is still running and may write to
  it, which the result says.
- **Timeout.** `timeout` stops the command: `SIGTERM` to the process group,
  then `SIGKILL` after 2 seconds.
- **Cancel.** The agent drops a cancelled call's future. The drop stops the
  command the same way, and the call gets the `bash: cancelled by user`
  result. The pipe stays open through the 2 seconds. If the runtime ends
  before they are over, the pipe closes and the group is killed at once.
- **Descendants.** As for a task, a process the command left behind after it
  ended keeps running, and so does one that left the process group.

## 3. Task notifications

For bash the notification body is the exit status, the final output tail and the
output path. For an agent it is the report.

```
<task-notification>
Background task #3 finished: cargo build --release — exit code 0
<output tail, if any>
Full output: <output path>
</task-notification>
```

A bash task whose process is gone without an exit status is reported lost,
with the output its file holds:

```
Background task #3 lost: cargo build --release — status unknown. Its process
is gone and left no exit status: it was killed, or the machine restarted.
```

The owning agent drains its task notifications at two points, in arrival
order:

1. **At the top of a turn**, before a new prompt is appended. Task
   notifications that arrived while the agent was idle come before the user's
   message.
2. **After a tool batch**, before the next inference.

The tag is added only when the message is projected for the model.

A task notification cannot be delivered between a tool call and its result,
because the provider requires the results to follow the call. The earliest
delivery is after the batch's last result. A foreground tool call can run for
any length of time, so the delay has no wall-clock bound. This is why the tool
descriptions tell the model not to sleep in the foreground.

Task notification bodies use the same budgets as bash results.

## 4. Wake

An agent cannot start its own turns. The host does, with the wake that also
starts the turn for a user prompt ([queued messages](message-queue-spec.md),
section 2). A wake on an agent with no task notification and no queued message
does nothing and emits nothing. Otherwise it drains them and runs a turn.

The host starts a wake turn on an idle agent at three points:

1. **On `TaskEnd`**, for the task's owner.
2. **When a driven turn completes** and the agent has a task notification or a
   queued message. This covers a task that ends after the turn's last drain
   point, and a turn that aborted with task notifications pending. A turn that
   failed with a fatal error is the exception: the next wake could fail the
   same way, so the work waits for `continue` or the next prompt.
3. **On `AgentEnd`** with a task notification or a queued message. A
   sub-agent's initial run is nested in its parent's turn, so point 2 never
   sees it end.

A busy owner is left alone. Its running turn drains the notification after the
next tool batch, or point 2 picks it up. Several triggers may fire for one
notification. The losers are no-ops.

A wake turn is an ordinary turn. It is serialized with the agent's other
turns, the user can cancel it, and it shows in the spinner and footer. A
sub-agent is woken in its own view, also after its run ended. Nothing is
passed on to the main agent.

A failed compaction holds automatic wakes of the main agent. See
[compaction](compaction-spec.md), section 5.

Print mode has no wake loop. It ends with the main turn, a task
notification that arrives after the final turn is never delivered, and tasks
it started that still run are killed at exit. Print mode that resumes a
session settles it as a host does and delivers the main agent's pending input and task
notifications with its prompt. It does not watch the recorded tasks an earlier
host left running: they stay recorded for the next host.

## 5. Cancellation and shutdown

A task's token is cancelled by `task_stop`, by the picker's kill, by a
cancel aimed at a detached sub-agent, or through the session's root, which
shutdown cancels. The turn token does not reach task drivers. Cancelling a
turn cancels a blocking `task_output` and leaves the task running.

Shutdown is not a cancel. It closes the session for writing, stops the
turns where they stand, cancels the root and waits up to 5 seconds for the
drivers. Nothing is written after the close, so what it leaves is what a
host that died leaves:

- A recorded bash task keeps running with its row. The next open takes it
  up (section 7).
- A foreground bash command ends with its tool call, as it does through
  its pipe after a host that died (section 2.4). Its call stays without a
  result, and the next open answers it as interrupted.
- A bash task that is not recorded is killed.
- A Code Mode cell ends. The loss it announced stays stored, and the next
  open delivers it (section 7).
- An agent run is stopped without a `SubAgentEnd`, a `TaskEnd` or a
  task notification. The next open closes it as interrupted
  ([sub-agent steering](subagent-steering-spec.md), section 4).

A user's cancel (Ctrl+C on a turn, `task_stop`, the picker's kill) is not a
shutdown. It writes its results as before: the aborted message, the
cancelled tool results, the `killed` task notification.

The quit hint counts running work
([sub-agent steering](subagent-steering-spec.md), section 3.3). The bash
tasks among it keep running after the quit.

## 6. Frontend

- **Footer.** `2 agents, 1 task (Alt+A)`, each part shown only when nonzero.
  An agent-backed task counts as an agent, not as a task. A second part
  counts task notifications pending for the viewed agent.
- **Picker.** The agent picker lists tasks below the agents. Confirming a
  task row opens a viewer with the command, a live status line and the
  scrollable output. `ctrl+k` kills the selected running task in both.
- **Live output.** `TaskOutput` updates the bash cell of the launching call
  in place. The cell keeps tailing after `ToolExecutionEnd` and freezes on
  `TaskEnd` with a status badge.
- Frontends read tasks from the chat model, which is built from task events
  and from the task table a `caught_up` frame carries. They do not read the
  registry, so a remote client behaves the same.

## 7. What an open does with recorded tasks

Opening a session answers the tool calls the last host left without a
result, then takes up its tasks. It does both for the main agent and for
each sub-agent the selected branch shows, as that agent. It starts no turn.
Every step leaves state the next open finishes from. One function does
this for every open ([session storage](session-storage.md), "What an open
settles").

A `bash` call without a result that a task row names launched that task,
and the host ended before it reported the launch. It is answered
`The command is still running, as background task #3: ...`. Every other call
without a result gets the generic "interrupted" result. That includes a
foreground `bash` call, whose command ended with the host.

**A task**, by its row:

| The row's command | The task |
|---|---|
| left an exit file | finished unwatched: its notification is saved, the row removed |
| is alive with the recorded start time | runs in the registry again under its ID, as if started by this host |
| neither | is lost: a task notification saying so is saved, the row removed |

A task notification saved at the open is delivered once, at the next turn's
drain point or by the `continue` action.

An open reads the end of a task's output file, as much as a rolling tail
holds, so its cost does not depend on what the command has written. The byte
total is the file's length. The lines before that part are not counted, so
the truncation marker of such a task's notification counts bytes:
`[Showing last 48.0KB of 2048.0MB of stdout. Full output at <path>]`.

A save of a task's end that fails changes nothing, for every kind of task.
The driver tries three more times over 7 seconds, then lets go of the task
and leaves what the store holds for the next open: a bash task's row, or
the loss an agent run or a cell announced. Nothing is delivered from
memory.

**Agent runs and Code Mode cells** live in the host's memory and end with it.
Neither has a row. Each announces its loss instead: when a background agent
run starts, and when a cell yields and becomes a task, a task notification
that says the work was lost is saved among its owner's pending work, before
the tool call is answered. A refused save fails the spawn, and ends the cell.
The task's notification is saved under the same ID and replaces the
announcement, so the store holds one notification for the task at any time:
the loss until the task's end is saved, the completion from then on. The next
open delivers whichever it finds, once, with the owner's other pending work.

For an agent run the announcement says the run was interrupted and can be
continued by sending the sub-agent a message. The next open also closes the
run ([sub-agent steering](subagent-steering-spec.md), section 4). For a
cell it says the cell was lost and its state is gone:

```
Code Mode cell <id> was lost: the session ended while it was open. Its
state is gone, with any result that was not collected. Tool calls it had
made may or may not have run.
```

A cell that never yielded belongs to its `exec` call, which the open
answers as interrupted.

A head switch is refused while any task runs or a task notification is
undelivered ([remote control](remote-control-spec.md), section 4), so a switch
takes up and settles nothing.
