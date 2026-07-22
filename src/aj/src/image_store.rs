//! Per-session store mapping tool-result-image entries to their transmitted
//! kitty-graphics ids.
//!
//! The host owns one [`ImageStore`], shared by `Rc<RefCell<..>>` into the
//! transcript builder. The builder records the `(AgentId, EntryId)` of any
//! visible image it wants to draw but has not transmitted yet in the pending
//! set. After a frame the host drains that set, transmits the bytes, and
//! records the returned id in `transmitted`. A recorded id is kept for the
//! session's lifetime, even after its entry scrolls off screen, so scrolling
//! back never re-transmits. Ids are freed and the collections cleared on a
//! session switch, since transmitted ids belong to one session's terminal
//! graphics memory.

use std::collections::{HashMap, HashSet};

use aj_agent::events::AgentId;
use aj_app::chat::EntryId;

/// How a tool-result-image entry renders this frame.
///
/// Making the three states explicit lets the tool cell force text when images
/// are off, reserve blank rows while a transmit is pending, and place the
/// image once its id arrives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ImageRender {
    /// Images are off (the terminal lacks the capability or the
    /// `show_image_in_terminal` config is false), or the entry is not a
    /// tool-result image. Renders the `[image: ...]` text fallback.
    Disabled,
    /// Enabled, but the id has not arrived yet. Reserves the footprint's rows
    /// blank so the image popping in one frame later does not shift layout.
    Pending,
    /// Enabled, but transmission gave up on this image because its bytes will
    /// not decode into a valid image. Terminal, never retried, and renders the
    /// `[image: ...]` text fallback like `Disabled`.
    Failed,
    /// Transmitted. Places the image at the reserved footprint.
    Transmitted(u32),
}

impl ImageRender {
    /// The value the per-entry render fingerprint folds to notice this image's
    /// render state changed.
    ///
    /// `Disabled` and `Pending` fold identically: the only transition between
    /// them is a `show_image_in_terminal` toggle, a session-wide input the
    /// wholesale `GlobalRenderInputs` clear owns. The per-entry axis is the
    /// transmit lifecycle, so `Pending` -> `Transmitted` (the id lands) and
    /// `Pending` -> `Failed` (the transmit gave up) each fold to a distinct
    /// value and rebuild the entry, swapping the blank reserve for the image or
    /// the text fallback.
    pub(crate) fn render_tag(self) -> (u8, u32) {
        match self {
            ImageRender::Disabled | ImageRender::Pending => (0, 0),
            ImageRender::Failed => (1, 0),
            ImageRender::Transmitted(id) => (2, id),
        }
    }
}

/// Maps tool-result-image entries to transmitted kitty-graphics ids, with a
/// pending set for images the builder wants but has not transmitted yet.
///
/// Keyed by `(AgentId, EntryId)` to match the transcript render cache's key,
/// so the two stay aligned. Per session: [`drain_ids`](ImageStore::drain_ids)
/// clears both collections on a switch.
#[derive(Default)]
pub(crate) struct ImageStore {
    /// Entries whose bytes are transmitted, mapped to their terminal id.
    transmitted: HashMap<(AgentId, EntryId), u32>,
    /// Visible entries the builder wants to draw but that are not transmitted
    /// yet. Drained by the host after each frame.
    pending: HashSet<(AgentId, EntryId)>,
    /// Entries whose transmit gave up on undecodable bytes. Terminal: kept so
    /// the builder renders the text fallback and never re-records them pending.
    failed: HashSet<(AgentId, EntryId)>,
}

impl ImageStore {
    /// The transmitted id for `(agent, entry)`, if any.
    pub(crate) fn get(&self, agent: AgentId, entry: EntryId) -> Option<u32> {
        self.transmitted.get(&(agent, entry)).copied()
    }

    /// Record a visible-but-untransmitted image so the host transmits it after
    /// the frame. Idempotent within a frame (a set).
    pub(crate) fn record_pending(&mut self, agent: AgentId, entry: EntryId) {
        self.pending.insert((agent, entry));
    }

    /// Drain the pending set, returning the keys recorded this frame.
    pub(crate) fn take_pending(&mut self) -> Vec<(AgentId, EntryId)> {
        self.pending.drain().collect()
    }

    /// Record the id a successful transmit allocated for `(agent, entry)`.
    pub(crate) fn insert(&mut self, agent: AgentId, entry: EntryId, img_id: u32) {
        self.transmitted.insert((agent, entry), img_id);
    }

    /// Whether `(agent, entry)`'s transmit gave up, so it renders text and is
    /// never retried.
    pub(crate) fn is_failed(&self, agent: AgentId, entry: EntryId) -> bool {
        self.failed.contains(&(agent, entry))
    }

    /// Mark `(agent, entry)`'s transmit as given up. Terminal for the session:
    /// the builder stops recording it pending, so the host stops re-attempting
    /// it every frame, and the cell falls back to text.
    pub(crate) fn mark_failed(&mut self, agent: AgentId, entry: EntryId) {
        self.failed.insert((agent, entry));
    }

    /// Return every transmitted id and clear both collections, for freeing the
    /// outgoing session's terminal graphics memory on a session switch.
    pub(crate) fn drain_ids(&mut self) -> Vec<u32> {
        let ids = self.transmitted.values().copied().collect();
        self.transmitted.clear();
        self.pending.clear();
        self.failed.clear();
        ids
    }
}

#[cfg(test)]
mod tests {
    use aj_app::chat::{EntryKind, NoticeEntry, NoticeLevel, Transcript};

    use super::*;

    /// Mint `n` distinct `EntryId`s through a fresh transcript, the only
    /// path that allocates them.
    fn entry_ids(n: usize) -> Vec<EntryId> {
        let mut t = Transcript::default();
        (0..n)
            .map(|i| {
                t.append(EntryKind::Notice(NoticeEntry {
                    level: NoticeLevel::Info,
                    text: format!("notice {i}"),
                }))
            })
            .collect()
    }

    #[test]
    fn record_pending_then_take_pending_drains() {
        let ids = entry_ids(2);
        let mut store = ImageStore::default();
        store.record_pending(AgentId::Main, ids[0]);
        store.record_pending(AgentId::Main, ids[1]);
        // A repeat within the frame does not duplicate the key.
        store.record_pending(AgentId::Main, ids[0]);

        let mut drained = store.take_pending();
        drained.sort_by_key(|(_, e)| *e);
        assert_eq!(
            drained,
            vec![(AgentId::Main, ids[0]), (AgentId::Main, ids[1])],
        );
        assert!(store.take_pending().is_empty(), "the set drained");
    }

    #[test]
    fn insert_then_get_returns_the_id() {
        let ids = entry_ids(2);
        let mut store = ImageStore::default();
        assert_eq!(store.get(AgentId::Main, ids[0]), None);
        store.insert(AgentId::Main, ids[0], 42);
        assert_eq!(store.get(AgentId::Main, ids[0]), Some(42));
        // A different key is unaffected.
        assert_eq!(store.get(AgentId::Main, ids[1]), None);
    }

    #[test]
    fn drain_ids_returns_ids_and_clears_both() {
        let ids = entry_ids(3);
        let mut store = ImageStore::default();
        store.insert(AgentId::Main, ids[0], 10);
        store.insert(AgentId::Main, ids[1], 20);
        store.record_pending(AgentId::Main, ids[2]);

        let mut drained = store.drain_ids();
        drained.sort_unstable();
        assert_eq!(drained, vec![10, 20]);
        // Both the map and the pending set are empty afterward.
        assert_eq!(store.get(AgentId::Main, ids[0]), None);
        assert!(store.take_pending().is_empty());
    }

    #[test]
    fn mark_failed_is_terminal_and_cleared_on_drain() {
        let ids = entry_ids(2);
        let mut store = ImageStore::default();
        assert!(!store.is_failed(AgentId::Main, ids[0]));
        store.mark_failed(AgentId::Main, ids[0]);
        assert!(store.is_failed(AgentId::Main, ids[0]));
        // A different key is unaffected.
        assert!(!store.is_failed(AgentId::Main, ids[1]));
        // A session switch clears the failed set along with the rest.
        store.drain_ids();
        assert!(!store.is_failed(AgentId::Main, ids[0]));
    }
}
