//! Filesystem path helpers: `$HOME` resolution, the `~/.aj/` directory
//! resolvers, git-root discovery, and display abbreviation.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::schema::{Config, ConfigError};

/// The user's home directory from `$HOME`, or `None` when it is unset.
///
/// The single place this crate reads `$HOME`. Each caller adapts the
/// `None` case to its own policy: [`Config::get_config_dir`] turns it
/// into [`ConfigError::HomeNotFound`], path display falls back to the
/// unabbreviated path, and instruction/skill discovery skips the
/// user-level lookup.
pub(crate) fn home_dir() -> Option<PathBuf> {
    env::var("HOME").ok().map(PathBuf::from)
}

pub(crate) fn find_git_root(start_path: &Path) -> Option<PathBuf> {
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

/// Directories from `working_directory` up to `git_root` (inclusive), most
/// specific first. Just the working directory when there is no git root.
/// `git_root` normally comes from [`find_git_root`] and is therefore an
/// ancestor of the working directory; one that isn't degrades to the
/// working directory only.
pub(crate) fn project_dirs_upward(
    working_directory: &Path,
    git_root: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = vec![working_directory.to_path_buf()];
    let Some(git_root) = git_root else {
        return dirs;
    };
    if !working_directory.starts_with(git_root) {
        return dirs;
    }
    let mut current = working_directory;
    while current != git_root {
        match current.parent() {
            Some(parent) => {
                dirs.push(parent.to_path_buf());
                current = parent;
            }
            None => break,
        }
    }
    dirs
}

/// Render `path` for display. If it lives under `$HOME`, abbreviate the home
/// prefix to `~`.
pub fn display_path(path: &Path) -> String {
    if let Some(home) = home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

/// Convert a path to a directory name by taking components after the home
/// directory and joining them with dashes. For example, /Users/user/Dev/project
/// becomes "Dev-project".
///
/// NOTE: the mapping is lossy and not collision-free. Dash-joining drops the
/// distinction between a separator and a literal dash, so `~/Dev/project` and
/// `~/Dev-project` both yield `Dev-project`, and a path outside `$HOME` falls
/// back to its last component, so `/opt/foo` and `/srv/foo` both yield `foo`.
/// Two projects that collide here share one [`Config::get_sessions_dir_path`]
/// directory and interleave their sessions. Rare in practice and tolerated
/// rather than guarded.
fn path_to_dir_name(path: &Path) -> String {
    if let Some(home_path) = home_dir() {
        // Try to get the relative path from home.
        if let Ok(relative_path) = path.strip_prefix(&home_path) {
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

impl Config {
    /// Returns the path to the `~/.aj/` config directory, creating it if
    /// necessary.
    pub fn get_config_dir() -> Result<PathBuf, ConfigError> {
        let aj_dir = home_dir().ok_or(ConfigError::HomeNotFound)?.join(".aj");

        if !aj_dir.exists() {
            fs::create_dir_all(&aj_dir)?;
        }

        Ok(aj_dir)
    }

    /// Path to `~/.aj/config.toml`. Creates the `~/.aj` directory if
    /// it doesn't exist (via [`Self::get_config_dir`]) but does not
    /// create the file itself.
    pub fn config_file_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::get_config_dir()?.join("config.toml"))
    }

    pub fn get_dotenv_file_path() -> Result<PathBuf, ConfigError> {
        let aj_dir = Self::get_config_dir()?;
        Ok(aj_dir.join(".env"))
    }

    /// Get the sessions directory path for the current project. The sessions
    /// are stored in subdirectories based on the git root directory. For
    /// example, if the git root is /Users/user/Dev/project, the subdirectory
    /// name will be "Dev-project".
    ///
    /// NOTE: the subdirectory name comes from `path_to_dir_name`, which is
    /// lossy. Two distinct projects whose names collide there share this
    /// directory and interleave their sessions. See that function.
    pub fn get_sessions_dir_path() -> Result<PathBuf, ConfigError> {
        let aj_dir = Self::get_config_dir()?;
        let sessions_base_dir = aj_dir.join("sessions");

        // Find the git root directory.
        let working_directory = env::current_dir().map_err(ConfigError::Io)?;
        if let Some(git_root) = find_git_root(&working_directory) {
            // Convert the git root path to a directory name.
            let project_dir_name = path_to_dir_name(&git_root);
            let project_sessions_dir = sessions_base_dir.join(project_dir_name);

            // Create the directory if it doesn't exist.
            if !project_sessions_dir.exists() {
                fs::create_dir_all(&project_sessions_dir)?;
            }

            Ok(project_sessions_dir)
        } else {
            // Fallback to a default directory if no git root is found.
            let default_sessions_dir = sessions_base_dir.join("default");
            if !default_sessions_dir.exists() {
                fs::create_dir_all(&default_sessions_dir)?;
            }
            Ok(default_sessions_dir)
        }
    }

    /// Path to the base directory holding every project's sessions
    /// subdirectory: `~/.aj/sessions`. Each immediate subdirectory is
    /// one project (named via `path_to_dir_name`). The prompt-history
    /// "all workspaces" search walks these. Unlike
    /// [`Self::get_sessions_dir_path`] this does not create or descend
    /// into a per-project directory. It just resolves the base path.
    pub fn get_sessions_base_dir_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::get_config_dir()?.join("sessions"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_project_dirs_upward() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/a/b");
        assert_eq!(
            project_dirs_upward(cwd, Some(root)),
            vec![
                PathBuf::from("/repo/a/b"),
                PathBuf::from("/repo/a"),
                PathBuf::from("/repo"),
            ]
        );
        // cwd == git root.
        assert_eq!(
            project_dirs_upward(root, Some(root)),
            vec![root.to_path_buf()]
        );
        // No git root, or one that isn't an ancestor.
        assert_eq!(project_dirs_upward(cwd, None), vec![cwd.to_path_buf()]);
        assert_eq!(
            project_dirs_upward(cwd, Some(Path::new("/elsewhere"))),
            vec![cwd.to_path_buf()]
        );
    }
}
