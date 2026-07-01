//! A port of libvaxis's `Table` widget: a data-driven table drawn onto a
//! [`Window`], with a selectable/active row, alternating row backgrounds, and
//! an optionally expanding active row.
//!
//! # Rows and cells
//!
//! Upstream reflects over an arbitrary row struct at comptime: field names
//! become headers and field types drive per-cell formatting. Rust has no
//! comptime reflection, so that capability is split into two traits:
//!
//! - [`TableRow`] exposes a row type's columns: `headers()`, `column_count()`,
//!   and `cell(col)`. It is derived with `#[derive(TableRow)]`, which maps each
//!   named field to a column.
//! - [`TableCell`] formats one value into a [`Cow<str>`]. A blanket impl covers
//!   every [`Display`] type (strings, integers, enums with a `Display` impl,
//!   which stands in for upstream's `@tagName`).
//!
//! Because a blanket `impl<T: Display> TableCell for T` cannot coexist with an
//! `impl<T> TableCell for Option<T>` (a future `Display for Option` would make
//! them overlap), `Option` fields are handled by the derive: it unwraps them in
//! `cell`, rendering `Some` via the inner value and `None` as `-`. This matches
//! upstream, which special-cases the optional field type rather than the value.
//!
//! # Widget state
//!
//! [`TableContext`] holds the mutable table state (active/selected row, scroll
//! start, colors, layout) that the application owns across frames and passes to
//! [`draw_table`] each draw, mirroring upstream's `TableContext`.

use std::borrow::Cow;
use std::fmt::Display;

use crate::cell::{Cell, Color, Segment, Style, Underline};
use crate::widgets::alignment;
use crate::window::{BorderOptions, BorderWhere, ChildOptions, PrintOptions, Window, Wrap};

/// Formats one cell value into display text.
///
/// A blanket impl covers every [`Display`] type, so `String`, `&str`, the
/// integer types, and enums that implement `Display` are cells without extra
/// code. `Option` is intentionally *not* implemented here: it would overlap the
/// blanket impl (see the module docs), so the [`TableRow`] derive unwraps
/// `Option` fields itself, rendering `None` as `-`.
pub trait TableCell {
    /// Returns this value's cell text.
    fn to_cell(&self) -> Cow<'_, str>;
}

impl<T: Display> TableCell for T {
    fn to_cell(&self) -> Cow<'_, str> {
        Cow::Owned(self.to_string())
    }
}

/// A row type whose columns are known statically.
///
/// Derived with `#[derive(TableRow)]` for structs with named fields. The
/// derive keeps `headers` and `cell` aligned: column `col` of `cell` formats
/// the same field whose name is `headers()[col]`.
pub trait TableRow {
    /// The column headers, in column order.
    fn headers() -> Vec<Cow<'static, str>>;

    /// The number of columns. Equals `headers().len()`.
    fn column_count() -> usize;

    /// The text of column `col` for this row. Out-of-range columns return an
    /// empty string.
    fn cell(&self, col: usize) -> Cow<'_, str>;
}

/// Callback that draws expanding content for the active row.
///
/// It receives the active row's window and returns the number of extra rows the
/// content occupies. [`draw_table`] offsets the rows below the active one by
/// that amount, reproducing upstream's `active_content_fn`.
///
/// The window is passed by mutable reference so the callback can grow it (set
/// `height`) before drawing children into it, exactly as upstream mutated the
/// window's `height` through the `*Window` pointer it received. The active
/// row's own cells are drawn afterwards at the window's top row.
pub type ActiveContentFn = Box<dyn for<'a, 's> FnMut(&'a mut Window<'s>) -> u16>;

/// How column widths are computed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WidthStyle {
    /// Size columns so they fill (most of) the table width.
    #[default]
    DynamicFill,
    /// Size each column to its header length plus twice this padding.
    DynamicHeaderLen(u16),
    /// Give every column the same fixed width.
    StaticAll(u16),
    /// Give each column an individual fixed width, indexed by display column.
    StaticIndividual(Vec<u16>),
}

/// Which columns of the row type to display, and in what order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ColumnIndexes {
    /// All columns, in declaration order.
    #[default]
    All,
    /// The columns at these field indexes, in this order.
    ByIdx(Vec<usize>),
}

/// Where the header text comes from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HeaderNames {
    /// Use the row type's field-derived headers.
    #[default]
    FieldNames,
    /// Use these custom headers, in display-column order.
    Custom(Vec<String>),
}

/// Horizontal text alignment within a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HorizontalAlignment {
    #[default]
    Left,
    Center,
}

/// Per-column horizontal alignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnAlignment {
    /// The same alignment for every column.
    All(HorizontalAlignment),
    /// An alignment per display column.
    ByIdx(Vec<HorizontalAlignment>),
}

impl Default for ColumnAlignment {
    fn default() -> Self {
        ColumnAlignment::All(HorizontalAlignment::Left)
    }
}

/// Mutable table state carried across draws.
///
/// The application owns one of these per table, mutates `row`/`col`/`sel_rows`
/// in response to input, and passes it to [`draw_table`], which updates the
/// scroll `start` and `active_y_off` as it lays the table out.
pub struct TableContext {
    /// Active (highlighted) row index into the data.
    pub row: u16,
    /// Active column index, used to highlight a header.
    pub col: u16,
    /// Index of the first visible row. Updated by [`draw_table`].
    pub start: u16,
    /// Rows drawn with the selected colors.
    pub sel_rows: Option<Vec<u16>>,

    /// Whether the table is the focused widget. Only an active table
    /// highlights its active row and column header.
    pub active: bool,
    /// Optional callback that vertically expands the active row.
    pub active_content_fn: Option<ActiveContentFn>,
    /// Extra rows the active content occupies. Managed by [`draw_table`].
    pub active_y_off: u16,

    pub selected_bg: Color,
    pub selected_fg: Color,
    pub active_bg: Color,
    pub active_fg: Color,
    pub hdr_bg_1: Color,
    pub hdr_bg_2: Color,
    pub row_bg_1: Color,
    pub row_bg_2: Color,

    /// Vertical offset of the whole table within the parent window.
    pub y_off: u16,
    /// Horizontal offset of text within each cell.
    pub cell_x_off: u16,

    pub col_width: WidthStyle,
    pub header_names: HeaderNames,
    pub col_indexes: ColumnIndexes,
    pub header_align: HorizontalAlignment,
    pub col_align: ColumnAlignment,

    /// Draw a left border between header cells.
    pub header_borders: bool,
    /// Draw a left border between data cells.
    pub col_borders: bool,
}

impl Default for TableContext {
    fn default() -> Self {
        // The header/row backgrounds match upstream's defaults. Upstream had no
        // default for `selected_bg`/`active_bg` (they were required fields), so
        // we pick the demo's blues, which stand out against the row grays.
        TableContext {
            row: 0,
            col: 0,
            start: 0,
            sel_rows: None,
            active: false,
            active_content_fn: None,
            active_y_off: 0,
            selected_bg: Color::Rgb([32, 64, 255]),
            selected_fg: Color::Default,
            active_bg: Color::Rgb([64, 128, 255]),
            active_fg: Color::Default,
            hdr_bg_1: Color::Rgb([64, 64, 64]),
            hdr_bg_2: Color::Rgb([8, 8, 24]),
            row_bg_1: Color::Rgb([32, 32, 32]),
            row_bg_2: Color::Rgb([8, 8, 8]),
            y_off: 0,
            cell_x_off: 1,
            col_width: WidthStyle::DynamicFill,
            header_names: HeaderNames::FieldNames,
            col_indexes: ColumnIndexes::All,
            header_align: HorizontalAlignment::Center,
            col_align: ColumnAlignment::All(HorizontalAlignment::Left),
            header_borders: false,
            col_borders: false,
        }
    }
}

/// Errors from table layout.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TableError {
    /// A static width style did not provide a width for column `col`.
    #[error("no static width provided for column {col}")]
    NotEnoughStaticWidths { col: usize },
}

/// Draws `data` as a table onto `win`, using and updating `ctx`.
///
/// Reproduces upstream `drawTable`: a header row, the visible data rows with
/// selected/active/alternating backgrounds, the scroll-window math that keeps
/// the active row on screen, and the optional expanding active row. Column
/// widths come from `ctx.col_width` via [`calc_col_width`].
///
/// Unlike upstream, `draw_table` never truncates cell text with an ellipsis
/// (that path was gated on an optional allocator). The window clips overlong
/// text instead, matching upstream's behavior when no allocator is passed.
pub fn draw_table<R: TableRow>(
    win: &Window<'_>,
    data: &[R],
    ctx: &mut TableContext,
) -> Result<(), crate::error::Error> {
    // Snapshot the immutable layout config into locals so the borrow of `ctx`
    // ends here and the row/scroll updates below can mutate `ctx` freely.
    let active = ctx.active;
    let selected_bg = ctx.selected_bg;
    let selected_fg = ctx.selected_fg;
    let active_bg = ctx.active_bg;
    let active_fg = ctx.active_fg;
    let hdr_bg_1 = ctx.hdr_bg_1;
    let hdr_bg_2 = ctx.hdr_bg_2;
    let row_bg_1 = ctx.row_bg_1;
    let row_bg_2 = ctx.row_bg_2;
    let y_off = ctx.y_off;
    let cell_x_off = ctx.cell_x_off;
    let header_align = ctx.header_align;
    let header_borders = ctx.header_borders;
    let col_borders = ctx.col_borders;
    let width_style = ctx.col_width.clone();
    let col_align = ctx.col_align.clone();
    let sel_rows = ctx.sel_rows.clone();

    let all_headers = R::headers();
    let field_indexes: Vec<usize> = match &ctx.col_indexes {
        ColumnIndexes::All => (0..R::column_count()).collect(),
        ColumnIndexes::ByIdx(v) => v.clone(),
    };
    // Headers are keyed by display column, so both custom names and field names
    // are reordered to match `field_indexes`. Out-of-range field indexes render
    // an empty header (upstream would read undefined memory here).
    let headers: Vec<Cow<'static, str>> = match &ctx.header_names {
        HeaderNames::FieldNames => field_indexes
            .iter()
            .map(|&f| all_headers.get(f).cloned().unwrap_or(Cow::Borrowed("")))
            .collect(),
        HeaderNames::Custom(names) => names.iter().map(|n| Cow::Owned(n.clone())).collect(),
    };
    let num_cols = headers.len();

    let table_win = win.child(ChildOptions {
        y_off: i32::from(y_off),
        width: Some(win.width),
        height: Some(win.height),
        ..ChildOptions::default()
    });

    // Header row.
    if num_cols > 0 {
        let last = u16::try_from(num_cols - 1).unwrap_or(u16::MAX);
        if ctx.col > last {
            ctx.col = last;
        }
    }
    let mut col_start: u16 = 0;
    for (pos, hdr_txt) in headers.iter().enumerate() {
        let col_width = calc_col_width(pos, &headers, &width_style, &table_win)?;
        let pos_u16 = u16::try_from(pos).unwrap_or(u16::MAX);
        let (hdr_fg, hdr_bg) = if active && pos_u16 == ctx.col {
            (active_fg, active_bg)
        } else if pos % 2 == 0 {
            (Color::Default, hdr_bg_1)
        } else {
            (Color::Default, hdr_bg_2)
        };
        let hdr_win = table_win.child(ChildOptions {
            x_off: i32::from(col_start),
            y_off: 0,
            width: Some(col_width),
            height: Some(1),
            border: border_left(header_borders && pos > 0),
        });
        hdr_win.fill(bg_cell(hdr_bg));
        let target = match header_align {
            HorizontalAlignment::Left => hdr_win,
            HorizontalAlignment::Center => {
                let text_w = table_win.gwidth(hdr_txt);
                alignment::center(
                    hdr_win,
                    col_width.saturating_sub(1).min(text_w.saturating_add(1)),
                    1,
                )
            }
        };
        let seg = Segment {
            text: hdr_txt.to_string(),
            style: Style {
                fg: hdr_fg,
                bg: hdr_bg,
                bold: true,
                ul_style: if pos_u16 == ctx.col {
                    Underline::Single
                } else {
                    Underline::Dotted
                },
                ..Style::default()
            },
            ..Segment::default()
        };
        target.print(
            &[seg],
            PrintOptions {
                wrap: Wrap::Word,
                ..PrintOptions::default()
            },
        );
        col_start = col_start.saturating_add(col_width);
    }

    // Scroll-window math. Without an active-content callback there is no
    // expansion, so reset the offset before computing the visible window.
    if ctx.active_content_fn.is_none() {
        ctx.active_y_off = 0;
    }
    let len = data.len();
    let len_u16 = u16::try_from(len).unwrap_or(u16::MAX);
    let visible_rows = table_win.height.saturating_sub(1);
    let max_items = if len > usize::from(visible_rows) {
        visible_rows
    } else {
        len_u16
    };

    let mut end = ctx.start.saturating_add(max_items);
    if ctx.row.saturating_add(ctx.active_y_off) >= win.height.saturating_sub(2) {
        end = end.saturating_sub(ctx.active_y_off);
    }
    if usize::from(end) > len {
        end = len_u16;
    }
    ctx.start = if ctx.row == 0 {
        0
    } else if ctx.row < ctx.start {
        // Upstream's `start - (start - row)` simplifies to `row`: scroll up so
        // the active row becomes the first visible one.
        ctx.row
    } else {
        if usize::from(ctx.row) >= len.saturating_sub(1) {
            ctx.row = len_u16.saturating_sub(1);
        }
        if ctx.row >= end {
            ctx.start
                .saturating_add(ctx.row.saturating_sub(end).saturating_add(1))
        } else {
            ctx.start
        }
    };
    end = ctx.start.saturating_add(max_items);
    if ctx.row.saturating_add(ctx.active_y_off) >= win.height.saturating_sub(2) {
        end = end.saturating_sub(ctx.active_y_off);
    }
    if usize::from(end) > len {
        end = len_u16;
    }
    ctx.start = ctx.start.min(end);
    ctx.active_y_off = 0;

    // Data rows.
    let start = ctx.start;
    let start_idx = usize::from(start).min(len);
    let end_idx = usize::from(end).min(len).max(start_idx);
    for (row, item) in data[start_idx..end_idx].iter().enumerate() {
        let row_u16 = u16::try_from(row).unwrap_or(u16::MAX);
        let abs = start.saturating_add(row_u16);
        let (row_fg, row_bg) = if active && abs == ctx.row {
            (active_fg, active_bg)
        } else if sel_rows.as_ref().is_some_and(|rows| rows.contains(&abs)) {
            (selected_fg, selected_bg)
        } else if row % 2 == 0 {
            (Color::Default, row_bg_1)
        } else {
            (Color::Default, row_bg_2)
        };

        let mut row_win = table_win.child(ChildOptions {
            x_off: 0,
            y_off: i32::from(
                1u16.saturating_add(row_u16)
                    .saturating_add(ctx.active_y_off),
            ),
            width: Some(table_win.width),
            height: Some(1),
            ..ChildOptions::default()
        });

        // The active row's cells still draw at its top; the callback expands
        // below it and its returned height pushes the following rows down.
        if abs == ctx.row {
            let expanded = match ctx.active_content_fn.as_mut() {
                Some(content) => content(&mut row_win),
                None => 0,
            };
            ctx.active_y_off = expanded;
        }

        let mut col_start: u16 = 0;
        for (pos, &f_idx) in field_indexes.iter().enumerate() {
            let col_width = calc_col_width(pos, &headers, &width_style, &table_win)?;
            let item_txt = item.cell(f_idx);
            let item_win = row_win.child(ChildOptions {
                x_off: i32::from(col_start),
                y_off: 0,
                width: Some(col_width),
                height: Some(1),
                border: border_left(col_borders && pos > 0),
            });
            item_win.fill(bg_cell(row_bg));
            let align = match &col_align {
                ColumnAlignment::All(a) => *a,
                ColumnAlignment::ByIdx(v) => v.get(pos).copied().unwrap_or_default(),
            };
            let target = match align {
                HorizontalAlignment::Left => item_win,
                HorizontalAlignment::Center => {
                    let text_w = table_win.gwidth(&item_txt);
                    let center = alignment::center(
                        item_win,
                        col_width.saturating_sub(1).min(text_w.saturating_add(1)),
                        1,
                    );
                    center.fill(bg_cell(row_bg));
                    center
                }
            };
            let seg = Segment {
                text: item_txt.into_owned(),
                style: Style {
                    fg: row_fg,
                    bg: row_bg,
                    ..Style::default()
                },
                ..Segment::default()
            };
            target.print(
                &[seg],
                PrintOptions {
                    wrap: Wrap::Word,
                    col_offset: cell_x_off,
                    ..PrintOptions::default()
                },
            );
            col_start = col_start.saturating_add(col_width);
        }
    }

    Ok(())
}

/// A left-border option when `on`, otherwise no border.
fn border_left(on: bool) -> BorderOptions {
    BorderOptions {
        location: if on {
            BorderWhere::Left
        } else {
            BorderWhere::None
        },
        ..BorderOptions::default()
    }
}

/// A blank cell carrying only a background color, for filling a column window.
fn bg_cell(bg: Color) -> Cell {
    Cell {
        style: Style {
            bg,
            ..Style::default()
        },
        ..Cell::default()
    }
}

/// Computes the width of display column `col` for the given width style.
///
/// `headers` is the display-ordered header list; `col` indexes into it. Returns
/// [`TableError::NotEnoughStaticWidths`] when a static/dynamic-header style has
/// no entry for `col`.
pub fn calc_col_width(
    col: usize,
    headers: &[Cow<'_, str>],
    style: &WidthStyle,
    table_win: &Window<'_>,
) -> Result<u16, TableError> {
    match style {
        WidthStyle::DynamicFill => {
            let n = headers.len();
            if n == 0 {
                return Ok(0);
            }
            let mut cw = u16::try_from(usize::from(table_win.width) / n).unwrap_or(u16::MAX);
            if cw % 2 != 0 {
                cw = cw.saturating_add(1);
            }
            // Grow the width until the columns nearly fill the table.
            while usize::from(cw) * n < usize::from(table_win.width.saturating_sub(1)) {
                cw = cw.saturating_add(1);
            }
            Ok(cw)
        }
        WidthStyle::DynamicHeaderLen(pad) => {
            let header = headers
                .get(col)
                .ok_or(TableError::NotEnoughStaticWidths { col })?;
            let len = u16::try_from(header.len()).unwrap_or(u16::MAX);
            Ok(len.saturating_add(pad.saturating_mul(2)))
        }
        WidthStyle::StaticAll(w) => Ok(*w),
        WidthStyle::StaticIndividual(widths) => widths
            .get(col)
            .copied()
            .ok_or(TableError::NotEnoughStaticWidths { col }),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::screen::Screen;

    /// A hand-written [`TableRow`] mirroring what `#[derive(TableRow)]`
    /// generates. The derive itself is exercised from `tests/table_derive.rs`,
    /// where `vaxis` is an external crate and the derive's `::vaxis::` paths
    /// resolve. (`extern crate self as vaxis` is not usable here because the
    /// crate already has a `vaxis` runtime module.)
    struct Person {
        name: String,
        age: u32,
        role: &'static str,
        email: Option<String>,
    }

    impl TableRow for Person {
        fn headers() -> Vec<Cow<'static, str>> {
            vec![
                Cow::Borrowed("name"),
                Cow::Borrowed("age"),
                Cow::Borrowed("role"),
                Cow::Borrowed("email"),
            ]
        }

        fn column_count() -> usize {
            4
        }

        fn cell(&self, col: usize) -> Cow<'_, str> {
            match col {
                0 => self.name.to_cell(),
                1 => self.age.to_cell(),
                2 => self.role.to_cell(),
                3 => match &self.email {
                    Some(v) => v.to_cell(),
                    None => Cow::Borrowed("-"),
                },
                _ => Cow::Borrowed(""),
            }
        }
    }

    fn person(name: &str, age: u32, role: &'static str, email: Option<&str>) -> Person {
        Person {
            name: name.to_string(),
            age,
            role,
            email: email.map(str::to_string),
        }
    }

    fn screen(cols: u16, rows: u16) -> RefCell<Screen> {
        RefCell::new(Screen::new(crate::Winsize {
            rows,
            cols,
            x_pixel: 0,
            y_pixel: 0,
        }))
    }

    fn root(screen: &RefCell<Screen>, cols: u16, rows: u16) -> Window<'_> {
        Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: cols,
            height: rows,
            screen,
        }
    }

    fn grapheme_at(screen: &RefCell<Screen>, col: u16, row: u16) -> String {
        screen
            .borrow()
            .read_cell(col, row)
            .map(|c| c.char.grapheme().to_string())
            .unwrap_or_default()
    }

    fn bg_at(screen: &RefCell<Screen>, col: u16, row: u16) -> Color {
        screen
            .borrow()
            .read_cell(col, row)
            .map(|c| c.style.bg)
            .unwrap_or(Color::Default)
    }

    /// A left-aligned, fixed-width context that makes cell positions
    /// deterministic for the placement assertions.
    fn grid_ctx() -> TableContext {
        TableContext {
            col_width: WidthStyle::StaticAll(10),
            header_align: HorizontalAlignment::Left,
            col_align: ColumnAlignment::All(HorizontalAlignment::Left),
            cell_x_off: 0,
            ..TableContext::default()
        }
    }

    fn people() -> Vec<Person> {
        vec![
            person("Ada", 30, "Admin", Some("ada@x.io")),
            person("Bo", 5, "User", None),
            person("Cy", 42, "User", Some("cy@x.io")),
        ]
    }

    #[test]
    fn table_cell_formats_via_display() {
        assert_eq!(String::from("hi").to_cell().as_ref(), "hi");
        assert_eq!(42u32.to_cell().as_ref(), "42");
        assert_eq!((-3i32).to_cell().as_ref(), "-3");
    }

    #[test]
    fn draw_places_headers_and_cells() {
        let screen = screen(40, 10);
        let win = root(&screen, 40, 10);
        let mut ctx = grid_ctx();
        draw_table(&win, &people(), &mut ctx).unwrap();

        // Header row (row 0): "name" at col 0, "age" at col 10.
        assert_eq!(grapheme_at(&screen, 0, 0), "n");
        assert_eq!(grapheme_at(&screen, 10, 0), "a");
        // First data row (row 1): name "Ada", age "30" at col 10.
        assert_eq!(grapheme_at(&screen, 0, 1), "A");
        assert_eq!(grapheme_at(&screen, 1, 1), "d");
        assert_eq!(grapheme_at(&screen, 10, 1), "3");
        // Third column of the second data row: role "User".
        assert_eq!(grapheme_at(&screen, 20, 2), "U");
    }

    #[test]
    fn selecting_a_row_applies_selected_background() {
        let screen = screen(40, 10);
        let win = root(&screen, 40, 10);
        let mut ctx = grid_ctx();
        ctx.selected_bg = Color::Rgb([1, 2, 3]);
        ctx.sel_rows = Some(vec![0]);
        draw_table(&win, &people(), &mut ctx).unwrap();

        // Row 0 draws with the selected background; row 1 does not.
        assert_eq!(bg_at(&screen, 0, 1), Color::Rgb([1, 2, 3]));
        assert_ne!(bg_at(&screen, 0, 2), Color::Rgb([1, 2, 3]));
    }

    #[test]
    fn active_row_applies_active_background() {
        let screen = screen(40, 10);
        let win = root(&screen, 40, 10);
        let mut ctx = grid_ctx();
        ctx.active = true;
        ctx.active_bg = Color::Rgb([9, 8, 7]);
        ctx.row = 1;
        draw_table(&win, &people(), &mut ctx).unwrap();

        // Row 1 is active (screen row 2), row 0 is not.
        assert_eq!(bg_at(&screen, 0, 2), Color::Rgb([9, 8, 7]));
        assert_ne!(bg_at(&screen, 0, 1), Color::Rgb([9, 8, 7]));
    }

    #[test]
    fn alternating_row_backgrounds() {
        let screen = screen(40, 10);
        let win = root(&screen, 40, 10);
        let mut ctx = grid_ctx();
        ctx.row_bg_1 = Color::Rgb([10, 10, 10]);
        ctx.row_bg_2 = Color::Rgb([20, 20, 20]);
        draw_table(&win, &people(), &mut ctx).unwrap();

        assert_eq!(bg_at(&screen, 0, 1), Color::Rgb([10, 10, 10]));
        assert_eq!(bg_at(&screen, 0, 2), Color::Rgb([20, 20, 20]));
        assert_eq!(bg_at(&screen, 0, 3), Color::Rgb([10, 10, 10]));
    }

    #[test]
    fn active_content_fn_expands_active_row() {
        let screen = screen(40, 10);
        let win = root(&screen, 40, 10);
        let mut ctx = grid_ctx();
        ctx.row = 0;
        ctx.active_content_fn = Some(Box::new(|w: &mut Window<'_>| {
            // Grow the row window, then draw one extra content row beneath it.
            w.height = 2;
            let child = w.child(ChildOptions {
                y_off: 1,
                width: Some(w.width),
                height: Some(1),
                ..ChildOptions::default()
            });
            child.fill(bg_cell(Color::Rgb([100, 100, 100])));
            1
        }));
        draw_table(&win, &people(), &mut ctx).unwrap();

        // The callback ran (its content row is filled) and the row below the
        // active one was pushed down by the returned height.
        assert_eq!(bg_at(&screen, 0, 2), Color::Rgb([100, 100, 100]));
        // The returned height is retained so next frame's scroll math can
        // account for the expansion; it is reset at the start of each draw.
        assert_eq!(ctx.active_y_off, 1);
    }

    #[test]
    fn custom_headers_and_column_reorder() {
        let screen = screen(40, 10);
        let win = root(&screen, 40, 10);
        let mut ctx = grid_ctx();
        // Reorder to role, name. Custom headers align to display columns.
        ctx.col_indexes = ColumnIndexes::ByIdx(vec![2, 0]);
        ctx.header_names = HeaderNames::Custom(vec!["Role".to_string(), "Who".to_string()]);
        draw_table(&win, &people(), &mut ctx).unwrap();

        assert_eq!(grapheme_at(&screen, 0, 0), "R");
        assert_eq!(grapheme_at(&screen, 10, 0), "W");
        // Column 0 now shows the role, column 1 the name.
        assert_eq!(grapheme_at(&screen, 0, 1), "A"); // "Admin"
        assert_eq!(grapheme_at(&screen, 10, 1), "A"); // "Ada"
    }

    #[test]
    fn calc_col_width_dynamic_fill() {
        let screen = screen(40, 4);
        let win = root(&screen, 40, 4);
        let headers = [
            Cow::Borrowed("Full Name"),
            Cow::Borrowed("age"),
            Cow::Borrowed("role"),
            Cow::Borrowed("email"),
        ];
        // 40 / 4 == 10, already even, and 10 * 4 already fills the width.
        assert_eq!(
            calc_col_width(0, &headers, &WidthStyle::DynamicFill, &win).unwrap(),
            10
        );
    }

    #[test]
    fn calc_col_width_dynamic_header_len() {
        let screen = screen(40, 4);
        let win = root(&screen, 40, 4);
        let headers = [Cow::Borrowed("Full Name"), Cow::Borrowed("age")];
        // "Full Name" is 9 bytes, plus 2 * pad.
        assert_eq!(
            calc_col_width(0, &headers, &WidthStyle::DynamicHeaderLen(3), &win).unwrap(),
            15
        );
    }

    #[test]
    fn calc_col_width_static_variants() {
        let screen = screen(40, 4);
        let win = root(&screen, 40, 4);
        let headers = [Cow::Borrowed("a"), Cow::Borrowed("b"), Cow::Borrowed("c")];
        assert_eq!(
            calc_col_width(1, &headers, &WidthStyle::StaticAll(12), &win).unwrap(),
            12
        );
        assert_eq!(
            calc_col_width(
                1,
                &headers,
                &WidthStyle::StaticIndividual(vec![5, 7, 9]),
                &win
            )
            .unwrap(),
            7
        );
    }

    #[test]
    fn calc_col_width_not_enough_static_widths() {
        let screen = screen(40, 4);
        let win = root(&screen, 40, 4);
        let headers = [Cow::Borrowed("a"), Cow::Borrowed("b")];
        assert_eq!(
            calc_col_width(
                5,
                &headers,
                &WidthStyle::StaticIndividual(vec![1, 2, 3]),
                &win
            ),
            Err(TableError::NotEnoughStaticWidths { col: 5 })
        );
    }
}
