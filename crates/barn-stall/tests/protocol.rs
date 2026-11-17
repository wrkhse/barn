//! The protocol's fixed parts, checked against the page: its examples and its
//! limits table are read from docs/stall-protocol.md itself, so a constant or a
//! field that drifts from the page fails here.

use std::time::Duration;

use barn_stall::{
    AuthScheme, BodyEncoding, DEFAULT_MAX_BODY_BYTES, DENIED_CODE, DrainParams, DrainResult,
    DurationError, Headers, HealthResult, Job, JobRunParams, JobRunResult, LogLevel, LogParams,
    MAX_LINE_BYTES, NO_SUCH_METHOD, NetAuth, NetRequestParams, NetResponse, PROTOCOL_VERSION,
    StallStartAnswer, StallStartParams, Token, VERSION_REFUSED, method, parse_duration,
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

/// The page's message with this id that carries an error.
fn errored(id: i64) -> Option<Value> {
    lines()?.into_iter().find(|message| {
        message.get("id").and_then(Value::as_i64) == Some(id) && message.get("error").is_some()
    })
}

/// The page's message that sends this method without an id, as a log line does.
fn notified(name: &str) -> Option<Value> {
    lines()?.into_iter().find(|message| {
        message.get("method").and_then(Value::as_str) == Some(name) && message.get("id").is_none()
    })
}

/// A message's error code.
fn code(message: &Value) -> Option<i64> {
    message
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64)
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
    assert_eq!(pinned("VERSION_REFUSED"), i64::from(VERSION_REFUSED));
    assert_eq!(pinned("NO_SUCH_METHOD"), i64::from(NO_SUCH_METHOD));
}

#[test]
fn the_denied_error_carries_the_pages_code() {
    let denied = errored(8).expect("the page carries a denied error");

    assert_eq!(code(&denied), Some(i64::from(DENIED_CODE)));
}

#[test]
fn a_refused_version_carries_the_pages_code() {
    let refused = errored(1).expect("the page answers a refused stall.start with an error");

    assert_eq!(code(&refused), Some(i64::from(VERSION_REFUSED)));
    assert_eq!(
        refused
            .get("error")
            .and_then(|error| error.get("data"))
            .and_then(|data| data.get("protocol")),
        Some(&Value::from(2))
    );
}

#[test]
fn a_stall_start_at_a_stall_carries_the_pages_code() {
    let refused = errored(5).expect("the page answers a stall.start at a stall with an error");

    assert_eq!(code(&refused), Some(i64::from(NO_SUCH_METHOD)));
}

#[test]
fn stall_start_reads_the_pages_request() {
    let params: StallStartParams = serde_json::from_value(
        sent(1)
            .expect("the page sends stall.start with id 1")
            .get("params")
            .cloned()
            .expect("stall.start carries params"),
    )
    .expect("the page's stall.start parameters");
    let token: Token = serde_json::from_value(json!("9f2c5a1e7b3d4c6f8a0b1c2d3e4f5a6b"))
        .expect("the page's token is a string");

    assert_eq!(params.protocol, PROTOCOL_VERSION);
    assert_eq!(params.stall, "sourcefed");
    assert_eq!(params.token, token);
    assert_eq!(params.version, "0.6.0");
    assert_eq!(params.jobs.len(), 2);

    let poll = params.jobs.get("poll").expect("the page declares poll");
    assert_eq!(
        poll,
        &Job {
            every: "15s".to_owned()
        }
    );
    assert_eq!(parse_duration(&poll.every), Ok(Duration::from_secs(15)));
}

#[test]
fn stall_start_reads_the_pages_answer() {
    let answer: StallStartAnswer = serde_json::from_value(
        answered(1)
            .expect("the page answers stall.start with id 1")
            .get("result")
            .cloned()
            .expect("stall.start carries a result"),
    )
    .expect("the page's stall.start answer");

    assert!(
        answer.state.starts_with('/'),
        "the state folder is absolute: {}",
        answer.state
    );
    assert_eq!(
        answer.state,
        "/Users/steven/.local/state/barn/stalls/sourcefed"
    );
}

#[test]
fn a_token_never_reaches_a_debug_line() {
    let params: StallStartParams = serde_json::from_value(
        sent(1)
            .expect("the page sends stall.start with id 1")
            .get("params")
            .cloned()
            .expect("stall.start carries params"),
    )
    .expect("the page's stall.start parameters");
    let token: Token =
        serde_json::from_value(json!("a-secret-value")).expect("a token is a string");

    assert_eq!(format!("{token:?}"), "[redacted]");
    assert!(format!("{params:?}").contains("[redacted]"));
    assert!(!format!("{params:?}").contains("9f2c5a1e"));
}

#[test]
fn health_reads_the_pages_result() {
    let result: HealthResult = serde_json::from_value(
        answered(2)
            .expect("the page answers stall.health with id 2")
            .get("result")
            .cloned()
            .expect("stall.health carries a result"),
    )
    .expect("the page's stall.health result");

    assert!(result.ok);
    assert_eq!(result.status, "ready");
}

#[test]
fn drain_reads_the_pages_parameters_and_result() {
    let params: DrainParams = serde_json::from_value(
        sent(3)
            .expect("the page sends stall.drain with id 3")
            .get("params")
            .cloned()
            .expect("stall.drain carries params"),
    )
    .expect("the page's stall.drain parameters");
    let result: DrainResult = serde_json::from_value(
        answered(3)
            .expect("the page answers stall.drain with id 3")
            .get("result")
            .cloned()
            .expect("stall.drain carries a result"),
    )
    .expect("the page's stall.drain result");

    assert_eq!(params.deadline_ms, 5000);
    assert!(result.drained);
}

#[test]
fn job_run_reads_the_pages_parameters_and_result() {
    let params: JobRunParams = serde_json::from_value(
        sent(4)
            .expect("the page sends job.run with id 4")
            .get("params")
            .cloned()
            .expect("job.run carries params"),
    )
    .expect("the page's job.run parameters");
    let result: JobRunResult = serde_json::from_value(
        answered(4)
            .expect("the page answers job.run with id 4")
            .get("result")
            .cloned()
            .expect("job.run carries a result"),
    )
    .expect("the page's job.run result");

    assert_eq!(params.job, "poll");
    assert!(result.ran);
}

#[test]
fn log_reads_the_pages_line() {
    let params: LogParams = serde_json::from_value(
        notified("log")
            .expect("the page sends log without an id")
            .get("params")
            .cloned()
            .expect("log carries params"),
    )
    .expect("the page's log parameters");

    assert_eq!(params.level, LogLevel::Info);
    assert_eq!(params.message, "polled 12 releases");
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
                scheme: AuthScheme::Bearer,
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
fn an_auth_scheme_writes_as_its_wire_name() {
    assert_eq!(
        serde_json::to_value(AuthScheme::Bearer).expect("the scheme serialises"),
        json!("bearer")
    );
    assert_eq!(
        serde_json::from_value::<AuthScheme>(json!("bearer")).expect("the wire name reads back"),
        AuthScheme::Bearer
    );
}

#[test]
fn an_unknown_auth_scheme_is_refused() {
    let refused =
        serde_json::from_value::<NetAuth>(json!({"secret": "github-token", "as": "basic"}));

    assert!(refused.is_err(), "a scheme off the list is a new version");
}

#[test]
fn every_log_level_keeps_its_name() {
    let levels = [
        (LogLevel::Debug, "debug"),
        (LogLevel::Info, "info"),
        (LogLevel::Warn, "warn"),
        (LogLevel::Error, "error"),
    ];

    for (level, wire) in levels {
        assert_eq!(
            serde_json::to_value(level).expect("the level serialises"),
            json!(wire)
        );
    }
}

#[test]
fn a_duration_reads_its_unit() {
    let durations = [
        ("250ms", Duration::from_millis(250)),
        ("15s", Duration::from_secs(15)),
        ("10m", Duration::from_secs(600)),
        ("2h", Duration::from_secs(7_200)),
    ];

    for (text, duration) in durations {
        assert_eq!(parse_duration(text), Ok(duration), "{text}");
    }
}

#[test]
fn an_empty_duration_is_refused() {
    assert_eq!(parse_duration(""), Err(DurationError::Empty));
}

#[test]
fn a_duration_without_a_unit_is_refused() {
    assert_eq!(parse_duration("15"), Err(DurationError::MissingUnit));
}

#[test]
fn a_duration_with_an_unknown_unit_is_refused() {
    assert_eq!(parse_duration("15d"), Err(DurationError::UnknownUnit));
}

#[test]
fn a_duration_without_digits_is_refused() {
    assert_eq!(parse_duration("fifteen"), Err(DurationError::NotNumeric));
}

#[test]
fn a_zero_duration_is_refused() {
    assert_eq!(parse_duration("0s"), Err(DurationError::Zero));
}

#[test]
fn a_duration_too_large_for_its_unit_is_refused() {
    assert_eq!(
        parse_duration("18446744073709551615h"),
        Err(DurationError::TooLarge)
    );
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

#[test]
fn every_duration_refusal_says_what_is_wrong() {
    let refusals = [
        ("", "the duration is empty"),
        ("fifteen", "does not start with digits"),
        ("15", "has no unit"),
        ("15d", "is not one of"),
        ("0s", "is zero"),
        ("18446744073709551615h", "too large"),
    ];

    for (text, phrase) in refusals {
        let error = parse_duration(text).expect_err("the duration is refused");

        assert!(error.to_string().contains(phrase), "{text}: {error}");
    }
}
