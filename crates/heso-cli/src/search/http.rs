//! HTTP client construction and per-backend fetch orchestration for the
//! search verb. Every outbound request the verb makes flows through
//! [`RotatingClient`]; the parsers it feeds live in [`super::parse`].
//!
//! The always-on resilience layer (ADR 0026) lives here: each request
//! carries a rotated browser fingerprint, is paced per host by a
//! [`governor`] rate limiter, and is retried with full-jitter
//! exponential backoff (the spider-rs technique) before any throttle is
//! reported. None of this is behind a flag — it is the default posture
//! so the verb effectively never trips a per-IP limit.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::Mutex;
use std::time::Duration;

use backon::Retryable;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Jitter, Quota, RateLimiter};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::Deserialize;

use super::classify::{classify_response, BackendOutcome};
use super::parse::marginalia_parse_json;
use super::{cache, canonical_url, BackendId, KnowledgeBlock, RawResult};

// ============================================================================
// Constants
// ============================================================================

/// Maximum number of DDG result pages we will request regardless of
/// `--limit`. Each page yields ~15 results; 4 pages ≈ 60 results,
/// matching the practical reach of the HTML endpoint. Going further
/// pulls in low-quality long-tail results AND risks DDG rate-limiting
/// the IP for the next while. The single-request-per-query default
/// (A.3.4) means this ceiling is reached only when a high `--limit` can't
/// be filled from the rest of the pool's first pages.
const MAX_DDG_PAGES: usize = 4;

/// Mojeek serves ~10 results per page. 4 pages ≈ 40 results — enough to
/// fill `--limit` toward its 100 cap alongside the pool without hammering
/// a small independent index harder than necessary.
const MAX_MOJEEK_PAGES: usize = 4;

/// Brave Search has no on-site pagination we drive; a single page already
/// returns ~20 organic results. Capped at one request — Brave is one of
/// the breadth sources the merge interleaves, not a depth source.
const MAX_BRAVE_PAGES: usize = 1;

/// Default per-request timeout for any single HTTP call (DDG page,
/// Wikipedia summary, SearXNG `/search`) when the caller passes no
/// `--timeout`. The outer retry budget multiplies this across attempts;
/// the per-host pacing keeps that budget rarely spent.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on a single search-backend response body. Wikipedia
/// summaries and SearXNG JSON for one query are kilobyte-sized; 4 MiB
/// is the headroom a hostile or misconfigured backend would need to
/// push the CLI into multi-hundred-MB allocations.
const MAX_SEARCH_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

// ============================================================================
// Fingerprint rotation (A.3.1)
// ============================================================================

/// One browser fingerprint: a real desktop `User-Agent` paired with the
/// `Accept-Language` a browser sending that UA would announce. Applied
/// per request (not pinned on the client) so each backend sees a
/// distinct, browser-shaped caller and no single static string becomes a
/// fingerprint anti-bot filters can pin.
struct HeaderProfile {
    user_agent: &'static str,
    accept_language: &'static str,
}

/// Curated pool of current desktop browser fingerprints (Chrome /
/// Firefox / Safari / Edge on Windows, macOS, and Linux). One is chosen
/// per host per process so Mojeek and Brave look like different clients;
/// none identifies as heso, because a recognisable token is itself a
/// fingerprint.
const PROFILES: &[HeaderProfile] = &[
    HeaderProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        accept_language: "en-US,en;q=0.9",
    },
    HeaderProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        accept_language: "en-US,en;q=0.9",
    },
    HeaderProfile {
        user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
        accept_language: "en-US,en;q=0.8",
    },
    HeaderProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:133.0) Gecko/20100101 Firefox/133.0",
        accept_language: "en-US,en;q=0.5",
    },
    HeaderProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:133.0) Gecko/20100101 Firefox/133.0",
        accept_language: "en-US,en;q=0.5",
    },
    HeaderProfile {
        user_agent: "Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:132.0) Gecko/20100101 Firefox/132.0",
        accept_language: "en-GB,en;q=0.7",
    },
    HeaderProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15",
        accept_language: "en-US,en;q=0.9",
    },
    HeaderProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0",
        accept_language: "en-US,en;q=0.9",
    },
    HeaderProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
        accept_language: "en-US,en;q=0.9",
    },
    HeaderProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
        accept_language: "en-CA,en;q=0.9",
    },
];

/// The `Accept` header a navigating browser sends for a top-level
/// document — shared across the pool because it does not vary by UA.
const ACCEPT_HTML: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";

// ============================================================================
// Backoff + retry classification (A.3.2) — spider-rs techniques, ported
// ============================================================================

/// Number of retries after the initial attempt. Total attempts =
/// `MAX_RETRIES + 1`. Surfaced to [`super::classify`] so a `RateLimited`
/// outcome can report how many retries were spent before giving up.
pub(super) const MAX_RETRIES: usize = 3;

/// Base unit for the exponential schedule (`base * 2^attempt`).
const BACKOFF_BASE: Duration = Duration::from_millis(250);

/// Ceiling for any single backoff delay, before jitter.
const BACKOFF_MAX: Duration = Duration::from_secs(4);

/// Upper bound on a server-supplied `Retry-After`. An attacker- or
/// misconfiguration-supplied "wait one hour" is a denial-of-service
/// vector, so any honoured `Retry-After` is clamped here.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(30);

/// Full-jitter exponential backoff: a uniform random delay in
/// `[0, min(base * 2^attempt, max)]`. This is spider-rs's
/// `utils/backoff` schedule. The cap is computed overflow-safe — a large
/// `attempt` saturates at `max` rather than wrapping — so the schedule
/// is total for every `u32`.
fn backoff_delay(attempt: u32, base: Duration, max: Duration, rng: &mut fastrand::Rng) -> Duration {
    let base_ms = base.as_millis() as u64;
    // `base * 2^attempt`, saturating: `checked_shl` guards the shift
    // amount (>= 64 wraps in plain `<<`) and `saturating_mul` guards the
    // multiply, so the ceiling can only ever reach `u64::MAX`.
    let scaled = 1u64
        .checked_shl(attempt)
        .map(|f| base_ms.saturating_mul(f))
        .unwrap_or(u64::MAX);
    let cap_ms = scaled.min(max.as_millis() as u64);
    // Uniform in `[0, cap]` inclusive — `u64(0..=cap)` is total even when
    // `cap == 0`.
    Duration::from_millis(rng.u64(0..=cap_ms))
}

/// spider-rs's retryable-status predicate. Retry transient throttles and
/// server faults; treat the rest — notably `403` ("you are blocked"),
/// `404`, and `501` — as permanent so we stop hammering a backend that
/// will not change its mind.
fn is_retryable_status(status: StatusCode) -> bool {
    let code = status.as_u16();
    match code {
        429 | 408 => true,
        // 501 Not Implemented is a permanent "this endpoint won't ever
        // serve you" within the otherwise-transient 5xx range; everything
        // else 5xx is a server fault worth retrying.
        501 => false,
        500..=599 => true,
        _ => false,
    }
}

/// A throttle outcome for a status that survived the transport retry
/// layer, shared by the JSON backends (SearXNG, Marginalia) whose bodies
/// don't route through [`super::classify`]. A `429 | 503 | 408 | 5xx` here
/// is spent retries on a retryable status — a loud rate-limit, never a
/// silent empty. Marginalia's documented `503` rate-limit lands here.
fn retryable_status_outcome(code: u16) -> Option<BackendOutcome> {
    if code == 429 || code == 503 || code == 408 || (500..=599).contains(&code) {
        Some(BackendOutcome::RateLimited {
            status: Some(code),
            retried: super::classify::TRANSPORT_RETRIES,
        })
    } else {
        None
    }
}

/// Parse a `Retry-After` header value — either a non-negative number of
/// seconds or an HTTP-date — into a delay, clamped to
/// [`RETRY_AFTER_CAP`]. Returns `None` for an unparseable value so the
/// caller falls back to the computed backoff.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs).min(RETRY_AFTER_CAP));
    }
    let target = httpdate::parse_http_date(value).ok()?;
    let now = std::time::SystemTime::now();
    let delay = target.duration_since(now).unwrap_or(Duration::ZERO);
    Some(delay.min(RETRY_AFTER_CAP))
}

// ============================================================================
// RotatingClient — the request chokepoint (A.3.1–A.3.4)
// ============================================================================

type HostLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

/// The always-on resilient HTTP client every search backend calls.
///
/// Owns the underlying [`reqwest::Client`], the per-host pacing limiters,
/// and the per-backend cooldown map the orchestrator consults. A request
/// made through [`RotatingClient::fetch`] is fingerprinted, paced, and
/// retried with full-jitter backoff automatically.
pub(super) struct RotatingClient {
    client: Client,
    /// One pacing limiter per host so a burst against Mojeek never eats
    /// Brave's budget.
    limiters: Mutex<HashMap<String, std::sync::Arc<HostLimiter>>>,
    /// Backends marked cooled-down (after a throttle/challenge) within
    /// this run; the orchestrator skips a cooled-down backend rather than
    /// spend another request on a source already declining to answer.
    /// Populated by [`RotatingClient::cool_down`].
    cooldowns: Mutex<HashSet<BackendId>>,
}

/// Per-host pacing quota: two requests per second, bursting up to four.
/// Keeps the verb well under the per-IP ceilings the big engines apply
/// so the retry path is rarely reached.
fn host_quota() -> Quota {
    let burst = NonZeroU32::new(4).expect("burst is non-zero");
    let per_sec = NonZeroU32::new(2).expect("rate is non-zero");
    Quota::per_second(per_sec).allow_burst(burst)
}

impl RotatingClient {
    /// Build the client with a per-request inner deadline. `timeout_ms`
    /// is the `--timeout` budget the caller chose: `Some(ms)` with
    /// `ms > 0` becomes the reqwest per-attempt timeout (the TLS
    /// handshake, redirect chain, and body stream for ONE attempt all
    /// share it); `None` falls back to [`DEFAULT_REQUEST_TIMEOUT`];
    /// `Some(0)` opts out of the per-request cap entirely (unbounded), the
    /// same "0 means no cap" convention the other network verbs use. The
    /// OUTER wall-clock budget is bounded separately by the retry count
    /// ([`MAX_RETRIES`]) plus the capped backoff, so total time is roughly
    /// `timeout * (1 + retries) + Σ backoff`.
    pub(super) fn new(timeout_ms: Option<u64>) -> Result<Self, String> {
        // The UA is NOT pinned on the client — it is rotated per request
        // from [`PROFILES`]. The builder still carries the cross-cutting
        // policy (redirects, per-request timeout) shared by every call.
        let mut builder =
            Client::builder().redirect(reqwest::redirect::Policy::limited(10));
        builder = match timeout_ms {
            Some(0) => builder,
            Some(ms) => builder.timeout(Duration::from_millis(ms)),
            None => builder.timeout(DEFAULT_REQUEST_TIMEOUT),
        };
        let client = builder
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;
        Ok(RotatingClient {
            client,
            limiters: Mutex::new(HashMap::new()),
            cooldowns: Mutex::new(HashSet::new()),
        })
    }

    /// Pick a fingerprint for `host`, stable for the life of the process
    /// so repeated pages to one backend keep the same identity. Seeded
    /// from the host string via `fastrand` so the choice is deterministic
    /// per host but varies across hosts.
    fn profile_for_host(host: &str) -> &'static HeaderProfile {
        let seed = host.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x100000001b3)
        });
        let idx = fastrand::Rng::with_seed(seed).usize(0..PROFILES.len());
        &PROFILES[idx]
    }

    /// Apply the per-request browser fingerprint to a builder: the
    /// rotated `User-Agent`/`Accept-Language` plus the fixed navigation
    /// headers a browser sends for a top-level document.
    fn fingerprint(&self, host: &str, req: RequestBuilder) -> RequestBuilder {
        let profile = Self::profile_for_host(host);
        req.header(reqwest::header::USER_AGENT, profile.user_agent)
            .header(reqwest::header::ACCEPT, ACCEPT_HTML)
            .header(reqwest::header::ACCEPT_LANGUAGE, profile.accept_language)
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header(reqwest::header::UPGRADE_INSECURE_REQUESTS, "1")
    }

    /// The per-host pacing limiter, lazily created.
    fn limiter_for(&self, host: &str) -> std::sync::Arc<HostLimiter> {
        let mut limiters = self.limiters.lock().expect("limiters mutex poisoned");
        limiters
            .entry(host.to_owned())
            .or_insert_with(|| std::sync::Arc::new(RateLimiter::direct(host_quota())))
            .clone()
    }

    /// Record that `backend` was throttled/challenged this run so the
    /// orchestrator can skip it (A.3.3 per-backend cooldown). Idempotent.
    pub(super) fn cool_down(&self, backend: BackendId) {
        self.cooldowns
            .lock()
            .expect("cooldowns mutex poisoned")
            .insert(backend);
    }

    /// True if `backend` has been cooled down this run.
    pub(super) fn is_cooled_down(&self, backend: BackendId) -> bool {
        self.cooldowns
            .lock()
            .expect("cooldowns mutex poisoned")
            .contains(&backend)
    }

    /// Send one fingerprinted, paced request, retrying retryable statuses
    /// and transient transport errors with full-jitter exponential
    /// backoff (A.3.2). `build` is called fresh on every attempt (the
    /// previous attempt's response body is consumed by classification),
    /// so it must produce an equivalent request each time.
    ///
    /// On success — or on a *permanent* status the caller should inspect
    /// (e.g. 403/404) — returns the [`Response`]. After retries are
    /// exhausted on a retryable status the last response is still
    /// returned, so the caller decides how loudly to surface it; only a
    /// transport error with no response yields `Err`.
    async fn fetch(
        &self,
        host: &str,
        mut build: impl FnMut() -> RequestBuilder,
    ) -> Result<Response, String> {
        let mut attempt: u32 = 0;
        let limiter = self.limiter_for(host);

        // The synchronous request build (and the limiter handle) are
        // prepared per attempt and moved into the future, so the async
        // block borrows nothing from the `FnMut` itself — only owned
        // values cross into it.
        let operation = || {
            let req = self.fingerprint(host, build());
            let limiter = limiter.clone();
            let host = host.to_owned();
            async move {
                // Pace before every outbound attempt; jitter spreads
                // retries so a cluster of backends never re-fires in
                // lockstep.
                limiter
                    .until_ready_with_jitter(Jitter::up_to(Duration::from_millis(250)))
                    .await;
                match req.send().await {
                    Ok(resp) => {
                        if is_retryable_status(resp.status()) {
                            Err(FetchError::Retryable {
                                retry_after: retry_after_of(&resp),
                                resp: Some(resp),
                            })
                        } else {
                            Ok(resp)
                        }
                    }
                    Err(e) => {
                        if e.is_timeout() || e.is_connect() {
                            Err(FetchError::Retryable {
                                retry_after: None,
                                resp: None,
                            })
                        } else {
                            Err(FetchError::Permanent(format!(
                                "{host} request failed: {e}"
                            )))
                        }
                    }
                }
            }
        };

        // Custom full-jitter schedule fed to backon: `Backoff` is any
        // `Iterator<Item = Duration>`, so we yield our own
        // `backoff_delay` values and `take(MAX_RETRIES)` to bound the
        // attempts. `.adjust` overrides the computed delay with a clamped
        // `Retry-After` when the server supplied one — but only within the
        // existing retry budget. backon hands the closure `None` once the
        // iterator is exhausted; returning `None` there ends the retries, so
        // a `Retry-After` on every response can substitute the delay, never
        // extend the attempt count past `MAX_RETRIES`.
        let mut sched_rng = fastrand::Rng::new();
        let backoff = std::iter::from_fn(move || {
            let d = backoff_delay(attempt, BACKOFF_BASE, BACKOFF_MAX, &mut sched_rng);
            attempt = attempt.saturating_add(1);
            Some(d)
        })
        .take(MAX_RETRIES);

        let result = operation
            .retry(backoff)
            .when(|e| matches!(e, FetchError::Retryable { .. }))
            .adjust(|e, dur| {
                // `None` here means the backoff iterator is exhausted; end
                // the retries rather than letting a `Retry-After` extend them.
                let dur = dur?;
                match e {
                    FetchError::Retryable {
                        retry_after: Some(ra),
                        ..
                    } => Some(*ra),
                    _ => Some(dur),
                }
            })
            .await;

        match result {
            Ok(resp) => Ok(resp),
            // Retries exhausted on a retryable status: hand the last
            // response back so the backend layer classifies it loudly
            // rather than guessing.
            Err(FetchError::Retryable {
                resp: Some(resp), ..
            }) => Ok(resp),
            Err(FetchError::Retryable { resp: None, .. }) => {
                Err(format!("{host} request failed after retries"))
            }
            Err(FetchError::Permanent(msg)) => Err(msg),
        }
    }

    /// A.3.4 — fetch one page for `query` and read its (capped) body.
    /// The single-request-per-query default builds on this: backends call
    /// it once and only page further when a clean first page demands it.
    async fn fetch_text(
        &self,
        host: &str,
        backend: &str,
        build: impl FnMut() -> RequestBuilder,
    ) -> Result<(StatusCode, String), String> {
        let resp = self.fetch(host, build).await?;
        let status = resp.status();
        let body = read_search_body_capped(resp, backend).await?;
        Ok((status, body))
    }
}

/// The retried-operation error. Carries the throttling response (if any)
/// so the final outcome can be surfaced after retries exhaust, and the
/// server's `Retry-After` so the schedule can honour it.
enum FetchError {
    Retryable {
        retry_after: Option<Duration>,
        resp: Option<Response>,
    },
    Permanent(String),
}

/// Read and clamp a response's `Retry-After`, if present.
fn retry_after_of(resp: &Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_retry_after)
}

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

// ============================================================================
// General-web backend dispatch + the single-request-per-query paging engine
// ============================================================================

/// Dispatch one general-web backend (everything except SearXNG, which
/// needs an operator URL, and Wikipedia, the knowledge block). The closed
/// [`BackendId`] match keeps the pool object-safe-free.
pub(super) async fn web_search(
    client: &RotatingClient,
    backend: BackendId,
    query: &str,
    target: usize,
) -> BackendOutcome {
    match backend {
        BackendId::Marginalia => marginalia_search(client, query, target).await,
        // The HTML/paged backends share the single-request-per-query
        // engine; their per-backend page cap differs.
        BackendId::Mojeek => paged_search(client, backend, query, target, MAX_MOJEEK_PAGES).await,
        BackendId::DdgHtml => paged_search(client, backend, query, target, MAX_DDG_PAGES).await,
        BackendId::DdgLite => paged_search(client, backend, query, target, MAX_DDG_PAGES).await,
        BackendId::Brave => paged_search(client, backend, query, target, MAX_BRAVE_PAGES).await,
        // SearXNG and Wikipedia are dispatched by the orchestrator
        // directly; they never reach here.
        BackendId::SearxNg | BackendId::Wiki => BackendOutcome::Results(Vec::new()),
    }
}

/// Fetch one page for a paged HTML backend. The page index is the 0-based
/// page number; each fetcher maps it to its endpoint's offset grammar.
async fn fetch_page_for(
    client: &RotatingClient,
    backend: BackendId,
    query: &str,
    page: usize,
) -> Result<(StatusCode, String), String> {
    match backend {
        BackendId::Mojeek => mojeek_fetch_page(client, query, page).await,
        BackendId::DdgHtml => ddg_html_fetch_page(client, query, page).await,
        BackendId::DdgLite => ddg_lite_fetch_page(client, query, page).await,
        BackendId::Brave => brave_fetch_page(client, query, page).await,
        // Only the paged HTML backends reach this dispatcher.
        BackendId::Marginalia | BackendId::SearxNg | BackendId::Wiki => {
            Err("internal: non-paged backend routed through fetch_page_for".to_owned())
        }
    }
}

/// The single-request-per-query paging engine (A.3.4). Fetches page 0 and
/// classifies it loudly: a throttle / challenge there is the whole answer
/// and is returned as-is rather than parsed into an empty list. It pages
/// further ONLY while the previous page came back clean (parseable,
/// non-empty) AND `target` is not yet met — so the default (a modest
/// `--limit` the pool's breadth already fills) makes exactly one request
/// per backend. Paging is hard-capped at `max_pages`.
async fn paged_search(
    client: &RotatingClient,
    backend: BackendId,
    query: &str,
    target: usize,
    max_pages: usize,
) -> BackendOutcome {
    let mut out: Vec<RawResult> = Vec::with_capacity(target);
    let mut seen: HashSet<String> = HashSet::new();
    for page in 0..max_pages {
        // A fresh cached page short-circuits the HTTP call entirely
        // (A.6) so an agent re-running the same query doesn't re-hit —
        // and risk re-throttling — the backend. A miss / stale / corrupt
        // entry falls through to the live fetch below.
        let rows = if let Some(cached) = cache::get(backend, query, page) {
            cached
        } else {
            let (status, body) = match fetch_page_for(client, backend, query, page).await {
                Ok(pair) => pair,
                // A transport failure with no response is itself a throttle
                // signal on page 0; once we already hold rows, stop and keep
                // them rather than discard a partial win.
                Err(_) => {
                    if out.is_empty() {
                        return BackendOutcome::RateLimited {
                            status: None,
                            retried: super::classify::TRANSPORT_RETRIES,
                        };
                    }
                    break;
                }
            };
            match classify_response(backend, status, &body) {
                BackendOutcome::Results(rows) => {
                    // Only a cleanly-parsed page is cached — never a
                    // throttle or challenge, so a transient block is not
                    // frozen for the TTL.
                    cache::put(backend, query, page, &rows);
                    rows
                }
                // A non-Results outcome on page 0 is the whole answer; on a
                // later page we already have clean rows, so stop and keep them.
                other => {
                    if out.is_empty() {
                        return other;
                    }
                    break;
                }
            }
        };
        // A clean-but-empty page is a genuine no-match: stop paging.
        if rows.is_empty() {
            break;
        }
        for row in rows {
            if seen.insert(canonical_url(&row.url)) {
                out.push(row);
                if out.len() >= target {
                    return BackendOutcome::Results(out);
                }
            }
        }
    }
    BackendOutcome::Results(out)
}

// ============================================================================
// DuckDuckGo HTML + lite backends
// ============================================================================

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

async fn ddg_html_fetch_page(
    client: &RotatingClient,
    query: &str,
    page: usize,
) -> Result<(StatusCode, String), String> {
    let offset = ddg_offset_for_page(page);
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
    // The status travels with the body so the classifier — not this
    // fetcher — decides whether a non-2xx page is a throttle, a
    // challenge, or a parseable result set.
    client
        .fetch_text("html.duckduckgo.com", "ddg", || {
            client
                .client
                .post("https://html.duckduckgo.com/html/")
                .form(&params)
        })
        .await
}

async fn ddg_lite_fetch_page(
    client: &RotatingClient,
    query: &str,
    page: usize,
) -> Result<(StatusCode, String), String> {
    let offset = ddg_offset_for_page(page);
    let mut params: Vec<(&str, String)> = vec![("q", query.to_owned())];
    if offset > 0 {
        params.push(("s", offset.to_string()));
    }
    // The lite endpoint takes the same POST form as the HTML one; only the
    // host and the response markup differ.
    client
        .fetch_text("lite.duckduckgo.com", "ddg-lite", || {
            client
                .client
                .post("https://lite.duckduckgo.com/lite/")
                .form(&params)
        })
        .await
}

// ============================================================================
// Mojeek backend
// ============================================================================

/// Mojeek's result-start parameter (`?s=`): page 0 → 1 (omitted), page 1
/// → 11, page 2 → 21, page 3 → 31. Ten results per page, 1-indexed.
fn mojeek_offset_for_page(page: usize) -> usize {
    page * 10 + 1
}

async fn mojeek_fetch_page(
    client: &RotatingClient,
    query: &str,
    page: usize,
) -> Result<(StatusCode, String), String> {
    let offset = mojeek_offset_for_page(page);
    let offset_s = (offset > 1).then(|| offset.to_string());
    client
        .fetch_text("www.mojeek.com", "mojeek", || {
            let mut req = client
                .client
                .get("https://www.mojeek.com/search")
                .query(&[("q", query)]);
            // The first page omits `s` (Mojeek treats a bare query as
            // offset 1); later pages send the 11 / 21 / 31… start index.
            if let Some(s) = offset_s.as_deref() {
                req = req.query(&[("s", s)]);
            }
            req
        })
        .await
}

// ============================================================================
// Brave Search backend
// ============================================================================

async fn brave_fetch_page(
    client: &RotatingClient,
    query: &str,
    _page: usize,
) -> Result<(StatusCode, String), String> {
    // GET `search.brave.com/search?q=&source=web`. Brave returns ~20
    // organic results on the first page; we drive no pagination (one
    // request, capped by [`MAX_BRAVE_PAGES`]).
    client
        .fetch_text("search.brave.com", "brave", || {
            client
                .client
                .get("https://search.brave.com/search")
                .query(&[("q", query), ("source", "web")])
        })
        .await
}

// ============================================================================
// Marginalia public JSON API backend
// ============================================================================

/// Hit Marginalia's public JSON API (`api.marginalia.nu`, key `public`).
/// A `503` is its documented, clean rate-limit signal (returned as
/// `RateLimited`); a non-JSON 200 is a parse error surfaced as a config
/// error. A single request returns the full small-index result set.
pub(super) async fn marginalia_search(
    client: &RotatingClient,
    query: &str,
    target: usize,
) -> BackendOutcome {
    // A fresh cached page short-circuits the request (A.6).
    if let Some(mut cached) = cache::get(BackendId::Marginalia, query, 0) {
        cached.truncate(target);
        return BackendOutcome::Results(cached);
    }
    // The public endpoint is `/public/search/<query>?count=N`. The query
    // is a path segment, so percent-encode it via `path_segments_mut`
    // (spaces → `%20`, not `+`) the same way the Wikipedia path is built.
    let mut url = match url::Url::parse("https://api.marginalia.nu/public/search") {
        Ok(u) => u,
        Err(e) => return BackendOutcome::ConfigError(format!("internal: bad marginalia URL: {e}")),
    };
    {
        let mut segs = match url.path_segments_mut() {
            Ok(s) => s,
            Err(_) => {
                return BackendOutcome::ConfigError(
                    "internal: marginalia base URL cannot be a base".to_owned(),
                )
            }
        };
        segs.push(query);
    }
    let count = target.to_string();
    let resp = match client
        .fetch("api.marginalia.nu", || {
            client
                .client
                .get(url.clone())
                .query(&[("count", count.as_str())])
                .header("Accept", "application/json")
        })
        .await
    {
        Ok(resp) => resp,
        Err(_) => {
            return BackendOutcome::RateLimited {
                status: None,
                retried: super::classify::TRANSPORT_RETRIES,
            }
        }
    };
    let code = resp.status().as_u16();
    // 503 is Marginalia's documented rate-limit; the generic retry layer
    // already exhausted retries on it, so report it loudly. (Other
    // retryable 5xx/429 also arrive here post-retry and are throttles.)
    if let Some(throttle) = retryable_status_outcome(code) {
        return throttle;
    }
    if code != 200 {
        return BackendOutcome::ConfigError(format!("marginalia HTTP {code}"));
    }
    let body = match read_search_body_capped(resp, "marginalia").await {
        Ok(b) => b,
        Err(e) => return BackendOutcome::ConfigError(e),
    };
    match marginalia_parse_json(&body) {
        Ok(mut rows) => {
            // Cache the full parsed set so a later run with a higher
            // `--limit` is served fully from cache, then truncate only the
            // value returned to this caller.
            cache::put(BackendId::Marginalia, query, 0, &rows);
            rows.truncate(target);
            BackendOutcome::Results(rows)
        }
        Err(e) => BackendOutcome::ConfigError(e),
    }
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
pub(super) async fn wiki_summary(
    client: &RotatingClient,
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
        .fetch("en.wikipedia.org", || {
            client
                .client
                .get(url.clone())
                .header("Accept", "application/json")
        })
        .await?;
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

pub(super) async fn searxng_search(
    client: &RotatingClient,
    base: &str,
    query: &str,
    limit: usize,
) -> BackendOutcome {
    // A fresh cached page short-circuits the request (A.6). The cache key
    // folds the instance base in alongside the query so two configured
    // instances never serve each other's cached results.
    let cache_key = format!("{base}\0{query}");
    if let Some(mut cached) = cache::get(BackendId::SearxNg, &cache_key, 0) {
        cached.truncate(limit);
        return BackendOutcome::Results(cached);
    }
    let base = base.trim_end_matches('/');
    let url = format!("{base}/search");
    let host = url::Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_owned()))
        .unwrap_or_else(|| "searxng".to_owned());
    let params = [("q", query), ("format", "json")];
    let resp = match client
        .fetch(&host, || {
            client
                .client
                .get(&url)
                .query(&params)
                .header("Accept", "application/json")
        })
        .await
    {
        Ok(resp) => resp,
        Err(_) => {
            return BackendOutcome::RateLimited {
                status: None,
                retried: super::classify::TRANSPORT_RETRIES,
            }
        }
    };
    let code = resp.status().as_u16();
    // 429/503/5xx survived the retry layer — a throttle.
    if let Some(throttle) = retryable_status_outcome(code) {
        return throttle;
    }
    // 403 on a SearXNG instance is a misconfiguration (the operator
    // blocked us), not a per-IP throttle — surface it as a config error so
    // an agent knows to fix the instance, not to wait and retry.
    if code == 403 {
        return BackendOutcome::ConfigError(format!(
            "searxng instance refused the request (HTTP {code}) — check instance ACL"
        ));
    }
    if code != 200 {
        return BackendOutcome::ConfigError(format!("searxng HTTP {code}"));
    }
    let body = match read_search_body_capped(resp, "searxng").await {
        Ok(b) => b,
        Err(e) => return BackendOutcome::ConfigError(e),
    };
    let parsed: SearxResponse = match serde_json::from_str(&body) {
        Ok(p) => p,
        // Most public SearXNG instances disable JSON output and answer a
        // `format=json` request with an HTML page — a config error, not a
        // throttle.
        Err(_) => {
            return BackendOutcome::ConfigError(
                "searxng returned non-JSON for format=json — enable the JSON output \
                 format on the instance (most public instances disable it)"
                    .to_owned(),
            )
        }
    };
    let out: Vec<RawResult> = parsed
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
                source: BackendId::SearxNg,
            })
        })
        .collect();
    // Cache the full parsed set so a later run with a higher `--limit` is
    // served fully from cache, then truncate only the returned value.
    cache::put(BackendId::SearxNg, &cache_key, 0, &out);
    BackendOutcome::Results(out.into_iter().take(limit).collect())
}

// ============================================================================
// Tests — offset arithmetic + the always-on backoff/Retry-After helpers.
// Fetch paths are exercised via `tests/search.rs`.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddg_offsets_match_python_lib() {
        assert_eq!(ddg_offset_for_page(0), 0);
        assert_eq!(ddg_offset_for_page(1), 10);
        assert_eq!(ddg_offset_for_page(2), 25);
        assert_eq!(ddg_offset_for_page(3), 40);
    }

    #[test]
    fn mojeek_offsets_increment_by_ten() {
        assert_eq!(mojeek_offset_for_page(0), 1);
        assert_eq!(mojeek_offset_for_page(1), 11);
        assert_eq!(mojeek_offset_for_page(2), 21);
        assert_eq!(mojeek_offset_for_page(3), 31);
    }

    #[test]
    fn backoff_delay_stays_within_cap() {
        let mut rng = fastrand::Rng::with_seed(42);
        // Every sample must land in `[0, min(base*2^attempt, max)]`.
        for attempt in 0..6u32 {
            let scaled = (BACKOFF_BASE.as_millis() as u64).saturating_mul(1u64 << attempt);
            let cap = scaled.min(BACKOFF_MAX.as_millis() as u64);
            for _ in 0..1000 {
                let d = backoff_delay(attempt, BACKOFF_BASE, BACKOFF_MAX, &mut rng);
                assert!(
                    d.as_millis() as u64 <= cap,
                    "attempt {attempt}: {d:?} exceeded cap {cap}ms"
                );
            }
        }
    }

    #[test]
    fn backoff_delay_caps_saturate_at_max() {
        let mut rng = fastrand::Rng::with_seed(7);
        // Once `base * 2^attempt` exceeds `max`, the cap is exactly `max`.
        let max_ms = BACKOFF_MAX.as_millis() as u64;
        for _ in 0..2000 {
            let d = backoff_delay(20, BACKOFF_BASE, BACKOFF_MAX, &mut rng);
            assert!(d.as_millis() as u64 <= max_ms);
        }
    }

    #[test]
    fn backoff_delay_no_overflow_at_large_attempt() {
        let mut rng = fastrand::Rng::with_seed(1);
        // `attempt` at and beyond the 64-bit shift width must not panic
        // (debug overflow) and must still respect the cap.
        let max_ms = BACKOFF_MAX.as_millis() as u64;
        for attempt in [63u32, 64, 100, u32::MAX] {
            let d = backoff_delay(attempt, BACKOFF_BASE, BACKOFF_MAX, &mut rng);
            assert!(d.as_millis() as u64 <= max_ms);
        }
    }

    #[test]
    fn retry_after_parses_seconds() {
        assert_eq!(parse_retry_after("5"), Some(Duration::from_secs(5)));
        assert_eq!(parse_retry_after("  12 "), Some(Duration::from_secs(12)));
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_seconds_clamped_at_30s() {
        assert_eq!(parse_retry_after("3600"), Some(RETRY_AFTER_CAP));
        assert_eq!(parse_retry_after("31"), Some(RETRY_AFTER_CAP));
        assert_eq!(parse_retry_after("30"), Some(Duration::from_secs(30)));
    }

    #[test]
    fn retry_after_http_date_clamped_at_30s() {
        // An HTTP-date far in the future must clamp to the cap, never the
        // raw multi-year delay.
        let far = "Wed, 21 Oct 2099 07:28:00 GMT";
        assert_eq!(parse_retry_after(far), Some(RETRY_AFTER_CAP));
    }

    #[test]
    fn retry_after_http_date_in_past_is_zero() {
        // A date already in the past yields no wait.
        let past = "Wed, 21 Oct 2015 07:28:00 GMT";
        assert_eq!(parse_retry_after(past), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_garbage_is_none() {
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn is_retryable_status_classifies_per_spider() {
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS)); // 429
        assert!(is_retryable_status(StatusCode::REQUEST_TIMEOUT)); // 408
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR)); // 500
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE)); // 503
        // 403 is "you are blocked" — permanent, never retried.
        assert!(!is_retryable_status(StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(StatusCode::NOT_IMPLEMENTED)); // 501
        assert!(!is_retryable_status(StatusCode::OK));
    }

    #[test]
    fn marginalia_503_is_rate_limited() {
        // Marginalia's documented rate-limit signal is a clean 503; the
        // JSON backends route a post-retry retryable status through this
        // shared helper, which must surface it loudly (never an empty).
        let out = retryable_status_outcome(503);
        assert!(matches!(
            out,
            Some(BackendOutcome::RateLimited {
                status: Some(503),
                ..
            })
        ));
    }

    #[test]
    fn retryable_status_outcome_passes_clean_statuses_through() {
        // 200 and 403 are not throttles for the JSON backends: 200 is a
        // body to parse, 403 a SearXNG config error handled separately.
        assert!(retryable_status_outcome(200).is_none());
        assert!(retryable_status_outcome(403).is_none());
    }

    #[test]
    fn cooldown_round_trip_skips_only_the_marked_backend() {
        // A backend marked cooled-down this run is skipped by the
        // orchestrator; others are untouched. Idempotent re-marking is a
        // no-op.
        let client = RotatingClient::new(None).expect("build client");
        assert!(!client.is_cooled_down(BackendId::Brave));
        client.cool_down(BackendId::Brave);
        client.cool_down(BackendId::Brave);
        assert!(client.is_cooled_down(BackendId::Brave));
        assert!(!client.is_cooled_down(BackendId::Mojeek));
    }
}
