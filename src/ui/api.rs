use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::VaultConfig;
use crate::db::readonly::{execute_readonly_query, QueryResult};
use crate::error::{Result, VaultError};
use crate::ingest::{
    self, PageListItem, PageQuery, PageStatuses, SourceRecord, SourceSummary, SyncSummary,
};
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
    /// Source id or name; every Source when absent. `GET /api/sources/{id}/pages`
    /// takes it from the path instead.
    pub source: Option<String>,
    /// Comma-separated subset of `active,stale,error`.
    pub status: Option<String>,
    pub q: Option<String>,
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

/// Run a blocking vault read off the async workers, which `serve` shares with `/mcp`
/// and the scheduler.
async fn blocking<T, F>(work: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| VaultError::Other(format!("vault read task failed: {err}")))?
        .map_err(ApiError)
}

/// `GET /api/sources`
pub async fn list_sources(State(state): State<UiState>) -> ApiResult<Json<Vec<SourceSummary>>> {
    let config = state.config;
    let sources = blocking(move || ingest::list_sources(&config)).await?;
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
    let source_name = resolve_source_name(&state, id_or_slug).await?;
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
    let config = state.config.clone();
    let source = blocking(move || ingest::find_source(&config, &id_or_slug)).await?;
    let (source_id, source_name) = (source.id, source.name);

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

/// `GET /api/pages`: Pages of one Source (`?source=`) or of every Source.
pub async fn list_pages(
    State(state): State<UiState>,
    Query(query): Query<ListPagesQuery>,
) -> ApiResult<Json<Vec<PageListItem>>> {
    let source = query.source.clone().filter(|s| !s.trim().is_empty());
    read_pages(state, source, query).await
}

/// `GET /api/sources/{id}/pages`: Pages of one Source.
pub async fn list_source_pages(
    State(state): State<UiState>,
    Path(id_or_slug): Path<String>,
    Query(query): Query<ListPagesQuery>,
) -> ApiResult<Json<Vec<PageListItem>>> {
    read_pages(state, Some(id_or_slug), query).await
}

async fn read_pages(
    state: UiState,
    source: Option<String>,
    query: ListPagesQuery,
) -> ApiResult<Json<Vec<PageListItem>>> {
    let statuses = PageStatuses::parse(query.status.as_deref().unwrap_or(""))?;
    let config = state.config;
    let pages = blocking(move || {
        let source_id = match source {
            Some(key) => Some(ingest::find_source(&config, &key)?.id),
            None => None,
        };
        ingest::list_pages(
            &config,
            &PageQuery {
                source_id,
                statuses,
                search: query.q,
            },
        )
    })
    .await?;
    Ok(Json(pages))
}

/// `GET /api/pages/{id}`
pub async fn get_page(
    State(state): State<UiState>,
    Path(page_id): Path<i64>,
) -> ApiResult<Json<PageDetailResponse>> {
    let config = state.config;
    let page = blocking(move || read_page_detail(&config, page_id)).await?;
    Ok(Json(page))
}

fn read_page_detail(config: &VaultConfig, page_id: i64) -> Result<PageDetailResponse> {
    let not_found = || VaultError::NotFound(format!("page {page_id} not found"));
    let conn = ingest::open_vault_for_read(config)?.ok_or_else(not_found)?;
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
        .optional()?
        .ok_or_else(not_found)?;

    let mut stmt = conn.prepare(
        "SELECT id, ordinal, heading, heading_path, level, content_md, word_count
         FROM sections
         WHERE page_id = ?1
         ORDER BY ordinal ASC",
    )?;
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
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(PageDetailResponse { sections, ..page })
}

/// `GET /api/pages/{id}/diff`
pub async fn get_page_diff(
    State(state): State<UiState>,
    Path(page_id): Path<i64>,
) -> ApiResult<Json<PageDiffResponse>> {
    let config = state.config;
    let diff = blocking(move || read_page_diff(&config, page_id)).await?;
    Ok(Json(diff))
}

fn read_page_diff(config: &VaultConfig, page_id: i64) -> Result<PageDiffResponse> {
    let not_found = || VaultError::NotFound(format!("page {page_id} not found"));
    let conn = ingest::open_vault_for_read(config)?.ok_or_else(not_found)?;
    let (uri, title): (String, String) = conn
        .query_row(
            "SELECT uri, title FROM pages WHERE id = ?1",
            [page_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(not_found)?;

    let revision: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT previous_hash, new_hash, unified_diff, changed_at
             FROM page_revisions
             WHERE page_id = ?1",
            [page_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;

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

    Ok(response)
}

/// `POST /api/query`
pub async fn query(
    State(state): State<UiState>,
    Json(payload): Json<QueryRequest>,
) -> ApiResult<Json<QueryResult>> {
    let db_path = state.config.db_path();
    let result =
        blocking(move || execute_readonly_query(&db_path, &payload.sql, payload.limit)).await?;
    Ok(Json(result))
}

async fn resolve_source_name(state: &UiState, id_or_slug: String) -> ApiResult<String> {
    let config = state.config.clone();
    let source = blocking(move || ingest::find_source(&config, &id_or_slug)).await?;
    Ok(source.name)
}
