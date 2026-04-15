use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SYSTEM_PROMPT: &str = include_str!("../SYSTEM_PROMPT.md");

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
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("home directory not found")]
    HomeNotFound,
}

#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub fields: HashMap<String, serde_json::Value>,
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        let config_dir = Config::get_config_dir()?;
        let config_path = config_dir.join("config.json");

        if !config_path.exists() {
            tracing::debug!(config_path = %config_path.display(), "no config file found, using empty config");
            return Ok(Config {
                fields: HashMap::new(),
            });
        }

        let content = fs::read_to_string(config_path)?;
        let config: Config = serde_json::from_str(&content)?;
        Ok(config)
    }

    fn get_config_dir() -> Result<PathBuf, ConfigError> {
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

        // Find the git root directory
        let working_directory = env::current_dir().map_err(ConfigError::Io)?;
        if let Some(git_root) = find_git_root(&working_directory) {
            // Convert the git root path to a directory name
            let project_dir_name = path_to_dir_name(&git_root);
            let project_threads_dir = threads_base_dir.join(project_dir_name);

            // Create the directory if it doesn't exist
            if !project_threads_dir.exists() {
                fs::create_dir_all(&project_threads_dir)?;
            }

            Ok(project_threads_dir)
        } else {
            // Fallback to a default directory if no git root is found
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
    // Get the home directory
    if let Ok(home_dir) = env::var("HOME") {
        let home_path = Path::new(&home_dir);

        // Try to get the relative path from home
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

    // Fallback: use the last component of the path
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
    fn test_config_load_empty() {
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
