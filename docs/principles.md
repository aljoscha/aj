# Principles

How AJ treats state: what survives, who owns it, and what a client sees.
This page states rules and the reasons for them. It names no functions, tables
or wire fields. A design or change that breaks a rule here either changes this
page first or is wrong.

## The test

State is durable when a user would be annoyed to lose it in a crash. State is
not durable when the user is still making it, or when it only describes work
in flight.

| Durable | Not durable |
|---|---|
| Entries: prompts, responses, tool calls and results | Text in the editor, attachments not yet sent |
| An accepted prompt that is waiting for the agent | Which overlay or view is open |
| A background task, before it starts | A fork point picked but not yet written to |
| A task notification, until the agent is told | Streamed text and thinking of a response in flight |
| Goal and its status | Live output of a running tool |
| Settings, account and environment of a thread | Spinners, timers, running token counts |
| A sub-agent's thread | A Code Mode cell's running state |
| Labels and archive markers | Anything before a session's first accepted prompt |

When a case is not in the table, apply the test. When the test is unclear, ask.

## Words

- **Session**: what a user opens, lists and resumes. It holds threads.
- **Thread**: one line of entries with one agent working on it: the main
  line, a line that continues from an entry of another, or a sub-agent's.
  A thread that continues from another copies nothing.
- **Entry**: one committed item of a thread: a message, a settings change,
  a goal change, a compaction, a sub-agent's spawn or end record. Entries
  are never changed or removed.
- **Message**: what the model sees.
- **Branch**: a thread plus the ones it continues from, up to the root. It
  is the word a user sees.
- **Fork point**: the entry where a thread leaves its parent.
- **Head**: the entry the main agent appends after.
- **Position**: the order of an entry in its session.
- **Input**: a prompt the host accepted and the agent has not taken yet. It
  is steering or a follow-up, and becomes a message when it is taken.
- **Pending**: accepted and not yet delivered. Inputs and task notifications
  are pending until their agent takes them.
- **Task**: background work with an ID: a command, an agent run or a Code
  Mode cell. Its stored form is a task record.
- **Task notification**: what a task tells its agent when it ends.
- **Notice**: text shown to the user only. The model never sees it.
- **Run**: a sub-agent's work from its spawn entry to its end entry.
- **Turn**: an agent working, from taking an input to going idle. Its turn
  state is stored.
- **Round**: one model response and the tool calls it triggers. A turn is
  one or more rounds.
- **Host**: the process that runs a session's agents and writes its state.
- **Client**: what a user looks at. It may be the host's own terminal or
  another machine.
- **Draft**: state the host has not accepted. It is held in memory, by a
  client or by the host, and is not stored.
- **Live overlay**: what a client shows of work in flight.

## Rules

1. **The store is the truth.** Memory is a cache that can be thrown away and
   read again. The store decides order, position and identity inside its
   transaction. No component tells the store what it believes the state to be.

2. **Commit, then act.** Accepting something means committing it. The agent
   holds an entry, a client is told, and an effect starts only after the
   commit. A refused commit refuses the action and changes nothing.

3. **One way to write.** One path commits a change and then publishes it.
   Order of publication is order of commit, by construction.

4. **A pending thing becomes its entry in one transaction.** An input or a
   task notification is saved under the identity of the entry it becomes, and
   the commit of that entry removes it. It is delivered once.

5. **Work outside the host is recorded before it starts.** A task has a task
   record before its command runs. Everything else the host starts dies with
   the host, by a mechanism that needs no cleanup code to run.

6. **A tool call runs at most once.** A call without a result is answered
   "interrupted, may or may not have run", and the model decides what to do.

7. **Quit is a crash.** No shutdown path writes anything a crash would not
   have written. There is one recovery path and every exit takes it.

8. **Opening is a function of stored state.** An open settles what the last
   host left: it answers open calls, takes up tasks, restores inputs. Each
   step can be repeated, so a crash during an open is just another open.
   Whether an interrupted turn then continues by itself or waits for the user
   is a policy applied after settling, the same for every agent.

9. **Nothing exists before its first input.** A new session and a new branch
   are drafts, held in memory and not stored. The first accepted prompt creates
   them and is stored with them. The host is never "on" something the store
   does not have.

10. **Work belongs to a thread.** Inputs, turns, tasks and goals hang off the
    thread they are for, never off "the session's current branch". Which
    branch a client shows is a view.

11. **One host per session.** A lock enforces it. A second host is refused,
    and a host takes a session over only when the first let go or died.

12. **Clients render committed state plus the live overlay.** The overlay is
    lossy and nothing is ever rebuilt from it. A client that attaches late
    converges with one that watched from the start.

13. **A storage failure ends the session.** The host stops and the session is
    opened again. A busy store is not a failure and is retried.

14. **Store once, derive the rest.** No fact is stored twice. What can be read
    off stored state, such as "this turn was interrupted", is not stored.

## Strength

A commit survives a crash of the host. The newest commits can be lost when the
machine loses power. That is enough.

## Cost

These are aims. They yield when meeting them would add complexity elsewhere.

- A turn writes once per entry, plus its start and its end.
- Opening a session reads what the model context and the first screen need,
  not the session.
- An idle session does no work. A running task costs nothing while it is
  quiet.
- A foreground command costs no store write.

## Not goals

- Two hosts writing one session.
- Running a tool call again after a crash.
- Keeping a response that was cut off mid-stream.
- Resuming a Code Mode cell. The model is told it was lost.
- Surviving power loss without losing the last commits.

## Limits that are not principles

These hold today and are meant to go.

- A session has one active main thread. Moving to another needs it to be
  idle, with nothing pending and no task running. The aim is that every
  thread runs on its own and a client only chooses which one to show.
- The host holds the draft of a new session and of a new branch, until
  clients do.
- The terminal UI is usually the host, so closing it stops the session's
  agents. The aim is one long-lived host per machine.
- Nothing removes task output files yet. They should be deleted with their
  session, and old or oversized output should be collected.
- Sessions in JSONL files are still listed and are imported when resumed.
  This lives behind one seam and is removed when the store has proven itself.
