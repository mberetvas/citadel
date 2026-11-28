use std::collections::HashSet;
use std::time::Duration;

use regex::RegexSet;
use spider::compact_str::CompactString;
use spider::configuration::{RedirectPolicy, WaitForIdleNetwork, WaitForSelector};
use spider::page::Page;
use spider::website::Website;
use url::Url;

use crate::error::{Result, VaultError};
use crate::ingest::persist::{OnWritten, SyncPages};
use crate::ingest::SourceRecord;
use crate::sources::origin::is_private_host;

mod collected;

use collected::{Collected, CollectedPages, LaterCrawl, PageCounts};

const DEFAULT_CONCURRENCY: usize = 4;
const DEFAULT_CRAWL_DELAY_MS: u64 = 100;
const USER_AGENT: &str = concat!("citadel/", env!("CARGO_PKG_VERSION"));
/// Run on every rendered page (path `/` matches all) after it settles and before its HTML is
/// read. Two steps:
/// 1. Web components draw into shadow DOM, which a page's HTML does not include (DevNet's API
///    reference renders each endpoint this way), so copy every open shadow root, innermost first,
///    into a plain element under its host. Styles and scripts are dropped.
/// 2. Drop navigation: a list or nav block with 30+ links whose text is over 80% link text is a
///    menu (a script-built sidebar can outweigh the page body a hundred to one). It is wrapped in
///    a `<nav>`, which the Markdown conversion ignores, rather than removed, because the crawler
///    reads this same HTML for links to follow. It is dropped only when at least 200 characters
///    of other text remain, so an index page keeps its list.
const PREPARE_RENDERED_PAGE_JS: &str = r#"(function () {
  (function flatten(root) {
    root.querySelectorAll('*').forEach(function (el) {
      if (!el.shadowRoot) { return; }
      flatten(el.shadowRoot);
      var copy = document.createElement('div');
      copy.setAttribute('data-shadow-content', '');
      copy.innerHTML = el.shadowRoot.innerHTML;
      copy.querySelectorAll('style, script, link, template').forEach(function (n) { n.remove(); });
      el.appendChild(copy);
    });
  })(document);

  if (!document.body) { return; }
  var size = function (el) { return (el.textContent || '').trim().length; };
  var menus = [];
  document.body.querySelectorAll('nav, aside, ul, ol').forEach(function (el) {
    if (menus.some(function (menu) { return menu.contains(el); })) { return; }
    var links = el.querySelectorAll('a');
    var total = size(el);
    if (links.length < 30 || total === 0) { return; }
    var linked = 0;
    links.forEach(function (a) { linked += size(a); });
    if (linked / total > 0.8) { menus.push(el); }
  });
  var pageText = document.body.cloneNode(true);
  pageText.querySelectorAll('script, style').forEach(function (n) { n.remove(); });
  var remaining = size(pageText);
  menus.forEach(function (menu) { remaining -= size(menu); });
  if (remaining < 200) { return; }
  menus.forEach(function (menu) {
    if (menu.tagName === 'NAV' || !menu.parentNode) { return; }
    var wrapper = document.createElement('nav');
    wrapper.setAttribute('data-citadel-menu', '');
    menu.parentNode.insertBefore(wrapper, menu);
    wrapper.appendChild(menu);
  });
})();"#;
/// A `render_js` crawl is stopped when no page finishes for this long (spider's own per-request
/// timeout is 120 s, so a healthy crawl always produces pages well within it).
const RENDER_STALL_LIMIT: Duration = Duration::from_secs(300);
/// Per-page cap on waiting for a JS-rendered page to settle before links are read.
const RENDER_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const SEED_FETCH_TIMEOUT_SECS: u64 = 10;
const MAX_CHILD_SITEMAPS: usize = 10;
const NO_CHROME_WARNING: &str = "render_js is enabled, but this is a lite citadel build without the `chrome` feature (--no-default-features); fell back to static HTTP crawl";

const STATIC_ASSET_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "ico", "bmp", "tiff", "avif", "css", "js", "mjs",
    "cjs", "map", "woff", "woff2", "ttf", "otf", "eot", "mp3", "mp4", "webm", "ogg", "wav", "avi",
    "mov", "pdf", "zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", "exe", "dmg", "pkg", "deb",
    "rpm", "wasm",
];

/// Ends every scope regex: a match may be followed by a query string or fragment, nothing else.
const QUERY_TAIL: &str = "(?:[?#].*)?$";
/// A character class that matches nothing (a glob class whose only ranges are reversed).
const NO_CHAR: &str = r"[^\x00-\x{10FFFF}]";

/// Which URLs belong to a web Source: built once per Sync from the Origin and the Source's
/// include/exclude patterns. Each pattern is parsed once ([`UrlPattern`]) into regexes anchored on
/// the absolute URL. spider gets those regex strings as its whitelist/blacklist and
/// `should_crawl`/`should_store` match the same strings, so every storable URL is crawlable.
#[derive(Debug, Clone)]
struct WebScope {
    origin: Url,
    origin_scheme: String,
    origin_host: String,
    origin_port: Option<u16>,
    /// The Origin page (only when includes are set, so its links are followed), then the
    /// includes, or the Origin's path prefix when there are none.
    crawl_allow: Vec<String>,
    /// The static-asset rule, then the excludes.
    deny: Vec<String>,
    crawl_allow_set: RegexSet,
    store_allow_set: RegexSet,
    deny_set: RegexSet,
}

impl WebScope {
    fn new(origin: &Url, include_patterns: &[String], exclude_patterns: &[String]) -> Result<Self> {
        let host = origin
            .host_str()
            .ok_or_else(|| VaultError::Validation(format!("origin URL '{origin}' lacks a host")))?
            .to_ascii_lowercase();
        let port = origin.port().map(|p| format!(":{p}")).unwrap_or_default();
        let authority = format!(
            "{}://{}{port}",
            regex::escape(&origin.scheme().to_ascii_lowercase()),
            regex::escape(&host)
        );
        let prefix = normalize_prefix(origin.path());
        let includes = pattern_regexes(&authority, &prefix, include_patterns)?;
        let excludes = pattern_regexes(&authority, &prefix, exclude_patterns)?;

        let (seed, store_allow) = if includes.is_empty() {
            let path = regex::escape(prefix.trim_end_matches('/'));
            (None, vec![format!("^{authority}{path}(?:[/?#].*)?$")])
        } else {
            let path = regex::escape(origin.path().trim_end_matches('/'));
            (Some(format!("^{authority}{path}/?{QUERY_TAIL}")), includes)
        };
        let crawl_allow: Vec<String> = seed.into_iter().chain(store_allow.clone()).collect();
        let asset_rule = format!(
            r"^[^?#]*\.(?i:{}){QUERY_TAIL}",
            STATIC_ASSET_EXTENSIONS.join("|")
        );
        let deny: Vec<String> = std::iter::once(asset_rule).chain(excludes).collect();

        Ok(Self {
            origin: origin.clone(),
            origin_scheme: origin.scheme().to_ascii_lowercase(),
            origin_host: host,
            origin_port: origin.port_or_known_default(),
            crawl_allow_set: compile_regex_set(&crawl_allow)?,
            store_allow_set: compile_regex_set(&store_allow)?,
            deny_set: compile_regex_set(&deny)?,
            crawl_allow,
            deny,
        })
    }

    fn same_authority(&self, url: &Url) -> bool {
        let scheme_ok = url.scheme().eq_ignore_ascii_case(&self.origin_scheme);
        let host_ok = url
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case(&self.origin_host));
        let port_ok = url.port_or_known_default() == self.origin_port;
        scheme_ok && host_ok && port_ok
    }

    fn should_crawl(&self, url: &Url) -> bool {
        self.same_authority(url)
            && self.crawl_allow_set.is_match(url.as_str())
            && !self.deny_set.is_match(url.as_str())
    }

    fn should_store(&self, url: &Url) -> bool {
        self.same_authority(url)
            && self.store_allow_set.is_match(url.as_str())
            && !self.deny_set.is_match(url.as_str())
    }

    fn whitelist_regexes(&self) -> Vec<CompactString> {
        self.crawl_allow.iter().map(CompactString::from).collect()
    }

    fn blacklist_regexes(&self) -> Vec<CompactString> {
        self.deny.iter().map(CompactString::from).collect()
    }
}

/// Check a web Source's include/exclude patterns with the parser its Syncs use, so a pattern
/// that cannot be matched fails when the Source is added or updated, not during a Sync.
pub(crate) fn validate_scope_patterns(
    origin: &str,
    include_patterns: &[String],
    exclude_patterns: &[String],
) -> Result<()> {
    let origin = Url::parse(origin).map_err(|err| {
        VaultError::Validation(format!("invalid web source URL '{origin}': {err}"))
    })?;
    WebScope::new(&origin, include_patterns, exclude_patterns).map(drop)
}

fn compile_regex_set(regexes: &[String]) -> Result<RegexSet> {
    RegexSet::new(regexes).map_err(|err| {
        VaultError::Validation(format!("include/exclude pattern cannot be matched: {err}"))
    })
}

/// One include/exclude pattern of a web Source, parsed once into a regex body that matches the
/// rest of the URL to its end (a query string or fragment may follow).
enum UrlPattern {
    /// An `http://` or `https://` pattern, matched against the whole URL.
    Absolute(String),
    /// Matched against the path from the host root and from the Origin's path prefix.
    Relative(String),
}

impl UrlPattern {
    /// A pattern with `*`, `?` or `[` is a glob with `glob` 0.3 syntax (checked by
    /// `glob::Pattern::new`); any other pattern is a path prefix of whole segments. URLs are
    /// matched percent-encoded, so the pattern's literal characters are encoded the way `url`
    /// encodes a path. An absolute pattern's scheme and host are lowercased and a default port
    /// dropped, as `url` normalizes them.
    fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let absolute = normalize_absolute_pattern(trimmed);
        let text = match &absolute {
            Some(normalized) => normalized.as_str(),
            None => trimmed.trim_start_matches("./").trim_start_matches('/'),
        };
        if text.is_empty() {
            return Err(VaultError::Validation(
                "include/exclude pattern must not be empty".into(),
            ));
        }
        // A `?` followed by `=` or `&` within the same path segment reads as a query string; a
        // `=` or `&` in a later segment is an ordinary path character.
        let query_like = text.split('?').skip(1).any(|after| {
            after
                .split('/')
                .next()
                .is_some_and(|seg| seg.contains(['=', '&']))
        });
        if query_like {
            return Err(VaultError::Validation(format!(
                "pattern '{raw}' looks like it matches a query string; patterns match only \
                 the URL path (any query string may follow a match), and '?' stands for one \
                 path character"
            )));
        }
        let body = if text.contains(['*', '?', '[']) {
            glob::Pattern::new(text).map_err(|err| {
                VaultError::Validation(format!("invalid glob pattern '{raw}': {err}"))
            })?;
            format!("{}{QUERY_TAIL}", glob_to_url_regex(text, raw)?)
        } else {
            let literal: String = text
                .trim_end_matches('/')
                .chars()
                .map(encoded_literal)
                .collect();
            format!("{literal}(?:[/?#].*)?$")
        };
        Ok(if absolute.is_some() {
            Self::Absolute(body)
        } else {
            Self::Relative(body)
        })
    }
}

/// An `http(s)://` pattern with its scheme and authority lowercased and a default port dropped,
/// or `None` for a relative pattern.
fn normalize_absolute_pattern(pattern: &str) -> Option<String> {
    let (scheme, rest) = pattern.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => ":80",
        "https" => ":443",
        _ => return None,
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let authority = authority.to_ascii_lowercase();
    let authority = authority.strip_suffix(default_port).unwrap_or(&authority);
    Some(format!("{scheme}://{authority}{path}"))
}

/// Whether `url` percent-encodes `ch` in a path (the WHATWG path percent-encode set, non-ASCII,
/// and the backslash, which special URLs turn into `/`).
fn encoded_in_path(ch: char) -> bool {
    !ch.is_ascii()
        || ch.is_ascii_control()
        || matches!(
            ch,
            ' ' | '"' | '#' | '<' | '>' | '?' | '^' | '`' | '{' | '}' | '\\'
        )
}

/// A literal pattern character as a regex for its form in a percent-encoded URL path.
fn encoded_literal(ch: char) -> String {
    if ch == '\\' {
        return "/".to_string();
    }
    if !encoded_in_path(ch) {
        return regex::escape(ch.encode_utf8(&mut [0u8; 4]));
    }
    // `url` writes the escapes it adds in uppercase but keeps a link's own escapes as written,
    // so match the hex digits in either case.
    ch.encode_utf8(&mut [0u8; 4])
        .bytes()
        .map(|b| {
            let hex: String = format!("{b:02X}")
                .chars()
                .map(|d| {
                    if d.is_ascii_alphabetic() {
                        format!("[{d}{}]", d.to_ascii_lowercase())
                    } else {
                        d.to_string()
                    }
                })
                .collect();
            format!("%{hex}")
        })
        .collect()
}

/// The anchored regexes of every pattern: one for an absolute pattern; for a relative one, one
/// from the host root and, when the Origin has a path prefix, one from that prefix.
fn pattern_regexes(
    authority: &str,
    origin_prefix: &str,
    patterns: &[String],
) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(patterns.len());
    for raw in patterns {
        match UrlPattern::parse(raw)? {
            UrlPattern::Absolute(body) => out.push(format!("^{body}")),
            UrlPattern::Relative(body) => {
                out.push(format!("^{authority}/{body}"));
                if origin_prefix != "/" {
                    let prefix = regex::escape(origin_prefix);
                    out.push(format!("^{authority}{prefix}/{body}"));
                }
            }
        }
    }
    Ok(out)
}

/// Translate a glob that `glob::Pattern::new` accepted into a regex with the same meaning on a
/// percent-encoded URL path (glob 0.3 with default options: `*` and `?` also match `/`,
/// case-sensitive), where no wildcard or class crosses into the query string or fragment.
fn glob_to_url_regex(glob: &str, raw: &str) -> Result<String> {
    let chars: Vec<char> = glob.chars().collect();
    let mut out = String::with_capacity(glob.len() * 2);
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '?' => {
                out.push_str(&format!("(?:[^?#%]|{ENCODED_CHAR})"));
                i += 1;
            }
            '*' if chars.get(i + 1) == Some(&'*') => {
                // `**` is a whole path component (glob rejects anything else): `**/` matches zero
                // or more directories, a trailing `**` anything.
                i += 2;
                if chars.get(i) == Some(&'/') {
                    out.push_str("(?:[^?#]*/)?");
                    i += 1;
                } else {
                    out.push_str("[^?#]*");
                }
            }
            '*' => {
                out.push_str("[^?#]*");
                i += 1;
            }
            '[' => {
                // As glob reads it: `[!` negates, and the first member may be `]`.
                let negated = chars.get(i + 1) == Some(&'!');
                let first = if negated { i + 2 } else { i + 1 };
                let close = chars
                    .get(first + 1..)
                    .and_then(|rest| rest.iter().position(|c| *c == ']'))
                    .map(|pos| first + 1 + pos)
                    .ok_or_else(|| {
                        VaultError::Validation(format!(
                            "invalid glob pattern '{raw}': unclosed '['"
                        ))
                    })?;
                out.push_str(&class_to_regex(&chars[first..close], negated, raw)?);
                i = close + 1;
            }
            ch => {
                out.push_str(&encoded_literal(ch));
                i += 1;
            }
        }
    }
    Ok(out)
}

/// One percent-encoded character: an encoded ASCII byte, or a UTF-8 lead byte and its
/// continuation bytes.
const ENCODED_CHAR: &str = "%[0-7][0-9A-Fa-f]|%[C-Fc-f][0-9A-Fa-f](?:%[89ABab][0-9A-Fa-f]){1,3}";

/// A glob character class (`a`, `a-z`; a reversed range matches nothing) as a regex for one
/// character of a percent-encoded path, never `?` or `#`. A member the path encodes is matched in
/// its encoded form; a negated class or a range cannot express that and is rejected.
fn class_to_regex(members: &[char], negated: bool, raw: &str) -> Result<String> {
    let unsupported = |what: String| {
        VaultError::Validation(format!(
            "pattern '{raw}': {what} cannot match a percent-encoded URL path character by character"
        ))
    };
    let escape = |ch: char| regex::escape(ch.encode_utf8(&mut [0u8; 4]));
    let mut set = String::new();
    let mut encoded = Vec::new();
    let mut i = 0usize;
    while i < members.len() {
        if i + 2 < members.len() && members[i + 1] == '-' {
            let (lo, hi) = (members[i], members[i + 2]);
            if lo <= hi {
                if (lo..=hi).any(encoded_in_path) {
                    return Err(unsupported(format!("the range '{lo}-{hi}'")));
                }
                set.push_str(&format!("{}-{}", escape(lo), escape(hi)));
            }
            i += 3;
        } else {
            let ch = members[i];
            if encoded_in_path(ch) {
                if negated {
                    return Err(unsupported(format!("the negated class member '{ch}'")));
                }
                encoded.push(encoded_literal(ch));
            } else {
                set.push_str(&escape(ch));
            }
            i += 1;
        }
    }
    if negated {
        return Ok(format!("(?:[^?#%{set}]|{ENCODED_CHAR})"));
    }
    let mut alternatives = encoded;
    if !set.is_empty() {
        alternatives.insert(0, format!("[[{set}]&&[^?#]]"));
    }
    Ok(match alternatives.len() {
        0 => NO_CHAR.to_string(),
        _ => format!("(?:{})", alternatives.join("|")),
    })
}

/// The record of one web Sync, logged as one `web_sync_summary` line when the Sync ends: the page
/// counts [`CollectedPages`] keeps, plus the delay and step durations the caller measures.
#[derive(Debug, Default)]
pub(crate) struct WebSyncAccounting {
    pub pages: PageCounts,
    /// The delay spider reports after the first crawl (`Crawl-delay` in `robots.txt` can change it).
    pub delay_ms: Option<u64>,
    pub discovery_ms: Option<u64>,
    pub first_crawl_ms: Option<u64>,
    pub later_crawls_ms: Option<u64>,
    pub conversion_ms: Option<u64>,
    pub write_ms: Option<u64>,
}

/// A step's duration on the summary line: milliseconds, or `-` for a step the Sync never reached.
struct StepMs(Option<u64>);

impl std::fmt::Display for StepMs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(ms) => write!(f, "{ms}"),
            None => f.write_str("-"),
        }
    }
}

/// Record how long a step took and pass its value on unchanged. Applied to a step's `Result`
/// before `?`, so a step that ran and failed still has a duration and its summary field is not `-`.
fn finish_step<T>(slot: &mut Option<u64>, start: std::time::Instant, value: T) -> T {
    *slot = Some(millis(start.elapsed()));
    value
}

/// A duration in whole milliseconds, saturating at `u64::MAX`.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The web-crawl driver: crawl one web Source and collect its pages. It writes nothing; the
/// Source lifecycle writes the pages and records the Sync run.
///
/// Every Sync whose Origin passes the checks logs one `web_sync_summary` line: here when the
/// crawl fails, otherwise from `on_written` once the lifecycle has written the pages.
pub(crate) async fn collect(source: &SourceRecord, prune: bool) -> Result<SyncPages> {
    let (origin, scope) = web_origin_and_scope(source)?;
    let (result, acc) = collect_with_accounting(source, prune, &origin, &scope).await;
    let mut pages = match result {
        Ok(pages) => pages,
        Err(err) => {
            log_web_sync_summary(source, false, &acc);
            return Err(err);
        }
    };
    pages.on_written = Some(log_after_write(source.clone(), acc));
    Ok(pages)
}

/// The `on_written` hook of a web Sync: log its `web_sync_summary` line, with `ok` and
/// `write_ms` for the write, once the lifecycle has written the pages (or failed to).
fn log_after_write(source: SourceRecord, mut acc: WebSyncAccounting) -> OnWritten {
    Box::new(move |ok, write| {
        acc.write_ms = Some(millis(write));
        log_web_sync_summary(&source, ok, &acc);
    })
}

/// Crawl a checked web Source and return its pages with the accounting record so far.
async fn collect_with_accounting(
    source: &SourceRecord,
    prune: bool,
    origin: &Url,
    scope: &WebScope,
) -> (Result<SyncPages>, WebSyncAccounting) {
    let mut acc = WebSyncAccounting::default();
    let result = execute_web_crawl(source, origin, scope, prune, &mut acc)
        .await
        .map(|collected| SyncPages {
            outcomes: collected.outcomes,
            missing_policy: collected.missing_policy,
            warnings: collected.warnings,
            on_written: None,
        });
    (result, acc)
}

fn log_web_sync_summary(source: &SourceRecord, ok: bool, acc: &WebSyncAccounting) {
    tracing::info!(
        source = ?source.name,
        origin = ?source.origin,
        ok,
        max_pages = source.max_pages,
        cut_off = acc.pages.cut_off,
        delay_ms = %StepMs(acc.delay_ms),
        pages_received = acc.pages.pages_received,
        results_stored = acc.pages.results_stored,
        results_failed = acc.pages.results_failed,
        lost_by_listener = acc.pages.lost_by_listener,
        unparsable_url = acc.pages.unparsable_url,
        already_collected = acc.pages.already_collected,
        redirected_out_of_scope = acc.pages.redirected_out_of_scope,
        url_not_storable = acc.pages.url_not_storable,
        duplicate_result = acc.pages.duplicate_result,
        markdown_twin = acc.pages.markdown_twin,
        canonical_duplicate = acc.pages.canonical_duplicate,
        url_alias = acc.pages.url_alias,
        binary_file = acc.pages.binary_file,
        later_crawls_started = acc.pages.later_crawls_started,
        discovery_ms = %StepMs(acc.discovery_ms),
        first_crawl_ms = %StepMs(acc.first_crawl_ms),
        later_crawls_ms = %StepMs(acc.later_crawls_ms),
        conversion_ms = %StepMs(acc.conversion_ms),
        write_ms = %StepMs(acc.write_ms),
        "web_sync_summary"
    );
}

fn web_origin_and_scope(source: &SourceRecord) -> Result<(Url, WebScope)> {
    let origin = Url::parse(&source.origin).map_err(|err| {
        VaultError::Validation(format!("invalid web source URL '{}': {err}", source.origin))
    })?;
    if !matches!(origin.scheme(), "http" | "https") || origin.host_str().is_none() {
        return Err(VaultError::Validation(format!(
            "web source URL '{}' must use http:// or https:// with a valid host",
            source.origin
        )));
    }

    let scope = WebScope::new(&origin, &source.include_patterns, &source.exclude_patterns)?;
    Ok((origin, scope))
}

async fn execute_web_crawl(
    source: &SourceRecord,
    origin: &Url,
    scope: &WebScope,
    prune: bool,
    acc: &mut WebSyncAccounting,
) -> Result<Collected> {
    let mut warnings = Vec::new();
    if source.render_js && !cfg!(feature = "chrome") {
        tracing::warn!(source = %source.name, "{}", NO_CHROME_WARNING);
        warnings.push(NO_CHROME_WARNING.to_string());
    }

    let discovery_start = std::time::Instant::now();
    let seed_urls = finish_step(
        &mut acc.discovery_ms,
        discovery_start,
        discover_seed_urls(origin, scope).await,
    );

    let mut collected = CollectedPages::new(scope, source.max_pages);
    if let Err(err) = run_crawls(source, origin, scope, seed_urls, &mut collected, acc).await {
        acc.pages = collected.counts().clone();
        return Err(err);
    }

    let conversion_start = std::time::Instant::now();
    let (result, counts) = collected.finish(prune);
    acc.pages = counts;
    let mut to_write = finish_step(&mut acc.conversion_ms, conversion_start, result)?;
    warnings.append(&mut to_write.warnings);
    to_write.warnings = warnings;
    Ok(to_write)
}

/// Run the first crawl from the Origin, then the later crawls, handing every page to `collected`.
async fn run_crawls(
    source: &SourceRecord,
    origin: &Url,
    scope: &WebScope,
    seed_urls: Vec<Url>,
    collected: &mut CollectedPages<'_>,
    acc: &mut WebSyncAccounting,
) -> Result<()> {
    let mut primary = configure_website(
        origin,
        scope,
        source.max_pages,
        source.max_depth,
        source.render_js,
    );
    let first_crawl_start = std::time::Instant::now();
    let (pages, lost) = finish_step(
        &mut acc.first_crawl_ms,
        first_crawl_start,
        run_website_crawl(&mut primary, source.render_js, origin).await,
    )?;
    acc.delay_ms = Some(millis(primary.get_delay()));
    collected.add_first_crawl(pages, lost);

    let later_crawls_start = std::time::Instant::now();
    let later_crawls = run_later_crawls(source, origin, scope, seed_urls, collected).await;
    finish_step(&mut acc.later_crawls_ms, later_crawls_start, later_crawls)
}

/// Start one more crawler per discovered URL that no earlier crawl collected, one after another,
/// each limited to what is left of the page budget.
async fn run_later_crawls(
    source: &SourceRecord,
    origin: &Url,
    scope: &WebScope,
    seed_urls: Vec<Url>,
    collected: &mut CollectedPages<'_>,
) -> Result<()> {
    for seed in seed_urls {
        let budget = match collected.next_later_crawl(&seed) {
            LaterCrawl::Skip => continue,
            LaterCrawl::Stop => break,
            LaterCrawl::Crawl(budget) => budget,
        };
        let mut seed_site =
            configure_website(&seed, scope, budget, source.max_depth, source.render_js);
        let (pages, lost) = run_website_crawl(&mut seed_site, source.render_js, origin).await?;
        collected.add_later_crawl(&seed, pages, lost);
    }
    Ok(())
}

fn configure_website(
    seed: &Url,
    scope: &WebScope,
    max_pages: u32,
    max_depth: u32,
    render_js: bool,
) -> Website {
    let mut website = Website::new_with_firewall(seed.as_str(), false);
    let base_segments = seed.path().split('/').filter(|s| !s.is_empty()).count();
    let effective_depth = base_segments.saturating_add(max_depth as usize).max(1);

    let redirect_policy = if is_private_host(seed) {
        RedirectPolicy::Strict
    } else {
        RedirectPolicy::Loose
    };

    website
        .with_respect_robots_txt(true)
        .with_limit(max_pages)
        .with_depth(effective_depth)
        .with_concurrency_limit(Some(DEFAULT_CONCURRENCY))
        .with_delay(DEFAULT_CRAWL_DELAY_MS)
        .with_subdomains(false)
        .with_tld(false)
        .with_redirect_policy(redirect_policy)
        .with_whitelist_url(Some(scope.whitelist_regexes()))
        .with_blacklist_url(Some(scope.blacklist_regexes()))
        .with_user_agent(Some(USER_AGENT));

    if render_js {
        // Script-built navigation (e.g. sidebars rendered from embedded JSON) only exists once
        // the page settles; read links after the network and DOM go quiet, capped per page.
        website
            // Citadel seeds from sitemaps itself (`discover_seed_urls`). spider's own sitemap
            // pass reconnects to the browser after the crawl; that pass can hang forever and
            // can report its sitemap 404 against the origin page.
            .with_ignore_sitemap(true)
            .with_execution_scripts(Some(
                [("/".to_string(), PREPARE_RENDERED_PAGE_JS.to_string())]
                    .into_iter()
                    .collect(),
            ))
            .with_wait_for_idle_network0(Some(WaitForIdleNetwork::new(Some(RENDER_WAIT_TIMEOUT))))
            .with_wait_for_idle_dom(Some(WaitForSelector::new(
                Some(RENDER_WAIT_TIMEOUT),
                "body".to_string(),
            )));
    }

    website
}

/// Fail a `render_js` Sync up front when the configured remote browser cannot be reached: spider
/// would otherwise fall back to a static fetch per page and silently miss script-built links.
#[cfg(feature = "chrome")]
async fn ensure_render_browser(website: &Website) -> Result<()> {
    match website.setup_browser().await {
        Some(mut browser) => {
            browser.dispose();
            Ok(())
        }
        None => Err(VaultError::Other(
            super::browser::NO_BROWSER_ERROR.to_string(),
        )),
    }
}

/// Give a `render_js` crawl its own browser: a remote one from `CHROME_URL` when set (checked
/// up front), otherwise a private headless Chrome launched for this crawl only.
#[cfg(feature = "chrome")]
async fn prepare_render_browser(
    website: &mut Website,
) -> Result<Option<super::browser::RenderBrowser>> {
    if std::env::var_os(super::browser::CHROME_URL_ENV).is_some() {
        ensure_render_browser(website).await?;
        return Ok(None);
    }
    let browser = super::browser::RenderBrowser::launch().await?;
    website.with_chrome_connection(Some(browser.websocket_url()));
    Ok(Some(browser))
}

/// When a crawl last produced a page, shared between the page collector and the stall watchdog.
#[derive(Clone)]
struct CrawlActivity {
    started: tokio::time::Instant,
    last_page_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl CrawlActivity {
    fn start() -> Self {
        Self {
            started: tokio::time::Instant::now(),
            last_page_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Record that a page just finished.
    fn touch(&self) {
        let elapsed = millis(self.started.elapsed());
        self.last_page_ms
            .store(elapsed, std::sync::atomic::Ordering::Relaxed);
    }

    /// Resolve once no page has finished (and the crawl has not started one) for `limit`.
    async fn stalled_for(&self, limit: Duration) {
        loop {
            let last =
                Duration::from_millis(self.last_page_ms.load(std::sync::atomic::Ordering::Relaxed));
            let idle = self.started.elapsed().saturating_sub(last);
            if idle >= limit {
                return;
            }
            tokio::time::sleep(limit - idle).await;
        }
    }
}

/// Run one crawler and return the pages it delivered with the number the receiving task missed
/// because it fell behind.
/// Run one crawl and return the pages it delivered and the number the receiving task lost. On a
/// rendered crawl, a browser-reported 4xx is checked against the server (`origin` builds the
/// client) before the pages are returned.
async fn run_website_crawl(
    website: &mut Website,
    render_js: bool,
    origin: &Url,
) -> Result<(Vec<Page>, usize)> {
    #[cfg(feature = "chrome")]
    let render_browser = if render_js {
        prepare_render_browser(website).await?
    } else {
        None
    };

    let mut rx = website.subscribe(256);
    let activity = CrawlActivity::start();
    let collector_activity = activity.clone();
    let collector = tokio::spawn(async move {
        let mut pages = Vec::new();
        let mut lost = 0usize;
        loop {
            match rx.recv().await {
                Ok(page) => {
                    collector_activity.touch();
                    pages.push(page);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "spider broadcast channel lagged during crawl");
                    lost = lost.saturating_add(usize::try_from(skipped).unwrap_or(usize::MAX));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        (pages, lost)
    });

    let mut stalled = false;
    if render_js && cfg!(feature = "chrome") {
        // A browser crawl can wait forever on a page or connection that never answers. Stop it
        // once no page has finished for `RENDER_STALL_LIMIT` instead of hanging the Sync.
        tokio::select! {
            () = website.crawl() => {}
            () = activity.stalled_for(RENDER_STALL_LIMIT) => stalled = true,
        }
    } else {
        website.crawl_raw().await;
    }
    website.unsubscribe();

    #[cfg(feature = "chrome")]
    if let Some(browser) = render_browser {
        browser.shutdown().await;
    }

    if stalled {
        collector.abort();
        tracing::warn!(url = %website.get_url(), "render_js crawl stalled; Sync stopped");
        return Err(VaultError::Other(format!(
            "render_js crawl stalled: no page finished for {} minutes, so the Sync was stopped \
             and the previously stored pages were kept",
            RENDER_STALL_LIMIT.as_secs() / 60
        )));
    }

    let (mut pages, lost) = collector
        .await
        .map_err(|err| VaultError::Other(format!("crawler collector task panicked: {err}")))?;
    if render_js && cfg!(feature = "chrome") {
        confirm_render_failures(&mut pages, origin).await;
    }
    Ok((pages, lost))
}

/// Re-check rendered pages that the browser reported as HTTP 4xx. A browser crawl occasionally
/// reports another response's status for a page (seen: the 404 of the browser's own
/// `/favicon.ico` request recorded against a page served with 200), so a client error is only
/// kept when a direct request to the server agrees.
async fn confirm_render_failures(pages: &mut [Page], origin: &Url) {
    let suspects = pages.iter().any(is_unconfirmed_client_error);
    if !suspects {
        return;
    }
    let client = match build_seed_http_client(origin) {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(error = %err, "failed to build HTTP client to confirm page errors");
            return;
        }
    };
    for page in pages
        .iter_mut()
        .filter(|page| is_unconfirmed_client_error(page))
    {
        let server_status = match client.get(page.get_url()).send().await {
            Ok(response) => Some(response.status()),
            Err(_) => None,
        };
        let confirmed = confirmed_status(page.status_code, server_status);
        if confirmed != page.status_code {
            tracing::debug!(
                url = page.get_url(),
                browser_status = page.status_code.as_u16(),
                server_status = confirmed.as_u16(),
                "browser-reported page error not confirmed by the server"
            );
            page.status_code = confirmed;
        }
    }
}

/// A rendered page with content whose only problem is a browser-reported 4xx status.
fn is_unconfirmed_client_error(page: &Page) -> bool {
    page.status_code.is_client_error()
        && page.error_status.is_none()
        && !page.get_html_bytes_u8().is_empty()
}

/// The status to record for a page the browser reported as a client error: the server's own
/// answer when it says the page is fine, otherwise the browser's.
fn confirmed_status(
    browser: reqwest::StatusCode,
    server: Option<reqwest::StatusCode>,
) -> reqwest::StatusCode {
    match server {
        Some(status) if status.is_success() => status,
        _ => browser,
    }
}

async fn discover_seed_urls(origin: &Url, scope: &WebScope) -> Vec<Url> {
    let client = match build_seed_http_client(origin) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(error = %err, "failed to build HTTP client for llms.txt/sitemap seeding");
            return Vec::new();
        }
    };

    let mut seeds = Vec::new();
    let mut seen = HashSet::new();

    for llms_url in build_root_candidates(origin, "llms.txt") {
        if let Some(body) = fetch_text_if_ok(&client, &llms_url).await {
            for candidate in parse_llms_txt(&body, &llms_url) {
                if scope.should_crawl(&candidate) && seen.insert(canonical_dedup_key(&candidate)) {
                    seeds.push(candidate);
                }
            }
        }
    }

    let mut sitemap_queue = build_root_candidates(origin, "sitemap.xml");
    let mut visited_sitemaps = HashSet::new();
    let mut child_fetches = 0usize;

    while let Some(sitemap_url) = sitemap_queue.pop() {
        if !visited_sitemaps.insert(sitemap_url.to_string()) {
            continue;
        }
        let Some(body) = fetch_text_if_ok(&client, &sitemap_url).await else {
            continue;
        };
        let (page_urls, child_sitemaps) = parse_sitemap_xml(&body, &sitemap_url);
        for candidate in page_urls {
            if scope.should_crawl(&candidate) && seen.insert(canonical_dedup_key(&candidate)) {
                seeds.push(candidate);
            }
        }
        for child in child_sitemaps {
            if child_fetches >= MAX_CHILD_SITEMAPS {
                break;
            }
            if scope.same_authority(&child) {
                child_fetches += 1;
                sitemap_queue.push(child);
            }
        }
    }

    seeds
}

fn build_seed_http_client(origin: &Url) -> std::result::Result<reqwest::Client, reqwest::Error> {
    let allow_local = is_private_host(origin);
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > 5 {
            return attempt.error("too many redirects");
        }
        if !allow_local && is_private_host(attempt.url()) {
            return attempt.error("SSRF blocked: redirect to internal address");
        }
        attempt.follow()
    });

    reqwest::Client::builder()
        .timeout(Duration::from_secs(SEED_FETCH_TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .redirect(policy)
        .build()
}

async fn fetch_text_if_ok(client: &reqwest::Client, url: &Url) -> Option<String> {
    let resp = client.get(url.as_str()).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().await.ok()
}

fn build_root_candidates(origin: &Url, filename: &str) -> Vec<Url> {
    let mut out = Vec::new();
    if let Ok(mut root) = Url::parse(origin.as_str()) {
        root.set_query(None);
        root.set_fragment(None);
        root.set_path(&format!("/{filename}"));
        out.push(root);
    }

    let prefix = normalize_prefix(origin.path());
    if prefix != "/" {
        if let Ok(mut sub) = Url::parse(origin.as_str()) {
            sub.set_query(None);
            sub.set_fragment(None);
            sub.set_path(&format!("{prefix}/{filename}"));
            if !out.contains(&sub) {
                out.push(sub);
            }
        }
    }

    out
}

pub(crate) fn parse_llms_txt(body: &str, base_url: &Url) -> Vec<Url> {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();

    // 1. Markdown `[label](target)` links
    let mut rest = body;
    while let Some(idx) = rest.find("](") {
        let after_open = &rest[idx + 2..];
        let Some(close_idx) = after_open.find(')') else {
            break;
        };
        let inside = after_open[..close_idx].trim();
        let target = extract_markdown_link_target(inside);
        if let Some(resolved) = resolve_seed_url(base_url, target) {
            if seen.insert(resolved.to_string()) {
                urls.push(resolved);
            }
        }
        rest = &after_open[close_idx + 1..];
    }

    // 2. Bare http(s):// URLs and root-relative paths on list lines
    for line in body.lines() {
        let trimmed = line.trim();
        let list_item = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .map(str::trim);
        if let Some(item) = list_item {
            if item.starts_with('/') {
                let path_token = item.split_whitespace().next().unwrap_or("");
                if let Some(resolved) = resolve_seed_url(base_url, path_token) {
                    if seen.insert(resolved.to_string()) {
                        urls.push(resolved);
                    }
                }
            }
        }

        for token in trimmed.split_whitespace() {
            let cleaned = token
                .trim_matches(['<', '>', '(', ')', '[', ']', '"', '\''])
                .trim_end_matches(['.', ',', ';', ':']);
            if cleaned.starts_with("http://") || cleaned.starts_with("https://") {
                if let Some(resolved) = resolve_seed_url(base_url, cleaned) {
                    if seen.insert(resolved.to_string()) {
                        urls.push(resolved);
                    }
                }
            }
        }
    }

    urls
}

fn extract_markdown_link_target(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed.strip_prefix('<') {
        if let Some(end) = inner.find('>') {
            return inner[..end].trim();
        }
    }
    trimmed.split_whitespace().next().unwrap_or("")
}

fn parse_sitemap_xml(body: &str, base_url: &Url) -> (Vec<Url>, Vec<Url>) {
    let mut pages = Vec::new();
    let mut sitemaps = Vec::new();
    let is_index = body.contains("<sitemapindex");

    let mut cursor = body;
    while let Some(start) = cursor.find("<loc") {
        let after_tag_start = &cursor[start + 4..];
        let Some(gt) = after_tag_start.find('>') else {
            break;
        };
        let after_gt = &after_tag_start[gt + 1..];
        let Some(end) = after_gt.find("</loc>") else {
            break;
        };
        let raw_loc = after_gt[..end].trim();
        let unescaped = raw_loc
            .strip_prefix("<![CDATA[")
            .and_then(|s| s.strip_suffix("]]>"))
            .unwrap_or(raw_loc)
            .replace("&amp;", "&");
        if let Some(resolved) = resolve_seed_url(base_url, unescaped.trim()) {
            if is_index {
                sitemaps.push(resolved);
            } else {
                pages.push(resolved);
            }
        }
        cursor = &after_gt[end + 6..];
    }

    (pages, sitemaps)
}

fn resolve_seed_url(base_url: &Url, raw_target: &str) -> Option<Url> {
    let trimmed = raw_target.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('#')
        || trimmed.starts_with("mailto:")
        || trimmed.starts_with("javascript:")
    {
        return None;
    }
    let mut joined = base_url.join(trimmed).ok()?;
    if !matches!(joined.scheme(), "http" | "https") {
        return None;
    }
    joined.set_fragment(None);
    Some(joined)
}

fn canonicalize_page_url(raw: &str) -> Option<Url> {
    let mut url = Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    Some(url)
}

/// `url` without its fragment or trailing slash. The key spider's budget and the cut-off rule
/// count by (one per fetched URL), and that scope checks and Markdown twin lookups match on. To
/// group copies of one document, use [`document_key`].
fn canonical_dedup_key(url: &Url) -> String {
    key_with_path(url, url.path().trim_end_matches('/'))
}

/// `url` as a string without its fragment and with `path` (`/` when empty) as its path.
fn key_with_path(url: &Url, path: &str) -> String {
    let mut normalized = url.clone();
    normalized.set_fragment(None);
    normalized.set_path(if path.is_empty() { "/" } else { path });
    normalized.to_string()
}

/// The key under which URLs of one document collapse: [`canonical_dedup_key`] with a trailing
/// `/index`, `/index.html` or `/index.htm` segment removed too (`/a/`, `/a/index` -> `/a`). The
/// query string is kept. Groups copies for storage and dedups later crawls; spider's budget and
/// the cut-off rule keep counting by [`canonical_dedup_key`].
fn document_key(url: &Url) -> String {
    key_with_path(url, document_path(url))
}

/// `url`'s path without a trailing slash or `/index`, `/index.html` or `/index.htm` segment.
fn document_path(url: &Url) -> &str {
    let path = url.path().trim_end_matches('/');
    ["/index.html", "/index.htm", "/index"]
        .iter()
        .find_map(|suffix| path.strip_suffix(suffix))
        .unwrap_or(path)
}

/// The URL a page's document is at: where its redirects ended, else the requested URL.
fn effective_url(page: &Page) -> Option<Url> {
    canonicalize_page_url(page.get_url_final())
}

fn normalize_prefix(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod scope_tests {
    use spider::website::ProcessLinkStatus;
    use spider::CaseInsensitiveString;

    use super::{configure_website, Url, WebScope};

    /// One scope row: which patterns, which URL, and whether the URL is stored and crawled.
    struct Row {
        origin: &'static str,
        includes: &'static [&'static str],
        excludes: &'static [&'static str],
        url: &'static str,
        store: bool,
        crawl: bool,
    }

    const ROOT: &str = "https://ex.com/";
    const GUIDE: &str = "https://ex.com/guide/";

    const fn row(
        origin: &'static str,
        includes: &'static [&'static str],
        excludes: &'static [&'static str],
        url: &'static str,
        store: bool,
        crawl: bool,
    ) -> Row {
        Row {
            origin,
            includes,
            excludes,
            url,
            store,
            crawl,
        }
    }

    #[rustfmt::skip]
    const ROWS: &[Row] = &[
        // Controls: the prefix scope and the asset rule block these.
        row(GUIDE, &[], &[], "https://ex.com/guide/a", true, true),
        row(GUIDE, &[], &[], "https://ex.com/other", false, false),
        row(ROOT, &[], &[], "https://ex.com/img/logo.png", false, false),
        row(ROOT, &[], &[], "https://other.com/a", false, false),
        // 1. A glob matches the whole path: `docs/*.md` is not `docs/*.mdx`.
        row(ROOT, &["docs/*.md"], &[], "https://ex.com/docs/x.md", true, true),
        row(ROOT, &["docs/*.md"], &[], "https://ex.com/docs/x.md?v=1", true, true),
        row(ROOT, &["docs/*.md"], &[], "https://ex.com/docs/x.mdx", false, false),
        row(ROOT, &[], &["docs/*.md"], "https://ex.com/docs/x.mdx", true, true),
        row(ROOT, &["docs/v?"], &[], "https://ex.com/docs/v1", true, true),
        row(ROOT, &["docs/v?"], &[], "https://ex.com/docs/v12", false, false),
        // 2. `[!x]` is negation; `[^x]` is the set {^, x} in glob.
        row(ROOT, &["docs/[!d]*"], &[], "https://ex.com/docs/api", true, true),
        row(ROOT, &["docs/[!d]*"], &[], "https://ex.com/docs/draft", false, false),
        row(ROOT, &["docs/[^a]*"], &[], "https://ex.com/docs/api", true, true),
        row(ROOT, &["docs/[^a]*"], &[], "https://ex.com/docs/bpi", false, false),
        row(ROOT, &["docs/[]a]*"], &[], "https://ex.com/docs/api", true, true),
        row(ROOT, &["docs/[]a]*"], &[], "https://ex.com/docs/bpi", false, false),
        // A reversed range matches nothing in glob; it must not drop the whole regex set.
        row(ROOT, &["docs/[z-a]*"], &[], "https://ex.com/blog/a", false, false),
        row(ROOT, &[], &["docs/[z-a]*"], "https://ex.com/img/a.png", false, false),
        row(ROOT, &[], &["docs/[z-a]*"], "https://ex.com/docs/a", true, true),
        // 3. One static-asset rule: case-insensitive, on the path only.
        row(ROOT, &[], &[], "https://ex.com/img/LOGO.PNG", false, false),
        row(ROOT, &[], &[], "https://ex.com/page?file=a.png", true, true),
        row(ROOT, &[], &[], "https://ex.com/img/a.png#top", false, false),
        // `**/` matches zero or more directories; a trailing `**` anything below.
        row(ROOT, &["docs/**/x"], &[], "https://ex.com/docs/x", true, true),
        row(ROOT, &["docs/**/x"], &[], "https://ex.com/docs/a/b/x", true, true),
        row(ROOT, &["docs/**/x"], &[], "https://ex.com/docs/ax", false, false),
        row(ROOT, &["docs/**"], &[], "https://ex.com/docs/a/b", true, true),
        row(ROOT, &["docs/**"], &[], "https://ex.com/docs", false, false),
        // Wildcards stop at the query string.
        row(ROOT, &["docs/*.md"], &[], "https://ex.com/docs/a?x=.md", false, false),
        // Relative patterns match the path, not the host.
        row("https://docs.ex.com/", &["*docs*"], &[], "https://docs.ex.com/other", false, false),
        // Prefix patterns: whole segments, any query.
        row(ROOT, &["docs/"], &[], "https://ex.com/docs", true, true),
        row(ROOT, &["docs/"], &[], "https://ex.com/docs/a", true, true),
        row(ROOT, &["docs/"], &[], "https://ex.com/docsx", false, false),
        row(ROOT, &["https://ex.com/docs"], &[], "https://ex.com/docs?x=1", true, true),
        row(ROOT, &["https://ex.com/docs/*"], &[], "https://ex.com/docs/a", true, true),
        // Origin-relative and root-relative forms of a relative pattern.
        row(GUIDE, &["intro*"], &[], "https://ex.com/guide/intro-1", true, true),
        row(GUIDE, &["intro*"], &[], "https://ex.com/intro-1", true, true),
        row(GUIDE, &["intro*"], &[], "https://ex.com/guide/other", false, false),
        row(GUIDE, &[], &["intro*"], "https://ex.com/guide/intro-1", false, false),
        // Patterns are case-sensitive.
        row(ROOT, &["Docs/*"], &[], "https://ex.com/docs/a", false, false),
        // The Origin page is crawled for its links even when no include stores it.
        row(ROOT, &["docs/*"], &[], "https://ex.com/", false, true),
        // Patterns match the percent-encoded URL; their literal characters are encoded the same way.
        row(ROOT, &["my docs/*"], &[], "https://ex.com/my%20docs/a", true, true),
        row(ROOT, &["docs/é*"], &[], "https://ex.com/docs/%C3%A9a", true, true),
        row(ROOT, &["docs/v?"], &[], "https://ex.com/docs/v%C3%A9", true, true),
        row(ROOT, &["docs/[^a]*"], &[], "https://ex.com/docs/%5Ex", true, true),
        row(ROOT, &["docs/my%20*"], &[], "https://ex.com/docs/my%20a", true, true),
        row(ROOT, &["docs/é*"], &[], "https://ex.com/docs/%c3%a9a", true, true),
        // `=` or `&` after a `?` in a later segment is a path character, not a query string.
        row(ROOT, &["wiki/v?/a=b"], &[], "https://ex.com/wiki/v1/a=b", true, true),
        // An absolute pattern's scheme and host are case-insensitive; a default port is implied.
        row(ROOT, &["HTTPS://Ex.com/docs"], &[], "https://ex.com/docs/a", true, true),
        row(ROOT, &["https://ex.com:443/docs/*"], &[], "https://ex.com/docs/a", true, true),
    ];

    /// Whether spider's own allow-list check admits `url` for a crawler configured from `scope`,
    /// and which of its compiled sets ("whitelist", "blacklist") came out empty: spider silently
    /// drops a set that fails to compile.
    fn spider_admits(origin: &Url, scope: &WebScope, url: &str) -> (bool, Vec<&'static str>) {
        let mut site = configure_website(origin, scope, 100, 10, false);
        site.configuration.configure_allowlist();
        let mut dropped = Vec::new();
        if site.configuration.get_whitelist_compiled().is_empty() {
            dropped.push("whitelist");
        }
        if site.configuration.get_blacklist_compiled().is_empty() {
            dropped.push("blacklist");
        }
        let status = site.is_allowed_default(&CaseInsensitiveString::from(url));
        (status == ProcessLinkStatus::Allowed, dropped)
    }

    #[test]
    fn crawl_and_store_filters_agree() {
        let mut failures = Vec::new();
        for r in ROWS {
            let origin = Url::parse(r.origin).expect("origin");
            let includes: Vec<String> = r.includes.iter().map(|s| s.to_string()).collect();
            let excludes: Vec<String> = r.excludes.iter().map(|s| s.to_string()).collect();
            let scope = match WebScope::new(&origin, &includes, &excludes) {
                Ok(scope) => scope,
                Err(err) => {
                    failures.push(format!(
                        "{:?} {:?} {}: scope error {err}",
                        r.includes, r.excludes, r.url
                    ));
                    continue;
                }
            };
            let url = Url::parse(r.url).expect("url");
            let store = scope.should_store(&url);
            let (crawl, dropped) = spider_admits(&origin, &scope, r.url);
            if store != r.store || crawl != r.crawl || !dropped.is_empty() {
                failures.push(format!(
                    "inc={:?} exc={:?} {} -> store {store} (want {}), crawl {crawl} (want {}), spider dropped {dropped:?}",
                    r.includes, r.excludes, r.url, r.store, r.crawl
                ));
            }
            if store && !crawl {
                failures.push(format!("  ^ storable but never crawled: {}", r.url));
            }
        }
        assert!(
            failures.is_empty(),
            "{} scope rows disagree:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// A relative pattern's regex, run on the percent-encoded path a real URL has, means what
    /// `glob::Pattern::matches` means on the decoded path.
    #[test]
    fn relative_pattern_regex_matches_like_glob() {
        let patterns = [
            "docs/*",
            "docs/*.md",
            "*.md",
            "docs/v?",
            "?ocs/*",
            "docs/**",
            "docs/**/x",
            "**/x",
            "**",
            "a/**/**/b",
            "docs/[!d]*",
            "docs/[^a]*",
            "docs/[]a]*",
            "docs/[!]a]*",
            "docs/[a-c]*",
            "docs/[z-a]*",
            "docs/[z-ab]*",
            "docs/[!z-a]*",
            "docs/[a-]*",
            "docs/[-a]*",
            "docs/[.]*",
            "docs/[*]x",
            "d.cs/(x)|+{1}$^",
            "docs/[\\d]*",
            "docs/[a&&b]*",
            "docs/[~~]*",
            "Docs/*",
            "docs/é*",
            "docs/*/index",
            "my docs/*",
            "docs/?",
            "docs/??",
            "docs/[!a]",
            "docs/[!a]*",
            "docs/[é]x",
            "docs/[{a]x",
        ];
        let paths = [
            "docs",
            "docs/",
            "docs/x.md",
            "docs/x.mdx",
            "docs/a/x.md",
            "x.md",
            "docs/v1",
            "docs/v12",
            "docs/a/b",
            "docs/x",
            "docs/a/b/x",
            "docs/ax",
            "x",
            "a/x",
            "a/b",
            "a/c/b",
            "a//b",
            "docs/api",
            "docs/draft",
            "docs/bpi",
            "docs/^x",
            "docs/]x",
            "docs/b",
            "docs/z",
            "docs/-x",
            "docs/.x",
            "docs/*x",
            "docs/ax",
            "d.cs/(x)|+{1}$^",
            "docs/dx",
            "docs/&x",
            "docs/~x",
            "Docs/a",
            "docs/éa",
            "docs/é",
            "docs/éé",
            "docs/ x",
            "docs/{x",
            "docs/a",
            "my docs/a",
            "docs/a/index",
            "docs/a/b/index",
            "",
        ];
        let mut failures = Vec::new();
        for pattern in patterns {
            let glob = glob::Pattern::new(pattern).expect("valid glob");
            let super::UrlPattern::Relative(body) =
                super::UrlPattern::parse(pattern).expect("parse")
            else {
                panic!("{pattern} parsed as absolute");
            };
            let regex = regex::Regex::new(&format!("^{body}")).expect("regex");
            for path in paths {
                let mut url = Url::parse(ROOT).expect("root");
                url.set_path(path);
                let encoded = url.path().trim_start_matches('/');
                let (by_glob, by_regex) = (glob.matches(path), regex.is_match(encoded));
                if by_glob != by_regex {
                    failures.push(format!(
                        "{pattern} on {path:?} ({encoded}): glob {by_glob}, regex {by_regex}"
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn patterns_glob_rejects_are_validation_errors() {
        let origin = Url::parse(ROOT).expect("origin");
        for bad in [
            "docs/**x",
            "a***",
            "docs/[",
            "docs/[!]",
            "  ",
            "/",
            // A query string: patterns match the path only.
            "https://ex.com/search?q=rust",
            "search?q=*",
            "list?a=1&b=*",
            "docs/v?/x?q=1",
            // Classes that a percent-encoded path cannot express as one character.
            "docs/[!é]*",
            "docs/[ -~]*",
        ] {
            let err = WebScope::new(&origin, &[bad.to_string()], &[]).expect_err(bad);
            assert!(
                matches!(err, crate::error::VaultError::Validation(_)),
                "{bad}: {err}"
            );
            let err = super::validate_scope_patterns(ROOT, &[], &[bad.to_string()]).expect_err(bad);
            assert!(
                matches!(err, crate::error::VaultError::Validation(_)),
                "{bad}: {err}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::{Arc, RwLock};

    use axum::extract::State;
    use axum::http::{header, StatusCode};
    use axum::response::{Html, IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use std::time::Duration;
    use tempfile::tempdir;

    use tokio::net::TcpListener;

    use crate::config::VaultConfig;
    use crate::db::open_rw_db;
    use crate::ingest::persist::SyncPages;
    use crate::ingest::SourceKind;
    use crate::sources::{AddSourceParams, Added, SourceLifecycle};

    type FixtureRouteMap = Arc<RwLock<HashMap<String, (StatusCode, String, String)>>>;

    #[derive(Clone, Default)]
    struct FixtureState {
        routes: FixtureRouteMap,
        /// Every request served, as (path, status, user-agent), for diagnosing failures.
        requests: Arc<RwLock<Vec<(String, u16, String)>>>,
    }

    impl FixtureState {
        fn set_html(&self, path: &str, status: StatusCode, body: &str) {
            let mut guard = self.routes.write().expect("lock routes");
            guard.insert(
                path.to_string(),
                (
                    status,
                    "text/html; charset=utf-8".to_string(),
                    body.to_string(),
                ),
            );
        }

        fn set_text(&self, path: &str, status: StatusCode, body: &str) {
            let mut guard = self.routes.write().expect("lock routes");
            guard.insert(
                path.to_string(),
                (
                    status,
                    "text/plain; charset=utf-8".to_string(),
                    body.to_string(),
                ),
            );
        }

        fn set_markdown(&self, path: &str, status: StatusCode, body: &str) {
            let mut guard = self.routes.write().expect("lock routes");
            guard.insert(
                path.to_string(),
                (
                    status,
                    "text/markdown; charset=utf-8".to_string(),
                    body.to_string(),
                ),
            );
        }

        fn set_xml(&self, path: &str, status: StatusCode, body: &str) {
            let mut guard = self.routes.write().expect("lock routes");
            guard.insert(
                path.to_string(),
                (
                    status,
                    "application/xml; charset=utf-8".to_string(),
                    body.to_string(),
                ),
            );
        }

        fn remove(&self, path: &str) {
            let mut guard = self.routes.write().expect("lock routes");
            guard.remove(path);
        }
    }

    async fn serve_fixture_route(
        State(state): State<FixtureState>,
        uri: axum::http::Uri,
        headers: axum::http::HeaderMap,
    ) -> Response {
        let path = uri.path().to_string();
        let entry = {
            let guard = state.routes.read().expect("lock routes");
            guard.get(&path).cloned()
        };
        let agent = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .chars()
            .take(40)
            .collect::<String>();
        state.requests.write().expect("lock requests").push((
            uri.to_string(),
            entry.as_ref().map_or(404, |(status, _, _)| status.as_u16()),
            agent,
        ));
        match entry {
            Some((status, content_type, body)) => (
                status,
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::ETAG, format!("\"etag-{path}\"")),
                    (
                        header::LAST_MODIFIED,
                        "Wed, 21 Oct 2026 07:28:00 GMT".to_string(),
                    ),
                ],
                body,
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, Html("<h1>404 Not Found</h1>")).into_response(),
        }
    }

    async fn spawn_fixture_server(
        state: FixtureState,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/", get(serve_fixture_route))
            .route("/{*path}", get(serve_fixture_route))
            .with_state(state);

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
    async fn crawls_local_fixture_site_within_prefix_and_extracts_markdown() {
        let state = FixtureState::default();
        state.set_text("/robots.txt", StatusCode::OK, "User-agent: *\nAllow: /\n");
        state.set_html(
            "/docs/",
            StatusCode::OK,
            r#"<!doctype html>
<html>
<head>
  <title>Docs Home</title>
  <style>.banner { display: none; /* noise-style-token */ }</style>
</head>
<body>
  <nav><a href="/blog/off-topic">Blog</a><a href="/docs-extra">Extra</a></nav>
  <main>
    <h1>Documentation</h1>
    <p>Welcome to the documentation hub with detailed reference material.</p>
    <ul>
      <li><a href="/docs/guide">Guide</a></li>
      <li><a href="/docs/api">API</a></li>
    </ul>
  </main>
  <footer>Site Footer Noise</footer>
  <script>console.log("noise-script-token");</script>
</body>
</html>"#,
        );
        state.set_html(
            "/docs/guide",
            StatusCode::OK,
            r#"<!doctype html>
<html>
<head><title>User Guide</title></head>
<body>
  <nav><a href="/blog/off-topic">Blog</a></nav>
  <main>
    <h1>User Guide</h1>
    <p>Getting started steps for setting up your local vault quickly.</p>
    <h2>Configuration</h2>
    <p>Set environment variables cleanly before running sync.</p>
  </main>
  <footer>Guide Footer Noise</footer>
</body>
</html>"#,
        );
        state.set_html(
            "/docs/api",
            StatusCode::OK,
            r#"<!doctype html>
<html>
<head><title>API Reference</title></head>
<body>
  <main>
    <h1>API Reference</h1>
    <p>HTTP endpoints reference for querying indexed documents.</p>
    <h3>Authentication</h3>
    <p>Use bearer tokens for remote API requests.</p>
  </main>
</body>
</html>"#,
        );
        state.set_html(
            "/blog/off-topic",
            StatusCode::OK,
            "<html><body><h1>Off Topic Blog</h1><p>Should never be ingested.</p></body></html>",
        );
        state.set_html(
            "/docs-extra",
            StatusCode::OK,
            "<html><body><h1>Docs Extra</h1><p>Sibling prefix must not be ingested.</p></body></html>",
        );

        let (addr, server_handle) = spawn_fixture_server(state).await;
        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();
        let origin = format!("http://{addr}/docs/");

        let Added {
            source,
            summary: summary_opt,
        } = SourceLifecycle::new(cfg.clone())
            .add(
                AddSourceParams {
                    origin: origin.clone(),
                    name: Some("fixture-docs".to_string()),
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("add should succeed");
        server_handle.abort();

        assert_eq!(source.kind, SourceKind::Web);
        assert_eq!(source.status, "ok");

        let summary = summary_opt.expect("sync summary should be present");
        assert_eq!(summary.source_name, "fixture-docs");
        assert_eq!(summary.pages_added, 3);
        assert_eq!(summary.pages_failed, 0);
        assert_eq!(summary.pages_stale, 0);

        let conn = open_rw_db(&cfg.db_path()).expect("open_rw_db");
        let rows: Vec<(String, String, String, String)> = conn
            .prepare(
                "SELECT uri, title, content_md, status
                 FROM pages
                 WHERE source_id = ?1
                 ORDER BY uri ASC",
            )
            .expect("prepare")
            .query_map([source.id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect");

        assert_eq!(rows.len(), 3);
        let uris: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        assert!(uris.contains(&format!("http://{addr}/docs/").as_str()));
        assert!(uris.contains(&format!("http://{addr}/docs/guide").as_str()));
        assert!(uris.contains(&format!("http://{addr}/docs/api").as_str()));
        assert!(!uris.iter().any(|u| u.contains("/blog/off-topic")));
        assert!(!uris.iter().any(|u| u.contains("/docs-extra")));

        for (uri, title, md, status) in &rows {
            assert_eq!(status, "active", "page {uri} should be active");
            assert!(!title.trim().is_empty(), "page {uri} must have title");
            assert!(
                !md.contains("Footer Noise")
                    && !md.contains("noise-script-token")
                    && !md.contains("noise-style-token"),
                "page {uri} leaked stripped noise into markdown: {md}"
            );
        }

        let section_paths: Vec<String> = conn
            .prepare("SELECT heading_path FROM sections WHERE source_id = ?1 ORDER BY id ASC")
            .expect("prepare sections")
            .query_map([source.id], |r| r.get(0))
            .expect("query sections")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect sections");

        assert!(
            section_paths
                .iter()
                .any(|p| p == "User Guide > Configuration"),
            "expected H1 > H2 breadcrumb in sections, got: {section_paths:?}"
        );
        assert!(
            section_paths
                .iter()
                .any(|p| p == "API Reference > Authentication"),
            "expected H1 > H3 breadcrumb in sections, got: {section_paths:?}"
        );
    }

    #[tokio::test]
    async fn seeds_urls_from_llms_txt_when_present() {
        let state = FixtureState::default();
        let (addr, server_handle) = spawn_fixture_server(state.clone()).await;

        state.set_html(
            "/docs/",
            StatusCode::OK,
            r#"<html><head><title>Docs</title></head><body><main><h1>Docs</h1><p>Index page with link to guide only.</p><a href="/docs/guide">Guide</a></main></body></html>"#,
        );
        state.set_html(
            "/docs/guide",
            StatusCode::OK,
            r#"<html><head><title>Guide</title></head><body><main><h1>Guide</h1><p>Standard linked guide content.</p></main></body></html>"#,
        );
        state.set_html(
            "/docs/hidden",
            StatusCode::OK,
            r#"<html><head><title>Hidden Doc</title></head><body><main><h1>Hidden Guide</h1><p>Unlinked page discovered via llms.txt tokenllms.</p></main></body></html>"#,
        );
        state.set_html(
            "/blog/secret",
            StatusCode::OK,
            r#"<html><body><h1>Secret Blog</h1><p>Out of prefix scope.</p></body></html>"#,
        );
        state.set_text(
            "/llms.txt",
            StatusCode::OK,
            &format!(
                "# Fixture Docs\n\
                 - [Guide](/docs/guide): Main guide\n\
                 - [Hidden](http://{addr}/docs/hidden): Hidden page\n\
                 - [Blog](/blog/secret): Out of scope\n"
            ),
        );

        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();
        let origin = format!("http://{addr}/docs/");

        let Added {
            source,
            summary: summary_opt,
        } = SourceLifecycle::new(cfg.clone())
            .add(
                AddSourceParams {
                    origin,
                    name: Some("llms-docs".to_string()),
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("add should succeed");
        server_handle.abort();

        let summary = summary_opt.expect("sync summary");
        assert_eq!(summary.pages_added, 3);

        let conn = open_rw_db(&cfg.db_path()).expect("open_rw_db");
        let uris: Vec<String> = conn
            .prepare("SELECT uri FROM pages WHERE source_id = ?1 ORDER BY uri")
            .expect("prepare")
            .query_map([source.id], |r| r.get(0))
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect");

        assert!(uris.contains(&format!("http://{addr}/docs/hidden")));
        assert!(!uris.iter().any(|u| u.contains("/blog/secret")));

        let fts_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sections_fts WHERE sections_fts MATCH 'tokenllms'",
                [],
                |r| r.get(0),
            )
            .expect("query sections_fts");
        assert_eq!(fts_hits, 1);
    }

    #[tokio::test]
    async fn recrawl_records_diff_marks_removed_stale_and_500_error() {
        let state = FixtureState::default();
        state.set_html(
            "/docs/",
            StatusCode::OK,
            r#"<html><head><title>Docs</title></head><body><nav><a href="/docs/guide">Guide</a><a href="/docs/api">API</a><a href="/docs/broken">Broken</a></nav><main><h1>Docs</h1><p>Stable home page.</p></main></body></html>"#,
        );
        state.set_html(
            "/docs/guide",
            StatusCode::OK,
            r#"<html><head><title>Guide</title></head><body><main><h1>Guide</h1><p>Initial guide paragraph for v1.</p></main></body></html>"#,
        );
        state.set_html(
            "/docs/api",
            StatusCode::OK,
            r#"<html><head><title>API</title></head><body><main><h1>API</h1><p>API endpoints v1.</p></main></body></html>"#,
        );
        state.set_html(
            "/docs/broken",
            StatusCode::OK,
            r#"<html><head><title>Broken</title></head><body><main><h1>Broken Page</h1><p>Healthy cached content before outage.</p></main></body></html>"#,
        );

        let (addr, server_handle) = spawn_fixture_server(state.clone()).await;
        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();
        let origin = format!("http://{addr}/docs/");

        let Added {
            source,
            summary: first_sum,
        } = SourceLifecycle::new(cfg.clone())
            .add(
                AddSourceParams {
                    origin,
                    name: Some("recrawl-docs".to_string()),
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("initial crawl");
        assert_eq!(first_sum.expect("first summary").pages_added, 4);

        // Phase 2: modify /docs/guide, unlink & remove /docs/api, return 500 on /docs/broken
        state.set_html(
            "/docs/",
            StatusCode::OK,
            r#"<html><head><title>Docs</title></head><body><nav><a href="/docs/guide">Guide</a><a href="/docs/broken">Broken</a></nav><main><h1>Docs</h1><p>Stable home page.</p></main></body></html>"#,
        );
        state.set_html(
            "/docs/guide",
            StatusCode::OK,
            r#"<html><head><title>Guide</title></head><body><main><h1>Guide</h1><p>Updated guide paragraph for v2 with new details.</p></main></body></html>"#,
        );
        state.remove("/docs/api");
        state.set_html(
            "/docs/broken",
            StatusCode::INTERNAL_SERVER_ERROR,
            "<html><body><h1>500 Internal Server Error</h1></body></html>",
        );

        let second = SourceLifecycle::new(cfg.clone())
            .sync("recrawl-docs", false)
            .await
            .expect("recrawl sync")
            .synced()
            .expect("synced");
        server_handle.abort();

        assert_eq!(second.pages_added, 0);
        assert_eq!(second.pages_updated, 1);
        assert_eq!(second.pages_unchanged, 1);
        assert_eq!(second.pages_stale, 1);
        assert_eq!(second.pages_failed, 1);

        let conn = open_rw_db(&cfg.db_path()).expect("open_rw_db");

        // Verify page_revisions diff for /docs/guide
        let diff: String = conn
            .query_row(
                "SELECT pr.unified_diff
                 FROM page_revisions pr
                 JOIN pages p ON p.id = pr.page_id
                 WHERE p.source_id = ?1 AND p.uri = ?2",
                rusqlite::params![source.id, format!("http://{addr}/docs/guide")],
                |r| r.get(0),
            )
            .expect("page_revisions row for /docs/guide");
        assert!(diff.contains("-Initial guide paragraph for v1."));
        assert!(diff.contains("+Updated guide paragraph for v2 with new details."));

        // Verify /docs/api is marked stale
        let api_status: String = conn
            .query_row(
                "SELECT status FROM pages WHERE source_id = ?1 AND uri = ?2",
                rusqlite::params![source.id, format!("http://{addr}/docs/api")],
                |r| r.get(0),
            )
            .expect("api status");
        assert_eq!(api_status, "stale");

        // Verify /docs/broken is marked error with http_status=500 and preserved content_md
        let (broken_status, broken_http, broken_err, broken_md): (
            String,
            Option<i64>,
            Option<String>,
            String,
        ) = conn
            .query_row(
                "SELECT status, http_status, error_message, content_md
                 FROM pages
                 WHERE source_id = ?1 AND uri = ?2",
                rusqlite::params![source.id, format!("http://{addr}/docs/broken")],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("broken page row");
        assert_eq!(broken_status, "error");
        assert_eq!(broken_http, Some(500));
        assert!(broken_err.is_some());
        assert!(
            broken_md.contains("Healthy cached content before outage."),
            "previous content_md must be preserved on error, got: {broken_md}"
        );
    }

    #[tokio::test]
    async fn render_js_without_chrome_feature_returns_clear_warning() {
        #[cfg(feature = "chrome")]
        let _chrome = CHROME_TEST_LOCK.lock().await;
        let state = FixtureState::default();
        state.set_html(
            "/docs/",
            StatusCode::OK,
            "<html><head><title>Static Page</title></head><body><main><h1>Static Page</h1><p>Static HTML fallback body.</p></main></body></html>",
        );

        let (addr, server_handle) = spawn_fixture_server(state).await;
        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();
        let origin = format!("http://{addr}/docs/");

        let Added {
            source,
            summary: summary_opt,
        } = SourceLifecycle::new(cfg.clone())
            .add(
                AddSourceParams {
                    origin,
                    name: Some("js-docs".to_string()),
                    render_js: true,
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("add should fall back or succeed gracefully");
        server_handle.abort();

        assert!(source.render_js);
        let summary = summary_opt.expect("summary");

        if !cfg!(feature = "chrome") {
            assert_eq!(summary.pages_added, 1);
            assert!(
                !summary.warnings.is_empty(),
                "expected actionable warning when render_js=true without chrome feature"
            );
            let combined = summary.warnings.join("\n").to_ascii_lowercase();
            assert!(
                combined.contains("chrome"),
                "warning must explain chrome feature requirement, got: {combined}"
            );
        }
    }

    /// Held by every test that launches Chrome, so browsers start one test at a time and do not
    /// starve timing-sensitive tests running in parallel.
    #[cfg(feature = "chrome")]
    static CHROME_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Fixture site whose only links to `/docs/a` and `/docs/b` are added by JavaScript after a
    /// short delay, like DevNet PubHub builds its sidebar from embedded JSON.
    fn js_links_fixture() -> FixtureState {
        let state = FixtureState::default();
        state.set_text("/robots.txt", StatusCode::OK, "User-agent: *\nAllow: /\n");
        state.set_html(
            "/docs/",
            StatusCode::OK,
            r#"<!doctype html><html><head><title>JS Docs</title></head><body><main>
<h1>JS Docs</h1><p>Index whose navigation is rendered by script.</p><nav id="sidebar"></nav>
</main><script>
setTimeout(function () {
  var toc = [{ title: "Alpha", url: "a" }, { title: "Beta", url: "b" }];
  var nav = document.getElementById("sidebar");
  toc.forEach(function (item) {
    var link = document.createElement("a");
    link.href = "/docs/" + item.url;
    link.textContent = item.title;
    nav.appendChild(link);
  });
}, 300);
</script></body></html>"#,
        );
        for (path, title) in [("/docs/a", "Alpha"), ("/docs/b", "Beta")] {
            state.set_html(
                path,
                StatusCode::OK,
                &format!(
                    "<html><head><title>{title}</title></head><body><main><h1>{title}</h1><p>{title} page reached through a script-built link.</p></main></body></html>"
                ),
            );
        }
        state
    }

    async fn add_js_links_source(render_js: bool) -> crate::ingest::SyncSummary {
        add_source_on(js_links_fixture(), render_js).await
    }

    async fn add_source_on(fixture: FixtureState, render_js: bool) -> crate::ingest::SyncSummary {
        let (addr, server_handle) = spawn_fixture_server(fixture.clone()).await;
        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();
        let Added { summary, .. } = SourceLifecycle::new(cfg)
            .add(
                AddSourceParams {
                    origin: format!("http://{addr}/docs/"),
                    name: Some("js-links".to_string()),
                    render_js,
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("add js-links source");
        server_handle.abort();
        let summary = summary.expect("summary");
        if summary.pages_failed > 0 {
            let conn = open_rw_db(&vault_dir.path().join("vault.db")).expect("open vault db");
            let mut stmt = conn
                .prepare("SELECT uri, http_status, error_message FROM pages WHERE status = 'error'")
                .expect("prepare");
            let failed: Vec<(String, Option<i64>, Option<String>)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .expect("query")
                .collect::<std::result::Result<_, _>>()
                .expect("rows");
            eprintln!("failed pages: {failed:?}");
            eprintln!(
                "fixture requests: {:#?}",
                fixture.requests.read().expect("lock requests")
            );
        }
        summary
    }

    #[tokio::test]
    async fn static_crawl_cannot_see_script_built_links() {
        let summary = add_js_links_source(false).await;
        assert_eq!(
            summary.pages_added, 1,
            "only the index is reachable without JS"
        );
    }

    #[cfg(feature = "chrome")]
    #[tokio::test(flavor = "multi_thread")]
    async fn render_js_crawl_follows_script_built_links() {
        let _chrome = CHROME_TEST_LOCK.lock().await;
        let summary = add_js_links_source(true).await;
        assert_eq!(
            summary.pages_added, 3,
            "render_js must wait for script-built links; warnings: {:?}",
            summary.warnings
        );
    }

    #[test]
    fn browser_client_error_is_kept_only_when_the_server_agrees() {
        use super::confirmed_status;
        use reqwest::StatusCode;

        let not_found = StatusCode::NOT_FOUND;
        assert_eq!(
            confirmed_status(not_found, Some(StatusCode::OK)),
            StatusCode::OK
        );
        assert_eq!(confirmed_status(not_found, Some(not_found)), not_found);
        assert_eq!(
            confirmed_status(not_found, Some(StatusCode::FORBIDDEN)),
            not_found
        );
        assert_eq!(confirmed_status(not_found, None), not_found);
    }

    #[tokio::test]
    async fn misreported_page_error_is_corrected_from_the_server() {
        use super::confirm_render_failures;
        use spider::page::build;
        use spider::utils::PageResponse;

        let state = FixtureState::default();
        state.set_html("/docs/", StatusCode::OK, "<html><body>fine</body></html>");
        let (addr, server_handle) = spawn_fixture_server(state).await;
        let origin = url::Url::parse(&format!("http://{addr}/docs/")).expect("origin");

        let page_with = |path: &str| {
            let mut page = build(
                &format!("http://{addr}{path}"),
                PageResponse {
                    content: Some(b"<html><body>rendered</body></html>".to_vec()),
                    ..Default::default()
                },
            );
            page.status_code = reqwest::StatusCode::NOT_FOUND;
            page
        };
        let mut pages = vec![page_with("/docs/"), page_with("/docs/missing")];
        confirm_render_failures(&mut pages, &origin).await;
        server_handle.abort();

        assert_eq!(
            pages[0].status_code,
            reqwest::StatusCode::OK,
            "server says 200"
        );
        assert_eq!(
            pages[1].status_code,
            reqwest::StatusCode::NOT_FOUND,
            "a real 404 stays a 404"
        );
    }

    #[tokio::test]
    async fn stall_watchdog_fires_only_after_pages_stop_arriving() {
        use super::CrawlActivity;

        let activity = CrawlActivity::start();
        let limit = Duration::from_millis(150);
        let keep_alive = activity.clone();
        let pages = tokio::spawn(async move {
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                keep_alive.touch();
            }
        });

        let started = std::time::Instant::now();
        activity.stalled_for(limit).await;
        let waited = started.elapsed();
        pages.await.expect("page task");

        // `touch` records whole milliseconds (truncated), so allow 2 ms of rounding.
        assert!(
            waited >= Duration::from_millis(300 + 150 - 2),
            "pages kept arriving for 300 ms, so the stall cannot fire before 450 ms: {waited:?}"
        );
    }

    #[cfg(feature = "chrome")]
    #[tokio::test(flavor = "multi_thread")]
    async fn render_js_stores_content_drawn_inside_shadow_dom() {
        let _chrome = CHROME_TEST_LOCK.lock().await;
        let state = FixtureState::default();
        state.set_text("/robots.txt", StatusCode::OK, "User-agent: *\nAllow: /\n");
        // Like DevNet's API reference: the body is rendered by a web component into shadow DOM,
        // nested one level, which a plain HTML read of the page does not include.
        state.set_html(
            "/docs/",
            StatusCode::OK,
            r#"<!doctype html><html><head><title>Shadow Docs</title></head><body>
<h1>Shadow Docs</h1><p>Light text lighttoken.</p><api-ref></api-ref>
<script>
setTimeout(function () {
  var outer = document.querySelector("api-ref").attachShadow({ mode: "open" });
  outer.innerHTML = "<style>p{color:red}</style><h2>Endpoint</h2><p>Outer body outertoken.</p><api-params></api-params>";
  var inner = outer.querySelector("api-params").attachShadow({ mode: "open" });
  inner.innerHTML = "<p>Nested parameter innertoken.</p>";
}, 200);
</script></body></html>"#,
        );
        let (addr, server_handle) = spawn_fixture_server(state).await;
        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();
        SourceLifecycle::new(cfg.clone())
            .add(
                AddSourceParams {
                    origin: format!("http://{addr}/docs/"),
                    name: Some("shadow".to_string()),
                    render_js: true,
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("add shadow source");
        server_handle.abort();

        let conn = open_rw_db(&cfg.db_path()).expect("open vault db");
        let content: String = conn
            .query_row("SELECT content_md FROM pages LIMIT 1", [], |row| row.get(0))
            .expect("stored page");
        for token in ["outertoken", "innertoken"] {
            assert!(content.contains(token), "missing {token} in: {content}");
        }
        assert!(
            !content.contains("color:red"),
            "styles must not be stored: {content}"
        );
    }

    /// Sync a one-page `render_js` Source serving `body` and return the stored Markdown.
    #[cfg(feature = "chrome")]
    async fn rendered_markdown(body: &str) -> String {
        let state = FixtureState::default();
        state.set_text(
            "/robots.txt",
            StatusCode::OK,
            "User-agent: *\nDisallow: /docs/x\n",
        );
        state.set_html("/docs/", StatusCode::OK, body);
        let (addr, server_handle) = spawn_fixture_server(state).await;
        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();
        SourceLifecycle::new(cfg.clone())
            .add(
                AddSourceParams {
                    origin: format!("http://{addr}/docs/"),
                    name: Some("rendered".to_string()),
                    render_js: true,
                    max_pages: Some(1),
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("add rendered source");
        server_handle.abort();
        let conn = open_rw_db(&cfg.db_path()).expect("open vault db");
        conn.query_row(
            "SELECT content_md FROM pages WHERE uri LIKE '%/docs/'",
            [],
            |row| row.get(0),
        )
        .expect("stored page")
    }

    #[cfg(feature = "chrome")]
    fn link_list(count: usize) -> String {
        (0..count)
            .map(|i| format!(r#"<li><a href="/docs/x{i}">Menu entry navtoken {i}</a></li>"#))
            .collect()
    }

    #[cfg(feature = "chrome")]
    #[tokio::test(flavor = "multi_thread")]
    async fn render_js_drops_a_big_navigation_list_next_to_real_content() {
        let _chrome = CHROME_TEST_LOCK.lock().await;
        let body = format!(
            "<html><head><title>Endpoint</title></head><body><div><ul>{}</ul></div>\
             <div><h1>Get Things</h1><p>{}</p></div></body></html>",
            link_list(60),
            "Returns the things bodytoken with enough explanation to count as content. ".repeat(6)
        );
        let content = rendered_markdown(&body).await;
        assert!(content.contains("bodytoken"), "{content}");
        assert!(
            !content.contains("navtoken"),
            "navigation must be dropped: {content}"
        );
    }

    #[cfg(feature = "chrome")]
    #[tokio::test(flavor = "multi_thread")]
    async fn render_js_keeps_a_link_list_that_is_the_page_content() {
        let _chrome = CHROME_TEST_LOCK.lock().await;
        let body = format!(
            "<html><head><title>Index</title></head><body><h1>Index</h1><ul>{}</ul></body></html>",
            link_list(60)
        );
        let content = rendered_markdown(&body).await;
        assert!(
            content.contains("navtoken 59"),
            "an index page keeps its list: {content}"
        );
    }

    #[cfg(feature = "chrome")]
    #[tokio::test(flavor = "multi_thread")]
    async fn render_js_follows_links_of_a_dropped_navigation_list() {
        let _chrome = CHROME_TEST_LOCK.lock().await;
        let state = FixtureState::default();
        state.set_text("/robots.txt", StatusCode::OK, "User-agent: *\nAllow: /\n");
        // A script-built sidebar big enough to count as a menu, next to real page content: it
        // is left out of the stored Markdown but its links must still be crawled.
        state.set_html(
            "/docs/",
            StatusCode::OK,
            &format!(
                r#"<!doctype html><html><head><title>Menu Docs</title></head><body>
<div id="sidebar"></div><main><h1>Menu Docs</h1><p>{}</p></main><script>
setTimeout(function () {{
  var list = document.createElement("ul");
  for (var i = 0; i < 40; i++) {{
    var item = document.createElement("li");
    var link = document.createElement("a");
    link.href = "/docs/p" + i;
    link.textContent = "Menu entry navtoken " + i;
    item.appendChild(link);
    list.appendChild(item);
  }}
  document.getElementById("sidebar").appendChild(list);
}}, 300);
</script></body></html>"#,
                "Body text bodytoken with enough explanation to count as content. ".repeat(6)
            ),
        );
        for i in 0..40 {
            state.set_html(
                &format!("/docs/p{i}"),
                StatusCode::OK,
                &format!(
                    "<html><head><title>P{i}</title></head><body><main><h1>P{i}</h1><p>Page {i} reached through the sidebar.</p></main></body></html>"
                ),
            );
        }
        let (addr, server_handle) = spawn_fixture_server(state).await;
        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();
        let Added { summary, .. } = SourceLifecycle::new(cfg.clone())
            .add(
                AddSourceParams {
                    origin: format!("http://{addr}/docs/"),
                    name: Some("menu".to_string()),
                    render_js: true,
                    max_pages: Some(3),
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("add menu source");
        server_handle.abort();
        let summary = summary.expect("summary");
        assert_eq!(
            summary.pages_added, 3,
            "sidebar links must still be followed; warnings: {:?}",
            summary.warnings
        );

        let conn = open_rw_db(&cfg.db_path()).expect("open vault db");
        let index: String = conn
            .query_row(
                "SELECT content_md FROM pages WHERE uri LIKE '%/docs/'",
                [],
                |row| row.get(0),
            )
            .expect("stored index");
        assert!(index.contains("bodytoken"), "{index}");
        assert!(
            !index.contains("navtoken"),
            "the menu must stay out of the Markdown: {index}"
        );
    }

    #[cfg(feature = "chrome")]
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_render_js_syncs_each_get_their_own_browser() {
        let _chrome = CHROME_TEST_LOCK.lock().await;
        crate::init_tracing();
        let (first, second) = tokio::join!(add_js_links_source(true), add_js_links_source(true));
        assert_eq!(first.pages_added, 3, "first sync: {first:?}");
        assert_eq!(second.pages_added, 3, "second sync: {second:?}");
    }

    #[cfg(feature = "chrome")]
    #[tokio::test(flavor = "multi_thread")]
    async fn render_js_without_reachable_browser_is_an_actionable_error() {
        let _chrome = CHROME_TEST_LOCK.lock().await;
        use super::{configure_website, ensure_render_browser, WebScope};
        use url::Url;

        let origin = Url::parse("https://example.com/docs/").expect("url");
        let scope = WebScope::new(&origin, &[], &[]).expect("scope");
        let mut website = configure_website(&origin, &scope, 5, 1, true);
        website.with_chrome_connection(Some("http://127.0.0.1:9".to_string()));

        let err = ensure_render_browser(&website)
            .await
            .expect_err("an unreachable browser must fail the Sync, not degrade silently");
        let message = err.to_string();
        assert!(
            message.contains("Chrome") && message.contains("CHROME_URL"),
            "error must say how to fix it, got: {message}"
        );
    }

    #[tokio::test]
    async fn seeds_from_sitemap_respects_robots_and_filters_patterns() {
        let state = FixtureState::default();
        let (addr, server_handle) = spawn_fixture_server(state.clone()).await;

        state.set_text(
            "/robots.txt",
            StatusCode::OK,
            "User-agent: *\nDisallow: /docs/private\n",
        );
        state.set_xml(
            "/sitemap.xml",
            StatusCode::OK,
            &format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\
                   <url><loc>http://{addr}/docs/from-sitemap</loc></url>\
                   <url><loc>http://{addr}/docs/private/secret</loc></url>\
                   <url><loc>http://{addr}/docs/drafts/wip</loc></url>\
                   <url><loc>http://{addr}/blog/news</loc></url>\
                 </urlset>"
            ),
        );
        state.set_html(
            "/docs/",
            StatusCode::OK,
            "<html><head><title>Home</title></head><body><main><h1>Home</h1><p>Docs landing page.</p></main></body></html>",
        );
        state.set_html(
            "/docs/from-sitemap",
            StatusCode::OK,
            "<html><head><title>Sitemap Page</title></head><body><main><h1>Sitemap Page</h1><p>Discovered via sitemap.xml tokensitemap.</p></main></body></html>",
        );
        state.set_html(
            "/docs/private/secret",
            StatusCode::OK,
            "<html><body><h1>Private</h1><p>Blocked by robots.txt.</p></body></html>",
        );
        state.set_html(
            "/docs/drafts/wip",
            StatusCode::OK,
            "<html><body><h1>WIP</h1><p>Excluded by pattern.</p></body></html>",
        );
        state.set_html(
            "/blog/news",
            StatusCode::OK,
            "<html><body><h1>Blog</h1><p>Outside prefix.</p></body></html>",
        );

        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();
        let origin = format!("http://{addr}/docs/");

        let Added {
            source,
            summary: summary_opt,
        } = SourceLifecycle::new(cfg.clone())
            .add(
                AddSourceParams {
                    origin,
                    name: Some("sitemap-docs".to_string()),
                    exclude_patterns: vec!["docs/drafts/*".to_string()],
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("sitemap add");
        server_handle.abort();

        let summary = summary_opt.expect("summary");
        assert_eq!(summary.pages_added, 2);

        let conn = open_rw_db(&cfg.db_path()).expect("open_rw_db");
        let uris: Vec<String> = conn
            .prepare("SELECT uri FROM pages WHERE source_id = ?1 ORDER BY uri")
            .expect("prepare")
            .query_map([source.id], |r| r.get(0))
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect");
        assert_eq!(
            uris,
            vec![
                format!("http://{addr}/docs/"),
                format!("http://{addr}/docs/from-sitemap"),
            ]
        );

        let (etag, last_mod): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT etag, last_modified FROM pages WHERE source_id = ?1 AND uri = ?2",
                rusqlite::params![source.id, format!("http://{addr}/docs/from-sitemap")],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("query validators");
        assert_eq!(etag.as_deref(), Some("\"etag-/docs/from-sitemap\""));
        assert_eq!(last_mod.as_deref(), Some("Wed, 21 Oct 2026 07:28:00 GMT"));
    }

    /// A served fixture site with a Source added on `http://{addr}/docs/` but not synced yet.
    struct UnsyncedSite {
        addr: SocketAddr,
        server: tokio::task::JoinHandle<()>,
        _vault_dir: tempfile::TempDir,
        cfg: VaultConfig,
        source: crate::ingest::SourceRecord,
    }

    async fn add_unsynced_docs_source(
        state: &FixtureState,
        params: AddSourceParams,
    ) -> UnsyncedSite {
        let (addr, server) = spawn_fixture_server(state.clone()).await;
        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();
        let Added { source, .. } = SourceLifecycle::new(cfg.clone())
            .add(
                AddSourceParams {
                    origin: format!("http://{addr}/docs/"),
                    ..params
                },
                false,
            )
            .await
            .expect("add without sync");
        UnsyncedSite {
            addr,
            server,
            _vault_dir: vault_dir,
            cfg,
            source,
        }
    }

    async fn sync(site: &UnsyncedSite, prune: bool) -> crate::ingest::SyncSummary {
        SourceLifecycle::new(site.cfg.clone())
            .sync(&site.source.name, prune)
            .await
            .expect("sync")
            .synced()
            .expect("synced")
    }

    /// (uri relative to the server, status, title, content_md) of every page, by uri.
    fn pages(site: &UnsyncedSite) -> Vec<(String, String, String, String)> {
        let base = format!("http://{}", site.addr);
        open_rw_db(&site.cfg.db_path())
            .expect("open_rw_db")
            .prepare(
                "SELECT uri, status, title, content_md FROM pages WHERE source_id = ?1 ORDER BY uri",
            )
            .expect("prepare")
            .query_map([site.source.id], |r| {
                let uri: String = r.get(0)?;
                Ok((
                    uri.trim_start_matches(&base).to_string(),
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                ))
            })
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect")
    }

    fn active_uris(site: &UnsyncedSite) -> Vec<String> {
        pages(site)
            .into_iter()
            .filter(|p| p.1 == "active")
            .map(|p| p.0)
            .collect()
    }

    /// Crawl a web Source and keep the accounting record a production Sync logs.
    async fn collect_accounted(
        source: &crate::ingest::SourceRecord,
    ) -> (crate::error::Result<SyncPages>, super::WebSyncAccounting) {
        let (origin, scope) = super::web_origin_and_scope(source).expect("checked origin");
        super::collect_with_accounting(source, false, &origin, &scope).await
    }

    /// Write crawled pages the way a Sync does: through the Source lifecycle.
    async fn write_collected(site: &UnsyncedSite, pages: SyncPages) -> crate::ingest::SyncSummary {
        SourceLifecycle::new(site.cfg.clone())
            .with_driver(crate::sources::FakeDriver::returning(pages))
            .sync(&site.source.name, false)
            .await
            .expect("sync")
            .synced()
            .expect("synced")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accounting_times_every_step_and_counts_later_crawls() {
        let state = FixtureState::default();
        state.set_html(
            "/docs/",
            StatusCode::OK,
            r#"<html><body><main><h1>Docs</h1><p>Root.</p><a href="/docs/a">A</a><a href="/docs/b">B</a></main></body></html>"#,
        );
        for (path, title) in [("/docs/a", "A"), ("/docs/b", "B")] {
            state.set_html(
                path,
                StatusCode::OK,
                &format!(
                    "<html><body><main><h1>{title}</h1><p>Linked page.</p></main></body></html>"
                ),
            );
        }
        for (path, title) in [("/docs/h1", "Hidden 1"), ("/docs/h2", "Hidden 2")] {
            state.set_html(
                path,
                StatusCode::OK,
                &format!(
                    r#"<html><body><main><h1>{title}</h1><p>Only in llms.txt.</p><a href="/docs/">Home</a></main></body></html>"#
                ),
            );
        }

        let site = add_unsynced_docs_source(
            &state,
            AddSourceParams {
                name: Some("accounted".to_string()),
                ..Default::default()
            },
        )
        .await;
        let addr = site.addr;
        state.set_text(
            "/llms.txt",
            StatusCode::OK,
            &format!(
                "# Docs\n\n- [Hidden 1](http://{addr}/docs/h1)\n- [Hidden 2](http://{addr}/docs/h2)\n"
            ),
        );

        let (pages, acc) = collect_accounted(&site.source).await;
        site.server.abort();
        let summary = write_collected(&site, pages.expect("crawl succeeds")).await;
        let requests = state.requests.read().expect("lock requests").clone();

        assert_eq!(acc.pages.later_crawls_started, 2, "{acc:?}\n{requests:?}");
        assert_eq!(acc.pages.already_collected, 6, "{acc:?}\n{requests:?}");
        assert_eq!(acc.pages.pages_received, 11, "{acc:?}\n{requests:?}");
        assert_eq!(acc.pages.lost_by_listener, 0);
        assert_eq!(acc.pages.results_stored, 5, "{summary:?}");
        for (step, value) in [
            ("discovery_ms", acc.discovery_ms),
            ("first_crawl_ms", acc.first_crawl_ms),
            ("later_crawls_ms", acc.later_crawls_ms),
            ("conversion_ms", acc.conversion_ms),
        ] {
            assert!(value.is_some(), "{step} not set: {acc:?}");
        }
        assert_eq!(
            acc.pages.pages_received,
            acc.pages.results_stored
                + acc.pages.results_failed
                + acc.pages.unparsable_url
                + acc.pages.already_collected
                + acc.pages.redirected_out_of_scope
                + acc.pages.url_not_storable
                + acc.pages.duplicate_result
                + acc.pages.markdown_twin
                + acc.pages.canonical_duplicate
                + acc.pages.url_alias
                + acc.pages.binary_file,
            "{acc:?}"
        );
    }

    /// The `web_sync_summary` line a web Sync's `on_written` hook logs for a write.
    fn summary_line_after_write(site: &UnsyncedSite, ok: bool, took: Duration) -> String {
        #[derive(Clone, Default)]
        struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("capture lock").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let hook = super::log_after_write(site.source.clone(), super::WebSyncAccounting::default());
        tracing::subscriber::with_default(subscriber, || hook(ok, took));
        let bytes = captured.0.lock().expect("capture lock").clone();
        String::from_utf8(bytes).expect("utf-8 log")
    }

    #[tokio::test]
    async fn summary_line_after_the_write_carries_ok_and_write_ms() {
        let site = add_unsynced_docs_source(
            &FixtureState::default(),
            AddSourceParams {
                name: Some("logged".to_string()),
                ..Default::default()
            },
        )
        .await;
        site.server.abort();
        for (ok, expected) in [(true, "ok=true"), (false, "ok=false")] {
            let line = summary_line_after_write(&site, ok, Duration::from_millis(7));
            assert!(line.contains("web_sync_summary"), "{line}");
            assert!(line.contains(expected), "{line}");
            assert!(line.contains("write_ms=7"), "{line}");
        }
    }

    #[test]
    fn failed_step_keeps_its_duration() {
        let mut slot = None;
        let result: crate::error::Result<()> = super::finish_step(
            &mut slot,
            std::time::Instant::now(),
            Err(crate::error::VaultError::Other("crawl stalled".to_string())),
        );
        assert!(result.is_err());
        assert!(
            slot.is_some(),
            "a step that ran and failed still has a duration"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cut_off_sync_with_dropped_page_marks_nothing_stale() {
        let state = FixtureState::default();
        state.set_html(
            "/docs/",
            StatusCode::OK,
            r#"<html><body><main><h1>Docs</h1><p>Root.</p><a href="/docs/p1">1</a><a href="/docs/p2">2</a><a href="/docs/p3">3</a><a href="/docs/p4">4</a><a href="/docs/p5">5</a></main></body></html>"#,
        );
        for i in 1..=5 {
            state.set_html(
                &format!("/docs/p{i}"),
                StatusCode::OK,
                &format!(
                    "<html><body><main><h1>Page {i}</h1><p>Body {i}.</p></main></body></html>"
                ),
            );
        }

        let site = add_unsynced_docs_source(
            &state,
            AddSourceParams {
                name: Some("cut-off".to_string()),
                include_patterns: vec!["docs/p*".to_string()],
                max_pages: Some(3),
                ..Default::default()
            },
        )
        .await;
        let cfg = &site.cfg;

        let archived = format!("http://{}/docs/archived", site.addr);
        let conn = open_rw_db(&cfg.db_path()).expect("open_rw_db");
        conn.execute(
            "INSERT INTO pages (source_id, uri, title, content_md, content_hash, word_count, status)
             VALUES (?1, ?2, 'Archived', '# Archived\n\nOld page.\n', 'hash-old', 3, 'active')",
            rusqlite::params![site.source.id, archived],
        )
        .expect("insert archived active page");
        drop(conn);

        let (pages, acc) = collect_accounted(&site.source).await;
        site.server.abort();
        let summary = write_collected(&site, pages.expect("crawl succeeds")).await;
        let requests = state.requests.read().expect("lock requests").clone();

        assert_eq!(
            acc.pages.url_not_storable, 1,
            "the Origin is dropped: {acc:?}"
        );
        assert_eq!(acc.pages.results_stored, 2, "{acc:?}\n{requests:?}");
        assert!(acc.pages.cut_off, "{acc:?}");
        assert_eq!(summary.pages_stale, 0, "{summary:?}");
        let status: String = open_rw_db(&cfg.db_path())
            .expect("open_rw_db")
            .query_row(
                "SELECT status FROM pages WHERE uri = ?1",
                [&archived],
                |r| r.get(0),
            )
            .expect("archived row");
        assert_eq!(status, "active");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn crawl_fetches_only_what_the_patterns_store() {
        let state = FixtureState::default();
        state.set_html(
            "/",
            StatusCode::OK,
            r#"<html><body><main><h1>Home</h1><p><a href="/docs/api">API</a> <a href="/docs/draft">Draft</a> <a href="/guide/x.md">MD</a> <a href="/guide/x.mdx">MDX</a> <a href="/img/LOGO.PNG">Logo</a></p></main></body></html>"#,
        );
        for path in [
            "/docs/api",
            "/docs/draft",
            "/guide/x.md",
            "/guide/x.mdx",
            "/img/LOGO.PNG",
        ] {
            state.set_html(
                path,
                StatusCode::OK,
                &format!("<html><body><main><h1>{path}</h1><p>Page.</p></main></body></html>"),
            );
        }
        let requests = state.requests.clone();
        let (addr, server_handle) = spawn_fixture_server(state).await;
        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();

        let Added { source, .. } = SourceLifecycle::new(cfg.clone())
            .add(
                AddSourceParams {
                    origin: format!("http://{addr}/"),
                    include_patterns: vec!["docs/[!d]*".to_string(), "guide/*.md".to_string()],
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("add and sync");

        let conn = open_rw_db(&cfg.db_path()).expect("open_rw_db");
        let uris: Vec<String> = conn
            .prepare("SELECT uri FROM pages WHERE source_id = ?1 ORDER BY uri")
            .expect("prepare")
            .query_map([source.id], |r| r.get(0))
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect");
        assert_eq!(
            uris,
            vec![
                format!("http://{addr}/docs/api"),
                format!("http://{addr}/guide/x.md")
            ]
        );

        let mut fetched: Vec<String> = requests
            .read()
            .expect("lock requests")
            .iter()
            .map(|(path, _, _)| path.clone())
            .filter(|path| path != "/" && !path.ends_with(".txt") && !path.ends_with(".xml"))
            .collect();
        fetched.sort();
        assert_eq!(
            fetched,
            vec!["/docs/api", "/guide/x.md"],
            "out-of-scope fetches"
        );
        server_handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn honors_max_pages_and_include_override() {
        let state = FixtureState::default();
        state.set_html(
            "/docs/",
            StatusCode::OK,
            r#"<html><body><nav><a href="/docs/p1">P1</a><a href="/docs/p2">P2</a><a href="/blog/included">Included</a><a href="/blog/ignored">Ignored</a></nav><main><h1>Docs</h1><p>Root page.</p></main></body></html>"#,
        );
        state.set_html(
            "/docs/p1",
            StatusCode::OK,
            "<html><body><main><h1>Page 1</h1><p>First page.</p></main></body></html>",
        );
        state.set_html(
            "/docs/p2",
            StatusCode::OK,
            "<html><body><main><h1>Page 2</h1><p>Second page.</p></main></body></html>",
        );
        state.set_html(
            "/blog/included",
            StatusCode::OK,
            "<html><body><main><h1>Included Blog</h1><p>Explicitly included outside /docs/.</p></main></body></html>",
        );
        state.set_html(
            "/blog/ignored",
            StatusCode::OK,
            "<html><body><main><h1>Ignored Blog</h1><p>Not included.</p></main></body></html>",
        );

        let (addr, server_handle) = spawn_fixture_server(state).await;
        let vault_dir = tempdir().expect("vault tempdir");
        let cfg = VaultConfig::new(vault_dir.path()).allowing_private_origins();

        // 1. Test include_patterns overriding /docs/ prefix to include /blog/included while ignoring /blog/ignored.
        let origin = format!("http://{addr}/docs/");
        let Added {
            source: inc_source,
            summary: inc_sum,
        } = SourceLifecycle::new(cfg.clone())
            .add(
                AddSourceParams {
                    origin: origin.clone(),
                    name: Some("include-override".to_string()),
                    include_patterns: vec!["docs/*".to_string(), "blog/included*".to_string()],
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("include override crawl");
        assert_eq!(inc_sum.expect("inc summary").pages_added, 4);

        let conn = open_rw_db(&cfg.db_path()).expect("open_rw_db");
        let inc_uris: Vec<String> = conn
            .prepare("SELECT uri FROM pages WHERE source_id = ?1 ORDER BY uri")
            .expect("prepare")
            .query_map([inc_source.id], |r| r.get(0))
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect");
        assert!(inc_uris.contains(&format!("http://{addr}/blog/included")));
        assert!(!inc_uris.iter().any(|u| u.contains("/blog/ignored")));

        // 2. Test the max_pages cap.
        let vault_dir2 = tempdir().expect("vault tempdir 2");
        let cfg2 = VaultConfig::new(vault_dir2.path()).allowing_private_origins();
        let lifecycle2 = SourceLifecycle::new(cfg2.clone());
        let Added {
            source,
            summary: summary_opt,
        } = lifecycle2
            .add(
                AddSourceParams {
                    origin,
                    name: Some("capped-docs".to_string()),
                    max_pages: Some(2),
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("capped add");

        let summary = summary_opt.expect("summary");
        assert_eq!(
            summary.pages_added, 2,
            "must cap at max_pages=2: {summary:?}"
        );

        let resync = lifecycle2
            .sync(&source.name, false)
            .await
            .expect("resync capped source")
            .synced()
            .expect("synced");
        // Which 2 of the 3 linked pages a capped crawl visits is not deterministic.
        assert_eq!(resync.pages_added + resync.pages_unchanged, 2);
        assert_eq!(resync.pages_stale, 0);

        // 3. Pages a capped crawl (max_pages=2) does not revisit must NOT be marked stale.
        let conn2 = open_rw_db(&cfg2.db_path()).expect("open_rw_db cfg2");
        conn2
            .execute(
                "INSERT INTO pages (source_id, uri, title, content_md, content_hash, word_count, status)
                 VALUES (?1, ?2, 'Archived', '# Archived\n\nOld page.\n', 'hash-old', 3, 'active')",
                rusqlite::params![source.id, format!("http://{addr}/docs/archived")],
            )
            .expect("insert extra active page outside cap");
        drop(conn2);

        let recapped = lifecycle2
            .sync(&source.name, false)
            .await
            .expect("recrawl capped source")
            .synced()
            .expect("synced");
        server_handle.abort();
        assert_eq!(
            recapped.pages_stale, 0,
            "capped crawl must not mark unvisited pages beyond max_pages as stale"
        );
    }

    mod markdown_twins {
        use axum::http::StatusCode;

        use super::{
            active_uris, add_unsynced_docs_source, pages, sync, FixtureState, UnsyncedSite,
        };
        use crate::db::open_rw_db;
        use crate::sources::AddSourceParams;

        const BANNER: &str = "This is documentation for an unreleased version banner";

        fn html_page(title: &str, alternate: Option<&str>, links: &[&str], body: &str) -> String {
            let alt = alternate
                .map(|href| format!(r#"<link rel="alternate" type="text/markdown" href="{href}">"#))
                .unwrap_or_default();
            let anchors: String = links
                .iter()
                .map(|href| format!(r#"<a href="{href}">{href}</a> "#))
                .collect();
            format!(
                "<!doctype html><html><head><title>{title} | Site</title>{alt}</head><body>\
                 <main><div class=\"banner\">{BANNER}</div><h1>{title}</h1><p>{body}</p>\
                 <p>{anchors}</p></main></body></html>"
            )
        }

        fn markdown_page(title: &str, body: &str) -> String {
            format!("---\ntitle: {title}\ndescription: about {title}\n---\n\n# {title}\n\n{body}\n")
        }

        /// A Docusaurus-like site: every HTML page links to its own `.md` copy, `llms.txt` lists the
        /// `.md` URLs, one `.md` has no HTML page, and one page is also served under `?version=2`.
        fn twin_site() -> FixtureState {
            let state = FixtureState::default();
            state.set_text("/robots.txt", StatusCode::OK, "User-agent: *\nAllow: /\n");
            state.set_html(
                "/docs/",
                StatusCode::OK,
                &html_page(
                    "Docs Home",
                    None,
                    &[
                        "/docs/guide",
                        "/docs/api",
                        "/docs/orphan.md",
                        "/docs/plain",
                        "/docs/plain?version=2",
                    ],
                    "Welcome to the documentation hub.",
                ),
            );
            state.set_html(
                "/docs/guide",
                StatusCode::OK,
                &html_page(
                    "User Guide",
                    Some("/docs/guide.md"),
                    &["/docs/guide.md"],
                    "Getting started steps for the guide.",
                ),
            );
            state.set_markdown(
                "/docs/guide.md",
                StatusCode::OK,
                &markdown_page("User Guide", "Getting started steps for the guide."),
            );
            // Minified HTML with unquoted attributes and no link tag: paired by URL suffix only.
            state.set_html(
                "/docs/api",
                StatusCode::OK,
                &format!(
                    "<html><head><title>API | Site</title><link rel=canonical href=/docs/api></head>\
                     <body><main><div>{BANNER}</div><h1>API</h1><p>HTTP endpoints reference.</p>\
                     </main></body></html>"
                ),
            );
            state.set_markdown(
                "/docs/api.md",
                StatusCode::OK,
                &markdown_page("API", "HTTP endpoints reference."),
            );
            state.set_markdown(
                "/docs/orphan.md",
                StatusCode::OK,
                &markdown_page("Orphan", "Only published as Markdown."),
            );
            // Served for both `/docs/plain` and `/docs/plain?version=2` (routes ignore the query).
            state.set_html(
                "/docs/plain",
                StatusCode::OK,
                "<html><head><title>Plain</title><link rel=\"canonical\" href=\"/docs/plain\"></head>\
                 <body><main><h1>Plain</h1><p>A page with no Markdown copy.</p></main></body></html>",
            );
            state.set_text(
                "/docs/llms.txt",
                StatusCode::OK,
                "# Docs\n\n- [User Guide](/docs/guide.md): guide\n- [API](/docs/api.md): api\n",
            );
            state
        }

        async fn add_site(state: FixtureState, max_pages: Option<u32>) -> UnsyncedSite {
            add_unsynced_docs_source(
                &state,
                AddSourceParams {
                    name: Some("twins".to_string()),
                    max_pages,
                    ..Default::default()
                },
            )
            .await
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn stores_one_page_per_markdown_twin() {
            let site = add_site(twin_site(), None).await;
            sync(&site, false).await;
            site.server.abort();

            assert_eq!(
                active_uris(&site),
                vec![
                    "/docs/",
                    "/docs/api",
                    "/docs/guide",
                    "/docs/orphan.md",
                    "/docs/plain"
                ],
                "{:#?}",
                pages(&site)
            );
            for (uri, _, title, md) in pages(&site) {
                if uri == "/docs/guide" || uri == "/docs/api" {
                    assert!(
                        !md.contains(BANNER),
                        "{uri} must hold the .md content: {md}"
                    );
                    assert!(
                        !title.contains("| Site"),
                        "{uri} title from frontmatter: {title}"
                    );
                }
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn resync_marks_stored_markdown_twins_stale() {
            // `Some(7)`: the first crawl reaches all 7 linked URLs, so the Sync is cut off and keeps
            // unvisited pages, yet the merged copies it did visit still go stale.
            for max_pages in [None, Some(7)] {
                let site = add_site(twin_site(), max_pages).await;
                let conn = open_rw_db(&site.cfg.db_path()).expect("open_rw_db");
                conn.execute(
                    "INSERT INTO pages (source_id, uri, title, content_md, content_hash, word_count, status)
                     VALUES (?1, ?2, 'Archived', '# Archived\n', 'hash-old', 1, 'active')",
                    rusqlite::params![site.source.id, format!("http://{}/docs/archived", site.addr)],
                )
                .expect("insert unvisited row");
                for path in ["/docs/guide.md", "/docs/api.md", "/docs/plain?version=2"] {
                    conn.execute(
                        "INSERT INTO pages (source_id, uri, title, content_md, content_hash, word_count, status)
                         VALUES (?1, ?2, 'Old', '# Old\n', 'hash-old', 1, 'active')",
                        rusqlite::params![site.source.id, format!("http://{}{path}", site.addr)],
                    )
                    .expect("insert old twin row");
                }
                drop(conn);

                sync(&site, false).await;
                let after = pages(&site);
                for path in ["/docs/guide.md", "/docs/api.md", "/docs/plain?version=2"] {
                    let row = after.iter().find(|p| p.0 == path);
                    assert_eq!(
                        row.map(|p| p.1.as_str()),
                        Some("stale"),
                        "max_pages={max_pages:?}: {path} must go stale: {after:#?}"
                    );
                }

                let archived = after
                    .iter()
                    .find(|p| p.0 == "/docs/archived")
                    .map(|p| p.1.as_str());
                let expected = if max_pages.is_some() {
                    "active"
                } else {
                    "stale"
                };
                assert_eq!(
                    archived,
                    Some(expected),
                    "max_pages={max_pages:?}: {after:#?}"
                );

                if max_pages.is_none() {
                    sync(&site, true).await;
                    let pruned = pages(&site);
                    assert!(
                        !pruned
                            .iter()
                            .any(|p| p.0.ends_with("guide.md") || p.0.contains('?')),
                        "--prune deletes merged pages: {pruned:#?}"
                    );
                }
                site.server.abort();
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn markdown_only_site_is_crawled_normally() {
            let state = FixtureState::default();
            state.set_text("/robots.txt", StatusCode::OK, "User-agent: *\nAllow: /\n");
            state.set_html(
                "/docs/",
                StatusCode::OK,
                &html_page(
                    "Index",
                    None,
                    &["/docs/a.md", "/docs/b.md"],
                    "Markdown files.",
                ),
            );
            state.set_markdown("/docs/a.md", StatusCode::OK, &markdown_page("A", "Alpha."));
            state.set_markdown("/docs/b.md", StatusCode::OK, &markdown_page("B", "Beta."));
            let site = add_site(state, None).await;
            sync(&site, false).await;
            site.server.abort();
            assert_eq!(
                active_uris(&site),
                vec!["/docs/", "/docs/a.md", "/docs/b.md"]
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn failed_twin_falls_back_to_the_other_copy() {
            let state = FixtureState::default();
            state.set_text("/robots.txt", StatusCode::OK, "User-agent: *\nAllow: /\n");
            state.set_html(
                "/docs/",
                StatusCode::OK,
                &html_page(
                    "Index",
                    None,
                    &["/docs/ok-html", "/docs/bad-html.md"],
                    "Two pages.",
                ),
            );
            // HTML fine, .md broken: keep the HTML content, no Error page for the .md.
            state.set_html(
                "/docs/ok-html",
                StatusCode::OK,
                &html_page(
                    "Ok Html",
                    Some("/docs/ok-html.md"),
                    &["/docs/ok-html.md"],
                    "Html body.",
                ),
            );
            state.set_text(
                "/docs/ok-html.md",
                StatusCode::INTERNAL_SERVER_ERROR,
                "boom",
            );
            // HTML broken, .md fine: no pairing, both handled as before.
            state.set_html("/docs/bad-html", StatusCode::NOT_FOUND, "<h1>gone</h1>");
            state.set_markdown(
                "/docs/bad-html.md",
                StatusCode::OK,
                &markdown_page("Bad Html", "Markdown survives."),
            );
            let site = add_site(state, None).await;
            sync(&site, false).await;
            site.server.abort();

            let rows = pages(&site);
            let status = |uri: &str| rows.iter().find(|p| p.0 == uri).map(|p| p.1.clone());
            assert_eq!(
                status("/docs/ok-html").as_deref(),
                Some("active"),
                "{rows:#?}"
            );
            assert_eq!(status("/docs/ok-html.md"), None, "{rows:#?}");
            assert_eq!(
                status("/docs/bad-html.md").as_deref(),
                Some("active"),
                "{rows:#?}"
            );
            let ok_md = &rows.iter().find(|p| p.0 == "/docs/ok-html").expect("row").3;
            assert!(ok_md.contains("Html body."), "{ok_md}");
        }
    }

    /// Issue #5: one Page per document across alias URLs (redirects, `x` and `x/`, `x/index`).
    /// spider refuses to follow a redirect to a loopback address (its SSRF guard), so the fixture
    /// site cannot serve one; redirects are covered with hand-made pages in `collected.rs`.
    mod url_aliases {
        use axum::http::StatusCode;

        use super::{
            active_uris, add_unsynced_docs_source, pages, sync, FixtureState, UnsyncedSite,
        };
        use crate::db::open_rw_db;
        use crate::sources::AddSourceParams;

        fn html(title: &str, canonical: Option<&str>, links: &[&str]) -> String {
            let canonical = canonical
                .map(|href| format!(r#"<link rel="canonical" href="{href}">"#))
                .unwrap_or_default();
            let anchors: String = links
                .iter()
                .map(|href| format!(r#"<a href="{href}">{href}</a> "#))
                .collect();
            format!(
                "<!doctype html><html><head><title>{title}</title>{canonical}</head><body>\
                 <main><h1>{title}</h1><p>All about {title}.</p><p>{anchors}</p></main>\
                 </body></html>"
            )
        }

        /// A docs site whose documents are reachable under alias URLs, as on dlthub.
        fn alias_site() -> FixtureState {
            let state = FixtureState::default();
            state.set_text("/robots.txt", StatusCode::OK, "User-agent: *\nAllow: /\n");
            state.set_html(
                "/docs/",
                StatusCode::OK,
                &html(
                    "Home",
                    None,
                    &[
                        "/docs/slash",
                        "/docs/slash/",
                        "/docs/idx",
                        "/docs/idx/index",
                        "/docs/plain",
                    ],
                ),
            );
            // Served with 200 both with and without the trailing slash, and as `/index`.
            let slash = html("Slash", None, &[]);
            state.set_html("/docs/slash", StatusCode::OK, &slash);
            state.set_html("/docs/slash/", StatusCode::OK, &slash);
            let idx = html("Idx", None, &[]);
            state.set_html("/docs/idx", StatusCode::OK, &idx);
            state.set_html("/docs/idx/index", StatusCode::OK, &idx);
            // Served for `/docs/plain` and the `?version=2` seed alike (routes ignore the query).
            state.set_html(
                "/docs/plain",
                StatusCode::OK,
                &html("Plain", Some("/docs/plain"), &[]),
            );
            state.set_text(
                "/docs/llms.txt",
                StatusCode::OK,
                "# Docs\n\n- [Plain](/docs/plain?version=2): plain\n",
            );
            state
        }

        const STORED: [&str; 4] = ["/docs/", "/docs/idx", "/docs/plain", "/docs/slash"];

        async fn add_site(max_pages: Option<u32>) -> UnsyncedSite {
            add_unsynced_docs_source(
                &alias_site(),
                AddSourceParams {
                    name: Some("aliases".to_string()),
                    max_pages,
                    ..Default::default()
                },
            )
            .await
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn stores_one_page_per_alias_group() {
            let site = add_site(None).await;
            sync(&site, false).await;
            site.server.abort();
            let all = pages(&site);
            assert_eq!(
                all.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
                STORED.to_vec(),
                "{all:#?}"
            );
            assert!(all.iter().all(|r| r.1 == "active"), "{all:#?}");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn stored_copy_is_stable_across_syncs() {
            let site = add_site(None).await;
            for round in 0..3 {
                let summary = sync(&site, false).await;
                assert_eq!(active_uris(&site), STORED.to_vec(), "round {round}");
                assert_eq!(summary.pages_stale, 0, "round {round}: {summary:?}");
            }
            site.server.abort();
        }

        const OLD_ALIASES: [&str; 3] = ["/docs/slash/", "/docs/idx/index", "/docs/plain?version=2"];

        fn insert_old_alias_rows(site: &UnsyncedSite) {
            let conn = open_rw_db(&site.cfg.db_path()).expect("open_rw_db");
            for path in OLD_ALIASES {
                conn.execute(
                    "INSERT INTO pages (source_id, uri, title, content_md, content_hash, word_count, status)
                     VALUES (?1, ?2, 'Old', '# Old\n', 'hash-old', 1, 'active')",
                    rusqlite::params![site.source.id, format!("http://{}{path}", site.addr)],
                )
                .expect("insert old alias row");
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn alias_rows_from_earlier_syncs_go_stale() {
            let site = add_site(None).await;
            insert_old_alias_rows(&site);
            sync(&site, false).await;
            let after = pages(&site);
            for path in OLD_ALIASES {
                let status = after.iter().find(|r| r.0 == path).map(|r| r.1.as_str());
                assert_eq!(status, Some("stale"), "{path}: {after:#?}");
            }
            assert_eq!(active_uris(&site), STORED.to_vec());

            sync(&site, true).await;
            site.server.abort();
            let pruned = pages(&site);
            assert_eq!(
                pruned.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
                STORED.to_vec(),
                "--prune deletes the alias rows: {pruned:#?}"
            );
        }

        /// The pages `alias_site`'s home links to, plus itself: the first crawl uses the budget up.
        const CUT_OFF_BUDGET: u32 = 6;

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn cut_off_sync_still_retires_visited_aliases() {
            // A budget the first crawl uses up: the Sync is cut off, so unvisited pages are kept,
            // yet the alias rows it did visit go stale.
            let site = add_site(Some(CUT_OFF_BUDGET)).await;
            insert_old_alias_rows(&site);
            let conn = open_rw_db(&site.cfg.db_path()).expect("open_rw_db");
            conn.execute(
                "INSERT INTO pages (source_id, uri, title, content_md, content_hash, word_count, status)
                 VALUES (?1, ?2, 'Archived', '# Archived\n', 'hash-arch', 1, 'active')",
                rusqlite::params![site.source.id, format!("http://{}/docs/archived", site.addr)],
            )
            .expect("insert unvisited row");
            drop(conn);
            sync(&site, false).await;
            site.server.abort();
            let after = pages(&site);
            let status = |path: &str| after.iter().find(|r| r.0 == path).map(|r| r.1.clone());
            assert_eq!(
                status("/docs/archived").as_deref(),
                Some("active"),
                "{after:#?}"
            );
            for path in ["/docs/slash/", "/docs/idx/index"] {
                assert_eq!(status(path).as_deref(), Some("stale"), "{path}: {after:#?}");
            }
        }
    }
}
