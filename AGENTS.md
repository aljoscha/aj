# Repository Guidelines

## Project Structure & Module Organization
- Root workspace managed by `Cargo.toml` with members under `src/`:
  - `src/aj` (CLI entrypoint), `src/aj-agent` (agent runtime), `src/aj-conf` (config/env), `src/aj-models` (model API), `src/aj-tools` (tools), `src/aj-ui` (UI), `src/anthropic-sdk`, `src/openai-sdk`.
- CLI binary: `src/aj/src/main.rs`; extra bins in `src/aj/src/bin/`.
- Persistent data lives in `~/.config/aj/` (e.g., `threads/`, `history.txt`, `.env`).

## Build, Test, and Development Commands
- Build workspace: `cargo build --workspace`.
- Run CLI: `cargo run -p aj -- [args]` (examples: `list-threads`, `resume <id>`, `resume-latest`).
- Run specific bin: `cargo run -p aj --bin test_diff`.
- Tests: `cargo test --workspace` (or `cargo test -p aj-agent`).
- Lint: `cargo clippy --workspace -- -D warnings`.
- Format: `cargo fmt --all`.

## Coding Style & Naming Conventions
- Rust edition: workspace targets 2024; follow standard Rust conventions.
- Indentation: 4 spaces; no tabs.
- Naming: snake_case for files/functions, CamelCase for types/traits, SCREAMING_SNAKE_CASE for consts.
- Keep modules small; colocate private tests next to code.

## Testing Guidelines
- Prefer unit tests in the same module with `#[cfg(test)]`.
- Integration tests in `<crate>/tests/` (files ending with `_test.rs` or descriptive names).
- Aim for clear, behavior-focused tests; avoid network calls.
- Run `cargo test --workspace` before pushing.

## Commit & Pull Request Guidelines
- Commit style: `area: short, imperative summary` (e.g., `tools: fix hidden directory filter`).
- Include a concise body for context and rationale when needed; reference issues like `Closes #123`.
- PRs must include: summary, scope (which crates), screenshots/CLI output if UX changes, and notes on testing.
- CI expectations: `cargo fmt`, `cargo clippy -D warnings`, and `cargo test` pass.

## Security & Configuration Tips
- Configuration `.env` is loaded from `~/.config/aj/.env` and project `.env`; never commit secrets.
- Model selection via flags or env: `--model_api`, `--model_url`, `--model_name` (env: `MODEL_API`, `MODEL_URL`, `MODEL_NAME`).
- Avoid writing to repo outside `src/*`; persisted threads are stored under `~/.config/aj/threads/`.
