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
pub(crate) fn usage_rows(statuses: &[ProviderUsageStatus]) -> Vec<Row> {
    let now_ms = now_unix_ms();
    let mut rows = Vec::new();
    for status in statuses {
        let id = status.provider_id.as_str();
        let mut prefix = id.to_string();
        let mut push = |rows: &mut Vec<Row>, label: &str, detail: Option<&str>| {
            let mut line = format!("{prefix:<12}  {label}");
            if let Some(detail) = detail {
                line.push_str(&format!("  {detail}"));
            }
            rows.push(plain(line));
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

/// One session-info row: a section header, a key/value pair, or a blank
/// spacer.
enum InfoRow {
    Header(String),
    Kv { key: String, value: String },
    Blank,
}

fn kv(key: &str, value: &str) -> InfoRow {
    InfoRow::Kv {
        key: key.to_string(),
        value: value.to_string(),
    }
}

/// Session-info rows: identity, recorded settings, activity, message
/// counts, aggregate usage, and the per-tool call breakdown. Ported from
/// `aj`'s session-info overlay so both frontends show the same digest.
pub(crate) fn session_info_rows(stats: &SessionStats) -> Vec<Row> {
    let total_messages = stats.user_messages + stats.assistant_messages + stats.tool_results;

    let mut rows: Vec<InfoRow> = vec![
        InfoRow::Header("Session".to_string()),
        kv("id", &stats.session_id),
        kv("file", &stats.path.display().to_string()),
        kv("project", &project_name(stats)),
        InfoRow::Blank,
        InfoRow::Header("Settings".to_string()),
        kv("model", &model_label(stats)),
        kv(
            "thinking",
            stats.settings.thinking.as_deref().unwrap_or("(default)"),
        ),
        kv(
            "speed",
            stats.settings.speed.as_deref().unwrap_or("(default)"),
        ),
        kv(
            "verbosity",
            stats.settings.verbosity.as_deref().unwrap_or("(default)"),
        ),
        InfoRow::Blank,
        InfoRow::Header("Activity".to_string()),
        kv("created", &timestamp(stats.created_at, "(unknown)")),
        kv("last activity", &timestamp(stats.last_activity, "(none)")),
        kv("size on disk", &size_label(stats.size_bytes)),
        InfoRow::Blank,
        InfoRow::Header("Messages".to_string()),
        kv("total", &total_messages.to_string()),
        kv("user", &stats.user_messages.to_string()),
        kv("assistant", &stats.assistant_messages.to_string()),
        kv("tool results", &stats.tool_results.to_string()),
        kv("sub-agents", &stats.subagents.to_string()),
        kv("compactions", &stats.compactions.to_string()),
        kv("log entries", &stats.total_entries.to_string()),
        InfoRow::Blank,
        InfoRow::Header("Usage".to_string()),
        kv("input", &stats.usage.input.to_string()),
        kv("output", &stats.usage.output.to_string()),
        kv("cache read", &stats.usage.cache_read.to_string()),
        kv("cache write", &stats.usage.cache_write.to_string()),
        kv("total tokens", &stats.usage.total_tokens.to_string()),
        kv("cost", &cost_label(stats.usage.cost.total)),
        InfoRow::Blank,
        InfoRow::Header(format!("Tool calls ({})", stats.tool_calls)),
    ];

    if stats.tool_call_counts.is_empty() {
        rows.push(kv("(none)", ""));
    } else {
        for (name, count) in &stats.tool_call_counts {
            rows.push(kv(name, &count.to_string()));
        }
    }

    render_info_rows(&rows)
}

/// Align every key/value pair against one shared key column so values
/// line up across sections.
fn render_info_rows(rows: &[InfoRow]) -> Vec<Row> {
    let key_width = rows
        .iter()
        .filter_map(|row| match row {
            InfoRow::Kv { key, .. } => Some(key.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|row| match row {
            InfoRow::Header(title) => plain(title.clone()),
            InfoRow::Kv { key, value } => plain(format!("  {key:<key_width$}  {value}")),
            InfoRow::Blank => plain(""),
        })
        .collect()
}

/// Project name = the per-project sessions directory the file lives in
/// (`~/.aj/sessions/<project>/<id>.jsonl`). Derived from the path since
/// the log itself does not carry it.
fn project_name(stats: &SessionStats) -> String {
    stats
        .path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("(unknown)")
        .to_string()
}

fn model_label(stats: &SessionStats) -> String {
    match &stats.settings.model {
        Some((provider, model_id)) => format!("{provider} / {model_id}"),
        None => "(unset)".to_string(),
    }
}

fn timestamp(value: Option<chrono::DateTime<chrono::Utc>>, fallback: &str) -> String {
    match value {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => fallback.to_string(),
    }
}

fn size_label(bytes: Option<u64>) -> String {
    match bytes {
        None => "(not written yet)".to_string(),
        Some(b) if b < 1024 => format!("{b} B"),
        Some(b) if b < 1024 * 1024 => format!("{} KB", b / 1024),
        Some(b) => format!("{} MB", b / (1024 * 1024)),
    }
}

/// Aggregate session cost as a dollar figure, four decimals so a
/// sub-cent session still shows a non-zero amount.
fn cost_label(total: f64) -> String {
    format!("${total:.4}")
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
        let rows = rows_text(&usage_rows(&[
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
        ]));
        assert!(rows.contains("anthropic"), "{rows}");
        assert!(rows.contains("5-hour"), "{rows}");
        assert!(rows.contains("50% used"), "{rows}");
        assert!(rows.contains("not configured"), "{rows}");
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
        let rows = rows_text(&session_info_rows(&sample_stats()));
        assert!(rows.contains("2026-06-19-14-22-03-512"), "{rows}");
        assert!(rows.contains("home-u-proj"), "{rows}");
        assert!(rows.contains("anthropic / claude-sonnet-4-5"), "{rows}");
        assert!(rows.contains("48 KB"), "{rows}");
        assert!(rows.contains("read_file"), "{rows}");
        assert!(rows.contains("Tool calls (31)"), "{rows}");
        assert!(rows.contains("total tokens"), "{rows}");
        assert!(rows.contains("$0.3300"), "{rows}");
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
