//! stall-b: the process entry point.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use stall_b::model::VERSION;
use stall_b::plugin::{self, Workers};
use stall_b::sse::Hub;
use stall_b::stall::{self, Inbound, Protocol};
use stall_b::store::{self, Store};
use stall_b::{http, log_line, schedule};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SchedulerMode {
    Realm,
    Seq,
}

impl SchedulerMode {
    fn as_str(self) -> &'static str {
        match self {
            SchedulerMode::Realm => "realm",
            SchedulerMode::Seq => "seq",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StoreKind {
    Json,
    Memory,
}

impl StoreKind {
    fn as_str(self) -> &'static str {
        match self {
            StoreKind::Json => "json",
            StoreKind::Memory => "memory",
        }
    }
}

struct Env {
    plugin_dir: PathBuf,
    state_dir: Option<PathBuf>,
    scheduler: SchedulerMode,
    store: StoreKind,
    tick: Duration,
    poll_deadline: Duration,
}

impl Env {
    fn from_env() -> Result<Env, String> {
        let plugin_dir = match std::env::var("STALL_PLUGIN_DIR") {
            Ok(value) if !value.is_empty() => PathBuf::from(value),
            _ => return Err("STALL_PLUGIN_DIR is required".to_string()),
        };
        if !plugin_dir.is_dir() {
            return Err(format!(
                "STALL_PLUGIN_DIR {} is not a directory",
                plugin_dir.display()
            ));
        }

        let state_dir = match std::env::var("STALL_STATE_DIR") {
            Ok(value) if !value.is_empty() => Some(PathBuf::from(value)),
            _ => None,
        };

        let scheduler = match std::env::var("STALL_SCHEDULER").as_deref() {
            Ok("realm") | Err(_) => SchedulerMode::Realm,
            Ok("seq") => SchedulerMode::Seq,
            Ok(other) => {
                return Err(format!(
                    "STALL_SCHEDULER must be realm or seq, got {other:?}"
                ))
            }
        };

        let store = match std::env::var("STALL_STORE").as_deref() {
            Ok("json") | Err(_) => StoreKind::Json,
            Ok("memory") => StoreKind::Memory,
            Ok(other) => return Err(format!("STALL_STORE must be json or memory, got {other:?}")),
        };

        Ok(Env {
            plugin_dir,
            state_dir,
            scheduler,
            store,
            tick: Duration::from_millis(env_millis("STALL_TICK_MS", 5000)?),
            poll_deadline: Duration::from_millis(env_millis("STALL_POLL_DEADLINE_MS", 10_000)?),
        })
    }
}

fn env_millis(name: &str, default: u64) -> Result<u64, String> {
    match std::env::var(name) {
        Ok(raw) if !raw.is_empty() => {
            let value: u64 = raw
                .parse()
                .map_err(|_| format!("{name} must be a positive integer, got {raw:?}"))?;
            if value == 0 {
                Err(format!("{name} must be greater than zero"))
            } else {
                Ok(value)
            }
        }
        _ => Ok(default),
    }
}

struct System {
    store: Arc<dyn Store>,
    inflight: Arc<AtomicUsize>,
    port: u16,
    state_dir: PathBuf,
    workers: Workers,
}

fn main() {
    let env = match Env::from_env() {
        Ok(env) => env,
        Err(error) => {
            log_line("error", &format!("startup: {error}"));
            std::process::exit(1);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log_line("error", &format!("tokio runtime: {error}"));
            std::process::exit(1);
        }
    };

    match runtime.block_on(run(env)) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            log_line("error", &format!("fatal: {error}"));
            std::process::exit(1);
        }
    }
}

async fn run(env: Env) -> Result<(), String> {
    let (protocol, read_half) = stall::open_fd3()?;
    let protocol = Arc::new(protocol);
    let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel();
    tokio::spawn(stall::read_loop(read_half, inbound_tx));

    let (host_tx, host_rx) = mpsc::unbounded_channel();
    tokio::spawn(plugin::host_pump(protocol.clone(), host_rx));

    let mut system: Option<Arc<System>> = None;

    loop {
        let Some(inbound) = inbound_rx.recv().await else {
            log_line("warn", "stall protocol closed; exiting");
            return Ok(());
        };

        match inbound {
            Inbound::Response { id, result } => {
                if !protocol.deliver(id, result) {
                    log_line(
                        "warn",
                        &format!("stall protocol response for unknown request id {id}"),
                    );
                }
            }
            Inbound::Request { id, method, params } => {
                handle_stall_request(&env, &protocol, &host_tx, &mut system, id, method, params)
                    .await;
            }
            Inbound::Notification { method, params } => {
                log_line(
                    "info",
                    &format!(
                        "stall protocol notification {method}: {}",
                        truncate(&params.to_string(), 512)
                    ),
                );
            }
            Inbound::Malformed(message) => {
                log_line(
                    "error",
                    &format!("stall protocol message dropped: {message}"),
                );
            }
        }
    }
}

async fn handle_stall_request(
    env: &Env,
    protocol: &Arc<Protocol>,
    host_tx: &mpsc::UnboundedSender<plugin::HostCall>,
    system: &mut Option<Arc<System>>,
    id: Value,
    method: String,
    params: Value,
) {
    match method.as_str() {
        "stall.start" => {
            if system.is_some() {
                protocol
                    .respond_error(id, -32600, "stall.start was already handled")
                    .await;
                return;
            }
            match start_system(env, host_tx, params).await {
                Ok((ready, new_system)) => {
                    *system = Some(new_system);
                    protocol.respond(id, ready).await;
                }
                Err(error) => {
                    log_line("error", &format!("stall.start: {error}"));
                    protocol.respond_error(id, -32600, &error).await;
                }
            }
        }
        "stall.health" => match system {
            Some(system) => {
                let scoped = system.store.clone();
                let monitors = tokio::task::spawn_blocking(move || scoped.monitor_count()).await;
                let inflight = system.inflight.load(Ordering::Relaxed);
                match monitors {
                    Ok(Ok(monitors)) => {
                        protocol
                            .respond(
                                id,
                                json!({ "ok": true, "monitors": monitors, "inflight": inflight }),
                            )
                            .await;
                    }
                    Ok(Err(error)) => {
                        protocol
                            .respond_error(id, -32603, &format!("health: {error}"))
                            .await;
                    }
                    Err(error) => {
                        protocol
                            .respond_error(id, -32603, &format!("health task: {error}"))
                            .await;
                    }
                }
            }
            None => {
                protocol
                    .respond_error(id, -32600, "stall.start has not run")
                    .await;
            }
        },
        "stall.drain" => {
            if let Some(system) = system {
                log_line(
                    "info",
                    &format!(
                        "drain requested: interruptions={} port={} state={}",
                        system.workers.interruptions(),
                        system.port,
                        system.state_dir.display()
                    ),
                );
            }
            protocol.respond(id, json!({ "ok": true })).await;
            std::process::exit(0);
        }
        other => {
            log_line("error", &format!("unknown stall method {other}"));
            protocol
                .respond_error(id, -32601, &format!("unknown stall method {other}"))
                .await;
        }
    }
}

async fn start_system(
    env: &Env,
    host_tx: &mpsc::UnboundedSender<plugin::HostCall>,
    params: Value,
) -> Result<(Value, Arc<System>), String> {
    let params: StartParams =
        serde_json::from_value(params).map_err(|error| format!("stall.start params: {error}"))?;
    if params.protocol != 1 {
        return Err(format!(
            "unsupported stall protocol {} (this stall speaks 1)",
            params.protocol
        ));
    }
    if params.stall != "sourcefed" {
        return Err(format!("unknown stall {:?}", params.stall));
    }
    let state_dir = params
        .state
        .or_else(|| env.state_dir.clone())
        .ok_or_else(|| {
            "stall.start gives no state directory and STALL_STATE_DIR is unset".to_string()
        })?;

    let store = match env.store {
        StoreKind::Json => store::open_json(&state_dir)
            .map_err(|error| format!("state dir {}: {error}", state_dir.display()))?,
        StoreKind::Memory => store::open_memory(),
    };
    let hub = Arc::new(Hub::new());
    let inflight = Arc::new(AtomicUsize::new(0));
    let (completions_tx, completions_rx) = mpsc::unbounded_channel();

    let faults = params.faults.len();
    let config = plugin::WorkerConfig {
        plugin_dir: env.plugin_dir.clone(),
        faults: params.faults,
        deadline: env.poll_deadline,
        completions: completions_tx,
        host_calls: host_tx.clone(),
    };
    let workers = match env.scheduler {
        SchedulerMode::Realm => plugin::spawn_per_type(config)?,
        SchedulerMode::Seq => plugin::spawn_seq(config)?,
    };

    let listener = tokio::net::TcpListener::bind((params.http.host.as_str(), params.http.port))
        .await
        .map_err(|error| format!("bind {}:{}: {error}", params.http.host, params.http.port))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("listener address: {error}"))?
        .port();

    tokio::spawn(http::serve(
        listener,
        Arc::new(http::HttpState {
            store: store.clone(),
            hub: hub.clone(),
        }),
    ));
    tokio::spawn(schedule::run(
        store.clone(),
        hub.clone(),
        workers.router.clone(),
        inflight.clone(),
        completions_rx,
        env.tick,
    ));

    log_line(
        "info",
        &format!(
            "{VERSION} ready on http://{}:{port} state={} scheduler={} store={} plugins={} faults={faults} tick={}ms deadline={}ms",
            params.http.host,
            state_dir.display(),
            env.scheduler.as_str(),
            env.store.as_str(),
            env.plugin_dir.display(),
            env.tick.as_millis(),
            env.poll_deadline.as_millis(),
        ),
    );

    let ready = json!({
        "ready": true,
        "version": VERSION,
        "http": { "port": port },
    });
    Ok((
        ready,
        Arc::new(System {
            store,
            inflight,
            port,
            state_dir,
            workers,
        }),
    ))
}

#[derive(Deserialize)]
struct StartParams {
    protocol: u32,
    stall: String,
    #[serde(default)]
    state: Option<PathBuf>,
    http: HttpListen,
    #[serde(default)]
    faults: Vec<plugin::Fault>,
}

#[derive(Deserialize)]
struct HttpListen {
    host: String,
    port: u16,
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}
