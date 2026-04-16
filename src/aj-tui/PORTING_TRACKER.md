# `aj-tui` porting sweep — area tracker

Tracker for the per-area porting sweep across `aj-tui`. Each area
is a self-contained unit of work: a sub-agent picks one row, brings
its implementation and tests to parity with the reference at
`/tmp/pi-mono/packages/tui/`, removes stale references in comments
that don't stand on their own, verifies with `cargo fmt` + `cargo
build` + `cargo test`, and commits before handing back.

The "Reference (pi-tui)" column points at the source files in
`/tmp/pi-mono/packages/tui/` so each agent can compare directly.

---

## Areas

| # | Area | Aj source | Aj tests | Reference (pi-tui) | Status |
|---|------|-----------|----------|--------------------|--------|
| 1 | fuzzy | `src/fuzzy.rs` | `tests/fuzzy.rs` | `src/fuzzy.ts`, `test/fuzzy.test.ts` | Done (kept nucleo-matcher wrapper; exact-match bonus retained, alpha-numeric swap retry intentionally dropped) |
| 2 | kill ring + undo stack | `src/kill_ring.rs`, `src/undo_stack.rs` | `tests/kill_ring.rs` | `src/kill-ring.ts`, `src/undo-stack.ts` | Done (behavior at parity; UndoStack relies on Rust ownership in place of `structuredClone`, callers clone before push) |
| 3 | terminal + capabilities | `src/terminal.rs`, `src/capabilities.rs` | `tests/terminal.rs`, `tests/capabilities.rs` | `src/terminal.ts`, capability bits in `src/terminal-image.ts` | Done (crossterm `EventStream` replaces hand-rolled stdin parsing; OSC 9;4 progress keepalive ported with thread + tests) |
| 4 | style helpers | `src/style.rs` | (covered via theme tests) | inline `chalk` calls in pi | Done (aj-only convenience layer; comments stand alone, full ANSI palette kept as documented public surface) |
| 5 | truncated text + spacer + text | `src/components/{truncated_text,spacer,text}.rs` | `tests/{truncated_text,text}.rs` | `src/components/{truncated-text,spacer,text}.ts`, `test/truncated-text.test.ts` | Done (all 9 truncated-text reference cases mirrored; comment sweep across the three leaf components and their tests) |
| 6 | text box (renamed from Box) | `src/components/text_box.rs` | `tests/text_box.rs` | `src/components/box.ts` | Done (comment sweep; behavior already at parity, all tests cover real reference behaviors) |
| 7 | text input (renamed from Input) | `src/components/text_input.rs` | `tests/text_input.rs`, `tests/text_input_newline_fallback.rs` | `src/components/input.ts`, `test/input.test.ts` | Done (renamed `Input` → `TextInput` to match `TextBox`; comment sweep across source and both test files; intentional divergences on focused-only cursor render, two-way scroll branch, and Alt+Enter submit retained) |
| 8 | ansi/utils + word_wrap + word_boundary | `src/ansi.rs`, `src/word_wrap.rs`, `src/word_boundary.rs` | `tests/{wrap_ansi,word_wrap,truncate_to_width,regression_regional_indicator_width}.rs` | `src/utils.ts`, `test/{wrap-ansi,truncate-to-width,regression-regional-indicator-width}.test.ts` | Done (comment sweep across `ansi.rs`; behavior already at parity, intentional divergences \u2014 no width cache, no pooled style tracker, stricter CSI scanner, stricter whitespace/punctuation grapheme rule \u2014 retained and documented in `PORTING.md`) |
| 9 | keys | `src/keys.rs` | `tests/keys.rs` | `src/keys.ts`, `test/keys.test.ts` | Done (comment sweep; descriptor-side parity audited) |
| 10 | keybindings | `src/keybindings.rs` | `tests/keybindings.rs`, `tests/keybindings_user_overrides.rs` | `src/keybindings.ts`, `test/keybindings.test.ts` | Done (parity audit; defaults 1:1 with the reference plus the approved `ctrl+p`/`ctrl+n` aliases on `tui.select.{up,down}`; user-override edges and process-scoped accessor pinned by extra tests; `KeybindingsConfig` JSON serde intentionally not in this crate) |
| 11 | autocomplete | `src/autocomplete.rs` | `tests/autocomplete.rs`, `tests/autocomplete_session.rs` | `src/autocomplete.ts`, `test/autocomplete.test.ts` | Done (parity-audited; `ignore::WalkBuilder` + `nucleo` are the deliberate idiomatic-Rust choices, scoped fuzzy walk handles out-of-tree prefixes, streaming `FuzzyFileSession` bails to `SessionInvalid` for those so the editor's one-shot fallback covers them) |
| 12 | loader + cancellable loader | `src/components/{loader,cancellable_loader}.rs` | `tests/{loader,cancellable_loader}.rs` | `src/components/{loader,cancellable-loader}.ts` | Done (comment sweep across source and tests; behavior already at parity, intentional divergences \u2014 explicit `verbatim` flag on `LoaderIndicatorOptions`, `invalidate` forwarded through both loaders, `tokio_util::CancellationToken` in place of `AbortController` \u2014 retained) |
| 13 | select list | `src/components/select_list.rs` | `tests/select_list.rs` | `src/components/select-list.ts`, `test/select-list.test.ts` | Done (comment sweep; wired `on_selection_change` to fire on every selection move; `normalize_to_single_line` no longer collapses tabs; intentional divergences — styled `selected_prefix` field, `pageUp`/`pageDown` keybindings, `items()` accessor — retained) |
| 14 | settings list | `src/components/settings_list.rs` | `tests/settings_list.rs` | `src/components/settings-list.ts` | Done (comment sweep; behavior already at parity, intentional divergences — `"> "` search prompt for gutter alignment, raw event forwarding to search input, unconditional filter recompute, `invalidate` forwarded to active submenu — retained) |
| 15 | markdown | `src/components/markdown.rs` | `tests/markdown.rs` | `src/components/markdown.ts`, `test/markdown.test.ts` | Done (comment sweep across source and tests; behavior already at parity, intentional divergences — trailing-blank trim, syntect-backed highlighting, `unicode-segmentation`/`unicode-width` math, OSC 8 gating read inline at the link-render site — retained) |
| 16 | editor component trait | `src/editor_component.rs` | inline tests in `src/editor_component.rs`, `tests/editor_border.rs` | `src/editor-component.ts` | Done |
| 17 | editor | `src/components/editor.rs` | `tests/editor_*.rs` (≈20 files) | `src/components/editor.ts`, `test/editor.test.ts` | Done — comment sweep across editor.rs and the per-feature test files. |
| 18 | tui core + container + component | `src/tui.rs`, `src/container.rs`, `src/component.rs` | `tests/{tui_render,tui_stop,overlay_*,input_listeners,component_wants_key_release,on_debug,write_log,virtual_terminal_helpers,support*}.rs` | `src/tui.ts`, `test/{tui-render,overlay-*,tui-overlay-style-leak}.test.ts` | Done (comment sweep across `tui.rs`, `container.rs`, `lib.rs`, and the `tests/support/` framework; documented divergences — bottom-up cursor-marker scan, no cell-size response branch, all-hidden overlay short-circuit, collapsed `\r\n` down-move, `pending_full_clear` flag, `Tui` container forwarding methods — retained with stand-alone explanations) |

## Intentionally not ported

These rows have no actionable work; they're listed so a future
reader doesn't re-investigate.

| Reference (pi-tui) | Reason |
|--------------------|--------|
| `src/stdin-buffer.ts`, `test/stdin-buffer.test.ts` (62 cases) | Crossterm's `EventStream` already frames CSI / OSC / DCS bytes into typed `Event` values. The buffering layer would be redundant. See `src/lib.rs`. |
| `src/terminal-image.ts`, `src/components/image.ts`, `test/terminal-image.test.ts`, `test/bug-regression-isimageline-startswith-bug.test.ts`, `test/tui-cell-size-input.test.ts`, `test/image-test.ts` | Image rendering deferred until an aj surface needs it. Capability detection is split out into `src/capabilities.rs`. See `src/lib.rs`. |

## Per-agent workflow

Each agent owns one row end-to-end:

1. Read the reference and the aj counterpart side by side. Verify the
   correlation columns are accurate; flag the human if files have
   moved.
2. Bring implementation, features, and tests to parity. Idiomatic Rust
   is fine; superfluous tests / cruft go. New behavior or test cases
   need a justification.
3. Sweep comments, doc-comments, and identifiers in the area for
   references that only make sense if you've read pi-tui. Each line
   should stand on its own. No mention of "pi", "pi-tui", "upstream",
   "original framework", "reference implementation", or links to the
   pi-mono repo. Keep technical notes that explain *what* the code
   does and *why*; remove the ones that explain it by pointing at
   another codebase.
4. `cargo fmt` + `cargo build -p aj-tui --all-targets` + `cargo test
   -p aj-tui` (or a tighter subset for the area). Fix any failures
   before committing.
5. Commit per logical change with descriptive messages (subsystem
   prefix, e.g. `fuzzy: …`). Multiple small commits are fine.
6. Tick the area off in this tracker (status `Done`) in the same
   commit(s).
