//! Argument parsing for the `aj` binary, plus `@file`
//! expansion for prompt arguments.
//!
//! Per `docs/aj-next-plan.md` §4 the CLI surface is split into
//! [`args`] (the [`clap`]-derived `Args` struct + dispatch enums)
//! and [`file_args`] (expansion of `@path` references in the
//! free-form prompt).

pub mod args;
pub mod file_args;

use anyhow::{Context, Result};

use crate::cli::args::{Args, Command};

/// Resolve the initial prompt supplied on the command line.
///
/// The prompt can arrive in either positional slot — the top-level
/// `aj <prompt...>` or `aj continue ID <prompt...>`. Clap's greedy
/// positional consumption keeps the two disjoint, so we take
/// whichever is populated, join the parts with spaces, and expand any
/// `@path` tokens via [`file_args::expand`].
///
/// Returns `None` when neither slot is populated. Print mode treats
/// that as a hard error (it is one-shot, with no editor to fall back
/// on); interactive mode treats it as "open with an empty editor".
pub fn initial_prompt(args: &Args) -> Result<Option<String>> {
    let prompt_parts: &[String] = match &args.command {
        Some(Command::Continue { prompt, .. }) if !prompt.is_empty() => prompt,
        _ => &args.prompt,
    };
    if prompt_parts.is_empty() {
        return Ok(None);
    }
    let joined = prompt_parts.join(" ");
    let expanded =
        file_args::expand(joined).context("failed to expand @file references in prompt")?;
    Ok(Some(expanded))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::args::Args;
    use crate::cli::initial_prompt;

    #[test]
    fn joins_positional_prompt_args() {
        let args = Args::parse_from(["aj", "hello", "world"]);
        assert_eq!(
            initial_prompt(&args).unwrap().as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn none_when_no_prompt_given() {
        let args = Args::parse_from(["aj"]);
        assert_eq!(initial_prompt(&args).unwrap(), None);
    }

    #[test]
    fn prefers_continue_prompt_slot() {
        let args = Args::parse_from(["aj", "continue", "ID", "do", "thing"]);
        assert_eq!(initial_prompt(&args).unwrap().as_deref(), Some("do thing"));
    }

    #[test]
    fn continue_without_prompt_is_none() {
        let args = Args::parse_from(["aj", "continue", "ID"]);
        assert_eq!(initial_prompt(&args).unwrap(), None);
    }
}
