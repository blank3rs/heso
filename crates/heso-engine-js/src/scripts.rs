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
//!   "execute a script". Three deliberate Phase-1C simplifications:
//!   - `<script type="module">` is treated as a regular classic script.
//!     Real ES-module loading + cyclic dependency resolution is out of
//!     scope for now.
//!   - `defer` and `async` are ignored — every recognized script runs
//!     synchronously in document order, like jsdom's basic mode.
//!   - External `src=...` either errors or is skipped with a console
//!     warning, gated by [`ScriptFetchPolicy`]. Fetching real subresources
//!     is item C in the next-phase plan (vendor `llrt_fetch`).
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

use rquickjs::{CatchResultExt, CaughtError, Context, Value};

use crate::engine::{ConsoleEntry, ConsoleLevel, EvalError};

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
pub fn run_scripts(
    context: &Context,
    document: &dom_query::Document,
    policy: ScriptFetchPolicy,
    console_buffer: &Arc<Mutex<Vec<ConsoleEntry>>>,
    fetch_client: Option<&(Arc<reqwest::Client>, tokio::runtime::Handle)>,
) -> Result<ScriptOutcome, EvalError> {
    let scripts = collect_scripts(document);
    let mut outcome = ScriptOutcome::default();

    for script in scripts {
        match script.kind {
            ScriptKind::Inline { source } => match eval_one(context, &source)? {
                Some(err_msg) => {
                    outcome.executed_with_error += 1;
                    push_console(console_buffer, ConsoleLevel::Error, err_msg);
                }
                None => {
                    outcome.executed += 1;
                }
            },
            ScriptKind::External { src } => {
                outcome.external_handled += 1;
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
                            format!(
                                "heso: external script fetch disabled. Wanted <script src=\"{src}\">"
                            ),
                        );
                    }
                    (ScriptFetchPolicy::Fetch, Some((client, rt))) => {
                        // Synchronous-blocking fetch + execute, matching
                        // jsdom's basic mode. Failures land on the
                        // console buffer; the pump continues.
                        match fetch_script_source(client, rt, &src) {
                            Ok(source) => match eval_one(context, &source)? {
                                Some(err_msg) => {
                                    outcome.executed_with_error += 1;
                                    push_console(
                                        console_buffer,
                                        ConsoleLevel::Error,
                                        format!("<script src=\"{src}\"> threw: {err_msg}"),
                                    );
                                }
                                None => {
                                    outcome.executed += 1;
                                }
                            },
                            Err(e) => {
                                push_console(
                                    console_buffer,
                                    ConsoleLevel::Error,
                                    format!("heso: <script src=\"{src}\"> fetch failed: {e}"),
                                );
                            }
                        }
                    }
                    (ScriptFetchPolicy::Fetch, None) => {
                        // Caller asked for Fetch policy but the engine
                        // doesn't have a client — surface a clear
                        // diagnostic so the agent can fix its config.
                        push_console(
                            console_buffer,
                            ConsoleLevel::Error,
                            format!(
                                "heso: <script src=\"{src}\"> wanted Fetch policy but engine has no fetch client (build with JsEngine::new_with_fetch)"
                            ),
                        );
                    }
                }
            }
            ScriptKind::NonScriptType => {
                outcome.skipped_non_script_type += 1;
            }
        }
    }

    Ok(outcome)
}

/// Synchronously fetch `src` via the shared `reqwest::Client`. Used
/// by [`ScriptFetchPolicy::Fetch`] to honor `<script src=...>` refs
/// during page hydration.
///
/// `src` is expected to be an absolute URL (or anything `reqwest`
/// can parse as one); relative URLs are not yet resolved against the
/// page's base URL — that's a documented limitation of the Phase 1C
/// pump. Callers that need relative-URL support should pre-resolve
/// against `final_url` before passing the HTML to the engine.
fn fetch_script_source(
    client: &reqwest::Client,
    rt: &tokio::runtime::Handle,
    src: &str,
) -> Result<String, String> {
    // `block_in_place` lets the CLI's `#[tokio::main]` flow run this
    // synchronously without tripping the "runtime from within a
    // runtime" panic — same trick as `crate::fetch::perform_request`.
    tokio::task::block_in_place(|| {
        rt.block_on(async {
            let resp = client
                .get(src)
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
    /// Inline `<script>...source...</script>`.
    Inline { source: String },
    /// `<script src="..."></script>` — content is at the URL.
    External { src: String },
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
        //    - "module" (ASCII case-insensitive) → module (we treat as
        //      classic; see module-punt note in the file header)
        //    - anything else (incl. "application/json", "importmap",
        //      "speculationrules", "text/html") → data block / null →
        //      not executed.
        let type_attr = descendant.attr("type").map(|s| s.to_string());
        if !is_runnable_script_type(type_attr.as_deref()) {
            out.push(ClassifiedScript {
                kind: ScriptKind::NonScriptType,
            });
            continue;
        }

        // 2. Inline vs external. Per jsdom's `_eval()`:
        //    `if (hasAttribute("src")) fetchExternalScript(); else
        //    fetchInternalScript();`
        if let Some(src) = descendant.attr("src") {
            // Empty src — happens with `<script src=""></script>` in
            // the wild. WHATWG says treat as if no src for the
            // classification step, then the empty-URL fetch fails. We
            // simplify: empty src → External("") and let the policy
            // surface its standard warning.
            out.push(ClassifiedScript {
                kind: ScriptKind::External {
                    src: src.to_string(),
                },
            });
            continue;
        }

        let source = descendant.text().to_string();
        out.push(ClassifiedScript {
            kind: ScriptKind::Inline { source },
        });
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

/// Evaluate one script's source against `context`. Returns
/// `Ok(None)` if the script ran without throwing, `Ok(Some(msg))` if
/// it threw a recoverable JS exception (the caller turns this into a
/// `console.error`), `Err(_)` only for engine-internal failures.
fn eval_one(context: &Context, source: &str) -> Result<Option<String>, EvalError> {
    context.with(|ctx| -> Result<Option<String>, EvalError> {
        match ctx.eval::<Value, _>(source).catch(&ctx) {
            Ok(_) => Ok(None),
            Err(CaughtError::Exception(exc)) => {
                let msg = match exc.message() {
                    Some(m) if !m.is_empty() => m,
                    _ => "<unknown script exception>".to_owned(),
                };
                Ok(Some(format_script_error(&msg, exc.stack())))
            }
            Err(CaughtError::Value(_)) => Ok(Some("<script threw non-error value>".to_owned())),
            Err(CaughtError::Error(e)) => {
                // Genuine engine error (OOM, alloc failure) — abort
                // the pump. The console-error-and-continue rule is
                // for *script* failures only.
                Err(EvalError::Engine(format!("script eval: {e}")))
            }
        }
    })
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
                ScriptKind::Inline { source } => source,
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
        assert!(matches!(scripts[0].kind, ScriptKind::External { .. }));
        assert!(matches!(scripts[1].kind, ScriptKind::NonScriptType));
        assert!(matches!(scripts[2].kind, ScriptKind::Inline { .. }));
    }
}
