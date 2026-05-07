# aj-next Plan Implementation Progress

Tracking file for `docs/aj-next-plan.md`. Each item maps to a step
in §2 (Phase 0), §4 (Phase 1), or §5 (Phase 2). Use `git log` for
the authoritative state; this file is the bridge between the plan
and the git history.

## Phase 0 — refactor the core (§2)

### §2.0 Preparation

- [ ] Reconnaissance: inspect on-disk thread files in `~/.aj/threads/`
      to choose between serde rename and rewriting walker for
      `UserOutput` → `ToolDetails` migration.
- [x] (b) Move contract types into `aj-agent`: new `events`, `tool`,
      and `message` modules. Types are defined but not yet wired.
- [ ] (c) Flip `aj-tools`'s dependency to `aj-agent` instead of
      `aj-ui`. Tool implementations + `get_builtin_tools()` only.
- [ ] (a) Extract `aj-session` crate from today's
      `aj_models::conversation::*`, with a replay module returning
      `impl Iterator<Item = AgentEvent>`.

### §2.1 Agent emits events alongside `AjUi` calls

- [ ] Agent gains a private bus and emits `AgentEvent` parallel to
      every `self.ui.foo(...)`. Tests subscribe; no production
      subscribers yet.

### §2.2 Refactor tools to `ToolContext` + `ToolOutcome`

- [ ] Per-tool migration off `&mut dyn AjUi` onto `&mut dyn ToolContext`.
  - [ ] `read_file` (Text)
  - [ ] `ls` (Text)
  - [ ] `glob` (Text)
  - [ ] `grep` (Text)
  - [ ] `agent` (Text / SubAgentReport)
  - [ ] `todo_read` (Todos + Text)
  - [ ] `todo_write` (Todos + Text)
  - [ ] `write_file` (Diff)
  - [ ] `edit_file` (Diff)
  - [ ] `edit_file_multi` (Diff)
  - [ ] `bash` (Bash)

### §2.3 Drive the legacy CLI off the bus

- [ ] Atomic swap: add `EventBridgeListener` + persistence listener,
      delete `self.ui.*` and `view.add_*` calls inside
      `Agent::execute_turn` in one commit.

### §2.4 Flip `Agent::run` to bus-only

- [ ] Remove `self.ui` and `&mut ConversationLog` parameter from
      `Agent`. The bus is the only output; `aj-session` is no longer
      reached from `aj-agent`.

### §2.5 Split agent loop from input loop

- [ ] `Agent::prompt` / `Agent::continue_run` API; binary owns
      readline loop, `ConversationLog`, persistence + renderer
      listeners, and replay.

### §2.6 Cleanup

- [ ] Delete `RecordingAjUi`.
- [ ] Delete `aj-ui` crate; absorb types into `aj-agent`.
- [ ] Replace `AjCli` trait impl with a plain `Renderer` struct.

## Phase 1 — `aj-next` (§4)

- [ ] Crate scaffold (`src/aj-next/`).
- [ ] Print mode (text, JSONL).
- [ ] Interactive TUI: layout slots, event pump, components.
- [ ] Selectors (model/thinking/session) and theming.

## Phase 2 — Cutover (§5)

- [ ] Behavioral parity verification for daily flows.
- [ ] Rename `aj-next` → `aj`, delete legacy `aj` crate.
- [ ] Drop `rustyline`, `termimad`, `console`.
- [ ] Remove `AjCli`, `AjCliCommon`, `cli_sub_agent`, `prompt_history`.
- [ ] Update `README.md` and `AGENTS.md`.
