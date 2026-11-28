# Sync claims live in the database, not in process memory

The CLI, `citadel mcp` (stdio) and `serve`/`daemon` run as separate processes against one vault, so an in-memory lock (the scheduler's old in-flight set) cannot stop two of them syncing the same Source at once. A Sync claim is therefore a row in a dedicated `sync_claims` table (primary key `source_id`, holder `pid`, holder `instance_token`, `claimed_at`): inserting the row takes the claim, deleting it releases it. Each process generates a random instance token at startup. A claim may be taken over when its holder pid is not running, when the pid is our own but the token is not ours (a previous run of this process reused the pid), or when it is older than a hard age limit.

## Considered Options

- **In-process mutex / in-flight set**: rejected; invisible to other processes.
- **Expiring lease with heartbeat**: rejected; pages persist only at the end of a crawl, so a heartbeat would need its own background task.
- **Claim columns on `sources`, or a pid on the running `sync_runs` row**: rejected; both need `ALTER TABLE`, and the schema has no migration mechanism (`init_db` re-applies `CREATE … IF NOT EXISTS`). A new table reaches existing vaults without one.

## Consequences

- The instance token exists because pids repeat: the container's `serve` entrypoint is pid 1 on every start, so a pid check alone would treat a crashed run's claim as still held by the restarted process.
- Pid liveness is only meaningful within one pid namespace. The claim does not yet record which host or namespace holds it, so on a vault shared between namespaces (the container's `serve` plus a host CLI on a bind-mounted volume, or two containers), a live holder's pid looks dead and its claim can be taken over mid-Sync. The default `make run` setup keeps the vault in a named volume used only by the container, so this needs an unusual setup. Follow-up: store a host/namespace identity with the claim and check liveness only when it matches the local one; otherwise rely on the age limit.
- There is no heartbeat, so a Sync that runs longer than the hard age limit (6h) can be taken over by another process while it still runs. A process never treats its own claim as expired.
- `sources.status = 'syncing'` remains the displayed status; `sync_claims` is the source of truth for exclusion.
- A process takes over its own claim (same pid and instance token) only when no live claim guard of that process holds it, which happens when releasing it failed. An in-process registry of live guards judges only whether this process's own claim is orphaned; exclusion still lives in `sync_claims`, the takeover uses the same guarded `UPDATE`, and other processes' claims are judged as before.
