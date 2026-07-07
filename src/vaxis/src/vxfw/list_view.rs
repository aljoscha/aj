//! [`ListView`]: a vertically scrolling list of widgets with a movable cursor.
//!
//! # The scroll state machine
//!
//! Scrolling is applied at draw time, not when the event arrives. An event
//! only mutates the [`Scroll`] accumulators (`offset`, `pending_lines`) or the
//! `cursor`. The next [`draw`](Widget::draw) reconciles those into a concrete
//! child layout and recomputes `top`, `offset`, and `has_more`. This split is
//! load-bearing: it lets several wheel events between frames accumulate, and it
//! lets cursor moves defer the "bring the cursor into view" work to the draw,
//! where the children are actually measured.
//!
//! NOTE(D8): [`ListView`] and [`ScrollView`](crate::vxfw::ScrollView) share
//! roughly 70% of this logic but with deliberate differences. They are kept
//! separate rather than unified behind a shared engine. The list-view side:
//! `draw_cursor` defaults to true, the cursor indicator is a const (not a
//! field), there is no horizontal axis, children are bounded to the available
//! width, and only the visible `[start..end]` window is returned as children.

use std::rc::Rc;

use crate::cell::{Cell, Character};
use crate::key::{Key, Modifiers};
use crate::mouse;
use crate::vxfw::{
    DrawContext, Event, EventContext, MaxSize, RelativePoint, ScrollableView, Size, SubSurface,
    Surface, Widget, WidgetRef, draw_widget,
};

/// Lazily builds the widget for an item index.
///
/// `idx` is the item index and `cursor` is the list's current cursor index, so
/// a builder can render the cursored item differently. Returning `None` marks
/// the end of the list. The first index that yields `None` bounds the list.
pub trait Builder {
    /// Returns the widget at `idx`, or `None` if `idx` is past the end.
    fn item_at_idx(&self, idx: usize, cursor: usize) -> Option<WidgetRef>;
}

/// Where a scrolling widget gets its children.
///
/// A `Slice` knows its length up front. A `Builder` lazily builds child widgets
/// by item index, which is useful for large lists or items whose rendering
/// depends on the cursor.
pub enum Source {
    Slice(Vec<WidgetRef>),
    Builder(Box<dyn Builder>),
}

impl Default for Source {
    fn default() -> Source {
        Source::Slice(Vec::new())
    }
}

/// Adapts a borrowed slice of widgets to the [`Builder`] interface.
///
/// `draw` resolves a `Source::Slice` into one of these so the slice and builder
/// paths share a single `draw_builder`.
struct SliceBuilder<'a> {
    slice: &'a [WidgetRef],
}

impl Builder for SliceBuilder<'_> {
    fn item_at_idx(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
        self.slice.get(idx).map(Rc::clone)
    }
}

/// The list-view scroll position.
///
/// Events mutate this; [`draw`](Widget::draw) reconciles it. See the module
/// docs for the draw-time contract.
struct Scroll {
    /// Index of the first fully-in-view widget.
    top: u32,
    /// Line offset within the top widget.
    offset: i32,
    /// Pending scroll amount, applied and cleared on the next draw.
    pending_lines: i32,
    /// Whether there is more room to scroll down.
    has_more: bool,
    /// The cursor must be brought into the viewport on the next draw.
    wants_cursor: bool,
}

impl Default for Scroll {
    fn default() -> Scroll {
        Scroll {
            top: 0,
            offset: 0,
            pending_lines: 0,
            has_more: true,
            wants_cursor: false,
        }
    }
}

impl Scroll {
    fn lines_down(&mut self, n: u8) -> bool {
        if !self.has_more {
            return false;
        }
        self.pending_lines += i32::from(n);
        true
    }

    fn lines_up(&mut self, n: u8) -> bool {
        if self.top == 0 && self.offset == 0 {
            return false;
        }
        self.pending_lines -= i32::from(n);
        true
    }
}

/// The indicator drawn in the cursor gutter next to the cursored item.
///
/// NOTE: This is a const in `ListView` but a field in `ScrollView`, matching
/// the upstream asymmetry.
fn cursor_indicator() -> Cell {
    Cell {
        char: Character::new("▐", 1),
        ..Cell::default()
    }
}

/// A binary-indexed (Fenwick) tree over `i64` values with O(log n) prefix
/// sums, point updates, and append.
///
/// Internally 1-indexed with a dummy slot at index 0, so `tree.len() - 1` is
/// the element count. Values are `i64` so a point update can subtract (a
/// re-measured item whose height shrank), even though the sums we take are
/// non-negative in practice.
struct Fenwick {
    tree: Vec<i64>,
}

/// Lowest set bit of `i`, the span a Fenwick node at index `i` covers.
fn lowbit(i: usize) -> usize {
    i & i.wrapping_neg()
}

impl Fenwick {
    fn new() -> Fenwick {
        Fenwick { tree: vec![0] }
    }

    fn len(&self) -> usize {
        self.tree.len() - 1
    }

    /// Appends one slot initialized to 0.
    ///
    /// The appended node covers a range that may include existing non-zero
    /// elements, so we seed it with the sum of the child nodes below it. That
    /// keeps prefix queries correct without re-touching the whole tree.
    fn push(&mut self) {
        let n = self.tree.len();
        self.tree.push(0);
        let lb = lowbit(n);
        let mut i = 1;
        while i < lb {
            self.tree[n] += self.tree[n - i];
            i <<= 1;
        }
    }

    /// Adds `delta` to the 0-based element `i`.
    fn add(&mut self, i: usize, delta: i64) {
        let n = self.len();
        let mut pos = i + 1;
        while pos <= n {
            self.tree[pos] += delta;
            pos += lowbit(pos);
        }
    }

    /// Sum of the 0-based elements `[0, i)`, so `prefix(0)` is 0 and
    /// `prefix(len)` is the whole sum. `i` is clamped to `len`.
    fn prefix(&self, i: usize) -> i64 {
        let mut pos = i.min(self.len());
        let mut sum = 0;
        while pos > 0 {
            sum += self.tree[pos];
            pos -= lowbit(pos);
        }
        sum
    }

    /// Shrinks to `len` elements, rebuilding from the retained values.
    ///
    /// Retained values are recovered from prefix diffs before the shrink, so
    /// the caller need not hand them in. A no-op when `len >= self.len()`.
    fn truncate(&mut self, len: usize) {
        if len >= self.len() {
            return;
        }
        let values: Vec<i64> = (0..len)
            .map(|i| self.prefix(i + 1) - self.prefix(i))
            .collect();
        let mut rebuilt = Fenwick::new();
        for &v in &values {
            rebuilt.push();
            let last = rebuilt.len() - 1;
            rebuilt.add(last, v);
        }
        *self = rebuilt;
    }
}

/// Per-item extent model that drives the scrollbar thumb. The scroll core is
/// index-anchored, so this never affects what is drawn, only the thumb.
///
/// An item counts as an estimated extent until it is laid out, then its
/// measured height replaces the estimate. The estimate is the running mean of
/// the measured heights, so the total sharpens as items scroll into view. Two
/// Fenwick trees give O(log n) prefix queries: one over measured heights (0 for
/// unmeasured), one over the measured flag (0/1), so `offset_for_index` can add
/// the exact measured heights below `i` plus the estimated heights of the
/// unmeasured items below `i`.
struct ListGeometry {
    heights: Vec<Option<u32>>,
    height_prefix: Fenwick,
    measured_prefix: Fenwick,
    measured_sum: u64,
    measured_count: usize,
}

impl ListGeometry {
    fn new() -> ListGeometry {
        ListGeometry {
            heights: Vec::new(),
            height_prefix: Fenwick::new(),
            measured_prefix: Fenwick::new(),
            measured_sum: 0,
            measured_count: 0,
        }
    }

    fn len(&self) -> usize {
        self.heights.len()
    }

    /// Resizes to `n` items, preserving existing measurements.
    ///
    /// Growing appends unmeasured slots, which is the common append case for a
    /// transcript, so we must never wholesale-clear here or every appended
    /// entry would wipe the measurements. Shrinking drops the tail and rebuilds
    /// the running counters and Fenwicks from the retained heights.
    fn set_len(&mut self, n: usize) {
        let len = self.len();
        if n > len {
            for _ in len..n {
                self.heights.push(None);
                self.height_prefix.push();
                self.measured_prefix.push();
            }
        } else if n < len {
            self.heights.truncate(n);
            self.measured_sum = 0;
            self.measured_count = 0;
            for h in &self.heights {
                if let Some(height) = h {
                    self.measured_sum += u64::from(*height);
                    self.measured_count += 1;
                }
            }
            self.height_prefix.truncate(n);
            self.measured_prefix.truncate(n);
        }
    }

    /// Records item `i`'s measured `height`, replacing any prior measurement.
    ///
    /// A no-op when `i` is out of range. Re-measuring an already-measured index
    /// (its height changed after a re-layout) adjusts the running sum by the
    /// height delta without double-counting it.
    fn set_measured(&mut self, i: usize, height: u32) {
        if i >= self.len() {
            return;
        }
        let old = self.heights[i];
        let delta = i64::from(height) - old.map_or(0, i64::from);
        self.heights[i] = Some(height);
        self.height_prefix.add(i, delta);
        match old {
            Some(prev) => {
                self.measured_sum = self.measured_sum - u64::from(prev) + u64::from(height);
            }
            None => {
                self.measured_prefix.add(i, 1);
                self.measured_count += 1;
                self.measured_sum += u64::from(height);
            }
        }
    }

    /// Estimated height of an unmeasured item: the mean of the measured
    /// heights, at least 1. Falls back to 1 before anything is measured.
    fn estimate(&self) -> u32 {
        if self.measured_count == 0 {
            return 1;
        }
        let count = u64::try_from(self.measured_count).expect("count fits u64");
        let mean = (self.measured_sum / count).max(1);
        u32::try_from(mean).unwrap_or(u32::MAX)
    }

    /// Line offset of the top of item `i`: the exact measured heights below `i`
    /// plus the estimate for each unmeasured item below `i`. `i` is clamped to
    /// `len`.
    fn offset_for_index(&self, i: usize) -> u64 {
        let i = i.min(self.len());
        let measured_height =
            u64::try_from(self.height_prefix.prefix(i)).expect("prefix sum is non-negative");
        let measured_below =
            usize::try_from(self.measured_prefix.prefix(i)).expect("count is non-negative");
        let unmeasured = u64::try_from(i - measured_below).expect("index fits u64");
        measured_height + unmeasured * u64::from(self.estimate())
    }

    /// Total content extent in lines.
    fn total(&self) -> u64 {
        self.offset_for_index(self.len())
    }

    /// Item index whose top is the largest offset `<= line`, clamped to a valid
    /// item index. `offset_for_index(0)` is 0, so a candidate always exists.
    fn item_at_line(&self, line: u64) -> usize {
        let n = self.len();
        if n == 0 {
            return 0;
        }
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo + 1) / 2;
            if self.offset_for_index(mid) <= line {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo.min(n - 1)
    }

    fn reset(&mut self) {
        self.heights.clear();
        self.height_prefix = Fenwick::new();
        self.measured_prefix = Fenwick::new();
        self.measured_sum = 0;
        self.measured_count = 0;
    }
}

/// A vertically scrolling list with a movable cursor.
///
/// Construct with [`ListView::new`] and tweak the public fields. The widget is
/// stateful and interactive: it overrides [`wants_events`](Widget::wants_events)
/// and mutates its scroll position during draw.
pub struct ListView {
    pub children: Source,
    pub cursor: u32,
    /// When true, a cursor indicator is drawn next to the cursored widget.
    pub draw_cursor: bool,
    /// Lines to scroll per mouse-wheel tick.
    pub wheel_scroll: u8,
    /// Set when the exact item count is known, which lets cursor moves and
    /// jumps avoid walking the builder.
    pub item_count: Option<u32>,
    scroll: Scroll,
    /// Viewport height (rows) the last [`draw`](Widget::draw) laid out
    /// against. `None` before the first draw. Page-scroll callers scale this
    /// to scroll by whole viewports (see [`viewport_height`](Self::viewport_height)).
    last_viewport_height: Option<u16>,
    /// Read-model of per-item extents, updated during draw and used only to
    /// size and place the scrollbar thumb. Layered on top of the index-anchored
    /// scroll core, so it never affects what is drawn.
    geometry: ListGeometry,
}

impl ListView {
    /// A list view over `children` with `draw_cursor` on and a wheel step of 3.
    pub fn new(children: Source) -> ListView {
        ListView {
            children,
            cursor: 0,
            draw_cursor: true,
            wheel_scroll: 3,
            item_count: None,
            scroll: Scroll::default(),
            last_viewport_height: None,
            geometry: ListGeometry::new(),
        }
    }

    /// Moves the cursor to the next item, bringing it into view.
    pub fn next_item(&mut self, ctx: &mut EventContext) {
        if let Some(count) = self.item_count {
            // NOTE: saturating here, where ScrollView uses a plain `count - 1`.
            if self.cursor >= count.saturating_sub(1) {
                return ctx.consume_event();
            }
            self.cursor += 1;
        } else {
            match &self.children {
                Source::Slice(slice) => {
                    let len = u32::try_from(slice.len()).expect("item count fits u32");
                    self.item_count = Some(len);
                    if self.cursor == len - 1 {
                        return ctx.consume_event();
                    }
                    self.cursor += 1;
                }
                Source::Builder(builder) => {
                    let prev = self.cursor;
                    self.cursor += 1;
                    // Walk back until we land on an item that exists, finding the
                    // last item when we stepped past the end.
                    while builder
                        .item_at_idx(
                            usize::try_from(self.cursor).expect("cursor fits usize"),
                            usize::try_from(self.cursor).expect("cursor fits usize"),
                        )
                        .is_none()
                    {
                        self.cursor = self.cursor.saturating_sub(1);
                    }
                    if self.cursor == prev {
                        return ctx.consume_event();
                    }
                }
            }
        }
        self.ensure_scroll();
        ctx.consume_and_redraw();
    }

    /// Moves the cursor to the previous item, bringing it into view.
    pub fn prev_item(&mut self, ctx: &mut EventContext) {
        if self.cursor == 0 {
            return ctx.consume_event();
        }
        if let Some(count) = self.item_count {
            self.cursor = (self.cursor - 1).min(count - 1);
        } else {
            match &self.children {
                Source::Slice(slice) => {
                    let len = u32::try_from(slice.len()).expect("item count fits u32");
                    self.item_count = Some(len);
                    self.cursor = (self.cursor - 1).min(len - 1);
                }
                Source::Builder(builder) => {
                    let prev = self.cursor;
                    self.cursor -= 1;
                    while builder
                        .item_at_idx(
                            usize::try_from(self.cursor).expect("cursor fits usize"),
                            usize::try_from(self.cursor).expect("cursor fits usize"),
                        )
                        .is_none()
                    {
                        self.cursor = self.cursor.saturating_sub(1);
                    }
                    if self.cursor == prev {
                        return ctx.consume_event();
                    }
                }
            }
        }
        self.ensure_scroll();
        ctx.consume_and_redraw();
    }

    /// Anchors the viewport so the cursored item is visible on the next draw.
    ///
    /// Call only after the cursor moved or to force the cursor into view. If the
    /// cursor is at or above the top we snap the top to it; otherwise we defer
    /// to the draw via `wants_cursor`.
    pub fn ensure_scroll(&mut self) {
        if self.cursor <= self.scroll.top {
            self.scroll.top = self.cursor;
            self.scroll.offset = 0;
        } else {
            self.scroll.wants_cursor = true;
        }
    }

    /// Returns the item count, caching a slice's length. Builder-backed lists
    /// without an explicit `item_count` return `None`.
    fn known_item_count(&mut self) -> Option<u32> {
        if let Some(count) = self.item_count {
            return Some(count);
        }
        match &self.children {
            Source::Slice(slice) => {
                let count = u32::try_from(slice.len()).expect("item count fits u32");
                self.item_count = Some(count);
                Some(count)
            }
            Source::Builder(_) => None,
        }
    }

    /// Moves the cursor to `idx` and starts drawing from it.
    ///
    /// Useful for large jumps: starting the draw at the cursor avoids building
    /// every child between the old and new positions. When the item count is
    /// known, `idx` is clamped to the last item.
    pub fn jump_to_item(&mut self, idx: u32) {
        let cursor = match self.known_item_count() {
            Some(0) => 0,
            Some(count) => idx.min(count - 1),
            None => idx,
        };
        self.cursor = cursor;
        self.scroll = Scroll {
            top: cursor,
            ..Scroll::default()
        };
    }

    /// Scrolls to the bottom when the item count is known, preserving the
    /// cursor. A builder-backed list without `item_count` has no known bottom,
    /// so this does nothing.
    pub fn scroll_to_bottom(&mut self) {
        let Some(count) = self.known_item_count() else {
            return;
        };
        // Position the window one past the last item. The next draw's downward
        // fill then runs out immediately and the back-fill lays out a full
        // viewport ending at the last item's bottom edge. Anchoring at
        // `count - 1` instead would pin the viewport to the top of the last
        // item, hiding its tail whenever it is taller than the viewport.
        self.scroll = Scroll {
            top: count,
            ..Scroll::default()
        };
    }

    /// Whether the last draw reached the end of the list, i.e. the
    /// final item's bottom edge landed inside the viewport and the
    /// list is scrolled to the bottom.
    ///
    /// Reflects the most recent [`draw`](Widget::draw): scroll events
    /// only accumulate pending state, so query this after a draw, not
    /// after an event.
    pub fn is_at_bottom(&self) -> bool {
        !self.scroll.has_more
    }

    /// Scroll the viewport by `delta` lines on the next draw: a negative
    /// `delta` moves toward the top, a positive one toward the bottom.
    ///
    /// The amount feeds the same `pending_lines` accumulator the mouse wheel
    /// uses, so several scrolls between frames accumulate and the next
    /// [`draw`](Widget::draw) reconciles them into a concrete layout. The
    /// draw clamps at both ends, so an oversized `delta` (a page taller than
    /// the content remaining in that direction) simply lands on the end
    /// rather than overscrolling. Whether a downward scroll reached the end
    /// shows up in [`is_at_bottom`](Self::is_at_bottom) after that draw.
    pub fn scroll_lines(&mut self, delta: i32) {
        self.scroll.pending_lines += delta;
    }

    /// The viewport height (rows) the last [`draw`](Widget::draw) laid out
    /// against, or `None` before the first draw.
    ///
    /// A page-scroll caller scales this to scroll by whole viewports.
    pub fn viewport_height(&self) -> Option<u16> {
        self.last_viewport_height
    }

    /// Item index of the first in-view widget as of the last completed draw.
    ///
    /// Together with [`scroll_offset`](Self::scroll_offset) this pins the
    /// viewport top: the top item begins `scroll_offset` lines above the
    /// viewport edge, so the absolute line shown at the top is that item's
    /// start line plus the offset. Reflects the last draw's reconciliation,
    /// not any scroll input queued since. Mirrors the `ScrollableView`
    /// accessor of the same name, exposed inherently so callers holding a
    /// concrete `ListView` need not import the trait.
    pub fn scroll_top(&self) -> u32 {
        self.scroll.top
    }

    /// Lines of the top item scrolled above the viewport edge as of the last
    /// completed draw.
    ///
    /// Always `>= 0`: the draw reconciles the offset to the count of the top
    /// item's rows sitting above the viewport top. See
    /// [`scroll_top`](Self::scroll_top) for the pairing.
    pub fn scroll_offset(&self) -> i32 {
        self.scroll.offset
    }

    /// Clears the scrollbar-thumb geometry so the next draw rebuilds it.
    ///
    /// For a wholesale content swap where an index no longer maps to the same
    /// item (a session switch). The append/truncate logic in
    /// [`set_len`](ListGeometry::set_len) cannot detect that from length alone,
    /// so it would carry the previous content's measurements into the new one.
    pub fn reset_geometry(&mut self) {
        self.geometry.reset();
    }

    /// Inserts children at the front of `child_list` until `add_height` lines
    /// are filled above the current top, walking upward from `top - 1`.
    fn insert_children(
        &mut self,
        ctx: &DrawContext,
        builder: &dyn Builder,
        child_list: &mut Vec<SubSurface>,
        add_height: i32,
    ) {
        debug_assert!(self.scroll.top > 0);
        self.scroll.top -= 1;
        let cursor = usize::try_from(self.cursor).expect("cursor fits usize");
        let max_size = ctx.max.size();
        let child_offset: u16 = if self.draw_cursor { 2 } else { 0 };
        let mut upheight = add_height;
        loop {
            let top = usize::try_from(self.scroll.top).expect("top fits usize");
            let Some(child) = builder.item_at_idx(top, cursor) else {
                break;
            };
            // NOTE: plain subtraction for the width here, where the down-loop
            // saturates. Reproduced from the upstream asymmetry.
            let child_ctx = ctx.with_constraints(
                Size {
                    width: max_size.width - child_offset,
                    height: 0,
                },
                MaxSize {
                    width: Some(max_size.width - child_offset),
                    height: None,
                },
            );
            let surf = draw_widget(&child, &child_ctx);
            // Record the measured height for the thumb geometry. See the
            // matching site in draw_builder for why stale off-screen heights
            // are acceptable after a resize.
            if top < self.geometry.len() {
                self.geometry.set_measured(top, u32::from(surf.size.height));
            }
            // Traversing backward, so accumulate before setting the origin.
            upheight -= i32::from(surf.size.height);
            child_list.insert(
                0,
                SubSurface {
                    origin: RelativePoint {
                        col: i32::from(child_offset),
                        row: upheight,
                    },
                    surface: surf,
                    z_index: 0,
                },
            );
            // Stop once we passed the top edge or reached the first item.
            if upheight <= 0 || self.scroll.top == 0 {
                break;
            }
            self.scroll.top -= 1;
        }
        // NOTE: upstream wraps this re-layout in a pair of `offset = upheight`
        // assignments with an interior `offset = 0` that the second assignment
        // immediately overrides. The only observable effect is that origins are
        // re-laid from row 0 when we overshot the top, and `offset` ends up as
        // `upheight`. We keep that effect.
        if self.scroll.top == 0 && upheight > 0 {
            let mut row: i32 = 0;
            for child in child_list.iter_mut() {
                child.origin.row = row;
                row += i32::from(child.surface.size.height);
            }
        }
        self.scroll.offset = upheight;
    }

    /// Reconciles the pending scroll into a concrete child layout.
    ///
    /// `builder` is the resolved source (a slice adapter or the user's builder).
    /// This is the heart of the state machine described in the module docs.
    fn draw_builder(&mut self, ctx: &DrawContext, builder: &dyn Builder) -> Surface {
        let max_size = ctx.max.size();
        // Record the viewport height so viewport-relative scrolls (page
        // up/down) can read it back after this draw.
        self.last_viewport_height = Some(max_size.height);

        // Size the thumb geometry to the known item count. We read
        // `self.item_count` directly rather than `known_item_count()` because
        // `draw` swaps `children` out for the default empty slice while we run,
        // so `known_item_count()` would walk that empty slice and report 0 for
        // a builder-backed list. `draw` has already resolved a slice's count
        // into `item_count`, so this covers slices and builders that know their
        // count, and leaves an unbounded builder's geometry empty (thumb falls
        // back to item-count sizing).
        if let Some(n) = self.item_count {
            self.geometry
                .set_len(usize::try_from(n).expect("item count fits usize"));
        }
        let cursor = usize::try_from(self.cursor).expect("cursor fits usize");

        // Assume there is more below; we only learn otherwise by running out of
        // items while drawing.
        self.scroll.has_more = true;

        let mut child_list: Vec<SubSurface> = Vec::new();

        // The accumulated height starts (offset + pending_lines) lines above the
        // top edge, so a pending downward scroll begins at a negative row and a
        // pending upward scroll begins below row 0 (to be back-filled).
        let mut accumulated_height: i32 = -(self.scroll.offset + self.scroll.pending_lines);
        self.scroll.pending_lines = 0;

        // Capture the starting index before insert_children mutates `top`.
        let mut i = usize::try_from(self.scroll.top).expect("top fits usize");

        // At the very top an upward scroll cannot consume anything, so clamp.
        if accumulated_height > 0 && self.scroll.top == 0 {
            self.scroll.offset = 0;
            accumulated_height = 0;
        }

        // Offset downward: back-fill children above the top before going down.
        if accumulated_height > 0 {
            self.insert_children(ctx, builder, &mut child_list, accumulated_height);
            let last = child_list.last().expect("insert_children added a child");
            accumulated_height = last.origin.row + i32::from(last.surface.size.height);
        }

        let child_offset: u16 = if self.draw_cursor { 2 } else { 0 };

        // The downward fill. Zig's `while (...) |x| {...} else {...}` runs the
        // `else` when the loop exhausts the optional without breaking; Rust has
        // no such construct, so we break out of a `loop` and set `has_more` on
        // the run-out path directly.
        loop {
            let Some(child) = builder.item_at_idx(i, cursor) else {
                // Ran out of items: nothing more below.
                self.scroll.has_more = false;
                break;
            };
            // NOTE: saturating width here, where insert_children uses plain
            // subtraction. Reproduced from the upstream asymmetry.
            let child_ctx = ctx.with_constraints(
                Size {
                    width: max_size.width.saturating_sub(child_offset),
                    height: 0,
                },
                MaxSize {
                    width: Some(max_size.width.saturating_sub(child_offset)),
                    height: None,
                },
            );
            let surf = draw_widget(&child, &child_ctx);
            let height = i32::from(surf.size.height);
            // Record the measured height for the thumb geometry, keyed by the
            // item index `i`. Heights are measured at the current draw width and
            // the geometry is not keyed by width, so after a resize off-screen
            // entries carry stale heights until re-measured on scroll. That only
            // sizes the thumb (cosmetic) and the visible window is re-measured
            // every draw, so it is fine.
            if i < self.geometry.len() {
                self.geometry.set_measured(i, u32::from(surf.size.height));
            }
            child_list.push(SubSurface {
                origin: RelativePoint {
                    col: i32::from(child_offset),
                    row: accumulated_height,
                },
                surface: surf,
                z_index: 0,
            });
            accumulated_height += height;

            // `i < cursor` uses the pre-increment index, matching the deferred
            // increment in upstream.
            let want_more_for_cursor = self.scroll.wants_cursor && i < cursor;
            i += 1;
            if want_more_for_cursor {
                continue;
            }
            if accumulated_height >= i32::from(max_size.height) {
                break;
            }
        }

        let mut total = total_height(&child_list);

        // On a resize we may have reached the bottom without filling the screen;
        // back-fill from above to use the empty space.
        if !self.scroll.has_more && total < usize::from(max_size.height) && self.scroll.top > 0 {
            let add =
                i32::try_from(usize::from(max_size.height) - total).expect("fill height fits i32");
            self.insert_children(ctx, builder, &mut child_list, add);
            total = total_height(&child_list);
        }

        // Wrap the cursored child with the indicator gutter. The wrapper is as
        // wide as the gutter plus the child (the ScrollView variant differs).
        if self.draw_cursor && self.cursor >= self.scroll.top {
            let cursored_idx = usize::try_from(self.cursor - self.scroll.top).expect("idx fits");
            if cursored_idx < child_list.len() {
                let child = child_list[cursored_idx].clone();
                let size = child.surface.size;
                let inner = SubSurface {
                    origin: RelativePoint {
                        col: i32::from(child_offset),
                        row: 0,
                    },
                    surface: child.surface,
                    z_index: 0,
                };
                let mut cursor_surf = Surface::with_children(
                    Size {
                        width: child_offset + size.width,
                        height: size.height,
                    },
                    vec![inner],
                );
                for row in 0..cursor_surf.size.height {
                    cursor_surf.write_cell(0, row, cursor_indicator());
                }
                child_list[cursored_idx] = SubSurface {
                    origin: RelativePoint {
                        col: 0,
                        row: child.origin.row,
                    },
                    surface: cursor_surf,
                    z_index: 0,
                };
            }
        }

        // If the cursor must be in view, ensure the cursored child is fully
        // visible: anchor it to the bottom, or make it the sole top item when it
        // is taller than the viewport.
        if self.scroll.wants_cursor {
            let cursored_idx = usize::try_from(self.cursor - self.scroll.top).expect("idx fits");
            let sub_origin_row = child_list[cursored_idx].origin.row;
            let sub_height = child_list[cursored_idx].surface.size.height;
            let bottom = sub_origin_row + i32::from(sub_height);
            if bottom > i32::from(max_size.height) {
                // Anchor the cursored child (and those above it) to the bottom.
                let mut origin = i32::from(max_size.height);
                let mut idx = cursored_idx + 1;
                while idx > 0 {
                    origin -= i32::from(child_list[idx - 1].surface.size.height);
                    child_list[idx - 1].origin.row = origin;
                    idx -= 1;
                }
            } else if sub_height >= max_size.height {
                // The cursored child fills the viewport: make it the only item.
                self.scroll.top = self.cursor;
                self.scroll.offset = 0;
                let surface = child_list[cursored_idx].surface.clone();
                let h = usize::from(surface.size.height);
                child_list.clear();
                child_list.push(SubSurface {
                    origin: RelativePoint { col: 0, row: 0 },
                    surface,
                    z_index: 0,
                });
                total = h;
            }
        }

        // Reaching the bottom re-anchors the children: from the top when they do
        // not fill the screen, from the bottom otherwise.
        if !self.scroll.has_more && total < usize::from(max_size.height) {
            debug_assert!(self.scroll.top == 0);
            self.scroll.offset = 0;
            let mut origin: i32 = 0;
            for child in child_list.iter_mut() {
                child.origin.row = origin;
                origin += i32::from(child.surface.size.height);
            }
        } else if !self.scroll.has_more {
            let mut origin = i32::from(max_size.height);
            let mut idx = child_list.len();
            while idx > 0 {
                origin -= i32::from(child_list[idx - 1].surface.size.height);
                child_list[idx - 1].origin.row = origin;
                idx -= 1;
            }
        }

        // Find the visible window and recompute top/offset from the laid-out
        // origins.
        let mut start: usize = 0;
        let mut end: usize = child_list.len();
        for (idx, child) in child_list.iter().enumerate() {
            if child.origin.row <= 0 && child.origin.row + i32::from(child.surface.size.height) > 0
            {
                start = idx;
                self.scroll.offset = -child.origin.row;
                self.scroll.top += u32::try_from(idx).expect("index fits u32");
            }
            if child.origin.row > i32::from(max_size.height) {
                end = idx;
                break;
            }
        }

        // Reset the deferred cursor request now that the draw consumed it.
        self.scroll.wants_cursor = false;

        // When drawing the cursor we allocate a buffer so the list obscures any
        // content underneath it.
        let mut surface = if self.draw_cursor {
            Surface::with_size(max_size)
        } else {
            Surface {
                size: max_size,
                widget: None,
                cursor: None,
                buffer: Vec::new(),
                children: Vec::new(),
            }
        };
        // Only the visible window is returned (ScrollView returns all children).
        surface.children = child_list[start..end].to_vec();
        surface
    }
}

/// Sum of the children's heights.
fn total_height(list: &[SubSurface]) -> usize {
    list.iter()
        .map(|child| usize::from(child.surface.size.height))
        .sum()
}

impl ScrollableView for ListView {
    fn total_item_count(&self) -> usize {
        if let Some(c) = self.item_count {
            return usize::try_from(c).expect("item count fits usize");
        }
        match &self.children {
            Source::Slice(slice) => slice.len(),
            Source::Builder(builder) => {
                let cursor = usize::try_from(self.cursor).expect("cursor fits usize");
                let mut counter = 0;
                while builder.item_at_idx(counter, cursor).is_some() {
                    counter += 1;
                }
                counter
            }
        }
    }

    fn scroll_top(&self) -> u32 {
        self.scroll.top
    }

    fn has_more_below(&self) -> bool {
        self.scroll.has_more
    }

    fn set_scroll_top(&mut self, top: u32) {
        // A fresh anchor at `top`: the old line offset and any pending wheel
        // amount belong to the previous position and would misplace the jump.
        self.scroll = Scroll {
            top,
            ..Scroll::default()
        };
    }

    // The list has no horizontal axis.

    fn scroll_left(&self) -> u32 {
        0
    }

    fn has_more_right(&self) -> bool {
        false
    }

    fn set_scroll_left(&mut self, _left: u32) {}

    // Geometry-backed thumb sizing. Empty geometry (an unbounded builder with
    // no item_count) reports `None` so the bars fall back to item-count sizing.

    fn content_extent(&self) -> Option<u32> {
        if self.geometry.len() == 0 {
            None
        } else {
            Some(u32::try_from(self.geometry.total()).unwrap_or(u32::MAX))
        }
    }

    fn viewport_top_line(&self) -> Option<u32> {
        if self.geometry.len() == 0 {
            return None;
        }
        let top = usize::try_from(self.scroll.top).expect("top fits usize");
        // `offset` is reconciled to `>= 0` after a draw. Clamp defensively.
        let offset = u64::try_from(self.scroll.offset.max(0)).expect("offset fits u64");
        let line = self.geometry.offset_for_index(top) + offset;
        Some(u32::try_from(line).unwrap_or(u32::MAX))
    }

    fn item_at_line(&self, line: u32) -> Option<u32> {
        if self.geometry.len() == 0 {
            None
        } else {
            let idx = self.geometry.item_at_line(u64::from(line));
            Some(u32::try_from(idx).unwrap_or(u32::MAX))
        }
    }
}

impl Widget for ListView {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // NOTE: take the children out so the SliceBuilder can borrow them while
        // `draw_builder` borrows `&mut self`. We restore them before returning,
        // so this is a borrow-checker dance, not a state change.
        let children = std::mem::take(&mut self.children);
        let surface = match &children {
            Source::Slice(slice) => {
                self.item_count = Some(u32::try_from(slice.len()).expect("item count fits u32"));
                let builder = SliceBuilder { slice };
                self.draw_builder(ctx, &builder)
            }
            Source::Builder(builder) => self.draw_builder(ctx, builder.as_ref()),
        };
        self.children = children;
        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::Mouse(m) => {
                if m.button == mouse::Button::WheelUp && self.scroll.lines_up(self.wheel_scroll) {
                    ctx.consume_and_redraw();
                }
                if m.button == mouse::Button::WheelDown && self.scroll.lines_down(self.wheel_scroll)
                {
                    ctx.consume_and_redraw();
                }
            }
            Event::KeyPress(key) => {
                if key.matches(u32::from('j'), Modifiers::empty())
                    || key.matches(u32::from('n'), Modifiers::CTRL)
                    || key.matches(Key::DOWN, Modifiers::empty())
                {
                    self.next_item(ctx);
                    return;
                }
                if key.matches(u32::from('k'), Modifiers::empty())
                    || key.matches(u32::from('p'), Modifiers::CTRL)
                    || key.matches(Key::UP, Modifiers::empty())
                {
                    self.prev_item(ctx);
                    return;
                }
                if key.matches(Key::ESCAPE, Modifiers::empty()) {
                    self.ensure_scroll();
                    ctx.consume_and_redraw();
                }
            }
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell as StdCell;
    use std::cell::RefCell;

    use super::*;
    use crate::gwidth;
    use crate::vxfw::Text;

    fn draw_ctx(width: u16, height: u16) -> DrawContext {
        DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(width),
                height: Some(height),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        }
    }

    fn text(s: &str) -> WidgetRef {
        Rc::new(RefCell::new(Text::new(s)))
    }

    #[test]
    fn list_view() {
        let mut list_view = ListView::new(Source::Slice(vec![
            text("abc\n  def\n  ghi"),
            text("def"),
            text("ghi"),
            text("jkl\n mno"),
        ]));
        list_view.wheel_scroll = 1;

        let ctx = draw_ctx(16, 4);

        let mut surface = list_view.draw(&ctx);
        // ListView expands to max height and width.
        assert_eq!(surface.size.height, 4);
        assert_eq!(surface.size.width, 16);
        // Only visible children appear as surfaces.
        assert_eq!(surface.children.len(), 2);

        let mut event_ctx = EventContext::new();
        let wheel_up = Event::Mouse(mouse_event(mouse::Button::WheelUp));
        let wheel_down = Event::Mouse(mouse_event(mouse::Button::WheelDown));

        list_view.handle_event(&mut event_ctx, &wheel_up);
        // Wheel up does not adjust the scroll at the top.
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 0);

        list_view.handle_event(&mut event_ctx, &wheel_down);
        surface = list_view.draw(&ctx);
        // Down one line, top widget unchanged, one more widget in view.
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 1);
        assert_eq!(surface.children.len(), 3);

        list_view.handle_event(&mut event_ctx, &wheel_down);
        list_view.handle_event(&mut event_ctx, &wheel_down);
        surface = list_view.draw(&ctx);
        // Down two more lines scrolls the top widget out of view.
        assert_eq!(list_view.scroll.top, 1);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 3);

        list_view.handle_event(&mut event_ctx, &wheel_down);
        surface = list_view.draw(&ctx);
        // At the bottom we do not advance further.
        assert_eq!(list_view.scroll.top, 1);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 3);

        // Escape resets the viewport and brings the cursor into view.
        list_view.handle_event(
            &mut event_ctx,
            &Event::KeyPress(Key {
                codepoint: Key::ESCAPE,
                ..Key::default()
            }),
        );
        surface = list_view.draw(&ctx);
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 2);

        let cursor_down = Event::KeyPress(Key {
            codepoint: u32::from('j'),
            ..Key::default()
        });

        list_view.handle_event(&mut event_ctx, &cursor_down);
        surface = list_view.draw(&ctx);
        // Cursor down, scroll unchanged.
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 2);
        assert_eq!(list_view.cursor, 1);

        list_view.handle_event(&mut event_ctx, &cursor_down);
        surface = list_view.draw(&ctx);
        // Cursor down, scroll advances one row.
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 1);
        assert_eq!(surface.children.len(), 3);
        assert_eq!(list_view.cursor, 2);

        list_view.handle_event(&mut event_ctx, &cursor_down);
        surface = list_view.draw(&ctx);
        // Cursored onto the last item: the whole last item comes into view.
        assert_eq!(list_view.scroll.top, 1);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 3);
        assert_eq!(list_view.cursor, 3);
    }

    /// A builder that counts how many times it is queried, so we can assert that
    /// jumps and scroll-to-bottom do not walk every intermediate child.
    struct CountingBuilder {
        len: usize,
        widget: WidgetRef,
        calls: Rc<StdCell<usize>>,
    }

    impl Builder for CountingBuilder {
        fn item_at_idx(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
            self.calls.set(self.calls.get() + 1);
            if idx >= self.len {
                return None;
            }
            Some(Rc::clone(&self.widget))
        }
    }

    #[test]
    fn list_view_jump_to_item_avoids_walking_intermediate_children() {
        let calls = Rc::new(StdCell::new(0usize));
        let builder = CountingBuilder {
            len: 1000,
            widget: text("item"),
            calls: Rc::clone(&calls),
        };
        let mut list_view = ListView {
            item_count: Some(1000),
            ..ListView::new(Source::Builder(Box::new(builder)))
        };

        let ctx = draw_ctx(16, 4);
        list_view.jump_to_item(999);
        let surface = list_view.draw(&ctx);

        assert_eq!(list_view.cursor, 999);
        assert_eq!(list_view.scroll.top, 996);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 4);
        assert!(calls.get() < 10);
    }

    #[test]
    fn list_view_jump_to_item_clamps_to_item_count() {
        let mut list_view = ListView {
            item_count: Some(10),
            ..ListView::new(Source::Slice(Vec::new()))
        };

        list_view.jump_to_item(100);

        assert_eq!(list_view.cursor, 9);
        assert_eq!(list_view.scroll.top, 9);
        assert_eq!(list_view.scroll.offset, 0);
    }

    #[test]
    fn list_view_scroll_to_bottom_avoids_walking_intermediate_children() {
        let calls = Rc::new(StdCell::new(0usize));
        let builder = CountingBuilder {
            len: 1000,
            widget: text("item"),
            calls: Rc::clone(&calls),
        };
        let mut list_view = ListView {
            item_count: Some(1000),
            ..ListView::new(Source::Builder(Box::new(builder)))
        };

        let ctx = draw_ctx(16, 4);
        list_view.scroll_to_bottom();
        let surface = list_view.draw(&ctx);

        assert_eq!(list_view.cursor, 0);
        assert_eq!(list_view.scroll.top, 996);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 4);
        assert!(calls.get() < 10);
    }

    #[test]
    fn list_view_scroll_to_bottom_gets_count_from_slice() {
        let mut list_view = ListView::new(Source::Slice(vec![text("0"), text("1"), text("2")]));

        list_view.scroll_to_bottom();

        assert_eq!(list_view.cursor, 0);
        // One past the end: the draw back-fills upward from the last item.
        assert_eq!(list_view.scroll.top, 3);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(list_view.item_count, Some(3));
    }

    #[test]
    fn list_view_scroll_to_bottom_shows_the_tail_of_an_oversized_last_item() {
        // The last item is far taller than the 8-row viewport. Following the
        // tail must anchor its bottom edge to the viewport bottom, not pin
        // the viewport to its top rows.
        let tall = (0..31)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut list_view = ListView::new(Source::Slice(vec![text("a"), text("b"), text(&tall)]));

        let ctx = draw_ctx(16, 8);
        list_view.scroll_to_bottom();
        let surface = list_view.draw(&ctx);

        let last = surface.children.last().expect("visible child");
        assert_eq!(last.surface.size.height, 31);
        assert_eq!(last.origin.row + i32::from(last.surface.size.height), 8);
        assert!(list_view.is_at_bottom());
    }

    #[test]
    fn list_view_scroll_to_bottom_on_an_underfull_list_anchors_from_the_top() {
        let mut list_view = ListView::new(Source::Slice(vec![text("a"), text("b"), text("c")]));

        let ctx = draw_ctx(16, 8);
        list_view.scroll_to_bottom();
        let surface = list_view.draw(&ctx);

        assert_eq!(surface.children.len(), 3);
        assert_eq!(surface.children[0].origin.row, 0);
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 0);
        assert!(list_view.is_at_bottom());
    }

    /// A huge `scroll_lines` in either direction lands on the end, never past
    /// it: the draw clamps at both the top and the bottom.
    #[test]
    fn list_view_scroll_lines_clamps_at_both_ends() {
        let mut list_view = ListView::new(Source::Slice(vec![
            text("0"),
            text("1"),
            text("2"),
            text("3"),
            text("4"),
            text("5"),
            text("6"),
        ]));
        list_view.draw_cursor = false;

        let ctx = draw_ctx(16, 4);
        // Establish item_count and the initial (top) scroll state.
        list_view.draw(&ctx);
        assert_eq!(list_view.scroll.top, 0);
        assert!(!list_view.is_at_bottom());

        // A page taller than the remaining content lands on the bottom.
        list_view.scroll_lines(100);
        let surface = list_view.draw(&ctx);
        assert!(list_view.is_at_bottom());
        assert_eq!(list_view.scroll.top, 3);
        assert_eq!(surface.children.len(), 4);

        // Scrolling down again from the bottom does not move past it.
        list_view.scroll_lines(100);
        list_view.draw(&ctx);
        assert!(list_view.is_at_bottom());
        assert_eq!(list_view.scroll.top, 3);

        // A huge upward scroll lands on the top, not above it.
        list_view.scroll_lines(-100);
        list_view.draw(&ctx);
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 0);
        assert!(!list_view.is_at_bottom());

        // Scrolling up again from the top does not move past it.
        list_view.scroll_lines(-100);
        list_view.draw(&ctx);
        assert_eq!(list_view.scroll.top, 0);
        assert_eq!(list_view.scroll.offset, 0);
    }

    /// The list records the viewport height it last drew against, so a
    /// page-scroll caller can scale by whole viewports. `None` before the
    /// first draw.
    #[test]
    fn list_view_records_viewport_height_after_draw() {
        let mut list_view = ListView::new(Source::Slice(vec![text("0"), text("1")]));
        assert_eq!(list_view.viewport_height(), None);
        list_view.draw(&draw_ctx(16, 7));
        assert_eq!(list_view.viewport_height(), Some(7));
    }

    /// `scroll_top` / `scroll_offset` report the viewport top the last draw
    /// reconciled, so a caller can map screen rows into the content's line
    /// space. Scrolling a few lines into a multi-line first item advances the
    /// offset without changing the top item; scrolling past it advances the
    /// top.
    #[test]
    fn list_view_reports_reconciled_scroll_top_and_offset() {
        let mut list_view = ListView::new(Source::Slice(vec![
            text("0\n1\n2\n3\n4"),
            text("5"),
            text("6"),
            text("7"),
        ]));
        list_view.draw_cursor = false;
        let ctx = draw_ctx(16, 3);
        list_view.draw(&ctx);
        assert_eq!((list_view.scroll_top(), list_view.scroll_offset()), (0, 0));

        // Two lines into the five-line first item: same top, offset 2.
        list_view.scroll_lines(2);
        list_view.draw(&ctx);
        assert_eq!((list_view.scroll_top(), list_view.scroll_offset()), (0, 2));

        // Past the first item: the top advances to the second item.
        list_view.scroll_lines(4);
        list_view.draw(&ctx);
        assert_eq!(list_view.scroll_top(), 1);
        assert_eq!(list_view.scroll_offset(), 0);
    }

    #[test]
    fn list_view_uneven_scroll() {
        let mut list_view = ListView::new(Source::Slice(vec![
            text("0"),
            text("1"),
            text("2"),
            text("3"),
            text("4"),
            text("5"),
            text("6"),
        ]));
        list_view.wheel_scroll = 1;

        let ctx = draw_ctx(16, 4);
        // Initial draw to establish item_count and the scroll state.
        list_view.draw(&ctx);

        let mut event_ctx = EventContext::new();
        let wheel_down = Event::Mouse(mouse_event(mouse::Button::WheelDown));
        let wheel_up = Event::Mouse(mouse_event(mouse::Button::WheelUp));

        list_view.handle_event(&mut event_ctx, &wheel_down);
        list_view.handle_event(&mut event_ctx, &wheel_down);
        list_view.handle_event(&mut event_ctx, &wheel_down);
        let mut surface = list_view.draw(&ctx);
        assert_eq!(list_view.scroll.top, 3);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 4);

        list_view.handle_event(&mut event_ctx, &wheel_up);
        list_view.handle_event(&mut event_ctx, &wheel_up);
        surface = list_view.draw(&ctx);
        assert_eq!(list_view.scroll.top, 1);
        assert_eq!(list_view.scroll.offset, 0);
        assert_eq!(surface.children.len(), 4);
    }

    fn mouse_event(button: mouse::Button) -> mouse::Mouse {
        mouse::Mouse {
            col: 0,
            row: 0,
            xoffset: 0,
            yoffset: 0,
            button,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Press,
        }
    }

    #[test]
    fn fenwick_prefix_sums_adds_pushes_and_truncate() {
        let mut f = Fenwick::new();
        assert_eq!(f.len(), 0);
        assert_eq!(f.prefix(0), 0);

        for _ in 0..5 {
            f.push();
        }
        assert_eq!(f.len(), 5);
        assert_eq!(f.prefix(5), 0);

        f.add(0, 3);
        f.add(2, 7);
        f.add(4, 5);
        // Values [3, 0, 7, 0, 5] give prefix sums [0, 3, 3, 10, 10, 15].
        assert_eq!(f.prefix(0), 0);
        assert_eq!(f.prefix(1), 3);
        assert_eq!(f.prefix(2), 3);
        assert_eq!(f.prefix(3), 10);
        assert_eq!(f.prefix(4), 10);
        assert_eq!(f.prefix(5), 15);
        // `i` beyond len clamps to the whole sum.
        assert_eq!(f.prefix(100), 15);

        // A negative delta (a re-measured item that shrank) subtracts.
        f.add(2, -2);
        assert_eq!(f.prefix(3), 8);
        assert_eq!(f.prefix(5), 13);

        // Append preserves existing values, the new slot holds 0.
        f.push();
        assert_eq!(f.len(), 6);
        assert_eq!(f.prefix(6), 13);
        f.add(5, 4);
        assert_eq!(f.prefix(6), 17);

        // Truncate drops the tail, retaining the earlier prefix sums.
        f.truncate(3);
        assert_eq!(f.len(), 3);
        assert_eq!(f.prefix(1), 3);
        assert_eq!(f.prefix(3), 8);
        // Truncate to a length at or above the current one is a no-op.
        f.truncate(10);
        assert_eq!(f.len(), 3);
        assert_eq!(f.prefix(3), 8);
    }

    #[test]
    fn list_geometry_measured_and_estimated_extents() {
        let mut g = ListGeometry::new();
        g.set_len(5);
        // Before any measurement every item estimates at 1.
        assert_eq!(g.estimate(), 1);
        assert_eq!(g.total(), 5);
        assert_eq!(g.offset_for_index(3), 3);

        g.set_measured(0, 10);
        g.set_measured(1, 4);
        // Mean of {10, 4} is 7.
        assert_eq!(g.estimate(), 7);
        // Measured items contribute exact heights.
        assert_eq!(g.offset_for_index(2), 14);
        // Below index 4: measured 14 plus two unmeasured items at 7 each.
        assert_eq!(g.offset_for_index(4), 28);
        // Total: measured 14 plus three unmeasured items at 7 each.
        assert_eq!(g.total(), 35);

        // item_at_line finds the item whose top is the largest offset <= line.
        assert_eq!(g.item_at_line(0), 0);
        assert_eq!(g.item_at_line(13), 1);
        assert_eq!(g.item_at_line(14), 2);
        // Beyond the total clamps to the last item.
        assert_eq!(g.item_at_line(10_000), 4);
    }

    #[test]
    fn list_geometry_set_len_grow_preserves_and_shrink_drops() {
        let mut g = ListGeometry::new();
        g.set_len(3);
        g.set_measured(0, 5);
        g.set_measured(1, 6);
        g.set_measured(2, 7);
        assert_eq!(g.total(), 18);

        // Growing preserves existing measurements. New slots estimate at the
        // mean (18 / 3 = 6).
        g.set_len(5);
        assert_eq!(g.len(), 5);
        assert_eq!(g.estimate(), 6);
        assert_eq!(g.offset_for_index(3), 18);
        assert_eq!(g.total(), 30);

        // Shrinking drops the tail and recomputes the mean over the retained
        // items ({5, 6} -> 11 / 2 = 5).
        g.set_len(2);
        assert_eq!(g.len(), 2);
        assert_eq!(g.estimate(), 5);
        assert_eq!(g.total(), 11);
    }

    #[test]
    fn list_geometry_remeasure_updates_without_double_counting() {
        let mut g = ListGeometry::new();
        g.set_len(3);
        g.set_measured(0, 5);
        g.set_measured(1, 5);
        assert_eq!(g.offset_for_index(2), 10);
        assert_eq!(g.estimate(), 5);

        // A re-layout gives index 0 a new height.
        g.set_measured(0, 20);
        assert_eq!(g.offset_for_index(1), 20);
        assert_eq!(g.offset_for_index(2), 25);
        // Mean is (20 + 5) / 2 = 12, measured count still 2.
        assert_eq!(g.estimate(), 12);
        // Item 2 stays unmeasured, contributing the estimate.
        assert_eq!(g.total(), 25 + 12);
    }

    /// The list records each drawn child's height into the geometry, so
    /// `content_extent` reflects the measured visible heights plus the
    /// estimated remainder, and `viewport_top_line` tracks the top item's line
    /// offset after a scroll.
    #[test]
    fn list_view_geometry_tracks_measured_and_estimated_extent() {
        let mut list_view = ListView::new(Source::Slice(vec![
            text("0\n1\n2"), // 3 rows
            text("3"),       // 1 row
            text("4\n5"),    // 2 rows
            text("6"),       // 1 row
            text("7"),       // 1 row
        ]));
        list_view.draw_cursor = false;

        let ctx = draw_ctx(16, 4);
        list_view.draw(&ctx);

        // Visible items 0 (h3) and 1 (h1) are measured, so the mean is 2 and
        // the three unmeasured items each estimate at 2: 4 + 3 * 2 = 10.
        assert_eq!(list_view.content_extent(), Some(10));
        assert_eq!(list_view.viewport_top_line(), Some(0));

        // Scroll past the 3-row first item: the top lands on item 1 at line 3.
        list_view.scroll_lines(3);
        list_view.draw(&ctx);
        assert_eq!(list_view.scroll_top(), 1);
        assert_eq!(list_view.scroll_offset(), 0);
        assert_eq!(list_view.viewport_top_line(), Some(3));
        // Items 0..=3 are now measured (3 + 1 + 2 + 1 = 7), item 4 estimates at
        // 7 / 4 = 1, so the extent is 7 + 1 = 8.
        assert_eq!(list_view.content_extent(), Some(8));
    }
}
