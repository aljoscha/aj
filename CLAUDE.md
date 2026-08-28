# Working in aj

AJ is a software engineering agent built around a small agent loop and focused
builtin tools. Preserve that simplicity. New behavior should fit the existing
crate boundaries and avoid scaffolding that only works through coordination.

## Architecture

The workspace is split into focused crates under `src/`:

- `aj-models` owns provider adapters, unified messages, streaming types, and
  the model registry.
- `aj-agent` owns the agent runtime, event bus, tool contract, message queues,
  and background task registry.
- `aj-wire` owns remote-control models and compatibility codecs. It contains no
  HTTP or other I/O types.
- `aj-session` owns the on-disk session format, conversation log, and replay.
  User-facing surfaces say "session". A `ConversationLog` contains threads and
  branches internally, so both terms are intentional.
- `aj-tools` owns builtin tool implementations.
- `aj-app` owns frontend-independent application behavior, session composition,
  the turn driver, state reduction, print mode, and settings. It has no TUI
  dependency or direct HTTP transport dependency.
- `aj-conf` owns configuration loading and path helpers.
- `vaxis`, `vaxis-derive`, and `vaxis-ucd` form the terminal UI framework.
- `aj` owns the binary, interactive UI, and wiring between `aj-app` and vaxis.
- `anthropic-sdk` and `openai-sdk` are thin clients used by provider adapters.

Frontends subscribe to the typed `AgentEvent` bus. Persistence is another
subscriber owned with the `ConversationLog` by `aj_app::session::SessionCore`.
The lower-level agent runtime does not own the log.

Keep behavior at its natural boundary. A library exposes a typed error when a
caller branches on failure. Render-only seams use the named opaque `BoxError`.
Frontend-independent application composition in `aj-app` may use `anyhow` when
callers only propagate or display the failure.

## Runtime contracts

Persistent state lives under `~/.aj/`. Secrets come from `.env` and are never
committed. The project `.aj/config.toml` overlays the user
`~/.aj/config.toml`, which overlays builtin defaults. For model selection,
explicit CLI values override their `MODEL_*` environment bindings. Both
override the effective config. Skills are discovered from user and project
`.aj`, `.agents`, and `.claude` skill directories up to the Git root.

## Verification

The current CI gate for review-ready work is:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `cargo build --all-targets`
- `./scripts/check-no-tui-dep.sh`
- `./scripts/check-no-http-dep.sh`
- `./scripts/check-test-scratch.sh`

Scale targeted checks while iterating, but verify the final range against the
guarantees it claims. Important behavior is exercised through the real composed
seam, not only through a helper that constructs one component. Assertions
observe the boundary where harm lands, and fixtures assert the preconditions
that make their measurement meaningful. When practical, break a claimed
guarantee and confirm that the relevant check notices. A surviving break is a
finding about the test or design, not a formality.

Scratch ownership must match task lifetime. Lexical test state uses an owning
guard. Work that may outlive a test uses a process-lifetime root with per-test
subdirectories so late tasks cannot recreate residue after cleanup.

## Commits

Subjects use `<scope>: <lowercase imperative summary>` with no trailing period.
Use the affected crate or area as the scope. A conventional-commit wrapper is
optional when it adds signal.
