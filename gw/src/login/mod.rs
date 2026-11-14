//! Provider login flows: browser PKCE with a loopback callback, device codes, and
//! API-key prompts. Ported from opencodex `src/oauth/`.
//!
//! Tokens are never printed: progress messages name the account, never the secret.

pub mod callback;
pub mod device;
pub mod pkce;
pub mod providers;
pub mod refresh;

use anyhow::Result;
use std::path::PathBuf;

use crate::{creds, ui};

/// Endpoint and port overrides. Every field is optional and defaults to the
/// provider's real upstream; tests point flows at a local fake server.
#[derive(Clone, Debug, Default)]
pub struct Endpoints {
    pub auth_url: Option<String>,
    pub token_url: Option<String>,
    pub port: Option<u16>,
    pub device_url: Option<String>,
    pub device_token_url: Option<String>,
    /// Provider API base used for post-login lookups (antigravity, command-code).
    pub base_url: Option<String>,
    /// Onboarding endpoint base (antigravity `onboardUser` lives on another host).
    pub onboard_url: Option<String>,
}

/// How the user proves identity when a provider supports more than one way.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Flow {
    /// The provider's usual flow: browser, except where a callback is impossible.
    #[default]
    Auto,
    Browser,
    Device,
}

/// Login options.
#[derive(Clone, Debug)]
pub struct Options {
    /// Directory holding `credentials.json`.
    pub dir: PathBuf,
    pub endpoints: Endpoints,
    pub flow: Flow,
    /// Open the authorization URL in the desktop browser (failure is ignored).
    pub open_browser: bool,
    /// Race the stdin paste prompt against the loopback listener.
    pub allow_paste: bool,
    /// Label stored on the account; the account id is used when unset.
    pub label: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        let dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config/gw");
        Self {
            dir,
            endpoints: Endpoints::default(),
            flow: Flow::default(),
            open_browser: true,
            allow_paste: true,
            label: None,
        }
    }
}

impl Options {
    pub(crate) fn token_url(&self, default: &str) -> String {
        self.endpoints
            .token_url
            .clone()
            .unwrap_or_else(|| default.to_string())
    }

    pub(crate) fn port(&self, default: u16) -> u16 {
        self.endpoints.port.unwrap_or(default)
    }

    pub(crate) fn base_url(&self, default: &str) -> String {
        self.endpoints
            .base_url
            .clone()
            .unwrap_or_else(|| default.to_string())
    }

    pub(crate) fn onboard_url(&self, default: &str) -> String {
        self.endpoints
            .onboard_url
            .clone()
            .unwrap_or_else(|| default.to_string())
    }
}

/// A freshly obtained credential, before it becomes an entry.
#[derive(Clone, Debug)]
pub struct Tokens {
    pub access: String,
    pub refresh: Option<String>,
    /// Unix seconds.
    pub expires_at: Option<u64>,
    /// Provider account id, used as the entry id when present.
    pub account: Option<String>,
    pub email: Option<String>,
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Tokens {
    pub fn new(access: impl Into<String>) -> Self {
        Self {
            access: access.into(),
            refresh: None,
            expires_at: None,
            account: None,
            email: None,
            extra: serde_json::Map::new(),
        }
    }

    /// Attach `expires_at` = now + `expires_in` seconds, minus the given skew.
    pub fn expires_in(mut self, expires_in: Option<u64>, skew_secs: u64) -> Self {
        let seconds = expires_in.unwrap_or(3600);
        self.expires_at = Some(now_secs() + seconds.saturating_sub(skew_secs));
        self
    }

    pub fn with_refresh(mut self, refresh: Option<String>) -> Self {
        self.refresh = refresh.filter(|r| !r.is_empty());
        self
    }

    pub fn extra(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.extra.insert(key.to_string(), value.into());
        self
    }

    /// Stable entry id: the provider account id, else the email, else random hex.
    pub fn id(&self) -> String {
        self.account
            .clone()
            .or_else(|| self.email.clone())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                pkce::random_bytes(6)
                    .map(|b| b.iter().map(|v| format!("{v:02x}")).collect())
                    .unwrap_or_else(|_| "account".to_string())
            })
    }

    /// Build the stored entry, carrying routing metadata into `extra`.
    pub fn entry(self, provider: &str, label: Option<String>) -> creds::Entry {
        let id = self.id();
        let account = self.account.filter(|v| !v.is_empty());
        let email = self.email.filter(|v| !v.is_empty());
        let mut extra = self.extra;
        if let Some(account) = account {
            extra.insert("account_id".to_string(), account.into());
        }
        if let Some(email) = email {
            extra.insert("email".to_string(), email.into());
        }
        creds::Entry {
            provider: provider.to_string(),
            id,
            label,
            paused: false,
            needs_reauth: false,
            last_error: None,
            credential: creds::Secret::OAuth(creds::OAuthCred {
                access: self.access,
                refresh: self.refresh,
                expires_at: self.expires_at,
                extra,
            }),
        }
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// What a finished flow produced.
#[derive(Clone, Debug)]
pub enum Credential {
    OAuth(Tokens),
    ApiKey(String),
}

impl Credential {
    /// Build the entry to store.
    pub fn entry(self, provider: &str, label: Option<String>) -> creds::Entry {
        match self {
            Credential::OAuth(tokens) => tokens.entry(provider, label),
            Credential::ApiKey(key) => creds::Entry {
                provider: provider.to_string(),
                id: pkce::random_bytes(6)
                    .map(|b| b.iter().map(|v| format!("{v:02x}")).collect())
                    .unwrap_or_else(|_| "account".to_string()),
                label,
                paused: false,
                needs_reauth: false,
                last_error: None,
                credential: creds::Secret::ApiKey { key },
            },
        }
    }
}

/// Run the provider's login flow, store the account and return the new entry.
pub async fn login(provider: &str, opts: Options) -> Result<creds::Entry> {
    let http = reqwest::Client::new();
    let credential = providers::run(provider, &http, &opts).await?;
    let entry = credential.entry(provider, opts.label.clone());
    let store = creds::Store::load(&opts.dir)?;
    store.upsert(entry.clone());
    store.save()?;
    ui::ok(&format!("saved {provider} account {}", entry.id));
    Ok(entry)
}
