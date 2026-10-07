// Added for AJ: acknowledged stored-value runtime contracts (Apache-2.0).
use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use codex_code_mode_runtime::*;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

type Writes = HashMap<String, Arc<Value>>;

struct StoreDelegate {
    entered: mpsc::UnboundedSender<(CellId, Writes)>,
    release: Notify,
    result: Result<(), String>,
}

impl StoreDelegate {
    fn new(result: Result<(), String>) -> (Arc<Self>, mpsc::UnboundedReceiver<(CellId, Writes)>) {
        let (entered, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                entered,
                release: Notify::new(),
                result,
            }),
            receiver,
        )
    }
}

impl CodeModeSessionDelegate for StoreDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _: CodeModeNestedToolCall,
        _: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async { panic!("unexpected tool call") })
    }

    fn notify<'a>(
        &'a self,
        _: String,
        _: CellId,
        _: String,
        _: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn store<'a>(&'a self, cell_id: CellId, writes: Writes) -> NotificationFuture<'a> {
        Box::pin(async move {
            self.entered.send((cell_id, writes)).unwrap();
            self.release.notified().await;
            self.result.clone()
        })
    }

    fn cell_closed(&self, _: &CellId) {}
}

fn request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "store-test".into(),
        enabled_tools: vec![],
        source: source.into(),
        yield_time_ms: Some(60_000),
        max_output_tokens: None,
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("runtime timeout")
}

fn completed(response: RuntimeResponse, output: &str) -> Option<String> {
    let RuntimeResponse::Result {
        content_items,
        error_text,
        ..
    } = response
    else {
        panic!("expected completed response, got {response:?}");
    };
    assert_eq!(
        content_items,
        vec![FunctionCallOutputContentItem::InputText {
            text: output.into()
        }]
    );
    error_text
}

async fn read(session: &InProcessCodeModeSession, source: &str) -> RuntimeResponse {
    bounded(session.execute(request(source), Arc::new(NoopCodeModeSessionDelegate), None))
        .await
        .unwrap()
        .initial_response()
        .await
        .unwrap()
}

#[tokio::test]
async fn termination_waits_for_reserved_commit_and_readers_wait_for_ack() {
    let session = InProcessCodeModeSession::new();
    let (delegate, mut entered) = StoreDelegate::new(Ok(()));
    let cell = session
        .execute(
            request(r#"store("key", "saved"); text("output");"#),
            delegate.clone(),
            None,
        )
        .await
        .unwrap();
    let (id, writes) = bounded(entered.recv()).await.unwrap();
    assert_eq!(id, cell.cell_id);
    assert_eq!(
        writes,
        HashMap::from([("key".into(), Arc::new(json!("saved")))])
    );

    let mut termination = Box::pin(session.terminate(id.clone()));
    assert!(
        poll_fn(|cx| Poll::Ready(termination.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert!(
        bounded(session.terminate(id))
            .await
            .unwrap_err()
            .contains("already terminating")
    );
    let mut reader = Box::pin(session.execute(
        request(r#"text(load("key"));"#),
        Arc::new(NoopCodeModeSessionDelegate),
        None,
    ));
    assert!(
        poll_fn(|cx| Poll::Ready(reader.as_mut().poll(cx)))
            .await
            .is_pending(),
        "reader took an unacknowledged snapshot"
    );

    delegate.release.notify_one();
    let WaitOutcome::LiveCell(response) = bounded(termination).await.unwrap() else {
        panic!("cell disappeared")
    };
    assert_eq!(completed(response, "output"), None);
    assert_eq!(
        completed(bounded(cell.initial_response()).await.unwrap(), "output"),
        None
    );
    let reader = bounded(reader).await.unwrap();
    assert_eq!(
        completed(bounded(reader.initial_response()).await.unwrap(), "saved"),
        None
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_store_preserves_script_error_output_and_previous_values() {
    let session =
        InProcessCodeModeSession::with_stored_values(HashMap::from([("key".into(), json!("old"))]));
    let (delegate, mut entered) = StoreDelegate::new(Err("disk unavailable".into()));
    let cell = session.execute(request(r#"store("key", "new"); store("absent", 1); text("output"); throw Error("script failed");"#), delegate.clone(), None).await.unwrap();
    let (_, writes) = bounded(entered.recv()).await.unwrap();
    assert_eq!(writes.len(), 2);
    delegate.release.notify_one();
    let error = completed(bounded(cell.initial_response()).await.unwrap(), "output").unwrap();
    assert!(error.contains("script failed"), "{error}");
    assert!(
        error.contains("stored values were not saved: disk unavailable"),
        "{error}"
    );
    assert_eq!(
        completed(
            read(
                &session,
                r#"text(load("key") + ":" + String(load("absent")));"#
            )
            .await,
            "old:undefined"
        ),
        None
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn restored_values_are_available_to_first_cell() {
    let session = InProcessCodeModeSession::with_stored_values(HashMap::from([(
        "key".into(),
        json!({"nested": [1, true, null]}),
    )]));
    assert_eq!(
        completed(
            read(&session, r#"text(JSON.stringify(load("key")));"#).await,
            r#"{"nested":[1,true,null]}"#
        ),
        None
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn uncaught_script_error_still_commits_writes() {
    let session = InProcessCodeModeSession::new();
    let (delegate, mut entered) = StoreDelegate::new(Ok(()));
    let cell = session
        .execute(
            request(r#"store("key", "saved"); text("output"); throw Error("script failed");"#),
            delegate.clone(),
            None,
        )
        .await
        .unwrap();
    let (_, writes) = bounded(entered.recv()).await.unwrap();
    assert_eq!(*writes["key"], json!("saved"));
    delegate.release.notify_one();
    assert!(
        completed(bounded(cell.initial_response()).await.unwrap(), "output")
            .unwrap()
            .contains("script failed")
    );
    assert_eq!(
        completed(read(&session, r#"text(load("key"));"#).await, "saved"),
        None
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_reserved_commit() {
    let session = InProcessCodeModeSession::new();
    let (delegate, mut entered) = StoreDelegate::new(Ok(()));
    let cell = session
        .execute(
            request(r#"store("key", "saved"); text("output");"#),
            delegate.clone(),
            None,
        )
        .await
        .unwrap();
    bounded(entered.recv()).await.unwrap();
    let mut shutdown = Box::pin(session.shutdown());
    assert!(
        poll_fn(|cx| Poll::Ready(shutdown.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    delegate.release.notify_one();
    assert_eq!(
        completed(bounded(cell.initial_response()).await.unwrap(), "output"),
        None
    );
    bounded(shutdown).await.unwrap();
}
