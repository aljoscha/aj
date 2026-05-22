//! Single-slot hook surface for agent runtime extension.
//!
//! Three hooks bracket the per-turn / per-tool flow inside
//! [`crate::Agent::execute_turn`]:
//!
//! - [`BeforeToolCallHook`] — fires after [`crate::events::AgentEvent::ToolExecutionStart`]
//!   and before the tool's `execute` closure runs. Can mutate the
//!   args (e.g. redaction) or short-circuit the call with a
//!   pre-baked [`crate::tool::ToolOutcome`] (permission denial,
//!   policy block).
//! - [`AfterToolCallHook`] — fires after the tool's `execute` returns
//!   and before [`crate::events::AgentEvent::ToolExecutionEnd`].
//!   Mutates the [`crate::tool::ToolOutcome`] in place — typical use
//!   is redaction, auto-truncation, or flipping an exit code.
//! - [`ShouldStopAfterTurnHook`] — fires after the post-tool-batch
//!   transcript update and before the next inference. Returning
//!   `true` ends the turn with no follow-up call (e.g. budget /
//!   context-window guard).
//!
//! All three are stored as `Option<Box<dyn Fn... + Send + Sync>>` —
//! one slot per hook, replacing on `set_*`. No registry, no
//! priority order; if a host wants to chain multiple effects it
//! composes them into a single closure.
//!
//! The hook closures are awaited inline by the agent (returning
//! `impl Future`), same as bus listeners, so they see a coherent
//! transcript snapshot and any blocking work they do delays the
//! next agent step. Hosts that need fire-and-forget side effects
//! should spawn a task inside the hook body.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::tool::ToolOutcome;

/// Owned future returned by every hook. We box-pin so the hook
/// surface stays object-safe.
type HookFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Information about the in-flight tool call passed to
/// [`BeforeToolCallHook`] and [`AfterToolCallHook`].
///
/// Carried by reference so the hook never owns the data; the agent
/// re-uses these fields for the surrounding bus events.
#[derive(Debug, Clone)]
pub struct ToolCallContext<'a> {
    pub call_id: &'a str,
    pub tool_name: &'a str,
}

/// Decision returned by [`BeforeToolCallHook`].
///
/// `Proceed` lets the tool's own `execute` run with the (possibly
/// mutated) arguments; `ShortCircuit` skips the tool and feeds the
/// pre-baked outcome into the rest of the flow as if the tool had
/// returned it.
pub enum BeforeToolCallOutcome {
    /// Allow the tool to execute. `args` is the (possibly mutated)
    /// JSON value the tool's deserializer will see.
    Proceed { args: Value },
    /// Skip the tool entirely. The agent treats `outcome` as if the
    /// tool had produced it: bus event, transcript, persistence
    /// all flow normally.
    ShortCircuit { outcome: ToolOutcome },
}

/// Closure invoked before each tool call.
///
/// Signature mirrors the bus-listener pattern: `Arc<dyn Fn ...>` so
/// the closure is sharable across spawned futures and tests.
pub type BeforeToolCallHook = Arc<
    dyn for<'a> Fn(ToolCallContext<'a>, Value) -> HookFuture<'a, BeforeToolCallOutcome>
        + Send
        + Sync,
>;

/// Closure invoked after each tool call returns.
///
/// The hook receives a mutable reference to the [`ToolOutcome`] and
/// may rewrite any of its fields before the bus event and the wire
/// projection fire.
pub type AfterToolCallHook = Arc<
    dyn for<'a> Fn(ToolCallContext<'a>, &'a mut ToolOutcome) -> HookFuture<'a, ()> + Send + Sync,
>;

/// Closure invoked after every assistant turn completes its tool
/// batch and before the agent decides whether to run another
/// inference. Returning `true` ends the turn without a follow-up
/// inference — useful for "stop before the context window fills"
/// guards or budget enforcement.
pub type ShouldStopAfterTurnHook = Arc<dyn Fn() -> HookFuture<'static, bool> + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn before_hook_can_short_circuit() {
        // Sanity check that the trait object shape compiles and a
        // simple closure can return `ShortCircuit` from the boxed
        // future without `Send` / `Sync` complaints.
        let hook: BeforeToolCallHook = Arc::new(|_ctx, _args| {
            Box::pin(async {
                BeforeToolCallOutcome::ShortCircuit {
                    outcome: ToolOutcome {
                        content: Vec::new(),
                        details: crate::tool::ToolDetails::Text {
                            summary: "denied".into(),
                            body: "permission denied".into(),
                        },
                        is_error: true,
                    },
                }
            })
        });
        let ctx = ToolCallContext {
            call_id: "tu_1",
            tool_name: "bash",
        };
        let outcome = hook(ctx, Value::Null).await;
        match outcome {
            BeforeToolCallOutcome::ShortCircuit { outcome } => assert!(outcome.is_error),
            BeforeToolCallOutcome::Proceed { .. } => panic!("expected ShortCircuit"),
        }
    }

    #[tokio::test]
    async fn after_hook_can_mutate_outcome_in_place() {
        let hook: AfterToolCallHook = Arc::new(|_ctx, outcome| {
            Box::pin(async move {
                outcome.is_error = true;
            })
        });
        let mut outcome = ToolOutcome {
            content: Vec::new(),
            details: crate::tool::ToolDetails::Text {
                summary: "ok".into(),
                body: "ok".into(),
            },
            is_error: false,
        };
        let ctx = ToolCallContext {
            call_id: "tu_1",
            tool_name: "bash",
        };
        hook(ctx, &mut outcome).await;
        assert!(outcome.is_error);
    }

    #[tokio::test]
    async fn should_stop_hook_returns_bool() {
        let hook: ShouldStopAfterTurnHook = Arc::new(|| Box::pin(async { true }));
        assert!(hook().await);
    }
}
