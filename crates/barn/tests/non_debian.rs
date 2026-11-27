//! The non-Debian loader grant, proved in the real sandbox. These tests start
//! and exec processes, so they run in CI only; without `CI` set they return at
//! once (AGENTS.md keeps process tests off the owner's Mac).

use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use barn::Grant;
use barn::supervisor::{StallSpec, Supervisor};
use barn_stall::protocol::method;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader as AsyncBufReader;
use tokio::net::UnixStream as AsyncUnixStream;
use tokio::time::timeout;

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// EACCES on Linux, which nono's Landlock backend returns for a write outside
/// the granted modes. CI confirms the number on Fedora, where the loader lives
/// in the lib64 family.
const EACCES: i32 = 13;

const REPLY_DEADLINE: Duration = Duration::from_secs(30);

fn in_ci() -> bool {
    env::var_os("CI").is_some()
}

/// A scratch folder with a state root, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("barn-nonddebian-{}-{seq}", std::process::id()));

        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("state"))?;

        Ok(Self(dir))
    }

    fn state(&self) -> PathBuf {
        self.0.join("state")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The host's plain library root, if it has a non-Debian layout. On a Debian
/// multiarch host the executable's grant is the multiarch directories, so no
/// plain root here would satisfy the write test's read assertion.
fn non_debian_library_dir() -> Option<PathBuf> {
    // A Debian multiarch host grants only the multiarch directories, never a
    // plain root, so the proof must skip it rather than assert the wrong grant.
    for multiarch in ["/lib/x86_64-linux-gnu", "/usr/lib/x86_64-linux-gnu"] {
        if Path::new(multiarch).is_dir() {
            return None;
        }
    }

    // Fedora's lib64 roots and Alpine's musl roots. A candidate that resolves
    // into a multiarch directory (Debian's `/usr/lib64`) is not a plain root.
    let roots = ["/usr/lib64", "/lib64", "/lib", "/usr/lib"];
    for candidate in roots {
        let Ok(resolved) = Path::new(candidate).canonicalize() else {
            continue;
        };

        if roots
            .iter()
            .any(|root| resolved.as_path() == Path::new(root))
        {
            return Some(resolved);
        }
    }

    None
}

/// A file under the granted library root that the sandbox must refuse to write.
fn write_target(library: &Path) -> PathBuf {
    let seq = NEXT.load(Ordering::Relaxed);
    library.join(format!("barn-nonddebian-{}-{seq}", std::process::id()))
}

/// The real `barn-exec` and `barn-luau-stall` binaries. cargo-nextest builds
/// only this package's binaries, so build both helpers in the inherited target
/// dir when they are not beside barn or in the usual debug folder.
fn binaries() -> (PathBuf, PathBuf) {
    let barn = PathBuf::from(env!("CARGO_BIN_EXE_barn"));
    let beside_helper = barn.with_file_name("barn-exec");
    let beside_program = barn.with_file_name("barn-luau-stall");
    let debug = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let debug_helper = debug.join("barn-exec");
    let debug_program = debug.join("barn-luau-stall");

    if beside_helper.is_file() && beside_program.is_file() {
        return (beside_helper, beside_program);
    }
    if debug_helper.is_file() && debug_program.is_file() {
        return (debug_helper, debug_program);
    }

    let inherited_target = env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
    let target = inherited_target
        .as_ref()
        .map_or_else(|| debug.clone(), |dir| dir.join("debug"));
    let mut build = std::process::Command::new("cargo");
    build.args([
        "build",
        "--locked",
        "-p",
        "barn-exec",
        "-p",
        "barn-luau-stall",
    ]);
    if let Some(dir) = &inherited_target {
        build.env("CARGO_TARGET_DIR", dir);
    }
    assert!(
        build.status().is_ok_and(|status| status.success()),
        "build barn-exec and barn-luau-stall failed"
    );

    assert!(
        target.join("barn-exec").is_file(),
        "barn-exec must be built"
    );
    assert!(
        target.join("barn-luau-stall").is_file(),
        "barn-luau-stall must be built"
    );

    (target.join("barn-exec"), target.join("barn-luau-stall"))
}

#[tokio::test]
async fn a_granted_non_debian_library_dir_refuses_a_write() {
    if !in_ci() || !cfg!(target_os = "linux") {
        return;
    }
    let Some(library) = non_debian_library_dir() else {
        return;
    };
    let scratch = Scratch::new().expect("scratch folder");
    let (helper, _) = binaries();
    let program = env::current_exe().expect("fixture executable");
    let spec = StallSpec {
        name: "fixture".to_owned(),
        program: program.clone(),
        args: ["--exact", "non_debian_write_fixture"]
            .map(Into::into)
            .to_vec(),
        grant: Grant {
            executable: Some(program),
            ..Grant::default()
        },
    };
    assert!(spec.grant.allows_read(&library) && !spec.grant.allows_write(&library));
    let supervisor = Supervisor::new(scratch.state(), helper);
    let mut stall = supervisor
        .start(spec)
        .await
        .expect("start the fixture under barn-exec");
    // `accept` checks the child's pid and spends the token its start frame
    // carried, so this proves the exec'd test binary authenticated.
    let mut connection = timeout(REPLY_DEADLINE, stall.accept())
        .await
        .expect("stall.start deadline")
        .expect("authenticate the fixture");

    let health = json!({"jsonrpc":"2.0","id":1,"method":method::STALL_HEALTH});
    send(&mut connection, &health).await.expect("send health");
    assert_eq!(
        receive(&mut connection).await.expect("health reply"),
        json!({"jsonrpc":"2.0","id":1,"result":null})
    );

    let request = json!({
        "jsonrpc":"2.0", "id":2, "method":method::REQUEST,
        "params":{"path":write_target(&library)}
    });
    send(&mut connection, &request)
        .await
        .expect("send the write request");

    assert_eq!(
        receive(&mut connection).await.expect("write reply"),
        json!({
            "jsonrpc":"2.0", "id":2,
            "result":{"kind":"PermissionDenied","errno":EACCES}
        })
    );
}

/// The test binary acts as a stall when barn-exec launches it with the
/// handshake environment. The write is a real syscall under the sandbox; its
/// error and raw errno go back in the reply.
#[test]
fn non_debian_write_fixture() {
    let Some(socket) = env::var_os("BARN_SOCKET").map(PathBuf::from) else {
        return;
    };
    let token = env::var("BARN_TOKEN").expect("start token");
    let mut client = connect(&socket, &token).expect("connect to barn");

    let start = read_message(&mut client).expect("start reply");
    assert_eq!(start["id"], json!("start"));
    assert!(start["result"]["state"].is_string());

    for _ in 0..3 {
        let Some(message) = read_message(&mut client) else {
            return;
        };
        let id = message["id"].clone();

        match message["method"].as_str() {
            Some(method::STALL_HEALTH) => {
                let reply = json!({"jsonrpc":"2.0","id":id,"result":null});
                writeln!(client.get_mut(), "{reply}").expect("send health reply");
            }
            Some(method::REQUEST) => {
                let path =
                    PathBuf::from(message["params"]["path"].as_str().expect("the write path"));
                let result = match fs::write(&path, b"barn") {
                    Ok(()) => json!({"kind":"Written","errno":Value::Null}),
                    Err(error) => json!({
                        "kind":format!("{:?}", error.kind()),
                        "errno":error.raw_os_error(),
                    }),
                };
                let reply = json!({"jsonrpc":"2.0","id":id,"result":result});
                writeln!(client.get_mut(), "{reply}").expect("send write reply");

                return;
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }
}

/// The Fedora job runs the real stall beside the sandbox proof, so its
/// handshake and health path are covered on a non-Debian host too.
#[tokio::test]
async fn a_real_luau_stall_starts_and_answers_health() {
    if !in_ci() || !cfg!(target_os = "linux") {
        return;
    }
    if non_debian_library_dir().is_none() {
        return;
    }
    let scratch = Scratch::new().expect("scratch folder");
    let (helper, program) = binaries();
    let plugin = scratch.0.join("plugin");
    fs::create_dir(&plugin).expect("plugin folder");
    fs::write(
        plugin.join("plugin.luau"),
        "return { name = 'fixture', tools = {} }\n",
    )
    .expect("plugin module");
    let spec = StallSpec {
        name: "stall".to_owned(),
        program: program.clone(),
        args: vec![plugin.clone().into_os_string()],
        grant: Grant {
            executable: Some(program),
            read_paths: vec![plugin.clone()],
            write_paths: vec![plugin],
            ..Grant::default()
        },
    };
    let supervisor = Supervisor::new(scratch.state(), helper);
    let mut stall = supervisor
        .start(spec)
        .await
        .expect("start the real stall under barn-exec");
    let mut connection = timeout(REPLY_DEADLINE, stall.accept())
        .await
        .expect("stall.start deadline")
        .expect("authenticate the real stall");

    let health = json!({"jsonrpc":"2.0","id":1,"method":method::STALL_HEALTH});
    send(&mut connection, &health).await.expect("send health");
    assert_eq!(
        receive(&mut connection).await.expect("health reply"),
        json!({"jsonrpc":"2.0","id":1,"result":null})
    );
}

async fn send(client: &mut AsyncBufReader<AsyncUnixStream>, message: &Value) -> io::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');

    client.get_mut().write_all(&line).await
}

async fn receive(client: &mut AsyncBufReader<AsyncUnixStream>) -> io::Result<Value> {
    let mut line = String::new();
    let read = timeout(REPLY_DEADLINE, client.read_line(&mut line))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "stall reply deadline"))??;

    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "stall closed before its reply",
        ));
    }

    serde_json::from_str(&line).map_err(io::Error::other)
}

fn connect(socket: &Path, token: &str) -> io::Result<BufReader<UnixStream>> {
    let mut stream = UnixStream::connect(socket)?;
    let deadline = Some(Duration::from_secs(35));
    stream.set_read_timeout(deadline)?;
    stream.set_write_timeout(deadline)?;
    let start = json!({
        "jsonrpc":"2.0", "id":"start", "method":method::STALL_START,
        "params":{"protocol":1,"token":token,"jobs":{}}
    });
    writeln!(stream, "{start}")?;

    Ok(BufReader::new(stream))
}

fn read_message(client: &mut BufReader<UnixStream>) -> Option<Value> {
    let mut line = String::new();
    client.read_line(&mut line).ok()?;

    serde_json::from_str(&line).ok()
}
