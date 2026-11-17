//! SSE fan-out: one broadcast per target, streaming to every connected client.
//!
//! Frames are exactly the contract's shapes: a `subscribed` frame on connect,
//! one `event` frame per delivery batch, and a `: keepalive` comment.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::model::{QueuedMonitorEvent, Target};

/// Bounded fan-out: a target with more clients than this gets a 503.
pub const MAX_CLIENTS_PER_TARGET: usize = 256;
/// Frames buffered for one client. A client that stops reading is dropped
/// instead of growing memory; the queued events stay in the store until acked.
const CLIENT_QUEUE_FRAMES: usize = 64;

struct Client {
    id: u64,
    sender: mpsc::Sender<Bytes>,
}

#[derive(Default)]
pub struct Hub {
    clients: Mutex<HashMap<Target, Vec<Client>>>,
    next_id: AtomicU64,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(self: &Arc<Self>, target: &Target) -> Result<Subscription, String> {
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if clients
            .get(target)
            .is_some_and(|list| list.len() >= MAX_CLIENTS_PER_TARGET)
        {
            return Err(format!(
                "too many event streams for target {}:{}",
                target.kind, target.id
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = mpsc::channel(CLIENT_QUEUE_FRAMES);
        clients
            .entry(target.clone())
            .or_default()
            .push(Client { id, sender });
        Ok(Subscription {
            hub: self.clone(),
            target: target.clone(),
            id,
            receiver,
        })
    }

    /// Send one frame to every client of `target`, dropping closed streams.
    pub fn publish(&self, target: &Target, frame: Bytes) {
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(list) = clients.get_mut(target) else {
            return;
        };
        let before = list.len();
        list.retain(|client| client.sender.try_send(frame.clone()).is_ok());
        let dropped = before - list.len();
        if dropped > 0 {
            crate::log_line(
                "warn",
                &format!(
                    "dropped {dropped} slow or closed SSE client(s) for {}:{}",
                    target.kind, target.id
                ),
            );
        }
        if list.is_empty() {
            clients.remove(target);
        }
    }

    fn unsubscribe(&self, target: &Target, id: u64) {
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(list) = clients.get_mut(target) {
            list.retain(|client| client.id != id);
            if list.is_empty() {
                clients.remove(target);
            }
        }
    }
}

pub struct Subscription {
    hub: Arc<Hub>,
    target: Target,
    id: u64,
    receiver: mpsc::Receiver<Bytes>,
}

impl Subscription {
    pub fn target(&self) -> &Target {
        &self.target
    }

    pub async fn recv(&mut self) -> Option<Bytes> {
        self.receiver.recv().await
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.unsubscribe(&self.target, self.id);
    }
}

#[derive(Serialize)]
struct Subscribed<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    target: &'a Target,
}

#[derive(Serialize)]
struct EventBatch<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    target: &'a Target,
    events: &'a [QueuedMonitorEvent],
}

pub fn subscribed_frame(target: &Target) -> Bytes {
    data_frame(&Subscribed {
        kind: "subscribed",
        target,
    })
}

pub fn event_frame(target: &Target, events: &[QueuedMonitorEvent]) -> Bytes {
    data_frame(&EventBatch {
        kind: "event",
        target,
        events,
    })
}

pub fn keepalive_frame() -> Bytes {
    Bytes::from_static(b": keepalive\n\n")
}

fn data_frame(payload: &impl Serialize) -> Bytes {
    let json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    Bytes::from(format!("data: {json}\n\n"))
}
