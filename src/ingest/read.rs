//! Read views over Sources and Pages: the one place that knows their column lists,
//! maps their rows, and decides what a missing vault reads as.
//!
//! Missing-vault rule: a read never creates the vault. A missing vault reads as an
//! empty vault: lists are empty, and a lookup by name or id is `NotFound`.

use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use super::{SourceDetail, SourceKind, SourceRecord, SourceSummary, SyncRunRecord};
use crate::config::VaultConfig;
use crate::error::{Result, VaultError};

/// One row of a Page list: a Page's metadata without its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageListItem {
    pub id: i64,
    pub source_id: i64,
    pub source_name: String,
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

/// The set of Page statuses a Page list keeps; the empty set keeps every Page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageStatuses {
    active: bool,
    stale: bool,
    error: bool,
}

impl PageStatuses {
    /// Every Page, whatever its status.
    pub const ANY: Self = Self {
        active: false,
        stale: false,
        error: false,
    };
    /// Stale pages and Error pages.
    pub const STALE_OR_ERROR: Self = Self {
        active: false,
        stale: true,
        error: true,
    };

    /// Parse a comma-separated subset of `active,stale,error`; blank means every status.
    pub fn parse(raw: &str) -> Result<Self> {
        let mut out = Self::ANY;
        for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part {
                "active" => out.active = true,
                "stale" => out.stale = true,
                "error" => out.error = true,
                other => return Err(VaultError::Validation(format!(
                    "invalid page status '{other}'; expected one or more of: active, stale, error"
                ))),
            }
        }
        Ok(out)
    }

    fn is_any(self) -> bool {
        self == Self::ANY
    }
}

/// Which Pages a Page list returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageQuery {
    /// One Source, or every Source when `None`.
    pub source_id: Option<i64>,
    pub statuses: PageStatuses,
    /// Search term; blank means no search.
    pub search: Option<String>,
}

const SOURCE_COLUMNS: &str = "id, name, kind, origin, tags, include_patterns, exclude_patterns,
     max_pages, max_depth, render_js, refresh_interval_secs,
     status, last_synced_at, last_error, created_at";

const PAGE_LIST_SELECT: &str = "SELECT p.id, p.source_id, s.name AS source_name, p.uri, p.title,
            p.word_count, p.status, p.http_status, p.error_message,
            p.first_seen_at, p.last_checked_at, p.last_changed_at
     FROM pages p
     JOIN sources s ON s.id = p.source_id";

const PAGE_LIST_FILTER: &str = "(?1 IS NULL OR p.source_id = ?1)
       AND (?2 OR (?3 AND p.status = 'active') OR (?4 AND p.status = 'stale')
                 OR (?5 AND p.status = 'error'))";

/// List Pages of one Source or of every Source, filtered by status and search term.
///
/// A search matches each word of the term as an FTS5 phrase in the title and Markdown
/// content (a trailing `*` makes it a prefix), or the whole term as a substring of the
/// URI. Matches are ranked by `bm25`, then by URI; without a search, Pages are ordered
/// by URI. The term cannot fail to parse, so every SQLite error is a real one and is
/// returned.
pub fn list_pages(config: &VaultConfig, query: &PageQuery) -> Result<Vec<PageListItem>> {
    match open_vault_for_read(config)? {
        Some(conn) => query_pages(&conn, query),
        None => Ok(Vec::new()),
    }
}

fn query_pages(conn: &Connection, query: &PageQuery) -> Result<Vec<PageListItem>> {
    let st = query.statuses;
    let filter = params![query.source_id, st.is_any(), st.active, st.stale, st.error];
    let term = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let rows = match term {
        None => {
            let sql = format!("{PAGE_LIST_SELECT} WHERE {PAGE_LIST_FILTER} ORDER BY p.uri ASC");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(filter, map_page_list_item)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
        Some(term) => {
            let sql = format!(
                "{PAGE_LIST_SELECT}
                 LEFT JOIN (SELECT rowid AS page_id, bm25(pages_fts) AS rank
                            FROM pages_fts WHERE pages_fts MATCH ?6) m ON m.page_id = p.id
                 WHERE {PAGE_LIST_FILTER}
                   AND (m.page_id IS NOT NULL OR p.uri LIKE ?7 ESCAPE '\\')
                 ORDER BY m.rank IS NULL, m.rank, p.uri ASC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(
                    params![
                        query.source_id,
                        st.is_any(),
                        st.active,
                        st.stale,
                        st.error,
                        fts_phrases(term),
                        like_substring(term)
                    ],
                    map_page_list_item,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
    };
    Ok(rows)
}

/// Quote each word as an FTS5 phrase, so no user input is parsed as FTS5 syntax.
/// A trailing `*` on a word keeps its prefix meaning.
fn fts_phrases(term: &str) -> String {
    term.split_whitespace()
        .map(|word| match word.strip_suffix('*') {
            Some(stem) if !stem.is_empty() => format!("\"{}\"*", stem.replace('"', "\"\"")),
            _ => format!("\"{}\"", word.replace('"', "\"\"")),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A `LIKE … ESCAPE '\'` pattern matching `term` as a literal substring.
fn like_substring(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 2);
    out.push('%');
    for ch in term.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

fn map_page_list_item(row: &Row<'_>) -> rusqlite::Result<PageListItem> {
    Ok(PageListItem {
        id: row.get("id")?,
        source_id: row.get("source_id")?,
        source_name: row.get("source_name")?,
        uri: row.get("uri")?,
        title: row.get("title")?,
        word_count: row.get("word_count")?,
        status: row.get("status")?,
        http_status: row.get("http_status")?,
        error_message: row.get("error_message")?,
        first_seen_at: row.get("first_seen_at")?,
        last_checked_at: row.get("last_checked_at")?,
        last_changed_at: row.get("last_changed_at")?,
    })
}

/// List all Sources with live Page and word counts from `v_sources`, by name.
pub fn list_sources(config: &VaultConfig) -> Result<Vec<SourceSummary>> {
    let Some(conn) = open_vault_for_read(config)? else {
        return Ok(Vec::new());
    };
    let sql = format!(
        "SELECT {SOURCE_COLUMNS}, active_pages, stale_pages, error_pages, total_words
         FROM v_sources ORDER BY name ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], map_source_summary)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One Source by name, with its last 10 Sync runs and its Stale and Error pages.
pub fn show_source(config: &VaultConfig, name: &str) -> Result<SourceDetail> {
    let not_found = || VaultError::NotFound(format!("source '{name}' not found"));
    let conn = open_vault_for_read(config)?.ok_or_else(not_found)?;
    let sql = format!(
        "SELECT {SOURCE_COLUMNS}, active_pages, stale_pages, error_pages, total_words
         FROM v_sources WHERE name = ?1"
    );
    let source = conn
        .query_row(&sql, [name], map_source_summary)
        .optional()?
        .ok_or_else(not_found)?;

    let mut runs_stmt = conn.prepare(
        "SELECT id, source_id, started_at, finished_at, status,
                pages_added, pages_updated, pages_unchanged, pages_stale, pages_failed, error_message
         FROM sync_runs
         WHERE source_id = ?1
         ORDER BY id DESC
         LIMIT 10",
    )?;
    let sync_runs = runs_stmt
        .query_map([source.id], |row| {
            Ok(SyncRunRecord {
                id: row.get("id")?,
                source_id: row.get("source_id")?,
                started_at: row.get("started_at")?,
                finished_at: row.get("finished_at")?,
                status: row.get("status")?,
                pages_added: row.get("pages_added")?,
                pages_updated: row.get("pages_updated")?,
                pages_unchanged: row.get("pages_unchanged")?,
                pages_stale: row.get("pages_stale")?,
                pages_failed: row.get("pages_failed")?,
                error_message: row.get("error_message")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let issue_pages = query_pages(
        &conn,
        &PageQuery {
            source_id: Some(source.id),
            statuses: PageStatuses::STALE_OR_ERROR,
            search: None,
        },
    )?;

    Ok(SourceDetail {
        source,
        sync_runs,
        issue_pages,
    })
}

/// One Source by id (when `key` is an integer that names one) or else by name.
pub fn find_source(config: &VaultConfig, key: &str) -> Result<SourceRecord> {
    let conn = open_vault_for_read(config)?
        .ok_or_else(|| VaultError::NotFound(format!("source '{key}' not found")))?;
    if let Ok(id) = key.parse::<i64>() {
        if let Some(found) = select_one_source(&conn, "id = ?1", id)? {
            return Ok(found);
        }
    }
    select_one_source(&conn, "name = ?1", key)?
        .ok_or_else(|| VaultError::NotFound(format!("source '{key}' not found")))
}

/// Every Source, by name.
pub fn all_sources(config: &VaultConfig) -> Result<Vec<SourceRecord>> {
    match open_vault_for_read(config)? {
        Some(conn) => load_all_sources(&conn),
        None => Ok(Vec::new()),
    }
}

/// Web Sources due for refresh at `now_epoch_secs` (UTC Unix seconds), longest-waiting
/// first. A Source is due once `refresh_interval_secs` has passed since its last Sync
/// attempt, successful or not; an interval of 0 means manual only.
pub fn due_web_sources(config: &VaultConfig, now_epoch_secs: i64) -> Result<Vec<SourceRecord>> {
    let Some(conn) = open_vault_for_read(config)? else {
        return Ok(Vec::new());
    };
    let sql = format!(
        "SELECT {SOURCE_COLUMNS}
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
         ORDER BY last_attempt ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([now_epoch_secs], map_source_record)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// How long a read waits for a writer's lock before failing with `SQLITE_BUSY`.
const READ_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Open the vault for reading, or `None` when it does not exist. Never creates it,
/// never writes, and skips the schema batch that `open_rw_db` re-runs.
pub(crate) fn open_vault_for_read(config: &VaultConfig) -> Result<Option<Connection>> {
    let path = config.db_path();
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(READ_BUSY_TIMEOUT)?;
    Ok(Some(conn))
}

pub(crate) fn load_source_by_id(conn: &Connection, id: i64) -> Result<SourceRecord> {
    select_one_source(conn, "id = ?1", id)?
        .ok_or_else(|| VaultError::NotFound(format!("source id {id} not found")))
}

pub(crate) fn load_source_by_name(conn: &Connection, name: &str) -> Result<SourceRecord> {
    select_one_source(conn, "name = ?1", name)?
        .ok_or_else(|| VaultError::NotFound(format!("source '{name}' not found")))
}

pub(crate) fn load_all_sources(conn: &Connection) -> Result<Vec<SourceRecord>> {
    let sql = format!("SELECT {SOURCE_COLUMNS} FROM sources ORDER BY name ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], map_source_record)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn select_one_source(
    conn: &Connection,
    condition: &str,
    value: impl rusqlite::ToSql,
) -> Result<Option<SourceRecord>> {
    let sql = format!("SELECT {SOURCE_COLUMNS} FROM sources WHERE {condition}");
    Ok(conn
        .query_row(&sql, [value], map_source_record)
        .optional()?)
}

fn map_source_record(row: &Row<'_>) -> rusqlite::Result<SourceRecord> {
    let kind_raw: String = row.get("kind")?;
    let kind =
        SourceKind::from_db(&kind_raw).map_err(|e| conversion_error(row, "kind", Box::new(e)))?;
    Ok(SourceRecord {
        id: row.get("id")?,
        name: row.get("name")?,
        kind,
        origin: row.get("origin")?,
        tags: json_list(row, "tags")?,
        include_patterns: json_list(row, "include_patterns")?,
        exclude_patterns: json_list(row, "exclude_patterns")?,
        max_pages: stored_limit(row, "max_pages")?,
        max_depth: stored_limit(row, "max_depth")?,
        render_js: row.get::<_, i64>("render_js")? != 0,
        refresh_interval_secs: row.get("refresh_interval_secs")?,
        status: row.get("status")?,
        last_synced_at: row.get("last_synced_at")?,
        last_error: row.get("last_error")?,
        created_at: row.get("created_at")?,
    })
}

fn map_source_summary(row: &Row<'_>) -> rusqlite::Result<SourceSummary> {
    let base = map_source_record(row)?;
    Ok(SourceSummary {
        id: base.id,
        name: base.name,
        kind: base.kind,
        origin: base.origin,
        tags: base.tags,
        include_patterns: base.include_patterns,
        exclude_patterns: base.exclude_patterns,
        max_pages: base.max_pages,
        max_depth: base.max_depth,
        render_js: base.render_js,
        refresh_interval_secs: base.refresh_interval_secs,
        status: base.status,
        last_synced_at: base.last_synced_at,
        last_error: base.last_error,
        created_at: base.created_at,
        active_pages: row.get("active_pages")?,
        stale_pages: row.get("stale_pages")?,
        error_pages: row.get("error_pages")?,
        total_words: row.get("total_words")?,
    })
}

/// A stored `max_pages`/`max_depth`; out of `u32` range is an error, not a silent default.
fn stored_limit(row: &Row<'_>, column: &str) -> rusqlite::Result<u32> {
    let raw: i64 = row.get(column)?;
    u32::try_from(raw).map_err(|_| {
        conversion_error(
            row,
            column,
            format!("stored {column} {raw} is out of range").into(),
        )
    })
}

fn json_list(row: &Row<'_>, column: &str) -> rusqlite::Result<Vec<String>> {
    let raw: String = row.get(column)?;
    serde_json::from_str(&raw).map_err(|e| conversion_error(row, column, Box::new(e)))
}

fn conversion_error(
    row: &Row<'_>,
    column: &str,
    err: Box<dyn std::error::Error + Send + Sync>,
) -> rusqlite::Error {
    match row.as_ref().column_index(column) {
        Ok(idx) => rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, err),
        Err(lookup) => lookup,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_rw_db;
    use tempfile::{tempdir, TempDir};

    /// A tempdir vault with two Sources: `alpha` (web) and `beta` (local).
    fn seeded_vault() -> (TempDir, VaultConfig) {
        let dir = tempdir().expect("tempdir");
        let config = VaultConfig::new(dir.path());
        let conn = open_rw_db(&config.db_path()).expect("open_rw_db");
        conn.execute_batch(
            "INSERT INTO sources (id, name, kind, origin) VALUES
                 (1, 'alpha', 'web', 'https://alpha.example.com/'),
                 (2, 'beta', 'local', '/tmp/beta');
             INSERT INTO pages (source_id, uri, title, content_md, content_hash, status,
                                http_status, error_message, last_checked_at) VALUES
                 (1, 'https://alpha.example.com/guide', 'Alpha guide',
                  'Install the vault with cargo.', 'h1', 'active', 200, NULL, '2026-11-01T00:00:00Z'),
                 (1, 'https://alpha.example.com/api/v2/missing', 'Gone',
                  'Old copy about tokio-runtime.', 'h2', 'error', 404, 'HTTP 404', '2026-11-03T00:00:00Z'),
                 (1, 'https://alpha.example.com/old', 'Old',
                  'Retired page.', 'h3', 'stale', NULL, NULL, '2026-11-02T00:00:00Z'),
                 (2, 'notes.md', 'Beta notes',
                  'Guides for the vault.', 'h4', 'active', NULL, NULL, '2026-11-01T00:00:00Z');",
        )
        .expect("seed vault");
        (dir, config)
    }

    fn uris(pages: &[PageListItem]) -> Vec<&str> {
        pages.iter().map(|p| p.uri.as_str()).collect()
    }

    fn search(term: &str) -> PageQuery {
        PageQuery {
            search: Some(term.to_string()),
            ..PageQuery::default()
        }
    }

    #[test]
    fn lists_pages_of_every_source_with_http_status() {
        let (_dir, config) = seeded_vault();
        let pages = list_pages(&config, &PageQuery::default()).expect("list all");
        assert_eq!(pages.len(), 4);
        let gone = pages
            .iter()
            .find(|p| p.uri.ends_with("/missing"))
            .expect("error page listed");
        assert_eq!(gone.source_name, "alpha");
        assert_eq!(gone.http_status, Some(404));
        assert_eq!(gone.error_message.as_deref(), Some("HTTP 404"));
        assert!(pages.iter().any(|p| p.source_name == "beta"));
    }

    #[test]
    fn page_list_has_no_row_cap() {
        let (_dir, config) = seeded_vault();
        let conn = open_rw_db(&config.db_path()).expect("open_rw_db");
        for i in 0..150 {
            conn.execute(
                "INSERT INTO pages (source_id, uri, content_md, content_hash)
                 VALUES (2, ?1, 'x', 'h')",
                [format!("bulk/{i:03}.md")],
            )
            .expect("insert bulk page");
        }
        let pages = list_pages(&config, &PageQuery::default()).expect("list all");
        assert_eq!(pages.len(), 154);
    }

    #[test]
    fn filters_pages_by_source_and_status_set() {
        let (_dir, config) = seeded_vault();
        let issues = list_pages(
            &config,
            &PageQuery {
                statuses: PageStatuses::parse("stale, error").expect("parse"),
                ..PageQuery::default()
            },
        )
        .expect("stale or error");
        assert_eq!(
            uris(&issues),
            vec![
                "https://alpha.example.com/api/v2/missing",
                "https://alpha.example.com/old"
            ]
        );

        let beta = list_pages(
            &config,
            &PageQuery {
                source_id: Some(2),
                statuses: PageStatuses::parse("active").expect("parse"),
                ..PageQuery::default()
            },
        )
        .expect("beta active");
        assert_eq!(uris(&beta), vec!["notes.md"]);
    }

    #[test]
    fn rejects_unknown_page_status() {
        assert_eq!(PageStatuses::parse("").expect("blank"), PageStatuses::ANY);
        assert_eq!(
            PageStatuses::parse("error,stale").expect("pair"),
            PageStatuses::STALE_OR_ERROR
        );
        assert!(matches!(
            PageStatuses::parse("active,gone"),
            Err(VaultError::Validation(_))
        ));
    }

    #[test]
    fn search_matches_words_in_content_and_substrings_of_the_uri() {
        let (_dir, config) = seeded_vault();
        // Stemmed FTS word match in title or content, across Sources ("Guides").
        let found = list_pages(&config, &search("guide")).expect("search guide");
        let mut found = uris(&found);
        found.sort_unstable();
        assert_eq!(found, vec!["https://alpha.example.com/guide", "notes.md"]);
        // URI substring, which FTS does not index.
        let found = list_pages(&config, &search("api/v2")).expect("search uri");
        assert_eq!(
            uris(&found),
            vec!["https://alpha.example.com/api/v2/missing"]
        );
        // Prefix search keeps working.
        let found = list_pages(&config, &search("instal*")).expect("search prefix");
        assert_eq!(uris(&found), vec!["https://alpha.example.com/guide"]);
    }

    #[test]
    fn search_terms_that_fts5_rejects_do_not_fail() {
        let (_dir, config) = seeded_vault();
        for term in [
            "tokio-runtime",
            "\"unterminated",
            "foo:bar",
            "a AND",
            "*",
            "NEAR(",
            "\"",
            "say \"hi\"",
        ] {
            list_pages(&config, &search(term))
                .unwrap_or_else(|err| panic!("search {term:?} failed: {err}"));
        }
        let found = list_pages(&config, &search("tokio-runtime")).expect("hyphenated");
        assert_eq!(
            uris(&found),
            vec!["https://alpha.example.com/api/v2/missing"]
        );
    }

    #[test]
    fn search_surfaces_real_errors() {
        let (_dir, config) = seeded_vault();
        let conn = open_rw_db(&config.db_path()).expect("open_rw_db");
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS pages_ai; DROP TRIGGER IF EXISTS pages_ad;
             DROP TRIGGER IF EXISTS pages_au; DROP TABLE pages_fts;",
        )
        .expect("break pages_fts");
        drop(conn);
        let err = list_pages(&config, &search("guide")).expect_err("broken index must fail");
        assert!(err.to_string().contains("pages_fts"), "{err}");
    }

    #[test]
    fn missing_vault_reads_as_empty_and_is_not_created() {
        let dir = tempdir().expect("tempdir");
        let config = VaultConfig::new(dir.path().join("absent"));

        assert!(list_sources(&config).expect("list").is_empty());
        assert!(all_sources(&config).expect("all").is_empty());
        assert!(due_web_sources(&config, 0).expect("due").is_empty());
        assert!(list_pages(&config, &search("x")).expect("pages").is_empty());
        assert!(matches!(
            show_source(&config, "alpha"),
            Err(VaultError::NotFound(_))
        ));
        assert!(matches!(
            find_source(&config, "1"),
            Err(VaultError::NotFound(_))
        ));
        assert!(!config.db_path().exists(), "a read created the vault");
    }

    #[test]
    fn finds_sources_by_id_then_name() {
        let (_dir, config) = seeded_vault();
        assert_eq!(find_source(&config, "2").expect("by id").name, "beta");
        assert_eq!(find_source(&config, "alpha").expect("by name").id, 1);
        assert!(matches!(
            find_source(&config, "99"),
            Err(VaultError::NotFound(_))
        ));
        let names: Vec<String> = all_sources(&config)
            .expect("all")
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn show_source_lists_stale_and_error_pages() {
        let (_dir, config) = seeded_vault();
        let detail = show_source(&config, "alpha").expect("show alpha");
        assert_eq!(detail.source.kind, SourceKind::Web);
        assert_eq!(detail.source.active_pages, 1);
        assert_eq!(
            uris(&detail.issue_pages),
            vec![
                "https://alpha.example.com/api/v2/missing",
                "https://alpha.example.com/old"
            ]
        );
        assert_eq!(detail.issue_pages[0].http_status, Some(404));
    }

    #[test]
    fn out_of_range_stored_limits_are_read_errors() {
        let (_dir, config) = seeded_vault();
        let conn = open_rw_db(&config.db_path()).expect("open_rw_db");
        conn.execute("UPDATE sources SET max_pages = -1 WHERE id = 1", [])
            .expect("corrupt max_pages");
        drop(conn);
        let err = find_source(&config, "alpha").expect_err("negative max_pages");
        assert!(err.to_string().contains("max_pages"), "{err}");
        assert!(list_sources(&config).is_err());
    }

    #[test]
    fn selects_only_due_web_sources() {
        let dir = tempdir().expect("tempdir");
        let config = VaultConfig::new(dir.path());
        let conn = open_rw_db(&config.db_path()).expect("open_rw_db");

        // Reference time: 2026-11-21T12:00:00Z
        let now_epoch: i64 = conn
            .query_row("SELECT unixepoch('2026-11-21T12:00:00Z')", [], |r| r.get(0))
            .expect("unixepoch");

        conn.execute_batch(
            "INSERT INTO sources (name, kind, origin, refresh_interval_secs, status, last_synced_at) VALUES
                 -- Overdue (last synced 2h ago, interval 1h): due
                 ('overdue-web', 'web', 'https://overdue.example.com', 3600, 'ok', '2026-11-21T10:00:00Z'),
                 -- Never synced, interval > 0: due
                 ('never-synced-web', 'web', 'https://never.example.com', 86400, 'idle', NULL),
                 -- Synced 10m ago, interval 1h: skip
                 ('recent-web', 'web', 'https://recent.example.com', 3600, 'ok', '2026-11-21T11:50:00Z'),
                 -- Manual only: skip
                 ('disabled-web', 'web', 'https://disabled.example.com', 0, 'idle', NULL),
                 -- Local, even if never synced: skip
                 ('local-docs', 'local', '/tmp/local-docs', 3600, 'idle', NULL),
                 -- Left 'syncing' by a crashed run: due; the Sync claim decides exclusion
                 ('syncing-web', 'web', 'https://syncing.example.com', 86400, 'syncing', NULL);",
        )
        .expect("seed sources");
        drop(conn);

        let mut names: Vec<String> = due_web_sources(&config, now_epoch)
            .expect("due_web_sources")
            .into_iter()
            .map(|s| s.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["never-synced-web", "overdue-web", "syncing-web"]
        );
    }

    #[test]
    fn failed_sync_waits_for_refresh_interval() {
        let dir = tempdir().expect("tempdir");
        let config = VaultConfig::new(dir.path());
        let conn = open_rw_db(&config.db_path()).expect("open_rw_db");
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
        drop(conn);

        let due = due_web_sources(&config, now_epoch).expect("due");
        assert!(due.is_empty(), "retried before its interval: {due:?}");
        let due_later = due_web_sources(&config, now_epoch + 3_000).expect("due later");
        assert_eq!(due_later.len(), 1);
    }
}
