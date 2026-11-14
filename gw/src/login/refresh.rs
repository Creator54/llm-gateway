//! Token refresh for the OAuth login providers.
//!
//! Implements [`creds::Refresher`] so `Store::refresh_if_due` can keep accounts alive
//! without knowing which provider a credential came from.

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use serde_json::Map;

use crate::creds::{OAuthCred, Refresher};
use crate::login::{providers, Endpoints, Tokens};

/// Refreshes whatever provider a credential belongs to.
pub struct TokenRefresher {
    pub provider: String,
    pub http: reqwest::Client,
    pub endpoints: Endpoints,
}

impl TokenRefresher {
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            http: reqwest::Client::new(),
            endpoints: Endpoints::default(),
        }
    }

    /// Endpoint overrides for tests.
    pub fn with_endpoints(mut self, endpoints: Endpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    async fn refresh_tokens(&self, cred: &OAuthCred) -> Result<Tokens> {
        let refresh = cred.refresh.as_deref().unwrap_or_default();
        if refresh.is_empty() {
            return Err(anyhow!(
                "{} has no refresh token; log in again",
                self.provider
            ));
        }
        let http = &self.http;
        match self.provider.as_str() {
            "anthropic" | "claude" => {
                let url = self
                    .endpoints
                    .token_url
                    .clone()
                    .unwrap_or_else(|| providers::anthropic::TOKEN_URL.to_string());
                providers::anthropic::refresh(http, &url, refresh).await
            }
            "openai" | "chatgpt" | "codex" => {
                let url = self
                    .endpoints
                    .token_url
                    .clone()
                    .unwrap_or_else(|| providers::openai::TOKEN_URL.to_string());
                providers::openai::refresh(http, &url, refresh).await
            }
            "google-antigravity" | "antigravity" => {
                let url = self
                    .endpoints
                    .token_url
                    .clone()
                    .unwrap_or_else(|| providers::antigravity::TOKEN_URL.to_string());
                let api = self
                    .endpoints
                    .base_url
                    .clone()
                    .unwrap_or_else(|| providers::antigravity::PROD_API.to_string());
                let onboard = self
                    .endpoints
                    .onboard_url
                    .clone()
                    .unwrap_or_else(|| providers::antigravity::DAILY_API.to_string());
                providers::antigravity::refresh(http, &url, &api, &onboard, refresh).await
            }
            "kiro" => {
                let region = cred
                    .extra
                    .get("sso_region")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("us-east-1");
                providers::kiro::refresh(http, self.endpoints.token_url.as_deref(), region, refresh)
                    .await
            }
            "command-code" | "commandcode" => {
                // The API key is its own refresh token and does not expire.
                Ok(Tokens::new(refresh).with_refresh(Some(refresh.to_string())))
            }
            other => Err(anyhow!("no token refresh for provider {other}")),
        }
    }
}

/// Keep routing metadata the provider did not resend.
fn merged(previous: &Map<String, serde_json::Value>, fresh: Tokens) -> OAuthCred {
    let mut extra = previous.clone();
    for (key, value) in fresh.extra {
        extra.insert(key, value);
    }
    OAuthCred {
        access: fresh.access,
        refresh: fresh.refresh,
        expires_at: fresh.expires_at,
        extra,
    }
}

impl Refresher for TokenRefresher {
    fn refresh<'a>(&'a self, cred: &'a OAuthCred) -> BoxFuture<'a, anyhow::Result<OAuthCred>> {
        Box::pin(async move {
            let fresh = self.refresh_tokens(cred).await?;
            Ok(merged(&cred.extra, fresh))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::login::now_secs;
    use axum::routing::post;
    use axum::Router;
    use serde_json::json;

    fn cred(refresh: &str) -> OAuthCred {
        let mut extra = Map::new();
        extra.insert("account_id".into(), json!("acct-1"));
        OAuthCred {
            access: "stale".into(),
            refresh: Some(refresh.into()),
            expires_at: Some(now_secs() - 10),
            extra,
        }
    }

    async fn serve(path: &str, body: serde_json::Value) -> String {
        let app = Router::new().route(
            path,
            post(move || {
                let body = body.clone();
                async move { axum::Json(body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}{path}")
    }

    #[tokio::test]
    async fn refresh_updates_the_expiry_and_keeps_routing_metadata() {
        let url = serve(
            "/token",
            json!({"access_token": "fresh", "expires_in": 3600}),
        )
        .await;
        let refresher = TokenRefresher::new("anthropic").with_endpoints(Endpoints {
            token_url: Some(url),
            ..Endpoints::default()
        });
        let old = cred("old-refresh");
        let before = old.expires_at.unwrap();
        let fresh = refresher.refresh(&old).await.unwrap();
        assert_eq!(fresh.access, "fresh");
        assert_eq!(fresh.refresh.as_deref(), Some("old-refresh"));
        assert!(fresh.expires_at.unwrap() > before + 400);
        assert_eq!(fresh.extra.get("account_id").unwrap(), "acct-1");
    }

    #[tokio::test]
    async fn a_missing_refresh_token_fails_closed() {
        let refresher = TokenRefresher::new("openai");
        let mut broken = cred("x");
        broken.refresh = None;
        assert!(refresher.refresh(&broken).await.is_err());
    }

    #[tokio::test]
    async fn command_code_keys_never_expire() {
        let refresher = TokenRefresher::new("command-code");
        let tokens = refresher.refresh_tokens(&cred("k")).await.unwrap();
        assert_eq!(tokens.access, "k");
        assert_eq!(tokens.expires_at, None);
    }
}
