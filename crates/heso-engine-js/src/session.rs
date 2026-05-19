//! # session
//!
//! Stateful page sessions: one [`JsEngine`], one [`Document`], one
//! [`Url`], persisted across many actions.
//!
//! The Phase 1B convenience methods on [`JsEngine`] —
//! [`JsEngine::dispatch_click`], [`JsEngine::set_input_value`],
//! [`JsEngine::submit_form`] — re-parse HTML and reinstall the
//! `document` global on **every** call. That is the right contract
//! for the one-shot CLI verbs `heso click` / `heso fill` /
//! `heso submit`: each invocation is its own short-lived process and
//! sees the page fresh.
//!
//! It is the wrong contract for a multi-step trace where an earlier
//! click mutates the page that a later step expects to operate on
//! (`cmd_replay` in `heso-cli`). The first pass of `cmd_replay`
//! shipped with `JsEngine::new()` per step + a re-fetch between
//! steps, which threw away every JS-side mutation in between.
//!
//! [`JsSession`] is the fix. It:
//!
//! - Installs the `document` global once at [`Self::open`] time via
//!   [`JsEngine::install_document`].
//! - Routes [`Self::click`] / [`Self::fill`] / [`Self::submit`] /
//!   [`Self::eval`] through [`JsEngine::eval`] against that same
//!   long-lived global — no re-parse, no reinstall, mutations
//!   accumulate.
//! - Exposes [`Self::navigate`] for genuine page transitions (an
//!   `<a href>` click that should navigate, or an explicit
//!   `Action::Open`) — the only path that *should* swap the document.
//!
//! Cookies (via the shared `reqwest::Client`), timers, RNG, and the
//! `fetch` shim all live on the underlying [`JsEngine`] and survive
//! [`Self::navigate`] as well — matching real-browser behavior where
//! only the document resets on navigation.
//!
//! ## Listener persistence
//!
//! Event listeners registered via `addEventListener` survive across
//! [`Self::click`] / [`Self::fill`] / [`Self::eval`] calls because
//! [`crate::events`] keys them by `dom_query::NodeId` on a registry
//! attached to the long-lived `document` global, not on per-call
//! Element wrappers. This is the property that makes
//! `addEventListener('click', …)` patterns work — including
//! React-style synthetic-event delegation in principle, though the
//! synthetic-event system itself isn't wired yet.

use url::Url;

use crate::dom::Document;
use crate::engine::{EvalError, EvalOutcome, JsEngine};
use crate::form_submit::{
    build_apply_fields_js, build_snapshot_js, issue_request, live_fetch_handle, FormSnapshot,
    SubmitResponse, SubmitSkip,
};
use crate::scripts::{ScriptFetchPolicy, ScriptOutcome};

/// Cap on `responseBody` size returned inside `submit`'s outcome value.
/// Keeps the JSON payload manageable when an agent submits a form
/// against a server that responds with a full HTML page. When the
/// response exceeds this, `responseBody` is truncated to the cap and
/// `responseBodyTruncated` is set to `true` so callers know to expect a
/// follow-up `eval-dom` if they need the full content.
const RESPONSE_BODY_TRUNCATE_BYTES: usize = 64 * 1024;

/// JS snippet that binds `submitter` (in the enclosing scope) to the
/// first submit-typed descendant of `form`, or `null` if none. Shared
/// by [`JsSession::submit`] and [`JsEngine::submit_form`] so the
/// fallback chain (`button[type="submit"]` → `input[type="submit"]` →
/// `button:not([type])`) stays defined in exactly one place.
///
/// The trailing `<button>` clause matches the HTML spec's "missing
/// type attribute default" for `<button>` — implicit type is `submit`.
pub(crate) const SUBMIT_DESCENDANT_FINDER_JS: &str = r#"
const submitter =
    form.querySelector('button[type="submit"]') ||
    form.querySelector('input[type="submit"]') ||
    form.querySelector('button:not([type])');
"#;

/// A long-lived page session bound to a single [`JsEngine`].
///
/// Construct with [`Self::open`]; advance with [`Self::click`] /
/// [`Self::fill`] / [`Self::submit`] / [`Self::eval`]; transition to a
/// new page with [`Self::navigate`]. The current URL is tracked
/// independently of the engine — JS has no way to mutate it because
/// `window.location` is not wired yet.
pub struct JsSession {
    engine: JsEngine,
    url: Url,
    // Rust-side handle to the same `Arc<dom_query::Document>` the JS
    // `document` global wraps. Lets us serialize the post-mutation DOM
    // (see `Self::document_html`) without a JS round-trip.
    document: Document,
}

impl JsSession {
    /// Open `html` at `url` as a fresh session. Builds a default
    /// [`JsEngine`] (no seed, no fetch shim), parses `html`, installs
    /// the resulting [`Document`] as the JS `document` global, and
    /// runs every inline `<script>` once.
    pub fn open(html: &str, url: Url) -> Result<(Self, ScriptOutcome), EvalError> {
        let engine = JsEngine::new()?;
        Self::open_on_engine(engine, html, url, ScriptFetchPolicy::default())
    }

    /// Like [`Self::open`] but seeds the engine's RNG and virtual
    /// clock so `Math.random` / `crypto.*` / `setTimeout` ordering is
    /// reproducible across runs.
    pub fn open_with_seed(
        html: &str,
        url: Url,
        seed: u64,
    ) -> Result<(Self, ScriptOutcome), EvalError> {
        let engine = JsEngine::new_with_seed(seed)?;
        Self::open_on_engine(engine, html, url, ScriptFetchPolicy::default())
    }

    /// Lowest-level constructor: caller supplies a fully-built
    /// [`JsEngine`] (so they can pre-attach a fetch shim, custom RNG,
    /// or a non-default memory cap) plus the
    /// [`ScriptFetchPolicy`] for inline `<script src=...>` references.
    pub fn open_on_engine(
        engine: JsEngine,
        html: &str,
        url: Url,
        policy: ScriptFetchPolicy,
    ) -> Result<(Self, ScriptOutcome), EvalError> {
        // Set the base URL before installing so the inline-script
        // pump can resolve relative `<script src="...">` references.
        engine.set_base_url(Some(url.clone()));
        let document = Document::from_html(html);
        let outcome = engine.install_document(document.clone(), policy)?;
        Ok((
            Self {
                engine,
                url,
                document,
            },
            outcome,
        ))
    }

    /// Replace the page: parse `html`, swap in a fresh [`Document`]
    /// as the `document` global, run its `<script>` tags. The engine
    /// itself (and therefore RNG / virtual clock / cookies via
    /// `fetch_state`) survives — only the DOM resets, matching
    /// browser behavior on real navigation.
    pub fn navigate(&mut self, html: &str, url: Url) -> Result<ScriptOutcome, EvalError> {
        self.navigate_with_policy(html, url, ScriptFetchPolicy::default())
    }

    /// [`Self::navigate`] with a caller-chosen [`ScriptFetchPolicy`].
    pub fn navigate_with_policy(
        &mut self,
        html: &str,
        url: Url,
        policy: ScriptFetchPolicy,
    ) -> Result<ScriptOutcome, EvalError> {
        // Re-point base URL before installing so relative
        // `<script src>` refs resolve against the NEW page.
        self.engine.set_base_url(Some(url.clone()));
        let document = Document::from_html(html);
        let outcome = self.engine.install_document(document.clone(), policy)?;
        self.document = document;
        self.url = url;
        Ok(outcome)
    }

    /// The session's current URL. Updated by [`Self::navigate`];
    /// not currently observable to JS.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Borrow the [`JsEngine`] backing this session.
    pub fn engine(&self) -> &JsEngine {
        &self.engine
    }

    /// Borrow the current document. Mutations made by JS are visible
    /// here because both sides share the same `Arc<dom_query::Document>`.
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// Serialize the current DOM back to HTML. Reflects every
    /// mutation JS has made since [`Self::open`] / the last
    /// [`Self::navigate`].
    pub fn document_html(&self) -> String {
        self.document.dom().html().to_string()
    }

    /// Dispatch a `click` event on the element matched by `selector`.
    ///
    /// Unlike [`JsEngine::dispatch_click`], no HTML is re-parsed and
    /// no `document` global is reinstalled — DOM mutations made by
    /// the click handler are observable on the next call.
    ///
    /// Returns an [`EvalOutcome`] whose `value` is the JSON object
    /// `{matched: bool, defaultPrevented: bool}`. `matched` is `false`
    /// when the selector found no element (and `defaultPrevented` is
    /// always `false` in that case). Otherwise the click is dispatched
    /// via `dispatchEvent` of a cancelable `click` Event so callers
    /// can observe whether a listener called `event.preventDefault()`.
    pub fn click(&self, selector: &str) -> Result<EvalOutcome, EvalError> {
        let selector_lit = serde_json::to_string(selector)
            .map_err(|e| EvalError::Engine(format!("encode selector: {e}")))?;
        let script = format!(
            r#"
            (() => {{
                const el = document.querySelector({selector_lit});
                if (!el) return {{ matched: false, defaultPrevented: false }};
                const ev = new Event('click', {{ bubbles: true, cancelable: true }});
                el.dispatchEvent(ev);
                return {{ matched: true, defaultPrevented: ev.defaultPrevented }};
            }})()
            "#,
        );
        self.engine.eval(&script)
    }

    /// Set the input's value and dispatch `input` then `change`
    /// events on it. Stateful counterpart of
    /// [`JsEngine::set_input_value`].
    ///
    /// Returns `{matched: bool, defaultPrevented: bool}` — `matched`
    /// is `false` if no element matched. `defaultPrevented` reports
    /// whether `preventDefault()` was called during the `input` or
    /// `change` dispatch (true if either was prevented).
    pub fn fill(&self, selector: &str, value: &str) -> Result<EvalOutcome, EvalError> {
        let selector_lit = serde_json::to_string(selector)
            .map_err(|e| EvalError::Engine(format!("encode selector: {e}")))?;
        let value_lit = serde_json::to_string(value)
            .map_err(|e| EvalError::Engine(format!("encode value: {e}")))?;
        let script = format!(
            r#"
            (() => {{
                const el = document.querySelector({selector_lit});
                if (!el) return {{ matched: false, defaultPrevented: false }};
                el.value = {value_lit};
                const inp = new Event('input', {{ bubbles: true, cancelable: true }});
                el.dispatchEvent(inp);
                const chg = new Event('change', {{ bubbles: true, cancelable: true }});
                el.dispatchEvent(chg);
                return {{
                    matched: true,
                    defaultPrevented: inp.defaultPrevented || chg.defaultPrevented,
                }};
            }})()
            "#,
        );
        self.engine.eval(&script)
    }

    /// Submit the form at `selector` per [WHATWG HTML §4.10.22]
    /// [spec] — dispatch the `submit` event, serialize the entry list,
    /// issue a real HTTP request through the engine's shared
    /// `reqwest::Client`, and replace this session's document with the
    /// response body.
    ///
    /// Replaces the pre-PR-1 dispatch-only path that was filed as the
    /// single biggest agent-write bug in `AGENT_FINDINGS.md` (every
    /// `heso submit` returned `ok=true` but issued no HTTP traffic).
    ///
    /// Behavior summary:
    ///
    /// - **No matching form / no submitter** → returns
    ///   `{matched: false, defaultPrevented: false, submitted: false}`
    ///   without firing the submit event.
    /// - **Listener called `event.preventDefault()`** → returns
    ///   `{matched: true, defaultPrevented: true, submitted: false}`
    ///   without issuing the request. Real-browser parity.
    /// - **Engine has no `FetchMode::Live` client** (built via
    ///   [`JsEngine::new`] without `new_with_fetch`, or seeded into
    ///   `DeterministicNoCassette`) → returns
    ///   `{matched: true, submitted: false, reason: "no_fetch_client"}`.
    ///   This is the legacy-compatible mode for tests that don't wire
    ///   a real client; the dispatch part still fired.
    /// - **HTTP error** → returns `{matched: true, submitted: false,
    ///   reason: "http_error", error: "..."}` and propagates an
    ///   [`EvalError`] only when the JS-side snapshot itself fails.
    /// - **Success** → issues the request through the shared client,
    ///   navigates this session to the response URL (cookies preserved
    ///   on the underlying engine), and returns the full outcome
    ///   including `responseStatus`, `responseUrl`, `responseBody`
    ///   (truncated to 64 KB with `responseBodyTruncated: true` when
    ///   larger), `responseContentType` (verbatim header), and — when
    ///   the response declares `application/json` — `responseJson`
    ///   (the parsed body so agents don't have to `JSON.parse`
    ///   themselves).
    ///
    /// Enctypes supported: `application/x-www-form-urlencoded`,
    /// `multipart/form-data`, `text/plain`. File inputs in multipart
    /// carry the filename only — `FormData`/`Blob` plumbing is filed
    /// as a follow-up.
    ///
    /// Method handling: GET serializes the entry list to a `?query`
    /// (replacing any existing query) on the action URL. POST sends
    /// the entry list as the request body per enctype. The action URL
    /// resolves against the session's current URL when relative.
    ///
    /// [spec]: https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#form-submission-algorithm
    pub fn submit(&mut self, selector: &str) -> Result<EvalOutcome, EvalError> {
        self.submit_inner(selector, &[])
    }

    /// Apply `(name, value)` field overrides to the form at `selector`,
    /// then submit. Combines `fill`-by-name with `submit` into one
    /// in-process call — fixes the agent UX gap filed as `R2`/`F2` in
    /// `AGENT_FINDINGS_V2.md` ("fill doesn't persist across verbs, so
    /// the typed value never reaches submit"). Field overrides are keyed
    /// by the input's `name` attribute (the WHATWG "successful control"
    /// key), not by `@eN` action-graph ref.
    ///
    /// File inputs are skipped (they can't have their value set from a
    /// string — full file upload lands with `FormData`/`Blob` plumbing,
    /// the next pass).
    ///
    /// Returns the same outcome shape as [`Self::submit`] with an
    /// additional `fieldsApplied` array (names that were actually
    /// touched) and a `fieldsSkipped` array (each entry
    /// `{name, reason}` where reason is `"no_match"` or `"file_input"`).
    pub fn submit_with_fields(
        &mut self,
        selector: &str,
        fields: &[(String, String)],
    ) -> Result<EvalOutcome, EvalError> {
        self.submit_inner(selector, fields)
    }

    /// Core implementation shared by [`Self::submit`] and
    /// [`Self::submit_with_fields`]. When `fields` is non-empty, runs
    /// the apply-fields pass first; the result is stitched into the
    /// final outcome as `fieldsApplied` / `fieldsSkipped`. When empty,
    /// the pre-fill step is skipped entirely (no JS round-trip cost
    /// for the legacy callers).
    fn submit_inner(
        &mut self,
        selector: &str,
        fields: &[(String, String)],
    ) -> Result<EvalOutcome, EvalError> {
        // Phase 0: apply field overrides (when any). Done before the
        // snapshot so the `submit` event fires against the post-fill
        // DOM — listeners that read input.value see the agent's
        // supplied data. Note: the apply pass dispatches `input` /
        // `change` events the same way `JsSession::fill` does, so any
        // validation listener that runs on those is honored.
        let (fields_applied, fields_skipped, fields_matched_form) = if fields.is_empty() {
            (
                serde_json::Value::Null,
                serde_json::Value::Null,
                true, // no overrides → don't second-guess the snapshot
            )
        } else {
            let apply_js = build_apply_fields_js(selector, fields);
            let apply_outcome = self.engine.eval(&apply_js)?;
            let applied = apply_outcome
                .value
                .get("applied")
                .cloned()
                .unwrap_or(serde_json::Value::Array(vec![]));
            let skipped = apply_outcome
                .value
                .get("skipped")
                .cloned()
                .unwrap_or(serde_json::Value::Array(vec![]));
            let matched = apply_outcome
                .value
                .get("matched")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            (applied, skipped, matched)
        };

        // Phase 1: extract the snapshot (dispatches the submit event
        // as a side effect, which is the spec-mandated checkpoint).
        let snapshot_js = build_snapshot_js(selector);
        let snapshot_outcome = self.engine.eval(&snapshot_js)?;
        let snapshot_value = snapshot_outcome.value.clone();

        // Re-deserialize the snapshot into a typed value. `eval` already
        // round-trips through JSON, so this is just a Value → struct hop.
        let snapshot: FormSnapshot = match serde_json::from_value(snapshot_value.clone()) {
            Ok(s) => s,
            Err(e) => {
                // Snapshot wasn't well-shaped (selector miss without
                // the right field, or a typo in the JS template).
                // Return the raw outcome with an explanatory field.
                let mut value = serde_json::json!({
                    "matched": false,
                    "defaultPrevented": false,
                    "submitted": false,
                    "reason": "snapshot_decode_error",
                    "error": e.to_string(),
                    "raw": snapshot_value,
                });
                attach_field_diagnostics(&mut value, &fields_applied, &fields_skipped);
                return Ok(EvalOutcome {
                    value,
                    console: snapshot_outcome.console,
                });
            }
        };

        // If the override pass didn't find the form but the snapshot
        // also didn't, surface that — the selector pointed at nothing.
        let _ = fields_matched_form; // reserved for future surface

        // No match / no submitter / cancelled → no HTTP traffic.
        let outcome = self.classify_skip(&snapshot);
        if let Some(skip) = outcome {
            let mut value = skip_value(&skip, &snapshot);
            attach_field_diagnostics(&mut value, &fields_applied, &fields_skipped);
            return Ok(EvalOutcome {
                value,
                console: snapshot_outcome.console,
            });
        }

        // Phase 2: issue the request. Falls back to a no-network
        // outcome if the engine wasn't built with a fetch client.
        let Some((client, rt_handle)) = live_fetch_handle(&self.engine) else {
            let mut value = serde_json::json!({
                "matched": true,
                "defaultPrevented": false,
                "submitted": false,
                "reason": "no_fetch_client",
                "method": snapshot.method,
                "enctype": snapshot.enctype,
                "action": snapshot.action,
            });
            attach_field_diagnostics(&mut value, &fields_applied, &fields_skipped);
            return Ok(EvalOutcome {
                value,
                console: snapshot_outcome.console,
            });
        };

        let response = match issue_request(&snapshot, &self.url, &client, &rt_handle) {
            Ok(r) => r,
            Err(e) => {
                let mut value = serde_json::json!({
                    "matched": true,
                    "defaultPrevented": false,
                    "submitted": false,
                    "reason": "http_error",
                    "error": e.to_string(),
                    "method": snapshot.method,
                    "enctype": snapshot.enctype,
                    "action": snapshot.action,
                });
                attach_field_diagnostics(&mut value, &fields_applied, &fields_skipped);
                return Ok(EvalOutcome {
                    value,
                    console: snapshot_outcome.console,
                });
            }
        };

        // Phase 3: install the response body as the new document.
        // Navigation reuses the engine, so cookies / RNG / virtual
        // clock survive. Inline scripts in the response page re-run.
        let SubmitResponse {
            final_url,
            body,
            status,
            content_type,
        } = response;
        let _nav_outcome = self.navigate(&body, final_url.clone())?;

        // Clamp the body to the documented cap and surface a flag when
        // we trimmed it. Truncation is byte-counted on a UTF-8 string;
        // we rewind to the nearest char boundary so the result is
        // valid UTF-8 (otherwise serde_json output would round-trip
        // through a lossy fallback in some readers).
        let (body_for_output, truncated) = if body.len() > RESPONSE_BODY_TRUNCATE_BYTES {
            let mut cap = RESPONSE_BODY_TRUNCATE_BYTES;
            while cap > 0 && !body.is_char_boundary(cap) {
                cap -= 1;
            }
            (body[..cap].to_owned(), true)
        } else {
            (body.clone(), false)
        };

        let mut value = serde_json::json!({
            "matched": true,
            "defaultPrevented": false,
            "submitted": true,
            "method": snapshot.method,
            "enctype": snapshot.enctype,
            "action": snapshot.action,
            "responseStatus": status,
            "responseUrl": final_url.as_str(),
            "responseBody": body_for_output,
            "responseBodyTruncated": truncated,
        });
        if let Some(ct) = content_type.as_ref() {
            value["responseContentType"] = serde_json::Value::String(ct.clone());
        }
        // Parse responseJson when the content-type's media component is
        // a JSON type. Per IANA, `application/json` plus the
        // structured-syntax-suffix forms `+json` (e.g.
        // `application/vnd.api+json`) all signal JSON. Truncated bodies
        // are NOT parsed because the JSON would be incomplete; agents
        // can re-fetch if they need the full payload.
        if !truncated {
            if let Some(ct) = content_type.as_ref() {
                if is_json_content_type(ct) {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body) {
                        value["responseJson"] = parsed;
                    }
                }
            }
        }
        attach_field_diagnostics(&mut value, &fields_applied, &fields_skipped);
        Ok(EvalOutcome {
            value,
            console: snapshot_outcome.console,
        })
    }

    /// Decide whether `snapshot` corresponds to a no-network outcome
    /// (selector miss or `preventDefault`). Returns `None` when the
    /// HTTP request should proceed.
    fn classify_skip(&self, snapshot: &FormSnapshot) -> Option<SubmitSkip> {
        if !snapshot.matched {
            return Some(SubmitSkip::NoForm);
        }
        if snapshot.default_prevented {
            return Some(SubmitSkip::DefaultPrevented);
        }
        None
    }

    /// Evaluate arbitrary JS against the live `document` global.
    pub fn eval(&self, code: &str) -> Result<EvalOutcome, EvalError> {
        self.engine.eval(code)
    }
}

/// Build the JSON `value` returned to callers when submission skips
/// the HTTP request (selector miss or `preventDefault`). Mirrors the
/// success/skip vocabulary the `submitted: bool` field documents.
fn skip_value(skip: &SubmitSkip, snapshot: &FormSnapshot) -> serde_json::Value {
    match skip {
        SubmitSkip::NoForm => serde_json::json!({
            "matched": false,
            "defaultPrevented": false,
            "submitted": false,
            "reason": "no_form",
        }),
        SubmitSkip::DefaultPrevented => serde_json::json!({
            "matched": true,
            "defaultPrevented": true,
            "submitted": false,
            "reason": "default_prevented",
            "method": snapshot.method,
            "enctype": snapshot.enctype,
            "action": snapshot.action,
        }),
    }
}

/// Splice `fieldsApplied` / `fieldsSkipped` into `value` when the
/// caller supplied field overrides. No-op when both are `Null`
/// (legacy `submit()` callers — keeps their output shape stable).
fn attach_field_diagnostics(
    value: &mut serde_json::Value,
    applied: &serde_json::Value,
    skipped: &serde_json::Value,
) {
    if applied.is_null() && skipped.is_null() {
        return;
    }
    if let Some(map) = value.as_object_mut() {
        if !applied.is_null() {
            map.insert("fieldsApplied".to_owned(), applied.clone());
        }
        if !skipped.is_null() {
            map.insert("fieldsSkipped".to_owned(), skipped.clone());
        }
    }
}

/// Returns `true` when `content_type` declares a JSON media type. Per
/// IANA ("Structured Syntax Suffix Specifications"), JSON-shaped
/// payloads use either `application/json` (and the rarer
/// `text/json`) or a `+json` suffix on a vendor type like
/// `application/vnd.api+json`. We compare only the media-type prefix
/// (before any `;` parameter list) and lowercase for caseless match.
fn is_json_content_type(content_type: &str) -> bool {
    let media = content_type
        .split(';')
        .next()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    media == "application/json"
        || media == "text/json"
        || media.ends_with("+json")
}


#[cfg(test)]
mod tests {
    use super::*;

    fn test_url() -> Url {
        Url::parse("https://example.com/").unwrap()
    }

    #[test]
    fn click_persists_dom_mutation_for_next_eval() {
        let html = r#"
            <!doctype html><html><body>
              <button id="b">click me</button>
              <div id="out">untouched</div>
              <script>
                document.querySelector('#b').addEventListener('click', () => {
                  document.querySelector('#out').textContent = 'clicked!';
                });
              </script>
            </body></html>
        "#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        let before = sess
            .eval("document.querySelector('#out').textContent")
            .unwrap();
        assert_eq!(before.value, serde_json::json!("untouched"));
        let clicked = sess.click("#b").unwrap();
        assert_eq!(clicked.value["matched"], serde_json::json!(true));
        let after = sess
            .eval("document.querySelector('#out').textContent")
            .unwrap();
        assert_eq!(after.value, serde_json::json!("clicked!"));
    }

    #[test]
    fn fill_persists_value_for_next_eval() {
        let html = r#"<!doctype html><html><body>
            <input id="i" type="text" value="">
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        let filled = sess.fill("#i", "hello").unwrap();
        assert_eq!(filled.value["matched"], serde_json::json!(true));
        let v = sess.eval("document.querySelector('#i').value").unwrap();
        assert_eq!(v.value, serde_json::json!("hello"));
    }

    #[test]
    fn click_then_fill_then_eval_sees_all_mutations() {
        let html = r#"
            <!doctype html><html><body>
              <button id="b">b</button>
              <input id="i" type="text" value="">
              <div id="out"></div>
              <script>
                document.querySelector('#b').addEventListener('click', () => {
                  document.querySelector('#out').textContent = 'clicked';
                });
                document.querySelector('#i').addEventListener('input', (e) => {
                  document.querySelector('#out').textContent += ':' + e.target.value;
                });
              </script>
            </body></html>
        "#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        assert_eq!(sess.click("#b").unwrap().value["matched"], serde_json::json!(true));
        assert_eq!(sess.fill("#i", "x").unwrap().value["matched"], serde_json::json!(true));
        let out = sess
            .eval("document.querySelector('#out').textContent")
            .unwrap();
        assert_eq!(out.value, serde_json::json!("clicked:x"));
    }

    #[test]
    fn navigate_resets_dom_but_keeps_engine_state() {
        let html_a = r#"
            <!doctype html><html><body>
              <div id="x">a</div>
              <script>globalThis.persisted = 'from-a';</script>
            </body></html>
        "#;
        let html_b = r#"<!doctype html><html><body><div id="x">b</div></body></html>"#;
        let (mut sess, _) = JsSession::open(html_a, test_url()).unwrap();
        let a = sess
            .eval("document.querySelector('#x').textContent")
            .unwrap();
        assert_eq!(a.value, serde_json::json!("a"));
        let new_url = Url::parse("https://example.com/b").unwrap();
        sess.navigate(html_b, new_url.clone()).unwrap();
        let b = sess
            .eval("document.querySelector('#x').textContent")
            .unwrap();
        assert_eq!(b.value, serde_json::json!("b"));
        let p = sess.eval("globalThis.persisted").unwrap();
        assert_eq!(p.value, serde_json::json!("from-a"));
        assert_eq!(sess.url(), &new_url);
    }

    #[test]
    fn document_html_reflects_post_click_mutations() {
        let html = r#"<!doctype html><html><body>
            <button id="b">b</button>
            <div id="out">orig</div>
            <script>
              document.querySelector('#b').addEventListener('click', () => {
                document.querySelector('#out').textContent = 'mut';
              });
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        sess.click("#b").unwrap();
        let serialized = sess.document_html();
        assert!(
            serialized.contains("mut"),
            "expected serialized HTML to contain mutation; got: {serialized}"
        );
        assert!(
            !serialized.contains(">orig<"),
            "expected serialized HTML to NOT contain original value; got: {serialized}"
        );
    }

    #[test]
    fn click_returns_false_on_unmatched_selector() {
        let html = "<!doctype html><html><body></body></html>";
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        let res = sess.click("#nope").unwrap();
        assert_eq!(res.value["matched"], serde_json::json!(false));
        assert_eq!(res.value["defaultPrevented"], serde_json::json!(false));
    }

    // -----------------------------------------------------------------
    // Real-world-ish patterns: counter, event delegation,
    // removeEventListener, innerHTML / appendChild / classList mutation
    // from handlers, multi-listener order, setTimeout-scheduled
    // mutations, navigation clearing listeners, preventDefault.
    // -----------------------------------------------------------------

    #[test]
    fn counter_accumulates_across_many_clicks() {
        // The simplest SPA pattern: a click handler increments shared
        // state. After N clicks the DOM should show N.
        let html = r#"<!doctype html><html><body>
            <button id="b">+1</button>
            <span id="n">0</span>
            <script>
              let n = 0;
              document.querySelector('#b').addEventListener('click', () => {
                n += 1;
                document.querySelector('#n').textContent = String(n);
              });
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        for _ in 0..5 {
            assert_eq!(sess.click("#b").unwrap().value["matched"], serde_json::json!(true));
        }
        let n = sess
            .eval("document.querySelector('#n').textContent")
            .unwrap();
        assert_eq!(n.value, serde_json::json!("5"));
    }

    #[test]
    fn event_delegation_on_body_routes_clicks_from_children() {
        // React-style delegation: a listener on a high ancestor handles
        // clicks on children. Bubbling must reach the body listener.
        let html = r#"<!doctype html><html><body>
            <div><button id="b">click</button></div>
            <div id="out">none</div>
            <script>
              document.body.addEventListener('click', (e) => {
                document.querySelector('#out').textContent = 'caught:' + e.target.id;
              });
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        assert_eq!(sess.click("#b").unwrap().value["matched"], serde_json::json!(true));
        let out = sess
            .eval("document.querySelector('#out').textContent")
            .unwrap();
        assert_eq!(out.value, serde_json::json!("caught:b"));
    }

    #[test]
    fn remove_event_listener_actually_removes_across_evals() {
        // addEventListener then removeEventListener with the same
        // callback identity should leave the element with no listener
        // — a subsequent click must NOT fire the handler.
        let html = r#"<!doctype html><html><body>
            <button id="b">b</button>
            <span id="n">0</span>
            <script>
              let n = 0;
              const handler = () => {
                n += 1;
                document.querySelector('#n').textContent = String(n);
              };
              const el = document.querySelector('#b');
              el.addEventListener('click', handler);
              globalThis.__detach = () => el.removeEventListener('click', handler);
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        sess.click("#b").unwrap();
        sess.eval("globalThis.__detach()").unwrap();
        sess.click("#b").unwrap();
        let n = sess
            .eval("document.querySelector('#n').textContent")
            .unwrap();
        assert_eq!(n.value, serde_json::json!("1"));
    }

    #[test]
    fn innerhtml_setter_from_handler_persists() {
        let html = r#"<!doctype html><html><body>
            <button id="b">b</button>
            <div id="out"><span>old</span></div>
            <script>
              document.querySelector('#b').addEventListener('click', () => {
                document.querySelector('#out').innerHTML = '<em>new</em>';
              });
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        sess.click("#b").unwrap();
        let inner = sess
            .eval("document.querySelector('#out').innerHTML")
            .unwrap();
        assert_eq!(inner.value, serde_json::json!("<em>new</em>"));
        // Serialized DOM also reflects the swap.
        assert!(sess.document_html().contains("<em>new</em>"));
    }

    #[test]
    fn dynamically_appended_element_is_clickable() {
        // Click a button that creates a new button at runtime; the new
        // button must be selectable AND its listener must fire when
        // clicked in a SUBSEQUENT session.click() call.
        let html = r#"<!doctype html><html><body>
            <div id="root"><button id="seed">seed</button></div>
            <div id="out">none</div>
            <script>
              document.querySelector('#seed').addEventListener('click', () => {
                const b = document.createElement('button');
                b.id = 'dyn';
                b.textContent = 'dyn';
                b.addEventListener('click', () => {
                  document.querySelector('#out').textContent = 'dyn-fired';
                });
                document.querySelector('#root').appendChild(b);
              });
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        sess.click("#seed").unwrap();
        // The dynamically-appended button now exists in the live DOM.
        let exists = sess
            .eval("document.querySelector('#dyn') !== null")
            .unwrap();
        assert_eq!(exists.value, serde_json::json!(true));
        // And clicking it fires the listener that was attached at
        // creation time — which lives in the node-keyed registry the
        // same way as any other listener.
        assert_eq!(sess.click("#dyn").unwrap().value["matched"], serde_json::json!(true));
        let out = sess
            .eval("document.querySelector('#out').textContent")
            .unwrap();
        assert_eq!(out.value, serde_json::json!("dyn-fired"));
    }

    #[test]
    fn classlist_toggle_from_handler_persists() {
        let html = r#"<!doctype html><html><body>
            <button id="b" class="off">b</button>
            <script>
              document.querySelector('#b').addEventListener('click', () => {
                document.querySelector('#b').classList.toggle('off');
                document.querySelector('#b').classList.add('on');
              });
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        sess.click("#b").unwrap();
        let cls = sess
            .eval("document.querySelector('#b').className")
            .unwrap();
        // Class string should have lost "off", gained "on".
        let cls_s = cls.value.as_str().unwrap_or_default().to_owned();
        assert!(
            cls_s.contains("on") && !cls_s.contains("off"),
            "expected class to include 'on' and exclude 'off'; got: {cls_s}"
        );
    }

    #[test]
    fn multiple_listeners_fire_in_registration_order() {
        let html = r#"<!doctype html><html><body>
            <button id="b">b</button>
            <script>
              globalThis.log = [];
              const el = document.querySelector('#b');
              el.addEventListener('click', () => globalThis.log.push('a'));
              el.addEventListener('click', () => globalThis.log.push('b'));
              el.addEventListener('click', () => globalThis.log.push('c'));
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        sess.click("#b").unwrap();
        let log = sess.eval("globalThis.log").unwrap();
        assert_eq!(log.value, serde_json::json!(["a", "b", "c"]));
    }

    #[test]
    fn once_listener_only_fires_once() {
        // addEventListener with `{ once: true }` must auto-remove after
        // firing — and the auto-removal must work across the node-keyed
        // registry, not just the per-wrapper map.
        let html = r#"<!doctype html><html><body>
            <button id="b">b</button>
            <span id="n">0</span>
            <script>
              let n = 0;
              document.querySelector('#b').addEventListener('click', () => {
                n += 1;
                document.querySelector('#n').textContent = String(n);
              }, { once: true });
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        sess.click("#b").unwrap();
        sess.click("#b").unwrap();
        sess.click("#b").unwrap();
        let n = sess
            .eval("document.querySelector('#n').textContent")
            .unwrap();
        assert_eq!(n.value, serde_json::json!("1"));
    }

    #[test]
    fn navigation_drops_old_listeners() {
        // After navigate(), listeners registered on the old document
        // must NOT fire when we click selectors on the new document.
        // Tests that the per-document node-listener registry doesn't
        // leak across navigations.
        let html_a = r#"<!doctype html><html><body>
            <button id="b">a</button>
            <script>
              globalThis.fired = false;
              document.querySelector('#b').addEventListener('click', () => {
                globalThis.fired = true;
              });
            </script>
        </body></html>"#;
        let html_b = r#"<!doctype html><html><body>
            <button id="b">b</button>
        </body></html>"#;
        let (mut sess, _) = JsSession::open(html_a, test_url()).unwrap();
        let new_url = Url::parse("https://example.com/b").unwrap();
        sess.navigate(html_b, new_url).unwrap();
        // Click the button on page B — same selector, but the listener
        // was on page A's document. globalThis.fired survives because
        // the engine survives nav, but the listener should NOT fire.
        sess.click("#b").unwrap();
        let fired = sess.eval("globalThis.fired").unwrap();
        assert_eq!(fired.value, serde_json::json!(false));
    }

    #[test]
    fn settimeout_scheduled_mutation_persists() {
        // setTimeout body mutates the DOM; after engine.advance_clock
        // the mutation should be visible via the session.
        let html = r#"<!doctype html><html><body>
            <div id="out">orig</div>
            <script>
              setTimeout(() => {
                document.querySelector('#out').textContent = 'late';
              }, 50);
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        // Before the timer fires.
        let before = sess
            .eval("document.querySelector('#out').textContent")
            .unwrap();
        assert_eq!(before.value, serde_json::json!("orig"));
        // Fire the timer.
        sess.engine().advance_clock(100).unwrap();
        let after = sess
            .eval("document.querySelector('#out').textContent")
            .unwrap();
        assert_eq!(after.value, serde_json::json!("late"));
    }

    #[test]
    fn preventdefault_observable_to_caller_script() {
        // A click handler can call e.preventDefault(); a separate eval
        // observing the same event via a globalThis flag should see it.
        let html = r#"<!doctype html><html><body>
            <button id="b">b</button>
            <script>
              globalThis.prevented = null;
              document.querySelector('#b').addEventListener('click', (e) => {
                e.preventDefault();
                globalThis.prevented = e.defaultPrevented;
              });
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        sess.click("#b").unwrap();
        let prevented = sess.eval("globalThis.prevented").unwrap();
        assert_eq!(prevented.value, serde_json::json!(true));
    }

    #[test]
    fn fill_fires_handler_that_reads_event_target_value() {
        // Tests the e.target.value path that real form code relies on.
        let html = r#"<!doctype html><html><body>
            <input id="i" type="text" value="">
            <span id="echo"></span>
            <script>
              document.querySelector('#i').addEventListener('input', (e) => {
                document.querySelector('#echo').textContent = 'got:' + e.target.value;
              });
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        sess.fill("#i", "abc").unwrap();
        let echo = sess
            .eval("document.querySelector('#echo').textContent")
            .unwrap();
        assert_eq!(echo.value, serde_json::json!("got:abc"));
    }

    #[test]
    fn nested_clicks_one_handler_triggers_another() {
        // Click A's handler programmatically clicks B; B's handler
        // mutates the DOM. Cross-element handler chaining must work
        // through the node-keyed registry.
        let html = r#"<!doctype html><html><body>
            <button id="a">a</button>
            <button id="b">b</button>
            <div id="out">orig</div>
            <script>
              document.querySelector('#a').addEventListener('click', () => {
                document.querySelector('#b').click();
              });
              document.querySelector('#b').addEventListener('click', () => {
                document.querySelector('#out').textContent = 'b-fired';
              });
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, test_url()).unwrap();
        sess.click("#a").unwrap();
        let out = sess
            .eval("document.querySelector('#out').textContent")
            .unwrap();
        assert_eq!(out.value, serde_json::json!("b-fired"));
    }

    #[test]
    fn submit_fires_form_handler_via_button_click() {
        // The Phase 1B submit path: locate <form>, find its submit
        // button, dispatch a click. A click listener on the submit
        // button must fire — and DOM mutations from it must persist.
        let html = r#"<!doctype html><html><body>
            <form id="f">
              <input id="i" name="i" value="">
              <button type="submit" id="sb">go</button>
            </form>
            <div id="out">orig</div>
            <script>
              document.querySelector('#sb').addEventListener('click', (e) => {
                e.preventDefault();
                document.querySelector('#out').textContent = 'submitted';
              });
            </script>
        </body></html>"#;
        let (mut sess, _) = JsSession::open(html, test_url()).unwrap();
        assert_eq!(sess.submit("#f").unwrap().value["matched"], serde_json::json!(true));
        let out = sess
            .eval("document.querySelector('#out').textContent")
            .unwrap();
        assert_eq!(out.value, serde_json::json!("submitted"));
    }
}
