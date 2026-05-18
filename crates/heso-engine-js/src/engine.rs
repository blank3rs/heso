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
    prelude::Rest, CatchResultExt, CaughtError, Class, Context, Ctx, Function, Object, Runtime,
    Value,
};

use crate::dom::{self, Document};
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
}

impl JsEngine {
    /// Create a fresh engine with conservative resource limits
    /// ([`DEFAULT_MEMORY_LIMIT_BYTES`], [`DEFAULT_MAX_STACK_BYTES`]).
    ///
    /// `console.log` / `info` / `warn` / `error` / `debug` / `trace`
    /// are installed as global functions that route into an
    /// in-process buffer instead of stdout, so receipts stay clean.
    pub fn new() -> Result<Self, EvalError> {
        let runtime = Runtime::new().map_err(|e| EvalError::Engine(e.to_string()))?;
        runtime.set_memory_limit(DEFAULT_MEMORY_LIMIT_BYTES);
        runtime.set_max_stack_size(DEFAULT_MAX_STACK_BYTES);

        let context = Context::full(&runtime).map_err(|e| EvalError::Engine(e.to_string()))?;
        let console_buffer: Arc<Mutex<Vec<ConsoleEntry>>> = Arc::new(Mutex::new(Vec::new()));

        install_console(&context, console_buffer.clone())?;
        install_dom_classes(&context)?;
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

        Ok(Self {
            _runtime: runtime,
            context,
            console_buffer,
            timers,
        })
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

    /// Drain and return the console buffer. Useful between calls to
    /// [`Self::advance_clock`] to observe what timer callbacks
    /// logged (or threw) since the last drain.
    pub fn drain_console(&self) -> Vec<ConsoleEntry> {
        let mut buf = self.console_buffer.lock().expect("console buffer poisoned");
        let out = buf.clone();
        buf.clear();
        out
    }

    /// Evaluate `js` against a parsed HTML page.
    ///
    /// Parses `html` into a [`dom_query::Document`], wraps it in an
    /// [`Arc`], constructs a [`Document`] instance, installs it as
    /// the `document` global, and then runs [`Self::eval`]. JS can
    /// call the full Phase 1B DOM — `document.querySelector`,
    /// `element.textContent`, `element.getAttribute`,
    /// `element.setAttribute`, `element.innerHTML = ...`,
    /// `element.classList.add(...)`, `element.appendChild(...)`, and
    /// the rest.
    ///
    /// Errors propagate the same way as [`Self::eval`].
    pub fn eval_with_html(&self, html: &str, js: &str) -> Result<EvalOutcome, EvalError> {
        let document = Document::from_html(html);
        self.context
            .with(|ctx| -> rquickjs::Result<()> {
                let doc = Class::instance(ctx.clone(), document)?;
                ctx.globals().set("document", doc)?;
                Ok(())
            })
            .map_err(|e| EvalError::Engine(format!("install document global: {e}")))?;
        self.eval(js)
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

        let value = self.context.with(|ctx| -> Result<serde_json::Value, EvalError> {
            match ctx.eval::<Value, _>(code).catch(&ctx) {
                Ok(v) => js_value_to_json(&ctx, v),
                Err(CaughtError::Exception(exc)) => Err(EvalError::Exception {
                    message: exc.message().unwrap_or_default(),
                    stack: exc.stack(),
                }),
                Err(CaughtError::Value(v)) => {
                    let repr = js_value_to_json(&ctx, v).unwrap_or(serde_json::Value::Null);
                    Err(EvalError::ThrownValue { value: repr })
                }
                Err(CaughtError::Error(e)) => Err(EvalError::Engine(e.to_string())),
            }
        })?;

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
        Self::new().expect("rquickjs Runtime + Context construction should never fail on default config")
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
        self.context.with(|_ctx| {
            if let Ok(mut s) = timers.lock() {
                s.clear_all();
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
            json_args.push(
                js_value_to_json(&arg_ctx, arg).unwrap_or(serde_json::Value::Null),
            );
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
        let out = e.eval("[1, 'two', null, true, {nested: 9}]").expect("eval ok");
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
        assert_eq!(out.console.len(), 1, "second eval should not see first eval's logs");
        assert_eq!(out.console[0].args[0], "second");
    }

    #[test]
    fn throw_new_error_returns_exception_variant() {
        let e = engine();
        let err = e.eval(r#"throw new Error('boom')"#).expect_err("should throw");
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
}
