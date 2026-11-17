//! The `?target=` parameter: base64url (no padding) around the target JSON.

use base64::engine::general_purpose::URL_SAFE;
use base64::Engine;
use stall_b::model::Target;
use stall_b::target;

fn target() -> Target {
    Target {
        kind: "program".to_string(),
        id: "spike".to_string(),
    }
}

#[test]
fn round_trips() {
    let encoded = target::encode(&target()).expect("encode");
    assert_eq!(target::decode(&encoded).expect("decode"), target());
}

#[test]
fn encoding_is_url_safe_and_unpadded() {
    let encoded = target::encode(&target()).expect("encode");
    assert!(!encoded.contains('='), "padding leaked: {encoded}");
    assert!(!encoded.contains('+'), "standard base64 leaked: {encoded}");
    assert!(!encoded.contains('/'), "standard base64 leaked: {encoded}");
}

#[test]
fn decodes_padded_base64url() {
    let json = serde_json::to_vec(&target()).expect("json");
    let padded = URL_SAFE.encode(json);
    assert_eq!(target::decode(&padded).expect("decode"), target());
}

#[test]
fn decodes_percent_escaped_padding() {
    let json = serde_json::to_vec(&target()).expect("json");
    let padded = URL_SAFE.encode(json);
    let escaped = padded.replace('=', "%3D");
    assert_eq!(target::decode_query(&escaped).expect("decode"), target());
}

#[test]
fn rejects_garbage() {
    let error = target::decode("!!!!").expect_err("must fail");
    assert!(error.contains("base64url"), "unexpected error: {error}");
}

#[test]
fn rejects_base64_of_the_wrong_json() {
    let encoded = URL_SAFE.encode(br#"{"kind":1}"#);
    let error = target::decode(&encoded).expect_err("must fail");
    assert!(error.contains("Target"), "unexpected error: {error}");
}
