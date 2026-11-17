//! `POST /rpc`: the daemon API the harness's clients use.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use hyper::http::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::http::HttpState;
use crate::model::{self, Monitor, Source, Target, VERSION};
use crate::store::StoreError;

#[derive(Deserialize)]
struct RpcRequest {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

pub struct RpcFailure {
    code: i64,
    message: String,
}

impl RpcFailure {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("unknown method {method}"),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }
}

pub async fn handle(state: &Arc<HttpState>, body: &[u8]) -> (StatusCode, Value) {
    let request: RpcRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                error_document(Value::Null, -32700, format!("malformed JSON: {error}")),
            )
        }
    };
    let id = request.id.clone().unwrap_or(Value::Null);
    match dispatch(state, request).await {
        Ok(result) => (StatusCode::OK, json!({ "id": id, "result": result })),
        Err(failure) => (
            StatusCode::BAD_REQUEST,
            error_document(id, failure.code, failure.message),
        ),
    }
}

pub fn error_document(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "id": id, "error": { "code": code, "message": message.into() } })
}

async fn dispatch(state: &Arc<HttpState>, request: RpcRequest) -> Result<Value, RpcFailure> {
    match request.method.as_str() {
        "daemon.health" => Ok(json!({ "ok": true, "version": VERSION, "protocol": 1 })),
        "monitor.create" => create_monitor(state, request.params).await,
        "monitor.ack" => ack(state, request.params).await,
        other => Err(RpcFailure::method_not_found(other)),
    }
}

#[derive(Deserialize)]
struct CreateParams {
    target: Target,
    name: String,
    #[serde(rename = "pollIntervalSec", default = "default_interval")]
    poll_interval_sec: u64,
    #[serde(flatten)]
    source: Source,
}

fn default_interval() -> u64 {
    15
}

async fn create_monitor(state: &Arc<HttpState>, params: Value) -> Result<Value, RpcFailure> {
    let params: CreateParams = serde_json::from_value(params)
        .map_err(|error| RpcFailure::invalid_params(format!("monitor.create: {error}")))?;

    if params.name.trim().is_empty() {
        return Err(RpcFailure::invalid_params("monitor.create: name is empty"));
    }
    if params.poll_interval_sec == 0 {
        return Err(RpcFailure::invalid_params(
            "monitor.create: pollIntervalSec must be greater than zero",
        ));
    }
    if params.target.kind.is_empty() || params.target.id.is_empty() {
        return Err(RpcFailure::invalid_params(
            "monitor.create: target needs a kind and an id",
        ));
    }

    let now = model::iso8601_ms(model::now_ms());
    let monitor = Monitor {
        id: new_monitor_id(),
        name: params.name,
        target: params.target,
        source: params.source,
        poll_interval_sec: params.poll_interval_sec,
        cursors: BTreeMap::new(),
        created_at: now.clone(),
        updated_at: now,
    };

    let scoped = state.store.clone();
    let stored = monitor.clone();
    store_call(move || scoped.create_monitor(&stored)).await?;

    Ok(json!({
        "ok": true,
        "created": true,
        "monitor": { "id": monitor.id, "name": monitor.name },
    }))
}

#[derive(Deserialize)]
struct AckParams {
    target: Target,
    #[serde(rename = "eventIDs")]
    event_ids: Vec<String>,
}

async fn ack(state: &Arc<HttpState>, params: Value) -> Result<Value, RpcFailure> {
    let params: AckParams = serde_json::from_value(params)
        .map_err(|error| RpcFailure::invalid_params(format!("monitor.ack: {error}")))?;

    let scoped = state.store.clone();
    let target = params.target;
    let ids = params.event_ids;
    store_call(move || scoped.ack(&target, &ids)).await?;
    Ok(json!({ "ok": true }))
}

async fn store_call<T, F>(operation: F) -> Result<T, RpcFailure>
where
    F: FnOnce() -> Result<T, StoreError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(RpcFailure::internal(format!("store: {error}"))),
        Err(error) => Err(RpcFailure::internal(format!(
            "store task panicked: {error}"
        ))),
    }
}

/// Opaque, collision-resistant monitor id. There is no uuid dependency in the
/// spike, so entropy comes from a randomly seeded `RandomState` over the
/// process counter and the wall clock.
fn new_monitor_id() -> String {
    use std::hash::{BuildHasher, Hasher};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_i64(model::now_ms());
    hasher.write_u64(counter);
    format!("m-{:016x}", hasher.finish())
}
