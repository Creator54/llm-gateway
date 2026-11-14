//! Anthropic (Claude Pro/Max) OAuth.
//!
//! Client id, URLs, scopes and bodies come from opencodex `oauth/anthropic.ts`.

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use super::{post_json, Pending};
use crate::login::{pkce, ui, Options, Tokens};

const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub(crate) const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const PORT: u16 = 54545;
const PATH: &str = "/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference";
/// Kept encoded in the source it was ported from.
const CLIENT_ID_B64: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";
/// Anthropic issues tokens that expire in an hour; refresh a little early.
const REFRESH_SKEW_SECS: u64 = 5 * 60;

fn client_id() -> String {
    String::from_utf8(STANDARD.decode(CLIENT_ID_B64).unwrap_or_default())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The authorize URL for a PKCE challenge and redirect URI.
pub fn auth_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    let client_id = client_id();
    let params = [
        ("code", "true"),
        ("client_id", client_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPES),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    format!("{AUTHORIZE_URL}?{}", super::form(&params))
}

/// A token response as both login and refresh understand it.
fn tokens(body: &Value, refresh_fallback: Option<&str>) -> Result<Tokens> {
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("Anthropic OAuth returned no access token"))?
        .to_string();
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| refresh_fallback.map(str::to_string));
    // A missing, negative or non-finite `expires_in` falls back to an hour.
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v as u64);
    let account = body
        .get("account")
        .and_then(|a| a.get("uuid"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let email = body
        .get("account")
        .and_then(|a| a.get("email_address"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    Ok(Tokens {
        access,
        refresh,
        expires_at: None,
        account,
        email,
        extra: serde_json::Map::new(),
    }
    .expires_in(expires_in, REFRESH_SKEW_SECS))
}

async fn post(http: &reqwest::Client, url: &str, body: Value) -> Result<Value> {
    let (status, payload) = post_json(http, url, &body).await?;
    if !(200..300).contains(&status) {
        let detail = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("request failed");
        return Err(anyhow!("Anthropic OAuth HTTP {status}: {detail}"));
    }
    Ok(payload)
}

/// Exchange an authorization code for tokens.
pub async fn exchange(
    http: &reqwest::Client,
    token_url: &str,
    redirect_uri: &str,
    pasted: &str,
    verifier: &str,
    state: &str,
) -> Result<Tokens> {
    // Claude may return `code#state`, and that fragment overrides the flow state.
    let (code, exchange_state) = match pasted.split_once('#') {
        Some((code, frag)) if !frag.is_empty() => (code, frag),
        Some((code, _)) => (code, state),
        None => (pasted, state),
    };
    let body = json!({
        "grant_type": "authorization_code",
        "client_id": client_id(),
        "code": code,
        "state": exchange_state,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
    });
    tokens(&post(http, token_url, body).await?, None)
}

/// Refresh an access token with a JSON body, as upstream does for Anthropic.
pub async fn refresh(
    http: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    let body = json!({
        "grant_type": "refresh_token",
        "client_id": client_id(),
        "refresh_token": refresh_token,
    });
    tokens(&post(http, token_url, body).await?, Some(refresh_token))
}

/// Browser PKCE login.
pub async fn login(http: &reqwest::Client, opts: &Options) -> Result<Tokens> {
    let pkce = pkce::generate()?;
    let state = pkce::state()?;
    let pending = Pending::begin(opts.port(PORT), "localhost", PATH, state.clone(), true).await?;
    let redirect_uri = pending.redirect_uri();
    let url = auth_url(&redirect_uri, &pkce.challenge, &state);
    ui::info("complete Claude login in your browser");
    let callback = pending.wait(&url, opts).await?;
    let token_url = opts.token_url(TOKEN_URL);
    exchange(
        http,
        &token_url,
        &redirect_uri,
        &callback.code,
        &pkce.verifier,
        &state,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::Router;
    use std::time::Duration;

    #[test]
    fn client_id_and_authorize_url_are_exact() {
        assert_eq!(client_id(), "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        let url = auth_url("http://localhost:54545/callback", "chal", "st");
        assert!(url.starts_with("https://claude.ai/oauth/authorize?code=true&client_id=9d1c250a"));
        assert!(url.contains("scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.ends_with("&state=st"));
    }

    /// Fake token endpoint: checks the request body, then answers like Claude.
    async fn serve() -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/v1/oauth/token",
            post(|body: String| async move {
                let sent: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                assert_eq!(sent["grant_type"], "authorization_code");
                assert_eq!(sent["code_verifier"], "verifier");
                assert_eq!(sent["redirect_uri"], "http://localhost:54545/callback");
                assert_eq!(sent["state"], "code-state");
                axum::Json(json!({
                    "access_token": "access-1",
                    "refresh_token": "refresh-1",
                    "expires_in": 3600,
                    "account": {"uuid": "uuid-1", "email_address": "a@b.c"},
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/v1/oauth/token"), task)
    }

    #[tokio::test]
    async fn exchange_sends_the_documented_body() {
        let (url, task) = serve().await;
        let tokens = exchange(
            &reqwest::Client::new(),
            &url,
            "http://localhost:54545/callback",
            "code-1#code-state",
            "verifier",
            "flow-state",
        )
        .await
        .unwrap();
        assert_eq!(tokens.access, "access-1");
        assert_eq!(tokens.refresh.as_deref(), Some("refresh-1"));
        assert_eq!(tokens.account.as_deref(), Some("uuid-1"));
        assert_eq!(tokens.email.as_deref(), Some("a@b.c"));
        // One hour minus the five minute skew.
        let left = tokens.expires_at.unwrap() - crate::login::now_secs();
        assert!((Duration::from_secs(left).as_secs() as i64 - 3300).abs() < 5);
        task.abort();
    }

    #[tokio::test]
    async fn refresh_keeps_the_old_refresh_token() {
        let app = Router::new().route(
            "/token",
            post(|body: String| async move {
                let sent: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                assert_eq!(sent["grant_type"], "refresh_token");
                assert_eq!(sent["refresh_token"], "old");
                axum::Json(json!({"access_token": "fresh", "expires_in": 60}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let tokens = refresh(
            &reqwest::Client::new(),
            &format!("http://{addr}/token"),
            "old",
        )
        .await
        .unwrap();
        assert_eq!(tokens.access, "fresh");
        assert_eq!(tokens.refresh.as_deref(), Some("old"));
        task.abort();
    }
}
