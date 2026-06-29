//! Self-contained HTML export of a conversation session.
//!
//! [`render_session_html`] turns a persisted
//! [`ConversationLog`](aj_session::ConversationLog) into a single
//! static HTML document with no external assets and no JavaScript: a
//! transcript you can open in a browser or share as a file.
//!
//! The render is driven by [`aj_session::replay`], the same projection
//! the live TUI consumes, so the page shows the same scrollback a
//! resumed session would: user prompts, assistant text and thinking,
//! tool calls with their structured results, sub-agent runs, and
//! compaction checkpoints. Tool results render from their structured
//! [`ToolDetails`] payload (diffs, command output, todo lists, images)
//! rather than the raw model-facing text, matching what the TUI shows.
//! Long tool output is truncated to a preview, with the remainder
//! behind a `<details>` toggle, mirroring the TUI's collapsed default.
//! The `agent` tool's result is omitted, since the sub-agent box
//! already shows that same final message.
//!
//! The document carries a light and a dark palette inlined as CSS. It
//! follows the OS `prefers-color-scheme` (defaulting to light when none
//! is set), and a CSS-only checkbox toggle flips whichever default is
//! active, so theming needs no script. Collapsible sections use native
//! `<details>`/`<summary>`. The complete session is also embedded as
//! JSON in a `<script type="application/json">` block, so the export
//! stays lossless and a richer client-side viewer can be layered on
//! later without changing the format.

use std::fmt::Write as _;

use aj_agent::events::AgentEvent;
use aj_agent::tool::{TodoStatus, ToolDetails};
use aj_models::types::{AssistantContent, Message, StopReason, UserContent};
use aj_session::{ConversationLog, SessionStats, replay};
use pulldown_cmark::{Event, Options, Parser, html};
use similar::{ChangeTag, TextDiff};

/// Render a whole session to a self-contained HTML document.
///
/// Pure over the log: it reads but never mutates, so it is safe to
/// call while a turn is in flight.
pub(crate) fn render_session_html(log: &ConversationLog) -> String {
    let stats = log.stats();

    // Render the body first so we can lift a `<title>` from the first
    // user prompt before assembling the `<head>`.
    let mut r = Renderer::default();
    for event in replay(log) {
        r.handle(&event);
    }
    r.close_open_sections();

    let title = r
        .title
        .as_deref()
        .map(truncate_title)
        .unwrap_or_else(|| "aj session".to_string());

    let header = render_header(&stats, &title, r.total_cost);
    let raw_json = embed_session_json(log);

    format!(
        "<!DOCTYPE html>\n\
<html lang=\"en\">\n\
<head>\n\
<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<title>{title}</title>\n\
<style>\n{CSS}\n</style>\n\
</head>\n\
<body>\n\
<input type=\"checkbox\" id=\"theme-toggle\">\n\
<label for=\"theme-toggle\" class=\"theme-toggle\" title=\"Toggle light / dark\" aria-label=\"Toggle light or dark theme\">\u{25d0}</label>\n\
<main class=\"container\">\n{header}{body}</main>\n\
<script type=\"application/json\" id=\"session-data\">{raw_json}</script>\n\
</body>\n\
</html>\n",
        title = escape(&title),
        body = r.body,
    )
}

/// Default line budget for collapsed text-style tool output, matching
/// the TUI's `TEXT_COLLAPSED_LINES`. The first lines stay visible and
/// the remainder folds into a `<details>`.
const TEXT_COLLAPSED_LINES: usize = 10;

/// Default line budget per command stream (stdout, stderr) for
/// collapsed `bash` output, matching the TUI's `BASH_COLLAPSED_LINES`.
/// The last lines stay visible, since a stream's tail is what usually
/// matters, and the earlier lines fold into a `<details>`.
const BASH_COLLAPSED_LINES: usize = 5;

/// Which end of a long tool-output block stays visible when collapsed.
/// Mirrors the TUI: text bodies keep their head, streamed command
/// output keeps its tail.
enum Truncate {
    Head(usize),
    Tail(usize),
}

/// Accumulates the transcript body while walking the replayed event
/// stream. Sub-agent runs are bracketed by
/// [`AgentEvent::SubAgentStart`] / [`AgentEvent::SubAgentEnd`], which
/// we mirror as nested `<details>` boxes. `open_sections` tracks the
/// bracket depth so an unbalanced tail (a log truncated mid sub-agent
/// run) still closes cleanly.
#[derive(Default)]
struct Renderer {
    body: String,
    title: Option<String>,
    open_sections: usize,
    /// Summed dollar cost over assistant turns. `SessionStats` does
    /// not carry a cost total, so we accumulate it as we walk.
    total_cost: f64,
}

impl Renderer {
    fn handle(&mut self, event: &AgentEvent) {
        match event {
            // Render finalized messages on `MessageEnd`. Replay emits
            // an empty assistant placeholder on `MessageStart`, so the
            // end event is the authoritative content.
            AgentEvent::MessageEnd { message, .. } => match message.as_wire() {
                Some(Message::User(u)) => self.user_message(&u.content),
                Some(Message::Assistant(a)) => self.assistant_message(a),
                // Tool results render from `ToolExecutionEnd`, which
                // carries the structured `ToolDetails`. Skip the wire
                // message so we don't render the result twice.
                Some(Message::ToolResult(_)) | None => {}
            },
            AgentEvent::ToolExecutionEnd {
                tool,
                result,
                content,
                is_error,
                ..
            } => {
                // A successful `agent` result is the sub-agent's last
                // assistant message verbatim, which already shows as the
                // closing message inside the sub-agent box, so we skip
                // it to avoid repeating the report. A failed run is
                // different: the failure rides an `is_error` result here
                // and the sub-agent box may be empty, so we keep those.
                // Otherwise the failure would render nowhere.
                if tool != "agent" || *is_error {
                    self.tool_result(tool, result, content, *is_error);
                }
            }
            AgentEvent::SubAgentStart { child, task, .. } => {
                let id = match child {
                    aj_agent::events::AgentId::Sub(n) => *n,
                    aj_agent::events::AgentId::Main => 0,
                };
                // The whole sub-agent run lives in one collapsible box,
                // collapsed by default to keep the main thread readable.
                // The summary keeps the id and task visible when closed.
                let _ = write!(
                    self.body,
                    "<details class=\"subagent\"><summary>\
                     <span class=\"sub-head\">sub-agent #{id}</span> \
                     <span class=\"sub-task\">{}</span></summary>",
                    escape(task)
                );
                self.open_sections += 1;
            }
            AgentEvent::SubAgentEnd { .. } => {
                if self.open_sections > 0 {
                    self.body.push_str("</details>");
                    self.open_sections -= 1;
                }
            }
            AgentEvent::Notice { text, .. } => {
                let _ = write!(self.body, "<div class=\"notice\">{}</div>", escape(text));
            }
            AgentEvent::CompactionEnd { summary, .. } => {
                if let Some(summary) = summary {
                    let _ = write!(
                        self.body,
                        "<div class=\"compaction\"><div class=\"compaction-head\">context compacted</div>{}</div>",
                        markdown(summary)
                    );
                }
            }
            // Lifecycle, streaming, usage, and transient task/retry
            // events carry nothing the static transcript shows.
            _ => {}
        }
    }

    fn user_message(&mut self, content: &[UserContent]) {
        if self.title.is_none() {
            self.title = first_text(content);
        }
        self.body
            .push_str("<div class=\"msg user\"><div class=\"role\">User</div>");
        // User prompts are authored prose, rendered as markdown to
        // match the TUI. Tool output, by contrast, is escaped verbatim
        // (see `tool_details`).
        for block in content {
            match block {
                UserContent::Text(t) => self.body.push_str(&markdown(&t.text)),
                UserContent::Image(img) => self.image(&img.mime_type, &img.data),
            }
        }
        self.body.push_str("</div>");
    }

    fn assistant_message(&mut self, a: &aj_models::types::AssistantMessage) {
        self.total_cost += a.usage.cost.total;
        let _ = write!(
            self.body,
            "<div class=\"msg assistant\"><div class=\"role\">Assistant <span class=\"model\">{}</span></div>",
            escape(&a.model)
        );
        for block in &a.content {
            match block {
                AssistantContent::Text(t) => self.body.push_str(&markdown(&t.text)),
                AssistantContent::Thinking(t) if !t.thinking.is_empty() => {
                    let _ = write!(
                        self.body,
                        "<details class=\"thinking\"><summary>Thinking</summary>{}</details>",
                        markdown(&t.thinking)
                    );
                }
                // A signed-but-empty thinking block carries no prose to show.
                AssistantContent::Thinking(_) => {}
                AssistantContent::ToolCall(call) => {
                    let args = serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
                    let _ = write!(
                        self.body,
                        "<div class=\"toolcall\"><span class=\"tool-name\">{}</span>",
                        escape(&call.name)
                    );
                    if !args.is_empty() && args != "{}" {
                        let _ = write!(self.body, "<pre class=\"args\">{}</pre>", escape(&args));
                    }
                    self.body.push_str("</div>");
                }
            }
        }
        // A failed turn records the cause on the message rather than in
        // a content block; surface it so the transcript shows why the
        // turn stopped.
        if matches!(a.stop_reason, StopReason::Error | StopReason::Aborted)
            && let Some(err) = &a.error
        {
            let _ = write!(
                self.body,
                "<div class=\"error\">{}: {}</div>",
                escape(&format!("{:?}", err.category)),
                escape(&err.message)
            );
        }
        self.body.push_str("</div>");
    }

    fn image(&mut self, mime: &str, data: &str) {
        let _ = write!(
            self.body,
            "<img class=\"img\" alt=\"image\" src=\"data:{};base64,{}\">",
            escape(mime),
            escape(data)
        );
    }

    /// Emit a `<pre>` block, optionally classed. `class` is written
    /// into the attribute unescaped, so it must be a trusted literal,
    /// never caller-derived text.
    fn pre(&mut self, text: &str, class: Option<&str>) {
        match class {
            Some(c) => {
                let _ = write!(self.body, "<pre class=\"{c}\">{}</pre>", escape(text));
            }
            None => {
                let _ = write!(self.body, "<pre>{}</pre>", escape(text));
            }
        }
    }

    /// Tool output collapsed to a preview, with the hidden remainder
    /// behind a scriptless `<details>` toggle, mirroring the TUI's
    /// default truncation. `Head` keeps the first lines and folds the
    /// rest below. `Tail` keeps the last lines and folds the earlier
    /// ones above. We split on `\n` and drop one trailing empty line so
    /// the line count matches the TUI's.
    fn output_block(&mut self, text: &str, class: Option<&str>, mode: Truncate) {
        let mut lines: Vec<&str> = text.split('\n').collect();
        if lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        match mode {
            Truncate::Head(limit) if lines.len() > limit => {
                let hidden = lines.len() - limit;
                self.pre(&lines[..limit].join("\n"), class);
                self.fold(&lines[limit..].join("\n"), class, hidden, "more");
            }
            Truncate::Tail(limit) if lines.len() > limit => {
                let split = lines.len() - limit;
                self.fold(&lines[..split].join("\n"), class, split, "earlier");
                self.pre(&lines[split..].join("\n"), class);
            }
            _ => self.pre(&lines.join("\n"), class),
        }
    }

    /// A collapsed `<details>` holding the folded-away lines, labeled
    /// "{count} {word} lines" (e.g. "12 more lines").
    fn fold(&mut self, text: &str, class: Option<&str>, count: usize, word: &str) {
        let _ = write!(
            self.body,
            "<details class=\"more\"><summary>{count} {word} lines</summary>"
        );
        self.pre(text, class);
        self.body.push_str("</details>");
    }

    fn tool_result(
        &mut self,
        tool: &str,
        details: &ToolDetails,
        content: &[UserContent],
        is_error: bool,
    ) {
        let class = if is_error { "tool error" } else { "tool" };
        let _ = write!(
            self.body,
            "<div class=\"{class}\"><div class=\"tool-head\">{}</div>",
            escape(tool)
        );
        self.tool_details(details, content);
        self.body.push_str("</div>");
    }

    fn tool_details(&mut self, details: &ToolDetails, content: &[UserContent]) {
        match details {
            ToolDetails::Text { summary, body } => {
                if !summary.is_empty() {
                    let _ = write!(
                        self.body,
                        "<div class=\"summary\">{}</div>",
                        escape(summary)
                    );
                }
                if !body.is_empty() {
                    self.output_block(body, None, Truncate::Head(TEXT_COLLAPSED_LINES));
                }
            }
            ToolDetails::Diff {
                path,
                before,
                after,
            } => {
                let _ = write!(self.body, "<div class=\"summary\">{}</div>", escape(path));
                self.body.push_str(&render_diff(before, after));
            }
            ToolDetails::Bash {
                command,
                stdout,
                stderr,
                exit_code,
                truncated,
                ..
            } => {
                let _ = write!(self.body, "<pre class=\"cmd\">$ {}</pre>", escape(command));
                if !stdout.is_empty() {
                    self.output_block(stdout, None, Truncate::Tail(BASH_COLLAPSED_LINES));
                }
                if !stderr.is_empty() {
                    self.output_block(stderr, Some("stderr"), Truncate::Tail(BASH_COLLAPSED_LINES));
                }
                if *truncated {
                    self.body
                        .push_str("<div class=\"summary\">[output truncated]</div>");
                }
                if let Some(code) = exit_code
                    && *code != 0
                {
                    let _ = write!(self.body, "<div class=\"summary\">exit code {code}</div>");
                }
            }
            // Unreachable on the production path: the only producer is
            // the `agent` tool, whose successful result is skipped in
            // `handle` because the sub-agent box already shows the
            // report. We keep a faithful rendering for exhaustiveness
            // and in case a future caller routes a report here directly.
            ToolDetails::SubAgentReport { task, report, .. } => {
                let _ = write!(self.body, "<div class=\"summary\">{}</div>", escape(task));
                self.body.push_str(&markdown(report));
            }
            ToolDetails::Todos { items } => {
                self.body.push_str("<ul class=\"todos\">");
                for item in items {
                    let (mark, cls) = match item.status {
                        TodoStatus::Completed => ("[x]", "done"),
                        TodoStatus::InProgress => ("[~]", "doing"),
                        TodoStatus::Todo => ("[ ]", "todo"),
                    };
                    let _ = write!(
                        self.body,
                        "<li class=\"{cls}\">{mark} {}</li>",
                        escape(&item.content)
                    );
                }
                self.body.push_str("</ul>");
            }
            ToolDetails::Image { summary, .. } => {
                let _ = write!(
                    self.body,
                    "<div class=\"summary\">{}</div>",
                    escape(summary)
                );
                // The bytes ride in the tool-result content, not the
                // details, so pull the first image block to embed.
                if let Some(UserContent::Image(img)) =
                    content.iter().find(|c| matches!(c, UserContent::Image(_)))
                {
                    self.image(&img.mime_type, &img.data);
                }
            }
            ToolDetails::Json(value) => {
                let json = serde_json::to_string_pretty(value).unwrap_or_default();
                let _ = write!(self.body, "<pre>{}</pre>", escape(&json));
            }
        }
    }

    fn close_open_sections(&mut self) {
        while self.open_sections > 0 {
            self.body.push_str("</details>");
            self.open_sections -= 1;
        }
    }
}

/// The session header card: title plus a one-line stats summary.
fn render_header(stats: &SessionStats, title: &str, total_cost: f64) -> String {
    let model = stats
        .settings
        .model
        .as_ref()
        .map(|(provider, id)| format!("{provider}/{id}"))
        .unwrap_or_else(|| "unknown model".to_string());

    let cost = if total_cost > 0.0 {
        format!(" \u{b7} ${total_cost:.4}")
    } else {
        String::new()
    };

    format!(
        "<header class=\"session\"><h1>{title}</h1>\
         <div class=\"meta\">{model} \u{b7} {user} prompts \u{b7} {asst} replies \u{b7} {tools} tool calls{cost}</div>\
         <div class=\"meta dim\">{id}</div></header>",
        title = escape(title),
        model = escape(&model),
        user = stats.user_messages,
        asst = stats.assistant_messages,
        tools = stats.tool_calls,
        cost = cost,
        id = escape(&stats.session_id),
    )
}

/// Serialize the full log as a JSON array of entries for the embedded
/// data block. Every `<` is rewritten to its `\u003c` escape so the
/// payload cannot open or close a tag inside the surrounding
/// `<script>` (not just `</script>`, but also `<!--` and `<script`,
/// which flip the HTML script-data tokenizer). In JSON `<` only
/// appears inside string values, where `\u003c` is an equivalent
/// escape, so the data stays valid and inert.
fn embed_session_json(log: &ConversationLog) -> String {
    let entries = log.entries_in_order();
    serde_json::to_string(&entries)
        .unwrap_or_default()
        .replace('<', "\\u003c")
}

/// Render markdown to HTML. Used only for authored prose (user and
/// assistant text, sub-agent reports, compaction summaries).
fn markdown(text: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    // Render raw HTML in the source as visible literal text rather than
    // live markup. A transcript can contain HTML (a pasted snippet, or
    // a prompt-injected `<script>` echoed by the model), and the export
    // is meant to be shared, so we must not let it inject elements into
    // the page. Mirrors the TUI markdown renderer, which routes raw
    // HTML to text for the same reason.
    let parser = Parser::new_ext(text, opts).map(|event| match event {
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// A line-level unified diff rendered as styled HTML, with a 3-line
/// context window around each change. Mirrors the TUI diff view.
fn render_diff(before: &str, after: &str) -> String {
    let diff = TextDiff::from_lines(before, after);
    const CONTEXT: usize = 3;
    let tags: Vec<ChangeTag> = diff.iter_all_changes().map(|c| c.tag()).collect();

    let mut out = String::from("<pre class=\"diff\">");
    let mut last_emitted: Option<usize> = None;
    for (idx, change) in diff.iter_all_changes().enumerate() {
        if matches!(change.tag(), ChangeTag::Equal) && !in_context(&tags, idx, CONTEXT) {
            continue;
        }
        if let Some(last) = last_emitted
            && idx > last + 1
        {
            out.push_str("<span class=\"ctx\">\u{2026}\n</span>");
        }
        last_emitted = Some(idx);

        let value = change.value().trim_end_matches('\n');
        let (cls, sign) = match change.tag() {
            ChangeTag::Delete => ("del", "-"),
            ChangeTag::Insert => ("add", "+"),
            ChangeTag::Equal => ("ctx", " "),
        };
        let _ = write!(
            out,
            "<span class=\"{cls}\">{sign} {}\n</span>",
            escape(value)
        );
    }
    out.push_str("</pre>");
    out
}

/// True if any non-equal change lies within `context` lines of `idx`,
/// used to drop equal lines outside the diff's context window.
fn in_context(tags: &[ChangeTag], idx: usize, context: usize) -> bool {
    let lo = idx.saturating_sub(context);
    let hi = (idx + context).min(tags.len().saturating_sub(1));
    (lo..=hi).any(|i| !matches!(tags[i], ChangeTag::Equal))
}

/// The first text block of a message, used to derive a page title.
fn first_text(content: &[UserContent]) -> Option<String> {
    content.iter().find_map(|c| match c {
        UserContent::Text(t) if !t.text.trim().is_empty() => Some(t.text.clone()),
        _ => None,
    })
}

/// Collapse a prompt to a single-line title, capped at 80 characters.
fn truncate_title(text: &str) -> String {
    let line = text.split('\n').next().unwrap_or(text).trim();
    if line.chars().count() > 80 {
        let truncated: String = line.chars().take(80).collect();
        format!("{truncated}\u{2026}")
    } else {
        line.to_string()
    }
}

/// Escape the five characters that are unsafe in HTML text or
/// double-quoted attribute values.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// The inlined stylesheet, script-free.
///
/// Theme selection is CSS-only. The base `:root` is light, which also
/// covers the "no OS preference" case (browsers report `light` then).
/// A `prefers-color-scheme: dark` media query switches the default to
/// dark when the OS asks for it. The `#theme-toggle` checkbox then
/// *inverts* whichever default is active: it is wired to the opposite
/// palette inside each media query, so one toggle flips light->dark and
/// dark->light without script. The two palettes are duplicated across
/// the rules because plain CSS has no way to alias a whole variable
/// set. Colors are a fixed pair rather than derived from the live TUI
/// theme, which is a follow-up.
///
/// NOTE: the toggle overrides live inside the two media queries, so on
/// a browser that reports neither preference (or lacks `:has()`) the
/// page falls back to the static light/OS theme and the toggle is
/// inert. Every engine new enough for `:has()` reports `light` when no
/// preference is set, so this only bites truly ancient browsers.
const CSS: &str = "\
:root{--bg:#fbfbfd;--panel:#ffffff;--panel2:#f0f1f4;--fg:#1f2328;--muted:#656d76;\
--border:#d6d9df;--user:#0969da;--assistant:#1a7f37;--tool:#8a5d00;--add:#1a7f37;\
--del:#cf222e;--err:#cf222e;--link:#0969da;--sub-bg:#f3f6fd}\
@media (prefers-color-scheme:dark){\
:root{--bg:#1e1e24;--panel:#26262e;--panel2:#2b2b34;--fg:#e6e6e6;--muted:#9aa0aa;\
--border:#3a3a44;--user:#7aa2f7;--assistant:#9ece6a;--tool:#e0af68;--add:#9ece6a;\
--del:#f7768e;--err:#f7768e;--link:#7dcfff;--sub-bg:rgba(122,162,247,.06)}\
:root:has(#theme-toggle:checked){--bg:#fbfbfd;--panel:#ffffff;--panel2:#f0f1f4;\
--fg:#1f2328;--muted:#656d76;--border:#d6d9df;--user:#0969da;--assistant:#1a7f37;\
--tool:#8a5d00;--add:#1a7f37;--del:#cf222e;--err:#cf222e;--link:#0969da;--sub-bg:#f3f6fd}}\
@media (prefers-color-scheme:light){\
:root:has(#theme-toggle:checked){--bg:#1e1e24;--panel:#26262e;--panel2:#2b2b34;\
--fg:#e6e6e6;--muted:#9aa0aa;--border:#3a3a44;--user:#7aa2f7;--assistant:#9ece6a;\
--tool:#e0af68;--add:#9ece6a;--del:#f7768e;--err:#f7768e;--link:#7dcfff;\
--sub-bg:rgba(122,162,247,.06)}}\
*{box-sizing:border-box}\
body{margin:0;background:var(--bg);color:var(--fg);font-size:15px;line-height:1.55;\
font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif}\
.container{max-width:860px;margin:0 auto;padding:24px 20px 80px}\
a{color:var(--link)}\
.theme-toggle{position:fixed;top:14px;right:14px;z-index:10;cursor:pointer;user-select:none;\
border:1px solid var(--border);background:var(--panel);color:var(--fg);border-radius:6px;\
padding:3px 9px;font-size:15px;line-height:1.4;box-shadow:0 1px 2px rgba(0,0,0,.12)}\
#theme-toggle{position:fixed;top:0;left:0;width:1px;height:1px;opacity:0;margin:0}\
#theme-toggle:focus-visible+.theme-toggle{outline:2px solid var(--link);outline-offset:2px}\
pre{background:var(--panel2);border:1px solid var(--border);border-radius:6px;padding:10px 12px;\
overflow-x:auto;white-space:pre-wrap;word-break:break-word;font-size:13px;\
font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}\
code{background:var(--panel2);border-radius:4px;padding:1px 4px;font-size:13px;\
font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}\
pre code{background:none;padding:0}\
.session{border-bottom:1px solid var(--border);padding-bottom:16px;margin-bottom:24px}\
.session h1{font-size:20px;margin:0 0 8px}\
.meta{color:var(--muted);font-size:13px}\
.meta.dim{opacity:.6;margin-top:4px}\
.msg{margin:18px 0;padding:12px 16px;border-radius:8px;background:var(--panel);\
border:1px solid var(--border)}\
.role{font-weight:600;font-size:12px;text-transform:uppercase;letter-spacing:.05em;margin-bottom:6px}\
.msg.user .role{color:var(--user)}\
.msg.assistant .role{color:var(--assistant)}\
.role .model{font-weight:400;text-transform:none;letter-spacing:0;color:var(--muted)}\
.toolcall{margin:10px 0 0;color:var(--tool);font-size:13px;\
font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}\
.tool-name{font-weight:600}\
.toolcall .args{margin-top:6px;color:var(--fg)}\
.tool{margin:10px 0 0;padding:8px 12px;border-left:3px solid var(--tool);background:var(--panel2);\
border-radius:0 6px 6px 0}\
.tool.error{border-left-color:var(--err)}\
.tool-head{font-weight:600;font-size:12px;color:var(--tool);margin-bottom:6px;\
font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}\
.tool.error .tool-head{color:var(--err)}\
.summary{color:var(--muted);font-size:13px;margin:4px 0}\
details.more{margin:4px 0}\
details.more>summary{cursor:pointer;color:var(--muted);font-size:12px;font-style:italic}\
details.more>summary::marker{font-style:normal}\
pre.cmd{color:var(--tool)}\
pre.stderr{color:var(--del)}\
pre.diff .add{color:var(--add)}\
pre.diff .del{color:var(--del)}\
pre.diff .ctx{color:var(--muted)}\
.todos{list-style:none;padding-left:0;margin:6px 0;font-size:14px}\
.todos li{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}\
.todos .done{color:var(--muted);text-decoration:line-through}\
.todos .doing{color:var(--tool)}\
details.thinking{margin:8px 0;color:var(--muted)}\
details.thinking summary{cursor:pointer;font-size:13px}\
.notice{color:var(--muted);font-size:12px;text-align:center;margin:14px 0;font-style:italic}\
.compaction{margin:18px 0;padding:10px 14px;border:1px dashed var(--border);border-radius:8px;\
color:var(--muted);font-size:13px}\
.compaction-head{text-transform:uppercase;font-size:11px;letter-spacing:.05em;margin-bottom:6px}\
.error{color:var(--err);font-size:13px;margin-top:8px}\
.img{max-width:100%;border-radius:6px;margin:8px 0}\
details.subagent{margin:14px 0;padding:10px 14px;border:1px solid var(--border);\
border-left:3px solid var(--user);border-radius:0 8px 8px 0;background:var(--sub-bg)}\
details.subagent>summary{cursor:pointer;font-weight:600;font-size:12px}\
details.subagent[open]>summary{margin-bottom:10px}\
.sub-head{color:var(--user);text-transform:uppercase;letter-spacing:.05em}\
.sub-task{color:var(--muted);font-weight:400;font-size:13px}";

#[cfg(test)]
mod tests {
    use std::fs;

    use aj_session::{ConversationLog, ConversationPersistence};
    use tempfile::tempdir;

    use super::*;

    /// Open a log from a JSONL fixture written into a temp sessions
    /// directory, exercising the same `resume` path the binary uses.
    fn log_from_jsonl(lines: &[&str]) -> (tempfile::TempDir, ConversationLog) {
        let dir = tempdir().expect("tempdir");
        let id = "test-session";
        fs::write(dir.path().join(format!("{id}.jsonl")), lines.join("\n")).expect("write fixture");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let log = ConversationLog::resume(&persistence, id).expect("resume fixture");
        (dir, log)
    }

    const SYSTEM: &str = r#"{"id":"root0001","timestamp":"2024-01-01T00:00:00Z","thread":"meta","type":"system_prompt","text":"You are aj."}"#;
    const USER: &str = r#"{"id":"u0000001","parent_id":"root0001","timestamp":"2024-01-01T00:00:01Z","thread":"user","type":"message","message":{"role":"user","content":[{"type":"text","text":"Hello **world**"}],"timestamp":1704067201000}}"#;
    const ASSISTANT: &str = r#"{"id":"a0000001","parent_id":"u0000001","timestamp":"2024-01-01T00:00:02Z","thread":"user","type":"message","message":{"role":"assistant","content":[{"type":"text","text":"Reading the file."},{"type":"tool_call","id":"call-1","name":"read_file","arguments":{"path":"/tmp/x"}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-test","usage":{"input":10,"output":5,"cache_read":0,"cache_write":0,"total_tokens":15,"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0}},"stop_reason":"ToolUse","timestamp":1704067202000}}"#;
    const TOOL_RESULT: &str = r#"{"id":"t0000001","parent_id":"a0000001","timestamp":"2024-01-01T00:00:03Z","thread":"user","type":"message","message":{"role":"tool_result","tool_call_id":"call-1","tool_name":"read_file","content":[{"type":"text","text":"the file body"}],"details":{"kind":"text","summary":"read_file /tmp/x","body":"the file body"},"is_error":false,"timestamp":1704067203000}}"#;

    const BASH_ASSISTANT: &str = r#"{"id":"ab000001","parent_id":"u0000001","thread":"user","type":"message","message":{"role":"assistant","content":[{"type":"tool_call","id":"call-b","name":"bash","arguments":{"command":"ls"}}],"api":"x","provider":"anthropic","model":"claude-test","usage":{"input":0,"output":0,"cache_read":0,"cache_write":0,"total_tokens":0,"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0}},"stop_reason":"ToolUse","timestamp":1704067202000}}"#;
    const BASH_RESULT: &str = r#"{"id":"tb000001","parent_id":"ab000001","thread":"user","type":"message","message":{"role":"tool_result","tool_call_id":"call-b","tool_name":"bash","content":[{"type":"text","text":"x"}],"details":{"kind":"bash","command":"ls /tmp","stdout":"file-a","stderr":"permission denied","exit_code":2,"truncated":false},"is_error":true,"timestamp":1704067203000}}"#;

    const ERROR_ASSISTANT: &str = r#"{"id":"ae000001","parent_id":"u0000001","thread":"user","type":"message","message":{"role":"assistant","content":[],"api":"x","provider":"anthropic","model":"claude-test","usage":{"input":0,"output":0,"cache_read":0,"cache_write":0,"total_tokens":0,"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0}},"stop_reason":"Error","error":{"category":"rate_limit","message":"slow down"},"timestamp":1704067202000}}"#;

    const SUB_CALL: &str = r#"{"id":"as000001","parent_id":"u0000001","thread":"user","type":"message","message":{"role":"assistant","content":[{"type":"tool_call","id":"call-agent","name":"agent","arguments":{"task":"investigate"}}],"api":"x","provider":"anthropic","model":"claude-test","usage":{"input":0,"output":0,"cache_read":0,"cache_write":0,"total_tokens":0,"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0}},"stop_reason":"ToolUse","timestamp":1704067202000}}"#;
    const SUB_SPAWN: &str = r#"{"id":"ss000001","parent_id":"as000001","thread":"subagent","agent_id":1,"type":"sub_agent_spawn","task":"investigate the bug","settings":{"provider":"anthropic","model_id":"claude-test","thinking":"off","speed":"standard","verbosity":""}}"#;
    const SUB_MSG: &str = r#"{"id":"sm000001","parent_id":"ss000001","thread":"subagent","agent_id":1,"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"sub agent finding"}],"api":"x","provider":"anthropic","model":"claude-test","usage":{"input":0,"output":0,"cache_read":0,"cache_write":0,"total_tokens":0,"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0}},"stop_reason":"Stop","timestamp":1704067203000}}"#;
    const SUB_RESULT: &str = r#"{"id":"ts000001","parent_id":"as000001","thread":"user","type":"message","message":{"role":"tool_result","tool_call_id":"call-agent","tool_name":"agent","content":[{"type":"text","text":"report"}],"details":{"kind":"sub_agent_report","agent_id":1,"task":"investigate the bug","report":"final report text"},"is_error":false,"timestamp":1704067204000}}"#;
    const SUB_FAIL_RESULT: &str = r#"{"id":"ts000002","parent_id":"as000001","thread":"user","type":"message","message":{"role":"tool_result","tool_call_id":"call-agent","tool_name":"agent","content":[{"type":"text","text":"sub-agent failed: boom"}],"details":{"kind":"text","summary":"agent: error","body":"sub-agent failed: boom"},"is_error":true,"timestamp":1704067204000}}"#;

    #[test]
    fn escapes_html_special_chars() {
        assert_eq!(escape("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&#39;");
    }

    #[test]
    fn markdown_renders_inline_styles() {
        let html = markdown("plain **bold** text");
        assert!(html.contains("<strong>bold</strong>"), "got: {html}");
    }

    #[test]
    fn diff_marks_added_and_removed_lines() {
        let html = render_diff("one\ntwo\n", "one\nthree\n");
        assert!(html.contains("class=\"del\">- two"), "got: {html}");
        assert!(html.contains("class=\"add\">+ three"), "got: {html}");
    }

    #[test]
    fn renders_full_transcript() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ASSISTANT, TOOL_RESULT]);
        let html = render_session_html(&log);

        // Structural shell.
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<script type=\"application/json\" id=\"session-data\">"));

        // User prompt: markdown-rendered, and lifted into the title.
        assert!(
            html.contains("<strong>world</strong>"),
            "user markdown missing"
        );
        assert!(
            html.contains("<title>Hello **world**</title>"),
            "title not derived"
        );

        // Assistant text, model label, and the tool call.
        assert!(html.contains("Reading the file."));
        assert!(html.contains("claude-test"));
        assert!(html.contains(">read_file</span>"));

        // Tool result rendered from structured details, not skipped.
        assert!(html.contains("read_file /tmp/x"));
        assert!(html.contains("the file body"));
    }

    #[test]
    fn title_falls_back_without_user_prompt() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM]);
        let html = render_session_html(&log);
        assert!(html.contains("<title>aj session</title>"));
    }

    #[test]
    fn script_island_neutralizes_angle_brackets() {
        // Tag-like sequences in a prompt must not be able to open or
        // close a tag inside the embedded JSON island.
        let user = r#"{"id":"u0000001","parent_id":"root0001","thread":"user","type":"message","message":{"role":"user","content":[{"type":"text","text":"</script><!--<script>x"}],"timestamp":1704067201000}}"#;
        let (_dir, log) = log_from_jsonl(&[SYSTEM, user]);
        let html = render_session_html(&log);
        let data = html
            .split_once("id=\"session-data\">")
            .and_then(|(_, rest)| rest.split_once("</script>"))
            .map(|(payload, _)| payload)
            .expect("data block present");
        assert!(!data.contains('<'), "raw '<' leaked into the JSON island");
        assert!(data.contains("\\u003c"), "angle bracket not neutralized");
    }

    #[test]
    fn neutralizes_raw_html_in_prose() {
        // Raw HTML in a prompt or reply must render as inert text, not
        // live markup, because the export is meant to be shared.
        let user = r#"{"id":"u0000001","parent_id":"root0001","thread":"user","type":"message","message":{"role":"user","content":[{"type":"text","text":"<img src=x onerror=alert(1)> then <script>alert(2)</script>"}],"timestamp":1704067201000}}"#;
        let (_dir, log) = log_from_jsonl(&[SYSTEM, user]);
        let html = render_session_html(&log);
        let body = html
            .split_once("<main")
            .and_then(|(_, rest)| rest.split_once("</main>"))
            .map(|(body, _)| body)
            .expect("body present");
        assert!(
            !body.contains("<img src=x onerror"),
            "raw img tag survived as live markup"
        );
        assert!(
            !body.contains("<script>alert(2)"),
            "raw script survived as live markup"
        );
        assert!(body.contains("&lt;script&gt;"), "html not escaped to text");
    }

    #[test]
    fn renders_bash_tool_result() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, BASH_ASSISTANT, BASH_RESULT]);
        let html = render_session_html(&log);
        assert!(html.contains("$ ls /tmp"), "command line missing");
        assert!(html.contains("file-a"), "stdout missing");
        assert!(html.contains("permission denied"), "stderr missing");
        assert!(html.contains("exit code 2"), "nonzero exit code missing");
        assert!(
            html.contains("class=\"tool error\""),
            "error styling missing"
        );
    }

    #[test]
    fn renders_error_turn() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ERROR_ASSISTANT]);
        let html = render_session_html(&log);
        assert!(html.contains("class=\"error\""), "error block missing");
        assert!(html.contains("slow down"), "error message missing");
    }

    #[test]
    fn renders_subagent_section() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, SUB_CALL, SUB_SPAWN, SUB_MSG, SUB_RESULT]);
        let html = render_session_html(&log);
        assert!(
            html.contains("<details class=\"subagent\""),
            "subagent box missing"
        );
        assert!(html.contains("sub-agent #1"));
        assert!(html.contains("investigate the bug"), "task missing");
        assert!(
            html.contains("sub agent finding"),
            "nested transcript missing"
        );
        assert_eq!(
            html.matches("<details").count(),
            html.matches("</details>").count(),
            "unbalanced details boxes"
        );

        // The `agent` tool result repeats the sub-agent's last message,
        // so the transcript body must not render it a second time. (It
        // still lives in the embedded JSON island, hence the body-only
        // check.)
        let body = html
            .split_once("<main")
            .and_then(|(_, rest)| rest.split_once("</main>"))
            .map(|(body, _)| body)
            .expect("body present");
        assert!(
            !body.contains("final report text"),
            "agent report rendered twice"
        );
    }

    #[test]
    fn failed_agent_run_surfaces_the_error() {
        // A failed sub-agent run carries the failure on an `is_error`
        // result for the `agent` tool, and the sub-agent box may be
        // empty, so the exporter must keep that result rather than skip
        // it the way it skips a successful (duplicate) report.
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, SUB_CALL, SUB_SPAWN, SUB_FAIL_RESULT]);
        let html = render_session_html(&log);
        let body = html
            .split_once("<main")
            .and_then(|(_, rest)| rest.split_once("</main>"))
            .map(|(body, _)| body)
            .expect("body present");
        assert!(
            body.contains("class=\"tool error\""),
            "failure not rendered"
        );
        assert!(
            body.contains("sub-agent failed: boom"),
            "error message missing"
        );
    }

    #[test]
    fn head_truncation_folds_remainder() {
        let mut r = Renderer::default();
        let body = (1..=12)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        r.output_block(&body, None, Truncate::Head(TEXT_COLLAPSED_LINES));

        let (preview, folded) = r.body.split_once("<details").expect("fold present");
        assert!(preview.contains("line10"), "10th line should stay visible");
        assert!(!preview.contains("line11"), "11th line should be folded");
        assert!(
            folded.contains("<summary>2 more lines</summary>"),
            "fold label wrong: {folded}"
        );
        assert!(
            folded.contains("line11") && folded.contains("line12"),
            "folded lines missing"
        );
    }

    #[test]
    fn tail_truncation_folds_earlier_lines() {
        let mut r = Renderer::default();
        let out = (1..=8)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        r.output_block(&out, None, Truncate::Tail(BASH_COLLAPSED_LINES));

        let (fold, tail) = r.body.split_once("</details>").expect("fold present");
        assert!(
            fold.contains("<summary>3 earlier lines</summary>"),
            "fold label wrong: {fold}"
        );
        assert!(
            fold.contains("line3") && !fold.contains("line4"),
            "earlier lines mis-split"
        );
        assert!(
            tail.contains("line4") && tail.contains("line8"),
            "tail lines should stay visible"
        );
    }

    #[test]
    fn short_output_is_not_folded() {
        let mut r = Renderer::default();
        r.output_block("only\ntwo\n", None, Truncate::Head(TEXT_COLLAPSED_LINES));
        assert!(!r.body.contains("<details"), "short output should not fold");
        assert!(r.body.contains("<pre>only\ntwo</pre>"), "plain pre missing");
    }

    #[test]
    fn exactly_at_limit_does_not_fold() {
        // The fold triggers on `len > limit`, so output of exactly the
        // limit stays whole, matching the TUI.
        let mut r = Renderer::default();
        let ten = (1..=10)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        r.output_block(&ten, None, Truncate::Head(TEXT_COLLAPSED_LINES));
        assert!(!r.body.contains("<details"), "head at limit must not fold");

        let mut r = Renderer::default();
        let five = (1..=5)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        r.output_block(&five, None, Truncate::Tail(BASH_COLLAPSED_LINES));
        assert!(!r.body.contains("<details"), "tail at limit must not fold");
    }

    #[test]
    fn trailing_newline_is_not_counted() {
        // 11 content lines plus a trailing newline. The empty trailing
        // line is dropped before counting, so exactly one line folds.
        // If the pop were missing we'd see "2 more lines".
        let mut r = Renderer::default();
        let lines = (1..=11)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        r.output_block(
            &format!("{lines}\n"),
            None,
            Truncate::Head(TEXT_COLLAPSED_LINES),
        );
        assert!(
            r.body.contains("<summary>1 more lines</summary>"),
            "trailing newline mis-counted: {}",
            r.body
        );
    }

    #[test]
    fn folded_content_is_escaped() {
        // The hidden remainder must be escaped like the preview, so a
        // tool that echoes markup cannot inject it through the fold. The
        // folded `<pre>` must also keep its stream class.
        let mut r = Renderer::default();
        let rest = (1..=10)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        // Tail folds the earlier lines, so put the markup up front.
        r.output_block(
            &format!("<script>x</script>\n{rest}"),
            Some("stderr"),
            Truncate::Tail(BASH_COLLAPSED_LINES),
        );
        assert!(!r.body.contains("<script>x"), "raw markup survived in fold");
        assert!(
            r.body.contains("&lt;script&gt;x"),
            "fold content not escaped"
        );
        assert!(
            r.body.contains("<pre class=\"stderr\">"),
            "stream class lost in fold"
        );
    }

    #[test]
    fn theme_follows_os_with_scriptless_toggle() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER]);
        let html = render_session_html(&log);
        assert!(
            html.contains("id=\"theme-toggle\""),
            "toggle control missing"
        );
        assert!(
            html.contains("class=\"theme-toggle\""),
            "toggle label missing"
        );
        // Light is the base default, which also covers no OS preference.
        assert!(
            html.contains(":root{--bg:#fbfbfd"),
            "light is not the default"
        );
        // OS preference is honored, and the toggle inverts it, both via CSS.
        assert!(
            html.contains("@media (prefers-color-scheme:dark)"),
            "dark OS preference not honored"
        );
        assert!(
            html.contains("@media (prefers-color-scheme:light)"),
            "toggle-to-dark rule missing"
        );
        assert!(
            html.contains(":root:has(#theme-toggle:checked)"),
            "toggle override missing"
        );
        // The two palettes are duplicated across the cascade (plain CSS
        // can't alias a variable set); guard the copy count so a dropped
        // or added block is caught. Light: base + dark-checked. Dark:
        // dark-default + light-checked.
        assert_eq!(
            CSS.matches("--bg:#fbfbfd").count(),
            2,
            "light palette block count drifted"
        );
        assert_eq!(
            CSS.matches("--bg:#1e1e24").count(),
            2,
            "dark palette block count drifted"
        );
        // Still no JavaScript: the only <script> is the inert JSON island.
        assert_eq!(html.matches("<script").count(), 1, "unexpected script tag");
    }

    #[test]
    fn subagent_details_stay_balanced_when_nested_and_truncated() {
        use aj_agent::events::{AgentId, AgentSettings};

        let settings = AgentSettings {
            provider: String::new(),
            model_id: String::new(),
            thinking: "off".to_string(),
            speed: "standard".to_string(),
            verbosity: String::new(),
        };
        // Two nested runs, only the inner one closed: the outer run is
        // left open, as a log truncated mid-run would leave it, so
        // `close_open_sections` has to finish the job.
        let mut r = Renderer::default();
        r.handle(&AgentEvent::SubAgentStart {
            parent: AgentId::Main,
            child: AgentId::Sub(1),
            task: "outer".to_string(),
            settings: settings.clone(),
        });
        r.handle(&AgentEvent::SubAgentStart {
            parent: AgentId::Sub(1),
            child: AgentId::Sub(2),
            task: "inner".to_string(),
            settings,
        });
        r.handle(&AgentEvent::SubAgentEnd {
            parent: AgentId::Sub(1),
            child: AgentId::Sub(2),
            report: "done".to_string(),
        });
        r.close_open_sections();

        assert_eq!(r.open_sections, 0, "sections not fully closed");
        assert_eq!(
            r.body.matches("<details").count(),
            r.body.matches("</details>").count(),
            "unbalanced details boxes"
        );
        assert_eq!(
            r.body.matches("<details").count(),
            2,
            "expected two sub-agent boxes"
        );
    }
}
