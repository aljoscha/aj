//! Argument parsing for the `aj` binary, plus resolution of `@file`
//! arguments into prompt content.
//!
//! The CLI surface is split into [`args`] (the [`clap`]-derived `Args`
//! struct + dispatch enums) and [`file_args`] (turning `@path`
//! arguments into `<file>`-wrapped text and image attachments).
//! [`initial_input`] ties them together: it interprets the positional
//! arguments as a mix of `@file` attachments and free-form messages and
//! produces the turns to run at launch.

pub mod args;
pub mod file_args;

use std::path::Path;

use anyhow::Result;

use aj_models::types::UserContent;

use crate::cli::args::{Args, Command};

/// The prompt content supplied on the command line, ready to run.
///
/// Positional arguments are interpreted argument-by-argument: any
/// argument starting with `@` is a file attachment, the rest are
/// free-form messages. The first message is concatenated onto the
/// resolved file text to form the initial turn (carrying the image
/// attachments); each remaining message becomes its own follow-up
/// turn, run in order.
pub struct InitialInput {
    /// Combined text for the first turn: resolved `<file>` blocks
    /// followed by the first free-form message. `None` when there is
    /// neither file text nor a message.
    message: Option<String>,
    /// Image attachments for the first turn.
    images: Vec<UserContent>,
    /// Additional free-form messages, each run as its own turn after
    /// the first, in order.
    followups: Vec<String>,
}

impl InitialInput {
    /// Whether nothing was supplied on the command line (no files, no
    /// messages). Print mode treats this as a hard error.
    pub fn is_empty(&self) -> bool {
        self.message.is_none() && self.images.is_empty() && self.followups.is_empty()
    }

    /// Flatten into an ordered list of turns, each a non-empty content
    /// block vector. The first turn carries the combined message text
    /// and image attachments; subsequent turns are the single-text
    /// follow-up messages. Both modes auto-submit these in order.
    pub fn into_turns(self) -> Vec<Vec<UserContent>> {
        let mut turns = Vec::new();

        let mut first = Vec::new();
        if let Some(text) = self.message {
            first.push(UserContent::text(text));
        }
        first.extend(self.images);
        if !first.is_empty() {
            turns.push(first);
        }

        for followup in self.followups {
            turns.push(vec![UserContent::text(followup)]);
        }
        turns
    }
}

/// Resolve the command-line positionals into the turns to run at
/// launch, relative to `cwd` (for `@file` path resolution).
///
/// The positionals come from whichever slot clap populated: the
/// top-level `aj <args...>` or `aj continue ID <args...>` (its greedy
/// positional consumption keeps the two disjoint). `@file` arguments
/// are resolved into `<file>` text + image attachments; a missing file
/// is an error.
pub fn initial_input(args: &Args, cwd: &Path) -> Result<InitialInput> {
    let positionals: &[String] = match &args.command {
        Some(Command::Continue { prompt, .. }) if !prompt.is_empty() => prompt,
        _ => &args.prompt,
    };

    let mut file_args = Vec::new();
    let mut messages = Vec::new();
    for token in positionals {
        match token.strip_prefix('@') {
            Some(path) => file_args.push(path.to_string()),
            None => messages.push(token.clone()),
        }
    }

    let resolved = file_args::process_file_args(&file_args, cwd)?;

    // The first message rides along with the file text into the initial
    // turn (matching how a user would paste files then ask about them);
    // the rest are sequential follow-up turns.
    let mut messages = messages.into_iter();
    let mut parts = Vec::new();
    if !resolved.text.is_empty() {
        parts.push(resolved.text);
    }
    if let Some(first) = messages.next() {
        parts.push(first);
    }
    let message = if parts.is_empty() {
        None
    } else {
        Some(parts.concat())
    };

    Ok(InitialInput {
        message,
        images: resolved.images,
        followups: messages.collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser;

    use crate::cli::args::Args;
    use crate::cli::initial_input;

    /// Flatten the first turn's text content for assertions.
    fn first_turn_text(args: &[&str]) -> Option<String> {
        let parsed = Args::parse_from(args);
        let turns = initial_input(&parsed, Path::new("/"))
            .expect("resolve")
            .into_turns();
        turns.into_iter().next().map(|content| {
            content
                .iter()
                .filter_map(|c| match c {
                    aj_models::types::UserContent::Text(t) => Some(t.text.clone()),
                    aj_models::types::UserContent::Image(_) => None,
                })
                .collect()
        })
    }

    #[test]
    fn first_positional_becomes_first_turn() {
        assert_eq!(first_turn_text(&["aj", "hello"]).as_deref(), Some("hello"));
    }

    #[test]
    fn bare_positionals_are_separate_turns() {
        let parsed = Args::parse_from(["aj", "first", "second"]);
        let turns = initial_input(&parsed, Path::new("/"))
            .expect("resolve")
            .into_turns();
        assert_eq!(turns.len(), 2);
    }

    #[test]
    fn empty_when_no_positionals() {
        let parsed = Args::parse_from(["aj"]);
        assert!(
            initial_input(&parsed, Path::new("/"))
                .expect("resolve")
                .is_empty()
        );
    }

    #[test]
    fn prefers_continue_slot() {
        assert_eq!(
            first_turn_text(&["aj", "continue", "ID", "do", "thing"]).as_deref(),
            Some("do")
        );
    }
}
