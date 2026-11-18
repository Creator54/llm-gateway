//! openai-api adapter through a fake upstream: endpoints, auth, translation, error mapping.

use std::sync::{Arc, Mutex};

use axum::{
    body::Body, extract::State, http::StatusCode, response::Response, routing::any, Router,
};
use futures_util::StreamExt;
use gw::{
    provider::{Account, Credential, Failure, Inbound, Provider, Request},
    providers::openai_api::OpenAiApi,
};
use http::HeaderMap;
use serde_json::{json, Value};

#[derive(Clone)]
struct Upstream {
    status: u16,
    body: &'static str,
    seen: Arc<Mutex<Vec<Seen>>>,
}

#[derive(Clone, Default)]
struct Seen {
    path: String,
    auth: String,
    body: Value,
}

async fn answer(State(state): State<Upstream>, request: axum::extract::Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    state.seen.lock().unwrap().push(Seen {
        path: parts.uri.path().to_owned(),
        auth: parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    });
    Response::builder()
        .status(state.status)
        .header("content-type", "application/json")
        .header("x-request-id", "req_1")
        .header("set-cookie", "internal=1")
        .body(Body::from(state.body))
        .unwrap()
}

const CHAT: &str = r#"{"id":"c1","object":"chat.completion","model":"gpt-5.5","choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}}"#;

async fn spawn(status: u16, body: &'static str) -> (OpenAiApi, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new().fallback(any(answer)).with_state(Upstream {
        status,
        body,
        seen: seen.clone(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // The base carries the version segment, as the real endpoint does.
    let adapter = OpenAiApi::new(reqwest::Client::new(), Some(format!("http://{addr}/v1/")));
    (adapter, seen)
}

fn request(inbound: Inbound, body: Value, stream: bool, fast: bool) -> Request {
    Request {
        inbound,
        model: "gpt-5.5".into(),
        headers: HeaderMap::new(),
        body: bytes::Bytes::from(serde_json::to_vec(&body).unwrap()),
        stream,
        session: "s1".into(),
        fast: fast.then_some(true),
    }
}

fn keyed() -> Account {
    Account {
        id: "k".into(),
        credential: Credential::ApiKey("sk-test".into()),
        extra: Default::default(),
    }
}

async fn failure(adapter: &OpenAiApi, req: Request) -> Failure {
    match adapter.send(&req, &keyed()).await {
        Ok(_) => panic!("expected a failure"),
        Err(failure) => failure,
    }
}

async fn text(reply: gw::provider::Reply) -> String {
    let parts = reply.body.collect::<Vec<_>>().await;
    String::from_utf8(parts.into_iter().flatten().flatten().collect()).unwrap()
}

#[tokio::test]
async fn chat_goes_to_the_api_with_the_key_and_the_routed_model() {
    let (adapter, seen) = spawn(200, CHAT).await;
    let body = json!({"model":"openai-api/gpt-5.5","messages":[{"role":"user","content":"hi"}]});
    let reply = adapter
        .send(&request(Inbound::Chat, body, false, false), &keyed())
        .await
        .expect("reply");
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["x-request-id"], "req_1");
    assert!(!reply.headers.contains_key("set-cookie"));
    assert!(text(reply).await.contains("pong"));

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].path, "/v1/chat/completions");
    assert_eq!(seen[0].auth, "Bearer sk-test");
    assert_eq!(seen[0].body["model"], "gpt-5.5");
    assert!(seen[0].body.get("service_tier").is_none());
}

#[tokio::test]
async fn tools_with_reasoning_effort_go_through_responses() {
    let responses = r#"{"id":"resp_1","object":"response","status":"completed","model":"gpt-6.2-sol","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"pong"}]}],"usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}"#;
    let (adapter, seen) = spawn(200, responses).await;
    let body = json!({
        "model": "gpt-6.2-sol",
        "reasoning_effort": "medium",
        "max_tokens": 16,
        "tools": [{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
        "messages": [{"role":"user","content":"hi"}]
    });
    let reply = adapter
        .send(&request(Inbound::Chat, body, false, false), &keyed())
        .await
        .expect("reply");
    let chat: serde_json::Value = serde_json::from_str(&text(reply).await).unwrap();
    assert_eq!(chat["object"], "chat.completion");
    assert_eq!(chat["choices"][0]["message"]["content"], "pong");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].path, "/v1/responses");
    assert_eq!(seen[0].body["reasoning"]["effort"], "medium");
    assert_eq!(seen[0].body["max_output_tokens"], 16);
    assert_eq!(seen[0].body["tools"][0]["name"], "f");
}

#[tokio::test]
async fn tools_without_reasoning_stay_on_chat_completions() {
    let (adapter, seen) = spawn(200, CHAT).await;
    let body = json!({
        "model": "gpt-5.5",
        "tools": [{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
        "messages": [{"role":"user","content":"hi"}]
    });
    adapter
        .send(&request(Inbound::Chat, body, false, false), &keyed())
        .await
        .expect("reply");
    assert_eq!(seen.lock().unwrap()[0].path, "/v1/chat/completions");
}

#[tokio::test]
async fn max_tokens_becomes_max_completion_tokens() {
    let (adapter, seen) = spawn(200, CHAT).await;
    let body = json!({"model":"gpt-6.2-sol","max_tokens":16,"messages":[]});
    adapter
        .send(&request(Inbound::Chat, body, false, false), &keyed())
        .await
        .expect("reply");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].body["max_completion_tokens"], 16);
    assert!(seen[0].body.get("max_tokens").is_none());
}

#[tokio::test]
async fn chat_streams_ask_for_usage_and_fast_asks_for_priority() {
    let (adapter, seen) = spawn(200, CHAT).await;
    let body = json!({"model":"gpt-5.5","stream":true,"messages":[]});
    let reply = adapter
        .send(&request(Inbound::Chat, body, true, true), &keyed())
        .await
        .expect("reply");
    assert_eq!(reply.headers["content-type"], "text/event-stream");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].body["stream_options"]["include_usage"], true);
    assert_eq!(seen[0].body["service_tier"], "priority");
}

#[tokio::test]
async fn responses_requests_use_the_responses_endpoint_unchanged() {
    let (adapter, seen) = spawn(200, r#"{"id":"r1","object":"response"}"#).await;
    let body = json!({"model":"x","input":"hi","stream":false});
    adapter
        .send(&request(Inbound::Responses, body, false, false), &keyed())
        .await
        .expect("reply");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].path, "/v1/responses");
    assert_eq!(seen[0].body["input"], "hi");
    assert_eq!(seen[0].body["model"], "gpt-5.5");
    assert!(seen[0].body.get("stream_options").is_none());
}

#[tokio::test]
async fn messages_requests_are_translated_to_chat_and_back() {
    let (adapter, seen) = spawn(200, CHAT).await;
    let body = json!({"model":"x","max_tokens":16,"messages":[{"role":"user","content":"hi"}]});
    let reply = adapter
        .send(&request(Inbound::Messages, body, false, false), &keyed())
        .await
        .expect("reply");
    let answer: Value = serde_json::from_str(&text(reply).await).unwrap();
    assert_eq!(answer["type"], "message");
    assert_eq!(answer["content"][0]["text"], "pong");
    assert_eq!(seen.lock().unwrap()[0].path, "/v1/chat/completions");
}

#[tokio::test]
async fn accounts_without_a_key_are_an_auth_failure() {
    let (adapter, seen) = spawn(200, CHAT).await;
    let body = json!({"messages":[]});
    for credential in [Credential::None, Credential::ApiKey(String::new())] {
        let account = Account {
            credential,
            ..keyed()
        };
        let result = adapter
            .send(
                &request(Inbound::Chat, body.clone(), false, false),
                &account,
            )
            .await;
        assert!(matches!(result, Err(Failure::Auth)));
    }
    assert!(seen.lock().unwrap().is_empty(), "no request without a key");
}

#[tokio::test]
async fn upstream_errors_map_to_failover_decisions() {
    let body = json!({"messages":[]});
    let chat = || request(Inbound::Chat, body.clone(), false, false);

    let (adapter, _) = spawn(401, r#"{"error":{"code":"invalid_api_key"}}"#).await;
    assert_eq!(failure(&adapter, chat()).await, Failure::Auth);

    let (adapter, _) = spawn(429, r#"{"error":{"code":"insufficient_quota"}}"#).await;
    assert!(matches!(
        failure(&adapter, chat()).await,
        Failure::Quota { .. }
    ));

    let (adapter, _) = spawn(429, r#"{"error":{"code":"rate_limit_exceeded"}}"#).await;
    assert!(matches!(
        failure(&adapter, chat()).await,
        Failure::RateLimited { .. }
    ));

    let (adapter, _) = spawn(503, "{}").await;
    assert_eq!(failure(&adapter, chat()).await, Failure::Retryable);

    // A model the key cannot use is the request's problem; the account stays usable.
    let (adapter, _) = spawn(403, r#"{"error":{"code":"model_not_found"}}"#).await;
    assert!(matches!(
        failure(&adapter, chat()).await,
        Failure::Fatal {
            status: StatusCode::FORBIDDEN,
            ..
        }
    ));
    let (adapter, _) = spawn(400, r#"{"error":{"message":"bad"}}"#).await;
    assert!(matches!(
        failure(&adapter, chat()).await,
        Failure::Fatal {
            status: StatusCode::BAD_REQUEST,
            ..
        }
    ));
}
