//! API-key "login" providers.
//!
//! Not OAuth: the user is pointed at the provider dashboard and pastes the key,
//! which is stored as an API key credential (opencodex `oauth/key-providers.ts`).

use anyhow::{anyhow, Result};

use super::Credential;
use crate::login::cli::KeySource;
use crate::login::ui;

/// A pasted key with surrounding whitespace removed.
pub fn clean(typed: &str) -> Result<String> {
    let key = typed.trim();
    if key.is_empty() {
        return Err(anyhow!("no API key entered"));
    }
    Ok(key.to_string())
}

/// Where each provider's keys are created.
fn dashboard(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("https://console.anthropic.com/settings/keys"),
        "openai-api" => Some("https://platform.openai.com/api-keys"),
        "opencode" => Some("https://opencode.ai/auth"),
        _ => None,
    }
}

/// Read the key from the chosen source.
fn read(provider: &str, source: KeySource) -> Result<String> {
    match source {
        KeySource::Env(var) => {
            let value = std::env::var(&var).map_err(|_| anyhow!("{var} is not set"))?;
            clean(&value)
        }
        KeySource::Value(value) => {
            ui::warn("a key on the command line stays in shell history; prefer --key or --env");
            clean(&value)
        }
        KeySource::Prompt => {
            if let Some(url) = dashboard(provider) {
                ui::info(&format!("create a key at {url}"));
            }
            clean(&ui::prompt(&format!("Paste your {provider} API key:")))
        }
    }
}

/// A cheap authenticated request that tells a wrong key from a right one; `None` where the
/// provider has no such endpoint.
fn check_request(
    http: &reqwest::Client,
    provider: &str,
    key: &str,
) -> Option<reqwest::RequestBuilder> {
    match provider {
        "openai-api" => Some(
            http.get("https://api.openai.com/v1/models")
                .bearer_auth(key),
        ),
        "anthropic" => Some(
            http.get("https://api.anthropic.com/v1/models")
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01"),
        ),
        _ => None,
    }
}

/// Reject a key the provider says is invalid; other failures (network, outage) don't block it.
async fn verify(http: &reqwest::Client, provider: &str, key: &str) -> Result<()> {
    let Some(request) = check_request(http, provider, key) else {
        return Ok(());
    };
    match request.send().await.map(|r| r.status().as_u16()) {
        Ok(401 | 403) => Err(anyhow!("{provider} rejected this API key")),
        _ => Ok(()),
    }
}

/// Log in with an API key from a prompt or an environment variable.
pub async fn login(
    http: &reqwest::Client,
    provider: &str,
    source: KeySource,
) -> Result<Credential> {
    let name = provider.to_owned();
    let key = tokio::task::spawn_blocking(move || read(&name, source)).await??;
    verify(http, provider, &key).await?;
    Ok(Credential::ApiKey(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_trimmed_and_blank_input_is_rejected() {
        assert_eq!(clean("  sk-abc \n").unwrap(), "sk-abc");
        assert!(clean("   ").is_err());
        assert!(clean("").is_err());
    }

    #[test]
    fn env_source_reads_the_variable_and_names_a_missing_one() {
        std::env::set_var("GW_TEST_KEY_VAR", " sk-from-env ");
        assert_eq!(
            read("openai-api", KeySource::Env("GW_TEST_KEY_VAR".into())).unwrap(),
            "sk-from-env"
        );
        let err = read("openai-api", KeySource::Env("GW_TEST_KEY_UNSET".into())).unwrap_err();
        assert!(err.to_string().contains("GW_TEST_KEY_UNSET is not set"));
    }
}
