//! Store semantics in a private temp directory: atomic pretty files with mode
//! 0600, no in-memory cache, cursor updates, event queuing, ack and the cap.
//! These tests only touch files under the system temp directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use stall_b::model::{self, Monitor, QueuedMonitorEvent, Source, SourceEvent, Target};
use stall_b::store;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "stall-b-test-{}-{tag}-{}",
            std::process::id(),
            model::now_ms()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn target() -> Target {
    Target {
        kind: "program".to_string(),
        id: "spike".to_string(),
    }
}

fn source() -> Source {
    Source::JiraQuery {
        jql: "project = PROJ1 ORDER BY key ASC".to_string(),
    }
}

fn monitor(id: &str) -> Monitor {
    Monitor {
        id: id.to_string(),
        name: format!("monitor {id}"),
        target: target(),
        source: source(),
        poll_interval_sec: 15,
        cursors: BTreeMap::new(),
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        updated_at: "2026-01-01T00:00:00.000Z".to_string(),
    }
}

fn queued(monitor_id: &str, event_id: &str) -> QueuedMonitorEvent {
    let event = SourceEvent {
        kind: "issue".to_string(),
        id: Some(event_id.to_string()),
        at: "2026-01-01T00:00:00.000Z".to_string(),
        summary: format!("summary {event_id}"),
        body: None,
        actionable: false,
        terminal: None,
        subject: None,
    };
    model::queued_event(
        monitor_id,
        &source(),
        &target(),
        &event,
        "2026-01-01T00:00:01.000Z",
    )
}

#[test]
fn monitors_round_trip_through_the_json_file() {
    let dir = TempDir::new("monitors");
    let store = store::open_json(dir.path()).expect("open store");
    store.create_monitor(&monitor("m1")).expect("create");
    store.create_monitor(&monitor("m2")).expect("create");

    let listed = store.list_monitors().expect("list");
    assert_eq!(listed.len(), 2);
    assert_eq!(
        store.get_monitor("m2").expect("get").expect("found"),
        monitor("m2")
    );
    assert!(store.get_monitor("nope").expect("get").is_none());
    assert_eq!(store.monitor_count().expect("count"), 2);

    // Pretty JSON (2 spaces) and no temp files left behind.
    let text = fs::read_to_string(dir.path().join("monitors.json")).expect("read file");
    assert!(text.starts_with("[\n  {"), "not pretty printed: {text:.40}");
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.path().join("monitors.json"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "monitors.json must be 0600");
    }
}

#[test]
fn duplicate_monitor_ids_are_rejected() {
    let dir = TempDir::new("duplicate");
    let store = store::open_json(dir.path()).expect("open store");
    store.create_monitor(&monitor("m1")).expect("create");
    let error = store
        .create_monitor(&monitor("m1"))
        .expect_err("duplicate must fail");
    assert!(
        error.to_string().contains("duplicate"),
        "unexpected: {error}"
    );
}

#[test]
fn every_read_hits_the_file() {
    let dir = TempDir::new("cache");
    let store = store::open_json(dir.path()).expect("open store");
    store.create_monitor(&monitor("m1")).expect("create");

    // Change the file behind the store's back: a cached store would miss this.
    let path = dir.path().join("monitors.json");
    let text = fs::read_to_string(&path).expect("read");
    fs::write(&path, text.replace("monitor m1", "renamed by hand")).expect("write");

    let fetched = store.get_monitor("m1").expect("get").expect("found");
    assert_eq!(fetched.name, "renamed by hand");
}

#[test]
fn a_second_handle_sees_persisted_monitors() {
    let dir = TempDir::new("reopen");
    {
        let store = store::open_json(dir.path()).expect("open store");
        store.create_monitor(&monitor("m1")).expect("create");
    }
    let reopened = store::open_json(dir.path()).expect("reopen");
    assert_eq!(reopened.list_monitors().expect("list").len(), 1);
}

#[test]
fn cursor_writes_persist_and_bump_updated_at() {
    let dir = TempDir::new("cursors");
    let store = store::open_json(dir.path()).expect("open store");
    store.create_monitor(&monitor("m1")).expect("create");

    store
        .set_cursors(
            "m1",
            &[
                ("jira-query".to_string(), serde_json::json!({"offset": 42})),
                (model::META_LAST_ATTEMPT.to_string(), serde_json::json!(777)),
            ],
        )
        .expect("set cursors");

    let fetched = store.get_monitor("m1").expect("get").expect("found");
    assert_eq!(fetched.cursors["jira-query"]["offset"], 42);
    assert_eq!(fetched.cursors[model::META_LAST_ATTEMPT], 777);
    assert_ne!(fetched.updated_at, "2026-01-01T00:00:00.000Z");

    let error = store
        .set_cursors("missing", &[("k".to_string(), serde_json::json!(1))])
        .expect_err("unknown monitor must fail");
    assert!(error.to_string().contains("not found"));
}

#[test]
fn events_append_ack_and_survive_reopen() {
    let dir = TempDir::new("events");
    let store = store::open_json(dir.path()).expect("open store");
    let batch = vec![queued("m1", "e1"), queued("m1", "e2"), queued("m2", "e1")];
    let outcome = store.append_events(&batch).expect("append");
    assert_eq!(outcome.appended, 3);
    assert_eq!(outcome.evicted, 0);

    let reopened = store::open_json(dir.path()).expect("reopen");
    assert_eq!(reopened.events().expect("events").len(), 3);

    let acked = reopened
        .ack(&target(), &["m1:e1".to_string()])
        .expect("ack");
    assert_eq!(acked, 1);
    let ids: Vec<String> = reopened
        .events()
        .expect("events")
        .into_iter()
        .map(|event| event.id)
        .collect();
    assert_eq!(ids, vec!["m1:e2", "m2:e1"]);

    assert_eq!(
        reopened
            .ack(&target(), &["m1:e2".to_string()])
            .expect("ack"),
        1
    );
    assert_eq!(
        reopened
            .ack(&target(), &["m1:e2".to_string()])
            .expect("ack"),
        0
    );
}

#[test]
fn the_queue_cap_drops_the_oldest_events_of_that_target() {
    let dir = TempDir::new("cap");
    let store = store::open_json(dir.path()).expect("open store");
    let batch: Vec<QueuedMonitorEvent> = (0..model::QUEUE_CAP_PER_TARGET + 5)
        .map(|index| queued("m1", &format!("e{index:04}")))
        .collect();
    let outcome = store.append_events(&batch).expect("append");
    assert_eq!(outcome.appended, model::QUEUE_CAP_PER_TARGET + 5);
    assert_eq!(outcome.evicted, 5);

    let events = store.events().expect("events");
    assert_eq!(events.len(), model::QUEUE_CAP_PER_TARGET);
    assert_eq!(events[0].id, "m1:e0005");
    assert_eq!(
        events.last().expect("last").id,
        format!("m1:e{:04}", model::QUEUE_CAP_PER_TARGET + 4)
    );
}

#[test]
fn the_memory_store_matches_the_json_store_and_writes_nothing() {
    let dir = TempDir::new("memory");
    let store = store::open_memory();
    store.create_monitor(&monitor("m1")).expect("create");
    store
        .append_events(&[queued("m1", "e1"), queued("m1", "e2")])
        .expect("append");
    store
        .set_cursors(
            "m1",
            &[(model::META_LAST_ATTEMPT.to_string(), serde_json::json!(5))],
        )
        .expect("cursor");

    assert_eq!(store.monitor_count().expect("count"), 1);
    assert_eq!(store.events().expect("events").len(), 2);
    assert_eq!(
        store.ack(&target(), &["m1:e1".to_string()]).expect("ack"),
        1
    );

    let written: Vec<_> = fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(Result::ok)
        .collect();
    assert!(written.is_empty(), "memory store wrote files: {written:?}");
}

#[test]
fn a_fresh_state_directory_reads_as_empty() {
    let dir = TempDir::new("empty");
    let store = store::open_json(dir.path()).expect("open store");
    assert!(store.list_monitors().expect("list").is_empty());
    assert!(store.events().expect("events").is_empty());
    assert_eq!(store.monitor_count().expect("count"), 0);
}
