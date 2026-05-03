# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build/Test/Lint Commands

- Build: `cargo check` or `cargo build`
- Run all tests: `cargo test`
- Run specific test: `cargo test --package package_name -- test_name`
- Run CLI: `cargo run -p aj -- [args]` (e.g., `list-threads`, `continue <id>`, `continue`)
- Run specific bin: `cargo run -p aj --bin test_diff`
- Format code: `cargo fmt`
- Lint: `cargo clippy`

## Architecture

AJ is an AI-driven agent for software engineering.

The agent follows a minimal agent loop pattern, focusing on providing the right set of builtin tools rather than complex scaffolding.

## Configuration & Runtime

- Persistent data lives in `~/.aj/` (threads/, history.txt, .env).
- Configuration `.env` is loaded from `~/.aj/.env` and project `.env`; never commit secrets.
- Model selection via flags or env: `--model_api`, `--model_url`, `--model_name` (env: `MODEL_API`, `MODEL_URL`, `MODEL_NAME`).

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

## Version Control

We use jujutsu (jj) for version control; prefer jj over git when possible.
The main branch/bookmark is `main`.

- Create individual jj changes with good descriptions; one logical change per commit.
- Prefix change description titles with the subsystem, e.g. `cli: implement CLI parsing` or `zfs: add pool operations`.
- Verify `cargo build` passes before finalizing a change.
- After `jj describe`, normally run `jj new` to create a fresh change for unrelated or follow-up work.

### jj Operations

- When fixing compilation across multiple changes after a rebase, work oldest-to-newest, one change at a time. Run `cargo build` and verify it passes before moving to the next change.
- Prefer manual file-level reverts over `jj backout` when the change touches files modified in descendant changes.
- When squashing, always verify the target change is correct before executing.
- Use `jj undo` immediately when an operation creates cascading conflicts, rather than trying to fix the mess.
- Never squash or reorder changes without asking first.
