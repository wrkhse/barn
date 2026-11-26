use std::collections::HashSet;
use std::ffi::c_void;

use mlua::{Lua, Table, Value as LuaValue};
use serde_json::{Map, Value};

use super::errors::{EngineError, LimitCause, MAX_DEPTH, MAX_ENTRIES};

/// Error text for a table reached twice in one conversion.
const SHARED_TABLE: &str = "shared or cyclic Lua table in result";

/// The most entries one conversion may produce: a tree at the depth and entry
/// caps fits exactly, while a shared DAG does not.
const MAX_NODES: usize = MAX_DEPTH.saturating_mul(MAX_ENTRIES);

pub(super) fn read_tool(table: &Table, key: &str) -> Result<(String, Value), EngineError> {
    let name = table
        .raw_get::<Option<String>>("name")?
        .unwrap_or_else(|| key.to_owned());
    let description = table
        .raw_get::<Option<String>>("description")?
        .unwrap_or_default();
    let mut fields = Map::new();

    for (index, field) in table.pairs::<LuaValue, LuaValue>().enumerate() {
        if index >= MAX_ENTRIES {
            return Err(EngineError::Limit(LimitCause::TooManyEntries {
                limit: MAX_ENTRIES,
            }));
        }

        let (key, value) = field?;
        if matches!(value, LuaValue::Function(_)) {
            continue;
        }

        let LuaValue::String(key) = key else {
            return Err(EngineError::InvalidResult(
                "tool field key must be a string".into(),
            ));
        };
        let key = key.to_str()?.to_string();
        if matches!(key.as_str(), "name" | "description") {
            continue;
        }

        fields.insert(key, lua_to_json(value, 0)?);
    }

    fields.insert("name".into(), Value::String(name.clone()));
    fields.insert("description".into(), Value::String(description));
    fields
        .entry("input")
        .or_insert_with(|| Value::Object(Map::new()));

    Ok((name, Value::Object(fields)))
}

pub(super) fn lua_key(value: LuaValue) -> Result<String, EngineError> {
    match value {
        LuaValue::String(value) => Ok(value.to_str()?.to_string()),
        LuaValue::Integer(value) => Ok(value.to_string()),
        LuaValue::Number(value) => Ok(value.to_string()),
        _ => Err(EngineError::InvalidResult(
            "tool key must be a string".into(),
        )),
    }
}

/// Translate a table position into the one-based integer keys Lua expects.
fn lua_index(index: usize) -> Result<i64, EngineError> {
    i64::try_from(index).map_err(|_| EngineError::InvalidResult("table index out of range".into()))
}

/// Accept only positive integers inside the entry ceiling as array keys.
fn array_position(index: impl TryInto<usize>) -> Result<usize, EngineError> {
    let position = index
        .try_into()
        .map_err(|_| EngineError::InvalidResult("unsupported table key".into()))?;
    if (1..=MAX_ENTRIES).contains(&position) {
        Ok(position)
    } else {
        Err(EngineError::InvalidResult("unsupported table key".into()))
    }
}

pub(super) fn json_to_lua(lua: &Lua, value: &Value, depth: usize) -> Result<LuaValue, EngineError> {
    if depth > MAX_DEPTH {
        return Err(EngineError::Limit(LimitCause::TooDeep { limit: MAX_DEPTH }));
    }
    match value {
        Value::Null => Ok(LuaValue::Nil),
        Value::Bool(value) => Ok(LuaValue::Boolean(*value)),
        Value::Number(value) => value.as_i64().map_or_else(
            || {
                value
                    .as_f64()
                    .map(LuaValue::Number)
                    .ok_or_else(|| EngineError::InvalidResult("invalid JSON number".into()))
            },
            |number| Ok(LuaValue::Integer(number)),
        ),
        Value::String(value) => Ok(LuaValue::String(lua.create_string(value.as_bytes())?)),
        Value::Array(values) => {
            if values.len() > MAX_ENTRIES {
                return Err(EngineError::Limit(LimitCause::TooManyEntries {
                    limit: MAX_ENTRIES,
                }));
            }
            let table = lua.create_table()?;
            for (index, item) in values.iter().enumerate() {
                let key = lua_index(index.saturating_add(1))?;
                let converted = json_to_lua(lua, item, depth.saturating_add(1))?;
                table.set(key, converted)?;
            }
            Ok(LuaValue::Table(table))
        }
        Value::Object(values) => {
            if values.len() > MAX_ENTRIES {
                return Err(EngineError::Limit(LimitCause::TooManyEntries {
                    limit: MAX_ENTRIES,
                }));
            }
            let table = lua.create_table()?;
            for (key, item) in values {
                let converted = json_to_lua(lua, item, depth.saturating_add(1))?;
                table.set(key.as_str(), converted)?;
            }
            Ok(LuaValue::Table(table))
        }
    }
}

/// Table identities and node count for one Lua-to-JSON conversion.
struct Conversion {
    visited: HashSet<*const c_void>,
    nodes: usize,
}

impl Conversion {
    fn new() -> Self {
        Self {
            visited: HashSet::new(),
            nodes: 0,
        }
    }

    /// Charge one table entry against the conversion-wide node budget.
    const fn spend(&mut self) -> Result<(), EngineError> {
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > MAX_NODES {
            return Err(EngineError::Limit(LimitCause::TooManyEntries {
                limit: MAX_NODES,
            }));
        }
        Ok(())
    }

    fn value(&mut self, value: LuaValue, depth: usize) -> Result<Value, EngineError> {
        if depth > MAX_DEPTH {
            return Err(EngineError::Limit(LimitCause::TooDeep { limit: MAX_DEPTH }));
        }
        match value {
            LuaValue::Nil => Ok(Value::Null),
            LuaValue::LightUserData(value) if value.0.is_null() => Ok(Value::Null),
            LuaValue::Boolean(value) => Ok(Value::Bool(value)),
            LuaValue::Integer(value) => Ok(Value::Number(value.into())),
            LuaValue::Number(value) => serde_json::Number::from_f64(value)
                .map(Value::Number)
                .ok_or_else(|| EngineError::InvalidResult("non-finite Lua number".into())),
            LuaValue::String(value) => Ok(Value::String(value.to_str()?.to_string())),
            LuaValue::Table(table) => self.table(&table, depth),
            _ => Err(EngineError::InvalidResult("unsupported Lua value".into())),
        }
    }

    fn table(&mut self, table: &Table, depth: usize) -> Result<Value, EngineError> {
        if !self.visited.insert(table.to_pointer()) {
            return Err(EngineError::InvalidResult(SHARED_TABLE.into()));
        }

        let mut object = Map::new();
        let mut array = Vec::new();
        let mut array_mode: Option<bool> = None;

        for pair in table.pairs::<LuaValue, LuaValue>() {
            if object.len().saturating_add(array.len()) >= MAX_ENTRIES {
                return Err(EngineError::Limit(LimitCause::TooManyEntries {
                    limit: MAX_ENTRIES,
                }));
            }
            self.spend()?;
            let (key, value) = pair?;
            match key {
                LuaValue::String(key) => {
                    if array_mode == Some(true) {
                        return Err(EngineError::InvalidResult("mixed table keys".into()));
                    }
                    array_mode = Some(false);
                    let converted = self.value(value, depth.saturating_add(1))?;
                    object.insert(key.to_str()?.to_string(), converted);
                }
                LuaValue::Integer(index) => {
                    let position = array_position(index)?;
                    if array_mode == Some(false) {
                        return Err(EngineError::InvalidResult("mixed table keys".into()));
                    }
                    array_mode = Some(true);
                    let converted = self.value(value, depth.saturating_add(1))?;
                    array.push((position, converted));
                }
                _ => {
                    return Err(EngineError::InvalidResult("unsupported table key".into()));
                }
            }
        }

        Self::finish(object, array, array_mode)
    }

    fn finish(
        object: Map<String, Value>,
        mut array: Vec<(usize, Value)>,
        array_mode: Option<bool>,
    ) -> Result<Value, EngineError> {
        if array_mode != Some(true) {
            return Ok(Value::Object(object));
        }
        array.sort_by_key(|(index, _)| *index);
        for (position, (index, _)) in array.iter().enumerate() {
            if *index != position.saturating_add(1) {
                return Err(EngineError::InvalidResult("sparse Lua array".into()));
            }
        }
        Ok(Value::Array(
            array.into_iter().map(|(_, value)| value).collect(),
        ))
    }
}

pub(super) fn lua_to_json(value: LuaValue, depth: usize) -> Result<Value, EngineError> {
    let mut conversion = Conversion::new();
    conversion.value(value, depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsupported() -> EngineError {
        EngineError::InvalidResult("unsupported table key".into())
    }

    fn lua_table_to_json(table: &Table, depth: usize) -> Result<Value, EngineError> {
        Conversion::new().table(table, depth)
    }

    #[test]
    fn lua_index_converts_the_position_exactly() {
        assert_eq!(lua_index(0), Ok(0));
        assert_eq!(lua_index(1), Ok(1));
        assert_eq!(lua_index(2), Ok(2));

        #[cfg(target_pointer_width = "64")]
        assert_eq!(
            lua_index(usize::MAX),
            Err(EngineError::InvalidResult(
                "table index out of range".into()
            ))
        );
    }

    #[test]
    fn array_position_accepts_only_positive_in_range_keys() {
        assert_eq!(array_position(1i64), Ok(1));
        assert_eq!(array_position(2i64), Ok(2));
        assert_eq!(
            array_position(i64::try_from(MAX_ENTRIES).unwrap()),
            Ok(MAX_ENTRIES)
        );

        let over = i64::try_from(MAX_ENTRIES + 1).unwrap();
        for bad in [0i64, -1, over] {
            assert_eq!(array_position(bad), Err(unsupported()), "key {bad}");
        }
    }

    #[test]
    fn lua_table_to_json_converts_each_key_kind() {
        let lua = Lua::new();

        let strings = lua.create_table().unwrap();
        strings.set("answer", 42).unwrap();
        assert_eq!(
            lua_table_to_json(&strings, 0).unwrap(),
            serde_json::json!({ "answer": 42 })
        );

        let integers = lua.create_table().unwrap();
        integers.set(1i64, "a").unwrap();
        integers.set(2i64, "b").unwrap();
        assert_eq!(
            lua_table_to_json(&integers, 0).unwrap(),
            serde_json::json!(["a", "b"])
        );
    }

    #[test]
    fn lua_table_to_json_rejects_a_fractional_key() {
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        table.set(1.5f64, "a").unwrap();
        assert_eq!(lua_table_to_json(&table, 0), Err(unsupported()));
    }

    #[test]
    fn lua_table_to_json_treats_a_whole_float_key_as_an_integer() {
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        table.set(1i64, "a").unwrap();
        table.set(2.0f64, "b").unwrap();
        assert_eq!(
            lua_table_to_json(&table, 0).unwrap(),
            serde_json::json!(["a", "b"])
        );
    }

    #[test]
    fn lua_table_to_json_allows_exactly_the_entry_cap() {
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        for index in 0..MAX_ENTRIES {
            let value = i64::try_from(index).unwrap();
            table.set(format!("k{index}"), value).unwrap();
        }

        let value = lua_table_to_json(&table, 0).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), MAX_ENTRIES);
        assert_eq!(object.get("k0"), Some(&serde_json::json!(0)));

        table.set("extra", 1).unwrap();
        assert_eq!(
            lua_table_to_json(&table, 0),
            Err(EngineError::Limit(LimitCause::TooManyEntries {
                limit: MAX_ENTRIES
            }))
        );
    }

    #[test]
    fn lua_to_json_converts_a_float_to_the_exact_json_number() {
        assert_eq!(
            lua_to_json(LuaValue::Number(1.5), 0),
            Ok(serde_json::json!(1.5))
        );
        assert_eq!(
            lua_to_json(LuaValue::Number(f64::NAN), 0),
            Err(EngineError::InvalidResult("non-finite Lua number".into()))
        );
    }

    #[test]
    fn lua_to_json_maps_only_the_null_light_userdata_to_null() {
        assert_eq!(lua_to_json(LuaValue::NULL, 0), Ok(Value::Null));

        let pointer = std::ptr::NonNull::<std::ffi::c_void>::dangling().as_ptr();
        assert_eq!(
            lua_to_json(LuaValue::LightUserData(mlua::LightUserData(pointer)), 0),
            Err(EngineError::InvalidResult("unsupported Lua value".into()))
        );
    }

    #[test]
    fn lua_table_to_json_refuses_a_sparse_array() {
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        table.set(1i64, "a").unwrap();
        table.set(3i64, "c").unwrap();
        assert_eq!(
            lua_table_to_json(&table, 0),
            Err(EngineError::InvalidResult("sparse Lua array".into()))
        );
    }

    #[test]
    fn lua_key_accepts_an_integer_key_at_its_exact_string_form() {
        assert_eq!(lua_key(LuaValue::Integer(7)), Ok("7".to_owned()));
    }

    #[test]
    fn lua_key_accepts_a_float_key_at_its_exact_string_form() {
        assert_eq!(lua_key(LuaValue::Number(7.5)), Ok("7.5".to_owned()));
    }

    #[test]
    fn json_to_lua_accepts_depth_16_and_refuses_depth_17() {
        let lua = Lua::new();

        assert_eq!(json_to_lua(&lua, &Value::Null, 16), Ok(LuaValue::Nil));
        assert_eq!(
            json_to_lua(&lua, &Value::Null, 17),
            Err(EngineError::Limit(LimitCause::TooDeep { limit: 16 }))
        );
    }

    #[test]
    fn json_to_lua_accepts_1024_array_entries_and_refuses_1025() {
        let lua = Lua::new();
        let at_cap = Value::Array(vec![Value::Bool(true); 1024]);
        let past_cap = Value::Array(vec![Value::Bool(true); 1025]);

        assert!(json_to_lua(&lua, &at_cap, 0).is_ok());
        assert_eq!(
            json_to_lua(&lua, &past_cap, 0),
            Err(EngineError::Limit(LimitCause::TooManyEntries {
                limit: 1024
            }))
        );
    }

    #[test]
    fn json_to_lua_accepts_1024_object_entries_and_refuses_1025() {
        let lua = Lua::new();
        let build = |count: usize| {
            (0..count)
                .map(|index| (format!("k{index}"), Value::Bool(true)))
                .collect::<Map<String, Value>>()
        };
        let at_cap = Value::Object(build(1024));
        let past_cap = Value::Object(build(1025));

        assert!(json_to_lua(&lua, &at_cap, 0).is_ok());
        assert_eq!(
            json_to_lua(&lua, &past_cap, 0),
            Err(EngineError::Limit(LimitCause::TooManyEntries {
                limit: 1024
            }))
        );
    }

    #[test]
    fn lua_to_json_accepts_depth_16_and_refuses_depth_17() {
        assert_eq!(
            lua_to_json(LuaValue::Integer(1), 16),
            Ok(serde_json::json!(1))
        );
        assert_eq!(
            lua_to_json(LuaValue::Integer(1), 17),
            Err(EngineError::Limit(LimitCause::TooDeep { limit: 16 }))
        );
    }

    fn shared_table() -> EngineError {
        EngineError::InvalidResult(SHARED_TABLE.into())
    }

    #[test]
    fn lua_to_json_refuses_a_shared_table_dag() {
        let lua = Lua::new();
        let mut tables = Vec::with_capacity(17);
        for _ in 0..17 {
            tables.push(lua.create_table().unwrap());
        }

        // Each table's entries all point at the next one, so a walk that
        // revisited tables would expand into 1024^16 host nodes.
        for index in 0..16usize {
            let child = tables[index.saturating_add(1)].clone();
            for key in 0..MAX_ENTRIES {
                tables[index].set(format!("k{key}"), child.clone()).unwrap();
            }
        }
        for key in 0..MAX_ENTRIES {
            tables[16].set(format!("k{key}"), 1i64).unwrap();
        }

        assert_eq!(
            lua_to_json(LuaValue::Table(tables[0].clone()), 0),
            Err(shared_table())
        );
    }

    #[test]
    fn lua_to_json_refuses_a_cyclic_table() {
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        table.set("self", table.clone()).unwrap();

        assert_eq!(lua_to_json(LuaValue::Table(table), 0), Err(shared_table()));
    }

    #[test]
    fn lua_to_json_refuses_a_conversion_past_the_node_budget() {
        let lua = Lua::new();
        let root = lua.create_table().unwrap();
        for index in 0..MAX_ENTRIES {
            let child = lua.create_table().unwrap();
            for field in 0..16 {
                child.set(format!("f{field}"), 1i64).unwrap();
            }
            root.set(format!("k{index}"), child).unwrap();
        }

        assert_eq!(
            lua_to_json(LuaValue::Table(root), 0),
            Err(EngineError::Limit(LimitCause::TooManyEntries {
                limit: MAX_NODES
            }))
        );
    }
}
