//! The protocol's fixed parts, checked against the page: its examples and its
//! limits table are read from docs/stall-protocol.md itself, so a constant or a
//! field that drifts from the page fails here.

use std::collections::BTreeMap;

use barn_stall::protocol::Job;
use barn_stall::protocol::TOKEN_TTL_SECS;
use barn_stall::{
    BodyEncoding, DEFAULT_MAX_BODY_BYTES, DENIED_CODE, Headers, MAX_LINE_BYTES, NetAuth,
    NetRequestParams, NetResponse, PROTOCOL_VERSION, StallStartParams, StallStartResult, method,
};
use serde_json::{Value, json};

/// The protocol page, compiled in, so these tests read exactly the text barn and
/// a stall's author read.
const PAGE: &str = include_str!("../../../docs/stall-protocol.md");

/// Every JSON-RPC line on the page, parsed; none, if one does not parse.
fn lines() -> Option<Vec<Value>> {
    PAGE.lines()
        .filter(|line| line.starts_with("{\"jsonrpc\""))
        .map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The page's one declared job, keyed by its name.
fn named_jobs() -> BTreeMap<String, Job> {
    BTreeMap::from([(
        "poll".to_owned(),
        Job {
            every: "15s".to_owned(),
        },
    )])
}

/// The page's message with this id that sends a method.
fn sent(id: i64) -> Option<Value> {
    lines()?.into_iter().find(|message| {
        message.get("id").and_then(Value::as_i64) == Some(id) && message.get("method").is_some()
    })
}

/// The page's message with this id that answers.
fn answered(id: i64) -> Option<Value> {
    lines()?.into_iter().find(|message| {
        message.get("id").and_then(Value::as_i64) == Some(id) && message.get("result").is_some()
    })
}

/// The page's constants table: each name and the value it pins.
fn table() -> Vec<(String, i64)> {
    PAGE.lines()
        .filter(|line| line.starts_with("| `"))
        .filter_map(|line| {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            let name = cells.get(1)?.strip_prefix('`')?.strip_suffix('`')?;
            let value: i64 = cells
                .get(2)?
                .strip_prefix('`')?
                .strip_suffix('`')?
                .parse()
                .ok()?;

            Some((name.to_owned(), value))
        })
        .collect()
}

#[test]
fn the_pages_table_pins_the_constants() {
    let table = table();
    let pinned = |name: &str| -> i64 {
        table
            .iter()
            .find(|(pinned, _)| pinned == name)
            .map(|(_, value)| *value)
            .expect("the page pins this constant")
    };

    assert_eq!(pinned("PROTOCOL_VERSION"), i64::from(PROTOCOL_VERSION));
    assert_eq!(
        pinned("MAX_LINE_BYTES"),
        i64::try_from(MAX_LINE_BYTES).expect("fits an i64")
    );
    assert_eq!(
        pinned("DEFAULT_MAX_BODY_BYTES"),
        i64::try_from(DEFAULT_MAX_BODY_BYTES).expect("fits an i64")
    );
    assert_eq!(pinned("DENIED_CODE"), i64::from(DENIED_CODE));
    assert_eq!(
        pinned("TOKEN_TTL_SECS"),
        i64::try_from(TOKEN_TTL_SECS).expect("fits an i64")
    );
}

#[test]
fn a_start_token_lasts_thirty_seconds() {
    assert_eq!(TOKEN_TTL_SECS, 30);
}

#[test]
fn the_denied_error_carries_the_pages_code() {
    let code = lines()
        .expect("every JSON-RPC line on the page parses")
        .into_iter()
        .filter_map(|message| message.get("error").cloned())
        .find(|error| error.get("message").and_then(Value::as_str) == Some("denied"))
        .and_then(|error| error.get("code").and_then(Value::as_i64))
        .expect("the page carries a denied error");

    assert_eq!(code, i64::from(DENIED_CODE));
}

#[test]
fn stall_start_reads_the_pages_request() {
    let request = sent(1);
    let wire = request.as_ref().and_then(|message| message.get("params"));
    let params = StallStartParams {
        protocol: PROTOCOL_VERSION,
        token: "…".to_owned(),
        jobs: named_jobs(),
    };

    assert_eq!(
        request
            .as_ref()
            .and_then(|message| message.get("method"))
            .and_then(Value::as_str),
        Some(method::STALL_START)
    );
    assert_eq!(
        wire.cloned()
            .and_then(|value| serde_json::from_value(value).ok()),
        Some(params.clone())
    );
    assert_eq!(serde_json::to_value(params).ok().as_ref(), wire);
}

#[test]
fn stall_start_reads_the_pages_result() {
    let reply = answered(1);
    let wire = reply.as_ref().and_then(|message| message.get("result"));
    let result = StallStartResult {
        state: "/var/lib/barn/sourcefed".to_owned(),
        jobs: named_jobs(),
    };

    assert_eq!(
        wire.cloned()
            .and_then(|value| serde_json::from_value(value).ok()),
        Some(result.clone())
    );
    assert_eq!(serde_json::to_value(result).ok().as_ref(), wire);
}

#[test]
fn a_start_result_keeps_jobs_under_their_names() {
    let reply = answered(1).expect("the page answers stall.start");
    let result: StallStartResult = serde_json::from_value(
        reply
            .get("result")
            .cloned()
            .expect("the start reply carries a result"),
    )
    .expect("the page's start result");

    assert_eq!(
        result.jobs.get("poll"),
        Some(&Job {
            every: "15s".to_owned(),
        })
    );
    assert_eq!(result.jobs.get("missing"), None);
}

#[test]
fn the_page_fixes_the_oversize_error_line() {
    let error = lines()
        .expect("every JSON-RPC line on the page parses")
        .into_iter()
        .find_map(|message| {
            message
                .get("error")
                .filter(|error| error.get("code").and_then(Value::as_i64) == Some(-32600))
                .cloned()
        })
        .expect("the page carries the oversize error");

    assert_eq!(
        error,
        json!({
            "code": -32600,
            "message": "stall start message exceeds the protocol line limit"
        })
    );
}

#[test]
fn start_parameters_keep_the_token_out_of_diagnostics() {
    let params = StallStartParams {
        protocol: PROTOCOL_VERSION,
        token: "secret".to_owned(),
        jobs: BTreeMap::new(),
    };

    assert_eq!(
        format!("{params:?}"),
        "StallStartParams { protocol: 1, token: \"[redacted]\", jobs: {} }"
    );
}

#[test]
fn a_start_without_jobs_omits_the_field_and_reads_it_as_empty() {
    let params: StallStartParams =
        serde_json::from_value(json!({"protocol": 1, "token": "secret"}))
            .expect("start parameters without jobs");
    let wire = json!({"state": "/state"});
    let result = serde_json::from_value::<StallStartResult>(wire).expect("jobs default");

    assert_eq!(params.jobs, BTreeMap::new());
    assert_eq!(result.jobs, BTreeMap::new());
    assert_eq!(
        serde_json::to_value(params).expect("params serialise"),
        json!({"protocol": 1, "token": "secret"})
    );
    assert_eq!(
        serde_json::to_value(result).expect("result serialise"),
        json!({"state": "/state"})
    );
}

#[test]
fn net_request_reads_the_pages_parameters() {
    let params: NetRequestParams = serde_json::from_value(
        sent(7)
            .expect("the page sends net.request with id 7")
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
            body_encoding: BodyEncoding::Utf8,
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
        answered(7)
            .expect("the page answers net.request with id 7")
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
fn the_pages_post_carries_headers_and_a_utf8_body() {
    let params: NetRequestParams = serde_json::from_value(
        sent(11)
            .expect("the page sends a POST with id 11")
            .get("params")
            .cloned()
            .expect("the POST carries params"),
    )
    .expect("the page's POST parameters");

    assert_eq!(params.method, "POST");
    assert_eq!(
        params.headers,
        vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "application/json".to_owned()),
        ]
    );
    assert_eq!(params.body_encoding, BodyEncoding::Utf8);
    assert!(params.body.is_some(), "the POST has a body");
}

#[test]
fn the_pages_post_result_keeps_a_repeated_header() {
    let response: NetResponse = serde_json::from_value(
        answered(11)
            .expect("the page answers the POST with id 11")
            .get("result")
            .cloned()
            .expect("the POST carries a result"),
    )
    .expect("the page's POST result");

    let cookies: Vec<&str> = response
        .headers
        .iter()
        .filter(|(name, _)| name == "set-cookie")
        .map(|(_, value)| value.as_str())
        .collect();

    assert_eq!(cookies, ["a=1", "b=2"]);
}

#[test]
fn the_pages_base64_body_reads_as_base64() {
    let params: NetRequestParams = serde_json::from_value(
        sent(12)
            .expect("the page sends a base64 body with id 12")
            .get("params")
            .cloned()
            .expect("the PUT carries params"),
    )
    .expect("the page's PUT parameters");

    assert_eq!(params.body_encoding, BodyEncoding::Base64);
    assert_eq!(params.body.as_deref(), Some("aGk="));
    assert_eq!(params.plugin, None);
}

#[test]
fn the_pages_base64_result_reads_as_an_empty_utf8_body() {
    let response: NetResponse = serde_json::from_value(
        answered(12)
            .expect("the page answers the PUT with id 12")
            .get("result")
            .cloned()
            .expect("the PUT carries a result"),
    )
    .expect("the page's PUT result");

    assert_eq!(response.status, 201);
    assert_eq!(response.headers, Headers::new());
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
        body_encoding: BodyEncoding::Utf8,
        auth: None,
        budget: None,
    };

    assert_eq!(
        serde_json::to_value(&params).expect("the parameters serialise"),
        json!({
            "method": "POST",
            "url": "https://api.github.com/",
            "headers": [["content-type", "application/json"]],
            "body": "{}",
            "body_encoding": "utf8"
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
