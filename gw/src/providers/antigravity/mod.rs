//! Antigravity (Google Cloud Code Assist) upstream adapter.
//!
//! Client protocols arrive in Chat shape, leave as the CCA Gemini envelope, and come
//! back translated frame by frame.

mod request;
mod response;
mod util;

#[cfg(test)]
mod tests;

use crate::provider::{Account, Credential, Failure, Inbound, Provider, Reply, Request};
use crate::translate::{
    chat_to_messages, chat_to_responses, messages_to_chat, responses_to_chat, MessagesStream,
    ResponsesStream, SseParser,
};
use bytes::Bytes;
use futures_util::{future::BoxFuture, stream, stream::BoxStream, StreamExt};
use http::{header, HeaderMap, HeaderValue, StatusCode};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

const DEFAULT_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
const USER_AGENT: &str =
    "antigravity/ide/2.5.5 (os_type=windows; arch=amd64; aidev_client; auth_method=oauth)";
const ERROR_BODY_CAP: usize = 8 * 1024;
const REPLY_BODY_CAP: usize = 100 * 1024 * 1024;

pub struct Antigravity {
    client: reqwest::Client,
    base_url: String,
}

impl Antigravity {
    pub fn new(client: reqwest::Client, base_url: Option<String>) -> Self {
        let base_url = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    async fn forward(&self, req: &Request, account: &Account) -> Result<Reply, Failure> {
        let token = match &account.credential {
            Credential::OAuth { access, .. } => access.clone(),
            Credential::ApiKey(key) => key.clone(),
            Credential::None => return Err(Failure::Auth),
        };
        let Some(project) = account
            .extra
            .get("project")
            .filter(|project| !project.is_empty())
        else {
            return Err(Fatal::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "antigravity account has no project id",
            )
            .into());
        };
        let body: Value = serde_json::from_slice(&req.body)
            .map_err(|_| Fatal::new(StatusCode::BAD_REQUEST, "request body is not JSON"))?;
        let chat = match req.inbound {
            Inbound::Chat => body.clone(),
            Inbound::Responses => {
                responses_to_chat(&body, &req.model).map_err(|error| translated(&error))?
            }
            Inbound::Messages => {
                messages_to_chat(&body, &req.model).map_err(|error| translated(&error))?
            }
        };

        let anchor = util::session_anchor(&req.headers, &chat);
        let session = util::session_id(anchor.as_deref());
        let request_id = format!("agent-{}", util::uuid4());
        let mut prepared = request::build(&chat, &req.model, project, &session, &request_id);
        if let Some(contents) = prepared
            .body
            .pointer_mut("/request/contents")
            .and_then(Value::as_array_mut)
        {
            let claude = req.model.to_ascii_lowercase().contains("claude");
            response::apply_replay(contents, &req.model, &session, claude, &HashMap::new());
        }

        let method = if req.stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let query = if req.stream { "?alt=sse" } else { "" };
        let url = format!("{}/v1internal:{method}{query}", self.base_url);
        let payload = serde_json::to_vec(&prepared.body)
            .map_err(|_| Fatal::new(StatusCode::BAD_REQUEST, "request body is not serializable"))?;
        let sent = self
            .client
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(payload)
            .send()
            .await
            .map_err(|_| Failure::Retryable)?;
        let status = sent.status();
        if !status.is_success() {
            return Err(failure(status, sent).await);
        }
        if req.stream {
            Ok(streamed(sent, req, prepared, session))
        } else {
            whole(sent, req, &prepared, &session).await
        }
    }
}

impl Provider for Antigravity {
    fn id(&self) -> &str {
        "antigravity"
    }

    fn send<'a>(
        &'a self,
        req: &'a Request,
        account: &'a Account,
    ) -> BoxFuture<'a, Result<Reply, Failure>> {
        Box::pin(self.forward(req, account))
    }
}

struct Fatal {
    status: StatusCode,
    message: String,
}

impl Fatal {
    fn new(status: StatusCode, message: &str) -> Self {
        Self {
            status,
            message: message.to_owned(),
        }
    }
}

impl From<Fatal> for Failure {
    fn from(fatal: Fatal) -> Self {
        Failure::Fatal {
            status: fatal.status,
            body: Bytes::from(
                json!({ "error": { "message": fatal.message, "type": "invalid_request_error" } })
                    .to_string(),
            ),
        }
    }
}

/// Translation refused the client request; keep its status and message.
fn translated(error: &crate::translate::Error) -> Failure {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST);
    Failure::Fatal {
        status,
        body: Bytes::from(
            json!({ "error": { "message": error.message, "type": "invalid_request_error" } })
                .to_string(),
        ),
    }
}

async fn whole(
    response: reqwest::Response,
    req: &Request,
    prepared: &request::Prepared,
    session: &str,
) -> Result<Reply, Failure> {
    let body = read_bounded(response, REPLY_BODY_CAP).await;
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|_| Fatal::new(StatusCode::BAD_GATEWAY, "upstream body is not JSON"))?;
    let mut turn = response::Turn::default();
    let delta =
        response::translate(&payload, &prepared.names).map_err(|message| Failure::Fatal {
            status: StatusCode::BAD_GATEWAY,
            body: Bytes::from(json!({ "error": { "message": message } }).to_string()),
        })?;
    if !delta.signatures.is_empty() {
        response::remember(&req.model, session, &delta.signatures);
    }
    turn.usage = response::usage_of(&payload);
    turn.apply(delta);
    if turn.finish.as_deref() == Some("MALFORMED_FUNCTION_CALL") && turn.tool_calls() > 0 {
        return Err(Fatal::new(StatusCode::BAD_GATEWAY, response::TRUNCATED).into());
    }
    let chat = turn.completion();
    let translated = match req.inbound {
        Inbound::Chat => chat,
        Inbound::Responses => chat_to_responses(
            &chat,
            &req.model,
            serde_json::from_slice::<Value>(&req.body)
                .ok()
                .and_then(|body| body.get("parallel_tool_calls").and_then(Value::as_bool)),
        )
        .map_err(|error| translated(&error))?,
        Inbound::Messages => {
            chat_to_messages(&chat, &req.model).map_err(|error| translated(&error))?
        }
    };
    Ok(Reply {
        status: StatusCode::OK,
        headers: json_headers(),
        body: once(translated),
    })
}

fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers
}

fn once(value: Value) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    let mut bytes = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    stream::once(async move { Ok(Bytes::from(bytes)) }).boxed()
}

enum Outbound {
    Chat { id: String, created: u64 },
    Responses(ResponsesStream),
    Messages(MessagesStream),
}

struct Streaming {
    source: BoxStream<'static, Result<Bytes, std::io::Error>>,
    parser: SseParser,
    out: Outbound,
    names: util::ToolNames,
    model: String,
    session: String,
    usage: Option<Value>,
    finish: Option<String>,
    tool_calls: u64,
    observed: Vec<(String, Value, String)>,
    pending: VecDeque<Result<Bytes, std::io::Error>>,
    done_input: bool,
    closed: bool,
}

fn streamed(
    response: reqwest::Response,
    req: &Request,
    prepared: request::Prepared,
    session: String,
) -> Reply {
    let out = match req.inbound {
        Inbound::Chat => Outbound::Chat {
            id: format!("chatcmpl-{}", util::uuid4()),
            created: util::now_seconds(),
        },
        Inbound::Responses => Outbound::Responses(ResponsesStream::new(
            format!("resp_{}", &util::uuid4()[..8]),
            &req.model,
        )),
        Inbound::Messages => Outbound::Messages(MessagesStream::new(
            format!("msg_{}", &util::uuid4()[..8]),
            &req.model,
        )),
    };
    let state = Streaming {
        source: response
            .bytes_stream()
            .map(|item| item.map_err(std::io::Error::other))
            .boxed(),
        parser: SseParser::new(),
        out,
        names: prepared.names,
        model: req.model.clone(),
        session,
        usage: None,
        finish: None,
        tool_calls: 0,
        observed: Vec::new(),
        pending: VecDeque::new(),
        done_input: false,
        closed: false,
    };
    // Drain the first frame eagerly so a broken wrapper fails before the reply starts.
    let body = stream::unfold(state, |mut state| async move {
        loop {
            if let Some(bytes) = state.pending.pop_front() {
                return Some((bytes, state));
            }
            if state.closed {
                return None;
            }
            match state.source.next().await {
                Some(Ok(chunk)) => {
                    state.feed(&chunk);
                    if state.done_input {
                        state.shutdown();
                    }
                }
                Some(Err(_)) | None => state.shutdown(),
            }
        }
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Reply {
        status: StatusCode::OK,
        headers,
        body: body.boxed(),
    }
}

impl Streaming {
    fn feed(&mut self, chunk: &[u8]) {
        let events = match self.parser.push(chunk) {
            Ok(events) => events,
            Err(_) => {
                self.done_input = true;
                return;
            }
        };
        for data in events {
            if self.done_input {
                return;
            }
            self.frame(&data);
        }
    }

    fn frame(&mut self, data: &str) {
        let Ok(payload) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if let Some(usage) = response::usage_of(&payload) {
            self.usage = Some(usage);
        }
        let delta = match response::translate(&payload, &self.names) {
            Ok(delta) => delta,
            Err(_) => {
                // A protocol break ends the turn; the stream closes without a done event.
                self.done_input = true;
                return;
            }
        };
        if let Some(finish) = &delta.finish {
            self.finish = Some(finish.clone());
        }
        self.observed.extend(delta.signatures.iter().cloned());
        if !delta.text.is_empty() {
            self.emit(self.chunk(&delta.text, &[]));
        }
        self.tool_calls += delta.calls.len() as u64;
        if !delta.calls.is_empty() {
            self.emit(self.chunk("", &delta.calls));
        }
    }

    fn chunk(&self, text: &str, calls: &[(String, Value)]) -> Value {
        let mut delta = serde_json::Map::new();
        if !text.is_empty() {
            delta.insert("content".into(), json!(text));
        }
        if !calls.is_empty() {
            let calls: Vec<Value> = calls
                .iter()
                .map(|(name, args)| {
                    json!({
                        "index": 0,
                        "id": format!("call_{}", &util::uuid4()[..8]),
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(args).unwrap_or_else(|_| "{}".into()),
                        }
                    })
                })
                .collect();
            delta.insert("tool_calls".into(), Value::Array(calls));
        }
        self.wrap(Value::Object(delta), None, None)
    }

    fn wrap(&self, delta: Value, finish: Option<&str>, usage: Option<&Value>) -> Value {
        let (id, created) = match &self.out {
            Outbound::Chat { id, created } => (id.clone(), *created),
            _ => (String::new(), 0),
        };
        let mut chunk = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish.map(Value::from).unwrap_or(Value::Null),
            }],
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage.clone();
        }
        chunk
    }

    fn emit(&mut self, chunk: Value) {
        let bytes = match &mut self.out {
            Outbound::Chat { .. } => {
                let data = serde_json::to_string(&chunk).unwrap_or_else(|_| "{}".into());
                Bytes::from(format!("data: {data}\n\n"))
            }
            Outbound::Responses(stream) => {
                let data = serde_json::to_string(&chunk).unwrap_or_else(|_| "{}".into());
                Bytes::from(stream.push_chunk(&data))
            }
            Outbound::Messages(stream) => {
                let data = serde_json::to_string(&chunk).unwrap_or_else(|_| "{}".into());
                Bytes::from(stream.push_chunk(&data))
            }
        };
        if !bytes.is_empty() {
            self.pending.push_back(Ok(bytes));
        }
    }

    fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        if !self.done_input {
            if let Ok(Some(data)) = self.parser.finish() {
                self.frame(&data);
            }
        }
        self.closed = true;
        if !self.observed.is_empty() {
            response::remember(&self.model, &self.session, &self.observed);
        }
        // A turn cut off mid tool call is not a completed turn: end the stream without a
        // terminal event rather than report success for a partial call.
        if self.tool_calls > 0
            && matches!(
                self.finish.as_deref(),
                Some("MAX_TOKENS" | "MALFORMED_FUNCTION_CALL")
            )
        {
            return;
        }
        let usage = self.usage.as_ref().map(response::usage_of_chat);
        let finish = response::chat_finish(self.finish.as_deref(), self.tool_calls > 0);
        if matches!(self.out, Outbound::Chat { .. }) {
            self.emit(self.wrap(json!({}), Some(finish), usage.as_ref()));
            self.pending
                .push_back(Ok(Bytes::from_static(b"data: [DONE]\n\n")));
            return;
        }
        self.emit(self.wrap(json!({}), Some(finish), usage.as_ref()));
        let tail = match &mut self.out {
            Outbound::Responses(stream) => stream.finish(),
            Outbound::Messages(stream) => stream.finish(),
            Outbound::Chat { .. } => Vec::new(),
        };
        if !tail.is_empty() {
            self.pending.push_back(Ok(Bytes::from(tail)));
        }
    }
}

async fn read_bounded(mut response: reqwest::Response, cap: usize) -> Bytes {
    let mut buffer = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if buffer.len() + chunk.len() > cap {
            break;
        }
        buffer.extend_from_slice(&chunk);
    }
    Bytes::from(buffer)
}

/// `Retry-After` delta-seconds only; an HTTP-date is not parsed.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

const QUOTA_NEEDLES: [&str; 10] = [
    "quotafailure",
    "quota exceeded",
    "exceeded your current quota",
    "billing",
    "individual quota reached",
    "quota reached",
    "enable overages",
    "exhausted your capacity",
    "daily limit reached",
    "weekly limit reached",
];
const RATE_PATTERNS: [&str; 9] = [
    "per minute",
    "per-minute",
    "per min",
    "rpm",
    "requests per minute",
    "too many requests",
    "rate limit",
    "retry after",
    "concurrent request limit",
];

/// A spent plan quota is not retryable; a transient rate limit is.
fn quota_exhausted(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    if RATE_PATTERNS.iter().any(|pattern| lower.contains(pattern)) {
        return false;
    }
    let status_ok = match serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.pointer("/error/status").cloned())
    {
        Some(Value::String(status)) => status == "RESOURCE_EXHAUSTED",
        _ => true,
    };
    status_ok && QUOTA_NEEDLES.iter().any(|needle| lower.contains(needle))
}

async fn failure(status: StatusCode, response: reqwest::Response) -> Failure {
    let retry = retry_after(response.headers());
    let body = read_bounded(response, ERROR_BODY_CAP).await;
    match status.as_u16() {
        429 if quota_exhausted(&String::from_utf8_lossy(&body)) => {
            Failure::Quota { reset_after: retry }
        }
        429 => Failure::RateLimited { retry_after: retry },
        401 | 403 => Failure::Auth,
        code if code >= 500 => Failure::Retryable,
        _ => Failure::Fatal { status, body },
    }
}
