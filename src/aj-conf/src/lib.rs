use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// The existing `config.toml` could not be parsed while preparing
    /// to write an update, so [`Config::persist_changed`] refused to
    /// clobber it. Carries the `toml_edit` parse error verbatim.
    #[error("cannot update config.toml (existing file is not valid TOML): {0}")]
    Update(String),
    /// Timed out waiting for the `config.toml` write lock — another
    /// process held it for longer than [`LOCK_ACQUIRE_TIMEOUT`].
    #[error("timed out acquiring the config.toml lock (another process may be writing it)")]
    LockTimeout,
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
    /// Serialize the field's current value into a `toml_edit::Item`
    /// for [`Config::persist_changed`], or `None` when the field holds
    /// its default/unset value. A `None` result tells the writer to
    /// drop the key from `config.toml` rather than emit a redundant
    /// at-default line — see [`Config::persist_changed`] for the full
    /// contract.
    to_toml_fn: fn(&Config) -> Option<toml_edit::Item>,
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

    /// Serialize the field's current value for
    /// [`Config::persist_changed`]. `None` means the field is at its
    /// default/unset value and the key should be removed from
    /// `config.toml`.
    pub fn to_toml(&self, config: &Config) -> Option<toml_edit::Item> {
        (self.to_toml_fn)(config)
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

/// `to_toml` helper for `Option<T: Display>` fields: emit the value's
/// canonical string form when set, or `None` (drop the key) when unset.
/// Enum fields rely on their lowercase [`fmt::Display`] matching the
/// parser's accepted spelling so the value round-trips.
fn opt_value_item<T: fmt::Display>(value: &Option<T>) -> Option<toml_edit::Item> {
    value.as_ref().map(|v| toml_edit::value(v.to_string()))
}

/// `to_toml` helper for `Vec<String>` list fields: emit a TOML array
/// when non-empty, or `None` (drop the key) when empty.
fn string_list_item(value: &[String]) -> Option<toml_edit::Item> {
    if value.is_empty() {
        return None;
    }
    let mut array = toml_edit::Array::new();
    for item in value {
        array.push(item.as_str());
    }
    Some(toml_edit::value(array))
}

/// `to_toml` helper for `bool` fields: emit the value only when it
/// differs from `default`, so a config left at its default doesn't
/// accumulate redundant lines. When the value matches `default` the
/// key is dropped from the file.
fn bool_item(value: bool, default: bool) -> Option<toml_edit::Item> {
    (value != default).then(|| toml_edit::value(value))
}

/// Canonical string form of an option's serialized value, or `None`
/// when the option is at its default/unset (its `to_toml` returns
/// `None`). Used to detect whether an option changed between two
/// configs without requiring `toml_edit::Item` to implement equality:
/// both sides come from the same [`ConfigOption::to_toml`] path, so
/// equal values render identically (decoration included).
fn item_value_repr(item: &Option<toml_edit::Item>) -> Option<String> {
    item.as_ref()
        .and_then(|i| i.as_value())
        .map(|v| v.to_string())
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
    /// `~/.aj/themes/`. Defaults to `light` when unset.
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
            to_toml_fn: |c| opt_value_item(&c.model_api),
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
            to_toml_fn: |c| opt_value_item(&c.model_url),
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
            to_toml_fn: |c| opt_value_item(&c.model_name),
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
            to_toml_fn: |c| opt_value_item(&c.thinking),
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
            to_toml_fn: |c| opt_value_item(&c.thinking_display),
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
            to_toml_fn: |c| opt_value_item(&c.speed),
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
            to_toml_fn: |c| opt_value_item(&c.theme),
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
            to_toml_fn: |c| string_list_item(&c.disabled_tools),
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
            to_toml_fn: |c| bool_item(c.hide_thinking_block, false),
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
            to_toml_fn: |c| bool_item(c.image_auto_resize, true),
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
            to_toml_fn: |c| bool_item(c.image_show_in_terminal, true),
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
            to_toml_fn: |c| bool_item(c.image_block, false),
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
    /// paths that actually need those directories (sessions, dotenv)
    /// will surface their own errors.
    pub fn load() -> (Self, Vec<ConfigDiagnostic>) {
        let Ok(config_path) = Self::config_file_path() else {
            return (Config::default(), Vec::new());
        };

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

    /// Path to `~/.aj/config.toml`. Creates the `~/.aj` directory if
    /// it doesn't exist (via [`Self::get_config_dir`]) but does not
    /// create the file itself.
    pub fn config_file_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::get_config_dir()?.join("config.toml"))
    }

    /// Persist the options this process changed to
    /// `~/.aj/config.toml`, merging them onto whatever is currently on
    /// disk so a concurrent writer isn't clobbered.
    ///
    /// `baseline` is the configuration as it was before the caller's
    /// in-memory mutation. Only the options whose value differs between
    /// `self` and `baseline` are written; every other key is left
    /// exactly as it appears in the file, so a second `aj` instance
    /// that changed a *different* key keeps its write instead of the
    /// last writer winning. The whole read-modify-write runs under a
    /// cross-process lock ([`ConfigLock`]).
    ///
    /// The write round-trips through `toml_edit`, so existing comments,
    /// key ordering, and surrounding whitespace are preserved. For a
    /// changed option, a `Some` from [`ConfigOption::to_toml`] sets (or
    /// updates in place, keeping its leading comment) the key, and a
    /// `None` — the value is back at its default — removes it. An
    /// existing file that isn't valid TOML is refused with
    /// [`ConfigError::Update`] rather than overwritten; a missing file
    /// is treated as empty and created on write.
    pub fn persist_changed(&self, baseline: &Config) -> Result<(), ConfigError> {
        let path = Self::config_file_path()?;
        self.persist_changed_at(baseline, &path)
    }

    /// [`Self::persist_changed`] against an explicit path, so the lock
    /// + merge can be exercised against a scratch file without touching
    /// `~/.aj`.
    fn persist_changed_at(&self, baseline: &Config, path: &Path) -> Result<(), ConfigError> {
        let _lock = ConfigLock::acquire(path)?;

        let existing = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(ConfigError::Io(e)),
        };

        let mut doc = existing
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| ConfigError::Update(e.to_string()))?;

        self.apply_changed_into_document(baseline, &mut doc);

        fs::write(path, doc.to_string())?;
        Ok(())
    }

    /// Apply into `doc` only the options whose serialized value differs
    /// between `self` and `baseline`, leaving every unchanged option's
    /// key exactly as it appears in `doc`. This is the merge that keeps
    /// a concurrent writer's edits: `doc` is parsed from the current
    /// file, and we touch only what this process actually changed.
    ///
    /// For a changed option, a `Some` value sets the key (updating in
    /// place to preserve a leading comment) and a `None` removes it —
    /// the same set/remove rule [`ConfigOption::to_toml`] documents.
    fn apply_changed_into_document(&self, baseline: &Config, doc: &mut toml_edit::DocumentMut) {
        for option in Self::OPTIONS {
            let new_item = option.to_toml(self);
            if item_value_repr(&new_item) == item_value_repr(&option.to_toml(baseline)) {
                continue;
            }
            match new_item {
                Some(item) => doc[option.name] = item,
                None => {
                    doc.remove(option.name);
                }
            }
        }
    }

    pub fn get_dotenv_file_path() -> Result<PathBuf, ConfigError> {
        let aj_dir = Self::get_config_dir()?;
        Ok(aj_dir.join(".env"))
    }

    /// Get the sessions directory path for the current project. The sessions
    /// are stored in subdirectories based on the git root directory. For
    /// example, if the git root is /Users/user/Dev/project, the subdirectory
    /// name will be "Dev-project".
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
    /// one project (named via [`path_to_dir_name`]); the prompt-history
    /// "all workspaces" search walks these. Unlike
    /// [`Self::get_sessions_dir_path`] this does not create or descend
    /// into a per-project directory \u2014 it just resolves the base path.
    pub fn get_sessions_base_dir_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::get_config_dir()?.join("sessions"))
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

// ---------------------------------------------------------------------------
// Cross-process config lock
// ---------------------------------------------------------------------------

/// Initial backoff between lock-acquisition retries; doubles each
/// attempt up to [`LOCK_MAX_BACKOFF`].
const LOCK_INITIAL_BACKOFF: Duration = Duration::from_millis(20);
const LOCK_MAX_BACKOFF: Duration = Duration::from_millis(250);
/// Give up acquiring the lock after this long and report
/// [`ConfigError::LockTimeout`]. A config write is a tiny
/// read-modify-write, so genuine contention clears in milliseconds;
/// this ceiling only bounds the pathological "another writer is
/// wedged" case (a crashed holder is reclaimed sooner via
/// [`LOCK_STALE_AGE`]). Kept short because the interactive loop calls
/// the writer synchronously.
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(3);
/// A lock directory whose mtime is older than this is assumed
/// abandoned by a crashed writer and stolen.
const LOCK_STALE_AGE: Duration = Duration::from_secs(60);

/// Sidecar lock for `config.toml`: an empty directory next to the file
/// (`config.toml.lock`). `create_dir` is atomic on every supported OS,
/// so its `AlreadyExists` error is the natural "already locked" signal.
/// Held across a read-modify-write so two processes editing the config
/// can't interleave and clobber each other.
///
/// Released on `Drop`; a process that aborts before `Drop` runs leaves
/// the directory behind, which the next acquirer reclaims once it's
/// older than [`LOCK_STALE_AGE`].
///
/// The same sidecar-directory scheme guards `auth.json`; this is its
/// synchronous twin, since the interactive loop persists config with no
/// async runtime in scope.
struct ConfigLock {
    path: PathBuf,
}

impl ConfigLock {
    /// Acquire the lock for `target_path`, retrying with exponential
    /// backoff up to [`LOCK_ACQUIRE_TIMEOUT`]. Returns
    /// [`ConfigError::LockTimeout`] if a live sibling holds it the whole
    /// time.
    fn acquire(target_path: &Path) -> Result<Self, ConfigError> {
        let lock_path = lock_path_for(target_path);

        // Make sure the parent exists so `create_dir(lock_path)` has
        // somewhere to land.
        if let Some(parent) = lock_path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }

        let start = std::time::Instant::now();
        let mut backoff = LOCK_INITIAL_BACKOFF;
        loop {
            match fs::create_dir(&lock_path) {
                Ok(()) => return Ok(Self { path: lock_path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if try_steal_stale_lock(&lock_path, LOCK_STALE_AGE) {
                        // Stole an abandoned lock; retry immediately. A
                        // racing acquirer that beat us just re-enters the
                        // backoff path next iteration.
                        continue;
                    }
                    if start.elapsed() > LOCK_ACQUIRE_TIMEOUT {
                        return Err(ConfigError::LockTimeout);
                    }
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(LOCK_MAX_BACKOFF);
                }
                Err(e) => return Err(ConfigError::Io(e)),
            }
        }
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        // Best-effort cleanup. The lock path is a directory we created,
        // so `remove_dir` succeeds unless something already tore it down.
        let _ = fs::remove_dir(&self.path);
    }
}

/// `config.toml` → `config.toml.lock` next to it.
fn lock_path_for(file_path: &Path) -> PathBuf {
    let parent = file_path.parent().unwrap_or_else(|| Path::new("."));
    let name = match file_path.file_name() {
        Some(n) => format!("{}.lock", n.to_string_lossy()),
        None => "config.lock".to_string(),
    };
    parent.join(name)
}

/// Remove `lock_path` if it exists and its mtime is older than
/// `max_age`, signalling the holder likely crashed. Returns `true` only
/// when it actually removed the directory, so the caller can retry. Any
/// I/O error is swallowed — worst case the caller backs off and times
/// out. `max_age` is a parameter so tests can drive the steal path with
/// a tiny threshold.
fn try_steal_stale_lock(lock_path: &Path, max_age: Duration) -> bool {
    let Ok(meta) = fs::metadata(lock_path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = modified.elapsed() else {
        return false;
    };
    if age <= max_age {
        return false;
    }
    fs::remove_dir(lock_path).is_ok()
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

    /// Apply the options that changed between `baseline` and `config`
    /// onto `existing` config-file text via the same merge
    /// [`Config::persist_changed`] uses, returning the rewritten text.
    /// Lets the round-trip be asserted without touching `~/.aj`.
    fn rewrite_changed(existing: &str, baseline: &Config, config: &Config) -> String {
        let mut doc = existing.parse::<toml_edit::DocumentMut>().unwrap();
        config.apply_changed_into_document(baseline, &mut doc);
        doc.to_string()
    }

    #[test]
    fn test_save_updates_value_and_preserves_comment() {
        let existing = "\
# Pick the reasoning effort.
thinking = \"low\"
";
        let mut config = Config::default();
        config.thinking = Some(ConfigThinkingLevel::High);

        let rewritten = rewrite_changed(existing, &Config::default(), &config);
        assert!(
            rewritten.contains("# Pick the reasoning effort."),
            "leading comment should survive: {rewritten:?}"
        );
        assert!(
            rewritten.contains("thinking = \"high\""),
            "got: {rewritten:?}"
        );

        // And it parses back to the value we wrote.
        let (parsed, diag) = parse_config(&rewritten, Path::new("/tmp/config.toml"));
        assert!(diag.is_empty(), "got: {diag:?}");
        assert_eq!(parsed.thinking, Some(ConfigThinkingLevel::High));
    }

    #[test]
    fn test_save_adds_missing_keys() {
        let mut config = Config::default();
        config.model_api = Some("anthropic".to_string());
        config.model_name = Some("claude-x".to_string());

        let rewritten = rewrite_changed("", &Config::default(), &config);
        let (parsed, diag) = parse_config(&rewritten, Path::new("/tmp/config.toml"));
        assert!(diag.is_empty(), "got: {diag:?}");
        assert_eq!(parsed.model_api.as_deref(), Some("anthropic"));
        assert_eq!(parsed.model_name.as_deref(), Some("claude-x"));
    }

    #[test]
    fn test_save_removes_key_reverted_to_default() {
        // Reverting an option to its default removes its key. The
        // baseline carries the prior non-default values; `config` is
        // back at defaults, so the merge drops each key it owns.
        let existing = "\
thinking = \"low\"
disabled_tools = [\"bash\"]
image_block = true
";
        let mut baseline = Config::default();
        baseline.thinking = Some(ConfigThinkingLevel::Low);
        baseline.disabled_tools = vec!["bash".to_string()];
        baseline.image_block = true;
        let config = Config::default();
        let rewritten = rewrite_changed(existing, &baseline, &config);

        assert!(!rewritten.contains("thinking"), "got: {rewritten:?}");
        assert!(!rewritten.contains("disabled_tools"), "got: {rewritten:?}");
        assert!(!rewritten.contains("image_block"), "got: {rewritten:?}");
    }

    #[test]
    fn test_save_omits_default_bools() {
        // A pristine default config writes nothing — defaults never
        // accumulate redundant lines in a fresh file.
        let rewritten = rewrite_changed("", &Config::default(), &Config::default());
        assert_eq!(rewritten.trim(), "", "got: {rewritten:?}");
    }

    #[test]
    fn test_save_writes_nondefault_bool() {
        let mut config = Config::default();
        // image_auto_resize defaults to true; flipping it off should persist.
        config.image_auto_resize = false;
        let rewritten = rewrite_changed("", &Config::default(), &config);
        let (parsed, diag) = parse_config(&rewritten, Path::new("/tmp/config.toml"));
        assert!(diag.is_empty(), "got: {diag:?}");
        assert!(!parsed.image_auto_resize);
    }

    #[test]
    fn test_save_full_round_trip() {
        // Every option set to a non-default value must survive a
        // serialize → parse cycle unchanged.
        let mut config = Config::default();
        config.model_api = Some("openai".to_string());
        config.model_url = Some("https://example.test".to_string());
        config.model_name = Some("gpt-x".to_string());
        config.thinking = Some(ConfigThinkingLevel::Max);
        config.thinking_display = Some(ConfigThinkingDisplay::Detailed);
        config.speed = Some(ConfigSpeed::Fast);
        config.theme = Some("light".to_string());
        config.disabled_tools = vec!["bash".to_string(), "todo_read".to_string()];
        config.hide_thinking_block = true;
        config.image_auto_resize = false;
        config.image_show_in_terminal = false;
        config.image_block = true;

        let rewritten = rewrite_changed("", &Config::default(), &config);
        let (parsed, diag) = parse_config(&rewritten, Path::new("/tmp/config.toml"));
        assert!(diag.is_empty(), "got: {diag:?}");

        assert_eq!(parsed.model_api.as_deref(), Some("openai"));
        assert_eq!(parsed.model_url.as_deref(), Some("https://example.test"));
        assert_eq!(parsed.model_name.as_deref(), Some("gpt-x"));
        assert_eq!(parsed.thinking, Some(ConfigThinkingLevel::Max));
        assert_eq!(
            parsed.thinking_display,
            Some(ConfigThinkingDisplay::Detailed)
        );
        assert_eq!(parsed.speed, Some(ConfigSpeed::Fast));
        assert_eq!(parsed.theme.as_deref(), Some("light"));
        assert_eq!(parsed.disabled_tools, vec!["bash", "todo_read"]);
        assert!(parsed.hide_thinking_block);
        assert!(!parsed.image_auto_resize);
        assert!(!parsed.image_show_in_terminal);
        assert!(parsed.image_block);
    }

    // ---- persist_changed (lock + merge) ----------------------------------

    #[test]
    fn persist_changed_does_not_clobber_a_concurrent_writers_key() {
        // Simulate a second process having written `model_api`. This
        // process only changed `theme`, so the merge must keep both.
        let dir = make_temp_dir("persist-no-clobber");
        let path = dir.join("config.toml");
        fs::write(&path, "model_api = \"anthropic\"\n").unwrap();

        let baseline = Config::default();
        let mut config = Config::default();
        config.theme = Some("dark".to_string());
        config
            .persist_changed_at(&baseline, &path)
            .expect("persist");

        let written = fs::read_to_string(&path).unwrap();
        let (parsed, diag) = parse_config(&written, &path);
        assert!(diag.is_empty(), "got: {diag:?}");
        assert_eq!(
            parsed.model_api.as_deref(),
            Some("anthropic"),
            "concurrent writer's key was clobbered: {written:?}"
        );
        assert_eq!(parsed.theme.as_deref(), Some("dark"), "got: {written:?}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persist_changed_refuses_to_clobber_invalid_toml() {
        let dir = make_temp_dir("persist-invalid");
        let path = dir.join("config.toml");
        let invalid = "this is = = not valid toml\n";
        fs::write(&path, invalid).unwrap();

        let mut config = Config::default();
        config.theme = Some("dark".to_string());
        let err = config
            .persist_changed_at(&Config::default(), &path)
            .expect_err("must refuse invalid TOML");
        assert!(matches!(err, ConfigError::Update(_)), "got: {err:?}");

        // The original file is left untouched.
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persist_changed_creates_a_missing_file() {
        let dir = make_temp_dir("persist-missing");
        let path = dir.join("config.toml");
        assert!(!path.exists());

        let mut config = Config::default();
        config.model_name = Some("gpt-x".to_string());
        config
            .persist_changed_at(&Config::default(), &path)
            .expect("persist");

        let (parsed, diag) = parse_config(&fs::read_to_string(&path).unwrap(), &path);
        assert!(diag.is_empty(), "got: {diag:?}");
        assert_eq!(parsed.model_name.as_deref(), Some("gpt-x"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persist_changed_preserves_comments_across_an_update() {
        let dir = make_temp_dir("persist-comments");
        let path = dir.join("config.toml");
        fs::write(&path, "# keep me\nthinking = \"low\"\n").unwrap();

        let mut baseline = Config::default();
        baseline.thinking = Some(ConfigThinkingLevel::Low);
        let mut config = Config::default();
        config.thinking = Some(ConfigThinkingLevel::High);
        config
            .persist_changed_at(&baseline, &path)
            .expect("persist");

        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("# keep me"), "got: {written:?}");
        assert!(written.contains("thinking = \"high\""), "got: {written:?}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_lock_round_trips_and_releases_on_drop() {
        let dir = make_temp_dir("lock-roundtrip");
        let target = dir.join("config.toml");
        let lock_dir = lock_path_for(&target);

        {
            let _lock = ConfigLock::acquire(&target).expect("acquire");
            assert!(lock_dir.exists(), "lock dir should exist while held");
        }
        assert!(!lock_dir.exists(), "lock dir should be gone after drop");

        // A second acquisition succeeds now that the first released.
        let _lock = ConfigLock::acquire(&target).expect("re-acquire");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn try_steal_stale_lock_reclaims_only_old_locks() {
        let dir = make_temp_dir("lock-steal");
        let lock_dir = dir.join("config.toml.lock");
        fs::create_dir(&lock_dir).unwrap();

        // A just-created lock (younger than the threshold) is left alone.
        assert!(!try_steal_stale_lock(&lock_dir, Duration::from_secs(3600)));
        assert!(lock_dir.exists());

        // A zero threshold treats any existing lock as stale and steals it.
        assert!(try_steal_stale_lock(&lock_dir, Duration::from_secs(0)));
        assert!(!lock_dir.exists());

        // Nothing to steal once it's gone.
        assert!(!try_steal_stale_lock(&lock_dir, Duration::from_secs(0)));

        fs::remove_dir_all(&dir).ok();
    }
}
