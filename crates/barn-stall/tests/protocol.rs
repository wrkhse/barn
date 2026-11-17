//! The protocol's fixed parts, checked against the JSON in the protocol page:
//! a field that is renamed, retyped or dropped fails here.

use std::collections::BTreeMap;

use barn_stall::{
    BodyEncoding, DEFAULT_MAX_BODY_BYTES, DENIED_CODE, Headers, Job, MAX_LINE_BYTES, NetAuth,
    NetRequestParams, NetResponse, PROTOCOL_VERSION, StallStartParams, StallStartResult, method,
};
use serde_json::{Value, json};

/// The protocol page, compiled in, so these tests read exactly the text barn and
/// a stall's author read.
const PAGE: &str = include_str!("../../../docs/stall-protocol.md");

/// Every JSON-RPC line in the page, parsed.
fn lines() -> Result<Vec<Value>, serde_json::Error> {
    PAGE.lines()
        .filter(|line| line.starts_with("{\"jsonrpc\""))
        .map(serde_json::from_str)
        .collect()
}

/// The page's message that sends this method, when the page has one.
fn sent(method: &str) -> Option<Value> {
    lines()
        .ok()?
        .into_iter()
        .find(|message| message.get("method").and_then(Value::as_str) == Some(method))
}

/// The page's answer that follows this method's message, when the page has one.
fn answered(method: &str) -> Option<Value> {
    lines()
        .ok()?
        .into_iter()
        .skip_while(|message| message.get("method").and_then(Value::as_str) != Some(method))
        .nth(1)
}

#[test]
fn the_version_is_one() {
    assert_eq!(PROTOCOL_VERSION, 1);
}

#[test]
fn the_limits_are_the_plans() {
    assert_eq!(MAX_LINE_BYTES, 8_388_608);
    assert_eq!(DEFAULT_MAX_BODY_BYTES, 4_194_304);
}

#[test]
fn the_denied_code_is_the_pages() {
    let code = lines()
        .expect("the page's JSON-RPC lines")
        .into_iter()
        .find_map(|message| {
            message
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64)
        })
        .expect("the page carries a denied error");

    assert_eq!(code, i64::from(DENIED_CODE));
}

#[test]
fn stall_start_reads_the_pages_request() {
    let params: StallStartParams = serde_json::from_value(
        sent("stall.start")
            .expect("the page sends this method")
            .get("params")
            .cloned()
            .expect("stall.start carries params"),
    )
    .expect("the page's stall.start parameters");

    assert_eq!(
        params,
        StallStartParams {
            protocol: PROTOCOL_VERSION,
            stall: "sourcefed".to_owned(),
            state: "~/.local/state/sourcefed".to_owned(),
        }
    );
}

#[test]
fn stall_start_reads_the_pages_result() {
    let result: StallStartResult = serde_json::from_value(
        answered("stall.start")
            .expect("the page answers this method")
            .get("result")
            .cloned()
            .expect("stall.start carries a result"),
    )
    .expect("the page's stall.start result");

    assert!(result.ready);
    assert_eq!(result.version, "0.6.0");
    assert_eq!(
        result.jobs,
        BTreeMap::from([(
            "poll".to_owned(),
            Job {
                every: "15s".to_owned(),
            },
        )])
    );
}

#[test]
fn net_request_reads_the_pages_parameters() {
    let params: NetRequestParams = serde_json::from_value(
        sent("net.request")
            .expect("the page sends this method")
            .get("params")
            .cloned()
            .expect("net.request carries params"),
    )
    .expect("the page's net.request parameters");

    assert_eq!(
        params,
        NetRequestParams {
            plugin: Some("github-releases".to_owned()),
            method: "GET".to_owned(),
            url: "https://api.github.com/repos/wrkhse/rungs/releases".to_owned(),
            headers: Headers::new(),
            body: None,
            auth: Some(NetAuth {
                secret: "github-token".to_owned(),
                scheme: "bearer".to_owned(),
            }),
            budget: Some("github".to_owned()),
        }
    );
}

#[test]
fn net_request_reads_the_pages_result() {
    let response: NetResponse = serde_json::from_value(
        answered("net.request")
            .expect("the page answers this method")
            .get("result")
            .cloned()
            .expect("net.request carries a result"),
    )
    .expect("the page's net.request result");

    assert_eq!(response.status, 200);
    assert_eq!(response.headers, vec![("etag".to_owned(), "v1".to_owned())]);
    assert_eq!(response.body, "hello");
    assert_eq!(response.body_encoding, BodyEncoding::Utf8);
}

#[test]
fn a_request_writes_what_it_carries_and_omits_what_it_does_not() {
    let params = NetRequestParams {
        plugin: None,
        method: "POST".to_owned(),
        url: "https://api.github.com/".to_owned(),
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: Some("{}".to_owned()),
        auth: None,
        budget: None,
    };

    assert_eq!(
        serde_json::to_value(&params).expect("the parameters serialise"),
        json!({
            "method": "POST",
            "url": "https://api.github.com/",
            "headers": [["content-type", "application/json"]],
            "body": "{}"
        })
    );
}

#[test]
fn a_response_writes_repeated_headers_and_a_base64_body() {
    let response = NetResponse {
        status: 200,
        headers: vec![
            ("set-cookie".to_owned(), "a=1".to_owned()),
            ("set-cookie".to_owned(), "b=2".to_owned()),
        ],
        body: "aGk=".to_owned(),
        body_encoding: BodyEncoding::Base64,
    };

    assert_eq!(
        serde_json::to_value(&response).expect("the response serialises"),
        json!({
            "status": 200,
            "headers": [["set-cookie", "a=1"], ["set-cookie", "b=2"]],
            "body": "aGk=",
            "body_encoding": "base64"
        })
    );
}

#[test]
fn a_response_without_headers_reads_the_defaults() {
    let response: NetResponse =
        serde_json::from_value(json!({"status": 204})).expect("the smallest result");

    assert_eq!(response.headers, Headers::new());
    assert_eq!(response.body, "");
    assert_eq!(response.body_encoding, BodyEncoding::Utf8);
}

#[test]
fn every_method_keeps_its_name() {
    let names = [
        (method::STALL_START, "stall.start"),
        (method::STALL_HEALTH, "stall.health"),
        (method::STALL_DRAIN, "stall.drain"),
        (method::REQUEST, "request"),
        (method::CANCEL, "cancel"),
        (method::STREAM_SEND, "stream.send"),
        (method::STREAM_END, "stream.end"),
        (method::WEBHOOK, "webhook"),
        (method::JOB_RUN, "job.run"),
        (method::NET_REQUEST, "net.request"),
        (method::MCP_CALL, "mcp.call"),
        (method::LOG, "log"),
    ];

    for (constant, wire) in names {
        assert_eq!(constant, wire);
    }
}
