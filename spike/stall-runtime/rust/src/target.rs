//! The `target` query parameter: base64url (no padding) of the target JSON.

use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;

use crate::model::Target;

pub fn encode(target: &Target) -> Result<String, String> {
    let json = serde_json::to_vec(target).map_err(|error| format!("target json: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(json))
}

pub fn decode(raw: &str) -> Result<Target, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .or_else(|_| URL_SAFE.decode(raw))
        .map_err(|error| format!("target is not base64url: {error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("target is not a Target: {error}"))
}

/// Decode the raw query value, accepting percent-escaped padding (`%3D`).
pub fn decode_query(raw: &str) -> Result<Target, String> {
    if raw.contains('%') {
        decode(&percent_decode(raw)?)
    } else {
        decode(raw)
    }
}

fn percent_decode(raw: &str) -> Result<String, String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = raw
                .get(index + 1..index + 3)
                .ok_or_else(|| "truncated percent escape".to_string())?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|_| format!("invalid percent escape %{hex}"))?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "percent escape produced non-UTF-8".to_string())
}
