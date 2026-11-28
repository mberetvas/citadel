# fix-sync-lifetime: the Source lifecycle owns a Sync from claim to release

Origin: architecture review 2026-11-28, candidate 02 (rated Strong).

## Findings

### A. Claim-leak window: confirmed

- `SourceLifecycle::claim_record` (`src/sources/mod.rs`, before this task) commits the `sync_claims` row inside `blocking(..)` (`spawn_blocking(..).await`). It builds the `ClaimedSync` guard only after that await returns.
- A caller dropped during the await abandons the detached blocking task. That task still commits the row, but no guard ever exists to release it.
- `claim::try_acquire` never treats a row with this process's pid and token as abandoned (`held_by_us`, `src/sources/claim.rs`). The holder cannot claim again either: see `second_claim_is_refused` in `claim_tests.rs`. So this process can never Sync that Source until it restarts, and other processes wait out the 6 h age limit.
- Red evidence: a test-only probe pauses the claim hop right after the row commits, and the test aborts the caller there. On `main` @ 116f42c:
  - `claim_caller_dropped_after_claim_commits_does_not_leak` fails: `the abandoned claim is released`, left `1`, right `0`. The row stays.
  - `sync_caller_dropped_after_claim_commits_neither_cancels_nor_leaks` fails with status `idle`: no Sync ran, and the claim leaked.
- `ClaimedSync::run` spawned the Sync task only once polled. A caller dropped before that point got no Sync at all; that is B.

### B. Flaky `dropped_sync_future_still_finishes_in_background`: confirmed, the cause is the drop landing before the spawn

- The MCP `manage sync` path reaches `SourceLifecycle::sync` with no await before it. `sync` then made two blocking hops before it spawned the Sync task:
  1. hop 1 loaded the Source;
  2. hop 2 took the claim.
- Each hop re-runs `open_rw_db` and its schema batch. When the 50 ms timeout fired inside either hop, the Sync never started (status `idle`). Under CPU load the hops take longer, so the test failed more often.
- Deterministic evidence, on `main`:
  - `sync_caller_dropped_while_loading_still_syncs` pauses hop 1 and drops the caller: status `idle`, no claim row.
  - The hop-2 variant (finding A) gives `idle` and a leaked row.
  - Rewritten to poll the MCP request exactly once (`timeout(Duration::ZERO, ..)`), `dropped_sync_future_still_finishes_in_background` fails every run with `idle`.

### C. Sync-run state has five writers in three files: confirmed; the "web vs local diverge" part is changed

The five writers on `main`:

| Writer | Where | Writes |
|---|---|---|
| `start_sync_run` | `src/ingest/local.rs:157` | `sources.status='syncing'`, a new `'running'` run |
| 'ok' close in `persist_sync_outcomes` | `local.rs:~509-536` | run `'ok'` and its counts, `sources.status='ok'`, in the page transaction |
| `fail_sync_run` | `local.rs:171` (called at `local.rs:218` and `web.rs:417`) | run `'error'`, `sources.status='error'`; errors swallowed with `let _ =` |
| `record_sync_failure` | `src/sources/mod.rs:471` | closes every `'running'` run, or inserts an error run when none is newer than a `MAX(id)` read taken before the Sync; `sources.status='error'` |
| `reset_interrupted_sync` | `src/sources/claim.rs:170` | on release or takeover: `'running'` runs and `'syncing'` Sources become `'error'` ("interrupted") |

- The run id never reaches the lifecycle. `sync_record` guesses it from `SELECT COALESCE(MAX(id),0)`, read before the Sync. Confirmed.
- `web.rs` imports `start_sync_run`, `fail_sync_run` and `persist_sync_outcomes` from `ingest::local`. Confirmed.
- **Changed:** "a failed web persist skips `fail_sync_run`" is true of the code path but not of the recorded state.
  - Every failure is recorded twice: once by the driver (`fail_sync_run`) and again by the lifecycle (`record_sync_failure`).
  - A web persist failure, or a failed `open_rw_db` after the crawl, skips the first write, but `record_sync_failure` still closes the `'running'` run.
  - Evidence: `failed_page_write_records_one_error_run_for_web_and_local` installs a trigger that aborts every page insert. On `main` it passes for both kinds: one error run carrying the write error, `status='error'`, claim released.
  - So the defect is duplicated and guessed bookkeeping, not a wrong result.

### `add` returns its first Sync as a value: not fixed (follow-up)

- A failed first Sync turns `add` into `Err` after the Source row has committed, and a retried `add` then fails with "already registered".
- Returning the first Sync as a value would change the CLI exit status, MCP `isError`, the UI response and SPEC. That is outside this task.
- Follow-up: make `Added` carry the first Sync's result, and have the CLI, MCP and UI report a registered Source whose first Sync failed.

## Task

Make the Source lifecycle run each Sync as one task, from claim to release:

- A caller holds only a handle. Dropping the handle can neither cancel the Sync nor leak its claim.
- The lifecycle opens and closes the Sync run exactly once and holds its id.
- The folder walk and the web crawl become drivers that only return page outcomes.
- Writing pages moves into one module that both drivers' outcomes flow through.

## Acceptance criteria

1. The claim guard is built in the same synchronous code that commits the `sync_claims` row. A caller dropped at any await point never leaves a row behind: `claim_caller_dropped_after_claim_commits_does_not_leak`.
2. A dropped `sync` caller neither cancels nor prevents the Sync, at every pause point:
   - while loading: `sync_caller_dropped_while_loading_still_syncs`;
   - after the claim: `sync_caller_dropped_after_claim_commits_neither_cancels_nor_leaks`;
   - during the walk or crawl, and while writing pages: fake-driver tests.
   - `dropped_sync_future_still_finishes_in_background` polls the MCP request exactly once and passes every run.
3. The UI keeps 202/409: the claim is known before the reply, and a second request gets 409 while a Sync runs.
4. One Sync opens exactly one Sync run and closes it exactly once, by id. No `MAX(id)` guess, no `record_sync_failure`.
5. The 'ok' close and `sources.status='ok'` commit in the same transaction as the page writes.
6. Every failure stage records one error run, sets `status='error'` and `last_error`, and releases the claim. The stages are:
   - Source missing (nothing recorded);
   - opening the run;
   - the driver fails;
   - the driver panics;
   - writing pages fails.
   - Both production drivers (local, web) are covered for the page-write failure.
7. `src/ingest/local.rs` no longer writes `sync_runs`. `web.rs` no longer imports Sync-run or persist code from `local.rs`.
8. The `web_sync_summary` line still carries `ok` and `write_ms` for the page write (SPEC §4.1 item 4).
9. The checks gate passes, including the lite clippy.

## Design decisions

### Interface

```rust
impl SourceLifecycle {
    pub fn start_sync(&self, name: &str, prune: bool) -> SyncHandle; // spawns now
    pub(crate) fn start_sync_record(&self, source: SourceRecord, prune: bool) -> SyncHandle;
    pub async fn sync(&self, name: &str, prune: bool) -> Result<SyncOutcome>; // start + finish
    pub(crate) async fn claim(&self, name: &str) -> Result<Option<ClaimGuard>>; // remove(), tests
}
impl SyncHandle {
    pub async fn claimed(self) -> Result<Option<RunningSync>>; // None = AlreadySyncing
    pub async fn finish(self) -> Result<SyncOutcome>;
}
impl RunningSync { pub async fn finish(self) -> Result<SyncSummary>; }
```

- `start_sync` is a plain `fn` that spawns the task at once, so nothing is lost between the call and the first await. `sync` is `start_sync(..).finish()`, so the MCP path spawns on its first poll.
- The task runs every step itself and reports through a oneshot whether it took the claim; it never waits on its caller:
  1. one blocking hop that loads the Source, takes the claim (building the guard) and opens the Sync run;
  2. the driver;
  3. one blocking hop that writes pages and closes the run, then releases the claim.
- The typestate `SyncHandle` → `RunningSync` gives the UI and scheduler the claim result before the Sync ends, without a second error copy.
- `ClaimGuard` (`src/sources/claim.rs`) replaces `ClaimedSync`'s Drop. If the Sync task panics, the guard's Drop releases the claim, and `reset_interrupted_sync` closes the run.
- `SyncRun` (`src/sources/sync_run.rs`) is the only writer of a Sync run besides crash recovery:
  - `open` marks the Source `'syncing'` and inserts the `'running'` row;
  - `succeed(&Transaction, &SyncSummary)` closes the run inside the page transaction;
  - `fail` closes it and returns errors instead of swallowing them.
  - `reset_interrupted_sync` stays in `claim.rs` as crash and takeover recovery.
- Driver seam (`SyncDriver`, a boxed-future method, no `async-trait` crate):
  - The production adapter dispatches on `SourceKind` to `local::collect` (on the blocking pool) and `web::collect`.
  - It runs the Private-origin check (`origin.rs`) before every web Sync.
  - The test adapter `FakeDriver` makes the seam real.
- Persist module `src/ingest/persist.rs` holds `SyncPages` (outcomes, missing-page policy, warnings, and an `on_written` report hook), `PageIngestOutcome`, `MissingPagePolicy`, `SyncSummary`, and `write_pages(&Transaction, ..)`.
- The web driver hands back an `on_written(ok, duration)` hook. The lifecycle calls it after the write, so the `web_sync_summary` line still has `ok` and `write_ms` without the lifecycle knowing about web accounting.

### Alternatives rejected

- **Spawn the task inside `ClaimedSync::run` only, with the guard still built after the hop.** This fixes B for MCP but leaves A open for `claim()`/`remove()` and for any caller dropped during the claim hop.
- **Keep the claim in a hop the caller awaits, and catch the drop with a `Drop` impl on the future.** A future cannot learn whether a detached blocking task committed, so this stays racy.
- **The lifecycle logs `web_sync_summary` itself.** That puts web accounting into `src/sources/`; the `on_written` hook keeps it in `web.rs`.
- **A separate transaction for the 'ok' close.** This regresses crash consistency: pages could commit while the run stays `'running'`.
- **The `async-trait` crate for the driver seam.** It adds a dependency for one method.
