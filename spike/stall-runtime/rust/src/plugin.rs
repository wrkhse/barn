//! Plugin realms: which thread runs a poll, and what the poll is asked.
//!
//! Each plugin type's JavaScript runs in its own realm on a dedicated OS thread, so the
//! tokio runtime never executes plugin code and a runaway poll cannot stall the protocol or
//! the HTTP server. `STALL_SCHEDULER=realm` (the default) gives every plugin type its own
//! thread, so polls of different types run in parallel and polls of the same type serialise
//! inside their realm; `STALL_SCHEDULER=seq` gives every realm to one thread that runs
//! polls one at a time, mirroring candidate A's sequential tick.
//!
//! A realm is an `engine::Engine` and nothing here knows which engine it is: this module
//! builds a poll's input, keeps one realm per plugin type, decodes the envelope the poll
//! answers with, and owns the host bridge. A poll is synchronous from JavaScript's point of
//! view: the prelude's http wrapper awaits a host function which blocks this realm thread
//! until the tokio side answers, and the engine's budget bounds plugin code that never
//! yields.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc as async_mpsc, oneshot, Semaphore};

use crate::engine::{self, Engine, Failure};
use crate::model::{self, Monitor, Source, SourceEvent, SourceType, Target};
use crate::stall::Protocol;

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
pub async fn host_pump(
    protocol: Arc<Protocol>,
    mut calls: async_mpsc::UnboundedReceiver<HostCall>,
) {
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
        let protocol = protocol.clone();
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
                Ok(params) => match protocol
                    .request("net.request", params, BROKER_TIMEOUT)
                    .await
                {
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
    realm: Option<Box<dyn Engine>>,
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
        match engine::realm(source_type, config) {
            Ok(realm) => slot.realm = Some(realm),
            Err(error) => return Err(format!("plugin {source_type} realm unavailable: {error}")),
        }
    }

    let input = poll_input(monitor)?;
    let started = Instant::now();
    let result = match slot.realm.as_mut() {
        Some(realm) => realm.poll(&input),
        None => return Err(format!("plugin {source_type} realm unavailable")),
    };

    // The engine returns the prelude's envelope, so the decoding is the same for both.
    match result.and_then(|envelope| decode_envelope(&envelope)) {
        Ok(output) => Ok(output),
        Err(Failure::Interrupted) => {
            // The engine has already recovered the realm, so only the poll is lost.
            let elapsed = started.elapsed().as_millis();
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
        Err(Failure::Js(message)) => Err(message),
        Err(Failure::Fatal(message)) => {
            slot.realm = None;
            Err(message)
        }
    }
}

/// What `__stallInvokePoll` parses: the monitor's source, its cursor and its id.
fn poll_input(monitor: &Monitor) -> Result<String, String> {
    let input = json!({
        "source": monitor.source.source_json(),
        "cursor": monitor.plugin_cursor(),
        "monitorId": monitor.id,
    });
    serde_json::to_string(&input).map_err(|error| format!("poll input json: {error}"))
}

/// The envelope the prelude always answers with. A malformed one means the bundle replaced
/// `__stallInvokePoll` or the engine lost the result, so the realm is dropped rather than
/// reused; a reported failure is the plugin's own throw and leaves it usable.
fn decode_envelope(envelope: &str) -> Result<PollOutput, Failure> {
    let envelope: PollEnvelope = serde_json::from_str(envelope).map_err(|error| {
        Failure::Fatal(format!("pluginPoll returned a malformed envelope: {error}"))
    })?;
    if !envelope.ok {
        return Err(Failure::Js(
            envelope
                .error
                .unwrap_or_else(|| "pluginPoll threw".to_string()),
        ));
    }
    envelope
        .result
        .ok_or_else(|| Failure::Js("pluginPoll returned no result".to_string()))
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

/// The realm side of the broker bridge, shared by both engines.
pub(crate) struct HostBridge {
    plugin: String,
    calls: async_mpsc::UnboundedSender<HostCall>,
}

impl HostBridge {
    pub(crate) fn new(plugin: SourceType, calls: async_mpsc::UnboundedSender<HostCall>) -> Self {
        Self {
            plugin: plugin.as_str().to_string(),
            calls,
        }
    }

    /// Called on the realm thread from inside the engine. It blocks that thread
    /// until the tokio side has the broker's answer, which is why it must never
    /// run on a tokio worker.
    pub(crate) fn handle(&self, payload: &str) -> String {
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

/// The variables the pinned providers read from `process.env`. Only these are
/// handed to a realm; the Jira base keeps the documented default so the bundles
/// can build issue URLs without a workspace being configured.
pub(crate) fn realm_env() -> Value {
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
