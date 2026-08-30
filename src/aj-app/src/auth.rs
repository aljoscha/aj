//! Binary-side authentication helpers.
//!
//! The credential engine ([`aj_models::auth::AuthStorage`], the OAuth
//! flows) lives in `aj-models`; this module holds the pieces that are
//! specifically about the *binary's* UX around it:
//!
//! - [`collect_statuses`] / [`provider_status`] turn the stored
//!   credentials, env vars, and runtime overrides into human-readable
//!   rows for the `/auth` status overlay and the login/logout pickers.
//! - [`open_browser`] best-effort launches the user's browser at the
//!   OAuth authorization URL during a login flow.
//! - [`auth_lines`] composes the login dialog's authorization-step
//!   wording (shared by both frontends so the copy stays identical).
//!
//! The interactive login dialog widget and the [`OAuthCallbacks`]
//! implementation that drives it live in each frontend's login-dialog
//! component; only the frontend-agnostic wording lives here.
//!
//! [`OAuthCallbacks`]: aj_models::oauth::OAuthCallbacks

use std::time::{SystemTime, UNIX_EPOCH};

use aj_models::auth::{AuthCredential, AuthStorage, find_env_keys};
use aj_models::oauth::OAuthAuthInfo;

/// Providers we always surface in the `/auth` status overlay even
/// when they have no credential yet, so the user can see what's
/// available to log into / configure. The union with
/// [`AuthStorage::oauth_provider_ids`] and any hand-added entry in
/// `auth.json` is computed at display time.
const KNOWN_PROVIDERS: &[&str] = &["anthropic", "openai", "openai-codex", "openrouter"];

/// A provider's resolved authentication status, ready to render.
///
/// `summary` describes the *method and source* that would win the
/// resolution chain (runtime override, then stored key, then stored
/// OAuth, then env). `detail` carries secondary info such as an OAuth
/// token's remaining lifetime.
#[derive(Debug, Clone)]
pub struct ProviderAuthStatus {
    pub provider_id: String,
    /// Exact raw account identity for a labeled row. `None` is the provider's
    /// bare credential or a provider-level source such as an environment key.
    pub account_label: Option<String>,
    /// Whether `account_label` is the store default.
    pub is_default: bool,
    /// Whether any credential source is configured.
    pub configured: bool,
    /// Short method/source label (e.g. `"subscription"`,
    /// `"env: ANTHROPIC_API_KEY"`, `"not configured"`).
    pub summary: String,
    /// Optional secondary line (e.g. `"expires in 1h 47m"`).
    pub detail: Option<String>,
}

/// Compute the auth status for a single `provider_id`.
///
/// `oauth_name` is the provider's display name when it's an OAuth
/// provider (used to annotate a stored subscription), otherwise
/// `None`. Mirrors the precedence in
/// [`AuthStorage::get_api_key`] but only *describes* the credential —
/// it never refreshes an OAuth token.
pub async fn provider_status(
    auth: &AuthStorage,
    provider_id: &str,
    oauth_name: Option<&str>,
) -> ProviderAuthStatus {
    // 1. Runtime override (`--api-key`).
    if auth.has_runtime_override(provider_id).await {
        return ProviderAuthStatus {
            provider_id: provider_id.to_string(),
            account_label: None,
            is_default: false,
            configured: true,
            summary: "API key (--api-key override)".to_string(),
            detail: None,
        };
    }

    // 2 & 3. Stored credential, reported before the environment to
    //        match the resolution order in `AuthStorage::get_api_key`.
    match auth.get(provider_id).await {
        Ok(Some(AuthCredential::ApiKey { .. })) => {
            return ProviderAuthStatus {
                provider_id: provider_id.to_string(),
                account_label: None,
                is_default: false,
                configured: true,
                summary: "API key (stored)".to_string(),
                detail: None,
            };
        }
        Ok(Some(AuthCredential::OAuth(creds))) => {
            let summary = match oauth_name {
                Some(name) => format!("subscription — {name}"),
                None => "subscription".to_string(),
            };
            return ProviderAuthStatus {
                provider_id: provider_id.to_string(),
                account_label: None,
                is_default: false,
                configured: true,
                summary,
                detail: Some(format_remaining(creds.expires, now_unix_ms())),
            };
        }
        Ok(None) => {}
        // A corrupt/locked auth.json shouldn't take down the overlay;
        // surface it as the status itself.
        Err(err) => {
            return ProviderAuthStatus {
                provider_id: provider_id.to_string(),
                account_label: None,
                is_default: false,
                configured: false,
                summary: format!("error reading auth.json: {err}"),
                detail: None,
            };
        }
    }

    // 4. Environment variable, reported by name when set.
    if let Some(var) = first_set_env_var(provider_id) {
        return ProviderAuthStatus {
            provider_id: provider_id.to_string(),
            account_label: None,
            is_default: false,
            configured: true,
            summary: format!("env: {var}"),
            detail: None,
        };
    }

    // 5. Nothing configured at any layer.
    ProviderAuthStatus {
        provider_id: provider_id.to_string(),
        account_label: None,
        is_default: false,
        configured: false,
        summary: "not configured".to_string(),
        detail: None,
    }
}

/// Describe one credential in a provider's labeled set without resolving or
/// refreshing it. The exact raw label remains separate from the rendered row.
fn account_status(
    provider_id: &str,
    label: String,
    is_default: bool,
    credential: AuthCredential,
    oauth_name: Option<&str>,
) -> ProviderAuthStatus {
    let (summary, detail) = match credential {
        AuthCredential::ApiKey { .. } => ("API key (stored)".to_string(), None),
        AuthCredential::OAuth(creds) => {
            let summary = oauth_name
                .map(|name| format!("subscription — {name}"))
                .unwrap_or_else(|| "subscription".to_string());
            (
                summary,
                Some(format_remaining(creds.expires, now_unix_ms())),
            )
        }
    };
    ProviderAuthStatus {
        provider_id: provider_id.to_string(),
        account_label: Some(label),
        is_default,
        configured: true,
        summary,
        detail,
    }
}

/// Build status rows for every provider worth showing: the
/// [`KNOWN_PROVIDERS`] set, every registered OAuth provider, and any
/// provider with a stored `auth.json` entry. Sorted by id for a
/// stable overlay order.
pub async fn collect_statuses(auth: &AuthStorage) -> Vec<ProviderAuthStatus> {
    let oauth = auth.oauth_provider_ids().await;

    let mut ids: Vec<String> = KNOWN_PROVIDERS.iter().map(|s| s.to_string()).collect();
    for (id, _) in &oauth {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    if let Ok(stored) = auth.list().await {
        for id in stored {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids.sort();

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let name = oauth
            .iter()
            .find(|(pid, _)| pid == &id)
            .map(|(_, name)| name.as_str());
        let has_override = auth.has_runtime_override(&id).await;
        if has_override {
            out.push(provider_status(auth, &id, name).await);
        }
        match auth.accounts(&id).await {
            Ok(Some(set)) => {
                let default = set.default;
                out.extend(set.accounts.into_iter().map(|(label, credential)| {
                    let is_default = label == default;
                    account_status(&id, label, is_default, credential, name)
                }));
            }
            Ok(None) | Err(_) if !has_override => out.push(provider_status(auth, &id, name).await),
            Ok(None) | Err(_) => {}
        }
    }
    out
}

/// First environment variable from [`find_env_keys`] that's set to a
/// non-empty value for `provider_id`, if any.
fn first_set_env_var(provider_id: &str) -> Option<&'static str> {
    find_env_keys(provider_id)
        .iter()
        .copied()
        .find(|name| std::env::var(name).is_ok_and(|v| !v.is_empty()))
}

/// Current wall-clock time in unix milliseconds.
fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Render the remaining lifetime of an OAuth access token expiring at
/// `expires_ms` as a coarse human string (`"expires in 1h 47m"`,
/// `"expired"`).
fn format_remaining(expires_ms: i64, now_ms: i64) -> String {
    let delta = expires_ms - now_ms;
    if delta <= 0 {
        return "expired (auto-refreshes on next request)".to_string();
    }
    let secs = delta / 1000;
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if days > 0 {
        format!("expires in {}d {}h", days, hours % 24)
    } else if hours > 0 {
        format!("expires in {}h {}m", hours, mins % 60)
    } else if mins > 0 {
        format!("expires in {mins}m")
    } else {
        format!("expires in {secs}s")
    }
}

/// Best-effort guess at whether a browser can be opened on *this*
/// machine — i.e. whether the login flow should attempt the automatic
/// loopback redirect or steer the user to the manual paste flow.
///
/// Heuristic, not authoritative:
/// - macOS / Windows: assume yes; a desktop session is the norm and the
///   launcher no-ops gracefully when there isn't one.
/// - Linux / other Unix: yes only if a display server or an explicit
///   `$BROWSER` is configured (`DISPLAY`, `WAYLAND_DISPLAY`, `BROWSER`).
///   A bare SSH session without X forwarding has none of these, which
///   is exactly the headless case the manual flow exists for.
pub fn browser_available() -> bool {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        true
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        ["DISPLAY", "WAYLAND_DISPLAY", "BROWSER"]
            .iter()
            .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty()))
    }
}

/// Best-effort open `url` in the user's default browser.
///
/// Spawns the platform launcher detached and ignores the outcome —
/// the login dialog always shows the URL (and accepts a manually
/// pasted redirect), so a failure here just means the user opens the
/// link themselves. Mirrors the fire-and-forget style of
/// [`crate::clipboard`].
pub fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let (program, args): (&str, &[&str]) = ("open", &[]);
    #[cfg(target_os = "windows")]
    let (program, args): (&str, &[&str]) = ("cmd", &["/C", "start", ""]);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let (program, args): (&str, &[&str]) = ("xdg-open", &[]);

    let _ = std::process::Command::new(program)
        .args(args)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// A single display line in the login dialog, tagged so each frontend
/// can color it through its own theme.
///
/// Frontend-agnostic on purpose: the wording lives here while the
/// frontend widget owns the actual styling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginLine {
    /// Plain informational text.
    Info(String),
    /// The authorization URL (rendered in the accent color).
    Url(String),
    /// A progress/status update emitted by the flow.
    Progress(String),
}

/// Compose the login dialog's authorization-step lines plus the URL to
/// store for clipboard copy.
///
/// Pure so the wording is unit-testable without sniffing the
/// environment through [`browser_available`] or touching the clipboard.
/// `copy` is the rendered shortcut for the copy action (e.g. `Ctrl+Y`).
///
/// The headless wording depends on whether the provider has a hosted
/// manual page ([`OAuthAuthInfo::manual_url`]). With one, the user pastes
/// a code that page shows. Without one (the redirect targets this
/// machine's loopback, which a remote browser can't reach), the user has
/// to copy the failed-redirect URL out of their browser's address bar, so
/// we say so explicitly.
pub fn auth_lines(
    can_open: bool,
    info: &OAuthAuthInfo<'_>,
    copy: &str,
) -> (Vec<LoginLine>, String) {
    let mut lines = Vec::new();

    if can_open {
        lines.push(LoginLine::Info(
            "Opening your browser to authorize\u{2026}".to_string(),
        ));
        if let Some(instructions) = info.instructions {
            lines.push(LoginLine::Info(instructions.to_string()));
        }
        lines.push(LoginLine::Info(format!(
            "If it doesn't open, click or copy ({copy}) this URL:"
        )));
        lines.push(LoginLine::Url(info.url.to_string()));
        if let Some(manual) = info.manual_url {
            lines.push(LoginLine::Info(
                "On a different machine? Open this URL instead, then paste the code it shows:"
                    .to_string(),
            ));
            lines.push(LoginLine::Url(manual.to_string()));
        }
        return (lines, info.url.to_string());
    }

    lines.push(LoginLine::Info(
        "No browser detected on this machine (headless/SSH).".to_string(),
    ));
    match info.manual_url {
        Some(manual) => {
            lines.push(LoginLine::Info(format!(
                "Open this URL on another device ({copy} to copy), then paste the code it shows:"
            )));
            lines.push(LoginLine::Url(manual.to_string()));
            (lines, manual.to_string())
        }
        None => {
            lines.push(LoginLine::Info(format!(
                "Open this URL on another device and sign in ({copy} to copy):"
            )));
            lines.push(LoginLine::Url(info.url.to_string()));
            lines.push(LoginLine::Info(
                "Your browser will then try to reach this machine and show a connection error. \
                 That's expected: copy the full URL from its address bar and paste it here."
                    .to_string(),
            ));
            (lines, info.url.to_string())
        }
    }
}

/// Copy `text` to the user's clipboard, best-effort, via two
/// complementary mechanisms so the common failure modes don't overlap:
///
/// - the system clipboard through `arboard` — works locally on
///   macOS / Windows / X11; and
/// - an OSC 52 terminal escape written to stdout, which many terminals
///   honor *over SSH* (iTerm2, kitty, wezterm, Alacritty), covering the
///   headless/remote case where `arboard` can only reach the *remote*
///   machine's clipboard (or no clipboard at all).
///
/// Nota bene: must be called on the UI thread. The OSC 52 write targets
/// the same stdout the TUI renders to, so issuing it off-thread could
/// interleave with a frame and corrupt the display.
///
/// Caveats worth knowing when this "doesn't work":
/// - The *outer* terminal must support OSC 52. macOS Terminal.app does
///   not; iTerm2 / kitty / wezterm / Alacritty do.
/// - Inside tmux the escape is also emitted in tmux's passthrough
///   wrapper (see [`osc52_payload`]); tmux still needs `set-clipboard`
///   on (to consume the bare form) or `allow-passthrough` on (to
///   forward the wrapped form) to relay it to the outer terminal.
/// - On X11 the `arboard` selection is dropped as soon as this returns
///   (ownership is process-bound). The always-visible URL line remains
///   the final fallback.
pub fn copy_to_clipboard(text: &str) {
    if let Err(err) = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
        tracing::debug!("clipboard: arboard set_text failed: {err}");
    }
    emit_osc52(text);
}

/// Write the OSC 52 clipboard payload for `text` to stdout, wrapping
/// for tmux when `$TMUX` is set.
fn emit_osc52(text: &str) {
    use std::io::Write;

    let payload = osc52_payload(text, std::env::var_os("TMUX").is_some());
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(payload.as_bytes());
    let _ = out.flush();
}

/// Build the OSC 52 "set clipboard" byte sequence(s) for `text`.
///
/// Always includes the bare `OSC 52 ; c ; <base64> BEL` form. When
/// `in_tmux`, also appends a tmux passthrough-wrapped copy
/// (`DCS tmux ; <payload-with-ESCs-doubled> ST`) so the escape reaches
/// the outer terminal regardless of whether the user's tmux is set up
/// to consume the bare form (`set-clipboard on`) or to forward the
/// wrapped form (`allow-passthrough on`). Setting the clipboard twice
/// when both apply is harmless.
fn osc52_payload(text: &str, in_tmux: bool) -> String {
    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let bare = format!("\x1b]52;c;{encoded}\x07");
    if in_tmux {
        // tmux passthrough: DCS tmux ; <data with every ESC doubled> ST
        let escaped = bare.replace('\x1b', "\x1b\x1b");
        format!("{bare}\x1bPtmux;{escaped}\x1b\\")
    } else {
        bare
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use aj_models::oauth::OAuthCredentials;
    use tempfile::TempDir;

    use super::*;

    /// Collect the line bodies (ignoring the line kind) for assertions.
    fn joined(lines: &[LoginLine]) -> String {
        lines
            .iter()
            .map(|l| match l {
                LoginLine::Info(t) | LoginLine::Url(t) | LoginLine::Progress(t) => t.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn urls(lines: &[LoginLine]) -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| match l {
                LoginLine::Url(u) => Some(u.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn collect_statuses_lists_each_exact_account_and_marks_the_default_without_secrets() {
        let dir = TempDir::new().expect("tempdir");
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), HashMap::new());
        auth.insert_account(
            "anthropic",
            "work",
            AuthCredential::OAuth(OAuthCredentials::new(
                "recognizable-refresh-secret",
                "recognizable-access-secret",
                i64::MAX,
            )),
        )
        .await
        .unwrap();
        auth.insert_account(
            "anthropic",
            "wo\nrk",
            AuthCredential::ApiKey {
                key: "recognizable-api-key-secret".to_string(),
            },
        )
        .await
        .unwrap_err();
        // Legacy keys are seeded through the raw compatibility boundary, not
        // through current creation.
        let raw = serde_json::json!({
            "anthropic": {
                "type": "accounts",
                "default": "work",
                "accounts": {
                    "work": {
                        "type": "oauth",
                        "refresh": "recognizable-refresh-secret",
                        "access": "recognizable-access-secret",
                        "expires": i64::MAX
                    },
                    "wo\nrk": {
                        "type": "api_key",
                        "key": "recognizable-api-key-secret"
                    }
                }
            }
        });
        std::fs::write(auth.path(), serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        auth.set_runtime_api_key("anthropic", "runtime-winner".to_string())
            .await;

        let statuses = collect_statuses(&auth).await;
        let rows = statuses
            .iter()
            .filter(|status| status.provider_id == "anthropic")
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 3);
        let winner = rows
            .iter()
            .find(|row| row.account_label.is_none())
            .expect("provider-level runtime override row");
        assert!(winner.summary.contains("--api-key override"));
        let work = rows
            .iter()
            .find(|row| row.account_label.as_deref() == Some("work"))
            .expect("work row");
        let hostile = rows
            .iter()
            .find(|row| row.account_label.as_deref() == Some("wo\nrk"))
            .expect("hostile row");
        assert!(work.is_default);
        assert!(!hostile.is_default);
        let visible_model = format!("{rows:?}");
        for secret in [
            "recognizable-refresh-secret",
            "recognizable-access-secret",
            "recognizable-api-key-secret",
            "runtime-winner",
        ] {
            assert!(!visible_model.contains(secret), "status leaked {secret}");
        }
    }

    /// Headless with no hosted manual page (e.g. openai-codex): the user
    /// must copy the failed-redirect URL out of the address bar, so the
    /// wording has to call that out and we surface the authorize URL.
    #[test]
    fn auth_lines_headless_without_hosted_page_explains_address_bar_copy() {
        let info = OAuthAuthInfo {
            url: "https://auth.example.com/authorize?x=1",
            manual_url: None,
            instructions: Some("ignored when headless"),
        };
        let (lines, stored) = auth_lines(false, &info, "Ctrl+Y");
        let body = joined(&lines);
        assert!(body.contains("headless/SSH"), "{body}");
        assert!(body.contains("address bar"), "{body}");
        assert!(body.contains("connection error"), "{body}");
        assert_eq!(urls(&lines), vec![info.url.to_string()]);
        assert_eq!(stored, info.url);
    }

    /// Headless with a hosted manual page (e.g. anthropic): lead with the
    /// manual URL and tell the user to paste the code it shows.
    #[test]
    fn auth_lines_headless_with_hosted_page_says_paste_code() {
        let info = OAuthAuthInfo {
            url: "http://localhost:1455/auth/callback?x=1",
            manual_url: Some("https://hosted.example.com/code"),
            instructions: None,
        };
        let (lines, stored) = auth_lines(false, &info, "Ctrl+Y");
        let body = joined(&lines);
        assert!(body.contains("paste the code it shows"), "{body}");
        assert!(!body.contains("address bar"), "{body}");
        assert_eq!(urls(&lines), vec!["https://hosted.example.com/code"]);
        assert_eq!(stored, "https://hosted.example.com/code");
    }

    /// With a browser available we open the automatic URL, store it for
    /// copy, and list a hosted manual URL as a secondary fallback.
    #[test]
    fn auth_lines_with_browser_lists_both_urls() {
        let info = OAuthAuthInfo {
            url: "http://localhost:1455/auth/callback?x=1",
            manual_url: Some("https://hosted.example.com/code"),
            instructions: Some("Complete login in your browser."),
        };
        let (lines, stored) = auth_lines(true, &info, "Ctrl+Y");
        let body = joined(&lines);
        assert!(body.contains("Opening your browser"), "{body}");
        assert!(body.contains("Complete login in your browser."), "{body}");
        assert_eq!(
            urls(&lines),
            vec![
                info.url.to_string(),
                "https://hosted.example.com/code".to_string()
            ]
        );
        assert_eq!(stored, info.url);
    }

    #[test]
    fn format_remaining_buckets() {
        let now = 1_000_000_000_000;
        assert_eq!(
            format_remaining(now - 1, now),
            "expired (auto-refreshes on next request)"
        );
        assert_eq!(format_remaining(now + 30_000, now), "expires in 30s");
        assert_eq!(format_remaining(now + 5 * 60_000, now), "expires in 5m");
        assert_eq!(
            format_remaining(now + (2 * 3600 + 15 * 60) * 1000, now),
            "expires in 2h 15m"
        );
        assert_eq!(
            format_remaining(now + (26 * 3600) * 1000, now),
            "expires in 1d 2h"
        );
    }

    #[test]
    fn osc52_bare_outside_tmux() {
        let payload = osc52_payload("hello", false);
        // base64("hello") == "aGVsbG8="
        assert_eq!(payload, "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn osc52_adds_tmux_passthrough_wrapper() {
        let payload = osc52_payload("hello", true);
        let bare = "\x1b]52;c;aGVsbG8=\x07";
        // Still starts with the bare form (for `set-clipboard`)...
        assert!(payload.starts_with(bare), "{payload:?}");
        // ...followed by the passthrough-wrapped form (for
        // `allow-passthrough`) with ESCs doubled and a ST terminator.
        let escaped = bare.replace('\x1b', "\x1b\x1b");
        assert_eq!(payload, format!("{bare}\x1bPtmux;{escaped}\x1b\\"));
    }
}
