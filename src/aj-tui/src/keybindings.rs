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
use std::sync::{Arc, LazyLock, RwLock};

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
///
/// `Clone` backs the copy-on-write swap in [`set_user_bindings`]: the
/// global stores the manager behind an `Arc`, so mutating it clones the
/// inner value when snapshots are still outstanding.
#[derive(Clone)]
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
    ///
    /// Override merge rules:
    ///
    /// - Action IDs absent from `user_bindings` keep their default keys.
    /// - Action IDs present with a non-empty key list use the override
    ///   verbatim (replacing, not extending, the defaults).
    /// - Action IDs present with an **empty** key list resolve to no
    ///   keys at all — i.e. the user has explicitly unbound the action.
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

/// Convert a canonical keybinding string like `"ctrl+o"` or
/// `"alt+shift+t"` or `"escape"` into the display form
/// `"Ctrl+O"` / `"Alt+Shift+T"` / `"Esc"` used in UI surfaces
/// (palette shortcut column, overlay subtitles, help screens).
///
/// Splits on `+`, maps modifier and named-key segments to their
/// display labels, title-cases everything else, and rejoins. The
/// canonical form parsed here is the same form stored by
/// [`KeybindingsManager::get_keys`].
pub fn format_keybinding(canonical: &str) -> String {
    canonical
        .split('+')
        .map(format_key_segment)
        .collect::<Vec<_>>()
        .join("+")
}

fn format_key_segment(seg: &str) -> String {
    let lower = seg.to_ascii_lowercase();
    match lower.as_str() {
        "ctrl" => "Ctrl".to_string(),
        "alt" => "Alt".to_string(),
        "shift" => "Shift".to_string(),
        // `super` is the only "windows/command/meta" modifier the
        // canonical grammar recognizes (see `keys::parse_key_id` and
        // `keys::format_key_descriptor`, which deliberately reject
        // `meta`/`hyper`). We display it under the same `super` spelling
        // so the label can't advertise a modifier the matcher rejects.
        // Unknown spellings like `cmd`/`meta` fall through to the
        // title-case arm, the same as any other unrecognized segment.
        "super" => "Super".to_string(),
        "escape" | "esc" => "Esc".to_string(),
        "enter" | "return" => "Enter".to_string(),
        "tab" => "Tab".to_string(),
        "space" => "Space".to_string(),
        "backspace" => "Backspace".to_string(),
        "delete" | "del" => "Del".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" => "PgUp".to_string(),
        "pagedown" => "PgDn".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        "insert" => "Insert".to_string(),
        _ => {
            // Title-case: uppercase the first character, leave the
            // rest as-is so symbol-only segments like `]` survive
            // and function keys like `f1` become `F1`.
            let mut chars = seg.chars();
            match chars.next() {
                Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// Look up the first key bound to `action` in the process-wide
/// manager and return it formatted for display via
/// [`format_keybinding`]. Returns `None` when the action is
/// unknown or unbound.
///
/// Only the first binding is surfaced; multiple bindings get
/// unwieldy in narrow UI columns (shortcut cells, subtitles), so
/// callers that want all of them should use
/// [`KeybindingsManager::get_keys`] directly.
pub fn format_action_shortcut(action: &str) -> Option<String> {
    let kb = get();
    kb.get_keys(action).first().map(|k| format_keybinding(k))
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
/// selection surfaces.
pub fn tui_keybindings() -> KeybindingDefinitions {
    use KeybindingDefinition as K;
    vec![
        // Editor navigation and editing
        // Up/Down also navigate prompt history when the cursor is on
        // the first/last visual line of the editor; `ctrl+p` / `ctrl+n`
        // are the readline-style aliases for the same action.
        (
            "tui.editor.cursorUp".to_string(),
            K::new(["up", "ctrl+p"], "Move cursor up"),
        ),
        (
            "tui.editor.cursorDown".to_string(),
            K::new(["down", "ctrl+n"], "Move cursor down"),
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
            K::new(["up", "ctrl+p"], "Move selection up"),
        ),
        (
            "tui.select.down".to_string(),
            K::new(["down", "ctrl+n"], "Move selection down"),
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
/// take effect uniformly across the process.
///
/// The manager lives behind an `Arc` and [`get`] returns a clone of it
/// rather than a read guard. We do this because a `std::sync::RwLock` is
/// neither reentrant nor reader-preferring: a thread that held a read
/// guard and then re-entered the lock (directly, or transitively through
/// a child component's `handle_input`/`render`, or via
/// [`format_action_shortcut`]) would block on its own second read as
/// soon as another thread had a writer waiting. That writer in turn
/// waits for the first read guard, closing a deadlock cycle. Handing
/// back an `Arc` snapshot means no lock is ever held across caller code,
/// so reentrant reads and the writer cycle are both impossible.
static GLOBAL_KEYBINDINGS: LazyLock<RwLock<Arc<KeybindingsManager>>> = LazyLock::new(|| {
    RwLock::new(Arc::new(KeybindingsManager::new(
        tui_keybindings(),
        Vec::<(String, Vec<KeyId>)>::new(),
    )))
});

/// Take a cheap snapshot of the process-wide [`KeybindingsManager`].
///
/// The read lock is held only for the `Arc` clone inside this call, not
/// for the lifetime of the returned value, so callers never block a
/// writer and a writer never blocks them. Callers typically bind the
/// snapshot once at the top of an input handler and reuse it for every
/// `kb.matches(event, "tui.xxx")` call inside that handler:
///
/// ```ignore
/// let kb = aj_tui::keybindings::get();
/// if kb.matches(event, "tui.editor.cursorUp") { /* ... */ }
/// if kb.matches(event, "tui.editor.cursorDown") { /* ... */ }
/// ```
///
/// A snapshot reflects the bindings as of the moment of the call. A
/// concurrent [`set_user_bindings`] or [`set_manager`] swaps the global
/// for later callers but leaves outstanding snapshots untouched.
pub fn get() -> Arc<KeybindingsManager> {
    Arc::clone(
        &GLOBAL_KEYBINDINGS
            .read()
            .expect("aj-tui keybindings lock poisoned"),
    )
}

/// Replace the user-supplied bindings on the process-wide manager.
///
/// `user_bindings` follows the same shape as the second argument to
/// [`KeybindingsManager::new`]: an iterable of `(action_id, keys)`
/// pairs, where keys can be a single `&str`, a `String`, an array of
/// `&str`, or a `Vec`. Unknown action IDs are silently ignored,
/// matching the manager's own contract.
///
/// Replaces the previous user bindings wholesale. Call with an empty
/// iterator (or use [`reset`]) to drop user overrides entirely.
///
/// The new bindings apply to snapshots taken after this returns.
/// Snapshots already handed out by [`get`] keep their previous bindings.
pub fn set_user_bindings<U, S, K>(user_bindings: U)
where
    U: IntoIterator<Item = (S, K)>,
    S: Into<String>,
    K: IntoKeyList,
{
    let mut lock = GLOBAL_KEYBINDINGS
        .write()
        .expect("aj-tui keybindings lock poisoned");
    // Copy-on-write: clones the inner manager only when snapshots from
    // `get` are still alive, leaving those outstanding snapshots intact.
    Arc::make_mut(&mut lock).set_user_bindings(user_bindings);
}

/// Replace the entire process-wide manager.
///
/// Use when an embedder wants to swap in a different definition set
/// (e.g. an extension of [`tui_keybindings`] with downstream-specific
/// actions).
///
/// The new manager becomes the next snapshot. Snapshots already handed
/// out by [`get`] keep pointing at the previous manager.
pub fn set_manager(manager: KeybindingsManager) {
    let mut lock = GLOBAL_KEYBINDINGS
        .write()
        .expect("aj-tui keybindings lock poisoned");
    *lock = Arc::new(manager);
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
    fn format_keybinding_handles_modifiers_and_named_keys() {
        assert_eq!(format_keybinding("ctrl+o"), "Ctrl+O");
        assert_eq!(format_keybinding("escape"), "Esc");
        assert_eq!(format_keybinding("alt+shift+t"), "Alt+Shift+T");
        assert_eq!(format_keybinding("ctrl+left"), "Ctrl+Left");
        assert_eq!(format_keybinding("enter"), "Enter");
        assert_eq!(format_keybinding("pageUp"), "PgUp");
        assert_eq!(format_keybinding("ctrl+]"), "Ctrl+]");
    }

    #[test]
    fn format_keybinding_super_matches_canonical_spelling() {
        // The display label mirrors the canonical `super` token that
        // `keys::parse_key_id`/`format_key_descriptor` use, so the two
        // halves of the vocabulary agree. The match arm keys off the
        // lowercased segment, so a non-canonical casing still resolves.
        assert_eq!(format_keybinding("super+k"), "Super+K");
        assert_eq!(format_keybinding("SUPER+k"), "Super+K");
    }

    #[test]
    fn format_keybinding_does_not_advertise_unmatched_modifiers() {
        // `cmd`/`meta` are not part of the canonical grammar
        // (`parse_key_id` rejects them, so the binding never fires).
        // The display side must not pretty-print them as a recognized
        // modifier: they title-case like any other unknown segment, so
        // the help text can't pretend the binding is valid.
        assert_eq!(format_keybinding("cmd+k"), "Cmd+K");
        assert_eq!(format_keybinding("meta+k"), "Meta+K");
    }

    #[test]
    fn format_action_shortcut_reads_global_manager() {
        // Install a manager containing a known dummy action so the
        // test is isolated from the bundled defaults' evolution.
        set_manager(KeybindingsManager::new(
            vec![(
                "test.dummy".to_string(),
                KeybindingDefinition::new("ctrl+x", "Dummy"),
            )],
            Vec::<(String, Vec<KeyId>)>::new(),
        ));
        assert_eq!(
            format_action_shortcut("test.dummy"),
            Some("Ctrl+X".to_string())
        );
        assert_eq!(format_action_shortcut("test.unknown"), None);
        reset();
    }

    #[test]
    fn unknown_user_overrides_are_ignored() {
        let kbm = KeybindingsManager::new(tui_keybindings(), [("tui.nonexistent", "ctrl+x")]);
        assert!(kbm.get_conflicts().is_empty());
        assert_eq!(kbm.get_keys("tui.input.submit"), &["enter"]);
    }
}
