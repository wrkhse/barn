//! Plugin realms.
//!
//! Each plugin type's JavaScript runs in its own QuickJS realm on a dedicated
//! OS thread, so the tokio runtime never executes plugin code and a runaway
//! poll cannot stall the lane or the HTTP server. `STALL_SCHEDULER=realm` (the
//! default) gives every plugin type its own thread, so polls of different types
//! run in parallel and polls of the same type serialise inside their realm;
//! `STALL_SCHEDULER=seq` gives every realm to one thread that runs polls one at
//! a time, mirroring candidate A's sequential tick.
//!
//! A poll is synchronous from JavaScript's point of view: the prelude's http
//! wrapper calls a native host function which blocks this realm thread until
//! the tokio side answers. That keeps bundles free of async plumbing and lets
//! the realm's interrupt handler bound plugin code that never yields.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rquickjs::{Context, Ctx, Function, Object, Promise, Runtime, Value as JsValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc as async_mpsc, oneshot, Semaphore};

use crate::lane::Lane;
use crate::model::{self, Monitor, Source, SourceEvent, SourceType, Target};

const PRELUDE: &str = include_str!("prelude.js");
const MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
/// Jobs waiting for a realm. The scheduler keeps at most one job per monitor in
/// flight, so this is far above the workload and `submit` never blocks.
const JOB_QUEUE_CAPACITY: usize = 8192;
/// Broker calls in flight at once across all realms.
const HOST_CALL_CONCURRENCY: usize = 256;
pub const BROKER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fault {
    #[serde(rename = "match")]
    pub pattern: String,
    pub mode: FaultMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FaultMode {
    Throw,
    Loop,
}

/// One outbound HTTP request a realm is waiting on.
pub struct HostCall {
    pub plugin: String,
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
    pub reply: oneshot::Sender<Result<NetResponse, String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetResponse {
    #[serde(default)]
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: String,
}

#[derive(Clone)]
pub struct WorkerConfig {
    pub plugin_dir: PathBuf,
    pub faults: Vec<Fault>,
    pub deadline: Duration,
    pub completions: async_mpsc::UnboundedSender<Completion>,
    pub host_calls: async_mpsc::UnboundedSender<HostCall>,
}

pub struct Job {
    pub monitor: Monitor,
}

pub struct Completion {
    pub monitor_id: String,
    pub target: Target,
    pub source: Source,
    pub source_type: SourceType,
    pub started_at_ms: i64,
    pub duration_ms: u64,
    pub outcome: Result<PollOutput, String>,
}

#[derive(Debug, Deserialize)]
pub struct PollOutput {
    #[serde(default)]
    pub cursor: Value,
    #[serde(default)]
    pub events: Vec<SourceEvent>,
}

#[derive(Clone)]
pub struct JobRouter {
    senders: HashMap<SourceType, SyncSender<Job>>,
}

impl JobRouter {
    pub fn submit(&self, source_type: SourceType, job: Job) -> Result<(), String> {
        let Some(sender) = self.senders.get(&source_type) else {
            return Err(format!("no realm worker for source type {source_type}"));
        };
        sender.try_send(job).map_err(|error| match error {
            TrySendError::Full(_) => format!("{source_type} job queue is full"),
            TrySendError::Disconnected(_) => format!("{source_type} realm worker stopped"),
        })
    }
}

pub struct Workers {
    pub router: JobRouter,
    interruptions: Arc<AtomicU64>,
}

impl Workers {
    pub fn interruptions(&self) -> u64 {
        self.interruptions.load(Ordering::Relaxed)
    }
}

/// One OS thread per plugin type.
pub fn spawn_per_type(config: WorkerConfig) -> Result<Workers, String> {
    let mut senders = HashMap::new();
    let interruptions = Arc::new(AtomicU64::new(0));

    for source_type in SourceType::ALL {
        let (sender, receiver) = mpsc::sync_channel(JOB_QUEUE_CAPACITY);
        let thread_config = config.clone();
        let thread_interruptions = interruptions.clone();
        std::thread::Builder::new()
            .name(format!("realm-{source_type}"))
            .spawn(move || worker_loop(receiver, thread_config, thread_interruptions))
            .map_err(|error| format!("spawn realm thread for {source_type}: {error}"))?;
        senders.insert(source_type, sender);
    }

    Ok(Workers {
        router: JobRouter { senders },
        interruptions,
    })
}

/// One OS thread hosting every realm, running polls strictly one at a time.
pub fn spawn_seq(config: WorkerConfig) -> Result<Workers, String> {
    let (sender, receiver) = mpsc::sync_channel(JOB_QUEUE_CAPACITY);
    let interruptions = Arc::new(AtomicU64::new(0));
    let thread_interruptions = interruptions.clone();

    std::thread::Builder::new()
        .name("realm-sequential".to_string())
        .spawn(move || worker_loop(receiver, config, thread_interruptions))
        .map_err(|error| format!("spawn sequential realm thread: {error}"))?;

    let mut senders = HashMap::new();
    for source_type in SourceType::ALL {
        senders.insert(source_type, sender.clone());
    }
    Ok(Workers {
        router: JobRouter { senders },
        interruptions,
    })
}

/// Forward every realm's host call to the broker, replying through its oneshot.
pub async fn host_pump(lane: Arc<Lane>, mut calls: async_mpsc::UnboundedReceiver<HostCall>) {
    let permits = Arc::new(Semaphore::new(HOST_CALL_CONCURRENCY));

    while let Some(call) = calls.recv().await {
        let permit = match permits.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                let _ = call
                    .reply
                    .send(Err("host bridge is shutting down".to_string()));
                return;
            }
        };
        let lane = lane.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let params = NetRequestParams {
                plugin: &call.plugin,
                method: &call.method,
                url: &call.url,
                headers: &call.headers,
                body: &call.body,
            };
            let outcome = match serde_json::to_value(&params) {
                Ok(params) => match lane.request("net.request", params, BROKER_TIMEOUT).await {
                    Ok(value) => serde_json::from_value::<NetResponse>(value)
                        .map_err(|error| format!("net.request result is not a response: {error}")),
                    Err(error) => Err(error.to_string()),
                },
                Err(error) => Err(format!("net.request params: {error}")),
            };
            let _ = call.reply.send(outcome);
        });
    }
}

#[derive(Serialize)]
struct NetRequestParams<'a> {
    plugin: &'a str,
    method: &'a str,
    url: &'a str,
    headers: &'a BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: &'a Option<String>,
}

// ---------------------------------------------------------------------------
// Realm worker
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RealmSlot {
    realm: Option<Realm>,
    interruptions: u64,
}

fn worker_loop(receiver: Receiver<Job>, config: WorkerConfig, interruptions: Arc<AtomicU64>) {
    let mut slots: HashMap<SourceType, RealmSlot> = HashMap::new();

    while let Ok(job) = receiver.recv() {
        let source_type = job.monitor.source_type();
        let started = Instant::now();
        let started_at_ms = model::now_ms();
        let outcome = poll_job(
            &mut slots,
            source_type,
            &job.monitor,
            &config,
            &interruptions,
        );

        let completion = Completion {
            monitor_id: job.monitor.id.clone(),
            target: job.monitor.target.clone(),
            source: job.monitor.source.clone(),
            source_type,
            started_at_ms,
            duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            outcome,
        };
        if config.completions.send(completion).is_err() {
            return;
        }
    }
}

fn poll_job(
    slots: &mut HashMap<SourceType, RealmSlot>,
    source_type: SourceType,
    monitor: &Monitor,
    config: &WorkerConfig,
    interruptions: &AtomicU64,
) -> Result<PollOutput, String> {
    let slot = slots.entry(source_type).or_default();

    if slot.realm.is_none() {
        match Realm::load(source_type, config) {
            Ok(realm) => slot.realm = Some(realm),
            Err(error) => return Err(format!("plugin {source_type} realm unavailable: {error}")),
        }
    }

    let started = Instant::now();
    let result = match slot.realm.as_mut() {
        Some(realm) => realm.poll(monitor),
        None => return Err(format!("plugin {source_type} realm unavailable")),
    };

    match result {
        Ok(output) => Ok(output),
        Err(RealmFailure::Interrupted) => {
            let elapsed = started.elapsed().as_millis();
            slot.realm = None;
            slot.interruptions += 1;
            interruptions.fetch_add(1, Ordering::Relaxed);
            crate::log_line(
                "error",
                &format!(
                    "plugin {source_type} realm restarted after {elapsed}ms (poll deadline {}ms)",
                    config.deadline.as_millis()
                ),
            );
            Err(format!("poll interrupted after {elapsed}ms"))
        }
        Err(RealmFailure::Js(message)) => Err(message),
        Err(RealmFailure::Fatal(message)) => {
            slot.realm = None;
            Err(message)
        }
    }
}

enum RealmFailure {
    Interrupted,
    Js(String),
    Fatal(String),
}

struct Realm {
    // The context keeps its own handle to the runtime; this field only makes
    // ownership explicit and drops the realm with its thread slot.
    _runtime: Runtime,
    context: Context,
    clock: Arc<PollClock>,
    deadline: Duration,
}

impl Realm {
    fn load(source_type: SourceType, config: &WorkerConfig) -> Result<Realm, String> {
        let runtime = Runtime::new().map_err(|error| format!("runtime: {error}"))?;
        runtime.set_memory_limit(MEMORY_LIMIT_BYTES);

        let clock = Arc::new(PollClock::new());
        let handler_clock = clock.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || handler_clock.expired())));

        let context = Context::full(&runtime).map_err(|error| format!("context: {error}"))?;
        let path = config.plugin_dir.join(format!("{source_type}.js"));
        let bundle = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read plugin bundle {}: {error}", path.display()))?;
        let realm_config = serde_json::to_string(&json!({
            "plugin": source_type.as_str(),
            "faults": config.faults,
            "env": realm_env(),
        }))
        .map_err(|error| format!("realm config json: {error}"))?;
        let bridge = HostBridge {
            plugin: source_type.as_str().to_string(),
            calls: config.host_calls.clone(),
        };
        let bundle_path = path.display().to_string();

        // Bound bundle evaluation too: a broken (or hostile) bundle must not
        // wedge the realm thread before it ever serves a poll.
        clock.arm(config.deadline);
        let loaded = context.with(|ctx| -> Result<(), String> {
            ctx.globals()
                .set("__stall_cfg", realm_config)
                .map_err(|error| describe_error(&ctx, error, "install realm config"))?;
            let http_host = Function::new(ctx.clone(), move |payload: String| -> String {
                bridge.handle(&payload)
            })
            .map_err(|error| describe_error(&ctx, error, "install http host function"))?;
            ctx.globals()
                .set("__stallHostHttp", http_host)
                .map_err(|error| describe_error(&ctx, error, "install http host function"))?;
            let log_host = Function::new(ctx.clone(), move |level: String, message: String| {
                let level = match level.as_str() {
                    "warn" | "error" | "debug" => level,
                    _ => "info".to_string(),
                };
                crate::log_line(&level, &format!("plugin {source_type}: {message}"));
            })
            .map_err(|error| describe_error(&ctx, error, "install log host function"))?;
            ctx.globals()
                .set("__stallLog", log_host)
                .map_err(|error| describe_error(&ctx, error, "install log host function"))?;
            ctx.eval::<JsValue, _>(PRELUDE)
                .map_err(|error| describe_error(&ctx, error, "prelude"))?;
            ctx.eval::<JsValue, _>(bundle.as_bytes())
                .map_err(|error| describe_error(&ctx, error, &format!("bundle {bundle_path}")))?;
            let defined: bool = ctx
                .eval("typeof globalThis.__stall.pluginPoll === 'function'")
                .map_err(|error| describe_error(&ctx, error, "check pluginPoll"))?;
            if !defined {
                return Err(format!(
                    "plugin bundle {bundle_path} did not define globalThis.__stall.pluginPoll"
                ));
            }
            Ok(())
        });
        if clock.disarm() {
            return Err(format!(
                "plugin bundle {bundle_path} exceeded the {}ms load deadline",
                config.deadline.as_millis()
            ));
        }
        loaded?;

        let malloc_limit = runtime.memory_usage().malloc_limit;
        let accepted = malloc_limit > 0 && malloc_limit as usize <= MEMORY_LIMIT_BYTES;
        crate::log_line(
            "info",
            &format!(
                "plugin {source_type} realm loaded from {} (memory limit {} MiB {}; allocator malloc_limit {} bytes)",
                path.display(),
                MEMORY_LIMIT_BYTES >> 20,
                if accepted { "accepted" } else { "NOT accepted" },
                malloc_limit
            ),
        );

        Ok(Realm {
            _runtime: runtime,
            context,
            clock,
            deadline: config.deadline,
        })
    }

    fn poll(&mut self, monitor: &Monitor) -> Result<PollOutput, RealmFailure> {
        self.clock.arm(self.deadline);
        let input = json!({
            "source": monitor.source.source_json(),
            "cursor": monitor.plugin_cursor(),
            "monitorId": monitor.id,
        });
        let input_json = serde_json::to_string(&input)
            .map_err(|error| RealmFailure::Fatal(format!("poll input json: {error}")))?;

        let result = self.context.with(|ctx| -> Result<String, RealmFailure> {
            let invoke: Function = ctx.globals().get("__stallInvokePoll").map_err(|error| {
                RealmFailure::Fatal(describe_error(&ctx, error, "look up __stallInvokePoll"))
            })?;
            let promise: Promise = invoke.call((input_json,)).map_err(|error| {
                RealmFailure::Fatal(describe_error(&ctx, error, "call pluginPoll"))
            })?;
            promise.finish::<String>().map_err(|error| {
                RealmFailure::Fatal(describe_error(&ctx, error, "pluginPoll result"))
            })
        });

        let fired = self.clock.disarm();
        if fired {
            return Err(RealmFailure::Interrupted);
        }

        let envelope: PollEnvelope = serde_json::from_str(&result?).map_err(|error| {
            RealmFailure::Fatal(format!("pluginPoll returned a malformed envelope: {error}"))
        })?;
        if !envelope.ok {
            return Err(RealmFailure::Js(
                envelope
                    .error
                    .unwrap_or_else(|| "pluginPoll threw".to_string()),
            ));
        }
        envelope
            .result
            .ok_or_else(|| RealmFailure::Js("pluginPoll returned no result".to_string()))
    }
}

#[derive(Deserialize)]
struct PollEnvelope {
    ok: bool,
    #[serde(default)]
    result: Option<PollOutput>,
    #[serde(default)]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Host functions
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HttpRequestPayload {
    method: String,
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Serialize)]
struct HostReply {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<NetResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

struct HostBridge {
    plugin: String,
    calls: async_mpsc::UnboundedSender<HostCall>,
}

impl HostBridge {
    /// Called on the realm thread from inside QuickJS. It blocks that thread
    /// until the tokio side has the broker's answer, which is why it must never
    /// run on a tokio worker.
    fn handle(&self, payload: &str) -> String {
        let request: HttpRequestPayload = match serde_json::from_str(payload) {
            Ok(request) => request,
            Err(error) => return reply_error(format!("bad host http payload: {error}")),
        };

        let (reply, wait) = oneshot::channel();
        let call = HostCall {
            plugin: self.plugin.clone(),
            method: request.method,
            url: request.url,
            headers: request.headers,
            body: request.body,
            reply,
        };
        if self.calls.send(call).is_err() {
            return reply_error("stall host bridge is closed".to_string());
        }

        match wait.blocking_recv() {
            Ok(Ok(response)) => {
                let reply = HostReply {
                    ok: true,
                    response: Some(response),
                    error: None,
                };
                serde_json::to_string(&reply)
                    .unwrap_or_else(|error| reply_error(format!("encode host reply: {error}")))
            }
            Ok(Err(message)) => reply_error(message),
            Err(_) => reply_error("host bridge dropped the request".to_string()),
        }
    }
}

fn reply_error(message: String) -> String {
    let reply = HostReply {
        ok: false,
        response: None,
        error: Some(message),
    };
    serde_json::to_string(&reply)
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"encode\"}".to_string())
}

/// QuickJS calls this regularly while executing bytecode; returning true
/// raises an uncatchable exception that aborts the current poll.
struct PollClock {
    origin: Instant,
    deadline_ms: AtomicU64,
    fired: AtomicBool,
}

impl PollClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            deadline_ms: AtomicU64::new(0),
            fired: AtomicBool::new(false),
        }
    }

    fn arm(&self, deadline: Duration) {
        let now = self.origin.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let budget = deadline.as_millis().min(u64::MAX as u128) as u64;
        self.fired.store(false, Ordering::SeqCst);
        self.deadline_ms
            .store(now.saturating_add(budget), Ordering::SeqCst);
    }

    fn disarm(&self) -> bool {
        self.deadline_ms.store(0, Ordering::SeqCst);
        self.fired.swap(false, Ordering::SeqCst)
    }

    fn expired(&self) -> bool {
        let deadline = self.deadline_ms.load(Ordering::Relaxed);
        if deadline == 0 {
            return false;
        }
        let now = self.origin.elapsed().as_millis().min(u64::MAX as u128) as u64;
        if now >= deadline {
            self.fired.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
}

/// The variables the pinned providers read from `process.env`. Only these are
/// handed to a realm; the Jira base keeps the documented default so the bundles
/// can build issue URLs without a workspace being configured.
fn realm_env() -> Value {
    let mut env = serde_json::Map::new();
    let jira_base = std::env::var("SOURCEFED_JIRA_BASE_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://example.atlassian.net".to_string());
    env.insert(
        "SOURCEFED_JIRA_BASE_URL".to_string(),
        Value::String(jira_base),
    );
    for name in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "ATLASSIAN_EMAIL",
        "ATLASSIAN_API_KEY",
    ] {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                env.insert(name.to_string(), Value::String(value));
            }
        }
    }
    Value::Object(env)
}

fn describe_error(ctx: &Ctx<'_>, error: rquickjs::Error, what: &str) -> String {
    if matches!(error, rquickjs::Error::Exception) {
        return format!("{what}: {}", describe_thrown(&ctx.catch()));
    }
    format!("{what}: {error}")
}

fn describe_thrown(thrown: &JsValue<'_>) -> String {
    if thrown.is_error() {
        if let Ok(object) = Object::from_value(thrown.clone()) {
            let message: Option<String> = object.get("message").ok().flatten();
            let stack: Option<String> = object.get("stack").ok().flatten();
            let message = message.unwrap_or_else(|| "exception".to_string());
            return match stack {
                Some(stack) if !stack.is_empty() => format!("{message}\n{stack}"),
                _ => message,
            };
        }
    }
    if let Ok(text) = thrown.get::<String>() {
        return text;
    }
    "unprintable JavaScript value".to_string()
}
