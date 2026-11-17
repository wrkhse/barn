//! Exact SSE frame bytes: the harness parses these, so the shapes are pinned.

use stall_b::model::{self, Source, SourceEvent, Target};
use stall_b::sse;

fn target() -> Target {
    Target {
        kind: "program".to_string(),
        id: "spike".to_string(),
    }
}

fn text(frame: bytes::Bytes) -> String {
    String::from_utf8(frame.to_vec()).expect("utf-8 frame")
}

#[test]
fn subscribed_frame_is_exact() {
    assert_eq!(
        text(sse::subscribed_frame(&target())),
        "data: {\"type\":\"subscribed\",\"target\":{\"kind\":\"program\",\"id\":\"spike\"}}\n\n"
    );
}

#[test]
fn event_frame_carries_one_batch() {
    let source = Source::GithubIssues {
        repo: "example-org/repo-1".to_string(),
        labels: vec!["atelier:build".to_string()],
        include_existing: false,
    };
    let event = SourceEvent {
        kind: "issue".to_string(),
        id: Some("issue-7".to_string()),
        at: "2026-01-01T00:00:00.000Z".to_string(),
        summary: "hello".to_string(),
        body: None,
        actionable: false,
        terminal: None,
        subject: None,
    };
    let queued = model::queued_event("m1", &source, &target(), &event, "2026-01-01T00:00:01.000Z");
    let frame = text(sse::event_frame(&target(), &[queued]));

    assert!(
        frame.starts_with(
            "data: {\"type\":\"event\",\"target\":{\"kind\":\"program\",\"id\":\"spike\"},\"events\":[{"
        ),
        "frame: {frame}"
    );
    assert!(frame.contains("\"id\":\"m1:issue-7\""), "frame: {frame}");
    assert!(frame.ends_with("]}\n\n"), "frame: {frame}");
}

#[test]
fn keepalive_frame_is_a_comment() {
    assert_eq!(text(sse::keepalive_frame()), ": keepalive\n\n");
}
