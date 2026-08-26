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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture server");
    let address = listener.local_addr().expect("fixture address");
    let (release, held) = oneshot::channel();
    let task = tokio::spawn(async move {
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
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                  connection: close\r\n\r\n",
            )
            .await
            .expect("write response head");
        for event in events {
            socket
                .write_all(format!("data: {event}\n\n").as_bytes())
                .await
                .expect("write SSE event");
        }
        let _ = held.await;
    });
    HeldSseServer {
        base_url: format!("http://{address}"),
        release,
        task,
    }
}
