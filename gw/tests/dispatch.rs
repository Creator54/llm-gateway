//! Failover through the account pool: a fake upstream, two stored accounts.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use gw::{
    config::{Config, Profile},
    creds,
};
use serde_json::{json, Value};

static DIR_N: AtomicUsize = AtomicUsize::new(0);

/// A state dir of its own, so accounts and the usage store never collide.
struct StateDir(PathBuf);

impl StateDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gw-dispatch-{}-{}",
            std::process::id(),
            DIR_N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Two accounts for `anthropic`, written with the credential store's own API.
fn accounts(dir: &Path, keys: &[(&str, &str)]) {
    let store = creds::Store::load(dir).unwrap();
    for (id, key) in keys {
        store.upsert(creds::Entry {
            provider: "anthropic".to_owned(),
            id: (*id).to_owned(),
            label: None,
            paused: false,
            needs_reauth: false,
            last_error: None,
            credential: creds::Secret::ApiKey {
                key: (*key).to_owned(),
            },
        });
    }
    store.save().unwrap();
}

/// Fake upstream: counts hits, refuses the first `refuse` of them with `Retry-After`.
#[derive(Clone, Default)]
struct Fake {
    hits: Arc<Mutex<Vec<String>>>,
    refuse: Arc<AtomicUsize>,
}

async fn messages(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    let key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let seen = {
        let mut hits = fake.hits.lock().unwrap();
        hits.push(key);
        hits.len()
    };
    if seen <= fake.refuse.load(Ordering::SeqCst) {
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}})),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, header::HeaderValue::from_static("20"));
        return response;
    }
    Json(json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
    }))
    .into_response()
}

/// What the forwarding fallback received, so the test can see it was left alone.
async fn echo(request: axum::extract::Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20)
        .await
        .unwrap_or_default();
    Json(json!({
        "method": parts.method.as_str(),
        "path": parts.uri.path(),
        "body": String::from_utf8_lossy(&bytes),
    }))
    .into_response()
}

fn upstream(fake: Fake) -> Router {
    Router::new()
        .route("/v1/messages", post(messages))
        .with_state(fake)
}

fn echo_upstream() -> Router {
    Router::new().fallback(axum::routing::any(echo))
}

async fn bind(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Start a gateway against `dir`, with `endpoints` naming the fake upstream.
async fn gateway(
    dir: &Path,
    endpoints: BTreeMap<String, String>,
    fallback: Option<String>,
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = Config {
        listen: addr,
        state_dir: dir.to_path_buf(),
        fallback,
        endpoints,
        // Byte-for-byte forwarding, so a test compares what it sent.
        profile: Profile::Parity,
        ..Config::default()
    };
    tokio::spawn(async move { gw::server::serve(listener, cfg).await });
    addr
}

async fn ask(addr: SocketAddr, model: &str) -> reqwest::Response {
    let body = json!({
        "model": model,
        "max_tokens": 16,
        "session_id": "s1",
        "messages": [{"role": "user", "content": "hi"}],
    })
    .to_string();
    reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .unwrap()
}

fn endpoint(addr: SocketAddr) -> BTreeMap<String, String> {
    BTreeMap::from([("anthropic".to_owned(), format!("http://{addr}"))])
}

#[tokio::test]
async fn rate_limited_account_fails_over_to_the_next() {
    let dir = StateDir::new();
    accounts(dir.path(), &[("one", "key-one"), ("two", "key-two")]);
    let fake = Fake::default();
    fake.refuse.store(1, Ordering::SeqCst);
    let up = bind(upstream(fake.clone())).await;
    let addr = gateway(dir.path(), endpoint(up), None).await;

    let response = ask(addr, "anthropic/claude-test").await;
    assert_eq!(response.status(), StatusCode::OK);
    let hits = fake.hits.lock().unwrap().clone();
    assert_eq!(
        hits.len(),
        2,
        "the refused account then the next one: {hits:?}"
    );
    assert_ne!(hits[0], hits[1], "the second attempt used another account");
}

#[tokio::test]
async fn every_account_rate_limited_answers_429_and_cools_down() {
    let dir = StateDir::new();
    accounts(dir.path(), &[("one", "key-one"), ("two", "key-two")]);
    let fake = Fake::default();
    fake.refuse.store(usize::MAX, Ordering::SeqCst);
    let up = bind(upstream(fake.clone())).await;
    let addr = gateway(dir.path(), endpoint(up), None).await;

    let response = ask(addr, "anthropic/claude-test").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .expect("429 carries a Retry-After");
    assert!(retry > 0 && retry <= 20, "retry-after {retry}");
    let hits = fake.hits.lock().unwrap().len();
    assert_eq!(hits, 2, "each account was tried once");

    // Both accounts are cooling, so the next request never reaches the upstream.
    let again = ask(addr, "anthropic/claude-test").await;
    assert_eq!(again.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(again.headers().contains_key(header::RETRY_AFTER));
    assert_eq!(
        fake.hits.lock().unwrap().len(),
        hits,
        "a cooling account is not retried"
    );
}

#[tokio::test]
async fn unknown_provider_falls_back_unchanged() {
    let dir = StateDir::new();
    let up = bind(echo_upstream()).await;
    let addr = gateway(dir.path(), BTreeMap::new(), Some(format!("http://{up}"))).await;

    let response = ask(addr, "acme/mystery-model").await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    let seen: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(seen["method"], "POST");
    assert_eq!(seen["path"], "/v1/messages");
    let body = seen["body"].as_str().unwrap_or_default();
    assert!(body.contains("acme/mystery-model"), "{body}");
}
