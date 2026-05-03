use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use std::str::FromStr;
use thiserror::Error;

/// Thinking level that can be set in `config.toml` as a default baseline.
///
/// When set, this is used for every request unless a trigger word in the user
/// message overrides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigThinkingLevel {
    Off,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl fmt::Display for ConfigThinkingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigThinkingLevel::Off => write!(f, "off"),
            ConfigThinkingLevel::Low => write!(f, "low"),
            ConfigThinkingLevel::Medium => write!(f, "medium"),
            ConfigThinkingLevel::High => write!(f, "high"),
            ConfigThinkingLevel::XHigh => write!(f, "xhigh"),
            ConfigThinkingLevel::Max => write!(f, "max"),
        }
    }
}

impl FromStr for ConfigThinkingLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "off" => Ok(ConfigThinkingLevel::Off),
            "low" => Ok(ConfigThinkingLevel::Low),
            "medium" => Ok(ConfigThinkingLevel::Medium),
            "high" => Ok(ConfigThinkingLevel::High),
            "xhigh" => Ok(ConfigThinkingLevel::XHigh),
            "max" => Ok(ConfigThinkingLevel::Max),
            _ => Err(format!(
                "invalid thinking level '{s}': expected off, low, medium, high, xhigh, or max"
            )),
        }
    }
}

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

/// A file that contributes to the agent's context (system prompt). Today this
/// covers user-level and project-level `AGENTS.md` / `CLAUDE.md`. In the
/// future this is the place to plug in additional context (e.g. skills).
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

/// The working environment of the agent, includes configuration, the system
/// prompt, working directories, etc.
#[derive(Debug, Clone)]
pub struct AgentEnv {
    pub working_directory: PathBuf,
    pub git_root_directory: Option<PathBuf>,
    pub operating_system: String,
    pub today_date: String,
    /// Files that get stitched into the agent's system prompt. Ordered from
    /// most general (user-level) to most specific (project-level).
    pub context_files: Vec<ContextFile>,
}

impl AgentEnv {
    pub fn new() -> Self {
        let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let git_root_directory = find_git_root(&working_directory);
        let operating_system = env::consts::OS.to_string();
        let today_date = chrono::Utc::now().format("%Y-%m-%d").to_string();

        let mut context_files = Vec::new();
        if let Some(file) = Self::load_user_instructions() {
            context_files.push(file);
        }
        if let Some(file) = Self::load_project_instructions(&working_directory) {
            context_files.push(file);
        }

        AgentEnv {
            working_directory,
            git_root_directory,
            operating_system,
            today_date,
            context_files,
        }
    }

    /// Load global user-level instructions. Prefers `~/.agents/AGENTS.md`
    /// (open standard) over `~/.claude/CLAUDE.md` (Claude Code) when both
    /// exist. Returns `None` if `HOME` isn't set or neither file exists.
    fn load_user_instructions() -> Option<ContextFile> {
        let home = env::var("HOME").ok()?;
        let home = PathBuf::from(home);

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

    /// Load project-level instructions from the working directory. Prefers
    /// `AGENTS.md` (open standard), falling back to `agents.md` and then to
    /// `CLAUDE.md` (Claude Code convention).
    fn load_project_instructions(working_directory: &Path) -> Option<ContextFile> {
        let candidates = [
            working_directory.join("AGENTS.md"),
            working_directory.join("agents.md"),
            working_directory.join("CLAUDE.md"),
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

/// Render `path` for display. If it lives under `$HOME`, abbreviate the home
/// prefix to `~`.
pub fn display_path(path: &Path) -> String {
    if let Ok(home) = env::var("HOME") {
        let home = PathBuf::from(home);
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

impl Default for AgentEnv {
    fn default() -> Self {
        Self::new()
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

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML parsing error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("home directory not found")]
    HomeNotFound,
}

/// Application configuration loaded from `~/.aj/config.toml`.
///
/// All fields are optional. Missing fields use application defaults. The
/// precedence order (highest to lowest) is: CLI flags > env vars > config file.
///
/// Example `config.toml`:
///
/// ```toml
/// model_api = "anthropic"
/// model_name = "claude-sonnet-4-20250514"
/// model_url = "https://api.anthropic.com"
/// thinking = "low"
/// disabled_tools = ["todo_read", "todo_write"]
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    /// Model API backend (e.g., "anthropic", "openai").
    pub model_api: Option<String>,
    /// Custom model endpoint URL.
    pub model_url: Option<String>,
    /// Model name override.
    pub model_name: Option<String>,
    /// Default thinking level used when no trigger word is present.
    pub thinking: Option<ConfigThinkingLevel>,
    /// Inference speed mode (Anthropic only). `fast` enables higher
    /// output-tokens-per-second at some quality cost.
    pub speed: Option<ConfigSpeed>,
    /// List of builtin tool names to disable. Tools in this list will not be
    /// available to the agent.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

/// Inference speed mode set in `config.toml` (Anthropic only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigSpeed {
    Standard,
    Fast,
}

impl fmt::Display for ConfigSpeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigSpeed::Standard => write!(f, "standard"),
            ConfigSpeed::Fast => write!(f, "fast"),
        }
    }
}

impl FromStr for ConfigSpeed {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "standard" => Ok(ConfigSpeed::Standard),
            "fast" => Ok(ConfigSpeed::Fast),
            _ => Err(format!("invalid speed '{s}': expected standard or fast")),
        }
    }
}

impl Config {
    /// Load configuration from `~/.aj/config.toml`.
    ///
    /// Returns a default (all-empty) config if the file does not exist.
    pub fn load() -> Result<Self, ConfigError> {
        let config_dir = Config::get_config_dir()?;
        let config_path = config_dir.join("config.toml");

        if !config_path.exists() {
            tracing::debug!(config_path = %config_path.display(), "no config file found, using defaults");
            return Ok(Config::default());
        }

        let content = fs::read_to_string(&config_path)?;
        let config: Config = toml::from_str(&content)?;
        tracing::debug!(config_path = %config_path.display(), "loaded config");
        Ok(config)
    }

    /// Returns the path to the `~/.aj/` config directory, creating it if
    /// necessary.
    pub fn get_config_dir() -> Result<PathBuf, ConfigError> {
        let home_dir = env::var("HOME").map_err(|_| ConfigError::HomeNotFound)?;
        let aj_dir = Path::new(&home_dir).join(".aj");

        if !aj_dir.exists() {
            fs::create_dir_all(&aj_dir)?;
        }

        Ok(aj_dir)
    }

    pub fn get_history_file_path() -> Result<PathBuf, ConfigError> {
        let aj_dir = Self::get_config_dir()?;
        Ok(aj_dir.join("history.txt"))
    }

    pub fn get_dotenv_file_path() -> Result<PathBuf, ConfigError> {
        let aj_dir = Self::get_config_dir()?;
        Ok(aj_dir.join(".env"))
    }

    /// Get the threads directory path for the current project. The threads are
    /// stored in subdirectories based on the git root directory. For example,
    /// if the git root is /Users/user/Dev/project, the subdirectory name will
    /// be "Dev-project".
    pub fn get_threads_dir_path() -> Result<PathBuf, ConfigError> {
        let aj_dir = Self::get_config_dir()?;
        let threads_base_dir = aj_dir.join("threads");

        // Find the git root directory.
        let working_directory = env::current_dir().map_err(ConfigError::Io)?;
        if let Some(git_root) = find_git_root(&working_directory) {
            // Convert the git root path to a directory name.
            let project_dir_name = path_to_dir_name(&git_root);
            let project_threads_dir = threads_base_dir.join(project_dir_name);

            // Create the directory if it doesn't exist.
            if !project_threads_dir.exists() {
                fs::create_dir_all(&project_threads_dir)?;
            }

            Ok(project_threads_dir)
        } else {
            // Fallback to a default directory if no git root is found.
            let default_threads_dir = threads_base_dir.join("default");
            if !default_threads_dir.exists() {
                fs::create_dir_all(&default_threads_dir)?;
            }
            Ok(default_threads_dir)
        }
    }
}

fn find_git_root(start_path: &Path) -> Option<PathBuf> {
    let mut current = start_path;

    loop {
        let git_dir = current.join(".git");
        if git_dir.exists() {
            return Some(current.to_path_buf());
        }

        match current.parent() {
            Some(parent) => current = parent,
            None => return None,
        }
    }
}

/// Convert a path to a directory name by taking components after the home
/// directory and joining them with dashes. For example, /Users/user/Dev/project
/// becomes "Dev-project".
fn path_to_dir_name(path: &Path) -> String {
    // Get the home directory.
    if let Ok(home_dir) = env::var("HOME") {
        let home_path = Path::new(&home_dir);

        // Try to get the relative path from home.
        if let Ok(relative_path) = path.strip_prefix(home_path) {
            return relative_path
                .components()
                .filter_map(|comp| {
                    if let std::path::Component::Normal(os_str) = comp {
                        os_str.to_str()
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("-");
        }
    }

    // Fallback: use the last component of the path.
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_env_creation() {
        let env = AgentEnv::new();
        assert!(!env.working_directory.as_os_str().is_empty());
        assert!(!env.operating_system.is_empty());
        assert!(!env.today_date.is_empty());
    }

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert!(config.model_api.is_none());
        assert!(config.model_url.is_none());
        assert!(config.model_name.is_none());
        assert!(config.thinking.is_none());
    }

    #[test]
    fn test_config_deserialize() {
        let toml_str = r#"
model_api = "anthropic"
model_name = "claude-sonnet-4-20250514"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.model_api.as_deref(), Some("anthropic"));
        assert_eq!(
            config.model_name.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert!(config.model_url.is_none());
        assert!(config.thinking.is_none());
    }

    #[test]
    fn test_config_deserialize_with_thinking() {
        let toml_str = r#"
model_api = "anthropic"
thinking = "medium"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.thinking, Some(ConfigThinkingLevel::Medium));
    }

    #[test]
    fn test_config_deserialize_thinking_levels() {
        for (input, expected) in [
            ("off", ConfigThinkingLevel::Off),
            ("low", ConfigThinkingLevel::Low),
            ("medium", ConfigThinkingLevel::Medium),
            ("high", ConfigThinkingLevel::High),
            ("xhigh", ConfigThinkingLevel::XHigh),
            ("max", ConfigThinkingLevel::Max),
        ] {
            let toml_str = format!("thinking = \"{input}\"");
            let config: Config = toml::from_str(&toml_str).unwrap();
            assert_eq!(config.thinking, Some(expected), "failed for input: {input}");
        }
    }

    #[test]
    fn test_config_thinking_level_from_str() {
        assert_eq!(
            "off".parse::<ConfigThinkingLevel>().unwrap(),
            ConfigThinkingLevel::Off
        );
        assert_eq!(
            "LOW".parse::<ConfigThinkingLevel>().unwrap(),
            ConfigThinkingLevel::Low
        );
        assert_eq!(
            "XHigh".parse::<ConfigThinkingLevel>().unwrap(),
            ConfigThinkingLevel::XHigh
        );
        assert!("invalid".parse::<ConfigThinkingLevel>().is_err());
    }

    #[test]
    fn test_config_thinking_level_display() {
        assert_eq!(ConfigThinkingLevel::Off.to_string(), "off");
        assert_eq!(ConfigThinkingLevel::Low.to_string(), "low");
        assert_eq!(ConfigThinkingLevel::Medium.to_string(), "medium");
        assert_eq!(ConfigThinkingLevel::High.to_string(), "high");
        assert_eq!(ConfigThinkingLevel::XHigh.to_string(), "xhigh");
        assert_eq!(ConfigThinkingLevel::Max.to_string(), "max");
    }

    #[test]
    fn test_config_deserialize_empty() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.model_api.is_none());
        assert!(config.disabled_tools.is_empty());
    }

    #[test]
    fn test_config_deserialize_disabled_tools() {
        let toml_str = r#"
disabled_tools = ["todo_read", "todo_write"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.disabled_tools, vec!["todo_read", "todo_write"]);
    }

    #[test]
    fn test_config_load_missing_file() {
        // Config::load() should succeed even when no file exists.
        let config = Config::load();
        assert!(config.is_ok());
    }

    #[test]
    fn test_agent_env_display() {
        let env = AgentEnv::new();
        let display_output = format!("{}", env);
        assert!(display_output.contains("Working directory:"));
        assert!(display_output.contains("Git root directory:"));
        assert!(display_output.contains("Operating system:"));
        assert!(display_output.contains("Today's date:"));
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
    fn test_display_path_tildifies_home() {
        // Pin HOME to a known value so the test is deterministic regardless
        // of the user running it.
        // SAFETY: tests are single-threaded per-binary by default, but env
        // mutation is still process-wide. We restore the prior value below.
        let prior_home = env::var("HOME").ok();
        unsafe {
            env::set_var("HOME", "/home/test-user");
        }

        let inside = PathBuf::from("/home/test-user/.agents/AGENTS.md");
        assert_eq!(display_path(&inside), "~/.agents/AGENTS.md");

        let outside = PathBuf::from("/etc/hosts");
        assert_eq!(display_path(&outside), "/etc/hosts");

        // Restore.
        unsafe {
            match prior_home {
                Some(value) => env::set_var("HOME", value),
                None => env::remove_var("HOME"),
            }
        }
    }

    /// Build a unique temp directory for tests that need a real filesystem
    /// scratch space without pulling in `tempfile`.
    fn make_temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("aj-conf-test-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_load_project_instructions_prefers_agents_md() {
        let dir = make_temp_dir("prefers-agents");
        fs::write(dir.join("AGENTS.md"), "agents content").unwrap();
        fs::write(dir.join("agents.md"), "lowercase content").unwrap();
        fs::write(dir.join("CLAUDE.md"), "claude content").unwrap();

        let file = AgentEnv::load_project_instructions(&dir).expect("file should load");
        assert_eq!(file.kind, ContextFileKind::ProjectInstructions);
        assert_eq!(file.content, "agents content");
        assert_eq!(file.path, dir.join("AGENTS.md"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_project_instructions_falls_back_to_claude_md() {
        let dir = make_temp_dir("falls-back-claude");
        fs::write(dir.join("CLAUDE.md"), "claude content").unwrap();

        let file = AgentEnv::load_project_instructions(&dir).expect("file should load");
        assert_eq!(file.content, "claude content");
        assert_eq!(file.path, dir.join("CLAUDE.md"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_project_instructions_none_when_missing() {
        let dir = make_temp_dir("none-missing");
        assert!(AgentEnv::load_project_instructions(&dir).is_none());
        fs::remove_dir_all(&dir).ok();
    }
}
