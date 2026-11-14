use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use futures_util::{Future, Stream, StreamExt};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderName, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use bytes::Bytes;

use crate::config::Config;

/// How long in-flight requests may keep the process alive after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

/// Run the gateway on `cfg.listen` until SIGINT or SIGTERM.
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    guard(&cfg)?;
    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    serve(listener, cfg).await
}

/// Serve on an already bound listener until SIGINT or SIGTERM.
pub async fn serve(listener: tokio::net::TcpListener, cfg: Config) -> anyhow::Result<()> {
    guard(&cfg)?;
    let state = Arc::new(Gateway::new(&cfg)?);
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/responses", any(forward))
        .route("/v1/chat/completions", any(forward))
        .route("/v1/messages", any(forward))
        .route("/v1/messages/count_tokens", any(forward))
        .route("/v1/models", any(forward))
        .fallback(any(forward))
        .with_state(state);
    tracing::info!(addr = %listener.local_addr()?, "gw listening");
    serve_until(listener, app, shutdown(), SHUTDOWN_GRACE).await
}

fn guard(cfg: &Config) -> anyhow::Result<()> {
    if !cfg.listen.ip().is_loopback() && cfg.api_key_file.is_none() {
        anyhow::bail!(
            "refusing to listen on {} without an api key file",
            cfg.listen
        );
    }
    Ok(())
}

/// Serve `app` until `stop` fires, then give in-flight requests at most `grace`.
async fn serve_until(
    listener: tokio::net::TcpListener,
    app: Router,
    stop: impl Future<Output = ()>,
    grace: Duration,
) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = rx.await;
    });
    let mut handle = tokio::spawn(async move { server.await });
    stop.await;
    let _ = tx.send(());
    match tokio::time::timeout(grace, &mut handle).await {
        Ok(joined) => joined??,
        Err(_) => {
            tracing::warn!(?grace, "in-flight requests still open; stopping anyway");
            handle.abort();
        }
    }
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
async fn forward(State(st): State<Arc<Gateway>>, req: Request) -> Response {
    let strip_authorization = match authenticate(&st, req.headers()) {
        Some(strip) => strip,
        None => return unauthorized(),
    };
    let Some(base) = st.fallback.as_deref() else {
        return json_error(StatusCode::BAD_GATEWAY, "no upstream configured");
    };
    if is_websocket_upgrade(req.headers()) {
        return json_error(StatusCode::NOT_IMPLEMENTED, "websocket not supported yet");
    }
    let (parts, body) = req.into_parts();
    let declared_len = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared_len.is_some_and(|len| len > st.max_body as u64) {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    }
    let chunked = has_chunked(&parts.headers);

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!("{}{path_and_query}", base.trim_end_matches('/'));

    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        if hop_by_hop(name) || name == "x-gw-key" || name == header::CONTENT_LENGTH {
            continue;
        }
        if strip_authorization && name == header::AUTHORIZATION {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }

    let over = Arc::new(AtomicBool::new(false));
    let mut outbound = st
        .client
        .request(parts.method.clone(), &url)
        .headers(headers);
    // HEAD has no body; other methods keep one when the client sent one. An empty body on GET
    // would become a pointless chunked write, which some upstreams reject.
    if parts.method != axum::http::Method::HEAD
        && (chunked || declared_len.is_some_and(|len| len > 0))
    {
        // A fixed length keeps the client's framing; without one hyper re-chunks the stream.
        if let Some(len) = declared_len {
            outbound = outbound.header(header::CONTENT_LENGTH, len);
        }
        outbound = outbound.body(reqwest::Body::wrap_stream(limited(
            body,
            st.max_body,
            over.clone(),
        )));
    }

    let upstream = outbound.send().await;
    // The stream can overflow before or after upstream answers, so check both paths.
    if over.load(Ordering::Relaxed) {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    }
    let upstream = match upstream {
        Ok(r) => r,
        Err(e) => {
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

/// Compare in constant time so a wrong key leaks neither its prefix nor its length.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u64;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= u64::from(x ^ y);
    }
    diff == 0
}

fn has_chunked(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::TRANSFER_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let has = |name: &str, want: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case(want))
    };
    has("connection", "upgrade") && has("upgrade", "websocket")
}

fn hop_by_hop(name: &HeaderName) -> bool {
    let name = name.as_str();
    matches!(
        name,
        "host" | "transfer-encoding" | "connection" | "keep-alive" | "upgrade" | "te" | "trailer"
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
            .route("/head", any(head_body))
            .fallback(any(echo))
    }

    async fn head_body() -> Response {
        let mut response = Response::new(Body::from("teapot"));
        response.headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_static("6"),
        );
        response
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

    /// Send bytes straight at the gateway so upgrade and chunked framing are not rewritten.
    async fn raw(addr: SocketAddr, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        sock.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut buf))
            .await
            .ok();
        String::from_utf8_lossy(&buf).into_owned()
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
        for dropped in [
            "connection",
            "keep-alive",
            "te",
            "trailer",
            "proxy-authorization",
            "x-gw-key",
        ] {
            assert!(!seen.contains(&dropped.to_string()), "{dropped} forwarded");
        }
        assert!(!seen.contains(&"transfer-encoding".to_string()));
        // The client's declared length survives, so upstream sees the same framing.
        assert_eq!(header(&body, "content-length").as_deref(), Some("5"));
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

    #[tokio::test]
    async fn serve_refuses_non_loopback_without_key() {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let cfg = Config {
            listen: "0.0.0.0:0".parse().unwrap(),
            ..Config::default()
        };
        assert!(serve(listener, cfg).await.is_err());
    }

    #[tokio::test]
    async fn auth_is_checked_before_missing_upstream() {
        let (addr, _key) = gateway(None, Some("secret"), 1024).await;
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/v1/models");
        assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
        let resp = client.get(&url).bearer_auth("secret").send().await.unwrap();
        assert_eq!(resp.status(), 502);
    }

    #[tokio::test]
    async fn chunked_oversize_body_is_413() {
        let up = bind(upstream()).await;
        let (addr, _key) = gateway(Some(up), None, 16).await;
        let chunks = futures_util::stream::iter(
            (0..8).map(|_| Ok::<_, std::io::Error>(Bytes::from_static(b"0123456789"))),
        );
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/v1/messages"))
            .body(reqwest::Body::wrap_stream(chunks))
            .send()
            .await;
        assert_eq!(
            resp.map(|r| r.status()).ok(),
            Some(StatusCode::PAYLOAD_TOO_LARGE)
        );
    }

    #[tokio::test]
    async fn chunked_request_stays_chunked() {
        let up = bind(upstream()).await;
        let (addr, _key) = gateway(Some(up), None, 1 << 20).await;
        let chunks = futures_util::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(b"hel")),
            Ok(Bytes::from_static(b"lo")),
        ]);
        let body = json_body(
            reqwest::Client::new()
                .post(format!("http://{addr}/v1/messages"))
                .body(reqwest::Body::wrap_stream(chunks))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["body"], "hello");
        assert!(header(&body, "content-length").is_none());
        assert_eq!(
            header(&body, "transfer-encoding").as_deref(),
            Some("chunked")
        );
    }

    #[tokio::test]
    async fn get_body_is_forwarded_and_head_keeps_content_length() {
        let up = bind(upstream()).await;
        let (addr, _key) = gateway(Some(up), None, 1 << 20).await;
        let client = reqwest::Client::new();

        let body = json_body(
            client
                .get(format!("http://{addr}/v1/models"))
                .body("query")
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["method"], "GET");
        assert_eq!(body["body"], "query");
        assert_eq!(header(&body, "content-length").as_deref(), Some("5"));

        // A body-less GET must not gain an empty chunked body.
        let body = json_body(
            client
                .get(format!("http://{addr}/v1/models"))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["body"], "");
        assert!(header(&body, "content-length").is_none());
        assert!(header(&body, "transfer-encoding").is_none());

        let resp = client
            .head(format!("http://{addr}/head"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("content-length").unwrap(), "6");
        assert_eq!(resp.bytes().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn websocket_upgrade_is_501() {
        let up = bind(upstream()).await;
        let (addr, _key) = gateway(Some(up), None, 1 << 20).await;
        let request = format!(
            "GET /v1/responses HTTP/1.1\r\nhost: {addr}\r\nconnection: Upgrade\r\n\
             upgrade: websocket\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             sec-websocket-version: 13\r\n\r\n"
        );
        let raw = raw(addr, &request).await;
        assert!(raw.starts_with("HTTP/1.1 501"), "{raw}");
        assert!(raw.contains("websocket not supported yet"), "{raw}");
    }

    #[test]
    fn ct_eq_ignores_length_and_content() {
        assert!(ct_eq(b"secret", b"secret"));
        assert!(!ct_eq(b"secret", b"secre"));
        assert!(!ct_eq(b"secre", b"secret"));
        assert!(!ct_eq(b"secret", b"secreT"));
        assert!(!ct_eq(b"", b"s"));
        assert!(ct_eq(b"", b""));
    }

    static SLOW_DONE: AtomicBool = AtomicBool::new(false);

    async fn slow() -> &'static str {
        tokio::time::sleep(Duration::from_secs(2)).await;
        SLOW_DONE.store(true, Ordering::Relaxed);
        "late"
    }

    async fn quick() -> &'static str {
        tokio::time::sleep(Duration::from_millis(300)).await;
        "done"
    }

    fn stop_channel() -> (tokio::sync::oneshot::Sender<()>, impl Future<Output = ()>) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        (tx, async move {
            let _ = rx.await;
        })
    }

    #[tokio::test]
    async fn shutdown_returns_after_grace_with_requests_still_in_flight() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/slow", get(slow));
        let grace = Duration::from_millis(100);
        let (stop, stopped) = stop_channel();
        let serving = tokio::spawn(serve_until(listener, app, stopped, grace));
        let inflight = tokio::spawn(async move {
            let client = reqwest::Client::new();
            let _ = client.get(format!("http://{addr}/slow")).send().await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let started = std::time::Instant::now();
        stop.send(()).unwrap();
        serving.await.unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_millis(1500));
        assert!(!SLOW_DONE.load(Ordering::Relaxed), "request should be cut");

        inflight.abort();
    }

    #[tokio::test]
    async fn shutdown_lets_in_flight_requests_finish_inside_grace() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/quick", get(quick));
        let (stop, stopped) = stop_channel();
        let serving = tokio::spawn(serve_until(listener, app, stopped, Duration::from_secs(5)));
        let inflight = tokio::spawn(async move {
            let resp = reqwest::get(format!("http://{addr}/quick")).await.unwrap();
            resp.text().await.unwrap()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.send(()).unwrap();
        serving.await.unwrap().unwrap();
        assert_eq!(inflight.await.unwrap(), "done");
    }
}
