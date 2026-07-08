//! Read-only content overlays: help, auth status, session info, usage.
//!
//! A [`ContentOverlay`] is a scrollable, non-interactive list of text
//! rows shown inside an [`OverlayWindow`]. Esc or Enter close it
//! (returning to the parent overlay or the editor), Up/Down and
//! PgUp/PgDn scroll the body, and every other key is swallowed so
//! nothing leaks to the layout behind the modal. Each row is a list of
//! styled spans ([`Row`]) the host builds from the shared `aj_app` data,
//! so the one widget backs all four read-only overlays.
//!
//! Async overlays (auth, session info, usage) open showing a single
//! "Loading…" row and are refilled through the [`ListView`] handle
//! [`open_content_overlay`] returns once the host's fetch lands. That
//! keeps the fetch off the open path so a slow network probe never
//! blocks the overlay from appearing.

use std::cell::RefCell;
use std::rc::Rc;

use aj_app::auth::ProviderAuthStatus;
use aj_app::commands::COMMANDS;
use aj_app::keybindings::{AJ_KEYBINDINGS, default_action_shortcut};
use aj_app::theme::{Theme, ThemeColor};
use aj_app::usage::{ProviderUsageStatus, UsageOutcome, format_window_status, now_unix_ms};
use aj_session::SessionStats;
use vaxis::cell::{Segment, Style};
use vaxis::key::{Key, Modifiers};
use vaxis::vxfw::{
    DrawContext, Event, EventContext, ListView, OverlayWindow, RelativePoint, RichText, ScrollBars,
    Source, SubSurface, Surface, Widget, WidgetRef, to_widget_ref,
};

use crate::overlay::{
    OpenOverlay, OverlayChrome, OverlayPlacement, OverlayStack, close_top, subtitle_close,
};
use crate::transcript::vaxis_color;

/// A single content-overlay row: styled spans laid out as one line.
///
/// A plain row is one default-styled span. Because [`RichText::new`]'s
/// layout defaults match [`Text`](vaxis::vxfw::Text)'s, a plain row draws
/// exactly like a plain-string row would, so converting the plain builders
/// to this model is appearance-preserving.
pub(crate) type Row = Vec<Segment>;

/// A single default-styled span carrying `text`.
pub(crate) fn plain(text: impl Into<String>) -> Row {
    vec![span(text, Style::default())]
}

/// A styled span for one column of a row.
fn span(text: impl Into<String>, style: Style) -> Segment {
    Segment {
        text: text.into(),
        style,
        ..Segment::default()
    }
}

/// Column tints for the read-only content pages, resolved once from the
/// theme like [`OverlayChrome`]. `dim` tints the provider-id column and
/// `muted` the secondary detail column, matching `aj`'s auth page.
#[derive(Clone, Copy)]
pub(crate) struct ContentStyles {
    pub(crate) dim: Style,
    pub(crate) muted: Style,
}

impl ContentStyles {
    pub(crate) fn from_theme(theme: &Theme) -> ContentStyles {
        let mode = theme.color_mode();
        let fg = |token: ThemeColor| Style {
            fg: vaxis_color(theme.fg_color(token), mode),
            ..Style::default()
        };
        ContentStyles {
            dim: fg(ThemeColor::Dim),
            muted: fg(ThemeColor::Muted),
        }
    }
}

/// PgUp/PgDn step, in rows. A fixed jump rather than a viewport-derived
/// one keeps the widget from needing to know its drawn height.
const PAGE_STEP: usize = 10;

/// A scrollable, read-only list of text rows.
///
/// Focus sits on this widget while it is the top overlay, so it
/// intercepts every key in its capturing phase: Esc/Enter close (via
/// [`Self::on_close`]), the arrow and page keys scroll, and everything
/// else is consumed so it can't reach the base layout.
pub(crate) struct ContentOverlay {
    /// The row list, shared with `bars` (which draws it) and handed back
    /// by [`open_content_overlay`] so the host can refill an async
    /// overlay's rows after the initial "Loading…" state.
    list: Rc<RefCell<ListView>>,
    bars: ScrollBars<ListView>,
    /// Closes this overlay and restores focus to the parent. Runs inside
    /// key dispatch, where the live [`EventContext`] can move focus.
    pub(crate) on_close: Option<Box<dyn FnMut(&mut EventContext)>>,
}

impl ContentOverlay {
    pub(crate) fn new(rows: Vec<Row>) -> ContentOverlay {
        let mut list = ListView::new(Source::Slice(row_widgets(&rows)));
        list.item_count = Some(u32::try_from(rows.len()).expect("row count fits u32"));
        // Document scroll with no visible cursor: the list moves its
        // hidden cursor to keep the viewport following the arrow keys.
        list.draw_cursor = false;
        let mut bars = ScrollBars::new(list);
        bars.draw_horizontal_scrollbar = false;
        let list = Rc::clone(&bars.view);
        ContentOverlay {
            list,
            bars,
            on_close: None,
        }
    }

    /// The shared row list, for the host to refill after an async fetch.
    pub(crate) fn list_handle(&self) -> Rc<RefCell<ListView>> {
        Rc::clone(&self.list)
    }
}

impl Widget for ContentOverlay {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // Wrap the bars in an opaque full-size surface so a shorter
        // refill can't leave stale cells from a taller previous frame.
        let mut surface = Surface::with_size(ctx.max.size());
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: self.bars.draw(ctx),
            z_index: 0,
        });
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            return;
        };
        // Esc and Enter both close: a read-only view has nothing to
        // confirm, so Enter is just a second "dismiss" key (matching
        // `aj`'s read-only overlays).
        if key.matches(Key::ESCAPE, Modifiers::empty())
            || key.matches(Key::ENTER, Modifiers::empty())
        {
            if let Some(cb) = self.on_close.as_mut() {
                cb(ctx);
            }
            ctx.consume_and_redraw();
            return;
        }
        if key.matches(Key::DOWN, Modifiers::empty()) {
            self.list.borrow_mut().next_item(ctx);
        } else if key.matches(Key::UP, Modifiers::empty()) {
            self.list.borrow_mut().prev_item(ctx);
        } else if key.matches(Key::PAGE_DOWN, Modifiers::empty()) {
            for _ in 0..PAGE_STEP {
                self.list.borrow_mut().next_item(ctx);
            }
        } else if key.matches(Key::PAGE_UP, Modifiers::empty()) {
            for _ in 0..PAGE_STEP {
                self.list.borrow_mut().prev_item(ctx);
            }
        }
        // Read-only: swallow every key so none reaches the base layout.
        ctx.consume_and_redraw();
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Build the list-row widgets for a set of rows.
fn row_widgets(rows: &[Row]) -> Vec<WidgetRef> {
    rows.iter()
        .map(|r| {
            let text: WidgetRef = Rc::new(RefCell::new(RichText::new(r.clone())));
            text
        })
        .collect()
}

/// Replace an open overlay's rows, for an async overlay whose fetch has
/// landed. Resets the scroll to the top so the filled content reads from
/// its first row.
pub(crate) fn set_rows(list: &Rc<RefCell<ListView>>, rows: Vec<Row>) {
    let mut list = list.borrow_mut();
    list.item_count = Some(u32::try_from(rows.len()).expect("row count fits u32"));
    list.children = Source::Slice(row_widgets(&rows));
    list.jump_to_item(0);
}

/// Push a read-only content overlay showing `rows` on top of `stack`,
/// keeping any parent overlay underneath (so Esc returns to it), and
/// move focus onto it.
///
/// Returns the row-list handle so the host can [`set_rows`] later. An
/// async overlay passes a `["Loading…"]` seed and fills it when its
/// fetch completes.
pub(crate) fn open_content_overlay(
    stack: &Rc<RefCell<OverlayStack>>,
    editor: &WidgetRef,
    chrome: &OverlayChrome,
    title: &str,
    rows: Vec<Row>,
    ctx: &mut EventContext,
) -> Rc<RefCell<ListView>> {
    let content = Rc::new(RefCell::new(ContentOverlay::new(rows)));
    let list = content.borrow().list_handle();
    {
        let stack_for_close = Rc::clone(stack);
        let editor_for_close = Rc::clone(editor);
        content.borrow_mut().on_close = Some(Box::new(move |ctx| {
            close_top(&stack_for_close, ctx, &editor_for_close);
        }));
    }
    // The window's child and the focus target are the same widget: keys
    // route to the ContentOverlay while the OverlayWindow supplies the
    // border and title chrome around it.
    let focus: WidgetRef = to_widget_ref(Rc::clone(&content));
    let mut window = OverlayWindow::new(title, to_widget_ref(content));
    // The close hint resolves through the shared keybinding data (Spec F):
    // Esc's label from `format_keybinding`, the close-all label from the
    // keymap action. The Esc/Enter *handling* stays a fixed `ContentOverlay`
    // convention (see the NOTE in `crate::overlay`).
    window.subtitle = subtitle_close();
    window.border_style = chrome.border;
    window.title_style = chrome.title;
    window.subtitle_style = chrome.subtitle;
    stack.borrow_mut().push(OpenOverlay {
        widget: to_widget_ref(Rc::new(RefCell::new(window))),
        focus: Rc::clone(&focus),
        placement: OverlayPlacement::Large,
    });
    ctx.request_focus(focus);
    ctx.redraw = true;
    list
}

// ============================================================================
// Row builders
// ============================================================================

/// The single-row "Loading…" seed an async overlay opens with.
pub(crate) fn loading_rows() -> Vec<Row> {
    vec![plain("Loading\u{2026}")]
}

/// Help overlay rows: the keybinding table and the command catalog, each
/// with its shortcut resolved from the keybinding data so a rebind flows
/// through to the label (Spec F's hint-label rule).
pub(crate) fn help_rows() -> Vec<Row> {
    let mut rows = Vec::new();

    rows.push(plain("Keybindings"));
    let short_w = AJ_KEYBINDINGS
        .iter()
        .filter_map(|(id, _, _)| default_action_shortcut(id))
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(0);
    for (id, _default_chord, desc) in AJ_KEYBINDINGS {
        // Resolve through `default_action_shortcut`, never the raw chord
        // literal in the table, so the label tracks the binding.
        let short = default_action_shortcut(id).unwrap_or_default();
        rows.push(plain(format!("  {short:<short_w$}  {desc}")));
    }

    rows.push(plain(""));
    rows.push(plain("Commands"));
    let cat_w = COMMANDS
        .iter()
        .map(|c| c.category.chars().count())
        .max()
        .unwrap_or(0);
    let title_w = COMMANDS
        .iter()
        .map(|c| c.title.chars().count())
        .max()
        .unwrap_or(0);
    for cmd in COMMANDS {
        let mut line = format!(
            "  {cat:<cat_w$}  {title:<title_w$}  {desc}",
            cat = cmd.category,
            title = cmd.title,
            desc = cmd.description
        );
        if let Some(short) = cmd.action_id.and_then(default_action_shortcut) {
            line.push_str(&format!("  ({short})"));
        }
        rows.push(plain(line));
    }
    rows
}

/// Auth-status rows: one per provider, its credential summary and any
/// secondary detail (e.g. token expiry).
///
/// Each row is three columns: the provider id in `styles.dim`, the summary
/// in the default style, and the optional detail in `styles.muted`. The
/// muted tint separates the detail column, so it reads as its own field
/// without the parentheses `aj` also omits. The id column stays
/// left-aligned, a deliberate divergence from `aj`'s right-aligned prefix.
pub(crate) fn auth_rows(statuses: &[ProviderAuthStatus], styles: &ContentStyles) -> Vec<Row> {
    if statuses.is_empty() {
        return vec![plain("No providers configured.")];
    }
    let id_w = statuses
        .iter()
        .map(|s| s.provider_id.chars().count())
        .max()
        .unwrap_or(0);
    statuses
        .iter()
        .map(|s| {
            let mut row = vec![
                span(format!("{id:<id_w$}", id = s.provider_id), styles.dim),
                span(
                    format!("  {summary}", summary = s.summary),
                    Style::default(),
                ),
            ];
            if let Some(detail) = &s.detail {
                row.push(span(format!("  {detail}"), styles.muted));
            }
            row
        })
        .collect()
}

/// Usage rows: one group per provider. Only a provider's first row
/// carries its id, continuation rows leave it blank so the id column
/// groups the windows visually (matching `aj`'s usage page).
///
/// Each row is up to three columns, tinted like [`auth_rows`]: the
/// fixed 12-col provider-id prefix in `styles.dim` (blank on
/// continuation rows), the window/status label in the default style,
/// and the per-window status detail in `styles.muted`.
pub(crate) fn usage_rows(statuses: &[ProviderUsageStatus], styles: &ContentStyles) -> Vec<Row> {
    let now_ms = now_unix_ms();
    let mut rows = Vec::new();
    for status in statuses {
        let id = status.provider_id.as_str();
        let mut prefix = id.to_string();
        let mut push = |rows: &mut Vec<Row>, label: &str, detail: Option<&str>| {
            let mut row = vec![
                span(format!("{prefix:<12}"), styles.dim),
                span(format!("  {label}"), Style::default()),
            ];
            if let Some(detail) = detail {
                row.push(span(format!("  {detail}"), styles.muted));
            }
            rows.push(row);
            prefix = String::new();
        };
        match &status.outcome {
            UsageOutcome::Usage(usage) => {
                if usage.windows.is_empty()
                    && usage.notes.is_empty()
                    && usage.reset_credits.is_none()
                {
                    push(&mut rows, "no usage data reported", None);
                }
                for window in &usage.windows {
                    let desc = format_window_status(window.used, window.resets_at, now_ms);
                    push(&mut rows, &window.label, Some(&desc));
                }
                for note in &usage.notes {
                    push(&mut rows, note, None);
                }
                if let Some(available) = usage.reset_credits {
                    let desc = if available > 0 {
                        format!("{available} available")
                    } else {
                        "no resets available".to_string()
                    };
                    push(&mut rows, "Rate-limit resets", Some(&desc));
                }
            }
            UsageOutcome::Unsupported { reason } => {
                push(
                    &mut rows,
                    &format!("usage not available \u{2014} {reason}"),
                    None,
                );
            }
            UsageOutcome::NotConfigured => push(&mut rows, "not configured", None),
            UsageOutcome::NoSource => push(&mut rows, "usage reporting not supported", None),
            UsageOutcome::Error(err) => push(&mut rows, &format!("error: {err}"), None),
        }
    }
    if rows.is_empty() {
        rows.push(plain("No usage sources."));
    }
    rows
}

/// One session-info row rendered from the shared `aj_app` digest.
/// Section headers indent to 2 columns and key/value pairs to 4, and a
/// blank spacer is emitted as a single-space row so it occupies a real
/// line instead of collapsing to zero height in the [`ListView`].
///
/// Ported layout from `aj`'s session-info overlay so both frontends
/// show the same digest with the same indentation.
pub(crate) fn session_info_rows(stats: &SessionStats) -> Vec<Row> {
    let rows = aj_app::session_info::digest(stats);
    let key_width = rows
        .iter()
        .filter_map(|row| match row {
            aj_app::session_info::InfoRow::Kv { key, .. } => Some(key.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|row| match row {
            aj_app::session_info::InfoRow::Header(title) => plain(format!("  {title}")),
            aj_app::session_info::InfoRow::Kv { key, value } => {
                plain(format!("    {key:<key_width$}  {value}"))
            }
            // A single space, not the empty string: an empty `RichText`
            // row collapses to zero height in the `ListView`, which would
            // erase the gap between sections. One space forces a line.
            aj_app::session_info::InfoRow::Blank => plain(" "),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use aj_app::keybindings::ACTION_PALETTE_OPEN;
    use aj_models::types::{Usage, UsageCost};
    use aj_session::SessionSettings;
    use vaxis::cell::Color;

    use super::*;

    /// Concatenate a row's span texts, so the plain-text `.contains(...)`
    /// assertions keep working on a styled row.
    fn row_text(row: &Row) -> String {
        row.iter().map(|s| s.text.as_str()).collect()
    }

    /// Join a set of rows into one plain-text blob for `.contains(...)`.
    fn rows_text(rows: &[Row]) -> String {
        rows.iter().map(row_text).collect::<Vec<_>>().join("\n")
    }

    /// Distinct dim/muted tints so a column left at the default fg fails the
    /// tinting assertions.
    fn test_styles() -> ContentStyles {
        ContentStyles {
            dim: Style {
                fg: Color::Index(1),
                ..Style::default()
            },
            muted: Style {
                fg: Color::Index(2),
                ..Style::default()
            },
        }
    }

    #[test]
    fn help_rows_resolve_shortcuts_from_binding_data() {
        let rows = rows_text(&help_rows());
        // The command catalog and keybinding table are both present.
        assert!(rows.contains("Keybindings"), "{rows}");
        assert!(rows.contains("Commands"), "{rows}");
        for cmd in COMMANDS {
            assert!(rows.contains(cmd.title), "missing command {}", cmd.title);
        }
        // The palette-open shortcut is data-derived: the assertion value
        // itself comes from the binding table, so a rebind changes both
        // this expectation and the rendered label together (never a
        // literal).
        let resolved =
            default_action_shortcut(ACTION_PALETTE_OPEN).expect("palette-open has a default chord");
        assert!(
            rows.contains(&resolved),
            "expected resolved shortcut {resolved:?} in help body: {rows}"
        );
    }

    #[test]
    fn auth_rows_render_summary_and_detail() {
        let rows = rows_text(&auth_rows(
            &[
                ProviderAuthStatus {
                    provider_id: "anthropic".into(),
                    configured: true,
                    summary: "subscription".into(),
                    detail: Some("expires in 1h".into()),
                },
                ProviderAuthStatus {
                    provider_id: "openai".into(),
                    configured: false,
                    summary: "not configured".into(),
                    detail: None,
                },
            ],
            &test_styles(),
        ));
        assert!(rows.contains("anthropic"), "{rows}");
        assert!(rows.contains("subscription"), "{rows}");
        assert!(rows.contains("expires in 1h"), "{rows}");
        assert!(rows.contains("not configured"), "{rows}");
    }

    /// The auth page tints its columns: the provider id in the dim style,
    /// the summary in the default style, and the detail in the muted style,
    /// with no parentheses around the detail. This fails if a column is
    /// left at the default fg.
    #[test]
    fn auth_rows_tint_columns_from_styles() {
        let styles = test_styles();
        let rows = auth_rows(
            &[ProviderAuthStatus {
                provider_id: "anthropic".into(),
                configured: true,
                summary: "subscription".into(),
                detail: Some("expires in 1h".into()),
            }],
            &styles,
        );
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.len(), 3, "id, summary, and detail spans: {row:?}");
        // Provider id in the dim tint.
        assert!(row[0].text.contains("anthropic"), "{row:?}");
        assert_eq!(row[0].style, styles.dim);
        // Summary in the default style.
        assert!(row[1].text.contains("subscription"), "{row:?}");
        assert_eq!(row[1].style, Style::default());
        // Detail in the muted tint, and no parentheses.
        assert!(row[2].text.contains("expires in 1h"), "{row:?}");
        assert_eq!(row[2].style, styles.muted);
        assert!(
            !row_text(row).contains('('),
            "detail carries no parentheses: {row:?}"
        );
    }

    /// A provider with no detail yields a two-span row (id + summary), so
    /// the empty case doesn't leave a stray muted span.
    #[test]
    fn auth_rows_without_detail_have_no_muted_span() {
        let styles = test_styles();
        let rows = auth_rows(
            &[ProviderAuthStatus {
                provider_id: "openai".into(),
                configured: false,
                summary: "not configured".into(),
                detail: None,
            }],
            &styles,
        );
        assert_eq!(rows[0].len(), 2, "{:?}", rows[0]);
    }

    #[test]
    fn usage_rows_group_windows_under_the_provider() {
        use aj_models::usage::{ProviderUsage, UsageWindow};
        let rows = rows_text(&usage_rows(
            &[
                ProviderUsageStatus {
                    provider_id: "anthropic".into(),
                    outcome: UsageOutcome::Usage(ProviderUsage {
                        windows: vec![UsageWindow {
                            label: "5-hour".into(),
                            used: 0.5,
                            resets_at: None,
                        }],
                        notes: Vec::new(),
                        reset_credits: None,
                    }),
                },
                ProviderUsageStatus {
                    provider_id: "openai".into(),
                    outcome: UsageOutcome::NotConfigured,
                },
            ],
            &test_styles(),
        ));
        assert!(rows.contains("anthropic"), "{rows}");
        assert!(rows.contains("5-hour"), "{rows}");
        assert!(rows.contains("50% used"), "{rows}");
        assert!(rows.contains("not configured"), "{rows}");
    }

    /// The usage page tints its columns like the auth page: the provider
    /// id in the dim style, the window label in the default style, and the
    /// status detail in the muted style. A provider's continuation rows
    /// leave the id column blank so the group reads as one provider. This
    /// fails if a column is left at the default fg.
    #[test]
    fn usage_rows_tint_columns_from_styles() {
        use aj_models::usage::{ProviderUsage, UsageWindow};
        let styles = test_styles();
        let rows = usage_rows(
            &[ProviderUsageStatus {
                provider_id: "anthropic".into(),
                outcome: UsageOutcome::Usage(ProviderUsage {
                    windows: vec![
                        UsageWindow {
                            label: "5-hour".into(),
                            used: 0.5,
                            resets_at: None,
                        },
                        UsageWindow {
                            label: "weekly".into(),
                            used: 0.25,
                            resets_at: None,
                        },
                    ],
                    notes: Vec::new(),
                    reset_credits: None,
                }),
            }],
            &styles,
        );
        assert_eq!(rows.len(), 2, "one row per window: {rows:?}");

        // First row: id, label, and detail spans.
        let first = &rows[0];
        assert_eq!(first.len(), 3, "id, label, and detail spans: {first:?}");
        // Provider id in the dim tint, on the first row of the group.
        assert!(first[0].text.contains("anthropic"), "{first:?}");
        assert_eq!(first[0].style, styles.dim);
        // Window label in the default style.
        assert!(first[1].text.contains("5-hour"), "{first:?}");
        assert_eq!(first[1].style, Style::default());
        // Status detail in the muted tint.
        assert!(first[2].text.contains("50% used"), "{first:?}");
        assert_eq!(first[2].style, styles.muted);

        // Continuation row: id column is blank (no provider id), still in
        // the dim tint, and the label/detail carry the same tints.
        let second = &rows[1];
        assert_eq!(second.len(), 3, "id, label, and detail spans: {second:?}");
        assert!(
            second[0].text.trim().is_empty(),
            "continuation id column is blank: {second:?}"
        );
        assert_eq!(second[0].style, styles.dim);
        assert!(second[1].text.contains("weekly"), "{second:?}");
        assert_eq!(second[1].style, Style::default());
        assert_eq!(second[2].style, styles.muted);
    }

    fn sample_stats() -> SessionStats {
        SessionStats {
            session_id: "2026-06-19-14-22-03-512".to_string(),
            path: PathBuf::from("/home/u/.aj/sessions/home-u-proj/2026-06-19-14-22-03-512.jsonl"),
            created_at: None,
            last_activity: None,
            size_bytes: Some(48 * 1024),
            total_entries: 127,
            user_messages: 15,
            assistant_messages: 18,
            tool_results: 30,
            tool_calls: 31,
            tool_call_counts: vec![("read_file".to_string(), 12), ("Bash".to_string(), 8)],
            subagents: 2,
            compactions: 1,
            usage: Usage {
                input: 1_000,
                output: 2_000,
                cache_read: 500,
                cache_write: 250,
                total_tokens: 3_750,
                cost: UsageCost {
                    input: 0.10,
                    output: 0.20,
                    cache_read: 0.01,
                    cache_write: 0.02,
                    total: 0.33,
                },
            },
            settings: SessionSettings {
                model: Some(("anthropic".to_string(), "claude-sonnet-4-5".to_string())),
                thinking: Some("medium".to_string()),
                speed: None,
                verbosity: None,
            },
        }
    }

    #[test]
    fn session_info_rows_render_identity_counts_and_tools() {
        let rows = session_info_rows(&sample_stats());
        let blob = rows_text(&rows);
        assert!(blob.contains("2026-06-19-14-22-03-512"), "{blob}");
        assert!(blob.contains("home-u-proj"), "{blob}");
        assert!(blob.contains("anthropic / claude-sonnet-4-5"), "{blob}");
        assert!(blob.contains("48 KB"), "{blob}");
        assert!(blob.contains("read_file"), "{blob}");
        assert!(blob.contains("Tool calls (31)"), "{blob}");
        assert!(blob.contains("total tokens"), "{blob}");
        assert!(blob.contains("$0.3300"), "{blob}");

        // Section headers indent to 2 columns, key/value rows to 4,
        // matching `aj`'s layout.
        let texts: Vec<String> = rows.iter().map(row_text).collect();
        assert!(
            texts.iter().any(|t| t == "  Session"),
            "header at 2-col indent: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.starts_with("    id ")),
            "key/value at 4-col indent: {texts:?}"
        );

        // The blank spacer between sections occupies a real line: the row
        // before the "Settings" header is blank but not the empty string.
        // An empty `RichText` collapses to zero height in the `ListView`,
        // so a non-empty (whitespace) line is what keeps the gap visible.
        let settings = texts
            .iter()
            .position(|t| t == "  Settings")
            .expect("Settings header present");
        assert!(settings >= 1, "Settings not first: {texts:?}");
        let spacer = &texts[settings - 1];
        assert!(
            !spacer.is_empty() && spacer.trim().is_empty(),
            "spacer row before Settings is a real blank line, not an empty \
             string that would collapse to zero height: {spacer:?}"
        );
    }

    /// A plain builder produces single-span, default-styled rows, pinning
    /// the appearance-preserving path: a plain row is one default span, so
    /// it draws exactly as a plain-string row would.
    #[test]
    fn plain_builder_rows_are_single_default_spans() {
        for row in session_info_rows(&sample_stats()) {
            assert_eq!(row.len(), 1, "plain row is one span: {row:?}");
            assert_eq!(
                row[0].style,
                Style::default(),
                "plain row default-styled: {row:?}"
            );
        }
    }
}
