//! `~/.aj/` configuration and the agent's working environment.
//!
//! Three concerns, one per module: `schema` (the `config.toml` schema,
//! parser, and writer), `paths` (the `~/.aj/` path resolvers and git-root
//! discovery), and `env` (the [`AgentEnv`] runtime environment and context
//! files). [`skills`] discovers SKILL.md directories. The public surface is
//! re-exported here so callers use `aj_conf::Config`, `aj_conf::AgentEnv`,
//! and friends without naming the inner modules.

pub mod skills;

mod env;
mod paths;
mod schema;

pub use env::{
    AGENTS_MD_PREFIX, AgentEnv, ContextFile, ContextFileKind, SystemPrompt, SystemPromptSource,
    USER_AGENTS_MD_PREFIX,
};
pub use paths::display_path;
pub use schema::{
    Config, ConfigDiagnostic, ConfigError, ConfigOption, ConfigSpeed, ConfigThinkingDisplay,
    ConfigThinkingLevel, ConfigVerbosity, Severity, ValueKind,
};

/// Unique temp directory for tests that need real filesystem scratch
/// space without pulling in `tempfile`. Shared across the module test
/// suites.
#[cfg(test)]
pub(crate) fn test_temp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("aj-conf-test-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
