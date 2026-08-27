//! The session tree: a segment-collapsed view of a log's branch structure.
//!
//! A conversation log is a tree of entries linked by `parent_id`. Deliberate
//! branches (see the branching flow in the binary) grow sibling chains off a
//! shared prefix. [`SessionTree`] projects that raw entry tree onto a compact
//! model the tree-view overlay renders: one node per *segment*, a maximal
//! linear run of user-thread entries between forks.
//!
//! The model is pure and built on demand from the in-memory log image
//! ([`ConversationLog::session_tree`]). It carries untruncated labels and
//! append-order children. Truncation and active-first display ordering are the
//! view's concern, not the model's.

use std::collections::{HashMap, HashSet};

use aj_models::types::{AssistantContent, Message, UserContent};
use chrono::{DateTime, Utc};

use crate::log::{
    ConversationEntry, ConversationEntryKind, ConversationLog, EntryId, LogSnapshot, ThreadKind,
};

/// The branch structure of a session, collapsed to one node per segment.
///
/// A degenerate log (empty or never persisted) yields no segments; a
/// branchless session yields exactly one; a fork yields the shared-prefix
/// segment plus one child segment per branch.
pub struct SessionTree {
    /// Segments in DFS pre-order from the roots (each root, then its
    /// descendants, before the next root; roots and children in append
    /// order). The `parent` and `children` indices refer into this vec.
    pub segments: Vec<TreeSegment>,
}

/// One node in a [`SessionTree`]: a maximal linear run of user-thread entries
/// running from a root (or a fork child) down through single-child
/// descendants until the next fork or a leaf.
pub struct TreeSegment {
    /// The segment's last entry. This is the switch target for the branch:
    /// setting the log head here selects this branch's tip.
    pub head: EntryId,
    /// Untruncated display text: the segment's first user message (the
    /// divergence point), with fallbacks. The view truncates.
    pub label: String,
    /// Count of `Message` entries in this segment.
    pub message_count: usize,
    /// Timestamp of the segment's last entry.
    pub last_timestamp: Option<DateTime<Utc>>,
    /// Parent segment index, `None` for a root segment.
    pub parent: Option<usize>,
    /// Child segment indices, in append order.
    pub children: Vec<usize>,
    /// Whether any of the segment's entries lie on the head's root->head
    /// chain. `false` for every segment when the log has no head.
    pub on_active_path: bool,
    /// Whether the segment's last entry has no user-thread children.
    pub is_leaf: bool,
}

impl LogSnapshot {
    /// Build the [`SessionTree`] on demand from the current log image. Cheap
    /// and in-memory: one pass to index children plus a DFS over the
    /// user-thread forest.
    pub fn session_tree(&self) -> SessionTree {
        SessionTree::build(&self.entries_in_order(), self.head())
    }
}

impl ConversationLog {
    /// See [`LogSnapshot::session_tree`].
    pub fn session_tree(&self) -> SessionTree {
        self.core().session_tree()
    }
}

impl SessionTree {
    fn build(entries: &[&ConversationEntry], head: Option<&EntryId>) -> SessionTree {
        // Index every entry by id for the active-path walk. Values borrow the
        // caller's slice.
        let by_id: HashMap<&str, &ConversationEntry> =
            entries.iter().map(|e| (e.id.as_str(), *e)).collect();

        // The set of user-thread entry ids. Sub-agent and meta entries are
        // excluded so a spawn root never reads as a branch point.
        let user_ids: HashSet<&str> = entries
            .iter()
            .copied()
            .filter(|e| e.thread == ThreadKind::User)
            .map(|e| e.id.as_str())
            .collect();

        // Children index over user-thread entries plus the virtual root's
        // children (the roots). A user entry whose parent is another user
        // entry is that entry's child; anything else (no parent, the
        // system-prompt meta entry, a missing parent) is a root of the virtual
        // root. Iterating in append order keeps every child list append-ordered.
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut roots: Vec<&str> = Vec::new();
        for e in entries
            .iter()
            .copied()
            .filter(|e| e.thread == ThreadKind::User)
        {
            match e.parent_id.as_deref() {
                Some(p) if user_ids.contains(p) => {
                    children.entry(p).or_default().push(e.id.as_str())
                }
                _ => roots.push(e.id.as_str()),
            }
        }

        let active = active_path_set(&by_id, head);

        let mut segments = Vec::new();
        for root in roots {
            build_segment(root, None, &by_id, &children, &active, &mut segments);
        }
        SessionTree { segments }
    }
}

/// The head's root->head ancestor id set, walking `parent_id` from `head`.
/// Empty when there is no head. The insert guard tolerates a cycle in a
/// hand-edited file.
fn active_path_set<'a>(
    by_id: &HashMap<&'a str, &'a ConversationEntry>,
    head: Option<&EntryId>,
) -> HashSet<&'a str> {
    let mut set = HashSet::new();
    let mut cursor = head.and_then(|id| by_id.get(id.as_str()).copied());
    while let Some(entry) = cursor {
        if !set.insert(entry.id.as_str()) {
            break;
        }
        cursor = entry
            .parent_id
            .as_deref()
            .and_then(|p| by_id.get(p).copied());
    }
    set
}

/// Walk the linear run from `start` (a root or a fork child) to the next fork
/// or leaf, push the resulting segment onto `out`, and recurse into a fork's
/// children. Returns the new segment's index.
fn build_segment<'a>(
    start: &'a str,
    parent: Option<usize>,
    by_id: &HashMap<&'a str, &'a ConversationEntry>,
    children: &HashMap<&'a str, Vec<&'a str>>,
    active: &HashSet<&'a str>,
    out: &mut Vec<TreeSegment>,
) -> usize {
    let child_ids = |id: &str| children.get(id).map(Vec::as_slice).unwrap_or(&[]);

    // The chain collects every entry we pass; the last is the segment head.
    let mut chain: Vec<&str> = vec![start];
    let mut cur = start;
    while let [only] = child_ids(cur) {
        cur = *only;
        chain.push(cur);
    }
    let head = cur;
    let kids = child_ids(head);
    let is_leaf = kids.is_empty();

    let seg_entries: Vec<&ConversationEntry> = chain
        .iter()
        .filter_map(|id| by_id.get(id).copied())
        .collect();
    let message_count = seg_entries
        .iter()
        .filter(|e| matches!(e.entry, ConversationEntryKind::Message { .. }))
        .count();
    let label = segment_label(&seg_entries);
    let last_timestamp = by_id.get(head).and_then(|e| e.timestamp);
    let on_active_path = chain.iter().any(|id| active.contains(id));

    // Reserve this segment's slot before recursing so children point back at
    // an already-pushed index (DFS pre-order keeps a parent ahead of its
    // descendants).
    let index = out.len();
    out.push(TreeSegment {
        head: head.to_string(),
        label,
        message_count,
        last_timestamp,
        parent,
        children: Vec::new(),
        on_active_path,
        is_leaf,
    });

    let mut child_indices = Vec::with_capacity(kids.len());
    for kid in kids {
        child_indices.push(build_segment(
            kid,
            Some(index),
            by_id,
            children,
            active,
            out,
        ));
    }
    out[index].children = child_indices;
    index
}

/// The segment's display label: its first user message text, falling back to
/// the first message of any role, then to a dim kind placeholder for a
/// message-free segment.
fn segment_label(entries: &[&ConversationEntry]) -> String {
    if let Some(text) = entries.iter().copied().find_map(user_message_text) {
        return text;
    }
    if let Some(text) = entries.iter().copied().find_map(any_message_text) {
        return text;
    }
    entries
        .first()
        .map(|e| kind_placeholder(&e.entry).to_string())
        .unwrap_or_default()
}

/// Non-empty text of a user-role message entry, if any.
fn user_message_text(entry: &ConversationEntry) -> Option<String> {
    let ConversationEntryKind::Message { message } = &entry.entry else {
        return None;
    };
    match message.as_stored_wire()? {
        Message::User(u) => first_text(&u.content),
        _ => None,
    }
}

/// Non-empty text of a message entry of any role, if any.
fn any_message_text(entry: &ConversationEntry) -> Option<String> {
    let ConversationEntryKind::Message { message } = &entry.entry else {
        return None;
    };
    match message.as_stored_wire()? {
        Message::User(u) => first_text(&u.content),
        Message::ToolResult(r) => first_text(&r.content),
        Message::Assistant(a) => a.content.iter().find_map(|c| match c {
            AssistantContent::Text(t) => non_empty(&t.text),
            _ => None,
        }),
    }
}

/// Text of the first non-empty [`UserContent::Text`] block, if any.
fn first_text(content: &[UserContent]) -> Option<String> {
    content.iter().find_map(|c| match c {
        UserContent::Text(t) => non_empty(&t.text),
        _ => None,
    })
}

/// The trimmed text if non-empty, else `None`.
fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// A dim placeholder for a segment that carries no message text, keyed on its
/// first entry's kind.
fn kind_placeholder(kind: &ConversationEntryKind) -> &'static str {
    match kind {
        ConversationEntryKind::Message { .. } => "(message)",
        ConversationEntryKind::SystemPrompt { .. } => "(system prompt)",
        ConversationEntryKind::ModelChange { .. }
        | ConversationEntryKind::ThinkingChange { .. }
        | ConversationEntryKind::SpeedChange { .. }
        | ConversationEntryKind::VerbosityChange { .. } => "(settings)",
        ConversationEntryKind::EnvChange { .. } => "(environment)",
        ConversationEntryKind::SubAgentSpawn { .. } => "(subagent)",
        ConversationEntryKind::Compaction { .. } => "(compaction)",
    }
}

#[cfg(test)]
mod tests {

    use aj_agent::message::AgentMessage;
    use aj_models::types::{AssistantContent, AssistantMessage, Message, TextContent, UserMessage};

    use tempfile::TempDir;

    use super::*;
    use crate::log::{ConversationLog, ConversationView, ThreadFilter};
    use crate::persistence::ConversationPersistence;

    /// A scratch directory for one test's persistence state, removed when the
    /// returned guard drops. Callers must hold the guard for as long as they
    /// use the directory.
    fn fresh_sessions_dir() -> TempDir {
        TempDir::new().expect("create temp dir")
    }

    /// A log on disk, plus the guard that removes its directory. The log keeps
    /// writing to that directory, so a caller has to hold the guard for as long
    /// as it uses the log.
    fn new_log() -> (TempDir, ConversationLog) {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("prompt".to_string())
            .expect("set system prompt");
        (dir, log)
    }

    fn user_text(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::User(UserMessage::text(text)))
    }

    fn assistant_text(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            ..AssistantMessage::empty()
        }))
    }

    /// Append `message` to the user thread and return the new entry id.
    fn add(log: &mut ConversationLog, message: AgentMessage) -> EntryId {
        ConversationView::user(log)
            .add_message(message)
            .expect("append")
            .id
    }

    fn segment_with_head<'a>(tree: &'a SessionTree, head: &str) -> &'a TreeSegment {
        tree.segments
            .iter()
            .find(|s| s.head == head)
            .unwrap_or_else(|| panic!("no segment with head {head}"))
    }

    /// An empty (unpersisted) log has no segments.
    #[test]
    fn empty_log_has_no_segments() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let log = ConversationLog::create(&persistence).expect("create log");
        assert!(log.session_tree().segments.is_empty());
    }

    /// A branchless session is exactly one segment spanning the whole
    /// conversation, with its head at the last entry and the whole chain
    /// active.
    #[test]
    fn linear_session_is_one_segment() {
        let (_dir, mut log) = new_log();
        add(&mut log, user_text("first question"));
        add(&mut log, assistant_text("an answer"));
        let last = add(&mut log, user_text("second question"));

        let tree = log.session_tree();
        assert_eq!(tree.segments.len(), 1);
        let seg = &tree.segments[0];
        assert_eq!(seg.head, last, "head is the last entry (switch target)");
        assert_eq!(
            seg.label, "first question",
            "label is the first user message"
        );
        assert_eq!(seg.message_count, 3);
        assert_eq!(seg.parent, None);
        assert!(seg.children.is_empty());
        assert!(seg.is_leaf);
        assert!(seg.on_active_path, "the head's chain is active");
    }

    /// A fork mid-session yields the shared-prefix segment plus one child
    /// segment per branch, with correct heads, labels, and counts. Only the
    /// prefix and the branch holding the head are marked active.
    #[test]
    fn fork_mid_session_splits_into_parent_and_two_children() {
        let (_dir, mut log) = new_log();
        add(&mut log, user_text("shared question"));
        let fork = add(&mut log, assistant_text("shared answer"));

        // First branch off the fork point.
        log.set_head(fork.clone()).expect("set head to fork");
        let branch_a = add(&mut log, user_text("branch A"));

        // Second branch off the same fork point. The most recent append leaves
        // the head on branch B, so branch B is the active one.
        log.set_head(fork.clone()).expect("set head to fork");
        let branch_b = add(&mut log, user_text("branch B"));

        let tree = log.session_tree();
        assert_eq!(tree.segments.len(), 3);

        let prefix = segment_with_head(&tree, &fork);
        assert_eq!(prefix.label, "shared question");
        assert_eq!(prefix.message_count, 2);
        assert_eq!(prefix.parent, None);
        assert_eq!(prefix.children.len(), 2);
        assert!(!prefix.is_leaf);
        assert!(prefix.on_active_path);

        let seg_a = segment_with_head(&tree, &branch_a);
        assert_eq!(seg_a.label, "branch A");
        assert_eq!(seg_a.message_count, 1);
        assert!(seg_a.is_leaf);
        assert!(!seg_a.on_active_path, "the abandoned branch is not active");

        let seg_b = segment_with_head(&tree, &branch_b);
        assert_eq!(seg_b.label, "branch B");
        assert!(seg_b.is_leaf);
        assert!(seg_b.on_active_path, "the head's branch is active");

        // Both children point back at the prefix segment.
        let prefix_index = tree
            .segments
            .iter()
            .position(|s| s.head == fork)
            .expect("prefix index");
        assert_eq!(seg_a.parent, Some(prefix_index));
        assert_eq!(seg_b.parent, Some(prefix_index));
    }

    /// A fork at the very first user message yields two root segments, each
    /// anchored above at the virtual root (parent `None`).
    #[test]
    fn root_fork_yields_two_root_segments() {
        let (_dir, mut log) = new_log();
        let root_a = add(&mut log, user_text("root A"));

        // Branch at the first user message: its parent is the system-prompt
        // meta entry, so re-anchor the head there and append a sibling.
        let meta = log.system_prompt_id().cloned().expect("system prompt id");
        log.set_head(meta).expect("set head to system prompt");
        let root_b = add(&mut log, user_text("root B"));

        let tree = log.session_tree();
        assert_eq!(tree.segments.len(), 2);
        for head in [&root_a, &root_b] {
            let seg = segment_with_head(&tree, head);
            assert_eq!(seg.parent, None, "root segments hang off the virtual root");
            assert!(seg.is_leaf);
        }
        assert_eq!(segment_with_head(&tree, &root_a).label, "root A");
        assert_eq!(segment_with_head(&tree, &root_b).label, "root B");
    }

    /// A branch whose first (and only) entry is a settings record carries no
    /// message, so its label falls back to the dim `(settings)` placeholder.
    #[test]
    fn message_free_segment_falls_back_to_settings_label() {
        let (_dir, mut log) = new_log();
        add(&mut log, user_text("shared question"));
        let fork = add(&mut log, assistant_text("shared answer"));

        // One real branch.
        log.set_head(fork.clone()).expect("set head to fork");
        add(&mut log, user_text("real branch"));

        // A second branch whose only entry is a settings change (no message),
        // anchored back at the fork point.
        log.set_head(fork.clone()).expect("set head to fork");
        let settings = log
            .append_model_change(ThreadFilter::USER, "prov", "model")
            .expect("append settings entry")
            .id;

        let tree = log.session_tree();
        let seg = segment_with_head(&tree, &settings);
        assert_eq!(seg.label, "(settings)");
        assert_eq!(seg.message_count, 0);
        assert!(seg.is_leaf);
    }

    /// `on_active_path` marks exactly the segments on the head's root->head
    /// chain: a deeper active branch keeps its whole ancestry marked and every
    /// abandoned sibling unmarked.
    #[test]
    fn on_active_path_marks_only_the_head_chain() {
        let (_dir, mut log) = new_log();
        add(&mut log, user_text("root"));
        let fork = add(&mut log, assistant_text("fork answer"));

        // Abandoned branch.
        log.set_head(fork.clone()).expect("set head");
        let abandoned = add(&mut log, user_text("abandoned"));

        // Active branch, extended one entry deeper. The last append leaves the
        // head at its tip.
        log.set_head(fork.clone()).expect("set head");
        add(&mut log, user_text("active head"));
        let active_tip = add(&mut log, assistant_text("active tail"));

        let tree = log.session_tree();
        assert!(segment_with_head(&tree, &fork).on_active_path);
        assert!(segment_with_head(&tree, &active_tip).on_active_path);
        assert!(!segment_with_head(&tree, &abandoned).on_active_path);
        assert_eq!(
            log.head(),
            Some(&active_tip),
            "head sits at the active branch's tip",
        );
    }

    /// A session with a spawned sub-agent stays a single linear segment: the
    /// spawn root and the sub-agent's messages live on the sub thread, so the
    /// spawning assistant message never reads as a fork, and `message_count`
    /// counts only user-thread `Message` entries.
    #[test]
    fn sub_agent_thread_does_not_fork_the_segment() {
        use aj_agent::events::AgentSettings;

        let (_dir, mut log) = new_log();
        add(&mut log, user_text("first question"));
        let spawner = add(&mut log, assistant_text("spawning a sub-agent"));

        // A real sub-agent: a spawn root anchored at the spawning assistant
        // message, then one message on the sub thread. Neither advances the
        // user-thread head.
        let settings = AgentSettings {
            provider: "anthropic".into(),
            model_id: "claude".into(),
            thinking: "off".into(),
            thinking_display: String::new(),
            speed: "standard".into(),
            verbosity: "default".into(),
        };
        let spawn = log
            .append_subagent_spawn(1, spawner.clone(), "do the thing", false, &settings)
            .expect("spawn root")
            .id;
        {
            let mut view = ConversationView::subagent(&mut log, spawn, 1);
            view.add_message(user_text("subtask")).expect("sub message");
        }

        // The user thread continues after the spawn, chaining onto the
        // spawning assistant message (still the head).
        let last = add(&mut log, user_text("second question"));

        let tree = log.session_tree();
        assert_eq!(
            tree.segments.len(),
            1,
            "the sub-agent spawn does not split the segment"
        );
        let seg = &tree.segments[0];
        assert_eq!(seg.head, last, "head is the last user-thread entry");
        assert_eq!(
            seg.message_count, 3,
            "only user-thread Message entries counted (spawn root and sub message excluded)"
        );
        assert!(seg.is_leaf);
    }

    /// A nested fork from a real log: a fork whose active child branch itself
    /// forks. The segment tree's parent/children indices, heads, labels, and
    /// active-path marks are correct at every level (exercises `build_segment`
    /// recursion and parent-index threading through `session_tree`).
    #[test]
    fn nested_fork_threads_parents_children_and_active_path() {
        let (_dir, mut log) = new_log();
        add(&mut log, user_text("shared"));
        let fork1 = add(&mut log, assistant_text("shared answer"));

        // First-level fork: branch A (a leaf) and branch B (which forks again).
        log.set_head(fork1.clone()).expect("head to fork1");
        let branch_a = add(&mut log, user_text("branch A"));

        log.set_head(fork1.clone()).expect("head to fork1");
        add(&mut log, user_text("branch B"));
        let fork2 = add(&mut log, assistant_text("branch B answer"));

        // Second-level fork under branch B: B1 (leaf) and B2 (leaf, active).
        log.set_head(fork2.clone()).expect("head to fork2");
        let branch_b1 = add(&mut log, user_text("branch B1"));

        log.set_head(fork2.clone()).expect("head to fork2");
        let branch_b2 = add(&mut log, user_text("branch B2"));

        let tree = log.session_tree();
        assert_eq!(tree.segments.len(), 5);

        let idx = |head: &str| {
            tree.segments
                .iter()
                .position(|s| s.head == head)
                .unwrap_or_else(|| panic!("no segment with head {head}"))
        };
        let prefix = idx(&fork1);
        let seg_a = idx(&branch_a);
        // Branch B's segment runs [branch B, fork2], so its head is fork2.
        let seg_b = idx(&fork2);
        let seg_b1 = idx(&branch_b1);
        let seg_b2 = idx(&branch_b2);

        // The shared prefix is a root, forking to A and B in append order.
        assert_eq!(tree.segments[prefix].parent, None);
        assert_eq!(tree.segments[prefix].children, vec![seg_a, seg_b]);
        assert!(!tree.segments[prefix].is_leaf);

        // Branch A is a leaf under the prefix.
        assert_eq!(tree.segments[seg_a].parent, Some(prefix));
        assert!(tree.segments[seg_a].is_leaf);
        assert_eq!(tree.segments[seg_a].label, "branch A");

        // Branch B is itself a fork under the prefix, splitting to B1 and B2.
        assert_eq!(tree.segments[seg_b].parent, Some(prefix));
        assert_eq!(tree.segments[seg_b].children, vec![seg_b1, seg_b2]);
        assert_eq!(tree.segments[seg_b].label, "branch B");
        assert!(!tree.segments[seg_b].is_leaf);

        // The second-level leaves point back at branch B's segment.
        assert_eq!(tree.segments[seg_b1].parent, Some(seg_b));
        assert!(tree.segments[seg_b1].is_leaf);
        assert_eq!(tree.segments[seg_b2].parent, Some(seg_b));
        assert!(tree.segments[seg_b2].is_leaf);

        // The head sits at B2 (the last append), so its whole ancestry is
        // active and every abandoned sibling is not.
        assert!(tree.segments[prefix].on_active_path);
        assert!(tree.segments[seg_b].on_active_path);
        assert!(tree.segments[seg_b2].on_active_path);
        assert!(!tree.segments[seg_a].on_active_path);
        assert!(!tree.segments[seg_b1].on_active_path);
    }

    /// A head parked on an interior entry of a segment (not its tip) still
    /// marks that segment via the any-entry-in-chain rule, and nothing below
    /// the head (the branches under the segment's terminating fork) is marked.
    #[test]
    fn interior_head_marks_its_segment_but_nothing_below() {
        let (_dir, mut log) = new_log();
        add(&mut log, user_text("shared"));
        let mid = add(&mut log, assistant_text("interior"));
        add(&mut log, user_text("more shared"));
        let fork = add(&mut log, assistant_text("fork answer"));

        // Two branches off the fork, so the prefix segment ends at a fork with
        // children below it.
        log.set_head(fork.clone()).expect("head to fork");
        let branch_a = add(&mut log, user_text("branch A"));
        log.set_head(fork.clone()).expect("head to fork");
        let branch_b = add(&mut log, user_text("branch B"));

        // Park the head on an interior entry of the prefix segment, above the
        // fork and its branches.
        log.set_head(mid.clone()).expect("head to interior");

        let tree = log.session_tree();
        // The prefix segment [shared, mid, more shared, fork] holds the head.
        assert!(
            segment_with_head(&tree, &fork).on_active_path,
            "the segment holding the interior head is marked"
        );
        // Nothing below the head (the branches under the fork) is marked.
        assert!(!segment_with_head(&tree, &branch_a).on_active_path);
        assert!(!segment_with_head(&tree, &branch_b).on_active_path);
        assert_eq!(log.head(), Some(&mid), "head is the interior entry");
    }
}
