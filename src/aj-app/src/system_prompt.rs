//! Host-side assembly of the agent's system prompt.
//!
//! The `aj-agent` runtime takes a finished system-prompt string and
//! never reaches for the host's configuration or filesystem. The
//! binary owns the [`AgentEnv`] (base prompt, AGENTS.md/CLAUDE.md
//! context files, discovered skills, environment summary) and turns
//! it into that string here, once, before seeding the agent.

use aj_conf::{AgentEnv, ContextFileKind};

// The default system prompt is embedded at compile time so the binaries
// ship as self-contained executables. The file lives next to this
// crate's `Cargo.toml`. At runtime, a `~/.agents/SYSTEM_PROMPT.md` (or
// `~/.claude/SYSTEM_PROMPT.md`) override file replaces it; see `AgentEnv`
// in `aj-conf`.
pub const SYSTEM_PROMPT: &str = include_str!("../SYSTEM_PROMPT.md");

/// Assemble the full system prompt: the base prompt, the stitched
/// context files, the optional skills listing, and the trailing
/// environment block.
///
/// `include_skills` gates the skills listing. Skills are progressive
/// disclosure reachable only with a `read_file` tool, so the caller
/// passes whether that tool is in the active set. Without it the
/// listing would be unreachable and is omitted entirely.
pub fn assemble_system_prompt(env: &AgentEnv, include_skills: bool) -> String {
    let mut text = env.system_prompt.content.clone();

    if !env.context_files.is_empty() {
        text.push_str(
            "\n\n## User and project guidance\n\n\
             Follow the applicable guidance below. Global guidance applies across projects. \
             Project guidance applies to its directory and descendants. \
             For conflicts, explicit user requests take precedence over this guidance, \
             project guidance over global guidance, and more specific directory guidance \
             over parent-directory guidance. This guidance overrides default behavior \
             within its scope.",
        );
    }

    for file in &env.context_files {
        let scope = match file.kind {
            ContextFileKind::UserInstructions => "all projects".to_string(),
            ContextFileKind::ProjectInstructions => format!(
                "{} and descendants",
                file.path
                    .parent()
                    .expect("context file has a directory")
                    .display()
            ),
        };
        text.push_str(&format!(
            "\n\nContents of {} (scope: {scope}):\n<agents-md>\n{}\n</agents-md>",
            file.path.display(),
            file.content
        ));
    }

    if include_skills {
        if let Some(block) = aj_conf::skills::format_skills_for_prompt(&env.skills) {
            text.push_str("\n\n");
            text.push_str(&block);
        }
    }

    text.push_str(&format!(
        "\n\nHere's useful information about your environment:\n<env>\n{env}\n</env>"
    ));

    text
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use aj_conf::{AgentEnv, ContextFile, ContextFileKind, SystemPrompt, SystemPromptSource};

    use super::assemble_system_prompt;

    fn env_with_skills(skills: Vec<aj_conf::skills::Skill>) -> AgentEnv {
        AgentEnv {
            working_directory: PathBuf::from("/tmp"),
            git_root_directory: None,
            operating_system: "test".to_string(),
            today_date: "2024-01-01".to_string(),
            system_prompt: SystemPrompt {
                content: "base prompt".to_string(),
                source: SystemPromptSource::Builtin,
            },
            context_files: Vec::new(),
            skills,
            skill_diagnostics: Vec::new(),
        }
    }

    #[test]
    fn guidance_includes_sources_scopes_and_precedence() {
        let mut env = env_with_skills(Vec::new());
        env.context_files = vec![
            ContextFile {
                path: "/home/user/.agents/AGENTS.md".into(),
                kind: ContextFileKind::UserInstructions,
                content: "Global rule".into(),
            },
            ContextFile {
                path: "/repo/AGENTS.md".into(),
                kind: ContextFileKind::ProjectInstructions,
                content: "Project rule".into(),
            },
            ContextFile {
                path: "/repo/src/CLAUDE.md".into(),
                kind: ContextFileKind::ProjectInstructions,
                content: "Nested rule".into(),
            },
        ];

        let prompt = assemble_system_prompt(&env, false);
        assert!(prompt.starts_with("base prompt"));
        assert!(prompt.contains("explicit user requests take precedence"));
        assert!(prompt.contains("project guidance over global guidance"));
        assert!(prompt.contains("more specific directory guidance over parent-directory guidance"));
        for (path, scope, rule) in [
            (
                "/home/user/.agents/AGENTS.md",
                "all projects",
                "Global rule",
            ),
            ("/repo/AGENTS.md", "/repo and descendants", "Project rule"),
            (
                "/repo/src/CLAUDE.md",
                "/repo/src and descendants",
                "Nested rule",
            ),
        ] {
            assert!(prompt.contains(&format!(
                "Contents of {path} (scope: {scope}):\n<agents-md>\n{rule}\n</agents-md>"
            )));
        }
        assert!(prompt.find("Global rule") < prompt.find("Project rule"));
        assert!(prompt.find("Project rule") < prompt.find("Nested rule"));

        env.context_files.clear();
        assert!(!assemble_system_prompt(&env, false).contains("## User and project guidance"));
    }

    #[test]
    fn lists_skills_behind_read_file_gate() {
        let skill = |name: &str, enabled: bool, dmi: bool| aj_conf::skills::Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
            enabled,
            disable_model_invocation: dmi,
        };
        let env = env_with_skills(vec![
            skill("alpha", true, false),
            skill("beta", false, false),
            skill("gamma", true, true),
        ]);

        // With the read_file tool: only the enabled, model-visible
        // skill is listed, and the listing precedes the env block.
        let prompt = assemble_system_prompt(&env, true);
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>alpha</name>"));
        assert!(!prompt.contains("beta"));
        assert!(!prompt.contains("gamma"));
        assert!(
            prompt.find("</available_skills>").unwrap() < prompt.find("<env>").unwrap(),
            "skills listing must come before the env block"
        );

        // Without it the listing is omitted entirely.
        let prompt = assemble_system_prompt(&env, false);
        assert!(!prompt.contains("<available_skills>"));
    }
}
