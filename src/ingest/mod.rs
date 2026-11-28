#[cfg(feature = "chrome")]
mod browser;
pub mod local;
pub mod normalize;
pub(crate) mod persist;
mod read;
pub mod sections;
pub mod web;

use serde::{Deserialize, Serialize};

use crate::error::{Result, VaultError};

pub use local::canonicalize_local_dir;
pub use normalize::{
    count_words, extract_title, normalize_markdown, normalize_whitespace, sha256_hex,
    strip_volatile_frontmatter, unified_diff, NormalizedDocument, MAX_DIFF_BYTES,
};
pub use persist::SyncSummary;
pub use read::{
    all_sources, due_web_sources, find_source, list_pages, list_sources, show_source, PageListItem,
    PageQuery, PageStatuses,
};
pub(crate) use read::{load_all_sources, load_source_by_id, load_source_by_name, read_vault};
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

/// Detailed source view including recent sync history and non-active pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDetail {
    pub source: SourceSummary,
    pub sync_runs: Vec<SyncRunRecord>,
    pub issue_pages: Vec<PageListItem>,
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
