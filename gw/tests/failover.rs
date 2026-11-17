//! Failover accounting: budget, undeclared accounts, and combo ordering.
//!
//! These exercise the loop in `Gateway::native` through the public server, with
//! a fake upstream and a temp credential store.

use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{body::Body, http::StatusCode, response::Response, routing::any, Router};
use gw::{
    combo::{Combo, Strategy, Target},
    config::{Config, Profile},
};

#[derive(Clone)]
struct Upstream {
    hits: Arc<AtomicUsize>,
    /// Answer 429 for requests bearing this key, 200 otherwise.
    refuse: &'static str,
}

async fn answer(
    axum::extract::State(state): axum::extract::State<Upstream>,
    request: axum::extract::Request,
) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    let auth = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if auth.ends_with(state.refuse) {
        return Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("content-type", "application/json")
            .header("retry-after", "7200")
            .body(Body::from(
                r#"{"type":"error","error":{"type":"FreeUsageLimitError","message":"x"}}"#,
            ))
            .unwrap();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"id":"c","object":"chat.completion","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
        ))
        .unwrap()
}

async fn upstream(refuse: &'static str) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let state = Upstream {
        hits: hits.clone(),
        refuse,
    };
    let app = Router::new().fallback(any(answer)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gw-dispatch-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut perms = std::fs::metadata(&dir).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o700);
    std::fs::set_permissions(&dir, perms).unwrap();
    dir
}

/// Writes a credential store with `accounts` as (provider, id, key).
fn creds(dir: &std::path::Path, accounts: &[(&str, &str, &str)]) {
    let mut items = Vec::new();
    for (provider, id, key) in accounts {
        items.push(format!(
            r#"{{"provider":"{provider}","id":"{id}","label":"{id}","paused":false,"needs_reauth":false,"credential":{{"type":"api_key","key":"{key}"}}}}"#
        ));
    }
    let body = format!(r#"{{"version":1,"accounts":[{}]}}"#, items.join(","));
    let path = dir.join("credentials.json");
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o600);
    std::fs::set_permissions(&path, perms).unwrap();
}

async fn serve(cfg: Config) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = gw::server::serve(listener, cfg).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    addr
}

fn combo(provider: &str, model: &str) -> Combo {
    Combo {
        name: String::new(),
        strategy: Strategy::Failover,
        targets: vec![Target {
            provider: provider.to_owned(),
            model: model.to_owned(),
            weight: 1,
        }],
        sticky_limit: 1,
    }
}

async fn chat(addr: std::net::SocketAddr, model: &str) -> (StatusCode, String) {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(format!(
            r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}]}}"#
        ))
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.text().await.unwrap())
}

/// A provider with no adapter must not consume the budget of a combo that has
/// a working target after it.
#[tokio::test]
async fn combo_skips_a_target_without_an_adapter() {
    let (base, hits) = upstream("never").await;
    let dir = temp_dir("skip");
    creds(&dir, &[("opencode", "acct1", "acct1-key")]);

    let mut combos = BTreeMap::new();
    let mut mixed = combo("unknown-provider", "whatever");
    mixed.targets.push(Target {
        provider: "opencode".into(),
        model: "beam-free".into(),
        weight: 1,
    });
    combos.insert("mixed".to_owned(), mixed);

    let addr = serve(Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        state_dir: dir,
        profile: Profile::Parity,
        combos,
        endpoints: BTreeMap::from([("opencode".to_owned(), base)]),
        ..Config::default()
    })
    .await;

    let (status, body) = chat(addr, "combo/mixed").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// A combo name the config does not define is an error, never a plain forward.
#[tokio::test]
async fn unknown_combo_is_not_found() {
    let (base, hits) = upstream("never").await;
    let dir = temp_dir("unknown-combo");
    let addr = serve(Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        state_dir: dir,
        profile: Profile::Parity,
        fallback: Some(base),
        ..Config::default()
    })
    .await;

    let (status, body) = chat(addr, "combo/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("unknown combo: nope"), "{body}");
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

/// An account in the store that the adapter cannot serve must not stop the loop
/// from reaching a later target either.
#[tokio::test]
async fn a_declared_account_still_serves_the_request() {
    let (base, hits) = upstream("never").await;
    let dir = temp_dir("declared");
    creds(&dir, &[("opencode", "acct1", "acct1-key")]);
    let addr = serve(Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        state_dir: dir,
        profile: Profile::Parity,
        endpoints: BTreeMap::from([("opencode".to_owned(), base)]),
        ..Config::default()
    })
    .await;

    let (status, body) = chat(addr, "opencode/beam-free").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// One cooling account must not make the whole request fail while another
/// account of the same provider is free.
#[tokio::test]
async fn a_cooling_account_fails_over_to_a_live_one() {
    let (base, hits) = upstream("acct1-key").await;
    let dir = temp_dir("failover");
    creds(
        &dir,
        &[
            ("opencode", "acct1", "acct1-key"),
            ("opencode", "acct2", "acct2-key"),
        ],
    );
    let addr = serve(Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        state_dir: dir,
        profile: Profile::Parity,
        endpoints: BTreeMap::from([("opencode".to_owned(), base)]),
        ..Config::default()
    })
    .await;

    let (status, body) = chat(addr, "opencode/beam-free").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("ok"));
    // One refusal, then the serving account.
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}
