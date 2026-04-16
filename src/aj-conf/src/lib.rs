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
}

impl fmt::Display for ConfigThinkingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigThinkingLevel::Off => write!(f, "off"),
            ConfigThinkingLevel::Low => write!(f, "low"),
            ConfigThinkingLevel::Medium => write!(f, "medium"),
            ConfigThinkingLevel::High => write!(f, "high"),
            ConfigThinkingLevel::XHigh => write!(f, "xhigh"),
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
            _ => Err(format!(
                "invalid thinking level '{s}': expected off, low, medium, high, or xhigh"
            )),
        }
    }
}

pub const AGENTS_MD_PREFIX: &str = r#"
Here are instructions about the code base from the user. It's the contents
of an AGENTS.md file. These instructions override default behavior and you
must follow them exactly as written:
"#;

/// The working environment of the agent, includes configuration, the system
/// prompt, working directories, etc.
#[derive(Debug, Clone)]
pub struct AgentEnv {
    pub working_directory: PathBuf,
    pub git_root_directory: Option<PathBuf>,
    pub operating_system: String,
    pub today_date: String,
    /// Contents of AGENTS.md, if present.
    pub agents_md: Option<String>,
}

impl AgentEnv {
    pub fn new() -> Self {
        let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let git_root_directory = find_git_root(&working_directory);
        let operating_system = env::consts::OS.to_string();
        let today_date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let agents_md_content = Self::load_agents_md(&working_directory);

        AgentEnv {
            working_directory,
            git_root_directory,
            operating_system,
            today_date,
            agents_md: agents_md_content,
        }
    }

    fn load_agents_md(working_directory: &Path) -> Option<String> {
        // Try AGENTS.md first, then agents.md.
        let uppercase_path = working_directory.join("AGENTS.md");
        if let Ok(content) = fs::read_to_string(&uppercase_path) {
            return Some(content);
        }
        let lowercase_path = working_directory.join("agents.md");
        fs::read_to_string(lowercase_path).ok()
    }
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
    /// List of builtin tool names to disable. Tools in this list will not be
    /// available to the agent.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
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
}
