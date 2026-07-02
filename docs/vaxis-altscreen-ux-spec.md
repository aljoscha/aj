# Spec E: alt-screen UX

## Status: proposal (not started)

Companion to `docs/aj-next-vaxis-plan.md`. This spec covers the user-facing
consequences of moving from `aj-tui`'s native-scrollback model to `aj-next`'s
alternate-screen model: how the transcript scrolls, how copy and search work, how
overlays and modal selectors are composed on a fixed viewport, the focus model
across the editor, overlays, and the transcript, and what is left on the terminal
after exit.

This is where the terminal-model change is most visible to users, so it is also
where we most need to be honest about the tradeoffs (copy and after-exit
scrollback both change).

## The shift

`aj-tui` renders the whole transcript into the terminal's normal buffer and lets
the terminal own scrolling and selection. `aj-next` runs on the alternate screen
with a fixed viewport (like the amp CLI). The transcript now lives inside a
scroll container the app owns and scrolls. The terminal's native scrollback and
click-drag selection are no longer the mechanism, so the app provides the
equivalents.

## 1. The chat scroll model

The chat view is a `vxfw::ListView` over the active transcript's entry widgets,
built from `ChatState` (Spec C). `ListView` is the substrate for three reasons:
it scrolls by line (smooth reading of long messages), it materializes items
lazily through `Source::Builder` (a long transcript only builds the visible entry
widgets each frame), and it has a movable item cursor we use for keyboard
navigation and selection. The cursor is hidden (`draw_cursor = false`) in the
default editor-focused mode, so the chat reads as plain free-scroll, and shown
in transcript-focus mode. Scroll position, follow-tail, and the mode are view
state, not terminal state.

**Follow-tail.** The view tracks `follow_tail: bool`, true when the viewport is
at (or within a small threshold of) the bottom. While `follow_tail` is true, new
content (streaming tokens, tool output, new messages) keeps the viewport pinned
to the bottom, so a running turn stays in view. If the user scrolls up manually,
`follow_tail` goes false and new content no longer yanks the viewport, so they
can read history while the agent works. Scrolling back to the bottom re-engages
`follow_tail`. This is the behavior native scrollback gave for free, made
explicit.

**Scroll input routing.** The editor is normally focused (it wants arrows and
line-editing keys), so transcript scrolling must work without stealing the
editor's keys:

- **Mouse wheel** over the chat area scrolls it. This routes by hit-test: the
  wheel event lands on the `ListView` under the pointer, per Spec A's mouse
  dispatch, regardless of keyboard focus.
- **PageUp / PageDown / Home / End** (and optionally Ctrl-U / Ctrl-D for
  half-page) are host-intercepted as chat-scroll commands even while the editor
  is focused, because they are not editor line-editing keys. The host translates
  them into scroll calls on the chat view.

**Transcript-focus mode.** A keyboard mode, entered from the editor (e.g. a chord
or PageUp from an empty editor) and left with Esc, moves focus to the `ListView`
and shows its cursor. In it, the arrow keys / j-k move the item cursor, and
Shift+arrows extend a selection (section 2). Leaving the mode returns focus to the
editor. This gives keyboard-only users full navigation and selection without a
mouse, and it is part of the first cut, not a later add.

**Resize.** The scroll container re-lays-out on `Event::Winsize`; `follow_tail`
keeps the bottom pinned across the resize.

**Per-view scroll.** Switching `active_view` (main vs a sub-agent, Spec C) shows
that transcript. Each view scrolls to its bottom on switch (follow-tail engaged),
which is the simplest coherent behavior.

## 2. Copy and selection

On the alternate screen with mouse reporting enabled, the terminal's own
click-drag selection is captured by the app, so we provide selection and copy
in-app. We build the full thing: free-form selection over the transcript, plus
structured copy affordances, plus the native escape hatch.

**Selection model.** A selection is an anchor and a caret, each an absolute
transcript position `(row, col)` where `row` is the line index within the fully
laid-out active transcript at the current chat width (0 = first line of the first
entry), not a screen row. Storing absolute positions means the selection tracks
the content across scrolling and follow-tail, not the viewport.

**Mouse.** A plain click-drag in the chat area sets the anchor on press and moves
the caret on drag. Dragging past the top or bottom edge auto-scrolls the
`ListView` so a selection can span more than one screen. On release the selection
is copied to the system clipboard via OSC 52 (select-to-copy, matching how most
terminals behave), and it stays highlighted until the next click or Esc clears
it.

**Keyboard.** In transcript-focus mode (section 1), Shift+arrows extend the
selection from the item cursor and a copy key (e.g. `y`) copies it. This is a
mouse-free path to the same selection model.

**Rendering the highlight.** For each visible line whose absolute row falls in
`[min(anchor, caret), max(anchor, caret)]`, the chat view draws those cells with
the selection style. Only visible lines are painted; off-screen selected lines
need no highlight.

**Extracting the text.** The selection may span off-screen lines that the lazy
`ListView` did not render, so we cannot read the text back from the visible cells
alone. On copy we lay out the active transcript into an off-screen surface at the
chat width (reusing the exact entry-drawing code, so wrapping matches the visible
render with no drift), then read the graphemes of the selected cell range row by
row, trimming trailing blanks per line. Copy is a rare, user-initiated action, so
a one-shot full-transcript layout at copy time is an acceptable cost and keeps a
single source of truth for wrapping. `vaxis` already supports drawing into an
oversized off-screen surface. This rendered-text layout is cached and shared with
search (section 3), which matches over the same rows.

**Structured copy.** On top of free-form selection, a copy-message action (copy
the cursored or clicked entry's text) and a copy-code-block action (copy a fenced
block's contents) give exact one-keystroke copies of the things users most often
want. Both go through the same OSC 52 path. The OSC 52 helpers already exist in
`aj` (`auth.rs` / `clipboard.rs`) and move to `aj-app`.

**Native escape hatch.** Shift+drag still bypasses application mouse capture in
essentially every modern terminal, so users retain the terminal's own selection
(including over the editor and chrome) for free. We document it in the help.

## 3. In-transcript search

Native scrollback gave search over history. On the alt screen the app provides it,
as a find-over-the-transcript rather than a modal.

**The find-bar.** Triggering search reveals a slim one-row find-bar, a conditional
layout slot below the chat (not an overlay), so the transcript stays visible and
highlighted underneath. It holds the query input and a match counter (`3/12`) and
takes focus while open.

**Matching.** Search runs over the transcript's rendered-text layout: the active
transcript's rows as plain text at the chat width, the same representation section
2 uses to extract selected text. It is computed when search opens (and on a
transcript or width change) and cached, so each keystroke matches against the
cache instead of re-laying-out the transcript. A match is a range in the same
absolute `(row, col)` space as a selection, so highlighting and scrolling reuse
the selection machinery. Matching is case-insensitive substring with smart-case:
an uppercase character in the query makes it case-sensitive.

**Navigation and highlight.** All matches are highlighted, and the current match
is highlighted distinctly. Next and previous move the current match and scroll the
`ListView` to bring it into view, disengaging follow-tail while searching. Closing
search clears the highlights, restores focus to the previous owner (the editor or
transcript-focus mode), and re-engages follow-tail if the viewport is back at the
bottom.

**Bindings.** Not Ctrl+F: that is `forward-char` in the `TextArea` (Spec B). The
default global trigger is Alt+S (Emacs `M-s` is the search prefix, and it does not
collide with the editor's word-motion chords), rebindable through the keymap (Spec
F). Inside transcript-focus mode (section 1) the editor is not focused, so the
less/vim keys are free: `/` opens forward search, `?` reverse, and `n` / `N` step
through matches. Inside the find-bar, Enter jumps to the next match, Shift+Enter
to the previous, and Esc closes.

## 4. The base layout

The root widget is a `vxfw::FlexColumn` of fixed slots, the alt-screen analog of
`aj-tui`'s `layout.rs`:

| Slot | Widget | Height |
|---|---|---|
| Header | one-line banner (session id + transient notice) | 1 |
| Chat | the scroll container over the active transcript | flex (fills remaining) |
| Search | slim find-bar (only while searching) | 0 or 1 |
| Status | loader/spinner line (empty when idle) | 0 or 1 |
| Pending | pending-message box (empty when none) | auto |
| Editor | the `TextArea` (Spec B) | auto, capped |
| Footer | one-line banner (model / cwd / usage) | 1 |

The chat slot takes the flex space; the editor sizes to its content up to a cap
(Spec B's constraint-based height), so the transcript grows and shrinks as the
editor does. This replaces `aj-tui`'s terminal-height-driven editor sizing with
layout-driven sizing, which is the natural vxfw model.

## 5. Overlays and the modal stack

`vxfw` has no built-in modal system. We compose one from what it has: z-indexed
`SubSurface`s and focus.

**Drawing.** When the overlay stack is non-empty, the root draws, above the base
layout, (1) a dim full-viewport scrim `SubSurface` and (2) the top overlay as a
`SubSurface` at a higher `z_index`, positioned from its anchor. Only the top
overlay is drawn, matching `aj`'s "push hides the parent" behavior, with the scrim
providing the modal backdrop. Anchor and sizing port from `aj`'s `OverlayOptions`
(center anchor, width as a percentage with min/max, capped height) into a small
placement config the root uses to compute the overlay's origin from the terminal
size.

**The stack is host state, and simpler than `aj`'s.** `aj`'s `SelectorStack`
imperatively calls `tui.set_overlay_hidden` / `hide_overlay` to manage visibility.
In `vxfw`, "what is open" is just a `Vec<OpenOverlay>` the host owns, and the root
draws the top from it each frame, so push/back/close_all only mutate the `Vec` and
move focus. No compositor calls. The control-flow model ports directly:

- `push(overlay)` adds a level (the palette chaining into a picked command keeps
  parents, so cancel returns to the palette; the agent picker drilling into the
  task viewer drops parents).
- `back()` pops to the parent overlay, or to the editor if it was the only level
  (Esc / cancel).
- `close_all()` tears the stack down to the editor (a terminal confirm or the
  close-all chord).

**Outcomes via callbacks, not polling.** `aj` polls an `OutcomeSlot<T>` each frame
to learn what a selector decided. `vxfw` widgets already use callbacks
(`on_submit`-style `Box<dyn FnMut(&mut EventContext, ...)>`), so each overlay takes
an on-confirm / on-cancel callback and the host applies the effect directly. The
`SelectorTransition` outcomes (`Back`, `Close(effects)`, `Open { action,
keep_parents }`) and `CloseEffects` (a chat notice, a login launch, a session
switch) are the same host-side control flow as today, driven from those callbacks
instead of a polled slot.

**The overlay widgets.** Most selectors are the same shape: a filter box over a
scrollable list with a highlighted row (command palette, model / thinking / speed
/ verbosity pickers, session switcher, agent picker, prompt-history search). These
build on one reusable `vxfw` filterable-select overlay widget (a `TextField`
filter plus a `ListView`, themed from the shared palette). The settings window is
a settings-list widget. The login dialog and read-only pages (auth status, usage,
session info, help) are simple content overlays. Each is a `vxfw` widget in
`aj-next`.

**Login.** The OAuth flow is host-driven exactly as today: the login dialog
overlay is opened, and the host spawns the auth flow and tracks it in its
`select!` (Spec A). `aj-models` owns the `OAuthCallbacks` trait; `aj-next` supplies
its own callbacks impl (the aj-tui `TuiOAuthCallbacks` does not port).

## 6. Focus model

- **Default focus is the editor** (`TextArea`). The root requests focus into it on
  `Init` (the vxfw idiom).
- **Opening an overlay moves focus to it** (the top of the stack), via
  `ctx.request_focus`. Keyboard events route along the focus path to the overlay
  (Spec A's focus dispatch). Closing returns focus to the parent overlay or the
  editor.
- **Transcript-focus mode moves focus to the chat `ListView`** (cursor shown) for
  keyboard navigation and selection (section 1); Esc returns focus to the editor.
  Overlays take priority: the chord that enters transcript-focus is inert while an
  overlay is open.
- **Global chords go through the `KeymapController`** in vxfw's capture and bubble
  phases (Spec F), not host-side interception. Pre-empting chords (the cancel/quit
  ladder, close-all, leader prefixes) match in the capture phase before the focused
  widget sees them. Shadowable shortcuts match in the bubble phase after the
  focused widget declines. The bindings are the shared keymap data (Spec D), and
  context-sensitivity is an `enabled` predicate per binding rather than scattered
  conditionals:
  - Ctrl+C ladder: cancel the running turn, else arm quit, else quit when idle.
  - Palette open, history open, agent picker: inert while a capturing overlay is
    up.
  - Close-all: only when an overlay is open (else the key falls through, e.g. to
    the Ctrl+C ladder, matching `aj`).
  - Steer submit (Alt+Enter), dequeue (Alt+Up), clipboard image paste (Ctrl+V),
    thinking toggle (Alt+T), tools-expand (Alt+O).
  - `/` at an empty editor opens the palette via the editor's `on_palette_trigger`
    (Spec B), not a global chord.
- **Mouse wheel** routes to the chat scroll by hit-test regardless of focus;
  clicks on interactive widgets (buttons, list rows, sub-agent boxes) route by
  hit-test through Spec A's mouse dispatch.

## 7. Exit behavior

On the alternate screen, quitting leaves the terminal clean: the conversation does
not remain in the user's scrollback the way it does with `aj` today. This is a
deliberate part of the model (it matches amp).

On exit `aj-next` leaves the alternate screen and then prints, to the normal
screen, the shutdown usage banner and the resume hint (the `aj continue <id>`
command), exactly as `aj` does today. The usage-summary and resume-hint
formatters already exist and move to `aj-app`. So the terminal is left clean apart
from the usage summary and the one-line command to pick the session back up. The
session is always on disk and resumable regardless.

## Phasing

Aligns with the plan's phases 5-9.

- **E1 (with the shell skeleton, plan phase 5):** the base layout `FlexColumn` and
  the chat `ListView` with follow-tail, wheel + page-key scrolling, and
  transcript-focus mode (cursor navigation). Exit behavior (clean exit + usage
  banner + resume hint). No overlays yet.
- **E2 (with components, plan phase 7):** per-view scroll on `active_view` switch,
  the status/pending/header/footer slots wired to the model, in-app selection
  (mouse drag + auto-scroll, Shift+arrow keyboard selection, select-to-copy via
  OSC 52, off-screen text extraction) plus the structured copy-message /
  copy-code actions, and in-transcript search (the find-bar, match highlighting,
  next/prev) sharing the rendered-text layout.
- **E3 (plan phase 8):** the overlay stack (host `Vec` + scrim + anchored draw +
  focus), the reusable filterable-select overlay, and each selector / dialog on
  top of it. Login flow wired through the host loop.
- **E4 (plan phase 9):** polish (Shift+drag native selection documented, mouse
  cursor shapes, remaining affordances).

## Decisions

- **E-1. Transcript keyboard focus. Resolved: build it from the start.**
  Editor-focused wheel + page-key scrolling, plus a transcript-focus mode (focus
  the chat `ListView`, show its cursor, arrow navigation, Shift+arrow selection)
  in the first cut.
- **E-2. Copy. Resolved: full in-app selection now, not deferred.** Free-form
  selection (mouse drag with auto-scroll, keyboard Shift+arrow), select-to-copy
  and structured copy-message / copy-code via OSC 52, off-screen text extraction
  via a copy-time off-screen layout, plus Shift+drag native selection as the
  escape hatch.
- **E-3. After-exit screen. Resolved: clean exit + banner + resume hint.** Leave
  the alternate screen and print the shutdown usage banner and the
  `aj continue <id>` resume command to the normal screen, as `aj` does today.
- **E-4. Chat container. Resolved: `ListView`** (`draw_cursor` off by default, on
  in transcript-focus mode), for line-level scrolling, lazy item windowing on
  long transcripts, and the built-in cursor that keyboard navigation and selection
  use.
- **E-5. In-transcript search. Resolved: build it.** A non-modal find-bar over the
  transcript's rendered-text layout, sharing the selection coordinate space and
  highlight machinery. Default trigger Alt+S (not Ctrl+F, which is `forward-char`
  in the editor), plus `/` `?` `n` `N` in transcript-focus mode. Case-insensitive
  smart-case substring, next/prev with scroll-to-match.
