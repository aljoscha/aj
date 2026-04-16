# `aj-tui` roadmap

Status and remaining work on the `aj-tui` crate. Items marked `[x]`
have landed with their regression tests; `[~]` are partial and
explain why; `[ ]` are open.

Ordered by value: render-engine correctness first (the part with the
hardest-earned lessons), then framework/component gaps, then
integration into the agent.

---

## Phase A: Render-engine parity (complete)

A side-by-side read of `src/tui.rs` surfaced a set of real
behavioral gaps. Each item below landed with a regression test.

### A1. "All deletions" shrink branch

- [x] Pure-deletion cleanup uses CUU/CUD + `\r\x1b[2K` (no `\r\n`),
      plus the `target_row < previous_viewport_top` full-redraw
      fallback. Caps the main-loop `render_end` at `lines.len() - 1`
      and uses the same non-scrolling cleanup for mixed
      change-plus-trailing-deletion frames.
- [x] Regression tests (`shrink_at_viewport_bottom_does_not_scroll_kept_content_off_screen`,
      `shrink_with_scrolled_content_falls_back_to_full_redraw`,
      `partial_change_with_trailing_deletions_preserves_earlier_rows`,
      `pure_deletion_whose_target_lies_above_viewport_falls_back_to_full_redraw`).

### A2. `force_full_render` must clear on next render

- [x] `pending_full_clear: bool` flag set by `force_full_render` /
      `request_full_render`; consumed on the next render so the
      full-clear branch fires even when `first_render` is also true.
- [x] Regression tests (`force_full_render_clears_screen_on_next_render`,
      `first_render_does_not_clear_so_pre_tui_shell_output_is_preserved`).

### A3. `apply_line_resets` uses `SEGMENT_RESET`, not `SGR_RESET`

- [x] Terminate non-empty lines with `SEGMENT_RESET` (SGR reset +
      OSC 8 empty URL) so an unclosed hyperlink doesn't bleed into
      downstream rows.
- [x] Regression test (`line_reset_closes_open_hyperlink_on_each_row`).

### A4. `composite_overlays` missing viewport-start offset

- [x] Two-pass compositing pads the line buffer up to
      `working_height = max(lines.len(), term_height,
      min_lines_needed)` and composites each overlay at
      `viewport_start + row + i`. Short-circuits when the overlay
      stack is empty so the terminal-height padding doesn't leak
      into the post-composite shrink detector.
- [x] Regression test
      (`overlay_renders_at_viewport_top_when_base_content_exceeds_terminal_height`).

### A5. Overlay boundary slice + final width clamp

- [x] `slice_with_width` / `slice_by_column` take a `strict: bool`
      parameter. `composite_line_at` passes `strict = true` for the
      overlay payload and runs a post-composition width clamp via
      `slice_by_column(&result, 0, total_width, true)`.
- [x] Regression test (`composite_drops_wide_char_that_would_straddle_overlay_boundary`).

### A6. Height-change diff path must recompute `previous_viewport_top`

- [x] Local `effective_viewport_top = max(0, prev_viewport_top +
      prev_height - new_height)` at the top of render, threaded into
      the `diff_above_viewport` / `deletion_only_needs_full` checks
      and written back to `self.previous_viewport_top` before
      `differential_render` runs.
- [x] Regression test
      (`termux_height_shrink_reaches_full_redraw_when_old_top_would_have_hidden_it`).

### A7. `show_hardware_cursor`: state vs preference

- [x] Split into `hardware_cursor_enabled: bool` (preference,
      default `true`) and `hardware_cursor_currently_shown: bool`
      (state). Public API: `hardware_cursor_enabled()` /
      `set_hardware_cursor_enabled(bool)`,
      `hardware_cursor_currently_shown()` /
      `set_hardware_cursor_currently_shown(bool)`. Old accessors
      kept as `#[deprecated]` aliases. `Tui::start()` emits the
      baseline cursor-hide.
- [x] Regression test
      (`hardware_cursor_enabled_preference_suppresses_cursor_even_with_marker`).

### A8. Scrolling preamble audit

- [x] The engine collapses the explicit scroll-into-viewport
      preamble (CUD-to-bottom + `\r\n.repeat(scroll)`) into
      unconditional `\r\n`-per-step for down moves. A stress test
      (`render_tracker_survives_grow_shrink_grow_sequence`) walks
      six grow/shrink/grow transitions on a small terminal and
      asserts tail-row visibility after each step plus a final
      single-row in-viewport edit.

### A9. `cursor_row` vs `hardware_cursor_row`

- [x] Dropped the unused `cursor_row` field. It was write-only:
      nothing reads end-of-content separately from physical cursor
      row. If a future change needs to track them apart, we'll
      reintroduce the distinction then.

### A10. Line-width overflow crash format

- [x] No code change required; the Rust crate's phase-4.5 strict
      check validates every line before any strategy runs — we fail
      before emitting a bad frame rather than during it. Documented
      in the render method.

---

## Phase A': Render-engine tail

Four small additions that came out of a second pass against
`doRender`. None block anything else.

### A'-1. `extract_segments` `strict_after` parameter

- [x] `ansi::extract_segments` takes a `strict_after: bool`. When
      `true`, a grapheme whose right half would extend past
      `after_end` is dropped from the `after` segment.
      `composite_line_at` passes `strict_after = true`.
- [x] Unit tests in `ansi.rs`
      (`test_extract_segments_strict_after_drops_wide_grapheme_at_boundary`).
      End-to-end behavior is unchanged because
      `composite_line_at`'s post-composition `slice_by_column` clamp
      catches any overshoot; `strict_after` is a library contract
      and performance fix.

### A'-2. `composite_line_at` after-segment padding

- [x] Right-pad composited lines so the visible width reaches
      `total_width` rather than stopping after the `after` segment.
      The `actual_before_width` / `actual_overlay_width` max-based
      sizing accounts for wide-grapheme overshoot in the
      before/overlay segments.
- [x] Regression test (`overlay_composite_pads_right_to_terminal_width`).

### A'-3. `append_start` shortcut in `differential_render`

- [x] When the new frame strictly appends rows (`first ==
      previous_lines.len() && first > 0`), move to `first - 1` and
      emit `\r\n` instead of `\r` after the move. Avoids the
      redundant `\r` the collapsed `\r\n`-for-down path would
      otherwise emit; byte-trims append-heavy workloads (streaming
      LLM output).
- [x] Regression test
      (`pure_append_diff_emits_append_start_shortcut_not_redundant_carriage_return`).

### A'-4. `total_renders()` counter

- [x] `total_renders: u64` bumped on every `Tui::render()` call
      that reaches the paint phase (non-zero terminal dimensions).
      Public accessor `Tui::total_renders()`. `full_redraws()` stays
      as the narrower "how many full redraws happened" counter.
- [x] Regression test (`total_renders_counts_one_per_coalesced_burst`).

---

## Phase B: Framework and component gaps

Closed except two partial items. Four items completed, one
investigated-and-skipped, one compile-verified-but-needs-TTY-smoke.

- [~] `keys.rs`: `key_id_matches` respects the Kitty base-layout key
      for non-Latin layouts (Cyrillic Ctrl+С → `ctrl+c`). **Blocked
      on crossterm.** crossterm 0.28/0.29 parse the Kitty CSI-u
      `base:shifted:base-layout` three-field form but discard the
      third (base-layout) field. Without an exposed `KeyEvent` field
      (or a separate event variant) carrying the base-layout
      codepoint, `key_id_matches` has no signal to disambiguate on.
      Revisit when crossterm grows the surface, or when we graduate
      to a CSI-u fork that exposes it.
- [x] `overlay_non_capturing.rs`:
  - [x] Microtask/async-deferred sub-overlay creation inside a
        `done` callback restores focus.
        (`microtask_deferred_sub_overlay_pattern_restores_focus_on_teardown`)
  - [x] Capturing overlay hide → show-again renders on top.
        (`capturing_overlay_hidden_and_shown_again_renders_on_top`)
        Required a behavior change in `Tui::set_overlay_hidden`:
        unhiding a capturing overlay now bumps its focus_order to
        the top of the stack and transfers focus to it. Non-
        capturing overlays unhide without any focus or z-order
        change — the previous "no auto-refocus" contract stays.
  - [x] Focused-overlay-becomes-invisible with non-capturing
        overlays in the stack skips them and restores pre-focus.
        (`invisible_focused_overlay_heals_focus_to_topmost_visible_capturing_on_input`)
        Required a new "focus heal" step at the top of
        `handle_input_after_listeners`. Also factored out a shared
        `overlay_is_visible` helper used by input routing, focus
        healing, and compositing.
- [x] Pub-export `is_whitespace_grapheme` / `is_punctuation_grapheme`
      from `ansi` for downstream consumers. Consolidated from
      duplicate `word_wrap.rs` / `editor.rs` private copies.
- [x] `format_key_descriptor(&InputEvent) -> Option<String>` —
      given a typed `InputEvent`, produce the canonical descriptor
      string that `key_id_matches` accepts. Fixed
      `shift+ctrl+alt+super+<key>` ordering. Char events fold shift
      into case (`"a"` not `"shift+a"`); named keys preserve shift
      (`"shift+enter"`).
- [~] Manual example TTY smoke + `examples/PARITY.md` update.
      Compile check on all four examples (`chat_simple`,
      `key_tester`, `settings_demo`, `viewport_overwrite_repro`)
      passes. PARITY.md has a "Verification status" section noting
      the compile-only confirmation and how to run each example for
      a real-TTY smoke. Marked partial because interactive TTY
      verification on at least one supported terminal (Kitty /
      Ghostty / iTerm2 / WezTerm / Alacritty / VS Code) needs to
      happen before integration.

---

## Phase B': Small parity items (complete)

Lightweight deltas surfaced during the Phase B cleanup pass.

### B'-1. OSC 8 hyperlink state in `AnsiStyleTracker`

- [x] `AnsiStyleTracker` tracks `active_hyperlink: Option<String>`
      alongside SGR attributes.
- [x] `process()` recognizes `\x1b]8;;URL(\x07|\x1b\\)` (open) and
      both close forms.
- [x] `reset()` (SGR reset) leaves the active hyperlink in place
      (matches terminal semantics — an SGR reset does not affect
      OSC 8). `clear()` fully resets, including the hyperlink.
- [x] `get_active_codes()` appends `\x1b]8;;{url}\x1b\\` when a
      hyperlink is active.
- [x] `get_line_end_reset()` appends `\x1b]8;;\x1b\\` when a
      hyperlink is active (paired with the existing underline-off).
- [x] Un-`ignore` the two OSC 8 wrap tests in `tests/wrap_ansi.rs`.

### B'-2. `TextBox` render cache

- [x] Cache `(child_lines, width, bg_sample) → lines` so a frame
      that re-renders a container whose children and width are
      unchanged doesn't rebuild padded/bg-applied lines. Cache is
      invalidated on child mutation and `invalidate()`.

### B'-3. `on_debug` hook

- [x] `Tui::set_on_debug(FnMut)` registers a global callback fired
      before input routing when `Shift+Ctrl+D` arrives. Useful for
      apps that want a dev-time hook without running an input
      listener.

### B'-4. CSI extraction strictness

- [x] `extract_ansi_code` CSI path terminates on any final byte in
      `0x40..=0x7E` *that is a recognized CSI final* (`m G K H J
      A B C D d T`). Unrecognized final bytes return `None` (the
      caller treats the `ESC` as literal) rather than silently
      consuming an unknown CSI whose shape we can't validate.

### B'-5. Panic message wording

- [x] Width-overflow panic drops the duplicated `"aj-tui:"` prefix
      and reads as a single coherent sentence.

---

## Phase E: Render-engine corrections (complete)

A side-by-side review of the render pipeline surfaced four small
but observable deltas. Each landed with a regression test.

### E1. Overlay layout bottom/right clamp

- [x] `resolve_overlay_layout` now applies a final
      `row.clamp(margin.top, term_height - margin.bottom -
      effective_height)` (and the col equivalent), so an explicit
      `row` past the terminal bottom or a positive `offset_y` that
      pushes the overlay off-screen is brought back inside the
      visible viewport instead of dropping rows during composite.
      Percentage row/col semantics also moved to the position-aware
      formula (`50%` centers the overlay vertically rather than
      putting its top edge halfway down the terminal).
- [x] Regression tests in `tests/overlay_options.rs`
      (`absolute_row_past_bottom_clamps_overlay_so_it_fits`,
      `absolute_col_past_right_clamps_overlay_so_it_fits`,
      `offset_y_past_bottom_clamps_overlay_so_it_fits`,
      `bottom_margin_pushes_overlay_above_its_extent`).

### E2. `position_hardware_cursor` row clamp

- [x] `position_hardware_cursor` now actually uses its `lines`
      argument: `row.min(lines.len() - 1)` (or 0 when `lines` is
      empty) so a future caller passing an OOB row can't emit
      `\r\n`s past the end of content and scroll rows into
      scrollback.
- [x] No regression test: `extract_cursor_position` already
      guarantees `row < lines.len()` for every current caller, so
      the clamp is defensive insurance documented in the function's
      doc comment.

### E3. Phase 4 line-reset terminator on empty lines

- [x] Phase 4 now appends `SEGMENT_RESET` to every rendered line,
      including empty ones. Previously a row produced as `""` by a
      component sat between two styled rows with no terminator of
      its own; on a degraded sync-mode setup an in-progress style
      on the terminal (not in our line buffer) could bleed through
      into the empty row's cells until something else reset.
- [x] Regression test (`line_reset_terminates_empty_lines_too`).

### E4. `extract_cursor_position` scan direction (deferred)

- [ ] Our `extract_cursor_position` scans top-down within the
      visible viewport; an alternative is to scan bottom-up. Edge
      case (only matters when two markers exist in one viewport,
      which is itself a bug). Deferred until a real scenario
      surfaces.

### E5. CSI `t` cell-size response handling (deferred)

- [ ] Verify what crossterm does with an unsolicited `\x1b[6;H;Wt`
      reply (e.g., from a tmux passthrough) and either swallow it
      in `Tui::handle_input` or document the fall-through. We don't
      query cell size, but a stray reply is an environmental input-
      corruption hazard. Deferred until investigation; tracking as
      a known-unknown.

### E6. Middle-row skip optimization documentation

- [x] The diff path's "skip `\x1b[2K`+rewrite for unchanged middle
      rows" optimization is intentional. Documented in `tui.rs`
      comments (already in place pre-Phase E); flagged here so
      future reviewers know to look for it.

---

## Phase F: Component corrections (in progress)

User-visible component drifts surfaced by the same review. F1 and F7
landed; the others are tracked for the next sweep.

### F1. Editor right-side row padding

- [x] `Editor::render` now emits `left_padding + line_text +
      inner-pad-to-content-width + right_padding` for each visible
      row and each autocomplete-popup row. Without the right-pad,
      a row whose text is shorter than the editor's content area
      would leave the right-side cells untouched and display the
      wrong terminal background color until the next full repaint.
      Cursor-in-padding shrinks the `right_padding` by one so total
      visible width still matches the render width.
- [x] Regression tests in `tests/editor_row_padding.rs`
      (visible width matches render width across padding 0/4,
      focused/unfocused, empty, and cursor-at-end-of-full-width-line).

### F7. Loader animation pump

- [x] When a `Loader` joins a `Tui` tree (via
      `set_render_handle`) and its indicator has more than one
      frame, the loader spawns a tokio task that pings
      `RenderHandle::request_render()` once per `interval`. The
      Tui's render throttle coalesces those pings, so the per-
      spinner overhead is bounded at one render per interval.
      The pump is cancelled on `Loader::stop()`, on `set_indicator`
      changes, and on drop. Spawn is gated on
      `tokio::runtime::Handle::try_current()` so a Loader rendered
      outside an async runtime (existing sync tests) doesn't panic.
- [x] Regression tests in `tests/loader.rs`
      (`loader_in_a_tui_tree_pings_the_render_loop_each_interval`,
      `loader_animation_pump_does_not_run_for_static_single_frame_indicator`,
      `loader_animation_pump_stops_when_loader_stop_is_called`).

### F2-F6, F8-F15. Remaining component drifts (tracked)

The next component sweep covers (in rough priority):

- [ ] F2: `Editor::max_visible_lines` should auto-size from
      terminal rows (`max(5, floor(rows * 0.3))`) instead of
      requiring a manual setter.
- [ ] F3: Word boundaries in `Editor` and `Input` should treat
      punctuation as breakers, not just whitespace. Extract a
      shared `word_boundary` helper.
- [ ] F4: Tab-in-paste expansion should be 4 spaces consistently
      (currently 1 space in `text_input.rs:592` and `editor.rs:697`).
- [ ] F5: `Editor::handle_paste` should prepend a space when a
      paste starts with `/`, `~`, or `.` and the cursor is preceded
      by a word character.
- [ ] F6: Editor backslash+Enter should be gated on the keymap
      having `shift+enter` bound to submit, not run unconditionally.
- [ ] F8: `Loader` should track a `verbatim` flag on
      `LoaderIndicatorOptions` so a custom-frame indicator skips
      `spinner_style` (currently double-styles).
- [ ] F9: `Markdown` heading style should be reapplied across
      nested inline-token resets so `# foo \`bar\` baz` keeps the
      heading color on `baz`.
- [ ] F10: `Markdown` italic/bold open boundary should require a
      non-word-char on both sides (currently italicizes inside
      `5*4*3`).
- [ ] F11: `Markdown` should handle `br` (soft line break) and
      either pass through or explicitly drop `html` tokens.
- [ ] F12: `Markdown` blockquotes should recurse block-level so a
      fenced code block inside a `>` quote renders.
- [ ] F13: `SelectList` filter behavior — decide whether to keep
      our scroll-offset persistence or switch to a stateless
      "recompute window from selected_index every render" model.
- [ ] F14: `Text` minor drifts — `padding_y` default, tab-to-3-
      spaces normalization, empty-text returning `[""]` not
      `Vec::new()`.
- [ ] F15: `TextBox` empty-row padding asymmetry (route both
      top/bottom and content-row paths through the same
      bg-application helper).

---

## Phase G: Test-harness shore-up (in progress)

Quality-of-life items for the test harness. G1, G2, G5 landed; the
rest are tracked.

### G1. `cells_in_row` width source

- [x] `cells_in_row` now sources its grid bounds from
      `screen().size()` rather than the cached `state.columns` /
      `state.rows`. The cache is updated alongside the parser by
      `resize`, but if a future change introduces drift, the
      parser-sourced bounds keep the helper from silently emitting
      trailing `CellInfo::default()` filler.
- [x] Regression test
      (`cells_in_row_uses_the_parsers_authoritative_width`).

### G2. `viewport()` trim contract

- [x] `viewport()` now actually trims trailing whitespace per row
      (was advertising the trim in the docstring but inheriting
      only the parser's "don't emit empty cells" behavior). A row
      a component pads with explicit trailing spaces now reads
      back without them, so assertions like `viewport()[0] ==
      "hello"` work regardless of how the row was padded.
- [x] Regression test
      (`viewport_strips_trailing_whitespace_from_each_row`).

### G5. `LoggingVirtualTerminal` alias smoke

- [x] Smoke test confirming the alias works through its alias
      name. The alias is a `pub type`, so a refactor that
      accidentally turned it into a wrapper struct would compile
      but break tests that signal intent through the alias.
- [x] Regression test
      (`logging_virtual_terminal_is_a_naming_alias_over_virtual_terminal`).

### G3, G4, G6-G9. Remaining harness items (tracked)

- [ ] G3: Doc note on `VirtualTerminal::flush()` clarifying that
      writes are synchronous and no flush is needed before a
      viewport read.
- [ ] G4: Panicking `cell_at` / `row_cells` variants alongside
      the `Option`-returning `cell` / `cells_in_row`.
- [ ] G6: Rename `support::wait_for_render` (sync) and
      `support::async_tui::wait_for_render` (async) to make the
      engine choice obvious at the call site.
- [ ] G7: `MutableLines::with_lines` over `Vec<String>` smoke test.
- [ ] G8: Doc note on `scroll_buffer()` flagging the internal
      `RefCell::borrow_mut` as not safe for re-entrant calls.
- [ ] G9: Tests-from-the-other-side that we could port — only
      `tui-cell-size-input.test.ts` is in scope (paired with E5
      above).

---

## Phase C: Integration into `aj`

Once the crate is stable, the `aj` coding agent can start consuming
`aj-tui` for its interactive surface. That's separate work and is
tracked here as open items.

- [ ] Wire the agent's chat surface through `aj-tui::Tui` with an
      `Editor` for user input and a `Markdown` component for model
      output.
- [ ] Replace the current placeholder `aj-ui` with `aj-tui`.
- [ ] Keep `aj-tui` standalone — no `aj-tui` → agent dependencies,
      the crate stays reusable.

---

## Phase D: Keybinding registry wiring (complete)

`src/keybindings.rs` shipped a `KeybindingsManager` with defaults in
`tui_keybindings()` and a user-override surface, but the built-in
components (`Editor`, `Input`, `SelectList`, `SettingsList`,
`CancellableLoader`) matched `KeyCode` directly and never consulted
the manager — user overrides had no effect on what components
reacted to. Phase D plumbed every component through the registry.

- [x] Process-scoped accessor: `aj_tui::keybindings::{get,
      set_user_bindings, set_manager, reset}`, backed by a
      `LazyLock<RwLock<KeybindingsManager>>` initialized to the
      built-in defaults. Mirrors the original framework's
      `getKeybindings()` / `setKeybindings()` shape.
- [x] Refactor `CancellableLoader` through `tui.select.cancel` (gains
      Ctrl+C alongside Escape).
- [x] Refactor `SelectList` through `tui.select.{up,down,confirm,
      cancel,pageUp,pageDown}` (gains Ctrl+C cancellation).
- [x] Refactor `SettingsList` through `tui.select.{up,down,confirm,
      cancel}`. Space remains hardcoded as a confirm alias and is
      reserved (never forwarded into the embedded search input),
      matching the original framework.
- [x] Refactor `Input` through `tui.select.cancel`,
      `tui.input.submit`, `tui.editor.undo`, and the
      `tui.editor.{cursor,delete,yank}*` family. Ctrl+C now fires
      `on_escape` (via `tui.select.cancel`) instead of bubbling to
      the parent — matching the original framework's Input.
- [x] Refactor `Editor` through `tui.input.{copy,tab,newLine,submit}`,
      `tui.editor.jump{Forward,Backward}`, `tui.editor.undo`,
      `tui.editor.cursor{Up,Down,Left,Right}`,
      `tui.editor.cursorLine{Start,End}`,
      `tui.editor.cursorWord{Left,Right}`,
      `tui.editor.page{Up,Down}`,
      `tui.editor.delete{Char,Word}{Backward,Forward}`,
      `tui.editor.deleteToLine{Start,End}`, `tui.editor.yank`,
      `tui.editor.yankPop`. The autocomplete-popup intercept routes
      through `tui.select.{cancel,up,down,confirm}` and
      `tui.input.tab`. Page scrolling uses `max_visible_lines` as the
      page size.
- [x] Inline `key_id_matches(event, "shift+backspace")` /
      `"shift+delete"` aliases on the editor's delete branches —
      these are deliberately outside the registry to match the
      original framework's `matchesKey(...)` shape.
      `shift+space` already inserts a literal space via the printable
      fallback (no special handler needed; crossterm reports it as
      `KeyCode::Char(' ') + SHIFT`).
- [x] Integration test
      (`tests/keybindings_user_overrides.rs`): rebind
      `tui.input.submit` to `ctrl+enter` and confirm the Editor
      responds to the new binding, plain Enter no longer submits,
      and unrelated actions (cursor-left, backspace, Ctrl+W) keep
      working untouched.

Net behavioral additions from registry routing:

- Editor: `PageUp` / `PageDown` now bound. The registry carried the
  defaults all along; the previous hand-rolled match never wired
  them. Page size = `max_visible_lines`. Auto-sizing
  `max_visible_lines` from terminal rows (the original framework's
  `max(5, floor(rows*0.3))`) is on the next-phase list and unaffected
  by Phase D.
- `CancellableLoader`, `SelectList`, `SettingsList`: Ctrl+C now
  cancels alongside Escape (matching the original framework's
  `tui.select.cancel = ["escape", "ctrl+c"]` default).
- `Input`: Ctrl+C fires `on_escape` instead of bubbling to the
  parent (matching the original framework's Input).

Defaults are otherwise unchanged: every key arm in the previous
hand-rolled matches resolves through the registry to identical
behavior.

---

## Done: Initial porting sweep

The bulk of the framework is in place. Everything below is retained
as a historical record.

### Test-support framework

- [x] Track `start` / `stop` call counts on `VirtualTerminal` (keep
      empty byte emission).
- [x] `support::send_keys(&mut tui, [...])` batching helper.
- [x] `fixtures::StaticOverlay` — lift the captured-width pattern
      out of `tests/overlay_options.rs`.
- [x] "Deviations" section in `tests/support/README.md` covering:
      `clear_line` emits CSI `2K` (not CSI `K`); typed `InputEvent`
      dispatch instead of raw byte streams; `force_full_render` as a
      separate surface; key release / repeat dropped at the
      crossterm boundary (until `wants_key_release` lands).
- [x] Move one sync-tier test onto the async-tier helper as a smoke
      check that the two helpers stay signature-compatible.

### Library surface

- [x] `Terminal::set_progress(active)` + OSC 9;4 emission in
      `ProcessTerminal`. No-op default on the trait;
      `VirtualTerminal` captures the last state.
- [x] `Component::wants_key_release` opt-in.
- [x] `Tui::add_input_listener` / `remove_input_listener` with
      `{ consume, rewrite }` return semantics.
- [x] `Tui::request_render` and `Tui::request_full_render` /
      `Tui::force_full_render` split.
- [x] `SettingsList` submenu support.
- [x] `Loader::set_indicator(Option<LoaderIndicatorOptions>)`.
- [x] Write-log env var (`AJ_TUI_WRITE_LOG`).
- [x] Terminal-capabilities surface with an OSC 8 hyperlinks flag
      on `MarkdownTheme`.

### Tests for the new surfaces

- [x] `set_progress` emits and clears the OSC 9;4 sequence.
- [x] `wants_key_release` opt-in (press / repeat / release dispatch
      at all three sites).
- [x] Input listeners (order, consume, rewrite, removal).
- [x] Forced full render.
- [x] Settings submenu (Enter → submenu → done → parent updates).
- [x] Loader indicator (custom frames, interval, hidden).
- [x] `AJ_TUI_WRITE_LOG` (file mode, directory mode, unset).

### Post-initial-sweep follow-ups

- [x] `Tui::differential_render` correctly tracks the hardware-
      cursor row when previous content was taller than new content.
- [x] `SettingsList` search input renders with a `"> "` prompt so
      its text aligns with the `"→ "` / `"  "` gutter on item rows.
- [x] `Input`: Enter on a value with a trailing backslash submits
      verbatim.
- [x] `Input::render`: clamps width for Hangul, Japanese, Chinese,
      and full-width ASCII at three cursor positions (start /
      middle / end) across three terminal widths.
- [x] `Input::render`: cursor stays inside the visible window when
      horizontally scrolling a wide CJK string.
- [x] `Editor`: undo reverts an Alt+D (delete-word-forward) as a
      single atomic unit.
- [x] `Editor` history: Down transitions forward to a newer entry
      after walking back past it and returning to an older one.
- [x] `Tui::show_hardware_cursor()` / `set_show_hardware_cursor`
      accessors (subsequently split per A7).
- [x] `Editor::padding_x()` / `autocomplete_max_visible()` getters.

### Async event loop

- [x] Fold the standalone driver into `Tui`; render channel and
      `RenderHandle` live on `Tui::handle()`.
- [x] `Terminal::take_input_stream() -> Option<InputStream>`.
- [x] Auto-wire `RenderHandle` to editors via `Container::add_child`
      / `insert_child` / `Tui::show_overlay`.
- [x] Drop `Component::tick`; editors drain at render time.
- [x] Delete `Terminal::poll_event` / `try_recv_event`; only
      `Tui::next_event().await`.
- [x] All examples and tests migrated to the async path.

### Examples

- [x] `chat_simple.rs`.
- [x] `key_tester.rs`.
- [x] `viewport_overwrite_repro.rs`.
- [x] `settings_demo.rs`.
- [x] Document design choices in `examples/PARITY.md`.

### Out of scope (will not be ported)

- Terminal image protocols (Kitty graphics, iTerm2, sixel) and the
  associated components/tests.
- Hand-rolled stdin byte-buffer parsing — crossterm already parses
  CSI / OSC / DCS / APC, bracketed paste, mouse reports, and the
  Kitty keyboard protocol at the `ProcessTerminal` boundary.
- Cell-size probing (tied to the image pipeline).
- Windows console `koffi`-based VT input shimming — crossterm
  handles Windows console modes.
