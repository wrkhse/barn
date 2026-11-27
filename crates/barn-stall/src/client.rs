//! The stall side of the `stall.start` handshake.
//!
//! A stall connects to the private socket in `BARN_SOCKET`, sends one bounded
//! `stall.start` line carrying the single-use `BARN_TOKEN`, and reads one
//! bounded reply. The caller gets the authenticated connection back, ready for
//! the frames barn sends next. Every read and write is capped at
//! [`MAX_LINE_BYTES`], so a hostile or broken peer cannot grow the line.

use std::collections::BTreeMap;
use std::fmt::{self, Formatter};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::protocol::{
    Job, MAX_LINE_BYTES, PROTOCOL_VERSION, StallStartParams, StallStartResult, method,
};

/// The id of the stall's one `stall.start` request.
const START_ID: u64 = 1;

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
    let stream = UnixStream::connect(socket).map_err(ClientError::Connect)?;

    handshake(stream, token, jobs)
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

    let mut reader = BufReader::new(stream);
    let line = read_bounded(&mut reader)?;
    let result = decode_reply(&line, &json!(START_ID))?;

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

/// Reads one newline-terminated line, bounded at [`MAX_LINE_BYTES`].
fn read_bounded(reader: &mut BufReader<UnixStream>) -> Result<Vec<u8>, ClientError> {
    let limit = u64::try_from(MAX_LINE_BYTES).unwrap_or(u64::MAX);
    let mut line = Vec::new();
    reader
        .take(limit)
        .read_until(b'\n', &mut line)
        .map_err(ClientError::Read)?;

    if line.last() == Some(&b'\n') {
        Ok(line)
    } else if line.len() == MAX_LINE_BYTES {
        Err(ClientError::Oversize)
    } else {
        Err(ClientError::Eof)
    }
}

/// Validates one reply line and decodes its `stall.start` result.
fn decode_reply(line: &[u8], id: &Value) -> Result<StallStartResult, ClientError> {
    let reply: Value = serde_json::from_slice(line).map_err(ClientError::Malformed)?;

    if reply.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(ClientError::VersionMismatch);
    }
    let offered = reply.get("protocol").and_then(Value::as_u64);
    if offered.is_some_and(|version| version != u64::from(PROTOCOL_VERSION)) {
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
    use std::thread;

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
                let mut request = Vec::new();
                let mut lines = BufReader::new(&mut server);
                lines.read_until(b'\n', &mut request).expect("the request");

                check(&request);

                if !reply.is_empty() {
                    let _ = lines.get_mut().write_all(reply);
                }
            });

            handshake(client, token, jobs)
        })
    }

    #[test]
    fn a_start_sends_one_line_and_returns_the_result() {
        let reply =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"state\":\"/state\",\"jobs\":{}}}\n";
        let jobs = BTreeMap::from([(
            "poll".to_owned(),
            Job {
                every: "15s".to_owned(),
            },
        )]);
        let expected = StallStartResult {
            state: "/state".to_owned(),
            jobs: jobs.clone(),
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
        let reply = b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"state\":\"/state\"}}\n";

        let error =
            exchange(reply, "secret", BTreeMap::new(), |_| {}).expect_err("the reply id differs");

        assert!(matches!(error, ClientError::IdMismatch));
    }

    #[test]
    fn a_denied_reply_is_rejected() {
        let reply = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32001,\"message\":\"denied\",\"data\":{\"reason\":\"protocol mismatch\"}}}\n";

        let error =
            exchange(reply, "secret", BTreeMap::new(), |_| {}).expect_err("barn refused the start");

        assert!(matches!(error, ClientError::ErrorReply(_)));
    }

    #[test]
    fn a_reply_from_another_version_is_rejected() {
        let stall =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"protocol\":2,\"result\":{\"state\":\"/state\"}}\n";
        let envelope = b"{\"jsonrpc\":\"1.0\",\"id\":1,\"result\":{\"state\":\"/state\"}}\n";

        let first = exchange(stall, "secret", BTreeMap::new(), |_| {})
            .expect_err("the stall protocol differs");
        let second = exchange(envelope, "secret", BTreeMap::new(), |_| {})
            .expect_err("the JSON-RPC version differs");

        assert!(matches!(first, ClientError::VersionMismatch));
        assert!(matches!(second, ClientError::VersionMismatch));
    }

    #[test]
    fn a_closed_socket_is_an_error() {
        let error = exchange(b"", "secret", BTreeMap::new(), |_| {})
            .expect_err("barn closed before replying");

        assert!(matches!(error, ClientError::Eof));
    }

    #[test]
    fn an_oversize_reply_is_an_error() {
        let reply = vec![b'x'; MAX_LINE_BYTES];

        let error = exchange(&reply, "secret", BTreeMap::new(), |_| {})
            .expect_err("the reply filled the line limit");

        assert!(matches!(error, ClientError::Oversize));
    }
}
