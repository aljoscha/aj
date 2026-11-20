# Queued messages: steering and follow-up

The user can type a message while an agent is working. The message is queued
and shown above the editor. A follow-up is sent when the turn ends. A steering
message is injected right after the next tool batch. A queued message can be
pulled back into the editor and edited.

Queues are keyed by `AgentId`. Every gesture targets the agent the chat view
shows, so the main agent and a sub-agent behave the same.

Every prompt is accepted the same way, whether its agent is idle or busy. The
host saves it as the agent's pending input, answers the command, and wakes the
agent if it is idle. The turn that takes the input stores it as a message and
removes the saved copy in one transaction. "Queued" is what a prompt is while
a running turn stands between it and that step. See section 2.

## 1. UX contract

Each agent has at most one pending message, of one kind: `Follow-up` or
`Steering`. It renders in a box directly above the editor, for the viewed
agent only.

**Composing while the viewed agent is busy**

- **Enter** appends the editor text to the pending message, joined by a
  newline, and clears the editor. With nothing pending it creates a
  `Follow-up`. Enter never changes the kind of an existing message.
- **Alt+Enter** appends the editor text and makes the message `Steering`.
  With an empty editor and a pending `Follow-up` it only promotes the message.
  With an empty editor and nothing pending it does nothing.
- The kind only escalates from `Follow-up` to `Steering`. To demote, pull the
  message back and resubmit it.
- A prompt with an attachment is refused while the agent is busy. The box
  shows text and a pull-back returns text, so a client could neither show a
  waiting attachment nor take it back.
- Alt+Enter is refused while a branch anchor is armed, and so is Alt+Up.

**Editing a queued message**

- **Up** or **Ctrl+P** with an empty, focused editor and a pending message
  moves the text back into the editor and removes the queue entry. The kind is
  forgotten: the next Enter queues a `Follow-up`, the next Alt+Enter a
  `Steering` message. With text in the editor, or with nothing pending, Up
  keeps its normal meaning.
- **Alt+Up** pulls the message back whatever the editor holds. The queued text
  goes in front of the draft, separated by a blank line.

**Delivery**

- A `Follow-up` is sent once the turn ends. The host starts a new turn on the
  agent for it (section 2). The agent counts as busy across that turn, so the
  user can keep queueing.
- A `Steering` message is injected after the running tool batch, before the
  next inference. If the turn ends before a tool batch consumes it, it is
  delivered with the follow-ups when the turn ends.
- A delivered message is an ordinary user message. It is stored and rendered
  like any prompt.

**Idle agent.** Enter and Alt+Enter both send the editor text as a prompt,
with any attachments, and a turn starts for it at once. The prompt shows as a
transcript message and never in the box. The box appears only for a busy
agent, for a message restored on reopen (section 4), and for a prompt whose
turn failed before taking it.

An idle agent can already have a pending message: one restored on reopen. A
prompt sent then joins it, as Enter does for a busy agent, and the turn
delivers one message. The rule is one pending message per agent in every
state, so a message is never overtaken by a later one and the box never has
two to show.

**Display.** The box previews the message as a user bubble with a hint line.
The text is capped at 6 lines, followed by `+N more lines`. The key labels in
the hints follow the user's bindings.

- Follow-up: `queued • sends when the turn ends • Up to edit • Alt+Enter to steer`
- Steering: `steering • sends at the next tool call • Up to edit`

**Keybindings**

| Gesture | Default | Action ID |
|---|---|---|
| Submit or append a follow-up | `enter` | editor submit |
| Submit or append steering | `alt+enter` | `aj.message.steer` |
| Pull back (editor empty) | `up`, `ctrl+p` | fires `aj.message.dequeue` |
| Pull back (always) | `alt+up` | `aj.message.dequeue` |

`alt+enter` never inserts a newline in the prompt editor. Newlines are entered
with `ctrl+j`, with `\` then Enter, and with `shift+enter` on a terminal that
speaks an enhanced keyboard protocol. The actions can be rebound in
`config.toml`.

## 2. Drain points and ownership

The agent drains queued messages at two points:

- **After a tool batch**, before the next inference: steering only. This is
  the drain that makes steering urgent.
- **At the top of a turn**: steering first, then follow-up. Task
  notifications drain just before them.

Pending input reaches the second point through a wake: a turn that
starts with no message of its own and takes what is pending. The host starts
one when it accepts a prompt for an idle agent. It starts one when a turn
ends with a message pending, as it does for a task notification (see
[background tasks](background-tasks-spec.md), section 4). A follow-up is
therefore delivered as a new turn with its own `AgentStart` and
`AgentEnd`. A wake that finds nothing pending starts no turn.

A turn that fails with a fatal error, a refused write for example, is not
followed by a wake. It may have failed before it took the pending message,
and the next wake would fail the same way. The message stays pending and
`continue` or the next prompt delivers it.

The host owns every change the user makes. A frontend sends a prompt, a
steer or a queue removal. The host saves the change, and a refused save
refuses the command and changes nothing. After an edit that leaves a message
waiting behind a running turn the host publishes a `QueueUpdate`, so every
attached client sees the change. It publishes none for a prompt to an idle
agent, which a turn takes at once. The agent publishes one after each drain.
If a turn ends and leaves a message pending that no wake takes, the host
publishes the queue then. The pending-message box renders from the queue
state a client folds out of those events. `QueueUpdate` events are not
stored.

## 3. Cancellation

Cancelling the viewed agent's turn does not drop its pending message. The
frontend first pulls the message back into the editor, then cancels. The order
matters: the host starts a wake turn after the cancelled turn, and that turn
would deliver whatever is still queued.

When nothing is running the cancel chord falls through to the quit ladder and
a queued message stays queued.

## 4. Persistence

A prompt is saved before it is acknowledged, per thread, and delivered
once. This holds for an idle agent as for a busy one: a host that dies after
the acknowledgement and before the turn stored the message leaves the prompt
pending. Attachments are saved with it. A reopen restores each agent's
pending message without starting a turn. The `continue` action delivers the
main agent's, and a sub-agent's is delivered by its next turn. A head switch
is refused while a message is pending, until it is delivered or withdrawn.
Delivered once is [principles](principles.md), rule 4.

A restored message with an attachment cannot be pulled back into the editor,
because a pull-back returns text. The host refuses the gesture. `continue`
delivers the message and clearing the queue discards it.

The first prompt of a new session and of a new branch is saved the same way.
A new session and a new branch are drafts that the store does not hold, so
accepting the prompt creates the session or the branch and saves the prompt in
one transaction. A reopen lands on that thread and restores the prompt.
See [session storage](session-storage.md).

## 5. Limits

- Only the user queues messages. Tools and the model cannot.
- Only plain text waits behind a running turn. Palette actions are not
  queueable.
- The box shows the viewed agent's message only.
