//! The one line barn writes to stdout when it launches a stall.
//!
//! barn starts the stall, then writes exactly one JSON line: on success the
//! private socket and the single-use token, or a named start error. tack reads
//! that line without any other channel, so the fields here are its whole
//! contract. A line is at most [`MAX_LAUNCH_LINE_BYTES`], its newline included,
//! and a single trailing newline is accepted.
//!
//! Each variant's key set is exact: an unknown field, a missing field or a
//! `protocol` other than [`LAUNCH_PROTOCOL`] is refused.

use std::fmt::{self, Formatter};
use std::path::PathBuf;

use serde_json::{Map, Value};

/// The launch pipe protocol version barn and tack speak.
pub const LAUNCH_PROTOCOL: u32 = 1;

/// The longest launch line, in bytes, its newline included: 8 KiB.
pub const MAX_LAUNCH_LINE_BYTES: usize = 8 * 1024;

const PROTOCOL_KEY: &str = "protocol";
const SOCKET_PATH_KEY: &str = "socket_path";
const TOKEN_KEY: &str = "token";
const ERROR_KEY: &str = "error";

/// What barn writes on the launch pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// The stall started: its private socket and single-use token.
    Endpoint {
        /// The absolute path of the socket barn holds for the stall.
        socket_path: PathBuf,
        /// The single-use token for `stall.start`.
        token: String,
    },
    /// The stall did not start, with the named reason barn reports.
    Error {
        /// A short, machine-readable reason.
        reason: String,
    },
}

/// Why a launch line could not be read as a [`Launch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// The line carried no bytes, or only its newline.
    Empty,
    /// The line was not one JSON object.
    Malformed(String),
    /// The line was longer than [`MAX_LAUNCH_LINE_BYTES`].
    Oversize,
    /// `protocol` was a number other than [`LAUNCH_PROTOCOL`].
    WrongProtocol,
    /// A field outside the variant's exact key set was present.
    UnknownField(String),
    /// A field the variant requires was absent.
    MissingField(&'static str),
    /// A field held a value of the wrong JSON type.
    WrongType(&'static str),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the launch line is empty"),
            Self::Malformed(reason) => write!(formatter, "malformed launch line: {reason}"),
            Self::Oversize => formatter.write_str("the launch line exceeded MAX_LAUNCH_LINE_BYTES"),
            Self::WrongProtocol => formatter.write_str("the launch line used another protocol"),
            Self::UnknownField(name) => write!(formatter, "unknown launch field: {name}"),
            Self::MissingField(name) => write!(formatter, "missing launch field: {name}"),
            Self::WrongType(name) => write!(formatter, "launch field {name} has the wrong type"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// Reads one launch line into a [`Launch`].
///
/// A single trailing newline is accepted. Any other newline, a line over
/// [`MAX_LAUNCH_LINE_BYTES`], or a line whose keys are not exactly those of one
/// variant is refused.
///
/// # Errors
///
/// Returns a [`LaunchError`] for an empty, oversize or malformed line, a wrong
/// protocol, or an unknown, missing or wrongly typed field.
pub fn parse(line: &[u8]) -> Result<Launch, LaunchError> {
    if line.len() > MAX_LAUNCH_LINE_BYTES {
        return Err(LaunchError::Oversize);
    }

    let body = line.strip_suffix(b"\n").unwrap_or(line);
    if body.contains(&b'\n') {
        return Err(LaunchError::Malformed("more than one line".to_owned()));
    }
    if body.is_empty() {
        return Err(LaunchError::Empty);
    }

    let value: Value =
        serde_json::from_slice(body).map_err(|error| LaunchError::Malformed(error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| LaunchError::Malformed("not a JSON object".to_owned()))?;

    check_protocol(object)?;

    if object.contains_key(ERROR_KEY) {
        parse_error(object)
    } else {
        parse_endpoint(object)
    }
}

/// The protocol field must be an integer holding the one version barn speaks.
fn check_protocol(object: &Map<String, Value>) -> Result<(), LaunchError> {
    let value = object
        .get(PROTOCOL_KEY)
        .ok_or(LaunchError::MissingField(PROTOCOL_KEY))?;

    match value.as_u64() {
        Some(version) if version == u64::from(LAUNCH_PROTOCOL) => Ok(()),
        Some(_) => Err(LaunchError::WrongProtocol),
        None => Err(LaunchError::WrongType(PROTOCOL_KEY)),
    }
}

/// Reads the error variant, whose only fields are `protocol` and `error`.
fn parse_error(object: &Map<String, Value>) -> Result<Launch, LaunchError> {
    reject_unknown(object, &[PROTOCOL_KEY, ERROR_KEY])?;
    let reason = string_field(object, ERROR_KEY)?;

    Ok(Launch::Error { reason })
}

/// Reads the endpoint variant, whose only fields are the socket and token.
fn parse_endpoint(object: &Map<String, Value>) -> Result<Launch, LaunchError> {
    reject_unknown(object, &[PROTOCOL_KEY, SOCKET_PATH_KEY, TOKEN_KEY])?;
    let socket_path = string_field(object, SOCKET_PATH_KEY)?;
    let token = string_field(object, TOKEN_KEY)?;

    Ok(Launch::Endpoint {
        socket_path: PathBuf::from(socket_path),
        token,
    })
}

/// No field outside the variant's exact key set may be present.
fn reject_unknown(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), LaunchError> {
    for name in object.keys() {
        if !allowed.contains(&name.as_str()) {
            return Err(LaunchError::UnknownField(name.clone()));
        }
    }

    Ok(())
}

/// A required field must be present and hold a JSON string.
fn string_field(object: &Map<String, Value>, key: &'static str) -> Result<String, LaunchError> {
    let value = object.get(key).ok_or(LaunchError::MissingField(key))?;

    value
        .as_str()
        .map(str::to_owned)
        .ok_or(LaunchError::WrongType(key))
}

#[cfg(test)]
mod tests {
    use super::LaunchError;

    #[test]
    fn every_launch_error_has_an_exact_display() {
        assert_eq!(LaunchError::Empty.to_string(), "the launch line is empty");
        assert_eq!(
            LaunchError::Malformed("bad json".to_owned()).to_string(),
            "malformed launch line: bad json"
        );
        assert_eq!(
            LaunchError::Oversize.to_string(),
            "the launch line exceeded MAX_LAUNCH_LINE_BYTES"
        );
        assert_eq!(
            LaunchError::WrongProtocol.to_string(),
            "the launch line used another protocol"
        );
        assert_eq!(
            LaunchError::UnknownField("extra".to_owned()).to_string(),
            "unknown launch field: extra"
        );
        assert_eq!(
            LaunchError::MissingField("token").to_string(),
            "missing launch field: token"
        );
        assert_eq!(
            LaunchError::WrongType("token").to_string(),
            "launch field token has the wrong type"
        );
    }
}
