//! HTML rendered to the browser when an OAuth callback completes.
//!
//! Both Anthropic and OpenAI bind a localhost HTTP
//! server, wait for the upstream provider to redirect a browser back
//! to it with `?code=...&state=...`, and then return a small page so
//! the user knows to switch back to the terminal. We render that page
//! here, in one place, so every provider's callback uses the same
//! visual language.
//!
//! The pages are intentionally minimal: a single heading, a status
//! message, and an optional details block. We embed the styling
//! inline so the page renders correctly when the user's browser has
//! no network connection back to us beyond this single response.
//!
//! These functions are pure string builders — they take user-supplied
//! text and produce HTML escaped to be safe to embed. They make no
//! HTTP calls and have no side effects, so they're trivial to unit
//! test.

/// HTML-escape a string so it can be safely interpolated into element
/// content or a quoted attribute value.
///
/// Covers the five characters that have special meaning in HTML
/// contexts: `&`, `<`, `>`, `"`, `'`. We escape `'` as the numeric
/// reference `&#39;` rather than the named `&apos;` because the
/// latter is not part of HTML 4 and a small minority of legacy
/// browsers don't recognise it.
fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
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

/// Render the shared page chrome with the supplied heading, message,
/// and optional details. Used by both [`success_page`] and
/// [`error_page`]; centralised here so the styling stays consistent.
fn render_page(title: &str, heading: &str, message: &str, details: Option<&str>) -> String {
    let title = escape_html(title);
    let heading = escape_html(heading);
    let message = escape_html(message);
    let details_block = match details {
        Some(d) if !d.is_empty() => {
            format!("\n    <div class=\"details\">{}</div>", escape_html(d))
        }
        _ => String::new(),
    };

    // Dark, centred, mono-detail styling. Kept self-contained so the
    // page works without any external CSS the browser might not be
    // able to fetch (firewalls, ad blockers, etc.).
    format!(
        "<!doctype html>\n\
<html lang=\"en\">\n\
<head>\n\
  <meta charset=\"utf-8\" />\n\
  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n\
  <title>{title}</title>\n\
  <style>\n\
    :root {{\n\
      --text: #fafafa;\n\
      --text-dim: #a1a1aa;\n\
      --page-bg: #09090b;\n\
      --font-sans: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, \"Segoe UI\", Roboto, sans-serif;\n\
      --font-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;\n\
    }}\n\
    * {{ box-sizing: border-box; }}\n\
    html {{ color-scheme: dark; }}\n\
    body {{\n\
      margin: 0;\n\
      min-height: 100vh;\n\
      display: flex;\n\
      align-items: center;\n\
      justify-content: center;\n\
      padding: 24px;\n\
      background: var(--page-bg);\n\
      color: var(--text);\n\
      font-family: var(--font-sans);\n\
      text-align: center;\n\
    }}\n\
    main {{ width: 100%; max-width: 560px; }}\n\
    h1 {{ margin: 0 0 10px; font-size: 28px; font-weight: 650; }}\n\
    p {{ margin: 0; line-height: 1.7; color: var(--text-dim); font-size: 15px; }}\n\
    .details {{ margin-top: 16px; font-family: var(--font-mono); font-size: 13px; color: var(--text-dim); white-space: pre-wrap; word-break: break-word; }}\n\
  </style>\n\
</head>\n\
<body>\n\
  <main>\n\
    <h1>{heading}</h1>\n\
    <p>{message}</p>{details_block}\n\
  </main>\n\
</body>\n\
</html>"
    )
}

/// Render the success page shown when the OAuth callback hits the
/// expected route with a valid `code` + `state` pair.
pub fn success_page(message: &str) -> String {
    render_page(
        "Authentication successful",
        "Authentication successful",
        message,
        None,
    )
}

/// Render the error page shown when the callback indicates a problem
/// (route not found, missing parameters, server-side `error=` query
/// param, etc.). `details` is optional extra context such as the
/// upstream error code.
pub fn error_page(message: &str, details: Option<&str>) -> String {
    render_page(
        "Authentication failed",
        "Authentication failed",
        message,
        details,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Successful page contains the user-supplied message and the
    /// expected heading. Smoke-test for the happy path.
    #[test]
    fn success_page_includes_message_and_heading() {
        let html = success_page("All done!");
        assert!(html.contains("<h1>Authentication successful</h1>"));
        assert!(html.contains("All done!"));
        assert!(html.contains("<title>Authentication successful</title>"));
    }

    /// Error page renders both the message and the optional details
    /// block, and skips the details block when none is provided.
    #[test]
    fn error_page_renders_optional_details() {
        let with = error_page("Login failed.", Some("Code: 42"));
        assert!(with.contains("Login failed."));
        assert!(with.contains(r#"<div class="details">Code: 42</div>"#));

        let without = error_page("Login failed.", None);
        assert!(without.contains("Login failed."));
        assert!(!without.contains("class=\"details\""));
    }

    /// User-supplied text must be HTML-escaped so a malicious or
    /// confused upstream error message can't inject scripts or break
    /// out of the page chrome.
    #[test]
    fn user_text_is_html_escaped() {
        let html = error_page("<script>alert('xss')</script>", Some("\">injection</div>"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("alert('xss')"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&quot;&gt;injection&lt;/div&gt;"));
    }

    /// Empty details strings should be treated the same as `None`:
    /// don't emit an empty `<div class="details">` block.
    #[test]
    fn empty_details_collapses_to_none() {
        let html = error_page("Login failed.", Some(""));
        assert!(!html.contains("class=\"details\""));
    }
}
