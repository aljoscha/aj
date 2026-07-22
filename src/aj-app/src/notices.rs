//! Frontend-agnostic startup notices for the terminal frontend:
//! the `Context:` listing and the sandbox warning.
//!
//! Both are host-side text the binary surfaces above the editor at
//! startup. Keeping them in `aj-app` keeps the notice strings
//! independent of the frontend.

use aj_conf::{AgentEnv, SystemPromptSource, display_path};

/// One row of the `Context:` listing, split so the bullet and the row content
/// can be styled apart.
///
/// We keep the row structured rather than pre-formatted because
/// [`build_context_notice`] strikes a disabled skill's row content without
/// striking its `  - ` bullet, matching aj. A flat string could not express
/// that distinction, so the split lives here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextLine {
    /// Leading bullet or indent, never struck. `  - ` for a listed row, empty
    /// for the `Context:` header.
    pub bullet: String,
    /// The row content, rendered struck when `struck` is set.
    pub text: String,
    /// True only for a disabled skill row (`!skill.enabled`), the only rows aj
    /// renders struck.
    pub struck: bool,
}

/// The structured `Context:` listing: the header, the base system prompt
/// (builtin or override file), every agents.md-style instruction file, and
/// every discovered skill, one [`ContextLine`] each.
///
/// Rows carry a tildified path and a label; skill rows also carry a marker
/// when the skill is excluded from the model's listing, either `disabled`
/// (the user's `disabled_skills` config) or `model-invocation disabled` (the
/// skill's own frontmatter). A disabled skill row is marked `struck`, the one
/// visual distinction. Same content and order as [`build_context_notice`],
/// which joins this.
pub(crate) fn context_lines(env: &AgentEnv) -> Vec<ContextLine> {
    let bullet = "  - ".to_string();
    let mut lines = vec![ContextLine {
        bullet: String::new(),
        text: "Context:".to_string(),
        struck: false,
    }];
    let source = &env.system_prompt.source;
    let prompt = match source {
        SystemPromptSource::Builtin => format!(
            "builtin ({}; override with ~/.agents/SYSTEM_PROMPT.md)",
            source.label()
        ),
        SystemPromptSource::Override(path) => {
            format!("{} ({})", display_path(path), source.label())
        }
    };
    lines.push(ContextLine {
        bullet: bullet.clone(),
        text: prompt,
        struck: false,
    });
    for file in &env.context_files {
        lines.push(ContextLine {
            bullet: bullet.clone(),
            text: format!("{} ({})", display_path(&file.path), file.kind.label()),
            struck: false,
        });
    }
    for skill in &env.skills {
        let marker = if !skill.enabled {
            ", disabled"
        } else if skill.disable_model_invocation {
            ", model-invocation disabled"
        } else {
            ""
        };
        lines.push(ContextLine {
            bullet: bullet.clone(),
            text: format!(
                "{} (skill: {}{marker})",
                display_path(&skill.path),
                skill.name
            ),
            struck: !skill.enabled,
        });
    }
    lines
}

/// Build the chat-scrollback "Context:" notice by joining [`context_lines`]
/// into one flat string, one row per line as `  - <tildified path> (<label>)`.
///
/// The assembly is frontend-agnostic. The one visual choice, setting a
/// disabled skill's row apart, is injected as `strike`: each frontend supplies
/// its own strike rendering (an ANSI `\x1b[9m..\x1b[29m` strikethrough today).
/// `strike` applies to the row content only, never the bullet, matching the
/// structured split.
pub fn build_context_notice(env: &AgentEnv, strike: fn(&str) -> String) -> String {
    context_lines(env)
        .into_iter()
        .map(|line| {
            let content = if line.struck {
                strike(&line.text)
            } else {
                line.text
            };
            format!("{}{content}", line.bullet)
        })
        .collect::<Vec<_>>()
        .join("\n")
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
/// Uses `std::env::var("AJ_DISABLE_SANDBOX_WARNING").is_err()`, so
/// setting the var to any value (including the empty string) suppresses
/// the warning.
pub fn sandbox_warning_enabled() -> bool {
    std::env::var("AJ_DISABLE_SANDBOX_WARNING").is_err()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use aj_conf::{
        AgentEnv, ContextFile, ContextFileKind, SystemPrompt, SystemPromptSource, skills::Skill,
    };

    use super::{SANDBOX_WARNING, build_context_notice, context_lines, sandbox_warning_enabled};

    /// Sentinel `strike` hook: wraps the row in a visible marker so a
    /// test can assert exactly which rows the hook fires on.
    fn strike(s: &str) -> String {
        format!("<s>{s}</s>")
    }

    /// Build an [`AgentEnv`] for the notice-builder tests. Working
    /// directory / OS / date / git root are stubbed: only
    /// `system_prompt`, `context_files`, and `skills` matter here.
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

    #[test]
    fn build_context_notice_without_files_lists_only_the_system_prompt() {
        let env = env_with(Vec::new());
        assert_eq!(
            build_context_notice(&env, strike),
            "Context:\n  - builtin (system prompt; override with ~/.agents/SYSTEM_PROMPT.md)"
        );
    }

    #[test]
    fn build_context_notice_lists_files_with_label_and_tildified_path() {
        // `display_path` tildifies under `$HOME`, so build the path
        // off the live `HOME` env var to keep the assertion stable
        // across machines.
        let home = std::env::var("HOME").expect("HOME set in test env");
        let user_path = PathBuf::from(&home).join(".agents/AGENTS.md");
        let project_path = PathBuf::from("/var/project/AGENTS.md");
        let env = env_with(vec![
            ContextFile {
                path: user_path,
                kind: ContextFileKind::UserInstructions,
                content: String::new(),
            },
            ContextFile {
                path: project_path,
                kind: ContextFileKind::ProjectInstructions,
                content: String::new(),
            },
        ]);

        let notice = build_context_notice(&env, strike);
        let expected = "Context:\n  \
             - builtin (system prompt; override with ~/.agents/SYSTEM_PROMPT.md)\n  \
             - ~/.agents/AGENTS.md (user instructions)\n  \
             - /var/project/AGENTS.md (project instructions)";
        assert_eq!(notice, expected);
    }

    #[test]
    fn build_context_notice_override_shows_tildified_prompt_path() {
        // `display_path` tildifies under `$HOME`, so build the path
        // off the live `HOME` env var to keep the assertion stable
        // across machines.
        let home = std::env::var("HOME").expect("HOME set in test env");
        let path = PathBuf::from(&home).join(".agents/SYSTEM_PROMPT.md");
        let mut env = env_with(Vec::new());
        env.system_prompt = SystemPrompt {
            content: "override prompt".to_string(),
            source: SystemPromptSource::Override(path),
        };
        assert_eq!(
            build_context_notice(&env, strike),
            "Context:\n  - ~/.agents/SYSTEM_PROMPT.md (system prompt)"
        );
    }

    #[test]
    fn build_context_notice_strikes_only_disabled_skill_rows() {
        let skill = |name: &str, enabled: bool, dmi: bool| Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            path: PathBuf::from(format!("/var/skills/{name}/SKILL.md")),
            enabled,
            disable_model_invocation: dmi,
        };
        let mut env = env_with(Vec::new());
        env.skills = vec![
            skill("alpha", true, false),
            skill("beta", false, false),
            skill("gamma", true, true),
        ];

        let notice = build_context_notice(&env, strike);
        // The hook fires on the disabled row (and only that one): the
        // enabled and model-invocation-disabled rows stay unwrapped.
        let expected = format!(
            "Context:\n  \
             - builtin (system prompt; override with ~/.agents/SYSTEM_PROMPT.md)\n  \
             - /var/skills/alpha/SKILL.md (skill: alpha)\n  \
             - {}\n  \
             - /var/skills/gamma/SKILL.md (skill: gamma, model-invocation disabled)",
            strike("/var/skills/beta/SKILL.md (skill: beta, disabled)")
        );
        assert_eq!(notice, expected);
        assert_eq!(notice.matches("<s>").count(), 1);
    }

    #[test]
    fn context_lines_mark_only_disabled_skill_rows_struck() {
        let skill = |name: &str, enabled: bool, dmi: bool| Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            path: PathBuf::from(format!("/var/skills/{name}/SKILL.md")),
            enabled,
            disable_model_invocation: dmi,
        };
        let mut env = env_with(Vec::new());
        env.skills = vec![
            skill("alpha", true, false),
            skill("beta", false, false),
            skill("gamma", true, true),
        ];

        let lines = context_lines(&env);
        // Only the disabled skill row is struck; the enabled and the
        // model-invocation-disabled rows are not.
        let struck: Vec<&str> = lines
            .iter()
            .filter(|l| l.struck)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(
            struck,
            vec!["/var/skills/beta/SKILL.md (skill: beta, disabled)"]
        );
        // The header carries no bullet; every listed row carries `  - `.
        assert_eq!(lines[0].bullet, "");
        assert_eq!(lines[0].text, "Context:");
        assert!(lines[1..].iter().all(|l| l.bullet == "  - "));
    }

    #[test]
    fn sandbox_warning_enabled_tracks_env_var_presence() {
        // SAFETY: tests in this module run on a single thread so
        // mutating the process env is fine. We save/restore the
        // pre-existing value so other tests aren't disturbed.
        let prev = std::env::var("AJ_DISABLE_SANDBOX_WARNING").ok();

        // SAFETY: single-threaded test runner per `cargo test`'s
        // default. No other test reads this var concurrently.
        unsafe {
            std::env::remove_var("AJ_DISABLE_SANDBOX_WARNING");
        }
        assert!(
            sandbox_warning_enabled(),
            "warning should show when the var is absent"
        );

        // SAFETY: same scope as above.
        unsafe {
            std::env::set_var("AJ_DISABLE_SANDBOX_WARNING", "1");
        }
        assert!(
            !sandbox_warning_enabled(),
            "warning should be suppressed when the var is set"
        );

        // `is_err()` semantics: even an empty value counts as "set"
        // and suppresses the warning.
        // SAFETY: same scope as above.
        unsafe {
            std::env::set_var("AJ_DISABLE_SANDBOX_WARNING", "");
        }
        assert!(
            !sandbox_warning_enabled(),
            "warning should stay suppressed when the var is set to the empty string"
        );

        // Restore.
        // SAFETY: same scope as above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AJ_DISABLE_SANDBOX_WARNING", v),
                None => std::env::remove_var("AJ_DISABLE_SANDBOX_WARNING"),
            }
        }
    }

    #[test]
    fn sandbox_warning_string_is_stable() {
        assert!(SANDBOX_WARNING.starts_with("WARNING: AJ has no sandboxing"));
        assert!(SANDBOX_WARNING.contains("AJ_DISABLE_SANDBOX_WARNING=1"));
    }
}
