# Atomic task: compile WebScope once

Source: architecture review 2026-11-28, candidate 04 (first raised 2026-11-24).
Worktree: `fix-webscope`.

## Findings

Evidence: the red table test `ingest::web::scope_tests::crawl_and_store_filters_agree` on `main` @ 116f42c.
For each pattern and URL, it compares `WebScope::should_store` with spider's own `Website::is_allowed_default`.
That is the check spider runs before it fetches a link: `RegexSet::is_match` over the full absolute URL (spider 2.54.2, `website.rs:2223`).
The test calls it on the crawler that `configure_website` builds, after `configure_allowlist()`.
19 rows failed on `main`; 6 of them are storable URLs that spider never crawls. A second red test, `tests::crawl_fetches_only_what_the_patterns_store` (a loopback-fixture crawl), also fails on `main`: with include `docs/[!d]*`, `/docs/api` is never fetched or stored.

| # | Claim | Verdict | Evidence (pattern → URL: store / crawl on `main`) |
|---|---|---|---|
| 1 | Glob regexes lack an end anchor | **Confirmed** | include `docs/*.md` → `/docs/x.mdx`: store ✗ / crawl ✓. As an exclude, the same pattern blocks a storable URL: store ✓ / crawl ✗. `docs/v?` → `/docs/v12`: ✗ / ✓. |
| 2 | `[!x]` is copied verbatim | **Confirmed** | `docs/[!d]*` → `/docs/api`: ✓ / ✗ (never fetched). `/docs/draft`: ✗ / ✓. The mirror case `[^a]` (glob: the set {`^`, `a`}; regex: not `a`) fails the same way. |
| 3 | Asset rule is case-sensitive on the crawl side | **Confirmed, and wider** | `/img/LOGO.PNG`: ✗ / ✓. `/img/a.png#top`: ✗ / ✓ (the regex allows only `?…` after the extension). `/page?file=a.png`: ✓ / ✗, because the regex reads the query string, while `is_static_asset_url` reads only the path. |
| 4 | `is_excluded` rebuilds per URL and fails open | **Changed** | It does rebuild a `PatternMatcher` for every URL (and `is_included` does too). Its `let Ok(..) else { return false }` cannot fire, because `WebScope::new` already compiled the same patterns. The real fail-open is in spider: `Configuration::get_whitelist`/`get_blacklist` (`configuration.rs:1186-1246`) turn a `RegexSet::new` error into an **empty** set. An empty whitelist admits everything, and an empty blacklist drops the asset rule and every exclude. Glob accepts patterns that `glob_to_regex` turns into invalid regex: `docs/[z-a]*` (reversed range) and `docs/[]a]*` (glob closes the class at the second `]`). An include like that crawls the whole host (`/blog/a`: ✗ / ✓); an exclude like that crawls `/img/a.png`. |
| 5 | `let _ = &scope.matcher;` is a no-op | **Confirmed** | `WebScope.matcher` is never read; `web.rs:381` only touches it. |

More disagreements the table found:
- `**/` matches zero directories in glob, but `glob_to_regex` emits `.*.*/`. Include `docs/**/x` → `/docs/x`: ✓ / ✗.
- `*` crosses into the query string in the regex. Include `docs/*.md` → `/docs/a?x=.md`: ✗ / ✓.
- The store side also tries relative patterns against the full URL string. Include `*docs*` on Origin `https://docs.ex.com/` stores `/other` (the host contains `docs`), but the crawl never admits it.
- An absolute prefix pattern ignores the query on the crawl side but not on the store side. `https://ex.com/docs` → `/docs?x=1`: ✗ / ✓.

The fixture-crawl tests use only patterns where both forms agree (`docs/*`, `docs/p*`, `docs/drafts/*`), so none of this was caught.

**Can spider take a predicate?** No. It has no URL filter that runs before the fetch.
- `on_should_crawl_callback` receives an already-fetched `Page` (the budget is already spent) and only stops link expansion.
- `on_link_find_callback` rewrites links; it does not filter them.

So the crawl filter has to stay regexes. They must be correct, anchored, and generated from the same parse as the store filter.

**Where patterns are validated today:** `SourceLifecycle` (`src/sources/mod.rs`, `register_source_row` and `update_source_row`) calls `local::validate_patterns`, the glob parse, for every Source kind. A pattern that glob accepts and regex rejects passes validation and then fails open at crawl time (claim 4).

## Task

WebScope parses each include/exclude pattern once, into one anchored URL regex. The crawl filter (spider's whitelist/blacklist) and the store filter (`should_store`/`should_crawl`) both use those same regex strings, with one static-asset rule. Web Sources validate their patterns with that same parser when they are added or updated.

## Acceptance criteria

1. `crawl_and_store_filters_agree` passes. For each row, the expected store result equals `should_store`, the expected crawl result equals spider's `is_allowed_default`, spider never drops a compiled set, and no storable URL is uncrawlable. The Origin page is the one deliberate exception: it is crawled for its links but not stored unless an include matches it.
2. A differential test shows that a relative pattern's regex matches the same paths as `glob::Pattern::matches` (glob 0.3.4, default options) over a corpus that covers `*`, `?`, `**`, `[..]`, `[!..]`, `[^..]`, `[]..]`, ranges and reversed ranges.
3. A loopback-fixture crawl with includes `docs/[!d]*` and `guide/*.md`, over the pages `/docs/api`, `/docs/draft`, `/guide/x.md`, `/guide/x.mdx` and `/img/LOGO.PNG`, stores and requests only `/docs/api` and `/guide/x.md` (plus the Origin, `robots.txt` and the discovery files).
4. Adding or updating a web Source with a pattern its parser rejects fails with a validation error through `SourceLifecycle`. Local Sources keep the glob validation.
5. `PatternMatcher` is no longer used by WebScope; `WebScope.matcher` and `web.rs:381` are gone; nothing rebuilds a matcher per URL.
6. `Docs/SPEC.md` §4.1 documents the scope-pattern semantics.
7. The gate passes: `fmt`, `clippy` (default and `--no-default-features`), `cargo test`.

## Design decisions

**Interface.** `WebScope::new(origin, includes, excludes) -> Result<WebScope>` keeps its signature, and so do `should_crawl`, `should_store`, `whitelist_regexes` and `blacklist_regexes`, so `collected.rs` and the crawl code do not change. Inside, `new` does all the work once:
- It parses each pattern into a `UrlPattern`: absolute or relative, a prefix or a glob token list (tokenized by glob 0.3.4's rules, after `glob::Pattern::new` has accepted it).
- It renders each `UrlPattern` as anchored regex strings: one for an absolute pattern, one for root-relative and one for Origin-relative.
- It compiles three `regex::RegexSet`s from those strings: crawl-allow (the Origin page plus the includes, or the path prefix), store-allow, and deny (the asset rule plus the excludes). An error becomes `VaultError::Validation`.

spider receives the identical strings, so store ⊆ crawl holds by construction.

**Semantics** (written into SPEC §4.1):
- A pattern matches the whole URL path.
- `*`, `?` and classes never cross into the query string or fragment, and any query string is accepted after a match.
- `**/` matches zero or more directories.
- `[!x]` is negation. `[^x]` is the set {`^`, `x`}, as in glob.
- A reversed range matches nothing, as in glob.
- Relative patterns match the path from the host root or from the Origin's path prefix, never the host.
- Assets are judged by the path's extension, case-insensitively.

**Validation.** `sources/mod.rs` branches on the Source kind: web Sources call `web::validate_scope_patterns`, which runs the same parse and regex build; local Sources keep `local::validate_patterns`. This is an edit outside the assigned region and is named in the report.

**Dependency.** `regex` becomes a direct dependency at the version already in `Cargo.lock` (1.13.2, the same crate spider compiles its sets with), so no new crate enters the build.

**Alternatives rejected:**
- *Crawl filter as a predicate callback.* spider has no pre-fetch hook (see Findings).
- *Keep `glob::Pattern` for the store side and fix only the regexes.* Two engines would still have to agree by test instead of by construction, and the regex for a glob has to be written anyway.
- *Reject reversed ranges as invalid.* Glob accepts them, and local Sources share the pattern syntax. Matching nothing keeps one meaning for both kinds.
- *Move `PatternMatcher` into a shared module.* Local Sources keep their glob matcher unchanged. Only WebScope stops using it, which keeps the diff out of `local.rs`.

**Risk: existing web Sources can change scope in both directions.**
- **Narrower.** On the next complete Sync, the pages that leave the scope go stale.
  - A Source whose patterns used to fail open, such as `docs/[]a]*` or `docs/[z-a]*`, gets its real scope.
  - Relative patterns no longer match the full URL or the host: `*docs*` on `docs.ex.com` stops storing everything.
  - `?` in an absolute pattern no longer matches the query separator.
  - Wildcards stop at the query string.
- **Wider.** New pages are stored.
  - Excludes are end-anchored, so the exclude `docs/*.md` no longer blocks `.mdx`.
  - `[!x]` and `**/` includes now admit what glob says they should.
  - `/page?file=a.png` is no longer blacklisted.

**Tests that were never red.** Three tests passed from the start:
- `relative_pattern_regex_matches_like_glob` and `patterns_glob_rejects_are_validation_errors`, which pin the new parser.
- `sources::tests::web_source_patterns_are_validated_on_add_and_update`.

Every glob-valid pattern now translates to a regex, so the web parser accepts exactly the patterns that local validation accepts. The lifecycle wiring keeps validation and Sync on one parser; it does not fix a separate bug.

## Review fixes

Source: the independent `/code-review` of `116f42c..731a597`. Fix commits: `ad27266` (code), `4d5b100` (SPEC §4.1).

| # | Finding | Outcome | Evidence |
|---|---|---|---|
| 1 | No version bump | **Deferred** by instruction | The bump and the `vX.Y.Z` tag happen once, at merge. |
| 2 | A `?` meant as the query separator is accepted but matches nothing | **Fixed: rejected by validation** | `ad27266`. `UrlPattern::parse` rejects a pattern whose `?` is followed by `=` or `&` and says that patterns match the path only. Red then green: `patterns_glob_rejects_are_validation_errors`, rows `https://ex.com/search?q=rust`, `search?q=*` and `list?a=1&b=*`. |
| 3 | Patterns are matched against the percent-encoded URL (pre-existing) | **Fixed** (contained to parsing) | `ad27266`. Literal characters are encoded with url's path set (`\` becomes `/`). `?` and classes match one encoded character. A negated class or a range with an encoded member is rejected. Red then green: `crawl_and_store_filters_agree` (rows `my docs/*`, `docs/é*`, `docs/v?` on `v%C3%A9`, `[^a]` on `%5E`) and `relative_pattern_regex_matches_like_glob`. That test now runs the regex on the path `Url::set_path` encodes, not on a decoded path no URL has (13 failures were red). |
| 4 | An absolute pattern with an uppercase host or a default port matches nothing (pre-existing) | **Fixed** | `ad27266`. `normalize_absolute_pattern` lowercases the scheme and authority and drops `:80`/`:443`. Red then green: `crawl_and_store_filters_agree` rows `HTTPS://Ex.com/docs` and `https://ex.com:443/docs/*`. |
| 5 | An Origin that names a file scopes only that page and the paths below it (pre-existing) | **Deferred: follow-up** | It is out of this task's scope and unchanged from `main`. `normalize_prefix` keeps the file segment. A follow-up should decide whether `/guide/index.html` scopes `/guide/`. |
| 6 | Stale `spider_admits` doc comment | **Fixed** | `ad27266` describes the returned tuple. |

Choice for finding 2: rejection, not literal query matching. Every other part of the scope ignores the query string (`QUERY_TAIL`), so matching it in one kind of pattern would add a second rule. The error tells the user what to write instead.

Remaining risk:
- A stored web Source with a query-like pattern now fails its Sync with a validation error. Updating its patterns fixes it.
- A literal `%` in a pattern is kept as written. A hand-encoded `%c3` (lowercase hex) does not match the uppercase hex that `url` emits, but URLs a site links to can keep their own casing.

## Second review

Source: the second `/code-review medium --fix` of `116f42c..433193b`.

| # | Finding | Outcome | Evidence |
|---|---|---|---|
| 1 | (important) A stored web Source whose patterns the new parser rejects fails every Sync before a Sync run starts, so it never shows `error` | **Not applied: false positive** | `sync_web_source` runs only through `SourceLifecycle::run_sync`. `sync_record` turns any early error into an error Sync run and `status = 'error'` (`record_sync_failure`). No `web_sync_summary` line is logged when the Origin checks fail, as documented. The real effect, a Source in `error` until its patterns are updated, is already listed under remaining risk. |
| 2 | The query-string check reads everything after the first `?`, so `wiki/v?/a=b` is rejected | **Applied** | `UrlPattern::parse` now rejects only a `?` followed by `=` or `&` in the same path segment, for every `?`. Rows: `wiki/v?/a=b` accepted; `docs/v?/x?q=1` rejected. SPEC §4.1 updated. |
| 3 | Escapes a pattern adds match only uppercase hex, but `url` keeps a link's own escapes as written | **Applied** | `encoded_literal` emits `%[Cc]3%[Aa]9`. Row: `docs/é*` matches `/docs/%c3%a9a` (store and crawl). SPEC §4.1 updated. A hand-encoded escape in a pattern is still matched as written. |
| 4 | An absolute pattern with a non-ASCII (punycode) or IPv6 host never matches | **Deferred: follow-up** | It needs absolute patterns to parse their authority with `Url::parse` and glob only the path, which changes what `*` means in a host. Rare. |
| 5 | No version bump | **Deferred** by instruction | The bump and the `vX.Y.Z` tag happen once, at merge. |
| 6 | A prefix pattern with `#` (`guide#legacy`) is encoded to `%23` and no longer matches the fragment URL | **Deferred: needs a decision** | Rejecting `#` (as with query patterns) or documenting it both change behavior. Fragments name the same page, so the impact is small. |

## User decisions

| Decision | Change | Evidence |
|---|---|---|
| Second review finding 6 (`#` in patterns): **reject** | `UrlPattern::parse` rejects any pattern that contains `#`, next to the query-pattern rule, with the message "pattern '…' contains '#'; patterns match only the URL path (fragments are ignored), so remove the '#...' part". Add and update through `SourceLifecycle` reject it too. A stored web Source with such a pattern fails its next Sync with a validation error until the pattern is edited, the same as a query pattern. SPEC §4.1 is updated. | Red then green: `scope_tests::patterns_glob_rejects_are_validation_errors` (rows `docs/page#intro`, `https://ex.com/docs#*`, `docs/*#`, `#top`) and `sources::tests::web_source_patterns_are_validated_on_add_and_update` (`api/page#intro` on add, `api/*#top` on update). |
