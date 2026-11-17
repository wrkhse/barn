//! The protocol's fixed parts, checked against the plan's own examples: a field
//! that is renamed or mistyped fails here.

use std::collections::BTreeMap;

use barn_stall::{
    DEFAULT_MAX_BODY_BYTES, DENIED_CODE, Job, MAX_LINE_BYTES, NetAuth, NetRequestParams,
    NetResponse, PROTOCOL_VERSION, StallStartParams, StallStartResult, fits_line, method,
};
use serde_json::{Value, json};

#[test]
fn the_version_is_one() {
    assert_eq!(PROTOCOL_VERSION, 1);
}

#[test]
fn the_limits_are_the_plans() {
    assert_eq!(MAX_LINE_BYTES, 1_048_576);
    assert_eq!(DEFAULT_MAX_BODY_BYTES, 4_194_304);
}

#[test]
fn the_denied_code_is_the_plans() {
    assert_eq!(DENIED_CODE, -32_001);
}

#[test]
fn stall_start_reads_the_plans_request() {
    let params: StallStartParams = serde_json::from_value(json!({
        "protocol": 1,
        "stall": "sourcefed",
        "state": "~/.local/state/sourcefed"
    }))
    .expect("the plan's stall.start parameters");

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
fn stall_start_writes_the_plans_result() {
    let result = StallStartResult {
        ready: true,
        version: "0.6.0".to_owned(),
        jobs: BTreeMap::from([(
            "poll".to_owned(),
            Job {
                every: "15s".to_owned(),
            },
        )]),
    };

    let written: Value = serde_json::to_value(&result).expect("the result serialises");
    assert_eq!(
        written,
        json!({"ready": true, "version": "0.6.0", "jobs": {"poll": {"every": "15s"}}})
    );
}

#[test]
fn net_request_reads_the_plans_parameters() {
    let params: NetRequestParams = serde_json::from_value(json!({
        "plugin": "github-releases",
        "method": "GET",
        "url": "https://api.github.com/repos/wrkhse/rungs/releases",
        "auth": {"secret": "github-token", "as": "bearer"},
        "budget": "github"
    }))
    .expect("the plan's net.request parameters");

    assert_eq!(
        params,
        NetRequestParams {
            plugin: "github-releases".to_owned(),
            method: "GET".to_owned(),
            url: "https://api.github.com/repos/wrkhse/rungs/releases".to_owned(),
            auth: Some(NetAuth {
                secret: "github-token".to_owned(),
                scheme: "bearer".to_owned(),
            }),
            budget: Some("github".to_owned()),
        }
    );
}

#[test]
fn net_request_writes_the_as_field_and_omits_what_is_absent() {
    let params = NetRequestParams {
        plugin: "github-releases".to_owned(),
        method: "GET".to_owned(),
        url: "https://api.github.com/".to_owned(),
        auth: Some(NetAuth {
            secret: "github-token".to_owned(),
            scheme: "bearer".to_owned(),
        }),
        budget: None,
    };

    let written: Value = serde_json::to_value(&params).expect("the parameters serialise");
    assert_eq!(
        written,
        json!({
            "plugin": "github-releases",
            "method": "GET",
            "url": "https://api.github.com/",
            "auth": {"secret": "github-token", "as": "bearer"}
        })
    );
}

#[test]
fn net_request_reads_the_plans_result() {
    let response: NetResponse = serde_json::from_value(json!({
        "status": 200,
        "headers": {"etag": "..."},
        "body": "..."
    }))
    .expect("the plan's net.request result");

    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers.get("etag").map(String::as_str),
        Some("...")
    );
    assert_eq!(response.body, "...");
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
fn a_line_fits_up_to_the_limit() {
    assert!(fits_line(0));
    assert!(fits_line(MAX_LINE_BYTES));
    assert!(!fits_line(1_048_577));
}
