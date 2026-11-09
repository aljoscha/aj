//! OSC 7501 reports the selected session to the terminal. No text from the
//! model is interpreted as a request for user action.

use std::io::{self, Write};

use aj_agent::tool::TaskStatus;
use aj_app::chat::ChatState;
use aj_app::client::SessionClient;
use aj_wire::TurnOutcome;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    Working,
    Done,
    Error,
    Auth,
    Clear,
}

impl Status {
    pub(crate) fn for_session(
        client: &SessionClient,
        chat: &ChatState,
        directory_working: bool,
        available: bool,
        auth_pending: bool,
    ) -> Self {
        if auth_pending {
            return Self::Auth;
        }
        if !available {
            return Self::Clear;
        }
        if directory_working
            || client.working()
            || (chat
                .goal
                .as_ref()
                .is_some_and(|goal| goal.status == aj_agent::goal::GoalStatus::Active)
                && client.turn_outcome() != Some(TurnOutcome::Waiting))
            || !client.lifecycle().running_agents().is_empty()
            || !client.lifecycle().compacting_agents().is_empty()
            || chat
                .tasks()
                .values()
                .any(|task| task.status == TaskStatus::Running)
        {
            return Self::Working;
        }
        match client.turn_outcome() {
            Some(TurnOutcome::Completed) => Self::Done,
            Some(TurnOutcome::Failed) => Self::Error,
            None | Some(TurnOutcome::Cancelled | TurnOutcome::Waiting) => Self::Idle,
        }
    }

    fn wire(self) -> &'static [u8] {
        match self {
            Self::Idle => b"\x1b]7501;state=idle:app=aj\x1b\\",
            Self::Working => b"\x1b]7501;state=working:app=aj\x1b\\",
            Self::Done => b"\x1b]7501;state=done:app=aj\x1b\\",
            Self::Error => b"\x1b]7501;state=error:app=aj\x1b\\",
            Self::Auth => b"\x1b]7501;state=blocked:kind=auth:app=aj\x1b\\",
            Self::Clear => b"\x1b]7501;state=clear\x1b\\",
        }
    }
}

pub(crate) struct Reporter {
    enabled: bool,
    sent: Option<Status>,
}

impl Reporter {
    pub(crate) fn new() -> Self {
        Self {
            // Older VTE parsers can leak unknown OSCs onto the screen. TUI
            // output uses /dev/tty, so redirected stdout is not a capability
            // test. Other terminals may safely ignore this optional protocol.
            enabled: supported_vte(std::env::var("VTE_VERSION").ok().as_deref()),
            sent: None,
        }
    }

    /// Send only changes. A failed write or flush remains retryable.
    pub(crate) fn update(&mut self, writer: &mut dyn Write, status: Status) -> io::Result<()> {
        if self.enabled && self.sent != Some(status) {
            writer.write_all(status.wire())?;
            writer.flush()?;
            self.sent = Some(status);
        }
        Ok(())
    }
}

fn supported_vte(version: Option<&str>) -> bool {
    version
        .and_then(|v| v.parse::<u32>().ok())
        .is_none_or(|v| v >= 5400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_changes_and_retries_failed_writes() {
        let mut reporter = Reporter {
            enabled: true,
            sent: None,
        };
        assert!(reporter.update(&mut io::sink(), Status::Working).is_ok());
        let mut bytes = Vec::new();
        reporter.update(&mut bytes, Status::Working).unwrap();
        assert!(bytes.is_empty());
        let mut full = &mut [][..];
        assert!(reporter.update(&mut full, Status::Auth).is_err());
        reporter.update(&mut bytes, Status::Auth).unwrap();
        reporter.update(&mut bytes, Status::Clear).unwrap();
        assert_eq!(
            bytes,
            b"\x1b]7501;state=blocked:kind=auth:app=aj\x1b\\\x1b]7501;state=clear\x1b\\"
        );
    }

    #[test]
    fn only_known_unsafe_vte_versions_disable_reporting() {
        for version in [None, Some(""), Some("unknown"), Some("5400"), Some("8000")] {
            assert!(supported_vte(version));
        }
        assert!(!supported_vte(Some("5300")));
    }
}
