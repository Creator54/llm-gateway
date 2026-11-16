//! Tool parameter schemas for Cloud Code Assist.
//!
//! The endpoint accepts only a subset of JSON Schema and answers 400 to any other keyword
//! (`$schema`, `exclusiveMinimum`, `additionalProperties`, ...). Rather than blocklist what
//! clients send, rebuild each schema from the keywords the endpoint documents.

use serde_json::{json, Map, Value};

const MAX_DEPTH: usize = 24;
const TYPES: [&str; 6] = ["string", "integer", "number", "boolean", "array", "object"];

/// A schema the endpoint accepts, describing (at least) every value `schema` allows.
pub fn sanitize(schema: &Value) -> Value {
    let mut out = clean(schema, schema, 0);
    // The root must be an object schema.
    if out.get("type") != Some(&json!("object")) {
        out = json!({ "type": "object", "properties": {} });
    }
    out
}

fn clean(root: &Value, schema: &Value, depth: usize) -> Value {
    let Some(map) = schema.as_object() else {
        return json!({ "type": "string" });
    };
    if depth > MAX_DEPTH {
        return json!({ "type": "string" });
    }
    if let Some(target) = map
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| lookup(root, r))
    {
        return clean(root, target, depth + 1);
    }
    let mut out = Map::new();
    let (kind, nullable) = kind(map);
    if let Some(kind) = kind {
        out.insert("type".into(), json!(kind));
    }
    if nullable {
        out.insert("nullable".into(), json!(true));
    }
    for key in ["description", "format"] {
        if let Some(text) = map.get(key).and_then(Value::as_str) {
            out.insert(key.into(), json!(text));
        }
    }
    if let Some(values) = map.get("enum").and_then(Value::as_array) {
        out.insert("enum".into(), json!(strings(values)));
    } else if let Some(value) = map.get("const") {
        out.insert("enum".into(), json!(strings(std::slice::from_ref(value))));
    }
    if let Some(props) = map.get("properties").and_then(Value::as_object) {
        let props: Map<String, Value> = props
            .iter()
            .map(|(k, v)| (k.clone(), clean(root, v, depth + 1)))
            .collect();
        let required: Vec<&Value> = map
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|name| name.as_str().is_some_and(|n| props.contains_key(n)))
            .collect();
        out.insert("properties".into(), Value::Object(props));
        if !required.is_empty() {
            out.insert("required".into(), json!(required));
        }
        out.entry("type").or_insert(json!("object"));
    }
    if let Some(items) = map.get("items") {
        let items = if items.is_array() { &json!({}) } else { items };
        out.insert("items".into(), clean(root, items, depth + 1));
        out.entry("type").or_insert(json!("array"));
    }
    if let Some(variants) = map
        .get("anyOf")
        .or_else(|| map.get("oneOf"))
        .and_then(Value::as_array)
    {
        let variants: Vec<Value> = variants
            .iter()
            .filter(|v| v.get("type") != Some(&json!("null")))
            .map(|v| clean(root, v, depth + 1))
            .collect();
        if variants.len() == 1 {
            // `X | null` is `X`, nullable.
            if let Some(only) = variants[0].as_object() {
                for (k, v) in only {
                    out.entry(k.clone()).or_insert(v.clone());
                }
            }
            if map
                .get("anyOf")
                .or_else(|| map.get("oneOf"))
                .is_some_and(|v| v.as_array().is_some_and(|a| a.len() > 1))
            {
                out.insert("nullable".into(), json!(true));
            }
        } else if variants.len() > 1 {
            out.insert("anyOf".into(), json!(variants));
        }
    }
    if !out.contains_key("type") && !out.contains_key("anyOf") {
        out.insert("type".into(), json!("string"));
    }
    Value::Object(out)
}

/// The single supported `type`, and whether `null` was among the allowed types.
fn kind(map: &Map<String, Value>) -> (Option<&'static str>, bool) {
    let names: Vec<&str> = match map.get("type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    let nullable = names.contains(&"null") || map.get("nullable") == Some(&json!(true));
    let kind = names
        .into_iter()
        .find_map(|n| TYPES.iter().find(|t| **t == n).copied());
    (kind, nullable)
}

fn strings(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
        .collect()
}

/// Resolve a local `#/...` reference against the root schema.
fn lookup<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    root.pointer(reference.strip_prefix('#')?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_unsupported_keywords_and_keeps_the_shape() {
        let out = sanitize(&json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "required": ["n", "ghost"],
            "properties": {
                "n": {"type": "integer", "exclusiveMinimum": 0, "description": "count"},
                "tags": {"type": "array", "items": {"type": "string", "pattern": "x"}},
                "mode": {"const": "fast"},
                "maybe": {"type": ["string", "null"]},
                "pick": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                "ref": {"$ref": "#/$defs/d"}
            },
            "$defs": {"d": {"type": "boolean", "title": "t"}}
        }));
        assert_eq!(
            out,
            json!({
                "type": "object",
                "required": ["n"],
                "properties": {
                    "n": {"type": "integer", "description": "count"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "mode": {"type": "string", "enum": ["fast"]},
                    "maybe": {"type": "string", "nullable": true},
                    "pick": {"type": "string", "nullable": true},
                    "ref": {"type": "boolean"}
                }
            })
        );
    }

    #[test]
    fn non_object_roots_and_cycles_are_safe() {
        assert_eq!(
            sanitize(&json!("x")),
            json!({"type": "object", "properties": {}})
        );
        let cyclic = json!({"type": "object", "properties": {"a": {"$ref": "#"}}});
        assert!(sanitize(&cyclic).is_object());
    }
}
