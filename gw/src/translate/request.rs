//! Client requests in Chat, Responses, or Messages form, rewritten for the upstream wire.

use super::{new_id, Error, ToolNamespaces};
use serde_json::{json, Map, Value};

/// Chat completions request as an equivalent Responses request body.
///
/// `stream_options` has no Responses equivalent: the upstream always reports usage in its
/// terminal event, so the stream encoder decides whether the client sees it.
pub fn chat_to_responses(request: &Value, upstream_model: &str) -> Result<Value, Error> {
    if !request.is_object() {
        return Err(Error::bad_request("request body must be a JSON object"));
    }
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::bad_request("missing 'messages'"))?;
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        chat_message(message, &mut instructions, &mut input)?;
    }
    if instructions.is_empty() && input.is_empty() {
        return Err(Error::bad_request(
            "messages must include at least one turn",
        ));
    }

    let mut body = json!({
        "model": upstream_model,
        "input": input,
        "stream": request.get("stream").and_then(Value::as_bool).unwrap_or(false),
        "store": false,
    });
    let object = body.as_object_mut().expect("object");
    if !instructions.is_empty() {
        object.insert(
            "instructions".into(),
            Value::String(instructions.join("\n\n")),
        );
    }
    if let Some(tools) = responses_tools(request.get("tools"))? {
        object.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = request.get("tool_choice") {
        object.insert("tool_choice".into(), responses_tool_choice(choice)?);
    }
    copy(request, object, "temperature", "temperature");
    copy(request, object, "top_p", "top_p");
    copy(request, object, "user", "user");
    copy(
        request,
        object,
        "parallel_tool_calls",
        "parallel_tool_calls",
    );
    copy(request, object, "service_tier", "service_tier");
    if let Some(max) = request
        .get("max_completion_tokens")
        .or_else(|| request.get("max_tokens"))
    {
        object.insert("max_output_tokens".into(), max.clone());
    }
    if let Some(effort) = request.get("reasoning_effort") {
        object.insert(
            "reasoning".into(),
            json!({ "effort": effort, "summary": "auto" }),
        );
    }
    Ok(body)
}

/// One Chat message as Responses `instructions` text or `input` items.
fn chat_message(
    message: &Value,
    instructions: &mut Vec<String>,
    input: &mut Vec<Value>,
) -> Result<(), Error> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    match role {
        "system" | "developer" => {
            let text = message
                .get("content")
                .map(content_to_text)
                .unwrap_or_default();
            if !text.trim().is_empty() {
                instructions.push(text.trim().to_owned());
            }
        }
        "user" => {
            let parts = user_parts(message.get("content"))?;
            if !parts.is_empty() {
                input.push(json!({ "type": "message", "role": "user", "content": parts }));
            }
        }
        "assistant" => {
            let text = message
                .get("content")
                .map(content_to_text)
                .unwrap_or_default();
            if !text.is_empty() {
                input.push(json!({ "type": "message", "role": "assistant",
                    "content": [{ "type": "output_text", "text": text }] }));
            }
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                input.push(chat_function_call(call));
            }
        }
        "tool" => {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::bad_request("tool messages require 'tool_call_id'"))?;
            let output = message
                .get("content")
                .map(content_to_text)
                .unwrap_or_default();
            input.push(json!({ "type": "function_call_output", "call_id": call_id,
                "output": output }));
        }
        other => {
            return Err(Error::bad_request(&format!(
                "unsupported message role '{other}'"
            )))
        }
    }
    Ok(())
}

/// Chat user content as Responses input parts; images keep their URL.
fn user_parts(content: Option<&Value>) -> Result<Vec<Value>, Error> {
    let Some(content) = content else {
        return Ok(Vec::new());
    };
    match content {
        Value::String(text) => {
            if text.is_empty() {
                return Ok(Vec::new());
            }
            Ok(vec![json!({ "type": "input_text", "text": text })])
        }
        Value::Array(parts) => {
            let mut converted = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => converted.push(json!({ "type": "input_text",
                        "text": part.get("text").and_then(Value::as_str).unwrap_or_default() })),
                    Some("image_url") => {
                        let url = part
                            .get("image_url")
                            .and_then(|value| value.get("url").or(Some(value)))
                            .or_else(|| part.get("url"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        converted.push(json!({ "type": "input_image", "image_url": url }));
                    }
                    other => {
                        return Err(Error::bad_request(&format!(
                            "unsupported content part '{}'",
                            other.unwrap_or("missing type")
                        )))
                    }
                }
            }
            Ok(converted)
        }
        _ => Err(Error::bad_request("'content' must be a string or array")),
    }
}

fn chat_function_call(call: &Value) -> Value {
    json!({
        "type": "function_call",
        "call_id": call.get("id").cloned().unwrap_or_else(|| json!(new_id("call"))),
        "name": call.pointer("/function/name").and_then(Value::as_str).unwrap_or("tool"),
        "arguments": call.pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    })
}

fn responses_tools(tools: Option<&Value>) -> Result<Option<Vec<Value>>, Error> {
    let Some(tools) = tools.and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut converted = Vec::new();
    for tool in tools {
        if let Some(other) = tool
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| *kind != "function")
        {
            return Err(Error::bad_request(&format!(
                "unsupported tool type '{other}'"
            )));
        }
        // Older clients send the function fields without the `function` wrapper.
        let function = tool.get("function").unwrap_or(tool);
        let name = function
            .get("name")
            .ok_or_else(|| Error::bad_request("tool is missing 'name'"))?;
        let mut entry = json!({
            "type": "function",
            "name": name,
            "description": function.get("description").cloned().unwrap_or(Value::Null),
            "parameters": function.get("parameters").cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
        });
        if let Some(strict) = function.get("strict") {
            entry["strict"] = strict.clone();
        }
        converted.push(entry);
    }
    Ok((!converted.is_empty()).then_some(converted))
}

fn responses_tool_choice(choice: &Value) -> Result<Value, Error> {
    let Some(kind) = choice.get("type").and_then(Value::as_str) else {
        // `auto`, `none`, and `required` are spelled the same on both wires.
        return Ok(choice.clone());
    };
    match kind {
        "function" => {
            let name = choice
                .pointer("/function/name")
                .or_else(|| choice.get("name"))
                .ok_or_else(|| Error::bad_request("tool_choice is missing 'name'"))?;
            Ok(json!({ "type": "function", "name": name }))
        }
        other => Err(Error::bad_request(&format!(
            "unsupported tool_choice type '{other}'"
        ))),
    }
}

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
    if let Some(tools) = chat_tools(request.get("tools"))? {
        object.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = request.get("tool_choice") {
        let choice = match choice.get("name").and_then(Value::as_str) {
            Some(name) => {
                let namespace = choice.get("namespace").and_then(Value::as_str);
                let name = namespaced(namespace.unwrap_or_default(), name);
                json!({ "type": "function", "function": { "name": name } })
            }
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
        let mut converted = Vec::new();
        for tool in tools {
            match tool.get("type").and_then(Value::as_str) {
                // Older clients send custom tools without a type.
                None | Some("custom") => {}
                Some(other) => {
                    return Err(Error::bad_request(&format!(
                        "unsupported tool type '{other}'"
                    )))
                }
            }
            let name = tool
                .get("name")
                .ok_or_else(|| Error::bad_request("tool is missing 'name'"))?;
            converted.push(json!({ "type": "function", "function": {
                "name": name,
                "description": tool.get("description").cloned().unwrap_or(Value::Null),
                "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
            }}));
        }
        if !converted.is_empty() {
            object.insert("tools".into(), Value::Array(converted));
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
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    match kind {
        "message" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            let content = item
                .get("content")
                .ok_or_else(|| Error::bad_request("message input is missing content"))?;
            messages.push(json!({ "role": role, "content": content_to_chat(content) }));
        }
        "function_call" => {
            let call = json!({
                "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": namespaced(
                        item.get("namespace").and_then(Value::as_str).unwrap_or_default(),
                        item.get("name").and_then(Value::as_str).unwrap_or("tool"),
                    ),
                    "arguments": arguments_text(item.get("arguments")),
                }
            });
            let merge = messages.last().is_some_and(|last| {
                last.get("role").and_then(Value::as_str) == Some("assistant")
                    && last.get("tool_calls").is_some_and(Value::is_array)
            });
            if !merge {
                messages
                    .push(json!({ "role": "assistant", "content": null, "tool_calls": [call] }));
                return Ok(());
            }
            if let Some(calls) = messages
                .last_mut()
                .and_then(|last| last.get_mut("tool_calls"))
                .and_then(Value::as_array_mut)
            {
                calls.push(call);
            }
        }
        "function_call_output" => messages.push(json!({
            "role": "tool",
            "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
            "content": content_to_text(item.get("output").unwrap_or(&Value::Null)),
        })),
        other => {
            return Err(Error::bad_request(&format!(
                "unsupported input item type '{other}'"
            )))
        }
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
        let text = parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        messages.push(json!({ "role": role, "content": text, "tool_calls": calls }));
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

/// Responses tool types executed by OpenAI rather than the client.
const HOSTED_TOOLS: [&str; 11] = [
    "web_search",
    "web_search_preview",
    "file_search",
    "computer_use_preview",
    "computer_use",
    "code_interpreter",
    "image_generation",
    "image_gen",
    "mcp",
    "tool_search",
    "local_shell",
];

/// Wire name of a namespaced tool for upstreams that only know flat function names.
fn namespaced(namespace: &str, name: &str) -> String {
    match namespace {
        "" | "functions" => name.to_owned(),
        _ => format!("{namespace}__{name}"),
    }
}

/// Flattened wire name to `(namespace, name)` for every namespaced tool in a Responses request,
/// so replies can name calls the way the client declared them.
pub fn tool_namespaces(request: &Value) -> ToolNamespaces {
    let mut map = ToolNamespaces::new();
    let groups = request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for group in groups.filter(|tool| tool.get("type").and_then(Value::as_str) == Some("namespace"))
    {
        let namespace = group
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        for child in group
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = child.get("name").and_then(Value::as_str) {
                let wire = namespaced(namespace, name);
                if wire != name {
                    map.insert(wire, (namespace.to_owned(), name.to_owned()));
                }
            }
        }
    }
    map
}

fn chat_tools(tools: Option<&Value>) -> Result<Option<Vec<Value>>, Error> {
    let Some(tools) = tools.and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut converted = Vec::new();
    for tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            Some("function") => {}
            Some("namespace") => {
                let namespace = tool.get("name").and_then(Value::as_str).unwrap_or_default();
                for child in tool
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(name) = child.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let mut child = child.clone();
                    child["name"] = Value::String(namespaced(namespace, name));
                    converted.extend(chat_tools(Some(&json!([child])))?.into_iter().flatten());
                }
                continue;
            }
            // Hosted tools run on OpenAI's side; a Chat upstream can only lack them.
            Some(hosted) if HOSTED_TOOLS.contains(&hosted) => continue,
            // Older clients omit `type` and send `function` or `name` directly.
            None if tool.get("function").is_some() || tool.get("name").is_some() => {}
            Some(other) => {
                return Err(Error::bad_request(&format!(
                    "unsupported tool type '{other}'"
                )))
            }
            None => return Err(Error::bad_request("tool is missing 'type'")),
        }
        let function = match tool.get("function") {
            Some(function) => function.clone(),
            None => json!({
                "name": tool.get("name").ok_or_else(|| Error::bad_request("tool is missing 'name'"))?,
                "description": tool.get("description").cloned().unwrap_or(Value::Null),
                "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            }),
        };
        converted.push(json!({ "type": "function", "function": function }));
    }
    Ok((!converted.is_empty()).then_some(converted))
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

    #[test]
    fn merges_messages_text_and_tool_use_into_one_turn() {
        let request = json!({
            "model": "m",
            "messages": [{ "role": "assistant", "content": [
                { "type": "text", "text": "one" },
                { "type": "text", "text": "two" },
                { "type": "tool_use", "id": "toolu_1", "name": "read", "input": { "path": "a" } }
            ]}]
        });
        let chat = messages_to_chat(&request, "m").unwrap();
        assert_eq!(chat["messages"].as_array().unwrap().len(), 1);
        assert_eq!(chat["messages"][0]["role"], "assistant");
        assert_eq!(chat["messages"][0]["content"], "one\ntwo");
        assert_eq!(
            chat["messages"][0]["tool_calls"][0]["function"]["name"],
            "read"
        );
    }

    #[test]
    fn merges_consecutive_function_calls() {
        let request = json!({
            "model": "m",
            "input": [
                { "type": "message", "role": "user", "content": "go" },
                { "type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{}" },
                { "type": "function_call", "call_id": "call_2", "name": "write", "arguments": "{}" }
            ]
        });
        let chat = responses_to_chat(&request, "m").unwrap();
        let messages = chat["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], Value::Null);
        let calls = messages[1]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[1]["id"], "call_2");
    }

    #[test]
    fn rejects_unknown_input_item_type() {
        let request = json!({
            "model": "m",
            "input": [
                { "type": "function_call", "call_id": "c", "name": "read", "arguments": "{}" },
                { "type": "reasoning", "summary": [] }
            ]
        });
        let error = responses_to_chat(&request, "m").unwrap_err();
        assert_eq!(error.status, 400);
        assert!(error.message.contains("reasoning"), "{}", error.message);
    }

    #[test]
    fn rejects_unsupported_tool_types() {
        let responses = json!({ "model": "m", "input": "hi",
            "tools": [{ "type": "custom", "name": "apply_patch" }] });
        let error = responses_to_chat(&responses, "m").unwrap_err();
        assert_eq!(error.status, 400);
        assert!(error.message.contains("custom"), "{}", error.message);

        let messages = json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "type": "computer_20241022", "name": "computer" }] });
        let error = messages_to_chat(&messages, "m").unwrap_err();
        assert_eq!(error.status, 400);
        assert!(
            error.message.contains("computer_20241022"),
            "{}",
            error.message
        );
    }

    #[test]
    fn flattens_namespaces_and_drops_hosted_tools() {
        let responses = json!({ "model": "m",
            "input": [{ "type": "function_call", "call_id": "c1", "namespace": "multi_agent_v1",
                "name": "wait_agent", "arguments": "{}" }],
            "tools": [
                { "type": "function", "name": "exec_command" },
                { "type": "namespace", "name": "multi_agent_v1", "tools": [
                    { "type": "function", "name": "spawn_agent", "parameters": { "type": "object" } },
                    { "type": "function", "name": "wait_agent" },
                ] },
                { "type": "web_search", "external_web_access": false },
            ] });
        let chat = responses_to_chat(&responses, "m").unwrap();
        let names: Vec<_> = chat["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "exec_command",
                "multi_agent_v1__spawn_agent",
                "multi_agent_v1__wait_agent"
            ]
        );
        assert_eq!(
            chat["messages"][0]["tool_calls"][0]["function"]["name"],
            "multi_agent_v1__wait_agent"
        );
    }
}
