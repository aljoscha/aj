# AJ Agent Guide

AJ is an AI-driven agent for software engineering. It is pair programming with
the user to architect and develop software.

## Build/Test/Lint Commands

- Build: `cargo check` or `cargo build`
- Run all tests: `cargo nextest run`
- Run specific test: `cargo nextest run --package package_name -- test_name`
- Single test with pattern: `cargo nextest run --package package_name -- pattern`
- Format code: `cargo fmt`
- Lint: `cargo clippy`

## Code Style Guidelines

- Use Rust edition 2024, 4-space tabs
- Import grouping: std → external crates (including aj_*) → crate imports
- Use absolute paths for crate imports (`crate::` not `super::`)
- Merge imports from same module, don't merge different modules
- Error handling: Use `thiserror` for structured errors, not `anyhow!`
- Follow clippy/rustfmt (enforced in CI)
- Use proper capitalization and punctuation when writing docstrings or
  comments.

## Architecture

AJ is an AI-driven agent for software engineering built as a Rust workspace with these core components:

- **aj** (`src/aj/`): Main binary that sets up the agent harness and tools
- **aj-agent** (`src/aj-agent/`): Core agent implementation with conversation loop and Anthropic API integration
- **aj-tools** (`src/aj-tools/`): Framework for builtin tools (currently includes read_file tool)
- **anthropic-sdk** (`src/anthropic-sdk/`): Minimal SDK for Anthropic API with messages and streaming support

The agent follows a minimal agent loop pattern, focusing on providing the right set of builtin tools rather than complex scaffolding. The main entry point creates tools with JSON schemas and passes them to the Agent which manages the conversation loop.
