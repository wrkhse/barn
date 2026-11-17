//! The prelude as QuickJS actually sees it: the provider-facing globals are
//! installed, the http shim shapes one GET request, and a provider-style async
//! poll resolves through the job queue. In-process only: no sockets, no child
//! processes, no signals.
//!
//! QuickJS only: the same prelude under djs is tests/engine_realm.rs.

#![cfg(feature = "engine-rquickjs")]

use rquickjs::{Context, Function, Runtime, Value};

const PRELUDE: &str = include_str!("../src/prelude.js");

#[test]
fn the_prelude_runs_under_quickjs() {
    let runtime = Runtime::new().expect("runtime");
    let context = Context::full(&runtime).expect("context");

    context.with(|ctx| {
        let http = Function::new(ctx.clone(), |payload: String| -> String {
            let request: serde_json::Value =
                serde_json::from_str(&payload).expect("http payload is JSON");
            assert_eq!(request["method"], "GET");
            assert_eq!(
                request["url"],
                "https://api.github.com/repos/example-org/repo-1/issues?state=open"
            );
            assert_eq!(request["headers"]["accept"], "application/vnd.github+json");
            assert!(request.get("body").is_none(), "the shim must not send a body");
            serde_json::json!({
                "ok": true,
                "response": {
                    "status": 200,
                    "headers": { "ETag": "\"v1\"" },
                    "body": "{\"hello\":\"world\"}"
                }
            })
            .to_string()
        })
        .expect("http host function");
        ctx.globals()
            .set("__stallHostHttp", http)
            .expect("install http host");
        let log = Function::new(ctx.clone(), |_level: String, _message: String| {})
            .expect("log host function");
        ctx.globals().set("__stallLog", log).expect("install log host");
        ctx.globals()
            .set(
                "__stall_cfg",
                r#"{"plugin":"github-issues","faults":[],"env":{"SOURCEFED_JIRA_BASE_URL":"https://example.atlassian.net","GH_TOKEN":"ghp_test"}}"#,
            )
            .expect("realm config");
        ctx.eval::<Value, _>(PRELUDE).expect("prelude evaluates");

        let probe: String = ctx
            .eval(
                r#"
                globalThis.__stall.pluginPoll = async function () {
                  var response = await globalThis.__stall.http(
                    "https://api.github.com/repos/example-org/repo-1/issues?state=open",
                    { accept: "application/vnd.github+json" }
                  );
                  var body = await response.json();
                  return {
                    cursor: { etag: response.headers.get("etag") },
                    events: [{ kind: "item.matched", id: "e1", at: "t", summary: body.hello, actionable: false }],
                  };
                };
                JSON.stringify({
                  env: process.env.SOURCEFED_JIRA_BASE_URL,
                  token: process.env.GH_TOKEN,
                  query: new URLSearchParams({ jql: "project = PROJ1", maxResults: "100" }).toString(),
                  jql: new URL("https://example.atlassian.net/rest/api/3/search/jql?jql=x").searchParams.get("jql"),
                  text: new TextDecoder().decode(new TextEncoder().encode("héllo")),
                  btoa: btoa("hi"),
                  timer: setTimeout(function () { throw new Error("timer fired"); }, 1),
                  signal: AbortSignal.timeout(1).aborted,
                })
                "#,
            )
            .expect("shim probe");
        let probe: serde_json::Value = serde_json::from_str(&probe).expect("probe json");
        assert_eq!(probe["env"], "https://example.atlassian.net");
        assert_eq!(probe["token"], "ghp_test");
        assert_eq!(probe["query"], "jql=project+%3D+PROJ1&maxResults=100");
        assert_eq!(probe["jql"], "x");
        assert_eq!(probe["text"], "héllo");
        assert_eq!(probe["btoa"], "aGk=");
        assert_eq!(probe["timer"], 1);
        assert_eq!(probe["signal"], false);

        ctx.eval::<Value, _>(
            r#"globalThis.__probe = null; __stallInvokePoll('{}').then(function (value) { globalThis.__probe = value; });"#,
        )
        .expect("call pluginPoll");
        while ctx.execute_pending_job() {}

        let envelope: String = ctx.globals().get("__probe").expect("poll envelope");
        let envelope: serde_json::Value = serde_json::from_str(&envelope).expect("envelope json");
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["result"]["cursor"]["etag"], "\"v1\"");
        assert_eq!(envelope["result"]["events"][0]["kind"], "item.matched");
        assert_eq!(envelope["result"]["events"][0]["summary"], "world");
    });
}
