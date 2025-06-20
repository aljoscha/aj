use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{SessionState, ToolDefinition, TurnState};

const DESCRIPTION: &str = r#"
Execute a command in the system shell (bash/sh). The command will be run in the
working directory of the agent session.

Safety:
- Commands have a configurable timeout to prevent hanging
- No shell metacharacters (pipes, redirection, etc.) are supported
- Output is truncated to 35000 characters to prevent excessive output

Usage:
- Provide a clear description of what the command does and why you want to run it
- Set an appropriate timeout (default is 30 seconds)
- The command will be parsed and executed directly, not inside a shell
"#;

pub struct BashTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct BashInput {
    /// The command to execute in the shell.
    command: String,
    /// Timeout in seconds after which the command will be cancelled (default: 30).
    #[serde(default = "default_timeout")]
    timeout: u64,
    /// A description explaining what the command does and why you want to run it.
    description: String,
}

fn default_timeout() -> u64 {
    30
}

const ALLOWED_COMMAND_PREFIXES: &[&str] = &[
    "ls",
    "pwd",
    "echo",
    "cat",
    "head",
    "tail",
    "rg",
    "find",
    "wc",
    "sort",
    "uniq",
    "which",
    "type",
    "file",
    "stat",
    "tree",
    "cargo build",
    "cargo check",
    "cargo test",
    "cargo nextest",
    "cargo run",
    "cargo fmt",
    "cargo clippy",
    "git status",
    "git log",
    "git show",
    "git diff",
    "git branch",
    "git remote",
    "npm test",
    "npm run",
    "npm build",
    "yarn test",
    "yarn run",
    "yarn build",
    "make test",
    "make build",
    "make check",
    "python -m pytest",
    "python -m unittest",
    "python -c",
    "node -e",
    "node --version",
    "npm --version",
    "cargo --version",
    "git --version",
    "rustc --version",
];

impl ToolDefinition for BashTool {
    type Input = BashInput;

    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn execute(
        &self,
        session_state: &mut dyn SessionState,
        _turn_state: &mut dyn TurnState,
        input: Self::Input,
    ) -> Result<String, anyhow::Error> {
        // Check if command is allowed
        if !is_command_allowed(&input.command) {
            // Ask user for confirmation
            println!("\nCommand not on allowlist: {}", input.command);
            println!("Description: {}", input.description);
            print!("Allow this command? (y/n): ");
            std::io::stdout().flush()?;

            let mut user_input = String::new();
            std::io::stdin().read_line(&mut user_input)?;
            let user_input = user_input.trim().to_lowercase();

            if user_input != "y" && user_input != "yes" {
                return Err(anyhow::anyhow!("Command execution cancelled by user"));
            }
        }

        let parsed_command = parse_command(&input.command)?;

        // // Resolve the executable path
        // let executable_path = resolve_executable(&parsed_command.executable)?;

        // Execute the command with timeout
        let working_dir = session_state.working_directory();
        let child = Command::new(&parsed_command.executable)
            .args(&parsed_command.args)
            .current_dir(&working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to start command '{}': {}",
                    parsed_command.executable,
                    e
                )
            })?;

        // Set up channels for timeout handling
        let (tx, rx) = mpsc::channel();
        let child_id = child.id();

        // Spawn a thread to wait for the process
        thread::spawn(move || {
            let output = child.wait_with_output();
            let _ = tx.send(output);
        });

        // Wait for completion or timeout
        match rx.recv_timeout(Duration::from_secs(input.timeout)) {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                let mut result = String::new();
                if !stdout.is_empty() {
                    result.push_str("STDOUT:\n");
                    result.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str("STDERR:\n");
                    result.push_str(&stderr);
                }

                if !output.status.success() {
                    result.push_str(&format!(
                        "\nCommand failed with exit code: {}",
                        output.status.code().unwrap_or(-1)
                    ));
                }

                // A full run of cargo clippy on the materialize repo, with
                // compiling deps and everything outputs about 35k of data. So
                // an incremental run should be below that, even when there are
                // warnings and stuff.
                if result.len() > 35000 {
                    result.truncate(35000);
                    result.push_str("\n[Output truncated at 35000 characters]");
                }

                Ok(result)
            }
            Ok(Err(e)) => Err(anyhow::anyhow!("Command execution failed: {}", e)),
            Err(_) => {
                // Timeout occurred, try to kill the process
                #[cfg(unix)]
                {
                    use std::process;
                    let _ = process::Command::new("kill")
                        .arg("-9")
                        .arg(child_id.to_string())
                        .output();
                }

                Err(anyhow::anyhow!(
                    "Command timed out after {} seconds",
                    input.timeout
                ))
            }
        }
    }
}

/// Represents a parsed command with executable and arguments.
#[derive(Debug, Clone)]
struct ParsedCommand {
    executable: String,
    args: Vec<String>,
}

/// Parse a command string into executable and arguments.
/// This is a simple parser that splits on whitespace and handles basic quoting.
fn parse_command(command: &str) -> Result<ParsedCommand, anyhow::Error> {
    let command = command.trim();
    if command.is_empty() {
        return Err(anyhow::anyhow!("Empty command"));
    }

    // Check for shell metacharacters that we don't support
    if command.contains(&['|', '>', '<', '&', ';', '`', '$', '(', ')'][..]) {
        return Err(anyhow::anyhow!(
            "Command contains shell metacharacters which are not supported for security reasons"
        ));
    }

    let mut parts = Vec::new();
    let mut current_part = String::new();
    let mut in_quotes = false;
    let mut quote_char = ' ';
    let chars = command.chars();

    for ch in chars {
        match ch {
            '"' | '\'' if !in_quotes => {
                in_quotes = true;
                quote_char = ch;
            }
            ch if in_quotes && ch == quote_char => {
                in_quotes = false;
                quote_char = ' ';
            }
            ' ' | '\t' if !in_quotes => {
                if !current_part.is_empty() {
                    parts.push(current_part.clone());
                    current_part.clear();
                }
            }
            _ => {
                current_part.push(ch);
            }
        }
    }

    if in_quotes {
        return Err(anyhow::anyhow!("Unterminated quote in command"));
    }

    if !current_part.is_empty() {
        parts.push(current_part);
    }

    if parts.is_empty() {
        return Err(anyhow::anyhow!("No executable found in command"));
    }

    let executable = parts[0].clone();
    let args = parts[1..].to_vec();

    Ok(ParsedCommand { executable, args })
}

/// Check if a command is allowed based on the allowlist.
fn is_command_allowed(command: &str) -> bool {
    let command = command.trim();

    ALLOWED_COMMAND_PREFIXES
        .iter()
        .any(|prefix| command.starts_with(prefix))
}
