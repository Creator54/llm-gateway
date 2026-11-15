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

/// A cache hit teaches the gateway that this upstream takes a cache key, so the
/// OpenAI-style `prompt_cache_key` can be sent on later requests.
#[tokio::test]
async fn a_cache_hit_records_cache_key_support() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc as StdArc;

    #[derive(Clone)]
    struct Cached {
        hits: StdArc<AtomicUsize>,
    }

    async fn answer(axum::extract::State(state): axum::extract::State<Cached>) -> Response {
        let first = state.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let cached = if first { 0 } else { 4096 };
        let body = format!(
            r#"{{"id":"c","object":"chat.completion","model":"beam-free","choices":[{{"index":0,"message":{{"role":"assistant","content":"pong"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":4200,"completion_tokens":2,"total_tokens":4202,"prompt_tokens_details":{{"cached_tokens":{cached}}}}}}}"#
        );
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap()
    }

    let hits = StdArc::new(AtomicUsize::new(0));
    let app = Router::new()
        .fallback(any(answer))
        .with_state(Cached { hits: hits.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = std::env::temp_dir().join(format!(
        "gw-quirk-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    {
        // Probe before the gateway starts, then release the connection so the
        // gateway's own handle is the only writer.
        let store = gw::store::Store::open(&dir).unwrap();
        assert!(store
            .get_quirk("opencode", "beam-free", "prompt_cache_key")
            .unwrap()
            .is_none());
    }

    // Go through the gateway so `Track::finish` sees the usage.
    let cfg = gw::config::Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        state_dir: dir.clone(),
        profile: gw::config::Profile::Parity,
        endpoints: std::collections::BTreeMap::from([(
            "opencode".to_owned(),
            format!("http://{addr}"),
        )]),
        ..gw::config::Config::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = gw::server::serve(listener, cfg).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    for _ in 0..2 {
        let response = reqwest::Client::new()
            .post(format!("http://{gateway}/v1/chat/completions"))
            .header("content-type", "application/json")
            .body(r#"{"model":"opencode/beam-free","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .expect("request");
        let _ = response.text().await;
    }

    // The quirk is written while the reply streams out.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let reopened = gw::store::Store::open(&dir).unwrap();
    let learned = reopened
        .get_quirk("opencode", "beam-free", "prompt_cache_key")
        .unwrap();
    assert_eq!(
        learned.as_deref(),
        Some("true"),
        "cache hit must be learned"
    );
}
