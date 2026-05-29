//! # search
//!
//! `heso search <query>` — first-class multi-source web search verb. Pure
//! HTTP + HTML parsing; no JS engine is spun up. Default engines are
//! Mojeek (an independent, scrape-friendly web index), DuckDuckGo HTML,
//! and the Wikipedia REST `summary` knowledge block — no API keys, no
//! signup. Optional SearXNG via `--searx-url` or `HESO_SEARX_URL`.
//!
//! DuckDuckGo's HTML endpoint rate-limits hard per IP and intermittently
//! serves its no-results landing page to scripted callers, so it is
//! treated as **best-effort**: a blocked or empty DDG response is not an
//! error, and Mojeek carries the result set so a search never comes back
//! empty just because DDG throttled this caller. Running both by default
//! is the redundancy that makes the verb reliable.
//!
//! ## Output shape (JSON to stdout)
//!
//! ```json
//! {
//!   "query": "rust web scraping",
//!   "engines_used": ["ddg", "mojeek", "wiki"],
//!   "results": [
//!     {"rank": 1, "title": "...", "url": "https://...",
//!      "snippet": "...", "source": "mojeek"}
//!   ],
//!   "knowledge": {
//!     "title": "Web scraping",
//!     "summary": "Web scraping is...",
//!     "url": "https://en.wikipedia.org/wiki/Web_scraping"
//!   },
//!   "errors": null
//! }
//! ```
//!
//! `knowledge` is `null` if no Wikipedia direct match. `errors` is
//! `null` when every engine succeeded, otherwise an array of
//! `{engine, message}`. `results` is capped by `--limit N` (default 30,
//! max 100).
//!
//! ## Backends
//!
//! 1. **Mojeek** (primary general-web backend, no key): GET
//!    `https://www.mojeek.com/search?q=<query>`. Mojeek runs its own
//!    independent crawl and index and does not gate scripted callers
//!    behind the aggressive per-IP rate limiting the big engines use, so
//!    it is the reliable backbone of the result set. Pagination via
//!    `s=N` (offset 1, 11, 21, 31 — first page omits `s`, then `+10` per
//!    additional page). Cap at 4 pages regardless of `--limit`. Each
//!    result is a `<ul class="results-standard"> <li>` carrying an
//!    `<a class="title" href="…">` (page title + direct href) and a
//!    `<p class="s">` snippet. Parse with [`scraper`] (workspace dep).
//!
//! 2. **DuckDuckGo HTML** (best-effort, no key): POST to
//!    `https://html.duckduckgo.com/html/` with form `q=<query>&l=us-en`.
//!    Pagination via `s=N` (offset 0, 10, 25, 40, 55 — first page is
//!    `s=0`, then `+15` per additional page, matching the `ddgs` Python
//!    library). Cap pagination at 4 pages regardless of `--limit`. Parse
//!    with [`scraper`] (workspace dep). The href on each result is wrapped
//!    in DDG's redirect (`//duckduckgo.com/l/?uddg=<urlencoded>&...`); we
//!    unwrap via `uddg=` query-param decode. DDG throttles scripted
//!    callers, so an empty page is treated as zero results, not an error.
//!
//! 3. **Wikipedia REST `summary`** (knowledge block, no key): GET
//!    `https://en.wikipedia.org/api/rest_v1/page/summary/<urlencoded>`.
//!    Returns JSON with `title`, `extract`, `content_urls.desktop.page`.
//!    404 → omitted; 200 with `type == "disambiguation"` → omitted (the
//!    page is a list of meanings, not a knowledge answer). Other errors
//!    → omitted with a stderr note.
//!
//! 4. **SearXNG** (optional, only if a URL is configured): GET
//!    `<base>/search?q=<q>&format=json`. Returns `{results: [{title, url,
//!    content, ...}]}` (the field name is `content`, not `snippet`).
//!    Mapped to our shape with `source: "searxng"`. Note: most public
//!    SearXNG instances disable JSON output by default — see
//!    <https://docs.searxng.org/dev/search_api.html>.
//!
//! ## Ranked merge
//!
//! Multiple engines → dedupe by canonical URL (lowercase host, strip
//! trailing `/` from path). Final order is round-robin from each
//! engine's top: DDG[0], Mojeek[0], Searx[0], DDG[1], Mojeek[1], ... so
//! when DDG comes back empty the merged list is simply Mojeek's (and
//! SearXNG's, if configured) in rank order. Wikipedia is NOT in the
//! results array — it goes in the top-level `knowledge` block.
//!
//! ## User-Agent
//!
//! Search uses a separate `reqwest::Client` from [`FetchEngine`] because
//! the engine identifies as `heso/<version>` (sensible for cooperative
//! sites that want to know who's calling), but DuckDuckGo's HTML
//! endpoint serves anti-bot pages to non-browser-shaped UAs. We send
//! the same `Mozilla/5.0 (compatible; heso/0.0.1)` string the JS
//! engine exposes via `navigator.userAgent` — one source of truth, and
//! it begins with `Mozilla/5.0` so naive sniffers don't reject it.
//!
//! ## Tests
//!
//! See `crates/heso-cli/tests/search.rs` — wiremock stubs the SearXNG
//! endpoint (the one backend whose base URL is configurable from
//! outside the binary) end-to-end; we verify merge order, dedupe,
//! limit, missing-knowledge handling, and the empty-results-no-crash
//! behaviour for nonsense queries. The Mojeek and DDG HTML parsers,
//! whose hosts aren't configurable, are unit-tested against fixtures in
//! the `tests` module below.

use std::collections::HashSet;
use std::process::ExitCode;
use std::time::Duration;

use reqwest::Client;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================================
// Constants
// ============================================================================

/// Browser-shaped User-Agent for search backends. Mirrors the value the
/// JS engine exposes via `navigator.userAgent` (see
/// `heso-engine-js::engine::install_browser_apis`). Begins with
/// `Mozilla/5.0` so anti-bot UA sniffers (Cloudflare et al., the DDG
/// HTML endpoint's own bot detector) accept the request; identifies
/// as heso parenthetically so cooperative operators can recognise us.
const BROWSER_UA: &str = "Mozilla/5.0 (compatible; heso/0.0.1)";

/// Default cap on merged result count when `--limit` is omitted.
/// Also the default for the JSON-RPC `search` method.
pub(crate) const DEFAULT_LIMIT: usize = 30;

/// Hard cap on merged result count. Higher values risk requesting
/// many DDG pages for very little marginal value — agents asking for
/// "top 1000" should be using a proper API instead.
pub(crate) const MAX_LIMIT: usize = 100;

/// Maximum number of DDG result pages we will request regardless of
/// `--limit`. Each page yields ~15 results; 4 pages ≈ 60 results,
/// matching the practical reach of the HTML endpoint. Going further
/// pulls in low-quality long-tail results AND risks DDG rate-limiting
/// the IP for the next while.
const MAX_DDG_PAGES: usize = 4;

/// Per-request timeout for any single HTTP call (DDG page, Wikipedia
/// summary, SearXNG `/search`). Total wall-clock is bounded by the
/// number of pages we request (≤ 4 DDG + 1 Wiki + 1 SearXNG ≈ 60 s
/// worst case). The CLI doesn't expose a flag for this — agents that
/// need different behaviour can shell out to `heso fetch` directly.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on a single search-backend response body. Wikipedia
/// summaries and SearXNG JSON for one query are kilobyte-sized; 4 MiB
/// is the headroom a hostile or misconfigured backend would need to
/// push the CLI into multi-hundred-MB allocations.
const MAX_SEARCH_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Read a search-backend response body, refusing payloads larger than
/// [`MAX_SEARCH_RESPONSE_BYTES`]. Streams chunks so a hostile
/// `Content-Length` header doesn't force a giant pre-alloc, and a
/// chunked response without a declared length is still capped.
async fn read_search_body_capped(
    resp: reqwest::Response,
    backend: &str,
) -> Result<String, String> {
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_SEARCH_RESPONSE_BYTES {
            return Err(format!(
                "{backend} response too large: declared {len} bytes (cap {MAX_SEARCH_RESPONSE_BYTES})"
            ));
        }
    }
    let mut acc: Vec<u8> = Vec::new();
    let mut resp = resp;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("{backend} body read failed: {e}"))?
    {
        if acc.len() + chunk.len() > MAX_SEARCH_RESPONSE_BYTES {
            return Err(format!(
                "{backend} response too large: exceeded {MAX_SEARCH_RESPONSE_BYTES} bytes"
            ));
        }
        acc.extend_from_slice(&chunk);
    }
    String::from_utf8(acc).map_err(|e| format!("{backend} response is not UTF-8: {e}"))
}

/// Environment variable name for the SearXNG base URL. Read by both
/// the CLI and the JSON-RPC `search` method when the request doesn't
/// supply `searx_url` / `--searx-url` explicitly.
pub const SEARX_URL_ENV: &str = "HESO_SEARX_URL";

// ============================================================================
// Public API — used by both `cmd_search` and the `search` RPC method
// ============================================================================

/// Backend identifiers carried in the `source` field of each result
/// and the top-level `engines_used` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Engine {
    Ddg,
    Mojeek,
    Wiki,
    SearxNg,
}

impl Engine {
    fn as_str(&self) -> &'static str {
        match self {
            Engine::Ddg => "ddg",
            Engine::Mojeek => "mojeek",
            Engine::Wiki => "wiki",
            Engine::SearxNg => "searxng",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Engine> {
        match s {
            "ddg" => Some(Engine::Ddg),
            "mojeek" => Some(Engine::Mojeek),
            "wiki" => Some(Engine::Wiki),
            "searxng" => Some(Engine::SearxNg),
            _ => None,
        }
    }
}

/// Parse the JSON-RPC `engines` parameter, accepting either a CSV
/// string (`"ddg,wiki"`) or a JSON array (`["ddg","wiki"]`). Both
/// shapes map to the same internal vector. Empty / null inputs
/// surface as an error — callers that want defaults should pass
/// `None` for the whole field, which `dispatch_search` handles
/// upstream.
pub(crate) fn parse_engines_value(v: &Value) -> Result<Vec<Engine>, String> {
    match v {
        Value::Null => Err("engines: empty list".to_owned()),
        Value::String(s) => parse_engines_csv(s),
        Value::Array(arr) => {
            let mut out = Vec::new();
            let mut seen = HashSet::new();
            for item in arr {
                let s = item.as_str().ok_or_else(|| {
                    format!("engines: array entries must be strings, got {item}")
                })?;
                let t = s.trim();
                if t.is_empty() {
                    continue;
                }
                let e = Engine::parse(t).ok_or_else(|| {
                    format!(
                        "engines: unknown engine `{t}` — supported: ddg, mojeek, wiki, searxng"
                    )
                })?;
                if seen.insert(t.to_owned()) {
                    out.push(e);
                }
            }
            if out.is_empty() {
                return Err("engines: list must contain at least one engine".to_owned());
            }
            Ok(out)
        }
        other => Err(format!(
            "engines: expected string or array of strings, got {other}"
        )),
    }
}

/// One row in the merged results array, before serialisation.
#[derive(Debug, Clone, Serialize)]
struct SearchResult {
    rank: usize,
    title: String,
    url: String,
    snippet: String,
    source: &'static str,
}

/// The top-level Wikipedia knowledge block (`null` when omitted).
#[derive(Debug, Clone, Serialize)]
struct KnowledgeBlock {
    title: String,
    summary: String,
    url: String,
}

/// One result row before merge — carries its source so the round-robin
/// merger can interleave correctly.
#[derive(Debug, Clone)]
struct RawResult {
    title: String,
    url: String,
    snippet: String,
    source: Engine,
}

/// Inputs accepted by both the CLI and the RPC. Constructed by
/// [`parse_cli_args`] for the binary and built directly by the
/// JSON-RPC dispatcher.
#[derive(Debug, Clone)]
pub(crate) struct SearchRequest {
    pub(crate) query: String,
    pub(crate) limit: usize,
    pub(crate) engines: Vec<Engine>,
    pub(crate) searx_url: Option<String>,
}

// (`SearchRequest` is constructed inline by `parse_cli_args` and
// `dispatch_search` — no constructor helper needed today.)

// ============================================================================
// CLI entry point
// ============================================================================

/// `heso search <query> [flags]` — entry point dispatched from `main()`.
pub async fn cmd_search(args: &[String]) -> ExitCode {
    let request = match parse_cli_args(args) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("{msg}");
            // Agents read stdout JSON; surface the argument error there
            // too. An empty `<query>` gets the dedicated `empty_query`
            // code; every other parse fault is a generic
            // `invalid_argument`.
            let code = if msg.contains("must not be empty") {
                "empty_query"
            } else {
                "invalid_argument"
            };
            return crate::emit_cli_error(code, &msg, 2);
        }
    };
    let value = match run_search(&request).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("search failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    // If every engine errored AND no knowledge block came back, the
    // request didn't actually accomplish anything — exit 1 so scripts
    // checking the return code don't treat an all-failed sweep as
    // success.
    let no_results = value
        .get("results")
        .and_then(|r| r.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true);
    let no_knowledge = value.get("knowledge").map(|k| k.is_null()).unwrap_or(true);
    let had_errors = value
        .get("errors")
        .map(|e| !e.is_null())
        .unwrap_or(false);
    let serialized = match serde_json::to_string_pretty(&value) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to serialize output: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("{serialized}");
    if no_results && no_knowledge && had_errors {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Parse the positional `<query>` and the `--limit / --engines /
/// --searx-url` flags, falling back to `HESO_SEARX_URL` when
/// `--searx-url` is omitted but `searxng` is in the engine list.
fn parse_cli_args(args: &[String]) -> Result<SearchRequest, String> {
    let mut query: Option<String> = None;
    let mut limit = DEFAULT_LIMIT;
    let mut engines = vec![Engine::Ddg, Engine::Mojeek, Engine::Wiki];
    let mut searx_url: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--limit" => {
                let raw = args.get(i + 1).ok_or_else(|| {
                    "--limit requires a value".to_owned()
                })?;
                let n: usize = raw.parse().map_err(|_| {
                    format!("--limit must be an integer, got `{raw}`")
                })?;
                if n == 0 {
                    return Err("--limit must be >= 1".to_owned());
                }
                limit = n.min(MAX_LIMIT);
                i += 2;
            }
            "--engines" => {
                let raw = args.get(i + 1).ok_or_else(|| {
                    "--engines requires a value".to_owned()
                })?;
                engines = parse_engines_csv(raw)?;
                i += 2;
            }
            "--searx-url" => {
                let raw = args.get(i + 1).ok_or_else(|| {
                    "--searx-url requires a value".to_owned()
                })?;
                searx_url = Some(raw.clone());
                i += 2;
            }
            "--json" => {
                // Documented as the default behaviour; accepted for
                // forward-compat with potential `--text` / `--md`
                // sibling flags. No-op today.
                i += 1;
            }
            "--help" | "-h" => {
                return Err(USAGE.to_owned());
            }
            _ => {
                if a.starts_with("--") {
                    return Err(format!("unknown flag: {a}\n\n{USAGE}"));
                }
                if query.is_some() {
                    return Err(format!(
                        "unexpected positional `{a}` — only one <query> allowed\n\n{USAGE}"
                    ));
                }
                query = Some(a.clone());
                i += 1;
            }
        }
    }
    let query = query.ok_or_else(|| USAGE.to_owned())?;
    if query.trim().is_empty() {
        return Err("<query> must not be empty".to_owned());
    }
    if searx_url.is_none() && engines.contains(&Engine::SearxNg) {
        if let Ok(env) = std::env::var(SEARX_URL_ENV) {
            if !env.trim().is_empty() {
                searx_url = Some(env);
            }
        }
    }
    Ok(SearchRequest {
        query,
        limit,
        engines,
        searx_url,
    })
}

const USAGE: &str = "usage: heso search <query> [--limit N] [--engines ddg,mojeek,wiki,searxng] [--searx-url URL]";

fn parse_engines_csv(raw: &str) -> Result<Vec<Engine>, String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for tok in raw.split(',') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        let e = Engine::parse(t).ok_or_else(|| {
            format!(
                "unknown engine `{t}` — supported: ddg, mojeek, wiki, searxng"
            )
        })?;
        if seen.insert(t.to_owned()) {
            out.push(e);
        }
    }
    if out.is_empty() {
        return Err("--engines must list at least one engine".to_owned());
    }
    Ok(out)
}

// ============================================================================
// Orchestrator
// ============================================================================

/// Build the result JSON for one search request. Pure I/O; deterministic
/// given fixed network responses (the test harness exploits this).
pub(crate) async fn run_search(req: &SearchRequest) -> Result<Value, String> {
    let client = build_client()?;
    let mut ddg_results: Vec<RawResult> = Vec::new();
    let mut mojeek_results: Vec<RawResult> = Vec::new();
    let mut searx_results: Vec<RawResult> = Vec::new();
    let mut knowledge: Option<KnowledgeBlock> = None;
    let mut engines_used: Vec<&'static str> = Vec::new();
    let mut errors: Vec<serde_json::Value> = Vec::new();

    for eng in &req.engines {
        match eng {
            Engine::Ddg => match ddg_search(&client, &req.query, req.limit).await {
                Ok(rs) => {
                    // Empty results still count as "we used this engine"
                    // (DDG returned a page, just with zero rows), so record
                    // it unconditionally — callers tell "asked, got nothing"
                    // from "didn't ask" by `ddg` being present in
                    // `engines_used`.
                    engines_used.push("ddg");
                    ddg_results = rs;
                }
                Err(e) => {
                    eprintln!("ddg search error: {e}");
                    errors.push(serde_json::json!({
                        "engine": "ddg",
                        "message": e,
                    }));
                }
            },
            Engine::Mojeek => match mojeek_search(&client, &req.query, req.limit).await {
                Ok(rs) => {
                    // Like DDG, an empty page still counts as "we asked"
                    // — record the engine so callers can tell "asked, got
                    // nothing" from "didn't ask".
                    engines_used.push("mojeek");
                    mojeek_results = rs;
                }
                Err(e) => {
                    eprintln!("mojeek search error: {e}");
                    errors.push(serde_json::json!({
                        "engine": "mojeek",
                        "message": e,
                    }));
                }
            },
            Engine::Wiki => match wiki_summary(&client, &req.query).await {
                Ok(Some(kb)) => {
                    knowledge = Some(kb);
                    engines_used.push("wiki");
                }
                Ok(None) => {
                    // Wikipedia returned a 404 or a disambiguation —
                    // not an error, just no knowledge block. Still
                    // record we tried it so callers see consistent
                    // accounting.
                    engines_used.push("wiki");
                }
                Err(e) => {
                    eprintln!("wikipedia search error: {e}");
                    errors.push(serde_json::json!({
                        "engine": "wiki",
                        "message": e,
                    }));
                }
            },
            Engine::SearxNg => {
                let base = match req.searx_url.as_deref() {
                    Some(s) if !s.trim().is_empty() => s,
                    _ => {
                        eprintln!(
                            "searxng engine requested but no --searx-url / HESO_SEARX_URL set; skipping"
                        );
                        errors.push(serde_json::json!({
                            "engine": "searxng",
                            "message": "no --searx-url / HESO_SEARX_URL set",
                        }));
                        continue;
                    }
                };
                match searxng_search(&client, base, &req.query, req.limit).await {
                    Ok(rs) => {
                        searx_results = rs;
                        engines_used.push("searxng");
                    }
                    Err(e) => {
                        eprintln!("searxng search error: {e}");
                        errors.push(serde_json::json!({
                            "engine": "searxng",
                            "message": e,
                        }));
                    }
                }
            }
        }
    }

    let merged = round_robin_merge(&[&ddg_results, &mojeek_results, &searx_results], req.limit);
    let results: Vec<SearchResult> = merged
        .into_iter()
        .enumerate()
        .map(|(idx, r)| SearchResult {
            rank: idx + 1,
            title: r.title,
            url: r.url,
            snippet: r.snippet,
            source: r.source.as_str(),
        })
        .collect();

    let value = serde_json::json!({
        "query": req.query,
        "engines_used": engines_used,
        "results": results,
        "knowledge": knowledge,
        "errors": if errors.is_empty() { Value::Null } else { Value::Array(errors) },
    });
    Ok(value)
}

fn build_client() -> Result<Client, String> {
    Client::builder()
        .user_agent(BROWSER_UA)
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

/// Interleave per-engine result lists in round-robin order, deduping by
/// canonical URL. Caller passes one slice per engine (in priority order
/// — DDG first today). Stops once `limit` non-duplicate results have
/// been emitted.
fn round_robin_merge(
    sources: &[&[RawResult]],
    limit: usize,
) -> Vec<RawResult> {
    let mut out = Vec::with_capacity(limit);
    let mut seen: HashSet<String> = HashSet::new();
    let mut i = 0;
    let max_len = sources.iter().map(|s| s.len()).max().unwrap_or(0);
    while i < max_len && out.len() < limit {
        for src in sources {
            if out.len() >= limit {
                break;
            }
            if let Some(r) = src.get(i) {
                let key = canonical_url(&r.url);
                if seen.insert(key) {
                    out.push(r.clone());
                }
            }
        }
        i += 1;
    }
    out
}

/// Lossy canonicalisation for dedupe keys ONLY. Lowercases the host,
/// strips a single trailing `/` from the path, drops the fragment, and
/// folds out the scheme so `http://` and `https://` of the same page
/// collapse to one result. Query string is preserved because `?id=42`
/// is a distinct page in most stores. Not a URL normalizer — don't use
/// this for anything outside of dedupe.
fn canonical_url(raw: &str) -> String {
    let parsed = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return raw.trim().to_lowercase(),
    };
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    let mut path = parsed.path().to_owned();
    if path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();
    format!("{host}{path}{query}")
}

// ============================================================================
// DuckDuckGo HTML backend
// ============================================================================

/// Hit `https://html.duckduckgo.com/html/` for the given query, paging
/// until we have `target` results or have requested [`MAX_DDG_PAGES`]
/// pages, whichever comes first.
async fn ddg_search(
    client: &Client,
    query: &str,
    target: usize,
) -> Result<Vec<RawResult>, String> {
    let mut out: Vec<RawResult> = Vec::with_capacity(target);
    let mut seen: HashSet<String> = HashSet::new();
    for page in 0..MAX_DDG_PAGES {
        let offset = ddg_offset_for_page(page);
        let html = ddg_fetch_page(client, query, offset).await?;
        let rows = ddg_parse_html(&html);
        if rows.is_empty() {
            break;
        }
        for row in rows {
            if seen.insert(canonical_url(&row.url)) {
                out.push(row);
                if out.len() >= target {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

/// Offsets mirror what `ddgs` (Python) sends: page 0 → s=0, page 1 →
/// s=10, page 2 → s=25, page 3 → s=40. After the first page, each
/// page is +15. DDG's HTML endpoint accepts these via either the POST
/// body or the GET query string.
fn ddg_offset_for_page(page: usize) -> usize {
    if page == 0 {
        0
    } else {
        10 + (page - 1) * 15
    }
}

async fn ddg_fetch_page(
    client: &Client,
    query: &str,
    offset: usize,
) -> Result<String, String> {
    let mut params: Vec<(&str, String)> = vec![
        ("q", query.to_owned()),
        ("l", "us-en".to_owned()),
        ("b", String::new()),
    ];
    if offset > 0 {
        params.push(("s", offset.to_string()));
    }
    // POST, matching the `ddgs` Python library — DDG's HTML form is
    // technically GET-or-POST, but POST avoids GET's URL-length limits
    // for long queries and is what their reference implementation does.
    let resp = client
        .post("https://html.duckduckgo.com/html/")
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("ddg POST failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("ddg body read failed: {e}"))?;
    if !status.is_success() {
        // DDG returns a rendered HTML page even on rate-limit pages
        // — surface the status but pass the body through so the parser
        // can try anyway. Empty results then propagate the way they
        // would for a no-match.
        eprintln!("ddg returned HTTP {status}; attempting parse anyway");
    }
    Ok(body)
}

/// Parse one DDG HTML page into `RawResult`s. Selectors:
///
/// - `.result` — each search-result block
/// - `a.result__a` — the title link (text + wrapped href)
/// - `.result__snippet` — the snippet (an `<a>` sibling, despite the name)
///
/// The href on `a.result__a` looks like
/// `//duckduckgo.com/l/?uddg=<urlencoded-real-url>&rut=...`; we extract
/// `uddg` and percent-decode it to the canonical destination.
fn ddg_parse_html(html: &str) -> Vec<RawResult> {
    let doc = Html::parse_document(html);
    let result_sel = Selector::parse(".result").expect("static selector .result");
    let title_sel = Selector::parse("a.result__a").expect("static selector a.result__a");
    // `.result__snippet` is sometimes an `<a>`, sometimes a `<div>` —
    // the class selector covers both.
    let snippet_sel = Selector::parse(".result__snippet").expect("static selector .result__snippet");

    let mut out = Vec::new();
    for result in doc.select(&result_sel) {
        let title_el = match result.select(&title_sel).next() {
            Some(el) => el,
            None => continue,
        };
        let title = collapse_ws(&extract_text(&title_el));
        let raw_href = match title_el.value().attr("href") {
            Some(h) => h,
            None => continue,
        };
        let url = match unwrap_ddg_href(raw_href) {
            Some(u) => u,
            None => continue,
        };
        // Filter DDG-internal "y.js" pixel links the way the ddgs
        // Python library does.
        if url.starts_with("https://duckduckgo.com/y.js?")
            || url.starts_with("http://duckduckgo.com/y.js?")
        {
            continue;
        }
        let snippet = result
            .select(&snippet_sel)
            .next()
            .map(|el| collapse_ws(&extract_text(&el)))
            .unwrap_or_default();
        if title.is_empty() || url.is_empty() {
            continue;
        }
        out.push(RawResult {
            title,
            url,
            snippet,
            source: Engine::Ddg,
        });
    }
    out
}

fn extract_text(el: &ElementRef) -> String {
    let mut s = String::new();
    for t in el.text() {
        s.push_str(t);
    }
    s
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Extract the real destination URL from a DDG redirect href.
///
/// DDG wraps every result link in `//duckduckgo.com/l/?uddg=<urlencoded>`
/// (note the leading `//` — protocol-relative). Some configurations
/// also serve direct hrefs without the wrapping; we pass those
/// through unchanged. The `uddg` parameter is percent-encoded with
/// the standard `url::form_urlencoded` rules.
fn unwrap_ddg_href(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // Normalize protocol-relative URLs to https for parsing.
    let normalized = if let Some(rest) = raw.strip_prefix("//") {
        format!("https://{rest}")
    } else if raw.starts_with('/') {
        // Relative path with no host — not useful as a search result.
        return None;
    } else {
        raw.to_owned()
    };
    let parsed = url::Url::parse(&normalized).ok()?;
    if parsed.host_str() == Some("duckduckgo.com")
        && (parsed.path() == "/l/" || parsed.path() == "/l")
    {
        for (k, v) in parsed.query_pairs() {
            if k == "uddg" {
                return Some(v.into_owned());
            }
        }
        // The expected `uddg` was missing — no usable destination.
        return None;
    }
    // Already a direct URL (some DDG modes), pass through.
    Some(parsed.into())
}

// ============================================================================
// Mojeek backend
// ============================================================================

/// Mojeek serves ~10 results per page. 4 pages ≈ 40 results — enough to
/// fill `--limit` toward its 100 cap alongside DDG without hammering a
/// small independent index harder than necessary.
const MAX_MOJEEK_PAGES: usize = 4;

/// Hit `https://www.mojeek.com/search` for the given query, paging until
/// we have `target` results or have requested [`MAX_MOJEEK_PAGES`] pages,
/// whichever comes first.
async fn mojeek_search(
    client: &Client,
    query: &str,
    target: usize,
) -> Result<Vec<RawResult>, String> {
    let mut out: Vec<RawResult> = Vec::with_capacity(target);
    let mut seen: HashSet<String> = HashSet::new();
    for page in 0..MAX_MOJEEK_PAGES {
        let offset = mojeek_offset_for_page(page);
        let html = mojeek_fetch_page(client, query, offset).await?;
        let rows = mojeek_parse_html(&html);
        if rows.is_empty() {
            break;
        }
        for row in rows {
            if seen.insert(canonical_url(&row.url)) {
                out.push(row);
                if out.len() >= target {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

/// Mojeek's result-start parameter (`?s=`): page 0 → 1 (omitted), page 1
/// → 11, page 2 → 21, page 3 → 31. Ten results per page, 1-indexed.
fn mojeek_offset_for_page(page: usize) -> usize {
    page * 10 + 1
}

async fn mojeek_fetch_page(
    client: &Client,
    query: &str,
    offset: usize,
) -> Result<String, String> {
    let mut req = client
        .get("https://www.mojeek.com/search")
        .query(&[("q", query)]);
    // The first page omits `s` (Mojeek treats a bare query as offset 1);
    // later pages send the 11 / 21 / 31… start index.
    if offset > 1 {
        req = req.query(&[("s", offset.to_string())]);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("mojeek GET failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        // Mojeek occasionally rate-limits or errors; surface the status
        // and pass the body through so the parser can try. A non-result
        // body simply yields zero rows, the same as a no-match page.
        eprintln!("mojeek returned HTTP {status}; attempting parse anyway");
    }
    read_search_body_capped(resp, "mojeek").await
}

/// Parse one Mojeek results page. Each result is a
/// `<ul class="results-standard"> <li>` carrying an
/// `<a class="title" href="…">title</a>` (the href is the direct
/// destination, not a redirect wrapper) plus a `<p class="s">` snippet.
fn mojeek_parse_html(html: &str) -> Vec<RawResult> {
    let doc = Html::parse_document(html);
    let item_sel = Selector::parse("ul.results-standard li")
        .expect("static selector ul.results-standard li");
    let title_sel = Selector::parse("a.title").expect("static selector a.title");
    let snippet_sel = Selector::parse("p.s").expect("static selector p.s");

    let mut out = Vec::new();
    for item in doc.select(&item_sel) {
        let title_el = match item.select(&title_sel).next() {
            Some(el) => el,
            None => continue,
        };
        let url = match title_el.value().attr("href") {
            Some(h) => h.trim().to_owned(),
            None => continue,
        };
        // Mojeek result hrefs are absolute; defend against the on-site
        // "see more results from <host>" refinement links (relative
        // `/search?q=site:…`) sneaking in if the markup shifts.
        if !url.starts_with("http://") && !url.starts_with("https://") {
            continue;
        }
        let title = collapse_ws(&extract_text(&title_el));
        if title.is_empty() {
            continue;
        }
        let snippet = item
            .select(&snippet_sel)
            .next()
            .map(|el| collapse_ws(&extract_text(&el)))
            .unwrap_or_default();
        out.push(RawResult {
            title,
            url,
            snippet,
            source: Engine::Mojeek,
        });
    }
    out
}

// ============================================================================
// Wikipedia REST summary backend
// ============================================================================

#[derive(Debug, Deserialize)]
struct WikiSummary {
    #[serde(default, rename = "type")]
    page_type: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    extract: Option<String>,
    #[serde(default)]
    content_urls: Option<WikiContentUrls>,
}

#[derive(Debug, Deserialize)]
struct WikiContentUrls {
    #[serde(default)]
    desktop: Option<WikiDesktopUrls>,
}

#[derive(Debug, Deserialize)]
struct WikiDesktopUrls {
    #[serde(default)]
    page: Option<String>,
}

/// Hit `https://en.wikipedia.org/api/rest_v1/page/summary/<query>`.
/// 404 → `Ok(None)`. 200 with type `disambiguation` → `Ok(None)`
/// (the page is a list of meanings, not a knowledge answer). Other
/// network errors propagate as `Err`.
async fn wiki_summary(
    client: &Client,
    query: &str,
) -> Result<Option<KnowledgeBlock>, String> {
    // Wikipedia's REST `summary` endpoint expects the title as a
    // PATH SEGMENT — spaces are `_` or `%20`, NOT `+`. Using
    // `form_urlencoded` (which encodes ' ' as `+`) returns a 404
    // because the gateway routes on the literal `+`. The cleanest
    // fix is `Url::path_segments_mut().push(query)`, which performs
    // path-component percent-encoding per RFC 3986. Per ADR
    // policy (zero new deps), reusing the existing `url` crate
    // beats adding `percent-encoding` as a direct dep.
    let mut url =
        url::Url::parse("https://en.wikipedia.org/api/rest_v1/page/summary")
            .map_err(|e| format!("internal: bad wiki base URL: {e}"))?;
    // `path_segments_mut().push(seg)` percent-encodes per RFC 3986
    // path-component rules (spaces → `%20`, not `+`). The base above
    // intentionally has NO trailing slash — otherwise `path_segments_mut`
    // treats the trailing `/` as an empty segment and we'd get the
    // double-slash `summary//Linus%20Torvalds` (which the gateway 404s).
    url.path_segments_mut()
        .map_err(|_| "internal: wiki base URL cannot be a base".to_owned())?
        .push(query);
    let resp = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("wikipedia GET failed: {e}"))?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("wikipedia HTTP {}", resp.status()));
    }
    let body = read_search_body_capped(resp, "wikipedia").await?;
    let summary: WikiSummary = serde_json::from_str(&body)
        .map_err(|e| format!("wikipedia JSON parse failed: {e}"))?;
    if matches!(summary.page_type.as_deref(), Some("disambiguation")) {
        return Ok(None);
    }
    let title = summary.title.unwrap_or_default();
    let extract = summary.extract.unwrap_or_default();
    let page_url = summary
        .content_urls
        .and_then(|c| c.desktop)
        .and_then(|d| d.page)
        .unwrap_or_default();
    if title.is_empty() && extract.is_empty() {
        return Ok(None);
    }
    Ok(Some(KnowledgeBlock {
        title,
        summary: extract,
        url: page_url,
    }))
}

// ============================================================================
// SearXNG backend
// ============================================================================

#[derive(Debug, Deserialize)]
struct SearxResponse {
    #[serde(default)]
    results: Vec<SearxResult>,
}

#[derive(Debug, Deserialize)]
struct SearxResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

async fn searxng_search(
    client: &Client,
    base: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<RawResult>, String> {
    let base = base.trim_end_matches('/');
    let url = format!("{base}/search");
    let params = [
        ("q", query),
        ("format", "json"),
    ];
    let resp = client
        .get(&url)
        .query(&params)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("searxng GET failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("searxng HTTP {}", resp.status()));
    }
    let body = read_search_body_capped(resp, "searxng").await?;
    let parsed: SearxResponse = serde_json::from_str(&body)
        .map_err(|e| format!("searxng JSON parse failed: {e}"))?;
    let out = parsed
        .results
        .into_iter()
        .filter_map(|r| {
            let url = r.url?.trim().to_owned();
            if url.is_empty() {
                return None;
            }
            Some(RawResult {
                title: r.title.unwrap_or_default(),
                url,
                snippet: r.content.unwrap_or_default(),
                source: Engine::SearxNg,
            })
        })
        .take(limit)
        .collect();
    Ok(out)
}

// ============================================================================
// Tests — unit-level; integration lives in `tests/search.rs`
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_engines_csv_accepts_known() {
        let v = parse_engines_csv("ddg,mojeek,wiki,searxng").unwrap();
        assert_eq!(
            v,
            vec![Engine::Ddg, Engine::Mojeek, Engine::Wiki, Engine::SearxNg]
        );
    }

    #[test]
    fn parse_engines_csv_dedupes() {
        let v = parse_engines_csv("ddg,ddg,wiki").unwrap();
        assert_eq!(v, vec![Engine::Ddg, Engine::Wiki]);
    }

    #[test]
    fn parse_engines_csv_rejects_unknown() {
        assert!(parse_engines_csv("ddg,google").is_err());
    }

    #[test]
    fn parse_engines_csv_rejects_empty_list() {
        assert!(parse_engines_csv("").is_err());
        assert!(parse_engines_csv(",,").is_err());
    }

    #[test]
    fn parse_cli_args_requires_query() {
        assert!(parse_cli_args(&[]).is_err());
        assert!(parse_cli_args(&["--limit".into(), "5".into()]).is_err());
    }

    #[test]
    fn parse_cli_args_basic_query_only() {
        let r = parse_cli_args(&["rust web scraping".into()]).unwrap();
        assert_eq!(r.query, "rust web scraping");
        assert_eq!(r.limit, DEFAULT_LIMIT);
        assert_eq!(r.engines, vec![Engine::Ddg, Engine::Mojeek, Engine::Wiki]);
        assert!(r.searx_url.is_none());
    }

    #[test]
    fn parse_cli_args_caps_limit_at_max() {
        let r = parse_cli_args(&["q".into(), "--limit".into(), "9999".into()]).unwrap();
        assert_eq!(r.limit, MAX_LIMIT);
    }

    #[test]
    fn parse_cli_args_rejects_zero_limit() {
        assert!(parse_cli_args(&["q".into(), "--limit".into(), "0".into()]).is_err());
    }

    #[test]
    fn parse_cli_args_engine_subset() {
        let r = parse_cli_args(&[
            "q".into(),
            "--engines".into(),
            "ddg".into(),
        ])
        .unwrap();
        assert_eq!(r.engines, vec![Engine::Ddg]);
    }

    #[test]
    fn canonical_url_strips_trailing_slash() {
        assert_eq!(
            canonical_url("https://example.com/foo/"),
            canonical_url("https://example.com/foo"),
        );
    }

    #[test]
    fn canonical_url_lowercases_host() {
        assert_eq!(
            canonical_url("https://Example.COM/foo"),
            canonical_url("https://example.com/foo"),
        );
    }

    #[test]
    fn canonical_url_preserves_query() {
        // Distinct query strings → distinct canonical URLs.
        assert_ne!(
            canonical_url("https://example.com/?a=1"),
            canonical_url("https://example.com/?a=2"),
        );
    }

    #[test]
    fn canonical_url_folds_scheme() {
        // http:// and https:// of the same page dedupe to one result.
        assert_eq!(
            canonical_url("http://pandas.pydata.org/"),
            canonical_url("https://pandas.pydata.org/"),
        );
    }

    #[test]
    fn ddg_offsets_match_python_lib() {
        assert_eq!(ddg_offset_for_page(0), 0);
        assert_eq!(ddg_offset_for_page(1), 10);
        assert_eq!(ddg_offset_for_page(2), 25);
        assert_eq!(ddg_offset_for_page(3), 40);
    }

    #[test]
    fn unwrap_ddg_href_decodes_uddg() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Ffoo&rut=abc";
        let real = unwrap_ddg_href(href).unwrap();
        assert_eq!(real, "https://example.com/foo");
    }

    #[test]
    fn unwrap_ddg_href_passes_direct_urls_through() {
        let real = unwrap_ddg_href("https://example.com/foo").unwrap();
        assert_eq!(real, "https://example.com/foo");
    }

    #[test]
    fn unwrap_ddg_href_handles_missing_uddg() {
        // A `/l/` redirect with no `uddg=` is unusable — return None.
        assert!(unwrap_ddg_href("//duckduckgo.com/l/?rut=abc").is_none());
    }

    #[test]
    fn unwrap_ddg_href_rejects_pure_relative() {
        assert!(unwrap_ddg_href("/local/path").is_none());
        assert!(unwrap_ddg_href("").is_none());
    }

    #[test]
    fn round_robin_merge_alternates_sources() {
        let a = vec![
            RawResult {
                title: "a1".into(),
                url: "https://a.example.com/1".into(),
                snippet: String::new(),
                source: Engine::Ddg,
            },
            RawResult {
                title: "a2".into(),
                url: "https://a.example.com/2".into(),
                snippet: String::new(),
                source: Engine::Ddg,
            },
        ];
        let b = vec![
            RawResult {
                title: "b1".into(),
                url: "https://b.example.com/1".into(),
                snippet: String::new(),
                source: Engine::SearxNg,
            },
            RawResult {
                title: "b2".into(),
                url: "https://b.example.com/2".into(),
                snippet: String::new(),
                source: Engine::SearxNg,
            },
        ];
        let merged = round_robin_merge(&[&a, &b], 10);
        let titles: Vec<&str> = merged.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["a1", "b1", "a2", "b2"]);
    }

    #[test]
    fn round_robin_merge_dedupes_by_canonical_url() {
        let a = vec![RawResult {
            title: "shared".into(),
            url: "https://example.com/x".into(),
            snippet: String::new(),
            source: Engine::Ddg,
        }];
        let b = vec![RawResult {
            title: "shared-trailing".into(),
            url: "https://example.com/x/".into(),
            snippet: String::new(),
            source: Engine::SearxNg,
        }];
        let merged = round_robin_merge(&[&a, &b], 10);
        assert_eq!(merged.len(), 1, "trailing slash must dedupe");
        assert_eq!(merged[0].title, "shared");
    }

    #[test]
    fn round_robin_merge_respects_limit() {
        let a: Vec<RawResult> = (0..5)
            .map(|i| RawResult {
                title: format!("a{i}"),
                url: format!("https://a.example.com/{i}"),
                snippet: String::new(),
                source: Engine::Ddg,
            })
            .collect();
        let merged = round_robin_merge(&[&a], 3);
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn ddg_parse_html_extracts_title_url_snippet() {
        // Minimal fixture mimicking the real DDG HTML structure: each
        // .result wraps an a.result__a (title + href with uddg) and a
        // .result__snippet sibling.
        let html = r#"<!doctype html><html><body>
            <div class="result">
                <div class="result__title">
                    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&rut=abc">
                        Rust Programming Language
                    </a>
                </div>
                <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F">
                    Rust is a fast, reliable, and productive language.
                </a>
            </div>
            <div class="result">
                <div class="result__title">
                    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2F&rut=def">
                        docs.rs
                    </a>
                </div>
                <div class="result__snippet">Documentation host for Rust crates.</div>
            </div>
        </body></html>"#;
        let rows = ddg_parse_html(html);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "Rust Programming Language");
        assert_eq!(rows[0].url, "https://rust-lang.org/");
        assert!(rows[0].snippet.contains("fast"));
        assert_eq!(rows[1].url, "https://docs.rs/");
    }

    #[test]
    fn ddg_parse_html_skips_y_js_pixel_links() {
        let html = r#"<!doctype html><html><body>
            <div class="result">
                <div class="result__title">
                    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fduckduckgo.com%2Fy.js%3Fabc&rut=def">
                        ad
                    </a>
                </div>
                <a class="result__snippet">ad</a>
            </div>
        </body></html>"#;
        assert!(ddg_parse_html(html).is_empty());
    }

    #[test]
    fn ddg_parse_html_empty_for_no_match_page() {
        // DDG renders the search page even for nonsense; selectors
        // simply find no .result rows. We must return [] without
        // panicking — this test pins that contract.
        let html = "<!doctype html><html><body><h1>No results</h1></body></html>";
        assert!(ddg_parse_html(html).is_empty());
    }

    #[test]
    fn mojeek_offsets_increment_by_ten() {
        assert_eq!(mojeek_offset_for_page(0), 1);
        assert_eq!(mojeek_offset_for_page(1), 11);
        assert_eq!(mojeek_offset_for_page(2), 21);
        assert_eq!(mojeek_offset_for_page(3), 31);
    }

    #[test]
    fn mojeek_parse_html_extracts_title_url_snippet() {
        // Mirrors the live Mojeek markup: each result is a
        // `ul.results-standard > li` with an `a.title` (direct href) and
        // a `p.s` snippet, followed by a relative `p.more` refinement
        // link that must NOT be promoted to a result.
        let html = r#"<!doctype html><html><body>
            <ul class="results-standard">
                <li>
                    <h2><a class="title" title="https://www.rust-lang.org/" href="https://www.rust-lang.org/">Rust Programming Language</a></h2>
                    <p class="s">A language empowering everyone to build <strong>reliable</strong> and efficient software.</p>
                    <p class="more"><a href="/search?q=site%3Awww.rust-lang.org+rust">See more results &raquo;</a></p>
                </li>
                <li>
                    <h2><a class="title" title="https://docs.rs/" href="https://docs.rs/">docs.rs</a></h2>
                    <p class="s">Documentation host for Rust crates.</p>
                </li>
            </ul>
        </body></html>"#;
        let rows = mojeek_parse_html(html);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "Rust Programming Language");
        assert_eq!(rows[0].url, "https://www.rust-lang.org/");
        assert!(rows[0].snippet.contains("reliable"));
        assert_eq!(rows[0].source, Engine::Mojeek);
        assert_eq!(rows[1].url, "https://docs.rs/");
        // The relative "see more results" refinement link must be filtered.
        assert!(rows.iter().all(|r| r.url.starts_with("http")));
    }

    #[test]
    fn mojeek_parse_html_empty_for_no_results() {
        // A page with no `ul.results-standard` yields zero rows without
        // panicking — search then falls through to the other engines.
        let html = "<!doctype html><html><body><p>No results found.</p></body></html>";
        assert!(mojeek_parse_html(html).is_empty());
    }

    #[test]
    fn collapse_ws_normalises_runs() {
        assert_eq!(collapse_ws("  a  \n\t b   c "), "a b c");
        assert_eq!(collapse_ws(""), "");
    }
}
