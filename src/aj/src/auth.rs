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
//!
//! The actual interactive login dialog and the [`OAuthCallbacks`]
//! implementation that drives it live in
//! [`crate::modes::interactive::components::login_dialog`].
//!
//! [`OAuthCallbacks`]: aj_models::oauth::OAuthCallbacks

use std::time::{SystemTime, UNIX_EPOCH};

use aj_models::auth::{AuthCredential, AuthStorage, find_env_keys};

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
            configured: true,
            summary: format!("env: {var}"),
            detail: None,
        };
    }

    // 5. Nothing configured at any layer.
    ProviderAuthStatus {
        provider_id: provider_id.to_string(),
        configured: false,
        summary: "not configured".to_string(),
        detail: None,
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
        out.push(provider_status(auth, &id, name).await);
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
    use super::*;

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
