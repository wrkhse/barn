//! A real `barn stall start`, through the sandbox and the host socket. Like
//! exit.rs this runs in CI only: AGENTS.md forbids starting, signalling or
//! killing a process on the owner's Mac. Without `CI` it returns at once.
//!
//! No workflow edit is needed: the `test` and `test-macos` jobs already run
//! `cargo nextest run --workspace --locked`, which picks up this integration
//! test on both platforms, so a new step would only duplicate it.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use barn_stall::protocol::{HostStartParams, PROTOCOL_VERSION, method};
use serde_json::{Value, json};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Every wait in this test is bounded: a launch that never serves the socket,
/// a host that never answers, or a barn that never exits fails rather than
/// hanging CI.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const STALL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);

/// A scratch state root, a plugin folder with a loadable entry, and the barn
/// child launched from them. stderr is drained on a thread so a named startup
/// failure is reported instead of hidden in an unread pipe. The child and any
/// stall it leaves behind are cleaned up on drop.
struct Launch {
    dir: PathBuf,
    plugin: PathBuf,
    state: PathBuf,
    child: Option<Child>,
    stderr: Arc<Mutex<String>>,
}

/// Unwrap a value that cannot be absent; on failure, print the reason and abort.
fn must_opt<T>(value: Option<T>, what: &str) -> T {
    value.unwrap_or_else(|| {
        eprintln!("{what}");
        std::process::abort()
    })
}

/// Unwrap a value that cannot fail; on failure, print the reason and abort.
fn must_res<T, E>(value: Result<T, E>, what: &str) -> T {
    value.unwrap_or_else(|_error| {
        eprintln!("{what}");
        std::process::abort()
    })
}

impl Launch {
    fn new() -> io::Result<Self> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("barn-launch-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let plugin = dir.join(format!("plugin-{n}"));
        let state = dir.join("state");
        fs::create_dir_all(&plugin)?;
        fs::create_dir_all(&state)?;
        fs::write(
            plugin.join("plugin.luau"),
            "return { name = 'launch', tools = {} }\n",
        )?;

        Ok(Self {
            dir,
            plugin,
            state,
            child: None,
            stderr: Arc::new(Mutex::new(String::new())),
        })
    }

    /// The stall name barn takes from the plugin folder's final component.
    fn stall_name(&self) -> &str {
        must_opt(
            self.plugin.file_name().and_then(|name| name.to_str()),
            "the plugin folder has a name",
        )
    }

    /// Copy barn and both helpers into this test's private `bin` dir and
    /// return the copied barn's path. barn finds its helpers beside the binary
    /// it runs, so the copy must carry all three; the cargo-built binary's
    /// folder stays untouched for other tests. `stall` overrides the runtime
    /// installed as barn-luau-stall, else the built helper is used.
    fn prepare_bin(&self, stall: Option<PathBuf>) -> PathBuf {
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_barn"));
        let bin = self.dir.join("bin");
        must_res(fs::create_dir_all(&bin), "the scratch bin dir");

        let exec = PathBuf::from(env!("CARGO_BIN_EXE_barn-test-exec"));
        let built_stall = PathBuf::from(env!("CARGO_BIN_EXE_barn-test-stall"));
        let stall = stall.unwrap_or(built_stall);

        for (from, name) in [
            (binary, "barn"),
            (exec, "barn-exec"),
            (stall, "barn-luau-stall"),
        ] {
            assert!(
                fs::copy(&from, bin.join(name)).is_ok(),
                "copy {name} into the scratch bin dir"
            );
        }

        bin.join("barn")
    }

    /// Spawn this test's copied barn and return a receiver for one stdout
    /// line. The copy and its helpers live together in the private bin dir.
    fn spawn(&mut self) -> Receiver<io::Result<String>> {
        let barn = self.prepare_bin(None);

        let mut child = must_res(
            Command::new(&barn)
                .args(["stall", "start", "--plugin"])
                .arg(&self.plugin)
                .args(["--daemon-pid", &std::process::id().to_string()])
                .env("BARN_STATE_DIR", &self.state)
                .env("RUST_BACKTRACE", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn(),
            "barn stall start spawns",
        );
        let stdout = must_opt(child.stdout.take(), "the child's stdout is piped");
        let stderr = must_opt(child.stderr.take(), "the child's stderr is piped");
        self.child = Some(child);

        // barn names a failure on stderr as well as stdout. Drain it to a
        // shared buffer so the report reaches the assertion message and an
        // unread pipe cannot block a child that writes a long error.
        let sink = Arc::clone(&self.stderr);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();

            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => must_res(sink.lock(), "the stderr buffer").push_str(&line),
                }
            }
        });

        // A pipe read has no deadline. Read on a thread and bound how long
        // the test waits, so a launch that writes no endpoint fails clearly.
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let result = reader.read_line(&mut line).map(|_| line);
            let _ = sender.send(result);
        });

        receiver
    }

    /// Spawn the copied barn for the SIGTERM test. The drain fixture replaces
    /// the built stall runtime in the shared bin setup, so barn finds both
    /// helpers beside the binary it actually runs. Returns the one endpoint
    /// line.
    fn spawn_with_drain_fixture(&mut self) -> Receiver<io::Result<String>> {
        // cargo builds the fixture as a test-only bin target, so its path is
        // resolved at compile time rather than built by a nested cargo here.
        let fixture = PathBuf::from(env!("CARGO_BIN_EXE_stall_drain_fixture"));

        let barn = self.prepare_bin(Some(fixture));

        let mut child = must_res(
            Command::new(&barn)
                .args(["stall", "start", "--plugin"])
                .arg(&self.plugin)
                .args(["--daemon-pid", &std::process::id().to_string()])
                .env("BARN_STATE_DIR", &self.state)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn(),
            "the copied barn stall start spawns",
        );
        let stdout = must_opt(child.stdout.take(), "the child's stdout is piped");
        let stderr = must_opt(child.stderr.take(), "the child's stderr is piped");
        self.child = Some(child);

        let sink = Arc::clone(&self.stderr);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();

            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => must_res(sink.lock(), "the stderr buffer").push_str(&line),
                }
            }
        });

        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let result = reader.read_line(&mut line).map(|_| line);
            let _ = sender.send(result);
        });

        receiver
    }

    /// Everything barn wrote on stderr, for assertion messages.
    fn stderr(&self) -> String {
        must_res(self.stderr.lock(), "the stderr buffer").clone()
    }

    /// A failure message that carries barn's own report.
    fn failure(&self, what: &str) -> String {
        format!("{what}; stderr: {}", self.stderr())
    }

    /// Read the one endpoint line within the deadline, checked so a named
    /// failure cannot be mistaken for a launch that reached the host socket.
    fn endpoint(&self, line: &Receiver<io::Result<String>>) -> (String, String) {
        let line = must_res(
            must_res(
                line.recv_timeout(LAUNCH_TIMEOUT),
                "barn writes its endpoint within the launch deadline",
            ),
            "reading the endpoint line",
        );
        let offered = parse(&line);

        assert_eq!(
            must_opt(
                offered.get("protocol").cloned(),
                "the launch reply names a protocol"
            ),
            json!(PROTOCOL_VERSION),
            "{}",
            self.failure("an unexpected launch protocol")
        );
        assert!(
            offered.get("error").is_none(),
            "{}",
            self.failure(&format!("barn refused the launch: {offered}"))
        );

        let socket = must_opt(
            offered.get("socket_path").and_then(Value::as_str),
            "the endpoint names a socket",
        );
        let token = must_opt(
            offered.get("token").and_then(Value::as_str),
            "the endpoint carries a token",
        );

        (socket.to_owned(), token.to_owned())
    }

    /// Wait for barn's exit, with a deadline so a stuck process cannot hang.
    fn wait_exit(&mut self) -> ExitStatus {
        let started = Instant::now();

        loop {
            let child = must_opt(self.child.as_mut(), "barn is running");
            if let Some(status) = must_res(child.try_wait(), "polling barn's exit") {
                return status;
            }
            assert!(
                started.elapsed() < EXIT_TIMEOUT,
                "{}",
                self.failure("barn did not exit after the host closed")
            );
            std::thread::sleep(POLL);
        }
    }

    /// Live stalls for this scratch plugin, read from the argv barn-exec
    /// passed to the runtime. The run dir holds no pid file and barn keeps its
    /// supervisor state private, so this unique folder is the observable
    /// handle. Zombies have already exited and are not live processes.
    fn stall_pids(&self) -> Vec<u32> {
        let output = must_res(
            Command::new("ps")
                .args(["-axww", "-o", "pid=", "-o", "stat=", "-o", "command="])
                .output(),
            "ps lists the process table",
        );

        let stdout = must_res(String::from_utf8(output.stdout), "ps output is UTF-8");
        let plugin = must_opt(self.plugin.to_str(), "the plugin path is UTF-8");

        stdout
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid: u32 = fields.next()?.parse().ok()?;
                let state = fields.next()?;
                let command = fields.collect::<Vec<_>>().join(" ");

                if state.starts_with('Z') {
                    return None;
                }
                if command.contains("barn-luau-stall") && command.contains(plugin) {
                    Some(pid)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Wait for one stall to leave the process table after barn drains it.
    /// A zombie has already exited, so it does not count as a live stall.
    fn wait_stall_gone(&self, pid: u32) -> bool {
        let started = Instant::now();

        while started.elapsed() < STALL_EXIT_TIMEOUT {
            if !self.stall_pids().contains(&pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        !self.stall_pids().contains(&pid)
    }

    /// Best-effort teardown for any path that did not end in a clean exit:
    /// SIGTERM first, SIGKILL for a peer that ignores it. Drop must not panic,
    /// so each signal and wait is bounded and its result is ignored.
    fn cleanup(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if child.try_wait().ok().flatten().is_none() {
                signal("-TERM", child.id());
                if !wait_child_exit(child) {
                    let _ = child.kill();
                }
            }
            let _ = child.wait();
        }

        for pid in self.stall_pids() {
            signal("-TERM", pid);
            if !wait_gone(pid) {
                signal("-KILL", pid);
            }
        }
    }
}

impl Drop for Launch {
    fn drop(&mut self) {
        self.cleanup();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn signal(flag: &str, pid: u32) {
    let _ = Command::new("kill").args([flag, &pid.to_string()]).status();
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

fn wait_child_exit(child: &mut Child) -> bool {
    let started = Instant::now();

    while started.elapsed() < SIGNAL_TIMEOUT {
        if child.try_wait().ok().flatten().is_some() {
            return true;
        }
        std::thread::sleep(POLL);
    }

    false
}

fn wait_gone(pid: u32) -> bool {
    let started = Instant::now();

    while started.elapsed() < SIGNAL_TIMEOUT {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(POLL);
    }

    !alive(pid)
}

fn read_line(reader: &mut impl BufRead) -> String {
    let mut line = String::new();
    must_res(reader.read_line(&mut line), "a reply line");
    line
}

/// On a failed health reply read, abort the stall so its SIGABRT lands in
/// barn's captured stderr with a backtrace, then fail the test.
fn read_line_with_stall_abort(reader: &mut impl BufRead, stall_pid: u32) -> String {
    let mut line = String::new();

    match reader.read_line(&mut line) {
        Ok(_) => line,
        Err(error) => {
            eprintln!("HOST TRACE health read error {error:?}; aborting stall {stall_pid}");
            let _ = Command::new("kill")
                .args(["-ABRT", &stall_pid.to_string()])
                .status();
            panic!("stall {stall_pid} aborted: {error}");
        }
    }
}

fn parse(line: &str) -> Value {
    must_res(serde_json::from_str(line), "one JSON line")
}

/// A bounded read and write so a host that never answers fails with a clear
/// message instead of hanging the test.
fn connect(socket: &str) -> BufReader<UnixStream> {
    let stream = must_res(UnixStream::connect(socket), "connect to host.sock");
    must_res(
        stream.set_read_timeout(Some(REPLY_TIMEOUT)),
        "a read deadline",
    );
    must_res(
        stream.set_write_timeout(Some(REPLY_TIMEOUT)),
        "a write deadline",
    );

    BufReader::new(stream)
}

#[test]
fn a_launched_stall_serves_one_host_connection_then_exits_cleanly() {
    if std::env::var_os("CI").is_none() {
        return;
    }

    let mut launch = Launch::new().expect("the scratch tree");
    let line = launch.spawn();
    let (socket, token) = launch.endpoint(&line);
    let mut host = connect(&socket);
    let t0 = Instant::now();
    eprintln!("HOST TRACE connect {:?}", t0.elapsed());

    let start = json!({
        "jsonrpc": "2.0",
        "id": 21,
        "method": method::HOST_START,
        "params": HostStartParams {
            protocol: PROTOCOL_VERSION,
            token,
            pid: std::process::id(),
        },
    });
    writeln!(host.get_mut(), "{start}").expect("send host.start");

    assert_eq!(
        parse(&read_line(&mut host)),
        json!({
            "jsonrpc": "2.0",
            "id": 21,
            "result": {"stall": launch.stall_name()},
        })
    );

    eprintln!("HOST TRACE reader-ready-check {:?}", t0.elapsed());

    // One connection owns the launch: the real stall is live while the host
    // drives it, and closing that host ends barn cleanly without a stall left
    // behind.
    let running = launch.stall_pids();
    assert_eq!(
        running.len(),
        1,
        "{}",
        launch.failure("one stall serves the host before it closes")
    );
    let stall_pid = running.first().copied().expect("one stall pid");
    eprintln!("HOST TRACE stall-pid {:?} {stall_pid}", t0.elapsed());

    let health = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method::STALL_HEALTH,
    });
    eprintln!("HOST TRACE health-send {:?}", t0.elapsed());
    writeln!(host.get_mut(), "{health}").expect("send stall.health");
    eprintln!("HOST TRACE health-written {:?}", t0.elapsed());

    eprintln!("HOST TRACE health-reply-wait {:?}", t0.elapsed());
    let health_reply = parse(&read_line_with_stall_abort(&mut host, stall_pid));
    eprintln!("HOST TRACE health-reply-got {:?}", t0.elapsed());
    assert_eq!(
        health_reply,
        json!({"jsonrpc": "2.0", "id": 1, "result": null}),
        "{}",
        launch.failure("stall.health reply")
    );

    drop(host);

    let status = launch.wait_exit();
    eprintln!("HOST TRACE exit {:?}", t0.elapsed());
    assert_eq!(
        status.code(),
        Some(0),
        "{}",
        launch.failure("barn exits zero after its host closes")
    );
    assert!(
        launch.wait_stall_gone(stall_pid),
        "{}",
        launch.failure(&format!("stall {stall_pid} is gone"))
    );

    launch.cleanup();
    assert!(
        launch.stall_pids().is_empty(),
        "{}",
        launch.failure("no stall process is left behind")
    );
}

#[test]
fn a_sigterm_drains_the_stall_and_removes_the_run_dir() {
    if std::env::var_os("CI").is_none() {
        return;
    }

    let mut launch = Launch::new().expect("the scratch tree");
    let line = launch.spawn_with_drain_fixture();
    let (socket, token) = launch.endpoint(&line);
    let mut host = connect(&socket);

    let start = json!({
        "jsonrpc": "2.0",
        "id": 21,
        "method": method::HOST_START,
        "params": HostStartParams {
            protocol: PROTOCOL_VERSION,
            token,
            pid: std::process::id(),
        },
    });
    writeln!(host.get_mut(), "{start}").expect("send host.start");

    assert_eq!(
        parse(&read_line(&mut host)),
        json!({
            "jsonrpc": "2.0",
            "id": 21,
            "result": {"stall": launch.stall_name()},
        })
    );

    // The fixture is the fake stall; its pid is the process barn drains on
    // SIGTERM, matched on the unique plugin path in its argv.
    let running = launch.stall_pids();
    assert_eq!(
        running.len(),
        1,
        "{}",
        launch.failure("one stall serves the host before SIGTERM")
    );
    let stall_pid = running.first().copied().expect("one stall pid");

    let child_id = launch
        .child
        .as_ref()
        .expect("the copied barn is running")
        .id();
    signal("-TERM", child_id);

    assert!(
        launch.wait_exit().success(),
        "{}",
        launch.failure("barn exits zero after SIGTERM")
    );

    assert_eq!(
        fs::read_to_string(launch.plugin.join("drain.marker")).expect("the drain marker"),
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"stall.drain\"}\n"
    );

    assert!(
        !launch.state.join("run").join(launch.stall_name()).exists(),
        "{}",
        launch.failure("the run dir is gone after SIGTERM")
    );

    assert!(
        launch.wait_stall_gone(stall_pid),
        "{}",
        launch.failure(&format!("stall {stall_pid} is gone"))
    );

    launch.cleanup();
    assert!(
        launch.stall_pids().is_empty(),
        "{}",
        launch.failure("no stall process is left behind")
    );
}
