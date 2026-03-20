# Session Prompt

Read @TODO.md for the task list. For design context, see @SPEC.md (or the relevant spec file).

Pick the first unchecked task (`- [ ]`) from the TODO. Implement, verify, check off, commit, follow the workflow below.

Use agent teams when it would speed things up — for example, to explore existing code, research patterns, or implement independent pieces in parallel.

## Workflow

* **Read this file again after each context compaction.**
* Code should be simple and clean, well-commented explaining what/how/why.
* Before committing, verify that what you produced is high quality and works.
* Follow the design in the spec closely.

## After each task

1. Verify the code compiles: `cargo build`.
2. Run `cargo fmt`.
3. Run tests for any modified code to verify they still pass.
4. Mark the completed task as done (`- [x]`) in the TODO.
5. Create a commit with a descriptive message (e.g. `agent: add streaming support`).
6. Continue with the next unchecked task.
