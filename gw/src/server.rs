use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderName, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::config::Config;

/// Run the gateway on `cfg.listen` until SIGINT or SIGTERM.
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    if !cfg.listen.ip().is_loopback() && cfg.api_key_file.is_none() {
        anyhow::bail!(
            "refusing to listen on {} without an api key file",
            cfg.listen
        );
    }
    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    serve(listener, cfg).await
}

/// Serve on an already bound listener until SIGINT or SIGTERM.
pub async fn serve(listener: tokio::net::TcpListener, cfg: Config) -> anyhow::Result<()> {
    let state = Arc::new(Gateway::new(&cfg)?);
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/responses", any(forward))
        .route("/v1/chat/completions", any(forward))
        .route("/v1/messages", any(forward))
        .route("/v1/messages/count_tokens", any(forward))
        .route("/v1/models", get(forward))
        .fallback(any(forward))
        .with_state(state);
    tracing::info!(addr = %listener.local_addr()?, "gw listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

struct Gateway {
    client: reqwest::Client,
    /// Base URL everything is forwarded to; `None` means 502.
    fallback: Option<String>,
    /// Key clients must send; `None` disables auth.
    key: Option<Arc<str>>,
    max_body: usize,
}

impl Gateway {
    fn new(cfg: &Config) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(60))
            // A redirect would break the 1:1 pass-through, so hand it to the client as a status.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let key = match &cfg.api_key_file {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
                let text = text.trim();
                if text.is_empty() {
                    anyhow::bail!("{}: api key is empty", path.display());
                }
                Some(Arc::from(text))
            }
            None => None,
        };
        Ok(Self {
            client,
            fallback: cfg.fallback.clone(),
            key,
            max_body: cfg.max_body_bytes,
        })
    }
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"ok": true}))
}

/// Forward one request to the fallback upstream, byte for byte.
// TODO: upgrade /v1/responses to a WebSocket tunnel when Responses-over-WS is needed.
async fn forward(State(st): State<Arc<Gateway>>, req: Request) -> Response {
    let Some(base) = st.fallback.as_deref() else {
        return json_error(StatusCode::BAD_GATEWAY, "no upstream configured");
    };
    let strip_authorization = match authenticate(&st, req.headers()) {
        Some(strip) => strip,
        None => return unauthorized(),
    };
    let (parts, body) = req.into_parts();
    if let Some(len) = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > st.max_body {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
        }
    }

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!("{}{path_and_query}", base.trim_end_matches('/'));

    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        if hop_by_hop(name) || name == "x-gw-key" {
            continue;
        }
        if strip_authorization && name == header::AUTHORIZATION {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }

    let over = Arc::new(AtomicBool::new(false));
    let stream = limited(body, st.max_body, over.clone());
    let mut outbound = st
        .client
        .request(parts.method.clone(), &url)
        .headers(headers);
    // GET and HEAD carry no body; sending an empty chunked body upsets some upstreams.
    if parts.method != axum::http::Method::GET && parts.method != axum::http::Method::HEAD {
        outbound = outbound.body(reqwest::Body::wrap_stream(stream));
    }

    let upstream = match outbound.send().await {
        Ok(r) => r,
        Err(e) => {
            if over.load(Ordering::Relaxed) {
                return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
            }
            tracing::warn!(url = %url, error = %e, "upstream request failed");
            return json_error(StatusCode::BAD_GATEWAY, "upstream request failed");
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out_headers = HeaderMap::new();
    for (name, value) in upstream.headers() {
        if hop_by_hop(name) {
            continue;
        }
        out_headers.append(name.clone(), value.clone());
    }
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = out_headers;
    response
}

/// Returns `Some(strip_authorization)` when the request may pass, `None` when it may not.
fn authenticate(st: &Gateway, headers: &HeaderMap) -> Option<bool> {
    let Some(key) = st.key.as_deref() else {
        return Some(false);
    };
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some((scheme, credential)) = value.split_once(' ') {
            if scheme.eq_ignore_ascii_case("bearer")
                && ct_eq(credential.trim().as_bytes(), key.as_bytes())
            {
                return Some(true);
            }
        }
    }
    let direct = headers.get("x-gw-key").and_then(|v| v.to_str().ok())?;
    ct_eq(direct.trim().as_bytes(), key.as_bytes()).then_some(false)
}

/// Compare in constant time so a wrong key leaks no prefix length.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hop_by_hop(name: &HeaderName) -> bool {
    let name = name.as_str();
    matches!(
        name,
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "upgrade"
            | "te"
            | "trailer"
    ) || name.starts_with("proxy-")
}

/// Cut the body off past `max` bytes and raise `over` so the caller can answer 413.
fn limited(
    body: Body,
    max: usize,
    over: Arc<AtomicBool>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    let mut used = 0usize;
    body.into_data_stream().map(move |frame| {
        let frame = frame.map_err(std::io::Error::other)?;
        used += frame.len();
        if used > max {
            over.store(true, Ordering::Relaxed);
            return Err(std::io::Error::other("request body too large"));
        }
        Ok(frame)
    })
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"error": {"message": message}})),
    )
        .into_response()
}

fn unauthorized() -> Response {
    let mut response = json_error(StatusCode::UNAUTHORIZED, "missing or invalid api key");
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_static("Bearer"),
    );
    response
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::SocketAddr, path::PathBuf, sync::atomic::AtomicUsize};

    use axum::extract::Path;
    use serde_json::{json, Value};

    static KEY_N: AtomicUsize = AtomicUsize::new(0);

    struct TempKey(PathBuf);

    impl TempKey {
        fn new(key: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "gw-key-{}-{}",
                std::process::id(),
                KEY_N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, format!("{key}\n")).unwrap();
            Self(path)
        }
    }

    impl Drop for TempKey {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    async fn bind(app: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    fn upstream() -> Router {
        Router::new()
            .route("/status/{code}", any(status))
            .route("/stream", any(streaming))
            .fallback(any(echo))
    }

    async fn echo(req: Request) -> Response {
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 20)
            .await
            .unwrap_or_default();
        let headers: Vec<(String, String)> = parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        Json(json!({
            "method": parts.method.as_str(),
            "path": parts.uri.path(),
            "query": parts.uri.query().unwrap_or(""),
            "headers": headers,
            "body": String::from_utf8_lossy(&bytes),
        }))
        .into_response()
    }

    async fn status(Path(code): Path<u16>) -> Response {
        let mut response = Response::new(Body::from("teapot"));
        *response.status_mut() = StatusCode::from_u16(code).unwrap();
        response
            .headers_mut()
            .insert("x-up", header::HeaderValue::from_static("1"));
        response
    }

    async fn streaming() -> Response {
        let chunks = futures_util::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(b"one")),
            Ok(Bytes::from_static(b"two")),
        ]);
        let mut response = Response::new(Body::from_stream(chunks));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain"),
        );
        response
    }

    async fn gateway(
        fallback: Option<SocketAddr>,
        key: Option<&str>,
        max_body: usize,
    ) -> (SocketAddr, Option<TempKey>) {
        let key_file = key.map(TempKey::new);
        let cfg = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            api_key_file: key_file.as_ref().map(|k| k.0.clone()),
            fallback: fallback.map(|a| format!("http://{a}")),
            max_body_bytes: max_body,
            ..Config::default()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            serve(listener, cfg).await.unwrap();
        });
        (addr, key_file)
    }

    async fn json_body(resp: reqwest::Response) -> Value {
        serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap()
    }

    fn names(v: &Value) -> Vec<String> {
        v["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h[0].as_str().unwrap().to_string())
            .collect()
    }

    fn header(v: &Value, name: &str) -> Option<String> {
        v["headers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h[0] == name)
            .map(|h| h[1].as_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn health_needs_no_key() {
        let (addr, _key) = gateway(None, Some("secret"), 1024).await;
        let resp = reqwest::get(format!("http://{addr}/health")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(json_body(resp).await, json!({"ok": true}));
    }

    #[tokio::test]
    async fn missing_fallback_is_502() {
        let (addr, _key) = gateway(None, None, 1024).await;
        let resp = reqwest::get(format!("http://{addr}/v1/models"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
        assert!(json_body(resp).await["error"].is_object());
    }

    #[tokio::test]
    async fn auth_is_required_and_stripped() {
        let up = bind(upstream()).await;
        let (addr, _key) = gateway(Some(up), Some("secret"), 1 << 20).await;
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/v1/models");

        assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
        assert_eq!(
            client
                .get(&url)
                .header("x-gw-key", "wrong")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let unauthorized = client.get(&url).bearer_auth("wrong").send().await.unwrap();
        assert_eq!(unauthorized.status(), 401);
        assert!(unauthorized.headers().contains_key("www-authenticate"));

        let resp = client.get(&url).bearer_auth("secret").send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = json_body(resp).await;
        assert!(header(&body, "authorization").is_none());
        assert!(header(&body, "x-gw-key").is_none());

        // Authenticating with x-gw-key leaves the upstream credential alone.
        let body = json_body(
            client
                .get(&url)
                .header("x-gw-key", "secret")
                .header("authorization", "Bearer upstream")
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            header(&body, "authorization").as_deref(),
            Some("Bearer upstream")
        );
        assert!(header(&body, "x-gw-key").is_none());
    }

    #[tokio::test]
    async fn request_is_forwarded_1_to_1() {
        let up = bind(upstream()).await;
        let (addr, _key) = gateway(Some(up), Some("secret"), 1 << 20).await;
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/v1/chat/completions?x=1&y=2"))
            .header("x-gw-key", "secret")
            .header("connection", "keep-alive")
            .header("keep-alive", "timeout=5")
            .header("te", "trailers")
            .header("trailer", "x-trailer")
            .header("proxy-authorization", "Basic zzz")
            .header("x-custom", "yes")
            .body("hello")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = json_body(resp).await;
        assert_eq!(body["method"], "POST");
        assert_eq!(body["path"], "/v1/chat/completions");
        assert_eq!(body["query"], "x=1&y=2");
        assert_eq!(body["body"], "hello");

        let seen = names(&body);
        // transfer-encoding is listed here as dropped but reappears because reqwest re-chunks the
        // streamed body; content-length is genuinely gone.
        for dropped in [
            "content-length",
            "connection",
            "keep-alive",
            "te",
            "trailer",
            "proxy-authorization",
            "x-gw-key",
        ] {
            assert!(!seen.contains(&dropped.to_string()), "{dropped} forwarded");
        }
        assert!(seen.contains(&"x-custom".to_string()));
        // reqwest sets host for the upstream; the client's value is not passed through.
        assert_eq!(
            header(&body, "host").as_deref(),
            Some(up.to_string().as_str())
        );
        assert!(!seen.iter().any(|n| n.starts_with("x-forwarded-")));
    }

    #[tokio::test]
    async fn status_and_headers_pass_through() {
        let up = bind(upstream()).await;
        let (addr, _key) = gateway(Some(up), None, 1 << 20).await;
        let resp = reqwest::get(format!("http://{addr}/status/418"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 418);
        assert_eq!(resp.headers().get("x-up").unwrap(), "1");
        assert_eq!(resp.text().await.unwrap(), "teapot");
    }

    #[tokio::test]
    async fn response_streams_through() {
        let up = bind(upstream()).await;
        let (addr, _key) = gateway(Some(up), None, 1 << 20).await;
        let resp = reqwest::get(format!("http://{addr}/stream")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("content-length").is_none());
        assert_eq!(resp.text().await.unwrap(), "onetwo");
    }

    #[tokio::test]
    async fn large_body_is_413() {
        let up = bind(upstream()).await;
        let (addr, _key) = gateway(Some(up), None, 16).await;
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/v1/messages"))
            .body("x".repeat(64))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 413);
    }

    #[tokio::test]
    async fn non_loopback_without_key_is_refused() {
        let cfg = Config {
            listen: "0.0.0.0:0".parse().unwrap(),
            ..Config::default()
        };
        assert!(run(cfg).await.is_err());
    }
}
