//! Opt-in measurements over real host and gateway sockets, not timing assertions.

use std::cell::Cell;
use std::time::Instant;

use eventsource_stream::Eventsource;
use futures::{StreamExt, TryStreamExt};

use super::*;
use crate::gateway::{Gateway, GatewayServer, GatewaySetup, Tuning};

// Linux reports process CPU in clock ticks. Convert using `getconf CLK_TCK`,
// not a hardcoded frequency. The process must run this profile alone.
fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("process CPU counters");
    let fields: Vec<_> = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

fn socket_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("process descriptors")
        .map(|entry| entry.expect("descriptor entry"))
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .filter(|target| {
            target
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(b"socket:")
        })
        .count()
}

/// Run alone with `--ignored --exact --nocapture --test-threads=1`.
///
/// The local observer supplies a delivery-time reference without modifying the
/// provider or production writer. Its work is included in process CPU. Gaps
/// are observations, not publication timestamps, and can be slightly negative
/// when the local observer is scheduled after the HTTP reader.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "profiling: real streaming workload and process-wide CPU/socket measurements"]
async fn profile_stream_pacing() {
    for gateway in [false, true] {
        for sessions in [1, 4] {
            for (chunks, chunk_size, delay_ms) in [(256, 64, 0), (256, 64, 2), (64, 8, 20)] {
                profile(gateway, sessions, chunks, chunk_size, delay_ms).await;
            }
        }
    }
}

async fn profile(
    via_gateway: bool,
    sessions: usize,
    chunks: usize,
    chunk_size: usize,
    delay_ms: u64,
) {
    let sockets_before = socket_count();
    let reply: String = ('a'..='z').cycle().take(chunks * chunk_size).collect();
    let fixture = Fixture::with_provider(scripted(
        vec![finalized_text_message(&reply); sessions],
        chunk_size,
        Duration::from_millis(delay_ms),
    ))
    .await;
    let mut ids = Vec::new();
    for _ in 0..sessions {
        ids.push(fixture.create().await);
    }
    let gateway_dir = TempDir::new().expect("gateway state");
    let (gateway, base, wire_ids) = if via_gateway {
        let gateway = Gateway::new(GatewaySetup {
            state_dir: gateway_dir.path().to_path_buf(),
            static_hosts: Vec::new(),
            tuning: Tuning::default(),
        })
        .expect("gateway");
        let host = gateway.enroll(&fixture.server.url()).await.expect("enroll");
        bounded("gateway directory readiness", async {
            while gateway.sessions().sessions.len() != sessions
                || !gateway.hosts().hosts.iter().any(|host| host.connected)
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        let server =
            GatewayServer::bind(gateway.clone(), addr("127.0.0.1:0"), IdentityGate::local())
                .await
                .expect("gateway server");
        let wire_ids = ids
            .iter()
            .map(|id| format!("{}:{id}", host.id.as_deref().expect("checked host id")))
            .collect::<Vec<_>>();
        let base = server.url();
        (Some((gateway, server)), base, wire_ids)
    } else {
        (None, fixture.server.url(), ids.clone())
    };

    let requests: Vec<_> = ids.iter().map(|id| attach(id)).collect();
    let mut local = fixture
        .host
        .attach(&requests)
        .await
        .expect("local observer");
    bounded("local attach blocks", async {
        let mut caught_up = 0;
        while caught_up < sessions {
            if matches!(
                local.recv().await.expect("local frame"),
                Frame::CaughtUp { .. }
            ) {
                caught_up += 1;
            }
        }
    })
    .await;

    let http = reqwest::Client::new();
    let response = bounded(
        "HTTP attach head",
        http.get(format!("{base}/v1/events"))
            .query(
                &wire_ids
                    .iter()
                    .map(|id| ("session", id))
                    .collect::<Vec<_>>(),
            )
            .send(),
    )
    .await
    .expect("HTTP attach")
    .error_for_status()
    .expect("successful attach");
    let body_bytes = Cell::new(0usize);
    let mut events = Box::pin(
        response
            .bytes_stream()
            .inspect_ok(|bytes| body_bytes.set(body_bytes.get() + bytes.len()))
            .eventsource(),
    );
    let mut frames = 0;
    bounded("HTTP attach blocks", async {
        let mut caught_up = 0;
        let mut listed = false;
        while caught_up < sessions || !listed {
            let event = events.next().await.expect("open stream").expect("SSE");
            frames += 1;
            match serde_json::from_str::<Frame>(&event.data).expect("wire frame") {
                Frame::CaughtUp { .. } => caught_up += 1,
                Frame::List { .. } => listed = true,
                _ => {}
            }
        }
    })
    .await;

    let started = Instant::now();
    let cpu_before = cpu_ticks();
    let prompts = async {
        futures::future::join_all(ids.iter().map(|id| fixture.prompt(id, "stream the reply")))
            .await;
    };
    let observe = async {
        let mut ended = BTreeMap::new();
        while ended.len() < sessions {
            if let Frame::Event { session, event, .. } = local.recv().await.expect("local frame")
                && matches!(
                    event.known(),
                    Some(AgentEvent::AgentEnd {
                        agent_id: AgentId::Main,
                        ..
                    })
                )
            {
                assert!(ended.insert(session, Instant::now()).is_none());
            }
        }
        ended
    };
    let receive = async {
        let mut ended = BTreeMap::new();
        let mut final_replies = BTreeSet::new();
        let mut snapshots = 0;
        while ended.len() < sessions {
            let event = events.next().await.expect("open stream").expect("SSE");
            frames += 1;
            if let Frame::Event { session, event, .. } =
                serde_json::from_str::<Frame>(&event.data).expect("wire frame")
            {
                let index = wire_ids
                    .iter()
                    .position(|id| *id == session)
                    .expect("attached id");
                match event.known() {
                    Some(AgentEvent::MessageUpdate { .. }) => snapshots += 1,
                    Some(AgentEvent::MessageEnd { message, .. }) => {
                        if let Some(Message::Assistant(message)) = message.as_stored_wire() {
                            let text: String = message
                                .content
                                .iter()
                                .filter_map(|part| match part {
                                    AssistantContent::Text(text) => Some(text.text.as_str()),
                                    _ => None,
                                })
                                .collect();
                            assert_eq!(text, reply, "the complete generated reply arrives");
                            assert!(final_replies.insert(index));
                        }
                    }
                    Some(AgentEvent::AgentEnd {
                        agent_id: AgentId::Main,
                        ..
                    }) => {
                        assert!(final_replies.contains(&index), "reply precedes completion");
                        assert!(ended.insert(ids[index].clone(), Instant::now()).is_none());
                    }
                    _ => {}
                }
            }
        }
        (ended, snapshots)
    };
    let (_, local_ends, (remote_ends, snapshots)) = bounded("all replies", async {
        tokio::join!(prompts, observe, receive)
    })
    .await;
    let cpu = cpu_ticks() - cpu_before;
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let gaps: Vec<_> = ids
        .iter()
        .map(|id| {
            let local = local_ends[id];
            let remote = remote_ends[id];
            if remote >= local {
                remote.duration_since(local).as_secs_f64() * 1000.0
            } else {
                -local.duration_since(remote).as_secs_f64() * 1000.0
            }
        })
        .collect();
    drop(events);
    drop(local);
    drop(http);
    let shutdown_started = Instant::now();
    if let Some((gateway, server)) = gateway {
        server.shutdown().await;
        gateway.shutdown().await;
    }
    fixture.shutdown().await;
    bounded("fixture sockets to close", async {
        while socket_count() > sockets_before {
            tokio::task::yield_now().await;
        }
    })
    .await;
    println!(
        "STREAM_PROFILE {}",
        serde_json::json!({
            "gateway": via_gateway, "sessions": sessions, "chunk_delay_ms": delay_ms,
            "chunks_per_reply": chunks, "reply_bytes": reply.len(),
            "wall_ms": wall_ms, "cpu_ticks": cpu, "frames": frames,
            "snapshots": snapshots, "sse_body_bytes": body_bytes.get(),
            "completion_gap_ms": gaps,
            "shutdown_ms": shutdown_started.elapsed().as_secs_f64() * 1000.0,
            "socket_baseline": sockets_before, "sockets_after_shutdown": socket_count(),
        })
    );
}
