# Session storage

[Principles](principles.md) states the rules. This page says what the store
holds today and what an operator needs to know.

Sessions live in one SQLite database, `~/.aj/sessions.sqlite3`, scoped by
project. Listings, previews and resume see the current project only.

## JSONL import and export

JSONL is the import and export format. A JSONL session under
`~/.aj/sessions/` that is not imported yet is still listed, previewed and
searched from its file. Resuming it imports it in one transaction, and from
then on the database is the session. The file and its sidecars are never
modified. Do not continue the file with an older AJ binary.

```sh
aj migrate-session SESSION_ID [--dry-run]   # import without opening
aj migrate-session --all [--dry-run]        # every project
aj export-session SESSION_ID exported/
aj import-session exported/SESSION_ID.jsonl
```

`migrate-session` refuses a session a running host holds. `--all` continues
past failures and exits nonzero if there was one.

An import keeps a final line without its newline when it is a complete
entry and drops it when it is not, and a resume says so. Of a duplicated
entry ID the first is kept. A malformed line, a parent link that names no
earlier entry or an unreadable sidecar refuses the import. `import-session`
also refuses an interrupted final line and an ID the project already uses.

Export writes every entry in append order, as this build reads it, so an
export need not equal the file it was imported from. It is history, not a
backup: task output files are not copied.

## Layout

Five tables: `sessions`, `threads`, `entries`, `pending` and `tasks`.

A thread is one line of entries: the main thread, a branch of it, or one
sub-agent run. One that does not start the session names the thread and
entry it continues from, its fork point, and copies nothing. Reading a
branch is reading its thread, then the one it continues from up to the fork
point, and so on to the root. The first thread also holds the system prompt
and the other meta entries, which belong to every branch.

A session and a branch are created by the first input accepted for them. One
transaction writes the session row when the session is new, the thread row,
the entries that precede the first message, and the input as a pending row.
For a session those entries are the system prompt, what it was assembled
from, the settings and the environment, and the row carries a label or
archive marker set before. For a branch they are what the head switch
produced: a settings, account or environment change, the answer to a tool
call the fork point left open, a paused goal. An entry of work appended
before any input (a goal, a compaction) and a turn that starts there create
the thread the same way, without an input.

Until then a new session and a new branch are a draft in the host's memory,
which holds exactly what that transaction will write. A draft that is left,
by a head switch or by the host ending, leaves nothing in the store.

So a thread can hold no entry for a while: between its creation and the
message its input becomes. It ends where it starts, at its fork point. If
its host dies then, a reopen lands on it only while it holds a pending row.
No reader shows it as a branch, and an export does not contain it.

The store files an entry in the transaction that appends it. The writer hands
over the entry, which names the agent it is for and the entry it continues
from, and names the main thread its main agent is on. An entry continues the
thread whose last entry it continues from, when the thread is its agent's.
An entry of the main agent that continues from where its own thread starts,
while that holds no entry, is the thread's first. Otherwise the entry starts
a thread that continues from that entry: a branch or a sub-agent run. A meta
entry goes beside the first thread's entries. The store assigns the position
and answers where the entry landed, and the host's memory follows that
answer. An import files a whole session by the same rule.

One host writes a session at a time. The session lock enforces that, and the
store relies on it.

Every row of `entries` keeps its whole entry and a session-wide position.
Deleting a session's row deletes everything filed under it.

An entry is found five ways. Every append maintains the first three
indexes, and no other but the two named after them:

- by session and position, to read a whole session in append order
- by session and entry ID
- by thread and position (`entries_path`), which also carries the type of
  the entry, so a reader can choose entries without reading one
- by thread and position among the state entries alone (`entries_state`):
  settings, goals, the environment, compactions and the entries that open
  and close a sub-agent's run. Only the append of a state entry maintains
  it.
- by thread and position among the model responses alone (`entries_usage`),
  which holds the usage each reported and why it stopped, read out of the
  entry. No column repeats them. Only the append of a response maintains it.
  SQLite reads the entry as JSON to do so, which every entry is.

`entries_state` lists the types it holds, and so does every query for state
entries. The query's list decides what is read. When a build knows a type
the index does not, the query is answered from `entries_path`, slower and
with the same rows. The store refuses a state entry of a type it does not
know.

`pending` holds the inputs and task notifications that the agent has not
taken yet. Every prompt passes through it, to an idle agent as to a busy
one, the first of a session or branch included: it is saved there before it
is acknowledged, and the append of its message removes it. A prompt that is
one block of text is stored as that text. Any other, one with an attachment
for example, is stored as a line feed followed by its content blocks as
JSON. A background agent run and an open Code Mode cell live in their
host's memory, so each saves a task notification here when it starts that
says it was lost. The notification of its end replaces that one under the
same ID. Whichever is stored when a host ends is what the next open
delivers, once.

`tasks` holds the task records of the background `bash` tasks that are
running, with the pid and process start time that identify each one and the
paths of its output and exit files. A task is keyed by its session and its
task ID.

## Schema changes

The schema is one migration, counted in `PRAGMA user_version`. A schema
change is a new numbered migration after it. The first process to open the
database applies the ones it lacks, each in one transaction. A migration is
a literal that is never edited once released, and a test pins the
fingerprint of each, so an edit fails the build's checks and not a user's
open.

A database from a newer build is refused. So is one whose applied migrations
differ in text from this build's, which `PRAGMA application_id` tells: a
build of another branch wrote it, with a migration of the same number and
other content. The pin cannot see that, so the check at open stays. A
database written by a development build of this branch is refused the same
way and must be moved aside. A `-wal` or `-shm` file without its database is
refused too, because SQLite would replay it into a new one.

To recover, stop every `aj` process, then move the database aside together
with its `-wal` and `-shm` files. `aj doctor` reports these conditions,
`integrity_check` findings and foreign-key violations, and lists every
task record with whether its process still runs. It changes nothing.

## Commit strength

SQLite runs in WAL mode with `synchronous=NORMAL`. A commit survives a process
crash. The newest commits can be lost when the machine loses power.

A write that finds the database locked by another connection has changed
nothing. A host tries a session's write again for up to five seconds, pausing
between tries. A write still refused then fails the action it was for: the
turn ends with an error, and the session goes on. Any other failed append ends
the session's writer, and the session must be reopened.

Every other write waits up to five seconds in place and then fails with
nothing changed: a pending row, a task record, a turn's end, a label change,
an import and what an open settles.

## Task output

Output files are not in the database. They live in `spill_dir` when it is
configured and in `~/.aj/tasks` otherwise. Nothing removes a task's output
file, and the directory has no size cap.

## What a reopen reads

Nothing stores which main thread a session is on. A reopen lands on the
one that holds a pending row, and otherwise on the one last appended to. So a
head switch to another thread's last entry writes nothing, and a
restart does not remember it unless something was done there.

A branch is read by its threads: the one it ends in, then the one that
one continues from up to the fork point, and so on to the first. Each read is a
range of one thread. No read of a reopen, a page of history or a
preview walks the session in append order, so another branch and a sub-agent
run that is not shown cost nothing. The reopened host holds, of the branch
it lands on:

- its entries from where the model context starts, or from a few hundred
  back, whichever is earlier
- the sub-agent runs that reach into that range
- its state entries from before that: settings, goals, the environment,
  compactions. The meta entries of the session are among them.
- the main agent's last response before that range, which names the model
  in use
- the user's earlier messages that the last compaction kept, which are part
  of the model context

What the main agent's earlier responses reported as usage is summed from
`entries_usage` when the session is opened. The entries themselves are not
read.

A session that was never compacted has its whole branch as model context,
and all of it is read.

Everything else is read when it is wanted. Earlier history is fetched in
pages as the reader scrolls. A head switch reads the branch it moves to. A
compaction chooses the user messages to keep among all of the branch's, and
reads the earlier ones when it is planned. Settings, the goal and the
environment at an entry of another branch are read from that branch's state
entries. The branch tree, session info, HTML export and JSON replay read the
whole session.

## What an open settles

Every exit of a host looks the same to the store, so there is one way back
in. An open reads the session, writes what settles it and then installs
the result in the live session. The writes, in order:

1. The main agent's tool calls without a result are answered.
2. A sub-agent run left with its turn working is closed, and the open tool
   calls of every sub-agent that was left work are answered
   ([sub-agent steering](subagent-steering-spec.md), section 4).
3. A task record whose command ended unwatched has its task notification
   saved and its row removed ([background tasks](background-tasks-spec.md), section 7).
4. A goal that was being pursued is paused.

Each write is safe to repeat and to stop after, so a host that dies during
an open, or after it and before a turn ran, leaves a session the next open
settles to the same result. Installing writes nothing and cannot fail: each
agent gets its transcript, its pending input and its undelivered task notifications,
and the tasks still running are watched again.

What happens to an interrupted turn is a policy applied after that, the
same for every agent: it waits for the user's `continue`. A sub-agent's run
that was cut short is closed instead and its parent told
([sub-agent steering](subagent-steering-spec.md), section 4).

Print mode settles a resumed session the same way and installs the main
agent's part. A sub-agent's pending work and the tasks still running stay
stored for the next host, because a print run drives one turn of the main
agent and ends.

## Where today's store departs from the principles

Each of these is to be removed, not relied on.

- The host holds the draft of a new session and of a branch, not the client.
  Its entries are filed in the host's memory by a copy of the store's rule,
  which the store's own filing replaces when the draft is created.
- A thread that was created and never got an entry stays in the
  store. Nothing removes it.
- A prompt with an attachment is refused while its agent is busy, and a
  pending input with an attachment cannot be withdrawn into the editor,
  because clients show and take back pending text only.
- Moving to another thread is refused while anything runs or is
  pending.
