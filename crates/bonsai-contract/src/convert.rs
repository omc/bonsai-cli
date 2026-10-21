//! OpenAPI 3.0 schema objects to JSON Schema (draft 2020-12).

use serde_json::{Map, Value, json};

const COMPONENT_REF: &str = "#/components/schemas/";

/// Keywords whose value is a map of name to schema.
const SCHEMA_MAPS: &[&str] = &["properties", "patternProperties"];
/// Keywords whose value is a schema (or, for `additionalProperties`, a bool).
const SCHEMAS: &[&str] = &["items", "additionalProperties", "not"];
/// Keywords whose value is an array of schemas.
const SCHEMA_LISTS: &[&str] = &["allOf", "anyOf", "oneOf"];

/// Convert one OpenAPI schema object. With `strict`, an `x-extensible-enum` becomes
/// a closed `enum`, so a value outside the documented set (a typo in the mock) fails.
pub fn convert_schema(schema: &Value, strict: bool) -> Value {
    let Value::Object(obj) = schema else {
        return schema.clone();
    };
    let mut out = Map::new();
    let mut nullable = false;
    for (key, value) in obj {
        if strict && key == "x-extensible-enum" {
            out.insert("enum".into(), value.clone());
            continue;
        }
        if key.starts_with("x-") || key == "example" {
            continue;
        }
        match key.as_str() {
            "nullable" => nullable = value.as_bool().unwrap_or(false),
            "$ref" => {
                let target = value.as_str().unwrap_or_default();
                let converted = match target.strip_prefix(COMPONENT_REF) {
                    Some(name) => format!("#/$defs/{name}"),
                    None => target.to_string(),
                };
                out.insert(key.clone(), Value::String(converted));
            }
            k if SCHEMA_MAPS.contains(&k) => {
                let map = value
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .map(|(n, s)| (n.clone(), convert_schema(s, strict)))
                            .collect()
                    })
                    .unwrap_or_default();
                out.insert(key.clone(), Value::Object(map));
            }
            k if SCHEMAS.contains(&k) => {
                out.insert(key.clone(), convert_schema(value, strict));
            }
            k if SCHEMA_LISTS.contains(&k) => {
                let list = value
                    .as_array()
                    .map(|a| a.iter().map(|s| convert_schema(s, strict)).collect())
                    .unwrap_or_default();
                out.insert(key.clone(), Value::Array(list));
            }
            _ => {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    if !nullable {
        return Value::Object(out);
    }
    if let Some(Value::String(t)) = out.get("type").cloned() {
        out.insert("type".into(), json!([t, "null"]));
        if let Some(Value::Array(values)) = out.get_mut("enum") {
            values.push(Value::Null);
        }
        Value::Object(out)
    } else {
        json!({ "anyOf": [Value::Object(out), { "type": "null" }] })
    }
}

/// Convert every entry of `components.schemas` into a `$defs` map.
pub fn convert_components(schemas: &Value, strict: bool) -> Value {
    let map = schemas
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(n, s)| (n.clone(), convert_schema(s, strict)))
                .collect()
        })
        .unwrap_or_default();
    Value::Object(map)
}
