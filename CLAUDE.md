# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build/Test/Lint Commands

- Build: `cargo check` or `cargo build`
- Run all tests: `cargo test`
- Run specific test: `cargo test --package package_name -- test_name`
- Run CLI: `cargo run -p aj -- [args]` (e.g. `list-threads`, `continue <id>`, `continue`)
- Format code: `cargo fmt`
- Lint: `cargo clippy --workspace --all-targets`

## Architecture

AJ is an AI-driven agent for software engineering. The agent follows a
minimal loop pattern, focusing on providing the right set of builtin tools
rather than complex scaffolding.

The workspace is split along the dependency graph from
`docs/aj-next-plan.md`:

```
aj-models  ←  aj-agent  ←  aj-tools
                ↑              ↑
                └─  aj-session  ─┘
                        ↑
                        aj
```

- `aj-models` — wire layer: provider SDKs, unified `Message` /
  `AssistantMessage` / streaming types, model registry.
- `aj-agent` — the `Agent` runtime, the typed `AgentEvent` bus, the
  tool trait, and `ToolDetails` for structured tool rendering.
- `aj-session` — on-disk thread format, `ConversationLog`, replay.
- `aj-tools` — the builtin tool implementations.
- `aj-tui` — in-process text-UI framework (layout, components, theming).
- `aj-conf` — `~/.aj/config.toml` loader and path helpers.
- `aj` — the binary: CLI parsing, print mode, interactive TUI, slash
  commands, selectors.
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
  disabled tools).
- `models.json` — model catalog; refresh with `aj models update`.
- `themes/<name>.json` — optional user themes layered on top of the
  bundled `dark` / `light` palettes. Hot-reloads on file changes.
- `threads/<project>/` — JSONL conversation logs, one file per thread.

Model selection precedence (highest to lowest): CLI flags
(`--model-api`, `--model-url`, `--model-name`) → env vars (`MODEL_API`,
`MODEL_URL`, `MODEL_NAME`) → `config.toml` → built-in defaults. Never
commit secrets.

## Code Style

- Rust edition 2024, 4-space indentation (spaces, not tabs).
- Import grouping: std → external crates (including aj_*) → crate imports.
- Use absolute paths for crate imports (`crate::` not `super::`).
- Merge imports from same module, don't merge different modules.
- Error handling: Use `thiserror` for defining error types in library crates. `anyhow` is acceptable for top-level application error propagation.
- Follow clippy/rustfmt (enforced with strict workspace lints).
- `snake_case` for functions/variables, `PascalCase` for types/traits, `SCREAMING_SNAKE_CASE` for constants.
- Use proper capitalization and punctuation when writing docstrings or comments.

## Rust Compilation

- When you make code changes, run `cargo fmt` and `cargo build` after each logical unit of change. Fix any compilation errors before committing.
- When refactoring function signatures or types, grep for all call sites and update them in the same pass.
- Check visibility (`pub`) before accessing fields/methods from other modules.
- Read and understand existing code before modifying it. Don't edit blind.

## Testing

- Unit tests live in the same module with `#[cfg(test)]`.
- Integration tests go in `<crate>/tests/`.
