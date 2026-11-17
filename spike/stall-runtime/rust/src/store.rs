//! Monitor and event persistence.
//!
//! The JSON store deliberately mirrors sourcefed's JsonStore: every read
//! re-reads and re-parses the whole file (there is no in-memory cache), and
//! every write replaces the file atomically (`<file>.<pid>.tmp` then rename,
//! mode 0600). The scheduler walks that naive path once per monitor per tick
//! because that is what today's sourcefed does; the memory store keeps the same
//! structures in RAM and writes nothing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::model::{self, Monitor, QueuedMonitorEvent, Target, QUEUE_CAP_PER_TARGET};

#[derive(Debug, Clone)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendOutcome {
    pub appended: usize,
    pub evicted: usize,
}

pub trait Store: Send + Sync {
    fn create_monitor(&self, monitor: &Monitor) -> Result<(), StoreError>;
    fn list_monitors(&self) -> Result<Vec<Monitor>, StoreError>;
    fn get_monitor(&self, id: &str) -> Result<Option<Monitor>, StoreError>;
    /// Apply cursor updates (and bump `updatedAt`) in one write.
    fn set_cursors(&self, id: &str, cursors: &[(String, Value)]) -> Result<(), StoreError>;
    fn events(&self) -> Result<Vec<QueuedMonitorEvent>, StoreError>;
    fn append_events(&self, batch: &[QueuedMonitorEvent]) -> Result<AppendOutcome, StoreError>;
    fn ack(&self, target: &Target, ids: &[String]) -> Result<usize, StoreError>;
    fn monitor_count(&self) -> Result<usize, StoreError>;
}

pub fn open_json(dir: &Path) -> Result<Arc<dyn Store>, StoreError> {
    fs::create_dir_all(dir)
        .map_err(|error| StoreError(format!("{}: create state dir: {error}", dir.display())))?;
    Ok(Arc::new(JsonStore {
        dir: dir.to_path_buf(),
        lock: Mutex::new(()),
    }))
}

pub fn open_memory() -> Arc<dyn Store> {
    Arc::new(MemoryStore {
        state: Mutex::new(MemoryState::default()),
    })
}

// ---------------------------------------------------------------------------
// JSON store
// ---------------------------------------------------------------------------

struct JsonStore {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl JsonStore {
    fn monitors_path(&self) -> PathBuf {
        self.dir.join("monitors.json")
    }

    fn events_path(&self) -> PathBuf {
        self.dir.join("events.json")
    }

    fn read_monitors(&self) -> Result<Vec<Monitor>, StoreError> {
        read_json(&self.monitors_path())
    }

    fn write_monitors(&self, monitors: &[Monitor]) -> Result<(), StoreError> {
        write_json(&self.monitors_path(), monitors)
    }

    fn read_events(&self) -> Result<Vec<QueuedMonitorEvent>, StoreError> {
        read_json(&self.events_path())
    }

    fn write_events(&self, events: &[QueuedMonitorEvent]) -> Result<(), StoreError> {
        write_json(&self.events_path(), events)
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, StoreError> {
    match fs::read(path) {
        Ok(bytes) if bytes.is_empty() => Ok(Vec::new()),
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| StoreError(format!("{}: parse: {error}", path.display()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(StoreError(format!("{}: read: {error}", path.display()))),
    }
}

fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| StoreError(format!("{}: encode: {error}", path.display())))?;

    let mut temp_name = path.as_os_str().to_os_string();
    temp_name.push(format!(".{}.tmp", std::process::id()));
    let temp_path = PathBuf::from(temp_name);

    fs::write(&temp_path, &bytes)
        .map_err(|error| StoreError(format!("{}: write: {error}", temp_path.display())))?;
    if let Err(error) = set_private_permissions(&temp_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(StoreError(format!(
            "{}: rename over {}: {error}",
            temp_path.display(),
            path.display()
        )));
    }
    Ok(())
}

fn set_private_permissions(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| StoreError(format!("{}: chmod 0600: {error}", path.display())))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

impl Store for JsonStore {
    fn create_monitor(&self, monitor: &Monitor) -> Result<(), StoreError> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut monitors = self.read_monitors()?;
        if monitors.iter().any(|existing| existing.id == monitor.id) {
            return Err(StoreError(format!("duplicate monitor id {}", monitor.id)));
        }
        monitors.push(monitor.clone());
        self.write_monitors(&monitors)
    }

    fn list_monitors(&self) -> Result<Vec<Monitor>, StoreError> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.read_monitors()
    }

    fn get_monitor(&self, id: &str) -> Result<Option<Monitor>, StoreError> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Ok(self
            .read_monitors()?
            .into_iter()
            .find(|monitor| monitor.id == id))
    }

    fn set_cursors(&self, id: &str, cursors: &[(String, Value)]) -> Result<(), StoreError> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut monitors = self.read_monitors()?;
        let monitor = monitors
            .iter_mut()
            .find(|monitor| monitor.id == id)
            .ok_or_else(|| StoreError(format!("monitor {id} not found")))?;
        for (key, value) in cursors {
            monitor.cursors.insert(key.clone(), value.clone());
        }
        monitor.updated_at = model::iso8601_ms(model::now_ms());
        self.write_monitors(&monitors)
    }

    fn events(&self) -> Result<Vec<QueuedMonitorEvent>, StoreError> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.read_events()
    }

    fn append_events(&self, batch: &[QueuedMonitorEvent]) -> Result<AppendOutcome, StoreError> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut events = self.read_events()?;
        let mut evicted = 0;
        let mut targets: Vec<Target> = Vec::new();
        for event in batch {
            events.push(event.clone());
            if !targets.contains(&event.target) {
                targets.push(event.target.clone());
            }
        }
        for target in &targets {
            evicted += model::evict_over_cap(&mut events, target, QUEUE_CAP_PER_TARGET);
        }
        self.write_events(&events)?;
        Ok(AppendOutcome {
            appended: batch.len(),
            evicted,
        })
    }

    fn ack(&self, target: &Target, ids: &[String]) -> Result<usize, StoreError> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut events = self.read_events()?;
        let before = events.len();
        events.retain(|event| &event.target != target || !ids.contains(&event.id));
        let removed = before - events.len();
        if removed > 0 {
            self.write_events(&events)?;
        }
        Ok(removed)
    }

    fn monitor_count(&self) -> Result<usize, StoreError> {
        Ok(self.list_monitors()?.len())
    }
}

// ---------------------------------------------------------------------------
// In-memory store
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemoryState {
    monitors: Vec<Monitor>,
    events: Vec<QueuedMonitorEvent>,
}

struct MemoryStore {
    state: Mutex<MemoryState>,
}

impl MemoryStore {
    fn with<T>(&self, f: impl FnOnce(&mut MemoryState) -> T) -> T {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        f(&mut state)
    }
}

impl Store for MemoryStore {
    fn create_monitor(&self, monitor: &Monitor) -> Result<(), StoreError> {
        self.with(|state| {
            if state
                .monitors
                .iter()
                .any(|existing| existing.id == monitor.id)
            {
                return Err(StoreError(format!("duplicate monitor id {}", monitor.id)));
            }
            state.monitors.push(monitor.clone());
            Ok(())
        })
    }

    fn list_monitors(&self) -> Result<Vec<Monitor>, StoreError> {
        self.with(|state| Ok(state.monitors.clone()))
    }

    fn get_monitor(&self, id: &str) -> Result<Option<Monitor>, StoreError> {
        self.with(|state| {
            Ok(state
                .monitors
                .iter()
                .find(|monitor| monitor.id == id)
                .cloned())
        })
    }

    fn set_cursors(&self, id: &str, cursors: &[(String, Value)]) -> Result<(), StoreError> {
        self.with(|state| {
            let monitor = state
                .monitors
                .iter_mut()
                .find(|monitor| monitor.id == id)
                .ok_or_else(|| StoreError(format!("monitor {id} not found")))?;
            for (key, value) in cursors {
                monitor.cursors.insert(key.clone(), value.clone());
            }
            monitor.updated_at = model::iso8601_ms(model::now_ms());
            Ok(())
        })
    }

    fn events(&self) -> Result<Vec<QueuedMonitorEvent>, StoreError> {
        self.with(|state| Ok(state.events.clone()))
    }

    fn append_events(&self, batch: &[QueuedMonitorEvent]) -> Result<AppendOutcome, StoreError> {
        self.with(|state| {
            let mut evicted = 0;
            let mut targets: Vec<Target> = Vec::new();
            for event in batch {
                state.events.push(event.clone());
                if !targets.contains(&event.target) {
                    targets.push(event.target.clone());
                }
            }
            for target in &targets {
                evicted += model::evict_over_cap(&mut state.events, target, QUEUE_CAP_PER_TARGET);
            }
            Ok(AppendOutcome {
                appended: batch.len(),
                evicted,
            })
        })
    }

    fn ack(&self, target: &Target, ids: &[String]) -> Result<usize, StoreError> {
        self.with(|state| {
            let before = state.events.len();
            state
                .events
                .retain(|event| &event.target != target || !ids.contains(&event.id));
            Ok(before - state.events.len())
        })
    }

    fn monitor_count(&self) -> Result<usize, StoreError> {
        self.with(|state| Ok(state.monitors.len()))
    }
}
