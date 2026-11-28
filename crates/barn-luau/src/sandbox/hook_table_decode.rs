//! Decode the three hook-table forms described in tack#122.
//!
//! A hook's Lua value is either one function, one handler table (`priority`,
//! `budgetMs`, `onError`, `run`), or an array of handler tables. This module
//! turns those forms into one bounded [`HookTable`]. Every refusal names the
//! field and the offending value so a plugin author can correct it directly.

use mlua::{Function, Table, Value as LuaValue};

use super::errors::{EngineError, LimitCause};
use super::hook_table::{HookHandler, HookRun, HookTable, OnError};
use crate::sandbox::errors::MAX_HANDLERS;

/// A bare function carries no ordering weight, so it starts at zero.
const DEFAULT_PRIORITY: i32 = 0;

const SPARSE: &str = "hook table array is sparse";
const NO_RUN: &str = "run must be a function";
const FIELD_KEYS: [&str; 4] = ["priority", "budgetMs", "onError", "run"];

/// The one error shape for a field with the wrong type or out-of-range value.
fn invalid(field: &str, expected: &str, value: &LuaValue) -> EngineError {
    EngineError::InvalidResult(format!("{field} must be {expected}, got {value:?}"))
}

fn sparse() -> EngineError {
    EngineError::InvalidResult(SPARSE.into())
}

fn no_run() -> EngineError {
    EngineError::InvalidResult(NO_RUN.into())
}

/// Read an optional integral `i32`, using `default` when the field is absent.
///
/// Luau stores whole numbers as integers, so only `Integer` is accepted.
/// Floats are refused rather than lossily converted to an integer.
fn integral_i32(table: &Table, field: &str, default: i32) -> Result<i32, EngineError> {
    const EXPECTED: &str = "an integral i32";
    match table.raw_get::<LuaValue>(field)? {
        LuaValue::Nil => Ok(default),
        LuaValue::Integer(number) => {
            i32::try_from(number).map_err(|_| invalid(field, EXPECTED, &LuaValue::Integer(number)))
        }
        other => Err(invalid(field, EXPECTED, &other)),
    }
}

/// Read the optional handler budget; an explicit zero is never a valid budget.
///
/// Only `Integer` is accepted; floats are refused rather than converted.
fn budget_ms(table: &Table, default: u64) -> Result<u64, EngineError> {
    const EXPECTED: &str = "a non-negative u64";
    match table.raw_get::<LuaValue>("budgetMs")? {
        LuaValue::Nil => Ok(default),
        LuaValue::Integer(number) => match u64::try_from(number) {
            Ok(0) => Err(EngineError::InvalidResult(
                "budgetMs must be positive".into(),
            )),
            Ok(value) => Ok(value),
            Err(_) => Err(invalid("budgetMs", EXPECTED, &LuaValue::Integer(number))),
        },
        other => Err(invalid("budgetMs", EXPECTED, &other)),
    }
}

/// Read the optional error policy; a missing field denies.
fn decode_on_error(table: &Table) -> Result<OnError, EngineError> {
    match table.raw_get::<LuaValue>("onError")? {
        LuaValue::Nil => Ok(OnError::Deny),
        LuaValue::String(value) => {
            let name = value.to_str()?.to_owned();
            match name.as_str() {
                "deliver" => Ok(OnError::Deliver),
                "deny" => Ok(OnError::Deny),
                _ => Err(invalid(
                    "onError",
                    "deliver or deny",
                    &LuaValue::String(value),
                )),
            }
        }
        other => Err(invalid("onError", "deliver or deny", &other)),
    }
}

/// A handler table may name only the four fields the engine understands.
fn validate_keys(table: &Table) -> Result<(), EngineError> {
    for pair in table.pairs::<LuaValue, LuaValue>() {
        let (key, _) = pair?;
        let allowed = match &key {
            LuaValue::String(name) => {
                let name = name.to_str()?.to_owned();
                FIELD_KEYS.contains(&name.as_str())
            }
            _ => false,
        };
        if !allowed {
            return Err(EngineError::InvalidResult(format!(
                "unknown handler key: {key:?}"
            )));
        }
    }
    Ok(())
}

/// The single-function form: defaults throughout, with no ordering weight.
fn from_function(function: Function, default_budget_ms: u64) -> HookHandler {
    HookHandler {
        priority: DEFAULT_PRIORITY,
        budget_ms: default_budget_ms,
        on_error: OnError::Deny,
        run: HookRun::new(function),
    }
}

/// Decode one handler table from its already-read `run` field.
fn decode_handler(
    table: &Table,
    run: &LuaValue,
    default_budget_ms: u64,
) -> Result<HookHandler, EngineError> {
    let LuaValue::Function(function) = run else {
        return Err(invalid("handler run", "a function", run));
    };
    let priority = integral_i32(table, "priority", DEFAULT_PRIORITY)?;
    let budget = budget_ms(table, default_budget_ms)?;
    let on_error = decode_on_error(table)?;
    validate_keys(table)?;
    Ok(HookHandler {
        priority,
        budget_ms: budget,
        on_error,
        run: HookRun::new(function.clone()),
    })
}

/// Decode the array form: one-based integer-keyed handler tables.
///
/// The bound is checked before any read or allocation so an oversized array
/// is refused without walking it. Sorting is left to [`HookTable::new`],
/// whose stable sort keeps equal priorities in their input order.
fn decode_array(table: &Table, default_budget_ms: u64) -> Result<HookTable, EngineError> {
    let len = table.raw_len();
    if len > MAX_HANDLERS {
        return Err(EngineError::Limit(LimitCause::TooManyHandlers {
            limit: MAX_HANDLERS,
        }));
    }

    let mut handlers = Vec::with_capacity(len);
    for index in 1..=len {
        let value = table.raw_get::<LuaValue>(index)?;
        if matches!(&value, LuaValue::Nil) {
            return Err(sparse());
        }
        let LuaValue::Table(handler) = &value else {
            return Err(invalid("handler", "a table", &value));
        };
        let run = handler.raw_get::<LuaValue>("run")?;
        handlers.push(decode_handler(handler, &run, default_budget_ms)?);
    }

    // Bounded: the first key the array cannot account for ends the walk. A
    // table with no array part at all was a handler table with no `run`.
    for pair in table.pairs::<LuaValue, LuaValue>() {
        let (key, _) = pair?;
        match key {
            LuaValue::Integer(key) => {
                let Ok(index) = usize::try_from(key) else {
                    return Err(sparse());
                };
                if !(1..=len).contains(&index) {
                    return Err(sparse());
                }
            }
            _ if len == 0 => return Err(no_run()),
            other => {
                return Err(EngineError::InvalidResult(format!(
                    "hook table array has an unexpected key: {other:?}"
                )));
            }
        }
    }

    HookTable::new(handlers)
}

/// Decode a hook value into a bounded, priority-ordered table.
///
/// # Errors
///
/// Returns [`EngineError::InvalidResult`] when the value is neither a function
/// nor a table, when a handler field has the wrong type or range, or when the
/// array form is sparse. Returns [`EngineError::Limit`] when there are too
/// many handlers.
pub(super) fn decode_hook_table(
    value: mlua::Value,
    default_budget_ms: u64,
) -> Result<crate::sandbox::hook_table::HookTable, crate::sandbox::errors::EngineError> {
    match value {
        LuaValue::Function(function) => {
            HookTable::new(vec![from_function(function, default_budget_ms)])
        }
        LuaValue::Table(table) => {
            let run = table.raw_get::<LuaValue>("run")?;
            if matches!(&run, LuaValue::Function(_)) {
                let handler = decode_handler(&table, &run, default_budget_ms)?;
                HookTable::new(vec![handler])
            } else {
                decode_array(&table, default_budget_ms)
            }
        }
        other => Err(invalid("hook", "a function or table", &other)),
    }
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    use super::*;

    /// A callable used only to populate `run`; the table's `Debug` hides it.
    fn noop(lua: &Lua) -> Function {
        lua.create_function(|_, ()| Ok::<(), mlua::Error>(()))
            .expect("function")
    }

    fn handler_table(lua: &Lua) -> Table {
        let table = lua.create_table().expect("handler table");
        table.set("run", noop(lua)).expect("set run");
        table
    }

    fn expected_handler(
        lua: &Lua,
        priority: i32,
        budget_ms: u64,
        on_error: OnError,
    ) -> HookHandler {
        HookHandler {
            priority,
            budget_ms,
            on_error,
            run: HookRun::new(noop(lua)),
        }
    }

    /// `HookRun` is intentionally opaque, so compare the decoded table's
    /// `Debug` with the expected handlers' `Debug` in the same shape: that
    /// checks handler count, order, priority, budget and error policy exactly.
    fn assert_decoded(decoded: &HookTable, expected: &[HookHandler]) {
        assert_eq!(
            format!("{decoded:?}"),
            format!("HookTable {{ handlers: {expected:?} }}")
        );
    }

    fn assert_invalid(result: Result<HookTable, EngineError>, expected: &str) {
        assert_eq!(
            result.expect_err("decoding must fail"),
            EngineError::InvalidResult(expected.to_owned())
        );
    }

    fn with_field(lua: &Lua, field: &str, value: LuaValue) -> Table {
        let table = handler_table(lua);
        table.set(field, value).expect("set field");
        table
    }

    #[test]
    fn a_function_decodes_to_one_handler_with_defaults() {
        let lua = Lua::new();
        let decoded = decode_hook_table(LuaValue::Function(noop(&lua)), 250).expect("decode");

        assert_decoded(&decoded, &[expected_handler(&lua, 0, 250, OnError::Deny)]);
    }

    #[test]
    fn a_handler_table_reads_every_field() {
        let lua = Lua::new();
        let handler = handler_table(&lua);
        handler.set("priority", -3i64).expect("set priority");
        handler.set("budgetMs", 42i64).expect("set budget");
        handler.set("onError", "deliver").expect("set onError");

        let decoded = decode_hook_table(LuaValue::Table(handler), 250).expect("decode");
        assert_decoded(
            &decoded,
            &[expected_handler(&lua, -3, 42, OnError::Deliver)],
        );
    }

    #[test]
    fn a_handler_table_defaults_missing_fields() {
        let lua = Lua::new();
        let decoded = decode_hook_table(LuaValue::Table(handler_table(&lua)), 75).expect("decode");
        assert_decoded(&decoded, &[expected_handler(&lua, 0, 75, OnError::Deny)]);
    }

    #[test]
    fn on_error_accepts_both_spellings() {
        let lua = Lua::new();
        for (name, expected) in [("deliver", OnError::Deliver), ("deny", OnError::Deny)] {
            let handler = with_field(
                &lua,
                "onError",
                LuaValue::String(lua.create_string(name).expect("string")),
            );
            let decoded = decode_hook_table(LuaValue::Table(handler), 250).expect("decode");

            assert_decoded(&decoded, &[expected_handler(&lua, 0, 250, expected)]);
        }
    }

    #[test]
    fn an_array_decodes_in_index_order_with_stable_ties() {
        let lua = Lua::new();
        let array = lua.create_table().expect("array");
        for (index, priority, budget) in [(1i64, 5i64, 10i64), (2, 5, 20), (3, 1, 30)] {
            let handler = handler_table(&lua);
            handler.set("priority", priority).expect("set priority");
            handler.set("budgetMs", budget).expect("set budget");
            array.set(index, handler).expect("set handler");
        }

        let decoded = decode_hook_table(LuaValue::Table(array), 250).expect("decode");
        assert_decoded(
            &decoded,
            &[
                expected_handler(&lua, 5, 10, OnError::Deny),
                expected_handler(&lua, 5, 20, OnError::Deny),
                expected_handler(&lua, 1, 30, OnError::Deny),
            ],
        );
    }

    #[test]
    fn an_empty_array_yields_an_empty_table() {
        let lua = Lua::new();
        let empty = lua.create_table().expect("empty array");

        let decoded = decode_hook_table(LuaValue::Table(empty), 250).expect("decode");
        assert_decoded(&decoded, &[]);
    }

    #[test]
    fn a_sparse_array_is_refused() {
        let lua = Lua::new();
        let array = lua.create_table().expect("array");
        array.set(1i64, handler_table(&lua)).expect("set first");
        array.set(3i64, handler_table(&lua)).expect("set third");

        assert_invalid(
            decode_hook_table(LuaValue::Table(array), 250),
            "hook table array is sparse",
        );
    }

    #[test]
    fn a_non_function_run_is_refused() {
        let lua = Lua::new();
        let handler = lua.create_table().expect("handler table");
        handler.set("run", "not a function").expect("set run");
        assert_invalid(
            decode_hook_table(LuaValue::Table(handler), 250),
            r#"handler run must be a function, got String("not a function")"#,
        );

        let array = lua.create_table().expect("array");
        array
            .set(1i64, lua.create_table().expect("element"))
            .expect("set element");
        assert_invalid(
            decode_hook_table(LuaValue::Table(array), 250),
            "handler run must be a function, got Nil",
        );
    }

    #[test]
    fn priority_refuses_non_integral_values() {
        let lua = Lua::new();
        for (value, expected) in [
            (
                LuaValue::String(lua.create_string("high").expect("string")),
                r#"priority must be an integral i32, got String("high")"#,
            ),
            (
                LuaValue::Number(f64::NAN),
                "priority must be an integral i32, got Number(NaN)",
            ),
            (
                LuaValue::Number(f64::INFINITY),
                "priority must be an integral i32, got Number(inf)",
            ),
            (
                LuaValue::Number(1.5),
                "priority must be an integral i32, got Number(1.5)",
            ),
            (
                LuaValue::Integer(2_147_483_648),
                "priority must be an integral i32, got Integer(2147483648)",
            ),
        ] {
            let handler = with_field(&lua, "priority", value);
            assert_invalid(decode_hook_table(LuaValue::Table(handler), 250), expected);
        }

        let array = lua.create_table().expect("array");
        let handler = with_field(&lua, "priority", LuaValue::Number(f64::NAN));
        array.set(1i64, handler).expect("set element");
        assert_invalid(
            decode_hook_table(LuaValue::Table(array), 250),
            "priority must be an integral i32, got Number(NaN)",
        );
    }

    #[test]
    fn budget_refuses_non_integral_values() {
        let lua = Lua::new();
        for (value, expected) in [
            (
                LuaValue::String(lua.create_string("fast").expect("string")),
                r#"budgetMs must be a non-negative u64, got String("fast")"#,
            ),
            (
                LuaValue::Number(f64::NAN),
                "budgetMs must be a non-negative u64, got Number(NaN)",
            ),
            (
                LuaValue::Integer(-1),
                "budgetMs must be a non-negative u64, got Integer(-1)",
            ),
            (
                LuaValue::Number(-2.5),
                "budgetMs must be a non-negative u64, got Number(-2.5)",
            ),
            (
                LuaValue::Number(2.5),
                "budgetMs must be a non-negative u64, got Number(2.5)",
            ),
            (
                LuaValue::Number(1e30),
                "budgetMs must be a non-negative u64, got Number(1000000000000000000000000000000)",
            ),
        ] {
            let handler = with_field(&lua, "budgetMs", value);
            assert_invalid(decode_hook_table(LuaValue::Table(handler), 250), expected);
        }
    }

    #[test]
    fn budget_ms_zero_is_refused() {
        let lua = Lua::new();
        let handler = with_field(&lua, "budgetMs", LuaValue::Integer(0));
        assert_invalid(
            decode_hook_table(LuaValue::Table(handler), 250),
            "budgetMs must be positive",
        );

        let array = lua.create_table().expect("array");
        array
            .set(1i64, with_field(&lua, "budgetMs", LuaValue::Integer(0)))
            .expect("set element");
        assert_invalid(
            decode_hook_table(LuaValue::Table(array), 250),
            "budgetMs must be positive",
        );
    }

    #[test]
    fn an_invalid_on_error_is_refused() {
        let lua = Lua::new();
        for (value, expected) in [
            (
                LuaValue::String(lua.create_string("maybe").expect("string")),
                r#"onError must be deliver or deny, got String("maybe")"#,
            ),
            (
                LuaValue::Integer(1),
                "onError must be deliver or deny, got Integer(1)",
            ),
        ] {
            let handler = with_field(&lua, "onError", value);
            assert_invalid(decode_hook_table(LuaValue::Table(handler), 250), expected);
        }
    }

    #[test]
    fn an_unknown_handler_key_is_refused() {
        let lua = Lua::new();
        let handler = with_field(&lua, "bogus", LuaValue::Integer(1));
        assert_invalid(
            decode_hook_table(LuaValue::Table(handler.clone()), 250),
            r#"unknown handler key: String("bogus")"#,
        );

        let array = lua.create_table().expect("array");
        array.set(1i64, handler).expect("set element");
        assert_invalid(
            decode_hook_table(LuaValue::Table(array), 250),
            r#"unknown handler key: String("bogus")"#,
        );

        let numeric = handler_table(&lua);
        numeric.set(1i64, true).expect("set numeric key");
        assert_invalid(
            decode_hook_table(LuaValue::Table(numeric), 250),
            "unknown handler key: Integer(1)",
        );
    }

    #[test]
    fn a_non_table_array_element_is_refused() {
        let lua = Lua::new();
        for (value, expected) in [
            (
                LuaValue::Integer(7),
                "handler must be a table, got Integer(7)",
            ),
            (
                LuaValue::String(lua.create_string("x").expect("string")),
                r#"handler must be a table, got String("x")"#,
            ),
        ] {
            let array = lua.create_table().expect("array");
            array.set(1i64, value).expect("set element");
            assert_invalid(decode_hook_table(LuaValue::Table(array), 250), expected);
        }
    }

    #[test]
    fn a_missing_run_outside_an_array_is_refused() {
        let lua = Lua::new();
        let only_priority = lua.create_table().expect("handler table");
        only_priority.set("priority", 5i64).expect("set priority");
        assert_invalid(
            decode_hook_table(LuaValue::Table(only_priority), 250),
            "run must be a function",
        );

        let only_run = lua.create_table().expect("handler table");
        only_run.set("run", LuaValue::Integer(1)).expect("set run");
        assert_invalid(
            decode_hook_table(LuaValue::Table(only_run), 250),
            "run must be a function",
        );
    }

    #[test]
    fn an_unexpected_key_in_an_array_is_refused() {
        let lua = Lua::new();
        let array = lua.create_table().expect("array");
        array
            .set(1i64, handler_table(&lua))
            .expect("set first element");
        array.set("extra", true).expect("set extra key");
        assert_invalid(
            decode_hook_table(LuaValue::Table(array), 250),
            r#"hook table array has an unexpected key: String("extra")"#,
        );
    }

    #[test]
    fn too_many_handlers_is_refused_before_decoding_entries() {
        let lua = Lua::new();
        let array = lua.create_table().expect("array");
        let limit = i64::try_from(MAX_HANDLERS).expect("limit fits");
        for index in 1..=limit + 1 {
            let value = if index == 1 {
                LuaValue::Integer(7)
            } else {
                LuaValue::Table(handler_table(&lua))
            };
            array.set(index, value).expect("set element");
        }

        match decode_hook_table(LuaValue::Table(array), 250) {
            Err(error) => assert_eq!(
                error,
                EngineError::Limit(LimitCause::TooManyHandlers {
                    limit: MAX_HANDLERS
                })
            ),
            Ok(_) => panic!("an oversized array must be refused"),
        }
    }

    #[test]
    fn a_non_function_non_table_value_is_refused() {
        let lua = Lua::new();
        for (value, expected) in [
            (LuaValue::Nil, "hook must be a function or table, got Nil"),
            (
                LuaValue::Boolean(true),
                "hook must be a function or table, got Boolean(true)",
            ),
            (
                LuaValue::Integer(1),
                "hook must be a function or table, got Integer(1)",
            ),
            (
                LuaValue::String(lua.create_string("hook").expect("string")),
                r#"hook must be a function or table, got String("hook")"#,
            ),
        ] {
            assert_invalid(decode_hook_table(value, 250), expected);
        }
    }
}
