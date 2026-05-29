//! # search
//!
//! `heso search <query>` — first-class multi-source web search verb. Pure
//! HTTP + HTML/JSON parsing; no JS engine is spun up. The default pool is
//! a breadth of independent indexes — Mojeek, Brave, Marginalia — backed
//! by the two DuckDuckGo endpoints (HTML and lite) and the Wikipedia REST
//! `summary` knowledge block, with no API keys and no signup. SearXNG
//! joins the default sweep when a base URL is configured via `--searx-url`
//! or `HESO_SEARX_URL`.
//!
//! Querying several independent indexes is the redundancy that makes the
//! verb reliable: when one backend throttles, the others carry the result
//! set, and the throttled backend is surfaced loudly (never folded into a
//! silent empty) so a caller can tell "asked, got blocked" from "asked,
//! got zero". A backend that throttles once in a run is cooled down and
//! skipped for the rest of that run.
//!
//! ## Output shape (JSON to stdout)
//!
//! ```json
//! {
//!   "query": "rust web scraping",
//!   "engines_used": ["mojeek", "brave", "marginalia", "wiki"],
//!   "blocked": ["ddg"],
//!   "results": [
//!     {"rank": 1, "title": "...", "url": "https://...",
//!      "snippet": "...", "source": "mojeek"}
//!   ],
//!   "knowledge": {
//!     "title": "Web scraping",
//!     "summary": "Web scraping is...",
//!     "url": "https://en.wikipedia.org/wiki/Web_scraping"
//!   },
//!   "errors": [
//!     {"engine": "ddg", "code": "rate_limited",
//!      "message": "rate limited (HTTP 202) after 3 retries", "http_status": 202}
//!   ]
//! }
//! ```
//!
//! `knowledge` is `null` if no Wikipedia direct match. `errors` is `null`
//! only when every attempted backend returned results; otherwise it is an
//! array of typed `{engine, code, message, ...}` rows whose `code` is one
//! of `rate_limited | bot_challenge | config_error | transport_error`. A
//! throttled backend appears in `blocked` (omitted/`null` when nothing was
//! throttled) and NOT in `engines_used`. `results` is capped by
//! `--limit N` (default 30, max 100).
//!
//! ## Module layout
//!
//! The verb is split across this directory so the always-on resilience
//! layer (ADR 0026) has room without bloating one file:
//!
//! - [`mod`](self) — re-exports; `cmd_search`, `parse_cli_args`, the
//!   `run_search` orchestrator, and the envelope structs.
//! - [`backend`] — the closed [`BackendId`] pool the orchestrator
//!   iterates, its wire names, and the `is_default()` extension point.
//! - [`http`] — the HTTP client + per-backend fetch orchestration.
//! - [`parse`] — the per-engine HTML/JSON parsers and their fixtures.
//! - [`classify`] — response classification into a typed outcome.
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
//! 2. **Brave Search** (independent index, no key for the web UI): GET
//!    `https://search.brave.com/search?q=<query>&source=web`. One request
//!    (~20 organic results); each `div.snippet[data-type="web"]` carries a
//!    result anchor (direct href) and a `.snippet-description`. A 403 is
//!    Brave's hard per-IP block (`rate_limited`); a 2xx with a PoW/CAPTCHA
//!    script and no result container is a `bot_challenge`.
//!
//! 3. **Marginalia** (small independent index, public JSON API, no key):
//!    GET `https://api.marginalia.nu/public/search/<query>?count=N`. A
//!    single request returns the full small-index set as
//!    `{results: [{url, title, description}]}`. **HTTP 503** is its
//!    documented, clean rate-limit signal (`rate_limited`).
//!
//! 4. **DuckDuckGo HTML** (best-effort, no key): POST to
//!    `https://html.duckduckgo.com/html/` with form `q=<query>&l=us-en`.
//!    Pagination via `s=N` (offset 0, 10, 25, 40, 55 — first page is
//!    `s=0`, then `+15` per additional page, matching the `ddgs` Python
//!    library). Capped at 4 pages, though the single-request-per-query
//!    default (below) reaches that ceiling only for a high `--limit`. The
//!    href on each result is wrapped in DDG's redirect
//!    (`//duckduckgo.com/l/?uddg=<urlencoded>&...`); we unwrap via `uddg=`
//!    query-param decode. A 202 or anomaly-detection body is a throttle.
//!
//! 5. **DuckDuckGo lite** (best-effort, no key): POST to
//!    `https://lite.duckduckgo.com/lite/`. The same DDG gate and redirect
//!    wrapping as the HTML endpoint, parsing the lighter `<table>` layout
//!    (`a.result-link` + `td.result-snippet`).
//!
//! 6. **Wikipedia REST `summary`** (knowledge block, no key): GET
//!    `https://en.wikipedia.org/api/rest_v1/page/summary/<urlencoded>`.
//!    Returns JSON with `title`, `extract`, `content_urls.desktop.page`.
//!    404 → omitted; 200 with `type == "disambiguation"` → omitted (the
//!    page is a list of meanings, not a knowledge answer). Other errors
//!    → omitted with a stderr note.
//!
//! 7. **SearXNG** (default only when a URL is configured): GET
//!    `<base>/search?q=<q>&format=json`. Returns `{results: [{title, url,
//!    content, ...}]}` (the field name is `content`, not `snippet`).
//!    Mapped to our shape with `source: "searxng"`. Note: most public
//!    SearXNG instances disable JSON output by default — see
//!    <https://docs.searxng.org/dev/search_api.html>.
//!
//! Yandex, Bing, and Google are intentionally NOT in the pool: they gate
//! scripted callers behind CAPTCHA / SearchGuard / active human challenges
//! that conflict with the always-on "never rate-limited" posture. They are
//! documented as unsupported rather than half-implemented (ADR 0026).
//!
//! ## Single request per query (A.3.4)
//!
//! Each backend fetches page 0 only by default; it pages further ONLY
//! while the previous page came back clean (parseable, non-empty) AND the
//! `--limit` is not yet met, hard-capped per backend. Because the pool's
//! breadth (Mojeek + Brave + Marginalia + the two DDG endpoints) already
//! fills a modest `--limit` from first pages alone, the common case is one
//! request per backend — the single biggest reduction in request volume.
//!
//! ## Ranked merge
//!
//! Multiple engines → dedupe by canonical URL (lowercase host, strip
//! trailing `/` from path). Final order is round-robin from each backend's
//! top, in pool-priority order (Mojeek leads), so when one backend comes
//! back empty or blocked the merged list is simply the others' in rank
//! order. Wikipedia is NOT in the results array — it goes in the top-level
//! `knowledge` block.
//!
//! ## Request fingerprints
//!
//! Search runs through [`http::RotatingClient`], a separate client from
//! [`FetchEngine`]: the engine identifies as `heso/<version>` (sensible
//! for cooperative sites that want to know who's calling), but the big
//! engines serve anti-bot pages to a fixed scripted caller. Every search
//! request instead carries a browser fingerprint drawn from a curated
//! pool of current desktop User-Agents and their matching headers,
//! chosen per host so each backend sees a distinct browser-shaped client.
//! Rotation, full-jitter retry, and per-host pacing are always on — there
//! is no flag — so the verb effectively never trips a per-IP limit.
//!
//! ## Tests
//!
//! See `crates/heso-cli/tests/search.rs` — wiremock stubs the SearXNG
//! endpoint (the one backend whose base URL is configurable from
//! outside the binary) end-to-end; we verify merge order, dedupe, limit,
//! missing-knowledge handling, the loud `blocked`/`errors` envelope on a
//! throttle, and the empty-results-no-crash behaviour for nonsense
//! queries. The Mojeek, Brave, Marginalia, and DuckDuckGo (HTML + lite)
//! parsers, whose hosts aren't configurable, are unit-tested against
//! pinned fixtures in the [`parse`] module so markup drift fails loudly.

use std::collections::HashSet;
use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;

mod backend;
mod cache;
mod classify;
mod http;
mod parse;

// ============================================================================
// Constants
// ============================================================================

/// Default cap on merged result count when `--limit` is omitted.
/// Also the default for the JSON-RPC `search` method.
pub(crate) const DEFAULT_LIMIT: usize = 30;

/// Hard cap on merged result count. Higher values risk requesting
/// many DDG pages for very little marginal value — agents asking for
/// "top 1000" should be using a proper API instead.
pub(crate) const MAX_LIMIT: usize = 100;

/// Environment variable name for the SearXNG base URL. Read by both
/// the CLI and the JSON-RPC `search` method when the request doesn't
/// supply `searx_url` / `--searx-url` explicitly.
pub const SEARX_URL_ENV: &str = "HESO_SEARX_URL";

// ============================================================================
// Public API — used by both `cmd_search` and the `search` RPC method
// ============================================================================

pub(crate) use backend::{BackendId, DEFAULT_POOL, SUPPORTED_NAMES};

/// Parse the JSON-RPC `engines` parameter, accepting either a CSV
/// string (`"ddg,wiki"`) or a JSON array (`["ddg","wiki"]`). Both
/// shapes map to the same internal vector. Empty / null inputs
/// surface as an error — callers that want defaults should pass
/// `None` for the whole field, which `dispatch_search` handles
/// upstream.
pub(crate) fn parse_engines_value(v: &Value) -> Result<Vec<BackendId>, String> {
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
                let e = BackendId::parse(t).ok_or_else(|| {
                    format!("engines: unknown engine `{t}` — supported: {SUPPORTED_NAMES}")
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
pub(crate) struct RawResult {
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) snippet: String,
    pub(crate) source: BackendId,
}

/// Inputs accepted by both the CLI and the RPC. Constructed by
/// [`parse_cli_args`] for the binary and built directly by the
/// JSON-RPC dispatcher.
#[derive(Debug, Clone)]
pub(crate) struct SearchRequest {
    pub(crate) query: String,
    pub(crate) limit: usize,
    pub(crate) engines: Vec<BackendId>,
    pub(crate) searx_url: Option<String>,
    /// Per-request inner deadline for every backend HTTP call, in
    /// milliseconds. `Some(ms)` with `ms > 0` is the reqwest per-attempt
    /// timeout; `Some(0)` opts out of the cap (unbounded); `None` lets the
    /// client pick its default. The retry budget multiplies this across
    /// attempts — see [`http::RotatingClient::new`].
    pub(crate) timeout_ms: Option<u64>,
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
    // An all-throttled sweep must be loud at the process level too: exit
    // non-zero when every result-backend we asked came back blocked and we
    // have neither results nor a knowledge block. Real results — even
    // alongside one blocked backend — are a partial success, so we exit 0
    // there; a blocked backend is still surfaced in the `blocked`/`errors`
    // JSON for the caller to inspect.
    let no_results = value
        .get("results")
        .and_then(|r| r.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true);
    let no_knowledge = value.get("knowledge").map(|k| k.is_null()).unwrap_or(true);
    let was_blocked = value
        .get("blocked")
        .map(|b| !b.is_null())
        .unwrap_or(false);
    let serialized = match serde_json::to_string_pretty(&value) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to serialize output: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("{serialized}");
    if no_results && no_knowledge && was_blocked {
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
    let mut engines = DEFAULT_POOL.to_vec();
    let mut engines_explicit = false;
    let mut searx_url: Option<String> = None;
    let mut timeout_ms: Option<u64> = None;
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
                engines_explicit = true;
                i += 2;
            }
            "--searx-url" => {
                let raw = args.get(i + 1).ok_or_else(|| {
                    "--searx-url requires a value".to_owned()
                })?;
                searx_url = Some(raw.clone());
                i += 2;
            }
            "--timeout" => {
                let raw = args.get(i + 1).ok_or_else(|| {
                    "--timeout requires a value".to_owned()
                })?;
                // Same duration grammar open/read use: `5s`, `500ms`,
                // bare ms. This is the per-request inner deadline for each
                // backend call; the retry budget multiplies it across
                // attempts (see the `--timeout` help in main.rs).
                let ms = crate::parse_duration_ms(raw).map_err(|e| format!("--timeout: {e}"))?;
                timeout_ms = Some(ms);
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
    if searx_url.is_none() {
        if let Ok(env) = std::env::var(SEARX_URL_ENV) {
            if !env.trim().is_empty() {
                searx_url = Some(env);
            }
        }
    }
    // SearXNG is "default iff configured" (ADR 0026): `is_default()` is
    // false for it precisely because it needs an operator URL, so it is
    // absent from `DEFAULT_POOL`. When one is set and the user didn't
    // override `--engines`, fold it into the default sweep. An explicit
    // `--engines` list is honoured verbatim.
    if !engines_explicit
        && searx_url.is_some()
        && !BackendId::SearxNg.is_default()
        && !engines.contains(&BackendId::SearxNg)
    {
        engines.push(BackendId::SearxNg);
    }
    Ok(SearchRequest {
        query,
        limit,
        engines,
        searx_url,
        timeout_ms,
    })
}

const USAGE: &str = "usage: heso search <query> [--limit N] [--engines mojeek,brave,marginalia,ddg,ddg-lite,searxng,wiki] [--searx-url URL] [--timeout DUR]";

fn parse_engines_csv(raw: &str) -> Result<Vec<BackendId>, String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for tok in raw.split(',') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        let e = BackendId::parse(t)
            .ok_or_else(|| format!("unknown engine `{t}` — supported: {SUPPORTED_NAMES}"))?;
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
///
/// Iterates the requested backend pool in priority order. Each
/// result-backend is classified loudly: a `Results` outcome feeds the
/// merge and lists the backend in `engines_used`; a throttle / challenge /
/// config error pushes a typed error row, lists the backend in `blocked`,
/// and marks it cooled-down so it is skipped for the rest of this run
/// (A.3.3). A cooled-down backend is never retried within a single search.
pub(crate) async fn run_search(req: &SearchRequest) -> Result<Value, String> {
    let client = http::RotatingClient::new(req.timeout_ms)?;
    // One result vector per result-backend, kept in the order the backends
    // were attempted so the round-robin merge interleaves by priority.
    let mut result_lists: Vec<Vec<RawResult>> = Vec::new();
    let mut knowledge: Option<KnowledgeBlock> = None;
    let mut engines_used: Vec<&'static str> = Vec::new();
    let mut blocked: Vec<&'static str> = Vec::new();
    let mut errors: Vec<serde_json::Value> = Vec::new();

    for &backend in &req.engines {
        // A backend throttled earlier this run is cooled down: skip it
        // rather than spend another request on a source we already know is
        // declining to answer.
        if client.is_cooled_down(backend) {
            continue;
        }
        match backend {
            BackendId::Wiki => match http::wiki_summary(&client, &req.query).await {
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
                        "code": "transport_error",
                        "message": e,
                    }));
                    blocked.push("wiki");
                }
            },
            BackendId::SearxNg => {
                let base = match req.searx_url.as_deref() {
                    Some(s) if !s.trim().is_empty() => s,
                    _ => {
                        eprintln!(
                            "searxng engine requested but no --searx-url / HESO_SEARX_URL set; skipping"
                        );
                        errors.push(serde_json::json!({
                            "engine": "searxng",
                            "code": "config_error",
                            "message": "no --searx-url / HESO_SEARX_URL set",
                        }));
                        blocked.push("searxng");
                        continue;
                    }
                };
                let outcome = http::searxng_search(&client, base, &req.query, req.limit).await;
                let rows = record_outcome(
                    &client,
                    BackendId::SearxNg,
                    outcome,
                    &mut engines_used,
                    &mut blocked,
                    &mut errors,
                );
                result_lists.push(rows);
            }
            web => {
                let outcome = http::web_search(&client, web, &req.query, req.limit).await;
                let rows = record_outcome(
                    &client,
                    web,
                    outcome,
                    &mut engines_used,
                    &mut blocked,
                    &mut errors,
                );
                result_lists.push(rows);
            }
        }
    }

    let list_refs: Vec<&[RawResult]> = result_lists.iter().map(|v| v.as_slice()).collect();
    let merged = round_robin_merge(&list_refs, req.limit);
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
        "blocked": if blocked.is_empty() { Value::Null } else { serde_json::to_value(&blocked).unwrap_or(Value::Null) },
        "results": results,
        "knowledge": knowledge,
        "errors": if errors.is_empty() { Value::Null } else { Value::Array(errors) },
    });
    Ok(value)
}

/// Fold one result-backend's [`classify::BackendOutcome`] into the
/// orchestrator's accumulators, returning the rows (empty for any
/// non-`Results` outcome).
///
/// `Results` is the only outcome that lists the backend in `engines_used`
/// — a genuine empty page still counts as "asked, got nothing" and is
/// recorded there. A throttle/challenge/config error instead pushes a
/// typed error row, adds the backend to `blocked`, AND marks it
/// cooled-down on `client` so the orchestrator skips it for the rest of
/// the run. So an agent reads "asked, got blocked" distinctly from
/// "asked, got zero". This is the loud-failure contract: `errors` is null
/// only when every attempted backend returned `Results`.
fn record_outcome(
    client: &http::RotatingClient,
    backend: BackendId,
    outcome: classify::BackendOutcome,
    engines_used: &mut Vec<&'static str>,
    blocked: &mut Vec<&'static str>,
    errors: &mut Vec<serde_json::Value>,
) -> Vec<RawResult> {
    use classify::BackendOutcome;
    let name = backend.as_str();
    match outcome {
        BackendOutcome::Results(rows) => {
            engines_used.push(name);
            rows
        }
        BackendOutcome::RateLimited { status, retried } => {
            let message = match status {
                Some(s) => format!("rate limited (HTTP {s}) after {retried} retries"),
                None => format!("rate limited after {retried} retries"),
            };
            eprintln!("{name} rate limited: {message}");
            let mut row = serde_json::json!({
                "engine": name,
                "code": "rate_limited",
                "message": message,
            });
            if let Some(s) = status {
                row["http_status"] = serde_json::json!(s);
            }
            errors.push(row);
            blocked.push(name);
            client.cool_down(backend);
            Vec::new()
        }
        BackendOutcome::BotChallenge { marker } => {
            let message = format!("bot challenge detected ({marker})");
            eprintln!("{name} blocked: {message}");
            errors.push(serde_json::json!({
                "engine": name,
                "code": "bot_challenge",
                "message": message,
                "marker": marker,
            }));
            blocked.push(name);
            client.cool_down(backend);
            Vec::new()
        }
        BackendOutcome::ConfigError(message) => {
            eprintln!("{name} config error: {message}");
            errors.push(serde_json::json!({
                "engine": name,
                "code": "config_error",
                "message": message,
            }));
            blocked.push(name);
            client.cool_down(backend);
            Vec::new()
        }
    }
}

/// Interleave per-engine result lists in round-robin order, deduping by
/// canonical URL. Caller passes one slice per engine in priority order
/// (Mojeek leads the default pool). Stops once `limit` non-duplicate
/// results have been emitted.
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
pub(crate) fn canonical_url(raw: &str) -> String {
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
// Tests — orchestrator-level; per-parser fixtures live in `parse.rs`,
// integration in `tests/search.rs`
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_engines_csv_accepts_known() {
        let v = parse_engines_csv("ddg,mojeek,wiki,searxng,brave,marginalia,ddg-lite").unwrap();
        assert_eq!(
            v,
            vec![
                BackendId::DdgHtml,
                BackendId::Mojeek,
                BackendId::Wiki,
                BackendId::SearxNg,
                BackendId::Brave,
                BackendId::Marginalia,
                BackendId::DdgLite,
            ]
        );
    }

    #[test]
    fn parse_engines_csv_dedupes() {
        let v = parse_engines_csv("ddg,ddg,wiki").unwrap();
        assert_eq!(v, vec![BackendId::DdgHtml, BackendId::Wiki]);
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
        assert_eq!(r.engines, DEFAULT_POOL.to_vec());
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
        assert_eq!(r.engines, vec![BackendId::DdgHtml]);
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
    fn round_robin_merge_alternates_sources() {
        let a = vec![
            RawResult {
                title: "a1".into(),
                url: "https://a.example.com/1".into(),
                snippet: String::new(),
                source: BackendId::DdgHtml,
            },
            RawResult {
                title: "a2".into(),
                url: "https://a.example.com/2".into(),
                snippet: String::new(),
                source: BackendId::DdgHtml,
            },
        ];
        let b = vec![
            RawResult {
                title: "b1".into(),
                url: "https://b.example.com/1".into(),
                snippet: String::new(),
                source: BackendId::SearxNg,
            },
            RawResult {
                title: "b2".into(),
                url: "https://b.example.com/2".into(),
                snippet: String::new(),
                source: BackendId::SearxNg,
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
            source: BackendId::DdgHtml,
        }];
        let b = vec![RawResult {
            title: "shared-trailing".into(),
            url: "https://example.com/x/".into(),
            snippet: String::new(),
            source: BackendId::SearxNg,
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
                source: BackendId::DdgHtml,
            })
            .collect();
        let merged = round_robin_merge(&[&a], 3);
        assert_eq!(merged.len(), 3);
    }
}
