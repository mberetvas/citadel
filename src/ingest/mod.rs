#[cfg(feature = "chrome")]
mod browser;
pub mod local;
pub mod normalize;
pub(crate) mod persist;
pub mod sections;
pub mod web;

use rusqlite::{Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::config::VaultConfig;
use crate::db::open_rw_db;
use crate::error::{Result, VaultError};

pub use local::canonicalize_local_dir;
pub use normalize::{
    count_words, extract_title, normalize_markdown, normalize_whitespace, sha256_hex,
    strip_volatile_frontmatter, unified_diff, NormalizedDocument, MAX_DIFF_BYTES,
};
pub use persist::SyncSummary;
pub use sections::{split_sections, SectionChunk};

pub(crate) const DEFAULT_MAX_PAGES: u32 = 500;
pub(crate) const DEFAULT_MAX_DEPTH: u32 = 5;

/// Supported source kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Web,
    Local,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Local => "local",
        }
    }

    fn from_db(raw: &str) -> Result<Self> {
        match raw {
            "web" => Ok(Self::Web),
            "local" => Ok(Self::Local),
            other => Err(VaultError::Other(format!("unknown source kind '{other}'"))),
        }
    }
}

/// Stored row from `sources`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRecord {
    pub id: i64,
    pub name: String,
    pub kind: SourceKind,
    pub origin: String,
    pub tags: Vec<String>,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub max_pages: u32,
    pub max_depth: u32,
    pub render_js: bool,
    pub refresh_interval_secs: i64,
    pub status: String,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
}

/// Source row joined with live counts from `v_sources`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSummary {
    pub id: i64,
    pub name: String,
    pub kind: SourceKind,
    pub origin: String,
    pub tags: Vec<String>,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub max_pages: u32,
    pub max_depth: u32,
    pub render_js: bool,
    pub refresh_interval_secs: i64,
    pub status: String,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub active_pages: i64,
    pub stale_pages: i64,
    pub error_pages: i64,
    pub total_words: i64,
}

/// A single row from `sync_runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRunRecord {
    pub id: i64,
    pub source_id: i64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub pages_added: i64,
    pub pages_updated: i64,
    pub pages_unchanged: i64,
    pub pages_stale: i64,
    pub pages_failed: i64,
    pub error_message: Option<String>,
}

/// Summary of a stale or failing page belonging to a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageIssueRecord {
    pub id: i64,
    pub uri: String,
    pub title: String,
    pub status: String,
    pub http_status: Option<i64>,
    pub error_message: Option<String>,
    pub last_checked_at: String,
}

/// Detailed source view including recent sync history and non-active pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDetail {
    pub source: SourceSummary,
    pub sync_runs: Vec<SyncRunRecord>,
    pub issue_pages: Vec<PageIssueRecord>,
}

/// Parse a duration string such as `"0"`, `"30s"`, `"15m"`, `"24h"`, `"7d"`, `"2w"`, or raw seconds.
pub fn parse_duration_secs(input: &str) -> Result<i64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(VaultError::Validation("duration must not be empty".into()));
    }

    if trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return trimmed
            .parse::<i64>()
            .map_err(|_| VaultError::Validation(format!("duration '{input}' is out of range")));
    }

    let Some(unit) = trimmed.chars().last() else {
        return Err(VaultError::Validation("duration must not be empty".into()));
    };

    let multiplier: i64 = match unit.to_ascii_lowercase() {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        'w' => 604_800,
        _ => {
            return Err(VaultError::Validation(format!(
                "invalid duration '{input}'; expected e.g. '0', '30s', '15m', '24h', '7d'"
            )));
        }
    };

    let num_part = &trimmed[..trimmed.len() - unit.len_utf8()];
    if num_part.is_empty() || !num_part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(VaultError::Validation(format!(
            "invalid duration '{input}'; expected non-negative integer before unit '{unit}'"
        )));
    }

    let qty = num_part
        .parse::<i64>()
        .map_err(|_| VaultError::Validation(format!("duration '{input}' is out of range")))?;

    qty.checked_mul(multiplier)
        .ok_or_else(|| VaultError::Validation(format!("duration '{input}' overflows i64")))
}

/// List all sources with live page and word counts from `v_sources`.
pub fn list_sources(config: &VaultConfig) -> Result<Vec<SourceSummary>> {
    if !config.db_path().exists() {
        return Ok(Vec::new());
    }
    let conn = open_rw_db(&config.db_path())?;
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, origin, tags, include_patterns, exclude_patterns,
                max_pages, max_depth, render_js, refresh_interval_secs,
                status, last_synced_at, last_error, created_at,
                active_pages, stale_pages, error_pages, total_words
         FROM v_sources
         ORDER BY name ASC",
    )?;
    let rows = stmt.query_map([], map_source_summary)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Retrieve source details, recent sync runs, and stale/error pages for `name`.
pub fn show_source(config: &VaultConfig, name: &str) -> Result<SourceDetail> {
    if !config.db_path().exists() {
        return Err(VaultError::NotFound(format!("source '{name}' not found")));
    }
    let conn = open_rw_db(&config.db_path())?;
    let source = conn
        .query_row(
            "SELECT id, name, kind, origin, tags, include_patterns, exclude_patterns,
                    max_pages, max_depth, render_js, refresh_interval_secs,
                    status, last_synced_at, last_error, created_at,
                    active_pages, stale_pages, error_pages, total_words
             FROM v_sources
             WHERE name = ?1",
            [name],
            map_source_summary,
        )
        .optional()?
        .ok_or_else(|| VaultError::NotFound(format!("source '{name}' not found")))?;

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
                id: row.get(0)?,
                source_id: row.get(1)?,
                started_at: row.get(2)?,
                finished_at: row.get(3)?,
                status: row.get(4)?,
                pages_added: row.get(5)?,
                pages_updated: row.get(6)?,
                pages_unchanged: row.get(7)?,
                pages_stale: row.get(8)?,
                pages_failed: row.get(9)?,
                error_message: row.get(10)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut issues_stmt = conn.prepare(
        "SELECT id, uri, title, status, http_status, error_message, last_checked_at
         FROM pages
         WHERE source_id = ?1 AND status IN ('stale', 'error')
         ORDER BY uri ASC",
    )?;
    let issue_pages = issues_stmt
        .query_map([source.id], |row| {
            Ok(PageIssueRecord {
                id: row.get(0)?,
                uri: row.get(1)?,
                title: row.get(2)?,
                status: row.get(3)?,
                http_status: row.get(4)?,
                error_message: row.get(5)?,
                last_checked_at: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(SourceDetail {
        source,
        sync_runs,
        issue_pages,
    })
}

fn parse_json_list(raw: &str) -> Result<Vec<String>> {
    serde_json::from_str(raw)
        .map_err(|err| VaultError::Other(format!("failed to decode stored JSON list: {err}")))
}

pub(crate) fn map_source_record(row: &Row<'_>) -> rusqlite::Result<SourceRecord> {
    let kind_raw: String = row.get(2)?;
    let tags_raw: String = row.get(4)?;
    let includes_raw: String = row.get(5)?;
    let excludes_raw: String = row.get(6)?;
    let max_pages: i64 = row.get(7)?;
    let max_depth: i64 = row.get(8)?;
    let render_js: i64 = row.get(9)?;

    let kind = SourceKind::from_db(&kind_raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let tags = parse_json_list(&tags_raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let include_patterns = parse_json_list(&includes_raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let exclude_patterns = parse_json_list(&excludes_raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(SourceRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        kind,
        origin: row.get(3)?,
        tags,
        include_patterns,
        exclude_patterns,
        max_pages: u32::try_from(max_pages).unwrap_or(DEFAULT_MAX_PAGES),
        max_depth: u32::try_from(max_depth).unwrap_or(DEFAULT_MAX_DEPTH),
        render_js: render_js != 0,
        refresh_interval_secs: row.get(10)?,
        status: row.get(11)?,
        last_synced_at: row.get(12)?,
        last_error: row.get(13)?,
        created_at: row.get(14)?,
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
        active_pages: row.get(15)?,
        stale_pages: row.get(16)?,
        error_pages: row.get(17)?,
        total_words: row.get(18)?,
    })
}

pub(crate) fn load_source_by_id(conn: &Connection, id: i64) -> Result<SourceRecord> {
    conn.query_row(
        "SELECT id, name, kind, origin, tags, include_patterns, exclude_patterns,
                max_pages, max_depth, render_js, refresh_interval_secs,
                status, last_synced_at, last_error, created_at
         FROM sources WHERE id = ?1",
        [id],
        map_source_record,
    )
    .optional()?
    .ok_or_else(|| VaultError::NotFound(format!("source id {id} not found")))
}

pub(crate) fn load_source_by_name(conn: &Connection, name: &str) -> Result<SourceRecord> {
    conn.query_row(
        "SELECT id, name, kind, origin, tags, include_patterns, exclude_patterns,
                max_pages, max_depth, render_js, refresh_interval_secs,
                status, last_synced_at, last_error, created_at
         FROM sources WHERE name = ?1",
        [name],
        map_source_record,
    )
    .optional()?
    .ok_or_else(|| VaultError::NotFound(format!("source '{name}' not found")))
}

pub(crate) fn load_all_sources(conn: &Connection) -> Result<Vec<SourceRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, origin, tags, include_patterns, exclude_patterns,
                max_pages, max_depth, render_js, refresh_interval_secs,
                status, last_synced_at, last_error, created_at
         FROM sources ORDER BY name ASC",
    )?;
    let rows = stmt.query_map([], map_source_record)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations_cleanly() {
        assert_eq!(parse_duration_secs("0").expect("0"), 0);
        assert_eq!(parse_duration_secs("45").expect("45"), 45);
        assert_eq!(parse_duration_secs("30s").expect("30s"), 30);
        assert_eq!(parse_duration_secs("15m").expect("15m"), 900);
        assert_eq!(parse_duration_secs("24H").expect("24H"), 86_400);
        assert_eq!(parse_duration_secs("7d").expect("7d"), 604_800);
        assert_eq!(parse_duration_secs("1w").expect("1w"), 604_800);

        assert!(matches!(
            parse_duration_secs("").unwrap_err(),
            VaultError::Validation(_)
        ));
        assert!(matches!(
            parse_duration_secs("-5m").unwrap_err(),
            VaultError::Validation(_)
        ));
        assert!(matches!(
            parse_duration_secs("10x").unwrap_err(),
            VaultError::Validation(_)
        ));
    }
}
