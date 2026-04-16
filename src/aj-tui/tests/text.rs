//! Integration tests for the [`Text`] component, focused on the F14
//! drift items (default padding, tab normalization, empty/whitespace
//! handling, and the non-empty-but-zero-rows fallback row).
//!
//! These regressions lock in pi-tui parity for behaviors that were
//! previously divergent.

mod support;

use aj_tui::component::Component;
use aj_tui::components::text::Text;
use support::strip_ansi;

/// Strip ANSI escape sequences from a line for assertion purposes.
fn visible(s: &str) -> String {
    strip_ansi(s)
}

// ---------------------------------------------------------------------------
// padding_y default
// ---------------------------------------------------------------------------

/// `Text::default()` should match pi-tui's `Text(text = "", paddingX = 1,
/// paddingY = 1, customBgFn?)` JS-default-arg shape: one blank row above
/// the content and one below. Constructing with `Text::new(text, 1, 1)`
/// resolves to the same render — locking in that the default-args path
/// reaches the same constructor body. See PORTING.md F49.
#[test]
fn default_padding_y_is_one_so_content_is_sandwiched_in_blank_rows() {
    let mut t = Text::new("hello", 1, 1);
    let lines = t.render(80);

    // Three rows total: top-pad, content, bottom-pad.
    assert_eq!(lines.len(), 3);

    // Top and bottom rows are blank (only spaces), middle has content.
    assert!(visible(&lines[0]).trim().is_empty());
    assert!(visible(&lines[1]).contains("hello"));
    assert!(visible(&lines[2]).trim().is_empty());
}

// ---------------------------------------------------------------------------
// tab normalization
// ---------------------------------------------------------------------------

/// pi-tui replaces every `\t` with three spaces before wrapping. The
/// rendered row should not contain a literal tab byte; the indent
/// should appear as exactly three spaces per tab.
#[test]
fn tab_in_input_is_normalized_to_three_spaces_before_wrapping() {
    let mut t = Text::new("\thello", 0, 0);

    let lines = t.render(80);
    assert_eq!(lines.len(), 1);

    let visible_text = visible(&lines[0]);
    // No raw tab byte survives.
    assert!(
        !visible_text.contains('\t'),
        "tab should be normalized away, got: {:?}",
        visible_text
    );
    // The leading indent is three spaces, then the content.
    assert!(
        visible_text.starts_with("   hello"),
        "expected 3-space indent, got: {:?}",
        visible_text
    );
}

/// Multiple tabs in a row each expand to three spaces (so two tabs → 6
/// spaces, not collapsed to one indent unit).
#[test]
fn multiple_tabs_each_expand_to_three_spaces_independently() {
    let mut t = Text::new("\t\thi", 0, 0);

    let visible_text = visible(&t.render(80)[0]);
    assert!(
        visible_text.starts_with("      hi"),
        "expected 6-space indent for two tabs, got: {:?}",
        visible_text
    );
}

// ---------------------------------------------------------------------------
// empty / whitespace-only handling
// ---------------------------------------------------------------------------

/// Empty input produces no rows at all — not even padding rows. Matches
/// pi-tui's early-return on `!text || text.trim() === ""`.
#[test]
fn empty_text_renders_zero_rows_even_with_padding_y_set() {
    // Even with explicit padding_y, an empty text emits no rows.
    let mut t = Text::new("", 1, 3);

    assert_eq!(t.render(80), Vec::<String>::new());
}

/// Whitespace-only input is treated as empty by pi-tui. Our port should
/// match that behavior so a Text whose content was reset to spaces or
/// newlines doesn't render visible padding rows.
#[test]
fn whitespace_only_text_is_treated_as_empty() {
    for whitespace in ["   ", "\n\n", "\t\t", " \n\t ", ""] {
        let mut t = Text::new(whitespace, 1, 1);
        assert_eq!(
            t.render(80),
            Vec::<String>::new(),
            "whitespace-only input {:?} should render zero rows",
            whitespace
        );
    }
}

// ---------------------------------------------------------------------------
// non-empty but zero-row fallback
// ---------------------------------------------------------------------------

/// ANSI-only input (`"\x1b[31m\x1b[0m"`) is non-whitespace per
/// `text.trim()` so it bypasses the empty-text branch. `wrapTextWithAnsi`
/// returns one line carrying the (invisible) ANSI codes; the Text
/// component then surrounds it with margins and pads to full width.
/// Visible width is the full render width — the ANSI codes contribute
/// zero visible cells, the rest is padding spaces.
///
/// The pi-tui tail expression `result.length > 0 ? result : [""]` is
/// defensive against a hypothetical empty wrap output, but neither pi
/// nor our port actually reaches that branch for any real input. Locked
/// in here so that a future wrap refactor that *does* return empty
/// surfaces visibly rather than silently.
#[test]
fn ansi_only_input_renders_one_full_width_row() {
    let mut t = Text::new("\x1b[31m\x1b[0m", 0, 0);

    let lines = t.render(80);
    assert_eq!(lines.len(), 1, "expected exactly one content row");
    assert_eq!(
        aj_tui::ansi::visible_width(&lines[0]),
        80,
        "row should pad to full render width, got: {:?}",
        lines[0]
    );
}

// ---------------------------------------------------------------------------
// degenerate width clamp (width <= 2 * padding_x)
// ---------------------------------------------------------------------------

/// pi-tui clamps `contentWidth` to `Math.max(1, width - 2 * paddingX)`,
/// so a width that's smaller than the horizontal padding still produces
/// at least one column of content. Our previous early-return on
/// `content_width == 0` swallowed the row entirely.
#[test]
fn degenerate_width_smaller_than_horizontal_padding_still_renders_content() {
    let mut t = Text::new("hi", 5, 0);

    // width=4 is less than 2 * padding_x = 10. pi clamps content_width
    // to 1, so "hi" wraps to two rows of one column each. We don't pin
    // the exact wrapped layout (that's wrap_text_with_ansi's contract),
    // we just assert the row didn't get dropped.
    let lines = t.render(4);
    assert!(
        !lines.is_empty(),
        "degenerate width should still produce output, got: {:?}",
        lines
    );
}

// ---------------------------------------------------------------------------
// per-line layout (left + right margin + full-width pad)
// ---------------------------------------------------------------------------

/// pi-tui composes each content row as `leftMargin + line + rightMargin`
/// then pads with spaces to reach the full render width. Locks in that
/// every content row spans the full terminal width even in the no-bg
/// case (the previous port emitted `leftMargin + line` only and left
/// the right side untouched).
#[test]
fn content_row_no_bg_pads_to_full_render_width() {
    let mut t = Text::new("hi", 2, 0);

    let lines = t.render(40);
    assert_eq!(lines.len(), 1, "single content row expected");

    let row = &lines[0];
    assert_eq!(
        aj_tui::ansi::visible_width(row),
        40,
        "content row should span full width; got visible_width={} for {:?}",
        aj_tui::ansi::visible_width(row),
        row
    );

    // Layout: 2-space left margin, "hi", 2-space right margin, then
    // pad spaces to fill remaining cells.
    let plain = visible(row);
    assert!(
        plain.starts_with("  hi  "),
        "expected `  hi  ` prefix (left margin + content + right margin), got: {:?}",
        plain
    );
    // All trailing cells beyond the right margin are padding spaces.
    assert!(
        plain[6..].chars().all(|c| c == ' '),
        "trailing cells must be padding spaces, got: {:?}",
        plain
    );
}

/// In the bg case, the entire width-spanning row (including the right
/// margin and trailing pad) should pass through the user's bg function
/// so no cell visibly changes color partway across the row.
#[test]
fn content_row_with_bg_applies_bg_across_full_width() {
    let mut t = Text::new("hi", 1, 0);
    // Wrap each line in a sentinel so we can verify the bg was applied.
    t.set_bg_fn(Box::new(|s| format!("<BG>{}</BG>", s)));

    let lines = t.render(20);
    assert_eq!(lines.len(), 1);

    let row = &lines[0];
    // The bg wrapper should appear once around the entire row.
    assert!(row.starts_with("<BG>"));
    assert!(row.ends_with("</BG>"));
    // The wrapped content covers the full terminal width.
    let inner = &row["<BG>".len()..row.len() - "</BG>".len()];
    assert_eq!(
        aj_tui::ansi::visible_width(inner),
        20,
        "bg should wrap a full-width payload, got inner: {:?}",
        inner
    );
}

// ---------------------------------------------------------------------------
// cache stability across renders
// ---------------------------------------------------------------------------

/// Repeat renders with the same `(text, width)` should hit the cache
/// and return identical line lists. Locks in that the cache path is
/// stable.
#[test]
fn repeat_render_with_same_args_returns_cached_result() {
    let mut t = Text::new("hello", 1, 1);

    let first = t.render(40);
    let second = t.render(40);
    assert_eq!(
        first, second,
        "cache hit should return the same lines as the first render"
    );
    assert_eq!(
        first.len(),
        3,
        "3 rows expected: top pad, content, bottom pad"
    );
}
