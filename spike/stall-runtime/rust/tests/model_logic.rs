//! Pure model logic: due computation, event identity and dedupe, timestamp
//! formatting, queue capping and the monitor record's field names.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;
use stall_b::model::{
    self, Monitor, QueuedMonitorEvent, Source, SourceEvent, Target, META_LAST_ATTEMPT,
};

fn target() -> Target {
    Target {
        kind: "program".to_string(),
        id: "spike".to_string(),
    }
}

fn monitor(id: &str) -> Monitor {
    Monitor {
        id: id.to_string(),
        name: format!("monitor {id}"),
        target: target(),
        source: Source::GithubIssues {
            repo: "example-org/repo-1".to_string(),
            labels: vec!["atelier:build".to_string()],
            include_existing: false,
        },
        poll_interval_sec: 15,
        cursors: BTreeMap::new(),
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        updated_at: "2026-01-01T00:00:00.000Z".to_string(),
    }
}

fn event(id: Option<&str>, summary: &str) -> SourceEvent {
    SourceEvent {
        kind: "issue".to_string(),
        id: id.map(str::to_string),
        at: "2026-01-01T00:00:00.000Z".to_string(),
        summary: summary.to_string(),
        body: None,
        actionable: false,
        terminal: None,
        subject: None,
    }
}

#[test]
fn due_when_never_attempted() {
    assert!(model::is_due(1_000, None, 15));
}

#[test]
fn due_only_after_the_interval() {
    let now = 1_000_000;
    assert!(!model::is_due(now, Some(now - 14_999), 15));
    assert!(model::is_due(now, Some(now - 15_000), 15));
    assert!(model::is_due(now, Some(now - 60_000), 15));
}

#[test]
fn attempt_timestamp_reads_only_numbers() {
    let mut m = monitor("m1");
    assert_eq!(model::last_attempt_ms(&m), None);
    m.cursors.insert(META_LAST_ATTEMPT.to_string(), json!(1234));
    assert_eq!(model::last_attempt_ms(&m), Some(1234));
    m.cursors
        .insert(META_LAST_ATTEMPT.to_string(), json!("1234"));
    assert_eq!(model::last_attempt_ms(&m), None);
}

#[test]
fn monitor_record_keeps_sourcefed_field_names() {
    let m = monitor("m1");
    let value = serde_json::to_value(&m).expect("serialize");
    assert_eq!(value["sourceType"], "github-issues");
    assert_eq!(value["repo"], "example-org/repo-1");
    assert_eq!(value["labels"][0], "atelier:build");
    assert_eq!(value["includeExisting"], false);
    assert_eq!(value["pollIntervalSec"], 15);
    assert!(value.get("createdAt").is_some());
    assert!(value.get("updatedAt").is_some());
    assert!(value.get("cursors").is_some());

    let back: Monitor = serde_json::from_value(value).expect("deserialize");
    assert_eq!(back, m);
}

#[test]
fn unknown_source_type_fails_loudly() {
    let value = json!({
        "id": "m1",
        "name": "m",
        "target": {"kind": "program", "id": "spike"},
        "sourceType": "rss",
        "pollIntervalSec": 15,
        "cursors": {},
        "createdAt": "x",
        "updatedAt": "x",
    });
    let error = serde_json::from_value::<Monitor>(value).expect_err("must reject rss");
    assert!(
        error.to_string().contains("unknown variant"),
        "unexpected error: {error}"
    );
}

#[test]
fn event_identity_prefers_the_plugin_id() {
    assert_eq!(event(Some("issue-7"), "x").event_id(), "issue-7");
    assert_eq!(
        event(None, "x").event_id(),
        "issue:2026-01-01T00:00:00.000Z:x"
    );
    assert_eq!(
        event(Some(""), "x").event_id(),
        "issue:2026-01-01T00:00:00.000Z:x"
    );
}

#[test]
fn dedupe_skips_queued_and_repeated_events() {
    let existing: BTreeSet<String> = ["m1:issue-1".to_string()].into_iter().collect();
    let batch = vec![
        event(Some("issue-1"), "already queued"),
        event(Some("issue-2"), "new"),
        event(Some("issue-2"), "repeat inside the batch"),
        event(None, "no id"),
    ];
    let indices = model::new_event_indices(&existing, "m1", &batch);
    assert_eq!(indices, vec![1, 3]);
}

#[test]
fn queued_event_shape_matches_the_contract() {
    let m = monitor("m1");
    let queued = model::queued_event(
        &m.id,
        &m.source,
        &m.target,
        &event(Some("issue-7"), "hello"),
        "2026-01-01T00:00:01.000Z",
    );
    let value = serde_json::to_value(&queued).expect("serialize");
    assert_eq!(value["id"], "m1:issue-7");
    assert_eq!(value["monitorID"], "m1");
    assert_eq!(value["target"]["kind"], "program");
    assert_eq!(value["queuedAt"], "2026-01-01T00:00:01.000Z");
    assert_eq!(value["event"]["source"]["type"], "github-issues");
    assert_eq!(value["event"]["source"]["includeExisting"], false);
    assert_eq!(value["event"]["kind"], "issue");
    assert_eq!(value["event"]["actionable"], false);
    assert_eq!(value["event"]["terminal"], false);
    assert!(value["event"].get("body").is_none());
    assert!(value["event"].get("subject").is_none());
}

#[test]
fn queue_cap_evicts_the_oldest_for_that_target_only() {
    let mut events = Vec::new();
    for index in 0..5 {
        let mut queued = model::queued_event(
            "m1",
            &monitor("m1").source,
            &target(),
            &event(Some(&format!("a{index}")), "a"),
            "t",
        );
        queued.id = format!("m1:a{index}");
        events.push(queued);
    }
    let other = Target {
        kind: "program".to_string(),
        id: "other".to_string(),
    };
    let mut foreign = model::queued_event(
        "m2",
        &monitor("m2").source,
        &other,
        &event(Some("b0"), "b"),
        "t",
    );
    foreign.id = "m2:b0".to_string();
    events.push(foreign);

    let dropped = model::evict_over_cap(&mut events, &target(), 3);
    assert_eq!(dropped, 2);
    let ids: Vec<&str> = events.iter().map(|queued| queued.id.as_str()).collect();
    assert_eq!(ids, vec!["m1:a2", "m1:a3", "m1:a4", "m2:b0"]);
}

#[test]
fn iso8601_formats_utc_with_millis() {
    assert_eq!(model::iso8601_ms(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(model::iso8601_ms(1), "1970-01-01T00:00:00.001Z");
    assert_eq!(
        model::iso8601_ms(1_700_000_000_000),
        "2023-11-14T22:13:20.000Z"
    );
    // Leap day.
    assert_eq!(
        model::iso8601_ms(1_709_164_800_123),
        "2024-02-29T00:00:00.123Z"
    );
    // Before the epoch.
    assert_eq!(model::iso8601_ms(-1), "1969-12-31T23:59:59.999Z");
}

#[test]
fn queued_events_deserialize_from_their_own_json() {
    let queued: QueuedMonitorEvent = serde_json::from_value(json!({
        "id": "m1:e1",
        "monitorID": "m1",
        "target": {"kind": "program", "id": "spike"},
        "queuedAt": "t",
        "event": {
            "source": {"type": "jira-query", "jql": "project = PROJ1 ORDER BY key ASC"},
            "kind": "issue",
            "at": "t",
            "summary": "s",
            "actionable": true,
            "terminal": false,
            "subject": {"key": null}
        }
    }))
    .expect("deserialize");
    assert_eq!(
        queued.event.source["jql"],
        "project = PROJ1 ORDER BY key ASC"
    );
    assert!(queued.event.actionable);
}
