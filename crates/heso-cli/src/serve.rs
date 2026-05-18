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
//! {"jsonrpc":"2.0","method":"ready","params":{"version":"...","methods":["open","ls","cat","find","close","ping"]}}
//! ```
//!
//! ## Methods
//!
//! | Method  | Params | Result |
//! |---------|--------|--------|
//! | `open`  | `{url, explore_links_depth?: u8, link_cap?: usize}` | `{page_id, url, title, description, metadata, tree, actions, linked_pages?}` |
//! | `ls`    | `{page_id, path?}` | `{path, entries: [LsRow, ...]}` |
//! | `cat`   | `{page_id, target}` | `{path, content}` for a tree path, or `ElementRef` for `@ref` |
//! | `find`  | `{page_id, role?, name_substr?, section?}` | `{count, matches: [ElementRef, ...]}` |
//! | `close` | `{page_id}` | `{closed: bool}` |
//! | `ping`  | none | `"pong"` |
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
use heso_engine_api::Page;
use heso_engine_fetch::{
    linked_pages_to_json, ExploreOptions, FetchEngine, FetchPage, DEFAULT_LINK_CAP, HARD_LINK_CAP,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

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

// ============================================================================
// Server state
// ============================================================================

struct ServerState {
    engine: FetchEngine,
    /// `page_id` → cached `FetchPage`. Lock is held briefly per request;
    /// `FetchPage: Clone` so handlers don't pin the lock across awaits.
    pages: Mutex<HashMap<String, FetchPage>>,
    /// Monotonic page id counter. `AtomicU64` avoids a second lock on
    /// every `open`.
    counter: AtomicU64,
}

impl ServerState {
    fn new() -> heso_core::Result<Self> {
        Ok(Self {
            engine: FetchEngine::new()?,
            pages: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
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
            "methods": ["open", "ls", "cat", "find", "close", "ping"],
        }
    });
    if write_line(&mut stdout, &hello).await.is_err() {
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
        let value = match serde_json::to_value(&response) {
            Ok(v) => v,
            Err(_) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": serde_json::Value::Null,
                "error": { "code": INTERNAL_ERROR, "message": "failed to serialize response" }
            }),
        };
        if write_line(&mut stdout, &value).await.is_err() {
            // stdout closed by the parent — we're done.
            break;
        }
    }
    ExitCode::SUCCESS
}

async fn write_line(out: &mut tokio::io::Stdout, v: &serde_json::Value) -> std::io::Result<()> {
    let mut s = serde_json::to_string(v)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    s.push('\n');
    out.write_all(s.as_bytes()).await?;
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
// Method handlers
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
    // BEFORE the page_id (which is server-instance-scoped, not part of
    // the plat's portable identity). The plat module strips `plat_hash`
    // and `page_id` would also bias the hash, so we hash a clone with
    // `page_id` removed, then put it back.
    let mut hash_input = payload.clone();
    if let Some(obj) = hash_input.as_object_mut() {
        obj.remove("page_id");
    }
    let hash = heso_engine_fetch::plat_hash(&hash_input);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("plat_hash".to_owned(), serde_json::Value::String(hash));
    }
    state.pages.lock().await.insert(page_id, page);
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
    let page = pages
        .get(&p.page_id)
        .ok_or_else(|| format!("no page_id `{}`", p.page_id))?;
    let rows = page.tree.ls(&p.path).map_err(|e| e.to_string())?;
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
    let page = pages
        .get(&p.page_id)
        .ok_or_else(|| format!("no page_id `{}`", p.page_id))?;
    if let Some(stripped) = p.target.strip_prefix('@') {
        let want = format!("@{stripped}");
        match heso_engine_fetch::resolve_action(&page.actions, &want) {
            Some(el) => serde_json::to_value(el).map_err(|e| e.to_string()),
            None => Err(format!("no element at ref `{want}`")),
        }
    } else {
        let content = page.tree.cat(&p.target).map_err(|e| e.to_string())?;
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
    let page = pages
        .get(&p.page_id)
        .ok_or_else(|| format!("no page_id `{}`", p.page_id))?;
    let matches = heso_engine_fetch::filter_actions(
        &page.actions,
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
    Ok(serde_json::json!({ "closed": removed.is_some() }))
}
