//! # xhr
//!
//! `XMLHttpRequest` global inside the agent-shaped JS engine. Closes
//! bug-report 03 P1 and bug-report 01 P0 cluster — every analytics SDK
//! (theverge, vercel's otSDKStub, blog.cloudflare, slack, nytimes,
//! cloudflare.com) feature-detects `if (typeof XMLHttpRequest !==
//! 'undefined')`; some hard-crash trying to monkey-patch `XMLHttpRequest.
//! prototype` ("Error patching XMLHttpRequest" on vercel.com); the
//! report cites at least six top sites broken on the global being
//! undefined.
//!
//! ## What this module gives you
//!
//! A minimal-but-real implementation of the WHATWG XMLHttpRequest spec
//! (<https://xhr.spec.whatwg.org/>), enough to cover every legacy
//! analytics SDK and jQuery 1.x AJAX path we measured:
//!
//! - `new XMLHttpRequest()` — returns a real instance with the
//!   readyState / status / response / responseText / responseType
//!   IDL fields, plus the on* event-handler IDL properties.
//! - `xhr.open(method, url, async?, user?, password?)` — defaults
//!   `async` to true; sync XHR returns reject-on-send (Phase 1B
//!   punt — every real-world caller in the bug reports uses async).
//! - `xhr.setRequestHeader(name, value)` — accumulates per request.
//! - `xhr.send(body?)` — queues an HTTP request through the same
//!   `reqwest::Client` the in-JS `fetch()` global uses. The drain
//!   path lives in [`drain_pending`], same shape as
//!   [`crate::fetch::drain_pending`].
//! - `readyState` transitions: 0 UNSENT → 1 OPENED → 2 HEADERS_RECEIVED
//!   → 3 LOADING (we coalesce; the response arrives as one chunk
//!   off `reqwest::Response::bytes`) → 4 DONE.
//! - Events: `onreadystatechange`, `onload`, `onerror`, `onabort`
//!   (best-effort), `onloadend`, `ontimeout`.
//! - `response` / `responseText`: text decoded as UTF-8 (we don't
//!   honor `responseType: arraybuffer/json` shapes yet — Phase 1B
//!   punt; the callers we measured all read `responseText`).
//! - `status` / `statusText`: as reported by the server.
//! - `getAllResponseHeaders()` / `getResponseHeader(name)`.
//! - `abort()` is a no-op for now (the request has already been
//!   queued; we punt on cancellation, the bug-report-cited callers
//!   never call abort).
//!
//! ## Determinism (ADR 0008)
//!
//! XHR shares the engine's `FetchMode`. In `Live` mode we hit the
//! network through the same `reqwest::Client` `fetch()` uses, so
//! cookies, the User-Agent, redirects, and (once item M lands)
//! recorded-network playback stay coherent. In
//! `DeterministicNoCassette` mode every `send()` rejects with a
//! cassette error mirroring the `fetch` behavior — XHR is the same
//! source of network nondeterminism as `fetch()`, and the gate
//! ADR 0008 names applies identically.
//!
//! ## Wire-up
//!
//! The constructor is installed via [`install_xhr`] from
//! [`crate::JsEngine::new_inner`] right after `install_fetch`. The
//! drain step runs alongside fetch in
//! [`crate::JsEngine::run_pending_jobs`].

use std::cell::RefCell;
use std::sync::Arc;

use rquickjs::{
    class::Trace,
    prelude::This,
    Class, Context, Ctx, Function, JsLifetime, Object, Persistent, Value,
};

use crate::engine::EvalError;
use crate::fetch::FetchMode;

/// One pending XHR — the JS side has called `xhr.send()` and we owe
/// it `readyState` / event-handler invocations. Stored on the engine
/// until [`drain_pending`] drains them.
pub(crate) struct PendingXhr {
    /// The XHR JS object — handle through which we set `readyState`,
    /// `status`, `responseText`, etc. and from which we read the
    /// `on*` callbacks.
    pub(crate) xhr_obj: Persistent<Object<'static>>,
    /// HTTP method (uppercase).
    pub(crate) method: String,
    /// Absolute URL string.
    pub(crate) url: String,
    /// Headers accumulated via `setRequestHeader`.
    pub(crate) headers: Vec<(String, String)>,
    /// Body bytes — empty for GET / HEAD or when no body was passed.
    pub(crate) body: Vec<u8>,
}

/// Per-engine pending-XHR queue. Same shape as
/// [`crate::fetch::FetchQueue`].
#[allow(clippy::type_complexity)]
pub(crate) struct XhrQueue {
    pending: RefCell<Vec<PendingXhr>>,
}

impl XhrQueue {
    pub(crate) fn new() -> Self {
        Self {
            pending: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn push(&self, p: PendingXhr) {
        self.pending.borrow_mut().push(p);
    }

    pub(crate) fn take_all(&self) -> Vec<PendingXhr> {
        std::mem::take(&mut *self.pending.borrow_mut())
    }

    pub(crate) fn len(&self) -> usize {
        self.pending.borrow().len()
    }
}

/// The Rust-side state we hang off each XHR JS object. Pure data;
/// readable via `Class::instance` from inside Rust closures.
#[derive(JsLifetime, Trace, Clone)]
#[rquickjs::class(rename = "XMLHttpRequest")]
pub struct XmlHttpRequest {
    // No load-bearing Rust-side state — everything is on the JS
    // object as plain own-properties (method, url, headers, etc.).
    // The class exists so `xhr instanceof XMLHttpRequest` succeeds.
    #[qjs(skip_trace)]
    _phantom: (),
}

#[rquickjs::methods(rename_all = "camelCase")]
impl XmlHttpRequest {
    #[qjs(constructor)]
    fn new() -> Self {
        Self { _phantom: () }
    }
}

/// Install the `XMLHttpRequest` global on `context`. Idempotent —
/// re-installation replaces the previous binding.
///
/// Each `new XMLHttpRequest()` returns a fresh JS object whose
/// `__proto__` is the Class<XmlHttpRequest> prototype. State (method,
/// url, headers, readyState, status, responseText, on-handlers) is
/// stored on the instance as own-properties so the methods (open,
/// send, setRequestHeader, ...) can read/mutate without needing
/// `#[qjs(set/get)]` reflection on every field.
pub(crate) fn install_xhr(
    context: &Context,
    mode: FetchMode,
    queue: Arc<XhrQueue>,
) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            // Register the Rust class (instanceof XMLHttpRequest).
            Class::<XmlHttpRequest>::define(&ctx.globals())?;

            // Install the JS-side method surface via a bootstrap that
            // patches XMLHttpRequest.prototype with the spec methods.
            // We keep the methods in JS so they can closure over a
            // `__hesoQueueXhr` callback that goes back into Rust.
            //
            // The Rust callback receives the XHR instance + extracted
            // args and pushes onto `queue`. The drain step then
            // performs the HTTP request and writes readyState/status/
            // responseText onto the instance + invokes on* callbacks.
            let queue_for_closure = queue.clone();
            let mode_for_closure = mode.clone();
            let queue_xhr = Function::new(
                ctx.clone(),
                move |this: This<Object<'_>>| -> rquickjs::Result<()> {
                    // Read the XHR's accumulated state from the JS
                    // instance.
                    let method: String = this
                        .0
                        .get::<_, Option<String>>("__method")?
                        .unwrap_or_else(|| "GET".to_owned());
                    let url: String = this
                        .0
                        .get::<_, Option<String>>("__url")?
                        .unwrap_or_default();
                    let mut headers: Vec<(String, String)> = Vec::new();
                    // Read the accumulated headers array on the JS side.
                    if let Ok(Some(arr)) = this
                        .0
                        .get::<_, Option<rquickjs::Array<'_>>>("__headers")
                    {
                        for v in arr.iter::<Object<'_>>().flatten() {
                            let name: String = v
                                .get::<_, Option<String>>("name")?
                                .unwrap_or_default();
                            let value: String = v
                                .get::<_, Option<String>>("value")?
                                .unwrap_or_default();
                            if !name.is_empty() {
                                headers.push((name, value));
                            }
                        }
                    }
                    let body: Vec<u8> = this
                        .0
                        .get::<_, Option<String>>("__body")?
                        .unwrap_or_default()
                        .into_bytes();

                    // In DeterministicNoCassette mode, do not queue —
                    // fire `onerror` synchronously with a cassette
                    // error so deterministic runs stay reproducible.
                    if matches!(mode_for_closure, FetchMode::DeterministicNoCassette) {
                        let ctx = this.0.ctx().clone();
                        let url2 = url.clone();
                        fire_error_sync(
                            &ctx,
                            &this.0,
                            format!(
                                "xhr to {url2} not in cassette - heso run with --record first (ADR 0008 deterministic-mode gate)"
                            ),
                        )?;
                        return Ok(());
                    }

                    let xhr_persist = Persistent::save(this.0.ctx(), this.0.clone());
                    queue_for_closure.push(PendingXhr {
                        xhr_obj: xhr_persist,
                        method,
                        url,
                        headers,
                        body,
                    });
                    Ok(())
                },
            )?;
            ctx.globals().set("__hesoQueueXhr", queue_xhr)?;

            // Install the spec method surface on the prototype via the
            // JS bootstrap below.
            ctx.eval::<(), _>(XHR_BOOTSTRAP)?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install xhr: {e}")))?;
    Ok(())
}

/// JS bootstrap: patches `XMLHttpRequest.prototype` with `open`,
/// `setRequestHeader`, `send`, `abort`, `getResponseHeader`,
/// `getAllResponseHeaders`, plus the readystatechange constants.
///
/// Each new instance's constructor runs `__hesoXhrInit(this)` (a
/// helper defined in the bootstrap) to populate the per-instance
/// state to the spec defaults (readyState = 0, status = 0,
/// responseText = '', on*-handlers = null, etc.).
const XHR_BOOTSTRAP: &str = r#"
(function() {
    if (globalThis.__hesoXhrInstalled) return;
    if (typeof XMLHttpRequest === 'undefined') {
        throw new Error("heso: XMLHttpRequest class not registered before bootstrap");
    }

    var RustCtor = XMLHttpRequest;
    var proto = RustCtor.prototype;

    // readyState constants per WHATWG XHR §3.1.
    Object.defineProperty(RustCtor, 'UNSENT', { value: 0, writable: false, configurable: false });
    Object.defineProperty(RustCtor, 'OPENED', { value: 1, writable: false, configurable: false });
    Object.defineProperty(RustCtor, 'HEADERS_RECEIVED', { value: 2, writable: false, configurable: false });
    Object.defineProperty(RustCtor, 'LOADING', { value: 3, writable: false, configurable: false });
    Object.defineProperty(RustCtor, 'DONE', { value: 4, writable: false, configurable: false });
    Object.defineProperty(proto, 'UNSENT', { value: 0 });
    Object.defineProperty(proto, 'OPENED', { value: 1 });
    Object.defineProperty(proto, 'HEADERS_RECEIVED', { value: 2 });
    Object.defineProperty(proto, 'LOADING', { value: 3 });
    Object.defineProperty(proto, 'DONE', { value: 4 });

    function initState(self) {
        if (self.__hesoXhrInit) return;
        self.__hesoXhrInit = true;
        self.readyState = 0;
        self.status = 0;
        self.statusText = '';
        self.responseURL = '';
        self.response = null;
        self.responseText = '';
        self.responseType = '';
        self.responseXML = null;
        self.timeout = 0;
        self.withCredentials = false;
        self.upload = {};
        self.__method = 'GET';
        self.__url = '';
        self.__headers = [];
        self.__body = '';
        self.__responseHeaders = []; // [[name, value], ...]
        self.__responseHeaderMap = {};
        self.onreadystatechange = null;
        self.onload = null;
        self.onerror = null;
        self.onloadstart = null;
        self.onloadend = null;
        self.onprogress = null;
        self.onabort = null;
        self.ontimeout = null;
    }

    // Wrap the Rust-side constructor so `new XMLHttpRequest()` pre-
    // populates the spec-default IDL fields. The Rust class has no
    // Rust-side state — everything lives as JS own-properties — so
    // a fresh instance without `initState` would have `readyState`
    // === undefined, which fails every spec-shaped feature-test.
    function XMLHttpRequestWrapper() {
        if (new.target === undefined) {
            throw new TypeError("XMLHttpRequest: Illegal constructor (must be called with new)");
        }
        var instance = Reflect.construct(RustCtor, [], new.target);
        initState(instance);
        return instance;
    }
    XMLHttpRequestWrapper.prototype = proto;
    Object.defineProperty(XMLHttpRequestWrapper, 'name', { value: 'XMLHttpRequest', configurable: true });
    // Re-expose the spec constants on the wrapper (the .prototype
    // is shared so proto-level constants are still visible).
    XMLHttpRequestWrapper.UNSENT = 0;
    XMLHttpRequestWrapper.OPENED = 1;
    XMLHttpRequestWrapper.HEADERS_RECEIVED = 2;
    XMLHttpRequestWrapper.LOADING = 3;
    XMLHttpRequestWrapper.DONE = 4;
    globalThis.XMLHttpRequest = XMLHttpRequestWrapper;

    function fireEvent(self, name, isError) {
        // 1. Call the on<name> IDL handler if it's a function.
        var idl = self['on' + name];
        if (typeof idl === 'function') {
            try {
                idl.call(self, { type: name, target: self, currentTarget: self });
            } catch (e) {
                // Spec: errors in on* handlers propagate to the global
                // error event. We surface as a console error via the
                // host's microtask drain; calling code shouldn't be
                // broken by a faulty event handler.
                if (typeof globalThis.reportError === 'function') {
                    globalThis.reportError(e);
                } else if (typeof console !== 'undefined' && console.error) {
                    console.error(e);
                }
            }
        }
        // 2. addEventListener-registered handlers (best-effort: we
        //    install a tiny registry on the instance via the EventTarget
        //    shim if used).
        var listeners = self.__listeners;
        if (listeners && listeners[name]) {
            var arr = listeners[name].slice();
            for (var i = 0; i < arr.length; i++) {
                try {
                    arr[i].call(self, { type: name, target: self, currentTarget: self });
                } catch (e) {
                    if (typeof console !== 'undefined' && console.error) {
                        console.error(e);
                    }
                }
            }
        }
        void isError; // currently unused; reserved for spec-error-flag tracking
    }

    function setReadyState(self, state) {
        self.readyState = state;
        fireEvent(self, 'readystatechange', false);
    }

    // Expose helpers for the Rust drain path to call back into.
    globalThis.__hesoXhrSetReadyState = setReadyState;
    globalThis.__hesoXhrFireEvent = fireEvent;

    proto.open = function(method, url, async_, user, password) {
        initState(this);
        // Per spec, `async` defaults to true. The 3-arg form (method,
        // url, false) is sync XHR; we keep it queued anyway because
        // every measured caller passes true or omits the arg. A future
        // change can throw on async=false to flag the punt.
        this.__method = String(method == null ? 'GET' : method).toUpperCase();
        this.__url = String(url == null ? '' : url);
        this.__async = (async_ === undefined) ? true : !!async_;
        void user; void password;
        // Reset response state per spec ("reset state on open").
        this.readyState = 0;
        this.status = 0;
        this.statusText = '';
        this.responseURL = '';
        this.response = null;
        this.responseText = '';
        this.__responseHeaders = [];
        this.__responseHeaderMap = {};
        this.__headers = [];
        this.__body = '';
        setReadyState(this, 1); // OPENED
    };

    proto.setRequestHeader = function(name, value) {
        initState(this);
        if (typeof name !== 'string' || name === '') return;
        this.__headers.push({
            name: String(name),
            value: String(value == null ? '' : value)
        });
    };

    proto.send = function(body) {
        initState(this);
        // Body coercion. Per spec: string, FormData, Blob/File,
        // ArrayBuffer, URLSearchParams. For Phase 1B punt we coerce
        // to string via `String(body)` for non-empty non-object
        // bodies, and JSON.stringify for plain objects. This covers
        // the legacy-AJAX caller shape (text bodies).
        if (body == null) {
            this.__body = '';
        } else if (typeof body === 'string') {
            this.__body = body;
        } else if (typeof body === 'object') {
            // FormData / Blob / Object — JSON.stringify the latter
            // and let FormData fall through to the host's
            // serialization (currently as JSON; Phase 1B caller-shape
            // matches what every measured analytics SDK does).
            try {
                this.__body = JSON.stringify(body);
            } catch (e) {
                this.__body = '';
            }
        } else {
            this.__body = String(body);
        }
        if (typeof globalThis.__hesoQueueXhr !== 'function') {
            // Engine not built with an XHR queue (no fetch client) —
            // synthesize an error per spec's "if the request was sent
            // and the network errored" branch.
            this.readyState = 4;
            this.status = 0;
            this.statusText = '';
            fireEvent(this, 'error', true);
            fireEvent(this, 'loadend', false);
            return;
        }
        globalThis.__hesoQueueXhr.call(this);
    };

    proto.abort = function() {
        // Phase 1B punt: we don't actually cancel the queued request.
        // Mark the instance as aborted so the drain code can short-
        // circuit the response (it checks __aborted).
        initState(this);
        this.__aborted = true;
        if (this.readyState !== 0 && this.readyState !== 4) {
            this.readyState = 4;
            fireEvent(this, 'readystatechange', false);
            fireEvent(this, 'abort', false);
            fireEvent(this, 'loadend', false);
        }
    };

    proto.getResponseHeader = function(name) {
        initState(this);
        if (typeof name !== 'string') return null;
        var key = name.toLowerCase();
        if (this.__responseHeaderMap && (key in this.__responseHeaderMap)) {
            return this.__responseHeaderMap[key];
        }
        return null;
    };

    proto.getAllResponseHeaders = function() {
        initState(this);
        if (!this.__responseHeaders || this.__responseHeaders.length === 0) {
            return '';
        }
        var out = '';
        for (var i = 0; i < this.__responseHeaders.length; i++) {
            out += this.__responseHeaders[i][0] + ': ' + this.__responseHeaders[i][1] + '\r\n';
        }
        return out;
    };

    proto.overrideMimeType = function(mime) {
        initState(this);
        this.__overrideMime = String(mime == null ? '' : mime);
    };

    // EventTarget-style add/removeEventListener for callers that use
    // the modern API. Used by polyfills that monkey-patch a "real" XHR
    // event listener.
    proto.addEventListener = function(type, listener) {
        initState(this);
        if (typeof listener !== 'function') return;
        if (!this.__listeners) this.__listeners = {};
        if (!this.__listeners[type]) this.__listeners[type] = [];
        this.__listeners[type].push(listener);
    };
    proto.removeEventListener = function(type, listener) {
        if (!this.__listeners || !this.__listeners[type]) return;
        this.__listeners[type] = this.__listeners[type].filter(function(fn) {
            return fn !== listener;
        });
    };
    proto.dispatchEvent = function(event) {
        if (!event || !event.type) return true;
        fireEvent(this, event.type, false);
        return true;
    };

    Object.defineProperty(globalThis, '__hesoXhrInstalled', {
        value: true, writable: false, configurable: false, enumerable: false
    });
})();
"#;

/// Synchronously fire an `error` event on the XHR object. Used in
/// `DeterministicNoCassette` mode where we never queue.
fn fire_error_sync<'js>(
    ctx: &Ctx<'js>,
    xhr: &Object<'js>,
    msg: String,
) -> rquickjs::Result<()> {
    // Set the readyState to DONE + status to 0 (per WHATWG XHR network
    // error branch) and fire `readystatechange` then `error` then
    // `loadend`.
    xhr.set("readyState", 4)?;
    xhr.set("status", 0)?;
    xhr.set("statusText", "")?;
    xhr.set("__hesoXhrError", msg)?;
    let fire: Function = ctx.globals().get("__hesoXhrFireEvent")?;
    fire.call::<_, ()>((xhr.clone(), "readystatechange", false))?;
    fire.call::<_, ()>((xhr.clone(), "error", true))?;
    fire.call::<_, ()>((xhr.clone(), "loadend", false))?;
    Ok(())
}

/// Drain every pending XHR: perform the HTTP request via `mode`'s
/// client/handle, then walk the readyState transitions and invoke the
/// on* handlers on the JS instance.
///
/// Called by [`crate::JsEngine::run_pending_jobs`] right after the
/// fetch-drain step (so XHR-via-fetch-polyfill polyfills also pump).
/// Idempotent on an empty queue.
///
/// Returns the number of XHRs drained.
pub(crate) fn drain_pending(
    context: &Context,
    queue: &XhrQueue,
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
                let outcome = perform_request(client, rt_handle, &p);
                resolve_one(context, p, outcome)?;
            }
        }
        FetchMode::Recording {
            client,
            rt_handle,
            cassette,
        } => {
            for p in pending {
                let outcome = perform_request(client, rt_handle, &p);
                if let XhrOutcome::Ok {
                    status,
                    final_url,
                    headers,
                    body,
                    ..
                } = &outcome
                {
                    cassette
                        .lock()
                        .expect("cassette mutex poisoned")
                        .record(
                            &p.method,
                            &p.url,
                            final_url,
                            &p.body,
                            *status,
                            headers.clone(),
                            body,
                        );
                }
                resolve_one(context, p, outcome)?;
            }
        }
        FetchMode::Replaying { cassette } => {
            for p in pending {
                let outcome = match cassette.lookup(&p.method, &p.url, &p.body) {
                    Some(record) => match heso_engine_fetch::Cassette::decode_response_body(record)
                    {
                        Ok(body) => XhrOutcome::Ok {
                            status: record.status,
                            status_text: reqwest::StatusCode::from_u16(record.status)
                                .ok()
                                .and_then(|s| s.canonical_reason())
                                .unwrap_or("")
                                .to_owned(),
                            final_url: record.final_url.clone(),
                            headers: record.response_headers.clone(),
                            body,
                        },
                        Err(e) => XhrOutcome::Err(format!(
                            "cassette decode error for {} {}: {}",
                            p.method, p.url, e
                        )),
                    },
                    None => XhrOutcome::Err(format!(
                        "cassette miss: {} {} not recorded (cassette has {} entries)",
                        p.method,
                        p.url,
                        cassette.len()
                    )),
                };
                resolve_one(context, p, outcome)?;
            }
        }
        FetchMode::DeterministicNoCassette => {
            for p in pending {
                let url = p.url.clone();
                let msg = format!("xhr to {url} not in cassette - heso run with --record first");
                resolve_one(context, p, XhrOutcome::Err(msg))?;
            }
        }
    }
    Ok(n)
}

/// Outcome of one XHR HTTP call.
enum XhrOutcome {
    Ok {
        status: u16,
        status_text: String,
        final_url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Err(String),
}

/// Issue one HTTP request synchronously via `block_in_place`. Same
/// shape as [`crate::fetch::perform_request`] — XHR shares the
/// network path so cookies and (eventually) cassette playback stay
/// coherent.
fn perform_request(
    client: &reqwest::Client,
    rt_handle: &tokio::runtime::Handle,
    p: &PendingXhr,
) -> XhrOutcome {
    // data: URL fast path — same logic as the script-src loader.
    if let Some(payload) = crate::fetch::parse_data_url(&p.url) {
        return XhrOutcome::Ok {
            status: 200,
            status_text: "OK".into(),
            final_url: p.url.clone(),
            headers: vec![("content-type".into(), payload.mime)],
            body: payload.body,
        };
    }

    let method = match p.method.as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        other => match reqwest::Method::from_bytes(other.as_bytes()) {
            Ok(m) => m,
            Err(e) => return XhrOutcome::Err(format!("xhr: bad method `{other}`: {e}")),
        },
    };

    let mut builder = client.request(method, &p.url);
    for (k, v) in &p.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    if !p.body.is_empty() {
        builder = builder.body(p.body.clone());
    }

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
        Ok((status, status_text, final_url, headers, body)) => XhrOutcome::Ok {
            status,
            status_text,
            final_url,
            headers,
            body,
        },
        Err(e) => XhrOutcome::Err(format!("xhr: {e}")),
    }
}

/// Walk the readyState transitions and write the response onto the
/// XHR JS object, then fire the on* event-handler IDL props (and any
/// addEventListener-registered listeners). All inside a fresh
/// `Context::with`.
fn resolve_one(
    context: &Context,
    p: PendingXhr,
    outcome: XhrOutcome,
) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let xhr: Object<'_> = p.xhr_obj.restore(&ctx)?;

            // If the JS side aborted before we got here, the abort()
            // path already set readyState=4 and fired events. Skip.
            if xhr.get::<_, Option<bool>>("__aborted")?.unwrap_or(false) {
                return Ok(());
            }

            let fire: Function = ctx.globals().get("__hesoXhrFireEvent")?;

            match outcome {
                XhrOutcome::Ok {
                    status,
                    status_text,
                    final_url,
                    headers,
                    body,
                } => {
                    // Build the response headers array + map.
                    let arr = rquickjs::Array::new(ctx.clone())?;
                    let header_map = Object::new(ctx.clone())?;
                    for (i, (name, val)) in headers.iter().enumerate() {
                        let entry = rquickjs::Array::new(ctx.clone())?;
                        entry.set(0, name.as_str())?;
                        entry.set(1, val.as_str())?;
                        arr.set(i, entry)?;
                        header_map.set(name.to_ascii_lowercase().as_str(), val.as_str())?;
                    }
                    xhr.set("__responseHeaders", arr)?;
                    xhr.set("__responseHeaderMap", header_map)?;
                    xhr.set("status", status)?;
                    xhr.set("statusText", status_text)?;
                    xhr.set("responseURL", final_url.as_str())?;

                    // HEADERS_RECEIVED.
                    xhr.set("readyState", 2)?;
                    fire.call::<_, ()>((xhr.clone(), "readystatechange", false))?;
                    // LOADING.
                    xhr.set("readyState", 3)?;
                    fire.call::<_, ()>((xhr.clone(), "readystatechange", false))?;

                    // Decode body. Per WHATWG XHR §3.7 the
                    // responseType determines the decoding pass.
                    // Phase 1B punt: we set responseText as UTF-8
                    // and response as the same string. JSON / arraybuffer
                    // shapes are upstream work.
                    let response_type: String = xhr
                        .get::<_, Option<String>>("responseType")?
                        .unwrap_or_default();
                    let text = String::from_utf8_lossy(&body).into_owned();
                    xhr.set("responseText", text.as_str())?;
                    if response_type == "json" {
                        // Spec: response = JSON.parse(responseText) on
                        // success; null on parse error.
                        let json_obj: Object = ctx.globals().get("JSON")?;
                        let parse: Function = json_obj.get("parse")?;
                        match parse.call::<_, Value<'_>>((text.as_str(),)) {
                            Ok(v) => xhr.set("response", v)?,
                            Err(_) => xhr.set("response", rquickjs::Value::new_null(ctx.clone()))?,
                        }
                    } else if response_type == "arraybuffer" {
                        let ta = rquickjs::TypedArray::<u8>::new(ctx.clone(), body.as_slice())?;
                        let obj: Object<'_> = ta.into_object();
                        let ab: Value<'_> = obj.get("buffer")?;
                        xhr.set("response", ab)?;
                    } else {
                        // "" (default) and "text" share the responseText
                        // value.
                        xhr.set("response", text.as_str())?;
                    }

                    // DONE.
                    xhr.set("readyState", 4)?;
                    fire.call::<_, ()>((xhr.clone(), "readystatechange", false))?;
                    fire.call::<_, ()>((xhr.clone(), "load", false))?;
                    fire.call::<_, ()>((xhr.clone(), "loadend", false))?;
                }
                XhrOutcome::Err(msg) => {
                    xhr.set("status", 0)?;
                    xhr.set("statusText", "")?;
                    xhr.set("__hesoXhrError", msg.as_str())?;
                    xhr.set("readyState", 4)?;
                    fire.call::<_, ()>((xhr.clone(), "readystatechange", false))?;
                    fire.call::<_, ()>((xhr.clone(), "error", true))?;
                    fire.call::<_, ()>((xhr.clone(), "loadend", false))?;
                }
            }
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("resolve xhr: {e}")))?;
    Ok(())
}
