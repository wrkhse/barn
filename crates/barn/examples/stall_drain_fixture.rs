//! A test-only example: the stand-in stall for the SIGTERM drain integration
//! test. Cargo builds examples for tests, but this binary is not part of barn's
//! release binary set: `cargo install` and the release tarball ship only `barn`.
//!
//! The fixture connects on the private socket barn names in `BARN_SOCKET`,
//! performs the `stall.start` handshake with the token in `BARN_TOKEN`, and
//! reads bounded frames until the exact `stall.drain` request. On that frame
//! it writes the frame's bytes to `<plugin folder>/drain.marker`, replies with
//! the matching success result, and exits so the test can read the marker.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use barn_stall::{MAX_LINE_BYTES, from_env};

/// The exact drain frame barn sends; the marker records these bytes verbatim.
const DRAIN_REQUEST: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"stall.drain\"}\n";

/// The success reply, carrying the same id, that ends the drain exchange.
const DRAIN_REPLY: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":null}\n";

/// Bound how many frames the fixture reads before it gives up and exits.
const MAX_FRAMES: usize = 1024;

/// Bound how long one frame read waits for bytes from barn.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    let Some(folder) = env::args_os().nth(1) else {
        eprintln!("usage: stall_drain_fixture <plugin-folder>");
        return ExitCode::FAILURE;
    };

    // from_env reads the only two runtime variables barn provides, connects a
    // blocking UnixStream, and runs the exact client.rs stall.start exchange.
    let (mut reader, _state) = match from_env(BTreeMap::new()) {
        Ok(connected) => connected,
        Err(error) => {
            eprintln!("stall.start handshake failed: {error}");
            return ExitCode::FAILURE;
        }
    };

    if reader
        .get_mut()
        .set_read_timeout(Some(FRAME_TIMEOUT))
        .is_err()
    {
        eprintln!("could not set a frame read deadline");
        return ExitCode::FAILURE;
    }

    let marker = PathBuf::from(folder).join("drain.marker");

    for _ in 0..MAX_FRAMES {
        let mut line = Vec::new();
        let got_bytes = reader
            .read_until(b'\n', &mut line)
            .is_ok_and(|count| count != 0);
        if !got_bytes {
            return ExitCode::SUCCESS;
        }

        // An unterminated or oversize line is not a frame this fixture acts on.
        if line.last() != Some(&b'\n') || line.len() > MAX_LINE_BYTES {
            return ExitCode::SUCCESS;
        }

        // Other frames, such as a health check, are skipped.
        if line.as_slice() != DRAIN_REQUEST {
            continue;
        }

        if fs::write(&marker, &line).is_err() {
            eprintln!("could not write the drain marker");
            return ExitCode::FAILURE;
        }

        let peer = reader.get_mut();
        if peer.write_all(DRAIN_REPLY).is_err() || peer.flush().is_err() {
            eprintln!("could not write the drain reply");
            return ExitCode::FAILURE;
        }

        return ExitCode::SUCCESS;
    }

    ExitCode::SUCCESS
}
