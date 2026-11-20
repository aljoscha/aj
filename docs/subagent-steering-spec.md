# Sub-agent steering

A sub-agent's view is interactive. When the chat view shows a sub-agent, a
submitted prompt goes to that sub-agent and its turn runs concurrently with
the main agent and with other sub-agents. The view behaves like the main view
in every respect except which agent it is wired to.

## 1. Contract

1. **Each agent runs at most one turn at a time. Different agents run
   concurrently.**
2. **Sub-agents are retained.** A sub-agent outlives the tool call that
   spawned it, in both spawn modes, and the process that ran it. The host
   resolves it by its `Sub(n)` ID to drive later turns. IDs are minted once
   per session and never reused.
3. **A continuation appends to the sub-agent's stored thread.** It
   starts no new sub-agent run.
4. **The parent hears from a sub-agent once.** A blocking `agent` call
   returns one report, and a background spawn delivers one task notification.
   Turns the sub-agent runs after that are a side exchange with the user.
   Nothing they produce reaches the parent model.
5. **One set of running agents drives all status display.** `AgentStart`
   adds an agent and `AgentEnd` removes it. The spinner, the footer count and
   the box status derive from it.
6. **The UI is single-threaded.** Concurrency lives in the agent turn tasks.
   Everything reaches a frontend through one ordered event stream.
7. **A sub-agent is its stored thread.** Any sub-agent whose run the
   selected branch shows can be prompted, by whoever can switch to it,
   whichever process spawned it. See section 4.
8. **Sub-agents do not nest.** The `agent` tool is filtered out of a
   sub-agent's tools.
9. **An agent is busy** when the host drives a turn on it or when it is in
   the running set. The first covers the gap between starting a turn and its
   `AgentStart`. The second covers a foreground sub-agent's initial run,
   which is nested in the main turn and not driven by the host.

## 2. Foreground assignments

A blocking spawn creates a foreground assignment: the parent's tool call
waits for one terminal report from the child.

- The assignment outlives an interrupted turn. If the child's initial turn is
  cancelled, the parent keeps waiting, and the user can continue the child
  with ordinary prompts. The first turn that ends without being cancelled
  delivers the report, or the error, to the parent.
- Killing the assignment ends it. The parent's tool call fails with
  `sub-agent assignment cancelled by user`.
- Dropping the parent's tool call, as when the parent's turn is cancelled,
  ends the assignment and any continuation of the child the host is driving.

`AgentStart` and `AgentEnd` carry `assignment_pending`, so a frontend knows
whether the parent still waits.

A background spawn has no assignment. Its run belongs to a background task.
See [background tasks](background-tasks-spec.md).

## 3. Frontend behaviour

### 3.1 Submit routing

A submitted prompt is addressed to the viewed agent. The host saves it as
that agent's pending input, in the sub-agent's own thread, before it
answers. If the agent is idle the host then starts a turn that takes it, and
if it is busy the text waits as a follow-up
([queued messages](message-queue-spec.md), section 2). Message events carry
the target's ID, so rendering and storage route to that agent.

Palette actions are not submitted text, and their scope depends on the
action. Model and thinking selectors act on the viewed agent, or on the main
agent's pending branch draft when a branch anchor is armed. Session actions
such as compaction keep their session scope.

### 3.2 Editor while busy

The editor stays available while the viewed agent is busy. Submitted text
appears in that agent's pending-message box. See
[queued messages](message-queue-spec.md) for steering and queue editing.

### 3.3 Ctrl+C

Ctrl+C acts on the viewed agent.

1. An overlay is open: close it.
2. The viewed agent is busy: pull its queued message back into the editor,
   then cancel that agent. The host routes the cancel to
   whatever owns the turn:
   - a turn the host drives: that turn's token.
   - a foreground sub-agent in its initial run: only that turn is
     interrupted. The assignment stays, and the parent keeps waiting.
   - a detached background sub-agent: its background task is killed.
3. The viewed agent is idle: the first Ctrl+C arms the quit sequence and
   shows a hint, and a second Ctrl+C quits. When other agents or tasks are
   running the hint names them: `N agents / M tasks still running`. Nothing
   is cancelled by arming. A turn that starts while the sequence is armed
   disarms it.

An agent in another view is never cancelled by Ctrl+C. To cancel it, switch
to its view. The agent picker's `ctrl+k` kills a running or pending sub-agent
assignment outright.

### 3.4 Spinner, footer and box status

**Spinner.** The spinner shows the viewed agent's activity: it runs when the
viewed agent is in the running set or is compacting. Switching views updates
it at once.

**Footer.** The footer counts running sub-agents across all views, so the
user sees background activity while viewing an idle agent.

**Box.** The box in the parent's transcript stands for the delegated run.
`SubAgentStart` creates it as `Running`, and `SubAgentEnd` concludes it with
the report. That end is stored, so a reopened or re-attached view shows the
same conclusion. Until it arrives, `AgentStart(Sub n)` and `AgentEnd(Sub n)`
flip the box between `Running` and `Done`, except while its assignment is
pending. After `SubAgentEnd` nothing changes the box. The sub-agent's own
view and the footer count still show it busy.

## 4. Reopened sessions and head switches

The live agent of a sub-agent exists in one process. Its thread is in
the session, and a host that holds no live agent for a sub-agent builds one
from that thread when a prompt, a steer or a settings change names
it. So a sub-agent is prompted after a reopen as before it.

**What is rebuilt.** The transcript is the run's model context. The model,
thinking effort, speed and verbosity are the spawn record's with the changes
recorded since, and a model the host cannot resolve falls back to the
current one of the tool that spawned it. The system prompt, the session
environment, the task registry and the queues are the session's. The cache
key is `<session>:sub:<n>`. The tools and the prompt suffix are the ones the
spawning tool (`agent` or `oracle`) gives a new sub-agent under the
configuration at the time it is built. They are derived once, then, not
each turn as the main agent's are, and again only for a sub-agent whose
model the user changes.

**What is not.** The todo list. What the parent held only in memory when it
spawned the sub-agent: `image_block` and `disabled_tools` are read from the
current configuration instead. The foreground assignment: the parent's tool
call was answered or its turn is over, so the rebuilt sub-agent reports to
nobody. Live Code Mode cells, as for the main agent.

**At an open.** Each sub-agent of the selected branch that has a recorded
command, pending work or an interrupted turn an earlier process left is
built at once and gets what the main agent gets: its
unanswered tool calls are answered, its recorded tasks taken up, its pending
input and undelivered task notifications put back. No turn starts.
Every other sub-agent is built when it is first used.

**An interrupted turn.** What the host does with a turn its last host left
unfinished is one policy, the same for every agent: the turn waits for the
user's `continue`. That holds for the main agent and for a sub-agent whose
turn after its run was cut short, a continuation the user had started. Such
a sub-agent's turn stays stored as running, the first client of the open
gets a notice naming it, and `continue` resumes it together with the main
agent's. A message to the sub-agent continues it as well.

A sub-agent's run, its first turn, is the exception, in both spawn modes.
`continue` does not resume it. It is closed as described next, and its
parent is told once that it can be continued by sending the sub-agent a
message. Resuming it would mean keeping the run open across hosts with its
report still owed to the parent, which a rebuilt sub-agent has no channel
for today.

A sub-agent that cannot be restored or closed this way, because its stored
run cannot be read for example, does not fail the open. It is skipped and
left as stored, a warning is logged, and the first client of the open gets
one notice naming it. Only an error that means the host can no longer
write the session fails the open: a failed append, or a write refused
because another writer has changed the session.

**A run cut short.** A run whose turn was left running and that has no
`SubAgentEnd` was abandoned: its host died, shut down, released the session
or stopped after a storage failure. A host that stops does not end the run
itself, so there is one outcome. The next open appends its one
`SubAgentEnd`, concluded as failed with a report that says it was
interrupted, answers its unanswered tool calls and stores its turn as not
running. The parent of a foreground run has its `agent` call answered with
the same news, tied to the sub-agent by the `call_id` the spawn record
stores. The parent of a background run gets one task notification that says
the run was interrupted and can be continued by sending the sub-agent a
message. That notification was saved when the run started
([background tasks](background-tasks-spec.md), section 7), so the open
finds it among the parent's pending work and saves it only if it is
missing.

A background run saves its task notification before its `SubAgentEnd`. A
host that ends between the two leaves the report as the parent's one
task notification, and the next open records the run's end as finished
without adding another.

A run without a `SubAgentEnd` whose turn is not running was dropped with its
parent's cancelled turn, and is concluded from its last message.

A run the user or its parent stops while the session lives (`task_stop`,
the picker's kill, a cancel aimed at the sub-agent) records its own
`SubAgentEnd`, concluded as failed with `turn aborted`, and gives its
parent a `killed` task notification.

Per contract 4, a prompt to a sub-agent whose box is closed writes no second
spawn or end and sends nothing to the parent.

**Head switches.** A switch drops every live sub-agent. One the new branch
shows is built again when it is next used, and one it does not show is
refused with `sub-agent N has no run on this branch`. A sub-agent spawned
before a fork point is shown by both branches and is the same thread
on both.

## 5. Edge cases

- **Prompt to a busy agent.** The text is queued as a follow-up. No second
  turn starts.
- **Prompt to an idle agent with a restored message.** The prompt joins the
  pending message and one turn delivers both as one message.
- **Continuation after a turn that failed without a reply.** The agent's
  transcript then ends on a user message, and the next prompt appends
  another, as for the main agent.
- **Main ends while a sub-agent continuation runs.** In the main view the
  spinner stops. The footer still counts the sub-agent, and its own view
  shows its spinner.
- **Quit with turns running.** See
  [background tasks](background-tasks-spec.md), section 5.
