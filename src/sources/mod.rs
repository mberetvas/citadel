//! Source lifecycle: everything that changes a Source (add, update, remove, sync).
//!
//! Every entry point (CLI, MCP, UI, daemon) crosses [`SourceLifecycle`]. The local
//! folder walk and all SQLite work run on Tokio's blocking pool, so the futures
//! returned here never block a runtime worker and are `Send`.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::VaultConfig;
use crate::db::open_rw_db;
use crate::error::{Result, VaultError};
use crate::ingest::{
    load_all_sources, load_source_by_id, load_source_by_name, local, sync_local_source,
    sync_web_source, SourceKind, SourceRecord, SyncSummary, DEFAULT_MAX_DEPTH, DEFAULT_MAX_PAGES,
};

mod claim;
pub(crate) mod origin;

use claim::{ClaimEnv, ClaimGuard};

use origin::{check_private_origin, classify_origin};

const DEFAULT_REFRESH_INTERVAL_SECS: i64 = 86_400;
const MAX_ALLOWED_PAGES: u32 = 5_000;
const MAX_ALLOWED_DEPTH: u32 = 20;

/// The one interface for changing Sources.
#[derive(Debug, Clone)]
pub struct SourceLifecycle {
    config: VaultConfig,
    claims: ClaimEnv,
    probe: Probe,
}

/// A point inside a Sync's blocking work where tests can pause it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// The Source row was loaded; no claim taken yet.
    Loaded,
    /// The Sync claim row is committed.
    Claimed,
}

/// Test seam: called synchronously at each [`Stage`]. Always empty in production.
#[derive(Clone, Default)]
struct Probe(Option<std::sync::Arc<dyn Fn(Stage) + Send + Sync>>);

impl Probe {
    fn at(&self, stage: Stage) {
        if let Some(probe) = &self.0 {
            probe(stage);
        }
    }
}

impl std::fmt::Debug for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Probe(set)"
        } else {
            "Probe(none)"
        })
    }
}

/// A newly registered Source and, when synced immediately, its first Sync summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Added {
    pub source: SourceRecord,
    pub summary: Option<SyncSummary>,
}

/// Result of asking for a Sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    Synced(SyncSummary),
    /// Another Sync of this Source holds the Sync claim; nothing was done.
    AlreadySyncing,
}

/// One Source's result within [`SourceLifecycle::sync_all`].
#[derive(Debug)]
pub struct SourceSyncResult {
    pub name: String,
    pub result: Result<SyncOutcome>,
}

/// Result of asking to remove a Source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    Removed(RemovedSource),
    /// A Sync of this Source is running; nothing was removed.
    AlreadySyncing,
}

impl SyncOutcome {
    /// The summary when the Sync ran, `None` when another Sync held the claim.
    pub fn synced(self) -> Option<SyncSummary> {
        match self {
            Self::Synced(summary) => Some(summary),
            Self::AlreadySyncing => None,
        }
    }
}

impl SourceLifecycle {
    pub fn new(config: VaultConfig) -> Self {
        Self {
            config,
            claims: ClaimEnv::current(),
            probe: Probe::default(),
        }
    }

    /// Pause points for tests: `probe` runs synchronously at every [`Stage`].
    #[cfg(test)]
    pub(crate) fn with_probe(mut self, probe: impl Fn(Stage) + Send + Sync + 'static) -> Self {
        self.probe = Probe(Some(std::sync::Arc::new(probe)));
        self
    }

    /// Register a new Source and optionally run its first Sync.
    pub async fn add(&self, params: AddSourceParams, sync_now: bool) -> Result<Added> {
        let config = self.config.clone();
        let source = blocking(move || register_source_row(&config, params)).await?;
        if !sync_now {
            return Ok(Added {
                source,
                summary: None,
            });
        }

        let id = source.id;
        let summary = match self.claim_record(source).await? {
            Some(claimed) => Some(claimed.run(false).await?),
            None => None,
        };
        let config = self.config.clone();
        let source = blocking(move || {
            let conn = open_rw_db(&config.db_path())?;
            load_source_by_id(&conn, id)
        })
        .await?;
        Ok(Added { source, summary })
    }

    /// Update mutable configuration fields for an existing Source.
    pub fn update(&self, name: &str, params: UpdateSourceParams) -> Result<SourceRecord> {
        update_source(&self.config, name, params)
    }

    /// Remove a Source by name, cascading its stored Pages. Refused while it is syncing.
    pub async fn remove(&self, name: &str) -> Result<Removal> {
        let Some(mut claimed) = self.claim(name).await? else {
            return Ok(Removal::AlreadySyncing);
        };
        let config = self.config.clone();
        let name = name.to_string();
        match blocking(move || remove_source(&config, &name)).await {
            Ok(removed) => {
                // Deleting the Source cascaded its claim row away.
                if let Some(guard) = claimed.guard.take() {
                    guard.forget();
                }
                Ok(Removal::Removed(removed))
            }
            Err(err) => {
                claimed.release().await;
                Err(err)
            }
        }
    }

    /// Sync one Source by name, unless another Sync of it is running.
    pub async fn sync(&self, name: &str, prune: bool) -> Result<SyncOutcome> {
        match self.claim(name).await? {
            Some(claimed) => Ok(SyncOutcome::Synced(claimed.run(prune).await?)),
            None => Ok(SyncOutcome::AlreadySyncing),
        }
    }

    /// Sync every registered Source in name order. Each Source gets its own result, so
    /// one failure does not stop the others.
    pub async fn sync_all(&self, prune: bool) -> Result<Vec<SourceSyncResult>> {
        let config = self.config.clone();
        let sources = blocking(move || {
            if !config.db_path().exists() {
                return Ok(Vec::new());
            }
            let conn = open_rw_db(&config.db_path())?;
            load_all_sources(&conn)
        })
        .await?;

        let mut results = Vec::with_capacity(sources.len());
        for source in sources {
            let name = source.name.clone();
            let result = match self.claim_record(source).await {
                Ok(Some(claimed)) => claimed.run(prune).await.map(SyncOutcome::Synced),
                Ok(None) => Ok(SyncOutcome::AlreadySyncing),
                Err(VaultError::NotFound(_)) => {
                    tracing::info!(source = %name, "skipped: removed during sync_all");
                    continue;
                }
                Err(err) => Err(err),
            };
            results.push(SourceSyncResult { name, result });
        }
        Ok(results)
    }

    /// Take the Sync claim on a Source so its Sync can run later via [`ClaimedSync::run`].
    /// Returns `None` when another Sync holds it.
    pub async fn claim(&self, name: &str) -> Result<Option<ClaimedSync>> {
        let config = self.config.clone();
        let name = name.to_string();
        let probe = self.probe.clone();
        let source = blocking(move || {
            if !config.db_path().exists() {
                return Err(VaultError::NotFound(format!("source '{name}' not found")));
            }
            let conn = open_rw_db(&config.db_path())?;
            let source = load_source_by_name(&conn, &name)?;
            probe.at(Stage::Loaded);
            Ok(source)
        })
        .await?;
        self.claim_record(source).await
    }

    /// Claim a Source, then reload it so the Sync uses its current settings. A Source
    /// removed since `source` was loaded is `NotFound`.
    pub(crate) async fn claim_record(&self, source: SourceRecord) -> Result<Option<ClaimedSync>> {
        let config = self.config.clone();
        let env = self.claims.clone();
        let id = source.id;
        let probe = self.probe.clone();
        let lifecycle = self.clone();
        // The guard is built inside this hop, so a caller dropped while it runs cannot leave
        // a claim row without one: the dropped result releases it.
        blocking(move || {
            let conn = open_rw_db(&config.db_path())?;
            let not_found = || VaultError::NotFound(format!("source '{}' not found", source.name));
            let exists: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM sources WHERE id = ?1)",
                [id],
                |r| r.get(0),
            )?;
            if !exists {
                return Err(not_found());
            }
            let Some(guard) = ClaimGuard::acquire(&conn, config.db_path(), id, &env)? else {
                return Ok(None);
            };
            probe.at(Stage::Claimed);
            let source = load_source_by_id(&conn, id)?;
            Ok(Some(ClaimedSync {
                lifecycle,
                source,
                guard: Some(guard),
            }))
        })
        .await
    }

    /// Run one Sync of a claimed Source. Any failure, whether before or during the walk
    /// or crawl, leaves the same trace: an error Sync run and `status = 'error'`.
    async fn sync_record(&self, source: &SourceRecord, prune: bool) -> Result<SyncSummary> {
        let config = self.config.clone();
        let id = source.id;
        let last_run_before = blocking(move || {
            let conn = open_rw_db(&config.db_path())?;
            Ok(conn.query_row(
                "SELECT COALESCE(MAX(id), 0) FROM sync_runs WHERE source_id = ?1",
                [id],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await?;

        let result = self.run_sync(source, prune).await;
        if let Err(err) = &result {
            let config = self.config.clone();
            let message = err.to_string();
            let recorded = blocking(move || {
                let conn = open_rw_db(&config.db_path())?;
                record_sync_failure(&conn, id, last_run_before, &message)
            })
            .await;
            if let Err(record_err) = recorded {
                tracing::warn!(source = %source.name, error = %record_err, "failed to record sync failure");
            }
        }
        result
    }

    async fn run_sync(&self, source: &SourceRecord, prune: bool) -> Result<SyncSummary> {
        match source.kind {
            SourceKind::Local => {
                let config = self.config.clone();
                let src = source.clone();
                blocking(move || {
                    let mut conn = open_rw_db(&config.db_path())?;
                    sync_local_source(
                        &mut conn,
                        src.id,
                        &src.name,
                        Path::new(&src.origin),
                        &src.include_patterns,
                        &src.exclude_patterns,
                        prune,
                    )
                })
                .await
            }
            SourceKind::Web => {
                check_private_origin(&source.origin, self.config.allow_private_origins)?;
                sync_web_source(&self.config.db_path(), source, prune).await
            }
        }
    }
}

/// A held Sync claim on one Source. [`ClaimedSync::run`] runs the Sync and releases the
/// claim; dropping it unrun releases the claim too.
#[derive(Debug)]
pub struct ClaimedSync {
    lifecycle: SourceLifecycle,
    source: SourceRecord,
    guard: Option<ClaimGuard>,
}

impl ClaimedSync {
    /// The claimed Source.
    pub fn source(&self) -> &SourceRecord {
        &self.source
    }

    /// Run the Sync, then release the claim whether it succeeded or failed.
    ///
    /// The Sync runs as its own task: dropping the returned future (an HTTP client that
    /// disconnects, a timed-out caller) does not cancel a crawl half-way.
    pub async fn run(self, prune: bool) -> Result<SyncSummary> {
        tokio::spawn(self.run_to_completion(prune))
            .await
            .map_err(|err| VaultError::Other(format!("sync task failed: {err}")))?
    }

    async fn run_to_completion(mut self, prune: bool) -> Result<SyncSummary> {
        let result = self.lifecycle.sync_record(&self.source, prune).await;
        self.release().await;
        result
    }

    /// Release the claim now. A failure is logged, never returned: the Sync's own result
    /// is what callers report.
    pub(crate) async fn release(&mut self) {
        let Some(guard) = self.guard.take() else {
            return;
        };
        let config = self.lifecycle.config.clone();
        let id = self.source.id;
        let released = blocking(move || {
            let conn = open_rw_db(&config.db_path())?;
            guard.release(&conn)
        })
        .await;
        if let Err(err) = released {
            tracing::warn!(source_id = id, error = %err, "failed to release sync claim");
        }
    }
}

/// Run blocking work (SQLite, folder walks) on Tokio's blocking pool.
async fn blocking<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| VaultError::Other(format!("blocking task failed: {err}")))?
}

/// Parameters for registering a new source.
#[derive(Debug, Clone, Default)]
pub struct AddSourceParams {
    pub origin: String,
    pub name: Option<String>,
    pub tags: Vec<String>,
    pub refresh_interval_secs: Option<i64>,
    pub max_pages: Option<u32>,
    pub max_depth: Option<u32>,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub render_js: bool,
}

/// Partial update parameters for an existing source.
#[derive(Debug, Clone, Default)]
pub struct UpdateSourceParams {
    pub tags: Option<Vec<String>>,
    pub refresh_interval_secs: Option<i64>,
    pub max_pages: Option<u32>,
    pub max_depth: Option<u32>,
    pub include_patterns: Option<Vec<String>>,
    pub exclude_patterns: Option<Vec<String>>,
    pub render_js: Option<bool>,
}

/// Metadata returned when removing a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedSource {
    pub id: i64,
    pub name: String,
    pub pages_deleted: usize,
}

fn register_source_row(config: &VaultConfig, params: AddSourceParams) -> Result<SourceRecord> {
    let (kind, normalized_origin) = classify_origin(&params.origin)?;
    if kind == SourceKind::Web {
        check_private_origin(&normalized_origin, config.allow_private_origins)?;
    }
    let refresh_interval_secs = params
        .refresh_interval_secs
        .unwrap_or(DEFAULT_REFRESH_INTERVAL_SECS);
    let max_pages = params.max_pages.unwrap_or(DEFAULT_MAX_PAGES);
    let max_depth = params.max_depth.unwrap_or(DEFAULT_MAX_DEPTH);

    validate_source_limits(refresh_interval_secs, max_pages, max_depth)?;
    local::validate_patterns(&params.include_patterns)?;
    local::validate_patterns(&params.exclude_patterns)?;

    let tags = normalize_string_list(params.tags);
    let includes = normalize_string_list(params.include_patterns);
    let excludes = normalize_string_list(params.exclude_patterns);

    let conn = open_rw_db(&config.db_path())?;

    if let Some(existing_name) = conn
        .query_row(
            "SELECT name FROM sources WHERE origin = ?1",
            [&normalized_origin],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Err(VaultError::Validation(format!(
            "origin '{normalized_origin}' is already registered as source '{existing_name}'"
        )));
    }

    let slug = resolve_unique_slug(&conn, kind, &normalized_origin, params.name.as_deref())?;
    let tags_json = serialize_json_list(&tags)?;
    let includes_json = serialize_json_list(&includes)?;
    let excludes_json = serialize_json_list(&excludes)?;

    conn.execute(
        "INSERT INTO sources (
             name, kind, origin, tags, include_patterns, exclude_patterns,
             max_pages, max_depth, render_js, refresh_interval_secs
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            slug,
            kind.as_str(),
            normalized_origin,
            tags_json,
            includes_json,
            excludes_json,
            i64::from(max_pages),
            i64::from(max_depth),
            i64::from(params.render_js),
            refresh_interval_secs,
        ],
    )?;

    let source_id = conn.last_insert_rowid();
    load_source_by_id(&conn, source_id)
}

/// Record a failed Sync: close the Sync run it started (or add one if it failed before
/// starting one) and mark the Source as errored.
fn record_sync_failure(
    conn: &Connection,
    source_id: i64,
    last_run_before: i64,
    message: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE sync_runs
         SET status = 'error', error_message = ?2,
             finished_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE source_id = ?1 AND status = 'running'",
        params![source_id, message],
    )?;
    conn.execute(
        "INSERT INTO sync_runs (source_id, started_at, finished_at, status, error_message)
         SELECT ?1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                'error', ?3
         WHERE NOT EXISTS (SELECT 1 FROM sync_runs WHERE source_id = ?1 AND id > ?2)",
        params![source_id, last_run_before, message],
    )?;
    conn.execute(
        "UPDATE sources SET status = 'error', last_error = ?2 WHERE id = ?1",
        params![source_id, message],
    )?;
    Ok(())
}

fn validate_source_limits(
    refresh_interval_secs: i64,
    max_pages: u32,
    max_depth: u32,
) -> Result<()> {
    if refresh_interval_secs < 0 {
        return Err(VaultError::Validation(
            "refresh_interval_secs must be >= 0".into(),
        ));
    }
    if !(1..=MAX_ALLOWED_PAGES).contains(&max_pages) {
        return Err(VaultError::Validation(format!(
            "max_pages must be between 1 and {MAX_ALLOWED_PAGES}"
        )));
    }
    if !(1..=MAX_ALLOWED_DEPTH).contains(&max_depth) {
        return Err(VaultError::Validation(format!(
            "max_depth must be between 1 and {MAX_ALLOWED_DEPTH}"
        )));
    }
    Ok(())
}

fn validate_explicit_slug(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(VaultError::Validation(
            "source name must not be empty".into(),
        ));
    }
    if trimmed.starts_with('.') {
        return Err(VaultError::Validation(
            "source name must not start with '.'".into(),
        ));
    }
    let valid_chars = trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    let has_alnum = trimmed.chars().any(|ch| ch.is_ascii_alphanumeric());
    if !valid_chars || !has_alnum {
        return Err(VaultError::Validation(format!(
            "invalid source name '{trimmed}'; use ASCII letters, digits, '-', '_', or '.'"
        )));
    }
    Ok(trimmed.to_string())
}

fn slugify_text(raw: &str) -> String {
    let mut slug = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn derive_slug(kind: SourceKind, origin: &str) -> String {
    let derived = match kind {
        SourceKind::Local => {
            let folder_name = Path::new(origin)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            slugify_text(&folder_name)
        }
        SourceKind::Web => match Url::parse(origin) {
            Ok(url) => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(host) = url.host_str() {
                    parts.push(host.to_string());
                }
                if let Some(segments) = url.path_segments() {
                    for seg in segments.filter(|s| !s.is_empty()).take(2) {
                        parts.push(seg.to_string());
                    }
                }
                slugify_text(&parts.join("-"))
            }
            Err(_) => String::new(),
        },
    };

    if derived.is_empty() {
        match kind {
            SourceKind::Local => "local-source".to_string(),
            SourceKind::Web => "web-source".to_string(),
        }
    } else {
        derived
    }
}

fn resolve_unique_slug(
    conn: &Connection,
    kind: SourceKind,
    origin: &str,
    explicit_name: Option<&str>,
) -> Result<String> {
    if let Some(raw_name) = explicit_name {
        let slug = validate_explicit_slug(raw_name)?;
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM sources WHERE name = ?1",
            [&slug],
            |r| r.get(0),
        )?;
        if exists {
            return Err(VaultError::Validation(format!(
                "source name '{slug}' already exists"
            )));
        }
        return Ok(slug);
    }

    let base = derive_slug(kind, origin);
    let mut candidate = base.clone();
    let mut suffix = 2usize;

    loop {
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM sources WHERE name = ?1",
            [&candidate],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(candidate);
        }
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
}

fn normalize_string_list(items: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        let trimmed = item.trim();
        if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

fn serialize_json_list(items: &[String]) -> Result<String> {
    serde_json::to_string(items)
        .map_err(|err| VaultError::Other(format!("failed to serialize list: {err}")))
}

/// Update mutable configuration fields for an existing source.
fn update_source(
    config: &VaultConfig,
    name: &str,
    params: UpdateSourceParams,
) -> Result<SourceRecord> {
    if !config.db_path().exists() {
        return Err(VaultError::NotFound(format!("source '{name}' not found")));
    }

    let conn = open_rw_db(&config.db_path())?;
    let existing = load_source_by_name(&conn, name)?;

    let tags = params
        .tags
        .map(normalize_string_list)
        .unwrap_or(existing.tags);
    let includes = match params.include_patterns {
        Some(list) => {
            local::validate_patterns(&list)?;
            normalize_string_list(list)
        }
        None => existing.include_patterns,
    };
    let excludes = match params.exclude_patterns {
        Some(list) => {
            local::validate_patterns(&list)?;
            normalize_string_list(list)
        }
        None => existing.exclude_patterns,
    };

    let refresh_interval_secs = params
        .refresh_interval_secs
        .unwrap_or(existing.refresh_interval_secs);
    let max_pages = params.max_pages.unwrap_or(existing.max_pages);
    let max_depth = params.max_depth.unwrap_or(existing.max_depth);
    let render_js = params.render_js.unwrap_or(existing.render_js);

    validate_source_limits(refresh_interval_secs, max_pages, max_depth)?;

    let tags_json = serialize_json_list(&tags)?;
    let includes_json = serialize_json_list(&includes)?;
    let excludes_json = serialize_json_list(&excludes)?;

    conn.execute(
        "UPDATE sources
         SET tags = ?2,
             include_patterns = ?3,
             exclude_patterns = ?4,
             max_pages = ?5,
             max_depth = ?6,
             render_js = ?7,
             refresh_interval_secs = ?8
         WHERE id = ?1",
        params![
            existing.id,
            tags_json,
            includes_json,
            excludes_json,
            i64::from(max_pages),
            i64::from(max_depth),
            i64::from(render_js),
            refresh_interval_secs,
        ],
    )?;

    load_source_by_id(&conn, existing.id)
}

/// Remove a source by slug name, cascading stored pages and returning deleted count.
fn remove_source(config: &VaultConfig, name: &str) -> Result<RemovedSource> {
    if !config.db_path().exists() {
        return Err(VaultError::NotFound(format!("source '{name}' not found")));
    }

    let mut conn = open_rw_db(&config.db_path())?;
    let tx = conn.transaction()?;

    let source_id: i64 = tx
        .query_row("SELECT id FROM sources WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| VaultError::NotFound(format!("source '{name}' not found")))?;

    let pages_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM pages WHERE source_id = ?1",
        [source_id],
        |r| r.get(0),
    )?;

    tx.execute("DELETE FROM sources WHERE id = ?1", [source_id])?;
    tx.commit()?;

    Ok(RemovedSource {
        id: source_id,
        name: name.to_string(),
        pages_deleted: usize::try_from(pages_count).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests;
