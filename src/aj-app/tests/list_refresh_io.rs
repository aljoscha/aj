//! The list refresh's I/O budget (spec 6.8).
//!
//! One test, in its own binary, because the oracle is this process's
//! cumulative read counter and any other test running beside it would show up
//! in the same number.
//!
//! What the budgets catch is a directory that goes back to the store for what
//! the host already holds, or that reads a log to produce a row. This is the
//! only test that can: the answers are correct either way, only the reads
//! differ, and the unit tests over the caching layer can only pin that it
//! honours the live set it is handed, not that the host hands it one.
//!
//! The read counter cannot see the other half of the contract. A directory
//! read and a `stat` transfer no bytes, so a refresh that enumerates the store
//! on every tick stays inside a byte budget, which is why the enumeration
//! counts are asserted beside it. Both of them: an enumeration point reads the
//! store's directory and the `meta/` one holding the tag sidecars, and a
//! refresh that went looking for labels would transfer no bytes either.

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
/// the budgets below, few enough that writing them costs nothing.
const COLD_LOGS: usize = 100;

/// Bytes each cold log holds. Above the 8 KiB a buffered first-line read pulls
/// in, so re-sniffing the store would cost ~800 KB per refresh and reading the
/// logs whole ~1.6 MB.
const LOG_BYTES: usize = 16 * 1024;

/// What one streaming turn's worth of refreshes may read.
///
/// Sized off both sides of the gap it has to separate: honouring the contract
/// measures ~112 bytes for the whole turn (reading `/proc/self/io` itself,
/// nothing else), while dropping just the live session from the scan filter
/// measures ~85 KB, and reading the logs costs megabytes. Well clear of
/// either end, so the test fails on a regression rather than on the noise a
/// different runtime or filesystem contributes.
const BUDGET: u64 = 16 * 1024;

/// What composing a host over `COLD_LOGS` logs may read.
///
/// Startup enumerates, and enumerating sniffs each log's first line, which a
/// `BufReader` pulls in 8 KiB at a time. That is the whole cost and it
/// measures ~820 KB, so the budget is that plus a little room. Reading the
/// logs themselves would add `COLD_LOGS * LOG_BYTES` on top, 1.6 MB, which is
/// what puts the two sides clearly apart. Sized against those two numbers, so
/// it moves if either constant above does.
const STARTUP_BUDGET: u64 = 900 * 1024;

/// The sidecar axes an enumeration lists: labels and archived bits, one
/// `readdir` of `meta/` each. What the assertions below are about is that the
/// count is per axis and never per session (spec 6.8), so it moves when an
/// axis is added and not otherwise.
const SIDECAR_AXES: u64 = 2;

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

fn setup(dir: &TempDir, persistence: &ConversationPersistence) -> HostSetup {
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
    HostSetup {
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
        live_capacity: None,
    }
}

/// Both halves of the directory's I/O contract, in one test because the
/// oracle is a process-wide counter and two tests would run concurrently.
///
/// Composing a host enumerates its store (spec 6.8), before the shell paints
/// anything. That enumeration may read a log's first line to place it in or
/// out of the directory, and nothing else: a row's stamp comes from the
/// `stat`, and it carries no position at all. The failure this pins is not
/// subtle in the wild, deriving each cold row's `last_seq` means parsing every
/// log in the store, which on a real one is a multi-second, multi-gigabyte
/// read before first paint.
///
/// Then a streaming turn, which marks the directory dirty on every event, so
/// the publisher refreshes at its coalescing rate throughout. Those refreshes
/// must touch no filesystem at all: the live session's state comes from the
/// host, and the store's cold half is served from the cache the last
/// enumeration point left.
#[tokio::test(flavor = "multi_thread")]
async fn the_directory_costs_a_first_line_at_startup_and_nothing_per_refresh() {
    let dir = TempDir::new().expect("tempdir");
    let sessions_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    seed_cold_logs(&sessions_dir);
    let persistence = ConversationPersistence::new(sessions_dir);
    // Everything the composition needs is built before the window opens, so
    // what it measures is the enumeration and nothing around it.
    let setup = setup(&dir, &persistence);

    let before = read_bytes();
    let host = SessionHost::new(setup).expect("host");
    let composed = read_bytes() - before;
    // Asserted beside the bytes because a directory read and a `stat` transfer
    // none: a startup that enumerated once per session, rather than once, would
    // sit inside the budget below.
    assert_eq!(
        host.store_directory_reads(),
        1,
        "composing a host is one enumeration point (spec 6.8)",
    );
    assert_eq!(
        host.store_sidecar_directory_reads(),
        SIDECAR_AXES,
        "which lists the sidecar directory once per axis and no more",
    );

    let listed = host.sessions().await.expect("sessions").sessions;
    assert_eq!(
        listed.len(),
        COLD_LOGS,
        "the store's logs are all in the listing",
    );
    assert!(
        listed.iter().all(|row| !row.live && row.last_seq.is_none()),
        "and every one of them is a cold row with no position",
    );
    assert!(
        composed < STARTUP_BUDGET,
        "composing a host over {COLD_LOGS} logs of {LOG_BYTES} bytes read \
         {composed} bytes, budget {STARTUP_BUDGET}: startup is reading logs, \
         not enumerating them",
    );

    // Materialize before the second window: building a session reads context
    // files and skills, a one-off cost the refresh budget is not about.
    let session = host.create().await.expect("create");
    host.sessions().await.expect("sessions");

    let before = read_bytes();
    let enumerations = host.store_directory_reads();
    let sidecar_enumerations = host.store_sidecar_directory_reads();
    // Every explicit listing below is an enumeration point, so the count is
    // attributable: what must not appear in it is a refresh.
    let mut polls = 0_u64;
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
    loop {
        polls += 1;
        let working = host
            .sessions()
            .await
            .expect("sessions")
            .sessions
            .iter()
            .any(|entry| entry.id == session && entry.working);
        if !working {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the turn never ended");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let read = read_bytes() - before;
    let enumerated = host.store_directory_reads() - enumerations;
    let sidecars_enumerated = host.store_sidecar_directory_reads() - sidecar_enumerations;
    host.shutdown().await;

    assert!(
        read < BUDGET,
        "a turn's refreshes read {read} bytes over a store of {COLD_LOGS} logs, \
         budget {BUDGET}: the refresh is going back to the store for what the \
         host already holds",
    );
    assert_eq!(
        enumerated, polls,
        "the host read its directory {enumerated} times over {polls} explicit \
         listings: the refresh is enumerating the store",
    );
    assert_eq!(
        sidecars_enumerated,
        polls * SIDECAR_AXES,
        "the host listed the sidecar directory {sidecars_enumerated} times over \
         {polls} explicit listings of {SIDECAR_AXES} axes: the refresh is going \
         after the sidecars",
    );
}
