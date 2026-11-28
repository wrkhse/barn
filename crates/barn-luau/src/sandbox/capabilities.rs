use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use mlua::{Lua, Table, Value as LuaValue, Variadic};
use serde_json::Value;

use super::deadline::Deadline;
use super::errors::{EngineError, LimitCause};
use super::values::{json_to_lua, lua_to_json};

type Callback = dyn Fn(Value) -> Result<Value, String> + Send + Sync;
type GroupCallback = dyn Fn(Vec<Value>) -> Result<Value, String> + Send + Sync;

/// Host callbacks made available in a plugin's per-call context.
#[derive(Default)]
pub struct Capabilities {
    callbacks: HashMap<String, Arc<Callback>>,
    groups: HashMap<(String, String), Arc<GroupCallback>>,
}

impl Capabilities {
    /// Register or replace a callback invoked as `ctx.name(argument)`.
    ///
    /// The names `allow`, `deny`, `rewrite`, `annotate`, and `drop` are
    /// reserved. Callbacks run synchronously; they must bound their own work
    /// and must not re-enter the same sandbox.
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        callback: impl Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
    ) {
        self.callbacks.insert(name.into(), Arc::new(callback));
    }

    /// Register or replace a grouped callback invoked as `ctx.group.name(...)`.
    ///
    /// The callback receives every Lua argument converted to a JSON value.
    /// The group names `allow`, `deny`, `rewrite`, `annotate`, and `drop` are
    /// reserved. Callbacks run synchronously; they must bound their own work
    /// and must not re-enter the same sandbox.
    pub fn insert_group(
        &mut self,
        group: impl Into<String>,
        name: impl Into<String>,
        callback: impl Fn(Vec<Value>) -> Result<Value, String> + Send + Sync + 'static,
    ) {
        self.groups
            .insert((group.into(), name.into()), Arc::new(callback));
    }

    pub(super) fn validate(&self) -> Result<(), EngineError> {
        for name in ["allow", "deny", "rewrite", "annotate", "drop"] {
            if self.callbacks.contains_key(name) {
                return Err(EngineError::InvalidResult(format!(
                    "reserved capability name: {name}"
                )));
            }
        }

        for (group, _) in self.groups.keys() {
            if ["allow", "deny", "rewrite", "annotate", "drop"].contains(&group.as_str())
                || self.callbacks.contains_key(group)
            {
                return Err(EngineError::InvalidResult(format!(
                    "reserved or conflicting group name: {group}"
                )));
            }
        }

        Ok(())
    }

    pub(super) fn populate(
        &self,
        lua: &Lua,
        ctx: &Table,
        deadline: &Arc<Deadline>,
    ) -> Result<(), EngineError> {
        for (name, callback) in &self.callbacks {
            let capability = name.clone();
            let callback = Arc::clone(callback);
            let deadline = Arc::clone(deadline);

            let function = lua.create_function(move |lua, input: LuaValue| {
                let input = lua_to_json(input, 0).map_err(mlua::Error::external)?;
                check_deadline(&deadline)?;

                let result = callback(input);
                check_deadline(&deadline)?;

                let output = result.map_err(|reason| {
                    mlua::Error::external(EngineError::Capability {
                        name: capability.clone(),
                        reason,
                    })
                })?;

                json_to_lua(lua, &output, 0).map_err(mlua::Error::external)
            })?;
            ctx.raw_set(name.as_str(), function)?;
        }

        let mut grouped: HashMap<&str, Vec<(&str, &Arc<GroupCallback>)>> = HashMap::new();
        for ((group, method), callback) in &self.groups {
            grouped
                .entry(group.as_str())
                .or_default()
                .push((method.as_str(), callback));
        }

        for (group, methods) in grouped {
            let table = lua.create_table()?;

            for (method, callback) in methods {
                let capability = format!("{group}.{method}");
                let callback = Arc::clone(callback);
                let deadline = Arc::clone(deadline);

                let function = lua.create_function(move |lua, args: Variadic<LuaValue>| {
                    let args = args
                        .iter()
                        .map(|arg| lua_to_json(arg.clone(), 0).map_err(mlua::Error::external))
                        .collect::<Result<Vec<_>, _>>()?;
                    check_deadline(&deadline)?;

                    let result = callback(args);
                    check_deadline(&deadline)?;

                    let output = result.map_err(|reason| {
                        mlua::Error::external(EngineError::Capability {
                            name: capability.clone(),
                            reason,
                        })
                    })?;

                    json_to_lua(lua, &output, 0).map_err(mlua::Error::external)
                })?;
                table.raw_set(method, function)?;
            }

            ctx.raw_set(group, table)?;
        }

        Ok(())
    }
}

impl fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Capabilities")
            .field("count", &self.callbacks.len())
            .finish_non_exhaustive()
    }
}

fn check_deadline(deadline: &Deadline) -> mlua::Result<()> {
    if deadline.expired() {
        return Err(mlua::Error::external(LimitCause::Deadline));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::clock::FakeClock;

    #[test]
    fn debug_reports_only_the_callback_count() {
        let mut capabilities = Capabilities::default();
        capabilities.insert("lookup", Ok);

        assert_eq!(format!("{capabilities:?}"), "Capabilities { count: 1, .. }");
    }

    #[test]
    fn a_capability_refuses_an_expired_deadline() {
        let lua = Lua::new();
        let ctx = lua.create_table().expect("create context");
        let deadline = Arc::new(Deadline::with_clock(Arc::new(FakeClock::new())));
        let mut capabilities = Capabilities::default();
        capabilities.insert("lookup", |_| Ok(Value::Bool(true)));
        capabilities
            .populate(&lua, &ctx, &deadline)
            .expect("populate context");
        let lookup: mlua::Function = ctx.get("lookup").expect("registered callback");

        // Call the host function directly so a VM interrupt cannot provide the error.
        deadline.begin(0);
        assert_eq!(
            lookup.call::<bool>(()).map_err(EngineError::from),
            Err(EngineError::Limit(LimitCause::Deadline))
        );
    }
}
