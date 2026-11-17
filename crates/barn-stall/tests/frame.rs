//! The framing: what a line is, what it is refused for, and what a read keeps
//! when it is dropped part way through one.
//!
//! Every test's work runs on a thread of its own under a five-second deadline,
//! so a read that waits for a message nobody wrote, or that spins as some
//! mutants make it, fails the test instead of holding it open.

use std::any::Any;
use std::future::Future;
use std::io;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::thread;
use std::time::Duration;

use barn_stall::frame::MAX_BLANK_LINES;
use barn_stall::{Answer, Lines, Message, Notification, Request, RpcError};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf, split};
use tokio::net::UnixStream;

/// The longest a test's work may take before the test fails.
const DEADLINE: Duration = Duration::from_secs(5);

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

/// The log notification these tests read and write.
fn log() -> Message {
    Message::Notification(Notification {
        method: "log".to_owned(),
        params: None,
    })
}

/// Runs one test's work on a thread of its own, under a five-second deadline.
///
/// The thread builds a current-thread runtime and runs the future `work()`
/// returns on it, then sends the value back. The deadline is the test thread's
/// own clock, so it fires even when the working thread never yields, which is
/// what it takes to fail a mutant that makes the read loop spin forever. The
/// spinning thread is left behind on purpose: nextest ends the process when the
/// test fails, and by then the test has failed.
///
/// A panic is not allowed outside a test function, so this returns the reason
/// the work produced nothing, and the test panics with it.
fn within<F, Fut, T>(what: &str, work: F) -> Result<T, String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = channel();

    let _ = thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = sender.send(Err(format!("the runtime does not build: {error}")));

                return;
            }
        };

        let value = catch_unwind(AssertUnwindSafe(|| runtime.block_on(work())));

        let _ = match value {
            Ok(value) => sender.send(Ok(value)),
            Err(payload) => sender.send(Err(panic_text(&*payload))),
        };
    });

    match receiver.recv_timeout(DEADLINE) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(message)) => Err(format!("{what} panicked: {message}")),
        Err(RecvTimeoutError::Timeout) => Err(format!("{what} did not finish in five seconds")),
        Err(RecvTimeoutError::Disconnected) => Err(format!("{what} ended without a result")),
    }
}

/// The text a panic carried, or a stand-in when it carried none.
fn panic_text(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
        })
        .unwrap_or_else(|| "the work panicked".to_owned())
}

#[test]
fn a_line_reads_at_its_newline() {
    within("the read at a newline", || async {
        let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
        peer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"stall.health\"}\n")
            .await
            .expect("the peer writes");

        assert_eq!(
            lines.read().await.expect("the line reads"),
            Message::Request(Request {
                id: 2,
                method: "stall.health".to_owned(),
                params: None,
            })
        );
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_blank_line_is_skipped() {
    within("the read after blank lines", || async {
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

        assert_eq!(
            lines
                .read()
                .await
                .expect("the line after the blank ones reads"),
            log()
        );
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_carriage_return_is_refused() {
    within("the read of a carriage return", || async {
        let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
        peer.write_all(LOG.as_bytes())
            .await
            .expect("the peer writes the log line");
        peer.write_all(b"\r\n")
            .await
            .expect("the peer writes a carriage return");

        assert_eq!(lines.read().await, Err(RpcError::Strict));
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_line_that_is_not_utf8_is_refused() {
    within("the read of a line that is not UTF-8", || async {
        let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
        peer.write_all(&[0xff, 0xfe, b'\n'])
            .await
            .expect("the peer writes");

        assert_eq!(lines.read().await, Err(RpcError::Strict));
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_read_after_a_carriage_return_is_poisoned() {
    within("the read after a refused carriage return", || async {
        let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
        peer.write_all(LOG.as_bytes())
            .await
            .expect("the peer writes the log line");
        peer.write_all(b"\r\n")
            .await
            .expect("the peer writes a carriage return");

        assert_eq!(lines.read().await, Err(RpcError::Strict));
        assert_eq!(lines.read().await, Err(RpcError::Poisoned));
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_read_after_a_message_that_is_not_strict_is_poisoned() {
    within("the read after a refused message", || async {
        let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
        peer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"seven\",\"method\":\"stall.health\"}\n")
            .await
            .expect("the peer writes");

        assert_eq!(lines.read().await, Err(RpcError::Strict));
        assert_eq!(lines.read().await, Err(RpcError::Poisoned));
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_line_over_the_cap_is_refused_and_poisons() {
    within("the read of a line over the cap", || async {
        let (mut lines, mut peer) = pair(8).expect("a socket pair");
        peer.write_all(b"123456789").await.expect("the peer writes");

        assert_eq!(lines.read().await, Err(RpcError::OverLimit));
        assert_eq!(lines.read().await, Err(RpcError::Poisoned));
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_line_at_the_cap_reads() {
    within("the read of a line at the cap", || async {
        let (mut lines, mut peer) = pair(LOG.len().saturating_add(1)).expect("a socket pair");
        peer.write_all(LOG.as_bytes())
            .await
            .expect("the peer writes the log line");
        peer.write_all(b"\n")
            .await
            .expect("the peer writes its newline");

        assert_eq!(
            lines.read().await.expect("the line at the cap reads"),
            log()
        );
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_line_one_past_the_cap_is_refused() {
    within("the read of a line one past the cap", || async {
        let (mut lines, mut peer) = pair(LOG.len()).expect("a socket pair");
        peer.write_all(LOG.as_bytes())
            .await
            .expect("the peer writes the log line");
        peer.write_all(b"\n")
            .await
            .expect("the peer writes its newline");

        assert_eq!(lines.read().await, Err(RpcError::OverLimit));
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn an_outgoing_line_over_the_cap_is_refused() {
    within("the write of a line over the cap", || async {
        let (mut lines, _peer) = pair(16).expect("a socket pair");

        assert_eq!(lines.write(&log()).await, Err(RpcError::OverLimit));
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn an_outgoing_line_at_the_cap_is_written() {
    within("the write of a line at the cap", || async {
        let line = serde_json::to_string(&log()).expect("the message serialises");
        let (ours, theirs) = UnixStream::pair().expect("a socket pair");
        let (our_read, our_write) = split(ours);
        let (their_read, their_write) = split(theirs);
        let mut ours = Lines::with_max_line(our_read, our_write, line.len().saturating_add(1));
        let mut theirs = Lines::with_max_line(their_read, their_write, CAP);

        ours.write(&log())
            .await
            .expect("the line at the cap writes");

        assert_eq!(theirs.read().await.expect("the line reads"), log());
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn an_outgoing_line_one_past_the_cap_is_refused() {
    within("the write of a line one past the cap", || async {
        let line = serde_json::to_string(&log()).expect("the message serialises");
        let (mut lines, mut peer) = pair(line.len()).expect("a socket pair");
        let mut buffer = [0_u8; 64];

        assert_eq!(lines.write(&log()).await, Err(RpcError::OverLimit));

        let seen = tokio::time::timeout(Duration::from_millis(50), peer.read(&mut buffer)).await;

        assert!(seen.is_err(), "the refused line never reaches the peer");
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_half_line_survives_a_read_that_stops_early() {
    within("the read that stops early", || async {
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

        assert_eq!(
            lines.read().await.expect("the finished line reads"),
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
            lines.read().await.expect("the next line reads"),
            Message::Request(Request {
                id: 3,
                method: "stall.health".to_owned(),
                params: None,
            })
        );
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_round_trip_is_bounded() {
    within("the round trip", || async {
        let (barn, stall) = UnixStream::pair().expect("a socket pair");
        let (barn_read, barn_write) = split(barn);
        let (stall_read, stall_write) = split(stall);
        let mut barn = Lines::with_max_line(barn_read, barn_write, CAP);
        let mut stall = Lines::with_max_line(stall_read, stall_write, CAP);
        let answer = Message::Answer(Answer::Result {
            id: 1,
            result: json!(true),
        });

        stall.write(&log()).await.expect("the notification writes");
        assert_eq!(barn.read().await.expect("the notification reads"), log());

        barn.write(&answer).await.expect("the answer writes");
        assert_eq!(stall.read().await.expect("the answer reads"), answer);
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_stream_that_ends_inside_a_line_is_partial() {
    within("the read at the end of a line", || async {
        let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
        peer.write_all(b"{\"jsonrpc\":\"2.0\"")
            .await
            .expect("the peer writes half a line");
        drop(peer);

        assert_eq!(lines.read().await, Err(RpcError::PartialLine));
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_stream_that_ends_between_lines_says_so() {
    within("the read at the end of the stream", || async {
        let (mut lines, peer) = pair(CAP).expect("a socket pair");
        drop(peer);

        assert_eq!(
            lines.read().await,
            Err(RpcError::Io("the stream closed".to_owned()))
        );
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_read_at_the_blank_line_limit_still_reads() {
    within("the read at the blank line limit", || async {
        let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
        let blanks = vec![b'\n'; MAX_BLANK_LINES];

        peer.write_all(&blanks)
            .await
            .expect("the peer writes the blank lines");
        peer.write_all(LOG.as_bytes())
            .await
            .expect("the peer writes the log line");
        peer.write_all(b"\n")
            .await
            .expect("the peer writes its newline");

        assert_eq!(
            lines
                .read()
                .await
                .expect("the line after the blank lines reads"),
            log()
        );
    })
    .unwrap_or_else(|message| panic!("{message}"));
}

#[test]
fn a_read_past_the_blank_line_limit_is_refused() {
    within("the read past the blank line limit", || async {
        let (mut lines, mut peer) = pair(CAP).expect("a socket pair");
        let blanks = vec![b'\n'; MAX_BLANK_LINES.saturating_add(1)];

        peer.write_all(&blanks)
            .await
            .expect("the peer writes the blank lines");

        assert_eq!(lines.read().await, Err(RpcError::Strict));
        assert_eq!(lines.read().await, Err(RpcError::Poisoned));
    })
    .unwrap_or_else(|message| panic!("{message}"));
}
