//! The local HTTP surface: `POST /rpc` and the SSE stream on `GET /events`.
//!
//! SSE bodies stream for as long as the client stays connected; no request
//! timeout closes them, and up to `MAX_CONNECTIONS` connections are served at
//! once. Unknown paths answer 404.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::{self, StreamExt};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderValue, CACHE_CONTROL, CONNECTION, CONTENT_TYPE};
use hyper::http::{Request, Response, StatusCode};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::rpc;
use crate::sse::{self, Hub, Subscription};
use crate::store::Store;
use crate::target;

const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_CONNECTIONS: usize = 256;
const KEEPALIVE: Duration = Duration::from_secs(30);

pub struct HttpState {
    pub store: Arc<dyn Store>,
    pub hub: Arc<Hub>,
}

type Body = BoxBody<Bytes, Infallible>;

pub async fn serve(listener: TcpListener, state: Arc<HttpState>) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    loop {
        let permit = match permits.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        let (stream, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                drop(permit);
                crate::log_line("warn", &format!("http accept: {error}"));
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(move |request| handle(request, state.clone()));
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                crate::log_line("debug", &format!("http connection {peer}: {error}"));
            }
        });
    }
}

async fn handle(
    request: Request<Incoming>,
    state: Arc<HttpState>,
) -> Result<Response<Body>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    let response = if method == hyper::Method::POST && path == "/rpc" {
        rpc_response(request, &state).await
    } else if method == hyper::Method::GET && path == "/events" {
        events_response(request, &state)
    } else if path == "/rpc" || path == "/events" {
        text_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
    } else {
        text_response(StatusCode::NOT_FOUND, "not found")
    };

    Ok(response)
}

async fn rpc_response(request: Request<Incoming>, state: &Arc<HttpState>) -> Response<Body> {
    let collected = Limited::new(request.into_body(), MAX_BODY_BYTES)
        .collect()
        .await;
    let (status, document) = match collected {
        Ok(body) => rpc::handle(state, &body.to_bytes()).await,
        Err(_) => (
            StatusCode::BAD_REQUEST,
            rpc::error_document(
                Value::Null,
                -32700,
                "request body is unreadable or over 1 MiB",
            ),
        ),
    };
    json_response(status, &document)
}

fn events_response(request: Request<Incoming>, state: &Arc<HttpState>) -> Response<Body> {
    let Some(raw_target) = query_target(request.uri().query()) else {
        return text_response(StatusCode::BAD_REQUEST, "missing target query parameter");
    };
    let target = match target::decode_query(&raw_target) {
        Ok(target) => target,
        Err(error) => {
            return text_response(StatusCode::BAD_REQUEST, &format!("bad target: {error}"))
        }
    };
    match state.hub.subscribe(&target) {
        Ok(subscription) => sse_response(subscription),
        Err(error) => text_response(StatusCode::SERVICE_UNAVAILABLE, &error),
    }
}

fn sse_response(subscription: Subscription) -> Response<Body> {
    let target = subscription.target().clone();
    let subscribed = Ok::<_, Infallible>(Frame::data(sse::subscribed_frame(&target)));

    let frames = stream::unfold(
        (subscription, tokio::time::Instant::now() + KEEPALIVE),
        |(mut subscription, mut deadline)| async move {
            let frame = tokio::select! {
                message = subscription.recv() => message?,
                _ = tokio::time::sleep_until(deadline) => {
                    deadline = tokio::time::Instant::now() + KEEPALIVE;
                    sse::keepalive_frame()
                }
            };
            Some((
                Ok::<_, Infallible>(Frame::data(frame)),
                (subscription, deadline),
            ))
        },
    );

    let body = BodyExt::boxed(StreamBody::new(
        stream::once(async move { subscribed }).chain(frames),
    ));
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

fn query_target(query: Option<&str>) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key == "target" {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn json_response(status: StatusCode, document: &Value) -> Response<Body> {
    let bytes = serde_json::to_vec(document).unwrap_or_else(|_| b"{}".to_vec());
    let mut response = Response::new(Full::new(Bytes::from(bytes)).boxed());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

fn text_response(status: StatusCode, text: &str) -> Response<Body> {
    let bytes = Bytes::copy_from_slice(text.as_bytes());
    let mut response = Response::new(Full::new(bytes).boxed());
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
