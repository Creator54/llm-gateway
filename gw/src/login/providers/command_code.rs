//! Command Code login.
//!
//! Ported from opencodex `oauth/command-code.ts`. The studio POSTs the key to a local
//! callback; the identity is then validated with `whoami` before it is stored.

use anyhow::{anyhow, Result};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::Pending;
use crate::login::{callback, pkce, ui, Options, Tokens};

const STUDIO_URL: &str = "https://commandcode.ai";
const PORT: u16 = 5959;
const PATH: &str = "/callback";
const TIMEOUT: Duration = Duration::from_secs(120);

/// The callback carries an API key rather than an authorization code.
#[derive(Clone, Debug, PartialEq)]
pub struct Callback {
    pub api_key: String,
    pub state: String,
    pub user_id: String,
    pub user_name: String,
    pub key_name: String,
}

fn field(body: &Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Validate a callback body against the flow state.
pub fn parse_callback(body: &Value, expected_state: &str) -> Result<Callback> {
    if body.get("state").and_then(Value::as_str) != Some(expected_state) {
        return Err(anyhow!("Command Code OAuth state mismatch"));
    }
    let get = |key: &str| {
        field(body, key)
            .ok_or_else(|| anyhow!("Command Code callback missing {key}"))
    };
    Ok(Callback {
        api_key: get("apiKey")?,
        state: expected_state.to_string(),
        user_id: get("userId")?,
        user_name: get("userName")?,
        key_name: get("keyName")?,
    })
}

/// Validate a raw pasted API key and answer with the identity it belongs to.
pub async fn whoami(http: &reqwest::Client, base_url: &str, api_key: &str) -> Option<(String, String)> {
    let response = http
        .get(format!("{base_url}/alpha/whoami"))
        .timeout(Duration::from_secs(10))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = crate::login::providers::json_body(response).await;
    let user = body.get("user");
    let id = user
        .and_then(|u| u.get("id"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let name = user
        .and_then(|u| u.get("userName"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    id.zip(name)
}

/// Stored tokens: the key is both access and refresh, with no expiry.
fn tokens(api_key: &str, account: Option<String>) -> Tokens {
    let mut tokens = Tokens {
        access: api_key.to_string(),
        refresh: Some(api_key.to_string()),
        expires_at: None,
        account,
        email: None,
        extra: serde_json::Map::new(),
    };
    tokens.extra.insert("auth_mode".into(), "key".into());
    tokens
}

/// A pasted API key, a pasted callback JSON, or a pasted callback URL.
pub fn parse_paste(input: &str, expected_state: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('{') {
        let body: Value = serde_json::from_str(trimmed).ok()?;
        return parse_callback(&body, expected_state).ok().map(|c| c.api_key);
    }
    callback::accept_paste(trimmed, expected_state)
        .ok()
        .map(|c| c.code)
}

/// Read one request: headers plus exactly the announced `Content-Length` body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
            match socket.read(&mut chunk).await.unwrap_or(0) {
                0 => break,
                n => {
                    data.extend_from_slice(&chunk[..n]);
                    continue;
                }
            }
        };
        let head = String::from_utf8_lossy(&data[..end]).to_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let want = end + 4 + length.min(64 * 1024);
        if data.len() >= want {
            break;
        }
        match socket.read(&mut chunk).await.unwrap_or(0) {
            0 => break,
            n => data.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&data).into_owned()
}

/// Serve one studio callback POST.
async fn serve(listener: TcpListener, expected_state: String) -> Result<Callback> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let request = read_request(&mut socket).await;
        let mut parts = request.split("\r\n\r\n");
        let head = parts.next().unwrap_or_default();
        let body = parts.next().unwrap_or_default();
        let mut line = head.lines();
        let request_line = line.next().unwrap_or_default();
        let is_post = request_line.starts_with("POST ");
        let route = request_line.split_whitespace().nth(1).unwrap_or_default();
        let route = route.split('?').next().unwrap_or_default();
        if route != PATH || !is_post {
            let _ = socket.write_all(reply("405 Method Not Allowed", r#"{"success":false}"#).as_bytes()).await;
            continue;
        }
        let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        match parse_callback(&parsed, &expected_state) {
            Ok(callback) => {
                let _ = socket
                    .write_all(reply("200 OK", r#"{"success":true}"#).as_bytes())
                    .await;
                let _ = socket.shutdown().await;
                return Ok(callback);
            }
            Err(err) => {
                let body = json!({"success": false, "error": err.to_string()});
                let _ = socket.write_all(reply("400 Bad Request", &body.to_string()).as_bytes()).await;
            }
        }
    }
}

fn reply(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: {STUDIO_URL}\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Studio callback login with a paste fallback, validated by `whoami`.
pub async fn login(http: &reqwest::Client, opts: &Options) -> Result<Tokens> {
    let base_url = opts.base_url("https://api.commandcode.ai");
    let state = pkce::state()?;
    let pending = Pending::begin(opts.port(PORT), "127.0.0.1", PATH, state.clone(), true).await?;
    let redirect = pending.redirect_uri();
    let url = format!(
        "{STUDIO_URL}/studio/auth/cli?callback={}&state={}",
        super::encode(&redirect),
        super::encode(&state)
    );
    ui::info("sign in with Command Code in the browser");
    if opts.open_browser {
        crate::login::callback::open_browser(&url);
    }
    ui::info(&format!("open this URL to continue: {url}"));
    let key = tokio::select! {
        result = tokio::time::timeout(TIMEOUT, serve(pending.listener, state.clone())) => {
            result.map_err(|_| anyhow!("Command Code OAuth callback timed out"))??.api_key
        }
        pasted = paste_loop(http, &base_url, &state, opts.allow_paste) => pasted?,
    };
    let account = whoami(http, &base_url, &key)
        .await
        .map(|(id, _)| id);
    Ok(tokens(&key, account))
}

async fn paste_loop(
    http: &reqwest::Client,
    base_url: &str,
    state: &str,
    allow: bool,
) -> Result<String> {
    if !allow || !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return std::future::pending().await;
    }
    loop {
        let input = tokio::task::spawn_blocking(|| -> std::io::Result<String> {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            Ok(line.trim().to_string())
        })
        .await??;
        let Some(key) = parse_paste(&input, state) else {
            ui::warn("that paste was not a valid Command Code callback or key");
            continue;
        };
        if whoami(http, base_url, &key).await.is_some() {
            return Ok(key);
        }
        ui::warn("that API key was rejected by the provider");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = "st";

    #[test]
    fn callback_requires_matching_state_and_fields() {
        let ok = json!({
            "apiKey": "k", "state": "st", "userId": "u", "userName": "name", "keyName": "cli",
        });
        let parsed = parse_callback(&ok, STATE).unwrap();
        assert_eq!(parsed.api_key, "k");
        assert_eq!(parsed.key_name, "cli");
        let mut wrong = ok.clone();
        wrong["state"] = json!("other");
        assert!(parse_callback(&wrong, STATE).is_err());
        let mut missing = ok.clone();
        missing.as_object_mut().unwrap().remove("userName");
        assert!(parse_callback(&missing, STATE).is_err());
    }

    #[test]
    fn pastes_accept_json_keys_and_urls() {
        let body = json!({
            "apiKey": "k", "state": "st", "userId": "u", "userName": "n", "keyName": "cli",
        })
        .to_string();
        assert_eq!(parse_paste(&body, STATE), Some("k".to_string()));
        assert_eq!(
            parse_paste("http://127.0.0.1:5959/callback?code=k2&state=st", STATE),
            Some("k2".to_string())
        );
        assert_eq!(parse_paste("bare-key", STATE), Some("bare-key".to_string()));
        assert_eq!(parse_paste("http://x/callback?code=k&state=no", STATE), None);
    }

    #[tokio::test]
    async fn studio_callback_is_served() {
        let (listener, port) = callback::bind(0, false).await.unwrap();
        let task = tokio::spawn(serve(listener, STATE.to_string()));
        let body = json!({
            "apiKey": "k", "state": "st", "userId": "u", "userName": "n", "keyName": "cli",
        })
        .to_string();
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!(
            "POST /callback HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        assert!(response.contains("200 OK"), "response: {response:?}");
        assert_eq!(task.await.unwrap().unwrap().api_key, "k");
    }

    #[tokio::test]
    async fn whoami_reads_the_identity() {
        let app = axum::Router::new().route(
            "/alpha/whoami",
            axum::routing::get(|| async {
                axum::Json(json!({"user": {"id": "u1", "userName": "neo"}}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://{addr}");
        let identity = whoami(&reqwest::Client::new(), &base, "k").await;
        assert_eq!(identity, Some(("u1".to_string(), "neo".to_string())));
        task.abort();
    }
}
