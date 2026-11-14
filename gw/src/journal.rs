//! Per-request log of route, account, status and token usage.

use crate::store::{Store, UsageRow};
use anyhow::{Context, Result};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Duration, MissedTickBehavior},
};

/// Rows buffered before `record` starts dropping.
const CAPACITY: usize = 1024;
/// Rows that trigger an early write.
const BATCH_ROWS: usize = 100;
const FLUSH_EVERY: Duration = Duration::from_millis(500);

/// Cheap, non-blocking handle for the request path.
pub struct Journal {
    tx: mpsc::Sender<UsageRow>,
    dropped: Arc<AtomicU64>,
}

impl Journal {
    /// Start the writer task. Must be called inside a Tokio runtime.
    pub fn start(store: Arc<Store>) -> (Self, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let handle = tokio::spawn(writer(store, rx));
        (Self { tx, dropped }, handle)
    }

    /// Queue a row; never blocks. A full channel counts as a drop.
    pub fn record(&self, row: UsageRow) {
        if self.tx.try_send(row).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Rows dropped so far because the buffer was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Stop accepting rows, flush what is buffered, and wait for the writer to finish.
    pub async fn shutdown(self, handle: JoinHandle<()>) -> Result<()> {
        let Self { tx, .. } = self;
        drop(tx);
        handle.await.context("usage journal task failed")?;
        Ok(())
    }
}

async fn writer(store: Arc<Store>, mut rx: mpsc::Receiver<UsageRow>) {
    let mut batch = Vec::with_capacity(BATCH_ROWS);
    let mut ticker = tokio::time::interval(FLUSH_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            row = rx.recv() => match row {
                Some(row) => {
                    batch.push(row);
                    if batch.len() >= BATCH_ROWS {
                        flush(&store, &mut batch).await;
                    }
                }
                None => {
                    flush(&store, &mut batch).await;
                    return;
                }
            },
            _ = ticker.tick() => flush(&store, &mut batch).await,
        }
    }
}

async fn flush(store: &Arc<Store>, batch: &mut Vec<UsageRow>) {
    if batch.is_empty() {
        return;
    }
    let rows = std::mem::replace(batch, Vec::with_capacity(BATCH_ROWS));
    let store = Arc::clone(store);
    match tokio::task::spawn_blocking(move || store.append_usage_batch(&rows)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "usage journal write failed"),
        Err(e) => tracing::warn!(error = %e, "usage journal task failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::now_secs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "gw-journal-{label}-{}-{unique}",
                std::process::id()
            ));
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

    fn row() -> UsageRow {
        UsageRow::new(
            "sess", "route", "openai", "acct", "gpt-5", 200, 10, 5, 0, 3, 42,
        )
    }

    #[tokio::test]
    async fn counts_drops_when_buffer_is_full() {
        let dir = TempDir::new("drops");
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let (journal, handle) = Journal::start(Arc::clone(&store));

        let extra = 7;
        for _ in 0..CAPACITY + extra {
            journal.record(row());
        }
        // A current-thread runtime cannot run the writer until this test yields.
        assert_eq!(journal.dropped(), extra as u64);

        journal.shutdown(handle).await.unwrap();

        let summary = store.usage_summary(0).unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].requests, CAPACITY as u64);
        assert_eq!(summary[0].input, 10 * CAPACITY as u64);
    }

    #[tokio::test]
    async fn flushes_buffered_rows_on_shutdown() {
        let dir = TempDir::new("flush");
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let (journal, handle) = Journal::start(Arc::clone(&store));

        journal.record(row());
        journal.record(row());
        assert_eq!(journal.dropped(), 0);
        assert!(store.usage_summary(now_secs() - 60).unwrap().is_empty());

        journal.shutdown(handle).await.unwrap();

        let summary = store.usage_summary(now_secs() - 60).unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].requests, 2);
        assert_eq!(summary[0].cached_read, 10);
    }
}
