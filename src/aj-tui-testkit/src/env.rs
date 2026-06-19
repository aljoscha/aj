//! RAII-style environment variable guards for tests.
//!
//! Because Rust runs tests in parallel by default and the process-wide
//! environment is shared state, env-mutating tests must be serialized. Use
//! `#[serial_test::serial]` on any test that calls [`with_env`].

use std::env;

/// Snapshot of an environment variable for later restoration.
struct Entry {
    key: String,
    previous: Option<String>,
}

/// RAII guard that restores a set of environment variables on drop.
#[must_use = "dropping the guard immediately undoes the env changes"]
pub struct EnvGuard {
    entries: Vec<Entry>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for entry in self.entries.drain(..) {
            match entry.previous {
                // SAFETY: In a single-threaded test (enforced by
                // `#[serial_test::serial]`) no other thread is reading from
                // the environment, so set/remove is safe.
                Some(value) => unsafe { env::set_var(&entry.key, value) },
                None => unsafe { env::remove_var(&entry.key) },
            }
        }
    }
}

/// Apply a set of environment variable updates for the lifetime of the
/// returned guard. `None` removes the variable; `Some(value)` sets it.
///
/// ```ignore
/// let _guard = with_env(&[("MY_FLAG", Some("1"))]);
/// run_test();
/// // MY_FLAG restored when _guard drops.
/// ```
pub fn with_env(updates: &[(&str, Option<&str>)]) -> EnvGuard {
    let mut entries = Vec::with_capacity(updates.len());
    for (key, value) in updates {
        let previous = env::var(key).ok();
        match value {
            // SAFETY: See `Drop` impl.
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
        entries.push(Entry {
            key: (*key).to_string(),
            previous,
        });
    }
    EnvGuard { entries }
}
