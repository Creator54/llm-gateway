//! Chat tools to Kiro `toolSpecification` list.

use super::wire::ToolNames;
use serde_json::{json, Map, Value};

const MAX_DESCRIPTION: usize = 1024;
const MAX_DESCRIPTION_GPT_56_SOL: usize = 9_216;
const MAX_TOOL_COUNT: usize = 48;
const MAX_TOOL_CATALOG_BYTES: usize = 96_000;

const REJECTED_SCHEMA_KEYS: &[&str] = &[
    "additionalProperties",
    "pattern",
    "format",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
    "contentEncoding",
    "contentMediaType",
    "$schema",
    "patternProperties",
    "propertyNames",
    "dependentSchemas",
    "dependentRequired",
    "if",
    "then",
    "else",
    "contains",
    "unevaluatedProperties",
    "unevaluatedItems",
    "encrypted",
];

/// Keys whose children are named schemas, not schema keywords.
const SCHEMA_MAP_KEYS: &[&str] = &["properties", "$defs", "definitions"];

const NEIGHBOR_TOOL_NAMES: &[&str] = &["Read", "Grep", "Glob", "Bash", "LS"];

/// The private completion channel; the model calls it instead of writing the answer as text.
pub(super) fn completion_tool() -> Value {
    json!({ "toolSpecification": {
        "name": super::wire::COMPLETION_TOOL,
        "description": "Terminal completion channel, not an ordinary work tool. When the task is fully complete and no more work or tool calls are needed, you must call this tool exactly once instead of providing the final answer as ordinary assistant text. Call it the same way when you cannot continue until the user supplies a decision, information, or a clarification that only they can give: the question itself is the answer. Put the complete user-facing final answer in `answer`. The call is complete when issued: it ends the turn, returns no tool result, and no text or tool call may follow it.",
        "inputSchema": { "json": {
            "type": "object",
            "properties": { "answer": {
                "type": "string",
                "description": "The complete final answer to show the user, or the blocking question you need the user to answer before you can continue.",
            }},
            "required": ["answer"],
        }},
    }})
}

/// Converted tool catalog, its injected nudge, and the names its aliases hide from the client.
pub(super) fn convert_tools(chat: &Value, names: &mut ToolNames) -> (Vec<Value>, Option<String>) {
    let requested: Vec<Value> = chat
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let limit =
        if super::wire::normalize_model_id(chat.get("model").and_then(Value::as_str).unwrap_or(""))
            == "gpt-5.6-sol"
        {
            MAX_DESCRIPTION_GPT_56_SOL
        } else {
            MAX_DESCRIPTION
        };
    // Aliases are assigned for every listed tool, even ones the budget drops.
    let mut converted: Vec<(usize, Value, String)> = Vec::new();
    for (index, tool) in requested.iter().enumerate() {
        let function = tool.get("function").unwrap_or(tool);
        let Some(wire_name) = function.get("name").and_then(Value::as_str) else {
            continue;
        };
        let alias = names.alias(wire_name);
        let description = function
            .get("description")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Tool: {wire_name}"));
        let description = truncate_description(&description, limit);
        let parameters = function.get("parameters").cloned().unwrap_or(Value::Null);
        converted.push((
            index,
            json!({ "toolSpecification": {
                "name": alias,
                "description": description,
                "inputSchema": { "json": root_object_schema(&parameters) },
            }}),
            wire_name.to_owned(),
        ));
    }
    let fits = converted.len() <= MAX_TOOL_COUNT
        && serialized_len(
            &converted
                .iter()
                .map(|(_, value, _)| value)
                .collect::<Vec<_>>(),
        ) <= MAX_TOOL_CATALOG_BYTES;
    let mut kept: Vec<(usize, Value, String)> = converted;
    let mut omitted = 0;
    if !fits {
        kept.truncate(MAX_TOOL_COUNT);
        while serialized_len(&kept.iter().map(|(_, value, _)| value).collect::<Vec<_>>())
            > MAX_TOOL_CATALOG_BYTES
        {
            kept.pop();
        }
        omitted = requested.len() - kept.len();
    }
    let names: Vec<String> = kept
        .iter()
        .filter_map(|(_, value, _)| {
            value
                .pointer("/toolSpecification/name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    let mut additions = Vec::new();
    if let Some(nudge) = catalog_nudge(&names) {
        additions.push(nudge);
    }
    if omitted > 0 {
        additions.push(format!(
            "[opencodex] Kiro's outbound catalog budget allows {} of {} client tools this turn. {omitted} tools are omitted and unavailable this turn.",
            kept.len(),
            requested.len()
        ));
    }
    (
        kept.into_iter().map(|(_, value, _)| value).collect(),
        (!additions.is_empty()).then(|| additions.join("\n\n")),
    )
}

/// Keep the model from inventing tools this turn does not advertise.
fn catalog_nudge(wire_names: &[String]) -> Option<String> {
    if wire_names.is_empty() {
        return None;
    }
    let advertised: Vec<&str> = wire_names.iter().map(String::as_str).collect();
    let quoted = |names: &[&str]| {
        names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let listed = quoted(&advertised);
    let mut parts = vec![
        "Tool contract: use the current tool catalog as ground truth.".to_owned(),
        format!("Valid tool names for this turn are exactly {listed}."),
        "These listed names are the complete top-level tool-call surface for this turn.".to_owned(),
        "Call only listed names with their listed argument keys; do not invent, translate, or rename tools."
            .to_owned(),
        "Names mentioned only in instructions, tool descriptions, argument descriptions, or nested helper APIs are not additional top-level tools."
            .to_owned(),
        "If a listed tool exposes nested helpers such as a tools.* API, call the listed parent tool and use those helpers only inside that tool's input."
            .to_owned(),
    ];
    let unavailable: Vec<&str> = NEIGHBOR_TOOL_NAMES
        .iter()
        .copied()
        .filter(|name| !advertised.contains(name))
        .collect();
    if !unavailable.is_empty() {
        parts.push(format!(
            "Do not use neighboring-agent tool names {} unless this turn's catalog lists those exact names.",
            quoted(&unavailable)
        ));
    }
    parts.push("If you need shell, file search, file read, edit, or discovery behavior, choose the listed tool that provides that capability.".to_owned());
    parts.push("Count a tool call only after its tool result returns; batch independent read-only calls when the runtime supports it.".to_owned());
    Some(parts.join(" "))
}

fn serialized_len(tools: &[&Value]) -> usize {
    serde_json::to_string(tools)
        .map(|text| text.len())
        .unwrap_or(0)
}

fn truncate_description(description: &str, limit: usize) -> String {
    if description.chars().count() <= limit {
        return description.to_owned();
    }
    let mut kept: String = description.chars().take(limit.saturating_sub(1)).collect();
    if kept.ends_with(|ch: char| (0xd800..=0xdbff).contains(&(ch as u32))) {
        kept.pop();
    }
    format!("{kept}…")
}

/// Flatten root composition and add the root type Kiro requires.
fn root_object_schema(schema: &Value) -> Value {
    let mut object = match schema {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    let compositions: Vec<&str> = ["oneOf", "anyOf", "allOf"]
        .into_iter()
        .filter(|key| object.get(*key).is_some_and(Value::is_array))
        .collect();
    if compositions.is_empty() {
        object.insert("type".into(), json!("object"));
        return sanitize(&Value::Object(object));
    }
    let mut properties = Map::new();
    let mut required: Vec<String> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if let Some(root) = object.get("properties").and_then(Value::as_object) {
        properties.extend(root.clone());
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        let Some(variants) = object.get(key).and_then(Value::as_array) else {
            continue;
        };
        for variant in variants {
            if let Some(fields) = variant.get("properties").and_then(Value::as_object) {
                properties.extend(fields.clone());
            }
            if key == "allOf" {
                if let Some(names) = variant.get("required").and_then(Value::as_array) {
                    required.extend(names.iter().filter_map(Value::as_str).map(str::to_owned));
                }
            }
        }
    }
    let mut merged = Map::new();
    for (key, value) in &object {
        if ["oneOf", "anyOf", "allOf", "type", "properties", "required"].contains(&key.as_str()) {
            continue;
        }
        merged.insert(key.clone(), value.clone());
    }
    merged.insert("type".into(), json!("object"));
    if !properties.is_empty() {
        merged.insert("properties".into(), Value::Object(properties));
    }
    if !required.is_empty() {
        merged.insert("required".into(), json!(required));
    }
    sanitize(&Value::Object(merged))
}

fn sanitize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, child) in map {
                if REJECTED_SCHEMA_KEYS.contains(&key.as_str()) {
                    continue;
                }
                if key == "required" && child.as_array().is_some_and(Vec::is_empty) {
                    continue;
                }
                let child = if SCHEMA_MAP_KEYS.contains(&key.as_str()) {
                    sanitize_map(child)
                } else {
                    sanitize(child)
                };
                out.insert(key.clone(), child);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn sanitize_map(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(name, child)| (name.clone(), sanitize(child)))
                .collect(),
        ),
        other => sanitize(other),
    }
}
