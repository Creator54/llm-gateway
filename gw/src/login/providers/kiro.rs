//! Kiro device login and token refresh.
//!
//! Ported from opencodex `oauth/kiro-device-login.ts` (social device grant) and
//! `oauth/kiro.ts` (desktop refresh endpoint).

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;

use super::{post_json, Credential};
use crate::login::{device, ui, Options, Tokens};

const SOCIAL: &str = "https://prod.us-east-1.auth.desktop.kiro.dev";
const REFRESH_URL: &str = "https://prod.{region}.auth.desktop.kiro.dev/refreshToken";
const CLIENT_ID: &str = "kiro-cli";
const LOGIN_PROVIDER: &str = "Google";
const REGION: &str = "us-east-1";

/// `arn:<part>:codewhisperer:<part>:<12 digits>:profile/<id>`.
fn valid_profile_arn(value: &str) -> bool {
    let mut parts = value.split(':');
    let (Some("arn"), Some(_), Some("codewhisperer"), Some(_), Some(account), Some(resource)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    account.len() == 12
        && account.chars().all(|c| c.is_ascii_digit())
        && resource.strip_prefix("profile/").is_some_and(|id| {
            !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        && parts.next().is_none()
}

fn field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Only a plain https URL is shown or opened.
fn usable_verification_uri(value: &str) -> bool {
    value.len() <= 2048
        && !value.chars().any(|c| c.is_control())
        && value
            .strip_prefix("https://")
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('@'))
}

fn millis(value: &Value, key: &str, default: u64) -> Duration {
    let raw = value.get(key).and_then(Value::as_f64).filter(|v| *v > 0.0);
    Duration::from_millis(raw.map(|v| v as u64).unwrap_or(default))
}

/// Ask for a device code from the social device endpoint.
pub async fn request_code(
    http: &reqwest::Client,
    device_url: &str,
) -> Result<(String, String, String, Duration)> {
    let body = json!({"clientId": CLIENT_ID, "loginProvider": LOGIN_PROVIDER});
    let (status, payload) = post_json(http, device_url, &body).await?;
    if !(200..300).contains(&status) {
        return Err(anyhow!("Kiro device authorization failed"));
    }
    let device_code =
        field(&payload, "deviceCode").ok_or_else(|| anyhow!("Kiro device authorization failed"))?;
    let user_code =
        field(&payload, "userCode").ok_or_else(|| anyhow!("Kiro device authorization failed"))?;
    let verification = field(&payload, "verificationUriComplete")
        .filter(|uri| usable_verification_uri(uri))
        .or_else(|| field(&payload, "verificationUri"))
        .filter(|uri| usable_verification_uri(uri))
        .ok_or_else(|| anyhow!("Kiro device authorization failed"))?;
    let interval = millis(&payload, "intervalInMilliseconds", 5_000);
    Ok((device_code, user_code, verification, interval))
}

/// Turn an approved poll response into tokens, rejecting a malformed profile ARN.
pub fn tokens(grant: &Value) -> Result<Tokens> {
    let access = field(grant, "accessToken")
        .ok_or_else(|| anyhow!("Kiro device authorization response missing accessToken"))?;
    let refresh = field(grant, "refreshToken")
        .ok_or_else(|| anyhow!("Kiro device authorization response missing refreshToken"))?;
    let profile_arn = field(grant, "profileArn")
        .filter(|arn| valid_profile_arn(arn))
        .ok_or_else(|| {
            anyhow!("Kiro device authorization response carried no usable profile ARN")
        })?;
    let expires_in = grant
        .get("expiresIn")
        .and_then(Value::as_f64)
        .filter(|v| *v > 0.0)
        .map(|v| v as u64);
    Ok(Tokens {
        access,
        refresh: Some(refresh),
        expires_at: None,
        account: Some(profile_arn.clone()),
        email: None,
        extra: serde_json::Map::new(),
    }
    .expires_in(expires_in, 0)
    .extra("profile_arn", profile_arn)
    .extra("sso_region", REGION)
    .extra("api_region", REGION))
}

/// Device login: no browser listener, only a user code to type.
pub async fn login(http: &reqwest::Client, opts: &Options) -> Result<Credential> {
    let device_url = opts
        .endpoints
        .device_url
        .clone()
        .unwrap_or_else(|| format!("{SOCIAL}/oauth/device/authorization"));
    let poll_url = opts
        .endpoints
        .device_token_url
        .clone()
        .unwrap_or_else(|| format!("{SOCIAL}/oauth/device/poll"));
    let (device_code, user_code, verification, interval) = request_code(http, &device_url).await?;
    ui::info(&format!("open {verification} and enter code: {user_code}"));
    if opts.open_browser {
        crate::login::callback::open_browser(&verification);
    }
    let poll_body = json!({"clientId": CLIENT_ID, "deviceCode": device_code});
    let deadline = std::time::Instant::now() + device::TTL;
    let grant = device::poll_with(
        || device::poll_once(http, &poll_url, &poll_body),
        deadline,
        interval.max(Duration::from_secs(1)),
        Duration::from_secs(5),
    )
    .await?;
    let mut tokens = tokens(&grant)?;
    // The profile ARN is shared by every login under one organisation, so it cannot tell
    // accounts apart; the signed-in user can.
    if let Some((user_id, email)) = identity(http, &tokens).await {
        tokens.account = Some(user_id);
        tokens.email = email;
    }
    Ok(Credential::OAuth(tokens))
}

/// The signed-in user's id and email from the usage endpoint, or `None` if it can't be read.
async fn identity(http: &reqwest::Client, tokens: &Tokens) -> Option<(String, Option<String>)> {
    let arn = tokens.extra.get("profile_arn")?.as_str()?;
    let body = json!({"origin": "AI_EDITOR", "isEmailRequired": true, "profileArn": arn});
    let reply = http
        .post(format!("https://management.{REGION}.kiro.dev/"))
        .query(&[
            ("origin", "AI_EDITOR"),
            ("isEmailRequired", "true"),
            ("profileArn", arn),
        ])
        .bearer_auth(&tokens.access)
        .header("content-type", "application/x-amz-json-1.0")
        .header("x-amz-target", "AmazonCodeWhispererService.GetUsageLimits")
        .header("x-amzn-codewhisperer-optout", "true")
        .body(body.to_string())
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    let reply: Value = serde_json::from_str(&reply).ok()?;
    let user = reply.get("userInfo")?;
    let id = user.get("userId")?.as_str()?.to_owned();
    let email = user
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_lowercase);
    Some((id, email))
}

/// Refresh against the desktop endpoint; the region comes from the stored account.
pub async fn refresh(
    http: &reqwest::Client,
    token_url: Option<&str>,
    region: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    if refresh_token.is_empty() {
        return Err(anyhow!("Kiro has no refresh token; re-run the login"));
    }
    let url = token_url
        .map(str::to_string)
        .unwrap_or_else(|| REFRESH_URL.replace("{region}", region));
    let (status, body) = post_json(http, &url, &json!({"refreshToken": refresh_token})).await?;
    if !(200..300).contains(&status) {
        return Err(anyhow!("Kiro token refresh failed: HTTP {status}"));
    }
    let access = field(&body, "accessToken")
        .ok_or_else(|| anyhow!("Kiro refresh returned no accessToken"))?;
    let expires_in = body
        .get("expiresIn")
        .and_then(Value::as_f64)
        .filter(|v| *v > 0.0)
        .map(|v| v as u64);
    Ok(Tokens {
        access,
        refresh: field(&body, "refreshToken").or_else(|| Some(refresh_token.to_string())),
        expires_at: None,
        account: None,
        email: None,
        extra: serde_json::Map::new(),
    }
    .expires_in(expires_in, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::Router;

    const ARN: &str = "arn:aws:codewhisperer:us-east-1:123456789012:profile/ABC-1";

    fn grant() -> Value {
        json!({
            "accessToken": "at", "refreshToken": "rt", "expiresIn": 3600,
            "profileArn": ARN,
        })
    }

    #[test]
    fn verification_uris_are_checked() {
        assert!(usable_verification_uri("https://view.awsapps.com/start"));
        assert!(!usable_verification_uri("http://view.awsapps.com/start"));
        assert!(!usable_verification_uri("https://user@host/"));
        assert!(!usable_verification_uri("https://host/\u{0}"));
    }

    #[test]
    fn profile_arns_are_checked() {
        assert!(valid_profile_arn(ARN));
        assert!(!valid_profile_arn(
            "arn:aws:codewhisperer:us-east-1:123:profile/X"
        ));
        assert!(!valid_profile_arn(
            "arn:aws:codewhisperer:us-east-1:123456789012:profile/"
        ));
        assert!(!valid_profile_arn("not-an-arn"));
        assert!(!valid_profile_arn(
            "arn:aws:codewhisperer:us-east-1:123456789012:profile/X:extra"
        ));
    }

    #[test]
    fn tokens_require_the_three_fields() {
        let tokens = super::tokens(&grant()).unwrap();
        assert_eq!(tokens.access, "at");
        assert_eq!(tokens.extra.get("profile_arn").unwrap(), ARN);
        assert_eq!(tokens.extra.get("sso_region").unwrap(), REGION);
        assert_eq!(tokens.account.as_deref(), Some(ARN));
        let mut broken = grant();
        broken["profileArn"] = json!("bad");
        assert!(super::tokens(&broken).is_err());
        let mut missing = grant();
        missing.as_object_mut().unwrap().remove("refreshToken");
        assert!(super::tokens(&missing).is_err());
    }

    #[tokio::test]
    async fn device_login_polls_until_approved() {
        let app = Router::new()
            .route(
                "/oauth/device/authorization",
                post(|| async {
                    axum::Json(json!({
                        "deviceCode": "dc",
                        "userCode": "ABCD-1234",
                        "verificationUri": "https://view.awsapps.com/start",
                        "expiresInMilliseconds": 300000,
                        "intervalInMilliseconds": 1,
                    }))
                }),
            )
            .route(
                "/oauth/device/poll",
                post(|| async {
                    axum::Json(json!({
                        "status": "approved",
                        "accessToken": "at",
                        "refreshToken": "rt",
                        "expiresIn": 3600,
                        "profileArn": ARN,
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://{addr}");
        let mut opts = Options::default();
        opts.endpoints.device_url = Some(format!("{base}/oauth/device/authorization"));
        opts.endpoints.device_token_url = Some(format!("{base}/oauth/device/poll"));
        opts.open_browser = false;
        let credential = login(&reqwest::Client::new(), &opts).await.unwrap();
        let Credential::OAuth(tokens) = credential else {
            panic!("kiro login must be OAuth");
        };
        assert_eq!(tokens.access, "at");
        task.abort();
    }

    #[tokio::test]
    async fn refresh_keeps_the_old_refresh_token() {
        let app = Router::new().route(
            "/refreshToken",
            post(|| async { axum::Json(json!({"accessToken": "fresh", "expiresIn": 60})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let tokens = super::refresh(
            &reqwest::Client::new(),
            Some(&format!("http://{addr}/refreshToken")),
            REGION,
            "old",
        )
        .await
        .unwrap();
        assert_eq!(tokens.access, "fresh");
        assert_eq!(tokens.refresh.as_deref(), Some("old"));
        task.abort();
    }
}
