//! The polling tick.
//!
//! Every `STALL_TICK_MS` the scheduler lists monitors, re-reads each one to
//! decide whether it is due (sourcefed does exactly that), and hands due ones to
//! their plugin's realm. A monitor never has two polls in flight. Completions
//! come back on a channel and are persisted here, one at a time, so event
//! writes and SSE fan-out are serialised.

use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::MissedTickBehavior;

use crate::model::{self, Monitor, QueuedMonitorEvent, META_LAST_ATTEMPT, META_LAST_POLLED};
use crate::plugin::{Completion, Job, JobRouter, PollOutput};
use crate::sse::{self, Hub};
use crate::store::{Store, StoreError};

pub async fn run(
    store: Arc<dyn Store>,
    hub: Arc<Hub>,
    router: JobRouter,
    inflight: Arc<AtomicUsize>,
    mut completions: UnboundedReceiver<Completion>,
    tick: Duration,
) {
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut inflight_ids: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match tick_once(&store, &router, &inflight_ids).await {
                    Ok(submitted) => {
                        inflight_ids.extend(submitted);
                        inflight.store(inflight_ids.len(), Ordering::Relaxed);
                    }
                    Err(error) => crate::log_line("error", &format!("tick: {error}")),
                }
            }
            Some(done) = completions.recv() => {
                inflight_ids.remove(&done.monitor_id);
                inflight.store(inflight_ids.len(), Ordering::Relaxed);
                if let Err(error) = apply_completion(&store, &hub, done).await {
                    crate::log_line("error", &format!("poll completion: {error}"));
                }
            }
        }
    }
}

/// List monitors, re-read each one, and submit the due ones that are not
/// already running. Returns the ids actually submitted.
async fn tick_once(
    store: &Arc<dyn Store>,
    router: &JobRouter,
    inflight: &HashSet<String>,
) -> Result<Vec<String>, String> {
    let now = model::now_ms();
    let tick_store = store.clone();
    let due = tokio::task::spawn_blocking(move || -> Result<Vec<Monitor>, String> {
        let listed = tick_store
            .list_monitors()
            .map_err(|error| error.to_string())?;
        let mut due = Vec::new();
        for monitor in listed {
            // Naive on purpose: one whole-file read per monitor per tick, which
            // is what sourcefed's JsonStore path does today.
            let Some(fresh) = tick_store
                .get_monitor(&monitor.id)
                .map_err(|error| error.to_string())?
            else {
                continue;
            };
            if model::is_due(now, model::last_attempt_ms(&fresh), fresh.poll_interval_sec) {
                due.push(fresh);
            }
        }
        Ok(due)
    })
    .await
    .map_err(|error| format!("tick task: {error}"))??;

    let mut submitted = Vec::new();
    for monitor in due {
        if inflight.contains(&monitor.id) {
            continue;
        }
        let source_type = monitor.source_type();
        match router.submit(
            source_type,
            Job {
                monitor: monitor.clone(),
            },
        ) {
            Ok(()) => submitted.push(monitor.id),
            Err(error) => {
                crate::log_line("error", &format!("submit monitor {}: {error}", monitor.id))
            }
        }
    }
    Ok(submitted)
}

async fn apply_completion(
    store: &Arc<dyn Store>,
    hub: &Arc<Hub>,
    done: Completion,
) -> Result<(), String> {
    let Completion {
        monitor_id,
        target,
        source,
        source_type,
        started_at_ms,
        duration_ms,
        outcome,
    } = done;

    match outcome {
        Err(error) => {
            let scoped = store.clone();
            let target_id = monitor_id.clone();
            let attempt_at = Value::from(started_at_ms);
            store_blocking(move || {
                scoped.set_cursors(&target_id, &[(META_LAST_ATTEMPT.to_string(), attempt_at)])
            })
            .await?;
            crate::log_line(
                "warn",
                &format!("poll {monitor_id} ({source_type}) failed after {duration_ms}ms: {error}"),
            );
            Ok(())
        }
        Ok(output) => {
            persist_success(
                store,
                hub,
                &monitor_id,
                &target,
                &source,
                source_type,
                started_at_ms,
                output,
            )
            .await
        }
    }
}

/// Mirror sourcefed's three writes per successful poll: cursor, queued events,
/// then the cursor again together with the attempt/polled stamps.
#[allow(clippy::too_many_arguments)]
async fn persist_success(
    store: &Arc<dyn Store>,
    hub: &Arc<Hub>,
    monitor_id: &str,
    target: &crate::model::Target,
    source: &crate::model::Source,
    source_type: crate::model::SourceType,
    started_at_ms: i64,
    output: PollOutput,
) -> Result<(), String> {
    let cursor_key = source_type.as_str().to_string();

    {
        let scoped = store.clone();
        let monitor_id = monitor_id.to_string();
        let key = cursor_key.clone();
        let cursor = output.cursor.clone();
        store_blocking(move || scoped.set_cursors(&monitor_id, &[(key, cursor)])).await?;
    }

    let queued_at = model::iso8601_ms(model::now_ms());
    let batch: Vec<QueuedMonitorEvent> = {
        let scoped = store.clone();
        let monitor_id = monitor_id.to_string();
        let target = target.clone();
        let source = source.clone();
        let events = output.events.clone();
        store_blocking(move || {
            let existing = scoped.events()?;
            let existing_ids: BTreeSet<String> = existing
                .into_iter()
                .filter(|event| event.target == target)
                .map(|event| event.id)
                .collect();
            let indices = model::new_event_indices(&existing_ids, &monitor_id, &events);
            let batch: Vec<QueuedMonitorEvent> = indices
                .into_iter()
                .map(|index| {
                    model::queued_event(&monitor_id, &source, &target, &events[index], &queued_at)
                })
                .collect();
            if batch.is_empty() {
                return Ok(batch);
            }
            let outcome = scoped.append_events(&batch)?;
            if outcome.evicted > 0 {
                crate::log_line(
                    "warn",
                    &format!(
                        "queue cap {} reached for {}:{}: evicted {} event(s)",
                        model::QUEUE_CAP_PER_TARGET,
                        target.kind,
                        target.id,
                        outcome.evicted
                    ),
                );
            }
            Ok(batch)
        })
        .await?
    };
    if !batch.is_empty() {
        hub.publish(target, sse::event_frame(target, &batch));
    }

    {
        let scoped = store.clone();
        let monitor_id = monitor_id.to_string();
        let cursor = output.cursor;
        let attempt_at = Value::from(started_at_ms);
        let polled_at = Value::from(model::now_ms());
        store_blocking(move || {
            scoped.set_cursors(
                &monitor_id,
                &[
                    (cursor_key, cursor),
                    (META_LAST_ATTEMPT.to_string(), attempt_at),
                    (META_LAST_POLLED.to_string(), polled_at),
                ],
            )
        })
        .await?;
    }

    Ok(())
}

async fn store_blocking<T, F>(operation: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, StoreError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("store: {error}")),
        Err(error) => Err(format!("store task panicked: {error}")),
    }
}
