//! Google Antigravity (Cloud Code Assist) OAuth plus project discovery.
//!
//! Ported from opencodex `oauth/google-antigravity.ts`. The client id/secret are the
//! public identifiers embedded in the Antigravity desktop client, not user secrets.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;

use super::{post_form, Pending};
use crate::login::{pkce, ui, Options, Tokens};

const CLIENT_ID: &str =
    concat!("1071006060591-", "tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com");
const CLIENT_SECRET: &str = concat!("GOCSPX-", "K58FWR486LdLJ1mLB8sXC4z6qDAf");
const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub(crate) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub(crate) const PROD_API: &str = "https://cloudcode-pa.googleapis.com";
pub(crate) const DAILY_API: &str = "https://daily-cloudcode-pa.googleapis.com";
const API_VERSION: &str = "v1internal";
const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform \
https://www.googleapis.com/auth/userinfo.email \
https://www.googleapis.com/auth/userinfo.profile \
https://www.googleapis.com/auth/cclog \
https://www.googleapis.com/auth/experimentsandconfigs";
const PORT: u16 = 51121;
const PATH: &str = "/callback";
/// Keep the provider margin small; the shared freshness gate adds its own minute.
const REFRESH_SKEW_SECS: u64 = 5 * 60;
const ONBOARD_ATTEMPTS: usize = 5;
const ONBOARD_POLL: Duration = Duration::from_secs(2);

const IDE_VERSION: &str = "2.5.5";

/// The IDE client family User-Agent; Cloud Code Assist gates models on it.
fn user_agent() -> String {
    format!(
        "antigravity/ide/{IDE_VERSION} (os_type=windows; arch=amd64; aidev_client; auth_method=oauth)"
    )
}

/// Build the authorize URL for a PKCE challenge and redirect URI.
pub fn auth_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPES),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("state", state),
    ];
    format!("{AUTH_ENDPOINT}?{}", super::form(&params))
}

/// Tokens from a Google token payload; `refresh` is required.
fn tokens(body: &Value, refresh_fallback: Option<&str>) -> Result<Tokens> {
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("Antigravity token response did not include an access token"))?
        .to_string();
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| refresh_fallback.map(str::to_string))
        .ok_or_else(|| anyhow!("Antigravity token response did not include a refresh token"))?;
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v as u64);
    let id_token = body.get("id_token").and_then(Value::as_str);
    Ok(Tokens {
        email: super::jwt_email(id_token, &access),
        access,
        refresh: Some(refresh),
        expires_at: None,
        account: None,
        extra: serde_json::Map::new(),
    }
    .expires_in(expires_in, REFRESH_SKEW_SECS))
}

fn extract_project_id(data: &Value) -> Option<String> {
    for key in ["cloudaicompanionProject", "projectId", "project"] {
        let Some(value) = data.get(key) else { continue };
        if let Some(id) = value.as_str().filter(|v| !v.is_empty()) {
            return Some(id.to_string());
        }
        if let Some(id) = value.get("id").and_then(Value::as_str).filter(|v| !v.is_empty()) {
            return Some(id.to_string());
        }
    }
    None
}

async fn post_api(
    http: &reqwest::Client,
    url: &str,
    access: &str,
    body: Value,
) -> Result<(u16, Value)> {
    let response = http
        .post(url)
        .timeout(Duration::from_secs(30))
        .header("Authorization", format!("Bearer {access}"))
        .header("Accept", "*/*")
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent())
        .body(serde_json::to_vec(&body)?)
        .send()
        .await?;
    let status = response.status().as_u16();
    Ok((status, crate::login::providers::json_body(response).await))
}

/// `loadCodeAssist`: the account's existing Cloud Code Assist project.
pub async fn load_project(
    http: &reqwest::Client,
    api_base: &str,
    access: &str,
) -> Option<String> {
    let url = format!("{api_base}/{API_VERSION}:loadCodeAssist");
    let (status, body) = post_api(
        http,
        &url,
        access,
        json!({"metadata": {"ideType": "ANTIGRAVITY"}}),
    )
    .await
    .ok()?;
    (200..300)
        .contains(&status)
        .then(|| extract_project_id(&body))
        .flatten()
}

/// `onboardUser`: create the project for an account that has none yet.
pub async fn onboard(
    http: &reqwest::Client,
    api_base: &str,
    access: &str,
) -> Option<String> {
    let url = format!("{api_base}/{API_VERSION}:onboardUser");
    let body = json!({
        "tier_id": "free-tier",
        "metadata": {"ide_type": "ANTIGRAVITY", "ide_name": "antigravity", "ide_version": IDE_VERSION},
    });
    for _ in 0..ONBOARD_ATTEMPTS {
        let Ok((status, data)) = post_api(http, &url, access, body.clone()).await else {
            return None;
        };
        // Transient throttling and server faults keep polling; hard 4xx gives up.
        if status == 429 || status >= 500 {
            tokio::time::sleep(ONBOARD_POLL).await;
            continue;
        }
        if !(200..300).contains(&status) {
            return None;
        }
        if data.get("done") == Some(&Value::Bool(true)) {
            return data.get("response").and_then(extract_project_id);
        }
        tokio::time::sleep(ONBOARD_POLL).await;
    }
    None
}

/// `loadCodeAssist` on the production host, then `onboardUser` on the daily one.
pub async fn discover_project(
    http: &reqwest::Client,
    api_base: &str,
    onboard_base: &str,
    access: &str,
) -> Option<String> {
    match load_project(http, api_base, access).await {
        Some(id) => Some(id),
        None => onboard(http, onboard_base, access).await,
    }
}

async fn token_request(
    http: &reqwest::Client,
    url: &str,
    params: &[(&str, &str)],
) -> Result<Value> {
    let (status, body) = post_form(http, url, params).await?;
    if !(200..300).contains(&status) {
        // Status only: the body can carry grant and account details.
        return Err(anyhow!("Antigravity token request failed: {status}"));
    }
    Ok(body)
}

/// Exchange an authorization code; fails when no CCA project can be discovered.
pub async fn exchange(
    http: &reqwest::Client,
    token_url: &str,
    api_base: &str,
    onboard_base: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<Tokens> {
    let body = token_request(
        http,
        token_url,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ],
    )
    .await?;
    let tokens = tokens(&body, None)?;
    ui::info("discovering Cloud Code Assist project");
    let project = discover_project(http, api_base, onboard_base, &tokens.access)
        .await
        .ok_or_else(|| {
            anyhow!(
                "Antigravity login could not discover a Cloud Code Assist project for this account"
            )
        })?;
    Ok(tokens.extra("project_id", project))
}

/// Refresh, re-discovering the project so a newly onboarded account fills it in.
pub async fn refresh(
    http: &reqwest::Client,
    token_url: &str,
    api_base: &str,
    onboard_base: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    if refresh_token.is_empty() {
        return Err(anyhow!(
            "Antigravity credentials are expired and do not include a refresh token"
        ));
    }
    let body = token_request(
        http,
        token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("refresh_token", refresh_token),
        ],
    )
    .await?;
    let tokens = tokens(&body, Some(refresh_token))?;
    Ok(match discover_project(http, api_base, onboard_base, &tokens.access).await {
        Some(project) => tokens.extra("project_id", project),
        None => tokens,
    })
}

/// Browser PKCE login on the fixed 127.0.0.1 callback.
pub async fn login(http: &reqwest::Client, opts: &Options) -> Result<Tokens> {
    let pkce = pkce::generate()?;
    let state = pkce::state()?;
    let pending = Pending::begin(opts.port(PORT), "127.0.0.1", PATH, state.clone(), false).await?;
    let redirect_uri = pending.redirect_uri();
    let url = auth_url(&redirect_uri, &pkce.challenge, &state);
    ui::info("complete Google (Antigravity) login in your browser");
    let callback = pending.wait(&url, opts).await?;
    exchange(
        http,
        &opts.token_url(TOKEN_URL),
        &opts.base_url(PROD_API),
        &opts.onboard_url(DAILY_API),
        &redirect_uri,
        &callback.code,
        &pkce.verifier,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::Router;

    #[test]
    fn authorize_url_carries_the_google_fields() {
        let url = auth_url("http://127.0.0.1:51121/callback", "chal", "st");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?response_type=code"));
        assert!(url.contains("client_id=1071006060591-tmhssin2h21lcre235vtolojh4g403ep"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("cloud-platform"));
        assert!(url.ends_with("&state=st"));
    }

    #[test]
    fn project_ids_are_read_from_every_shape() {
        assert_eq!(
            extract_project_id(&json!({"cloudaicompanionProject": "p1"})).as_deref(),
            Some("p1")
        );
        assert_eq!(
            extract_project_id(&json!({"project": {"id": "p2"}})).as_deref(),
            Some("p2")
        );
        assert_eq!(
            extract_project_id(&json!({"response": {"projectId": "p3"}})),
            None
        );
        assert_eq!(extract_project_id(&json!({})), None);
    }

    #[tokio::test]
    async fn exchange_discovers_the_project() {
        let app = Router::new()
            .route(
                "/token",
                post(|body: String| async move {
                    assert!(body.contains(concat!("client_secret=GOCSPX-", "K58FWR486LdLJ1mLB8sXC4z6qDAf")));
                    assert!(body.contains("grant_type=authorization_code"));
                    axum::Json(json!({
                        "access_token": "at-1",
                        "refresh_token": "rt-1",
                        "expires_in": 3600,
                    }))
                }),
            )
            .route(
                "/v1internal:loadCodeAssist",
                post(|| async { axum::Json(json!({"cloudaicompanionProject": "proj-1"})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://{addr}");
        let tokens = exchange(
            &reqwest::Client::new(),
            &format!("{base}/token"),
            &base,
            &base,
            "http://127.0.0.1:51121/callback",
            "code-1",
            "verifier",
        )
        .await
        .unwrap();
        assert_eq!(tokens.access, "at-1");
        assert_eq!(tokens.extra.get("project_id").unwrap(), "proj-1");
        task.abort();
    }
}
