# Session lifecycle refactor: clean-slate session worlds

Status: planned. This document specifies how the interactive TUI
manages agent and UI state across session boundaries (startup,
`/new`, `/resume`), replacing in-place mutation with full
reconstruction.

## 1. Problem

All of the logic lives in `aj/src/modes/interactive.rs`. There are
three near-identical "bind to a session" code paths:

1. **Startup** (`InteractiveMode::run`, roughly lines 366–630):
   resolve/create the `ConversationLog`, repair interrupted tool
   uses, seed the agent transcript, resolve/freeze the system
   prompt, seed the sub-agent counter, build the `EventPump`,
   replay, set header/footer, subscribe the persistence listener.
2. **`perform_session_swap`** (`/resume`): the same steps in a
   slightly different order, mutating shared state in place.
3. **`perform_new_session`** (`/new`): a third copy, minus the
   repair/seed parts.

Session switches "reset" the world by surgically mutating shared
state: `Agent::seed_messages`, `Agent::seed_sub_agent_counter`,
`Agent::set_assembled_system_prompt`, swapping the
`Arc<TokioMutex<ConversationLog>>`, replacing the persistence
`SubscriptionHandle`, rebuilding the `EventPump`, clearing the
`ChatView`, refreshing the header. Every piece of per-session state
must be remembered and reset individually, and several are missed
today:

- **`SubAgentRegistry` is never cleared.** It is injected once at
  startup and survives `/new` and `/resume`, so sub-agents from a
  previous session stay registered: the agent picker can list them
  and they can be prompted against a session they don't belong to.
- **`SessionState` leaks across sessions.** The agent's todo list,
  turn counter, and accumulated usage all survive `/new`. A todo
  list from the previous session is visible to the model in the
  fresh session.
- **`tools_expanded` is dropped on session switch.** The swap paths
  rebuild `RenderSettings` carrying `hide_thinking_block` and
  `show_image_in_terminal` but hardcode `tools_expanded: false`,
  so an `aj.tools.expand` toggle silently reverts.
- The three copies drift: e.g. startup re-linearizes after repair
  while the swap path linearizes once; they are equivalent only by
  accident of how repair appends entries.

The root cause is that everything shares one lifetime (the process)
and session changes are handled by trying to mutate the world back
into a pristine state. The fix is to give per-session state its own
lifetime and rebuild it from scratch on every session change.

## 2. Design

Split the interactive mode's state by how long it actually lives.

### 2.1 `Shell` — process lifetime

Owns everything that must survive a session switch:

| Field | Why it survives |
|---|---|
| `tui: Tui` (+ layout, editor) | no raw-mode teardown/flicker on switch; editor draft text and prompt-history ring live in components |
| `theme: ThemeHandle`, watcher guard + rx | theme and hot-reload are process concerns |
| `config: Arc<std::sync::Mutex<Config>>` | shared with selector outcomes that persist choices |
| `auth: AuthStorage` | credential store; `/login` state is orthogonal to sessions |
| `model_catalog: Arc<Vec<ModelInfo>>` | loaded once |
| `run_config: Arc<std::sync::Mutex<RunConfigSnapshot>>` | a `/model` or `/thinking` choice made mid-process survives `/new` (current behavior, now by explicit design) |
| `conversation_persistence: ConversationPersistence` | sessions directory handle |
| `render_settings: RenderSettings` | `alt+t` / `alt+o` toggles survive switches; each session's pump gets a clone |
| `completed_sessions: Vec<(String, UsageSummary)>` | usage from torn-down session worlds, for the shutdown banner (§2.5) |
| overlay/palette request flags (`AtomicBool`s) | wired into editor callbacks once |

`Shell` lives in a new module `aj/src/modes/interactive/session.rs`
together with `SessionWorld` (the exact split between `session.rs`
and `interactive.rs` may be adjusted during implementation; the
boundary that matters is the struct boundary).

### 2.2 `SessionWorld` — session lifetime

Owns everything else. Built fresh on every session change, never
reseeded after construction:

| Field | Notes |
|---|---|
| `agent: Arc<TokioMutex<Agent>>` | constructed fresh via `build_agent` (§2.3) |
| `registry: SubAgentRegistry` | fresh and empty; injected into the new agent |
| `log: Arc<TokioMutex<ConversationLog>>` | resumed or created |
| `persistence_handle: SubscriptionHandle` | persistence listener on the new agent's bus |
| `event_handle, event_rx` | bus→channel forwarder feeding the pump |
| `pump: EventPump` | built from `shell.render_settings.clone()` |
| `turns: JoinSet<…>`, `turn_cancels: HashMap<…>` | dropping the world drops the (empty, see §2.6) turn set |
| `session_id: String` | convenience copy |

Loop-local UI state (`open_selector`, `login_session`,
`login_task`, `quit_armed`) stays in local variables of the
session-run function; it cannot outlive a session because a switch
can only be triggered with no overlay or login in flight (§2.6).

### 2.3 `build_agent`

A function that constructs a ready-to-run `Agent` from the shell:

```rust
fn build_agent(shell: &Shell, seed: AgentSeed) -> Agent
```

- `env`: a fresh `AgentEnv::new()` per session world. A `/new`
  picks up edits to AGENTS.md files and the current date; a
  `/resume` also rebuilds the env but uses the log's persisted
  system prompt (cache-warm), same as today.
- Provider, model info, stream options, thinking: read from
  `shell.run_config` (not from `Config`), so runtime selector
  choices carry into the new agent. Constructed via
  `Agent::with_provider` + `set_default_thinking` +
  `set_block_images` + `set_sub_agent_registry`.
- Tools: `get_builtin_tools` filtered by `config.disabled_tools`,
  rebuilt per world.
- `seed: AgentSeed { transcript, assembled_system_prompt,
  sub_agent_counter }` — the resume data applied during
  construction (empty/zero for a fresh session). See §2.7 for the
  `aj-agent` API.

The startup-time model *resolution* (CLI/env/config precedence,
`--scripted`, `--api-key`) is unchanged and still happens once,
before the first world is built; its output lives in
`shell.run_config`.

### 2.4 Control flow

```rust
// InteractiveMode::run, after startup resolution:
let mut shell = /* terminal, layout, theme, run_config, ... */;
let mut spec = SessionSpec::from_cli(&args);   // Create | Resume(id)
let exit = loop {
    let mut world = match SessionWorld::build(&shell, &spec) {
        Ok(w) => w,
        Err(err) => /* fallback, see below */,
    };
    world.install(&mut shell, &spec);          // chat reset, replay, header
    match world.run(&mut shell).await {
        Ok(SessionExit::Quit) => break finish(shell, world, Ok(())),
        Ok(SessionExit::Switch(id)) => spec = SessionSpec::Resume(id),
        Ok(SessionExit::New) => spec = SessionSpec::Create,
        Err(fatal) => break finish(shell, world, Err(fatal)),
    }
    shell.completed_sessions.push(world.usage_snapshot().await);
    // world dropped here: agent, registry, subscriptions, pump — all gone
};
```

- `SessionWorld::build` performs: log resolve (create/resume),
  interrupted-tool-use repair, linearize → `AgentSeed`, system
  prompt resolve/freeze, `build_agent`, bus subscriptions
  (persistence + event channel), pump construction. No TUI access.
- `SessionWorld::install` performs the UI side: `ChatView::reset`,
  editor agent-marker reset to `Main`, `pump.sync_footer`, replay
  of the log through the pump, header session-id + notice.
- `SessionWorld::run` contains the current main `select!` loop,
  returning `SessionExit` instead of mutating shared slots.
  `perform_session_swap` and `perform_new_session` are deleted.

Header/chat notices preserve today's strings exactly:

| Entry | Header notice | Chat notice |
|---|---|---|
| startup, fresh | `Chat with AJ — Ctrl+C to quit` | — |
| startup, resume | `Resuming conversation` | — |
| `/resume` switch | `Resumed conversation` | `Switched to session {id}.` |
| `/new` | `Fresh session` | `Started a fresh session ({id}).` |

`SessionSpec` therefore carries the entry reason (initial vs.
switch) so `install` can pick the right notice. Startup-only output
(config diagnostics, `Context:` listing, sandbox warning, auth
warning) is emitted by `run()` once, after the first `install`,
exactly as today — it is process-level, not per-session.

**Build-failure fallback.** Today a failed `/resume` leaves the old
session intact because the swap commits only after all fallible
steps. In the rebuild model the old world is already torn down when
`build` runs, so: on `SessionWorld::build` failure for a *switch*,
surface the error and retry with `SessionSpec::Resume(previous_id)`
(the previous session's log is on disk and current — resuming it
restores the same transcript). If that also fails, exit with the
error. At startup there is no previous session; a build failure is
fatal, as today.

### 2.5 Shutdown banner across sessions

Today the agent survives switches, so the end-of-process usage
banner accumulates everything since process start. With per-session
agents, `Shell::completed_sessions` collects a `(session_id,
UsageSummary)` per torn-down world. At exit:

- If only one session ran (the common case), print exactly today's
  output: one usage block + resume hint.
- If several ran, print one usage block per session, each preceded
  by a dim `Session: <id>` line, in order, then the resume hint for
  the final session. Per-session blocks are more truthful than
  merging (sub-agent ids restart per session, so rows can't be
  meaningfully merged).

The resume-hint eligibility check (session has at least one
user-thread leaf) applies to the final session, as today.

### 2.6 Invariants

- **No turn in flight at a switch.** Already enforced: `/new` and
  `/resume` are refused while `turns` is non-empty
  (`session_busy_notice`). The exit-and-rebuild path keeps this
  guard, so dropping a `SessionWorld` never aborts live work.
  `SessionExit::Switch`/`New` may only be returned when
  `turns.is_empty()`.
- **No overlay or login at a switch.** A switch originates from the
  session-selector confirm or `/new` dispatch, both of which run
  with the overlay already torn down; the palette/login gating
  (`open_selector` / `login_session` checks) is unchanged.
- **Subscription ordering.** Within `build`, the persistence
  listener is attached to the new agent's bus before the world is
  exposed; the old world's listeners die with its agent. The
  cross-wiring window that the old swap code defended against
  (events landing between handle drop and re-subscribe) no longer
  exists because the bus itself is per-world.

### 2.7 `aj-agent`: seeding becomes construction

The three mutation-style methods `seed_messages`,
`seed_sub_agent_counter`, and `set_assembled_system_prompt` exist
to retrofit resume data onto a live agent. With per-session agents
they are only ever called on a freshly constructed, not-yet-shared
agent — i.e. they are construction, not mutation. Consolidate them:

```rust
/// One-shot session seed applied at construction time.
pub struct AgentSeed {
    pub transcript: Vec<AgentMessage>,
    pub assembled_system_prompt: Option<String>,
    pub sub_agent_counter: usize,
}

impl Agent {
    /// Apply a resume seed. Contract: call at most once, before the
    /// agent is shared or drives its first turn.
    pub fn seed_session(&mut self, seed: AgentSeed);
}
```

`seed_session` replaces the three public setters (which are
removed). Callers to update: interactive mode, print mode
(`aj/src/modes/print.rs`), and `aj/tests/replay_parity.rs`. The
print mode keeps its current behavior — it builds one agent per
process anyway.

## 3. What deliberately changes / stays

Behavior changes (all bug fixes or explicit decisions):

1. Sub-agent registry is empty after `/new` and `/resume`
   (previous sessions' sub-agents are no longer promptable —
   matching what the resumed transcript actually contains).
2. Todo list, turn counter, and per-agent usage counters reset per
   session.
3. `tools_expanded` survives session switches (it resets today).
4. The shutdown usage banner itemizes per session when more than
   one session ran in the process.
5. `/new` re-reads AGENTS.md context files and the current date
   into the fresh agent's env (today the startup env is reused).
6. A failed `/resume` switch re-resumes the previous session
   (replayed scrollback) instead of leaving it untouched in place.

Explicitly preserved:

- Model / thinking selector choices survive switches (via
  `shell.run_config`).
- Editor draft text, prompt-history ring, theme, keybindings,
  overlay chords.
- All user-facing notice strings listed in §2.4.
- The mid-turn refusal of session-changing commands.
- Print mode and one-shot flows are untouched except for the
  `seed_session` API migration.

## 4. Implementation phases

Each phase must compile (`cargo check`), be formatted (`cargo
fmt`), pass lints (`cargo clippy --workspace --all-targets`) and
tests (`cargo test`), and land as one commit.

### Phase 1 — extract `build_agent`, share `RenderSettings`

- Factor agent construction out of the scripted/registry branches
  of `InteractiveMode::run` into a helper that takes the resolved
  provider bundle; both branches and (later) `SessionWorld::build`
  use it.
- Make `run()` own a single `RenderSettings` instance; pass clones
  to every `EventPump::new` call site, including
  `perform_session_swap` / `perform_new_session` (which stop
  reconstructing settings by hand). This fixes the `tools_expanded`
  reset and removes the manual `hide_thinking_block` /
  `show_image_in_terminal` carrying.
- Pure code motion otherwise; no behavior change beyond the toggle
  fix.
- Commit: `aj: extract agent construction and share render settings
  across session swaps`

### Phase 2 — introduce `SessionWorld`

- New module `aj/src/modes/interactive/session.rs` with
  `SessionSpec`, `SessionWorld::build`, `SessionWorld::install` as
  specified in §2.4 (minus `run`; the main loop stays in
  `interactive.rs` for now).
- Startup uses `build` + `install`. `perform_session_swap` /
  `perform_new_session` collapse to thin wrappers: build a new
  world, replace the current one (`*world = new_world`), drop the
  old. The agent is now rebuilt per session; the registry, todo
  list, counters reset.
- Per-session usage accumulation for the shutdown banner (§2.5).
- Commit: `aj: rebuild agent and session state from scratch on
  session switch`

### Phase 3 — outer session loop

- Introduce `Shell`, move the main `select!` loop into
  `SessionWorld::run(&mut Shell) -> Result<SessionExit>`, add the
  outer loop from §2.4, and delete the wrapper swap functions.
- `SlashHandled` / `SelectorPollOutcome` gain a session-request
  channel (e.g. `Option<SessionRequest>` where `SessionRequest =
  New | Resume(String)`) that `run` maps to `SessionExit`; the
  slash/selector handlers lose their `log` / `persistence_handle` /
  `pump` parameters in favor of `&mut SessionWorld` or narrower
  borrows.
- Build-failure fallback (§2.4) and shutdown sequencing
  (`finish`: abort turns, stop TUI, print banners) live in the
  outer loop.
- Commit: `aj: drive session switches through an outer
  rebuild loop`

### Phase 4 — `aj-agent` seed API consolidation

- Add `AgentSeed` + `Agent::seed_session`; remove `seed_messages`,
  `seed_sub_agent_counter`, `set_assembled_system_prompt` from the
  public API; migrate interactive mode, print mode, and
  `replay_parity` tests; document the at-most-once-before-sharing
  contract.
- Commit: `aj-agent,aj: consolidate session seeding into a
  one-shot construction API`
