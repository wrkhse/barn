//! The stall side of the `stall.start` handshake.
//!
//! A stall connects to the private socket in `BARN_SOCKET`, sends one bounded
//! `stall.start` line carrying the single-use `BARN_TOKEN`, and reads one
//! bounded reply. The caller gets the authenticated connection back, ready for
//! the frames barn sends next. Every read and write is capped at
//! [`MAX_LINE_BYTES`], so a hostile or broken peer cannot grow the line.

use std::collections::BTreeMap;
use std::fmt::{self, Formatter};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::protocol::{
    Job, MAX_LINE_BYTES, PROTOCOL_VERSION, StallStartParams, StallStartResult, method,
};

/// The id of the stall's one `stall.start` request.
const START_ID: u64 = 1;

/// How long a stall waits for the bounded `stall.start` reply line.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a fallback read waits for bytes before rechecking its deadline.
const RETRY_INTERVAL: Duration = Duration::from_millis(1);

/// Why a stall could not complete its `stall.start` handshake.
#[derive(Debug)]
pub enum ClientError {
    /// `BARN_SOCKET` is absent from the stall's environment.
    MissingSocket,
    /// `BARN_TOKEN` is absent from the stall's environment.
    MissingToken,
    /// The private socket could not be opened.
    Connect(io::Error),
    /// The request line could not be written.
    Write(io::Error),
    /// The reply line could not be read.
    Read(io::Error),
    /// A socket read deadline could not be set or cleared.
    SetTimeout {
        step: &'static str,
        source: io::Error,
    },
    /// The bounded reply did not arrive before its deadline.
    Timeout,
    /// The socket closed before a newline-terminated reply arrived.
    Eof,
    /// A line reached [`MAX_LINE_BYTES`] without a newline.
    Oversize,
    /// A line was not JSON, or its result was not a `StallStartResult`.
    Malformed(serde_json::Error),
    /// barn answered with a JSON-RPC error instead of a result.
    ErrorReply(Value),
    /// The reply carried an id other than the request's.
    IdMismatch,
    /// The reply used a different JSON-RPC or stall protocol version.
    VersionMismatch,
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSocket => formatter.write_str("BARN_SOCKET is not set"),
            Self::MissingToken => formatter.write_str("BARN_TOKEN is not set"),
            Self::Connect(error) => write!(formatter, "connecting to barn: {error}"),
            Self::Write(error) => write!(formatter, "writing stall start: {error}"),
            Self::Read(error) => write!(formatter, "reading the start reply: {error}"),
            Self::SetTimeout { step, source } => {
                write!(formatter, "setting {step} timeout: {source}")
            }
            Self::Timeout => formatter.write_str("the start reply timed out"),
            Self::Eof => formatter.write_str("the start reply closed before its newline"),
            Self::Oversize => formatter.write_str("a start line exceeded MAX_LINE_BYTES"),
            Self::Malformed(error) => write!(formatter, "a start line is malformed: {error}"),
            Self::ErrorReply(error) => write!(formatter, "barn refused stall start: {error}"),
            Self::IdMismatch => formatter.write_str("the start reply carried another id"),
            Self::VersionMismatch => {
                formatter.write_str("the start reply used another protocol version")
            }
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(error) | Self::Write(error) | Self::Read(error) => Some(error),
            Self::SetTimeout { source, .. } => Some(source),
            Self::Malformed(error) => Some(error),
            _ => None,
        }
    }
}

/// Connects to `socket` and completes the `stall.start` handshake.
///
/// # Errors
///
/// Returns a [`ClientError`] when the socket cannot be opened, a line cannot
/// be written or read within [`MAX_LINE_BYTES`], or barn answers with an
/// error, a mismatched id or a different protocol version.
pub fn connect(
    socket: &Path,
    token: &str,
    jobs: BTreeMap<String, Job>,
) -> Result<(BufReader<UnixStream>, StallStartResult), ClientError> {
    connect_with_timeout(socket, token, jobs, REPLY_TIMEOUT)
}

/// Connects to `socket` and runs the handshake with a bounded reply deadline.
fn connect_with_timeout(
    socket: &Path,
    token: &str,
    jobs: BTreeMap<String, Job>,
    timeout: Duration,
) -> Result<(BufReader<UnixStream>, StallStartResult), ClientError> {
    let stream = UnixStream::connect(socket).map_err(ClientError::Connect)?;
    handshake(stream, token, jobs, timeout)
}

/// Connects using the `BARN_SOCKET` and `BARN_TOKEN` in the environment.
///
/// # Errors
///
/// Returns [`ClientError::MissingSocket`] or [`ClientError::MissingToken`]
/// when a variable is absent, and any [`connect`] error otherwise.
pub fn from_env(
    jobs: BTreeMap<String, Job>,
) -> Result<(BufReader<UnixStream>, StallStartResult), ClientError> {
    let socket = std::env::var_os("BARN_SOCKET")
        .map(PathBuf::from)
        .ok_or(ClientError::MissingSocket)?;
    let token = std::env::var("BARN_TOKEN").map_err(|_| ClientError::MissingToken)?;

    connect(&socket, &token, jobs)
}

/// Runs the one-line exchange on an already connected stream.
fn handshake(
    mut stream: UnixStream,
    token: &str,
    jobs: BTreeMap<String, Job>,
    timeout: Duration,
) -> Result<(BufReader<UnixStream>, StallStartResult), ClientError> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": START_ID,
        "method": method::STALL_START,
        "params": StallStartParams {
            protocol: PROTOCOL_VERSION,
            token: token.to_owned(),
            jobs,
        },
    });

    write_line(&mut stream, &request)?;

    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(ClientError::Timeout)?;
    let mut reader = BufReader::new(stream);
    let line = read_bounded(&mut reader, deadline)?;
    let result = decode_reply(&line, &json!(START_ID))?;
    set_read_deadline(&reader, None, "clear read")?;

    Ok((reader, result))
}

/// Writes one newline-terminated JSON line, bounded at [`MAX_LINE_BYTES`].
fn write_line(writer: &mut UnixStream, message: &Value) -> Result<(), ClientError> {
    let mut line = serde_json::to_vec(message).map_err(ClientError::Malformed)?;
    line.push(b'\n');

    if line.len() > MAX_LINE_BYTES {
        return Err(ClientError::Oversize);
    }

    writer.write_all(&line).map_err(ClientError::Write)
}

/// Maps a read that ran out of time to [`ClientError::Timeout`].
fn read_error(error: io::Error) -> ClientError {
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => ClientError::Timeout,
        _ => ClientError::Read(error),
    }
}

/// Classifies one `fill_buf` result: `Interrupted` asks `read_bounded` to
/// retry after rechecking its deadline; every other error keeps its named form.
fn fill_outcome(result: io::Result<&[u8]>) -> Result<Option<&[u8]>, ClientError> {
    match result {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(None),
        Err(error) => Err(read_error(error)),
    }
}

/// Sets or clears a read deadline on the stream under `reader`.
///
/// macOS returns `EINVAL` from `setsockopt` once the peer has shut down its
/// write half. While clearing a deadline after a complete reply, or while the
/// reader already holds bytes, that is benign: the next read is bounded by
/// data or EOF, not by time. Arming a deadline with nothing buffered fails
/// closed here; the caller turns that `EINVAL` into a nonblocking read whose
/// deadline is enforced in the read loop.
fn set_read_deadline(
    reader: &BufReader<UnixStream>,
    timeout: Option<Duration>,
    step: &'static str,
) -> Result<(), ClientError> {
    read_deadline_result(reader, reader.get_ref().set_read_timeout(timeout), step)
}

/// Decides whether a failed `setsockopt` is the benign closed-peer `EINVAL`.
fn read_deadline_result(
    reader: &BufReader<UnixStream>,
    result: io::Result<()>,
    step: &'static str,
) -> Result<(), ClientError> {
    match result {
        Ok(()) => Ok(()),
        Err(source)
            if source.kind() == io::ErrorKind::InvalidInput
                && (step == "clear read" || !reader.buffer().is_empty()) =>
        {
            Ok(())
        }
        Err(source) => Err(ClientError::SetTimeout { step, source }),
    }
}

/// Reads one newline-terminated line, bounded at [`MAX_LINE_BYTES`] and by
/// `deadline`, restoring blocking mode on every exit path.
fn read_bounded(
    reader: &mut BufReader<UnixStream>,
    deadline: Instant,
) -> Result<Vec<u8>, ClientError> {
    let mut nonblocking = false;
    let result = read_line(reader, deadline, &mut nonblocking);
    if nonblocking {
        reader
            .get_ref()
            .set_nonblocking(false)
            .map_err(|source| ClientError::SetTimeout {
                step: "clear nonblocking",
                source,
            })?;
    }
    result
}

/// Arms the socket read timeout. A closed peer's EINVAL selects the nonblocking
/// fallback; every other failure keeps its named error.
fn arm_read_deadline(
    reader: &BufReader<UnixStream>,
    remaining: Duration,
) -> Result<bool, ClientError> {
    let armed = reader.get_ref().set_read_timeout(Some(remaining));
    read_arm_result(reader, armed)
}

/// Classifies a failed read-arm `setsockopt`, switching to a nonblocking read
/// when a shut-down peer makes it fail with EINVAL.
fn read_arm_result(
    reader: &BufReader<UnixStream>,
    result: io::Result<()>,
) -> Result<bool, ClientError> {
    match read_deadline_result(reader, result, "read") {
        Ok(()) => Ok(false),
        Err(ClientError::SetTimeout {
            step: "read",
            source,
        }) if source.kind() == io::ErrorKind::InvalidInput => {
            reader
                .get_ref()
                .set_nonblocking(true)
                .map_err(|source| ClientError::SetTimeout {
                    step: "set nonblocking",
                    source,
                })?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

/// Runs the read loop. `nonblocking` records whether the fallback took over,
/// so `read_bounded` can restore blocking mode before returning.
fn read_line(
    reader: &mut BufReader<UnixStream>,
    deadline: Instant,
    nonblocking: &mut bool,
) -> Result<Vec<u8>, ClientError> {
    let mut line: Vec<u8> = Vec::new();

    loop {
        if let Some(end) = reader.buffer().iter().position(|&byte| byte == b'\n') {
            if line.len().saturating_add(end).saturating_add(1) > MAX_LINE_BYTES {
                return Err(ClientError::Oversize);
            }
            let slice = reader
                .buffer()
                .get(..=end)
                .ok_or_else(|| ClientError::Read(io::Error::other("incomplete buffered line")))?;
            line.extend_from_slice(slice);
            reader.consume(end.saturating_add(1));
            return Ok(line);
        }

        if line.len().saturating_add(reader.buffer().len()) >= MAX_LINE_BYTES {
            return Err(ClientError::Oversize);
        }
        let buffered = reader.buffer().len();
        let slice = reader
            .buffer()
            .get(..buffered)
            .ok_or_else(|| ClientError::Read(io::Error::other("incomplete buffered line")))?;
        line.extend_from_slice(slice);
        reader.consume(buffered);

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClientError::Timeout);
        }

        if !*nonblocking {
            // A zero timeval means "no timeout" on Unix, so keep it positive.
            *nonblocking = arm_read_deadline(reader, remaining.max(RETRY_INTERVAL))?;
        }

        let result = reader.fill_buf();
        if *nonblocking
            && matches!(&result, Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        {
            if deadline.saturating_duration_since(Instant::now()).is_zero() {
                return Err(ClientError::Timeout);
            }
            thread::sleep(RETRY_INTERVAL);
            continue;
        }
        let Some(bytes) = fill_outcome(result)? else {
            continue;
        };
        if bytes.is_empty() {
            return Err(ClientError::Eof);
        }
    }
}

/// Validates one reply line and decodes its `stall.start` result.
fn decode_reply(line: &[u8], id: &Value) -> Result<StallStartResult, ClientError> {
    let reply: Value = serde_json::from_slice(line).map_err(ClientError::Malformed)?;

    if reply.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(ClientError::VersionMismatch);
    }
    // The protocol field is additive: an older barn may omit it, but one that
    // sends it must match this stall's version.
    if let Some(protocol) = reply.get("protocol")
        && protocol.as_u64() != Some(u64::from(PROTOCOL_VERSION))
    {
        return Err(ClientError::VersionMismatch);
    }
    if reply.get("id") != Some(id) {
        return Err(ClientError::IdMismatch);
    }
    if let Some(error) = reply.get("error") {
        return Err(ClientError::ErrorReply(error.clone()));
    }

    let result = reply.get("result").ok_or_else(|| {
        ClientError::Malformed(serde_json::Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            "no result",
        )))
    })?;

    serde_json::from_value(result.clone()).map_err(ClientError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::Shutdown;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    /// Runs a handshake against a fake barn on the other end of a socket pair.
    ///
    /// The fake reads the one request line, passes it to `check`, writes
    /// `reply` when it is not empty, and drops the socket.
    fn exchange<F>(
        reply: &[u8],
        token: &str,
        jobs: BTreeMap<String, Job>,
        check: F,
    ) -> Result<(BufReader<UnixStream>, StallStartResult), ClientError>
    where
        F: FnOnce(&[u8]) + Send,
    {
        let (client, server) = UnixStream::pair().expect("a socket pair");

        thread::scope(|scope| {
            scope.spawn(|| {
                let mut server = server;
                server
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("a read deadline");
                let mut request = Vec::new();
                let mut lines = BufReader::new(&mut server);
                lines.read_until(b'\n', &mut request).expect("the request");

                check(&request);

                if !reply.is_empty() {
                    let _ = lines.get_mut().write_all(reply);
                }
            });

            handshake(client, token, jobs, Duration::from_secs(2))
        })
    }

    #[test]
    fn a_start_sends_one_line_and_returns_the_result() {
        let reply =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"protocol\":1,\"result\":{\"state\":\"/state\",\"jobs\":{\"heartbeat\":{\"every\":\"30s\"}}}}\n";
        let jobs = BTreeMap::from([(
            "poll".to_owned(),
            Job {
                every: "15s".to_owned(),
            },
        )]);
        let expected = StallStartResult {
            state: "/state".to_owned(),
            jobs: BTreeMap::from([(
                "heartbeat".to_owned(),
                Job {
                    every: "30s".to_owned(),
                },
            )]),
        };

        let (mut reader, result) = exchange(reply, "secret", jobs, |line| {
            assert_eq!(line.last(), Some(&b'\n'));

            let sent: Value = serde_json::from_slice(line).expect("the request is one JSON line");
            let params = StallStartParams {
                protocol: PROTOCOL_VERSION,
                token: "secret".to_owned(),
                jobs: BTreeMap::from([(
                    "poll".to_owned(),
                    Job {
                        every: "15s".to_owned(),
                    },
                )]),
            };

            assert_eq!(
                sent,
                json!({
                    "jsonrpc": "2.0",
                    "id": START_ID,
                    "method": method::STALL_START,
                    "params": params,
                })
            );
        })
        .expect("the handshake succeeds");

        assert_eq!(result, expected);

        let mut next = Vec::new();
        assert_eq!(
            reader
                .read_until(b'\n', &mut next)
                .expect("read after reply"),
            0
        );
    }

    #[test]
    fn a_reply_with_another_id_is_rejected() {
        let reply =
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"protocol\":1,\"result\":{\"state\":\"/state\"}}\n";

        let error =
            exchange(reply, "secret", BTreeMap::new(), |_| {}).expect_err("the reply id differs");

        assert!(matches!(error, ClientError::IdMismatch));
    }

    #[test]
    fn a_denied_reply_is_rejected() {
        let reply = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"protocol\":1,\"error\":{\"code\":-32001,\"message\":\"denied\",\"data\":{\"reason\":\"protocol mismatch\"}}}\n";

        let error =
            exchange(reply, "secret", BTreeMap::new(), |_| {}).expect_err("barn refused the start");

        match error {
            ClientError::ErrorReply(reply) => assert_eq!(
                reply,
                json!({
                    "code": -32001,
                    "message": "denied",
                    "data": { "reason": "protocol mismatch" },
                })
            ),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_reply_from_another_version_is_rejected() {
        let stall =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"protocol\":2,\"result\":{\"state\":\"/state\"}}\n";
        let envelope =
            b"{\"jsonrpc\":\"1.0\",\"id\":1,\"protocol\":1,\"result\":{\"state\":\"/state\"}}\n";

        let first = exchange(stall, "secret", BTreeMap::new(), |_| {})
            .expect_err("the stall protocol differs");
        let second = exchange(envelope, "secret", BTreeMap::new(), |_| {})
            .expect_err("the JSON-RPC version differs");

        assert!(matches!(first, ClientError::VersionMismatch));
        assert!(matches!(second, ClientError::VersionMismatch));
    }

    #[test]
    fn a_reply_without_a_protocol_is_accepted() {
        let reply = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"state\":\"/state\"}}\n";
        let expected = StallStartResult {
            state: "/state".to_owned(),
            jobs: BTreeMap::new(),
        };

        let (_, result) = exchange(reply, "secret", BTreeMap::new(), |_| {})
            .expect("an absent protocol is accepted");

        assert_eq!(result, expected);
    }

    #[test]
    fn a_reply_with_a_null_protocol_is_rejected() {
        let reply =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"protocol\":null,\"result\":{\"state\":\"/state\"}}\n";

        let error =
            exchange(reply, "secret", BTreeMap::new(), |_| {}).expect_err("the protocol is null");

        assert!(
            matches!(&error, ClientError::VersionMismatch),
            "got {error:?}"
        );
    }

    #[test]
    fn a_reply_with_a_string_protocol_is_rejected() {
        let reply =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"protocol\":\"1\",\"result\":{\"state\":\"/state\"}}\n";

        let error = exchange(reply, "secret", BTreeMap::new(), |_| {})
            .expect_err("the protocol is a string");

        assert!(
            matches!(&error, ClientError::VersionMismatch),
            "got {error:?}"
        );
    }

    /// A unique scratch directory that cleans itself up at the end of a test.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let unique = format!(
                "barn-stall-{}-{}-{}",
                label,
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(unique);
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a scratch directory");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_silent_peer_times_out() {
        let scratch = TempDir::new("silent");
        let path = scratch.0.join("stall.sock");
        let listener = UnixListener::bind(&path).expect("a listener");
        let timeout = Duration::from_millis(200);
        let started = Instant::now();

        let error = thread::scope(|scope| {
            scope.spawn(|| {
                listener.set_nonblocking(true).expect("a bounded listener");
                let deadline = Instant::now() + Duration::from_secs(2);
                let peer = loop {
                    match listener.accept() {
                        Ok((peer, _)) => break peer,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "the client never connected");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept failed: {error}"),
                    }
                };
                thread::sleep(timeout + Duration::from_millis(500));
                drop(peer);
            });

            connect_with_timeout(&path, "secret", BTreeMap::new(), timeout)
        })
        .expect_err("the silent peer times out");

        assert!(matches!(error, ClientError::Timeout), "got {error:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_closed_socket_is_an_error() {
        let error = exchange(b"", "secret", BTreeMap::new(), |_| {})
            .expect_err("barn closed before replying");

        assert!(matches!(&error, ClientError::Eof), "got {error:?}");
    }

    #[test]
    fn a_shut_down_peer_still_yields_the_buffered_reply() {
        let reply =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"protocol\":1,\"result\":{\"state\":\"/state\"}}\n";
        let expected = StallStartResult {
            state: "/state".to_owned(),
            jobs: BTreeMap::new(),
        };
        let (client, mut server) = UnixStream::pair().expect("a socket pair");

        // The peer buffers a reply and shuts down its write half before the
        // client starts. On macOS the client's set_read_timeout calls then
        // fail with EINVAL while the reply is still available to read.
        server.write_all(reply).expect("the peer writes a reply");
        server
            .shutdown(Shutdown::Write)
            .expect("the peer shuts down its write half");

        let (_, result) = handshake(client, "secret", BTreeMap::new(), Duration::from_secs(2))
            .expect("the buffered reply completes the handshake");

        assert_eq!(result, expected);
    }

    #[test]
    fn an_unbuffered_read_deadline_fails_closed_on_invalid_input() {
        let (client, _peer) = UnixStream::pair().expect("a socket pair");
        let reader = BufReader::new(client);
        let message = "injected EINVAL";

        let source = io::Error::new(io::ErrorKind::InvalidInput, message);
        let error = read_deadline_result(&reader, Err(source), "read")
            .expect_err("an EINVAL with no buffered data fails closed");

        match error {
            ClientError::SetTimeout { step, source } => {
                assert_eq!(step, "read");
                assert_eq!(source.kind(), io::ErrorKind::InvalidInput);
                assert_eq!(source.to_string(), message);
            }
            other => panic!("got {other:?}"),
        }

        let source = io::Error::new(io::ErrorKind::InvalidInput, message);
        assert!(
            read_deadline_result(&reader, Err(source), "clear read").is_ok(),
            "clearing after a reply still tolerates a closed peer"
        );
    }

    #[test]
    fn a_buffered_read_deadline_tolerates_invalid_input() {
        let (client, mut peer) = UnixStream::pair().expect("a socket pair");
        peer.write_all(b"buffered\n")
            .expect("the peer writes bytes");
        let mut reader = BufReader::new(client);
        reader.fill_buf().expect("the reader buffers the bytes");

        let source = io::Error::new(io::ErrorKind::InvalidInput, "injected EINVAL");

        assert!(
            read_deadline_result(&reader, Err(source), "read").is_ok(),
            "buffered bytes bound the next read without a deadline"
        );
    }

    #[test]
    fn an_interrupted_fill_is_retried_and_other_errors_keep_their_names() {
        let data: &[u8] = b"line\n";
        assert!(matches!(fill_outcome(Ok(data)), Ok(Some(bytes)) if bytes == data));

        let interrupted = io::Error::from(io::ErrorKind::Interrupted);
        assert!(
            matches!(fill_outcome(Err(interrupted)), Ok(None)),
            "an interrupted fill asks read_bounded to retry after its deadline check"
        );

        let timed_out = io::Error::from(io::ErrorKind::WouldBlock);
        assert!(matches!(
            fill_outcome(Err(timed_out)),
            Err(ClientError::Timeout)
        ));

        let denied = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        match fill_outcome(Err(denied)) {
            Err(ClientError::Read(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                assert_eq!(error.to_string(), "denied");
            }
            outcome => panic!("got {outcome:?}"),
        }
    }

    #[test]
    fn a_read_arm_einval_switches_to_a_userspace_deadline() {
        let (client, _peer) = UnixStream::pair().expect("a socket pair");
        let reader = BufReader::new(client);
        let source = io::Error::new(io::ErrorKind::InvalidInput, "injected EINVAL");

        let fallback = read_arm_result(&reader, Err(source)).expect("EINVAL selects the fallback");

        assert!(fallback, "the read arm runs on the userspace deadline");
        let mut byte = [0u8; 1];
        assert!(
            matches!(reader.get_ref().read(&mut byte), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "a nonblocking read reports WouldBlock while no bytes are buffered"
        );

        let source = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let error = read_arm_result(&reader, Err(source)).expect_err("other errors stay named");

        match error {
            ClientError::SetTimeout { step, source } => {
                assert_eq!(step, "read");
                assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
                assert_eq!(source.to_string(), "denied");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_nonblocking_read_retries_would_block_until_its_deadline() {
        let (client, _peer) = UnixStream::pair().expect("a socket pair");
        let mut reader = BufReader::new(client);
        reader
            .get_ref()
            .set_nonblocking(true)
            .expect("a nonblocking handshake read");
        let timeout = Duration::from_millis(50);
        let deadline = Instant::now() + timeout;
        let started = Instant::now();
        let mut nonblocking = true;

        let error = read_line(&mut reader, deadline, &mut nonblocking)
            .expect_err("no reply arrives before the deadline");

        assert!(matches!(error, ClientError::Timeout), "got {error:?}");
        assert!(started.elapsed() >= timeout);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn an_oversize_reply_is_an_error() {
        let reply = vec![b'x'; MAX_LINE_BYTES];

        let error = exchange(&reply, "secret", BTreeMap::new(), |_| {})
            .expect_err("the reply filled the line limit");

        assert!(matches!(error, ClientError::Oversize));
    }

    #[test]
    fn a_reply_line_exactly_at_the_byte_cap_is_accepted() {
        // Serialize a valid reply skeleton, then pad its state so the whole
        // line, including the newline, is exactly MAX_LINE_BYTES. A reply that
        // lands on the cap still fits and must not be reported as Oversize.
        let skeleton = json!({
            "jsonrpc": "2.0",
            "id": START_ID,
            "protocol": PROTOCOL_VERSION,
            "result": { "state": "", "jobs": {} },
        });
        let skeleton_len = serde_json::to_vec(&skeleton)
            .expect("the skeleton serializes")
            .len()
            + 1; // the newline
        let state = "x".repeat(MAX_LINE_BYTES - skeleton_len);

        let expected = StallStartResult {
            state: state.clone(),
            jobs: BTreeMap::new(),
        };
        let message = json!({
            "jsonrpc": "2.0",
            "id": START_ID,
            "protocol": PROTOCOL_VERSION,
            "result": { "state": state, "jobs": {} },
        });
        let mut reply = serde_json::to_vec(&message).expect("the reply serializes");
        reply.push(b'\n');
        assert_eq!(reply.len(), MAX_LINE_BYTES);

        let (_, result) = exchange(&reply, "secret", BTreeMap::new(), |_| {})
            .expect("a line exactly at the cap is accepted");

        assert_eq!(result, expected);
    }

    #[test]
    fn an_oversize_write_is_refused_before_it_reaches_the_socket() {
        let (mut writer, _peer) = UnixStream::pair().expect("a socket pair");
        writer.set_nonblocking(true).expect("a nonblocking writer");

        let padding = "x".repeat(MAX_LINE_BYTES);
        let oversize = json!(padding);

        let error = write_line(&mut writer, &oversize).expect_err("the line exceeds the cap");

        assert!(matches!(error, ClientError::Oversize), "got {error:?}");
    }

    #[test]
    fn a_small_write_reaches_the_peer_as_one_newline_terminated_line() {
        let (mut writer, peer) = UnixStream::pair().expect("a socket pair");
        let mut reader = BufReader::new(peer);
        reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("a read deadline");

        let expected = b"{\"a\":1}\n".to_vec();

        thread::scope(|scope| {
            scope.spawn(|| {
                let mut received = Vec::new();
                reader
                    .read_until(b'\n', &mut received)
                    .expect("the peer reads the whole line");
                assert_eq!(received, expected);
            });

            write_line(&mut writer, &json!({"a": 1})).expect("the small write succeeds");
        });
    }

    #[test]
    fn a_write_at_the_byte_cap_is_accepted_and_reaches_the_peer() {
        let (mut writer, peer) = UnixStream::pair().expect("a socket pair");

        // A JSON string adds two quote bytes and the newline adds one, so a
        // payload of MAX_LINE_BYTES - 3 makes the line exactly MAX_LINE_BYTES.
        let padding = "x".repeat(MAX_LINE_BYTES - 3);
        let message = json!(padding);
        let mut expected = serde_json::to_vec(&message).expect("the message serializes");
        expected.push(b'\n');
        assert_eq!(expected.len(), MAX_LINE_BYTES);

        thread::scope(|scope| {
            scope.spawn(|| {
                let mut peer = peer;
                peer.set_read_timeout(Some(Duration::from_secs(1)))
                    .expect("a read deadline");
                let mut received = Vec::new();
                peer.read_to_end(&mut received)
                    .expect("the peer drains the line");
                assert_eq!(received, expected);
            });

            write_line(&mut writer, &message).expect("a line at the cap is accepted");
            drop(writer);
        });
    }

    #[test]
    fn errors_display_their_exact_messages() {
        let connect = io::Error::new(io::ErrorKind::ConnectionRefused, "cannot connect");
        let connect_message = connect.to_string();
        let write = io::Error::new(io::ErrorKind::BrokenPipe, "cannot write");
        let write_message = write.to_string();
        let read = io::Error::new(io::ErrorKind::UnexpectedEof, "cannot read");
        let read_message = read.to_string();
        let timeout = io::Error::new(io::ErrorKind::InvalidInput, "invalid argument");
        let timeout_message = timeout.to_string();
        let malformed = serde_json::from_str::<Value>("{").expect_err("invalid JSON");
        let malformed_message = malformed.to_string();
        let reply = json!({"code": -32001, "message": "denied"});
        let reply_message = reply.to_string();

        assert_eq!(
            ClientError::MissingSocket.to_string(),
            "BARN_SOCKET is not set"
        );
        assert_eq!(
            ClientError::MissingToken.to_string(),
            "BARN_TOKEN is not set"
        );
        assert_eq!(
            ClientError::Connect(connect).to_string(),
            format!("connecting to barn: {connect_message}")
        );
        assert_eq!(
            ClientError::Write(write).to_string(),
            format!("writing stall start: {write_message}")
        );
        assert_eq!(
            ClientError::Read(read).to_string(),
            format!("reading the start reply: {read_message}")
        );
        assert_eq!(
            ClientError::SetTimeout {
                step: "read",
                source: timeout,
            }
            .to_string(),
            format!("setting read timeout: {timeout_message}")
        );
        assert_eq!(
            ClientError::Timeout.to_string(),
            "the start reply timed out"
        );
        assert_eq!(
            ClientError::Eof.to_string(),
            "the start reply closed before its newline"
        );
        assert_eq!(
            ClientError::Oversize.to_string(),
            "a start line exceeded MAX_LINE_BYTES"
        );
        assert_eq!(
            ClientError::Malformed(malformed).to_string(),
            format!("a start line is malformed: {malformed_message}")
        );
        assert_eq!(
            ClientError::ErrorReply(reply).to_string(),
            format!("barn refused stall start: {reply_message}")
        );
        assert_eq!(
            ClientError::IdMismatch.to_string(),
            "the start reply carried another id"
        );
        assert_eq!(
            ClientError::VersionMismatch.to_string(),
            "the start reply used another protocol version"
        );
    }

    #[test]
    fn connect_write_read_and_malformed_expose_their_source() {
        let connect = io::Error::new(io::ErrorKind::ConnectionRefused, "cannot connect");
        let connect_message = connect.to_string();
        let error = ClientError::Connect(connect);
        let source = std::error::Error::source(&error).expect("connect has a source");
        assert_eq!(source.to_string(), connect_message);

        let write = io::Error::new(io::ErrorKind::BrokenPipe, "cannot write");
        let write_message = write.to_string();
        let error = ClientError::Write(write);
        let source = std::error::Error::source(&error).expect("write has a source");
        assert_eq!(source.to_string(), write_message);

        let read = io::Error::new(io::ErrorKind::UnexpectedEof, "cannot read");
        let read_message = read.to_string();
        let error = ClientError::Read(read);
        let source = std::error::Error::source(&error).expect("read has a source");
        assert_eq!(source.to_string(), read_message);

        let timeout = io::Error::new(io::ErrorKind::InvalidInput, "invalid argument");
        let timeout_message = timeout.to_string();
        let error = ClientError::SetTimeout {
            step: "clear read",
            source: timeout,
        };
        let source = std::error::Error::source(&error).expect("set timeout has a source");
        assert_eq!(source.to_string(), timeout_message);

        let malformed = serde_json::from_str::<Value>("{").expect_err("invalid JSON");
        let malformed_message = malformed.to_string();
        let error = ClientError::Malformed(malformed);
        let source = std::error::Error::source(&error).expect("malformed has a source");
        assert_eq!(source.to_string(), malformed_message);
    }

    #[test]
    fn errors_without_a_source_return_none() {
        let reply = json!({"code": -32001, "message": "denied"});
        let cases = [
            ClientError::MissingSocket,
            ClientError::MissingToken,
            ClientError::Eof,
            ClientError::Oversize,
            ClientError::ErrorReply(reply),
            ClientError::Timeout,
            ClientError::IdMismatch,
            ClientError::VersionMismatch,
        ];

        for error in cases {
            assert!(std::error::Error::source(&error).is_none());
        }
    }
}
