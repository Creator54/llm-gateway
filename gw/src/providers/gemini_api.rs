//! gemini-api adapter: direct HTTP to `generativelanguage.googleapis.com` with a Google API key.
//!
//! The pay-per-token Gemini API (v1beta), not the Google Cloud Code Assist backend that the
//! `antigravity` adapter fronts. Inbound protocols are normalized onto the chat-completions wire
//! by the shared translators, mapped onto the Gemini `generateContent` envelope, and rewritten
//! back per the client's protocol.

use bytes::Bytes;
use futures_util::{
    future::BoxFuture,
    stream::{self, BoxStream, StreamExt},
    FutureExt,
};
use http::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use http::StatusCode;
use serde_json::{json, Map, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::chat::{buffered_reply, chat_body, error_body, invalid, retry_after};
use crate::provider::{Account, Credential, Failure, Inbound, Provider, Reply, Request};
use crate::translate::{Error, MessagesStream, ResponsesStream, SseParser};

/// Gemini API v1beta root; the model name is appended to form the method path.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Models served by the pay-per-token Gemini API. No free tier: the key is billed per token.
pub const SUPPORTED_MODELS: &[&str] = &[
    "gemini-2.0-flash",
    "gemini-2.0-pro",
    "gemini-1.5-flash",
    "gemini-1.5-pro",
];

/// The upstream call: `{base}/{model}:{method}?key=…`, plus `?alt=sse` when streaming.
fn request_url(base: &str, model: &str, method: &str, stream: bool, key: &str) -> String {
    let query = if stream { "alt=sse&" } else { "" };
    format!("{base}/{model}:{method}?{query}key={key}")
}

pub struct GeminiApi {
    client: reqwest::Client,
    base: String,
}

impl GeminiApi {
    /// `base_url` overrides the canonical endpoint for tests and proxies.
    pub fn new(client: reqwest::Client, base_url: Option<String>) -> Self {
        let base = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    async fn turn(&self, req: &Request, account: &Account) -> Result<Reply, Failure> {
        let Credential::ApiKey(key) = &account.credential else {
            return Err(Failure::Auth);
        };
        if key.is_empty() {
            return Err(Failure::Auth);
        }
        let chat = chat_body(req)?;
        let body = to_gemini(&chat);
        let sent = body.clone();
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| invalid("request body is not serializable", req.inbound))?;

        let method = if req.stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let url = request_url(&self.base, &req.model, method, req.stream, key);
        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .header("x-goog-api-key", key)
            .body(bytes)
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(url = %url, %error, "gemini-api upstream request failed");
                Failure::Retryable
            })?;
        let status = response.status();
        if !status.is_success() {
            let failure = reject(status, response).await;
            if let Failure::Fatal { body: reply, .. } = &failure {
                tracing::warn!(
                    %status, %url, sent = %shape(&sent),
                    reply = %String::from_utf8_lossy(reply),
                    "gemini-api refused the request"
                );
            }
            return Err(failure);
        }

        if req.stream {
            let mut headers = reply_headers(response.headers());
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
            headers.insert(
                http::header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache"),
            );
            Ok(Reply {
                status,
                headers,
                body: gemini_stream(response, req),
            })
        } else {
            Ok(buffered_reply(req, response).await?)
        }
    }
}

impl Provider for GeminiApi {
    fn id(&self) -> &str {
        "gemini-api"
    }

    fn send<'a>(
        &'a self,
        req: &'a Request,
        account: &'a Account,
    ) -> BoxFuture<'a, Result<Reply, Failure>> {
        self.turn(req, account).boxed()
    }
}

/// The protocol sink the upstream frames are rewritten into, per the client's protocol.
enum Sink {
    Chat(ChatOut),
    Responses(ResponsesStream),
    Messages(MessagesStream),
}

impl Sink {
    fn new(req: &Request, model: &str) -> Self {
        match req.inbound {
            Inbound::Responses => Sink::Responses(ResponsesStream::new(
                format!("resp_{}", unique_suffix()),
                model,
                &serde_json::from_slice(&req.body).unwrap_or_default(),
            )),
            Inbound::Messages => Sink::Messages(MessagesStream::new(
                format!("msg_{}", unique_suffix()),
                model,
            )),
            Inbound::Chat => Sink::Chat(ChatOut::new(model)),
        }
    }

    fn push(&mut self, chunk: &Value) -> Vec<u8> {
        match self {
            Sink::Chat(out) => out.push(chunk),
            Sink::Responses(stream) => {
                stream.push_chunk(&serde_json::to_string(chunk).unwrap_or_else(|_| "{}".into()))
            }
            Sink::Messages(stream) => {
                stream.push_chunk(&serde_json::to_string(chunk).unwrap_or_else(|_| "{}".into()))
            }
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        match self {
            Sink::Chat(out) => out.finish(),
            Sink::Responses(stream) => stream.finish(),
            Sink::Messages(stream) => stream.finish(),
        }
    }
}

/// One Gemini SSE stream, reduced to the client's protocol as events arrive.
struct GeminiStreamState {
    source: BoxStream<'static, Result<Bytes, std::io::Error>>,
    parser: SseParser,
    sink: Sink,
    inbound: Inbound,
    model: String,
    finished: bool,
}

impl GeminiStreamState {
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        let events = match self.parser.push(chunk) {
            Ok(events) => events,
            Err(_) => {
                self.finished = true;
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for data in events {
            if self.finished {
                return out;
            }
            out.extend(self.frame(&data));
        }
        out
    }

    fn frame(&mut self, data: &str) -> Vec<u8> {
        let Ok(payload) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        if let Some(error) = payload.get("error") {
            self.finished = true;
            let message = error
                .pointer("/message")
                .and_then(Value::as_str)
                .unwrap_or("upstream error");
            let error = Error::bad_gateway(message);
            let body = serde_json::to_vec(&error.to_body(wire(self.inbound))).unwrap_or_default();
            let frame = match self.inbound {
                Inbound::Chat => format!("data: {}\n\n", String::from_utf8_lossy(&body)),
                Inbound::Responses | Inbound::Messages => {
                    format!("event: error\ndata: {}\n\n", String::from_utf8_lossy(&body))
                }
            };
            return frame.into_bytes();
        }
        let Some(chunk) = gemini_to_chat_chunk(&payload, &self.model) else {
            return Vec::new();
        };
        self.sink.push(&chunk)
    }

    fn flush(&mut self) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if let Ok(Some(data)) = self.parser.finish() {
            self.frame(&data);
        }
        self.sink.finish()
    }
}

fn gemini_stream(
    response: reqwest::Response,
    req: &Request,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    let source = response
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other))
        .boxed();
    let state = GeminiStreamState {
        source,
        parser: SseParser::new(),
        sink: Sink::new(req, &req.model),
        inbound: req.inbound,
        model: req.model.clone(),
        finished: false,
    };
    stream::unfold(state, |mut state| async move {
        loop {
            if state.finished {
                return None;
            }
            match state.source.next().await {
                Some(Ok(chunk)) => {
                    let out = state.feed(&chunk);
                    if state.finished {
                        let tail = state.flush();
                        if tail.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(tail)), state));
                    }
                    if out.is_empty() {
                        continue;
                    }
                    return Some((Ok(Bytes::from(out)), state));
                }
                Some(Err(error)) => {
                    state.finished = true;
                    let tail = state.flush();
                    if tail.is_empty() {
                        return None;
                    }
                    return Some((Err(std::io::Error::other(error)), state));
                }
                None => {
                    let tail = state.flush();
                    if tail.is_empty() {
                        return None;
                    }
                    return Some((Ok(Bytes::from(tail)), state));
                }
            }
        }
    })
    .boxed()
}

/// The chat-completions-shaped stream output for a Chat-protocol client.
struct ChatOut {
    id: String,
    created: u64,
    model: String,
    usage: Value,
    finish: Option<String>,
    started: bool,
    done: bool,
}

impl ChatOut {
    fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", unique_suffix()),
            created: now_secs(),
            model: model.to_owned(),
            usage: json!({}),
            finish: None,
            started: false,
            done: false,
        }
    }

    fn frame(&self, delta: Value, finish: Option<&str>, usage: Option<&Value>) -> Vec<u8> {
        let mut data = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        if let Some(usage) = usage {
            data["usage"] = usage.clone();
            let mut final_choice = Map::new();
            final_choice.insert("index".into(), json!(0));
            final_choice.insert("delta".into(), Value::Object(Map::new()));
            final_choice.insert(
                "finish_reason".into(),
                finish.map(Value::from).unwrap_or(Value::Null),
            );
            data["choices"] = json!([final_choice]);
        }
        let data = serde_json::to_string(&data).unwrap_or_else(|_| "{}".into());
        format!("data: {data}\n\n").into_bytes()
    }

    fn push(&mut self, chunk: &Value) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(choice) = chunk.pointer("/choices/0") {
            if !self.started {
                self.started = true;
                out.extend(self.frame(json!({ "role": "assistant", "content": "" }), None, None));
            }
            if let Some(delta) = choice.get("delta") {
                if let Some(content) = delta.get("content").and_then(Value::as_str) {
                    if !content.is_empty() {
                        out.extend(self.frame(json!({ "content": content }), None, None));
                    }
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    if !calls.is_empty() {
                        out.extend(self.frame(json!({ "tool_calls": calls.clone() }), None, None));
                    }
                }
            }
            if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish = Some(finish.to_owned());
            }
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            self.usage = usage.clone();
        }
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out = self.frame(json!({}), self.finish.as_deref(), Some(&self.usage));
        out.extend_from_slice(b"data: [DONE]\n\n");
        out
    }
}

fn wire(inbound: Inbound) -> crate::translate::Inbound {
    match inbound {
        Inbound::Chat => crate::translate::Inbound::Chat,
        Inbound::Responses => crate::translate::Inbound::Responses,
        Inbound::Messages => crate::translate::Inbound::Messages,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unique_suffix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64
}

fn gemini_finish(finish: &str) -> Option<&'static str> {
    match finish {
        "STOP" => Some("stop"),
        "MAX_TOKENS" => Some("length"),
        "FUNCTION_CALL" => Some("tool_calls"),
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
            Some("content_filter")
        }
        _ => Some("stop"),
    }
}

/// Map a Gemini response onto the chat-completions-shaped chunk the protocol sinks consume.
fn gemini_to_chat_chunk(payload: &Value, model: &str) -> Option<Value> {
    if payload.get("error").is_some() {
        return None;
    }
    let candidate = payload.pointer("/candidates/0")?;
    let parts = candidate.pointer("/content/parts")?.as_array()?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for part in parts {
        if let Some(value) = part.get("text").and_then(Value::as_str) {
            text.push_str(value);
        }
        if let Some(call) = part.get("functionCall") {
            let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .unwrap_or("call_0")
                .to_owned();
            calls.push(json!({
                "index": calls.len(),
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
                },
            }));
        }
    }
    let mut delta = Map::new();
    if !calls.is_empty() {
        delta.insert("tool_calls".into(), Value::Array(calls));
    } else if !text.is_empty() {
        delta.insert("content".into(), Value::String(text));
    }
    let finish = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .map(gemini_finish);
    let mut chunk = json!({
        "id": format!("chatcmpl-{}", unique_suffix()),
        "object": "chat.completion.chunk",
        "created": now_secs(),
        "model": model,
        "choices": [{ "index": 0, "delta": Value::Object(delta), "finish_reason": finish }],
    });
    if let Some(usage) = payload
        .pointer("/usageMetadata")
        .filter(|usage| usage.is_object())
    {
        chunk["usage"] = usage_from_gemini(usage);
    }
    Some(chunk)
}

/// Gemini `usageMetadata` onto the chat-completions usage object.
fn usage_from_gemini(usage: &Value) -> Value {
    let prompt = usage
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = usage
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("cachedContentTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let thoughts = usage
        .get("thoughtsTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
        "prompt_tokens_details": { "cached_tokens": cached },
        "completion_tokens_details": { "reasoning_tokens": thoughts },
    })
}

/// The chat-completions-shaped request body onto the Gemini `generateContent` envelope.
fn to_gemini(chat: &Value) -> Value {
    let mut body = Map::new();
    if let Some(system) = system_text(chat) {
        body.insert(
            "systemInstruction".into(),
            json!({ "parts": [{ "text": system }] }),
        );
    }
    let contents = chat_contents(chat);
    if !contents.as_array().is_none_or(|items| items.is_empty()) {
        body.insert("contents".into(), contents);
    }
    let mut config = Map::new();
    if let Some(max) = chat
        .get("max_tokens")
        .or_else(|| chat.get("max_completion_tokens"))
    {
        config.insert("maxOutputTokens".into(), max.clone());
    }
    if let Some(temperature) = chat.get("temperature") {
        config.insert("temperature".into(), temperature.clone());
    }
    if !config.is_empty() {
        body.insert("generationConfig".into(), Value::Object(config));
    }
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        if !tools.is_empty() {
            body.insert("tools".into(), gemini_tools(tools));
        }
    }
    Value::Object(body)
}

fn system_text(chat: &Value) -> Option<String> {
    let system = chat.get("system")?;
    let text = match system {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    (!text.is_empty()).then_some(text)
}

fn chat_contents(chat: &Value) -> Value {
    let messages = chat
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let contents = messages
        .iter()
        .filter_map(|message| {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let role = if role == "assistant" { "model" } else { "user" };
            let text = message_content(message);
            if text.is_empty() {
                return None;
            }
            Some(json!({ "role": role, "parts": [{ "text": text }] }))
        })
        .collect::<Vec<_>>();
    Value::Array(contents)
}

fn message_content(message: &Value) -> String {
    let content = message.get("content");
    let text = match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    text
}

fn gemini_tools(tools: &[Value]) -> Value {
    let declarations = tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(json!({
                "name": function.get("name").and_then(Value::as_str).unwrap_or(""),
                "description": function.get("description").and_then(Value::as_str).unwrap_or(""),
                "parameters": gemini_schema(
                    &function.get("parameters").cloned().unwrap_or_else(|| json!({
                        "type": "object",
                        "properties": {},
                    })),
                ),
            }))
        })
        .collect::<Vec<_>>();
    json!([{ "function_declarations": declarations }])
}

/// Gemini's function-declaration schema is the OpenAPI subset: no `additionalProperties` and no
/// bare `const` (a single-value `enum` instead).
fn gemini_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                match key.as_str() {
                    // Gemini has no `additionalProperties`; a plain property list suffices.
                    "additionalProperties" => {}
                    // Gemini expresses a fixed value as a single-element `enum`.
                    "const" => {
                        out.insert("enum".into(), json!([item.clone()]));
                    }
                    _ => {
                        out.insert(key.clone(), gemini_schema(item));
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(gemini_schema).collect()),
        other => other.clone(),
    }
}

/// Relay the headers worth passing on: content type and Google's own cache and rate-limit headers.
fn reply_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in upstream {
        let name = name.as_str();
        let relayed =
            name == "content-type" || name.starts_with("x-goog-") || name.starts_with("cache-");
        if let (true, Ok(name)) = (relayed, HeaderName::from_bytes(name.as_bytes())) {
            headers.append(name, value.clone());
        }
    }
    headers
}

/// 401 and 403 are auth failures; a 400 is the client's bad request; 429 is a rate limit; 5xx retries.
async fn reject(status: StatusCode, response: reqwest::Response) -> Failure {
    let wait = retry_after(response.headers());
    let body = error_body(response).await;
    failure_for(status, &body, wait)
}

fn failure_for(status: StatusCode, body: &[u8], wait: Option<Duration>) -> Failure {
    match status.as_u16() {
        401 | 403 => Failure::Auth,
        400 => Failure::Fatal {
            status,
            body: Bytes::from(body.to_vec()),
        },
        429 => Failure::RateLimited { retry_after: wait },
        code if (500..600).contains(&code) => Failure::Retryable,
        _ => Failure::Fatal {
            status,
            body: Bytes::from(body.to_vec()),
        },
    }
}

fn shape(body: &Value) -> String {
    let Some(object) = body.as_object() else {
        return String::new();
    };
    object
        .iter()
        .map(|(key, value)| match value {
            Value::Array(items) => format!("{key}[{}]", items.len()),
            Value::String(text) if key != "model" => format!("{key}=<{} chars>", text.len()),
            Value::String(_) | Value::Number(_) | Value::Bool(_) => format!("{key}={value}"),
            _ => key.clone(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(inbound: Inbound, stream: bool) -> Request {
        Request {
            inbound,
            model: "gemini-2.0-flash".to_owned(),
            headers: HeaderMap::new(),
            body: Bytes::from_static(br#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#),
            stream,
            session: "test-session".to_owned(),
            fast: None,
        }
    }

    fn sse(payload: &Value) -> Vec<u8> {
        format!("data: {}\n\n", serde_json::to_string(payload).unwrap()).into_bytes()
    }

    #[test]
    fn serves_only_paid_gemini_models() {
        assert!(SUPPORTED_MODELS.contains(&"gemini-2.0-flash"));
        assert!(SUPPORTED_MODELS.contains(&"gemini-2.0-pro"));
        assert!(SUPPORTED_MODELS.contains(&"gemini-1.5-pro"));
        assert!(!SUPPORTED_MODELS.contains(&"gemini-pro-free"));
    }

    #[test]
    fn builds_the_upstream_url_with_the_key() {
        assert_eq!(
            request_url(
                "https://generativelanguage.googleapis.com/v1beta/models",
                "gemini-2.0-flash",
                "generateContent",
                false,
                "k"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent?key=k"
        );
        assert_eq!(
            request_url(
                "https://generativelanguage.googleapis.com/v1beta/models",
                "gemini-2.0-flash",
                "streamGenerateContent",
                true,
                "k"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:streamGenerateContent?alt=sse&key=k"
        );
    }

    #[test]
    fn builds_the_gemini_envelope_from_chat() {
        let chat = json!({
            "model": "gemini-2.0-flash",
            "system": "be brief",
            "max_tokens": 512,
            "messages": [
                { "role": "user", "content": "hello" },
                { "role": "assistant", "content": "hi" },
                { "role": "user", "content": "again" },
            ],
        });
        let body = to_gemini(&chat);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["text"], "hi");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 512);
    }

    #[test]
    fn maps_tools_onto_function_declarations() {
        let chat = json!({
            "tools": [{ "type": "function", "function": { "name": "read", "description": "read", "parameters": { "type": "object" } } }],
        });
        let body = to_gemini(&chat);
        let declarations = body["tools"][0]["function_declarations"]
            .as_array()
            .unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0]["name"], "read");
    }

    #[test]
    fn strips_tool_keywords_gemini_does_not_accept() {
        let chat = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "read",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "additionalProperties": false },
                            "mode": { "type": "string", "const": "fast" },
                        },
                    },
                },
            }],
        });
        let body = to_gemini(&chat);
        let declaration = &body["tools"][0]["function_declarations"][0];
        let parameters = declaration["parameters"].as_object().unwrap();
        assert!(parameters.get("additionalProperties").is_none());
        assert!(parameters.get("properties").unwrap()["path"]
            .as_object()
            .unwrap()
            .get("additionalProperties")
            .is_none());
        assert_eq!(parameters["properties"]["mode"]["enum"], json!(["fast"]));
    }

    #[test]
    fn maps_gemini_usage_to_chat_usage() {
        let usage = json!({
            "promptTokenCount": 10,
            "candidatesTokenCount": 4,
            "cachedContentTokenCount": 6,
            "thoughtsTokenCount": 2,
        });
        let chat_usage = usage_from_gemini(&usage);
        assert_eq!(chat_usage["prompt_tokens"], 10);
        assert_eq!(chat_usage["completion_tokens"], 4);
        assert_eq!(chat_usage["total_tokens"], 14);
        assert_eq!(chat_usage["prompt_tokens_details"]["cached_tokens"], 6);
        assert_eq!(
            chat_usage["completion_tokens_details"]["reasoning_tokens"],
            2
        );
    }

    #[test]
    fn maps_gemini_text_to_chat_chunk() {
        let payload = json!({
            "candidates": [{ "content": { "parts": [{ "text": "Hello" }] }, "finishReason": "STOP" }],
            "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 2 },
        });
        let chunk = gemini_to_chat_chunk(&payload, "gemini-2.0-flash").expect("chunk");
        assert_eq!(chunk["choices"][0]["delta"]["content"], "Hello");
        assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
        assert_eq!(chunk["usage"]["prompt_tokens"], 3);
    }

    #[test]
    fn maps_gemini_usage_tolerates_missing_fields() {
        let chat_usage = usage_from_gemini(&json!({}));
        assert_eq!(chat_usage["prompt_tokens"], 0);
        assert_eq!(chat_usage["completion_tokens"], 0);
        assert_eq!(chat_usage["total_tokens"], 0);
        let malformed = usage_from_gemini(&json!({
            "promptTokenCount": "lots",
            "candidatesTokenCount": null,
        }));
        assert_eq!(malformed["prompt_tokens"], 0);
        assert_eq!(malformed["completion_tokens"], 0);
    }

    #[test]
    fn maps_gemini_tool_calls_to_chat_chunks() {
        let payload = json!({
            "candidates": [{ "content": { "parts": [{ "functionCall": { "name": "read", "args": { "path": "a" } } }] }, "finishReason": "FUNCTION_CALL" }],
        });
        let chunk = gemini_to_chat_chunk(&payload, "gemini-2.0-flash").expect("chunk");
        let calls = chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls[0]["function"]["name"], "read");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a\"}");
        assert_eq!(chunk["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn classifies_gemini_failures() {
        assert_eq!(
            failure_for(StatusCode::UNAUTHORIZED, b"no", None),
            Failure::Auth
        );
        assert_eq!(
            failure_for(StatusCode::FORBIDDEN, b"no", None),
            Failure::Auth
        );
        assert_eq!(
            failure_for(StatusCode::BAD_REQUEST, b"bad", None),
            Failure::Fatal {
                status: StatusCode::BAD_REQUEST,
                body: Bytes::from_static(b"bad")
            }
        );
        assert_eq!(
            failure_for(
                StatusCode::TOO_MANY_REQUESTS,
                b"slow down",
                Some(Duration::from_secs(2))
            ),
            Failure::RateLimited {
                retry_after: Some(Duration::from_secs(2))
            }
        );
        assert_eq!(
            failure_for(StatusCode::INTERNAL_SERVER_ERROR, b"boom", None),
            Failure::Retryable
        );
    }

    #[test]
    fn relays_google_cache_headers_only() {
        let mut upstream = HeaderMap::new();
        upstream.insert(
            "x-goog-cache-allowed-headers",
            HeaderValue::from_static("*"),
        );
        upstream.insert("content-type", HeaderValue::from_static("application/json"));
        upstream.insert("x-custom", HeaderValue::from_static("nope"));
        let relayed = reply_headers(&upstream);
        assert!(relayed.contains_key("x-goog-cache-allowed-headers"));
        assert!(relayed.contains_key("content-type"));
        assert!(!relayed.contains_key("x-custom"));
    }

    fn chat_state() -> GeminiStreamState {
        let req = request(Inbound::Chat, true);
        GeminiStreamState {
            source: Box::pin(futures_util::stream::empty::<Result<Bytes, std::io::Error>>()),
            parser: SseParser::new(),
            sink: Sink::new(&req, "gemini-2.0-flash"),
            inbound: Inbound::Chat,
            model: "gemini-2.0-flash".to_owned(),
            finished: false,
        }
    }

    #[tokio::test]
    async fn streams_gemini_text_through_the_chat_sink() {
        let mut state = chat_state();
        let chunk = json!({
            "candidates": [{ "content": { "parts": [{ "text": "Hello" }] }, "finishReason": "STOP" }],
            "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 2, "cachedContentTokenCount": 1 },
        });
        let out = state.feed(&sse(&chunk));
        assert!(String::from_utf8_lossy(&out).contains("\"content\":\"Hello\""));
        let tail = state.flush();
        let text = String::from_utf8_lossy(&tail).to_string();
        assert!(text.contains(r#""cached_tokens":1"#));
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn streams_gemini_text_across_chunks_without_loss() {
        let mut state = chat_state();
        let first = json!({
            "candidates": [{ "content": { "parts": [{ "text": "Hello" }] }, "finishReason": null }],
        });
        let second = json!({
            "candidates": [{ "content": { "parts": [{ "text": " world" }] }, "finishReason": "STOP" }],
        });
        let out = state.feed(&sse(&first));
        assert!(String::from_utf8_lossy(&out).contains("\"content\":\"Hello\""));
        let out = state.feed(&sse(&second));
        assert!(String::from_utf8_lossy(&out).contains("\"content\":\" world\""));
        let tail = state.flush();
        let text = String::from_utf8_lossy(&tail).to_string();
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn streams_gemini_error_events() {
        let mut state = chat_state();
        let out = state.feed(b"data: {\"error\":{\"message\":\"bad input\"}}\n\n");
        assert!(!out.is_empty());
        assert!(state.finished);
    }
}
