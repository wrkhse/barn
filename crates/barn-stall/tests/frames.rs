//! The shared frames are the contract tack and a plugin stall build against.
//! Every line in `fixtures/frames.jsonl` is read here and round-tripped through
//! its type, so a changed field or tag fails these tests.

use barn_stall::{Content, FRAMED_VERSION, Meta, Outcome, Reply, Request};
use serde_json::Value;

const FRAMES: &str = include_str!("../fixtures/frames.jsonl");
const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn wires() -> Option<Vec<Value>> {
    FRAMES
        .lines()
        .map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn requests() -> Option<Vec<Request>> {
    wires()?
        .into_iter()
        .step_by(2)
        .map(|wire| serde_json::from_value(wire).ok())
        .collect()
}

fn replies() -> Option<Vec<Reply>> {
    wires()?
        .into_iter()
        .skip(1)
        .step_by(2)
        .map(|wire| serde_json::from_value(wire).ok())
        .collect()
}

/// Every fixture line is a request followed by its reply.
#[test]
fn every_fixture_line_round_trips() {
    let wires = wires().expect("fixture frames");
    assert_eq!(wires.len(), 14);

    for (index, wire) in wires.iter().enumerate() {
        if index % 2 == 0 {
            let req: Request = serde_json::from_value(wire.clone()).expect("req");
            let back = serde_json::to_value(req).expect("serialise");
            assert_eq!(&back, wire);
        } else {
            let rep: Reply = serde_json::from_value(wire.clone()).expect("rep");
            let back = serde_json::to_value(rep).expect("serialise");
            assert_eq!(&back, wire);
        }
    }
}

/// A hook answer names its decision, and repeats the request's id.
#[test]
fn the_denied_call_answers_with_a_deny_reason() {
    let requests = requests().expect("request frames");
    let replies = replies().expect("reply frames");

    let Request::Hook { id, hook, call, .. } = &requests[0] else {
        panic!("the denied frame is a hook request");
    };
    let Reply::Hook {
        id: reply_id,
        outcome,
    } = &replies[0]
    else {
        panic!("the denial is a hook reply");
    };
    let Outcome::Deny { reason } = outcome else {
        panic!("the denial carries a reason");
    };

    assert_eq!(hook, "PreToolUse");
    assert_eq!(call.tool, "bash");
    assert_eq!(call.input, serde_json::json!({"command": "git push"}));
    assert_eq!(reply_id, id);
    assert_eq!(reason.as_str(), "push blocked");
}

/// The rewrite request carries trace context and the framed version.
#[test]
fn the_rewrite_request_carries_trace_context() {
    let requests = requests().expect("request frames");
    let Request::Hook {
        id,
        version,
        _meta: meta,
        ..
    } = &requests[1]
    else {
        panic!("the rewrite frame is a hook request");
    };

    assert_eq!(*id, 2);
    assert_eq!(*version, FRAMED_VERSION);

    let meta = meta.as_ref().expect("the rewrite request carries meta");
    assert_eq!(
        *meta,
        Meta {
            traceparent: Some(TRACEPARENT.to_owned()),
            tracestate: Some("congo=t61rcWkgMzE".to_owned()),
        }
    );
}

#[test]
fn the_rewrite_pair_carries_the_rewritten_command() {
    let replies = replies().expect("reply frames");
    let Reply::Hook { id, outcome } = &replies[1] else {
        panic!("the rewrite is a hook reply");
    };
    let Outcome::Rewrite { command } = outcome else {
        panic!("the rewrite carries a command");
    };

    assert_eq!(*id, 2);
    assert_eq!(command.as_str(), "git status --short");
}

#[test]
fn the_allowed_call_answers_with_allow() {
    let requests = requests().expect("request frames");
    let replies = replies().expect("reply frames");

    let Request::Hook { id, hook, .. } = &requests[2] else {
        panic!("the allowed frame is a hook request");
    };
    let Reply::Hook {
        id: reply_id,
        outcome,
    } = &replies[2]
    else {
        panic!("the allowance is a hook reply");
    };

    assert_eq!(hook, "PreToolUse");
    assert_eq!(*id, 3);
    assert_eq!(reply_id, id);
    assert_eq!(outcome, &Outcome::Allow);
}

#[test]
fn the_annotated_call_carries_the_note() {
    let requests = requests().expect("request frames");
    let replies = replies().expect("reply frames");

    let Request::Hook { id, hook, .. } = &requests[3] else {
        panic!("the annotated frame is a hook request");
    };
    let Reply::Hook {
        id: reply_id,
        outcome,
    } = &replies[3]
    else {
        panic!("the annotation is a hook reply");
    };
    let Outcome::Annotate { text } = outcome else {
        panic!("the annotation carries text");
    };

    assert_eq!(hook, "PostToolUse");
    assert_eq!(*id, 4);
    assert_eq!(reply_id, id);
    assert_eq!(text.as_str(), "review before merge");
}

#[test]
fn the_dropped_call_answers_with_drop() {
    let requests = requests().expect("request frames");
    let replies = replies().expect("reply frames");

    let Request::Hook { id, hook, .. } = &requests[4] else {
        panic!("the dropped frame is a hook request");
    };
    let Reply::Hook {
        id: reply_id,
        outcome,
    } = &replies[4]
    else {
        panic!("the drop is a hook reply");
    };

    assert_eq!(hook, "PreToolUse");
    assert_eq!(*id, 5);
    assert_eq!(reply_id, id);
    assert_eq!(outcome, &Outcome::Drop);
}

/// A tool request answers with content, not a verdict.
#[test]
fn the_tool_pair_carries_content() {
    let requests = requests().expect("request frames");
    let replies = replies().expect("reply frames");

    let Request::Tool { id, call, .. } = &requests[5] else {
        panic!("the tool frame is a tool request");
    };
    let Reply::Tool {
        id: reply_id,
        content,
    } = &replies[5]
    else {
        panic!("the tool answer is a tool reply");
    };

    assert_eq!(call.tool, "notes");
    assert_eq!(call.name, "read");
    assert_eq!(*id, 6);
    assert_eq!(reply_id, id);
    assert_eq!(
        *content,
        Content {
            kind: "text".to_owned(),
            text: "buy milk".to_owned(),
        }
    );
}

#[test]
fn the_error_pair_names_the_failure() {
    let replies = replies().expect("reply frames");
    let Reply::Error { id, error, message } = &replies[6] else {
        panic!("the failure is an error reply");
    };

    assert_eq!(*id, 7);
    assert_eq!(error, "stall_unavailable");
    assert_eq!(message, "the plugin stall did not answer");
}

/// Absence of trace context and of a session id is valid.
#[test]
fn a_hook_without_meta_or_session_is_valid() {
    let requests = requests().expect("request frames");
    let Request::Hook {
        call, _meta: meta, ..
    } = &requests[0]
    else {
        panic!("the denied frame is a hook request");
    };

    assert_eq!(meta, &None);
    assert_eq!(call.session_id, None);
}

/// A session id travels only when the caller supplied one.
#[test]
fn a_session_id_is_carried_only_when_supplied() {
    let requests = requests().expect("request frames");
    let Request::Hook { call, .. } = &requests[1] else {
        panic!("the rewrite frame is a hook request");
    };
    assert_eq!(call.session_id.as_deref(), Some("sess-42"));

    let Request::Tool { call, .. } = &requests[5] else {
        panic!("the tool frame is a tool request");
    };
    assert_eq!(call.session_id, None);
}

#[test]
fn every_request_carries_the_framed_version() {
    for request in requests().expect("request frames") {
        match request {
            Request::Hook { version, .. } | Request::Tool { version, .. } => {
                assert_eq!(version, FRAMED_VERSION);
            }
        }
    }
}
