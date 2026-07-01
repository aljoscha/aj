//! Argument parsing for the `aj` binary, plus resolution of `@file`
//! arguments into prompt content.
//!
//! The CLI surface is split into [`args`] (the [`clap`]-derived `Args`
//! struct + dispatch enums) and [`file_args`] (turning `@path`
//! arguments into `<file>`-wrapped text and image attachments).
//! [`initial_input`] ties them together: it interprets the positional
//! arguments as a mix of `@file` attachments and free-form messages and
//! produces the content to auto-submit as the launch turn.

pub mod args;
pub mod file_args;

use std::path::Path;

use anyhow::Result;

use aj_models::types::UserContent;

use crate::cli::args::{Args, Command};

/// The prompt content supplied on the command line, ready to submit.
///
/// Positional arguments are interpreted argument-by-argument: any
/// argument starting with `@` is a file attachment, the rest are
/// free-form messages joined with spaces. The resolved file text and
/// the joined messages form a single launch turn (carrying any image
/// attachments) that both modes auto-submit.
pub struct InitialInput {
    /// Combined launch text: resolved `<file>` blocks followed by the
    /// joined messages. `None` when there is neither file text nor a
    /// message.
    message: Option<String>,
    /// Image attachments for the launch turn.
    images: Vec<UserContent>,
}

impl InitialInput {
    /// Whether nothing was supplied on the command line (no files, no
    /// messages). Print mode treats this as a hard error.
    pub fn is_empty(&self) -> bool {
        self.message.is_none() && self.images.is_empty()
    }

    /// Content blocks for the auto-submitted launch turn: the combined
    /// message text (if any) followed by the image attachments.
    pub fn into_content(self) -> Vec<UserContent> {
        let mut content = Vec::new();
        if let Some(text) = self.message {
            content.push(UserContent::text(text));
        }
        content.extend(self.images);
        content
    }
}

/// Resolve the command-line positionals into the launch turn content,
/// relative to `cwd` (for `@file` path resolution).
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
            None => messages.push(token.as_str()),
        }
    }

    let resolved = file_args::process_file_args(&file_args, cwd)?;

    // File text is prepended to the joined messages so the model sees
    // the attachments before the question, all as one launch turn.
    let mut message = resolved.text;
    if !messages.is_empty() {
        message.push_str(&messages.join(" "));
    }
    let message = if message.is_empty() {
        None
    } else {
        Some(message)
    };

    Ok(InitialInput {
        message,
        images: resolved.images,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser;

    use crate::cli::args::Args;
    use crate::cli::initial_input;

    /// Flatten the launch turn's text content for assertions.
    fn content_text(args: &[&str]) -> Option<String> {
        let parsed = Args::parse_from(args);
        let content = initial_input(&parsed, Path::new("/"))
            .expect("resolve")
            .into_content();
        if content.is_empty() {
            return None;
        }
        Some(
            content
                .iter()
                .filter_map(|c| match c {
                    aj_models::types::UserContent::Text(t) => Some(t.text.clone()),
                    aj_models::types::UserContent::Image(_) => None,
                })
                .collect(),
        )
    }

    #[test]
    fn single_message_is_used() {
        assert_eq!(content_text(&["aj", "hello"]).as_deref(), Some("hello"));
    }

    #[test]
    fn bare_messages_are_joined() {
        assert_eq!(
            content_text(&["aj", "first", "second"]).as_deref(),
            Some("first second")
        );
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
            content_text(&["aj", "continue", "ID", "do", "thing"]).as_deref(),
            Some("do thing")
        );
    }
}
