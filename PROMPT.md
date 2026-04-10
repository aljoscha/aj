# Session Prompt

The current spec is `docs/models-spec.md` — read it for design context.

Before starting work, check `git log` to understand what has already been implemented. The spec may describe features that are already done. Use the git history (commits, diffs) as the source of truth for current implementation state.

When working on a spec, create or update a tracking file (e.g. `docs/models-progress.md`) that records which parts of the spec have been implemented and which remain. Check off items as you complete them. This file is the bridge between the spec and the git history — it should stay accurate.

Pick the first unimplemented item from the spec and complete one self-contained unit of work. Do not chain multiple items in one session. Implement, verify, update the tracking file, commit, then stop.

Use agent teams when it would speed things up — for example, to explore existing code, research patterns, or implement independent pieces in parallel.

## Workflow

* Code should be simple and clean, well-commented explaining what/how/why.
* Before committing, verify that what you produced is high quality and works.
* Follow the design in the spec closely.

## After each task

1. If you changed code, verify it compiles: `cargo build`.
2. If you changed Rust code, run `cargo fmt`.
3. If you changed code, run tests for any modified code to verify they still pass.
4. Mark the completed item as done in the tracking file.
5. Create a commit with a descriptive message (e.g. `agent: add streaming support`).
6. Stop after this self-contained unit of work. Do not continue to the next task automatically.
