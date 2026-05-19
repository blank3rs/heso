//! Engine: a thin wrapper around [`rquickjs`] that exposes a safe,
//! agent-shaped JavaScript evaluation surface.
//!
//! Public surface in Phase 1A:
//!
//! - [`JsEngine`] — owns a [`rquickjs::Runtime`] + [`rquickjs::Context`]
//!   pair plus a shared console buffer. Evaluation is single-threaded
//!   and synchronous.
//! - [`JsEngine::eval`] — runs a script string. Returns
//!   [`EvalOutcome`] with the script's return value (as
//!   [`serde_json::Value`]) and any captured `console.*` calls.
//! - [`EvalError`] — typed exceptions: JS exceptions (with stack),
//!   non-Error thrown values, and engine-internal errors.
//!
//! No DOM, no `window`, no `<script>`-tag execution yet — that's
//! Phase 1B. Right now the engine is a sandboxed JS evaluator with
//! captured console output, and that's it.

use std::sync::{Arc, Mutex};

use rquickjs::{
    prelude::{Func, Rest, This},
    CatchResultExt, CaughtError, Class, Context, Ctx, Function, Object, Runtime, Value,
};

use url::Url;

use crate::dom::{self, Document};
use crate::fetch::{self, FetchMode, FetchQueue};
use crate::rng::SeededRng;
use crate::scripts::{self, ScriptFetchPolicy, ScriptOutcome};
use crate::timers::{self, TimerScheduler};

/// Memory cap per [`JsEngine`]. 10 MB is enough for typical
/// page-hydration JS but cheap to bump if a real page needs more.
const DEFAULT_MEMORY_LIMIT_BYTES: usize = 10 * 1024 * 1024;

/// Stack cap per [`JsEngine`]. 256 KB matches the rquickjs docs
/// example and is plenty for normal recursion depths.
const DEFAULT_MAX_STACK_BYTES: usize = 256 * 1024;

/// Severity of a captured `console.*` call.
///
/// Mirrors the standard browser console levels. `Trace` is included
/// because some libraries route low-priority diagnostics there; we
/// keep them so an agent can see them if it asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleLevel {
    /// `console.log` — default information.
    Log,
    /// `console.info` — explicit info-level.
    Info,
    /// `console.warn` — warnings.
    Warn,
    /// `console.error` — errors.
    Error,
    /// `console.debug` — debug-level diagnostics.
    Debug,
    /// `console.trace` — stack-trace-flavored diagnostics.
    Trace,
}

/// A single captured `console.*` call.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConsoleEntry {
    /// Which `console.*` method produced this entry.
    pub level: ConsoleLevel,
    /// Arguments to the call, each converted to a JSON value via
    /// `JSON.stringify` semantics. Non-JSON-representable values
    /// (functions, symbols, undefined) become [`serde_json::Value::Null`].
    pub args: Vec<serde_json::Value>,
}

/// Successful evaluation result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvalOutcome {
    /// The value the script evaluated to, as JSON.
    ///
    /// `undefined`, functions, and symbols become
    /// [`serde_json::Value::Null`]. Objects and arrays go through
    /// `JSON.stringify` so they appear with the same key ordering JS
    /// produces.
    pub value: serde_json::Value,
    /// All `console.*` calls captured during the evaluation, in the
    /// order they were made.
    pub console: Vec<ConsoleEntry>,
}

/// Failure modes for [`JsEngine::eval`].
///
/// The three variants distinguish the typical JS-engine error shapes
/// agents need to handle differently: a normal `throw new Error(...)`,
/// a `throw <non-Error-value>` (any value can be thrown in JS), and
/// engine-internal failures (out-of-memory, stack overflow,
/// allocation failures from the Rust side).
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// The script threw an `Error` (or subclass).
    ///
    /// Stack traces are attached when QuickJS provides them — they
    /// won't have file paths since the script is anonymous, but line
    /// numbers within the eval'd source are useful.
    #[error("javascript exception: {message}")]
    Exception {
        /// `e.message` of the thrown error.
        message: String,
        /// `e.stack` of the thrown error, when available.
        stack: Option<String>,
    },

    /// The script threw a non-Error value (`throw "string"`,
    /// `throw 42`, `throw {custom: true}`).
    ///
    /// JS allows throwing anything; we capture a JSON representation
    /// of whatever was thrown.
    #[error("script threw non-error value: {value}")]
    ThrownValue {
        /// JSON-encoded representation of the thrown value.
        value: serde_json::Value,
    },

    /// Engine-internal error: out-of-memory, stack overflow, etc.
    ///
    /// The message is the underlying [`rquickjs::Error`] display,
    /// which usually identifies the limit that tripped.
    #[error("engine error: {0}")]
    Engine(String),
}

/// A reusable JavaScript engine instance.
///
/// Holds a single QuickJS runtime + context pair and a shared
/// buffer for captured `console.*` calls. The buffer is cleared at
/// the start of every [`JsEngine::eval`] call so each evaluation
/// produces a clean log.
///
/// One engine per logical "page" or session — they are intentionally
/// cheap (no warm-up cost beyond ~1 ms to allocate the runtime), so
/// callers can create and drop freely.
pub struct JsEngine {
    /// Held alive for the lifetime of `context`; QuickJS requires
    /// the runtime to outlive any contexts and values referencing it.
    _runtime: Runtime,
    context: Context,
    console_buffer: Arc<Mutex<Vec<ConsoleEntry>>>,
    /// Per-engine timer scheduler. Owns the virtual clock and the
    /// pending-timer heap; shared with the JS-side `setTimeout` /
    /// `setInterval` closures and the Rust-side `advance_clock` /
    /// `pending_timers` methods. See [`crate::timers`] for the full
    /// design.
    timers: Arc<Mutex<TimerScheduler>>,
    /// Per-engine seeded PRNG backing `Math.random`,
    /// `crypto.getRandomValues`, and `crypto.randomUUID`. Constructed
    /// from the `--seed N` value the host passed to
    /// [`Self::new_with_seed`] (or `0` for [`Self::new`]). See
    /// [`crate::rng`] for the design; ADR 0008 for the determinism
    /// contract.
    rng: SeededRng,
    /// Per-engine pending-fetch queue + fetch mode.
    ///
    /// Populated only when the host called [`Self::new_with_fetch`]
    /// or [`Self::new_with_seed_and_fetch`]; otherwise `None` and the
    /// `fetch` global is not installed in the JS context.
    ///
    /// `RefCell` (not `Mutex`) because [`Self`] is single-threaded by
    /// construction — the QuickJS runtime is `!Send`, so the engine
    /// never crosses a thread boundary.
    fetch_state: Option<FetchState>,
    /// Per-engine "current page URL" — used to resolve relative
    /// `<script src="...">` references during inline-script execution.
    /// `None` for engines created without an associated page (e.g.
    /// bare `heso eval-js`); set by [`Self::set_base_url`] or by
    /// [`crate::JsSession`] at open/navigate time.
    base_url: Mutex<Option<Url>>,
}

/// Bundles a per-engine fetch queue with the mode that drives it.
pub(crate) struct FetchState {
    pub(crate) queue: Arc<FetchQueue>,
    pub(crate) mode: FetchMode,
}

impl JsEngine {
    /// Create a fresh engine with conservative resource limits
    /// ([`DEFAULT_MEMORY_LIMIT_BYTES`], [`DEFAULT_MAX_STACK_BYTES`])
    /// and the default RNG seed (`0`).
    ///
    /// `console.log` / `info` / `warn` / `error` / `debug` / `trace`
    /// are installed as global functions that route into an
    /// in-process buffer instead of stdout, so receipts stay clean.
    ///
    /// For seeded determinism (per ADR 0008) use
    /// [`Self::new_with_seed`].
    pub fn new() -> Result<Self, EvalError> {
        Self::new_with_seed(0)
    }

    /// Create a fresh engine with the given PRNG seed. Same seed +
    /// same script + same `advance_clock` sequence → byte-identical
    /// observable output (per ADR 0008).
    ///
    /// `seed = 0` matches [`Self::new`]'s behavior — a real seed, not
    /// a "no seed" sentinel, so two unseeded sessions are still
    /// reproducible against each other.
    ///
    /// The seed wires up:
    ///
    /// - `Math.random()` — draws uniform `f64` in `[0, 1)` from the
    ///   seeded ChaCha20 stream.
    /// - `crypto.getRandomValues(view)` — fills the passed
    ///   `Uint8Array` from the same stream.
    /// - `crypto.randomUUID()` — emits a v4-format UUID whose 16
    ///   underlying bytes come from the same stream.
    pub fn new_with_seed(seed: u64) -> Result<Self, EvalError> {
        Self::new_inner(seed, None)
    }

    /// Create a fresh engine with the default seed (`0`) and the
    /// `fetch` global wired to the supplied [`reqwest::Client`].
    ///
    /// Use this when constructing an engine for a session that should
    /// be able to issue HTTP requests from JS — typically `heso
    /// eval-dom --js-fetch` or `heso open --js`. Pass the same
    /// [`Arc<reqwest::Client>`] you use for the static path
    /// ([`heso_engine_fetch::FetchEngine::client`]) so cookies, TLS
    /// state, and (when item M lands) recorded-network playback stay
    /// coherent across the two paths.
    ///
    /// `rt_handle` is the host's [`tokio::runtime::Handle`] — the
    /// engine uses it to drive `reqwest::Client::send` from inside
    /// the synchronous JS context. The host MUST call this from a
    /// context where `Handle::try_current()` succeeds (e.g. inside
    /// a `#[tokio::main]` function or a `tokio::task::spawn_blocking`
    /// pool), otherwise constructing the engine still works but
    /// every `fetch()` rejects with an executor error.
    pub fn new_with_fetch(
        client: Arc<reqwest::Client>,
        rt_handle: tokio::runtime::Handle,
    ) -> Result<Self, EvalError> {
        Self::new_inner(0, Some(FetchMode::Live { client, rt_handle }))
    }

    /// Like [`Self::new_with_fetch`] but also seeds the PRNG. When
    /// `seed` is non-zero this is `--seed N` mode WITHOUT a recording
    /// cassette and — per ADR 0008's "determinism gate" — `fetch()`
    /// is installed in the `DeterministicNoCassette` variant that
    /// rejects every call with a clear error pointing the user at
    /// `heso run --record`.
    ///
    /// Use [`Self::new_with_seed_and_live_fetch`] if you have a
    /// recording cassette and want live fetch under a seed.
    pub fn new_with_seed_and_fetch(
        seed: u64,
        _client: Arc<reqwest::Client>,
        _rt_handle: tokio::runtime::Handle,
    ) -> Result<Self, EvalError> {
        // Currently every seeded run lands in `DeterministicNoCassette`
        // because item M (record/replay) hasn't shipped yet. When it
        // does, the cassette decides whether we route to the live
        // client or replay from disk; the public surface stays the
        // same.
        Self::new_inner(seed, Some(FetchMode::DeterministicNoCassette))
    }

    /// Escape hatch: seeded PRNG + live fetch. Used by tests that
    /// pin both at once and by future code that has a cassette and
    /// wants to route through it. Most callers want
    /// [`Self::new_with_seed_and_fetch`] instead.
    pub fn new_with_seed_and_live_fetch(
        seed: u64,
        client: Arc<reqwest::Client>,
        rt_handle: tokio::runtime::Handle,
    ) -> Result<Self, EvalError> {
        Self::new_inner(seed, Some(FetchMode::Live { client, rt_handle }))
    }

    /// Internal constructor — the single place that wires up all
    /// globals so the public `new_*` variants don't drift.
    fn new_inner(seed: u64, fetch_mode: Option<FetchMode>) -> Result<Self, EvalError> {
        let runtime = Runtime::new().map_err(|e| EvalError::Engine(e.to_string()))?;
        runtime.set_memory_limit(DEFAULT_MEMORY_LIMIT_BYTES);
        runtime.set_max_stack_size(DEFAULT_MAX_STACK_BYTES);

        let context = Context::full(&runtime).map_err(|e| EvalError::Engine(e.to_string()))?;
        let console_buffer: Arc<Mutex<Vec<ConsoleEntry>>> = Arc::new(Mutex::new(Vec::new()));

        install_console(&context, console_buffer.clone())?;
        install_dom_classes(&context)?;
        // Install the JS-side `__hesoMakeStyleProxy` factory before
        // any Element wrapper is created — `Element.style` reaches
        // for the global on every access.
        install_style_proxy(&context)?;
        crate::events::install_events(&context)?;

        // rquickjs's `Persistent<Function<'static>>` (held inside
        // [`TimerScheduler`]'s entries) is not `Send + Sync` because
        // QuickJS objects are pinned to their parent runtime. The
        // engine is single-threaded so the `Arc` will never cross
        // threads in practice; we keep `Arc` (rather than `Rc`) for
        // consistency with the existing `console_buffer: Arc<Mutex>`
        // pattern.
        #[allow(clippy::arc_with_non_send_sync)]
        let timers: Arc<Mutex<TimerScheduler>> = Arc::new(Mutex::new(TimerScheduler::new()));
        timers::install_timers(&context, timers.clone())
            .map_err(|e| EvalError::Engine(format!("install timers: {e}")))?;

        // Determinism shims (ADR 0008): override `Math.random` and
        // install a `crypto` global with `getRandomValues` and
        // `randomUUID`. The RNG closures own a [`SeededRng`] clone
        // (cheap — bumps an Arc refcount), so RNG state lives on the
        // JS side via the Function objects, not on Rust-held
        // `Persistent`s. That sidesteps the Runtime-drop ordering trap
        // that `timers.rs` had to design around.
        let rng = SeededRng::new(seed);
        install_rng(&context, rng.clone())?;

        // Determinism shim for the host wall clock: route `Date.now()`
        // and zero-arg `new Date()` through the same `VirtualClock`
        // that backs `setTimeout` / `setInterval`. Explicit-input forms
        // (`new Date(ms)`, `new Date(str)`, `new Date(y,m,d,...)`,
        // `Date.parse`, `Date.UTC`) are pure functions of their inputs
        // and stay on the QuickJS built-in. See [`install_date`].
        install_date(&context, timers.clone())?;

        // Install `globalThis.location` (and a `globalThis.window`
        // self-reference so `window.location` resolves). Starts as
        // `about:blank`; [`Self::set_base_url`] rewrites the fields
        // when the host navigates the engine to a real page.
        install_location(&context, None)?;

        // Install the "trivial browser globals" cluster — small APIs
        // that real pages reach for during init (`navigator`, storage,
        // `performance.now`, `queueMicrotask`, `requestAnimationFrame`,
        // `atob` / `btoa`, `matchMedia`). Each is a one-or-two-line
        // shim individually; collectively they unblock dozens of init
        // paths on real-world pages that would otherwise throw on a
        // missing global. See [`install_browser_apis`].
        install_browser_apis(&context, timers.clone())?;

        // Optional: install the `fetch` global.
        let fetch_state = if let Some(mode) = fetch_mode {
            #[allow(clippy::arc_with_non_send_sync)]
            let queue = Arc::new(FetchQueue::new());
            fetch::install_fetch(&context, mode.clone(), queue.clone())?;
            Some(FetchState { queue, mode })
        } else {
            None
        };

        Ok(Self {
            _runtime: runtime,
            context,
            console_buffer,
            timers,
            rng,
            fetch_state,
            base_url: Mutex::new(None),
        })
    }

    /// Set or clear the page URL used to resolve relative
    /// `<script src="...">` references when [`Self::install_document`]
    /// or [`Self::eval_with_html`] runs the inline-script pump.
    ///
    /// Without this, the script pump treats `src="base.js"` as a
    /// literal URL and `reqwest` rejects it — see the relative-URL
    /// path in [`crate::scripts::fetch_script_source`]. With it set,
    /// the pump resolves via [`Url::join`] before issuing the fetch.
    pub fn set_base_url(&self, url: Option<Url>) {
        *self.base_url.lock().expect("base_url poisoned") = url.clone();
        // Reflect the URL into `globalThis.location` so page JS that
        // reads `window.location.href` / `pathname` / etc. sees the
        // new page. Swallow errors here — install_location is
        // best-effort cosmetic and a failure shouldn't poison
        // navigation.
        let _ = install_location(&self.context, url.as_ref());
    }

    /// Current page URL, if any. See [`Self::set_base_url`].
    pub fn base_url(&self) -> Option<Url> {
        self.base_url.lock().expect("base_url poisoned").clone()
    }

    /// The seed-backed RNG installed into the JS context. Useful for
    /// tests that want to assert host-side determinism — the same
    /// `SeededRng` clone observed in JS is reachable here.
    pub fn rng(&self) -> &SeededRng {
        &self.rng
    }

    /// Advance the deterministic virtual clock by `delta_ms`
    /// milliseconds. Fires every `setTimeout` / `setInterval`
    /// callback whose recorded fire-time is now `<= virtual_now`, in
    /// ascending `(fire_time, insertion_seq)` order.
    ///
    /// Tie-breaking is by insertion order — an earlier `setTimeout`
    /// fires before a later `setTimeout` that resolves at the same
    /// virtual time.
    ///
    /// Per [ADR 0008], a callback that throws is captured into the
    /// engine's console buffer as a [`ConsoleLevel::Error`] entry and
    /// the timer pump continues — halting on a JS throw would make
    /// firing order observably affect the engine's continued
    /// operation, which is a determinism trap.
    ///
    /// The console buffer is **not** cleared by this call (unlike
    /// [`Self::eval`]) — captured throws accumulate alongside any
    /// `console.*` output produced from prior evals or by the
    /// callbacks themselves. Use [`Self::drain_console`] to snapshot
    /// and clear if you want a clean slate.
    pub fn advance_clock(&self, delta_ms: u64) -> Result<(), EvalError> {
        timers::advance_clock(&self.context, &self.timers, &self.console_buffer, delta_ms)
            .map_err(|e| EvalError::Engine(format!("advance_clock: {e}")))?;
        Ok(())
    }

    /// Advance the deterministic virtual clock and return a snapshot
    /// of the **entire** console buffer (including entries left over
    /// from prior evals) after the advance completes.
    ///
    /// Test-and-introspection helper — production callers should use
    /// [`Self::advance_clock`] plus [`Self::drain_console`] or the
    /// per-eval `console` field on [`EvalOutcome`].
    pub fn advance_clock_capture(&self, delta_ms: u64) -> Result<Vec<ConsoleEntry>, EvalError> {
        self.advance_clock(delta_ms)?;
        Ok(self
            .console_buffer
            .lock()
            .expect("console buffer poisoned")
            .clone())
    }

    /// Number of un-fired timers currently scheduled. Counts both
    /// one-shots (`setTimeout`) and intervals (`setInterval`); an
    /// interval counts as `1` regardless of how many times it has
    /// already fired.
    pub fn pending_timers(&self) -> usize {
        self.timers
            .lock()
            .expect("timer scheduler poisoned")
            .pending_count()
    }

    /// Drain every pending `fetch()` call: dispatch the HTTP request
    /// through the engine's shared `reqwest::Client`, resolve (or
    /// reject) the Promise that `fetch()` returned, then loop until
    /// QuickJS reports no more pending microtask jobs.
    ///
    /// A single pass works for the simple `.then(...)` shape because
    /// resolving a Promise immediately enqueues its `.then` callbacks
    /// as microtasks, which QuickJS runs in `Runtime::execute_pending_job`.
    ///
    /// **Limitation:** top-level `await fetch(...)` in `eval` does
    /// not yet work — that requires the [`rquickjs::AsyncRuntime`]
    /// path (item K, microtask pump). For now, callers should use
    /// `.then(...)` chains and observe the result via either
    /// [`Self::drain_console`] or a side-effect they capture in JS.
    ///
    /// Returns the number of fetches drained. `0` is the steady
    /// state — every call after the first that introduces no new
    /// pending fetches returns `0`. Idempotent on an engine that has
    /// no fetch state installed.
    pub fn run_pending_jobs(&self) -> Result<usize, EvalError> {
        let Some(fs) = self.fetch_state.as_ref() else {
            // No fetch installed → no fetches to drain, but we still
            // need to pump QuickJS's microtask queue so that
            // `queueMicrotask(fn)` / `Promise.resolve().then(fn)`
            // / inline-`.then(...)`-on-an-already-resolved-promise
            // bodies fire before we return. Without this pump,
            // microtask side effects (e.g. a queueMicrotask that
            // sets `globalThis.X = ...`) wouldn't be observable
            // from a subsequent `eval`.
            self.execute_pending_jobs_until_idle()?;
            return Ok(0);
        };
        let mut total = 0;
        // Loop: a `.then` callback can call `fetch()` again, which
        // re-fills the queue. Drain repeatedly until empty.
        loop {
            let drained = fetch::drain_pending(&self.context, &fs.queue, &fs.mode)?;
            if drained == 0 {
                break;
            }
            total += drained;
            // Drive any microtasks queued by the resolve() calls.
            self.execute_pending_jobs_until_idle()?;
        }
        // One final pump for microtasks queued by the last drain.
        self.execute_pending_jobs_until_idle()?;
        Ok(total)
    }

    /// Number of pending fetches not yet drained — observable only
    /// between an `eval` that called `fetch()` and the matching
    /// [`Self::run_pending_jobs`] call.
    pub fn pending_fetches(&self) -> usize {
        self.fetch_state
            .as_ref()
            .map(|fs| fs.queue.len())
            .unwrap_or(0)
    }

    /// Run QuickJS's microtask queue until it reports idle.
    /// Internal helper; the public surface is
    /// [`Self::run_pending_jobs`] (which also drives fetches).
    fn execute_pending_jobs_until_idle(&self) -> Result<(), EvalError> {
        // Loop guard so a pathological microtask that re-enqueues
        // itself doesn't spin forever. 10_000 is well above what any
        // page-hydration pass should produce.
        const MAX_PUMP: usize = 10_000;
        for _ in 0..MAX_PUMP {
            // `Runtime::execute_pending_job` returns Ok(true) if a job
            // ran, Ok(false) if the queue is empty, Err(e) if a job
            // threw. We treat the thrown case as a captured `console.error`
            // — same containment rule as `timers::advance_clock`.
            match self._runtime.execute_pending_job() {
                Ok(true) => continue,
                Ok(false) => return Ok(()),
                Err(e) => {
                    if let Ok(mut buf) = self.console_buffer.lock() {
                        buf.push(ConsoleEntry {
                            level: ConsoleLevel::Error,
                            args: vec![serde_json::Value::String(format!("microtask: {e}"))],
                        });
                    }
                }
            }
        }
        Err(EvalError::Engine(format!(
            "microtask pump exceeded {MAX_PUMP} iterations - possible infinite loop"
        )))
    }

    /// Eval `code` and capture its completion value as JSON.
    ///
    /// If the value is a thenable (a Promise or a Promise-shaped
    /// object), register `.then(resolve, reject)` callbacks that
    /// stash the settled JSON into a shared slot, drive the
    /// microtask pump via [`Self::run_pending_jobs`], and return
    /// the slot's resolved value. Otherwise, serialize the value
    /// synchronously.
    ///
    /// This is what lets a user expression `await heso.flush()`
    /// observe DOM mutations queued by an earlier `dispatchEvent`
    /// — Preact's re-render is queued as a microtask, and the
    /// microtask checkpoint runs before our `.then(resolve)` fires.
    ///
    /// A thenable that never settles (e.g. waits on a macrotask we
    /// don't advance) yields [`serde_json::Value::Null`]. We trust
    /// the run loop to make the user's next call see the eventually
    /// settled state via the virtual clock.
    fn eval_value_with_promise_await(
        &self,
        code: &str,
    ) -> Result<serde_json::Value, EvalError> {
        type SettleSlot =
            Arc<Mutex<Option<Result<serde_json::Value, EvalError>>>>;
        let slot: SettleSlot = Arc::new(Mutex::new(None));
        let needs_pump = self
            .context
            .with(|ctx| -> Result<bool, EvalError> {
                let raw = match ctx.eval::<Value, _>(code).catch(&ctx) {
                    Ok(v) => v,
                    Err(CaughtError::Exception(exc)) => {
                        return Err(EvalError::Exception {
                            message: exc.message().unwrap_or_default(),
                            stack: exc.stack(),
                        })
                    }
                    Err(CaughtError::Value(v)) => {
                        let repr = js_value_to_json(&ctx, v)
                            .unwrap_or(serde_json::Value::Null);
                        return Err(EvalError::ThrownValue { value: repr });
                    }
                    Err(CaughtError::Error(e)) => {
                        return Err(EvalError::Engine(e.to_string()))
                    }
                };

                // Thenable detection: an object whose `.then` is a
                // function. Per Promises/A+ §1.1 that's a thenable.
                let then_fn: Option<Function<'_>> = raw
                    .as_object()
                    .and_then(|o| o.get::<_, Value>("then").ok())
                    .and_then(|v| v.into_function());

                let Some(then_fn) = then_fn else {
                    // Sync value — serialize and stash.
                    let json = js_value_to_json(&ctx, raw)?;
                    *slot.lock().expect("settle slot poisoned") =
                        Some(Ok(json));
                    return Ok(false);
                };

                // Register settle pair. Each callback captures one
                // arg, converts it to JSON, and stashes it. We move a
                // cloned `Arc` into each closure so they can outlive
                // this `ctx.with` block (rquickjs holds the closures
                // inside the Function until JS calls them).
                let slot_ok = slot.clone();
                let resolve = Function::new(
                    ctx.clone(),
                    move |args: Rest<Value>| {
                        let json = match args.into_inner().into_iter().next()
                        {
                            Some(arg) => {
                                let c = arg.ctx().clone();
                                js_value_to_json(&c, arg)
                                    .unwrap_or(serde_json::Value::Null)
                            }
                            None => serde_json::Value::Null,
                        };
                        if let Ok(mut g) = slot_ok.lock() {
                            *g = Some(Ok(json));
                        }
                    },
                )
                .map_err(|e| {
                    EvalError::Engine(format!("build resolve callback: {e}"))
                })?;

                let slot_err = slot.clone();
                let reject = Function::new(
                    ctx.clone(),
                    move |args: Rest<Value>| {
                        let json = match args.into_inner().into_iter().next()
                        {
                            Some(arg) => {
                                let c = arg.ctx().clone();
                                js_value_to_json(&c, arg)
                                    .unwrap_or(serde_json::Value::Null)
                            }
                            None => serde_json::Value::Null,
                        };
                        if let Ok(mut g) = slot_err.lock() {
                            *g = Some(Err(EvalError::ThrownValue {
                                value: json,
                            }));
                        }
                    },
                )
                .map_err(|e| {
                    EvalError::Engine(format!("build reject callback: {e}"))
                })?;

                // promise.then(resolve, reject) — bind `this` to the
                // promise so `then` sees its own internal slots.
                then_fn
                    .call::<_, ()>((This(raw.clone()), resolve, reject))
                    .map_err(|e| {
                        EvalError::Engine(format!("call .then: {e}"))
                    })?;

                Ok(true)
            })?;

        if needs_pump {
            // Drive microtasks: the .then we registered runs here,
            // settling the slot. Anything Preact / React queued
            // before our await (re-renders, effects) also drains.
            self.run_pending_jobs()?;
        }

        // Bind the take()'d value to a local so the MutexGuard from
        // `.lock()` drops at the semicolon, not the end of the match.
        let result = slot.lock().expect("settle slot poisoned").take();
        match result {
            Some(Ok(v)) => Ok(v),
            Some(Err(e)) => Err(e),
            // Thenable registered but never settled inside the
            // microtask pump — could be a Promise waiting on a
            // macrotask (a setTimeout we didn't advance). Return
            // null rather than block.
            None => Ok(serde_json::Value::Null),
        }
    }

    /// Drain and return the console buffer. Useful between calls to
    /// [`Self::advance_clock`] to observe what timer callbacks
    /// logged (or threw) since the last drain.
    pub fn drain_console(&self) -> Vec<ConsoleEntry> {
        let mut buf = self.console_buffer.lock().expect("console buffer poisoned");
        let out = buf.clone();
        buf.clear();
        out
    }

    /// Install `document` as a JS global and execute every inline
    /// `<script>` once, **without** then evaluating any user JS.
    ///
    /// Building block for [`crate::JsSession`]: load a page once,
    /// then run many `click` / `fill` / `submit` / `eval` calls
    /// against the same in-memory DOM. DOM mutations persist across
    /// calls because the `document` global stays installed until
    /// [`Self::install_document`] is called again to navigate to a
    /// different page.
    ///
    /// Equivalent to the install + script-pump prefix of
    /// [`Self::eval_with_html_capture`], minus the trailing user-JS
    /// `eval`. Clears the console buffer on entry so callers can
    /// drain page-script output independently.
    pub fn install_document(
        &self,
        document: Document,
        policy: ScriptFetchPolicy,
    ) -> Result<ScriptOutcome, EvalError> {
        let dom = document.dom_arc();

        self.console_buffer
            .lock()
            .expect("console buffer poisoned")
            .clear();

        self.context
            .with(|ctx| -> rquickjs::Result<()> {
                let doc = Class::instance(ctx.clone(), document)?;
                ctx.globals().set("document", doc)?;
                Ok(())
            })
            .map_err(|e| EvalError::Engine(format!("install document global: {e}")))?;

        // Now that `document` is installed (and `document.createElement`
        // is callable), pre-define `on*` event-handler IDL properties
        // on Element.prototype. Idempotent — only does work on the
        // first install_document.
        install_on_event_handlers(&self.context)?;

        let script_fetch_client = self.fetch_state.as_ref().and_then(|fs| match &fs.mode {
            FetchMode::Live { client, rt_handle } => Some((client.clone(), rt_handle.clone())),
            FetchMode::DeterministicNoCassette => None,
        });
        let base_url = self.base_url();
        let script_outcome = scripts::run_scripts(
            &self.context,
            &dom,
            policy,
            &self.console_buffer,
            script_fetch_client.as_ref(),
            base_url.as_ref(),
        )?;

        self.run_pending_jobs()?;
        Ok(script_outcome)
    }

    /// Evaluate `js` against a parsed HTML page.
    ///
    /// Parses `html` into a [`dom_query::Document`], wraps it in an
    /// [`Arc`], constructs a [`Document`] instance, installs it as
    /// the `document` global, **runs every `<script>` element in
    /// document order** (Phase 1C, ADR 0014), and then runs
    /// [`Self::eval`]. JS can call the full Phase 1B DOM —
    /// `document.querySelector`, `element.textContent`,
    /// `element.getAttribute`, `element.setAttribute`,
    /// `element.innerHTML = ...`, `element.classList.add(...)`,
    /// `element.appendChild(...)`, and the rest — and observe the
    /// post-hydration DOM that page scripts produced.
    ///
    /// External `<script src="...">` references are skipped with a
    /// `console.warn` entry. To choose a different policy (e.g. to
    /// surface a hard error so callers know a real fetch was needed),
    /// use [`Self::eval_with_html_policy`].
    ///
    /// A script that throws is captured into the engine's console
    /// buffer as a `console.error` and the next script still runs —
    /// see [`crate::scripts`] for the determinism rationale.
    ///
    /// Errors propagate the same way as [`Self::eval`].
    pub fn eval_with_html(&self, html: &str, js: &str) -> Result<EvalOutcome, EvalError> {
        self.eval_with_html_policy(html, js, ScriptFetchPolicy::default())
    }

    /// Same as [`Self::eval_with_html`] but lets the caller pick the
    /// [`ScriptFetchPolicy`] for external `<script src=...>` refs.
    /// Returns the same [`EvalOutcome`] as [`Self::eval_with_html`];
    /// the per-script [`ScriptOutcome`] tally is appended onto the
    /// console buffer via the warn/error entries the policy emits and
    /// is otherwise discarded here. Callers that need the structured
    /// counts should use [`Self::eval_with_html_capture`].
    pub fn eval_with_html_policy(
        &self,
        html: &str,
        js: &str,
        policy: ScriptFetchPolicy,
    ) -> Result<EvalOutcome, EvalError> {
        let (outcome, _scripts) = self.eval_with_html_capture(html, js, policy)?;
        Ok(outcome)
    }

    /// Lowest-level wrapper around [`Self::eval_with_html`] that also
    /// returns the [`ScriptOutcome`] tally from the script-pump pass.
    /// Used by tests and by callers that want to surface a per-page
    /// "ran N scripts, M errored" stat in their own receipt.
    ///
    /// Unlike a bare [`Self::eval`] call, this method **does not**
    /// clear the console buffer between the `<script>`-pump pass and
    /// the user's `js` evaluation. The returned [`EvalOutcome`]
    /// therefore contains *both* (a) any console output emitted by
    /// page scripts as they ran (including the `console.warn` /
    /// `console.error` entries [`crate::scripts`] adds for
    /// external-src refs and script throws), *and* (b) anything the
    /// user's `js` argument logged. The structured counts in
    /// [`ScriptOutcome`] are a parallel, per-eval-fresh tally for
    /// callers that only care about totals.
    ///
    /// Rationale: page-script output is part of "what happened on
    /// this page" and an agent debugging a hydration failure wants to
    /// see it without a second roundtrip. The cost is that the
    /// per-eval-fresh contract of [`Self::eval`] *does not extend*
    /// here — callers explicitly choose this method when they want
    /// the merged transcript.
    pub fn eval_with_html_capture(
        &self,
        html: &str,
        js: &str,
        policy: ScriptFetchPolicy,
    ) -> Result<(EvalOutcome, ScriptOutcome), EvalError> {
        let document = Document::from_html(html);
        let dom = document.dom_arc();

        // Clear once at the entry point so the merged transcript is
        // bounded to this single call.
        self.console_buffer
            .lock()
            .expect("console buffer poisoned")
            .clear();

        self.context
            .with(|ctx| -> rquickjs::Result<()> {
                let doc = Class::instance(ctx.clone(), document)?;
                ctx.globals().set("document", doc)?;
                Ok(())
            })
            .map_err(|e| EvalError::Engine(format!("install document global: {e}")))?;

        // See `install_document` — sets up `on*` IDL props on
        // Element.prototype so Preact's `prop.toLowerCase() in el`
        // feature-detect picks the lowercase event name.
        install_on_event_handlers(&self.context)?;

        // Run every <script> against the shared context — mutations
        // land on the same `Arc<dom_query::Document>` the JS-side
        // `document` global wraps, so by the time we eval `js` below,
        // the DOM reflects post-hydration state.
        //
        // External `<script src>` references are honored when policy
        // is `Fetch` *and* the engine was built with a fetch client.
        // The fetch shim borrows the same `reqwest::Client` we use
        // for the rest of the page — see `crate::fetch` for the
        // determinism-gate rules.
        let script_fetch_client = self.fetch_state.as_ref().and_then(|fs| match &fs.mode {
            FetchMode::Live { client, rt_handle } => Some((client.clone(), rt_handle.clone())),
            FetchMode::DeterministicNoCassette => None,
        });
        let base_url = self.base_url();
        let script_outcome = scripts::run_scripts(
            &self.context,
            &dom,
            policy,
            &self.console_buffer,
            script_fetch_client.as_ref(),
            base_url.as_ref(),
        )?;

        // Drive any fetches the page scripts queued before running
        // user JS — agent code expects `globalThis.window.__DATA = await fetch(...)`
        // patterns to have completed by the time the user's `js`
        // argument runs.
        self.run_pending_jobs()?;

        let user_outcome = self.eval_no_clear(js)?;

        // Drive any fetches the user JS queued. This is how
        // `fetch(...).then(r => r.text()).then(t => globalThis.X = t)`
        // becomes observable as `globalThis.X` from a subsequent
        // [`Self::eval`] in the same engine.
        self.run_pending_jobs()?;
        Ok((user_outcome, script_outcome))
    }

    /// Variant of [`Self::eval`] that does **not** clear the console
    /// buffer first. Used by [`Self::eval_with_html_capture`] so the
    /// page-script transcript and the user-script transcript merge
    /// into a single returned [`EvalOutcome::console`].
    ///
    /// Empty `code` is a fast no-op: skip the rquickjs eval and
    /// return the current buffer snapshot. This lets a caller pass
    /// `js = ""` to mean "just run the scripts; give me whatever
    /// they produced."
    fn eval_no_clear(&self, code: &str) -> Result<EvalOutcome, EvalError> {
        let value = if code.is_empty() {
            serde_json::Value::Null
        } else {
            self.eval_value_with_promise_await(code)?
        };

        // Drain pending fetches + microtasks before snapshotting the
        // console — `fetch(url).then(r => r.text()).then(t =>
        // console.log(t))` queues a pending fetch synchronously, and
        // its resolve path eventually pushes a `console.log` entry.
        // Snapshot order matters: pump first, then snapshot.
        self.run_pending_jobs()?;

        let console = self
            .console_buffer
            .lock()
            .expect("console buffer poisoned")
            .clone();

        Ok(EvalOutcome { value, console })
    }

    /// Load `html`, find the element at `selector`, and dispatch a
    /// cancelable `"click"` event on it. The existing event-dispatch
    /// plumbing (per [`crate::events`]) fires any handlers registered
    /// via `addEventListener('click', …)` in script that ran during
    /// the same evaluation.
    ///
    /// The returned [`EvalOutcome`]'s `value` is `true` when an
    /// element matched the selector (and was clicked), `false` when no
    /// element matched — callers can branch on it instead of treating
    /// "not found" as an error. The `console` field carries everything
    /// the click handler's body logged.
    ///
    /// `selector` must be a valid CSS selector that resolves through
    /// `document.querySelector` — typically a `#id` or a tag +
    /// attribute selector built from the action graph entry's
    /// attributes (see the CLI's `selector_for_action` helper).
    ///
    /// Phase 1B: dispatch is **synchronous** and **flat** (no capture
    /// or bubble walk). Listeners attached directly to the target
    /// element fire; ancestors are not visited. Tree-aware bubbling
    /// is a follow-up.
    pub fn dispatch_click(&self, html: &str, selector: &str) -> Result<EvalOutcome, EvalError> {
        // `serde_json::to_string` gives us a JS-safe string literal —
        // it escapes quotes, backslashes, and control chars correctly,
        // so a selector like `a[href="/path with \"quote\""]` round-
        // trips without breaking the snippet.
        let selector_lit = serde_json::to_string(selector)
            .map_err(|e| EvalError::Engine(format!("encode selector: {e}")))?;
        // `script` runs inside `eval_with_html`'s context, where
        // `document` is already wired. We want the expression-position
        // value of the script to be the boolean "found and clicked?",
        // so we wrap the body in an IIFE.
        let script = format!(
            r#"
            (() => {{
                const el = document.querySelector({selector_lit});
                if (!el) return false;
                el.click();
                return true;
            }})()
            "#,
        );
        self.eval_with_html(html, &script)
    }

    /// Load `html`, find the element at `selector`, set its `value`
    /// to `value`, and dispatch first an `"input"` event then a
    /// `"change"` event on it. Both events are constructed as
    /// `bubbles: true, cancelable: true` (matching real browser
    /// behavior when a user types into an `<input>` / `<textarea>`).
    ///
    /// The returned [`EvalOutcome`]'s `value` is `true` when an
    /// element matched the selector, `false` when no element matched
    /// — same shape as [`Self::dispatch_click`]. The `console` field
    /// includes any output from `input` / `change` listeners.
    pub fn set_input_value(
        &self,
        html: &str,
        selector: &str,
        value: &str,
    ) -> Result<EvalOutcome, EvalError> {
        let selector_lit = serde_json::to_string(selector)
            .map_err(|e| EvalError::Engine(format!("encode selector: {e}")))?;
        let value_lit = serde_json::to_string(value)
            .map_err(|e| EvalError::Engine(format!("encode value: {e}")))?;
        let script = format!(
            r#"
            (() => {{
                const el = document.querySelector({selector_lit});
                if (!el) return false;
                el.value = {value_lit};
                el.dispatchEvent(new Event('input', {{ bubbles: true, cancelable: true }}));
                el.dispatchEvent(new Event('change', {{ bubbles: true, cancelable: true }}));
                return true;
            }})()
            "#,
        );
        self.eval_with_html(html, &script)
    }

    /// Load `html`, find the form at `selector`, then find its first
    /// `<button type="submit">` or `<input type="submit">`
    /// descendant and dispatch a cancelable `"click"` event on it.
    /// A form's own `submit` handler — registered via
    /// `form.addEventListener('submit', …)` in real browsers — is
    /// **NOT yet wired in Phase 1B**: this primitive intentionally
    /// only fires the submit-button click, on the assumption that
    /// most modern forms intercept submission via the click on a
    /// submit-type control (or via a JS framework's onSubmit wired
    /// to that control).
    ///
    /// Returns `value: true` when a submit-typed descendant was
    /// found, `value: false` otherwise (form had no submit control,
    /// or selector didn't match).
    ///
    /// Limitation (deferred until [PR2 fetch + form serialize]):
    ///
    /// - No HTTP form submission. If the page lacks JS handlers,
    ///   nothing actually leaves the engine; future work serializes
    ///   the form fields and POSTs them through `reqwest::Client`.
    /// - No `submit` event on the `<form>` itself. The `dispatchEvent`
    ///   plumbing supports it, but most pages observe submit by
    ///   listening on the submit button click, so we leave the form-
    ///   level event out until a real page makes us care.
    pub fn submit_form(&self, html: &str, selector: &str) -> Result<EvalOutcome, EvalError> {
        let selector_lit = serde_json::to_string(selector)
            .map_err(|e| EvalError::Engine(format!("encode selector: {e}")))?;
        let finder = crate::session::SUBMIT_DESCENDANT_FINDER_JS;
        let script = format!(
            r#"
            (() => {{
                const form = document.querySelector({selector_lit});
                if (!form) return false;
                {finder}
                if (!submitter) return false;
                submitter.click();
                return true;
            }})()
            "#,
        );
        self.eval_with_html(html, &script)
    }

    /// Evaluate `code` as a script.
    ///
    /// Returns the script's completion value as JSON plus all
    /// `console.*` calls made during evaluation. The console buffer
    /// is cleared before evaluation begins, so each call produces an
    /// independent log.
    ///
    /// Failure modes:
    ///
    /// - `throw new Error(...)` → [`EvalError::Exception`]
    /// - `throw <other>` → [`EvalError::ThrownValue`]
    /// - Out-of-memory / stack-overflow / parser failure → [`EvalError::Engine`]
    pub fn eval(&self, code: &str) -> Result<EvalOutcome, EvalError> {
        // Reset console buffer per-eval so each call is independent.
        self.console_buffer
            .lock()
            .expect("console buffer poisoned")
            .clear();

        let value = self.eval_value_with_promise_await(code)?;

        // If the script queued any `fetch()` calls, drain them now so
        // `.then(...)` callbacks fire before we return. Side effects
        // observable via `globalThis.X = ...` will be visible to the
        // next `eval` on this engine.
        self.run_pending_jobs()?;

        let console = self
            .console_buffer
            .lock()
            .expect("console buffer poisoned")
            .clone();

        Ok(EvalOutcome { value, console })
    }
}

impl Default for JsEngine {
    fn default() -> Self {
        Self::new()
            .expect("rquickjs Runtime + Context construction should never fail on default config")
    }
}

impl Drop for JsEngine {
    /// Drain the timer scheduler before the runtime tears down so any
    /// [`rquickjs::Persistent`] callbacks still in the heap drop while
    /// their parent [`rquickjs::Runtime`] is still alive. Dropping a
    /// `Persistent` after the runtime is gone trips QuickJS's
    /// `list_empty(&rt->gc_obj_list)` debug assertion and aborts the
    /// process.
    ///
    /// This runs even on panic-unwind: the scheduler is dropped
    /// regardless and we just need its inner `Persistent`s released
    /// first.
    fn drop(&mut self) {
        // Hold the context for the drain so the Persistents drop
        // inside `ctx.with` and the QuickJS engine can free their
        // bound objects synchronously.
        let timers = self.timers.clone();
        let fetch_queue = self.fetch_state.as_ref().map(|fs| fs.queue.clone());
        self.context.with(|_ctx| {
            if let Ok(mut s) = timers.lock() {
                s.clear_all();
            }
            // Drop every queued fetch's Persistent<Function> handles
            // while the runtime is still alive. Same trap that
            // `timers.clear_all` is solving: a Persistent dropped
            // after the parent Runtime aborts the process via
            // QuickJS's `list_empty(&rt->gc_obj_list)` debug assert.
            if let Some(q) = fetch_queue {
                let _drained = q.take_all();
            }
        });
    }
}

/// Register the DOM [`Document`] and [`Element`] classes on the
/// context so they can be instantiated and recognized at runtime.
/// Idempotent — calling on a context that already has them re-binds
/// the constructors, which QuickJS handles cleanly.
fn install_dom_classes(context: &Context) -> Result<(), EvalError> {
    context
        .with(|ctx| dom::register_classes(&ctx))
        .map_err(|e| EvalError::Engine(format!("register DOM classes: {e}")))?;
    Ok(())
}

/// Install `on*` event-handler IDL properties on
/// `Element.prototype` as `null`. Real browsers expose all of
/// them (`el.onclick`, `el.oninput`, `el.onkeydown`, ...) as
/// null-by-default IDL properties; framework code (Preact in
/// particular) feature-detects via
/// `propName.toLowerCase() in el` to decide whether to register
/// a listener under the lowercase event name ("keydown") or to
/// fall back to the camelCase-stripped form ("KeyDown") which
/// never matches a real keyboard event. Without these pre-defines,
/// Preact ships a listener under "KeyDown" and the user's Enter
/// keypress lands on "keydown" — nothing fires.
///
/// The properties only need to *exist* — Preact uses
/// `addEventListener` to actually register handlers. We don't
/// implement IDL-property-to-listener reflection here.
///
/// Runs on every [`JsEngine::install_document`] because we need
/// `document.createElement('div')` to be available to reach the
/// Element prototype (`rquickjs::Class::define` registers the
/// class internally but doesn't put a constructor on globalThis,
/// so JS-side `Element` is undefined). The bootstrap is idempotent
/// via a one-shot sentinel.
fn install_on_event_handlers(context: &Context) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            ctx.eval::<(), _>(ON_EVENT_HANDLERS_BOOTSTRAP)?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install on* event-handler IDL properties: {e}")))?;
    Ok(())
}

/// JS source that pre-populates `on*` event-handler IDL properties
/// on `Element.prototype` with `null`. Called once after
/// `register_classes`.
///
/// The list is the union of:
/// - WHATWG HTML GlobalEventHandlers mixin (~50 names — click, input,
///   keydown, keyup, focus, blur, submit, change, mouse*, drag*,
///   pointer*, touch*, animation*, transition*, ...)
/// - HTMLElement and Document handlers that aren't on GEH
///   (load, unload, beforeunload, hashchange, popstate, message,
///   storage, online, offline, visibilitychange, fullscreenchange,
///   readystatechange, DOMContentLoaded, error, abort, scroll, ...)
///
/// Source: <https://html.spec.whatwg.org/multipage/webappapis.html#globaleventhandlers>
const ON_EVENT_HANDLERS_BOOTSTRAP: &str = r#"
(function() {
    // Idempotent — `install_document` calls this on every navigate.
    if (globalThis.__hesoOnHandlersInstalled) return;

    // `rquickjs::Class::define` registers Element internally but
    // doesn't expose it as a `globalThis.Element` constructor. To
    // reach `Element.prototype` we walk an actual instance.
    if (typeof document === 'undefined') return;
    const probe = document.createElement('div');
    if (!probe) return;
    const proto = Object.getPrototypeOf(probe);
    if (!proto) return;

    const names = [
        // GlobalEventHandlers (WHATWG HTML §8)
        'onabort', 'onauxclick', 'onbeforeinput', 'onbeforematch',
        'onbeforetoggle', 'onblur', 'oncancel', 'oncanplay',
        'oncanplaythrough', 'onchange', 'onclick', 'onclose',
        'oncontextlost', 'oncontextmenu', 'oncontextrestored',
        'oncopy', 'oncuechange', 'oncut', 'ondblclick', 'ondrag',
        'ondragend', 'ondragenter', 'ondragleave', 'ondragover',
        'ondragstart', 'ondrop', 'ondurationchange', 'onemptied',
        'onended', 'onerror', 'onfocus', 'onformdata', 'oninput',
        'oninvalid', 'onkeydown', 'onkeypress', 'onkeyup', 'onload',
        'onloadeddata', 'onloadedmetadata', 'onloadstart',
        'onmousedown', 'onmouseenter', 'onmouseleave', 'onmousemove',
        'onmouseout', 'onmouseover', 'onmouseup', 'onpaste', 'onpause',
        'onplay', 'onplaying', 'onpointercancel', 'onpointerdown',
        'onpointerenter', 'onpointerleave', 'onpointermove',
        'onpointerout', 'onpointerover', 'onpointerrawupdate',
        'onpointerup', 'onprogress', 'onratechange', 'onreset',
        'onresize', 'onscroll', 'onscrollend', 'onsecuritypolicyviolation',
        'onseeked', 'onseeking', 'onselect', 'onselectionchange',
        'onselectstart', 'onslotchange', 'onstalled', 'onsubmit',
        'onsuspend', 'ontimeupdate', 'ontoggle', 'ontouchcancel',
        'ontouchend', 'ontouchmove', 'ontouchstart', 'ontransitioncancel',
        'ontransitionend', 'ontransitionrun', 'ontransitionstart',
        'onvolumechange', 'onwaiting', 'onwebkitanimationend',
        'onwebkitanimationiteration', 'onwebkitanimationstart',
        'onwebkittransitionend', 'onwheel',
        // Document/Window only — but real browsers also expose on
        // Element to support `<body onload=...>` style attributes
        'onafterprint', 'onbeforeprint', 'onbeforeunload',
        'onhashchange', 'onlanguagechange', 'onmessage', 'onmessageerror',
        'onoffline', 'ononline', 'onpagehide', 'onpageshow', 'onpopstate',
        'onrejectionhandled', 'onstorage', 'onunhandledrejection',
        'onunload', 'onvisibilitychange', 'onfullscreenchange',
        'onfullscreenerror', 'onreadystatechange',
        // Animation events (mixed-case Capture cousins handled by
        // Preact's separate `.replace(/Capture$/, "")` step)
        'onanimationcancel', 'onanimationend', 'onanimationiteration',
        'onanimationstart'
    ];
    for (let i = 0; i < names.length; i++) {
        const name = names[i];
        // Only define if missing — never clobber a pre-existing prop.
        if (!(name in proto)) {
            Object.defineProperty(proto, name, {
                value: null,
                writable: true,
                configurable: true,
                enumerable: false
            });
        }
    }
    Object.defineProperty(globalThis, '__hesoOnHandlersInstalled', {
        value: true, writable: false, configurable: false, enumerable: false
    });
})();
"#;

/// Install a `console` global on the given context that routes calls
/// into `buffer`. Each method (`log`, `info`, `warn`, `error`,
/// `debug`, `trace`) becomes a function that converts its arguments
/// to JSON and pushes one [`ConsoleEntry`] onto the buffer.
fn install_console(
    context: &Context,
    buffer: Arc<Mutex<Vec<ConsoleEntry>>>,
) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let console = Object::new(ctx.clone())?;

            // Use one closure per level. `Func::new` takes a closure
            // with the rquickjs argument-conversion conventions; we
            // accept `(Ctx, Rest<Value>)` to get the eval-time
            // context plus all variadic args.
            install_console_method(&ctx, &console, "log", ConsoleLevel::Log, buffer.clone())?;
            install_console_method(&ctx, &console, "info", ConsoleLevel::Info, buffer.clone())?;
            install_console_method(&ctx, &console, "warn", ConsoleLevel::Warn, buffer.clone())?;
            install_console_method(&ctx, &console, "error", ConsoleLevel::Error, buffer.clone())?;
            install_console_method(&ctx, &console, "debug", ConsoleLevel::Debug, buffer.clone())?;
            install_console_method(&ctx, &console, "trace", ConsoleLevel::Trace, buffer.clone())?;

            ctx.globals().set("console", console)?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(e.to_string()))?;
    Ok(())
}

/// Install the seeded-RNG determinism shims onto the context's
/// globals (per ADR 0008):
///
/// 1. **`Math.random`** — replaced with a closure that draws the next
///    `f64` from the engine's [`SeededRng`]. JS code calling
///    `Math.random()` therefore sees the same sequence on every run
///    with the same seed.
/// 2. **`crypto.getRandomValues(view)`** — fills the bytes of the
///    passed `Uint8Array` (or any typed-array-shaped object with a
///    `length`) from the same stream. Returns the view, matching the
///    [WebCrypto spec](https://www.w3.org/TR/WebCryptoAPI/#Crypto-method-getRandomValues).
///    Implementation note: rather than poking at the underlying
///    `ArrayBuffer` via raw pointers (the crate forbids
///    `unsafe_code`), we use indexed `Object::set` — JS engines route
///    `arr[i] = byte` on a TypedArray to the backing buffer, so this
///    is observably equivalent without unsafe.
/// 3. **`crypto.randomUUID()`** — returns a v4-format UUID whose 16
///    bytes come from the same stream.
///
/// `Date.now` and zero-arg `new Date()` are routed separately by
/// [`install_date`], which shares the [`VirtualClock`](crate::timers)
/// backing `setTimeout` / `setInterval`. Explicit-input `Date` forms
/// (`new Date(ms)`, `new Date(str)`, `new Date(y,m,d,...)`,
/// `Date.parse`, `Date.UTC`) are pure functions of their inputs and
/// stay on the QuickJS built-in.
fn install_rng(context: &Context, rng: SeededRng) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();

            // ---- Math.random ----
            //
            // Reach for the existing `Math` object so we don't replace
            // it (and lose Math.floor, Math.abs, etc.). Overriding the
            // `random` property leaves the rest of Math intact.
            let math: Object = globals.get("Math")?;
            let math_random_rng = rng.clone();
            let math_random = Func::from(move || math_random_rng.next_f64());
            math.set("random", math_random)?;

            // ---- crypto ----
            //
            // We unconditionally install a fresh `crypto` object.
            // QuickJS doesn't ship one by default, and even if a host
            // ever pre-populates it the determinism contract requires
            // ours to win.
            let crypto = Object::new(ctx.clone())?;

            // crypto.getRandomValues(view: Uint8Array) -> view
            //
            // We accept the view as a generic [`Object`] (which is
            // what a Uint8Array is at the JS level) so we don't need
            // an rquickjs `TypedArray<u8>` import; we read its
            // `length` and write each byte via indexed `Object::set`.
            // QuickJS routes indexed writes on a TypedArray to its
            // backing buffer, so `view[i]` on the JS side sees the
            // filled bytes.
            // crypto.getRandomValues(view) — fills the buffer in-place
            // and returns the view, per the WebCrypto spec. We return
            // `()` from the Rust side because returning the same
            // `Object<'js>` we received trips an
            // independent-lifetime mismatch in rquickjs's `Func::from`
            // HRTB inference (the closure's input and return lifetimes
            // don't unify with `Object` being invariant). Side-effects
            // (the fill) are the load-bearing part; we re-attach the
            // "return the view" half from JS by wrapping the binding in
            // a tiny preamble below so `crypto.getRandomValues(v)`
            // still produces `v`.
            let gv_rng = rng.clone();
            let get_random_values_raw = Func::from(move |view: Object<'_>| {
                let len: usize = match view.get::<_, usize>("length") {
                    Ok(n) => n,
                    // No `length` property → silently no-op (matches
                    // "throw on bad arg" being more disruptive than
                    // the spec strictly requires for a determinism
                    // shim).
                    Err(_) => return,
                };
                if len == 0 {
                    return;
                }
                // Cap at a sane size to avoid a runaway allocator on
                // huge requests. The WebCrypto spec caps at 65536; we
                // honor that.
                const MAX_LEN: usize = 65_536;
                let effective = len.min(MAX_LEN);
                let mut buf = vec![0u8; effective];
                gv_rng.fill_bytes(&mut buf);
                for (i, byte) in buf.iter().enumerate() {
                    // Best-effort: if a particular index set fails
                    // (e.g. the view is read-only), we skip it rather
                    // than abort the fill. `effective <= 65_536` so
                    // the cast to u32 is loss-free.
                    let _ = view.set(i as u32, *byte);
                }
            });
            // Install the raw fill function on the crypto object
            // under a private name; the JS-side wrap below renames it
            // to the spec-shape `getRandomValues` that returns the
            // view.
            crypto.set("__getRandomValuesRaw", get_random_values_raw)?;

            // crypto.randomUUID() -> string
            let uuid_rng = rng.clone();
            let random_uuid = Func::from(move || uuid_rng.random_uuid());
            crypto.set("randomUUID", random_uuid)?;

            // Publish the crypto global before running the wrap script
            // so the script can reach it.
            globals.set("crypto", crypto)?;

            // Wrap the raw fill function so the spec-shape
            // `crypto.getRandomValues(view)` returns `view`. The Rust
            // side returns `()` because rquickjs's `Func::from` HRTB
            // can't unify the input and return Object lifetimes when
            // both are anonymous; the JS wrapper re-attaches the
            // "return the view" half cheaply.
            let wrap_src = r#"
                (function() {
                    const raw = globalThis.crypto.__getRandomValuesRaw;
                    globalThis.crypto.getRandomValues = function(view) {
                        raw(view);
                        return view;
                    };
                    delete globalThis.crypto.__getRandomValuesRaw;
                })()
            "#;
            ctx.eval::<(), _>(wrap_src)?;

            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install rng: {e}")))?;
    Ok(())
}

/// Install the deterministic `Date` shim onto the context's globals
/// (per [ADR 0008]).
///
/// Two surfaces are intercepted; everything else stays on QuickJS's
/// built-in `Date`:
///
/// 1. **`Date.now()`** — returns the current
///    [`VirtualClock`](crate::timers) reading as an `f64`, matching
///    the spec's "milliseconds since the Unix epoch" shape. Because
///    the clock starts at zero on a fresh engine, `Date.now()` on a
///    just-constructed engine is `0` — i.e. midnight 1970-01-01 UTC.
///    The host can shift this by either calling
///    [`JsEngine::advance_clock`] (the same control surface as timers)
///    or by setting an initial epoch via a future
///    `new_with_seed_and_epoch_ms` constructor — both are valid; both
///    keep determinism.
///
/// 2. **`new Date()`** (zero-arg construction) — pins the constructed
///    `Date` instance to the same virtual time. All explicit-input
///    forms (`new Date(ms)`, `new Date(str)`, `new Date(y, m, d, ...)`)
///    are *pure functions of their inputs* and pass through to the
///    QuickJS built-in unchanged.
///
/// ## Why this shape (monkey-patch over JS)
///
/// QuickJS's `Date` is implemented in C and built into the runtime;
/// there's no clean rquickjs API to swap out the constructor's
/// host-time-reading code path. The idiomatic move (matching
/// `sinon.useFakeTimers` and `happy-dom`'s fake clock) is to leave
/// the original `Date` intact and replace `globalThis.Date` with a
/// thin JS wrapper that forwards every form except the zero-arg
/// constructor to the original, and the zero-arg constructor to
/// `new OriginalDate(Date.now())`. `Date.prototype` and the static
/// surface (`Date.parse`, `Date.UTC`, `Date.now`) are copied across
/// so `instanceof Date`, `Date.parse('...')`, etc. still work.
///
/// We rebind `Date.now` first (Rust closure → `VirtualClock.now_ms`)
/// then run a tiny JS bootstrap that builds the wrapper using the
/// rebound `Date.now`. The wrapper itself is JS so it stays inside
/// the QuickJS sandbox — no Rust callback per construction.
///
/// [ADR 0008]: ../../decisions/0008-deterministic-execution.md
fn install_date(
    context: &Context,
    timers: Arc<Mutex<TimerScheduler>>,
) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();

            // Step 1: replace `Date.now` on the *original* Date with a
            // closure that reads the shared VirtualClock. The wrapper
            // built in step 2 then copies this Date.now onto itself.
            //
            // The clock is read under the scheduler lock; on a poisoned
            // mutex (effectively unreachable — single-threaded engine)
            // we fall back to `0.0` rather than panic.
            let now_timers = timers.clone();
            let date_now = Func::from(move || -> f64 {
                match now_timers.lock() {
                    Ok(s) => s.now_ms() as f64,
                    Err(_) => 0.0,
                }
            });
            let date_obj: Object = globals.get("Date")?;
            date_obj.set("now", date_now)?;

            // Step 2: build the JS-side wrapper around the original
            // Date. The wrapper:
            //
            //   - intercepts zero-arg `new Date()` → returns
            //     `new OriginalDate(Date.now())` (which now reads the
            //     virtual clock).
            //   - forwards every other construction form unchanged.
            //   - forwards calls without `new` (`Date()` returns a
            //     string in the spec) to the original.
            //   - preserves `Date.prototype` so `instanceof Date` keeps
            //     working for both zero-arg and explicit-input
            //     instances.
            //   - copies the static surface (`now`, `parse`, `UTC`)
            //     across so `Date.parse` / `Date.UTC` / `Date.now`
            //     still resolve.
            //
            // Note: we copy *all* own properties of the original Date
            // (rather than hardcoding {now, parse, UTC}) so any future
            // QuickJS-side additions ride along automatically.
            let bootstrap = r#"
                (function() {
                    const OriginalDate = globalThis.Date;
                    function WrappedDate(...args) {
                        // Called without `new` — per the spec,
                        // `Date(...)` returns a string representation
                        // of the current time, ignoring its arguments.
                        // Defer to the original so we keep that
                        // behavior; the original will route through
                        // our patched `Date.now` via its own
                        // construction path on most engines, but
                        // QuickJS reads the host clock here, so we
                        // pin it explicitly using the virtual clock.
                        if (!(this instanceof WrappedDate)) {
                            return new OriginalDate(OriginalDate.now()).toString();
                        }
                        // Zero-arg construction: pin to virtual clock.
                        if (args.length === 0) {
                            return new OriginalDate(OriginalDate.now());
                        }
                        // Explicit-input forms — pass through.
                        // Spread to preserve `new Date(y, m, d, ...)`
                        // multi-arg shape.
                        return new OriginalDate(...args);
                    }
                    // Preserve prototype identity so
                    // `instanceof Date` works for instances created
                    // by both the wrapper and the original (the
                    // wrapper returns instances constructed by the
                    // original, so they're `instanceof OriginalDate`
                    // already; by aliasing prototypes we also satisfy
                    // `instanceof WrappedDate`).
                    WrappedDate.prototype = OriginalDate.prototype;
                    // Copy the static surface (now, parse, UTC, and
                    // any future additions) onto the wrapper.
                    for (const key of Object.getOwnPropertyNames(OriginalDate)) {
                        if (key === 'length' || key === 'name' || key === 'prototype') {
                            continue;
                        }
                        const desc = Object.getOwnPropertyDescriptor(OriginalDate, key);
                        if (desc) {
                            Object.defineProperty(WrappedDate, key, desc);
                        }
                    }
                    globalThis.Date = WrappedDate;
                })()
            "#;
            ctx.eval::<(), _>(bootstrap)?;

            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install date: {e}")))?;
    Ok(())
}

/// Install (or re-install) `globalThis.location` and the
/// `globalThis.window` self-reference so page scripts that read
/// `location.href` / `window.location.pathname` / `window.location`
/// see the engine's current page URL.
///
/// We re-write the whole `location` object on each call (cheap —
/// it's a tiny POJO) instead of reading via a getter, so plain
/// property access stays synchronous and side-effect-free. The host
/// calls this from [`JsEngine::set_base_url`] on every navigation.
///
/// `None` resolves to `about:blank`. Mutation surface (`assign`,
/// `replace`, `reload`, `toString`) is installed but is a no-op for
/// now — heso does not yet implement script-driven navigation
/// (that's part of the Phase 2 stubs PR alongside `history.pushState`).
fn install_location(context: &Context, url: Option<&Url>) -> Result<(), EvalError> {
    let (href, protocol, host, hostname, port, pathname, search, hash, origin) = match url {
        Some(u) => {
            let port = u.port().map(|p| p.to_string()).unwrap_or_default();
            let host = match u.port() {
                Some(p) => format!("{}:{}", u.host_str().unwrap_or(""), p),
                None => u.host_str().unwrap_or("").to_string(),
            };
            let origin = match (u.scheme(), u.host_str()) {
                (s, Some(h)) if s == "http" || s == "https" => match u.port() {
                    Some(p) => format!("{}://{}:{}", s, h, p),
                    None => format!("{}://{}", s, h),
                },
                _ => "null".to_string(),
            };
            (
                u.as_str().to_string(),
                format!("{}:", u.scheme()),
                host,
                u.host_str().unwrap_or("").to_string(),
                port,
                u.path().to_string(),
                u.query().map(|q| format!("?{}", q)).unwrap_or_default(),
                u.fragment().map(|f| format!("#{}", f)).unwrap_or_default(),
                origin,
            )
        }
        None => (
            "about:blank".to_string(),
            "about:".to_string(),
            String::new(),
            String::new(),
            String::new(),
            "blank".to_string(),
            String::new(),
            String::new(),
            "null".to_string(),
        ),
    };

    context
        .with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();
            let loc = Object::new(ctx.clone())?;
            loc.set("href", href.clone())?;
            loc.set("protocol", protocol)?;
            loc.set("host", host)?;
            loc.set("hostname", hostname)?;
            loc.set("port", port)?;
            loc.set("pathname", pathname)?;
            loc.set("search", search)?;
            loc.set("hash", hash)?;
            loc.set("origin", origin)?;
            // Best-effort stubs. Real navigation isn't wired yet —
            // see the Phase 2 stubs PR. `toString()` returns `href`
            // per the WHATWG `Location` interface.
            let href_for_to_string = href.clone();
            loc.set(
                "toString",
                Func::from(move || -> String { href_for_to_string.clone() }),
            )?;
            loc.set("assign", Func::from(|_: String| {}))?;
            loc.set("replace", Func::from(|_: String| {}))?;
            loc.set("reload", Func::from(|| {}))?;
            globals.set("location", loc)?;

            // `window` aliases `globalThis` so `window.location`,
            // `window.document`, `window.setTimeout`, etc. all
            // resolve via the same prototype chain page scripts
            // expect. Install once; subsequent calls re-bind which
            // is a no-op.
            ctx.eval::<(), _>(
                "if (typeof globalThis.window === 'undefined') { globalThis.window = globalThis; }",
            )?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install location: {e}")))?;
    Ok(())
}

/// Install `globalThis.__hesoMakeStyleProxy` — the JS-side factory
/// backing the [`Element.style`](crate::dom::Element) getter. Idempotent
/// (calling twice rebinds the global, which is fine).
///
/// See [`STYLE_PROXY_BOOTSTRAP`] for the source and a design discussion
/// of the trap semantics.
fn install_style_proxy(context: &Context) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            ctx.eval::<(), _>(STYLE_PROXY_BOOTSTRAP)?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install style proxy: {e}")))?;
    Ok(())
}

/// JS source for `__hesoMakeStyleProxy`. Backs the `Element.style`
/// getter; see [`crate::dom::Element::style`] for the call site.
///
/// Design notes:
///
/// - `has(key)` consults `KNOWN_CSS_PROPS`, a Set built from the
///   csstype standard-property list (derived from MDN data —
///   <https://github.com/frenic/csstype>, longhands + shorthands +
///   SVG presentation properties, vendor prefixes stripped). Real
///   `CSSStyleDeclaration` exposes a *closed* property list:
///   `'color' in el.style === true` but `'foo' in el.style === false`
///   in every shipping browser. React's feature-detect (`for (t in n)
///   if (t in Ct) ...` where `Ct = el.style`) specifically relies on
///   that closed list to discover whether `n`'s key maps to a real
///   CSS property; returning `true` for everything makes React copy
///   arbitrary keys onto inline style and silently corrupts opinionated
///   CSS-in-JS libraries. CSS custom properties (those starting with
///   `--`) bypass the allow-list and are always reported present, per
///   spec (they're open-ended by design).
/// - camelCase ↔ kebab-case normalization runs on every access so
///   `style.backgroundColor = "red"` and `style.getPropertyValue
///   ("background-color")` agree. CSS custom properties (those
///   starting with `--`) bypass the conversion.
/// - Writes go through `set_attr` on the backing element, so the
///   serialized `style="..."` attribute stays in sync and is
///   visible via `outerHTML` / `getAttribute('style')`. We do *not*
///   filter `set` through `KNOWN_CSS_PROPS` even though real browsers
///   silently no-op writes to unknown property names — too many
///   frameworks rely on the open write surface, and the read path
///   (which is what `for ... in` ultimately consults) is the
///   load-bearing half.
/// - The `getPropertyValue` / `setProperty` / `removeProperty`
///   methods are the spec-canonical interface; some frameworks
///   reach for them instead of direct property access. Wired here
///   so they share the same parse/serialize round-trip.
const STYLE_PROXY_BOOTSTRAP: &str = r#"
(function() {
    // Canonical CSS property allow-list. Source: csstype standard-only
    // surface (longhands + shorthands + SVG presentation props),
    // derived from MDN's compat data — https://github.com/frenic/csstype.
    // Vendor-prefixed entries (`-webkit-*`, `-moz-*`, `-ms-*`, `-o-*`,
    // `-khtml-*`, `-epub-*`, `-apple-*`) are intentionally excluded:
    // React's feature-detect for prefixed CSS does
    // `prefix + camelCased in style` (e.g. `'WebkitTransform' in style`),
    // which lookups would *fail* against this set, sending React to its
    // unprefixed-fallback branch — which is what we want, since we
    // serialize a single unprefixed value and the browser would do its
    // own normalization downstream.
    const KNOWN_CSS_PROPS = new Set([
        'accent-color', 'align-content', 'align-items', 'align-self',
        'align-tracks', 'alignment-baseline', 'all', 'anchor-name',
        'anchor-scope', 'animation', 'animation-composition', 'animation-delay',
        'animation-direction', 'animation-duration', 'animation-fill-mode',
        'animation-iteration-count', 'animation-name', 'animation-play-state',
        'animation-range', 'animation-range-end', 'animation-range-start',
        'animation-timeline', 'animation-timing-function', 'appearance',
        'aspect-ratio', 'backdrop-filter', 'backface-visibility', 'background',
        'background-attachment', 'background-blend-mode', 'background-clip',
        'background-color', 'background-image', 'background-origin',
        'background-position', 'background-position-x', 'background-position-y',
        'background-repeat', 'background-size', 'baseline-shift', 'block-size',
        'border', 'border-block', 'border-block-color', 'border-block-end',
        'border-block-end-color', 'border-block-end-style',
        'border-block-end-width', 'border-block-start',
        'border-block-start-color', 'border-block-start-style',
        'border-block-start-width', 'border-block-style', 'border-block-width',
        'border-bottom', 'border-bottom-color', 'border-bottom-left-radius',
        'border-bottom-right-radius', 'border-bottom-style',
        'border-bottom-width', 'border-collapse', 'border-color',
        'border-end-end-radius', 'border-end-start-radius', 'border-image',
        'border-image-outset', 'border-image-repeat', 'border-image-slice',
        'border-image-source', 'border-image-width', 'border-inline',
        'border-inline-color', 'border-inline-end', 'border-inline-end-color',
        'border-inline-end-style', 'border-inline-end-width',
        'border-inline-start', 'border-inline-start-color',
        'border-inline-start-style', 'border-inline-start-width',
        'border-inline-style', 'border-inline-width', 'border-left',
        'border-left-color', 'border-left-style', 'border-left-width',
        'border-radius', 'border-right', 'border-right-color',
        'border-right-style', 'border-right-width', 'border-spacing',
        'border-start-end-radius', 'border-start-start-radius', 'border-style',
        'border-top', 'border-top-color', 'border-top-left-radius',
        'border-top-right-radius', 'border-top-style', 'border-top-width',
        'border-width', 'bottom', 'box-decoration-break', 'box-shadow',
        'box-sizing', 'break-after', 'break-before', 'break-inside',
        'caption-side', 'caret', 'caret-color', 'caret-shape', 'clear', 'clip',
        'clip-path', 'clip-rule', 'color', 'color-adjust', 'color-interpolation',
        'color-interpolation-filters', 'color-rendering', 'color-scheme',
        'column-count', 'column-fill', 'column-gap', 'column-rule',
        'column-rule-color', 'column-rule-style', 'column-rule-width',
        'column-span', 'column-width', 'columns', 'contain',
        'contain-intrinsic-block-size', 'contain-intrinsic-height',
        'contain-intrinsic-inline-size', 'contain-intrinsic-size',
        'contain-intrinsic-width', 'container', 'container-name',
        'container-type', 'content', 'content-visibility', 'counter-increment',
        'counter-reset', 'counter-set', 'cursor', 'cx', 'cy', 'd', 'direction',
        'display', 'dominant-baseline', 'empty-cells', 'field-sizing', 'fill',
        'fill-opacity', 'fill-rule', 'filter', 'flex', 'flex-basis',
        'flex-direction', 'flex-flow', 'flex-grow', 'flex-shrink', 'flex-wrap',
        'float', 'flood-color', 'flood-opacity', 'font', 'font-family',
        'font-feature-settings', 'font-kerning', 'font-language-override',
        'font-optical-sizing', 'font-palette', 'font-size', 'font-size-adjust',
        'font-smooth', 'font-stretch', 'font-style', 'font-synthesis',
        'font-synthesis-position', 'font-synthesis-small-caps',
        'font-synthesis-style', 'font-synthesis-weight', 'font-variant',
        'font-variant-alternates', 'font-variant-caps', 'font-variant-east-asian',
        'font-variant-emoji', 'font-variant-ligatures', 'font-variant-numeric',
        'font-variant-position', 'font-variation-settings', 'font-weight',
        'font-width', 'forced-color-adjust', 'gap', 'glyph-orientation-vertical',
        'grid', 'grid-area', 'grid-auto-columns', 'grid-auto-flow',
        'grid-auto-rows', 'grid-column', 'grid-column-end', 'grid-column-start',
        'grid-row', 'grid-row-end', 'grid-row-start', 'grid-template',
        'grid-template-areas', 'grid-template-columns', 'grid-template-rows',
        'hanging-punctuation', 'height', 'hyphenate-character',
        'hyphenate-limit-chars', 'hyphens', 'image-orientation',
        'image-rendering', 'image-resolution', 'initial-letter',
        'initial-letter-align', 'inline-size', 'inset', 'inset-block',
        'inset-block-end', 'inset-block-start', 'inset-inline',
        'inset-inline-end', 'inset-inline-start', 'interpolate-size', 'isolation',
        'justify-content', 'justify-items', 'justify-self', 'justify-tracks',
        'left', 'letter-spacing', 'lighting-color', 'line-break', 'line-clamp',
        'line-height', 'line-height-step', 'list-style', 'list-style-image',
        'list-style-position', 'list-style-type', 'margin', 'margin-block',
        'margin-block-end', 'margin-block-start', 'margin-bottom',
        'margin-inline', 'margin-inline-end', 'margin-inline-start',
        'margin-left', 'margin-right', 'margin-top', 'margin-trim', 'marker',
        'marker-end', 'marker-mid', 'marker-start', 'mask', 'mask-border',
        'mask-border-mode', 'mask-border-outset', 'mask-border-repeat',
        'mask-border-slice', 'mask-border-source', 'mask-border-width',
        'mask-clip', 'mask-composite', 'mask-image', 'mask-mode', 'mask-origin',
        'mask-position', 'mask-repeat', 'mask-size', 'mask-type',
        'masonry-auto-flow', 'math-depth', 'math-shift', 'math-style',
        'max-block-size', 'max-height', 'max-inline-size', 'max-lines',
        'max-width', 'min-block-size', 'min-height', 'min-inline-size',
        'min-width', 'mix-blend-mode', 'motion', 'motion-distance', 'motion-path',
        'motion-rotation', 'object-fit', 'object-position', 'object-view-box',
        'offset', 'offset-anchor', 'offset-distance', 'offset-path',
        'offset-position', 'offset-rotate', 'offset-rotation', 'opacity', 'order',
        'orphans', 'outline', 'outline-color', 'outline-offset', 'outline-style',
        'outline-width', 'overflow', 'overflow-anchor', 'overflow-block',
        'overflow-clip-box', 'overflow-clip-margin', 'overflow-inline',
        'overflow-wrap', 'overflow-x', 'overflow-y', 'overlay',
        'overscroll-behavior', 'overscroll-behavior-block',
        'overscroll-behavior-inline', 'overscroll-behavior-x',
        'overscroll-behavior-y', 'padding', 'padding-block', 'padding-block-end',
        'padding-block-start', 'padding-bottom', 'padding-inline',
        'padding-inline-end', 'padding-inline-start', 'padding-left',
        'padding-right', 'padding-top', 'page', 'paint-order', 'perspective',
        'perspective-origin', 'place-content', 'place-items', 'place-self',
        'pointer-events', 'position', 'position-anchor', 'position-area',
        'position-try', 'position-try-fallbacks', 'position-try-order',
        'position-visibility', 'print-color-adjust', 'quotes', 'r', 'resize',
        'right', 'rotate', 'row-gap', 'ruby-align', 'ruby-merge', 'ruby-overhang',
        'ruby-position', 'rx', 'ry', 'scale', 'scroll-behavior',
        'scroll-initial-target', 'scroll-margin', 'scroll-margin-block',
        'scroll-margin-block-end', 'scroll-margin-block-start',
        'scroll-margin-bottom', 'scroll-margin-inline',
        'scroll-margin-inline-end', 'scroll-margin-inline-start',
        'scroll-margin-left', 'scroll-margin-right', 'scroll-margin-top',
        'scroll-padding', 'scroll-padding-block', 'scroll-padding-block-end',
        'scroll-padding-block-start', 'scroll-padding-bottom',
        'scroll-padding-inline', 'scroll-padding-inline-end',
        'scroll-padding-inline-start', 'scroll-padding-left',
        'scroll-padding-right', 'scroll-padding-top', 'scroll-snap-align',
        'scroll-snap-margin', 'scroll-snap-margin-bottom',
        'scroll-snap-margin-left', 'scroll-snap-margin-right',
        'scroll-snap-margin-top', 'scroll-snap-stop', 'scroll-snap-type',
        'scroll-timeline', 'scroll-timeline-axis', 'scroll-timeline-name',
        'scrollbar-color', 'scrollbar-gutter', 'scrollbar-width',
        'shape-image-threshold', 'shape-margin', 'shape-outside',
        'shape-rendering', 'speak-as', 'stop-color', 'stop-opacity', 'stroke',
        'stroke-color', 'stroke-dasharray', 'stroke-dashoffset', 'stroke-linecap',
        'stroke-linejoin', 'stroke-miterlimit', 'stroke-opacity', 'stroke-width',
        'tab-size', 'table-layout', 'text-align', 'text-align-last',
        'text-anchor', 'text-autospace', 'text-box', 'text-box-edge',
        'text-box-trim', 'text-combine-upright', 'text-decoration',
        'text-decoration-color', 'text-decoration-line', 'text-decoration-skip',
        'text-decoration-skip-ink', 'text-decoration-style',
        'text-decoration-thickness', 'text-emphasis', 'text-emphasis-color',
        'text-emphasis-position', 'text-emphasis-style', 'text-indent',
        'text-justify', 'text-orientation', 'text-overflow', 'text-rendering',
        'text-shadow', 'text-size-adjust', 'text-spacing-trim', 'text-transform',
        'text-underline-offset', 'text-underline-position', 'text-wrap',
        'text-wrap-mode', 'text-wrap-style', 'timeline-scope', 'top',
        'touch-action', 'transform', 'transform-box', 'transform-origin',
        'transform-style', 'transition', 'transition-behavior',
        'transition-delay', 'transition-duration', 'transition-property',
        'transition-timing-function', 'translate', 'unicode-bidi', 'user-select',
        'vector-effect', 'vertical-align', 'view-timeline', 'view-timeline-axis',
        'view-timeline-inset', 'view-timeline-name', 'view-transition-class',
        'view-transition-name', 'visibility', 'white-space',
        'white-space-collapse', 'widows', 'width', 'will-change', 'word-break',
        'word-spacing', 'word-wrap', 'writing-mode', 'x', 'y', 'z-index', 'zoom'
    ]);
    function parseStyle(s) {
        const out = Object.create(null);
        if (!s) return out;
        for (const part of s.split(';')) {
            const i = part.indexOf(':');
            if (i < 0) continue;
            const k = part.slice(0, i).trim();
            const v = part.slice(i + 1).trim();
            if (k) out[k] = v;
        }
        return out;
    }
    function serializeStyle(o) {
        const parts = [];
        for (const k of Object.keys(o)) parts.push(k + ': ' + o[k]);
        return parts.join('; ');
    }
    function camelToKebab(s) {
        // Custom properties (--*) are not camelCase — pass through.
        if (s.startsWith('--')) return s;
        return s.replace(/[A-Z]/g, function(m) { return '-' + m.toLowerCase(); });
    }
    function isKnownProp(prop) {
        // CSS custom properties are open-ended; spec says they're
        // always "present" on the declaration regardless of allow-list.
        if (prop.startsWith('--')) return true;
        // Normalize camelCase queries (`backgroundColor`) to the kebab
        // form the allow-list stores. Leading-capital queries
        // (`BackgroundColor`) become `-background-color` after the
        // regex, which fails the lookup — matching real-browser
        // behavior where `'BackgroundColor' in el.style === false`.
        return KNOWN_CSS_PROPS.has(camelToKebab(prop));
    }
    globalThis.__hesoMakeStyleProxy = function(read, write) {
        const methods = {
            getPropertyValue: function(name) {
                const o = parseStyle(read());
                return o[camelToKebab(String(name))] || '';
            },
            setProperty: function(name, value) {
                const o = parseStyle(read());
                const k = camelToKebab(String(name));
                if (value == null || value === '') delete o[k];
                else o[k] = String(value);
                write(serializeStyle(o));
            },
            removeProperty: function(name) {
                const o = parseStyle(read());
                const k = camelToKebab(String(name));
                const prev = o[k] || '';
                delete o[k];
                write(serializeStyle(o));
                return prev;
            }
        };
        return new Proxy(Object.create(null), {
            get: function(_, prop) {
                if (typeof prop === 'symbol') return undefined;
                if (prop === 'cssText') return read();
                if (methods[prop]) return methods[prop];
                if (prop === 'length') return Object.keys(parseStyle(read())).length;
                const o = parseStyle(read());
                return o[camelToKebab(prop)] || '';
            },
            set: function(_, prop, value) {
                if (typeof prop === 'symbol') return true;
                if (prop === 'cssText') { write(String(value == null ? '' : value)); return true; }
                const o = parseStyle(read());
                const k = camelToKebab(prop);
                const v = value == null ? '' : String(value);
                if (v === '') delete o[k];
                else o[k] = v;
                write(serializeStyle(o));
                return true;
            },
            has: function(_, prop) {
                // Real-browser `CSSStyleDeclaration` is a *closed*
                // property list. React's hydration feature-detect
                // (`for (t in n) if (t in Ct) ...` where `Ct = el.style`)
                // depends on the closed list — returning `true` for
                // every key makes React copy arbitrary attributes onto
                // inline style and silently corrupts opinionated
                // CSS-in-JS libraries. Custom properties (`--*`) are
                // open-ended per spec; everything else is gated by
                // the allow-list.
                if (typeof prop !== 'string') return false;
                return isKnownProp(prop);
            },
            deleteProperty: function(_, prop) {
                if (typeof prop !== 'string') return true;
                const o = parseStyle(read());
                delete o[camelToKebab(prop)];
                write(serializeStyle(o));
                return true;
            },
            ownKeys: function() {
                return Object.keys(parseStyle(read()));
            },
            getOwnPropertyDescriptor: function(_, prop) {
                if (typeof prop !== 'string') return undefined;
                const o = parseStyle(read());
                const k = camelToKebab(prop);
                if (k in o) return { enumerable: true, configurable: true, value: o[k], writable: true };
                return undefined;
            }
        });
    };
})();
"#;

/// Install the "trivial browser globals" cluster on the context.
///
/// Each individual API is small — a `navigator` POJO, `performance.now`
/// reading the virtual clock, `queueMicrotask` piggybacking on
/// `Promise.resolve().then(...)`, `requestAnimationFrame` routing to
/// `setTimeout(cb, 16)`, base64 `atob`/`btoa` via the Rust `base64`
/// crate, a `matchMedia` POJO that always returns `matches: false`,
/// and in-memory `localStorage` / `sessionStorage` maps. Collectively
/// they unblock dozens of init paths on real-world pages that would
/// otherwise throw on a missing global.
///
/// ## Design choices, with citations
///
/// - **User-agent string**: `"Mozilla/5.0 (compatible; heso/0.0.1)"`.
///   Real-browser-shaped (begins with `Mozilla/5.0` so naive
///   UA-sniffers don't crash), but identifies as heso so server
///   operators see who's calling. The `(compatible; ...)` form is the
///   same family as Googlebot's user-agent — the convention is "tell
///   sniffers a baseline shape; identify yourself parenthetically."
/// - **`navigator.webdriver = false`**: anti-bot scripts gate on this
///   (Playwright defaults to `true`, which trips Cloudflare et al.).
///   For an agent browser that genuinely isn't using WebDriver, the
///   honest value is `false`. ADR 0016 (positioning) makes this the
///   policy: heso is an agent browser, not a stealth Selenium.
/// - **`requestAnimationFrame` → `setTimeout(cb, 16)`**: 16ms ≈ 60fps,
///   close enough that pages relying on rAF for animation timing see
///   a sensible-shaped delay. The ID returned by `setTimeout` doubles
///   as the rAF id — `cancelAnimationFrame` simply calls
///   `clearTimeout`.
/// - **`performance.now()`**: pinned to `VirtualClock.now_ms() as f64`,
///   same source as `Date.now`. Real browsers spec performance.now as
///   "monotonic clock starting at `performance.timeOrigin`"; we give
///   millisecond resolution from `0` (matching `Date.now`'s start).
/// - **`matchMedia`**: always `matches: false`. No layout → no media
///   queries can match. Frameworks gate on `matchMedia` *existing*,
///   not on a specific match result.
/// - **Storage**: in-memory `Map` per engine, separate maps for
///   `localStorage` and `sessionStorage`. ADR 0014 commits to this
///   shape (in-memory, deterministic, no persistence yet).
/// - **`atob` / `btoa`**: Rust-side closures using the `base64` crate
///   (0.22 — `Engine::decode` / `Engine::encode` with the standard
///   alphabet). Invalid input throws a plain `Error` for now; a full
///   `DOMException('InvalidCharacterError')` is a later concern.
fn install_browser_apis(
    context: &Context,
    timers: Arc<Mutex<TimerScheduler>>,
) -> Result<(), EvalError> {
    use base64::Engine as _;
    let perf_timers = timers.clone();
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();

            // ---- navigator ----
            let navigator = Object::new(ctx.clone())?;
            navigator.set("userAgent", "Mozilla/5.0 (compatible; heso/0.0.1)")?;
            navigator.set("language", "en-US")?;
            // `languages` is read-only in real browsers; we expose a
            // plain array (frameworks iterate, they don't mutate).
            let languages = rquickjs::Array::new(ctx.clone())?;
            languages.set(0, "en-US")?;
            navigator.set("languages", languages)?;
            navigator.set("onLine", true)?;
            navigator.set("cookieEnabled", true)?;
            // anti-bot scripts gate on `webdriver`; heso isn't using
            // WebDriver, so the honest answer is `false`. See ADR 0016.
            navigator.set("webdriver", false)?;
            // Platform is a freeform string; "Linux x86_64" is the
            // baseline Chrome/Firefox value on Linux desktops and is
            // the safest default for cross-platform sniffers.
            navigator.set("platform", "Linux x86_64")?;
            globals.set("navigator", navigator)?;

            // ---- performance.now() ----
            //
            // Reads the same VirtualClock that backs Date.now and the
            // timer scheduler. Determinism: same advance_clock sequence
            // → same performance.now() readings across engines.
            let perf = Object::new(ctx.clone())?;
            let now_fn = Func::from(move || -> f64 {
                match perf_timers.lock() {
                    Ok(s) => s.now_ms() as f64,
                    Err(_) => 0.0,
                }
            });
            perf.set("now", now_fn)?;
            // performance.timeOrigin: real browsers expose this as "ms
            // since UNIX epoch when navigation started". heso's virtual
            // clock starts at 0 so timeOrigin = 0 keeps the invariant
            // `Date.now() === performance.timeOrigin + performance.now()`
            // true on a fresh engine.
            perf.set("timeOrigin", 0.0_f64)?;
            globals.set("performance", perf)?;

            // ---- atob / btoa ----
            //
            // base64 0.22's `STANDARD` engine uses the RFC 4648
            // alphabet with padding, which is what real browsers'
            // atob/btoa do.
            let atob = Func::from(|ctx: Ctx<'_>, s: String| -> rquickjs::Result<String> {
                match base64::engine::general_purpose::STANDARD.decode(s.as_bytes()) {
                    // atob returns a "binary string" — each output byte
                    // becomes one char (code point 0..=255). Mapping via
                    // `from_utf8_lossy` would corrupt high bytes; map
                    // byte-to-char directly.
                    Ok(bytes) => {
                        let mut out = String::with_capacity(bytes.len());
                        for b in bytes {
                            out.push(b as char);
                        }
                        Ok(out)
                    }
                    Err(_) => Err(rquickjs::Exception::throw_message(
                        &ctx,
                        "InvalidCharacterError: atob: invalid base64 input",
                    )),
                }
            });
            globals.set("atob", atob)?;

            let btoa = Func::from(|ctx: Ctx<'_>, s: String| -> rquickjs::Result<String> {
                // btoa expects a "binary string" — every code point
                // must be in 0..=255. Spec throws InvalidCharacterError
                // for anything outside that range.
                let mut bytes = Vec::with_capacity(s.len());
                for c in s.chars() {
                    let code = c as u32;
                    if code > 0xFF {
                        return Err(rquickjs::Exception::throw_message(
                            &ctx,
                            "InvalidCharacterError: btoa: character > U+00FF",
                        ));
                    }
                    bytes.push(code as u8);
                }
                Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
            });
            globals.set("btoa", btoa)?;

            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install browser apis (Rust): {e}")))?;

    // The rest is pure JS — queueMicrotask via Promise, rAF via
    // setTimeout, matchMedia POJO, in-memory storage. Installed in
    // one bootstrap so the source stays inspectable in one place.
    context
        .with(|ctx| -> rquickjs::Result<()> {
            ctx.eval::<(), _>(BROWSER_APIS_BOOTSTRAP)?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install browser apis (JS): {e}")))?;
    Ok(())
}

/// JS bootstrap for the pure-JS half of [`install_browser_apis`]:
/// `queueMicrotask`, `requestAnimationFrame` / `cancelAnimationFrame`,
/// `matchMedia`, `localStorage`, `sessionStorage`, `heso.flush`, and
/// noop observer ctors (`MutationObserver`, `IntersectionObserver`,
/// `ResizeObserver`, `PerformanceObserver`).
const BROWSER_APIS_BOOTSTRAP: &str = r#"
(function() {
    // queueMicrotask(fn) — schedule `fn` after the current synchronous
    // block but before the next macrotask. Spec semantics are
    // `Promise.resolve().then(fn)`. QuickJS's microtask pump surfaces
    // any throw to `execute_pending_job`, which the engine captures as
    // a console.error.
    if (typeof globalThis.queueMicrotask !== 'function') {
        globalThis.queueMicrotask = function(fn) {
            if (typeof fn !== 'function') {
                throw new TypeError('queueMicrotask: argument is not a function');
            }
            Promise.resolve().then(fn);
        };
    }

    // requestAnimationFrame(cb) / cancelAnimationFrame(id) — route to
    // setTimeout(cb, 16). 16ms ~= 60fps. The id returned IS the
    // setTimeout id, so cancelAnimationFrame just calls clearTimeout.
    // Spec requires the callback to receive a high-res timestamp; we
    // pass performance.now() so animation code that interpolates
    // against the delta sees a sensible-shaped number.
    if (typeof globalThis.requestAnimationFrame !== 'function') {
        globalThis.requestAnimationFrame = function(cb) {
            if (typeof cb !== 'function') {
                throw new TypeError('requestAnimationFrame: argument is not a function');
            }
            return setTimeout(function() { cb(performance.now()); }, 16);
        };
    }
    if (typeof globalThis.cancelAnimationFrame !== 'function') {
        globalThis.cancelAnimationFrame = function(id) {
            clearTimeout(id);
        };
    }

    // matchMedia(query) — return a MediaQueryList-shaped POJO that
    // always reports `matches: false`. No layout → no media queries
    // can match. The listener surface lets framework code subscribe
    // without throwing.
    if (typeof globalThis.matchMedia !== 'function') {
        globalThis.matchMedia = function(query) {
            return {
                matches: false,
                media: String(query == null ? '' : query),
                onchange: null,
                addListener: function() {},      // legacy
                removeListener: function() {},   // legacy
                addEventListener: function() {},
                removeEventListener: function() {},
                dispatchEvent: function() { return false; }
            };
        };
    }

    // localStorage / sessionStorage — in-memory Map per engine. ADR
    // 0014 commits to this shape (in-memory, deterministic, no
    // persistence yet). Closure-private Map keeps JS from poking at
    // the backing store directly.
    function makeStorage() {
        var store = new Map();
        return Object.create(null, {
            length: { get: function() { return store.size; }, enumerable: true },
            getItem: { value: function(k) {
                var key = String(k);
                return store.has(key) ? store.get(key) : null;
            }, enumerable: true },
            setItem: { value: function(k, v) {
                store.set(String(k), String(v));
            }, enumerable: true },
            removeItem: { value: function(k) {
                store.delete(String(k));
            }, enumerable: true },
            clear: { value: function() {
                store.clear();
            }, enumerable: true },
            key: { value: function(i) {
                var idx = Number(i) | 0;
                if (idx < 0 || idx >= store.size) return null;
                var keys = Array.from(store.keys());
                return keys[idx];
            }, enumerable: true }
        });
    }
    if (typeof globalThis.localStorage === 'undefined') {
        globalThis.localStorage = makeStorage();
    }
    if (typeof globalThis.sessionStorage === 'undefined') {
        globalThis.sessionStorage = makeStorage();
    }

    // heso.flush() — yield to the microtask queue. Lets user JS
    // observe DOM mutations queued by earlier `dispatchEvent` calls
    // (e.g. Preact re-renders).
    //
    //   await heso.flush();   // anything queued before this point runs
    //
    // Returning `Promise.resolve()` is enough because the engine's
    // microtask pump runs FIFO and the Rust-side eval awaits the
    // returned Promise via `.then(settle)` — that settle is queued
    // *after* any microtask that ran while the user's `await` was
    // suspended. Deeply-nested microtask chains drain in the same
    // pump (`execute_pending_jobs_until_idle` loops until empty),
    // so a single flush usually suffices.
    if (typeof globalThis.heso !== 'object' || globalThis.heso === null) {
        globalThis.heso = {};
    }
    if (typeof globalThis.heso.flush !== 'function') {
        globalThis.heso.flush = function() {
            return Promise.resolve();
        };
    }

    // MutationObserver / IntersectionObserver / ResizeObserver /
    // PerformanceObserver — noop constructors that match the spec
    // surface so SPA hydration code that does `new MutationObserver(cb)`
    // doesn't ReferenceError before the rest of the page runs. We don't
    // actually observe anything; the callback is retained per spec but
    // never invoked, and `takeRecords()` always returns []. Shape
    // cross-referenced against happy-dom's intersection-observer /
    // resize-observer stubs (MIT, capricorn86/happy-dom).
    //
    // Spec notes:
    // - Each ctor takes `(callback, options?)`. We store `callback` on
    //   the instance (spec doesn't require it be enumerable, so we use
    //   a non-enumerable own property to avoid leaking through
    //   JSON.stringify of the observer).
    // - `observe(target, options)` / `unobserve(target)` / `disconnect()`
    //   return undefined; `takeRecords()` returns [].
    // - PerformanceObserver additionally exposes a static
    //   `supportedEntryTypes` (FrozenArray<DOMString>). We return [] so
    //   code that does `PerformanceObserver.supportedEntryTypes.includes('foo')`
    //   gets `false` instead of throwing.
    function defineNoopObserver(name) {
        if (typeof globalThis[name] !== 'undefined') return;
        function Observer(callback) {
            if (!(this instanceof Observer)) {
                throw new TypeError(
                    "Constructor " + name + " requires 'new'"
                );
            }
            // Per spec, the callback is required for these ctors. Real
            // browsers throw TypeError when it's missing or not callable;
            // we mirror that so feature-detection via try/catch behaves
            // the same.
            if (typeof callback !== 'function') {
                throw new TypeError(
                    name + " constructor: argument 1 is not a function"
                );
            }
            Object.defineProperty(this, '_callback', {
                value: callback,
                writable: false,
                enumerable: false,
                configurable: false
            });
        }
        Observer.prototype.observe = function() {};
        Observer.prototype.unobserve = function() {};
        Observer.prototype.disconnect = function() {};
        Observer.prototype.takeRecords = function() { return []; };
        // Name the function so `obs.constructor.name` and
        // `new MutationObserver(cb).toString()` show the real spec name
        // instead of "Observer". Object.defineProperty since Function's
        // `name` is non-writable but configurable.
        Object.defineProperty(Observer, 'name', { value: name });
        globalThis[name] = Observer;
    }
    defineNoopObserver('MutationObserver');
    defineNoopObserver('IntersectionObserver');
    defineNoopObserver('ResizeObserver');
    defineNoopObserver('PerformanceObserver');

    // PerformanceObserver.supportedEntryTypes — spec-defined static
    // FrozenArray<DOMString>. Empty because we don't actually record
    // any entries; feature-detection (e.g.
    // `PerformanceObserver.supportedEntryTypes.includes('longtask')`)
    // will correctly return false instead of throwing.
    if (typeof globalThis.PerformanceObserver === 'function' &&
        typeof globalThis.PerformanceObserver.supportedEntryTypes === 'undefined') {
        Object.defineProperty(globalThis.PerformanceObserver, 'supportedEntryTypes', {
            value: Object.freeze([]),
            writable: false,
            enumerable: true,
            configurable: false
        });
    }
})();
"#;

fn install_console_method<'js>(
    ctx: &Ctx<'js>,
    console: &Object<'js>,
    name: &str,
    level: ConsoleLevel,
    buffer: Arc<Mutex<Vec<ConsoleEntry>>>,
) -> rquickjs::Result<()> {
    // The closure must satisfy `for<'js> Fn(Rest<Value<'js>>) -> _`.
    // We avoid the two-lifetime-parameters trap by taking only the
    // variadic args and extracting the [`Ctx`] from each [`Value`]
    // (Value carries its parent Ctx, so we don't need a separate
    // Ctx parameter to recover it).
    let fun = Function::new(ctx.clone(), move |args: Rest<Value>| {
        let mut json_args: Vec<serde_json::Value> = Vec::with_capacity(args.len());
        for arg in args.into_inner() {
            let arg_ctx = arg.ctx().clone();
            json_args.push(js_value_to_json(&arg_ctx, arg).unwrap_or(serde_json::Value::Null));
        }
        if let Ok(mut buf) = buffer.lock() {
            buf.push(ConsoleEntry {
                level,
                args: json_args,
            });
        }
    })?;
    console.set(name, fun)?;
    Ok(())
}

/// Convert an arbitrary [`rquickjs::Value`] to [`serde_json::Value`].
///
/// Strategy:
///
/// - Primitives are handled by `JSON.stringify`-style semantics:
///   `null` and `undefined` → [`Null`]; numbers → [`Number`]; strings
///   → [`String`]; booleans → [`Bool`].
/// - Objects and arrays go through QuickJS's own `JSON.stringify` and
///   then [`serde_json::from_str`]. This keeps key ordering identical
///   to what the script saw and handles cycles/non-JSON values the
///   way native JSON does (it errors / produces `null` for those).
/// - Functions and symbols become [`Null`] (same as `JSON.stringify`
///   silently drops them).
fn js_value_to_json<'js>(ctx: &Ctx<'js>, val: Value<'js>) -> Result<serde_json::Value, EvalError> {
    // Fast paths for primitives — avoid the JSON.stringify round-trip
    // when we don't need it.
    if val.is_null() || val.is_undefined() {
        return Ok(serde_json::Value::Null);
    }
    if let Some(b) = val.as_bool() {
        return Ok(serde_json::Value::Bool(b));
    }
    if let Some(i) = val.as_int() {
        return Ok(serde_json::Value::Number(i.into()));
    }
    if let Some(f) = val.as_float() {
        return Ok(serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null));
    }
    if let Some(s) = val.as_string() {
        let s = s
            .to_string()
            .map_err(|e| EvalError::Engine(format!("read JS string: {e}")))?;
        return Ok(serde_json::Value::String(s));
    }

    // Functions and symbols have no JSON representation — match
    // `JSON.stringify` semantics by producing null.
    if val.is_function() || val.is_symbol() {
        return Ok(serde_json::Value::Null);
    }

    // Objects and arrays: hand to JS's own JSON.stringify, then parse.
    let globals = ctx.globals();
    let json_obj: Object = globals
        .get("JSON")
        .map_err(|e| EvalError::Engine(format!("get JSON global: {e}")))?;
    let stringify: Function = json_obj
        .get("stringify")
        .map_err(|e| EvalError::Engine(format!("get JSON.stringify: {e}")))?;
    let stringified: Value = stringify
        .call((val,))
        .map_err(|e| EvalError::Engine(format!("call JSON.stringify: {e}")))?;
    if stringified.is_undefined() {
        // JSON.stringify returns undefined for unsupported types
        // (functions, symbols, undefined). We've already handled
        // those, but defensive fallback.
        return Ok(serde_json::Value::Null);
    }
    let s = stringified
        .as_string()
        .ok_or_else(|| EvalError::Engine("JSON.stringify did not return a string".to_owned()))?
        .to_string()
        .map_err(|e| EvalError::Engine(format!("decode stringified JSON: {e}")))?;
    serde_json::from_str(&s).map_err(|e| EvalError::Engine(format!("parse stringified JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> JsEngine {
        JsEngine::new().expect("engine new")
    }

    #[test]
    fn evaluates_simple_arithmetic() {
        let e = engine();
        let out = e.eval("1 + 2 + 3").expect("eval ok");
        assert_eq!(out.value, serde_json::json!(6));
        assert!(out.console.is_empty());
    }

    #[test]
    fn evaluates_string_concatenation() {
        let e = engine();
        let out = e.eval(r#""hello, " + "world""#).expect("eval ok");
        assert_eq!(out.value, serde_json::json!("hello, world"));
    }

    #[test]
    fn evaluates_object_literal_via_json_stringify() {
        let e = engine();
        let out = e
            .eval(r#"({a: 1, b: "two", c: [3, 4, 5]})"#)
            .expect("eval ok");
        assert_eq!(out.value["a"], 1);
        assert_eq!(out.value["b"], "two");
        assert_eq!(out.value["c"][1], 4);
    }

    #[test]
    fn evaluates_array_literal() {
        let e = engine();
        let out = e
            .eval("[1, 'two', null, true, {nested: 9}]")
            .expect("eval ok");
        assert_eq!(out.value[0], 1);
        assert_eq!(out.value[1], "two");
        assert!(out.value[2].is_null());
        assert_eq!(out.value[3], true);
        assert_eq!(out.value[4]["nested"], 9);
    }

    #[test]
    fn undefined_becomes_json_null() {
        let e = engine();
        let out = e.eval("undefined").expect("eval ok");
        assert!(out.value.is_null());
    }

    #[test]
    fn function_value_becomes_null() {
        // Same semantics as JSON.stringify(fn) -> undefined -> we map
        // to null since the JSON value model has no undefined.
        let e = engine();
        let out = e.eval("(function() { return 1 })").expect("eval ok");
        assert!(out.value.is_null());
    }

    #[test]
    fn captures_console_log() {
        let e = engine();
        let out = e.eval("console.log('hi', 1, true); 42").expect("eval ok");
        assert_eq!(out.value, serde_json::json!(42));
        assert_eq!(out.console.len(), 1);
        assert_eq!(out.console[0].level, ConsoleLevel::Log);
        assert_eq!(out.console[0].args[0], "hi");
        assert_eq!(out.console[0].args[1], 1);
        assert_eq!(out.console[0].args[2], true);
    }

    #[test]
    fn captures_each_console_level_distinctly() {
        let e = engine();
        let out = e
            .eval(
                r#"
                console.log('a');
                console.info('b');
                console.warn('c');
                console.error('d');
                console.debug('e');
                console.trace('f');
                "done"
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!("done"));
        let levels: Vec<ConsoleLevel> = out.console.iter().map(|c| c.level).collect();
        assert_eq!(
            levels,
            vec![
                ConsoleLevel::Log,
                ConsoleLevel::Info,
                ConsoleLevel::Warn,
                ConsoleLevel::Error,
                ConsoleLevel::Debug,
                ConsoleLevel::Trace,
            ]
        );
    }

    #[test]
    fn console_buffer_resets_between_evals() {
        let e = engine();
        let _ = e.eval("console.log('first')").expect("eval ok");
        let out = e.eval("console.log('second'); 0").expect("eval ok");
        assert_eq!(
            out.console.len(),
            1,
            "second eval should not see first eval's logs"
        );
        assert_eq!(out.console[0].args[0], "second");
    }

    #[test]
    fn throw_new_error_returns_exception_variant() {
        let e = engine();
        let err = e
            .eval(r#"throw new Error('boom')"#)
            .expect_err("should throw");
        match err {
            EvalError::Exception { message, .. } => {
                assert_eq!(message, "boom");
            }
            other => panic!("expected Exception variant, got {other:?}"),
        }
    }

    #[test]
    fn throw_non_error_value_returns_thrown_value_variant() {
        let e = engine();
        let err = e
            .eval(r#"throw {custom: true, code: 42}"#)
            .expect_err("should throw");
        match err {
            EvalError::ThrownValue { value } => {
                assert_eq!(value["custom"], true);
                assert_eq!(value["code"], 42);
            }
            other => panic!("expected ThrownValue variant, got {other:?}"),
        }
    }

    #[test]
    fn syntax_error_is_reported() {
        let e = engine();
        // QuickJS reports parse errors as SyntaxError exceptions.
        let err = e.eval("this is not js (((").expect_err("syntax error");
        // Either Exception (SyntaxError) or Engine, depending on
        // how rquickjs surfaces it. Both are acceptable; the
        // important property is that we don't silently succeed.
        assert!(matches!(
            err,
            EvalError::Exception { .. } | EvalError::Engine(_)
        ));
    }

    #[test]
    fn engine_can_be_reused_across_multiple_evals() {
        let e = engine();
        for i in 0..5 {
            let out = e.eval(&format!("{i} + 1")).expect("eval ok");
            assert_eq!(out.value, serde_json::json!(i + 1));
        }
    }

    #[test]
    fn evaluates_modern_es_features() {
        let e = engine();
        // Arrow functions, spread, destructuring, optional chaining,
        // template literals, default args — all QuickJS-native and
        // should Just Work.
        let out = e
            .eval(
                r#"
                const sum = (...xs) => xs.reduce((a, b) => a + b, 0);
                const obj = {a: 1, b: 2, c: 3};
                const {a, ...rest} = obj;
                `total: ${sum(...Object.values(rest)) + (obj?.a ?? 0)}`
                "#,
            )
            .expect("eval ok");
        // rest = {b:2, c:3}; sum = 5; + a (1) = 6
        assert_eq!(out.value, serde_json::json!("total: 6"));
    }

    #[test]
    fn json_stringify_roundtrips_through_value() {
        // The engine itself uses JSON.stringify to convert values to
        // JSON. Verify a value that originated from JSON.parse
        // makes it through unchanged.
        let e = engine();
        let out = e
            .eval(r#"JSON.parse('{"x":1,"y":[2,3],"z":{"w":"abc"}}')"#)
            .expect("eval ok");
        assert_eq!(out.value["x"], 1);
        assert_eq!(out.value["y"][0], 2);
        assert_eq!(out.value["z"]["w"], "abc");
    }

    #[test]
    fn each_engine_is_isolated() {
        let e1 = engine();
        let e2 = engine();
        let _ = e1.eval("globalThis.flagA = 1").expect("eval ok");
        let out = e2.eval("typeof globalThis.flagA").expect("eval ok");
        assert_eq!(
            out.value, "undefined",
            "globals on engine 1 should not leak to engine 2"
        );
    }

    // ===== Phase 1B integration: JS reaches into the agent-shaped DOM =====

    #[test]
    fn js_can_read_document_title_from_html() {
        let html = "<html><head><title>Hello DOM</title></head><body></body></html>";
        let out = engine()
            .eval_with_html(html, "document.title")
            .expect("eval_with_html ok");
        assert_eq!(out.value, serde_json::json!("Hello DOM"));
    }

    #[test]
    fn js_can_query_selector_and_read_text_content() {
        let html = "<html><body><h1>page heading</h1><p>body copy</p></body></html>";
        let out = engine()
            .eval_with_html(html, "document.querySelector('h1').textContent")
            .expect("eval_with_html ok");
        assert_eq!(out.value, serde_json::json!("page heading"));
    }

    #[test]
    fn js_can_query_selector_all_and_iterate() {
        let html = r#"<html><body><ul><li>a</li><li>b</li><li>c</li></ul></body></html>"#;
        let out = engine()
            .eval_with_html(
                html,
                "Array.from(document.querySelectorAll('li')).map(el => el.textContent)",
            )
            .expect("eval_with_html ok");
        assert_eq!(out.value, serde_json::json!(["a", "b", "c"]));
    }

    #[test]
    fn js_can_read_attributes_via_get_attribute() {
        let html = r#"<html><body><a href="https://example.com" class="cta">go</a></body></html>"#;
        let out = engine()
            .eval_with_html(
                html,
                "[document.querySelector('a').getAttribute('href'), document.querySelector('a').getAttribute('class')]",
            )
            .expect("eval_with_html ok");
        assert_eq!(out.value, serde_json::json!(["https://example.com", "cta"]));
    }

    #[test]
    fn js_get_element_by_id_finds_element() {
        let html = r#"<html><body><div id="main"><p>inside</p></div></body></html>"#;
        let out = engine()
            .eval_with_html(html, "document.getElementById('main').textContent")
            .expect("eval_with_html ok");
        assert_eq!(out.value, serde_json::json!("inside"));
    }

    #[test]
    fn js_can_access_tag_name_uppercase() {
        let html = "<html><body><section>x</section></body></html>";
        let out = engine()
            .eval_with_html(html, "document.querySelector('section').tagName")
            .expect("eval_with_html ok");
        assert_eq!(out.value, serde_json::json!("SECTION"));
    }

    #[test]
    fn js_query_selector_returns_null_when_no_match() {
        let html = "<html><body><p>only</p></body></html>";
        let out = engine()
            .eval_with_html(html, "document.querySelector('nav')")
            .expect("eval_with_html ok");
        // `null` in JS → serde_json::Value::Null
        assert!(out.value.is_null());
    }

    #[test]
    fn js_can_chain_element_query_selector() {
        let html = r#"<html><body><article><h2>title</h2><p>body</p></article></body></html>"#;
        let out = engine()
            .eval_with_html(
                html,
                "document.querySelector('article').querySelector('h2').textContent",
            )
            .expect("eval_with_html ok");
        assert_eq!(out.value, serde_json::json!("title"));
    }

    #[test]
    fn js_console_log_works_alongside_dom_access() {
        let html = "<html><body><h1>greet</h1></body></html>";
        let out = engine()
            .eval_with_html(
                html,
                "console.log('found:', document.querySelector('h1').textContent); 'done'",
            )
            .expect("eval_with_html ok");
        assert_eq!(out.value, serde_json::json!("done"));
        assert_eq!(out.console.len(), 1);
        assert_eq!(out.console[0].args[0], "found:");
        assert_eq!(out.console[0].args[1], "greet");
    }

    #[test]
    fn js_can_read_inner_html() {
        let html = r#"<html><body><div class="x"><span>hi</span></div></body></html>"#;
        let out = engine()
            .eval_with_html(html, "document.querySelector('.x').innerHTML")
            .expect("eval_with_html ok");
        let s = out.value.as_str().expect("value should be a string");
        assert!(s.contains("<span>hi</span>"), "got: {s:?}");
    }

    #[test]
    fn js_can_read_outer_html() {
        let html = r#"<html><body><div class="x"><span>hi</span></div></body></html>"#;
        let out = engine()
            .eval_with_html(html, "document.querySelector('.x').outerHTML")
            .expect("eval_with_html ok");
        let s = out.value.as_str().expect("value should be a string");
        assert!(s.contains(r#"<div class="x">"#), "got: {s:?}");
    }

    // ===== Mutation surface integration tests =====

    #[test]
    fn js_can_set_attribute_and_read_it_back() {
        let html = r#"<html><body><a href="/old">go</a></body></html>"#;
        let out = engine()
            .eval_with_html(
                html,
                r#"
                const a = document.querySelector('a');
                a.setAttribute('href', '/new');
                a.setAttribute('data-source', 'agent');
                [a.getAttribute('href'), a.getAttribute('data-source')]
                "#,
            )
            .expect("eval_with_html ok");
        assert_eq!(out.value, serde_json::json!(["/new", "agent"]));
    }

    #[test]
    fn js_inner_html_setter_replaces_children() {
        let html = "<html><body><div id=\"target\"><p>old</p></div></body></html>";
        let out = engine()
            .eval_with_html(
                html,
                r#"
                const target = document.getElementById('target');
                target.innerHTML = '<span class="new">freshly parsed</span>';
                target.querySelector('.new').textContent
                "#,
            )
            .expect("eval_with_html ok");
        assert_eq!(out.value, serde_json::json!("freshly parsed"));
    }

    #[test]
    fn js_class_list_add_remove_toggle_contains_round_trip() {
        let html = r#"<html><body><div class="a">x</div></body></html>"#;
        let out = engine()
            .eval_with_html(
                html,
                r#"
                const d = document.querySelector('div');
                d.classList.add('b');
                d.classList.add('c');
                d.classList.remove('a');
                const toggled = d.classList.toggle('highlight');  // adds → true
                const hasB = d.classList.contains('b');
                const hasA = d.classList.contains('a');
                [d.className, toggled, hasB, hasA]
                "#,
            )
            .expect("eval_with_html ok");
        // Order of tokens reflects insertion order; "a" was removed.
        assert_eq!(out.value[1], true);
        assert_eq!(out.value[2], true);
        assert_eq!(out.value[3], false);
        let class = out.value[0].as_str().expect("className is string");
        for token in ["b", "c", "highlight"] {
            assert!(
                class.split_ascii_whitespace().any(|t| t == token),
                "expected token {token} in {class:?}"
            );
        }
        assert!(
            !class.split_ascii_whitespace().any(|t| t == "a"),
            "did not expect 'a' in {class:?}"
        );
    }

    #[test]
    fn js_append_child_reparents() {
        let html = "<html><body><div id=\"src\"><p id=\"item\">x</p></div><div id=\"dst\"></div></body></html>";
        let out = engine()
            .eval_with_html(
                html,
                r#"
                const src = document.getElementById('src');
                const dst = document.getElementById('dst');
                const item = document.getElementById('item');
                dst.appendChild(item);
                [src.children.length, dst.children.length, dst.children[0].id]
                "#,
            )
            .expect("eval_with_html ok");
        assert_eq!(out.value, serde_json::json!([0, 1, "item"]));
    }

    // ===== Timer integration (Phase 2 — virtual clock + setTimeout) =====

    #[test]
    fn engine_advance_clock_fires_three_timers_into_console_in_order() {
        // Schedule three timers from JS, advance the virtual clock
        // from Rust, observe their messages appear on the engine's
        // console buffer in the right order.
        let e = engine();
        let _ = e
            .eval(
                r#"
                setTimeout(() => console.log('third'), 30);
                setTimeout(() => console.log('first'), 10);
                setTimeout(() => console.log('second'), 20);
                "#,
            )
            .expect("schedule ok");
        // Nothing fired yet — the eval above didn't advance the clock.
        assert_eq!(e.pending_timers(), 3);

        let console_after = e.advance_clock_capture(100).expect("advance ok");
        let msgs: Vec<&str> = console_after
            .iter()
            .filter(|c| c.level == ConsoleLevel::Log)
            .filter_map(|c| c.args.first().and_then(|v| v.as_str()))
            .collect();
        assert_eq!(msgs, vec!["first", "second", "third"]);
        assert_eq!(e.pending_timers(), 0);
    }

    #[test]
    fn engine_advance_clock_in_steps_fires_partial_then_remaining() {
        // Verify the virtual clock is *cumulative* across multiple
        // `advance_clock` calls: a timer at 250ms fires after
        // advance(100) + advance(150), not before.
        let e = engine();
        let _ = e
            .eval(
                r#"
                setTimeout(() => console.log('early'), 50);
                setTimeout(() => console.log('late'), 250);
                "#,
            )
            .expect("schedule ok");

        // Advance to virtual time 100. Only the 50ms timer fires.
        e.advance_clock(100).expect("advance 1 ok");
        let first = e.drain_console();
        let first_msgs: Vec<&str> = first
            .iter()
            .filter_map(|c| c.args.first().and_then(|v| v.as_str()))
            .collect();
        assert_eq!(first_msgs, vec!["early"]);
        assert_eq!(e.pending_timers(), 1);

        // Advance another 150 (cumulative virtual time = 250). The
        // remaining timer fires.
        e.advance_clock(150).expect("advance 2 ok");
        let second = e.drain_console();
        let second_msgs: Vec<&str> = second
            .iter()
            .filter_map(|c| c.args.first().and_then(|v| v.as_str()))
            .collect();
        assert_eq!(second_msgs, vec!["late"]);
        assert_eq!(e.pending_timers(), 0);
    }

    #[test]
    fn engine_set_interval_from_js_fires_correct_count_after_advance() {
        // Schedule an interval, advance, observe the count.
        let e = engine();
        let _ = e
            .eval(
                r#"
                globalThis.count = 0;
                setInterval(() => {
                    globalThis.count += 1;
                    console.log('tick ' + globalThis.count);
                }, 30);
                "#,
            )
            .expect("schedule ok");
        e.advance_clock(100).expect("advance ok");

        // Drain BEFORE the next `eval`, because [`Self::eval`] resets
        // the console buffer at the start of each call.
        let drained = e.drain_console();
        let ticks: Vec<&str> = drained
            .iter()
            .filter_map(|c| c.args.first().and_then(|v| v.as_str()))
            .collect();
        assert_eq!(ticks, vec!["tick 1", "tick 2", "tick 3"]);

        // Fires at 30, 60, 90 — count should be 3.
        let count = e.eval("globalThis.count").expect("eval ok");
        assert_eq!(count.value, serde_json::json!(3));
    }

    #[test]
    fn engine_clear_timeout_from_js_prevents_advance_from_firing() {
        // JS schedules a timer and then clears it; advance_clock
        // observes no fire.
        let e = engine();
        let _ = e
            .eval(
                r#"
                const id = setTimeout(() => console.log('should not fire'), 50);
                clearTimeout(id);
                "#,
            )
            .expect("schedule+clear ok");
        assert_eq!(e.pending_timers(), 0);

        e.advance_clock(1000).expect("advance ok");
        let drained = e.drain_console();
        let logs: Vec<&ConsoleEntry> = drained
            .iter()
            .filter(|c| c.level == ConsoleLevel::Log)
            .collect();
        assert_eq!(logs.len(), 0, "no logs expected after clear");
    }

    #[test]
    fn engine_advance_clock_with_zero_delta_fires_zero_delay_timer() {
        // Engine-level equivalent of the timers::tests version, this
        // time verifying the public surface produces a real
        // console-side observation.
        let e = engine();
        let _ = e
            .eval("setTimeout(() => console.log('immediate'), 0)")
            .expect("schedule ok");
        e.advance_clock(0).expect("advance ok");
        let drained = e.drain_console();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].level, ConsoleLevel::Log);
        assert_eq!(drained[0].args[0], "immediate");
    }

    #[test]
    fn engine_throwing_timer_writes_console_error_and_pump_keeps_going() {
        // Critical determinism property (ADR 0008): a throwing
        // callback must not stop subsequent timers from firing.
        // Validated at the engine surface using `advance_clock`.
        let e = engine();
        let _ = e
            .eval(
                r#"
                setTimeout(() => console.log('A'), 10);
                setTimeout(() => { throw new Error('mid-throw'); }, 20);
                setTimeout(() => console.log('C'), 30);
                "#,
            )
            .expect("schedule ok");

        e.advance_clock(100).expect("advance ok");

        let drained = e.drain_console();
        // We should see exactly: log 'A', error 'mid-throw', log 'C'.
        let log_msgs: Vec<&str> = drained
            .iter()
            .filter(|c| c.level == ConsoleLevel::Log)
            .filter_map(|c| c.args.first().and_then(|v| v.as_str()))
            .collect();
        assert_eq!(log_msgs, vec!["A", "C"]);

        let errors: Vec<&ConsoleEntry> = drained
            .iter()
            .filter(|c| c.level == ConsoleLevel::Error)
            .collect();
        assert_eq!(errors.len(), 1);
        let err_msg = errors[0].args[0].as_str().expect("err arg is string");
        assert!(err_msg.contains("mid-throw"), "got: {err_msg:?}");
    }

    // ===== Phase 1B event-model integration tests =====
    //
    // These exercise the global classes installed by
    // `crate::events::install_events` end-to-end from JavaScript:
    // create an EventTarget, wire a listener, dispatch, and observe
    // the side effect via console capture or the dispatch return.

    #[test]
    fn js_event_target_dispatch_runs_listener_and_console_observes() {
        let out = engine()
            .eval(
                r#"
                const t = new EventTarget();
                t.addEventListener('demo', (ev) => {
                    console.log('saw', ev.type);
                });
                const r = t.dispatchEvent(new Event('demo'));
                r
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value, true);
        assert_eq!(out.console.len(), 1);
        assert_eq!(out.console[0].args[0], "saw");
        assert_eq!(out.console[0].args[1], "demo");
    }

    #[test]
    fn js_custom_event_detail_is_visible_to_listener() {
        // A listener attached via addEventListener should receive a
        // CustomEvent whose `detail` carries through the dispatch
        // intact.
        let out = engine()
            .eval(
                r#"
                const t = new EventTarget();
                let saw = null;
                t.addEventListener('payload', (ev) => { saw = ev.detail; });
                t.dispatchEvent(new CustomEvent('payload', {detail: {id: 7, name: 'alice'}}));
                saw
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value["id"], 7);
        assert_eq!(out.value["name"], "alice");
    }

    #[test]
    fn js_abort_controller_signals_listener_and_flips_state() {
        // Create an AbortController, subscribe to "abort" on its
        // signal, abort, and verify both that the listener fires and
        // that the signal's state reflects the abort.
        let out = engine()
            .eval(
                r#"
                const c = new AbortController();
                let count = 0;
                let reasonSeen = null;
                c.signal.addEventListener('abort', () => {
                    count += 1;
                    reasonSeen = c.signal.reason;
                });
                const before = c.signal.aborted;
                c.abort('shutdown');
                // Calling abort() twice should be idempotent.
                c.abort('ignored');
                [before, c.signal.aborted, count, reasonSeen]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], false);
        assert_eq!(out.value[1], true);
        // Listener should have fired exactly once even though we
        // called abort twice.
        assert_eq!(out.value[2], 1);
        assert_eq!(out.value[3], "shutdown");
    }

    #[test]
    fn js_prevent_default_propagates_back_to_caller_via_dispatch_return() {
        // dispatchEvent should return false iff a listener called
        // preventDefault on a cancelable event. We observe both
        // outcomes within the same engine to confirm the contract.
        let out = engine()
            .eval(
                r#"
                const t = new EventTarget();
                t.addEventListener('cancelable', (ev) => { ev.preventDefault(); });
                t.addEventListener('plain', () => { /* no preventDefault */ });
                const a = t.dispatchEvent(new Event('cancelable', {cancelable: true}));
                const b = t.dispatchEvent(new Event('plain'));
                [a, b]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], false);
        assert_eq!(out.value[1], true);
    }

    #[test]
    fn js_dom_exception_round_trips_from_js() {
        // DOMException should be reachable from JS as a constructor,
        // with name → code mapping working end-to-end through the
        // engine. This shores up the engine-wiring path even though
        // events.rs has its own unit tests for the table.
        let out = engine()
            .eval(
                r#"
                const e = new DOMException('not here', 'NotFoundError');
                [e.message, e.name, e.code, e.toString()]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], "not here");
        assert_eq!(out.value[1], "NotFoundError");
        assert_eq!(out.value[2], 8);
        assert_eq!(out.value[3], "DOMException: not here");
    }

    // ===== Phase 2 determinism: seeded Math.random / crypto =====
    //
    // ADR 0008: same seed + same script must produce byte-identical
    // observable output. These four tests assert the JS surface honors
    // the contract end-to-end, with two fresh engines per pair so we
    // cover construction-time wiring (not just intra-engine repeat
    // calls, which would just re-prove the RNG itself is deterministic).

    #[test]
    fn seeded_math_random_same_seed_same_sequence() {
        let e1 = JsEngine::new_with_seed(42).expect("engine seed 42");
        let e2 = JsEngine::new_with_seed(42).expect("engine seed 42");
        let a = e1
            .eval("Array.from({length: 5}, Math.random)")
            .expect("eval ok");
        let b = e2
            .eval("Array.from({length: 5}, Math.random)")
            .expect("eval ok");
        assert_eq!(
            a.value, b.value,
            "two fresh engines with seed=42 must yield identical Math.random sequences"
        );
        // And the values are real numbers in the contract range.
        let arr = a.value.as_array().expect("value is array");
        assert_eq!(arr.len(), 5);
        for v in arr {
            let n = v.as_f64().expect("array element is a number");
            assert!(
                (0.0..1.0).contains(&n),
                "Math.random should yield [0,1): got {n}"
            );
        }
    }

    #[test]
    fn seeded_math_random_different_seed_different_sequence() {
        let e1 = JsEngine::new_with_seed(1).expect("engine seed 1");
        let e2 = JsEngine::new_with_seed(2).expect("engine seed 2");
        let a = e1
            .eval("Array.from({length: 5}, Math.random)")
            .expect("eval ok");
        let b = e2
            .eval("Array.from({length: 5}, Math.random)")
            .expect("eval ok");
        assert_ne!(
            a.value, b.value,
            "different seeds should produce different Math.random sequences"
        );
    }

    #[test]
    fn seeded_crypto_random_uuid_same_seed_same_string() {
        let e1 = JsEngine::new_with_seed(123).expect("engine seed 123");
        let e2 = JsEngine::new_with_seed(123).expect("engine seed 123");
        let a = e1.eval("crypto.randomUUID()").expect("eval ok");
        let b = e2.eval("crypto.randomUUID()").expect("eval ok");
        assert_eq!(a.value, b.value, "same seed → same randomUUID");
        // Sanity-check v4 shape on the value we got back.
        let s = a.value.as_str().expect("randomUUID returns a string");
        assert_eq!(s.len(), 36, "UUID len; got {s:?}");
        assert_eq!(&s[14..15], "4", "version nibble = 4 in {s:?}");
        let variant = &s[19..20];
        assert!(
            matches!(variant, "8" | "9" | "a" | "b"),
            "variant nibble in {{8,9,a,b}} in {s:?}"
        );
    }

    #[test]
    fn seeded_crypto_get_random_values_same_seed_same_bytes() {
        let e1 = JsEngine::new_with_seed(99).expect("engine seed 99");
        let e2 = JsEngine::new_with_seed(99).expect("engine seed 99");
        // Allocate a fresh Uint8Array(16), fill via getRandomValues,
        // dump as a plain array of numbers so two engines' outputs
        // can be compared as JSON values.
        let js = r#"
            const buf = new Uint8Array(16);
            crypto.getRandomValues(buf);
            Array.from(buf)
        "#;
        let a = e1.eval(js).expect("eval ok");
        let b = e2.eval(js).expect("eval ok");
        assert_eq!(
            a.value, b.value,
            "same seed → identical getRandomValues output"
        );
        // 16 bytes of u8 → 16 numeric entries, each in 0..=255.
        let arr = a.value.as_array().expect("value is array");
        assert_eq!(arr.len(), 16);
        for v in arr {
            let n = v.as_u64().expect("byte is a non-negative integer");
            assert!(n <= 255, "byte out of range: {n}");
        }
        // Sanity: a different seed produces different bytes.
        let e3 = JsEngine::new_with_seed(100).expect("engine seed 100");
        let c = e3.eval(js).expect("eval ok");
        assert_ne!(
            a.value, c.value,
            "different seed should produce different bytes"
        );
    }

    // ===== Phase 1C script-on-load integration tests =====
    //
    // These pin the load-bearing behavior of the script pump:
    // inline scripts in document order, error containment, type-attr
    // classification (data blocks skipped, JS MIMEs honored), external
    // src= policy gating, and the user-eval-sees-post-hydration
    // invariant.

    #[test]
    fn inline_script_runs_before_user_js_and_sets_document_title() {
        let html = r#"<html><head><script>document.title = "set by script"</script></head><body></body></html>"#;
        let out = engine()
            .eval_with_html(html, "document.title")
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!("set by script"));
    }

    #[test]
    fn two_inline_scripts_run_in_document_order() {
        // script 1 sets window.x = 1; script 2 reads window.x and
        // sets window.y. If document-order is broken, window.y will
        // be NaN/undefined and the assertion fails.
        let html = r#"<html><head>
            <script>globalThis.x = 1;</script>
            <script>globalThis.y = globalThis.x + 1;</script>
        </head><body></body></html>"#;
        let out = engine()
            .eval_with_html(html, "globalThis.y")
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!(2));
    }

    #[test]
    fn syntax_error_in_one_script_does_not_prevent_next_script_from_running() {
        // Critical determinism property: one bad script doesn't poison
        // the rest of the page.
        let html = r#"<html><head>
            <script>globalThis.before = 'ok';</script>
            <script>this is not valid javascript (((</script>
            <script>globalThis.after = 'ok';</script>
        </head><body></body></html>"#;
        let out = engine()
            .eval_with_html(html, "[globalThis.before, globalThis.after]")
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!(["ok", "ok"]));
    }

    #[test]
    fn throwing_script_does_not_prevent_next_script_from_running() {
        // Same as the syntax-error case but a runtime throw rather
        // than a parse failure. jsdom and happy-dom both keep going.
        let html = r#"<html><head>
            <script>globalThis.a = 1;</script>
            <script>throw new Error('boom');</script>
            <script>globalThis.b = 2;</script>
        </head><body></body></html>"#;
        let out = engine()
            .eval_with_html(html, "[globalThis.a, globalThis.b]")
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!([1, 2]));
    }

    #[test]
    fn external_script_src_is_skipped_with_console_warn_under_default_policy() {
        // External src= must NOT trigger a network fetch under the
        // default ScriptFetchPolicy::Skip; a console.warn entry
        // identifies what was skipped.
        let html = r#"<html><head><script src="https://example.com/app.js"></script></head><body></body></html>"#;
        let e = engine();
        let _ = e.eval_with_html(html, "'done'").expect("eval ok");
        // User-facing eval doesn't see the warn — buffer was cleared
        // before the user's `js` ran (per the documented contract).
        // Use eval_with_html_capture to see the warn + count.
        let (out, script_outcome) = e
            .eval_with_html_capture(html, "", ScriptFetchPolicy::Skip)
            .expect("eval ok");
        // Empty user-js path: the buffer survives (we cleared once,
        // then ran one script, then ran `""`). Verify both pieces.
        assert_eq!(script_outcome.external_handled, 1);
        assert_eq!(script_outcome.executed, 0);
        // The warn entry from the script pump remains on the buffer
        // because `js=""` is a no-op that doesn't push anything.
        assert!(
            out.console
                .iter()
                .any(|c| matches!(c.level, ConsoleLevel::Warn)
                    && c.args
                        .first()
                        .and_then(|v| v.as_str())
                        .map(|s| s.contains("example.com/app.js"))
                        .unwrap_or(false)),
            "expected a warn naming app.js, got: {:?}",
            out.console
        );
    }

    #[test]
    fn external_script_src_under_error_policy_emits_console_error() {
        let html = r#"<html><head><script src="/bundle.js"></script></head><body></body></html>"#;
        let (out, script_outcome) = engine()
            .eval_with_html_capture(html, "", ScriptFetchPolicy::Error)
            .expect("eval ok");
        assert_eq!(script_outcome.external_handled, 1);
        assert!(
            out.console
                .iter()
                .any(|c| matches!(c.level, ConsoleLevel::Error)
                    && c.args
                        .first()
                        .and_then(|v| v.as_str())
                        .map(|s| s.contains("bundle.js"))
                        .unwrap_or(false)),
            "expected an error naming bundle.js, got: {:?}",
            out.console
        );
    }

    #[test]
    fn script_can_mutate_dom_and_user_js_sees_post_mutation_state() {
        let html = r#"<html><body>
            <div id="target">original</div>
            <script>
                document.getElementById('target').textContent = 'hydrated';
                document.getElementById('target').setAttribute('data-state', 'ready');
            </script>
        </body></html>"#;
        let out = engine()
            .eval_with_html(
                html,
                r#"
                const el = document.getElementById('target');
                [el.textContent, el.getAttribute('data-state')]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!(["hydrated", "ready"]));
    }

    #[test]
    fn script_can_query_selector_and_append_new_element() {
        let html = r#"<html><body>
            <ul id="list"><li>a</li></ul>
            <script>
                const li = document.getElementById('list').querySelector('li');
                li.setAttribute('data-marked', '1');
                document.getElementById('list').innerHTML += '<li>b</li>';
            </script>
        </body></html>"#;
        let out = engine()
            .eval_with_html(
                html,
                r#"
                const items = Array.from(document.querySelectorAll('#list li'))
                  .map(el => el.textContent);
                [items, document.querySelector('#list li').getAttribute('data-marked')]
                "#,
            )
            .expect("eval ok");
        // First item carries the mutation; second appended via innerHTML +=
        assert_eq!(out.value[0][0], "a");
        assert!(out.value[0].as_array().expect("array").len() >= 2);
        assert_eq!(out.value[1], "1");
    }

    #[test]
    fn data_block_script_type_is_skipped_not_executed() {
        // <script type="application/ld+json"> is structured data, not
        // code. We must NOT eval its contents (which would be a
        // SyntaxError because JSON object literals at statement
        // position parse as labelled statements).
        let html = r#"<html><head>
            <script type="application/ld+json">{"@type":"Article","headline":"x"}</script>
            <script>globalThis.ran = true;</script>
        </head><body></body></html>"#;
        let (out, script_outcome) = engine()
            .eval_with_html_capture(html, "globalThis.ran", ScriptFetchPolicy::default())
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!(true));
        // The JSON data block was not executed; the JS script was.
        assert_eq!(script_outcome.executed, 1);
        assert_eq!(script_outcome.executed_with_error, 0);
        assert_eq!(script_outcome.skipped_non_script_type, 1);
    }

    #[test]
    fn explicit_text_javascript_type_attr_runs_as_classic_script() {
        let html = r#"<html><head>
            <script type="text/javascript">globalThis.flag = 7;</script>
        </head><body></body></html>"#;
        let out = engine()
            .eval_with_html(html, "globalThis.flag")
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!(7));
    }

    #[test]
    fn module_type_attr_runs_as_classic_for_now_phase_1c_punt() {
        // We do NOT implement real ES module loading; we just treat
        // type="module" as classic so the body of a simple module
        // still gets a chance to populate the DOM. This documents
        // the punt so a follow-up agent doesn't break the test
        // without realizing it.
        let html = r#"<html><head>
            <script type="module">globalThis.moduleRan = 'yes';</script>
        </head><body></body></html>"#;
        let out = engine()
            .eval_with_html(html, "globalThis.moduleRan")
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!("yes"));
    }

    #[test]
    fn eval_with_html_capture_returns_script_outcome_counts() {
        let html = r#"<html><head>
            <script>globalThis.ok1 = true;</script>
            <script type="application/json">{"x":1}</script>
            <script src="/missing.js"></script>
            <script>throw new Error('intentional');</script>
            <script>globalThis.ok2 = true;</script>
        </head><body></body></html>"#;
        let (out, script_outcome) = engine()
            .eval_with_html_capture(
                html,
                "[globalThis.ok1, globalThis.ok2]",
                ScriptFetchPolicy::Skip,
            )
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!([true, true]));
        assert_eq!(script_outcome.executed, 2);
        assert_eq!(script_outcome.executed_with_error, 1);
        assert_eq!(script_outcome.external_handled, 1);
        assert_eq!(script_outcome.skipped_non_script_type, 1);
    }

    // ===== Date virtualization (ADR 0008 determinism shim) =====
    //
    // The contract: `Date.now()` and zero-arg `new Date()` route
    // through the engine's VirtualClock, while every explicit-input
    // form (`new Date(ms)`, `new Date(str)`, `new Date(y, m, d, ...)`,
    // `Date.parse`, `Date.UTC`) stays pure-of-input on the QuickJS
    // built-in.

    #[test]
    fn date_now_starts_at_zero_on_fresh_engine() {
        let e = engine();
        let out = e.eval("Date.now()").expect("eval ok");
        assert_eq!(out.value, serde_json::json!(0));
    }

    #[test]
    fn date_now_advances_by_exactly_advance_clock_delta() {
        let e = engine();
        e.advance_clock(1234).expect("advance ok");
        let out = e.eval("Date.now()").expect("eval ok");
        assert_eq!(out.value, serde_json::json!(1234));
        e.advance_clock(766).expect("advance ok");
        let out = e.eval("Date.now()").expect("eval ok");
        assert_eq!(out.value, serde_json::json!(2000));
    }

    #[test]
    fn date_now_is_byte_identical_across_engines_with_same_advance_sequence() {
        // Two fresh engines, same advance sequence → byte-identical
        // Date.now() readings at every step.
        fn run() -> Vec<serde_json::Value> {
            let e = engine();
            let mut out = Vec::new();
            for delta in [0u64, 10, 25, 100, 50] {
                e.advance_clock(delta).expect("advance ok");
                out.push(e.eval("Date.now()").expect("eval ok").value);
            }
            out
        }
        assert_eq!(run(), run());
    }

    #[test]
    fn zero_arg_new_date_matches_date_now() {
        let e = engine();
        e.advance_clock(500_000).expect("advance ok");
        let out = e
            .eval("[new Date().getTime(), Date.now()]")
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!([500_000, 500_000]));
    }

    #[test]
    fn explicit_input_date_forms_are_untouched() {
        // The whole point of the wrapper is that explicit-input forms
        // remain pure of input. Advance the clock to a nonzero virtual
        // time first to prove the explicit constructors don't pick it
        // up.
        let e = engine();
        e.advance_clock(9_999_999).expect("advance ok");

        // new Date(ms). Large integers come back as JSON floats —
        // compare the f64 value, not the JSON variant.
        let out = e
            .eval("new Date(1234567890000).getTime()")
            .expect("eval ok");
        assert_eq!(out.value.as_f64(), Some(1234567890000.0));

        // Date.parse(str) is pure of input.
        // 2024-01-01T00:00:00Z = 1704067200000 ms since epoch.
        let out = e
            .eval("Date.parse('2024-01-01T00:00:00Z')")
            .expect("eval ok");
        assert_eq!(out.value.as_f64(), Some(1704067200000.0));

        // Date.UTC(...) is pure of input.
        let out = e.eval("Date.UTC(2024, 0, 1, 0, 0, 0)").expect("eval ok");
        assert_eq!(out.value.as_f64(), Some(1704067200000.0));

        // new Date(y, m, d, ...) — month is 0-indexed; we use UTC
        // accessors to avoid timezone variance in test environments.
        let out = e
            .eval("new Date(Date.UTC(2024, 0, 1)).getUTCFullYear()")
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!(2024));
    }

    #[test]
    fn date_instanceof_still_works_for_both_construction_paths() {
        // Both the zero-arg wrapper path and the explicit-input
        // passthrough path must produce instances that pass
        // `instanceof Date`.
        let e = engine();
        let out = e
            .eval(
                r#"[
                    new Date() instanceof Date,
                    new Date(0) instanceof Date,
                    new Date(2024, 0, 1) instanceof Date,
                ]"#,
            )
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!([true, true, true]));
    }

    #[test]
    fn date_now_is_a_function_on_the_global_date() {
        // Regression guard: the wrapper must carry `Date.now` across so
        // libraries that read `Date.now` directly (not through `new
        // Date()`) get the virtual clock.
        let e = engine();
        e.advance_clock(42).expect("advance ok");
        let out = e
            .eval("[typeof Date.now, Date.now()]")
            .expect("eval ok");
        assert_eq!(out.value, serde_json::json!(["function", 42]));
    }
}
