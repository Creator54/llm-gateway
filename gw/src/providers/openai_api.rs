//! openai-api adapter: direct HTTP to `api.openai.com` with a platform API key.
//!
//! This is the pay-per-token API, not the ChatGPT subscription backend that the `openai`
//! adapter fronts. Chat and Responses requests go to their own endpoints unchanged; Messages
//! requests are translated to chat completions.

use bytes::Bytes;
use futures_util::{future::BoxFuture, FutureExt};
use http::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use http::StatusCode;
use serde_json::{json, Value};

use super::chat::{
    buffered_reply, chat_body, error_body, invalid, names_error, relay, responses_json_to_chat,
    responses_to_chat_stream, retry_after, stream_headers, translate_failure, translated_stream,
};
use crate::provider::{Account, Credential, Failure, Inbound, Provider, Reply, Request};

/// Includes the API version segment, so a test or proxy override replaces the whole prefix.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// `error.code` of a 429 that no waiting fixes: the account is out of credit.
const OUT_OF_CREDIT: &str = "insufficient_quota";

/// The priority tier `--fast` asks for.
const PRIORITY_TIER: &str = "priority";

/// Reply headers worth relaying: request ids for support, and the limit counters.
const RELAYED: [&str; 2] = ["x-request-id", "openai-request-id"];

pub struct OpenAiApi {
    client: reqwest::Client,
    base: String,
}

impl OpenAiApi {
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
        let mut via_responses = false;
        let (path, mut body) = match req.inbound {
            Inbound::Chat => {
                let body = passthrough(req, true)?;
                if needs_responses(&body) {
                    via_responses = true;
                    let translated = crate::translate::chat_request_to_responses(&body, &req.model)
                        .map_err(|error| translate_failure(&error, req.inbound))?;
                    ("responses", translated)
                } else {
                    ("chat/completions", body)
                }
            }
            Inbound::Responses => ("responses", passthrough(req, false)?),
            Inbound::Messages => ("chat/completions", chat_body(req)?),
        };
        if path == "responses" {
            instructions_into_input(&mut body);
        }
        if req.fast == Some(true) {
            if let Some(object) = body.as_object_mut() {
                object
                    .entry("service_tier")
                    .or_insert_with(|| Value::String(PRIORITY_TIER.into()));
            }
        }
        let sent = body.clone();
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| invalid("request body is not serializable", req.inbound))?;

        let url = format!("{}/{path}", self.base);
        let response = match self
            .client
            .post(&url)
            .bearer_auth(key)
            .header(CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(url = %url, %error, "openai-api upstream request failed");
                return Err(Failure::Retryable);
            }
        };
        let status = response.status();
        if !status.is_success() {
            let failure = reject(status, response).await;
            if let Failure::Fatal { body: reply, .. } = &failure {
                tracing::warn!(
                    %status, %url, sent = %shape(&sent),
                    reply = %String::from_utf8_lossy(reply),
                    "openai-api refused the request"
                );
            }
            return Err(failure);
        }

        if req.inbound == Inbound::Messages {
            return if req.stream {
                Ok(Reply {
                    status,
                    headers: stream_headers(),
                    body: translated_stream(req, response),
                })
            } else {
                buffered_reply(req, response).await
            };
        }
        if via_responses {
            return if req.stream {
                let usage = chat_usage_asked(&req.body);
                Ok(Reply {
                    status,
                    headers: stream_headers(),
                    body: responses_to_chat_stream(response, &req.model, usage),
                })
            } else {
                responses_json_to_chat(response, &req.model).await
            };
        }
        let mut headers = reply_headers(response.headers());
        if req.stream {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        }
        Ok(Reply {
            status,
            headers,
            body: relay(response),
        })
    }
}

impl Provider for OpenAiApi {
    fn id(&self) -> &str {
        "openai-api"
    }

    fn send<'a>(
        &'a self,
        req: &'a Request,
        account: &'a Account,
    ) -> BoxFuture<'a, Result<Reply, Failure>> {
        self.turn(req, account).boxed()
    }
}

/// The platform API does not cache the Responses `instructions` field on current models, but
/// does cache the same text as the first `developer` input item. Only the Codex backend
/// requires `instructions`, so here the system prompt moves into the input.
fn instructions_into_input(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let Some(Value::String(text)) = object
        .get("instructions")
        .filter(|v| v.is_string())
        .cloned()
    else {
        return;
    };
    let item = json!({
        "type": "message",
        "role": "developer",
        "content": [{ "type": "input_text", "text": text }],
    });
    let mut items = match object.remove("input") {
        Some(Value::Array(items)) => items,
        Some(Value::String(text)) => vec![json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        })],
        _ => Vec::new(),
    };
    items.insert(0, item);
    object.insert("input".into(), Value::Array(items));
    object.remove("instructions");
}

/// Function tools are refused on chat completions by models that reason by default, even when
/// the caller names no effort; the Responses endpoint accepts them. Only an explicit `none`
/// is known to be safe on chat completions.
fn needs_responses(body: &Value) -> bool {
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let no_reasoning = body.get("reasoning_effort").and_then(Value::as_str) == Some("none");
    tools && !no_reasoning
}

/// Top-level keys of a body, with the sizes that matter when a request is refused.
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

/// Whether the caller asked for the final usage chunk of a chat stream.
fn chat_usage_asked(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.pointer("/stream_options/include_usage")?.as_bool())
        .unwrap_or(false)
}

/// The client's body with the routed model, which is already the API's own shape. A chat
/// stream also asks for its usage chunk, or spend and cache hits would go unrecorded.
fn passthrough(req: &Request, chat: bool) -> Result<Value, Failure> {
    let mut value: Value = serde_json::from_slice(&req.body)
        .map_err(|_| invalid("request body is not valid JSON", req.inbound))?;
    let Some(object) = value.as_object_mut() else {
        return Err(invalid("request body is not a JSON object", req.inbound));
    };
    object.insert("model".into(), Value::String(req.model.clone()));
    // Newer models refuse the deprecated `max_tokens`; `max_completion_tokens` works on all.
    if chat && !object.contains_key("max_completion_tokens") {
        if let Some(limit) = object.remove("max_tokens") {
            object.insert("max_completion_tokens".into(), limit);
        }
    }
    if chat && object.get("stream").and_then(Value::as_bool) == Some(true) {
        if let Some(options) = object
            .entry("stream_options")
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
        {
            options.entry("include_usage").or_insert(Value::Bool(true));
        }
    }
    Ok(value)
}

fn reply_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in upstream {
        let name = name.as_str();
        let relayed =
            name == "content-type" || RELAYED.contains(&name) || name.starts_with("x-ratelimit-");
        if let (true, Ok(name)) = (relayed, HeaderName::from_bytes(name.as_bytes())) {
            headers.append(name, value.clone());
        }
    }
    headers
}

/// 401 is a bad key. A 403 is left to the caller: the key may only lack access to this
/// model, and marking the account as logged out would sideline it for everything else.
async fn reject(status: StatusCode, response: reqwest::Response) -> Failure {
    let wait = retry_after(response.headers());
    let body = error_body(response).await;
    match status.as_u16() {
        401 => Failure::Auth,
        429 if names_error(&body, OUT_OF_CREDIT) => Failure::Quota { reset_after: None },
        429 => Failure::RateLimited { retry_after: wait },
        code if (500..600).contains(&code) => Failure::Retryable,
        _ => Failure::Fatal {
            status,
            body: Bytes::from(body),
        },
    }
}
