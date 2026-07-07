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

**Bottom-anchored transcript.** When the transcript is shorter than the viewport,
entries sit at the bottom of the chat slot, so the first message lands right
above the editor and later messages grow upward. This is the feel `aj` gets for
free from native scrollback. A `ListView` top-aligns by default, so the chat view
pads the top with the unused height (or anchors its layout to the bottom edge)
until the content is tall enough to fill the slot and scroll. Follow-tail keeps
the bottom pinned once the content overflows.

**Scrollbar thumb.** The chat view shows a vertical scrollbar thumb so the
user can see how much transcript is above and below the viewport, and drag
it to jump. `vxfw`'s `ScrollBars` widget already wraps a scrollable view
with a draggable thumb sized from an estimated content extent, so the chat
`ListView` is wrapped in one. The thumb is only drawn when the transcript
overflows the viewport (content taller than the slot), matching how a
native scrollbar hides itself for short content. Dragging the thumb moves
the viewport directly and disengages `follow_tail`; dragging or scrolling
back to the bottom re-engages it, the same rule as wheel and page-key
scrolling. This is a position affordance only, it does not change the
follow-tail, scroll-input, or selection model above.

**Scroll input routing.** The editor is normally focused (it wants arrows and
line-editing keys), so transcript scrolling must work without stealing the
editor's keys. Only keys the `TextArea` does not already bind can drive
chat-scroll while the editor is focused. The rest are reachable from
transcript-focus mode below, where the editor is not focused.

- **Mouse wheel** over the chat area scrolls it. This routes by hit-test: the
  wheel event lands on the `ListView` under the pointer, per Spec A's mouse
  dispatch, regardless of keyboard focus.
- **PageUp / PageDown** page the transcript up and down. They are host-intercepted
  ahead of the editor (whose `TextArea` would otherwise consume them for its own
  multi-line scroll), so a page turn works even while composing.
- **Shift+Home / Shift+End** jump to the top and bottom of the transcript. Plain
  Home / End are the editor's line-start / line-end chords (`Ctrl-A` / `Ctrl-E`),
  so we take the Shift-modified variants, which the editor does not bind, for the
  absolute top / bottom jump. This gives keyboard users a scroll-to-top /
  scroll-to-bottom without first entering transcript-focus mode.
- **Alt+k / Alt+j** scroll the transcript up and down one line, for fine reading
  of a long message without paging. The editor's Alt chords are word-motion and
  word-kill (`Alt-b` / `Alt-f` / `Alt-d` / `Alt-Backspace`) plus `Alt-y`. It does
  not bind `Alt-j` / `Alt-k`, so they are free for line-scroll. (`Ctrl-j` is the
  editor's newline, so the line-scroll keys are the Alt variants, not the Ctrl
  ones.)

Plain Home / End and half-page (`Ctrl-U` / `Ctrl-D`) cannot double as
editor-focused chat-scroll keys, because they are editor chords (line start / end,
kill-to-start, delete-forward). Home / End are handled inside transcript-focus
mode instead, and half-page scroll is deferred until there is a free binding for
it.

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
layout-driven sizing, which is the natural vxfw model. Until the `TextArea` port
lands (Spec B), `aj-next` fills the Editor slot with a stopgap single-line
`TextField`, so multi-line editing, history, paste markers, jump mode, and
autocomplete are not present yet.

**The splash / empty state.** Before the conversation has any user or assistant
message, the chat slot shows a splash rather than a bare scroll area. It has
three parts:

- An animated `aj` logo that drifts slowly around the slot and pulses larger and
  smaller, driven off the frame tick the async driver already runs (Spec A), so
  it costs a periodic redraw and no extra thread.
- A hint line, `Ctrl+O for commands`, with `Ctrl+O` bold and in the
  keybinding-hint palette color (the same `#275DD0` token the command palette's
  shortcut column uses).
- A bordered box holding the startup notices and warnings `aj` shows at launch:
  the config diagnostics, the `Context:` list of stitched-in prompt / AGENTS.md
  files, the sandbox / no-permissions warning, the auth warning, the tmux options
  warning, and any skill warnings. These come from the same sources `aj` uses
  (the `build_warning` tmux helper and the auth / sandbox / context builders in
  `aj-app`).

Those notices are the transcript's leading `Notice` entries (Spec C), so the box
is just an alternate presentation of them. When the first message dismisses the
splash, the same notices render as the normal leading rows of the transcript and
scroll into history, so nothing is stored twice.

## 5. Overlays and the modal stack

`vxfw` has no built-in modal system. We compose one from what it has: z-indexed
`SubSurface`s and focus.

**Drawing.** When the overlay stack is non-empty, the root draws, above the base
layout, (1) a full-viewport **transparent** scrim `SubSurface` and (2) the top
overlay as a `SubSurface` at a higher `z_index`, positioned from its anchor. The
scrim paints nothing (an empty-buffer surface blits no cells but still
hit-tests), so the overlay window floats over the fully visible base layout,
the same composition `aj` uses (it draws no backdrop either). Dimming the base
is not expressible anyway: surfaces composite by opaque cell blits, and cells
styled with default or indexed colors have no known RGB to scale. The scrim's
job is purely behavioral, blocking mouse input from reaching the base layout.
Only the top overlay is drawn, matching `aj`'s "push hides the parent" behavior.
Anchor and sizing port from `aj`'s `OverlayOptions`
(center anchor, width as a percentage with min/max, capped height) into a small
placement config the root uses to compute the overlay's origin from the terminal
size. Each overlay window carries one column of horizontal padding inside its
border, so a content row reads as border, space, content, space, border. This
matches `aj`'s overlay window, whose inner width is the frame width minus four
(two border columns plus the two padding columns), and which also insets the body
with a blank row above and below the content.

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
filter plus a `ListView`, themed from the shared palette). The filter box shows a
`search: ` prefix before the typed query, matching `aj`. The settings window is
a settings-list widget that shows the selected item's help text below the list in
a muted tone, updating as the selection moves, as `aj` does. The login dialog and read-only pages (auth status, usage,
session info) are simple content overlays. The help screen is a read-only page
too, with its own grouped layout (see "The help screen" below). Each is a `vxfw`
widget in `aj-next`.

Across list overlays, metadata columns (a command's category, a usage row's
provider) are right-aligned and drawn in a dim tone (`aj`'s dim prefix column),
so the label column stays vertically aligned across rows. The row description,
the scroll-info line, and the no-match text are a step lighter, in `Muted`
(`#808080`), the same split `aj` draws, so `aj-next` keeps the two grays distinct
rather than collapsing them into one (Spec D). Read-only and
scrollable overlays (help, usage, session info) scroll line by line with the
configured up and down bindings (default `up` / `ctrl+p` and `down` / `ctrl+n`,
resolved through the keymap so a rebind carries), on top of PageUp / PageDown /
Home / End.

**Selection highlight.** The selected row in every list-style overlay is drawn as
a full-width colored band. The whole row (the filter-matched label, the dim
category column, and any right-aligned shortcut) is painted over the theme's
`selectedBg` background and keeps its normal foreground colors on top. This
replaces `aj`'s marker style, an accent `→ ` prefix plus a bold accent label that
leaves the rest of the row unpainted, with a bar the eye tracks at a glance, which
reads better on the fixed viewport. `selectedBg` is the palette token already
defined for this purpose (`ThemeBg::SelectedBg`, documented as the selected-row
background in select-list overlays), so this is the existing theme's highlight
color, not a new one. It moves to `aj-app` with the palette core (Spec D), and
`aj-next`'s theme builder maps it to a `vaxis` background `Style`. The band spans
the overlay's inner content width so selection reads as a filled row rather than
tinted text. Read-only pages (help, usage, session info) have no selected row and
draw no band.

NOTE: `ThemeBg::SelectedBg` exists in the palette and both bundled themes
(`#3a3a4a` dark, `#d0d0e0` light) but `aj` never actually paints it, its
selectors use the `→ ` marker instead. `aj-next` is the first consumer to render
the token, so no theme file changes are needed.

**Row layout and column styling.** A list row keeps `aj`'s column layout: a
right-aligned metadata column (the command's category, dim), the label, and a
right-aligned shortcut column. `aj-next` diverges from `aj` on weight and the
shortcut color. The label and the shortcut are drawn bold, not only on the
selected row, and the shortcut uses a dedicated keybinding-hint color, `#275DD0`
(RGB 39, 93, 208). That color is a new palette token (Spec D) rather than a
literal, so it themes and downsamples like every other color, and the splash
reuses it for its `Ctrl+O` hint. This is the command palette's styling, and the
same shortcut-column treatment applies to any list overlay that shows one.

**The help screen.** The help overlay is a read-only, scrollable page in the same
grouped style as the reference amp help. A bordered window titled "Help & Keymap"
holds a body split into sections, each a colored section heading followed by two
aligned columns: the key or command on the left in an accent color, and a
description on the right that wraps within its own column when it is long, so
only the description column reflows and the key / command column stays put. A vertical scrollbar thumb appears when the
content overflows (the same `ScrollBars` treatment as the chat, section 1), and
the window subtitle carries the resolved close and scroll hints (the existing
`subtitle_close` style, e.g. `Esc to close`). The page scrolls line by line with
the configured up and down bindings (`up` / `ctrl+p`, `down` / `ctrl+n`) as well
as PageUp / PageDown / Home / End. The sections are:

- **Editor shortcuts**: the `TextArea` editing chords (cursor and word motion,
  kill / yank, history, submit vs newline, jump mode) plus the compose-time
  global chords (open palette, paste image, thinking toggle, tools-expand,
  edit-in-`$EDITOR`).
- **Scrolling & navigation**: the chat-scroll and transcript keys from sections
  1 to 3 (PageUp / PageDown, half-page, Home / End, mouse wheel, transcript-focus
  mode, search).
- **Command palette commands**: one row per entry in the command catalog (Spec D
  `commands.rs`), grouped by category, each with its bound shortcut in the
  right-hand column when the action has one.

This is a wider surface than `aj`'s help, which is a single flat list of palette
commands with the shortcut folded into the description. We keep that list as the
last section and add the editor-shortcut and scrolling sections above it, so the
help is a complete keymap reference rather than only a command index.

**Generated, not hardcoded.** The help content is built from the same
authoritative data the rest of the UI resolves against, never a static snapshot
of key labels. Spec F's "hint labels are resolved, never hardcoded" rule applies
here in full:

- Global-chord and command rows resolve their key labels through the compiled
  `Keymap<AjAction>` (Spec F) via `format_keybinding`, and command rows come from
  the `COMMANDS` catalog with the shortcut column resolved from each command's
  `action_id`, exactly as the palette does. A rebound action relabels its help
  row automatically.
- Editor-shortcut rows come from a binding descriptor table the `TextArea`
  exposes (Spec B), so the editor's own chords are the single source of truth and
  the help cannot drift from what the editor actually does.

Each help row carries a display section and a description alongside its key or
command, so grouping and column layout fall out of the data. Adding a command or
rebinding a chord updates the help with no edit to the help module.

**Login.** The OAuth flow is host-driven exactly as today: the login dialog
overlay is opened, and the host spawns the auth flow and tracks it in its
`select!` (Spec A). `aj-models` owns the `OAuthCallbacks` trait; `aj-next` supplies
its own callbacks impl (the aj-tui `TuiOAuthCallbacks` does not port). The
verification URL is rendered as an OSC 8 hyperlink so it is clickable, gated on
the terminal advertising hyperlink support and falling back to the plain styled
URL otherwise, and it is auto-copied to the clipboard on display and on the copy
chord, matching `aj`.

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
  - Yank the queued message back into the editor with the configured cursor-up
    binding (`up` / `ctrl+p`) when the editor is empty and a message is pending,
    matching `aj`. With a non-empty editor those keys keep their editor role
    (cursor / history up), so the empty-editor guard is what separates a yank
    from navigation. Alt+Up (dequeue) yanks regardless of editor contents. Both
    resolve through the keymap, so a rebind carries.
  - `/` at an empty editor opens the palette via the editor's `on_palette_trigger`
    (Spec B), the legacy trigger kept alongside the primary `Ctrl+O` chord, not a
    global chord.
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
  the chat `ListView` (wrapped in `ScrollBars` for the vertical thumb) with
  follow-tail, wheel + page-key scrolling, and transcript-focus mode (cursor
  navigation). Exit behavior (clean exit + usage banner + resume hint). No
  overlays yet.
- **E2 (with components, plan phase 7):** per-view scroll on `active_view` switch,
  the status/pending/header/footer slots wired to the model, in-app selection
  (mouse drag + auto-scroll, Shift+arrow keyboard selection, select-to-copy via
  OSC 52, off-screen text extraction) plus the structured copy-message /
  copy-code actions, and in-transcript search (the find-bar, match highlighting,
  next/prev) sharing the rendered-text layout. It also brings the splash
  empty-state: the animated `aj` logo, the `Ctrl+O for commands` hint, and the
  bordered startup-notices box.
- **E3 (plan phase 8):** the overlay stack (host `Vec` + scrim + anchored draw +
  focus), the reusable filterable-select overlay (with the full-width `selectedBg`
  selection band), and each selector / dialog on top of it, including the grouped
  help screen generated from the keymap, command catalog, and editor binding
  table. Login flow wired through the host loop.
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
- **E-6. Scrollbar thumb. Resolved: build it.** The chat view shows a vertical
  scrollbar thumb, via the `vxfw` `ScrollBars` widget wrapping the chat
  `ListView`. It is drawn only when the transcript overflows the viewport, gives a
  position affordance and drag-to-jump, and reuses the follow-tail engage/disengage
  rules. Part of E1 so the position affordance ships with the first chat view, not
  deferred to polish.
- **E-7. Selection marker style. Resolved: full-width `selectedBg` band.** The
  selected row in list-style overlays is a full-width band painted over the
  theme's existing `selectedBg` token, with normal foreground colors on top,
  replacing `aj`'s accent `→ ` prefix plus bold label. Reuses the palette token
  already defined for this purpose (`ThemeBg::SelectedBg`), so no theme files
  change. `aj-next` is the first consumer to actually render it.
- **E-8. Help screen. Resolved: grouped keymap reference, generated.** The help
  overlay is a read-only scrollable page grouped into sections (editor shortcuts,
  scrolling & navigation, command palette commands), each a colored heading over
  two aligned columns (key / command, description), in the reference amp style.
  Content is generated from authoritative data (the compiled `Keymap<AjAction>`
  for global chords, the `COMMANDS` catalog for palette commands, and a binding
  descriptor table exposed by the `TextArea` for editor chords), never a static
  label snapshot, per Spec F's "resolved, never hardcoded" rule. This widens
  `aj`'s flat command list into a full keymap reference.
- **E-9. Splash / empty state. Resolved: build it.** Before the first user or
  assistant message, the chat slot shows an animated `aj` logo (slow drift plus a
  grow / shrink pulse off the frame tick), a `Ctrl+O for commands` hint (Ctrl+O
  bold, in the keybinding-hint palette color), and a bordered box surfacing the
  startup notices and warnings (config diagnostics, context files, sandbox /
  no-permissions, auth, tmux options, skills). The box presents the transcript's
  leading `Notice` entries, so they become the normal leading rows once the
  splash is dismissed.
- **E-10. List row styling and the keybinding-hint color. Resolved.** List rows
  keep `aj`'s column layout (right-aligned dim category / metadata, label,
  right-aligned shortcut). `aj-next` draws the label and shortcut bold, and the
  shortcut in a new keybinding-hint palette token, `#275DD0` (RGB 39, 93, 208),
  added to the shared palette and both bundled themes (Spec D). The splash
  `Ctrl+O` hint reuses the token.
