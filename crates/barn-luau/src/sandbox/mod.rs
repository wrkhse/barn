//! One mlua VM per plugin, bounded by a memory ceiling and a per-call clock.
//!
//! The plugin is a Luau table at plugin.luau: named hooks and a tools table.
//! A hook is a function or a bounded hook table: one handler with `priority`,
//! `budgetMs`, `onError` and `run`, or an array of such handlers run from
//! the highest priority down. A handler returns nil to allow or pass on, or a
//! table with exactly one verdict: allow, deny, rewrite, annotate or drop,
//! built by the matching `ctx` helper. `ctx.allow()` is the explicit allow
//! verdict. A runnable returns a JSON-compatible value. Every input crossing
//! the boundary is converted under a depth and table-size ceiling.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use mlua::{Function, Lua, Table, Value as LuaValue, VmState};
use serde_json::{Value, json};

use crate::api::{Invocation, Outcome};
use crate::require;

mod call;
mod capabilities;
mod clock;
mod deadline;
mod errors;
mod hook_table;
mod hook_table_decode;
mod loading;
mod outcome;
#[cfg(test)]
mod tests;
mod values;

pub use capabilities::Capabilities;
pub use clock::Clock;
pub use errors::{EngineError, LimitCause, LoadCause};

use deadline::Deadline;
use errors::MAX_ENTRIES;
use loading::load_plugin;
use outcome::decode_outcome;
use values::{json_to_lua, lua_key, lua_to_json, read_tool, tool_named};

// The supported floor for the VM, its sandbox globals, and plugin startup.
const MIN_MEMORY_BYTES: usize = 16 * 1024 * 1024;

/// The host's bounds for a single sandbox.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    memory_bytes: usize,
    call_deadline_ms: u64,
}

impl Limits {
    /// Set the Luau allocator ceiling and the budget in milliseconds for
    /// startup, manifest reads, and each call.
    ///
    /// # Errors
    ///
    /// Returns a memory limit error for ceilings below the 16 MiB VM baseline.
    pub const fn new(memory_bytes: usize, call_deadline_ms: u64) -> Result<Self, EngineError> {
        if memory_bytes < MIN_MEMORY_BYTES {
            return Err(EngineError::Limit(LimitCause::Memory));
        }

        Ok(Self {
            memory_bytes,
            call_deadline_ms,
        })
    }
}

/// A single plugin VM with its own limits and capabilities.
#[derive(Debug)]
pub struct Sandbox {
    lua: Lua,
    plugin: Table,
    deadline: Arc<Deadline>,
    call_lock: Mutex<()>,
    call_deadline_ms: u64,
    capabilities: Capabilities,
}

impl Sandbox {
    /// Build a sandbox rooted at `folder` under the given bounds, timed by `clock`.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Load`] when the folder or entry module cannot be
    /// loaded, [`EngineError::Limit`] when startup exhausts a bound, and
    /// [`EngineError::InvalidResult`] when a capability uses a reserved name.
    pub fn new(
        folder: &Path,
        limits: Limits,
        capabilities: Capabilities,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, EngineError> {
        capabilities.validate()?;
        let lua = Lua::new();
        lua.set_memory_limit(limits.memory_bytes)?;
        let state = require::RequireState::new(folder)
            .map_err(|message| EngineError::Load(LoadCause::Message(message)))?;
        let loader = require::install(&lua, state)?;
        require::confine_global_require(&lua)?;
        lua.sandbox(true)?;

        let deadline = Arc::new(Deadline::with_clock(clock));
        let watchdog = Arc::clone(&deadline);
        lua.set_interrupt(move |_| {
            if watchdog.expired() {
                Err(mlua::Error::external(LimitCause::Deadline))
            } else {
                Ok(VmState::Continue)
            }
        });

        // A top-level loop must not hang startup either. The deadline is
        // armed around the entry chunk, then disarmed before the VM is shared.
        deadline.begin(limits.call_deadline_ms);
        let plugin = load_plugin(&loader);
        deadline.disarm();

        Ok(Self {
            lua,
            plugin: plugin?,
            deadline,
            call_lock: Mutex::new(()),
            call_deadline_ms: limits.call_deadline_ms,
            capabilities,
        })
    }

    /// Take the call lock, refusing to run after a caller panicked holding it.
    fn lock_call(&self) -> Result<MutexGuard<'_, ()>, EngineError> {
        self.call_lock.lock().map_err(|_| EngineError::Poisoned)
    }

    /// Run a named hook and decode its verdict within the smaller of the two deadlines.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Limit`] when the call exceeds its deadline or
    /// memory ceiling, [`EngineError::InvalidResult`] when the hook returns
    /// a malformed result, [`EngineError::Capability`] when a callback error
    /// reaches the host, and [`EngineError::Poisoned`] after a call panics.
    pub fn hook(&self, invocation: &Invocation, deadline_ms: u64) -> Result<Outcome, EngineError> {
        let _guard = self.lock_call()?;
        let cap_ms = deadline_ms.min(self.call_deadline_ms);
        let call_start = self.deadline.micros();
        self.deadline.begin(cap_ms);
        let outcome = self.run_hook(invocation, cap_ms, call_start);
        self.deadline.disarm();
        outcome
    }

    /// Run a named runnable and convert its result to JSON within the smaller deadline.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::UnknownTool`] when no such tool exists,
    /// [`EngineError::Limit`] when the call is bounded,
    /// [`EngineError::InvalidResult`] when the tool returns malformed content,
    /// [`EngineError::Capability`] when a callback error reaches the host,
    /// and [`EngineError::Poisoned`] after a call panics.
    pub fn run(&self, name: &str, input: &Value, deadline_ms: u64) -> Result<Value, EngineError> {
        let _guard = self.lock_call()?;
        self.deadline.begin(deadline_ms.min(self.call_deadline_ms));
        let result = self.run_runnable(name, input);
        self.deadline.disarm();
        result
    }

    /// Read the plugin's name, needs, and array of tool metadata as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::InvalidResult`] when the manifest is malformed
    /// or repeats a tool name, [`EngineError::Limit`] when reading is bounded,
    /// and [`EngineError::Poisoned`] when a previous call panicked while
    /// holding the call lock.
    pub fn manifest(&self) -> Result<Value, EngineError> {
        let _guard = self.lock_call()?;
        self.deadline.begin(self.call_deadline_ms);
        let manifest = self.read_manifest();
        self.deadline.disarm();
        manifest
    }

    fn run_hook(
        &self,
        invocation: &Invocation,
        call_cap_ms: u64,
        call_start: u128,
    ) -> Result<Outcome, EngineError> {
        let value = self.plugin.get::<LuaValue>(invocation.hook.as_str())?;

        match value {
            LuaValue::Nil => Ok(Outcome::Allow),
            LuaValue::Function(function) => {
                let payload = json_to_lua(&self.lua, &invocation.payload, 0)?;
                let result =
                    function.call::<LuaValue>((payload, self.ctx(&invocation.context)?))?;

                decode_outcome(result)
            }
            LuaValue::Table(value) => {
                const RESERVED: [&str; 3] = ["name", "needs", "tools"];
                let name = invocation.hook.as_str();
                if RESERVED.contains(&name) {
                    return Err(EngineError::InvalidResult(format!(
                        "reserved hook name: {name}"
                    )));
                }

                let table = self::hook_table_decode::decode_hook_table(
                    LuaValue::Table(value),
                    call_cap_ms,
                )?;

                Ok(self::hook_table::run_table_inner(
                    self,
                    &table,
                    invocation,
                    call_cap_ms,
                    call_start,
                ))
            }
            _ => Err(EngineError::InvalidResult(
                "hook must be a function or table".into(),
            )),
        }
    }

    fn run_runnable(&self, name: &str, input: &Value) -> Result<Value, EngineError> {
        let tools = self
            .plugin
            .get::<Option<Table>>("tools")?
            .ok_or_else(|| EngineError::UnknownTool(name.to_owned()))?;
        let Some(definition) = tool_named(&tools, name)? else {
            return Err(EngineError::UnknownTool(name.to_owned()));
        };
        let run = definition
            .get::<Option<Function>>("run")?
            .ok_or_else(|| EngineError::UnknownTool(name.to_owned()))?;
        let lua_input = json_to_lua(&self.lua, input, 0)?;
        let result = run.call::<LuaValue>((lua_input, self.ctx(&Value::Null)?))?;

        lua_to_json(result, 0)
    }
}

impl Sandbox {
    fn read_manifest(&self) -> Result<Value, EngineError> {
        let name = self
            .plugin
            .get::<Option<String>>("name")?
            .unwrap_or_default();
        if name.is_empty() {
            return Err(EngineError::InvalidResult("plugin name is empty".into()));
        }

        Ok(json!({
            "name": name,
            "needs": self.read_needs()?,
            "tools": self.read_tools()?,
        }))
    }

    fn read_needs(&self) -> Result<Vec<String>, EngineError> {
        let Some(value) = self.plugin.get::<Option<LuaValue>>("needs")? else {
            return Ok(Vec::new());
        };
        let LuaValue::Table(table) = value else {
            return Err(EngineError::InvalidResult("needs must be an array".into()));
        };
        let mut needs = Vec::new();
        for pair in table.pairs::<LuaValue, LuaValue>() {
            if needs.len() >= MAX_ENTRIES {
                return Err(EngineError::Limit(LimitCause::TooManyEntries {
                    limit: MAX_ENTRIES,
                }));
            }
            let (_, item) = pair?;
            let need = match item {
                LuaValue::String(value) => value.to_str()?.to_string(),
                _ => {
                    return Err(EngineError::InvalidResult(
                        "needs must contain strings".into(),
                    ));
                }
            };
            needs.push(need);
        }
        Ok(needs)
    }

    fn read_tools(&self) -> Result<Vec<Value>, EngineError> {
        let Some(tools) = self.plugin.get::<Option<Table>>("tools")? else {
            return Ok(Vec::new());
        };
        let mut definitions = Vec::new();
        let mut seen = HashSet::new();
        for pair in tools.pairs::<LuaValue, LuaValue>() {
            if definitions.len() >= MAX_ENTRIES {
                return Err(EngineError::Limit(LimitCause::TooManyEntries {
                    limit: MAX_ENTRIES,
                }));
            }
            let (key, value) = pair?;
            let key = lua_key(key)?;
            let LuaValue::Table(table) = value else {
                return Err(EngineError::InvalidResult("tool must be a table".into()));
            };
            let (name, definition) = read_tool(&table, &key)?;
            if name.is_empty() {
                return Err(EngineError::InvalidResult("empty tool name".into()));
            }
            if seen.contains(&name) {
                return Err(EngineError::InvalidResult(format!(
                    "duplicate tool name: {name}"
                )));
            }
            seen.insert(name);
            definitions.push(definition);
        }
        Ok(definitions)
    }
}

#[cfg(test)]
mod hook_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};

    use crate::api::Invocation;

    use super::clock::{Clock, FakeClock};
    use super::tests::Scratch;
    use super::{Capabilities, EngineError, Limits, Outcome, Sandbox};

    const MEMORY: usize = 16 * 1024 * 1024;
    const DEADLINE_MS: u64 = 1000;
    const DEADLINE_REASON: &str = "sandbox deadline exceeded";
    const TIMEOUT_REASON: &str = "hook handler budget exceeded";

    const PLUGIN: &str = r#"
    return {
        name = "hook-table",
        needs = {},
        tools = {},
        array = {
            {
                run = function(payload, ctx)
                    return ctx.deny("array verdict")
                end,
            },
        },
        single = {
            priority = 3,
            run = function(payload, ctx)
                return ctx.deny("single verdict")
            end,
        },
        scalar = 7,
        spend = {
            run = function(payload, ctx)
                ctx.advance(payload.us)
                return ctx.deny("kept")
            end,
        },
        budgeted = {
            budgetMs = 5,
            run = function(payload, ctx)
                ctx.advance(payload.us)
                return ctx.deny("kept")
            end,
        },
        low_first = {
            {
                priority = 1,
                run = function(payload, ctx)
                    return ctx.deny("low first")
                end,
            },
            {
                priority = 9,
                run = function(payload, ctx)
                    return ctx.deny("high second")
                end,
            },
        },
        high_first = {
            {
                priority = 9,
                run = function(payload, ctx)
                    return ctx.deny("high first")
                end,
            },
            {
                priority = 1,
                run = function(payload, ctx)
                    return ctx.deny("low second")
                end,
            },
        },
    }
    "#;

    fn fixture() -> (Scratch, Arc<FakeClock>, Sandbox) {
        let scratch = Scratch::new();
        scratch.write("plugin.luau", PLUGIN);
        let fake = Arc::new(FakeClock::new());
        let step = Arc::clone(&fake);
        let mut capabilities = Capabilities::default();
        capabilities.insert("advance", move |value: Value| -> Result<Value, String> {
            let micros = value.as_u64().unwrap_or_default();
            step.advance(Duration::from_micros(micros));
            Ok(Value::Null)
        });
        let clock: Arc<dyn Clock> = fake.clone();
        let limits = Limits::new(MEMORY, DEADLINE_MS).expect("valid limits");
        let sandbox =
            Sandbox::new(scratch.path(), limits, capabilities, clock).expect("fixture loads");

        (scratch, fake, sandbox)
    }

    fn call(hook: &str) -> Invocation {
        Invocation {
            hook: hook.into(),
            payload: Value::Null,
            context: Value::Null,
        }
    }

    #[test]
    fn an_array_hook_reaches_a_verdict() {
        let (_scratch, _fake, sandbox) = fixture();

        assert_eq!(
            sandbox.hook(&call("array"), DEADLINE_MS),
            Ok(Outcome::Deny {
                reason: "array verdict".into()
            })
        );
    }

    #[test]
    fn a_single_handler_table_reaches_a_verdict() {
        let (_scratch, _fake, sandbox) = fixture();

        assert_eq!(
            sandbox.hook(&call("single"), DEADLINE_MS),
            Ok(Outcome::Deny {
                reason: "single verdict".into()
            })
        );
    }

    #[test]
    fn a_non_function_non_table_hook_names_the_shape() {
        let (_scratch, _fake, sandbox) = fixture();

        assert_eq!(
            sandbox.hook(&call("scalar"), DEADLINE_MS),
            Err(EngineError::InvalidResult(
                "hook must be a function or table".into()
            ))
        );
    }

    #[test]
    fn a_reserved_plugin_key_cannot_be_a_hook() {
        let (_scratch, _fake, sandbox) = fixture();

        assert_eq!(
            sandbox.hook(&call("tools"), DEADLINE_MS),
            Err(EngineError::InvalidResult(
                "reserved hook name: tools".into()
            ))
        );
    }

    #[test]
    fn a_handler_without_a_budget_uses_the_call_cap() {
        let (_scratch, _fake, sandbox) = fixture();
        let mut under = call("spend");
        under.payload = json!({ "us": 4_999 });

        assert_eq!(
            sandbox.hook(&under, 5),
            Ok(Outcome::Deny {
                reason: "kept".into()
            })
        );

        let (_other, _clock, at_cap) = fixture();
        let mut exact = call("spend");
        exact.payload = json!({ "us": 5_000 });

        assert_eq!(
            at_cap.hook(&exact, 5),
            Ok(Outcome::Deny {
                reason: DEADLINE_REASON.into()
            })
        );
    }

    #[test]
    fn a_budget_set_in_the_lua_handler_table_is_honored() {
        let (_scratch, _clock, sandbox) = fixture();
        let mut under = call("budgeted");
        under.payload = json!({ "us": 4_999 });

        assert_eq!(
            sandbox.hook(&under, 5),
            Ok(Outcome::Deny {
                reason: "kept".into()
            })
        );

        let (_other, _other_clock, at_cap) = fixture();
        let mut exact = call("budgeted");
        exact.payload = json!({ "us": 5_000 });

        assert_eq!(
            at_cap.hook(&exact, 5),
            Ok(Outcome::Deny {
                reason: DEADLINE_REASON.into()
            })
        );

        let (_third, _third_clock, own_budget) = fixture();
        let mut own = call("budgeted");
        own.payload = json!({ "us": 5_000 });

        assert_eq!(
            own_budget.hook(&own, 10),
            Ok(Outcome::Deny {
                reason: TIMEOUT_REASON.into()
            })
        );
    }

    #[test]
    fn each_array_handler_is_bound_to_its_own_function_in_priority_order() {
        let (_scratch, _fake, sandbox) = fixture();

        assert_eq!(
            sandbox.hook(&call("low_first"), DEADLINE_MS),
            Ok(Outcome::Deny {
                reason: "high second".into()
            })
        );
        assert_eq!(
            sandbox.hook(&call("high_first"), DEADLINE_MS),
            Ok(Outcome::Deny {
                reason: "high first".into()
            })
        );
    }
}
