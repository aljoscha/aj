# Skill: tmux-subagents

Run "real" aj sub-agents inside detached tmux sessions and drive them
programmatically. Use this when you need a sub-agent that can run tools, do
many turns, or work in parallel with others — i.e. more than the in-process
`agent` tool gives you.

The scripts live next to this file under `scripts/`. They are self-contained
`uv` scripts (no installs, no virtualenvs). Each prints JSON to stdout by
default for easy parsing.

Replace `SKILL_DIR` below with the absolute path to this skill's `scripts/`
directory.

## When to use this vs. the in-process `agent` tool

Use **this skill** when any of:
- The task needs many tool calls or long-running commands.
- You want several sub-agents working in parallel.
- You want interactive multi-round back-and-forth with sub-agents.

Otherwise prefer the in-process `agent` tool — it's cheaper and simpler.

## Workflow

1. **Spawn** one or more sub-agents, each with an initial message:
   ```bash
   SKILL_DIR/spawn.py refactor-auth \
       --task "rewrite auth module to use new token API" \
       --message "Read src/auth/*.rs and propose a refactor plan."
   ```
   Use `--cwd DIR` to run in a different project. `--continue-thread [ID]`
   resumes an existing aj thread instead of starting fresh. Pass any extra
   aj flags after `--`, e.g. `-- --model-name claude-sonnet-4-5`.

2. **Wait** until at least one needs you (or has exited):
   ```bash
   SKILL_DIR/wait.py --any refactor-auth bench-runner --timeout 600
   ```
   With no agent names, waits over all registered agents in this project.
   `--all` waits until every named agent is awaiting input or exited.
   Exits 124 on timeout.

3. **Read** what the agent said since your last message:
   ```bash
   SKILL_DIR/read.py refactor-auth --since-last-send
   ```
   Other modes: `--lines N` (visible pane tail), `--full` (entire scrollback).

4. **Send** a follow-up:
   ```bash
   SKILL_DIR/send.py refactor-auth "Now implement the plan; run cargo build."
   ```
   Use `-` as the message to read from stdin (handy for long prompts).
   Use `--keys C-c` to send raw tmux keys instead (e.g. interrupting a tool).
   For permission prompts (`Allow this command? (y/n):`) just send `y` or `n`.

5. **Stop** when done:
   ```bash
   SKILL_DIR/stop.py refactor-auth
   ```
   Graceful by default (Ctrl-C, Ctrl-D, then kill if still alive).
   `--force` kills the tmux session immediately. The aj thread is preserved
   either way and can be resumed later with `spawn.py ... --continue-thread ID`.

## Other commands

- `status.py [NAMES...]` — one-shot snapshot. With no names lists every
  registered sub-agent for this project. Add `--text` for human output.

## State model

`status.py` and `wait.py` report one of:

| state | meaning |
|---|---|
| `working` | aj is processing (model streaming, tool running, etc) |
| `awaiting_input` | aj is at the `you:` prompt, ready for a message |
| `awaiting_permission` | aj is asking `Allow this command? (y/n):` |
| `exited` | the aj process is no longer running in the pane |

`awaiting_input` and `awaiting_permission` are both "needs you" states, and
`wait.py` wakes on either.

## Registry

Sub-agent metadata lives at `~/.aj/subagents/<project_slug>/<name>.json`,
where `<project_slug>` is derived from the current working directory. So:

- Sub-agents are scoped per project automatically.
- Different projects can reuse the same names.
- Restart-safe: the orchestrator can come back later and resume management.

tmux sessions are named `aj-sub-<NAME>` — visible in `tmux ls` for debugging.
You can `tmux attach -t aj-sub-NAME` to watch a sub-agent live (detach with
`Ctrl-b d`; do not type into it while the orchestrator is also driving it).

## Tips

- Use descriptive NAMEs: `refactor-auth`, `bench-runner`, `docs-pass`.
- Always `stop.py` agents you're done with so the registry stays clean.
- If `send.py` complains the agent isn't idle, either wait (`--wait 30`) or
  `read.py NAME --lines 40` to see what it's doing.
- Long messages: `cat prompt.md | SKILL_DIR/send.py NAME -`.

## Concise CLAUDE.md addendum

Drop the following into a project's `CLAUDE.md` (or your user-wide one),
adjusting `SKILL_DIR`:

> **Real sub-agents (tmux skill).** When a sub-task needs its own tools,
> long-running commands, parallel work, or multi-round dialogue, drive a
> full aj sub-agent via the tmux-subagents skill at `SKILL_DIR/`:
>
> - `spawn.py NAME --task "..." --message "..."` — start an agent
> - `wait.py --any [NAMES...] [--timeout S]` — block until one needs you
> - `read.py NAME --since-last-send` — read its latest output
> - `send.py NAME "..."` — reply (or `--keys C-c` to interrupt; `y`/`n` for
>   permission prompts)
> - `status.py [NAMES...]` — snapshot all sub-agents for this project
> - `stop.py NAME` — terminate when done (graceful by default)
>
> Sub-agents are tmux sessions named `aj-sub-<NAME>`; state lives under
> `~/.aj/subagents/<project>/`. Always `stop.py` agents you're done with.
