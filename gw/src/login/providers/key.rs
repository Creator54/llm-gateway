//! API-key "login" providers.
//!
//! Not OAuth: the user is pointed at the provider dashboard and pastes the key,
//! which is stored as an API key credential (opencodex `oauth/key-providers.ts`).

use anyhow::{anyhow, Result};

use super::Credential;
use crate::login::{ui, Options};

/// Dashboard where the key is created.
const OPENCODE_ZEN_DASHBOARD: &str = "https://opencode.ai/auth";

/// A pasted key with surrounding whitespace removed.
pub fn clean(typed: &str) -> Result<String> {
    let key = typed.trim();
    if key.is_empty() {
        return Err(anyhow!("no API key entered"));
    }
    Ok(key.to_string())
}

/// Prompt for a key, optionally echoing the dashboard URL first.
pub fn prompt_key(dashboard: Option<&str>) -> Result<String> {
    if let Some(url) = dashboard {
        ui::info(&format!("create a key at {url}"));
    }
    clean(&ui::prompt("Paste your opencode-zen API key:"))
}

/// Login by pasting the provider's API key.
pub async fn login(provider: &str, _opts: &Options) -> Result<Credential> {
    let dashboard = (provider == "opencode-zen").then_some(OPENCODE_ZEN_DASHBOARD);
    let key = tokio::task::spawn_blocking(move || prompt_key(dashboard)).await??;
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
}
