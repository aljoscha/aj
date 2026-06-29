# Off-screen render skip & clamped straddle repaint — spec

Status: implemented.

## Motivation

The TUI renders the whole UI as one tall, bottom-anchored frame and
relies on the terminal's native scrollback for overflow. The render
engine (`src/aj-tui/src/tui.rs`) keeps the bottom `height` rows of that
frame on the physical screen and lets older rows scroll off into
scrollback. It repaints with a row-level diff against `previous_lines`,
its model of what is currently on the terminal.

A background sub-agent's progress is rendered inline in the main
transcript as a `SubAgentBox`. While the sub-agent streams, the box
keeps mutating, and as the main thread (or a finished turn parked on a
prompt) sits below it, the box scrolls up above the visible viewport.
The diff engine then sees a change whose first row is above the
viewport top and falls back to a full clear-and-repaint
(`\x1b[2J\x1b[H\x1b[3J` + re-emit) on every streamed token. That is the
flicker and the steady CPU burn: the engine repaints the entire visible
screen, at the throttle ceiling, to reflect updates to rows the user
cannot even see.

## The real constraint: reachability, not "off-screen must be redrawn"

The differential renderer updates the screen with relative cursor moves
(`\x1b[nA` / `\x1b[nB`, `\r`, `\r\n`). It cannot address a row that has
scrolled above physical row 0 into native scrollback. That is the only
hard constraint.

The old code turned "the first changed row is unreachable" into
"therefore rebuild the entire frame." Those are two separate questions,
and they should be asked independently per frame:

1. Is the changed content actually visible?
2. Does the frame need to scroll to stay bottom-anchored?

A full redraw is only forced when the answer to (2) is yes and the
scroll cannot be driven from the visible region.

Leaving off-screen rows untouched is consistent with how the engine
already treats history. A normal differential render never rewrites
scrollback. It only ever rewrites visible rows and lets new content
scroll into history naturally. The scrollback wipe (`\x1b[3J`) happens
only on the full-redraw paths, as a side effect of rebuilding from the
top. So scrollback is already a historical record of what was on screen,
not a live re-render of current state.

## The three cases

Let `(first, last)` be the changed row range from `compute_diff_range`
and `vt` the effective viewport top (logical row currently at physical
row 0). The engine classifies a change relative to the viewport:

1. **Entirely above the viewport** (`last < vt`): invisible. Paint
   nothing. This is the dominant steady-state case for a saturated,
   off-screen sub-agent box.
2. **Straddles the viewport top** (`first < vt <= last`): part is in
   scrollback (unreachable), part is visible.
   - If the line count is unchanged (`lines.len() ==
     previous_lines.len()`), nothing scrolls, so the visible rows kept
     their screen positions. Clamp the repaint to start at `vt` and
     rewrite only `[vt, last]`. Leave the off-screen rows stale.
   - If the line count changed above the fold, a net insert or delete
     forces the whole frame to scroll to stay bottom-anchored, and the
     moved rows are unreachable. This is the one case that genuinely
     still needs a full redraw.
3. **In or below the viewport** (`first >= vt`): the existing
   differential path, unchanged.

`last < vt` (entirely above) implies the line count is unchanged: a net
insert or delete shifts the tail rows, which would surface as changes at
high indices (at or below the viewport, well past `vt`), contradicting
`last < vt`. So the skip case is always a pure in-place change and never
needs to reconcile a length delta.

The other hard full-redraw triggers (first render, width change, height
change, clear-on-shrink, pure-deletion-above-viewport, forced full
clear) take precedence over the skip and the clamped repaint.

## The honest-buffer invariant

`previous_lines` must always equal what is physically on the terminal,
including the rows frozen in scrollback. This is what makes "resume when
scrolled back into view" work without any extra machinery:

- On a **skip**, leave `previous_lines` (and the viewport tracker,
  hardware cursor, and Kitty image registry) completely untouched. The
  engine keeps believing the terminal shows the old, scrolled-off
  content, which is true.
- On a **clamped straddle repaint**, advance only the rows that were
  actually painted. Keep the off-screen prefix `[0, vt)` as its
  last-painted content and adopt the repainted visible suffix `[vt, ..)`.

When those rows later re-enter the viewport (the content below shrinks,
or the terminal grows), the next diff compares the current frame against
the stale `previous_lines`, sees the difference at the now-visible rows,
and repaints them. If `previous_lines` had instead been advanced while
the rows were off-screen, the engine would believe they were already up
to date and would leave stale content on screen at re-entry. That is the
trap the invariant avoids.

A skipped, still-dirty off-screen row keeps the computed diff range
starting above the viewport on subsequent frames. As long as only
off-screen rows change, each frame is another skip. If an in-viewport
row also changes in the same frame, the range straddles the viewport top
and takes the clamped repaint, which is still strictly cheaper than the
old full clear-and-repaint and produces no flicker (no `\x1b[2J`).

## Tradeoffs

- **Stale scrollback.** A sub-agent that runs and finishes entirely
  off-screen leaves its last on-screen state frozen in the terminal's
  native scrollback. Scrolling the terminal up shows that frozen state,
  not the latest. This is inherent to not touching off-screen rows, and
  it matches how the engine already treats scrollback. The live box is a
  progress affordance. The final result still reaches the main thread as
  a normal message or tool result.
- **Transient straddle redraws.** A box growing toward its compact cap,
  or the brief moment a box scrolls across the top edge, changes the
  line count above the fold and takes a full redraw. Short-lived and
  acceptable.
- **Residual scheduling cost.** The event pump still requests a render
  per sub-agent event, so the engine still walks the component tree
  (with leaf caches) and then skips. That removes the flicker and the
  expensive full-redraw writes, which is the visible bug. Gating the
  render request itself for events that only mutate a non-visible
  component is a separate, optional scheduling-layer change.

## Testing

End-to-end tests against the `VirtualTerminal` sink, asserting both the
viewport the user sees and the engine's strategy counters
(`full_redraws()`, the new `skipped_renders()`):

- An off-screen, equal-length change takes the skip path: no full
  redraw, no content bytes written, viewport unchanged.
- A skipped off-screen change becomes visible again (content below
  shrinks) and the current content appears: resume works.
- A straddling, equal-length change takes the clamped repaint: no full
  redraw, the visible portion updates, off-screen rows are left
  untouched.
- A straddling change that alters the line count above the fold still
  takes a full redraw.
