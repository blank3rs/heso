//! # serve
//!
//! `heso serve` — line-delimited JSON-RPC 2.0 server over stdin/stdout.
//!
//! Lets a framework author (Browser Use, Stagehand, a Playwright-style
//! wrapper, an in-house agent) integrate against heso **once** and stay
//! in-process — instead of spawning `heso open <url>` per call, they
//! launch one `heso serve` child and pipe newline-delimited JSON
//! requests in / responses out.
//!
//! ## Wire format
//!
//! Each line on stdin is one JSON-RPC 2.0 request:
//!
//! ```json
//! {"jsonrpc":"2.0","id":1,"method":"open","params":{"url":"https://stripe.com"}}
//! ```
//!
//! Each line on stdout is one response (success):
//!
//! ```json
//! {"jsonrpc":"2.0","id":1,"result":{"page_id":"p1", ...}}
//! ```
//!
//! …or one response (error):
//!
//! ```json
//! {"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"unknown method"}}
//! ```
//!
//! When the server starts, it emits one **notification** (no `id`) so the
//! client can confirm it's alive and see the supported methods:
//!
//! ```json
//! {"jsonrpc":"2.0","method":"ready","params":{"version":"...","methods":["open","ls","cat","find","close","ping","fill","click","submit","eval","navigate"]}}
//! ```
//!
//! ## Methods
//!
//! | Method   | Params | Result |
//! |----------|--------|--------|
//! | `open`     | `{url, explore_links_depth?: u8, link_cap?: usize}` | `{page_id, url, title, description, metadata, tree, actions, linked_pages?}` |
//! | `ls`       | `{page_id, path?}` | `{path, entries: [LsRow, ...]}` |
//! | `cat`      | `{page_id, target}` | `{path, content}` for a tree path, or `ElementRef` for `@ref` |
//! | `find`     | `{page_id, role?, name_substr?, section?}` | `{count, matches: [ElementRef, ...]}` |
//! | `close`    | `{page_id}` | `{closed: bool}` |
//! | `ping`     | none | `"pong"` |
//! | `fill`     | `{ref, value, page_id?}` | `{ok, op, url, ref, selector, value, console}` |
//! | `click`    | `{ref, page_id?}` | `{ok, op, url, ref, selector, value, console}` |
//! | `submit`   | `{ref, field?: {name: value}, data?: {name: value}, page_id?}` | `{ok, op, url, ref, selector, value, console, postUrl}` |
//! | `eval`     | `{js, page_id?}` | `{ok, url, value, console}` |
//! | `navigate` | `{url, page_id?}` | `{ok, url, page_id, scripts}` |
//! | `read`     | `{page_id?, include?}` | `{url, title, text, tree, actions, forms, cookies, console, framework, scripts, plat_hash}` |
//! | `wait`     | `{page_id?, selector_exists?, text_contains?, url_matches?, network_idle?, idle_window_ms?, time_ms?, timeout_ms?}` | `{ok, elapsed_ms, condition, error?}` |
//!
//! `open` with `explore_links_depth >= 1` pre-fetches up to `link_cap`
//! same-origin links per level and embeds their tree + metadata + actions
//! under `linked_pages`. Defaults match the CLI: `explore_links_depth = 0`
//! (off) and `link_cap = 20` (hard max 50). Per-link errors are captured
//! as `linked_pages[i].error` and do not fail the whole call.
//!
//! Pages are cached server-side keyed by the returned `page_id` (`p1`,
//! `p2`, …). Drop with `close` when done.
//!
//! ## Stateful write methods (PR-Y2)
//!
//! `fill`, `click`, `submit`, `eval`, and `navigate` operate on a
//! per-page-id `JsSession` that is created lazily on the first
//! write-verb call. Subsequent calls to the same `page_id` see the
//! DOM mutations from previous calls — that's the whole point of the
//! sessioned interface vs. the per-process CLI verbs. Cookies, the
//! virtual clock, the seeded RNG, and any in-engine fetch state survive
//! across `navigate` (the session reuses its engine; only the DOM
//! resets). The `@e7` action-ref vocabulary is the same one the read
//! methods use; after `navigate` the action graph is re-extracted
//! against the new page so refs continue to resolve. When `page_id` is
//! omitted on a write verb, the server defaults to the most recently
//! opened (or navigated) page.
//!
//! `eval` runs JS against the live `document` global — globals set in
//! one `eval` call are observable in the next. `submit` accepts the
//! same `--field NAME=VALUE` / `--data JSON` shape as the CLI verb (via
//! `field` / `data` JSON params), and applies the same merge rules:
//! `field` overrides `data` on key collisions.
//!
//! ## Concurrency
//!
//! v1 is **strictly sequential**: read a line, dispatch, write a line,
//! repeat. The `page_id` indirection lets a future version pipeline
//! requests across multiple pages without changing the wire format.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use heso_core::Url;
use heso_engine_api::{EngineApi, Page};
use heso_engine_fetch::{
    linked_pages_to_json, resolve_action, resolve_locator_from_html, ElementRef, ExploreOptions,
    FetchEngine, FetchPage, LocatorError, DEFAULT_LINK_CAP, HARD_LINK_CAP,
};
use heso_engine_js::{JsEngine, JsSession, ScriptFetchPolicy, WaitCondition};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::{
    attach_failure_envelope, classify_failure_envelope, collect_cookies, compute_delta,
    delta_no_prior, detect_framework, group_forms, merge_submit_fields, parse_include_filter,
    selector_for_action, ReadSnapshot,
};

/// Cap on the per-session `read` snapshot store. After the 8th distinct
/// URL is read, the least-recently-touched URL is evicted. Bounded
/// memory is the only reason for the cap — an agent that ping-pongs
/// across 50 URLs in one `serve` session is still mostly going to be
/// interested in the last few it touched. 8 was picked over 4 to give
/// click-heavy multi-page flows some headroom; over 16 because past
/// that the linear LRU scan starts to feel silly.
const SNAPSHOT_LRU_CAP: usize = 8;

// ============================================================================
// JSON-RPC types
// ============================================================================

#[derive(Deserialize)]
struct Request {
    jsonrpc: String,
    #[serde(default = "default_id")]
    id: serde_json::Value,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

fn default_id() -> serde_json::Value {
    serde_json::Value::Null
}

#[derive(Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

// Per JSON-RPC 2.0 spec.
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INTERNAL_ERROR: i32 = -32603;

/// Method list advertised by the `ready` notification and by errors
/// pointing the client at the supported set. Keep in sync with the
/// `match` arms in [`handle`].
const ADVERTISED_METHODS: &[&str] = &[
    "open", "ls", "cat", "find", "close", "ping", "fill", "click", "submit", "eval", "navigate",
    "read", "wait",
];

// ============================================================================
// Server state
// ============================================================================

/// One cached page. Holds the static [`FetchPage`] used by the read
/// methods (ls/cat/find) plus a **lazily-built** [`JsSession`] used by
/// the write methods (fill/click/submit/eval/navigate).
///
/// The session is `None` for pure read-only flows so we don't pay the
/// QuickJS init cost when nothing needs JS. The first write-verb call
/// constructs the session from the cached `page.body_html` and keeps
/// it for every subsequent write on the same `page_id`.
///
/// `current_actions` is replaced by `navigate` so `@e7` refs resolve
/// against whatever page the session is currently on. For unmodified
/// `open`-then-write flows it stays equal to `page.actions`.
struct PageRecord {
    page: FetchPage,
    session: Option<JsSession>,
    current_actions: Vec<ElementRef>,
}

impl PageRecord {
    fn new(page: FetchPage) -> Self {
        let actions = page.actions.clone();
        Self {
            page,
            session: None,
            current_actions: actions,
        }
    }
}

struct ServerState {
    engine: FetchEngine,
    /// `page_id` → `PageRecord`. Wrapped in a `tokio::sync::Mutex` so
    /// handlers can `.await` (e.g. the `navigate` HTTP fetch) without
    /// holding the lock — they take the record briefly, do async work,
    /// then re-acquire.
    pages: Mutex<HashMap<String, PageRecord>>,
    /// Monotonic page id counter. `AtomicU64` avoids a second lock on
    /// every `open`.
    counter: AtomicU64,
    /// Most-recently opened (or navigated) `page_id`. Used as the
    /// default when a write verb omits `page_id`, so single-page agents
    /// can stay terse. `None` until the first `open` succeeds.
    last_page_id: Mutex<Option<String>>,
    /// LRU snapshot store keyed by URL — populated by every `read`,
    /// consulted by the next `read` with `--since <hash>`. Bounded at
    /// `SNAPSHOT_LRU_CAP` entries; on touch the matched URL moves to
    /// the front (Vec back == evict candidate). One store per `serve`
    /// process; not persisted across runs.
    ///
    /// Why URL-keyed and not page_id-keyed: a `navigate` swaps the URL
    /// on an existing `page_id`, so an agent that reads A → navigates
    /// to B → reads → expects to diff B against its previous B read
    /// should match by URL, not by page_id. The agent passes the prior
    /// `content_hash` it observed; we look up snapshots for the
    /// current URL and check if any one has that hash.
    snapshots: Mutex<Vec<(Url, ReadSnapshot)>>,
}

impl ServerState {
    fn new() -> heso_core::Result<Self> {
        Ok(Self {
            engine: FetchEngine::new()?,
            pages: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            last_page_id: Mutex::new(None),
            snapshots: Mutex::new(Vec::new()),
        })
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("p{n}")
    }
}

/// Look up the snapshot stored under `url` and return a *clone* of it
/// IFF its `content_hash` matches `since`. None means "no prior
/// snapshot for this URL, or it didn't match the hash the agent
/// supplied" — both cases collapse to `delta.since_matched: false` at
/// the caller.
///
/// Mutates the store to bump the matched URL to the front (LRU
/// touch). Caller already holds the `pages` lock briefly above this
/// call, but the snapshot store is its own Mutex so the read-side
/// lookup and the post-read write are independent of the page lock.
fn find_snapshot(store: &mut Vec<(Url, ReadSnapshot)>, url: &Url, since: &str) -> Option<ReadSnapshot> {
    let idx = store.iter().position(|(u, _)| u == url)?;
    if store[idx].1.content_hash != since {
        return None;
    }
    // LRU touch — move matched entry to front.
    let (u, snap) = store.remove(idx);
    let cloned = snap.clone();
    store.insert(0, (u, snap));
    Some(cloned)
}

/// Insert/replace the snapshot for `url` and evict beyond
/// `SNAPSHOT_LRU_CAP`. New (or refreshed) entry lands at the front;
/// the oldest tail entry drops out when we overflow.
fn install_snapshot(store: &mut Vec<(Url, ReadSnapshot)>, url: Url, snap: ReadSnapshot) {
    if let Some(idx) = store.iter().position(|(u, _)| u == &url) {
        store.remove(idx);
    }
    store.insert(0, (url, snap));
    while store.len() > SNAPSHOT_LRU_CAP {
        store.pop();
    }
}

// ============================================================================
// Entry point
// ============================================================================

/// Run the JSON-RPC server until stdin closes. Returns the exit code.
pub async fn run() -> ExitCode {
    // `ServerState` is `!Send + !Sync` because each `JsSession` holds an
    // `Arc<dom_query::Document>` (a `RefCell`-backed mutable DOM, see
    // `heso_engine_js::dom`). We still wrap it in `Arc` to share the
    // server-wide state across async handler calls within the same task;
    // the strictly-sequential request loop guarantees the Arc never
    // actually crosses a thread boundary, matching the same reasoning
    // documented on `Document::from_html`.
    #[allow(clippy::arc_with_non_send_sync)]
    let state = match ServerState::new() {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("failed to construct server: {e}");
            return ExitCode::FAILURE;
        }
    };

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    // One-shot ready notification so clients can confirm the server is up
    // and learn the method list without a probe round-trip.
    let hello = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "ready",
        "params": {
            "version": env!("CARGO_PKG_VERSION"),
            "methods": ADVERTISED_METHODS,
        }
    });
    let hello_line = serde_json::to_string(&hello).unwrap_or_else(|_| String::from("{}"));
    if write_line(&mut stdout, &hello_line).await.is_err() {
        return ExitCode::FAILURE;
    }

    loop {
        let line = match reader.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break, // stdin closed → clean exit
            Err(e) => {
                eprintln!("stdin read error: {e}");
                return ExitCode::FAILURE;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = handle(state.clone(), trimmed).await;
        // Serialize the Response straight to a String — `serde_json::to_string`
        // walks the struct directly, no intermediate `Value` tree.
        let line = match serde_json::to_string(&response) {
            Ok(s) => s,
            Err(_) => {
                // Pre-canonicalized fallback. Tiny, no allocation
                // beyond the single static literal copy.
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"failed to serialize response"}}"#
                    .to_owned()
            }
        };
        if write_line(&mut stdout, &line).await.is_err() {
            // stdout closed by the parent — we're done.
            break;
        }
    }
    ExitCode::SUCCESS
}

async fn write_line(out: &mut tokio::io::Stdout, line: &str) -> std::io::Result<()> {
    out.write_all(line.as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await
}

// ============================================================================
// Dispatch
// ============================================================================

async fn handle(state: Arc<ServerState>, line: &str) -> Response {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                serde_json::Value::Null,
                PARSE_ERROR,
                format!("parse error: {e}"),
            );
        }
    };
    if req.jsonrpc != "2.0" {
        return error_response(req.id, INVALID_REQUEST, "`jsonrpc` must be \"2.0\"".into());
    }
    let id = req.id.clone();
    let result = match req.method.as_str() {
        "open" => dispatch_open(state.clone(), req.params).await,
        "ls" => dispatch_ls(state.clone(), req.params).await,
        "cat" => dispatch_cat(state.clone(), req.params).await,
        "find" => dispatch_find(state.clone(), req.params).await,
        "close" => dispatch_close(state.clone(), req.params).await,
        "ping" => Ok(serde_json::json!("pong")),
        "fill" => dispatch_fill(state.clone(), req.params).await,
        "click" => dispatch_click(state.clone(), req.params).await,
        "submit" => dispatch_submit(state.clone(), req.params).await,
        "eval" => dispatch_eval(state.clone(), req.params).await,
        "navigate" => dispatch_navigate(state.clone(), req.params).await,
        "read" => dispatch_read(state.clone(), req.params).await,
        "wait" => dispatch_wait(state.clone(), req.params).await,
        m => {
            return error_response(id, METHOD_NOT_FOUND, format!("unknown method `{m}`"));
        }
    };
    match result {
        Ok(v) => success(id, v),
        Err(msg) => error_response(id, INTERNAL_ERROR, msg),
    }
}

fn success(id: serde_json::Value, result: serde_json::Value) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn error_response(id: serde_json::Value, code: i32, message: String) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError { code, message }),
    }
}

// ============================================================================
// Method handlers — read methods (unchanged behavior)
// ============================================================================

#[derive(Deserialize)]
struct OpenParams {
    url: String,
    /// Optional depth for link-graph cartography. `0` (or omitted) keeps
    /// the classic static-only behavior. `1` pre-fetches direct
    /// same-origin links; `>=2` recurses.
    #[serde(default)]
    explore_links_depth: u8,
    /// Optional cap on links followed per level. Clamped server-side to
    /// `HARD_LINK_CAP` ([`HARD_LINK_CAP`]). Defaults to
    /// `DEFAULT_LINK_CAP` ([`DEFAULT_LINK_CAP`]).
    #[serde(default)]
    link_cap: Option<usize>,
}

async fn dispatch_open(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: OpenParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let url = Url::parse(&p.url).map_err(|e| format!("invalid URL: {e}"))?;
    let opts = ExploreOptions {
        depth: p.explore_links_depth,
        link_cap: p.link_cap.unwrap_or(DEFAULT_LINK_CAP).min(HARD_LINK_CAP),
    };
    let page = state
        .engine
        .open_with_explore(&url, opts)
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    let page_id = state.next_id();
    let mut payload = serde_json::json!({
        "page_id": &page_id,
        "url": page.url().as_str(),
        "title": page.tree.title,
        "description": page.tree.description,
        "metadata": page.metadata,
        "tree": page.tree,
        "actions": page.actions,
    });
    if !page.inline_data.is_empty() {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(
                "inline_data".to_owned(),
                serde_json::to_value(&page.inline_data).unwrap_or(serde_json::Value::Null),
            );
        }
    }
    if !page.data_attrs.is_empty() {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(
                "data_attrs".to_owned(),
                serde_json::to_value(&page.data_attrs).unwrap_or(serde_json::Value::Null),
            );
        }
    }
    if !page.linked_pages.is_empty() {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(
                "linked_pages".to_owned(),
                linked_pages_to_json(&page.linked_pages),
            );
        }
    }
    // Structured-failure envelope: `dispatch_open` does not run the
    // JS-side script pump (the page is parsed statically and the JS
    // session is built lazily by the first write verb). We always
    // emit the envelope per the schema bump — a future enhancement
    // could opt into hydration here when the caller passes a
    // best-effort flag through `OpenParams`. For now, the envelope
    // reports the trivially-clean shape.
    attach_failure_envelope(&mut payload, false, "ok", &[], 0);
    // Compute the plat_hash AFTER all content fields are in place but
    // BEFORE re-attaching `page_id` (which is server-instance-scoped
    // and would bias the hash). Detach `page_id` from the payload,
    // hash the rest, re-attach. Previously this cloned the whole
    // payload tree just to drop one key — for `--explore-links` plats
    // with many linked pages, that clone dominated.
    let page_id_value = payload
        .as_object_mut()
        .and_then(|obj| obj.remove("page_id"));
    let hash = heso_engine_fetch::plat_hash(&payload);
    if let Some(obj) = payload.as_object_mut() {
        if let Some(pid) = page_id_value {
            obj.insert("page_id".to_owned(), pid);
        }
        obj.insert("plat_hash".to_owned(), serde_json::Value::String(hash));
    }
    state
        .pages
        .lock()
        .await
        .insert(page_id.clone(), PageRecord::new(page));
    *state.last_page_id.lock().await = Some(page_id);
    Ok(payload)
}

fn default_root_path() -> String {
    "/".to_owned()
}

#[derive(Deserialize)]
struct LsParams {
    page_id: String,
    #[serde(default = "default_root_path")]
    path: String,
}

async fn dispatch_ls(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: LsParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let pages = state.pages.lock().await;
    let record = pages
        .get(&p.page_id)
        .ok_or_else(|| format!("no page_id `{}`", p.page_id))?;
    let rows = record.page.tree.ls(&p.path).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "path": p.path, "entries": rows }))
}

#[derive(Deserialize)]
struct CatParams {
    page_id: String,
    target: String,
}

async fn dispatch_cat(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: CatParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let pages = state.pages.lock().await;
    let record = pages
        .get(&p.page_id)
        .ok_or_else(|| format!("no page_id `{}`", p.page_id))?;
    if let Some(stripped) = p.target.strip_prefix('@') {
        let want = format!("@{stripped}");
        match heso_engine_fetch::resolve_action(&record.page.actions, &want) {
            Some(el) => serde_json::to_value(el).map_err(|e| e.to_string()),
            None => Err(format!("no element at ref `{want}`")),
        }
    } else {
        let content = record.page.tree.cat(&p.target).map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "path": p.target, "content": content }))
    }
}

#[derive(Deserialize)]
struct FindParams {
    page_id: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name_substr: Option<String>,
    #[serde(default)]
    section: Option<String>,
}

async fn dispatch_find(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: FindParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let pages = state.pages.lock().await;
    let record = pages
        .get(&p.page_id)
        .ok_or_else(|| format!("no page_id `{}`", p.page_id))?;
    let matches = heso_engine_fetch::filter_actions(
        &record.page.actions,
        p.role.as_deref(),
        p.name_substr.as_deref(),
        p.section.as_deref(),
    );
    Ok(serde_json::json!({ "count": matches.len(), "matches": matches }))
}

#[derive(Deserialize)]
struct CloseParams {
    page_id: String,
}

async fn dispatch_close(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: CloseParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let mut pages = state.pages.lock().await;
    let removed = pages.remove(&p.page_id);
    // If the closed page_id was the most-recent default, clear it so
    // subsequent default-pageless write calls produce a clean "no
    // active session" error instead of dangling at a stale id.
    let mut last = state.last_page_id.lock().await;
    if last.as_deref() == Some(p.page_id.as_str()) {
        *last = None;
    }
    Ok(serde_json::json!({ "closed": removed.is_some() }))
}

// ============================================================================
// Method handlers — write methods (PR-Y2)
// ============================================================================

/// Locator shape mirrored from the CLI flags. When provided as a
/// `locator` JSON object on a write-method call, it is an alternative
/// to the `ref` field — exactly one of the two must be supplied.
/// Multiple matches yield an error carrying the candidate refs; zero
/// matches yields a clear "no match" error.
#[derive(Deserialize, Default, Clone)]
struct LocatorParams {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    #[serde(default, alias = "aria-label")]
    aria_label: Option<String>,
}

impl LocatorParams {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.selector.is_none() && self.aria_label.is_none()
    }
}

/// Resolve either an `@e<N>` ref OR a `LocatorParams` against a page's
/// action graph. Returns an owned [`ElementRef`] so the caller can drop
/// the lock without borrow trouble. Errors come back as JSON-RPC-shaped
/// `String`s.
fn resolve_ref_or_locator(
    record: &PageRecord,
    ref_str: Option<&str>,
    locator: Option<&LocatorParams>,
) -> Result<ElementRef, String> {
    let has_ref = ref_str.is_some();
    let has_locator = locator.map(|l| !l.is_empty()).unwrap_or(false);
    if !has_ref && !has_locator {
        return Err(
            "need either `ref` or `locator: {text|selector|aria_label}`".to_owned(),
        );
    }
    if has_ref && has_locator {
        return Err("cannot pass both `ref` and `locator`".to_owned());
    }
    if let Some(rs) = ref_str {
        let want = normalize_ref(rs);
        return resolve_action(&record.current_actions, &want)
            .cloned()
            .ok_or_else(|| format!("no element at ref `{want}`"));
    }
    let l = locator.expect("locator present (checked above)");
    let mut matches = resolve_locator_from_html(
        &record.page.body_html,
        &record.current_actions,
        l.text.as_deref(),
        l.selector.as_deref(),
        l.aria_label.as_deref(),
    )
    .map_err(|e| match e {
        LocatorError::BadSelector { selector, message } => {
            format!("invalid CSS selector `{selector}`: {message}")
        }
    })?;
    match matches.len() {
        0 => Err(format!(
            "no element matched locator {}",
            format_locator(l.text.as_deref(), l.selector.as_deref(), l.aria_label.as_deref())
        )),
        1 => Ok(matches.remove(0)),
        n => {
            let candidates: Vec<serde_json::Value> = matches
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "ref": c.ref_id,
                        "role": c.role,
                        "tag": c.tag,
                        "name": c.name,
                    })
                })
                .collect();
            // Pack the candidates list into the error message so a
            // JSON-RPC client can parse it programmatically. The
            // leading prose keeps human-tail logs readable.
            let body = serde_json::json!({
                "kind": "ambiguous",
                "matched": n,
                "locator": {
                    "text": l.text,
                    "selector": l.selector,
                    "aria_label": l.aria_label,
                },
                "candidates": candidates,
            });
            Err(format!(
                "ambiguous: {n} elements matched; candidates={body}"
            ))
        }
    }
}

/// Render the supplied locator filters back as a `{k: "v"}`-ish blob
/// for error messages. Mirrors the CLI's `format_locator`.
fn format_locator(
    text: Option<&str>,
    css_selector: Option<&str>,
    aria_label: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if let Some(v) = text {
        parts.push(format!(
            "text: {}",
            serde_json::to_string(v).unwrap_or_else(|_| "\"\"".to_owned())
        ));
    }
    if let Some(v) = css_selector {
        parts.push(format!(
            "selector: {}",
            serde_json::to_string(v).unwrap_or_else(|_| "\"\"".to_owned())
        ));
    }
    if let Some(v) = aria_label {
        parts.push(format!(
            "aria-label: {}",
            serde_json::to_string(v).unwrap_or_else(|_| "\"\"".to_owned())
        ));
    }
    format!("{{ {} }}", parts.join(", "))
}

#[derive(Deserialize)]
struct FillParams {
    #[serde(default, rename = "ref")]
    ref_str: Option<String>,
    #[serde(default)]
    locator: Option<LocatorParams>,
    value: String,
    #[serde(default)]
    page_id: Option<String>,
}

async fn dispatch_fill(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: FillParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let page_id = resolve_page_id(&state, p.page_id).await?;
    let mut pages = state.pages.lock().await;
    let record = pages
        .get_mut(&page_id)
        .ok_or_else(|| format!("no page_id `{}`", page_id))?;
    let elem = resolve_ref_or_locator(record, p.ref_str.as_deref(), p.locator.as_ref())?;
    let want = elem.ref_id.clone();
    let selector = selector_for_action(&elem).ok_or_else(|| {
        format!(
            "could not build a CSS selector for `{want}` (tag={:?}, attrs={:?})",
            elem.tag, elem.attrs
        )
    })?;

    ensure_session(&state.engine, record)?;
    let session = record.session.as_ref().expect("session ensured above");

    let outcome = session
        .fill(&selector, &p.value)
        .map_err(|e| format!("js fill failed: {e}"))?;
    Ok(serde_json::json!({
        "ok": true,
        "op": "fill",
        "url": session.url().to_string(),
        "ref": want,
        "selector": selector,
        "value": outcome.value,
        "console": outcome.console,
    }))
}

#[derive(Deserialize)]
struct ClickParams {
    #[serde(default, rename = "ref")]
    ref_str: Option<String>,
    #[serde(default)]
    locator: Option<LocatorParams>,
    #[serde(default)]
    page_id: Option<String>,
}

async fn dispatch_click(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: ClickParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let page_id = resolve_page_id(&state, p.page_id).await?;
    let mut pages = state.pages.lock().await;
    let record = pages
        .get_mut(&page_id)
        .ok_or_else(|| format!("no page_id `{}`", page_id))?;
    let elem = resolve_ref_or_locator(record, p.ref_str.as_deref(), p.locator.as_ref())?;
    let want = elem.ref_id.clone();
    let selector = selector_for_action(&elem).ok_or_else(|| {
        format!(
            "could not build a CSS selector for `{want}` (tag={:?}, attrs={:?})",
            elem.tag, elem.attrs
        )
    })?;

    ensure_session(&state.engine, record)?;
    let session = record.session.as_ref().expect("session ensured above");

    let outcome = session
        .click(&selector)
        .map_err(|e| format!("js click failed: {e}"))?;
    Ok(serde_json::json!({
        "ok": true,
        "op": "click",
        "url": session.url().to_string(),
        "ref": want,
        "selector": selector,
        "value": outcome.value,
        "console": outcome.console,
    }))
}

#[derive(Deserialize)]
struct SubmitParams {
    #[serde(default, rename = "ref")]
    ref_str: Option<String>,
    #[serde(default)]
    locator: Option<LocatorParams>,
    /// Optional `name → value` overrides (CLI `--field NAME=VALUE`
    /// equivalent). Values are JSON; strings/numbers/bools coerce to
    /// strings, `null` becomes empty. Other shapes are an error.
    #[serde(default)]
    field: Option<serde_json::Value>,
    /// Optional dict of `name → value` (CLI `--data JSON` equivalent).
    /// Same coercion rules. On key collision, `field` wins (matches the
    /// CLI's apply-order).
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    page_id: Option<String>,
}

async fn dispatch_submit(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: SubmitParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let page_id = resolve_page_id(&state, p.page_id).await?;

    // Parse both field / data dicts into the (name, value) shape
    // `JsSession::submit_with_fields` consumes. Mirror the CLI's
    // tolerance: strings, numbers, bools, null all coerce. Arrays /
    // nested objects are rejected with a clear message — the spec key
    // for form fields is one string per `name`.
    let field_fields = parse_field_dict(p.field.as_ref(), "field")?;
    let data_fields = parse_field_dict(p.data.as_ref(), "data")?;
    let merged = merge_submit_fields(&data_fields, &field_fields);

    let mut pages = state.pages.lock().await;
    let record = pages
        .get_mut(&page_id)
        .ok_or_else(|| format!("no page_id `{}`", page_id))?;
    let elem = resolve_ref_or_locator(record, p.ref_str.as_deref(), p.locator.as_ref())?;
    let want = elem.ref_id.clone();
    let selector = selector_for_action(&elem).ok_or_else(|| {
        format!(
            "could not build a CSS selector for `{want}` (tag={:?}, attrs={:?})",
            elem.tag, elem.attrs
        )
    })?;

    ensure_session(&state.engine, record)?;
    let session = record.session.as_mut().expect("session ensured above");

    let outcome = session
        .submit_with_fields(&selector, &merged)
        .map_err(|e| format!("js submit failed: {e}"))?;
    let post_url = session.url().to_string();
    // The post-submit URL may differ from the page record's URL because
    // navigate-on-success swaps the session's document; the static
    // FetchPage cached for ls/cat/find stays as-is (those still read
    // the pre-submit content). After submit, the agent should call
    // `navigate` or re-`open` to refresh the action graph for the new
    // landing page if they want to keep using the read methods. We
    // update `current_actions` here best-effort so any immediate
    // follow-up write-verb call still finds the form's submit button
    // (or its successor) by `@e7` if the page hasn't changed too much.
    // The proper refresh path is `navigate` (which re-fetches the
    // action graph via FetchEngine::open).
    Ok(serde_json::json!({
        "ok": true,
        "op": "submit",
        "url": post_url.clone(),
        "ref": want,
        "selector": selector,
        "value": outcome.value,
        "console": outcome.console,
        "postUrl": post_url,
    }))
}

#[derive(Deserialize)]
struct EvalParams {
    js: String,
    #[serde(default)]
    page_id: Option<String>,
}

async fn dispatch_eval(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: EvalParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let page_id = resolve_page_id(&state, p.page_id).await?;
    let mut pages = state.pages.lock().await;
    let record = pages
        .get_mut(&page_id)
        .ok_or_else(|| format!("no page_id `{}`", page_id))?;
    ensure_session(&state.engine, record)?;
    let session = record.session.as_ref().expect("session ensured above");
    let outcome = session
        .eval(&p.js)
        .map_err(|e| format!("js eval failed: {e}"))?;
    Ok(serde_json::json!({
        "ok": true,
        "url": session.url().to_string(),
        "value": outcome.value,
        "console": outcome.console,
    }))
}

#[derive(Deserialize)]
struct NavigateParams {
    url: String,
    #[serde(default)]
    page_id: Option<String>,
}

async fn dispatch_navigate(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: NavigateParams =
        serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let new_url = Url::parse(&p.url).map_err(|e| format!("invalid URL: {e}"))?;

    // Two cases:
    //
    // 1) `page_id` resolves (explicit or default) → navigate the
    //    existing session to `new_url`, replacing its document but
    //    preserving the engine (RNG / virtual clock / cookies via the
    //    shared reqwest::Client).
    //
    // 2) No active page (no `open` ever called, or every page was
    //    `close`d) → this becomes a fresh `open`: fetch, build a new
    //    PageRecord, allocate a `page_id`. This matches the "navigate
    //    is also a session-creator" hint in the task spec ("opens a
    //    new URL in the SAME session" — when no session exists, "the
    //    same session" is the one we create here).
    //
    // The HTTP fetch happens BEFORE we hold the pages lock so other
    // RPC calls don't block on the network. After the fetch returns
    // we re-acquire the lock briefly to install the new state.
    let fetched = state
        .engine
        .open(&new_url)
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;

    let page_id_opt = match p.page_id {
        Some(id) => Some(id),
        None => state.last_page_id.lock().await.clone(),
    };

    if let Some(page_id) = page_id_opt {
        let mut pages = state.pages.lock().await;
        if let Some(record) = pages.get_mut(&page_id) {
            // Active record — navigate the live session if it exists;
            // otherwise just update the cached FetchPage so the read
            // methods reflect the new page.
            let script_outcome = if let Some(session) = record.session.as_mut() {
                Some(
                    session
                        .navigate(&fetched.body_html, fetched.url().clone())
                        .map_err(|e| format!("js session navigate failed: {e}"))?,
                )
            } else {
                None
            };
            record.current_actions = fetched.actions.clone();
            record.page = fetched;
            let scripts_json = match script_outcome {
                Some(o) => serde_json::json!({
                    "executed": o.executed,
                    "executed_with_error": o.executed_with_error,
                    "external_handled": o.external_handled,
                    "skipped_non_script_type": o.skipped_non_script_type,
                }),
                None => serde_json::Value::Null,
            };
            return Ok(serde_json::json!({
                "ok": true,
                "op": "navigate",
                "url": record.page.url().as_str(),
                "page_id": page_id,
                "scripts": scripts_json,
            }));
        }
        // page_id was stale (e.g. previously closed). Fall through to
        // the fresh-open branch below — this matches the "navigate is
        // a session-creator when nothing is active" contract.
    }

    // No active page → mint a fresh page_id and install the record.
    let page_id = state.next_id();
    let url_str = fetched.url().as_str().to_owned();
    state
        .pages
        .lock()
        .await
        .insert(page_id.clone(), PageRecord::new(fetched));
    *state.last_page_id.lock().await = Some(page_id.clone());
    Ok(serde_json::json!({
        "ok": true,
        "op": "navigate",
        "url": url_str,
        "page_id": page_id,
        "scripts": serde_json::Value::Null,
    }))
}

// ============================================================================
// Method handlers — read primitive (PR `read`)
// ============================================================================

#[derive(Deserialize)]
struct ReadParams {
    #[serde(default)]
    page_id: Option<String>,
    /// Comma-separated list of optional fields to emit (matches the
    /// CLI's `--include` flag). When omitted, every field ships. When
    /// supplied, only the listed fields ship — required envelope
    /// fields (url, title, meta, tree, actions, plat_hash) always
    /// emit regardless.
    #[serde(default)]
    include: Option<String>,
    /// Optional prior `content_hash` (the agent's most-recent observed
    /// hash for this URL). When supplied, populate the `delta` field:
    /// matching hash → `since_matched: true` with a real diff; no
    /// matching snapshot → `since_matched: false` with everything in
    /// `actions_added`. When omitted, `delta` is `null`.
    #[serde(default)]
    since: Option<String>,
}

/// `read` — the agent's one-call page report against an open page_id.
///
/// Same envelope shape as `heso read <url>` on the CLI, with the
/// difference that this method ALWAYS sees post-mutation state
/// (since `JsSession` persists across calls). If an earlier `click` /
/// `fill` / `eval` mutated the DOM, `read` reflects those mutations
/// in `text` / `forms` / `console`.
async fn dispatch_read(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: ReadParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let page_id = resolve_page_id(&state, p.page_id).await?;
    let include = parse_include_filter(p.include.as_deref());
    let since = p.since.clone();

    // Phase 1: gather everything off the page record + session under
    // the `pages` lock, then DROP it before touching the snapshots
    // mutex. Same nested-lock-avoidance pattern as `dispatch_navigate`.
    let (mut body, snap, live_url, failed_scripts, console_errors_count) = {
        let mut pages = state.pages.lock().await;
        let record = pages
            .get_mut(&page_id)
            .ok_or_else(|| format!("no page_id `{}`", page_id))?;
        // Ensure a session exists so the post-mutation DOM and console
        // entries are observable. For purely read-only flows where no
        // write verb has touched the page, this builds a fresh session
        // and runs inline scripts once — same cost as a lazy `click`
        // would have paid.
        ensure_session(&state.engine, record)?;

        // Snapshot the static fields off the cached page first, then
        // overlay the live post-hydration extras from the session.
        let static_page = &record.page;
        let session = record.session.as_ref().expect("session ensured above");

        let console = session.engine().drain_console();
        // Best-effort envelope — session-mode `read` peeks the script
        // failures snapshot (without clearing) so a subsequent verb on
        // the same `page_id` still sees the same failure list. This is
        // the same data the CLI verb's `drain_script_failures` returns
        // immediately after `JsSession::open_on_engine`.
        let failed_scripts = session.engine().script_failures_snapshot();
        let console_errors_count = console
            .iter()
            .filter(|e| matches!(e.level, heso_engine_js::ConsoleLevel::Error))
            .count();
        let post_html = session.document_html();
        let live_url = session.url().clone();

        // Always compute visible_text + forms — they feed `content_hash`
        // and the snapshot store even when `include` would drop them
        // from the body. Identical contract to the one-shot CLI path.
        let visible_text = heso_engine_fetch::extract_visible_text(&post_html);
        let forms_json = group_forms(&record.current_actions);

        let mut body = serde_json::json!({
            "url": live_url.as_str(),
            "title": static_page.tree.title,
            "description": static_page.tree.description,
            "metadata": static_page.metadata,
            "tree": static_page.tree,
            "actions": record.current_actions,
        });
        if !static_page.inline_data.is_empty() {
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "inline_data".to_owned(),
                    serde_json::to_value(&static_page.inline_data)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
        }
        if !static_page.data_attrs.is_empty() {
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "data_attrs".to_owned(),
                    serde_json::to_value(&static_page.data_attrs)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
        }
        if include.text {
            body["text"] = serde_json::Value::String(visible_text.clone());
        }
        if include.forms {
            body["forms"] = forms_json.clone();
        }
        if include.cookies {
            body["cookies"] = collect_cookies(&state.engine, &live_url);
        }
        if include.console {
            body["console"] = serde_json::to_value(&console).unwrap_or(serde_json::Value::Null);
        }
        if include.framework {
            body["framework"] = serde_json::Value::String(detect_framework(static_page));
        }
        if include.scripts {
            // The script-execution tally on a long-lived session is
            // computed at install_document time and not re-tracked across
            // subsequent navigates/evals. Without re-running scripts here
            // we surface a `null` to signal the field is intentionally
            // not available on a read-against-session call — the agent
            // can call `navigate` to refresh.
            body["scripts"] = serde_json::Value::Null;
        }
        let snap = ReadSnapshot::from_parts(
            &static_page.tree.title,
            &visible_text,
            &record.current_actions,
            &forms_json,
        );
        (body, snap, live_url, failed_scripts, console_errors_count)
    };

    // Phase 2: snapshot lookup + delta. Mutex held only for this
    // short critical section.
    let delta = {
        let mut store = state.snapshots.lock().await;
        let prior = since
            .as_deref()
            .and_then(|s| find_snapshot(&mut store, &live_url, s));
        let delta_value = match (since.as_deref(), prior.as_ref()) {
            (None, _) => serde_json::Value::Null,
            (Some(_), None) => delta_no_prior(&snap),
            (Some(_), Some(prev)) => compute_delta(&snap, prev),
        };
        // Install the freshly-computed snapshot under the live URL.
        // Replaces any prior entry for the URL; evicts the tail past
        // the LRU cap.
        install_snapshot(&mut store, live_url.clone(), snap.clone());
        delta_value
    };

    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "content_hash".to_owned(),
            serde_json::Value::String(snap.content_hash.clone()),
        );
        obj.insert("delta".to_owned(), delta);
    }

    // Structured-failure envelope — always present per the schema
    // bump. See [`classify_failure_envelope`] for the
    // partial_reason vocabulary.
    let (partial, partial_reason) =
        classify_failure_envelope(&failed_scripts, console_errors_count);
    attach_failure_envelope(
        &mut body,
        partial,
        partial_reason,
        &failed_scripts,
        console_errors_count,
    );

    let hash = heso_engine_fetch::plat_hash(&body);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("plat_hash".to_owned(), serde_json::Value::String(hash));
        obj.insert("page_id".to_owned(), serde_json::Value::String(page_id));
    }
    Ok(body)
}

// ============================================================================
// Method handlers — wait primitive (PR `wait`)
// ============================================================================

#[derive(Deserialize)]
struct WaitParams {
    #[serde(default)]
    page_id: Option<String>,
    /// Exactly one of these condition fields must be set. Matches the
    /// CLI's flag set: `--selector-exists` / `--text-contains` /
    /// `--url-matches` / `--network-idle` / `--time`.
    #[serde(default)]
    selector_exists: Option<String>,
    #[serde(default)]
    text_contains: Option<String>,
    #[serde(default)]
    url_matches: Option<String>,
    #[serde(default)]
    network_idle: Option<bool>,
    /// Network-idle quiet window in ms. Defaults to 500 (Playwright
    /// parity). Ignored unless `network_idle` is `true`.
    #[serde(default)]
    idle_window_ms: Option<u64>,
    /// Virtual-clock advance amount in ms. Setting this picks the
    /// `--time DUR` condition; otherwise unused.
    #[serde(default)]
    time_ms: Option<u64>,
    /// Overall wall-clock timeout in ms. Defaults to 30000.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

async fn dispatch_wait(
    state: Arc<ServerState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let p: WaitParams = serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))?;
    let page_id = resolve_page_id(&state, p.page_id).await?;

    // Construct the WaitCondition. Mirrors `cmd_wait` validation —
    // exactly one of the condition fields must be set.
    let count = [
        p.selector_exists.is_some(),
        p.text_contains.is_some(),
        p.url_matches.is_some(),
        p.network_idle.unwrap_or(false),
        p.time_ms.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if count != 1 {
        return Err(format!(
            "wait: exactly one of selector_exists / text_contains / url_matches / network_idle / time_ms is required (got {count})"
        ));
    }

    let condition = if let Some(css) = p.selector_exists {
        WaitCondition::SelectorExists(css)
    } else if let Some(needle) = p.text_contains {
        WaitCondition::TextContains(needle)
    } else if let Some(pat) = p.url_matches {
        match regex::Regex::new(&pat) {
            Ok(re) => WaitCondition::UrlMatches(re),
            Err(e) => return Err(format!("url_matches: invalid regex: {e}")),
        }
    } else if p.network_idle.unwrap_or(false) {
        WaitCondition::NetworkIdle {
            idle_window_ms: p
                .idle_window_ms
                .unwrap_or(heso_engine_js::wait_for::DEFAULT_NETWORK_IDLE_WINDOW_MS),
        }
    } else {
        // time_ms
        WaitCondition::TimeElapsed {
            duration_ms: p.time_ms.unwrap_or(0),
        }
    };

    let timeout = std::time::Duration::from_millis(
        p.timeout_ms
            .unwrap_or(heso_engine_js::wait_for::DEFAULT_TIMEOUT_MS),
    );

    let mut pages = state.pages.lock().await;
    let record = pages
        .get_mut(&page_id)
        .ok_or_else(|| format!("no page_id `{}`", page_id))?;
    ensure_session(&state.engine, record)?;
    let session = record.session.as_ref().expect("session ensured above");

    let outcome = heso_engine_js::wait_for_on_engine(
        session.engine(),
        &condition,
        timeout,
        heso_engine_js::wait_for::DEFAULT_TICK_MS,
    )
    .map_err(|e| format!("wait failed: {e}"))?;

    // Drain post-pump structured failures + console errors so the
    // envelope shape matches `read` / `open`.
    let console_after = session.engine().drain_console();
    let failed_scripts = session.engine().drain_script_failures();
    let console_errors_count = console_after
        .iter()
        .filter(|e| matches!(e.level, heso_engine_js::ConsoleLevel::Error))
        .count();

    let mut body = outcome.to_json();
    let (mut partial, mut partial_reason) =
        classify_failure_envelope(&failed_scripts, console_errors_count);
    if !outcome.ok {
        // Timeout dominates over a stale script-crash signal — same
        // policy as the CLI verb.
        partial = true;
        partial_reason = "wait_timeout";
    }
    attach_failure_envelope(
        &mut body,
        partial,
        partial_reason,
        &failed_scripts,
        console_errors_count,
    );
    Ok(body)
}

// ============================================================================
// Helpers — session lifecycle, ref normalization, field dict parsing
// ============================================================================

/// Lazily construct the [`JsSession`] inside `record` if it hasn't
/// been built yet. The session reuses the server's shared
/// `reqwest::Client` AND its shared cookie jar (so `Set-Cookie`
/// responses landed by the original page fetch / by any `navigate`
/// call are visible to subsequent `fetch` / `navigate` calls AND to
/// JS `document.cookie` reads) plus the current tokio runtime handle
/// (so JS-issued `fetch()` calls work).
///
/// Sharing the cookie jar is what makes login flows work end-to-end:
/// `open https://app.com/login` → `submit ref=@e3` (server sends
/// `Set-Cookie: session=abc`) → `navigate /dashboard` carries the
/// `session=abc` Cookie header → server returns the auth-gated page.
///
/// Idempotent — calling on a record that already has a session is a
/// no-op. Errors propagate from
/// `JsEngine::new_with_fetch_and_cookies` or the initial
/// inline-script pump.
fn ensure_session(engine: &FetchEngine, record: &mut PageRecord) -> Result<(), String> {
    if record.session.is_some() {
        return Ok(());
    }
    let client = engine.client();
    let cookie_jar = engine.cookie_jar();
    let rt_handle = tokio::runtime::Handle::current();
    let js_engine = JsEngine::new_with_fetch_and_cookies(client, rt_handle, cookie_jar)
        .map_err(|e| format!("failed to create JS engine: {e}"))?;
    // Use the same script policy as `JsSession::open` so inline
    // `<script>` tags run on first attach. Subsequent write verbs see
    // the post-script DOM, matching what an agent sees on a real page.
    let (session, _outcome) = JsSession::open_on_engine(
        js_engine,
        &record.page.body_html,
        record.page.url().clone(),
        ScriptFetchPolicy::default(),
    )
    .map_err(|e| format!("js session open failed: {e}"))?;
    record.session = Some(session);
    Ok(())
}

/// Resolve the effective `page_id` for a write verb. Returns the
/// explicit value when supplied, else the most-recent `open`/`navigate`
/// id, else an error pointing the caller at `open`.
async fn resolve_page_id(
    state: &ServerState,
    explicit: Option<String>,
) -> Result<String, String> {
    if let Some(id) = explicit {
        return Ok(id);
    }
    state
        .last_page_id
        .lock()
        .await
        .clone()
        .ok_or_else(|| "no active page — call `open` first or pass a `page_id`".to_owned())
}

/// Accept both `@e7` and `e7` for the ref argument — matches the
/// ergonomics of the CLI verbs (`heso click @e7` / `heso click e7`).
fn normalize_ref(s: &str) -> String {
    if let Some(stripped) = s.strip_prefix('@') {
        format!("@{stripped}")
    } else {
        format!("@{s}")
    }
}

/// Parse a JSON dict-of-string-coercibles into `(name, value)` pairs,
/// preserving insertion order. Mirrors the `cmd_submit` `--data` /
/// `--field` parser so the RPC and CLI surfaces accept the same shapes.
/// `label` is `"field"` or `"data"`; surfaces in error messages.
fn parse_field_dict(
    v: Option<&serde_json::Value>,
    label: &str,
) -> Result<Vec<(String, String)>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    if v.is_null() {
        return Ok(Vec::new());
    }
    let map = v.as_object().ok_or_else(|| {
        format!("`{label}`: expected a JSON object at the top level, got {v}")
    })?;
    let mut out = Vec::with_capacity(map.len());
    for (k, val) in map {
        let s = match val {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Null => String::new(),
            other => {
                return Err(format!(
                    "`{label}`: value for `{k}` must be a string/number/bool/null, got {other}",
                ));
            }
        };
        out.push((k.clone(), s));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_ref_adds_leading_at_when_missing() {
        assert_eq!(normalize_ref("e7"), "@e7");
    }

    #[test]
    fn normalize_ref_preserves_existing_at() {
        assert_eq!(normalize_ref("@e3"), "@e3");
    }

    #[test]
    fn parse_field_dict_none_yields_empty() {
        let out = parse_field_dict(None, "field").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn parse_field_dict_null_yields_empty() {
        let v = serde_json::Value::Null;
        let out = parse_field_dict(Some(&v), "field").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn parse_field_dict_coerces_scalars_to_strings() {
        let v = serde_json::json!({
            "name": "alice",
            "age": 32,
            "subscribed": true,
            "empty": null,
        });
        let out = parse_field_dict(Some(&v), "field").unwrap();
        // `serde_json::Value::Object` is BTreeMap-backed unless the
        // `preserve_order` feature is enabled, so the iteration order
        // is alphabetical by key. The CLI submit path already accepts
        // that (it uses `merge_submit_fields` to deterministically
        // re-order field-wins-over-data), so we just assert the
        // multiset and the per-key string coercion.
        let mut sorted = out.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![
                ("age".to_owned(), "32".to_owned()),
                ("empty".to_owned(), "".to_owned()),
                ("name".to_owned(), "alice".to_owned()),
                ("subscribed".to_owned(), "true".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_field_dict_rejects_non_object_root() {
        let v = serde_json::json!(["a", "b"]);
        let err = parse_field_dict(Some(&v), "field").unwrap_err();
        assert!(err.contains("expected a JSON object"), "got: {err}");
    }

    #[test]
    fn parse_field_dict_rejects_nested_object_value() {
        let v = serde_json::json!({"name": {"nested": "x"}});
        let err = parse_field_dict(Some(&v), "data").unwrap_err();
        assert!(err.contains("must be a string/number/bool/null"), "got: {err}");
    }
}
