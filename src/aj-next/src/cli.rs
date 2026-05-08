//! Argument parsing for the `aj-next` binary, plus `@file`
//! expansion for prompt arguments.
//!
//! Per `docs/aj-next-plan.md` §4 the CLI surface is split into
//! [`args`] (the [`clap`]-derived `Args` struct + dispatch enums)
//! and [`file_args`] (expansion of `@path` references in the
//! free-form prompt).

pub mod args;
pub mod file_args;
