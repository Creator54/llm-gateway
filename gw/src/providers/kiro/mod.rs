//! kiro upstream adapter: CodeWhisperer `GenerateAssistantResponse` over AWS eventstream.

mod digest;
mod eventstream;
mod payload;
mod response;
mod tools;
mod wire;

pub(crate) use wire::region;

#[cfg(test)]
mod tests;

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

fn headers(token: &str, profile_arn: Option<&str>, client: wire::WireClient) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in wire::headers(token, profile_arn, client) {
        if let Ok(value) = HeaderValue::from_str(&value) {
            out.insert(name, value);
        }
    }
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
    let rate_window = lower.contains("requests per min")
        || lower.contains("per minute")
        || lower.contains("per second")
        || lower.contains("rpm")
        || lower.contains("tpm");
    if rate_window {
        return false;
    }
    lower.contains("insufficient_quota")
        || lower.contains("quota exhausted")
        || lower.contains("quota exceeded")
        || lower.contains("monthly_request_count")
        || lower.contains("account quota")
        || lower.contains("monthly quota")
        || lower.contains("exhausted your")
}

const SUSPENSION_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);

/// Upstream suspended the account; the caller should stop using it for a day.
fn is_suspension(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("temporarily_suspended")
        || lower.contains("temporarily suspended")
        || lower.contains("temporarily is suspended")
        || lower.contains("locked your account")
        || lower.contains("locked it as a")
}

/// Account-scoped refusals: suspension or an exhausted monthly allowance.
fn is_refusal(body: &str) -> bool {
    let lower = body.to_lowercase();
    is_quota(body)
        || lower.contains("temporarily_suspended")
        || lower.contains("temporarily suspended")
        || lower.contains("locked your account")
        || lower.contains("monthly_request_count")
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
    match status.as_u16() {
        429 if is_quota(body) => Failure::Quota { reset_after: None },
        429 => Failure::RateLimited {
            retry_after: retry_after(headers, body),
        },
        401 | 403 if is_suspension(body) => Failure::Quota {
            reset_after: Some(SUSPENSION_COOLDOWN),
        },
        401 | 403 if is_refusal(body) => Failure::Quota { reset_after: None },
        401 | 403 => Failure::Auth,
        400 if is_quota(body) => Failure::Quota { reset_after: None },
        status if (500..600).contains(&status) => Failure::Retryable,
        _ => fatal(status, upstream_message(status, body)),
    }
}

fn upstream_message(status: StatusCode, body: &str) -> String {
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
                && !account.extra.contains_key("profile_arn")
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
            let call = Call {
                client: self.client.clone(),
                base_url: self.base_url.clone(),
                token,
                profile_arn,
                wire_client,
                chat,
                session: req.session.clone(),
                region: wire::region(&account.extra),
            };
            let (response, built) = call.send(None).await?;
            let chat = call.chat.clone();
            let session = session_parts(&chat, req.inbound, &built, call);
            if req.stream {
                Ok(stream_reply(response, session))
            } else {
                let chat = collect(response, &session).await?;
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

/// One client request's wire state, reused by the bounded completion retry.
struct Call {
    client: reqwest::Client,
    base_url: Option<String>,
    token: String,
    profile_arn: Option<String>,
    wire_client: wire::WireClient,
    chat: Value,
    session: String,
    region: String,
}

impl Call {
    async fn send(
        &self,
        forced: Option<payload::CompletionMode>,
    ) -> Result<(reqwest::Response, payload::Built), Failure> {
        let built = payload::build(
            &self.chat,
            self.wire_client,
            self.profile_arn.as_deref(),
            &self.session,
            forced,
        )
        .map_err(|message| fatal(StatusCode::BAD_REQUEST, message))?;
        let url = wire::endpoint(self.base_url.as_deref(), &self.region)
            .map_err(|message| fatal(StatusCode::INTERNAL_SERVER_ERROR, message))?;
        let body = serde_json::to_vec(&built.body).unwrap_or_default();
        let headers = headers(&self.token, self.profile_arn.as_deref(), self.wire_client);
        let mut response = self
            .client
            .post(&url)
            .timeout(TIMEOUT)
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await;
        // The runtime host retries on the legacy canonical host before giving up.
        let unreachable = match &response {
            Ok(response) => matches!(response.status().as_u16(), 502..=504),
            Err(_) => true,
        };
        if unreachable {
            if let Some(fallback) = wire::fallback_endpoint(self.base_url.as_deref(), &self.region)
            {
                response = self
                    .client
                    .post(&fallback)
                    .timeout(TIMEOUT)
                    .headers(headers)
                    .body(body)
                    .send()
                    .await;
            }
        }
        let response = response.map_err(|_| Failure::Retryable)?;
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            return Err(classify(status, &headers, &text));
        }
        Ok((response, built))
    }

    /// The single bounded retry: replay the visible text and demand the completion call.
    async fn retry(&self, text: &str) -> Result<(reqwest::Response, payload::Built), Failure> {
        let mut chat = self.chat.clone();
        if !text.trim().is_empty() {
            if let Some(messages) = chat.get_mut("messages").and_then(Value::as_array_mut) {
                messages.push(serde_json::json!({ "role": "assistant", "content": text }));
            }
        }
        let retry = Call {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            token: self.token.clone(),
            profile_arn: self.profile_arn.clone(),
            wire_client: self.wire_client,
            chat,
            session: self.session.clone(),
            region: self.region.clone(),
        };
        retry
            .send(Some(payload::CompletionMode::TextFallback))
            .await
    }
}

fn json_reply(body: Value) -> Reply {
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    Reply {
        status: StatusCode::OK,
        headers,
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
    mode: payload::CompletionMode,
    call: Call,
}

fn session_parts(chat: &Value, inbound: Inbound, built: &payload::Built, call: Call) -> Session {
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
        names: built.names.clone(),
        mode: built.mode,
        call,
    }
}

/// Drain one eventstream into a translator without emitting anything.
async fn drain(
    response: reqwest::Response,
    translator: &mut response::Translator,
) -> Result<(), Failure> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut decoder = eventstream::Decoder::new();
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
    Ok(())
}

/// Collect the whole eventstream for a non-streamed client request.
async fn collect(response: reqwest::Response, session: &Session) -> Result<Value, Failure> {
    let mut translator =
        response::Translator::new(&session.model, session.names.clone(), session.mode);
    drain(response, &mut translator).await?;
    translator.finish();
    if let Some(text) = translator.fallback_text() {
        // A turn that stopped talking without delivering an answer gets exactly one retry that
        // demands the completion call; a failed retry still returns the first attempt's text.
        if let Ok((response, _)) = session.call.retry(&text).await {
            let mut retry = response::Translator::new(
                &session.model,
                session.names.clone(),
                payload::CompletionMode::TextFallback,
            );
            if drain(response, &mut retry).await.is_ok() {
                retry.finish();
                if retry.produced_output() {
                    return Ok(retry.completion());
                }
            }
        }
    }
    Ok(translator.completion())
}

/// Stream the translated reply to the client as it arrives.
fn stream_reply(response: reqwest::Response, session: Session) -> Reply {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        let mut sink = Sink::new(session.inbound, &session.model);
        if let Some(bytes) = sink.start() {
            if tx.send(Ok(bytes)).await.is_err() {
                return;
            }
        }
        let mut translator =
            response::Translator::new(&session.model, session.names.clone(), session.mode);
        let mut chunks = match pump(response, &mut translator, &mut sink, &tx).await {
            Ok(()) => translator.finish(),
            Err(error) => {
                // Nothing has reached the client yet in a deferred turn, so a failed attempt is
                // reported as an error rather than a partial answer.
                if let Some(bytes) = sink.error(&error) {
                    let _ = tx.send(Ok(bytes)).await;
                }
                finish_sink(&mut sink, &tx).await;
                return;
            }
        };
        if let Some(text) = translator.fallback_text() {
            // One bounded retry that demands the completion call; its answer replaces the
            // progress text of the first attempt.
            if let Ok((retry, built)) = session.call.retry(&text).await {
                let mut second = response::Translator::new(
                    &session.model,
                    built.names,
                    payload::CompletionMode::TextFallback,
                );
                if pump(retry, &mut second, &mut sink, &tx).await.is_ok() {
                    let mut retry_chunks = second.finish();
                    if second.produced_output() {
                        for chunk in retry_chunks {
                            if tx.send(Ok(sink.push(&chunk))).await.is_err() {
                                return;
                            }
                        }
                        finish_sink(&mut sink, &tx).await;
                        return;
                    }
                    retry_chunks.clear();
                }
            }
            chunks = translator.release();
        }
        for chunk in chunks {
            if tx.send(Ok(sink.push(&chunk))).await.is_err() {
                return;
            }
        }
        finish_sink(&mut sink, &tx).await;
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

/// Forward one eventstream attempt into the sink. Never buffers the body.
async fn pump(
    response: reqwest::Response,
    translator: &mut response::Translator,
    sink: &mut Sink,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> Result<(), String> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut decoder = eventstream::Decoder::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "Kiro upstream connection failed".to_owned())?;
        for message in decoder.push(&chunk).map_err(|error| error.to_string())? {
            let Some(event) = response::parse(&message)? else {
                continue;
            };
            for chunk in translator.push(event)? {
                if tx.send(Ok(sink.push(&chunk))).await.is_err() {
                    return Err("client closed the stream".to_owned());
                }
            }
        }
    }
    decoder.finish().map_err(|error| error.to_string())
}

async fn finish_sink(
    sink: &mut Sink,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) {
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

    fn error(&mut self, message: &str) -> Option<Bytes> {
        match self {
            Sink::Chat => {
                let body = serde_json::json!({ "error": { "message": message } });
                Some(Bytes::from(format!("data: {body}\n\n")))
            }
            // The stream adapters have no mid-stream error event; close the reply instead.
            Sink::Responses(_) | Sink::Messages(_) => None,
        }
    }
}
