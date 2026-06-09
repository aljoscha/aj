# Session lifecycle test coverage

Status: planned. Companion to `session-lifecycle-spec.md`: that
document specifies the clean-slate session worlds; this one
specifies the test coverage that locks their behavior in place.

## 1. Gap

The session lifecycle (`SessionWorld::build` / `install`, the outer
rebuild loop in `InteractiveMode::run`, the `/new` and `/resume`
flows) has almost no direct coverage:

- `aj/tests/replay_parity.rs` proves live-vs-replay *rendering*
  parity, but hand-builds its agent and pump — it never goes
  through `SessionWorld`.
- `session.rs` unit tests cover only the `header_notice` wording.
- The shutdown formatters are unit-tested, but not the accumulation
  of per-session usage.
- The build-failure fallback in the outer loop is entirely
  untested.

A regression in transcript seeding, system-prompt reuse, registry
freshness, or the fallback path would not be caught today.

## 2. Constraints that shape the design

- `SessionWorld::build` is `pub(crate)` and takes
  `&Arc<Mutex<RunConfigSnapshot>>`, whose fields are private to the
  `modes::interactive` module. Integration tests under `aj/tests/`
  cannot touch either. Per the house rule (unit tests live in the
  same module), the lifecycle tests are **unit tests in
  `session.rs`'s `#[cfg(test)] mod tests`** — as a child module of
  `modes::interactive`, it can construct `RunConfigSnapshot`
  directly.
- The scripted provider (`aj_models::scripted::ScriptedProvider`)
  and `tempfile` (dev-dependency) are available; `tokio::test`
  works (tokio is a regular dependency with macros in use).
- Tests must not read the host environment. `SessionWorld::build`
  calls `AgentEnv::new()` internally, which probes cwd/git/context
  files — acceptable for behavior assertions that don't depend on
  env contents (transcript, registry, log round-trips), but
  assertions on the *assembled system prompt's text* must compare
  prompt values against each other (persisted vs. agent-held), not
  against golden strings.
- The outer loop's fallback logic lives inline in
  `InteractiveMode::run` where it cannot be unit-tested; it gets
  extracted (§3.2) into a function with no `Tui` dependency.

## 3. Specification

### 3.1 `SessionWorld` lifecycle tests (in `session.rs`)

Shared test helpers (inside the test module):

- A `StubTerminal` mirroring the one in `replay_parity.rs`
  (fixed 100×24, writes discarded). The duplication is deliberate:
  unit tests can't import from the integration-test crate.
- `scripted_run_config(messages) -> Arc<Mutex<RunConfigSnapshot>>`
  building a snapshot off a `ScriptedProvider` and the scripted
  `ModelInfo` triple (mirror `replay_parity.rs`'s
  `scripted_model_info`).
- `test_world(persistence, run_config, spec) -> SessionWorld`
  wrapping `SessionWorld::build` with a default `Config` and a
  fresh `RenderSettings` / bundled theme.
- A way to drive one scripted turn against a world's agent
  (`agent.lock().await.prompt(...)` with a fresh cancel token) so a
  log accumulates real persisted content.

Tests:

1. **Create seeds the log's system prompt.** Build a `Create`
   world; assert `log.system_prompt()` is `Some` and equals the
   agent's `assembled_system_prompt()`.
2. **Resume round trip.** Build a `Create` world, drive one
   scripted text turn (persistence listener writes the log), drop
   the world; build a `Resume` world on the same session id and
   assert:
   - the new agent's `messages()` contain the user prompt and the
     assistant reply (transcript seeded);
   - the new agent's `assembled_system_prompt()` equals the
     persisted `log.system_prompt()` (cache-warm reuse, byte-equal);
   - `install` on a fresh `Tui` + `build_layout` renders the prior
     conversation into the chat slot (assert the prompt/reply text
     appears in `ChatView::render` output) — this extends replay
     parity through the real `SessionWorld` path.
3. **Clean slate across rebuilds.** Insert a dummy entry into world
   A's `registry` (`SubAgentRegistry::insert` with a throwaway
   shared agent); build world B (Resume of the same session);
   assert `registry.ids()` of world B is empty and world B's
   `usage_summary()` totals are zero.
4. **Sub-agent counter floor.** If the log can be given entries
   carrying `agent_id: Some(n)` through public APIs (e.g. by
   driving the persistence listener with synthetic
   `AgentId::Sub(n)` events, or via an `aj-session` append API),
   assert a resumed world's agent mints sub-agent ids strictly
   greater than `n`. If no reasonable public path exists, cover the
   seam instead: assert `log.max_agent_id()`-driven seeding via
   `AgentSeed` in an `aj-agent` test (mint an id after seeding and
   assert it is `n + 1`), and note the gap in a comment.
5. **Install announcements.** After `install` with each
   `SessionSpec` variant, assert the header component's rendered
   lines contain the session id and the expected notice string
   (render via `Component::render`, mirroring how `header.rs`'s own
   tests do it).
6. **Build failure.** `Resume` with a nonexistent session id
   returns `Err` and creates no session file.

### 3.2 Outer-loop transition extraction + tests (in `interactive.rs`)

Extract the post-`run_session` transition from the outer loop into
a function (same file, near `Shell`):

```rust
/// Outcome of building the next session world after a switch
/// request: the world to install, the spec it was built for (the
/// requested one, or the fallback onto the previous session), and
/// the chat notices to pump after install (switch confirmation, or
/// the failure text followed by nothing — the fallback world's
/// install already announces itself).
struct NextWorld {
    world: SessionWorld,
    spec: SessionSpec,
    notices: Vec<String>,
}

fn build_next_world(
    config: &Config,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    render_settings: &RenderSettings,
    theme: &ThemeHandle,
    persistence: &ConversationPersistence,
    requested: SessionSpec,
    previous_session_id: &str,
) -> Result<NextWorld>
```

Behavior (exactly today's, relocated):

- Requested build succeeds → `notices` is the single switch
  confirmation (`Started a fresh session ({id}).` /
  `Switched to session {id}.`).
- Requested build fails → fall back to
  `Resume { previous_session_id, Switch }`; on success `notices` is
  the single failure line (`Failed to start a fresh session: {err}`
  / `Failed to switch to session {id}: {err}`).
- Fallback also fails → `Err` (outer loop treats as fatal).

The outer loop shrinks to: snapshot usage, call `build_next_world`,
`install`, pump `notices`. No `Tui` enters the function, so it is
unit-testable.

Tests (unit tests in `interactive.rs`, reusing §3.1's helpers via
`pub(super)` or a small shared `#[cfg(test)]` helper module under
`modes::interactive` — implementer's choice, keep it minimal):

1. Successful `Create` request → world on a fresh session id,
   notice `Started a fresh session ({id}).`.
2. Successful `Resume` request → world bound to the requested id,
   notice `Switched to session {id}.`.
3. `Resume` of a nonexistent id with a valid previous session →
   world bound to the previous id, single notice starting with
   `Failed to switch to session`.
4. `Resume` of a nonexistent id with a *also-nonexistent* previous
   id → `Err`.

### 3.3 Out of scope

The `run_session` `select!` loop (key events → slash dispatch →
selector polling) remains uncovered; testing it requires a
synthetic-input terminal driver and is a separate effort. The
shutdown banner's print path stays covered by the existing
formatter tests only.

## 4. Implementation phases

Each phase: `cargo fmt`, `cargo check`,
`cargo clippy --workspace --all-targets`, `cargo test`, one commit.

### Phase 1 — `SessionWorld` lifecycle tests

Implement §3.1. Test-only code; no production changes except
visibility tweaks if strictly needed (justify any in the report).

Commit: `aj: cover the session-world lifecycle with unit tests`

### Phase 2 — extract and test the outer-loop transition

Implement §3.2: the `build_next_world` extraction (behavior-
preserving) plus its tests.

Commit: `aj: extract and test the session-switch fallback
transition`
