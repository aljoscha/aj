//! [`clap`]-derived argument parsing for the `aj` binary.
//!
//! The `--print` / `--json` toggles select the non-interactive
//! print mode; otherwise the binary runs the interactive
//! TUI. Subcommands (`list-sessions`, `continue`, `update-models`)
//! short-circuit before mode dispatch.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use aj_conf::ConfigThinkingLevel;
use aj_session::{SessionEnvError, TagError, normalize_tag, validate_session_env};
use aj_wire::{HostNameError, normalize_host_name};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use thiserror::Error;

/// Why repeated `--env KEY=VALUE` launch arguments are invalid.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LaunchEnvError {
    #[error("environment argument {argument:?} must contain '='")]
    MissingEquals { argument: String },
    #[error("environment key {key:?} was stated more than once")]
    DuplicateKey { key: String },
    #[error(transparent)]
    Invalid(#[from] SessionEnvError),
}

/// Top-level CLI for the `aj` binary.
///
/// Construct through this type's parse methods. They scope shared options to
/// the commands that use them and combine options stated before and after a
/// subcommand. The derived `clap::Args` implementation is only used by the
/// private root parser.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// New-session model API (e.g. `anthropic`, `openai`, `openai-codex`,
    /// `openrouter`).
    #[arg(long, env = "MODEL_API")]
    pub model_api: Option<String>,

    /// Override the new session's model endpoint URL.
    #[arg(long, env = "MODEL_URL")]
    pub model_url: Option<String>,

    /// New-session model name (provider-specific identifier).
    #[arg(long, env = "MODEL_NAME")]
    pub model_name: Option<String>,

    /// API key for a new local session, applied as a runtime
    /// override for this run only. Takes precedence over env vars
    /// and any credential stored in `~/.aj/auth.json`, and is never
    /// written to disk. Intentionally has no `env =` binding so the
    /// only way to supply it is the explicit flag (provider-specific
    /// env vars like `ANTHROPIC_API_KEY` remain the env path).
    #[arg(long, conflicts_with_all = ["account", "default_account"])]
    pub api_key: Option<String>,

    /// Pin the new session's provider to this stored account.
    /// An empty value selects the unnamed account.
    #[arg(long, value_name = "NAME", conflicts_with = "default_account")]
    pub account: Option<String>,

    /// Use the provider's shared default account in the new session.
    #[arg(long, conflicts_with = "account")]
    pub default_account: bool,

    /// New-session shared Main/Oracle inference speed: standard, fast, ultrafast, flex,
    /// or a model-advertised mode. Unsupported models use standard speed.
    #[arg(long, env = "AJ_SPEED")]
    pub speed: Option<String>,

    /// New-session Main thinking effort: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`,
    /// or `max`.
    ///
    /// Local, print, and `connect --new` runs share one spelling and
    /// the same CLI > env > config precedence. Explicit session-setting flags
    /// require a new session. Remote creation requires `connect --new`.
    #[arg(long, env = "AJ_THINKING", value_name = "LEVEL")]
    pub thinking: Option<ConfigThinkingLevel>,

    /// Run in non-interactive print mode: stream events to stdout
    /// and exit when the agent reports `AgentEnd`. The trailing
    /// positional `prompt` is required in this mode.
    #[arg(long)]
    pub print: bool,

    /// Output format for print mode. `text` (default) renders
    /// human-readable lines; `json` writes one JSONL event per
    /// line. Requires `--print` when set explicitly.
    #[arg(long, value_enum, default_value_t = PrintFormat::Text, requires = "print")]
    pub format: PrintFormat,

    /// Free-form launch input. Each positional argument is either a
    /// `@file` attachment (its contents are wrapped in a `<file>` block
    /// and images are attached inline) or a message; the messages are
    /// joined and combined with the file content into a single launch
    /// turn, which both print and interactive mode auto-submit.
    pub prompt: Vec<String>,

    /// Replace the live model with a scripted fake that replays a canned
    /// assistant-event sequence. Useful for eyeballing how the TUI / print
    /// mode renders thinking blocks, tool calls, errors, and the like,
    /// without spending a real API round-trip.
    ///
    /// The argument is the demo name. Pass `--scripted help` (or any
    /// unknown name) to see the catalog. When set the binary skips
    /// registry-driven provider construction entirely and registers a
    /// scripted provider in its place; every other code path (TUI,
    /// persistence, tools, commands) runs unchanged so the eyeball test
    /// exercises the real surface.
    #[arg(long)]
    pub scripted: Option<String>,

    /// Serve this working directory's sessions on a control port, and
    /// accept clients on it. `--listen` alone binds the loopback default
    /// (`127.0.0.1:6161`), which is the only address the `local` identity
    /// mode will serve (see `--auth`).
    ///
    /// An explicit address needs the equals form, `--listen=<addr>`: the
    /// value is optional, and a space-separated one would swallow the first
    /// word of `aj --listen fix the parser`.
    ///
    /// Interactive runs embed the server alongside the TUI, so the local
    /// shell and every remote client attach to one host as peers. `aj serve`
    /// runs the same host headless.
    #[arg(
        long,
        env = "AJ_LISTEN",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = DEFAULT_LISTEN_ADDRESS,
    )]
    pub listen: Option<String>,

    /// Who may connect to the control port: `local` (default, loopback
    /// peers only), `tailscale` (verify each peer against the local
    /// tailscale daemon), or `open` (accept everyone, for a host-private
    /// network only).
    ///
    /// The control port runs arbitrary commands through the agent, so
    /// serving a non-loopback address in `local` mode refuses to start
    /// rather than serving unauthenticated.
    #[arg(long, env = "AJ_AUTH", default_value = "local")]
    pub auth: String,

    /// Tailnet login allowed to connect in `--auth tailscale` mode,
    /// repeatable. Spelled exactly as the tailscale daemon reports it
    /// (e.g. `alice@github`). A tagged node has no login and is admitted
    /// only by the aj control capability granted in the tailnet policy.
    #[arg(long, env = "AJ_ALLOW", value_delimiter = ',')]
    pub allow: Vec<String>,

    /// Name the session this run creates, shown in place of its id
    /// wherever a session is listed.
    ///
    /// Available locally and on `connect` for sessions created on the host. A run that
    /// resumes an existing session creates nothing for the flag to name, and
    /// says so rather than dropping it.
    #[arg(long)]
    pub tag: Option<String>,

    /// Add one initial environment entry to sessions this invocation creates.
    ///
    /// Repeatable and create-only. The first `=` separates key from value, so
    /// values may contain additional equals signs. Intentionally has no
    /// environment-variable binding: session identity must be stated on the
    /// create rather than inherited from the launching process.
    #[arg(
        long,
        value_name = "KEY=VALUE",
        action = clap::ArgAction::Append
    )]
    pub env: Vec<String>,

    /// Name the host this run serves, shown in place of its id wherever a
    /// client lists hosts.
    ///
    /// Available for local interactive hosts and `aj serve`. A gateway
    /// names the hosts behind it, not itself.
    ///
    /// Absent, the host names itself after its working directory, `~`-
    /// abbreviated under home. That is a fallback: a fleet of clones is
    /// easier to read with names its operator chose.
    #[arg(long, env = "AJ_NAME")]
    pub name: Option<String>,

    /// Subcommand selector for the non-conversational utilities
    /// (`list-sessions`, `continue`, `update-models`) and the
    /// remote-control modes (`serve`, `connect`).
    #[command(subcommand)]
    pub command: Option<Command>,
}

// Options shared with the root launch mode, scoped to their consumers.
const SESSION_OPTIONS: &[&str] = &[
    "model_api",
    "model_url",
    "model_name",
    "account",
    "default_account",
    "speed",
    "thinking",
    "tag",
    "env",
];
const SERVER_OPTIONS: &[&str] = &["listen", "auth", "allow"];
const HOST_OPTIONS: &[&str] = &["listen", "auth", "allow", "name"];
const SCOPED_OPTIONS: &[&str] = &[
    "model_api",
    "model_url",
    "model_name",
    "api_key",
    "account",
    "default_account",
    "speed",
    "thinking",
    "tag",
    "env",
    "listen",
    "auth",
    "allow",
    "name",
];

fn command_options(command: &str) -> &'static [&'static str] {
    match command {
        "connect" => SESSION_OPTIONS,
        "serve" => HOST_OPTIONS,
        "continue" => &["listen", "auth", "allow", "name", "tag", "env"],
        "gateway" => SERVER_OPTIONS,
        _ => &[],
    }
}

#[derive(Parser, Debug)]
#[command(name = "aj")]
#[command(about = "AI-driven agent for software engineering")]
#[command(long_about = None)]
struct CliParser {
    #[command(flatten)]
    args: Args,
}

impl Args {
    /// Parse the process command line while preserving every `--env` and
    /// `--allow` occurrence across subcommand boundaries.
    pub fn parse() -> Self {
        Self::parse_from(std::env::args_os())
    }

    /// Parse `argv`, exiting with clap's normal diagnostic on failure.
    pub fn parse_from<I, T>(argv: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        Self::try_parse_from(argv).unwrap_or_else(|err| err.exit())
    }

    /// Parse `argv` while preserving every `--env` and `--allow`
    /// occurrence across subcommand boundaries.
    ///
    /// Shared options are matched independently at the root and subcommand.
    /// Explicit child values override root defaults. Repeatable options retain
    /// all explicit occurrences in command-line order.
    pub fn try_parse_from<I, T>(argv: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let argv = argv.into_iter().map(Into::into).collect::<Vec<_>>();
        let matches = Self::command().try_get_matches_from(argv)?;
        let mut parsed = CliParser::from_arg_matches(&matches)?.args;
        // Root options remain accepted before a subcommand, but only when that
        // command uses them. Child CLI values override root defaults, not vice versa.
        if let Some((command, child)) = matches.subcommand() {
            for &id in SCOPED_OPTIONS {
                if !command_options(command).contains(&id)
                    && matches.value_source(id) == Some(clap::parser::ValueSource::CommandLine)
                {
                    return Err(clap::Error::raw(
                        clap::error::ErrorKind::ArgumentConflict,
                        format!("--{} does not apply to {command}", id.replace('_', "-")),
                    ));
                }
            }
            macro_rules! take_child {
                ($($field:ident: $ty:ty),* $(,)?) => {
                    $(
                        if child.try_contains_id(stringify!($field)).unwrap_or(false)
                            && child.value_source(stringify!($field))
                                == Some(clap::parser::ValueSource::CommandLine)
                        {
                            parsed.$field = child.get_one::<$ty>(stringify!($field)).cloned();
                        }
                    )*
                };
            }
            take_child!(
                model_api: String, model_url: String, model_name: String,
                account: String, speed: String, thinking: ConfigThinkingLevel,
                listen: String, tag: String, name: String,
            );
            if child.try_contains_id("default_account").unwrap_or(false)
                && child.value_source("default_account")
                    == Some(clap::parser::ValueSource::CommandLine)
            {
                parsed.default_account = child.get_flag("default_account");
            }
            if child.try_contains_id("auth").unwrap_or(false)
                && child.value_source("auth") == Some(clap::parser::ValueSource::CommandLine)
            {
                parsed.auth = child.get_one::<String>("auth").unwrap().clone();
            }
        }
        // Environment bindings remain defaults even on commands that cannot
        // accept an explicit session choice. ArgMatches retains the provenance
        // that the typed fields no longer distinguish.
        for id in [
            "model_api",
            "model_name",
            "model_url",
            "thinking",
            "speed",
            "account",
            "default_account",
            "api_key",
        ] {
            if matches.value_source(id) != Some(clap::parser::ValueSource::CommandLine)
                && !matches.subcommand().is_some_and(|(_, child)| {
                    child.try_contains_id(id).unwrap_or(false)
                        && child.value_source(id) == Some(clap::parser::ValueSource::CommandLine)
                })
            {
                continue;
            }
            let flag = id.replace('_', "-");
            let message = match &parsed.command {
                None => continue,
                Some(Command::Connect { new: true, .. }) if id != "api_key" => continue,
                Some(Command::Connect { .. }) if id == "api_key" => format!(
                    "--{flag} is only supported for new local sessions. Configure credentials on the host instead."
                ),
                Some(Command::Connect { .. }) => format!(
                    "--{flag} requires --new when connecting. To change an existing session, use interactive settings."
                ),
                _ => format!(
                    "--{flag} only applies when creating a session. Use interactive settings to change an existing session, or config to set host defaults."
                ),
            };
            return Err(clap::Error::raw(
                clap::error::ErrorKind::ArgumentConflict,
                message,
            ));
        }
        parsed.env = explicit_arguments(&matches, "env").unwrap_or_default();
        if let Some(allow) = explicit_arguments(&matches, "allow") {
            parsed.allow = allow;
        }
        // Clap checks conflicts within each command. These flags can arrive
        // on opposite sides of a subcommand, so validate the complete choice.
        if (parsed.account.is_some() && parsed.default_account)
            || (parsed.api_key.is_some() && (parsed.account.is_some() || parsed.default_account))
        {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::ArgumentConflict,
                "--account, --default-account, and --api-key cannot be combined",
            ));
        }
        Ok(parsed)
    }

    /// An explicit new-session account choice. `--default-account` selects
    /// the provider default rather than a named account.
    pub fn account_selection(&self) -> Option<aj_wire::AccountSelection> {
        if self.default_account {
            Some(aj_wire::AccountSelection { name: None })
        } else {
            self.account
                .clone()
                .map(|name| aj_wire::AccountSelection { name: Some(name) })
        }
    }

    /// Build the clap command for help, completion, and grammar introspection.
    ///
    /// An [`Args`] value is constructed only by this type's parse methods.
    /// Materializing one from this command's `ArgMatches` would bypass the
    /// normalization described by [`Self::try_parse_from`].
    pub fn command() -> clap::Command {
        let mut command = CliParser::command();
        let options = command.get_arguments().cloned().collect::<Vec<_>>();
        for name in ["continue", "serve", "connect", "gateway"] {
            command = command.mut_subcommand(name, |child| {
                let offset = child.get_arguments().count();
                child.args(
                    options
                        .iter()
                        .filter(|arg| command_options(name).contains(&arg.get_id().as_str()))
                        .enumerate()
                        .map(|(index, arg)| arg.clone().display_order(offset + index)),
                )
            });
        }
        command
    }

    /// The validated `--tag` value for the session this run creates.
    ///
    /// `Ok(None)` covers both "no flag" and a flag whose value normalizes to
    /// nothing, which is the same "no label" a blank tag means everywhere
    /// else. Validating here is what keeps the CLI and the wire agreeing on
    /// what a legal tag is: a label a host would refuse never reaches a
    /// session, and the refusal reads on the normal screen.
    pub fn launch_tag(&self) -> Result<Option<String>, TagError> {
        match &self.tag {
            Some(tag) => normalize_tag(tag),
            None => Ok(None),
        }
    }

    /// The complete validated environment map for each create this run makes.
    ///
    /// `None` means the flag was not stated and differs from an explicitly
    /// supplied empty map at non-CLI create boundaries. Validation remains
    /// exact and case-sensitive because a connected host may execute under a
    /// different process environment than this client.
    pub fn launch_env(&self) -> Result<Option<BTreeMap<String, String>>, LaunchEnvError> {
        if self.env.is_empty() {
            return Ok(None);
        }
        let mut env = BTreeMap::new();
        for argument in &self.env {
            let Some((key, value)) = argument.split_once('=') else {
                return Err(LaunchEnvError::MissingEquals {
                    argument: argument.clone(),
                });
            };
            if env.contains_key(key) {
                return Err(LaunchEnvError::DuplicateKey {
                    key: key.to_string(),
                });
            }
            env.insert(key.to_string(), value.to_string());
        }
        validate_session_env(&env)?;
        Ok(Some(env))
    }

    /// The validated `--name` value for the host this run serves.
    ///
    /// `Ok(None)` covers both "no flag" and a flag whose value names nothing,
    /// which is what leaves the host to derive a name from its working
    /// directory. Validated here for the reason [`Self::launch_tag`] is: a
    /// name a peer would refuse to render never reaches the wire, and the
    /// refusal reads before anything is served.
    pub fn host_name(&self) -> Result<Option<String>, HostNameError> {
        match &self.name {
            Some(name) => normalize_host_name(name),
            None => Ok(None),
        }
    }

    /// The host `--host` named, for the session a `connect` run creates.
    ///
    /// Subcommand-scoped: a create through a
    /// gateway is the only thing it can point at, and `connect` is the only
    /// mode that reaches one.
    pub fn connect_host(&self) -> Option<&str> {
        self.connect_launch().and_then(|launch| launch.host())
    }

    /// Whether `--tag` carries a label for this run to give a session.
    ///
    /// The normalized answer, so a flag that names nothing reads as no flag,
    /// which is what makes [`TAG_WITHOUT_A_CREATE`] a report about a label
    /// that exists. An illegal one answers `false` here and is reported by
    /// [`Self::launch_tag`], which every mode calls before anything is minted.
    pub fn has_launch_tag(&self) -> bool {
        matches!(self.launch_tag(), Ok(Some(_)))
    }

    /// Whether this invocation stated at least one create-only env argument.
    pub fn has_launch_env(&self) -> bool {
        !self.env.is_empty()
    }

    /// What a `connect` run asks for, or `None` for every other command.
    ///
    /// The one interpretation of the connect grammar: the session to open, the
    /// host a create names, and the launch input all come from here, so no two
    /// readers can disagree about which positional is which.
    pub fn connect_launch(&self) -> Option<ConnectLaunch<'_>> {
        let Some(Command::Connect {
            url,
            session_id,
            new,
            host,
            prompt,
        }) = &self.command
        else {
            return None;
        };
        // Clap fills the id slot before the prompt slot, so under `--new`,
        // which names no session, that slot holds the launch input's first
        // word.
        let (session, leading) = match (*new, session_id.as_deref()) {
            (true, first) => (ConnectSession::Fresh, first),
            (false, Some(id)) => (ConnectSession::Named(id), None),
            (false, None) => (ConnectSession::Latest, None),
        };
        Some(ConnectLaunch {
            url,
            session,
            host: host.as_deref(),
            prompt: leading
                .into_iter()
                .chain(prompt.iter().map(String::as_str))
                .collect(),
        })
    }

    /// The launch turn's positionals, in argv order, from whichever slot clap
    /// filled: top-level `aj <args...>`, `aj continue ID <args...>`, or
    /// `aj connect URL [ID] <args...>`.
    ///
    /// A subcommand's own slot answers when it holds anything, else the
    /// top-level one does. Both can be filled at once: a bare positional
    /// before a subcommand swallows the subcommand name, but a flag between
    /// them re-opens subcommand matching, so `aj "do this" --tag t connect URL`
    /// carries its turn in the top-level slot.
    pub fn launch_positionals(&self) -> Vec<&str> {
        let subcommand = match (self.connect_launch(), &self.command) {
            (Some(launch), _) => launch.prompt,
            (None, Some(Command::Continue { prompt, .. })) => {
                prompt.iter().map(String::as_str).collect()
            }
            (None, _) => Vec::new(),
        };
        if subcommand.is_empty() {
            return self.prompt.iter().map(String::as_str).collect();
        }
        subcommand
    }
}

/// Preserve explicit repeatable values across the root and subcommand.
/// Environment defaults apply only when neither scope supplied CLI values.
fn explicit_arguments(matches: &clap::ArgMatches, id: &str) -> Option<Vec<String>> {
    let scopes = std::iter::once(matches).chain(matches.subcommand().map(|(_, child)| child));
    let mut values = None;
    for scope in scopes {
        if scope.try_contains_id(id).unwrap_or(false)
            && scope.value_source(id) == Some(clap::parser::ValueSource::CommandLine)
        {
            values
                .get_or_insert_with(Vec::new)
                .extend(scope.get_many::<String>(id).into_iter().flatten().cloned());
        }
    }
    values
}

/// What a `connect` run's command line asks for.
///
/// Obtained only from [`Args::connect_launch`], which is what makes it the
/// answer to "what did this run ask for" rather than one of several: a caller
/// in another crate cannot assemble one that says something else.
#[derive(Debug)]
pub struct ConnectLaunch<'a> {
    url: &'a str,
    session: ConnectSession<'a>,
    host: Option<&'a str>,
    prompt: Vec<&'a str>,
}

impl<'a> ConnectLaunch<'a> {
    /// Base url of the peer's control port.
    pub fn url(&self) -> &'a str {
        self.url
    }

    /// The session to open with.
    pub fn session(&self) -> ConnectSession<'a> {
        self.session
    }

    /// The peer's host `--host` named, for a session this run creates.
    pub fn host(&self) -> Option<&'a str> {
        self.host
    }
}

/// The session a `connect` run asks for.
///
/// One value for three states rather than an id and a flag that can both
/// speak: a run that creates names no session, so it has no id to overrule
/// and no reader has to rank them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectSession<'a> {
    /// The id named on the command line, attached whatever its archived bit
    /// says.
    Named(&'a str),
    /// `--new`: a session this run creates, whatever the host already holds.
    Fresh,
    /// Nothing named: the host's most recently modified session that is not
    /// archived, and a fresh one when it holds none.
    Latest,
}

/// What a run says when `--tag` named a session it never created.
///
/// The flag labels the session a run mints. A resumed session already carries
/// whatever label it was given, and relabelling it from the launch line would
/// be a second meaning for one flag, so the run reports instead.
pub const TAG_WITHOUT_A_CREATE: &str = "--tag has nothing to name: this run resumed a session rather than creating one. \
     Use the session-tag command to relabel it.";

/// What a run says when `--host` named a host for a session it never created.
///
/// The flag names where a session is minted. A run that attached one instead is
/// looking at a session that already lives on a host, and moving it is not
/// something the flag or anything else can do.
pub const HOST_WITHOUT_A_CREATE: &str = "--host has nothing to point at: this run attached an existing session rather than \
     creating one, and a session stays on the host that holds it.";

/// What a run says when `--env` was armed but its primary gesture resumed.
pub const ENV_WITHOUT_A_CREATE: &str = "--env applies only to sessions this run creates. The resumed or attached session keeps the environment recorded in its own log.";

/// Control-port address a bare `--listen` binds: loopback, because the
/// port is remote code execution and the identity gate's default mode
/// trusts nothing else.
pub const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1:6161";

/// Output formats supported by print mode.
#[derive(ValueEnum, Copy, Clone, Eq, PartialEq, Debug, Default)]
#[value(rename_all = "lowercase")]
pub enum PrintFormat {
    /// Human-readable text — same look as the interactive mode's
    /// scrollback, minus colour and progressive updates.
    #[default]
    Text,
    /// One JSONL agent event per line. Stable shape suitable for piping
    /// into another process.
    Json,
}

/// Non-conversational subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// List existing conversation sessions for this project.
    ListSessions,
    /// Continue a conversation session (latest unarchived one if no id
    /// given).
    ///
    /// Accepts optional positional launch input after the session id:
    /// `aj continue ID <args...>` resumes the session and auto-submits
    /// the args (messages and `@file` attachments) as the next turn.
    /// With no session id, the latest session for the current project
    /// that is not archived is resumed; supplying input without a session
    /// id is ambiguous, so callers wanting "latest + prompt" should pass
    /// the session id explicitly (e.g. via `aj list-sessions`).
    Continue {
        /// Conversation ID to continue. If absent, the latest session for
        /// this project that is not archived is resumed. Naming one works
        /// whatever its archived bit says, and `aj list-sessions` shows
        /// archived rows, so its first row can differ from the unnamed
        /// pick.
        session_id: Option<String>,
        /// Launch input for the resumed run, interpreted exactly like the
        /// top-level launch input: a mix of `@file` attachments and
        /// messages, auto-submitted as the next turn.
        prompt: Vec<String>,
    },
    /// Refresh the user model catalog at `~/.aj/models.json` from
    /// `https://models.dev/api.json`.
    UpdateModels,
    /// Serve this working directory's sessions headlessly on the control
    /// port, with no terminal UI of its own.
    ///
    /// The address comes from `--listen` / `AJ_LISTEN`, which may be given on either side of the subcommand. It defaults to the
    /// loopback control port when neither is given, so a bare `aj serve` is
    /// reachable by `aj connect` on the same machine.
    Serve,
    /// Attach the interactive TUI to a session on a remote aj host.
    ///
    /// With no session id the host's most recently modified session is
    /// attached, and one is created when the host has none.
    ///
    /// Launch input follows the session id, exactly as for `continue`:
    /// `aj connect URL ID <args...>` attaches and auto-submits the args as
    /// the next turn. Without an id the first positional is read as one, so a
    /// run that resumes has to name its session to carry input. `--new` names
    /// no session, so under it every positional is launch input:
    /// `aj connect URL --new <args...>`.
    Connect {
        /// Base URL of the host's control port (e.g.
        /// `http://100.64.0.2:6161`).
        url: String,
        /// Session to attach. Omit to take the host's latest session that is
        /// not archived. Naming a listed one works whatever its archived bit
        /// says.
        ///
        /// Under `--new` this is launch input rather than an id, because a run
        /// that creates its session has none to name.
        session_id: Option<String>,
        /// Create a fresh session instead of attaching an existing one.
        #[arg(long)]
        new: bool,
        /// Which of the peer's hosts a created session is for, needed when a
        /// gateway has more than one enrolled and there is no terminal to ask.
        ///
        /// Names a host by its id, or by any prefix of an id that only one
        /// host answers to. A value that matches none of them, or several, is
        /// refused with the candidates listed rather than resolved to a guess:
        /// a create runs an agent in a working directory. Against a plain host
        /// the only value it may name is that host's own id, which is what the
        /// host itself accepts.
        ///
        /// A create is all it can point at, and it is resolved on every run
        /// that carries it, so a stale or misspelled value is refused rather
        /// than dropped. A run that ends up attaching an existing session
        /// reports that the flag had nothing to point at, as `--tag` does.
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Launch input for the attached session, interpreted exactly like
        /// the top-level launch input: a mix of `@file` attachments and
        /// messages, auto-submitted as the next turn.
        prompt: Vec<String>,
    },
    /// Aggregate many session hosts behind one address.
    ///
    /// A gateway serves the session-facing API of a host, forwarding every
    /// request to the host that owns the session, and adds the host-management
    /// endpoints. It holds no sessions of its own.
    ///
    /// The address comes from `--listen` / `AJ_LISTEN`, which may be given on either side of the subcommand, and defaults to the
    /// loopback control port.
    Gateway {
        /// Read static host addresses from this file instead of
        /// `~/.aj/gateway.toml`.
        ///
        /// A file named here has to exist. The default one need not: a gateway
        /// told about its hosts over the wire needs no configuration file.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subcommand_help_and_parsing_scope_shared_options() {
        for (command, expected) in [
            ("list-sessions", vec![]),
            ("update-models", vec![]),
            (
                "continue",
                vec!["listen", "auth", "allow", "name", "tag", "env"],
            ),
            ("serve", vec!["listen", "auth", "allow", "name"]),
            ("gateway", vec!["listen", "auth", "allow"]),
            (
                "connect",
                vec![
                    "model_api",
                    "model_url",
                    "model_name",
                    "account",
                    "default_account",
                    "speed",
                    "thinking",
                    "tag",
                    "env",
                ],
            ),
        ] {
            let help = Args::try_parse_from(["aj", command, "--help"]).unwrap_err();
            assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
            let help = help.to_string();
            for id in SCOPED_OPTIONS {
                let flag = format!("--{}", id.replace('_', "-"));
                assert_eq!(
                    help.contains(&flag),
                    expected.contains(id),
                    "{command}: {flag}\n{help}"
                );
                if !expected.contains(id) {
                    for before in [true, false] {
                        let option = if *id == "default_account" {
                            flag.clone()
                        } else if *id == "thinking" {
                            format!("{flag}=high")
                        } else {
                            format!("{flag}=unused")
                        };
                        let mut argv = vec!["aj"];
                        if before {
                            argv.push(&option);
                        }
                        argv.push(command);
                        if command == "connect" {
                            argv.push("http://host");
                        }
                        if !before {
                            argv.push(&option);
                        }
                        assert!(Args::try_parse_from(&argv).is_err(), "{argv:?}");
                    }
                }
            }
        }
        Args::command().debug_assert();
    }

    #[test]
    fn scoped_values_keep_explicit_root_choices_and_prefer_explicit_child_choices() {
        let args = parse(&["aj", "--thinking=high", "connect", "http://host", "--new"]);
        assert_eq!(args.thinking, Some(ConfigThinkingLevel::High));
        let args = parse(&[
            "aj",
            "--thinking=high",
            "connect",
            "http://host",
            "--new",
            "--thinking=low",
        ]);
        assert_eq!(args.thinking, Some(ConfigThinkingLevel::Low));
        let args = parse(&["aj", "--auth=open", "serve"]);
        assert_eq!(args.auth, "open");
        let args = parse(&["aj", "--auth=open", "serve", "--auth=tailscale"]);
        assert_eq!(args.auth, "tailscale");
    }

    #[test]
    fn explicit_format_requires_print_mode() {
        let error = Args::try_parse_from(["aj", "--format", "json", "hello"])
            .expect_err("--format without --print must be rejected");

        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn default_format_does_not_require_print_mode() {
        let args = Args::try_parse_from(["aj", "hello"]).expect("interactive arguments parse");

        assert!(!args.print);
        assert_eq!(args.format, PrintFormat::Text);
    }

    fn parse(argv: &[&str]) -> Args {
        Args::try_parse_from(argv).unwrap_or_else(|err| panic!("{argv:?} should parse: {err}"))
    }

    #[test]
    fn session_flags_require_creation_independent_of_subcommand_placement() {
        for flag in [
            "--model-api=openai",
            "--model-name=gpt-5.5",
            "--model-url=https://example.test",
            "--thinking=high",
            "--speed=fast",
            "--account=work",
            "--default-account",
            "--api-key=synthetic",
        ] {
            for command in [
                vec![],
                vec!["--print", "hello"],
                vec!["connect", "http://host", "--new"],
                vec!["connect", "http://host"],
                vec!["connect", "http://host", "id"],
                vec!["continue"],
                vec!["--print", "continue", "id", "prompt"],
                vec!["serve"],
                vec!["gateway"],
                vec!["list-sessions"],
                vec!["update-models"],
            ] {
                let allowed = command.is_empty()
                    || command == ["--print", "hello"]
                    || (command.contains(&"--new") && !flag.starts_with("--api-key"));
                for before in [true, false] {
                    let mut argv = vec!["aj"];
                    if before {
                        argv.push(flag);
                    }
                    argv.extend(command.iter().copied());
                    if !before {
                        argv.push(flag);
                    }
                    let result = Args::try_parse_from(&argv);
                    assert_eq!(result.is_ok(), allowed, "{argv:?}: {result:?}");
                    if let Err(error) = result {
                        assert!(
                            matches!(
                                error.kind(),
                                clap::error::ErrorKind::ArgumentConflict
                                    | clap::error::ErrorKind::UnknownArgument
                            ),
                            "{argv:?}: {error}"
                        );
                        assert!(error.to_string().contains(flag.split('=').next().unwrap()));
                    }
                }
            }
        }
        let resumed = parse(&["aj", "continue", "id", "prompt", "@file"]);
        assert!(
            matches!(resumed.command, Some(Command::Continue { prompt, .. }) if prompt == ["prompt", "@file"])
        );
    }

    #[test]
    fn environment_session_defaults_do_not_count_as_explicit_flags() {
        const SENTINEL: &str = "AJ_TEST_SESSION_DEFAULTS_CHILD";
        if let Some(path) = std::env::var_os(SENTINEL) {
            for command in [
                vec!["serve"],
                vec!["continue", "id"],
                vec!["connect", "http://host"],
                vec!["gateway"],
                vec!["list-sessions"],
                vec!["update-models"],
            ] {
                let args = parse(&[vec!["aj"], command].concat());
                assert_eq!(args.model_api.as_deref(), Some("openai"));
                assert_eq!(args.model_name.as_deref(), Some("env-model"));
                assert_eq!(args.model_url.as_deref(), Some("https://example.test"));
                assert_eq!(args.thinking, Some(ConfigThinkingLevel::High));
                assert_eq!(args.speed.as_deref(), Some("fast"));
            }
            let root = parse(&["aj", "--thinking=off", "connect", "http://host", "--new"]);
            assert_eq!(root.thinking, Some(ConfigThinkingLevel::Off));
            let args = parse(&["aj", "connect", "http://host", "--new", "--thinking=off"]);
            assert_eq!(args.thinking, Some(ConfigThinkingLevel::Off));
            assert!(Args::try_parse_from(["aj", "continue", "--thinking=off"]).is_err());
            std::fs::write(path, "observed").unwrap();
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let sentinel = dir.path().join("observed");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::args::tests::environment_session_defaults_do_not_count_as_explicit_flags",
                "--nocapture",
            ])
            .env("MODEL_API", "openai")
            .env("MODEL_NAME", "env-model")
            .env("MODEL_URL", "https://example.test")
            .env("AJ_THINKING", "high")
            .env("AJ_SPEED", "fast")
            .env(SENTINEL, &sentinel)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "observed");
    }

    #[test]
    fn account_flags_distinguish_omission_default_and_exact_empty_identity() {
        assert!(parse(&["aj"]).account_selection().is_none());
        for (flag, expected) in [
            ("--default-account", None),
            ("--account=", Some("")),
            ("--account=default", Some("default")),
        ] {
            for argv in [
                vec!["aj", flag],
                vec!["aj", flag, "connect", "http://localhost:6161", "--new"],
                vec!["aj", "connect", "http://localhost:6161", "--new", flag],
            ] {
                assert_eq!(
                    parse(&argv).account_selection().unwrap().name.as_deref(),
                    expected
                );
            }
        }
    }

    #[test]
    fn account_flags_refuse_conflicts_and_commands_without_a_session() {
        for argv in [
            vec!["aj", "--account=work", "--default-account"],
            vec![
                "aj",
                "--account=work",
                "connect",
                "http://host",
                "--new",
                "--default-account",
            ],
            vec![
                "aj",
                "--default-account",
                "connect",
                "http://host",
                "--new",
                "--account=work",
            ],
            vec!["aj", "--api-key=key", "--account=work"],
            vec!["aj", "--api-key=key", "--default-account"],
            vec!["aj", "--api-key=key", "continue", "--account=work"],
            vec!["aj", "--api-key=key", "continue", "--default-account"],
            vec!["aj", "--account=personal", "continue", "--default-account"],
            vec!["aj", "--default-account", "continue", "--account=personal"],
            vec!["aj", "--account=work", "serve"],
            vec!["aj", "gateway", "--default-account"],
            vec!["aj", "--account=", "list-sessions"],
            vec!["aj", "update-models", "--default-account"],
        ] {
            assert!(
                Args::try_parse_from(&argv).is_err(),
                "accepted ineffective account choice: {argv:?}"
            );
        }
    }

    /// The control-port flags are shared with server commands:
    /// they configure the host a `serve` run is, and both placements read the
    /// same to a user.
    #[test]
    fn the_control_port_flags_parse_on_either_side_of_a_subcommand() {
        for argv in [
            [
                "aj",
                "--listen=127.0.0.1:6199",
                "--auth=tailscale",
                "--allow=alice@github",
                "serve",
            ],
            [
                "aj",
                "serve",
                "--listen=127.0.0.1:6199",
                "--auth=tailscale",
                "--allow=alice@github",
            ],
        ] {
            let args = parse(&argv);
            assert_eq!(args.listen.as_deref(), Some("127.0.0.1:6199"), "{argv:?}");
            assert_eq!(args.auth, "tailscale", "{argv:?}");
            assert_eq!(args.allow, vec!["alice@github".to_string()], "{argv:?}");
            assert!(matches!(args.command, Some(Command::Serve)), "{argv:?}");
        }
    }

    #[test]
    fn the_control_allowlist_aggregates_across_a_subcommand() {
        let args = parse(&[
            "aj",
            "--allow=alice@github,carol@github",
            "serve",
            "--allow",
            "bob@github",
            "--allow=dana@github,erin@github",
        ]);

        assert_eq!(
            args.allow,
            [
                "alice@github",
                "carol@github",
                "bob@github",
                "dana@github",
                "erin@github",
            ],
        );
    }

    /// The argv recovery applies only to explicit flags. Clap's environment
    /// binding remains authoritative when argv states no allowlist at all. The
    /// parse runs in a child so changing AJ_ALLOW cannot race sibling tests.
    #[test]
    fn the_control_allowlist_keeps_an_environment_only_value() {
        const CHILD_SENTINEL: &str = "AJ_TEST_ALLOW_ENV_CHILD_SENTINEL";

        if let Some(sentinel) = std::env::var_os(CHILD_SENTINEL) {
            let args = parse(&["aj", "serve"]);
            assert_eq!(
                args.allow,
                ["env-alice@github", "env-bob@github"],
                "argv recovery replaced AJ_ALLOW despite seeing no --allow flag",
            );
            assert_eq!(
                parse(&[
                    "aj",
                    "--allow",
                    "cli-alice@github",
                    "serve",
                    "--allow",
                    "cli-bob@github",
                ])
                .allow,
                ["cli-alice@github", "cli-bob@github"],
                "an explicit allowlist must retain CLI precedence over AJ_ALLOW",
            );
            std::fs::write(sentinel, "observed").expect("write the child sentinel");
            return;
        }

        let dir = tempfile::TempDir::new().expect("sentinel tempdir");
        let sentinel = dir.path().join("observed");
        let output =
            std::process::Command::new(std::env::current_exe().expect("locate this test binary"))
                .args([
                    "--exact",
                    "cli::args::tests::the_control_allowlist_keeps_an_environment_only_value",
                    "--nocapture",
                ])
                .env("AJ_ALLOW", "env-alice@github,env-bob@github")
                .env(CHILD_SENTINEL, &sentinel)
                .output()
                .expect("run the isolated parser child");

        assert!(
            output.status.success(),
            "isolated parser child failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            std::fs::read_to_string(sentinel).expect("the exact child test ran"),
            "observed",
        );
    }

    /// A bare `--listen` takes the loopback default in either placement, and
    /// `--auth` defaults to the mode that trusts nothing but loopback.
    #[test]
    fn a_bare_listen_defaults_after_a_subcommand_too() {
        for argv in [["aj", "--listen", "serve"], ["aj", "serve", "--listen"]] {
            let args = parse(&argv);
            assert_eq!(
                args.listen.as_deref(),
                Some(DEFAULT_LISTEN_ADDRESS),
                "{argv:?}",
            );
            assert_eq!(args.auth, "local", "{argv:?}");
            assert!(args.allow.is_empty(), "{argv:?}");
        }
    }

    /// A gateway takes its address from the same listen flag a host does, and
    /// its configuration file from a flag of its own.
    #[test]
    fn the_gateway_subcommand_takes_a_listen_address_and_a_config_file() {
        let args = parse(&[
            "aj",
            "--listen=127.0.0.1:6000",
            "gateway",
            "--config",
            "/etc/aj/gateway.toml",
        ]);
        assert_eq!(args.listen.as_deref(), Some("127.0.0.1:6000"));
        assert!(matches!(
            args.command,
            Some(Command::Gateway { config: Some(ref path) })
                if path == std::path::Path::new("/etc/aj/gateway.toml")
        ));

        let bare = parse(&["aj", "gateway"]);
        assert!(matches!(
            bare.command,
            Some(Command::Gateway { config: None })
        ));
        assert_eq!(
            bare.listen, None,
            "the address is resolved by the mode, which defaults it",
        );
        assert_eq!(bare.auth, "local", "and the gate defaults closed here too");
    }

    /// `--tag` is shared with `connect`, so it can name the session it asks
    /// the host to create without a second spelling of the flag.
    #[test]
    fn the_tag_flag_parses_on_either_side_of_a_subcommand() {
        for argv in [
            ["aj", "--tag", "fix-auth", "connect", "http://host:6161"],
            ["aj", "connect", "http://host:6161", "--tag", "fix-auth"],
        ] {
            let args = parse(&argv);
            assert_eq!(args.tag.as_deref(), Some("fix-auth"), "{argv:?}");
            assert_eq!(
                args.launch_tag(),
                Ok(Some("fix-auth".to_string())),
                "{argv:?}",
            );
        }
        assert_eq!(parse(&["aj"]).launch_tag(), Ok(None), "no flag, no label");
    }

    #[test]
    fn launch_env_is_repeatable_and_splits_only_the_first_equals() {
        let invocations: [&[&str]; 2] = [
            &[
                "aj",
                "--env",
                "AJ_CASE=upper,comma",
                "--env",
                "aj_case=lower=tail",
                "connect",
                "http://host",
                "--new",
            ],
            &[
                "aj",
                "connect",
                "http://host",
                "--new",
                "--env",
                "AJ_CASE=upper,comma",
                "--env",
                "aj_case=lower=tail",
            ],
        ];
        for argv in invocations {
            let args = parse(argv);
            assert_eq!(
                args.launch_env(),
                Ok(Some(BTreeMap::from([
                    ("AJ_CASE".to_string(), "upper,comma".to_string()),
                    ("aj_case".to_string(), "lower=tail".to_string()),
                ]))),
                "{argv:?}"
            );
        }
        assert_eq!(parse(&["aj"]).launch_env(), Ok(None));
    }

    #[test]
    fn launch_env_aggregates_two_valid_values_across_a_subcommand() {
        let two_valid = parse(&[
            "aj",
            "--env",
            "BEFORE=one",
            "connect",
            "http://host",
            "--new",
            "--env=AFTER=two=tail",
        ]);
        assert_eq!(
            two_valid.launch_env(),
            Ok(Some(BTreeMap::from([
                ("AFTER".to_string(), "two=tail".to_string()),
                ("BEFORE".to_string(), "one".to_string()),
            ])))
        );
    }

    #[test]
    fn long_help_keeps_product_description_and_lists_shared_options_once() {
        let help = Args::command().render_long_help().to_string();
        assert_eq!(help.matches("--env <KEY=VALUE>").count(), 1, "{help}");
        assert_eq!(help.matches("--allow <ALLOW>").count(), 1, "{help}");
        assert_eq!(help.matches("[env: AJ_ALLOW=]").count(), 1, "{help}");
        assert!(
            help.starts_with("AI-driven agent for software engineering"),
            "{help}"
        );
        assert!(!help.contains("Construct this type through"), "{help}");
        assert!(!help.contains("<Args as clap::Parser>"), "{help}");
    }

    /// Rendered help is prose for someone at a shell, so it names no Rust
    /// item. Doc comments on clap-visible fields are the help text, which
    /// makes any rustdoc link or path in them leak into the terminal. The
    /// rendered help is asserted directly because a doc comment can be edited,
    /// or a field added, without any other subject noticing.
    ///
    /// Two checks because the failure is a shape, except for constants: a
    /// `SCREAMING_CASE` item in brackets looks exactly like clap's own
    /// `[SESSION_ID]` placeholder, so those are pinned by name.
    #[test]
    fn long_help_names_no_rust_item() {
        let help = Args::command().render_long_help().to_string();
        for constant in ["HOST_WITHOUT_A_CREATE", "TAG_WITHOUT_A_CREATE"] {
            assert!(
                !help.contains(constant),
                "rendered help names the constant {constant}: {help}"
            );
        }
        assert!(
            !help.contains("::"),
            "rendered help contains Rust path syntax: {help}"
        );
        // A bracketed token is clap's own placeholder vocabulary (`[OPTIONS]`,
        // `[SESSION_ID]`) or an annotation carrying spaces (`[env: AJ_ALLOW=]`).
        // A rustdoc link is neither: it is a bare Rust item, so it is the only
        // bracketed token that is CamelCase.
        for token in help
            .split('[')
            .skip(1)
            .filter_map(|rest| rest.split_once(']'))
            .map(|(token, _)| token)
        {
            let rust_item = !token.contains(char::is_whitespace)
                && token.starts_with(char::is_uppercase)
                && token.contains(char::is_lowercase);
            assert!(
                !rust_item,
                "rendered help links the Rust item [{token}]: {help}"
            );
        }
    }

    #[test]
    fn launch_env_keeps_a_malformed_value_before_a_subcommand() {
        let malformed = parse(&[
            "aj",
            "--env",
            "MISSING",
            "connect",
            "http://host",
            "--new",
            "--env",
            "OK=value",
        ]);
        assert!(matches!(
            malformed.launch_env(),
            Err(LaunchEnvError::MissingEquals { argument }) if argument == "MISSING"
        ));
    }

    #[test]
    fn launch_env_detects_a_duplicate_split_by_a_subcommand() {
        let duplicate = parse(&[
            "aj",
            "--env",
            "KEY=first",
            "connect",
            "http://host",
            "--new",
            "--env",
            "KEY=second",
        ]);
        assert!(matches!(
            duplicate.launch_env(),
            Err(LaunchEnvError::DuplicateKey { key }) if key == "KEY"
        ));
    }

    #[test]
    fn launch_env_refuses_missing_equals_empty_key_and_exact_duplicates() {
        let cases: [(&[&str], &str); 3] = [
            (&["aj", "--env", "MISSING"], "must contain '='"),
            (&["aj", "--env", "=value"], "key \"\" is empty"),
            (
                &["aj", "--env", "KEY=first", "--env", "KEY=second"],
                "key \"KEY\" was stated more than once",
            ),
        ];
        for (argv, expected) in cases {
            let args = parse(argv);
            let err = args.launch_env().expect_err("invalid launch env");
            assert!(err.to_string().contains(expected), "{argv:?}: {err}");
        }
    }

    /// Thinking is shared with connect and reads the same on either side of a
    /// subcommand.
    #[test]
    fn the_thinking_flag_parses_on_either_side_of_connect() {
        for argv in [
            [
                "aj",
                "--thinking",
                "high",
                "connect",
                "http://host:6161",
                "--new",
            ],
            [
                "aj",
                "connect",
                "http://host:6161",
                "--new",
                "--thinking",
                "high",
            ],
        ] {
            let args = parse(&argv);
            assert_eq!(args.thinking, Some(ConfigThinkingLevel::High), "{argv:?}");
            assert!(
                matches!(args.command, Some(Command::Connect { .. })),
                "{argv:?}"
            );
        }
    }

    /// The typed CLI boundary rejects an illegal level before any frontend can
    /// acquire a terminal or dial a host.
    #[test]
    fn an_unknown_thinking_level_is_refused_before_frontend_dispatch() {
        let invocations: [&[&str]; 3] = [
            &["aj", "--thinking", "ludicrous"],
            &["aj", "--print", "--thinking", "ludicrous", "hello"],
            &[
                "aj",
                "connect",
                "http://host:6161",
                "--thinking",
                "ludicrous",
            ],
        ];

        for argv in invocations {
            let error = Args::try_parse_from(argv)
                .expect_err("an unknown thinking level must be refused by clap");
            assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
            assert!(
                error.to_string().contains(
                    "invalid thinking level 'ludicrous': expected off, minimal, low, medium, high, xhigh, or max"
                ),
                "{argv:?}: {error}"
            );
        }
    }

    /// A tag the store would not keep is refused where the user typed it, so
    /// the run never creates a session the flag failed to name. The trim and
    /// the blank-clears rule are the same ones the wire applies.
    #[test]
    fn an_illegal_tag_is_refused_at_the_flag() {
        assert_eq!(
            parse(&["aj", "--tag", "two\nlines"]).launch_tag(),
            Err(TagError::Control),
        );
        let long = "a".repeat(aj_session::MAX_TAG_BYTES + 1);
        assert!(matches!(
            parse(&["aj", "--tag", &long]).launch_tag(),
            Err(TagError::TooLong { .. }),
        ));
        assert_eq!(
            parse(&["aj", "--tag", "  spaced  "]).launch_tag(),
            Ok(Some("spaced".to_string())),
        );
        assert_eq!(parse(&["aj", "--tag", "   "]).launch_tag(), Ok(None));
    }

    /// The report on a resume goes by the normalized label, so a flag that
    /// names nothing has nothing to report and an illegal one is left to
    /// `launch_tag`, which refuses the run outright.
    #[test]
    fn a_blank_tag_flag_names_no_session() {
        assert!(parse(&["aj", "--tag", "fix-auth"]).has_launch_tag());
        assert!(!parse(&["aj"]).has_launch_tag());
        assert!(!parse(&["aj", "--tag", "   "]).has_launch_tag());
        assert!(!parse(&["aj", "--tag", "two\nlines"]).has_launch_tag());
    }

    /// `--name` is shared with host commands and reads the same on either side of a
    /// subcommand, the way the control-port flags do.
    ///
    /// Nothing here asserts what an absent flag yields: `AJ_NAME` in the
    /// environment answers that, and this suite must not depend on the
    /// operator's. The host's own fallback is pinned where it happens.
    #[test]
    fn the_name_flag_parses_on_either_side_of_a_subcommand() {
        for argv in [
            ["aj", "--name", "umber/aj", "serve"],
            ["aj", "serve", "--name", "umber/aj"],
        ] {
            let args = parse(&argv);
            assert_eq!(
                args.host_name(),
                Ok(Some("umber/aj".to_string())),
                "{argv:?}",
            );
        }
    }

    /// A name no peer would render is refused where the operator typed it, so
    /// the host never comes up under one. The trim and the blank-names-nothing
    /// rule are the ones the wire applies.
    #[test]
    fn an_illegal_name_is_refused_at_the_flag() {
        assert_eq!(
            parse(&["aj", "--name", "two\nlines"]).host_name(),
            Err(HostNameError::Control),
        );
        let long = "a".repeat(aj_wire::MAX_HOST_NAME_BYTES + 1);
        assert!(matches!(
            parse(&["aj", "--name", &long]).host_name(),
            Err(HostNameError::TooLong { .. }),
        ));
        assert_eq!(
            parse(&["aj", "--name", "  spaced  "]).host_name(),
            Ok(Some("spaced".to_string())),
        );
        assert_eq!(
            parse(&["aj", "--name", "   "]).host_name(),
            Ok(None),
            "a flag that names nothing leaves the derivation to it",
        );
    }

    /// The three states of the grammar, and the fourth argv shape that is not
    /// a state: a run under `--new` creates, so the id slot holds input.
    #[test]
    fn connect_positionals_divide_by_whether_the_run_creates() {
        let latest = parse(&["aj", "connect", "http://host:6161"]);
        let latest = latest.connect_launch().expect("a connect run");
        assert_eq!(latest.session(), ConnectSession::Latest);
        assert!(latest.prompt.is_empty());

        let named = parse(&["aj", "connect", "http://host:6161", "ID", "do", "this"]);
        let named = named.connect_launch().expect("a connect run");
        assert_eq!(named.session(), ConnectSession::Named("ID"));
        assert_eq!(named.prompt, ["do", "this"]);

        let fresh = parse(&["aj", "connect", "http://host:6161", "--new"]);
        let fresh = fresh.connect_launch().expect("a connect run");
        assert_eq!(fresh.session(), ConnectSession::Fresh);
        assert!(fresh.prompt.is_empty());

        // One quoted sentence, which clap binds to the id slot because it
        // fills that one first.
        let created = parse(&["aj", "connect", "http://host:6161", "--new", "do this"]);
        let created = created.connect_launch().expect("a connect run");
        assert_eq!(
            created.session(),
            ConnectSession::Fresh,
            "a run that creates resolved to a session to attach",
        );
        assert_eq!(
            created.prompt,
            ["do this"],
            "the launch input was read as a session id",
        );
    }

    /// Under `--new` the id slot is the launch input's first word, so the turn
    /// reads in argv order rather than losing or reordering it.
    #[test]
    fn a_created_session_keeps_its_launch_input_in_order() {
        let args = parse(&["aj", "connect", "http://host:6161", "--new", "do", "this"]);
        assert_eq!(args.launch_positionals(), ["do", "this"]);
    }

    /// An id typed under `--new` is launch input like any other positional.
    /// Nothing tells it from a one-word prompt, so the grammar reads it the one
    /// way it can and the run creates.
    #[test]
    fn an_id_under_new_is_read_as_input() {
        let args = parse(&[
            "aj",
            "connect",
            "http://host:6161",
            "2026-08-04-12-00-00-000",
            "--new",
            "and",
            "more",
        ]);
        let launch = args.connect_launch().expect("a connect run");
        assert_eq!(launch.session(), ConnectSession::Fresh);
        assert_eq!(launch.prompt, ["2026-08-04-12-00-00-000", "and", "more"]);
    }

    /// Both positional slots can be filled at once, so the slot table needs a
    /// rule for it. A bare positional before a subcommand swallows the
    /// subcommand name, but a flag between them re-opens subcommand matching,
    /// and then the run's turn is the one in the top-level slot.
    #[test]
    fn a_subcommand_with_an_empty_slot_yields_to_the_top_level_one() {
        let swallowed = parse(&["aj", "hello", "connect", "http://host:6161"]);
        assert!(swallowed.command.is_none());
        assert_eq!(
            swallowed.launch_positionals(),
            ["hello", "connect", "http://host:6161"]
        );

        for argv in [
            vec!["aj", "hello", "--tag", "t", "connect", "http://host:6161"],
            vec![
                "aj",
                "hello",
                "--tag",
                "t",
                "connect",
                "http://host:6161",
                "--new",
            ],
            vec!["aj", "hello", "--name", "host", "continue", "ID"],
        ] {
            let args = parse(&argv);
            assert!(
                args.command.is_some(),
                "{argv:?} parsed with no subcommand, so it no longer reaches the fallback",
            );
            assert_eq!(
                args.launch_positionals(),
                ["hello"],
                "{argv:?} dropped the turn its top-level slot carried",
            );
        }
    }

    /// Only a connect run has this grammar, and the other slots keep theirs.
    #[test]
    fn the_other_launch_slots_are_untouched() {
        assert!(parse(&["aj", "hello"]).connect_launch().is_none());
        assert!(
            parse(&["aj", "continue", "ID", "do", "this"])
                .connect_launch()
                .is_none()
        );
        assert_eq!(
            parse(&["aj", "hello", "there"]).launch_positionals(),
            ["hello", "there"]
        );
        assert_eq!(
            parse(&["aj", "continue", "ID", "do", "this"]).launch_positionals(),
            ["do", "this"]
        );
    }
}
