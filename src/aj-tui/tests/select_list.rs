//! Component tests for `SelectList`.
//!
//! These target the rendering surface: description normalization,
//! primary-column alignment, min/max bounds, and the custom-truncator
//! escape hatch. We call `render(width)` directly — no virtual terminal
//! needed — so the assertions only care about what the component put on
//! each line.

mod support;

use aj_tui::ansi::visible_width;
use aj_tui::component::Component;
use aj_tui::components::select_list::{
    SelectItem, SelectList, SelectListLayout, TruncatePrimaryContext,
};

use support::themes::identity_select_list_theme;
use support::visible_index_of;

fn make_list(items: Vec<SelectItem>, layout: SelectListLayout) -> SelectList {
    // F49: theme + layout are required at construction; no setters.
    SelectList::new(items, 5, identity_select_list_theme(), layout)
}

#[test]
fn normalizes_multiline_descriptions_to_single_line() {
    let items =
        vec![SelectItem::new("test", "test").with_description("Line one\nLine two\nLine three")];

    let mut list = make_list(items, SelectListLayout::default());
    let rendered = list.render(100);

    assert!(!rendered.is_empty());
    assert!(
        !rendered[0].contains('\n'),
        "row {:?} contains a newline",
        rendered[0],
    );
    assert!(
        rendered[0].contains("Line one Line two Line three"),
        "row {:?} should merge newlines into single spaces",
        rendered[0],
    );
}

#[test]
fn keeps_descriptions_aligned_when_primary_text_is_truncated() {
    let items = vec![
        SelectItem::new("short", "short").with_description("short description"),
        SelectItem::new(
            "very-long-command-name-that-needs-truncation",
            "very-long-command-name-that-needs-truncation",
        )
        .with_description("long description"),
    ];

    let mut list = make_list(items, SelectListLayout::default());
    let rendered = list.render(80);

    assert_eq!(
        visible_index_of(&rendered[0], "short description"),
        visible_index_of(&rendered[1], "long description"),
        "descriptions should line up across rows; got rows:\n  [0]: {:?}\n  [1]: {:?}",
        rendered[0],
        rendered[1],
    );
}

#[test]
fn uses_the_configured_minimum_primary_column_width() {
    let items = vec![
        SelectItem::new("a", "a").with_description("first"),
        SelectItem::new("bb", "bb").with_description("second"),
    ];

    let mut list = make_list(
        items,
        SelectListLayout {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(20),
            truncate_primary: None,
        },
    );
    let rendered = list.render(80);

    // Descriptions start at column 14 = prefix(2) + primary column (12).
    assert_eq!(visible_index_of(&rendered[0], "first"), 14);
    assert_eq!(visible_index_of(&rendered[1], "second"), 14);
}

#[test]
fn uses_the_configured_maximum_primary_column_width() {
    let items = vec![
        SelectItem::new(
            "very-long-command-name-that-needs-truncation",
            "very-long-command-name-that-needs-truncation",
        )
        .with_description("first"),
        SelectItem::new("short", "short").with_description("second"),
    ];

    let mut list = make_list(
        items,
        SelectListLayout {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(20),
            truncate_primary: None,
        },
    );
    let rendered = list.render(80);

    // Descriptions start at column 22 = prefix(2) + primary column (20).
    assert_eq!(visible_index_of(&rendered[0], "first"), 22);
    assert_eq!(visible_index_of(&rendered[1], "second"), 22);
}

#[test]
fn allows_overriding_primary_truncation_while_preserving_description_alignment() {
    let items = vec![
        SelectItem::new(
            "very-long-command-name-that-needs-truncation",
            "very-long-command-name-that-needs-truncation",
        )
        .with_description("first"),
        SelectItem::new("short", "short").with_description("second"),
    ];

    let mut list = make_list(
        items,
        SelectListLayout {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(12),
            truncate_primary: Some(Box::new(|ctx: TruncatePrimaryContext<'_>| {
                if visible_width(ctx.text) <= ctx.max_width {
                    ctx.text.to_string()
                } else {
                    let keep = ctx.max_width.saturating_sub(1);
                    let prefix: String = ctx.text.chars().take(keep).collect();
                    format!("{}…", prefix)
                }
            })),
        },
    );
    let rendered = list.render(80);

    assert!(
        rendered[0].contains('…'),
        "expected an ellipsis in the truncated primary row {:?}",
        rendered[0],
    );
    assert_eq!(
        visible_index_of(&rendered[0], "first"),
        visible_index_of(&rendered[1], "second"),
    );
}

// ---------------------------------------------------------------------------
// Filter + selection
// ---------------------------------------------------------------------------

#[test]
fn filter_restricts_visible_items_and_resets_selection() {
    let items = vec![
        SelectItem::new("apple", "apple"),
        SelectItem::new("apricot", "apricot"),
        SelectItem::new("banana", "banana"),
    ];
    let mut list = make_list(items, SelectListLayout::default());

    list.set_filter("ap");
    let rendered = list.render(80);

    let joined = rendered.join("\n");
    assert!(joined.contains("apple"));
    assert!(joined.contains("apricot"));
    assert!(!joined.contains("banana"));
    assert_eq!(
        list.selected_item().map(|i| i.value.as_str()),
        Some("apple"),
        "filter should reset selection to the first match",
    );
}

#[test]
fn shows_no_match_row_when_filter_matches_nothing() {
    let items = vec![SelectItem::new("apple", "apple")];
    let mut list = make_list(items, SelectListLayout::default());

    list.set_filter("zz");
    let rendered = list.render(80);

    assert_eq!(rendered.len(), 1);
    assert!(rendered[0].contains("No matching"));
}

#[test]
fn arrow_keys_wrap_around_and_emit_selection_changes() {
    use aj_tui::keys::Key;

    let items = vec![
        SelectItem::new("a", "a"),
        SelectItem::new("b", "b"),
        SelectItem::new("c", "c"),
    ];
    let mut list = make_list(items, SelectListLayout::default());

    // Up from index 0 wraps to the last item.
    list.handle_input(&Key::up());
    assert_eq!(list.selected_item().map(|i| i.value.as_str()), Some("c"));

    // Down wraps back to the first item.
    list.handle_input(&Key::down());
    assert_eq!(list.selected_item().map(|i| i.value.as_str()), Some("a"));
}

// ---------------------------------------------------------------------------
// Stateless centering scroll model (F13 + F32 in PORTING.md)
//
// These tests lock in the chosen scroll model, which mirrors pi's
// `select-list.ts`: the visible window is recomputed from `selected` on
// every render via clamp(selected - max_visible / 2, 0, len - max_visible).
// They cover sequential navigation (selection floats near center),
// wraparound at both ends of a long list (window clamps to top/bottom
// edge), and filter resets.
// ---------------------------------------------------------------------------

/// Build a 10-item list (`item0`..`item9`) with `max_visible = 3` so we
/// always exercise the scrolling path.
fn make_long_list() -> SelectList {
    let items: Vec<SelectItem> = (0..10)
        .map(|i| {
            let s = format!("item{}", i);
            SelectItem::new(&s, &s)
        })
        .collect();
    // F49: theme + layout are required at construction; no setters.
    SelectList::new(
        items,
        3,
        identity_select_list_theme(),
        SelectListLayout::default(),
    )
}

#[test]
fn sequential_down_navigation_centers_selection_in_window() {
    use aj_tui::keys::Key;

    let mut list = make_long_list();

    // Press Down 4 times — selected becomes 4. With pi's centering
    // formula and max_visible = 3, the window starts at
    // clamp(4 - 1, 0, 7) = 3, so [item3, item4, item5] are visible and
    // item2 / item6 are not.
    for _ in 0..4 {
        list.handle_input(&Key::down());
    }
    assert_eq!(
        list.selected_item().map(|i| i.value.as_str()),
        Some("item4"),
    );

    let rendered = list.render(80);
    let joined = rendered.join("\n");
    assert!(joined.contains("item3"), "rendered:\n{}", joined);
    assert!(joined.contains("item4"), "rendered:\n{}", joined);
    assert!(joined.contains("item5"), "rendered:\n{}", joined);
    assert!(!joined.contains("item2"), "rendered:\n{}", joined);
    assert!(!joined.contains("item6"), "rendered:\n{}", joined);
}

#[test]
fn up_wrap_at_top_clamps_window_to_show_selection_at_bottom_edge() {
    use aj_tui::keys::Key;

    let mut list = make_long_list();

    // Up at index 0 wraps to len-1 = 9; centering formula gives
    // start = clamp(9 - 1, 0, 7) = 7, so window is [item7, item8, item9].
    list.handle_input(&Key::up());
    assert_eq!(
        list.selected_item().map(|i| i.value.as_str()),
        Some("item9"),
    );

    let rendered = list.render(80);
    let joined = rendered.join("\n");
    assert!(joined.contains("item7"), "rendered:\n{}", joined);
    assert!(joined.contains("item8"), "rendered:\n{}", joined);
    assert!(joined.contains("item9"), "rendered:\n{}", joined);
    assert!(!joined.contains("item6"), "rendered:\n{}", joined);
    assert!(!joined.contains("item0"), "rendered:\n{}", joined);
}

#[test]
fn down_wrap_at_bottom_clamps_window_to_show_selection_at_top_edge() {
    use aj_tui::keys::Key;

    let mut list = make_long_list();

    // Jump to the last item — window clamps to [item7..item9].
    list.set_selected_index(9);

    // Down at len-1 wraps to 0; centering formula gives
    // start = clamp(0 - 1, 0, 7) = 0, so window is [item0, item1, item2].
    list.handle_input(&Key::down());
    assert_eq!(
        list.selected_item().map(|i| i.value.as_str()),
        Some("item0"),
    );

    let rendered = list.render(80);
    let joined = rendered.join("\n");
    assert!(joined.contains("item0"), "rendered:\n{}", joined);
    assert!(joined.contains("item1"), "rendered:\n{}", joined);
    assert!(joined.contains("item2"), "rendered:\n{}", joined);
    assert!(!joined.contains("item3"), "rendered:\n{}", joined);
    // The previous tail items must not bleed into the new window.
    assert!(!joined.contains("item7"), "rendered:\n{}", joined);
    assert!(!joined.contains("item8"), "rendered:\n{}", joined);
    assert!(!joined.contains("item9"), "rendered:\n{}", joined);
}

#[test]
fn set_filter_resets_selection_to_top_and_window_follows() {
    use aj_tui::keys::Key;

    let mut list = make_long_list();

    // Scroll down so the window is mid-list.
    for _ in 0..5 {
        list.handle_input(&Key::down());
    }
    assert_eq!(
        list.selected_item().map(|i| i.value.as_str()),
        Some("item5"),
    );

    // Re-applying the (empty) filter resets selection to 0; the window
    // recomputes from selected (no scroll_offset to clear).
    list.set_filter("");
    assert_eq!(
        list.selected_item().map(|i| i.value.as_str()),
        Some("item0"),
    );

    let rendered = list.render(80);
    let joined = rendered.join("\n");
    assert!(joined.contains("item0"), "rendered:\n{}", joined);
    assert!(joined.contains("item1"), "rendered:\n{}", joined);
    assert!(joined.contains("item2"), "rendered:\n{}", joined);
    assert!(!joined.contains("item5"), "rendered:\n{}", joined);
}

#[test]
fn scroll_info_is_truncated_to_terminal_width_minus_two() {
    // Pi's render truncates the `(N/TOTAL)` indicator to `width - 2` to
    // keep narrow overlays readable (F31 in PORTING.md).
    let items: Vec<SelectItem> = (0..100)
        .map(|i| {
            let s = format!("item{}", i);
            SelectItem::new(&s, &s)
        })
        .collect();
    let mut list = SelectList::new(
        items,
        3,
        identity_select_list_theme(),
        SelectListLayout::default(),
    );

    // The full indicator is "  (1/100)" = 9 cells. Render at width = 6
    // so the truncate path actually fires (max becomes 4).
    let rendered = list.render(6);
    // Last line is the scroll indicator.
    let info_line = rendered.last().expect("expected scroll info line");
    let visible = aj_tui::ansi::visible_width(info_line);
    assert!(
        visible <= 4,
        "scroll info should be truncated to width-2 = 4 cells; got {} cells in {:?}",
        visible,
        info_line,
    );
}
