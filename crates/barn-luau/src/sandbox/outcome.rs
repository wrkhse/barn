use mlua::{Table, Value as LuaValue};

use serde_json::Value;

use crate::api::Outcome;

use super::errors::EngineError;
use super::values::lua_to_json;

/// A handler's verdict, decoded from the table its Lua function returns.
///
/// Continue is a nil return: the handler passes and the table moves on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookResult {
    Allow,
    Deny {
        reason: String,
    },
    Rewrite {
        payload: serde_json::Value,
    },
    Annotate {
        fields: serde_json::Map<String, serde_json::Value>,
    },
    Drop,
    Continue,
}

const VERDICTS: [&str; 5] = ["allow", "deny", "rewrite", "annotate", "drop"];

/// Decode a hook return value into the call's outcome.
///
/// The Lua parse lives in `decode_hook_result`; this converts its verdict
/// into an `Outcome`, so there is only one verdict decoder.
pub(super) fn decode_outcome(value: LuaValue) -> Result<Outcome, EngineError> {
    decode_hook_result(value).map(Outcome::from)
}

impl From<HookResult> for Outcome {
    fn from(result: HookResult) -> Self {
        match result {
            HookResult::Allow | HookResult::Continue => Self::Allow,
            HookResult::Deny { reason } => Self::Deny { reason },
            HookResult::Rewrite { payload } => Self::Rewrite { payload },
            HookResult::Annotate { fields } => Self::Annotate { fields },
            HookResult::Drop => Self::Drop,
        }
    }
}

/// Find the one verdict key in a result table, or name why there is not one.
///
/// Both decoders share this scan so they agree on the zero- and many-verdict
/// errors.
fn single_verdict(table: &Table) -> Result<&'static str, EngineError> {
    let mut found = Vec::new();
    for key in VERDICTS {
        if !matches!(table.get::<LuaValue>(key)?, LuaValue::Nil) {
            found.push(key);
        }
    }

    match found.as_slice() {
        [] => Err(EngineError::InvalidResult(
            "hook result has no verdict: expected allow, deny, rewrite, annotate or drop".into(),
        )),
        [_, _, ..] => Err(EngineError::InvalidResult(format!(
            "hook result has multiple verdicts: {}",
            found.join(", ")
        ))),
        [key] => Ok(*key),
    }
}

/// A boolean verdict must be exactly true, not another Lua value.
fn require_true(table: &Table, key: &str) -> Result<(), EngineError> {
    match table.get::<LuaValue>(key)? {
        LuaValue::Boolean(true) => Ok(()),
        _ => Err(EngineError::InvalidResult(format!("{key} must be true"))),
    }
}

/// Decode a hook handler's verdict table into a `HookResult`.
///
/// A nil return means Continue; a table carries exactly one verdict.
///
/// # Errors
///
/// Returns [`EngineError::InvalidResult`] when the return is not a table with
/// exactly one well-formed verdict.
pub(super) fn decode_hook_result(value: LuaValue) -> Result<HookResult, EngineError> {
    if matches!(value, LuaValue::Nil) {
        return Ok(HookResult::Continue);
    }

    let LuaValue::Table(table) = value else {
        return Err(EngineError::InvalidResult(
            "hook must return a table or nil".into(),
        ));
    };

    match single_verdict(&table)? {
        "allow" => {
            require_true(&table, "allow")?;
            Ok(HookResult::Allow)
        }
        "deny" => {
            let reason = table
                .get::<Option<String>>("deny")?
                .ok_or_else(|| EngineError::InvalidResult("deny must be a string".into()))?;
            Ok(HookResult::Deny { reason })
        }
        "rewrite" => Ok(HookResult::Rewrite {
            payload: lua_to_json(table.get::<LuaValue>("rewrite")?, 0)?,
        }),
        "annotate" => {
            let Value::Object(fields) = lua_to_json(table.get::<LuaValue>("annotate")?, 0)? else {
                return Err(EngineError::InvalidResult(
                    "annotate must be an object".into(),
                ));
            };
            Ok(HookResult::Annotate { fields })
        }
        "drop" => {
            require_true(&table, "drop")?;
            Ok(HookResult::Drop)
        }
        key => Err(EngineError::InvalidResult(format!(
            "unknown verdict: {key}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use mlua::Lua;
    use serde_json::{Map, json};

    use super::*;

    /// Assert both decoders agree with each other and with the exact expected
    /// values for one return value.
    fn assert_decoders(
        value: LuaValue,
        expected_result: &Result<HookResult, EngineError>,
        expected_outcome: &Result<Outcome, EngineError>,
    ) {
        assert_eq!(decode_hook_result(value.clone()), *expected_result);
        assert_eq!(decode_outcome(value.clone()), *expected_outcome);
        assert_eq!(
            decode_outcome(value.clone()),
            decode_hook_result(value).map(Outcome::from)
        );
    }

    #[test]
    fn nil_decodes_to_continue() {
        assert_decoders(
            LuaValue::Nil,
            &Ok(HookResult::Continue),
            &Ok(Outcome::Allow),
        );
    }

    #[test]
    fn an_allow_verdict_decodes_through_both_decoders() {
        let lua = Lua::new();
        let allow = lua.create_table().expect("allow table");
        allow.raw_set("allow", true).expect("set allow");

        assert_decoders(
            LuaValue::Table(allow),
            &Ok(HookResult::Allow),
            &Ok(Outcome::Allow),
        );
    }

    #[test]
    fn a_non_true_allow_is_refused() {
        let lua = Lua::new();
        for value in [LuaValue::Boolean(false), LuaValue::Integer(1)] {
            let result = lua.create_table().expect("result table");
            result.raw_set("allow", value).expect("set allow");

            let expected = EngineError::InvalidResult("allow must be true".into());
            assert_decoders(
                LuaValue::Table(result),
                &Err(expected.clone()),
                &Err(expected),
            );
        }
    }

    #[test]
    fn a_non_table_return_is_refused() {
        let expected = EngineError::InvalidResult("hook must return a table or nil".into());
        assert_decoders(
            LuaValue::Boolean(true),
            &Err(expected.clone()),
            &Err(expected),
        );
    }

    #[test]
    fn each_verdict_decodes_through_both_decoders() {
        let lua = Lua::new();

        let deny = lua.create_table().expect("deny table");
        deny.raw_set("deny", "blocked").expect("set deny");

        let payload = lua.create_table().expect("payload");
        payload.raw_set("command", "echo").expect("set command");
        let rewrite = lua.create_table().expect("rewrite table");
        rewrite.raw_set("rewrite", payload).expect("set rewrite");

        let fields = lua.create_table().expect("fields");
        fields.raw_set("risk", "high").expect("set risk");
        let annotate = lua.create_table().expect("annotate table");
        annotate.raw_set("annotate", fields).expect("set annotate");

        let drop = lua.create_table().expect("drop table");
        drop.raw_set("drop", true).expect("set drop");

        let mut expected_fields = Map::new();
        expected_fields.insert("risk".to_owned(), json!("high"));

        assert_decoders(
            LuaValue::Table(deny),
            &Ok(HookResult::Deny {
                reason: "blocked".to_owned(),
            }),
            &Ok(Outcome::Deny {
                reason: "blocked".to_owned(),
            }),
        );
        assert_decoders(
            LuaValue::Table(rewrite),
            &Ok(HookResult::Rewrite {
                payload: json!({ "command": "echo" }),
            }),
            &Ok(Outcome::Rewrite {
                payload: json!({ "command": "echo" }),
            }),
        );
        assert_decoders(
            LuaValue::Table(annotate),
            &Ok(HookResult::Annotate {
                fields: expected_fields.clone(),
            }),
            &Ok(Outcome::Annotate {
                fields: expected_fields,
            }),
        );
        assert_decoders(
            LuaValue::Table(drop),
            &Ok(HookResult::Drop),
            &Ok(Outcome::Drop),
        );
    }

    #[test]
    fn an_empty_result_is_refused() {
        let lua = Lua::new();
        let empty = lua.create_table().expect("empty table");
        let expected = EngineError::InvalidResult(
            "hook result has no verdict: expected allow, deny, rewrite, annotate or drop".into(),
        );
        assert_decoders(
            LuaValue::Table(empty),
            &Err(expected.clone()),
            &Err(expected),
        );
    }

    #[test]
    fn multiple_verdicts_are_refused() {
        let lua = Lua::new();
        let result = lua.create_table().expect("result table");
        result.raw_set("deny", "no").expect("set deny");
        result.raw_set("drop", true).expect("set drop");

        let expected =
            EngineError::InvalidResult("hook result has multiple verdicts: deny, drop".into());
        assert_decoders(
            LuaValue::Table(result),
            &Err(expected.clone()),
            &Err(expected),
        );
    }
}
