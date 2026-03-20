# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build/Test/Lint Commands

- Build: `cargo check` or `cargo build`
- Run all tests: `cargo nextest run`
- Run specific test: `cargo nextest run --package package_name -- test_name`
- Single test with pattern: `cargo nextest run --package package_name -- pattern`
- Format code: `cargo fmt`
- Lint: `cargo clippy`

## Architecture

AJ is an AI-driven agent for software engineering built as a Rust workspace with these core components:

- **aj** (`src/aj/`): Main binary that sets up the agent harness and tools
- **aj-agent** (`src/aj-agent/`): Core agent implementation with conversation loop and Anthropic API integration
- **aj-tools** (`src/aj-tools/`): Framework for builtin tools (currently includes read_file tool)
- **anthropic-sdk** (`src/anthropic-sdk/`): Minimal SDK for Anthropic API with messages and streaming support

The agent follows a minimal agent loop pattern, focusing on providing the right set of builtin tools rather than complex scaffolding. The main entry point creates tools with JSON schemas and passes them to the Agent which manages the conversation loop.

## Code Style

- Rust edition 2024, 4-space tabs.
- Import grouping: std → external crates (including aj_*) → crate imports.
- Use absolute paths for crate imports (`crate::` not `super::`).
- Merge imports from same module, don't merge different modules.
- Error handling: Use `thiserror` for structured errors, not `anyhow!`.
- Follow clippy/rustfmt (enforced with strict workspace lints).
- `snake_case` for functions/variables, `PascalCase` for types/traits, `SCREAMING_SNAKE_CASE` for constants.
- Use proper capitalization and punctuation when writing docstrings or
  comments.

## Rust Compilation

- Always run `cargo fmt` and `cargo build` after making edits before reporting success.
- When refactoring function signatures or types, grep for all call sites and update them in the same pass.
- Check visibility (`pub`) before accessing fields/methods from other modules.
- Run `cargo build` after each logical unit of change. Fix all compilation errors before editing the next file.

## Debugging & Refactoring Approach

- When debugging build or test failures, start by reproducing the exact failing command and reading its output. Do not run generic checks in a shotgun approach.
- When exploring for design or debugging, start producing actionable output (plans, hypotheses, code) early. Don't spend the whole session just reading code.
- If stuck after 3-4 investigation steps without progress, stop and summarize what you've tried and found, then ask for direction.

## Approach

- Code should be simple and clean, well-commented explaining what/how/why.
- Before committing, verify that what you produced is high quality and works.
- When working through a TODO or task list, pick the first unchecked task, implement, verify, check off, commit, then continue with the next.
- Use agent teams when it would speed things up — for example, to explore existing code, research patterns, or implement independent pieces in parallel.

## Commit Style

- Commit style: `area: short, imperative summary` (e.g., `tools: fix hidden directory filter`, `agent: add streaming support`).
- Include a concise body for context and rationale when needed.
