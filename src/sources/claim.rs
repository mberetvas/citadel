//! Sync claims: the vault-wide exclusive right to Sync one Source ([ADR-0001]).
//!
//! A claim is a row in `sync_claims`. Inserting it takes the claim and deleting it
//! releases it, so every process sharing the vault sees the same claims. A claim left
//! behind by a crash may be taken over when its holder pid is not running, when the pid
//! is ours but the instance token is not (a previous run reused the pid, e.g. pid 1 in
//! the container), or when it is older than [`MAX_CLAIM_AGE_SECS`].
//!
//! [ADR-0001]: ../../../Docs/adr/0001-database-sync-claim.md

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::open_rw_db;
use crate::error::Result;

/// Hard age limit after which any claim may be taken over.
pub(crate) const MAX_CLAIM_AGE_SECS: i64 = 6 * 3_600;

const INTERRUPTED_MESSAGE: &str = "sync was interrupted before it finished";

/// The identity and clock a process claims with. Tests swap the clock and liveness check.
#[derive(Clone)]
pub(crate) struct ClaimEnv {
    pub(crate) pid: i64,
    pub(crate) token: String,
    /// Current time in Unix seconds.
    pub(crate) now: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Whether a process with this pid is running on this host.
    pub(crate) pid_alive: Arc<dyn Fn(i64) -> bool + Send + Sync>,
}

impl std::fmt::Debug for ClaimEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimEnv")
            .field("pid", &self.pid)
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl ClaimEnv {
    /// This process: its pid, its instance token, the system clock and a real liveness check.
    pub(crate) fn current() -> Self {
        Self {
            pid: i64::from(std::process::id()),
            token: instance_token().to_string(),
            now: Arc::new(unix_now),
            pid_alive: Arc::new(pid_is_running),
        }
    }
}

/// A random token generated once per process.
fn instance_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        format!("{:016x}", hasher.finish())
    })
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Whether `pid` names a running process on this host (any owner).
#[cfg(target_os = "linux")]
fn pid_is_running(pid: i64) -> bool {
    // The slim container image has no `ps`; `/proc` is always there on Linux.
    pid > 0 && std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Whether `pid` names a running process on this host (any owner).
#[cfg(all(unix, not(target_os = "linux")))]
fn pid_is_running(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        // If `ps` cannot run, assume alive and rely on the age limit.
        .map_or(true, |status| status.success())
}

#[cfg(not(unix))]
fn pid_is_running(_pid: i64) -> bool {
    true
}

/// Try to take the claim on `source_id`. Returns `false` when another live Sync holds it.
pub(crate) fn try_acquire(conn: &Connection, source_id: i64, env: &ClaimEnv) -> Result<bool> {
    let now = (env.now)();
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO sync_claims (source_id, pid, instance_token, claimed_at)
         VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%SZ', ?4, 'unixepoch'))",
        params![source_id, env.pid, env.token, now],
    )?;
    if inserted == 1 {
        return Ok(true);
    }

    let holder: Option<(i64, String, i64)> = conn
        .query_row(
            "SELECT pid, instance_token, unixepoch(claimed_at)
             FROM sync_claims WHERE source_id = ?1",
            [source_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((holder_pid, holder_token, claimed_at)) = holder else {
        // Released between our insert and our read; the caller may simply try again later.
        return Ok(false);
    };

    let reused_pid = holder_pid == env.pid && holder_token != env.token;
    let expired = now - claimed_at >= MAX_CLAIM_AGE_SECS;
    let held_by_us = holder_pid == env.pid && holder_token == env.token;
    // Our own live claim is never abandoned, however old: another task of ours is running it.
    let abandoned = !held_by_us && (reused_pid || expired || !(env.pid_alive)(holder_pid));
    if !abandoned {
        return Ok(false);
    }

    // Guarded by the old holder, so two processes racing to take over cannot both win.
    let taken = conn.execute(
        "UPDATE sync_claims
         SET pid = ?2, instance_token = ?3,
             claimed_at = strftime('%Y-%m-%dT%H:%M:%SZ', ?4, 'unixepoch')
         WHERE source_id = ?1 AND pid = ?5 AND instance_token = ?6",
        params![source_id, env.pid, env.token, now, holder_pid, holder_token],
    )?;
    if taken == 1 {
        tracing::warn!(source_id, holder_pid, "took over an abandoned sync claim");
        reset_interrupted_sync(conn, source_id)?;
    }
    Ok(taken == 1)
}

/// A held Sync claim. It is built in the same synchronous code that commits the claim row,
/// so no await can separate the row from its guard; dropping the guard releases the claim.
#[derive(Debug)]
pub(crate) struct ClaimGuard {
    db_path: PathBuf,
    env: ClaimEnv,
    source_id: i64,
    held: bool,
}

impl ClaimGuard {
    /// Take the claim on `source_id`, or `None` when another live Sync holds it.
    pub(crate) fn acquire(
        conn: &Connection,
        db_path: PathBuf,
        source_id: i64,
        env: &ClaimEnv,
    ) -> Result<Option<Self>> {
        if !try_acquire(conn, source_id, env)? {
            return Ok(None);
        }
        Ok(Some(Self {
            db_path,
            env: env.clone(),
            source_id,
            held: true,
        }))
    }

    /// Release the claim now, on `conn`.
    pub(crate) fn release(mut self, conn: &Connection) -> Result<()> {
        self.held = false;
        release(conn, self.source_id, &self.env)
    }

    /// The claim row is already gone: deleting the Source cascaded it away.
    pub(crate) fn forget(mut self) {
        self.held = false;
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        let (db_path, env, id) = (self.db_path.clone(), self.env.clone(), self.source_id);
        let release = move || {
            let result = open_rw_db(&db_path).and_then(|conn| release(&conn, id, &env));
            if let Err(err) = result {
                tracing::warn!(source_id = id, error = %err, "failed to release sync claim");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(release);
            }
            Err(_) => release(),
        }
    }
}

/// Release our claim on `source_id` and reset any Sync status it left unfinished.
pub(crate) fn release(conn: &Connection, source_id: i64, env: &ClaimEnv) -> Result<()> {
    let deleted = conn.execute(
        "DELETE FROM sync_claims WHERE source_id = ?1 AND pid = ?2 AND instance_token = ?3",
        params![source_id, env.pid, env.token],
    )?;
    if deleted == 1 {
        reset_interrupted_sync(conn, source_id)?;
    }
    Ok(())
}

/// A Sync that stopped without finishing leaves `sources.status = 'syncing'` and a
/// `'running'` Sync run; mark both as errors.
fn reset_interrupted_sync(conn: &Connection, source_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE sync_runs
         SET status = 'error', error_message = ?2,
             finished_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE source_id = ?1 AND status = 'running'",
        params![source_id, INTERRUPTED_MESSAGE],
    )?;
    conn.execute(
        "UPDATE sources SET status = 'error', last_error = ?2
         WHERE id = ?1 AND status = 'syncing'",
        params![source_id, INTERRUPTED_MESSAGE],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "claim_tests.rs"]
mod tests;
