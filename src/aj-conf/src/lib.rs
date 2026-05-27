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

/// How the assistant's reasoning channel is surfaced in
/// `config.toml`. Mirrors [`aj_models::types::ThinkingDisplay`] —
/// the field name matches the Anthropic SDK's `display` knob so
/// users who've read the upstream docs find the same vocabulary
/// here.
///
/// How much of the assistant's reasoning channel to surface to the
/// user. A single knob that fans out to both provider-specific wire
/// fields:
///
/// | Variant       | Anthropic `thinking.display` | OpenAI Responses `reasoning.summary` |
/// |---------------|------------------------------|--------------------------------------|
/// | `Summarized`  | `Summarized`                 | `Concise`                            |
/// | `Detailed`    | `Summarized`*                | `Detailed`                           |
/// | `Omitted`     | `Omitted`                    | (no summary requested)               |
///
/// *Anthropic has no "detailed" mode for adaptive thinking — it
/// degrades to `Summarized` and the user gets the verbose
/// counterpart only on OpenAI Responses. Leaving the config key
/// unset is the cross-provider default ("provider default behavior")
/// and is generally what produces a `Thinking…` placeholder with no
/// streamed body on adaptive Anthropic models, and no reasoning
/// summary on OpenAI Responses.
///
/// Providers that don't have a reasoning channel knob at all (e.g.
/// Chat Completions) see both wire fields populated by the mapping
/// and ignore them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigThinkingDisplay {
    /// Stream a terse model-generated summary of the reasoning.
    /// Maps onto Anthropic `Summarized` and OpenAI `Concise`.
    Summarized,
    /// Stream a verbose model-generated summary of the reasoning.
    /// Maps onto Anthropic `Summarized` (no Detailed variant) and
    /// OpenAI `Detailed`.
    Detailed,
    /// Suppress the reasoning channel entirely. Maps onto
    /// Anthropic `Omitted`; on OpenAI Responses this is achieved by
    /// not requesting a summary (equivalent to leaving the key
    /// unset on that provider).
    Omitted,
}

impl fmt::Display for ConfigThinkingDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigThinkingDisplay::Summarized => write!(f, "summarized"),
            ConfigThinkingDisplay::Detailed => write!(f, "detailed"),
            ConfigThinkingDisplay::Omitted => write!(f, "omitted"),
        }
    }
}

impl FromStr for ConfigThinkingDisplay {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "summarized" => Ok(ConfigThinkingDisplay::Summarized),
            "detailed" => Ok(ConfigThinkingDisplay::Detailed),
            "omitted" => Ok(ConfigThinkingDisplay::Omitted),
            _ => Err(format!(
                "invalid thinking_display '{s}': expected summarized, detailed, or omitted"
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
    #[error("home directory not found")]
    HomeNotFound,
}

/// Severity of a [`ConfigDiagnostic`]. Determines how the diagnostic
/// should be surfaced to the user (e.g. yellow vs red in the TUI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The config was loaded but something in it was ignored (e.g. an
    /// unknown key). The user's other settings still took effect.
    Warning,
    /// The config could not be applied at all and built-in defaults
    /// were used instead. The user almost certainly wants to fix this.
    Error,
}

/// A non-fatal problem encountered while loading `~/.aj/config.toml`.
///
/// `Config::load` returns one of these per issue alongside the
/// best-effort parsed config so the binary can surface them to the
/// user (TUI chat scrollback, stderr in print mode) instead of
/// silently falling back to defaults.
#[derive(Debug, Clone)]
pub enum ConfigDiagnostic {
    /// `config.toml` exists but could not be read (e.g. permissions).
    /// Built-in defaults were used.
    Unreadable { path: PathBuf, error: String },
    /// `config.toml` is not syntactically valid TOML. Built-in
    /// defaults were used; the entire file is ignored.
    ParseFailed { path: PathBuf, error: String },
    /// A known key has a value that failed to deserialize (unknown
    /// enum variant, wrong type, etc). Only this field was dropped —
    /// the rest of the file still took effect.
    InvalidValue {
        path: PathBuf,
        key: String,
        error: String,
    },
    /// A top-level key in `config.toml` that [`Config`] doesn't
    /// recognize. The rest of the file was parsed normally; this key
    /// was dropped. `suggestion` carries the closest known key when
    /// the typo is within edit-distance range, so the user gets a
    /// "did you mean `theme`?" hint for `themee`.
    UnknownKey {
        path: PathBuf,
        key: String,
        suggestion: Option<&'static str>,
    },
}

impl ConfigDiagnostic {
    pub fn severity(&self) -> Severity {
        match self {
            ConfigDiagnostic::Unreadable { .. } | ConfigDiagnostic::ParseFailed { .. } => {
                Severity::Error
            }
            ConfigDiagnostic::InvalidValue { .. } | ConfigDiagnostic::UnknownKey { .. } => {
                Severity::Warning
            }
        }
    }
}

impl fmt::Display for ConfigDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigDiagnostic::Unreadable { path, error } => {
                write!(
                    f,
                    "failed to read {} (using built-in defaults): {error}",
                    display_path(path)
                )
            }
            ConfigDiagnostic::ParseFailed { path, error } => {
                // The TOML crate's error already includes line/column
                // and a caret-pointed snippet; pass it through verbatim
                // so the user sees the same diagnostic they'd get from
                // `taplo` or any other TOML tool.
                write!(
                    f,
                    "failed to parse {} (using built-in defaults):\n{error}",
                    display_path(path)
                )
            }
            ConfigDiagnostic::InvalidValue { path, key, error } => {
                // Per-field error: other keys still applied. We strip
                // any trailing newline the toml error tacks on so the
                // one-line warning format stays one line.
                let error = error.trim_end();
                write!(
                    f,
                    "{}: invalid value for `{key}` (ignored): {error}",
                    display_path(path)
                )
            }
            ConfigDiagnostic::UnknownKey {
                path,
                key,
                suggestion,
            } => match suggestion {
                Some(s) => write!(
                    f,
                    "{}: unknown key `{key}` (did you mean `{s}`?)",
                    display_path(path)
                ),
                None => write!(f, "{}: unknown key `{key}`", display_path(path)),
            },
        }
    }
}

/// Kind of value a [`ConfigOption`] accepts. Drives help text and
/// (eventually) tab completion in the settings command — the file
/// parser uses the option's `apply_toml_fn` directly and doesn't
/// need to branch on this.
#[derive(Debug, Clone, Copy)]
pub enum ValueKind {
    /// A free-form string (stored as `Option<String>` on `Config`).
    String,
    /// A boolean.
    Bool,
    /// One of a fixed set of variants. The slice lists every
    /// accepted value in its canonical (lowercase) form, in the
    /// order they should be shown to the user.
    Enum(&'static [&'static str]),
    /// A list of strings.
    StringList,
}

impl fmt::Display for ValueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValueKind::String => write!(f, "string"),
            ValueKind::Bool => write!(f, "bool"),
            ValueKind::Enum(variants) => write!(f, "{}", variants.join(" | ")),
            ValueKind::StringList => write!(f, "list of strings"),
        }
    }
}

/// Schema entry for a single key in `~/.aj/config.toml`.
///
/// The full table is [`Config::OPTIONS`] — that's the single source
/// of truth the file parser, the unknown-key suggester, and the
/// settings command all walk. To add a new config option: add a
/// field to [`Config`], then add a matching entry to
/// [`Config::OPTIONS`]. The `test_options_table_matches_config_fields`
/// test catches drift.
///
/// The two `fn` pointers are intentionally private — callers go
/// through [`Self::apply_toml`] / [`Self::display`] so the schema
/// stays the only place that touches `Config` fields by name.
pub struct ConfigOption {
    /// Key as it appears in `config.toml` and on the CLI.
    pub name: &'static str,
    /// One-line user-facing description, suitable for `aj settings show`.
    pub description: &'static str,
    /// What the option accepts. Used for help text and (future) tab
    /// completion.
    pub kind: ValueKind,
    /// Parse `value` and write it into the matching field of `config`.
    /// Returns the toml error verbatim on failure so the parser can
    /// wrap it in [`ConfigDiagnostic::InvalidValue`].
    apply_toml_fn: fn(toml::Value, &mut Config) -> Result<(), toml::de::Error>,
    /// Render the field's current value, distinguishing unset
    /// (`<unset>`) from set values.
    display_fn: fn(&Config) -> String,
}

impl ConfigOption {
    /// Apply a parsed TOML value to `config`.
    pub fn apply_toml(
        &self,
        value: toml::Value,
        config: &mut Config,
    ) -> Result<(), toml::de::Error> {
        (self.apply_toml_fn)(value, config)
    }

    /// Render the field's current value for display.
    pub fn display(&self, config: &Config) -> String {
        (self.display_fn)(config)
    }
}

/// Display helper for `Option<T: Display>` fields. Returns the
/// inner value's `Display` form when set, or the literal string
/// `<unset>` otherwise.
fn display_opt<T: fmt::Display>(value: &Option<T>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "<unset>".to_string(),
    }
}

/// Display helper for `Vec<String>` list fields. Renders as
/// `["a", "b"]`, or `<empty>` when the list has no entries.
fn display_string_list(value: &[String]) -> String {
    if value.is_empty() {
        "<empty>".to_string()
    } else {
        let items: Vec<String> = value.iter().map(|s| format!("\"{s}\"")).collect();
        format!("[{}]", items.join(", "))
    }
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
/// thinking_display = "summarized"
/// theme = "dark"
/// disabled_tools = ["todo_read", "todo_write"]
/// hide_thinking_block = false
/// ```
#[derive(Debug, Clone)]
pub struct Config {
    /// Model API backend (e.g., "anthropic", "openai").
    pub model_api: Option<String>,
    /// Custom model endpoint URL.
    pub model_url: Option<String>,
    /// Model name override.
    pub model_name: Option<String>,
    /// Default thinking level used when no trigger word is present.
    pub thinking: Option<ConfigThinkingLevel>,
    /// How much of the assistant's reasoning channel to surface to
    /// the user. Defaults to `None`, which leaves both providers'
    /// upstream defaults in place — that's typically a `Thinking…`
    /// placeholder with no streamed body on adaptive Anthropic
    /// models, and no reasoning summary on OpenAI Responses.
    /// See [`ConfigThinkingDisplay`] for the per-variant mapping.
    pub thinking_display: Option<ConfigThinkingDisplay>,
    /// Inference speed mode (Anthropic only). `fast` enables higher
    /// output-tokens-per-second at some quality cost.
    pub speed: Option<ConfigSpeed>,
    /// Interactive TUI theme name. Resolved against the bundled
    /// catalog (`dark`, `light`) plus any `*.json` files in
    /// `~/.aj/themes/`. Defaults to `dark` when unset.
    pub theme: Option<String>,
    /// List of builtin tool names to disable. Tools in this list will not be
    /// available to the agent.
    pub disabled_tools: Vec<String>,
    /// Replace expanded thinking blocks with a single italic
    /// "Thinking…" placeholder line in the interactive TUI.
    /// Defaults to `false` (expanded). Toggled at runtime with
    /// `Ctrl+T`; see `docs/aj-next-plan.md` §4.4.
    pub hide_thinking_block: bool,
    /// Whether `read_file` resizes images to fit within the inline
    /// image budget before attaching them to tool results. Defaults
    /// to `true`; setting to `false` attaches the raw bytes, which
    /// is useful for full-quality images but may be rejected by the
    /// wire layer when the source exceeds the provider's per-image
    /// size limit.
    pub image_auto_resize: bool,
    /// Whether the interactive TUI renders tool-result image
    /// attachments inline via Kitty graphics / iTerm2 OSC 1337.
    /// Defaults to `true`. When `false`, the textual placeholder
    /// (`[image: mime · WxH]`) is shown regardless of terminal
    /// capability. Independent of `image_block`: this only affects
    /// what the user sees, not what the model receives.
    pub image_show_in_terminal: bool,
    /// Defense-in-depth: when `true`, strip every
    /// [`aj_models::types::UserContent::Image`] block from outgoing
    /// wire messages (both user messages and tool result messages)
    /// and replace each with a single text block noting the
    /// omission. The model never sees the bytes regardless of its
    /// declared vision capability. Defaults to `false`.
    pub image_block: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model_api: None,
            model_url: None,
            model_name: None,
            thinking: None,
            thinking_display: None,
            speed: None,
            theme: None,
            disabled_tools: Vec::new(),
            hide_thinking_block: false,
            // Image features: resize and inline-render by default;
            // blocking is opt-in.
            image_auto_resize: true,
            image_show_in_terminal: true,
            image_block: false,
        }
    }
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
    /// Schema for every option this binary understands. The file
    /// parser, the unknown-key suggester, and (eventually) the
    /// `settings` command all walk this table — there is no other
    /// source of truth for what `~/.aj/config.toml` accepts.
    ///
    /// Each entry's `description` is the user-facing one-liner shown
    /// by the settings command; the field-level doc comments on
    /// [`Config`] are the developer-facing reference. Keep them
    /// roughly consistent but they don't need to match verbatim.
    pub const OPTIONS: &'static [ConfigOption] = &[
        ConfigOption {
            name: "model_api",
            description: "Model API backend (e.g. \"anthropic\", \"openai\").",
            kind: ValueKind::String,
            apply_toml_fn: |v, c| {
                c.model_api = v.try_into()?;
                Ok(())
            },
            display_fn: |c| display_opt(&c.model_api),
        },
        ConfigOption {
            name: "model_url",
            description: "Custom model endpoint URL.",
            kind: ValueKind::String,
            apply_toml_fn: |v, c| {
                c.model_url = v.try_into()?;
                Ok(())
            },
            display_fn: |c| display_opt(&c.model_url),
        },
        ConfigOption {
            name: "model_name",
            description: "Model name override.",
            kind: ValueKind::String,
            apply_toml_fn: |v, c| {
                c.model_name = v.try_into()?;
                Ok(())
            },
            display_fn: |c| display_opt(&c.model_name),
        },
        ConfigOption {
            name: "thinking",
            description: "Default thinking level used when no trigger word is present.",
            kind: ValueKind::Enum(&["off", "low", "medium", "high", "xhigh", "max"]),
            apply_toml_fn: |v, c| {
                c.thinking = v.try_into()?;
                Ok(())
            },
            display_fn: |c| display_opt(&c.thinking),
        },
        ConfigOption {
            name: "thinking_display",
            description: "How much of the assistant's reasoning channel to surface to the user.",
            kind: ValueKind::Enum(&["summarized", "detailed", "omitted"]),
            apply_toml_fn: |v, c| {
                c.thinking_display = v.try_into()?;
                Ok(())
            },
            display_fn: |c| display_opt(&c.thinking_display),
        },
        ConfigOption {
            name: "speed",
            description: "Inference speed mode (Anthropic only).",
            kind: ValueKind::Enum(&["standard", "fast"]),
            apply_toml_fn: |v, c| {
                c.speed = v.try_into()?;
                Ok(())
            },
            display_fn: |c| display_opt(&c.speed),
        },
        ConfigOption {
            name: "theme",
            description: "Interactive TUI theme name (built-ins: dark, light).",
            kind: ValueKind::String,
            apply_toml_fn: |v, c| {
                c.theme = v.try_into()?;
                Ok(())
            },
            display_fn: |c| display_opt(&c.theme),
        },
        ConfigOption {
            name: "disabled_tools",
            description: "Builtin tool names to hide from the agent.",
            kind: ValueKind::StringList,
            apply_toml_fn: |v, c| {
                c.disabled_tools = v.try_into()?;
                Ok(())
            },
            display_fn: |c| display_string_list(&c.disabled_tools),
        },
        ConfigOption {
            name: "hide_thinking_block",
            description: "Collapse expanded thinking blocks to a placeholder in the TUI.",
            kind: ValueKind::Bool,
            apply_toml_fn: |v, c| {
                c.hide_thinking_block = v.try_into()?;
                Ok(())
            },
            display_fn: |c| c.hide_thinking_block.to_string(),
        },
        ConfigOption {
            name: "image_auto_resize",
            description: "Resize images attached by tools (e.g. read_file) to fit the inline image budget.",
            kind: ValueKind::Bool,
            apply_toml_fn: |v, c| {
                c.image_auto_resize = v.try_into()?;
                Ok(())
            },
            display_fn: |c| c.image_auto_resize.to_string(),
        },
        ConfigOption {
            name: "image_show_in_terminal",
            description: "Render tool-result images inline in the TUI when the terminal supports it.",
            kind: ValueKind::Bool,
            apply_toml_fn: |v, c| {
                c.image_show_in_terminal = v.try_into()?;
                Ok(())
            },
            display_fn: |c| c.image_show_in_terminal.to_string(),
        },
        ConfigOption {
            name: "image_block",
            description: "Strip image attachments from outgoing wire messages (defense-in-depth).",
            kind: ValueKind::Bool,
            apply_toml_fn: |v, c| {
                c.image_block = v.try_into()?;
                Ok(())
            },
            display_fn: |c| c.image_block.to_string(),
        },
    ];

    /// Look up an option by its config key, if any. Returns `None`
    /// for unknown keys.
    pub fn option(name: &str) -> Option<&'static ConfigOption> {
        Self::OPTIONS.iter().find(|o| o.name == name)
    }

    /// Load configuration from `~/.aj/config.toml`.
    ///
    /// Always returns a [`Config`]: a missing file yields defaults
    /// with no diagnostics, while a malformed file yields defaults
    /// plus a [`ConfigDiagnostic::ParseFailed`] so the caller can
    /// surface the failure. Unknown top-level keys are reported as
    /// [`ConfigDiagnostic::UnknownKey`] warnings while the rest of
    /// the file is honored.
    ///
    /// Truly fatal conditions (no `$HOME`, can't `mkdir ~/.aj`)
    /// degrade gracefully to defaults + no diagnostics — other code
    /// paths that actually need those directories (threads, dotenv)
    /// will surface their own errors.
    pub fn load() -> (Self, Vec<ConfigDiagnostic>) {
        let Ok(config_dir) = Self::get_config_dir() else {
            return (Config::default(), Vec::new());
        };
        let config_path = config_dir.join("config.toml");

        if !config_path.exists() {
            tracing::debug!(config_path = %config_path.display(), "no config file found, using defaults");
            return (Config::default(), Vec::new());
        }

        match fs::read_to_string(&config_path) {
            Ok(content) => {
                tracing::debug!(config_path = %config_path.display(), "loaded config");
                parse_config(&content, &config_path)
            }
            Err(e) => (
                Config::default(),
                vec![ConfigDiagnostic::Unreadable {
                    path: config_path,
                    error: e.to_string(),
                }],
            ),
        }
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

/// Parse a `config.toml` content string into a [`Config`] plus a list
/// of [`ConfigDiagnostic`]s describing any non-fatal issues. The
/// `path` is only used for diagnostic messages — it isn't read.
///
/// Per-field leniency: each top-level key is dispatched against
/// [`Config::OPTIONS`] and applied independently. A bad value for
/// `thinking` doesn't prevent `model_api` and friends from taking
/// effect — only the offending field is dropped (and reported as
/// [`ConfigDiagnostic::InvalidValue`]). Wholesale fallback to
/// defaults only happens when the file isn't valid TOML at all
/// ([`ConfigDiagnostic::ParseFailed`]).
fn parse_config(content: &str, path: &Path) -> (Config, Vec<ConfigDiagnostic>) {
    let table = match content.parse::<toml::Table>() {
        Ok(t) => t,
        Err(e) => {
            return (
                Config::default(),
                vec![ConfigDiagnostic::ParseFailed {
                    path: path.to_path_buf(),
                    error: e.to_string(),
                }],
            );
        }
    };

    let mut config = Config::default();
    let mut diagnostics = Vec::new();

    for (key, value) in table {
        match Config::option(&key) {
            Some(option) => {
                if let Err(e) = option.apply_toml(value, &mut config) {
                    diagnostics.push(ConfigDiagnostic::InvalidValue {
                        path: path.to_path_buf(),
                        key,
                        error: e.to_string(),
                    });
                }
            }
            None => diagnostics.push(ConfigDiagnostic::UnknownKey {
                path: path.to_path_buf(),
                suggestion: suggest_key(&key),
                key,
            }),
        }
    }

    (config, diagnostics)
}

/// Return the closest known key to `unknown` if it's within
/// edit-distance range, or `None` if nothing is similar enough to be
/// worth suggesting.
///
/// Threshold: distance strictly less than half the user's key length,
/// capped at 3. That accepts `themee` → `theme` (dist 1) and
/// `disabled_tool` → `disabled_tools` (dist 1) while rejecting
/// `completely_unrelated` from matching anything.
fn suggest_key(unknown: &str) -> Option<&'static str> {
    let max_distance = (unknown.len() / 2).min(3).max(1);
    Config::OPTIONS
        .iter()
        .map(|o| (o.name, strsim::levenshtein(unknown, o.name)))
        .filter(|(_, d)| *d <= max_distance)
        .min_by_key(|(_, d)| *d)
        .map(|(k, _)| k)
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
    fn test_config_deserialize_thinking_display() {
        // Unset → None so the binary leaves both providers' upstream
        // defaults in place.
        let (config, diag) = parse_config("", Path::new("/tmp/config.toml"));
        assert!(diag.is_empty());
        assert!(config.thinking_display.is_none());

        let cases = [
            ("summarized", ConfigThinkingDisplay::Summarized),
            ("detailed", ConfigThinkingDisplay::Detailed),
            ("omitted", ConfigThinkingDisplay::Omitted),
        ];
        for (input, expected) in cases {
            let toml_str = format!("thinking_display = \"{input}\"");
            let (config, diag) = parse_config(&toml_str, Path::new("/tmp/config.toml"));
            assert!(diag.is_empty(), "failed for {input}: {diag:?}");
            assert_eq!(
                config.thinking_display,
                Some(expected),
                "failed for input: {input}"
            );
        }
    }

    #[test]
    fn test_config_thinking_display_from_str_round_trips() {
        for variant in [
            ConfigThinkingDisplay::Summarized,
            ConfigThinkingDisplay::Detailed,
            ConfigThinkingDisplay::Omitted,
        ] {
            let s = variant.to_string();
            assert_eq!(s.parse::<ConfigThinkingDisplay>().unwrap(), variant);
        }
        // Case-insensitive parse, matching the
        // `ConfigThinkingLevel` precedent.
        assert_eq!(
            "SUMMARIZED".parse::<ConfigThinkingDisplay>().unwrap(),
            ConfigThinkingDisplay::Summarized,
        );
        assert!("nope".parse::<ConfigThinkingDisplay>().is_err());
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
            let (config, diag) = parse_config(&toml_str, Path::new("/tmp/config.toml"));
            assert!(diag.is_empty(), "failed for {input}: {diag:?}");
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
    fn test_config_load_missing_file() {
        // Config::load() always succeeds — a missing file just yields
        // defaults with no diagnostics. We don't assert specifics about
        // the surrounding environment here; we just confirm the call
        // doesn't panic and returns the expected tuple shape.
        let (_config, _diagnostics) = Config::load();
    }

    #[test]
    fn test_parse_config_empty_yields_no_diagnostics() {
        let (config, diagnostics) = parse_config("", Path::new("/tmp/config.toml"));
        assert!(diagnostics.is_empty());
        assert!(config.model_api.is_none());
    }

    #[test]
    fn test_parse_config_valid_yields_no_diagnostics() {
        let toml_str = r#"
model_api = "anthropic"
thinking = "medium"
theme = "dark"
"#;
        let (config, diagnostics) = parse_config(toml_str, Path::new("/tmp/config.toml"));
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
        assert_eq!(config.model_api.as_deref(), Some("anthropic"));
        assert_eq!(config.thinking, Some(ConfigThinkingLevel::Medium));
        assert_eq!(config.theme.as_deref(), Some("dark"));
    }

    #[test]
    fn test_parse_config_unknown_key_reports_warning_with_suggestion() {
        // `themee` is one edit away from `theme`; expect a hint.
        let toml_str = r#"
model_api = "anthropic"
themee = "dark"
"#;
        let path = Path::new("/tmp/config.toml");
        let (config, diagnostics) = parse_config(toml_str, path);

        // The valid key still took effect.
        assert_eq!(config.model_api.as_deref(), Some("anthropic"));
        assert!(config.theme.is_none());

        assert_eq!(diagnostics.len(), 1);
        match &diagnostics[0] {
            ConfigDiagnostic::UnknownKey {
                key, suggestion, ..
            } => {
                assert_eq!(key, "themee");
                assert_eq!(*suggestion, Some("theme"));
            }
            other => panic!("expected UnknownKey, got {other:?}"),
        }
        assert_eq!(diagnostics[0].severity(), Severity::Warning);
    }

    #[test]
    fn test_parse_config_unknown_key_no_suggestion_when_unrelated() {
        let toml_str = r#"completely_unrelated_setting = 42"#;
        let (_config, diagnostics) = parse_config(toml_str, Path::new("/tmp/config.toml"));
        assert_eq!(diagnostics.len(), 1);
        match &diagnostics[0] {
            ConfigDiagnostic::UnknownKey {
                key, suggestion, ..
            } => {
                assert_eq!(key, "completely_unrelated_setting");
                assert_eq!(*suggestion, None);
            }
            other => panic!("expected UnknownKey, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_config_reports_all_unknown_keys() {
        let toml_str = r#"
themee = "dark"
disabled_tool = ["x"]
"#;
        let (_config, diagnostics) = parse_config(toml_str, Path::new("/tmp/config.toml"));
        assert_eq!(diagnostics.len(), 2);
        let keys: Vec<&str> = diagnostics
            .iter()
            .map(|d| match d {
                ConfigDiagnostic::UnknownKey { key, .. } => key.as_str(),
                _ => panic!("expected UnknownKey"),
            })
            .collect();
        assert!(keys.contains(&"themee"));
        assert!(keys.contains(&"disabled_tool"));
    }

    #[test]
    fn test_parse_config_invalid_value_keeps_other_fields() {
        // `thinking = "meh"` is an unknown enum variant. Under the
        // lenient parser, the bad field is dropped and the rest of
        // the file still takes effect.
        let toml_str = r#"
model_api = "anthropic"
model_name = "claude-sonnet-4-20250514"
thinking = "meh"
theme = "dark"
"#;
        let (config, diagnostics) = parse_config(toml_str, Path::new("/tmp/config.toml"));

        // The valid keys took effect.
        assert_eq!(config.model_api.as_deref(), Some("anthropic"));
        assert_eq!(
            config.model_name.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert_eq!(config.theme.as_deref(), Some("dark"));
        // The bad key was dropped to its default.
        assert!(config.thinking.is_none());

        assert_eq!(diagnostics.len(), 1);
        match &diagnostics[0] {
            ConfigDiagnostic::InvalidValue { key, error, .. } => {
                assert_eq!(key, "thinking");
                assert!(error.contains("meh"), "got: {error}");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
        assert_eq!(diagnostics[0].severity(), Severity::Warning);
    }

    #[test]
    fn test_parse_config_invalid_value_alongside_unknown_key() {
        let toml_str = r#"
themee = "dark"
thinking = "meh"
model_api = "anthropic"
"#;
        let (config, diagnostics) = parse_config(toml_str, Path::new("/tmp/config.toml"));

        assert_eq!(config.model_api.as_deref(), Some("anthropic"));
        assert_eq!(diagnostics.len(), 2);
        // Both a typo warning and an invalid-value warning should
        // appear; order matches the order of keys in the file (which
        // toml::Table preserves as BTreeMap sort order — alphabetical).
        assert!(
            diagnostics
                .iter()
                .any(|d| matches!(d, ConfigDiagnostic::UnknownKey { key, .. } if key == "themee"))
        );
        assert!(
            diagnostics.iter().any(
                |d| matches!(d, ConfigDiagnostic::InvalidValue { key, .. } if key == "thinking")
            )
        );
    }

    #[test]
    fn test_parse_config_syntax_failure_yields_defaults_and_error() {
        // Genuine TOML syntax error: missing closing quote.
        let toml_str = r#"model_api = "anthropic"#;
        let (config, diagnostics) = parse_config(toml_str, Path::new("/tmp/config.toml"));

        // Defaults: nothing from the file applied.
        assert!(config.model_api.is_none());

        assert_eq!(diagnostics.len(), 1);
        assert!(matches!(
            diagnostics[0],
            ConfigDiagnostic::ParseFailed { .. }
        ));
        assert_eq!(diagnostics[0].severity(), Severity::Error);
    }

    #[test]
    fn test_parse_config_invalid_value_wrong_type() {
        // `disabled_tools` expects an array; a string should be
        // reported as InvalidValue, not coerced.
        let toml_str = r#"
disabled_tools = "bash"
theme = "dark"
"#;
        let (config, diagnostics) = parse_config(toml_str, Path::new("/tmp/config.toml"));
        assert!(config.disabled_tools.is_empty());
        assert_eq!(config.theme.as_deref(), Some("dark"));
        assert_eq!(diagnostics.len(), 1);
        assert!(matches!(
            &diagnostics[0],
            ConfigDiagnostic::InvalidValue { key, .. } if key == "disabled_tools"
        ));
    }

    #[test]
    fn test_config_diagnostic_display_invalid_value() {
        let d = ConfigDiagnostic::InvalidValue {
            path: PathBuf::from("/tmp/config.toml"),
            key: "thinking".to_string(),
            error: "unknown variant `meh`\n".to_string(),
        };
        let s = d.to_string();
        assert!(s.contains("invalid value for `thinking`"));
        assert!(s.contains("ignored"));
        assert!(s.contains("meh"));
        // Trailing newline from the toml error should be trimmed so
        // the message is single-line-friendly.
        assert!(!s.ends_with('\n'));
    }

    #[test]
    fn test_suggest_key() {
        assert_eq!(suggest_key("themee"), Some("theme"));
        assert_eq!(suggest_key("theem"), Some("theme"));
        assert_eq!(suggest_key("disabled_tool"), Some("disabled_tools"));
        // `model` is too short and ambiguous between `model_api`,
        // `model_url`, and `model_name` to suggest any one of them.
        assert_eq!(suggest_key("model"), None);
        // Far enough that no suggestion is more useful than a wrong one.
        assert_eq!(suggest_key("completely_unrelated_setting"), None);
    }

    #[test]
    fn test_options_table_matches_config_fields() {
        // Spot-check that every entry in `Config::OPTIONS` accepts a
        // sensible value for its kind and actually assigns it to the
        // matching field. The values here cover every variant of
        // `ValueKind` we use.
        let toml_str = r#"
model_api = "anthropic"
model_url = "https://example.test"
model_name = "x"
thinking = "low"
thinking_display = "summarized"
speed = "fast"
theme = "dark"
disabled_tools = ["bash"]
hide_thinking_block = true
"#;
        let (config, diagnostics) = parse_config(toml_str, Path::new("/tmp/config.toml"));
        assert!(diagnostics.is_empty(), "got drift: {diagnostics:?}");

        // Every option's apply_toml_fn actually wrote to its field.
        assert_eq!(config.model_api.as_deref(), Some("anthropic"));
        assert_eq!(config.model_url.as_deref(), Some("https://example.test"));
        assert_eq!(config.model_name.as_deref(), Some("x"));
        assert_eq!(config.thinking, Some(ConfigThinkingLevel::Low));
        assert_eq!(
            config.thinking_display,
            Some(ConfigThinkingDisplay::Summarized)
        );
        assert_eq!(config.speed, Some(ConfigSpeed::Fast));
        assert_eq!(config.theme.as_deref(), Some("dark"));
        assert_eq!(config.disabled_tools, vec!["bash".to_string()]);
        assert!(config.hide_thinking_block);
    }

    #[test]
    fn test_config_image_keys_default_and_parse() {
        // Defaults: auto_resize=true, show_in_terminal=true, block=false.
        let cfg = Config::default();
        assert!(cfg.image_auto_resize);
        assert!(cfg.image_show_in_terminal);
        assert!(!cfg.image_block);

        let toml_str = r#"
image_auto_resize = false
image_show_in_terminal = false
image_block = true
"#;
        let (cfg, diag) = parse_config(toml_str, Path::new("/tmp/config.toml"));
        assert!(diag.is_empty(), "got: {diag:?}");
        assert!(!cfg.image_auto_resize);
        assert!(!cfg.image_show_in_terminal);
        assert!(cfg.image_block);
    }

    #[test]
    fn test_options_table_has_no_duplicates() {
        // Sanity-check that no two options share a name; the parser's
        // `find` would silently prefer the first match.
        let mut names: Vec<&str> = Config::OPTIONS.iter().map(|o| o.name).collect();
        names.sort();
        let original_len = names.len();
        names.dedup();
        assert_eq!(
            names.len(),
            original_len,
            "duplicate option name(s) in Config::OPTIONS"
        );
    }

    #[test]
    fn test_config_option_lookup() {
        assert!(Config::option("model_api").is_some());
        assert_eq!(Config::option("model_api").unwrap().name, "model_api");
        assert!(Config::option("nonexistent").is_none());
    }

    #[test]
    fn test_config_option_display_unset_and_set() {
        let mut config = Config::default();
        let theme = Config::option("theme").unwrap();
        assert_eq!(theme.display(&config), "<unset>");

        config.theme = Some("dark".to_string());
        assert_eq!(theme.display(&config), "dark");
    }

    #[test]
    fn test_config_option_display_bool() {
        let mut config = Config::default();
        let opt = Config::option("hide_thinking_block").unwrap();
        assert_eq!(opt.display(&config), "false");
        config.hide_thinking_block = true;
        assert_eq!(opt.display(&config), "true");
    }

    #[test]
    fn test_config_option_display_string_list() {
        let mut config = Config::default();
        let opt = Config::option("disabled_tools").unwrap();
        assert_eq!(opt.display(&config), "<empty>");
        config.disabled_tools = vec!["bash".into(), "grep".into()];
        assert_eq!(opt.display(&config), r#"["bash", "grep"]"#);
    }

    #[test]
    fn test_config_option_display_enum() {
        let mut config = Config::default();
        let opt = Config::option("thinking").unwrap();
        assert_eq!(opt.display(&config), "<unset>");
        config.thinking = Some(ConfigThinkingLevel::Medium);
        assert_eq!(opt.display(&config), "medium");
    }

    #[test]
    fn test_value_kind_display() {
        // Each option's kind renders sensibly for help text.
        for option in Config::OPTIONS {
            let rendered = option.kind.to_string();
            assert!(!rendered.is_empty(), "empty kind for {}", option.name);
        }
        assert_eq!(ValueKind::String.to_string(), "string");
        assert_eq!(ValueKind::Bool.to_string(), "bool");
        assert_eq!(ValueKind::StringList.to_string(), "list of strings");
        assert_eq!(ValueKind::Enum(&["a", "b", "c"]).to_string(), "a | b | c");
    }

    #[test]
    fn test_config_diagnostic_display_unknown_key() {
        let d = ConfigDiagnostic::UnknownKey {
            path: PathBuf::from("/tmp/config.toml"),
            key: "themee".to_string(),
            suggestion: Some("theme"),
        };
        let s = d.to_string();
        assert!(s.contains("unknown key `themee`"));
        assert!(s.contains("did you mean `theme`"));
    }

    #[test]
    fn test_config_diagnostic_display_parse_failed() {
        let d = ConfigDiagnostic::ParseFailed {
            path: PathBuf::from("/tmp/config.toml"),
            error: "bad variant".to_string(),
        };
        let s = d.to_string();
        assert!(s.contains("failed to parse"));
        assert!(s.contains("bad variant"));
        assert!(s.contains("using built-in defaults"));
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
