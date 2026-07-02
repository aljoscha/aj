//! The interactive alt-screen shell, driven by `vxfw::AsyncApp`.
//!
//! This is the skeleton of the base layout from the alt-screen UX spec: a
//! one-line header, a flex-filling chat area, an editor, and a one-line
//! footer, stacked in a `FlexColumn`. No agent or session wiring yet, the
//! shell only proves out the driver: input reaches the editor, resizes apply
//! live, and Ctrl+C quits with the terminal restored.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use aj_app::cli::args::Args;
use anyhow::Result;
use vaxis::key::Modifiers;
use vaxis::tty::PosixTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    AsyncApp, DrawContext, Event, EventContext, FlexColumn, FlexItem, Options, Surface, Text,
    TextField, Widget, WidgetRef, draw_widget, to_widget_ref,
};

/// The root widget: the base-layout skeleton plus the global quit chord.
struct Shell {
    layout: WidgetRef,
    /// Typed handle to the editor so `Init` can focus it.
    editor: Rc<RefCell<TextField>>,
}

impl Shell {
    fn new() -> Shell {
        let editor = Rc::new(RefCell::new(TextField::new()));
        let layout: WidgetRef = Rc::new(RefCell::new(FlexColumn {
            children: vec![
                FlexItem::init(Rc::new(RefCell::new(Text::new("aj-next"))), 0),
                FlexItem::init(Rc::new(RefCell::new(Text::new("(no conversation yet)"))), 1),
                FlexItem::init(to_widget_ref(Rc::clone(&editor)), 0),
                FlexItem::init(Rc::new(RefCell::new(Text::new("ctrl+c to quit"))), 0),
            ],
        }));
        Shell { layout, editor }
    }
}

impl Widget for Shell {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // The caller's draw_widget re-stamps the returned surface with the
        // Shell's identity, replacing the column's. That is what we want: the
        // column takes no events, and the Shell must sit on the focus path so
        // its capture_event sees every key.
        draw_widget(&self.layout, ctx)
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        // Quit lives in the capture phase so the focused editor can never
        // shadow it. Ctrl+D is not a quit chord here: the TextField binds it
        // to delete-after-cursor.
        if let Event::KeyPress(key) = event {
            if key.matches(u32::from('c'), Modifiers::CTRL) {
                ctx.quit = true;
                ctx.consume_event();
            }
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        if let Event::Init = event {
            ctx.request_focus(to_widget_ref(Rc::clone(&self.editor)));
            ctx.redraw = true;
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Runs the interactive shell until the user quits.
///
/// Restores the terminal via [`AsyncApp::shutdown`] on the way out. The
/// driver's futures are `!Send`, so this must run on a top-level `block_on`
/// (the `#[tokio::main]` future), not a spawned task.
pub async fn run(args: Args) -> Result<()> {
    // Accepted for signature stability. Later phases wire session and model
    // selection from it.
    let _ = args;

    let tty = PosixTty::new()?;
    let reader = tty.open_reader()?;
    let mut app = AsyncApp::new(Vaxis::new(VaxisOptions::default()), Box::new(tty), reader);
    let root: WidgetRef = Rc::new(RefCell::new(Shell::new()));
    app.init(Rc::clone(&root), Options::default()).await?;

    // Restore the terminal even when the loop exits with a render error,
    // otherwise the user is left stuck on the alt screen.
    let result = drive(&mut app, &root).await;
    app.shutdown().await;
    result
}

/// The host loop per the async-app spec: later phases add their own arms
/// (agent events, turn joins, theme reloads) to this exact select.
async fn drive(app: &mut AsyncApp, root: &WidgetRef) -> Result<()> {
    loop {
        // Compute the tick deadline before the select so no arm holds a
        // borrow of `app` another arm needs. The sleep expression is
        // evaluated even when the guard is false, hence the fallback instant.
        let deadline = app.next_tick_deadline();
        tokio::select! {
            event = app.next_input() => {
                match event {
                    Some(event) => {
                        if app.handle_input(event).quit {
                            break;
                        }
                    }
                    // The reader ended (EOF or a read error), so no further
                    // input can arrive.
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now).into()),
                if deadline.is_some() =>
            {
                if app.fire_due_timers().quit {
                    break;
                }
            }
        }
        app.render_if_needed(root)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{PipeWriter, Write};

    use vaxis::tty::TestTty;

    use super::*;

    /// Builds and initializes an `AsyncApp` over a `TestTty`, with a pipe as
    /// the read source. Keep the returned writer alive or the reader sees EOF.
    async fn init_app() -> (AsyncApp, PipeWriter, Rc<RefCell<Shell>>, WidgetRef) {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        // Answer the DA1 probe up front so init's capability wait returns as
        // soon as the reader consumes the reply instead of after its timeout.
        writer.write_all(b"\x1b[?c").expect("write DA1 reply");

        let shell = Rc::new(RefCell::new(Shell::new()));
        let root: WidgetRef = to_widget_ref(Rc::clone(&shell));
        let mut app = AsyncApp::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
            reader.into(),
        );
        app.init(Rc::clone(&root), Options::default())
            .await
            .expect("init");
        (app, writer, shell, root)
    }

    #[tokio::test]
    async fn typed_key_reaches_the_editor_and_latches_redraw() {
        let (mut app, mut writer, shell, _root) = init_app().await;
        assert!(!app.needs_redraw(), "init's first draw clears the latch");

        writer.write_all(b"j").expect("write key byte");
        let event = app.next_input().await.expect("input event");
        let frame = app.handle_input(event);

        assert!(!frame.quit);
        assert!(app.needs_redraw());
        // Init focused the editor, so the typed grapheme landed there.
        assert_eq!(shell.borrow().editor.borrow().graphemes_before_cursor(), 1);
    }

    #[tokio::test]
    async fn ctrl_c_quits() {
        let (mut app, mut writer, _shell, _root) = init_app().await;

        writer.write_all(&[0x03]).expect("write ctrl+c byte");
        let event = app.next_input().await.expect("input event");
        assert!(app.handle_input(event).quit);

        app.shutdown().await;
    }
}
