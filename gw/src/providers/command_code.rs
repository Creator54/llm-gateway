//! command-code upstream adapter (`POST /alpha/generate`, NDJSON in, client protocol out).

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures_util::{future::BoxFuture, StreamExt};
use http::{header, HeaderMap, HeaderValue, StatusCode};
use serde_json::{json, Value};

use crate::{
    provider::{Account, Credential, Failure, Inbound, Provider, Reply, Request},
    translate::{
        chat_to_messages, chat_to_responses, messages_to_chat, responses_to_chat, MessagesStream,
        ResponsesStream,
    },
};

const DEFAULT_BASE_URL: &str = "https://api.commandcode.ai";
const DEFAULT_VERSION: &str = "0.52.1";
const DEFAULT_MAX_TOKENS: u64 = 64_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(200);
const GIT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_STRUCTURE: usize = 64;
const MAX_COMMITS: usize = 8;
const MAX_COMMIT_LEN: usize = 512;
const MAX_GIT_STATUS: usize = 2048;
const WORKSPACE_TTL: Duration = Duration::from_secs(30);
const MAX_WORKSPACES: usize = 128;
const MAX_NON_STREAM_BODY: usize = 32 * 1024 * 1024;

/// `command-code` upstream.
pub struct CommandCode {
    client: reqwest::Client,
    base_url: String,
}

impl CommandCode {
    pub fn new(client: reqwest::Client, base_url: Option<String>) -> Self {
        Self {
            client,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_owned()),
        }
    }
}

impl Provider for CommandCode {
    fn id(&self) -> &str {
        "command-code"
    }

    fn send<'a>(
        &'a self,
        req: &'a Request,
        account: &'a Account,
    ) -> BoxFuture<'a, Result<Reply, Failure>> {
        Box::pin(self.forward(req, account))
    }
}

impl CommandCode {
    async fn forward(&self, req: &Request, account: &Account) -> Result<Reply, Failure> {
        let token = bearer(account)?;
        let body = self.body(req, account).await?;
        let url = format!(
            "{}/alpha/generate",
            self.base_url.strip_suffix('/').unwrap_or(&self.base_url)
        );
        let workspace = workspace_name();
        let slug = workspace.as_deref().and_then(project_slug);
        let mut request = self
            .client
            .post(url)
            .timeout(REQUEST_TIMEOUT)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("user-agent", "cli")
            .header(
                "x-command-code-version",
                account
                    .extra
                    .get("command_code_version")
                    .map(String::as_str)
                    .unwrap_or(DEFAULT_VERSION),
            )
            .header("x-cli-environment", "production")
            .header("x-taste-learning", "false")
            .header("x-co-flag", "false")
            .header("x-session-id", session_id(&req.session));
        if let Some(slug) = slug {
            request = request.header("x-project-slug", slug);
        }
        let response = request
            .body(body)
            .send()
            .await
            .map_err(|_| Failure::Retryable)?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after(response.headers());
            let body = response.bytes().await.unwrap_or_default();
            // Upstream refusals are otherwise invisible; log enough to diagnose.
            let preview = String::from_utf8_lossy(&body);
            tracing::warn!(
                status = %status,
                body = %preview.chars().take(200).collect::<String>(),
                "command-code refused the request"
            );
            return Err(classify(status, retry_after, &body));
        }
        if req.stream {
            Ok(stream_reply(req, response))
        } else {
            buffered_reply(req, response).await
        }
    }
}

fn bearer(account: &Account) -> Result<&str, Failure> {
    match &account.credential {
        Credential::ApiKey(key) => Ok(key),
        Credential::OAuth { access, .. } => Ok(access),
        Credential::None => Err(Failure::Auth),
    }
}

fn classify(status: StatusCode, retry_after: Option<Duration>, body: &Bytes) -> Failure {
    // Cloudflare answers 403 with its own error page when it dislikes the client
    // (error code 1010 and friends). That is a transport block, not a bad token,
    // so treat it as retryable instead of cooling the account for minutes.
    if status == StatusCode::FORBIDDEN && cloudflare_block(body) {
        return Failure::Retryable;
    }
    // A 403 that names an unknown model is a request problem: retrying or cooling
    // the account would only hide the mistake and take the account out of service.
    if status == StatusCode::FORBIDDEN && unknown_model(body) {
        return Failure::Fatal {
            status,
            body: body.clone(),
        };
    }
    match status.as_u16() {
        401 | 403 => Failure::Auth,
        429 if quota_exhausted(body) => Failure::Quota {
            reset_after: retry_after,
        },
        429 => Failure::RateLimited { retry_after },
        402 => Failure::Quota {
            reset_after: retry_after,
        },
        500..=599 => Failure::Retryable,
        _ => Failure::Fatal {
            status,
            body: body.clone(),
        },
    }
}

fn quota_exhausted(body: &Bytes) -> bool {
    let text = String::from_utf8_lossy(body).to_lowercase();
    ["quota", "credit", "allowance", "usage limit"]
        .iter()
        .any(|needle| text.contains(needle))
}

/// The API says the model or provider id is not one it serves.
fn unknown_model(body: &Bytes) -> bool {
    let text = String::from_utf8_lossy(body).to_lowercase();
    [
        "model/provider not recognized",
        "model not recognized",
        "unknown model",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Whether a 403 came from Cloudflare rather than from the API itself.
///
/// The edge returns a short `error code: NNNN` page for bot and TLS-fingerprint
/// rules; the API returns a JSON body with its own error shape.
fn cloudflare_block(body: &Bytes) -> bool {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim_start();
    if trimmed.starts_with("{") {
        return false;
    }
    let lower = text.to_lowercase();
    [
        "error code:",
        "cloudflare",
        "attention required",
        "just a moment",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// `Retry-After` in seconds, else a millisecond/seconds field in the error body.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    if let Some(value) = headers.get(header::RETRY_AFTER) {
        if let Some(seconds) = value
            .to_str()
            .ok()
            .and_then(|text| text.trim().parse::<f64>().ok())
        {
            return Some(Duration::from_secs_f64(seconds).min(Duration::from_secs(86_400)));
        }
    }
    None
}

/// Deterministic UUID-shaped session affinity id derived from the gateway session.
fn session_id(session: &str) -> String {
    let identity = if session.is_empty() {
        format!("anonymous\0{}", unique_suffix())
    } else {
        format!("thread\0{session}")
    };
    let hex = sha256(format!("command-code:{identity}").as_bytes());
    let hex: String = hex
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(
        "{}-{}-4{}-8{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

fn unique_suffix() -> u128 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    nanos ^ u128::from(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

fn project_slug(cwd: &str) -> Option<String> {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in cwd.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    let truncated: String = slug.chars().take(64).collect();
    Some(if truncated.is_empty() {
        "workspace".to_owned()
    } else {
        truncated
    })
}

/// SHA-256, only for the session-affinity digest.
fn sha256(input: &[u8]) -> [u8; 32] {
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
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = input.to_vec();
    let bits = (input.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());

    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
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
        let mut h = state;
        for i in 0..64 {
            let s1 = h[4].rotate_right(6) ^ h[4].rotate_right(11) ^ h[4].rotate_right(25);
            let ch = (h[4] & h[5]) ^ (!h[4] & h[6]);
            let t1 = h[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = h[0].rotate_right(2) ^ h[0].rotate_right(13) ^ h[0].rotate_right(22);
            let maj = (h[0] & h[1]) ^ (h[0] & h[2]) ^ (h[1] & h[2]);
            let t2 = s0.wrapping_add(maj);
            h[7] = h[6];
            h[6] = h[5];
            h[5] = h[4];
            h[4] = h[3].wrapping_add(t1);
            h[3] = h[2];
            h[2] = h[1];
            h[1] = h[0];
            h[0] = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip(h) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// No tool result was recorded for a declared call; the upstream rejects unpaired calls.
const NO_RESULT: &str =
    "[ocx] no tool result was recorded for this tool call; execution status unknown.";

#[derive(Clone, Default)]
struct GitInfo {
    is_git: bool,
    current_branch: String,
    main_branch: String,
    git_status: String,
    recent_commits: Vec<String>,
}

fn workspaces() -> &'static Mutex<HashMap<String, (Instant, GitInfo)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, GitInfo)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Absolute process working directory, like `process.cwd()`.
fn workspace_name() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|path| path.display().to_string())
}

impl CommandCode {
    async fn body(&self, req: &Request, account: &Account) -> Result<Bytes, Failure> {
        let inbound = req.inbound;
        let value: Value = serde_json::from_slice(&req.body)
            .map_err(|_| invalid("request body is not valid JSON", inbound))?;
        let chat = match inbound {
            Inbound::Chat => value,
            Inbound::Responses => responses_to_chat(&value, &req.model)
                .map_err(|error| translate_failure(&error, inbound))?,
            Inbound::Messages => messages_to_chat(&value, &req.model)
                .map_err(|error| translate_failure(&error, inbound))?,
        };
        let empty = Vec::new();
        let messages = chat
            .get("messages")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        let cwd = workspace_name();
        let git = git_info(cwd.as_deref()).await;
        let canonical = canonical_model(&req.model);

        let mut config = serde_json::Map::new();
        if let Some(cwd) = &cwd {
            config.insert("workingDir".into(), json!(cwd));
        }
        config.insert("date".into(), json!(utc_date()));
        config.insert("environment".into(), json!(node_platform()));
        config.insert("structure".into(), json!(structure(cwd.as_deref())));
        config.insert("isGitRepo".into(), json!(git.is_git));
        config.insert("currentBranch".into(), json!(git.current_branch));
        config.insert("mainBranch".into(), json!(git.main_branch));
        config.insert("gitStatus".into(), json!(git.git_status));
        config.insert("recentCommits".into(), json!(git.recent_commits));

        let mut params = serde_json::Map::new();
        params.insert("model".into(), json!(canonical));
        params.insert("messages".into(), Value::Array(wire_messages(messages)));
        params.insert("tools".into(), Value::Array(wire_tools(&chat)?));
        params.insert("system".into(), json!(system_prompt(messages)));
        params.insert("max_tokens".into(), json!(max_tokens(&chat, account)));
        params.insert("stream".into(), json!(true));
        if let Some(temperature) = chat.get("temperature") {
            params.insert("temperature".into(), temperature.clone());
        }
        if let Some(effort) = chat
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .and_then(|effort| supported_effort(&canonical, effort))
        {
            params.insert("reasoning_effort".into(), json!(effort));
        }

        Ok(Bytes::from(
            serde_json::to_vec(&json!({
                "config": Value::Object(config),
                "memory": "",
                "taste": null,
                "skills": null,
                "permissionMode": "standard",
                "mode": "agent",
                "params": Value::Object(params),
            }))
            .unwrap_or_default(),
        ))
    }
}

fn invalid(message: &str, inbound: Inbound) -> Failure {
    let error = crate::translate::Error::bad_request(message);
    translate_failure(&error, inbound)
}

fn translate_failure(error: &crate::translate::Error, inbound: Inbound) -> Failure {
    let wire = match inbound {
        Inbound::Chat => crate::translate::Inbound::Chat,
        Inbound::Responses => crate::translate::Inbound::Responses,
        Inbound::Messages => crate::translate::Inbound::Messages,
    };
    Failure::Fatal {
        status: StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST),
        body: Bytes::from(serde_json::to_vec(&error.to_body(wire)).unwrap_or_default()),
    }
}

fn canonical_model(model: &str) -> String {
    match model {
        "deepseek-v4-flash" => "deepseek/deepseek-v4-flash".into(),
        "deepseek-v4.1-flash" => "deepseek/deepseek-v4.1-flash".into(),
        "deepseek-v4.1" => "deepseek/deepseek-v4.1".into(),
        "kimi-k3" => "moonshotai/Kimi-K3".into(),
        "glm-5.3" => "zai-org/GLM-5.3".into(),
        "glm-5.2" => "zai-org/GLM-5.2".into(),
        // A bare DeepSeek id reaches the API as `anthropic:<id>` and is refused.
        other if other.starts_with("deepseek") && !other.contains('/') => {
            format!("deepseek/{other}")
        }
        other => other.to_owned(),
    }
}

/// Shipped reasoning ladder for the canonical model id, case-insensitively.
fn effort_ladder(model: &str) -> Option<&'static [&'static str]> {
    const FIVE: &[&str] = &["low", "medium", "high", "xhigh", "max"];
    let model = model.to_lowercase();
    let ladder = match model.as_str() {
        "claude-fable-5-1"
        | "claude-opus-5-5"
        | "deepseek/deepseek-v4-flash"
        | "deepseek/deepseek-v4-flash-vision-exp"
        | "deepseek/deepseek-v4.1-flash"
        | "gpt-5.6-luna"
        | "google/gemini-3.7-flash"
        | "zai-org/glm-5.3"
        | "z-ai/glm-5.3-flash"
        | "meta/muse-spark-1.3"
        | "meta/muse-spark-1.3-contributor"
        | "meta/muse-spark-1.2"
        | "meta/muse-spark-1.2-contributor"
        | "meta/muse-spark-1.1"
        | "qwen/qwen3.8-flash"
        | "tencent/hy4-preview"
        | "moonshotai/kimi-k3"
        | "minimaxai/minimax-m3"
        | "xiaomi/mimo-v2.5"
        | "xai/grok-4.5"
        | "xai/grok-4.6"
        | "tencent/hy3-paid"
        | "stepfun/step-3.7-flash"
        | "qwen/qwen3.8-max"
        | "qwen/qwen3.8-27b"
        | "nvidia/nemotron-3-ultra-550b-a55b"
        | "meituan/longcat-2.0:free"
        | "inclusionai/ling-3.0-flash-sante:free"
        | "thinkingmachines/inkling-small" => FIVE,
        "deepseek/deepseek-v4-flash-fast" => &["low", "high", "max"],
        "google/gemini-3.8-flash" => &["low", "medium", "high"],
        "zai-org/glm-5" | "zai-org/glm-5.1" | "zai-org/glm-5.2" | "zai-org/glm-5.2-fast" => {
            &["high", "max"]
        }
        "z-ai/glm-5.3-flashx" => &["low", "high", "max"],
        "qwen/qwen3.8-omni-flash" | "qwen/qwen3.8-max-0902" => &["low", "medium", "xhigh"],
        "stepfun/step-5-preview" | "xiaomi/mimo-v2.5-pro" => &["low", "medium", "high"],
        "xai/grok-4.7"
        | "moonshotai/kimi-k2.7-code"
        | "qwen/qwen3.7-32b"
        | "qwen/qwen3.7-72b"
        | "qwen/qwen3.6-35b-a22b" => &["low", "medium", "high", "xhigh"],
        "moonshotai/kimi-k2.7-code-highspeed" => &["low", "high", "xhigh", "max"],
        "poolside/laguna-s-2.1-free" => &["medium"],
        _ => return None,
    };
    Some(ladder)
}

fn supported_effort(model: &str, requested: &str) -> Option<&'static str> {
    if requested.is_empty() || requested == "none" {
        return None;
    }
    let ladder = effort_ladder(model)?;
    let lower = model.to_lowercase();
    let aliases_max = lower == "deepseek/deepseek-v4-flash" || lower == "zai-org/glm-5.2";
    let lift_to_max = !ladder.contains(&requested)
        && ladder.contains(&"max")
        && (requested == "xhigh" || (requested == "ultra" && aliases_max));
    let wire = if lift_to_max { "max" } else { requested };
    ladder.iter().copied().find(|effort| *effort == wire)
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn utc_date() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since the Unix epoch to a Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Non-hidden directory entries, in directory order, capped like opencodex.
fn structure(cwd: Option<&str>) -> Vec<String> {
    let Some(cwd) = cwd else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(cwd) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .take(MAX_STRUCTURE)
        .collect()
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

async fn git(cwd: &str, args: &[&str]) -> String {
    let output = tokio::time::timeout(
        GIT_TIMEOUT,
        tokio::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output(),
    )
    .await;
    match output {
        Ok(Ok(output)) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
        _ => String::new(),
    }
}

async fn git_info(cwd: Option<&str>) -> GitInfo {
    let Some(cwd) = cwd else {
        return GitInfo::default();
    };
    if let Some((at, info)) = workspaces().lock().unwrap().get(cwd) {
        if at.elapsed() < WORKSPACE_TTL {
            return info.clone();
        }
    }
    let info = if git(cwd, &["rev-parse", "--show-toplevel"]).await.is_empty() {
        GitInfo::default()
    } else {
        let current = match git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
            branch if branch.is_empty() => "HEAD".to_owned(),
            branch => branch,
        };
        let origin = git(
            cwd,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        )
        .await;
        let main = match origin.strip_prefix("origin/") {
            Some(name) if !name.is_empty() => name.to_owned(),
            _ => current.clone(),
        };
        let status = truncate(&git(cwd, &["status", "--porcelain"]).await, MAX_GIT_STATUS);
        let commits = git(cwd, &["log", "--oneline", &format!("-{MAX_COMMITS}")])
            .await
            .split('\n')
            .filter(|line| !line.is_empty())
            .take(MAX_COMMITS)
            .map(|line| truncate(line, MAX_COMMIT_LEN))
            .collect();
        GitInfo {
            is_git: true,
            current_branch: current,
            main_branch: main,
            git_status: status,
            recent_commits: commits,
        }
    };
    let mut cache = workspaces().lock().unwrap();
    cache.retain(|_, (at, _)| at.elapsed() < WORKSPACE_TTL);
    if cache.len() >= MAX_WORKSPACES {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(cwd.to_owned(), (Instant::now(), info.clone()));
    info
}

fn system_prompt(messages: &[Value]) -> String {
    messages
        .iter()
        .filter(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        })
        .map(|message| content_text(message.get("content")))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn max_tokens(chat: &Value, account: &Account) -> u64 {
    chat.get("max_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            account
                .extra
                .get("default_max_output_tokens")
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(DEFAULT_MAX_TOKENS)
}

/// Text of a Chat content value: a string, or its text parts joined.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn wire_tools(chat: &Value) -> Result<Vec<Value>, Failure> {
    let choice = chat.get("tool_choice");
    if choice.and_then(Value::as_str) == Some("none") {
        return Ok(Vec::new());
    }
    let forced = choice
        .filter(|choice| choice.is_object())
        .and_then(|choice| choice.pointer("/function/name"))
        .and_then(Value::as_str);
    let mut tools = Vec::new();
    for tool in chat
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let function = tool.get("function").unwrap_or(tool);
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                let error = crate::translate::Error::bad_request("tool is missing 'name'");
                translate_failure(&error, Inbound::Chat)
            })?;
        if forced.is_some_and(|forced| forced != name) {
            continue;
        }
        tools.push(json!({
            "name": name,
            "description": function.get("description").cloned().unwrap_or(Value::Null),
            "input_schema": function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" })),
        }));
    }
    Ok(tools)
}

/// Chat history as the upstream ModelMessage array, keeping every call paired with a result.
fn wire_messages(messages: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut carriers: Vec<Value> = Vec::new();

    for message in messages {
        match message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
        {
            "assistant" => {
                close_pending(&mut out, &mut pending, &mut carriers);
                let mut content: Vec<Value> = Vec::new();
                let thinking = message
                    .get("reasoning_content")
                    .or_else(|| message.get("reasoning"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty());
                if let Some(thinking) = thinking {
                    content.push(json!({ "type": "reasoning", "text": thinking }));
                }
                for text in content_parts(message.get("content"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                {
                    if !text.is_empty() {
                        content.push(json!({ "type": "text", "text": text }));
                    }
                }
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned();
                    let input = tool_input(call.pointer("/function/arguments"));
                    content.push(json!({
                        "type": "tool-call", "toolCallId": id, "toolName": name, "input": input,
                    }));
                    pending.push((id, name));
                }
                out.push(json!({ "role": "assistant", "content": content }));
            }
            "tool" => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let text = tool_result_text(message.get("content"));
                let images: Vec<Value> = content_parts(message.get("content"))
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("image_url"))
                    .filter_map(image_url)
                    .map(wire_image_part)
                    .collect();
                match pending.iter().position(|(call, _)| *call == id) {
                    Some(index) => {
                        let (_, name) = pending.remove(index);
                        out.push(json!({ "role": "tool", "content": [{
                            "type": "tool-result", "toolCallId": id, "toolName": name,
                            "output": { "type": "text", "value": text },
                        }]}));
                        if !images.is_empty() {
                            carriers.push(json!({ "role": "user", "content": images }));
                        }
                    }
                    None => {
                        close_pending(&mut out, &mut pending, &mut carriers);
                        let label = id;
                        let mut content = vec![json!({
                            "type": "text",
                            "text": format!("[tool result without adjacent tool call: {label}]\n{text}"),
                        })];
                        content.extend(images);
                        out.push(json!({ "role": "user", "content": content }));
                    }
                }
            }
            // System text already travels in `params.system`.
            "system" | "developer" => {}
            _ => {
                close_pending(&mut out, &mut pending, &mut carriers);
                let mut content: Vec<Value> = Vec::new();
                match message.get("content") {
                    Some(Value::String(text)) => {
                        content.push(json!({ "type": "text", "text": text }))
                    }
                    Some(Value::Array(parts)) => {
                        for part in parts {
                            match part.get("type").and_then(Value::as_str) {
                                Some("text") => content.push(json!({
                                    "type": "text",
                                    "text": part.get("text").and_then(Value::as_str).unwrap_or_default(),
                                })),
                                Some("image_url") => {
                                    if let Some(url) = image_url(part) {
                                        content.push(wire_image_part(url));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                out.push(json!({ "role": "user", "content": content }));
            }
        }
    }
    close_pending(&mut out, &mut pending, &mut carriers);
    out
}

fn close_pending(
    out: &mut Vec<Value>,
    pending: &mut Vec<(String, String)>,
    carriers: &mut Vec<Value>,
) {
    for (id, name) in pending.drain(..) {
        out.push(json!({ "role": "tool", "content": [{
            "type": "tool-result", "toolCallId": id, "toolName": name,
            "output": { "type": "error-text", "value": NO_RESULT },
        }]}));
    }
    out.append(carriers);
}

fn content_parts(content: Option<&Value>) -> impl Iterator<Item = &Value> {
    content.and_then(Value::as_array).into_iter().flatten()
}

fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                Some("image_url") => "[image]".to_owned(),
                _ => String::new(),
            })
            .collect(),
        _ => String::new(),
    }
}

fn tool_input(arguments: Option<&Value>) -> Value {
    let parsed = match arguments {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
        Some(other) => Some(other.clone()),
        None => None,
    };
    match parsed {
        Some(Value::Object(object)) => Value::Object(object),
        Some(Value::String(text)) => Value::String(text),
        _ => json!({}),
    }
}

fn image_url(part: &Value) -> Option<&str> {
    match part.get("image_url") {
        Some(Value::String(url)) => Some(url),
        Some(value) => value.get("url").and_then(Value::as_str),
        None => part.get("url").and_then(Value::as_str),
    }
}

fn wire_image_part(url: &str) -> Value {
    let mut part = serde_json::Map::new();
    part.insert("type".into(), json!("image"));
    part.insert("image".into(), json!(url));
    if let Some(media) = media_type(url) {
        part.insert("mediaType".into(), json!(media));
    }
    Value::Object(part)
}

fn media_type(url: &str) -> Option<String> {
    if let Some(rest) = url.strip_prefix("data:") {
        return rest
            .split(';')
            .next()
            .filter(|media| !media.is_empty())
            .map(str::to_owned);
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let extension = path.rsplit('.').next()?.to_lowercase();
    let media = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    };
    Some(media.to_owned())
}

enum Event {
    Text(String),
    Thinking(String),
    Tool {
        id: String,
        name: String,
        arguments: String,
    },
    Done {
        usage: Option<Value>,
        stop_reason: Option<String>,
    },
    Error(String),
}

fn event(value: &Value) -> Option<Event> {
    let kind = value.get("type")?.as_str()?;
    match kind {
        "text-delta" => value
            .get("text")
            .and_then(Value::as_str)
            .map(|text| Event::Text(text.to_owned())),
        "reasoning-delta" => value
            .get("text")
            .and_then(Value::as_str)
            .map(|text| Event::Thinking(text.to_owned())),
        "tool-call" => {
            let id = value
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("call_{:032x}", unique_suffix()));
            let name = value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned();
            let input = value
                .get("input")
                .or_else(|| value.get("args"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            let arguments = match input {
                Value::String(text) => text,
                other => serde_json::to_string(&other).unwrap_or_else(|_| "{}".into()),
            };
            Some(Event::Tool {
                id,
                name,
                arguments,
            })
        }
        "finish-step" | "finish" => Some(Event::Done {
            usage: value
                .get("totalUsage")
                .or_else(|| value.get("usage"))
                .filter(|usage| usage.is_object())
                .cloned(),
            stop_reason: value
                .get("rawFinishReason")
                .or_else(|| value.get("finishReason"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        "error" => Some(Event::Error(event_error(value.get("error")))),
        _ => None,
    }
}

fn event_error(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) if !text.is_empty() => text.clone(),
        Some(Value::Object(object)) => object
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("Command Code stream error")
            .to_owned(),
        _ => "Command Code stream error".to_owned(),
    }
}

fn chat_finish_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("length") | Some("max_tokens") | Some("max-tokens") => "length",
        Some("tool_calls") | Some("tool-calls") | Some("tool_use") => "tool_calls",
        _ => "stop",
    }
}

fn chat_usage(usage: &Value) -> Value {
    let input = usage
        .get("inputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("outputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .pointer("/inputTokenDetails/cacheReadTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // Chat shape: `prompt_tokens` includes whatever came from the cache, and
    // `inputTokens` upstream already counts the cached part.
    let mut chat = json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input + output,
    });
    if cached > 0 {
        chat["prompt_tokens_details"] = json!({ "cached_tokens": cached });
    }
    chat
}

/// Folds upstream events into one Chat completion for a non-streaming client.
struct Fold {
    id: String,
    model: String,
    created: u64,
    text: String,
    thinking: String,
    calls: Vec<Value>,
    usage: Option<Value>,
    finish: Option<String>,
    done: bool,
}

impl Fold {
    fn new(req: &Request) -> Self {
        Self {
            id: format!("chatcmpl-{:032x}", unique_suffix()),
            model: req.model.clone(),
            created: unix_seconds(),
            text: String::new(),
            thinking: String::new(),
            calls: Vec::new(),
            usage: None,
            finish: None,
            done: false,
        }
    }

    fn apply(&mut self, event: Event) -> Result<(), Failure> {
        match event {
            Event::Text(text) => self.text.push_str(&text),
            Event::Thinking(text) => self.thinking.push_str(&text),
            Event::Tool {
                id,
                name,
                arguments,
            } => self.calls.push(json!({
                "id": id, "type": "function",
                "function": { "name": name, "arguments": arguments },
            })),
            Event::Done { usage, stop_reason } => {
                if self.done {
                    return Ok(());
                }
                self.done = true;
                if stop_reason.as_deref() == Some("error") {
                    return Err(Failure::Retryable);
                }
                self.usage = usage;
                self.finish = Some(chat_finish_reason(stop_reason.as_deref()).to_owned());
            }
            Event::Error(_) => return Err(Failure::Retryable),
        }
        Ok(())
    }

    fn chat(&self) -> Value {
        let mut message = json!({ "role": "assistant" });
        message["content"] = if self.text.is_empty() {
            Value::Null
        } else {
            json!(self.text)
        };
        if !self.thinking.is_empty() {
            message["reasoning_content"] = json!(self.thinking);
        }
        if !self.calls.is_empty() {
            message["tool_calls"] = Value::Array(self.calls.clone());
        }
        json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": self.finish.clone().unwrap_or_else(|| "stop".into()),
            }],
            "usage": self.usage.as_ref().map(chat_usage).unwrap_or_else(|| json!({})),
        })
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn buffered_reply(req: &Request, response: reqwest::Response) -> Result<Reply, Failure> {
    let body = read_body(response).await?;
    let mut fold = Fold::new(req);
    for line in body.split(|byte| *byte == b'\n') {
        let text = String::from_utf8_lossy(line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let text = text.strip_prefix("data:").map(str::trim).unwrap_or(text);
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if let Some(event) = event(&value) {
            fold.apply(event)?;
        }
    }
    let chat = fold.chat();
    let reply = match req.inbound {
        Inbound::Chat => chat,
        Inbound::Responses => {
            let parallel = serde_json::from_slice::<Value>(&req.body)
                .ok()
                .and_then(|body| body.get("parallel_tool_calls").and_then(Value::as_bool));
            chat_to_responses(&chat, &req.model, parallel)
                .map_err(|error| translate_failure(&error, req.inbound))?
        }
        Inbound::Messages => chat_to_messages(&chat, &req.model)
            .map_err(|error| translate_failure(&error, req.inbound))?,
    };
    let bytes = serde_json::to_vec(&reply).unwrap_or_default();
    Ok(Reply {
        status: StatusCode::OK,
        headers: json_headers(),
        body: futures_util::stream::iter(vec![Ok(Bytes::from(bytes))]).boxed(),
    })
}

async fn read_body(response: reqwest::Response) -> Result<Vec<u8>, Failure> {
    let mut upstream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = upstream.next().await {
        let chunk = chunk.map_err(|_| Failure::Retryable)?;
        body.extend_from_slice(&chunk);
        if body.len() > MAX_NON_STREAM_BODY {
            return Err(Failure::Retryable);
        }
    }
    Ok(body)
}

fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers
}

fn stream_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers
}

const MAX_LINE: usize = 8 * 1024 * 1024;

enum Encoder {
    Responses(ResponsesStream),
    Messages(MessagesStream),
    Chat,
}

/// Turns the NDJSON body into the client's protocol while it arrives.
struct Decoder {
    id: String,
    model: String,
    created: u64,
    inbound: Inbound,
    tail: Vec<u8>,
    encoder: Encoder,
    tool_index: u64,
    finished: bool,
    done: bool,
}

impl Decoder {
    fn new(req: &Request) -> Self {
        let unique = format!("{:032x}", unique_suffix());
        let (id, encoder) = match req.inbound {
            Inbound::Responses => {
                let id = format!("resp_{unique}");
                (
                    id.clone(),
                    Encoder::Responses(ResponsesStream::new(id, &req.model)),
                )
            }
            Inbound::Messages => {
                let id = format!("msg_{unique}");
                (
                    id.clone(),
                    Encoder::Messages(MessagesStream::new(id, &req.model)),
                )
            }
            Inbound::Chat => (format!("chatcmpl-{unique}"), Encoder::Chat),
        };
        Self {
            id,
            model: req.model.clone(),
            created: unix_seconds(),
            inbound: req.inbound,
            tail: Vec::new(),
            encoder,
            tool_index: 0,
            finished: false,
            done: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        self.tail.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = self.tail.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.tail.drain(..=end).collect();
            self.line(&line, &mut out);
            if self.done {
                return out;
            }
        }
        if self.tail.len() > MAX_LINE {
            self.tail.clear();
            let frame = self.error_frame("Command Code event exceeds 8 MiB");
            out.extend(frame);
        }
        out
    }

    /// Flush a final line that arrived without a newline.
    fn finish(&mut self) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.tail.is_empty() {
            let line = std::mem::take(&mut self.tail);
            self.line(&line, &mut out);
        }
        if self.done {
            return out;
        }
        self.done = true;
        match &mut self.encoder {
            Encoder::Responses(stream) => out.extend(stream.finish()),
            Encoder::Messages(stream) => out.extend(stream.finish()),
            Encoder::Chat => out.extend_from_slice(b"data: [DONE]\n\n"),
        }
        out
    }

    fn line(&mut self, line: &[u8], out: &mut Vec<u8>) {
        let text = String::from_utf8_lossy(line);
        let text = text.trim();
        let text = text.strip_prefix("data:").map(str::trim).unwrap_or(text);
        if text.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if !value.is_object() {
            return;
        }
        if let Some(event) = event(&value) {
            self.event(event, out);
        }
    }

    fn event(&mut self, event: Event, out: &mut Vec<u8>) {
        match event {
            Event::Text(text) if !text.is_empty() => {
                let chunk = self.chunk_value(json!({ "content": text }), None, None);
                self.emit(chunk, out);
            }
            Event::Text(_) => {}
            Event::Thinking(text) if !text.is_empty() => {
                let chunk = self.chunk_value(json!({ "reasoning_content": text }), None, None);
                self.emit(chunk, out);
            }
            Event::Thinking(_) => {}
            Event::Tool {
                id,
                name,
                arguments,
            } => {
                let index = self.tool_index;
                self.tool_index += 1;
                let chunk = self.chunk_value(
                    json!({ "tool_calls": [{
                        "index": index, "id": id, "type": "function",
                        "function": { "name": name, "arguments": arguments },
                    }]}),
                    None,
                    None,
                );
                self.emit(chunk, out);
            }
            Event::Done { usage, stop_reason } => {
                if self.finished {
                    return;
                }
                self.finished = true;
                if stop_reason.as_deref() == Some("error") {
                    let frame = self.error_frame(
                        "Command Code upstream ended the turn with finishReason \"error\"",
                    );
                    out.extend(frame);
                    return;
                }
                let chunk = self.chunk_value(
                    json!({}),
                    Some(chat_finish_reason(stop_reason.as_deref())),
                    usage.as_ref(),
                );
                self.emit(chunk, out);
            }
            Event::Error(message) => {
                if self.finished {
                    return;
                }
                let frame = self.error_frame(&message);
                out.extend(frame);
            }
        }
    }

    fn chunk_value(&self, delta: Value, finish: Option<&str>, usage: Option<&Value>) -> Value {
        let mut chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        if let Some(usage) = usage {
            chunk["usage"] = chat_usage(usage);
        }
        chunk
    }

    fn emit(&mut self, chunk: Value, out: &mut Vec<u8>) {
        let text = serde_json::to_string(&chunk).unwrap_or_default();
        match &mut self.encoder {
            Encoder::Responses(stream) => out.extend(stream.push_chunk(&text)),
            Encoder::Messages(stream) => out.extend(stream.push_chunk(&text)),
            Encoder::Chat => {
                out.extend_from_slice(format!("data: {text}\n\n").as_bytes());
            }
        }
    }

    fn error_frame(&mut self, message: &str) -> Vec<u8> {
        self.done = true;
        self.finished = true;
        let error = crate::translate::Error::bad_gateway(message);
        let wire = match self.inbound {
            Inbound::Chat => crate::translate::Inbound::Chat,
            Inbound::Responses => crate::translate::Inbound::Responses,
            Inbound::Messages => crate::translate::Inbound::Messages,
        };
        let body = serde_json::to_string(&error.to_body(wire)).unwrap_or_default();
        match self.inbound {
            Inbound::Chat => format!("data: {body}\n\n").into_bytes(),
            _ => format!("event: error\ndata: {body}\n\n").into_bytes(),
        }
    }
}

fn stream_reply(req: &Request, response: reqwest::Response) -> Reply {
    let decoder = Decoder::new(req);
    let upstream = response.bytes_stream().boxed();
    let body = futures_util::stream::unfold(
        (decoder, upstream),
        |(mut decoder, mut upstream)| async move {
            loop {
                if decoder.done {
                    return None;
                }
                match upstream.next().await {
                    Some(Ok(chunk)) => {
                        let out = decoder.feed(&chunk);
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), (decoder, upstream)));
                        }
                    }
                    Some(Err(_)) => {
                        return Some((
                            Err(std::io::Error::other("command-code upstream stream failed")),
                            (decoder, upstream),
                        ));
                    }
                    None => {
                        let out = decoder.finish();
                        if out.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(out)), (decoder, upstream)));
                    }
                }
            }
        },
    )
    .boxed();
    Reply {
        status: StatusCode::OK,
        headers: stream_headers(),
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cf_page() -> Bytes {
        Bytes::from_static(b"error code: 1010")
    }

    #[test]
    fn deepseek_variants_get_their_provider_prefix() {
        assert_eq!(canonical_model("deepseek-v4.1"), "deepseek/deepseek-v4.1");
        assert_eq!(
            canonical_model("deepseek-v4.1-flash"),
            "deepseek/deepseek-v4.1-flash"
        );
        // Already qualified stays untouched.
        assert_eq!(
            canonical_model("deepseek/deepseek-v4.1"),
            "deepseek/deepseek-v4.1"
        );
        // Other providers are passed through.
        assert_eq!(canonical_model("claude-opus-5-5"), "claude-opus-5-5");
    }

    #[test]
    fn cloudflare_403_is_retryable_not_an_auth_failure() {
        let failure = classify(StatusCode::FORBIDDEN, None, &cf_page());
        assert!(matches!(failure, Failure::Retryable));
    }

    #[test]
    fn a_real_403_with_a_json_body_is_still_auth() {
        let body = Bytes::from_static(br#"{"error":"forbidden"}"#);
        let failure = classify(StatusCode::FORBIDDEN, None, &body);
        assert!(matches!(failure, Failure::Auth));
    }

    #[test]
    fn a_401_is_auth() {
        let failure = classify(StatusCode::UNAUTHORIZED, None, &Bytes::new());
        assert!(matches!(failure, Failure::Auth));
    }

    #[test]
    fn a_json_403_mentioning_cloudflare_still_counts_as_auth() {
        // The API never returns Cloudflare branding in valid JSON, so a JSON
        // body keeps the auth reading even if the text is odd.
        let body = Bytes::from_static(br#"{"error":"cloudflare"}"#);
        assert!(matches!(
            classify(StatusCode::FORBIDDEN, None, &body),
            Failure::Auth
        ));
    }
}
