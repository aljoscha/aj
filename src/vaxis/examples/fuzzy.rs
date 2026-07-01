//! Port of upstream `examples/fuzzy.zig`: a fuzzy file finder.
//!
//! A [`TextField`] filters a file list live; matches render as [`RichText`] with
//! the matched graphemes highlighted; a [`ListView`] shows and navigates them.
//! Enter selects the cursored line, which is printed to stdout on exit. Ctrl-C
//! quits with no selection.
//!
//! # File enumeration
//!
//! Upstream shells out to `fd`. We do the same via [`std::process::Command`],
//! and fall back to an in-process `ignore`-crate walk (honoring `.gitignore`,
//! like `fd`) when `fd` is not installed, so the example runs anywhere.
//!
//! # Shared state
//!
//! The `TextField` callbacks and the `ListView` builder both need the filtered
//! results, but neither can borrow the model. So the state that would live on
//! the upstream model is held in `Rc<RefCell<..>>` handles cloned into each:
//!
//! - `lines`: the immutable source file list.
//! - `filtered_widgets`: the highlighted [`RichText`] rows the list draws.
//! - `filtered_text`: the plain matched lines, parallel to `filtered_widgets`,
//!   so submit can recover the selected string without inspecting widgets.
//! - `list_view`: shared so `on_change` can reset it to the top.
//! - `result`: the selection, read after the app returns.

use std::cell::RefCell;
use std::error::Error;
use std::process::Command;
use std::rc::Rc;

use vaxis::cell::{Color, Segment, Style};
use vaxis::key::Modifiers;
use vaxis::tty::PosixTty;
use vaxis::unicode::grapheme_iterator;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    App, DrawContext, Event, EventContext, ListSource, ListView, MaxSize, Options, RelativePoint,
    RichText, Source, SubSurface, Surface, Text, TextField, TextSpan, Widget, WidgetRef,
    draw_widget, to_widget_ref,
};

/// Hands the list view a filtered row by index.
struct FilteredSource {
    filtered: Rc<RefCell<Vec<WidgetRef>>>,
}

impl ListSource for FilteredSource {
    fn item(&self, idx: usize, _cursor: usize) -> Option<WidgetRef> {
        self.filtered.borrow().get(idx).cloned()
    }
}

/// The application state.
struct FuzzyModel {
    lines: Rc<Vec<String>>,
    filtered_widgets: Rc<RefCell<Vec<WidgetRef>>>,
    filtered_text: Rc<RefCell<Vec<String>>>,
    list_view: Rc<RefCell<ListView>>,
    text_field: Rc<RefCell<TextField>>,
}

impl Widget for FuzzyModel {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let max = ctx.max.size();

        // The results fill the screen below the two-row prompt area.
        let list_view = to_widget_ref(Rc::clone(&self.list_view));
        let list_surf = draw_widget(
            &list_view,
            &ctx.with_constraints(
                ctx.min,
                MaxSize {
                    width: Some(max.width),
                    height: Some(max.height.saturating_sub(3)),
                },
            ),
        );

        // The input line, one row tall, indented past the prompt glyph.
        let text_field = to_widget_ref(Rc::clone(&self.text_field));
        let field_surf = draw_widget(
            &text_field,
            &ctx.with_constraints(
                ctx.min,
                MaxSize {
                    width: Some(max.width),
                    height: Some(1),
                },
            ),
        );

        // A prompt glyph in the top-left corner.
        let prompt: WidgetRef = Rc::new(RefCell::new(Text {
            style: Style {
                fg: Color::Index(4),
                ..Style::default()
            },
            ..Text::new("\u{f054}")
        }));
        let prompt_surf = draw_widget(
            &prompt,
            &ctx.with_constraints(
                ctx.min,
                MaxSize {
                    width: Some(2),
                    height: Some(1),
                },
            ),
        );

        Surface {
            size: max,
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children: vec![
                SubSurface {
                    origin: RelativePoint { row: 2, col: 0 },
                    surface: list_surf,
                    z_index: 0,
                },
                SubSurface {
                    origin: RelativePoint { row: 0, col: 2 },
                    surface: field_surf,
                    z_index: 0,
                },
                SubSurface {
                    origin: RelativePoint { row: 0, col: 0 },
                    surface: prompt_surf,
                    z_index: 0,
                },
            ],
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::Init => {
                // Start with every line matching (empty query).
                filter("", &self.lines, &self.filtered_widgets, &self.filtered_text);
                let text_field = to_widget_ref(Rc::clone(&self.text_field));
                ctx.request_focus(text_field);
            }
            Event::FocusIn => {
                let text_field = to_widget_ref(Rc::clone(&self.text_field));
                ctx.request_focus(text_field);
            }
            Event::KeyPress(key) => {
                if key.matches(u32::from('c'), Modifiers::CTRL) {
                    ctx.quit = true;
                    return;
                }
                // Keys the text field did not consume (the arrows) bubble here
                // and drive the list cursor.
                self.list_view.borrow_mut().handle_event(ctx, event);
            }
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Rebuilds the filtered widget and text lists for `query`.
///
/// A line matches when the query's grapheme clusters occur in it in order. When
/// the query is all lowercase the match is case-insensitive (we lowercase the
/// line first); any uppercase makes it case-sensitive. Matched graphemes are
/// highlighted. ASCII lowercasing preserves byte offsets, so offsets found in
/// the lowercased line index the original line safely.
fn filter(
    query: &str,
    lines: &[String],
    filtered_widgets: &RefCell<Vec<WidgetRef>>,
    filtered_text: &RefCell<Vec<String>>,
) {
    let mut widgets = filtered_widgets.borrow_mut();
    let mut texts = filtered_text.borrow_mut();
    widgets.clear();
    texts.clear();

    let has_upper = query.bytes().any(|b| b.is_ascii_uppercase());
    let match_style = Style {
        fg: Color::Index(4),
        reverse: true,
        ..Style::default()
    };

    'outer: for line in lines {
        let target = if has_upper {
            line.clone()
        } else {
            line.to_ascii_lowercase()
        };

        let mut spans: Vec<TextSpan> = Vec::new();
        let mut i: usize = 0;
        for grapheme in grapheme_iterator(query) {
            let needle = grapheme.bytes(query);
            match target[i..].find(needle) {
                Some(rel) => {
                    let idx = i + rel;
                    spans.push(Segment {
                        text: line[i..idx].to_string(),
                        ..Segment::default()
                    });
                    spans.push(Segment {
                        text: line[idx..idx + needle.len()].to_string(),
                        style: match_style,
                        ..Segment::default()
                    });
                    i = idx + needle.len();
                }
                None => continue 'outer,
            }
        }
        spans.push(Segment {
            text: line[i..].to_string(),
            ..Segment::default()
        });

        texts.push(line.clone());
        let row: WidgetRef = Rc::new(RefCell::new(RichText::new(spans)));
        widgets.push(row);
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let lines: Rc<Vec<String>> = Rc::new(enumerate_files());
    let filtered_widgets: Rc<RefCell<Vec<WidgetRef>>> = Rc::new(RefCell::new(Vec::new()));
    let filtered_text: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let result: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let list_view = Rc::new(RefCell::new(ListView::new(Source::Builder(Box::new(
        FilteredSource {
            filtered: Rc::clone(&filtered_widgets),
        },
    )))));

    let mut text_field = TextField::new();
    {
        let lines = Rc::clone(&lines);
        let filtered_widgets = Rc::clone(&filtered_widgets);
        let filtered_text = Rc::clone(&filtered_text);
        let list_view = Rc::clone(&list_view);
        text_field.on_change = Some(Box::new(move |ctx, query| {
            filter(query, &lines, &filtered_widgets, &filtered_text);
            // Reset the list to the top, matching upstream's scroll/cursor reset.
            list_view.borrow_mut().jump_to_item(0);
            ctx.consume_and_redraw();
        }));
    }
    {
        let filtered_text = Rc::clone(&filtered_text);
        let list_view = Rc::clone(&list_view);
        let result = Rc::clone(&result);
        text_field.on_submit = Some(Box::new(move |ctx, _query| {
            let cursor = usize::try_from(list_view.borrow().cursor).unwrap_or(usize::MAX);
            let selected = filtered_text.borrow();
            if cursor < selected.len() {
                *result.borrow_mut() = Some(selected[cursor].clone());
            }
            ctx.quit = true;
        }));
    }

    let model: WidgetRef = Rc::new(RefCell::new(FuzzyModel {
        lines,
        filtered_widgets,
        filtered_text,
        list_view,
        text_field: Rc::new(RefCell::new(text_field)),
    }));

    run(model)?;

    // Upstream prints the selection and exits 0, or exits 130 with no selection.
    match result.borrow().clone() {
        Some(selection) => {
            println!("{selection}");
            Ok(())
        }
        None => std::process::exit(130),
    }
}

/// Lists files under the current directory, preferring `fd`.
fn enumerate_files() -> Vec<String> {
    match run_fd() {
        Some(files) => files,
        None => walk_current_dir(),
    }
}

/// Runs `fd` and returns its output lines, or `None` if it is not installed or
/// exits with an error.
fn run_fd() -> Option<Vec<String>> {
    let output = Command::new("fd").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Some(
        stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// In-process fallback: walk the current directory honoring `.gitignore`.
fn walk_current_dir() -> Vec<String> {
    ignore::WalkBuilder::new(".")
        .build()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            // Report paths relative to the cwd without the leading "./", and
            // skip the walk root itself.
            let rel = entry.path().strip_prefix(".").ok()?;
            if rel.as_os_str().is_empty() {
                return None;
            }
            Some(rel.to_string_lossy().into_owned())
        })
        .collect()
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
