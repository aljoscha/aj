# Session Prompt

Read @TODO.md for the task list. For design context, see @SPEC.md (or the relevant spec file).

Pick the first unchecked task (`- [ ]`) from the TODO and complete one self-contained unit of work. Do not chain multiple TODO items in one session. Implement, verify, check off, commit, then stop.

Use agent teams when it would speed things up — for example, to explore existing code, research patterns, or implement independent pieces in parallel.

## Workflow

* Code should be simple and clean, well-commented explaining what/how/why.
* Before committing, verify that what you produced is high quality and works.
* Follow the design in the spec closely.

## After each task

1. If you changed code, verify it compiles: `cargo build`.
2. If you changed Rust code, run `cargo fmt`.
3. If you changed code, run tests for any modified code to verify they still pass.
4. Mark the completed task as done (`- [x]`) in the TODO.
5. Create a commit with a descriptive message (e.g. `agent: add streaming support`).
6. Stop after this self-contained unit of work. Do not continue to the next task automatically.
