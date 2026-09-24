use super::*;
use crate::remote::tests::{HostHandles, addr, bounded, scripted, scripted_host};
use crate::remote::{IdentityGate, RemoteServer};
use aj_agent::tool::{TaskKind, TaskOutputSource, TaskRead, TaskStatus};
use aj_wire::{TASK_OUTPUT_CAPABILITY, TASK_OUTPUT_CHUNK_BYTES};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

struct Spill(Option<PathBuf>);

impl TaskOutputSource for Spill {
    fn snapshot(&self) -> TaskRead {
        TaskRead {
            stdout_tail: "only the rolling tail".into(),
            spill_path: self.0.clone(),
            ..Default::default()
        }
    }
}

#[tokio::test]
async fn task_output_adapters_read_bytes_append_completion_and_errors() {
    bounded("task output adapters", async {
        let dir = tempfile::tempdir().unwrap();
        super::history_tests::write_prompts(
            &dir.path().join("sessions"),
            "cold",
            &[("stored session", 1)],
        );
        let host = scripted_host(
            &dir,
            scripted(vec![], 0, Duration::ZERO),
            HostHandles::new(&dir),
            None,
        );
        let session = host.create().await.unwrap();
        let registry = host.local_handles(&session).await.unwrap().task_registry;
        let path = dir.path().join("output");
        let mut expected: Vec<u8> = (0..TASK_OUTPUT_CHUNK_BYTES * 2 + 17)
            .map(|i| u8::try_from(i % 256).unwrap())
            .collect();
        expected[TASK_OUTPUT_CHUNK_BYTES - 1..TASK_OUTPUT_CHUNK_BYTES + 2]
            .copy_from_slice("€".as_bytes());
        std::fs::write(&path, &expected).unwrap();
        let register = |path| {
            registry
                .register_unowned_for_test(
                    AgentId::Main,
                    "call-output".into(),
                    TaskKind::Bash {
                        command: "fixture".into(),
                    },
                    "fixture".into(),
                    Arc::new(Spill(path)),
                )
                .0
        };
        let task = register(Some(path.clone()));
        let no_spill = register(None);
        let unreadable = register(Some(dir.path().join("missing")));
        registry.set_status(no_spill, TaskStatus::Exited(Some(0)));
        registry.set_status(unreadable, TaskStatus::Exited(Some(0)));
        let server = RemoteServer::bind(host.clone(), addr("127.0.0.1:0"), IdentityGate::local())
            .await
            .unwrap();
        let client = RemoteClient::new(&server.url()).unwrap();
        assert!(
            client
                .hello()
                .await
                .unwrap()
                .capabilities
                .iter()
                .any(|c| c == TASK_OUTPUT_CAPABILITY)
        );
        let local = Control::local(host.clone());
        let remote = Control::remote(client);
        for control in [&local, &remote] {
            assert_ne!(
                host.task(&session, task)
                    .await
                    .unwrap()
                    .stdout_tail
                    .as_bytes(),
                expected
            );
            let mut collected = Vec::new();
            while collected.len() < expected.len() {
                let offset = u64::try_from(collected.len()).unwrap();
                let chunk = control.task_output(&session, task, offset).await.unwrap();
                assert_eq!(chunk.id, task);
                assert_eq!(chunk.status, TaskStatus::Running);
                assert_eq!(chunk.offset, offset);
                assert_eq!(chunk.total_bytes, u64::try_from(expected.len()).unwrap());
                assert!(!chunk.bytes.is_empty(), "a read before EOF makes progress");
                assert!(chunk.bytes.len() <= TASK_OUTPUT_CHUNK_BYTES);
                collected.extend(chunk.bytes);
            }
            assert_eq!(collected, expected);
            assert!(
                control
                    .task_output(&session, task, u64::try_from(expected.len()).unwrap())
                    .await
                    .unwrap()
                    .bytes
                    .is_empty()
            );
            for (id, offset, code, message) in [
                (task, u64::MAX, "invalid_request", "exceeds current length"),
                (no_spill, 0, "unsupported", "no spill file"),
                (unreadable, 0, "unsupported", "full output unavailable"),
                (usize::MAX, 0, "unknown_task", "unknown background task"),
            ] {
                let err = control.task_output(&session, id, offset).await.unwrap_err();
                let actual = match &err {
                    ControlError::Host(err) => Some(err.code()),
                    ControlError::Remote(err) => err.code(),
                    _ => None,
                };
                assert_eq!(actual, Some(code), "{err}");
                assert!(err.to_string().contains(message), "{err}");
            }
            let cold = control.task_output("cold", task, 0).await.unwrap_err();
            assert!(cold.to_string().contains("unknown background task"));
            assert!(
                !host
                    .sessions()
                    .await
                    .unwrap()
                    .sessions
                    .iter()
                    .find(|row| row.id == "cold")
                    .unwrap()
                    .live
            );
        }
        let offset = u64::try_from(expected.len()).unwrap();
        let appended = b"\xfffinal output\n";
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(appended)
            .unwrap();
        for control in [&local, &remote] {
            let chunk = control.task_output(&session, task, offset).await.unwrap();
            assert_eq!(chunk.status, TaskStatus::Running);
            assert_eq!(
                chunk.total_bytes,
                offset + u64::try_from(appended.len()).unwrap()
            );
            assert_eq!(chunk.bytes, appended);
        }
        registry.set_status(task, TaskStatus::Exited(Some(0)));
        for control in [&local, &remote] {
            let chunk = control.task_output(&session, task, offset).await.unwrap();
            assert_eq!(chunk.status, TaskStatus::Exited(Some(0)));
            assert_eq!(chunk.bytes, appended);
        }
        let http = reqwest::Client::new();
        for query in [
            "",
            "?offset=-1",
            "?offset=18446744073709551616",
            "?offset=0&path=/etc/passwd",
        ] {
            let response = http
                .get(format!(
                    "{}/v1/sessions/{session}/tasks/{task}/output{query}",
                    server.url()
                ))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
            assert_eq!(
                response
                    .json::<aj_wire::ErrorResponse>()
                    .await
                    .unwrap()
                    .code,
                "invalid_request"
            );
        }
        host.shutdown().await;
        server.shutdown().await;
    })
    .await;
}
