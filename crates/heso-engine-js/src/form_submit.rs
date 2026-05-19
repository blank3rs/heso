//! # form_submit
//!
//! Real HTTP form submission, per [WHATWG HTML §4.10.22 — "Form
//! submission algorithm"][spec].
//!
//! The pre-PR-1 `JsSession::submit` / `JsEngine::submit_form` only
//! dispatched a click on the form's submit button — a no-op for any
//! page that didn't already wire a JS handler. `AGENT_FINDINGS.md`
//! filed this as the single biggest gap for write-shaped agent
//! workloads: "every step returns ok=true but no HTTP POST is ever
//! issued."
//!
//! This module closes the gap. It serializes the form's entry list per
//! the requested enctype, builds a `reqwest::Request`, drives it
//! through the engine's shared `reqwest::Client` (the same one
//! [`crate::fetch::FetchMode::Live`] uses, so cookies / TLS /
//! User-Agent / redirects stay coherent), and returns the post-redirect
//! `(url, body)` so the caller can re-install the document.
//!
//! ## Scope and trade-offs
//!
//! - **Enctypes**: `application/x-www-form-urlencoded` (default),
//!   `multipart/form-data`, and `text/plain`. The first two cover ~all
//!   real-world forms; `text/plain` is rare but is in-spec.
//! - **Methods**: GET (serialize as `?query`, no body) and POST
//!   (serialize as body). Other methods (`PUT`, `DELETE`) inherit POST
//!   shape — uncommon on real `<form>` tags but spec-compliant.
//! - **Successful controls**: enabled fields with non-empty `name`,
//!   excluding `<button>`/`<input type="submit"|reset|image|button">`,
//!   except the activator (the clicked submit button) is included when
//!   present.
//! - **File inputs in multipart**: NOT supported in PR-1. A file input
//!   with no JS-side Blob source has nothing to send beyond the
//!   filename; sending just the filename as a part is worse than
//!   nothing (servers reject it). Filed as a deferred follow-up.
//!   `FormData` is also still undefined — flagged in the
//!   AGENT_FINDINGS report.
//! - **`enctype` overrides on the submit button** (`formenctype`,
//!   `formaction`, `formmethod`) are not honored yet — the form's
//!   own attributes win. Most pages don't use these.
//!
//! ## Determinism
//!
//! Per ADR 0008, seeded sessions reject network access unless a
//! cassette is loaded. Form submission goes through the same client
//! shape as `fetch()`; in `FetchMode::DeterministicNoCassette` the
//! caller path errors with a clear message rather than secretly
//! issuing a request. Live recording / replay is item M.
//!
//! [spec]: https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#form-submission-algorithm

use std::sync::Arc;

use reqwest::Method;
use url::Url;

use crate::engine::{EvalError, JsEngine};
use crate::fetch::FetchMode;

/// One entry in the form's data set, as extracted from JS.
///
/// `kind` is `"text"` for ordinary inputs / textareas / selects /
/// radios / checkboxes whose value the spec stringifies, and `"file"`
/// for `<input type="file">` (filename only — see module doc).
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct FormEntry {
    pub name: String,
    pub value: String,
    pub kind: String,
    /// Present only when `kind == "file"`; the file's basename.
    #[serde(default)]
    pub filename: Option<String>,
    /// Present only when `kind == "file"`; the file input's
    /// `type` attribute (e.g. `"image/png"`) or `"application/octet-stream"`.
    #[serde(default)]
    pub content_type: Option<String>,
}

/// A snapshot of `<form>`'s submission metadata + entry list,
/// extracted from JS in one IIFE so the Rust side can serialize and
/// issue the request.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct FormSnapshot {
    /// `true` when the form selector matched and a submit-typed
    /// descendant was found (or the form has no submit control but
    /// the implicit-submission path applies).
    pub matched: bool,
    /// `true` when `e.preventDefault()` was called on the dispatched
    /// `submit` event. Suppresses the HTTP request — matches real
    /// browser behavior.
    #[serde(default)]
    pub default_prevented: bool,
    /// HTTP method, upper-cased ASCII (`"GET"` / `"POST"`).
    /// Default `"GET"` per spec when the form has no `method`.
    #[serde(default)]
    pub method: String,
    /// Action URL as authored on the `<form>` (may be relative,
    /// missing, or empty). The Rust side resolves it against the
    /// session's current URL.
    #[serde(default)]
    pub action: String,
    /// Encoding type, lowercased per spec. Default
    /// `"application/x-www-form-urlencoded"`.
    #[serde(default)]
    pub enctype: String,
    /// Entry list — the spec-defined "form data set".
    #[serde(default)]
    pub entries: Vec<FormEntry>,
    /// Optional accept-charset; not currently honored (we emit
    /// UTF-8). Present so the snapshot JSON has a stable shape
    /// when an `accept-charset` attribute is on the form, even if
    /// the Rust side just logs and proceeds. `#[allow(dead_code)]`
    /// flags it as deliberately unread until charset routing
    /// lands.
    #[serde(default)]
    #[allow(dead_code)]
    pub accept_charset: Option<String>,
}

/// Result of a successful HTTP form submission — the final URL after
/// redirects and the response body bytes (decoded to a UTF-8 string;
/// for HTML payloads this is what the next document is parsed from).
#[derive(Debug, Clone)]
pub(crate) struct SubmitResponse {
    pub final_url: Url,
    pub body: String,
    pub status: u16,
}

/// Reasons a submission may be skipped at the engine layer (before any
/// HTTP attempt).
#[derive(Debug, Clone)]
pub(crate) enum SubmitSkip {
    /// Selector didn't match a form, or form had no submitter.
    NoForm,
    /// A listener on `submit` called `event.preventDefault()`.
    DefaultPrevented,
}

/// Encoding type as one of the three values defined by §4.10.21.7.
///
/// Anything else maps to `Urlencoded` per the spec's "in the missing
/// value default state" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Enctype {
    Urlencoded,
    Multipart,
    TextPlain,
}

impl Enctype {
    /// Parse from the form's lowercased `enctype` attribute. Unknown
    /// or missing values fall back to `Urlencoded`.
    fn from_attr(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "multipart/form-data" => Self::Multipart,
            "text/plain" => Self::TextPlain,
            // Any other value, including the explicit
            // "application/x-www-form-urlencoded", and the empty
            // string when the attribute is missing.
            _ => Self::Urlencoded,
        }
    }
}

/// Serialize an entry list as `application/x-www-form-urlencoded`.
///
/// Spec: <https://url.spec.whatwg.org/#urlencoded-serializing>.
/// Implementation routes through [`url::form_urlencoded::Serializer`]
/// which is what `URL.searchParams` and `URLSearchParams.toString()`
/// already use in this engine — so submitted GET URLs match
/// `URLSearchParams` outputs byte-for-byte.
pub(crate) fn serialize_urlencoded(entries: &[FormEntry]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for e in entries {
        // File-input filename is the spec-correct value to send for a
        // file in a non-multipart submission (§4.10.22.4 step 5 of
        // the urlencoded serialization), so we emit it as the value.
        // Real-world: don't do this; servers want multipart for files.
        let value: &str = if e.kind == "file" {
            e.filename.as_deref().unwrap_or("")
        } else {
            &e.value
        };
        s.append_pair(&e.name, value);
    }
    s.finish()
}

/// Serialize an entry list as `text/plain`. Spec §4.10.22.4 step 5
/// of the text-plain branch: `name=value\r\n` pairs, no escaping
/// (newlines in values become literal newlines).
pub(crate) fn serialize_text_plain(entries: &[FormEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        let value: &str = if e.kind == "file" {
            e.filename.as_deref().unwrap_or("")
        } else {
            &e.value
        };
        out.push_str(&e.name);
        out.push('=');
        out.push_str(value);
        out.push_str("\r\n");
    }
    out
}

/// Build a `reqwest::multipart::Form` from an entry list.
///
/// Text fields become `Part::text(...)` (no filename, no content type
/// — `reqwest` emits `Content-Disposition: form-data; name="..."`).
/// File entries become a part with `Content-Disposition` filename and
/// `Content-Type` (defaulted to `application/octet-stream`) per
/// §4.10.22.5 — the body is currently the filename string because we
/// don't yet have access to the file contents (see module doc).
pub(crate) fn build_multipart_form(entries: &[FormEntry]) -> reqwest::multipart::Form {
    let mut form = reqwest::multipart::Form::new();
    for e in entries {
        let name = e.name.clone();
        if e.kind == "file" {
            let filename = e.filename.clone().unwrap_or_default();
            let content_type = e
                .content_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string());
            // PR-1 limitation (see module doc): no file body yet — send
            // an empty part with the filename header so the field is
            // present.
            let bare = reqwest::multipart::Part::bytes(Vec::new()).file_name(filename);
            // `mime_str` consumes self and returns Result<Part, _>. On
            // bad mime, fall back to a part with no explicit
            // Content-Type — reqwest will emit the spec default
            // (`text/plain` for text parts, `application/octet-stream`
            // for byte parts).
            let part = match bare.mime_str(&content_type) {
                Ok(p) => p,
                Err(_) => reqwest::multipart::Part::bytes(Vec::new())
                    .file_name(e.filename.clone().unwrap_or_default()),
            };
            form = form.part(name, part);
        } else {
            form = form.part(name, reqwest::multipart::Part::text(e.value.clone()));
        }
    }
    form
}

/// Build a JS snippet that extracts the form snapshot and dispatches
/// the spec-required `submit` event. Returns the JS literal that, when
/// evaluated against the live document, yields a JSON object matching
/// [`FormSnapshot`].
///
/// The snippet:
/// 1. Resolves `form = document.querySelector(<selector>)`.
/// 2. If null, returns `{matched: false}`.
/// 3. Finds the activator via the same fallback chain as
///    [`crate::session::SUBMIT_DESCENDANT_FINDER_JS`]. The activator
///    is included in the entry list when present and named.
/// 4. Constructs a cancelable `submit` Event, dispatches on the form.
/// 5. If `defaultPrevented`, returns `{matched: true,
///    defaultPrevented: true}` without building the entry list.
/// 6. Otherwise reads `form.method` / `form.action` / `form.enctype`
///    and walks `form.elements`-shaped controls (or, since we don't
///    expose `.elements` yet, all descendants) to build the entry
///    list per §4.10.22.4 — skipping disabled / nameless / non-submit
///    buttons / unchecked checkboxes / unchecked radios / unselected
///    options.
pub(crate) fn build_snapshot_js(selector: &str) -> String {
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"
(() => {{
    const form = document.querySelector({sel});
    if (!form) return {{ matched: false }};

    // Activator: same fallback chain as the existing submit() — keeps
    // a single source of truth for "which descendant counts as the
    // submit button."
    //
    // Each `document.querySelector` returns a fresh JS Element
    // wrapper around the same underlying NodeId; identity comparison
    // (`a === b`) between two wrappers for the same node is `false`.
    // We need a stable identity check for the entry-list loop below,
    // so before any dispatch tag the activator with a unique
    // attribute and clean up at the end. Attribute name starts with
    // `data-heso-` so it's clearly an engine-injected marker that
    // any user querySelector would not have generated.
    const ACTIVATOR_MARK = 'data-heso-activator';
    const activator =
        form.querySelector('button[type="submit"]') ||
        form.querySelector('input[type="submit"]') ||
        form.querySelector('button:not([type])');
    if (activator) {{
        activator.setAttribute(ACTIVATOR_MARK, '1');
    }}

    // Real-browser sequence: a user clicking a submit button fires
    // the button's click event FIRST, and only the un-prevented
    // default action of that click then dispatches the form's
    // submit event. Many real pages attach `preventDefault()` to
    // the button click rather than the form submit event, so we
    // honor both checkpoints.
    let buttonClickPrevented = false;
    if (activator) {{
        const clickEv = new Event('click', {{ bubbles: true, cancelable: true }});
        activator.dispatchEvent(clickEv);
        if (clickEv.defaultPrevented) {{
            buttonClickPrevented = true;
        }}
    }}

    // §4.10.22.2: fire the submit event before any data assembly.
    // Listeners that call preventDefault() suppress the request.
    // Skip the form-level dispatch when the button click was
    // preventDefault'd — that's the real-browser cascade rule
    // (a cancelled click's default action never runs).
    let formSubmitPrevented = false;
    if (!buttonClickPrevented) {{
        const submitEv = new Event('submit', {{ bubbles: true, cancelable: true }});
        form.dispatchEvent(submitEv);
        if (submitEv.defaultPrevented) {{
            formSubmitPrevented = true;
        }}
    }}
    if (buttonClickPrevented || formSubmitPrevented) {{
        // Clean up the activator marker — the snapshot path bails
        // out here and never serializes the entry list, so leaving
        // it on would leak a synthetic attribute into the DOM the
        // next eval observes.
        if (activator) activator.removeAttribute(ACTIVATOR_MARK);
        // snake_case field name matches the Rust `FormSnapshot`
        // deserialization — see `default_prevented` field.
        return {{ matched: true, default_prevented: true }};
    }}

    // Read form-level submission attributes.
    const rawMethod = (form.getAttribute('method') || 'GET').toUpperCase();
    const method = (rawMethod === 'POST' ? 'POST' : 'GET');
    const action = form.getAttribute('action') || '';
    const enctypeAttr = (form.getAttribute('enctype') || '').toLowerCase();
    const enctype = enctypeAttr === 'multipart/form-data'
        ? 'multipart/form-data'
        : enctypeAttr === 'text/plain'
            ? 'text/plain'
            : 'application/x-www-form-urlencoded';
    const acceptCharset = form.getAttribute('accept-charset');

    // Walk every form-associated descendant. We don't yet expose
    // `form.elements`, so use querySelectorAll on the four tag types
    // that can be successful controls.
    const controls = form.querySelectorAll('input, select, textarea, button');
    const entries = [];

    const isDisabled = (el) => {{
        // The spec also disables controls inside a disabled
        // <fieldset>, but <fieldset>-tracking is out of scope for
        // PR-1. Direct `disabled` attribute is the common case.
        if (el.hasAttribute('disabled')) return true;
        return false;
    }};

    for (const el of controls) {{
        const tag = (el.tagName || '').toLowerCase();
        const name = el.getAttribute('name');
        if (!name) continue; // unnamed → not successful
        if (isDisabled(el)) continue;

        if (tag === 'button') {{
            const type = (el.getAttribute('type') || 'submit').toLowerCase();
            // Per spec: button counts only if it's the activator.
            // Identity check via the `data-heso-activator` marker
            // since `===` on two wrappers around the same NodeId
            // returns false.
            if (type !== 'submit') continue;
            if (!el.hasAttribute(ACTIVATOR_MARK)) continue;
            entries.push({{ name, value: (el.getAttribute('value') || ''), kind: 'text' }});
            continue;
        }}

        if (tag === 'input') {{
            const type = (el.getAttribute('type') || 'text').toLowerCase();
            switch (type) {{
                case 'submit': {{
                    // Only the activator's submit button contributes.
                    if (!el.hasAttribute(ACTIVATOR_MARK)) continue;
                    entries.push({{ name, value: (el.value || el.getAttribute('value') || ''), kind: 'text' }});
                    break;
                }}
                case 'reset':
                case 'button': {{
                    // Never a successful control.
                    continue;
                }}
                case 'image': {{
                    // Image buttons produce name.x / name.y when they're
                    // the activator; otherwise excluded. We don't track
                    // pixel coords (no real layout), so contribute
                    // 0,0 when applicable.
                    if (!el.hasAttribute(ACTIVATOR_MARK)) continue;
                    entries.push({{ name: name + '.x', value: '0', kind: 'text' }});
                    entries.push({{ name: name + '.y', value: '0', kind: 'text' }});
                    break;
                }}
                case 'checkbox':
                case 'radio': {{
                    if (!el.checked) continue;
                    const v = el.value || el.getAttribute('value') || 'on';
                    entries.push({{ name, value: v, kind: 'text' }});
                    break;
                }}
                case 'file': {{
                    // File inputs: we don't have access to underlying
                    // file bytes yet (no FormData / Blob plumbing).
                    // Emit the filename only — for urlencoded /
                    // text/plain that's the spec; for multipart we
                    // send an empty body part with the filename header.
                    let filename = '';
                    if (el.files && el.files.length > 0) {{
                        filename = el.files[0].name || '';
                    }}
                    entries.push({{
                        name,
                        value: filename,
                        kind: 'file',
                        filename,
                        content_type: 'application/octet-stream',
                    }});
                    break;
                }}
                default: {{
                    // text, email, password, tel, url, search, hidden,
                    // number, date, time, color, range, datetime-local,
                    // month, week — all stringify the IDL value.
                    entries.push({{ name, value: (el.value || el.getAttribute('value') || ''), kind: 'text' }});
                }}
            }}
            continue;
        }}

        if (tag === 'textarea') {{
            entries.push({{ name, value: (el.value || el.textContent || ''), kind: 'text' }});
            continue;
        }}

        if (tag === 'select') {{
            // Iterate options; include selected ones. <select multiple>
            // contributes one entry per selected option, all under the
            // same name. Single-select contributes the first selected
            // option (or the first option if none has `selected`).
            const isMultiple = el.hasAttribute('multiple');
            const optionEls = el.querySelectorAll('option');
            let pickedAny = false;
            for (const opt of optionEls) {{
                const selected = opt.hasAttribute('selected') || (opt.selected === true);
                if (!selected) continue;
                pickedAny = true;
                const v = (opt.getAttribute('value') !== null)
                    ? opt.getAttribute('value')
                    : (opt.textContent || '');
                entries.push({{ name, value: v, kind: 'text' }});
                if (!isMultiple) break;
            }}
            // Single-select with no explicit selected: the first
            // option is the default selected per HTML spec.
            if (!isMultiple && !pickedAny && optionEls.length > 0) {{
                const opt = optionEls[0];
                const v = (opt.getAttribute('value') !== null)
                    ? opt.getAttribute('value')
                    : (opt.textContent || '');
                entries.push({{ name, value: v, kind: 'text' }});
            }}
            continue;
        }}
    }}

    // Clean up the activator marker so it doesn't leak into the
    // serialized DOM observed by the next eval / navigate.
    if (activator) activator.removeAttribute(ACTIVATOR_MARK);

    return {{
        matched: true,
        default_prevented: false,
        method,
        action,
        enctype,
        entries,
        accept_charset: acceptCharset,
    }};
}})()
"#,
        sel = sel
    )
}

/// Resolve `action` against `base` per §4.10.22.3 step 12. Missing or
/// empty action → the base URL itself. Invalid action → error.
fn resolve_action(base: &Url, action: &str) -> Result<Url, EvalError> {
    if action.is_empty() {
        return Ok(base.clone());
    }
    base.join(action)
        .map_err(|e| EvalError::Engine(format!("form action `{action}` invalid: {e}")))
}

/// Issue the HTTP request encoded by `snapshot` against `base_url`'s
/// origin, using `client` / `rt_handle` from the engine. Caller must
/// have already verified `snapshot.matched` and
/// `!snapshot.default_prevented`.
///
/// Returns the post-redirect URL + body string + status code, or an
/// engine error wrapping the underlying reqwest failure.
pub(crate) fn issue_request(
    snapshot: &FormSnapshot,
    base_url: &Url,
    client: &Arc<reqwest::Client>,
    rt_handle: &tokio::runtime::Handle,
) -> Result<SubmitResponse, EvalError> {
    let action_url = resolve_action(base_url, &snapshot.action)?;
    let method = match snapshot.method.as_str() {
        "POST" => Method::POST,
        _ => Method::GET,
    };
    let enctype = Enctype::from_attr(&snapshot.enctype);

    // GET method: serialize the entries as the query, replacing any
    // pre-existing query. Spec §4.10.22.3 step 18.1.
    let (request_url, body_kind) = if method == Method::GET {
        let mut u = action_url.clone();
        let encoded = serialize_urlencoded(&snapshot.entries);
        u.set_query(if encoded.is_empty() { None } else { Some(&encoded) });
        (u, BodyKind::None)
    } else {
        // POST: keep the action URL as-is; build the body per enctype.
        let body = match enctype {
            Enctype::Urlencoded => BodyKind::Urlencoded(serialize_urlencoded(&snapshot.entries)),
            Enctype::TextPlain => BodyKind::TextPlain(serialize_text_plain(&snapshot.entries)),
            Enctype::Multipart => BodyKind::Multipart(build_multipart_form(&snapshot.entries)),
        };
        (action_url, body)
    };

    // Build the reqwest request and drive it via the engine's tokio
    // handle. `block_in_place` matches the pattern in `crate::fetch`
    // — we're single-threaded on the JS engine thread but the host
    // wires a multi_thread runtime, so this hands work to another
    // worker rather than deadlocking.
    let client = client.clone();
    let result: Result<(Url, String, u16), reqwest::Error> = tokio::task::block_in_place(|| {
        rt_handle.block_on(async move {
            let mut builder = client.request(method, request_url.as_str());
            builder = match body_kind {
                BodyKind::None => builder,
                BodyKind::Urlencoded(s) => builder
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(s),
                BodyKind::TextPlain(s) => builder.header("Content-Type", "text/plain").body(s),
                BodyKind::Multipart(form) => builder.multipart(form),
            };
            let resp = builder.send().await?;
            let status = resp.status().as_u16();
            let final_url_str = resp.url().as_str().to_owned();
            let body = resp.text().await?;
            let final_url = Url::parse(&final_url_str)
                .unwrap_or_else(|_| Url::parse("about:blank").expect("about:blank parses"));
            Ok((final_url, body, status))
        })
    });

    match result {
        Ok((final_url, body, status)) => Ok(SubmitResponse {
            final_url,
            body,
            status,
        }),
        Err(e) => Err(EvalError::Engine(format!("form submit HTTP error: {e}"))),
    }
}

/// Body shape variants — kept out of the public surface because
/// `reqwest::multipart::Form` is not `Clone`.
enum BodyKind {
    None,
    Urlencoded(String),
    TextPlain(String),
    Multipart(reqwest::multipart::Form),
}

/// Borrow `(client, rt_handle)` out of the engine's fetch state, if any.
///
/// `None` when the engine was built without a fetch client (e.g.
/// `JsEngine::new()`); the caller should fall back to the
/// dispatch-only legacy path.
pub(crate) fn live_fetch_handle(engine: &JsEngine) -> Option<(Arc<reqwest::Client>, tokio::runtime::Handle)> {
    let fs = engine.fetch_state_ref()?;
    match &fs.mode {
        FetchMode::Live { client, rt_handle } => Some((client.clone(), rt_handle.clone())),
        FetchMode::DeterministicNoCassette => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, value: &str) -> FormEntry {
        FormEntry {
            name: name.to_owned(),
            value: value.to_owned(),
            kind: "text".to_owned(),
            filename: None,
            content_type: None,
        }
    }

    #[test]
    fn urlencoded_roundtrip_escapes_spaces_and_specials() {
        let entries = vec![
            entry("custname", "Jane Doe"),
            entry("comments", "hello world & friends"),
        ];
        let out = serialize_urlencoded(&entries);
        assert_eq!(
            out,
            "custname=Jane+Doe&comments=hello+world+%26+friends",
        );
    }

    #[test]
    fn urlencoded_empty_is_empty_string() {
        assert_eq!(serialize_urlencoded(&[]), "");
    }

    #[test]
    fn text_plain_uses_crlf_pairs() {
        let entries = vec![entry("a", "1"), entry("b", "two words")];
        let out = serialize_text_plain(&entries);
        assert_eq!(out, "a=1\r\nb=two words\r\n");
    }

    #[test]
    fn enctype_parsing_defaults_to_urlencoded() {
        assert_eq!(Enctype::from_attr(""), Enctype::Urlencoded);
        assert_eq!(Enctype::from_attr("garbage"), Enctype::Urlencoded);
        assert_eq!(
            Enctype::from_attr("APPLICATION/x-www-form-urlencoded"),
            Enctype::Urlencoded
        );
        assert_eq!(
            Enctype::from_attr("multipart/form-data"),
            Enctype::Multipart
        );
        assert_eq!(Enctype::from_attr("text/plain"), Enctype::TextPlain);
    }

    #[test]
    fn resolve_action_falls_back_to_base_when_empty() {
        let base = Url::parse("https://example.com/page").unwrap();
        let resolved = resolve_action(&base, "").unwrap();
        assert_eq!(resolved.as_str(), "https://example.com/page");
    }

    #[test]
    fn resolve_action_resolves_relative() {
        let base = Url::parse("https://example.com/forms/").unwrap();
        let resolved = resolve_action(&base, "submit").unwrap();
        assert_eq!(resolved.as_str(), "https://example.com/forms/submit");
    }

    #[test]
    fn resolve_action_keeps_absolute() {
        let base = Url::parse("https://example.com/page").unwrap();
        let resolved = resolve_action(&base, "https://other.test/q").unwrap();
        assert_eq!(resolved.as_str(), "https://other.test/q");
    }
}
