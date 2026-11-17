//! The engine seam end to end, under whichever engine the test binary was built with:
//! a bundle in a temp directory, a realm built from it, one poll answered over the host
//! bridge, and a poll stopped by the realm's budget that leaves the realm usable. In
//! process only: no sockets, no child processes, no signals.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use stall_b::engine::{self, Engine, Failure};
use stall_b::model::{self, SourceType};
use stall_b::plugin::{HostCall, NetResponse, WorkerConfig};
use tokio::sync::mpsc as async_mpsc;

/// Short on purpose: the budget test would otherwise sit for the 10 s default.
const DEADLINE: Duration = Duration::from_millis(200);

/// A bundle shaped like the real ones: the pinned providers' request function handed to
/// `__stall.http`, and a poll that can be told to run past its budget.
const BUNDLE: &str = r#"
async function request(url, headers) {
  return globalThis.__stall.http(url, headers);
}

globalThis.__stall.pluginPoll = async function (input) {
  console.log("poll " + input.monitorId);
  if (input.source.mode === "loop") { for (;;) {} }
  var response = await request(
    "https://api.github.com/repos/example-org/repo-1/issues?state=open",
    { accept: "application/vnd.github+json" }
  );
  var body = await response.json();
  return {
    cursor: { etag: response.headers.get("etag") },
    events: [{ kind: "item.matched", id: "e1", at: "t", summary: body.hello, actionable: false }],
    probe: {
      query: new URLSearchParams({ jql: "project = PROJ1", maxResults: "100" }).toString(),
      text: new TextDecoder().decode(new TextEncoder().encode("héllo")),
      btoa: btoa("hi"),
      timer: setTimeout(function () { throw new Error("timer fired"); }, 1),
      signal: AbortSignal.timeout(1).aborted
    }
  };
};
"#;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "stall-b-test-{}-{tag}-{}",
            std::process::id(),
            model::now_ms()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_realm_polls_and_recovers_under_this_engines_budget() {
    let dir = TempDir::new("engine-realm");
    fs::write(dir.path().join("github-issues.js"), BUNDLE).expect("write bundle");

    let (completions, _completions) = async_mpsc::unbounded_channel();
    let (host_calls, mut calls) = async_mpsc::unbounded_channel();
    let config = WorkerConfig {
        plugin_dir: dir.path().to_path_buf(),
        faults: Vec::new(),
        deadline: DEADLINE,
        completions,
        host_calls,
    };

    let mut realm = engine::realm(SourceType::GithubIssues, &config).expect("realm builds");

    // A poll whose http call the bridge answers, through the same prelude both engines run.
    let envelope = poll_once(&mut *realm, &mut calls, &input("ok"));
    let envelope = expect_envelope(envelope);
    assert_eq!(envelope["result"]["cursor"]["etag"], "\"v1\"");
    assert_eq!(envelope["result"]["events"][0]["summary"], "world");
    assert_eq!(
        envelope["result"]["probe"]["query"],
        "jql=project+%3D+PROJ1&maxResults=100"
    );
    assert_eq!(envelope["result"]["probe"]["text"], "héllo");
    assert_eq!(envelope["result"]["probe"]["btoa"], "aGk=");
    assert_eq!(envelope["result"]["probe"]["timer"], 1);
    assert_eq!(envelope["result"]["probe"]["signal"], false);

    // A poll that never yields is stopped by the realm's budget, and the realm is ready
    // again: the same realm answers the next poll.
    match poll_once(&mut *realm, &mut calls, &input("loop")) {
        Err(Failure::Interrupted) => {}
        Err(failure) => panic!(
            "expected the budget to interrupt the poll, got {}",
            describe(&failure)
        ),
        Ok(envelope) => panic!("expected the budget to interrupt the poll, got {envelope}"),
    }
    let envelope = expect_envelope(poll_once(&mut *realm, &mut calls, &input("ok")));
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["result"]["events"][0]["summary"], "world");
}

fn input(mode: &str) -> String {
    serde_json::json!({
        "source": { "mode": mode },
        "cursor": {},
        "monitorId": "m1",
    })
    .to_string()
}

fn expect_envelope(outcome: Result<String, Failure>) -> serde_json::Value {
    let envelope = match outcome {
        Ok(envelope) => envelope,
        Err(failure) => panic!("the poll failed: {}", describe(&failure)),
    };
    let envelope: serde_json::Value = serde_json::from_str(&envelope).expect("envelope json");
    assert_eq!(envelope["ok"], true, "envelope: {envelope}");
    envelope
}

fn describe(failure: &Failure) -> String {
    match failure {
        Failure::Interrupted => "interrupted".to_string(),
        Failure::Js(message) => format!("js: {message}"),
        Failure::Fatal(message) => format!("fatal: {message}"),
    }
}

/// One poll on its own thread, with this thread answering the host bridge exactly as the
/// tokio side does in the stall. Bounded: the engine's deadline ends a runaway poll and
/// the bridge is answered as soon as it is asked.
fn poll_once(
    realm: &mut dyn Engine,
    calls: &mut async_mpsc::UnboundedReceiver<HostCall>,
    input: &str,
) -> Result<String, Failure> {
    let (done_tx, done_rx) = mpsc::channel();
    let started = Instant::now();

    thread::scope(|scope| {
        scope.spawn(|| {
            let outcome = realm.poll(input);
            let _ = done_tx.send(outcome);
        });

        loop {
            while let Ok(call) = calls.try_recv() {
                serve(call);
            }
            match done_rx.try_recv() {
                Ok(outcome) => return outcome,
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => panic!("the poll thread died"),
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the poll did not finish"
            );
            thread::sleep(Duration::from_millis(1));
        }
    })
}

/// What the stall's broker answers a GET of the pinned GitHub issues URL.
fn serve(call: HostCall) {
    assert_eq!(call.method, "GET");
    assert!(call.plugin == "github-issues", "plugin {}", call.plugin);
    assert!(call.body.is_none(), "the shim must not send a body");
    assert_eq!(
        call.headers.get("accept").map(String::as_str),
        Some("application/vnd.github+json")
    );
    assert!(
        call.url
            .starts_with("https://api.github.com/repos/example-org/repo-1/issues"),
        "unexpected url {}",
        call.url
    );

    let response = NetResponse {
        status: 200,
        headers: BTreeMap::from([("ETag".to_string(), "\"v1\"".to_string())]),
        body: "{\"hello\":\"world\"}".to_string(),
    };
    call.reply.send(Ok(response)).expect("host reply");
}
