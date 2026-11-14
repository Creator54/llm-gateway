//! Client requests in Responses or Messages form, rewritten for a Chat upstream.

use super::Error;
use serde_json::{json, Map, Value};

/// Responses request as an equivalent Chat completions body.
pub fn responses_to_chat(request: &Value, upstream_model: &str) -> Result<Value, Error> {
    let mut messages = Vec::new();
    if let Some(text) = request.get("instructions").and_then(Value::as_str) {
        messages.push(json!({ "role": "system", "content": text }));
    }
    match request.get("input") {
        Some(Value::String(text)) => messages.push(json!({ "role": "user", "content": text })),
        Some(Value::Array(items)) => {
            for item in items {
                responses_item(item, &mut messages)?;
            }
        }
        None => return Err(Error::bad_request("missing 'input'")),
        _ => return Err(Error::bad_request("'input' must be a string or array")),
    }

    let mut body = json!({
        "model": upstream_model,
        "messages": messages,
        "stream": request.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    let object = body.as_object_mut().expect("object");
    copy(request, object, "temperature", "temperature");
    copy(request, object, "top_p", "top_p");
    copy(request, object, "user", "user");
    copy(
        request,
        object,
        "parallel_tool_calls",
        "parallel_tool_calls",
    );
    copy(request, object, "max_output_tokens", "max_tokens");
    if let Some(effort) = request.pointer("/reasoning/effort") {
        object.insert("reasoning_effort".into(), effort.clone());
    }
    if let Some(tools) = chat_tools(request.get("tools")) {
        object.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = request.get("tool_choice") {
        let choice = match choice.get("name").and_then(Value::as_str) {
            Some(name) => json!({ "type": "function", "function": { "name": name } }),
            None => choice.clone(),
        };
        object.insert("tool_choice".into(), choice);
    }
    Ok(body)
}

/// Messages request as an equivalent Chat completions body.
pub fn messages_to_chat(request: &Value, upstream_model: &str) -> Result<Value, Error> {
    let mut messages = Vec::new();
    match request.get("system") {
        Some(Value::String(text)) if !text.is_empty() => {
            messages.push(json!({ "role": "system", "content": text }));
        }
        Some(Value::Array(parts)) => {
            let text = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                messages.push(json!({ "role": "system", "content": text }));
            }
        }
        _ => {}
    }
    for message in request
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        messages_message(message, &mut messages);
    }

    let mut body = json!({
        "model": upstream_model,
        "messages": messages,
        "stream": request.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    let object = body.as_object_mut().expect("object");
    copy(request, object, "max_tokens", "max_tokens");
    copy(request, object, "temperature", "temperature");
    copy(request, object, "top_p", "top_p");
    copy(request, object, "stop_sequences", "stop");
    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        let tools: Vec<Value> = tools
            .iter()
            .filter_map(|tool| {
                Some(json!({ "type": "function", "function": {
                    "name": tool.get("name")?,
                    "description": tool.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                }}))
            })
            .collect();
        if !tools.is_empty() {
            object.insert("tools".into(), Value::Array(tools));
        }
    }
    if let Some(choice) = request.get("tool_choice") {
        object.insert("tool_choice".into(), messages_tool_choice(choice));
    }
    Ok(body)
}

fn copy(source: &Value, target: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = source.get(from) {
        target.insert(to.to_owned(), value.clone());
    }
}

fn responses_item(item: &Value, messages: &mut Vec<Value>) -> Result<(), Error> {
    match item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
    {
        "message" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            let content = item
                .get("content")
                .ok_or_else(|| Error::bad_request("message input is missing content"))?;
            messages.push(json!({ "role": role, "content": content_to_chat(content) }));
        }
        "function_call" => {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
            messages.push(json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": { "name": name, "arguments": arguments_text(item.get("arguments")) }
                }]
            }));
        }
        "function_call_output" => messages.push(json!({
            "role": "tool",
            "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
            "content": content_to_text(item.get("output").unwrap_or(&Value::Null)),
        })),
        _ => {}
    }
    Ok(())
}

/// Content is a plain string or a list of typed parts.
fn content_to_chat(content: &Value) -> Value {
    let Value::Array(parts) = content else {
        return content.clone();
    };
    let converted: Vec<Value> = parts
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => Some(json!({
                "type": "text",
                "text": part.get("text").and_then(Value::as_str).unwrap_or_default(),
            })),
            Some("input_image" | "image_url") => {
                let url = part
                    .get("image_url")
                    .and_then(|value| value.get("url").or(Some(value)))
                    .or_else(|| part.get("url"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Some(json!({ "type": "image_url", "image_url": { "url": url } }))
            }
            _ => None,
        })
        .collect();
    Value::Array(converted)
}

fn messages_message(message: &Value, messages: &mut Vec<Value>) {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    let content = message.get("content").cloned().unwrap_or(Value::Null);
    if let Some(text) = content.as_str() {
        messages.push(json!({ "role": role, "content": text }));
        return;
    }
    let mut parts = Vec::new();
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for part in content.as_array().into_iter().flatten() {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(json!({
                "type": "text",
                "text": part.get("text").and_then(Value::as_str).unwrap_or_default(),
            })),
            Some("image") => {
                let source = part.get("source").unwrap_or(&Value::Null);
                let url = if source.get("type").and_then(Value::as_str) == Some("base64") {
                    format!(
                        "data:{};base64,{}",
                        source
                            .get("media_type")
                            .and_then(Value::as_str)
                            .unwrap_or("image/png"),
                        source
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                    )
                } else {
                    source
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                parts.push(json!({ "type": "image_url", "image_url": { "url": url } }));
            }
            Some("tool_use") => calls.push(json!({
                "id": part.get("id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": part.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": serde_json::to_string(part.get("input").unwrap_or(&json!({})))
                        .unwrap_or_else(|_| "{}".into()),
                }
            })),
            Some("tool_result") => results.push(json!({
                "role": "tool",
                "tool_call_id": part.get("tool_use_id").cloned().unwrap_or(Value::Null),
                "content": content_to_text(part.get("content").unwrap_or(&Value::Null)),
            })),
            _ => {}
        }
    }
    // Tool results must directly follow the assistant tool calls in Chat.
    messages.extend(results);
    if !calls.is_empty() {
        messages.push(json!({ "role": "assistant", "content": "", "tool_calls": calls }));
    } else if !parts.is_empty() {
        messages.push(json!({ "role": role, "content": parts }));
    }
}

fn messages_tool_choice(choice: &Value) -> Value {
    match choice.get("type").and_then(Value::as_str) {
        Some("auto") => json!("auto"),
        Some("any") => json!("required"),
        Some("none") => json!("none"),
        Some("tool") => match choice.get("name").and_then(Value::as_str) {
            Some(name) => json!({ "type": "function", "function": { "name": name } }),
            None => choice.clone(),
        },
        _ => choice.clone(),
    }
}

fn chat_tools(tools: Option<&Value>) -> Option<Vec<Value>> {
    let tools: Vec<Value> = tools?
        .as_array()?
        .iter()
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        .filter_map(|tool| {
            if let Some(function) = tool.get("function") {
                return Some(json!({ "type": "function", "function": function }));
            }
            Some(json!({ "type": "function", "function": {
                "name": tool.get("name")?,
                "description": tool.get("description").cloned().unwrap_or(Value::Null),
                "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            }}))
        })
        .collect();
    (!tools.is_empty()).then_some(tools)
}

fn arguments_text(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(other) => Value::String(serde_json::to_string(other).unwrap_or_else(|_| "{}".into())),
        None => Value::String("{}".into()),
    }
}

fn content_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item.get("text").and_then(Value::as_str) {
                Some(text) => text.to_owned(),
                None => serde_json::to_string(item).unwrap_or_default(),
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(object)
            if object.contains_key("content") || object.contains_key("structuredContent") =>
        {
            let mut sections = Vec::new();
            if let Some(content) = object.get("content") {
                let text = content_to_text(content);
                if !text.is_empty() {
                    sections.push(text);
                }
            }
            if let Some(structured) = object.get("structuredContent") {
                sections.push(serde_json::to_string(structured).unwrap_or_default());
            }
            sections.join("\n")
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_responses_request() {
        let request = json!({
            "model": "provider/model",
            "instructions": "Be terse",
            "stream": true,
            "max_output_tokens": 50,
            "reasoning": { "effort": "low" },
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "what is this" },
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA" }
                ]},
                { "type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}" },
                { "type": "function_call_output", "call_id": "call_1", "output": "file body" }
            ],
            "tools": [{ "type": "function", "name": "read", "description": "Read", "parameters": { "type": "object" } }],
            "tool_choice": { "type": "function", "name": "read" }
        });
        let chat = responses_to_chat(&request, "model").unwrap();
        assert_eq!(chat["model"], "model");
        assert_eq!(
            chat["messages"][0],
            json!({ "role": "system", "content": "Be terse" })
        );
        assert_eq!(chat["messages"][1]["content"][0]["text"], "what is this");
        assert_eq!(
            chat["messages"][1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
        assert_eq!(
            chat["messages"][2]["tool_calls"][0]["function"]["name"],
            "read"
        );
        assert_eq!(chat["messages"][3]["role"], "tool");
        assert_eq!(chat["messages"][3]["content"], "file body");
        assert_eq!(chat["tools"][0]["function"]["name"], "read");
        assert_eq!(chat["tool_choice"]["function"]["name"], "read");
        assert_eq!(chat["reasoning_effort"], "low");
        assert_eq!(chat["max_tokens"], 50);
        assert_eq!(chat["stream"], true);
    }

    #[test]
    fn converts_responses_image_object_and_status() {
        let request = json!({
            "model": "provider/model",
            "input": [{ "type": "message", "role": "user", "content": [
                { "type": "input_image", "image_url": { "url": "https://example.com/a.png" } }
            ]}]
        });
        let chat = responses_to_chat(&request, "model").unwrap();
        assert_eq!(
            chat["messages"][0]["content"][0]["image_url"]["url"],
            "https://example.com/a.png"
        );
        assert_eq!(chat["stream"], false);
        assert!(chat.get("tools").is_none());
        assert_eq!(
            responses_to_chat(&json!({ "model": "m" }), "m")
                .unwrap_err()
                .status,
            400
        );
    }

    #[test]
    fn converts_messages_request() {
        let request = json!({
            "model": "provider/model",
            "max_tokens": 100,
            "system": [{ "type": "text", "text": "Be terse" }],
            "messages": [
                { "role": "user", "content": [
                    { "type": "text", "text": "look" },
                    { "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": "AAAA" } }
                ]},
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "toolu_1", "name": "read", "input": { "path": "a" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "file body" }
                ]}
            ],
            "tools": [{ "name": "read", "description": "Read", "input_schema": { "type": "object" } }],
            "tool_choice": { "type": "tool", "name": "read" },
            "stop_sequences": ["END"]
        });
        let chat = messages_to_chat(&request, "model").unwrap();
        assert_eq!(
            chat["messages"][0],
            json!({ "role": "system", "content": "Be terse" })
        );
        assert_eq!(
            chat["messages"][1]["content"][1]["image_url"]["url"],
            "data:image/jpeg;base64,AAAA"
        );
        assert_eq!(
            chat["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        assert_eq!(chat["messages"][3]["role"], "tool");
        assert_eq!(chat["messages"][3]["content"], "file body");
        assert_eq!(
            chat["tools"][0]["function"]["parameters"],
            json!({ "type": "object" })
        );
        assert_eq!(chat["tool_choice"]["function"]["name"], "read");
        assert_eq!(chat["stop"], json!(["END"]));
        assert_eq!(chat["max_tokens"], 100);
        assert_eq!(chat["stream"], false);
    }

    #[test]
    fn maps_messages_tool_choice_modes() {
        let request = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "tool_choice": { "type": "any" }
        });
        assert_eq!(
            messages_to_chat(&request, "m").unwrap()["tool_choice"],
            "required"
        );
    }
}
