//! Fake-peer lifecycle tests: a stall answers over a socket pair, with no barn
//! process, no real listener and no spawned binary.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use barn_luau::{Capabilities, Limits, Sandbox};
use barn_luau_stall::bridge::real_clock;
use barn_luau_stall::capabilities_for;
use barn_luau_stall::control::ControlBridge;
use barn_luau_stall::serve::{MAX_IN_FLIGHT, ServeError, serve};
use barn_stall::protocol::deny::{
    BAD_FRAME, OVERSIZE_FRAME, OVERSIZE_REPLY, STALL_BUSY, STALL_DRAINING,
};
use barn_stall::{DENIED_CODE, MAX_LINE_BYTES, Outcome, Reply, protocol::DRAIN_DEADLINE_MS};
use serde_json::{Value, json};
use tack_protocol::wire::{PROTOCOL_VERSION, Reply as TackReply, ReplyError as TackReplyError};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use tokio::time::Duration;

const FRAMES: &str = include_str!("../../barn-stall/fixtures/frames.jsonl");
const MEMORY_BYTES: usize = 16 * 1024 * 1024;
const CALL_MS: u64 = 1000;
/// Above the 500 ms drain deadline, so the engine cannot time out first.
const DRAIN_CALL_MS: u64 = 2000;
/// Short enough that an unbounded loop reaches the engine's deadline limit.
const LIMIT_CALL_MS: u64 = 100;
const WAIT: Duration = Duration::from_secs(2);

type BoxError = Box<dyn std::error::Error>;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The plugin answers every framed request in the shared fixture with the
/// fixture reply. The engine sees the call's input as the hook payload and
/// its name as a runnable name.
const PLUGIN: &str = r#"
local content = "buy milk"

return {
    name = "fixture",
    PreToolUse = function(payload, ctx)
        local command = payload.command
        if command == "git push" then
            return ctx.deny("push blocked")
        end
        if command == "echo hi" then
            return ctx.drop()
        end
        if command == "rm -rf /tmp/x" then
            error("the stall did not answer")
        end
        if command == "echo loop" then
            while true do end
        end
        return nil
    end,
    PostToolUse = function(payload, ctx)
        if payload.title == "Fix flaky test" then
            return ctx.rewrite({
                title = "Fix flaky test",
                labels = { "bug", "p1" },
            })
        end
        if payload.text == "hi" then
            return ctx.annotate({ chauffeurTriaged = true })
        end
        return nil
    end,
    tools = {
        read = { name = "read", run = function() return content end },
        notes = { name = "notes", run = function() return content end },
        ["notes.read"] = {
            name = "notes.read",
            run = function() return content end,
        },
    },
}
"#;

/// A tool whose result fills the engine's output budget, making the framed
/// reply longer than one line can carry. The engine charges the two quotes
/// around the string, so the result stops two bytes short of the budget.
const BIG_PLUGIN: &str = r#"
return {
    name = "oversize",
    tools = {
        big = {
            name = "big",
            run = function()
                return string.rep("x", 8 * 1024 * 1024 - 2)
            end,
        },
    },
}
"#;

/// A plugin that declares one control need and uses the capability by a
/// computed context key.
const CONTROL_NEED_PLUGIN: &str = r#"
return {
    name = "control-need",
    needs = { "control.call.navigate" },
    PreToolUse = function(payload, ctx)
        return ctx.rewrite(ctx['con' .. 'trol'].call('navigate', { who = 'x' }))
    end,
    tools = {},
}
"#;

/// The same call from a plugin that declared no control need.
const CONTROL_NO_NEED_PLUGIN: &str = r#"
return {
    name = "control-no-need",
    PreToolUse = function(payload, ctx)
        return ctx.rewrite(ctx['con' .. 'trol'].call('navigate', { who = 'x' }))
    end,
    tools = {},
}
"#;

fn control_hook(id: u64) -> String {
    json!({
        "type": "hook",
        "id": id,
        "version": 1,
        "hook": "PreToolUse",
        "call": { "tool": "bash", "name": "run", "input": {} },
    })
    .to_string()
}

struct PluginFolder(PathBuf);

impl PluginFolder {
    fn new() -> Result<Self, io::Error> {
        let root = std::env::temp_dir().join(format!(
            "barn-luau-stall-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root)?;
        Ok(Self(root))
    }

    fn write(&self, name: &str, body: &str) -> Result<(), io::Error> {
        std::fs::write(self.0.join(name), body)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PluginFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Peer {
    wire: BufReader<UnixStream>,
    task: JoinHandle<Result<(), ServeError>>,
    _folder: PluginFolder,
}

fn peer() -> Result<Peer, BoxError> {
    peer_with(PLUGIN, CALL_MS)
}

fn peer_with(plugin: &str, call_ms: u64) -> Result<Peer, BoxError> {
    let folder = PluginFolder::new()?;
    folder.write("plugin.luau", plugin)?;
    let limits = Limits::new(MEMORY_BYTES, call_ms)?;
    let (bridge, control_rx) = ControlBridge::new();

    // The real stall reads the manifest once, then builds capabilities from
    // its declared needs. Do the same here so conformance tests exercise the
    // registration path rather than a fixture-only copy of it.
    let probe = Sandbox::new(folder.path(), limits, Capabilities::default(), real_clock())?;
    let manifest = probe.manifest()?;
    let needs = manifest
        .get("needs")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "manifest has no needs array"))?;
    let capabilities = capabilities_for(needs, bridge)?;
    let sandbox = Sandbox::new(folder.path(), limits, capabilities, real_clock())?;
    let (client, stall) = UnixStream::pair()?;

    Ok(Peer {
        wire: BufReader::new(client),
        task: tokio::spawn(serve(stall, sandbox, control_rx)),
        _folder: folder,
    })
}

impl Peer {
    async fn send(&mut self, frame: &str) -> Result<(), io::Error> {
        self.wire.write_all(format!("{frame}\n").as_bytes()).await
    }

    async fn read(&mut self) -> Result<String, BoxError> {
        let mut line = String::new();
        let _ = self.wire.read_line(&mut line).await?;

        if !line.ends_with('\n') {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "no reply line").into());
        }
        Ok(line)
    }

    async fn reply(&mut self) -> Result<Reply, BoxError> {
        Ok(serde_json::from_str(&self.read().await?)?)
    }

    async fn json(&mut self) -> Result<Value, BoxError> {
        Ok(serde_json::from_str(&self.read().await?)?)
    }
}

async fn finish(
    task: JoinHandle<Result<(), ServeError>>,
) -> Result<Result<(), ServeError>, BoxError> {
    Ok(tokio::time::timeout(WAIT, task).await??)
}

fn wires() -> Result<Vec<Value>, serde_json::Error> {
    FRAMES.lines().map(serde_json::from_str).collect()
}

#[tokio::test]
async fn every_fixture_pair_travels_through_serve() {
    let frames = wires().expect("fixture frames");
    assert_eq!(frames.len(), 18);
    let mut peer = peer().expect("a peer");

    for pair in frames.as_chunks::<2>().0 {
        // Pair 7 is the `stall_unavailable` deny for a worker that failed
        // before answering. `handle_request` returns a Reply for every engine
        // outcome and never panics, so this fake peer cannot lose a worker the
        // way barn does when a stall process dies.
        if pair[0]["id"] == 7 {
            continue;
        }

        let frame = serde_json::to_string(&pair[0]).expect("a request line");
        peer.send(&frame).await.expect("send one request");
        let expected: Reply = serde_json::from_value(pair[1].clone()).expect("the fixture reply");

        assert_eq!(
            peer.reply().await.expect("a reply"),
            expected,
            "request {frame}"
        );
    }

    drop(peer.wire);
    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Closed)));
}

#[tokio::test]
async fn health_replies_with_the_matching_id() {
    let mut peer = peer().expect("a peer");

    for id in [2, 21] {
        peer.send(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"stall.health\"}}"
        ))
        .await
        .expect("send health");

        assert_eq!(
            peer.json().await.expect("a health reply"),
            json!({"jsonrpc": "2.0", "id": id, "result": null})
        );
    }
}

#[tokio::test]
async fn drain_replies_null_and_stops_within_the_deadline() {
    let mut peer = peer().expect("a peer");
    peer.send("{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"stall.drain\"}")
        .await
        .expect("send drain");

    assert_eq!(
        peer.json().await.expect("a drain reply"),
        json!({"jsonrpc": "2.0", "id": 3, "result": null})
    );
    let outcome = tokio::time::timeout(Duration::from_millis(DRAIN_DEADLINE_MS), finish(peer.task))
        .await
        .expect("serve stops within the drain deadline")
        .expect("the serve task joins");
    assert!(outcome.is_ok(), "drain succeeds: {outcome:?}");
}

#[tokio::test]
async fn a_call_at_drain_finishes_or_gets_a_named_deny() {
    let mut peer = peer_with(PLUGIN, DRAIN_CALL_MS).expect("a peer");
    let request = json!({
        "type": "hook",
        "id": 99,
        "version": 1,
        "hook": "PreToolUse",
        "call": {
            "tool": "bash",
            "name": "run",
            "input": {"command": "echo loop"},
        },
    });
    peer.send(&request.to_string())
        .await
        .expect("send the call");
    peer.send("{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"stall.drain\"}")
        .await
        .expect("send drain");

    let reply = peer.reply().await.expect("the call's reply");
    assert_eq!(
        reply,
        Reply::Error {
            id: 99,
            error: STALL_DRAINING.into(),
            message: "in-flight call did not finish before the drain deadline".into(),
        }
    );
    assert_eq!(
        peer.json().await.expect("a drain reply"),
        json!({"jsonrpc": "2.0", "id": 3, "result": null})
    );

    let outcome = tokio::time::timeout(
        Duration::from_millis(DRAIN_DEADLINE_MS.saturating_mul(2)),
        finish(peer.task),
    )
    .await
    .expect("serve stops after the in-flight call")
    .expect("the serve task joins");
    assert!(outcome.is_ok(), "drain succeeds: {outcome:?}");
}

#[tokio::test]
async fn an_oversize_line_is_named() {
    let mut peer = peer().expect("a peer");
    peer.wire
        .write_all(&vec![b'x'; MAX_LINE_BYTES])
        .await
        .expect("write an unbounded line");

    let error = peer.json().await.expect("an oversize error line");
    assert_eq!(
        error,
        json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {
                "code": DENIED_CODE,
                "message": "denied",
                "data": {"reason": OVERSIZE_FRAME},
            },
        })
    );

    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Oversize)));
}

#[tokio::test]
async fn a_malformed_line_is_named() {
    let mut peer = peer().expect("a peer");
    peer.send("{\"type\":\"unknown\"}")
        .await
        .expect("send an unknown frame");

    let error = peer.json().await.expect("a malformed error line");
    assert_eq!(
        error,
        json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {
                "code": DENIED_CODE,
                "message": "denied",
                "data": {"reason": BAD_FRAME},
            },
        })
    );

    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Malformed(_))));
}

#[tokio::test]
async fn a_closed_peer_is_named() {
    let peer = peer().expect("a peer");
    drop(peer.wire);

    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Closed)));
}

#[tokio::test]
async fn a_call_over_its_deadline_is_named() {
    let mut peer = peer_with(PLUGIN, LIMIT_CALL_MS).expect("a peer");
    peer.send(
        &json!({
            "type": "hook",
            "id": 50,
            "version": 1,
            "hook": "PreToolUse",
            "call": {"tool": "bash", "name": "run", "input": {"command": "echo loop"}},
        })
        .to_string(),
    )
    .await
    .expect("send the call");

    assert_eq!(
        peer.reply().await.expect("a reply"),
        Reply::Error {
            id: 50,
            error: "limit".into(),
            message: "script deadline exceeded".into(),
        }
    );

    drop(peer.wire);
    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Closed)));
}

#[tokio::test]
async fn a_peer_closing_mid_line_is_named() {
    let mut peer = peer().expect("a peer");
    peer.wire
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,")
        .await
        .expect("write a partial line");

    drop(peer.wire);
    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Closed)));
}

#[tokio::test]
async fn health_is_answered_while_a_call_is_in_flight() {
    let mut peer = peer().expect("a peer");
    peer.send(
        &json!({
            "type": "hook",
            "id": 40,
            "version": 1,
            "hook": "PreToolUse",
            "call": {"tool": "bash", "name": "run", "input": {"command": "echo loop"}},
        })
        .to_string(),
    )
    .await
    .expect("send the long call");

    peer.wire
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":41,")
        .await
        .expect("write the first half of health");
    peer.wire
        .write_all(b"\"method\":\"stall.health\"}\n")
        .await
        .expect("write the rest of health");

    let health = tokio::time::timeout(Duration::from_secs(1), peer.json())
        .await
        .expect("health is answered while the call runs")
        .expect("a health reply");
    assert_eq!(health, json!({"jsonrpc": "2.0", "id": 41, "result": null}));

    drop(peer.wire);
    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Closed)));
}

#[tokio::test]
async fn a_call_past_the_in_flight_cap_is_busy() {
    let mut peer = peer().expect("a peer");
    let busy_id = u64::try_from(MAX_IN_FLIGHT).expect("the cap fits an id") + 1;

    for id in 1..=busy_id {
        let frame = json!({
            "type": "hook",
            "id": id,
            "version": 1,
            "hook": "PreToolUse",
            "call": {"tool": "bash", "name": "run", "input": {"command": "echo loop"}},
        });
        peer.send(&frame.to_string()).await.expect("send a call");
    }

    assert_eq!(
        peer.reply().await.expect("a reply"),
        Reply::Error {
            id: busy_id,
            error: STALL_BUSY.into(),
            message: "too many in-flight calls".into(),
        }
    );

    drop(peer.wire);
    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Closed)));
}

#[tokio::test]
async fn an_oversized_reply_is_denied_and_serving_continues() {
    let mut peer = peer_with(BIG_PLUGIN, CALL_MS).expect("a peer");
    peer.send(
        &json!({
            "type": "tool",
            "id": 60,
            "version": 1,
            "call": {"tool": "notes", "name": "big", "input": {}},
        })
        .to_string(),
    )
    .await
    .expect("send the tool call");

    assert_eq!(
        peer.reply().await.expect("a reply"),
        Reply::Error {
            id: 60,
            error: OVERSIZE_REPLY.into(),
            message: "the reply exceeds MAX_LINE_BYTES".into(),
        }
    );

    peer.send("{\"jsonrpc\":\"2.0\",\"id\":61,\"method\":\"stall.health\"}")
        .await
        .expect("send health after the deny");
    assert_eq!(
        peer.json().await.expect("a health reply"),
        json!({"jsonrpc": "2.0", "id": 61, "result": null})
    );

    drop(peer.wire);
    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Closed)));
}

#[tokio::test]
async fn a_declared_control_need_returns_the_exact_tack_result() {
    let mut peer = peer_with(CONTROL_NEED_PLUGIN, CALL_MS).expect("a peer");
    peer.send(&control_hook(70)).await.expect("send the hook");

    let request: Value = serde_json::from_str(&peer.read().await.expect("a control request line"))
        .expect("a JSON frame");
    assert_eq!(
        request,
        json!({
            "kind": "control",
            "id": 0,
            "version": PROTOCOL_VERSION,
            "call": "navigate",
            "input": { "who": "x" },
        })
    );

    let reply = TackReply::Control {
        id: 0,
        version: PROTOCOL_VERSION,
        result: json!({ "url": "https://example.test/x" }),
    };
    peer.send(&serde_json::to_string(&reply).expect("a control reply"))
        .await
        .expect("send the control reply");

    assert_eq!(
        peer.reply().await.expect("the verdict"),
        Reply::Hook {
            id: 70,
            outcome: Outcome::Rewrite {
                input: json!({ "url": "https://example.test/x" }),
            },
        }
    );

    drop(peer.wire);
    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Closed)));
}

#[tokio::test]
async fn an_undeclared_control_need_is_named_without_a_frame() {
    let mut peer = peer_with(CONTROL_NO_NEED_PLUGIN, CALL_MS).expect("a peer");
    peer.send(&control_hook(71)).await.expect("send the hook");

    assert_eq!(
        peer.reply().await.expect("the verdict"),
        Reply::Error {
            id: 71,
            error: "capability".into(),
            message: "capability control.call: no declared control need for navigate".into(),
        }
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), peer.read())
            .await
            .is_err(),
        "an undeclared need must not write a control frame"
    );

    drop(peer.wire);
    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Closed)));
}

#[tokio::test]
async fn a_named_tack_control_error_becomes_the_hook_error() {
    let mut peer = peer_with(CONTROL_NEED_PLUGIN, CALL_MS).expect("a peer");
    peer.send(&control_hook(72)).await.expect("send the hook");

    let request: Value = serde_json::from_str(&peer.read().await.expect("a control request line"))
        .expect("a JSON frame");
    assert_eq!(
        request,
        json!({
            "kind": "control",
            "id": 0,
            "version": PROTOCOL_VERSION,
            "call": "navigate",
            "input": { "who": "x" },
        })
    );

    let reply = TackReply::Error {
        id: 0,
        version: PROTOCOL_VERSION,
        error: TackReplyError::DaemonError,
        message: "navigate refused".to_owned(),
    };
    peer.send(&serde_json::to_string(&reply).expect("a control error"))
        .await
        .expect("send the control error");

    assert_eq!(
        peer.reply().await.expect("the verdict"),
        Reply::Error {
            id: 72,
            error: "capability".into(),
            message: "capability control.call: daemon_error: navigate refused".into(),
        }
    );

    drop(peer.wire);
    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Closed)));
}
