//! One Sync as one task, from claim to release.
//!
//! [`SourceLifecycle::start_sync`] spawns the task at once and hands back a [`SyncHandle`].
//! The task loads the Source, takes its Sync claim, runs the Sync and releases the claim;
//! it never waits on its caller, so dropping the handle neither cancels the Sync nor leaks
//! the claim.

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::claim::ClaimGuard;
use super::{blocking, SourceLifecycle, Stage, SyncOutcome};
use crate::db::open_rw_db;
use crate::error::{Result, VaultError};
use crate::ingest::{load_source_by_id, load_source_by_name, SourceRecord, SyncSummary};

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
    // A caller that went away no longer wants the report; the Sync goes on regardless.
    let (source, guard) = match begun {
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

    let result = lifecycle.sync_record(&source, prune).await;
    let config = lifecycle.config.clone();
    let released = blocking(move || {
        let conn = open_rw_db(&config.db_path())?;
        guard.release(&conn)
    })
    .await;
    if let Err(err) = released {
        tracing::warn!(source = %source.name, error = %err, "failed to release sync claim");
    }
    result
}

/// Load the Source and take its claim, in one blocking hop. The guard is built here, next
/// to the row it guards, so no await separates them.
fn begin(
    lifecycle: &SourceLifecycle,
    target: Target,
) -> Result<Option<(SourceRecord, ClaimGuard)>> {
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

fn begin_on(
    lifecycle: &SourceLifecycle,
    conn: &rusqlite::Connection,
    id: i64,
) -> Result<Option<(SourceRecord, ClaimGuard)>> {
    lifecycle.probe.at(Stage::Loaded);
    let Some(guard) = ClaimGuard::acquire(conn, lifecycle.config.db_path(), id, &lifecycle.claims)?
    else {
        return Ok(None);
    };
    lifecycle.probe.at(Stage::Claimed);
    // Reload so the Sync uses the Source's current settings.
    let source = load_source_by_id(conn, id)?;
    Ok(Some((source, guard)))
}
