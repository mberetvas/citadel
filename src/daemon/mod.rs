pub mod info;
pub mod service;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebounceEventResult};
use rusqlite::Connection;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::VaultConfig;
use crate::db::open_rw_db;
use crate::error::{Result, VaultError};
use crate::ingest::{local::canonicalize_local_dir, map_source_record, SourceRecord, SyncSummary};
use crate::sources::{SourceLifecycle, SyncOutcome};

use info::{DaemonInfo, DaemonInfoGuard};

pub use service::{
    generate_launchd_plist, generate_systemd_unit, install_service, service_file_path,
    start_service, uninstall_service, LAUNCHD_LABEL, SYSTEMD_SERVICE_NAME,
};

/// Default web scheduler tick interval (60 seconds).
pub const DEFAULT_SCHEDULER_TICK: Duration = Duration::from_secs(60);
/// Default maximum concurrent web source syncs.
pub const DEFAULT_MAX_CONCURRENT_SYNCS: usize = 2;
/// Default maximum random jitter (in seconds) before starting a scheduled web sync.
pub const DEFAULT_MAX_JITTER_SECS: u64 = 30;
/// Default file watcher debounce window (2 seconds).
pub const DEFAULT_WATCHER_DEBOUNCE: Duration = Duration::from_secs(2);
/// Interval at which the watcher refreshes the list of registered local sources.
const WATCHER_SOURCE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Current liveness state of the background daemon for a given vault directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStatus {
    /// A daemon holds an exclusive lock on `daemon.pid` with the recorded PID.
    Running(u32),
    /// No `daemon.pid` file exists.
    Stopped,
    /// `daemon.pid` exists on disk, but no live process holds its advisory lock.
    Stale,
}

/// Exclusive advisory file lock guarding `daemon.pid` for single-instance enforcement.
#[derive(Debug)]
pub struct PidLock {
    path: PathBuf,
    _file: File,
}

impl PidLock {
    /// Acquire an exclusive advisory lock on `path` and record the current process PID.
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let existing_pid = read_pid_from_path(path);
                let detail = match existing_pid {
                    Some(pid) => format!(
                        "daemon is already running (pid {pid}, lockfile {})",
                        path.display()
                    ),
                    None => format!("daemon is already running (lockfile {})", path.display()),
                };
                return Err(VaultError::AlreadyRunning(detail));
            }
            Err(TryLockError::Error(err)) => return Err(VaultError::Io(err)),
        }

        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        writeln!(file, "{}", std::process::id())?;
        file.flush()?;
        let _ = file.sync_data();

        Ok(Self {
            path: path.to_path_buf(),
            _file: file,
        })
    }

    /// Path of the locked PID file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn read_pid_from_file(file: &mut File) -> Option<u32> {
    let mut contents = String::new();
    let _ = file.seek(SeekFrom::Start(0));
    file.read_to_string(&mut contents).ok()?;
    contents.trim().parse::<u32>().ok()
}

fn read_pid_from_path(path: &Path) -> Option<u32> {
    let contents = fs::read_to_string(path).ok()?;
    contents.trim().parse::<u32>().ok()
}

/// Inspect the status of the background daemon for `config`.
pub fn daemon_status(config: &VaultConfig) -> Result<DaemonStatus> {
    let pid_path = config.pid_path();
    if !pid_path.exists() {
        return Ok(DaemonStatus::Stopped);
    }

    // Read-only: status must work on a pid file it may not write (flock needs no write access).
    let mut file = match OpenOptions::new().read(true).open(&pid_path) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DaemonStatus::Stopped);
        }
        Err(err) => return Err(VaultError::Io(err)),
    };

    match file.try_lock() {
        Ok(()) => Ok(DaemonStatus::Stale),
        Err(TryLockError::WouldBlock) => {
            let pid = read_pid_from_file(&mut file).unwrap_or(0);
            Ok(DaemonStatus::Running(pid))
        }
        Err(TryLockError::Error(err)) => Err(VaultError::Io(err)),
    }
}

/// Write `daemon.json` for this process. It only feeds `citadel doctor`, so a
/// failure is logged and the daemon runs on without it. Keep the returned guard
/// alive for the daemon's lifetime: dropping it removes the file.
fn write_daemon_info_best_effort(path: &Path) -> Option<DaemonInfoGuard> {
    match DaemonInfo::for_current_process().and_then(|info| DaemonInfoGuard::write(path, &info)) {
        Ok(guard) => Some(guard),
        Err(err) => {
            tracing::warn!(error = %err, "could not write daemon.json; continuing without it");
            None
        }
    }
}

/// Start the daemon either in the foreground (blocking until `SIGINT`/`SIGTERM`) or detached.
///
/// Returns `Ok(Some(child_pid))` when detached, or `Ok(None)` after clean foreground shutdown.
pub fn start_daemon(config: &VaultConfig, foreground: bool) -> Result<Option<u32>> {
    config.ensure_data_dir()?;

    if foreground {
        let _lock = PidLock::acquire(&config.pid_path())?;
        let _info = write_daemon_info_best_effort(&config.daemon_info_path());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| VaultError::Other(format!("failed to build tokio runtime: {err}")))?;
        rt.block_on(run_daemon_foreground(config.clone()))?;
        return Ok(None);
    }

    start_daemon_with_exe(config, &std::env::current_exe()?)
}

/// Spawn the detached daemon from an explicit executable path (avoids Linux `/proc/self/exe` `(deleted)` paths after self-update).
pub fn start_daemon_with_exe(config: &VaultConfig, exe: &Path) -> Result<Option<u32>> {
    if let DaemonStatus::Running(pid) = daemon_status(config)? {
        return Err(VaultError::AlreadyRunning(format!(
            "daemon is already running (pid {pid})"
        )));
    }

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.log_path())?;
    let log_err = log_file.try_clone()?;

    let child = Command::new(exe)
        .arg("--data-dir")
        .arg(&config.data_dir)
        .arg("daemon")
        .arg("start")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err))
        .spawn()?;

    Ok(Some(child.id()))
}

/// Stop a running daemon cleanly via `SIGTERM`, or clean up a stale `daemon.pid`.
///
/// Returns `Ok(Some(pid))` if a running daemon was stopped, or `Ok(None)` if none was running.
pub fn stop_daemon(config: &VaultConfig) -> Result<Option<u32>> {
    match daemon_status(config)? {
        DaemonStatus::Stopped => Ok(None),
        DaemonStatus::Stale => {
            let _ = fs::remove_file(config.pid_path());
            let _ = fs::remove_file(config.daemon_info_path());
            Ok(None)
        }
        DaemonStatus::Running(pid) => {
            if pid == 0 {
                return Err(VaultError::Other(
                    "daemon lock is held, but daemon.pid does not contain a valid PID".into(),
                ));
            }

            send_sigterm(pid)?;

            for _ in 0..50 {
                if !matches!(daemon_status(config)?, DaemonStatus::Running(_)) {
                    return Ok(Some(pid));
                }
                std::thread::sleep(Duration::from_millis(100));
            }

            Err(VaultError::Other(format!(
                "sent SIGTERM to daemon (pid {pid}), but it did not exit within 5 seconds"
            )))
        }
    }
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()?;
    if !status.success() {
        return Err(VaultError::Other(format!(
            "failed to send SIGTERM to pid {pid} (exit status {status})"
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn send_sigterm(pid: u32) -> Result<()> {
    Err(VaultError::Other(format!(
        "stopping daemon pid {pid} via signal is not supported on this platform"
    )))
}

/// Run the scheduler and local folder watcher concurrently until `SIGINT` or `SIGTERM`.
pub async fn run_daemon_foreground(config: VaultConfig) -> Result<()> {
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();

    let signal_task = tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("shutdown signal received; stopping daemon tasks");
        signal_cancel.cancel();
    });

    let scheduler_cfg = config.clone();
    let scheduler_cancel = cancel.clone();
    let watcher_cfg = config;
    let watcher_cancel = cancel.clone();

    let (sched_res, watch_res) = tokio::join!(
        run_scheduler(scheduler_cfg, scheduler_cancel),
        run_watcher(watcher_cfg, watcher_cancel),
    );

    cancel.cancel();
    signal_task.abort();

    sched_res?;
    watch_res?;
    Ok(())
}

pub(crate) async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Select all `web` sources that are due for refresh at the current wall-clock time.
pub fn select_due_web_sources(conn: &Connection) -> Result<Vec<SourceRecord>> {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    select_due_web_sources_at(conn, now_secs)
}

/// Select all `web` sources that are due for refresh at `now_epoch_secs` (UTC Unix seconds).
///
/// A Source is due once `refresh_interval_secs` has passed since its last Sync attempt,
/// successful or not.
pub fn select_due_web_sources_at(
    conn: &Connection,
    now_epoch_secs: i64,
) -> Result<Vec<SourceRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, origin, tags, include_patterns, exclude_patterns,
                max_pages, max_depth, render_js, refresh_interval_secs,
                status, last_synced_at, last_error, created_at
         FROM (
             SELECT s.*,
                    -- Last attempt, failed or not, so a failing Source waits its interval.
                    MAX(COALESCE(unixepoch(s.last_synced_at), 0),
                        COALESCE((SELECT MAX(unixepoch(r.started_at))
                                  FROM sync_runs r WHERE r.source_id = s.id), 0)) AS last_attempt
             FROM sources s
         )
         WHERE kind = 'web'
           AND refresh_interval_secs > 0
           AND last_attempt + refresh_interval_secs <= ?1
         ORDER BY last_attempt ASC, id ASC",
    )?;
    let rows = stmt.query_map([now_epoch_secs], map_source_record)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Run the background web refresh scheduler with default cadence (60s tick, concurrency 2, 0–30s jitter).
pub async fn run_scheduler(config: VaultConfig, cancel: CancellationToken) -> Result<()> {
    run_scheduler_with_options(
        config,
        DEFAULT_SCHEDULER_TICK,
        DEFAULT_MAX_CONCURRENT_SYNCS,
        DEFAULT_MAX_JITTER_SECS,
        cancel,
    )
    .await
}

/// Run the web refresh scheduler with configurable tick interval, concurrency bound, and jitter.
pub async fn run_scheduler_with_options(
    config: VaultConfig,
    tick_interval: Duration,
    max_concurrency: usize,
    max_jitter_secs: u64,
    cancel: CancellationToken,
) -> Result<()> {
    let semaphore = Arc::new(Semaphore::new(max_concurrency.max(1)));
    let lifecycle = SourceLifecycle::new(config.clone());
    // Sources this scheduler has already queued, so each tick does not queue them again.
    // This is not exclusion: the Sync claim, taken once a slot is free, decides who syncs.
    let queued: Arc<std::sync::Mutex<HashSet<i64>>> = Arc::default();
    let mut tasks: JoinSet<()> = JoinSet::new();

    let mut ticker = tokio::time::interval(tick_interval.max(Duration::from_millis(10)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                break;
            }
            _ = ticker.tick() => {
                // Reap any finished sync tasks to keep memory bounded.
                while tasks.try_join_next().is_some() {}

                let due_sources = match load_due_web_sources(&config) {
                    Ok(sources) => sources,
                    Err(err) => {
                        tracing::warn!(error = %err, "scheduler failed to query due web sources");
                        continue;
                    }
                };

                for source in due_sources {
                    if cancel.is_cancelled() {
                        break;
                    }
                    if !queued.lock().map(|mut q| q.insert(source.id)).unwrap_or(false) {
                        continue;
                    }

                    let sem = Arc::clone(&semaphore);
                    let task_cancel = cancel.clone();
                    let lifecycle = lifecycle.clone();
                    let queued = Arc::clone(&queued);

                    tasks.spawn(async move {
                        scheduled_sync(&lifecycle, source.clone(), &sem, max_jitter_secs, &task_cancel).await;
                        if let Ok(mut q) = queued.lock() {
                            q.remove(&source.id);
                        }
                    });
                }
            }
        }
    }

    // Drain in-flight tasks so any active SQLite transactions finish cleanly.
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// Wait for a free slot and the jitter delay, then claim and sync one due Source.
/// Claiming only now means a Source waiting in the queue is not reported as syncing.
async fn scheduled_sync(
    lifecycle: &SourceLifecycle,
    source: SourceRecord,
    semaphore: &Arc<Semaphore>,
    max_jitter_secs: u64,
    cancel: &CancellationToken,
) {
    let permit = tokio::select! {
        _ = cancel.cancelled() => None,
        acquired = Arc::clone(semaphore).acquire_owned() => acquired.ok(),
    };
    let Some(_permit) = permit else {
        return;
    };

    let jitter = compute_jitter(max_jitter_secs, source.id);
    if !jitter.is_zero() {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(jitter) => {}
        }
    }
    if cancel.is_cancelled() {
        return;
    }

    let name = source.name.clone();
    let origin = source.origin.clone();
    let claimed = match lifecycle.claim_record(source).await {
        Ok(Some(claimed)) => claimed,
        Ok(None) => {
            tracing::debug!(source = %name, "skipped: already syncing");
            return;
        }
        Err(err) => {
            tracing::warn!(source = %name, error = %err, "scheduler failed to claim source");
            return;
        }
    };
    tracing::info!(source = %name, origin = %origin, "starting scheduled web sync");
    match claimed.run(false).await {
        Ok(summary) => log_sync_summary(&summary, "scheduled web sync complete"),
        Err(err) => {
            tracing::warn!(source = %name, error = %err, "scheduled web sync failed");
        }
    }
}

fn log_sync_summary(summary: &SyncSummary, message: &str) {
    tracing::info!(
        source = %summary.source_name,
        added = summary.pages_added,
        updated = summary.pages_updated,
        unchanged = summary.pages_unchanged,
        stale = summary.pages_stale,
        failed = summary.pages_failed,
        "{message}"
    );
    for line in sync_warning_lines(summary) {
        tracing::warn!("{line}");
    }
}

/// One log line per warning in a Sync summary, each starting with the Source name.
fn sync_warning_lines(summary: &SyncSummary) -> Vec<String> {
    summary
        .warnings
        .iter()
        .map(|warning| format!("{}: {warning}", summary.source_name))
        .collect()
}

fn load_due_web_sources(config: &VaultConfig) -> Result<Vec<SourceRecord>> {
    let db_path = config.db_path();
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = open_rw_db(&db_path)?;
    select_due_web_sources(&conn)
}

fn compute_jitter(max_jitter_secs: u64, source_id: i64) -> Duration {
    if max_jitter_secs == 0 {
        return Duration::ZERO;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    let mix = nanos ^ (source_id.unsigned_abs().wrapping_mul(2_654_435_761));
    Duration::from_secs(mix % (max_jitter_secs + 1))
}

/// Run the local Markdown folder watcher with the default 2-second debounce window.
pub async fn run_watcher(config: VaultConfig, cancel: CancellationToken) -> Result<()> {
    run_watcher_with_debounce(config, DEFAULT_WATCHER_DEBOUNCE, cancel).await
}

/// Run the local Markdown folder watcher with a custom debounce duration.
pub async fn run_watcher_with_debounce(
    config: VaultConfig,
    debounce: Duration,
    cancel: CancellationToken,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<DebounceEventResult>();
    let mut debouncer = new_debouncer(debounce, move |res: DebounceEventResult| {
        let _ = tx.send(res);
    })
    .map_err(|err| VaultError::Other(format!("failed to initialize file watcher: {err}")))?;

    let lifecycle = SourceLifecycle::new(config.clone());
    let mut watched_roots: HashMap<PathBuf, SourceRecord> = HashMap::new();
    refresh_watched_local_sources(&config, &mut debouncer, &mut watched_roots);
    // Sources that changed while another Sync held their claim; each gets one follow-up.
    let mut dirty: HashSet<i64> = HashSet::new();

    let mut refresh_ticker = tokio::time::interval(WATCHER_SOURCE_REFRESH_INTERVAL);
    refresh_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the immediate initial tick since we already loaded sources above.
    refresh_ticker.tick().await;

    let mut retry_ticker = tokio::time::interval(debounce.max(Duration::from_millis(10)));
    retry_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                break;
            }
            _ = refresh_ticker.tick() => {
                refresh_watched_local_sources(&config, &mut debouncer, &mut watched_roots);
                dirty.retain(|id| watched_roots.values().any(|s| s.id == *id));
            }
            _ = retry_ticker.tick(), if !dirty.is_empty() => {
                let pending: Vec<i64> = dirty.iter().copied().collect();
                for source_id in pending {
                    if watcher_sync(&lifecycle, &watched_roots, source_id).await {
                        dirty.remove(&source_id);
                    }
                }
            }
            maybe_res = rx.recv() => {
                let Some(event_res) = maybe_res else {
                    break;
                };
                match event_res {
                    Ok(events) => {
                        for source_id in match_affected_sources(&events, &watched_roots) {
                            if !watcher_sync(&lifecycle, &watched_roots, source_id).await {
                                dirty.insert(source_id);
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "file watcher reported error");
                    }
                }
            }
        }
    }

    drop(debouncer);
    Ok(())
}

/// Sync one watched Source. Returns `false` when another Sync holds its claim, so the
/// caller can queue one follow-up pass.
async fn watcher_sync(
    lifecycle: &SourceLifecycle,
    watched_roots: &HashMap<PathBuf, SourceRecord>,
    source_id: i64,
) -> bool {
    let Some(name) = watched_roots
        .values()
        .find(|s| s.id == source_id)
        .map(|s| s.name.clone())
    else {
        return true;
    };
    match lifecycle.sync(&name, false).await {
        Ok(SyncOutcome::Synced(summary)) => {
            log_sync_summary(&summary, "watcher re-indexed local source");
            true
        }
        Ok(SyncOutcome::AlreadySyncing) => {
            tracing::debug!(source = %name, "already syncing; queued a follow-up pass");
            false
        }
        Err(err) => {
            tracing::warn!(source = %name, error = %err, "watcher local sync failed");
            true
        }
    }
}

fn load_local_sources(config: &VaultConfig) -> Result<Vec<SourceRecord>> {
    let db_path = config.db_path();
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = open_rw_db(&db_path)?;
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, origin, tags, include_patterns, exclude_patterns,
                max_pages, max_depth, render_js, refresh_interval_secs,
                status, last_synced_at, last_error, created_at
         FROM sources
         WHERE kind = 'local'
         ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([], map_source_record)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn refresh_watched_local_sources(
    config: &VaultConfig,
    debouncer: &mut notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
    watched_roots: &mut HashMap<PathBuf, SourceRecord>,
) {
    let sources = match load_local_sources(config) {
        Ok(list) => list,
        Err(err) => {
            tracing::warn!(error = %err, "watcher failed to load local sources");
            return;
        }
    };

    let mut desired: HashMap<PathBuf, SourceRecord> = HashMap::new();
    for source in sources {
        match canonicalize_local_dir(Path::new(&source.origin)) {
            Ok(canon) => {
                desired.insert(canon, source);
            }
            Err(err) => {
                tracing::warn!(
                    source = %source.name,
                    origin = %source.origin,
                    error = %err,
                    "skipping unavailable local source directory in watcher"
                );
            }
        }
    }

    // Unwatch directories that are no longer registered.
    let stale_roots: Vec<PathBuf> = watched_roots
        .keys()
        .filter(|path| !desired.contains_key(*path))
        .cloned()
        .collect();
    for path in stale_roots {
        let _ = debouncer.watcher().unwatch(&path);
        watched_roots.remove(&path);
    }

    // Watch newly registered directories and keep SourceRecord metadata fresh.
    for (path, source) in desired {
        if !watched_roots.contains_key(&path) {
            if let Err(err) = debouncer.watcher().watch(&path, RecursiveMode::Recursive) {
                tracing::warn!(
                    source = %source.name,
                    path = %path.display(),
                    error = %err,
                    "failed to register directory with file watcher"
                );
                continue;
            }
            tracing::debug!(source = %source.name, path = %path.display(), "watching local source");
        }
        watched_roots.insert(path, source);
    }
}

fn match_affected_sources(
    events: &[notify_debouncer_mini::DebouncedEvent],
    watched_roots: &HashMap<PathBuf, SourceRecord>,
) -> Vec<i64> {
    let mut affected: HashSet<i64> = HashSet::new();

    for event in events {
        let candidate = canonicalize_event_path(&event.path);
        for (root, source) in watched_roots {
            let rel = match candidate.strip_prefix(root) {
                Ok(rel) => rel,
                Err(_) => match event.path.strip_prefix(root) {
                    Ok(rel) => rel,
                    Err(_) => continue,
                },
            };

            if is_ignored_relative_path(rel) || !is_markdown_path(rel) {
                continue;
            }

            affected.insert(source.id);
        }
    }

    let mut ids: Vec<i64> = affected.into_iter().collect();
    ids.sort_unstable();
    ids
}

fn canonicalize_event_path(path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return canon;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(canon_parent) = fs::canonicalize(parent) {
            return canon_parent.join(name);
        }
    }
    path.to_path_buf()
}

fn is_ignored_relative_path(rel: &Path) -> bool {
    for component in rel.components() {
        if let Component::Normal(part) = component {
            let s = part.to_string_lossy();
            if s.starts_with('.') || s == "node_modules" {
                return true;
            }
        }
    }
    false
}

fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_rw_db;
    use crate::error::VaultError;
    use crate::sources::{AddSourceParams, SourceLifecycle};
    use tempfile::tempdir;

    fn summary_with_warnings(warnings: Vec<String>) -> SyncSummary {
        SyncSummary {
            source_name: "dlthub".to_string(),
            warnings,
            ..Default::default()
        }
    }

    #[test]
    fn sync_warning_lines_name_the_source() {
        assert!(sync_warning_lines(&summary_with_warnings(Vec::new())).is_empty());

        let lines = sync_warning_lines(&summary_with_warnings(vec![
            "first warning".to_string(),
            "second warning".to_string(),
        ]));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("dlthub") && lines[0].contains("first warning"));
        assert!(lines[1].starts_with("dlthub") && lines[1].contains("second warning"));
    }

    #[test]
    fn selects_only_due_web_sources() {
        let dir = tempdir().expect("tempdir");
        let conn = open_rw_db(&dir.path().join("vault.db")).expect("open_rw_db");

        // Reference time: 2026-11-21T12:00:00Z
        let now_epoch: i64 = conn
            .query_row("SELECT unixepoch('2026-11-21T12:00:00Z')", [], |r| r.get(0))
            .expect("unixepoch");

        // 1. Overdue web source (last synced 2h ago, interval 1h) -> DUE
        conn.execute(
            "INSERT INTO sources (name, kind, origin, refresh_interval_secs, last_synced_at)
             VALUES ('overdue-web', 'web', 'https://overdue.example.com', 3600, '2026-11-21T10:00:00Z')",
            [],
        )
        .expect("insert overdue web");

        // 2. Never-synced web source with interval > 0 -> DUE
        conn.execute(
            "INSERT INTO sources (name, kind, origin, refresh_interval_secs, last_synced_at)
             VALUES ('never-synced-web', 'web', 'https://never.example.com', 86400, NULL)",
            [],
        )
        .expect("insert never-synced web");

        // 3. Recently synced web source (last synced 10m ago, interval 1h) -> SKIP
        conn.execute(
            "INSERT INTO sources (name, kind, origin, refresh_interval_secs, last_synced_at)
             VALUES ('recent-web', 'web', 'https://recent.example.com', 3600, '2026-11-21T11:50:00Z')",
            [],
        )
        .expect("insert recent web");

        // 4. Disabled web source (refresh_interval_secs = 0) -> SKIP
        conn.execute(
            "INSERT INTO sources (name, kind, origin, refresh_interval_secs, last_synced_at)
             VALUES ('disabled-web', 'web', 'https://disabled.example.com', 0, NULL)",
            [],
        )
        .expect("insert disabled web");

        // 5. Local source (even if overdue / never synced) -> SKIP
        conn.execute(
            "INSERT INTO sources (name, kind, origin, refresh_interval_secs, last_synced_at)
             VALUES ('local-docs', 'local', '/tmp/local-docs', 3600, NULL)",
            [],
        )
        .expect("insert local");

        // 6. Web source left 'syncing' by a crashed run -> DUE; the Sync claim decides exclusion
        conn.execute(
            "INSERT INTO sources (name, kind, origin, refresh_interval_secs, status, last_synced_at)
             VALUES ('syncing-web', 'web', 'https://syncing.example.com', 86400, 'syncing', NULL)",
            [],
        )
        .expect("insert syncing web");

        let due = select_due_web_sources_at(&conn, now_epoch).expect("select_due_web_sources_at");
        let mut names: Vec<String> = due.into_iter().map(|s| s.name).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["never-synced-web", "overdue-web", "syncing-web"]
        );
    }

    #[test]
    fn failed_sync_waits_for_refresh_interval() {
        let dir = tempdir().expect("tempdir");
        let conn = open_rw_db(&dir.path().join("vault.db")).expect("open_rw_db");
        let now_epoch: i64 = 1_800_000_000;
        // Never synced successfully, but its last attempt failed 10 minutes ago.
        conn.execute(
            "INSERT INTO sources (name, kind, origin, refresh_interval_secs, status, last_synced_at)
             VALUES ('refused', 'web', 'http://10.0.0.5/docs', 3600, 'error', NULL)",
            [],
        )
        .expect("insert refused");
        conn.execute(
            "INSERT INTO sync_runs (source_id, started_at, finished_at, status, error_message)
             VALUES (1, strftime('%Y-%m-%dT%H:%M:%SZ', ?1, 'unixepoch'),
                     strftime('%Y-%m-%dT%H:%M:%SZ', ?1, 'unixepoch'), 'error', 'refused')",
            [now_epoch - 600],
        )
        .expect("insert failed run");

        let due = select_due_web_sources_at(&conn, now_epoch).expect("due");
        assert!(due.is_empty(), "retried before its interval: {due:?}");
        let due_later = select_due_web_sources_at(&conn, now_epoch + 3_000).expect("due later");
        assert_eq!(due_later.len(), 1);
    }

    #[test]
    fn start_daemon_with_exe_spawns_explicit_executable_path() {
        let vault_dir = tempdir().expect("vault tempdir");
        let config = VaultConfig::new(vault_dir.path());
        let missing_exe = vault_dir.path().join("nonexistent-citadel-bin");
        let err = start_daemon_with_exe(&config, &missing_exe)
            .expect_err("nonexistent explicit exe must fail with io NotFound");
        assert!(
            matches!(err, VaultError::Io(ref io_err) if io_err.kind() == std::io::ErrorKind::NotFound),
            "expected NotFound Io error from explicit exe path, got: {err}"
        );
    }

    #[test]
    fn daemon_info_write_failure_is_not_fatal() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("missing-dir").join("daemon.json");
        assert!(write_daemon_info_best_effort(&path).is_none());
        let ok_path = dir.path().join("daemon.json");
        let guard = write_daemon_info_best_effort(&ok_path);
        assert!(guard.is_some() && ok_path.exists());
        drop(guard);
        assert!(!ok_path.exists());
    }

    #[test]
    fn pid_lock_prevents_second_instance() {
        let dir = tempdir().expect("tempdir");
        let config = VaultConfig::new(dir.path().join("nested"));
        let pid_path = config.pid_path();

        assert_eq!(
            daemon_status(&config).expect("stopped status"),
            DaemonStatus::Stopped
        );

        let lock = PidLock::acquire(&pid_path).expect("first lock should succeed");
        assert!(pid_path.exists());
        let recorded = std::fs::read_to_string(&pid_path).expect("read pid file");
        assert_eq!(recorded.trim(), std::process::id().to_string());
        assert_eq!(
            daemon_status(&config).expect("running status"),
            DaemonStatus::Running(std::process::id())
        );

        let err = PidLock::acquire(&pid_path).expect_err("second lock must fail");
        assert!(
            matches!(err, VaultError::AlreadyRunning(ref msg) if msg.contains(&std::process::id().to_string())),
            "expected AlreadyRunning mentioning PID, got: {err:?}"
        );

        drop(lock);
        assert!(!pid_path.exists(), "daemon.pid should be removed on drop");
        assert_eq!(
            daemon_status(&config).expect("stopped after drop"),
            DaemonStatus::Stopped
        );

        // Write a stale unlocked pid file and verify DaemonStatus::Stale
        std::fs::write(&pid_path, "999999\n").expect("write stale pid");
        assert_eq!(
            daemon_status(&config).expect("stale status"),
            DaemonStatus::Stale
        );

        let reacquired = PidLock::acquire(&pid_path).expect("lock should succeed over stale file");
        drop(reacquired);
    }

    #[tokio::test]
    async fn local_watcher_reindexes_modified_markdown_file() {
        let vault_dir = tempdir().expect("vault tempdir");
        let docs_dir = tempdir().expect("docs tempdir");
        let md_path = docs_dir.path().join("guide.md");
        std::fs::write(&md_path, "# Guide\n\nInitial version.\n").expect("write initial md");

        let config = VaultConfig::new(vault_dir.path());
        SourceLifecycle::new(config.clone())
            .add(
                AddSourceParams {
                    origin: docs_dir.path().to_string_lossy().into_owned(),
                    name: Some("watched-docs".to_string()),
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("add should succeed");

        let conn = open_rw_db(&config.db_path()).expect("open_rw_db");
        let initial_hash: String = conn
            .query_row(
                "SELECT content_hash FROM pages WHERE uri = 'guide.md'",
                [],
                |r| r.get(0),
            )
            .expect("initial hash");
        drop(conn);

        let cancel = CancellationToken::new();
        let watcher_handle = tokio::spawn(run_watcher_with_debounce(
            config.clone(),
            Duration::from_millis(150),
            cancel.clone(),
        ));

        // Give watcher a brief moment to register the directory.
        tokio::time::sleep(Duration::from_millis(200)).await;

        std::fs::write(
            &md_path,
            "# Guide\n\nUpdated version with new watcher content.\n",
        )
        .expect("update md");

        // Poll up to 4 seconds for the debounced re-index to complete.
        let mut updated_hash = initial_hash.clone();
        let mut revision_count: i64 = 0;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let conn = open_rw_db(&config.db_path()).expect("open_rw_db");
            updated_hash = conn
                .query_row(
                    "SELECT content_hash FROM pages WHERE uri = 'guide.md'",
                    [],
                    |r| r.get(0),
                )
                .expect("query updated hash");
            revision_count = conn
                .query_row("SELECT COUNT(*) FROM page_revisions", [], |r| r.get(0))
                .expect("query revisions");
            if updated_hash != initial_hash && revision_count > 0 {
                break;
            }
        }

        cancel.cancel();
        watcher_handle
            .await
            .expect("watcher task join")
            .expect("watcher clean shutdown");

        assert_ne!(
            updated_hash, initial_hash,
            "content_hash should change after file edit"
        );
        assert_eq!(
            revision_count, 1,
            "page_revisions should record the change diff"
        );
    }

    /// The pid of a process that has already exited.
    fn dead_pid() -> i64 {
        let mut child = Command::new("true").spawn().expect("spawn true");
        let pid = i64::from(child.id());
        child.wait().expect("wait true");
        pid
    }

    #[tokio::test]
    async fn scheduler_picks_source_with_dead_claim() {
        use axum::{response::Html, routing::get, Router};

        let app = Router::new().route(
            "/docs/",
            get(|| async {
                Html("<html><body><main><h1>Docs</h1><p>Up.</p></main></body></html>")
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let vault_dir = tempdir().expect("vault tempdir");
        let config = VaultConfig::new(vault_dir.path()).allowing_private_origins();
        let added = SourceLifecycle::new(config.clone())
            .add(
                AddSourceParams {
                    origin: format!("http://{addr}/docs/"),
                    name: Some("crashed".into()),
                    ..Default::default()
                },
                false,
            )
            .await
            .expect("register");

        // A Sync that crashed: status stuck at 'syncing' and a claim held by a dead pid.
        let conn = open_rw_db(&config.db_path()).expect("open_rw_db");
        conn.execute(
            "UPDATE sources SET status = 'syncing' WHERE id = ?1",
            [added.source.id],
        )
        .expect("stuck status");
        conn.execute(
            "INSERT INTO sync_claims (source_id, pid, instance_token, claimed_at)
             VALUES (?1, ?2, 'crashed-run', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))",
            rusqlite::params![added.source.id, dead_pid()],
        )
        .expect("dead claim");
        drop(conn);

        let cancel = CancellationToken::new();
        let scheduler = tokio::spawn(run_scheduler_with_options(
            config.clone(),
            Duration::from_millis(20),
            1,
            0,
            cancel.clone(),
        ));

        let mut state = (String::new(), 1_i64);
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let conn = open_rw_db(&config.db_path()).expect("open_rw_db");
            state = conn
                .query_row(
                    "SELECT s.status, (SELECT COUNT(*) FROM sync_claims c WHERE c.source_id = s.id)
                     FROM sources s WHERE s.id = ?1",
                    [added.source.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .expect("query state");
            if state == ("ok".to_string(), 0) {
                break;
            }
        }
        cancel.cancel();
        scheduler.await.expect("join").expect("scheduler shutdown");
        server.abort();
        assert_eq!(state, ("ok".to_string(), 0));
    }

    #[tokio::test]
    async fn watcher_runs_follow_up_after_already_syncing() {
        let vault_dir = tempdir().expect("vault tempdir");
        let docs_dir = tempdir().expect("docs tempdir");
        let md_path = docs_dir.path().join("guide.md");
        std::fs::write(&md_path, "# Guide\n\nInitial version.\n").expect("write md");

        let config = VaultConfig::new(vault_dir.path());
        let lifecycle = SourceLifecycle::new(config.clone());
        lifecycle
            .add(
                AddSourceParams {
                    origin: docs_dir.path().to_string_lossy().into_owned(),
                    name: Some("busy-docs".to_string()),
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("add");

        // Another Sync holds the claim while the file changes.
        let held = lifecycle
            .claim("busy-docs")
            .await
            .expect("claim")
            .expect("claim is free");

        let cancel = CancellationToken::new();
        let watcher = tokio::spawn(run_watcher_with_debounce(
            config.clone(),
            Duration::from_millis(100),
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(&md_path, "# Guide\n\nChanged while busy.\n").expect("update md");

        // Let the watcher see the change and get AlreadySyncing.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let read_body = || -> String {
            open_rw_db(&config.db_path())
                .expect("open_rw_db")
                .query_row(
                    "SELECT content_md FROM pages WHERE uri = 'guide.md'",
                    [],
                    |r| r.get(0),
                )
                .expect("body")
        };
        assert!(
            read_body().contains("Initial version"),
            "claim held: no sync yet"
        );

        drop(held);
        let mut body = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            body = read_body();
            if body.contains("Changed while busy") {
                break;
            }
        }
        cancel.cancel();
        watcher.await.expect("join").expect("watcher shutdown");
        assert!(
            body.contains("Changed while busy"),
            "follow-up sync ran: {body}"
        );
    }
}
