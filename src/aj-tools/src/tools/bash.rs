use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{SessionContext, ToolDefinition, ToolResult, TurnContext};
use aj_ui::AjUi;

const DESCRIPTION: &str = r#"
Execute a command in the system shell (bash). The command will be run in the
working directory of the agent session.

- There are no permissions checks or sandboxing. You are free to run any command
  you consider reasonable and safe.
- Commands have a configurable timeout to prevent hanging (default: 30s).
- Output is truncated to 35000 characters to prevent excessive output.
- The command is passed to `bash -c`, so pipes, redirects, and shell features work.
- IMPORTANT: Don't use search commands like find and grep. Instead use the grep and glob tools. Also don't use commands like cat and ls, instead use the read_file and ls tools.
- If you absolutely must use a grep-like tool, use ripgrep (rg).
"#;

#[derive(Clone)]
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

impl ToolDefinition for BashTool {
    type Input = BashInput;

    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    async fn execute(
        &self,
        session_ctx: &mut dyn SessionContext,
        _turn_ctx: &mut dyn TurnContext,
        ui: &mut dyn AjUi,
        input: Self::Input,
    ) -> Result<ToolResult, anyhow::Error> {
        let working_dir = session_ctx.working_directory();
        let child = Command::new("bash")
            .arg("-c")
            .arg(&input.command)
            .current_dir(&working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to start command '{}': {}", input.command, e))?;

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

                // Display the command and result using ui
                let display_command = format!("$ {}", input.command);
                ui.display_tool_result("bash", &display_command, &result);

                Ok(ToolResult::new(result))
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

                let error_msg = format!("Command timed out after {} seconds", input.timeout);
                Err(anyhow::anyhow!(error_msg))
            }
        }
    }
}
