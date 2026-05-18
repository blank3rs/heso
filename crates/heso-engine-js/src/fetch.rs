//! # fetch
//!
//! `fetch(url, options?)` inside the agent-shaped JS engine — the
//! item C unlock per [`next-phase-plan.md`][plan]. JS code can now do:
//!
//! ```js
//! fetch("https://example.com/api").then(r => r.text())
//! ```
//!
//! …and the request goes through heso's shared [`reqwest::Client`]
//! (the same one [`heso_engine_fetch::FetchEngine`] uses for static
//! page loads) so cookies, the `User-Agent`, redirects, and (once
//! item M lands) recorded-network playback stay coherent with the
//! rest of the receipt.
//!
//! ## What this module is and is not
//!
//! - **It is** the in-JS network surface: a single global `fetch`
//!   function returning a Promise that resolves to a Response-shaped
//!   plain object (`{ ok, status, statusText, url, headers, text(),
//!   json(), arrayBuffer() }`). Enough for the call shape every modern
//!   JS app uses; not the full WHATWG `Response` class.
//! - **It is not** a port of `awslabs/llrt`'s `llrt_fetch`. That
//!   module is hyper-based, sits on top of six other `llrt_*` crates
//!   (`llrt_http`, `llrt_utils`, `llrt_abort`, `llrt_context`,
//!   `llrt_compression`, `llrt_encoding`), and is not on crates.io.
//!   Vendoring it would mean pulling in a parallel HTTP stack and
//!   wiring our [`reqwest::Client`] back over the top — strictly more
//!   code than writing the thin adapter here, and gives up control
//!   over the cookie/jar/receipt path the plan explicitly calls out.
//!   When `llrt_fetch` lands on crates.io with a `with_client` knob
//!   we'll revisit.
//!
//! ## Determinism (ADR 0008)
//!
//! Two modes:
//!
//! - [`FetchMode::Live`] — performs the network request via the held
//!   `reqwest::Client`, using the [`tokio::runtime::Handle`] the host
//!   provides to drive the future. This is the path `heso eval-dom
//!   --js-fetch` and `heso open --js` take.
//! - [`FetchMode::DeterministicNoCassette`] — `--seed N` mode without
//!   a recording cassette. Every fetch call rejects synchronously
//!   with a clear error explaining the user must run `heso run
//!   --record` first. This is the gate ADR 0008 mandates *before*
//!   the full record/replay layer (item M) lands; it ensures a seeded
//!   run never produces an observable that depends on whatever the
//!   network happened to do on this machine.
//!
//! ## Promise resolution
//!
//! `fetch` returns a Promise immediately. The request runs on a
//! `tokio::task::spawn_blocking` (wrapping `Handle::block_on`) — *not*
//! a `Handle::spawn`, because we need to drop back into the QuickJS
//! `Context` from the same OS thread that owns the runtime. Concretely:
//!
//! 1. JS calls `fetch(url, opts)`.
//! 2. Rust extracts (url, method, body, headers) into owned `String`s
//!    plus a [`rquickjs::Persistent`] handle to the resolve/reject
//!    pair.
//! 3. The current host (the [`JsEngine::run_pending_jobs`] call after
//!    the synchronous JS finishes) drains the **pending fetch queue**:
//!    for each entry, run the request via `Handle::block_on` and
//!    resolve/reject the promise inside a fresh `Context::with`.
//! 4. Any `.then(...)` callbacks queued by the JS side run as
//!    microtasks; QuickJS pumps them automatically when the resolve
//!    function is invoked, and [`JsEngine::run_pending_jobs`] drives
//!    `Runtime::execute_pending_job` until idle.
//!
//! This pattern is a deliberate punt on "top-level `await fetch(...)`
//! works" — that requires an `AsyncRuntime`/`AsyncContext` switch
//! (item K, microtask pump). For PR2 the `.then(...)` shape is enough
//! and matches what real pages emit in production.
//!
//! [plan]: ../../.agent/next-phase-plan.md

use std::cell::RefCell;
use std::sync::Arc;

use rquickjs::{
    prelude::{Rest, This},
    Context, Ctx, Function, Object, Persistent, Promise, Value,
};

use crate::engine::EvalError;

/// How the in-JS `fetch` global should behave.
#[derive(Debug, Clone)]
pub enum FetchMode {
    /// Live network access. The held [`Arc`] is cloned into the
    /// closure that backs `fetch`, so the JS engine and any other
    /// caller (the static [`heso_engine_fetch::FetchEngine`]) share
    /// the same connection pool, cookie jar, and TLS state.
    Live {
        /// Shared HTTP client. Same instance the rest of the workspace
        /// uses, threaded through from
        /// [`heso_engine_fetch::FetchEngine::client`].
        client: Arc<reqwest::Client>,
        /// Tokio runtime handle to drive async HTTP work. The QuickJS
        /// runtime is single-threaded and `!Send`, so we cannot move
        /// it onto a worker; instead we use `Handle::block_on` from
        /// the same OS thread when draining the pending-fetch queue.
        rt_handle: tokio::runtime::Handle,
    },
    /// Deterministic mode (`--seed N`) without a network-recording
    /// cassette. Every `fetch(url, ...)` call rejects with a clear
    /// error pointing the user at `heso run --record` (per ADR 0008).
    /// This is the explicit gate before item M lands the full
    /// record/replay layer.
    DeterministicNoCassette,
}

/// A pending fetch — the JS side has called `fetch(...)` and we owe
/// it a Promise resolution. Stored on the engine until
/// [`JsEngine::run_pending_jobs`] drains them.
pub(crate) struct PendingFetch {
    pub request: PendingRequest,
    pub resolve: Persistent<Function<'static>>,
    pub reject: Persistent<Function<'static>>,
}

/// The user-supplied half of a fetch — extracted from JS into owned
/// Rust values so the request can be issued without holding any
/// rquickjs borrows.
#[derive(Debug, Clone)]
pub(crate) struct PendingRequest {
    pub url: String,
    pub method: String,
    pub body: Option<Vec<u8>>,
    pub headers: Vec<(String, String)>,
}

/// Per-engine pending-fetch queue. Pushed-into from the JS-side
/// `fetch` global; drained by [`drain_pending`] after the synchronous
/// JS finishes.
#[allow(clippy::type_complexity)]
pub(crate) struct FetchQueue {
    pending: RefCell<Vec<PendingFetch>>,
}

impl FetchQueue {
    pub(crate) fn new() -> Self {
        Self {
            pending: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn push(&self, p: PendingFetch) {
        self.pending.borrow_mut().push(p);
    }

    pub(crate) fn take_all(&self) -> Vec<PendingFetch> {
        std::mem::take(&mut *self.pending.borrow_mut())
    }

    pub(crate) fn len(&self) -> usize {
        self.pending.borrow().len()
    }
}

/// Install the `fetch` global on `context`.
///
/// Idempotent — calling twice replaces the previous binding.
///
/// In [`FetchMode::Live`], the closure captures both the shared
/// `Arc<reqwest::Client>` and the `tokio::runtime::Handle` so the
/// fetch closure can do the actual work later from
/// [`drain_pending`].
///
/// In [`FetchMode::DeterministicNoCassette`], the closure rejects
/// every invocation synchronously without queueing — there is no
/// later work to drain, so the engine's "no network in deterministic
/// mode" guarantee holds even if the host forgets to call
/// [`drain_pending`].
pub(crate) fn install_fetch(
    context: &Context,
    mode: FetchMode,
    queue: Arc<FetchQueue>,
) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let fetch_fn = match mode {
                FetchMode::Live { .. } => make_fetch_live(&ctx, queue.clone())?,
                FetchMode::DeterministicNoCassette => make_fetch_deterministic(&ctx)?,
            };
            ctx.globals().set("fetch", fetch_fn)?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install fetch: {e}")))?;
    Ok(())
}

/// In `Live` mode: build the JS function that queues a fetch and
/// returns a Promise.
fn make_fetch_live<'js>(ctx: &Ctx<'js>, queue: Arc<FetchQueue>) -> rquickjs::Result<Function<'js>> {
    // Promote the outer 'js lifetime onto the closure so the closure's
    // input ctx and its return `Promise<'js>` share one lifetime
    // parameter; otherwise the `'_` shortcut on both makes them
    // independent and rquickjs's HRTB rejects the closure. Using the
    // method-form `Function::new(ctx, ...)` confines the closure's
    // lifetime to one `ctx.with` call where 'js is the only lifetime.
    Function::new(
        ctx.clone(),
        move |args: Rest<Value<'_>>| -> rquickjs::Result<Persistent<Promise<'static>>> {
            // We can't return Promise<'js> from a closure that doesn't
            // bind 'js (rquickjs's HRTB inference rejects that). Trick:
            // return `Persistent<Promise>` — Persistent is 'static —
            // and let rquickjs's `IntoJs` automatically restore it on
            // the way out. Same pattern as the rng module's helpers
            // when they need to round-trip a Value through a closure
            // boundary.
            let args_inner = args.into_inner();
            let ctx = match args_inner.first() {
                Some(v) => v.ctx().clone(),
                None => {
                    return Err(rquickjs::Error::new_from_js("undefined", "url"));
                }
            };
            let req = match extract_request(&ctx, &args_inner) {
                Ok(r) => r,
                Err(msg) => {
                    let (promise, _resolve, reject) = Promise::new(&ctx)?;
                    let err = ctx.eval::<Value, _>(format!(
                        "new TypeError({})",
                        serde_json::to_string(&msg)
                            .unwrap_or_else(|_| "\"fetch: bad arguments\"".into())
                    ))?;
                    reject.call::<_, ()>((err,))?;
                    return Ok(Persistent::save(&ctx, promise));
                }
            };
            let (promise, resolve, reject) = Promise::new(&ctx)?;
            let resolve_p: Persistent<Function<'static>> = Persistent::save(&ctx, resolve);
            let reject_p: Persistent<Function<'static>> = Persistent::save(&ctx, reject);
            queue.push(PendingFetch {
                request: req,
                resolve: resolve_p,
                reject: reject_p,
            });
            Ok(Persistent::save(&ctx, promise))
        },
    )
}

/// In `DeterministicNoCassette` mode: every fetch rejects immediately
/// with a clear error explaining the gate.
fn make_fetch_deterministic<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Function<'js>> {
    Function::new(
        ctx.clone(),
        move |args: Rest<Value<'_>>| -> rquickjs::Result<Persistent<Promise<'static>>> {
            let args_inner = args.into_inner();
            let ctx = match args_inner.first() {
                Some(v) => v.ctx().clone(),
                None => {
                    return Err(rquickjs::Error::new_from_js("undefined", "url"));
                }
            };
            let url = args_inner
                .first()
                .and_then(|v| v.as_string())
                .and_then(|s| s.to_string().ok())
                .unwrap_or_else(|| "<unknown>".to_owned());
            let (promise, _resolve, reject) = Promise::new(&ctx)?;
            let msg = format!(
                "fetch to {url} not in cassette - heso run with --record first (ADR 0008 deterministic-mode gate; full record/replay is item M)"
            );
            let err = ctx.eval::<Value, _>(format!(
                "new Error({})",
                serde_json::to_string(&msg).unwrap_or_else(|_| "\"fetch: not in cassette\"".into())
            ))?;
            reject.call::<_, ()>((err,))?;
            Ok(Persistent::save(&ctx, promise))
        },
    )
}

/// Extract `(url, method, body, headers)` from `fetch`'s argument
/// list. Mirrors the WHATWG fetch shape:
///
/// - `fetch(url)` → GET to `url`, no body.
/// - `fetch(url, { method, headers, body })` → same with overrides.
/// - `body` may be a string (sent as utf-8) or a JS object (the
///   options bag itself, in which case it's serialized as JSON with
///   `Content-Type: application/json` if not already set).
///
/// Returns a human-readable error string on bad shape; the caller
/// wraps it in a `TypeError` rejection.
fn extract_request<'js>(ctx: &Ctx<'js>, args: &[Value<'js>]) -> Result<PendingRequest, String> {
    let url = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or_else(|| "fetch: first argument must be a string URL".to_owned())?
        .to_string()
        .map_err(|e| format!("fetch: read url: {e}"))?;

    let mut method = "GET".to_owned();
    let mut body: Option<Vec<u8>> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut body_is_json = false;

    if let Some(opts) = args.get(1).and_then(|v| v.as_object()) {
        if let Some(m) = opts.get::<_, Option<String>>("method").ok().flatten() {
            method = m.to_ascii_uppercase();
        }
        if let Ok(Some(b_val)) = opts.get::<_, Option<Value<'_>>>("body") {
            if let Some(s) = b_val.as_string() {
                body = Some(
                    s.to_string()
                        .map_err(|e| format!("fetch: read body string: {e}"))?
                        .into_bytes(),
                );
            } else if b_val.is_object() && !b_val.is_function() {
                // JSON-shaped body — serialize via JSON.stringify.
                let stringify: Function = ctx
                    .globals()
                    .get::<_, Object>("JSON")
                    .and_then(|j| j.get("stringify"))
                    .map_err(|e| format!("fetch: get JSON.stringify: {e}"))?;
                let s: String = stringify
                    .call((b_val,))
                    .map_err(|e| format!("fetch: JSON.stringify body: {e}"))?;
                body = Some(s.into_bytes());
                body_is_json = true;
            } else if !b_val.is_undefined() && !b_val.is_null() {
                return Err("fetch: body must be a string or JSON-shaped object".into());
            }
        }
        if let Ok(Some(hdr_val)) = opts.get::<_, Option<Value<'_>>>("headers") {
            if let Some(hdr_obj) = hdr_val.as_object() {
                for k_val in hdr_obj.keys::<String>().flatten() {
                    if let Ok(v) = hdr_obj.get::<_, String>(&k_val) {
                        headers.push((k_val, v));
                    }
                }
            }
        }
    }
    // Auto Content-Type for JSON-bodies if caller didn't set one.
    if body_is_json
        && !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        headers.push(("Content-Type".into(), "application/json".into()));
    }
    Ok(PendingRequest {
        url,
        method,
        body,
        headers,
    })
}

/// Drain every pending fetch on `queue`: perform the HTTP request via
/// `mode`'s client/handle, then resolve or reject each Promise from
/// inside a fresh `Context::with` so the JS engine sees the result.
///
/// Called by [`crate::engine::JsEngine::run_pending_jobs`] after the
/// synchronous JS finishes. Idempotent on an empty queue.
///
/// Returns the number of fetches drained.
pub(crate) fn drain_pending(
    context: &Context,
    queue: &FetchQueue,
    mode: &FetchMode,
) -> Result<usize, EvalError> {
    let pending = queue.take_all();
    let n = pending.len();
    if n == 0 {
        return Ok(0);
    }

    match mode {
        FetchMode::Live { client, rt_handle } => {
            for p in pending {
                let outcome = perform_request(client, rt_handle, &p.request);
                resolve_one(context, p, outcome)?;
            }
        }
        FetchMode::DeterministicNoCassette => {
            // Should never queue in deterministic mode (the
            // closure rejects synchronously), but be defensive.
            for p in pending {
                let url = p.request.url.clone();
                resolve_one(
                    context,
                    p,
                    FetchOutcome::Err(format!(
                        "fetch to {url} not in cassette - heso run with --record first"
                    )),
                )?;
            }
        }
    }
    Ok(n)
}

/// Outcome of one HTTP call, ready to feed into JS.
enum FetchOutcome {
    Ok {
        status: u16,
        status_text: String,
        final_url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// Rejected — TypeError with this message.
    Err(String),
}

/// Synchronously issue one request via the shared `reqwest::Client`.
/// `data:` URLs are handled inline (reqwest doesn't support them) so
/// tests can use them as a zero-network-call shortcut.
fn perform_request(
    client: &reqwest::Client,
    rt_handle: &tokio::runtime::Handle,
    req: &PendingRequest,
) -> FetchOutcome {
    if let Some(payload) = parse_data_url(&req.url) {
        return FetchOutcome::Ok {
            status: 200,
            status_text: "OK".into(),
            final_url: req.url.clone(),
            headers: vec![("content-type".into(), payload.mime)],
            body: payload.body,
        };
    }
    let method = match req.method.as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        other => match reqwest::Method::from_bytes(other.as_bytes()) {
            Ok(m) => m,
            Err(e) => return FetchOutcome::Err(format!("fetch: bad method `{other}`: {e}")),
        },
    };

    let mut builder = client.request(method, &req.url);
    for (k, v) in &req.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    if let Some(body) = &req.body {
        builder = builder.body(body.clone());
    }

    // Use `block_in_place` so calling code inside an existing tokio
    // runtime (the CLI's `#[tokio::main]` flow) doesn't trip the
    // "cannot start a runtime from within a runtime" guard. On a
    // multi-thread runtime this hands the current task off to another
    // worker; on a current-thread runtime it would panic — but the
    // engine is single-threaded and the host always wires
    // `flavor = "multi_thread"` for that reason.
    let result = tokio::task::block_in_place(|| {
        rt_handle.block_on(async move {
            let resp = builder.send().await?;
            let status = resp.status();
            let status_text = status.canonical_reason().unwrap_or("").to_owned();
            let final_url = resp.url().as_str().to_owned();
            let mut headers: Vec<(String, String)> = Vec::new();
            for (name, val) in resp.headers().iter() {
                if let Ok(s) = val.to_str() {
                    headers.push((name.as_str().to_owned(), s.to_owned()));
                }
            }
            let body = resp.bytes().await?.to_vec();
            Ok::<_, reqwest::Error>((status.as_u16(), status_text, final_url, headers, body))
        })
    });

    match result {
        Ok((status, status_text, final_url, headers, body)) => FetchOutcome::Ok {
            status,
            status_text,
            final_url,
            headers,
            body,
        },
        Err(e) => FetchOutcome::Err(format!("fetch: {e}")),
    }
}

/// Resolve one pending fetch's Promise inside a fresh `Context::with`.
fn resolve_one(context: &Context, p: PendingFetch, outcome: FetchOutcome) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            match outcome {
                FetchOutcome::Ok {
                    status,
                    status_text,
                    final_url,
                    headers,
                    body,
                } => {
                    let response =
                        build_response(&ctx, status, &status_text, &final_url, &headers, body)?;
                    let resolve = p.resolve.restore(&ctx)?;
                    resolve.call::<_, ()>((response,))?;
                }
                FetchOutcome::Err(msg) => {
                    let err = ctx.eval::<Value, _>(format!(
                        "new TypeError({})",
                        serde_json::to_string(&msg).unwrap_or_else(|_| "\"fetch failed\"".into())
                    ))?;
                    let reject = p.reject.restore(&ctx)?;
                    reject.call::<_, ()>((err,))?;
                }
            }
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("resolve fetch promise: {e}")))?;
    Ok(())
}

/// Build the Response-shaped plain object returned to JS.
///
/// Shape:
/// ```text
/// {
///   ok: bool, status: number, statusText: string, url: string,
///   headers: { has(name), get(name), keys(), entries() },
///   text(): Promise<string>,
///   json(): Promise<any>,
///   arrayBuffer(): Promise<ArrayBuffer>,
/// }
/// ```
fn build_response<'js>(
    ctx: &Ctx<'js>,
    status: u16,
    status_text: &str,
    final_url: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> rquickjs::Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("ok", (200..300).contains(&status))?;
    obj.set("status", status)?;
    obj.set("statusText", status_text)?;
    obj.set("url", final_url)?;
    obj.set("redirected", false)?;
    obj.set("type", "basic")?;

    // Build the Headers-shaped sub-object. Closures here use the
    // same Persistent-return trick as `make_fetch_live`: we can't
    // return `Value<'js>` directly from a closure unless we name 'js
    // — but `Persistent` is 'static, so wrapping in Persistent lets
    // rquickjs handle the round-trip.
    let headers_obj = Object::new(ctx.clone())?;
    {
        // Lowercase-keyed map for case-insensitive lookups.
        let map = Object::new(ctx.clone())?;
        for (k, v) in headers {
            map.set(k.to_ascii_lowercase().as_str(), v.as_str())?;
        }
        headers_obj.set("__map", map)?;
    }
    headers_obj.set(
        "get",
        Function::new(
            ctx.clone(),
            move |this: This<Object<'_>>,
                  name: String|
                  -> rquickjs::Result<Persistent<Value<'static>>> {
                let ctx = this.0.ctx().clone();
                let map: Object = this.0.get("__map")?;
                let v = map
                    .get::<_, Value<'_>>(name.to_ascii_lowercase().as_str())
                    .unwrap_or_else(|_| Value::new_null(ctx.clone()));
                Ok(Persistent::save(&ctx, v))
            },
        )?,
    )?;
    headers_obj.set(
        "has",
        Function::new(
            ctx.clone(),
            move |this: This<Object<'_>>, name: String| -> rquickjs::Result<bool> {
                let map: Object = this.0.get("__map")?;
                map.contains_key(name.to_ascii_lowercase().as_str())
            },
        )?,
    )?;
    obj.set("headers", headers_obj)?;

    // Body accessors. Each returns a Promise that resolves to the
    // decoded body — Promise-typed so the WHATWG `.then()` shape works.
    let body_shared = Arc::new(body);

    let body_for_text = body_shared.clone();
    let text_fn = Function::new(
        ctx.clone(),
        move |this: This<Object<'_>>| -> rquickjs::Result<Persistent<Promise<'static>>> {
            let ctx = this.0.ctx().clone();
            let (promise, resolve, _reject) = Promise::new(&ctx)?;
            let s = String::from_utf8_lossy(&body_for_text).into_owned();
            resolve.call::<_, ()>((s,))?;
            Ok(Persistent::save(&ctx, promise))
        },
    )?;
    obj.set("text", text_fn)?;

    let body_for_json = body_shared.clone();
    let json_fn = Function::new(
        ctx.clone(),
        move |this: This<Object<'_>>| -> rquickjs::Result<Persistent<Promise<'static>>> {
            let ctx = this.0.ctx().clone();
            let (promise, resolve, reject) = Promise::new(&ctx)?;
            let s = String::from_utf8_lossy(&body_for_json).into_owned();
            // Use JS-native JSON.parse so the parsed value plugs
            // directly into the JS heap (Object/Array/Number/...).
            let json_obj: Object = ctx.globals().get("JSON")?;
            let parse: Function = json_obj.get("parse")?;
            match parse.call::<_, Value<'_>>((s,)) {
                Ok(v) => {
                    resolve.call::<_, ()>((v,))?;
                }
                Err(e) => {
                    let msg = format!("Response.json: parse error: {e}");
                    let err = ctx.eval::<Value, _>(format!(
                        "new SyntaxError({})",
                        serde_json::to_string(&msg)
                            .unwrap_or_else(|_| "\"json parse error\"".into())
                    ))?;
                    reject.call::<_, ()>((err,))?;
                }
            }
            Ok(Persistent::save(&ctx, promise))
        },
    )?;
    obj.set("json", json_fn)?;

    let body_for_ab = body_shared.clone();
    let ab_fn = Function::new(
        ctx.clone(),
        move |this: This<Object<'_>>| -> rquickjs::Result<Persistent<Promise<'static>>> {
            let ctx = this.0.ctx().clone();
            let (promise, resolve, _reject) = Promise::new(&ctx)?;
            // ArrayBuffer construction goes via TypedArray in rquickjs.
            let ta = rquickjs::TypedArray::<u8>::new(ctx.clone(), body_for_ab.as_slice())?;
            // .buffer is the underlying ArrayBuffer.
            let obj: Object = ta.into_object();
            let ab: Value<'_> = obj.get("buffer")?;
            resolve.call::<_, ()>((ab,))?;
            Ok(Persistent::save(&ctx, promise))
        },
    )?;
    obj.set("arrayBuffer", ab_fn)?;

    Ok(obj)
}

/// Decoded `data:` URL.
struct DataPayload {
    mime: String,
    body: Vec<u8>,
}

/// Parse a `data:[<mime>][;base64],<payload>` URL into its mime type
/// and body bytes. Returns `None` if `url` isn't a data URL. Keeps
/// our test shape (`fetch("data:text/plain,hello")`) working without
/// pulling in another crate.
fn parse_data_url(url: &str) -> Option<DataPayload> {
    let rest = url.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let meta = &rest[..comma];
    let payload = &rest[comma + 1..];
    let is_base64 = meta.ends_with(";base64");
    let mime = if is_base64 {
        meta.trim_end_matches(";base64").to_owned()
    } else {
        meta.to_owned()
    };
    let mime = if mime.is_empty() {
        "text/plain;charset=US-ASCII".to_owned()
    } else {
        mime
    };
    let body = if is_base64 {
        base64_decode(payload)?
    } else {
        urlencoding_decode(payload).unwrap_or_else(|| payload.as_bytes().to_vec())
    };
    Some(DataPayload { mime, body })
}

/// Tiny base64 decoder for `data:;base64,...` URLs — not worth a crate.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &c) in TABLE.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for b in bytes {
        if b == b'=' {
            break;
        }
        let v = lookup[b as usize];
        if v == 255 {
            return None;
        }
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Tiny percent-decoder for `data:,...` URLs.
fn urlencoding_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 2 < bytes.len() {
            let hi = hex(bytes[i + 1])?;
            let lo = hex(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else if b == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    Some(out)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_data_url_plain_text() {
        let p = parse_data_url("data:text/plain,hello").expect("parse");
        assert_eq!(p.mime, "text/plain");
        assert_eq!(p.body, b"hello");
    }

    #[test]
    fn parse_data_url_percent_decoded() {
        let p = parse_data_url("data:text/plain,hello%20world").expect("parse");
        assert_eq!(p.body, b"hello world");
    }

    #[test]
    fn parse_data_url_base64() {
        // base64("hi") == "aGk="
        let p = parse_data_url("data:application/octet-stream;base64,aGk=").expect("parse");
        assert_eq!(p.mime, "application/octet-stream");
        assert_eq!(p.body, b"hi");
    }

    #[test]
    fn parse_data_url_default_mime_for_empty() {
        let p = parse_data_url("data:,abc").expect("parse");
        assert_eq!(p.mime, "text/plain;charset=US-ASCII");
        assert_eq!(p.body, b"abc");
    }

    #[test]
    fn parse_data_url_returns_none_for_non_data() {
        assert!(parse_data_url("https://example.com").is_none());
        assert!(parse_data_url("data:no-comma").is_none());
    }

    #[test]
    fn base64_round_trip_short() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
    }
}
