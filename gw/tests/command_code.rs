//! Command Code adapter egress and reply translation against a fake upstream.

use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::{Request as AxumRequest, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::any,
    Router,
};
use bytes::Bytes;
use futures_util::StreamExt;
use gw::{
    provider::{Account, Credential, Failure, Inbound, Provider, Request},
    providers::command_code::CommandCode,
};
use serde_json::{json, Value};
use tokio::sync::oneshot;

struct Captured {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Bytes,
}

struct Fake {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    retry_after: Option<&'static str>,
}

impl Fake {
    fn ndjson(lines: &[&str]) -> Self {
        Self {
            status: 200,
            content_type: "application/x-ndjson",
            body: lines.join("\n").into_bytes(),
            retry_after: None,
        }
    }

    fn error(status: u16, body: &str, retry_after: Option<&'static str>) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
            retry_after,
        }
    }
}

#[derive(Clone)]
struct FakeState {
    reply: Arc<Fake>,
    seen: Arc<Mutex<Option<oneshot::Sender<Captured>>>>,
}

async fn handler(State(state): State<FakeState>, request: AxumRequest) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 4 * 1024 * 1024)
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
    if let Some(sender) = state.seen.lock().unwrap().take() {
        let _ = sender.send(Captured {
            method: parts.method.to_string(),
            path: parts.uri.path().to_owned(),
            headers,
            body,
        });
    }
    let mut response = Response::builder()
        .status(StatusCode::from_u16(state.reply.status).unwrap())
        .header("content-type", state.reply.content_type);
    if let Some(retry_after) = state.reply.retry_after {
        response = response.header("retry-after", retry_after);
    }
    response.body(Body::from(state.reply.body.clone())).unwrap()
}

async fn upstream(reply: Fake) -> (String, oneshot::Receiver<Captured>) {
    let (sender, receiver) = oneshot::channel();
    let state = FakeState {
        reply: Arc::new(reply),
        seen: Arc::new(Mutex::new(Some(sender))),
    };
    let app = Router::new().fallback(any(handler)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), receiver)
}

fn test_provider(base: &str) -> CommandCode {
    CommandCode::new(reqwest::Client::new(), Some(base.to_owned()))
}

fn test_account() -> Account {
    Account {
        id: "a1".into(),
        credential: Credential::ApiKey("cmd_test".into()),
        extra: Default::default(),
    }
}

fn test_request(inbound: Inbound, body: Value, stream: bool) -> Request {
    Request {
        inbound,
        model: "deepseek/deepseek-v4.1-flash".into(),
        headers: HeaderMap::new(),
        body: Bytes::from(serde_json::to_vec(&body).unwrap()),
        stream,
        session: "sess-1".into(),
        fast: None,
    }
}

async fn collected(reply: gw::provider::Reply) -> Vec<u8> {
    reply
        .body
        .fold(Vec::new(), |mut all, item| async move {
            all.extend_from_slice(&item.unwrap());
            all
        })
        .await
}

fn responses_body(stream: bool) -> Value {
    json!({
        "model": "command-code/deepseek/deepseek-v4.1-flash",
        "instructions": "Be terse",
        "input": [
            { "type": "message", "role": "user", "content": [
                { "type": "input_text", "text": "hi" },
                { "type": "input_image", "image_url": "data:image/png;base64,AAAA" }
            ]},
            { "type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}" },
            { "type": "function_call_output", "call_id": "call_1", "output": "body" }
        ],
        "tools": [{ "type": "function", "name": "read", "description": "Read", "parameters": { "type": "object" } }],
        "max_output_tokens": 128,
        "temperature": 0.3,
        "reasoning": { "effort": "high" },
        "stream": stream,
    })
}

async fn send(
    reply: Fake,
    inbound: Inbound,
    body: Value,
    stream: bool,
) -> (Result<gw::provider::Reply, Failure>, Captured) {
    let (base, seen) = upstream(reply).await;
    let provider = test_provider(&base);
    let request = test_request(inbound, body, stream);
    let account = test_account();
    let (sent, received) = tokio::join!(provider.send(&request, &account), seen);
    (sent, received.expect("upstream request"))
}

#[tokio::test]
async fn sends_the_exact_command_code_request() {
    let (result, seen) = send(
        Fake::ndjson(&[]),
        Inbound::Responses,
        responses_body(true),
        true,
    )
    .await;
    let reply = result.expect("reply");
    assert_eq!(reply.status, 200);
    let _ = collected(reply).await;

    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/alpha/generate");
    let names: Vec<&str> = seen.headers.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "authorization",
            "content-type",
            "user-agent",
            "x-command-code-version",
            "x-cli-environment",
            "x-taste-learning",
            "x-co-flag",
            "x-session-id",
            "x-project-slug",
            "accept",
            "host",
            "content-length",
        ]
    );
    let header = |name: &str| {
        seen.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    assert_eq!(header("authorization"), "Bearer cmd_test");
    assert_eq!(header("content-type"), "application/json");
    assert_eq!(header("user-agent"), "cli");
    assert_eq!(header("x-command-code-version"), "0.52.1");
    assert_eq!(header("x-cli-environment"), "production");
    assert_eq!(header("x-taste-learning"), "false");
    assert_eq!(header("x-co-flag"), "false");
    let session = header("x-session-id");
    assert_eq!(session.len(), 36, "{session}");
    assert_eq!(&session[14..15], "4");
    assert_eq!(&session[19..20], "8");
    let slug = header("x-project-slug");
    assert!(!slug.is_empty());
    assert!(slug
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'));

    let body: Value = serde_json::from_slice(&seen.body).unwrap();
    let top: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(
        top,
        vec![
            "config",
            "memory",
            "taste",
            "skills",
            "permissionMode",
            "mode",
            "params"
        ]
    );
    assert_eq!(body["memory"], "");
    assert_eq!(body["taste"], Value::Null);
    assert_eq!(body["skills"], Value::Null);
    assert_eq!(body["permissionMode"], "standard");
    assert_eq!(body["mode"], "agent");

    let config = body["config"].as_object().unwrap();
    let keys: Vec<&String> = config.keys().collect();
    assert_eq!(
        keys,
        vec![
            "workingDir",
            "date",
            "environment",
            "structure",
            "isGitRepo",
            "currentBranch",
            "mainBranch",
            "gitStatus",
            "recentCommits"
        ]
    );
    assert_eq!(config["environment"], "linux");
    assert!(config["structure"].is_array());
    assert!(config["recentCommits"].is_array());
    let date = config["date"].as_str().unwrap();
    assert_eq!(date.len(), 10, "{date}");

    let params = body["params"].as_object().unwrap();
    let keys: Vec<&String> = params.keys().collect();
    assert_eq!(
        keys,
        vec![
            "model",
            "messages",
            "tools",
            "system",
            "max_tokens",
            "stream",
            "temperature",
            "reasoning_effort"
        ]
    );
    assert_eq!(params["model"], "deepseek/deepseek-v4.1-flash");
    assert_eq!(params["system"], "Be terse");
    assert_eq!(params["max_tokens"], 128);
    assert_eq!(params["stream"], true);
    assert_eq!(params["temperature"], 0.3);
    assert_eq!(params["reasoning_effort"], "high");
    assert_eq!(
        params["tools"],
        json!([{ "name": "read", "description": "Read", "input_schema": { "type": "object" } }])
    );
    assert_eq!(
        params["messages"],
        json!([
            { "role": "user", "content": [
                { "type": "text", "text": "hi" },
                { "type": "image", "image": "data:image/png;base64,AAAA", "mediaType": "image/png" }
            ]},
            { "role": "assistant", "content": [
                { "type": "tool-call", "toolCallId": "call_1", "toolName": "read", "input": { "path": "a" } }
            ]},
            { "role": "tool", "content": [{
                "type": "tool-result", "toolCallId": "call_1", "toolName": "read",
                "output": { "type": "text", "value": "body" }
            }] }
        ])
    );
}

#[tokio::test]
async fn keeps_session_id_stable_and_omits_unsupported_effort() {
    let mut body = responses_body(false);
    body["reasoning"] = json!({ "effort": "ultra" });
    let (result, first) = send(Fake::ndjson(&[]), Inbound::Responses, body, false).await;
    let _ = collected(result.expect("reply")).await;

    let (result, second) = send(
        Fake::ndjson(&[]),
        Inbound::Responses,
        responses_body(false),
        false,
    )
    .await;
    let _ = collected(result.expect("reply")).await;
    let first_body: Value = serde_json::from_slice(&first.body).unwrap();
    assert!(first_body["params"].get("reasoning_effort").is_none());
    let second_body: Value = serde_json::from_slice(&second.body).unwrap();
    assert_eq!(second_body["params"]["reasoning_effort"], "high");

    let session = |seen: &Captured| {
        seen.headers
            .iter()
            .find(|(name, _)| name == "x-session-id")
            .map(|(_, value)| value.clone())
            .unwrap()
    };
    assert_eq!(session(&first), session(&second));
}

const STREAM: &[&str] = &[
    r#"{"type":"reasoning-delta","text":"think"}"#,
    r#"{"type":"text-delta","text":"Hel"}"#,
    r#"{"type":"text-delta","text":"lo"}"#,
    r#"{"type":"tool-call","toolCallId":"call_9","toolName":"read","input":{"path":"a"}}"#,
    r#"{"type":"finish-step","finishReason":"tool-calls","usage":{"inputTokens":12,"outputTokens":5,"inputTokenDetails":{"cacheReadTokens":3}}}"#,
];

#[tokio::test]
async fn streams_ndjson_as_responses_events() {
    let (result, _seen) = send(
        Fake::ndjson(STREAM),
        Inbound::Responses,
        responses_body(true),
        true,
    )
    .await;
    let reply = result.expect("reply");
    assert_eq!(reply.status, 200);
    assert_eq!(reply.headers["content-type"], "text/event-stream");
    let text = String::from_utf8(collected(reply).await).unwrap();

    assert!(text.contains("event: response.created\n"));
    assert!(text.contains(r#""delta":"Hel""#), "{text}");
    assert!(text.contains(r#""delta":"lo""#));
    assert!(text.contains("event: response.function_call_arguments.delta\n"));
    assert!(text.contains(r#""arguments":"{\"path\":\"a\"}""#), "{text}");
    assert!(text.contains(r#""input_tokens":12"#), "{text}");
    assert!(text.contains(r#""output_tokens":5"#));
    assert!(text.contains(r#""cached_tokens":3"#));
    assert!(text.contains("event: response.completed\n"));
    assert!(text.ends_with("data: [DONE]\n\n"));
}

#[tokio::test]
async fn streams_ndjson_as_messages_events() {
    let body = json!({
        "model": "command-code/deepseek/deepseek-v4.1-flash",
        "max_tokens": 64,
        "stream": true,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let (result, _seen) = send(Fake::ndjson(STREAM), Inbound::Messages, body, true).await;
    let text = String::from_utf8(collected(result.expect("reply")).await).unwrap();
    assert!(text.contains("event: message_start\n"), "{text}");
    assert!(text.contains(r#""text":"Hel""#));
    assert!(text.contains(r#""input_tokens":9"#), "{text}");
    assert!(text.contains(r#""cache_read_input_tokens":3"#));
    assert!(text.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
}

#[tokio::test]
async fn streams_ndjson_as_chat_chunks() {
    let body = json!({
        "model": "deepseek/deepseek-v4.1-flash",
        "stream": true,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let (result, _seen) = send(Fake::ndjson(STREAM), Inbound::Chat, body, true).await;
    let reply = result.expect("reply");
    assert_eq!(reply.headers["content-type"], "text/event-stream");
    let text = String::from_utf8(collected(reply).await).unwrap();
    assert!(text.contains(r#""content":"Hel""#), "{text}");
    assert!(text.contains(r#""reasoning_content":"think""#));
    assert!(text.contains(r#""tool_calls":[{"index":0"#));
    assert!(text.contains(r#""finish_reason":"tool_calls""#));
    assert!(text.ends_with("data: [DONE]\n\n"));
}

#[tokio::test]
async fn buffers_responses_reply() {
    let (result, _seen) = send(
        Fake::ndjson(STREAM),
        Inbound::Responses,
        responses_body(false),
        false,
    )
    .await;
    let reply = result.expect("reply");
    assert_eq!(reply.headers["content-type"], "application/json");
    let body: Value = serde_json::from_slice(&collected(reply).await).unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["model"], "deepseek/deepseek-v4.1-flash");
    assert_eq!(body["output"][0]["content"][0]["text"], "Hello");
    assert_eq!(body["output"][1]["type"], "function_call");
    assert_eq!(body["output"][1]["call_id"], "call_9");
    assert_eq!(body["output"][1]["arguments"], r#"{"path":"a"}"#);
    assert_eq!(body["usage"]["input_tokens"], 12);
    assert_eq!(body["usage"]["output_tokens"], 5);
}

#[tokio::test]
async fn buffers_messages_and_chat_replies() {
    let body = json!({
        "model": "command-code/deepseek/deepseek-v4.1-flash",
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let (result, _seen) = send(Fake::ndjson(STREAM), Inbound::Messages, body, false).await;
    let reply: Value = serde_json::from_slice(&collected(result.expect("reply")).await).unwrap();
    assert_eq!(reply["type"], "message");
    assert_eq!(reply["content"][0]["text"], "Hello");
    assert_eq!(reply["stop_reason"], "tool_use");

    let body = json!({
        "model": "deepseek/deepseek-v4.1-flash",
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let (result, _seen) = send(Fake::ndjson(STREAM), Inbound::Chat, body, false).await;
    let reply: Value = serde_json::from_slice(&collected(result.expect("reply")).await).unwrap();
    assert_eq!(reply["object"], "chat.completion");
    assert_eq!(reply["choices"][0]["message"]["content"], "Hello");
    assert_eq!(
        reply["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "read"
    );
    assert_eq!(reply["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(reply["usage"]["total_tokens"], 17);
}

#[tokio::test]
async fn maps_upstream_failures() {
    let cases: Vec<(u16, &str, Option<&str>, Failure)> = vec![
        (401, "", None, Failure::Auth),
        (403, "", None, Failure::Auth),
        (
            429,
            "slow down",
            Some("7"),
            Failure::RateLimited {
                retry_after: Some(std::time::Duration::from_secs(7)),
            },
        ),
        (
            402,
            "payment required",
            None,
            Failure::Quota { reset_after: None },
        ),
        (
            429,
            "quota exhausted",
            None,
            Failure::Quota { reset_after: None },
        ),
        (500, "boom", None, Failure::Retryable),
        (503, "down", None, Failure::Retryable),
    ];
    for (status, body, retry_after, expected) in cases {
        let (result, _seen) = send(
            Fake::error(status, body, retry_after),
            Inbound::Chat,
            json!({ "model": "deepseek/deepseek-v4.1-flash", "messages": [] }),
            false,
        )
        .await;
        let failure = match result {
            Ok(_) => panic!("expected a failure for status {status}"),
            Err(failure) => failure,
        };
        match (failure, expected) {
            (Failure::Auth, Failure::Auth) => {}
            (
                Failure::RateLimited { retry_after },
                Failure::RateLimited {
                    retry_after: expected,
                },
            ) => assert_eq!(retry_after, expected),
            (
                Failure::Quota { reset_after },
                Failure::Quota {
                    reset_after: expected,
                },
            ) => {
                assert_eq!(reset_after, expected)
            }
            (Failure::Retryable, Failure::Retryable) => {}
            (Failure::Fatal { status, body }, Failure::Fatal { status: want, .. }) => {
                assert_eq!(status, want);
                assert!(!body.is_empty());
            }
            other => panic!("unexpected mapping: {other:?} for status {status}"),
        }
    }
}

#[tokio::test]
async fn maps_other_4xx_to_fatal_with_body() {
    let (result, _seen) = send(
        Fake::error(400, r#"{"error":"bad tool"}"#, None),
        Inbound::Chat,
        json!({ "model": "deepseek/deepseek-v4.1-flash", "messages": [] }),
        false,
    )
    .await;
    let failure = match result {
        Ok(_) => panic!("expected a failure"),
        Err(failure) => failure,
    };
    match failure {
        Failure::Fatal { status, body } => {
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body.as_ref(), br#"{"error":"bad tool"}"#);
        }
        other => panic!("unexpected mapping: {other:?}"),
    }
}

#[tokio::test]
async fn stream_error_becomes_an_error_event() {
    let lines = [
        r#"{"type":"text-delta","text":"Hel"}"#,
        r#"{"type":"error","error":{"message":"upstream exploded"}}"#,
    ];
    let (result, _seen) = send(
        Fake::ndjson(&lines),
        Inbound::Responses,
        responses_body(true),
        true,
    )
    .await;
    let text = String::from_utf8(collected(result.expect("reply")).await).unwrap();
    assert!(text.contains("event: error\n"), "{text}");
    assert!(text.contains("upstream exploded"));
    assert!(!text.contains("response.completed"));
}
