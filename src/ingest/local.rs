use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use glob::Pattern;

use crate::error::{Result, VaultError};
use crate::ingest::normalize::normalize_markdown;
use crate::ingest::persist::{MissingPagePolicy, PageIngestOutcome, SyncPages};
use crate::ingest::sections::split_sections;

#[derive(Debug, Clone)]
enum CompiledPattern {
    Prefix(String),
    Glob(Pattern),
}

impl CompiledPattern {
    fn compile(raw: &str) -> Result<Self> {
        let trimmed = raw.trim().trim_start_matches("./").trim_start_matches('/');
        if trimmed.is_empty() {
            return Err(VaultError::Validation(
                "include/exclude pattern must not be empty".into(),
            ));
        }
        if trimmed.contains(['*', '?', '[']) {
            let pattern = Pattern::new(trimmed).map_err(|err| {
                VaultError::Validation(format!("invalid glob pattern '{raw}': {err}"))
            })?;
            Ok(Self::Glob(pattern))
        } else {
            Ok(Self::Prefix(trimmed.to_string()))
        }
    }

    fn matches(&self, uri: &str) -> bool {
        match self {
            Self::Glob(pat) => pat.matches(uri),
            Self::Prefix(prefix) => {
                let clean = prefix.trim_end_matches('/');
                uri == prefix || uri == clean || uri.starts_with(&format!("{clean}/"))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PatternMatcher {
    includes: Vec<CompiledPattern>,
    excludes: Vec<CompiledPattern>,
}

impl PatternMatcher {
    pub(crate) fn new(includes: &[String], excludes: &[String]) -> Result<Self> {
        let includes = includes
            .iter()
            .map(|p| CompiledPattern::compile(p))
            .collect::<Result<Vec<_>>>()?;
        let excludes = excludes
            .iter()
            .map(|p| CompiledPattern::compile(p))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { includes, excludes })
    }

    pub(crate) fn matches(&self, uri: &str) -> bool {
        let included = self.includes.is_empty() || self.includes.iter().any(|pat| pat.matches(uri));
        if !included {
            return false;
        }
        !self.excludes.iter().any(|pat| pat.matches(uri))
    }
}

/// Validate that all glob/prefix patterns compile cleanly.
pub(crate) fn validate_patterns(patterns: &[String]) -> Result<()> {
    for pat in patterns {
        CompiledPattern::compile(pat)?;
    }
    Ok(())
}

/// Validate that `origin` exists and is a directory, returning its canonical path.
pub fn canonicalize_local_dir(origin: &Path) -> Result<PathBuf> {
    if origin.as_os_str().is_empty() {
        return Err(VaultError::Validation(
            "local origin directory path must not be empty".into(),
        ));
    }

    let canonical = fs::canonicalize(origin).map_err(|err| {
        VaultError::Validation(format!(
            "local origin directory '{}' is invalid: {err}",
            origin.display()
        ))
    })?;

    if !canonical.is_dir() {
        return Err(VaultError::Validation(format!(
            "local origin '{}' is not a directory",
            origin.display()
        )));
    }

    Ok(canonical)
}

/// The folder-walk driver: collect the Markdown pages of one local Source. It only reads
/// the folder; the Source lifecycle writes the pages and records the Sync run.
pub(crate) fn collect(
    source_name: &str,
    origin_dir: &Path,
    include_patterns: &[String],
    exclude_patterns: &[String],
    prune: bool,
) -> Result<SyncPages> {
    let canon_root = canonicalize_local_dir(origin_dir)?;
    let matcher = PatternMatcher::new(include_patterns, exclude_patterns)?;
    let missing_policy = if prune {
        MissingPagePolicy::Prune
    } else {
        MissingPagePolicy::MarkStale
    };
    let outcomes = collect_local_outcomes(source_name, &canon_root, &matcher)?;
    Ok(SyncPages::new(outcomes, missing_policy))
}

fn collect_local_outcomes(
    source_name: &str,
    canon_root: &Path,
    matcher: &PatternMatcher,
) -> Result<Vec<PageIngestOutcome>> {
    let files = collect_markdown_files(canon_root, matcher)?;
    let mut outcomes = Vec::with_capacity(files.len());

    for (uri, abs_path) in files {
        let raw_bytes = match fs::read(&abs_path) {
            Ok(bytes) => bytes,
            Err(io_err) => {
                tracing::warn!(
                    source = source_name,
                    uri = %uri,
                    error = %io_err,
                    "failed to read markdown file"
                );
                outcomes.push(PageIngestOutcome::Failed {
                    uri,
                    http_status: None,
                    error_message: io_err.to_string(),
                    insert_if_missing: false,
                });
                continue;
            }
        };

        let raw_md = match String::from_utf8(raw_bytes) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(
                    source = source_name,
                    uri = %uri,
                    "markdown file is not strict UTF-8; decoding lossily"
                );
                String::from_utf8_lossy(err.as_bytes()).into_owned()
            }
        };

        let fallback_title = abs_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uri.clone());

        let doc = normalize_markdown(&raw_md, &fallback_title);
        let chunks = split_sections(&doc.content_md);
        outcomes.push(PageIngestOutcome::Ready {
            uri,
            doc,
            chunks,
            etag: None,
            last_modified: None,
            http_status: None,
        });
    }

    Ok(outcomes)
}

fn collect_markdown_files(root: &Path, matcher: &PatternMatcher) -> Result<Vec<(String, PathBuf)>> {
    let mut visited_dirs: HashSet<PathBuf> = HashSet::new();
    visited_dirs.insert(root.to_path_buf());

    let mut files = Vec::new();
    walk_dir(root, root, &mut visited_dirs, matcher, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    visited_dirs: &mut HashSet<PathBuf>,
    matcher: &PatternMatcher,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name_lossy = name.to_string_lossy();
        if should_skip_entry(&name_lossy) {
            continue;
        }

        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            let canon_sub = fs::canonicalize(&path)?;
            if visited_dirs.insert(canon_sub) {
                walk_dir(root, &path, visited_dirs, matcher, out)?;
            }
            continue;
        }

        if file_type.is_symlink() {
            match fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => {
                    let canon_sub = fs::canonicalize(&path)?;
                    if visited_dirs.insert(canon_sub) {
                        walk_dir(root, &path, visited_dirs, matcher, out)?;
                    }
                    continue;
                }
                Ok(meta) if meta.is_file() => {}
                Ok(_) => continue,
                Err(_) => {
                    // Preserve broken symlinks with Markdown extensions so sync tracks them as `pages_failed`.
                }
            }
        } else if !file_type.is_file() {
            continue;
        }

        if !is_markdown_file(&path) {
            continue;
        }

        let rel = path.strip_prefix(root).map_err(|err| {
            VaultError::Other(format!(
                "failed to compute relative path for '{}': {err}",
                path.display()
            ))
        })?;
        let uri = relative_path_to_uri(rel)?;
        if matcher.matches(&uri) {
            out.push((uri, path));
        }
    }

    Ok(())
}

fn should_skip_entry(name: &str) -> bool {
    name.starts_with('.') || name == "node_modules"
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

fn relative_path_to_uri(rel: &Path) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for component in rel.components() {
        if let Component::Normal(os_str) = component {
            let part = os_str.to_str().ok_or_else(|| {
                VaultError::Validation(format!("non-UTF-8 path segment in '{}'", rel.display()))
            })?;
            parts.push(part.to_string());
        }
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    //! The folder walk is tested through the Source lifecycle, the interface every entry
    //! point uses: it walks the folder, writes the pages and records the Sync run.

    use super::*;
    use crate::config::VaultConfig;
    use crate::db::open_rw_db;
    use crate::ingest::SyncSummary;
    use crate::sources::{AddSourceParams, SourceLifecycle};
    use rusqlite::Connection;
    use tempfile::tempdir;

    /// A vault with one local Source on `root`, registered but not synced.
    struct LocalVault {
        lifecycle: SourceLifecycle,
        conn: Connection,
        source_id: i64,
        name: String,
    }

    impl LocalVault {
        async fn new(vault: &Path, root: &Path, name: &str) -> Self {
            Self::with_patterns(vault, root, name, Vec::new(), Vec::new()).await
        }

        async fn with_patterns(
            vault: &Path,
            root: &Path,
            name: &str,
            include_patterns: Vec<String>,
            exclude_patterns: Vec<String>,
        ) -> Self {
            let config = VaultConfig::new(vault);
            let lifecycle = SourceLifecycle::new(config.clone());
            let added = lifecycle
                .add(
                    AddSourceParams {
                        origin: root.to_string_lossy().into_owned(),
                        name: Some(name.to_string()),
                        include_patterns,
                        exclude_patterns,
                        ..Default::default()
                    },
                    false,
                )
                .await
                .expect("register local source");
            Self {
                lifecycle,
                conn: open_rw_db(&config.db_path()).expect("open_rw_db"),
                source_id: added.source.id,
                name: name.to_string(),
            }
        }

        async fn sync(&self, prune: bool) -> SyncSummary {
            self.lifecycle
                .sync(&self.name, prune)
                .await
                .expect("sync")
                .synced()
                .expect("claim is free")
        }
    }

    #[tokio::test]
    async fn ingests_nested_markdown_and_ignores_git_hidden_dirs() {
        let vault_dir = tempdir().expect("tempdir should succeed");
        let docs_dir = tempdir().expect("docs tempdir should succeed");
        let root = docs_dir.path();

        std::fs::create_dir_all(root.join("chapters/nested")).expect("create nested dir");
        std::fs::create_dir_all(root.join(".git/objects")).expect("create .git dir");
        std::fs::create_dir_all(root.join("node_modules/pkg")).expect("create node_modules dir");
        std::fs::create_dir_all(root.join(".hidden-dir")).expect("create hidden dir");

        std::fs::write(
            root.join("README.md"),
            "# Vault Book\n\nRoot overview document.\n",
        )
        .expect("write README.md");
        std::fs::write(
            root.join("chapters/ch01.markdown"),
            "---\ntitle: Chapter One\n---\n\n## Basics\n\nContent for chapter one.\n",
        )
        .expect("write ch01.markdown");
        std::fs::write(
            root.join("chapters/nested/deep.MD"),
            "# Deep Topic\n\nNested uppercase extension.\n",
        )
        .expect("write deep.MD");

        // Ignored files/dirs
        std::fs::write(root.join(".git/objects/ignored.md"), "# Git Ignored\n")
            .expect("write .git file");
        std::fs::write(
            root.join("node_modules/pkg/readme.md"),
            "# Node Modules Ignored\n",
        )
        .expect("write node_modules file");
        std::fs::write(root.join(".hidden-dir/secret.md"), "# Hidden Dir\n")
            .expect("write hidden dir file");
        std::fs::write(root.join(".hidden-file.md"), "# Hidden File\n").expect("write hidden file");
        std::fs::write(root.join("notes.txt"), "Not a markdown file.\n").expect("write txt file");

        let vault = LocalVault::new(vault_dir.path(), root, "book").await;
        let (conn, source_id) = (&vault.conn, vault.source_id);
        let summary = vault.sync(false).await;

        assert_eq!(summary.source_name, "book");
        assert_eq!(summary.pages_added, 3);
        assert_eq!(summary.pages_updated, 0);
        assert_eq!(summary.pages_unchanged, 0);
        assert_eq!(summary.pages_stale, 0);
        assert_eq!(summary.pages_failed, 0);
        assert_eq!(summary.pruned, 0);

        let uris: Vec<(String, String, String)> = conn
            .prepare("SELECT uri, title, status FROM pages WHERE source_id = ?1 ORDER BY uri")
            .expect("prepare")
            .query_map([source_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect");

        assert_eq!(
            uris,
            vec![
                (
                    "README.md".to_string(),
                    "Vault Book".to_string(),
                    "active".to_string()
                ),
                (
                    "chapters/ch01.markdown".to_string(),
                    "Chapter One".to_string(),
                    "active".to_string()
                ),
                (
                    "chapters/nested/deep.MD".to_string(),
                    "Deep Topic".to_string(),
                    "active".to_string()
                ),
            ]
        );

        let section_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sections WHERE source_id = ?1",
                [source_id],
                |r| r.get(0),
            )
            .expect("count sections");
        assert_eq!(section_count, 3);

        let fts_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sections_fts WHERE sections_fts MATCH 'uppercase'",
                [],
                |r| r.get(0),
            )
            .expect("query sections_fts");
        assert_eq!(fts_hits, 1);

        let run_status: (String, i64) = conn
            .query_row(
                "SELECT status, pages_added FROM sync_runs WHERE source_id = ?1",
                [source_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("query sync_runs");
        assert_eq!(run_status, ("ok".to_string(), 3));
    }

    #[tokio::test]
    async fn resync_skips_unchanged_and_records_diff_on_change() {
        let vault_dir = tempdir().expect("tempdir should succeed");
        let docs_dir = tempdir().expect("docs tempdir should succeed");
        let root = docs_dir.path();

        std::fs::write(
            root.join("alpha.md"),
            "---\ntitle: Alpha\nupdated: 2026-01-01\n---\n\n# Alpha\n\nOriginal alpha section.\n",
        )
        .expect("write alpha");
        std::fs::write(root.join("beta.md"), "# Beta\n\nStable beta body.\n").expect("write beta");

        let vault = LocalVault::new(vault_dir.path(), root, "docs").await;
        let (conn, source_id) = (&vault.conn, vault.source_id);
        let first = vault.sync(false).await;
        assert_eq!(first.pages_added, 2);

        // Substantive edit on alpha; beta stays untouched.
        std::fs::write(
            root.join("alpha.md"),
            "---\ntitle: Alpha\nupdated: 2026-11-21\n---\n\n# Alpha\n\nUpdated alpha section.\n\n## New Subsection\n\nTokennewsection content.\n",
        )
        .expect("rewrite alpha");

        let second = vault.sync(false).await;

        assert_eq!(second.pages_added, 0);
        assert_eq!(second.pages_updated, 1);
        assert_eq!(second.pages_unchanged, 1);
        assert_eq!(second.pages_stale, 0);
        assert_eq!(second.pages_failed, 0);

        let diff_row: (String, String) = conn
            .query_row(
                "SELECT p.uri, pr.unified_diff
                 FROM page_revisions pr
                 JOIN pages p ON p.id = pr.page_id
                 WHERE p.source_id = ?1",
                [source_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("query page_revisions");
        assert_eq!(diff_row.0, "alpha.md");
        assert!(diff_row.1.contains("-Original alpha section."));
        assert!(diff_row.1.contains("+Updated alpha section."));

        let new_sec_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sections_fts WHERE sections_fts MATCH 'tokennewsection'",
                [],
                |r| r.get(0),
            )
            .expect("query new section in FTS");
        assert_eq!(new_sec_hits, 1);

        let old_sec_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sections_fts WHERE sections_fts MATCH 'Original'",
                [],
                |r| r.get(0),
            )
            .expect("query old section in FTS");
        assert_eq!(old_sec_hits, 0);
    }

    #[tokio::test]
    async fn marks_deleted_file_stale_and_prunes_only_with_flag() {
        let vault_dir = tempdir().expect("tempdir should succeed");
        let docs_dir = tempdir().expect("docs tempdir should succeed");
        let root = docs_dir.path();

        std::fs::write(root.join("keep.md"), "# Keep\n\nStays around.\n").expect("write keep");
        std::fs::write(root.join("drop.md"), "# Drop\n\nWill be deleted.\n").expect("write drop");

        let vault = LocalVault::new(vault_dir.path(), root, "docs").await;
        let (conn, source_id) = (&vault.conn, vault.source_id);
        vault.sync(false).await;

        std::fs::remove_file(root.join("drop.md")).expect("remove drop.md");

        let default_resync = vault.sync(false).await;
        assert_eq!(default_resync.pages_unchanged, 1);
        assert_eq!(default_resync.pages_stale, 1);
        assert_eq!(default_resync.pruned, 0);

        let drop_status: String = conn
            .query_row(
                "SELECT status FROM pages WHERE source_id = ?1 AND uri = 'drop.md'",
                [source_id],
                |r| r.get(0),
            )
            .expect("query drop.md status");
        assert_eq!(drop_status, "stale");

        let prune_resync = vault.sync(true).await;
        assert_eq!(prune_resync.pages_unchanged, 1);
        assert_eq!(prune_resync.pages_stale, 0);
        assert_eq!(prune_resync.pruned, 1);

        let drop_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE source_id = ?1 AND uri = 'drop.md'",
                [source_id],
                |r| r.get(0),
            )
            .expect("count drop.md after prune");
        assert_eq!(drop_count, 0);
    }

    #[test]
    fn canonicalizes_path_and_rejects_non_directory() {
        let dir = tempdir().expect("tempdir should succeed");
        let missing = dir.path().join("does-not-exist");
        let file_path = dir.path().join("single-file.md");
        std::fs::write(&file_path, "# Just A File\n").expect("write single file");

        let missing_err = canonicalize_local_dir(&missing).unwrap_err();
        assert!(matches!(missing_err, VaultError::Validation(_)));

        let file_err = canonicalize_local_dir(&file_path).unwrap_err();
        assert!(matches!(file_err, VaultError::Validation(_)));

        let nested = dir.path().join("a").join("..").join("real-docs");
        std::fs::create_dir_all(&nested).expect("create real-docs");
        let canon = canonicalize_local_dir(&nested).expect("canonicalize nested");
        let expected = std::fs::canonicalize(dir.path().join("real-docs")).expect("canonicalize");
        assert_eq!(canon, expected);

        let walk_err = collect("bad", &file_path, &[], &[], false)
            .expect_err("the folder walk must reject a non-directory");
        assert!(matches!(walk_err, VaultError::Validation(_)));
    }

    #[tokio::test]
    async fn filters_files_with_include_and_exclude_patterns() {
        let vault_dir = tempdir().expect("tempdir should succeed");
        let docs_dir = tempdir().expect("docs tempdir should succeed");
        let root = docs_dir.path();

        std::fs::create_dir_all(root.join("docs/drafts")).expect("create dirs");
        std::fs::create_dir_all(root.join("blog")).expect("create blog");
        std::fs::write(root.join("docs/guide.md"), "# Guide\n").expect("write guide");
        std::fs::write(root.join("docs/drafts/wip.md"), "# WIP\n").expect("write wip");
        std::fs::write(root.join("blog/post.md"), "# Post\n").expect("write post");

        let vault = LocalVault::with_patterns(
            vault_dir.path(),
            root,
            "filtered",
            vec!["docs/*".to_string()],
            vec!["docs/drafts".to_string()],
        )
        .await;
        let (conn, source_id) = (&vault.conn, vault.source_id);
        let summary = vault.sync(false).await;

        assert_eq!(summary.pages_added, 1);
        let uri: String = conn
            .query_row(
                "SELECT uri FROM pages WHERE source_id = ?1",
                [source_id],
                |r| r.get(0),
            )
            .expect("select uri");
        assert_eq!(uri, "docs/guide.md");
    }
}
