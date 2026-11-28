//! Writing a Sync's pages: the one path both drivers' outcomes flow through.
//!
//! A driver (the folder walk in [`super::local`], the web crawl in [`super::web`]) only
//! collects [`SyncPages`]; the Source lifecycle writes them with [`write_pages`] in the same
//! transaction that closes the Sync run.

use std::collections::HashSet;
use std::time::Duration;

use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ingest::normalize::{unified_diff, NormalizedDocument};
use crate::ingest::sections::SectionChunk;

/// Summary metrics produced by syncing a single source.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SyncSummary {
    pub source_name: String,
    pub pages_added: usize,
    pub pages_updated: usize,
    pub pages_unchanged: usize,
    pub pages_stale: usize,
    pub pages_failed: usize,
    pub pruned: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// How [`write_pages`] treats stored pages the Sync did not see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingPagePolicy {
    /// Mark previously active/error pages not seen in this crawl as `'stale'`.
    MarkStale,
    /// Leave unvisited pages untouched (used when a web crawl hit `max_pages` without `--prune`).
    Preserve,
    /// Delete pages not seen in this crawl (`--prune`).
    Prune,
}

/// One page's result from a driver (folder walk or web crawl), before it is written.
pub(crate) enum PageIngestOutcome {
    Ready {
        uri: String,
        doc: NormalizedDocument,
        chunks: Vec<SectionChunk>,
        etag: Option<String>,
        last_modified: Option<String>,
        http_status: Option<i64>,
    },
    Failed {
        uri: String,
        http_status: Option<i64>,
        error_message: String,
        insert_if_missing: bool,
    },
    /// A URI this Sync visited that no longer holds a Page of its own. Its stored row is
    /// retired (stale, or deleted with `--prune`) whatever the missing-page policy, since the
    /// Sync did see it; no row is inserted for it.
    Retired { uri: String },
}

/// Called once a Sync's pages were written (`true`) or failed to write (`false`), with how
/// long the write took.
pub(crate) type OnWritten = Box<dyn FnOnce(bool, Duration) + Send>;

/// Everything a driver collected in one Sync, ready to write.
pub(crate) struct SyncPages {
    pub outcomes: Vec<PageIngestOutcome>,
    pub missing_policy: MissingPagePolicy,
    pub warnings: Vec<String>,
    /// The driver's own report of the write, e.g. the web `web_sync_summary` line.
    pub on_written: Option<OnWritten>,
}

impl std::fmt::Debug for SyncPages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncPages")
            .field("outcomes", &self.outcomes.len())
            .field("missing_policy", &self.missing_policy)
            .field("warnings", &self.warnings)
            .finish_non_exhaustive()
    }
}

impl SyncPages {
    pub(crate) fn new(outcomes: Vec<PageIngestOutcome>, missing_policy: MissingPagePolicy) -> Self {
        Self {
            outcomes,
            missing_policy,
            warnings: Vec::new(),
            on_written: None,
        }
    }
}

/// Write one Sync's page outcomes inside `tx`: store pages and their sections, record diffs,
/// and retire pages the Sync did not see according to its [`MissingPagePolicy`]. Closing the
/// Sync run is the caller's, in the same transaction.
pub(crate) fn write_pages(
    tx: &Transaction<'_>,
    source_id: i64,
    source_name: &str,
    pages: SyncPages,
) -> Result<SyncSummary> {
    let SyncPages {
        outcomes,
        missing_policy,
        warnings,
        ..
    } = pages;
    let mut summary = SyncSummary {
        source_name: source_name.to_string(),
        warnings,
        ..Default::default()
    };
    let mut seen_uris: HashSet<String> = HashSet::with_capacity(outcomes.len());
    let mut retired: Vec<String> = Vec::new();

    for outcome in outcomes {
        match outcome {
            PageIngestOutcome::Retired { uri } => retired.push(uri),
            PageIngestOutcome::Failed {
                uri,
                http_status,
                error_message,
                insert_if_missing,
            } => {
                seen_uris.insert(uri.clone());
                summary.pages_failed += 1;
                let existing_id: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM pages WHERE source_id = ?1 AND uri = ?2",
                        params![source_id, uri],
                        |row| row.get(0),
                    )
                    .optional()?;

                match existing_id {
                    Some(page_id) => {
                        tx.execute(
                            "UPDATE pages
                             SET status = 'error',
                                 http_status = ?2,
                                 error_message = ?3,
                                 last_checked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                             WHERE id = ?1",
                            params![page_id, http_status, error_message],
                        )?;
                    }
                    None if insert_if_missing => {
                        tx.execute(
                            "INSERT INTO pages (
                                 source_id, uri, title, content_md, content_hash, word_count,
                                 status, http_status, error_message
                             ) VALUES (?1, ?2, '', '', '', 0, 'error', ?3, ?4)",
                            params![source_id, uri, http_status, error_message],
                        )?;
                    }
                    None => {}
                }
            }
            PageIngestOutcome::Ready {
                uri,
                doc,
                chunks,
                etag,
                last_modified,
                http_status,
            } => {
                seen_uris.insert(uri.clone());
                let existing: Option<(i64, String, String, String)> = tx
                    .query_row(
                        "SELECT id, content_hash, content_md, status
                         FROM pages
                         WHERE source_id = ?1 AND uri = ?2",
                        params![source_id, uri],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;

                match existing {
                    None => {
                        tx.execute(
                            "INSERT INTO pages (
                                 source_id, uri, title, content_md, content_hash, word_count,
                                 etag, last_modified, status, http_status
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9)",
                            params![
                                source_id,
                                uri,
                                doc.title,
                                doc.content_md,
                                doc.content_hash,
                                i64::try_from(doc.word_count).unwrap_or(i64::MAX),
                                etag,
                                last_modified,
                                http_status,
                            ],
                        )?;
                        let page_id = tx.last_insert_rowid();
                        insert_sections(tx, page_id, source_id, &chunks)?;
                        summary.pages_added += 1;
                    }
                    Some((page_id, old_hash, old_content_md, old_status)) => {
                        if old_hash.is_empty() {
                            tx.execute(
                                "UPDATE pages
                                 SET title = ?2,
                                     content_md = ?3,
                                     content_hash = ?4,
                                     word_count = ?5,
                                     etag = ?6,
                                     last_modified = ?7,
                                     status = 'active',
                                     http_status = ?8,
                                     error_message = NULL,
                                     last_checked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                                     last_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                                 WHERE id = ?1",
                                params![
                                    page_id,
                                    doc.title,
                                    doc.content_md,
                                    doc.content_hash,
                                    i64::try_from(doc.word_count).unwrap_or(i64::MAX),
                                    etag,
                                    last_modified,
                                    http_status,
                                ],
                            )?;
                            tx.execute("DELETE FROM sections WHERE page_id = ?1", [page_id])?;
                            insert_sections(tx, page_id, source_id, &chunks)?;
                            summary.pages_added += 1;
                        } else if old_hash == doc.content_hash {
                            if old_status != "active"
                                || http_status.is_some()
                                || etag.is_some()
                                || last_modified.is_some()
                            {
                                tx.execute(
                                    "UPDATE pages
                                     SET status = 'active',
                                         error_message = NULL,
                                         etag = COALESCE(?2, etag),
                                         last_modified = COALESCE(?3, last_modified),
                                         http_status = COALESCE(?4, http_status),
                                         last_checked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                                     WHERE id = ?1",
                                    params![page_id, etag, last_modified, http_status],
                                )?;
                            } else {
                                tx.execute(
                                    "UPDATE pages
                                     SET last_checked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                                     WHERE id = ?1",
                                    [page_id],
                                )?;
                            }
                            summary.pages_unchanged += 1;
                        } else {
                            let diff = unified_diff(&old_content_md, &doc.content_md);
                            tx.execute(
                                "INSERT INTO page_revisions (page_id, previous_hash, new_hash, unified_diff, changed_at)
                                 VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
                                 ON CONFLICT(page_id) DO UPDATE SET
                                     previous_hash = excluded.previous_hash,
                                     new_hash = excluded.new_hash,
                                     unified_diff = excluded.unified_diff,
                                     changed_at = excluded.changed_at",
                                params![page_id, old_hash, doc.content_hash, diff],
                            )?;
                            tx.execute(
                                "UPDATE pages
                                 SET title = ?2,
                                     content_md = ?3,
                                     content_hash = ?4,
                                     word_count = ?5,
                                     etag = ?6,
                                     last_modified = ?7,
                                     status = 'active',
                                     http_status = ?8,
                                     error_message = NULL,
                                     last_checked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                                     last_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                                 WHERE id = ?1",
                                params![
                                    page_id,
                                    doc.title,
                                    doc.content_md,
                                    doc.content_hash,
                                    i64::try_from(doc.word_count).unwrap_or(i64::MAX),
                                    etag,
                                    last_modified,
                                    http_status,
                                ],
                            )?;
                            tx.execute("DELETE FROM sections WHERE page_id = ?1", [page_id])?;
                            insert_sections(tx, page_id, source_id, &chunks)?;
                            summary.pages_updated += 1;
                        }
                    }
                }
            }
        }
    }

    retire_visited_pages(
        tx,
        source_id,
        retired,
        &mut seen_uris,
        missing_policy == MissingPagePolicy::Prune,
        &mut summary,
    )?;

    match missing_policy {
        MissingPagePolicy::Preserve => {}
        MissingPagePolicy::MarkStale => {
            reconcile_missing_pages(tx, source_id, &seen_uris, false, &mut summary)?;
        }
        MissingPagePolicy::Prune => {
            reconcile_missing_pages(tx, source_id, &seen_uris, true, &mut summary)?;
        }
    }

    Ok(summary)
}

fn insert_sections(
    tx: &Transaction<'_>,
    page_id: i64,
    source_id: i64,
    chunks: &[crate::ingest::sections::SectionChunk],
) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "INSERT INTO sections (page_id, source_id, ordinal, heading, heading_path, level, content_md, word_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for chunk in chunks {
        stmt.execute(params![
            page_id,
            source_id,
            i64::try_from(chunk.ordinal).unwrap_or(i64::MAX),
            chunk.heading,
            chunk.heading_path,
            i64::from(chunk.level),
            chunk.content_md,
            i64::try_from(chunk.word_count).unwrap_or(i64::MAX),
        ])?;
    }
    Ok(())
}

/// Retire the stored row of each `Retired` URI, and record it as seen so the missing-page pass
/// does not count it again. A URI that also got its own result is left alone.
fn retire_visited_pages(
    tx: &Transaction<'_>,
    source_id: i64,
    retired: Vec<String>,
    seen_uris: &mut HashSet<String>,
    prune: bool,
    summary: &mut SyncSummary,
) -> Result<()> {
    let mut lookup =
        tx.prepare_cached("SELECT id, status FROM pages WHERE source_id = ?1 AND uri = ?2")?;
    for uri in retired {
        if !seen_uris.insert(uri.clone()) {
            continue;
        }
        let existing: Option<(i64, String)> = lookup
            .query_row(params![source_id, uri], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        if let Some((page_id, status)) = existing {
            retire_page(tx, page_id, &status, prune, summary)?;
        }
    }
    Ok(())
}

/// Delete a page that is no longer at its Origin (`prune`), or mark an active or error page
/// stale.
fn retire_page(
    tx: &Transaction<'_>,
    page_id: i64,
    status: &str,
    prune: bool,
    summary: &mut SyncSummary,
) -> Result<()> {
    if prune {
        tx.execute("DELETE FROM pages WHERE id = ?1", [page_id])?;
        summary.pruned += 1;
    } else if status == "active" || status == "error" {
        tx.execute(
            "UPDATE pages
             SET status = 'stale',
                 last_checked_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE id = ?1",
            [page_id],
        )?;
        summary.pages_stale += 1;
    }
    Ok(())
}

fn reconcile_missing_pages(
    tx: &Transaction<'_>,
    source_id: i64,
    seen_uris: &HashSet<String>,
    prune: bool,
    summary: &mut SyncSummary,
) -> Result<()> {
    let missing: Vec<(i64, String)> = {
        let mut stmt = tx.prepare(
            "SELECT id, uri, status
             FROM pages
             WHERE source_id = ?1",
        )?;
        let rows = stmt.query_map([source_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut collected = Vec::new();
        for row in rows {
            let (id, uri, status) = row?;
            if !seen_uris.contains(&uri) {
                collected.push((id, status));
            }
        }
        collected
    };

    for (page_id, status) in missing {
        retire_page(tx, page_id, &status, prune, summary)?;
    }

    Ok(())
}
