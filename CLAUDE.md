# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build/Test/Lint Commands

- Build: `cargo check` or `cargo build`
- Run all tests: `cargo test`
- Run specific test: `cargo test --package package_name -- test_name`
- Run CLI: `cargo run -p aj -- [args]` (e.g., `list-threads`, `resume <id>`, `resume-latest`)
- Run specific bin: `cargo run -p aj --bin test_diff`
- Format code: `cargo fmt`
- Lint: `cargo clippy`

## Architecture

AJ is an AI-driven agent for software engineering built as a Rust workspace with these core components:

- **aj** (`src/aj/`): Main binary (`src/aj/src/main.rs`) that sets up the agent harness and tools. Extra bins in `src/aj/src/bin/`.
- **aj-agent** (`src/aj-agent/`): Core agent implementation with conversation loop.
- **aj-conf** (`src/aj-conf/`): Configuration, working directory, git root detection, and loading agent/project instructions from AGENT.md files.
- **aj-models** (`src/aj-models/`): Abstraction layer supporting multiple LLM providers (Anthropic, OpenAI) with streaming inference and conversation management.
- **aj-tools** (`src/aj-tools/`): Framework for builtin tools (currently includes read_file tool).
- **aj-ui** (`src/aj-ui/`): UI abstraction trait for displaying agent output, user input, tool results, and token usage.
- **anthropic-sdk** (`src/anthropic-sdk/`): Minimal SDK for the Anthropic API with messages and streaming support.
- **openai-sdk** (`src/openai-sdk/`): Minimal SDK for the OpenAI chat completions API.

The agent follows a minimal agent loop pattern, focusing on providing the right set of builtin tools rather than complex scaffolding. The main entry point creates tools with JSON schemas and passes them to the Agent which manages the conversation loop.

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
- If you changed code, `cargo test` must pass before committing.

## Debugging & Refactoring Approach

- When debugging build or test failures, start by reproducing the exact failing command and reading its output. Do not run generic checks in a shotgun approach.
- When exploring for design or debugging, start producing actionable output (plans, hypotheses, code) early. Don't spend the whole session just reading code.
- If stuck after 3-4 investigation steps without progress, stop and summarize what you've tried and found, then ask for direction.

## Approach

- Code should be simple and clean, well-commented explaining what/how/why.
- Before committing, verify that what you produced is high quality and works.
- When working through a TODO or task list, pick the first unchecked task, complete one self-contained unit of work, implement, verify, check off, commit, then stop. Do not continue automatically to the next task.
- Use agent teams when it would speed things up — for example, to explore existing code, research patterns, or implement independent pieces in parallel.

## Commit Style

- Commit style: `area: short, imperative summary` (e.g., `tools: fix hidden directory filter`, `agent: add streaming support`).
- Include a concise body for context and rationale when needed.

## Configuration & Runtime

- Persistent data lives in `~/.config/aj/` (threads/, history.txt, .env).
- Configuration `.env` is loaded from `~/.config/aj/.env` and project `.env`; never commit secrets.
- Model selection via flags or env: `--model_api`, `--model_url`, `--model_name` (env: `MODEL_API`, `MODEL_URL`, `MODEL_NAME`).
