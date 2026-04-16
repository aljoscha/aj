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

## Phase E: Render-engine corrections (mostly complete)

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

### E4. `extract_cursor_position` scan direction

- [x] Ported pi-tui's bottom-up scan
      (`packages/tui/src/tui.ts:868-887`,
      `for (let row = lines.length - 1; row >= viewportTop; row--)`).
      The Rust spelling is `for row in (viewport_top..lines.len()).rev()`,
      which is empty when `lines` is empty (no underflow) and
      otherwise yields the bottom row first. The original deferral
      framed this as a correctness edge case (only matters when two
      markers exist in one viewport, which is itself a component
      bug); a re-read of pi's CHANGELOG #1004 surfaces the real
      motivation as **performance**: the cursor lives in whatever
      component currently has focus, which in chat/agent layouts is
      almost always an `Editor` or `Input` pinned near the bottom
      of the frame. Bottom-up early-outs on the typical case before
      touching the (often much longer) markdown / log content
      above it. Pi's scan also early-outs above `viewportTop`; our
      port already matched that part. No new test added: pi has
      none for this private function (it's covered indirectly by
      hardware-cursor-positioning integration tests), and the
      existing `test_cursor_marker_extraction` (cursor on row 1
      out of 2) and `extract_cursor_position_strips_only_the_first_marker_on_a_line`
      (single-row case) both continue to pass — they exercise the
      function under conditions where direction doesn't change the
      result.

### E5. CSI `t` cell-size response handling

- [x] Investigated. **Delegate to crossterm; no `Tui::handle_input`
      consume needed for our scenario.**

      **Pi's design.** Pi-tui has an explicit
      `consumeCellSizeResponse` step in `TUI.handleInput`
      (`/tmp/pi-mono/packages/tui/src/tui.ts:576-594`) that regex-
      matches `^\x1b\[6;(\d+);(\d+)t$` on raw bytes, calls
      `setCellDimensions({ widthPx, heightPx })`, then `invalidate()`
      and `requestRender()`. The cell dimensions feed `terminal-image.ts`
      (kitty/iTerm2 image encoding, `calculateImageRows`); pi gates the
      query on `getCapabilities().images`, so the response only
      arrives when image rendering is plausible. Pi additionally has
      a [`StdinBuffer`] (its own file under `tui/src/stdin-buffer.ts`)
      with explicit CSI/OSC/DCS terminator detection and a 10ms timeout
      to assemble complete sequences before they reach `handleInput`.

      **Our position.** We have no image rendering — `capabilities.rs`
      tracks `images: Option<ImageProtocol>` as a flag for future use,
      but no encoder, no `terminal-image` analog, no
      `calculateImageRows`. So the side-effect pi extracts from the
      reply (`setCellDimensions`) has nothing to feed in our port; if
      we mirrored pi's consume verbatim, we'd be tracking a piece of
      state that no consumer reads.

      We also can't pattern-match raw bytes at our typed-input layer:
      by the time data reaches `Tui::handle_input` it's already a
      typed `InputEvent` (crossterm parsed it). The structural
      counterpart of pi's regex consume is therefore one layer earlier,
      at crossterm's `Parser::advance` — and crossterm 0.28 already
      does the right thing for the typical atomic-reply case.

      **Crossterm 0.28 behavior on `\x1b[6;<h>;<w>t`.** `Parser::advance`
      pushes bytes one at a time. While the buffer ends in a digit or
      semicolon, `parse_csi` waits (`last_byte` not in `64..=126`).
      Once the final `t` arrives, the digit branch in `parse_csi`
      doesn't recognize `t` (not `M`/`~`/`u`/`R`) and falls through to
      `parse_csi_modifier_key_code`, which fails because `t` isn't in
      the keycode table `{A,B,C,D,F,H,P,Q,R,S}`. `Parser::advance`'s
      `Err(_)` arm clears the buffer. The 10-byte response is silently
      consumed; following bytes (e.g. a user's `q`) start a fresh
      parse. For the typical case — terminal sends the reply as a
      single atomic write to stdin — behavior matches pi: the response
      is invisible to focused components and following user input
      arrives intact.

      **Partial-reply edge.** If a reply gets split into chunks
      interleaved with user input (vanishingly rare in practice, since
      the OS pipe usually delivers the reply atomically), both pi and
      crossterm degrade identically. Pi's `StdinBuffer` 10ms timeout
      fires and emits the partial bytes, which then don't match
      `consumeCellSizeResponse`'s regex and dump to the focused
      component as garbage input. Crossterm sees the user's `\x1b[`
      arrive while its accumulator still holds the partial reply
      (`\x1b[6;`), the digit branch lands on `parse_csi_modifier_key_code`
      with junk in the params and the new `[` as the final byte,
      errors, and clears the buffer — which means the user's `\x1b[`
      is gone, and their following `1;5D` (etc.) gets emitted as char
      events. Different shape of corruption, same edge of equivalent
      severity. Not worth fixing on either side without a real-world
      complaint.

      **Code change.** Documented in [`InputEvent`]'s `TryFrom<Event>`
      rustdoc in `src/keys.rs` (concise — the full trace lives here
      in PORTING.md), and as a note at the head of
      `Tui::handle_input_after_listeners` in `src/tui.rs` flagging the
      missing-by-design step with a cross-reference. No behavior
      change.
- [x] No regression test required. Crossterm's `parse_event` /
      `Parser::advance` are `pub(crate)` and `EventStream` reads from
      real stdin only; a Rust-side unit test analogous to
      `tui-cell-size-input.test.ts` isn't expressible without a PTY
      harness, and the current crate has no PTY dependency. The
      behavior is locked in by the trace above against crossterm
      0.28's source; future crossterm upgrades will need a re-read.
      Cross-link G9 below for the test-port decision.

### E6. Middle-row skip optimization documentation

- [x] The diff path's "skip `\x1b[2K`+rewrite for unchanged middle
      rows" optimization is intentional. Documented in `tui.rs`
      comments (already in place pre-Phase E); flagged here so
      future reviewers know to look for it.

### E7. Cursor-marker stripping: one occurrence vs all

- [x] `extract_cursor_position` in `src/tui.rs` (search for
      `CURSOR_MARKER`, near line 500) now splices out only the marker
      at `marker_pos` rather than calling `line.replace(CURSOR_MARKER,
      "")` (which scrubs every occurrence on the line). Matches the
      original framework's single-splice behavior so a buggy component
      that emits two markers on one row leaves the stray visible in
      the rendered frame for diagnosis instead of having it silently
      removed.
- [x] Regression test
      (`extract_cursor_position_strips_only_the_first_marker_on_a_line`):
      a line with three markers extracts the first, returns its
      column, and asserts the other two survive verbatim in the
      rendered output.

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

- [x] F2: `Editor::max_visible_lines` auto-sizes from terminal rows
      (`max(5, floor(rows * 0.3))`) when the user has not called
      [`Editor::set_max_visible_lines`]. The cap is read from
      [`RenderHandle::terminal_rows`] (newly published by `Tui` from
      `start()` and the top of every `render()`); a freshly-
      constructed editor with no handle falls back to the floor of
      `5`. `set_max_visible_lines` still works as an explicit
      override; `clear_max_visible_lines` is the escape hatch back
      to auto-sizing. Regression tests in
      `tests/editor_max_visible_lines.rs`
      (`max_visible_lines_falls_back_to_five_with_no_render_handle`,
      `max_visible_lines_auto_sizes_from_terminal_rows_after_start`,
      `max_visible_lines_floor_is_five_on_short_terminals`,
      `explicit_set_max_visible_lines_wins_over_auto_size`,
      `clear_max_visible_lines_re_enables_auto_sizing`).
- [x] F3: Word boundaries in `Editor` and `Input` use a shared
      three-class model (whitespace / punctuation / word). The
      classifier and walker live in
      [`aj_tui::word_boundary`](src/word_boundary.rs):
      `word_boundary_left`, `word_boundary_right`, plus the
      lower-level `skip_whitespace_forward` / `skip_word_class_forward`
      pair the editor uses to splice paste-marker handling between
      the whitespace skip and the class skip without duplicating
      either step. Both components delegate to the helper. Input-side
      regression tests in `tests/text_input.rs`
      (`ctrl_right_walks_word_then_punctuation_then_word_runs`,
      `ctrl_left_walks_word_then_punctuation_then_word_runs`,
      `ctrl_w_treats_punctuation_run_as_its_own_word`) plus unit
      tests in `src/word_boundary.rs`. Editor's existing tests
      (`tests/editor_unicode.rs`) continue to pass unchanged.
- [x] F4: Tab-in-paste expansion is now four spaces consistently in
      both `Editor::handle_paste` and `Input`'s `InputEvent::Paste`
      arm. Previously each pasted tab collapsed to a single space,
      so a copy-paste-from-source-code action lost its indent.
      Newlines are still preserved by the editor (and stripped by
      the single-line input); other control chars are still dropped.
      Regression tests in `tests/text_input.rs`
      (`paste_with_tabs_expands_each_tab_to_four_spaces`,
      `paste_strips_other_control_chars_but_keeps_tabs_expanded`)
      and `tests/editor_paste_marker.rs`
      (`small_paste_expands_each_tab_to_four_spaces`,
      `small_paste_with_tabs_and_newlines_preserves_both`,
      `small_paste_strips_non_tab_control_chars`).
- [x] F5: `Editor::handle_paste` prepends a space when the paste
      begins with `/`, `~`, or `.` and the cursor is preceded by a
      word-class grapheme on the current line. Prevents
      `cd` + paste `/etc/hosts` from reading as `cd/etc/hosts`. The
      space lands in the line buffer (not the stored paste content),
      so both inline pastes and marker-replaced pastes get the
      separator and `get_expanded_text` recovers the literal pasted
      bytes alongside the separator. Cursor at start-of-line, after
      whitespace, or after punctuation skips the prepend. Regression
      tests in `tests/editor_paste_marker.rs`
      (`paste_with_slash_prefix_after_word_prepends_space`,
      `paste_with_tilde_prefix_after_word_prepends_space`,
      `paste_with_dot_prefix_after_word_prepends_space`,
      `paste_with_path_prefix_after_space_does_not_prepend`,
      `paste_with_path_prefix_at_start_of_line_does_not_prepend`,
      `paste_with_path_prefix_after_punctuation_does_not_prepend`,
      `paste_without_path_prefix_after_word_does_not_prepend`,
      `large_paste_with_path_prefix_after_word_separates_marker_with_space`).
- [x] F6: Editor backslash+Enter has an inverse workaround for the
      "swap config" (user has bound `shift+enter` to `tui.input.submit`,
      typically with `enter` bound to `tui.input.newLine`). In that
      config, plain `\<Enter>` strips the trailing backslash and
      submits — mirroring the standard workaround in the opposite
      direction. Gated on `tui.input.submit` containing `shift+enter`
      or `shift+return`, on the input being plain `enter` (no
      modifiers), on `disable_submit` being false, and on the cursor
      being preceded by `\`. Mirrors the original framework's
      `shouldSubmitOnBackslashEnter`. Also extracted a shared
      `submit_value` helper used by the standard submit branch and the
      inverse workaround. Regression tests in
      `tests/editor_backslash_enter.rs`
      (`enter_with_backslash_in_swap_config_submits_instead_of_newline`,
      `enter_with_backslash_in_default_config_still_inserts_newline`,
      `shift_enter_with_backslash_in_swap_config_submits_normally_no_strip`,
      `enter_without_backslash_in_swap_config_inserts_newline_not_submits`,
      `disable_submit_blocks_inverse_workaround_too`).
- [x] F8: `Loader` tracks a `verbatim` flag on
      `LoaderIndicatorOptions` so a custom-frame indicator whose
      strings already carry their own ANSI escapes skips
      `spinner_style` instead of double-wrapping. Set via
      [`LoaderIndicatorOptions::with_verbatim(true)`]; defaults to
      `false` so existing call sites are unchanged. The flag is
      stored on `Loader` itself and reset to `false` on
      `set_indicator(None)` (the default braille spinner is plain
      text and should be styled). `message_style` is unaffected.
      Regression tests in `tests/loader.rs`
      (`verbatim_indicator_skips_spinner_style_so_pre_styled_frames_arent_double_wrapped`,
      `non_verbatim_indicator_still_applies_spinner_style`,
      `set_indicator_none_clears_the_verbatim_flag`).
- [x] F9: `Markdown` heading style is reapplied across nested
      inline-token resets so a heading like `# foo \`bar\` baz` keeps
      the heading color on `baz`. Implementation wraps each top-level
      inline (text run, code, bold, italic, strikethrough, link) with
      the heading style independently, so an inline code's
      `\x1b[39m` no longer strips the heading's foreground color from
      following text. H1 keeps its outer underline by composing
      `heading(underline(...))` per segment; H3+ render the `### `
      prefix as its own heading-styled segment so the prefix and body
      get the same treatment. Refactor split `render_inlines` into a
      per-element [`Markdown::render_inline`] helper plus a new
      [`Markdown::render_inlines_with_outer`] that coalesces
      consecutive text runs into a single wrap call. Tightened the
      existing weak tests
      (`preserves_heading_styling_after_inline_code_h3`,
      `preserves_heading_styling_after_inline_code_h1`,
      `preserves_heading_styling_after_bold_text`) so they assert the
      heading style codes appear *between* the inline element's
      content and the trailing text rather than just somewhere in
      the 40 chars before; added
      `preserves_heading_color_on_trailing_text_after_inline_code`
      as the dedicated F9 regression that walks back from ` baz` to
      the most recent `\x1b[36m` open and asserts it lies strictly
      after the inline code's `bar` content.
- [x] F10: `Markdown` italic/bold open boundary requires a non-word
      char on both outer sides of the emphasis run. The opening
      `*`/`_` (single or double) must not be preceded by a word
      character, and the closing must not be followed by one — so
      `5*4*3` and `5**4**3` parse as plain text instead of italicizing
      `4`, and `foo_bar_baz` stays a single intraword run. The italic
      arm also rejects opening when `chars[i - 1] == chars[i]` so a
      rejected `**` opener doesn't fall through to italic on its
      second `*`. Implementation factored into
      `find_emphasis_closing`, `preceded_by_word_char`, and
      `is_emphasis_word_char` (alphanumeric or `_`) helpers next to
      the existing `find_closing_marker`. Regression tests in
      `tests/markdown.rs`
      (`intraword_asterisks_do_not_italicize_when_both_sides_are_word_chars`,
      `intraword_double_asterisks_do_not_bold_when_both_sides_are_word_chars`,
      `intraword_underscores_do_not_italicize`,
      `intraword_double_underscores_do_not_bold`,
      `well_formed_italic_at_a_word_boundary_still_renders`,
      `well_formed_bold_at_a_word_boundary_still_renders`,
      `italic_at_start_of_text_still_opens`,
      `italic_at_end_of_text_still_closes`).
- [x] F11: `Markdown` paragraph soft / hard line breaks and HTML-token
      policy. **Pi-aligned**: every newline in paragraph source is
      preserved as a visible line break in the rendered output (matches
      the upstream marked + wrap-text-with-ANSI pipeline, which
      preserves `\n` inside paragraph text tokens). The CommonMark
      "soft break renders as a space" default isn't applied because
      it's a poor fit for an agent's chat surface — a CLI user typing a
      multi-line message expects each typed line on its own row. The
      two CommonMark hard-line-break markers (two-or-more trailing
      spaces, or a single trailing backslash at end-of-line) are still
      recognized and stripped so the marker bytes don't render
      literally; functionally they're now equivalent to the un-marked
      case (every break is visible), just without the trailing
      whitespace or `\\`. Implemented as [`join_paragraph_lines`] +
      [`split_hard_break`] helpers next to `parse_markdown`. HTML
      tokens are documented as pass-through-as-plain-text in the
      rustdoc on `parse_inline`: a model emitting
      `<thinking>...</thinking>` keeps those exact bytes visible in
      the output rather than being silently swallowed. Regression
      tests in `tests/markdown.rs`
      (`paragraph_with_two_trailing_spaces_inserts_hard_line_break`,
      `paragraph_with_trailing_backslash_inserts_hard_line_break`,
      `paragraph_with_single_trailing_space_still_breaks_to_a_new_visible_row`,
      `paragraph_with_three_or_more_trailing_spaces_still_inserts_hard_line_break`,
      `paragraph_without_break_marker_renders_each_source_line_on_its_own_row`,
      `paragraph_with_break_at_last_line_strips_marker_without_emitting_blank_row`,
      `html_tags_in_paragraph_text_pass_through_verbatim`).
- [x] F12: `Markdown` blockquotes recurse block-level. The blockquote
      parser strips the `> ` prefix off each source line, joins the
      result, and feeds it back through `parse_markdown` to obtain
      `Vec<Block>`. The renderer dispatches each sub-block via
      `render_block` at the reduced inner width, applies `theme.quote`
      to each non-empty rendered line, and prepends the quote border
      to every line. A fenced code block, list, heading, table, or
      any other block-level element nested inside a `>` quote now
      renders as its native block instead of as literal per-line text.
      `Block::Blockquote` stores `Vec<Block>` instead of
      `Vec<Vec<Inline>>`. Multi-line plain-text quotes
      (`> Foo\n> bar`) keep rendering as two bordered rows: the F11
      pi-aligned change preserves `\n` inside the inner paragraph,
      and downstream `wrap_text_with_ansi` expands those newlines
      into visible rows. No per-line / sub-block hybrid hack — the
      F11 architectural fix made full block-level recursion the
      cleanest model. Regression tests in `tests/markdown.rs`
      (`renders_fenced_code_block_inside_blockquote`,
      `fenced_code_block_inside_blockquote_does_not_collapse_surrounding_paragraphs`).
- [x] F13: `SelectList` filter behavior — switched to pi's
      stateless centering model. The visible window is
      recomputed from `selected` on every render via
      `start = clamp(selected - max_visible / 2, 0, len -
      max_visible)`, mirroring `select-list.ts` byte-for-byte.
      The persistent `scroll_offset` field is gone; selection
      is now the only piece of scroll-affecting state.
      `set_filter` / `set_items` reset `selected` to `0` (no
      separate scroll bookkeeping). Move semantics in
      `move_selection` simplify to "update `selected` only" —
      the next render naturally lands the new selection at
      the appropriate window position. Documented on the
      type-level docstring of [`SelectList`] in
      `src/components/select_list.rs`.
      Regression tests in `tests/select_list.rs`
      (`sequential_down_navigation_centers_selection_in_window`,
      `up_wrap_at_top_clamps_window_to_show_selection_at_bottom_edge`,
      `down_wrap_at_bottom_clamps_window_to_show_selection_at_top_edge`,
      `set_filter_resets_selection_to_top_and_window_follows`).
- [x] F14: `Text` minor drifts — pi-tui parity for default
      padding, tab normalization, the empty / zero-row paths, and
      the per-line layout. `Text::new(...)` defaults to
      `padding_y = 1` (matches pi's constructor default) so a
      vanilla call renders with one blank row above and below
      content. Tabs in the source string are normalized to three
      spaces before wrapping (`text.replace('\t', "   ")`). Whitespace-
      only input is treated as empty (matches pi's
      `text.trim() === ""` early return) so resetting the content
      to spaces / newlines emits zero rows instead of just-padding
      rows. `content_width` clamps to
      `max(1, width - 2 * padding_x)` to match pi's `Math.max(1, ...)`,
      removing the early `Vec::new()` return on degenerate widths.
      Per-line layout now matches pi byte-for-byte:
      `leftMargin + line + rightMargin` followed by space-padding
      to the full render width (the previous port emitted only
      `leftMargin + line` with no right margin or trailing pad in
      the no-bg case, leaving the right side of each row at
      whatever width the wrapped text produced). The bg branch
      routes through `apply_background_to_line` for both content
      rows and the top/bottom padding rows, hoisting the empty-line
      template outside the loop the same way pi does. Cache
      semantics also match pi's quirky asymmetry: `cached_lines`
      stores the pre-fallback `result`, so the tail
      `result.length > 0 ? result : [""]` only fires on the *current*
      render and a subsequent cache-hit returns the (possibly empty)
      cached vec verbatim. The fallback itself is defensive code —
      both pi's and our `wrap_text_with_ansi` always return at
      least one line for any input that passed the empty check, so
      it's unreachable for real inputs; we keep it for byte-level
      parity. Module-level `DEFAULT_PADDING_X`, `DEFAULT_PADDING_Y`,
      `TAB_AS_SPACES` consts document the pi origin in place.
      Existing in-module unit tests updated to set explicit
      `padding_y(0)` where they assert content-only line counts.
      New regression tests in `tests/text.rs`
      (`default_padding_y_is_one_so_content_is_sandwiched_in_blank_rows`,
      `tab_in_input_is_normalized_to_three_spaces_before_wrapping`,
      `multiple_tabs_each_expand_to_three_spaces_independently`,
      `empty_text_renders_zero_rows_even_with_padding_y_set`,
      `whitespace_only_text_is_treated_as_empty`,
      `ansi_only_input_renders_one_full_width_row`,
      `degenerate_width_smaller_than_horizontal_padding_still_renders_content`,
      `content_row_no_bg_pads_to_full_render_width`,
      `content_row_with_bg_applies_bg_across_full_width`,
      `repeat_render_with_same_args_returns_cached_result`).
- [x] F15: `TextBox` empty-row padding asymmetry. Added a private
      `apply_bg_row(line, width)` helper modeled on pi-tui's
      `Box.applyBg`: pad the line to `width` columns by visible
      width, then if `bg_fn` is set call `apply_background_to_line`,
      otherwise return the padded line. The render method now routes
      every row of output (top padding, content rows, bottom
      padding) through the helper, replacing three different code
      paths with one. Visible fix: the no-bg content path now
      right-pads shorter rows so the box always renders a full-width
      rectangle (previously a row whose child returned text shorter
      than the content width emitted only `left_pad + child_line`,
      leaving the right-side cells at whatever the terminal had
      previously). Regression tests in `tests/text_box.rs`
      (`no_bg_content_row_right_pads_to_full_width`,
      `no_bg_padding_row_matches_pi_byte_for_byte`,
      `bg_padding_and_content_paths_share_one_pipeline`,
      `bg_content_with_ansi_escapes_pads_by_visible_width`).

### F16-F48. Audit-surfaced component drifts (tracked)

A side-by-side review of every component pair against the
original surfaced 15 drifts beyond the F2-F15 set, plus a
handful of internal-API divergences. Subsequent cross-walks
(F38-F44 first; F45-F48 from the F41 pi cross-check) added a
few more as the review widened. Grouped by component for sweep
planning. Severity ranking (visible-to-user first) is in the
line ordering within each group.

**Priority note (closed).** F49 (cross-component constructor-vs-setter
parity sweep, listed under `#### TextBox` for emergence reasons)
was the next priority item across `Text` / `Markdown` /
`TruncatedText` / `SelectList`, ahead of every other open F-item
including F46-F48. **All four sub-components landed**; F50
(Editor autocomplete slash-command layout) was newly tracked
during the F49/SelectList pi cross-check. F51 (Tui-level
container-method forwarding) was tracked during the F35 pi
cross-check. Resume the remaining open F-items in document
order: F51 (F46, F47, F48, and F50 landed).

#### Editor

- [x] F16: Cursor cell reset uses `\x1b[7m{ch}\x1b[0m` in
      `src/components/editor.rs` (the two render-cursor sites,
      around lines 2879 and 2898). Previously emitted
      `\x1b[7m{ch}\x1b[27m` — reverse-video-only — which left any
      foreground/background/attribute styling open in the line
      text bleeding past the cursor cell into the cells that
      follow on the same row. The full SGR reset matches the
      original framework and closes whatever styling was open.
      Both branches (cursor-on-grapheme and cursor-at-end-of-line
      highlighted-space form) updated in the same change.
      Regression tests in `tests/editor_cursor_reset.rs`
      (`cursor_cell_at_end_of_line_terminates_with_full_sgr_reset`,
      `cursor_cell_on_a_grapheme_terminates_with_full_sgr_reset`,
      `styling_open_before_cursor_does_not_bleed_past_the_cursor_cell`).
- [x] F17: Editor newline-encoding fallback. Verified against pi-tui
      (`/tmp/pi-mono/packages/tui/src/components/editor.ts`, newline
      branch around line 717). The Rust port now mirrors pi's
      structure: byte-form fallbacks fire **regardless of
      `disable_submit`**, not just inside the `disable_submit` branch.
      The shared
      [`is_newline_event`][aj_tui::keys::is_newline_event] helper in
      `src/keys.rs` recognizes exactly three modifier-precise patterns
      so it doesn't shadow user-customized bindings:
      `KeyCode::Char('\n')` with no modifiers (raw LF from non-raw-
      mode input), `KeyCode::Char('j') + CONTROL` exactly (raw LF in
      raw mode — pi's `data === "\n"` byte check), and
      `KeyCode::Enter + ALT` exactly (the parsed form of `\x1b\r`).
      Plain Enter, Shift+Enter, Ctrl+Enter, and combined modifiers
      are intentionally excluded so the registry can route
      `tui.input.submit` / `tui.input.newLine` and any user-rebound
      action without interference. **H2 followup (later):** the
      `(KeyCode::Enter && disable_submit) → newline` arm referenced
      in earlier revisions of this paragraph has been removed for
      strict pi parity. Plain Enter under `disable_submit` is now a
      silent no-op, matching pi's `editor.ts:735-749` submit-branch
      `if (this.disableSubmit) return;` shape; see H2 for the full
      rationale and test impact.
      Crossterm 0.28 still drops the `\x1b[13;2~` xterm Shift+Enter
      encoding before it reaches us; documented in the helper's
      rustdoc as a known gap until crossterm parses it. F20 (Input)
      will consume the same shared helper. Regression tests in
      `tests/editor_newline_fallback.rs` cover all three byte forms
      in both `disable_submit` and normal modes plus the
      submit-not-shadowed and Shift+Enter-via-registry cases. Unit
      tests in `src/keys.rs`
      (`is_newline_event_recognizes_pi_tui_byte_form_fallbacks`,
      `is_newline_event_excludes_keys_that_are_or_might_be_user_bindings`,
      `is_newline_event_rejects_unrelated_keys`) lock in the helper's
      contract directly.
- [x] F18: Slash-command auto-trigger context. Verified against
      `/tmp/pi-mono/packages/tui/src/components/editor.ts`:
      pi-tui's auto-trigger gate inside an existing slash or `@`
      context is `/[a-zA-Z0-9.\-_]/.test(char)` (line 1068), and
      `isInSlashCommandContext` is
      `isSlashMenuAllowed() && textBeforeCursor.trimStart().startsWith("/")`
      (line 2040). Our `is_identifier_char` and
      `is_in_slash_command_context` in `src/components/editor.rs`
      already match byte-for-byte. Added two unit tests in the
      module's `tests` block:
      `is_identifier_char_matches_pi_tui_alphabet` walks every
      ASCII letter/digit plus `.`/`-`/`_` (must qualify) and a
      representative spread of whitespace, the trigger characters
      themselves, common ASCII punctuation/symbols, and non-ASCII
      letters (must not qualify), so an alphabet drift fails
      loudly. `is_in_slash_command_context_matches_pi_tui_trim_start_rule`
      pins down the leading-whitespace tolerance, the
      `is_slash_menu_allowed` second-line gate, and the
      `foo/bar`-not-in-context corner that the trim-start +
      starts-with shape encodes.
- [x] F19: `Editor::with_theme(theme)` lands as the explicit-theme
      constructor that matches the original framework's required-theme
      argument shape. `Editor::new()` stays as the argumentless ergonomic
      shortcut and now delegates to `Self::with_theme(EditorTheme::default())`
      so there is one canonical initializer rather than two parallel field
      lists. Late binding via [`Editor::set_theme`] still works for callers
      that build the editor first and the theme later. Regression tests in
      the editor module's `tests` block
      (`with_theme_applies_supplied_theme_to_render_output`,
      `new_falls_back_to_editor_theme_default`) use a sentinel
      `border_color` to confirm the supplied theme is applied directly to
      the render output and that `new()` keeps the dim-grey default.

      Followed up by extending the same `with_theme` / `new` pattern to
      every other themed component for parity with pi-tui's
      required-theme constructor shape:

      - `Markdown::with_theme(text, theme)` — pi takes
        `(text, paddingX, paddingY, theme, defaultTextStyle?)`; the Rust
        port mirrors only the theme axis since padding and pre-style stay
        as setters. `Markdown::new(text)` delegates here with
        `MarkdownTheme::default()`. Tests
        (`with_theme_applies_supplied_theme_to_render_output`,
        `new_falls_back_to_markdown_theme_default`) verify the supplied
        `code` styler reaches the render output and that `new()` keeps
        the yellow-inline-code default.
      - `SelectList::with_theme(items, max_visible, theme)` — pi takes
        `(items, maxVisible, theme, layout?)`; the Rust port mirrors the
        theme axis and leaves layout as a setter
        ([`SelectList::set_layout`]). Tests
        (`with_theme_applies_supplied_theme_to_render_output`,
        `new_falls_back_to_select_list_theme_default`) verify the
        supplied `selected_prefix` is applied and that `new()` keeps the
        dim-cyan default.
      - `SettingsList::new(items, max_visible, theme, on_change,
        on_cancel, options)` already takes the theme as a required
        argument, byte-for-byte matching pi's
        `new SettingsList(items, maxVisible, theme, onChange, onCancel,
        options?)`. No change required.
      - `Input`, `Loader`, `CancellableLoader`, `Text`, `TextBox`,
        `Spacer`, `TruncatedText` are theme-less in pi-tui too; nothing
        to plumb.

- [x] F50: Editor autocomplete-popup slash-command layout. **Surfaced
      during the F49/SelectList pi cross-check.** Pi-tui's
      `editor.ts:210-213` defines a slash-command-specific
      `SelectListLayoutOptions` constant:

      ```ts
      const SLASH_COMMAND_SELECT_LIST_LAYOUT: SelectListLayoutOptions = {
          minPrimaryColumnWidth: 12,
          maxPrimaryColumnWidth: 32,
      };
      ```

      and `editor.ts:2074-2080` selects between it and the layout
      default based on the autocomplete prefix:

      ```ts
      private createAutocompleteList(
          prefix: string,
          items: Array<{ value: string; label: string; description?: string }>,
      ): SelectList {
          const layout = prefix.startsWith("/") ? SLASH_COMMAND_SELECT_LIST_LAYOUT : undefined;
          return new SelectList(items, this.autocompleteMaxVisible, this.theme.selectList, layout);
      }
      ```

      Our Rust port's `dispatch_autocomplete_request` and
      `apply_autocomplete_snapshot` paths in `src/components/editor.rs`
      both unconditionally pass `SelectListLayout::default()`,
      regardless of whether the autocomplete is firing for a slash
      prefix or for `@`-style file completion.

      **Visible UX gap.** With pi's slash-command layout, the primary
      column for a slash-command popup is bounded `[12, 32]` cells
      regardless of the longest command name; without it, the column
      defaults to `[32, 32]` (the constant `DEFAULT_PRIMARY_COLUMN_WIDTH`
      is `32` in both `select-list.ts` and `select_list.rs`, which
      becomes both the min and the max when neither layout bound is
      supplied). Net effect: pi gives slash commands a tighter `12`-cell
      minimum, so short commands occupy less of the popup width and the
      description column gets more room, while still allowing growth up
      to `32`. Our port behaves like the `@`-completion case for slash
      too — wider primary column, narrower description.

      **Pre-existing.** F49 didn't introduce this gap; the previous
      Rust constructor signature didn't accept a layout at all, so
      every call site was effectively passing pi's `undefined` /
      default. F49 made the layout argument required and the call
      sites now pass `SelectListLayout::default()` explicitly, which
      surfaces the gap as a visible call-site choice rather than an
      implicit default.

      **Resolution.** Three steps, all in `src/components/editor.rs`:

      1. Add a module-level builder mirroring pi's constant:

         ```rust
         /// Slash-command autocomplete popup layout. Mirrors pi-tui's
         /// `SLASH_COMMAND_SELECT_LIST_LAYOUT` (`editor.ts:210-213`).
         /// The `[12, 32]` bounds give short commands more breathing
         /// room for the description column without losing the cap on
         /// long names.
         fn slash_command_select_list_layout() -> SelectListLayout {
             SelectListLayout {
                 min_primary_column_width: Some(12),
                 max_primary_column_width: Some(32),
                 truncate_primary: None,
             }
         }
         ```

         (A function rather than a `const` because `SelectListLayout`
         carries an `Option<Box<dyn Fn>>` field for `truncate_primary`,
         which isn't `const`-compatible.)

      2. Add a private helper `select_list_layout_for_prefix(&str) ->
         SelectListLayout` that returns
         `slash_command_select_list_layout()` when the prefix begins
         with `/` and `SelectListLayout::default()` otherwise. Mirrors
         pi's `prefix.startsWith("/") ? ... : undefined` ternary.

      3. Replace the two `SelectListLayout::default()` call sites
         (around `editor.rs:2262` and `editor.rs:2451`, the
         `SelectList::new(items, ..., self.theme.select_list.clone(),
         SelectListLayout::default())` rows) with
         `self.select_list_layout_for_prefix(&self.autocomplete_prefix)`.
         Both call sites have the prefix already populated on `self`
         before constructing the popup.

      **Tests.** Two regression tests in
      `tests/editor_autocomplete.rs`:

      - `slash_prefix_autocomplete_uses_slash_command_layout`: build
        an editor with a slash-command provider returning items where
        the longest label is ~20 cells, type `/` and a few chars to
        trigger autocomplete, await the suggestions, render the popup,
        and assert the description column starts at column
        `prefix(2) + min_primary(12) + gap(2) = 16` (the F49
        `uses_the_configured_minimum_primary_column_width` shape) —
        not at column `prefix(2) + default(32) + gap(2) = 36`.
      - `at_prefix_autocomplete_keeps_default_layout`: same
        scaffolding but with an `@` provider; assert the default
        column-width behavior (32-cell primary) so the slash branch
        is correctly gated.

      **Cost.** ~30 lines: one builder helper, one prefix
      discriminator, two call-site swaps, and two regression tests.
      Mechanical change; no architectural shifts.

      **Landed.** Both helpers ended up as private methods on
      `Editor` (rather than module-level free functions) so the
      call sites read as `self.select_list_layout_for_prefix(...)`.
      Both call sites in `dispatch_autocomplete_request` and
      `apply_autocomplete_snapshot` now route through the helper.
      The two regression tests assert the description column
      directly: column **14** for the slash branch (= prefix
      marker `2` + min primary column `12`, where
      `min_primary_column_width` already includes the 2-cell gap
      per `select_list.rs`), and column **34** for the default
      branch (= prefix marker `2` + default primary column `32`).
      The original spec text computed `+ gap(2)` separately,
      double-counting the gap; the F49
      `uses_the_configured_minimum_primary_column_width` test in
      `tests/select_list.rs` was the cross-check that landed the
      correct numbers.

      **Shape parity vs pi.** Both Rust SelectList construction
      sites (the one-shot
      `dispatch_autocomplete_request` — pi's analogue — and the
      streaming-session path `apply_autocomplete_snapshot` — a
      Rust-port addition for incremental fuzzy-walker results that
      pi doesn't have) now route through a single
      `create_autocomplete_list(&self, prefix, items) -> SelectList`
      helper. That helper inlines the
      `prefix.startsWith("/") ? SLASH_COMMAND_SELECT_LIST_LAYOUT :
      undefined` ternary the same way pi-tui's
      `editor.ts:2074-2080` does. Caller is still responsible for
      post-construction state (best-match selection, mode flag),
      mirroring pi's split between `createAutocompleteList` and
      `applyAutocompleteSuggestions`. No behavioral change from
      the previous F50 landing — purely a constructor-shape
      consolidation.

#### Input

- [x] F20: Input newline-encoding fallbacks. The submit branch in
      `src/components/text_input.rs` now consults the shared
      [`is_newline_event`][aj_tui::keys::is_newline_event] helper
      alongside `kb.matches(event, "tui.input.submit")`, mirroring
      pi-tui's `input.ts:100` (`kb.matches(data, "tui.input.submit")
      || data === "\n"`).

      **Deliberate parity divergence from pi.** Pi's narrower
      `data === "\n"` check, mapped through crossterm's parsing,
      arrives as either `KeyCode::Char('\n')` (non-raw mode LF) or
      `KeyCode::Char('j') + CTRL` (raw mode — Ctrl+J is ASCII LF
      0x0A). `is_newline_event` recognizes both of those *and* one
      extra form: `KeyCode::Enter + ALT` (Alt+Enter, byte `\x1b\r`).
      Pi-tui's Input silently drops Alt+Enter at the printable-
      character tail (the byte string contains `\x1b`, which fails
      the control-char filter). We submit on it. Three reasons:
      (1) the Editor's newline branch (F17) already uses
      `is_newline_event`, so reusing the recognizer keeps byte-form
      handling symmetric across the two text components;
      (2) submitting on Alt+Enter is strictly better UX than
      silently dropping the keystroke (pi's behavior is arguably a
      bug-of-omission rather than an explicit design choice);
      (3) plain Enter, Shift+Enter, Ctrl+Enter, and combined
      modifiers stay excluded from `is_newline_event` so user
      rebinds of `tui.input.submit` / `tui.input.newLine` are not
      shadowed by the fallback.

      For a single-line `Input`, every byte form the helper catches
      maps to **submit** rather than newline-insert (there's nowhere
      for a newline character to live).

      Regression tests in `tests/text_input_newline_fallback.rs`
      (`raw_lf_char_submits_the_current_value`,
      `ctrl_j_submits_the_current_value`,
      `alt_enter_submits_the_current_value` — explicitly the
      divergence-from-pi path,
      `plain_enter_still_submits_via_the_registry`,
      `plain_j_inserts_a_literal_letter_and_does_not_submit`,
      `ctrl_enter_routes_through_user_rebound_submit_not_the_fallback`,
      `pasted_newline_does_not_submit`).
- [x] F21: `Input::set_value` clamps the existing cursor to the new
      value's length instead of moving it to end. Mirrors pi-tui's
      `Math.min(this.cursor, value.length)` (`input.ts:42-45`), so a
      programmatic value swap mid-edit doesn't yank the caret out
      from under the user. The clamped offset is also snapped
      *forward* to the next UTF-8 char boundary as a defensive guard
      — pi works in JS code units (panic-free), our cursor is a byte
      offset and would crash later string slicing if it landed
      mid-codepoint. Forward (rather than backward) places the
      cursor "past" the disrupted codepoint, matching the analogous
      position pi's UTF-16 clamp would produce when the new value's
      encoding shifts under the cursor (a BMP char that takes two
      UTF-8 bytes is one UTF-16 unit, so pi's `min` lands one
      grapheme further along than ours pre-snap).
      The test helper `seeded(...)` in `tests/text_input.rs` now
      sends Ctrl+E after `set_value` to keep the "type a value,
      cursor lands at end" expectation that drives most existing
      tests; two in-module unit tests
      (`test_input_backspace`, `test_input_undo`) seed cursor
      directly. Regression tests in `tests/text_input.rs`
      (`set_value_keeps_existing_cursor_when_within_new_length`,
      `set_value_clamps_existing_cursor_to_new_length`,
      `set_value_on_a_freshly_constructed_input_keeps_cursor_at_zero`,
      `set_value_snaps_clamped_cursor_off_a_mid_codepoint_byte_offset`,
      `set_value_snap_forward_keeps_cursor_after_partial_multibyte_in_middle`).
- [x] F22: Input undo-coalescing transitions aligned with pi-tui's
      single-`type-word` rule. Pi-tui's `Input.insertCharacter`
      (`/tmp/pi-mono/packages/tui/src/components/input.ts:212-221`)
      pushes a snapshot when the inserted char is whitespace OR
      `lastAction !== "type-word"`, then sets `lastAction =
      "type-word"` unconditionally. The Rust port previously drove
      a 2x4 match table with a separate `LastAction::TypeWhitespace`
      state — observable behavior was identical in every test
      scenario (the `(false, TypeWhitespace) → no-push` arm produced
      the same outcome as pi's "always TypeWord, never push when
      continuing" rule), but the structural divergence was easy to
      drift on. Collapsed `insert_char` to pi's single-line check,
      removed the `TypeWhitespace` enum variant, mirrored the
      multi-line `Editor::insert_char` rule. Regression tests in
      `tests/text_input.rs`
      (`typing_a_word_after_a_space_extends_the_spaces_undo_unit_not_a_new_one`,
      `typing_after_a_kill_starts_a_fresh_undo_unit`,
      `typing_after_a_yank_starts_a_fresh_undo_unit`,
      `typing_after_a_backspace_starts_a_fresh_undo_unit`,
      `each_whitespace_insert_pushes_its_own_snapshot_even_with_words_between`)
      pin the cross-state transitions explicitly.
- [x] F23: Input horizontal scroll heuristic for wide-CJK is
      simpler than the original (see scroll math around
      `src/components/text_input.rs:405-411`). The existing CJK
      clamp tests
      (`render_clamps_line_width_for_wide_cjk_and_fullwidth_strings`,
      `render_keeps_the_cursor_visible_when_horizontally_scrolling_wide_text`)
      cover the rendered-width invariant; the orthogonal invariant —
      that the cursor's underlying *byte position* moves by exactly one
      grapheme's UTF-8 byte length per typed char, per arrow step, and
      per backspace — is now pinned by eight new tests in
      `tests/text_input.rs`
      (`cursor_advances_by_utf8_byte_length_when_typing_hangul`,
      `cursor_advances_by_utf8_byte_length_when_typing_japanese_hiragana`,
      `cursor_advances_by_utf8_byte_length_when_typing_chinese`,
      `cursor_advances_by_utf8_byte_length_when_typing_fullwidth_ascii`,
      `cursor_left_arrow_walks_one_grapheme_per_step_through_wide_text`,
      `cursor_right_arrow_walks_one_grapheme_per_step_through_wide_text`,
      `backspace_decrements_cursor_by_each_graphemes_byte_length_in_wide_text`,
      `mixed_ascii_and_cjk_typing_keeps_cursor_byte_position_consistent`).
      The simpler scroll heuristic stays — it's observably equivalent
      for the existing rendered-width assertions and the new tests
      lock in the byte-arithmetic in `insert_char`, `move_left` /
      `move_right`, and `backspace` independent of how render scrolls.
- [x] F24: Rust adds an `on_change` callback on `Input` that
      the original does not have (`src/components/text_input.rs:38`).
      **Decision: removed for strict parity.** The field had been on
      `Input` since the initial port commit (`30cdfee`) with no
      documented rationale and no consumer anywhere in the workspace
      — the only test that referenced it (`on_change_fires_for_every_mutation`)
      existed to verify the field's existence rather than any feature
      that depended on it. Most likely added during the initial port
      for symmetry with `Editor::on_change` (which *is* in pi-tui),
      not as a deliberate response to a use case. Removed the field,
      its initializer, the eight `on_change` callsites in the
      mutation methods (`insert_char`, `backspace`, `delete_forward`,
      `kill_word_backward`, `kill_word_forward`, `kill_to_end`,
      `kill_to_start`, `yank`, `yank_pop`, paste arm), and the
      `on_change_fires_for_every_mutation` test. If a future caller
      actually needs a "value changed" hook, add it back then with
      a real consumer driving the design.
- [x] F38: `Input::yank_pop` now mirrors pi-tui's structural shape:
      `pushUndo()` at entry (after the `lastAction !== "yank"` /
      ring-len-< 2 guards) and `lastAction = "yank"` at exit. The
      visible behavior change: undo after a yank-pop reverts the
      rotation as its own step (back to the previously-yanked
      content) instead of collapsing all the way to the pre-yank
      state. Surfaced during the F22 pi-side cross-walk of every
      `lastAction` setter; the trailing reassertion is a defensive
      structural mirror — currently nothing in the rotate/insert
      body mutates `last_action`, but pi's invariant is that
      yank-pop ends in the "yank" state and the explicit set
      protects future drift. Regression tests in
      `tests/text_input.rs`
      (`undo_after_yank_pop_reverts_the_rotation_as_its_own_step`,
      `undo_after_chained_yank_pops_steps_back_one_rotation_at_a_time`)
      walk `kill, yank, yank-pop, undo` and the chained
      `kill×3, yank, yank-pop×2, undo×3` form. Note that pi's
      `undo` clears `lastAction` to null the same way ours does,
      so a follow-up Alt+Y after an undo does *not* see
      `lastAction === "yank"`; the F38 description's "would still
      see lastAction === 'yank' and rotate again" framing
      overstated that case. The actual visible invariant is the
      undo-step granularity, which the new tests pin down.

#### Loader

- [x] F25: `Loader::render` now mirrors pi-tui's
      `Loader extends Text("", paddingX=1, paddingY=0)` shape: an
      embedded [`Text`] with `padding_x = 1, padding_y = 0` renders the
      spinner+message body, and the loader prepends a leading blank
      row (`["", ...super.render(width)]`). Visible fixes: the
      spinner glyph is indented one column from the left, every body
      row is right-padded to the full render width (so a Loader sitting
      between styled rows doesn't leak the previous terminal contents
      on its right side), and a long message wraps at `width - 2 *
      padding_x` instead of overflowing past the terminal edge.
      Verbatim-frame and message-style behavior are unchanged: the
      composed body string still bypasses `spinner_style` when verbatim
      and still applies `message_style` to the message half. Existing
      `tests/loader.rs` shape assertions updated to allow the
      padding-x leading space and the trailing width pad. New
      regression tests in `tests/loader.rs`
      (`loader_indents_spinner_one_space_to_match_text_padding_x_1`,
      `loader_pads_each_row_to_the_full_render_width`,
      `long_message_wraps_to_render_width_minus_padding_x_pair`).
      `CancellableLoader` inherits the fix automatically since its
      `render` delegates to the wrapped `Loader`.
- [x] F26: Loader `spinner_style` and `message_style` aligned with
      pi-tui's required-at-construction shape. `Loader::new` now
      takes `(handle, spinner_style, message_style, message)` matching
      pi-tui's `new Loader(ui, spinnerColorFn, messageColorFn,
      message, indicator?)`. The `set_spinner_style` /
      `set_message_style` setters are gone (pi has no equivalent —
      pi takes the closures as immutable constructor args).

      `Loader::with_identity_styles(handle, message)` and
      `CancellableLoader::with_identity_styles(handle, message)`
      land as Rust-side test conveniences: they fill in identity
      closures for callers that don't need themed output, avoiding
      the per-site `Box::new(|s| s.to_string())` boilerplate that
      makes test code unreadable. Pi-tui's tests don't need this
      because JS arrow-function literals (`(s) => s`) are ergonomic;
      Rust closure boxing is noisy enough that the helper is worth
      the small API-surface addition.

      `CancellableLoader` mirrors the change: required styles in
      `new`, `with_identity_styles` for test convenience, no
      forwarding setters.

      The internal `spinner_style` / `message_style` fields drop
      their `Option<Box<dyn Fn>>` wrapping (always set, no fallback
      to identity-in-render needed).

      Migration count: 9 sites in `tests/cancellable_loader.rs`,
      ~16 sites in `tests/loader.rs`, plus the `examples/chat_simple.rs`
      `Loader` construction. Three of the loader tests
      (`verbatim_indicator_skips_spinner_style_so_pre_styled_frames_arent_double_wrapped`,
      `non_verbatim_indicator_still_applies_spinner_style`,
      `set_indicator_none_clears_the_verbatim_flag`) genuinely test
      spinner-style behavior and now pass the bold-wrapper closure
      at construction instead of via setter. All other test sites
      use `with_identity_styles`.

      Cross-references: F39 (the synchronous render-request points
      that previously needed `request_repaint` calls in the now-
      removed setters; those calls disappear with the setters).
      F19 / F19-followup (the broader pattern of moving themed
      components to required-at-construction shape; F19 noted
      Loader as "theme-less in pi-tui — nothing to plumb," but
      pi's Loader does have **two required styling closures**,
      which F26 now honors).
- [x] F39: Loader synchronous-render-request points landed.
      Mirrored pi-tui's `updateDisplay → requestRender` chain by
      adding a private `Loader::request_repaint` and calling it at
      the tail of `Loader::new`, `set_message`, `set_indicator`, and
      `start` (every point where pi-tui calls `updateDisplay`).
      Also called from `set_spinner_style` and `set_message_style`
      as a Rust-side addition: pi takes the spinner/message color
      functions as immutable constructor arguments and has no
      equivalent setters, so there's nothing in pi to mirror, but
      a style change is observable on the next paint and the
      same "request a render so it's visible" rule applies.

      Order note: pi's `start()` is `updateDisplay(); restartAnimation();`
      (request first, then schedule the pump). My Rust `start` and
      `set_indicator` reverse the order — `restart_animation_pump()`
      first, then `request_repaint()`. Both operations are
      non-blocking and have no inter-dependency (the pump spawns
      a tokio task, `request_render` sends on a channel), so the
      order is observably identical and not worth a code change.

      Detached render handles silently drop the request per H6's
      [`RenderHandle::detached`] contract, so standalone callers
      continue to function as cheap no-ops. The animation pump's
      per-tick `request_render` call is unchanged — that one was
      already correct; F39 was about the *synchronous* state-change
      paths that pi-tui covers via `updateDisplay`.

      Visible fix: a static or empty-frame loader now repaints on
      `set_message`, `set_indicator`, and construction even on an
      idle Tui with `set_initial_render(false)`. Previously the
      message change would stay invisible until something else
      triggered a render, and a freshly-constructed multi-frame
      loader appeared one `interval` late (first pump tick).

      Regression tests in `tests/loader.rs`
      (`set_message_on_a_static_loader_requests_a_render`,
      `set_indicator_swap_requests_a_render_even_when_new_indicator_is_static`,
      `freshly_constructed_loader_requests_a_render_with_initial_render_disabled`).
      Each test was confirmed to fail without the fix and pass with
      it. Two pre-existing tests
      (`loader_animation_pump_does_not_run_for_static_single_frame_indicator`,
      `loader_animation_pump_stops_when_loader_stop_is_called`)
      were updated to drain the construction-time Render via
      `tokio::time::timeout(32-64ms, tui.next_event())` before
      asserting "no further events" — the prior `support::async_tui::drain_ready`
      helper uses a 0ms timeout that never lets the throttle window
      elapse under paused tokio time, so its drain was always a
      no-op for events gated on the throttle.

#### Markdown

- [x] F27: Markdown tab → 3-spaces normalization at parse time.
      `Markdown::render` now runs `self.text.replace('\t', "   ")`
      on the source string before passing it to `parse_markdown`,
      mirroring pi-tui's
      `const normalizedText = this.text.replace(/\t/g, "   ")` step
      in `markdown.ts:135-136`. Three spaces matches both pi-tui's
      chosen visible width and the `Text` component's
      [`TAB_AS_SPACES`] constant (F14). Visible fix on two paths:
      (1) tab-indented fenced-code-block bodies — a literal `\t`
      previously survived the parser and the rendered cell, now it
      expands to the expected three-space indent; (2) tab-indented
      list nesting — `indent_of` only counts space bytes, so a tab-
      indented continuation marker was previously parsed as a new
      top-level item rather than a nested one. The cache key
      (`cached_text`) still holds the *original* text so an
      unchanged input still hits the cache; normalization is
      idempotent, so a hit returns the same result we'd produce by
      re-normalizing. Module-level `TAB_AS_SPACES` const documents
      the pi origin in place. Regression tests in
      `tests/markdown.rs`
      (`tab_in_fenced_code_block_body_is_normalized_to_three_spaces`,
      `multiple_tabs_in_code_block_each_expand_to_three_spaces`,
      `tab_indented_list_continuation_is_recognized_as_nested`).
      Each test was confirmed to fail without the fix and pass with
      it.
- [x] F28: Markdown headings now wrap with `theme.bold` separately
      from `theme.heading`, mirroring pi-tui's
      `markdown.ts:296-308`:
      H1 renders as `theme.heading(theme.bold(theme.underline(text)))`,
      H2+ as `theme.heading(theme.bold(text))`. Previously the Rust
      port wrapped only `theme.heading(theme.underline(text))` for H1
      and `theme.heading(text)` for H2+, so a custom theme whose
      `heading` provided only a color (the pi convention; the
      framework's default theme also uses bold-only `heading`, so the
      drift was invisible against `MarkdownTheme::default()`) lost the
      bold styling on every heading level. The H3+ prefix `### `
      shares the wrap with the body, so the prefix and trailing
      content stay in lockstep. Comment block on the heading branch
      now documents the pi alignment explicitly. Regression tests in
      `tests/markdown.rs`
      (`h1_renders_as_bold_even_when_theme_heading_is_only_a_color`,
      `h2_renders_as_bold_even_when_theme_heading_is_only_a_color`,
      `h3_renders_as_bold_even_when_theme_heading_is_only_a_color`,
      `h1_inline_code_still_reopens_bold_underline_and_color_after_reset`)
      build a theme where `heading` is only `style::cyan` (no bold)
      and assert that the bold escape `\x1b[1m` appears on the
      rendered heading at every level. The H1 inline-code test
      composes with F9 to confirm bold + underline + cyan are all
      reopened on the trailing text after an inline-code reset. Each
      test was confirmed to fail without the fix and pass with it.
- [x] F29: `<hr>` length basis. **No code change required — the
      apparent drift is a variable-naming artifact in pi-tui.** Pi's
      `renderToken(width, ...)` is called from `render(width)` with
      `contentWidth = max(1, width - paddingX * 2)` as the second
      argument (`/tmp/pi-mono/packages/tui/src/components/markdown.ts:147`),
      so the local `width` parameter inside `renderToken` is already the
      content width. Line 420 emits
      `─.repeat(Math.min(width, 80))` where that local `width` is
      `contentWidth`, so pi's hr is `min(content_width, 80)` cells wide
      — byte-for-byte what our Rust port already does at
      `src/components/markdown.rs` (`Block::HorizontalRule` arm,
      around line 1219). The PORTING.md note that "original uses full
      `width.min(80)`" was based on reading the local parameter name in
      isolation. Added a comment block on the Rust arm flagging the
      pi alignment so a future reader doesn't re-trigger the same
      investigation, plus three regression tests in `tests/markdown.rs`
      (`horizontal_rule_caps_at_eighty_visible_columns_when_content_width_is_wider`,
      `horizontal_rule_uses_content_width_when_narrower_than_eighty`,
      `horizontal_rule_subtracts_padding_x_pair_from_render_width_for_cap`)
      pinning the content-width-not-render-width axis explicitly across
      `padding_x = 0` / `padding_x = 4` and content widths above and
      below the 80-cell cap.
- [x] F30: Trailing-blank-line trim policy. Documented in
      `src/aj-tui/src/components/markdown.rs` (the policy comment block
      above the trim loop at the tail of `Markdown::render`) and locked
      in by regression tests in `tests/markdown.rs` under
      "Trailing-blank-line trim policy (PORTING.md F30)".

      Decision: keep the trim, document the (narrow) divergence from
      pi-tui. Each [`Markdown::render_block`] arm unconditionally
      appends a single `String::new()` so blocks separate cleanly in
      the source-driven case; pi-tui's renderer is next-token-aware
      (marked emits explicit `space` tokens between blocks and pi only
      emits `""` when the next token isn't `space`). Our parser
      collapses blank source lines into nothing — there's no "next-
      token type" available to gate the trailing emission — so
      unconditional emit + post-trim is the structurally cleaner shape
      for our AST.

      Empirical cross-check against pi-tui (marked@15.0.12, traced via
      `marked.lexer` + a faithful reimplementation of pi's `renderToken`
      switch). Three buckets:

      | source                                  | marked tokens                  | pi output           | our output      | diverge?      |
      | --------------------------------------- | ------------------------------ | ------------------- | --------------- | ------------- |
      | `"hello"` / `"hello\n"`                 | `[paragraph]`                  | `["hello"]`         | `["hello"]`     | no            |
      | `"hello\n\n"` / `"\n\n\n"` / `"\n\n\n\n"` | `[paragraph, space]`         | `["hello", ""]`     | `["hello"]`     | yes, 1 row    |
      | `"# title\n\n\n"`                       | `[heading]`                    | `["title"]`         | `["title"]`     | no            |
      | `` "```js\nfoo\n```\n\n\n" ``           | `[code, space]`                | `["…", "…", "…", ""]` | `["…", "…", "…"]` | yes, 1 row |
      | `"alpha\n\n\n\nbeta\n"`                 | `[paragraph, space, paragraph]` | `["alpha","","beta"]` | `["alpha","","beta"]` | no    |
      | `"alpha\n\nbeta\n\n\n\n"`               | `[paragraph, space, paragraph, space]` | `["alpha","","beta",""]` | `["alpha","","beta"]` | yes, 1 row |

      So the divergence is **exactly one row** and **only** when the
      document ends with 2+ trailing `\n`s after a non-heading block.
      Headings absorb trailing `\n`s into marked's `raw` field, so no
      trailing `space` token is emitted there and pi behaves exactly
      like us. Inter-block spacing is unaffected by the trim — the
      trim only fires at the end of the document.

      Visible UX trade-off. The chat surface this crate ships into
      prefers the trimmed shape — trailing typing artifacts in an LLM-
      emitted message read as dead space at the bottom of the rendered
      cell, not intentional structure. Mirrors F11's same-direction
      divergence that chose chat-surface UX over literal CommonMark
      fidelity. The cost is bounded: at most one row per render, on a
      narrow input shape.

      Bottom padding still works — the trim runs *before* `padding_y`
      is applied, so a Markdown with `padding_y = 2` and a trailing-
      blank-rich input emits exactly 2 + content + 2 rows (pi would
      emit one extra from the source-trailing `space` token). Cross-
      block invariants (the `does_not_add_a_trailing_blank_line_when_*_is_last`
      family already covering headings, paragraphs, code blocks,
      blockquotes, tables, and horizontal rules) keep working
      unchanged. New F30 tests cover the divergence and the
      look-alike-but-not cases:

        - `paragraph_with_one_trailing_newline_renders_without_a_trailing_blank_row` (no divergence)
        - `paragraph_with_multiple_trailing_blank_lines_renders_without_trailing_blank_rows` (one-row divergence)
        - `document_ending_with_heading_then_blank_lines_emits_no_trailing_blank_row` (no divergence — marked absorbs)
        - `padding_y_is_applied_after_trailing_blank_trim` (one-row divergence with explicit vertical padding)
        - `two_paragraphs_separated_by_blank_lines_keep_exactly_one_blank_between` (no divergence — inter-block spacing unaffected)
- [x] F40: Markdown empty / whitespace-only input check is narrower
      than pi-tui's. pi-tui's render guard is
      `!this.text || this.text.trim() === ""`
      (`/tmp/pi-mono/packages/tui/src/components/markdown.ts:126`),
      so any whitespace-only input (including all-tab or blank-line
      strings) returns an empty result without parsing. Our previous
      `Markdown::render` only checked `self.text.is_empty()`, so a
      string of tabs / spaces / newlines fell through to
      normalization + parse + an empty-block-list render.

      **Resolution.** Tightened the early-return guard to
      `self.text.trim().is_empty()` (covers both the truly-empty and
      whitespace-only cases — `trim().is_empty()` is true for both)
      and populated `cached_text / cached_width / cached_lines`
      before returning the empty vec, byte-for-byte mirroring pi's
      `markdown.ts:127-132`. Also reordered `Markdown::render` so
      the cache check runs *before* the empty-text guard (matching
      pi's `markdown.ts:117-120` → `markdown.ts:126` order). The
      reorder is what makes the cache-write actually deliver: a
      repeated whitespace-only render now hits the cache check on
      the second call and returns the cached empty vec without
      re-running `trim()`. With the guard placed first, the
      cache-write would be rewritten every call but never hit.
      Mirrors the analogous F14 fix on the `Text` component.

      Note: this only fixes the early-return cache write. The
      analogous tail cache write for non-empty input is tracked as
      F43 below.

      Regression tests in `tests/markdown.rs` under "Empty /
      whitespace-only input (PORTING.md F40)":

      - `whitespace_only_text_is_treated_as_empty` walks `""`,
        `"   "`, `"\n\n"`, `"\t\t"`, `" \n\t "`, and a mixed-
        whitespace string; each must render as `Vec::<String>::new()`.
        Analogous to `tests/text.rs::whitespace_only_text_is_treated_as_empty`.
      - `whitespace_only_text_with_padding_y_still_renders_zero_rows`
        confirms the early-return runs before the top-padding loop
        (a whitespace-only input with `padding_y = 3` still emits
        zero rows, not three blank padding rows).
      - `repeat_whitespace_only_render_returns_cached_empty_result`
        calls `render(80)` three times on the same whitespace-only
        input and asserts identical empty vecs every call,
        exercising the cache-hit path.
- [x] F41: Markdown inline-style context for outer-style restoration
      across non-text inlines. **F9 follow-up; surfaced during the F28
      cross-walk against pi.**

      Ported pi-tui's `InlineStyleContext` machinery
      (`markdown.ts:73-76`) into `src/components/markdown.rs`:

      - New `InlineStyleContext<'a>` struct with
        `apply_text: &'a dyn Fn(&str) -> String` and
        `style_prefix: &'a str`. Borrowed references rather than owned
        boxes so each block-render builds the context locally without
        per-allocation overhead.
      - New `get_style_prefix(wrap)` free function mirroring pi's
        `getStylePrefix` (`markdown.ts:273-279`). Calls `wrap("\u{0}")`
        and slices everything before the NUL byte. Returns `""` when
        the closure swallows the sentinel (defensive — not expected
        for any real wrapper).
      - New `Markdown::render_inline_tokens(inlines, ctx)` mirroring
        pi's `renderInlineTokens` (`markdown.ts:448-541`): text runs
        use `apply_text` (split-and-rejoin per `\n` like pi's
        `applyTextWithNewlines`); non-text inlines (bold, italic,
        strikethrough, code, link) emit their own per-variant styling
        and append `style_prefix` after; trailing dangling
        `style_prefix` is trimmed at the end. Nested inlines recurse
        with the same context, mirroring pi's pass-through.
      - `Markdown::render_inlines` is now a thin wrapper around
        `render_inline_tokens` with an identity context. Callers that
        don't have an outer style to thread (table cells, list items,
        blockquote internals) keep using it unchanged.
      - Removed `Markdown::render_inline` and
        `Markdown::render_inlines_with_outer`. The first folded into
        the per-variant arms of `render_inline_tokens`; the second is
        replaced by the heading/paragraph context paths.

      Branch migrations:

      - **Heading** (the F28 / F9 path): builds a `heading_apply`
        closure of the F28 formula (H1 = `heading(bold(underline(t)))`,
        H2+ = `heading(bold(t))`), extracts its style_prefix, and
        renders inlines through that context. Visible change: an inline
        code inside a heading like `# foo \`bar\` baz` no longer gets
        the heading style baked into the inline content; just the
        inline's own `theme.code` styling, with the heading style
        re-emitted *between* the inline and the trailing text. Existing
        F9 / F28 tests continue to pass because they assert opens
        between code-end and trailing-prose, and pi's machinery
        produces those opens via the post-inline style_prefix re-emit.
      - **Paragraph** (the pre-style path): builds a `pre_apply`
        closure that calls `apply_pre_style`, extracts its prefix when
        `pre_style` is set (otherwise the prefix is `""` and the
        machinery degrades to identity), and threads it through the
        inline walk. **Visible fix**: the configured `pre_style`
        (gray + italic for thinking traces) now restores on text
        following an inline reset. Previously the pre-style wrapped
        the whole paragraph string once at the end, so an inline
        code's `\x1b[39m` reset stripped the gray on subsequent text
        until the paragraph's own outer close fired at the very end.
      - **Blockquote** (the F12 path): kept the F12 per-line
        `theme.quote` outer-wrap rather than migrating to pi's
        `quoteInlineStyleContext` / `applyQuoteStyle` line-replace
        path. Inline-comment block on the blockquote arm enumerates
        three structural deltas with bounded visible impact:
        (1) pi always wraps with `theme.italic` under `theme.quote`;
        we rely on `theme.quote` itself carrying italic (matches our
        default theme but diverges for custom non-italic
        `theme.quote`); (2) pi splices the quote prefix after every
        `\x1b[0m` (full SGR reset, emitted by syntect), we don't —
        affects code-block-in-blockquote tails; (3) pi appends the
        quote prefix after each non-text inline via the inline
        context, we rely on the outer `theme.quote` wrap surviving
        inner set-affecting closes (which it does for `\x1b[39m` and
        `\x1b[22m`, the only closes our inlines emit). Default-theme
        + plain paragraphs/headings/lists inside `>` match pi cell-
        for-cell; migration to pi's full machinery is deferred until
        a real-input regression surfaces.

      Scope decision: `pre_style` only threads through the paragraph
      branch. Lists, tables, and blockquote internals stay on the
      identity context, matching the existing `PreStyle` rustdoc
      contract ("Lists currently don't receive pre-styling either").
      Headings keep an outer `apply_pre_style` wrap on the rendered
      heading line for backward-compat with the existing aj-tui API
      contract; pi's heading branch doesn't apply the
      `defaultTextStyle` and the outer wrap is observably equivalent
      for plain-text headings (the only case where the outer wrap is
      visible — a heading with inlines loses the pre-style on inline
      resets either way, since pre-style isn't in the heading's
      inline context). Documented in the heading branch's comment
      block.

      Regression tests in `tests/markdown.rs` under "Inline-style
      context (PORTING.md F41)":

      - `pre_styled_paragraph_reopens_gray_italic_after_inline_code`
        (the main F41 fix — gray + italic re-open between inline
        code's content and trailing text).
      - `pre_styled_paragraph_reopens_gray_italic_after_bold_span`
        (same shape for `**bold**` inlines).
      - `pre_styled_paragraph_with_no_inlines_still_wraps_text_in_gray_italic`
        (regression guard: pure-text paragraph with pre-style still
        produces gray + italic).
      - `pre_styled_paragraph_ending_with_inline_code_trims_dangling_prefix`
        (trim invariant: the rendered output doesn't end with a
        dangling pre-style prefix when the last inline is a non-text
        token).
      - `h1_inline_code_keeps_its_own_styling_only_not_double_wrapped_with_heading`
        (visible change on the heading branch: inline code is no
        longer wrapped with the heading style; the post-inline byte
        is the heading style_prefix re-emit, not the heading wrap's
        underline-off close).
      - `h1_inline_code_at_end_of_heading_trims_dangling_heading_prefix`
        (trim parallel for the heading branch: rendered heading
        doesn't end with a stray heading style_prefix).
- [x] F42: Markdown degenerate-width handling. **Surfaced during the
      F29 cross-walk against pi.** Pi-tui's `Markdown.render` (line 123
      of `/tmp/pi-mono/packages/tui/src/components/markdown.ts`)
      computes `contentWidth = Math.max(1, width - this.paddingX * 2)`,
      so any degenerate render width — `width = 0`, or
      `width < 2 * paddingX` — clamps `contentWidth` to `1` and renders
      the document into a one-cell-wide column (everything wraps
      aggressively, but content still appears). Our Rust port previously
      computed `content_width = width.saturating_sub(self.padding_x * 2)`
      and short-circuited to `Vec::new()` when `content_width == 0`, so
      the same input rendered as zero rows.

      **Resolution.** Switched the `content_width` computation in
      `Markdown::render` to
      `width.saturating_sub(self.padding_x * 2).max(1)` and dropped the
      `content_width == 0` early return. The downstream paths
      (`render_block`, `render_list`, table rendering, hr, paragraph
      wrap) all accept `width = 1` gracefully —
      [`wrap_text_with_ansi`][aj_tui::ansi::wrap_text_with_ansi] breaks
      long words one grapheme per row, hr emits a one-cell `─`, and
      bullets / quote borders reduce the inner width via their own
      saturating subtractions. Cache parity (F42 sub-decision 2) needed
      no extra work: once `content_width` reaches `1`, the regular F43
      cache write at the tail of `render` fires for the degenerate path
      same as for any other input. F34 (the same divergence pattern on
      `TextBox`) is still open; F14 / F40 / F42 keep `Text` / `Markdown`
      on the "degenerate width clamps to 1, never returns empty" rule.

      Regression tests in `tests/markdown.rs` under "Degenerate render
      width (PORTING.md F42)":

      - `render_at_zero_width_clamps_content_width_to_one_cell`:
        `md("# hello").render(0)` returns a non-empty vec, every row's
        `visible_width` is `<= 1`, and the heading body letters survive
        the per-grapheme break.
      - `render_with_padding_x_exceeding_half_width_clamps_to_one_cell`:
        `padding_x = 8, width = 10` (the `width < 2 * padding_x` shape)
        also clamps to a one-cell content column; rows stay within
        `padding_x + 1` visible cells.
      - `repeat_degenerate_width_render_returns_cached_result`: three
        consecutive `render(0)` calls on the same input return the
        same vec, exercising the F43 tail cache write on the
        degenerate path.

      Each test was confirmed to fail without the `.max(1)` clamp and
      pass with it.

      **Follow-up: F44** (open) tracks the related per-arm inner-width
      clamps that pi has and we don't (blockquote `Math.max(1, width
      - 2)`, table cell wrap `Math.max(1, maxWidth)`, plus the
      structural divergence in `renderList`). All three pre-existed
      F42 but were unreachable through the outer `content_width == 0`
      short-circuit; F42's clamp makes them reachable for
      content-width-1 inputs. Output is graceful (no panics) but not
      byte-for-byte aligned with pi at degenerate widths. See F44 for
      the resolution plan.
- [x] F43: Markdown render-tail cache write. **Surfaced during the
      F40 cross-walk against pi.** Pi-tui's `Markdown.render`
      populates the cache at the *tail* of the function as well as on
      the empty-text branch (`/tmp/pi-mono/packages/tui/src/components/markdown.ts:196-199`,
      after combining top padding + content + bottom padding into
      `result`). Our previous Rust port had the cache fields, the
      cache-check at the top of `render`, and (after F40) a
      cache-write on the empty-text early return — but the non-empty
      path never wrote the cache at the tail. Result: the cache-check
      at the top was effectively dead code for any non-whitespace
      input. Every `render(width)` call on a normal markdown document
      re-ran `replace('\t', "   ")` + `parse_markdown` + the per-block
      render loop + the F30 trim, even when the inputs hadn't changed.

      **Resolution.** Added the cache write at the tail of
      `Markdown::render` (after the F30 trim and the bottom-padding
      loop), storing the pre-fallback `result`. Added the `[""]`
      fallback (pi `markdown.ts:201`) on the return path: a non-empty
      input that produces zero rendered rows still emits a single
      blank row so the component occupies one cell. The fallback is
      defensive in our port — every `parse_markdown(non-whitespace)`
      returns at least one block, and every `render_block` arm emits
      at least one line plus a separator, so this branch is
      unreachable for real inputs; kept for byte-level pi parity.

      The cache stores `result` *before* the `[""]` fallback — same
      asymmetry F14 documented for `Text`: a cache-hit returns
      whatever was computed verbatim (potentially the empty vec),
      only the first-call return path runs through the fallback.

      Verified that every existing setter that mutates render-affecting
      state (`set_text`, `set_padding_x`, `set_padding_y`, `set_theme`,
      `set_pre_style`, `set_hyperlinks`) calls `invalidate_cache()`,
      so a cache hit is only possible when the cached
      `(text, width)` is genuinely valid.

      **Tests.** Four in-module unit tests in
      `src/aj-tui/src/components/markdown.rs::tests`
      (private-field access — integration tests can only observe
      behavior, not the cache state directly):
      - `render_populates_cache_at_tail_for_non_empty_input`: cache
        is `None` before render; after `render(80)` on `"# hello"`,
        `cached_text`, `cached_width`, and `cached_lines` are all
        populated and match the returned vec.
      - `second_render_with_same_inputs_returns_cached_result`:
        repeat `render(80)` on a multi-block input returns equal
        results.
      - `set_text_invalidates_cache_so_next_render_reflects_mutation`:
        `set_text` clears the cache fields to `None`; the next
        render returns a different result and repopulates the cache
        for the new text.
      - `render_at_a_different_width_misses_the_cache_and_repopulates`:
        rendering the same text at `width = 20` then `width = 80`
        produces different wrapping and writes a fresh cache entry
        for the new width.

      Skipped the `[""]` fallback test because no real input is
      constructable that triggers it — the non-empty path always
      produces at least one block which always renders at least one
      line. Kept the fallback in code as defensive parity with pi.
- [x] F44: Markdown per-arm inner-width clamps and table fallback
      (blockquote, table, list architectural shape). **Surfaced during
      the F42 cross-walk against pi, refined during the post-landing
      pi cross-check.** **(A) blockquote clamp landed; (A2) table
      fallback landed; (B) list refactor explicitly deferred below.**

      The F42 fix aligned the outermost `content_width` computation in
      `Markdown::render` (`markdown.rs:123` in pi) byte-for-byte with
      pi. While confirming downstream paths handle `content_width = 1`
      gracefully, three additional divergences surfaced in pi (lines
      from `/tmp/pi-mono/packages/tui/src/components/markdown.ts`):

      1. **Blockquote inner width** (`markdown.ts:382`):
         ```ts
         const quoteContentWidth = Math.max(1, width - 2);
         ```
         Our blockquote arm computed
         `inner_width = content_width.saturating_sub(border_width)`
         without the `.max(1)` clamp. At outer `content_width = 1`
         (now reachable via F42), pi recurses with `quoteContentWidth
         = 1` while we recursed with `inner_width = 0`. Visible effect:
         the inner paragraph wrap at `width = 0` prepends a leading
         empty line, which the blockquote loop turns into a bare `│ `
         row before the content rows.

      2. **Table fallback to raw markdown** (`markdown.ts:696-703`):
         ```ts
         if (availableForCells < numCols) {
             const fallbackLines = token.raw ? wrapTextWithAnsi(token.raw, availableWidth) : [];
             ...
             return fallbackLines;
         }
         ```
         When the table chrome alone (`3 * n_cols + 1` chars of
         border + cell padding) consumes the full `content_width`, pi
         falls back to wrapping the original markdown source through
         `wrapTextWithAnsi` instead of attempting to render a table
         whose chrome is wider than its content. Pi's
         `Math.max(1, maxWidth)` inside `wrapCellText`
         (`markdown.ts:672`) is a separate defensive clamp at the
         per-cell wrap point; on its own it's a no-op for our port
         because `distribute_column_widths` already guarantees per-
         column widths `>= 1`. The actual user-visible divergence is
         the missing fallback: our port rendered the table with chrome
         wider than the content area at degenerate widths.

      3. **List architectural shape** (deeper than a clamp). Pi's
         `renderList` (`markdown.ts:546`) doesn't take a `width`
         parameter at all — it builds bullet-prefixed lines without
         wrapping, and the outer `render` pass at line 157 wraps every
         line through `wrapTextWithAnsi(line, contentWidth)`. Our
         `render_list` takes `content_width` and does its own wrap at
         a reduced `text_width = content_width - indent - bullet`,
         then prepends the bullet to the first line. Both
         architectures produce equivalent output at non-degenerate
         widths; at degenerate widths they diverge structurally.

      **Reachability before vs. after F42.** All three divergences
      pre-existed F42 in the source. Before F42, the outer
      `content_width == 0` short-circuit prevented the recursive paths
      from ever being called at content_width <= 2 (the only widths
      where the divergences observably differ from pi). With F42, the
      outer clamp lands at `content_width = 1` for degenerate inputs,
      so the inner divergences are now reachable.

      **Resolution.** Two-axis decision:

      (A) **Blockquote inner-width clamp. Landed.** The blockquote
      arm now ends in `.max(1)`:
      ```rust
      // blockquote arm (`Block::Blockquote` branch in `render_block`)
      let inner_width = content_width.saturating_sub(border_width).max(1);
      ```
      For non-degenerate widths (`content_width >= 3`) the `.max(1)`
      is a no-op so the common path stays identical. Only the
      degenerate path changes, mirroring pi. Regression tests in
      `tests/markdown.rs`
      (`blockquote_at_zero_width_recurses_with_one_cell_inner_width`,
      `repeat_degenerate_width_blockquote_render_returns_cached_result`).

      (A2) **Table fallback to raw markdown. Landed.** `Block::Table`
      now carries a `raw: String` field populated by `parse_table`
      with the original `lines[start..i].join("\n")` source. The
      `render_table` arm computes
      `available_for_cells = content_width.saturating_sub(chrome)` and,
      when `available_for_cells < n_cols`, returns
      `wrap_text_with_ansi(raw, content_width)` plus the standard
      trailing blank instead of rendering a table. Mirrors
      `markdown.ts:696-703`. The `Math.max(1, maxWidth)` per-cell
      clamp in pi's `wrapCellText` is a defensive guard that's a
      no-op for our port (our `distribute_column_widths` already
      clamps per-column widths to `>= 1`); not ported. Regression
      tests in `tests/markdown.rs`
      (`single_column_table_at_zero_width_falls_back_to_raw_markdown`,
      `two_column_table_at_zero_width_falls_back_to_raw_markdown`,
      `single_column_table_falls_back_when_width_equals_chrome`,
      `repeat_degenerate_width_table_render_returns_cached_result`),
      verified by toggling the fallback gate off and confirming all
      three render-time tests fail.

      (B) **List architectural alignment. Deferred.** Larger refactor:
      move `render_list` to a width-agnostic shape and have `render` /
      `render_block` apply `wrap_text_with_ansi(line, content_width)`
      as a single post-pass over every block's output. This is a
      bigger change (the bullet-on-first-line rule has to survive the
      wrap, which means the wrap pass needs to know about list
      continuation indentation), and may not be worth the churn given
      the only observable difference is at content_width <= 2. Defer
      until there's a concrete user-visible regression.

      **Tests.** (A) and (A2) landed with the regression tests listed
      in their "Landed" blocks above. Each test was verified to fail
      when the corresponding fix is reverted, so they pin actual
      observable behavior rather than just smoke-tested no-op. For
      (B), re-run our existing list tests after the architectural
      change to confirm no non-degenerate regressions, plus add a
      degenerate-width parity test that asserts byte-for-byte
      equivalence with pi's wrap-everything-once shape.
- [x] F45: Heading branch outer `apply_pre_style` wrap. **Surfaced
      during the F41 pi cross-check.** Pi-tui's heading branch builds
      a `headingStyleContext` and passes it to `renderInlineTokens`,
      but does *not* thread `defaultTextStyle` (its analog of our
      `PreStyle`) through headings — `defaultTextStyle` only applies
      to paragraphs (and, via the default inline context, lists and
      tables).

      Our Rust port had `apply_pre_style(&styled)` on the heading
      branch from the initial port through F41. The divergence shape
      was "we apply pre-style to {paragraphs, headings}; pi applies
      defaultTextStyle to {paragraphs, lists, tables}". F45 closes
      the "we-extra-apply-on-headings" half; the
      "pi-extra-applies-on-lists/tables" half stays open as its own
      scope decision (no real-input demand has surfaced; the existing
      `PreStyle` rustdoc explicitly says "Lists and table cells
      currently don't receive pre-styling either; when a test port
      needs it, extend the render path").

      **Resolution.** Dropped the `apply_pre_style(&styled)` call on
      the heading branch of `Markdown::render_block` in
      `src/aj-tui/src/components/markdown.rs`. Updated the `PreStyle`
      rustdoc, the `Markdown::pre_style` field doc, and the
      `set_pre_style` rustdoc to read "applied to rendered paragraph
      lines" (drop the "and heading lines" phrase). Updated the
      heading-branch comment block to explain the F45 alignment with
      pi (heading wrap on text runs only, no outer pre-style).

      **Visible change.** With `pre_style = gray + italic`, a plain
      `# Heading` previously rendered with an outer
      `\x1b[90m\x1b[3m...\x1b[23m\x1b[39m` wrap on top of the heading
      style. After F45 it ships with just the heading style, matching
      pi byte-for-byte. For a heading with inlines (`# foo \`bar\` baz`),
      the outer wrap was structurally there but the inline code's
      `\x1b[39m` reset stripped the gray on `baz` anyway, so the
      visible difference is "outer gray on plain headings only".

      Regression tests in `tests/markdown.rs` under "Heading branch
      does not apply pre-style (PORTING.md F45)":

      - `pre_styled_text_does_not_wrap_headings`: a Markdown with
        gray + italic pre-style and `# My Heading` content renders
        without `\x1b[90m` (gray) or `\x1b[3m` (italic) anywhere in
        the rendered output. The heading body text still survives.
      - `pre_styled_text_wraps_paragraphs_but_not_headings_in_same_document`:
        regression guard against a silent degrade to "pre-style is
        broken everywhere". Builds a mixed-block document
        (`# Heading line\n\nParagraph line`), locates each rendered
        row by content, and asserts the heading row carries no
        pre-style codes while the paragraph row still carries both.

      Each test was confirmed to fail without the fix (heading-row
      assertions panic with the gray/italic open codes leading the
      rendered byte string) and pass with it.
- [x] F46: Blockquote machinery — port pi's `applyQuoteStyle` +
      `quoteInlineStyleContext`. **Surfaced during the F41 pi
      cross-check.** F41 documented three structural deltas in our
      blockquote arm relative to pi-tui's
      `markdown.ts:370-417`. F12 chose a simpler "outer
      `theme.quote` wrap per line" model; F41 confirmed cell-for-
      cell parity for default-theme + plain-content but found three
      conditions where byte paths diverge. F46 is the migration
      that closes them.

      **The three deltas (from the F41 cross-check).**

      1. **Italic underwrap.** Pi's `quoteStyle` is
         `(text) => theme.quote(theme.italic(text))` —
         `theme.italic` always wraps under `theme.quote`. We rely on
         `theme.quote` itself carrying italic. With our default
         `theme.quote = style::italic`, both paths visibly italicize.
         A custom non-italic `theme.quote` would render plain in our
         output but stay italic in pi's.

      2. **`\x1b[0m` splice on full SGR resets.** Pi's
         `applyQuoteStyle` does
         `line.replace(/\x1b\[0m/g, "\x1b[0m" + quoteStylePrefix)`
         before wrapping with `quoteStyle`. The replace re-opens the
         quote/italic style after every full SGR reset. Our
         syntect-backed `highlight_code` terminates each highlighted
         line with `\x1b[0m`, so a fenced code block inside a `>`
         quote loses the quote styling on its trailing cells in our
         output but keeps it in pi's.

      3. **Inline context for sub-block inlines.** Pi builds
         `quoteInlineStyleContext = { applyText: identity,
         stylePrefix: getStylePrefix(quoteStyle) }` and passes it
         down through every `renderToken` call inside the
         blockquote, so sub-block inline rendering re-emits the
         quote prefix after each non-text inline. We render sub-
         blocks via `render_block` (no quote context threaded), and
         the outer per-line `theme.quote` wrap survives inner set-
         affecting closes (`\x1b[39m` from inline code, `\x1b[22m`
         from bold) because those don't strip italic. So for our
         current inline set this is observably equivalent for the
         default theme; it would diverge if a future inline emitted
         `\x1b[0m` (full reset) or `\x1b[23m` (italic off) inline.

      **Landed.** All three deltas closed in
      `src/aj-tui/src/components/markdown.rs`:

      - New free function `apply_quote_style(line, quote_apply,
        quote_prefix)` next to the existing `get_style_prefix`,
        mirroring pi-tui's `applyQuoteStyle` (markdown.ts:373-379):
        replaces every `\x1b[0m` with `\x1b[0m{quote_prefix}` then
        wraps with `quote_apply`. Skipped when `quote_prefix` is
        empty (defensive — the sentinel trick in `get_style_prefix`
        returns `""` only when the wrapper swallows the sentinel,
        which neither `theme.quote` nor `theme.italic` does).
      - `render_block` split into a thin wrapper plus a new
        `render_block_in_context(block, content_width, ctx:
        Option<&InlineStyleContext>)` that takes the optional outer
        inline-style context. Mirrors pi's `renderToken(token,
        width, nextType, styleContext?)` signature
        (`markdown.ts:287-292`).
      - Paragraph branch consumes `ctx` directly when supplied
        (skipping the pre-style build), mirroring pi's
        `markdown.ts:325` (`renderInlineTokens(token.tokens,
        styleContext)`) plus the blockquote's
        `markdown.ts:386` comment ("Default message style should
        not apply inside blockquotes"). Heading branch keeps
        building its own heading context regardless of incoming
        `ctx`, byte-for-byte matching pi's heading at
        `markdown.ts:310-313`.
      - Blockquote branch builds the pi machinery: `quote_apply =
        |t| theme.quote(theme.italic(t))` (delta #1),
        `quote_prefix = get_style_prefix(quote_apply)`,
        `quote_inline_ctx = { apply_text: identity, style_prefix:
        &quote_prefix }` (delta #3). Sub-blocks render through
        `render_block_in_context(sub, inner_width,
        Some(&quote_inline_ctx))`. Each non-empty sub-block line
        flows through `apply_quote_style(line, quote_apply,
        quote_prefix)` (delta #2) before getting the border
        prepended.
      - Scope decision matches the F46 spec: paragraph forwards the
        incoming `ctx`; heading builds its own; lists, tables, code
        blocks, hr ignore `ctx` (the F46 spec text "paragraph and
        heading branches forward to `render_inline_tokens` with
        `ctx`" was misread of pi — pi heading at
        `markdown.ts:310-315` ignores incoming `styleContext` and
        builds `headingStyleContext` from scratch, so we match
        pi rather than the spec text). The narrower "list/table
        context-free" scope was closed in the F46-followup commit
        below: `render_list` and `render_table` now both take
        `ctx: Option<&InlineStyleContext>` and forward it to their
        per-item / per-cell `render_inline_tokens` calls (mirroring
        pi's `markdown.ts:357` and `:365`). A list or table inside
        a `>` quote now re-emits the quote prefix after each
        non-text inline at the bullet/cell level.

      **Tests.** Three regression tests in `tests/markdown.rs` under
      "Blockquote inline-context machinery + applyQuoteStyle
      (PORTING.md F46)":

      - `custom_non_italic_quote_theme_still_underwraps_with_italic`
        (delta #1): custom theme with `quote = style::cyan` (no
        italic). Renders `> hello`. Asserts both `\x1b[3m`
        (italic from the underwrap) and `\x1b[36m` (cyan from the
        quote) appear in the rendered output. Without the
        `theme.italic` underwrap the test sees only the cyan wrap.
      - `code_block_in_blockquote_reopens_quote_styling_after_syntect_reset`
        (delta #2): renders ` > ```rust\n> let x = 1;\n> ``` `.
        Locates the highlighted code body row by `strip_ansi`
        (syntect breaks the row into per-token color escapes so the
        plain-text "let x = 1" survives but raw substrings don't).
        Asserts the row contains `\x1b[0m\x1b[3m\x1b[3m` (the
        splice + default-theme quote_prefix). Without the splice
        the row ends in `\x1b[0m\x1b[23m\x1b[23m` (closes only).
      - `paragraph_inline_inside_blockquote_reopens_quote_styling_after_inline_code`
        (delta #3): cyan-quote theme so the re-emit is observable
        as a `\x1b[36m` open. Renders `> See `code` for more`.
        Walks past the inline code's `\x1b[39m` foreground reset
        and asserts a fresh `\x1b[36m` open follows before the line
        ends. Without the inline-context style_prefix, the
        post-reset bytes are just the trailing prose then the
        outer wrap's closes.

      Each test was confirmed to fail without its corresponding fix
      (italic underwrap toggled off, splice replaced with identity,
      `style_prefix` zeroed) and pass with all three together.

      **Cost.** ~50 lines of code (the `apply_quote_style` helper,
      the `render_block_in_context` rename + paragraph
      `Option<&InlineStyleContext>` consumer, the rebuilt
      blockquote arm) plus the three regression tests. Net is a
      strict superset of the F12 path: default-theme + plain
      content matches byte-for-byte; the deltas now match pi for
      every input the previous comment block flagged.

      **F46-followup pipeline order (markdown.ts:392-411).** A
      cross-check against pi's `case "blockquote":` flow surfaced
      three byte-level architectural deltas the F46 landing
      preserved from the F12 baseline. The followup commit closes
      all three by restructuring the blockquote arm into pi's
      explicit phased shape:

      1. **Wrap order.** Pi's flow is `applyQuoteStyle(line) →
         wrapTextWithAnsi(styled, inner_width) → border prepend`.
         Our previous F46 path applied quote style PER row from
         the sub-block's internal wrap (so each wrapped row got an
         independent wrap pair). The new path mirrors pi: collect
         sub-block lines → trim trailing blanks → for each line,
         apply quote style + wrap + prepend border. For
         already-fitting content the new wrap step is a no-op (the
         paragraph's internal wrap to `inner_width` covers the
         common case), but a sub-block whose line exceeds
         `inner_width` (e.g. a code-block row at a narrow render
         width) now wraps with ANSI state propagation instead of
         overflowing past the border.
      2. **Trim location.** Pi pops trailing `""` entries from
         `renderedQuoteLines` BEFORE applying quote style
         (`markdown.ts:402-404`). Our previous path dropped
         border-only rows AFTER applying quote style. New path
         pops blanks from the collected `quote_lines` vec
         pre-style; functionally equivalent for the previous test
         set but mirrors pi's order so a future change to quote
         styling can't accidentally trip the trim predicate.
      3. **Empty-line short-circuit.** Pi calls `applyQuoteStyle`
         on every remaining line (after the trim) including
         mid-quote blank rows. Our previous path emitted bare
         borders for empty lines as an optimization. New path
         routes every line through `apply_quote_style`; for an
         empty line that produces `quote_apply("")` =
         `\x1b[3m\x1b[3m\x1b[23m\x1b[23m` for the default theme,
         which the bordered output now carries (visible width
         identical, byte structure matches pi).

      **List/table context forwarding.** Pi forwards `styleContext`
      to `renderList` (`markdown.ts:357`) and `renderTable`
      (`markdown.ts:365`), threading the quote inline context down
      through per-item / per-cell `renderInlineTokens` calls. The
      F46-followup commit adds `ctx: Option<&InlineStyleContext>`
      to both `render_list` and `render_table`, and routes
      `render_block_in_context`'s `Block::UnorderedList` /
      `Block::OrderedList` / `Block::Table` arms through it. A list
      or table inside a `>` quote now re-emits the quote prefix
      after each non-text inline at the bullet / cell level —
      observable when the inline code is followed by trailing text
      in the same item or cell (the trailing `\x1b[39m` no longer
      strips the quote color).

      **Followup tests.** Five additional regression tests in
      `tests/markdown.rs`, each verified to fail when its
      corresponding fix is reverted:

      - `list_inside_blockquote_reopens_quote_styling_after_inline_code`
        (list context forwarding).
      - `table_inside_blockquote_reopens_quote_styling_after_inline_code_in_cell`
        (table context forwarding).
      - `mid_quote_blank_row_gets_empty_content_quote_wrap_not_bare_border`
        (empty-line short-circuit removed).
      - `long_code_block_line_inside_blockquote_wraps_with_quote_state_propagation`
        (wrap-after-applyQuoteStyle).
      - `trailing_blank_inside_blockquote_is_trimmed_before_apply_quote_style`
        (pre-trim location).
- [x] F47: `PreStyle::apply` field-ordering parity with pi's
      `applyDefaultStyle`. **Surfaced during the F41 pi
      cross-check.** Pi-tui's `applyDefaultStyle`
      (`/tmp/pi-mono/packages/tui/src/components/markdown.ts:210-237`)
      applies styles in this order, color innermost → underline
      outermost:

      ```ts
      let styled = text;
      if (color) styled = color(styled);
      if (bold) styled = theme.bold(styled);
      if (italic) styled = theme.italic(styled);
      if (strikethrough) styled = theme.strikethrough(styled);
      if (underline) styled = theme.underline(styled);
      ```

      Pre-F47 our [`PreStyle::apply`] in
      `src/aj-tui/src/components/markdown.rs` applied italic *first*
      then color, color outermost:

      ```rust
      let mut out = if self.italic { italic(s) } else { s.to_string() };
      if let Some(color) = &self.color { out = color(&out); }
      ```

      So:

      | input | pi (italic+color)         | ours pre-F47 (italic+color) |
      | ----- | ------------------------- | -------------------------- |
      | `t`   | `\x1b[3m\x1b[Cm{t}\x1b[39m\x1b[23m` | `\x1b[Cm\x1b[3m{t}\x1b[23m\x1b[39m` |

      Open order was flipped. Visible output is identical (italic +
      gray are both active for `t`), but the byte structure
      differed, and the F41 `style_prefix` extracted from each
      version differed too (`\x1b[3m\x1b[Cm` vs `\x1b[Cm\x1b[3m`).
      For the F41 trim invariant the difference was not observable
      since the trim does exact byte matching; but for byte-for-
      byte parity tests against pi the divergence showed up.

      **Note on PreStyle scope.** Our `PreStyle` only carries
      `color: Option<Arc<dyn Fn>>` and `italic: bool` — pi's
      `DefaultTextStyle` carries color, bgColor, bold, italic,
      strikethrough, underline. The pi-vs-ours field-set
      divergence is its own thing (we never needed bold /
      strikethrough / underline on the pre-style for the thinking-
      trace use case); F47 covers only the ordering of the fields
      we *do* have.

      **Landed.** Reordered `PreStyle::apply` to match pi's
      color-innermost shape:

      ```rust
      pub(crate) fn apply(&self, s: &str) -> String {
          let mut out = s.to_string();
          if let Some(color) = &self.color {
              out = color(&out);
          }
          if self.italic {
              out = crate::style::italic(&out);
          }
          out
      }
      ```

      Visible output unchanged. Byte structure flipped. The F41
      `pre_styled_paragraph_*` regression tests pin down that
      `\x1b[90m` and `\x1b[3m` are both present after each inline
      reset using `find` / `rfind` with no order constraint, so
      they passed under both orderings unchanged. The trim test
      `pre_styled_paragraph_ending_with_inline_code_trims_dangling_prefix`
      had a hardcoded `pre_prefix = "\x1b[90m\x1b[3m"`; updated to
      `"\x1b[3m\x1b[90m"` to keep the assertion meaningful (the
      trim removes whatever the actual prefix is, so the test
      passed either way, but the literal in the test comment now
      matches what the trim is actually stripping).

      **Tests.** New regression test in `tests/markdown.rs` under
      "PreStyle::apply field-ordering parity with pi (PORTING.md
      F47)":
      `apply_pre_style_open_order_matches_pi_default_text_style`
      finds the first `\x1b[3m` (italic open) and first `\x1b[90m`
      (gray open) in the rendered output and asserts the italic
      open precedes the gray open in the byte stream. Verified to
      fail with the pre-F47 italic-innermost code path (italic
      open at byte 5, gray open at byte 0 — the exact reverse) and
      pass with the F47 reorder. Updated the rustdoc on
      `PreStyle::apply` and the inline rustdoc on
      `Markdown::apply_pre_style` to read "color then italic, per
      pi-tui's applyDefaultStyle order" instead of the previous
      "italic then color".
- [x] F48: `Inline::Link` parser shape — support nested inlines in
      link text. **Surfaced during the F41 pi cross-check.**
      Pi-tui's link inline carries `tokens` (a
      `Token[]` array of nested inlines):

      ```ts
      case "link": {
          const linkText = this.renderInlineTokens(token.tokens || [], resolvedStyleContext);
          const styledLink = this.theme.link(this.theme.underline(linkText));
          ...
      }
      ```

      So `[**bold**](url)` renders with the bold span inside the
      link text styled bold + linked. Our parser's `Inline::Link`
      previously stored `(String, String)` — plain text + url, no
      nested inlines, so `[**bold**](url)` rendered the asterisks
      as literal text inside the link.

      **Landed.** Three sites in `src/aj-tui/src/components/markdown.rs`:

      1. **Parser shape.** `Inline::Link(String, String)` →
         `Inline::Link(Vec<Inline>, String)`. The bracket-content
         path now calls `parse_inline(&link_text)` recursively so
         emphasis inside link text survives. Bare-URL and email
         autolinks construct `Inline::Link(vec![Inline::Text(url)], url)`
         to preserve the single-text-segment shape, mirroring what
         marked produces for autolink tokens.
      2. **Render shape.** `Markdown::render_inline_tokens`'s
         `Link` arm recurses with the same `ctx` it received,
         mirroring pi's `renderInlineTokens(token.tokens,
         resolvedStyleContext)`. The rendered inner string lands in
         a renamed `render_link(rendered, plain, url)` that wraps
         with `link(underline(...))` and applies the OSC 8 / parens
         fallback decision on `plain` rather than `rendered` — the
         styled string carries ANSI codes that would never match
         the bare URL.
      3. **`inline_plain_text` helper.** A free function next to
         the other markdown helpers, recursively walking the inline
         tree to extract the unstyled content. Drives the autolink-
         vs-fallback decision in `render_link` and is the analog of
         marked's `text` field on a link token.

      **Cross-impact with F41.** A link inside a heading now picks
      up the heading wrap (cyan + bold) on its visible text via the
      threaded `ctx`; a link inside a paragraph with `pre_style`
      picks up the pre-style. Both compose naturally with the
      link's own `link(underline(...))` wrap.

      **Visible changes.**
      - `[**important**](url)`: bold span inside the link is now
        rendered bold (was literal asterisks).
      - `# See [docs](url)` with a custom `theme.heading`: "docs"
        now carries heading bold + cyan in addition to the link's
        underline + blue (was: link styling only).
      - Autolink inside a heading (`# https://example.com`): the
        no-parens fallback fires correctly because the comparison
        uses unstyled content (was: rendered as
        `https://example.com (https://example.com)` because the
        styled inner string didn't match the URL).

      **Tests.** Four regression tests in `tests/markdown.rs` under
      "Nested inlines in link text (PORTING.md F48)":

      - `link_with_bold_text_renders_bold_inside_link_styling`:
        `[**important**](url)` renders with bold + link styling and
        no literal asterisks. Locks in the parser-recursion fix.
      - `link_inside_heading_inherits_heading_style_via_context`:
        `# See [docs](url)` renders with heading style threading
        through the link's inner text. Asserts the heading style
        appears between the link's `\x1b[34m` blue open and the
        literal "docs" content, not just somewhere in the line.
      - `autolinked_url_renders_with_simple_text_unchanged`:
        regression guard that a bare URL with stars in its path
        (`https://example.com/path*with*stars`) keeps rendering
        once, without italic emphasis from the new parser
        recursion accidentally consuming the asterisks.
      - `autolinked_url_inside_heading_keeps_no_parens_fallback`:
        the `plain == url` fix specifically — `# https://example.com`
        renders the URL once (heading-styled, no parens), not as
        `URL (URL)`. The companion test that locks in the
        plain-text projection over the styled rendered string.

      Each test was verified to fail when the corresponding fix is
      reverted (parser recursion off → bold test fails; `rendered`
      vs `plain` swap → autolink-in-heading test fails) and pass
      with the F48 landing.

#### SelectList

- [x] F31: Scroll-info `(N/TOTAL)` line is width-clamped via
      `truncate_to_width(info, width - 2, "", false)`, matching
      pi's `truncateToWidth(scrollText, width - 2, "")` call.
      The render-time gate also switched from
      `total > max_visible` to pi's `start_index > 0 ||
      end_index < len` form (equivalent for `total > max_visible`
      cases, but now the byte path matches pi). Fixed in the
      same change as F13/F32 since they share the render-path
      rewrite. Regression test
      (`scroll_info_is_truncated_to_terminal_width_minus_two`)
      builds a 100-item list, renders at width 6, asserts the
      indicator line stays within 4 visible cells.
- [x] F32: Wraparound scroll semantics. Resolved by F13's
      switch to pi's stateless centering model. After the
      switch, `move_selection` only updates `selected`; the
      next render's window is `clamp(selected - max_visible / 2,
      0, len - max_visible)`, which lands the new selection at
      the bottom edge on up-wrap (selected = len-1, start =
      len - max_visible) and at the top edge on down-wrap
      (selected = 0, start = 0). Byte-for-byte equivalent to
      pi. Locked in by the F13 wraparound regression tests in
      `tests/select_list.rs`.
- [x] F33: Theme-type `Default` impls were a Rust-side ergonomic
      addition not present in pi-tui (verified by reading
      `packages/tui/src/components/{select-list,markdown,editor,settings-list}.ts`
      and `packages/tui/test/test-themes.ts`: pi's tui crate ships no
      upstream defaults, themes are interfaces, every constructor takes
      the theme as a required argument, and even pi's own tests build
      themes inline in `test-themes.ts`). Removed the `Default` impls
      for `SelectListTheme`, `MarkdownTheme`, `EditorTheme`, and
      `SettingsListTheme`; the tui crate is now palette-agnostic by
      design.

      Companion fixes that fell out of the cleanup:

      - **Constructor surface collapsed to one entry point per
        component.** `SelectList::new(items, max_visible)` and
        `SelectList::with_theme(items, max_visible, theme)` collapsed
        into a single `SelectList::new(items, max_visible, theme)`,
        byte-for-byte matching pi's
        `new SelectList(items, maxVisible, theme, layout?)`. Same shape
        applied to `Editor::new(handle, theme)` and
        `Markdown::new(text, theme)`. The argumentless / `with_theme`
        duals are gone.
      - **`Box<dyn Fn> → Arc<dyn Fn>` for theme closures, and
        `#[derive(Clone)]` on the theme structs.** Pi's themes are
        plain TS objects with reference-shared closure values; passing
        a theme from a parent component to a child works there because
        TS closures are reference types. Our previous port couldn't
        share themes (closures aren't `Clone`), and `Editor`'s
        autocomplete-popup construction had been silently dropping the
        agent's configured `select_list` theme via a
        `SelectListTheme::default()` workaround in `clone_theme`. With
        `Arc`-backed closures the theme structs derive `Clone` cheaply
        (refcount bump, no closure rebuild), the workaround is gone,
        and the autocomplete popup now uses
        `self.theme.select_list.clone()` — the actual configured theme.
      - **`tests/support/themes.rs` (mirroring pi's
        `packages/tui/test/test-themes.ts`)** centralizes the
        `default_*_theme` / `identity_*_theme` factories used across
        all integration tests. In-module unit tests in
        `select_list.rs`, `markdown.rs`, and `editor.rs` carry their
        own tiny `identity_theme` helpers for the same reason pi's
        do not (their tests aren't in the same file as the component).
      - **Examples build their own themes inline.** `chat_simple.rs`
        and `settings_demo.rs` now construct `EditorTheme` /
        `MarkdownTheme` / `SelectListTheme` / `SettingsListTheme`
        explicitly, demonstrating the agent-layer pattern. A real
        agent would assemble these from a central palette; the
        examples flag this in their comments.
      - **Removed the F19-followup tests
        `with_theme_applies_supplied_theme_to_render_output` /
        `new_falls_back_to_*_theme_default`** in the in-module unit
        tests. The `with_theme`-vs-`new` distinction they tested no
        longer exists; the kept `new_applies_supplied_theme_to_render_output`
        in each component's `#[cfg(test)]` mod verifies the
        single-`new` path with a sentinel theme.
      - **Removed `tests/capabilities.rs`'
        `default_markdown_theme_seeds_hyperlinks_from_capabilities`**.
        It tested that `MarkdownTheme::default()` consulted the
        capabilities cache; with `Default` gone, the cache integration
        moves to the agent layer. `get_capabilities().hyperlinks`
        itself is still exercised by the cap-cache tests above it.

      Migration count: ~100 call sites across 30 files (test files,
      examples, internal autocomplete-popup construction).
      Documented on each theme struct's rustdoc, on each component's
      `new` rustdoc, and at the top of `tests/support/themes.rs`.

#### TextBox

- [x] F34: `width = 0` (or `width < 2 * padding_x`) edge case.
      Pre-F34 the Rust port short-circuited to `Vec::new()` and
      reset the cache when `content_width` saturated to zero;
      pi-tui's `box.ts` clamps `contentWidth = max(1, width -
      paddingX * 2)` and renders one cell of content. The Rust
      port now mirrors that with
      `let content_width = width.saturating_sub(self.padding_x *
      2).max(1);` plus a separate `self.children.is_empty()` early
      return that no longer touches the cache (pi's empty-children
      path returns `[]` without invalidating, and `add_child` /
      `remove_child` / `clear` already invalidate; the cache reset
      on the empty-children path was redundant). The
      `child_lines.is_empty()` early return also stops resetting
      the cache for the same reason. Aligns with the analogous
      F14 / F40 / F42 fixes on `Text` / `Markdown`. Regression
      tests in `tests/text_box.rs` under "F34: degenerate
      render-width clamps to one cell"
      (`degenerate_width_clamps_content_width_to_one_cell`,
      `zero_render_width_clamps_content_width_to_one_cell`,
      `padding_x_exceeding_half_width_clamps_to_one_cell`,
      `empty_children_does_not_invalidate_cache`); each of the
      first three was confirmed to fail without the fix and pass
      with it. The previous `zero_content_width_renders_as_empty`
      test was retired (it pinned the diverged-from-pi shape).

      **F34-followup (constructor shape).** Aligned the TextBox
      constructor with pi-tui's `Box(paddingX = 1, paddingY = 1,
      bgFn?)` shape: `TextBox::new()` → `TextBox::new(padding_x,
      padding_y)`, padding is required at construction, no setters.
      `Default::default()` matches pi's JS-default-arg shape
      (`new(1, 1)`). Dropped `set_padding_x` and `set_padding_y`
      (no equivalent in pi) along with their cache-invalidation
      bookkeeping (the cache contract simplifies because padding is
      now immutable per instance). All 18 test sites in
      `tests/text_box.rs` migrated; the
      `padding_change_invalidates_cache` test was retired (it tested
      a setter we removed). No production callers.

- [x] F49: Cross-component constructor-vs-setter parity sweep.
      F34-followup migrated `TextBox` to pi's required-padding
      constructor shape and dropped its padding setters. The same
      shape applied cleanly to `Text`, `Markdown`, `TruncatedText`,
      and `SelectList`, where the Rust port had grown setters that
      pi-tui doesn't have. Goal: every component's constructor
      signature and setter surface mirrors pi's, so the Rust API
      matches what a pi-tui-conversant reader expects. **All four
      sub-component migrations landed.**

      Audit (vs `/tmp/pi-mono/packages/tui/src/components/`):

      | component       | pi constructor                                            | rust constructor                                | divergent rust setters                                                                       |
      | --------------- | --------------------------------------------------------- | ----------------------------------------------- | -------------------------------------------------------------------------------------------- |
      | `TextBox`       | `(paddingX=1, paddingY=1, bgFn?)`                         | `new(padding_x, padding_y)`                     | (none — F34-followup landed)                                                                 |
      | `Text`          | `(text="", paddingX=1, paddingY=1, customBgFn?)`          | `new(text, padding_x, padding_y)`               | (none — F49/Text landed)                                                                     |
      | `Markdown`      | `(text, paddingX, paddingY, theme, defaultTextStyle?)`    | `new(text, padding_x, padding_y, theme, pre_style)` | (none — F49/Markdown landed)                                                              |
      | `TruncatedText` | `(text, paddingX=0, paddingY=0)`                          | `new(text, padding_x, padding_y)`               | (none — F49/TruncatedText landed)                                                            |
      | `SelectList`    | `(items, maxVisible, theme, layout?)`                     | `new(items, max_visible, theme, layout)`        | (none — F49/SelectList landed)                                                               |
      | `Spacer`        | `(lines=1)`                                               | `new(lines)`                                    | (aligned)                                                                                    |
      | `Editor`        | `(tui, theme, options?)`                                  | `new(handle, theme)`                            | (aligned — `setPaddingX` exists in both)                                                     |
      | `Loader`        | `(ui, spinnerColor, messageColor, message, indicator?)`   | `new(handle, spinner_style, message_style, msg)` | (aligned post F26)                                                                          |

      Migration pattern (mirrors F34-followup byte-for-byte):

      1. Change `T::new(...)` to take pi's full required-arg list.
         Optional pi args (default-valued in JS) become required
         positional args on the Rust side; provide
         `Default::default()` only when pi's defaults are
         documentation values (not opinionated palette choices —
         see F33 for why themes don't get `Default`).
      2. Drop Rust setters that pi doesn't have. Their
         cache-invalidation bookkeeping goes too (the cache key
         simplifies when the underlying field is immutable).
      3. Migrate test sites: fold `set_padding_*` and equivalents
         into the constructor call. Retire any test that exists
         only to verify a now-removed setter's invalidation
         behavior.

      Per-component scope:

      - **Text**: **landed.** Constructor is now
        `Text::new(text, padding_x, padding_y)` with
        `Default::default() = new("", 1, 1)`. `set_padding_x` and
        `set_padding_y` were dropped along with their cache-
        invalidation bookkeeping (the cache key simplifies because
        padding is now immutable per instance). `set_text` and
        `set_bg_fn` stay (pi exposes `setText` / `setCustomBgFn`).
        9 test sites in `tests/text.rs` and 4 in-module unit tests
        migrated; the in-module tests now construct with explicit
        padding instead of post-construction setters. Two production
        callers updated: `loader.rs` (now `Text::new("", 1, 0)`) and
        the two examples (`chat_simple`, `settings_demo`).

      - **Markdown**: **landed.** Constructor is now
        `Markdown::new(text, padding_x, padding_y, theme, pre_style:
        Option<PreStyle>)`, byte-for-byte mirroring pi-tui's
        `new Markdown(text, paddingX, paddingY, theme,
        defaultTextStyle?)` (`packages/tui/src/components/markdown.ts:91-103`).
        `set_padding_x`, `set_padding_y`, `set_pre_style`,
        `set_theme`, and `set_hyperlinks` were all dropped — pi has
        none of them. Only `set_text` survives, mirroring pi's lone
        `setText` setter. Capabilities-driven hyperlink toggling now
        goes through theme construction (`MarkdownTheme { hyperlinks:
        true, ..default_markdown_theme() }`) rather than a post-
        construction flip. Cache contract simplifies: padding,
        theme, and pre-style are immutable per instance, so the
        cache key only tracks `(text, width)` and `set_text` is the
        only mutator that needs invalidation. Migration count: 9
        in-module unit tests in `markdown.rs`, the four helpers at
        the top of `tests/markdown.rs` (`md`, `md_with_padding`,
        `md_with_hyperlinks`, `md_with_gray_italic`) plus 7 direct
        test sites that built with `theme_with_color_only_heading`,
        2 sites in `tests/capabilities.rs` (the `set_hyperlinks(true)`
        / `set_hyperlinks(false)` toggles became theme-construction
        struct-update-syntax expressions), and 2 sites in
        `examples/chat_simple.rs`. Two F45 tests had redundant
        `set_padding_x(0); set_padding_y(0)` calls after
        `md_with_gray_italic(...)` (which now constructs at `(0, 0)`
        directly); those were dropped.

      - **TruncatedText**: **landed.** Constructor is now
        `TruncatedText::new(text, padding_x, padding_y)` with
        `Default::default() = new("", 0, 0)`. `set_padding_x`,
        `set_padding_y`, and `set_text` were all dropped — pi has
        *zero* setters (`/tmp/pi-mono/packages/tui/src/components/truncated-text.ts:7-65`),
        the component is fully immutable. `text()` getter survives
        for callers that need to read back. The single test helper
        in `tests/truncated_text.rs` migrated from
        `TruncatedText::new(text); set_padding_x(...); set_padding_y(...)`
        to a single `TruncatedText::new(text, padding_x, padding_y)`
        call. Three in-module unit tests
        (`test_truncated_text_fits`, `test_truncated_text_truncates`,
        `test_truncated_text_multiline_takes_first`) updated to pass
        explicit `(0, 0)` padding instead of relying on the previous
        argless constructor; one new in-module unit test
        (`default_constructs_with_zero_padding`) verifies the
        `Default::default()` shape. No production callers existed
        — `TruncatedText` only had the test helper, the module file
        itself, and `components.rs`'s pub-use re-export. The
        `set_text` removal forces callers to reconstruct on text
        change — which matches pi's contract; if a future caller
        needs streaming text, design the API around a different
        component (e.g. a custom `Component` impl) rather than
        reintroducing the setter.

      - **SelectList**: **landed.** Constructor is now
        `SelectList::new(items, max_visible, theme, layout)`,
        byte-for-byte mirroring pi-tui's
        `new SelectList(items, maxVisible, theme, layout?)`
        (`packages/tui/src/components/select-list.ts:52-58`).
        `set_theme`, `set_layout`, and `set_items` were all
        dropped — pi has none of the three. Pi-equivalent setters
        survive: `set_filter` (pi's `setFilter`),
        `set_selected_index` (pi's `setSelectedIndex`). The
        `items()` getter stays as a Rust-side ergonomic addition
        used by the Editor's autocomplete-popup and the
        `settings_demo` example to look up an item index without
        rebuilding the input slice. Layout is required at
        construction (pi defaults it to `{}`); call sites that
        don't tune column bounds pass [`SelectListLayout::default`]
        explicitly so the value is visible at the call site.
        Migration count: 2 helpers + 1 direct site in
        `tests/select_list.rs` (the previous helper-internal
        `set_theme(...) ; set_layout(...)` pair folded into
        `SelectList::new(items, max_visible, theme, layout)`),
        2 in-module unit tests in `select_list.rs`, 2 production
        sites in `editor.rs` (autocomplete-popup construction in
        `dispatch_autocomplete_request` and the streaming-snapshot
        path), and 1 site in `examples/settings_demo.rs`.

      Order of execution: do the components in order of audit
      table position (Text → Markdown → TruncatedText → SelectList),
      one component per change, each with its own commit. **All
      four landed.** The Markdown migration confirmed the Text
      template scales: the cache-contract simplification (cache key
      shrinks to `(text, width)` once padding/theme/pre-style are
      immutable per instance) is the recurring dividend.
      TruncatedText was the smallest of the bunch — no cache, no
      production callers — and confirmed the same shape carries
      cleanly to a fully-immutable component too. SelectList
      consolidated three setter removals (`set_theme`, `set_layout`,
      `set_items`) into a single constructor migration with two
      production sites; layout joining theme as required-at-
      construction was the cleanest recurrence of the pattern across
      the four components.

      **Out of scope.** Setters that pi *also* has (`setText`,
      `setIndicator`, `setMessage`, `setValue`, `setFilter`,
      `setBgFn` etc.) stay as-is. The Rust-side `with_*` builder
      methods (`Loader::with_frames`, `LoaderIndicatorOptions::with_interval`)
      are ergonomic builders for option types pi takes as plain
      object literals; they're not setter-vs-constructor drift.

      **Why now.** Two payoffs: (1) cleaner reading for someone
      coming from pi-tui (the constructor signatures match
      one-to-one); (2) simpler cache contracts (padding-immutable
      components don't need to invalidate on padding change, and
      the absence of setters means there are fewer mutation paths
      to reason about). F34-followup demonstrated both on `TextBox`.
- [x] F35: `remove_child` API mismatch. Pi-tui's
      `Container.removeChild(component)` (`tui.ts:185`) and
      `Box.removeChild(component)` (`box.ts:34`) both take a
      `Component` reference and do an `indexOf` lookup. The Rust
      port previously only exposed an index-based `remove_child(usize)`
      shape (`Vec::remove`), so call sites that hold a borrow of a
      previously-added child had to track its position separately.

      Replaced the index-based `remove_child(usize)` on both
      [`Container`] and [`TextBox`] with
      `remove_child_by_ref(&dyn Component) -> Option<Box<dyn Component>>`,
      mirroring pi's `removeChild(component)` shape byte-for-byte.
      Pi has no index-based remove (its `Container.children` /
      `Box.children` arrays are public, so a caller wanting indexed
      removal could `tui.children.splice(idx, 1)` directly, but no
      `removeChildAtIndex` / equivalent method exists); the Rust
      port aligns by removing the index variant rather than carrying
      drift forward.

      Identity is determined by [`std::ptr::addr_eq`] on the
      underlying `&dyn Component`, so the caller's reference must
      point to the same in-memory instance that was added — two
      distinct instances of the same type with byte-identical fields
      compare unequal. Returns `None` for the not-present case
      (mirroring pi's `indexOf === -1` no-op); `TextBox` also leaves
      its render cache intact when nothing was removed (matching pi's
      "only invalidate when splice fired" shape on `box.ts:38`).

      The Rust port's return type is `Option<Box<dyn Component>>`
      rather than pi's `void`, so callers can re-attach the removed
      component elsewhere; callers wanting pi's discard semantics
      write `let _ = c.remove_child_by_ref(child);`. This is the
      single intentional Rust-idiom alignment — symmetric with
      `Vec::remove`'s return shape and with the dropped index-based
      `remove_child`'s former return shape.

      Migration of dependents:
      - `examples/chat_simple.rs`: `BotState::Thinking` now stores
        a `*const dyn Component` to the loader instead of a
        `loader_index: usize`. Stash the pointer at insertion time
        (just before the `Box<dyn Component>` moves into
        `tui.root`), dereference it inside `remove_child_by_ref`
        when the bot's turn ends. The `/clear` and `/delete` paths
        use a per-iteration "fetch raw pointer from `tui.root.get(i)`,
        immediately consume it in the by-ref call" idiom to satisfy
        the borrow checker, since `&dyn Component` from `get(i)`
        borrows `tui.root` and conflicts with the `&mut tui.root`
        the by-ref method needs. Each unsafe block is documented
        with the contract that keeps the dereference sound (the
        heap allocation outlives the pointer because the owning
        `Box` doesn't move; Vec resizes copy box pointers, not the
        boxed contents).
      - In-module `Container::tests::last_index_tracks_the_current_len`:
        same idiom, swapped two `c.remove_child(0)` calls for
        per-iteration raw-pointer-stash patterns.

      Regression tests in the in-module
      [`Container`][src/container.rs] tests
      (`remove_child_by_ref_removes_the_matching_child_and_returns_it`,
      `remove_child_by_ref_returns_none_when_child_is_not_in_container`,
      `remove_child_by_ref_does_not_match_a_distinct_instance_with_identical_fields`)
      and `tests/text_box.rs`
      (`remove_child_by_ref_removes_the_matching_child_and_returns_it`,
      `remove_child_by_ref_returns_none_when_child_is_not_in_box`,
      `remove_child_by_ref_does_not_match_a_distinct_instance_with_identical_fields`)
      cover the happy path, the not-present case, and the
      identity-not-equality contract on both containers.

      **Followup tracked: F51** (Tui-level forwarding methods so
      callers can write `tui.add_child(c)` / `tui.remove_child_by_ref(c)`
      instead of reaching through `tui.root`). Pi's `TUI extends
      Container` exposes the Container methods directly on the
      Tui; our `pub root: Container` field requires a `tui.root.X`
      access that's drift from pi's call shape. Out of scope for
      F35; tracked separately because the migration touches ~145
      `tui.root.*` call sites across tests and examples.

#### CancellableLoader

- [x] F36: API shape was diverging from pi-tui's `AbortSignal`
      model: the Rust port shipped with `cancel: Arc<AtomicBool>`,
      a `cancel_flag() -> Arc<AtomicBool>` accessor, a manual
      `abort()` method with no pi equivalent, and a transition
      guard on `on_abort` (fires once vs. pi's fire-every-press).
      The `Arc<AtomicBool>` choice was a holdover from the
      initial port commit (`1bbca9a`) and was internally
      inconsistent — every other place in `aj-tui` that needs
      cancellation already uses `tokio_util::sync::CancellationToken`
      (Editor's `autocomplete_cancel`, Loader's `animation_cancel`,
      the autocomplete walker's `AutocompleteContext::cancel`).
      `tokio-util` is already a workspace dep, so this was an
      accident, not a deliberate trade-off.

      **Resolution: strict pi parity migration.**
      `tokio_util::sync::CancellationToken` is the idiomatic Rust
      analog of pi's `AbortSignal`: clonable, observable
      (poll via `is_cancelled()` or await via
      `cancelled().await`), and integrates with
      `tokio::select!` the same way pi's
      `await fetch({ signal })` integrates with the JS event
      loop. Migration:

      - `cancel: Arc<AtomicBool>` → `cancel: CancellationToken`.
      - `cancel_flag() -> Arc<AtomicBool>` →
        `cancel_token() -> CancellationToken` (mirrors pi's
        `signal: AbortSignal` getter; clones share state).
      - `is_aborted()` stays (delegates to
        `cancel.is_cancelled()`); semantics unchanged. Mirrors
        pi's `aborted: boolean` getter.
      - **Dropped** `abort()` (Rust-only, no pi equivalent, no
        production callers in the workspace, only one test that
        existed to verify the method itself).
      - **Dropped** the transition guard in `handle_input`. Now
        mirrors pi byte-for-byte:
        `cancel.cancel(); on_abort.as_mut().map(|cb| cb());`.
        `on_abort` fires on every cancel-key press;
        cancellation itself is idempotent (the token only flips
        false → true once).
      - **Dropped** the `self.inner.stop()` call from
        `handle_input` — pi doesn't auto-stop either; the parent
        is expected to call `dispose()` (= `stop()`) from its
        `on_abort` handler. `Drop for Loader` cancels the
        animation pump on tree removal, so the spinner never
        leaks.

      Module-level rustdoc reframed as a faithful port (drop the
      elaborate "we diverge" wording the previous attempt added).
      Per-method rustdoc cross-references the pi member at the
      call site (`cancel_token` ↔ `signal`,
      `is_aborted` ↔ `aborted`, `set_on_abort` ↔ `onAbort`,
      `stop` ↔ `dispose`).

      Test migration in `tests/cancellable_loader.rs`:

      - `cancel_flag_starts_unset` →
        `cancel_token_starts_unaborted`.
      - `escape_trips_the_cancel_flag_and_marks_the_event_handled`
        → `escape_trips_the_cancel_token_and_marks_the_event_handled`.
      - `ctrl_c_trips_the_cancel_flag_via_tui_select_cancel`
        → `ctrl_c_trips_the_cancel_token_via_tui_select_cancel`.
      - `cancel_flag_is_shared_with_workers_that_hold_a_clone`
        → `cancel_token_is_shared_with_workers_that_hold_a_clone`.
      - `on_abort_callback_fires_exactly_once` →
        `on_abort_callback_fires_on_every_cancel_press`
        (now asserts N presses → N callback calls, matching pi).
      - **Removed** `abort_method_sets_flag_without_firing_callback`
        — the method is gone.

      All 8 cancellable_loader tests pass; full `cargo test
      -p aj-tui` suite stays green (~870 tests across
      ~50 binaries).

#### SettingsList (investigation)

- [x] F37: Confirm search-input filter trigger parity. **Spot-check
      complete; landed.** Walked every branch of
      [`Input::handle_input`] in `src/components/text_input.rs` and
      audited the return value vs. value-mutation invariant:
      every branch that mutates `value` returns `true`, and every
      branch that returns `false` leaves `value` untouched. The
      previously-existing `if handled` gate in
      [`SettingsList::handle_input`] was therefore functionally
      correct today, but structurally fragile against a future Input
      change that adds a return-false-but-mutate path.

      **Per-branch audit** (`text_input.rs:459-630`):

      - `tui.select.cancel` → returns `true`, calls `on_escape`,
        `value` untouched. (In SettingsList's setup this branch is
        unreachable through the search forward: cancel is caught at
        the SettingsList level via the registry check before the
        fallthrough.)
      - `tui.editor.undo` → returns `true`, restores from snapshot
        (mutates `value`).
      - `tui.input.submit` / `is_newline_event` → returns `true`,
        calls `on_submit`, clears undo stack, `value` untouched.
        (Plain Enter is caught by `tui.select.confirm` at the
        SettingsList level, so the search forward only sees this
        branch for less-common submit bindings.)
      - All `tui.editor.delete*` / `yank*` arms → return `true`,
        mutate `value`.
      - All `tui.editor.cursor*` arms → return `true`, move byte
        offset, leave `value` untouched.
      - Printable char tail (bare `KeyCode::Char` with no
        Ctrl/Alt) → returns `true`, inserts char (mutates `value`).
      - Printable Char with Ctrl or Alt that didn't match any
        registry entry → falls through to the trailing `false`.
        `value` untouched.
      - `InputEvent::Paste(_)` → returns `true`, inserts cleaned
        text (mutates `value`).
      - `InputEvent::Resize(_, _)` (the only non-Key, non-Paste
        variant in our [`InputEvent`] enum) → matches the catch-all
        `_ => false` arm. `value` untouched. **In practice
        unreachable through component dispatch**: [`Tui`] intercepts
        `InputEvent::Resize` at the top of
        `handle_input_after_listeners` (`tui.rs:1851-1854`) and
        returns before any component sees it. Mouse / Focus events
        aren't represented at all — they're filtered out at the
        `TryFrom<crossterm::event::Event>` boundary in `keys.rs`,
        which only converts `Key` / `Paste` / `Resize`. So the only
        events `SettingsList` actually forwards to the embedded
        search input are `Key` and `Paste`, both of which
        [`Input::handle_input`] handles meaningfully.

      Conclusion: the gate's invariant ("returns true ⟹ may have
      mutated; returns false ⟹ definitely didn't") holds today.
      Pi-tui's `applyFilter` (`settings-list.ts:194-195`) runs
      unconditionally regardless, since pi's `Input.handleInput`
      returns `void`. To match pi byte-for-byte and remove the drift
      surface, dropped the `if handled` gate in
      `SettingsList::handle_input` so `apply_filter` runs
      unconditionally after every `search.handle_input(event)` call.
      Cost: one extra `apply_filter` invocation per Key event with
      Ctrl/Alt modifiers that didn't match any registry entry (e.g.
      `Ctrl+Z` under default bindings). Negligible: `apply_filter`
      only iterates the items list, which is human-readable size,
      and only fires while search is enabled.

      Side effect: cursor moves inside the search input
      (`tui.editor.cursorLeft` / `cursorRight` / etc., which fall
      through `SettingsList`'s registry checks and reach the search
      forward) now reset `selected` to 0 via the always-recompute,
      mirroring pi. The underlying filter is unchanged on a cursor
      move since the search value didn't change, but the recompute
      resets the navigation cursor regardless. This was already the
      case before F37 too (cursor-move arms in `Input::handle_input`
      return `true`, so the gate let `apply_filter` through). The
      F37 change is invisible for these bindings; it only matters
      for the `Ctrl+Z`-shaped cases that used to skip recompute.

      Regression test in `tests/settings_list.rs`
      (`typing_a_printable_recomputes_the_filter_after_each_keystroke`)
      walks the filter through narrow ('c' → "confirm") → narrow
      again ('o' → "confirm" still) → no-match ('z' → empty) →
      broaden (backspace → "confirm" returns) → clear (two more
      backspaces → unfiltered). Each step asserts the filter
      recomputed in lockstep with the search input's current value.
      The test passes under both the gate-on and gate-off code
      paths because every event in the test is a Backspace or
      printable Char, both of which `Input::handle_input` returns
      `true` for; the test's value is structural — it pins the
      always-recompute invariant for future regressions.

#### Tui (container access surface)

- [x] F51: `Tui` forwards Container's methods directly so callers
      write `tui.add_child(c)` / `tui.remove_child_by_ref(c)`
      / `tui.insert_child(i, c)` / `tui.clear()` instead of the
      former `tui.root.X` access pattern. **Surfaced during the F35
      pi cross-check.** Pi-tui's `TUI extends Container`
      (`packages/tui/src/tui.ts:217`), so all of `Container`'s public
      surface — `addChild`, `removeChild`, `clear`, the `children`
      array — appears directly on `tui`. Real-world pi call sites
      reflect this: `tui.removeChild(loader)`,
      `tui.addChild(component)`, `tui.children[i]`, etc.

      The Rust port previously exposed `pub root: Container` and
      every caller wrote `tui.root.add_child(...)`. That was a
      structural drift from pi's call shape touching every consumer
      of aj-tui (~148 `tui.root.*` access sites across tests and
      examples).

      Landed shape:

      1. Added forwarding methods on `Tui` for the Container surface
         consumers actually use: `add_child`, `insert_child`,
         `remove_child_by_ref`, `clear`, `len`, `is_empty`,
         `last_index`, `get`, `get_mut`, `get_as<T>`, `get_mut_as<T>`.
         Each is a thin pass-through to `self.root.X(...)`; no
         behavior change.

      2. Made `root: Container` private. Initially F51 left
         `pub root` in place as an escape hatch for "callers needing
         direct Container-level access not on the forwarding list",
         but no such caller exists in-tree after the migration: the
         only remaining external references to `tui.root` are
         doc-comments. Rust can't `extend` a struct the way pi's TS
         does, but the forwarding surface is the full public API
         that consumers actually use, and dropping the field's
         `pub` keeps the type's surface narrow.

      3. Migrated all external call sites mechanically: every
         `tui.root.add_child(...)` / `tui.root.get(...)` / etc. in
         tests, examples, and the lone in-tree component
         (`cancellable_loader.rs`) now reads `tui.add_child(...)` /
         `tui.get(...)`. Internal `self.root.X(...)` calls inside
         `Tui`'s own `impl` block stayed as-is (they're already inside
         the type, so going through `self.X(...)` would just add a
         redundant call frame).

      4. Updated `examples/PARITY.md` with a "Container surface on
         `Tui`" note pointing at the forwarding pattern as the
         canonical call shape.

      No new regression tests required: every existing test that
      manipulates the child stack now exercises the forwarding path,
      and the underlying `Container` methods continue to be covered
      by `container.rs`'s own unit tests.

---

## Phase G: Test-harness shore-up (in progress)

Quality-of-life items for the test harness. G1, G2, G5 landed; the
rest are tracked.

### G1. `cells_in_row` width source (retired)

- [x] Originally landed: `cells_in_row` was sourcing its grid bounds
      from `screen().size()` rather than the cached `state.columns` /
      `state.rows`, so a future drift between cache and parser
      couldn't silently emit trailing `CellInfo::default()` filler.
      Regression test was `cells_in_row_uses_the_parsers_authoritative_width`.
- [x] **Retired alongside the G4 revert.** `cells_in_row` itself was
      removed as cruft (zero real callers in the workspace; only
      consumed by its own meta-tests). The parser-bounds fix and its
      regression test went with it. The historical entry stays as a
      record that the fix landed at the time.

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

- [x] G3: Doc note on `VirtualTerminal::flush()` clarifying that
      writes are synchronous and no flush is needed before a
      viewport read. Promoted the existing one-line `// No-op` to
      a rustdoc block on `VirtualTerminal`'s `flush` impl
      (`tests/support/virtual_terminal.rs`) covering: (a) the
      synchronous `state.parser.process(...)` call in `write` is
      what makes the no-op safe, and the reader methods
      (`viewport`, `cell`, `cells_in_row`, `scroll_buffer`)
      observe the post-write state directly; (b) the trait-level
      `flush` exists because `ProcessTerminal` wraps line-buffered
      `io::stdout()` and actually needs to push frames to the
      kernel; (c) parity note — pi-tui's `Terminal` interface
      does not include `flush` and pi's `VirtualTerminal` exposes
      a separate async test helper that awaits xterm.js's drain
      callback; the Rust port collapses both roles onto the
      synchronous trait method because `vt100_ctt`'s parser is
      synchronous and `ProcessTerminal` needs a flush hook.
- [x] G4: Panicking `cell_at` / `row_cells` variants alongside
      the `Option`-returning `cell` / `cells_in_row`.

      **Reverted as cruft after a usage audit.** G4 originally landed
      with `cell_at(row, col) -> CellInfo` and
      `row_cells(row) -> Vec<CellInfo>` mirroring the
      `Option`-returning `cell` / `cells_in_row` but panicking on OOB
      with a coord-bearing message. A follow-up audit found that:

      - `cell()` itself has 5 real callers across `markdown.rs`,
        `overlay_style_leak.rs`, and `tui_render.rs` — all checking
        italic / underline styling in the same files where pi-tui has
        per-file `getCellItalic` / `getCellUnderline` helpers. **Earns
        its keep.**
      - `cells_in_row()` had **zero real callers** in the entire
        1300+ test suite. Only consumer was its own meta-tests in
        `tests/virtual_terminal_helpers.rs`. **Cruft.**
      - `cell_at()` (G4, just landed) had **zero real callers** —
        couldn't justify the addition.
      - `row_cells()` (G4, just landed) had **zero real callers** —
        same.
      - `CellInfo::default()` was only constructed by `cells_in_row`
        as OOB filler. Dead with `cells_in_row` removed.

      End state: one Rust-side helper, `cell(row, col) -> Option<CellInfo>`.
      Surface shrinks from 4 methods to 1, the `Default` impl on
      `CellInfo` goes away, and the resulting shape is closer to
      pi-tui (which has zero cell-level helpers on `VirtualTerminal`
      at all — pi tests reach into `xterm.buffer.active` per-file).
      The 5 existing `cell()` callers continue using
      `.expect("cell (X,Y) should exist")`, which is informative and
      idiomatic.

      Cross-link: G1 was retired in the same revert (its fix was on
      `cells_in_row`'s parser-bounds sourcing, which goes away with
      the helper).
- [x] G6: Renamed `support::wait_for_render` (sync) to
      [`support::render_now`] so the engine choice is obvious at the
      call site. The sync helper is a one-shot `tui.render()` (no
      waiting, no event-loop pumping); the async-tier helper
      [`support::async_tui::wait_for_render`] keeps its name because
      it is the byte-for-byte analog of pi-tui's `terminal.waitForRender()`
      (`packages/tui/test/virtual-terminal.ts:213`). The companion sync
      helper `request_and_wait_for_render` was renamed to
      `request_and_render_now` for sibling consistency. The deliberate
      name asymmetry \u2014 `render_now()` (no await) vs
      `wait_for_render().await` \u2014 makes the engine choice obvious at
      the call site while keeping pi parity on the async side; the
      sync helper is a Rust-only addition (pi has no sync render
      path) so renaming it doesn't break any pi-aligned name.
      Cross-references on both helpers' rustdoc point at the
      counterpart and explain the shape. `tests/support/README.md`
      table row, "Async-driver tier" bullet, and "Async vs sync"
      section all updated. Mechanical rename across ~12 test files
      (~190 sync call sites); full `cargo test -p aj-tui` suite stays
      green.
- [x] G7: `MutableLines::with_lines` over `Vec<String>` smoke test —
      **won't do.** Re-examined against pi: pi-tui has no
      `MutableLines` equivalent (the suite uses an inline
      `TestComponent` class with a public `lines: string[]` field —
      see `packages/tui/test/tui-render.test.ts:7`), so this is not a
      parity item. The Rust-side `MutableLines::with_lines<I, S>`
      generic is already exercised across `S = &str` (every `[&str;
      N]` call site) and `S = String` (`tests/tui_render.rs:1442`
      seeds it from `(0..12).map(|i| format!("base-{i:02}"))`).
      Monomorphization is enforced at compile time by the existing
      call sites — narrowing the bound would break all of them, not
      just a runtime smoke test. Skipping per the
      "no testing cruft just because" guideline.
- [x] G8: Doc note on `scroll_buffer()` flagging the internal
      `RefCell::borrow_mut` as not safe for re-entrant calls —
      **won't do.** Re-examined against pi: pi-tui's `getScrollBuffer`
      (`test/virtual-terminal.ts:170`) is plain JS over
      `xterm.buffer.active`; no `RefCell`, no re-entrancy concept.
      The borrow hazard exists only because the Rust port wraps
      `vt100::Parser` in `RefCell<State>` — and it applies uniformly
      to all 25+ public `VirtualTerminal` methods that touch
      `self.state` (`viewport`, `cell`, `cursor_position`,
      `cursor_visible`, `title`, `writes`, `start_count`, etc.), not
      just `scroll_buffer`. Singling out one method would imply the
      others are different, which they aren't. Re-entry can't happen
      in practice either: `scroll_buffer` only reads from
      `state.parser.screen()` and never dispatches a user callback
      that could re-enter the borrow. `RefCell` panic-on-re-entry is
      already documented Rust stdlib behavior. Skipping per the
      "no doc cruft just because" guideline.
- [x] G9: Tests-from-the-other-side that we could port —
      `tui-cell-size-input.test.ts` is in scope (paired with E5 above).
      **Won't-port** for our typed-event harness. Pi's test feeds raw
      bytes (`\x1b[6;20;10t`, then `q`) into a `VirtualTerminal` and
      asserts that the cell-size reply is consumed and `q` reaches the
      `InputRecorder`. Our `VirtualTerminal` deliberately doesn't accept
      a raw byte stream — `tests/support/README.md` documents this as
      an explicit non-goal: "Raw stdin-buffer / Kitty-protocol /
      cell-size-query byte streams are not testable here and aren't
      planned." The structural equivalent of pi's consume happens at
      the crossterm parser layer (`Parser::advance` Err arm clears the
      buffer for the malformed CSI; trace in E5 above); crossterm's
      `parse_event` is `pub(crate)`, so a Rust-side unit test isn't
      reachable without a PTY harness either. Re-evaluate if and when
      we add image support and start sending `\x1b[16t` queries from
      our own `start()` — at that point we'd want a real PTY-backed
      smoke test that exercises the whole loop, which is a separate
      lift from porting pi's harness-level test.
- [x] G10: `VirtualTerminal::clear_viewport` doc-comment in
      `tests/support/virtual_terminal.rs` updated. Replaced the
      misleading "equivalent to the user hitting a 'clear' button in
      their terminal" line (in xterm and most modern terminals, that
      button wipes scrollback — ours doesn't) with an accurate
      description: scrollback is preserved, the emitted bytes are CSI
      `2J` + cursor home, the CSI `3J` that would also wipe scrollback
      is intentionally omitted. Flagged as the deliberate divergence
      from pi's `VirtualTerminal.clear()` (which calls
      `xterm.clear()` and wipes scrollback). Cross-link to
      [`Self::reset`] for the wipe-everything alternative. Pi never
      actually calls `terminal.clear()` from any test, so the
      divergence has no operational impact — this is a doc-correctness
      fix on an intentional Rust-side rename (`clear_viewport` vs
      `clear`), not a behavior change.
- [x] G11: Port the original `viewport-overwrite-repro.ts`
      driver script to a proper integration test —
      **won't do.** Re-examined against pi:
      `packages/tui/test/viewport-overwrite-repro.ts` is *not* a test
      (no `.test.ts` suffix, not picked up by the test runner). It's
      a standalone driver: uses `ProcessTerminal` (real terminal,
      not `VirtualTerminal`), has `main().catch(...)` at the bottom,
      documents `npx tsx packages/tui/test/viewport-overwrite-repro.ts`
      invocation, and recommends running it under tmux at 8-12 rows
      for reliable repro. Pi has zero automated tests for this
      scenario (`PRE-TOOL` / `POST-TOOL` strings appear only in the
      repro script). The Rust port already mirrors this faithfully
      with `examples/viewport_overwrite_repro.rs` — same name, same
      manual-driver shape, same tmux usage instructions, same
      `request_render`-only scheduling note. Converting it into a
      `VirtualTerminal`-backed integration test would diverge from
      pi's deliberate choice to keep the bug as a real-terminal
      manual repro (the bug is timing/dimension-sensitive; a fixed
      virtual-terminal scenario may not exercise the same render-
      pipeline edge case). Sticking with parity.

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
- [ ] **Centralized agent-side theme system.** pi's
      `pi-coding-agent` ships a `Theme` class with 37 named color
      tokens (`accent`, `border`, `mdHeading`, …), JSON theme files
      under `~/.pi/agent/themes/*.json`, two built-in themes
      (`dark` / `light`), terminal-capability detection
      (truecolor vs 256-color with hex→256 fallback), and helpers
      (`getMarkdownTheme()`, `getEditorTheme()`,
      `getSelectListTheme()`) that bridge the central palette to
      each pi-tui component's theme interface. Theme selection is a
      `/theme` slash command and a `theme: <name>` settings entry.

      Our state today: `aj-tui` is now strictly palette-agnostic.
      The per-component theme structs (`EditorTheme`,
      `MarkdownTheme`, `SelectListTheme`, `SettingsListTheme`) no
      longer carry `Default` impls (F33 removed the hardcoded ANSI
      escape sequences they used to ship), and every constructor
      takes the theme as a required argument
      (`Editor::new(handle, theme)`, `Markdown::new(text, theme)`,
      `SelectList::new(items, max, theme)`,
      `SettingsList::new(items, max, theme, ...)` —
      byte-for-byte parity with pi's constructors). Tests source
      themes from `tests/support/themes.rs` (mirroring pi's
      `packages/tui/test/test-themes.ts`); examples build themes
      inline.

      What's still missing is the agent-side bridge: a centralized
      palette in `aj-conf` or a new `aj-theme` crate, the JSON
      loader for `~/.aj/themes/*.json`, the `dark` / `light` `/theme`
      slash command, and the `getEditorTheme()` /
      `getMarkdownTheme()` / `getSelectListTheme()` /
      `getSettingsListTheme()` helpers that bridge the palette to
      the tui crate's theme interfaces. This work belongs on the
      agent side, not in `aj-tui`.

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
  them. Page size = `max_visible_lines` (auto-sized from terminal
  rows as `max(5, floor(rows*0.3))` per F2 in Phase F).
- `CancellableLoader`, `SelectList`, `SettingsList`: Ctrl+C now
  cancels alongside Escape (matching the original framework's
  `tui.select.cancel = ["escape", "ctrl+c"]` default).
- `Input`: Ctrl+C fires `on_escape` instead of bubbling to the
  parent (matching the original framework's Input).

Defaults are otherwise unchanged: every key arm in the previous
hand-rolled matches resolves through the registry to identical
behavior.

---

## Phase H: Wiring polish (open)

An end-to-end audit of the keybindings and theming surfaces
(both flagged by the user as needing to be properly wired)
found that defaults match exactly, themes have no hardcoded-
style leaks in render bodies, and 4 of 5 components route
input through the registry cleanly. The items below are the
remaining gaps.

### H1. Editor jump-mode Esc bypasses `tui.select.cancel`

- [x] Resolved by removing the explicit Esc cancel branch from
      `src/components/editor.rs` jump-mode entirely (rather than
      replacing it with `kb.matches`). **Pi-parity move.** Pi-tui's
      jump-mode loop (`editor.ts:534-556`) has *no* explicit
      `tui.select.cancel` match — only the same-hotkey cancel,
      the printable-jump arm, and a silent fallthrough that
      clears `jumpMode` for any control character. The original
      H1 plan ("replace with `kb.matches(event,
      "tui.select.cancel")` to match how every other modal in
      the codebase handles cancel") was framed against aj-tui's
      *internal* consistency, but a side-by-side read of pi
      surfaces the deliberate asymmetry: pi's autocomplete /
      SelectList / SettingsList / CancellableLoader / Input all
      route cancel through `kb.matches`, but jump-mode does not.
      Adding `kb.matches` here would have introduced an explicit
      consume the parent surface couldn't see, diverging from pi
      in the opposite direction. Comment block above the
      jump-mode `if let` cross-references `editor.ts:534-556`
      and notes the deliberate exception.
- [x] Regression tests in `tests/editor_character_jump.rs`
      (`jump_mode_does_not_consume_escape_for_pi_parity`,
      `jump_mode_does_not_consume_user_bound_cancel_for_pi_parity`)
      lock in the parity shape: Esc in jump-mode returns `false`
      (silent fallthrough; parent can handle Esc), and a user
      who rebinds `tui.select.cancel` to `ctrl+g` sees the same
      shape — Ctrl+G in jump-mode also returns `false`, since
      jump-mode does not consult the registry's cancel binding.
      The existing `cancels_jump_mode_on_escape_and_processes_the_escape`
      test (which asserts behavioral outcome, not the return
      value) continues to pass — its name was already
      anticipating this shape ("processes the escape" = the Esc
      falls through, not consumed).

### H2. Editor `disable_submit` Enter check hardcodes `KeyCode::Enter`

- [x] Resolved by removing the disable_submit-newline arm entirely
      and short-circuiting the submit branch on `disable_submit`,
      mirroring pi-tui exactly. **Pi-parity move.** A side-by-side
      read of pi (`/tmp/pi-mono/packages/tui/src/components/editor.ts`)
      shows pi has *no* "submit-key becomes newline under
      `disableSubmit`" arm. Pi's structure (`editor.ts:716-749`):
      ```typescript
      // newline branch (lines 717-732) — fires regardless of disableSubmit
      if (kb.matches(data, "tui.input.newLine") || ...byte forms...) {
          this.addNewLine();
          return;
      }
      // submit branch (lines 735-749)
      if (kb.matches(data, "tui.input.submit")) {
          if (this.disableSubmit) return;  // silent no-op, consumes the key
          ...
      }
      ```
      The Rust port now has the same structure. Newline is just
      `kb.matches(event, "tui.input.newLine") || is_newline_event(event)`
      — no disable_submit arm. The submit branch tests
      `self.disable_submit` *inside* and returns `true` early
      (consumed-but-no-op, the Rust equivalent of pi's bare
      `return;`). Comment block above the `is_newline` line
      cross-references `editor.ts:716-732` and the comment above
      the submit branch cross-references `editor.ts:735-749`. The
      original H2 plan listed only "route through `tui.input.submit`"
      vs. "document as deliberate"; the pi cross-check (requested by
      the user post-implementation) surfaced this third option as
      strict parity, with a small enough blast radius (4 plain-Enter
      calls in `editor_paste_marker.rs` to switch to Shift+Enter, plus
      the in-module `test_editor_newline`) that parity was the right
      move.
- [x] Three regression tests in `tests/editor_newline_fallback.rs`
      lock in the parity shape:
      `default_submit_is_silently_consumed_in_disable_submit_mode`
      asserts plain Enter under default config returns `true` from
      `handle_input` (consumed) but doesn't insert a newline; the
      same test asserts Shift+Enter then *does* insert a newline via
      the `tui.input.newLine` registry path.
      `rebound_submit_is_silently_consumed_in_disable_submit_mode`
      rebinds submit to ctrl+enter and confirms the rebound key is
      consumed silently with no newline insertion.
      `plain_enter_falls_through_when_submit_is_rebound_away_in_disable_submit_mode`
      proves the registry-driven shape: with submit rebound away
      from Enter, plain Enter doesn't match anything and
      `handle_input` returns `false` (parent surface can handle it).
      Existing F17 byte-form tests
      (`raw_lf_inserts_a_newline_in_disable_submit_mode`,
      `alt_enter_inserts_a_newline_in_disable_submit_mode`,
      `ctrl_j_inserts_a_newline_in_disable_submit_mode`) continue to
      pass — the byte-form fallback is unchanged. Test fixture
      tweaks: `editor_paste_marker.rs` `preserves_sticky_column_*`
      tests now use `Key::shift_enter()` to insert newlines (was
      `Key::enter()`, which used to coast on the Rust-port
      divergence); the in-module `test_editor_newline` likewise
      sends Shift+Enter and notes the parity rationale in a
      comment.

### H3. Expand user-override coverage to all five components

- [x] `tests/keybindings_user_overrides.rs` previously claimed to
      cover Editor, Input, SelectList, SettingsList, and
      CancellableLoader, but only Editor had real assertions. Added
      one rebind-and-fire test per remaining component, mirroring
      the existing Editor pattern (`#[serial(global_keybindings)]`
      plus `keybindings::reset()` bookends). Each test asserts that
      the rebound key fires the action AND that the previous default
      no longer fires it (via the wholesale-replace semantics of
      `set_user_bindings`):
      - Input
        (`rebinding_tui_editor_delete_char_backward_takes_effect_on_input`):
        rebind `tui.editor.deleteCharBackward` to `ctrl+h`. Asserts
        ctrl+h is consumed and deletes one grapheme; plain
        `backspace` is not consumed and the value is unchanged.
      - SelectList
        (`rebinding_tui_select_up_takes_effect_on_select_list`):
        rebind `tui.select.up` to `ctrl+p` on a 3-item list with
        selection pre-set to index 1. Asserts ctrl+p is consumed and
        moves selection to index 0; plain `up` is not consumed and
        selection stays put. Reads selection via `selected_item()`
        (the public getter — no `selected_index()` is exposed).
      - SettingsList
        (`rebinding_tui_select_confirm_takes_effect_on_settings_list`):
        rebind `tui.select.confirm` to `tab`. Asserts tab is consumed
        and cycles the selected item's value via `on_change` (which
        is SettingsList's confirm callback — there is no separate
        `on_confirm`); plain `enter` is not consumed and `on_change`
        does not fire a second time. Note: Space is hardcoded as a
        confirm alias regardless of the registry; that invariant
        stays covered by the Phase D settings_list tests.
      - CancellableLoader
        (`rebinding_tui_select_cancel_takes_effect_on_cancellable_loader`):
        rebind `tui.select.cancel` to `ctrl+g`. Asserts ctrl+g is
        consumed, `on_abort` fires once, and the cancellation token
        flips to cancelled; neither `escape` nor `ctrl+c` (the two
        previous defaults) is consumed afterwards and `on_abort`
        does not fire a second time.

### H4. `markdown::PreStyle` decision

H4 originally tracked two decisions (the `MarkdownTheme.hyperlinks`
field shape, and the `markdown::PreStyle` shape). The first item was
resolved by removing the field for strict pi parity (see below);
this section now tracks just the remaining `PreStyle` decision.

- [x] `MarkdownTheme` had a `hyperlinks: bool` field
      (`src/components/markdown.rs`) with no counterpart in pi-tui's
      `MarkdownTheme` interface
      (`packages/tui/src/components/markdown.ts:53-71`). Pi gates OSC
      8 emission inline at the link-render site by reading
      `getCapabilities().hyperlinks` directly (`markdown.ts:492`).
      The field was removed for strict struct-parity.

      **Decision: remove for parity.** A first-pass attempt to keep
      the field landed with a "tests can flip per-instance without
      mutating the global cache" justification, but a cross-check
      against pi-tui's tests
      (`packages/tui/test/markdown.test.ts:1093-1198`) showed pi
      tests **do** mutate the global cap cache via `setCapabilities`
      / `resetCapabilitiesCache` — there is no per-theme override on
      pi's side. Pi's `defaultMarkdownTheme` is constructed once and
      reused across the entire `describe("Links")` block; only the
      cap cache changes between tests. Our crate already had every
      piece of pi-equivalent surface (`set_capabilities`,
      `reset_capabilities_cache`, `#[serial_test::serial]` plumbing
      in `tests/capabilities.rs::isolated_env`), so the per-theme
      override was duplicative state.

      The "agent layer wants per-component override" half of the
      original justification was speculative: zero in-tree consumers
      outside `tests/capabilities.rs`, and the cap reflects a *terminal*
      property (does this terminal support OSC 8) so two markdown
      components rendering to the same terminal naturally share it.

      **Resolution.** Three structural changes:

      1. Removed `pub hyperlinks: bool` from `MarkdownTheme` and
         changed `Markdown::render_link` to read
         `get_capabilities().hyperlinks` directly. Mirrors pi's
         `markdown.ts:492` byte-for-byte. The render-time read is
         cheap (one `RwLock::read` over a process-wide cache).
      2. Migrated the five cap-state-dependent link-rendering tests
         from `tests/markdown.rs`
         (`shows_url_in_parentheses_when_hyperlinks_are_not_supported`,
         `shows_mailto_url_in_parentheses_when_hyperlinks_are_not_supported`,
         `emits_osc_8_hyperlink_sequence_when_terminal_supports_hyperlinks`,
         `uses_osc_8_for_mailto_links_when_terminal_supports_hyperlinks`,
         `uses_osc_8_for_bare_urls_when_terminal_supports_hyperlinks`)
         to `tests/capabilities.rs`, where the serial-gating and cap
         cache mutation infra already lives. Mirrors pi's structural
         layout (pi puts these in `markdown.test.ts`'s
         `describe("Links")` block with `afterEach(resetCapabilitiesCache)`;
         our crate has a dedicated cap-cache test binary instead).
         The two `does_not_duplicate_*` tests stay in `markdown.rs`
         because both render branches return the URL exactly once
         (autolink + mailto-strip shortcuts are independent of
         `hyperlinks`).
      3. Replaced the previous two redundant render-branch tests in
         `tests/capabilities.rs`
         (`markdown_render_emits_osc_8_when_theme_hyperlinks_is_true` /
         `markdown_render_falls_back_to_parens_when_theme_hyperlinks_is_false`)
         with the migrated 5 — the previous tests were duplicates of
         two of the migrated ones with the per-theme-field shape
         instead of cap-cache mutation. Net test count: -2 redundant
         + 5 migrated = +3.

      Examples and PARITY.md updated:
      `examples/chat_simple.rs` no longer seeds a per-theme
      `hyperlinks` field; `examples/PARITY.md` documents the
      cap-cache read at the link-render site as the single source of
      truth.

- [x] `markdown::PreStyle` decision. **Resolved.** The Rust-side
      analog of pi-tui's `DefaultTextStyle` interface
      (`/tmp/pi-mono/packages/tui/src/components/markdown.ts:34-47`)
      is now named `DefaultTextStyle`, exposes five of pi's six
      fields, and routes through the configured [`MarkdownTheme`]
      for every text decoration the same way pi does.

      **Decision 1: Naming. Renamed `PreStyle` → `DefaultTextStyle`.**
      Pi-name parity removes the most visible drift. A reader
      following along from pi-tui now finds the same identifiers on
      both sides (the `Markdown::default_text_style` field, the
      `default_text_style: Option<DefaultTextStyle>` constructor
      argument, and the `apply_default_style` private helper). The
      rename was mechanical: ~3 files, no semantic change. Test
      function names that read "`pre_styled_*`" stay as-is — they
      use "pre-styled" as an adjective describing the rendering
      pattern (paragraphs that get an outer wrap before the inline
      walk), not as a reference to the type name; renaming them
      would not improve clarity. The single test name that
      explicitly referenced the renamed method
      (`apply_pre_style_open_order_matches_pi_default_text_style`)
      was renamed to `apply_default_style_open_order_*`.

      **Decision 2: Field-set. Mirrored every pi field except
      `bgColor`.** A pi cross-walk found that pi's
      `DefaultTextStyle` exposes six fields (`color`, `bgColor`,
      `bold`, `italic`, `strikethrough`, `underline`) but the only
      production consumer (pi-coding-agent) uses `color + italic`
      only — and uses a `Box` wrapper, not `bgColor`, when it
      needs per-message backgrounds. Pi's tests exercise only
      `color + italic` too. The first-pass H4 landing kept the
      narrow 2-field shape on that basis, but on review the public
      *interface* matters more than the internal consumer
      pattern: a future agent-side caller porting from a pi
      consumer would expect to write `{ bold: true, italic: true,
      ..Default::default() }` against our struct. Adding the four
      missing decoration fields is mechanical (~10 lines of
      struct + apply branches), so the parity move beats the
      "narrow scope" instinct here.

      What landed: `bold`, `italic`, `strikethrough`, `underline`
      as `bool` fields next to `color`, plus a `#[derive(Default)]`
      so callers can use struct-update syntax matching the TS
      ergonomics (`{ color: x, italic: true, ..Default::default() }`
      mirrors `{ color: x, italic: true }` against pi's
      all-optional interface).

      **Decision 3: `bgColor`. Deferred with explicit consumer
      cross-check.** Pi-coding-agent does not use
      `defaultTextStyle.bgColor` in production — every site that
      wants a per-message background wraps the [`Markdown`] in a
      `Box(paddingX, paddingY, bgFn)` (pi's `Box`, our
      [`crate::components::TextBox`]) and lets the box do the
      bg-fill at its own padding stage. So omitting `bgColor` from
      our `DefaultTextStyle` does not block any known pi usage
      pattern; the `Box`-wrapper approach already works in our
      port. That's the consumer-side justification.

      The implementation-side justification: pi routes
      `defaultTextStyle.bgColor` through the margin/padding-fill
      stage at the top of [`Markdown::render`] (`markdown.ts:164`:
      `const bgFn = this.defaultTextStyle?.bgColor;`), not via
      `applyDefaultStyle`. Wiring it up here would also require
      closing a separate, pre-existing gap: our `Markdown::render`
      emits a left padding only (`padding` prefix on each line),
      while pi emits `leftMargin + line + rightMargin` then pads to
      `width` and applies `bgFn` over the whole row
      (`markdown.ts:161-191`). That's a wider component-
      architecture parity gap (a separate work item, not a
      `DefaultTextStyle` field). Revisit when that work item
      lands; the field can be added then with bounded blast
      radius.

      **Decision 4 (incidental): Apply logic moved to `Markdown`
      and theme-routed.** Pi's `applyDefaultStyle` lives on the
      `Markdown` class and uses `this.theme.bold` /
      `this.theme.italic` / etc. — the apply path is theme-routed
      so a custom theme can override the SGR shape of each
      decoration. The pre-H4 Rust port had `PreStyle::apply` doing
      the work itself with hardcoded `crate::style::italic` calls,
      a silent divergence (byte output is identical against the
      default theme but a custom italic styler wouldn't fire). H4
      moves the logic to `Markdown::apply_default_style`,
      removing `DefaultTextStyle::apply`, and routes through
      `self.theme.bold` / `self.theme.italic` /
      `self.theme.strikethrough` / `self.theme.underline`. The
      [`DefaultTextStyle`] type stays as a pure-data struct
      (idiomatic Rust: data lives on the value, behavior lives on
      the consumer that has the relevant context). Pi-aligned
      apply order: `color → bold → italic → strikethrough →
      underline` per `markdown.ts:218-234`.

      **Tests.** No new tests added — pi has zero tests for
      `bold` / `strikethrough` / `underline`, and the existing
      `apply_default_style_open_order_matches_pi_default_text_style`
      test pins down the color-vs-italic byte structure already.
      Adding "smoke each new boolean fires through" tests would be
      cruft per the workflow guidance; the rustdoc on
      [`DefaultTextStyle`] documents the apply order and the
      delegation through the theme. Existing
      `pre_styled_paragraph_*` tests continue to exercise the
      `color + italic` path that pi-coding-agent actually uses,
      keeping the regression surface focused on real-world usage.

      **Documentation.** All decisions are documented on
      [`DefaultTextStyle`]'s rustdoc with explicit cross-references
      to pi's `markdown.ts` line numbers, the F-items that touch
      this type (F41 inline-context machinery, F45 heading-branch
      scope, F47 field ordering), and the rationale for the
      `bgColor` deferral including the pi-coding-agent
      `Box`-wrapper observation. The F49 historical note in
      [`Markdown`]'s rustdoc preserves the previous `set_pre_style`
      reference with a note pointing at the H4 rename for reader
      continuity.

### H5. Optional ergonomics polish

- [x] `SettingsList::with_default_theme(items, ...)` constructor
      to mirror the `Editor` / `SelectList` / `Markdown` shape.
      **Retired as superseded by F33 + F49.** When this item was
      filed, `Editor`, `Markdown`, and `SelectList` all shipped
      `Default`-driven `new()` constructors and a parallel
      `with_theme(...)` for explicit themes; the gap was that
      `SettingsList` had only the explicit-theme shape. F33
      removed every `Default` impl on the theme structs (the tui
      crate is palette-agnostic by design — pi-tui's components
      don't ship upstream defaults either, themes are a required
      argument and the agent layer assembles them from a central
      palette). F49 then collapsed each component down to a
      single `new(items, ..., theme, ...)` entry point matching
      pi byte-for-byte; the `with_theme` duals are gone. So both
      premises of the item are now false: there is no
      `SettingsListTheme::default()` to call, and the four
      components are uniform — every `new()` takes the theme as
      a required argument. Adding a `with_default_theme` to
      `SettingsList` alone would re-introduce the asymmetry F33
      / F49 deliberately removed and would require resurrecting
      a hardcoded default theme that contradicts the
      palette-agnostic contract. Pi cross-check
      (`packages/tui/src/components/settings-list.ts:48-65`)
      confirms: pi's `SettingsList` has exactly one constructor
      that takes `theme: SettingsListTheme` as a required
      argument, no default-theme variant, no factory. No code
      change; the design decision has already landed.
- [x] `cursor_style` hook on `EditorTheme` (and an implicit
      Input theme) for terminals that want themed cursor cells.
      **Retired as a Rust-side divergence with no pi backing.**
      Pi cross-check: pi's `EditorTheme`
      (`packages/tui/src/components/editor.ts:200-203`) has only
      `borderColor` and `selectList` — no `cursorStyle` /
      `cursorColor` / `cursor` field of any kind. Pi's `Editor`
      hardcodes the cursor cell as `\x1b[7m{grapheme}\x1b[0m`
      (`editor.ts:488` and `editor.ts:493`). Pi's `Input` has no
      theme at all (`packages/tui/src/components/input.ts`) and
      hardcodes `\x1b[7m{atCursor}\x1b[27m` at line 493. So
      adding a `cursor_style` hook on either side would be a
      pure Rust-side divergence — there's no pi consumer to
      port from, no pi parity claim to make. The user-side
      benefit (alternate-cursor themes) is real but speculative,
      and the implementation cost (touching `EditorTheme` and
      adding a new `InputTheme` from scratch) is non-trivial.
      Revisit if a concrete agent-side caller surfaces a
      requirement; until then, ours matches pi's hardcoded
      reverse-video and that's the reference behavior.

### H6. Lazy `RenderHandle` wiring vs pi-tui's required-at-construction shape

**Status: complete. Migrated to pi-tui's required-at-construction
shape (Option A in the original plan).**

`Editor::new`, `Editor::with_theme`, `Loader::new`, and
`CancellableLoader::new` now take a [`RenderHandle`] as their first
argument. Standalone callers (tests, isolated component construction)
pass [`RenderHandle::detached`], a new no-op handle whose
`request_render` is a silent drop and whose `terminal_rows` /
`terminal_columns` read `0`. Production code passes `tui.handle()`
directly.

Removed surface:

- [x] `Component::set_render_handle` is gone from the trait. The two
      impls that used it (Editor, Loader) take the handle at
      construction instead.
- [x] `Container`'s `render_handle: Option<RenderHandle>` field, the
      `install_handle_on` helper, and the trait impl that forwarded
      to children are gone. `add_child` / `insert_child` are now
      pass-through pushes; children arrive with their own handles.
- [x] `Tui::new`'s `root.set_render_handle(...)` call is gone.
- [x] `Tui::show_overlay`'s post-construction handle injection is gone
      (the `mut component: Box<dyn Component>` parameter is back to
      plain `component`).
- [x] Loader no longer has a `set_render_handle`-triggered animation
      pump; the pump is now kicked from `new` / `start` / `set_indicator`.
- [x] `Editor::Default` is gone (the constructor takes a required
      argument; `Default::default()` had no useful meaning).
- [x] The `if let Some(handle) = render_handle` guards in
      `Editor::dispatch_autocomplete_request` and
      `make_autocomplete_notify` are gone. `autocomplete_render_handle`
      is now `RenderHandle` (not `Option<RenderHandle>`).

Added surface:

- [x] [`RenderHandle::detached`] — no-op handle for tests and
      isolated construction. Documented as such on its rustdoc.

Test migration:

- [x] All 67+ `Editor::new()`, `Loader::new("...")`,
      `CancellableLoader::new("...")` call sites in `tests/` and
      `examples/` updated to pass a handle. Tests with a Tui pass
      `tui.handle()`; standalone tests pass
      `RenderHandle::detached()`. The
      `max_visible_lines_falls_back_to_five_with_no_render_handle`
      test was renamed to
      `max_visible_lines_falls_back_to_five_with_a_detached_handle`
      and re-grounded on the detached-handle case (the underlying
      behavior — `terminal_rows() == 0` falls back to 5 — is
      identical).

The five concrete costs flagged in the original plan are all closed:
the Loader's animation-pump multi-site re-pump is gone, the dead-code
`if let Some` guards in Editor are gone, the Container's forwarding
plumbing is gone, the `max_visible_lines` default-case story is now
"detached handle reads 0 rows" (same observable outcome but uniform
with the with-Tui case), and a future contributor adding a new
component that needs a handle has to pass one at construction — no
silent degradation if they forget to override a trait method.

The original plan estimated ~70 call sites to update. The actual
count was 96 across 24 test files and one example. Mechanical
refactor; no behavior change on the production happy path.

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
