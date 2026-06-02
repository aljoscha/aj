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
const KNOWN_PROVIDERS: &[&str] = &["anthropic", "openai", "openai-codex"];

/// A provider's resolved authentication status, ready to render.
///
/// `summary` describes the *method and source* that would win the
/// resolution chain (runtime override → env → stored key → stored
/// OAuth); `detail` carries secondary info such as an OAuth token's
/// remaining lifetime.
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

    // 2. Environment variable — report which one is set.
    if let Some(var) = first_set_env_var(provider_id) {
        return ProviderAuthStatus {
            provider_id: provider_id.to_string(),
            configured: true,
            summary: format!("env: {var}"),
            detail: None,
        };
    }

    // 3 & 4. Stored credential.
    match auth.get(provider_id).await {
        Ok(Some(AuthCredential::ApiKey { .. })) => ProviderAuthStatus {
            provider_id: provider_id.to_string(),
            configured: true,
            summary: "API key (stored)".to_string(),
            detail: None,
        },
        Ok(Some(AuthCredential::OAuth(creds))) => {
            let summary = match oauth_name {
                Some(name) => format!("subscription — {name}"),
                None => "subscription".to_string(),
            };
            ProviderAuthStatus {
                provider_id: provider_id.to_string(),
                configured: true,
                summary,
                detail: Some(format_remaining(creds.expires, now_unix_ms())),
            }
        }
        Ok(None) => ProviderAuthStatus {
            provider_id: provider_id.to_string(),
            configured: false,
            summary: "not configured".to_string(),
            detail: None,
        },
        // A corrupt/locked auth.json shouldn't take down the overlay;
        // surface it as the status itself.
        Err(err) => ProviderAuthStatus {
            provider_id: provider_id.to_string(),
            configured: false,
            summary: format!("error reading auth.json: {err}"),
            detail: None,
        },
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
///   honor *over SSH* (iTerm2, kitty, wezterm, tmux with
///   `set-clipboard on`), covering the headless/remote case where
///   `arboard` can't reach a clipboard.
///
/// Nota bene: must be called on the UI thread. The OSC 52 write targets
/// the same stdout the TUI renders to, so issuing it off-thread could
/// interleave with a frame and corrupt the display.
///
/// On X11 the `arboard` selection is dropped as soon as this returns
/// (X11 clipboard ownership is process-bound), so on a plain X11
/// terminal without OSC 52 support the copy may not outlive a paste
/// attempt — the always-visible URL line remains the final fallback.
pub fn copy_to_clipboard(text: &str) {
    if let Err(err) = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
        tracing::debug!("clipboard: arboard set_text failed: {err}");
    }
    emit_osc52(text);
}

/// Write an OSC 52 "set clipboard" escape for `text` to stdout.
fn emit_osc52(text: &str) {
    use base64::Engine;
    use std::io::Write;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    // OSC 52 ; c ; <base64> BEL — `c` selects the clipboard buffer.
    let seq = format!("\x1b]52;c;{encoded}\x07");
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
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
}
