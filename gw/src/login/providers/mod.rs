//! Provider-specific login flows and the small helpers they share.

pub mod anthropic;
pub mod antigravity;
pub mod command_code;
pub mod key;
pub mod kiro;
pub mod openai;

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;
use std::time::Duration;
use tokio::net::TcpListener;

use super::{callback, ui, Credential, Options, Tokens};

/// Provider HTTP calls are bounded; a hung connection must not hold the login slot.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Percent-encode one component the way `URLSearchParams` does.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            byte => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `a=1&b=2` with every value encoded.
pub fn form(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Append `params` as a query string to `base`.
pub fn with_query(base: &str, params: &[(&str, &str)]) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}{}", form(params))
}

/// POST a form body, returning status and the parsed JSON payload.
pub async fn post_form(
    http: &reqwest::Client,
    url: &str,
    params: &[(&str, &str)],
) -> Result<(u16, Value)> {
    let response = http
        .post(url)
        .timeout(TIMEOUT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form(params))
        .send()
        .await?;
    let status = response.status().as_u16();
    Ok((status, json_body(response).await))
}

/// POST a JSON body, returning status and the parsed JSON payload.
pub async fn post_json(http: &reqwest::Client, url: &str, body: &Value) -> Result<(u16, Value)> {
    let response = http
        .post(url)
        .timeout(TIMEOUT)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(serde_json::to_vec(body)?)
        .send()
        .await?;
    let status = response.status().as_u16();
    Ok((status, json_body(response).await))
}

/// Parse a body as JSON. `reqwest` was built without its `json` feature.
pub async fn json_body(response: reqwest::Response) -> Value {
    response
        .bytes()
        .await
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

/// Decode a JWT payload. Claims are routing hints, never proof of authenticity.
pub fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn claim<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
}

/// ChatGPT account id: top-level claim, then the `api.openai.com/auth` namespace,
/// then the first organisation id.
pub fn openai_account_id(id_token: Option<&str>, access_token: &str) -> Option<String> {
    for token in id_token.into_iter().chain(std::iter::once(access_token)) {
        let Some(payload) = jwt_payload(token) else {
            continue;
        };
        if let Some(id) = claim(&payload, "chatgpt_account_id") {
            return Some(id.to_string());
        }
        if let Some(id) = payload
            .get("https://api.openai.com/auth")
            .and_then(|ns| ns.get("chatgpt_account_id"))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            return Some(id.to_string());
        }
        if let Some(id) = payload
            .get("organizations")
            .and_then(Value::as_array)
            .and_then(|orgs| orgs.first())
            .and_then(|org| org.get("id"))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            return Some(id.to_string());
        }
    }
    None
}

/// Lowercased `email` claim from the id token, else the access token.
pub fn jwt_email(id_token: Option<&str>, access_token: &str) -> Option<String> {
    for token in id_token.into_iter().chain(std::iter::once(access_token)) {
        if let Some(email) = jwt_payload(token).and_then(|p| {
            p.get("email")
                .and_then(Value::as_str)
                .map(|e| e.to_lowercase())
        }) {
            return Some(email);
        }
    }
    None
}

/// A bound callback listener plus the redirect URI that points at it.
pub struct Pending {
    listener: TcpListener,
    pub port: u16,
    pub path: String,
    pub host: String,
    pub state: String,
}

impl Pending {
    /// Bind `port`, falling back to an ephemeral port when the provider's redirect
    /// URI is not registered as a fixed one.
    pub async fn begin(
        port: u16,
        host: &str,
        path: &str,
        state: String,
        fallback: bool,
    ) -> Result<Self> {
        let (listener, port) = callback::bind(port, fallback).await?;
        Ok(Self {
            listener,
            port,
            path: path.to_string(),
            host: host.to_string(),
            state,
        })
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://{}:{}{}", self.host, self.port, self.path)
    }

    /// Announce `url`, open the browser, then wait for the redirect or a paste.
    pub async fn wait(self, url: &str, opts: &Options) -> Result<callback::Callback> {
        ui::info(&format!("open this URL to continue: {url}"));
        if opts.open_browser {
            callback::open_browser(url);
        }
        callback::wait(self.listener, &self.path, &self.state, opts.allow_paste).await
    }
}

/// Dispatch on the provider name.
pub async fn run(provider: &str, http: &reqwest::Client, opts: &Options) -> Result<Credential> {
    let oauth = |result: Result<Tokens>| result.map(Credential::OAuth);
    match provider {
        "anthropic" => oauth(anthropic::login(http, opts).await),
        "openai" => oauth(openai::login(http, opts).await),
        "antigravity" => oauth(antigravity::login(http, opts).await),
        "kiro" => kiro::login(http, opts).await,
        "command-code" => oauth(command_code::login(http, opts).await),
        "opencode" => key::login(http, provider, super::cli::KeySource::Prompt).await,
        other => Err(anyhow!("unknown login provider: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding_matches_url_search_params() {
        assert_eq!(encode("a b/c"), "a+b%2Fc");
        assert_eq!(
            form(&[("grant_type", "authorization_code"), ("code", "a b")]),
            "grant_type=authorization_code&code=a+b"
        );
        assert_eq!(with_query("https://x/y", &[("a", "1")]), "https://x/y?a=1");
    }

    fn jwt(payload: Value) -> String {
        format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    #[test]
    fn jwt_claims_supply_account_and_email() {
        let token = jwt(serde_json::json!({"email": "A@B.c", "chatgpt_account_id": "acct-1"}));
        assert_eq!(openai_account_id(None, &token).as_deref(), Some("acct-1"));
        assert_eq!(jwt_email(None, &token).as_deref(), Some("a@b.c"));
        assert_eq!(openai_account_id(None, "not-a-jwt"), None);
    }

    #[test]
    fn namespaced_and_organization_claims_are_read() {
        let ns = jwt(serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "ns-1"},
        }));
        assert_eq!(openai_account_id(None, &ns).as_deref(), Some("ns-1"));
        let orgs = jwt(serde_json::json!({"organizations": [{"id": "org-1"}]}));
        assert_eq!(openai_account_id(None, &orgs).as_deref(), Some("org-1"));
    }
}
