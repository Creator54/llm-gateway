//! Chat-completions plumbing shared by the adapters that talk to an OpenAI-shaped
//! upstream: request translation, reply translation and error-body reading.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::{stream::BoxStream, StreamExt};
use http::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use http::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::provider::{Failure, Inbound, Reply, Request};
use crate::translate::{
    chat_to_messages, chat_to_responses, messages_to_chat, responses_to_chat, MessagesStream,
    ResponsesStream,
};

/// Largest error body read before classifying a failure.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Largest non-streaming reply buffered for translation.
const MAX_NON_STREAM_BODY: usize = 32 * 1024 * 1024;

/// The chat-completions request for `req`, with the routed model and usage reporting set.
pub(super) fn chat_body(req: &Request) -> Result<Value, Failure> {
    let value: Value = serde_json::from_slice(&req.body)
        .map_err(|_| invalid("request body is not valid JSON", req.inbound))?;
    let mut chat = match req.inbound {
        Inbound::Responses => responses_to_chat(&value, &req.model),
        Inbound::Messages => messages_to_chat(&value, &req.model),
        Inbound::Chat => Ok(value),
    }
    .map_err(|error| translate_failure(&error, req.inbound))?;
    let Some(object) = chat.as_object_mut() else {
        return Err(invalid("request body is not a JSON object", req.inbound));
    };
    // The routed id, without the `opencode/` prefix the client used to pick us.
    object.insert("model".into(), Value::String(req.model.clone()));
    // Without this the stream carries no usage, so spend and cache hits go unrecorded.
    if object.get("stream").and_then(Value::as_bool) == Some(true) {
        let options = object
            .entry("stream_options")
            .or_insert_with(|| Value::Object(Default::default()));
        if let Some(options) = options.as_object_mut() {
            options.entry("include_usage").or_insert(Value::Bool(true));
        }
    }
    Ok(chat)
}

pub(super) fn stream_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(
        http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    headers
}

pub(super) fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers
}

pub(super) fn invalid(message: &str, inbound: Inbound) -> Failure {
    translate_failure(&crate::translate::Error::bad_request(message), inbound)
}

pub(super) fn translate_failure(error: &crate::translate::Error, inbound: Inbound) -> Failure {
    Failure::Fatal {
        status: StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_GATEWAY),
        body: Bytes::from(serde_json::to_vec(&error.to_body(wire(inbound))).unwrap_or_default()),
    }
}

fn wire(inbound: Inbound) -> crate::translate::Inbound {
    match inbound {
        Inbound::Chat => crate::translate::Inbound::Chat,
        Inbound::Responses => crate::translate::Inbound::Responses,
        Inbound::Messages => crate::translate::Inbound::Messages,
    }
}

/// An error reply's body, read up to a cap.
pub(super) async fn error_body(response: reqwest::Response) -> Vec<u8> {
    let mut stream = response.bytes_stream();
    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        collected.extend_from_slice(&chunk);
        if collected.len() > MAX_ERROR_BODY {
            break;
        }
    }
    collected
}

/// Whether the body names `code` in a `type` or `code` field at the roots the upstream uses.
pub(super) fn names_error(body: &[u8], code: &str) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    ["", "/error"].iter().any(|root| {
        ["type", "code"].iter().any(|field| {
            value
                .pointer(&format!("{root}/{field}"))
                .and_then(Value::as_str)
                == Some(code)
        })
    })
}

pub(super) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    raw.parse::<f64>()
        .ok()
        .map(|seconds| Duration::from_secs_f64(seconds.max(0.0)))
}

pub(super) fn relay(
    response: reqwest::Response,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    response
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other))
        .boxed()
}

async fn read_body(response: reqwest::Response) -> Result<Vec<u8>, Failure> {
    let mut upstream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = upstream.next().await {
        let chunk = chunk.map_err(|_| Failure::Retryable)?;
        body.extend_from_slice(&chunk);
        if body.len() > MAX_NON_STREAM_BODY {
            return Err(Failure::Retryable);
        }
    }
    Ok(body)
}

pub(super) async fn buffered_reply(
    req: &Request,
    response: reqwest::Response,
) -> Result<Reply, Failure> {
    let body = read_body(response).await?;
    let chat: Value = serde_json::from_slice(&body).map_err(|_| Failure::Retryable)?;
    let reply = match req.inbound {
        Inbound::Chat => chat,
        Inbound::Responses => chat_to_responses(
            &chat,
            &req.model,
            &serde_json::from_slice(&req.body).unwrap_or_default(),
        )
        .map_err(|error| translate_failure(&error, req.inbound))?,
        Inbound::Messages => chat_to_messages(&chat, &req.model)
            .map_err(|error| translate_failure(&error, req.inbound))?,
    };
    let bytes = serde_json::to_vec(&reply).unwrap_or_default();
    Ok(Reply {
        status: StatusCode::OK,
        headers: json_headers(),
        body: futures_util::stream::iter(vec![Ok(Bytes::from(bytes))]).boxed(),
    })
}

enum Encoder {
    Responses(ResponsesStream),
    Messages(MessagesStream),
}

/// Rewrites the upstream chat SSE into the client's protocol as it arrives.
struct Decoder {
    encoder: Encoder,
    buffer: Vec<u8>,
    done: bool,
}

impl Decoder {
    fn new(req: &Request) -> Self {
        let unique = format!("{:032x}", unique_suffix());
        let encoder = match req.inbound {
            Inbound::Responses => Encoder::Responses(ResponsesStream::new(
                format!("resp_{unique}"),
                &req.model,
                &serde_json::from_slice(&req.body).unwrap_or_default(),
            )),
            _ => Encoder::Messages(MessagesStream::new(format!("msg_{unique}"), &req.model)),
        };
        Self {
            encoder,
            buffer: Vec::new(),
            done: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            let text = text.strip_prefix("data:").map(str::trim).unwrap_or(text);
            if text.is_empty() || text == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if value.get("error").is_some() {
                self.done = true;
                out.extend(self.error_frame(&value.to_string()));
                return out;
            }
            let payload = serde_json::to_string(&value).unwrap_or_default();
            match &mut self.encoder {
                Encoder::Responses(stream) => out.extend(stream.push_chunk(&payload)),
                Encoder::Messages(stream) => out.extend(stream.push_chunk(&payload)),
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        match &mut self.encoder {
            Encoder::Responses(stream) => stream.finish(),
            Encoder::Messages(stream) => stream.finish(),
        }
    }

    fn error_frame(&self, message: &str) -> Vec<u8> {
        let error = crate::translate::Error::bad_gateway(message);
        let inbound = match self.encoder {
            Encoder::Responses(_) => crate::translate::Inbound::Responses,
            Encoder::Messages(_) => crate::translate::Inbound::Messages,
        };
        let body = serde_json::to_string(&error.to_body(inbound)).unwrap_or_default();
        format!("event: error\ndata: {body}\n\n").into_bytes()
    }
}

pub(super) fn translated_stream(
    req: &Request,
    response: reqwest::Response,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    let decoder = Decoder::new(req);
    let upstream = response.bytes_stream().boxed();
    futures_util::stream::unfold(
        (decoder, upstream),
        |(mut decoder, mut upstream)| async move {
            loop {
                if decoder.done {
                    return None;
                }
                match upstream.next().await {
                    Some(Ok(chunk)) => {
                        let out = decoder.feed(&chunk);
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), (decoder, upstream)));
                        }
                    }
                    Some(Err(_)) => {
                        return Some((
                            Err(std::io::Error::other("opencode upstream stream failed")),
                            (decoder, upstream),
                        ));
                    }
                    None => {
                        let tail = decoder.finish();
                        return if tail.is_empty() {
                            None
                        } else {
                            Some((Ok(Bytes::from(tail)), (decoder, upstream)))
                        };
                    }
                }
            }
        },
    )
    .boxed()
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn unique_suffix() -> u64 {
    let digest = digest(&format!("{}:{}", now_ms(), std::process::id()));
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

pub(super) fn digest(input: &str) -> Vec<u8> {
    Sha256::digest(input.as_bytes()).to_vec()
}
