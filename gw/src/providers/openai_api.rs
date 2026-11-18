//! openai-api adapter: direct HTTP to `api.openai.com` with a platform API key.
//!
//! This is the pay-per-token API, not the ChatGPT subscription backend that the `openai`
//! adapter fronts. Chat and Responses requests go to their own endpoints unchanged; Messages
//! requests are translated to chat completions.

use bytes::Bytes;
use futures_util::{future::BoxFuture, FutureExt};
use http::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use http::StatusCode;
use serde_json::Value;

use super::chat::{
    buffered_reply, chat_body, error_body, invalid, names_error, relay, retry_after,
    stream_headers, translated_stream,
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
        let (path, mut body) = match req.inbound {
            Inbound::Chat => ("chat/completions", passthrough(req, true)?),
            Inbound::Responses => ("responses", passthrough(req, false)?),
            Inbound::Messages => ("chat/completions", chat_body(req)?),
        };
        if req.fast == Some(true) {
            if let Some(object) = body.as_object_mut() {
                object
                    .entry("service_tier")
                    .or_insert_with(|| Value::String(PRIORITY_TIER.into()));
            }
        }
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
            return Err(reject(status, response).await);
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
