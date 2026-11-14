//! OpenAI / ChatGPT login: the localhost callback flow and the headless device grant.
//!
//! Client id, URLs, scopes and bodies come from opencodex `oauth/chatgpt.ts` and
//! `oauth/chatgpt-device.ts`.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Instant;

use super::{jwt_email, openai_account_id, post_form, Pending};
use crate::login::{device, pkce, ui, Flow, Options, Tokens};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
pub(crate) const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
const PORT: u16 = 1455;
const PATH: &str = "/auth/callback";
const ORIGINATOR: &str = "opencodex";

const USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
/// Fixed, and safe to show anywhere.
pub const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";

/// Build the authorization URL for a PKCE challenge and redirect URI.
pub fn auth_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("codex_cli_simplified_flow", "true"),
        ("originator", ORIGINATOR),
        ("id_token_add_organizations", "true"),
    ];
    format!("{AUTH_URL}?{}", super::form(&params))
}

/// Build tokens from a token endpoint payload.
fn tokens(body: &Value) -> Result<Tokens> {
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("ChatGPT token response missing access token"))?
        .to_string();
    let id_token = body.get("id_token").and_then(Value::as_str);
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v as u64);
    Ok(Tokens {
        account: openai_account_id(id_token, &access),
        email: jwt_email(id_token, &access),
        access,
        refresh,
        expires_at: None,
        extra: serde_json::Map::new(),
    }
    .expires_in(expires_in, 0))
}

fn failure(stage: &str, status: u16) -> anyhow::Error {
    anyhow!("ChatGPT {stage} failed: HTTP {status}")
}

/// Exchange an authorization code via form-urlencoded POST.
pub async fn exchange(
    http: &reqwest::Client,
    token_url: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<Tokens> {
    let (status, body) = post_form(
        http,
        token_url,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ],
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(failure("token exchange", status));
    }
    tokens(&body)
}

/// Refresh a ChatGPT token (form-urlencoded per RFC 6749 §6).
pub async fn refresh(
    http: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    let (status, body) = post_form(
        http,
        token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ],
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(failure("refresh", status));
    }
    tokens(&body)
}

/// Browser PKCE login on the registered localhost callback.
pub async fn browser(http: &reqwest::Client, opts: &Options) -> Result<Tokens> {
    let pkce = pkce::generate()?;
    let state = pkce::state()?;
    // The redirect URI is registered as a fixed localhost:1455 callback, so a busy
    // port cannot fall back to an ephemeral one.
    let pending = Pending::begin(opts.port(PORT), "localhost", PATH, state.clone(), false).await?;
    let redirect_uri = format!("http://localhost:{}{PATH}", pending.port);
    let url = auth_url(&redirect_uri, &pkce.challenge, &state);
    ui::info("complete ChatGPT login in your browser");
    let callback = pending.wait(&url, opts).await?;
    let token_url = opts.token_url(TOKEN_URL);
    exchange(
        http,
        &token_url,
        &redirect_uri,
        &callback.code,
        &pkce.verifier,
    )
    .await
}

/// Device grant: ask for a user code, poll for the grant, then exchange it.
///
/// Upstream differs from RFC 8628 here: the poll returns the authorization code
/// *and* a server-generated PKCE verifier, so none is generated locally.
pub async fn device(http: &reqwest::Client, opts: &Options) -> Result<Tokens> {
    let usercode_url = opts
        .endpoints
        .device_url
        .clone()
        .unwrap_or_else(|| USERCODE_URL.to_string());
    let poll_url = opts
        .endpoints
        .device_token_url
        .clone()
        .unwrap_or_else(|| DEVICE_TOKEN_URL.to_string());
    let code = device::request_user_code(http, &usercode_url, CLIENT_ID).await?;
    ui::info(&format!(
        "open {DEVICE_VERIFICATION_URL} and enter code: {}",
        code.user_code
    ));
    if opts.open_browser {
        crate::login::callback::open_browser(DEVICE_VERIFICATION_URL);
    }
    let poll_body = json!({
        "device_auth_id": code.device_auth_id,
        "user_code": code.user_code,
    });
    let deadline = Instant::now() + device::TTL;
    let grant = device::poll_with(
        || device::poll_once(http, &poll_url, &poll_body),
        deadline,
        code.interval,
        std::time::Duration::from_secs(5),
    )
    .await?;
    let authorization_code = grant
        .get("authorization_code")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("ChatGPT device authorization response missing required fields"))?;
    let verifier = grant
        .get("code_verifier")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("ChatGPT device authorization response missing required fields"))?;
    let token_url = opts.token_url(TOKEN_URL);
    exchange(
        http,
        &token_url,
        DEVICE_REDIRECT_URI,
        authorization_code,
        verifier,
    )
    .await
}

/// Pick the flow the caller asked for; browser is the default.
pub async fn login(http: &reqwest::Client, opts: &Options) -> Result<Tokens> {
    match opts.flow {
        Flow::Device => device(http, opts).await,
        Flow::Auto | Flow::Browser => browser(http, opts).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::Router;

    #[test]
    fn auth_url_carries_the_registered_fields() {
        let url = auth_url("http://localhost:1455/auth/callback", "chal", "st");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?response_type=code"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains(
            "scope=openid+profile+email+offline_access+api.connectors.read+api.connectors.invoke"
        ));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("id_token_add_organizations=true"));
        assert!(url.contains("originator=opencodex"));
        assert!(url.contains("&state=st&"));
    }

    #[tokio::test]
    async fn token_exchange_reads_the_identity_claims() {
        let app = Router::new().route(
            "/oauth/token",
            post(|body: String| async move {
                assert!(body.contains("grant_type=authorization_code"));
                assert!(body.contains("code_verifier=verifier"));
                assert!(body.contains(
                    "redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback"
                ));
                axum::Json(json!({
                    "access_token": "access-1",
                    "refresh_token": "refresh-1",
                    "expires_in": 3600,
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let tokens = exchange(
            &reqwest::Client::new(),
            &format!("http://{addr}/oauth/token"),
            DEVICE_REDIRECT_URI,
            "code-1",
            "verifier",
        )
        .await
        .unwrap();
        assert_eq!(tokens.access, "access-1");
        assert_eq!(tokens.refresh.as_deref(), Some("refresh-1"));
        assert!(tokens.expires_at.unwrap() >= crate::login::now_secs() + 3500);
        task.abort();
    }

    #[test]
    fn token_payload_without_access_is_an_error() {
        assert!(tokens(&json!({"expires_in": 10})).is_err());
    }
}
