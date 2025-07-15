use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SYSTEM_PROMPT: &str = include_str!("../SYSTEM_PROMPT.md");

pub const AGENT_MD_PREFIX: &str = r#"
Here are instructions about the code base from the user. It's the contents
of a AGENT.md file. These instructions override default behavior and you
MUST follow them exactly as written:
"#;

/// The working environment of the agent, includes configuration, the system
/// prompt, working directories, etc.
#[derive(Debug, Clone)]
pub struct AgentEnv {
    pub working_directory: PathBuf,
    pub git_root_directory: Option<PathBuf>,
    pub operating_system: String,
    pub today_date: String,
    /// Contents of AGENT.md, if present.
    pub agent_md: Option<String>,
}

impl AgentEnv {
    pub fn new() -> Self {
        let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let git_root_directory = Self::find_git_root(&working_directory);
        let operating_system = env::consts::OS.to_string();
        let today_date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let agent_md_content = Self::load_agent_md(&working_directory);

        AgentEnv {
            working_directory,
            git_root_directory,
            operating_system,
            today_date,
            agent_md: agent_md_content,
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

    fn load_agent_md(working_directory: &Path) -> Option<String> {
        let agent_md_path = working_directory.join("AGENT.md");
        fs::read_to_string(agent_md_path).ok()
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
        let aj_dir = Path::new(&home_dir).join(".config").join("aj");

        // Create the ~/.config/aj directory if it doesn't exist
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
