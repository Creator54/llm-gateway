//! Egress and translation tests against an in-process fake CCA upstream.

use super::*;
use crate::provider::{Account, Credential, Inbound as Wire, Request};
use axum::{extract::State, response::Response, routing::any, Router};
use futures_util::TryStreamExt;
use http::HeaderMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Fake {
    status: u16,
    body: String,
    content_type: &'static str,
    extra_headers: Vec<(&'static str, &'static str)>,
    seen: Arc<Mutex<Option<Seen>>>,
}

#[derive(Clone)]
struct Seen {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Value,
}

async fn handler(State(fake): State<Fake>, req: axum::extract::Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 22)
        .await
        .unwrap_or_default();
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    *fake.seen.lock().unwrap() = Some(Seen {
        method: parts.method.as_str().to_owned(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or_default().to_owned(),
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    });
    let mut response = Response::new(axum::body::Body::from(fake.body.clone()));
    *response.status_mut() = StatusCode::from_u16(fake.status).unwrap();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(fake.content_type),
    );
    for (name, value) in fake.extra_headers {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

async fn upstream(status: u16, body: &str, content_type: &'static str) -> (String, Fake) {
    upstream_with(status, body, content_type, Vec::new()).await
}

async fn upstream_with(
    status: u16,
    body: &str,
    content_type: &'static str,
    extra_headers: Vec<(&'static str, &'static str)>,
) -> (String, Fake) {
    let seen = Arc::new(Mutex::new(None));
    let fake = Fake {
        status,
        body: body.to_owned(),
        content_type,
        extra_headers,
        seen: seen.clone(),
    };
    let app = Router::new()
        .fallback(any(handler))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), Fake { seen, ..fake })
}

fn account() -> Account {
    let mut extra = std::collections::BTreeMap::new();
    extra.insert("project".to_owned(), "proj-1".to_owned());
    Account {
        id: "a1".into(),
        credential: Credential::OAuth {
            access: "tok".into(),
            refresh: None,
            expires_at: None,
        },
        extra,
    }
}

fn request(inbound: Wire, stream: bool, body: Value) -> Request {
    Request {
        inbound,
        model: "gemini-3.1-pro".into(),
        headers: HeaderMap::new(),
        body: Bytes::from(body.to_string()),
        stream,
        session: "s1".into(),
    }
}

async fn reply_body(reply: Reply) -> String {
    let bytes: Vec<Bytes> = reply.body.try_collect().await.unwrap();
    String::from_utf8(bytes.concat()).unwrap()
}

fn header_of(seen: &Seen, name: &str) -> Option<String> {
    seen.headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

#[tokio::test]
async fn egress_matches_the_envelope_shape() {
    let (base, fake) = upstream(
        200,
        r#"{"response":{"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}}}"#,
        "application/json",
    )
    .await;
    let provider = Antigravity::new(reqwest::Client::new(), Some(base));
    let body = json!({
        "model": "gemini-3.1-pro",
        "max_tokens": 90000,
        "temperature": 3,
        "stop": ["a", "a", "b"],
        "messages": [
            { "role": "system", "content": "Be terse" },
            { "role": "user", "content": "hello" }
        ],
        "tools": [{ "type": "function", "function": {
            "name": "mcp__fs__read", "description": "Read", "parameters": { "type": "object" }
        }}]
    });
    let reply = provider
        .send(&request(Wire::Chat, false, body), &account())
        .await
        .unwrap();
    let seen = fake.seen.lock().unwrap().as_ref().cloned().unwrap();

    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/v1internal:generateContent");
    assert_eq!(seen.query, "");
    assert_eq!(
        header_of(&seen, "content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(header_of(&seen, "user-agent").as_deref(), Some(USER_AGENT));
    assert_eq!(
        header_of(&seen, "authorization").as_deref(),
        Some("Bearer tok")
    );
    // The adapter sets no Accept; the HTTP client sends `*/*`, exactly as Bun's fetch does
    // for opencodex, so the recorded bytes still line up.
    assert_eq!(header_of(&seen, "accept").as_deref(), Some("*/*"));
    assert!(!seen
        .headers
        .iter()
        .any(|(name, _)| name == "x-goog-api-client" || name.starts_with("anthropic-")));

    let keys: Vec<&String> = seen.body.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "model",
            "userAgent",
            "requestType",
            "project",
            "requestId",
            "request"
        ]
    );
    assert_eq!(seen.body["model"], "gemini-3.1-pro");
    assert_eq!(seen.body["userAgent"], "antigravity");
    assert_eq!(seen.body["requestType"], "agent");
    assert_eq!(seen.body["project"], "proj-1");
    assert!(seen.body["requestId"]
        .as_str()
        .unwrap()
        .starts_with("agent-"));

    let inner = seen.body["request"].as_object().unwrap();
    let inner_keys: Vec<&String> = inner.keys().collect();
    assert_eq!(
        inner_keys,
        [
            "contents",
            "systemInstruction",
            "tools",
            "generationConfig",
            "sessionId"
        ]
    );
    assert!(inner["systemInstruction"]["parts"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("Be terse\n\nOutput style"));
    assert_eq!(inner["contents"][0]["role"], "user");
    assert_eq!(inner["contents"][0]["parts"][0]["text"], "hello");
    assert_eq!(
        inner["tools"][0]["functionDeclarations"][0]["name"],
        "mcp__fs__read"
    );
    assert_eq!(inner["generationConfig"]["maxOutputTokens"], 65535);
    assert_eq!(inner["generationConfig"]["temperature"], 2.0);
    assert_eq!(
        inner["generationConfig"]["stopSequences"],
        json!(["a", "b"])
    );
    assert!(inner["sessionId"].as_str().unwrap().starts_with('-'));
    assert!(inner.get("toolConfig").is_none());

    // Non-streaming Chat reply: text and usage from the `response` wrapper.
    let reply = reply_body(reply).await;
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["object"], "chat.completion");
    assert_eq!(reply["choices"][0]["message"]["content"], "hi");
    assert_eq!(reply["choices"][0]["finish_reason"], "stop");
    assert_eq!(reply["usage"]["prompt_tokens"], 3);
    assert_eq!(reply["usage"]["completion_tokens"], 2);
}

#[tokio::test]
async fn streaming_translates_frames_into_responses_events() {
    let sse = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}]}}]}}\n\n\
data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"lo\"}]}}]}}\n\n\
data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"a\"},\"id\":\"t1\"}}]},\"finishReason\":\"STOP\"}],\
\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":5}}}\n\n";
    let (base, fake) = upstream(200, sse, "text/event-stream").await;
    let provider = Antigravity::new(reqwest::Client::new(), Some(base));
    let body = json!({
        "stream": true,
        "input": [
            { "type": "message", "role": "user", "content": "go" },
            { "type": "function_call", "call_id": "c1", "name": "read", "arguments": "{}" },
            { "type": "function_call_output", "call_id": "c1", "output": "file" }
        ],
        "tools": [{ "type": "function", "name": "read", "parameters": { "type": "object" } }]
    });
    let reply = provider
        .send(&request(Wire::Responses, true, body), &account())
        .await
        .unwrap();
    let seen = fake.seen.lock().unwrap().as_ref().cloned().unwrap();
    assert_eq!(seen.path, "/v1internal:streamGenerateContent");
    assert_eq!(seen.query, "alt=sse");
    assert_eq!(
        header_of(&seen, "content-type").as_deref(),
        Some("application/json")
    );

    let out = reply_body(reply).await;
    assert!(out.contains("event: response.created"), "{out}");
    assert!(out.contains("\"delta\":\"Hel\""));
    assert!(out.contains("\"delta\":\"lo\""));
    assert!(out.contains("response.function_call_arguments.delta"));
    assert!(out.contains("\"name\":\"read\""));
    assert!(out.contains("\"call_id\":\"call_"));
    assert!(out.contains("\"output_tokens\":5"));
    assert!(out.ends_with("data: [DONE]\n\n"));
}

#[tokio::test]
async fn messages_inbound_maps_request_and_reply() {
    let (base, fake) = upstream(
        200,
        r#"{"response":{"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":1}}}"#,
        "application/json",
    )
    .await;
    let provider = Antigravity::new(reqwest::Client::new(), Some(base));
    let body = json!({
        "max_tokens": 10,
        "system": "Be terse",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let reply = provider
        .send(&request(Wire::Messages, false, body), &account())
        .await
        .unwrap();
    let seen = fake.seen.lock().unwrap().as_ref().cloned().unwrap();
    assert_eq!(
        seen.body["request"]["generationConfig"]["maxOutputTokens"],
        10
    );
    let reply: Value = serde_json::from_str(&reply_body(reply).await).unwrap();
    assert_eq!(reply["type"], "message");
    assert_eq!(reply["content"][0]["text"], "done");
    assert_eq!(reply["stop_reason"], "end_turn");
    assert_eq!(reply["usage"]["input_tokens"], 4);
}

#[tokio::test]
async fn claude_models_force_validated_function_calling() {
    let (base, fake) = upstream(
        200,
        r#"{"response":{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}]}}"#,
        "application/json",
    )
    .await;
    let provider = Antigravity::new(reqwest::Client::new(), Some(base));
    let mut req = request(
        Wire::Chat,
        false,
        json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "type": "function", "function": { "name": "read" } }]
        }),
    );
    req.model = "claude-sonnet-4-6".into();
    provider.send(&req, &account()).await.unwrap();
    let seen = fake.seen.lock().unwrap().as_ref().cloned().unwrap();
    assert_eq!(
        seen.body["request"]["toolConfig"]["functionCallingConfig"]["mode"],
        "VALIDATED"
    );
}

#[tokio::test]
async fn responses_inbound_gets_a_response_object() {
    let (base, fake) = upstream(
        200,
        r#"{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"a"}}}]},
            "finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":3}}}"#,
        "application/json",
    )
    .await;
    let provider = Antigravity::new(reqwest::Client::new(), Some(base));
    let body = json!({
        "input": "hi",
        "parallel_tool_calls": false,
        "tools": [{ "type": "function", "name": "read", "parameters": { "type": "object" } }]
    });
    let mut req = request(Wire::Responses, false, body);
    req.headers
        .insert("x-codex-parent-thread-id", "parent".parse().unwrap());
    let reply = provider.send(&req, &account()).await.unwrap();
    let seen = fake.seen.lock().unwrap().as_ref().cloned().unwrap();
    // The thread header anchors the session id deterministically.
    let session = seen.body["request"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        session,
        super::util::session_id(Some("codex-thread:parent"))
    );

    let out = reply_body(reply).await;
    let reply: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(reply["object"], "response");
    assert_eq!(reply["status"], "completed");
    assert_eq!(reply["parallel_tool_calls"], false);
    assert_eq!(reply["output"][0]["type"], "function_call");
    assert_eq!(reply["output"][0]["name"], "read");
    assert_eq!(reply["usage"]["output_tokens"], 3);
}

#[tokio::test]
async fn streaming_messages_inbound_gets_message_events() {
    let sse = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":6,\"candidatesTokenCount\":1}}}\n\n";
    let (base, _fake) = upstream(200, sse, "text/event-stream").await;
    let provider = Antigravity::new(reqwest::Client::new(), Some(base));
    let body = json!({ "max_tokens": 5, "stream": true,
        "messages": [{ "role": "user", "content": "hi" }] });
    let reply = provider
        .send(&request(Wire::Messages, true, body), &account())
        .await
        .unwrap();
    let out = reply_body(reply).await;
    assert!(out.contains("event: message_start"), "{out}");
    assert!(out.contains("\"text\":\"ok\""));
    assert!(out.contains("event: message_stop"));
    assert!(out.contains("\"output_tokens\":1"));
}

#[tokio::test]
async fn maps_upstream_statuses_to_failures() {
    type Case = (
        u16,
        &'static str,
        Option<(&'static str, &'static str)>,
        &'static str,
    );
    let cases: Vec<Case> = vec![
        (
            429,
            r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"rate limit"}}"#,
            Some(("retry-after", "3")),
            "rate",
        ),
        (
            429,
            r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"Daily limit reached"}}"#,
            None,
            "quota",
        ),
        (401, "{}", None, "auth"),
        (
            403,
            r#"{"error":{"status":"PERMISSION_DENIED"}}"#,
            None,
            "auth",
        ),
        (500, "{}", None, "retry"),
        (400, r#"{"error":{"message":"bad"}}"#, None, "fatal"),
    ];
    for (status, body, extra, want) in cases {
        let extra = extra.into_iter().collect();
        let (base, _fake) = upstream_with(status, body, "application/json", extra).await;
        let provider = Antigravity::new(reqwest::Client::new(), Some(base));
        let failure = match provider
            .send(
                &request(
                    Wire::Chat,
                    false,
                    json!({ "messages": [{ "role": "user", "content": "hi" }] }),
                ),
                &account(),
            )
            .await
        {
            Err(failure) => failure,
            Ok(_) => panic!("expected a failure for {status}"),
        };
        match (want, &failure) {
            ("rate", Failure::RateLimited { retry_after }) => {
                assert_eq!(*retry_after, Some(Duration::from_secs(3)));
            }
            ("quota", Failure::Quota { reset_after }) => assert_eq!(*reset_after, None),
            ("auth", Failure::Auth) => {}
            ("retry", Failure::Retryable) => {}
            ("fatal", Failure::Fatal { status, body }) => {
                assert_eq!(*status, StatusCode::BAD_REQUEST);
                assert!(String::from_utf8_lossy(body).contains("bad"));
            }
            other => panic!("unexpected failure: {other:?}"),
        }
    }
}

#[tokio::test]
async fn missing_project_is_a_fatal_configuration_error() {
    let (base, _fake) = upstream(200, "{}", "application/json").await;
    let provider = Antigravity::new(reqwest::Client::new(), Some(base));
    let mut account = account();
    account.extra.clear();
    let failure = provider
        .send(
            &request(
                Wire::Chat,
                false,
                json!({ "messages": [{ "role": "user", "content": "hi" }] }),
            ),
            &account,
        )
        .await
        .err()
        .expect("missing project should fail");
    assert!(
        matches!(failure, Failure::Fatal { status, .. } if status == StatusCode::INTERNAL_SERVER_ERROR)
    );
}

#[tokio::test]
async fn overrides_base_url_and_reports_missing_wrapper() {
    let (base, _fake) = upstream(200, r#"{"candidates":[]}"#, "application/json").await;
    let provider = Antigravity::new(reqwest::Client::new(), Some(format!("{base}/")));
    let failure = provider
        .send(
            &request(
                Wire::Chat,
                false,
                json!({ "messages": [{ "role": "user", "content": "hi" }] }),
            ),
            &account(),
        )
        .await
        .err()
        .expect("missing wrapper should fail");
    match failure {
        Failure::Fatal { status, body } => {
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert!(String::from_utf8_lossy(&body).contains("missing response wrapper"));
        }
        other => panic!("unexpected failure: {other:?}"),
    }
}
