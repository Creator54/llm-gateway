//! Account credentials on disk (0600) and token refresh.

use crate::provider::{Account, Credential};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

/// On-disk format version; anything else is refused rather than guessed at.
pub const VERSION: u32 = 1;
/// Refresh this long before a token expires, so a request never uses a stale one.
pub const DEFAULT_SKEW_SECS: u64 = 300;

const FILE: &str = "credentials.json";
const LOCK: &str = "credentials.lock";
/// A save takes milliseconds; a lock older than this was left behind by a crash.
const LOCK_STALE: Duration = Duration::from_secs(60);
const LOCK_WAIT: Duration = Duration::from_millis(10);
const LOCK_TRIES: usize = 50;
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// OAuth token set; `extra` carries provider routing data such as `account_id`.
///
/// Unknown fields are tolerated so a credential written by a newer build still loads.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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

/// One stored account. Unknown fields are tolerated; see `OAuthCred`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub provider: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub paused: bool,
    /// Set when a refresh failed: the stored tokens are presumed dead until a re-login.
    #[serde(default)]
    pub needs_reauth: bool,
    /// Truncated message from the last refresh failure; never token material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Unix time until which the provider-disabled account sits out; the next request after it rechecks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_until: Option<u64>,
    pub credential: Secret,
}

impl Entry {
    /// The login email, when the provider reported one.
    pub fn email(&self) -> Option<&str> {
        match &self.credential {
            Secret::OAuth(oauth) => oauth.extra.get("email").and_then(serde_json::Value::as_str),
            _ => None,
        }
    }
}

impl Entry {
    /// True when this token is expired or expires within `skew_secs`; unknown expiry counts as fresh.
    pub fn needs_refresh(&self, now_unix: u64, skew_secs: u64) -> bool {
        needs_refresh(self, now_unix, skew_secs)
    }
}

impl From<&Entry> for Account {
    fn from(entry: &Entry) -> Self {
        // `provider::Credential` derives `Debug`, which would print the secret: log
        // accounts through `RedactedAccount`, never with `{:?}` on the account itself.
        let extra = match &entry.credential {
            Secret::OAuth(c) => c
                .extra
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect(),
            Secret::ApiKey { .. } => Default::default(),
        };
        Account {
            id: entry.id.clone(),
            credential: entry.credential.to_credential(),
            extra,
        }
    }
}

/// `Debug` view of a provider account with the secret elided.
pub struct RedactedAccount<'a>(pub &'a Account);

impl fmt::Debug for RedactedAccount<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let credential = match &self.0.credential {
            Credential::None => "none",
            Credential::ApiKey(_) => "api_key(<redacted>)",
            Credential::OAuth { refresh, .. } => {
                if refresh.is_some() {
                    "oauth(<redacted>, refresh set)"
                } else {
                    "oauth(<redacted>)"
                }
            }
        };
        f.debug_struct("Account")
            .field("id", &self.0.id)
            .field("credential", &credential)
            .finish()
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

#[derive(Deserialize)]
struct Version {
    version: u32,
}

#[derive(Default)]
struct State {
    accounts: Vec<Entry>,
    /// One lock per known account, so concurrent requests cannot refresh the same token twice.
    locks: HashMap<(String, String), Arc<AsyncMutex<()>>>,
}

/// Credentials held in memory; every mutation is kept until `save`.
///
/// Sharing is `Arc<Store>`: refreshes hold only the per-account lock across the network
/// call, and the account list is behind an `RwLock` taken for short clones.
pub struct Store {
    dir: PathBuf,
    state: RwLock<State>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("dir", &self.dir)
            .field("accounts", &self.read().accounts)
            .finish()
    }
}

impl Store {
    /// Load `<dir>/credentials.json`; a missing file means no accounts. World/group readable is an error.
    pub fn load(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let path = dir.join(FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    dir,
                    state: RwLock::new(State::default()),
                })
            }
            Err(e) => return Err(anyhow::anyhow!("{}: {e}", path.display())),
        };
        let mode = fs::metadata(&path)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "{} is accessible by other users (mode {mode:o}); run `chmod 600 {}`",
                path.display(),
                path.display()
            );
        }
        // Check the version before the full parse: a file from a newer build then fails
        // with "unsupported version" instead of an unknown-field error.
        let Version { version } =
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        if version != VERSION {
            anyhow::bail!(
                "{}: unsupported version {version} (expected {VERSION})",
                path.display()
            );
        }
        let file: File =
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        validate(&file.accounts).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        warn_loose_dir(&dir);
        Ok(Self {
            dir,
            state: RwLock::new(State {
                accounts: file.accounts,
                locks: HashMap::new(),
            }),
        })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    /// All accounts of one provider, paused ones included; the pool decides eligibility.
    pub fn accounts(&self, provider: &str) -> Vec<Entry> {
        self.read()
            .accounts
            .iter()
            .filter(|e| e.provider == provider)
            .cloned()
            .collect()
    }

    /// Every account, in file order.
    pub fn all(&self) -> Vec<Entry> {
        self.read().accounts.clone()
    }

    /// One account by provider and id.
    pub fn get(&self, provider: &str, id: &str) -> Option<Entry> {
        self.read()
            .accounts
            .iter()
            .find(|e| e.provider == provider && e.id == id)
            .cloned()
    }

    /// Insert or replace by `(provider, id)`.
    pub fn upsert(&self, entry: Entry) {
        let mut state = self.write();
        match state
            .accounts
            .iter_mut()
            .find(|e| e.provider == entry.provider && e.id == entry.id)
        {
            Some(slot) => *slot = entry,
            None => state.accounts.push(entry),
        }
    }

    /// Drop one account and its lock; true when it existed.
    pub fn remove(&self, provider: &str, id: &str) -> bool {
        let mut state = self.write();
        let before = state.accounts.len();
        state
            .accounts
            .retain(|e| !(e.provider == provider && e.id == id));
        let removed = state.accounts.len() != before;
        if removed {
            state.locks.remove(&(provider.to_string(), id.to_string()));
        }
        removed
    }

    /// Pause or resume one account; true when it existed.
    /// Set or clear the provider-disabled mark; false when the account is unknown.
    pub fn set_disabled_until(&self, provider: &str, id: &str, until: Option<u64>) -> bool {
        match self
            .write()
            .accounts
            .iter_mut()
            .find(|e| e.provider == provider && (e.id == id || e.label.as_deref() == Some(id)))
        {
            Some(entry) => {
                entry.disabled_until = until;
                true
            }
            None => false,
        }
    }

    /// Rename an account, resolving it by id or current label.
    pub fn rename(&self, provider: &str, id_or_label: &str, label: &str) -> anyhow::Result<String> {
        anyhow::ensure!(!label.trim().is_empty(), "account label cannot be empty");
        let mut state = self.write();
        let matches: Vec<_> = state
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.provider == provider
                    && (entry.id == id_or_label || entry.label.as_deref() == Some(id_or_label))
            })
            .map(|(index, _)| index)
            .collect();
        anyhow::ensure!(!matches.is_empty(), "no {provider} account {id_or_label}");
        anyhow::ensure!(
            matches.len() == 1,
            "multiple {provider} accounts match {id_or_label:?}; use an account id from `gw account list`"
        );
        anyhow::ensure!(
            !state.accounts.iter().enumerate().any(|(index, entry)| {
                entry.provider == provider
                    && index != matches[0]
                    && (entry.id == label || entry.label.as_deref() == Some(label))
            }),
            "{provider} account label {label:?} conflicts with another account id or label"
        );
        let id = state.accounts[matches[0]].id.clone();
        state.accounts[matches[0]].label = Some(label.to_owned());
        Ok(id)
    }

    pub fn set_paused(&self, provider: &str, id: &str, paused: bool) -> bool {
        match self
            .write()
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

    /// The refresh lock for an existing account; `None` for an unknown one.
    fn account_lock(&self, provider: &str, id: &str) -> Option<Arc<AsyncMutex<()>>> {
        let mut state = self.write();
        if !state
            .accounts
            .iter()
            .any(|e| e.provider == provider && e.id == id)
        {
            return None;
        }
        Some(
            state
                .locks
                .entry((provider.to_string(), id.to_string()))
                .or_default()
                .clone(),
        )
    }

    fn is_due(&self, provider: &str, id: &str, now_unix: u64, skew_secs: u64) -> bool {
        match self.get(provider, id) {
            // A failed refresh stays quiet until a re-login replaces the entry.
            Some(entry) => !entry.needs_reauth && needs_refresh(&entry, now_unix, skew_secs),
            None => false,
        }
    }

    /// Refresh an expiring account under its lock and persist the result.
    ///
    /// Returns `None` when the account is absent or still fresh. Due-ness is rechecked
    /// after taking the lock, so a task that waited behind another refresh makes no
    /// second HTTP call. A failed refresh marks the entry `needs_reauth`, saves, and
    /// returns the error; the next request then skips it instead of retrying.
    pub async fn refresh_if_due(
        &self,
        refresher: &dyn Refresher,
        provider: &str,
        id: &str,
        now_unix: u64,
        skew_secs: u64,
    ) -> anyhow::Result<Option<Entry>> {
        if !self.is_due(provider, id, now_unix, skew_secs) {
            return Ok(None);
        }
        let Some(lock) = self.account_lock(provider, id) else {
            return Ok(None);
        };
        let _held = lock.lock().await;
        if !self.is_due(provider, id, now_unix, skew_secs) {
            return Ok(None);
        }
        let Some(entry) = self.get(provider, id) else {
            return Ok(None);
        };
        let Secret::OAuth(cred) = entry.credential.clone() else {
            return Ok(None);
        };
        match refresher.refresh(&cred).await {
            Ok(fresh) => {
                let mut updated = entry;
                updated.credential = Secret::OAuth(fresh);
                updated.needs_reauth = false;
                updated.last_error = None;
                self.upsert(updated.clone());
                self.save()?;
                Ok(Some(updated))
            }
            // The endpoint never answered: keep the account and try again next request.
            Err(err) if unreachable(&err) => Err(err),
            Err(err) => {
                let mut failed = entry;
                failed.needs_reauth = true;
                failed.last_error = Some(brief(&err));
                self.upsert(failed);
                if let Err(e) = self.save() {
                    tracing::warn!(error = %e, "could not persist the refresh failure marker");
                }
                Err(err)
            }
        }
    }

    /// Write the whole store atomically at mode 0600, keeping the previous file as `.bak`.
    ///
    /// The lock covers only this write: a long-running gateway still holds its loaded
    /// snapshot, so an account added by a CLI while it runs needs a restart to be seen.
    pub fn save(&self) -> anyhow::Result<()> {
        create_private_dir(&self.dir)?;
        let _guard = DirLock::acquire(&self.dir)?;
        let file = File {
            version: VERSION,
            accounts: self.all(),
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

fn validate(accounts: &[Entry]) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    for entry in accounts {
        if entry.provider.is_empty() || entry.id.is_empty() {
            anyhow::bail!("account with empty provider or id");
        }
        if !seen.insert((entry.provider.as_str(), entry.id.as_str())) {
            anyhow::bail!("duplicate account {}/{}", entry.provider, entry.id);
        }
    }
    Ok(())
}

fn warn_loose_dir(dir: &Path) {
    let Ok(meta) = fs::metadata(dir) else { return };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        tracing::warn!(
            "credentials directory {} has mode {mode:o}; run `chmod 700` on it",
            dir.display()
        );
    }
}

/// First 200 chars of a refresh error; the refresher must keep tokens out of its message.
/// A transport failure (DNS, connect, timeout) rather than a refused refresh.
fn unreachable(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>().is_some_and(|e| {
            e.status().is_none() && (e.is_connect() || e.is_timeout() || e.is_request())
        })
    })
}

fn brief(err: &anyhow::Error) -> String {
    err.to_string().chars().take(200).collect()
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    // Do not chmod an existing directory: it may not be ours.
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Write `bytes` to `path` via a temp file in the same directory, fsync, then rename.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    create_private_dir(dir)?;
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
    // Keep the previous good file; a failed copy is not worth losing the new one over.
    let bak = dir.join(format!("{name}.bak"));
    if path.exists() {
        match fs::copy(path, &bak) {
            Ok(_) => {
                let _ = fs::set_permissions(&bak, fs::Permissions::from_mode(0o600));
            }
            Err(e) => tracing::warn!("could not keep {}: {e}", bak.display()),
        }
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

/// Advisory cross-process lock on `<dir>/credentials.lock`, held for one save.
struct DirLock {
    path: PathBuf,
}

impl DirLock {
    fn acquire(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join(LOCK);
        for _ in 0..LOCK_TRIES {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    std::thread::sleep(LOCK_WAIT);
                }
                Err(e) => return Err(anyhow::anyhow!("{}: {e}", path.display())),
            }
        }
        anyhow::bail!("{}: still locked by another process", path.display())
    }
}

fn stale(path: &Path) -> bool {
    match fs::metadata(path).and_then(|m| m.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|age| age > LOCK_STALE)
            .unwrap_or(false),
        Err(_) => true,
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
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
            needs_reauth: false,
            last_error: None,
            disabled_until: None,
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
            needs_reauth: false,
            last_error: None,
            disabled_until: None,
            credential: Secret::OAuth(OAuthCred {
                access: "access-fake".into(),
                refresh: Some("refresh-fake".into()),
                expires_at: Some(expires_at),
                extra,
            }),
        }
    }

    fn write_600(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
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
        let store = Store::load(&dir.0).unwrap();
        store.upsert(api_entry("openai", "work"));
        store.upsert(oauth_entry("anthropic", "a1", 2_000_000_000));
        store.save().unwrap();

        let mode = fs::metadata(dir.0.join(FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let loaded = Store::load(&dir.0).unwrap();
        assert_eq!(loaded.all(), store.all());
        assert_eq!(loaded.accounts("openai").len(), 1);
        assert_eq!(loaded.accounts("anthropic")[0].id, "a1");
        assert!(!dir.0.join(LOCK).exists(), "lock released");
    }

    #[test]
    fn save_creates_a_private_dir() {
        let root = TempDir::new("dir-mode");
        let dir = root.0.join("state/creds");
        let store = Store::load(&dir).unwrap();
        store.upsert(api_entry("openai", "a"));
        store.save().unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn save_keeps_the_previous_file_as_backup() {
        let dir = TempDir::new("bak");
        let store = Store::load(&dir.0).unwrap();
        store.upsert(api_entry("openai", "first"));
        store.save().unwrap();
        let bak = dir.0.join("credentials.json.bak");
        assert!(!bak.exists(), "nothing to back up yet");

        store.upsert(api_entry("openai", "second"));
        store.save().unwrap();
        let previous = fs::read_to_string(&bak).unwrap();
        assert!(previous.contains("first"), "{previous}");
        assert!(!previous.contains("second"), "{previous}");
        assert_eq!(
            fs::metadata(&bak).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(fs::read_to_string(dir.0.join(FILE))
            .unwrap()
            .contains("second"));
    }

    #[test]
    fn save_refuses_while_another_process_holds_the_lock() {
        let dir = TempDir::new("locked");
        let store = Store::load(&dir.0).unwrap();
        store.upsert(api_entry("openai", "a"));
        fs::write(dir.0.join(LOCK), "999999").unwrap();
        let err = store.save().unwrap_err().to_string();
        assert!(err.contains("locked"), "{err}");
        assert!(!dir.0.join(FILE).exists(), "no partial file written");
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
        write_600(&path, r#"{"version":9,"accounts":[]}"#);
        let err = Store::load(&dir.0).unwrap_err().to_string();
        assert!(err.contains("unsupported version"), "{err}");
    }

    #[test]
    fn version_is_checked_before_the_account_fields() {
        let dir = TempDir::new("version-first");
        // A newer build's file has both a new version and fields this build does not know.
        write_600(
            &dir.0.join(FILE),
            r#"{"version":9,"future_field":1,"accounts":[{"provider":"p","id":"i","scope":"new","credential":{"type":"api_key","key":"k"}}]}"#,
        );
        let err = Store::load(&dir.0).unwrap_err().to_string();
        assert!(err.contains("unsupported version"), "{err}");
    }

    #[test]
    fn tolerates_unknown_account_fields() {
        let dir = TempDir::new("unknown-fields");
        write_600(
            &dir.0.join(FILE),
            r#"{"version":1,"accounts":[{"provider":"p","id":"i","future":true,"credential":{"type":"o_auth","access":"a","scope":"x"}}]}"#,
        );
        let store = Store::load(&dir.0).unwrap();
        assert_eq!(store.all().len(), 1);
        assert_eq!(store.get("p", "i").unwrap().id, "i");
    }

    #[test]
    fn refuses_duplicate_accounts() {
        let dir = TempDir::new("duplicate");
        write_600(
            &dir.0.join(FILE),
            r#"{"version":1,"accounts":[
                {"provider":"p","id":"i","credential":{"type":"api_key","key":"a"}},
                {"provider":"p","id":"i","credential":{"type":"api_key","key":"b"}}
            ]}"#,
        );
        let err = Store::load(&dir.0).unwrap_err().to_string();
        assert!(err.contains("duplicate account p/i"), "{err}");
    }

    #[test]
    fn rename_resolves_ids_and_labels_and_rejects_ambiguous_names() {
        let dir = TempDir::new("rename");
        let store = Store::load(&dir.0).unwrap();
        let mut first = api_entry("openai", "id-1");
        first.label = Some("first".into());
        store.upsert(first);
        store.upsert(api_entry("openai", "id-2"));

        store.rename("openai", "id-1", "primary").unwrap();
        store.rename("openai", "id-2", "secondary").unwrap();
        assert_eq!(
            store.get("openai", "id-1").unwrap().label.as_deref(),
            Some("primary")
        );
        assert_eq!(
            store.get("openai", "id-2").unwrap().label.as_deref(),
            Some("secondary")
        );
        assert!(store.rename("openai", "primary", "secondary").is_err());
        assert!(store.rename("openai", "id-1", "id-2").is_err());
        assert!(store.rename("openai", "missing", "unused").is_err());
        assert!(store.rename("openai", "id-1", "  ").is_err());
        assert!(store.rename("anthropic", "id-1", "unused").is_err());
    }

    #[test]
    fn upsert_remove_and_pause() {
        let dir = TempDir::new("mutate");
        let store = Store::load(&dir.0).unwrap();
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
    fn redacted_account_hides_secrets() {
        let account = Account::from(&api_entry("openai", "x"));
        let text = format!("{:?}", RedactedAccount(&account));
        assert!(!text.contains("sk-fake-not-real"), "{text}");
        assert!(text.contains("redacted"), "{text}");
        assert!(text.contains('x'), "{text}");

        let oauth = Account::from(&oauth_entry("anthropic", "a1", 1_000));
        let text = format!("{:?}", RedactedAccount(&oauth));
        assert!(!text.contains("access-fake"), "{text}");
        assert!(!text.contains("refresh-fake"), "{text}");
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
                // Hand the other tasks a chance to race for the same lock.
                tokio::task::yield_now().await;
                Ok(OAuthCred {
                    access: "access-new".into(),
                    refresh: cred.refresh.clone(),
                    expires_at: Some(9_999),
                    extra: cred.extra.clone(),
                })
            })
        }
    }

    struct DownRefresher;

    impl Refresher for DownRefresher {
        fn refresh<'a>(&'a self, _cred: &'a OAuthCred) -> BoxFuture<'a, anyhow::Result<OAuthCred>> {
            Box::pin(async move {
                // Nothing listens on port 1.
                let err = reqwest::Client::new()
                    .post("http://127.0.0.1:1/token")
                    .send()
                    .await
                    .unwrap_err();
                Err(anyhow::Error::new(err).context("token refresh"))
            })
        }
    }

    struct FailingRefresher {
        calls: AtomicUsize,
    }

    impl Refresher for FailingRefresher {
        fn refresh<'a>(&'a self, _cred: &'a OAuthCred) -> BoxFuture<'a, anyhow::Result<OAuthCred>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!("invalid_grant: refresh rejected"))
            })
        }
    }

    #[test]
    fn refresh_locks_exist_only_for_known_accounts() {
        let dir = TempDir::new("locks");
        let store = Store::load(&dir.0).unwrap();
        assert!(store.account_lock("anthropic", "missing").is_none());
        assert_eq!(store.read().locks.len(), 0);

        store.upsert(oauth_entry("anthropic", "a1", 1_000));
        let a = store.account_lock("anthropic", "a1").unwrap();
        assert!(Arc::ptr_eq(
            &a,
            &store.account_lock("anthropic", "a1").unwrap()
        ));
        assert_eq!(store.read().locks.len(), 1);

        assert!(store.remove("anthropic", "a1"));
        assert_eq!(store.read().locks.len(), 0, "lock dropped with the account");
    }

    #[tokio::test]
    async fn concurrent_refresh_calls_the_refresher_once() {
        let dir = TempDir::new("concurrent");
        let store = Arc::new(Store::load(&dir.0).unwrap());
        store.upsert(oauth_entry("anthropic", "a1", 1_000));
        store.save().unwrap();
        let refresher = Arc::new(CountingRefresher {
            calls: AtomicUsize::new(0),
        });

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let store = Arc::clone(&store);
            let refresher = Arc::clone(&refresher);
            tasks.push(tokio::spawn(async move {
                store
                    .refresh_if_due(
                        refresher.as_ref(),
                        "anthropic",
                        "a1",
                        900,
                        DEFAULT_SKEW_SECS,
                    )
                    .await
            }));
        }
        let mut refreshed = 0;
        for task in tasks {
            if task.await.unwrap().unwrap().is_some() {
                refreshed += 1;
            }
        }
        assert_eq!(
            refreshed, 1,
            "one task refreshed, the rest saw a fresh token"
        );
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_runs_once_per_account() {
        let dir = TempDir::new("refresh");
        let store = Store::load(&dir.0).unwrap();
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

    #[tokio::test]
    async fn unreachable_token_endpoint_does_not_mark_the_entry() {
        let dir = TempDir::new("refresh-down");
        let store = Store::load(&dir.0).unwrap();
        store.upsert(oauth_entry("anthropic", "a1", 1_000));
        let refresher = DownRefresher;
        assert!(store
            .refresh_if_due(&refresher, "anthropic", "a1", 900, DEFAULT_SKEW_SECS)
            .await
            .is_err());
        assert!(!store.get("anthropic", "a1").unwrap().needs_reauth);
    }

    #[tokio::test]
    async fn a_forced_refresh_of_a_fresh_token_marks_it_when_refused() {
        let dir = TempDir::new("refresh-forced");
        let store = Store::load(&dir.0).unwrap();
        store.upsert(oauth_entry("anthropic", "a1", 1_000_000));
        let refresher = FailingRefresher {
            calls: AtomicUsize::new(0),
        };
        assert!(store
            .refresh_if_due(&refresher, "anthropic", "a1", 900, u64::MAX)
            .await
            .is_err());
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
        assert!(store.get("anthropic", "a1").unwrap().needs_reauth);
    }

    #[tokio::test]
    async fn refresh_failure_marks_the_entry_and_stops_retrying() {
        let dir = TempDir::new("refresh-fail");
        let store = Store::load(&dir.0).unwrap();
        store.upsert(oauth_entry("anthropic", "a1", 1_000));
        store.save().unwrap();
        let refresher = FailingRefresher {
            calls: AtomicUsize::new(0),
        };

        let err = store
            .refresh_if_due(&refresher, "anthropic", "a1", 900, DEFAULT_SKEW_SECS)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid_grant"), "{err}");
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);

        let marked = store.get("anthropic", "a1").unwrap();
        assert!(marked.needs_reauth);
        assert!(marked
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("invalid_grant"));
        match &marked.credential {
            // The tokens stay so a manual re-login can still compare them.
            Secret::OAuth(c) => assert_eq!(c.access, "access-fake"),
            other => panic!("unexpected {other:?}"),
        }

        let reloaded = Store::load(&dir.0).unwrap();
        assert!(reloaded.get("anthropic", "a1").unwrap().needs_reauth);

        // The marker is persisted, so the next request does not hammer the provider.
        assert!(store
            .refresh_if_due(&refresher, "anthropic", "a1", 900, DEFAULT_SKEW_SECS)
            .await
            .unwrap()
            .is_none());
        assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn debug_redacts_secrets() {
        let dir = TempDir::new("debug");
        let store = Store::load(&dir.0).unwrap();
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
}
