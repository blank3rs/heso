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
    linked_pages_to_json, resolve_action, ElementRef, ExploreOptions, FetchEngine, FetchPage,
    DEFAULT_LINK_CAP, HARD_LINK_CAP,
};
use heso_engine_js::{JsEngine, JsSession, ScriptFetchPolicy};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::{merge_submit_fields, selector_for_action};

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
}

impl ServerState {
    fn new() -> heso_core::Result<Self> {
        Ok(Self {
            engine: FetchEngine::new()?,
            pages: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            last_page_id: Mutex::new(None),
        })
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("p{n}")
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

#[derive(Deserialize)]
struct FillParams {
    #[serde(rename = "ref")]
    ref_str: String,
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
    let want = normalize_ref(&p.ref_str);
    let elem = resolve_action(&record.current_actions, &want)
        .ok_or_else(|| format!("no element at ref `{want}`"))?
        .clone();
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
    #[serde(rename = "ref")]
    ref_str: String,
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
    let want = normalize_ref(&p.ref_str);
    let elem = resolve_action(&record.current_actions, &want)
        .ok_or_else(|| format!("no element at ref `{want}`"))?
        .clone();
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
    #[serde(rename = "ref")]
    ref_str: String,
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
    let want = normalize_ref(&p.ref_str);
    let elem = resolve_action(&record.current_actions, &want)
        .ok_or_else(|| format!("no element at ref `{want}`"))?
        .clone();
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
// Helpers — session lifecycle, ref normalization, field dict parsing
// ============================================================================

/// Lazily construct the [`JsSession`] inside `record` if it hasn't
/// been built yet. The session reuses the server's shared
/// `reqwest::Client` (cookies, TLS, UA stay coherent with the static
/// fetch path) and the current tokio runtime handle (so JS-issued
/// `fetch()` calls work).
///
/// Idempotent — calling on a record that already has a session is a
/// no-op. Errors propagate from `JsEngine::new_with_fetch` or the
/// initial inline-script pump.
fn ensure_session(engine: &FetchEngine, record: &mut PageRecord) -> Result<(), String> {
    if record.session.is_some() {
        return Ok(());
    }
    let client = engine.client();
    let rt_handle = tokio::runtime::Handle::current();
    let js_engine = JsEngine::new_with_fetch(client, rt_handle)
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
