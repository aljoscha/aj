# Module audit plan — boundaries, simplicity, abstractions, tests

A structured, resumable review of every crate in the workspace against
our engineering guidance: simplicity, clean boundaries and abstractions,
testing at the boundaries, well-documented contracts, and disciplined
comments and error handling.

This document is the **stable specification**: the rubric, the report
format, the per-step procedure, and the ordered list of audit targets.
Mutable state (what's done, findings links, counts) lives in the
companion `docs/audit/audit-progress.md`.

The driving loop, in any session, is:

> follow `@docs/audit/audit-progress.md` and do the next step

## Operating mode

- **Read-only.** An audit step produces a findings report. It does **not**
  change source code, tests, or behavior. Fixes are separate follow-up
  work that the user greenlights after reviewing findings. This matches
  our guidance: keep changes minimal, call out deviations, decide
  together.
- **One self-contained step per unit of work.** A step audits one scoped
  set of files, writes one findings report, updates progress, and commits.
- **Findings are actionable.** Each finding is specific (file:line),
  classified, and carries a suggested action and rough effort so it can
  later be promoted into its own task.

## Orchestration (how the driver runs a step)

The main agent acts as the driver and delegates the analysis of each step
to a **read-only sub-agent**. Per step:

1. Read `audit-progress.md`; pick the first step whose status is `TODO`.
2. Record the commit being audited: `git rev-parse --short HEAD`.
3. Spawn one sub-agent with a task that:
   - points it at this plan (`docs/audit/audit-plan.md`) for the rubric,
     severity taxonomy, and report template;
   - lists the exact files in the step's scope (including their tests);
   - instructs it to write its report to
     `docs/audit/findings/<unit>.md` using the template, and to return a
     short summary with severity counts.
4. Read the written findings file and sanity-check its quality.
5. Update `audit-progress.md`: flip the step to `Done`, fill in severity
   counts, the findings link, the date, and the audited commit.
6. Commit (`docs(audit): <unit> findings`).
7. In incremental mode, stop. In driver mode, continue to the next step.

Delegating the *analysis* fits our sub-agent guidance (search, explore,
understand how a system works). Writing the plan itself and the
synthesis step are done by the main agent directly.

## Rubric

Every step assesses the scoped code against all of the dimensions below.
Not every dimension yields findings every time; absence of findings on a
dimension is itself a useful signal (note it under "What's good").

### 1. Boundaries & abstractions

- Does the crate's dependency direction match the intended graph in
  `CLAUDE.md`? Flag any edge that points the wrong way or that the graph
  doesn't mention.
- Are module and crate boundaries cohesive? Does each module have a clear
  single responsibility, or is it a grab-bag?
- Is the public surface minimal? Flag `pub` items that could be
  `pub(crate)` or private. Flag types/fields leaking across a layer they
  shouldn't (e.g. wire types bleeding into UI, persistence into runtime).
- Are traits/interfaces well-designed: cohesive, minimal, at the right
  level of abstraction? Flag traits with one impl that add no seam, or
  god-traits doing too much.
- Are there leaky abstractions — callers forced to know internals,
  invariants enforced by convention rather than types?

### 2. Simplicity

- Unnecessary complexity, over-engineering, premature abstraction,
  needless indirection or generality.
- Duplication (logic that should be shared) and its inverse (forced
  sharing that couples unrelated things).
- Dead code: unused items, unreachable branches, unused parameters,
  vestigial config.
- Functions/types that are too large or do too many things.

### 3. Contracts & assumptions

- Are invariants, preconditions, and postconditions documented where
  they aren't obvious from types?
- Are panics, `unwrap`/`expect`, and `unreachable!` justified and noted?
- Are error conditions and edge-case behavior specified at the API
  boundary?

### 4. Comments & documentation

- No "fluff" comments that restate what the code plainly does.
- Tricky parts and non-obvious decisions are called out (the "nota bene"
  cases).
- Comments are concise but complete; contracts are clearly described.
- No chronology: no "previously", "used to", "in a future PR" framing.
  Comments must stand on their own. (Recording *why* something behaves a
  certain way is fine; narrating change history is not.)
- No references to other projects we drew inspiration from.
- Module-level docs exist where they aid navigation.

### 5. Error handling

- Library crates define error types with `thiserror`; `anyhow` is only
  acceptable at the top-level application (`aj`).
- Errors carry enough context to diagnose; no silently swallowed errors.
- No `unwrap`/`expect`/`panic!` on reachable paths in library code
  without a documented justification.

### 6. Testing at the boundaries

This dimension assesses **quality and placement**, not count.

- Do tests exercise the public contract / boundary, or do they reach into
  internals and ossify implementation details?
- Are the right things covered: edge cases, error paths, boundary values
  — not just the happy path?
- Are tests at the right layer (unit in-module vs. integration in
  `tests/`)? Per `CLAUDE.md`: unit tests in-module under `#[cfg(test)]`,
  integration tests in `<crate>/tests/`.
- Quality of fixtures and helpers: shared, readable, not brittle.
- Hidden coupling to wall-clock time, real network, real filesystem, or
  ordering that invites flakiness.
- Gaps: untested public items or contracts that matter.

### 7. Naming & style conventions

- `snake_case` functions/vars, `PascalCase` types/traits,
  `SCREAMING_SNAKE_CASE` consts.
- Import grouping: std → external crates (incl. `aj_*`) → `crate::`
  imports; absolute `crate::` paths (not `super::`); merged imports from
  the same module, not across modules.
- Rust edition 2024; rustfmt-clean; clippy-clean under the workspace
  lints.

### 8. Dependency hygiene

- Dependencies come from `[workspace.dependencies]` where shared; no
  unused dependencies; no needless heavyweight or duplicate deps.
- Feature flags are minimal and intentional.

### Opportunistic checks

Not primary dimensions, but flag if noticed:

- `unsafe` blocks without a safety comment or clear justification.
- Obvious performance footguns on hot paths (needless clones/allocs in
  render or streaming loops).
- Secrets handling: API keys/tokens logged, persisted, or printed.

## Severity taxonomy

- **Critical** — correctness/safety/data-loss risk, secret leak, or an
  architecture-level boundary violation (e.g. a dependency cycle or a
  layer reaching across the graph) that undermines the design.
- **Major** — significant design smell: wrong or missing abstraction, a
  key boundary with no test coverage, error handling that will bite,
  substantial avoidable complexity.
- **Minor** — localized smell: a missing contract doc, a test gap on an
  edge case, a small duplication.
- **Nit** — style, naming, fluff comment, import ordering.

## Categories

Tag each finding with one primary category (matching the rubric):
`Boundaries`, `Simplicity`, `Contracts`, `Comments`, `Errors`, `Testing`,
`Style`, `Dependencies`, `Misc`.

## Findings report format

Copy `docs/audit/findings/_TEMPLATE.md` to
`docs/audit/findings/<unit>.md` and fill it in. The `<unit>` matches the
step's findings filename in the progress file (e.g. `aj-models-core`).

Each finding uses the form:

```
### [SEVERITY][Category] Short title — `path/to/file.rs:line`
**What:** the issue, concretely.
**Why it matters:** the consequence (for boundaries/simplicity/etc.).
**Suggested action:** what a fix would do.
**Effort:** S | M | L
```

## Ordered audit targets

Steps are grouped by phase and ordered leaf-first along the dependency
graph so boundaries are reviewed from the bottom up. Each step lists its
file scope (paths relative to the crate's `src/` unless noted). Tests for
the scoped code are always in scope for the Testing dimension.

The canonical checkable state for these steps lives in
`audit-progress.md`; this list is their stable definition.

### Phase S — provider SDK clients

- **S1 · `anthropic-sdk`** — `client.rs`, `messages.rs`, `stealth.rs`,
  `lib.rs`.
- **S2 · `openai-sdk`** — `client.rs`, `types.rs`,
  `types/chat_completions.rs`, `types/common.rs`, `types/responses.rs`,
  `lib.rs`.

### Phase M — `aj-models` (wire layer)

- **M1 · models-core** — `lib.rs`, `provider.rs`, `registry.rs`,
  `refresh.rs`, `types.rs`, `errors.rs`, `tools.rs`.
- **M2 · models-streaming** — `streaming.rs`, `transform.rs`,
  `partial_json.rs`.
- **M3 · models-anthropic** — `anthropic.rs`, `anthropic/provider.rs`
  (+ `tests/roundtrip/anthropic.rs`).
- **M4 · models-openai** — `openai.rs`, `openai/provider.rs`,
  `openai/responses.rs`, `openai/codex.rs` (+
  `tests/roundtrip/openai_*.rs`).
- **M5 · models-auth** — `auth.rs`, `oauth.rs`, `oauth/anthropic.rs`,
  `oauth/openai.rs`, `oauth/page.rs`, `oauth/pkce.rs`, `scripted.rs`,
  `scripted/demos.rs`.

### Phase C — `aj-conf`

- **C1 · conf** — `lib.rs`.

### Phase AG — `aj-agent` (runtime + contracts)

- **AG1 · agent-runtime** — `lib.rs`, `bus.rs`, `events.rs`,
  `projection.rs`, `hooks.rs`.
- **AG2 · agent-contracts** — `tool.rs`, `message.rs`, `types.rs`.

### Phase SE — `aj-session`

- **SE1 · session** — `lib.rs`, `log.rs`, `persistence.rs`, `listener.rs`,
  `replay.rs`, `repair.rs`.

### Phase TO — `aj-tools`

- **TO1 · tools-framework** — `lib.rs`, `tools.rs`, `truncate.rs`,
  `sanitize.rs`, `image.rs`, `testing.rs`.
- **TO2 · tools-impls** — `tools/agent.rs`, `tools/bash.rs`,
  `tools/read_file.rs`, `tools/write_file.rs`, `tools/edit_file.rs`,
  `tools/edit_file_multi.rs`, `tools/todo.rs`.

### Phase T — `aj-tui`

- **T1 · tui-core** — `lib.rs`, `component.rs`, `container.rs`, `tui.rs`,
  `terminal.rs`, `capabilities.rs`.
- **T2 · tui-text** — `ansi.rs`, `word_wrap.rs`, `word_boundary.rs`,
  `style.rs`, `fuzzy.rs`.
- **T3 · tui-editor** — `editor_component.rs`, `components/editor.rs`,
  `components/text_input.rs`, `components/text_box.rs`, `kill_ring.rs`,
  `undo_stack.rs`, `keybindings.rs`, `keys.rs`, `autocomplete.rs`.
- **T4 · tui-components** — `components.rs`, `components/markdown.rs`,
  `components/image.rs`, `image_protocol.rs`, `components/loader.rs`,
  `components/cancellable_loader.rs`, `components/select_list.rs`,
  `components/settings_list.rs`, `components/overlay_window.rs`,
  `components/spacer.rs`, `components/text.rs`,
  `components/truncated_text.rs`.
- **T5 · tui-tests** — the `tests/` suite (assess boundary coverage,
  the `tests/support/` harness, and overall test design).

### Phase A — `aj` (binary)

- **A1 · aj-cli** — `cli.rs`, `cli/args.rs`, `cli/file_args.rs`,
  `config.rs`, `config/keybindings.rs`, `config/slash_commands.rs`,
  `config/theme.rs`, `model.rs`, `auth.rs`, `clipboard.rs`.
- **A2 · aj-core** — `lib.rs`, `main.rs`, `modes.rs`, `modes/print.rs`,
  `persistence.rs`, `scripted.rs`.
- **A3 · aj-interactive** — `modes/interactive.rs`,
  `modes/interactive/event_pump.rs`, `modes/interactive/layout.rs`,
  `modes/interactive/keys.rs`, `modes/interactive/editor_ext.rs`,
  `modes/interactive/footer_data.rs`, `modes/interactive/shutdown.rs`.
- **A4 · aj-components** — `modes/interactive/components.rs` and every
  file under `modes/interactive/components/`.
- **A5 · aj-tests** — `tests/replay_parity.rs` and binary-level boundary
  observations.

### Phase X — synthesis

- **X1 · cross-crate synthesis** — done by the main agent directly. Read
  all findings reports plus the workspace manifests and the dependency
  graph in `CLAUDE.md`. Verify the real dependency edges match the
  intended graph, collate recurring cross-cutting themes, and produce a
  prioritized top-level summary in `docs/audit/findings/_SUMMARY.md`.
