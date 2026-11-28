//! One Sync as one task, from claim to release.
//!
//! [`SourceLifecycle::start_sync`] spawns the task at once and hands back a [`SyncHandle`].
//! The task loads the Source, takes its Sync claim, runs the Sync and releases the claim;
//! it never waits on its caller, so dropping the handle neither cancels the Sync nor leaks
//! the claim.

use std::time::Instant;

use rusqlite::Connection;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::claim::ClaimGuard;
use super::sync_run::SyncRun;
use super::{blocking, SourceLifecycle, Stage, SyncOutcome};
use crate::db::open_rw_db;
use crate::error::{Result, VaultError};
use crate::ingest::persist::{write_pages, SyncPages};
use crate::ingest::{load_source_by_id, load_source_by_name, SourceRecord, SyncSummary};

/// A Sync that holds its claim and has opened its Sync run.
struct Begun {
    source: SourceRecord,
    guard: ClaimGuard,
    run: SyncRun,
}

/// Which Source a Sync is for.
pub(super) enum Target {
    Name(String),
    /// A Source loaded earlier; it may have been removed since (`NotFound`).
    Record {
        id: i64,
        name: String,
    },
}

/// What the task tells its caller once it knows whether it holds the claim.
type ClaimReport = Result<bool>;

/// A Sync that was started. Dropping it does not stop the Sync.
#[derive(Debug)]
pub struct SyncHandle {
    claimed: oneshot::Receiver<ClaimReport>,
    task: JoinHandle<Result<SyncSummary>>,
}

/// A Sync that holds its claim and is running. Dropping it does not stop the Sync.
#[derive(Debug)]
pub struct RunningSync {
    task: JoinHandle<Result<SyncSummary>>,
}

impl SyncHandle {
    /// Wait until the Sync holds its claim: `None` when another Sync of the Source holds it
    /// and nothing was done.
    pub async fn claimed(self) -> Result<Option<RunningSync>> {
        match self.claimed.await {
            Ok(Ok(true)) => Ok(Some(RunningSync { task: self.task })),
            Ok(Ok(false)) => Ok(None),
            Ok(Err(err)) => Err(err),
            // The task ended without reporting: it panicked before the claim.
            Err(_) => match self.task.await {
                Err(err) => Err(task_failed(&err)),
                Ok(_) => Err(VaultError::Other(
                    "sync ended before taking its claim".into(),
                )),
            },
        }
    }

    /// Wait for the Sync to end.
    pub async fn finish(self) -> Result<SyncOutcome> {
        match self.claimed().await? {
            Some(running) => Ok(SyncOutcome::Synced(running.finish().await?)),
            None => Ok(SyncOutcome::AlreadySyncing),
        }
    }
}

impl RunningSync {
    /// Wait for the Sync to end.
    pub async fn finish(self) -> Result<SyncSummary> {
        self.task.await.map_err(|err| task_failed(&err))?
    }
}

fn task_failed(err: &tokio::task::JoinError) -> VaultError {
    VaultError::Other(format!("sync task failed: {err}"))
}

/// Spawn the Sync task now and return its handle.
pub(super) fn start(lifecycle: &SourceLifecycle, target: Target, prune: bool) -> SyncHandle {
    let (report, claimed) = oneshot::channel();
    let task = tokio::spawn(run(lifecycle.clone(), target, prune, report));
    SyncHandle { claimed, task }
}

/// The whole Sync. Every step runs here, never in the caller's future.
async fn run(
    lifecycle: SourceLifecycle,
    target: Target,
    prune: bool,
    report: oneshot::Sender<ClaimReport>,
) -> Result<SyncSummary> {
    let begun = {
        let lifecycle = lifecycle.clone();
        blocking(move || begin(&lifecycle, target)).await
    };
    // A caller that went away no longer wants the report; the Sync goes on regardless. When
    // the Sync does not start, the report is the whole answer: the handle never reads the
    // task's own error below.
    let Begun { source, guard, run } = match begun {
        Ok(Some(begun)) => {
            let _ = report.send(Ok(true));
            begun
        }
        Ok(None) => {
            let _ = report.send(Ok(false));
            return Err(VaultError::AlreadyRunning(
                "sync claim held elsewhere".into(),
            ));
        }
        Err(err) => {
            let message = err.to_string();
            let _ = report.send(Err(err));
            return Err(VaultError::Other(message));
        }
    };

    // If the driver panics, unwinding drops the guard: it releases the claim, and the
    // release closes the open run as interrupted.
    let collected = lifecycle.driver.collect(source.clone(), prune).await;
    blocking(move || finish(&lifecycle, &source, run, guard, collected)).await
}

/// Write the pages and close the run (or record the failure), then release the claim.
fn finish(
    lifecycle: &SourceLifecycle,
    source: &SourceRecord,
    run: SyncRun,
    guard: ClaimGuard,
    collected: Result<SyncPages>,
) -> Result<SyncSummary> {
    let mut conn = open_rw_db(&lifecycle.config.db_path())?;
    let result = collected.and_then(|pages| write(lifecycle, &mut conn, source, run, pages));
    if let Err(err) = &result {
        if let Err(record_err) = run.fail(&conn, &err.to_string()) {
            tracing::warn!(source = %source.name, error = %record_err, "failed to record sync failure");
        }
    }
    if let Err(err) = guard.release(&conn) {
        tracing::warn!(source = %source.name, error = %err, "failed to release sync claim");
    }
    result
}

/// Write the pages and close the run as `'ok'` in one transaction, then tell the driver.
fn write(
    lifecycle: &SourceLifecycle,
    conn: &mut Connection,
    source: &SourceRecord,
    run: SyncRun,
    mut pages: SyncPages,
) -> Result<SyncSummary> {
    let on_written = pages.on_written.take();
    lifecycle.probe.at(Stage::Writing);
    let started = Instant::now();
    let written = conn.transaction().map_err(VaultError::from).and_then(|tx| {
        let summary = write_pages(&tx, source.id, &source.name, pages)?;
        run.succeed(&tx, &summary)?;
        tx.commit()?;
        Ok(summary)
    });
    if let Some(on_written) = on_written {
        on_written(written.is_ok(), started.elapsed());
    }
    written
}

/// Load the Source, take its claim and open its Sync run, in one blocking hop. The guard is
/// built here, next to the row it guards, so no await separates them.
fn begin(lifecycle: &SourceLifecycle, target: Target) -> Result<Option<Begun>> {
    let db_path = lifecycle.config.db_path();
    match target {
        Target::Name(name) => {
            if !db_path.exists() {
                return Err(VaultError::NotFound(format!("source '{name}' not found")));
            }
            let conn = open_rw_db(&db_path)?;
            let id = load_source_by_name(&conn, &name)?.id;
            begin_on(lifecycle, &conn, id)
        }
        Target::Record { id, name } => {
            let conn = open_rw_db(&db_path)?;
            let exists: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM sources WHERE id = ?1)",
                [id],
                |r| r.get(0),
            )?;
            if !exists {
                return Err(VaultError::NotFound(format!("source '{name}' not found")));
            }
            begin_on(lifecycle, &conn, id)
        }
    }
}

fn begin_on(lifecycle: &SourceLifecycle, conn: &Connection, id: i64) -> Result<Option<Begun>> {
    lifecycle.probe.at(Stage::Loaded);
    let Some(guard) = ClaimGuard::acquire(conn, lifecycle.config.db_path(), id, &lifecycle.claims)?
    else {
        return Ok(None);
    };
    lifecycle.probe.at(Stage::Claimed);
    // Reload so the Sync uses the Source's current settings. On an error the guard drops
    // here and releases the claim.
    let source = load_source_by_id(conn, id)?;
    let run = SyncRun::open(conn, id)?;
    Ok(Some(Begun { source, guard, run }))
}
