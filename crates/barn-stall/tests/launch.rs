//! The launch line is the only channel barn and tack share at startup.
//! These tests pin both wire shapes and every rejection, so a changed key set,
//! protocol or limit fails here.

use std::path::PathBuf;

use barn_stall::{LAUNCH_PROTOCOL, Launch, LaunchError, MAX_LAUNCH_LINE_BYTES, parse};
use serde_json::{Value, json};

/// Serialises a wire value without a trailing newline.
fn wire(value: &Value) -> Option<Vec<u8>> {
    serde_json::to_vec(value).ok()
}

/// Adds the one newline barn writes after the JSON.
fn with_newline(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.push(b'\n');
    bytes
}

/// The canonical success value, with a short single-use token.
fn endpoint(token: &str) -> Value {
    json!({
        "protocol": LAUNCH_PROTOCOL,
        "socket_path": "/run/barn/sourcefed.sock",
        "token": token,
    })
}

#[test]
fn the_contract_constants_are_pinned() {
    assert_eq!(LAUNCH_PROTOCOL, 1);
    assert_eq!(MAX_LAUNCH_LINE_BYTES, 8192);
}

#[test]
fn the_success_line_has_the_exact_wire_shape() {
    assert_eq!(
        with_newline(wire(&endpoint("one-use")).expect("a wire line")),
        b"{\"protocol\":1,\"socket_path\":\"/run/barn/sourcefed.sock\",\"token\":\"one-use\"}\n"
            .to_vec()
    );
}

#[test]
fn a_success_line_reads_the_socket_and_token() {
    let bare = wire(&endpoint("one-use")).expect("a wire line");
    let terminated = with_newline(bare.clone());
    let expected = Launch::Endpoint {
        socket_path: PathBuf::from("/run/barn/sourcefed.sock"),
        token: "one-use".to_owned(),
    };

    assert_eq!(parse(&bare), Ok(expected.clone()));
    assert_eq!(parse(&terminated), Ok(expected));
}

#[test]
fn an_error_line_reads_the_named_reason() {
    let error = json!({"protocol": 1, "error": "exec failed"});
    let line = with_newline(wire(&error).expect("a wire line"));

    assert_eq!(
        parse(&line),
        Ok(Launch::Error {
            reason: "exec failed".to_owned(),
        })
    );
}

#[test]
fn an_unknown_field_is_refused() {
    let mut extra = endpoint("one-use");
    extra
        .as_object_mut()
        .expect("an endpoint object")
        .insert("extra".to_owned(), json!(1));
    let mut mixed = json!({"protocol": 1, "error": "exec failed"});
    mixed
        .as_object_mut()
        .expect("an error object")
        .insert("token".to_owned(), json!("one-use"));

    assert_eq!(
        parse(&wire(&extra).expect("a wire line")),
        Err(LaunchError::UnknownField("extra".to_owned()))
    );
    assert_eq!(
        parse(&wire(&mixed).expect("a wire line")),
        Err(LaunchError::UnknownField("token".to_owned()))
    );
}

#[test]
fn a_missing_field_is_refused() {
    let no_token = json!({"protocol": 1, "socket_path": "/run/barn/sourcefed.sock"});
    let no_socket = json!({"protocol": 1, "token": "one-use"});
    let no_protocol = json!({"socket_path": "/run/barn/sourcefed.sock", "token": "one-use"});

    assert_eq!(
        parse(&wire(&no_token).expect("a wire line")),
        Err(LaunchError::MissingField("token"))
    );
    assert_eq!(
        parse(&wire(&no_socket).expect("a wire line")),
        Err(LaunchError::MissingField("socket_path"))
    );
    assert_eq!(
        parse(&wire(&no_protocol).expect("a wire line")),
        Err(LaunchError::MissingField("protocol"))
    );
}

#[test]
fn a_wrong_protocol_is_refused() {
    let line = json!({"protocol": 2, "socket_path": "/x", "token": "t"});

    assert_eq!(
        parse(&wire(&line).expect("a wire line")),
        Err(LaunchError::WrongProtocol)
    );
}

#[test]
fn a_wrong_field_type_is_refused() {
    let protocol = json!({"protocol": "1", "socket_path": "/x", "token": "t"});
    let socket = json!({"protocol": 1, "socket_path": 7, "token": "t"});
    let token = json!({"protocol": 1, "socket_path": "/x", "token": ["t"]});
    let error = json!({"protocol": 1, "error": 7});

    assert_eq!(
        parse(&wire(&protocol).expect("a wire line")),
        Err(LaunchError::WrongType("protocol"))
    );
    assert_eq!(
        parse(&wire(&socket).expect("a wire line")),
        Err(LaunchError::WrongType("socket_path"))
    );
    assert_eq!(
        parse(&wire(&token).expect("a wire line")),
        Err(LaunchError::WrongType("token"))
    );
    assert_eq!(
        parse(&wire(&error).expect("a wire line")),
        Err(LaunchError::WrongType("error"))
    );
}

#[test]
fn a_line_at_the_byte_cap_is_accepted_and_one_over_is_refused() {
    let overhead = with_newline(wire(&endpoint("")).expect("a wire line")).len();
    let token_len = MAX_LAUNCH_LINE_BYTES.saturating_sub(overhead);
    let at_cap = with_newline(wire(&endpoint(&"x".repeat(token_len))).expect("a wire line"));
    let token = "x".repeat(token_len.saturating_add(1));
    let over_cap = with_newline(wire(&endpoint(&token)).expect("a wire line"));

    assert_eq!(at_cap.len(), MAX_LAUNCH_LINE_BYTES);
    assert_eq!(over_cap.len(), MAX_LAUNCH_LINE_BYTES.saturating_add(1));
    assert_eq!(
        parse(&at_cap),
        Ok(Launch::Endpoint {
            socket_path: PathBuf::from("/run/barn/sourcefed.sock"),
            token: "x".repeat(token_len),
        })
    );
    assert_eq!(parse(&over_cap), Err(LaunchError::Oversize));
}

#[test]
fn a_malformed_line_is_refused() {
    let not_json = b"not json".as_slice();
    let not_object = wire(&json!([1, 2, 3])).expect("a wire line");
    let truncated = b"{\"protocol\":1".as_slice();

    assert!(matches!(parse(not_json), Err(LaunchError::Malformed(_))));
    assert!(matches!(parse(&not_object), Err(LaunchError::Malformed(_))));
    assert!(matches!(parse(truncated), Err(LaunchError::Malformed(_))));
}

#[test]
fn an_empty_line_is_refused() {
    assert_eq!(parse(b""), Err(LaunchError::Empty));
    assert_eq!(parse(b"\n"), Err(LaunchError::Empty));
}

#[test]
fn only_one_trailing_newline_is_accepted() {
    let bare = wire(&endpoint("one-use")).expect("a wire line");
    let doubled = with_newline(with_newline(bare.clone()));
    let mut leading = bare.clone();
    leading.insert(0, b'\n');

    assert!(matches!(parse(&doubled), Err(LaunchError::Malformed(_))));
    assert!(matches!(parse(&leading), Err(LaunchError::Malformed(_))));
    assert_eq!(parse(&with_newline(bare.clone())), parse(&bare));
}
