//! The agent's working environment: working directory, git root, OS, date,
//! the base system prompt, and the user/project `AGENTS.md`/`CLAUDE.md`
//! context files stitched into the prompt.

use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths::{find_git_root, home_dir, project_dirs_upward};
use crate::skills::{self, Skill, SkillDiagnostic};

/// Prefix for project-level AGENTS.md instructions injected into the system
/// prompt.
pub const AGENTS_MD_PREFIX: &str = r#"
Here are instructions about the code base from the user. It's the contents
of an AGENTS.md file. These instructions override default behavior and you
must follow them exactly as written:
"#;

/// Prefix for user-level (global) instructions injected into the system
/// prompt.
pub const USER_AGENTS_MD_PREFIX: &str = r#"
Here are global instructions from the user that apply across all projects.
They are loaded from the user's home directory (e.g. ~/.agents/AGENTS.md
or ~/.claude/CLAUDE.md). These instructions override default behavior and
you must follow them exactly as written:
"#;

/// A file that contributes to the agent's context (system prompt). Covers
/// user-level and project-level `AGENTS.md` / `CLAUDE.md`; the whole file
/// content is stitched into the prompt (unlike skills, which are listed by
/// name and read on demand — see [`skills`]).
#[derive(Debug, Clone)]
pub struct ContextFile {
    /// Path to the file on disk.
    pub path: PathBuf,
    /// What kind of context file this is. Used to pick the right framing when
    /// stitching the file into the system prompt and to label it in the UI.
    pub kind: ContextFileKind,
    /// Contents of the file.
    pub content: String,
}

/// Kind of a [ContextFile]. Determines the prefix text used when injecting the
/// content into the system prompt and the human-readable label shown in the
/// UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextFileKind {
    /// Global, user-level instructions from `~/.agents/AGENTS.md` or
    /// `~/.claude/CLAUDE.md`.
    UserInstructions,
    /// Project-level instructions from `AGENTS.md` / `agents.md` in the
    /// working directory.
    ProjectInstructions,
}

impl ContextFileKind {
    /// Returns the prefix text injected into the system prompt before the
    /// file's content.
    pub fn prompt_prefix(&self) -> &'static str {
        match self {
            ContextFileKind::UserInstructions => USER_AGENTS_MD_PREFIX,
            ContextFileKind::ProjectInstructions => AGENTS_MD_PREFIX,
        }
    }

    /// Short human-readable label, used when displaying the context to the
    /// user.
    pub fn label(&self) -> &'static str {
        match self {
            ContextFileKind::UserInstructions => "user instructions",
            ContextFileKind::ProjectInstructions => "project instructions",
        }
    }
}

/// The agent's base system prompt and where it came from.
#[derive(Debug, Clone)]
pub struct SystemPrompt {
    pub content: String,
    pub source: SystemPromptSource,
}

/// Where the base system prompt was loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPromptSource {
    /// The prompt compiled into the binary.
    Builtin,
    /// An override file from the user's home directory. Its content fully
    /// replaces the builtin prompt.
    Override(PathBuf),
}

impl SystemPromptSource {
    /// Short human-readable label, used when displaying the context to the
    /// user. Parallel to [`ContextFileKind::label`].
    pub fn label(&self) -> &'static str {
        "system prompt"
    }
}

/// The working environment of the agent, includes configuration, the system
/// prompt, working directories, etc.
#[derive(Debug, Clone)]
pub struct AgentEnv {
    pub working_directory: PathBuf,
    pub git_root_directory: Option<PathBuf>,
    pub operating_system: String,
    pub today_date: String,
    /// The base system prompt: the builtin one or an override file from the
    /// user's home directory. Context files are appended to this when the
    /// full prompt is assembled.
    pub system_prompt: SystemPrompt,
    /// Files that get stitched into the agent's system prompt. Ordered from
    /// most general (user-level) to most specific (project-level).
    pub context_files: Vec<ContextFile>,
    /// Skills discovered at env load time, in precedence order (most
    /// specific first). Includes disabled and model-invocation-disabled
    /// skills so the UI can show them; only those with
    /// [`Skill::in_model_context`] reach the system prompt.
    pub skills: Vec<Skill>,
    /// Non-fatal problems hit while discovering skills, for the binary to
    /// surface alongside its other startup diagnostics.
    pub skill_diagnostics: Vec<SkillDiagnostic>,
}

impl AgentEnv {
    /// Read the environment from the real host: working directory, `$HOME`,
    /// and the current date. Delegates the discovery itself to
    /// `discover`. `disabled_skills` carries the
    /// `disabled_skills` config value. Matching skills are discovered but
    /// marked disabled.
    pub fn new(builtin_system_prompt: &str, disabled_skills: &[String]) -> Self {
        let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let home = home_dir();
        let today_date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        Self::discover(
            working_directory,
            home.as_deref(),
            today_date,
            builtin_system_prompt,
            disabled_skills,
        )
    }

    /// Discover the environment from explicit inputs: git root, instruction
    /// files, skills, and the base system prompt (`builtin_system_prompt`
    /// unless an override file exists, see `resolve_system_prompt`).
    ///
    /// Takes the working directory, `$HOME`, and date as parameters rather
    /// than reading them, so discovery can be driven hermetically in tests.
    /// [`AgentEnv::new`] is the real-host wrapper.
    fn discover(
        working_directory: PathBuf,
        home: Option<&Path>,
        today_date: String,
        builtin_system_prompt: &str,
        disabled_skills: &[String],
    ) -> Self {
        let git_root_directory = find_git_root(&working_directory);
        let operating_system = env::consts::OS.to_string();
        let system_prompt = Self::resolve_system_prompt(home, builtin_system_prompt);

        let mut context_files = Vec::new();
        if let Some(file) = Self::load_user_instructions_in(home) {
            context_files.push(file);
        }
        context_files.extend(Self::load_project_instructions(
            &working_directory,
            git_root_directory.as_deref(),
        ));

        let (skills, skill_diagnostics) = skills::discover_skills_at(
            home,
            &working_directory,
            git_root_directory.as_deref(),
            disabled_skills,
        );

        AgentEnv {
            working_directory,
            git_root_directory,
            operating_system,
            today_date,
            system_prompt,
            context_files,
            skills,
            skill_diagnostics,
        }
    }

    /// Resolve the base system prompt: an override file under `home` fully
    /// replaces the builtin prompt. Falls back to `builtin` when `home` is
    /// `None` or no override file exists.
    fn resolve_system_prompt(home: Option<&Path>, builtin: &str) -> SystemPrompt {
        if let Some(home) = home {
            // Prefer .agents over .claude, mirroring the precedence for
            // user-level instruction files.
            let candidates = [
                home.join(".agents").join("SYSTEM_PROMPT.md"),
                home.join(".claude").join("SYSTEM_PROMPT.md"),
            ];
            for path in candidates {
                if let Ok(content) = fs::read_to_string(&path) {
                    return SystemPrompt {
                        content,
                        source: SystemPromptSource::Override(path),
                    };
                }
            }
        }
        SystemPrompt {
            content: builtin.to_string(),
            source: SystemPromptSource::Builtin,
        }
    }

    /// Load global user-level instructions from `home`. Prefers
    /// `~/.agents/AGENTS.md` (open standard) over `~/.claude/CLAUDE.md`
    /// (Claude Code) when both exist. Returns `None` if `home` is `None`
    /// or neither file exists.
    fn load_user_instructions_in(home: Option<&Path>) -> Option<ContextFile> {
        let home = home?;

        // Prefer .agents over .claude.
        let candidates = [
            home.join(".agents").join("AGENTS.md"),
            home.join(".claude").join("CLAUDE.md"),
        ];

        for path in candidates {
            if let Ok(content) = fs::read_to_string(&path) {
                return Some(ContextFile {
                    path,
                    kind: ContextFileKind::UserInstructions,
                    content,
                });
            }
        }
        None
    }

    /// Load project-level instructions: one file per directory from the
    /// git root down to the working directory, so a repo-level AGENTS.md
    /// and a subproject-level one both apply (general first, specific
    /// last). Outside a git repository only the working directory is
    /// consulted.
    fn load_project_instructions(
        working_directory: &Path,
        git_root: Option<&Path>,
    ) -> Vec<ContextFile> {
        let mut dirs = project_dirs_upward(working_directory, git_root);
        // The walk is most-specific-first; context files are stitched
        // most-general-first so the specific file overrides.
        dirs.reverse();
        dirs.iter()
            .filter_map(|dir| Self::load_project_instructions_in(dir))
            .collect()
    }

    /// Load the instructions file of a single directory. Prefers
    /// `AGENTS.md` (open standard), falling back to `agents.md` and then
    /// to `CLAUDE.md` (Claude Code convention).
    fn load_project_instructions_in(dir: &Path) -> Option<ContextFile> {
        let candidates = [
            dir.join("AGENTS.md"),
            dir.join("agents.md"),
            dir.join("CLAUDE.md"),
        ];

        for path in candidates {
            if let Ok(content) = fs::read_to_string(&path) {
                return Some(ContextFile {
                    path,
                    kind: ContextFileKind::ProjectInstructions,
                    content,
                });
            }
        }
        None
    }
}

impl fmt::Display for AgentEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Working directory: {}", self.working_directory.display())?;
        match &self.git_root_directory {
            Some(git_root) => writeln!(f, "Git root directory: {}", git_root.display())?,
            None => writeln!(f, "Git root directory: None")?,
        }
        writeln!(f, "Operating system: {}", self.operating_system)?;
        write!(f, "Today's date: {}", self.today_date)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_is_hermetic() {
        // An empty home and an empty working directory: no override prompt,
        // no context files, the date and cwd are exactly what we passed.
        let home = crate::test_temp_dir("discover-home");
        let cwd = crate::test_temp_dir("discover-cwd");

        let env = AgentEnv::discover(
            cwd.clone(),
            Some(&home),
            "2026-01-02".to_string(),
            "builtin prompt",
            &[],
        );

        assert_eq!(env.working_directory, cwd);
        assert_eq!(env.today_date, "2026-01-02");
        assert!(!env.operating_system.is_empty());
        assert_eq!(env.system_prompt.content, "builtin prompt");
        assert_eq!(env.system_prompt.source, SystemPromptSource::Builtin);
        // The fresh temp dirs have no instruction files. This holds as long
        // as the temp cwd has no `.git` ancestor. The search would otherwise
        // walk into a real tree, so we assert that precondition here, making
        // a surprising ancestor repo fail loudly rather than as a confusing
        // context-files mismatch.
        assert!(
            env.git_root_directory.is_none(),
            "temp cwd unexpectedly under a git repo: {:?}",
            env.git_root_directory
        );
        assert!(
            env.context_files.is_empty(),
            "no AGENTS.md in the temp dirs: {:?}",
            env.context_files
        );

        fs::remove_dir_all(&home).ok();
        fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn new_reads_the_host_environment() {
        // Smoke test: the real-host wrapper populates the fields it reads
        // from the process (cwd, OS, date). It necessarily touches the host
        // environment, so the assertions stay weak.
        let env = AgentEnv::new("builtin prompt", &[]);
        assert!(!env.working_directory.as_os_str().is_empty());
        assert!(!env.operating_system.is_empty());
        assert!(!env.today_date.is_empty());
    }

    #[test]
    fn display_format_is_stable() {
        let env = AgentEnv::discover(
            PathBuf::from("/work"),
            None,
            "2026-01-02".to_string(),
            "builtin prompt",
            &[],
        );
        let display_output = format!("{env}");
        assert!(display_output.contains("Working directory: /work"));
        assert!(display_output.contains("Git root directory: None"));
        assert!(display_output.contains("Operating system:"));
        assert!(display_output.contains("Today's date: 2026-01-02"));
    }

    #[test]
    fn test_context_file_kind_prompt_prefix() {
        // Each kind has a non-empty prefix; smoke-test that the user-level
        // prefix is distinct from the project-level one so the model sees
        // them framed differently.
        assert!(!ContextFileKind::UserInstructions.prompt_prefix().is_empty());
        assert!(
            !ContextFileKind::ProjectInstructions
                .prompt_prefix()
                .is_empty()
        );
        assert_ne!(
            ContextFileKind::UserInstructions.prompt_prefix(),
            ContextFileKind::ProjectInstructions.prompt_prefix()
        );
    }

    #[test]
    fn test_context_file_kind_label() {
        assert_eq!(
            ContextFileKind::UserInstructions.label(),
            "user instructions"
        );
        assert_eq!(
            ContextFileKind::ProjectInstructions.label(),
            "project instructions"
        );
    }

    #[test]
    fn test_load_project_instructions_prefers_agents_md() {
        let dir = crate::test_temp_dir("prefers-agents");
        fs::write(dir.join("AGENTS.md"), "agents content").unwrap();
        fs::write(dir.join("agents.md"), "lowercase content").unwrap();
        fs::write(dir.join("CLAUDE.md"), "claude content").unwrap();

        let file = AgentEnv::load_project_instructions_in(&dir).expect("file should load");
        assert_eq!(file.kind, ContextFileKind::ProjectInstructions);
        assert_eq!(file.content, "agents content");
        assert_eq!(file.path, dir.join("AGENTS.md"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_project_instructions_falls_back_to_claude_md() {
        let dir = crate::test_temp_dir("falls-back-claude");
        fs::write(dir.join("CLAUDE.md"), "claude content").unwrap();

        let file = AgentEnv::load_project_instructions_in(&dir).expect("file should load");
        assert_eq!(file.content, "claude content");
        assert_eq!(file.path, dir.join("CLAUDE.md"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_project_instructions_none_when_missing() {
        let dir = crate::test_temp_dir("none-missing");
        assert!(AgentEnv::load_project_instructions_in(&dir).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_project_instructions_walks_up_to_git_root() {
        let root = crate::test_temp_dir("walk-instructions");
        let mid = root.join("mid");
        let cwd = mid.join("leaf");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(root.join("AGENTS.md"), "root content").unwrap();
        fs::write(cwd.join("CLAUDE.md"), "leaf content").unwrap();

        let files = AgentEnv::load_project_instructions(&cwd, Some(&root));
        // General (git root) first, specific (cwd) last; the
        // instruction-less middle directory contributes nothing.
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["root content", "leaf content"]);

        // Without a git root only the working directory is consulted.
        let files = AgentEnv::load_project_instructions(&cwd, None);
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["leaf content"]);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_resolve_system_prompt_prefers_agents_override() {
        let home = crate::test_temp_dir("sysprompt-prefers-agents");
        fs::create_dir_all(home.join(".agents")).unwrap();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".agents/SYSTEM_PROMPT.md"), "agents prompt").unwrap();
        fs::write(home.join(".claude/SYSTEM_PROMPT.md"), "claude prompt").unwrap();

        let prompt = AgentEnv::resolve_system_prompt(Some(&home), "builtin prompt");
        assert_eq!(prompt.content, "agents prompt");
        assert_eq!(
            prompt.source,
            SystemPromptSource::Override(home.join(".agents/SYSTEM_PROMPT.md"))
        );

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_resolve_system_prompt_falls_back_to_claude_override() {
        let home = crate::test_temp_dir("sysprompt-claude");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/SYSTEM_PROMPT.md"), "claude prompt").unwrap();

        let prompt = AgentEnv::resolve_system_prompt(Some(&home), "builtin prompt");
        assert_eq!(prompt.content, "claude prompt");
        assert_eq!(
            prompt.source,
            SystemPromptSource::Override(home.join(".claude/SYSTEM_PROMPT.md"))
        );

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_resolve_system_prompt_builtin_when_no_override() {
        let home = crate::test_temp_dir("sysprompt-builtin");
        let prompt = AgentEnv::resolve_system_prompt(Some(&home), "builtin prompt");
        assert_eq!(prompt.content, "builtin prompt");
        assert_eq!(prompt.source, SystemPromptSource::Builtin);
        fs::remove_dir_all(&home).ok();

        let prompt = AgentEnv::resolve_system_prompt(None, "builtin prompt");
        assert_eq!(prompt.content, "builtin prompt");
        assert_eq!(prompt.source, SystemPromptSource::Builtin);
    }
}
