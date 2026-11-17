//! The daemon RPC surface, driven through `rpc::handle` with an in-memory store
//! and no HTTP listener: status codes, envelope shapes and stored monitors.

use std::sync::Arc;

use hyper::http::StatusCode;
use serde_json::{json, Value};
use stall_b::http::HttpState;
use stall_b::model::{self, SourceEvent, Target};
use stall_b::rpc;
use stall_b::sse::Hub;
use stall_b::store;

fn state() -> Arc<HttpState> {
    Arc::new(HttpState {
        store: store::open_memory(),
        hub: Arc::new(Hub::new()),
    })
}

async fn call(state: &Arc<HttpState>, body: Value) -> (StatusCode, Value) {
    let bytes = serde_json::to_vec(&body).expect("encode request");
    rpc::handle(state, &bytes).await
}

#[tokio::test]
async fn daemon_health_matches_the_contract() {
    let state = state();
    let (status, document) = call(&state, json!({"id": 1, "method": "daemon.health"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["id"], 1);
    assert_eq!(document["result"]["ok"], true);
    assert_eq!(document["result"]["protocol"], 1);
    assert_eq!(document["result"]["version"], model::VERSION);
}

#[tokio::test]
async fn creates_a_github_issues_monitor() {
    let state = state();
    let (status, document) = call(
        &state,
        json!({
            "id": 2,
            "method": "monitor.create",
            "params": {
                "target": {"kind": "program", "id": "spike"},
                "name": "gh",
                "sourceType": "github-issues",
                "repo": "example-org/repo-1",
                "labels": ["atelier:build"],
                "includeExisting": false,
                "pollIntervalSec": 15
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "document: {document}");
    assert_eq!(document["id"], 2);
    assert_eq!(document["result"]["ok"], true);
    assert_eq!(document["result"]["created"], true);
    let id = document["result"]["monitor"]["id"]
        .as_str()
        .expect("monitor id");
    assert_eq!(document["result"]["monitor"]["name"], "gh");

    let stored = state.store.get_monitor(id).expect("get").expect("found");
    assert_eq!(stored.source_type(), model::SourceType::GithubIssues);
    assert_eq!(stored.poll_interval_sec, 15);
    assert!(
        stored.cursors.is_empty(),
        "a new monitor must be due at once"
    );
    assert_eq!(stored.target.kind, "program");
}

#[tokio::test]
async fn creates_a_jira_query_monitor_with_the_default_interval() {
    let state = state();
    let (status, document) = call(
        &state,
        json!({
            "id": 3,
            "method": "monitor.create",
            "params": {
                "target": {"kind": "program", "id": "spike"},
                "name": "jira",
                "sourceType": "jira-query",
                "jql": "project = PROJ1 ORDER BY key ASC"
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "document: {document}");
    let id = document["result"]["monitor"]["id"].as_str().expect("id");
    let stored = state.store.get_monitor(id).expect("get").expect("found");
    assert_eq!(stored.source_type(), model::SourceType::JiraQuery);
    assert_eq!(stored.poll_interval_sec, 15);
    assert_eq!(
        stored.source.source_json()["jql"],
        "project = PROJ1 ORDER BY key ASC"
    );
}

#[tokio::test]
async fn rejects_an_unknown_source_type() {
    let state = state();
    let (status, document) = call(
        &state,
        json!({
            "id": 4,
            "method": "monitor.create",
            "params": {
                "target": {"kind": "program", "id": "spike"},
                "name": "rss",
                "sourceType": "rss",
                "url": "https://example.invalid/feed"
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(document["id"], 4);
    assert_eq!(document["error"]["code"], -32602);
    let message = document["error"]["message"].as_str().expect("message");
    assert!(message.contains("unknown variant"), "message: {message}");
    assert_eq!(state.store.monitor_count().expect("count"), 0);
}

#[tokio::test]
async fn rejects_a_zero_poll_interval() {
    let state = state();
    let (status, _document) = call(
        &state,
        json!({
            "id": 5,
            "method": "monitor.create",
            "params": {
                "target": {"kind": "program", "id": "spike"},
                "name": "bad",
                "sourceType": "github-issues",
                "repo": "example-org/repo-1",
                "labels": [],
                "includeExisting": false,
                "pollIntervalSec": 0
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn malformed_json_is_a_400_with_a_null_id() {
    let state = state();
    let (status, document) = rpc::handle(&state, b"{not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(document["id"].is_null());
    assert_eq!(document["error"]["code"], -32700);
}

#[tokio::test]
async fn unknown_methods_get_method_not_found() {
    let state = state();
    let (status, document) = call(&state, json!({"id": 6, "method": "monitor.delete"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(document["error"]["code"], -32601);
}

#[tokio::test]
async fn ack_removes_only_the_named_events_of_the_target() {
    let state = state();
    let target = Target {
        kind: "program".to_string(),
        id: "spike".to_string(),
    };
    let source = model::Source::JiraQuery {
        jql: "project = PROJ1 ORDER BY key ASC".to_string(),
    };
    let event = SourceEvent {
        kind: "issue".to_string(),
        id: Some("e1".to_string()),
        at: "t".to_string(),
        summary: "s".to_string(),
        body: None,
        actionable: false,
        terminal: None,
        subject: None,
    };
    let queued = model::queued_event("m1", &source, &target, &event, "q");
    state
        .store
        .append_events(std::slice::from_ref(&queued))
        .expect("seed events");

    let (status, document) = call(
        &state,
        json!({
            "id": 7,
            "method": "monitor.ack",
            "params": {"target": {"kind": "program", "id": "spike"}, "eventIDs": ["m1:e1"]}
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(document["result"]["ok"], true);
    assert!(state.store.events().expect("events").is_empty());
}
