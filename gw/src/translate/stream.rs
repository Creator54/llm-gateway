//! Live Chat chunks rewritten as Responses or Messages events.

use super::{
    chat_usage, messages_usage, name_call, new_id, now, responses_usage, stop_reason,
    ToolNamespaces,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

fn sse(event: &str, data: &Value) -> Vec<u8> {
    let data = serde_json::to_string(data).unwrap_or_else(|_| "{}".into());
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

/// Reads one Chat completion chunk, ignoring anything unexpected.
fn chunk(data: &str) -> Option<Value> {
    if data.trim() == "[DONE]" {
        return None;
    }
    serde_json::from_str(data).ok()
}

/// `index` of a streamed tool call; Chat defaults to the first slot.
fn call_index(call: &Value) -> u64 {
    call.get("index").and_then(Value::as_u64).unwrap_or(0)
}

fn call_argument(call: &Value) -> Option<&str> {
    call.pointer("/function/arguments")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// Chat tool call name, appended because it may arrive in pieces.
fn append_name(state: &mut String, call: &Value) {
    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
        state.push_str(name);
    }
}

struct CallState {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    announced: bool,
}

impl CallState {
    fn new() -> Self {
        Self {
            item_id: new_id("fc"),
            call_id: String::new(),
            name: String::new(),
            arguments: String::new(),
            announced: false,
        }
    }

    fn item(&self, status: &str, arguments: &str, namespaces: &ToolNamespaces) -> Value {
        let mut item = json!({
            "id": self.item_id, "type": "function_call", "status": status,
            "call_id": self.call_id, "arguments": arguments,
        });
        name_call(&mut item, &self.name, namespaces);
        item
    }
}

/// Turns Responses SSE events into Chat completion chunks.
pub struct ChatStream {
    id: String,
    model: String,
    created: u64,
    include_usage: bool,
    /// Tool call slot per Responses item id, in announcement order.
    calls: Vec<String>,
    started: bool,
    done: bool,
}

impl ChatStream {
    /// Start a stream; `include_usage` mirrors the client's `stream_options`.
    pub fn new(model: impl Into<String>, include_usage: bool) -> Self {
        Self {
            id: new_id("chatcmpl"),
            model: model.into(),
            created: now(),
            include_usage,
            calls: Vec::new(),
            started: false,
            done: false,
        }
    }

    fn frame(&self, delta: Value, finish: Option<&str>, usage: Option<Value>) -> Vec<u8> {
        let mut data = json!({
            "id": self.id, "object": "chat.completion.chunk",
            "created": self.created, "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        if let Some(usage) = usage {
            data["usage"] = usage;
            data["choices"] = json!([]);
        }
        let data = serde_json::to_string(&data).unwrap_or_else(|_| "{}".into());
        format!("data: {data}\n\n").into_bytes()
    }

    fn open(&mut self) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        self.frame(json!({ "role": "assistant", "content": "" }), None, None)
    }

    fn slot(&self, item_id: &str) -> Option<usize> {
        self.calls.iter().position(|known| known == item_id)
    }

    /// Feed one Responses SSE payload and get the Chat chunks it produces.
    pub fn push_event(&mut self, data: &str) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" => {
                out.extend(self.open());
                let text = event.get("delta").and_then(Value::as_str).unwrap_or("");
                if !text.is_empty() {
                    out.extend(self.frame(json!({ "content": text }), None, None));
                }
            }
            "response.output_item.added" => {
                let item = &event["item"];
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    out.extend(self.open());
                    let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                    self.calls.push(item_id.to_owned());
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or(item_id);
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    out.extend(self.frame(
                        json!({ "tool_calls": [{
                            "index": self.calls.len() - 1, "id": call_id, "type": "function",
                            "function": { "name": name, "arguments": "" }
                        }] }),
                        None,
                        None,
                    ));
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = event.get("item_id").and_then(Value::as_str).unwrap_or("");
                let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
                if let (Some(index), false) = (self.slot(item_id), delta.is_empty()) {
                    out.extend(self.frame(
                        json!({ "tool_calls": [{
                            "index": index, "function": { "arguments": delta }
                        }] }),
                        None,
                        None,
                    ));
                }
            }
            kind @ ("response.completed" | "response.incomplete") => {
                out.extend(self.open());
                let finish = if !self.calls.is_empty() {
                    "tool_calls"
                } else if kind == "response.incomplete" {
                    "length"
                } else {
                    "stop"
                };
                out.extend(self.frame(json!({}), Some(finish), None));
                if self.include_usage {
                    if let Some(usage) = event.pointer("/response/usage") {
                        out.extend(self.frame(json!({}), None, Some(chat_usage(usage))));
                    }
                }
                out.extend_from_slice(b"data: [DONE]\n\n");
                self.done = true;
            }
            "response.failed" | "error" => {
                tracing::warn!(%event, "upstream ended the stream with an error");
                let message = event
                    .pointer("/response/error/message")
                    .or_else(|| event.pointer("/error/message"))
                    .or_else(|| event.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream failed");
                let error = json!({ "error": { "message": message, "type": "upstream_error" } });
                out.extend(format!("data: {error}\n\n").into_bytes());
                out.extend_from_slice(b"data: [DONE]\n\n");
                self.done = true;
            }
            _ => {}
        }
        out
    }

    /// Closing frames if the upstream ended without a terminal event.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out = self.open();
        let finish = if self.calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        out.extend(self.frame(json!({}), Some(finish), None));
        out.extend_from_slice(b"data: [DONE]\n\n");
        out
    }
}

/// Turns Chat completion chunks into Responses SSE events.
pub struct ResponsesStream {
    response_id: String,
    model: String,
    message_id: String,
    text: String,
    calls: BTreeMap<u64, CallState>,
    namespaces: ToolNamespaces,
    usage: Value,
    /// The upstream Chat finish reason, when the final chunk carried one.
    finish: Option<String>,
    seq: u64,
    started: bool,
    done: bool,
}

impl ResponsesStream {
    /// Start a stream for one client response; `request` is the client's Responses body.
    pub fn new(response_id: impl Into<String>, model: impl Into<String>, request: &Value) -> Self {
        Self {
            response_id: response_id.into(),
            model: model.into(),
            message_id: new_id("msg"),
            text: String::new(),
            calls: BTreeMap::new(),
            namespaces: super::tool_namespaces(request),
            usage: json!({}),
            finish: None,
            seq: 0,
            started: false,
            done: false,
        }
    }

    /// Opening events; called automatically by `push_chunk`.
    pub fn start(&mut self) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let created = self.response("in_progress", Vec::new());
        let mut out = self.event("response.created", json!({ "response": created }));
        let created = self.response("in_progress", Vec::new());
        out.extend(self.event("response.in_progress", json!({ "response": created })));
        out.extend(self.event(
            "response.output_item.added",
            json!({ "output_index": 0, "item": {
                "id": self.message_id, "type": "message", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
        ));
        out.extend(self.event(
            "response.content_part.added",
            json!({ "item_id": self.message_id, "output_index": 0, "content_index": 0,
                "part": { "type": "output_text", "text": "", "annotations": [] }
            }),
        ));
        out
    }

    /// Feed one Chat SSE payload and get the Responses events it produces.
    pub fn push_chunk(&mut self, data: &str) -> Vec<u8> {
        let mut out = self.start();
        if self.done {
            return out;
        }
        let Some(chunk) = chunk(data) else {
            return out;
        };
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            self.usage = usage.clone();
        }
        if let Some(finish) = chunk
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            self.finish = Some(finish.to_owned());
        }
        let Some(delta) = chunk.pointer("/choices/0/delta") else {
            return out;
        };
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.text.push_str(text);
            let event = self.event(
                "response.output_text.delta",
                json!({ "item_id": self.message_id, "output_index": 0,
                    "content_index": 0, "delta": text
                }),
            );
            out.extend(event);
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = call_index(call);
            let id = call.get("id").and_then(Value::as_str);
            let arguments = call_argument(call);
            {
                let state = self.calls.entry(index).or_insert_with(CallState::new);
                if let Some(id) = id {
                    state.call_id.push_str(id);
                }
                append_name(&mut state.name, call);
                if let Some(arguments) = arguments {
                    state.arguments.push_str(arguments);
                }
            }
            // The name is usually only whole once the id or the arguments arrive.
            let announce = self.calls.get(&index).is_some_and(|state| {
                !state.announced && !state.name.is_empty() && (id.is_some() || arguments.is_some())
            });
            if announce {
                let item = {
                    let state = self.calls.get_mut(&index).expect("call state");
                    state.announced = true;
                    state.item("in_progress", "", &self.namespaces)
                };
                let event = self.event(
                    "response.output_item.added",
                    json!({ "output_index": index + 1, "item": item }),
                );
                out.extend(event);
            }
            if let Some(arguments) = arguments {
                let item_id = self
                    .calls
                    .get(&index)
                    .and_then(|state| state.announced.then(|| state.item_id.clone()));
                if let Some(item_id) = item_id {
                    let event = self.event(
                        "response.function_call_arguments.delta",
                        json!({ "item_id": item_id, "output_index": index + 1, "delta": arguments }),
                    );
                    out.extend(event);
                }
            }
        }
        out
    }

    /// Closing events, including the finished response and `[DONE]`.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        let mut out = self.start();
        self.done = true;
        out.extend(self.event(
            "response.output_text.done",
            json!({ "item_id": self.message_id, "output_index": 0,
                "content_index": 0, "text": self.text
            }),
        ));
        out.extend(self.event(
            "response.content_part.done",
            json!({ "item_id": self.message_id, "output_index": 0, "content_index": 0,
                "part": { "type": "output_text", "text": self.text, "annotations": [] }
            }),
        ));
        let item = self.message_item();
        out.extend(self.event(
            "response.output_item.done",
            json!({ "output_index": 0, "item": item }),
        ));
        let calls: Vec<(u64, CallState)> = self
            .calls
            .iter()
            .map(|(index, state)| {
                (
                    *index,
                    CallState {
                        item_id: state.item_id.clone(),
                        call_id: state.call_id.clone(),
                        name: state.name.clone(),
                        arguments: state.arguments.clone(),
                        announced: state.announced,
                    },
                )
            })
            .collect();
        for (index, state) in calls {
            if !state.announced {
                let item = state.item("in_progress", "", &self.namespaces);
                let event = self.event(
                    "response.output_item.added",
                    json!({ "output_index": index + 1, "item": item }),
                );
                out.extend(event);
            }
            out.extend(self.event(
                "response.function_call_arguments.done",
                json!({ "item_id": state.item_id, "output_index": index + 1,
                    "arguments": state.arguments
                }),
            ));
            let item = state.item("completed", &state.arguments, &self.namespaces);
            out.extend(self.event(
                "response.output_item.done",
                json!({ "output_index": index + 1, "item": item }),
            ));
        }
        let status = if self.finish.as_deref() == Some("length") {
            "incomplete"
        } else {
            "completed"
        };
        let response = self.response(status, self.output_items());
        let kind = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        out.extend(self.event(kind, json!({ "response": response })));
        out.extend_from_slice(b"data: [DONE]\n\n");
        out
    }

    fn message_item(&self) -> Value {
        json!({ "id": self.message_id, "type": "message", "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": self.text, "annotations": [] }]
        })
    }

    fn output_items(&self) -> Vec<Value> {
        let mut output = vec![self.message_item()];
        for state in self.calls.values() {
            output.push(state.item("completed", &state.arguments, &self.namespaces));
        }
        output
    }

    fn response(&self, status: &str, output: Vec<Value>) -> Value {
        let mut response = json!({ "id": self.response_id, "object": "response", "created_at": now(),
            "status": status, "error": null, "model": self.model, "output": output,
            "usage": responses_usage(&self.usage),
        });
        if status == "incomplete" {
            if let Some(object) = response.as_object_mut() {
                object.insert(
                    "incomplete_details".into(),
                    json!({ "reason": "max_output_tokens" }),
                );
            }
        }
        response
    }

    fn event(&mut self, name: &str, data: Value) -> Vec<u8> {
        self.seq += 1;
        let mut object = match data {
            Value::Object(object) => object,
            other => Map::from_iter([("data".to_owned(), other)]),
        };
        object.insert("type".into(), Value::String(name.to_owned()));
        object.insert("sequence_number".into(), json!(self.seq));
        sse(name, &Value::Object(object))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    Text,
    Tool,
}

struct ToolBlock {
    block: usize,
    id: String,
    name: String,
    started: bool,
}

/// Turns Chat completion chunks into Messages SSE events.
pub struct MessagesStream {
    message_id: String,
    model: String,
    usage: Value,
    next_index: usize,
    open: Option<(OpenBlock, usize)>,
    calls: BTreeMap<u64, ToolBlock>,
    tool_used: bool,
    finish: Option<String>,
    started: bool,
    done: bool,
}

impl MessagesStream {
    /// Start a stream for one client message.
    pub fn new(message_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            model: model.into(),
            usage: json!({}),
            next_index: 0,
            open: None,
            calls: BTreeMap::new(),
            tool_used: false,
            finish: None,
            started: false,
            done: false,
        }
    }

    /// Opening events; called automatically by `push_chunk`.
    pub fn start(&mut self) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let usage = messages_usage(&self.usage);
        messages_event(
            "message_start",
            json!({ "message": {
                "id": self.message_id, "type": "message", "role": "assistant",
                "model": self.model, "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": {
                    "input_tokens": usage["input_tokens"],
                    "cache_read_input_tokens": usage["cache_read_input_tokens"],
                    "output_tokens": 0,
                }
            }}),
        )
    }

    /// Feed one Chat SSE payload and get the Messages events it produces.
    pub fn push_chunk(&mut self, data: &str) -> Vec<u8> {
        let mut out = self.start();
        if self.done {
            return out;
        }
        let Some(chunk) = chunk(data) else {
            return out;
        };
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            self.usage = usage.clone();
        }
        if let Some(finish) = chunk
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            self.finish = Some(finish.to_owned());
        }
        let Some(delta) = chunk.pointer("/choices/0/delta") else {
            return out;
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            let index = match self.open {
                Some((OpenBlock::Text, index)) => index,
                _ => self.open_text(&mut out),
            };
            let event = messages_event(
                "content_block_delta",
                json!({ "index": index, "delta": { "type": "text_delta", "text": text } }),
            );
            out.extend(event);
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = call_index(call);
            let arguments = call_argument(call);
            {
                let block = self.calls.entry(index).or_insert_with(|| {
                    let block = self.next_index;
                    self.next_index += 1;
                    ToolBlock {
                        block,
                        id: String::new(),
                        name: String::new(),
                        started: false,
                    }
                });
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    block.id.push_str(id);
                }
                append_name(&mut block.name, call);
            }
            let ready = self.calls.get(&index).is_some_and(|block| {
                !block.started
                    && !block.name.is_empty()
                    && (call.get("id").is_some() || arguments.is_some())
            });
            if ready {
                self.open_tool(&mut out, index);
            }
            if let Some(arguments) = arguments {
                let block = self
                    .calls
                    .get(&index)
                    .filter(|block| block.started)
                    .map(|block| block.block);
                if let Some(block) = block {
                    let event = messages_event(
                        "content_block_delta",
                        json!({ "index": block,
                            "delta": { "type": "input_json_delta", "partial_json": arguments }
                        }),
                    );
                    out.extend(event);
                }
            }
        }
        out
    }

    /// Closing events: open blocks stop, then usage and `message_stop`.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        let mut out = self.start();
        self.done = true;
        let pending: Vec<u64> = self
            .calls
            .iter()
            .filter(|(_, block)| !block.started)
            .map(|(index, _)| *index)
            .collect();
        self.close_open(&mut out);
        for index in pending {
            self.open_tool(&mut out, index);
            self.close_open(&mut out);
        }
        self.close_open(&mut out);
        let stop = stop_reason(self.finish.as_deref(), self.tool_used);
        out.extend(messages_event(
            "message_delta",
            json!({ "delta": { "stop_reason": stop, "stop_sequence": null },
                "usage": messages_usage(&self.usage)
            }),
        ));
        out.extend(messages_event("message_stop", json!({})));
        out
    }

    fn open_index(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn open_text(&mut self, out: &mut Vec<u8>) -> usize {
        self.close_open(out);
        let index = self.open_index();
        out.extend(messages_event(
            "content_block_start",
            json!({ "index": index, "content_block": { "type": "text", "text": "" } }),
        ));
        self.open = Some((OpenBlock::Text, index));
        index
    }

    fn open_tool(&mut self, out: &mut Vec<u8>, index: u64) {
        let Some(block) = self.calls.get(&index) else {
            return;
        };
        if block.started {
            return;
        }
        let (block_index, id, name) = (block.block, block.id.clone(), block.name.clone());
        self.calls.get_mut(&index).expect("call state").started = true;
        self.tool_used = true;
        self.close_open(out);
        out.extend(messages_event(
            "content_block_start",
            json!({ "index": block_index, "content_block": {
                "type": "tool_use", "id": id, "name": name, "input": {}
            }}),
        ));
        self.open = Some((OpenBlock::Tool, block_index));
    }

    fn close_open(&mut self, out: &mut Vec<u8>) {
        if let Some((_, index)) = self.open.take() {
            out.extend(messages_event(
                "content_block_stop",
                json!({ "index": index }),
            ));
        }
    }
}

fn messages_event(name: &str, mut data: Value) -> Vec<u8> {
    if let Value::Object(object) = &mut data {
        object.insert("type".into(), Value::String(name.to_owned()));
    }
    sse(name, &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(out: &str, name: &str) -> usize {
        out.matches(&format!("event: {name}\n")).count()
    }

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn streams_responses_text() {
        let mut stream = ResponsesStream::new("resp_1", "provider/model", &json!({}));
        let mut out = stream.start();
        out.extend(stream.push_chunk(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#));
        out.extend(stream.push_chunk(r#"{"choices":[{"delta":{"content":"lo"}}]}"#));
        out.extend(stream.push_chunk(
            r#"{"choices":[{"delta":{"content":""},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7,
                "prompt_tokens_details":{"cached_tokens":3}}}"#,
        ));
        out.extend(stream.push_chunk("[DONE]"));
        out.extend(stream.finish());
        let out = text(out);

        assert_eq!(events(&out, "response.created"), 1);
        assert!(out.contains(r#""delta":"Hel""#));
        assert!(out.contains(r#""delta":"lo""#));
        assert!(out.contains("response.output_text.done"));
        assert!(out.contains(r#""text":"Hello""#));
        assert_eq!(events(&out, "response.content_part.done"), 1);
        assert_eq!(events(&out, "response.completed"), 1);
        assert!(out.contains(r#""cached_tokens":3"#));
        assert!(out.contains(r#""output_tokens":2"#));
        assert!(out.ends_with("data: [DONE]\n\n"));
        let created = out.find("response.created").unwrap();
        let delta = out.find("response.output_text.delta").unwrap();
        let completed = out.find("response.completed").unwrap();
        assert!(created < delta && delta < completed);
    }

    #[test]
    fn streams_responses_tool_arguments_in_pieces() {
        let mut stream = ResponsesStream::new("resp_2", "m", &json!({}));
        let mut out = stream.start();
        out.extend(stream.push_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function",
                "function":{"name":"read","arguments":""}}]}}]}"#,
        ));
        out.extend(stream.push_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,
                "function":{"arguments":"{\"pa"}}]}}]}"#,
        ));
        out.extend(stream.push_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,
                "function":{"arguments":"th\":\"a\"}"}}]}}]}"#,
        ));
        out.extend(stream.finish());
        let out = text(out);

        assert_eq!(events(&out, "response.output_item.added"), 2);
        assert_eq!(events(&out, "response.function_call_arguments.delta"), 2);
        assert_eq!(events(&out, "response.function_call_arguments.done"), 1);
        assert!(out.contains(r#""call_id":"call_1""#));
        assert!(out.contains(r#""id":"fc_"#));
        assert!(out.contains(r#""arguments":"{\"path\":\"a\"}""#));
    }

    #[test]
    fn streams_messages_text_with_usage() {
        let mut stream = MessagesStream::new("msg_1", "provider/model");
        let mut out = stream.start();
        out.extend(
            stream.push_chunk(r#"{"choices":[{"delta":{"role":"assistant","content":"Hi"}}]}"#),
        );
        out.extend(stream.push_chunk(
            r#"{"choices":[{"delta":{"content":" there"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10,"completion_tokens":4,
                "prompt_tokens_details":{"cached_tokens":6}}}"#,
        ));
        out.extend(stream.finish());
        let out = text(out);

        assert_eq!(events(&out, "message_start"), 1);
        assert_eq!(events(&out, "content_block_start"), 1);
        assert_eq!(events(&out, "content_block_delta"), 2);
        assert_eq!(events(&out, "content_block_stop"), 1);
        assert!(out.contains(r#""text":"Hi""#));
        assert!(out.contains(r#""text":" there""#));
        assert!(out.contains(r#""stop_reason":"end_turn""#));
        assert!(out.contains(r#""cache_read_input_tokens":6"#));
        assert!(out.contains(r#""input_tokens":4"#));
        assert!(out.contains(r#""output_tokens":4"#));
        assert!(out.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
    }

    #[test]
    fn streams_messages_tool_arguments_in_pieces() {
        let mut stream = MessagesStream::new("msg_2", "m");
        let mut out = stream.start();
        out.extend(stream.push_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function",
                "function":{"name":"read","arguments":""}}]}}]}"#,
        ));
        out.extend(stream.push_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,
                "function":{"arguments":"{\"pa"}}]}}]}"#,
        ));
        out.extend(stream.push_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,
                "function":{"arguments":"th\":\"a\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ));
        out.extend(stream.finish());
        let out = text(out);

        assert_eq!(events(&out, "content_block_start"), 1);
        assert!(out.contains(r#""name":"read""#));
        assert!(out.contains(r#""input":{}"#));
        assert!(out.contains(r#""id":"call_1""#));
        assert_eq!(out.matches(r#""partial_json":"#).count(), 2);
        assert_eq!(events(&out, "content_block_stop"), 1);
        assert!(out.contains(r#""stop_reason":"tool_use""#));
    }
}
