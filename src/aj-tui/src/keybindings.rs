//! Keybinding registry with user-overridable defaults.
//!
//! A [`KeybindingsManager`] maps string action IDs (e.g.
//! `"tui.editor.cursorUp"`) to the list of keys that should trigger them.
//! Definitions supply defaults; user-supplied overrides replace the
//! defaults for a given action but do not evict other actions that happen
//! to share the same key. Conflicts are only reported between two user
//! overrides, never between a user override and a default.
//!
//! ```ignore
//! use aj_tui::keybindings::{KeybindingsManager, tui_keybindings};
//!
//! let kbm = KeybindingsManager::new(
//!     tui_keybindings(),
//!     [("tui.input.submit", vec!["enter", "ctrl+enter"])],
//! );
//! assert_eq!(kbm.get_keys("tui.input.submit"), &["enter", "ctrl+enter"]);
//! // `tui.select.confirm` still defaults to `"enter"` even though the user
//! // bound it to the input submit action too.
//! assert_eq!(kbm.get_keys("tui.select.confirm"), &["enter"]);
//! ```

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{LazyLock, RwLock, RwLockReadGuard};

use crate::keys::{InputEvent, key_id_matches};

/// A key identifier string like `"ctrl+b"`, `"alt+left"`, or
/// `"shift+enter"`. See [`crate::keys::key_id_matches`] for the grammar.
pub type KeyId = String;

/// Definition of a single keybinding action.
#[derive(Debug, Clone)]
pub struct KeybindingDefinition {
    /// The default keys that trigger this action when the user has not
    /// supplied an override.
    pub default_keys: Vec<KeyId>,
    /// Human-readable description, shown in help screens and binding
    /// editors.
    pub description: String,
}

impl KeybindingDefinition {
    /// Build a definition from a key-or-list and a description.
    pub fn new<K, D>(keys: K, description: D) -> Self
    where
        K: IntoKeyList,
        D: Into<String>,
    {
        Self {
            default_keys: keys.into_key_list(),
            description: description.into(),
        }
    }
}

/// Conversion trait so callers can pass a single `&str`, an array, or a
/// `Vec<&str>` / `Vec<String>` to APIs that want a key list.
pub trait IntoKeyList {
    fn into_key_list(self) -> Vec<KeyId>;
}

impl IntoKeyList for &str {
    fn into_key_list(self) -> Vec<KeyId> {
        vec![self.to_string()]
    }
}

impl IntoKeyList for String {
    fn into_key_list(self) -> Vec<KeyId> {
        vec![self]
    }
}

impl IntoKeyList for Vec<&str> {
    fn into_key_list(self) -> Vec<KeyId> {
        self.into_iter().map(str::to_string).collect()
    }
}

impl IntoKeyList for Vec<String> {
    fn into_key_list(self) -> Vec<KeyId> {
        self
    }
}

impl<const N: usize> IntoKeyList for [&str; N] {
    fn into_key_list(self) -> Vec<KeyId> {
        self.iter().map(|s| (*s).to_string()).collect()
    }
}

/// An ordered list of `(action_id, definition)` pairs. Order is preserved
/// so that [`KeybindingsManager::get_resolved_bindings`] is deterministic.
pub type KeybindingDefinitions = Vec<(String, KeybindingDefinition)>;

/// Report of a single user-introduced conflict: the same key bound by the
/// user to two or more distinct action IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    pub key: KeyId,
    pub keybindings: Vec<String>,
}

/// Registry that resolves default definitions against optional user
/// overrides and exposes the flattened key list per action.
pub struct KeybindingsManager {
    definitions: KeybindingDefinitions,
    user_bindings: Vec<(String, Vec<KeyId>)>,
    keys_by_id: HashMap<String, Vec<KeyId>>,
    conflicts: Vec<KeybindingConflict>,
}

impl KeybindingsManager {
    /// Build a manager from a set of definitions and an optional set of
    /// user overrides. Overrides for action IDs not present in
    /// `definitions` are silently ignored.
    pub fn new<D, U, S1, S2, K>(definitions: D, user_bindings: U) -> Self
    where
        D: IntoIterator<Item = (S1, KeybindingDefinition)>,
        S1: Into<String>,
        U: IntoIterator<Item = (S2, K)>,
        S2: Into<String>,
        K: IntoKeyList,
    {
        let definitions: KeybindingDefinitions = definitions
            .into_iter()
            .map(|(id, def)| (id.into(), def))
            .collect();
        let user_bindings: Vec<(String, Vec<KeyId>)> = user_bindings
            .into_iter()
            .map(|(id, keys)| (id.into(), normalize_keys(keys.into_key_list())))
            .collect();

        let mut manager = Self {
            definitions,
            user_bindings,
            keys_by_id: HashMap::new(),
            conflicts: Vec::new(),
        };
        manager.rebuild();
        manager
    }

    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.conflicts.clear();

        // Only count claims by user-supplied bindings. This is the
        // "don't evict defaults" rule: a user pressing `ctrl+c` for a
        // custom action does not disqualify the built-in `ctrl+c`
        // bindings on other actions.
        let known_ids: HashSet<&str> = self.definitions.iter().map(|(id, _)| id.as_str()).collect();
        let mut user_claims: Vec<(KeyId, Vec<String>)> = Vec::new();
        for (keybinding, keys) in &self.user_bindings {
            if !known_ids.contains(keybinding.as_str()) {
                continue;
            }
            for key in keys {
                match user_claims.iter_mut().find(|(k, _)| k == key) {
                    Some((_, ids)) => {
                        if !ids.iter().any(|s| s == keybinding) {
                            ids.push(keybinding.clone());
                        }
                    }
                    None => user_claims.push((key.clone(), vec![keybinding.clone()])),
                }
            }
        }

        for (key, keybindings) in user_claims {
            if keybindings.len() > 1 {
                self.conflicts.push(KeybindingConflict { key, keybindings });
            }
        }

        let user_map: HashMap<&str, &Vec<KeyId>> = self
            .user_bindings
            .iter()
            .map(|(id, keys)| (id.as_str(), keys))
            .collect();

        for (id, definition) in &self.definitions {
            let keys = match user_map.get(id.as_str()) {
                Some(user_keys) => (*user_keys).clone(),
                None => normalize_keys(definition.default_keys.clone()),
            };
            self.keys_by_id.insert(id.clone(), keys);
        }
    }

    /// Check whether the given `event` matches any of the keys bound to
    /// `keybinding`.
    pub fn matches(&self, event: &InputEvent, keybinding: &str) -> bool {
        let Some(keys) = self.keys_by_id.get(keybinding) else {
            return false;
        };
        keys.iter().any(|key| key_id_matches(event, key))
    }

    /// Return the resolved key list for `keybinding`, or an empty slice
    /// if the action is unknown.
    pub fn get_keys(&self, keybinding: &str) -> &[KeyId] {
        self.keys_by_id
            .get(keybinding)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Look up the definition for `keybinding`.
    pub fn get_definition(&self, keybinding: &str) -> Option<&KeybindingDefinition> {
        self.definitions
            .iter()
            .find_map(|(id, def)| (id == keybinding).then_some(def))
    }

    /// Return all current user-introduced conflicts.
    pub fn get_conflicts(&self) -> &[KeybindingConflict] {
        &self.conflicts
    }

    /// Replace the user overrides and rebuild the resolved keys.
    pub fn set_user_bindings<U, S, K>(&mut self, user_bindings: U)
    where
        U: IntoIterator<Item = (S, K)>,
        S: Into<String>,
        K: IntoKeyList,
    {
        self.user_bindings = user_bindings
            .into_iter()
            .map(|(id, keys)| (id.into(), normalize_keys(keys.into_key_list())))
            .collect();
        self.rebuild();
    }

    /// Return the set of user-supplied overrides in the order they were
    /// supplied.
    pub fn get_user_bindings(&self) -> &[(String, Vec<KeyId>)] {
        &self.user_bindings
    }

    /// Return every action ID paired with its resolved key list.
    ///
    /// Order matches the order the definitions were supplied.
    pub fn get_resolved_bindings(&self) -> Vec<(String, Vec<KeyId>)> {
        self.definitions
            .iter()
            .map(|(id, _)| {
                let keys = self.keys_by_id.get(id).cloned().unwrap_or_default();
                (id.clone(), keys)
            })
            .collect()
    }
}

/// Remove duplicate entries while preserving order.
fn normalize_keys(keys: Vec<KeyId>) -> Vec<KeyId> {
    let mut seen: BTreeSet<KeyId> = BTreeSet::new();
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        if seen.insert(key.clone()) {
            out.push(key);
        }
    }
    out
}

/// Built-in keybinding definitions for the TUI's editor, input, and
/// selection surfaces. Equivalent to `TUI_KEYBINDINGS` in the JavaScript
/// framework we're modeling the API on.
pub fn tui_keybindings() -> KeybindingDefinitions {
    use KeybindingDefinition as K;
    vec![
        // Editor navigation and editing
        (
            "tui.editor.cursorUp".to_string(),
            K::new("up", "Move cursor up"),
        ),
        (
            "tui.editor.cursorDown".to_string(),
            K::new("down", "Move cursor down"),
        ),
        (
            "tui.editor.cursorLeft".to_string(),
            K::new(["left", "ctrl+b"], "Move cursor left"),
        ),
        (
            "tui.editor.cursorRight".to_string(),
            K::new(["right", "ctrl+f"], "Move cursor right"),
        ),
        (
            "tui.editor.cursorWordLeft".to_string(),
            K::new(["alt+left", "ctrl+left", "alt+b"], "Move cursor word left"),
        ),
        (
            "tui.editor.cursorWordRight".to_string(),
            K::new(
                ["alt+right", "ctrl+right", "alt+f"],
                "Move cursor word right",
            ),
        ),
        (
            "tui.editor.cursorLineStart".to_string(),
            K::new(["home", "ctrl+a"], "Move to line start"),
        ),
        (
            "tui.editor.cursorLineEnd".to_string(),
            K::new(["end", "ctrl+e"], "Move to line end"),
        ),
        (
            "tui.editor.jumpForward".to_string(),
            K::new("ctrl+]", "Jump forward to character"),
        ),
        (
            "tui.editor.jumpBackward".to_string(),
            K::new("ctrl+alt+]", "Jump backward to character"),
        ),
        ("tui.editor.pageUp".to_string(), K::new("pageUp", "Page up")),
        (
            "tui.editor.pageDown".to_string(),
            K::new("pageDown", "Page down"),
        ),
        (
            "tui.editor.deleteCharBackward".to_string(),
            K::new("backspace", "Delete character backward"),
        ),
        (
            "tui.editor.deleteCharForward".to_string(),
            K::new(["delete", "ctrl+d"], "Delete character forward"),
        ),
        (
            "tui.editor.deleteWordBackward".to_string(),
            K::new(["ctrl+w", "alt+backspace"], "Delete word backward"),
        ),
        (
            "tui.editor.deleteWordForward".to_string(),
            K::new(["alt+d", "alt+delete"], "Delete word forward"),
        ),
        (
            "tui.editor.deleteToLineStart".to_string(),
            K::new("ctrl+u", "Delete to line start"),
        ),
        (
            "tui.editor.deleteToLineEnd".to_string(),
            K::new("ctrl+k", "Delete to line end"),
        ),
        ("tui.editor.yank".to_string(), K::new("ctrl+y", "Yank")),
        (
            "tui.editor.yankPop".to_string(),
            K::new("alt+y", "Yank pop"),
        ),
        ("tui.editor.undo".to_string(), K::new("ctrl+-", "Undo")),
        // Generic input actions
        (
            "tui.input.newLine".to_string(),
            K::new("shift+enter", "Insert newline"),
        ),
        (
            "tui.input.submit".to_string(),
            K::new("enter", "Submit input"),
        ),
        (
            "tui.input.tab".to_string(),
            K::new("tab", "Tab / autocomplete"),
        ),
        (
            "tui.input.copy".to_string(),
            K::new("ctrl+c", "Copy selection"),
        ),
        // Generic selection actions
        (
            "tui.select.up".to_string(),
            K::new("up", "Move selection up"),
        ),
        (
            "tui.select.down".to_string(),
            K::new("down", "Move selection down"),
        ),
        (
            "tui.select.pageUp".to_string(),
            K::new("pageUp", "Selection page up"),
        ),
        (
            "tui.select.pageDown".to_string(),
            K::new("pageDown", "Selection page down"),
        ),
        (
            "tui.select.confirm".to_string(),
            K::new("enter", "Confirm selection"),
        ),
        (
            "tui.select.cancel".to_string(),
            K::new(["escape", "ctrl+c"], "Cancel selection"),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Process-scoped manager
// ---------------------------------------------------------------------------

/// Process-wide [`KeybindingsManager`] backing [`get`] and the helper
/// mutators below. Initialized lazily on first access with the built-in
/// defaults from [`tui_keybindings`] and no user overrides.
///
/// Components consult this singleton when handling input so that user
/// rebindings (installed via [`set_user_bindings`] or [`set_manager`])
/// take effect uniformly across the process — the same way TS's
/// `getKeybindings()` / `setKeybindings()` work.
static GLOBAL_KEYBINDINGS: LazyLock<RwLock<KeybindingsManager>> = LazyLock::new(|| {
    RwLock::new(KeybindingsManager::new(
        tui_keybindings(),
        Vec::<(String, Vec<KeyId>)>::new(),
    ))
});

/// Acquire a read-only handle to the process-wide [`KeybindingsManager`].
///
/// The returned guard holds a shared lock for its lifetime, so callers
/// typically bind it once at the top of an input handler and reuse it
/// for every `kb.matches(event, "tui.xxx")` call inside that handler:
///
/// ```ignore
/// let kb = aj_tui::keybindings::get();
/// if kb.matches(event, "tui.editor.cursorUp") { /* ... */ }
/// if kb.matches(event, "tui.editor.cursorDown") { /* ... */ }
/// ```
///
/// Concurrent reads are allowed; concurrent calls to
/// [`set_user_bindings`] or [`set_manager`] block until every read
/// guard has been dropped.
pub fn get() -> RwLockReadGuard<'static, KeybindingsManager> {
    GLOBAL_KEYBINDINGS
        .read()
        .expect("aj-tui keybindings lock poisoned")
}

/// Replace the user-supplied bindings on the process-wide manager.
///
/// `user_bindings` follows the same shape as the second argument to
/// [`KeybindingsManager::new`]: an iterable of `(action_id, keys)`
/// pairs, where keys can be a single `&str`, a `String`, an array of
/// `&str`, or a `Vec`. Unknown action IDs are silently ignored,
/// matching the manager's own contract.
///
/// Replaces the previous user bindings wholesale — call with an empty
/// iterator (or use [`reset`]) to drop user overrides entirely.
pub fn set_user_bindings<U, S, K>(user_bindings: U)
where
    U: IntoIterator<Item = (S, K)>,
    S: Into<String>,
    K: IntoKeyList,
{
    let mut lock = GLOBAL_KEYBINDINGS
        .write()
        .expect("aj-tui keybindings lock poisoned");
    lock.set_user_bindings(user_bindings);
}

/// Replace the entire process-wide manager.
///
/// Use when an embedder wants to swap in a different definition set
/// (e.g. an extension of [`tui_keybindings`] with downstream-specific
/// actions). Equivalent to TS's `setKeybindings(manager)`.
pub fn set_manager(manager: KeybindingsManager) {
    let mut lock = GLOBAL_KEYBINDINGS
        .write()
        .expect("aj-tui keybindings lock poisoned");
    *lock = manager;
}

/// Restore the process-wide manager to a freshly-built default
/// ([`tui_keybindings`] with no user overrides).
///
/// Primarily a test helper: integration tests that rebind keys for a
/// specific scenario should call this in a teardown step (or rely on
/// `serial_test::serial` plus a reset at the top of the next test) so
/// later tests see the canonical defaults.
pub fn reset() {
    set_manager(KeybindingsManager::new(
        tui_keybindings(),
        Vec::<(String, Vec<KeyId>)>::new(),
    ));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keys_removes_duplicates_preserving_order() {
        let out = normalize_keys(vec![
            "ctrl+c".to_string(),
            "enter".to_string(),
            "ctrl+c".to_string(),
            "escape".to_string(),
            "enter".to_string(),
        ]);
        assert_eq!(out, vec!["ctrl+c", "enter", "escape"]);
    }

    #[test]
    fn unknown_user_overrides_are_ignored() {
        let kbm = KeybindingsManager::new(tui_keybindings(), [("tui.nonexistent", "ctrl+x")]);
        assert!(kbm.get_conflicts().is_empty());
        assert_eq!(kbm.get_keys("tui.input.submit"), &["enter"]);
    }
}
