//! kiro upstream adapter: CodeWhisperer `GenerateAssistantResponse` over AWS eventstream.

mod digest;
mod eventstream;
mod payload;
mod response;
mod tools;
mod wire;

use crate::provider::{Account, Credential, Failure, Inbound, Provider, Reply, Request};
use crate::translate::{
    chat_to_messages, chat_to_responses, messages_to_chat, responses_to_chat, Inbound as Protocol,
    MessagesStream, ResponsesStream,
};
use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Builder ID accounts have no profile of their own; Kiro's own client uses this fixed one.
const BUILDER_ID_SERVICE_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";
const TIMEOUT: Duration = Duration::from_secs(200);

pub struct Kiro {
    client: reqwest::Client,
    base_url: Option<String>,
}

impl Kiro {
    pub fn new(client: reqwest::Client, base_url: Option<String>) -> Self {
        Self { client, base_url }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn token(credential: &Credential) -> Option<&str> {
    match credential {
        Credential::ApiKey(key) => Some(key.as_str()),
        Credential::OAuth { access, .. } => Some(access.as_str()),
        Credential::None => None,
    }
}

fn chat_request(req: &Request) -> Result<Value, Failure> {
    let body: Value = serde_json::from_slice(&req.body).map_err(|error| {
        fatal(
            StatusCode::BAD_REQUEST,
            format!("invalid request body: {error}"),
        )
    })?;
    match req.inbound {
        Inbound::Chat => Ok(body),
        Inbound::Responses => responses_to_chat(&body, &req.model).map_err(translate_failure),
        Inbound::Messages => messages_to_chat(&body, &req.model).map_err(translate_failure),
    }
}

fn translate_failure(error: crate::translate::Error) -> Failure {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST);
    fatal(status, error.message)
}

fn fatal(status: StatusCode, message: impl Into<String>) -> Failure {
    let body = serde_json::json!({ "error": { "message": message.into() } });
    Failure::Fatal {
        status,
        body: Bytes::from(serde_json::to_vec(&body).unwrap_or_default()),
    }
}

fn headers(
    account: &Account,
    token: &str,
    profile_arn: Option<&str>,
    client: wire::WireClient,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in wire::headers(token, profile_arn, client) {
        if let Ok(value) = HeaderValue::from_str(&value) {
            out.insert(name, value);
        }
    }
    let _ = account;
    out
}

fn retry_after(headers: &HeaderMap, body: &str) -> Option<Duration> {
    if let Some(value) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        if let Ok(seconds) = value.trim().parse::<u64>() {
            return Some(Duration::from_secs(seconds));
        }
    }
    let body: Value = serde_json::from_str(body).ok()?;
    for key in ["retry_after", "retryAfter", "retry_after_ms"] {
        if let Some(value) = body.get(key).and_then(Value::as_u64) {
            return Some(if key.ends_with("_ms") {
                Duration::from_millis(value)
            } else {
                Duration::from_secs(value)
            });
        }
    }
    None
}

fn is_quota(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("insufficient_quota")
        || lower.contains("quota exhausted")
        || lower.contains("quota exceeded")
        || lower.contains("monthly_request_count")
        || lower.contains("account quota")
}

/// Plain text carried by an upstream error body.
fn upstream_text(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            for key in ["message", "Message", "errorMessage", "reason"] {
                if let Some(text) = value.get(key).and_then(Value::as_str) {
                    return Some(text.to_owned());
                }
            }
            None
        })
        .unwrap_or_else(|| body.to_owned())
}

fn classify(status: StatusCode, headers: &HeaderMap, body: &str) -> Failure {
    let lower = body.to_lowercase();
    match status.as_u16() {
        429 => {
            if is_quota(body) {
                return Failure::Quota { reset_after: None };
            }
            let suspended = lower.contains("temporarily_suspended") || lower.contains("suspended");
            if suspended && is_quota(body) {
                return Failure::Quota { reset_after: None };
            }
            Failure::RateLimited {
                retry_after: retry_after(headers, body),
            }
        }
        401 | 403 => {
            if is_quota(body) || lower.contains("temporarily_suspended") {
                Failure::Quota { reset_after: None }
            } else {
                Failure::Auth
            }
        }
        400 if lower.contains("insufficient_quota") => Failure::Quota { reset_after: None },
        status if (500..600).contains(&status) => Failure::Retryable,
        _ => fatal(status, upsteam_message(status, body)),
    }
}

fn upsteam_message(status: StatusCode, body: &str) -> String {
    let text = upstream_text(body);
    let text: String = text.chars().take(500).collect();
    if text.trim().is_empty() {
        format!("Kiro upstream error: HTTP {}", status.as_u16())
    } else {
        format!("Kiro upstream error: {text}")
    }
}

impl Provider for Kiro {
    fn id(&self) -> &str {
        "kiro"
    }

    fn send<'a>(
        &'a self,
        req: &'a Request,
        account: &'a Account,
    ) -> BoxFuture<'a, Result<Reply, Failure>> {
        Box::pin(async move {
            let chat = chat_request(req)?;
            let token = token(&account.credential).ok_or(Failure::Auth)?.to_owned();
            if token.trim().is_empty() {
                return Err(Failure::Auth);
            }
            let api_key = token.starts_with("ksk_");
            let builder_id = !api_key
                && account.extra.get("profile_arn").is_none()
                && account
                    .extra
                    .get("auth_type")
                    .is_some_and(|kind| kind == "aws_sso_oidc");
            let profile_arn = if api_key {
                None
            } else if let Some(arn) = account.extra.get("profile_arn") {
                Some(arn.clone())
            } else if builder_id {
                Some(BUILDER_ID_SERVICE_PROFILE_ARN.to_owned())
            } else {
                None
            };
            let wire_client = if api_key || builder_id || profile_arn.is_none() {
                wire::WireClient::Cli
            } else {
                wire::WireClient::Ide
            };
            let built = payload::build(&chat, wire_client, profile_arn.as_deref(), &req.session)
                .map_err(|message| fatal(StatusCode::BAD_REQUEST, message))?;
            let url = wire::endpoint(self.base_url.as_deref(), &wire::region(&account.extra))
                .map_err(|message| fatal(StatusCode::INTERNAL_SERVER_ERROR, message))?;
            let body = serde_json::to_vec(&built.body).unwrap_or_default();
            let response = self
                .client
                .post(&url)
                .timeout(TIMEOUT)
                .headers(headers(
                    account,
                    &token,
                    profile_arn.as_deref(),
                    wire_client,
                ))
                .body(body)
                .send()
                .await
                .map_err(|_| Failure::Retryable)?;
            let status = response.status();
            if !status.is_success() {
                let headers = response.headers().clone();
                let text = response.text().await.unwrap_or_default();
                return Err(classify(status, &headers, &text));
            }
            let session = session_parts(&chat, req.inbound, built);
            if req.stream {
                Ok(stream_reply(response, session))
            } else {
                let chat = collect(response, session).await?;
                let body = match req.inbound {
                    Inbound::Chat => chat,
                    Inbound::Responses => {
                        chat_to_responses(&chat, &req.model, None).map_err(translate_failure)?
                    }
                    Inbound::Messages => {
                        chat_to_messages(&chat, &req.model).map_err(translate_failure)?
                    }
                };
                Ok(json_reply(body))
            }
        })
    }
}

fn json_reply(body: Value) -> Reply {
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    Reply {
        status: StatusCode::OK,
        headers: HeaderMap::new(),
        body: Box::pin(futures_util::stream::once(
            async move { Ok(Bytes::from(bytes)) },
        )),
    }
}

/// Per-request state the reply side needs.
struct Session {
    model: String,
    inbound: Protocol,
    names: wire::ToolNames,
}

fn session_parts(chat: &Value, inbound: Inbound, built: payload::Built) -> Session {
    let model = chat
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let inbound = match inbound {
        Inbound::Chat => Protocol::Chat,
        Inbound::Responses => Protocol::Responses,
        Inbound::Messages => Protocol::Messages,
    };
    Session {
        model,
        inbound,
        names: built.names,
    }
}

/// Collect the whole eventstream for a non-streamed client request.
async fn collect(response: reqwest::Response, session: Session) -> Result<Value, Failure> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut decoder = eventstream::Decoder::new();
    let mut translator = response::Translator::new(&session.model, session.names);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Failure::Retryable)?;
        let messages = decoder.push(&chunk).map_err(|_| Failure::Retryable)?;
        for message in messages {
            if let Some(event) = response::parse(&message).map_err(|_| Failure::Retryable)? {
                translator.push(event).map_err(|_| Failure::Retryable)?;
            }
        }
    }
    decoder.finish().map_err(|_| Failure::Retryable)?;
    Ok(translator.completion())
}

/// Stream the translated reply to the client as it arrives.
fn stream_reply(response: reqwest::Response, session: Session) -> Reply {
    use futures_util::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let model = session.model.clone();
    tokio::spawn(async move {
        let mut decoder = eventstream::Decoder::new();
        let mut translator = response::Translator::new(&model, session.names);
        let mut sink = Sink::new(session.inbound, &model);
        let mut stream = response.bytes_stream();
        if let Some(bytes) = sink.start() {
            if tx.send(Ok(bytes)).await.is_err() {
                return;
            }
        }
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                fail_stream(&mut sink, &tx, "Kiro upstream connection failed").await;
                return;
            };
            let messages = match decoder.push(&chunk) {
                Ok(messages) => messages,
                Err(error) => {
                    fail_stream(&mut sink, &tx, &error.to_string()).await;
                    return;
                }
            };
            for message in messages {
                let event = match response::parse(&message) {
                    Ok(Some(event)) => event,
                    Ok(None) => continue,
                    Err(error) => {
                        fail_stream(&mut sink, &tx, &error).await;
                        return;
                    }
                };
                let chunks = match translator.push(event) {
                    Ok(chunks) => chunks,
                    Err(error) => {
                        fail_stream(&mut sink, &tx, &error).await;
                        return;
                    }
                };
                for chunk in chunks {
                    if tx.send(Ok(sink.push(&chunk))).await.is_err() {
                        return;
                    }
                }
            }
        }
        if let Err(error) = decoder.finish() {
            fail_stream(&mut sink, &tx, &error.to_string()).await;
            return;
        }
        for chunk in translator.finish() {
            if tx.send(Ok(sink.push(&chunk))).await.is_err() {
                return;
            }
        }
        if let Some(bytes) = sink.finish() {
            let _ = tx.send(Ok(bytes)).await;
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    Reply {
        status: StatusCode::OK,
        headers,
        body: Box::pin(stream) as BoxStream<'static, Result<Bytes, std::io::Error>>,
    }
}

async fn fail_stream(
    sink: &mut Sink,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    message: &str,
) {
    let bytes = sink.error(message);
    let _ = tx.send(Ok(bytes)).await;
    if let Some(bytes) = sink.finish() {
        let _ = tx.send(Ok(bytes)).await;
    }
}

/// Protocol-specific serialization of Chat chunks.
enum Sink {
    Chat,
    Responses(ResponsesStream),
    Messages(MessagesStream),
}

impl Sink {
    fn new(inbound: Protocol, model: &str) -> Self {
        match inbound {
            Protocol::Chat => Sink::Chat,
            Protocol::Responses => Sink::Responses(ResponsesStream::new("resp_kiro", model)),
            Protocol::Messages => Sink::Messages(MessagesStream::new("msg_kiro", model)),
        }
    }

    fn start(&mut self) -> Option<Bytes> {
        match self {
            Sink::Responses(stream) => Some(Bytes::from(stream.start())),
            Sink::Messages(stream) => Some(Bytes::from(stream.start())),
            Sink::Chat => None,
        }
    }

    fn push(&mut self, chunk: &Value) -> Bytes {
        let text = serde_json::to_string(chunk).unwrap_or_default();
        match self {
            Sink::Chat => Bytes::from(format!("data: {text}\n\n")),
            Sink::Responses(stream) => Bytes::from(stream.push_chunk(&text)),
            Sink::Messages(stream) => Bytes::from(stream.push_chunk(&text)),
        }
    }

    fn finish(&mut self) -> Option<Bytes> {
        match self {
            Sink::Chat => Some(Bytes::from_static(b"data: [DONE]\n\n")),
            Sink::Responses(stream) => Some(Bytes::from(stream.finish())),
            Sink::Messages(stream) => Some(Bytes::from(stream.finish())),
        }
    }

    fn error(&mut self, message: &str) -> Bytes {
        match self {
            Sink::Chat => {
                let body = serde_json::json!({ "error": { "message": message } });
                Bytes::from(format!("data: {body}\n\n"))
            }
            // The stream adapters have no mid-stream error event; close the reply instead.
            Sink::Responses(_) | Sink::Messages(_) => Bytes::new(),
        }
    }
}
