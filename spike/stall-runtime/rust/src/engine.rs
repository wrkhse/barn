//! The engine seam: one realm interface, one implementation per JavaScript engine.
//!
//! A realm is the JavaScript side of one plugin type: the shared prelude, the bundle from
//! `STALL_PLUGIN_DIR`, and the two host functions the prelude calls. `engine-rquickjs` runs
//! it in QuickJS through rquickjs 0.14; `engine-djs` runs it in djs. `plugin.rs` builds a
//! poll's input, schedules the poll and reads its envelope, and never learns which engine
//! it has: a binary compiles exactly one of them (src/lib.rs holds that to one).
//!
//! Both engines answer the same calls, log through the same lines and report the same two
//! timings, which is what lets the harness compare candidate B and candidate C:
//! `[stall-b] engine <engine> realm-build <plugin> <ms>` and `... realm-reset ...`.

use std::time::Duration;

use crate::model::SourceType;
use crate::plugin::WorkerConfig;

/// The prelude both engines run: one source file, so the JavaScript contract cannot drift
/// between them.
const PRELUDE: &str = include_str!("prelude.js");

/// Why a poll did not produce an envelope.
pub enum Failure {
    /// The poll used up its budget. The engine has already put the realm back into its
    /// prelude-and-bundle state, timed as `realm-reset`, so the next poll is ready.
    Interrupted,
    /// The plugin's JavaScript threw or rejected: the realm is still usable.
    Js(String),
    /// The realm cannot go on (it did not load, or its engine panicked). The caller drops
    /// it and the next poll builds a new one.
    Fatal(String),
}

/// One plugin type's realm. `poll` runs on the realm's own OS thread, one poll at a time.
pub trait Engine: Send {
    /// One poll: `input_json` is the text the prelude's `__stallInvokePoll` parses, and the
    /// answer is the envelope string it returns, `{"ok":true,"result":...}` or
    /// `{"ok":false,"error":...}`.
    fn poll(&mut self, input_json: &str) -> Result<String, Failure>;

    /// The engine's name: `"rquickjs"` or `"djs"`.
    fn name(&self) -> &'static str;
}

/// The engine this binary was compiled with.
#[cfg(feature = "engine-rquickjs")]
pub fn name() -> &'static str {
    "rquickjs"
}

#[cfg(feature = "engine-djs")]
pub fn name() -> &'static str {
    "djs"
}

/// A realm for `source_type`, ready for its first poll.
pub fn realm(source_type: SourceType, config: &WorkerConfig) -> Result<Box<dyn Engine>, String> {
    Compiled::load(source_type, config).map(|realm| Box::new(realm) as Box<dyn Engine>)
}

#[cfg(feature = "engine-rquickjs")]
type Compiled = quickjs::Realm;
#[cfg(feature = "engine-djs")]
type Compiled = djs::Realm;

/// `realm-build` or `realm-reset` for one plugin type, in milliseconds with one decimal:
/// the harness reads these lines to compare the engines.
fn log_timing(kind: &str, source_type: SourceType, elapsed: Duration) {
    eprintln!(
        "[stall-b] engine {} {kind} {source_type} {:.1}",
        name(),
        elapsed.as_secs_f64() * 1000.0
    );
}

/// A plugin's own log line, so both engines report `__stallLog` identically.
fn log_plugin(source_type: SourceType, level: &str, message: &str) {
    let level = match level {
        "warn" | "error" | "debug" => level,
        _ => "info",
    };
    crate::log_line(level, &format!("plugin {source_type}: {message}"));
}

// ---------------------------------------------------------------------------
// rquickjs: candidate B's realm, moved here unchanged
// ---------------------------------------------------------------------------

#[cfg(feature = "engine-rquickjs")]
mod quickjs {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rquickjs::{Context, Ctx, Function, Object, Promise, Runtime, Value as JsValue};
    use serde_json::json;

    use super::{log_plugin, log_timing, Engine, Failure, PRELUDE};
    use crate::model::SourceType;
    use crate::plugin::{HostBridge, WorkerConfig};

    const MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;

    /// A QuickJS realm: a runtime, its context, the prelude and the bundle, built together
    /// so a rebuild after an interrupt reproduces exactly the realm it replaces.
    pub struct Realm {
        source_type: SourceType,
        config: WorkerConfig,
        clock: Arc<PollClock>,
        state: Option<State>,
    }

    struct State {
        // The context keeps its own handle to the runtime; this field only makes ownership
        // explicit and drops the runtime with the state.
        _runtime: Runtime,
        context: Context,
    }

    // One worker thread builds, polls and drops a realm, and its box never leaves that
    // thread, which is the only sense in which the stall needs it sendable. rquickjs can
    // declare its runtime Send with its `parallel` feature, but that feature puts a lock on
    // every runtime access, and candidate B's timings are what this spike measures.
    unsafe impl Send for Realm {}

    impl Realm {
        /// Runtime, context, prelude and bundle: what `realm-build` measures.
        pub fn load(source_type: SourceType, config: &WorkerConfig) -> Result<Realm, String> {
            let clock = Arc::new(PollClock::new());
            let started = Instant::now();
            let state = build(source_type, config, &clock)?;
            log_timing("realm-build", source_type, started.elapsed());
            Ok(Realm {
                source_type,
                config: config.clone(),
                clock,
                state: Some(state),
            })
        }

        /// Puts the realm back into a fresh prelude-and-bundle state. QuickJS cannot be
        /// reused once its interrupt handler fired, so recovery is a rebuild, done here
        /// rather than before the next poll so that `realm-reset` can report what it cost.
        fn recover(&mut self) -> Result<(), String> {
            let started = Instant::now();
            self.state = None;
            let state = build(self.source_type, &self.config, &self.clock)?;
            self.state = Some(state);
            log_timing("realm-reset", self.source_type, started.elapsed());
            Ok(())
        }
    }

    impl Engine for Realm {
        fn name(&self) -> &'static str {
            "rquickjs"
        }

        fn poll(&mut self, input_json: &str) -> Result<String, Failure> {
            let Some(state) = self.state.as_mut() else {
                return Err(Failure::Fatal(format!(
                    "plugin {} realm is unusable",
                    self.source_type
                )));
            };
            self.clock.arm(self.config.deadline);
            let result = evaluate(state, input_json);
            let fired = self.clock.disarm();
            if fired {
                return match self.recover() {
                    Ok(()) => Err(Failure::Interrupted),
                    Err(error) => Err(Failure::Fatal(error)),
                };
            }
            result
        }
    }

    /// A runtime with the memory limit and the interrupt handler, a context, and the prelude
    /// and bundle evaluated into it: today's realm load, unchanged.
    fn build(
        source_type: SourceType,
        config: &WorkerConfig,
        clock: &Arc<PollClock>,
    ) -> Result<State, String> {
        let runtime = Runtime::new().map_err(|error| format!("runtime: {error}"))?;
        runtime.set_memory_limit(MEMORY_LIMIT_BYTES);

        let handler_clock = clock.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || handler_clock.expired())));

        let context = Context::full(&runtime).map_err(|error| format!("context: {error}"))?;
        let path = config.plugin_dir.join(format!("{source_type}.js"));
        let bundle = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read plugin bundle {}: {error}", path.display()))?;
        let realm_config = serde_json::to_string(&json!({
            "plugin": source_type.as_str(),
            "faults": config.faults,
            "env": crate::plugin::realm_env(),
        }))
        .map_err(|error| format!("realm config json: {error}"))?;
        let bridge = HostBridge::new(source_type, config.host_calls.clone());
        let bundle_path = path.display().to_string();

        // Bound bundle evaluation too: a broken (or hostile) bundle must not wedge the
        // realm thread before it ever serves a poll.
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
                log_plugin(source_type, &level, &message);
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

        Ok(State {
            _runtime: runtime,
            context,
        })
    }

    /// One poll: `__stallInvokePoll(input)` to its envelope. The clock belongs to the realm,
    /// the interrupt it fires is the realm's too, so it is armed by the caller.
    fn evaluate(state: &mut State, input_json: &str) -> Result<String, Failure> {
        state.context.with(|ctx| -> Result<String, Failure> {
            let invoke: Function = ctx.globals().get("__stallInvokePoll").map_err(|error| {
                Failure::Fatal(describe_error(&ctx, error, "look up __stallInvokePoll"))
            })?;
            let promise: Promise = invoke
                .call((input_json,))
                .map_err(|error| Failure::Fatal(describe_error(&ctx, error, "call pluginPoll")))?;
            promise
                .finish::<String>()
                .map_err(|error| Failure::Fatal(describe_error(&ctx, error, "pluginPoll result")))
        })
    }

    /// QuickJS calls this regularly while executing bytecode; returning true raises an
    /// uncatchable exception that aborts the current poll.
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
}

// ---------------------------------------------------------------------------
// djs: candidate C's realm, the same prelude and bundle in another interpreter
// ---------------------------------------------------------------------------

#[cfg(feature = "engine-djs")]
mod djs {
    use std::time::Instant;

    use djs_embed::{
        CallId, Data, Error, Exception, HostFn, Instance, Limits, Snapshot, Source, Step, Throw,
        Value,
    };
    use serde_json::json;

    use super::{log_plugin, log_timing, Engine, Failure, PRELUDE};
    use crate::model::SourceType;
    use crate::plugin::{HostBridge, WorkerConfig};

    /// The djs host function ids. `import()` takes `u32::MAX`, so anything below it is free.
    const HOST_HTTP: u32 = 1;
    const HOST_LOG: u32 = 2;
    /// The heap cap: the same 64 MiB the rquickjs realm gets.
    const MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
    /// Fuel per poll, about 16 s of work at 4 ns a unit: set wide on purpose, so the
    /// deadline (`STALL_POLL_DEADLINE_MS`, 10 s by default) is what stops a runaway plugin.
    /// That is the difference the spike measures: djs checks its deadline between slices,
    /// where rquickjs needs the interrupt handler above.
    const POLL_FUEL: u64 = 4_000_000_000;
    /// Setup runs before any host exists, so the entry point is checked inside the snapshot
    /// rather than through a host call: the same check the rquickjs realm makes after its
    /// bundle evaluates.
    const PLUGIN_CHECK: &str = "if (typeof globalThis.__stall.pluginPoll !== 'function') { \
        throw new Error('the bundle did not define globalThis.__stall.pluginPoll'); }";

    /// A djs realm: the snapshot built from the prelude and the bundle, and an instance
    /// copied from it. An instance serves one poll at a time; a reset costs a copy, not a
    /// compile.
    pub struct Realm {
        source_type: SourceType,
        bridge: HostBridge,
        // The instance keeps its snapshot (and its host functions) alive, so the realm
        // holds only what a poll needs.
        instance: Option<Instance>,
    }

    impl Realm {
        /// Snapshot build plus instantiate: what `realm-build` measures here.
        pub fn load(source_type: SourceType, config: &WorkerConfig) -> Result<Realm, String> {
            let started = Instant::now();
            let snapshot = build_snapshot(source_type, config)?;
            let instance = snapshot
                .instantiate(limits(config))
                .map_err(|error| format!("plugin {source_type} snapshot instantiate: {error}"))?;
            crate::log_line(
                "info",
                &format!(
                    "plugin {source_type} realm loaded from {} (memory limit {} MiB; snapshot {} bytes)",
                    config.plugin_dir.join(format!("{source_type}.js")).display(),
                    MEMORY_LIMIT_BYTES >> 20,
                    snapshot.image_len()
                ),
            );
            log_timing("realm-build", source_type, started.elapsed());
            Ok(Realm {
                source_type,
                bridge: HostBridge::new(source_type, config.host_calls.clone()),
                instance: Some(instance),
            })
        }

        /// After a failed poll: a budget failure is recovered by copying the snapshot back
        /// into the instance (`realm-reset`); a thrown exception leaves the realm idle and
        /// usable; anything else drops the instance, and the next poll builds a new
        /// snapshot. djs poisons only the instance a panic happened in, never the snapshot,
        /// so a rebuild inside this poll would only be discarded with the realm.
        fn recover(&mut self, error: Error) -> Failure {
            match error {
                Error::Deadline | Error::OutOfFuel { .. } => {
                    let started = Instant::now();
                    match self.instance.as_mut().map(Instance::reset) {
                        Some(Ok(())) => {
                            log_timing("realm-reset", self.source_type, started.elapsed());
                            Failure::Interrupted
                        }
                        Some(Err(error)) => {
                            self.instance = None;
                            Failure::Fatal(format!(
                                "plugin {} realm reset: {error}",
                                self.source_type
                            ))
                        }
                        None => {
                            Failure::Fatal(format!("plugin {} realm is unusable", self.source_type))
                        }
                    }
                }
                Error::Exception(exception) => Failure::Js(describe(&exception)),
                other => {
                    self.instance = None;
                    Failure::Fatal(format!("plugin {} realm: {other}", self.source_type))
                }
            }
        }
    }

    impl Engine for Realm {
        fn name(&self) -> &'static str {
            "djs"
        }

        fn poll(&mut self, input_json: &str) -> Result<String, Failure> {
            let Some(instance) = self.instance.as_mut() else {
                return Err(Failure::Fatal(format!(
                    "plugin {} realm is unusable",
                    self.source_type
                )));
            };
            match run(instance, &self.bridge, self.source_type, input_json) {
                Ok(envelope) => Ok(envelope),
                Err(error) => Err(self.recover(error)),
            }
        }
    }

    fn limits(config: &WorkerConfig) -> Limits {
        Limits {
            memory: MEMORY_LIMIT_BYTES,
            fuel: POLL_FUEL,
            // The engine's own slice, so the deadline is checked about every millisecond.
            slice: Limits::default().slice,
            deadline: Some(config.deadline),
        }
    }

    /// One poll: `__stallInvokePoll(input)` stepped to its envelope, answering the prelude's
    /// host calls as they are made.
    fn run(
        instance: &mut Instance,
        bridge: &HostBridge,
        source_type: SourceType,
        input_json: &str,
    ) -> Result<String, Error> {
        instance.start("__stallInvokePoll", &[Value::Str(input_json)])?;
        loop {
            match instance.step()? {
                Step::Done => {
                    let value = instance.result()?;
                    let Some(text) = value.as_str() else {
                        return Err(Error::Usage(format!(
                            "__stallInvokePoll returned {} instead of the envelope string",
                            value.type_name()
                        )));
                    };
                    return Ok(text.into_owned());
                }
                Step::Yield => {}
                Step::HostCall(id) => answer(instance, bridge, source_type, id)?,
                Step::Wait(ids) if ids.is_empty() => {
                    // Bounded: nothing can settle the poll's promise, so do not step forever.
                    return Err(Error::Usage(
                        "the poll's promise is pending and no host call can settle it".to_string(),
                    ));
                }
                Step::Wait(ids) => {
                    for id in ids {
                        answer(instance, bridge, source_type, id)?;
                    }
                }
            }
        }
    }

    /// Answers one host call the prelude made. Logging is synchronous; the http request is
    /// asynchronous and gets the same reply JSON the rquickjs host function returns. The
    /// bridge blocks this realm thread either way, exactly as it does under QuickJS.
    fn answer(
        instance: &mut Instance,
        bridge: &HostBridge,
        source_type: SourceType,
        id: CallId,
    ) -> Result<(), Error> {
        let call = instance.host_call(id)?;
        match call.fn_id() {
            HOST_HTTP => {
                let payload = call.arg(0).as_str().map(|text| text.into_owned());
                let Some(payload) = payload else {
                    return call.reply(Err(Throw::type_error(
                        "__stallHostHttp: the request payload is not a string",
                    )));
                };
                call.reply(Ok(Value::Str(&bridge.handle(&payload))))
            }
            HOST_LOG => {
                let level = call.arg(0).as_str().map(|text| text.into_owned());
                let message = call.arg(1).as_str().map(|text| text.into_owned());
                log_plugin(
                    source_type,
                    level.as_deref().unwrap_or_default(),
                    message.as_deref().unwrap_or_default(),
                );
                call.reply(Ok(Value::Undefined))
            }
            other => call.reply(Err(Throw::type_error(format!(
                "__stallHostHttp and __stallLog are the only host functions; got {other}"
            )))),
        }
    }

    /// The image both the prelude and the bundle are compiled into, in the order the
    /// rquickjs realm evaluates them: the prelude, the bundle, then the entry-point check.
    fn build_snapshot(source_type: SourceType, config: &WorkerConfig) -> Result<Snapshot, String> {
        let path = config.plugin_dir.join(format!("{source_type}.js"));
        let bundle = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read plugin bundle {}: {error}", path.display()))?;
        let realm_config = serde_json::to_string(&json!({
            "plugin": source_type.as_str(),
            "faults": config.faults,
            "env": crate::plugin::realm_env(),
        }))
        .map_err(|error| format!("realm config json: {error}"))?;
        let bundle_name = path.display().to_string();

        Snapshot::builder()
            .host_fn(HostFn::asynchronous(HOST_HTTP, "__stallHostHttp"))
            .host_fn(HostFn::sync(HOST_LOG, "__stallLog"))
            .global("__stall_cfg", Data::from(realm_config))
            .source(Source::script("prelude.js", PRELUDE))
            .source(Source::script(bundle_name.clone(), bundle))
            .source(Source::script("plugin-entry.js", PLUGIN_CHECK))
            .build()
            .map_err(|error| format!("plugin bundle {bundle_name} did not load: {error}"))
    }

    /// A thrown value as `Failure::Js` reports it: message and stack, in the shape the
    /// rquickjs realm's `describe_thrown` produces.
    fn describe(exception: &Exception) -> String {
        match exception.stack.as_deref() {
            Some(stack) if !stack.is_empty() => format!("{}\n{stack}", exception.message),
            _ => exception.message.clone(),
        }
    }
}
