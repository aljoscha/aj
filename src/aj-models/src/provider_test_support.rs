use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub(crate) struct HeldSseServer {
    pub(crate) base_url: String,
    release: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl HeldSseServer {
    pub(crate) async fn finish(self) {
        let _ = self.release.send(());
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("fixture server request completed within 5 seconds")
            .expect("fixture server task");
    }
}

pub(crate) async fn held_sse_server(path: &'static str, events: Vec<String>) -> HeldSseServer {
    held_sse_server_with_body(path, events, BodyEnd::Clean).await
}

/// Hold a streaming response open after valid SSE events, then end its body
/// short of the declared Content-Length when the fixture is released.
pub(crate) async fn failing_sse_server(path: &'static str, events: Vec<String>) -> HeldSseServer {
    held_sse_server_with_body(path, events, BodyEnd::Short).await
}

#[derive(Clone, Copy)]
enum BodyEnd {
    Clean,
    Short,
}

async fn held_sse_server_with_body(
    path: &'static str,
    events: Vec<String>,
    body_end: BodyEnd,
) -> HeldSseServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture server");
    let address = listener.local_addr().expect("fixture address");
    let (release, held) = oneshot::channel();
    let task = tokio::spawn(async move {
        let body = events
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        let (mut socket, _) = listener.accept().await.expect("accept provider request");
        let mut request = vec![0; 32 * 1024];
        let read = socket
            .read(&mut request)
            .await
            .expect("read provider request");
        assert!(
            String::from_utf8_lossy(&request[..read]).contains(path),
            "provider request did not target {path}"
        );
        let content_length = match body_end {
            BodyEnd::Clean => String::new(),
            BodyEnd::Short => format!("content-length: {}\r\n", body.len() + 1),
        };
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     {content_length}connection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write response head");
        socket
            .write_all(body.as_bytes())
            .await
            .expect("write SSE events");
        let _ = held.await;
    });
    HeldSseServer {
        base_url: format!("http://{address}"),
        release,
        task,
    }
}
