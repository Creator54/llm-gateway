//! End-to-end checks against an in-process fake Kiro runtime.

use super::eventstream::encode_message;
use super::wire::WireClient;
use super::*;
use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Captured {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct Upstream {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    captured: Mutex<Option<Captured>>,
}

async fn handler(State(state): State<Arc<Upstream>>, request: axum::extract::Request) -> Response {
    let (parts, body) = request.into_parts();
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
    *state.captured.lock().expect("capture") = Some(Captured {
        method: parts.method.as_str().to_owned(),
        path: parts.uri.path().to_owned(),
        headers,
        body: bytes.to_vec(),
    });
    let mut response = Response::builder().status(state.status);
    for (name, value) in &state.headers {
        response = response.header(name, value);
    }
    response
        .body(Body::from(state.body.clone()))
        .expect("response")
}

async fn upstream(
    status: u16,
    headers: Vec<(&str, &str)>,
    body: Vec<u8>,
) -> (String, Arc<Upstream>) {
    let state = Arc::new(Upstream {
        status,
        headers: headers
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        body,
        captured: Mutex::new(None),
    });
    let app = Router::new()
        .fallback(any(handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (format!("http://{address}/"), state)
}

/// One scripted reply: status, headers, body.
type Staged = (u16, Vec<(String, String)>, Vec<u8>);
type StagedInput<'a> = (u16, Vec<(&'a str, &'a str)>, Vec<u8>);

/// Fake upstream that answers with a scripted sequence of responses.
struct Scripted {
    replies: Mutex<Vec<Staged>>,
    captured: Mutex<Vec<Captured>>,
}

async fn scripted_handler(
    State(state): State<Arc<Scripted>>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
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
    state.captured.lock().expect("capture").push(Captured {
        method: parts.method.as_str().to_owned(),
        path: parts.uri.path().to_owned(),
        headers,
        body: bytes.to_vec(),
    });
    let reply = state.replies.lock().expect("replies").remove(0);
    let mut response = Response::builder().status(reply.0);
    for (name, value) in &reply.1 {
        response = response.header(name, value);
    }
    response.body(Body::from(reply.2)).expect("response")
}

async fn scripted(replies: Vec<StagedInput<'_>>) -> (String, Arc<Scripted>) {
    let state = Arc::new(Scripted {
        replies: Mutex::new(
            replies
                .into_iter()
                .map(|(status, headers, body)| {
                    (
                        status,
                        headers
                            .into_iter()
                            .map(|(name, value)| (name.to_owned(), value.to_owned()))
                            .collect(),
                        body,
                    )
                })
                .collect(),
        ),
        captured: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .fallback(any(scripted_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (format!("http://{address}/"), state)
}

fn account(extra: &[(&str, &str)], token: &str) -> Account {
    Account {
        id: "acct".into(),
        credential: Credential::ApiKey(token.to_owned()),
        extra: extra
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn request(inbound: Inbound, body: serde_json::Value, stream: bool) -> Request {
    Request {
        inbound,
        model: "kiro/claude-sonnet-4-5".into(),
        headers: HeaderMap::new(),
        body: Bytes::from(serde_json::to_vec(&body).expect("body")),
        stream,
        session: "session-1".into(),
        fast: None,
    }
}

fn provider(base_url: &str) -> Kiro {
    Kiro::new(reqwest::Client::new(), Some(base_url.to_owned()))
}

fn chat_body(stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": "kiro/claude-sonnet-4-5",
        "stream": stream,
        "messages": [
            { "role": "system", "content": "Be terse." },
            { "role": "user", "content": "Say hello." }
        ],
        "tools": [{ "type": "function", "function": {
            "name": "read_file",
            "description": "Read a file.",
            "parameters": { "type": "object", "properties": { "path": { "type": "string", "pattern": "x" } } }
        }}]
    })
}

fn text_frame(text: &str) -> Vec<u8> {
    let mut out = encode_message(
        &[
            (":message-type", "event"),
            (":event-type", "assistantResponseEvent"),
        ],
        format!(r#"{{"content":"{text}"}}"#).as_bytes(),
    );
    out.extend(encode_message(
        &[(":message-type", "event"), (":event-type", "metadataEvent")],
        br#"{"stopReason":"END_TURN","tokenUsage":{"uncachedInputTokens":7,"cacheReadInputTokens":2,"outputTokens":3,"totalTokens":12}}"#,
    ));
    out
}

#[tokio::test]
async fn cli_egress_matches_spec_and_streams_chat() {
    let (base, upstream) = upstream(
        200,
        vec![("content-type", "application/vnd.amazon.eventstream")],
        text_frame("Hello"),
    )
    .await;
    let reply = provider(&base)
        .send(
            &request(Inbound::Chat, chat_body(true), true),
            &account(&[], "aoa-token"),
        )
        .await
        .expect("reply");
    let captured = upstream
        .captured
        .lock()
        .expect("capture")
        .clone()
        .expect("request");
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/");

    let names: Vec<&str> = captured
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        names.starts_with(&[
            "authorization",
            "content-type",
            "accept",
            "x-amz-target",
            "user-agent",
            "x-amz-user-agent",
            "x-amzn-codewhisperer-optout",
        ]),
        "{names:?}"
    );
    let header = |name: &str| {
        captured
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or_default()
    };
    assert_eq!(header("accept"), "*/*");
    assert_eq!(
        header("x-amz-target"),
        "AmazonCodeWhispererStreamingService.GenerateAssistantResponse"
    );
    assert!(header("user-agent")
        .starts_with("aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererstreaming/0.1.17975 os/"));
    assert!(!header("x-amz-user-agent").contains("md/appVersion"));
    assert_eq!(header("amz-sdk-request"), "attempt=1; max=3");
    assert!(header("amz-sdk-invocation-id").len() == 36);
    assert_eq!(header("tokentype"), "");
    assert_eq!(header("x-amzn-kiro-profile-arn"), "");

    let body: Value = serde_json::from_slice(&captured.body).expect("json");
    let state = &body["conversationState"];
    assert_eq!(state["chatTriggerType"], "MANUAL");
    assert_eq!(state["agentTaskType"], "vibe");
    assert!(state["agentContinuationId"]
        .as_str()
        .is_some_and(|id| id.len() == 36));
    assert!(state.get("history").is_none());
    let user = &state["currentMessage"]["userInputMessage"];
    assert_eq!(user["modelId"], "claude-sonnet-4.5");
    assert_eq!(user["origin"], "KIRO_CLI");
    let content = user["content"].as_str().expect("content");
    assert!(
        content.starts_with("Be terse.\n\nTool contract:"),
        "{content}"
    );
    assert!(
        content.contains("When tools are available, ordinary assistant text"),
        "{content}"
    );
    assert!(content.ends_with("\n\nSay hello."), "{content}");
    assert!(!content.contains("Think in English"), "{content}");
    let tools = user["userInputMessageContext"]["tools"]
        .as_array()
        .expect("tools");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["toolSpecification"]["name"], "read_file");
    assert!(
        tools[0]["toolSpecification"]["inputSchema"]["json"]
            .get("properties")
            .is_some()
            && tools[0]["toolSpecification"]["inputSchema"]["json"]["properties"]["path"]
                .get("pattern")
                .is_none()
    );
    assert_eq!(
        tools[1]["toolSpecification"]["name"],
        "codex_kiro_final_answer"
    );
    assert!(body.get("profileArn").is_none());

    let mut bytes = Vec::new();
    let mut stream = reply.body;
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    let text = String::from_utf8(bytes).expect("utf8");
    assert!(text.contains("\"content\":\"Hello\""), "{text}");
    assert!(text.contains("\"finish_reason\":\"stop\""), "{text}");
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
    assert!(text.contains("\"prompt_tokens\":9"), "{text}");
}

#[tokio::test]
async fn ide_egress_uses_profile_arn_and_streams_responses() {
    let (base, upstream) = upstream(
        200,
        vec![("content-type", "application/vnd.amazon.eventstream")],
        text_frame("Hello"),
    )
    .await;
    let reply = provider(&base)
        .send(
            &request(
                Inbound::Responses,
                serde_json::json!({
                    "model": "kiro/claude-sonnet-4-5",
                    "instructions": "Be terse.",
                    "stream": true,
                    "input": "Say hello.",
                }),
                true,
            ),
            &account(
                &[("profile_arn", "arn:aws:codewhisperer:eu-west-1:1:profile/x")],
                "aoa-token",
            ),
        )
        .await
        .expect("reply");
    let captured = upstream
        .captured
        .lock()
        .expect("capture")
        .clone()
        .expect("request");
    let header = |name: &str| {
        captured
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or_default()
    };
    assert_eq!(header("accept"), "application/vnd.amazon.eventstream");
    assert_eq!(header("x-amzn-kiro-agent-mode"), "vibe");
    assert!(header("user-agent").starts_with("aws-sdk-js/1.0.27 ua/2.1 os/"));
    assert_eq!(
        header("x-amzn-kiro-profile-arn"),
        "arn:aws:codewhisperer:eu-west-1:1:profile/x"
    );
    // Transport headers (`content-length`, `host`) trail the request headers.
    let names: Vec<&str> = captured
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    let profile_at = names
        .iter()
        .position(|name| *name == "x-amzn-kiro-profile-arn")
        .expect("profile header");
    assert!(
        names[profile_at + 1..].iter().all(|name| matches!(
            *name,
            "content-length" | "host" | "accept-encoding" | "connection"
        )),
        "{names:?}"
    );
    let body: Value = serde_json::from_slice(&captured.body).expect("json");
    assert_eq!(
        body["conversationState"]["currentMessage"]["userInputMessage"]["origin"],
        "AI_EDITOR"
    );
    assert!(body["conversationState"].get("agentTaskType").is_none());
    assert_eq!(
        body["profileArn"],
        "arn:aws:codewhisperer:eu-west-1:1:profile/x"
    );

    let mut bytes = Vec::new();
    let mut stream = reply.body;
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    let text = String::from_utf8(bytes).expect("utf8");
    assert!(text.contains("response.output_text.delta"), "{text}");
    assert!(text.contains("\"delta\":\"Hello\""), "{text}");
    assert!(text.contains("response.completed"), "{text}");
}

#[tokio::test]
async fn non_stream_responses_reply_is_translated() {
    let (base, _) = upstream(
        200,
        vec![("content-type", "application/vnd.amazon.eventstream")],
        text_frame("Hello"),
    )
    .await;
    let reply = provider(&base)
        .send(
            &request(
                Inbound::Responses,
                serde_json::json!({
                    "model": "kiro/claude-sonnet-4-5",
                    "input": "Say hello.",
                }),
                false,
            ),
            &account(&[], "aoa-token"),
        )
        .await
        .expect("reply");
    let mut bytes = Vec::new();
    let mut stream = reply.body;
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["object"], "response");
    assert_eq!(body["output"][0]["content"][0]["text"], "Hello");
    assert_eq!(body["usage"]["input_tokens"], 9);
    assert_eq!(body["usage"]["output_tokens"], 3);
}

#[tokio::test]
async fn messages_reply_uses_anthropic_shape() {
    let (base, _) = upstream(
        200,
        vec![("content-type", "application/vnd.amazon.eventstream")],
        text_frame("Hello"),
    )
    .await;
    let reply = provider(&base)
        .send(
            &request(
                Inbound::Messages,
                serde_json::json!({
                    "model": "kiro/claude-sonnet-4-5",
                    "max_tokens": 100,
                    "messages": [{ "role": "user", "content": "Say hello." }],
                }),
                false,
            ),
            &account(&[], "aoa-token"),
        )
        .await
        .expect("reply");
    let mut bytes = Vec::new();
    let mut stream = reply.body;
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["text"], "Hello");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["input_tokens"], 7);
}

#[tokio::test]
async fn error_mapping_matches_the_contract() {
    type Case = (
        u16,
        Vec<(&'static str, &'static str)>,
        &'static str,
        Failure,
    );
    let cases: Vec<Case> = vec![
        (
            429,
            vec![("retry-after", "7")],
            r#"{"message":"Too many requests, please wait"}"#,
            Failure::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            },
        ),
        (
            429,
            vec![],
            r#"{"message":"You have exhausted your monthly quota"}"#,
            Failure::Quota { reset_after: None },
        ),
        (401, vec![], r#"{"message":"bad token"}"#, Failure::Auth),
        (
            403,
            vec![],
            r#"{"message":"AccessDeniedException"}"#,
            Failure::Auth,
        ),
        (
            403,
            vec![],
            r#"{"reason":"TEMPORARILY_SUSPENDED","message":"Account temporarily suspended"}"#,
            Failure::Quota {
                reset_after: Some(Duration::from_secs(24 * 60 * 60)),
            },
        ),
        (500, vec![], "boom", Failure::Retryable),
        (
            400,
            vec![],
            r#"{"message":"ValidationException"}"#,
            Failure::Fatal {
                status: StatusCode::BAD_REQUEST,
                body: Bytes::new(),
            },
        ),
    ];
    for (status, headers, body, expected) in cases {
        let (base, _) = upstream(status, headers, body.as_bytes().to_vec()).await;
        let result = provider(&base)
            .send(
                &request(Inbound::Chat, chat_body(false), false),
                &account(&[], "aoa-token"),
            )
            .await;
        match (&expected, result.err()) {
            (Failure::Fatal { status, .. }, Some(Failure::Fatal { status: got, body })) => {
                assert_eq!(status, &got, "{status}");
                assert!(!body.is_empty());
            }
            (expected, Some(got)) => assert_eq!(expected, &got),
            (expected, None) => panic!("expected {expected:?}, got a reply"),
        }
    }
}

#[tokio::test]
async fn rejects_non_automatic_tool_choice() {
    let (base, _) = upstream(200, vec![], Vec::new()).await;
    let mut body = chat_body(false);
    body["tool_choice"] = serde_json::json!("required");
    let failure = provider(&base)
        .send(
            &request(Inbound::Chat, body, false),
            &account(&[], "aoa-token"),
        )
        .await
        .err()
        .expect("fatal");
    assert!(matches!(failure, Failure::Fatal { status, .. } if status == StatusCode::BAD_REQUEST));
}

#[tokio::test]
async fn missing_credential_is_an_auth_failure() {
    let (base, _) = upstream(200, vec![], Vec::new()).await;
    let account = Account {
        id: "acct".into(),
        credential: Credential::None,
        extra: BTreeMap::new(),
    };
    let failure = provider(&base)
        .send(&request(Inbound::Chat, chat_body(false), false), &account)
        .await
        .err()
        .expect("auth");
    assert_eq!(failure, Failure::Auth);
}

#[test]
fn chat_requests_use_the_routed_model_not_the_pinned_one() {
    let mut req = request(
        Inbound::Chat,
        serde_json::json!({ "model": "kiro@acct/claude-haiku-4.5", "messages": [] }),
        false,
    );
    req.model = "claude-haiku-4.5".into();
    assert_eq!(chat_request(&req).unwrap()["model"], "claude-haiku-4.5");
}

#[test]
fn fallback_endpoint_only_replaces_the_canonical_runtime_host() {
    assert_eq!(
        wire::fallback_endpoint(None, "eu-west-1").as_deref(),
        Some("https://q.eu-west-1.amazonaws.com/")
    );
    assert_eq!(
        wire::fallback_endpoint(Some("https://runtime.us-east-1.kiro.dev/"), "us-east-1")
            .as_deref(),
        Some("https://q.us-east-1.amazonaws.com/")
    );
    // A configured non-canonical endpoint is authoritative; never swap it behind the operator.
    assert!(wire::fallback_endpoint(Some("http://127.0.0.1:8080/v1"), "us-east-1").is_none());
    assert!(
        wire::fallback_endpoint(Some("https://runtime.us-east-1.kiro.dev/api"), "us-east-1")
            .is_none()
    );
}

#[tokio::test]
async fn tool_choice_none_sends_no_tools() {
    let (base, upstream) = upstream(
        200,
        vec![("content-type", "application/vnd.amazon.eventstream")],
        text_frame("Hello"),
    )
    .await;
    let mut body = chat_body(true);
    body["tool_choice"] = serde_json::json!("none");
    let _ = provider(&base)
        .send(
            &request(Inbound::Chat, body, true),
            &account(&[], "aoa-token"),
        )
        .await
        .expect("reply");
    let captured = upstream
        .captured
        .lock()
        .expect("capture")
        .clone()
        .expect("request");
    let body: Value = serde_json::from_slice(&captured.body).expect("json");
    let user = &body["conversationState"]["currentMessage"]["userInputMessage"];
    assert!(user.get("userInputMessageContext").is_none(), "{body}");
}

#[tokio::test]
async fn empty_user_message_is_rejected_locally() {
    let (base, upstream) = upstream(200, vec![], Vec::new()).await;
    let body = serde_json::json!({
        "model": "kiro/claude-sonnet-4-5",
        "messages": [{ "role": "user", "content": "   " }],
    });
    let failure = provider(&base)
        .send(
            &request(Inbound::Chat, body, false),
            &account(&[], "aoa-token"),
        )
        .await
        .err()
        .expect("fatal");
    assert!(matches!(failure, Failure::Fatal { status, .. } if status == StatusCode::BAD_REQUEST));
    assert!(upstream.captured.lock().expect("capture").is_none());
}

#[tokio::test]
async fn required_completion_retries_once_then_takes_the_answer() {
    let first = {
        let mut body = encode_message(
            &[
                (":message-type", "event"),
                (":event-type", "assistantResponseEvent"),
            ],
            br#"{"content":"Let me look."}"#,
        );
        body.extend(encode_message(
            &[(":message-type", "event"), (":event-type", "metadataEvent")],
            br#"{"stopReason":"END_TURN"}"#,
        ));
        body
    };
    let answer = {
        let first_part = serde_json::json!({
            "name": "codex_kiro_final_answer",
            "toolUseId": "call-1",
            "input": "{\"answer\":\"",
        });
        let mut body = encode_message(
            &[(":message-type", "event"), (":event-type", "toolUseEvent")],
            first_part.to_string().as_bytes(),
        );
        let rest = serde_json::json!({ "input": "Done.\"}", "stop": true });
        body.extend(encode_message(
            &[(":message-type", "event"), (":event-type", "toolUseEvent")],
            rest.to_string().as_bytes(),
        ));
        body.extend(encode_message(
            &[(":message-type", "event"), (":event-type", "metadataEvent")],
            br#"{"stopReason":"END_TURN"}"#,
        ));
        body
    };
    let (base, upstream) = scripted(vec![
        (
            200,
            vec![("content-type", "application/vnd.amazon.eventstream")],
            first,
        ),
        (
            200,
            vec![("content-type", "application/vnd.amazon.eventstream")],
            answer,
        ),
    ])
    .await;
    let reply = provider(&base)
        .send(
            &request(Inbound::Chat, chat_body(false), false),
            &account(&[], "aoa-token"),
        )
        .await
        .expect("reply");
    let mut bytes = Vec::new();
    let mut stream = reply.body;
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    let text = body["choices"][0]["message"]["content"]
        .as_str()
        .expect("content");
    assert_eq!(text, "Done.");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");

    let captured = upstream.captured.lock().expect("capture");
    assert_eq!(captured.len(), 2, "exactly one bounded retry");
    let retry: Value = serde_json::from_slice(&captured[1].body).expect("json");
    let state = &retry["conversationState"];
    let history = state["history"].as_array().expect("history");
    let first = history[0]["userInputMessage"]["content"]
        .as_str()
        .expect("content");
    assert!(first.starts_with("Be terse."), "{first}");
    assert!(first.ends_with("Say hello."), "{first}");
    assert_eq!(
        history[1]["assistantResponseMessage"]["content"],
        "Let me look."
    );
    let content = state["currentMessage"]["userInputMessage"]["content"]
        .as_str()
        .expect("content");
    assert!(
        content.contains("call codex_kiro_final_answer now"),
        "{content}"
    );
    assert!(
        state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["toolSpecification"]["name"] == "codex_kiro_final_answer")
    );
}

#[test]
fn tool_alias_round_trips_to_the_client_name() {
    use super::wire::ToolNames;
    let mut names = ToolNames::new();
    let long = format!("mcp__srv__{}", "n".repeat(70));
    let alias = names.alias(&long);
    assert_ne!(alias, long);
    assert_eq!(names.restore(&alias), long);
    assert_eq!(names.restore("plain"), "plain");
}

#[test]
fn wire_client_constant_is_used() {
    assert_eq!(WireClient::Cli.origin(), "KIRO_CLI");
    assert_eq!(WireClient::Ide.origin(), "AI_EDITOR");
}

#[tokio::test]
async fn turns_without_token_usage_report_an_estimate() {
    let body = encode_message(
        &[
            (":message-type", "event"),
            (":event-type", "assistantResponseEvent"),
        ],
        br#"{"content":"Hello there, friend."}"#,
    );
    let (base, _) = upstream(
        200,
        vec![("content-type", "application/vnd.amazon.eventstream")],
        body,
    )
    .await;
    let reply = provider(&base)
        .send(
            &request(Inbound::Chat, chat_body(false), false),
            &account(&[], "aoa-token"),
        )
        .await
        .expect("reply");
    let mut bytes = Vec::new();
    let mut stream = reply.body;
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    let usage = &body["usage"];
    assert!(usage["prompt_tokens"].as_u64().unwrap() > 0, "{usage}");
    assert_eq!(usage["completion_tokens"], 6, "20 chars / 3.5");
}

fn event(kind: &str, payload: &Value) -> Vec<u8> {
    encode_message(
        &[(":message-type", "event"), (":event-type", kind)],
        payload.to_string().as_bytes(),
    )
}

fn talk(text: &str) -> Vec<u8> {
    let mut body = event(
        "assistantResponseEvent",
        &serde_json::json!({ "content": text }),
    );
    body.extend(event(
        "metadataEvent",
        &serde_json::json!({ "stopReason": "END_TURN" }),
    ));
    body
}

/// Chat reply text, streamed and collected, after a first attempt that only talked, then `retry`.
async fn content_after_retry(retry: Vec<u8>) -> [String; 2] {
    let mut texts = [String::new(), String::new()];
    for (text, stream) in texts.iter_mut().zip([true, false]) {
        let events = || vec![("content-type", "application/vnd.amazon.eventstream")];
        let (base, _) = scripted(vec![
            (200, events(), talk("gwtest")),
            (200, events(), retry.clone()),
        ])
        .await;
        let reply = provider(&base)
            .send(
                &request(Inbound::Chat, chat_body(stream), stream),
                &account(&[], "aoa-token"),
            )
            .await
            .expect("reply");
        let mut bytes = Vec::new();
        let mut body = reply.body;
        while let Some(chunk) = body.next().await {
            bytes.extend_from_slice(&chunk.expect("chunk"));
        }
        let bytes = String::from_utf8(bytes).expect("utf8");
        if !stream {
            let body: Value = serde_json::from_str(&bytes).expect("json");
            *text = body["choices"][0]["message"]["content"]
                .as_str()
                .expect("content")
                .to_owned();
            continue;
        }
        for data in bytes.lines().filter_map(|line| line.strip_prefix("data: ")) {
            if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                text.push_str(
                    chunk["choices"][0]["delta"]["content"]
                        .as_str()
                        .unwrap_or_default(),
                );
            }
        }
    }
    texts
}

#[tokio::test]
async fn a_retry_answer_supersedes_the_retry_preamble() {
    let mut retry = event(
        "assistantResponseEvent",
        &serde_json::json!({ "content": "The output is:\n\n" }),
    );
    retry.extend(event(
        "toolUseEvent",
        &serde_json::json!({ "name": "codex_kiro_final_answer", "toolUseId": "c1",
            "input": "{\"answer\":\"gwtest\"}", "stop": true }),
    ));
    assert_eq!(content_after_retry(retry).await, ["gwtest", "gwtest"]);
}

#[tokio::test]
async fn a_retry_that_only_talks_keeps_the_first_answer() {
    let retry = talk("I don't have an existing task to continue.");
    assert_eq!(content_after_retry(retry).await, ["gwtest", "gwtest"]);
}
