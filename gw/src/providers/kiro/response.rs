//! Kiro eventstream payloads to Chat completion chunks.

use super::eventstream::Message;
use super::payload::CompletionMode;
use super::wire::{self, ToolNames};
use serde_json::{json, Map, Value};

const KNOWN_EVENTS: &[&str] = &[
    "assistantResponseEvent",
    "reasoningContentEvent",
    "toolUseEvent",
    "messageMetadataEvent",
    "initial-response",
    "metadataEvent",
    "meteringEvent",
    "contextUsageEvent",
    "invalidStateEvent",
    "error",
];

/// One decoded upstream event.
pub(super) enum Event {
    Content(String),
    Reasoning(String),
    Tool {
        name: Option<String>,
        id: Option<String>,
        input: Option<String>,
        stop: Option<bool>,
    },
    /// `metadataEvent` carries usage and/or the native stop reason.
    Metadata {
        usage: Option<Usage>,
        stop: Option<String>,
    },
    Failure(String),
}

#[derive(Debug, Clone, Default)]
pub(super) struct Usage {
    pub input: u64,
    pub cached: Option<u64>,
    pub output: u64,
    pub total: u64,
}

/// Parse a frame's payload. Unknown event types yield `None` without parsing.
pub(super) fn parse(message: &Message) -> Result<Option<Event>, String> {
    let kind = message.header(":message-type").unwrap_or("");
    if kind == "exception" || kind == "error" {
        let reason = message
            .header(":exception-type")
            .or_else(|| message.header(":error-type"))
            .unwrap_or("InvalidState");
        return Ok(Some(Event::Failure(format!(
            "{reason}: {}",
            message.payload_str()
        ))));
    }
    if kind != "event" {
        return Err(format!("unsupported Smithy message type {kind:?}"));
    }
    let event_type = message
        .header(":event-type")
        .ok_or_else(|| "event is missing :event-type".to_owned())?;
    if !KNOWN_EVENTS.contains(&event_type) {
        return Ok(None);
    }
    let text = message.payload_str();
    let object: Value = serde_json::from_str(&text)
        .map_err(|_| format!("invalid Kiro {event_type} payload: expected valid JSON"))?;
    if !object.is_object() {
        return Err(format!(
            "invalid Kiro {event_type} payload: expected an object"
        ));
    }
    let field = |name: &str| -> Result<Option<String>, String> {
        match object.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(format!(
                "invalid Kiro {event_type} payload: {name} must be a string"
            )),
        }
    };
    let number = |name: &str| -> Result<Option<f64>, String> {
        match object.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value.as_f64().map(Some).ok_or_else(|| {
                format!("invalid Kiro {event_type} payload: {name} must be a number")
            }),
        }
    };
    Ok(Some(match event_type {
        "assistantResponseEvent" => Event::Content(field("content")?.unwrap_or_default()),
        "reasoningContentEvent" => Event::Reasoning(field("text")?.unwrap_or_default()),
        "toolUseEvent" => {
            let stop = match object.get("stop") {
                None | Some(Value::Null) => None,
                Some(Value::Bool(value)) => Some(*value),
                Some(_) => {
                    return Err(format!(
                        "invalid Kiro {event_type} payload: stop must be a boolean"
                    ))
                }
            };
            Event::Tool {
                name: field("name")?,
                id: field("toolUseId")?,
                input: field("input")?,
                stop,
            }
        }
        "metadataEvent" => {
            let stop = field("stopReason")?;
            let usage = token_usage(&object)?;
            if stop.is_none() && usage.is_none() {
                number("contextUsagePercentage")?;
                return Ok(None);
            }
            Event::Metadata { usage, stop }
        }
        "meteringEvent" => {
            field("unit")?.ok_or_else(|| {
                "invalid Kiro meteringEvent payload: unit must be a string".to_owned()
            })?;
            return Ok(None);
        }
        "messageMetadataEvent" | "initial-response" => {
            field("conversationId")?.or(field("utteranceId")?);
            return Ok(None);
        }
        "contextUsageEvent" => {
            number("contextUsagePercentage")?;
            return Ok(None);
        }
        "invalidStateEvent" => Event::Failure(
            field("message")?.unwrap_or_else(|| "Kiro entered an invalid state".to_owned()),
        ),
        "error" => {
            let reason = field("reason")?
                .or(field("type")?)
                .or(field("__type")?)
                .unwrap_or_default();
            let message = field("message")?
                .or(field("Message")?)
                .unwrap_or_else(|| "Kiro request failed".to_owned());
            Event::Failure(format!("{reason}: {message}"))
        }
        _ => return Ok(None),
    }))
}

fn token_usage(object: &Value) -> Result<Option<Usage>, String> {
    let Some(usage) = object.get("tokenUsage") else {
        return Ok(None);
    };
    if !usage.is_object() {
        return Err("invalid Kiro metadataEvent payload: tokenUsage must be an object".to_owned());
    }
    let count = |name: &str| -> Result<u64, String> {
        match usage.get(name) {
            None => Ok(0),
            Some(value) => value.as_u64().ok_or_else(|| {
                format!("invalid Kiro metadataEvent payload: {name} must be a non-negative integer")
            }),
        }
    };
    let optional = |name: &str| -> Result<Option<u64>, String> {
        match usage.get(name) {
            None => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or_else(|| {
                format!("invalid Kiro metadataEvent payload: {name} must be a non-negative integer")
            }),
        }
    };
    let uncached = count("uncachedInputTokens")?;
    let cached = optional("cacheReadInputTokens")?;
    let written = optional("cacheWriteInputTokens")?;
    Ok(Some(Usage {
        input: uncached + cached.unwrap_or(0) + written.unwrap_or(0),
        cached,
        output: count("outputTokens")?,
        total: count("totalTokens")?,
    }))
}

/// Builds Chat chunks from events, for both streaming and collected replies.
pub(super) struct Translator {
    model: String,
    names: ToolNames,
    /// Text stays staged until the turn is known to have delivered its answer.
    defer: bool,
    /// Whether a turn that only talked may be retried once to demand the answer.
    may_retry: bool,
    staged: Vec<Value>,
    text: String,
    calls: Vec<Value>,
    open: Option<(String, String, String)>,
    usage: Option<Usage>,
    /// Estimated prompt tokens, for turns where Kiro reports no `tokenUsage` (the usual case).
    input_estimate: u64,
    stop_reason: Option<String>,
    next_index: u64,
    answered: bool,
    saw_real_tool: bool,
    needs_fallback: bool,
    finished: bool,
}

/// One accumulated tool call.
fn tool_call(index: u64, id: &str, name: &str, arguments: &str) -> Value {
    json!({ "index": index, "id": id, "type": "function",
        "function": { "name": name, "arguments": arguments } })
}

impl Translator {
    pub(super) fn new(
        model: &str,
        names: ToolNames,
        mode: CompletionMode,
        input_estimate: u64,
    ) -> Self {
        Self {
            model: model.to_owned(),
            names,
            // A completion answer supersedes the text staged before it, on either attempt.
            defer: mode != CompletionMode::Disabled,
            may_retry: mode == CompletionMode::Required,
            staged: Vec::new(),
            text: String::new(),
            calls: Vec::new(),
            open: None,
            usage: None,
            input_estimate,
            stop_reason: None,
            next_index: 0,
            answered: false,
            saw_real_tool: false,
            needs_fallback: false,
            finished: false,
        }
    }

    /// Chat chunks produced by one event.
    pub(super) fn push(&mut self, event: Event) -> Result<Vec<Value>, String> {
        match event {
            Event::Content(text) => {
                if text.is_empty() {
                    return Ok(Vec::new());
                }
                self.text.push_str(&text);
                let chunk = self.chunk(json!({ "content": text }));
                if self.defer {
                    self.staged.push(chunk);
                    return Ok(Vec::new());
                }
                Ok(vec![chunk])
            }
            Event::Reasoning(text) => {
                if text.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(vec![self.chunk(json!({ "reasoning_content": text }))])
            }
            Event::Tool {
                name,
                id,
                input,
                stop,
            } => self.tool_event(name, id, input, stop),
            Event::Metadata { usage, stop } => {
                if let Some(usage) = usage {
                    self.usage = Some(usage);
                }
                if let Some(stop) = stop {
                    self.stop_reason = Some(stop);
                }
                Ok(Vec::new())
            }
            Event::Failure(message) => Err(message),
        }
    }

    fn tool_event(
        &mut self,
        name: Option<String>,
        id: Option<String>,
        input: Option<String>,
        stop: Option<bool>,
    ) -> Result<Vec<Value>, String> {
        match &mut self.open {
            None => {
                if stop == Some(true) {
                    return Err("tool stop received without an open tool call".to_owned());
                }
                let id = id.ok_or_else(|| "new tool event is missing toolUseId".to_owned())?;
                let name = name.ok_or_else(|| "new tool event is missing name".to_owned())?;
                let mut open = (id, self.names.restore(&name), String::new());
                if let Some(input) = input {
                    open.2.push_str(&input);
                }
                self.open = Some(open);
                Ok(Vec::new())
            }
            Some(open) => {
                if id.as_deref().is_some_and(|id| id != open.0)
                    || name
                        .as_deref()
                        .is_some_and(|name| self.names.restore(name) != open.1)
                {
                    return Err("tool input changed identity before stop".to_owned());
                }
                if let Some(input) = input {
                    open.2.push_str(&input);
                }
                if stop == Some(true) {
                    let (id, name, arguments) = self.open.take().expect("open call");
                    return Ok(self.close_call(id, name, arguments));
                }
                Ok(Vec::new())
            }
        }
    }

    fn close_call(&mut self, id: String, name: String, arguments: String) -> Vec<Value> {
        if name == wire::COMPLETION_TOOL {
            // The completion call carries the final answer and supersedes staged progress text.
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&arguments) {
                if let Some(answer) = map.get("answer").and_then(Value::as_str) {
                    if !answer.is_empty() {
                        self.text = answer.to_owned();
                        self.answered = true;
                        self.staged.clear();
                        return vec![self.chunk(json!({ "content": answer }))];
                    }
                }
            }
            return Vec::new();
        }
        self.saw_real_tool = true;
        let index = self.next_index;
        self.next_index += 1;
        self.calls.push(tool_call(index, &id, &name, &arguments));
        vec![self.chunk(json!({ "tool_calls": [tool_call(index, &id, &name, &arguments)] }))]
    }

    /// Whether this turn still owes a bounded `text_fallback` retry.
    pub(super) fn fallback_text(&self) -> Option<String> {
        self.needs_fallback.then(|| self.text.clone())
    }

    /// Whether the turn delivered its answer or did real work. A retry that only talks again
    /// usually asks what the task is, so the first attempt's text is the better answer.
    pub(super) fn delivered(&self) -> bool {
        self.answered || self.saw_real_tool
    }

    /// Release staged progress text as the final answer when no retry is possible.
    pub(super) fn release(&mut self) -> Vec<Value> {
        self.needs_fallback = false;
        if self.finished {
            return Vec::new();
        }
        let mut chunks = std::mem::take(&mut self.staged);
        chunks.extend(self.final_chunk());
        chunks
    }

    /// Final chunk: the stop reason plus usage, or nothing when already finished.
    pub(super) fn finish(&mut self) -> Vec<Value> {
        if self.finished {
            return Vec::new();
        }
        if self.may_retry && !self.answered && !self.saw_real_tool && !self.text.trim().is_empty() {
            let retry_stop = matches!(
                self.stop_reason.as_deref(),
                None | Some("END_TURN") | Some("STOP_SEQUENCE")
            );
            if !retry_stop {
                let mut chunks = std::mem::take(&mut self.staged);
                chunks.extend(self.final_chunk());
                return chunks;
            }
            self.needs_fallback = true;
            return Vec::new();
        }
        let mut chunks = std::mem::take(&mut self.staged);
        chunks.extend(self.final_chunk());
        chunks
    }

    fn final_chunk(&mut self) -> Vec<Value> {
        self.finished = true;
        let mut chunk = self.chunk(json!({}));
        let finish = if self.calls.is_empty() {
            match self.stop_reason.as_deref() {
                Some("TOOL_USE") => "tool_calls",
                Some(reason) if reason.contains("MAX_TOKENS") || reason.contains("LENGTH") => {
                    "length"
                }
                _ => "stop",
            }
        } else {
            "tool_calls"
        };
        if let Some(choice) = chunk
            .get_mut("choices")
            .and_then(Value::as_array_mut)
            .and_then(|choices| choices.first_mut())
        {
            choice["finish_reason"] = json!(finish);
        }
        Object::insert_usage(&mut chunk, &self.usage());
        vec![chunk]
    }

    /// The whole reply as one Chat completion object.
    pub(super) fn completion(&self) -> Value {
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        message.insert("content".into(), json!(self.text));
        if !self.calls.is_empty() {
            let calls: Vec<Value> = self
                .calls
                .iter()
                .map(|call| {
                    json!({ "id": call["id"], "type": "function", "function": call["function"] })
                })
                .collect();
            message.insert("tool_calls".into(), Value::Array(calls));
        }
        let finish = if self.calls.is_empty() {
            match self.stop_reason.as_deref() {
                Some("TOOL_USE") => "tool_calls",
                Some(reason) if reason.contains("MAX_TOKENS") || reason.contains("LENGTH") => {
                    "length"
                }
                _ => "stop",
            }
        } else {
            "tool_calls"
        };
        let mut completion = json!({
            "id": "chatcmpl-kiro",
            "object": "chat.completion",
            "created": super::now(),
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": finish,
            }],
        });
        Object::insert_usage(&mut completion, &self.usage());
        completion
    }

    /// Kiro's own counts, else an estimate from the prompt and the streamed text and arguments.
    fn usage(&self) -> Usage {
        if let Some(usage) = &self.usage {
            return usage.clone();
        }
        let arguments: String = self
            .calls
            .iter()
            .filter_map(|call| call.pointer("/function/arguments").and_then(Value::as_str))
            .collect();
        let output =
            crate::tokens::text_tokens(&self.text) + crate::tokens::text_tokens(&arguments);
        Usage {
            input: self.input_estimate,
            cached: None,
            output,
            total: self.input_estimate + output,
        }
    }

    fn chunk(&self, delta: Value) -> Value {
        json!({
            "id": "chatcmpl-kiro",
            "object": "chat.completion.chunk",
            "created": super::now(),
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": null }],
        })
    }
}

/// Usage shape shared by chunks and whole completions.
struct Object;

impl Object {
    fn insert_usage(target: &mut Value, usage: &Usage) {
        let mut object = Map::new();
        object.insert("prompt_tokens".into(), json!(usage.input));
        object.insert("completion_tokens".into(), json!(usage.output));
        object.insert("total_tokens".into(), json!(usage.total));
        if let Some(cached) = usage.cached {
            object.insert(
                "prompt_tokens_details".into(),
                json!({ "cached_tokens": cached }),
            );
        }
        target["usage"] = Value::Object(object);
    }
}
