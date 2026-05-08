//! Replay a persisted [`ConversationLog`](crate::log::ConversationLog)
//! as an iterator over its [`ConversationEntry`]s.
//!
//! Resuming a thread should look the same to a frontend as a live
//! run: the renderer consumes a single typed event stream regardless
//! of whether the events came from a running agent or from a
//! previously recorded log on disk. `replay` is the bridge between
//! disk and that pipeline.
//!
//! See `docs/aj-next-plan.md` §2.5 for the binary-side wiring (the
//! `aj` / `aj-next` binary opens a log, registers persistence and
//! renderer listeners on the agent, then drains `replay(...)` into
//! the renderer pipeline before entering its input loop).
//!
//! ## Current shape
//!
//! `replay` returns an iterator of [`&ConversationEntry`] — the raw
//! log payload in append order. The aj-next plan’s long-term target
//! is `Iterator<Item = aj_agent::events::AgentEvent>` so the same
//! pump that drives live runs also drives replay. We can’t return
//! that today: `aj-agent` still imports
//! [`ConversationLog`](crate::log::ConversationLog) (and friends)
//! from this crate during the §2.0–§2.3 migration, so a
//! `aj-session → aj-agent` dependency would create a cycle.
//!
//! Once §2.4 lands and `aj-agent` no longer depends on
//! `aj-session`, this signature flips to emit `AgentEvent` directly
//! and the binary call site stops doing the projection itself. The
//! call sites already drive an iterator, so the upgrade is local to
//! this module.

use crate::log::{ConversationEntry, ConversationLog};

/// Walk `log` in append order and yield each persisted entry in turn.
///
/// The iterator borrows from `log` so the caller doesn't pay for a
/// full clone of the in-memory entry table; collect into a `Vec` if
/// the events need to outlive the log.
pub fn replay(log: &ConversationLog) -> impl Iterator<Item = &ConversationEntry> + '_ {
    log.entries_in_order().into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{ConversationEntryKind, ConversationLog, ConversationView};
    use crate::persistence::ConversationPersistence;
    use aj_models::messages::ContentBlockParam;
    use std::path::PathBuf;

    fn fresh_threads_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "aj-session-replay-test-{pid}-{tid:?}-{nanos}",
            pid = std::process::id(),
            tid = std::thread::current().id(),
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn replay_yields_entries_in_append_order() {
        // Append a SystemPrompt root, then a user message, then an
        // assistant message. `replay` should yield them in exactly
        // the same order they were appended, so a renderer driving
        // the same iterator sees a faithful reconstruction of the
        // original turn sequence.
        let dir = fresh_threads_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("user msg");
            view.add_assistant_message(vec![ContentBlockParam::new_text_block("hello".into())])
                .expect("assistant msg");
        }

        let replayed: Vec<&ConversationEntry> = replay(&log).collect();
        assert_eq!(replayed.len(), 3);
        assert!(matches!(
            replayed[0].entry,
            ConversationEntryKind::SystemPrompt { .. }
        ));
        assert!(matches!(
            replayed[1].entry,
            ConversationEntryKind::Message(_)
        ));
        assert!(matches!(
            replayed[2].entry,
            ConversationEntryKind::Message(_)
        ));
    }
}
