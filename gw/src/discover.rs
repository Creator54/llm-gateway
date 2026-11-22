//! Live model discovery: ask each provider what the logged-in account can use and
//! keep the answer in `<state_dir>/models.json`, which the gateway merges into `/v1/models`.
//!
//! Only providers with a real listing endpoint are covered (anthropic, antigravity, gemini-api);
//! the rest keep their `[models]` entries from the config.

use crate::config::Config;
use crate::creds::{self, Entry};
use crate::providers::{anthropic, antigravity, command_code, gemini_api, openai_api};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const FILE: &str = "models.json";

/// Provider id → account name → model ids, as saved by the last refresh.
pub type Accounts = BTreeMap<String, BTreeMap<String, Vec<String>>>;

/// Models saved by the last refresh; empty when there is none or it is unreadable.
pub fn load_accounts(state_dir: &Path) -> Accounts {
    std::fs::read(state_dir.join(FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Provider id → every model any of its accounts can use.
pub fn load(state_dir: &Path) -> BTreeMap<String, Vec<String>> {
    load_accounts(state_dir)
        .into_iter()
        .map(|(provider, accounts)| {
            let all: BTreeSet<String> = accounts.into_values().flatten().collect();
            (provider, all.into_iter().collect())
        })
        .collect()
}

/// Query every account of every supported provider and save the result. An account that
/// fails keeps its previously saved list. Each provider reports its model count, or the
/// first account error when none succeeded.
pub async fn refresh(cfg: &Config) -> anyhow::Result<Vec<(String, anyhow::Result<usize>)>> {
    let store = creds::Store::load(&cfg.state_dir)?;
    let client = reqwest::Client::new();
    let mut saved = load_accounts(&cfg.state_dir);
    let mut report = Vec::new();
    for provider in [
        "anthropic",
        "antigravity",
        "command-code",
        "gemini-api",
        "openai-api",
    ] {

        let accounts = store.accounts(provider);
        let mut first_error = None;
        let mut succeeded = false;
        for entry in accounts
            .into_iter()
            .filter(|e| !e.paused && !e.needs_reauth)
        {
            let name = entry.label.clone().unwrap_or_else(|| entry.id.clone());
            match discover(cfg, &store, &client, provider, entry).await {
                Ok(ids) => {
                    succeeded = true;
                    saved
                        .entry(provider.to_owned())
                        .or_default()
                        .insert(name, ids);
                }
                Err(e) => {
                    first_error.get_or_insert_with(|| e.context(format!("account {name}")));
                }
            }
        }
        let count = load_union(&saved, provider);
        report.push((
            provider.to_owned(),
            match (succeeded, first_error) {
                (false, Some(e)) => Err(e),
                (false, None) => Err(anyhow::anyhow!(
                    "no usable account; run `gw login {provider}`"
                )),
                _ => Ok(count),
            },
        ));
    }
    std::fs::create_dir_all(&cfg.state_dir)?;
    std::fs::write(cfg.state_dir.join(FILE), serde_json::to_vec_pretty(&saved)?)?;
    Ok(report)
}

fn load_union(saved: &Accounts, provider: &str) -> usize {
    saved
        .get(provider)
        .into_iter()
        .flat_map(|accounts| accounts.values().flatten())
        .collect::<BTreeSet<_>>()
        .len()
}

/// The account with its access token refreshed if it is due.
pub(crate) async fn fresh(
    store: &creds::Store,
    client: &reqwest::Client,
    provider: &str,
    entry: Entry,
) -> anyhow::Result<Entry> {
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
    Ok(store.get(provider, &entry.id).unwrap_or(entry))
}

async fn discover(
    cfg: &Config,
    store: &creds::Store,
    client: &reqwest::Client,
    provider: &str,
    entry: Entry,
) -> anyhow::Result<Vec<String>> {
    let entry = fresh(store, client, provider, entry).await?;
    let token = access_token(&entry)?;
    let base = cfg.endpoints.get(provider).map(|b| b.trim_end_matches('/'));
    let body = match provider {
        "gemini-api" => {
            let base = base.unwrap_or(gemini_api::DEFAULT_BASE_URL);
            let response = client
                .get(format!("{base}/models"))
                .header("x-goog-api-key", &token)
                .send()
                .await?;
            json_body(response).await?
        }
        "anthropic" => {
            let base = base.unwrap_or(anthropic::DEFAULT_BASE);
            get(client, &format!("{base}/v1/models?limit=1000"), &token).await?
        }
        "openai-api" | "command-code" => {
            let url = match provider {
                "command-code" => format!(
                    "{}/provider/v1/models",
                    base.unwrap_or(command_code::DEFAULT_BASE_URL)
                ),
                _ => format!("{}/models", base.unwrap_or(openai_api::DEFAULT_BASE_URL)),
            };
            let response = client.get(url).bearer_auth(&token).send().await?;
            json_body(response).await?
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

pub(crate) fn access_token(entry: &Entry) -> anyhow::Result<String> {
    match crate::provider::Account::from(entry).credential {
        crate::provider::Credential::OAuth { access, .. } => Ok(access),
        crate::provider::Credential::ApiKey(key) => Ok(key),
        crate::provider::Credential::None => anyhow::bail!("account has no credential"),
    }
}

pub(crate) fn account_extra(entry: &Entry, key: &str) -> anyhow::Result<String> {
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
        // `chat_*` and `tab_*` are the editor's internal models, not something to route to.
        return body["models"]
            .as_object()
            .map(|m| {
                m.keys()
                    .filter(|id| !id.starts_with("chat_") && !id.starts_with("tab_"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
    }
    if provider == "gemini-api" {
        // The listing nests under `models` with `name` values like `models/gemini-2.0-flash`.
        return body["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["name"].as_str())
            .map(|name| name.trim_start_matches("models/").to_owned())
            .collect();
    }
    body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str())
        .filter(|id| provider != "openai-api" || is_chat_model(id))
        .map(str::to_owned)
        .collect()
}

/// OpenAI's listing also holds embeddings, speech, images and moderation, none of which a
/// chat client can route to.
fn is_chat_model(id: &str) -> bool {
    const SKIPPED: [&str; 7] = [
        "realtime",
        "audio",
        "transcribe",
        "tts",
        "image",
        "embedding",
        "moderation",
    ];
    let family = id.starts_with("gpt-")
        || id.starts_with("chatgpt-")
        || id.starts_with("codex-")
        || id
            .strip_prefix('o')
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
    family && !SKIPPED.iter().any(|word| id.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_listing_shapes() {
        let openai_style = json!({ "data": [{ "id": "claude-a" }, { "id": "claude-b" }] });
        assert_eq!(ids("anthropic", &openai_style), ["claude-a", "claude-b"]);
        let cca =
            json!({ "models": { "gemini-x": {}, "claude-y": {}, "chat_1": {}, "tab_flash": {} } });
        let mut got = ids("antigravity", &cca);
        got.sort();
        assert_eq!(got, ["claude-y", "gemini-x"]);
    }

    #[test]
    fn openai_listing_keeps_only_chat_models() {
        let body = json!({ "data": [
            { "id": "gpt-5.5" }, { "id": "o3-mini" }, { "id": "chatgpt-4o-latest" },
            { "id": "text-embedding-3-small" }, { "id": "whisper-1" }, { "id": "dall-e-3" },
            { "id": "gpt-4o-realtime-preview" }, { "id": "gpt-image-1" }, { "id": "omni-moderation-latest" },
        ] });
        assert_eq!(
            ids("openai-api", &body),
            ["gpt-5.5", "o3-mini", "chatgpt-4o-latest"]
        );
    }

    #[test]
    fn saved_models_round_trip_and_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("gw-discover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load(&dir).is_empty());
        std::fs::write(dir.join(FILE), r#"{"anthropic":{"a":["m"],"b":["m","n"]}}"#).unwrap();
        assert_eq!(load(&dir)["anthropic"], ["m", "n"]);
        assert_eq!(load_accounts(&dir)["anthropic"]["a"], ["m"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
