# Citadel

A local vault that ingests documentation websites and Markdown folders into a searchable store that agents and people can query.

## Language

### Sources

**Source**:
A registered origin of pages: either a web URL or a local Markdown folder.
_Avoid_: site, repo, collection

**Origin**:
The URL or canonical folder path that identifies a Source; unique across the vault.
_Avoid_: root, base URL, seed

**Private origin**:
An Origin that points at loopback, private-network, link-local or local hostnames; refused unless the vault allows private origins.
_Avoid_: internal URL, SSRF target

**Source lifecycle**:
Everything that changes a Source: adding, updating, removing and syncing it.
_Avoid_: source service, source manager

### Syncing

**Sync**:
One pass that brings a Source's pages in line with its Origin.
_Avoid_: refresh, crawl, ingest (crawl and walk are how a Sync fetches, not the Sync itself)

**Sync run**:
The recorded history entry of one Sync, including its counts and any error.
_Avoid_: job, task

**Sync claim**:
The vault-wide exclusive right to Sync one Source; at most one holder at a time, across every process.
_Avoid_: lock, lease, in-flight

**Cut-off Sync**:
A Sync whose first crawl used up the page budget (`max_pages`), or that collected as many distinct storable pages as the budget, or recorded as many results. A result is a stored Page, an Error page, or a copy merged into another Page (a Markdown twin or Canonical duplicate). Pages count whether or not they were stored.

### Daemon

**Supervised daemon**:
The daemon of a vault whose OS service (launchd job or systemd unit) names that vault; it is started, stopped and restarted through that service, never as a detached process beside it.
_Avoid_: managed daemon, service mode

### Pages

**Page**:
One document from a Source, identified by its URI.

**Collected page**:
A page any crawl of a web Sync delivered. It becomes a Page, an Error page or a copy merged into another Page, or it is dropped, for exactly one reason.
_Avoid_: crawled page, raw page

**Stale page**:
A Page that was present before but missing from a complete Sync, or that a Sync merged into another copy of its document; kept, not deleted. A cut-off Sync leaves unvisited Pages as they were.

**Markdown twin**:
The raw Markdown copy a site serves of one of its HTML pages (`X.md` next to `X`, or the target of `<link rel="alternate" type="text/markdown">`). A Sync stores the pair as one Page, at the HTML page's URI, with the twin's content.
_Avoid_: md copy, duplicate page

**Canonical duplicate**:
A page whose `rel=canonical` names another collected page that differs only by its query string (`/guide?version=2` for `/guide`). Stored only as the canonical page.

**URL alias**:
Another URL of a collected document: a redirect source, or a `/` or `/index` variant (`/guide/`, `/guide/index.html` for `/guide`). A Sync stores the document once, at its redirect destination; among variants, a cleanly fetched one wins, then the one a `rel=canonical` names, then the shortest URL.
_Avoid_: duplicate URL

**Error page**:
A Page whose latest fetch or parse failed.
