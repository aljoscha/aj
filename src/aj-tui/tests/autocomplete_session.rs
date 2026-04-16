//! Integration tests for the streaming `@`-fuzzy autocomplete
//! session.
//!
//! These exercise the [`AutocompleteProvider::try_start_session`]
//! path that hands the editor a live
//! [`AutocompleteSession`] backed by
//! [`nucleo`]. The sync-path tests in
//! `tests/autocomplete.rs` still cover rank-order semantics via
//! `get_suggestions`; the tests here verify the session-specific
//! machinery that doesn't exist on the one-shot path:
//!
//! - Starting a session returns `Some` for `@`-contexts and `None`
//!   for slash / plain-text / direct-path contexts.
//! - Incremental `update` re-scores without re-walking.
//! - Scope changes (typing a `/` that redirects the walker root)
//!   return [`SessionInvalid`] so the editor knows to restart.
//! - Dropping the session cancels the walker.
//! - A populated snapshot ranks sensibly (nucleo's path-mode
//!   scoring puts filename matches above scattered-subsequence
//!   matches).

use std::fs;
use std::path::Path;
use std::sync::Arc;

use aj_tui::autocomplete::{
    AutocompleteProvider, AutocompleteSession, CombinedAutocompleteProvider, SessionInvalid,
    SessionStatus,
};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FolderShape<'a> {
    dirs: &'a [&'a str],
    files: &'a [&'a str],
}

fn setup(base: &Path, shape: FolderShape<'_>) {
    for dir in shape.dirs {
        fs::create_dir_all(base.join(dir)).expect("mkdir");
    }
    for rel in shape.files {
        let full = base.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir parent");
        }
        fs::write(&full, "").expect("write");
    }
}

/// Shared "render was requested" signal so tests can assert that
/// nucleo's notify callback fires when new matches arrive. Not
/// strictly needed for correctness — the important observable is
/// the snapshot — but confirms the plumbing is intact.
fn noop_notify() -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(|| {})
}

/// Block until the session reports `!running` or we exceed the
/// iteration cap. Needed because nucleo's matcher runs on its own
/// thread pool and we want a stable snapshot before asserting.
/// `budget_ms` is per-tick; the overall spin tops out at ~2s.
async fn drive_to_quiescent(session: &mut Box<dyn AutocompleteSession>) -> SessionStatus {
    let mut last = SessionStatus {
        changed: false,
        running: true,
    };
    for _ in 0..200 {
        tokio::task::yield_now().await;
        last = session.tick(50);
        if !last.running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    last
}

// ---------------------------------------------------------------------------
// Session creation vs. other contexts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn try_start_session_returns_some_on_at_context() {
    let tmp = TempDir::new().unwrap();
    setup(
        tmp.path(),
        FolderShape {
            dirs: &[],
            files: &["readme.md"],
        },
    );
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    let lines = vec!["@".to_string()];
    let session = provider.try_start_session(&lines, 0, 1, noop_notify());
    assert!(
        session.is_some(),
        "typing `@` should open a streaming session",
    );
}

#[tokio::test]
async fn try_start_session_returns_none_for_slash_context() {
    let tmp = TempDir::new().unwrap();
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    let lines = vec!["/".to_string()];
    let session = provider.try_start_session(&lines, 0, 1, noop_notify());
    assert!(
        session.is_none(),
        "slash commands stay on the one-shot path — streaming would add complexity with no win"
    );
}

#[tokio::test]
async fn try_start_session_returns_none_for_plain_text() {
    let tmp = TempDir::new().unwrap();
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    let lines = vec!["hello world".to_string()];
    let session = provider.try_start_session(&lines, 0, 11, noop_notify());
    assert!(
        session.is_none(),
        "non-completable contexts should not return a session",
    );
}

// ---------------------------------------------------------------------------
// Streaming & ranking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_snapshot_populates_after_walker_finishes() {
    let tmp = TempDir::new().unwrap();
    setup(
        tmp.path(),
        FolderShape {
            dirs: &[],
            files: &["alpha.rs", "beta.rs", "gamma.rs"],
        },
    );
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    let lines = vec!["@a".to_string()];
    let mut session = provider
        .try_start_session(&lines, 0, 2, noop_notify())
        .expect("session should open");

    drive_to_quiescent(&mut session).await;
    let snap = session.snapshot();

    // `@a` fuzzy-matches alpha and gamma (both contain an `a`),
    // but alpha leads with `a` and thus scores strictly higher
    // under nucleo's path-mode bonus. Assert alpha is in there;
    // don't pin the exact set because nucleo's subsequence match
    // may or may not accept "beta" depending on config nuances.
    let labels: Vec<String> = snap.iter().map(|it| it.label.clone()).collect();
    assert!(
        labels.contains(&"alpha.rs".to_string()),
        "snapshot should include alpha.rs; got {:?}",
        labels,
    );
}

#[tokio::test]
async fn session_ranks_filename_prefix_match_above_scattered_subsequence() {
    // Files laid out so nucleo's ranking is the discriminator:
    // "auto" is a prefix of `autocomplete.rs` (strong match) and
    // appears as a scattered subsequence in
    // `tests/support/mod.rs` (weaker match). The streaming
    // session trusts nucleo's ordering; a filename-prefix match
    // must outrank a scattered-subsequence hit.
    let tmp = TempDir::new().unwrap();
    setup(
        tmp.path(),
        FolderShape {
            dirs: &[],
            files: &["src/autocomplete.rs", "tests/support/mod.rs"],
        },
    );
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    let lines = vec!["@auto".to_string()];
    let mut session = provider
        .try_start_session(&lines, 0, 5, noop_notify())
        .expect("session should open");

    drive_to_quiescent(&mut session).await;
    let snap = session.snapshot();

    let first_label = snap.first().map(|it| it.label.clone()).unwrap_or_default();
    assert_eq!(
        first_label,
        "autocomplete.rs",
        "filename-prefix match should rank above scattered subsequence; got {:?}",
        snap.iter().map(|it| &it.label).collect::<Vec<_>>(),
    );
}

// ---------------------------------------------------------------------------
// Update / invalidation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_narrows_matches_without_restarting_walker() {
    let tmp = TempDir::new().unwrap();
    setup(
        tmp.path(),
        FolderShape {
            dirs: &[],
            files: &["alpha.rs", "anvil.rs", "beta.rs"],
        },
    );
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    // Start with just `@` (every file matches) ...
    let lines = vec!["@".to_string()];
    let mut session = provider
        .try_start_session(&lines, 0, 1, noop_notify())
        .expect("session should open");
    drive_to_quiescent(&mut session).await;
    let initial_count = session.snapshot().len();
    assert!(
        initial_count >= 3,
        "empty @ should surface every walked file; got {initial_count}"
    );

    // ... then narrow to `@an`. We reuse the same session: no
    // restart, no re-walk — nucleo just re-scores the injected
    // items against the longer needle.
    let lines = vec!["@an".to_string()];
    session
        .update(&lines, 0, 3)
        .expect("narrowing should keep the session alive");

    drive_to_quiescent(&mut session).await;
    let narrowed = session.snapshot();
    let labels: Vec<String> = narrowed.iter().map(|it| it.label.clone()).collect();
    assert!(
        labels.iter().any(|l| l == "anvil.rs"),
        "narrowed snapshot should include anvil.rs; got {:?}",
        labels,
    );
    assert!(
        !labels.iter().any(|l| l == "beta.rs"),
        "`an` should not fuzzy-match beta.rs; got {:?}",
        labels,
    );

    // `prefix()` follows the user's typing so the editor knows
    // how many characters to replace on apply.
    assert_eq!(session.prefix(), "@an");
}

#[tokio::test]
async fn update_keeps_session_alive_across_slash_in_prefix() {
    // The session is rooted at the project base once at construction
    // and stays there. Typing a `/` inside the `@`-prefix used to
    // re-root the walker under a sub-directory and invalidate the
    // session; it no longer does. The new characters are folded into
    // the nucleo pattern and `match_paths()` scoring promotes hits
    // at path-delimiter boundaries instead.
    let tmp = TempDir::new().unwrap();
    setup(
        tmp.path(),
        FolderShape {
            dirs: &["src"],
            files: &["src/lib.rs", "README.md"],
        },
    );
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    let lines = vec!["@s".to_string()];
    let mut session = provider
        .try_start_session(&lines, 0, 2, noop_notify())
        .expect("session should open");

    let lines = vec!["@src/".to_string()];
    session
        .update(&lines, 0, 5)
        .expect("slash inside the prefix should keep the session alive");
    assert_eq!(session.prefix(), "@src/");
}

#[tokio::test]
async fn update_returns_invalid_when_leaving_at_context() {
    let tmp = TempDir::new().unwrap();
    setup(
        tmp.path(),
        FolderShape {
            dirs: &[],
            files: &["x.rs"],
        },
    );
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    let lines = vec!["@".to_string()];
    let mut session = provider
        .try_start_session(&lines, 0, 1, noop_notify())
        .expect("session should open");

    // Remove the `@`. The session can't serve a non-`@` context.
    let lines = vec!["".to_string()];
    let outcome = session.update(&lines, 0, 0);
    assert_eq!(outcome, Err(SessionInvalid));
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_tick_reports_running_then_quiescent() {
    // The contract of `tick().running`: true while either the
    // walker is still pushing or the matcher is still absorbing
    // items; false once everything has settled. Tests and the
    // editor's `wait_for_pending_autocomplete` both rely on this
    // transition actually happening.
    let tmp = TempDir::new().unwrap();
    setup(
        tmp.path(),
        FolderShape {
            dirs: &[],
            files: &["a.rs", "b.rs", "c.rs"],
        },
    );
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    let lines = vec!["@".to_string()];
    let mut session = provider
        .try_start_session(&lines, 0, 1, noop_notify())
        .expect("session should open");

    let final_status = drive_to_quiescent(&mut session).await;
    assert!(
        !final_status.running,
        "session should eventually quiesce after the walker finishes"
    );
}

#[tokio::test]
async fn dropping_session_cancels_the_walker() {
    // Can't assert directly that the walker thread died — it's
    // owned by the `spawn_blocking` pool — but we can assert that
    // dropping the session is a non-blocking operation even
    // during an in-flight walk. If the cancel token wasn't wired,
    // `Drop` would still have to wait for the injector to be
    // dropped by the walker task.
    let tmp = TempDir::new().unwrap();
    setup(
        tmp.path(),
        FolderShape {
            dirs: &[],
            files: &["x.rs"],
        },
    );
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    let lines = vec!["@".to_string()];
    let session = provider
        .try_start_session(&lines, 0, 1, noop_notify())
        .expect("session should open");

    // Immediate drop: should complete without hanging.
    let start = std::time::Instant::now();
    drop(session);
    assert!(
        start.elapsed() < std::time::Duration::from_millis(500),
        "dropping a session should be near-instantaneous",
    );
}

// ---------------------------------------------------------------------------
// Notify wake-up
// ---------------------------------------------------------------------------

#[tokio::test]
async fn notify_fires_when_walker_pushes_items() {
    // Nucleo calls our notify callback whenever there's new
    // information. The editor relies on this to schedule a
    // re-render as matches stream in. We assert notify fires at
    // least once during a session that matches something.
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = TempDir::new().unwrap();
    setup(
        tmp.path(),
        FolderShape {
            dirs: &[],
            files: &["alpha.rs", "beta.rs", "gamma.rs"],
        },
    );
    let provider = CombinedAutocompleteProvider::new(vec![], tmp.path());

    let count = Arc::new(AtomicUsize::new(0));
    let count_cb = Arc::clone(&count);
    let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        count_cb.fetch_add(1, Ordering::Relaxed);
    });

    let lines = vec!["@a".to_string()];
    let mut session = provider
        .try_start_session(&lines, 0, 2, notify)
        .expect("session should open");

    drive_to_quiescent(&mut session).await;
    assert!(
        count.load(Ordering::Relaxed) > 0,
        "notify should fire at least once during a streaming session",
    );
}
