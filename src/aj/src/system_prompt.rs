//! Host-side assembly of the agent's system prompt.
//!
//! The `aj-agent` runtime takes a finished system-prompt string and
//! never reaches for the host's configuration or filesystem. The
//! binary owns the [`AgentEnv`] (base prompt, AGENTS.md/CLAUDE.md
//! context files, discovered skills, environment summary) and turns
//! it into that string here, once, before seeding the agent.

use aj_conf::AgentEnv;

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

    // Each context file is wrapped in an `<agents-md>` block so the
    // model can tell where instructions start and end, with the
    // kind-specific prefix text introducing it.
    for file in &env.context_files {
        text.push_str(&format!(
            "\n\n{}\n<agents-md>\n{}\n</agents-md>",
            file.kind.prompt_prefix(),
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

    use aj_conf::{AgentEnv, SystemPrompt, SystemPromptSource};

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
