//! The session-tree overlay: navigate and switch a session's branches.
//!
//! A [`FilterableSelect`] over the current session's [`SessionTree`], one row
//! per segment, with the tree art baked into each row's label (the
//! session-selector shape, see [`crate::session_selector`]). Confirming a row
//! parks a [`SessionRequest::Branch`] for the host, which rebuilds the session
//! onto the chosen segment's tip through the same branch path the `b` shortcut
//! uses. Confirming the row that is already the current tip is a no-op close,
//! and Esc cancels.
//!
//! Rows are laid out by [`build_tree_rows`]: a DFS over the segment tree with
//! active-path siblings sorted first (so the current branch reads as a
//! straight line from the top), box-drawing connectors for structure, the
//! segment label truncated to one line, and a dim message-count/age suffix on
//! leaf segments. The confirm callback resolves a row back to its segment head
//! through a filter-key -> head map, the same indirection the session selector
//! uses.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use aj_app::session::SessionRequest;
use aj_session::{EntryId, SessionTree};
use chrono::{DateTime, Utc};
use vaxis::vxfw::{FilterableSelect, SelectItem, to_widget_ref};

use crate::interactive::OverlayHandles;
use crate::overlay::{OverlayPlacement, close_all, close_key_label, close_top, confirm_key_label};
use crate::session_selector::{format_age, truncate_chars};
use crate::settings_ui::push_window;
use crate::toasts::{busy_refusal, show_toast};

/// How much of a segment's label a row shows before truncating.
const LABEL_MAX_CHARS: usize = 60;

/// One rendered tree row: the display line, its dim leaf suffix, the plain
/// label used for filtering, and the segment head the row switches to.
pub(crate) struct TreeRow {
    /// The display line: ancestor connectors, an active marker, and the
    /// truncated segment label.
    pub display: String,
    /// The dim right-column suffix (message count + age) for a leaf segment.
    /// Empty for a fork.
    pub description: String,
    /// The untruncated segment label, so typing the branch text matches even
    /// past the display truncation.
    pub search: String,
    /// The segment's last entry: the switch target.
    pub head: EntryId,
}

/// Flatten a [`SessionTree`] into rows for the overlay. DFS from the roots;
/// among siblings the active-path child is rendered first so the current
/// branch reads top-down as a straight line. Root segments render flat (no
/// connector), so a branchless or mostly-linear session stays at column 0.
pub(crate) fn build_tree_rows(tree: &SessionTree, now: DateTime<Utc>) -> Vec<TreeRow> {
    let mut rows = Vec::with_capacity(tree.segments.len());
    let roots: Vec<usize> = tree
        .segments
        .iter()
        .enumerate()
        .filter(|(_, s)| s.parent.is_none())
        .map(|(i, _)| i)
        .collect();
    for root in active_first(tree, roots) {
        // Roots hang off the (invisible) virtual root, so they carry no
        // connector and their children start at column 0.
        render_segment(tree, root, "", "", "", now, &mut rows);
    }
    rows
}

/// Draw one segment's row, then recurse into its children with the ASCII
/// connectors for the next level.
///
/// `prefix` is the accumulated ancestor-continuation columns, `connector` is
/// this segment's own branch glyph (empty for a root), and `child_base` is the
/// prefix this segment's children prepend before their connectors.
fn render_segment(
    tree: &SessionTree,
    index: usize,
    prefix: &str,
    connector: &str,
    child_base: &str,
    now: DateTime<Utc>,
    rows: &mut Vec<TreeRow>,
) {
    let seg = &tree.segments[index];
    // The active branch is marked; inactive rows pad the marker column so
    // labels within a sibling group line up.
    let marker = if seg.on_active_path {
        "\u{2022} "
    } else {
        "  "
    };
    // Collapse the label to its first line before truncating. A branch's
    // first user message can contain newlines, and a multi-line row would
    // spill across visual rows, misaligning the connectors and breaking the
    // selector's single-line overflow math. Mirrors the session selector's
    // primary-column handling.
    let one_line = seg.label.lines().next().unwrap_or(&seg.label);
    let label = truncate_chars(one_line, LABEL_MAX_CHARS);
    let description = if seg.is_leaf {
        leaf_suffix(seg.message_count, seg.last_timestamp, now)
    } else {
        String::new()
    };
    rows.push(TreeRow {
        display: format!("{prefix}{connector}{marker}{label}"),
        description,
        search: seg.label.clone(),
        head: seg.head.clone(),
    });

    let ordered = active_first(tree, seg.children.clone());
    let last = ordered.len().saturating_sub(1);
    for (pos, child) in ordered.into_iter().enumerate() {
        let is_last = pos == last;
        // `└─` closes the last sibling, `├─` continues; the child's own
        // children then either clear the column (last) or keep the `│` riser.
        let (child_connector, grandchild_base) = if is_last {
            ("\u{2514}\u{2500} ", format!("{child_base}   "))
        } else {
            ("\u{251c}\u{2500} ", format!("{child_base}\u{2502}  "))
        };
        render_segment(
            tree,
            child,
            child_base,
            child_connector,
            &grandchild_base,
            now,
            rows,
        );
    }
}

/// Order `indices` so active-path segments come first, keeping append order
/// within each group (a stable sort).
fn active_first(tree: &SessionTree, mut indices: Vec<usize>) -> Vec<usize> {
    indices.sort_by_key(|&i| !tree.segments[i].on_active_path);
    indices
}

/// The dim leaf suffix: the segment's message count and the relative age of
/// its last entry. Drops the age when the entry carries no timestamp.
fn leaf_suffix(count: usize, last: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let word = if count == 1 { "msg" } else { "msgs" };
    match last {
        Some(ts) => format!("{count} {word} \u{00b7} {}", format_age(now, ts)),
        None => format!("{count} {word}"),
    }
}

/// Open the session-tree overlay over `rows`. A confirmed row parks a
/// [`SessionRequest::Branch`] in `handles.session_request` unless it is
/// already the current tip (`current_head`); Esc closes with no change. An
/// empty `rows` (an unpersisted session) shows an inert placeholder. Does not
/// move focus: the caller posts the refocus event.
///
/// The overlay opens read-only at any time. A real branch switch is refused
/// at confirm time while `handles.busy` (an in-flight turn or background
/// work): it raises a toast into `handles.toasts` and keeps the overlay open
/// rather than parking a request that would tear live work down.
pub(crate) fn open_session_tree(
    handles: &OverlayHandles,
    rows: Vec<TreeRow>,
    current_head: Option<EntryId>,
) -> Rc<RefCell<FilterableSelect>> {
    let heads = heads_map(&rows);
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        tree_items(&rows),
        handles.chrome.select.clone(),
    )));
    let focus = select.borrow().focus_target();
    {
        let mut sel = select.borrow_mut();
        // A branchy session can outgrow the overlay, so show the scroll bar.
        sel.set_show_scrollbar(true);
        let heads_c = heads.clone();
        let current_c = current_head.clone();
        let request_c = Rc::clone(&handles.session_request);
        let busy_c = Rc::clone(&handles.busy);
        let toasts_c = Rc::clone(&handles.toasts);
        let stack_c = Rc::clone(&handles.stack);
        let editor_c = Rc::clone(&handles.editor);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            if let Some(request) = confirm_request(&heads_c, current_c.as_deref(), &item.filter_key)
            {
                // A real branch switch mid-work would tear live turns and
                // background work down, so refuse it: raise the toast and keep
                // the overlay open (the user can Esc or wait).
                if busy_c.get() {
                    show_toast(&toasts_c, busy_refusal("switch branches"));
                    ctx.redraw = true;
                    return;
                }
                *request_c.borrow_mut() = Some(request);
            }
            // A confirmed pick is terminal: tear the whole stack down (palette
            // included) back to the transcript, matching the session selector.
            close_all(&stack_c, ctx, &editor_c);
        }));
        let stack_cancel = Rc::clone(&handles.stack);
        let editor_cancel = Rc::clone(&handles.editor);
        sel.on_cancel = Some(Box::new(move |ctx| {
            close_top(&stack_cancel, ctx, &editor_cancel)
        }));
    }
    // Pre-select the current branch tip's row when the head maps to a segment
    // head. A mid-segment head matches nothing and leaves the default cursor.
    if let Some(head) = current_head.as_deref() {
        select
            .borrow()
            .select_matching(|item| heads.get(&item.filter_key).map(String::as_str) == Some(head));
    }
    push_window(
        &handles.stack,
        &handles.chrome,
        "Session tree",
        subtitle(),
        to_widget_ref(Rc::clone(&select)),
        focus,
        OverlayPlacement::Large,
    );
    // The built select, so tests can drive its confirm closure directly (the
    // host caller ignores it, matching `push_window`'s ignored window handle).
    select
}

/// Resolve a confirmed row's filter key to a branch switch request. `None`
/// when the row is the inert placeholder or already the current tip (an exact
/// head match, so selecting the active branch with a mid-segment head still
/// switches, fast-forwarding to the segment's end).
fn confirm_request(
    heads: &HashMap<String, EntryId>,
    current_head: Option<&str>,
    filter_key: &str,
) -> Option<SessionRequest> {
    let head = heads.get(filter_key)?;
    if current_head == Some(head.as_str()) {
        return None;
    }
    Some(SessionRequest::Branch { head: head.clone() })
}

/// One select item per row: the display line as the label, the leaf suffix as
/// the dim description, and `"{search} {head}"` as the filter key so typing the
/// branch text finds it and the head keeps the key unique. An empty `rows`
/// yields a single inert placeholder (its empty filter key is absent from the
/// head map, so a confirm on it parks nothing).
fn tree_items(rows: &[TreeRow]) -> Vec<SelectItem> {
    if rows.is_empty() {
        return vec![SelectItem::new("(no branches yet)", "")];
    }
    rows.iter()
        .map(|row| {
            let mut item = SelectItem::new(row.display.clone(), row_key(row));
            if !row.description.is_empty() {
                item = item.with_description(row.description.clone());
            }
            item
        })
        .collect()
}

/// filter_key -> segment head, for the confirm resolution.
fn heads_map(rows: &[TreeRow]) -> HashMap<String, EntryId> {
    rows.iter()
        .map(|row| (row_key(row), row.head.clone()))
        .collect()
}

/// A row's filter key: the searchable branch text plus its unique head.
fn row_key(row: &TreeRow) -> String {
    format!("{} {}", row.search, row.head)
}

/// The confirm/close subtitle, with labels resolved from the keybinding data.
fn subtitle() -> String {
    format!(
        "{} to switch  \u{2022}  {} to close",
        confirm_key_label(),
        close_key_label()
    )
}

#[cfg(test)]
mod tests {
    use aj_session::TreeSegment;
    use chrono::Duration;

    use super::*;

    fn segment(
        head: &str,
        label: &str,
        parent: Option<usize>,
        children: Vec<usize>,
        on_active_path: bool,
    ) -> TreeSegment {
        TreeSegment {
            head: head.to_string(),
            label: label.to_string(),
            message_count: 1,
            last_timestamp: Some(Utc::now()),
            parent,
            children: children.clone(),
            on_active_path,
            is_leaf: children.is_empty(),
        }
    }

    /// A branchless session renders as a single flat row keyed to its head.
    #[test]
    fn single_segment_yields_one_row() {
        let tree = SessionTree {
            segments: vec![segment("h", "only branch", None, vec![], true)],
        };
        let rows = build_tree_rows(&tree, Utc::now());
        assert_eq!(rows.len(), 1);
        assert!(rows[0].display.contains("only branch"));
        // A root carries no connector.
        assert!(!rows[0].display.contains('\u{251c}'));
        assert!(!rows[0].display.contains('\u{2514}'));
        assert_eq!(rows[0].head, "h");
        // A leaf carries the count/age suffix.
        assert!(
            rows[0].description.contains("msg"),
            "{}",
            rows[0].description
        );
    }

    /// A fork with a deeper active sub-fork exercises every connector, the
    /// active-first sibling ordering, and the row->head key mapping.
    #[test]
    fn fork_rows_carry_connectors_active_first_and_keys() {
        // shared -> { branch B (active) -> { sub B1 (active), sub B2 },
        //             branch A }
        // The head sits at sub B1, so shared, branch B, and sub B1 are active.
        let mut prefix = segment("p", "shared", None, vec![1, 2], true);
        prefix.message_count = 2;
        prefix.is_leaf = false;
        let mut branch_b = segment("b", "branch B", Some(0), vec![3, 4], true);
        branch_b.is_leaf = false;
        let branch_a = segment("a", "branch A", Some(0), vec![], false);
        let sub_b1 = segment("b1", "sub B1", Some(1), vec![], true);
        let sub_b2 = segment("b2", "sub B2", Some(1), vec![], false);
        let tree = SessionTree {
            segments: vec![prefix, branch_b, branch_a, sub_b1, sub_b2],
        };

        let rows = build_tree_rows(&tree, Utc::now());
        let display: Vec<&str> = rows.iter().map(|r| r.display.as_str()).collect();
        assert_eq!(rows.len(), 5);

        // Active branch B sorts before abandoned branch A among the prefix's
        // children, so the current branch reads top-down.
        assert!(display[0].contains("shared"));
        assert!(display[1].contains("branch B") && display[1].starts_with('\u{251c}'));
        assert!(display[2].contains("sub B1") && display[2].starts_with("\u{2502}  \u{251c}"));
        assert!(display[3].contains("sub B2") && display[3].starts_with("\u{2502}  \u{2514}"));
        assert!(display[4].contains("branch A") && display[4].starts_with('\u{2514}'));

        // The active segments carry the bullet marker; the abandoned ones do
        // not.
        assert!(
            display[1].contains('\u{2022}'),
            "active B marked: {}",
            display[1]
        );
        assert!(
            !display[4].contains('\u{2022}'),
            "inactive A unmarked: {}",
            display[4]
        );

        // Leaf rows carry the suffix; the fork rows do not.
        assert!(rows[0].description.is_empty(), "fork has no suffix");
        assert!(rows[2].description.contains("msg"), "leaf has a suffix");

        // Every row maps back to its own segment head.
        let heads = heads_map(&rows);
        for row in &rows {
            assert_eq!(heads.get(&row_key(row)), Some(&row.head));
        }
    }

    /// Truncation applies to long labels.
    #[test]
    fn long_labels_truncate() {
        let long = "x".repeat(200);
        let tree = SessionTree {
            segments: vec![segment("h", &long, None, vec![], true)],
        };
        let rows = build_tree_rows(&tree, Utc::now());
        assert!(rows[0].display.ends_with('\u{2026}'), "{}", rows[0].display);
    }

    /// A multi-line label renders as a single line: only its first line, then
    /// truncated. A newline in the label would otherwise spill the row across
    /// visual rows and misalign the tree.
    #[test]
    fn multiline_label_collapses_to_one_line() {
        let tree = SessionTree {
            segments: vec![segment("h", "line one\nline two", None, vec![], true)],
        };
        let rows = build_tree_rows(&tree, Utc::now());
        assert_eq!(rows.len(), 1);
        assert!(
            !rows[0].display.contains('\n'),
            "row body is one line: {:?}",
            rows[0].display
        );
        assert!(
            rows[0].display.contains("line one"),
            "first line kept: {:?}",
            rows[0].display
        );
        assert!(
            !rows[0].display.contains("line two"),
            "second line dropped: {:?}",
            rows[0].display
        );
    }

    /// Leaf suffix drops the age when there is no timestamp, and pluralizes.
    #[test]
    fn leaf_suffix_formats_count_and_age() {
        let now = Utc::now();
        assert_eq!(leaf_suffix(1, None, now), "1 msg");
        assert_eq!(
            leaf_suffix(3, Some(now - Duration::hours(2)), now),
            "3 msgs \u{00b7} 2h"
        );
    }

    /// Confirming a segment parks a branch switch for its head; confirming the
    /// current tip parks nothing.
    #[test]
    fn confirm_parks_branch_and_no_ops_on_current_head() {
        let tree = SessionTree {
            segments: vec![
                segment("p", "shared", None, vec![1, 2], true),
                segment("b", "branch B", Some(0), vec![], true),
                segment("a", "branch A", Some(0), vec![], false),
            ],
        };
        let rows = build_tree_rows(&tree, Utc::now());
        let heads = heads_map(&rows);
        let key_for = |head: &str| {
            rows.iter()
                .find(|r| r.head == head)
                .map(row_key)
                .expect("row for head")
        };

        // The head sits at branch B's tip. Confirming it is a no-op.
        assert!(confirm_request(&heads, Some("b"), &key_for("b")).is_none());

        // Confirming the abandoned branch A parks a switch onto its head.
        assert!(matches!(
            confirm_request(&heads, Some("b"), &key_for("a")),
            Some(SessionRequest::Branch { head }) if head == "a"
        ));

        // The inert placeholder key parks nothing.
        assert!(confirm_request(&heads, Some("b"), "").is_none());
    }

    /// Drive the REAL confirm closure `open_session_tree` builds, over a live
    /// overlay stack, selecting the abandoned branch A (a real switch off the
    /// current tip "b"). Returns the parked request, the stack (to check
    /// open/closed), and the toast stack.
    #[expect(clippy::type_complexity)]
    fn confirm_switch_over(
        busy: bool,
    ) -> (
        Rc<RefCell<Option<SessionRequest>>>,
        Rc<RefCell<crate::overlay::OverlayStack>>,
        crate::toasts::ToastStack,
    ) {
        let tree = SessionTree {
            segments: vec![
                segment("p", "shared", None, vec![1, 2], true),
                segment("b", "branch B", Some(0), vec![], true),
                segment("a", "branch A", Some(0), vec![], false),
            ],
        };
        let rows = build_tree_rows(&tree, Utc::now());
        let key_for_a = rows
            .iter()
            .find(|r| r.head == "a")
            .map(row_key)
            .expect("a row for branch A");

        let handles = OverlayHandles::for_tests();
        handles.busy.set(busy);

        let select = open_session_tree(&handles, rows, Some("b".to_string()));
        select
            .borrow()
            .select_matching(|item| item.filter_key == key_for_a);
        let picked = select.borrow().selected().expect("a row is selected");
        if let Some(cb) = select.borrow_mut().on_confirm.as_mut() {
            let mut ctx = vaxis::vxfw::EventContext::new();
            cb(&mut ctx, &picked);
        }
        (
            Rc::clone(&handles.session_request),
            Rc::clone(&handles.stack),
            Rc::clone(&handles.toasts),
        )
    }

    /// While busy, confirming a real branch switch raises the toast and parks
    /// NO request, leaving the overlay open so the user can Esc or wait.
    #[test]
    fn confirm_switch_while_busy_toasts_and_stays_open() {
        let (request, stack, toasts) = confirm_switch_over(true);
        assert!(request.borrow().is_none(), "no request parked while busy");
        assert!(
            stack.borrow().is_open(),
            "the overlay stays open while busy"
        );
        assert!(
            crate::toasts::toast_texts(&toasts)
                .iter()
                .any(|m| m.contains("Can't switch branches while work is running")),
            "the refusal raises the branch toast"
        );
    }

    /// While idle, confirming a real branch switch parks it and closes.
    #[test]
    fn confirm_switch_while_idle_parks_and_closes() {
        let (request, stack, toasts) = confirm_switch_over(false);
        assert!(
            matches!(request.borrow().as_ref(), Some(SessionRequest::Branch { head }) if head == "a"),
            "an idle switch parks a branch for the picked head",
        );
        assert!(!stack.borrow().is_open(), "the confirm closed the overlay");
        assert!(
            crate::toasts::toast_texts(&toasts).is_empty(),
            "no toast raised while idle"
        );
    }

    /// A mid-segment head (not equal to any segment's last entry) is not the
    /// current tip of any row, so every selection switches (fast-forward).
    #[test]
    fn mid_segment_head_switches_on_any_selection() {
        let tree = SessionTree {
            segments: vec![segment("tip", "only branch", None, vec![], true)],
        };
        let rows = build_tree_rows(&tree, Utc::now());
        let heads = heads_map(&rows);
        let key = row_key(&rows[0]);
        // The current head is "mid", an interior entry, not the segment tip
        // "tip". Selecting the segment fast-forwards to "tip".
        assert!(matches!(
            confirm_request(&heads, Some("mid"), &key),
            Some(SessionRequest::Branch { head }) if head == "tip"
        ));
    }

    /// An empty tree (an unpersisted session) shows one inert placeholder row.
    #[test]
    fn empty_tree_shows_placeholder() {
        let rows = build_tree_rows(&SessionTree { segments: vec![] }, Utc::now());
        assert!(rows.is_empty());
        let items = tree_items(&rows);
        assert_eq!(items.len(), 1);
        assert!(items[0].label.contains("no branches"));
        // The placeholder's empty key resolves to no head.
        assert!(heads_map(&rows).is_empty());
    }
}
