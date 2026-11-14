//! Account credentials on disk (0600) and token refresh.

use crate::provider::{Account, Credential};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as AsyncMutex;

/// On-disk format version; anything else is refused rather than guessed at.
pub const VERSION: u32 = 1;
/// Refresh this long before a token expires, so a request never uses a stale one.
pub const DEFAULT_SKEW_SECS: u64 = 300;

const FILE: &str = "credentials.json";
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// One refresh lock per `(provider, id)`, created on first use.
type LockMap = StdMutex<HashMap<(String, String), Arc<AsyncMutex<()>>>>;

/// OAuth token set; `extra` carries provider routing data such as `account_id`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthCred {
    pub access: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl fmt::Debug for OAuthCred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthCred")
            .field("access", &"<redacted>")
            .field(
                "refresh",
                &if self.refresh.is_some() {
                    "set"
                } else {
                    "none"
                },
            )
            .field("expires_at", &self.expires_at)
            .field("extra", &self.extra)
            .finish()
    }
}

/// How one account authenticates.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Secret {
    ApiKey { key: String },
    OAuth(OAuthCred),
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Secret::ApiKey { .. } => f.write_str("ApiKey(<redacted>)"),
            Secret::OAuth(c) => fmt::Debug::fmt(c, f),
        }
    }
}

impl Secret {
    /// Convert to the credential form adapters send upstream.
    pub fn to_credential(&self) -> Credential {
        match self {
            Secret::ApiKey { key } => Credential::ApiKey(key.clone()),
            Secret::OAuth(c) => Credential::OAuth {
                access: c.access.clone(),
                refresh: c.refresh.clone(),
                expires_at: c.expires_at,
            },
        }
    }
}

/// One stored account.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub provider: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub paused: bool,
    pub credential: Secret,
}

impl Entry {
    /// True when this token is expired or expires within `skew_secs`; unknown expiry counts as fresh.
    pub fn needs_refresh(&self, now_unix: u64, skew_secs: u64) -> bool {
        needs_refresh(self, now_unix, skew_secs)
    }
}

impl From<&Entry> for Account {
    fn from(entry: &Entry) -> Self {
        Account {
            id: entry.id.clone(),
            credential: entry.credential.to_credential(),
        }
    }
}

/// Refreshes one OAuth credential. Provider-specific HTTP lives behind this trait.
pub trait Refresher: Send + Sync {
    fn refresh<'a>(&'a self, cred: &'a OAuthCred) -> BoxFuture<'a, anyhow::Result<OAuthCred>>;
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    accounts: Vec<Entry>,
}

/// Credentials held in memory; every mutation is kept until `save`.
pub struct Store {
    dir: PathBuf,
    accounts: Vec<Entry>,
    /// One lock per account so concurrent requests cannot refresh the same token twice.
    locks: LockMap,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("dir", &self.dir)
            .field("accounts", &self.accounts)
            .finish()
    }
}

impl Store {
    /// Load `<dir>/credentials.json`; a missing file means no accounts. World/group readable is an error.
    pub fn load(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let path = dir.join(FILE);
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    dir,
                    accounts: Vec::new(),
                    locks: StdMutex::default(),
                })
            }
            Err(e) => return Err(anyhow::anyhow!("{}: {e}", path.display())),
        };
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "{} is accessible by other users (mode {mode:o}); run `chmod 600 {}`",
                path.display(),
                path.display()
            );
        }
        let text =
            fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let file: File =
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        if file.version != VERSION {
            anyhow::bail!(
                "{}: unsupported version {} (expected {VERSION})",
                path.display(),
                file.version
            );
        }
        for entry in &file.accounts {
            if entry.provider.is_empty() || entry.id.is_empty() {
                anyhow::bail!("{}: account with empty provider or id", path.display());
            }
        }
        Ok(Self {
            dir,
            accounts: file.accounts,
            locks: StdMutex::default(),
        })
    }

    /// All accounts of one provider, paused ones included; the pool decides eligibility.
    pub fn accounts(&self, provider: &str) -> Vec<&Entry> {
        self.accounts
            .iter()
            .filter(|e| e.provider == provider)
            .collect()
    }

    /// Every account, in file order.
    pub fn all(&self) -> &[Entry] {
        &self.accounts
    }

    /// One account by provider and id.
    pub fn get(&self, provider: &str, id: &str) -> Option<&Entry> {
        self.accounts
            .iter()
            .find(|e| e.provider == provider && e.id == id)
    }

    /// Insert or replace by `(provider, id)`.
    pub fn upsert(&mut self, entry: Entry) {
        match self
            .accounts
            .iter_mut()
            .find(|e| e.provider == entry.provider && e.id == entry.id)
        {
            Some(slot) => *slot = entry,
            None => self.accounts.push(entry),
        }
    }

    /// Drop one account; true when it existed.
    pub fn remove(&mut self, provider: &str, id: &str) -> bool {
        let before = self.accounts.len();
        self.accounts
            .retain(|e| !(e.provider == provider && e.id == id));
        self.accounts.len() != before
    }

    /// Pause or resume one account; true when it existed.
    pub fn set_paused(&mut self, provider: &str, id: &str, paused: bool) -> bool {
        match self
            .accounts
            .iter_mut()
            .find(|e| e.provider == provider && e.id == id)
        {
            Some(entry) => {
                entry.paused = paused;
                true
            }
            None => false,
        }
    }

    /// The lock guarding a refresh of one account, created on first use.
    pub fn refresh_lock(&self, provider: &str, id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock().expect("refresh lock map poisoned");
        locks
            .entry((provider.to_string(), id.to_string()))
            .or_default()
            .clone()
    }

    /// Refresh an expiring account under its lock and persist the result.
    ///
    /// Returns `None` when the account is absent or still fresh; rechecked after taking the
    /// lock so a task that waited behind another refresh does no second HTTP call.
    pub async fn refresh_if_due(
        &mut self,
        refresher: &dyn Refresher,
        provider: &str,
        id: &str,
        now_unix: u64,
        skew_secs: u64,
    ) -> anyhow::Result<Option<Entry>> {
        let lock = self.refresh_lock(provider, id);
        let _held = lock.lock().await;
        let Some(mut entry) = self.get(provider, id).cloned() else {
            return Ok(None);
        };
        if !needs_refresh(&entry, now_unix, skew_secs) {
            return Ok(None);
        }
        let fresh = match &entry.credential {
            Secret::OAuth(cred) => refresher.refresh(cred).await?,
            Secret::ApiKey { .. } => return Ok(None),
        };
        entry.credential = Secret::OAuth(fresh);
        self.upsert(entry.clone());
        self.save()?;
        Ok(Some(entry))
    }

    /// Write the whole store atomically at mode 0600.
    pub fn save(&self) -> anyhow::Result<()> {
        let file = File {
            version: VERSION,
            accounts: self.accounts.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file)?;
        bytes.push(b'\n');
        write_private(&self.dir.join(FILE), &bytes)
    }
}

/// True when the credential is OAuth, has a refresh token, and expires within `skew_secs`.
///
/// An unset `expires_at` is treated as fresh: with nothing to compare against, refreshing
/// on every check would loop.
pub fn needs_refresh(entry: &Entry, now_unix: u64, skew_secs: u64) -> bool {
    match &entry.credential {
        Secret::OAuth(c) => match c.expires_at {
            Some(expires_at) => {
                c.refresh.is_some() && expires_at <= now_unix.saturating_add(skew_secs)
            }
            None => false,
        },
        Secret::ApiKey { .. } => false,
    }
}

/// Write `bytes` to `path` via a temp file in the same directory, fsync, then rename.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let write = || -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow::anyhow!("{}: {e}", tmp.display()));
    }
    // The temp file is created 0600, so the rename publishes a private file.
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow::anyhow!("{}: {e}", path.display()));
    }
    // Persist the rename itself; otherwise a crash can resurface the old file.
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Read opencodex's `auth.json` and `codex-accounts.json` as gw entries.
///
/// Only non-secret routing fields are carried into `extra`; provider client secrets are
/// dropped. Accounts must be re-logged-in when a provider needs those secrets to refresh.
pub fn import_opencodex(dir: &Path) -> anyhow::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    if let Some(auth) = read_json(&dir.join("auth.json"))? {
        entries.extend(entries_from_auth(&auth));
    }
    if let Some(codex) = read_json(&dir.join("codex-accounts.json"))? {
        entries.extend(entries_from_codex(&codex));
    }
    Ok(entries)
}

fn read_json(path: &Path) -> anyhow::Result<Option<serde_json::Value>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?,
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
    }
}

/// `auth.json` maps provider -> `{activeAccountId, accounts: [...]}` or one legacy credential.
fn entries_from_auth(auth: &serde_json::Value) -> Vec<Entry> {
    let Some(providers) = auth.as_object() else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for (provider, value) in providers {
        let Some(cred) = oauth_from(value) else {
            // Multi-account sets look like `{activeAccountId, accounts: [...]}`.
            let Some(accounts) = value.get("accounts").and_then(serde_json::Value::as_array) else {
                continue;
            };
            for account in accounts {
                let Some(id) = account.get("id").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let Some(cred) = account.get("credential").and_then(oauth_from) else {
                    continue;
                };
                entries.push(Entry {
                    provider: provider.clone(),
                    id: id.to_string(),
                    label: account
                        .get("alias")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    paused: account
                        .get("paused")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    credential: Secret::OAuth(cred),
                });
            }
            continue;
        };
        // A legacy single slot has no stored id; its identity is the only stable choice.
        let id = cred
            .extra
            .get("account_id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| cred.extra.get("email").and_then(serde_json::Value::as_str))
            .unwrap_or(provider)
            .to_string();
        entries.push(Entry {
            provider: provider.clone(),
            id,
            label: None,
            paused: false,
            credential: Secret::OAuth(cred),
        });
    }
    entries
}

/// `codex-accounts.json` maps account id -> `{generation, credential: {...}}` (or a bare credential).
fn entries_from_codex(store: &serde_json::Value) -> Vec<Entry> {
    let Some(records) = store.as_object() else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for (id, record) in records {
        // Tombstones keep their slot but hold no usable credential.
        if record.get("deletedAt").is_some() {
            continue;
        }
        let cred = record.get("credential").unwrap_or(record);
        let (Some(access), Some(refresh), Some(expires_at), Some(account_id)) = (
            cred.get("accessToken").and_then(serde_json::Value::as_str),
            cred.get("refreshToken").and_then(serde_json::Value::as_str),
            cred.get("expiresAt").and_then(serde_json::Value::as_u64),
            cred.get("chatgptAccountId")
                .and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        let mut extra = serde_json::Map::new();
        extra.insert("account_id".into(), account_id.into());
        entries.push(Entry {
            provider: "openai".into(),
            id: id.clone(),
            label: None,
            paused: false,
            credential: Secret::OAuth(OAuthCred {
                access: access.to_string(),
                refresh: Some(refresh.to_string()),
                // opencodex stores epoch milliseconds; gw stores seconds.
                expires_at: Some(expires_at / 1000),
                extra,
            }),
        });
    }
    entries
}

fn oauth_from(value: &serde_json::Value) -> Option<OAuthCred> {
    let access = value.get("access")?.as_str()?.to_string();
    let mut extra = serde_json::Map::new();
    let fields = [
        ("accountId", "account_id"),
        ("projectId", "project_id"),
        ("email", "email"),
        ("apiBaseUrl", "api_base_url"),
    ];
    for (from, to) in fields {
        if let Some(v) = value.get(from).and_then(serde_json::Value::as_str) {
            extra.insert(to.into(), v.into());
        }
    }
    if let Some(kiro) = value.get("kiro") {
        for (from, to) in [
            ("profileArn", "profile_arn"),
            ("ssoRegion", "sso_region"),
            ("apiRegion", "api_region"),
        ] {
            if let Some(v) = kiro.get(from).and_then(serde_json::Value::as_str) {
                extra.insert(to.into(), v.into());
            }
        }
    }
    Some(OAuthCred {
        access,
        refresh: value
            .get("refresh")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        // opencodex `expires` is epoch milliseconds.
        expires_at: value
            .get("expires")
            .and_then(serde_json::Value::as_u64)
            .map(|ms| ms / 1000),
        extra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "gw-creds-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn api_entry(provider: &str, id: &str) -> Entry {
        Entry {
            provider: provider.into(),
            id: id.into(),
            label: Some("test".into()),
            paused: false,
            credential: Secret::ApiKey {
                key: "sk-fake-not-real".into(),
            },
        }
    }

    fn oauth_entry(provider: &str, id: &str, expires_at: u64) -> Entry {
        let mut extra = serde_json::Map::new();
        extra.insert("account_id".into(), "acct-fake".into());
        Entry {
            provider: provider.into(),
            id: id.into(),
            label: None,
            paused: false,
            credential: Secret::OAuth(OAuthCred {
                access: "access-fake".into(),
                refresh: Some("refresh-fake".into()),
                expires_at: Some(expires_at),
                extra,
            }),
        }
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = TempDir::new("missing");
        let store = Store::load(&dir.0).unwrap();
        assert!(store.all().is_empty());
        assert!(store.accounts("openai").is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = TempDir::new("roundtrip");
        let mut store = Store::load(&dir.0).unwrap();
        store.upsert(api_entry("openai", "work"));
        store.upsert(oauth_entry("anthropic", "a1", 2_000_000_000));
        store.save().unwrap();

        let mode = fs::metadata(dir.0.join(FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let loaded = Store::load(&dir.0).unwrap();
        assert_eq!(loaded.all(), store.all());
        assert_eq!(loaded.accounts("openai").len(), 1);
        assert_eq!(loaded.accounts("anthropic")[0].id, "a1");
    }

    #[test]
    fn refuses_group_or_world_readable_file() {
        let dir = TempDir::new("mode");
        let path = dir.0.join(FILE);
        fs::write(&path, r#"{"version":1,"accounts":[]}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let err = Store::load(&dir.0).unwrap_err().to_string();
        assert!(err.contains("chmod 600"), "{err}");
    }

    #[test]
    fn refuses_other_version() {
        let dir = TempDir::new("version");
        let path = dir.0.join(FILE);
        fs::write(&path, r#"{"version":9,"accounts":[]}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let err = Store::load(&dir.0).unwrap_err().to_string();
        assert!(err.contains("unsupported version"), "{err}");
    }

    #[test]
    fn upsert_remove_and_pause() {
        let dir = TempDir::new("mutate");
        let mut store = Store::load(&dir.0).unwrap();
        store.upsert(api_entry("openai", "work"));
        store.upsert(api_entry("openai", "home"));
        store.upsert(api_entry("anthropic", "work"));
        assert_eq!(store.accounts("openai").len(), 2);

        let mut replaced = api_entry("openai", "work");
        replaced.label = Some("renamed".into());
        store.upsert(replaced);
        assert_eq!(store.accounts("openai").len(), 2);
        assert_eq!(
            store.get("openai", "work").unwrap().label.as_deref(),
            Some("renamed")
        );

        assert!(store.set_paused("openai", "home", true));
        assert!(store.get("openai", "home").unwrap().paused);
        assert!(!store.set_paused("openai", "nope", true));

        assert!(store.remove("openai", "home"));
        assert!(!store.remove("openai", "home"));
        assert_eq!(store.accounts("openai").len(), 1);
        assert_eq!(store.accounts("anthropic").len(), 1);
    }

    #[test]
    fn converts_to_provider_account() {
        let mut oauth = oauth_entry("anthropic", "a1", 1234);
        oauth.paused = true;
        let account = Account::from(&oauth);
        assert_eq!(account.id, "a1");
        match account.credential {
            Credential::OAuth {
                access,
                refresh,
                expires_at,
            } => {
                assert_eq!(access, "access-fake");
                assert_eq!(refresh.as_deref(), Some("refresh-fake"));
                assert_eq!(expires_at, Some(1234));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            Account::from(&api_entry("openai", "x")).credential,
            Credential::ApiKey(_)
        ));
    }

    #[test]
    fn needs_refresh_rules() {
        let expiring = oauth_entry("anthropic", "a1", 1_000);
        assert!(needs_refresh(&expiring, 700, 300), "inside skew");
        assert!(!needs_refresh(&expiring, 699, 300), "outside skew");
        assert!(needs_refresh(&expiring, 5_000, 300), "expired");

        let mut no_refresh = expiring.clone();
        if let Secret::OAuth(c) = &mut no_refresh.credential {
            c.refresh = None;
        }
        assert!(
            !needs_refresh(&no_refresh, 5_000, 300),
            "nothing to refresh with"
        );

        let mut unknown = expiring;
        if let Secret::OAuth(c) = &mut unknown.credential {
            c.expires_at = None;
        }
        assert!(!needs_refresh(&unknown, u64::MAX, 300), "unknown expiry");

        assert!(!needs_refresh(&api_entry("openai", "x"), u64::MAX, 300));
        assert_eq!(DEFAULT_SKEW_SECS, 300);
    }

    struct CountingRefresher {
        calls: AtomicUsize,
    }

    impl Refresher for CountingRefresher {
        fn refresh<'a>(&'a self, cred: &'a OAuthCred) -> BoxFuture<'a, anyhow::Result<OAuthCred>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(OAuthCred {
                    access: "access-new".into(),
                    refresh: cred.refresh.clone(),
                    expires_at: Some(9_999),
                    extra: cred.extra.clone(),
                })
            })
        }
    }

    #[test]
    fn refresh_lock_is_per_account() {
        let dir = TempDir::new("locks");
        let store = Store::load(&dir.0).unwrap();
        let a = store.refresh_lock("anthropic", "a1");
        assert!(Arc::ptr_eq(&a, &store.refresh_lock("anthropic", "a1")));
        assert!(!Arc::ptr_eq(&a, &store.refresh_lock("anthropic", "a2")));
        assert!(!Arc::ptr_eq(&a, &store.refresh_lock("openai", "a1")));
    }

    #[tokio::test]
    async fn refresh_runs_once_per_account() {
        let dir = TempDir::new("refresh");
        let mut store = Store::load(&dir.0).unwrap();
        store.upsert(oauth_entry("anthropic", "a1", 1_000));
        store.save().unwrap();
        let refresher = CountingRefresher {
            calls: AtomicUsize::new(0),
        };

        let first = store
            .refresh_if_due(&refresher, "anthropic", "a1", 900, DEFAULT_SKEW_SECS)
            .await
            .unwrap()
            .expect("refreshed");
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
        match &first.credential {
            Secret::OAuth(c) => assert_eq!(c.access, "access-new"),
            other => panic!("unexpected {other:?}"),
        }

        // A second call sees the persisted expiry and does not hit the refresher again.
        let second = store
            .refresh_if_due(&refresher, "anthropic", "a1", 900, DEFAULT_SKEW_SECS)
            .await
            .unwrap();
        assert!(second.is_none());
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
        assert_eq!(Store::load(&dir.0).unwrap().all(), store.all());

        assert!(store
            .refresh_if_due(&refresher, "anthropic", "missing", 900, DEFAULT_SKEW_SECS)
            .await
            .unwrap()
            .is_none());
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn debug_redacts_secrets() {
        let dir = TempDir::new("debug");
        let mut store = Store::load(&dir.0).unwrap();
        store.upsert(api_entry("openai", "work"));
        store.upsert(oauth_entry("anthropic", "a1", 1_000));
        let text = format!("{store:?}");
        assert!(!text.contains("sk-fake-not-real"), "{text}");
        assert!(!text.contains("access-fake"), "{text}");
        assert!(!text.contains("refresh-fake"), "{text}");
        assert!(text.contains("work"), "{text}");

        let entry = format!("{:?}", store.get("anthropic", "a1").unwrap());
        assert!(!entry.contains("access-fake"), "{entry}");
        assert!(entry.contains("acct-fake"), "{entry}");
    }

    #[test]
    fn imports_opencodex_files() {
        let dir = TempDir::new("import");
        let auth = serde_json::json!({
            "anthropic": {
                "activeAccountId": "a1",
                "accounts": [
                    {
                        "id": "a1",
                        "alias": "work",
                        "paused": true,
                        "credential": {
                            "access": "fake-access-1",
                            "refresh": "fake-refresh-1",
                            "expires": 1_700_000_000_000u64,
                            "accountId": "acct-1",
                            "email": "fake@example.test"
                        }
                    },
                    { "id": "a2", "credential": { "access": "fake-access-2", "refresh": "fake-refresh-2", "expires": 1_700_000_001_000u64 } }
                ]
            },
            "kiro": {
                "access": "fake-access-3",
                "refresh": "fake-refresh-3",
                "expires": 1_700_000_002_000u64,
                "kiro": { "profileArn": "arn:fake", "ssoRegion": "eu-west-1" }
            }
        });
        let codex = serde_json::json!({
            "codex-work": {
                "generation": 3,
                "credential": {
                    "accessToken": "fake-at",
                    "refreshToken": "fake-rt",
                    "expiresAt": 1_700_000_003_000u64,
                    "chatgptAccountId": "chatgpt-fake"
                }
            },
            "codex-gone": { "generation": 1, "deletedAt": 1_700_000_000_000u64 },
            "codex-legacy": {
                "accessToken": "fake-at-2",
                "refreshToken": "fake-rt-2",
                "expiresAt": 1_700_000_004_000u64,
                "chatgptAccountId": "chatgpt-fake-2"
            }
        });
        fs::write(dir.0.join("auth.json"), auth.to_string()).unwrap();
        fs::write(dir.0.join("codex-accounts.json"), codex.to_string()).unwrap();

        let mut entries = import_opencodex(&dir.0).unwrap();
        entries.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
        let keys: Vec<_> = entries
            .iter()
            .map(|e| (e.provider.as_str(), e.id.as_str()))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("anthropic", "a1"),
                ("anthropic", "a2"),
                ("kiro", "kiro"),
                ("openai", "codex-legacy"),
                ("openai", "codex-work"),
            ]
        );

        let a1 = entries.iter().find(|e| e.id == "a1").unwrap();
        assert_eq!(a1.label.as_deref(), Some("work"));
        assert!(a1.paused);
        match &a1.credential {
            Secret::OAuth(c) => {
                assert_eq!(c.access, "fake-access-1");
                assert_eq!(c.expires_at, Some(1_700_000_000));
                assert_eq!(c.extra.get("account_id").unwrap(), "acct-1");
                assert_eq!(c.extra.get("email").unwrap(), "fake@example.test");
            }
            other => panic!("unexpected {other:?}"),
        }

        let kiro = entries.iter().find(|e| e.provider == "kiro").unwrap();
        match &kiro.credential {
            Secret::OAuth(c) => {
                assert_eq!(c.extra.get("profile_arn").unwrap(), "arn:fake");
                assert_eq!(c.extra.get("sso_region").unwrap(), "eu-west-1");
                assert!(c.extra.get("client_secret").is_none());
            }
            other => panic!("unexpected {other:?}"),
        }

        let codex_work = entries.iter().find(|e| e.id == "codex-work").unwrap();
        match &codex_work.credential {
            Secret::OAuth(c) => {
                assert_eq!(c.access, "fake-at");
                assert_eq!(c.refresh.as_deref(), Some("fake-rt"));
                assert_eq!(c.expires_at, Some(1_700_000_003));
                assert_eq!(c.extra.get("account_id").unwrap(), "chatgpt-fake");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(entries.iter().all(|e| e.id != "codex-gone"));
    }

    #[test]
    fn import_tolerates_missing_files() {
        let dir = TempDir::new("import-missing");
        assert!(import_opencodex(&dir.0).unwrap().is_empty());
        assert!(import_opencodex(&dir.0.join("no-such")).unwrap().is_empty());
    }
}
