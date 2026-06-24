//! Skill discovery and parsing.
//!
//! A skill is a directory containing a `SKILL.md` file: YAML frontmatter
//! (`name`, `description`, optional `disable-model-invocation`) followed by
//! markdown instructions. Sibling files (scripts, references) are freeform
//! and referenced relative to the skill directory.
//!
//! Skills are progressive disclosure: only name, description, and file
//! location are pinned into the system prompt (see
//! [`format_skills_for_prompt`]); the model reads the full `SKILL.md` on
//! demand with its file-reading tool. Accordingly, this module parses only
//! the frontmatter and never keeps a skill's body in memory.
//!
//! Discovery scans a fixed set of roots (see [`skill_roots`]): project-level
//! `.aj/skills/`, `.agents/skills/`, and `.claude/skills/` directories from
//! the working directory up to the git root, then the user-level
//! `~/.aj/skills/`, `~/.agents/skills/`, and `~/.claude/skills/`. The first
//! skill found wins a name collision, so more specific locations override
//! more general ones.
//!
//! Validation is lenient: format violations (bad name, overlong
//! description) produce a [`SkillDiagnostic`] but the skill still loads.
//! The one hard requirement is a non-empty `description` — without it the
//! model-visible listing entry would be useless.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::paths::display_path;

/// Maximum length of a skill name, per the Agent Skills convention.
const MAX_NAME_LENGTH: usize = 64;
/// Maximum length of a skill description, per the Agent Skills convention.
const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// A discovered skill: the parsed frontmatter of one `SKILL.md` file.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Stable name used for the model-visible listing and for the
    /// `disabled_skills` config option. From frontmatter `name`, falling
    /// back to the parent directory name.
    pub name: String,
    /// Model-visible description of when to use the skill.
    pub description: String,
    /// Absolute path to the `SKILL.md` file.
    pub path: PathBuf,
    /// `false` when the user disabled the skill via the `disabled_skills`
    /// config option. Disabled skills are kept on the env so the UI can
    /// show them, but they are excluded from the model-visible listing.
    pub enabled: bool,
    /// Frontmatter `disable-model-invocation: true`: the skill asked to
    /// stay out of the model-visible listing. Like a disabled skill it is
    /// still discovered and shown in the UI.
    pub disable_model_invocation: bool,
}

impl Skill {
    /// Whether the skill appears in the model-visible listing appended to
    /// the system prompt.
    pub fn in_model_context(&self) -> bool {
        self.enabled && !self.disable_model_invocation
    }
}

/// A non-fatal problem encountered while discovering or parsing skills.
/// Surfaced to the user (TUI scrollback, stderr in print mode); except for
/// a missing description the offending skill still loads.
#[derive(Debug, Clone)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for SkillDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "skill {}: {}", display_path(&self.path), self.message)
    }
}

/// The frontmatter fields we read. Unknown fields (`license`,
/// `metadata`, ...) are ignored.
#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "disable-model-invocation")]
    disable_model_invocation: Option<bool>,
}

/// Discover skills for the current process environment: project roots from
/// the working directory up to the git root, then user roots under `$HOME`.
/// `disabled` carries the `disabled_skills` config value; matching skills
/// are still returned but marked [`Skill::enabled`]` = false`.
pub fn discover_skills(disabled: &[String]) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let working_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let git_root = crate::paths::find_git_root(&working_directory);
    let home = crate::paths::home_dir();
    discover_skills_at(
        home.as_deref(),
        &working_directory,
        git_root.as_deref(),
        disabled,
    )
}

/// [`discover_skills`] against explicit locations, so discovery can be
/// exercised in tests without touching the process environment.
pub fn discover_skills_at(
    home: Option<&Path>,
    working_directory: &Path,
    git_root: Option<&Path>,
    disabled: &[String],
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills: Vec<Skill> = Vec::new();
    let mut diagnostics = Vec::new();
    // Symlinked directories can alias each other (or form cycles), so we
    // track every visited directory and skill file by canonical path and
    // scan each at most once.
    let mut visited_dirs = BTreeSet::new();
    let mut visited_files = BTreeSet::new();

    for root in skill_roots(home, working_directory, git_root) {
        if !root.is_dir() {
            continue;
        }
        scan_dir(
            &root,
            &mut skills,
            &mut diagnostics,
            &mut visited_dirs,
            &mut visited_files,
        );
    }

    for skill in &mut skills {
        skill.enabled = !disabled.contains(&skill.name);
    }

    (skills, diagnostics)
}

/// Skill directory roots in precedence order, most specific first:
/// `.aj/skills`, `.agents/skills`, and `.claude/skills` under each
/// directory from `working_directory` up to `git_root` (inclusive), then
/// the same three under `home`. Outside a git repository only the working
/// directory itself contributes project roots.
fn skill_roots(
    home: Option<&Path>,
    working_directory: &Path,
    git_root: Option<&Path>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for dir in crate::paths::project_dirs_upward(working_directory, git_root) {
        for sub in [".aj", ".agents", ".claude"] {
            roots.push(dir.join(sub).join("skills"));
        }
    }
    if let Some(home) = home {
        for sub in [".aj", ".agents", ".claude"] {
            roots.push(home.join(sub).join("skills"));
        }
    }
    roots
}

/// Recursively scan `dir` for skills. A directory containing a `SKILL.md`
/// is a skill root — load it and don't descend further (its subdirectories
/// are the skill's own scripts/references). Hidden directories and
/// `node_modules` are skipped.
fn scan_dir(
    dir: &Path,
    skills: &mut Vec<Skill>,
    diagnostics: &mut Vec<SkillDiagnostic>,
    visited_dirs: &mut BTreeSet<PathBuf>,
    visited_files: &mut BTreeSet<PathBuf>,
) {
    let Ok(canonical) = dir.canonicalize() else {
        return;
    };
    if !visited_dirs.insert(canonical) {
        return;
    }

    let skill_file = dir.join("SKILL.md");
    if skill_file.is_file() {
        if let Ok(canonical_file) = skill_file.canonicalize() {
            if !visited_files.insert(canonical_file) {
                return;
            }
        }
        if let Some(skill) = load_skill_from_file(&skill_file, diagnostics) {
            if let Some(existing) = skills.iter().find(|s| s.name == skill.name) {
                diagnostics.push(SkillDiagnostic {
                    path: skill.path.clone(),
                    message: format!(
                        "name `{}` already used by {}; ignoring this skill",
                        skill.name,
                        display_path(&existing.path)
                    ),
                });
            } else {
                skills.push(skill);
            }
        }
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    // Sort entries so discovery order (and thus collision resolution
    // within a root) is deterministic across platforms.
    let mut subdirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();
    for subdir in subdirs {
        let name = subdir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        scan_dir(&subdir, skills, diagnostics, visited_dirs, visited_files);
    }
}

/// Parse one `SKILL.md`. Returns `None` (with diagnostics) when the file
/// is unreadable, has no parseable frontmatter, or lacks a description;
/// every other violation is reported as a diagnostic while the skill still
/// loads.
fn load_skill_from_file(path: &Path, diagnostics: &mut Vec<SkillDiagnostic>) -> Option<Skill> {
    let mut diag = |message: String| {
        diagnostics.push(SkillDiagnostic {
            path: path.to_path_buf(),
            message,
        });
    };

    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            diag(format!("failed to read: {err}"));
            return None;
        }
    };

    let Some(frontmatter) = extract_frontmatter(&content) else {
        diag("missing YAML frontmatter (`---` block with a description)".to_string());
        return None;
    };
    let frontmatter: SkillFrontmatter = match serde_yaml_ng::from_str(frontmatter) {
        Ok(fm) => fm,
        Err(err) => {
            diag(format!("invalid YAML frontmatter: {err}"));
            return None;
        }
    };

    // The parent directory name is the conventional fallback when the
    // frontmatter doesn't name the skill.
    let parent_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let name = match frontmatter.name {
        Some(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => parent_name,
    };
    if name.is_empty() {
        diag("no usable name (no frontmatter `name`, no parent directory name)".to_string());
        return None;
    }
    if let Some(warning) = validate_name(&name) {
        diag(warning);
    }

    let description = frontmatter
        .description
        .map(|d| d.trim().to_string())
        .unwrap_or_default();
    if description.is_empty() {
        diag("missing `description` in frontmatter; skill not loaded".to_string());
        return None;
    }
    if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        diag(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters"
        ));
    }

    Some(Skill {
        name,
        description,
        path: path.to_path_buf(),
        enabled: true,
        disable_model_invocation: frontmatter.disable_model_invocation.unwrap_or(false),
    })
}

/// Check `name` against the Agent Skills conventions (≤64 chars, lowercase
/// alphanumerics and hyphens, no leading/trailing/double hyphen). Returns a
/// warning message on violation; the skill still loads.
fn validate_name(name: &str) -> Option<String> {
    if name.chars().count() > MAX_NAME_LENGTH {
        return Some(format!("name exceeds {MAX_NAME_LENGTH} characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Some(format!(
            "name `{name}` should use only lowercase letters, digits, and hyphens"
        ));
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Some(format!(
            "name `{name}` should not have leading, trailing, or consecutive hyphens"
        ));
    }
    None
}

/// Extract the YAML between a leading `---` line and the next `---` line.
/// Returns `None` when the file doesn't start with a frontmatter block or
/// the block is unterminated.
fn extract_frontmatter(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    for (idx, _) in rest.match_indices("\n---") {
        let after = &rest[idx + "\n---".len()..];
        let after = after.strip_prefix('\r').unwrap_or(after);
        if after.is_empty() || after.starts_with('\n') {
            return Some(&rest[..idx]);
        }
    }
    None
}

/// Render the model-visible skills listing appended to the system prompt,
/// or `None` when no skill is in model context. Only name, description,
/// and location are listed — the model reads the full `SKILL.md` with its
/// file-reading tool when a task matches, so a skill costs a few dozen
/// prompt tokens until it's actually needed.
pub fn format_skills_for_prompt(skills: &[Skill]) -> Option<String> {
    let visible: Vec<&Skill> = skills.iter().filter(|s| s.in_model_context()).collect();
    if visible.is_empty() {
        return None;
    }

    let mut text = String::from(
        "The following skills provide specialized instructions for specific tasks.\n\
         Use the read_file tool to load a skill's file when the task matches its description.\n\
         When a skill references a relative path, resolve it against the skill's directory \
         (the parent of its SKILL.md file).\n\n<available_skills>",
    );
    for skill in visible {
        text.push_str(&format!(
            "\n  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    \
             <location>{}</location>\n  </skill>",
            escape_xml(&skill.name),
            escape_xml(&skill.description),
            escape_xml(&skill.path.display().to_string()),
        ));
    }
    text.push_str("\n</available_skills>");
    Some(text)
}

/// Escape the XML-special characters in a text node.
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, sub: &str, frontmatter: &str, body: &str) -> PathBuf {
        let skill_dir = dir.join(sub);
        fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        fs::write(&path, format!("---\n{frontmatter}---\n{body}")).unwrap();
        path
    }

    #[test]
    fn test_load_skill_with_full_frontmatter() {
        let dir = crate::test_temp_dir("full");
        let path = write_skill(
            &dir,
            "my-skill",
            "name: custom-name\ndescription: Does things.\n",
            "Body text.",
        );

        let mut diagnostics = Vec::new();
        let skill = load_skill_from_file(&path, &mut diagnostics).expect("skill loads");
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
        assert_eq!(skill.name, "custom-name");
        assert_eq!(skill.description, "Does things.");
        assert_eq!(skill.path, path);
        assert!(skill.enabled);
        assert!(!skill.disable_model_invocation);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_skill_name_falls_back_to_directory() {
        let dir = crate::test_temp_dir("dir-name");
        let path = write_skill(&dir, "from-dir", "description: Does things.\n", "");

        let mut diagnostics = Vec::new();
        let skill = load_skill_from_file(&path, &mut diagnostics).expect("skill loads");
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
        assert_eq!(skill.name, "from-dir");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_skill_without_description_is_rejected() {
        let dir = crate::test_temp_dir("no-desc");
        let path = write_skill(&dir, "no-desc", "name: no-desc\n", "");

        let mut diagnostics = Vec::new();
        assert!(load_skill_from_file(&path, &mut diagnostics).is_none());
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("description"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_skill_without_frontmatter_is_rejected() {
        let dir = crate::test_temp_dir("no-fm");
        let skill_dir = dir.join("plain");
        fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        fs::write(&path, "# Just markdown\n").unwrap();

        let mut diagnostics = Vec::new();
        assert!(load_skill_from_file(&path, &mut diagnostics).is_none());
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("frontmatter"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_skill_bad_name_warns_but_loads() {
        let dir = crate::test_temp_dir("bad-name");
        let path = write_skill(
            &dir,
            "bad",
            "name: Bad_Name\ndescription: Does things.\n",
            "",
        );

        let mut diagnostics = Vec::new();
        let skill = load_skill_from_file(&path, &mut diagnostics).expect("loads with warning");
        assert_eq!(skill.name, "Bad_Name");
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("lowercase"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_skill_disable_model_invocation() {
        let dir = crate::test_temp_dir("dmi");
        let path = write_skill(
            &dir,
            "manual",
            "name: manual\ndescription: Does things.\ndisable-model-invocation: true\n",
            "",
        );

        let mut diagnostics = Vec::new();
        let skill = load_skill_from_file(&path, &mut diagnostics).expect("skill loads");
        assert!(skill.disable_model_invocation);
        assert!(!skill.in_model_context());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_extract_frontmatter_variants() {
        assert_eq!(extract_frontmatter("---\na: 1\n---\nbody"), Some("a: 1"));
        assert_eq!(extract_frontmatter("---\na: 1\n---"), Some("a: 1"));
        assert_eq!(
            extract_frontmatter("---\r\na: 1\r\n---\r\nbody"),
            Some("a: 1\r")
        );
        // Unterminated block or no block at all.
        assert_eq!(extract_frontmatter("---\na: 1\n"), None);
        assert_eq!(extract_frontmatter("# heading\n"), None);
        // A `----` ruler is not a terminator.
        assert_eq!(extract_frontmatter("---\na: 1\n----\n"), None);
    }

    #[test]
    fn test_validate_name() {
        assert!(validate_name("good-name-2").is_none());
        assert!(validate_name("Bad").is_some());
        assert!(validate_name("-lead").is_some());
        assert!(validate_name("trail-").is_some());
        assert!(validate_name("dou--ble").is_some());
        assert!(validate_name(&"x".repeat(65)).is_some());
    }

    #[test]
    fn test_discovery_walks_project_dirs_up_to_git_root() {
        let root = crate::test_temp_dir("walk");
        let cwd = root.join("nested/inner");
        fs::create_dir_all(&cwd).unwrap();
        write_skill(
            &root.join(".agents/skills"),
            "root-skill",
            "description: At the git root.\n",
            "",
        );
        write_skill(
            &cwd.join(".aj/skills"),
            "cwd-skill",
            "description: In the working directory.\n",
            "",
        );

        let (skills, diagnostics) = discover_skills_at(None, &cwd, Some(&root), &[]);
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        // Most specific (cwd) first.
        assert_eq!(names, vec!["cwd-skill", "root-skill"]);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_discovery_includes_user_roots_and_marks_disabled() {
        let home = crate::test_temp_dir("home");
        let cwd = crate::test_temp_dir("cwd");
        write_skill(
            &home.join(".claude/skills"),
            "user-skill",
            "description: User-level.\n",
            "",
        );

        let disabled = vec!["user-skill".to_string()];
        let (skills, diagnostics) = discover_skills_at(Some(&home), &cwd, None, &disabled);
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "user-skill");
        assert!(!skills[0].enabled);
        assert!(!skills[0].in_model_context());

        fs::remove_dir_all(&home).ok();
        fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn test_discovery_first_skill_wins_name_collision() {
        let root = crate::test_temp_dir("collision");
        let cwd = root.join("inner");
        fs::create_dir_all(&cwd).unwrap();
        let cwd_path = write_skill(
            &cwd.join(".agents/skills"),
            "shared",
            "name: shared\ndescription: Specific.\n",
            "",
        );
        write_skill(
            &root.join(".agents/skills"),
            "shared",
            "name: shared\ndescription: General.\n",
            "",
        );

        let (skills, diagnostics) = discover_skills_at(None, &cwd, Some(&root), &[]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].path, cwd_path);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("already used"));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_discovery_skips_hidden_and_node_modules() {
        let cwd = crate::test_temp_dir("skips");
        let skills_root = cwd.join(".agents/skills");
        write_skill(
            &skills_root.join("node_modules"),
            "dep-skill",
            "description: Should be skipped.\n",
            "",
        );
        write_skill(
            &skills_root.join(".hidden"),
            "hidden-skill",
            "description: Should be skipped.\n",
            "",
        );
        // Nested grouping directory: still found.
        write_skill(
            &skills_root.join("group"),
            "nested-skill",
            "description: Found via recursion.\n",
            "",
        );

        let (skills, _) = discover_skills_at(None, &cwd, None, &[]);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["nested-skill"]);

        fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn test_skill_dir_is_not_recursed_into() {
        let cwd = crate::test_temp_dir("no-recurse");
        let skills_root = cwd.join(".aj/skills");
        write_skill(&skills_root, "outer", "description: Outer skill.\n", "");
        // A SKILL.md nested inside a skill's own tree (e.g. a reference
        // copy) must not be picked up as a second skill.
        write_skill(
            &skills_root.join("outer/references"),
            "inner",
            "description: Inner copy.\n",
            "",
        );

        let (skills, diagnostics) = discover_skills_at(None, &cwd, None, &[]);
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["outer"]);

        fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn test_format_skills_for_prompt() {
        let skill = |name: &str, enabled: bool, dmi: bool| Skill {
            name: name.to_string(),
            description: format!("Description of {name} <&>."),
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
            enabled,
            disable_model_invocation: dmi,
        };

        // No model-visible skills → no block.
        assert_eq!(format_skills_for_prompt(&[]), None);
        assert_eq!(
            format_skills_for_prompt(&[skill("a", false, false), skill("b", true, true)]),
            None
        );

        let block =
            format_skills_for_prompt(&[skill("alpha", true, false), skill("beta", false, false)])
                .expect("block for visible skill");
        assert!(block.contains("<available_skills>"));
        assert!(block.contains("<name>alpha</name>"));
        assert!(block.contains("<location>/skills/alpha/SKILL.md</location>"));
        // Escaped description text.
        assert!(block.contains("Description of alpha &lt;&amp;&gt;."));
        // Disabled skills are excluded.
        assert!(!block.contains("beta"));
    }
}
