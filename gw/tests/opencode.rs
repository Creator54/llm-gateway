//! opencode adapter through a fake upstream: egress, the anonymous key, and retry.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{
    body::Body, extract::State, http::StatusCode, response::Response, routing::any, Router,
};
use futures_util::StreamExt;
use gw::{
    provider::{Account, Credential, Failure, Inbound, Provider, Request},
    providers::opencode::Opencode,
};
use http::HeaderMap;
use serde_json::{json, Value};

#[derive(Clone)]
struct Upstream {
    hits: Arc<AtomicUsize>,
    refuse: usize,
    seen: Arc<Mutex<Vec<Seen>>>,
}

#[derive(Clone, Default)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn answer(State(state): State<Upstream>, request: axum::extract::Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 24)
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
    state.seen.lock().unwrap().push(Seen {
        path: parts.uri.path().to_owned(),
        headers,
        body: bytes.to_vec(),
    });

    let count = state.hits.fetch_add(1, Ordering::SeqCst) + 1;
    if count <= state.refuse {
        return Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"type":"error","error":{"type":"FreeUsageLimitError","message":"x"}}"#,
            ))
            .unwrap();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"id":"c1","object":"chat.completion","model":"beam-free","choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}}"#,
        ))
        .unwrap()
}

async fn spawn(refuse: usize) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<Seen>>>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let state = Upstream {
        hits: hits.clone(),
        refuse,
        seen: seen.clone(),
    };
    let app = Router::new().fallback(any(answer)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits, seen)
}

use std::sync::Mutex;

fn request(inbound: Inbound, body: Value) -> Request {
    Request {
        inbound,
        model: "beam-free".into(),
        headers: HeaderMap::new(),
        body: bytes::Bytes::from(serde_json::to_vec(&body).unwrap()),
        stream: false,
        session: "conversation-1".into(),
    }
}

fn anonymous() -> Account {
    Account {
        id: "anonymous".into(),
        credential: Credential::None,
        extra: Default::default(),
    }
}

#[tokio::test]
async fn chat_body_is_forwarded_unchanged_with_the_anonymous_key() {
    let (base, _, seen) = spawn(0).await;
    let adapter = Opencode::new(reqwest::Client::new(), Some(base));
    let body =
        json!({"model":"beam-free","messages":[{"role":"user","content":"hi"}],"stream":false});
    let reply = adapter
        .send(&request(Inbound::Chat, body.clone()), &anonymous())
        .await
        .expect("reply");
    assert_eq!(reply.status, StatusCode::OK);

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].path, "/zen/v1/chat/completions");
    assert_eq!(
        serde_json::from_slice::<Value>(&seen[0].body).unwrap(),
        body,
        "client bytes must reach upstream unchanged"
    );
    let auth = seen[0]
        .headers
        .iter()
        .find(|(name, _)| name == "authorization")
        .map(|(_, value)| value.as_str());
    assert_eq!(auth, Some("Bearer public"));
}

#[tokio::test]
async fn transient_refusals_are_retried_until_served() {
    let (base, hits, _) = spawn(2).await;
    let adapter = Opencode::new(reqwest::Client::new(), Some(base));
    let body = json!({"model":"beam-free","messages":[{"role":"user","content":"hi"}]});
    let reply = adapter
        .send(&request(Inbound::Chat, body), &anonymous())
        .await
        .expect("third attempt succeeds");
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(hits.load(Ordering::SeqCst), 3);

    let body = reply.body.collect::<Vec<_>>().await;
    let text = String::from_utf8(body.into_iter().flatten().flatten().collect()).unwrap();
    assert!(text.contains("pong"), "{text}");
}

#[tokio::test]
async fn a_spent_budget_surfaces_as_a_quota_failure() {
    let (base, hits, _) = spawn(usize::MAX).await;
    let adapter = Opencode::new(reqwest::Client::new(), Some(base));
    let body = json!({"model":"beam-free","messages":[{"role":"user","content":"hi"}]});
    let failure = match adapter
        .send(&request(Inbound::Chat, body), &anonymous())
        .await
    {
        Ok(_) => panic!("expected a quota failure"),
        Err(failure) => failure,
    };
    assert!(matches!(failure, Failure::Quota { .. }));
    // Three attempts, then give up rather than looping.
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}
