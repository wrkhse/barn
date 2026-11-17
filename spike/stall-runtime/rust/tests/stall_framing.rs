//! Stall protocol line framing and JSON-RPC classification. No sockets: the splitter
//! is a pure byte-level state machine and classification works on `&str`.

use stall_b::stall::{parse_inbound, Inbound, Line, LineSplitter};

fn split(chunks: &[&[u8]], max: usize) -> Vec<(Line, String)> {
    let mut splitter = LineSplitter::new(max);
    let mut out = Vec::new();
    for chunk in chunks {
        splitter.push(chunk, &mut out);
    }
    out
}

#[test]
fn splits_several_lines_from_one_chunk() {
    let lines = split(&[b"{\"id\":1}\n{\"id\":2}\n"], 1024);
    assert_eq!(
        lines,
        vec![
            (Line::Text, "{\"id\":1}".to_string()),
            (Line::Text, "{\"id\":2}".to_string()),
        ]
    );
}

#[test]
fn joins_a_line_split_across_chunks() {
    let lines = split(&[b"{\"id\"", b":3", b"}\n"], 1024);
    assert_eq!(lines, vec![(Line::Text, "{\"id\":3}".to_string())]);
}

#[test]
fn strips_carriage_returns() {
    let lines = split(&[b"{\"id\":4}\r\n"], 1024);
    assert_eq!(lines, vec![(Line::Text, "{\"id\":4}".to_string())]);
}

#[test]
fn ignores_blank_lines() {
    let lines = split(&[b"\n\n"], 1024);
    assert!(lines.is_empty());
}

#[test]
fn overlong_line_is_dropped_and_framing_survives() {
    // max 4: "12345" is over the cap, the next line must still parse.
    let lines = split(&[b"12345\nok\n"], 4);
    assert_eq!(
        lines,
        vec![
            (Line::Overlong, String::new()),
            (Line::Text, "ok".to_string())
        ]
    );
}

#[test]
fn overlong_line_spanning_chunks_is_dropped_once() {
    let lines = split(&[b"12345", b"67890", b"\nnext\n"], 4);
    assert_eq!(
        lines,
        vec![
            (Line::Overlong, String::new()),
            (Line::Text, "next".to_string()),
        ]
    );
}

#[test]
fn invalid_utf8_is_reported_not_panicked() {
    let lines = split(&[&[0xff, b'\n']], 1024);
    assert_eq!(lines, vec![(Line::NonUtf8, String::new())]);
}

#[test]
fn accepts_a_line_exactly_at_the_cap() {
    let lines = split(&[b"1234\n"], 4);
    assert_eq!(lines, vec![(Line::Text, "1234".to_string())]);
}

#[test]
fn classifies_a_request() {
    let inbound =
        parse_inbound(r#"{"jsonrpc":"2.0","id":1,"method":"stall.start","params":{"protocol":1}}"#);
    match inbound {
        Inbound::Request { id, method, params } => {
            assert_eq!(id, 1);
            assert_eq!(method, "stall.start");
            assert_eq!(params["protocol"], 1);
        }
        other => panic!("expected a request, got {other:?}"),
    }
}

#[test]
fn classifies_a_notification() {
    let inbound = parse_inbound(r#"{"jsonrpc":"2.0","method":"log","params":{"level":"info"}}"#);
    match inbound {
        Inbound::Notification { method, params } => {
            assert_eq!(method, "log");
            assert_eq!(params["level"], "info");
        }
        other => panic!("expected a notification, got {other:?}"),
    }
}

#[test]
fn classifies_a_successful_response() {
    let inbound = parse_inbound(r#"{"jsonrpc":"2.0","id":7,"result":{"status":200}}"#);
    match inbound {
        Inbound::Response { id, result } => {
            assert_eq!(id, 7);
            let result = result.expect("result");
            assert_eq!(result["status"], 200);
        }
        other => panic!("expected a response, got {other:?}"),
    }
}

#[test]
fn classifies_a_failed_response() {
    let inbound =
        parse_inbound(r#"{"jsonrpc":"2.0","id":8,"error":{"code":-32001,"message":"boom"}}"#);
    match inbound {
        Inbound::Response { id, result } => {
            assert_eq!(id, 8);
            let failure = result.expect_err("error");
            assert_eq!(failure.code, -32001);
            assert_eq!(failure.message, "boom");
        }
        other => panic!("expected a response, got {other:?}"),
    }
}

#[test]
fn malformed_messages_are_classified_not_fatal() {
    assert!(matches!(parse_inbound("not json"), Inbound::Malformed(_)));
    assert!(matches!(parse_inbound("[]"), Inbound::Malformed(_)));
    assert!(matches!(parse_inbound("{}"), Inbound::Malformed(_)));
    assert!(matches!(
        parse_inbound(r#"{"id":"one","result":1}"#),
        Inbound::Malformed(_)
    ));
}
