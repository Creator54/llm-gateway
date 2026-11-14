//! Small SQLite state: learned provider quirks, sessions, usage.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Negative means KiB; 512 KiB is plenty and keeps the process small.
const CACHE_SIZE_KIB: i32 = -512;

/// Applied in order; `user_version` records how many have run.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE quirks (
    provider   TEXT NOT NULL,
    model      TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider, model, key)
) WITHOUT ROWID;

CREATE TABLE sessions (
    session    TEXT PRIMARY KEY,
    target     TEXT NOT NULL,
    account    TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE usage (
    ts          INTEGER NOT NULL,
    session     TEXT NOT NULL,
    route       TEXT NOT NULL,
    provider    TEXT NOT NULL,
    account     TEXT NOT NULL,
    model       TEXT NOT NULL,
    status      INTEGER NOT NULL,
    input       INTEGER NOT NULL,
    cached_read INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    output      INTEGER NOT NULL,
    latency_ms  INTEGER NOT NULL
);
CREATE INDEX usage_ts ON usage(ts);
CREATE INDEX usage_provider_model ON usage(provider, model);
"#];

/// One finished request, as appended to the usage table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow {
    /// Unix seconds.
    pub ts: i64,
    pub session: String,
    pub route: String,
    pub provider: String,
    pub account: String,
    pub model: String,
    pub status: u16,
    pub input: u64,
    pub cached_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub latency_ms: u64,
}

impl UsageRow {
    /// Build a row with the current time.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: impl Into<String>,
        route: impl Into<String>,
        provider: impl Into<String>,
        account: impl Into<String>,
        model: impl Into<String>,
        status: u16,
        input: u64,
        cached_read: u64,
        cache_write: u64,
        output: u64,
        latency_ms: u64,
    ) -> Self {
        Self {
            ts: now_secs(),
            session: session.into(),
            route: route.into(),
            provider: provider.into(),
            account: account.into(),
            model: model.into(),
            status,
            input,
            cached_read,
            cache_write,
            output,
            latency_ms,
        }
    }
}

/// Where a session is currently pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub target: String,
    pub account: String,
    pub updated_at: i64,
}

/// Per provider/model totals over a time window.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSummary {
    pub provider: String,
    pub model: String,
    pub requests: u64,
    pub input: u64,
    pub cached_read: u64,
    pub cache_write: u64,
    pub output: u64,
    /// `cached_read / (input + cached_read + cache_write)`, 0 when there was no input.
    pub cache_read_share: f64,
}

/// Shared SQLite handle. All methods are synchronous; callers use `spawn_blocking`.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) `<state_dir>/gw.db`.
    pub fn open(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create {}", state_dir.display()))?;
        let db = state_dir.join("gw.db");
        let mut conn = Connection::open(&db).with_context(|| format!("open {}", db.display()))?;

        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .context("enable WAL")?;
        if !mode.eq_ignore_ascii_case("wal") {
            bail!(
                "could not enable WAL on {}: journal_mode={mode}",
                db.display()
            );
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "cache_size", CACHE_SIZE_KIB)?;
        migrate(&mut conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Take the connection, recovering from a poisoned lock so a panicked caller cannot wedge the gateway.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read a learned fact such as `("anthropic", "claude", "prompt_cache_retention")`.
    pub fn get_quirk(&self, provider: &str, model: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT value FROM quirks WHERE provider = ?1 AND model = ?2 AND key = ?3",
                params![provider, model, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Insert or replace a learned fact.
    pub fn set_quirk(&self, provider: &str, model: &str, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO quirks (provider, model, key, value, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(provider, model, key) DO UPDATE SET
                 value = excluded.value,
                 updated_at = excluded.updated_at",
            params![provider, model, key, value, now_secs()],
        )?;
        Ok(())
    }

    /// Pin a session to a target and account, replacing any previous pin.
    pub fn put_session(&self, session: &str, target: &str, account: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO sessions (session, target, account, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session) DO UPDATE SET
                 target = excluded.target,
                 account = excluded.account,
                 updated_at = excluded.updated_at",
            params![session, target, account, now_secs()],
        )?;
        Ok(())
    }

    /// Current pin for a session, if it has not been pruned.
    pub fn get_session(&self, session: &str) -> Result<Option<Session>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT target, account, updated_at FROM sessions WHERE session = ?1",
                [session],
                |r| {
                    Ok(Session {
                        target: r.get(0)?,
                        account: r.get(1)?,
                        updated_at: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Delete pins older than `older_than_days` and return how many went.
    pub fn prune_sessions(&self, older_than_days: u64) -> Result<usize> {
        let cutoff = now_secs() - (older_than_days as i64) * 86_400;
        Ok(self
            .conn()
            .execute("DELETE FROM sessions WHERE updated_at < ?1", [cutoff])?)
    }

    /// Append one usage row.
    pub fn append_usage(&self, row: &UsageRow) -> Result<()> {
        self.append_usage_batch(std::slice::from_ref(row))
    }

    /// Append rows in one transaction.
    pub fn append_usage_batch(&self, rows: &[UsageRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO usage
                     (ts, session, route, provider, account, model, status,
                      input, cached_read, cache_write, output, latency_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.ts,
                    r.session,
                    r.route,
                    r.provider,
                    r.account,
                    r.model,
                    r.status,
                    r.input,
                    r.cached_read,
                    r.cache_write,
                    r.output,
                    r.latency_ms,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Totals per provider/model for rows at or after `since_ts`.
    pub fn usage_summary(&self, since_ts: i64) -> Result<Vec<UsageSummary>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT provider, model, COUNT(*),
                    SUM(input), SUM(cached_read), SUM(cache_write), SUM(output)
             FROM usage
             WHERE ts >= ?1
             GROUP BY provider, model
             ORDER BY provider, model",
        )?;
        let rows = stmt.query_map([since_ts], |r| {
            let input: i64 = r.get(3)?;
            let cached_read: i64 = r.get(4)?;
            let cache_write: i64 = r.get(5)?;
            let total_input = input + cached_read + cache_write;
            Ok(UsageSummary {
                provider: r.get(0)?,
                model: r.get(1)?,
                requests: r.get::<_, i64>(2)? as u64,
                input: input as u64,
                cached_read: cached_read as u64,
                cache_write: cache_write as u64,
                output: r.get::<_, i64>(6)? as u64,
                cache_read_share: if total_input == 0 {
                    0.0
                } else {
                    cached_read as f64 / total_input as f64
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// Current Unix time in seconds, the unit every timestamp column uses.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let mut version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    while (version as usize) < MIGRATIONS.len() {
        let next = version + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(MIGRATIONS[version as usize])?;
        tx.pragma_update(None, "user_version", next)?;
        tx.commit()?;
        version = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("gw-store-{label}-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn row(
        ts: i64,
        provider: &str,
        model: &str,
        input: u64,
        cached_read: u64,
        cache_write: u64,
        output: u64,
    ) -> UsageRow {
        UsageRow {
            ts,
            session: "s".into(),
            route: "r".into(),
            provider: provider.into(),
            account: "a".into(),
            model: model.into(),
            status: 200,
            input,
            cached_read,
            cache_write,
            output,
            latency_ms: 5,
        }
    }

    fn version(store: &Store) -> i64 {
        let conn = store.conn();
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = TempDir::new("migrate");
        let first = Store::open(dir.path()).unwrap();
        assert_eq!(version(&first), MIGRATIONS.len() as i64);
        drop(first);

        let second = Store::open(dir.path()).unwrap();
        assert_eq!(version(&second), MIGRATIONS.len() as i64);
        second.set_quirk("p", "m", "k", "v").unwrap();
        assert_eq!(
            second.get_quirk("p", "m", "k").unwrap().as_deref(),
            Some("v")
        );
    }

    #[test]
    fn quirks_roundtrip() {
        let dir = TempDir::new("quirks");
        let store = Store::open(dir.path()).unwrap();

        assert_eq!(
            store
                .get_quirk("openai", "gpt-5", "prompt_cache_retention")
                .unwrap(),
            None
        );
        store
            .set_quirk("openai", "gpt-5", "prompt_cache_retention", "unsupported")
            .unwrap();
        assert_eq!(
            store
                .get_quirk("openai", "gpt-5", "prompt_cache_retention")
                .unwrap()
                .as_deref(),
            Some("unsupported")
        );
        store
            .set_quirk("openai", "gpt-5", "prompt_cache_retention", "supported")
            .unwrap();
        assert_eq!(
            store
                .get_quirk("openai", "gpt-5", "prompt_cache_retention")
                .unwrap()
                .as_deref(),
            Some("supported")
        );
    }

    #[test]
    fn session_upsert_and_prune() {
        let dir = TempDir::new("sessions");
        let store = Store::open(dir.path()).unwrap();

        store.put_session("s1", "openai/gpt-5", "acct-a").unwrap();
        let got = store.get_session("s1").unwrap().unwrap();
        assert_eq!(got.target, "openai/gpt-5");
        assert_eq!(got.account, "acct-a");

        store
            .put_session("s1", "anthropic/claude", "acct-b")
            .unwrap();
        let got = store.get_session("s1").unwrap().unwrap();
        assert_eq!(got.target, "anthropic/claude");
        assert_eq!(got.account, "acct-b");

        store.put_session("old", "x", "y").unwrap();
        store
            .conn()
            .execute(
                "UPDATE sessions SET updated_at = ?1 WHERE session = 'old'",
                [now_secs() - 3 * 86_400],
            )
            .unwrap();

        assert_eq!(store.prune_sessions(2).unwrap(), 1);
        assert!(store.get_session("old").unwrap().is_none());
        assert!(store.get_session("s1").unwrap().is_some());
    }

    #[test]
    fn usage_summary_math() {
        let dir = TempDir::new("usage");
        let store = Store::open(dir.path()).unwrap();
        let now = 1_700_000_000;

        store
            .append_usage(&row(now - 10, "openai", "m", 100, 50, 0, 10))
            .unwrap();
        store
            .append_usage(&row(now - 5, "openai", "m", 100, 50, 0, 20))
            .unwrap();
        store
            .append_usage(&row(now - 5, "anthropic", "c", 10, 30, 20, 100))
            .unwrap();
        // Outside the window.
        store
            .append_usage(&row(now - 1_000, "openai", "m", 1, 0, 0, 1))
            .unwrap();

        let summary = store.usage_summary(now - 100).unwrap();
        assert_eq!(summary.len(), 2);

        let openai = summary.iter().find(|s| s.provider == "openai").unwrap();
        assert_eq!(openai.model, "m");
        assert_eq!(openai.requests, 2);
        assert_eq!(openai.input, 200);
        assert_eq!(openai.cached_read, 100);
        assert_eq!(openai.cache_write, 0);
        assert_eq!(openai.output, 30);
        assert!((openai.cache_read_share - 100.0 / 300.0).abs() < 1e-9);

        let anthropic = summary.iter().find(|s| s.provider == "anthropic").unwrap();
        assert_eq!(anthropic.requests, 1);
        assert_eq!(anthropic.input, 10);
        assert_eq!(anthropic.cached_read, 30);
        assert_eq!(anthropic.cache_write, 20);
        assert_eq!(anthropic.output, 100);
        assert!((anthropic.cache_read_share - 30.0 / 60.0).abs() < 1e-9);
    }
}
