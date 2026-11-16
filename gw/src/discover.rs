//! Live model discovery: ask each provider what the logged-in account can use and
//! keep the answer in `<state_dir>/models.json`, which the gateway merges into `/v1/models`.
//!
//! Only providers with a real listing endpoint are covered (anthropic, antigravity);
//! the rest keep their `[models]` entries from the config.

use crate::config::Config;
use crate::creds::{self, Entry};
use crate::providers::{anthropic, antigravity};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

const FILE: &str = "models.json";

/// Provider id → model ids, as saved by the last refresh.
pub type Discovered = BTreeMap<String, Vec<String>>;

/// Models saved by the last refresh; empty when there is none or it is unreadable.
pub fn load(state_dir: &Path) -> Discovered {
    std::fs::read(state_dir.join(FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Query every supported provider with a usable account and save the result.
/// A provider that fails keeps its previously saved list; the error is returned per provider.
pub async fn refresh(cfg: &Config) -> anyhow::Result<Vec<(String, anyhow::Result<usize>)>> {
    let store = creds::Store::load(&cfg.state_dir)?;
    let client = reqwest::Client::new();
    let mut saved = load(&cfg.state_dir);
    let mut report = Vec::new();
    for provider in ["anthropic", "antigravity"] {
        let result = discover(cfg, &store, &client, provider).await;
        report.push((
            provider.to_owned(),
            result.map(|ids| {
                let count = ids.len();
                saved.insert(provider.to_owned(), ids);
                count
            }),
        ));
    }
    std::fs::create_dir_all(&cfg.state_dir)?;
    std::fs::write(cfg.state_dir.join(FILE), serde_json::to_vec_pretty(&saved)?)?;
    Ok(report)
}

async fn discover(
    cfg: &Config,
    store: &creds::Store,
    client: &reqwest::Client,
    provider: &str,
) -> anyhow::Result<Vec<String>> {
    let entry = store
        .accounts(provider)
        .into_iter()
        .find(|e| !e.paused && !e.needs_reauth)
        .ok_or_else(|| anyhow::anyhow!("no usable account; run `gw login {provider}`"))?;
    let refresher = crate::login::refresh::TokenRefresher {
        provider: provider.to_owned(),
        http: client.clone(),
        endpoints: Default::default(),
    };
    store
        .refresh_if_due(
            &refresher,
            provider,
            &entry.id,
            crate::login::now_secs(),
            creds::DEFAULT_SKEW_SECS,
        )
        .await?;
    let entry = store.get(provider, &entry.id).unwrap_or(entry);
    let token = access_token(&entry)?;
    let base = cfg.endpoints.get(provider).map(|b| b.trim_end_matches('/'));
    let body = match provider {
        "anthropic" => {
            let base = base.unwrap_or(anthropic::DEFAULT_BASE);
            get(client, &format!("{base}/v1/models?limit=1000"), &token).await?
        }
        _ => {
            let base = base.unwrap_or(antigravity::DEFAULT_BASE_URL);
            let project = account_extra(&entry, "project_id")?;
            post(
                client,
                &format!("{base}/v1internal:fetchAvailableModels"),
                &token,
                &project,
            )
            .await?
        }
    };
    let ids = ids(provider, &body);
    anyhow::ensure!(!ids.is_empty(), "the provider returned no models");
    Ok(ids)
}

fn access_token(entry: &Entry) -> anyhow::Result<String> {
    match crate::provider::Account::from(entry).credential {
        crate::provider::Credential::OAuth { access, .. } => Ok(access),
        crate::provider::Credential::ApiKey(key) => Ok(key),
        crate::provider::Credential::None => anyhow::bail!("account has no credential"),
    }
}

fn account_extra(entry: &Entry, key: &str) -> anyhow::Result<String> {
    crate::provider::Account::from(entry)
        .extra
        .get(key)
        .filter(|v| !v.is_empty())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("account has no {key}"))
}

async fn get(client: &reqwest::Client, url: &str, token: &str) -> anyhow::Result<Value> {
    let response = client
        .get(url)
        .header("anthropic-version", anthropic::API_VERSION)
        .header("anthropic-beta", anthropic::OAUTH_BETA)
        .bearer_auth(token)
        .send()
        .await?;
    json_body(response).await
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    project: &str,
) -> anyhow::Result<Value> {
    let response = client
        .post(url)
        .header("user-agent", antigravity::USER_AGENT)
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body(json!({ "project": project }).to_string())
        .send()
        .await?;
    json_body(response).await
}

async fn json_body(response: reqwest::Response) -> anyhow::Result<Value> {
    let status = response.status();
    let bytes = response.bytes().await?;
    anyhow::ensure!(status.is_success(), "HTTP {status}");
    Ok(serde_json::from_slice(&bytes)?)
}

/// Model ids from a provider's listing: `{data:[{id}]}` or antigravity's `{models:{id:{…}}}`.
fn ids(provider: &str, body: &Value) -> Vec<String> {
    if provider == "antigravity" {
        return body["models"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
    }
    body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_listing_shapes() {
        let openai_style = json!({ "data": [{ "id": "claude-a" }, { "id": "claude-b" }] });
        assert_eq!(ids("anthropic", &openai_style), ["claude-a", "claude-b"]);
        let cca = json!({ "models": { "gemini-x": {}, "claude-y": {} } });
        let mut got = ids("antigravity", &cca);
        got.sort();
        assert_eq!(got, ["claude-y", "gemini-x"]);
    }

    #[test]
    fn saved_models_round_trip_and_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("gw-discover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load(&dir).is_empty());
        std::fs::write(dir.join(FILE), r#"{"anthropic":["m"]}"#).unwrap();
        assert_eq!(load(&dir)["anthropic"], ["m"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
