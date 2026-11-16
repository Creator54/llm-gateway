//! Chat Completions clients against a Messages-only upstream (Anthropic).

use super::{new_id, now, Error};
use serde_json::{json, Map, Value};

/// Messages requires `max_tokens`; Chat clients often leave it out.
const DEFAULT_MAX_TOKENS: u64 = 8192;

/// A Chat Completions request as a Messages request.
pub fn chat_request_to_messages(chat: &Value, upstream_model: &str) -> Result<Value, Error> {
    let source = chat
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::bad_request("`messages` must be an array"))?;
    let mut system = Vec::new();
    let mut turns: Vec<(String, Vec<Value>)> = Vec::new();
    for message in source {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "system" | "developer" => {
                let text = text_of(message.get("content"));
                if !text.is_empty() {
                    system.push(json!({ "type": "text", "text": text }));
                }
            }
            "assistant" => {
                let mut blocks = content_blocks(message.get("content"));
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    blocks.push(tool_use(call));
                }
                push_turn(&mut turns, "assistant", blocks);
            }
            "tool" | "function" => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": text_of(message.get("content")),
                });
                push_turn(&mut turns, "user", vec![block]);
            }
            _ => push_turn(&mut turns, "user", content_blocks(message.get("content"))),
        }
    }
    if turns.is_empty() {
        return Err(Error::bad_request(
            "`messages` has no user or assistant turn",
        ));
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(upstream_model));
    let max_tokens = chat
        .get("max_completion_tokens")
        .or_else(|| chat.get("max_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    body.insert("max_tokens".into(), json!(max_tokens));
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    let messages: Vec<Value> = turns
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect();
    body.insert("messages".into(), Value::Array(messages));
    for key in ["temperature", "top_p"] {
        if let Some(value) = chat.get(key).filter(|value| value.is_number()) {
            body.insert(key.into(), value.clone());
        }
    }
    match chat.get("stop") {
        Some(Value::String(stop)) => {
            body.insert("stop_sequences".into(), json!([stop]));
        }
        Some(Value::Array(stops)) if !stops.is_empty() => {
            body.insert("stop_sequences".into(), Value::Array(stops.clone()));
        }
        _ => {}
    }
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        let tools: Vec<Value> = tools.iter().filter_map(messages_tool).collect();
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
            if let Some(choice) = chat.get("tool_choice").and_then(tool_choice) {
                body.insert("tool_choice".into(), choice);
            }
        }
    }
    if let Some(user) = chat.get("user").and_then(Value::as_str) {
        body.insert("metadata".into(), json!({ "user_id": user }));
    }
    let stream = chat.get("stream").and_then(Value::as_bool).unwrap_or(false);
    body.insert("stream".into(), json!(stream));
    Ok(Value::Object(body))
}

/// Merge into the previous turn when the role repeats; Messages needs alternation.
fn push_turn(turns: &mut Vec<(String, Vec<Value>)>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    match turns.last_mut() {
        Some((last, content)) if last == role => content.extend(blocks),
        _ => turns.push((role.to_owned(), blocks)),
    }
}

fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn content_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({ "type": "text", "text": text })]
        }
        Some(Value::Array(parts)) => parts.iter().filter_map(content_block).collect(),
        _ => Vec::new(),
    }
}

fn content_block(part: &Value) -> Option<Value> {
    match part.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = part.get("text").and_then(Value::as_str)?;
            (!text.is_empty()).then(|| json!({ "type": "text", "text": text }))
        }
        Some("image_url") => {
            let url = part
                .pointer("/image_url/url")
                .or_else(|| part.get("image_url"))
                .and_then(Value::as_str)?;
            Some(image_block(url))
        }
        _ => None,
    }
}

fn image_block(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((media, data)) = rest.split_once(";base64,") {
            return json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media, "data": data },
            });
        }
    }
    json!({ "type": "image", "source": { "type": "url", "url": url } })
}

fn tool_use(call: &Value) -> Value {
    let arguments = call
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let input: Value = serde_json::from_str(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    json!({
        "type": "tool_use",
        "id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
        "name": call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default(),
        "input": input,
    })
}

fn messages_tool(tool: &Value) -> Option<Value> {
    let function = tool.get("function")?;
    let mut out = Map::new();
    out.insert("name".into(), function.get("name")?.clone());
    if let Some(description) = function.get("description") {
        out.insert("description".into(), description.clone());
    }
    let schema = function
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    out.insert("input_schema".into(), schema);
    Some(Value::Object(out))
}

fn tool_choice(choice: &Value) -> Option<Value> {
    match choice {
        Value::String(kind) => match kind.as_str() {
            "auto" => Some(json!({ "type": "auto" })),
            "required" => Some(json!({ "type": "any" })),
            "none" => Some(json!({ "type": "none" })),
            _ => None,
        },
        Value::Object(_) => {
            let name = choice.pointer("/function/name").and_then(Value::as_str)?;
            Some(json!({ "type": "tool", "name": name }))
        }
        _ => None,
    }
}

fn finish_reason(stop: Option<&str>) -> &'static str {
    match stop {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    }
}

/// Messages `usage` as Chat `usage`; Chat counts cached input inside the prompt.
fn chat_usage(usage: &Value) -> Value {
    let number = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let cached = number("cache_read_input_tokens");
    let prompt = number("input_tokens") + cached + number("cache_creation_input_tokens");
    let completion = number("output_tokens");
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
        "prompt_tokens_details": { "cached_tokens": cached },
    })
}

/// A Messages reply as a Chat completion.
pub fn messages_reply_to_chat(reply: &Value, requested_model: &str) -> Result<Value, Error> {
    let blocks = reply
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::bad_gateway("upstream reply has no content"))?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => calls.push(json!({
                "id": block.get("id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": block.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": block.get("input").unwrap_or(&json!({})).to_string(),
                },
            })),
            _ => {}
        }
    }
    let mut message = json!({ "role": "assistant", "content": text });
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    let stop = reply.get("stop_reason").and_then(Value::as_str);
    let mut chat = json!({
        "id": reply.get("id").and_then(Value::as_str).map_or_else(|| new_id("chatcmpl"), str::to_owned),
        "object": "chat.completion",
        "created": now(),
        "model": requested_model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason(stop) }],
    });
    if let Some(usage) = reply.get("usage") {
        chat["usage"] = chat_usage(usage);
    }
    Ok(chat)
}

/// Turns Messages SSE events into Chat completion chunks.
pub struct MessagesToChat {
    id: String,
    model: String,
    created: u64,
    include_usage: bool,
    /// Content block index of each tool call, in announcement order.
    calls: Vec<u64>,
    usage: Map<String, Value>,
    stop: Option<String>,
    started: bool,
    done: bool,
}

impl MessagesToChat {
    /// Start a stream; `include_usage` mirrors the client's `stream_options`.
    pub fn new(model: impl Into<String>, include_usage: bool) -> Self {
        Self {
            id: new_id("chatcmpl"),
            model: model.into(),
            created: now(),
            include_usage,
            calls: Vec::new(),
            usage: Map::new(),
            stop: None,
            started: false,
            done: false,
        }
    }

    fn frame(&self, delta: Value, finish: Option<&str>) -> Vec<u8> {
        let data = json!({
            "id": self.id, "object": "chat.completion.chunk",
            "created": self.created, "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        format!("data: {data}\n\n").into_bytes()
    }

    fn open(&mut self) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        self.frame(json!({ "role": "assistant", "content": "" }), None)
    }

    fn merge_usage(&mut self, usage: Option<&Value>) {
        if let Some(Value::Object(usage)) = usage {
            for (key, value) in usage {
                self.usage.insert(key.clone(), value.clone());
            }
        }
    }

    /// Feed one Messages SSE payload and get the Chat chunks it produces.
    pub fn push_event(&mut self, data: &str) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                self.merge_usage(event.pointer("/message/usage"));
                out.extend(self.open());
            }
            "content_block_start" => {
                let block = &event["content_block"];
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    out.extend(self.open());
                    self.calls
                        .push(event.get("index").and_then(Value::as_u64).unwrap_or(0));
                    out.extend(self.frame(
                        json!({ "tool_calls": [{
                            "index": self.calls.len() - 1,
                            "id": block.get("id").cloned().unwrap_or(Value::Null),
                            "type": "function",
                            "function": { "name": block.get("name").cloned().unwrap_or(Value::Null), "arguments": "" },
                        }] }),
                        None,
                    ));
                }
            }
            "content_block_delta" => {
                let delta = &event["delta"];
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if !text.is_empty() {
                            out.extend(self.open());
                            out.extend(self.frame(json!({ "content": text }), None));
                        }
                    }
                    Some("input_json_delta") => {
                        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if let (Some(slot), false) = (
                            self.calls.iter().position(|&known| known == index),
                            partial.is_empty(),
                        ) {
                            out.extend(self.frame(
                                json!({ "tool_calls": [{ "index": slot, "function": { "arguments": partial } }] }),
                                None,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(stop) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(stop.to_owned());
                }
                self.merge_usage(event.get("usage"));
            }
            "message_stop" => out.extend(self.close()),
            "error" => {
                let message = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream failed");
                let error = json!({ "error": { "message": message, "type": "upstream_error" } });
                out.extend(format!("data: {error}\n\ndata: [DONE]\n\n").into_bytes());
                self.done = true;
            }
            _ => {}
        }
        out
    }

    fn close(&mut self) -> Vec<u8> {
        self.done = true;
        let mut out = self.open();
        out.extend(self.frame(json!({}), Some(finish_reason(self.stop.as_deref()))));
        if self.include_usage {
            let data = json!({
                "id": self.id, "object": "chat.completion.chunk",
                "created": self.created, "model": self.model,
                "choices": [], "usage": chat_usage(&Value::Object(self.usage.clone())),
            });
            out.extend(format!("data: {data}\n\n").into_bytes());
        }
        out.extend_from_slice(b"data: [DONE]\n\n");
        out
    }

    /// Closing frames if the upstream ended without `message_stop`.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        self.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_maps_system_tools_and_tool_results() {
        let chat = json!({
            "model": "anthropic/claude-haiku-4-5",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "get", "arguments": "{\"city\":\"x\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"},
                {"role": "tool", "tool_call_id": "call_2", "content": "warm"},
                {"role": "user", "content": [{"type": "text", "text": "thanks"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}}]}
            ],
            "tools": [{"type": "function", "function": {"name": "get", "parameters": {"type": "object"}}}],
            "tool_choice": "required",
            "stop": "END",
            "stream": true,
        });
        let body = chat_request_to_messages(&chat, "claude-haiku-4-5").unwrap();
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["system"][0]["text"], "be brief");
        assert_eq!(body["stop_sequences"], json!(["END"]));
        assert_eq!(body["tool_choice"], json!({"type": "any"}));
        assert_eq!(body["tools"][0]["input_schema"], json!({"type": "object"}));
        let messages = body["messages"].as_array().unwrap();
        let roles: Vec<_> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        assert_eq!(messages[1]["content"][0]["input"], json!({"city": "x"}));
        // Both tool results and the next user turn share one user message.
        let last = messages[2]["content"].as_array().unwrap();
        assert_eq!(last.len(), 4);
        assert_eq!(last[1]["tool_use_id"], "call_2");
        assert_eq!(last[3]["source"]["media_type"], "image/png");
    }

    #[test]
    fn request_without_turns_is_rejected() {
        let chat = json!({"messages": [{"role": "system", "content": "x"}]});
        assert_eq!(
            chat_request_to_messages(&chat, "m").unwrap_err().status,
            400
        );
    }

    #[test]
    fn reply_maps_text_tools_and_usage() {
        let reply = json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "tu_1", "name": "get", "input": {"a": 1}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "cache_read_input_tokens": 5, "output_tokens": 3},
        });
        let chat = messages_reply_to_chat(&reply, "anthropic/claude").unwrap();
        assert_eq!(chat["object"], "chat.completion");
        assert_eq!(chat["model"], "anthropic/claude");
        assert_eq!(chat["choices"][0]["message"]["content"], "hi");
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        let call = &chat["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["function"]["arguments"], "{\"a\":1}");
        assert_eq!(chat["usage"]["prompt_tokens"], 15);
        assert_eq!(chat["usage"]["prompt_tokens_details"]["cached_tokens"], 5);
        assert_eq!(chat["usage"]["total_tokens"], 18);
    }

    #[test]
    fn stream_maps_text_tool_calls_and_usage() {
        let events = [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"get"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let mut stream = MessagesToChat::new("anthropic/claude", true);
        let mut out = Vec::new();
        for event in events {
            out.extend(stream.push_event(event));
        }
        out.extend(stream.finish());
        let text = String::from_utf8(out).unwrap();
        let chunks: Vec<Value> = text
            .split("\n\n")
            .filter_map(|frame| frame.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "hi");
        assert_eq!(
            chunks[2]["choices"][0]["delta"]["tool_calls"][0]["id"],
            "tu_1"
        );
        assert_eq!(
            chunks[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":"
        );
        assert_eq!(chunks[4]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chunks[5]["usage"]["prompt_tokens"], 7);
        assert_eq!(chunks[5]["usage"]["completion_tokens"], 4);
        assert!(text.ends_with("data: [DONE]\n\n"));
    }
}
