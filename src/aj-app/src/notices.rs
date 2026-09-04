//! Frontend-agnostic startup notices: the session's `Context:` record and the
//! sandbox warning.
//!
//! The context record is written into the session log at creation and
//! rendered by replay ([`SessionContext::notice`]), so every frontend shows
//! the same listing at the top of a session for as long as it exists. The
//! sandbox warning is about the process running the agent, not the session,
//! and stays a live row.

use aj_conf::{AgentEnv, SystemPromptSource, display_path};
use aj_session::{ContextFileRecord, ContextSkillRecord, SessionContext};

/// What `env`'s system prompt is assembled from, as the record the session
/// log keeps: the prompt's source, every instruction file in the order they
/// are stitched in, and every discovered skill with whether it reaches the
/// model's listing. Paths are abbreviated the way this host displays them.
pub fn session_context(env: &AgentEnv) -> SessionContext {
    session_context_with_display(env, display_path)
}

fn session_context_with_display(
    env: &AgentEnv,
    display: impl Fn(&std::path::Path) -> String,
) -> SessionContext {
    SessionContext {
        system_prompt: match &env.system_prompt.source {
            SystemPromptSource::Builtin => None,
            SystemPromptSource::Override(path) => Some(display(path)),
        },
        files: env
            .context_files
            .iter()
            .map(|file| ContextFileRecord {
                path: display(&file.path),
                kind: file.kind.label().to_string(),
            })
            .collect(),
        skills: env
            .skills
            .iter()
            .map(|skill| ContextSkillRecord {
                path: display(&skill.path),
                name: skill.name.clone(),
                enabled: skill.enabled,
                model_invocation: !skill.disable_model_invocation,
            })
            .collect(),
    }
}

/// The exact sandbox-warning string the binary emits at startup
/// unless `AJ_DISABLE_SANDBOX_WARNING` is set in the environment.
/// Kept in a `const` so it's easy to assert on in tests.
pub const SANDBOX_WARNING: &str = "WARNING: AJ has no sandboxing or permission checks. The agent can execute \
     arbitrary commands on your system. Do not use AJ if you don't understand what \
     this means. Set AJ_DISABLE_SANDBOX_WARNING=1 to suppress this warning.";

/// Returns `true` when the sandbox warning should be shown, i.e. when
/// `AJ_DISABLE_SANDBOX_WARNING` is unset in the environment.
///
/// Setting the var to any value (including the empty string) suppresses the
/// warning.
pub fn sandbox_warning_enabled() -> bool {
    sandbox_warning_enabled_for(std::env::var_os("AJ_DISABLE_SANDBOX_WARNING").is_some())
}

fn sandbox_warning_enabled_for(disable_var_is_present: bool) -> bool {
    !disable_var_is_present
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use aj_conf::{
        AgentEnv, ContextFile, ContextFileKind, SystemPrompt, SystemPromptSource, skills::Skill,
    };

    use super::{SANDBOX_WARNING, sandbox_warning_enabled_for, session_context_with_display};

    /// Build an [`AgentEnv`] for the record tests. Working directory / OS /
    /// date / git root are stubbed: only `system_prompt`, `context_files`, and
    /// `skills` matter here.
    fn env_with(context_files: Vec<ContextFile>) -> AgentEnv {
        AgentEnv {
            working_directory: PathBuf::from("/tmp"),
            git_root_directory: None,
            operating_system: "linux".to_string(),
            today_date: "2025-01-01".to_string(),
            system_prompt: SystemPrompt {
                content: "builtin prompt".to_string(),
                source: SystemPromptSource::Builtin,
            },
            context_files,
            skills: Vec::new(),
            skill_diagnostics: Vec::new(),
        }
    }

    fn plain(path: &std::path::Path) -> String {
        path.display().to_string()
    }

    /// The record carries what the notice shows and nothing the prompt text
    /// already holds: files by path and kind, skills by path, name and listing
    /// status, the prompt by its override path or not at all.
    #[test]
    fn the_record_names_every_source_of_the_prompt() {
        let skill = |name: &str, enabled: bool, dmi: bool| Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            path: PathBuf::from(format!("/var/skills/{name}/SKILL.md")),
            enabled,
            disable_model_invocation: dmi,
        };
        let mut env = env_with(vec![
            ContextFile {
                path: PathBuf::from("/var/user/.agents/AGENTS.md"),
                kind: ContextFileKind::UserInstructions,
                content: String::new(),
            },
            ContextFile {
                path: PathBuf::from("/var/project/AGENTS.md"),
                kind: ContextFileKind::ProjectInstructions,
                content: String::new(),
            },
        ]);
        env.skills = vec![skill("alpha", true, false), skill("beta", false, true)];

        let record = session_context_with_display(&env, plain);
        assert_eq!(record.system_prompt, None, "the builtin prompt has no path");
        assert_eq!(
            record
                .files
                .iter()
                .map(|f| (f.path.as_str(), f.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("/var/user/.agents/AGENTS.md", "user instructions"),
                ("/var/project/AGENTS.md", "project instructions"),
            ],
        );
        assert_eq!(
            record
                .skills
                .iter()
                .map(|s| (s.name.as_str(), s.enabled, s.model_invocation))
                .collect::<Vec<_>>(),
            vec![("alpha", true, true), ("beta", false, false)],
        );
        assert_eq!(
            record.notice(),
            "Context:\n  \
             - builtin (system prompt; override with ~/.agents/SYSTEM_PROMPT.md)\n  \
             - /var/user/.agents/AGENTS.md (user instructions)\n  \
             - /var/project/AGENTS.md (project instructions)\n  \
             - /var/skills/alpha/SKILL.md (skill: alpha)\n  \
             - \x1b[9m/var/skills/beta/SKILL.md (skill: beta, disabled)\x1b[29m",
        );
    }

    #[test]
    fn an_override_prompt_is_recorded_by_its_path() {
        let mut env = env_with(Vec::new());
        env.system_prompt = SystemPrompt {
            content: "override prompt".to_string(),
            source: SystemPromptSource::Override(PathBuf::from(
                "/var/user/.agents/SYSTEM_PROMPT.md",
            )),
        };
        let record = session_context_with_display(&env, plain);
        assert_eq!(
            record.system_prompt.as_deref(),
            Some("/var/user/.agents/SYSTEM_PROMPT.md")
        );
        assert_eq!(
            record.notice(),
            "Context:\n  - /var/user/.agents/SYSTEM_PROMPT.md (system prompt)"
        );
    }

    #[test]
    fn sandbox_warning_enabled_tracks_env_var_presence() {
        assert!(sandbox_warning_enabled_for(false));
        assert!(!sandbox_warning_enabled_for(true));
    }

    #[test]
    fn sandbox_warning_string_is_stable() {
        assert!(SANDBOX_WARNING.starts_with("WARNING: AJ has no sandboxing"));
        assert!(SANDBOX_WARNING.contains("AJ_DISABLE_SANDBOX_WARNING=1"));
    }
}
