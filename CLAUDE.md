# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build/Test/Lint Commands

- Build: `cargo check` or `cargo build`
- Run all tests: `cargo test`
- Run specific test: `cargo test --package package_name -- test_name`
- Run CLI: `cargo run -p aj -- [args]` (e.g. `list-sessions`, `continue <id>`, `continue`)
- Format code: `cargo fmt`
- Lint: `cargo clippy --workspace --all-targets`

The workspace enables strict custom lints (see `[workspace.lints]` in
`Cargo.toml`), so run `cargo fmt` and `cargo check` before reporting a
change done, and `cargo clippy` for anything non-trivial.

## Architecture

AJ is an AI-driven agent for software engineering. The agent follows a
minimal loop pattern, focusing on providing the right set of builtin tools
rather than complex scaffolding.

The workspace splits into focused crates (run `cargo tree` for the
exact dependency edges):

- `aj-models` — wire layer: provider SDKs, unified `Message` /
  `AssistantMessage` / streaming types, model registry.
- `aj-agent` — the `Agent` runtime, the typed `AgentEvent` bus, the
  tool trait, `ToolDetails` for structured tool rendering, and the
  `TaskRegistry` for background tasks (detached bash commands and
  sub-agent runs that outlive their turn).
- `aj-session` — on-disk session format, `ConversationLog`, replay. The
  user-facing surface (CLI, storage) says "session"; internally a
  session's `ConversationLog` holds threads and branches, so both terms
  are intentional.
- `aj-tools` — the builtin tool implementations.
- `aj-tui` — in-process text-UI framework (layout, components, theming).
- `aj-conf` — `~/.aj/config.toml` loader and path helpers.
- `aj` — the binary: CLI parsing, print mode, interactive TUI, command
  palette, selectors.
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

- Rust edition 2024, 4-space indentation (spaces, not tabs).
- Import grouping: std → external crates (including aj_*) → crate imports.
- Use absolute paths for crate imports (`crate::` not `super::`).
- Merge imports from same module, don't merge different modules.
- Error handling: a library boundary exposes a typed error where callers branch on the failure (a `thiserror` enum, e.g. the SDK `ClientError` carrying status + `Retry-After`), and a named opaque error (`Box<dyn std::error::Error + Send + Sync>`, aliased `aj_agent::BoxError`) at render-only seams where the caller only ever displays the cause (tool execution, the event bus, `TurnError`'s `Recoverable`/`Fatal` payloads). Never put `anyhow` in a public library signature. `anyhow` is for top-level application error propagation in the `aj` binary only.
- Follow clippy/rustfmt (enforced with strict workspace lints).

## Testing

- Unit tests live in the same module with `#[cfg(test)]`.
- Integration tests go in `<crate>/tests/`.

## Commit Messages

- Prefix the subject with a scope followed by a colon: `<scope>: <summary>`.
  The scope is the affected crate or area (e.g. `aj`, `aj-models`,
  `aj-tui`, `workspace`). Comma-separate multiple scopes:
  `aj-tui,aj: ...`.
- A Conventional-Commits type may wrap the scope when it adds signal:
  `feat(history): ...`, `perf(history): ...`. Plain `scope:` is fine for
  everything else.
- Write the summary in imperative mood, lower-case, with no trailing
  period (e.g. `aj: rename the model switch command to model use`).
