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
    lost: Arc<AtomicU64>,
    handle: JoinHandle<()>,
}

impl Journal {
    /// Start the writer task. Must be called inside a Tokio runtime.
    pub fn start(store: Arc<Store>) -> Self {
        let (tx, rx) = mpsc::channel(CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = Arc::new(AtomicU64::new(0));
        let handle = tokio::spawn(writer(store, rx, Arc::clone(&lost)));
        Self {
            tx,
            dropped,
            lost,
            handle,
        }
    }

    /// Queue a row; never blocks. A full channel counts as a drop.
    pub fn record(&self, row: UsageRow) {
        if self.tx.try_send(row).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Rows dropped because the buffer was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Rows lost because a write failed twice.
    pub fn lost(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }

    /// Stop accepting rows, flush what is buffered, and report anything lost.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            tx,
            dropped,
            lost,
            handle,
        } = self;
        drop(tx);
        handle.await.context("usage journal task failed")?;
        let dropped = dropped.load(Ordering::Relaxed);
        let lost = lost.load(Ordering::Relaxed);
        if dropped > 0 || lost > 0 {
            tracing::warn!(dropped, lost, "usage journal lost rows");
        }
        Ok(())
    }
}

async fn writer(store: Arc<Store>, mut rx: mpsc::Receiver<UsageRow>, lost: Arc<AtomicU64>) {
    let mut batch = Vec::with_capacity(BATCH_ROWS);
    let mut ticker = tokio::time::interval(FLUSH_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            row = rx.recv() => match row {
                Some(row) => {
                    batch.push(row);
                    if batch.len() >= BATCH_ROWS {
                        flush(&store, &mut batch, &lost).await;
                    }
                }
                None => {
                    flush(&store, &mut batch, &lost).await;
                    return;
                }
            },
            _ = ticker.tick() => flush(&store, &mut batch, &lost).await,
        }
    }
}

async fn flush(store: &Arc<Store>, batch: &mut Vec<UsageRow>, lost: &AtomicU64) {
    if batch.is_empty() {
        return;
    }
    let rows = Arc::new(std::mem::replace(batch, Vec::with_capacity(BATCH_ROWS)));
    if let Err(first) = write(store, &rows).await {
        // One immediate retry rides out a competing writer; busy_timeout already
        // waited, so anything still failing is not going to fix itself.
        if let Err(e) = write(store, &rows).await {
            lost.fetch_add(rows.len() as u64, Ordering::Relaxed);
            tracing::warn!(
                error = %e,
                first = %first,
                rows = rows.len(),
                "usage journal write failed twice, rows lost"
            );
        }
    }
}

async fn write(store: &Arc<Store>, rows: &Arc<Vec<UsageRow>>) -> Result<()> {
    let store = Arc::clone(store);
    let rows = Arc::clone(rows);
    tokio::task::spawn_blocking(move || store.append_usage_batch(&rows))
        .await
        .context("usage journal task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::now_secs;
    use crate::store::testutil::TempDir;

    fn row() -> UsageRow {
        UsageRow::new(
            "sess", "route", "openai", "acct", "gpt-5", 200, 10, 5, 0, 3, 42,
        )
    }

    #[tokio::test]
    async fn counts_drops_when_buffer_is_full() {
        let dir = TempDir::new("drops");
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let journal = Journal::start(Arc::clone(&store));

        let extra = 7;
        for _ in 0..CAPACITY + extra {
            journal.record(row());
        }
        // A current-thread runtime cannot run the writer until this test yields.
        assert_eq!(journal.dropped(), extra as u64);

        journal.shutdown().await.unwrap();

        let summary = store.usage_summary(0).unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].requests, CAPACITY as u64);
        assert_eq!(summary[0].input, 10 * CAPACITY as u64);
    }

    #[tokio::test]
    async fn flushes_buffered_rows_on_shutdown() {
        let dir = TempDir::new("flush");
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let journal = Journal::start(Arc::clone(&store));

        journal.record(row());
        journal.record(row());
        assert_eq!(journal.dropped(), 0);
        assert!(store.usage_summary(now_secs() - 60).unwrap().is_empty());

        journal.shutdown().await.unwrap();

        let summary = store.usage_summary(now_secs() - 60).unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].requests, 2);
        assert_eq!(summary[0].cached_read, 10);
    }

    #[tokio::test]
    async fn retries_a_failed_batch_once_then_counts_rows_lost() {
        let dir = TempDir::new("lost");
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let journal = Journal::start(Arc::clone(&store));

        // Drop the table from a second connection so every write fails.
        rusqlite::Connection::open(dir.path().join("gw.db"))
            .unwrap()
            .execute_batch("DROP TABLE usage")
            .unwrap();

        for _ in 0..BATCH_ROWS {
            journal.record(row());
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while journal.lost() == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("writer should give up on the batch");

        assert_eq!(journal.lost(), BATCH_ROWS as u64);
        assert!(store.usage_summary(0).is_err());
        journal.shutdown().await.unwrap();
    }
}
