//! `/metrics` behind the gateway key, and the counters it renders.

use axum::{body::Body, http::StatusCode, response::Response, routing::any, Router};
use gw::config::{Config, Profile};
use std::collections::BTreeMap;

async fn upstream() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"ok":true}"#))
        .unwrap()
}

async fn bind() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(any(upstream)))
            .await
            .unwrap();
    });
    format!("{addr}")
}

fn state() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gw-metrics-{}",
        std::process::id() as u64
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn serve(cfg: Config) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = gw::server::serve(listener, cfg).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    addr
}

fn config(fallback: String, key: Option<&str>) -> Config {
    let dir = state();
    let key_file = key.map(|value| {
        let path = dir.join("key");
        std::fs::write(&path, value).unwrap();
        path
    });
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        fallback: Some(fallback),
        api_key_file: key_file,
        state_dir: dir,
        profile: Profile::Parity,
        endpoints: BTreeMap::new(),
        ..Config::default()
    }
}

#[tokio::test]
async fn metrics_requires_the_key() {
    let addr = serve(config(format!("http://{}/", bind().await), Some("secret"))).await;
    let resp = reqwest::get(format!("http://{addr}/metrics"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().contains_key("www-authenticate"));

    let ok = reqwest::Client::new()
        .get(format!("http://{addr}/metrics"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
}

#[tokio::test]
async fn metrics_uses_the_prometheus_content_type() {
    let addr = serve(config(format!("http://{}/", bind().await), None)).await;
    let resp = reqwest::get(format!("http://{addr}/metrics"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/plain; version=0.0.4; charset=utf-8"
    );
}

#[tokio::test]
async fn a_proxied_request_shows_up_in_metrics() {
    let addr = serve(config(format!("http://{}/", bind().await), None)).await;
    let client = reqwest::Client::new();
    let proxied = client
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-sonnet-4","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(proxied.status(), StatusCode::OK);
    let _ = proxied.text().await;

    let body = client
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("gw_requests_total"), "{body}");
    assert!(body.contains("gw_inflight"), "{body}");
}

#[tokio::test]
async fn metrics_can_be_switched_off() {
    let mut cfg = config(format!("http://{}/", bind().await), None);
    cfg.metrics = false;
    let addr = serve(cfg).await;
    let resp = reqwest::get(format!("http://{addr}/metrics"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
