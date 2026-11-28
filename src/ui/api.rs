use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::open_rw_db;
use crate::db::readonly::{execute_readonly_query, QueryResult};
use crate::error::{Result, VaultError};
use crate::ingest::{self, SourceRecord, SourceSummary, SyncSummary};
use crate::sources::{AddSourceParams, Removal, RemovedSource, SourceLifecycle};
use crate::ui::UiState;
use crate::update::check::{read_status_and_claim_refresh, spawn_background_refresh, UpdateStatus};

/// HTTP error wrapper mapping [`VaultError`] variants to structured JSON responses.
#[derive(Debug)]
pub struct ApiError(pub VaultError);

impl From<VaultError> for ApiError {
    fn from(err: VaultError) -> Self {
        Self(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            VaultError::NotFound(_) => StatusCode::NOT_FOUND,
            VaultError::Validation(_) | VaultError::InvalidQuery(_) => StatusCode::BAD_REQUEST,
            VaultError::QueryTimeout(_) => StatusCode::REQUEST_TIMEOUT,
            VaultError::AlreadyRunning(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.0.to_string() }))).into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct CreateSourceRequest {
    pub origin: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub refresh_interval_secs: Option<i64>,
    #[serde(default)]
    pub max_pages: Option<u32>,
    #[serde(default)]
    pub max_depth: Option<u32>,
    #[serde(default)]
    pub include_patterns: Vec<String>,
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    #[serde(default)]
    pub render_js: bool,
    #[serde(default = "default_sync_now")]
    pub sync_now: bool,
}

fn default_sync_now() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct CreateSourceResponse {
    pub source: SourceRecord,
    pub summary: Option<SyncSummary>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SyncQuery {
    #[serde(default)]
    pub prune: bool,
}

#[derive(Debug, Serialize)]
pub struct SyncAcceptedResponse {
    pub status: &'static str,
    pub source_id: i64,
    pub source: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListPagesQuery {
    pub status: Option<String>,
    pub q: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageListItem {
    pub id: i64,
    pub source_id: i64,
    pub uri: String,
    pub title: String,
    pub word_count: i64,
    pub status: String,
    pub http_status: Option<i64>,
    pub error_message: Option<String>,
    pub first_seen_at: String,
    pub last_checked_at: String,
    pub last_changed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageSectionItem {
    pub id: i64,
    pub ordinal: i64,
    pub heading: String,
    pub heading_path: String,
    pub level: i64,
    pub content_md: String,
    pub word_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageDetailResponse {
    pub id: i64,
    pub source_id: i64,
    pub source_name: String,
    pub uri: String,
    pub title: String,
    pub content_md: String,
    pub content_hash: String,
    pub word_count: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub status: String,
    pub http_status: Option<i64>,
    pub error_message: Option<String>,
    pub first_seen_at: String,
    pub last_checked_at: String,
    pub last_changed_at: String,
    pub sections: Vec<PageSectionItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageDiffResponse {
    pub page_id: i64,
    pub uri: String,
    pub title: String,
    pub has_revision: bool,
    pub previous_hash: Option<String>,
    pub new_hash: Option<String>,
    pub unified_diff: Option<String>,
    pub changed_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    pub sql: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/health`
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// `GET /api/version`
pub async fn get_version(State(state): State<UiState>) -> Json<UpdateStatus> {
    let (status, refresh) = read_status_and_claim_refresh(&state.config, (state.clock)());
    if refresh {
        spawn_background_refresh(
            state.config.update_check_path(),
            state.config.update_api_url.clone(),
        );
    }
    Json(status)
}

/// `GET /api/sources`
pub async fn list_sources(State(state): State<UiState>) -> ApiResult<Json<Vec<SourceSummary>>> {
    let sources = ingest::list_sources(&state.config)?;
    Ok(Json(sources))
}

/// `POST /api/sources`
pub async fn create_source(
    State(state): State<UiState>,
    Json(payload): Json<CreateSourceRequest>,
) -> ApiResult<(StatusCode, Json<CreateSourceResponse>)> {
    let params = AddSourceParams {
        origin: payload.origin,
        name: payload.name,
        tags: payload.tags,
        refresh_interval_secs: payload.refresh_interval_secs,
        max_pages: payload.max_pages,
        max_depth: payload.max_depth,
        include_patterns: payload.include_patterns,
        exclude_patterns: payload.exclude_patterns,
        render_js: payload.render_js,
    };

    let added = SourceLifecycle::new(state.config.clone())
        .add(params, payload.sync_now)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreateSourceResponse {
            source: added.source,
            summary: added.summary,
        }),
    ))
}

/// `DELETE /api/sources/{id}`
pub async fn delete_source(
    State(state): State<UiState>,
    Path(id_or_slug): Path<String>,
) -> ApiResult<(StatusCode, Json<RemovedSource>)> {
    let (_source_id, source_name) = resolve_source_in_vault(&state.config, &id_or_slug)?;
    match SourceLifecycle::new(state.config.clone())
        .remove(&source_name)
        .await?
    {
        Removal::Removed(removed) => Ok((StatusCode::OK, Json(removed))),
        Removal::AlreadySyncing => Err(already_syncing(&source_name)),
    }
}

fn already_syncing(name: &str) -> ApiError {
    ApiError(VaultError::AlreadyRunning(format!(
        "source '{name}' is already syncing; try again when it finishes"
    )))
}

/// `POST /api/sources/{id}/sync`
pub async fn sync_source(
    State(state): State<UiState>,
    Path(id_or_slug): Path<String>,
    Query(query): Query<SyncQuery>,
) -> ApiResult<(StatusCode, Json<SyncAcceptedResponse>)> {
    let (source_id, source_name) = resolve_source_in_vault(&state.config, &id_or_slug)?;

    // Wait for the claim before replying, so a second request gets 409 instead of a silent
    // no-op. The Sync runs as its own task: a client that disconnects does not stop it.
    let Some(running) = SourceLifecycle::new(state.config.clone())
        .start_sync(&source_name, query.prune)
        .claimed()
        .await?
    else {
        return Err(already_syncing(&source_name));
    };
    let name_for_task = source_name.clone();
    tokio::spawn(async move {
        if let Err(err) = running.finish().await {
            tracing::error!(
                source = %name_for_task,
                error = %err,
                "background source sync failed"
            );
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(SyncAcceptedResponse {
            status: "accepted",
            source_id,
            source: source_name,
        }),
    ))
}

/// `GET /api/sources/{id}/pages`
pub async fn list_source_pages(
    State(state): State<UiState>,
    Path(id_or_slug): Path<String>,
    Query(query): Query<ListPagesQuery>,
) -> ApiResult<Json<Vec<PageListItem>>> {
    let status_filter = match query.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(s @ ("active" | "stale" | "error")) => Some(s),
        Some(other) => {
            return Err(VaultError::Validation(format!(
                "invalid page status '{other}'; expected one of: active, stale, error"
            ))
            .into());
        }
    };

    let (source_id, _source_name) = resolve_source_in_vault(&state.config, &id_or_slug)?;
    let conn = open_rw_db(&state.config.db_path())?;
    let search_q = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let pages = match search_q {
        Some(term) => match query_pages_fts(&conn, source_id, status_filter, term) {
            Ok(rows) => rows,
            Err(_) => query_pages_like(&conn, source_id, status_filter, term)?,
        },
        None => query_pages_plain(&conn, source_id, status_filter)?,
    };

    Ok(Json(pages))
}

/// `GET /api/pages/{id}`
pub async fn get_page(
    State(state): State<UiState>,
    Path(page_id): Path<i64>,
) -> ApiResult<Json<PageDetailResponse>> {
    if !state.config.db_path().exists() {
        return Err(VaultError::NotFound(format!("page {page_id} not found")).into());
    }

    let conn = open_rw_db(&state.config.db_path())?;
    let page = conn
        .query_row(
            "SELECT p.id, p.source_id, s.name, p.uri, p.title, p.content_md, p.content_hash,
                    p.word_count, p.etag, p.last_modified, p.status, p.http_status,
                    p.error_message, p.first_seen_at, p.last_checked_at, p.last_changed_at
             FROM pages p
             JOIN sources s ON s.id = p.source_id
             WHERE p.id = ?1",
            [page_id],
            |row| {
                Ok(PageDetailResponse {
                    id: row.get(0)?,
                    source_id: row.get(1)?,
                    source_name: row.get(2)?,
                    uri: row.get(3)?,
                    title: row.get(4)?,
                    content_md: row.get(5)?,
                    content_hash: row.get(6)?,
                    word_count: row.get(7)?,
                    etag: row.get(8)?,
                    last_modified: row.get(9)?,
                    status: row.get(10)?,
                    http_status: row.get(11)?,
                    error_message: row.get(12)?,
                    first_seen_at: row.get(13)?,
                    last_checked_at: row.get(14)?,
                    last_changed_at: row.get(15)?,
                    sections: Vec::new(),
                })
            },
        )
        .optional()
        .map_err(VaultError::from)?
        .ok_or_else(|| VaultError::NotFound(format!("page {page_id} not found")))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, ordinal, heading, heading_path, level, content_md, word_count
             FROM sections
             WHERE page_id = ?1
             ORDER BY ordinal ASC",
        )
        .map_err(VaultError::from)?;
    let sections = stmt
        .query_map([page_id], |row| {
            Ok(PageSectionItem {
                id: row.get(0)?,
                ordinal: row.get(1)?,
                heading: row.get(2)?,
                heading_path: row.get(3)?,
                level: row.get(4)?,
                content_md: row.get(5)?,
                word_count: row.get(6)?,
            })
        })
        .map_err(VaultError::from)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(VaultError::from)?;

    Ok(Json(PageDetailResponse { sections, ..page }))
}

/// `GET /api/pages/{id}/diff`
pub async fn get_page_diff(
    State(state): State<UiState>,
    Path(page_id): Path<i64>,
) -> ApiResult<Json<PageDiffResponse>> {
    if !state.config.db_path().exists() {
        return Err(VaultError::NotFound(format!("page {page_id} not found")).into());
    }

    let conn = open_rw_db(&state.config.db_path())?;
    let (uri, title): (String, String) = conn
        .query_row(
            "SELECT uri, title FROM pages WHERE id = ?1",
            [page_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(VaultError::from)?
        .ok_or_else(|| VaultError::NotFound(format!("page {page_id} not found")))?;

    let revision: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT previous_hash, new_hash, unified_diff, changed_at
             FROM page_revisions
             WHERE page_id = ?1",
            [page_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(VaultError::from)?;

    let response = match revision {
        Some((previous_hash, new_hash, unified_diff, changed_at)) => PageDiffResponse {
            page_id,
            uri,
            title,
            has_revision: true,
            previous_hash: Some(previous_hash),
            new_hash: Some(new_hash),
            unified_diff: Some(unified_diff),
            changed_at: Some(changed_at),
        },
        None => PageDiffResponse {
            page_id,
            uri,
            title,
            has_revision: false,
            previous_hash: None,
            new_hash: None,
            unified_diff: None,
            changed_at: None,
        },
    };

    Ok(Json(response))
}

/// `POST /api/query`
pub async fn query(
    State(state): State<UiState>,
    Json(payload): Json<QueryRequest>,
) -> ApiResult<Json<QueryResult>> {
    let result = execute_readonly_query(&state.config.db_path(), &payload.sql, payload.limit)?;
    Ok(Json(result))
}

fn resolve_source_in_vault(
    config: &crate::config::VaultConfig,
    id_or_slug: &str,
) -> Result<(i64, String)> {
    if !config.db_path().exists() {
        return Err(VaultError::NotFound(format!(
            "source '{id_or_slug}' not found"
        )));
    }
    let conn = open_rw_db(&config.db_path())?;
    resolve_source_id_and_name(&conn, id_or_slug)
}

fn resolve_source_id_and_name(conn: &Connection, id_or_slug: &str) -> Result<(i64, String)> {
    if let Ok(parsed_id) = id_or_slug.parse::<i64>() {
        if let Some(found) = conn
            .query_row(
                "SELECT id, name FROM sources WHERE id = ?1",
                [parsed_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
        {
            return Ok(found);
        }
    }

    conn.query_row(
        "SELECT id, name FROM sources WHERE name = ?1",
        [id_or_slug],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()?
    .ok_or_else(|| VaultError::NotFound(format!("source '{id_or_slug}' not found")))
}

fn query_pages_plain(
    conn: &Connection,
    source_id: i64,
    status: Option<&str>,
) -> Result<Vec<PageListItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, source_id, uri, title, word_count, status, http_status,
                error_message, first_seen_at, last_checked_at, last_changed_at
         FROM pages
         WHERE source_id = ?1
           AND (?2 IS NULL OR status = ?2)
         ORDER BY uri ASC",
    )?;
    let rows = stmt
        .query_map(params![source_id, status], map_page_list_item)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn query_pages_fts(
    conn: &Connection,
    source_id: i64,
    status: Option<&str>,
    term: &str,
) -> Result<Vec<PageListItem>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.source_id, p.uri, p.title, p.word_count, p.status, p.http_status,
                p.error_message, p.first_seen_at, p.last_checked_at, p.last_changed_at
         FROM pages p
         JOIN pages_fts ON pages_fts.rowid = p.id
         WHERE p.source_id = ?1
           AND (?2 IS NULL OR p.status = ?2)
           AND pages_fts MATCH ?3
         ORDER BY bm25(pages_fts), p.uri ASC",
    )?;
    let rows = stmt
        .query_map(params![source_id, status, term], map_page_list_item)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn query_pages_like(
    conn: &Connection,
    source_id: i64,
    status: Option<&str>,
    term: &str,
) -> Result<Vec<PageListItem>> {
    let pattern = format!("%{term}%");
    let mut stmt = conn.prepare(
        "SELECT id, source_id, uri, title, word_count, status, http_status,
                error_message, first_seen_at, last_checked_at, last_changed_at
         FROM pages
         WHERE source_id = ?1
           AND (?2 IS NULL OR status = ?2)
           AND (title LIKE ?3 OR uri LIKE ?3 OR content_md LIKE ?3)
         ORDER BY uri ASC",
    )?;
    let rows = stmt
        .query_map(params![source_id, status, pattern], map_page_list_item)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn map_page_list_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<PageListItem> {
    Ok(PageListItem {
        id: row.get(0)?,
        source_id: row.get(1)?,
        uri: row.get(2)?,
        title: row.get(3)?,
        word_count: row.get(4)?,
        status: row.get(5)?,
        http_status: row.get(6)?,
        error_message: row.get(7)?,
        first_seen_at: row.get(8)?,
        last_checked_at: row.get(9)?,
        last_changed_at: row.get(10)?,
    })
}
