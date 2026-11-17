//! The framing: what a line is, what it is refused for, and what a read keeps
//! when it is dropped part way through one.
//!
//! Every read sits inside a five-second bound, so a read that waits for a
//! message nobody wrote fails the test instead of holding it open.

use std::future::Future;
use std::io;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::time::Duration;

use barn_stall::{Answer, Lines, Message, Notification, Request, RpcError};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf, split};
use tokio::net::UnixStream;

/// The longest a read in these tests may wait.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The cap most of these tests read under.
const CAP: usize = 1_024;

/// The line these tests send back and forth.
const LOG: &str = "{\"jsonrpc\":\"2.0\",\"method\":\"log\"}";

/// The lines under test, as a socket end split into its two halves.
type UnderTest = Lines<ReadHalf<UnixStream>, WriteHalf<UnixStream>>;

/// A connected pair: the lines under test, and the peer that talks to them.
fn pair(max_line: usize) -> io::Result<(UnderTest, UnixStream)> {
    let (ours, peer) = UnixStream::pair()?;
    let (read, write) = split(ours);

    Ok((Lines::with_max_line(read, write, max_line), peer))
}

/// Runs one read inside the test's bound, so a read that waits for a message
/// nobody wrote fails instead of hanging.
async fn bounded<T>(read: impl Future<Output = T>) -> Option<T> {
    tokio::time::timeout(TIMEOUT, read).await.ok()
}

#[tokio::test]
async fn a_line_reads_at_its_newline() {
    let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
    peer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"stall.health\"}\n")
        .await
        .expect("the peer writes");

    let message = bounded(lines.read())
        .await
        .expect("the read returns inside five seconds")
        .expect("the line reads");

    assert_eq!(
        message,
        Message::Request(Request {
            id: 2,
            method: "stall.health".to_owned(),
            params: None,
        })
    );
}

#[tokio::test]
async fn a_blank_line_is_skipped() {
    let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
    peer.write_all(b"\n\n")
        .await
        .expect("the peer writes two blank lines");
    peer.write_all(LOG.as_bytes())
        .await
        .expect("the peer writes the log line");
    peer.write_all(b"\n")
        .await
        .expect("the peer writes its newline");

    let message = bounded(lines.read())
        .await
        .expect("the read returns inside five seconds")
        .expect("the line after the blank ones reads");

    assert_eq!(
        message,
        Message::Notification(Notification {
            method: "log".to_owned(),
            params: None,
        })
    );
}

#[tokio::test]
async fn a_carriage_return_is_refused() {
    let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
    peer.write_all(LOG.as_bytes())
        .await
        .expect("the peer writes the log line");
    peer.write_all(b"\r\n")
        .await
        .expect("the peer writes a carriage return");

    assert_eq!(
        bounded(lines.read())
            .await
            .expect("the read returns inside five seconds"),
        Err(RpcError::Strict)
    );
}

#[tokio::test]
async fn a_line_that_is_not_utf8_is_refused() {
    let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
    peer.write_all(&[0xff, 0xfe, b'\n'])
        .await
        .expect("the peer writes");

    assert_eq!(
        bounded(lines.read())
            .await
            .expect("the read returns inside five seconds"),
        Err(RpcError::Strict)
    );
}

#[tokio::test]
async fn a_read_after_a_carriage_return_is_poisoned() {
    let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
    peer.write_all(LOG.as_bytes())
        .await
        .expect("the peer writes the log line");
    peer.write_all(b"\r\n")
        .await
        .expect("the peer writes a carriage return");

    assert_eq!(
        bounded(lines.read())
            .await
            .expect("the read returns inside five seconds"),
        Err(RpcError::Strict)
    );
    assert_eq!(
        bounded(lines.read())
            .await
            .expect("the poisoned read returns at once"),
        Err(RpcError::Poisoned)
    );
}

#[tokio::test]
async fn a_read_after_a_message_that_is_not_strict_is_poisoned() {
    let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
    peer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"seven\",\"method\":\"stall.health\"}\n")
        .await
        .expect("the peer writes");

    assert_eq!(
        bounded(lines.read())
            .await
            .expect("the read returns inside five seconds"),
        Err(RpcError::Strict)
    );
    assert_eq!(
        bounded(lines.read())
            .await
            .expect("the poisoned read returns at once"),
        Err(RpcError::Poisoned)
    );
}

#[tokio::test]
async fn a_line_over_the_cap_is_refused_and_poisons() {
    let (mut lines, mut peer) = pair(8).expect("a socket pair");
    peer.write_all(b"123456789").await.expect("the peer writes");

    assert_eq!(
        bounded(lines.read())
            .await
            .expect("the read returns inside five seconds"),
        Err(RpcError::OverLimit)
    );
    assert_eq!(
        bounded(lines.read())
            .await
            .expect("the poisoned read returns at once"),
        Err(RpcError::Poisoned)
    );
}

#[tokio::test]
async fn a_line_at_the_cap_reads() {
    let (mut lines, mut peer) = pair(LOG.len().saturating_add(1)).expect("a socket pair");
    peer.write_all(LOG.as_bytes())
        .await
        .expect("the peer writes the log line");
    peer.write_all(b"\n")
        .await
        .expect("the peer writes its newline");

    let message = bounded(lines.read())
        .await
        .expect("the read returns inside five seconds")
        .expect("the line at the cap reads");

    assert_eq!(
        message,
        Message::Notification(Notification {
            method: "log".to_owned(),
            params: None,
        })
    );
}

#[tokio::test]
async fn a_line_one_past_the_cap_is_refused() {
    let (mut lines, mut peer) = pair(LOG.len()).expect("a socket pair");
    peer.write_all(LOG.as_bytes())
        .await
        .expect("the peer writes the log line");
    peer.write_all(b"\n")
        .await
        .expect("the peer writes its newline");

    assert_eq!(
        bounded(lines.read())
            .await
            .expect("the read returns inside five seconds"),
        Err(RpcError::OverLimit)
    );
}

#[tokio::test]
async fn an_outgoing_line_over_the_cap_is_refused() {
    let (mut lines, _peer) = pair(16).expect("a socket pair");
    let log = Message::Notification(Notification {
        method: "log".to_owned(),
        params: None,
    });

    assert_eq!(lines.write(&log).await, Err(RpcError::OverLimit));
}

#[tokio::test]
async fn an_outgoing_line_at_the_cap_is_written() {
    let log = Message::Notification(Notification {
        method: "log".to_owned(),
        params: None,
    });
    let line = serde_json::to_string(&log).expect("the message serialises");
    let (ours, theirs) = UnixStream::pair().expect("a socket pair");
    let (our_read, our_write) = split(ours);
    let (their_read, their_write) = split(theirs);
    let mut ours = Lines::with_max_line(our_read, our_write, line.len().saturating_add(1));
    let mut theirs = Lines::with_max_line(their_read, their_write, CAP);

    ours.write(&log).await.expect("the line at the cap writes");

    assert_eq!(
        bounded(theirs.read())
            .await
            .expect("the read returns inside five seconds")
            .expect("the line reads"),
        log
    );
}

#[tokio::test]
async fn an_outgoing_line_one_past_the_cap_is_refused() {
    let log = Message::Notification(Notification {
        method: "log".to_owned(),
        params: None,
    });
    let line = serde_json::to_string(&log).expect("the message serialises");
    let (mut lines, mut peer) = pair(line.len()).expect("a socket pair");
    let mut buffer = [0_u8; 64];

    assert_eq!(lines.write(&log).await, Err(RpcError::OverLimit));

    let seen = tokio::time::timeout(Duration::from_millis(50), peer.read(&mut buffer)).await;

    assert!(seen.is_err(), "the refused line never reaches the peer");
}

#[tokio::test]
async fn a_half_line_survives_a_read_that_stops_early() {
    let (ours, peer) = StdUnixStream::pair().expect("a socket pair");
    ours.set_nonblocking(true)
        .expect("the socket turns non-blocking");
    peer.set_nonblocking(true)
        .expect("the socket turns non-blocking");
    let other = peer.try_clone().expect("the peer clones");
    let (read, write) = split(UnixStream::from_std(ours).expect("a socket as tokio reads it"));
    let mut peer = UnixStream::from_std(peer).expect("a socket as tokio writes it");
    let mut other = UnixStream::from_std(other).expect("a socket as tokio writes it");
    let mut lines = Lines::with_max_line(read, write, CAP);

    peer.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"params\":{\"level\":\"info\",")
        .await
        .expect("the peer writes half a line");

    let stopped = tokio::time::timeout(Duration::from_millis(50), lines.read()).await;

    assert!(
        stopped.is_err(),
        "the read is dropped at the deadline with half a line in hand"
    );

    other
        .write_all(b"\"message\":\"hi\"}}\n")
        .await
        .expect("the other handle finishes the line");
    other
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"stall.health\"}\n")
        .await
        .expect("the other handle writes another line");

    let first = bounded(lines.read())
        .await
        .expect("the read returns inside five seconds")
        .expect("the finished line reads");
    let second = bounded(lines.read())
        .await
        .expect("the read returns inside five seconds")
        .expect("the next line reads");

    assert_eq!(
        first,
        Message::Notification(Notification {
            method: "log".to_owned(),
            params: Some(
                json!({"level": "info", "message": "hi"})
                    .as_object()
                    .cloned()
                    .expect("the test's parameters are an object"),
            ),
        })
    );
    assert_eq!(
        second,
        Message::Request(Request {
            id: 3,
            method: "stall.health".to_owned(),
            params: None,
        })
    );
}

#[tokio::test]
async fn a_round_trip_is_bounded() {
    let (barn, stall) = UnixStream::pair().expect("a socket pair");
    let (barn_read, barn_write) = split(barn);
    let (stall_read, stall_write) = split(stall);
    let mut barn = Lines::with_max_line(barn_read, barn_write, CAP);
    let mut stall = Lines::with_max_line(stall_read, stall_write, CAP);

    let log = Message::Notification(Notification {
        method: "log".to_owned(),
        params: None,
    });
    let answer = Message::Answer(Answer::Result {
        id: 1,
        result: json!(true),
    });

    stall.write(&log).await.expect("the notification writes");
    assert_eq!(
        bounded(barn.read())
            .await
            .expect("the read returns inside five seconds")
            .expect("the notification reads"),
        log
    );

    barn.write(&answer).await.expect("the answer writes");
    assert_eq!(
        bounded(stall.read())
            .await
            .expect("the read returns inside five seconds")
            .expect("the answer reads"),
        answer
    );
}

#[tokio::test]
async fn a_stream_that_ends_inside_a_line_is_partial() {
    let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
    peer.write_all(b"{\"jsonrpc\":\"2.0\"")
        .await
        .expect("the peer writes half a line");
    drop(peer);

    assert_eq!(
        bounded(lines.read())
            .await
            .expect("the read returns inside five seconds"),
        Err(RpcError::PartialLine)
    );
}

#[tokio::test]
async fn a_stream_that_ends_between_lines_says_so() {
    let (mut lines, peer) = pair(CAP).expect("a socket pair");
    drop(peer);

    assert_eq!(
        bounded(lines.read())
            .await
            .expect("the read returns inside five seconds"),
        Err(RpcError::Io("the stream closed".to_owned()))
    );
}
