//! Device-code grant polling (RFC 8628 plus the OpenAI deviceauth variant).
//!
//! Two pending signals are in use upstream: a bare 403/404 (OpenAI deviceauth) and a
//! body carrying `authorization_pending` (AWS SSO OIDC). Both are recognised here, and
//! neither is treated as a terminal failure.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// The grant's own lifetime; polling past it only produces a worse error.
pub const TTL: Duration = Duration::from_secs(15 * 60);
/// Per-request deadline, fresh for each attempt so a stuck socket cannot eat the grant.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_POLL: Duration = Duration::from_secs(5);
const MIN_POLL: Duration = Duration::from_secs(1);
/// What one `slow_down` adds (RFC 8628 §3.5).
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

/// The human code plus the opaque id the poll endpoint needs.
#[derive(Clone, Debug)]
pub struct DeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: Duration,
}

/// One poll outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum Poll {
    Pending,
    SlowDown,
    Expired,
    Ready(Value),
    Failed(u16),
}

fn text<'a>(body: &'a Value, key: &str) -> &'a str {
    body.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn non_empty(body: &Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Coerce the server `interval` (seconds as number or string) into a bounded duration.
pub fn normalize_interval(raw: Option<&Value>) -> Duration {
    let secs = match raw {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|s| s.is_finite() && *s > 0.0);
    match secs {
        Some(s) => Duration::from_millis((s * 1000.0).round() as u64).clamp(MIN_POLL, TTL),
        None => DEFAULT_POLL,
    }
}

/// Classify a poll response.
pub fn classify(status: u16, body: &Value) -> Poll {
    // OpenAI deviceauth answers "keep waiting" with 403/404 and no body.
    if status == 403 || status == 404 {
        return Poll::Pending;
    }
    let error = text(body, "error");
    let state = text(body, "status");
    if matches!(
        error,
        "authorization_pending" | "AuthorizationPendingException"
    ) || state == "authorization_pending"
    {
        return Poll::Pending;
    }
    if matches!(error, "slow_down" | "SlowDownException") {
        return Poll::SlowDown;
    }
    if matches!(error, "expired_token" | "ExpiredTokenException")
        || matches!(state, "expired_token" | "expired")
    {
        return Poll::Expired;
    }
    if (200..300).contains(&status) {
        Poll::Ready(body.clone())
    } else {
        Poll::Failed(status)
    }
}

/// Authenticated JSON POST with a per-request timeout.
pub async fn post(
    http: &reqwest::Client,
    url: &str,
    body: &Value,
) -> Result<(u16, Value)> {
    let response = http
        .post(url)
        .timeout(FETCH_TIMEOUT)
        .body(serde_json::to_vec(body)?)
        .send()
        .await?;
    let status = response.status().as_u16();
    Ok((status, crate::login::providers::json_body(response).await))
}

/// One poll attempt against `url`.
pub async fn poll_once(http: &reqwest::Client, url: &str, body: &Value) -> Result<Poll> {
    let (status, payload) = post(http, url, body).await?;
    Ok(classify(status, &payload))
}

/// Ask for a user code. The `/usercode` alias is accepted, as upstream sends both spellings.
pub async fn request_user_code(
    http: &reqwest::Client,
    url: &str,
    client_id: &str,
) -> Result<DeviceCode> {
    let (status, body) = post(http, url, &json!({ "client_id": client_id })).await?;
    if !(200..300).contains(&status) {
        return Err(anyhow!("device authorization request failed: HTTP {status}"));
    }
    let device_auth_id = non_empty(&body, "device_auth_id")
        .ok_or_else(|| anyhow!("device authorization response missing device_auth_id"))?;
    let user_code = non_empty(&body, "user_code")
        .or_else(|| non_empty(&body, "usercode"))
        .ok_or_else(|| anyhow!("device authorization response missing user_code"))?;
    Ok(DeviceCode {
        device_auth_id,
        user_code,
        interval: normalize_interval(body.get("interval")),
    })
}

/// Poll until a terminal outcome, capping each sleep at the time actually left.
pub async fn poll_with<F, Fut>(
    mut fetch: F,
    deadline: Instant,
    mut interval: Duration,
    slow_down_step: Duration,
) -> Result<Value>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Poll>>,
{
    loop {
        if Instant::now() >= deadline {
            return Err(anyhow!("device authorization expired"));
        }
        match fetch().await? {
            Poll::Ready(value) => return Ok(value),
            Poll::Expired => return Err(anyhow!("device authorization expired")),
            Poll::Failed(status) => {
                return Err(anyhow!("device authorization poll failed: HTTP {status}"))
            }
            Poll::SlowDown => interval += slow_down_step,
            Poll::Pending => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(anyhow!("device authorization expired"));
        }
        tokio::time::sleep(interval.min(left)).await;
    }
}

/// Poll `url` with `body` until it yields a grant, using the standard slow-down step.
pub async fn poll(
    http: &reqwest::Client,
    url: &str,
    body: &Value,
    interval: Duration,
) -> Result<Value> {
    let deadline = Instant::now() + TTL;
    poll_with(
        || poll_once(http, url, body),
        deadline,
        interval,
        SLOW_DOWN_STEP,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn interval_is_coerced_and_bounded() {
        assert_eq!(normalize_interval(Some(&json!(2))), Duration::from_secs(2));
        assert_eq!(normalize_interval(Some(&json!("3"))), Duration::from_secs(3));
        // Zero, negative, non-numeric and absurd values fall back or clamp.
        assert_eq!(normalize_interval(Some(&json!(0))), DEFAULT_POLL);
        assert_eq!(normalize_interval(Some(&json!(-1))), DEFAULT_POLL);
        assert_eq!(normalize_interval(Some(&json!("x"))), DEFAULT_POLL);
        assert_eq!(normalize_interval(None), DEFAULT_POLL);
        assert_eq!(normalize_interval(Some(&json!(0.01))), MIN_POLL);
        assert_eq!(normalize_interval(Some(&json!(1e9))), TTL);
    }

    #[test]
    fn pending_and_terminal_states_are_recognised() {
        assert_eq!(classify(403, &Value::Null), Poll::Pending);
        assert_eq!(classify(404, &Value::Null), Poll::Pending);
        assert_eq!(
            classify(400, &json!({"error":"authorization_pending"})),
            Poll::Pending
        );
        assert_eq!(
            classify(200, &json!({"status":"authorization_pending"})),
            Poll::Pending
        );
        assert_eq!(
            classify(400, &json!({"error":"SlowDownException"})),
            Poll::SlowDown
        );
        assert_eq!(
            classify(400, &json!({"error":"expired_token"})),
            Poll::Expired
        );
        assert_eq!(classify(200, &json!({"status":"expired"})), Poll::Expired);
        assert_eq!(classify(500, &json!({"oops":true})), Poll::Failed(500));
        assert_eq!(
            classify(200, &json!({"authorization_code":"c"})),
            Poll::Ready(json!({"authorization_code":"c"}))
        );
    }

    #[tokio::test]
    async fn pending_then_slow_down_then_success() {
        let mut seen = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        let value = tokio::time::timeout(
            Duration::from_secs(2),
            poll_with(
                || {
                    seen += 1;
                    let polls = [
                        Poll::Pending,
                        Poll::SlowDown,
                        Poll::Ready(json!({"authorization_code":"c","code_verifier":"v"})),
                    ];
                    let poll = polls[seen.min(polls.len()) - 1].clone();
                    async move { Ok(poll) }
                },
                deadline,
                ms(1),
                ms(1),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(value["code_verifier"], "v");
        assert_eq!(seen, 3);
    }

    #[tokio::test]
    async fn failures_and_expiry_end_the_poll() {
        let deadline = Instant::now() + Duration::from_secs(5);
        let failed = poll_with(
            || async { Ok(Poll::Failed(418)) },
            deadline,
            ms(1),
            ms(1),
        )
        .await
        .unwrap_err();
        assert!(failed.to_string().contains("418"));

        let expired = poll_with(|| async { Ok(Poll::Pending) }, Instant::now(), ms(1), ms(1))
            .await
            .unwrap_err();
        assert!(expired.to_string().contains("expired"));
    }
}
