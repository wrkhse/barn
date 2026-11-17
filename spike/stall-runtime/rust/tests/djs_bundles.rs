//! The real bundled plugins under the djs engine: the same two bundles the harness ships
//! (`plugins-dist/github-issues.js`, `plugins-dist/jira-query.js`) compiled into a djs
//! snapshot with the same prelude, then polled against an answer shaped like the harness's
//! mock upstream. This is the proof that the pinned providers' JavaScript runs unchanged
//! under djs. In process only: no sockets, no child processes, no signals.

#![cfg(feature = "engine-djs")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use djs_embed::{CallId, Data, HostFn, Instance, Limits, Snapshot, Source, Step, Value};
use serde_json::{json, Value as Json};

const PRELUDE: &str = include_str!("../src/prelude.js");
const HOST_HTTP: u32 = 1;
const HOST_LOG: u32 = 2;

/// The mock upstream's repository, as the harness's workload names one.
const REPO: &str = "example-org/repo-1";

#[test]
fn the_github_issues_bundle_polls_under_djs() {
    let Some(bundle) = read_bundle("github-issues") else {
        return;
    };
    let mut instance = build_instance("github-issues", &bundle, &github_env());

    // The harness's shape: 100 issues plus one pull request the provider must drop, and an
    // ETag that moves when the list changes, which is what makes `if-none-match` a 304.
    let mut broker = Broker::github();

    let first = poll(&mut instance, &mut broker, &github_input(Json::Null));
    assert_eq!(first["ok"], true, "first envelope: {first}");
    assert_eq!(
        first["result"]["events"].as_array().map(Vec::len),
        Some(0),
        "the first poll primes silently: {first}"
    );
    let cursor = &first["result"]["cursor"];
    assert_eq!(cursor["primed"], true, "cursor: {cursor}");
    assert_eq!(cursor["etag"], "\"e-100\"", "cursor: {cursor}");
    let keys = cursor_keys(cursor);
    assert_eq!(keys.len(), 100, "the pull request is filtered: {cursor}");
    assert!(
        !keys.iter().any(|key| key == "example-org/repo-1#999"),
        "cursor: {cursor}"
    );

    let primed = broker.requests[0].clone();
    assert_eq!(primed["method"], "GET");
    assert!(
        primed["url"]
            .as_str()
            .is_some_and(|url| url.contains("/repos/example-org/repo-1/issues")),
        "request url: {primed}"
    );
    assert!(
        primed.get("body").is_none(),
        "the shim sends no body: {primed}"
    );
    assert!(
        primed["headers"].get("if-none-match").is_none(),
        "an unprimed poll sends no validator: {primed}"
    );

    // One new issue, with the same 100 listed again so nothing is reported gone.
    broker.add_issue(101);
    let second = poll(&mut instance, &mut broker, &github_input(cursor.clone()));
    assert_eq!(second["ok"], true, "second envelope: {second}");
    let events = second["result"]["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1, "exactly one event: {second}");
    assert_eq!(events[0]["kind"], "item.matched");
    assert_eq!(events[0]["subject"]["key"], "example-org/repo-1#101");
    assert_eq!(second["result"]["cursor"]["etag"], "\"e-101\"");

    let conditional = broker.requests[1].clone();
    assert_eq!(conditional["method"], "GET");
    assert!(
        conditional["url"]
            .as_str()
            .is_some_and(|url| url.contains("/repos/example-org/repo-1/issues")),
        "request url: {conditional}"
    );
    assert_eq!(
        conditional["headers"]["if-none-match"], "\"e-100\"",
        "the second poll sends the cursor's ETag: {conditional}"
    );
    assert!(
        conditional.get("body").is_none(),
        "the shim sends no body: {conditional}"
    );
    assert_eq!(broker.statuses, [200, 200], "two changed lists were served");

    // Nothing changed upstream, so the same validator gets a 304 and no events at all.
    let third = poll(
        &mut instance,
        &mut broker,
        &github_input(second["result"]["cursor"].clone()),
    );
    assert_eq!(third["ok"], true, "third envelope: {third}");
    assert_eq!(
        third["result"]["events"].as_array().map(Vec::len),
        Some(0),
        "an unchanged list reports nothing: {third}"
    );
    assert_eq!(third["result"]["cursor"]["etag"], "\"e-101\"");
    assert_eq!(
        broker.statuses,
        [200, 200, 304],
        "the third poll's validator got a 304"
    );
}

#[test]
fn the_jira_query_bundle_polls_under_djs() {
    let Some(bundle) = read_bundle("jira-query") else {
        return;
    };
    let mut instance = build_instance("jira-query", &bundle, &jira_env());

    let mut broker = Broker::jira("PROJ1");

    let first = poll(&mut instance, &mut broker, &jira_input(Json::Null));
    assert_eq!(first["ok"], true, "first envelope: {first}");
    assert_eq!(
        first["result"]["events"].as_array().map(Vec::len),
        Some(0),
        "the first poll primes silently: {first}"
    );
    let cursor = &first["result"]["cursor"];
    assert_eq!(cursor["primed"], true, "cursor: {cursor}");
    let keys = cursor_keys(cursor);
    assert_eq!(keys.len(), 2, "two issues primed: {cursor}");
    assert!(
        keys.iter().any(|key| key == "PROJ1-1") && keys.iter().any(|key| key == "PROJ1-2"),
        "cursor: {cursor}"
    );

    let url = broker.requests[0]["url"].as_str().expect("request url");
    assert!(
        url.starts_with("https://example.atlassian.net/rest/api/3/search/jql?"),
        "request url: {url}"
    );
    let jql = query_param(url, "jql").expect("a jql parameter");
    assert!(jql.contains("PROJ1"), "jql: {jql}");
    assert_eq!(broker.requests[0]["method"], "GET");

    broker.add_issue(3);
    let second = poll(&mut instance, &mut broker, &jira_input(cursor.clone()));
    assert_eq!(second["ok"], true, "second envelope: {second}");
    let events = second["result"]["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1, "exactly one event: {second}");
    assert_eq!(events[0]["kind"], "item.matched");
    assert_eq!(events[0]["subject"]["key"], "PROJ1-3");

    let url = broker.requests[1]["url"].as_str().expect("request url");
    assert!(url.contains("/rest/api/3/search/jql"), "request url: {url}");
    assert!(
        query_param(url, "jql").is_some_and(|jql| jql.contains("PROJ1")),
        "request url: {url}"
    );
    assert_eq!(broker.statuses, [200, 200], "two searches were served");
}

// ---------------------------------------------------------------------------
// The two bundles
// ---------------------------------------------------------------------------

/// The plugin directory the harness sets, else the one `scripts/build-plugins.mjs` writes.
fn plugin_dir() -> PathBuf {
    match std::env::var("STALL_PLUGIN_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins-dist"),
    }
}

/// The bundle's text, or none (with a note) when this checkout has no built plugins.
fn read_bundle(plugin: &str) -> Option<String> {
    let path = plugin_dir().join(format!("{plugin}.js"));
    match fs::read_to_string(&path) {
        Ok(bundle) => Some(bundle),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("skipped: {} is missing", path.display());
            None
        }
        Err(error) => panic!("cannot read {}: {error}", path.display()),
    }
}

/// The realm config the prelude reads, with only the variables these bundles can need.
fn github_env() -> Json {
    json!({
        "plugin": "github-issues",
        "faults": [],
        "env": {
            "SOURCEFED_JIRA_BASE_URL": "https://example.atlassian.net",
            "GH_TOKEN": "ghp_test",
            "GITHUB_TOKEN": "ghp_test"
        }
    })
}

fn jira_env() -> Json {
    json!({
        "plugin": "jira-query",
        "faults": [],
        "env": {
            "SOURCEFED_JIRA_BASE_URL": "https://example.atlassian.net",
            "ATLASSIAN_EMAIL": "spike@example.com",
            "ATLASSIAN_API_KEY": "test-key"
        }
    })
}

fn github_input(cursor: Json) -> Json {
    json!({
        "source": {
            "type": "github-issues",
            "repo": REPO,
            "labels": ["atelier:build"],
            "includeExisting": false
        },
        "cursor": cursor,
        "monitorId": "m1",
    })
}

fn jira_input(cursor: Json) -> Json {
    json!({
        "source": { "type": "jira-query", "jql": "project = PROJ1" },
        "cursor": cursor,
        "monitorId": "m1",
    })
}

/// The primed cursor's keys, so an assertion about 100 items is one line.
fn cursor_keys(cursor: &Json) -> Vec<String> {
    cursor["keys"]
        .as_array()
        .expect("cursor keys")
        .iter()
        .map(|key| key.as_str().expect("a key string").to_string())
        .collect()
}

/// One query parameter of a URL, decoded as the prelude encodes it (`+` for a space).
fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.replace('+', " ").replace("%3D", "="))
    })
}

// ---------------------------------------------------------------------------
// The djs side: one snapshot per bundle, and one step loop that answers its host calls
// ---------------------------------------------------------------------------

fn build_instance(plugin: &str, bundle: &str, config: &Json) -> Instance {
    let snapshot = Snapshot::builder()
        .host_fn(HostFn::asynchronous(HOST_HTTP, "__stallHostHttp"))
        .host_fn(HostFn::sync(HOST_LOG, "__stallLog"))
        .global("__stall_cfg", Data::from(config.to_string()))
        .source(Source::script("prelude.js", PRELUDE))
        .source(Source::script(format!("{plugin}.js"), bundle))
        .build()
        .unwrap_or_else(|error| panic!("plugin {plugin} did not build a snapshot: {error}"));
    snapshot
        .instantiate(Limits {
            deadline: Some(Duration::from_secs(30)),
            ..Limits::default()
        })
        .expect("snapshot instantiate")
}

/// The upstream a broker answers from, and every `__stallHostHttp` payload the bundle
/// sent (the payload in: `{method,url,headers}`, the broker's reply out).
struct Broker {
    requests: Vec<Json>,
    /// The status of each answer, in order, so a 304 is proven rather than assumed.
    statuses: Vec<u16>,
    upstream: Upstream,
}

enum Upstream {
    Github(GithubUpstream),
    Jira(JiraUpstream),
}

impl Broker {
    fn github() -> Broker {
        Broker {
            requests: Vec::new(),
            statuses: Vec::new(),
            upstream: Upstream::Github(GithubUpstream::new()),
        }
    }

    fn jira(project: &str) -> Broker {
        Broker {
            requests: Vec::new(),
            statuses: Vec::new(),
            upstream: Upstream::Jira(JiraUpstream::new(project)),
        }
    }

    fn add_issue(&mut self, number: u64) {
        match &mut self.upstream {
            Upstream::Github(upstream) => upstream.add_issue(number),
            Upstream::Jira(upstream) => upstream.add_issue(number),
        }
    }

    fn answer(&mut self, payload: &Json) -> Json {
        self.requests.push(payload.clone());
        let reply = match &mut self.upstream {
            Upstream::Github(upstream) => upstream.reply(payload),
            Upstream::Jira(upstream) => upstream.reply(payload),
        };
        let status = reply["response"]["status"]
            .as_u64()
            .expect("an answer status");
        self.statuses.push(status as u16);
        reply
    }
}

/// One poll through `__stallInvokePoll`, answering every host call the way the stall's
/// bridge would, and the parsed envelope it answers with.
fn poll(instance: &mut Instance, broker: &mut Broker, input: &Json) -> Json {
    let input = input.to_string();
    instance
        .start("__stallInvokePoll", &[Value::Str(&input)])
        .expect("start __stallInvokePoll");
    loop {
        match instance.step().expect("step") {
            Step::Done => {
                let value = instance.result().expect("result");
                let envelope = value.as_str().expect("the envelope string");
                return serde_json::from_str(&envelope).expect("envelope json");
            }
            Step::Yield => {}
            Step::HostCall(id) => answer_log(instance, id),
            Step::Wait(ids) => {
                assert!(!ids.is_empty(), "the poll waits for no host call");
                for id in ids {
                    answer_http(instance, broker, id);
                }
            }
        }
    }
}

fn answer_http(instance: &mut Instance, broker: &mut Broker, id: CallId) {
    let call = instance.host_call(id).expect("a pending host call");
    assert_eq!(call.fn_id(), HOST_HTTP, "the prelude's http host function");
    assert!(call.is_async(), "__stallHostHttp is asynchronous");
    let payload = call.arg(0).as_str().expect("the request payload");
    let payload: Json = serde_json::from_str(&payload).expect("request payload json");
    let reply = broker.answer(&payload).to_string();
    call.reply(Ok(Value::Str(&reply))).expect("reply");
}

fn answer_log(instance: &mut Instance, id: CallId) {
    let call = instance.host_call(id).expect("a pending host call");
    assert_eq!(call.fn_id(), HOST_LOG, "the prelude's log host function");
    call.reply(Ok(Value::Undefined)).expect("reply to the log");
}

// ---------------------------------------------------------------------------
// The mock upstream, in the harness's shapes
// ---------------------------------------------------------------------------

/// GitHub's issues list: 100 open issues plus one pull request, and an ETag over the list.
struct GithubUpstream {
    issues: Vec<Json>,
    pull_request: Json,
    etag: String,
}

impl GithubUpstream {
    fn new() -> GithubUpstream {
        GithubUpstream {
            issues: (1..=100).map(github_issue).collect(),
            pull_request: github_pull_request(),
            etag: etag(100),
        }
    }

    fn add_issue(&mut self, number: u64) {
        self.issues.push(github_issue(number));
        self.etag = etag(number);
    }

    fn listing(&self) -> Json {
        let mut items = vec![self.pull_request.clone()];
        items.extend(self.issues.iter().cloned());
        Json::Array(items)
    }

    fn reply(&mut self, payload: &Json) -> Json {
        if payload["headers"]["if-none-match"].as_str() == Some(self.etag.as_str()) {
            return json!({
                "ok": true,
                "response": { "status": 304, "headers": { "etag": self.etag }, "body": "" }
            });
        }
        json!({
            "ok": true,
            "response": {
                "status": 200,
                "headers": { "etag": self.etag },
                "body": self.listing().to_string()
            }
        })
    }
}

fn github_issue(number: u64) -> Json {
    json!({
        "number": number,
        "title": format!("repo-1 issue {number}"),
        "html_url": format!("https://github.com/{REPO}/issues/{number}"),
        "state": "open",
        "labels": [{ "name": "atelier:build" }],
        "body": "lorem ipsum dolor sit amet consectetur adipiscing elit",
        "user": { "login": "spike" },
    })
}

/// The one entry the provider must skip: pull requests share the issues list.
fn github_pull_request() -> Json {
    json!({
        "number": 999,
        "title": "repo-1 spike pull request (filtered)",
        "html_url": format!("https://github.com/{REPO}/pull/999"),
        "state": "open",
        "labels": [],
        "body": "filtered",
        "user": { "login": "spike" },
        "pull_request": {
            "url": format!("https://api.github.com/repos/{REPO}/pulls/999"),
            "html_url": format!("https://github.com/{REPO}/pull/999"),
            "merged_at": null
        }
    })
}

fn etag(top: u64) -> String {
    format!("\"e-{top}\"")
}

/// Jira's enhanced search: every issue of one project, one page.
struct JiraUpstream {
    project: String,
    issues: Vec<Json>,
}

impl JiraUpstream {
    fn new(project: &str) -> JiraUpstream {
        JiraUpstream {
            project: project.to_string(),
            issues: (1..=2).map(|number| jira_issue(project, number)).collect(),
        }
    }

    fn add_issue(&mut self, number: u64) {
        self.issues.push(jira_issue(&self.project, number));
    }

    fn reply(&self, _payload: &Json) -> Json {
        let body = json!({ "issues": self.issues, "isLast": true });
        json!({
            "ok": true,
            "response": { "status": 200, "headers": {}, "body": body.to_string() }
        })
    }
}

fn jira_issue(project: &str, number: u64) -> Json {
    json!({
        "expand": "",
        "id": number.to_string(),
        "self": format!("https://example.atlassian.net/rest/api/3/issue/{number}"),
        "key": format!("{project}-{number}"),
        "fields": { "summary": format!("{project} issue {number}") },
    })
}
