//! Lua values to JSON and back, for the store, tool arguments and results.
//!
//! A table whose keys are exactly `1..n` becomes an array, an empty table an
//! empty array, and any other table an object with string keys. JSON
//! `null` becomes `nil`, so it drops out of tables.

use oximond::runtime::TableHandle;
use oximond::{Runtime, RuntimeError, RuntimeResult, Value};
use serde_json::{Map, Number, Value as Json};

/// How deep tables may nest. Deeper ones, and tables that contain
/// themselves, are an error.
const MAX_DEPTH: usize = 64;

/// `value` as JSON. Functions, userdata and threads are an error.
pub(crate) fn to_json(runtime: &Runtime, value: &Value) -> RuntimeResult<Json> {
    convert(runtime, value, 0)
}

fn convert(runtime: &Runtime, value: &Value, depth: usize) -> RuntimeResult<Json> {
    Ok(match value {
        Value::Nil => Json::Null,
        Value::Boolean(on) => Json::Bool(*on),
        Value::Integer(n) => Json::from(*n),
        Value::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 9.0e15 {
                // Lua 5.4 floats such as `2.0` read as integers in JSON.
                #[allow(clippy::cast_possible_truncation)]
                Json::from(*n as i64)
            } else {
                Number::from_f64(*n).map_or(Json::Null, Json::Number)
            }
        }
        Value::String(text) => Json::String(String::from_utf8_lossy(text.as_bytes()).into_owned()),
        Value::Table(table) => {
            if depth >= MAX_DEPTH {
                return Err(RuntimeError::message(
                    "the value nests too deep or contains itself",
                ));
            }
            table_json(runtime, *table, depth + 1)?
        }
        other => {
            return Err(RuntimeError::message(format!(
                "a {} cannot be stored or sent as JSON",
                type_name(other)
            )));
        }
    })
}

fn table_json(runtime: &Runtime, table: TableHandle, depth: usize) -> RuntimeResult<Json> {
    let mut entries: Vec<(Value, Value)> = Vec::new();
    let mut key = Value::Nil;
    loop {
        let (next, value) = runtime.table_next(table, &key)?;
        if matches!(next, Value::Nil) {
            break;
        }
        key = next.clone();
        entries.push((next, value));
    }
    let index = |key: &Value| match key {
        Value::Integer(n) => usize::try_from(*n).ok(),
        Value::Number(n) if n.fract() == 0.0 && *n >= 1.0 =>
        {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some(*n as usize)
        }
        _ => None,
    };
    let len = entries.len();
    let is_array = entries
        .iter()
        .all(|(key, _)| index(key).is_some_and(|i| (1..=len).contains(&i)));
    if is_array {
        let mut items = vec![Json::Null; len];
        for (key, value) in &entries {
            if let Some(i) = index(key) {
                items[i - 1] = convert(runtime, value, depth)?;
            }
        }
        return Ok(Json::Array(items));
    }
    let mut object = Map::new();
    for (key, value) in &entries {
        let name = match key {
            Value::String(text) => String::from_utf8_lossy(text.as_bytes()).into_owned(),
            Value::Integer(n) => n.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Boolean(on) => on.to_string(),
            other => {
                return Err(RuntimeError::message(format!(
                    "a table key of type {} cannot be a JSON key",
                    type_name(other)
                )));
            }
        };
        object.insert(name, convert(runtime, value, depth)?);
    }
    Ok(Json::Object(object))
}

/// `json` as a Lua value.
pub(crate) fn from_json(runtime: &mut Runtime, json: &Json) -> RuntimeResult<Value> {
    Ok(match json {
        Json::Null => Value::Nil,
        Json::Bool(on) => Value::Boolean(*on),
        Json::Number(n) => match n.as_i64() {
            Some(n) => Value::Integer(n),
            None => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(text) => Value::from(text.as_str()),
        Json::Array(items) => {
            let table = runtime.create_table()?;
            for (index, item) in (1_i64..).zip(items) {
                let value = from_json(runtime, item)?;
                runtime.table_set(table, &Value::Integer(index), value)?;
            }
            Value::Table(table)
        }
        Json::Object(object) => {
            let table = runtime.create_table()?;
            for (key, item) in object {
                let value = from_json(runtime, item)?;
                runtime.table_set(table, &Value::from(key.as_str()), value)?;
            }
            Value::Table(table)
        }
    })
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Nil => "nil",
        Value::Boolean(_) => "boolean",
        Value::Integer(_) | Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Table(_) => "table",
        Value::Userdata(_) => "userdata",
        _ => "function or thread",
    }
}
