//! Domain model shared by the lane, the store, the scheduler and the plugins.
//!
//! Field names mirror sourcefed's on-disk records: a monitor is stored with
//! `sourceType` and its source parameters at the top level, plus the metadata
//! cursors `__lastAttemptAt` / `__lastPolledAt` inside `cursors`.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const VERSION: &str = "stall-b-0.1.0-spike";

/// Cursor key naming the sourcefed metadata cursors.
pub const META_LAST_ATTEMPT: &str = "__lastAttemptAt";
pub const META_LAST_POLLED: &str = "__lastPolledAt";

/// Queued events are capped per target; the oldest are evicted first.
pub const QUEUE_CAP_PER_TARGET: usize = 1000;

/// Everything a queued event is addressed to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Target {
    pub kind: String,
    pub id: String,
}

/// The plugin types the stall knows how to host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceType {
    GithubIssues,
    JiraQuery,
}

impl SourceType {
    pub const ALL: [SourceType; 2] = [SourceType::GithubIssues, SourceType::JiraQuery];

    pub fn as_str(self) -> &'static str {
        match self {
            SourceType::GithubIssues => "github-issues",
            SourceType::JiraQuery => "jira-query",
        }
    }
}

impl std::fmt::Display for SourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A monitor's source configuration: the `sourceType` tag plus its parameters.
///
/// The variant tag is serialized as `sourceType` so a monitor record stays flat,
/// exactly like the JSON sourcefed writes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "sourceType",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Source {
    GithubIssues {
        repo: String,
        labels: Vec<String>,
        include_existing: bool,
    },
    JiraQuery {
        jql: String,
    },
}

impl Source {
    pub fn source_type(&self) -> SourceType {
        match self {
            Source::GithubIssues { .. } => SourceType::GithubIssues,
            Source::JiraQuery { .. } => SourceType::JiraQuery,
        }
    }

    /// The source as the contract's event/plugin payload names it: `type` plus
    /// the parameters (not `sourceType`).
    pub fn source_json(&self) -> Value {
        match self {
            Source::GithubIssues {
                repo,
                labels,
                include_existing,
            } => json!({
                "type": SourceType::GithubIssues.as_str(),
                "repo": repo,
                "labels": labels,
                "includeExisting": include_existing,
            }),
            Source::JiraQuery { jql } => json!({
                "type": SourceType::JiraQuery.as_str(),
                "jql": jql,
            }),
        }
    }
}

/// One monitor as persisted in `monitors.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub id: String,
    pub name: String,
    pub target: Target,
    #[serde(flatten)]
    pub source: Source,
    #[serde(rename = "pollIntervalSec")]
    pub poll_interval_sec: u64,
    #[serde(default)]
    pub cursors: BTreeMap<String, Value>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

impl Monitor {
    pub fn source_type(&self) -> SourceType {
        self.source.source_type()
    }

    /// The cursor the plugin reads: whatever it returned last time, under its
    /// own source type key.
    pub fn plugin_cursor(&self) -> Value {
        self.cursors
            .get(self.source_type().as_str())
            .cloned()
            .unwrap_or(Value::Null)
    }
}

/// An event as a plugin bundle returns it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceEvent {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub at: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub actionable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Value>,
}

impl SourceEvent {
    /// `event.id`, or the contract's fallback identity.
    pub fn event_id(&self) -> String {
        match &self.id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => format!("{}:{}:{}", self.kind, self.at, self.summary),
        }
    }
}

/// A queued event as delivered over SSE and stored in `events.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueuedMonitorEvent {
    pub id: String,
    #[serde(rename = "monitorID")]
    pub monitor_id: String,
    pub target: Target,
    #[serde(rename = "queuedAt")]
    pub queued_at: String,
    pub event: QueuedEvent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueuedEvent {
    pub source: Value,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub at: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub actionable: bool,
    pub terminal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Value>,
}

/// Build the queued form of a source event for a monitor.
pub fn queued_event(
    monitor_id: &str,
    source: &Source,
    target: &Target,
    event: &SourceEvent,
    queued_at: &str,
) -> QueuedMonitorEvent {
    let event_id = event.event_id();
    QueuedMonitorEvent {
        id: format!("{monitor_id}:{event_id}"),
        monitor_id: monitor_id.to_string(),
        target: target.clone(),
        queued_at: queued_at.to_string(),
        event: QueuedEvent {
            source: source.source_json(),
            kind: event.kind.clone(),
            id: event.id.clone(),
            at: event.at.clone(),
            summary: event.summary.clone(),
            body: event.body.clone(),
            actionable: event.actionable,
            terminal: event.terminal.unwrap_or(false),
            subject: event.subject.clone(),
        },
    }
}

/// Indices of the events whose `<monitorID>:<eventID>` identity is not queued
/// yet. Order is preserved so a batch keeps the plugin's ordering.
pub fn new_event_indices(
    existing_queued_ids: &std::collections::BTreeSet<String>,
    monitor_id: &str,
    events: &[SourceEvent],
) -> Vec<usize> {
    let mut seen = existing_queued_ids.clone();
    let mut keep = Vec::new();
    for (index, event) in events.iter().enumerate() {
        let id = format!("{monitor_id}:{}", event.event_id());
        if seen.insert(id) {
            keep.push(index);
        }
    }
    keep
}

/// Evict the oldest queued events for `target` until at most `cap` remain.
/// Returns how many were dropped.
pub fn evict_over_cap(events: &mut Vec<QueuedMonitorEvent>, target: &Target, cap: usize) -> usize {
    let mut dropped = 0;
    while events
        .iter()
        .filter(|event| &event.target == target)
        .count()
        > cap
    {
        match events.iter().position(|event| &event.target == target) {
            Some(index) => {
                events.remove(index);
                dropped += 1;
            }
            None => break,
        }
    }
    dropped
}

/// Due when the monitor has never been attempted, or its last attempt is at
/// least one poll interval old.
pub fn is_due(now_ms: i64, last_attempt_ms: Option<i64>, poll_interval_sec: u64) -> bool {
    match last_attempt_ms {
        None => true,
        Some(last) => {
            let interval = poll_interval_sec.saturating_mul(1000).min(i64::MAX as u64) as i64;
            now_ms.saturating_sub(last) >= interval
        }
    }
}

/// The stored `__lastAttemptAt` value, when it is a whole number of ms.
pub fn last_attempt_ms(monitor: &Monitor) -> Option<i64> {
    monitor
        .cursors
        .get(META_LAST_ATTEMPT)
        .and_then(Value::as_i64)
}

pub fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis().min(i64::MAX as u128) as i64,
        Err(_) => 0,
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` in UTC, without pulling in a date library.
pub fn iso8601_ms(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3600;
    let minute = (second_of_day % 3600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}
