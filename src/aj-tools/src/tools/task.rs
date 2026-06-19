//! `task_output` / `task_stop` builtins — observe and stop background
//! tasks.
//!
//! Both are thin wrappers over the shared [`TaskRegistry`]: reads are
//! stateless (repeated calls return overlapping tails; incremental
//! consumption goes through `read_file` on the spill path), and both
//! run with the default [`ExecutionMode::Parallel`] so they never
//! serialize a tool batch.
//!
//! [`ExecutionMode::Parallel`]: aj_agent::tool::ExecutionMode::Parallel

use std::time::Duration;

use aj_agent::TaskRegistry;
use aj_agent::tool::{
    BashStreamTruncation, TaskId, TaskKind, TaskStatus, ToolContext, ToolDefinition, ToolDetails,
    ToolOutcome,
};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tools::bash::{render_stream_block, task_status_text};
use crate::truncate::{BASH_MAX_BYTES, BASH_MAX_LINES, TruncatedBy, truncate_tail};

const OUTPUT_DESCRIPTION: &str = r#"
Read the current output and status of a background task.

- With block: true (the default) the call waits until the task reaches a
  terminal status or the timeout elapses, whichever comes first; it returns
  immediately if the task already finished. Hitting the timeout is not an
  error — the result is a normal "still running" report.
- Reads are stateless: repeated calls return overlapping output tails. For
  incremental consumption of long or fast-moving output, use read_file with
  offset/limit on the task's output path and track your own position.
- Don't babysit a running task with repeated blocking calls; you will be
  notified when it completes.
"#;

const STOP_DESCRIPTION: &str = r#"
Stop a background task. Kills the task's process group, waits briefly for it
to terminate, and reports the final status and output tail. Stopping an
already-finished task is not an error — it reports the terminal status.
"#;

/// Bounded grace `task_stop` waits for the driver to flip the status
/// after firing the task's cancel token. Drivers respond promptly
/// (SIGKILL on the process group + reap), so this only guards against
/// a wedged driver; on expiry we report whatever status we see.
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct TaskOutputTool;

#[derive(JsonSchema, Deserialize, Clone, Debug)]
pub struct TaskOutputInput {
    /// Id of the background task to read.
    pub id: TaskId,
    /// Wait for the task to finish before returning (default: true).
    /// When false, return an immediate status snapshot.
    #[serde(default = "default_block")]
    pub block: bool,
    /// Maximum seconds to wait when blocking (default: 30). Hitting
    /// the timeout is not an error; the result reports the task as
    /// still running.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_block() -> bool {
    true
}

fn default_timeout() -> u64 {
    30
}

impl ToolDefinition for TaskOutputTool {
    type Input = TaskOutputInput;

    fn name(&self) -> &'static str {
        "task_output"
    }

    fn description(&self) -> &'static str {
        OUTPUT_DESCRIPTION
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> Result<ToolOutcome, aj_agent::BoxError> {
        let registry = ctx.task_registry();
        let Some(status) = registry.status(input.id) else {
            return Ok(unknown_id_outcome(&registry, input.id));
        };
        if input.block && !status.is_terminal() {
            // The turn token cancels the *wait*, never the task: a
            // Ctrl+C during a blocking read resolves this call and
            // leaves the task running. The timeout arm is the normal
            // "still running" path, not an error.
            let cancel = ctx.cancellation();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {}
                _ = registry.wait_terminal(input.id) => {}
                _ = tokio::time::sleep(Duration::from_secs(input.timeout)) => {}
            }
        }
        Ok(report_outcome(&registry, input.id))
    }
}

#[derive(Clone)]
pub struct TaskStopTool;

#[derive(JsonSchema, Deserialize, Clone, Debug)]
pub struct TaskStopInput {
    /// Id of the background task to stop.
    pub id: TaskId,
}

impl ToolDefinition for TaskStopTool {
    type Input = TaskStopInput;

    fn name(&self) -> &'static str {
        "task_stop"
    }

    fn description(&self) -> &'static str {
        STOP_DESCRIPTION
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> Result<ToolOutcome, aj_agent::BoxError> {
        let registry = ctx.task_registry();
        let Some(status) = registry.status(input.id) else {
            return Ok(unknown_id_outcome(&registry, input.id));
        };
        if !status.is_terminal() {
            registry.kill(input.id);
            let _ = tokio::time::timeout(STOP_GRACE, registry.wait_terminal(input.id)).await;
        }
        Ok(report_outcome(&registry, input.id))
    }
}

/// Compose the status + rolling-tail report for task `id`.
///
/// Bash-backed tasks render through the standard bash budgets and
/// markers onto [`ToolDetails::Bash`] (with `task_id` set and
/// `exit_code` populated once terminal); agent-backed tasks render
/// what [`aj_agent::tool::TaskRead`] gives as [`ToolDetails::Text`].
fn report_outcome(registry: &TaskRegistry, id: TaskId) -> ToolOutcome {
    let Some(summary) = registry.summary(id) else {
        return unknown_id_outcome(registry, id);
    };
    let Some((status, read)) = registry.read(id) else {
        return unknown_id_outcome(registry, id);
    };

    let header = match status {
        TaskStatus::Running => format!("Background task #{id} still running: {}", summary.label),
        terminal => format!(
            "Background task #{id} finished: {} — {}",
            summary.label,
            task_status_text(terminal)
        ),
    };

    if let TaskKind::Agent { agent_id, .. } = &summary.kind {
        // Agent-backed tasks have no process streams to tail. While
        // the run is live the body points at the sub-agent's chat;
        // once terminal the driver has stored the final report on the
        // output source, so a read returns it (the same text the
        // completion notice delivers).
        let body = match read.report {
            Some(report) if status.is_terminal() => report,
            _ => format!("Sub-agent {agent_id} runs this task in its own chat."),
        };
        return ToolOutcome {
            content: vec![UserContent::text(format!("{header}\n{body}"))],
            details: ToolDetails::Text {
                summary: header,
                body,
            },
            is_error: false,
        };
    }

    let (stdout_str, stdout_truncation) = truncate_stream_tail(&read.stdout_tail);
    let (stderr_str, stderr_truncation) = truncate_stream_tail(&read.stderr_tail);
    let block = render_stream_block(
        &stdout_str,
        &stderr_str,
        stdout_truncation.as_ref(),
        stderr_truncation.as_ref(),
        read.spill_path.as_deref(),
    );

    let mut wire = header;
    if !block.is_empty() {
        wire.push('\n');
        wire.push_str(&block);
    }
    if let Some(path) = &read.spill_path {
        if !wire.ends_with('\n') {
            wire.push('\n');
        }
        wire.push_str(&format!("Full output: {}", path.display()));
    }

    let exit_code = match status {
        TaskStatus::Exited(code) => code,
        TaskStatus::Running | TaskStatus::Killed => None,
    };
    ToolOutcome {
        content: vec![UserContent::text(wire)],
        details: ToolDetails::Bash {
            command: summary.label,
            stdout: stdout_str,
            stderr: stderr_str,
            exit_code,
            truncated: stdout_truncation.is_some() || stderr_truncation.is_some(),
            full_output_path: read.spill_path,
            stdout_truncation,
            stderr_truncation,
            task_id: Some(id),
        },
        is_error: false,
    }
}

/// Apply the standard bash display budgets to a rolling tail.
///
/// NOTE: the truncation summary counts the rolling tail, not the full
/// stream — the tail is the only window a stateless read has. The
/// exact byte totals and the full output live on the spill file.
#[allow(clippy::as_conversions)]
fn truncate_stream_tail(tail: &str) -> (String, Option<BashStreamTruncation>) {
    let tt = truncate_tail(tail, BASH_MAX_LINES, BASH_MAX_BYTES);
    if !tt.truncated {
        return (tt.content, None);
    }
    let last_line_bytes = tail
        .rsplit('\n')
        .next()
        .map(|l| l.len() as u64)
        .unwrap_or(0);
    let summary = BashStreamTruncation {
        total_lines: tt.total_lines as u64,
        total_bytes: tt.total_bytes as u64,
        output_lines: tt.output_lines as u64,
        output_bytes: tt.output_bytes as u64,
        truncated_by: tt.truncated_by.unwrap_or(TruncatedBy::Bytes),
        last_line_partial: tt.last_line_partial,
        last_line_bytes,
    };
    (tt.content, Some(summary))
}

/// `is_error` outcome for an id the registry doesn't know, listing the
/// live task ids so the model can correct itself.
fn unknown_id_outcome(registry: &TaskRegistry, id: TaskId) -> ToolOutcome {
    let live: Vec<String> = registry
        .snapshot()
        .into_iter()
        .filter(|s| !s.status.is_terminal())
        .map(|s| format!("#{} ({})", s.id, s.label))
        .collect();
    let live_text = if live.is_empty() {
        "none".to_string()
    } else {
        live.join(", ")
    };
    let body = format!("No background task with id {id}. Live tasks: {live_text}");
    ToolOutcome {
        content: vec![UserContent::text(body.clone())],
        details: ToolDetails::Text {
            summary: format!("task #{id}: unknown id"),
            body,
        },
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use aj_agent::TaskRegistry;
    use aj_agent::tool::TaskId;

    use aj_models::types::UserContent;

    use super::*;
    use crate::testing::DummyToolContext;
    use crate::tools::bash::{BashInput, BashTool};

    fn extract_text(content: &[UserContent]) -> String {
        content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text.as_str()),
                UserContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Start `command` as a background bash task on `ctx`, returning
    /// the task id and spill path.
    async fn start_background(ctx: &mut DummyToolContext, command: &str) -> (TaskId, PathBuf) {
        let outcome = BashTool
            .execute(
                ctx,
                BashInput {
                    command: command.to_string(),
                    timeout: 30,
                    description: "test background".to_string(),
                    run_in_background: true,
                },
            )
            .await
            .expect("execute bash");
        match &outcome.details {
            ToolDetails::Bash {
                task_id: Some(id),
                full_output_path: Some(path),
                ..
            } => (*id, path.clone()),
            other => panic!("expected started Bash details, got {other:?}"),
        }
    }

    async fn read_task(
        ctx: &mut DummyToolContext,
        id: TaskId,
        block: bool,
        timeout: u64,
    ) -> ToolOutcome {
        TaskOutputTool
            .execute(ctx, TaskOutputInput { id, block, timeout })
            .await
            .expect("task_output executes")
    }

    /// Poll `cond` until it holds (bounded), yielding to the runtime
    /// so detached drivers and reader tasks make progress.
    async fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    fn spill_contains(registry: &TaskRegistry, id: TaskId, needle: &str) -> bool {
        let Some((_, read)) = registry.read(id) else {
            return false;
        };
        read.stdout_tail.contains(needle)
    }

    /// Reads are stateless: two reads of a running task return
    /// overlapping tails (the second still contains the first's
    /// content).
    #[tokio::test]
    async fn stateless_reads_return_overlapping_tails() {
        let mut ctx = DummyToolContext::default();
        let (id, spill) =
            start_background(&mut ctx, "echo one; sleep 0.2; echo two; sleep 30").await;
        let registry = ctx.task_registry();

        wait_for(|| spill_contains(&registry, id, "one"), "first line").await;
        let first = read_task(&mut ctx, id, false, 30).await;
        assert!(!first.is_error);
        let first_wire = extract_text(&first.content);
        assert!(first_wire.contains("still running"), "wire: {first_wire:?}");
        assert!(first_wire.contains("one"), "wire: {first_wire:?}");

        wait_for(|| spill_contains(&registry, id, "two"), "second line").await;
        let second = read_task(&mut ctx, id, false, 30).await;
        let second_wire = extract_text(&second.content);
        // Overlap: the second read repeats the first read's content.
        assert!(second_wire.contains("one"), "wire: {second_wire:?}");
        assert!(second_wire.contains("two"), "wire: {second_wire:?}");

        registry.kill(id);
        std::fs::remove_file(spill).ok();
    }

    /// A blocking read resolves when the task exits, reporting the
    /// exit code and the final tail on `ToolDetails::Bash`.
    #[tokio::test]
    async fn block_resolves_on_terminal_status() {
        let mut ctx = DummyToolContext::default();
        let (id, spill) = start_background(&mut ctx, "sleep 0.2; echo done; exit 3").await;

        let outcome = read_task(&mut ctx, id, true, 30).await;
        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("exit code 3"), "wire: {wire:?}");
        assert!(wire.contains("done"), "wire: {wire:?}");
        assert!(
            wire.contains(&format!("Full output: {}", spill.display())),
            "wire: {wire:?}"
        );
        match &outcome.details {
            ToolDetails::Bash {
                exit_code: Some(3),
                task_id: Some(tid),
                full_output_path: Some(_),
                ..
            } => assert_eq!(*tid, id),
            other => panic!("expected terminal Bash details, got {other:?}"),
        }

        std::fs::remove_file(spill).ok();
    }

    /// Hitting the blocking timeout is a normal "still running"
    /// report, not an error.
    #[tokio::test]
    async fn block_timeout_is_not_an_error() {
        let mut ctx = DummyToolContext::default();
        let (id, spill) = start_background(&mut ctx, "sleep 30").await;

        let outcome = read_task(&mut ctx, id, true, 1).await;
        assert!(!outcome.is_error, "timeout must not be an error");
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("still running"), "wire: {wire:?}");
        match &outcome.details {
            ToolDetails::Bash {
                exit_code: None,
                task_id: Some(tid),
                ..
            } => assert_eq!(*tid, id),
            other => panic!("expected running Bash details, got {other:?}"),
        }

        ctx.task_registry().kill(id);
        std::fs::remove_file(spill).ok();
    }

    /// Cancelling the turn cancels the blocking wait, not the task:
    /// the call resolves promptly and the task keeps running.
    #[tokio::test]
    async fn turn_cancel_cancels_the_wait_not_the_task() {
        let mut ctx = DummyToolContext::default();
        let (id, spill) = start_background(&mut ctx, "sleep 30").await;

        let token = ctx.cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            token.cancel();
        });

        let start = Instant::now();
        let _outcome = read_task(&mut ctx, id, true, 30).await;
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "cancel should resolve the wait promptly"
        );
        assert_eq!(
            ctx.task_registry().status(id),
            Some(TaskStatus::Running),
            "the task itself must keep running"
        );

        ctx.task_registry().kill(id);
        std::fs::remove_file(spill).ok();
    }

    /// Tails larger than the bash budgets pick up the standard
    /// `[Showing lines ...]` markers, with the spill path attached.
    #[tokio::test]
    async fn long_tail_gets_truncation_markers() {
        let mut ctx = DummyToolContext::default();
        // ~200KB of stdout overflows the 50KB display budget; the
        // command exits so the read sees the final tail.
        let (id, spill) = start_background(&mut ctx, "yes ABCDEFGH | head -c 200000").await;

        let outcome = read_task(&mut ctx, id, true, 30).await;
        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("[Showing lines "), "wire tail: {wire:?}");
        assert!(wire.contains(" of stdout"), "wire tail: {wire:?}");
        match &outcome.details {
            ToolDetails::Bash {
                truncated: true,
                stdout_truncation: Some(t),
                ..
            } => {
                assert!(t.output_bytes <= 50 * 1024);
            }
            other => panic!("expected truncated Bash details, got {other:?}"),
        }

        std::fs::remove_file(spill).ok();
    }

    /// Unknown ids are an error outcome that lists the live task ids
    /// so the model can correct itself.
    #[tokio::test]
    async fn unknown_id_lists_live_tasks() {
        let mut ctx = DummyToolContext::default();
        let (id, spill) = start_background(&mut ctx, "sleep 30").await;

        let outcome = read_task(&mut ctx, 999, false, 30).await;
        assert!(outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("No background task with id 999"),
            "wire: {wire:?}"
        );
        assert!(
            wire.contains(&format!("#{id} (sleep 30)")),
            "wire: {wire:?}"
        );

        // task_stop reports unknown ids the same way.
        let stop = TaskStopTool
            .execute(&mut ctx, TaskStopInput { id: 999 })
            .await
            .expect("task_stop executes");
        assert!(stop.is_error);

        ctx.task_registry().kill(id);
        std::fs::remove_file(spill).ok();
    }

    /// `task_stop` kills the whole process group: a grandchild
    /// spawned by the shell dies with it.
    #[tokio::test]
    async fn task_stop_kills_the_process_group() {
        let mut ctx = DummyToolContext::default();
        // Unique sleep duration so pgrep only matches this test's
        // grandchild even when tests run in parallel.
        let marker = format!("60.{}", std::process::id());
        let command = format!("sleep {marker} & wait");
        let (id, spill) = start_background(&mut ctx, &command).await;

        let grandchild_alive = || {
            std::process::Command::new("pgrep")
                .args(["-f", &format!("sleep {marker}")])
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false)
        };
        wait_for(grandchild_alive, "grandchild to spawn").await;

        let outcome = TaskStopTool
            .execute(&mut ctx, TaskStopInput { id })
            .await
            .expect("task_stop executes");
        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("killed"), "wire: {wire:?}");
        assert_eq!(ctx.task_registry().status(id), Some(TaskStatus::Killed));

        // The process-group SIGKILL reaches the shell's grandchild.
        wait_for(|| !grandchild_alive(), "grandchild to die").await;

        std::fs::remove_file(spill).ok();
    }

    /// Stopping an already-finished task is a normal report of its
    /// terminal status, not an error.
    #[tokio::test]
    async fn task_stop_on_terminal_task_reports_normally() {
        let mut ctx = DummyToolContext::default();
        let (id, spill) = start_background(&mut ctx, "echo hi").await;
        let registry = ctx.task_registry();
        tokio::time::timeout(Duration::from_secs(10), registry.wait_terminal(id))
            .await
            .expect("task terminates")
            .expect("task id known");

        let outcome = TaskStopTool
            .execute(&mut ctx, TaskStopInput { id })
            .await
            .expect("task_stop executes");
        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("exit code 0"), "wire: {wire:?}");

        std::fs::remove_file(spill).ok();
    }

    /// Output source standing in for an agent-backed task: no
    /// streams, just an optional report like the production driver
    /// stores at finish.
    struct StubAgentOutput {
        report: std::sync::Mutex<Option<String>>,
    }

    impl aj_agent::tool::TaskOutputSource for StubAgentOutput {
        fn snapshot(&self) -> aj_agent::tool::TaskRead {
            aj_agent::tool::TaskRead {
                report: self.report.lock().unwrap().clone(),
                ..aj_agent::tool::TaskRead::default()
            }
        }
    }

    /// Agent-backed tasks read as a status line while running and as
    /// the final report (`ToolDetails::Text`) once terminal.
    #[tokio::test]
    async fn agent_task_reads_status_then_report() {
        use aj_agent::events::AgentId;
        use aj_agent::tool::{TaskKind, TaskStatus};
        use std::sync::Arc;

        let mut ctx = DummyToolContext::default();
        let registry = ctx.task_registry();
        let output = Arc::new(StubAgentOutput {
            report: std::sync::Mutex::new(None),
        });
        let output_dyn: Arc<dyn aj_agent::tool::TaskOutputSource> =
            Arc::<StubAgentOutput>::clone(&output);
        let (id, _cancel) = registry.register(
            AgentId::Main,
            TaskKind::Agent {
                agent_id: 2,
                task: "investigate".to_string(),
            },
            "agent 2".to_string(),
            output_dyn,
        );

        // Running: a status line pointing at the sub-agent's chat.
        let running = read_task(&mut ctx, id, false, 30).await;
        assert!(!running.is_error);
        let wire = extract_text(&running.content);
        assert!(wire.contains("still running: agent 2"), "wire: {wire:?}");
        assert!(
            wire.contains("runs this task in its own chat"),
            "wire: {wire:?}"
        );

        // Terminal: the stored report is the body.
        *output.report.lock().unwrap() = Some("the final report".to_string());
        registry.set_status(id, TaskStatus::Exited(Some(0)));
        let finished = read_task(&mut ctx, id, false, 30).await;
        assert!(!finished.is_error);
        let wire = extract_text(&finished.content);
        assert!(
            wire.contains("finished: agent 2 \u{2014} exit code 0"),
            "wire: {wire:?}"
        );
        assert!(wire.contains("the final report"), "wire: {wire:?}");
        match &finished.details {
            ToolDetails::Text { body, .. } => assert_eq!(body, "the final report"),
            other => panic!("expected Text details, got {other:?}"),
        }
    }
}
