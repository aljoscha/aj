//! Binary entry point for the `aj` CLI.
//!
//! Loads `~/.aj/.env`, parses the shared CLI surface
//! ([`aj_app::cli::args::Args`]), and dispatches. The non-interactive
//! subcommands and print mode reuse `aj-app` directly. The interactive branch
//! is the vaxis frontend (see `interactive`).

use aj_app::cli::args::{Args, Command};
use aj_conf::Config;
use anyhow::{Result, anyhow, bail};
use tracing_subscriber::EnvFilter;

/// Where this run's log output goes.
enum LogDestination<W> {
    Stderr,
    File(W),
    /// Logs are dropped on the floor.
    Discard,
}

/// Picks the log destination, opening `path` with `open` when one is given.
///
/// An interactive run must never write to the terminal: the TUI renders to the
/// same tty, so log lines punch holes in the alt screen. So when we cannot open
/// a log file there we discard the logs instead of falling back to stderr. The
/// returned error is the caller's to report, before the TUI takes over.
fn select_log_destination<P, W, E>(
    path: Option<P>,
    interactive: bool,
    open: impl FnOnce(P) -> Result<W, E>,
) -> (LogDestination<W>, Option<E>) {
    let fallback = if interactive {
        LogDestination::Discard
    } else {
        LogDestination::Stderr
    };
    let Some(path) = path else {
        return (fallback, None);
    };
    match open(path) {
        Ok(writer) => (LogDestination::File(writer), None),
        Err(error) => (fallback, Some(error)),
    }
}

/// Whether `args` starts the interactive TUI, as opposed to print mode or a
/// one-shot subcommand.
///
/// `connect` is interactive too: it runs the same shell against a remote
/// host, so it shares the rule that logs must never share the terminal.
fn is_interactive(args: &Args) -> bool {
    !args.print
        && matches!(
            args.command,
            None | Some(Command::Continue { .. }) | Some(Command::Connect { .. })
        )
}

mod agent_picker;
mod autocomplete;
mod bubble;
mod connect;
mod content_overlay;
mod control;
mod corner_box;
mod footer;
mod frame_stats_box;
mod gateway;
mod host_picker;
mod image_store;
mod interactive;
mod keymap;
mod login;
mod markdown_view;
mod overlay;
mod palette;
mod pending;
mod prompt_history;
mod quit_hint;
mod remote;
mod scroll;
mod selection_copied;
mod serve;
mod session_selector;
mod session_tag;
mod session_tree;
mod settings_ui;
mod sidebar;
mod splash;
mod status;
mod subagent_box;
mod task_output;
mod terminal;
#[cfg(test)]
mod test_support;
mod text;
mod toasts;
mod tool_cell;
mod transcript;
mod usage_overlay;

#[tokio::main]
async fn main() -> Result<()> {
    // `~/.aj/.env` first (highest priority for env-driven config), then a
    // project-local `.env` if present. `dotenv` preserves values already set,
    // so loading in this order implements the documented precedence.
    if let Ok(dotenv_path) = Config::get_dotenv_file_path() {
        dotenv::from_path(dotenv_path).ok();
    }
    dotenv::dotenv().ok();

    // Logs never share the terminal with the interactive TUI: an interactive
    // run writes them to `AJ_LOG_FILE`, or to `~/.aj/logs/aj.log`. Print mode
    // and the one-shot subcommands keep stderr, which is the CLI convention.
    // Nothing is emitted at all unless `RUST_LOG` is set, since an empty
    // `EnvFilter` enables no callsites.
    let args = Args::parse();
    args.launch_env().map_err(|err| anyhow!("--env: {err}"))?;
    // CLI-wide mode refusals live here because this is the only boundary that
    // can reject before logging and dispatch. Command runners receive
    // preflighted arguments and do not repeat this policy.
    if args.has_launch_env() {
        match &args.command {
            Some(Command::Serve) => {
                bail!("--env is stated per session create and cannot configure aj serve")
            }
            Some(Command::Gateway { .. }) => {
                bail!("--env is stated per session create and cannot configure aj gateway")
            }
            _ => {}
        }
    }
    let interactive = is_interactive(&args);
    let log_path = match std::env::var_os("AJ_LOG_FILE") {
        Some(path) => Some(std::path::PathBuf::from(path)),
        None if interactive => match Config::log_file_path() {
            Ok(path) => Some(path),
            Err(error) => {
                eprintln!("aj: warning: could not resolve the log file; logs disabled: {error}");
                None
            }
        },
        None => None,
    };
    let (destination, open_error) =
        select_log_destination(log_path.as_deref(), interactive, |path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        });
    if let Some(error) = open_error {
        let fallback = if interactive {
            "logs disabled"
        } else {
            "logging to stderr"
        };
        eprintln!("aj: warning: could not open the log file; {fallback}: {error}");
    }
    let builder = tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env());
    match destination {
        LogDestination::File(file) => {
            builder
                .with_ansi(false)
                .with_writer(std::sync::Arc::new(file))
                .init();
        }
        LogDestination::Stderr => builder.with_ansi(true).init(),
        LogDestination::Discard => builder.with_writer(std::io::sink).init(),
    }

    match args.command {
        Some(Command::UpdateModels) => aj_app::handle_update_models_command().await,
        Some(Command::ListSessions) => aj_app::handle_list_sessions(),
        Some(Command::Serve) => serve::run(args).await,
        Some(Command::Gateway { .. }) => gateway::run(args).await,
        Some(Command::Continue {
            session_id: _,
            prompt: _,
        }) => dispatch_session_mode(args).await,
        Some(Command::Connect { .. }) | None => dispatch_session_mode(args).await,
    }
}

#[cfg(test)]
mod startup_tests {
    use std::path::Path;

    use aj_app::cli::args::{Args, Command};

    use super::{LogDestination, is_interactive, select_log_destination};

    #[test]
    fn logging_defaults_to_stderr_for_non_interactive_runs() {
        let (destination, error) =
            select_log_destination::<&Path, (), ()>(None, false, |_| unreachable!());
        assert!(matches!(destination, LogDestination::Stderr));
        assert!(error.is_none());
    }

    #[test]
    fn interactive_logging_is_discarded_without_a_path() {
        let (destination, error) =
            select_log_destination::<&Path, (), ()>(None, true, |_| unreachable!());
        assert!(matches!(destination, LogDestination::Discard));
        assert!(error.is_none());
    }

    #[test]
    fn logging_falls_back_to_stderr_when_logfile_open_fails() {
        let (destination, error) =
            select_log_destination(Some(Path::new("bad.log")), false, |_| {
                Err::<(), _>("open failed")
            });
        assert!(matches!(destination, LogDestination::Stderr));
        assert_eq!(error, Some("open failed"));
    }

    #[test]
    fn interactive_logging_never_falls_back_to_the_terminal() {
        let (destination, error) = select_log_destination(Some(Path::new("bad.log")), true, |_| {
            Err::<(), _>("open failed")
        });
        assert!(matches!(destination, LogDestination::Discard));
        assert_eq!(error, Some("open failed"));
    }

    #[test]
    fn logging_uses_an_opened_logfile() {
        let (destination, error) =
            select_log_destination(Some(Path::new("aj.log")), true, |_| Ok::<_, ()>(7));
        assert!(matches!(destination, LogDestination::File(7)));
        assert!(error.is_none());
    }

    fn args_of(argv: &[&str]) -> Args {
        Args::try_parse_from(argv).expect("parses")
    }

    #[test]
    fn only_the_tui_modes_count_as_interactive() {
        assert!(is_interactive(&args_of(&["aj"])));
        assert!(is_interactive(&args_of(&["aj", "continue"])));
        assert!(is_interactive(&args_of(&["aj", "continue", "abc"])));
        assert!(is_interactive(&args_of(&[
            "aj",
            "connect",
            "http://host:6161"
        ])));
        assert!(!is_interactive(&args_of(&["aj", "--print", "hello"])));
        assert!(!is_interactive(&args_of(&["aj", "list-sessions"])));
        assert!(!is_interactive(&args_of(&["aj", "update-models"])));
        assert!(
            !is_interactive(&args_of(&["aj", "serve"])),
            "serve has no terminal of its own, so its logs keep stderr",
        );
        assert!(
            !is_interactive(&args_of(&["aj", "gateway"])),
            "nor does a gateway, which renders nothing at all",
        );
    }

    /// `--listen` takes the loopback default bare and an explicit address
    /// with `=`. The equals form is load-bearing: a space-separated optional
    /// value would swallow the first word of `aj --listen fix the parser`.
    #[test]
    fn listen_defaults_to_loopback_and_accepts_an_explicit_address() {
        assert_eq!(
            args_of(&["aj", "--listen"]).listen.as_deref(),
            Some("127.0.0.1:6161"),
        );
        assert_eq!(
            args_of(&["aj", "--listen=100.64.0.1:7000"])
                .listen
                .as_deref(),
            Some("100.64.0.1:7000"),
        );
        assert_eq!(args_of(&["aj"]).listen, None, "a plain run serves nothing");

        let with_prompt = args_of(&["aj", "--listen", "explain", "this"]);
        assert_eq!(with_prompt.listen.as_deref(), Some("127.0.0.1:6161"));
        assert_eq!(with_prompt.prompt, vec!["explain", "this"]);
    }

    /// The identity gate's inputs, whose defaults are the safe ones: no
    /// allowlist, and the mode that serves loopback only.
    #[test]
    fn the_identity_gate_arguments_default_closed() {
        let plain = args_of(&["aj"]);
        assert_eq!(plain.auth, "local");
        assert!(plain.allow.is_empty());

        let shared = args_of(&[
            "aj",
            "--auth",
            "tailscale",
            "--allow",
            "alice@github",
            "--allow",
            "bob@github,carol@github",
        ]);
        assert_eq!(shared.auth, "tailscale");
        assert_eq!(
            shared.allow,
            vec!["alice@github", "bob@github", "carol@github"],
            "repeated flags and comma-separated values both accumulate",
        );
    }

    #[test]
    fn connect_takes_an_optional_session_and_a_new_flag() {
        let bare = args_of(&["aj", "connect", "http://host:6161"]);
        assert!(matches!(
            bare.command,
            Some(Command::Connect { ref url, session_id: None, new: false, .. })
                if url == "http://host:6161"
        ));
        let picked = args_of(&["aj", "connect", "http://host:6161", "20260804-120000"]);
        assert!(matches!(
            picked.command,
            Some(Command::Connect { session_id: Some(ref id), .. }) if id == "20260804-120000"
        ));
        assert!(matches!(
            args_of(&["aj", "connect", "http://host:6161", "--new"]).command,
            Some(Command::Connect { new: true, .. }),
        ));
    }

    /// Launch input for connect mode follows the session id, exactly as for
    /// `continue`. The grammar is ambiguous without the id: the first free
    /// positional is the session, so `aj connect URL "do this"` asks for a
    /// session called `do this` rather than submitting a prompt. What that
    /// binding means for a run that creates is
    /// [`Args::connect_launch`](aj_app::cli::args::Args::connect_launch),
    /// which is where the id slot is read.
    #[test]
    fn connect_takes_launch_input_after_the_session_id() {
        let with_prompt = args_of(&["aj", "connect", "http://host:6161", "ID", "do", "this"]);
        assert!(matches!(
            with_prompt.command,
            Some(Command::Connect { session_id: Some(ref id), ref prompt, .. })
                if id == "ID" && prompt == &["do".to_string(), "this".to_string()]
        ));
        let ambiguous = args_of(&["aj", "connect", "http://host:6161", "do this"]);
        assert!(matches!(
            ambiguous.command,
            Some(Command::Connect { session_id: Some(ref id), ref prompt, .. })
                if id == "do this" && prompt.is_empty()
        ));
    }
}

/// Dispatch to the interactive or print mode based on `--print`.
///
/// Print mode reuses `aj-app`'s headless runner. The interactive branch is
/// the vaxis alt-screen shell. Its futures are `!Send`, which is fine here:
/// `#[tokio::main]` drives this future with a top-level `block_on`.
async fn dispatch_session_mode(args: Args) -> Result<()> {
    if args.print {
        aj_app::print::run(args).await
    } else {
        interactive::run(args).await
    }
}
