# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build/Test/Lint Commands

- Build: `cargo check` or `cargo build`
- Run all tests: `cargo test`
- Run specific test: `cargo test --package package_name -- test_name`
- Run CLI: `cargo run -p aj -- [args]` (e.g. `list-sessions`, `continue <id>`, `continue`)
- Format code: `cargo fmt`
- Lint: `cargo clippy --workspace --all-targets`
- Scratch-space guard: `./scripts/check-test-scratch.sh` (runs the suite
  under an empty `TMPDIR` and fails on residue)

The workspace enables strict custom lints (see `[workspace.lints]` in
`Cargo.toml`), so run `cargo fmt` and `cargo check` before reporting a
change done, and `cargo clippy` for anything non-trivial.

## Architecture

AJ is an AI-driven agent for software engineering. The agent follows a
minimal loop pattern, focusing on providing the right set of builtin tools
rather than complex scaffolding.

The workspace splits into focused crates under `src/` (run
`cargo tree` for the exact dependency edges):

- `aj-models` — wire layer: provider SDKs, unified `Message` /
  `AssistantMessage` / streaming types, model registry, the scripted
  provider used by tests.
- `aj-agent` — the `Agent` runtime, the typed `AgentEvent` bus, the
  tool trait, `ToolDetails` for structured tool rendering, message
  queues, and the `TaskRegistry` for background tasks (detached bash
  commands and sub-agent runs that outlive their turn).
- `aj-wire`: remote-control protocol models, frame and event
  compatibility wrappers, and strict JSON codecs. It contains no I/O
  or HTTP types.
- `aj-session` — on-disk session format, `ConversationLog`, replay. The
  user-facing surface (CLI, storage) says "session"; internally a
  session's `ConversationLog` holds threads and branches, so both terms
  are intentional.
- `aj-tools` — the builtin tool implementations.
- `aj-app` — frontend-agnostic application logic: CLI surface, session
  composition (`SessionCore`), the turn driver, the `ChatState`
  reducer, print mode, settings. CI (`scripts/check-no-tui-dep.sh`)
  keeps it free of TUI dependencies.
- `aj-conf` — `~/.aj/config.toml` loader and path helpers.
- `vaxis` (+ `vaxis-derive`, `vaxis-ucd`) — the terminal-UI framework
  (rendering, widgets, layout, input).
- `aj` — the binary: interactive TUI, command palette, selectors,
  overlays, wiring `aj-app` to `vaxis`.
- `anthropic-sdk` / `openai-sdk` — thin async clients used by
  `aj-models`'s provider adapters.

Frontends (TUI, print mode, tests) subscribe to the agent's `AgentEvent`
bus via `Agent::subscribe(...)`. Persistence is just another subscriber.
`Agent::prompt` does not take a `&ConversationLog`; the binary owns the
log and registers a persistence listener.

## Configuration & Runtime

Persistent state lives under `~/.aj/`:

- `.env` — secrets (API keys); loaded before the project-local `.env`.
- `config.toml` — defaults (model, thinking level, speed, theme,
  disabled tools/skills).
- `models.json` — model catalog; refresh with `aj update-models`.
- `skills/` — user-level skills (SKILL.md directories); also discovered
  from `~/.agents/skills/`, `~/.claude/skills/`, and project-level
  `.aj/`/`.agents/`/`.claude/` `skills/` dirs up to the git root.
- `themes/<name>.json` — optional user themes layered on top of the
  bundled `dark` / `light` palettes. Hot-reloads on file changes.
- `sessions/<project>/` — JSONL conversation logs, one file per session.

Model selection precedence (highest to lowest): CLI flags
(`--model-api`, `--model-url`, `--model-name`) → env vars (`MODEL_API`,
`MODEL_URL`, `MODEL_NAME`) → `config.toml` → built-in defaults. Never
commit secrets.

## Code Style

- Import grouping: std → external crates (including aj_*) → crate imports.
- Use absolute paths for crate imports (`crate::` not `super::`), except `use super::*;` in `#[cfg(test)]` modules.
- Merge imports from same module, don't merge different modules.
- Error handling: a library boundary exposes a typed error where callers branch on the failure (a `thiserror` enum, e.g. the SDK `ClientError` carrying status + `Retry-After`), and a named opaque error (`Box<dyn std::error::Error + Send + Sync>`, aliased `aj_agent::BoxError`) at render-only seams where the caller only ever displays the cause (tool execution, the event bus, `TurnError`'s `Recoverable`/`Fatal` payloads). Never put `anyhow` in a public library signature. `anyhow` is for top-level application error propagation in the `aj` binary only.
- Follow clippy/rustfmt.

## Testing

- Unit tests live in the same module with `#[cfg(test)]`.
- Integration tests go in `<crate>/tests/`.
- Mutation-check tests that pin subtle properties: temporarily break
  the behavior the test names (flip the rule, remove the guard,
  reinstate the bug), verify exactly that test fails, then revert. A
  test that stays green under the mutation is vacuous and must be
  fixed. Assertions like `assert_ne!` on values that are almost never
  equal, or events dispatched to a widget that does not handle them,
  are the usual tells.
- The strongest mutation is deleting the feature's wiring: leave the
  widget out of the composed layout, drop the sync call from the
  drive loop, and check the suite goes red. Tests that drive a
  component directly through a helper prove the component, not the
  feature. At least one test per feature must exercise the real
  path end to end (real input bytes, the composed tree).
- Test helpers return owning guards (`TempDir`), never bare paths, so
  scratch-space lifetime is compiler-checked. State that must outlive
  a test (leaked runtimes, async reaps) lives in a per-test
  subdirectory under one named per-process root. No manual teardown,
  a failing assertion skips it.

## Commit Messages

- Prefix the subject with a scope followed by a colon: `<scope>: <summary>`.
  The scope is the affected crate or area (e.g. `aj`, `aj-models`,
  `vaxis`, `workspace`, `docs`). Comma-separate multiple scopes:
  `aj-app,aj: ...`.
- A Conventional-Commits type may wrap the scope when it adds signal:
  `feat(history): ...`, `perf(history): ...`. Plain `scope:` is fine for
  everything else.
- Write the summary in imperative mood, lower-case, with no trailing
  period (e.g. `aj: rename the model switch command to model use`).
