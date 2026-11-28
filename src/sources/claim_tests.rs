use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use rusqlite::Connection;
use tempfile::tempdir;

use super::*;
use crate::db::open_rw_db;

const T0: i64 = 1_800_000_000;

/// A fake process: fixed pid and token, a settable clock, and a pid-liveness answer.
fn env(pid: i64, token: &str, clock: &Arc<AtomicI64>, alive: bool) -> ClaimEnv {
    let clock = Arc::clone(clock);
    ClaimEnv {
        pid,
        token: token.to_string(),
        now: Arc::new(move || clock.load(Ordering::SeqCst)),
        pid_alive: Arc::new(move |_| alive),
        on_late_release: Arc::new(|_| {}),
        live: LiveGuards::default(),
    }
}

fn vault_with_source() -> (tempfile::TempDir, Connection, i64) {
    let dir = tempdir().expect("tempdir");
    let conn = open_rw_db(&dir.path().join("vault.db")).expect("open db");
    conn.execute(
        "INSERT INTO sources (name, kind, origin) VALUES ('docs', 'local', '/tmp/docs')",
        [],
    )
    .expect("insert source");
    let id = conn.last_insert_rowid();
    (dir, conn, id)
}

fn status(conn: &Connection, id: i64) -> String {
    conn.query_row("SELECT status FROM sources WHERE id = ?1", [id], |r| {
        r.get(0)
    })
    .expect("status")
}

#[test]
fn second_claim_is_refused() {
    let (_dir, conn, id) = vault_with_source();
    let clock = Arc::new(AtomicI64::new(T0));
    let first = env(100, "a", &clock, true);
    let second = env(200, "b", &clock, true);

    assert!(try_acquire(&conn, id, &first).expect("first claim"));
    assert!(!try_acquire(&conn, id, &second).expect("second claim"));
    // The holder itself cannot claim twice either.
    assert!(!try_acquire(&conn, id, &first).expect("re-claim"));

    release(&conn, id, &first).expect("release");
    assert!(try_acquire(&conn, id, &second).expect("claim after release"));
}

#[test]
fn dead_holder_claim_is_taken_over() {
    let (_dir, conn, id) = vault_with_source();
    let clock = Arc::new(AtomicI64::new(T0));
    let crashed = env(100, "a", &clock, true);
    assert!(try_acquire(&conn, id, &crashed).expect("claim"));
    conn.execute("UPDATE sources SET status = 'syncing' WHERE id = ?1", [id])
        .expect("simulate stuck status");

    let sees_dead_holder = env(200, "b", &clock, false);
    assert!(try_acquire(&conn, id, &sees_dead_holder).expect("takeover"));
    assert_eq!(status(&conn, id), "error", "stuck 'syncing' is reset");

    // The crashed holder's late release must not drop the new holder's claim.
    release(&conn, id, &crashed).expect("stale release");
    let third = env(300, "c", &clock, true);
    assert!(!try_acquire(&conn, id, &third).expect("still held"));
}

#[test]
fn expired_claim_is_taken_over() {
    let (_dir, conn, id) = vault_with_source();
    let clock = Arc::new(AtomicI64::new(T0));
    let holder = env(100, "a", &clock, true);
    let other = env(200, "b", &clock, true);
    assert!(try_acquire(&conn, id, &holder).expect("claim"));

    clock.store(T0 + MAX_CLAIM_AGE_SECS - 1, Ordering::SeqCst);
    assert!(!try_acquire(&conn, id, &other).expect("not yet expired"));

    clock.store(T0 + MAX_CLAIM_AGE_SECS, Ordering::SeqCst);
    assert!(try_acquire(&conn, id, &other).expect("expired takeover"));
}

#[test]
fn same_pid_other_token_is_taken_over() {
    let (_dir, conn, id) = vault_with_source();
    let clock = Arc::new(AtomicI64::new(T0));
    // pid 1 in the container: the earlier run and this run share a pid, and it is alive.
    let earlier_run = env(1, "earlier", &clock, true);
    let this_run = env(1, "current", &clock, true);
    assert!(try_acquire(&conn, id, &earlier_run).expect("claim"));

    assert!(try_acquire(&conn, id, &this_run).expect("takeover from earlier run"));
    assert!(!try_acquire(&conn, id, &this_run).expect("now held by this run"));
}

#[test]
fn real_liveness_check_sees_dead_and_live_pids() {
    let env = ClaimEnv::current();
    assert!((env.pid_alive)(env.pid), "this process is running");
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn true");
    let pid = i64::from(child.id());
    child.wait().expect("wait true");
    assert!(!(env.pid_alive)(pid), "exited child is not running");
}

#[test]
fn own_claim_never_expires_for_its_holder() {
    let (_dir, conn, id) = vault_with_source();
    let clock = Arc::new(AtomicI64::new(T0));
    let holder = env(100, "a", &clock, true);
    assert!(try_acquire(&conn, id, &holder).expect("claim"));

    // Six hours later a second task in the same process must not take over its own claim.
    clock.store(T0 + MAX_CLAIM_AGE_SECS + 1, Ordering::SeqCst);
    assert!(!try_acquire(&conn, id, &holder).expect("own expired claim stays held"));
}

fn claim_count(conn: &Connection, id: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM sync_claims WHERE source_id = ?1",
        [id],
        |r| r.get(0),
    )
    .expect("count claims")
}

/// A connection that cannot write: releasing a claim on it fails.
fn read_only(dir: &tempfile::TempDir) -> Connection {
    Connection::open_with_flags(
        dir.path().join("vault.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open read-only")
}

#[test]
fn failed_release_leaves_an_orphan_its_own_process_takes_over() {
    let (dir, conn, id) = vault_with_source();
    let clock = Arc::new(AtomicI64::new(T0));
    let this_process = env(100, "a", &clock, true);
    let guard = ClaimGuard::acquire(&conn, dir.path().join("vault.db"), id, &this_process)
        .expect("acquire")
        .expect("free");

    guard
        .release(&read_only(&dir))
        .expect_err("release on a read-only connection fails");
    assert_eq!(claim_count(&conn, id), 1, "the failed release left the row");

    assert!(
        try_acquire(&conn, id, &this_process).expect("claim again"),
        "no live guard holds the row: this process takes its orphan over"
    );
    // Another live process still sees the claim as held.
    let other = env(200, "b", &clock, true);
    assert!(!try_acquire(&conn, id, &other).expect("other process"));
}

#[test]
fn claim_with_a_live_guard_is_never_taken_over_by_its_own_process() {
    let (dir, conn, id) = vault_with_source();
    let clock = Arc::new(AtomicI64::new(T0));
    let this_process = env(100, "a", &clock, true);
    let guard = ClaimGuard::acquire(&conn, dir.path().join("vault.db"), id, &this_process)
        .expect("acquire")
        .expect("free");

    assert!(!try_acquire(&conn, id, &this_process).expect("second claim"));
    clock.store(T0 + MAX_CLAIM_AGE_SECS + 1, Ordering::SeqCst);
    assert!(
        !try_acquire(&conn, id, &this_process).expect("after the age limit"),
        "a live guard's claim never expires for its own process"
    );

    guard.release(&conn).expect("release");
    assert!(try_acquire(&conn, id, &this_process).expect("claim after release"));
}

#[test]
fn only_one_task_takes_over_an_orphan() {
    let (dir, conn, id) = vault_with_source();
    let clock = Arc::new(AtomicI64::new(T0));
    let this_process = env(100, "a", &clock, true);
    // An orphan: our pid and token, but no live guard (its release failed).
    conn.execute(
        "INSERT INTO sync_claims (source_id, pid, instance_token, claimed_at)
         VALUES (?1, 100, 'a', '2027-01-15T08:00:00Z')",
        [id],
    )
    .expect("insert orphan");

    let path = dir.path().join("vault.db");
    let takers: Vec<_> = (0..8)
        .map(|_| {
            let (path, env) = (path.clone(), this_process.clone());
            std::thread::spawn(move || {
                let conn = open_rw_db(&path).expect("open");
                try_acquire(&conn, id, &env).expect("try_acquire")
            })
        })
        .collect();
    let won = takers
        .into_iter()
        .map(|t| t.join().expect("taker thread"))
        .filter(|won| *won)
        .count();
    assert_eq!(won, 1, "exactly one task takes the orphan over");
}
