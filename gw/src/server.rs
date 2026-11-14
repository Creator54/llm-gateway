use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use futures_util::{Future, Stream, StreamExt};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use bytes::Bytes;
use serde_json::Value;

use crate::{
    cache, config::Config, creds, journal::Journal, metrics, models, provider, providers, route,
    store::Store, tidy, tokens, translate::SseParser, usage,
};

/// How long in-flight requests may keep the process alive after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

/// Largest JSON reply kept for usage parsing; bigger replies stream unread.
const MAX_JSON_REPLY: u64 = 8 << 20;

const TRANSFORMS: &str = "x-gw-transforms";

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
        .route("/metrics", get(metrics_endpoint))
        .route("/v1/responses", any(transformed))
        .route("/v1/chat/completions", any(transformed))
        .route("/v1/messages", any(transformed))
        .route("/v1/messages/count_tokens", any(count_tokens))
        .route("/v1/models", any(list_models))
        .fallback(any(forward))
        .with_state(Arc::clone(&state));
    tracing::info!(addr = %listener.local_addr()?, "gw listening");
    serve_until(listener, app, shutdown(), SHUTDOWN_GRACE).await?;
    // Flush buffered usage rows; a stream cut off by the grace window keeps a handle.
    match Arc::try_unwrap(state) {
        Ok(gateway) => {
            if let Some(journal) = gateway.journal {
                match Arc::try_unwrap(journal) {
                    Ok(journal) => journal.shutdown().await?,
                    Err(_) => tracing::warn!(
                        "usage journal still in use at shutdown; buffered rows may be lost"
                    ),
                }
            }
        }
        Err(_) => tracing::warn!("usage journal not flushed; a request task outlived shutdown"),
    }
    Ok(())
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
    /// Where accounts and the usage journal live; adapters read credentials from here.
    state_dir: std::path::PathBuf,
    /// Per-provider endpoint overrides.
    endpoints: std::collections::BTreeMap<String, String>,
    cache: cache::Options,
    tidy: tidy::Options,
    /// Learned provider quirks; `None` when the store could not be opened.
    store: Option<Arc<Store>>,
    /// `None` when the state dir was unusable; requests still serve, rows are skipped.
    journal: Option<Arc<Journal>>,
    /// Combos from the config, so `/v1/models` can name what this gateway routes.
    combos: Vec<crate::combo::Combo>,
    /// Prefix hash and cache read of the last reply, for the cache-break detector.
    prefix: Arc<Mutex<Option<(u64, u64)>>>,
    /// Request counters rendered at `/metrics`.
    metrics: Arc<metrics::Metrics>,
    /// Serve `/metrics` at all; off keeps the port for proxying only.
    metrics_on: bool,
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
        let store = match Store::open(&cfg.state_dir) {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                tracing::warn!(error = %e, "usage journal disabled");
                None
            }
        };
        let journal = store
            .as_ref()
            .map(|store| Arc::new(Journal::start(Arc::clone(store))));
        Ok(Self {
            client,
            fallback: cfg.fallback.clone(),
            key,
            max_body: cfg.max_body_bytes,
            state_dir: cfg.state_dir.clone(),
            cache: cfg.cache_options(),
            tidy: cfg.tidy_options(),
            store,
            journal,
            combos: cfg.combos.values().cloned().collect(),
            endpoints: cfg.endpoints.clone(),
            prefix: Arc::new(Mutex::new(None)),
            metrics: Arc::new(metrics::Metrics::new()),
            metrics_on: cfg.metrics,
        })
    }
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"ok": true}))
}

/// Prometheus scrape endpoint; authenticated exactly like the proxy routes.
async fn metrics_endpoint(State(st): State<Arc<Gateway>>, headers: HeaderMap) -> Response {
    if !st.metrics_on {
        return json_error(StatusCode::NOT_FOUND, "metrics are disabled");
    }
    // A scrape carries no model, so only the key is checked here.
    if st.key.is_some() && authenticate(&st, &headers).is_none() {
        return unauthorized();
    }
    let (dropped, lost) = st
        .journal
        .as_deref()
        .map(|journal| (journal.dropped(), journal.lost()))
        .unwrap_or_default();
    let body = st.metrics.render(&metrics::Extra::new(dropped, lost));
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    response
}

/// One request as it will go upstream; also the original to fall back to.
#[derive(Clone)]
struct Plan {
    method: axum::http::Method,
    path_and_query: String,
    headers: HeaderMap,
    body: Bytes,
    /// Length the client declared, so an unedited body keeps its framing.
    declared_len: Option<u64>,
    /// Edits that changed the body.
    applied: Vec<&'static str>,
}

impl Plan {
    fn new(parts: axum::http::request::Parts, body: Bytes, strip_authorization: bool) -> Self {
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_else(|| "/".to_owned());
        let declared_len = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
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
        Self {
            method: parts.method,
            path_and_query,
            headers,
            body,
            declared_len,
            applied: Vec::new(),
        }
    }

    fn edited(&self) -> bool {
        !self.applied.is_empty()
    }

    /// The `x-gw-transforms` value; absent when nothing changed.
    fn header(&self) -> Option<HeaderValue> {
        if self.applied.is_empty() {
            return None;
        }
        HeaderValue::from_str(&self.applied.join(",")).ok()
    }
}

/// Read a request body of at most `max` bytes; `Err` is the reply to send instead.
async fn read_body(body: Body, max: usize) -> Result<Bytes, Response> {
    let over = Arc::new(AtomicBool::new(false));
    let mut stream = limited(body, max, over);
    let mut buf = Vec::new();
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(frame) => buf.extend_from_slice(&frame),
            Err(_) => {
                return Err(json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request body too large",
                ))
            }
        }
    }
    Ok(Bytes::from(buf))
}

/// Auth, upgrade and size checks, then the body. `Err` is the reply to send instead.
async fn prepare(st: &Gateway, req: Request) -> Result<Plan, Response> {
    let strip_authorization = match authenticate(st, req.headers()) {
        Some(strip) => strip,
        None => return Err(unauthorized()),
    };
    if is_websocket_upgrade(req.headers()) {
        return Err(json_error(
            StatusCode::NOT_IMPLEMENTED,
            "websocket not supported yet",
        ));
    }
    let declared_len = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared_len.is_some_and(|len| len > st.max_body as u64) {
        return Err(json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body too large",
        ));
    }
    let (parts, body) = req.into_parts();
    let body = read_body(body, st.max_body).await?;
    Ok(Plan::new(parts, body, strip_authorization))
}

/// Forward one request to the fallback upstream, byte for byte.
async fn forward(State(st): State<Arc<Gateway>>, req: Request) -> Response {
    match prepare(&st, req).await {
        // A provider with a native adapter is served here; everything else is forwarded.
        Ok(plan) => dispatch(&st, plan).await,
        Err(response) => response,
    }
}

/// Forward one request after the cache and tidy edits; any failure keeps the original bytes.
async fn transformed(State(st): State<Arc<Gateway>>, req: Request) -> Response {
    let mut plan = match prepare(&st, req).await {
        Ok(plan) => plan,
        Err(response) => return response,
    };
    if !plan.body.is_empty() && plan.method != axum::http::Method::HEAD {
        if let Some((body, applied)) = st.edit(&plan.body, family(&plan.path_and_query)) {
            plan.body = body;
            plan.applied = applied;
        }
    }
    dispatch(&st, plan).await
}

/// Serve a prepared request through a native adapter, else forward it untouched.
async fn dispatch(st: &Gateway, plan: Plan) -> Response {
    match st.native(&plan).await {
        Some(response) => response,
        None => st.send(plan).await,
    }
}

/// Local `count_tokens`: an estimate, so the request never reaches an upstream.
async fn count_tokens(State(st): State<Arc<Gateway>>, req: Request) -> Response {
    let started = Instant::now();
    let plan = match prepare(&st, req).await {
        Ok(plan) => plan,
        Err(response) => return response,
    };
    let count = tokens::estimate(&plan.body);
    let info = RequestInfo::of(&plan.body);
    let mut response = Response::new(Body::from(tokens::response(count)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert(TRANSFORMS, HeaderValue::from_static("count_tokens:local"));
    let status = response.status().as_u16();
    st.finish(
        &plan.path_and_query,
        &info,
        started,
        status,
        usage::Tokens {
            input: count,
            ..usage::Tokens::default()
        },
    );
    response
}

/// Local model list when the config can name any model, else the upstream's.
async fn list_models(State(st): State<Arc<Gateway>>, req: Request) -> Response {
    let shape = models::shape(req.headers());
    let declared_len = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let local = st.model_list();
    if local.is_empty() || declared_len.is_some_and(|len| len > 0) {
        return forward(State(st), req).await;
    }
    let mut response = Response::new(Body::from(models::render(&local, shape)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

impl Gateway {
    /// Cache and tidy edits; `None` when nothing changed and the bytes stand.
    fn edit(&self, body: &Bytes, family: cache::Family) -> Option<(Bytes, Vec<&'static str>)> {
        let session = session_key(body);
        let known = |model: &str| self.cache_key_support(model);
        let marked = cache::rewrite(body, family, &self.cache, &session, known);
        let mut applied = Vec::new();
        let mut out = body.clone();
        if marked.as_slice() != body.as_ref() {
            out = Bytes::from(marked);
            applied.push("cache");
        }
        let tidied = tidy::tidy(out.clone(), &self.tidy);
        if tidied != out {
            out = tidied;
            applied.push("tidy");
        }
        if applied.is_empty() {
            return None;
        }
        Some((out, applied))
    }

    /// Whether an upstream is known to accept `prompt_cache_key` for `model`.
    fn cache_key_support(&self, model: &str) -> Option<bool> {
        let store = self.store.as_deref()?;
        let (provider, _) = route_of(model);
        let value = store
            .get_quirk(&provider, model, "prompt_cache_key")
            .ok()
            .flatten()?;
        match value.as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        }
    }

    /// Models this gateway can route, or empty when the config names none.
    fn model_list(&self) -> Vec<models::Entry> {
        let mut entries = Vec::new();
        for target in self.combos.iter().flat_map(|combo| &combo.targets) {
            let id = format!("{}/{}", target.provider, target.model);
            if !entries.iter().any(|entry: &models::Entry| entry.id == id) {
                entries.push(models::Entry {
                    id,
                    provider: target.provider.clone(),
                    context_window: 0,
                    max_output: 0,
                });
            }
        }
        if entries.is_empty() {
            return entries;
        }
        let names: Vec<String> = self.combos.iter().map(|c| c.name.clone()).collect();
        models::with_builtins(entries, &names)
    }

    /// Send `plan` upstream and stream the reply back, journaling it as it passes.
    async fn send(&self, plan: Plan) -> Response {
        let started = Instant::now();
        let Some(base) = self.fallback.as_deref() else {
            return json_error(StatusCode::BAD_GATEWAY, "no upstream configured");
        };
        let url = format!("{}{}", base.trim_end_matches('/'), plan.path_and_query);
        let mut outbound = self
            .client
            .request(plan.method.clone(), &url)
            .headers(plan.headers.clone());
        // HEAD has no body; other methods keep one when the client sent one. An empty body on GET
        // would become a pointless chunked write, which some upstreams reject.
        if plan.method != axum::http::Method::HEAD && !plan.body.is_empty() {
            // A fixed length keeps the client's framing; an edited body needs its own.
            let len = match plan.edited() {
                true => Some(plan.body.len() as u64),
                false => plan.declared_len,
            };
            if let Some(len) = len {
                outbound = outbound.header(header::CONTENT_LENGTH, len);
            }
            // A client that streamed its body without a length keeps that framing.
            outbound = match len {
                Some(_) => outbound.body(plan.body.clone()),
                None => {
                    let body = plan.body.clone();
                    outbound.body(reqwest::Body::wrap_stream(futures_util::stream::once(
                        async move { Ok::<_, std::io::Error>(body) },
                    )))
                }
            };
        }

        let upstream = match outbound.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "upstream request failed");
                let status = StatusCode::BAD_GATEWAY;
                let info = RequestInfo::of(&plan.body);
                self.finish(
                    &plan.path_and_query,
                    &info,
                    started,
                    status.as_u16(),
                    usage::Tokens::default(),
                );
                return json_error(status, "upstream request failed");
            }
        };
        self.reply(upstream, plan, started)
    }

    /// Pass the upstream reply through untouched except for the transforms header.
    /// Serve a request through a native adapter when the route names one.
    ///
    /// Returns `None` when no adapter owns the provider, so the caller falls back
    /// to plain forwarding.
    async fn native(&self, plan: &Plan) -> Option<Response> {
        let started = Instant::now();
        let inbound = match plan.path_and_query.split('?').next().unwrap_or_default() {
            "/v1/messages" => provider::Inbound::Messages,
            "/v1/responses" => provider::Inbound::Responses,
            "/v1/chat/completions" => provider::Inbound::Chat,
            _ => return None,
        };
        let info = RequestInfo::of(&plan.body);
        // Routing already resolved `provider/model`; an unqualified model has no adapter.
        if info.provider.is_empty() || info.provider == "combo" || info.provider == "auto" {
            return None;
        }
        let adapter: Box<dyn provider::Provider> = match info.provider.as_str() {
            "opencode" => Box::new(providers::opencode::Opencode::new(
                self.client.clone(),
                self.endpoints.get(&info.provider).cloned(),
            )),
            _ => return None,
        };
        let account = match self.account(&info.provider) {
            Ok(account) => account,
            Err(response) => return Some(*response),
        };
        let request = provider::Request {
            inbound,
            model: info.model.clone(),
            headers: plan.headers.clone(),
            body: plan.body.clone(),
            stream: stream_requested(&plan.body),
            session: session_key(&plan.body),
        };
        let reply = match adapter.send(&request, &account).await {
            Ok(reply) => reply,
            Err(failure) => return Some(failure_response(failure)),
        };
        Some(self.adapter_reply(reply, plan.clone(), started))
    }

    /// The account to bill for `provider`; the free tier needs none.
    fn account(&self, provider: &str) -> Result<provider::Account, Box<Response>> {
        let store = creds::Store::load(&self.state_dir).map_err(|_| {
            Box::new(json_error(
                StatusCode::BAD_GATEWAY,
                "credential store is unreadable",
            ))
        })?;
        match store.accounts(provider).into_iter().next() {
            Some(entry) => Ok(provider::Account::from(&entry)),
            // The adapter sends the anonymous marker when there is no key.
            None if provider == "opencode" => Ok(provider::Account {
                id: "anonymous".to_owned(),
                credential: provider::Credential::None,
                extra: Default::default(),
            }),
            None => Err(Box::new(json_error(
                StatusCode::BAD_GATEWAY,
                "no account for this provider",
            ))),
        }
    }

    /// Relay an adapter reply with the same usage accounting as a forwarded one.
    fn adapter_reply(&self, reply: provider::Reply, plan: Plan, started: Instant) -> Response {
        let status = StatusCode::from_u16(reply.status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let info = RequestInfo::of(&plan.body);
        let sse = is_event_stream(&reply.headers);
        let mut headers = HeaderMap::new();
        for (name, value) in &reply.headers {
            if hop_by_hop(name) {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        if let Some(value) = plan.header() {
            headers.insert(TRANSFORMS, value);
        }
        let tracked = Tracked {
            inner: reply.body,
            sse: sse.then(SseParser::new),
            full: None,
            tokens: usage::Tokens::default(),
            track: Some(Track {
                path: plan.path_and_query.clone(),
                info,
                status: status.as_u16(),
                started,
                hash: prefix(&plan.body),
                journal: self.journal.clone(),
                prefix: Arc::clone(&self.prefix),
                metrics: Arc::clone(&self.metrics),
                ttfb: None,
            }),
        };
        let mut response = Response::new(Body::from_stream(tracked));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        response
    }

    fn reply(&self, upstream: reqwest::Response, plan: Plan, started: Instant) -> Response {
        let status =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let info = RequestInfo::of(&plan.body);
        let sse = is_event_stream(upstream.headers());
        let buffered = upstream
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|len| len <= MAX_JSON_REPLY);
        let mut headers = HeaderMap::new();
        for (name, value) in upstream.headers() {
            if hop_by_hop(name) {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        if let Some(value) = plan.header() {
            headers.insert(TRANSFORMS, value);
        }
        let tracked = Tracked {
            inner: upstream
                .bytes_stream()
                .map(|c| c.map_err(std::io::Error::other))
                .boxed(),
            sse: sse.then(SseParser::new),
            // An SSE reply is read as events, never as one JSON body.
            full: (buffered && !sse).then(Vec::new),
            tokens: usage::Tokens::default(),
            track: Some(Track {
                path: plan.path_and_query.clone(),
                info,
                status: status.as_u16(),
                started,
                hash: prefix(&plan.body),
                journal: self.journal.clone(),
                prefix: Arc::clone(&self.prefix),
                metrics: Arc::clone(&self.metrics),
                ttfb: None,
            }),
        };
        let mut response = Response::new(Body::from_stream(tracked));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        response
    }

    /// Journal one finished request that never reached an upstream reply.
    fn finish(
        &self,
        path: &str,
        info: &RequestInfo,
        started: Instant,
        status: u16,
        tokens: usage::Tokens,
    ) {
        Track {
            path: path.to_owned(),
            info: info.clone(),
            status,
            started,
            hash: None,
            journal: self.journal.clone(),
            prefix: Arc::clone(&self.prefix),
            metrics: Arc::clone(&self.metrics),
            ttfb: None,
        }
        .finish(tokens);
    }
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

/// Which protocol a request path names.
fn family(path_and_query: &str) -> cache::Family {
    let path = path_and_query.split('?').next().unwrap_or(path_and_query);
    if path == "/v1/messages" {
        cache::Family::Anthropic
    } else {
        cache::Family::OpenAi
    }
}

fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next().unwrap_or(v).trim() == "text/event-stream")
}

/// `provider/model` split as the router reads it; an empty namespace when it is not a model id.
fn route_of(model: &str) -> (String, String) {
    match route::parse(model) {
        Some(route::Target::Model {
            provider, model, ..
        }) => (provider, model),
        _ => (String::new(), model.to_owned()),
    }
}

/// What the client asked for, read without an adapter.
#[derive(Clone)]
struct RequestInfo {
    session: String,
    provider: String,
    account: String,
    model: String,
}

impl RequestInfo {
    fn of(body: &[u8]) -> Self {
        let value = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
        let asked = value
            .pointer("/model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let (provider, model, account) = match route::parse(&asked) {
            Some(route::Target::Model {
                provider,
                account,
                model,
            }) => (
                provider,
                model,
                account.unwrap_or_else(|| "auto".to_owned()),
            ),
            Some(route::Target::Combo(name)) => ("combo".to_owned(), name, String::new()),
            Some(route::Target::Auto(profile)) => {
                ("auto".to_owned(), asked, profile.unwrap_or_default())
            }
            None => (String::new(), asked, String::new()),
        };
        Self {
            session: session_of(&value),
            provider,
            account,
            model,
        }
    }
}

/// The session a request belongs to, when the client names one.
fn session_key(body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return String::new();
    };
    session_of(&value)
}

fn session_of(value: &Value) -> String {
    for pointer in ["/session_id", "/prompt_cache_key", "/metadata/session_id"] {
        let key = value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|key| !key.is_empty());
        if let Some(key) = key {
            return key.to_owned();
        }
    }
    String::new()
}

/// Hash of what an upstream can serve from cache. Unparsable bodies have none.
fn prefix(body: &[u8]) -> Option<u64> {
    let value = serde_json::from_slice::<Value>(body).ok()?;
    Some(cache::prefix_hash(&value))
}

/// Request fields the journal row needs, carried into the reply stream.
struct Track {
    path: String,
    info: RequestInfo,
    status: u16,
    started: Instant,
    hash: Option<u64>,
    journal: Option<Arc<Journal>>,
    prefix: Arc<Mutex<Option<(u64, u64)>>>,
    metrics: Arc<metrics::Metrics>,
    /// Time to the first body chunk; `None` until it arrives.
    ttfb: Option<f64>,
}

impl Track {
    /// Record when the first chunk reached the client, once.
    fn first_chunk(&mut self) {
        if self.ttfb.is_none() {
            self.ttfb = Some(self.started.elapsed().as_secs_f64());
        }
    }
}

impl Track {
    fn finish(self, tokens: usage::Tokens) {
        self.check_cache_break(tokens.cached_read);
        self.count(&tokens);
        let Some(journal) = self.journal.as_deref() else {
            return;
        };
        let latency_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        journal.record(crate::store::UsageRow::new(
            &self.info.session,
            &self.path,
            &self.info.provider,
            &self.info.account,
            &self.info.model,
            self.status,
            tokens.input,
            tokens.cached_read,
            tokens.cache_write,
            tokens.output,
            latency_ms,
        ));
    }

    /// Remember this prefix's cache read, and log a break when the share drops.
    /// A request as Prometheus sees it: status class, latency, tokens.
    fn count(&self, tokens: &usage::Tokens) {
        let provider = &self.info.provider;
        let model = &self.info.model;
        let account = &self.info.account;
        self.metrics.request(
            provider,
            model,
            account,
            &format!("{}xx", self.status / 100),
        );
        self.metrics
            .observe_duration(provider, self.started.elapsed().as_secs_f64());
        if let Some(ttfb) = self.ttfb {
            self.metrics.observe_ttfb(provider, ttfb);
        }
        for (kind, n) in [
            (metrics::TokenKind::Input, tokens.input),
            (metrics::TokenKind::Output, tokens.output),
            (metrics::TokenKind::CacheRead, tokens.cached_read),
            (metrics::TokenKind::CacheWrite, tokens.cache_write),
        ] {
            if n > 0 {
                self.metrics.tokens(provider, model, kind, n);
            }
        }
    }

    fn check_cache_break(&self, cached_read: u64) {
        let Some(hash) = self.hash else {
            return;
        };
        let previous = self
            .prefix
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace((hash, cached_read));
        if let Some((previous, before)) = previous {
            if previous != hash && cached_read < before {
                tracing::info!(
                    path = %self.path,
                    before,
                    after = cached_read,
                    "cache break: prefix changed and cache read dropped"
                );
            }
        }
    }
}

/// Passes upstream bytes through unchanged while reading token usage from them.
struct Tracked {
    inner: std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
    /// An SSE reply, parsed event by event.
    sse: Option<SseParser>,
    /// A small JSON reply, kept whole to parse at the end.
    full: Option<Vec<u8>>,
    tokens: usage::Tokens,
    track: Option<Track>,
}

impl Tracked {
    fn observe(&mut self, chunk: &[u8]) {
        if let Some(parser) = &mut self.sse {
            match parser.push(chunk) {
                Ok(events) => {
                    for payload in events {
                        absorb(&mut self.tokens, &payload);
                    }
                }
                Err(_) => self.sse = None,
            }
        } else if let Some(buf) = &mut self.full {
            if buf.len() + chunk.len() <= MAX_JSON_REPLY as usize {
                buf.extend_from_slice(chunk);
            } else {
                self.full = None;
            }
        }
    }

    /// Stop inspecting and journal the request; runs once, at the end or on a drop.
    fn end(&mut self) {
        if let Some(mut parser) = self.sse.take() {
            if let Ok(Some(payload)) = parser.finish() {
                absorb(&mut self.tokens, &payload);
            }
        }
        if let Some(buf) = self.full.take().filter(|buf| !buf.is_empty()) {
            self.tokens = usage::body(&buf);
        }
        if let Some(track) = self.track.take() {
            track.finish(self.tokens);
        }
    }
}

impl Stream for Tracked {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                if let Some(track) = this.track.as_mut() {
                    track.first_chunk();
                }
                this.observe(&chunk);
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
            std::task::Poll::Ready(Some(Err(e))) => {
                std::task::Poll::Ready(Some(Err(std::io::Error::other(e))))
            }
            std::task::Poll::Ready(None) => {
                this.end();
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.end();
    }
}

fn absorb(tokens: &mut usage::Tokens, payload: &str) {
    if let Some(seen) = usage::event(payload) {
        tokens.merge(seen);
    }
}
fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"error": {"message": message}})),
    )
        .into_response()
}

/// Whether the client asked for a streamed reply.
fn stream_requested(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("stream").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// An adapter failure as the client's status, before any byte was streamed.
fn failure_response(failure: provider::Failure) -> Response {
    match failure {
        provider::Failure::Fatal { status, body } => {
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = status;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            response
        }
        provider::Failure::Auth => {
            json_error(StatusCode::UNAUTHORIZED, "upstream rejected the account")
        }
        provider::Failure::Quota { .. } => {
            json_error(StatusCode::TOO_MANY_REQUESTS, "provider quota exhausted")
        }
        provider::Failure::RateLimited { .. } => {
            json_error(StatusCode::TOO_MANY_REQUESTS, "provider rate limited")
        }
        provider::Failure::Retryable => {
            json_error(StatusCode::BAD_GATEWAY, "upstream request failed")
        }
    }
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
            state_dir: temp_state(),
            ..Config::default()
        };
        (gateway_with(cfg).await, key_file)
    }

    async fn gateway_with(cfg: Config) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            serve(listener, cfg).await.unwrap();
        });
        addr
    }

    /// A state dir of this test's own. Left behind: the journal task outlives the test.
    fn temp_state() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "gw-state-{}-{}",
            std::process::id(),
            KEY_N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
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

    /// One Messages body the default edits change: no breakpoint, no cache key.
    const MESSAGES: &str = concat!(
        r#"{"model":"claude-sonnet-4","max_tokens":16,"#,
        r#""system":[{"type":"text","text":"be brief"}],"#,
        r#""messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#,
    );

    fn anthropic_post(addr: SocketAddr, path: &str, body: &str) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .post(format!("http://{addr}{path}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_owned())
    }

    fn transforms(resp: &reqwest::Response) -> Option<String> {
        resp.headers()
            .get("x-gw-transforms")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    #[tokio::test]
    async fn parity_forwards_request_bytes_unchanged() {
        let up = bind(upstream()).await;
        let cfg = Config {
            fallback: Some(format!("http://{up}")),
            state_dir: temp_state(),
            profile: crate::config::Profile::Parity,
            ..Config::default()
        };
        let addr = gateway_with(cfg).await;
        let resp = anthropic_post(addr, "/v1/messages", MESSAGES)
            .send()
            .await
            .unwrap();
        assert_eq!(transforms(&resp), None);
        let body = json_body(resp).await;
        assert_eq!(body["body"], MESSAGES);
    }

    #[tokio::test]
    async fn default_profile_adds_anthropic_cache_marker() {
        let up = bind(upstream()).await;
        let (addr, _key) = gateway(Some(up), None, 1 << 20).await;
        let resp = anthropic_post(addr, "/v1/messages", MESSAGES)
            .send()
            .await
            .unwrap();
        let applied = transforms(&resp).unwrap_or_default();
        assert!(applied.split(',').any(|name| name == "cache"), "{applied}");
        let body = json_body(resp).await;
        let sent = body["body"].as_str().unwrap();
        assert!(sent.contains("cache_control"), "{sent}");
        assert_ne!(sent, MESSAGES);
    }

    #[tokio::test]
    async fn transform_failure_forwards_original_bytes() {
        let up = bind(upstream()).await;
        let (addr, _key) = gateway(Some(up), None, 1 << 20).await;
        let broken = r#"{"model":"claude-sonnet-4","messages":[ }"#;
        let resp = anthropic_post(addr, "/v1/messages", broken)
            .send()
            .await
            .unwrap();
        assert_eq!(transforms(&resp), None);
        let body = json_body(resp).await;
        assert_eq!(body["body"], broken);
    }

    #[tokio::test]
    async fn count_tokens_is_local() {
        let cfg = Config {
            // Nothing listens here: a forwarded request would answer 502.
            fallback: Some("http://127.0.0.1:1".into()),
            state_dir: temp_state(),
            ..Config::default()
        };
        let addr = gateway_with(cfg).await;
        let resp = anthropic_post(addr, "/v1/messages/count_tokens", MESSAGES)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = json_body(resp).await;
        assert!(body["input_tokens"].as_u64().unwrap() > 0, "{body}");
    }

    #[tokio::test]
    async fn request_writes_a_usage_row() {
        let up = bind(Router::new().fallback(any(sse_usage))).await;
        let state = temp_state();
        let cfg = Config {
            fallback: Some(format!("http://{up}")),
            state_dir: state.clone(),
            ..Config::default()
        };
        let addr = gateway_with(cfg).await;
        let resp = anthropic_post(addr, "/v1/messages", MESSAGES)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = resp.text().await;

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let store = Store::open(&state).unwrap();
            let rows = store.usage_summary(0).unwrap();
            if let Some(row) = rows.first() {
                assert_eq!(row.requests, 1);
                assert_eq!(row.input, 12);
                assert_eq!(row.cached_read, 4);
                assert_eq!(row.output, 7);
                break;
            }
            assert!(Instant::now() < deadline, "no usage row was written");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn sse_usage() -> Response {
        let events = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":",
            "{\"input_tokens\":12,\"cache_read_input_tokens\":4}}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
        );
        let mut response = Response::new(Body::from(events));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/event-stream"),
        );
        response
    }
}
