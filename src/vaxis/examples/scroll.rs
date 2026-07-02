//! Port of upstream `examples/scroll.zig`: a [`ScrollBars`]-wrapped
//! [`ScrollView`](vaxis::vxfw::ScrollView) of custom `ModelRow` widgets, each
//! drawing a right-aligned index next to a soft-wrappable paragraph.
//!
//! # Custom widget
//!
//! `ModelRow` is an app widget: its [`draw`](Widget::draw) builds two child
//! [`Text`] surfaces (the index and the paragraph) and positions them. The rows
//! are stored once as `Rc<RefCell<ModelRow>>` and shared two ways: the
//! [`ScrollView`](vaxis::vxfw::ScrollView) builder holds [`WidgetRef`] clones to
//! lay them out, and the `ScrollModel` holds the concrete handles so Ctrl-W can
//! toggle every row's wrap flag.
//!
//! # Keys
//!
//! - Ctrl-C quits.
//! - Ctrl-W toggles soft wrapping and swaps the estimated content height
//!   between 800 and the real row count.
//! - Ctrl-E toggles the estimated content height between 800 and unset.
//! - Tab toggles the scroll-view cursor.
//! - Ctrl-V / Ctrl-H toggle the vertical / horizontal scroll bars.
//! - Shift-Tab toggles both bars.
//! - Everything else forwards to the scroll view (j/k, arrows, Ctrl-D/U, etc).

use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;

use vaxis::key::{Key, Modifiers};
use vaxis::tty::PosixTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    App, Builder, DrawContext, Event, EventContext, MaxSize, Options, RelativePoint, ScrollBars,
    ScrollView, Size, Source, SubSurface, Surface, Text, Widget, WidgetRef, draw_widget,
    to_widget_ref,
};

/// A single scroll row: a right-aligned index next to a paragraph.
struct ModelRow {
    text: Rc<str>,
    idx: usize,
    wrap_lines: bool,
}

impl Widget for ModelRow {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // The index, right-aligned in four columns.
        let idx_widget: WidgetRef = Rc::new(RefCell::new(Text::new(format!("{:>4}", self.idx))));
        let idx_surf = draw_widget(
            &idx_widget,
            // Constrain only the width; a row's index is always one line tall.
            &ctx.with_constraints(
                Size {
                    width: 1,
                    height: 1,
                },
                MaxSize {
                    width: Some(4),
                    height: Some(1),
                },
            ),
        );

        // The paragraph, shifted six columns right. We must subtract that offset
        // from the width constraint or the text would draw past the edge.
        let text_widget: WidgetRef = Rc::new(RefCell::new(Text {
            softwrap: self.wrap_lines,
            ..Text::new(self.text.as_ref())
        }));
        let text_max = if self.wrap_lines {
            MaxSize {
                width: Some(ctx.min.width.saturating_sub(6)),
                height: ctx.max.height,
            }
        } else {
            MaxSize {
                width: ctx.max.width.map(|w| w - 6),
                height: ctx.max.height,
            }
        };
        let text_surf = draw_widget(&text_widget, &ctx.with_constraints(ctx.min, text_max));

        Surface {
            size: Size {
                width: 6 + text_surf.size.width,
                height: idx_surf.size.height.max(text_surf.size.height),
            },
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children: vec![
                SubSurface {
                    origin: RelativePoint { row: 0, col: 0 },
                    surface: idx_surf,
                    z_index: 0,
                },
                SubSurface {
                    origin: RelativePoint { row: 0, col: 6 },
                    surface: text_surf,
                    z_index: 0,
                },
            ],
        }
    }
}

/// Lazily hands the scroll view a row by index.
struct RowSource {
    rows: Vec<WidgetRef>,
}

impl Builder for RowSource {
    fn item_at_idx(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
        self.rows.get(idx).cloned()
    }
}

/// The application state: the scroll bars and the shared row handles.
struct ScrollModel {
    scroll_bars: Rc<RefCell<ScrollBars>>,
    rows: Vec<Rc<RefCell<ModelRow>>>,
}

impl Widget for ScrollModel {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let max = ctx.max.size();
        let scroll_bars = to_widget_ref(Rc::clone(&self.scroll_bars));
        let surface = draw_widget(&scroll_bars, ctx);
        Surface {
            size: max,
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children: vec![SubSurface {
                origin: RelativePoint { row: 0, col: 0 },
                surface,
                z_index: 0,
            }],
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            return;
        };

        if key.matches(u32::from('c'), Modifiers::CTRL) {
            ctx.quit = true;
            return;
        }
        if key.matches(u32::from('w'), Modifiers::CTRL) {
            for row in &self.rows {
                let mut row = row.borrow_mut();
                row.wrap_lines = !row.wrap_lines;
            }
            let mut sb = self.scroll_bars.borrow_mut();
            let count = u32::try_from(self.rows.len()).unwrap_or(u32::MAX);
            sb.estimated_content_height = if sb.estimated_content_height == Some(800) {
                Some(count)
            } else {
                Some(800)
            };
            return ctx.consume_and_redraw();
        }
        if key.matches(u32::from('e'), Modifiers::CTRL) {
            let mut sb = self.scroll_bars.borrow_mut();
            sb.estimated_content_height = match sb.estimated_content_height {
                None => Some(800),
                Some(_) => None,
            };
            return ctx.consume_and_redraw();
        }
        if key.matches(Key::TAB, Modifiers::empty()) {
            let sb = self.scroll_bars.borrow_mut();
            let mut sv = sb.view.borrow_mut();
            sv.draw_cursor = !sv.draw_cursor;
            return ctx.consume_and_redraw();
        }
        if key.matches(u32::from('v'), Modifiers::CTRL) {
            let mut sb = self.scroll_bars.borrow_mut();
            sb.draw_vertical_scrollbar = !sb.draw_vertical_scrollbar;
            return ctx.consume_and_redraw();
        }
        if key.matches(u32::from('h'), Modifiers::CTRL) {
            let mut sb = self.scroll_bars.borrow_mut();
            sb.draw_horizontal_scrollbar = !sb.draw_horizontal_scrollbar;
            return ctx.consume_and_redraw();
        }
        if key.matches(Key::TAB, Modifiers::SHIFT) {
            let mut sb = self.scroll_bars.borrow_mut();
            sb.draw_vertical_scrollbar = !sb.draw_vertical_scrollbar;
            sb.draw_horizontal_scrollbar = !sb.draw_horizontal_scrollbar;
            return ctx.consume_and_redraw();
        }

        // Anything else is a scroll-view navigation key.
        let sb = self.scroll_bars.borrow_mut();
        sb.view.borrow_mut().handle_event(ctx, event);
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// The ten Lorem-ipsum paragraphs, repeated to build the row dataset.
const LIPSUM: [&str; 10] = [
    "    Lorem ipsum dolor sit amet, consectetur adipiscing elit. Nunc sit amet nunc porta, commodo tellus eu, blandit lectus. Aliquam dignissim rhoncus mi eu ultrices. Suspendisse lectus massa, bibendum sed lorem sit amet, egestas aliquam ante. Mauris venenatis nibh neque. Nulla a mi eget purus porttitor malesuada. Sed ac porta felis. Morbi ultricies urna nisi, et maximus elit convallis a. Morbi ut felis nec orci euismod congue efficitur egestas ex. Quisque eu feugiat magna. Pellentesque porttitor tortor ut iaculis dictum. Nulla erat neque, sollicitudin vitae enim nec, pharetra blandit tortor. Sed orci ante, condimentum vitae sodales in, sodales ut nulla. Suspendisse quam felis, aliquet ut neque a, lacinia sagittis turpis. Vivamus nec dui purus. Proin tempor nisl et porttitor consequat.",
    "    Vivamus elit massa, commodo in laoreet nec, scelerisque ac orci. Donec nec ante sit amet nisi ullamcorper dictum quis non enim. Proin ante libero, consequat sit amet semper a, vulputate non odio. Mauris ut suscipit lacus. Mauris nec dolor id ex mollis tempor at quis ligula. Integer varius commodo ipsum id gravida. Sed ut lobortis est, id egestas nunc. In fringilla ullamcorper porttitor. Donec quis dignissim arcu, vitae sagittis tortor. Sed tempor porttitor arcu, sit amet elementum est ornare id. Morbi rhoncus, ipsum eget tincidunt volutpat, mauris enim vestibulum nibh, mollis iaculis ante enim quis enim. Donec pharetra odio vel ex fringilla, ut laoreet ipsum commodo. Praesent tempus, leo a pellentesque sodales, erat ipsum pretium nulla, id faucibus sem turpis at nibh. Aenean ut dui luctus, vehicula felis vel, aliquam nulla.",
    "    Cras interdum mattis elit non varius. In condimentum velit a tellus sollicitudin interdum. Etiam pulvinar semper ex, eget congue ante tristique ut. Phasellus commodo magna magna, at fermentum tortor porttitor ac. Fusce a efficitur diam, a congue ante. Mauris maximus ultrices leo, non viverra ex hendrerit eu. Donec laoreet turpis nulla, eget imperdiet tortor mollis aliquam. Donec a est eget ante consequat rhoncus.",
    "    Morbi facilisis libero nec viverra imperdiet. Ut dictum faucibus bibendum. Vestibulum ut nisl eu magna sollicitudin elementum vel eu ante. Phasellus euismod ligula massa, vel rutrum elit hendrerit ut. Vivamus id luctus lectus, at ullamcorper leo. Pellentesque in risus finibus, viverra ligula sed, porta nisl. Aliquam pretium accumsan placerat. Etiam a elit posuere, varius erat sed, aliquet quam. Morbi finibus gravida erat, non imperdiet dolor sollicitudin dictum. Aenean eget ullamcorper lacus, et hendrerit lorem. Quisque sed varius mauris.",
    "    Nullam vitae euismod mauris, eu gravida dolor. Nunc vel urna laoreet justo faucibus tempus. Vestibulum tincidunt sagittis metus ac dignissim. Curabitur eleifend dolor consequat malesuada posuere. In hac habitasse platea dictumst. Fusce eget ipsum tincidunt, placerat orci ut, malesuada ante. Vivamus ultrices purus vel orci posuere, sed posuere eros porta. Vestibulum a tellus et tortor scelerisque varius. Pellentesque vel leo sed est semper bibendum. Mauris tellus ante, cursus et nunc vitae, dictum pellentesque ex. In tristique purus felis, non efficitur ante mollis id. Nulla quam nisi, suscipit sit amet mattis vel, placerat sit amet lectus. Vestibulum cursus auctor quam, at convallis felis euismod non. Sed nec magna nisi. Morbi scelerisque accumsan nunc, sed sagittis sem varius sit amet. Maecenas arcu dui, euismod et sem quis, condimentum blandit tellus.",
    "    Nullam auctor lobortis libero non viverra. Mauris a imperdiet eros, a luctus est. Integer pellentesque eros et metus rhoncus egestas. Suspendisse eu risus mauris. Mauris posuere nulla in justo pharetra molestie. Maecenas sagittis at nunc et finibus. Vestibulum quis leo ac mauris malesuada vestibulum vitae eu enim. Ut et maximus elit. Pellentesque lorem felis, tristique vitae posuere vitae, auctor tempus magna. Fusce cursus purus sit amet risus pulvinar, non egestas ligula imperdiet.",
    "    Proin rhoncus tincidunt congue. Curabitur pretium mauris eu erat iaculis semper. Vestibulum augue tortor, vehicula id maximus at, semper eu leo. Vivamus feugiat at purus eu dapibus. Mauris luctus sollicitudin nibh, in placerat est mattis vitae. Morbi ut risus felis. Etiam lobortis mollis diam, id tempor odio sollicitudin a. Morbi congue, lacus ac accumsan consequat, ipsum eros facilisis est, in congue metus ex nec ligula. Vestibulum dolor ligula, interdum nec iaculis vel, interdum a diam. Curabitur mattis, risus at rhoncus gravida, diam est viverra diam, ut mattis augue nulla sed lacus.",
    "    Duis rutrum orci sit amet dui imperdiet porta. In pulvinar imperdiet enim nec tristique. Etiam egestas pulvinar arcu, viverra mollis ipsum. Ut sit amet sapien nibh. Maecenas ut velit egestas, suscipit dolor vel, interdum tellus. Pellentesque faucibus euismod risus, ac vehicula erat sodales a. Aliquam egestas sit amet enim ac posuere. In id venenatis eros, et pharetra neque. Proin facilisis, odio id vehicula elementum, sapien ligula interdum dui, quis vestibulum est quam sit amet nisl. Aliquam in orci et felis aliquet tempus quis id magna. Sed interdum malesuada sem. Proin sagittis est metus, eu vestibulum nunc lacinia in. Vestibulum enim erat, cursus at justo at, porta feugiat quam. Phasellus vestibulum finibus nulla, at egestas augue imperdiet dapibus. Nunc in felis at ante congue interdum ut nec sapien.",
    "    Etiam lacinia ornare mauris, ut lacinia elit sollicitudin non. Morbi cursus dictum enim, et vulputate mi sollicitudin vel. Fusce rutrum augue justo. Phasellus et mauris tincidunt erat lacinia bibendum sed eu orci. Sed nunc lectus, dignissim sit amet ultricies sit amet, efficitur eu urna. Fusce feugiat malesuada ipsum nec congue. Praesent ultrices metus eu pulvinar laoreet. Maecenas pellentesque, metus ac lobortis rhoncus, ligula eros consequat urna, eget dictum lectus sem ut orci. Donec lobortis, lacus sed bibendum auctor, odio turpis suscipit odio, vitae feugiat leo metus ac lectus. Curabitur sed sem arcu.",
    "    Mauris nisi tortor, auctor venenatis turpis a, finibus condimentum lectus. Donec id velit odio. Curabitur ac varius lorem. Nam cursus quam in velit gravida, in bibendum purus fermentum. Sed non rutrum dui, nec ultrices ligula. Integer lacinia blandit nisl non sollicitudin. Praesent nec malesuada eros, sit amet tincidunt nunc.",
];

fn main() -> Result<(), Box<dyn Error>> {
    let paragraphs: Vec<Rc<str>> = LIPSUM.iter().map(|p| Rc::from(*p)).collect();

    // Ten copies of the ten paragraphs, numbered 0..100.
    let mut rows: Vec<Rc<RefCell<ModelRow>>> = Vec::new();
    for i in 0..10usize {
        for (j, paragraph) in paragraphs.iter().enumerate() {
            rows.push(Rc::new(RefCell::new(ModelRow {
                text: Rc::clone(paragraph),
                idx: i * 10 + j,
                wrap_lines: true,
            })));
        }
    }

    let builder_rows: Vec<WidgetRef> = rows
        .iter()
        .map(|row| {
            let widget = to_widget_ref(Rc::clone(row));
            widget
        })
        .collect();

    let scroll_view = ScrollView::new(Source::Builder(Box::new(RowSource { rows: builder_rows })));
    let mut scroll_bars = ScrollBars::new(scroll_view);
    // NOTE: an estimate, not the true content height. Playing with this value
    // (or unsetting it via Ctrl-E) shows how it drives the thumb size.
    scroll_bars.estimated_content_height = Some(800);

    let model: WidgetRef = Rc::new(RefCell::new(ScrollModel {
        scroll_bars: Rc::new(RefCell::new(scroll_bars)),
        rows,
    }));
    run(model)
}

/// Wires an [`App`] over the real terminal and runs `root`. See `counter.rs`.
fn run(root: WidgetRef) -> Result<(), Box<dyn Error>> {
    let tty = PosixTty::new()?;
    let source = tty.dup_reader()?;
    let vx = Vaxis::new(VaxisOptions::default());
    let mut app = App::new(vx, Box::new(tty), Box::new(source));
    app.run(root, Options::default())?;
    Ok(())
}
