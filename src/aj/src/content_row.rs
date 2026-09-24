//! Read-only report rows whose columns wrap independently.

use std::{cell::RefCell, rc::Rc};

use vaxis::cell::Segment;
use vaxis::vxfw::{
    DrawContext, MaxSize, RelativePoint, RichText, Size, SubSurface, Surface, Widget, WidgetRef,
};

#[derive(Clone, Debug)]
pub(crate) enum Row {
    Text(Vec<Segment>),
    /// Preferred widths for all but the final, flexible column. Builders use
    /// the same widths across a report to keep its column origins aligned.
    /// The last cell spans any remaining columns when fewer cells are supplied.
    Columns {
        indent: u16,
        widths: Vec<usize>,
        cells: Vec<Vec<Segment>>,
    },
}

impl Row {
    pub(crate) fn columns(indent: u16, widths: Vec<usize>, cells: Vec<Vec<Segment>>) -> Self {
        assert!(!cells.is_empty() && cells.len() <= widths.len() + 1);
        Self::Columns {
            indent,
            widths,
            cells,
        }
    }

    #[cfg(test)]
    pub(crate) fn spans(&self) -> impl Iterator<Item = &Segment> {
        let cells = match self {
            Self::Text(text) => std::slice::from_ref(text),
            Self::Columns { cells, .. } => cells.as_slice(),
        };
        cells.iter().flatten()
    }
}

pub(crate) fn row_widgets(rows: &[Row]) -> Vec<WidgetRef> {
    rows.iter()
        .map(|row| {
            let widget: WidgetRef = Rc::new(RefCell::new(row.clone()));
            widget
        })
        .collect()
}

impl Widget for Row {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let Self::Columns {
            indent,
            widths,
            cells,
        } = self
        else {
            let Self::Text(text) = self else {
                unreachable!()
            };
            return RichText::new(text.clone()).draw(ctx);
        };
        let width = ctx.max.width.expect("report rows require bounded width");
        if width == 0 {
            return Surface::with_size(Size { width, height: 0 });
        }
        let count = u16::try_from(widths.len() + 1).expect("report column count fits u16");
        let inset = (*indent).min(width.saturating_sub(count));
        let available = width - inset;
        // At extremely narrow widths stack cells without losing content. In
        // normal layouts, shrink wide labels before starving the final value.
        let stacked = available < count.saturating_mul(2).saturating_sub(1);
        let gap = if stacked || count == 1 {
            0
        } else {
            2.min((available - count) / (count - 1))
        };
        let mut remaining = available.saturating_sub(gap * (count - 1));
        let mut col = inset;
        let mut height: u16 = 0;
        let mut children = Vec::new();
        for (index, cell) in cells.iter().enumerate() {
            let left = count - u16::try_from(index).expect("column index fits");
            let column_width = if stacked {
                available
            } else if index + 1 == cells.len() {
                remaining + gap * (left - 1)
            } else {
                let preferred = u16::try_from(widths[index]).unwrap_or(u16::MAX).max(1);
                let fair = (width.saturating_sub(col).saturating_sub(gap) / 2)
                    .min(remaining.saturating_sub(left - 1))
                    .max(1);
                preferred.min(fair)
            };
            let child_ctx = ctx.with_constraints(
                Size {
                    width: 0,
                    height: 0,
                },
                MaxSize {
                    width: Some(column_width),
                    height: ctx.max.height.map(|max| {
                        if stacked {
                            max.saturating_sub(height)
                        } else {
                            max
                        }
                    }),
                },
            );
            let child = RichText::new(cell.clone()).draw(&child_ctx);
            let child_height = child.size.height;
            children.push(SubSurface {
                origin: RelativePoint {
                    row: if stacked { i32::from(height) } else { 0 },
                    col: i32::from(col),
                },
                surface: child,
                z_index: 0,
            });
            if stacked {
                height = height.saturating_add(child_height);
            } else {
                height = height.max(child_height);
                remaining = remaining.saturating_sub(column_width);
                col = col.saturating_add(column_width).saturating_add(gap);
            }
        }
        Surface::with_children(
            Size {
                width,
                height: height.max(ctx.min.height),
            },
            children,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vaxis::cell::{Color, Style};

    fn cell(text: &str, color: u8) -> Vec<Segment> {
        vec![Segment {
            text: text.into(),
            style: Style {
                fg: Color::Index(color),
                ..Style::default()
            },
            ..Segment::default()
        }]
    }

    #[test]
    fn columns_preserve_all_text_and_styles_across_resizes() {
        let values = [
            "A-long-account-name",
            "97% used",
            "resets tomorrow afternoon",
        ];
        let mut row = Row::columns(
            2,
            vec![30, 8],
            values
                .iter()
                .enumerate()
                .map(|(index, text)| cell(text, u8::try_from(index + 1).unwrap()))
                .collect(),
        );
        for width in [80, 32, 14, 5, 2, 1, 32, 80] {
            let surface = row.draw(&crate::test_support::draw_ctx(width, None));
            let grid = crate::test_support::flatten(&surface);
            for (index, value) in values.iter().enumerate() {
                let color = Color::Index(u8::try_from(index + 1).unwrap());
                let visible: String = grid
                    .iter()
                    .flatten()
                    .filter(|cell| cell.style.fg == color)
                    .flat_map(|cell| cell.char.grapheme().chars())
                    .filter(|ch| !ch.is_whitespace())
                    .collect();
                assert_eq!(
                    visible,
                    value
                        .chars()
                        .filter(|ch| !ch.is_whitespace())
                        .collect::<String>(),
                    "width {width}"
                );
            }
            if width >= 14 {
                let starts: Vec<_> = (1..=3)
                    .map(|color| {
                        grid.iter()
                            .enumerate()
                            .flat_map(|(row, cells)| {
                                cells.iter().enumerate().filter_map(move |(col, cell)| {
                                    (cell.style.fg == Color::Index(color)
                                        && !cell.char.grapheme().trim().is_empty())
                                    .then_some((row, col))
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect();
                for index in 1..3 {
                    let start = starts[index].iter().map(|(_, col)| *col).min().unwrap();
                    assert!(starts[index - 1].iter().all(|(_, col)| *col < start));
                }
                if width == 32 {
                    assert!(starts[2].iter().any(|(row, _)| *row > 0), "fixture wraps");
                }
            }
        }
    }

    #[test]
    fn wide_and_combining_graphemes_survive_column_wrapping() {
        for method in [
            vaxis::gwidth::Method::Unicode,
            vaxis::gwidth::Method::Wcwidth,
        ] {
            for width in [12, 24, 80] {
                let values = ["個人用アカウント", "cafe\u{301} expires tomorrow"];
                let mut row =
                    Row::columns(0, vec![16], vec![cell(values[0], 1), cell(values[1], 2)]);
                let mut ctx = crate::test_support::draw_ctx(width, None);
                ctx.width_method = method;
                let surface = row.draw(&ctx);
                let grid = crate::test_support::flatten(&surface);
                for (index, text) in values.iter().enumerate() {
                    let color = Color::Index(u8::try_from(index + 1).unwrap());
                    let visible: String = grid
                        .iter()
                        .flatten()
                        .filter(|cell| cell.style.fg == color)
                        .flat_map(|cell| cell.char.grapheme().chars())
                        .filter(|ch| !ch.is_whitespace())
                        .collect();
                    assert_eq!(
                        visible,
                        text.chars()
                            .filter(|ch| !ch.is_whitespace())
                            .collect::<String>(),
                        "{width} {method:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn value_column_origin_matches_rows_with_and_without_a_reset_column() {
        for width in 1..=80 {
            let mut metric = Row::columns(
                2,
                vec![20, 8],
                vec![
                    cell("Window", 1),
                    cell("100%used", 2),
                    cell("resets soon", 3),
                ],
            );
            let mut detail = Row::columns(
                2,
                vec![20, 8],
                vec![cell("Credits", 1), cell("available", 2)],
            );
            let origin = |row: &mut Row| {
                let surface = row.draw(&crate::test_support::draw_ctx(width, None));
                crate::test_support::flatten(&surface)
                    .iter()
                    .find_map(|row| row.iter().position(|cell| cell.style.fg == Color::Index(2)))
                    .unwrap()
            };
            assert_eq!(origin(&mut metric), origin(&mut detail), "width {width}");
        }
    }
}
