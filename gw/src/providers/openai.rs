//! openai adapter: ChatGPT Codex Responses passthrough.
//!
//! Wire contract: `docs/provider-specs/openai.md`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::{
    future::BoxFuture,
    stream::{self, BoxStream},
    FutureExt, StreamExt,
};
use http::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use http::StatusCode;
use serde_json::Value;

use crate::provider::{Account, Credential, Failure, Inbound, Provider, Reply, Request};
use crate::translate::{Error, SseParser};

/// Canonical endpoint; the pool's ChatGPT token is only valid here.
const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Caller headers relayed in this exact order, emitted lowercased.
const FORWARD_HEADERS: [&str; 18] = [
    "authorization",
    "chatgpt-account-id",
    "openai-beta",
    "originator",
    "session_id",
    "session-id",
    "thread-id",
    "x-client-request-id",
    "x-codex-beta-features",
    "x-codex-installation-id",
    "x-codex-parent-thread-id",
    "x-codex-turn-metadata",
    "x-codex-turn-state",
    "x-codex-window-id",
    "x-oai-attestation",
    "x-openai-subagent",
    "x-responsesapi-include-timing-metrics",
    "x-openai-internal-codex-responses-lite",
];

const ROUTING_HINT: &str = "x-codex-routing-hint";

/// Exhaustion codes the backend reports in the body of a 429/402.
const QUOTA_CODES: [&str; 6] = [
    "usage_limit_exceeded",
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
];

pub struct OpenAi {
    client: reqwest::Client,
    base: String,
}

impl OpenAi {
    /// `base_url` overrides the canonical endpoint for tests.
    pub fn new(client: reqwest::Client, base_url: Option<String>) -> Self {
        let base = base_url.unwrap_or_else(|| BASE_URL.to_owned());
        Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    async fn turn(&self, req: &Request, account: &Account) -> Result<Reply, Failure> {
        if req.inbound == Inbound::Messages {
            return Err(unsupported(req.inbound));
        }
        // The pool identity is mandatory; without it the backend answers with a generic 401.
        let Some(access) = access_token(account) else {
            return Err(Failure::Auth);
        };
        let Some(account_id) = account.extra.get("account_id") else {
            return Err(Failure::Auth);
        };

        let chat = req.inbound == Inbound::Chat;
        let chat_body: Option<Value> = if chat {
            serde_json::from_slice(&req.body).map_err(|_| bad_body())?
        } else {
            None
        };
        let raw = match &chat_body {
            Some(value) => {
                let translated = crate::translate::chat_request_to_responses(value, &req.model)
                    .map_err(|error| bad_request(&error))?;
                Bytes::from(serde_json::to_vec(&translated).map_err(|_| bad_body())?)
            }
            None => req.body.clone(),
        };
        let body = outbound_body(&raw, &req.model, req.fast.unwrap_or(false))?;
        let headers = outbound_headers(&req.headers, access, account_id, &body);
        let url = format!("{}/responses", self.base);
        let bytes = serde_json::to_vec(&body).map_err(|_| Failure::Retryable)?;

        let response = match self
            .client
            .post(&url)
            .headers(headers)
            .body(bytes)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(url = %url, %error, "openai upstream request failed");
                return Err(Failure::Retryable);
            }
        };
        let status = response.status();
        if !status.is_success() {
            return Err(reject(status, response).await);
        }

        let mut headers = reply_headers(response.headers());
        let body = if chat && req.stream {
            let include_usage = chat_body
                .as_ref()
                .and_then(|value| value.pointer("/stream_options/include_usage"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            chat_stream(response, &req.model, include_usage)
        } else if req.stream {
            relay(response)
        } else {
            let mut folded = fold(response).await?;
            if chat {
                folded = chat_fold(folded, &req.model).await?;
            }
            headers.remove(http::header::CONTENT_LENGTH);
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            folded
        };
        Ok(Reply {
            status,
            headers,
            body,
        })
    }
}

impl Provider for OpenAi {
    fn id(&self) -> &str {
        "openai"
    }

    fn send<'a>(
        &'a self,
        req: &'a Request,
        account: &'a Account,
    ) -> BoxFuture<'a, Result<Reply, Failure>> {
        self.turn(req, account).boxed()
    }
}

fn access_token(account: &Account) -> Option<&str> {
    match &account.credential {
        Credential::OAuth { access, .. } if !access.is_empty() => Some(access),
        _ => None,
    }
}

fn bad_request(error: &Error) -> Failure {
    let body =
        serde_json::to_vec(&error.to_body(crate::translate::Inbound::Chat)).unwrap_or_default();
    Failure::Fatal {
        status: StatusCode::BAD_REQUEST,
        body: Bytes::from(body),
    }
}

fn unsupported(inbound: Inbound) -> Failure {
    let inbound = match inbound {
        Inbound::Chat => crate::translate::Inbound::Chat,
        Inbound::Messages => crate::translate::Inbound::Messages,
        Inbound::Responses => crate::translate::Inbound::Responses,
    };
    let error = Error::bad_request("unsupported protocol for openai yet");
    let body = serde_json::to_vec(&error.to_body(inbound)).unwrap_or_default();
    Failure::Fatal {
        status: StatusCode::BAD_REQUEST,
        body: Bytes::from(body),
    }
}

/// Content-Type first, then the caller allowlist, then the pool identity overriding the forwarded
/// credential fields in place, then the user agent fallback and routing hint.
fn outbound_headers(
    incoming: &HeaderMap,
    access: &str,
    account_id: &str,
    body: &Value,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    for name in FORWARD_HEADERS {
        if let Some(value) = incoming.get(name) {
            headers.insert(HeaderName::from_static(name), value.clone());
        }
    }
    if let Ok(value) = HeaderValue::from_bytes(format!("Bearer {access}").as_bytes()) {
        headers.insert(http::header::AUTHORIZATION, value);
    }
    if let Ok(value) = HeaderValue::from_bytes(account_id.as_bytes()) {
        headers.insert(HeaderName::from_static("chatgpt-account-id"), value);
    }
    // Nothing provider-owned matches `user-agent`, so the caller's fingerprint is relayed.
    if let Some(value) = incoming.get(http::header::USER_AGENT) {
        headers.insert(http::header::USER_AGENT, value.clone());
    }
    if let Some(hint) = routing_hint(body) {
        headers.insert(HeaderName::from_static(ROUTING_HINT), hint);
    }
    headers
}

/// `model=<model>[;tier=<tier>]`, derived from the final body; an invalid component drops it.
fn routing_hint(body: &Value) -> Option<HeaderValue> {
    let model = hint_component(body.get("model")?, 256)?;
    let tier = match body.get("service_tier") {
        None => String::new(),
        Some(tier) => format!(";tier={}", hint_component(tier, 64)?),
    };
    HeaderValue::from_str(&format!("model={model}{tier}")).ok()
}

fn hint_component(value: &Value, max: usize) -> Option<&str> {
    let text = value.as_str()?;
    let printable = text
        .bytes()
        .all(|byte| matches!(byte, 0x21..=0x7e) && byte != b';' && byte != b'=');
    (printable && !text.is_empty() && text.len() <= max).then_some(text)
}

/// Caller body rewritten for the canonical backend, keeping the caller's key order.
///
/// `--fast` is the client's only way to ask for the priority tier, so it sets `service_tier`
/// unless the caller already chose a tier itself.
fn outbound_body(raw: &Bytes, model: &str, fast: bool) -> Result<Value, Failure> {
    let mut body: Value = serde_json::from_slice(raw).map_err(|_| bad_body())?;
    if !body.is_object() {
        return Err(bad_body());
    }
    let object = body.as_object_mut().expect("object");
    object.remove("previous_response_id");
    strip_identity(object);
    object.remove("metadata");
    object.remove("max_output_tokens");
    for key in [
        "temperature",
        "top_p",
        "stop",
        "user",
        "prompt_cache_options",
    ] {
        object.remove(key);
    }
    if model == "gpt-5.6" || model.starts_with("gpt-5.6-") {
        object.remove("prompt_cache_retention");
    }
    object.remove("truncation");
    fold_system_messages(object);
    object.insert("model".into(), Value::String(model.to_owned()));
    if fast && object.get("service_tier").is_none_or(Value::is_null) {
        object.insert("service_tier".into(), Value::String("priority".into()));
    }
    if object.get("store").and_then(Value::as_bool) == Some(false) {
        strip_continuation_markers(object);
    }
    object.insert("stream".into(), Value::Bool(true));
    Ok(body)
}

fn bad_body() -> Failure {
    let error = Error::bad_request("request body must be a JSON object");
    Failure::Fatal {
        status: StatusCode::BAD_REQUEST,
        body: Bytes::from(
            serde_json::to_vec(&error.to_body(crate::translate::Inbound::Responses))
                .unwrap_or_default(),
        ),
    }
}

/// Delete the identity sentences a routed proxy leaves behind; empty prompts are removed.
fn strip_identity(object: &mut serde_json::Map<String, Value>) {
    let instructions = object.get("instructions").and_then(Value::as_str);
    if let Some(next) = instructions.and_then(strip_routed_identity) {
        if next.is_empty() {
            object.remove("instructions");
        } else {
            object.insert("instructions".into(), Value::String(next));
        }
    }
    let Some(Value::Array(items)) = object.get_mut("input") else {
        return;
    };
    let mut kept = Vec::with_capacity(items.len());
    for mut item in items.drain(..) {
        let emptied = match item.as_object_mut().filter(|item| prompt_message(item)) {
            Some(message) => strip_prompt_content(message),
            None => false,
        };
        if !emptied {
            kept.push(item);
        }
    }
    *items = kept;
}

/// Strip the text of one privileged item; `true` when nothing the caller wrote remains.
fn strip_prompt_content(message: &mut serde_json::Map<String, Value>) -> bool {
    match message.get_mut("content") {
        Some(Value::String(text)) => match strip_routed_identity(text) {
            Some(next) => {
                let emptied = next.is_empty();
                *text = next;
                emptied
            }
            None => false,
        },
        Some(Value::Array(parts)) => {
            let mut changed = false;
            let mut all_empty = true;
            for part in parts.iter_mut() {
                let Some(part) = part.as_object_mut() else {
                    all_empty = false;
                    continue;
                };
                match part.get("text").and_then(Value::as_str).map(str::to_owned) {
                    Some(text) => match strip_routed_identity(&text) {
                        Some(next) => {
                            changed = true;
                            all_empty &= next.is_empty();
                            part.insert("text".into(), Value::String(next));
                        }
                        None => all_empty &= text.is_empty(),
                    },
                    None => all_empty = false,
                }
            }
            changed && all_empty
        }
        _ => false,
    }
}

/// A privileged prompt item; the Responses backend accepts `system` text only at the top level.
fn prompt_message(message: &serde_json::Map<String, Value>) -> bool {
    let message_kind = match message.get("type") {
        None => true,
        Some(Value::String(kind)) => kind == "message",
        Some(_) => false,
    };
    message_kind
        && matches!(
            message.get("role").and_then(Value::as_str),
            Some("developer" | "system")
        )
}

/// The literal and the named sentence this proxy generates for routed models.
const NEUTRAL_IDENTITY: &str =
    "You are a coding agent. Do not claim to be GPT-5 or to be made by OpenAI.";
const ROUTED_HEAD: &str = "You are a coding agent powered by the ";
const ROUTED_MID: &str = ". If asked which model you are, identify as ";
const ROUTED_TAIL: &str = ". Do not claim to be ";
const ROUTED_CLAIMS: [&str; 2] = [
    "a different model or to have a different creator.",
    "GPT-5 or made by OpenAI.",
];

/// Remove every identity sentence this proxy generated; `None` when the text carries none.
fn strip_routed_identity(text: &str) -> Option<String> {
    let mut out = text.to_owned();
    let mut changed = false;
    while let Some(at) = out.find(NEUTRAL_IDENTITY) {
        out.replace_range(at..at + NEUTRAL_IDENTITY.len(), "");
        changed = true;
    }
    while let Some((start, end)) = routed_identity_at(&out) {
        out.replace_range(start..end, "");
        changed = true;
    }
    if !changed {
        return None;
    }
    // Collapse the gap a removed sentence leaves behind, then trim like the source does.
    let mut collapsed = String::with_capacity(out.len());
    let mut newlines = 0;
    for character in out.chars() {
        if character == '\n' {
            newlines += 1;
            if newlines > 2 {
                continue;
            }
        } else {
            newlines = 0;
        }
        collapsed.push(character);
    }
    Some(collapsed.trim().to_owned())
}

/// Byte range of one `ROUTED_*` sentence in `text`, if the whole sequence matches.
fn routed_identity_at(text: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(offset) = text[from..].find(ROUTED_HEAD) {
        let start = from + offset;
        if let Some(end) = routed_identity_end(text, start) {
            return Some((start, end));
        }
        from = start + ROUTED_HEAD.len();
    }
    None
}

fn routed_identity_end(text: &str, start: usize) -> Option<usize> {
    let mut at = start + ROUTED_HEAD.len();
    at = model_component(text, at)?;
    at = text[at..]
        .starts_with(ROUTED_MID)
        .then_some(at + ROUTED_MID.len())?;
    at = model_component(text, at)?;
    at = text[at..]
        .starts_with(ROUTED_TAIL)
        .then_some(at + ROUTED_TAIL.len())?;
    ROUTED_CLAIMS
        .iter()
        .find(|claim| text[at..].starts_with(*claim))
        .map(|claim| at + claim.len())
}

/// `configured model` or a model id, matching the character class the proxy can emit.
fn model_component(text: &str, at: usize) -> Option<usize> {
    if text[at..].starts_with("configured model") {
        return Some(at + "configured model".len());
    }
    let len = text[at..]
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || b"._/@:+-[]~".contains(byte))
        .count();
    (len > 0).then_some(at + len)
}

/// Fold textual system input items into `instructions`, as this backend rejects them.
fn fold_system_messages(object: &mut serde_json::Map<String, Value>) {
    let Some(Value::Array(items)) = object.get("input") else {
        return;
    };
    let mut folded = Vec::new();
    let mut removable = 0;
    for item in items {
        let Some(message) = item.as_object() else {
            continue;
        };
        if !(message.get("type").is_none() || message["type"] == "message")
            || message["role"] != "system"
        {
            continue;
        }
        match system_text(message) {
            Some(text) => {
                folded.push(text);
                removable += 1;
            }
            // One multimodal system item keeps every item in place.
            None => return,
        }
    }
    if removable == 0 {
        return;
    }
    let Value::Array(items) = object.get_mut("input").expect("input") else {
        return;
    };
    items.retain(|item| {
        !item.as_object().is_some_and(|message| {
            (message.get("type").is_none() || message["type"] == "message")
                && message["role"] == "system"
                && system_text(message).is_some()
        })
    });
    let folded = folded.join("\n\n");
    if folded.is_empty() {
        return;
    }
    let existing = object
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let joined = if existing.is_empty() {
        folded
    } else {
        format!("{existing}\n\n{folded}")
    };
    object.insert("instructions".into(), Value::String(joined));
}

/// Lossless text of one system message; `None` when it carries anything but text.
fn system_text(message: &serde_json::Map<String, Value>) -> Option<String> {
    match message.get("content") {
        None => Some(String::new()),
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            for block in blocks {
                let block = block.as_object()?;
                if block["type"] != "input_text" && block["type"] != "text" {
                    return None;
                }
                text.push_str(block.get("text")?.as_str()?);
            }
            Some(text)
        }
        Some(_) => None,
    }
}

/// This backend cannot resolve stored references or client cache markers on a `store: false` turn.
fn strip_continuation_markers(object: &mut serde_json::Map<String, Value>) {
    let Some(Value::Array(items)) = object.get_mut("input") else {
        return;
    };
    items.retain(|item| {
        !item
            .as_object()
            .is_some_and(|item| item["type"] == "item_reference")
    });
    // The marker walk aborts atomically when a hostile body exceeds the budget.
    if !cache_budget_ok(items) {
        return;
    }
    let mut input = Value::Array(std::mem::take(items));
    strip_cache_markers(&mut input);
    if let Value::Array(rewritten) = input {
        *items = rewritten;
    }
}

const CACHE_MAX_DEPTH: usize = 64;
const CACHE_MAX_NODES: usize = 100_000;

fn cache_budget_ok(items: &[Value]) -> bool {
    fn walk(value: &Value, depth: usize, nodes: &mut usize) -> bool {
        *nodes += 1;
        if depth > CACHE_MAX_DEPTH || *nodes > CACHE_MAX_NODES {
            return false;
        }
        match value {
            Value::Array(items) => items.iter().all(|item| walk(item, depth + 1, nodes)),
            Value::Object(map) => map.values().all(|item| walk(item, depth + 1, nodes)),
            _ => true,
        }
    }
    let mut nodes = 0;
    items.iter().all(|item| walk(item, 1, &mut nodes))
}

fn strip_cache_markers(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(strip_cache_markers),
        Value::Object(map) => {
            map.remove("prompt_cache_breakpoint");
            map.values_mut().for_each(strip_cache_markers);
        }
        _ => {}
    }
}

/// Longest error body kept for the client and for quota-code inspection.
const MAX_ERROR_BODY: usize = 64 * 1024;
/// Longest buffered non-streaming reply before giving up.
const MAX_FOLD_BODY: usize = 32 * 1024 * 1024;

async fn reject(status: StatusCode, response: reqwest::Response) -> Failure {
    let retry_after = retry_after(response.headers());
    let mut body = response.bytes_stream();
    let mut collected = Vec::new();
    while let Some(chunk) = body.next().await {
        let Ok(chunk) = chunk else { break };
        collected.extend_from_slice(&chunk);
        if collected.len() > MAX_ERROR_BODY {
            break;
        }
    }
    let quota = QUOTA_CODES.iter().any(|code| names_code(&collected, code));
    match status.as_u16() {
        401 | 403 => Failure::Auth,
        402 => Failure::Quota {
            reset_after: retry_after,
        },
        429 if quota => Failure::Quota {
            reset_after: retry_after,
        },
        429 => Failure::RateLimited { retry_after },
        code if (500..600).contains(&code) => Failure::Retryable,
        _ => Failure::Fatal {
            status,
            body: Bytes::from(collected),
        },
    }
}

/// Whether the body names `code` in a `code` or `type` field at the roots the backend uses.
fn names_code(body: &[u8], code: &str) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    ["", "/error", "/detail"].iter().any(|root| {
        ["code", "type"].iter().any(|field| {
            let pointer = format!("{root}/{field}");
            value.pointer(&pointer).and_then(Value::as_str) == Some(code)
        })
    })
}

/// `Retry-After` as seconds or an HTTP date; a past date means retry now.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = raw.parse::<f64>() {
        return Some(Duration::from_secs_f64(seconds.max(0.0)));
    }
    let at = http_date_seconds(raw)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Some(Duration::from_secs(at.saturating_sub(now)))
}

/// Seconds since the epoch for an `IMF-fixdate`, e.g. `Sun, 06 Nov 1994 08:49:37 GMT`.
fn http_date_seconds(value: &str) -> Option<u64> {
    let rest = value
        .split_once(", ")
        .map(|(_, rest)| rest)
        .unwrap_or(value);
    let parts: Vec<&str> = rest.split(' ').filter(|part| !part.is_empty()).collect();
    let [day, month, year, clock, zone] = parts.as_slice() else {
        return None;
    };
    if !zone.eq_ignore_ascii_case("GMT") {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|name| name.eq_ignore_ascii_case(month))?
        + 1;
    let year: i64 = year.parse().ok()?;
    let day: i64 = day.parse().ok()?;
    let [hour, minute, second]: [i64; 3] = clock
        .split(':')
        .map(|part| part.parse().ok())
        .collect::<Option<Vec<i64>>>()?
        .try_into()
        .ok()?;
    let days = days_from_civil(year, month as i64, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    u64::try_from(seconds).ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Response headers worth relaying; the rest is transport noise or backend internals.
fn reply_headers(upstream: &HeaderMap) -> HeaderMap {
    const RELAYED: [&str; 7] = [
        "content-type",
        "retry-after",
        "x-request-id",
        "openai-request-id",
        "x-codex-turn-state",
        "openai-model",
        "x-models-etag",
    ];
    let mut headers = HeaderMap::new();
    for (name, value) in upstream {
        let name = name.as_str();
        let quota_family = name.starts_with("x-codex")
            && (name.ends_with("-used-percent")
                || name.ends_with("-window-minutes")
                || name.ends_with("-reset-at")
                || name.ends_with("-limit-name"));
        let relayed = RELAYED.contains(&name)
            || name == "x-reasoning-included"
            || name.starts_with("x-ratelimit-")
            || name.starts_with("x-codex-credits-")
            || name.starts_with("x-codex-safety-buffering-")
            || quota_family;
        if let (true, Ok(name)) = (relayed, HeaderName::from_bytes(name.as_bytes())) {
            headers.append(name, value.clone());
        }
    }
    headers
}

/// Streaming reply: relay the upstream bytes untouched, never buffering the body.
fn relay(response: reqwest::Response) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    response
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other))
        .boxed()
}

/// Upstream Responses SSE rewritten as Chat completion chunks.
fn chat_stream(
    response: reqwest::Response,
    model: &str,
    include_usage: bool,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    let state = (
        response.bytes_stream().boxed(),
        SseParser::new(),
        crate::translate::ChatStream::new(model, include_usage),
        false,
    );
    stream::unfold(
        state,
        |(mut source, mut parser, mut encoder, ended)| async move {
            if ended {
                return None;
            }
            loop {
                match source.next().await {
                    Some(Ok(chunk)) => {
                        let Ok(payloads) = parser.push(&chunk) else {
                            let out = encoder.finish();
                            return Some((Ok(Bytes::from(out)), (source, parser, encoder, true)));
                        };
                        let out: Vec<u8> = payloads
                            .iter()
                            .flat_map(|payload| encoder.push_event(payload))
                            .collect();
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), (source, parser, encoder, false)));
                        }
                    }
                    Some(Err(error)) => {
                        return Some((
                            Err(std::io::Error::other(error)),
                            (source, parser, encoder, true),
                        ));
                    }
                    None => {
                        let out = encoder.finish();
                        if out.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(out)), (source, parser, encoder, true)));
                    }
                }
            }
        },
    )
    .boxed()
}

/// Folded Responses JSON rewritten as a Chat completion.
async fn chat_fold(
    folded: BoxStream<'static, Result<Bytes, std::io::Error>>,
    model: &str,
) -> Result<BoxStream<'static, Result<Bytes, std::io::Error>>, Failure> {
    let mut bytes = Vec::new();
    let mut folded = folded;
    while let Some(chunk) = folded.next().await {
        bytes.extend_from_slice(&chunk.map_err(|_| Failure::Retryable)?);
    }
    let response: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::Retryable)?;
    let chat = crate::translate::responses_reply_to_chat(&response, model)
        .map_err(|_| Failure::Retryable)?;
    let bytes = serde_json::to_vec(&chat).map_err(|_| Failure::Retryable)?;
    Ok(stream::once(async move { Ok(Bytes::from(bytes)) }).boxed())
}

/// Non-streaming reply: fold the SSE terminal event into one JSON Responses object.
async fn fold(
    response: reqwest::Response,
) -> Result<BoxStream<'static, Result<Bytes, std::io::Error>>, Failure> {
    let mut parser = SseParser::new();
    let mut stream = response.bytes_stream();
    let mut used = 0usize;
    let mut terminal = None;
    'read: while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Failure::Retryable)?;
        used += chunk.len();
        if used > MAX_FOLD_BODY {
            return Err(Failure::Retryable);
        }
        for payload in parser.push(&chunk).map_err(|_| Failure::Retryable)? {
            if let Some(mut response) = terminal_response(&payload) {
                if let Some(object) = response.as_object_mut() {
                    object
                        .entry("output")
                        .or_insert_with(|| Value::Array(Vec::new()));
                }
                terminal = Some(response);
                break 'read;
            }
        }
    }
    // A stream that ended without a terminal event has no answer to deliver.
    let Some(response) = terminal else {
        return Err(Failure::Retryable);
    };
    let bytes = serde_json::to_vec(&response).map_err(|_| Failure::Retryable)?;
    Ok(stream::once(async move { Ok(Bytes::from(bytes)) }).boxed())
}

/// The `response` object of a terminal event, with the event's status applied.
fn terminal_response(payload: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(payload).ok()?;
    let status = match value.get("type").and_then(Value::as_str)? {
        "response.completed" => "completed",
        "response.incomplete" => "incomplete",
        "response.failed" => "failed",
        _ => return None,
    };
    let mut response = value.get("response")?.clone();
    let object = response.as_object_mut()?;
    if object
        .get("output")
        .is_some_and(|output| !output.is_array())
    {
        return None;
    }
    object.insert("status".into(), Value::String(status.into()));
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One raw HTTP/1.1 responder, so egress is asserted on the wire bytes.
    struct Upstream {
        addr: std::net::SocketAddr,
        requests: Arc<Mutex<Vec<String>>>,
    }

    struct Captured {
        line: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Captured {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }

        fn names(&self) -> Vec<&str> {
            self.headers.iter().map(|(key, _)| key.as_str()).collect()
        }
    }

    impl Upstream {
        async fn start(status: &str, headers: &[(&str, &str)], body: &str) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let mut response = format!("HTTP/1.1 {status}\r\n");
            for (name, value) in headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str(&format!(
                "content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            ));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let raw = read_request(&mut socket).await;
                    seen.lock().unwrap().push(raw);
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
            });
            Self { addr, requests }
        }

        fn base(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn captured(&self) -> Captured {
            let raw = self.requests.lock().unwrap().first().cloned().unwrap();
            capture(&raw)
        }
    }

    fn capture(raw: &str) -> Captured {
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
        let mut lines = head.split("\r\n");
        let line = lines.next().unwrap_or_default().to_owned();
        let headers = lines
            .filter_map(|line| line.split_once(": "))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        Captured {
            line,
            headers,
            body: body.to_owned(),
        }
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                return String::from_utf8_lossy(&buf).into_owned();
            }
            buf.extend_from_slice(&chunk[..read]);
            let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            while buf.len() < end + 4 + length {
                let read = socket.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..read]);
            }
            return String::from_utf8_lossy(&buf).into_owned();
        }
    }

    fn adapter(upstream: &Upstream) -> OpenAi {
        OpenAi::new(reqwest::Client::new(), Some(upstream.base()))
    }

    fn account() -> Account {
        Account {
            id: "pool-1".into(),
            credential: Credential::OAuth {
                access: "pool-token".into(),
                refresh: None,
                expires_at: None,
            },
            extra: BTreeMap::from([("account_id".to_owned(), "acct-123".to_owned())]),
        }
    }

    fn request(
        inbound: Inbound,
        headers: &[(&'static str, &'static str)],
        body: &Value,
        stream: bool,
    ) -> Request {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        Request {
            inbound,
            model: "gpt-5.1-codex".into(),
            headers: map,
            body: Bytes::from(serde_json::to_vec(body).unwrap()),
            stream,
            session: "session-1".into(),
            fast: None,
        }
    }

    async fn failure(adapter: &OpenAi, request: &Request) -> Failure {
        failure_for(adapter, &account(), request).await
    }

    async fn failure_for(adapter: &OpenAi, account: &Account, request: &Request) -> Failure {
        match adapter.send(request, account).await {
            Err(failure) => failure,
            Ok(_) => panic!("expected a failure"),
        }
    }

    async fn body_text(reply: Reply) -> String {
        let mut out = Vec::new();
        let mut stream = reply.body;
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        String::from_utf8(out).unwrap()
    }

    const ROUTING: &str = "model=gpt-5.1-codex;tier=priority";

    fn codex_body() -> Value {
        serde_json::json!({
            "model": "gpt-5.1-codex",
            "service_tier": "priority",
            "instructions": "caller instructions",
            "previous_response_id": "resp_1",
            "metadata": {"trace": "abc"},
            "max_output_tokens": 900,
            "temperature": 0.5,
            "top_p": 0.9,
            "stop": ["done"],
            "user": "caller",
            "prompt_cache_options": {"mode": "explicit"},
            "truncation": "auto",
            "prompt_cache_retention": "24h",
            "store": false,
            "stream": false,
            "input": [
                {"type": "message", "role": "user", "content": "hello"},
                {"type": "message", "role": "system", "content": [
                    {"type": "input_text", "text": "be terse"}
                ]},
                {"type": "item_reference", "id": "rs_1"},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "later", "prompt_cache_breakpoint": true}
                ]}
            ]
        })
    }

    #[tokio::test]
    async fn forwards_canonical_responses_egress() {
        let upstream =
            Upstream::start("200 OK", &[("content-type", "text/event-stream")], "").await;
        let req = request(
            Inbound::Responses,
            &[
                ("authorization", "Bearer caller-token"),
                ("chatgpt-account-id", "caller-account"),
                ("openai-beta", "responses=experimental"),
                ("originator", "codex_cli_rs"),
                ("session_id", "caller-session"),
                ("x-codex-turn-state", "turn-state"),
                ("x-client-request-id", "req-1"),
                ("accept", "text/event-stream"),
            ],
            &codex_body(),
            true,
        );
        let reply = adapter(&upstream).send(&req, &account()).await.unwrap();
        assert_eq!(reply.status, StatusCode::OK);

        let captured = upstream.captured();
        assert_eq!(captured.line, "POST /responses HTTP/1.1");
        // Content-Type first, then the allowlist order, then the rewritten pool identity.
        let ours: Vec<&str> = captured
            .names()
            .into_iter()
            .filter(|name| {
                FORWARD_HEADERS.contains(name)
                    || *name == CONTENT_TYPE.as_str()
                    || *name == ROUTING_HINT
                    || *name == "user-agent"
            })
            .collect();
        assert_eq!(
            ours,
            vec![
                "content-type",
                "authorization",
                "chatgpt-account-id",
                "openai-beta",
                "originator",
                "session_id",
                "x-client-request-id",
                "x-codex-turn-state",
                ROUTING_HINT,
            ]
        );
        assert_eq!(captured.header("authorization"), Some("Bearer pool-token"));
        assert_eq!(captured.header("chatgpt-account-id"), Some("acct-123"));
        assert_eq!(captured.header(ROUTING_HINT), Some(ROUTING));
        assert_eq!(
            captured.header(CONTENT_TYPE.as_str()),
            Some("application/json")
        );
        // The caller's Accept is not allowlisted; reqwest's default takes its place.
        assert_eq!(captured.header("accept"), Some("*/*"));
        assert_eq!(captured.header("user-agent"), None);

        let body: Value = serde_json::from_str(&captured.body).unwrap();
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "model",
                "service_tier",
                "instructions",
                "input",
                "stream",
                "store",
                "prompt_cache_retention"
            ]
        );
        assert_eq!(body["instructions"], "caller instructions\n\nbe terse");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
        assert_eq!(
            body["input"][1]["content"][0].get("prompt_cache_breakpoint"),
            None
        );
        assert_eq!(body["input"][0]["content"], "hello");
    }

    #[tokio::test]
    async fn fast_suffix_sends_the_routed_model_and_the_priority_tier() {
        let upstream = Upstream::start("200 OK", &[], "").await;
        let body = serde_json::json!({"model": "gpt-6.1-sol--fast", "input": "hi"});
        let mut req = request(Inbound::Responses, &[], &body, true);
        req.model = "gpt-6.1-sol".into();
        req.fast = Some(true);
        adapter(&upstream).send(&req, &account()).await.unwrap();

        let captured = upstream.captured();
        let sent: Value = serde_json::from_str(&captured.body).unwrap();
        assert_eq!(sent["model"], "gpt-6.1-sol");
        assert_eq!(sent["service_tier"], "priority");
        assert_eq!(
            captured.header(ROUTING_HINT),
            Some("model=gpt-6.1-sol;tier=priority")
        );
    }

    #[tokio::test]
    async fn fast_suffix_keeps_a_tier_the_caller_chose() {
        let upstream = Upstream::start("200 OK", &[], "").await;
        let body = serde_json::json!({
            "model": "gpt-6.1-sol--fast",
            "service_tier": "flex",
            "input": "hi"
        });
        let mut req = request(Inbound::Responses, &[], &body, true);
        req.model = "gpt-6.1-sol".into();
        req.fast = Some(true);
        adapter(&upstream).send(&req, &account()).await.unwrap();

        let captured = upstream.captured();
        let sent: Value = serde_json::from_str(&captured.body).unwrap();
        assert_eq!(sent["service_tier"], "flex");
    }

    #[tokio::test]
    async fn routing_hint_keeps_valid_components_only() {
        let upstream = Upstream::start("200 OK", &[], "").await;
        let body = serde_json::json!({"model": "gpt-5.1-codex", "input": "hi"});
        let reply = adapter(&upstream)
            .send(&request(Inbound::Responses, &[], &body, true), &account())
            .await
            .unwrap();
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            upstream.captured().header(ROUTING_HINT),
            Some("model=gpt-5.1-codex")
        );
    }

    #[tokio::test]
    async fn routing_hint_is_dropped_for_invalid_tier() {
        let upstream = Upstream::start("200 OK", &[], "").await;
        let body = serde_json::json!({
            "model": "gpt-5.1-codex",
            "service_tier": "pri ority",
            "input": "hi"
        });
        adapter(&upstream)
            .send(&request(Inbound::Responses, &[], &body, true), &account())
            .await
            .unwrap();
        assert_eq!(upstream.captured().header(ROUTING_HINT), None);
    }

    const SSE: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}]}}\n\n";

    #[tokio::test]
    async fn streaming_reply_relays_upstream_bytes_verbatim() {
        let upstream = Upstream::start(
            "200 OK",
            &[
                ("content-type", "text/event-stream"),
                ("x-request-id", "req-1"),
                ("x-internal-debug", "secret"),
            ],
            SSE,
        )
        .await;
        let body = serde_json::json!({"model": "gpt-5.1-codex", "input": "hi"});
        let reply = adapter(&upstream)
            .send(&request(Inbound::Responses, &[], &body, true), &account())
            .await
            .unwrap();
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.headers.get(CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(reply.headers.get("x-request-id").unwrap(), "req-1");
        assert_eq!(reply.headers.get("x-internal-debug"), None);
        assert_eq!(body_text(reply).await, SSE);
    }

    #[tokio::test]
    async fn chat_inbound_is_translated_both_ways() {
        let upstream =
            Upstream::start("200 OK", &[("content-type", "text/event-stream")], SSE).await;
        let body = serde_json::json!({
            "model": "gpt-5.1-codex",
            "messages": [{"role": "user", "content": "hi"}],
            "stream_options": {"include_usage": true},
        });
        let reply = adapter(&upstream)
            .send(&request(Inbound::Chat, &[], &body, true), &account())
            .await
            .unwrap();
        let text = body_text(reply).await;
        assert!(text.contains("chat.completion.chunk"), "{text}");
        assert!(text.contains("\"content\":\"hi\""), "{text}");
        assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
        let sent = upstream.requests.lock().unwrap()[0].clone();
        assert!(sent.contains("\"input\""), "{sent}");
        assert!(!sent.contains("\"messages\""), "{sent}");

        let reply = adapter(&upstream)
            .send(&request(Inbound::Chat, &[], &body, false), &account())
            .await
            .unwrap();
        let chat: Value = serde_json::from_str(&body_text(reply).await).unwrap();
        assert_eq!(chat["object"], "chat.completion");
        assert_eq!(chat["choices"][0]["message"]["content"], "hi");
    }

    #[tokio::test]
    async fn non_streaming_reply_folds_the_terminal_event() {
        let upstream =
            Upstream::start("200 OK", &[("content-type", "text/event-stream")], SSE).await;
        let body = serde_json::json!({"model": "gpt-5.1-codex", "input": "hi"});
        let reply = adapter(&upstream)
            .send(&request(Inbound::Responses, &[], &body, false), &account())
            .await
            .unwrap();
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.headers.get(CONTENT_TYPE).unwrap(), "application/json");
        let folded: Value = serde_json::from_str(&body_text(reply).await).unwrap();
        assert_eq!(folded["id"], "resp_1");
        assert_eq!(folded["status"], "completed");
        assert_eq!(folded["output"][0]["content"][0]["text"], "hi");
    }

    #[tokio::test]
    async fn non_streaming_reply_without_terminal_is_retryable() {
        let upstream = Upstream::start(
            "200 OK",
            &[("content-type", "text/event-stream")],
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
        )
        .await;
        let body = serde_json::json!({"model": "gpt-5.1-codex", "input": "hi"});
        let failure = failure(
            &adapter(&upstream),
            &request(Inbound::Responses, &[], &body, false),
        )
        .await;
        assert!(matches!(failure, Failure::Retryable), "{failure:?}");
    }
    #[tokio::test]
    async fn error_replies_map_to_failure() {
        #[allow(clippy::type_complexity)]
        let cases: [(&str, &[(&str, &str)], &str, Failure); 6] = [
            (
                "429 Too Many Requests",
                &[("retry-after", "12")],
                "",
                Failure::RateLimited {
                    retry_after: Some(Duration::from_secs(12)),
                },
            ),
            (
                "429 Too Many Requests",
                &[],
                "{\"error\":{\"code\":\"usage_limit_exceeded\"}}",
                Failure::Quota { reset_after: None },
            ),
            ("401 Unauthorized", &[], "", Failure::Auth),
            (
                "403 Forbidden",
                &[],
                "{\"detail\":{\"type\":\"insufficient_quota\"}}",
                Failure::Auth,
            ),
            (
                "402 Payment Required",
                &[],
                "",
                Failure::Quota { reset_after: None },
            ),
            ("503 Service Unavailable", &[], "", Failure::Retryable),
        ];
        for (status, headers, body, expected) in cases {
            let upstream = Upstream::start(status, headers, body).await;
            let request = request(
                Inbound::Responses,
                &[],
                &serde_json::json!({"model": "gpt-5.1-codex", "input": "hi"}),
                true,
            );
            let failure = failure(&adapter(&upstream), &request).await;
            assert_eq!(failure, expected, "{status} {body}");
        }
    }

    #[tokio::test]
    async fn client_error_keeps_status_and_body() {
        let upstream = Upstream::start(
            "400 Bad Request",
            &[("content-type", "application/json")],
            "{\"error\":{\"message\":\"bad input\"}}",
        )
        .await;
        let request = request(
            Inbound::Responses,
            &[],
            &serde_json::json!({"model": "gpt-5.1-codex", "input": "hi"}),
            true,
        );
        match failure(&adapter(&upstream), &request).await {
            Failure::Fatal { status, body } => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert_eq!(&body[..], br#"{"error":{"message":"bad input"}}"#);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn messages_inbound_is_rejected() {
        {
            let inbound = Inbound::Messages;
            let upstream = Upstream::start("200 OK", &[], "").await;
            let request = request(inbound, &[], &serde_json::json!({"model": "m"}), false);
            match failure(&adapter(&upstream), &request).await {
                Failure::Fatal { status, body } => {
                    assert_eq!(status, StatusCode::BAD_REQUEST);
                    assert!(String::from_utf8_lossy(&body)
                        .contains("unsupported protocol for openai yet"));
                }
                other => panic!("unexpected {other:?}"),
            }
            assert!(upstream.requests.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn unreachable_upstream_is_retryable() {
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let adapter = OpenAi::new(
            reqwest::Client::new(),
            Some(format!("http://127.0.0.1:{port}")),
        );
        let request = request(
            Inbound::Responses,
            &[],
            &serde_json::json!({"model": "gpt-5.1-codex", "input": "hi"}),
            true,
        );
        assert!(matches!(
            failure(&adapter, &request).await,
            Failure::Retryable
        ));
    }

    #[tokio::test]
    async fn missing_pool_identity_is_an_auth_failure() {
        let mut no_account = account();
        no_account.extra.clear();
        let request = request(
            Inbound::Responses,
            &[],
            &serde_json::json!({"model": "gpt-5.1-codex", "input": "hi"}),
            true,
        );
        // No upstream is dialled, so an unreachable base is enough.
        let adapter = OpenAi::new(reqwest::Client::new(), Some("http://127.0.0.1:1".into()));
        assert_eq!(
            failure_for(&adapter, &no_account, &request).await,
            Failure::Auth
        );
        let api_key = Account {
            credential: Credential::ApiKey("sk-test".into()),
            ..account()
        };
        assert_eq!(
            failure_for(&adapter, &api_key, &request).await,
            Failure::Auth
        );
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(7)));
        headers.insert(
            http::header::RETRY_AFTER,
            HeaderValue::from_static("Sun, 06 Nov 1994 08:49:37 GMT"),
        );
        assert_eq!(retry_after(&headers), Some(Duration::ZERO));
        headers.insert(
            http::header::RETRY_AFTER,
            HeaderValue::from_static("Sun, 06 Nov 2094 08:49:37 GMT"),
        );
        assert_eq!(
            http_date_seconds("Sun, 06 Nov 2094 08:49:37 GMT"),
            Some(3_939_871_777)
        );
    }

    #[test]
    fn identity_sentences_are_removed() {
        let routed = "You are a coding agent powered by the configured model. If asked which model you are, identify as configured model. Do not claim to be GPT-5 or made by OpenAI.";
        assert_eq!(strip_routed_identity(routed), Some(String::new()));
        assert_eq!(
            strip_routed_identity(&format!("keep {NEUTRAL_IDENTITY}")),
            Some("keep".to_owned())
        );
        assert_eq!(strip_routed_identity("plain instructions"), None);
    }
}
