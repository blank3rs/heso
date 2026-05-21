//! # scripts
//!
//! `<script>` tag execution on page load — the SPA-hydration unlock per
//! [`next-phase-plan.md`][plan] item A. When [`JsEngine::eval_with_html`]
//! takes a parsed page, this module walks every `<script>` element in
//! the parsed [`dom_query::Document`] in document order and executes its
//! source against the shared QuickJS context **before** the user's
//! evaluation runs. Net effect: by the time the user's `js` argument
//! reads `document.title` or `globalThis.x`, every inline script the
//! page would have run in a real browser has already run.
//!
//! ## What this module is and is not
//!
//! - **It is** the engine-side glue that closes the loop between the
//!   parsed DOM (already in tree from [`crate::dom::Document`]) and the
//!   JS engine (already alive from [`crate::engine::JsEngine`]).
//! - **It is not** a full implementation of WHATWG "prepare a script" +
//!   "execute a script". Two deliberate Phase-1C simplifications remain:
//!   - `defer` and `async` are ignored — every recognized script runs
//!     synchronously in document order, like jsdom's basic mode.
//!   - External `src=...` either errors or is skipped with a console
//!     warning, gated by [`ScriptFetchPolicy`]. Fetching real subresources
//!     is item C in the next-phase plan (vendor `llrt_fetch`).
//!
//! ## ES modules (item M-A)
//!
//! `<script type="module">` runs as a real ES module per WHATWG HTML
//! §8.1.3 "Module scripts" — `import` / `export` syntax is now legal,
//! and `import "./dep.js"` walks the dependency graph through the
//! engine's [`crate::modules::HttpLoader`]. The engine pre-seeds inline
//! module bodies into the [`crate::modules::ModuleCache`] under a
//! synthetic specifier ([`crate::modules::inline_module_specifier`]),
//! then calls `Module::evaluate`. External `<script type="module"
//! src="...">` references go through the same `ScriptFetchPolicy::Fetch`
//! path as classic external scripts — pre-fetched and seeded into the
//! cache, then evaluated through QuickJS's module pump so any chained
//! `import` runs through [`crate::modules::HttpLoader`] too.
//!
//! ## Algorithm references
//!
//! The MIME-classification table and the "inline vs external" branch
//! mirror the canonical browser-engine implementations of "prepare a
//! script element":
//!
//! - `jsdom`/`lib/jsdom/living/nodes/HTMLScriptElement-impl.js` (MIT) —
//!   `_getTypeString()`, `getType()`, `_eval()`. The MIME list, the
//!   classic-vs-module split, and the inline-vs-external dispatch all
//!   come from here.
//! - `happy-dom`/`packages/happy-dom/src/nodes/html-script-element/`
//!   (MIT) — `HTMLScriptElement.ts`, `ScriptUtility.ts`. Same shape,
//!   simpler error-handling story.
//! - WHATWG HTML Living Standard §4.12.1
//!   <https://html.spec.whatwg.org/multipage/scripting.html> — the
//!   normative classification rules (classic / module / importmap /
//!   speculationrules / data block / null).
//!
//! No vendoring: both jsdom and happy-dom are JavaScript and cannot be
//! linked directly. The algorithm above is small enough that reading
//! the two implementations + the spec and porting the *order* into
//! Rust is cheaper than building a JS-in-Rust bridge. License of the
//! lifted *logic* (no code copied verbatim, only the algorithm shape)
//! is irrelevant for that reason — but both prior arts are MIT so even
//! a direct port would be compatible with heso's MIT/Apache dual.
//!
//! ## Error containment (ADR 0008 spirit)
//!
//! A script that throws is captured as a [`ConsoleEntry`] of level
//! [`ConsoleLevel::Error`] on the engine's shared console buffer; the
//! next script still runs. WHATWG's "report the exception" reduces to
//! the same observable in our agent context — we don't have a real
//! `error` event dispatch target (no `Window`), and halting all
//! subsequent scripts on a single throw would make page-fragility
//! observably leak into the engine's continued operation, which is
//! the same determinism trap [`crate::timers::advance_clock`]
//! discusses for setTimeout callbacks.
//!
//! [plan]: ../../.agent/next-phase-plan.md

use std::sync::{Arc, Mutex};

use rquickjs::{
    context::EvalOptions, CatchResultExt, CaughtError, Context, Function, Module, Value,
};

use crate::engine::{ConsoleEntry, ConsoleLevel, EvalError};
use crate::import_map::parse_import_map;
use crate::modules::{inline_module_specifier, ModuleCache, SharedImportMap};

/// Snapshot of what `document.currentScript` should reflect while a
/// `<script>` element is executing.
///
/// Per WHATWG HTML §3.1.1 "Document.currentScript":
///
/// > Returns the script element, or the SVG script element, that is
/// > currently executing, as long as the element represents a classic
/// > script. In the case of reentrant script execution, returns the
/// > one that most recently started executing amongst those that have
/// > not yet finished executing. Returns null if the Document is not
/// > currently executing a script or SVG script element (e.g., because
/// > the running script is an event handler, or a timeout), or if the
/// > currently executing script or SVG script element is a module
/// > script.
///
/// Turbopack-emitted chunks rely on this for chunk-self-identification:
/// each `<script src="/_next/static/chunks/<hash>.js">` body opens with
///
/// ```js
/// (globalThis.TURBOPACK||(globalThis.TURBOPACK=[])).push([
///     "object"==typeof document ? document.currentScript : void 0,
///     <chunkId>, …
/// ]);
/// ```
///
/// then the runtime (added by the entrypoint Turbopack chunk) defines
/// the `push` hook as `registerChunk`, which reads
/// `registration[0].getAttribute("src")` to know which chunk just
/// executed. When `document.currentScript` is `undefined`, that
/// registration[0] is `void 0`, and the runtime throws
/// `"chunk path empty but not in a worker"` (vercel/next.js,
/// `turbopack/crates/turbopack-ecmascript-runtime/js/src/browser/runtime/base/runtime-base.ts`,
/// the `getChunkFromRegistration` function), which kills hydration on
/// every modern Next.js site (nextjs.org, vercel.com, Linear, Notion,
/// most YC pages).
///
/// We don't have JS-side `HTMLScriptElement` objects for every parsed
/// `<script>` (heso uses raw `dom_query` nodes for the pump), so the
/// patch installs a small synthetic object on `document.currentScript`
/// that mimics the bits browsers expose from a script element:
///
/// - `tagName` / `nodeName` — "SCRIPT" (frameworks check this when
///   resolving the script's own ancestor).
/// - `getAttribute(name)` — returns the raw `src` attribute value for
///   external scripts (and `null` otherwise), exactly matching the DOM
///   `Element.getAttribute(name)` contract (returns the literal string
///   from the source HTML, not a resolved URL).
/// - `hasAttribute("src")` — true iff external.
/// - `src` — the *resolved* absolute URL, matching the HTMLScriptElement
///   `src` IDL attribute which reflects via the URL parser. Consumers
///   that want the resolved form (rare) get it; consumers that want the
///   raw attribute (the common case, used by Turbopack) call
///   `getAttribute("src")`.
///
/// Modules and the no-script-currently-running state set
/// `document.currentScript = null`, per spec.
enum CurrentScriptShape<'a> {
    /// No script currently running — set to `null`.
    None,
    /// Inline classic script — set to a synthetic with no `src`
    /// (matches `<script>…</script>` where `getAttribute("src")`
    /// returns `null`).
    InlineClassic,
    /// External classic script — set to a synthetic carrying the raw
    /// `src` attribute (what `getAttribute("src")` returns to JS) and
    /// the resolved URL (what the `.src` IDL property returns).
    ExternalClassic {
        raw_src: &'a str,
        resolved_src: &'a str,
    },
}

/// Update `document.currentScript` to reflect `shape`.
///
/// Builds the synthetic POJO described on [`CurrentScriptShape`] and
/// assigns it to `document.currentScript`. Best-effort: if the
/// document global is missing (e.g. a future caller skips
/// [`crate::JsEngine::install_document`]) the call is a no-op so the
/// script pump still runs.
///
/// Failures inside the QuickJS eval are silently swallowed (returned as
/// `Ok(())`) — a script pump that crashed because we couldn't write a
/// debug-only spec field would be a worse outcome than the field being
/// stale; this is the same "swallow internal-bookkeeping failures"
/// posture [`install_location`] takes after a navigation.
fn set_current_script(context: &Context, shape: CurrentScriptShape<'_>) -> Result<(), EvalError> {
    let snippet = match shape {
        CurrentScriptShape::None => {
            "if (typeof document !== 'undefined') { document.currentScript = null; }".to_owned()
        }
        CurrentScriptShape::InlineClassic => {
            // Inline script: spec-compliant `getAttribute("src")` returns
            // null (an inline `<script>` has no `src` attribute). Most
            // page code only checks `document.currentScript !== null`,
            // so the non-null sentinel is enough; the few callers that
            // call `.getAttribute("src")` see null, matching a real
            // inline script.
            r#"
            if (typeof document !== 'undefined') {
                document.currentScript = {
                    tagName: 'SCRIPT',
                    nodeName: 'SCRIPT',
                    nodeType: 1,
                    src: '',
                    type: '',
                    async: false,
                    defer: false,
                    noModule: false,
                    getAttribute: function(name) { return null; },
                    hasAttribute: function(name) { return false; },
                    getAttributeNames: function() { return []; },
                };
            }
            "#
            .to_owned()
        }
        CurrentScriptShape::ExternalClassic {
            raw_src,
            resolved_src,
        } => {
            // External script: `getAttribute("src")` returns the raw
            // attribute (Turbopack reads this), `.src` returns the
            // resolved absolute URL (HTMLScriptElement IDL contract).
            //
            // Quote the strings via JSON.stringify-equivalent escaping
            // — `serde_json` handles every JS-string edge case (quotes,
            // backslashes, control chars, unicode) the same way
            // `JSON.stringify` would.
            let raw_lit = serde_json::Value::String(raw_src.to_owned()).to_string();
            let resolved_lit = serde_json::Value::String(resolved_src.to_owned()).to_string();
            format!(
                r#"
            if (typeof document !== 'undefined') {{
                var __hesoCsRaw = {raw_lit};
                var __hesoCsRes = {resolved_lit};
                document.currentScript = {{
                    tagName: 'SCRIPT',
                    nodeName: 'SCRIPT',
                    nodeType: 1,
                    src: __hesoCsRes,
                    type: '',
                    async: false,
                    defer: false,
                    noModule: false,
                    getAttribute: function(name) {{
                        if (typeof name !== 'string') return null;
                        return name.toLowerCase() === 'src' ? __hesoCsRaw : null;
                    }},
                    hasAttribute: function(name) {{
                        return typeof name === 'string' && name.toLowerCase() === 'src';
                    }},
                    getAttributeNames: function() {{ return ['src']; }},
                }};
            }}
            "#
            )
        }
    };
    context
        .with(|ctx| -> rquickjs::Result<()> {
            // Swallow eval errors (no document, strict-mode quirks):
            // currentScript bookkeeping must never abort the pump.
            let _ = ctx.eval::<Value, _>(snippet);
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("set document.currentScript: {e}")))?;
    Ok(())
}

/// Resolve a (possibly relative) `src` attribute against the page base
/// URL — the canonical form a real `HTMLScriptElement.src` IDL property
/// would return.
///
/// Mirrors the same `Url::join` rule [`fetch_script_source`] uses for
/// the HTTP fetch path, so the URL bound to `document.currentScript.src`
/// matches what the network layer requested. Without a base URL, the
/// raw `src` is returned unchanged.
fn resolve_script_src(src: &str, base_url: Option<&url::Url>) -> String {
    match base_url {
        Some(base) => match base.join(src) {
            Ok(u) => u.to_string(),
            Err(_) => src.to_owned(),
        },
        None => src.to_owned(),
    }
}

/// Policy for handling external `<script src="...">` references.
///
/// The synchronous-blocking fetch jsdom defaults to is the
/// correct-by-spec behavior but introduces network traffic on what an
/// agent expects to be a "parse + eval" command. The default is to
/// skip + warn; opt-in via `--js-fetch` on the CLI flips to
/// [`Self::Fetch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScriptFetchPolicy {
    /// External scripts are silently skipped (a `console.warn` entry is
    /// appended for visibility). This is the default for
    /// `heso eval-dom` without `--js-fetch`.
    #[default]
    Skip,
    /// External scripts produce a [`ConsoleLevel::Error`] entry
    /// explaining that subresource fetch isn't wired yet. The script
    /// is not executed (no `Error::NotReady` style abort — same
    /// containment rule as a throwing inline script). Reserved for
    /// historical callers; new code should use [`Self::Fetch`] when
    /// fetch is wired and [`Self::Skip`] otherwise.
    Error,
    /// External scripts are fetched synchronously via the engine's
    /// shared `reqwest::Client` (the same client the rest of the
    /// workspace uses, threaded in via [`crate::JsEngine::new_with_fetch`]).
    /// Per jsdom's basic mode: each `<script src=...>` blocks the
    /// pump until its body returns, then executes inline. Failures
    /// (HTTP error, body decode error, timeout) are captured as
    /// [`ConsoleLevel::Error`] entries; the pump continues to the
    /// next script — same containment rule as a throwing inline
    /// script.
    ///
    /// If the engine has no fetch client (no
    /// [`crate::JsEngine::new_with_fetch`] call), [`Self::Fetch`]
    /// degrades to [`Self::Error`] semantics: a clear message
    /// explaining the engine wasn't built with a fetch backend.
    Fetch,
}

/// Outcome of [`run_scripts`] — useful for receipts and tests.
///
/// All counts refer to `<script>` elements we *encountered*, not just
/// JavaScript MIME types: a `<script type="application/ld+json">` data
/// block is counted under `skipped_non_script_type`, not `executed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct ScriptOutcome {
    /// Inline scripts of recognized JS type that ran without throwing.
    pub executed: usize,
    /// Inline scripts of recognized JS type that threw — their errors
    /// were captured on the console buffer; counted separately so
    /// callers can tell apart "ran fine" from "ran but threw."
    pub executed_with_error: usize,
    /// External `<script src=...>` references touched (skipped or
    /// errored per [`ScriptFetchPolicy`]).
    pub external_handled: usize,
    /// Elements whose `type` attribute did not classify as classic /
    /// module (e.g. `application/json`, `application/ld+json`). These
    /// are data blocks per the HTML spec, not code.
    pub skipped_non_script_type: usize,
}

/// One captured per-script failure — surfaced into the `failed_scripts`
/// field of the `heso open` / `heso read` / `heso wait` envelope when
/// the agent passes `--best-effort` (or unconditionally as a
/// `failed_scripts` companion to the same envelope's `console` entries).
///
/// Categorical `reason` values keep the agent-facing surface narrow:
///
/// - `"script_crash"` — an inline or external script threw a synchronous
///   exception (`throw new Error(...)`, syntax error, non-Error throw).
///   This is the dominant failure mode for broken hydration.
/// - `"fetch_failed"` — an external `<script src=...>` couldn't be
///   downloaded (HTTP error, DNS, body decode). The script was never
///   executed.
/// - `"importmap_parse_error"` — a `<script type="importmap">` data
///   block failed to parse as a valid import map. The pump continued
///   with no map installed.
///
/// `url` is the resolved (absolute) URL for external scripts, and
/// `None` for inline scripts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScriptFailure {
    /// Resolved URL of the failed script — `None` for inline scripts.
    pub url: Option<String>,
    /// Categorical failure reason (one of `"script_crash"`,
    /// `"fetch_failed"`, `"importmap_parse_error"`).
    pub reason: String,
    /// Human-readable message — the exception message for crashes, the
    /// HTTP/transport error text for fetch failures. Always present;
    /// truncated by the per-throw formatter when long.
    pub message: String,
    /// 1-indexed line number inside the script where the exception was
    /// thrown, when QuickJS provided one. `None` when the engine
    /// couldn't recover a line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// Run every `<script>` element in `document` against `context`, in
/// document order, recording outcomes per [`ScriptOutcome`].
///
/// Exceptions thrown by individual scripts are appended to
/// `console_buffer` as [`ConsoleLevel::Error`] entries; subsequent
/// scripts still execute. Engine-internal failures (out-of-memory,
/// runtime tear-down) propagate as [`EvalError::Engine`] and abort the
/// pump — those are not script bugs and continuing past them would
/// produce nonsense.
///
/// This function does **not** clear `console_buffer`; the caller
/// chooses whether script output is observable from the user's `eval`
/// call (it is — that's the point) or should be drained first.
///
/// ## Why we re-enter `Ctx::with` per script
///
/// `Context::with` is the only safe way to acquire a `Ctx<'_>` from
/// rquickjs, and `Ctx` is single-shot-per-`with`. We could batch all
/// scripts inside one `Ctx::with`, but doing so would force us to
/// extract script source under the rquickjs borrow — which would then
/// require borrowing the `dom_query::Document` (already inside an
/// `Arc`) for the full duration. The per-script-with pattern keeps
/// the two borrow scopes independent: extract source under `dom_query`,
/// then enter rquickjs to execute. Cost is one extra context
/// acquisition per script — cheap.
#[allow(clippy::too_many_arguments)]
pub fn run_scripts(
    context: &Context,
    document: &dom_query::Document,
    policy: ScriptFetchPolicy,
    console_buffer: &Arc<Mutex<Vec<ConsoleEntry>>>,
    failures: &Arc<Mutex<Vec<ScriptFailure>>>,
    fetch_client: Option<&(Arc<reqwest::Client>, tokio::runtime::Handle)>,
    base_url: Option<&url::Url>,
    module_cache: &ModuleCache,
    import_map: &SharedImportMap,
) -> Result<ScriptOutcome, EvalError> {
    let scripts = collect_scripts(document);
    let mut outcome = ScriptOutcome::default();
    // Track inline module ordinal — each inline `<script type="module">`
    // gets a distinct synthetic specifier so the runtime's module map
    // doesn't collide them. See [`inline_module_specifier`].
    let mut inline_module_index: usize = 0;

    // Initialize `document.currentScript = null` per WHATWG HTML §3.1.1.
    // Real browsers initialize this on Document construction; we do it
    // at script-pump entry instead — `document` is a Class instance
    // installed by [`JsEngine::install_document`] right before
    // [`run_scripts`] runs, and arbitrary-property writes on Class
    // instances are cheap. The null sentinel matters even on pages
    // with no scripts: code that reads `document.currentScript` from a
    // later `eval_no_clear` call (e.g. a user's probe) gets `null`
    // (the spec value), not `undefined`.
    set_current_script(context, CurrentScriptShape::None)?;

    // Wire 2 — pre-pass: scan for the first `<script type="importmap">`
    // and install its parsed map into the engine's `SharedImportMap`
    // BEFORE any module script runs.
    //
    // Per WHATWG HTML §8.1.5, the import map must be installed before
    // any "fetch a single module script" call evaluates against it.
    // In our pump, that's equivalent to "before any
    // ScriptKind::InlineModule / ExternalModule runs." We don't need
    // to honor document-order *within* the importmap-vs-module
    // sequencing because the spec actually requires the import map
    // to be ready first regardless of where it appears in the
    // source — a future spec edit moving toward strict
    // before-first-module-fetch ordering is what shipping browsers
    // do today.
    //
    // The `collect_scripts` pre-pass already enforces "only the first
    // importmap counts"; we just walk the classified vector here.
    for script in &scripts {
        if let ScriptKind::ImportMap { source } = &script.kind {
            install_import_map(import_map, source, base_url, console_buffer, failures);
            break;
        }
    }

    for script in scripts {
        match script.kind {
            ScriptKind::InlineClassic { source } => {
                // Mark `document.currentScript` per WHATWG HTML §3.1.1:
                // "set to the script element being processed; classic
                //  scripts only." Inline classics get a non-null
                //  synthetic whose `getAttribute("src")` returns null.
                set_current_script(context, CurrentScriptShape::InlineClassic)?;
                let eval_result = eval_one(context, &source);
                set_current_script(context, CurrentScriptShape::None)?;
                match eval_result? {
                    Some(err_msg) => {
                        outcome.executed_with_error += 1;
                        push_failure(
                            failures,
                            ScriptFailure {
                                url: None,
                                reason: "script_crash".to_owned(),
                                message: err_msg.clone(),
                                line: extract_line_from_message(&err_msg),
                            },
                        );
                        push_console(console_buffer, ConsoleLevel::Error, err_msg);
                    }
                    None => {
                        outcome.executed += 1;
                    }
                }
            }
            ScriptKind::InlineModule { source } => {
                let specifier = inline_module_specifier(base_url, inline_module_index);
                inline_module_index += 1;
                // Pre-seed the cache so `HttpLoader::load` serves the
                // body without trying to fetch the synthetic URL.
                // Importing relative URLs from inside this module will
                // still go through the loader and (on cache miss) hit
                // the HTTP path.
                module_cache.insert(specifier.clone(), source.clone());
                // Per HTML §3.1.1, modules keep `document.currentScript`
                // null — only classic scripts set it.
                set_current_script(context, CurrentScriptShape::None)?;
                match eval_one_module(context, &specifier, &source)? {
                    Some(err_msg) => {
                        outcome.executed_with_error += 1;
                        push_failure(
                            failures,
                            ScriptFailure {
                                url: None,
                                reason: "script_crash".to_owned(),
                                message: err_msg.clone(),
                                line: extract_line_from_message(&err_msg),
                            },
                        );
                        push_console(console_buffer, ConsoleLevel::Error, err_msg);
                    }
                    None => {
                        outcome.executed += 1;
                    }
                }
            }
            ScriptKind::ExternalClassic { src } => {
                outcome.external_handled += 1;
                handle_external_classic(
                    context,
                    policy,
                    console_buffer,
                    failures,
                    fetch_client,
                    base_url,
                    &src,
                    &mut outcome,
                )?;
            }
            ScriptKind::ExternalModule { src } => {
                outcome.external_handled += 1;
                handle_external_module(
                    context,
                    policy,
                    console_buffer,
                    failures,
                    fetch_client,
                    base_url,
                    &src,
                    module_cache,
                    &mut outcome,
                )?;
            }
            ScriptKind::ImportMap { .. } => {
                // Already installed in the pre-pass above. We still
                // count it as a data block (same bucket as
                // `application/ld+json` etc. — non-runnable code).
                outcome.skipped_non_script_type += 1;
            }
            ScriptKind::NonScriptType => {
                outcome.skipped_non_script_type += 1;
            }
        }
    }

    // Per spec, `document.currentScript` is null once script pump exits
    // (it's only non-null while a classic script's body is executing).
    set_current_script(context, CurrentScriptShape::None)?;

    Ok(outcome)
}

/// Parse the body of a `<script type="importmap">` data block and
/// install the result into the engine's [`SharedImportMap`]. Failures
/// (malformed JSON, structurally invalid map) are appended to the
/// console buffer as `console.error` — same containment story as a
/// throwing inline script.
///
/// The engine's `SharedImportMap` is replaced in-place (not merged)
/// so a navigation onto a new page that declares its own importmap
/// drops the previous page's map atomically.
fn install_import_map(
    import_map: &SharedImportMap,
    source: &str,
    base_url: Option<&url::Url>,
    console_buffer: &Arc<Mutex<Vec<ConsoleEntry>>>,
    failures: &Arc<Mutex<Vec<ScriptFailure>>>,
) {
    // `parse_import_map` needs a base URL to normalize keys + scope
    // prefixes. Without a page URL (bare `eval-js` / similar), fall
    // back to `about:blank`. An importmap on a page with no URL is
    // unusual but the parser is robust: bare-name keys stay bare,
    // and any "./relative" address fails to parse against the cannot-
    // be-a-base `about:blank`, becoming `None` (per
    // `normalize_specifier_map`'s spec rule).
    let base = match base_url {
        Some(u) => u.clone(),
        None => match url::Url::parse("about:blank") {
            Ok(u) => u,
            Err(_) => return,
        },
    };
    match parse_import_map(source, &base) {
        Ok(map) => {
            *import_map.borrow_mut() = map;
        }
        Err(e) => {
            let msg = format!("heso: <script type=\"importmap\"> failed to parse: {e}");
            push_failure(
                failures,
                ScriptFailure {
                    url: None,
                    reason: "importmap_parse_error".to_owned(),
                    message: msg.clone(),
                    line: None,
                },
            );
            push_console(console_buffer, ConsoleLevel::Error, msg);
        }
    }
}

/// External classic `<script src="...">` — fetch synchronously, then
/// evaluate as a classic script via `ctx.eval`. Same containment story
/// as inline classics: a throw lands on the console buffer as
/// `ConsoleLevel::Error` and the pump continues.
#[allow(clippy::too_many_arguments)]
fn handle_external_classic(
    context: &Context,
    policy: ScriptFetchPolicy,
    console_buffer: &Arc<Mutex<Vec<ConsoleEntry>>>,
    failures: &Arc<Mutex<Vec<ScriptFailure>>>,
    fetch_client: Option<&(Arc<reqwest::Client>, tokio::runtime::Handle)>,
    base_url: Option<&url::Url>,
    src: &str,
    outcome: &mut ScriptOutcome,
) -> Result<(), EvalError> {
    match (policy, fetch_client) {
        (ScriptFetchPolicy::Skip, _) => {
            push_console(
                console_buffer,
                ConsoleLevel::Warn,
                format!(
                    "heso: skipped external script <script src=\"{src}\"> (pass --js-fetch to enable subresource fetch)"
                ),
            );
        }
        (ScriptFetchPolicy::Error, _) => {
            push_console(
                console_buffer,
                ConsoleLevel::Error,
                format!("heso: external script fetch disabled. Wanted <script src=\"{src}\">"),
            );
        }
        (ScriptFetchPolicy::Fetch, Some((client, rt))) => {
            match fetch_script_source(client, rt, src, base_url) {
                Ok(source) => {
                    // Set `document.currentScript` to a synthetic
                    // HTMLScriptElement-shaped POJO so
                    // [Turbopack-emitted chunks][1] can self-identify.
                    //
                    // [1]: see `CurrentScriptShape` for the runtime
                    // contract (`getAttribute("src")` returning the raw
                    // attribute is what trips the
                    // "chunk path empty but not in a worker" throw when
                    // it's missing).
                    let resolved = resolve_script_src(src, base_url);
                    set_current_script(
                        context,
                        CurrentScriptShape::ExternalClassic {
                            raw_src: src,
                            resolved_src: &resolved,
                        },
                    )?;
                    let eval_result = eval_one(context, &source);
                    set_current_script(context, CurrentScriptShape::None)?;
                    match eval_result? {
                        Some(err_msg) => {
                            outcome.executed_with_error += 1;
                            push_failure(
                                failures,
                                ScriptFailure {
                                    url: Some(resolved.clone()),
                                    reason: "script_crash".to_owned(),
                                    message: err_msg.clone(),
                                    line: extract_line_from_message(&err_msg),
                                },
                            );
                            push_console(
                                console_buffer,
                                ConsoleLevel::Error,
                                format!("<script src=\"{src}\"> threw: {err_msg}"),
                            );
                        }
                        None => {
                            outcome.executed += 1;
                        }
                    }
                }
                Err(e) => {
                    let resolved = resolve_script_src(src, base_url);
                    push_failure(
                        failures,
                        ScriptFailure {
                            url: Some(resolved),
                            reason: "fetch_failed".to_owned(),
                            message: e.clone(),
                            line: None,
                        },
                    );
                    push_console(
                        console_buffer,
                        ConsoleLevel::Error,
                        format!("heso: <script src=\"{src}\"> fetch failed: {e}"),
                    );
                }
            }
        }
        (ScriptFetchPolicy::Fetch, None) => {
            push_console(
                console_buffer,
                ConsoleLevel::Error,
                format!(
                    "heso: <script src=\"{src}\"> wanted Fetch policy but engine has no fetch client (build with JsEngine::new_with_fetch)"
                ),
            );
        }
    }
    Ok(())
}

/// External `<script type="module" src="...">` — resolve the URL,
/// pre-fetch its body so the first hop stays on the sync path, seed
/// it into [`ModuleCache`] under the resolved URL, then call
/// [`Module::evaluate`]. QuickJS recursively calls [`HttpLoader::load`]
/// for every nested `import` it encounters; the loader either serves a
/// cached body or fetches via HTTP.
#[allow(clippy::too_many_arguments)]
fn handle_external_module(
    context: &Context,
    policy: ScriptFetchPolicy,
    console_buffer: &Arc<Mutex<Vec<ConsoleEntry>>>,
    failures: &Arc<Mutex<Vec<ScriptFailure>>>,
    fetch_client: Option<&(Arc<reqwest::Client>, tokio::runtime::Handle)>,
    base_url: Option<&url::Url>,
    src: &str,
    module_cache: &ModuleCache,
    outcome: &mut ScriptOutcome,
) -> Result<(), EvalError> {
    match (policy, fetch_client) {
        (ScriptFetchPolicy::Skip, _) => {
            push_console(
                console_buffer,
                ConsoleLevel::Warn,
                format!(
                    "heso: skipped external module <script type=\"module\" src=\"{src}\"> (pass --js-fetch to enable subresource fetch)"
                ),
            );
        }
        (ScriptFetchPolicy::Error, _) => {
            push_console(
                console_buffer,
                ConsoleLevel::Error,
                format!(
                    "heso: external module fetch disabled. Wanted <script type=\"module\" src=\"{src}\">"
                ),
            );
        }
        (ScriptFetchPolicy::Fetch, Some((client, rt))) => {
            // Resolve `src` against page base URL — same join rule
            // [`fetch_script_source`] uses internally. We do the join
            // explicitly here so the cache key matches the URL that
            // QuickJS's module evaluator will see (after our resolver
            // also joins).
            let resolved: String = match base_url {
                Some(base) => match base.join(src) {
                    Ok(u) => u.to_string(),
                    Err(_) => src.to_owned(),
                },
                None => src.to_owned(),
            };
            // Per HTML §3.1.1, modules keep `document.currentScript`
            // null — only classic scripts set it.
            set_current_script(context, CurrentScriptShape::None)?;
            match fetch_script_source(client, rt, src, base_url) {
                Ok(source) => {
                    module_cache.insert(resolved.clone(), source.clone());
                    match eval_one_module(context, &resolved, &source)? {
                        Some(err_msg) => {
                            outcome.executed_with_error += 1;
                            push_failure(
                                failures,
                                ScriptFailure {
                                    url: Some(resolved.clone()),
                                    reason: "script_crash".to_owned(),
                                    message: err_msg.clone(),
                                    line: extract_line_from_message(&err_msg),
                                },
                            );
                            push_console(
                                console_buffer,
                                ConsoleLevel::Error,
                                format!("<script type=\"module\" src=\"{src}\"> threw: {err_msg}"),
                            );
                        }
                        None => {
                            outcome.executed += 1;
                        }
                    }
                }
                Err(e) => {
                    push_failure(
                        failures,
                        ScriptFailure {
                            url: Some(resolved.clone()),
                            reason: "fetch_failed".to_owned(),
                            message: e.clone(),
                            line: None,
                        },
                    );
                    push_console(
                        console_buffer,
                        ConsoleLevel::Error,
                        format!("heso: <script type=\"module\" src=\"{src}\"> fetch failed: {e}"),
                    );
                }
            }
        }
        (ScriptFetchPolicy::Fetch, None) => {
            push_console(
                console_buffer,
                ConsoleLevel::Error,
                format!(
                    "heso: <script type=\"module\" src=\"{src}\"> wanted Fetch policy but engine has no fetch client (build with JsEngine::new_with_fetch)"
                ),
            );
        }
    }
    Ok(())
}

/// Synchronously fetch `src` via the shared `reqwest::Client`. Used
/// by [`ScriptFetchPolicy::Fetch`] to honor `<script src=...>` refs
/// during page hydration.
///
/// `src` may be absolute (`https://...`) or relative (`base.js`,
/// `../foo/bar.js`). Relative refs are resolved against `base_url`
/// via [`url::Url::join`]. Without a base URL, `src` is sent to
/// `reqwest` as-is — which works for absolute URLs and fails with
/// "send: builder error" for relative ones (caller is responsible
/// for setting the engine's base URL via
/// [`crate::JsEngine::set_base_url`]).
fn fetch_script_source(
    client: &reqwest::Client,
    rt: &tokio::runtime::Handle,
    src: &str,
    base_url: Option<&url::Url>,
) -> Result<String, String> {
    // Resolve relative src against base. `Url::join` handles both
    // absolute src (returns src) and relative (joins). If parsing
    // fails outright, fall back to the raw src so reqwest can produce
    // a clear error.
    let resolved: String = match base_url {
        Some(base) => match base.join(src) {
            Ok(u) => u.to_string(),
            Err(_) => src.to_owned(),
        },
        None => src.to_owned(),
    };
    // `block_in_place` lets the CLI's `#[tokio::main]` flow run this
    // synchronously without tripping the "runtime from within a
    // runtime" panic — same trick as `crate::fetch::perform_request`.
    tokio::task::block_in_place(|| {
        rt.block_on(async {
            let resp = client
                .get(&resolved)
                .send()
                .await
                .map_err(|e| format!("send: {e}"))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }
            resp.text().await.map_err(|e| format!("read body: {e}"))
        })
    })
}

/// Internal: one script element after classification.
enum ScriptKind {
    /// Inline `<script>...source...</script>` of classic (non-module)
    /// JavaScript MIME type. Evaluated via `ctx.eval` against the
    /// shared global scope.
    InlineClassic { source: String },
    /// Inline `<script type="module">...source...</script>`. Evaluated
    /// via [`Module::evaluate`] after the source is pre-seeded into
    /// the [`ModuleCache`] under a synthetic specifier
    /// ([`inline_module_specifier`]). `import` statements inside the
    /// body resolve through the engine's [`HttpLoader`].
    InlineModule { source: String },
    /// `<script src="..."></script>` — classic-MIME content at a URL.
    ExternalClassic { src: String },
    /// `<script type="module" src="..."></script>` — ES module at a
    /// URL. The engine pre-fetches the body, seeds it into the
    /// [`ModuleCache`] under the resolved URL, then drives
    /// [`Module::evaluate`] which runs through QuickJS's module pump
    /// (recursively calling our `HttpLoader::load` for every nested
    /// `import` it finds).
    ExternalModule { src: String },
    /// `<script type="importmap">…JSON…</script>` — WHATWG HTML §4.12.1
    /// data block carrying the page's import map. Per HTML §8.1.5,
    /// only the first such block on the page is honored; subsequent
    /// ones are silently ignored (the pre-pass in [`collect_scripts`]
    /// uses `seen_import_map` to enforce this). The body is parsed via
    /// [`parse_import_map`] and installed into the engine's
    /// [`SharedImportMap`] *before* any module script runs, so even
    /// the first `<script type="module">` on the page sees the map.
    ImportMap { source: String },
    /// `<script type="...">` whose type is not a JavaScript MIME nor
    /// `"module"` — a data block per HTML spec §4.12.1. Counted but
    /// not executed.
    NonScriptType,
}

/// One classified `<script>` element ready for the runner.
struct ClassifiedScript {
    kind: ScriptKind,
}

/// Walk `document` in document order, classify every `<script>`
/// element, and return them in execution order.
///
/// We do the walk + classification once up-front (rather than streaming
/// during execution) so that script-injected `<script>` elements
/// (e.g. inline script that does `document.body.appendChild(scriptEl)`)
/// do **not** re-trigger this pass. That matches jsdom's "scripts
/// inserted by an already-running script don't get re-prepared by the
/// initial parse phase" — it's a deliberate punt: dynamic script
/// insertion is a Phase 1D concern once we wire `appendChild` to
/// re-run prepare. Today, the agent sees a single document-order pass.
fn collect_scripts(document: &dom_query::Document) -> Vec<ClassifiedScript> {
    let mut out = Vec::new();
    let root = document.tree.root();
    // Per HTML §8.1.5, *only the first* `<script type="importmap">`
    // on the page is honored. Later importmap blocks are silently
    // ignored (the spec also lets a UA print a console warning;
    // we skip that for now). We track `seen_import_map` so the
    // second-and-onward importmap blocks fall through to
    // [`ScriptKind::NonScriptType`] (counted but ignored).
    let mut seen_import_map = false;
    for descendant in root.descendants_it() {
        if !descendant.is_element() {
            continue;
        }
        let Some(name) = descendant.node_name() else {
            continue;
        };
        if !name.as_ref().eq_ignore_ascii_case("script") {
            continue;
        }

        // 1. Classify by `type` attribute. Per the spec / jsdom's
        //    `_getTypeString` + `getType`:
        //    - absent / empty / JS MIME essence-match → classic
        //    - "module" (ASCII case-insensitive) → real ES module
        //      (item M-A — see [`crate::modules`])
        //    - "importmap" (ASCII case-insensitive) → WHATWG HTML
        //      §8.1.5 data block (item M-B + this wireup) — first
        //      one wins, later ones become NonScriptType.
        //    - anything else (incl. "application/json",
        //      "speculationrules", "text/html") → data block / null
        //      → not executed.
        let type_attr = descendant.attr("type").map(|s| s.to_string());
        if is_import_map_script_type(type_attr.as_deref()) {
            if seen_import_map {
                // Second-and-later importmap blocks — spec says
                // ignore. We classify as NonScriptType so the
                // outcome tally counts it correctly.
                out.push(ClassifiedScript {
                    kind: ScriptKind::NonScriptType,
                });
                continue;
            }
            seen_import_map = true;
            // Per spec, importmap data blocks are inline-only — a
            // `<script type="importmap" src="...">` is invalid (the
            // spec says "the src attribute must not be specified");
            // browsers treat such a block as if the `src` were
            // absent (parse the inline text) but the realistic
            // shape we see in the wild is inline JSON.
            let source = descendant.text().to_string();
            out.push(ClassifiedScript {
                kind: ScriptKind::ImportMap { source },
            });
            continue;
        }
        if !is_runnable_script_type(type_attr.as_deref()) {
            out.push(ClassifiedScript {
                kind: ScriptKind::NonScriptType,
            });
            continue;
        }
        let is_module = is_module_script_type(type_attr.as_deref());

        // 2. Inline vs external. Per jsdom's `_eval()`:
        //    `if (hasAttribute("src")) fetchExternalScript(); else
        //    fetchInternalScript();`
        if let Some(src) = descendant.attr("src") {
            // Empty src — happens with `<script src=""></script>` in
            // the wild. WHATWG says treat as if no src for the
            // classification step, then the empty-URL fetch fails. We
            // simplify: empty src → External("") and let the policy
            // surface its standard warning.
            let kind = if is_module {
                ScriptKind::ExternalModule {
                    src: src.to_string(),
                }
            } else {
                ScriptKind::ExternalClassic {
                    src: src.to_string(),
                }
            };
            out.push(ClassifiedScript { kind });
            continue;
        }

        let source = descendant.text().to_string();
        let kind = if is_module {
            ScriptKind::InlineModule { source }
        } else {
            ScriptKind::InlineClassic { source }
        };
        out.push(ClassifiedScript { kind });
    }
    out
}

/// JavaScript MIME types accepted by the `type` attribute as a "classic
/// script", per WHATWG MIME Sniffing §4 (referenced from HTML §4.12.1).
///
/// Source list lifted from jsdom's `lib/jsdom/living/nodes/HTMLScriptElement-impl.js`
/// (MIT) which mirrors the spec's "JavaScript MIME type essence match."
const JS_MIME_TYPES: &[&str] = &[
    "application/ecmascript",
    "application/javascript",
    "application/x-ecmascript",
    "application/x-javascript",
    "text/ecmascript",
    "text/javascript",
    "text/javascript1.0",
    "text/javascript1.1",
    "text/javascript1.2",
    "text/javascript1.3",
    "text/javascript1.4",
    "text/javascript1.5",
    "text/jscript",
    "text/livescript",
    "text/x-ecmascript",
    "text/x-javascript",
];

/// Return `true` if `type_attr` classifies the script as runnable
/// (classic or module).
///
/// Trim, lowercase, and compare ASCII-insensitively against
/// [`JS_MIME_TYPES`] or the literal `"module"`. Per the spec:
///
/// - Missing `type` → classic (true).
/// - Empty `type` → classic (true).
/// - `type` containing parameters (e.g. `"text/javascript; charset=utf-8"`)
///   → treated as a non-essence-match → **data block** → false. This
///   matches MDN's note "Including any parameter in the type attribute
///   is the same as setting it to an unrecognized value." It might bite
///   us on real pages that copy-paste an old W3C example with the
///   charset; we'll loosen later if needed.
fn is_runnable_script_type(type_attr: Option<&str>) -> bool {
    let raw = match type_attr {
        None => return true,
        Some(s) => s.trim(),
    };
    if raw.is_empty() {
        return true;
    }
    let lower = raw.to_ascii_lowercase();
    if lower == "module" {
        return true;
    }
    JS_MIME_TYPES.iter().any(|m| *m == lower)
}

/// Return `true` only when `type_attr` classifies the script as an
/// ES **module** (per WHATWG HTML §4.12.1 + §8.1.3). Missing, empty,
/// and JS-MIME types are classic and return `false` here. Used to
/// route module scripts through the [`crate::modules`] loader and
/// classic scripts through plain `ctx.eval`.
fn is_module_script_type(type_attr: Option<&str>) -> bool {
    match type_attr {
        None => false,
        Some(s) => s.trim().eq_ignore_ascii_case("module"),
    }
}

/// Return `true` only when `type_attr` classifies the script as an
/// import-map data block (per WHATWG HTML §4.12.1 + §8.1.5). The
/// spec value is the exact literal `"importmap"` (case-insensitive).
/// Missing / empty / module / JS-MIME types all return `false`.
fn is_import_map_script_type(type_attr: Option<&str>) -> bool {
    match type_attr {
        None => false,
        Some(s) => s.trim().eq_ignore_ascii_case("importmap"),
    }
}

/// Evaluate one script's source against `context`. Returns
/// `Ok(None)` if the script ran without throwing, `Ok(Some(msg))` if
/// it threw a recoverable JS exception (the caller turns this into a
/// `console.error`), `Err(_)` only for engine-internal failures.
///
/// **Sloppy-mode evaluation.** Classic `<script>` bodies run in
/// **sloppy mode** by default per WHATWG HTML §16.1.3 (a classic
/// script is "non-strict" unless its source begins with the
/// `"use strict"` directive). Real browsers therefore accept the
/// shape `RLCONF = {...}` (Wikipedia's MediaWiki ResourceLoader) and
/// `require = function(){...}` (Apple's browserify-style UMD on
/// `ac-target.js`) as a top-level assignment that creates a global
/// property on first run. rquickjs's `Ctx::eval` defaults to *strict*
/// (`EvalOptions::default()` sets `strict: true`), which turned those
/// same lines into a `ReferenceError: <name> is not defined`. We
/// pass an explicit `EvalOptions { strict: false, .. }` here to match
/// the spec. ES modules (handled in [`eval_one_module`]) stay strict
/// per ECMA-262 §16.2.2 — module code is always strict regardless of
/// any directive.
fn eval_one(context: &Context, source: &str) -> Result<Option<String>, EvalError> {
    context.with(|ctx| -> Result<Option<String>, EvalError> {
        // `EvalOptions` is `#[non_exhaustive]` in rquickjs 0.11 — struct-literal
        // construction is forbidden across crate boundaries. Build via `default()`
        // then flip the strict bit; the other fields (`global: true`,
        // `backtrace_barrier: false`, `promise: false`, `filename: None`) stay at
        // their spec-matching defaults.
        let mut options = EvalOptions::default();
        options.strict = false;
        match ctx
            .eval_with_options::<Value, _>(source, options)
            .catch(&ctx)
        {
            Ok(_) => Ok(None),
            Err(CaughtError::Exception(exc)) => {
                let msg = match exc.message() {
                    Some(m) if !m.is_empty() => m,
                    _ => "<unknown script exception>".to_owned(),
                };
                Ok(Some(format_script_error(&msg, exc.stack())))
            }
            Err(CaughtError::Value(v)) => {
                Ok(Some(format_non_error_throw(&ctx, &v, "script")))
            },
            Err(CaughtError::Error(e)) => {
                // Genuine engine error (OOM, alloc failure) — abort
                // the pump. The console-error-and-continue rule is
                // for *script* failures only.
                Err(EvalError::Engine(format!("script eval: {e}")))
            }
        }
    })
}

/// Evaluate one ES-module script's source against `context` via
/// [`Module::evaluate`] under the synthetic `specifier` (which
/// doubles as the module's identity in QuickJS's internal module
/// map and as the base URL for relative `import` resolution).
///
/// The promise [`Module::evaluate`] returns resolves to `undefined`
/// when the module's top-level body finishes — including its
/// dependency graph and any top-level synchronous `await`s the body
/// performs. We don't await it here: the engine's `run_pending_jobs`
/// pump runs immediately after [`run_scripts`] returns and drains
/// any microtask the module produced. The Promise is dropped on
/// purpose — QuickJS sees no unhandled-rejection if it rejects,
/// because [`CatchResultExt`] intercepts the synchronous-throw path
/// below.
///
/// Returns `Ok(None)` if the module's top-level compiled and ran
/// without a synchronous throw, `Ok(Some(msg))` if it threw a
/// recoverable JS exception (compile-time syntax error, top-level
/// throw, or import-resolution error surfaced via
/// [`Error::new_loading`]), `Err(_)` only for engine-internal failures.
fn eval_one_module(
    context: &Context,
    specifier: &str,
    source: &str,
) -> Result<Option<String>, EvalError> {
    context.with(|ctx| -> Result<Option<String>, EvalError> {
        // Module::evaluate returns a Promise; we only care about the
        // synchronous-error path for now. Module bodies that read
        // through `await fetch(...)` resolve later, when the engine's
        // run_pending_jobs pump fires after we return.
        let result =
            Module::evaluate(ctx.clone(), specifier.to_owned(), source.to_owned()).catch(&ctx);
        match result {
            Ok(_promise) => Ok(None),
            Err(CaughtError::Exception(exc)) => {
                let msg = match exc.message() {
                    Some(m) if !m.is_empty() => m,
                    _ => "<unknown module exception>".to_owned(),
                };
                Ok(Some(format_script_error(&msg, exc.stack())))
            }
            Err(CaughtError::Value(v)) => Ok(Some(format_non_error_throw(&ctx, &v, "module"))),
            Err(CaughtError::Error(e)) => Err(EvalError::Engine(format!("module eval: {e}"))),
        }
    })
}

/// Format a non-`Error` thrown value into a readable diagnostic
/// string.
///
/// JavaScript lets `throw` accept any value — `throw 42`, `throw "oops"`,
/// `throw {code: ENOTFOUND}`, `throw Symbol("BAILOUT")`, `throw null`,
/// even `throw Promise.resolve(x)` (the React-Suspense pattern). The
/// rquickjs `CaughtError::Value(v)` arm hands us the raw [`Value`]
/// that was thrown. Without coercion the only thing the engine can
/// report is "non-error value," which makes diagnosing a page bug
/// impossible — a real-agent V8 run flagged this on three Next.js
/// chunks (supabase, stripe, posthog) where the actual throw turned
/// out to be `null` (presumably from a webpack module-init pattern).
///
/// We surface the type and a structural summary:
///
/// - `null` / `undefined` — literal text "null" / "undefined".
/// - Boolean / Number / BigInt — `String(value)` coercion (`true`,
///   `42`, `9007199254740993n`).
/// - String — quoted via `JSON.stringify` so embedded newlines /
///   quotes stay visible. Truncated to ~200 chars with an ellipsis
///   marker so a page that throws a megabyte of HTML doesn't spam
///   the console buffer.
/// - Symbol — `String(sym)` returns `"Symbol(description)"` which is
///   exactly what a debugger would show; this is the form
///   frameworks rely on for sentinels like `Symbol(BAILOUT_TO_CSR)`.
/// - Promise — `[object Promise]` plus a hint that this is the
///   React-Suspense bailout pattern (page code that throws a
///   Promise expects the surrounding `<Suspense>` to catch it; we
///   don't have a Suspense boundary at the top level, so the throw
///   escapes).
/// - Plain Object / Array — `JSON.stringify`, falling back to
///   `Object.prototype.toString.call(v)` when stringify rejects
///   (cycles, BigInt fields, custom toJSON throwing).
/// - Function / Class — `[Function: name]` / `[Function: anonymous]`,
///   matching Node.js's inspect format.
///
/// All extraction goes through best-effort `Function::call` /
/// `ctx.eval`. If any helper itself throws (extremely unlikely — these
/// are built-ins) the fallback is a clearly-marked diagnostic message
/// rather than panic.
///
/// `source_label` is `"script"` or `"module"` — picked to match the
/// historical `<script threw non-error value>` /
/// `<module threw non-error value>` prefix so existing log scrapers
/// keep parsing.
fn format_non_error_throw<'js>(
    ctx: &rquickjs::Ctx<'js>,
    v: &Value<'js>,
    source_label: &str,
) -> String {
    let summary = describe_thrown_value(ctx, v);
    format!("<{source_label} threw non-Error value: {summary}>")
}

/// Internal: produce the structural summary used by
/// [`format_non_error_throw`]. Split out so tests can exercise the
/// describe step against synthetic throws without round-tripping
/// through the full pump.
fn describe_thrown_value<'js>(ctx: &rquickjs::Ctx<'js>, v: &Value<'js>) -> String {
    // Primitive shortcuts — JSON.stringify / String() round-trips
    // for these are wasteful and (for null/undefined) ambiguous.
    if v.is_null() {
        return "null".to_owned();
    }
    if v.is_undefined() {
        return "undefined".to_owned();
    }
    if let Some(b) = v.as_bool() {
        return b.to_string();
    }
    if let Some(i) = v.as_int() {
        return i.to_string();
    }
    if let Some(f) = v.as_float() {
        return f.to_string();
    }
    if v.is_big_int() {
        // BigInt → String(n) gives the digit form with the `n`
        // suffix elided; matches what `console.log(1n)` prints.
        return coerce_to_string(ctx, v).unwrap_or_else(|| "<bigint>".to_owned()) + "n";
    }
    if let Some(s) = v.as_string() {
        // Quote via JSON.stringify so embedded \n, \t, quotes stay
        // readable. Truncate to keep one-line console entries small.
        let raw = s.to_string().unwrap_or_default();
        let truncated = truncate_for_display(&raw, 200);
        return serde_json::Value::String(truncated).to_string();
    }
    if v.is_symbol() {
        // `String(Symbol("BAILOUT"))` → `"Symbol(BAILOUT)"`.
        return coerce_to_string(ctx, v).unwrap_or_else(|| "Symbol(?)".to_owned());
    }
    if v.is_promise() {
        // React Suspense throws a Promise (a thenable) to signal
        // "render this client-side" / "wait for me." Without a
        // surrounding `<Suspense>` (we don't render Suspense
        // boundaries server-side), the throw escapes to the script
        // pump as a bare non-Error throw. Call out the pattern so
        // debugging users don't chase a phantom error.
        return "Promise (React Suspense bailout?)".to_owned();
    }
    if v.is_function() {
        // [Function: name] or [Function: anonymous] — matches
        // Node.js's util.inspect output for functions.
        let name = read_function_name(v);
        return format!("[Function: {}]", if name.is_empty() { "anonymous" } else { &name });
    }

    // Object / array fallthrough. Try JSON.stringify first — it gives
    // the most useful summary for the common case (plain POJO error
    // shapes like `{code: 42, message: "..."}`). If stringify rejects
    // (cycles, BigInt fields, custom toJSON throwing) fall back to
    // the `[object Foo]` tag from Object.prototype.toString.call.
    if let Some(json) = json_stringify_safely(ctx, v) {
        return truncate_for_display(&json, 300);
    }
    object_prototype_tag(ctx, v).unwrap_or_else(|| "[object ?]".to_owned())
}

/// Call `String(v)` via the JS global — works for any value type,
/// including symbols and BigInts (where direct `.to_string()` is
/// awkward). Returns `None` if the call itself faults.
fn coerce_to_string<'js>(ctx: &rquickjs::Ctx<'js>, v: &Value<'js>) -> Option<String> {
    let string_fn: Function<'_> = ctx.globals().get("String").ok()?;
    let result: rquickjs::String<'_> = string_fn.call((v.clone(),)).ok()?;
    result.to_string().ok()
}

/// Call `Object.prototype.toString.call(v)` to get the canonical
/// `"[object Foo]"` tag — works on any object, never throws.
fn object_prototype_tag<'js>(ctx: &rquickjs::Ctx<'js>, v: &Value<'js>) -> Option<String> {
    let to_string: Function<'_> = ctx.eval("Object.prototype.toString").ok()?;
    let result: rquickjs::String<'_> = to_string.call((v.clone(),)).ok()?;
    result.to_string().ok()
}

/// Call `JSON.stringify(v)` safely — if stringify throws (cycle,
/// BigInt field, custom toJSON throwing) or returns undefined, we
/// return None and the caller falls back to a [object Foo] tag.
///
/// Why we re-resolve `JSON.stringify` each time rather than caching:
/// the cost (one global get + one prop get) is negligible compared
/// to the page-script bug we're diagnosing, and going through the
/// globals every time means a page that monkey-patches
/// `JSON.stringify` still gets honored (matches Node.js's behavior).
fn json_stringify_safely<'js>(ctx: &rquickjs::Ctx<'js>, v: &Value<'js>) -> Option<String> {
    let json: rquickjs::Object<'_> = ctx.globals().get("JSON").ok()?;
    let stringify: Function<'_> = json.get("stringify").ok()?;
    // CatchResultExt would let us swallow the throw cleanly, but
    // we're already inside `Ctx::with`; just rely on `.ok()` to
    // drop the error.
    let result: Value<'_> = stringify.call((v.clone(),)).ok()?;
    if result.is_undefined() {
        return None;
    }
    let s = result.as_string()?;
    s.to_string().ok()
}

/// Read a function's `.name` property (the standard ES spec field
/// every named function has). Returns the empty string for anonymous
/// functions / arrow expressions / classes without a binding name.
fn read_function_name<'js>(v: &Value<'js>) -> String {
    let Some(obj) = v.as_object() else {
        return String::new();
    };
    obj.get::<_, String>("name").ok().unwrap_or_default()
}

/// Truncate `s` to at most `max_chars` characters (by Unicode scalar,
/// not bytes — `String::truncate` would panic mid-grapheme on
/// `s.len() > max_chars && s.is_char_boundary(max_chars) == false`).
/// Appends "…(truncated, N more chars)" when truncation occurs so
/// the user knows the value was longer.
fn truncate_for_display(s: &str, max_chars: usize) -> String {
    let total: usize = s.chars().count();
    if total <= max_chars {
        return s.to_owned();
    }
    let kept: String = s.chars().take(max_chars).collect();
    let dropped = total - max_chars;
    format!("{kept}… (truncated, {dropped} more chars)")
}

/// Format an exception captured from a `<script>` into a readable
/// single-line console message. Stack is included on a second line
/// when present.
fn format_script_error(message: &str, stack: Option<String>) -> String {
    match stack {
        Some(s) if !s.trim().is_empty() => format!("{message}\n{s}"),
        _ => message.to_owned(),
    }
}

/// Append one entry to the shared console buffer. Single-argument
/// helper that matches the `[args]` shape `console.*` calls produce
/// from JS — receipt consumers can treat both alike.
fn push_console(buffer: &Arc<Mutex<Vec<ConsoleEntry>>>, level: ConsoleLevel, message: String) {
    if let Ok(mut buf) = buffer.lock() {
        buf.push(ConsoleEntry {
            level,
            args: vec![serde_json::Value::String(message)],
        });
    }
}

/// Append one [`ScriptFailure`] to the shared failures buffer. Used
/// by every per-script failure path (inline crash, external crash,
/// fetch error, import-map parse error) so the agent-facing
/// `failed_scripts` array carries the same data structure regardless
/// of which step failed.
///
/// Lock-poison-safe in the same shape as [`push_console`] — a
/// poisoned mutex (extremely rare; happens only if another thread
/// panicked while holding it) is silently swallowed because losing
/// one failure record is strictly better than aborting the pump.
fn push_failure(buffer: &Arc<Mutex<Vec<ScriptFailure>>>, failure: ScriptFailure) {
    if let Ok(mut buf) = buffer.lock() {
        buf.push(failure);
    }
}

/// Try to recover a 1-indexed line number from an exception message
/// in the shape [`format_script_error`] produces. The QuickJS stack
/// format that ships through `exc.stack()` is one of:
///
/// ```text
///     at <eval> (eval_script.js:3)
///     at fn (eval_script.js:7:5)
/// ```
///
/// The simplest extraction that handles both shapes: find the FIRST
/// `:NNN` sequence after `eval_script.js` (or `<eval>`) in the second
/// line of the message — which is where [`format_script_error`]
/// puts the stack. Returns `None` if no match — the agent still gets
/// the message, just without a structured line hint.
fn extract_line_from_message(msg: &str) -> Option<u32> {
    // Look for a `eval_script.js:NN` substring; that's where QuickJS
    // emits the line for an anonymous eval. We accept either the
    // `:LINE` or `:LINE:COL` form.
    let marker = "eval_script.js:";
    let start = msg.find(marker)?;
    let after = &msg[start + marker.len()..];
    // Take leading digits up to `:` / newline / space / `)`.
    let digits: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_runnable_classifies_missing_and_empty_as_classic() {
        assert!(is_runnable_script_type(None));
        assert!(is_runnable_script_type(Some("")));
        assert!(is_runnable_script_type(Some("   ")));
    }

    #[test]
    fn is_runnable_accepts_canonical_javascript_mimes() {
        assert!(is_runnable_script_type(Some("text/javascript")));
        assert!(is_runnable_script_type(Some("application/javascript")));
        assert!(is_runnable_script_type(Some("application/ecmascript")));
        assert!(is_runnable_script_type(Some("text/x-javascript")));
    }

    #[test]
    fn is_runnable_accepts_module_case_insensitive() {
        assert!(is_runnable_script_type(Some("module")));
        assert!(is_runnable_script_type(Some("MODULE")));
        assert!(is_runnable_script_type(Some("Module")));
    }

    #[test]
    fn is_runnable_rejects_data_block_types() {
        assert!(!is_runnable_script_type(Some("application/json")));
        assert!(!is_runnable_script_type(Some("application/ld+json")));
        assert!(!is_runnable_script_type(Some("importmap")));
        assert!(!is_runnable_script_type(Some("speculationrules")));
        assert!(!is_runnable_script_type(Some("text/html")));
    }

    #[test]
    fn is_runnable_rejects_mime_with_parameters() {
        // Per the spec essence-match rule: any parameter (e.g.
        // ;charset=utf-8) disqualifies. MDN/Rocket-Validator note this
        // explicitly.
        assert!(!is_runnable_script_type(Some(
            "text/javascript; charset=utf-8"
        )));
        assert!(!is_runnable_script_type(Some(
            "text/javascript;charset=utf-8"
        )));
    }

    #[test]
    fn is_runnable_trims_whitespace_around_type() {
        assert!(is_runnable_script_type(Some("  text/javascript  ")));
        assert!(is_runnable_script_type(Some(" module ")));
    }

    #[test]
    fn collect_scripts_walks_in_document_order() {
        let doc = dom_query::Document::from(
            r#"<html><body>
                <script>var a = 1;</script>
                <div>
                    <script>var b = 2;</script>
                </div>
                <script>var c = 3;</script>
            </body></html>"#,
        );
        let scripts = collect_scripts(&doc);
        // Three inline scripts, in source-order top-to-bottom even
        // though one is nested.
        assert_eq!(scripts.len(), 3);
        let sources: Vec<String> = scripts
            .into_iter()
            .map(|s| match s.kind {
                ScriptKind::InlineClassic { source } => source,
                _ => "other".into(),
            })
            .collect();
        assert!(sources[0].contains("a = 1"));
        assert!(sources[1].contains("b = 2"));
        assert!(sources[2].contains("c = 3"));
    }

    #[test]
    fn collect_scripts_classifies_external_and_data_blocks() {
        let doc = dom_query::Document::from(
            r#"<html><body>
                <script src="/app.js"></script>
                <script type="application/ld+json">{"@context":"x"}</script>
                <script>console.log('inline')</script>
            </body></html>"#,
        );
        let scripts = collect_scripts(&doc);
        assert_eq!(scripts.len(), 3);
        assert!(matches!(
            scripts[0].kind,
            ScriptKind::ExternalClassic { .. }
        ));
        assert!(matches!(scripts[1].kind, ScriptKind::NonScriptType));
        assert!(matches!(scripts[2].kind, ScriptKind::InlineClassic { .. }));
    }

    #[test]
    fn collect_scripts_classifies_module_inline_and_external() {
        // The new item M-A surface — module scripts get their own
        // variants so the runner can route them through
        // [`Module::evaluate`] rather than `ctx.eval`.
        let doc = dom_query::Document::from(
            r#"<html><body>
                <script type="module">export const x = 1;</script>
                <script type="module" src="/m.js"></script>
                <script>var c = 1;</script>
                <script src="/c.js"></script>
            </body></html>"#,
        );
        let scripts = collect_scripts(&doc);
        assert_eq!(scripts.len(), 4);
        assert!(matches!(scripts[0].kind, ScriptKind::InlineModule { .. }));
        assert!(matches!(scripts[1].kind, ScriptKind::ExternalModule { .. }));
        assert!(matches!(scripts[2].kind, ScriptKind::InlineClassic { .. }));
        assert!(matches!(
            scripts[3].kind,
            ScriptKind::ExternalClassic { .. }
        ));
    }

    #[test]
    fn is_module_script_type_recognizes_module_only() {
        assert!(is_module_script_type(Some("module")));
        assert!(is_module_script_type(Some("MODULE")));
        assert!(is_module_script_type(Some("Module")));
        assert!(is_module_script_type(Some("  module  ")));
        // Classic-MIME types are NOT modules.
        assert!(!is_module_script_type(Some("text/javascript")));
        assert!(!is_module_script_type(Some("application/javascript")));
        // Missing/empty types are classic, not modules.
        assert!(!is_module_script_type(None));
        assert!(!is_module_script_type(Some("")));
        assert!(!is_module_script_type(Some("   ")));
        // Data-block types are not modules either.
        assert!(!is_module_script_type(Some("application/json")));
        assert!(!is_module_script_type(Some("importmap")));
    }

    #[test]
    fn resolve_script_src_handles_absolute_and_relative() {
        // Absolute src — returns as-is (relative join still produces
        // the absolute form back).
        let base = url::Url::parse("https://example.com/foo/").unwrap();
        assert_eq!(
            resolve_script_src("https://other.example/bar.js", Some(&base)),
            "https://other.example/bar.js",
        );
        // Site-absolute (/path) — joined against the host root.
        assert_eq!(
            resolve_script_src("/chunk.js", Some(&base)),
            "https://example.com/chunk.js",
        );
        // Relative — joined against the base directory.
        assert_eq!(
            resolve_script_src("dep.js", Some(&base)),
            "https://example.com/foo/dep.js",
        );
        // Without a base, raw `src` is returned unchanged (matches
        // the contract `fetch_script_source` uses internally).
        assert_eq!(resolve_script_src("/x.js", None), "/x.js");
    }

    #[test]
    fn truncate_for_display_passthroughs_short_strings() {
        // A string within the cap is returned verbatim — no marker,
        // no character loss.
        assert_eq!(truncate_for_display("hi", 100), "hi");
        assert_eq!(truncate_for_display("", 100), "");
    }

    #[test]
    fn truncate_for_display_clips_long_strings_with_marker() {
        // A string over the cap is sliced to exactly `max_chars` and
        // gains a "(truncated, N more chars)" marker. The marker
        // gives the user a concrete sense of how much was dropped.
        let long: String = "X".repeat(500);
        let truncated = truncate_for_display(&long, 200);
        assert!(
            truncated.starts_with(&"X".repeat(200)),
            "first 200 chars must be preserved verbatim"
        );
        assert!(
            truncated.contains("truncated, 300 more chars"),
            "marker must include the exact dropped-char count; got: {truncated}"
        );
    }

    #[test]
    fn truncate_for_display_handles_unicode_at_boundary() {
        // `String::truncate` would panic if the byte boundary fell
        // mid-grapheme; we use `.chars()` to clip on scalar values.
        // Three 4-byte scalars and a cap of 2 should keep the first
        // two scalars and drop the third — never panic.
        let s = "𝓐𝓑𝓒"; // three 4-byte UTF-8 sequences
        let truncated = truncate_for_display(s, 2);
        assert!(
            truncated.starts_with("𝓐𝓑"),
            "first two unicode scalars must be preserved; got: {truncated}"
        );
        assert!(
            truncated.contains("1 more chars"),
            "dropped count must be in scalar units, not bytes; got: {truncated}"
        );
    }

    /// `document.currentScript` lifecycle on a clean engine: after the
    /// pump runs an inline classic script, it observes a non-null
    /// synthetic; after the pump exits, the user's eval sees `null`.
    ///
    /// This is the core regression for the Turbopack
    /// `"chunk path empty but not in a worker"` throw — though the
    /// full end-to-end Turbopack-shaped runtime lives in
    /// `tests/current_script.rs`.
    #[test]
    fn pump_initializes_current_script_to_null_and_restores_after() {
        let engine = crate::JsEngine::new().expect("engine builds");
        // No scripts in the page — pump still runs and sets
        // currentScript to null (the spec value), not undefined.
        let html = "<!doctype html><html><body></body></html>";
        let out = engine
            .eval_with_html_policy(
                html,
                "[typeof document.currentScript, document.currentScript === null]",
                ScriptFetchPolicy::Skip,
            )
            .expect("eval ok");
        // After pump: currentScript exists and is null (not undefined).
        assert_eq!(out.value, serde_json::json!(["object", true]));
    }

    /// Regression for bug-report 03 P0: classic `<script>` bodies run
    /// in **sloppy mode** by default. A bare top-level assignment to
    /// a non-declared identifier (`RLCONF = {...}` — what MediaWiki's
    /// ResourceLoader emits on Wikipedia; `require = function(){}` —
    /// what Apple's browserify-style UMD bundles emit) must create a
    /// global property, NOT throw `ReferenceError: <name> is not
    /// defined`.
    ///
    /// Before fix: `ctx.eval(...)` ran with `EvalOptions { strict: true,
    /// .. }` (rquickjs's default) and rejected the bare assignment.
    /// After fix: `eval_one` routes through `ctx.eval_with_options(...,
    /// EvalOptions { strict: false, .. })` so the spec-mandated classic
    /// sloppy semantics apply.
    #[test]
    fn classic_script_runs_in_sloppy_mode_bare_assign_succeeds() {
        let engine = crate::JsEngine::new().expect("engine builds");
        // Wikipedia-shape: `RLCONF = {x: 1}` at top level. In strict
        // mode this throws "RLCONF is not defined"; in sloppy mode it
        // creates `globalThis.RLCONF = {x:1}` (the historic
        // implicit-global rule).
        let html = r#"<!doctype html><html><body>
            <script>RLCONF = {x: 1}; window.RLCONF = RLCONF;</script>
        </body></html>"#;
        let out = engine
            .eval_with_html_policy(
                html,
                "[typeof RLCONF, RLCONF && RLCONF.x, typeof window.RLCONF]",
                ScriptFetchPolicy::Skip,
            )
            .expect("eval ok");
        assert_eq!(
            out.value,
            serde_json::json!(["object", 1, "object"]),
            "RLCONF must have been created as a global by the sloppy-mode <script>"
        );
        // No console-error entries — strict mode would have produced
        // a "RLCONF is not defined" error.
        let any_strict_error = out
            .console
            .iter()
            .any(|c| matches!(c.level, ConsoleLevel::Error));
        assert!(
            !any_strict_error,
            "no console.error expected from sloppy-mode classic script; got console: {:?}",
            out.console
        );
        // And no script-failure either.
        let failures = engine.script_failures_snapshot();
        assert!(
            failures.is_empty(),
            "no script failures expected; got: {failures:?}"
        );
    }

    /// Complement to the previous test: ES modules MUST stay strict
    /// per ECMA-262 §16.2.2 even after the classic-script sloppy fix.
    /// Same bare-assign body inside `<script type="module">` must
    /// reject with a ReferenceError — observable as `RLCONF` and
    /// `window.RLCONF` both staying `undefined` (the assignment
    /// throws before reaching the `window.RLCONF = ...` line).
    ///
    /// Note: a top-level module throw rejects the Promise that
    /// `Module::evaluate` returned rather than throwing synchronously;
    /// the current `eval_one_module` only routes the synchronous-throw
    /// path into `script_failures`, so we assert the observable
    /// state-of-the-globals here rather than the failure-bucket
    /// contents. The important property the test guards against is
    /// *strict-mode regression* — if modules ever flipped to sloppy,
    /// `RLCONF` would become a defined global on the assignment line
    /// and this `["undefined", "undefined"]` assert would fail loudly.
    #[test]
    fn module_script_stays_strict_bare_assign_errors() {
        let engine = crate::JsEngine::new().expect("engine builds");
        let html = r#"<!doctype html><html><body>
            <script type="module">RLCONF = {x: 1}; window.RLCONF = RLCONF;</script>
        </body></html>"#;
        let out = engine
            .eval_with_html_policy(
                html,
                "[typeof RLCONF, typeof window.RLCONF]",
                ScriptFetchPolicy::Skip,
            )
            .expect("eval ok");
        // RLCONF must NOT have been created — module bodies are strict.
        assert_eq!(
            out.value,
            serde_json::json!(["undefined", "undefined"]),
            "module body's bare assignment must have thrown — RLCONF must NOT be a global"
        );
    }
}
