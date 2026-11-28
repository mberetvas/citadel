//! The Sync run: the recorded history entry of one Sync.
//!
//! The Source lifecycle opens one run per Sync right after taking the claim and closes it
//! exactly once, by id: [`SyncRun::succeed`] inside the transaction that writes the pages, or
//! [`SyncRun::fail`]. Only crash recovery (`claim::reset_interrupted_sync`) writes runs
//! otherwise.

use rusqlite::{params, Connection, Transaction};

use crate::error::Result;
use crate::ingest::SyncSummary;

/// An open (`'running'`) Sync run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SyncRun {
    id: i64,
    source_id: i64,
}

impl SyncRun {
    /// Mark the Source `'syncing'` and insert its `'running'` run.
    pub(crate) fn open(conn: &Connection, source_id: i64) -> Result<Self> {
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE sources SET status = 'syncing', last_error = NULL WHERE id = ?1",
            [source_id],
        )?;
        tx.execute(
            "INSERT INTO sync_runs (source_id, started_at, status)
             VALUES (?1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), 'running')",
            [source_id],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Self { id, source_id })
    }

    /// Close the run as `'ok'` with its counts and mark the Source synced, inside the
    /// transaction that wrote the pages.
    pub(crate) fn succeed(self, tx: &Transaction<'_>, summary: &SyncSummary) -> Result<()> {
        let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        tx.execute(
            "UPDATE sync_runs
             SET finished_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                 status = 'ok',
                 pages_added = ?2,
                 pages_updated = ?3,
                 pages_unchanged = ?4,
                 pages_stale = ?5,
                 pages_failed = ?6
             WHERE id = ?1",
            params![
                self.id,
                count(summary.pages_added),
                count(summary.pages_updated),
                count(summary.pages_unchanged),
                count(summary.pages_stale),
                count(summary.pages_failed),
            ],
        )?;
        tx.execute(
            "UPDATE sources
             SET status = 'ok',
                 last_synced_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                 last_error = NULL
             WHERE id = ?1",
            [self.source_id],
        )?;
        Ok(())
    }

    /// Close the run as `'error'` and mark the Source errored with `message`.
    pub(crate) fn fail(self, conn: &Connection, message: &str) -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE sync_runs
             SET finished_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                 status = 'error',
                 error_message = ?2
             WHERE id = ?1",
            params![self.id, message],
        )?;
        tx.execute(
            "UPDATE sources SET status = 'error', last_error = ?2 WHERE id = ?1",
            params![self.source_id, message],
        )?;
        tx.commit()?;
        Ok(())
    }
}
