//! The Collected pages of one web Sync, and what each becomes.
//!
//! Every crawl of a web Sync hands its pages to [`CollectedPages`]. For each Collected page it
//! decides whether it becomes a Page, an Error page or a copy merged into another Page, or is
//! dropped, counted under the first reason it meets. It also sizes each later crawl, decides
//! whether the Sync is cut off, picks the missing-page policy and adds the budget warning. It does
//! no I/O: crawling, timing and persistence stay with the caller.

use std::collections::{HashMap, HashSet};

use spider::page::Page;
use spider_transformations::transformation::content::{
    transform_content, ReturnFormat, TransformConfig,
};
use url::Url;

use super::{
    canonical_dedup_key, canonicalize_page_url, document_key, document_path, effective_url,
    resolve_seed_url, WebScope,
};
use crate::error::{Result, VaultError};
use crate::ingest::normalize::{count_words, normalize_markdown};
use crate::ingest::persist::{MissingPagePolicy, PageIngestOutcome};
use crate::ingest::sections::split_sections;

/// Counts of the pages one web Sync's crawls delivered. When every crawl finishes, each page
/// received is counted once: as `results_stored`, `results_failed`, or under one of the nine drop
/// reasons (`unparsable_url` to `binary_file`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PageCounts {
    /// Every page any crawl delivered.
    pub pages_received: usize,
    /// Pages spider delivered that the receiving task missed because it fell behind.
    pub lost_by_listener: usize,
    pub later_crawls_started: usize,
    pub results_stored: usize,
    pub results_failed: usize,
    pub unparsable_url: usize,
    pub already_collected: usize,
    pub redirected_out_of_scope: usize,
    pub url_not_storable: usize,
    pub duplicate_result: usize,
    pub binary_file: usize,
    /// Markdown copies of a collected HTML page, stored as that page (issue #4).
    pub markdown_twin: usize,
    /// Pages whose `rel=canonical` names another collected page that differs only by its query
    /// string, or names the stored copy of their URL alias group.
    pub canonical_duplicate: usize,
    /// Pages at another URL of a collected document: `x/` or `x/index` for `x` (issue #5).
    pub url_alias: usize,
    /// Extra results for the requested URL of a redirected page (`Retired`, or `Failed` when the
    /// destination failed). Not printed; left out of the cut-off count, which counts one result
    /// per page.
    pub redirect_source_results: usize,
    /// Whether the Sync is a Cut-off Sync. Decided only when it has results to write.
    pub cut_off: bool,
}

/// What the next later crawl may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LaterCrawl {
    /// An earlier crawl collected the seed, or started from it.
    Skip,
    /// The page budget is spent: start no more crawls.
    Stop,
    /// Crawl from the seed with this much of the page budget.
    Crawl(u32),
}

/// What a web Sync writes: one result per stored, failed or merged page, the policy for stored
/// pages it did not visit, and its warnings.
pub(super) struct Collected {
    pub outcomes: Vec<PageIngestOutcome>,
    pub missing_policy: MissingPagePolicy,
    pub warnings: Vec<String>,
}

/// The Collected pages of one web Sync. Hand it the first crawl's pages, then each later
/// crawl's, and [`finish`](Self::finish) it.
pub(super) struct CollectedPages<'a> {
    scope: &'a WebScope,
    max_pages: u32,
    pages: Vec<Page>,
    /// What the crawls collected so far.
    visited: Visited,
    /// Pages the first crawl delivered plus pages it lost: whether it used up spider's limit.
    first_crawl_delivered_or_lost: usize,
    /// The requested URLs of the first pages dropped as `redirected_out_of_scope`.
    redirect_examples: Vec<String>,
    counts: PageCounts,
}

impl<'a> CollectedPages<'a> {
    pub(super) fn new(scope: &'a WebScope, max_pages: u32) -> Self {
        Self {
            scope,
            max_pages,
            pages: Vec::new(),
            visited: Visited::default(),
            first_crawl_delivered_or_lost: 0,
            redirect_examples: Vec::new(),
            counts: PageCounts::default(),
        }
    }

    /// Take the first crawl's pages and the number it lost. Every page is kept: an unparsable URL
    /// or a second copy of a URI is judged in [`finish`](Self::finish).
    pub(super) fn add_first_crawl(&mut self, pages: Vec<Page>, lost: usize) {
        self.first_crawl_delivered_or_lost = pages.len() + lost;
        self.counts.pages_received += pages.len();
        self.counts.lost_by_listener += lost;
        for page in pages {
            self.visited.collect(&page);
            self.pages.push(page);
        }
    }

    /// Whether to crawl from `seed` next, and with how much budget. `Crawl` counts the crawl as
    /// started. The budget counts documents, so a Markdown twin pair takes one page of it.
    pub(super) fn next_later_crawl(&mut self, seed: &Url) -> LaterCrawl {
        if self.visited.documents.contains(&document_key(seed)) {
            return LaterCrawl::Skip;
        }
        let used_budget = count_scoped_documents(&self.pages, self.scope);
        if used_budget >= self.max_pages as usize {
            return LaterCrawl::Stop;
        }
        self.counts.later_crawls_started += 1;
        LaterCrawl::Crawl(self.max_pages - used_budget as u32)
    }

    /// Take the pages of the later crawl started from `seed`, and the number it lost. A page whose
    /// URL cannot be parsed, or that an earlier crawl collected, is dropped and counted here.
    pub(super) fn add_later_crawl(&mut self, seed: &Url, pages: Vec<Page>, lost: usize) {
        self.counts.pages_received += pages.len();
        self.counts.lost_by_listener += lost;
        for page in pages {
            match self.visited.collect(&page) {
                None => self.counts.unparsable_url += 1,
                Some(true) => self.pages.push(page),
                Some(false) => self.counts.already_collected += 1,
            }
        }
        self.visited.documents.insert(document_key(seed));
    }

    /// The counts so far, for the summary line of a Sync whose crawl failed.
    pub(super) fn counts(&self) -> &PageCounts {
        &self.counts
    }

    /// Decide what every collected page becomes, whether the Sync is cut off, and what happens to
    /// stored pages it did not visit (`prune` deletes them). Fails when no page was stored and
    /// every result is a transport failure, or there is no result at all. The counts come back
    /// either way.
    pub(super) fn finish(mut self, prune: bool) -> (Result<Collected>, PageCounts) {
        let max_pages = self.max_pages as usize;
        let collected_storable_urls = count_scoped_urls(&self.pages, self.scope);
        let pages = std::mem::take(&mut self.pages);
        let (outcomes, ok_count, transport_failures) = build_page_outcomes(
            pages,
            self.scope,
            &mut self.counts,
            &mut self.redirect_examples,
        );

        let results = result_count(&outcomes);
        if ok_count == 0 && (results == 0 || transport_failures == results) {
            let reason = outcomes
                .iter()
                .find_map(|o| match o {
                    PageIngestOutcome::Failed { error_message, .. } => Some(error_message.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    format!("no reachable pages crawled from '{}'", self.scope.origin)
                });
            return (Err(VaultError::Other(reason)), self.counts);
        }

        self.counts.cut_off = is_cut_off(
            self.first_crawl_delivered_or_lost,
            collected_storable_urls,
            outcomes.len() - self.counts.redirect_source_results,
            max_pages,
        );
        let missing_policy = if prune {
            MissingPagePolicy::Prune
        } else if self.counts.cut_off {
            MissingPagePolicy::Preserve
        } else {
            MissingPagePolicy::MarkStale
        };
        let warnings = budget_warning(&self.counts, &self.redirect_examples, max_pages)
            .into_iter()
            .collect();
        let collected = Collected {
            outcomes,
            missing_policy,
            warnings,
        };
        (Ok(collected), self.counts)
    }
}

/// What the crawls of one Sync collected so far.
#[derive(Debug, Default)]
struct Visited {
    /// [`canonical_dedup_key`] of each requested URL: a page fetched again is dropped. Alias URLs
    /// of a collected document are kept, so `pair_documents` can retire their old rows.
    fetched: HashSet<String>,
    /// [`document_key`] of each requested URL, redirect destination and finished seed: a seed
    /// that is a copy of one of them starts no crawler.
    documents: HashSet<String>,
}

impl Visited {
    /// Record a page; `Some(false)` when its URL was collected already, `None` when it does not
    /// parse.
    fn collect(&mut self, page: &Page) -> Option<bool> {
        let requested = canonicalize_page_url(page.get_url())?;
        if !self.fetched.insert(canonical_dedup_key(&requested)) {
            return Some(false);
        }
        self.documents.insert(document_key(&requested));
        if let Some(effective) = effective_url(page) {
            self.documents.insert(document_key(&effective));
        }
        Some(true)
    }
}

/// Distinct storable URLs among the collected pages. The cut-off rule uses this count: spider's
/// limit counts every fetch, Markdown twins included, so it must not be lowered by pairing.
fn count_scoped_urls(pages: &[Page], scope: &WebScope) -> usize {
    scoped_urls(pages, scope).len()
}

/// Distinct documents among the storable collected pages: a Markdown URL whose HTML page was also
/// collected (`X` and `X.md`) counts once. Sizes the budget of later crawls only, so twins do not
/// take the place of other documents; never feed it to `is_cut_off`.
fn count_scoped_documents(pages: &[Page], scope: &WebScope) -> usize {
    let urls = scoped_urls(pages, scope);
    let twins: HashSet<String> = urls
        .values()
        .filter(|url| !url.path().ends_with(".md"))
        .flat_map(markdown_twin_urls)
        .map(|twin| canonical_dedup_key(&twin))
        .filter(|key| urls.contains_key(key))
        .collect();
    urls.len() - twins.len()
}

/// The storable collected URLs, by dedup key.
fn scoped_urls(pages: &[Page], scope: &WebScope) -> HashMap<String, Url> {
    pages
        .iter()
        .filter_map(|page| canonicalize_page_url(page.get_url()))
        .filter(|url| scope.should_store(url))
        .map(|url| (canonical_dedup_key(&url), url))
        .collect()
}

/// Turn collected pages into results, counting each page that gets none in `counts` under the first
/// reason it meets. Copies of one document (see [`pair_documents`]) become one result at the
/// document's URI plus a `Retired` result per copy. A redirected page is stored at its
/// destination, and its requested URI gets a `Retired` result too.
fn build_page_outcomes(
    pages: Vec<Page>,
    scope: &WebScope,
    counts: &mut PageCounts,
    redirect_examples: &mut Vec<String>,
) -> (Vec<PageIngestOutcome>, usize, usize) {
    let superseded = pair_documents(&pages, scope);
    let twin_of: HashMap<usize, usize> = superseded
        .iter()
        .filter_map(|(&twin, kept)| match kept {
            Kept::Twin(html) => Some((*html, twin)),
            Kept::Canonical(_) | Kept::Alias(_) => None,
        })
        .collect();
    let mut outcomes = Vec::new();
    let mut seen_uris = HashSet::new();
    let mut ok_count = 0usize;
    let mut transport_failures = 0usize;

    for (index, page) in pages.iter().enumerate() {
        let (Some(requested), Some(canonical_url)) =
            (canonicalize_page_url(page.get_url()), effective_url(page))
        else {
            counts.unparsable_url += 1;
            continue;
        };

        if redirected_out_of_scope(page, scope) {
            counts.redirected_out_of_scope += 1;
            if redirect_examples.len() < MAX_REDIRECT_EXAMPLES {
                redirect_examples.push(page.get_url().to_string());
            }
            continue;
        }

        if !scope.should_store(&canonical_url) {
            counts.url_not_storable += 1;
            continue;
        }

        // A redirected page's document is stored at its destination, so a row an earlier Sync
        // stored at the requested URL no longer holds a Page of its own. When the destination
        // failed (a maintenance page, say), that row is marked `error` instead, keeping its
        // content, but no row is inserted for it.
        let status_u16 = page.status_code.as_u16();
        let is_transport = status_u16 == 0 || status_u16 >= 520 || page.error_status.is_some();
        if requested != canonical_url {
            if fetched_ok(page) {
                outcomes.push(PageIngestOutcome::Retired {
                    uri: requested.to_string(),
                });
                counts.redirect_source_results += 1;
            } else if page_failed(page) {
                outcomes.push(PageIngestOutcome::Failed {
                    uri: requested.to_string(),
                    http_status: (status_u16 > 0).then_some(i64::from(status_u16)),
                    error_message: format_page_error(page, status_u16),
                    insert_if_missing: false,
                });
                counts.redirect_source_results += 1;
                if is_transport {
                    transport_failures += 1;
                }
            }
        }

        let uri = canonical_url.to_string();
        if !seen_uris.insert(uri.clone()) {
            counts.duplicate_result += 1;
            continue;
        }

        if let Some(kept) = superseded.get(&index) {
            match kept {
                Kept::Twin(_) => counts.markdown_twin += 1,
                Kept::Canonical(_) => counts.canonical_duplicate += 1,
                Kept::Alias(_) => counts.url_alias += 1,
            }
            tracing::debug!(
                uri = %uri,
                by = pages[kept.index()].get_url(),
                "page is a copy of another collected page"
            );
            outcomes.push(PageIngestOutcome::Retired { uri });
            continue;
        }

        if page_failed(page) {
            if is_transport {
                transport_failures += 1;
            }
            let error_message = format_page_error(page, status_u16);
            outcomes.push(PageIngestOutcome::Failed {
                uri,
                http_status: (status_u16 > 0).then_some(i64::from(status_u16)),
                error_message,
                insert_if_missing: true,
            });
            counts.results_failed += 1;
            continue;
        }

        if page.binary_file {
            counts.binary_file += 1;
            continue;
        }

        // A cleanly fetched Markdown twin replaces the HTML conversion; the HTML page keeps its
        // URI, its `<title>` as the fallback title and its HTTP validators.
        let twin = twin_of
            .get(&index)
            .map(|&twin| &pages[twin])
            .filter(|twin| fetched_ok(twin));
        let (raw_md, html_title) = match twin {
            Some(twin) => (twin.get_html(), extract_html_title(&page.get_html_cow())),
            None => extract_page_markdown_and_title(page, &canonical_url),
        };
        let fallback_title = html_title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| fallback_title_from_url(&canonical_url));

        let doc = normalize_markdown(&raw_md, &fallback_title);
        let chunks = split_sections(&doc.content_md);
        let (etag, last_modified) = extract_http_validators(page);

        ok_count += 1;
        counts.results_stored += 1;
        outcomes.push(PageIngestOutcome::Ready {
            uri,
            doc,
            chunks,
            etag,
            last_modified,
            http_status: Some(i64::from(status_u16)),
        });
    }

    (outcomes, ok_count, transport_failures)
}

/// Whether a page was redirected to a URL the crawl would not follow.
fn redirected_out_of_scope(page: &Page, scope: &WebScope) -> bool {
    page.final_redirect_destination
        .as_deref()
        .and_then(canonicalize_page_url)
        .is_some_and(|dest| !scope.should_crawl(&dest))
}

/// A page whose fetch failed: an HTTP error status, no status at all, or a transport error.
fn page_failed(page: &Page) -> bool {
    let status = page.status_code.as_u16();
    status >= 400 || status == 0 || page.error_status.is_some()
}

/// A page that fetched cleanly with text content.
fn fetched_ok(page: &Page) -> bool {
    !page_failed(page) && !page.binary_file
}

/// The page whose result a superseded page's document is stored under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kept {
    /// The superseded page is the Markdown twin of the HTML page at this index.
    Twin(usize),
    /// The superseded page is a canonical duplicate of the page at this index.
    Canonical(usize),
    /// The superseded page is a URL alias of the page at this index.
    Alias(usize),
}

impl Kept {
    fn index(self) -> usize {
        match self {
            Self::Twin(i) | Self::Canonical(i) | Self::Alias(i) => i,
        }
    }
}

/// Find the copies of one document among the collected pages, as a map from each superseded
/// page's index to the page its document is stored under. Every page is placed at its effective
/// URL ([`effective_url`]: where its redirects ended).
/// - **URL alias:** pages whose effective URLs share a [`document_key`] (`/p`, `/p/`,
///   `/p/index.html`) are one document. The stored copy does not depend on arrival order: a
///   cleanly fetched page beats a failed one, then the URL a member's `rel=canonical` names, then
///   the shortest URL, then the lowest. A member whose canonical names that copy is counted as a
///   canonical duplicate, any other as an alias.
/// - **Canonical duplicate:** an HTML page whose `<link rel="canonical">` names another
///   collected, cleanly fetched page that differs only by its query string, e.g.
///   `/guide?version=2` -> `/guide`. Requiring the same path keeps a site that points every
///   canonical at its home page from collapsing into one page.
/// - **Markdown twin:** the Markdown copy of a cleanly fetched HTML page `X`: the target of its
///   `<link rel="alternate" type="text/markdown">`, else `X.md`, else `X/index.md`, when that URL
///   was collected and is Markdown. A twin whose HTML page failed is not paired, so it is
///   handled like any other page.
fn pair_documents(pages: &[Page], scope: &WebScope) -> HashMap<usize, Kept> {
    // Only the first page per exact effective URI takes part, as only it gets a result in
    // `build_page_outcomes`.
    let mut taken: HashSet<String> = HashSet::new();
    let urls: Vec<Option<Url>> = pages
        .iter()
        .map(|page| {
            effective_url(page)
                .filter(|url| scope.should_store(url) && !redirected_out_of_scope(page, scope))
                .filter(|url| taken.insert(url.to_string()))
        })
        .collect();
    let links: HashMap<usize, HeadLinks> = pages
        .iter()
        .zip(&urls)
        .enumerate()
        .filter_map(|(index, (page, url))| {
            let url = url.as_ref()?;
            (fetched_ok(page) && !is_markdown_response(page, url))
                .then(|| (index, head_links(&page.get_html_cow(), url)))
        })
        .collect();
    let canonical_of = |index: usize| links.get(&index).and_then(|l| l.canonical.as_ref());

    let mut groups: HashMap<String, Vec<(usize, &Url)>> = HashMap::new();
    for (index, url) in urls.iter().enumerate() {
        if let Some(url) = url {
            groups
                .entry(document_key(url))
                .or_default()
                .push((index, url));
        }
    }
    let mut superseded: HashMap<usize, Kept> = HashMap::new();
    let mut stored_by_key: HashMap<String, usize> = HashMap::with_capacity(groups.len());
    for (key, members) in groups {
        let named: HashSet<&str> = members
            .iter()
            .filter_map(|&(index, _)| canonical_of(index))
            .map(Url::as_str)
            .collect();
        let Some(&(kept, kept_url)) = members.iter().min_by_key(|&&(index, url)| {
            let uri = url.as_str();
            (
                !fetched_ok(&pages[index]),
                !named.contains(uri),
                uri.len(),
                uri,
            )
        }) else {
            continue;
        };
        for &(index, _) in &members {
            if index == kept {
                continue;
            }
            let copy = if canonical_of(index) == Some(kept_url) {
                Kept::Canonical(kept)
            } else {
                Kept::Alias(kept)
            };
            superseded.insert(index, copy);
        }
        stored_by_key.insert(key, kept);
    }
    let lookup = |url: &Url| stored_by_key.get(&document_key(url)).copied();
    let path_key = |url: &Url| {
        let mut url = url.clone();
        url.set_query(None);
        document_key(&url)
    };

    // The pages left after merging aliases, in arrival order so results are deterministic.
    let mut html_pages: Vec<(usize, &Url)> = stored_by_key
        .values()
        .filter(|index| links.contains_key(index))
        .filter_map(|&index| Some((index, urls[index].as_ref()?)))
        .collect();
    html_pages.sort_unstable_by_key(|&(index, _)| index);

    // Kept apart from `superseded` until chains are removed: an alias whose stored copy turns
    // out to be a canonical duplicate must stay superseded.
    let mut canonical_duplicates: HashMap<usize, usize> = HashMap::new();
    for &(index, url) in &html_pages {
        let Some(canonical) = canonical_of(index) else {
            continue;
        };
        let Some(kept) = lookup(canonical) else {
            continue;
        };
        if kept != index && path_key(canonical) == path_key(url) && fetched_ok(&pages[kept]) {
            canonical_duplicates.insert(index, kept);
        }
    }
    // A page kept for a duplicate must not itself be superseded (a canonical chain or cycle).
    let chained: Vec<usize> = canonical_duplicates
        .iter()
        .filter(|(_, kept)| canonical_duplicates.contains_key(kept))
        .map(|(index, _)| *index)
        .collect();
    for index in chained {
        canonical_duplicates.remove(&index);
    }
    superseded.extend(
        canonical_duplicates
            .into_iter()
            .map(|(index, kept)| (index, Kept::Canonical(kept))),
    );

    for &(index, url) in &html_pages {
        if superseded.contains_key(&index) {
            continue;
        }
        let candidates = links
            .get(&index)
            .and_then(|l| l.markdown_alternate.clone())
            .into_iter()
            .chain(markdown_twin_urls(url));
        let twin = candidates.filter_map(|url| lookup(&url)).find(|&twin| {
            twin != index
                && !superseded.contains_key(&twin)
                && urls[twin]
                    .as_ref()
                    .is_some_and(|twin_url| is_markdown_response(&pages[twin], twin_url))
        });
        if let Some(twin) = twin {
            superseded.insert(twin, Kept::Twin(index));
        }
    }

    superseded
}

/// The `<link>` targets in a page that name other copies of it.
#[derive(Debug, Default, PartialEq, Eq)]
struct HeadLinks {
    canonical: Option<Url>,
    markdown_alternate: Option<Url>,
}

/// Read the first `rel="canonical"` and `rel="alternate" type="text/markdown"` links of an HTML
/// page, resolved against `base`. Tolerates minified HTML (unquoted attributes). Parsing stops at
/// `<body>`, where head links end.
fn head_links(html: &str, base: &Url) -> HeadLinks {
    const BODY_REACHED: &str = "body reached";
    let mut found = HeadLinks::default();
    let mut rewriter = lol_html::HtmlRewriter::new(
        lol_html::Settings {
            element_content_handlers: vec![
                lol_html::element!("body", |_| Err(BODY_REACHED.into())),
                lol_html::element!("link[rel][href]", |el| {
                    let rel = el.get_attribute("rel").unwrap_or_default();
                    let has_rel = |name: &str| {
                        rel.split_ascii_whitespace()
                            .any(|r| r.eq_ignore_ascii_case(name))
                    };
                    let href = el.get_attribute("href").unwrap_or_default();
                    if has_rel("canonical") && found.canonical.is_none() {
                        found.canonical = resolve_seed_url(base, &href);
                    } else if has_rel("alternate") && found.markdown_alternate.is_none() {
                        let kind = el.get_attribute("type").unwrap_or_default();
                        let kind = kind.to_ascii_lowercase();
                        if kind.starts_with("text/markdown") || kind.starts_with("text/x-markdown")
                        {
                            found.markdown_alternate = resolve_seed_url(base, &href);
                        }
                    }
                    Ok(())
                }),
            ],
            ..lol_html::Settings::new()
        },
        |_: &[u8]| {},
    );
    let parsed = rewriter
        .write(html.as_bytes())
        .and_then(|()| rewriter.end());
    match parsed {
        Err(lol_html::errors::RewritingError::ContentHandlerError(err))
            if err.to_string() == BODY_REACHED => {}
        Err(err) => tracing::debug!(url = %base, error = %err, "could not read link tags of page"),
        Ok(()) => {}
    }
    found
}

/// The URLs a site may serve the Markdown copy of HTML page `url` at: `X.md` and `X/index.md`
/// (for `X`, `X/` and `X/index.html`), without the query string.
fn markdown_twin_urls(url: &Url) -> Vec<Url> {
    let mut base = url.clone();
    base.set_query(None);
    base.set_fragment(None);
    let path = document_path(url).to_string();
    let mut urls = Vec::with_capacity(2);
    if !path.is_empty() {
        let mut twin = base.clone();
        twin.set_path(&format!("{path}.md"));
        urls.push(twin);
    }
    let mut index = base;
    index.set_path(&format!("{path}/index.md"));
    urls.push(index);
    urls
}

fn format_page_error(page: &Page, status_u16: u16) -> String {
    let reason = page.status_code.canonical_reason().unwrap_or("Error");
    match page.error_status.as_deref() {
        Some(err) if !err.trim().is_empty() => {
            if status_u16 > 0 {
                format!("HTTP {status_u16} ({reason}): {err}")
            } else {
                err.to_string()
            }
        }
        _ => format!("HTTP {status_u16} {reason}"),
    }
}

fn extract_http_validators(page: &Page) -> (Option<String>, Option<String>) {
    let Some(headers) = page.headers.as_ref() else {
        return (None, None);
    };
    let etag = headers
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let last_modified = headers
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    (etag, last_modified)
}

fn extract_page_markdown_and_title(page: &Page, url: &Url) -> (String, Option<String>) {
    let raw_content = page.get_html();
    if page.content_is_markdown || is_markdown_response(page, url) {
        return (raw_content, None);
    }

    let html_title = extract_html_title(&raw_content);
    let cleaned_html = strip_non_content_html(&raw_content);

    let fallback_cfg = TransformConfig {
        readability: false,
        return_format: ReturnFormat::Markdown,
        filter_images: false,
        clean_html: false,
        filter_svg: true,
        main_content: false,
    };

    let chosen = match extract_landmark_html(&cleaned_html) {
        LandmarkSelection::Single(landmark_html) => {
            let landmark_page = page_with_html(url, landmark_html);
            transform_content(&landmark_page, &fallback_cfg, &None, &None, &None)
        }
        LandmarkSelection::MultipleArticles => {
            // Multiple <article> elements without <main> typically represent a card/post listing
            // that may also have a page-level <h1> or intro outside the cards; convert the
            // already-stripped HTML directly so Readability neither drops the outside heading
            // nor truncates to the first <article>.
            let cleaned_page = page_with_html(url, cleaned_html);
            transform_content(&cleaned_page, &fallback_cfg, &None, &None, &None)
        }
        LandmarkSelection::None => {
            let html_h1 = extract_html_h1(&cleaned_html);
            let cleaned_page = page_with_html(url, cleaned_html);
            let readability_cfg = TransformConfig {
                readability: true,
                ..fallback_cfg
            };
            let readability_md =
                transform_content(&cleaned_page, &readability_cfg, &None, &None, &None);
            let fallback_md = transform_content(&cleaned_page, &fallback_cfg, &None, &None, &None);

            if should_prefer_fallback(
                &readability_md,
                &fallback_md,
                html_title.as_deref(),
                html_h1.as_deref(),
            ) {
                fallback_md
            } else {
                readability_md
            }
        }
    };

    (chosen, html_title)
}

fn page_with_html(url: &Url, html: String) -> Page {
    let mut page = Page::default();
    page.set_url(url.to_string());
    page.set_url_parsed(url.clone());
    page.set_html_bytes(Some(html.into_bytes()));
    page
}

/// Remove navigation, chrome, and widget elements before Readability or Markdown conversion runs.
fn strip_non_content_html(html: &str) -> String {
    const STRIP_SELECTORS: &[&str] = &[
        "nav",
        "footer",
        "script",
        "style",
        "noscript",
        "title",
        "aside",
        "dialog",
        "[role=\"complementary\"]",
        "[role=\"dialog\"]",
    ];
    let handlers = STRIP_SELECTORS
        .iter()
        .map(|sel| {
            lol_html::element!(*sel, |el| {
                el.remove();
                Ok(())
            })
        })
        .collect();
    lol_html::rewrite_str(
        html,
        lol_html::RewriteStrSettings {
            element_content_handlers: handlers,
            ..lol_html::RewriteStrSettings::new()
        },
    )
    .unwrap_or_else(|_| html.to_string())
}

enum LandmarkSelection {
    Single(String),
    MultipleArticles,
    None,
}

/// Extract the primary semantic landmark (`<main>`, `[role="main"]`, or a single `<article>`)
/// when it contains non-whitespace text.
fn extract_landmark_html(cleaned_html: &str) -> LandmarkSelection {
    for selector in ["main", "[role=\"main\"]"] {
        if let Some(found) = extract_selector_matches(cleaned_html, selector)
            .into_iter()
            .find(|fragment| has_visible_text(fragment))
        {
            return LandmarkSelection::Single(found);
        }
    }

    let mut non_empty_articles = extract_selector_matches(cleaned_html, "article")
        .into_iter()
        .filter(|fragment| has_visible_text(fragment));
    let Some(first) = non_empty_articles.next() else {
        return LandmarkSelection::None;
    };
    if non_empty_articles.next().is_some() {
        LandmarkSelection::MultipleArticles
    } else {
        LandmarkSelection::Single(first)
    }
}

fn extract_selector_matches(html: &str, selector: &str) -> Vec<String> {
    use std::cell::Cell;
    use std::rc::Rc;

    let mut suffix = 0usize;
    let (start_marker, end_marker) = loop {
        let s = format!("<!--citadel-lm-start-{suffix}-->");
        let e = format!("<!--citadel-lm-end-{suffix}-->");
        if !html.contains(&s) && !html.contains(&e) {
            break (s, e);
        }
        suffix += 1;
    };

    let depth = Rc::new(Cell::new(0usize));
    let depth_el = Rc::clone(&depth);
    let start_for_el = start_marker.clone();
    let end_for_el = end_marker.clone();

    let Ok(rewritten) = lol_html::rewrite_str(
        html,
        lol_html::RewriteStrSettings {
            element_content_handlers: vec![lol_html::element!(selector, move |el| {
                if !el.can_have_content() {
                    return Ok(());
                }
                let d = depth_el.get();
                depth_el.set(d + 1);
                if d == 0 {
                    el.before(&start_for_el, lol_html::html_content::ContentType::Html);
                }
                let depth_end = Rc::clone(&depth_el);
                let end_marker_inner = end_for_el.clone();
                el.on_end_tag(
                    Box::new(move |end: &mut lol_html::html_content::EndTag<'_>| {
                        let next = depth_end.get().saturating_sub(1);
                        depth_end.set(next);
                        if d == 0 && next == 0 {
                            end.after(&end_marker_inner, lol_html::html_content::ContentType::Html);
                        }
                        Ok(())
                    })
                        as Box<
                            dyn FnOnce(
                                &mut lol_html::html_content::EndTag<'_>,
                            ) -> std::result::Result<
                                (),
                                Box<dyn std::error::Error + Send + Sync>,
                            >,
                        >,
                )?;
                Ok(())
            })],
            ..lol_html::RewriteStrSettings::new()
        },
    ) else {
        return Vec::new();
    };

    let mut matches = Vec::new();
    let mut rest = rewritten.as_str();
    while let Some(start_idx) = rest.find(&start_marker) {
        let after_start = &rest[start_idx + start_marker.len()..];
        if let Some(end_idx) = after_start.find(&end_marker) {
            matches.push(after_start[..end_idx].to_string());
            rest = &after_start[end_idx + end_marker.len()..];
        } else {
            matches.push(after_start.to_string());
            break;
        }
    }
    matches
}

fn has_visible_text(html: &str) -> bool {
    let mut has_text = false;
    let _ = lol_html::rewrite_str(
        html,
        lol_html::RewriteStrSettings {
            element_content_handlers: vec![lol_html::text!("*", |t| {
                if !t.as_str().trim().is_empty() {
                    has_text = true;
                }
                Ok(())
            })],
            ..lol_html::RewriteStrSettings::new()
        },
    );
    has_text
}

fn extract_html_h1(html: &str) -> Option<String> {
    extract_selector_matches(html, "h1")
        .into_iter()
        .find_map(|h1_html| {
            let mut text = String::new();
            let _ = lol_html::rewrite_str(
                &h1_html,
                lol_html::RewriteStrSettings {
                    element_content_handlers: vec![lol_html::text!("*", |t| {
                        text.push_str(t.as_str());
                        Ok(())
                    })],
                    ..lol_html::RewriteStrSettings::new()
                },
            );
            let decoded = decode_basic_html_entities(&text);
            let collapsed = decoded
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .trim_end_matches(['¶', '\u{200B}'])
                .trim()
                .to_string();
            if collapsed.is_empty() {
                None
            } else {
                Some(collapsed)
            }
        })
}

fn normalize_heading_text(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['¶', '\u{200B}'])
        .trim()
        .to_ascii_lowercase()
}

fn markdown_heading_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = &trimmed[hashes..];
    if !after.starts_with(' ') {
        return None;
    }
    Some(after.trim())
}

fn title_matches_heading(norm_title: &str, norm_heading: &str) -> bool {
    if norm_title == norm_heading {
        return true;
    }
    norm_title
        .split(['|', '—', '–', ':'])
        .flat_map(|part| part.split(" - "))
        .map(str::trim)
        .filter(|seg| seg.len() >= 3)
        .any(|seg| seg == norm_heading)
}

fn has_matching_heading(md: &str, html_title: Option<&str>, html_h1: Option<&str>) -> bool {
    let norm_h1 = html_h1
        .map(normalize_heading_text)
        .filter(|s| !s.is_empty());
    let norm_title = html_title
        .map(normalize_heading_text)
        .filter(|s| !s.is_empty());
    if norm_h1.is_none() && norm_title.is_none() {
        return false;
    }

    md.lines().filter_map(markdown_heading_text).any(|heading| {
        let norm_h = normalize_heading_text(heading);
        if norm_h.is_empty() {
            return false;
        }
        if let Some(h1) = norm_h1.as_deref() {
            return norm_h == h1 || norm_h.contains(h1);
        }
        if let Some(title) = norm_title.as_deref() {
            return title_matches_heading(title, &norm_h);
        }
        false
    })
}

fn should_prefer_fallback(
    readability_md: &str,
    fallback_md: &str,
    html_title: Option<&str>,
    html_h1: Option<&str>,
) -> bool {
    if fallback_md.trim().is_empty() {
        return false;
    }
    if readability_md.trim().is_empty() {
        return true;
    }
    let read_words = count_words(readability_md);
    let fall_words = count_words(fallback_md);
    if !readability_md.contains("# ") && fallback_md.contains("# ") {
        return true;
    }
    if !has_matching_heading(readability_md, html_title, html_h1)
        && has_matching_heading(fallback_md, html_title, html_h1)
    {
        return true;
    }
    fall_words > 10 && read_words.saturating_mul(4) < fall_words
}

fn is_markdown_response(page: &Page, url: &Url) -> bool {
    if let Some(headers) = page.headers.as_ref() {
        if let Some(ct) = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase)
        {
            if ct.contains("text/markdown") || ct.contains("text/x-markdown") {
                return true;
            }
        }
    }
    let path_lower = url.path().to_ascii_lowercase();
    path_lower.ends_with(".md") || path_lower.ends_with(".markdown")
}

/// Whether a Sync stopped because it used up its page budget, so pages it did not visit must be
/// kept. Any one condition is enough:
/// 1. the first crawl delivered or lost `max_pages` pages: spider's limit ran out (a single
///    crawler cannot fetch a page twice);
/// 2. the distinct storable URLs among the collected pages reach `max_pages`;
/// 3. the results reach `max_pages`: stored, failed and merged copies (`Retired`) all count.
fn is_cut_off(
    first_crawl_delivered_or_lost: usize,
    collected_storable_urls: usize,
    outcomes: usize,
    max_pages: usize,
) -> bool {
    first_crawl_delivered_or_lost >= max_pages
        || collected_storable_urls >= max_pages
        || outcomes >= max_pages
}

/// A cut-off Sync warns about redirects only when at least this many pages redirected outside
/// the Source...
const MIN_REDIRECTS_FOR_WARNING: usize = 5;
/// ...and they are more than this share (in percent) of the pages received.
const REDIRECT_SHARE_PERCENT: usize = 10;
/// How many redirected URLs the warning names.
const MAX_REDIRECT_EXAMPLES: usize = 3;

/// The warning for a cut-off Sync that spent much of its page budget on pages that redirected
/// outside the Source, or `None`.
fn budget_warning(
    counts: &PageCounts,
    redirect_examples: &[String],
    max_pages: usize,
) -> Option<String> {
    let redirected = counts.redirected_out_of_scope;
    let received = counts.pages_received;
    if !counts.cut_off
        || redirected < MIN_REDIRECTS_FOR_WARNING
        || redirected * 100 <= received * REDIRECT_SHARE_PERCENT
    {
        return None;
    }
    let examples = redirect_examples.join(", ");
    Some(format!(
        "{redirected} of {received} pages received redirected outside this Source and were not \
         stored, for example {examples}. They count against max_pages ({max_pages}), so other \
         pages were not reached. Raise max_pages, or exclude that section if you do not need it."
    ))
}

/// Results that stand for a page of their own (stored or failed), leaving out superseded copies.
fn result_count(outcomes: &[PageIngestOutcome]) -> usize {
    outcomes
        .iter()
        .filter(|o| !matches!(o, PageIngestOutcome::Retired { .. }))
        .count()
}

fn fallback_title_from_url(url: &Url) -> String {
    url.path_segments()
        .and_then(|segments| segments.rev().find(|s| !s.is_empty()))
        .map(|seg| seg.replace(['-', '_'], " "))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| url.as_str().to_string())
}

fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start_tag = lower.find("<title")?;
    let after_open = &lower[start_tag + 6..];
    let gt_rel = after_open.find('>')?;
    let content_start = start_tag + 6 + gt_rel + 1;
    let end_rel = lower[content_start..].find("</title>")?;
    let raw = html[content_start..content_start + end_rel].trim();
    if raw.is_empty() {
        return None;
    }
    let decoded = decode_basic_html_entities(raw);
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

fn decode_basic_html_entities(raw: &str) -> String {
    raw.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    use super::{Collected, CollectedPages, LaterCrawl, PageCounts};
    use crate::error::Result;
    use crate::ingest::persist::{MissingPagePolicy, PageIngestOutcome};
    use crate::ingest::web::{Page, Url, WebScope};

    fn scope() -> WebScope {
        let origin = Url::parse("https://docs.example/docs/").expect("origin");
        WebScope::new(&origin, &[], &[]).expect("scope")
    }

    fn page(url: &str) -> Page {
        let mut page = Page::default();
        page.set_url(url.to_string());
        page.status_code = StatusCode::OK;
        page
    }

    /// A page served with `content_type` and `body`.
    fn typed_page(url: &str, content_type: &str, body: &str) -> Page {
        let mut page = page(url);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            content_type.parse().expect("content type"),
        );
        page.headers = Some(headers);
        page.set_html_bytes(Some(body.as_bytes().to_vec()));
        page
    }

    /// A page whose request was redirected outside the Source.
    fn redirected(url: &str) -> Page {
        let mut page = page(url);
        page.final_redirect_destination = Some("https://elsewhere.example/x".to_string());
        page
    }

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("url")
    }

    /// Each result as (kind, URI), in order.
    fn results(collected: &Collected) -> Vec<(&'static str, &str)> {
        collected
            .outcomes
            .iter()
            .map(|outcome| match outcome {
                PageIngestOutcome::Ready { uri, .. } => ("ready", uri.as_str()),
                PageIngestOutcome::Failed { uri, .. } => ("failed", uri.as_str()),
                PageIngestOutcome::Retired { uri } => ("retired", uri.as_str()),
            })
            .collect()
    }

    /// Collect one first crawl and finish.
    fn first_crawl(
        pages: Vec<Page>,
        lost: usize,
        max_pages: u32,
        prune: bool,
    ) -> (Result<Collected>, PageCounts) {
        let scope = scope();
        let mut collected = CollectedPages::new(&scope, max_pages);
        collected.add_first_crawl(pages, lost);
        collected.finish(prune)
    }

    #[test]
    fn counts_each_dropped_page_under_its_first_reason() {
        let scope = scope();
        let mut redirected = page("https://docs.example/docs/moved");
        redirected.final_redirect_destination = Some("https://elsewhere.example/x".to_string());
        let mut binary = page("https://docs.example/docs/file");
        binary.binary_file = true;
        let mut missing = page("https://docs.example/docs/missing");
        missing.status_code = StatusCode::NOT_FOUND;

        let mut collected = CollectedPages::new(&scope, 100);
        collected.add_first_crawl(
            vec![
                page("ftp://docs.example/docs/unparsable"),
                redirected,
                page("https://docs.example/blog/outside"),
                page("https://docs.example/docs/stored"),
                page("https://docs.example/docs/stored"),
                binary,
                missing,
            ],
            0,
        );
        let (result, counts) = collected.finish(false);

        assert_eq!(result.expect("one page is stored").outcomes.len(), 2);
        assert_eq!(
            counts,
            PageCounts {
                pages_received: 7,
                unparsable_url: 1,
                redirected_out_of_scope: 1,
                url_not_storable: 1,
                duplicate_result: 1,
                binary_file: 1,
                results_stored: 1,
                results_failed: 1,
                ..PageCounts::default()
            }
        );
    }

    #[test]
    fn later_crawl_drops_duplicates_and_unparsable_urls_at_intake() {
        let scope = scope();
        let mut collected = CollectedPages::new(&scope, 100);
        collected.add_first_crawl(
            vec![
                page("https://docs.example/docs/"),
                page("https://docs.example/docs/a"),
            ],
            0,
        );
        let seed = url("https://docs.example/docs/h1");
        assert_eq!(collected.next_later_crawl(&seed), LaterCrawl::Crawl(98));
        collected.add_later_crawl(
            &seed,
            vec![
                page("https://docs.example/docs/h1"),
                page("https://docs.example/docs/a"),
                page("https://docs.example/docs/a/"),
                page("ftp://docs.example/docs/x"),
            ],
            1,
        );
        assert_eq!(collected.next_later_crawl(&seed), LaterCrawl::Skip);

        let (result, counts) = collected.finish(false);
        assert_eq!(result.expect("pages stored").outcomes.len(), 3);
        assert_eq!(
            counts,
            PageCounts {
                pages_received: 6,
                lost_by_listener: 1,
                later_crawls_started: 1,
                already_collected: 2,
                unparsable_url: 1,
                results_stored: 3,
                ..PageCounts::default()
            }
        );
    }

    #[test]
    fn next_later_crawl_skips_a_collected_seed_and_stops_when_the_budget_is_spent() {
        let scope = scope();
        let mut collected = CollectedPages::new(&scope, 2);
        collected.add_first_crawl(vec![page("https://docs.example/docs/a")], 0);

        assert_eq!(
            collected.next_later_crawl(&url("https://docs.example/docs/a/")),
            LaterCrawl::Skip,
            "same dedup key as a collected page"
        );
        let seed = url("https://docs.example/docs/b");
        assert_eq!(collected.next_later_crawl(&seed), LaterCrawl::Crawl(1));
        collected.add_later_crawl(&seed, vec![page("https://docs.example/docs/b")], 0);
        assert_eq!(
            collected.next_later_crawl(&url("https://docs.example/docs/c")),
            LaterCrawl::Stop
        );

        let (_, counts) = collected.finish(false);
        assert_eq!(counts.later_crawls_started, 1);
    }

    #[test]
    fn later_crawl_budget_counts_a_markdown_twin_pair_once() {
        let scope = scope();
        let mut collected = CollectedPages::new(&scope, 6);
        collected.add_first_crawl(
            vec![
                typed_page("https://docs.example/docs/guide", "text/html", ""),
                typed_page("https://docs.example/docs/guide.md", "text/markdown", ""),
                typed_page("https://docs.example/docs/orphan.md", "text/markdown", ""),
                typed_page("https://docs.example/docs/", "text/html", ""),
                typed_page("https://docs.example/docs/index.md", "text/markdown", ""),
            ],
            0,
        );
        // Five storable URLs, three documents: guide + guide.md, orphan.md, docs/ + index.md.
        assert_eq!(
            collected.next_later_crawl(&url("https://docs.example/docs/next")),
            LaterCrawl::Crawl(3)
        );
    }

    #[test]
    fn every_received_page_lands_in_exactly_one_count() {
        let html = r#"<html><head><link rel="canonical" href="/docs/guide"></head><body><h1>Guide</h1></body></html>"#;
        let mut binary = page("https://docs.example/docs/file");
        binary.binary_file = true;
        let mut missing = page("https://docs.example/docs/missing");
        missing.status_code = StatusCode::NOT_FOUND;

        let scope = scope();
        let mut collected = CollectedPages::new(&scope, 100);
        collected.add_first_crawl(
            vec![
                page("ftp://docs.example/docs/unparsable"),
                redirected("https://docs.example/docs/moved"),
                page("https://docs.example/blog/outside"),
                page("https://docs.example/docs/a"),
                page("https://docs.example/docs/a"),
                page("https://docs.example/docs/a/"),
                binary,
                missing,
                typed_page("https://docs.example/docs/guide", "text/html", html),
                typed_page(
                    "https://docs.example/docs/guide.md",
                    "text/markdown",
                    "# Guide\n\nMd.\n",
                ),
                typed_page(
                    "https://docs.example/docs/guide?version=2",
                    "text/html",
                    html,
                ),
            ],
            0,
        );
        // Six documents so far: the redirected page's requested URL counts against the budget,
        // and guide.md pairs with guide.
        let seed = url("https://docs.example/docs/h1");
        assert_eq!(collected.next_later_crawl(&seed), LaterCrawl::Crawl(94));
        collected.add_later_crawl(
            &seed,
            vec![
                page("https://docs.example/docs/h1"),
                page("https://docs.example/docs/a"),
                page("ftp://docs.example/docs/y"),
            ],
            0,
        );

        let (_, counts) = collected.finish(false);
        assert_eq!(
            counts,
            PageCounts {
                pages_received: 14,
                later_crawls_started: 1,
                results_stored: 3,
                results_failed: 1,
                unparsable_url: 2,
                already_collected: 1,
                redirected_out_of_scope: 1,
                url_not_storable: 1,
                duplicate_result: 1,
                binary_file: 1,
                markdown_twin: 1,
                canonical_duplicate: 1,
                url_alias: 1,
                ..PageCounts::default()
            }
        );
        let landed = counts.results_stored
            + counts.results_failed
            + counts.unparsable_url
            + counts.already_collected
            + counts.redirected_out_of_scope
            + counts.url_not_storable
            + counts.duplicate_result
            + counts.binary_file
            + counts.markdown_twin
            + counts.canonical_duplicate
            + counts.url_alias;
        assert_eq!(landed, 14, "14 pages were handed over");
    }

    #[test]
    fn reads_link_tags_from_minified_html() {
        let base = url("https://docs.example/docs/guide?version=2");
        let links = super::head_links(
            "<html><head><link rel=canonical href=/docs/guide><link data-rh=true \
             rel=alternate type=text/markdown href=guide.md></head><body></body></html>",
            &base,
        );
        assert_eq!(
            links.canonical.map(|u| u.to_string()).as_deref(),
            Some("https://docs.example/docs/guide")
        );
        assert_eq!(
            links.markdown_alternate.map(|u| u.to_string()).as_deref(),
            Some("https://docs.example/docs/guide.md")
        );
    }

    #[test]
    fn canonical_to_another_path_does_not_collapse_pages() {
        let html = r#"<html><head><link rel="canonical" href="https://docs.example/docs/"></head><body><h1>Page</h1><p>Text.</p></body></html>"#;
        let (result, counts) = first_crawl(
            vec![
                typed_page("https://docs.example/docs/", "text/html", html),
                typed_page("https://docs.example/docs/a", "text/html", html),
                typed_page("https://docs.example/docs/b", "text/html", html),
            ],
            0,
            100,
            false,
        );
        assert_eq!(result.expect("stored").outcomes.len(), 3);
        assert_eq!(counts.results_stored, 3);
        assert_eq!(counts.canonical_duplicate, 0);
    }

    #[test]
    fn twins_and_canonical_duplicates_are_counted_and_retired() {
        let html = r#"<html><head><link rel="canonical" href="/docs/guide"></head><body><h1>Guide</h1><p>Html text.</p></body></html>"#;
        let (result, counts) = first_crawl(
            vec![
                typed_page(
                    "https://docs.example/docs/guide.md",
                    "text/markdown",
                    "# Guide\n\nMd text.\n",
                ),
                typed_page("https://docs.example/docs/guide", "text/html", html),
                typed_page(
                    "https://docs.example/docs/guide?version=2",
                    "text/html",
                    html,
                ),
                typed_page(
                    "https://docs.example/docs/index.md",
                    "text/markdown",
                    "# Home\n\nMd home.\n",
                ),
                typed_page(
                    "https://docs.example/docs/",
                    "text/html",
                    "<html><body><h1>Home</h1></body></html>",
                ),
            ],
            0,
            100,
            false,
        );
        let collected = result.expect("stored");

        assert_eq!(
            results(&collected),
            vec![
                ("retired", "https://docs.example/docs/guide.md"),
                ("ready", "https://docs.example/docs/guide"),
                ("retired", "https://docs.example/docs/guide?version=2"),
                ("retired", "https://docs.example/docs/index.md"),
                ("ready", "https://docs.example/docs/"),
            ]
        );
        assert_eq!(counts.results_stored, 2);
        assert_eq!(counts.markdown_twin, 2);
        assert_eq!(counts.canonical_duplicate, 1);
        for outcome in &collected.outcomes {
            if let PageIngestOutcome::Ready { uri, doc, .. } = outcome {
                assert!(doc.content_md.contains("Md "), "{uri}: {}", doc.content_md);
            }
        }
    }

    #[test]
    fn a_failed_first_copy_of_a_page_claims_no_twin() {
        let mut failed = typed_page("https://docs.example/docs/guide", "text/html", "");
        failed.status_code = StatusCode::NOT_FOUND;
        let (result, counts) = first_crawl(
            vec![
                failed,
                typed_page(
                    "https://docs.example/docs/guide",
                    "text/html",
                    "<h1>Guide</h1>",
                ),
                typed_page(
                    "https://docs.example/docs/guide.md",
                    "text/markdown",
                    "# Guide\n\nMd.\n",
                ),
            ],
            0,
            100,
            false,
        );
        assert_eq!(
            results(&result.expect("stored")),
            vec![
                ("failed", "https://docs.example/docs/guide"),
                ("ready", "https://docs.example/docs/guide.md"),
            ],
            "the .md is stored on its own"
        );
        assert_eq!(counts.markdown_twin, 0, "{counts:?}");
    }

    #[test]
    fn canonical_names_the_exact_trailing_slash_variant() {
        let html = r#"<html><head><link rel="canonical" href="/docs/p/"></head><body><h1>P</h1></body></html>"#;
        let (result, counts) = first_crawl(
            vec![
                typed_page("https://docs.example/docs/p", "text/html", html),
                typed_page("https://docs.example/docs/p/", "text/html", html),
            ],
            0,
            100,
            false,
        );
        assert_eq!(
            results(&result.expect("stored")),
            vec![
                ("retired", "https://docs.example/docs/p"),
                ("ready", "https://docs.example/docs/p/"),
            ]
        );
        assert_eq!(counts.canonical_duplicate, 1);
    }

    #[test]
    fn first_crawl_at_limit_is_cut_off() {
        // The dlthub Sync of 2026-11-23: spider's limit ran out, though pages were lost.
        let (result, counts) = first_crawl(
            vec![
                page("https://docs.example/docs/a"),
                page("https://docs.example/docs/b"),
            ],
            1,
            3,
            false,
        );
        assert!(counts.cut_off, "{counts:?}");
        assert_eq!(
            result.expect("stored").missing_policy,
            MissingPagePolicy::Preserve
        );
    }

    #[test]
    fn collected_urls_at_limit_is_cut_off() {
        // Pages that redirect out of the Source get no result but still use up the budget.
        let scope = scope();
        let mut collected = CollectedPages::new(&scope, 3);
        collected.add_first_crawl(vec![page("https://docs.example/docs/a")], 0);
        let seed = url("https://docs.example/docs/r1");
        assert_eq!(collected.next_later_crawl(&seed), LaterCrawl::Crawl(2));
        collected.add_later_crawl(
            &seed,
            vec![
                redirected("https://docs.example/docs/r1"),
                redirected("https://docs.example/docs/r2"),
            ],
            0,
        );
        let (result, counts) = collected.finish(false);
        assert_eq!(counts.results_stored, 1);
        assert!(counts.cut_off, "{counts:?}");
        assert_eq!(
            result.expect("stored").missing_policy,
            MissingPagePolicy::Preserve
        );
    }

    #[test]
    fn results_at_limit_count_retired_copies() {
        // `/docs/p/` merges into `/docs/p`: two storable URLs, but three results with the merged copy.
        let html = r#"<html><head><link rel="canonical" href="/docs/p"></head><body><h1>P</h1></body></html>"#;
        let scope = scope();
        let mut collected = CollectedPages::new(&scope, 3);
        collected.add_first_crawl(
            vec![
                typed_page("https://docs.example/docs/p", "text/html", html),
                typed_page("https://docs.example/docs/p/", "text/html", html),
            ],
            0,
        );
        let seed = url("https://docs.example/docs/q");
        assert_eq!(collected.next_later_crawl(&seed), LaterCrawl::Crawl(2));
        collected.add_later_crawl(&seed, vec![page("https://docs.example/docs/q")], 0);
        let (_, counts) = collected.finish(false);
        assert_eq!(counts.canonical_duplicate, 1, "{counts:?}");
        assert!(counts.cut_off, "{counts:?}");
    }

    #[test]
    fn under_budget_is_complete() {
        let (result, counts) = first_crawl(
            vec![
                page("https://docs.example/docs/a"),
                page("https://docs.example/docs/b"),
            ],
            0,
            10,
            false,
        );
        assert!(!counts.cut_off, "{counts:?}");
        assert_eq!(
            result.expect("stored").missing_policy,
            MissingPagePolicy::MarkStale
        );
    }

    #[test]
    fn prune_wins_over_the_cut_off_policy() {
        let pages = || {
            vec![
                page("https://docs.example/docs/a"),
                page("https://docs.example/docs/b"),
            ]
        };
        let policy = |max_pages, prune| {
            first_crawl(pages(), 0, max_pages, prune)
                .0
                .expect("stored")
                .missing_policy
        };
        assert_eq!(policy(2, true), MissingPagePolicy::Prune, "cut off");
        assert_eq!(policy(10, true), MissingPagePolicy::Prune, "complete");
        assert_eq!(policy(2, false), MissingPagePolicy::Preserve);
        assert_eq!(policy(10, false), MissingPagePolicy::MarkStale);
    }

    /// Finish a first crawl of `received` pages, `redirected` of them redirected out of the Source
    /// (`/docs/r1`, `/docs/r2`, ...), and return its warnings.
    fn redirect_warnings(redirected_pages: usize, received: usize, max_pages: u32) -> Vec<String> {
        let mut pages: Vec<Page> = (1..=redirected_pages)
            .map(|i| redirected(&format!("https://docs.example/docs/r{i}")))
            .collect();
        pages.extend(
            (1..=received - redirected_pages)
                .map(|i| page(&format!("https://docs.example/docs/p{i}"))),
        );
        first_crawl(pages, 0, max_pages, false)
            .0
            .expect("stored")
            .warnings
    }

    #[test]
    fn warns_when_budget_went_to_redirects() {
        let warnings = redirect_warnings(5, 20, 20);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = &warnings[0];
        assert!(warning.starts_with("5 of 20 pages received redirected outside this Source"));
        for example in ["/docs/r1", "/docs/r2", "/docs/r3"] {
            assert!(warning.contains(example), "{warning}");
        }
        assert!(!warning.contains("/docs/r4"), "names three: {warning}");
        assert!(warning.contains("max_pages (20)"), "{warning}");
    }

    #[test]
    fn no_warning_below_five_redirects() {
        assert_eq!(redirect_warnings(4, 20, 20), Vec::<String>::new());
    }

    #[test]
    fn no_warning_at_ten_percent() {
        assert_eq!(redirect_warnings(5, 50, 50), Vec::<String>::new());
    }

    #[test]
    fn warns_above_ten_percent() {
        assert_eq!(redirect_warnings(5, 49, 49).len(), 1);
    }

    #[test]
    fn no_warning_when_not_cut_off() {
        assert_eq!(redirect_warnings(5, 20, 100), Vec::<String>::new());
    }

    #[test]
    fn nothing_reachable_fails_with_the_first_error() {
        let mut refused = page("https://docs.example/docs/");
        refused.status_code = StatusCode::INTERNAL_SERVER_ERROR;
        refused.error_status = Some("connection refused".to_string());
        let (result, counts) = first_crawl(vec![refused], 0, 100, false);
        let Err(err) = result else {
            panic!("a Sync with only transport failures fails");
        };
        assert_eq!(
            err.to_string(),
            "HTTP 500 (Internal Server Error): connection refused"
        );
        assert_eq!(counts.results_failed, 1, "the counts survive the error");
    }

    #[test]
    fn a_sync_with_no_pages_fails_naming_the_origin() {
        let (result, _) = first_crawl(Vec::new(), 0, 100, false);
        let Err(err) = result else {
            panic!("a Sync with no pages fails");
        };
        assert_eq!(
            err.to_string(),
            "no reachable pages crawled from 'https://docs.example/docs/'"
        );
    }

    #[test]
    fn an_http_error_page_alone_is_still_a_result() {
        let mut missing = page("https://docs.example/docs/");
        missing.status_code = StatusCode::NOT_FOUND;
        let (result, counts) = first_crawl(vec![missing], 0, 100, false);
        assert_eq!(
            results(&result.expect("a 404 is not a transport failure")),
            vec![("failed", "https://docs.example/docs/")]
        );
        assert_eq!(counts.results_failed, 1);
    }

    /// Issue #5: one Page per document across alias URLs. Redirects are built by hand: spider
    /// refuses to follow a redirect to the loopback fixture server.
    mod url_aliases {
        use reqwest::StatusCode;

        use super::super::{build_page_outcomes, document_key, PageCounts};
        use super::{scope, typed_page};
        use crate::ingest::persist::PageIngestOutcome;
        use crate::ingest::web::{Page, Url};

        #[test]
        fn document_key_folds_trailing_slash_and_index() {
            let key = |raw: &str| document_key(&Url::parse(raw).expect("url"));
            let a = "https://docs.example/docs/a";
            assert_eq!(key("https://docs.example/docs/a/"), a);
            assert_eq!(key("https://docs.example/docs/a/index"), a);
            assert_eq!(key("https://docs.example/docs/a/index.html"), a);
            assert_eq!(key("https://docs.example/docs/a/index.htm"), a);
            assert_eq!(
                key("https://docs.example/index.html"),
                "https://docs.example/"
            );
            assert_eq!(key("https://docs.example/"), "https://docs.example/");
            assert_eq!(
                key("https://docs.example/docs/a/index.md"),
                "https://docs.example/docs/a/index.md"
            );
            assert_eq!(
                key("https://docs.example/docs/reindex"),
                "https://docs.example/docs/reindex"
            );
            assert_eq!(
                key("https://docs.example/docs/a/?v=2"),
                "https://docs.example/docs/a?v=2"
            );
        }

        fn redirected_to(url: &str, to: &str, body: &str) -> Page {
            let mut page = typed_page(url, "text/html", body);
            page.final_redirect_destination = Some(to.to_string());
            page
        }

        /// (stored URIs, retired URIs, accounting) of a set of pages.
        fn outcome_uris(pages: Vec<Page>) -> (Vec<String>, Vec<String>, PageCounts) {
            let mut acc = PageCounts::default();
            let (outcomes, _, _) = build_page_outcomes(pages, &scope(), &mut acc, &mut Vec::new());
            let mut stored = Vec::new();
            let mut retired = Vec::new();
            for outcome in outcomes {
                match outcome {
                    PageIngestOutcome::Ready { uri, .. }
                    | PageIngestOutcome::Failed { uri, .. } => stored.push(uri),
                    PageIngestOutcome::Retired { uri } => retired.push(uri),
                }
            }
            stored.sort();
            retired.sort();
            (stored, retired, acc)
        }

        const BODY: &str = "<html><body><h1>P</h1><p>Text.</p></body></html>";

        #[test]
        fn shortest_variant_wins_whatever_the_arrival_order() {
            for urls in [
                [
                    "https://docs.example/docs/p/",
                    "https://docs.example/docs/p",
                    "https://docs.example/docs/p/index.html",
                ],
                [
                    "https://docs.example/docs/p/index.html",
                    "https://docs.example/docs/p",
                    "https://docs.example/docs/p/",
                ],
            ] {
                let pages = urls
                    .iter()
                    .map(|u| typed_page(u, "text/html", BODY))
                    .collect();
                let (stored, retired, acc) = outcome_uris(pages);
                assert_eq!(stored, vec!["https://docs.example/docs/p"]);
                assert_eq!(
                    retired,
                    vec![
                        "https://docs.example/docs/p/",
                        "https://docs.example/docs/p/index.html"
                    ]
                );
                assert_eq!(acc.url_alias, 2, "{acc:?}");
                assert_eq!(acc.results_stored + acc.url_alias, 3);
            }
        }

        #[test]
        fn cleanly_fetched_variant_beats_a_failed_one() {
            let mut failed = typed_page("https://docs.example/docs/q", "text/html", BODY);
            failed.status_code = StatusCode::NOT_FOUND;
            let (stored, retired, acc) = outcome_uris(vec![
                failed,
                typed_page("https://docs.example/docs/q/", "text/html", BODY),
            ]);
            assert_eq!(stored, vec!["https://docs.example/docs/q/"]);
            assert_eq!(retired, vec!["https://docs.example/docs/q"]);
            assert_eq!(acc.results_failed, 0, "{acc:?}");
        }

        #[test]
        fn canonical_target_beats_the_shortest_variant() {
            let body = r#"<html><head><link rel="canonical" href="/docs/r/index.html"></head><body><h1>R</h1></body></html>"#;
            let (stored, retired, _) = outcome_uris(vec![
                typed_page("https://docs.example/docs/r", "text/html", body),
                typed_page("https://docs.example/docs/r/index.html", "text/html", body),
            ]);
            assert_eq!(stored, vec!["https://docs.example/docs/r/index.html"]);
            assert_eq!(retired, vec!["https://docs.example/docs/r"]);
        }

        #[test]
        fn redirected_page_is_stored_at_its_destination() {
            let (stored, retired, acc) = outcome_uris(vec![
                redirected_to(
                    "https://docs.example/docs/old",
                    "https://docs.example/docs/new",
                    BODY,
                ),
                redirected_to(
                    "https://docs.example/docs/a/",
                    "https://docs.example/docs/a",
                    BODY,
                ),
            ]);
            assert_eq!(
                stored,
                vec![
                    "https://docs.example/docs/a",
                    "https://docs.example/docs/new"
                ]
            );
            assert_eq!(
                retired,
                vec![
                    "https://docs.example/docs/a/",
                    "https://docs.example/docs/old"
                ]
            );
            assert_eq!(acc.results_stored, 2);
        }

        #[test]
        fn redirect_source_is_retired_when_its_destination_was_also_fetched() {
            for pages in [
                vec![
                    redirected_to(
                        "https://docs.example/docs/old",
                        "https://docs.example/docs/new",
                        BODY,
                    ),
                    typed_page("https://docs.example/docs/new", "text/html", BODY),
                ],
                vec![
                    typed_page("https://docs.example/docs/new", "text/html", BODY),
                    redirected_to(
                        "https://docs.example/docs/old",
                        "https://docs.example/docs/new",
                        BODY,
                    ),
                ],
            ] {
                let (stored, retired, acc) = outcome_uris(pages);
                assert_eq!(stored, vec!["https://docs.example/docs/new"]);
                assert_eq!(retired, vec!["https://docs.example/docs/old"]);
                assert_eq!(acc.results_stored + acc.duplicate_result, 2, "{acc:?}");
            }
        }

        #[test]
        fn final_url_equal_to_the_request_is_no_redirect() {
            let (stored, retired, _) = outcome_uris(vec![redirected_to(
                "https://docs.example/docs/same",
                "https://docs.example/docs/same#top",
                BODY,
            )]);
            assert_eq!(stored, vec!["https://docs.example/docs/same"]);
            assert!(retired.is_empty(), "{retired:?}");
        }

        #[test]
        fn redirect_to_a_failed_page_marks_the_source_error_not_stale() {
            let mut down = redirected_to(
                "https://docs.example/docs/guide",
                "https://docs.example/docs/maintenance",
                BODY,
            );
            down.status_code = StatusCode::SERVICE_UNAVAILABLE;
            let mut acc = PageCounts::default();
            let (outcomes, _, _) =
                build_page_outcomes(vec![down], &scope(), &mut acc, &mut Vec::new());
            let failed: Vec<(&str, bool)> = outcomes
                .iter()
                .map(|o| match o {
                    PageIngestOutcome::Failed {
                        uri,
                        insert_if_missing,
                        ..
                    } => (uri.as_str(), *insert_if_missing),
                    _ => panic!("only failures expected"),
                })
                .collect();
            assert_eq!(
                failed,
                vec![
                    ("https://docs.example/docs/guide", false),
                    ("https://docs.example/docs/maintenance", true),
                ]
            );
            assert_eq!(acc.redirect_source_results, 1);
        }

        #[test]
        fn redirect_extras_are_left_out_of_the_cut_off_count() {
            let pages = (0..3)
                .map(|i| {
                    redirected_to(
                        &format!("https://docs.example/docs/p{i}"),
                        &format!("https://docs.example/docs/p{i}/"),
                        BODY,
                    )
                })
                .collect();
            let mut acc = PageCounts::default();
            let (outcomes, _, _) = build_page_outcomes(pages, &scope(), &mut acc, &mut Vec::new());
            assert_eq!(outcomes.len(), 6, "a Ready and a Retired per page");
            assert_eq!(outcomes.len() - acc.redirect_source_results, 3);
        }

        #[test]
        fn markdown_twin_of_an_index_html_copy_is_paired() {
            let body = r#"<html><head><link rel="canonical" href="/docs/x/index.html"></head><body><h1>X</h1><p>Html.</p></body></html>"#;
            let mut acc = PageCounts::default();
            let (outcomes, _, _) = build_page_outcomes(
                vec![
                    typed_page("https://docs.example/docs/x", "text/html", body),
                    typed_page("https://docs.example/docs/x/index.html", "text/html", body),
                    typed_page(
                        "https://docs.example/docs/x.md",
                        "text/markdown",
                        "# X\n\nMd.\n",
                    ),
                ],
                &scope(),
                &mut acc,
                &mut Vec::new(),
            );
            assert_eq!(acc.markdown_twin, 1, "{acc:?}");
            let stored: Vec<&str> = outcomes
                .iter()
                .filter_map(|o| match o {
                    PageIngestOutcome::Ready { uri, .. } => Some(uri.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(stored, vec!["https://docs.example/docs/x/index.html"]);
        }

        #[test]
        fn later_crawls_keep_alias_pages_but_skip_alias_seeds() {
            let mut visited = super::super::Visited::default();
            let key = |raw: &str| document_key(&Url::parse(raw).expect("url"));
            assert_eq!(
                visited.collect(&typed_page(
                    "https://docs.example/docs/p",
                    "text/html",
                    BODY
                )),
                Some(true)
            );
            assert_eq!(
                visited.collect(&typed_page(
                    "https://docs.example/docs/p/index.html",
                    "text/html",
                    BODY
                )),
                Some(true),
                "an alias page is kept so its old row is retired"
            );
            assert_eq!(
                visited.collect(&typed_page(
                    "https://docs.example/docs/p",
                    "text/html",
                    BODY
                )),
                Some(false)
            );
            assert_eq!(
                visited.collect(&redirected_to(
                    "https://docs.example/docs/old",
                    "https://docs.example/docs/new",
                    BODY
                )),
                Some(true)
            );
            for seed in [
                "https://docs.example/docs/p/",
                "https://docs.example/docs/new",
            ] {
                assert!(visited.documents.contains(&key(seed)), "{seed}");
            }
        }
    }

    mod extraction_tests {
        use super::super::extract_page_markdown_and_title;
        use super::{typed_page, url};

        fn extract(html: &str) -> String {
            let page = typed_page("https://docs.example/docs/page", "text/html", html);
            let (md, _) =
                extract_page_markdown_and_title(&page, &url("https://docs.example/docs/page"));
            md
        }

        #[test]
        fn short_main_card_grid_beats_longer_aside_and_dialog_widgets() {
            let long_help = "This demo works on codespaces. ".repeat(30);
            let html = format!(
                r#"<html><head><title>Destinations | Docs</title></head><body>
                <nav><a href="/docs">Docs</a></nav>
                <main><h1>Destinations</h1><ul><li><a href="/big">BigQuery</a></li><li><a href="/duck">DuckDB</a></li></ul></main>
                <aside><h2>DHelp</h2><p>{long_help}</p></aside>
                <div role="dialog"><p>Dialog help widget text that is also very long.</p></div>
                <div role="complementary"><p>Complementary sidebar widget text.</p></div>
                <dialog><p>Native dialog widget text.</p></dialog>
                </body></html>"#
            );
            let md = extract(&html);
            assert!(md.contains("Destinations"), "missing heading: {md}");
            assert!(md.contains("BigQuery"), "missing card content: {md}");
            assert!(!md.contains("DHelp"), "aside widget leaked: {md}");
            assert!(!md.contains("codespaces"), "aside body leaked: {md}");
            assert!(!md.contains("Dialog help"), "role=dialog leaked: {md}");
            assert!(
                !md.contains("Complementary sidebar"),
                "role=complementary leaked: {md}"
            );
            assert!(!md.contains("Native dialog"), "dialog tag leaked: {md}");
        }

        #[test]
        fn empty_main_falls_through_to_article() {
            let html = r#"<html><body>
                <main><nav>Menu</nav></main>
                <article><h1>Guide</h1><p>Body.</p></article>
            </body></html>"#;
            let md = extract(html);
            assert!(md.contains("Guide"), "{md}");
            assert!(md.contains("Body."), "{md}");
            assert!(!md.contains("Menu"), "{md}");
        }

        #[test]
        fn nested_article_inside_main_is_preserved() {
            let html = r#"<html><body>
                <main><h1>Overview</h1><article><h2>Card</h2><p>Details.</p></article></main>
            </body></html>"#;
            let md = extract(html);
            assert!(md.contains("Overview"), "{md}");
            assert!(md.contains("Details."), "{md}");
        }

        #[test]
        fn multiple_articles_without_main_are_not_truncated_to_first() {
            let html = r#"<html><body>
                <article><h2>First</h2><p>One.</p></article>
                <article><h2>Second</h2><p>Two.</p></article>
            </body></html>"#;
            let md = extract(html);
            assert!(md.contains("First"), "{md}");
            assert!(md.contains("Second"), "{md}");
        }

        #[test]
        fn widget_inside_main_is_stripped() {
            let html = r#"<html><body>
                <main>
                    <h1>Page</h1>
                    <p>Real.</p>
                    <aside><p>Widget.</p></aside>
                    <div role="dialog"><p>Popup.</p></div>
                </main>
            </body></html>"#;
            let md = extract(html);
            assert!(md.contains("Real."), "{md}");
            assert!(!md.contains("Widget."), "{md}");
            assert!(!md.contains("Popup."), "{md}");
        }

        #[test]
        fn non_landmark_page_prefers_fallback_when_readability_drops_matching_heading() {
            let widget_prose = "This demo works on codespaces. ".repeat(30);
            let html = format!(
                r#"<html><head><title>Destinations | Docs</title></head><body>
                <div><h1>Destinations</h1><p>Short list.</p></div>
                <div class="widget"><h2>DHelp</h2><p>{widget_prose}</p></div>
                </body></html>"#
            );
            let md = extract(&html);
            assert!(md.contains("# Destinations"), "missing h1: {md}");
            assert!(md.contains("Short list."), "missing body: {md}");
        }

        #[test]
        fn should_prefer_fallback_keeps_readability_when_matching_heading_present() {
            let readability_md = "# Destinations\n\nClean prose body with enough words to pass the length ratio check easily.";
            let fallback_md = "# Destinations\n\nClean prose body with enough words to pass the length ratio check easily.\n\nExtra footer.";
            assert!(!super::super::should_prefer_fallback(
                readability_md,
                fallback_md,
                Some("Destinations | Docs"),
                Some("Destinations"),
            ));
        }

        #[test]
        fn non_landmark_page_preserves_base_url_for_relative_links() {
            let html = r#"<html><body><div><h1>Guide</h1><p>Read the <a href="/docs/next">next page</a> for full details and examples of usage.</p></div></body></html>"#;
            let md = extract(html);
            assert!(
                !md.contains("example.net"),
                "relative link fell back to example.net: {md}"
            );
            assert!(md.contains("docs.example/docs/next"), "{md}");
        }

        #[test]
        fn void_role_main_element_does_not_break_landmark_extraction() {
            let html = r#"<html><body>
                <input role="main" type="text">
                <div role="main"><h1>Real</h1><p>Body.</p></div>
            </body></html>"#;
            let md = extract(html);
            assert!(md.contains("Real"), "{md}");
            assert!(md.contains("Body."), "{md}");
        }

        #[test]
        fn multiple_articles_preserve_outside_h1_and_all_cards() {
            let html = r#"<html><body>
                <div>
                    <h1>Catalog</h1>
                    <p>Intro.</p>
                    <article><h2>First</h2><p>One.</p></article>
                    <article><h2>Second</h2><p>Two.</p></article>
                </div>
            </body></html>"#;
            let md = extract(html);
            assert!(md.contains("Catalog"), "missing outside h1: {md}");
            assert!(md.contains("Intro."), "missing outside intro: {md}");
            assert!(md.contains("First"), "{md}");
            assert!(md.contains("Second"), "{md}");
        }

        #[test]
        fn extract_html_h1_decodes_entities_and_skips_empty_first_h1() {
            let widget_prose = "This demo works on codespaces. ".repeat(30);
            let html = format!(
                r#"<html><head><title>Users &amp; Roles | Docs</title></head><body>
                <h1><img src="/logo.png" alt="Logo"></h1>
                <div><h1>Users &amp; Roles</h1><p>Short.</p></div>
                <div class="widget"><h2>DHelp</h2><p>{widget_prose}</p></div>
                </body></html>"#
            );
            let md = extract(&html);
            assert!(md.contains("Users & Roles"), "missing h1: {md}");
            assert!(md.contains("Short."), "missing body: {md}");
        }

        #[test]
        fn has_matching_heading_does_not_mask_missing_h1_with_title_suffix() {
            let widget_prose = "This demo works on codespaces. ".repeat(30);
            let html = format!(
                r#"<html><head><title>Destinations | Docs</title></head><body>
                <div><h1>Destinations</h1><p>Short list.</p></div>
                <div class="widget"><h2>Docs</h2><p>{widget_prose}</p></div>
                </body></html>"#
            );
            let md = extract(&html);
            assert!(md.contains("# Destinations"), "missing h1: {md}");
            assert!(md.contains("Short list."), "missing body: {md}");
        }
    }
}
