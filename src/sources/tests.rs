use std::net::SocketAddr;
use std::sync::atomic::Ordering;

use axum::response::Html;
use axum::routing::get;
use axum::Router;
use tempfile::tempdir;
use tokio::net::TcpListener;

use super::*;
use crate::config::VaultConfig;
use crate::error::VaultError;
use crate::ingest::{SourceKind, SyncSummary};

fn synced(outcome: SyncOutcome) -> SyncSummary {
    match outcome {
        SyncOutcome::Synced(summary) => summary,
        SyncOutcome::AlreadySyncing => panic!("expected Synced, got AlreadySyncing"),
    }
}

/// A one-page site whose page takes `delay` to respond, so a Sync holds its claim that long.
async fn spawn_slow_site(delay: std::time::Duration) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/docs/",
        get(move || async move {
            tokio::time::sleep(delay).await;
            Html("<html><body><main><h1>Slow</h1><p>Eventually.</p></main></body></html>")
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, handle)
}

async fn spawn_two_page_site() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route(
            "/docs/",
            get(|| async {
                Html(
                    r#"<html><body><nav><a href="/docs/next">Next</a></nav><main><h1>Docs</h1><p>Root page.</p></main></body></html>"#,
                )
            }),
        )
        .route(
            "/docs/next",
            get(|| async {
                Html("<html><body><main><h1>Next</h1><p>Second page.</p></main></body></html>")
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, handle)
}

#[tokio::test]
async fn add_local_then_sync_reports_summary() {
    let vault_dir = tempdir().expect("vault tempdir");
    let docs_dir = tempdir().expect("docs tempdir");
    std::fs::write(docs_dir.path().join("intro.md"), "# Intro\n\nHello.\n").expect("write");
    std::fs::write(docs_dir.path().join("guide.md"), "# Guide\n\nSteps.\n").expect("write");

    let lifecycle = SourceLifecycle::new(VaultConfig::new(vault_dir.path()));
    let added = lifecycle
        .add(
            AddSourceParams {
                origin: docs_dir.path().to_string_lossy().into_owned(),
                name: Some("notes".into()),
                ..Default::default()
            },
            true,
        )
        .await
        .expect("add local source");
    assert_eq!(added.source.kind, SourceKind::Local);
    assert_eq!(added.source.status, "ok");
    assert_eq!(added.summary.expect("summary").pages_added, 2);

    std::fs::write(docs_dir.path().join("extra.md"), "# Extra\n\nMore.\n").expect("write");
    let summary = synced(lifecycle.sync("notes", false).await.expect("sync"));
    assert_eq!(summary.pages_added, 1);
    assert_eq!(summary.pages_unchanged, 2);

    let all = lifecycle.sync_all(false).await.expect("sync_all");
    assert_eq!(all.len(), 1);
    let only = all.into_iter().next().expect("one result");
    assert_eq!(only.name, "notes");
    let summary = synced(only.result.expect("synced"));
    assert_eq!(summary.pages_unchanged, 3);
}

#[tokio::test]
async fn add_web_then_sync_via_loopback_fixture() {
    let (addr, server) = spawn_two_page_site().await;
    let vault_dir = tempdir().expect("vault tempdir");
    let lifecycle =
        SourceLifecycle::new(VaultConfig::new(vault_dir.path()).allowing_private_origins());

    let added = lifecycle
        .add(
            AddSourceParams {
                origin: format!("http://{addr}/docs/"),
                name: Some("fixture-docs".into()),
                ..Default::default()
            },
            true,
        )
        .await
        .expect("add web source");
    assert_eq!(added.source.kind, SourceKind::Web);
    assert_eq!(added.summary.expect("summary").pages_added, 2);

    let summary = synced(lifecycle.sync("fixture-docs", false).await.expect("resync"));
    server.abort();
    assert_eq!(summary.pages_unchanged, 2);
}

#[tokio::test]
async fn update_and_remove_roundtrip() {
    let vault_dir = tempdir().expect("vault tempdir");
    let lifecycle = SourceLifecycle::new(VaultConfig::new(vault_dir.path()));

    let added = lifecycle
        .add(
            AddSourceParams {
                origin: "https://example.com/docs".into(),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("register without sync");
    assert_eq!(added.source.name, "example-com-docs");
    assert!(added.summary.is_none());

    let updated = lifecycle
        .update(
            "example-com-docs",
            UpdateSourceParams {
                tags: Some(vec!["rust".into(), "rust".into(), " docs ".into()]),
                refresh_interval_secs: Some(0),
                ..Default::default()
            },
        )
        .expect("update");
    assert_eq!(updated.tags, vec!["rust", "docs"]);
    assert_eq!(updated.refresh_interval_secs, 0);

    let Removal::Removed(removed) = lifecycle.remove("example-com-docs").await.expect("remove")
    else {
        panic!("expected Removed");
    };
    assert_eq!(removed.name, "example-com-docs");
    assert_eq!(removed.pages_deleted, 0);

    let missing = lifecycle.remove("example-com-docs").await.unwrap_err();
    assert!(matches!(missing, VaultError::NotFound(_)));
}

#[tokio::test]
async fn operations_on_missing_vault_do_not_create_it() {
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path().join("absent"));
    let lifecycle = SourceLifecycle::new(config.clone());

    assert!(matches!(
        lifecycle.sync("nope", false).await.unwrap_err(),
        VaultError::NotFound(_)
    ));
    assert!(matches!(
        lifecycle.remove("nope").await.unwrap_err(),
        VaultError::NotFound(_)
    ));
    assert!(lifecycle.sync_all(false).await.expect("empty").is_empty());
    assert!(!config.db_path().exists());
}

#[tokio::test]
async fn add_rejects_private_origin_by_default() {
    let vault_dir = tempdir().expect("vault tempdir");
    let lifecycle = SourceLifecycle::new(VaultConfig::new(vault_dir.path()));
    for origin in [
        "http://127.0.0.1/docs",
        "http://10.0.0.1/",
        "http://docs.localhost/",
    ] {
        let err = lifecycle
            .add(
                AddSourceParams {
                    origin: origin.into(),
                    ..Default::default()
                },
                false,
            )
            .await
            .expect_err(origin);
        assert!(
            matches!(err, VaultError::Validation(_)),
            "{origin}: {err:?}"
        );
    }
}

#[tokio::test]
async fn add_allows_private_origin_when_enabled() {
    let vault_dir = tempdir().expect("vault tempdir");
    let lifecycle =
        SourceLifecycle::new(VaultConfig::new(vault_dir.path()).allowing_private_origins());
    let added = lifecycle
        .add(
            AddSourceParams {
                origin: "http://127.0.0.1:9/docs".into(),
                name: Some("intranet".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("private origin allowed");
    assert_eq!(added.source.kind, SourceKind::Web);
}

#[tokio::test]
async fn sync_rejects_stored_private_origin_when_disabled() {
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path());
    let conn = crate::db::open_rw_db(&config.db_path()).expect("open db");
    conn.execute(
        "INSERT INTO sources (name, kind, origin) VALUES ('legacy', 'web', 'http://192.168.1.10/docs')",
        [],
    )
    .expect("insert legacy private source");
    drop(conn);

    let err = SourceLifecycle::new(config)
        .sync("legacy", false)
        .await
        .expect_err("stored private origin must be refused");
    assert!(matches!(err, VaultError::Validation(_)), "{err:?}");
}

#[tokio::test]
async fn concurrent_sync_returns_already_syncing() {
    let (addr, server) = spawn_slow_site(std::time::Duration::from_millis(400)).await;
    let vault_dir = tempdir().expect("vault tempdir");
    let lifecycle =
        SourceLifecycle::new(VaultConfig::new(vault_dir.path()).allowing_private_origins());
    lifecycle
        .add(
            AddSourceParams {
                origin: format!("http://{addr}/docs/"),
                name: Some("slow".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("register");

    let (a, b) = tokio::join!(lifecycle.sync("slow", false), lifecycle.sync("slow", false));
    let outcomes = [a.expect("first"), b.expect("second")];
    let synced_count = outcomes
        .iter()
        .filter(|o| matches!(o, SyncOutcome::Synced(_)))
        .count();
    let refused = outcomes
        .iter()
        .filter(|o| matches!(o, SyncOutcome::AlreadySyncing))
        .count();
    assert_eq!((synced_count, refused), (1, 1), "{outcomes:?}");

    // The claim is released once the Sync finishes.
    assert!(matches!(
        lifecycle.sync("slow", false).await.expect("third"),
        SyncOutcome::Synced(_)
    ));
    server.abort();
}

#[tokio::test]
async fn remove_while_syncing_returns_already_syncing() {
    let vault_dir = tempdir().expect("vault tempdir");
    let docs_dir = tempdir().expect("docs tempdir");
    std::fs::write(docs_dir.path().join("a.md"), "# A\n\nAlpha.\n").expect("write");
    let lifecycle = SourceLifecycle::new(VaultConfig::new(vault_dir.path()));
    lifecycle
        .add(
            AddSourceParams {
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
    assert!(matches!(
        lifecycle.remove("busy").await.expect("remove"),
        Removal::AlreadySyncing
    ));
    assert!(matches!(
        lifecycle.sync("busy", false).await.expect("sync"),
        SyncOutcome::AlreadySyncing
    ));

    drop(held);
    let mut outcome = SyncOutcome::AlreadySyncing;
    for _ in 0..100 {
        outcome = lifecycle.sync("busy", false).await.expect("sync");
        if outcome != SyncOutcome::AlreadySyncing {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        synced(outcome).pages_added,
        1,
        "the dropped guard released the claim"
    );
    assert!(matches!(
        lifecycle.remove("busy").await.expect("remove"),
        Removal::Removed(_)
    ));
}

/// The Source shows the failure: status 'error', `last_error`, and an error Sync run on top.
fn assert_failure_recorded(config: &VaultConfig, name: &str) {
    let detail = crate::ingest::show_source(config, name).expect("show_source");
    assert_eq!(detail.source.status, "error", "{name}: {:?}", detail.source);
    assert!(detail.source.last_error.is_some(), "{name}: no last_error");
    let claims: i64 = crate::db::open_rw_db(&config.db_path())
        .expect("open db")
        .query_row(
            "SELECT COUNT(*) FROM sync_claims WHERE source_id = ?1",
            [detail.source.id],
            |r| r.get(0),
        )
        .expect("count claims");
    assert_eq!(claims, 0, "{name}: claim released after the failed Sync");
    let latest = detail.sync_runs.first().expect("a sync run was recorded");
    assert_eq!(latest.status, "error", "{name}: {latest:?}");
    assert!(latest.error_message.is_some(), "{name}: {latest:?}");
    assert!(latest.finished_at.is_some(), "{name}: {latest:?}");
    assert_eq!(
        detail
            .sync_runs
            .iter()
            .filter(|r| r.status == "running")
            .count(),
        0,
        "{name}: no run left running"
    );
}

async fn add_local(lifecycle: &SourceLifecycle, dir: &std::path::Path, name: &str) {
    lifecycle
        .add(
            AddSourceParams {
                origin: dir.to_string_lossy().into_owned(),
                name: Some(name.into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("register local source");
}

#[tokio::test]
async fn missing_local_folder_records_error_run() {
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path());
    let lifecycle = SourceLifecycle::new(config.clone());

    for name in ["via-sync", "via-claim"] {
        let docs_dir = tempdir().expect("docs tempdir");
        add_local(&lifecycle, docs_dir.path(), name).await;
        drop(docs_dir); // the folder disappears
    }

    lifecycle
        .sync("via-sync", false)
        .await
        .expect_err("missing folder must fail");
    assert_failure_recorded(&config, "via-sync");

    // The path the UI and scheduler take: wait for the claim, then the end.
    lifecycle
        .start_sync("via-claim", false)
        .claimed()
        .await
        .expect("claim")
        .expect("free")
        .finish()
        .await
        .expect_err("missing folder must fail");
    assert_failure_recorded(&config, "via-claim");
}

#[tokio::test]
async fn rejected_private_origin_records_error_run() {
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path());
    let conn = crate::db::open_rw_db(&config.db_path()).expect("open db");
    conn.execute(
        "INSERT INTO sources (name, kind, origin) VALUES ('legacy', 'web', 'http://10.1.2.3/docs')",
        [],
    )
    .expect("insert legacy private source");
    drop(conn);

    SourceLifecycle::new(config.clone())
        .sync("legacy", false)
        .await
        .expect_err("private origin refused");
    assert_failure_recorded(&config, "legacy");
}

#[tokio::test]
async fn add_with_sync_failure_records_error_run() {
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path()).allowing_private_origins();
    // Nothing listens on port 9 (discard), so the crawl fails.
    SourceLifecycle::new(config.clone())
        .add(
            AddSourceParams {
                origin: "http://127.0.0.1:9/docs/".into(),
                name: Some("unreachable".into()),
                ..Default::default()
            },
            true,
        )
        .await
        .expect_err("first sync fails");
    assert_failure_recorded(&config, "unreachable");
}

#[tokio::test]
async fn sync_all_continues_after_failure() {
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path());
    let lifecycle = SourceLifecycle::new(config.clone());

    let dirs: Vec<_> = (0..3).map(|_| tempdir().expect("docs tempdir")).collect();
    for (dir, name) in dirs.iter().zip(["a-first", "b-broken", "c-last"]) {
        std::fs::write(dir.path().join("p.md"), "# P\n\nPage.\n").expect("write");
        add_local(&lifecycle, dir.path(), name).await;
    }
    let mut dirs = dirs.into_iter();
    let (first, broken, last) = (dirs.next(), dirs.next(), dirs.next());
    drop(broken);

    let results = lifecycle.sync_all(false).await.expect("sync_all");
    let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["a-first", "b-broken", "c-last"]);
    assert!(matches!(results[0].result, Ok(SyncOutcome::Synced(_))));
    assert!(results[1].result.is_err());
    assert!(matches!(results[2].result, Ok(SyncOutcome::Synced(_))));
    assert_failure_recorded(&config, "b-broken");
    drop((first, last));
}

#[tokio::test]
async fn sync_all_skips_source_removed_mid_run() {
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path());
    let lifecycle = SourceLifecycle::new(config.clone());
    let docs = tempdir().expect("docs");
    std::fs::write(docs.path().join("p.md"), "# P\n\nPage.\n").expect("write");
    add_local(&lifecycle, docs.path(), "kept").await;

    // A claim attempt on a Source id that no longer exists is "not found", not a failure.
    let mut ghost = crate::ingest::load_source_by_name(
        &crate::db::open_rw_db(&config.db_path()).expect("open db"),
        "kept",
    )
    .expect("load");
    ghost.id += 1_000;
    let err = lifecycle
        .start_sync_record(ghost, false)
        .finish()
        .await
        .expect_err("ghost source");
    assert!(matches!(err, VaultError::NotFound(_)), "{err:?}");
}

#[tokio::test]
async fn dropped_sync_future_still_finishes_in_background() {
    let (addr, server) = spawn_slow_site(std::time::Duration::from_millis(300)).await;
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path()).allowing_private_origins();
    let lifecycle = SourceLifecycle::new(config.clone());
    lifecycle
        .add(
            AddSourceParams {
                origin: format!("http://{addr}/docs/"),
                name: Some("slow".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("register");

    // The MCP HTTP transport: the request future is dropped when the client disconnects.
    let request = crate::mcp::handle_jsonrpc_payload(
        &config,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "manage", "arguments": {"action": "sync", "name": "slow"}}
        }),
    );
    // Poll the request exactly once, then drop it: the client is gone before any reply.
    let _ = tokio::time::timeout(std::time::Duration::ZERO, request).await;

    let status = wait_for_status(&config, "slow", "ok").await;
    server.abort();
    assert_eq!(status, "ok", "sync must not be cancelled with its caller");
}

/// Pauses a Sync at one [`Stage`]: `reached` resolves once the Sync is there, and the Sync
/// stays paused until `open` is called (or the gate is dropped).
struct Gate {
    reached: tokio::sync::mpsc::UnboundedReceiver<()>,
    go: std::sync::mpsc::Sender<()>,
}

impl Gate {
    /// A lifecycle whose Syncs pause at `stage`, and the gate that holds them there.
    fn at(lifecycle: SourceLifecycle, stage: Stage) -> (SourceLifecycle, Gate) {
        let (reached_tx, reached) = tokio::sync::mpsc::unbounded_channel();
        let (go, go_rx) = std::sync::mpsc::channel::<()>();
        let go_rx = std::sync::Mutex::new(go_rx);
        let lifecycle = lifecycle.with_probe(move |at| {
            if at == stage {
                let _ = reached_tx.send(());
                let _ = go_rx.lock().expect("gate lock").recv();
            }
        });
        (lifecycle, Gate { reached, go })
    }

    async fn reached(&mut self) {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.reached.recv())
            .await
            .expect("the Sync reaches the gate")
            .expect("gate open");
    }

    fn open(&self) {
        let _ = self.go.send(());
    }
}

fn claim_rows(config: &VaultConfig) -> i64 {
    crate::db::open_rw_db(&config.db_path())
        .expect("open db")
        .query_row("SELECT COUNT(*) FROM sync_claims", [], |r| r.get(0))
        .expect("count claims")
}

/// Poll the Source's status until it is `want` or 5 s pass; returns the last status seen.
async fn wait_for_status(config: &VaultConfig, name: &str, want: &str) -> String {
    let mut status = String::new();
    for _ in 0..100 {
        status = crate::ingest::show_source(config, name)
            .expect("show")
            .source
            .status;
        if status == want {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    status
}

/// A local Source with one page, registered but not synced.
async fn local_source(lifecycle: &SourceLifecycle, name: &str) -> tempfile::TempDir {
    let docs = tempdir().expect("docs tempdir");
    std::fs::write(docs.path().join("p.md"), "# P\n\nPage.\n").expect("write");
    add_local(lifecycle, docs.path(), name).await;
    docs
}

/// Drop a `sync` caller while the Sync is paused at `stage`, then let it go on. Returns the
/// vault and docs dirs so they live as long as the caller's assertions.
async fn drop_sync_caller_at(
    stage: Stage,
) -> (VaultConfig, SourceLifecycle, [tempfile::TempDir; 2]) {
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path());
    let plain = SourceLifecycle::new(config.clone());
    let docs = local_source(&plain, "docs").await;

    let (gated, mut gate) = Gate::at(plain.clone(), stage);
    let caller = tokio::spawn(async move { gated.sync("docs", false).await });
    gate.reached().await;
    caller.abort();
    assert!(caller.await.expect_err("aborted").is_cancelled());
    gate.open();
    (config, plain, [vault_dir, docs])
}

#[tokio::test]
async fn sync_caller_dropped_after_claim_commits_neither_cancels_nor_leaks() {
    let (config, lifecycle, _dirs) = drop_sync_caller_at(Stage::Claimed).await;
    assert_eq!(
        wait_for_status(&config, "docs", "ok").await,
        "ok",
        "the Sync runs to the end without its caller"
    );
    let mut rows = claim_rows(&config);
    for _ in 0..100 {
        if rows == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        rows = claim_rows(&config);
    }
    assert_eq!(rows, 0, "the claim is released");
    assert!(
        matches!(
            lifecycle.sync("docs", false).await.expect("sync again"),
            SyncOutcome::Synced(_)
        ),
        "this process can Sync the Source again"
    );
}

#[tokio::test]
async fn sync_caller_dropped_while_loading_still_syncs() {
    let (config, _lifecycle, _dirs) = drop_sync_caller_at(Stage::Loaded).await;
    assert_eq!(
        wait_for_status(&config, "docs", "ok").await,
        "ok",
        "a Sync whose caller left before the claim still runs"
    );
}

#[tokio::test]
async fn claim_caller_dropped_after_claim_commits_does_not_leak() {
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path());
    let plain = SourceLifecycle::new(config.clone());
    let _docs = local_source(&plain, "docs").await;

    let (gated, mut gate) = Gate::at(plain.clone(), Stage::Claimed);
    let caller = tokio::spawn(async move { gated.claim("docs").await.map(|c| c.is_some()) });
    // `claim` takes the claim in one hop; the gate pauses it right after the row commits.
    gate.reached().await;
    caller.abort();
    let _ = caller.await;
    gate.open();

    let mut rows = 1;
    for _ in 0..100 {
        rows = claim_rows(&config);
        if rows == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(rows, 0, "the abandoned claim is released");
    assert!(plain.claim("docs").await.expect("claim").is_some());
}

/// Make every page write fail, as a full disk or a corrupt index would.
fn break_page_writes(config: &VaultConfig) {
    crate::db::open_rw_db(&config.db_path())
        .expect("open db")
        .execute_batch(
            "CREATE TRIGGER fail_page_writes BEFORE INSERT ON pages
             BEGIN SELECT RAISE(ABORT, 'disk is full'); END;",
        )
        .expect("install trigger");
}

/// The Sync runs of a Source, oldest first: (status, error_message, finished).
fn sync_runs(config: &VaultConfig, name: &str) -> Vec<(String, Option<String>, bool)> {
    let conn = crate::db::open_rw_db(&config.db_path()).expect("open db");
    let mut stmt = conn
        .prepare(
            "SELECT r.status, r.error_message, r.finished_at IS NOT NULL
             FROM sync_runs r JOIN sources s ON s.id = r.source_id
             WHERE s.name = ?1 ORDER BY r.id",
        )
        .expect("prepare");
    stmt.query_map([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<std::result::Result<_, _>>()
        .expect("rows")
}

#[tokio::test]
async fn failed_page_write_records_one_error_run_for_web_and_local() {
    let (addr, server) = spawn_two_page_site().await;
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path()).allowing_private_origins();
    let lifecycle = SourceLifecycle::new(config.clone());
    let _docs = local_source(&lifecycle, "local").await;
    lifecycle
        .add(
            AddSourceParams {
                origin: format!("http://{addr}/docs/"),
                name: Some("web".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("register web");
    break_page_writes(&config);

    for name in ["local", "web"] {
        let err = lifecycle.sync(name, false).await.expect_err(name);
        assert!(err.to_string().contains("disk is full"), "{name}: {err}");
        assert_failure_recorded(&config, name);
        let runs = sync_runs(&config, name);
        assert_eq!(runs.len(), 1, "{name}: exactly one Sync run: {runs:?}");
        assert!(
            runs[0]
                .1
                .as_deref()
                .is_some_and(|m| m.contains("disk is full")),
            "{name}: {runs:?}"
        );
    }
    server.abort();
}

/// A vault with one registered (unsynced) Source named `docs`, and a lifecycle on it.
async fn vault_with_docs() -> (tempfile::TempDir, VaultConfig, SourceLifecycle) {
    let vault_dir = tempdir().expect("vault tempdir");
    let config = VaultConfig::new(vault_dir.path());
    let lifecycle = SourceLifecycle::new(config.clone());
    lifecycle
        .add(
            AddSourceParams {
                origin: vault_dir.path().to_string_lossy().into_owned(),
                name: Some("docs".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("register");
    (vault_dir, config, lifecycle)
}

/// Wait until no Sync claim is left (a dropped guard releases on the blocking pool).
async fn wait_for_no_claims(config: &VaultConfig) {
    for _ in 0..100 {
        if claim_rows(config) == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(claim_rows(config), 0, "the claim is released");
}

fn status_of(config: &VaultConfig, name: &str) -> String {
    crate::ingest::show_source(config, name)
        .expect("show")
        .source
        .status
}

#[tokio::test]
async fn successful_sync_opens_and_closes_exactly_one_run() {
    let (_vault, config, lifecycle) = vault_with_docs().await;
    let fake = FakeDriver::one_page();
    let summary = synced(
        lifecycle
            .clone()
            .with_driver(fake.clone())
            .sync("docs", false)
            .await
            .expect("sync"),
    );
    assert_eq!(summary.pages_added, 1);
    assert_eq!(sync_runs(&config, "docs"), [("ok".to_string(), None, true)]);
    assert_eq!(status_of(&config, "docs"), "ok");
    assert_eq!(claim_rows(&config), 0);
    assert_eq!(
        fake.written(),
        [true],
        "the driver hears the write succeeded"
    );
}

#[tokio::test]
async fn driver_failure_records_one_error_run() {
    let (_vault, config, lifecycle) = vault_with_docs().await;
    let err = lifecycle
        .with_driver(FakeDriver::failing("crawl broke"))
        .sync("docs", false)
        .await
        .expect_err("driver fails");
    assert!(err.to_string().contains("crawl broke"), "{err}");
    assert_failure_recorded(&config, "docs");
    assert_eq!(
        sync_runs(&config, "docs"),
        [("error".to_string(), Some("crawl broke".to_string()), true)]
    );
}

#[tokio::test]
async fn page_write_failure_records_one_error_run_and_tells_the_driver() {
    let (_vault, config, lifecycle) = vault_with_docs().await;
    break_page_writes(&config);
    let fake = FakeDriver::one_page();
    lifecycle
        .with_driver(fake.clone())
        .sync("docs", false)
        .await
        .expect_err("write fails");
    assert_failure_recorded(&config, "docs");
    assert_eq!(sync_runs(&config, "docs").len(), 1);
    assert_eq!(fake.written(), [false], "the driver hears the write failed");
}

#[tokio::test]
async fn driver_panic_releases_claim_and_closes_run() {
    let (_vault, config, lifecycle) = vault_with_docs().await;
    let err = lifecycle
        .clone()
        .with_driver(FakeDriver::panicking())
        .sync("docs", false)
        .await
        .expect_err("driver panics");
    assert!(err.to_string().contains("sync task failed"), "{err}");
    wait_for_no_claims(&config).await;
    assert_failure_recorded(&config, "docs");
    assert_eq!(sync_runs(&config, "docs").len(), 1);

    let next = lifecycle.with_driver(FakeDriver::one_page());
    assert!(matches!(
        next.sync("docs", false).await.expect("sync again"),
        SyncOutcome::Synced(_)
    ));
}

#[tokio::test]
async fn failure_to_open_the_run_releases_the_claim() {
    let (_vault, config, lifecycle) = vault_with_docs().await;
    let conn = crate::db::open_rw_db(&config.db_path()).expect("open db");
    conn.execute_batch(
        "CREATE TRIGGER no_runs BEFORE INSERT ON sync_runs
         BEGIN SELECT RAISE(ABORT, 'runs table is locked'); END;",
    )
    .expect("install trigger");
    let lifecycle = lifecycle.with_driver(FakeDriver::one_page());

    let (lifecycle, late) = counting_late_releases(lifecycle);

    let err = lifecycle.sync("docs", false).await.expect_err("open fails");
    assert!(err.to_string().contains("runs table is locked"), "{err}");
    assert_eq!(
        late.load(Ordering::SeqCst),
        0,
        "released before the error returned"
    );
    assert_eq!(claim_rows(&config), 0, "an immediate retry is not refused");
    assert!(sync_runs(&config, "docs").is_empty());
    assert_eq!(status_of(&config, "docs"), "idle", "nothing was started");

    conn.execute_batch("DROP TRIGGER no_runs")
        .expect("drop trigger");
    assert!(matches!(
        lifecycle.sync("docs", false).await.expect("sync"),
        SyncOutcome::Synced(_)
    ));
}

#[tokio::test]
async fn missing_source_is_not_found_and_records_nothing() {
    let (_vault, config, lifecycle) = vault_with_docs().await;
    let err = lifecycle
        .with_driver(FakeDriver::one_page())
        .sync("nope", false)
        .await
        .expect_err("no such Source");
    assert!(matches!(err, VaultError::NotFound(_)), "{err:?}");
    assert!(sync_runs(&config, "docs").is_empty());
    assert_eq!(claim_rows(&config), 0);
}

#[tokio::test]
async fn sync_of_a_loaded_source_after_the_vault_is_gone_does_not_create_it() {
    let (_vault, config, lifecycle) = vault_with_docs().await;
    let source = {
        let conn = open_rw_db(&config.db_path()).expect("open vault");
        load_source_by_name(&conn, "docs").expect("docs")
    };
    let db = config.db_path();
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
        if path.exists() {
            std::fs::remove_file(&path).expect("remove vault file");
        }
    }

    let err = lifecycle
        .with_driver(FakeDriver::one_page())
        .start_sync_record(source, false)
        .finish()
        .await
        .expect_err("the vault is gone");
    assert!(matches!(err, VaultError::NotFound(_)), "{err:?}");
    assert!(!db.exists(), "the Sync recreated an empty vault");
}

#[tokio::test]
async fn second_start_while_a_sync_runs_is_already_syncing() {
    let (_vault, config, lifecycle) = vault_with_docs().await;
    let (fake, mut gate) = FakeDriver::one_page().gated();
    let lifecycle = lifecycle.with_driver(fake);

    // The UI's order: wait for the claim, reply 202, let the Sync run on.
    let running = lifecycle
        .start_sync("docs", false)
        .claimed()
        .await
        .expect("claim")
        .expect("first Sync takes the claim");
    gate.reached().await;
    assert!(
        lifecycle
            .start_sync("docs", false)
            .claimed()
            .await
            .expect("claim")
            .is_none(),
        "a second request while the first runs is refused (UI: 409)"
    );
    gate.open();
    assert_eq!(running.finish().await.expect("finish").pages_added, 1);
    assert_eq!(sync_runs(&config, "docs").len(), 1);
}

/// Drop the caller while the Sync is inside the driver or writing, then let it go on: the
/// Sync still finishes with one 'ok' run, and the claim is released.
#[tokio::test]
async fn sync_caller_dropped_during_driver_or_write_still_finishes() {
    for at_write in [false, true] {
        let (_vault, config, lifecycle) = vault_with_docs().await;
        let (fake, mut driver_gate) = FakeDriver::one_page().gated();
        let (lifecycle, mut write_gate) = Gate::at(lifecycle.with_driver(fake), Stage::Writing);

        let gated = lifecycle.clone();
        let caller = tokio::spawn(async move { gated.sync("docs", false).await });
        driver_gate.reached().await;
        if at_write {
            driver_gate.open();
            write_gate.reached().await;
        }
        caller.abort();
        assert!(caller.await.expect_err("aborted").is_cancelled());
        driver_gate.open();
        write_gate.open();

        assert_eq!(
            wait_for_status(&config, "docs", "ok").await,
            "ok",
            "at_write={at_write}"
        );
        wait_for_no_claims(&config).await;
        assert_eq!(
            sync_runs(&config, "docs"),
            [("ok".to_string(), None, true)],
            "at_write={at_write}"
        );
    }
}

/// The lifecycle, and a count of claims its dropped guards had to release in the
/// background, after the call that ended the claim had already returned.
fn counting_late_releases(
    mut lifecycle: SourceLifecycle,
) -> (
    SourceLifecycle,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let late = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&late);
    lifecycle.claims.on_late_release = std::sync::Arc::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    (lifecycle, late)
}

#[tokio::test]
async fn failed_remove_releases_the_claim_before_returning() {
    let (_vault, config, lifecycle) = vault_with_docs().await;
    crate::db::open_rw_db(&config.db_path())
        .expect("open db")
        .execute_batch(
            "CREATE TRIGGER keep_sources BEFORE DELETE ON sources
             BEGIN SELECT RAISE(ABORT, 'sources are read-only'); END;",
        )
        .expect("install trigger");
    let (lifecycle, late) = counting_late_releases(lifecycle.with_driver(FakeDriver::one_page()));

    let err = lifecycle.remove("docs").await.expect_err("remove fails");
    assert!(err.to_string().contains("sources are read-only"), "{err}");
    assert_eq!(
        late.load(Ordering::SeqCst),
        0,
        "released before the error returned"
    );
    assert_eq!(claim_rows(&config), 0);
    assert!(
        matches!(
            lifecycle.sync("docs", false).await.expect("sync"),
            SyncOutcome::Synced(_)
        ),
        "an immediate retry is not refused (UI: 409)"
    );
}

/// Make the final hop's first database open fail (as a locked or exhausted database does),
/// and let the retry that records the failure succeed.
fn failing_first_finish_open(lifecycle: SourceLifecycle, config: &VaultConfig) -> SourceLifecycle {
    use std::os::unix::fs::PermissionsExt;
    let db = config.db_path();
    lifecycle.with_probe(move |stage| {
        let mode = match stage {
            Stage::Finishing => 0o000,
            Stage::Recording => 0o644,
            _ => return,
        };
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(mode))
            .expect("chmod vault.db");
    })
}

fn restore_db_permissions(config: &VaultConfig) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(config.db_path(), std::fs::Permissions::from_mode(0o644))
        .expect("restore vault.db");
}

#[tokio::test]
async fn database_open_failure_after_the_driver_keeps_its_error_and_closes_the_run() {
    let (_vault, config, lifecycle) = vault_with_docs().await;
    let (lifecycle, late) = counting_late_releases(lifecycle);
    let lifecycle = failing_first_finish_open(
        lifecycle.with_driver(FakeDriver::failing("crawl broke")),
        &config,
    );

    let err = lifecycle.sync("docs", false).await.expect_err("sync fails");
    restore_db_permissions(&config);
    assert!(
        err.to_string().contains("crawl broke"),
        "the driver's error wins: {err}"
    );
    assert_eq!(
        late.load(Ordering::SeqCst),
        0,
        "released before the error returned"
    );
    assert_failure_recorded(&config, "docs");
    assert_eq!(
        sync_runs(&config, "docs"),
        [("error".to_string(), Some("crawl broke".to_string()), true)]
    );
}

#[tokio::test]
async fn database_open_failure_before_the_write_reports_it_and_closes_the_run() {
    let (_vault, config, lifecycle) = vault_with_docs().await;
    let fake = FakeDriver::one_page();
    let (lifecycle, late) = counting_late_releases(lifecycle);
    let lifecycle = failing_first_finish_open(lifecycle.with_driver(fake.clone()), &config);

    let err = lifecycle.sync("docs", false).await.expect_err("sync fails");
    restore_db_permissions(&config);
    assert!(err.to_string().contains("unable to open"), "{err}");
    assert_eq!(fake.written(), [false], "the driver hears the write failed");
    assert_eq!(
        late.load(Ordering::SeqCst),
        0,
        "released before the error returned"
    );
    assert_failure_recorded(&config, "docs");
    assert_eq!(sync_runs(&config, "docs").len(), 1);
}

#[tokio::test]
async fn sync_all_reports_a_drivers_not_found_instead_of_skipping_the_source() {
    let (_vault, _config, lifecycle) = vault_with_docs().await;
    let results = lifecycle
        .with_driver(FakeDriver::not_found("remote tree is gone"))
        .sync_all(false)
        .await
        .expect("sync_all");
    assert_eq!(
        results.len(),
        1,
        "only a Source removed before its claim is skipped"
    );
    assert!(
        matches!(&results[0].result, Err(VaultError::NotFound(_))),
        "{:?}",
        results[0].result
    );
}
