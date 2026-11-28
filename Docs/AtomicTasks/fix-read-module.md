# fix-read-module: one read module for Sources and Pages

Origin: architecture review 2026-11-28, candidate 03 (rated Strong). Worktree `fix-read-module`, branched from `main` @ 116f42c.

## Findings

Each claim from the review, checked against the code at 116f42c.

### Pages: the UI builds the Page list twice

| Claim | Verdict | Evidence |
|---|---|---|
| The server runs plain, FTS and LIKE queries, and `Err(_) => query_pages_like(..)` swallows every FTS error | **Confirmed** | `src/ui/api.rs:300-306`. Every FTS failure falls back to LIKE, whether it is a bad search term or a broken `pages_fts`. A probe (Python `sqlite3`, SQLite 3.53) shows that every rejected term is plain `SQLITE_ERROR`: `foo-bar` gives `no such column: bar`, `api/v2` gives `fts5: syntax error near "/"`, `*` gives `unknown special query`. A schema fault gives the same code, and `no such column` is the same message. No error-code or message test can tell bad input from a real failure. |
| `queryAllSourcePages` (`app.js:776-827`) builds SQL strings and posts them to `/api/query` | **Confirmed** | `src/ui/assets/app.js:776-815`. It interpolates the status and search term into SQL through `escapeSqlLiteral`. |
| The client copy selects no `http_status` or `error_message`, so the HTTP badge never shows in that mode | **Confirmed** | `app.js:782,805` select 7 columns. `mapQueryRowsToPages` (`app.js:817-828`) maps no `http_status`. `renderBrowserPageList` (`app.js:870-873`) shows `HTTP …` only when `p.http_status` is set, so it never does in All-sources mode. |
| The client copy stops at `LIMIT 100`; the server has no limit | **Confirmed** | `app.js:786,808` (`LIMIT 100`, plus `limit: 100` in the body). `query_pages_*` in `api.rs` have no `LIMIT`. The count badge (`browser-page-count`) shows 100 when more Pages match. |
| The client copy's `catch {}` swallows errors | **Changed** | `app.js:794` catches every FTS failure and retries with LIKE. This is the same swallow as the server's, not a silent empty list. Errors from the LIKE retry do reach the user (`searchBrowserPages`, `app.js:850`). |
| `renderPageStatusBanner` has its own Error-page rule (`status === "error" \|\| errMsg \|\| http >= 400`), while the counts and the filter use `pages.status` | **Confirmed** | `app.js:906-911`. The `v_sources` counts and the `status` filter use `pages.status` only (`schema.sql:30-31`, `CHECK (status IN ('active','stale','error'))`). |

### Stale/Error Page lists in other places

| Place | SQL | Callers | Verdict |
|---|---|---|---|
| `ingest::show_source` issue pages (`ingest/mod.rs:244-249`) | `pages WHERE source_id = ? AND status IN ('stale','error') ORDER BY uri` | CLI `show` (`write_source_detail`), CLI `rm` existence check | **Confirmed** duplicate of the Page list, with its own row type `PageIssueRecord` |
| `app.js` `loadHealthPages` (`app.js:1083-1092`) | Fixed SQL through `/api/query`: stale/error Pages across all Sources, `ORDER BY last_checked_at DESC LIMIT 100` | Changes & Health view | **Confirmed**: a third copy, with the same silent 100-row cap |
| `app.js:22-24` SQL preset "Stale & error pages" | Sample SQL for the SQL Explorer | SQL Explorer | **Changed**: an editable example for the raw-SQL tool, not a list the UI reads. Keep it. |
| `src/skill/SKILL.md:78-85` | `WHERE p.status != 'active'` | Agents, through MCP `query` | **Changed**: the `CHECK` on `pages.status` makes `!= 'active'` equal to `IN ('stale','error')`. It is a raw-SQL recipe for MCP `query`, which stays raw SQL by invariant. No defect. |

### Sources: the row is read by position with a hand-copied column list

| Claim | Verdict | Evidence |
|---|---|---|
| The 15-column SELECT appears five times in `ingest/mod.rs` and twice in `daemon/mod.rs` | **Confirmed** | `ingest/mod.rs:183, 206, 344, 357, 370`; `daemon/mod.rs:339` (due for refresh), `daemon/mod.rs:643` (local Sources for the watcher). `map_source_record` reads columns 0-14 by position (`ingest/mod.rs:276-315`). |
| The daemon imports the crate-private `map_source_record` | **Confirmed** | `daemon/mod.rs:22`, used at `:355` and `:650`. |
| `u32::try_from(..).unwrap_or(DEFAULT_..)` silently replaces out-of-range stored values | **Confirmed, latent** | `ingest/mod.rs:306-307`. `sources.max_pages`/`max_depth` have no `CHECK` (`schema.sql:9-10`). Every write path takes a `u32`, so only a hand edit of `vault.db` can store a negative or oversized value. When that happens, a Source with `max_pages = -1` is read back as 500, with no error. |

The full set of Source-row reads at 116f42c:

| Read | SQL | Callers |
|---|---|---|
| `ingest::list_sources` | `v_sources ORDER BY name` | CLI `list`, `GET /api/sources` |
| `ingest::show_source` | `v_sources WHERE name = ?` | CLI `show`, CLI `rm` (check) |
| `load_source_by_id` | `sources WHERE id = ?` | `sources/mod.rs` (`add`, `claim_record`, `update`, registration) |
| `load_source_by_name` | `sources WHERE name = ?` | `sources/mod.rs` (`claim`, `update`) |
| `load_all_sources` | `sources ORDER BY name` | `sources/mod.rs` (`sync_all`) |
| `daemon::select_due_web_sources_at` | `sources` + last attempt from `sync_runs` | scheduler tick (`load_due_web_sources`) |
| `daemon::load_local_sources` | `sources WHERE kind = 'local' ORDER BY id` | folder watcher |
| `ui::api::resolve_source_id_and_name` | `SELECT id, name FROM sources WHERE id = ? / name = ?` | UI `DELETE`, `sync`, `pages` handlers |

### The missing-vault rule varies by entry point

Nine `db_path().exists()` checks: `ingest/mod.rs:178, 200`; `ui/api.rs:316, 388, 453`; `sources/mod.rs:150, 181, 657, 723`. There are also `db_path.exists()` checks in `daemon/mod.rs:515, 638`, `mcp/mod.rs:198` and `db/readonly.rs:89`.

| Entry point | Missing vault gives |
|---|---|
| CLI `list`, UI `GET /api/sources` | empty list |
| CLI `show`, CLI `rm`, UI pages/page/diff/sync/delete | NotFound (`source 'x' not found`, `page n not found`) |
| `SourceLifecycle` `sync_all` / `claim` / `update` / `remove` | empty / NotFound |
| daemon scheduler and watcher | empty list |
| CLI `query` (`readonly.rs:89`) | NotFound (`vault database not found at …`) |
| MCP `query` (`mcp/mod.rs:196-203`, `ensure_db_initialized`) | **creates** `vault.db` with the schema, then runs the query |
| `citadel ui` / `serve` | creates the vault at startup (`ui/mod.rs:126`, `serve.rs:30`), so their handlers never see a missing vault |

**Verdict: confirmed.** Every read except MCP `query` treats a missing vault as an empty vault: lists are empty and lookups are NotFound. CLI `query` cannot run SQL against tables that do not exist, so its NotFound is the honest form of "empty".

**Decided rule:** a read never creates the vault. A missing vault reads as an empty vault: lists are empty, and a lookup by name or id is NotFound. The read module owns this rule. MCP `query` breaks it, but `src/mcp/` is outside this task's region and its raw SQL is an AGENTS.md invariant, so that is a follow-up, not part of this fix.

### Blocking SQLite inside async handlers

| Claim | Verdict | Evidence |
|---|---|---|
| UI handlers run blocking SQLite inside async fns, with no `spawn_blocking` | **Confirmed** | `api.rs:188` (`list_sources`), `:297` (`list_source_pages`), `:320` (`get_page`), `:392` (`get_page_diff`), `:445` (`query`, which can run up to the 5 s read-only timeout). `resolve_source_in_vault` (`:226`, `:248`) also blocks before the `sync` and `DELETE` handlers hand off. In contrast, MCP `query` already uses `spawn_blocking` (`mcp/mod.rs:282`). |
| `open_rw_db` re-runs `create_dir_all`, the pragmas and the whole schema batch on every call | **Confirmed; the cost is CPU, not lock waits** | `db/mod.rs:18-27`. A probe with a writer holding `BEGIN IMMEDIATE` in WAL mode shows the `CREATE … IF NOT EXISTS` batch and the pragmas return at once (0.00 s), so a UI read does not wait on a running Sync. The cost is the extra work on every request. |

**Risk under `serve`:** `serve` runs one multi-thread runtime (`cli/mod.rs:456`, one worker per core). The `/mcp` handlers, the scheduler and the UI share those workers. Every UI read holds a worker for its whole SQLite call: up to 5 s for `/api/query`, and, for a search, a scan of `content_md` across the vault. In a 1-2 CPU container, two slow UI reads can stall `/mcp` responses and scheduler ticks. The All-sources endpoint this task adds makes vault-wide scans a normal Browser action, so its reads must run off the async workers.

## Task

Add one read module for Sources and Pages, `src/ingest/read.rs`, and route every Source and Page list read through it:

- Pages of one Source or of all Sources, by a set of statuses and a search term, with one search rule that cannot hide a real error.
- Sources by name, by id, all of them, and those due for refresh.
- The module owns the column lists, the row mapping (private) and the missing-vault rule. It opens a read-only connection and never creates the vault.

Callers:

- **UI:** `GET /api/pages?source=&status=&q=` serves both "All sources" and one Source. `GET /api/sources/:id/pages` stays, through the same read. `app.js` stops building SQL for the Browser and the Health list. The Browser shows `http_status` in both modes. The banner's Error rule becomes `status === "error"`, the server's rule. Every UI read runs in `spawn_blocking`.
- **CLI:** `list`/`show` keep calling `ingest::list_sources`/`show_source`. These are now backed by the module, and `show`'s Stale/Error list is a Page-list read.
- **Daemon:** the due-for-refresh and local-Source reads call the module. The daemon no longer imports `map_source_record`.
- **Unchanged:** MCP `query` stays raw read-only SQL through `src/db/readonly.rs`.

## Acceptance criteria

1. `GET /api/pages` with no `source` returns Pages from every Source, including `http_status` and `error_message`. It has no row cap, the same as the per-Source endpoint. Test: `ui::tests::all_sources_page_list_has_http_status_and_no_cap`.
2. `status` accepts a comma-separated subset of `active,stale,error`. An unknown status is 400, and an unknown `source` is 404, on both endpoints.
3. A search cannot fail on user input. Each word is matched as an FTS5 phrase in title and content (a trailing `*` makes it a prefix), or the whole term as a URI substring. Any SQLite error propagates: a vault whose `pages_fts` is missing returns an error, not LIKE results. Test: `ingest::read::tests::search_surfaces_real_errors`.
4. A missing vault: list reads return empty, lookups return NotFound, and no `vault.db` is created. Test: `ingest::read::tests::missing_vault_reads_as_empty_and_is_not_created`.
5. An out-of-range stored `max_pages`/`max_depth` is a read error, not a silent default.
6. Neither `daemon/mod.rs` nor any other module outside `ingest::read` names a Source column list or a Source row mapper (`grep -n map_source_record src` matches only `src/ingest/read.rs`).
7. `app.js` has no `queryAllSourcePages`, `escapeSqlLiteral` or Browser/Health `/api/query` call, and `renderPageStatusBanner` keys its Error banner on `status === "error"`.
8. SPEC §7 lists `GET /api/pages` and the `status` list format. The §2 layout names `ingest/read.rs`.
9. The gate passes: `cargo fmt --check`, both clippy runs, and `cargo test`. The REVIEW.md headless-Chrome loop runs on a tempdir vault that has error Pages.

## Design decisions

**Placement: `src/ingest/read.rs`, re-exported from `ingest`.** SPEC §2 already makes `ingest/mod.rs` the home of the record types and the read views. A child module keeps that home and gives the reads their own file: the column lists, the mappers and the vault-open rule are private to it. The record types stay in `ingest/mod.rs`, because `sources/` and `web.rs` also use them. *Rejected:* a top-level `src/vault/` or `src/read/` module. It would move the types away from SPEC's layout and touch `sources/`, which is outside this task's region.

**Interface (small):**
- Vault-level reads take `&VaultConfig` and own the missing-vault rule: `list_sources`, `show_source`, `list_pages(&PageQuery)`, `find_source(key)` (id, then name), `all_sources`, `due_web_sources(now)`.
- Connection-level reads `load_source_by_id`, `load_source_by_name` and `load_all_sources` keep their signatures, because `SourceLifecycle` calls them inside its own write transactions. Keeping them means `src/sources/` is not touched.
- `PageQuery { source_id: Option<i64>, statuses: PageStatuses, search: Option<String> }`. `PageStatuses::parse("stale,error")` is the single place that validates a status.

**Search: quote the term instead of falling back on error.** Each whitespace-separated word becomes an FTS5 phrase (`"…"`, with embedded quotes doubled; a trailing `*` keeps prefix search). A probe of `foo-bar`, `"unterminated`, `foo:bar`, `a AND`, `*`, `NEAR(`, `api/v2`, a lone `"` and `say "hi"` shows that the quoted form never errors. As a result, every SQLite error from a search is real and propagates. The same query also matches the term as a URI substring (`LIKE … ESCAPE`), which keeps the search box's promise ("Search title, URI, or Markdown content"). FTS never indexed the URI; before this change, a URI search worked only when FTS happened to reject the term. *Behaviour change:* hyphenated or slashed terms (`c-d`, `api/v2`) now match as FTS phrases, not as LIKE substrings of the content. FTS5 operators (`AND`, `NEAR`, column filters) typed in the Browser are matched as words. *Rejected:* falling back to LIKE on `SQLITE_ERROR`, because it cannot separate bad input from a broken index (see Findings).

**No row cap in either mode.** The per-Source endpoint never had one, and a silent cap makes the count badge lie. Keeping the response a plain array keeps the existing endpoint's contract. *Risk:* in a large vault (for example 20 Sources × 500 Pages), "All sources" returns about 10,000 rows, around 3 MB of JSON, and renders that many list items. *Rejected:* a cap with a `truncated` flag, because it changes the response shape of an existing endpoint. That belongs in a later paging task.

**Read connections are read-only and skip the schema batch.** `SQLITE_OPEN_READ_ONLY` plus `busy_timeout`. This keeps reads from writing or creating the vault, and from re-running `create_dir_all`, the pragmas and the DDL. Writers (`SourceLifecycle`, Syncs) still use `open_rw_db`, so new tables still reach old vaults on their first write.

**Health list moves to the endpoint.** `loadHealthPages` calls `GET /api/pages?status=stale,error`, and the client sorts by `last_checked_at` descending. `PageListItem` gains `source_name` (one join), which the Health table needs and the All-sources list can show. This removes the third Stale/Error copy and its hidden 100-row cap. The SQL Explorer preset and the SKILL.md recipe stay: both are raw-SQL examples.

**Out-of-range stored limits become errors.** The mapper reads `max_pages`/`max_depth` as `u32`, so rusqlite reports `IntegralValueOutOfRange` for the column. This is loud rather than a silent default. The value can only come from a hand edit.

**Blocking reads run in `spawn_blocking`.** One helper in `ui/api.rs` wraps every handler's read, including `/api/query` and the source resolution before `sync`/`DELETE`. There is no unit test: a blocked worker is not observable through `oneshot`. The evidence is the code review, plus the measurement in Findings.

**Follow-ups, not done here:** MCP `query` creates the vault on a missing path (`mcp/mod.rs:196`, outside this task's region). The `exists()` checks in `sources/mod.rs` (four) already follow the decided rule; folding them into the module means touching `sources/`.
