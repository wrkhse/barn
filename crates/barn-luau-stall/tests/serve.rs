//! Fake-peer lifecycle tests: a stall answers over a socket pair, with no barn
//! process, no real listener and no spawned binary.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use barn_luau::{Capabilities, Limits, Sandbox};
use barn_luau_stall::bridge::real_clock;
use barn_luau_stall::serve::{ServeError, serve};
use barn_stall::{MAX_LINE_BYTES, Reply, protocol::DRAIN_DEADLINE_MS};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use tokio::time::Duration;

const FRAMES: &str = include_str!("../../barn-stall/fixtures/frames.jsonl");
const MEMORY_BYTES: usize = 16 * 1024 * 1024;
const CALL_MS: u64 = 1000;
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
    let folder = PluginFolder::new()?;
    folder.write("plugin.luau", PLUGIN)?;
    let limits = Limits::new(MEMORY_BYTES, CALL_MS)?;
    let sandbox = Sandbox::new(folder.path(), limits, Capabilities::default(), real_clock())?;
    let (client, stall) = UnixStream::pair()?;

    Ok(Peer {
        wire: BufReader::new(client),
        task: tokio::spawn(serve(stall, sandbox)),
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

const fn request_id(reply: &Reply) -> u64 {
    match reply {
        Reply::Hook { id, .. } | Reply::Tool { id, .. } | Reply::Error { id, .. } => *id,
    }
}

#[tokio::test]
async fn every_fixture_pair_travels_through_serve() {
    let frames = wires().expect("fixture frames");
    assert_eq!(frames.len(), 18);
    let mut peer = peer().expect("a peer");

    for pair in frames.as_chunks::<2>().0 {
        // Pair 7 belongs to the fail-closed slice: it is the tack-side named
        // deny for a dead stall, which the Luau engine cannot produce.
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
    let mut peer = peer().expect("a peer");
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
    assert_eq!(
        peer.json().await.expect("a drain reply"),
        json!({"jsonrpc": "2.0", "id": 3, "result": null})
    );

    let reply = peer.reply().await.expect("the call's reply");
    if let Reply::Error { id, error, .. } = &reply
        && error == "stall_draining"
    {
        assert_eq!(
            reply,
            Reply::Error {
                id: *id,
                error: "stall_draining".into(),
                message: "in-flight call did not finish before the drain deadline".into(),
            }
        );
    }
    assert_eq!(request_id(&reply), 99, "the original call is answered");

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

    let outcome = finish(peer.task).await.expect("serve stops");
    assert!(matches!(outcome, Err(ServeError::Oversize)));
}

#[tokio::test]
async fn a_malformed_line_is_named() {
    let mut peer = peer().expect("a peer");
    peer.send("{\"type\":\"unknown\"}")
        .await
        .expect("send an unknown frame");

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
