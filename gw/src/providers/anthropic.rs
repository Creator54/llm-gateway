//! anthropic upstream adapter: the native Messages lane (Claude Pro/Max OAuth or an API key).
//!
//! Only a caller that already speaks Messages is served; the body is cut to the protocol's
//! field allowlist and re-shaped for the credential. Other inbound protocols have no
//! translation yet and are refused.

use crate::provider::{Account, Credential, Failure, Inbound, Provider, Reply, Request};
use crate::translate::Error as TranslateError;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_BASE: &str = "https://api.anthropic.com";
/// Anthropic's own API host; the only destination a plan credential is trusted for.
const FIRST_PARTY_HOST: &str = "api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const SDK_VERSION: &str = "0.74.0";
const NODE_VERSION: &str = "22.11.0";
const OAUTH_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
const CLAUDE_CODE_IDENTITY: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const TOOL_PREFIX: &str = "custom_";
/// Anthropic's own tools keep their names under the OAuth prefix.
const BUILTIN_TOOLS: [&str; 4] = ["web_search", "code_execution", "text_editor", "computer"];
/// Anthropic's error bodies are small; anything past this is not worth reading.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Top-level Messages fields the native lane forwards. Everything else is dropped.
const PASSTHROUGH_FIELDS: [&str; 15] = [
    "model",
    "messages",
    "system",
    "max_tokens",
    "metadata",
    "stop_sequences",
    "stream",
    "temperature",
    "top_p",
    "top_k",
    "tools",
    "tool_choice",
    "thinking",
    "output_config",
    "service_tier",
];

/// Messages requests to `api.anthropic.com`.
pub struct Anthropic {
    client: reqwest::Client,
    base_url: String,
}

impl Anthropic {
    pub fn new(client: reqwest::Client, base_url: Option<String>) -> Self {
        Self {
            client,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE.to_owned()),
        }
    }
}

/// One upstream request, fully decided before any I/O.
#[derive(Debug)]
struct Plan {
    url: String,
    /// Ordered as they must reach the wire.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    stream: bool,
    /// Wire tool name to the caller's name, for the OAuth rename.
    renames: HashMap<String, String>,
}

impl Provider for Anthropic {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn send<'a>(
        &'a self,
        req: &'a Request,
        account: &'a Account,
    ) -> futures_util::future::BoxFuture<'a, Result<Reply, Failure>> {
        Box::pin(async move {
            let plan = plan(&self.base_url, req, account)?;
            let mut outbound = self.client.post(&plan.url);
            for (name, value) in &plan.headers {
                outbound = outbound.header(name.as_str(), value.as_str());
            }
            let resp = match outbound.body(plan.body).send().await {
                Ok(resp) => resp,
                Err(error) => {
                    tracing::debug!(%error, "anthropic request failed");
                    return Err(Failure::Retryable);
                }
            };
            reply(resp, plan.stream, plan.renames).await
        })
    }
}

/// `{origin}/v1/messages` from any base URL spelling.
fn messages_url(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let root = strip_v1_messages(trimmed);
    let url = format!("{}/v1/messages", root.trim_end_matches('/'));
    if let (Some(open), Some(_)) = (url.find('{'), url.find('}')) {
        return Err(format!("base URL contains unresolved {}", &url[open..]));
    }
    Ok(url)
}

fn strip_v1_messages(url: &str) -> &str {
    match url.rsplit_once('/') {
        Some((head, tail)) if tail.eq_ignore_ascii_case("messages") => strip_v1(head),
        _ => strip_v1(url),
    }
}

fn strip_v1(url: &str) -> &str {
    match url.rsplit_once('/') {
        Some((head, tail)) if tail.eq_ignore_ascii_case("v1") => head,
        _ => url,
    }
}

/// Whether a base URL is Anthropic's own API over HTTPS on the default port.
fn first_party(base_url: &str) -> bool {
    let Some(rest) = base_url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.contains('@') && !host.contains(':') && host.eq_ignore_ascii_case(FIRST_PARTY_HOST)
}

/// Loopback upstreams (local Anthropic-compatible proxies, tests) are the one exception to
/// the first-party rule for an OAuth credential.
fn loopback(base_url: &str) -> bool {
    let rest = base_url
        .strip_prefix("http://")
        .or_else(|| base_url.strip_prefix("https://"))
        .unwrap_or("");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.rsplit_once(':').map_or(host, |(name, _)| name);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn fatal(inbound: Inbound, message: &str) -> Failure {
    let body = TranslateError::bad_request(message).to_body(protocol(inbound));
    Failure::Fatal {
        status: StatusCode::BAD_REQUEST,
        body: Bytes::from(body.to_string()),
    }
}

fn protocol(inbound: Inbound) -> crate::translate::Inbound {
    match inbound {
        Inbound::Chat => crate::translate::Inbound::Chat,
        Inbound::Responses => crate::translate::Inbound::Responses,
        Inbound::Messages => crate::translate::Inbound::Messages,
    }
}

/// Decide the whole request: URL, headers in order, wire body.
fn plan(base_url: &str, req: &Request, account: &Account) -> Result<Plan, Failure> {
    if req.inbound != Inbound::Messages {
        return Err(fatal(
            req.inbound,
            "this provider only accepts the Messages protocol",
        ));
    }
    let url = messages_url(base_url).map_err(|e| fatal(req.inbound, &e))?;
    let first_party = first_party(&url);
    let oauth = matches!(account.credential, Credential::OAuth { .. });
    if oauth && !first_party && !loopback(&url) {
        return Err(fatal(
            req.inbound,
            "a Claude OAuth credential is only sent to api.anthropic.com",
        ));
    }
    let caller: Map<String, Value> = serde_json::from_slice(&req.body)
        .map_err(|_| fatal(req.inbound, "request body is not a JSON object"))?;
    let stream = caller
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (body, renames) =
        wire_body(&caller, &req.model, oauth, first_party).map_err(|e| fatal(req.inbound, &e))?;
    let mut headers = base_headers(stream);
    match &account.credential {
        Credential::OAuth { access, .. } => oauth_auth(&mut headers, access),
        Credential::ApiKey(key) => key_auth(&mut headers, key, &account.extra),
        Credential::None => {
            return Err(Failure::Auth);
        }
    }
    let caller_beta = req
        .headers
        .get("anthropic-beta")
        .and_then(|value| value.to_str().ok());
    merge_betas(&mut headers, caller_beta, first_party);
    let body = serde_json::to_vec(&Value::Object(body))
        .map_err(|e| fatal(req.inbound, &format!("cannot encode request: {e}")))?;
    Ok(Plan {
        url,
        headers,
        body,
        stream,
        renames,
    })
}

fn base_headers(stream: bool) -> Vec<(String, String)> {
    vec![
        ("Content-Type".into(), "application/json".into()),
        ("anthropic-version".into(), API_VERSION.into()),
        (
            "Accept".into(),
            if stream {
                "text/event-stream".into()
            } else {
                "application/json".into()
            },
        ),
        (
            "User-Agent".into(),
            format!("@anthropic-ai/sdk/{SDK_VERSION}"),
        ),
    ]
}

/// `x-api-key` by default, or a bearer when the login asks for one.
fn key_auth(headers: &mut Vec<(String, String)>, key: &str, extra: &BTreeMap<String, String>) {
    let bearer = extra
        .get("api_key_transport")
        .is_some_and(|value| value == "bearer");
    if bearer {
        headers.push(("Authorization".into(), format!("Bearer {key}")));
    } else {
        headers.push(("x-api-key".into(), key.to_owned()));
    }
}

/// The bearer plus the Claude Code client fingerprint the credential was minted for.
fn oauth_auth(headers: &mut Vec<(String, String)>, access: &str) {
    let mut push = |name: &str, value: String| headers.push((name.to_owned(), value));
    push("Authorization", format!("Bearer {access}"));
    push("anthropic-beta", OAUTH_BETA.to_owned());
    push("X-App", "cli".into());
    push("X-Stainless-Retry-Count", "0".into());
    push("X-Stainless-Runtime", "node".into());
    push("X-Stainless-Lang", "js".into());
    push("X-Stainless-Timeout", "600".into());
    push("X-Stainless-Arch", node_arch());
    push("X-Stainless-OS", node_platform());
    push("X-Stainless-Package-Version", SDK_VERSION.into());
    push("X-Stainless-Runtime-Version", NODE_VERSION.into());
    push("X-Claude-Code-Session-Id", session_id(Some(access)));
    push("x-client-request-id", client_request_id());
}

/// `process.arch` spelling, which the fingerprint pins.
fn node_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        "loongarch64" => "loong64",
        other => other,
    }
    .to_owned()
}

/// `process.platform` spelling, which the fingerprint pins.
fn node_platform() -> String {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
    .to_owned()
}

/// Beta values a caller may forward, re-emitted in our own spelling; everything else is dropped.
fn allowed_betas(first_party: bool) -> &'static [&'static str] {
    if first_party {
        &["interleaved-thinking-2025-05-14"]
    } else {
        &[]
    }
}

/// Collapse every spelling of `anthropic-beta` into one canonical header with unique tokens.
fn merge_betas(headers: &mut Vec<(String, String)>, caller: Option<&str>, first_party: bool) {
    let mut tokens: Vec<String> = Vec::new();
    let add = |raw: &str, tokens: &mut Vec<String>| {
        for part in raw.split(',') {
            let token = part.trim();
            if token.is_empty() || tokens.iter().any(|kept| kept.eq_ignore_ascii_case(token)) {
                continue;
            }
            tokens.push(token.to_owned());
        }
    };
    for (_, value) in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
    {
        add(value, &mut tokens);
    }
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("anthropic-beta"));
    let forwarded = caller.filter(|value| value.len() <= 2048).unwrap_or("");
    for part in forwarded.split(',') {
        let token = part.trim().to_lowercase();
        if let Some(canonical) = allowed_betas(first_party)
            .iter()
            .find(|allowed| allowed.eq_ignore_ascii_case(&token))
        {
            add(canonical, &mut tokens);
        }
    }
    if !tokens.is_empty() {
        headers.push(("anthropic-beta".into(), tokens.join(",")));
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip(v) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0u8; 32];
    for (chunk, word) in out.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Shape 32 hex digits into a version-4 UUID, as Claude Code's session id is formed.
fn shaped_uuid(hex: &str) -> String {
    let nibble = |i: usize| hex.as_bytes()[i] as char;
    let digit = |c: char| c.to_digit(16).unwrap_or(0);
    let variant = ((digit(nibble(16)) & 0x3) | 0x8) & 0xf;
    let variant = format!("{variant:x}");
    format!(
        "{}-{}-4{}-{variant}{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Deterministic per-credential session id; the token never leaves this function.
fn session_id(token: Option<&str>) -> String {
    let seed = token
        .filter(|value| !value.is_empty())
        .unwrap_or("opencodex-anon");
    shaped_uuid(&hex(&sha256(
        format!("claude-code-session:{seed}").as_bytes(),
    )))
}

/// A fresh request id in UUID shape; only the shape matters to the upstream.
fn client_request_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos ^ NEXT.fetch_add(1, Ordering::Relaxed).rotate_left(17);
    let mut scratch = String::with_capacity(32);
    for _ in 0..2 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        scratch.push_str(&format!("{state:016x}"));
    }
    shaped_uuid(&scratch)
}

/// Wire tool name to the caller's name.
type Renames = HashMap<String, String>;

/// The allowlisted body with the routed model, plus the OAuth rename map.
fn wire_body(
    caller: &Map<String, Value>,
    model: &str,
    oauth: bool,
    first_party: bool,
) -> Result<(Map<String, Value>, Renames), String> {
    let mut body = Map::new();
    for (key, value) in caller {
        if PASSTHROUGH_FIELDS.contains(&key.as_str()) {
            body.insert(key.clone(), value.clone());
        }
    }
    body.insert("model".to_owned(), Value::String(model.to_owned()));
    if !first_party {
        strip_opaque_state(&mut body);
    }
    if oauth {
        let renames = oauth_shape(&mut body)?;
        return Ok((body, renames));
    }
    Ok((body, HashMap::new()))
}

/// Thinking signatures and redacted blocks only reach Anthropic itself.
fn strip_opaque_state(body: &mut Map<String, Value>) {
    fn opaque(block: &Value) -> bool {
        match block.get("type").and_then(Value::as_str) {
            Some("redacted_thinking") => true,
            Some("thinking") => block.get("signature").is_some(),
            _ => false,
        }
    }
    let Some(Value::Array(messages)) = body.get("messages") else {
        return;
    };
    if !messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| content.iter().any(opaque))
    }) {
        return;
    }
    let mut kept = Vec::with_capacity(messages.len());
    for message in messages {
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            kept.push(message.clone());
            continue;
        };
        if !content.iter().any(opaque) {
            kept.push(message.clone());
            continue;
        }
        let mut blocks = Vec::with_capacity(content.len());
        for block in content {
            if !opaque(block) {
                blocks.push(block.clone());
            } else if block.get("type").and_then(Value::as_str) == Some("thinking") {
                let mut visible = block.clone();
                if let Some(object) = visible.as_object_mut() {
                    object.remove("signature");
                }
                blocks.push(visible);
            }
        }
        if blocks.is_empty() {
            continue;
        }
        let mut message = message.clone();
        if let Some(object) = message.as_object_mut() {
            object.insert("content".to_owned(), Value::Array(blocks));
        }
        kept.push(message);
    }
    body.insert("messages".to_owned(), Value::Array(kept));
}

/// A tool the caller executes, as opposed to a typed server tool.
fn client_tool(tool: &Value) -> Option<&str> {
    let name = tool.get("name")?.as_str()?;
    match tool.get("type") {
        None => Some(name),
        Some(Value::String(kind)) if kind == "custom" => Some(name),
        _ => None,
    }
}

fn wire_tool_name(name: &str) -> String {
    let lowered = name.to_lowercase();
    if BUILTIN_TOOLS.contains(&lowered.as_str()) || lowered.starts_with(TOOL_PREFIX) {
        return name.to_owned();
    }
    format!("{TOOL_PREFIX}{name}")
}

/// The Claude Code request shape: identity system block first, client tool names prefixed.
fn oauth_shape(body: &mut Map<String, Value>) -> Result<Renames, String> {
    let identity = json!({ "type": "text", "text": CLAUDE_CODE_IDENTITY });
    let system = match body.get("system") {
        Some(Value::String(text)) if !text.is_empty() => Value::Array(vec![
            identity,
            json!({ "type": "text", "text": strip_docs(text) }),
        ]),
        Some(Value::Array(blocks)) => {
            let has_identity = blocks.first().and_then(|block| block.get("text"))
                == identity.get("text")
                && blocks
                    .first()
                    .and_then(|block| block.get("type"))
                    .and_then(Value::as_str)
                    == Some("text");
            let mut out = blocks.clone();
            for block in out.iter_mut() {
                let scrubbed = block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| text.contains("<docs>"))
                    .map(strip_docs);
                if let (Some(text), Some(object)) = (scrubbed, block.as_object_mut()) {
                    object.insert("text".to_owned(), Value::String(text));
                }
            }
            if has_identity {
                Value::Array(out)
            } else {
                let mut with_identity = vec![identity];
                with_identity.extend(out);
                Value::Array(with_identity)
            }
        }
        _ => Value::Array(vec![identity]),
    };
    body.insert("system".to_owned(), system);

    let mut renames = Renames::new();
    let mut owners: HashMap<String, String> = HashMap::new();
    let mut declare =
        |name: &str, renames: &mut HashMap<String, String>| -> Result<String, String> {
            let wire = wire_tool_name(name);
            if let Some(owner) = owners.get(&wire) {
                if owner != name {
                    return Err("tool names collide under the Claude OAuth tool prefix".to_owned());
                }
            }
            owners.insert(wire.clone(), name.to_owned());
            if wire != name {
                renames.insert(wire.clone(), name.to_owned());
            }
            Ok(wire)
        };
    let mut declared: Vec<String> = Vec::new();
    if let Some(Value::Array(tools)) = body.get("tools") {
        let mut out = Vec::with_capacity(tools.len());
        for tool in tools {
            match client_tool(tool) {
                Some(name) => {
                    declared.push(name.to_owned());
                    let wire = declare(name, &mut renames)?;
                    let mut tool = tool.clone();
                    if let Some(object) = tool.as_object_mut() {
                        object.insert("name".to_owned(), Value::String(wire));
                    }
                    out.push(tool);
                }
                None => out.push(tool.clone()),
            }
        }
        body.insert("tools".to_owned(), Value::Array(out));
    }
    let named_choice = body
        .get("tool_choice")
        .and_then(|choice| choice.get("name"))
        .and_then(Value::as_str)
        .filter(|name| declared.iter().any(|declared| declared == name))
        .map(str::to_owned);
    if let Some(name) = named_choice {
        let wire = declare(&name, &mut renames)?;
        let mut choice = body.get("tool_choice").cloned().unwrap_or_default();
        if let Some(object) = choice.as_object_mut() {
            object.insert("name".to_owned(), Value::String(wire));
        }
        body.insert("tool_choice".to_owned(), choice);
    }
    if let Some(Value::Array(messages)) = body.get("messages") {
        let mut out = Vec::with_capacity(messages.len());
        for message in messages {
            let Some(content) = message.get("content").and_then(Value::as_array) else {
                out.push(message.clone());
                continue;
            };
            let uses_renamed = content.iter().any(|block| {
                block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && block
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| declared.iter().any(|declared| declared == name))
            });
            if !uses_renamed {
                out.push(message.clone());
                continue;
            }
            let mut blocks = Vec::with_capacity(content.len());
            for block in content {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| {
                        block.get("type").and_then(Value::as_str) == Some("tool_use")
                            && declared.iter().any(|declared| declared == name)
                    })
                    .map(str::to_owned);
                match name {
                    Some(name) => {
                        let wire = declare(&name, &mut renames)?;
                        let mut block = block.clone();
                        if let Some(object) = block.as_object_mut() {
                            object.insert("name".to_owned(), Value::String(wire));
                        }
                        blocks.push(block);
                    }
                    None => blocks.push(block.clone()),
                }
            }
            let mut message = message.clone();
            if let Some(object) = message.as_object_mut() {
                object.insert("content".to_owned(), Value::Array(blocks));
            }
            out.push(message);
        }
        body.insert("messages".to_owned(), Value::Array(out));
    }
    Ok(renames)
}

/// Delete every `<docs>…</docs>` block: Anthropic reads it as a third-party harness.
fn strip_docs(text: &str) -> String {
    if !text.contains("<docs>") {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<docs>") {
        let mut cut = start;
        while cut > 0 && matches!(rest.as_bytes()[cut - 1], b' ' | b'\t') {
            cut -= 1;
        }
        out.push_str(&rest[..cut]);
        let after = &rest[start + "<docs>".len()..];
        let Some(end) = after.find("</docs>") else {
            out.push_str(&rest[cut..]);
            return out;
        };
        rest = &after[end + "</docs>".len()..];
        let bytes = rest.as_bytes();
        let mut skip = 0;
        while skip < bytes.len() && matches!(bytes[skip], b' ' | b'\t') {
            skip += 1;
        }
        if skip < bytes.len() && bytes[skip] == b'\n' {
            skip += 1;
        }
        rest = &rest[skip..];
    }
    out.push_str(rest);
    out
}

async fn reply(
    mut resp: reqwest::Response,
    stream: bool,
    renames: HashMap<String, String>,
) -> Result<Reply, Failure> {
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    if !status.is_success() {
        let body = read_capped(&mut resp, MAX_ERROR_BODY).await;
        return Err(failure(status, resp.headers(), &body));
    }
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        headers.insert(http::header::CONTENT_TYPE, value);
    }
    let sse = content_type.starts_with("text/event-stream");
    let body = if stream && sse {
        stream_body(resp, renames)
    } else {
        let bytes = resp.bytes().await.map_err(|_| Failure::Retryable)?;
        let bytes = if renames.is_empty() {
            bytes
        } else {
            rewrite_body(&bytes, &renames)
        };
        Box::pin(futures_util::stream::once(async move { Ok(bytes) })) as Body
    };
    Ok(Reply {
        status,
        headers,
        body,
    })
}

type Body = futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>>;

/// Upstream SSE, passed through unchanged unless a tool name must be mapped back.
fn stream_body(resp: reqwest::Response, renames: HashMap<String, String>) -> Body {
    let inner: Body = resp
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other))
        .boxed();
    if renames.is_empty() {
        return inner;
    }
    let state = (inner, Vec::<u8>::new(), renames);
    futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(end) = event_end(&state.1) {
                let event: Vec<u8> = state.1.drain(..end).collect();
                let rewritten = rewrite_event(&event, &state.2);
                return Some((Ok(rewritten), state));
            }
            match state.0.next().await {
                Some(Ok(chunk)) => state.1.extend_from_slice(&chunk),
                Some(Err(error)) => return Some((Err(error), state)),
                None => {
                    if state.1.is_empty() {
                        return None;
                    }
                    let rest = std::mem::take(&mut state.1);
                    return Some((Ok(Bytes::from(rest)), state));
                }
            }
        }
    })
    .boxed()
}

/// Byte offset just past the blank line that ends an SSE event.
fn event_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .map(|pos| pos + 2)
}

/// Rewrite a `tool_use` name back to the caller's name; every other event is untouched.
fn rewrite_event(event: &[u8], renames: &HashMap<String, String>) -> Bytes {
    let text = String::from_utf8_lossy(event);
    let data = text
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .map(|value| value.strip_prefix(' ').unwrap_or(value));
    let Some(data) = data else {
        return Bytes::copy_from_slice(event);
    };
    let Ok(mut value) = serde_json::from_str::<Value>(data.trim()) else {
        return Bytes::copy_from_slice(event);
    };
    if value.get("type").and_then(Value::as_str) != Some("content_block_start") {
        return Bytes::copy_from_slice(event);
    }
    let block = value.get_mut("content_block");
    let renamed = block
        .as_ref()
        .and_then(|block| block.get("type"))
        .and_then(Value::as_str)
        == Some("tool_use")
        && block
            .as_ref()
            .and_then(|block| block.get("name"))
            .and_then(Value::as_str)
            .is_some_and(|name| renames.contains_key(name));
    if !renamed {
        return Bytes::copy_from_slice(event);
    }
    let caller = block
        .as_ref()
        .and_then(|block| block.get("name"))
        .and_then(Value::as_str)
        .and_then(|name| renames.get(name))
        .cloned()
        .unwrap_or_default();
    if let Some(object) = block.and_then(Value::as_object_mut) {
        object.insert("name".to_owned(), Value::String(caller));
    }
    let event_name = text
        .lines()
        .find_map(|line| line.strip_prefix("event:"))
        .map(|value| value.trim().to_owned())
        .or_else(|| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default();
    Bytes::from(format!("event: {event_name}\ndata: {value}\n\n"))
}

/// Rewrite tool names in a whole Messages body.
fn rewrite_body(bytes: &Bytes, renames: &HashMap<String, String>) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<Value>(bytes) else {
        return bytes.clone();
    };
    if let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) {
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let caller = block
                .get("name")
                .and_then(Value::as_str)
                .and_then(|name| renames.get(name))
                .cloned();
            if let (Some(caller), Some(object)) = (caller, block.as_object_mut()) {
                object.insert("name".to_owned(), Value::String(caller));
            }
        }
    }
    match serde_json::to_vec(&value) {
        Ok(encoded) => Bytes::from(encoded),
        Err(_) => bytes.clone(),
    }
}

/// Upstream status to the contract's failure; the body decides nothing but quota.
fn failure(status: StatusCode, headers: &HeaderMap, body: &Bytes) -> Failure {
    match status.as_u16() {
        401 | 403 => Failure::Auth,
        code if (code == 400 || code == 429)
            && (quota_rejected(headers) || quota_in_body(body)) =>
        {
            Failure::Quota {
                reset_after: quota_reset(headers),
            }
        }
        429 => Failure::RateLimited {
            // Without a header the pool's minute is the same default opencodex uses.
            retry_after: retry_after(headers).or(Some(Duration::from_secs(60))),
        },
        code if (500..600).contains(&code) => Failure::Retryable,
        _ => Failure::Fatal {
            status,
            body: body.clone(),
        },
    }
}

/// Plan-exhaustion wording the upstream uses for its own long windows.
fn quota_in_body(body: &Bytes) -> bool {
    let text = String::from_utf8_lossy(body).to_lowercase();
    [
        "insufficient_quota",
        "usage limit",
        "exceeded your current quota",
        "quota exhausted",
        "account quota exceeded",
        "monthly quota exceeded",
        "daily quota exceeded",
    ]
    .iter()
    .any(|cue| text.contains(cue))
}

/// A rejected 5h or 7d window means the plan allowance, not the request rate, ran out.
fn quota_rejected(headers: &HeaderMap) -> bool {
    ["5h", "7d"].iter().any(|window| {
        header(
            headers,
            &format!("anthropic-ratelimit-unified-{window}-status"),
        )
        .is_some_and(|value| value.eq_ignore_ascii_case("rejected"))
    })
}

/// Latest reset among the rejected windows, as time from now.
fn quota_reset(headers: &HeaderMap) -> Option<Duration> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let mut latest: Option<u64> = None;
    for window in ["5h", "7d"] {
        let rejected = header(
            headers,
            &format!("anthropic-ratelimit-unified-{window}-status"),
        )
        .is_some_and(|value| value.eq_ignore_ascii_case("rejected"));
        if !rejected {
            continue;
        }
        let reset = header(
            headers,
            &format!("anthropic-ratelimit-unified-{window}-reset"),
        )
        .and_then(|value| value.parse::<u64>().ok());
        if let Some(reset) = reset {
            latest = Some(latest.map_or(reset, |current| current.max(reset)));
        }
    }
    latest.map(|reset| Duration::from_secs(reset.saturating_sub(now)))
}

/// Seconds or fractional seconds; the HTTP-date form is not parsed.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = header(headers, "retry-after")?;
    let seconds = raw.parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Some(Duration::from_millis((seconds * 1000.0).ceil() as u64))
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_owned())
}

/// Read at most `max` bytes; a read error just ends the body.
async fn read_capped(resp: &mut reqwest::Response, max: usize) -> Bytes {
    let mut out = Vec::new();
    while out.len() < max {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = max - out.len();
                out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::response::Response;
    use axum::routing::any;
    use axum::Router;
    use std::sync::{Arc, Mutex};

    fn oauth_account() -> Account {
        Account {
            id: "a1".into(),
            credential: Credential::OAuth {
                access: "test-token".into(),
                refresh: None,
                expires_at: None,
            },
            extra: Default::default(),
        }
    }

    fn key_account() -> Account {
        Account {
            id: "a1".into(),
            credential: Credential::ApiKey("sk-ant-test".into()),
            extra: Default::default(),
        }
    }

    fn request(inbound: Inbound, body: Value) -> Request {
        Request {
            inbound,
            model: "claude-sonnet-4-5".into(),
            headers: HeaderMap::new(),
            stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
            body: Bytes::from(body.to_string()),
            session: "session".into(),
        }
    }

    fn header_pairs(headers: &[(String, String)]) -> Vec<(&str, &str)> {
        headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect()
    }

    #[test]
    fn normalizes_base_urls() {
        for base in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1/",
            "https://api.anthropic.com/v1/messages",
        ] {
            assert_eq!(
                messages_url(base).unwrap(),
                "https://api.anthropic.com/v1/messages",
                "{base}"
            );
        }
        assert_eq!(
            messages_url("http://127.0.0.1:9/anthropic").unwrap(),
            "http://127.0.0.1:9/anthropic/v1/messages"
        );
        assert!(messages_url("https://{region}.example.com").is_err());
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn session_id_is_a_stable_uuid_from_the_token() {
        assert_eq!(
            session_id(Some("test-token")),
            "35ea9acf-080d-424f-9ad7-63da80aa653b"
        );
        assert_ne!(session_id(None), session_id(Some("test-token")));
        let id = client_request_id();
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
    }

    #[test]
    fn oauth_egress_matches_the_spec() {
        let body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "system": "harness\n<docs>install paths</docs>\ntail",
            "tools": [
                { "name": "read_file", "description": "d", "input_schema": { "type": "object" } },
                { "type": "web_search_20250305", "name": "web_search" }
            ],
            "tool_choice": { "type": "tool", "name": "read_file" },
            "messages": [
                { "role": "user", "content": [
                    { "type": "tool_use", "id": "t1", "name": "read_file", "input": {} }
                ] }
            ],
            "unexpected_field": true,
            "stream": false
        });
        let mut req = request(Inbound::Messages, body);
        req.headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("interleaved-thinking-2025-05-14,other-beta"),
        );
        let plan = plan("https://api.anthropic.com", &req, &oauth_account()).unwrap();

        assert_eq!(plan.url, "https://api.anthropic.com/v1/messages");
        assert!(!plan.stream);
        let expected = vec![
            ("Content-Type", "application/json"),
            ("anthropic-version", "2023-06-01"),
            ("Accept", "application/json"),
            ("User-Agent", "@anthropic-ai/sdk/0.74.0"),
            ("Authorization", "Bearer test-token"),
            ("X-App", "cli"),
            ("X-Stainless-Retry-Count", "0"),
            ("X-Stainless-Runtime", "node"),
            ("X-Stainless-Lang", "js"),
            ("X-Stainless-Timeout", "600"),
            ("X-Stainless-Arch", node_arch_expected()),
            ("X-Stainless-OS", node_platform_expected()),
            ("X-Stainless-Package-Version", "0.74.0"),
            ("X-Stainless-Runtime-Version", "22.11.0"),
            (
                "X-Claude-Code-Session-Id",
                "35ea9acf-080d-424f-9ad7-63da80aa653b",
            ),
        ];
        let pairs = header_pairs(&plan.headers);
        assert_eq!(&pairs[..expected.len()], &expected[..]);
        let (name, request_id) = &pairs[expected.len()];
        assert_eq!(*name, "x-client-request-id");
        assert_eq!(request_id.len(), 36);
        assert_eq!(
            *pairs.last().unwrap(),
            (
                "anthropic-beta",
                "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14"
            )
        );

        let expected_body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "system": [
                { "type": "text", "text": CLAUDE_CODE_IDENTITY },
                { "type": "text", "text": "harness\ntail" }
            ],
            "tools": [
                { "name": "custom_read_file", "description": "d", "input_schema": { "type": "object" } },
                { "type": "web_search_20250305", "name": "web_search" }
            ],
            "tool_choice": { "type": "tool", "name": "custom_read_file" },
            "messages": [
                { "role": "user", "content": [
                    { "type": "tool_use", "id": "t1", "name": "custom_read_file", "input": {} }
                ] }
            ],
            "stream": false
        });
        assert_eq!(
            String::from_utf8(plan.body).unwrap(),
            serde_json::to_string(&expected_body).unwrap()
        );
        assert_eq!(
            plan.renames.get("custom_read_file").map(String::as_str),
            Some("read_file")
        );
    }

    #[test]
    fn key_auth_uses_x_api_key() {
        let req = request(
            Inbound::Messages,
            json!({ "model": "m", "max_tokens": 8, "messages": [], "stream": true }),
        );
        let direct = plan("http://127.0.0.1:9", &req, &key_account()).unwrap();
        assert_eq!(direct.url, "http://127.0.0.1:9/v1/messages");
        let pairs = header_pairs(&direct.headers);
        assert_eq!(pairs[2], ("Accept", "text/event-stream"));
        assert_eq!(pairs[4], ("x-api-key", "sk-ant-test"));
        assert!(!pairs
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization")));
        assert!(!pairs
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta")));
        assert!(direct.renames.is_empty());

        let mut bearer = key_account();
        bearer
            .extra
            .insert("api_key_transport".into(), "bearer".into());
        let by_bearer = plan("http://127.0.0.1:9", &req, &bearer).unwrap();
        assert_eq!(
            header_pairs(&by_bearer.headers)[4],
            ("Authorization", "Bearer sk-ant-test")
        );
    }

    #[test]
    fn caller_betas_survive_only_for_the_first_party_allowlist() {
        let mut req = request(
            Inbound::Messages,
            json!({ "model": "m", "max_tokens": 8, "messages": [] }),
        );
        req.headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("interleaved-thinking-2025-05-14"),
        );
        let first = plan("https://api.anthropic.com", &req, &key_account()).unwrap();
        assert_eq!(
            header_pairs(&first.headers).last().copied(),
            Some(("anthropic-beta", "interleaved-thinking-2025-05-14"))
        );
        let local = plan("http://127.0.0.1:9", &req, &key_account()).unwrap();
        assert!(!local
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta")));
    }

    #[test]
    fn opaque_state_only_reaches_the_first_party_host() {
        let body = json!({
            "model": "m",
            "max_tokens": 8,
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "step", "signature": "sig" },
                    { "type": "redacted_thinking", "data": "opaque" }
                ]},
                { "role": "user", "content": "hi" }
            ]
        });
        let req = request(Inbound::Messages, body);
        let local = plan("http://127.0.0.1:9", &req, &key_account()).unwrap();
        let wire: Value = serde_json::from_slice(&local.body).unwrap();
        assert_eq!(
            wire.pointer("/messages/0/content").unwrap(),
            &json!([{ "type": "thinking", "thinking": "step" }])
        );
        let remote = plan("https://api.anthropic.com", &req, &key_account()).unwrap();
        let wire: Value = serde_json::from_slice(&remote.body).unwrap();
        assert_eq!(wire["messages"][0]["content"][0]["signature"], "sig");
        assert_eq!(
            wire["messages"][0]["content"][1]["type"],
            "redacted_thinking"
        );
    }

    #[test]
    fn other_inbound_protocols_are_refused_before_io() {
        let req = request(
            Inbound::Chat,
            json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] }),
        );
        let failure = plan("https://api.anthropic.com", &req, &key_account()).unwrap_err();
        match failure {
            Failure::Fatal { status, body } => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"]["type"], "invalid_request_error");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn oauth_is_refused_for_a_remote_third_party_host() {
        let req = request(
            Inbound::Messages,
            json!({ "model": "m", "max_tokens": 8, "messages": [] }),
        );
        let failure = plan("https://api.example.com", &req, &oauth_account()).unwrap_err();
        assert!(matches!(failure, Failure::Fatal { .. }));
    }

    #[derive(Clone)]
    struct Seen {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    /// Status, headers and body the fake upstream answers with.
    struct Fake {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    type Upstream = Arc<(Arc<Mutex<Option<Seen>>>, Fake)>;

    async fn capture(State(state): State<Upstream>, req: axum::extract::Request) -> Response {
        let (parts, body) = req.into_parts();
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
        *state.0.lock().unwrap() = Some(Seen {
            method: parts.method.to_string(),
            path: parts.uri.path().to_owned(),
            headers,
            body: bytes.to_vec(),
        });
        let mut response = Response::builder().status(state.1.status);
        for (name, value) in &state.1.headers {
            response = response.header(name, value);
        }
        response
            .body(axum::body::Body::from(state.1.body.clone()))
            .unwrap()
    }

    async fn spawn(fake: Fake) -> (String, Arc<Mutex<Option<Seen>>>) {
        let seen = Arc::new(Mutex::new(None));
        let state: Upstream = Arc::new((seen.clone(), fake));
        let app = Router::new().fallback(any(capture)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), seen)
    }

    /// Upstream that answers `chunks` as separate writes, `delay` apart.
    async fn spawn_chunked(
        chunks: Vec<Vec<u8>>,
        delay: Duration,
    ) -> (String, Arc<Mutex<Option<Seen>>>) {
        let seen = Arc::new(Mutex::new(None));
        #[derive(Clone)]
        struct Stateful {
            seen: Arc<Mutex<Option<Seen>>>,
            chunks: Vec<Vec<u8>>,
            delay: Duration,
        }
        async fn capture_chunked(
            State(state): State<Arc<Stateful>>,
            req: axum::extract::Request,
        ) -> Response {
            let (parts, body) = req.into_parts();
            let bytes = axum::body::to_bytes(body, 1 << 20)
                .await
                .unwrap_or_default();
            *state.seen.lock().unwrap() = Some(Seen {
                method: parts.method.to_string(),
                path: parts.uri.path().to_owned(),
                headers: Vec::new(),
                body: bytes.to_vec(),
            });
            let chunks = state.chunks.clone();
            let delay = state.delay;
            let body = axum::body::Body::from_stream(futures_util::stream::unfold(
                (chunks.into_iter(), 0usize),
                move |(mut chunks, index)| async move {
                    let chunk = chunks.next()?;
                    if index > 0 {
                        tokio::time::sleep(delay).await;
                    }
                    Some((
                        Ok::<_, std::io::Error>(bytes::Bytes::from(chunk)),
                        (chunks, index + 1),
                    ))
                },
            ));
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(body)
                .unwrap()
        }
        let state = Arc::new(Stateful {
            seen: seen.clone(),
            chunks,
            delay,
        });
        let app = Router::new()
            .fallback(any(capture_chunked))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), seen)
    }

    async fn failure_of(adapter: &Anthropic, req: &Request, account: &Account) -> Failure {
        match adapter.send(req, account).await {
            Ok(_) => panic!("expected a failure"),
            Err(failure) => failure,
        }
    }

    fn seen_header(seen: &Seen, name: &str) -> Option<String> {
        seen.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    async fn body_of(mut reply: Reply) -> Bytes {
        let mut out = Vec::new();
        while let Some(chunk) = reply.body.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        Bytes::from(out)
    }

    fn messages_request(body: Value) -> Request {
        request(Inbound::Messages, body)
    }

    #[tokio::test]
    async fn non_stream_passthrough_hits_the_messages_endpoint() {
        let answer = json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5", "content": [{ "type": "text", "text": "hi" }],
            "stop_reason": "end_turn", "usage": { "input_tokens": 3, "output_tokens": 1 }
        });
        let (base, seen) = spawn(Fake {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: answer.to_string().into_bytes(),
        })
        .await;
        let adapter = Anthropic::new(reqwest::Client::new(), Some(format!("{base}/v1")));
        let mut req = messages_request(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 16,
            "messages": [{ "role": "user", "content": "hi" }],
            "dropped": true
        }));
        req.headers
            .insert("x-api-key", HeaderValue::from_static("caller-key"));
        let reply = adapter
            .send(&req, &key_account())
            .await
            .unwrap_or_else(|failure| panic!("unexpected {failure:?}"));
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(body_of(reply).await, Bytes::from(answer.to_string()));

        let seen = seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.path, "/v1/messages");
        assert_eq!(
            seen_header(&seen, "content-type").as_deref(),
            Some("application/json")
        );
        assert_eq!(
            seen_header(&seen, "anthropic-version").as_deref(),
            Some("2023-06-01")
        );
        assert_eq!(
            seen_header(&seen, "accept").as_deref(),
            Some("application/json")
        );
        assert_eq!(
            seen_header(&seen, "user-agent").as_deref(),
            Some("@anthropic-ai/sdk/0.74.0")
        );
        assert_eq!(
            seen_header(&seen, "x-api-key").as_deref(),
            Some("sk-ant-test")
        );
        assert!(!seen.body.windows(7).any(|window| window == b"dropped"));
        let wire: Value = serde_json::from_slice(&seen.body).unwrap();
        assert_eq!(wire["model"], "claude-sonnet-4-5");
        assert_eq!(wire["max_tokens"], 16);
        assert!(wire.get("dropped").is_none());
    }

    #[tokio::test]
    async fn streaming_passes_chunks_through_without_buffering() {
        let first = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n".to_vec();
        let second = b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec();
        let (base, _seen) =
            spawn_chunked(vec![first.clone(), second.clone()], Duration::from_secs(5)).await;
        let adapter = Anthropic::new(reqwest::Client::new(), Some(base));
        let req = messages_request(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 16,
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }]
        }));
        let mut reply = adapter
            .send(&req, &key_account())
            .await
            .unwrap_or_else(|failure| panic!("unexpected {failure:?}"));
        assert_eq!(
            reply.headers.get(http::header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        let head = tokio::time::timeout(Duration::from_secs(2), reply.body.next())
            .await
            .expect("first chunk before the upstream finishes")
            .unwrap()
            .unwrap();
        assert_eq!(head, Bytes::from(first));
        let rest = body_of(reply).await;
        assert_eq!(rest, Bytes::from(second));
    }

    #[tokio::test]
    async fn oauth_stream_maps_tool_names_back() {
        let sse = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"custom_read_file\",\"input\":{}}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n"
        );
        let (base, seen) = spawn(Fake {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: sse.as_bytes().to_vec(),
        })
        .await;
        let adapter = Anthropic::new(reqwest::Client::new(), Some(base));
        let req = messages_request(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 16,
            "stream": true,
            "tools": [{ "name": "read_file", "input_schema": { "type": "object" } }],
            "messages": [{ "role": "user", "content": "hi" }]
        }));
        let reply = adapter
            .send(&req, &oauth_account())
            .await
            .unwrap_or_else(|failure| panic!("unexpected {failure:?}"));
        let body = body_of(reply).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("\"name\":\"read_file\""), "{text}");
        assert!(!text.contains("custom_read_file"), "{text}");
        assert!(text.contains(
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n"
        ));
        let seen = seen.lock().unwrap().clone().unwrap();
        let wire: Value = serde_json::from_slice(&seen.body).unwrap();
        assert_eq!(wire["tools"][0]["name"], "custom_read_file");
        assert_eq!(wire["system"][0]["text"], CLAUDE_CODE_IDENTITY);
    }

    #[tokio::test]
    async fn non_stream_maps_tool_names_back() {
        let answer = json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{ "type": "tool_use", "id": "toolu_1", "name": "custom_read_file", "input": {} }],
            "stop_reason": "tool_use", "usage": { "input_tokens": 1, "output_tokens": 1 }
        });
        let (base, _seen) = spawn(Fake {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: answer.to_string().into_bytes(),
        })
        .await;
        let adapter = Anthropic::new(reqwest::Client::new(), Some(base));
        let req = messages_request(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 16,
            "tools": [{ "name": "read_file", "input_schema": { "type": "object" } }],
            "messages": [{ "role": "user", "content": "hi" }]
        }));
        let reply = adapter
            .send(&req, &oauth_account())
            .await
            .unwrap_or_else(|failure| panic!("unexpected {failure:?}"));
        let body: Value = serde_json::from_slice(&body_of(reply).await).unwrap();
        assert_eq!(body["content"][0]["name"], "read_file");
    }

    #[tokio::test]
    async fn upstream_errors_map_to_failures() {
        for status in [500, 400] {
            let (base, _seen) = spawn(Fake {
                status,
                headers: Vec::new(),
                body:
                    br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#
                        .to_vec(),
            })
            .await;
            let adapter = Anthropic::new(reqwest::Client::new(), Some(base));
            let failure = failure_of(
                &adapter,
                &messages_request(json!({ "model": "m", "max_tokens": 1, "messages": [] })),
                &key_account(),
            )
            .await;
            match (status, failure) {
                (500, Failure::Retryable) => {}
                (400, Failure::Fatal { status, body }) => {
                    assert_eq!(status, StatusCode::BAD_REQUEST);
                    assert!(String::from_utf8_lossy(&body).contains("invalid_request_error"));
                }
                (_, other) => panic!("unexpected {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn rate_limits_carry_a_cooldown() {
        let (base, _seen) = spawn(Fake {
            status: 429,
            headers: vec![("retry-after".into(), "7".into())],
            body: b"{}".to_vec(),
        })
        .await;
        let adapter = Anthropic::new(reqwest::Client::new(), Some(base));
        let failure = failure_of(
            &adapter,
            &messages_request(json!({ "model": "m", "max_tokens": 1, "messages": [] })),
            &key_account(),
        )
        .await;
        assert_eq!(
            failure,
            Failure::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );

        let reset = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 120;
        let (base, _seen) = spawn(Fake {
            status: 429,
            headers: vec![
                (
                    "anthropic-ratelimit-unified-5h-status".into(),
                    "rejected".into(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset".into(),
                    reset.to_string(),
                ),
            ],
            body: b"{}".to_vec(),
        })
        .await;
        let adapter = Anthropic::new(reqwest::Client::new(), Some(base));
        let failure = failure_of(
            &adapter,
            &messages_request(json!({ "model": "m", "max_tokens": 1, "messages": [] })),
            &key_account(),
        )
        .await;
        match failure {
            Failure::Quota {
                reset_after: Some(reset_after),
            } => assert!(reset_after.as_secs() >= 110 && reset_after.as_secs() <= 120),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn quota_wording_in_the_body_is_a_quota_failure() {
        for status in [429, 400] {
            let (base, _seen) = spawn(Fake {
                status,
                headers: Vec::new(),
                body: br#"{"type":"error","error":{"type":"invalid_request_error","message":"You have reached your usage limit"}}"#.to_vec(),
            })
            .await;
            let adapter = Anthropic::new(reqwest::Client::new(), Some(base));
            let failure = failure_of(
                &adapter,
                &messages_request(json!({ "model": "m", "max_tokens": 1, "messages": [] })),
                &key_account(),
            )
            .await;
            assert_eq!(failure, Failure::Quota { reset_after: None }, "{status}");
        }
    }

    #[tokio::test]
    async fn rejected_credentials_are_auth_failures() {
        for status in [401, 403] {
            let (base, _seen) = spawn(Fake {
                status,
                headers: Vec::new(),
                body: b"{}".to_vec(),
            })
            .await;
            let adapter = Anthropic::new(reqwest::Client::new(), Some(base));
            let failure = failure_of(
                &adapter,
                &messages_request(json!({ "model": "m", "max_tokens": 1, "messages": [] })),
                &key_account(),
            )
            .await;
            assert_eq!(failure, Failure::Auth, "{status}");
        }
    }

    #[tokio::test]
    async fn unreachable_upstream_is_retryable() {
        let adapter = Anthropic::new(reqwest::Client::new(), Some("http://127.0.0.1:1".into()));
        let failure = failure_of(
            &adapter,
            &messages_request(json!({ "model": "m", "max_tokens": 1, "messages": [] })),
            &key_account(),
        )
        .await;
        assert_eq!(failure, Failure::Retryable);
    }

    fn node_arch_expected() -> &'static str {
        match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            "x86" => "ia32",
            "powerpc64" => "ppc64",
            "loongarch64" => "loong64",
            other => other,
        }
    }

    fn node_platform_expected() -> &'static str {
        match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        }
    }
}
