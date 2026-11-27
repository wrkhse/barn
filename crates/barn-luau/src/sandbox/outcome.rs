use mlua::{Table, Value as LuaValue};

use serde_json::Value;

use crate::api::Outcome;

use super::errors::EngineError;
use super::values::lua_to_json;

const VERDICTS: [&str; 4] = ["deny", "rewrite", "annotate", "drop"];

pub(super) fn decode_outcome(value: LuaValue) -> Result<Outcome, EngineError> {
    if matches!(value, LuaValue::Nil) {
        return Ok(Outcome::Allow);
    }

    let LuaValue::Table(table) = value else {
        return Err(EngineError::InvalidResult(
            "hook must return a table or nil".into(),
        ));
    };

    let mut found = Vec::new();
    for key in VERDICTS {
        if !matches!(table.get::<LuaValue>(key)?, LuaValue::Nil) {
            found.push(key);
        }
    }

    match found.as_slice() {
        [] => Err(EngineError::InvalidResult(
            "hook result has no verdict: expected deny, rewrite, annotate or drop".into(),
        )),
        [_, _, ..] => Err(EngineError::InvalidResult(format!(
            "hook result has multiple verdicts: {}",
            found.join(", ")
        ))),
        [key] => decode_verdict(&table, key),
    }
}

fn decode_verdict(table: &Table, key: &str) -> Result<Outcome, EngineError> {
    match key {
        "deny" => {
            let reason = table
                .get::<Option<String>>(key)?
                .ok_or_else(|| EngineError::InvalidResult("deny must be a string".into()))?;
            Ok(Outcome::Deny { reason })
        }
        "rewrite" => Ok(Outcome::Rewrite {
            payload: lua_to_json(table.get::<LuaValue>(key)?, 0)?,
        }),
        "annotate" => {
            let Value::Object(fields) = lua_to_json(table.get::<LuaValue>(key)?, 0)? else {
                return Err(EngineError::InvalidResult(
                    "annotate must be an object".into(),
                ));
            };
            Ok(Outcome::Annotate { fields })
        }
        "drop" => match table.get::<LuaValue>(key)? {
            LuaValue::Boolean(true) => Ok(Outcome::Drop),
            _ => Err(EngineError::InvalidResult("drop must be true".into())),
        },
        _ => Err(EngineError::InvalidResult(format!(
            "unknown verdict: {key}"
        ))),
    }
}
