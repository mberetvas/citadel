pub mod api;

use std::net::SocketAddr;

use axum::extract::{Path, Request};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde_json::json;
use tokio::net::TcpListener;
use url::{Host, Url};

use crate::config::VaultConfig;
use crate::db::open_rw_db;
use crate::error::{Result, VaultError};

/// Default localhost port for `citadel ui`.
pub const DEFAULT_UI_PORT: u16 = 4317;

/// Compile-time embedded offline Web UI assets (`index.html`, `style.css`, `app.js`).
#[derive(RustEmbed)]
#[folder = "src/ui/assets"]
struct UiAssets;

/// Shared state passed to Axum UI handlers.
#[derive(Debug, Clone)]
pub struct UiState {
    pub config: VaultConfig,
    /// Current Unix time in seconds; injectable so tests control cache freshness.
    pub clock: fn() -> i64,
}

impl UiState {
    /// State using the real system clock.
    pub fn new(config: VaultConfig) -> Self {
        Self {
            config,
            clock: crate::update::check::current_unix_secs,
        }
    }
}

/// Build the Axum router for the local Web UI REST API and embedded static assets.
pub fn build_router(config: VaultConfig) -> Router {
    build_ui_routes(UiState::new(config)).layer(middleware::from_fn(origin_host_guard))
}

/// Build the unified `citadel serve` router combining the embedded Web UI (`/`),
/// REST API (`/api/*`), and MCP Streamable HTTP endpoint (`/mcp`).
pub fn build_serve_router(config: VaultConfig) -> Router {
    build_ui_routes(UiState::new(config.clone()))
        .merge(crate::mcp::http::mcp_router(config))
        .layer(middleware::from_fn(origin_host_guard))
}

fn build_ui_routes(state: UiState) -> Router {
    Router::new()
        .route("/api/health", get(api::health))
        .route("/api/version", get(api::get_version))
        .route(
            "/api/sources",
            get(api::list_sources).post(api::create_source),
        )
        .route("/api/sources/{id}", delete(api::delete_source))
        .route("/api/sources/{id}/sync", post(api::sync_source))
        .route("/api/sources/{id}/pages", get(api::list_source_pages))
        .route("/api/pages", get(api::list_pages))
        .route("/api/pages/{id}", get(api::get_page))
        .route("/api/pages/{id}/diff", get(api::get_page_diff))
        .route("/api/query", post(api::query))
        .route("/", get(serve_index))
        .route("/{*path}", get(serve_static_asset))
        .with_state(state)
}

async fn serve_index() -> Response {
    serve_embedded_path("index.html")
}

async fn serve_static_asset(Path(path): Path<String>) -> Response {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return serve_embedded_path("index.html");
    }
    if trimmed == "api" || trimmed.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_embedded_path(trimmed)
}

fn serve_embedded_path(asset_path: &str) -> Response {
    match UiAssets::get(asset_path) {
        Some(file) => {
            let content_type = content_type_for_path(asset_path);
            let mut resp = Response::new(axum::body::Body::from(file.data.into_owned()));
            *resp.status_mut() = StatusCode::OK;
            let header_val = HeaderValue::from_str(&content_type)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
            resp.headers_mut().insert(header::CONTENT_TYPE, header_val);
            resp
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn content_type_for_path(path: &str) -> String {
    if path.ends_with(".html") {
        "text/html; charset=utf-8".to_string()
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8".to_string()
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8".to_string()
    } else {
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .as_ref()
            .to_string()
    }
}

/// Bind and serve the local Web UI HTTP server strictly on `127.0.0.1`.
pub async fn run_ui_server(config: VaultConfig, port: Option<u16>, no_open: bool) -> Result<()> {
    // Ensure vault.db and schema exist on fresh vaults before serving requests.
    let _conn = open_rw_db(&config.db_path())?;

    let listener = bind_loopback_listener(port).await?;
    let actual_port = listener.local_addr()?.port();
    let url = format!("http://127.0.0.1:{actual_port}");

    println!("citadel UI listening on {url}");
    tracing::info!(%url, "started local web UI server");

    if !no_open {
        if let Err(err) = open::that_detached(&url) {
            tracing::warn!(%url, error = %err, "failed to open browser automatically");
        }
    }

    axum::serve(listener, build_router(config))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| VaultError::Other(format!("UI server error: {err}")))?;

    Ok(())
}

async fn bind_loopback_listener(port: Option<u16>) -> Result<TcpListener> {
    match port {
        Some(explicit_port) => {
            let addr = SocketAddr::from(([127, 0, 0, 1], explicit_port));
            Ok(TcpListener::bind(addr).await?)
        }
        None => {
            let default_addr = SocketAddr::from(([127, 0, 0, 1], DEFAULT_UI_PORT));
            match TcpListener::bind(default_addr).await {
                Ok(listener) => Ok(listener),
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                    tracing::warn!(
                        port = DEFAULT_UI_PORT,
                        "default UI port is in use; binding ephemeral loopback port"
                    );
                    let ephemeral = SocketAddr::from(([127, 0, 0, 1], 0));
                    Ok(TcpListener::bind(ephemeral).await?)
                }
                Err(err) => Err(err.into()),
            }
        }
    }
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        tracing::warn!(error = %err, "failed to listen for ctrl-c shutdown signal");
    }
}

/// CSRF guard rejecting cross-origin or non-loopback `Origin`/`Host` headers on mutating requests.
async fn origin_host_guard(request: Request, next: Next) -> Response {
    if matches!(
        *request.method(),
        Method::POST | Method::DELETE | Method::PUT | Method::PATCH
    ) {
        if let Err(msg) = validate_mutating_headers(request.headers()) {
            return (StatusCode::FORBIDDEN, Json(json!({ "error": msg }))).into_response();
        }
    }
    next.run(request).await
}

fn validate_mutating_headers(headers: &HeaderMap) -> std::result::Result<(), &'static str> {
    if let Some(origin_val) = headers.get(header::ORIGIN) {
        let origin_str = origin_val
            .to_str()
            .map_err(|_| "invalid Origin header encoding")?;
        let parsed = Url::parse(origin_str).map_err(|_| "invalid Origin header URL")?;
        if !matches!(parsed.scheme(), "http" | "https") || !is_loopback_host(parsed.host()) {
            return Err("cross-origin mutating requests are forbidden");
        }
    }

    if let Some(host_val) = headers.get(header::HOST) {
        let host_str = host_val
            .to_str()
            .map_err(|_| "invalid Host header encoding")?;
        let parsed =
            Url::parse(&format!("http://{host_str}")).map_err(|_| "invalid Host header value")?;
        if parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || !is_loopback_host(parsed.host())
        {
            return Err("non-loopback Host header is forbidden on mutating requests");
        }
    }

    Ok(())
}

fn is_loopback_host(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tempfile::tempdir;
    use tower::ServiceExt;

    use super::*;
    use crate::db::open_rw_db;

    #[tokio::test]
    async fn create_source_rejects_private_origin() {
        let vault_dir = tempdir().expect("vault tempdir");
        let app = build_router(VaultConfig::new(vault_dir.path()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sources")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"origin": "http://10.0.0.1/", "sync_now": false}).to_string(),
                    ))
                    .expect("build request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn sync_returns_409_when_claimed() {
        let vault_dir = tempdir().expect("vault tempdir");
        let docs_dir = tempdir().expect("docs tempdir");
        std::fs::write(docs_dir.path().join("a.md"), "# A\n\nAlpha.\n").expect("write a.md");
        let config = VaultConfig::new(vault_dir.path());
        let lifecycle = crate::sources::SourceLifecycle::new(config.clone());
        lifecycle
            .add(
                crate::sources::AddSourceParams {
                    origin: docs_dir.path().to_string_lossy().into_owned(),
                    name: Some("busy".into()),
                    ..Default::default()
                },
                false,
            )
            .await
            .expect("register");
        let held = lifecycle
            .claim("busy")
            .await
            .expect("claim")
            .expect("claim is free");

        let app = build_router(config.clone());
        for (method, uri) in [
            (Method::POST, "/api/sources/busy/sync"),
            (Method::DELETE, "/api/sources/busy"),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method.clone())
                        .uri(uri)
                        .body(Body::empty())
                        .expect("build request"),
                )
                .await
                .expect("response");
            assert_eq!(resp.status(), StatusCode::CONFLICT, "{method} {uri}");
        }
        held.release(&crate::db::open_rw_db(&config.db_path()).expect("open db"))
            .expect("release");

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sources/busy/sync")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    async fn read_json_body(resp: axum::response::Response) -> Value {
        let bytes = resp
            .into_body()
            .collect()
            .await
            .expect("collect response body")
            .to_bytes();
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "invalid JSON body ({e}): {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    #[tokio::test]
    async fn health_and_sources_crud_api() {
        let vault_dir = tempdir().expect("vault tempdir");
        let docs_dir = tempdir().expect("docs tempdir");

        std::fs::write(
            docs_dir.path().join("guide.md"),
            "# Vault Guide\n\n## Intro\n\nWelcome to citadel local search.\n\n## Details\n\nFTS5 indexes sections.\n",
        )
        .expect("write guide.md");

        let config = VaultConfig::new(vault_dir.path());
        let app = build_router(config.clone());

        // 1. GET /api/health
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/health")
                    .body(Body::empty())
                    .expect("build health request"),
            )
            .await
            .expect("health response");
        assert_eq!(resp.status(), StatusCode::OK);
        let health = read_json_body(resp).await;
        assert_eq!(health["status"], "ok");
        assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));

        // 2. POST /api/sources (local Markdown folder)
        let create_body = json!({
            "origin": docs_dir.path().to_string_lossy(),
            "name": "local-guide",
            "tags": ["docs", "local"]
        });
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sources")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(create_body.to_string()))
                    .expect("build create source request"),
            )
            .await
            .expect("create source response");
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = read_json_body(resp).await;
        let source_id = created["source"]["id"].as_i64().expect("source id");
        assert_eq!(created["source"]["name"], "local-guide");
        assert_eq!(created["summary"]["pages_added"], 1);

        // 3. GET /api/sources
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/sources")
                    .body(Body::empty())
                    .expect("build list sources request"),
            )
            .await
            .expect("list sources response");
        assert_eq!(resp.status(), StatusCode::OK);
        let sources = read_json_body(resp).await;
        let sources_arr = sources.as_array().expect("sources array");
        assert_eq!(sources_arr.len(), 1);
        assert_eq!(sources_arr[0]["id"], source_id);
        assert_eq!(sources_arr[0]["active_pages"], 1);

        // 4. GET /api/sources/{id}/pages (with status and q filter)
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!(
                        "/api/sources/{source_id}/pages?status=active&q=FTS5"
                    ))
                    .body(Body::empty())
                    .expect("build list pages request"),
            )
            .await
            .expect("list pages response");
        assert_eq!(resp.status(), StatusCode::OK);
        let pages = read_json_body(resp).await;
        let pages_arr = pages.as_array().expect("pages array");
        assert_eq!(pages_arr.len(), 1);
        let page_id = pages_arr[0]["id"].as_i64().expect("page id");
        assert_eq!(pages_arr[0]["uri"], "guide.md");

        // 5. GET /api/pages/{id}
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/pages/{page_id}"))
                    .body(Body::empty())
                    .expect("build get page request"),
            )
            .await
            .expect("get page response");
        assert_eq!(resp.status(), StatusCode::OK);
        let page_detail = read_json_body(resp).await;
        assert_eq!(page_detail["id"], page_id);
        assert_eq!(page_detail["uri"], "guide.md");
        assert!(page_detail["content_md"]
            .as_str()
            .expect("content_md")
            .contains("FTS5 indexes sections."));
        let sections = page_detail["sections"].as_array().expect("sections array");
        assert!(sections.len() >= 2);

        // 6. GET /api/pages/{id}/diff (initially no revision)
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/pages/{page_id}/diff"))
                    .body(Body::empty())
                    .expect("build get diff request"),
            )
            .await
            .expect("get diff response");
        assert_eq!(resp.status(), StatusCode::OK);
        let diff_empty = read_json_body(resp).await;
        assert_eq!(diff_empty["page_id"], page_id);
        assert_eq!(diff_empty["has_revision"], false);
        assert!(diff_empty["unified_diff"].is_null());

        // Modify file and sync to produce a revision diff
        std::fs::write(
            docs_dir.path().join("guide.md"),
            "# Vault Guide\n\n## Intro\n\nWelcome to citadel local search.\n\n## Details\n\nFTS5 indexes sections and diffs.\n",
        )
        .expect("update guide.md");
        crate::sources::SourceLifecycle::new(config.clone())
            .sync("local-guide", false)
            .await
            .expect("sync updated file");

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/pages/{page_id}/diff"))
                    .body(Body::empty())
                    .expect("build get diff after edit request"),
            )
            .await
            .expect("get diff after edit response");
        assert_eq!(resp.status(), StatusCode::OK);
        let diff_present = read_json_body(resp).await;
        assert_eq!(diff_present["has_revision"], true);
        assert!(diff_present["unified_diff"]
            .as_str()
            .expect("unified_diff string")
            .contains("+FTS5 indexes sections and diffs."));

        // 7. DELETE /api/sources/{id}
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/sources/{source_id}"))
                    .body(Body::empty())
                    .expect("build delete source request"),
            )
            .await
            .expect("delete source response");
        assert_eq!(resp.status(), StatusCode::OK);
        let deleted = read_json_body(resp).await;
        assert_eq!(deleted["id"], source_id);
        assert_eq!(deleted["name"], "local-guide");
        assert_eq!(deleted["pages_deleted"], 1);

        // Deleting again returns 404
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/sources/{source_id}"))
                    .body(Body::empty())
                    .expect("build second delete request"),
            )
            .await
            .expect("second delete response");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn post_query_enforces_readonly_boundary() {
        let vault_dir = tempdir().expect("vault tempdir");
        let config = VaultConfig::new(vault_dir.path());
        let conn = open_rw_db(&config.db_path()).expect("init db");
        conn.execute(
            "INSERT INTO sources (name, kind, origin, status) VALUES ('demo', 'local', '/tmp/demo', 'ok')",
            [],
        )
        .expect("insert demo source");
        drop(conn);

        let app = build_router(config);

        // Allowed SELECT
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/query")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "sql": "SELECT name FROM v_sources", "limit": 10 }).to_string(),
                    ))
                    .expect("build select query request"),
            )
            .await
            .expect("select query response");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_json_body(resp).await;
        assert_eq!(body["row_count"], 1);
        assert_eq!(body["rows"][0][0], "demo");

        // Blocked DELETE / DROP
        for bad_sql in ["DELETE FROM sources", "DROP TABLE sources"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/query")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(json!({ "sql": bad_sql }).to_string()))
                        .expect("build bad query request"),
                )
                .await
                .expect("bad query response");
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "expected 400 for `{bad_sql}`"
            );
            let err_body = read_json_body(resp).await;
            assert!(err_body["error"].as_str().is_some());
        }
    }

    async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(uri)
                    .body(Body::empty())
                    .expect("build GET request"),
            )
            .await
            .expect("GET response");
        let status = resp.status();
        (status, read_json_body(resp).await)
    }

    async fn get_pages(app: &Router, uri: &str) -> (StatusCode, bool, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(uri)
                    .body(Body::empty())
                    .expect("build GET request"),
            )
            .await
            .expect("GET response");
        let status = resp.status();
        let truncated = resp
            .headers()
            .get("x-citadel-truncated")
            .is_some_and(|v| v == "true");
        (status, truncated, read_json_body(resp).await)
    }

    #[tokio::test]
    async fn all_sources_page_list_has_http_status_and_a_marked_cap() {
        let vault_dir = tempdir().expect("vault tempdir");
        let config = VaultConfig::new(vault_dir.path());
        let conn = open_rw_db(&config.db_path()).expect("init db");
        conn.execute_batch(
            "INSERT INTO sources (id, name, kind, origin) VALUES
                 (1, 'alpha', 'web', 'https://alpha.example.com/'),
                 (2, 'beta', 'local', '/tmp/beta');
             INSERT INTO pages (source_id, uri, title, content_md, content_hash, status,
                                http_status, error_message)
             VALUES (1, 'https://alpha.example.com/gone', 'Gone', 'Old copy.', 'h', 'error',
                     404, 'HTTP 404');",
        )
        .expect("seed sources");
        for i in 0..120 {
            conn.execute(
                "INSERT INTO pages (source_id, uri, content_md, content_hash)
                 VALUES (2, ?1, 'Body text.', 'h')",
                [format!("bulk/{i:03}.md")],
            )
            .expect("insert bulk page");
        }
        drop(conn);
        let app = build_router(config);

        // All sources: every Page, with http_status, beyond the old 100-row cap; under
        // the default cap, so not marked truncated.
        let (status, truncated, pages) = get_pages(&app, "/api/pages").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!truncated);
        let pages = pages.as_array().expect("pages array");
        assert_eq!(pages.len(), 121);
        let gone = pages
            .iter()
            .find(|p| p["uri"] == "https://alpha.example.com/gone")
            .expect("error page listed");
        assert_eq!(gone["http_status"], 404);
        assert_eq!(gone["error_message"], "HTTP 404");
        assert_eq!(gone["source_name"], "alpha");

        // Status set and search, across Sources; a term FTS5 would reject is fine.
        let (status, pages) = get_json(&app, "/api/pages?status=stale,error&q=old-copy").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(pages.as_array().expect("array").len(), 1);

        // A smaller limit cuts the list and marks it truncated; a list that exactly
        // fits is not marked.
        let (_, truncated, pages) = get_pages(&app, "/api/pages?limit=50").await;
        assert_eq!(pages.as_array().expect("array").len(), 50);
        assert!(truncated);
        let (_, truncated, _) = get_pages(&app, "/api/pages?limit=121").await;
        assert!(!truncated);
        for uri in ["/api/pages?limit=0", "/api/pages?limit=10001"] {
            let (status, _) = get_json(&app, uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }

        // One Source, by query parameter (surrounding blanks ignored) or by path.
        let (status, _) = get_json(&app, "/api/pages?source=beta%20").await;
        assert_eq!(status, StatusCode::OK);
        let (_, by_param) = get_json(&app, "/api/pages?source=beta&status=active").await;
        let (_, by_path) = get_json(&app, "/api/sources/2/pages?status=active").await;
        assert_eq!(by_param.as_array().expect("array").len(), 120);
        assert_eq!(by_param, by_path);

        // Same validation on both endpoints.
        for uri in ["/api/pages?status=gone", "/api/sources/1/pages?status=gone"] {
            let (status, _) = get_json(&app, uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }
        for uri in ["/api/pages?source=nope", "/api/sources/nope/pages"] {
            let (status, _) = get_json(&app, uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn rejects_cross_origin_mutating_requests() {
        let vault_dir = tempdir().expect("vault tempdir");
        let docs_dir = tempdir().expect("docs tempdir");
        std::fs::write(docs_dir.path().join("doc.md"), "# Doc\n\nBody.\n").expect("write doc.md");

        let config = VaultConfig::new(vault_dir.path());
        let app = build_router(config);

        // Cross-origin POST /api/sources -> 403 Forbidden
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sources")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://evil.example")
                    .body(Body::from(
                        json!({ "origin": docs_dir.path().to_string_lossy() }).to_string(),
                    ))
                    .expect("build evil post request"),
            )
            .await
            .expect("evil post response");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Cross-origin DELETE /api/sources/1 -> 403 Forbidden
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/api/sources/1")
                    .header(header::ORIGIN, "https://evil.example")
                    .body(Body::empty())
                    .expect("build evil delete request"),
            )
            .await
            .expect("evil delete response");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Non-loopback Host header on mutating request -> 403 Forbidden
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/api/sources/1")
                    .header(header::HOST, "evil.example:4317")
                    .body(Body::empty())
                    .expect("build evil host request"),
            )
            .await
            .expect("evil host response");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Valid loopback Origin: http://127.0.0.1:4317 succeeds
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sources")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "http://127.0.0.1:4317")
                    .header(header::HOST, "127.0.0.1:4317")
                    .body(Body::from(
                        json!({ "origin": docs_dir.path().to_string_lossy(), "name": "safe-docs" })
                            .to_string(),
                    ))
                    .expect("build loopback post request"),
            )
            .await
            .expect("loopback post response");
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn post_sync_triggers_async_source_refresh() {
        let vault_dir = tempdir().expect("vault tempdir");
        let docs_dir = tempdir().expect("docs tempdir");
        std::fs::write(docs_dir.path().join("a.md"), "# Page A\n\nInitial.\n").expect("write a.md");

        let config = VaultConfig::new(vault_dir.path());
        let crate::sources::Added { source, .. } =
            crate::sources::SourceLifecycle::new(config.clone())
                .add(
                    crate::sources::AddSourceParams {
                        origin: docs_dir.path().to_string_lossy().into_owned(),
                        name: Some("sync-target".into()),
                        ..Default::default()
                    },
                    true,
                )
                .await
                .expect("add initial source");

        // Add a second markdown file before triggering async sync
        std::fs::write(docs_dir.path().join("b.md"), "# Page B\n\nAdded.\n").expect("write b.md");

        let app = build_router(config.clone());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/sources/{}/sync", source.id))
                    .body(Body::empty())
                    .expect("build sync request"),
            )
            .await
            .expect("sync response");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let accepted = read_json_body(resp).await;
        assert_eq!(accepted["status"], "accepted");
        assert_eq!(accepted["source"], "sync-target");

        // Poll sync_runs briefly until the background sync finishes (2 total runs: initial + async)
        let mut completed = false;
        for _ in 0..40 {
            let conn = open_rw_db(&config.db_path()).expect("open db for poll");
            let ok_runs: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sync_runs WHERE source_id = ?1 AND status = 'ok'",
                    [source.id],
                    |r| r.get(0),
                )
                .expect("count sync_runs");
            let page_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pages WHERE source_id = ?1 AND status = 'active'",
                    [source.id],
                    |r| r.get(0),
                )
                .expect("count active pages");
            if ok_runs >= 2 && page_count == 2 {
                completed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            completed,
            "background sync did not record a completed sync_run and ingest b.md within timeout"
        );
    }

    async fn read_text_body(resp: axum::response::Response) -> String {
        let bytes = resp
            .into_body()
            .collect()
            .await
            .expect("collect response body")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("valid utf-8 body")
    }

    #[tokio::test]
    async fn serves_embedded_index_css_and_js_with_correct_mime() {
        let vault_dir = tempdir().expect("vault tempdir");
        let app = build_router(VaultConfig::new(vault_dir.path()));

        let cases = [
            ("/", "text/html"),
            ("/index.html", "text/html"),
            ("/style.css", "text/css"),
            ("/app.js", "application/javascript"),
            ("/favicon.svg", "image/svg+xml"),
        ];

        for (uri, expected_mime) in cases {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri(uri)
                        .body(Body::empty())
                        .expect("build asset request"),
                )
                .await
                .expect("asset response");
            assert_eq!(resp.status(), StatusCode::OK, "expected 200 OK for `{uri}`");
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert!(
                content_type.starts_with(expected_mime),
                "expected Content-Type starting with `{expected_mime}` for `{uri}`, got `{content_type}`"
            );
            let body = read_text_body(resp).await;
            assert!(
                !body.trim().is_empty(),
                "expected non-empty body for `{uri}`"
            );
        }
    }

    async fn get_asset_text(app: &Router, uri: &str) -> String {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(uri)
                    .body(Body::empty())
                    .expect("build asset request"),
            )
            .await
            .expect("asset response");
        assert_eq!(resp.status(), StatusCode::OK, "expected 200 OK for `{uri}`");
        read_text_body(resp).await
    }

    #[tokio::test]
    async fn ui_assets_include_pages_tooltip_and_accessible_header() {
        let vault_dir = tempdir().expect("vault tempdir");
        let app = build_router(VaultConfig::new(vault_dir.path()));

        let index_html = get_asset_text(&app, "/").await;
        for expected in [
            "Pages (active)",
            "id=\"pages-tooltip\"",
            "popover=\"hint\"",
            "role=\"tooltip\"",
        ] {
            assert!(
                index_html.contains(expected),
                "index.html missing `{expected}`"
            );
        }

        let app_js = get_asset_text(&app, "/app.js").await;
        for expected in [
            "formatPagesBreakdown",
            "aria-describedby",
            "browse-status",
            "initPagesTooltip",
        ] {
            assert!(app_js.contains(expected), "app.js missing `{expected}`");
        }

        let style_css = get_asset_text(&app, "/style.css").await;
        for expected in [".pages-tooltip", ".pages-count-btn"] {
            assert!(
                style_css.contains(expected),
                "style.css missing `{expected}`"
            );
        }
    }

    #[tokio::test]
    async fn ui_assets_include_browser_error_banner_and_drilldown() {
        let vault_dir = tempdir().expect("vault tempdir");
        let app = build_router(VaultConfig::new(vault_dir.path()));

        let app_js = get_asset_text(&app, "/app.js").await;
        for expected in [
            "renderPageStatusBanner",
            "navigateToBrowser",
            "error_message",
            "status === \"error\"",
        ] {
            assert!(app_js.contains(expected), "app.js missing `{expected}`");
        }
    }

    #[tokio::test]
    async fn ui_assets_read_page_lists_from_the_server() {
        let vault_dir = tempdir().expect("vault tempdir");
        let app = build_router(VaultConfig::new(vault_dir.path()));

        let app_js = get_asset_text(&app, "/app.js").await;
        assert!(app_js.contains("/api/pages"), "app.js must use /api/pages");
        // A cut list is shown as such, and a superseded Browser search cannot
        // overwrite a newer one.
        for expected in ["X-Citadel-Truncated", "browserSearchSeq"] {
            assert!(app_js.contains(expected), "app.js missing `{expected}`");
        }
        // The Browser and the Health list no longer build SQL in the client, and the
        // banner's Error rule is the server's (`pages.status`), not its own.
        for forbidden in [
            "queryAllSourcePages",
            "escapeSqlLiteral",
            "SELECT p.id",
            "hasFailingHttp",
        ] {
            assert!(
                !app_js.contains(forbidden),
                "app.js still has `{forbidden}`"
            );
        }
    }

    #[tokio::test]
    async fn ui_assets_reset_and_hide_pages_tooltip_safely() {
        let vault_dir = tempdir().expect("vault tempdir");
        let app = build_router(VaultConfig::new(vault_dir.path()));

        let app_js = get_asset_text(&app, "/app.js").await;
        for expected in [
            "hidePagesTooltip()",
            "tooltip.style.left = \"0px\"",
            "tooltip.style.top = \"0px\"",
            "tooltip.hidden = true;",
        ] {
            assert!(app_js.contains(expected), "app.js missing `{expected}`");
        }
    }

    #[tokio::test]
    async fn assets_contain_zero_external_cdn_links() {
        let vault_dir = tempdir().expect("vault tempdir");
        let app = build_router(VaultConfig::new(vault_dir.path()));

        for uri in ["/", "/style.css", "/app.js"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri(uri)
                        .body(Body::empty())
                        .expect("build asset request"),
                )
                .await
                .expect("asset response");
            assert_eq!(resp.status(), StatusCode::OK, "expected 200 OK for `{uri}`");
            let body = read_text_body(resp).await;
            let lower = body.to_ascii_lowercase();

            // Check that no external script/link/CSS url() references exist
            for forbidden in [
                "<script src=\"http://",
                "<script src=\"https://",
                "<script src='http://",
                "<script src='https://",
                "href=\"http://",
                "href=\"https://",
                "href='http://",
                "href='https://",
                "url(http://",
                "url(https://",
                "url(\"http://",
                "url(\"https://",
                "url('http://",
                "url('https://",
                "@import url(http",
                "@import \"http",
                "@import 'http",
            ] {
                assert!(
                    !lower.contains(forbidden),
                    "asset `{uri}` must not contain external CDN link `{forbidden}`"
                );
            }
        }
    }

    #[tokio::test]
    async fn unknown_asset_returns_404() {
        let vault_dir = tempdir().expect("vault tempdir");
        let app = build_router(VaultConfig::new(vault_dir.path()));

        for uri in ["/nonexistent.js", "/assets/missing.css", "/api/nonexistent"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::GET)
                        .uri(uri)
                        .body(Body::empty())
                        .expect("build unknown asset request"),
                )
                .await
                .expect("unknown asset response");
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "expected 404 Not Found for `{uri}`"
            );
        }
    }

    #[tokio::test]
    async fn serve_router_mounts_ui_api_and_mcp_with_origin_guard() {
        let vault_dir = tempdir().expect("vault tempdir");
        let app = build_serve_router(VaultConfig::new(vault_dir.path()));

        // GET / -> 200 HTML
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/")
                    .body(Body::empty())
                    .expect("GET /"),
            )
            .await
            .expect("GET / resp");
        assert_eq!(resp.status(), StatusCode::OK);

        // GET /api/health -> 200 OK
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/health")
                    .body(Body::empty())
                    .expect("GET /api/health"),
            )
            .await
            .expect("GET /api/health resp");
        assert_eq!(resp.status(), StatusCode::OK);

        // GET /mcp -> 200 JSON (overrides /{*path})
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/mcp")
                    .body(Body::empty())
                    .expect("GET /mcp"),
            )
            .await
            .expect("GET /mcp resp");
        assert_eq!(resp.status(), StatusCode::OK);
        let info = read_json_body(resp).await;
        assert_eq!(info["transport"], "streamable-http");

        // POST /mcp with loopback Host -> 200 OK
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header(header::HOST, "127.0.0.1:4317")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": "tools/list",
                            "params": {}
                        })
                        .to_string(),
                    ))
                    .expect("POST /mcp"),
            )
            .await
            .expect("POST /mcp resp");
        assert_eq!(resp.status(), StatusCode::OK);

        // Cross-origin POST /mcp -> 403 Forbidden
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header(header::ORIGIN, "https://evil.example")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "jsonrpc": "2.0",
                            "id": 2,
                            "method": "tools/list",
                            "params": {}
                        })
                        .to_string(),
                    ))
                    .expect("evil POST /mcp"),
            )
            .await
            .expect("evil POST /mcp resp");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Fixed test clock: 2026-01-01T00:00:00Z.
    const NOW: i64 = 1_767_225_600;

    /// Unroutable release URL: a test that forgets to mock GitHub can never reach it.
    const NO_NETWORK_URL: &str = "http://127.0.0.1:9/never";

    fn version_config(vault_dir: &std::path::Path) -> VaultConfig {
        let mut config = VaultConfig::new(vault_dir);
        config.update_api_url = NO_NETWORK_URL.to_string();
        config
    }

    fn version_router(config: VaultConfig) -> Router {
        build_ui_routes(UiState {
            config,
            clock: || NOW,
        })
    }

    fn seed_cache(config: &VaultConfig, checked_at: i64, latest_version: &str) {
        let cache = crate::update::check::UpdateCheckCache {
            checked_at: crate::update::check::format_iso8601_utc(checked_at),
            latest_version: latest_version.to_string(),
            release_url: format!(
                "https://github.com/MaximeBeretvas/citadel/releases/tag/{latest_version}"
            ),
        };
        crate::update::check::write_cache_atomic(&config.update_check_path(), &cache)
            .expect("seed update-check.json");
    }

    async fn get_version_json(app: Router) -> Value {
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/version")
                    .body(Body::empty())
                    .expect("GET /api/version"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        read_json_body(resp).await
    }

    #[tokio::test]
    async fn get_version_returns_current_version_when_cache_missing() {
        let vault_dir = tempdir().expect("vault tempdir");
        let mut config = version_config(vault_dir.path());
        config.no_update_check = true;

        let body = get_version_json(version_router(config)).await;
        assert_eq!(body["current_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["update_available"], false);
        assert!(body["latest_version"].is_null());
    }

    #[tokio::test]
    async fn get_version_reports_update_available_when_cache_has_newer_semver() {
        let vault_dir = tempdir().expect("vault tempdir");
        let config = version_config(vault_dir.path());
        seed_cache(&config, NOW - 60, "v99.0.0");

        let body = get_version_json(version_router(config.clone())).await;
        assert_eq!(body["current_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["update_available"], true);
        assert_eq!(body["latest_version"], "99.0.0");
        assert_eq!(
            body["release_url"],
            "https://github.com/MaximeBeretvas/citadel/releases/tag/v99.0.0"
        );
        assert!(
            !config
                .update_check_path()
                .with_extension("attempt")
                .exists(),
            "a fresh cache must not claim a refresh"
        );
    }

    #[tokio::test]
    async fn get_version_reports_up_to_date_when_cache_matches_current() {
        let vault_dir = tempdir().expect("vault tempdir");
        let config = version_config(vault_dir.path());
        let current = env!("CARGO_PKG_VERSION");
        seed_cache(&config, NOW - 60, &format!("v{current}"));

        let body = get_version_json(version_router(config)).await;
        assert_eq!(body["current_version"], current);
        assert_eq!(body["latest_version"], current);
        assert_eq!(body["update_available"], false);
    }

    #[tokio::test]
    async fn get_version_refreshes_stale_cache_from_configured_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock github server");
        let addr = listener.local_addr().expect("local addr");
        let github = Router::new().route(
            "/releases/latest",
            get(|| async {
                Json(json!({
                    "tag_name": "v42.0.0",
                    "html_url": "https://github.com/MaximeBeretvas/citadel/releases/tag/v42.0.0"
                }))
            }),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, github).await;
        });

        let vault_dir = tempdir().expect("vault tempdir");
        let mut config = version_config(vault_dir.path());
        config.update_api_url = format!("http://{addr}/releases/latest");
        seed_cache(
            &config,
            NOW - crate::update::check::CACHE_TTL_SECS - 1,
            "v0.0.1",
        );

        let body = get_version_json(version_router(config.clone())).await;
        assert_eq!(
            body["latest_version"], "0.0.1",
            "stale data is served while refreshing"
        );

        let mut refreshed = None;
        for _ in 0..50 {
            let raw = std::fs::read_to_string(config.update_check_path()).unwrap_or_default();
            if raw.contains("42.0.0") {
                refreshed = Some(raw);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        server.abort();
        assert!(
            refreshed.is_some(),
            "stale cache must be refreshed from the configured release URL"
        );
    }
}
