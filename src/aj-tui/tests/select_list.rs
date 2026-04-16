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
    let mut list = SelectList::new(items, 5);
    list.set_theme(identity_select_list_theme());
    list.set_layout(layout);
    list
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
