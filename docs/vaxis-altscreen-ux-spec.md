# Spec E: alt-screen UX

## Status: proposal (not started)

Companion to `docs/aj-next-vaxis-plan.md`. This spec covers the user-facing
consequences of moving from `aj-tui`'s native-scrollback model to `aj-next`'s
alternate-screen model: how the transcript scrolls, how copy works, how
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
navigation. In the default editor-focused mode the chat reads as plain
free-scroll with no visible cursor. In transcript-focus mode the item cursor
tracks the focused user message and is drawn as a border around it (section 2),
not a cursor gutter. Scroll position, follow-tail, and the mode are view state,
not terminal state.

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

**Scroll geometry.** The `ListView` scroll position stays index-anchored (the
top in-view entry plus a line offset into it), which pins the viewport to an
entry so an off-screen height change never shifts what the user is reading. On
top of that it keeps a measured-extent geometry purely to size and place the
scrollbar thumb: each entry counts as an estimated extent (the running mean of
the measured entries) until it is laid out at the current width, and its
measured height replaces the estimate as it scrolls through the viewport. A
prefix-sum (Fenwick) tree over the extents answers total-extent and
top-of-viewport offset in `O(log n)`, so the thumb reflects real content height
and position rather than a coarse entry count, and never lays the whole
transcript out. Only the entries drawn each frame (the visible window) are
measured, the rest ride on the estimate and sharpen as they are scrolled into
view. We keep the index-anchored core rather than the absolute-offset one the
reference uses, because index anchoring gives viewport stability for free, and
the absolute model's other benefits (offset-precise scroll-to-match, sub-entry
scroll animation) have no consumer here now that search is out.

**Scrollbar thumb.** The chat view shows a vertical scrollbar thumb so the
user can see how much transcript is above and below the viewport, and drag
it to jump. `vxfw`'s `ScrollBars` widget already wraps a scrollable view
with a draggable thumb sized from the scroll geometry's total extent, so the chat
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
- **Home / End** jump to the top and bottom of the transcript. They are
  host-intercepted in the capture phase ahead of the editor, so they work while
  composing. We deliberately take them from the editor, whose line-start /
  line-end stay on `Ctrl-A` / `Ctrl-E`, because scroll-to-extremes is the more
  useful binding for those keys here. Since they dispatch to the transcript, they
  are mode-aware: in editor mode they scroll the viewport (End re-engages
  follow-tail), in transcript-focus mode they move the message cursor to the
  first / last user message.

**Transcript-focus mode.** A keyboard mode for stepping through past user
messages, entered with Tab (whenever the autocomplete popup is closed) and left
with Esc. Entering moves
focus to the chat `ListView` and lands on the last (newest) user message. In the
mode the navigation unit is the user message, not the individual entry: Tab and
Up (and `k`) step to the next-older user message, Shift+Tab and Down (and `j`)
step to the next-newer one, and Home / End jump to the first / last. Assistant,
tool, and other entries are skipped by the cursor. Wheel and page-key scrolling
still work, so the replies between the user messages the reader steps through
stay readable. Leaving the mode returns focus to the editor. This is part of the
first cut, not a later add.

Tab is the editor's accept key while the autocomplete popup is open, so the
enter-focus binding is gated to the popup being closed and matches in the capture
phase. With the popup open Tab applies the highlighted completion. With it closed
Tab focuses the transcript even when the editor holds a draft, so a draft does not
block peeking at history. Typing `@` opens the popup implicitly, so the editor
does not need Tab to request completions. The focused message is marked by a
border rather than a cursor gutter (section 2), so `draw_cursor` is not used for
this mode.

**Resize.** The scroll container re-lays-out on `Event::Winsize`; `follow_tail`
keeps the bottom pinned across the resize.

**Per-view scroll.** Switching `active_view` (main vs a sub-agent, Spec C) shows
that transcript. Each view scrolls to its bottom on switch (follow-tail engaged),
which is the simplest coherent behavior.

## 2. Copy and selection

On the alternate screen with mouse reporting enabled, the terminal's own
click-drag selection is captured by the app, so we provide selection and copy
in-app. We build free-form mouse selection over the transcript, a whole-message
copy in transcript-focus mode, and the native escape hatch.

**Selection model.** A selection is an anchor and a caret, each an entry-relative
position `(entry_id, offset)`: the entry it lands in and a grapheme offset into
that entry's text laid out at the current chat width. Anchoring to the entry, not
a screen row or a global line number, means the selection tracks the content
across scrolling and follow-tail (the entry keeps its identity as the viewport
moves) and needs no global coordinate space over the whole transcript. This is
the amp selection model: a selection is expressed against the entries it touches,
not a materialized grid.

**Mouse.** A plain click-drag in the chat area sets the anchor on press and moves
the caret on drag. Dragging past the top or bottom edge auto-scrolls the
`ListView` so a selection can span more than one screen. On release the selection
is copied to the system clipboard via OSC 52 (select-to-copy, matching how most
terminals behave), and it stays highlighted until the next click or Esc clears
it.

**Keyboard.** There is no character-level keyboard selection. In
transcript-focus mode (section 1) the copy key copies the whole focused user
message (see "Focused-message marker and copy" below), which covers the common
mouse-free case. Arbitrary sub-message copy is the mouse's job, or the native
Shift+drag escape hatch.

**Rendering the highlight.** The selected span covers the entries from the anchor
entry to the caret entry, with a partial run at the offset boundary in the first
and last entry. For each visible line the chat view maps its screen row back to
the entry and offset it renders, and paints the covered cells with the selection
style. Only visible lines are painted; off-screen selected entries need no
highlight, so the highlight costs nothing beyond the viewport.

**Extracting the text.** A selection spans a known, contiguous run of entries
(anchor entry through caret entry), so copy materializes only those entries, not
the whole transcript. Each entry exposes a text provider that lays its own content
out at the chat width into plain rows (the same wrap the visible render uses, so
there is no drift), cached per `(entry, width)`. Copy walks the spanned entries in
order, takes the covered offset range out of each, and joins them. Unlike amp,
which must keep off-screen selected render objects alive to read their text, our
entries are laid out on demand from `ChatState`, which holds every entry
independent of what the view has realized, so a selection spanning off-screen
entries needs no keep-alive. The cost is bounded by the selection length, not the
transcript length.

**Focused-message marker and copy.** In transcript-focus mode the focused user
message is drawn inside a semi-thick border in the app's highlight color, with a
copy-key hint on the border's bottom edge, the same way an overlay shows its key
hints in its chrome. Pressing that key copies the whole message through the same
OSC 52 path the mouse selection uses. The key label in the border resolves
through the keybinding data (Spec F), never a literal. The border is the `vxfw`
`Border` widget with a bottom `BorderLabel`, wrapping the entry widget only when
that entry is the focused one, so it costs nothing on the other rows. The OSC 52
helpers already exist in `aj` (`auth.rs` / `clipboard.rs`) and move to `aj-app`.

There is no copy-code-block action: reading and copying code out of assistant
replies is served by the mouse selection and the native Shift+drag escape hatch.

**Native escape hatch.** Shift+drag still bypasses application mouse capture in
essentially every modern terminal, so users retain the terminal's own selection
(including over the editor and chrome) for free. We document it in the help.

## 3. In-transcript search

**Not built, not planned.** The reference has none either, and reading back
through history is served by scrolling (section 1) plus mouse selection and the
native Shift+drag escape hatch (section 2). Search is the reason the selection
model stays entry-relative rather than a whole-transcript grid: it was the only
consumer that wanted a single global coordinate space, and with it out nothing
needs a whole-transcript layout. If it is ever revisited it must match over the
same per-entry text the selection extraction produces, walking entries, not a
rebuilt global grid.

## 4. The base layout

The root widget is a `vxfw::FlexColumn` of fixed slots, the alt-screen analog of
`aj-tui`'s `layout.rs`:

| Slot | Widget | Height |
|---|---|---|
| Header | one-line banner (session id + transient notice) | 1 |
| Chat | the scroll container over the active transcript | flex (fills remaining) |
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
  it costs a periodic redraw and no extra thread. It is colored in a ramp of
  lavender / purple shades that cycle with the pulse, defined as named splash
  colors (a decorative gradient, not keybinding-resolved data).
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
a muted tone, updating as the selection moves, as `aj` does. The login dialog
and the read-only content pages (auth status, usage, session info) are content
overlays, detailed under "The read-only content pages" below. The help screen is
a read-only page too, with its own grouped layout (see "The help screen" below).
Each is a `vxfw` widget in `aj-next`.

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

**The read-only content pages.** Auth status, usage, and session info are content
overlays over a scrollable, non-interactive row list wrapped in the standard
overlay window. They read like `aj` under a few rules:

- **Column tinting.** Auth and usage rows are styled spans, not one default-fg
  string. The provider-id column is drawn in the dim tone and the detail or
  status column in `Muted`, the same two-gray split the list overlays use, so a
  provider id, its summary, and its expiry or usage read as three distinct
  columns. Session info stays uncolored in the body, the way `aj` draws it, its
  structure carried by section headers and indentation.
- **Session-info layout.** The digest is grouped into sections (`Session`,
  `Settings`, `Activity`, `Messages`, `Usage`, `Tool calls (N)`) separated by a
  blank spacer row that occupies a real line rather than collapsing, with the
  section header at one indent and its key/value rows a step deeper, matching
  `aj`. Keys are padded to a shared column so values line up across sections.
- **Subtitle from keybinding data.** The window subtitle (`Esc to close`, and the
  back / close split when a distinct close-all chord exists) is built by
  resolving labels through `aj-app`'s keybinding resolver (`format_keybinding` /
  `default_action_shortcut`, `fixed_keys` for the two fixed chords), never a
  free-form literal, the same rule the palette shortcut column already follows
  (Spec F). The overlay's Esc-back and Enter-confirm are fixed `vxfw` widget
  conventions, so their labels come from `format_keybinding` on the canonical
  chord the widget handles. Making those in-widget keys rebindable stays the
  tracked intra-widget-keys follow-up. This fixes only the label so it cannot
  drift from what the widget does.
- **Shared formatting.** The data these pages show already comes from `aj-app`
  (`auth::collect_statuses`, `usage::collect_usage`). The session-info digest's
  row formatting (section order, labels, the token and cost formatting) moves to
  `aj-app` as well, so both binaries render the same digest from one source
  rather than a per-binary copy.
- **Deliberate divergences.** `aj-next` renders these pages at the larger
  centered `Large` placement and shows a scrollbar thumb, rather than `aj`'s
  fixed 22-row frame and `(a-b/total)` text indicator. Both are intentional
  `aj-next` choices (more room for read-only content, a thumb consistent with the
  chat), not parity misses.

**The usage reset flow.** The usage page is not purely read-only. It carries
`aj`'s rate-limit-reset action, so it is an interactive overlay with its own
small state machine rather than the shared read-only content widget. When a
provider reports available resets and has a matching reset source, a bound chord
(default `r`, a rebindable action resolved through the keymap so its footer-hint
label and its handling stay in sync) starts an in-overlay flow that selects the
provider (when more than one is eligible), confirms, consumes the reset off the
UI thread the way the initial usage fetch runs, and reports the outcome. The
eligibility check, the consume call, and the result wording reuse the `aj-app`
usage helpers, so both binaries behave identically.

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
  1 and 2 (PageUp / PageDown, Home / End, mouse wheel, transcript-focus mode).
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
chord, matching `aj`. The dialog subtitle and the input prompts resolve their key
labels through the same keybinding resolver as the other overlays (submit and
cancel through the keymap, the copy chord through `fixed_keys::CTRL_Y`), never a
hardcoded literal.

## 6. Focus model

- **Default focus is the editor** (`TextArea`). The root requests focus into it on
  `Init` (the vxfw idiom).
- **Opening an overlay moves focus to it** (the top of the stack), via
  `ctx.request_focus`. Keyboard events route along the focus path to the overlay
  (Spec A's focus dispatch). Closing returns focus to the parent overlay or the
  editor.
- **Transcript-focus mode moves focus to the chat `ListView`** for stepping
  through past user messages (section 1). Esc returns focus to the editor. Entered
  with Tab, matched in the capture phase, the chord is inert while an overlay is
  open and while the autocomplete popup is open, where Tab applies the completion.
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
  the status/pending/header/footer slots wired to the model, in-app mouse
  selection (drag + auto-scroll, select-to-copy via OSC 52, entry-relative text
  extraction) plus the focused-message border and copy key in transcript-focus
  mode. It also brings the splash empty-state: the animated, lavender/purple `aj`
  logo, the `Ctrl+O for commands` hint, and the bordered startup-notices box.
- **E3 (plan phase 8):** the overlay stack (host `Vec` + scrim + anchored draw +
  focus), the reusable filterable-select overlay (with the full-width `selectedBg`
  selection band), and each selector / dialog on top of it, including the grouped
  help screen generated from the keymap, command catalog, and editor binding
  table. Login flow wired through the host loop.
- **E4 (plan phase 9):** polish (Shift+drag native selection documented, mouse
  cursor shapes, remaining affordances).

## Decisions

- **E-1. Transcript keyboard focus. Resolved: build it from the start.**
  Editor-focused wheel + page-key scrolling and Home / End top / bottom (Home /
  End are taken from the editor for this, its line motions stay on Ctrl-A /
  Ctrl-E), plus a transcript-focus mode entered with Tab (whenever the autocomplete
  popup is closed) that
  steps between past user messages (Tab / Up older, Shift+Tab / Down newer, Home /
  End first / last). The focused message is marked by a highlight-colored border,
  not a cursor gutter. No keyboard character selection.
- **E-2. Copy. Resolved: mouse selection plus focused-message copy.** Free-form
  mouse selection with auto-scroll and select-to-copy via OSC 52, plus Shift+drag
  native selection as the escape hatch. On the keyboard the only copy is the whole
  focused user message, triggered by the copy key shown in that message's border.
  There is no character-level keyboard selection and no copy-code-block action.
  Selection is entry-relative (`(entry_id, offset)`) and text extraction
  materializes only the spanned entries (or the single focused message) via
  per-entry text providers laid out on demand from `ChatState`, so copy never lays
  out the whole transcript and needs no off-screen keep-alive.
- **E-3. After-exit screen. Resolved: clean exit + banner + resume hint.** Leave
  the alternate screen and print the shutdown usage banner and the
  `aj continue <id>` resume command to the normal screen, as `aj` does today.
- **E-4. Chat container. Resolved: `ListView`** for line-level scrolling, lazy
  item windowing on long transcripts, and a movable item cursor. The cursor tracks
  the focused user message in transcript-focus mode and is drawn as a border around
  that message (`draw_cursor` stays off, the gutter is not used), and it drives the
  keyboard navigation.
- **E-5. In-transcript search. Resolved: not now.** Earlier drafts specified a
  find-bar sharing a global transcript coordinate space. That coordinate space is
  what forced select-to-copy to materialize the whole transcript, so with the
  entry-relative selection model (E-2) we drop search rather than keep the
  full-transcript layout alive for it. The reference amp CLI has no in-transcript
  search either; scrolling plus selection and the native Shift+drag escape hatch
  cover reading history. If revisited, search matches over the same per-entry text
  the selection extraction produces, not a rebuilt global grid.
- **E-6. Scrollbar thumb. Resolved: build it.** The chat view shows a vertical
  scrollbar thumb, via the `vxfw` `ScrollBars` widget wrapping the chat
  `ListView`. It is drawn only when the transcript overflows the viewport, gives a
  position affordance and drag-to-jump, and reuses the follow-tail engage/disengage
  rules. The thumb is sized from the scroll geometry's estimated-then-measured
  total extent (section 1), so it sharpens as entries are measured rather than
  staying a coarse item-count guess. Part of E1 so the position affordance ships
  with the first chat view, not deferred to polish.
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
- **E-9. Splash / empty state. Resolved: build it, animated and colored.** Before
  the first user or assistant message, the chat slot shows an animated `aj` logo
  (slow drift plus a grow / shrink pulse off the frame tick, colored in a cycling
  lavender / purple ramp), a `Ctrl+O for commands` hint (Ctrl+O bold, in the
  keybinding-hint palette color), and a bordered box surfacing the startup notices
  and warnings (config diagnostics, context files, sandbox / no-permissions, auth,
  tmux options, skills). The box presents the transcript's leading `Notice`
  entries, so they become the normal leading rows once the splash is dismissed.
- **E-10. List row styling and the keybinding-hint color. Resolved.** List rows
  keep `aj`'s column layout (right-aligned dim category / metadata, label,
  right-aligned shortcut). `aj-next` draws the label and shortcut bold, and the
  shortcut in a new keybinding-hint palette token, `#275DD0` (RGB 39, 93, 208),
  added to the shared palette and both bundled themes (Spec D). The splash
  `Ctrl+O` hint reuses the token.
- **E-11. Read-only overlay parity. Resolved.** The read-only content pages (auth
  status, usage, session info) and the login dialog match `aj` on body styling
  and label resolution, with two ratified `aj-next` divergences. Auth and usage
  tint the provider-id column dim and the detail / status column `Muted` (styled
  spans, not a default-fg dump). Session info keeps `aj`'s uncolored body but
  restores the section spacer rows and indent, and its digest formatting moves to
  `aj-app` alongside the auth and usage data builders it already shares. Every
  overlay subtitle and the login dialog's hints resolve their key labels through
  the keybinding resolver, never literals (Spec F). The login URL is an OSC 8
  hyperlink with a plain fallback (already required in section 5). Ratified
  divergences: these pages keep `aj-next`'s larger `Large` placement (more room
  for read-only content) and the scrollbar thumb rather than `aj`'s 22-row frame
  and `(a-b/total)` indicator. The usage page
  additionally ports `aj`'s rate-limit-reset flow: a bound chord opens an
  in-overlay provider-select / confirm / consume state machine, so the usage
  overlay is interactive rather than a plain content page.
