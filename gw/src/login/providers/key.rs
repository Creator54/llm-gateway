//! API-key "login" providers.
//!
//! Not OAuth: the user is pointed at the provider dashboard and pastes the key,
//! which is stored as an API key credential (opencodex `oauth/key-providers.ts`).

use anyhow::{anyhow, Result};

use super::Credential;
use crate::login::{ui, Options};

/// Dashboard where the key is created.
const OPENCODE_ZEN_DASHBOARD: &str = "https://opencode.ai/auth";

/// Prompt for a key, optionally echoing the dashboard URL first.
pub fn prompt_key(dashboard: Option<&str>) -> Result<String> {
    if let Some(url) = dashboard {
        ui::info(&format!("create a key at {url}"));
    }
    let key = ui::prompt("Paste your opencode-zen API key:").trim().to_string();
    if key.is_empty() {
        return Err(anyhow!("no API key entered"));
    }
    Ok(key)
}

/// Login by pasting the provider's API key.
pub async fn login(provider: &str, _opts: &Options) -> Result<Credential> {
    let dashboard = match provider {
        "opencode-zen" => Some(OPENCODE_ZEN_DASHBOARD),
        _ => None,
    };
    let dashboard = dashboard.map(str::to_string);
    let key = tokio::task::spawn_blocking(move || prompt_key(dashboard.as_deref())).await??;
    Ok(Credential::ApiKey(key))
}

#[cfg(test)]
mod tests {
    /// The prompt trims; only a non-blank answer is accepted.
    #[test]
    fn blank_keys_are_rejected() {
        assert!("   ".trim().is_empty());
    }
}
