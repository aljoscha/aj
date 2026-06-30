//! [`FlexColumn`]: lays children out top to bottom, distributing leftover
//! height by flex factor.
//!
//! NOTE: [`FlexColumn`] and [`FlexRow`](crate::vxfw::FlexRow) distribute flex
//! space differently, and the asymmetry is deliberate (D8): it is reproduced
//! faithfully rather than unified. The differences:
//!
//! - Measurement set: `FlexColumn` measures every child in pass 1 (to learn
//!   each inherent height). `FlexRow` measures only the inflexible (`flex==0`)
//!   children.
//! - Flex formula: a `FlexColumn` flex child gets `inherent + share`, where
//!   `share = remaining_space * flex / total_flex`. A `FlexRow` flex child gets
//!   just `share` (no inherent term, since flex children are never measured).
//! - Leftover handling: `FlexColumn` subtracts with plain (non-saturating)
//!   arithmetic for both `remaining_space` and the last child's remainder.
//!   `FlexRow` saturates both.

use crate::vxfw::{
    DrawContext, FlexItem, MaxSize, RelativePoint, Size, SubSurface, Surface, Widget, draw_widget,
};

/// Stacks its children vertically, giving each `flex==0` child its inherent
/// height and proportioning the rest of the column among the flexible children.
///
/// Requires a bounded max on both axes (asserted via [`MaxSize::size`]). The
/// result fills the max height and is as wide as its widest child.
pub struct FlexColumn {
    pub children: Vec<FlexItem>,
}

impl Widget for FlexColumn {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let max = ctx.max.size();
        if self.children.is_empty() {
            return Surface::with_size(ctx.min);
        }

        // Pass 1: measure every child's inherent height under an unbounded
        // height and the column's max width. We draw into surfaces we drop
        // after reading their sizes (upstream uses a nested arena for this).
        let layout_ctx = ctx.with_constraints(
            Size {
                width: 0,
                height: 0,
            },
            MaxSize {
                width: ctx.max.width,
                height: None,
            },
        );
        let mut size_list: Vec<u16> = Vec::with_capacity(self.children.len());
        let mut first_pass_height: u16 = 0;
        let mut total_flex: u16 = 0;
        for child in &self.children {
            let surf = draw_widget(&child.widget, &layout_ctx);
            first_pass_height += surf.size.height;
            total_flex += u16::from(child.flex);
            size_list.push(surf.size.height);
        }

        // Pass 2: redraw with distributed heights.
        let mut children: Vec<SubSurface> = Vec::new();
        let mut second_pass_height: u16 = 0;
        let mut max_width: u16 = 0;
        // NOTE: plain subtraction, matching upstream (FlexRow saturates here).
        let remaining_space = max.height - first_pass_height;
        let len = self.children.len();
        for (idx, child) in self.children.iter().enumerate() {
            let inherent_height = size_list[idx];
            let child_height = if child.flex == 0 {
                inherent_height
            } else if idx + 1 == len {
                // The last child takes whatever height is left. Plain
                // subtraction, matching upstream.
                max.height - second_pass_height
            } else {
                // NOTE: inherent height plus the flex share, where FlexRow uses
                // the share alone.
                inherent_height + (remaining_space * u16::from(child.flex)) / total_flex
            };

            let child_ctx = ctx.with_constraints(
                Size {
                    width: 0,
                    height: child_height,
                },
                MaxSize {
                    width: Some(max.width),
                    height: Some(child_height),
                },
            );
            let surf = draw_widget(&child.widget, &child_ctx);
            let surf_size = surf.size;
            children.push(SubSurface {
                origin: RelativePoint {
                    col: 0,
                    row: i32::from(second_pass_height),
                },
                surface: surf,
                z_index: 0,
            });
            max_width = max_width.max(surf_size.width);
            second_pass_height += surf_size.height;
        }

        Surface {
            size: Size {
                width: max_width,
                height: second_pass_height,
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
    fn flex_column() {
        // Each Text is height=1, width=3, except jklmno which is height=2.
        let abc: WidgetRef = Rc::new(RefCell::new(Text::new("abc")));
        let def: WidgetRef = Rc::new(RefCell::new(Text::new("def")));
        let ghi: WidgetRef = Rc::new(RefCell::new(Text::new("ghi")));
        let jklmno: WidgetRef = Rc::new(RefCell::new(Text::new("jkl\nmno")));

        let flex: WidgetRef = Rc::new(RefCell::new(FlexColumn {
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
        // FlexColumn expands to the max height and its widest child.
        assert_eq!(surface.size.height, 16);
        assert_eq!(surface.size.width, 3);
        assert_eq!(surface.children.len(), 4);

        // Track the row to confirm origins.
        let mut row: i32 = 0;
        // First child has flex=0, so it keeps its inherent height.
        assert_eq!(surface.children[0].surface.size.height, 1);
        assert_eq!(surface.children[0].origin.row, row);
        row += i32::from(surface.children[0].surface.size.height);

        // 4 children into 16 rows: 3 are 1 tall, 1 is 2 tall, total 5. The
        // first child is flex=0 (1 row), the remaining 11 rows distribute
        // evenly over the other 3 (11 / 3 = 3 extra each, last gets remainder).
        assert_eq!(surface.children[1].surface.size.height, 1 + 3);
        assert_eq!(surface.children[1].origin.row, row);
        row += i32::from(surface.children[1].surface.size.height);

        assert_eq!(surface.children[2].surface.size.height, 1 + 3);
        assert_eq!(surface.children[2].origin.row, row);
        row += i32::from(surface.children[2].surface.size.height);

        assert_eq!(surface.children[3].surface.size.height, 2 + 3 + 2);
        assert_eq!(surface.children[3].origin.row, row);
    }
}
