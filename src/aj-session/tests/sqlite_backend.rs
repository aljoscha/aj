//! The session store and JSONL import, through the public session API.

use std::path::Path;

use aj_agent::events::AgentId;
use aj_agent::message::AgentMessage;
use aj_models::types::{
    AssistantContent, AssistantMessage, Message, ToolCall, ToolResultMessage, UserContent,
    UserMessage,
};
use aj_session::{
    LogEntryKind, MigrationResult, ProjectSessions, SessionError, SessionLock, SessionLog,
    ThreadFilter, ThreadKind, fail_sqlite_commits, jsonl_session, migrate_all_jsonl,
};

fn message(text: &str) -> LogEntryKind {
    LogEntryKind::Message {
        message: AgentMessage::wire(Message::User(UserMessage::text(text))),
    }
}

fn append(log: &mut SessionLog, text: &str) -> String {
    let parent = log.head().or_else(|| log.system_prompt_id()).cloned();
    log.append(parent, ThreadKind::Main, None, message(text))
        .unwrap()
        .id
}

fn add(log: &mut SessionLog, message: Message) -> String {
    let parent = log.head().or_else(|| log.system_prompt_id()).cloned();
    let message = AgentMessage::wire(message);
    let entry = LogEntryKind::Message { message };
    log.append(parent, ThreadKind::Main, None, entry)
        .unwrap()
        .id
}

/// Append the user message the pending input `id` becomes.
fn add_as(log: &mut SessionLog, id: &str, text: &str) -> String {
    let parent = log.head().or_else(|| log.system_prompt_id()).cloned();
    let mut message = AgentMessage::wire(Message::User(UserMessage::text(text)));
    message.set_id(id.to_owned());
    let entry = LogEntryKind::Message { message };
    log.append(parent, ThreadKind::Main, None, entry)
        .unwrap()
        .id
}

fn call(id: &str) -> Message {
    let call = ToolCall {
        is_raw: false,
        id: id.into(),
        name: "read".into(),
        arguments: serde_json::json!({}),
    };
    Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::ToolCall(call)],
        ..AssistantMessage::empty()
    })
}

fn result(id: &str, output: &str) -> Message {
    Message::ToolResult(ToolResultMessage::text(id, "read", output, false))
}

/// Append turn `n`: a prompt, a tool call and its result. Returns the
/// prompt's id.
fn tool_turn(log: &mut SessionLog, n: usize) -> String {
    let prompt = append(log, &format!("prompt {n}"));
    add(log, call(&format!("call-{n}")));
    add(log, result(&format!("call-{n}"), &format!("output {n}")));
    prompt
}

/// The model context turn `n` contributes, one line per message.
fn turn_lines(n: usize) -> [String; 3] {
    [
        format!("user: prompt {n}"),
        format!("call call-{n}"),
        format!("result call-{n}: output {n}"),
    ]
}

/// The model context of the branch ending at `head`, one line per message.
fn context(log: &SessionLog, head: &String) -> Vec<String> {
    let text = |content: &[UserContent]| -> String {
        let texts = content.iter().map(|part| match part {
            UserContent::Text(text) => text.text.as_str(),
            UserContent::Image(_) => "image",
        });
        texts.collect()
    };
    let messages = log.linearize(head, ThreadFilter::MAIN).agent_messages();
    let wire = messages.iter().filter_map(AgentMessage::to_projected_wire);
    wire.map(|message| match message {
        Message::User(user) => match text(&user.content) {
            text if text.contains("SUMMARY") => "summary".into(),
            text if text.starts_with("The following user messages are retained") => {
                "retained:".into()
            }
            text => format!("user: {text}"),
        },
        Message::Assistant(assistant) => {
            let parts = assistant.content.iter().map(|part| match part {
                AssistantContent::Text(text) => format!("assistant: {}", text.text),
                AssistantContent::ToolCall(call) => format!("call {}", call.id),
                AssistantContent::Thinking(_) => "thinking".into(),
            });
            parts.collect::<Vec<_>>().join(" + ")
        }
        Message::ToolResult(result) => {
            format!("result {}: {}", result.tool_call_id, text(&result.content))
        }
    })
    .collect()
}

/// A project scope under `root`, sharing `root/sessions.sqlite3`.
fn project(root: &Path, name: &str) -> ProjectSessions {
    ProjectSessions::for_project(
        root.join("sessions.sqlite3"),
        root.join("sessions").join(name),
        name.to_owned(),
    )
}

/// A JSONL session with one prompt. Returns its ID and source path.
fn legacy_session(store: &ProjectSessions, text: &str) -> (String, std::path::PathBuf) {
    jsonl_session(store, |log| {
        append(log, text);
    })
}

fn prompts(store: &ProjectSessions) -> Vec<String> {
    let history = aj_session::workspace_history(store, 10, &|| false);
    history.into_iter().map(|entry| entry.text).collect()
}

#[test]
fn new_sessions_share_one_database_and_stay_project_scoped() {
    let root = tempfile::tempdir().unwrap();
    let (one, two) = (project(root.path(), "one"), project(root.path(), "two"));

    let mut log = SessionLog::create(&one).unwrap();
    let id = log.session_id().to_owned();
    // A label and an archive marker may precede the first entry. They wait
    // with the log, and nothing is stored for a session that is abandoned.
    let mut abandoned = SessionLog::create(&one).unwrap();
    let abandoned_id = abandoned.session_id().to_owned();
    for log in [&mut log, &mut abandoned] {
        log.set_tag(Some("label")).unwrap();
        log.set_archived(true).unwrap();
        assert!(log.set_tag(Some(" not normalized ")).is_err());
    }
    drop(abandoned);
    assert!(one.list_sessions().unwrap().is_empty());
    assert_eq!(one.read_tag(&id).unwrap(), None);
    assert!(
        !one.sqlite_path().exists(),
        "a label alone stored something"
    );

    append(&mut log, "one prompt");
    assert!(!one.in_database(&abandoned_id).unwrap());
    assert!(
        one.enumerate_tags().unwrap().len() == 1
            && one.enumerate_archived().unwrap() == [id.clone()]
    );
    // Once stored, a label is committed as it is set.
    log.set_tag(Some("renamed")).unwrap();
    assert_eq!(one.read_tag(&id).unwrap().as_deref(), Some("renamed"));
    log.set_tag(Some("label")).unwrap();
    append(&mut SessionLog::create(&two).unwrap(), "two prompt");
    drop(log);

    assert!(one.in_database(&id).unwrap());
    assert_eq!(one.list_sessions().unwrap().len(), 1);
    assert_eq!(prompts(&one), ["one prompt"]);
    assert_eq!(prompts(&two), ["two prompt"]);
    assert_eq!(one.read_tag(&id).unwrap().as_deref(), Some("label"));
    assert!(one.read_archived(&id).unwrap());
    assert!(two.session_metadata(&id).unwrap().is_none());
    assert_eq!(two.read_tag(&id).unwrap(), None);
    assert_eq!(SessionLog::resume(&one, &id).unwrap().last_seq(), 1);
    // Nothing is written to the legacy per-project layout.
    assert!(!one.sessions_dir().join(format!("{id}.jsonl")).exists());
    assert!(!one.sessions_dir().join("meta").exists());
}

/// A JSONL file without entries is not imported. Its sidecars' label and
/// archive marker are stored with the session once it has an entry.
#[test]
fn an_empty_jsonl_session_keeps_its_sidecar_labels_when_it_is_stored() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let id = "2024-01-01-00-00-00-000";
    std::fs::create_dir_all(store.sessions_dir()).unwrap();
    std::fs::write(store.sessions_dir().join(format!("{id}.jsonl")), "").unwrap();
    store.write_tag(id, Some("label")).unwrap();
    store.write_archived(id, true).unwrap();

    let mut log = SessionLog::resume(&store, id).unwrap();
    assert!(!store.in_database(id).unwrap());
    append(&mut log, "prompt");
    drop(log);
    // The database answers now, not the sidecars.
    std::fs::remove_dir_all(store.sessions_dir().join("meta")).unwrap();
    assert_eq!(store.read_tag(id).unwrap().as_deref(), Some("label"));
    assert!(store.read_archived(id).unwrap());
}

#[test]
fn migration_preserves_history_and_makes_sqlite_authoritative() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut entries = serde_json::Value::Null;
    let (id, path) = jsonl_session(&store, |log| {
        let first = append(log, "root");
        append(log, "abandoned branch");
        log.set_head(first).unwrap();
        let chosen = append(log, "selected branch");
        log.append(Some(chosen), ThreadKind::Sub, Some(7), message("child"))
            .unwrap();
        entries = serde_json::to_value(log.entries_in_order()).unwrap();
    });
    store.write_tag(&id, Some("label")).unwrap();
    store.write_archived(&id, true).unwrap();
    let source = std::fs::read(&path).unwrap();

    assert_eq!(store.migrate_jsonl(&id).unwrap(), MigrationResult::Imported);
    assert!(store.in_database(&id).unwrap());
    assert_eq!(store.list_sessions().unwrap().len(), 1);
    assert_eq!(store.read_tag(&id).unwrap().as_deref(), Some("label"));
    assert!(store.read_archived(&id).unwrap());
    let mut restored = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(
        serde_json::to_value(store.read_entries(&id).unwrap()).unwrap(),
        entries
    );
    // The reopened log holds the selected branch, not the abandoned one.
    assert_eq!((restored.len(), restored.snapshot().held()), (4, 3));
    // An export of the imported branch and sub-agent thread is the source
    // file again, byte for byte.
    let exported = store.export_jsonl(&id, &root.path().join("out")).unwrap();
    assert_eq!(std::fs::read(exported).unwrap(), source);

    // Later work and label edits land in SQLite and survive a retry. The
    // retained source and its sidecars no longer supply any state, also
    // when they change.
    append(&mut restored, "after import");
    drop(restored);
    store.write_tag(&id, None).unwrap();
    let sidecar = store.sessions_dir().join("meta").join(format!("{id}.tag"));
    std::fs::write(sidecar, "changed\n").unwrap();
    assert_eq!(
        store.migrate_jsonl(&id).unwrap(),
        MigrationResult::AlreadyImported
    );
    assert_eq!(store.read_tag(&id).unwrap(), None);
    assert_eq!(SessionLog::resume(&store, &id).unwrap().last_seq(), 5);
    assert_eq!(std::fs::read(&path).unwrap(), source);
}

#[test]
fn migration_refuses_busy_and_damaged_sources_without_touching_them() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");

    let (busy, _) = legacy_session(&store, "busy");
    let lock = SessionLock::try_acquire(&store, &busy, "another host")
        .unwrap()
        .unwrap();
    for attempt in [
        store.check_jsonl_migration(&busy),
        store.migrate_jsonl(&busy),
    ] {
        assert!(matches!(attempt, Err(SessionError::MigrationBusy(_))));
    }
    drop(lock);

    // A malformed entry that has its newline is not an interrupted write.
    let (damaged, path) = legacy_session(&store, "damaged");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(b"{\"id\":\n").unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(store.check_jsonl_migration(&damaged).is_err());
    assert!(store.migrate_jsonl(&damaged).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    for id in [&busy, &damaged] {
        assert!(!store.in_database(id).unwrap());
    }
    assert!(!store.sqlite_path().exists());
}

#[test]
fn dry_run_predicts_the_import_without_creating_state() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let (id, _) = legacy_session(&store, "prompt");
    let locks = store.sessions_dir().join("locks");
    let _ = std::fs::remove_dir_all(&locks);

    assert_eq!(
        store.check_jsonl_migration(&id).unwrap(),
        MigrationResult::WouldImport
    );
    assert!(!store.sqlite_path().exists());
    assert!(!locks.exists());

    store.migrate_jsonl(&id).unwrap();
    assert_eq!(
        store.check_jsonl_migration(&id).unwrap(),
        MigrationResult::AlreadyImported
    );
}

#[test]
fn bulk_migration_covers_every_project_and_continues_past_failures() {
    let root = tempfile::tempdir().unwrap();
    let (one, two) = (project(root.path(), "one"), project(root.path(), "two"));
    let (good_one, _) = legacy_session(&one, "one prompt");
    let (good_two, _) = legacy_session(&two, "two prompt");
    std::fs::write(one.sessions_dir().join("broken.jsonl"), "not json\n").unwrap();
    let run = |dry_run| {
        let mut reports = Vec::new();
        let database = root.path().join("sessions.sqlite3");
        migrate_all_jsonl(&database, &root.path().join("sessions"), dry_run, |r| {
            reports.push((r.project_id, r.session_id.unwrap(), r.result.ok()))
        })
        .unwrap();
        reports.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        reports
    };
    let expect = |result| {
        let mut reports = vec![
            ("one".to_owned(), "broken".to_owned(), None),
            ("one".to_owned(), good_one.clone(), Some(result)),
            ("two".to_owned(), good_two.clone(), Some(result)),
        ];
        reports.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        reports
    };

    assert_eq!(run(true), expect(MigrationResult::WouldImport));
    assert!(!one.sqlite_path().exists());
    assert_eq!(run(false), expect(MigrationResult::Imported));
    assert_eq!(run(false), expect(MigrationResult::AlreadyImported));
    assert!(one.in_database(&good_one).unwrap());
    assert_eq!(prompts(&one), ["one prompt"]);
    assert_eq!(prompts(&two), ["two prompt"]);
}

#[test]
fn export_and_import_carry_history_and_labels_between_projects() {
    let root = tempfile::tempdir().unwrap();
    let (one, two) = (project(root.path(), "one"), project(root.path(), "two"));
    let mut log = SessionLog::create(&one).unwrap();
    let first = append(&mut log, "root");
    append(&mut log, "abandoned branch");
    log.set_head(first).unwrap();
    append(&mut log, "selected branch");
    let id = log.session_id().to_owned();
    drop(log);
    one.write_tag(&id, Some("label")).unwrap();
    one.write_archived(&id, true).unwrap();

    let directory = root.path().join("exported");
    let file = one.export_jsonl(&id, &directory).unwrap();
    assert!(one.export_jsonl(&id, &directory).is_err(), "no overwrite");
    assert_eq!(two.import_jsonl(&file).unwrap(), id);

    let records =
        |store: &ProjectSessions| serde_json::to_value(store.read_entries(&id).unwrap()).unwrap();
    assert_eq!(records(&two), records(&one));
    assert_eq!(two.read_tag(&id).unwrap().as_deref(), Some("label"));
    assert!(two.read_archived(&id).unwrap());
    assert!(matches!(
        two.import_jsonl(&file),
        Err(SessionError::MigrationConflict(_))
    ));
    // The export is itself a session an older, JSONL-only reader can open.
    let plain = ProjectSessions::new(directory);
    assert_eq!(records(&plain), records(&one));
    assert_eq!(one.check_database().unwrap(), Some(Vec::new()));
}

#[test]
fn a_failed_commit_fuses_the_log_and_reopening_sees_only_committed_records() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut log = SessionLog::create(&store).unwrap();
    append(&mut log, "committed");
    let (id, head) = (log.session_id().to_owned(), log.head().cloned());

    fail_sqlite_commits(&mut log);
    for _ in 0..2 {
        assert!(matches!(
            log.append(head.clone(), ThreadKind::Main, None, message("refused")),
            Err(SessionError::WriteFailed(_))
        ));
        assert_eq!((log.head(), log.last_seq()), (head.as_ref(), 1));
    }
    // Every later mutation refuses with that first failure.
    let first = log.write_failure().cloned();
    let refused = log.set_head(head.clone().unwrap()).unwrap_err();
    assert_eq!(refused.persistence_failure(), first.as_ref());
    drop(log);
    assert_eq!(SessionLog::resume(&store, &id).unwrap().last_seq(), 1);
}

/// An append waits for another writer for the busy wait every write
/// has. A writer that holds the database longer refuses the append, which
/// writes nothing and leaves the log writable: the same append lands once
/// the database is free.
#[test]
fn an_append_waits_out_another_writer_and_a_busy_one_changes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut log = SessionLog::create(&store).unwrap();
    append(&mut log, "first");

    // Another process holds the shared database, as a bulk import does,
    // for less than the wait.
    let other = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let held = std::time::Duration::from_millis(300);
    let release = std::thread::spawn(move || {
        std::thread::sleep(held);
        other.execute_batch("COMMIT").unwrap();
        other
    });
    let started = std::time::Instant::now();
    append(&mut log, "second");
    assert!(started.elapsed() >= held, "the append did not have to wait");
    let other = release.join().unwrap();

    // It holds on past the wait.
    log.set_busy_wait(std::time::Duration::from_millis(10))
        .unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let head = log.head().cloned();
    let refused = log
        .append(head.clone(), ThreadKind::Main, None, message("third"))
        .unwrap_err();
    assert!(refused.is_busy(), "{refused}");
    assert!(refused.persistence_failure().is_none(), "{refused}");
    assert!(log.write_failure().is_none());
    assert_eq!((log.head(), log.last_seq()), (head.as_ref(), 2));
    other.execute_batch("COMMIT").unwrap();

    let third = append(&mut log, "third");
    assert_eq!((log.head(), log.last_seq()), (Some(&third), 3));
    let id = log.session_id().to_owned();
    drop(log);
    assert_eq!(SessionLog::resume(&store, &id).unwrap().last_seq(), 3);
}

/// The first commit of a session creates it. One the database is too busy
/// to take leaves the session unstored and its waiting entries in place,
/// and the next try stores all of them.
#[test]
fn a_busy_first_commit_leaves_the_session_to_be_created_by_the_next() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    // The database exists, as it does once any session was stored.
    let mut earlier = SessionLog::create(&store).unwrap();
    append(&mut earlier, "earlier");
    drop(earlier);

    let mut log = SessionLog::create(&store).unwrap();
    log.set_system_prompt("prompt".into()).unwrap();
    log.set_busy_wait(std::time::Duration::from_millis(10))
        .unwrap();
    let other = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let parent = log.system_prompt_id().cloned();
    let refused = log
        .append(parent, ThreadKind::Main, None, message("first"))
        .unwrap_err();
    assert!(refused.is_busy(), "{refused}");
    assert!(log.write_failure().is_none());
    assert!(!log.is_durable());
    assert_eq!((log.head(), log.last_seq()), (None, 1));
    other.execute_batch("COMMIT").unwrap();

    let first = append(&mut log, "first");
    assert!(log.is_durable());
    assert_eq!((log.head(), log.last_seq()), (Some(&first), 2));
    let id = log.session_id().to_owned();
    drop(log);
    let log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!((log.head(), log.last_seq()), (Some(&first), 2));
}

#[test]
fn a_session_reopens_while_another_process_holds_the_write_lock() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut log = SessionLog::create(&store).unwrap();
    append(&mut log, "first");
    let id = log.session_id().to_owned();
    drop(log);

    let other = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.last_seq(), 1);
    // With nothing saved there is nothing to drop, so nothing is written.
    let pending = log.pending();
    assert!(
        log.restore_pending(&pending, AgentId::Main)
            .unwrap()
            .is_empty()
    );
    other.execute_batch("COMMIT").unwrap();
}

#[test]
fn an_empty_file_at_the_database_path_becomes_the_database() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    std::fs::File::create(store.sqlite_path()).unwrap();
    for text in ["first", "second"] {
        // The second session finds what the first left.
        let mut log = SessionLog::create(&store).unwrap();
        append(&mut log, text);
    }
    assert_eq!(store.list_sessions().unwrap().len(), 2);
}

/// A recorded task is in the store until its end is saved, and the save
/// swaps the record for the notification or changes nothing.
#[test]
fn a_task_record_is_replaced_by_its_notification_in_one_transaction() {
    use aj_agent::events::AgentId::Main;
    use aj_agent::message::{TaskNotification, TaskNotificationKind, TaskOutcome};
    use aj_session::{Pending, TaskRecord};

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let task = |id| TaskRecord {
        id,
        owner: Main,
        call_id: "call-1".into(),
        command: "make".into(),
        pid: 4242,
        process_start: 99,
        output_path: "/tmp/out.log".into(),
        exit_path: "/tmp/out.exit".into(),
        started_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
    };
    let notice = AgentMessage::task_notification(TaskNotification::new(
        "make".into(),
        TaskNotificationKind::Bash,
        TaskOutcome::Succeeded,
        "exit 0".into(),
    ));

    // A session that is not stored has nowhere to record a task.
    let mut log = SessionLog::create(&store).unwrap();
    let tasks = log.task_records();
    assert!(tasks.start(&task(3)).is_err());
    assert_eq!(tasks.last_id().unwrap(), 0);

    append(&mut log, "prompt");
    let id = log.session_id().to_owned();
    tasks.start(&task(3)).unwrap();
    assert_eq!(
        log.restore_tasks(&tasks, AgentId::Main).unwrap(),
        vec![task(3)]
    );
    assert_eq!(tasks.last_id().unwrap(), 3);
    assert!(tasks.start(&task(3)).is_err(), "an id records one task");

    // The notification cannot be saved, so the record is not removed either.
    store.refuse_sqlite_pending_writes();
    assert!(tasks.finish(3, Main, &notice).is_err());
    assert_eq!(
        log.restore_tasks(&tasks, AgentId::Main).unwrap(),
        vec![task(3)]
    );
    store.allow_sqlite_pending_writes();
    assert!(
        log.restore_pending(&log.pending(), AgentId::Main)
            .unwrap()
            .is_empty(),
        "nothing was saved for delivery"
    );

    // The record cannot be removed, so the notification is not saved either.
    store.refuse_sqlite_task_writes(false, true);
    assert!(tasks.finish(3, Main, &notice).is_err());
    assert!(
        log.restore_pending(&log.pending(), AgentId::Main)
            .unwrap()
            .is_empty()
    );
    store.refuse_sqlite_task_writes(false, false);

    tasks.finish(3, Main, &notice).unwrap();
    assert!(log.restore_tasks(&tasks, AgentId::Main).unwrap().is_empty());
    let pending = log.restore_pending(&log.pending(), AgentId::Main).unwrap();
    assert!(
        matches!(&pending[..], [Pending::TaskNotification(Main, message)] if message.id() == notice.id()),
        "{pending:?}"
    );

    // A later process mints above every id this one used, recorded or not.
    tasks.reserve(7).unwrap();
    tasks.reserve(5).unwrap();

    tasks.start(&task(8)).unwrap();
    let read = |log: &SessionLog, tasks: &aj_session::TaskRecords| {
        let listed = store.task_records().unwrap();
        let listed: Vec<_> = listed.into_iter().map(|(_, _, task)| task).collect();
        assert_eq!(listed, vec![task(8)]);
        assert_eq!(
            log.restore_tasks(tasks, AgentId::Main).unwrap(),
            vec![task(8)]
        );
    };
    read(&log, &tasks);

    drop((tasks, log));
    let log = SessionLog::resume(&store, &id).unwrap();
    let tasks = log.task_records();
    assert_eq!(tasks.last_id().unwrap(), 8);
    read(&log, &tasks);
}

#[test]
fn pending_work_is_retired_only_by_the_append_that_delivers_it() {
    use aj_agent::events::AgentId::Main;
    use aj_agent::message::{TaskNotification, TaskNotificationKind, TaskOutcome};
    use aj_agent::queue::MessageQueues;
    use aj_session::Pending;

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut log = SessionLog::create(&store).unwrap();
    let prompt = append(&mut log, "prompt");
    let id = log.session_id().to_owned();

    // The session accepts a pending input and a finished task's notification.
    let pending = log.pending();
    let queues = MessageQueues::default();
    queues.append_follow_up(Main, "queued");
    let (kind, input) = queues.pending(Main).unwrap();
    pending.save_input(Main, kind, &input).unwrap();
    let notice = AgentMessage::task_notification(TaskNotification::new(
        "make".into(),
        TaskNotificationKind::Bash,
        TaskOutcome::Succeeded,
        "exit 0".into(),
    ));
    // The task has no record, so only its notification is saved.
    log.task_records().finish(1, Main, &notice).unwrap();
    // A sub-agent's pending input is saved with the sub-agent's own
    // thread, so it is not the main agent's to deliver.
    let settings = aj_agent::events::AgentSettings {
        context_window: 0,
        provider: "p".into(),
        model_id: "m".into(),
        thinking: "off".into(),
        thinking_display: String::new(),
        speed: "default".into(),
        verbosity: "default".into(),
    };
    log.append_subagent_spawn(1, prompt.clone(), "task", "agent", None, true, &settings)
        .unwrap();
    let sub = aj_agent::events::AgentId::Sub(1);
    queues.append_follow_up(sub, "for the sub-agent");
    let (sub_kind, sub_input) = queues.pending(sub).unwrap();
    pending.save_input(sub, sub_kind, &sub_input).unwrap();

    // The agent drains the queue in memory, but the append never commits.
    let drained = queues.drain_follow_up(Main).remove(0).into_message();
    fail_sqlite_commits(&mut log);
    let entry = LogEntryKind::Message { message: drained };
    assert!(
        log.append(log.head().cloned(), ThreadKind::Main, None, entry)
            .is_err()
    );
    drop((log, pending));

    // A new process finds both, delivers them, and they are gone for good.
    let mut log = SessionLog::resume(&store, &id).unwrap();
    let pending = log.pending();
    // The sub-agent's is found on the sub-agent's thread.
    let left = log.sub_agents_with_work().unwrap();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].agent_id, 1);
    match log.restore_pending(&pending, sub).unwrap().as_slice() {
        [Pending::Input(owner, _, input)] => {
            assert_eq!((*owner, input.text().as_str()), (sub, "for the sub-agent"));
        }
        other => panic!("unexpected {other:?}"),
    }
    let restored = log.restore_pending(&pending, AgentId::Main).unwrap();
    assert_eq!(restored.len(), 2, "{restored:?}");
    for work in restored {
        let message = match work {
            Pending::Input(Main, _, input) => {
                assert_eq!(input.text(), "queued");
                input.into_message()
            }
            Pending::TaskNotification(Main, message) => {
                assert_eq!(message.id(), notice.id());
                message
            }
            other => panic!("unexpected {other:?}"),
        };
        let entry = LogEntryKind::Message { message };
        log.append(log.head().cloned(), ThreadKind::Main, None, entry)
            .unwrap();
    }
    assert!(
        log.restore_pending(&pending, AgentId::Main)
            .unwrap()
            .is_empty()
    );
    assert_eq!(log.last_seq(), 4);
    // A save that lost the race with delivery does not bring the work back.
    pending.save_input(Main, kind, &input).unwrap();
    assert!(
        log.restore_pending(&pending, AgentId::Main)
            .unwrap()
            .is_empty()
    );
    // A withdrawn message is forgotten.
    let withdrawn = aj_agent::queue::PendingInput {
        id: "w".into(),
        content: vec![aj_models::types::UserContent::text("no")],
    };
    pending.save_input(Main, kind, &withdrawn).unwrap();
    pending.forget("w").unwrap();
    assert!(
        log.restore_pending(&pending, AgentId::Main)
            .unwrap()
            .is_empty()
    );

    // Pending work stays with the branch it was accepted on.
    let kept = aj_agent::queue::PendingInput {
        id: "k".into(),
        content: vec![aj_models::types::UserContent::text("for this branch")],
    };
    pending.save_input(Main, kind, &kept).unwrap();
    let tip = log.head().cloned().unwrap();
    log.set_head(prompt).unwrap();
    let restored = log.restore_pending(&pending, AgentId::Main).unwrap();
    assert!(restored.is_empty(), "an earlier entry is not the branch");
    append(&mut log, "another branch");
    assert!(
        log.restore_pending(&pending, AgentId::Main)
            .unwrap()
            .is_empty()
    );
    log.set_head(tip.clone()).unwrap();
    let on_its_branch = |log: &SessionLog| {
        let pending = log.pending();
        match log
            .restore_pending(&pending, AgentId::Main)
            .unwrap()
            .as_slice()
        {
            [Pending::Input(Main, _, input)] => assert_eq!(input.text(), "for this branch"),
            other => panic!("unexpected {other:?}"),
        }
    };
    on_its_branch(&log);
    // The session reopens on the branch that was selected, which is not
    // the one last written to, and finds that branch's pending work.
    drop((log, pending));
    let log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.head(), Some(&tip));
    on_its_branch(&log);
}

#[test]
fn concurrent_first_use_never_exposes_a_partly_initialized_database() {
    let root = tempfile::tempdir().unwrap();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let (root, barrier) = (root.path(), &barrier);
            scope.spawn(move || {
                let store = project(root, "p");
                barrier.wait();
                if worker < 4 {
                    append(&mut SessionLog::create(&store).unwrap(), "prompt");
                }
                store.list_sessions().unwrap();
            });
        }
    });
    assert_eq!(project(root.path(), "p").list_sessions().unwrap().len(), 4);
}

/// A long session reopens holding its model context and little else. The
/// fresh view starts at a recent turn, and the pages of earlier history add
/// up to exactly what a projection of the whole session shows.
#[test]
fn a_long_session_reopens_partially_and_pages_back_to_the_same_history() {
    use aj_session::{project_page, project_suffix};
    use std::collections::BTreeSet;

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut log = SessionLog::create(&store).unwrap();
    let mut turn_starts = Vec::new();
    for turn in 0..300 {
        turn_starts.push(tool_turn(&mut log, turn));
        if turn == 250 {
            log.append_compaction(
                ThreadFilter::MAIN,
                "summary".into(),
                turn_starts[100].clone(),
                vec![turn_starts[3].clone()],
                1000,
                None,
                Default::default(),
            )
            .unwrap();
        }
    }
    let id = log.session_id().to_owned();
    // A branch from before the compaction, whose context reaches turn 0.
    let early = turn_starts[50].clone();
    let last = log.last_seq();
    drop(log);

    let mut log = SessionLog::resume(&store, &id).unwrap();
    let snapshot = log.snapshot();
    // The compaction keeps from turn 100, further back than the display
    // tail. Of the first hundred turns the log holds the prompt the
    // compaction kept and the last call. The rest stays in the store.
    assert!(last - aj_session::DISPLAY_TAIL > 301);
    assert_eq!(snapshot.window_start(), 301);
    assert_eq!(u64::try_from(snapshot.held()).unwrap(), last - 300 + 1 + 1);
    let stats = log.stats().unwrap();
    assert_eq!(stats.tool_results, 300, "the whole session is counted");

    fn render(events: impl Iterator<Item = aj_session::TaggedEvent>) -> Vec<String> {
        // A replayed assistant `MessageStart` carries a throwaway id.
        events
            .map(|tagged| format!("{:?} {:?}", tagged.entry, tagged.event))
            .filter(|event| !event.starts_with("None MessageStart"))
            .collect()
    }
    let mut paged = render(project_suffix(
        &snapshot,
        Some(snapshot.display_start() - 1),
        &BTreeSet::new(),
    ));
    let mut before = snapshot.display_start();
    assert!(before < last);
    while before > 1 {
        let mut page = snapshot.clone();
        let start = store
            .load_page(&mut page, AgentId::Main, before, 150)
            .unwrap();
        let live = BTreeSet::new();
        let mut events = render(project_page(&page, AgentId::Main, start, before, &live));
        events.append(&mut paged);
        (paged, before) = (events, start);
    }

    log.load_all().unwrap();
    assert!(log.snapshot().is_complete());
    let full = render(project_suffix(&log.snapshot(), None, &BTreeSet::new()));
    assert_eq!(paged.len(), full.len());
    assert!(paged == full, "the pages and the tail are the full history");
    drop(log);

    // A branch switch reads the target branch: nothing for the selected
    // one, and its fifty turns for the early one.
    let mut log = SessionLog::resume(&store, &id).unwrap();
    let head = log.head().unwrap().clone();
    assert!(log.branch_read(&head).unwrap().is_none());
    let read = log.branch_read(&early).unwrap();
    let read = read.expect("the early branch is not held");
    // The head cannot move onto a branch whose context is not held.
    assert!(matches!(
        log.set_head(early.clone()),
        Err(SessionError::InvalidHead(_))
    ));
    assert_eq!(log.head(), Some(&head));
    // A switch that is called off puts back what the log held.
    let held = (context(&log, &head), log.snapshot().held());
    let left = log.install_branch(read.read().unwrap()).unwrap();
    assert_ne!(log.snapshot().held(), held.1);
    log.restore_branch(left);
    assert_eq!((context(&log, &head), log.snapshot().held()), held);
    let read = log.branch_read(&early).unwrap().expect("not held again");
    log.install_branch(read.read().unwrap()).unwrap();
    // Turns 0 to 49 and the prompt of turn 50. The compaction is on the
    // branch that was left.
    assert_eq!(log.snapshot().held(), 50 * 3 + 1);
    assert_eq!(log.snapshot().window_start(), 1);
    log.set_head(early).unwrap();

    // A read the session was appended to since would drop the appended
    // entries from memory, so it is refused.
    let stale = log.branch_read(&head).unwrap().expect("left behind");
    let stale = stale.read().unwrap();
    append(&mut log, "meanwhile");
    assert!(matches!(
        log.install_branch(stale),
        Err(SessionError::InvalidHead(_))
    ));
}

/// A transcript and what the model is to be given for it: branch heads with
/// the context each has, the reopened session's own head first.
type Scenario = fn(&mut SessionLog) -> Vec<(String, Vec<String>)>;

/// Only what the model said and was told is context: not a tool's nested
/// activity, and not a sub-agent's thread.
fn a_short_session(log: &mut SessionLog) -> Vec<(String, Vec<String>)> {
    tool_turn(log, 0);
    let parent = log.head().cloned();
    for message in [call("nested"), result("nested", "inner")] {
        let message = AgentMessage::tool_activity("cell".into(), message);
        let entry = LogEntryKind::Message { message };
        log.append(log.head().cloned(), ThreadKind::Main, None, entry)
            .unwrap();
    }
    log.append(parent, ThreadKind::Sub, Some(1), message("child"))
        .unwrap();
    tool_turn(log, 1);
    let head = append(log, "thanks");
    let turns = [turn_lines(0), turn_lines(1)].concat();
    vec![(head, [turns, vec!["user: thanks".into()]].concat())]
}

/// Without a compaction the context is the whole branch, however long, and
/// a branch forked early has its own.
fn a_long_session_with_a_fork(log: &mut SessionLog) -> Vec<(String, Vec<String>)> {
    let mut fork = String::new();
    for turn in 0..300 {
        tool_turn(log, turn);
        if turn == 50 {
            fork = log.head().cloned().unwrap();
        }
    }
    let tip = log.head().cloned().unwrap();
    log.set_head(fork).unwrap();
    let branch = append(log, "branch prompt");
    log.set_head(tip).unwrap();
    let head = append(log, "main prompt");
    let turns = |turns: std::ops::Range<usize>| turns.flat_map(turn_lines).collect::<Vec<_>>();
    vec![
        (
            head,
            [turns(0..300), vec!["user: main prompt".into()]].concat(),
        ),
        (
            branch,
            [turns(0..51), vec!["user: branch prompt".into()]].concat(),
        ),
    ]
}

/// A compaction replaces what precedes its first kept entry with the users
/// it retained and its summary. A branch from before it is not compacted,
/// and one from after it shares it.
fn a_session_compacted_long_ago(log: &mut SessionLog) -> Vec<(String, Vec<String>)> {
    let mut prompts = Vec::new();
    let mut later = String::new();
    for turn in 0..300 {
        prompts.push(tool_turn(log, turn));
        if turn == 250 {
            let (kept, retained) = (prompts[100].clone(), vec![prompts[3].clone()]);
            let usage = Default::default();
            log.append_compaction(
                ThreadFilter::MAIN,
                "SUMMARY".into(),
                kept,
                retained,
                1,
                None,
                usage,
            )
            .unwrap();
        }
        if turn == 260 {
            later = log.head().cloned().unwrap();
        }
    }
    let head = log.head().cloned().unwrap();
    let turns = |turns: std::ops::Range<usize>| turns.flat_map(turn_lines).collect::<Vec<_>>();
    let compacted = |end| {
        let summary = ["retained:", "user: prompt 3", "summary"].map(String::from);
        [summary.to_vec(), turns(100..end)].concat()
    };
    vec![
        (head, compacted(300)),
        (later, compacted(261)),
        (
            prompts[50].clone(),
            [turns(0..50), vec!["user: prompt 50".into()]].concat(),
        ),
    ]
}

/// The model context read back from a store is the one the transcript
/// defines, for every branch, also of a session resumed from its JSONL file.
#[test]
fn the_model_context_read_back_is_what_the_transcript_says() {
    let scenarios: [Scenario; 3] = [
        a_short_session,
        a_long_session_with_a_fork,
        a_session_compacted_long_ago,
    ];
    for (scenario, jsonl) in scenarios
        .iter()
        .flat_map(|scenario| [(scenario, false), (scenario, true)])
    {
        let root = tempfile::tempdir().unwrap();
        let store = project(root.path(), "p");
        let mut expected = Vec::new();
        let mut write = |log: &mut SessionLog| {
            expected = scenario(log);
            for (head, lines) in &expected {
                assert!(&context(log, head) == lines, "the expectation is wrong");
            }
        };
        let id = if jsonl {
            jsonl_session(&store, write).0
        } else {
            let mut log = SessionLog::create(&store).unwrap();
            write(&mut log);
            log.session_id().to_owned()
        };

        let mut log = SessionLog::resume(&store, &id).unwrap();
        assert_eq!(log.head(), Some(&expected[0].0));
        for (index, (head, lines)) in expected.iter().enumerate() {
            // A head may only move to a branch the log holds.
            if let Some(read) = log.branch_read(head).unwrap() {
                log.install_branch(read.read().unwrap()).unwrap();
            }
            let read_back = context(&log, head);
            let first_difference = read_back.iter().zip(lines).position(|(a, b)| a != b);
            assert!(
                &read_back == lines,
                "jsonl={jsonl} branch {index}: {} lines for {}, first \
                 difference at {first_difference:?}",
                read_back.len(),
                lines.len(),
            );
        }
    }
}

/// A session of a few thousand entries from `seed`: tool turns, model
/// changes, background sub-agents whose entries fall between the main
/// agent's, compactions and branches. Returns the heads of the branches
/// it left behind.
fn a_generated_session(log: &mut SessionLog, seed: u64) -> Vec<String> {
    use aj_agent::events::AgentSettings;
    use aj_models::types::{StopReason, Usage};

    // Odd and distinct for every seed, as the generator needs a nonzero state.
    let mut state = seed * 2 + 1;
    let mut roll = |below: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % below
    };
    let said = |text: &str, tokens: u64| {
        let usage = Usage {
            input: tokens,
            output: 1,
            ..Usage::default()
        };
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(aj_models::types::TextContent {
                text: text.into(),
                text_signature: None,
            })],
            usage,
            stop_reason: StopReason::Stop,
            ..AssistantMessage::empty()
        })
    };
    let settings = AgentSettings {
        context_window: 1000,
        provider: "p".into(),
        model_id: "m".into(),
        thinking: "off".into(),
        thinking_display: String::new(),
        speed: "default".into(),
        verbosity: "default".into(),
    };
    let sub = |log: &mut SessionLog, n: usize, message: Message| {
        let parent = log.latest_leaf(ThreadFilter::sub(n));
        let message = AgentMessage::wire(message);
        let entry = LogEntryKind::Message { message };
        log.append(parent, ThreadKind::Sub, Some(n), entry).unwrap();
    };

    let (mut prompts, mut left) = (Vec::new(), Vec::new());
    // The running sub-agents, each with the call it has not answered yet.
    let mut running: Vec<(usize, Option<String>)> = Vec::new();
    // The sub-agents whose run has ended with an entry of it.
    let mut finished: Vec<usize> = Vec::new();
    let (mut spawned, mut compacted_at) = (0, 0);
    // The main agent's call that is still running, during which only the
    // sub-agents write.
    let mut calling: Option<String> = None;
    for step in 0..900 {
        let action = match (roll(20), &calling) {
            (action @ (9..=15 | 18 | 19), _) | (action, None) => action,
            _ => continue,
        };
        match action {
            0..=7 => {
                prompts.push(tool_turn(log, step));
                add(log, said(&format!("answer {step}"), 10 + roll(90)));
            }
            // A step of a long turn: no prompt stands between the
            // sub-agents' entries around it.
            18 | 19 if !prompts.is_empty() => match calling.take() {
                Some(id) => drop(add(log, result(&id, "more"))),
                None => {
                    let id = format!("call-{step}");
                    add(log, call(&id));
                    calling = Some(id);
                }
            },
            8 => {
                let model = format!("m{step}");
                log.append_model_change(ThreadFilter::MAIN, "p", &model, 1000)
                    .unwrap();
            }
            9 | 10 if running.len() < 3 && !prompts.is_empty() => {
                spawned += 1;
                let head = log.head().cloned().unwrap();
                let task = format!("task {spawned}");
                log.append_subagent_spawn(spawned, head, &task, "agent", None, true, &settings)
                    .unwrap();
                sub(log, spawned, Message::User(UserMessage::text(task)));
                running.push((spawned, None));
            }
            // The user talks to a sub-agent whose run is over.
            9..=15 if !finished.is_empty() && step % 3 == 0 => {
                let n = finished[usize::try_from(roll(97)).unwrap() % finished.len()];
                sub(
                    log,
                    n,
                    Message::User(UserMessage::text(format!("more {step}"))),
                );
                sub(log, n, said(&format!("continued {step}"), 4 + roll(5)));
            }
            9..=15 if !running.is_empty() => {
                let index = usize::try_from(roll(3)).unwrap() % running.len();
                let (n, waiting) = running[index].clone();
                match waiting {
                    Some(id) => {
                        sub(log, n, result(&id, &format!("sub output {step}")));
                        running[index].1 = None;
                    }
                    None if roll(4) == 0 => {
                        sub(log, n, said(&format!("report {step}"), 5 + roll(20)));
                        running.remove(index);
                        // Some runs end without an entry, as one cut off
                        // by a crash does.
                        if roll(4) != 0 {
                            let done = aj_agent::events::SubAgentConclusion::Completed;
                            log.append_subagent_end(n, done, &format!("report {step}"))
                                .unwrap();
                            finished.push(n);
                        }
                    }
                    None => {
                        let id = format!("sub-{n}-{step}");
                        let mut asked = call(&id);
                        if let Message::Assistant(asked) = &mut asked {
                            asked.stop_reason = StopReason::ToolUse;
                            asked.usage.input = 3 + roll(9);
                            // Text beside the call is the run's report
                            // until its bracket closes.
                            let text = aj_models::types::TextContent {
                                text: format!("working {step}"),
                                text_signature: None,
                            };
                            asked.content.insert(0, AssistantContent::Text(text));
                        }
                        sub(log, n, asked);
                        running[index].1 = Some(id);
                    }
                }
            }
            16 if prompts.len() > compacted_at + 40 => {
                let (kept, retained) = (prompts.len() - 12, prompts.len() - 30);
                let (kept, retained) = (prompts[kept].clone(), vec![prompts[retained].clone()]);
                let usage = Usage {
                    input: 7,
                    ..Usage::default()
                };
                log.append_compaction(
                    ThreadFilter::MAIN,
                    "SUMMARY".into(),
                    kept,
                    retained,
                    9,
                    None,
                    usage,
                )
                .unwrap();
                compacted_at = prompts.len();
            }
            17 if step % 7 == 0 && prompts.len() > 60 => {
                left.push(log.head().cloned().unwrap());
                // Mostly a recent turn, sometimes one far back.
                let back = if roll(3) == 0 { 50 } else { 3 };
                prompts.truncate(prompts.len() - back);
                compacted_at = compacted_at.min(prompts.len());
                log.set_head(prompts.last().cloned().unwrap()).unwrap();
                running.clear();
                finished.clear();
            }
            _ => {}
        }
    }
    if let Some(id) = calling {
        add(log, result(&id, "more"));
    }
    prompts.push(tool_turn(log, 900));
    left
}

/// A reopened log holds part of its session, and everything read through it
/// is what the whole session says: the model context, the recent tail, each
/// page of earlier history of each agent, and the same again after a switch
/// to another branch. Only the occupancy a compaction reports may differ
/// (see `project_suffix`).
#[test]
fn a_partly_held_session_reads_like_the_whole_one() {
    use aj_agent::events::AgentEvent;
    use aj_session::{LogSnapshot, TaggedEvent, project_page, project_suffix};
    use std::collections::BTreeSet;

    fn render(events: impl Iterator<Item = TaggedEvent>) -> Vec<String> {
        let events = events.map(|mut tagged| {
            if let AgentEvent::CompactionEnd { tokens_after, .. } = &mut tagged.event {
                *tokens_after = 0;
            }
            let settings = &tagged.branch_settings;
            format!("{:?} {:?} {settings:?}", tagged.entry, tagged.event)
        });
        // A replayed assistant `MessageStart` carries a throwaway id.
        events
            .filter(|event| !event.starts_with("None MessageStart"))
            .collect()
    }
    fn same(what: &str, held: Vec<String>, whole: Vec<String>) {
        let differs = held.iter().zip(&whole).position(|(a, b)| a != b);
        let at = differs.map(|at| (&held[at], &whole[at]));
        let lengths = (held.len(), whole.len());
        assert!(
            held == whole,
            "{what}: {lengths:?}, first difference {at:#?}"
        );
    }
    /// Compare everything a client and the model read of the selected
    /// branch. Returns how many entries the two logs hold.
    fn compare(
        seed: u64,
        store: &ProjectSessions,
        held: &LogSnapshot,
        whole: &LogSnapshot,
    ) -> (usize, usize) {
        let head = whole.head().unwrap();
        assert_eq!(held.head(), Some(head));
        let context = |log: &LogSnapshot| {
            let messages = log.linearize(head, ThreadFilter::MAIN).agent_messages();
            // A synthesized message has a fresh id on every read.
            let wire = messages.iter().map(AgentMessage::to_projected_wire);
            wire.map(|message| format!("{message:?}")).collect()
        };
        same(&format!("{seed} context"), context(held), context(whole));
        assert_eq!(held.settings_at(head), whole.settings_at(head), "{seed}");
        let start = whole.display_start();
        assert_eq!(held.display_start(), start, "{seed}");
        let cursor = (start > 1).then(|| start - 1);
        let subs = whole.sub_agent_ids();
        for live in [BTreeSet::new(), subs.clone()] {
            let tail = |log| render(project_suffix(log, cursor, &live));
            same(&format!("{seed} tail"), tail(held), tail(whole));
            // A client that last saw the entry before the window's start.
            let window = held.window_start().checked_sub(1).filter(|at| *at > 0);
            let rest = |log| render(project_suffix(log, window, &live));
            same(&format!("{seed} window"), rest(held), rest(whole));
            // Every few sub-agents, once: each page read walks the session.
            let paged = subs.iter().step_by(5).map(|n| AgentId::Sub(*n));
            let paged = paged.filter(|_| live.is_empty());
            for agent in [AgentId::Main].into_iter().chain(paged) {
                let mut before = start;
                while before > 1 {
                    let mut page = held.clone();
                    let from = store.load_page(&mut page, agent, before, 150).unwrap();
                    assert_eq!(
                        from,
                        whole.page_start(agent, before, 150),
                        "{seed} {agent:?}"
                    );
                    let events = |log| render(project_page(log, agent, from, before, &live));
                    let what = format!("{seed} {agent:?} page {from}..{before}");
                    same(&what, events(&page), events(whole));
                    before = from;
                }
            }
        }
        (held.held(), whole.held())
    }

    /// The next compaction of the selected branch keeps and summarizes
    /// the same messages. It chooses among every message the user sent,
    /// which a log reads when one is planned. Returns whether there was
    /// something to compact and whether a message from before the held
    /// log's window was kept.
    fn plan(seed: u64, held: &mut SessionLog, whole: &LogSnapshot) -> (bool, bool) {
        let window = held.snapshot().window_start();
        held.hold_user_messages().unwrap();
        let held = held.snapshot();
        let head = whole.head().unwrap();
        let plan = |log: &LogSnapshot| {
            let branch = log.linearize(head, ThreadFilter::MAIN);
            let plan = aj_session::prepare_compaction(&branch, 300);
            plan.map(|plan| {
                let summarized = format!("{:?}", plan.messages_to_summarize);
                (
                    plan.first_kept_entry_id,
                    plan.retained_user_entry_ids,
                    summarized,
                )
            })
        };
        let planned = plan(whole);
        assert_eq!(plan(&held), planned, "{seed}");
        let position = |id: &String| whole.entries_in_order().iter().position(|e| e.id == *id);
        let kept = planned.iter().flat_map(|(_, kept, _)| kept);
        let early = kept
            .filter_map(position)
            .any(|at| u64::try_from(at).unwrap() + 1 < window);
        (planned.is_some(), early)
    }

    let (mut partial, mut planned, mut early) = (0, 0, 0);
    for seed in 1..=4 {
        let root = tempfile::tempdir().unwrap();
        let store = project(root.path(), "p");
        let mut log = SessionLog::create(&store).unwrap();
        log.set_system_prompt("system".into()).unwrap();
        let left = a_generated_session(&mut log, seed);
        let id = log.session_id().to_owned();
        // What the appending log holds is the whole session.
        let written = log.snapshot();
        assert!(written.is_complete());
        drop(log);

        let mut held = SessionLog::resume(&store, &id).unwrap();
        let mut whole = SessionLog::resume(&store, &id).unwrap();
        whole.load_all().unwrap();
        assert_eq!(
            serde_json::to_value(whole.entries_in_order()).unwrap(),
            serde_json::to_value(written.entries_in_order()).unwrap(),
        );
        // One writer at a time: the session refuses a second one's writes.
        let before = whole.snapshot();
        drop(whole);
        let (some, all) = compare(seed, &store, &held.snapshot(), &before);
        assert_eq!(all, before.len());
        partial += usize::from(some * 2 < all);
        let (some, kept_early) = plan(seed, &mut held, &before);
        planned += usize::from(some);
        early += usize::from(kept_early);
        // The messages read for the plan change nothing a reader sees.
        compare(seed, &store, &held.snapshot(), &before);

        if let Some(head) = left.last() {
            if let Some(read) = held.branch_read(head).unwrap() {
                held.install_branch(read.read().unwrap()).unwrap();
            }
            held.set_head(head.clone()).unwrap();
            let mut whole = SessionLog::resume(&store, &id).unwrap();
            whole.load_all().unwrap();
            whole.set_head(head.clone()).unwrap();
            let whole = whole.snapshot();
            compare(seed, &store, &held.snapshot(), &whole);
            let (some, kept_early) = plan(seed, &mut held, &whole);
            planned += usize::from(some);
            early += usize::from(kept_early);
        }
    }
    assert!(partial >= 3, "only {partial} sessions reopened in part");
    // The plans compared were of something, and some kept a message that
    // the reopened log did not hold before it planned.
    assert!(
        planned >= 4,
        "only {planned} branches had something to compact"
    );
    assert!(
        early >= 2,
        "only {early} plans kept a message from before the window"
    );
}

/// A sub-agent run that ended before the window starts is not held, and a
/// view that starts there shows no box for it, from the reopened log and
/// from the whole session alike.
#[test]
fn a_run_finished_before_the_window_shows_no_box() {
    use aj_agent::events::{AgentEvent, AgentSettings};
    use aj_session::project_suffix;
    use std::collections::BTreeSet;

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut log = SessionLog::create(&store).unwrap();
    log.set_system_prompt("system".into()).unwrap();
    tool_turn(&mut log, 0);
    add(&mut log, call("long"));
    let settings = AgentSettings {
        context_window: 0,
        provider: "p".into(),
        model_id: "m".into(),
        thinking: "off".into(),
        thinking_display: String::new(),
        speed: "default".into(),
        verbosity: "default".into(),
    };
    let head = log.head().cloned().unwrap();
    log.append_subagent_spawn(1, head, "task", "agent", None, false, &settings)
        .unwrap();
    let sub = |log: &mut SessionLog, message: Message| {
        let parent = log.latest_leaf(ThreadFilter::sub(1));
        let message = AgentMessage::wire(message);
        let entry = LogEntryKind::Message { message };
        log.append(parent, ThreadKind::Sub, Some(1), entry).unwrap();
    };
    sub(&mut log, Message::User(UserMessage::text("task")));
    sub(&mut log, call("inner"));
    sub(&mut log, result("inner", "done"));
    add(&mut log, result("long", "done"));
    let kept = append(&mut log, "kept");
    let start = log.last_seq();
    for turn in 1..150 {
        tool_turn(&mut log, turn);
    }
    log.append_compaction(
        ThreadFilter::MAIN,
        "SUMMARY".into(),
        kept,
        vec![],
        1,
        None,
        Default::default(),
    )
    .unwrap();
    tool_turn(&mut log, 150);
    let id = log.session_id().to_owned();
    drop(log);

    let mut log = SessionLog::resume(&store, &id).unwrap();
    let held = log.snapshot();
    assert_eq!(held.window_start(), start);
    let brackets = |log: &aj_session::LogSnapshot| -> Vec<String> {
        let events = project_suffix(log, Some(start - 1), &BTreeSet::new());
        events
            .filter_map(|tagged| match tagged.event {
                AgentEvent::SubAgentStart { .. } => Some("start".to_string()),
                AgentEvent::SubAgentEnd { report, .. } => Some(format!("end {report:?}")),
                _ => None,
            })
            .collect()
    };
    assert_eq!(brackets(&held), [""; 0]);
    log.load_all().unwrap();
    assert_eq!(brackets(&log.snapshot()), [""; 0]);
}

/// Nothing stores which thread a session is on. A reopen lands on
/// the main thread last appended to, also when a sub-agent wrote
/// the session's last entry.
#[test]
fn a_reopen_lands_on_the_main_thread_last_written() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut log = SessionLog::create(&store).unwrap();
    let a = append(&mut log, "a");
    let b = append(&mut log, "b");
    let settings = aj_agent::events::AgentSettings {
        context_window: 0,
        provider: "p".into(),
        model_id: "m".into(),
        thinking: "off".into(),
        thinking_display: String::new(),
        speed: "default".into(),
        verbosity: "default".into(),
    };
    log.append_subagent_spawn(1, b, "task", "agent", None, true, &settings)
        .unwrap();
    log.set_head(a).unwrap();
    let last = append(&mut log, "a branch");
    // The session's last entry is a sub-agent's.
    log.append_subagent_spawn(2, last.clone(), "task", "agent", None, true, &settings)
        .unwrap();
    let id = log.session_id().to_owned();
    drop(log);
    let log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.head(), Some(&last));
}

/// SQLite replays a write-ahead log into the database that carries its
/// name, so a database is not created beside one that lost its own.
#[test]
fn a_database_is_not_created_beside_another_ones_log() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut log = SessionLog::create(&store).unwrap();
    append(&mut log, "old database");
    let wal = std::path::PathBuf::from(format!("{}-wal", store.sqlite_path().display()));
    // The database is moved aside while a process still has it open.
    let aside = root.path().join("aside.sqlite3");
    std::fs::rename(store.sqlite_path(), &aside).unwrap();
    assert!(wal.exists());

    let mut new = SessionLog::create(&store).unwrap();
    let parent = new.head().cloned();
    let error = new
        .append(parent, ThreadKind::Main, None, message("new database"))
        .unwrap_err();
    assert!(error.to_string().contains("-wal"), "{error}");
    assert!(!store.sqlite_path().exists());
}

/// Read connections are kept between reads, but only for the database file
/// they were opened on. A database that was removed or replaced at its path
/// is not answered from an old connection.
#[test]
fn a_reader_answers_from_the_database_now_at_its_path() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let listed = |store: &ProjectSessions| -> Vec<String> {
        let sessions = store.list_sessions().unwrap();
        sessions.into_iter().map(|s| s.session_id).collect()
    };
    let remove_database = || {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(root.path().join(format!("sessions.sqlite3{suffix}")));
        }
    };

    let mut first = SessionLog::create(&store).unwrap();
    append(&mut first, "first");
    let first = first.session_id().to_owned();
    for _ in 0..3 {
        assert_eq!(listed(&store), std::slice::from_ref(&first));
    }

    // Replaced with no read in between, so a connection to the old file
    // is still idle when the next reader comes.
    remove_database();
    let mut second = SessionLog::create(&store).unwrap();
    append(&mut second, "second");
    assert_eq!(listed(&store), [second.session_id().to_owned()]);

    remove_database();
    assert!(listed(&store).is_empty(), "no database, no sessions");
}

fn an_input(text: &str) -> (aj_agent::queue::PendingKind, aj_agent::queue::PendingInput) {
    let queues = aj_agent::queue::MessageQueues::default();
    queues.append_follow_up(aj_agent::events::AgentId::Main, text);
    queues.pending(aj_agent::events::AgentId::Main).unwrap()
}

#[test]
fn a_pending_input_is_saved_or_the_save_fails() {
    use aj_agent::events::AgentId::{Main, Sub};
    use aj_session::Pending;

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut log = SessionLog::create(&store).unwrap();
    let id = log.session_id().to_owned();

    // A session that is not stored has no thread to save a message
    // under, for any agent. The save says so and nothing waits in memory.
    let pending = log.pending();
    let (kind, kept) = an_input("kept");
    let (_, withdrawn) = an_input("withdrawn");
    let (_, early) = an_input("early");
    for agent in [Main, Sub(7)] {
        let refused = pending.save_input(agent, kind, &early).unwrap_err();
        let refused = refused.to_string();
        assert!(refused.contains("has no thread"), "{refused}");
    }
    pending.forget(&early.id).unwrap();
    append(&mut log, "first");
    pending.save_input(Main, kind, &kept).unwrap();
    pending.save_input(Main, kind, &withdrawn).unwrap();
    pending.forget(&withdrawn.id).unwrap();
    // No sub-agent has a thread, so its message still has no place.
    assert!(pending.save_input(Sub(7), kind, &kept).is_err());
    drop((log, pending));

    let log = SessionLog::resume(&store, &id).unwrap();
    let restored = log.restore_pending(&log.pending(), AgentId::Main).unwrap();
    match restored.as_slice() {
        [Pending::Input(Main, _, input)] => assert_eq!(input.text(), "kept"),
        other => panic!("{other:?}"),
    }
}

/// The append that delivers pending work removes its saved copy in the same
/// transaction: when the copy cannot be removed, the message is not stored.
#[test]
fn a_delivery_that_cannot_retire_its_pending_row_stores_nothing() {
    use aj_agent::events::AgentId::Main;
    use aj_session::Pending;

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut log = SessionLog::create(&store).unwrap();
    append(&mut log, "first");
    let id = log.session_id().to_owned();
    let (kind, input) = an_input("queued");
    log.pending().save_input(Main, kind, &input).unwrap();

    store.refuse_sqlite_pending_writes();
    let delivered = LogEntryKind::Message {
        message: input.clone().into_message(),
    };
    let head = log.head().cloned();
    assert!(
        log.append(head, ThreadKind::Main, None, delivered.clone())
            .is_err()
    );
    drop(log);

    let saved = |log: &SessionLog| match log.restore_pending(&log.pending(), AgentId::Main) {
        Ok(restored) => restored.len(),
        Err(error) => panic!("{error}"),
    };
    let mut log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.last_seq(), 1, "the message was stored without retiring");
    match log
        .restore_pending(&log.pending(), AgentId::Main)
        .unwrap()
        .as_slice()
    {
        [Pending::Input(Main, _, saved)] => assert_eq!(saved.id, input.id),
        other => panic!("{other:?}"),
    }
    // The refused append left the session's version alone, so the next
    // one is not taken for a stale writer's.
    store.allow_sqlite_pending_writes();
    let head = log.head().cloned();
    log.append(head, ThreadKind::Main, None, delivered).unwrap();
    assert_eq!((log.last_seq(), saved(&log)), (2, 0));
}

/// A session comes to exist with its label and archive marker, in the
/// transaction of its first entry.
#[test]
fn a_session_is_created_with_what_waited_for_it_or_not_at_all() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    // The database exists before the fault below is installed in it.
    append(&mut SessionLog::create(&store).unwrap(), "another");

    // The label and the archive marker are part of the row the session is
    // created as, not a write that follows it.
    let mut log = SessionLog::create(&store).unwrap();
    let id = log.session_id().to_owned();
    log.set_tag(Some("label")).unwrap();
    log.set_archived(true).unwrap();
    store.refuse_sqlite_annotation_writes();
    append(&mut log, "first");
    drop(log);
    assert_eq!(store.read_tag(&id).unwrap().as_deref(), Some("label"));
    assert!(store.read_archived(&id).unwrap());
    store.allow_sqlite_annotation_writes();
}

/// What the store keeps of a session's threads, in the order they
/// started: each one's kind, the thread it continues from by its
/// place in this list with the position it continues at, and the IDs of
/// its entries.
type Threads = Vec<(String, Option<(usize, u64)>, Vec<String>)>;

fn threads(store: &ProjectSessions, id: &str) -> Threads {
    let raw = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let mut rows = raw
        .prepare(
            "SELECT t.id, t.kind, t.parent, t.forked_at FROM threads t
             JOIN sessions s ON s.id = t.session
             WHERE s.project = ?1 AND s.name = ?2 ORDER BY t.id",
        )
        .unwrap();
    let rows: Vec<(i64, String, Option<i64>, Option<i64>)> = rows
        .query_map([store.project_id(), id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let place = |thread: i64| rows.iter().position(|row| row.0 == thread).unwrap();
    let mut records = raw
        .prepare("SELECT entry_id FROM entries WHERE thread = ?1 ORDER BY seq")
        .unwrap();
    rows.iter()
        .map(|(thread, kind, parent, forked_at)| {
            let ids = records.query_map([thread], |row| row.get(0)).unwrap();
            (
                kind.clone(),
                parent.map(|parent| (place(parent), u64::try_from(forked_at.unwrap()).unwrap())),
                ids.collect::<Result<_, _>>().unwrap(),
            )
        })
        .collect()
}

/// Move the head to `head`, reading its branch first when the log does not
/// hold it.
fn switch(log: &mut SessionLog, head: &String) {
    if let Some(read) = log.branch_read(head).unwrap() {
        log.install_branch(read.read().unwrap()).unwrap();
    }
    log.set_head(head.clone()).unwrap();
}

/// A session with a system prompt and the entry of what it was assembled
/// from, as a host creates one. Returns the IDs of the two entries.
fn a_prompted_session(store: &ProjectSessions) -> (SessionLog, String, String) {
    let mut log = SessionLog::create(store).unwrap();
    let prompt = log.set_system_prompt("be brief".into()).unwrap().id;
    let context = aj_session::SessionContext {
        system_prompt: None,
        files: Vec::new(),
        skills: Vec::new(),
    };
    let context = log.append_context(context).unwrap().id;
    (log, prompt, context)
}

/// A session is one thread from the system prompt on. Starting over
/// at the prompt starts a second one that continues from the prompt, and
/// each branch reads back with its own model context.
#[test]
fn a_session_begins_in_one_thread_and_starting_over_branches_from_the_prompt() {
    use aj_agent::events::AgentEvent;
    use aj_session::project_suffix;
    use std::collections::BTreeSet;

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let (mut log, prompt, context_record) = a_prompted_session(&store);
    let id = log.session_id().to_owned();
    assert!(store.list_sessions().unwrap().is_empty(), "not stored yet");
    let ids = |log: &SessionLog| -> Vec<String> {
        let entries = log.snapshot();
        let entries = entries.entries_in_order();
        entries.iter().map(|entry| entry.id.clone()).collect()
    };
    tool_turn(&mut log, 0);
    let first = ids(&log);
    assert_eq!(first[..2], [prompt.clone(), context_record.clone()]);
    assert_eq!(first.len(), 5);
    let one = |ids: &[String]| vec![("main".to_owned(), None, ids.to_vec())];
    assert_eq!(threads(&store, &id), one(&first));

    // A reopened session goes on in the same thread.
    drop(log);
    let mut log = SessionLog::resume(&store, &id).unwrap();
    tool_turn(&mut log, 1);
    let tip = log.head().unwrap().clone();
    log.load_all().unwrap();
    let mut first = ids(&log);
    assert_eq!(first.len(), 8);
    assert_eq!(threads(&store, &id), one(&first));
    let whole: Vec<String> = [turn_lines(0), turn_lines(1)].concat();
    assert_eq!(context(&log, &tip), whole);

    // The prompt can be a head. The entry beside it cannot.
    assert!(matches!(
        log.set_head(context_record),
        Err(SessionError::InvalidHead(_))
    ));
    log.set_head(prompt.clone()).unwrap();
    assert!(context(&log, &prompt).is_empty());
    tool_turn(&mut log, 2);
    let second: Vec<String> = ids(&log)[first.len()..].to_vec();
    let branch = log.head().unwrap().clone();
    let mut expected = one(&first);
    expected.push(("main".to_owned(), Some((0, 1)), second.clone()));
    assert_eq!(threads(&store, &id), expected);
    // A second start over is another branch from the prompt.
    log.set_head(prompt.clone()).unwrap();
    let third = append(&mut log, "third start");
    expected.push(("main".to_owned(), Some((0, 1)), vec![third.clone()]));
    assert_eq!(threads(&store, &id), expected);
    log.set_head(branch.clone()).unwrap();
    drop(log);

    // A reopen is on the thread last appended to: the switch stored
    // nothing. Each branch has the model context it had, and the view of
    // the branch from the prompt opens with the session's context notice.
    let mut log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.head(), Some(&third));
    switch(&mut log, &branch);
    let snapshot = log.snapshot();
    let events = project_suffix(&snapshot, None, &BTreeSet::new());
    let notices = events.filter(|tagged| {
        matches!(&tagged.event, AgentEvent::Notice { text, .. } if text.starts_with("Context:"))
    });
    assert_eq!(notices.count(), 1);
    for (head, lines) in [
        (&branch, turn_lines(2).to_vec()),
        (&tip, whole),
        (&third, vec!["user: third start".to_owned()]),
        (&prompt, Vec::new()),
    ] {
        switch(&mut log, head);
        assert_eq!(context(&log, head), lines, "the branch ending at {head}");
    }
    // The switch to the prompt started no branch: no entry is there yet.
    assert_eq!(threads(&store, &id), expected);
    // The next message on the first thread still continues it.
    switch(&mut log, &tip);
    first.push(append(&mut log, "on"));
    expected[0].2 = first;
    assert_eq!(threads(&store, &id), expected);
}

/// How many writes the session database has committed (see
/// [`ProjectSessions::committed_writes`]) and how many
/// threads the session has.
fn selection(store: &ProjectSessions, id: &str) -> (u64, i64) {
    let raw = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let sql = "SELECT COUNT(*) FROM threads t JOIN sessions s ON s.id = t.session
               WHERE s.project = ?1 AND s.name = ?2";
    let count = raw
        .query_row(sql, [store.project_id(), id], |row| row.get(0))
        .unwrap();
    (store.committed_writes(), count)
}

/// A branch is a draft until its first work. A switch to an earlier entry
/// stores nothing, and neither does a settings entry appended there. A
/// process that ends there reopens on the thread last appended to.
/// The first message creates the branch with the entries that waited. A
/// switch to a thread's last entry stores nothing either, so a
/// reopen follows what was appended and not what was looked at.
#[test]
fn a_fork_is_a_draft_until_its_first_message_and_no_switch_is_stored() {
    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let mut log = SessionLog::create(&store).unwrap();
    let first = append(&mut log, "first");
    let tip = append(&mut log, "second");
    let id = log.session_id().to_owned();
    // A connection that stays open, so closing the log does not restart
    // the write-ahead log the writes are counted in.
    let open = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let sessions = "SELECT COUNT(*) FROM sessions";
    assert_eq!(open.query_row(sessions, [], |row| row.get(0)), Ok(1));

    let before = selection(&store, &id);
    assert_eq!(before.1, 1);
    log.set_head(first.clone()).unwrap();
    assert_eq!(log.head(), Some(&first));
    assert!(log.is_draft());
    let waiting = log
        .append_model_change(ThreadFilter::MAIN, "provider", "abandoned", 1000)
        .unwrap();
    assert_eq!(log.head(), Some(&waiting.id), "reads see the draft");
    assert_eq!(selection(&store, &id), before, "the draft wrote");
    drop(log);
    let mut log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.head(), Some(&tip), "the draft outlived its process");
    assert!(!log.contains(&waiting.id));

    // A draft that is left for another entry is forgotten.
    log.set_head(first.clone()).unwrap();
    let left = log
        .append_model_change(ThreadFilter::MAIN, "provider", "left", 1000)
        .unwrap();
    switch(&mut log, &tip);
    assert!(!log.is_draft() && !log.contains(&left.id));
    assert_eq!(log.last_seq(), 2);

    // The first message creates the branch, with the settings entry that
    // waited for it, in one write.
    log.set_head(first.clone()).unwrap();
    let setting = log
        .append_model_change(ThreadFilter::MAIN, "provider", "model", 1000)
        .unwrap();
    assert_eq!((setting.seq, log.stored_seq()), (3, 2));
    assert_eq!(selection(&store, &id), before);
    let on = append(&mut log, "on the branch");
    assert!(!log.is_draft());
    assert_eq!(selection(&store, &id), (before.0 + 1, 2));
    let filed = threads(&store, &id);
    let records = vec![setting.id.clone(), on.clone()];
    assert_eq!(filed[1], ("main".to_owned(), Some((0, 1)), records));
    assert_eq!(log.stored_seq(), 4);

    // The end of a thread is switched to without a write, so a
    // reopen is on the branch, which was appended to last.
    let written = selection(&store, &id);
    switch(&mut log, &tip);
    assert_eq!(selection(&store, &id), written, "the switch wrote");
    drop(log);
    let mut log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.head(), Some(&on));
    // Something done after a switch makes it last.
    switch(&mut log, &tip);
    let later = append(&mut log, "third");
    drop(log);
    let log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.head(), Some(&later));
    assert_eq!(selection(&store, &id).1, 2);
}

/// The first input accepted on a draft creates what it is for and is
/// saved with it, in one write: a session with the entries that waited,
/// or a branch. A process that ends before the input's message is appended
/// reopens on that thread with the input pending. A thread
/// left without an entry and without input is never landed on.
#[test]
fn the_first_input_creates_its_session_or_fork_and_is_saved_with_it() {
    use aj_session::Pending;

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let restored = |log: &SessionLog| -> Vec<String> {
        let work = log.restore_pending(&log.pending(), AgentId::Main).unwrap();
        let texts = work.into_iter().map(|work| match work {
            Pending::Input(AgentId::Main, _, input) => input.text(),
            other => panic!("unexpected {other:?}"),
        });
        texts.collect()
    };

    // A session: its prompt, context and label are stored with the input.
    let (mut log, prompt, context_record) = a_prompted_session(&store);
    let id = log.session_id().to_owned();
    log.set_tag(Some("kept")).unwrap();
    assert!(log.is_draft() && !log.is_durable());
    assert!(store.list_sessions().unwrap().is_empty());
    let (kind, input) = an_input("hello");
    log.accept(kind, &input).unwrap();
    assert!(!log.is_draft() && log.is_durable());
    assert_eq!(store.committed_writes(), 1, "one transaction");
    let created = vec![prompt.clone(), context_record];
    assert_eq!(threads(&store, &id), [("main".to_owned(), None, created)]);
    assert_eq!(restored(&log), ["hello"]);
    drop(log);
    let mut log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(restored(&log), ["hello"]);
    assert_eq!(store.read_tag(&id).unwrap().as_deref(), Some("kept"));
    // The message the input becomes continues the thread and takes
    // the saved copy with it.
    let hello = add_as(&mut log, &input.id, "hello");
    assert!(restored(&log).is_empty());
    assert_eq!(threads(&store, &id)[0].2.last(), Some(&hello));
    let tip = append(&mut log, "second");

    // A branch: the thread row, and the input.
    log.set_head(hello.clone()).unwrap();
    let before = selection(&store, &id);
    let (kind, input) = an_input("at the fork");
    log.accept(kind, &input).unwrap();
    assert_eq!(selection(&store, &id), (before.0 + 1, 2));
    assert_eq!(
        threads(&store, &id)[1],
        ("main".to_owned(), Some((0, 3)), Vec::new())
    );
    assert_eq!(log.head(), Some(&hello));
    assert_eq!(log.session_tree().segments.len(), 1, "no branch shows");
    drop(log);
    // The reopen lands on the branch, which holds no entry of its own, and
    // the next message is its first.
    let mut log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.head(), Some(&hello));
    assert_eq!(restored(&log), ["at the fork"]);
    assert_eq!(log.session_tree().segments.len(), 1);
    assert_eq!(log.latest_leaf(ThreadFilter::MAIN), Some(tip.clone()));
    let forked = add_as(&mut log, &input.id, "at the fork");
    assert!(restored(&log).is_empty());
    assert_eq!(threads(&store, &id)[1].2, std::slice::from_ref(&forked));
    assert_eq!(selection(&store, &id).1, 2, "the fork was continued");

    // A branch whose input is withdrawn before any message holds nothing. A
    // reopen is not on it, and no branch shows.
    log.set_head(hello.clone()).unwrap();
    let (kind, input) = an_input("withdrawn");
    log.accept(kind, &input).unwrap();
    log.pending().forget(&input.id).unwrap();
    assert_eq!(selection(&store, &id).1, 3);
    drop(log);
    let mut log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.head(), Some(&forked));
    assert!(restored(&log).is_empty());
    // Every reader of the whole session passes over it.
    log.load_all().unwrap();
    assert_eq!(log.len(), 5);
    assert_eq!(log.session_tree().segments.len(), 3);
    let exported = store.export_jsonl(&id, &root.path().join("out")).unwrap();
    let copy = project(root.path(), "copy");
    assert_eq!(copy.import_jsonl(&exported).unwrap(), id);
    assert_eq!(threads(&copy, &id).len(), 2);
    // Another branch at the same entry is its own thread.
    switch(&mut log, &hello);
    let again = append(&mut log, "again");
    let filed = threads(&store, &id);
    assert_eq!(filed.len(), 4);
    assert_eq!(filed[2].2, Vec::<String>::new());
    assert_eq!(filed[3], ("main".to_owned(), Some((0, 3)), vec![again]));
}

/// At an earlier entry the main agent has no thread until the branch
/// is created. Nothing accepts work for it there: a save that bypasses the
/// accepting path is an error and is not filed under the thread the
/// session was on. From the branch's creation on, work belongs to the branch.
#[test]
fn work_accepted_at_an_earlier_entry_belongs_to_the_branch_that_starts_there() {
    use aj_agent::events::AgentId::Main;
    use aj_agent::message::{TaskNotification, TaskNotificationKind, TaskOutcome};
    use aj_session::{Pending, TaskRecord};

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let (mut log, prompt, _) = a_prompted_session(&store);
    let tip = append(&mut log, "first");
    let id = log.session_id().to_owned();
    let restored = |log: &SessionLog| -> Vec<String> {
        let work = log.restore_pending(&log.pending(), AgentId::Main).unwrap();
        let texts = work.into_iter().map(|work| match work {
            Pending::Input(Main, _, input) => input.text(),
            other => panic!("unexpected {other:?}"),
        });
        texts.collect()
    };
    let recorded = |log: &SessionLog| {
        log.restore_tasks(&log.task_records(), AgentId::Main)
            .unwrap()
    };
    let task = TaskRecord {
        id: 1,
        owner: Main,
        call_id: "call".into(),
        command: "make".into(),
        pid: 4242,
        process_start: 99,
        output_path: "/tmp/out.log".into(),
        exit_path: "/tmp/out.exit".into(),
        started_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
    };
    let notice = AgentMessage::task_notification(TaskNotification::new(
        "agent run".into(),
        TaskNotificationKind::Bash,
        TaskOutcome::Succeeded,
        "done".into(),
    ));

    log.set_head(prompt.clone()).unwrap();
    let before = selection(&store, &id);
    let (kind, input) = an_input("on the branch");
    let (pending, tasks) = (log.pending(), log.task_records());
    let refusals = [
        ("queue save", pending.save_input(Main, kind, &input).err()),
        ("task start", tasks.start(&task).err()),
        ("notice", tasks.finish(9, Main, &notice).err()),
    ];
    for (write, refusal) in refusals {
        let refusal = refusal.unwrap_or_else(|| panic!("{write} was saved"));
        let refusal = refusal.to_string();
        assert!(refusal.contains("has no thread"), "{write}: {refusal}");
    }
    assert_eq!(selection(&store, &id), before);
    assert!(restored(&log).is_empty() && recorded(&log).is_empty());
    // Nothing was filed under the thread the session was on.
    switch(&mut log, &tip);
    assert!(restored(&log).is_empty() && recorded(&log).is_empty());

    // The first message at the entry starts the branch, and work accepted
    // from then on is the branch's.
    switch(&mut log, &prompt);
    let started = append(&mut log, "start over");
    pending.save_input(Main, kind, &input).unwrap();
    tasks.start(&task).unwrap();
    assert_eq!(restored(&log), ["on the branch"]);
    assert_eq!(recorded(&log), std::slice::from_ref(&task));
    switch(&mut log, &tip);
    assert!(restored(&log).is_empty() && recorded(&log).is_empty());
    // The prompt is where another branch would start.
    switch(&mut log, &prompt);
    assert!(restored(&log).is_empty() && recorded(&log).is_empty());
    switch(&mut log, &started);
    drop((log, pending, tasks));
    let log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.head(), Some(&started));
    assert_eq!(restored(&log), ["on the branch"]);
    assert_eq!(recorded(&log), [task]);
}

/// An export is the entries in append order as they were written, also
/// for a session with a branch from the system prompt, and importing it
/// files them under the same threads.
#[test]
fn a_session_with_a_branch_from_the_prompt_exports_what_was_appended() {
    let root = tempfile::tempdir().unwrap();
    let (live, copy) = (project(root.path(), "live"), project(root.path(), "copy"));
    let (mut log, prompt, _) = a_prompted_session(&live);
    tool_turn(&mut log, 0);
    log.set_head(prompt).unwrap();
    tool_turn(&mut log, 1);
    let id = log.session_id().to_owned();
    // The log has held every entry since it wrote the first.
    let snapshot = log.snapshot();
    assert!(snapshot.is_complete());
    let mut appended = String::new();
    for entry in snapshot.entries_in_order() {
        appended.push_str(&serde_json::to_string(entry).unwrap());
        appended.push('\n');
    }
    assert_eq!(appended.lines().count(), 8);
    drop(log);

    let file = live
        .export_jsonl(&id, &root.path().join("exported"))
        .unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), appended);
    assert_eq!(copy.import_jsonl(&file).unwrap(), id);
    let filed = threads(&live, &id);
    assert_eq!(filed.len(), 2);
    assert_eq!(filed[1].1, Some((0, 1)));
    assert_eq!(threads(&copy, &id), filed);
    let log = SessionLog::resume(&copy, &id).unwrap();
    let head = log.head().unwrap().clone();
    assert_eq!(context(&log, &head), turn_lines(1));
}

/// The turn state of each of the session's threads as stored, in the
/// order they started.
fn stored_turns(store: &ProjectSessions, id: &str) -> Vec<String> {
    let raw = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let sql = "SELECT t.turn FROM threads t JOIN sessions s ON s.id = t.session
               WHERE s.project = ?1 AND s.name = ?2 ORDER BY t.id";
    let mut rows = raw.prepare(sql).unwrap();
    let rows = rows.query_map([store.project_id(), id], |row| row.get(0));
    rows.unwrap().collect::<Result<_, _>>().unwrap()
}

/// A thread stores whether its turn is working. A log that comes to
/// be on one whose turn it does not run reads a working turn as
/// interrupted while the branch is unanswered, and stores it idle
/// otherwise. Interrupted is never stored.
#[test]
fn a_turn_is_stored_on_the_thread_the_session_is_on() {
    use aj_session::TurnState::{Idle, Interrupted, Working};

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let reopened = |id: &str| SessionLog::resume(&store, id).unwrap();

    // On a draft the main agent has no thread to hold a turn. A turn
    // that starts there creates the session with it.
    let mut log = SessionLog::create(&store).unwrap();
    let id = log.session_id().to_owned();
    let unheld = log.turn_state().set(AgentId::Main, Working);
    let unheld = unheld.expect_err("no thread").to_string();
    assert!(unheld.contains("has no thread"), "{unheld}");
    log.turn_state().set(AgentId::Main, Idle).unwrap();
    log.start_turn(AgentId::Main).unwrap();
    assert_eq!(stored_turns(&store, &id), ["working"]);
    let first = append(&mut log, "first");
    let tip = append(&mut log, "second");
    drop(log);
    assert_eq!(stored_turns(&store, &id), ["working"]);
    let mut log = reopened(&id);
    assert_eq!(log.turn_state().get(AgentId::Main), Interrupted);
    assert_eq!(stored_turns(&store, &id), ["working"]);

    // It stays with its thread: an earlier entry ends none, and
    // coming back reads it again.
    log.set_head(first.clone()).unwrap();
    assert_eq!(log.turn_state().get(AgentId::Main), Idle);
    log.set_head(tip.clone()).unwrap();
    assert_eq!(log.turn_state().get(AgentId::Main), Interrupted);

    // A turn that starts at an earlier entry creates the branch as working.
    // A process that ends before the branch's first entry leaves a
    // thread that holds nothing, and a reopen is not on it.
    log.set_head(first.clone()).unwrap();
    let before = selection(&store, &id);
    log.start_turn(AgentId::Main).unwrap();
    assert_eq!(log.turn_state().get(AgentId::Main), Working);
    assert_eq!(selection(&store, &id), (before.0 + 1, 2));
    drop(log);
    let mut log = reopened(&id);
    assert_eq!(log.head(), Some(&tip));
    assert_eq!(log.turn_state().get(AgentId::Main), Interrupted);
    // The turn's entries continue the branch it was stored on.
    log.set_head(first.clone()).unwrap();
    log.start_turn(AgentId::Main).unwrap();
    let branch = append(&mut log, "branch");
    assert_eq!(log.turn_state().get(AgentId::Main), Working);
    assert_eq!(stored_turns(&store, &id), ["working", "working", "working"]);
    assert_eq!(selection(&store, &id).1, 3);
    drop(log);
    let mut log = reopened(&id);
    assert_eq!(log.head(), Some(&branch));
    assert_eq!(
        log.turn_state().get(AgentId::Main),
        Interrupted,
        "stored with the branch"
    );
    switch(&mut log, &tip);
    assert_eq!(
        log.turn_state().get(AgentId::Main),
        Interrupted,
        "the other's stays"
    );

    // A tool call without a result is not an answer either. A completed
    // assistant message is: the process ended between it and the mark, so
    // the turn is over and is stored so.
    switch(&mut log, &branch);
    log.turn_state().set(AgentId::Main, Working).unwrap();
    add(&mut log, call("open"));
    drop(log);
    let mut log = reopened(&id);
    assert_eq!(log.turn_state().get(AgentId::Main), Interrupted);
    log.turn_state().set(AgentId::Main, Working).unwrap();
    add(&mut log, result("open", "output"));
    let answered = add(&mut log, Message::Assistant(AssistantMessage::empty()));
    switch(&mut log, &tip);
    assert_eq!(stored_turns(&store, &id), ["working", "working", "working"]);
    switch(&mut log, &answered);
    assert_eq!(log.turn_state().get(AgentId::Main), Idle);
    assert_eq!(stored_turns(&store, &id), ["working", "working", "idle"]);

    // Ending a turn stores it idle, and an append that is not at the head
    // starts a thread no turn has run on.
    switch(&mut log, &tip);
    log.turn_state().set(AgentId::Main, Working).unwrap();
    log.turn_state().set(AgentId::Main, Idle).unwrap();
    let aside = log
        .append(Some(first), ThreadKind::Main, None, message("aside"))
        .unwrap()
        .id;
    assert_eq!(log.turn_state().get(AgentId::Main), Idle);
    assert_eq!(
        stored_turns(&store, &id),
        ["idle", "working", "idle", "idle"]
    );
    drop(log);
    let log = reopened(&id);
    assert_eq!(log.head(), Some(&aside));
    assert_eq!(log.turn_state().get(AgentId::Main), Idle);
}

/// A sub-agent's thread stores its turn too: working from its spawn
/// entry on, idle once its process says so. A later log that picks the
/// run up reads a turn left working as it reads the main agent's. An
/// imported session's runs are idle.
#[test]
fn a_sub_agents_turn_is_stored_on_its_own_thread() {
    use aj_agent::events::AgentId::Sub;
    use aj_session::TurnState::{Idle, Interrupted, Working};

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let settings = aj_agent::events::AgentSettings {
        context_window: 0,
        provider: "p".into(),
        model_id: "m".into(),
        thinking: "off".into(),
        thinking_display: String::new(),
        speed: "default".into(),
        verbosity: "default".into(),
    };
    let mut log = SessionLog::create(&store).unwrap();
    let id = log.session_id().to_owned();
    let prompt = append(&mut log, "prompt");
    for agent in [1, 2] {
        log.append_subagent_spawn(
            agent,
            prompt.clone(),
            "task",
            "agent",
            None,
            true,
            &settings,
        )
        .unwrap();
    }
    let turns = log.turn_state();
    assert_eq!(
        (turns.get(Sub(1)), turns.working_subs()),
        (Working, vec![1, 2])
    );
    assert_eq!(stored_turns(&store, &id), ["idle", "working", "working"]);
    // The second run got as far as its task. The first ended its turn.
    let task = log.latest_leaf(ThreadFilter::sub(2));
    log.append(task, ThreadKind::Sub, Some(2), message("task"))
        .unwrap();
    turns.set(Sub(1), Idle).unwrap();
    assert_eq!(turns.working_subs(), [2]);
    assert_eq!(stored_turns(&store, &id), ["idle", "idle", "working"]);
    drop((log, turns));

    // A later log is told which runs were left working, and reads one when
    // it picks the run up: interrupted while its task is unanswered.
    let mut log = SessionLog::resume(&store, &id).unwrap();
    let left = log.sub_agents_with_work().unwrap();
    assert_eq!(
        (left.len(), left[0].agent_id, left[0].working),
        (1, 2, true)
    );
    let turns = log.turn_state();
    assert_eq!((turns.get(Sub(2)), turns.working_subs()), (Idle, vec![]));
    assert_eq!(log.pick_up_turn(2).unwrap(), Interrupted);
    assert_eq!(
        (turns.get(Sub(2)), turns.working_subs()),
        (Interrupted, vec![])
    );
    assert_eq!(stored_turns(&store, &id), ["idle", "idle", "working"]);
    // A turn that starts there and gets its answer is stored idle by the
    // log that finds it, as the main agent's is.
    turns.set(Sub(2), Working).unwrap();
    let asked = log.latest_leaf(ThreadFilter::sub(2));
    let answer = LogEntryKind::Message {
        message: AgentMessage::wire(Message::Assistant(AssistantMessage::empty())),
    };
    log.append(asked, ThreadKind::Sub, Some(2), answer).unwrap();
    drop((log, turns));
    let mut log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(log.pick_up_turn(2).unwrap(), Idle);
    assert_eq!(stored_turns(&store, &id), ["idle", "idle", "idle"]);
    assert!(log.sub_agents_with_work().unwrap().is_empty());
    log.turn_state().set(Sub(2), Working).unwrap();
    drop(log);

    // A log that has not read the run's turn does not know it, so ending
    // the turn is written and not taken for the idle it has in memory.
    let log = SessionLog::resume(&store, &id).unwrap();
    assert_eq!(stored_turns(&store, &id), ["idle", "idle", "working"]);
    log.turn_state().set(Sub(2), Idle).unwrap();
    assert_eq!(stored_turns(&store, &id), ["idle", "idle", "idle"]);
    drop(log);

    // An import starts no run.
    let copy = project(root.path(), "copy");
    let file = store.export_jsonl(&id, &root.path().join("exported"));
    assert_eq!(copy.import_jsonl(&file.unwrap()).unwrap(), id);
    assert_eq!(stored_turns(&copy, &id), ["idle", "idle", "idle"]);
}

/// What each of the session's sub-agent threads holds, in the order
/// they started: its stored turn, the texts of its pending inputs, the
/// call ids of its task records, and how many records it has.
fn sub_agent_rows(
    store: &ProjectSessions,
    id: &str,
) -> Vec<(String, Vec<String>, Vec<String>, usize)> {
    let raw = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let sql = "SELECT t.id, t.turn FROM threads t JOIN sessions s ON s.id = t.session
               WHERE s.project = ?1 AND s.name = ?2 AND t.kind = 'sub' ORDER BY t.id";
    let mut runs = raw.prepare(sql).unwrap();
    let runs = runs.query_map([store.project_id(), id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    });
    let runs: Vec<_> = runs.unwrap().collect::<Result<_, _>>().unwrap();
    let texts = |sql: &str, run: i64| -> Vec<String> {
        let mut rows = raw.prepare(sql).unwrap();
        let rows = rows.query_map([run], |row| row.get(0)).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    let rows = runs.into_iter().map(|(run, turn)| {
        let pending = "SELECT body FROM pending WHERE thread = ?1 ORDER BY rowid";
        let tasks = "SELECT call_id FROM tasks WHERE thread = ?1 ORDER BY rowid";
        let entries = "SELECT entry_id FROM entries WHERE thread = ?1";
        let count = texts(entries, run).len();
        (turn, texts(pending, run), texts(tasks, run), count)
    });
    rows.collect()
}

/// Two branches can each have a sub-agent 1, as in an imported session.
/// Everything written for sub-agent 1 goes to its run on the selected
/// branch: its entries, its turn state, its pending input and its
/// task record. After a switch it all goes to the other branch's run.
#[test]
fn a_sub_agents_writes_go_to_its_run_on_the_selected_branch() {
    use aj_agent::events::AgentId::Sub;
    use aj_agent::events::SubAgentConclusion;
    use aj_session::{TaskRecord, TurnState};

    let root = tempfile::tempdir().unwrap();
    let store = project(root.path(), "p");
    let settings = aj_agent::events::AgentSettings {
        context_window: 0,
        provider: "p".into(),
        model_id: "m".into(),
        thinking: "off".into(),
        thinking_display: String::new(),
        speed: "default".into(),
        verbosity: "default".into(),
    };
    let mut log = SessionLog::create(&store).unwrap();
    let id = log.session_id().to_owned();
    let fork = append(&mut log, "prompt");
    let first = append(&mut log, "the first branch");
    log.append_subagent_spawn(1, first.clone(), "task", "agent", None, true, &settings)
        .unwrap();
    switch(&mut log, &fork);
    let second = append(&mut log, "the second branch");
    log.append_subagent_spawn(1, second.clone(), "task", "agent", None, true, &settings)
        .unwrap();
    // The spawn records left both runs working. Idle is the state a
    // switch finds.
    let turns = log.turn_state();
    turns.set(Sub(1), TurnState::Idle).unwrap();
    switch(&mut log, &first);
    log.pick_up_turn(1).unwrap();
    turns.set(Sub(1), TurnState::Idle).unwrap();
    let nothing = ("idle".to_string(), vec![], vec![], 1);
    assert_eq!(
        sub_agent_rows(&store, &id),
        [nothing.clone(), nothing.clone()]
    );

    // Each round writes one of everything for sub-agent 1 and removes the
    // pending input and the command again, as a switch needs.
    let notice = AgentMessage::task_notification(aj_agent::message::TaskNotification::new(
        "make".into(),
        aj_agent::message::TaskNotificationKind::Bash,
        aj_agent::message::TaskOutcome::Succeeded,
        "exit 0".into(),
    ));
    let round = |log: &mut SessionLog, branch: &String, task_id: usize, name: &str| {
        switch(log, branch);
        let (pending, tasks) = (log.pending(), log.task_records());
        let (kind, input) = an_input(name);
        pending.save_input(Sub(1), kind, &input).unwrap();
        let task = TaskRecord {
            id: task_id,
            owner: Sub(1),
            call_id: name.into(),
            command: "make".into(),
            pid: 4242,
            process_start: 99,
            output_path: "/tmp/out.log".into(),
            exit_path: "/tmp/out.exit".into(),
            started_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        };
        tasks.start(&task).unwrap();
        turns.set(Sub(1), TurnState::Working).unwrap();
        // A message, a state entry and the run's end, which find the
        // run's last entry in three ways.
        let answer = ToolResultMessage::text(name, "read", "output", false);
        aj_session::answer_tool_call(log, Sub(1), answer).unwrap();
        log.append_thinking_change(ThreadFilter::sub(1), "low")
            .unwrap();
        let end = log.append_subagent_end(1, SubAgentConclusion::Completed, "done");
        assert!(end.unwrap().is_some());
        let rows = sub_agent_rows(&store, &id);
        pending.forget(&input.id).unwrap();
        tasks.finish(task_id, Sub(1), &notice).unwrap();
        pending.forget(notice.id()).unwrap();
        turns.set(Sub(1), TurnState::Idle).unwrap();
        rows
    };
    let wrote = |name: &str, records| {
        (
            "working".to_string(),
            vec![name.to_string()],
            vec![name.to_string()],
            records,
        )
    };
    assert_eq!(
        round(&mut log, &second, 1, "second"),
        [nothing.clone(), wrote("second", 4)]
    );
    assert_eq!(
        round(&mut log, &first, 2, "first"),
        [wrote("first", 4), ("idle".to_string(), vec![], vec![], 4)]
    );
    // What the log reads back for sub-agent 1 is the selected branch's too.
    let run = log.agent_branch(Sub(1)).unwrap().expect("a run");
    assert_eq!(run.entries().len(), 4);
    assert_eq!(
        sub_agent_rows(&store, &id)
            .iter()
            .map(|row| row.3)
            .sum::<usize>(),
        8
    );
}
