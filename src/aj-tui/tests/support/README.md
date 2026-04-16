# `aj-tui` integration test support

This directory holds shared helpers used by every file under `aj-tui/tests/`.

## Structure

- `tests/support.rs` — top-level module declarations (re-exports and the
  sync `render_now`). Uses `#[path]` to load sibling files from
  `support/`. Cargo compiles every file under `tests/` as a separate
  integration-test binary, so `support.rs` itself produces an empty
  "running 0 tests" line when the suite runs — that's expected.
- `virtual_terminal.rs` — `VirtualTerminal`, a [`Terminal`] implementation
  backed by the [`vt100-ctt`] crate (maintained fork of `vt100`). Writes are
  fed into a VT parser; tests read back `viewport()`, `viewport_trimmed()`,
  `scroll_buffer()`, `cursor()`, `cell(row, col)`, etc. For per-cell style
  assertions, `cell(row, col)` returns an owned `CellInfo` with `bold`,
  `italic`, `underline`, `fg`, `bg`, and wide-char flags — use that instead
  of reaching into the parser directly. For out-of-band state control,
  `clear_viewport` / `reset` wipe terminal state without touching the
  captured writes log, and `resize(cols, rows)` updates dimensions and
  enqueues a `Resize` event so async-driver tests see the notification.
- `logging_terminal.rs` — `LoggingVirtualTerminal` (a naming alias over
  `VirtualTerminal`, which already records every `write()`).
- `ansi.rs` — ANSI-related test helpers: `strip_ansi(s)` removes CSI/OSC
  escape sequences so assertions can compare plain text; `plain_lines`
  and `plain_lines_trim_end` apply it to slices of rendered lines; and
  `visible_index_of(line, needle)` returns the visible-column offset of
  a substring ignoring any preceding styling codes.
- `env.rs` — `with_env(&[(key, value)])` RAII guard for env-mutating tests.
  Pair with `#[serial_test::serial]`.
- `fixtures.rs` — reusable component fixtures. Right now:
  - `StaticLines::new([...])` — renders a fixed list of lines verbatim
    once and never mutates. Prefer for filler content whose shape is
    fixed at construction time.
  - `MutableLines::new()` / `MutableLines::with_lines([...])` — renders
    a list of lines the test can mutate *after* the component has been
    handed to the `Tui`. Backed by `Rc<RefCell<_>>` so cloning the
    handle hands out another reference to the same buffer: install a
    clone into the root container (via `tui.add_child(...)`) and keep
    the original to call `set(...)` /
    `append(...)` / `push(...)` / `clear()` between render passes. Use
    for streaming / incremental-content tests.
  - `StaticOverlay::new([...])` — renders a fixed list of lines and
    records the `width` it was last called with. Use for overlay tests
    that need to assert the compositor sized the overlay correctly
    before rendering it.
  - `InputRecorder::new()` — records every `InputEvent` dispatched to it
    and returns a `(Self, shared log)` pair.
- `themes.rs` — factory functions that construct real `aj_tui` theme types.
  Comes in two flavors: `default_*` (canonical styled output via
  `aj_tui::style`) and `identity_*` (closures that pass strings through
  verbatim, for structural tests that shouldn't have to strip ANSI).
- `async_tui.rs` — async-tier helpers for tests that drive
  `aj_tui::tui::Tui::next_event` directly (see [Async vs sync](#async-vs-sync)
  below).

The non-`mod.rs` layout with `#[path]` attributes on each submodule satisfies
the workspace's `clippy::mod_module_files` lint and keeps all the shared test
code under one `support/` directory.

## API overview

Headless tests drive rendering through a `Tui` that writes into a
`VirtualTerminal`, then read back what landed on the terminal. The
surfaces you'll use most:

| Task                                                | API                                                                          |
| --------------------------------------------------- | ---------------------------------------------------------------------------- |
| Create a virtual terminal                           | `VirtualTerminal::new(cols, rows)`                                           |
| Feed input into a `Tui`                             | Build an `InputEvent` and call `tui.handle_input(&event)`, or batch with `support::send_keys(&mut tui, [events])` |
| Resize the terminal                                 | `vt.resize(cols, rows)` (also enqueues an `InputEvent::Resize`)              |
| Read the current viewport                           | `vt.viewport()` (one string per row), `vt.viewport_text()` (joined by `\n`), or `vt.viewport_trimmed()` (trailing empty rows dropped) |
| Read scrollback + viewport as one buffer            | `vt.scroll_buffer()`                                                         |
| Read cursor position                                | `vt.cursor()` → `(row, col)`                                                 |
| Request a render and wait for it                    | `support::render_now(&mut tui)` / `support::request_and_render_now(&mut tui)` |
| Cell-level style access                             | `vt.cell(row, col)` returns an owned `CellInfo`                              |

Raw escape sequences sent by the `Tui` are captured verbatim: see
`vt.writes()`, `vt.writes_joined()`, `vt.clear_writes()`.

## Behavior notes

A few design choices worth internalizing:

- **`clear_line` emits CSI `2K` (whole line), not CSI `K` (cursor→EOL).**
  The render engine inlines `\x1b[2K` in its diffing path and the
  [`Terminal::clear_line`] contract requires the same, so assertions
  on the writes log should look for `\x1b[2K`. See the doc comment on
  `Terminal::clear_line` in `src/terminal.rs` for the rationale
  (clearing only from the cursor would leave stale bytes to the left
  on a redraw that didn't start at column 0).
- **Cursor coordinates are `(row, col)`, not `(x, y)`.** `vt.cursor()`
  returns `(row_first, col_second)`. Easy to transpose in an
  assertion.

## Deviations from a byte-stream testing model

Earlier shapes of this kind of test harness drove components with raw
byte strings. A handful of concerns changed on the way to idiomatic
Rust + crossterm; assertions ported across from any byte-oriented
source need to account for the following:

- **`clear_line` uses CSI `2K`, not CSI `K`.** Covered above, but
  flagged again here because it's the most common port-time
  surprise.
- **Typed `InputEvent` dispatch, not raw byte streams.**
  Tests build `crossterm::event::KeyEvent` values (or reach for the
  `Key::*` helpers in `aj_tui::keys`) and call `Tui::handle_input`
  with a `&InputEvent`. There is no string-based equivalent of a raw
  byte-stream `sendInput("\x1b[A")` — `ProcessTerminal` relies on
  crossterm to parse stdin into typed events, and tests follow the
  same contract. Multi-event inputs use
  [`support::send_keys`][support::send_keys].
- **`force_full_render` is a separate `Tui` method, not a `force`
  argument on `request_render`.** One conventional shape overloads
  `request_render(force = true)` to clear the diff state and re-emit
  everything on the next render; here we keep `Tui::request_render`
  parameter-free and expose the same semantics through
  `Tui::request_full_render` (and the lower-level
  `Tui::force_full_render`, kept for callers that want to clear diff
  state without also setting the render-request flag).
- **Key release and repeat handling at the crossterm boundary.**
  The `crossterm → InputEvent` conversion preserves `Press`, `Repeat`,
  and `Release` kinds. `Tui::handle_input` filters `Release` events
  out before dispatch unless the target component opts in via
  [`Component::wants_key_release`][wants_key_release]. Repeats always
  flow through. The filter lives on the dispatch path rather than at
  the terminal boundary because crossterm emits the kind directly.

[support::send_keys]: ./support.rs
[wants_key_release]: aj_tui::component::Component::wants_key_release
[`Terminal::clear_line`]: aj_tui::terminal::Terminal::clear_line

For tests that specifically want to assert on the escape sequences
the `Tui` emitted (e.g. "no `\x1b[2J` on resize"), reach for
`LoggingVirtualTerminal` (a naming alias) plus `vt.writes()` /
`vt.writes_joined()` / `vt.clear_writes()`.

## Additions over a byte-stream testing model

The mirror of the "Deviations" section above: a handful of surfaces
this harness ships that aren't part of the byte-stream shape we
started from. None of them are required for a basic test, but
reaching for them keeps assertions short and focused.

- **Per-cell style snapshots.** `vt.cell(row, col)` returns an owned
  `CellInfo` carrying `contents`, `fg`, `bg`, `bold`, `italic`,
  `underline`, `inverse`, `is_wide`, and `is_wide_continuation`. A
  byte-stream harness has to reach into the parser for these; here
  it's an owned value that drops the parser borrow before the
  assertion runs. Pi-tui's `VirtualTerminal` has no equivalent
  helper — pi tests reach into `xterm.buffer.active` directly via
  per-file boilerplate (`getCellItalic`, `getCellUnderline` in
  `markdown.test.ts` / `tui-render.test.ts` / `tui-overlay-style-leak.test.ts`).
  The Rust port consolidates that pattern onto `VirtualTerminal`
  itself so the same five test sites don't have to re-implement the
  reach-through.
- **Writes log built into the terminal.** `vt.writes()`,
  `vt.writes_joined()`, `vt.clear_writes()` are on every
  `VirtualTerminal`, not on a separate `LoggingVirtualTerminal`
  subclass. The `LoggingVirtualTerminal` type alias still exists
  so a test's signature can flag intent ("this test is about
  emitted bytes"), but no behavior is gated on it.
- **Viewport read shapes.** `viewport()` always pads to `rows`,
  `viewport_trimmed()` drops trailing empty rows for diff-friendly
  snapshots, `viewport_text()` joins with `\n`. Pick by what the
  assertion wants to see.
- **Lifecycle observability.** `vt.title()`, `vt.is_cursor_visible()`,
  `vt.is_progress_active()`, `vt.start_count()`, `vt.stop_count()`
  expose the side effects the framework asks for so a test can
  assert "start was called exactly once" / "title was set to X" /
  "progress cleared on shutdown" without scraping the writes log.
- **Per-test capability overrides.** `vt.set_kitty_protocol_active(bool)`
  lets a test exercise the no-Kitty code path without touching
  global state. Defaults to `true` (matching the implicit assumption
  in most component tests); flip it off when testing the fallback.
- **`identity_*` theme variants.** `tests/support/themes.rs` exposes
  both the styled `default_*` themes and matching `identity_*`
  themes whose closures pass strings through verbatim. Use the
  identity flavor for structural tests that don't want to strip
  ANSI from their assertions.
- **`support::send_keys` / `request_and_render_now`.** Batch
  helpers for the two patterns that dominate component-mutation
  tests. `send_keys(&mut tui, [Key::char('h'), Key::char('i'),
  Key::enter()])` replaces three `tui.handle_input(&event)` lines;
  `request_and_render_now(&mut tui)` packs the
  `request_render` + `render` pair into one call when the test
  wants to convey intent without two lines of ceremony.
- **ANSI-aware string helpers.** `support::{strip_ansi, plain_lines,
  plain_lines_trim_end, visible_index_of}` for assertions that
  shouldn't have to know about SGR codes. `visible_index_of`
  returns the visible-column offset of a substring inside a styled
  line — handy for asserting that "→ " lands at the right column
  on a line whose prefix is wrapped in color.
- **Async-driver tier.** `support::async_tui::{channel_tui, advance,
  wait_for_render, drain_ready}` for tests that genuinely care
  about throttle timing or input-stream ordering. Pair with
  `#[tokio::test(start_paused = true)]` and `advance(delta).await`
  so timing is deterministic. The sync tier (`support::render_now`)
  is a one-line `tui.render()`; only graduate to the async tier
  when the test cares about coalescing.
- **Framework self-tests.** `tests/support_smoke.rs`,
  `tests/virtual_terminal_helpers.rs`, and
  `tests/support_framework.rs` guard the support layer itself.
  When `VirtualTerminal::cell()` regresses or
  `fixtures::MutableLines::set` silently swaps semantics, these
  fail first with a focused message — much cheaper to diagnose
  than the cascade of unrelated component-test failures the same
  bug would otherwise produce.

## How to add a new test file

1. Create `tests/<topic>.rs`.
2. Declare `mod support;` at the top of the file.
3. Use `support::VirtualTerminal`, `support::render_now`,
   `support::StaticLines`, etc.

Every integration test file is compiled as its own binary by Cargo, so
`support/` is compiled once per test binary. That overhead is acceptable at
our scale, and the pattern is idiomatic for Rust integration tests.

## Smoke-test coverage of the support framework itself

- `tests/support_smoke.rs` — the bare "plug `VirtualTerminal` into `Tui`,
  render, read viewport" path. If this breaks, every other integration
  test breaks for the same reason; failing early makes the cause obvious.
- `tests/virtual_terminal_helpers.rs` — guards `VirtualTerminal`'s own
  helpers: `clear_viewport`, `reset`, `resize`, the writes log, the
  per-cell `CellInfo`, and `scroll_buffer`.
- `tests/support_framework.rs` — guards `env::with_env`, the
  `StaticLines` / `MutableLines` / `InputRecorder` fixtures, the theme
  factories (both `default_*` and `identity_*`) in `themes.rs`, and
  the `ansi::strip_ansi` / `visible_index_of` helpers. Keeps silent
  field renames or restore-behavior regressions from hiding in the
  component-level tests.

## Historical note

An older in-source `TestTerminal` used to live in `src/terminal.rs`.
It's been retired; `VirtualTerminal` (backed by a real VT parser) is
strictly more accurate for integration tests, and the unit tests that
leaned on `TestTerminal`'s `previous_lines` access were duplicating
coverage that already lived in the viewport-based integration tests.

## Intentional simplifications

A few shapes are deliberately absent:

- **No `tui.start()` / `tui.stop()` ceremony in tests.** The `Terminal`
  trait has default-noop `start` / `stop`, and the sync engine doesn't
  need them. `ProcessTerminal` still overrides them for the real
  stdin/stdout path; headless tests don't.
- **Typed input, not raw bytes.** Tests drive components by calling
  `Tui::handle_input` (or the component's own `handle_input`) with
  pre-parsed [`InputEvent`]s. `VirtualTerminal` does not accept a raw
  byte stream: the only event it enqueues itself is `InputEvent::Resize`
  from `resize()`. Crossterm at the `ProcessTerminal` boundary handles
  byte-level parsing, so tests do not exercise escape-sequence parsing
  through this harness. Raw stdin-buffer / Kitty-protocol /
  cell-size-query byte streams are not testable here and aren't
  planned.
- **`Tui::render()` is unconditional.** There's no throttled-render
  coalescing in the sync engine, so `render_now(&mut tui)` is just
  `tui.render()`. Tests that mutate component state sometimes add a
  preceding `tui.request_render()` for intent, but it is not required.
  `support::request_and_render_now(&mut tui)` wraps both calls
  into one when you want to convey intent concisely. The async-driver
  tier (`support::async_driver`) has its own throttle-aware helper for
  the small set of tests that genuinely care.
- **No `drain_input` surface on the `Terminal` trait.** A real
  terminal sometimes needs to flush pending Kitty-protocol release
  events before restoring itself; `ProcessTerminal` handles that
  through its `Drop` impl, and headless tests don't drive shutdown
  through the trait, so the method is simply absent from `Terminal`.

## Out-of-scope features

The following are explicit non-goals, so no support-layer hooks are
built for them:

- **Terminal images.** No Kitty / iTerm2 graphics protocol support,
  no image cell-dimension probing, no image component.
- **Raw stdin byte-buffer parsing.** Crossterm parses CSI / OSC /
  DCS / APC, bracketed paste, mouse reports, and the Kitty keyboard
  protocol inside `event::read()`, so the crate does not ship a
  hand-rolled stdin state machine.
- **Cell-size and capability probing over stdin.** The protocol
  round-trip used to measure terminal cell dimensions is tied to
  the image pipeline above and is not implemented.

## Why integration tests for pure functions too?

The workspace's `AGENTS.md` default is to keep unit tests inline with
`#[cfg(test)]`. We deviate for this crate's framework tests: every test file
targets one public surface (a component, the renderer, a helper), and keeping
them under `tests/` keeps the `src/` modules focused on implementation.
Private helpers that don't have a public counterpart still use inline unit
tests.

## Async vs sync

The engine itself runs synchronously inside the `Tui`'s `render()` and
`handle_input()` methods — most tests drive those two calls directly, so
`support::render_now(&mut tui)` just invokes `tui.render()`. For the
small set of tests that genuinely care about throttle timing — render
coalescing, input stream ordering — the `support::async_tui` module
pairs with `aj_tui::tui::Tui::next_event` and tokio's paused clock.
Those tests use `#[tokio::test(start_paused = true)]` plus
`advance(delta).await` rather than real sleeps, which keeps them
deterministic and fast.

`async_tui::wait_for_render(&mut tui).await` mirrors pi-tui's
`terminal.waitForRender()` (`packages/tui/test/virtual-terminal.ts:213`):
it pumps `Tui::next_event` until the throttled render fires,
dispatching any input events to the `Tui` along the way, then calls
`tui.render()`. The sync-tier `support::render_now` is a Rust-only
addition (pi has no sync render path); the deliberate name asymmetry
— `render_now()` (no await) vs `wait_for_render().await` — makes the
engine choice obvious at the call site while keeping the async helper
aligned with pi's name. See `tests/async_tui.rs` for examples.

## Fixtures vs. file-local components

`fixtures::StaticLines`, `fixtures::MutableLines`, and
`fixtures::InputRecorder` are the shared component fixtures. Test
files are encouraged to keep their own local components when those
components are tightly coupled to the test's intent (overlay-capturing
test components, components that record a specific render-time
signal). Extract into `fixtures.rs` only once a component has earned a
copy in three or more test files or has reusable shape.

[`Terminal`]: aj_tui::terminal::Terminal
[`vt100-ctt`]: https://docs.rs/vt100-ctt
