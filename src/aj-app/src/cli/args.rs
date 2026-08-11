//! [`clap`]-derived argument parsing for the `aj` binary.
//!
//! The `--print` / `--json` toggles select the non-interactive
//! print mode; otherwise the binary runs the interactive
//! TUI. Subcommands (`list-sessions`, `continue`, `update-models`)
//! short-circuit before mode dispatch.

use std::path::PathBuf;

use aj_session::{TagError, normalize_tag};
use clap::{Parser, Subcommand, ValueEnum};

/// Top-level CLI for the `aj` binary.
#[derive(Parser, Debug)]
#[command(name = "aj")]
#[command(about = "AI-driven agent for software engineering")]
#[command(flatten_help = true)]
pub struct Args {
    /// Model API to use (e.g. `anthropic`, `openai`, `openai-codex`,
    /// `openrouter`).
    #[arg(long, env = "MODEL_API")]
    pub model_api: Option<String>,

    /// Override the model endpoint URL.
    #[arg(long, env = "MODEL_URL")]
    pub model_url: Option<String>,

    /// Model name to use (provider-specific identifier).
    #[arg(long, env = "MODEL_NAME")]
    pub model_name: Option<String>,

    /// API key for the resolved provider, applied as a runtime
    /// override for this run only. Takes precedence over env vars
    /// and any credential stored in `~/.aj/auth.json`, and is never
    /// written to disk. Intentionally has no `env =` binding so the
    /// only way to supply it is the explicit flag (provider-specific
    /// env vars like `ANTHROPIC_API_KEY` remain the env path).
    #[arg(long)]
    pub api_key: Option<String>,

    /// Inference speed mode: `standard` (default) or `fast`. Fast mode
    /// is Anthropic-only — it sends `speed: "fast"` in the request body
    /// together with the `fast-mode-2026-02-01` beta header. Models
    /// that don't support fast mode reject the request.
    #[arg(long, env = "AJ_SPEED")]
    pub speed: Option<String>,

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
    /// turn, which both print and interactive mode auto-submit. See
    /// [`crate::cli::initial_input`] for the full rules.
    pub prompt: Vec<String>,

    /// Replace the live model with a scripted fake that replays a
    /// canned
    /// [`AssistantMessageEvent`](aj_models::streaming::AssistantMessageEvent)
    /// sequence. Useful for eyeballing how the TUI / print mode
    /// renders thinking blocks, tool calls, errors, and the like,
    /// without spending a real API round-trip.
    ///
    /// The argument is the demo name. Pass `--scripted help` (or any
    /// unknown name) to see the catalog. When set the binary skips
    /// registry-driven provider construction entirely and registers a
    /// [`ScriptedProvider`](aj_models::scripted::ScriptedProvider)
    /// in its place; every other code path (TUI, persistence, tools,
    /// commands) runs unchanged so the eyeball test exercises
    /// the real surface.
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
        global = true,
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
    #[arg(long, global = true, env = "AJ_AUTH", default_value = "local")]
    pub auth: String,

    /// Tailnet login allowed to connect in `--auth tailscale` mode,
    /// repeatable. Spelled exactly as the tailscale daemon reports it
    /// (e.g. `alice@github`). A tagged node has no login and is admitted
    /// only by the aj control capability granted in the tailnet policy.
    #[arg(long, global = true, env = "AJ_ALLOW", value_delimiter = ',')]
    pub allow: Vec<String>,

    /// Name the session this run creates, shown in place of its id
    /// wherever a session is listed.
    ///
    /// Global, so it reads the same on either side of a subcommand and
    /// reaches the create a `connect --new` performs on the host. A run that
    /// resumes an existing session creates nothing for the flag to name, and
    /// says so rather than dropping it.
    #[arg(long, global = true)]
    pub tag: Option<String>,

    /// Subcommand selector for the non-conversational utilities
    /// (`list-sessions`, `continue`, `update-models`) and the
    /// remote-control modes (`serve`, `connect`).
    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Args {
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

    /// Whether `--tag` carries a label for this run to give a session.
    ///
    /// The normalized answer, so a flag that names nothing reads as no flag,
    /// which is what makes [`TAG_WITHOUT_A_CREATE`] a report about a label
    /// that exists. An illegal one answers `false` here and is reported by
    /// [`Self::launch_tag`], which every mode calls before anything is minted.
    pub fn has_launch_tag(&self) -> bool {
        matches!(self.launch_tag(), Ok(Some(_)))
    }
}

/// What a run says when `--tag` named a session it never created.
///
/// The flag labels the session a run mints. A resumed session already carries
/// whatever label it was given, and relabelling it from the launch line would
/// be a second meaning for one flag, so the run reports instead.
pub const TAG_WITHOUT_A_CREATE: &str = "--tag has nothing to name: this run resumed a session rather than creating one. \
     Use the session-tag command to relabel it.";

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
    /// One JSONL [`aj_agent::events::AgentEvent`] per line. Stable
    /// shape suitable for piping into another process.
    Json,
}

/// Non-conversational subcommands.
#[derive(Subcommand, Debug)]
#[command(flatten_help = true)]
pub enum Command {
    /// List existing conversation sessions for this project.
    ListSessions,
    /// Continue a conversation session (latest if no id given).
    ///
    /// Accepts optional positional launch input after the session id:
    /// `aj continue ID <args...>` resumes the session and auto-submits
    /// the args (messages and `@file` attachments) as the next turn.
    /// With no session id, the latest session for the current project
    /// is resumed; supplying input without a session id is ambiguous,
    /// so callers wanting "latest + prompt" should pass the session id
    /// explicitly (e.g. via `aj list-sessions`).
    Continue {
        /// Conversation ID to continue. If absent, the latest
        /// session for this project is resumed.
        session_id: Option<String>,
        /// Launch input for the resumed run, interpreted exactly like
        /// the top-level [`Args::prompt`]: a mix of `@file` attachments
        /// and messages, auto-submitted as the next turn.
        prompt: Vec<String>,
    },
    /// Refresh the user model catalog at `~/.aj/models.json` from
    /// `https://models.dev/api.json`.
    UpdateModels,
    /// Serve this working directory's sessions headlessly on the control
    /// port, with no terminal UI of its own.
    ///
    /// The address comes from `--listen` / `AJ_LISTEN`, which are global and
    /// so may be given on either side of the subcommand. It defaults to the
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
    /// the next turn. Supplying input without an id is ambiguous (the first
    /// positional is read as the session id), so the id has to be explicit.
    Connect {
        /// Base URL of the host's control port (e.g.
        /// `http://100.64.0.2:6161`).
        url: String,
        /// Session to attach. Omit to take the host's latest.
        session_id: Option<String>,
        /// Create a fresh session instead of attaching an existing one.
        #[arg(long)]
        new: bool,
        /// Launch input for the attached session, interpreted exactly like
        /// the top-level [`Args::prompt`]: a mix of `@file` attachments and
        /// messages, auto-submitted as the next turn.
        prompt: Vec<String>,
    },
    /// Aggregate many session hosts behind one address.
    ///
    /// A gateway serves the session-facing API of a host, forwarding every
    /// request to the host that owns the session, and adds the host-management
    /// endpoints. It holds no sessions of its own.
    ///
    /// The address comes from `--listen` / `AJ_LISTEN`, which are global and so
    /// may be given on either side of the subcommand, and defaults to the
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
    use clap::Parser;

    use super::*;

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

    /// The control-port flags are global, so a subcommand does not hide them:
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

    /// A connect run reaches them too, which is what lets the shell say the
    /// flag has nothing to serve rather than silently dropping it.
    #[test]
    fn the_control_port_flags_reach_a_connect_run() {
        let args = parse(&["aj", "connect", "http://127.0.0.1:6161", "--listen"]);
        assert_eq!(args.listen.as_deref(), Some(DEFAULT_LISTEN_ADDRESS));
        assert!(matches!(args.command, Some(Command::Connect { .. })));
    }

    /// A gateway takes its address from the same global flag a host does, and
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

    /// `--tag` is global, so a `connect --new` can name the session it asks
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
}
