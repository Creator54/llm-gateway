//! opencode adapter: direct HTTP to opencode.ai, no CLI subprocess.
//!
//! Wire contract and the free-tier gate: `docs/provider-specs/opencode-direct.md`.
//! Every model, free or paid, uses one OpenAI-shaped endpoint.

mod id;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{future::BoxFuture, FutureExt};
use http::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use http::StatusCode;

use super::chat::{
    buffered_reply, chat_body, digest, error_body, invalid, names_error, now_ms, relay,
    retry_after, stream_headers, translated_stream,
};
use crate::provider::{Account, Credential, Failure, Inbound, Provider, Reply, Request};

/// Canonical endpoint. Zen and Go share one host and one request shape.
const BASE_URL: &str = "https://opencode.ai";

/// The free tier treats this key as anonymous; a real key enables the paid models.
const ANONYMOUS_KEY: &str = "public";

/// `Retry-After` on a free-tier refusal is hours; cap it so a pool recovers on its own.
const MAX_COOLDOWN: Duration = Duration::from_secs(6 * 3600);

/// Inline retries when the free tier answers a transient 429.
const RETRY_ATTEMPTS: u32 = 3;

/// Longest `Retry-After` still worth waiting for inside one request.
const RETRY_WINDOW: Duration = Duration::from_secs(3);

/// Pause before the second and third attempt; doubled each time.
const RETRY_BACKOFF: Duration = Duration::from_millis(400);

#[derive(Clone)]
pub struct Opencode {
    client: reqwest::Client,
    base: String,
    version: String,
    /// Session ids by client session: the CLI mints one per session and reuses it.
    sessions: Arc<Mutex<HashMap<String, String>>>,
}

/// Bound on remembered sessions; past it the map restarts, minting fresh ids.
const MAX_SESSIONS: usize = 1024;

impl Opencode {
    pub fn new(client: reqwest::Client, base_url: Option<String>) -> Self {
        let base = base_url
            .unwrap_or_else(|| BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        Self {
            client,
            base,
            version: "1.19.5".to_string(),
            sessions: Arc::default(),
        }
    }

    async fn turn(&self, req: &Request, account: &Account) -> Result<Reply, Failure> {
        let body = outbound_body(req)?;
        let url = format!("{}/zen/v1/chat/completions", self.base);

        let mut attempt = 1;
        let response = loop {
            let headers = self.outbound_headers(req, account);
            let sent = self
                .client
                .post(&url)
                .headers(headers)
                .body(body.clone())
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!(url = %url, %error, "opencode upstream request failed");
                    return Err(Failure::Retryable);
                }
            };
            // A 429 without a long Retry-After is the free tier's intermittent
            // refusal; a long one means the budget is spent and waiting is futile.
            let transient = response.status() == StatusCode::TOO_MANY_REQUESTS
                && retry_after(response.headers()).is_none_or(|wait| wait <= RETRY_WINDOW);
            if !transient || attempt >= RETRY_ATTEMPTS {
                break response;
            }
            let pause = RETRY_BACKOFF * (1 << (attempt - 1));
            tracing::debug!(attempt, ?pause, "opencode asked to slow down; retrying");
            tokio::time::sleep(pause).await;
            attempt += 1;
        };

        let status = response.status();
        if !status.is_success() {
            return Err(reject(status, response).await);
        }

        if !req.stream {
            return buffered_reply(req, response).await;
        }
        // Chat inbound is already the upstream shape, so its stream passes through.
        if req.inbound == Inbound::Chat {
            return Ok(Reply {
                status,
                headers: stream_headers(),
                body: relay(response),
            });
        }
        Ok(Reply {
            status,
            headers: stream_headers(),
            body: translated_stream(req, response),
        })
    }

    /// The CLI's session id for `key`, stamped when the session is first seen.
    /// Requests without a client session are each their own session, like `opencode run`.
    fn session_id(&self, key: &str, now: u64) -> String {
        let mint = || id::session(now, 1, &digest(&format!("{key}:{now}")));
        if key.is_empty() {
            return mint();
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if sessions.len() >= MAX_SESSIONS && !sessions.contains_key(key) {
            sessions.clear();
        }
        sessions.entry(key.to_owned()).or_insert_with(mint).clone()
    }

    fn outbound_headers(&self, req: &Request, account: &Account) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let key = key_of(account);
        let mut auth = String::with_capacity(7 + key.len());
        auth.push_str("Bearer ");
        auth.push_str(key);
        if let Ok(value) = HeaderValue::from_str(&auth) {
            headers.insert(http::header::AUTHORIZATION, value);
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let now = now_ms();
        let session = self.session_id(&req.session, now);
        let request = id::request(now, 1, &digest(&format!("{}:{now}", req.session)));
        let agent = format!(
            "opencode/{} ai-sdk/provider-utils/4.0.23 runtime/bun/1.4.4",
            self.version
        );
        // Order matches the CLI so a recorded diff stays readable.
        for (name, value) in [
            ("user-agent", agent.as_str()),
            ("x-opencode-client", "cli"),
            ("x-opencode-project", "global"),
            ("x-opencode-request", request.as_str()),
            ("x-opencode-session", session.as_str()),
            ("x-opencode-session-id", session.as_str()),
        ] {
            if let (Ok(name), Ok(value)) = (
                http::header::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
        headers.insert(http::header::ACCEPT, HeaderValue::from_static("*/*"));
        if let Some(encoding) = req.headers.get(http::header::ACCEPT_ENCODING) {
            if let Ok(value) = HeaderValue::from_bytes(encoding.as_bytes()) {
                headers.insert(http::header::ACCEPT_ENCODING, value);
            }
        }
        headers
    }
}

impl Provider for Opencode {
    fn id(&self) -> &str {
        "opencode"
    }

    fn send<'a>(
        &'a self,
        req: &'a Request,
        account: &'a Account,
    ) -> BoxFuture<'a, Result<Reply, Failure>> {
        self.turn(req, account).boxed()
    }
}

/// Chat-shaped upstream body for any inbound protocol.
fn outbound_body(req: &Request) -> Result<Vec<u8>, Failure> {
    let chat = chat_body(req)?;
    serde_json::to_vec(&chat).map_err(|_| invalid("request body is not serializable", req.inbound))
}

fn key_of(account: &Account) -> &str {
    match &account.credential {
        Credential::ApiKey(key) if !key.is_empty() => key,
        Credential::OAuth { access, .. } if !access.is_empty() => access,
        _ => ANONYMOUS_KEY,
    }
}

async fn reject(status: StatusCode, response: reqwest::Response) -> Failure {
    let retry_after = retry_after(response.headers());
    let collected = error_body(response).await;
    let free_limit = names_error(&collected, "FreeUsageLimitError");
    match status.as_u16() {
        401 | 403 => Failure::Auth,
        402 => Failure::Quota {
            reset_after: retry_after,
        },
        429 if free_limit => Failure::Quota {
            reset_after: retry_after.map(|wait| wait.min(MAX_COOLDOWN)),
        },
        429 => Failure::RateLimited { retry_after },
        code if (500..600).contains(&code) => Failure::Retryable,
        _ => Failure::Fatal {
            status,
            body: Bytes::from(collected),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, routing::any, Router};
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Seen {
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    type Captured = Arc<Mutex<Option<Seen>>>;

    async fn capture(
        State(state): State<(Captured, u16)>,
        req: axum::extract::Request,
    ) -> axum::response::Response {
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 20)
            .await
            .unwrap_or_default();
        let headers = parts
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        *state.0.lock().unwrap() = Some(Seen {
            path: parts.uri.path().to_owned(),
            headers,
            body: bytes.to_vec(),
        });
        let body = if state.1 >= 400 {
            r#"{"type":"error","error":{"type":"FreeUsageLimitError","message":"limit"}}"#
                .to_owned()
        } else {
            r#"{"id":"c","object":"chat.completion","model":"beam-free","choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}]}"#.to_owned()
        };
        axum::response::Response::builder()
            .status(state.1)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap()
    }

    async fn spawn(status: u16) -> (String, Captured) {
        let captured: Captured = Arc::new(Mutex::new(None));
        let app = Router::new()
            .fallback(any(capture))
            .with_state((captured.clone(), status));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), captured)
    }

    fn request(inbound: Inbound, body: Value, stream: bool) -> Request {
        Request {
            inbound,
            model: "beam-free".into(),
            headers: HeaderMap::new(),
            body: Bytes::from(serde_json::to_vec(&body).unwrap()),
            stream,
            session: "session-abc".into(),
            fast: None,
        }
    }

    fn anonymous() -> Account {
        Account {
            id: "anonymous".into(),
            credential: Credential::None,
            extra: Default::default(),
        }
    }

    fn header(seen: &Seen, name: &str) -> Option<String> {
        seen.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    #[tokio::test]
    async fn chat_requests_send_the_bare_model_id() {
        let (base, captured) = spawn(200).await;
        let adapter = Opencode::new(reqwest::Client::new(), Some(base));
        let body = serde_json::json!({"model":"opencode/beam-free","messages":[{"role":"user","content":"hi"}]});
        let _ = adapter
            .send(&request(Inbound::Chat, body, false), &anonymous())
            .await
            .unwrap();
        let seen = captured.lock().unwrap().clone().unwrap();
        assert_eq!(seen.path, "/zen/v1/chat/completions");
        assert_eq!(
            serde_json::from_slice::<Value>(&seen.body).unwrap(),
            serde_json::json!({"model":"beam-free","messages":[{"role":"user","content":"hi"}]})
        );
    }

    #[test]
    fn streams_ask_for_usage() {
        let body = serde_json::json!({"model":"opencode/beam-free","stream":true,"messages":[]});
        let out: Value =
            serde_json::from_slice(&outbound_body(&request(Inbound::Chat, body, true)).unwrap())
                .unwrap();
        assert_eq!(out["stream_options"]["include_usage"], true);
        let body = serde_json::json!({"model":"x","stream":true,"stream_options":{"include_usage":false},"messages":[]});
        let out: Value =
            serde_json::from_slice(&outbound_body(&request(Inbound::Chat, body, true)).unwrap())
                .unwrap();
        assert_eq!(out["stream_options"]["include_usage"], false);
    }

    #[tokio::test]
    async fn free_tier_sends_the_anonymous_key_and_session_ids() {
        let (base, captured) = spawn(200).await;
        let adapter = Opencode::new(reqwest::Client::new(), Some(base));
        let body = serde_json::json!({"model":"beam-free","messages":[]});
        let _ = adapter
            .send(&request(Inbound::Chat, body, false), &anonymous())
            .await
            .unwrap();
        let seen = captured.lock().unwrap().clone().unwrap();
        assert_eq!(
            header(&seen, "authorization").as_deref(),
            Some("Bearer public")
        );
        assert_eq!(header(&seen, "x-opencode-client").as_deref(), Some("cli"));
        let session = header(&seen, "x-opencode-session").unwrap();
        assert!(session.starts_with("ses_"));
        assert_eq!(session.len(), 30);
        assert_eq!(
            header(&seen, "x-opencode-session-id").as_deref(),
            Some(session.as_str())
        );
        assert!(header(&seen, "x-opencode-request")
            .unwrap()
            .starts_with("msg_"));

        // The CLI keeps one session id for a whole session.
        let body = serde_json::json!({"model":"beam-free","messages":[]});
        let _ = adapter
            .send(&request(Inbound::Chat, body, false), &anonymous())
            .await
            .unwrap();
        let again = captured.lock().unwrap().clone().unwrap();
        assert_eq!(header(&again, "x-opencode-session"), Some(session));
    }

    #[tokio::test]
    async fn api_key_accounts_send_their_key() {
        let (base, captured) = spawn(200).await;
        let adapter = Opencode::new(reqwest::Client::new(), Some(base));
        let account = Account {
            id: "paid".into(),
            credential: Credential::ApiKey("sk-test".into()),
            extra: Default::default(),
        };
        let _ = adapter
            .send(
                &request(Inbound::Chat, serde_json::json!({"model":"glm-5.3"}), false),
                &account,
            )
            .await
            .unwrap();
        let seen = captured.lock().unwrap().clone().unwrap();
        assert_eq!(
            header(&seen, "authorization").as_deref(),
            Some("Bearer sk-test")
        );
    }

    #[tokio::test]
    async fn messages_requests_are_translated_to_chat() {
        let (base, captured) = spawn(200).await;
        let adapter = Opencode::new(reqwest::Client::new(), Some(base));
        let body = serde_json::json!({
            "model": "beam-free",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let _ = adapter
            .send(&request(Inbound::Messages, body, false), &anonymous())
            .await
            .unwrap();
        let seen = captured.lock().unwrap().clone().unwrap();
        let sent: Value = serde_json::from_slice(&seen.body).unwrap();
        assert_eq!(sent["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn free_usage_limit_is_a_quota_failure() {
        let (base, _) = spawn(429).await;
        let adapter = Opencode::new(reqwest::Client::new(), Some(base));
        let body = serde_json::json!({"model":"beam-free","messages":[]});
        let failure = match adapter
            .send(&request(Inbound::Chat, body, false), &anonymous())
            .await
        {
            Ok(_) => panic!("expected a quota failure"),
            Err(failure) => failure,
        };
        assert!(matches!(failure, Failure::Quota { .. }));
    }

    /// Upstream answers `failures` 429s, then `status` with the usual body.
    async fn spawn_flaky(failures: usize, status: u16) -> (String, Arc<Mutex<usize>>) {
        let hits = Arc::new(Mutex::new(0usize));
        #[derive(Clone)]
        struct Stateful {
            hits: Arc<Mutex<usize>>,
            failures: usize,
            status: u16,
        }
        async fn answer(State(state): State<Stateful>) -> axum::response::Response {
            let mut hits = state.hits.lock().unwrap();
            *hits += 1;
            let seen = *hits;
            drop(hits);
            let (code, body) = if seen <= state.failures {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    r#"{"type":"error","error":{"type":"FreeUsageLimitError","message":"x"}}"#,
                )
            } else {
                (
                    StatusCode::from_u16(state.status).unwrap(),
                    r#"{"id":"c","object":"chat.completion","model":"m","choices":[]}"#,
                )
            };
            axum::response::Response::builder()
                .status(code)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .unwrap()
        }
        let state = Stateful {
            hits: hits.clone(),
            failures,
            status,
        };
        let app = Router::new().fallback(any(answer)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), hits)
    }

    #[tokio::test]
    async fn transient_429_is_retried_then_served() {
        let (base, hits) = spawn_flaky(2, 200).await;
        let adapter = Opencode::new(reqwest::Client::new(), Some(base));
        let body = serde_json::json!({"model":"beam-free","messages":[]});
        let reply = adapter
            .send(&request(Inbound::Chat, body, false), &anonymous())
            .await
            .expect("third attempt should succeed");
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(*hits.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn persistent_429_stops_after_the_attempt_limit() {
        let (base, hits) = spawn_flaky(usize::MAX, 200).await;
        let adapter = Opencode::new(reqwest::Client::new(), Some(base));
        let body = serde_json::json!({"model":"beam-free","messages":[]});
        let failure = match adapter
            .send(&request(Inbound::Chat, body, false), &anonymous())
            .await
        {
            Ok(_) => panic!("expected a quota failure"),
            Err(failure) => failure,
        };
        assert!(matches!(failure, Failure::Quota { .. }));
        assert_eq!(*hits.lock().unwrap(), RETRY_ATTEMPTS as usize);
    }

    #[test]
    fn free_usage_error_is_recognised() {
        let body = br#"{"type":"error","error":{"type":"FreeUsageLimitError","message":"x"}}"#;
        assert!(names_error(body, "FreeUsageLimitError"));
        assert!(!names_error(
            br#"{"error":{"type":"other"}}"#,
            "FreeUsageLimitError"
        ));
    }
}
