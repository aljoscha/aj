//! [`FlexRow`]: lays children out left to right, distributing leftover width by
//! flex factor.
//!
//! NOTE: See [`FlexColumn`](crate::vxfw::FlexColumn) for the full description of
//! the deliberate asymmetry between the two (D8), reproduced faithfully rather
//! than unified. In short, `FlexRow` measures only `flex==0` children in pass
//! 1, gives flex children just the flex share (no inherent term), and saturates
//! its subtractions where `FlexColumn` uses plain arithmetic.

use crate::vxfw::{
    DrawContext, FlexItem, MaxSize, RelativePoint, Size, SubSurface, Surface, Widget, draw_widget,
};

/// Stacks its children horizontally, giving each `flex==0` child its inherent
/// width and proportioning the rest of the row among the flexible children.
///
/// Requires a bounded max on both axes (asserted via [`MaxSize::size`]). The
/// result fills the max width and is as tall as its tallest child.
pub struct FlexRow {
    pub children: Vec<FlexItem>,
}

impl Widget for FlexRow {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let max = ctx.max.size();
        if self.children.is_empty() {
            return Surface::with_size(ctx.min);
        }

        // Pass 1: measure only the inflexible (flex==0) children under an
        // unbounded width and the row's max height.
        //
        // NOTE: unlike FlexColumn, flex children are not measured, so their
        // `size_list` entries stay 0 and are never read in pass 2.
        let layout_ctx = ctx.with_constraints(
            Size {
                width: 0,
                height: 0,
            },
            MaxSize {
                width: None,
                height: ctx.max.height,
            },
        );
        let mut size_list: Vec<u16> = vec![0; self.children.len()];
        let mut first_pass_width: u16 = 0;
        let mut total_flex: u16 = 0;
        for (idx, child) in self.children.iter().enumerate() {
            if child.flex == 0 {
                let surf = draw_widget(&child.widget, &layout_ctx);
                first_pass_width += surf.size.width;
                size_list[idx] = surf.size.width;
            }
            total_flex += u16::from(child.flex);
        }

        // Pass 2: redraw with distributed widths.
        let mut children: Vec<SubSurface> = Vec::new();
        let mut second_pass_width: u16 = 0;
        let mut max_height: u16 = 0;
        // NOTE: saturating subtraction, where FlexColumn uses plain.
        let remaining_space = max.width.saturating_sub(first_pass_width);
        let len = self.children.len();
        for (idx, child) in self.children.iter().enumerate() {
            let child_width = if child.flex == 0 {
                size_list[idx]
            } else if idx == len - 1 {
                // The last child takes whatever width is left, saturating.
                max.width.saturating_sub(second_pass_width)
            } else {
                // NOTE: just the flex share, no inherent term (unlike FlexColumn).
                (remaining_space * u16::from(child.flex)) / total_flex
            };

            let child_ctx = ctx.with_constraints(
                Size {
                    width: child_width,
                    height: 0,
                },
                MaxSize {
                    width: Some(child_width),
                    height: Some(max.height),
                },
            );
            let surf = draw_widget(&child.widget, &child_ctx);
            let surf_size = surf.size;
            children.push(SubSurface {
                origin: RelativePoint {
                    col: i32::from(second_pass_width),
                    row: 0,
                },
                surface: surf,
                z_index: 0,
            });
            max_height = max_height.max(surf_size.height);
            second_pass_width += surf_size.width;
        }

        Surface {
            size: Size {
                width: second_pass_width,
                height: max_height,
            },
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::gwidth;
    use crate::vxfw::{Text, WidgetRef, draw_widget};

    #[test]
    fn flex_row() {
        // Each Text is height=1, width=3, except jklmno which is height=2.
        let abc: WidgetRef = Rc::new(RefCell::new(Text::new("abc")));
        let def: WidgetRef = Rc::new(RefCell::new(Text::new("def")));
        let ghi: WidgetRef = Rc::new(RefCell::new(Text::new("ghi")));
        let jklmno: WidgetRef = Rc::new(RefCell::new(Text::new("jkl\nmno")));

        let flex: WidgetRef = Rc::new(RefCell::new(FlexRow {
            children: vec![
                FlexItem::init(Rc::clone(&abc), 0),
                FlexItem::init(Rc::clone(&def), 1),
                FlexItem::init(Rc::clone(&ghi), 1),
                FlexItem::init(Rc::clone(&jklmno), 1),
            ],
        }));

        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(16),
                height: Some(16),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        };

        let surface = draw_widget(&flex, &ctx);
        // FlexRow expands to the max width and its tallest child.
        assert_eq!(surface.size.width, 16);
        assert_eq!(surface.size.height, 2);
        assert_eq!(surface.children.len(), 4);

        // Track the column to confirm origins.
        let mut col: i32 = 0;
        // First child has flex=0, so it keeps its inherent width.
        assert_eq!(surface.children[0].surface.size.width, 3);
        assert_eq!(surface.children[0].origin.col, col);
        col += i32::from(surface.children[0].surface.size.width);

        // 4 children into 16 cols: all are 3 wide, total 12. The first is
        // flex=0 (3 cols), the remaining 4 cols distribute over the other 3
        // (4 / 3 = 1 extra each, last gets the remainder).
        assert_eq!(surface.children[1].surface.size.width, 1 + 3);
        assert_eq!(surface.children[1].origin.col, col);
        col += i32::from(surface.children[1].surface.size.width);

        assert_eq!(surface.children[2].surface.size.width, 1 + 3);
        assert_eq!(surface.children[2].origin.col, col);
        col += i32::from(surface.children[2].surface.size.width);

        assert_eq!(surface.children[3].surface.size.width, 1 + 3 + 1);
        assert_eq!(surface.children[3].origin.col, col);
    }
}
