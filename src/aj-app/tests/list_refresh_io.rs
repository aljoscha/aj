//! The list refresh's I/O budget (spec 6.8).
//!
//! One test, in its own binary, because the oracle is this process's
//! cumulative read counter and any other test running beside it would show up
//! in the same number.
//!
//! What the budget catches is a refresh that goes back to the store for what
//! the host already holds. It is the only test that can: the answers a refresh
//! gives are correct either way, only the reads differ, and the unit tests over
//! the caching layer can only pin that it honours the live set it is handed,
//! not that the host hands it one.

#![cfg(target_os = "linux")]

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aj_agent::events::AgentId;
use aj_app::host::{Command, HostSetup, SessionHost};
use aj_app::session_setup::RunConfigSnapshot;
use aj_app::settings::ConfigLayers;
use aj_app::test_support::{finalized_text_message, scripted_model_info};
use aj_conf::{Config, ConfigLayer};
use aj_models::auth::AuthStorage;
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::types::UserContent;
use aj_session::ConversationPersistence;
use tempfile::TempDir;

/// Cold logs in the store. Enough that re-reading them is unmistakable next to
/// the budget below, few enough that writing them costs nothing.
const COLD_LOGS: usize = 100;

/// Bytes each cold log holds. Above the 8 KiB a buffered first-line read pulls
/// in, so re-sniffing the store would cost ~800 KB per refresh and re-counting
/// it ~1.6 MB.
const LOG_BYTES: usize = 16 * 1024;

/// What one streaming turn's worth of refreshes may read.
///
/// Sized off both sides of the gap it has to separate: honouring the contract
/// measures ~112 bytes for the whole turn (reading `/proc/self/io` itself,
/// nothing else), while dropping just the live session from the scan filter
/// measures ~85 KB, and dropping the caches costs megabytes. Well clear of
/// either end, so the test fails on a regression rather than on the noise a
/// different runtime or filesystem contributes.
const BUDGET: u64 = 16 * 1024;

/// This process's cumulative read bytes, `rchar` from `/proc/self/io`. Counts
/// every read that reached a file descriptor, page cache or not.
fn read_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .expect("this process's io counters")
        .lines()
        .find_map(|line| line.strip_prefix("rchar: "))
        .expect("an rchar line")
        .trim()
        .parse()
        .expect("a byte count")
}

/// Write `COLD_LOGS` current-format logs nothing will ever materialize.
fn seed_cold_logs(sessions_dir: &std::path::Path) {
    let entry = serde_json::json!({
        "id": "00000000",
        "timestamp": "2024-01-01T00:00:00Z",
        "thread": "meta",
        "type": "system_prompt",
        "text": "x".repeat(400),
    })
    .to_string();
    let body: String = std::iter::repeat_n(entry, LOG_BYTES / 400)
        .map(|line| format!("{line}\n"))
        .collect();
    for i in 0..COLD_LOGS {
        std::fs::write(
            sessions_dir.join(format!("2020-01-01-00-00-00-{i:03}.jsonl")),
            &body,
        )
        .expect("write a cold log");
    }
}

fn host(dir: &TempDir, persistence: &ConversationPersistence) -> SessionHost {
    let provider = Arc::new(
        ScriptedProvider::from_messages(
            // Long and slow, so the turn spans many coalescing ticks.
            vec![finalized_text_message(
                &"a slowly streamed answer ".repeat(40),
            )],
            1,
            Duration::from_millis(4),
        )
        .on_exhausted(ExhaustedBehavior::Panic),
    );
    SessionHost::new(HostSetup {
        config: Arc::new(StdMutex::new(Config::default())),
        layers: Arc::new(StdMutex::new(ConfigLayers {
            user: Config::default(),
            project: ConfigLayer::default(),
            project_path: None,
        })),
        catalog: Arc::new(Vec::new()),
        run_config: RunConfigSnapshot {
            provider,
            model_info: Arc::new(scripted_model_info()),
            stream_options: aj_models::types::StreamOptions::default(),
            thinking: None,
            thinking_display: None,
            speed: None,
            model_key: ("scripted".to_string(), "scripted".to_string()),
            session_id: None,
        },
        restore: None,
        persistence: persistence.clone(),
        auth: AuthStorage::new(dir.path().join("auth.json")),
        working_directory: dir.path().to_path_buf(),
        idle_grace: None,
    })
    .expect("host")
}

/// A streaming turn marks the directory dirty on every event, so the publisher
/// refreshes at its coalescing rate throughout. Once the store's cold half is
/// cached, those refreshes must read nothing: the live session's mark comes
/// from the host, and nothing on disk changed.
#[tokio::test(flavor = "multi_thread")]
async fn a_turns_worth_of_refreshes_reads_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let sessions_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    seed_cold_logs(&sessions_dir);
    let persistence = ConversationPersistence::new(sessions_dir);
    let host = host(&dir, &persistence);

    // Materialize and warm the cache first: building a session reads context
    // files and skills, and the first refresh counts every cold log once. Both
    // are one-off costs the budget is not about.
    let session = host.create().await.expect("create");
    host.sessions().await.expect("sessions");

    let before = read_bytes();
    host.command(
        &session,
        Command::Prompt {
            agent: AgentId::Main,
            content: vec![UserContent::text("go")],
        },
    )
    .await
    .expect("prompt");
    // Polling the host is itself a refresh, so the wait does not understate
    // what a turn's refreshes cost.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while host
        .sessions()
        .await
        .expect("sessions")
        .sessions
        .iter()
        .any(|entry| entry.id == session && entry.working)
    {
        assert!(std::time::Instant::now() < deadline, "the turn never ended");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let read = read_bytes() - before;
    host.shutdown().await;

    assert!(
        read < BUDGET,
        "a turn's refreshes read {read} bytes over a store of {COLD_LOGS} logs, \
         budget {BUDGET}: the refresh is going back to the store for what the \
         host already holds",
    );
}
