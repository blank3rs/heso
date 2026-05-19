//! # dom
//!
//! Phase 1B of the agent-shaped DOM per [ADR 0014]. Read-and-traverse
//! plus the **mutation surface** the rest of the JS-DOM bridge needs
//! before events and timers land.
//!
//! What this module gives you:
//!
//! - A [`Document`] Rust struct exposed to JavaScript as the global
//!   `document` once an HTML page has been loaded into the engine.
//! - An [`Element`] Rust struct returned from queries on [`Document`]
//!   or other elements.
//! - A [`DomTokenList`] Rust struct returned by `element.classList`
//!   exposing `.add/.remove/.toggle/.contains` over the space-separated
//!   `class` attribute.
//! - Read methods: `querySelector`, `querySelectorAll`, `getElementById`,
//!   `documentElement`, `body`, `head`, `title` on [`Document`];
//!   `tagName`, `localName`, `id`, `className`, `textContent`,
//!   `innerHTML`, `outerHTML`, `getAttribute`, `hasAttribute`,
//!   `querySelector`, `querySelectorAll`, `children`, `parentElement`
//!   on [`Element`].
//! - Mutation methods: `setAttribute`, `removeAttribute`, `innerHTML`
//!   setter, `textContent` setter, `appendChild`, `removeChild`, plus
//!   the `classList` API.
//!
//! What this module does NOT yet do:
//!
//! - **No events.** `addEventListener`, `removeEventListener`,
//!   `dispatchEvent`, `click()` — a follow-up agent integrates the
//!   event model.
//! - **No timers / no fetch.** `setTimeout` / `setInterval` / `fetch`
//!   land in a separate follow-up so the determinism story (ADR 0008)
//!   can be locked down per-API.
//! - **No layout.** `getBoundingClientRect`, `offsetWidth`, etc. — out
//!   of scope per [ADR 0016].
//!
//! ## Why "agent-shaped"
//!
//! The DOM standard is huge. We implement the read-and-traverse half
//! plus the mutation primitives real pages reach for during hydration,
//! and leave the layout-and-paint half out. Per [ADR 0016] this is the
//! bet: the "agent-relevant half" of the browser surface is what we
//! ship; the rest of Chromium is bloat for the agent use case.
//!
//! ## Backing store: `dom_query`
//!
//! The tree underneath is [`dom_query::Document`] (a jQuery-like
//! wrapper over `html5ever` with mutable [`dom_query::NodeRef`]s).
//! Selected over `scraper` for Phase 1C because:
//!
//! - `dom_query::NodeRef` supports `set_attr`, `set_html`, `set_text`,
//!   `append_child`, `remove_from_parent`. `scraper::Html` is parse-and-
//!   freeze.
//! - Handles are [`dom_query::NodeId`]s — `Copy`, stable across
//!   mutations within the same tree (the underlying arena reuses
//!   indices only after explicit detach + drop, not on simple moves).
//! - `html5ever`-backed, matches the rest of the workspace.
//!
//! We pin to `=0.28.0` exactly — see this crate's `Cargo.toml`.
//!
//! ## Lifetime story
//!
//! [`dom_query::Document`] owns the [`dom_query::Tree`] inside a
//! `RefCell`-shaped arena. We share it via [`Arc<dom_query::Document>`]
//! so multiple [`Element`] handles can outlive any given JavaScript
//! call. Each [`Element`] stores `(Arc<Document>, NodeId)` and resolves
//! the [`dom_query::NodeRef`] on every access — cheap, since
//! [`dom_query::Tree::get`] is O(1).
//!
//! [ADR 0014]: ../../decisions/0014-bundled-quickjs-agent-dom.md
//! [ADR 0016]: ../../decisions/0016-positioning-headless-browser-for-agents.md

use std::sync::Arc;

use dom_query::{Document as DqDocument, NodeId, NodeRef};
use rquickjs::{
    class::Trace,
    prelude::{Opt, This},
    Class, Ctx, Function, JsLifetime, Object, Value,
};
use url::Url;

use crate::events::{
    self, add_listener_to_map, dispatch_with_node_path, parse_listener_options,
    remove_listener_from_map,
};

/// Name of the hidden property on `globalThis.document` whose value
/// is an object mapping per-element listener maps, keyed by a stable
/// stringification of [`dom_query::NodeId`].
///
/// This indirection is the reason `addEventListener` survives across
/// `eval` boundaries: every `document.querySelector(...)` returns a
/// **new** JS `Element` wrapper, but `globalThis.document` itself is
/// a single long-lived object installed at session-open time, so any
/// state hung off it lives as long as the session does.
const PROP_NODE_LISTENERS: &str = "__nodeListeners";

/// Hidden key on each per-node listener map storing the JS-side
/// Element wrapper that the framework used as `this` when calling
/// `addEventListener`. Dispatch reuses this wrapper as the per-node
/// `currentTarget` so hidden-property mutations the framework
/// stashed on the wrapper (e.g. Preact's `e.l[type+capture] = fn`)
/// are visible inside its registered event proxy.
const PROP_OWNER_WRAPPER: &str = "__owner";

/// Name of the hidden registry on `globalThis.document` whose value is
/// an object mapping per-element IDL state, keyed by a stable
/// stringification of [`dom_query::NodeId`]. Holds the "dirty value
/// flag" + "API value" pair that separates `HTMLInputElement.value`
/// (the typed-in text) from the `value` content attribute (=
/// `defaultValue`), plus the analogous bits for `checked`.
///
/// Why a document-side registry instead of own-props on the Element
/// JS instance: every `document.querySelector(...)` produces a **new**
/// Element wrapper, so own-props on the wrapper don't survive across
/// `el = document.querySelector(...)` calls. The listener registry
/// solves the same problem the same way — see [`PROP_NODE_LISTENERS`].
///
/// IDL state for input form controls per the HTML spec:
/// <https://html.spec.whatwg.org/multipage/input.html#the-input-element>.
const PROP_NODE_IDL_STATE: &str = "__nodeIdlState";

/// Key under [`PROP_NODE_IDL_STATE`]`[node_key]` holding the IDL
/// value (string). Present only after the JS-side setter has fired.
const IDL_VALUE: &str = "value";
/// Key holding the "dirty value flag" — `true` once `.value` has been
/// set programmatically. The getter falls back to the `value` content
/// attribute (= `defaultValue`) until this flips to `true`.
const IDL_VALUE_DIRTY: &str = "valueDirty";
/// Key holding the IDL `checked` flag (bool). Present only after the
/// JS-side setter has fired.
const IDL_CHECKED: &str = "checked";
/// Key holding the "dirty checkedness flag" — `true` once `.checked`
/// has been set programmatically. The getter falls back to
/// `hasAttribute('checked')` (= `defaultChecked`) until this flips.
const IDL_CHECKED_DIRTY: &str = "checkedDirty";

/// Stringify a [`NodeId`] for use as a JS-object key in the
/// node-keyed listener registry. Debug-formatting is fine here —
/// `NodeId` derives `Debug`, the format is stable for the lifetime
/// of the parse tree, and the resulting string is only ever compared
/// for equality against other strings produced the same way.
fn node_key(node_id: NodeId) -> String {
    format!("{:?}", node_id)
}

/// Look up (or lazily create) the per-element listener map for
/// `node_id` on the long-lived `globalThis.document.__nodeListeners`
/// registry. Returns the inner map object whose keys are event types
/// and whose values are arrays of listener records — the same shape
/// [`crate::events`] expects.
pub(crate) fn element_listener_map<'js>(
    ctx: &Ctx<'js>,
    node_id: NodeId,
) -> rquickjs::Result<Object<'js>> {
    let globals = ctx.globals();
    let document: Object<'js> = globals.get("document")?;
    let registry: Object<'js> = match document.get::<_, Option<Object<'js>>>(PROP_NODE_LISTENERS)? {
        Some(r) => r,
        None => {
            let r = Object::new(ctx.clone())?;
            document.set(PROP_NODE_LISTENERS, r.clone())?;
            r
        }
    };
    let key = node_key(node_id);
    match registry.get::<_, Option<Object<'js>>>(key.as_str())? {
        Some(m) => Ok(m),
        None => {
            let m = Object::new(ctx.clone())?;
            registry.set(key.as_str(), m.clone())?;
            Ok(m)
        }
    }
}

/// Delete the listener-registry entries for every NodeId in `ids`
/// from `globalThis.document.__nodeListeners`. Used by
/// [`Element::remove_child`] to keep the registry from accumulating
/// stale records for detached subtrees.
pub(crate) fn clear_listeners_for_nodes<'js>(
    ctx: &Ctx<'js>,
    ids: &[NodeId],
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let document: Option<Object<'js>> = globals.get::<_, Option<Object<'js>>>("document")?;
    let Some(document) = document else {
        return Ok(());
    };
    let registry: Option<Object<'js>> =
        document.get::<_, Option<Object<'js>>>(PROP_NODE_LISTENERS)?;
    let Some(registry) = registry else {
        return Ok(());
    };
    for id in ids {
        let key = node_key(*id);
        let _ = registry.remove(key.as_str());
    }
    Ok(())
}

/// Look up (or lazily create) the per-element IDL state map for
/// `node_id` on the long-lived `globalThis.document.__nodeIdlState`
/// registry. Returns the inner map object whose keys are
/// [`IDL_VALUE`] / [`IDL_VALUE_DIRTY`] / [`IDL_CHECKED`] /
/// [`IDL_CHECKED_DIRTY`].
fn element_idl_state<'js>(
    ctx: &Ctx<'js>,
    node_id: NodeId,
) -> rquickjs::Result<Object<'js>> {
    let globals = ctx.globals();
    let document: Object<'js> = globals.get("document")?;
    let registry: Object<'js> = match document.get::<_, Option<Object<'js>>>(PROP_NODE_IDL_STATE)? {
        Some(r) => r,
        None => {
            let r = Object::new(ctx.clone())?;
            document.set(PROP_NODE_IDL_STATE, r.clone())?;
            r
        }
    };
    let key = node_key(node_id);
    match registry.get::<_, Option<Object<'js>>>(key.as_str())? {
        Some(m) => Ok(m),
        None => {
            let m = Object::new(ctx.clone())?;
            registry.set(key.as_str(), m.clone())?;
            Ok(m)
        }
    }
}

/// Read-only variant of [`element_idl_state`] — returns `None` if no
/// IDL writes have happened for this node yet. Used by getters so a
/// read of `.value` / `.checked` on a never-mutated input doesn't
/// litter the registry with empty maps.
fn element_idl_state_opt<'js>(
    ctx: &Ctx<'js>,
    node_id: NodeId,
) -> rquickjs::Result<Option<Object<'js>>> {
    let globals = ctx.globals();
    let document: Option<Object<'js>> = globals.get::<_, Option<Object<'js>>>("document")?;
    let Some(document) = document else {
        return Ok(None);
    };
    let registry: Option<Object<'js>> =
        document.get::<_, Option<Object<'js>>>(PROP_NODE_IDL_STATE)?;
    let Some(registry) = registry else {
        return Ok(None);
    };
    let key = node_key(node_id);
    registry.get::<_, Option<Object<'js>>>(key.as_str())
}

/// Delete the IDL-state entries for every NodeId in `ids` from the
/// document-side registry. Mirrors [`clear_listeners_for_nodes`] so a
/// detached subtree doesn't leave stale IDL state behind (and so a
/// recycled NodeId can't pick up the previous occupant's `value`).
pub(crate) fn clear_idl_state_for_nodes<'js>(
    ctx: &Ctx<'js>,
    ids: &[NodeId],
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let document: Option<Object<'js>> = globals.get::<_, Option<Object<'js>>>("document")?;
    let Some(document) = document else {
        return Ok(());
    };
    let registry: Option<Object<'js>> =
        document.get::<_, Option<Object<'js>>>(PROP_NODE_IDL_STATE)?;
    let Some(registry) = registry else {
        return Ok(());
    };
    for id in ids {
        let key = node_key(*id);
        let _ = registry.remove(key.as_str());
    }
    Ok(())
}

/// Read-only variant of [`element_listener_map`] — returns `None` if
/// no listeners have been registered for this node yet. Used by the
/// dispatch path so it doesn't litter the registry with empty maps
/// for every element that's ever had `dispatchEvent` called on it
/// without listeners.
pub(crate) fn element_listener_map_opt<'js>(
    ctx: &Ctx<'js>,
    node_id: NodeId,
) -> rquickjs::Result<Option<Object<'js>>> {
    let globals = ctx.globals();
    let document: Option<Object<'js>> = globals.get::<_, Option<Object<'js>>>("document")?;
    let Some(document) = document else {
        return Ok(None);
    };
    let registry: Option<Object<'js>> = document.get::<_, Option<Object<'js>>>(PROP_NODE_LISTENERS)?;
    let Some(registry) = registry else {
        return Ok(None);
    };
    let key = node_key(node_id);
    registry.get::<_, Option<Object<'js>>>(key.as_str())
}

/// Document-level listener map. Keyed off the same
/// `__nodeListeners` registry as elements, under the fixed
/// sentinel `"document"` (a [`NodeId`] could never produce this
/// stringification, so collisions are impossible).
fn document_listener_map<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let globals = ctx.globals();
    let document: Object<'js> = globals.get("document")?;
    let registry: Object<'js> = match document.get::<_, Option<Object<'js>>>(PROP_NODE_LISTENERS)? {
        Some(r) => r,
        None => {
            let r = Object::new(ctx.clone())?;
            document.set(PROP_NODE_LISTENERS, r.clone())?;
            r
        }
    };
    match registry.get::<_, Option<Object<'js>>>("document")? {
        Some(m) => Ok(m),
        None => {
            let m = Object::new(ctx.clone())?;
            registry.set("document", m.clone())?;
            Ok(m)
        }
    }
}

/// Read-only variant of [`document_listener_map`]: `None` if no
/// document listeners have been registered yet.
fn document_listener_map_opt<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Option<Object<'js>>> {
    let globals = ctx.globals();
    let document: Option<Object<'js>> = globals.get::<_, Option<Object<'js>>>("document")?;
    let Some(document) = document else {
        return Ok(None);
    };
    let registry: Option<Object<'js>> =
        document.get::<_, Option<Object<'js>>>(PROP_NODE_LISTENERS)?;
    let Some(registry) = registry else {
        return Ok(None);
    };
    registry.get::<_, Option<Object<'js>>>("document")
}

/// Read `globalThis.location.href` and parse it as a [`Url`]. Returns
/// `None` when `location` is missing, its `href` field is not a
/// string, or the value isn't an absolute URL (e.g. `"about:blank"`
/// parses fine; an empty string does not).
///
/// Used by the [`HTMLHyperlinkElementUtils`] mixin on
/// [`Element`] (`href`, `protocol`, `host`, `hostname`, `port`,
/// `pathname`, `search`, `hash`, `origin`, `username`, `password` on
/// `<a>` / `<area>`) to resolve the element's `href` content attribute
/// against the document base URL per WHATWG HTML §4.6.6.
///
/// The base URL "lives" on `globalThis.location.href` rather than on
/// the [`Document`] struct because the engine already routes the
/// page URL through `install_location` on every navigation — see
/// [`crate::engine::install_location`]. Reading from `location` keeps
/// us coherent with `history.pushState` / `history.replaceState`
/// without threading an extra base-URL field through every Element.
///
/// [`HTMLHyperlinkElementUtils`]: https://html.spec.whatwg.org/multipage/links.html#htmlhyperlinkelementutils
fn document_base_url<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Option<Url>> {
    let globals = ctx.globals();
    let location: Option<Object<'js>> = globals.get::<_, Option<Object<'js>>>("location")?;
    let Some(location) = location else {
        return Ok(None);
    };
    let href: Option<String> = location.get::<_, Option<String>>("href")?;
    let Some(href) = href else {
        return Ok(None);
    };
    Ok(Url::parse(&href).ok())
}

/// Resolve an anchor / area element's `href` content attribute
/// against the document's base URL. Returns:
///
/// - `Ok(None)` when the element has no `href` attribute. Per spec
///   this maps to `""` from the `href` getter and the empty-string
///   defaults for every decomposition property.
/// - `Ok(Some(url))` when the `href` attribute parsed successfully
///   (either as an absolute URL, or relative to the document base).
/// - `Err(raw)` when the attribute is present but `url::Url::parse`
///   plus `base.join` both rejected it. Per WHATWG HTML §4.6.6 the
///   `href` getter falls back to returning the raw attribute text in
///   this case; decomposition properties return `""`.
///
/// `node` is borrowed for the read; we resolve once per IDL property
/// access. `url::Url::parse` is sub-microsecond, and matching the
/// "lazy reinitialize" model the spec describes avoids us having to
/// invalidate a cached `Url` on every `setAttribute('href', …)`.
fn resolve_anchor_url<'js>(
    ctx: &Ctx<'js>,
    node: &NodeRef<'_>,
) -> rquickjs::Result<Result<Option<Url>, String>> {
    let Some(raw) = node.attr("href") else {
        return Ok(Ok(None));
    };
    let raw = raw.to_string();
    // Try as absolute first — `Url::parse` handles `javascript:`,
    // `mailto:`, `data:` and anything else with a scheme without
    // needing a base. Only fall through to `base.join` for relative
    // refs, so we match WHATWG behavior for "URLs with non-relative
    // flag set" (mycustomprotocol:abc → protocol = "mycustomprotocol:").
    if let Ok(u) = Url::parse(&raw) {
        return Ok(Ok(Some(u)));
    }
    // Relative — try resolving against the document base URL.
    if let Some(base) = document_base_url(ctx)? {
        if let Ok(u) = base.join(&raw) {
            return Ok(Ok(Some(u)));
        }
    }
    Ok(Err(raw))
}

/// True iff `name` is an element tag that the WHATWG
/// `HTMLHyperlinkElementUtils` mixin applies to (`<a>` with an `href`
/// attribute and `<area>` with an `href` attribute, per HTML §4.6.6).
/// The `href` content attribute itself is checked elsewhere; this
/// gate only blocks the IDL properties on, say, `<link>` or `<base>`
/// (which reflect `href` as a plain string attribute, not the
/// decomposition mixin).
fn is_hyperlink_tag(name: &str) -> bool {
    name.eq_ignore_ascii_case("a") || name.eq_ignore_ascii_case("area")
}

/// Higher-level wrapper used by every `HTMLHyperlinkElementUtils`
/// getter on [`Element`]. Returns `Some(url)` when the element is an
/// `<a>` or `<area>` with a successfully-resolved `href`, and `None`
/// otherwise (non-hyperlink tag, missing attribute, or parse failure).
///
/// Decomposition getters (`protocol`, `host`, `pathname`, etc.) all
/// return `""` on `None`. The `href` getter is the one exception — it
/// surfaces the raw attribute on parse failure, so it bypasses this
/// helper and calls [`resolve_anchor_url`] directly.
fn anchor_url<'js>(
    this: &This<Class<'js, Element>>,
    ctx: &Ctx<'js>,
) -> rquickjs::Result<Option<Url>> {
    let (doc, node_id) = {
        let borrowed = this.0.borrow();
        (borrowed.doc.clone(), borrowed.node_id)
    };
    let Some(node) = doc.tree.get(&node_id) else {
        return Ok(None);
    };
    let is_hyperlink = node
        .node_name()
        .map(|n| is_hyperlink_tag(n.as_ref()))
        .unwrap_or(false);
    if !is_hyperlink {
        return Ok(None);
    }
    match resolve_anchor_url(ctx, &node)? {
        Ok(opt) => Ok(opt),
        Err(_) => Ok(None),
    }
}

/// Persist the (possibly-mutated) parsed URL back into the element's
/// `href` content attribute as the serialized absolute URL. Used by
/// every `HTMLHyperlinkElementUtils` setter (`protocol`, `host`,
/// `pathname`, `search`, …) so a read-then-write round-trip via
/// `anchor.protocol = "https"` is observable through both
/// `anchor.href` and `anchor.getAttribute('href')`.
fn write_anchor_href<'js>(this: &This<Class<'js, Element>>, url: &Url) {
    let (doc, node_id) = {
        let borrowed = this.0.borrow();
        (borrowed.doc.clone(), borrowed.node_id)
    };
    if let Some(node) = doc.tree.get(&node_id) {
        node.set_attr("href", url.as_str());
    }
}

// =====================================================================
// HTMLFormElement helpers (WHATWG HTML §4.10.3)
// =====================================================================
//
// The IDL surface for `<form>` lives below as gated methods on the
// shared [`Element`] class (same pattern as the `HTMLHyperlinkElementUtils`
// mixin on `<a>` / `<area>`). These helpers normalize the spec corners
// so the getter bodies stay compact.

/// True iff `name` is the `<form>` tag (case-insensitive). The
/// HTMLFormElement IDL props (`action`, `method`, `enctype`,
/// `elements`, `length`, `submit()`, `reset()`) all gate on this
/// — every other tag returns the spec's "missing-value default"
/// from the getters and silent no-ops from the methods.
fn is_form_tag(name: &str) -> bool {
    name.eq_ignore_ascii_case("form")
}

/// True iff `name` is a "listed element" per WHATWG HTML §4.10.2 —
/// `button`, `fieldset`, `input`, `object`, `output`, `select`,
/// `textarea`, `img` (when associated with a form via form= attribute).
///
/// Used by `form.elements` and `form.length` to filter the form's
/// descendants down to its actual control set. We don't track the
/// `form=` cross-tree association yet, so this just gates on tag
/// name; that matches the common case where every control is a
/// physical descendant of `<form>`.
///
/// Note: `<img>` is technically a listed element (for image-button
/// purposes), but only `<img>` with an `ismap` / `usemap` semantics
/// applies. We omit it from `form.elements` because real-world
/// pages don't rely on the rare img-as-form-control path; if a
/// page does, it would still find the img via
/// `form.querySelector('img')`.
fn is_listed_form_control_tag(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "button" | "fieldset" | "input" | "object" | "output" | "select" | "textarea"
    )
}

/// Walk a form's element subtree and collect every listed form
/// control. Returns the node ids in document order so callers can
/// preserve the spec-required ordering.
///
/// Spec: <https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#the-form-element>
/// step ("the elements IDL attribute must return an HTMLFormControlsCollection
/// rooted at this form's node...").
fn collect_form_listed_controls(doc: &Arc<DqDocument>, form_id: NodeId) -> Vec<NodeId> {
    let Some(form) = doc.tree.get(&form_id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for descendant in form.descendants_it() {
        if !descendant.is_element() {
            continue;
        }
        let Some(name) = descendant.node_name() else {
            continue;
        };
        if is_listed_form_control_tag(name.as_ref()) {
            out.push(descendant.id);
        }
    }
    out
}

/// Resolve a form's `action` attribute against the document base
/// URL per WHATWG HTML §4.10.3. The getter spec returns the
/// resolved absolute URL when the attribute is set and parseable,
/// or the document URL when the attribute is missing/empty.
///
/// Returns the serialized absolute URL or empty string when
/// resolution fails (e.g. base URL itself isn't parseable).
fn resolve_form_action<'js>(ctx: &Ctx<'js>, node: &NodeRef<'_>) -> rquickjs::Result<String> {
    // Per spec: when action is missing or empty, use the document URL.
    let raw_action = node.attr("action").map(|s| s.to_string());
    let action = raw_action.as_deref().unwrap_or("");
    // Empty string or absent → return the document base URL itself.
    if action.is_empty() {
        return Ok(document_base_url(ctx)?
            .map(|u| u.as_str().to_owned())
            .unwrap_or_default());
    }
    // Try absolute parse first.
    if let Ok(u) = Url::parse(action) {
        return Ok(u.as_str().to_owned());
    }
    // Relative — resolve against document base.
    if let Some(base) = document_base_url(ctx)? {
        if let Ok(u) = base.join(action) {
            return Ok(u.as_str().to_owned());
        }
    }
    // Parse failure with no base → fall back to the raw attribute.
    Ok(action.to_owned())
}

/// Per WHATWG HTML §4.10.3, `form.method` getter normalizes the
/// `method` content attribute to one of `"get"`, `"post"`,
/// `"dialog"` (all lowercase). Anything else — including a missing
/// attribute — returns the spec's "missing value default" of
/// `"get"`.
///
/// Spec: <https://html.spec.whatwg.org/multipage/forms.html#dom-fs-method>.
fn normalize_form_method(raw: Option<&str>) -> &'static str {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "post" => "post",
        "dialog" => "dialog",
        // "get" or any other value (including the empty string when
        // the attribute is missing) → default state "get".
        _ => "get",
    }
}

/// Per WHATWG HTML §4.10.3, `form.enctype` getter normalizes the
/// `enctype` content attribute to one of the three valid values
/// (lowercase). Anything else falls back to the "missing value
/// default" of `"application/x-www-form-urlencoded"`.
///
/// `form.encoding` is a spec-defined alias for `form.enctype` with
/// identical semantics.
///
/// Spec: <https://html.spec.whatwg.org/multipage/forms.html#dom-fs-enctype>.
fn normalize_form_enctype(raw: Option<&str>) -> &'static str {
    match raw.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "multipart/form-data" => "multipart/form-data",
        "text/plain" => "text/plain",
        // "application/x-www-form-urlencoded" or any other value
        // (including missing) → default.
        _ => "application/x-www-form-urlencoded",
    }
}

/// Helper for `<form>`-gated IDL methods. Returns the form's
/// [`NodeRef`] when the element is a `<form>`, else `None`.
fn form_node_ref<'a, 'js>(
    this: &This<Class<'js, Element>>,
    doc: &'a Arc<DqDocument>,
) -> Option<NodeRef<'a>> {
    let node_id = this.0.borrow().node_id;
    let node = doc.tree.get(&node_id)?;
    let is_form = node
        .node_name()
        .map(|n| is_form_tag(n.as_ref()))
        .unwrap_or(false);
    if !is_form {
        return None;
    }
    Some(node)
}

/// The `document` global.
///
/// Wraps a parsed [`dom_query::Document`]. Construction is from Rust
/// only — JavaScript cannot `new Document()` because no
/// `#[qjs(constructor)]` is provided. The engine installs a single
/// instance under the `document` global at page-load time.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class]
pub struct Document {
    /// Backing parse tree. Shared with all [`Element`] handles
    /// produced from this document.
    #[qjs(skip_trace)]
    doc: Arc<DqDocument>,
}

impl Document {
    /// Construct a new [`Document`] wrapping `doc`. Called by the
    /// engine; not exposed to JS.
    pub fn new(doc: Arc<DqDocument>) -> Self {
        Self { doc }
    }

    /// Parse `html` as a full HTML document and wrap it. Convenience
    /// for the engine and for tests; not exposed to JS.
    ///
    /// `dom_query::Document` is `!Send + !Sync` (it owns a `RefCell`-
    /// backed arena to support mutation). We still wrap it in [`Arc`]
    /// because rquickjs's class machinery requires the inner storage
    /// to be shareable across `Class<T>` instances, and rquickjs's
    /// own [`rquickjs::Runtime`] is single-threaded: the runtime
    /// holds an internal mutex that serializes every `Ctx`, so the
    /// `Arc` will never actually cross threads in practice. Hence
    /// the `arc_with_non_send_sync` allow.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn from_html(html: &str) -> Self {
        Self::new(Arc::new(DqDocument::from(html)))
    }

    /// Rust-side helper for selector lookup that returns an
    /// `Option<Element>` directly. The JS-facing `querySelector`
    /// wraps this so it can return JS `null` (rather than rquickjs's
    /// default `undefined` for `None`) on no-match.
    pub fn query_selector_inner(&self, selector: &str) -> Option<Element> {
        let sel = self.doc.try_select(selector)?;
        let nodes = sel.nodes();
        let first = nodes.first()?;
        Some(Element::from_id(self.doc.clone(), first.id))
    }

    /// Borrow the underlying [`dom_query::Document`] (useful for the
    /// engine to introspect the parse alongside the JS, e.g. to wire
    /// in the action graph).
    pub fn dom(&self) -> &DqDocument {
        &self.doc
    }

    /// Clone the [`Arc`] wrapping the underlying [`dom_query::Document`].
    ///
    /// Useful when the engine needs to keep one extra refcount on the
    /// same parse tree (for example, to walk it from Rust *and* hand
    /// the same tree to a `Class<Document>` JS instance — the Phase 1C
    /// script pump does this). Both handles share mutations: anything
    /// JS does via `document.querySelector(...).setAttribute(...)`
    /// shows up through this `Arc` too, because the underlying tree
    /// is the *same* `dom_query::Document`, not a clone.
    pub fn dom_arc(&self) -> Arc<DqDocument> {
        self.doc.clone()
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl Document {
    /// `document.querySelector(selector)` — return the first element
    /// matching `selector`, or `null`. An invalid selector returns
    /// `null` rather than panicking (DOM technically throws
    /// `SyntaxError`; alignment with that is a Phase 1C follow-up).
    fn query_selector<'js>(
        &self,
        ctx: Ctx<'js>,
        selector: String,
    ) -> rquickjs::Result<Value<'js>> {
        match self.query_selector_inner(&selector) {
            Some(el) => {
                let instance = Class::instance(ctx.clone(), el)?;
                Ok(instance.into_value())
            }
            // DOM spec: querySelector returns null when no match.
            None => ctx.eval::<Value<'js>, _>("null"),
        }
    }

    /// `document.querySelectorAll(selector)` — return all matching
    /// elements as an array, in document order. An invalid selector
    /// yields an empty array.
    fn query_selector_all(&self, selector: String) -> Vec<Element> {
        match self.doc.try_select(&selector) {
            Some(sel) => sel
                .nodes()
                .iter()
                .map(|n| Element::from_id(self.doc.clone(), n.id))
                .collect(),
            None => Vec::new(),
        }
    }

    /// `document.getElementById(id)` — return the first element whose
    /// `id` attribute equals `id`, or `null`.
    ///
    /// Implemented as a tree walk rather than a CSS-selector shortcut
    /// so we don't depend on `id` being a valid CSS identifier
    /// (real-world ids contain dots, brackets, slashes).
    fn get_element_by_id(&self, id: String) -> Option<Element> {
        let root = self.doc.tree.root();
        for descendant in root.descendants_it() {
            if !descendant.is_element() {
                continue;
            }
            if let Some(attr_id) = descendant.id_attr() {
                if attr_id.as_ref() == id.as_str() {
                    return Some(Element::from_id(self.doc.clone(), descendant.id));
                }
            }
        }
        None
    }

    /// `document.documentElement` — the root `<html>` element, or
    /// `null` if the parse is empty.
    #[qjs(get)]
    fn document_element(&self) -> Option<Element> {
        // Find the first <html> child of the document root. Using a
        // walk instead of `html_root()` because the latter panics on
        // empty fragments; we want a clean `null`.
        let root = self.doc.tree.root();
        for child in root.children_it(false) {
            if !child.is_element() {
                continue;
            }
            if let Some(name) = child.node_name() {
                if name.as_ref().eq_ignore_ascii_case("html") {
                    return Some(Element::from_id(self.doc.clone(), child.id));
                }
            }
        }
        None
    }

    /// `document.body` — the `<body>` element, or `null`.
    #[qjs(get)]
    fn body(&self) -> Option<Element> {
        self.doc
            .body()
            .map(|n| Element::from_id(self.doc.clone(), n.id))
    }

    /// `document.head` — the `<head>` element, or `null`.
    #[qjs(get)]
    fn head(&self) -> Option<Element> {
        self.doc
            .head()
            .map(|n| Element::from_id(self.doc.clone(), n.id))
    }

    /// `document.title` — text content of the `<title>` tag, or
    /// empty string.
    #[qjs(get)]
    fn title(&self) -> String {
        match self.doc.try_select("title") {
            Some(sel) => sel.text().trim().to_owned(),
            None => String::new(),
        }
    }

    /// `document.createElement(tagName)` — create a fresh orphan
    /// element with the given tag, no parent, no children, no
    /// attributes.
    ///
    /// The new node is allocated in the **same** `dom_query::Tree`
    /// as the rest of the document, so its `NodeId` is coherent with
    /// the node-keyed event-listener registry (see [`PROP_NODE_LISTENERS`]).
    /// `addEventListener` calls on the returned [`Element`] register
    /// against that registry, and dispatch via `element.click()` or
    /// `element.dispatchEvent(...)` after the node has been
    /// `appendChild`'d into the tree will find those listeners.
    ///
    /// Uses [`dom_query::Tree::new_element`] which creates an orphan
    /// element node (no parent, empty attribute list) and returns its
    /// stable [`NodeId`].
    fn create_element(&self, tag_name: String) -> Element {
        let node_ref = self.doc.tree.new_element(&tag_name);
        Element::from_id(self.doc.clone(), node_ref.id)
    }

    /// `document.createElementNS(namespace, qualifiedName)` — create an
    /// orphan element with the given qualified name. The namespace
    /// argument is currently **ignored**: heso renders an
    /// agent-shaped DOM, not an SVG/MathML rendering surface, so the
    /// element behaves as if it were `createElement(qualifiedName)`.
    ///
    /// Why expose this at all: framework bundlers (Preact, React,
    /// Vue) call `createElementNS` for SVG roots even on pages that
    /// don't actually use SVG visually, and a `not a function` throw
    /// halts the diff. Returning a plain element is correct enough
    /// for agents: the tag is preserved, attributes round-trip, the
    /// tree shape stays consistent.
    #[qjs(rename = "createElementNS")]
    fn create_element_ns(&self, _namespace: String, qualified_name: String) -> Element {
        let node_ref = self.doc.tree.new_element(&qualified_name);
        Element::from_id(self.doc.clone(), node_ref.id)
    }

    /// `document.createTextNode(data)` — create an orphan text node
    /// wrapping `data`. The returned value is an [`Element`] wrapper
    /// around the text-node's [`NodeId`] so it can be `appendChild`'d
    /// into the live tree alongside element nodes; `textContent` /
    /// `nodeValue` reads the data back.
    ///
    /// Phase 1B: the wrapper is the same [`Element`] type used for
    /// element nodes. Element-only properties (`tagName`, `id`,
    /// `classList`) return empty / no-op on a text-node wrapper.
    /// This is enough for the Preact / React / Vue render path,
    /// which only ever calls `appendChild` and `textContent`-style
    /// updates on text nodes.
    fn create_text_node(&self, data: String) -> Element {
        let node_ref = self.doc.tree.new_text(data);
        Element::from_id(self.doc.clone(), node_ref.id)
    }

    /// `document.getElementsByTagName(name)` — return every element
    /// whose tag matches `name`, in document order. `"*"` matches
    /// every element.
    ///
    /// The DOM spec says this returns a live `HTMLCollection`; here
    /// we return a plain array because (a) `querySelectorAll`
    /// already returns a plain array, (b) liveness is rarely the
    /// load-bearing property — callers iterate immediately — and
    /// (c) the GA snippet that prompted this method
    /// (`document.getElementsByTagName('script')[0]`) only ever
    /// indexes once.
    fn get_elements_by_tag_name(&self, name: String) -> Vec<Element> {
        let selector = if name == "*" { "*".to_owned() } else { name };
        match self.doc.try_select(&selector) {
            Some(sel) => sel
                .nodes()
                .iter()
                .map(|n| Element::from_id(self.doc.clone(), n.id))
                .collect(),
            None => Vec::new(),
        }
    }

    // ===== HTMLCollection accessors (WHATWG HTML §3.1.4) =====================
    //
    // `document.scripts` / `.forms` / `.images` / `.links` /
    // `.anchors` are spec-defined "live HTMLCollection"s of common
    // tag families. Per spec each is "live" — appending a new element
    // re-shows up in the collection on next read. We snapshot at read
    // time (each getter walks the tree and returns a plain Vec) for
    // the same reason `getElementsByTagName` does: real pages iterate
    // immediately, and re-reading the property produces an up-to-date
    // snapshot anyway.
    //
    // Bug-of-record: V2 agent findings reported all five accessors
    // return `undefined`, blocking the common scraping idiom of
    // `Array.from(document.forms).filter(...)`. See AGENT_FINDINGS_V2.md
    // "Bonus findings" + "Top NEW bugs" #6.

    /// `document.scripts` — array of every `<script>` element in the
    /// document, in document order.
    ///
    /// Snapshot HTMLCollection-shape per the module note on liveness.
    /// Spec: <https://html.spec.whatwg.org/multipage/dom.html#dom-document-scripts>.
    #[qjs(get)]
    fn scripts(&self) -> Vec<Element> {
        match self.doc.try_select("script") {
            Some(sel) => sel
                .nodes()
                .iter()
                .map(|n| Element::from_id(self.doc.clone(), n.id))
                .collect(),
            None => Vec::new(),
        }
    }

    /// `document.forms` — array of every `<form>` element in the
    /// document, in document order.
    ///
    /// Snapshot HTMLCollection-shape per the module note on liveness.
    /// Spec: <https://html.spec.whatwg.org/multipage/dom.html#dom-document-forms>.
    #[qjs(get)]
    fn forms(&self) -> Vec<Element> {
        match self.doc.try_select("form") {
            Some(sel) => sel
                .nodes()
                .iter()
                .map(|n| Element::from_id(self.doc.clone(), n.id))
                .collect(),
            None => Vec::new(),
        }
    }

    /// `document.images` — array of every `<img>` element in the
    /// document, in document order.
    ///
    /// Snapshot HTMLCollection-shape per the module note on liveness.
    /// Spec: <https://html.spec.whatwg.org/multipage/dom.html#dom-document-images>.
    #[qjs(get)]
    fn images(&self) -> Vec<Element> {
        match self.doc.try_select("img") {
            Some(sel) => sel
                .nodes()
                .iter()
                .map(|n| Element::from_id(self.doc.clone(), n.id))
                .collect(),
            None => Vec::new(),
        }
    }

    /// `document.links` — array of every `<a>` and `<area>` element
    /// that has an `href` content attribute, in document order. Per
    /// WHATWG: "links" specifically requires the `href` attribute
    /// (so anchors without one are excluded — they're not really
    /// links).
    ///
    /// Snapshot HTMLCollection-shape per the module note on liveness.
    /// Spec: <https://html.spec.whatwg.org/multipage/dom.html#dom-document-links>.
    #[qjs(get)]
    fn links(&self) -> Vec<Element> {
        // `a[href], area[href]` — comma selector returns both, in
        // document order. Per spec, only elements with the attribute
        // count; the attribute-presence filter is the `[href]`
        // matcher rather than a separate post-filter.
        match self.doc.try_select("a[href], area[href]") {
            Some(sel) => sel
                .nodes()
                .iter()
                .map(|n| Element::from_id(self.doc.clone(), n.id))
                .collect(),
            None => Vec::new(),
        }
    }

    /// `document.anchors` — array of every `<a>` element with a
    /// `name` content attribute, in document order. Deprecated in
    /// HTML5 (named anchors were superseded by `id`), but still
    /// part of the spec for backward compat.
    ///
    /// Snapshot HTMLCollection-shape per the module note on liveness.
    /// Spec: <https://html.spec.whatwg.org/multipage/dom.html#dom-document-anchors>.
    #[qjs(get)]
    fn anchors(&self) -> Vec<Element> {
        match self.doc.try_select("a[name]") {
            Some(sel) => sel
                .nodes()
                .iter()
                .map(|n| Element::from_id(self.doc.clone(), n.id))
                .collect(),
            None => Vec::new(),
        }
    }

    /// `document.title = value` — set the text content of the existing
    /// `<title>` element, or create one inside `<head>` if missing.
    ///
    /// The HTML spec says assigning to `document.title` mutates the
    /// first `<title>` element if any; otherwise it inserts a new
    /// `<title>` at the appropriate place (in `<head>` for an
    /// HTML document; the document element for SVG; etc.). We
    /// implement the HTML branch — which covers every page
    /// `heso eval-dom` and `heso open --js` are likely to touch.
    ///
    /// Inline script reaches for this constantly (SSR hydration
    /// often sets `document.title = ...` to reflect route changes),
    /// so a Phase 1C `<script>`-execution pass would be obviously
    /// broken without this setter.
    #[qjs(set, rename = "title")]
    fn set_title(&self, value: String) {
        if let Some(sel) = self.doc.try_select("title") {
            if let Some(first) = sel.nodes().first() {
                first.set_text(value.clone());
                return;
            }
        }
        // No <title> present — create one and attach to <head> (or
        // documentElement as a fallback).
        let parent = self.doc.head().or_else(|| self.doc.body()).or_else(|| {
            // Last resort: the document element.
            let root = self.doc.tree.root();
            root.children_it(false).find(|c| c.is_element())
        });
        let Some(parent) = parent else { return };
        // Build the new <title>X</title> via an HTML fragment so the
        // text is properly escaped + we don't need a low-level
        // node-construction API.
        let escaped = html_escape(&value);
        let fragment = format!("<title>{escaped}</title>");
        // Append by setting innerHTML on a temporary holder, then
        // re-parent. dom_query exposes `set_html` on a node, which
        // replaces children; instead we use the trick of appending to
        // a detached node. Simpler: just patch the parent's
        // children — but that loses sibling order. Use the dom_query
        // primitive that fits: `append_html` if available, otherwise
        // fall back to set_html-on-a-temp + append_child of the
        // single child. dom_query 0.28 has `append_html` on NodeRef.
        parent.append_html(fragment);
    }

    // ===== Trivial browser-globals batch =====================================
    //
    // Spec-required reads that frameworks gate on during init. Each
    // returns a fixed-shape value because heso doesn't have the
    // underlying machinery (load lifecycle, focus tracker, real cookie
    // jar) — but the read NEEDS to exist or the page crashes.

    /// `document.readyState` — always `"complete"`.
    ///
    /// heso parses + runs every `<script>` synchronously before
    /// returning from `eval_with_html` / `install_document`, so by
    /// the time JS gets to read `readyState`, the document is fully
    /// loaded. There's no "loading" or "interactive" state to expose.
    /// Frameworks (React, Vue, jQuery) gate boot on
    /// `readyState === 'complete'`; returning anything else makes them
    /// wait for a `DOMContentLoaded` event that will never fire in
    /// heso's synchronous-load model.
    #[qjs(get)]
    fn ready_state(&self) -> &'static str {
        "complete"
    }

    /// `document.activeElement` — currently always `document.body`.
    ///
    /// Per spec, `activeElement` is the focused element; the fallback
    /// when nothing is focused is `<body>` (or `<html>` if no body).
    /// heso has no real focus tracker yet, so we always return the
    /// spec fallback. React's selection-restoration code and many
    /// modal libraries call `document.activeElement` during init;
    /// returning `null` makes them throw `Cannot read properties of
    /// null`. Returning the body is the safest "nothing is focused
    /// right now" answer.
    #[qjs(get)]
    fn active_element(&self) -> Option<Element> {
        self.doc
            .body()
            .map(|n| Element::from_id(self.doc.clone(), n.id))
    }

    /// `document.cookie` getter — always `""`.
    ///
    /// Real cookie wiring is bigger than this batch: it needs to
    /// route through the same cookie jar `heso-engine-fetch` uses for
    /// HTTP requests, with respect for SameSite / Secure / HttpOnly
    /// flags. For now, returning empty string keeps cookie-reading
    /// init code from crashing while it waits for real cookies; pages
    /// that gate behavior on a specific cookie will fall to their
    /// default branch.
    #[qjs(get)]
    fn cookie(&self) -> &'static str {
        ""
    }

    /// `document.cookie = value` setter — no-op.
    ///
    /// Same rationale as the getter: real cookies aren't wired yet.
    /// Silent no-op (rather than throw) so analytics / consent
    /// libraries that set tracking cookies during init don't crash.
    /// A future cookie-jar agent will replace this with the real
    /// thing.
    #[qjs(set, rename = "cookie")]
    fn set_cookie(&self, _value: String) {
        // intentional no-op — see getter doc.
    }

    /// `document.contains(other)` — true if `other` is the document
    /// itself or a descendant of the document tree, false otherwise.
    ///
    /// Implemented as an ancestor walk from `other`'s node up to the
    /// root: a node is "in this document" iff its top ancestor is the
    /// document's root node. A detached element (created via
    /// `createElement` and never `appendChild`'d) walks to its own
    /// orphan root, which is the same `doc.tree.root()` as the live
    /// document — so we additionally require the walk to reach the
    /// document element via an actual parent edge.
    ///
    /// Frameworks call `document.contains(node)` before binding
    /// listeners and during teardown to avoid double-mounting; React
    /// 19's createRoot path is one caller. A missing method throws
    /// "document.contains is not a function".
    fn contains(&self, other: Option<Element>) -> bool {
        let Some(other) = other else { return false };
        // The document's root NodeId is the parse-tree root; anything
        // reachable by walking parents from `other` ending there is a
        // descendant. We also accept `other` being the document
        // element itself (an element node whose parent IS the root).
        let root_id = self.doc.tree.root().id;
        let Some(start) = self.doc.tree.get(&other.node_id) else {
            return false;
        };
        // Walk up from `other` until we either hit the document root
        // (success) or run out of ancestors (failure: detached).
        if start.id == root_id {
            return true;
        }
        let mut cur = start.parent();
        while let Some(n) = cur {
            if n.id == root_id {
                return true;
            }
            cur = n.parent();
        }
        false
    }

    /// `document.addEventListener(type, listener, options?)` —
    /// register a JS callback for document-level events.
    /// Listener storage is JS-side under the same
    /// `__nodeListeners` registry shape used by Element, keyed
    /// off the fixed sentinel `"document"`. The element-rooted
    /// dispatch path prepends the document so these listeners
    /// fire for bubbling events that started on a descendant
    /// element.
    fn add_event_listener<'js>(
        &self,
        ctx: Ctx<'js>,
        event_type: String,
        listener: Function<'js>,
        options: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        let (capture, once, passive) = parse_listener_options(&ctx, options.0)?;
        let map = document_listener_map(&ctx)?;
        add_listener_to_map(&ctx, &map, &event_type, &listener, capture, once, passive)
    }

    /// `document.removeEventListener(type, listener, options?)`.
    fn remove_event_listener<'js>(
        &self,
        ctx: Ctx<'js>,
        event_type: String,
        listener: Function<'js>,
        options: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        let (capture, _, _) = parse_listener_options(&ctx, options.0)?;
        if let Some(map) = document_listener_map_opt(&ctx)? {
            remove_listener_from_map(&ctx, &map, &event_type, &listener, capture)?;
        }
        Ok(())
    }

    /// `document.dispatchEvent(event)` — fire `event` against
    /// document-level listeners only. No tree walk (the document is
    /// the root). Returns `false` iff a listener called
    /// `preventDefault()` and the event is cancelable.
    fn dispatch_event<'js>(
        &self,
        ctx: Ctx<'js>,
        event: Value<'js>,
    ) -> rquickjs::Result<bool> {
        let map = document_listener_map_opt(&ctx)?;
        let doc_value: Value<'js> = ctx.globals().get("document")?;
        events::dispatch_with_map(&ctx, map.as_ref(), Some(doc_value), event)    }
}

/// Escape `s` so it is safe to embed in HTML text content.
///
/// Phase-1C scope: we only need to handle the title-setter path, so
/// the bare-minimum substitutions (`& < >`) suffice — `<title>` is a
/// "raw text" element per the HTML spec, meaning the parser ignores
/// `<` inside it, but we still escape both `&` (which is recognized
/// as a numeric reference start) and angle brackets for defense in
/// depth. Quote escapement is unnecessary because we never embed in
/// an attribute.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// A handle to a single element in a [`Document`]'s tree.
///
/// Holds a refcounted handle to the parent [`dom_query::Document`] plus
/// the [`dom_query::NodeId`] of this element. All access is via the
/// parse tree — we never store a borrowed [`dom_query::NodeRef`]
/// because that would tie the type to a specific borrow lifetime that
/// doesn't survive JavaScript call boundaries.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class]
pub struct Element {
    #[qjs(skip_trace)]
    doc: Arc<DqDocument>,
    #[qjs(skip_trace)]
    node_id: NodeId,
}

impl Element {
    /// Construct from the (doc, id) pair. Internal — callers reach
    /// this via [`Document`] queries.
    fn from_id(doc: Arc<DqDocument>, node_id: NodeId) -> Self {
        Self { doc, node_id }
    }

    /// Resolve this element's [`dom_query::NodeRef`] in the backing
    /// tree. Returns `None` if the node id has been recycled —
    /// shouldn't happen via our constructors, but is defensive.
    fn node_ref(&self) -> Option<NodeRef<'_>> {
        self.doc.tree.get(&self.node_id)
    }

    /// Rust-side detach helper used by tests. Mirrors the JS-facing
    /// `remove_child` but skips the listener-registry cleanup (no
    /// `Ctx` available outside a JS call). Use `Document::query_selector_inner`
    /// to obtain handles in tests.
    #[cfg(test)]
    fn remove_child_rs(&self, child: Element) -> Element {
        if let Some(child_ref) = self.doc.tree.get(&child.node_id) {
            if let Some(parent) = child_ref.parent() {
                if parent.id == self.node_id {
                    child_ref.remove_from_parent();
                }
            }
        }
        child
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl Element {
    /// `element.tagName` — uppercase per the DOM spec
    /// (e.g. `"DIV"`, `"A"`, `"H1"`). Empty string for non-element or
    /// stale nodes (shouldn't be reachable through our constructors).
    #[qjs(get)]
    fn tag_name(&self) -> String {
        self.node_ref()
            .and_then(|n| n.node_name())
            .map(|t| t.to_ascii_uppercase())
            .unwrap_or_default()
    }

    /// `element.localName` — lowercase per the DOM spec.
    #[qjs(get)]
    fn local_name(&self) -> String {
        self.node_ref()
            .and_then(|n| n.node_name())
            .map(|t| t.to_string())
            .unwrap_or_default()
    }

    /// `element.id` — the element's `id` attribute, or empty string.
    #[qjs(get)]
    fn id(&self) -> String {
        self.node_ref()
            .and_then(|n| n.id_attr())
            .map(|t| t.to_string())
            .unwrap_or_default()
    }

    /// `element.id = value` — set the element's `id` attribute.
    /// Standard DOM IDL: `id` is a reflected attribute.
    #[qjs(set, rename = "id")]
    fn set_id(&self, value: String) {
        if let Some(n) = self.node_ref() {
            n.set_attr("id", &value);
        }
    }

    /// `element.className` — the element's `class` attribute, or
    /// empty string. (Parsed list lives on `classList`.)
    #[qjs(get)]
    fn class_name(&self) -> String {
        self.node_ref()
            .and_then(|n| n.class())
            .map(|t| t.to_string())
            .unwrap_or_default()
    }

    /// `element.className = value` — write the element's `class`
    /// content attribute verbatim, per the [DOM spec][spec].
    ///
    /// Framework code reaches for this constantly: Tailwind's `apply`
    /// directive, Vue's `:class` static-path, jQuery's `addClass`, and
    /// every utility-CSS pattern writes `el.className = '...'`. Without
    /// a setter, those assignments silently no-op'd and styling broke.
    ///
    /// `Coerced<String>` (rather than `String`) is load-bearing:
    /// frameworks pass numbers, bools, and template-literal results
    /// whose coercion isn't always a `string` typeof — strict typing
    /// throws mid-render. `Coerced` applies WebIDL `DOMString`
    /// semantics, so `null` / `undefined` stringify to `"null"` /
    /// `"undefined"`. Don't special-case those; that matches the spec.
    ///
    /// Setting `""` writes an empty `class` attribute rather than
    /// removing it — `removeAttribute('class')` is a different
    /// concern, and the empty-string form is allowable per spec.
    ///
    /// [spec]: https://dom.spec.whatwg.org/#dom-element-classname
    #[qjs(set, rename = "className")]
    fn set_class_name(&self, value: rquickjs::Coerced<String>) {
        if let Some(n) = self.node_ref() {
            n.set_attr("class", &value.0);
        }
    }

    /// `element.textContent` — concatenated text of this element and
    /// all descendants, in document order.
    #[qjs(get, rename = "textContent")]
    fn text_content(&self) -> String {
        self.node_ref()
            .map(|n| n.text().to_string())
            .unwrap_or_default()
    }

    /// `element.textContent = value` — replace the element's children
    /// with a single text node containing `value`. Per the DOM spec,
    /// this does **not** parse `value` as HTML — it is set verbatim
    /// as a text node.
    #[qjs(set, rename = "textContent")]
    fn set_text_content(&self, value: String) {
        if let Some(n) = self.node_ref() {
            n.set_text(value);
        }
    }

    /// `element.innerHTML` — serialized HTML of this element's
    /// children.
    ///
    /// Explicit rename: `camelCase` would produce `innerHtml`, but the
    /// DOM spec is `innerHTML` (all caps for the acronym).
    #[qjs(get, rename = "innerHTML")]
    fn inner_html(&self) -> String {
        self.node_ref()
            .map(|n| n.inner_html().to_string())
            .unwrap_or_default()
    }

    /// `element.innerHTML = value` — parse `value` as an HTML fragment
    /// and replace this element's children with the parsed nodes.
    #[qjs(set, rename = "innerHTML")]
    fn set_inner_html(&self, value: String) {
        if let Some(n) = self.node_ref() {
            n.set_html(value);
        }
    }

    /// `element.outerHTML` — serialized HTML of this element including
    /// itself.
    #[qjs(get, rename = "outerHTML")]
    fn outer_html(&self) -> String {
        self.node_ref()
            .map(|n| n.html().to_string())
            .unwrap_or_default()
    }

    /// `element.style` — a Proxy over the element's `style` attribute
    /// shaped like the DOM [`CSSStyleDeclaration`][spec] interface.
    ///
    /// Reads/writes round-trip through the inline `style="..."`
    /// attribute, so `style.color = "red"` becomes visible via
    /// `outerHTML` / `getAttribute('style')` and vice versa. The
    /// Proxy itself is created by `globalThis.__hesoMakeStyleProxy`
    /// (installed in [`crate::engine::install_style_proxy`]); see the
    /// `STYLE_PROXY_BOOTSTRAP` constant there for trap-by-trap
    /// semantics — in particular the `has` trap is gated on a real
    /// allow-list of CSS property names so React's hydration
    /// feature-detect (`for (t in n) if (t in Ct) ...`) discriminates
    /// real CSS properties from arbitrary attribute keys.
    ///
    /// On a stale element handle (the underlying node has been
    /// detached and recycled — defensive only; not reachable via the
    /// public constructors), reads return empty and writes silently
    /// no-op.
    ///
    /// [spec]: https://drafts.csswg.org/cssom/#cssstyledeclaration
    #[qjs(get)]
    fn style<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
    ) -> rquickjs::Result<Value<'js>> {
        let element = this.0.borrow().clone();

        // `read` returns the current `style` attribute value (or "").
        let read_el = element.clone();
        let read = Function::new(ctx.clone(), move || -> String {
            read_el
                .node_ref()
                .and_then(|n| n.attr("style"))
                .map(|s| s.to_string())
                .unwrap_or_default()
        })?;

        // `write` replaces the `style` attribute with the given
        // serialized string. Empty string clears the attribute
        // (mirrors `setAttribute('style', '')` semantics — the
        // attribute stays but is empty; cheap to keep consistent
        // with the read path).
        let write_el = element;
        let write = Function::new(ctx.clone(), move |value: String| {
            if let Some(n) = write_el.node_ref() {
                n.set_attr("style", &value);
            }
        })?;

        // Reach for the JS-side factory installed at engine boot. If
        // the factory is missing (shouldn't happen — `install_style_proxy`
        // runs unconditionally), fall back to returning `null` so the
        // caller sees a TypeError on member access rather than a
        // panic.
        let globals = ctx.globals();
        let factory: Function<'js> = globals.get("__hesoMakeStyleProxy")?;
        let proxy: Value<'js> = factory.call((read, write))?;
        Ok(proxy)
    }

    /// `element.getAttribute(name)` — return the attribute value, or
    /// `null` if not present.
    fn get_attribute(&self, name: String) -> Option<String> {
        self.node_ref()
            .and_then(|n| n.attr(&name))
            .map(|t| t.to_string())
    }

    /// `element.hasAttribute(name)` — return true if the attribute is
    /// present (even if empty).
    fn has_attribute(&self, name: String) -> bool {
        self.node_ref().map(|n| n.has_attr(&name)).unwrap_or(false)
    }

    /// `element.setAttribute(name, value)` — set or replace the
    /// attribute named `name` with `value`. Silently no-ops on a stale
    /// element handle.
    fn set_attribute(
        &self,
        name: String,
        value: Option<rquickjs::Coerced<String>>,
    ) {
        // Framework renderers (Preact, React, Vue, lit-html) routinely
        // call `setAttribute(name, value)` with non-string `value`
        // arguments — `true` / `false` for boolean attrs, numbers for
        // `tabindex` / `width`, `null` to mean "remove this attr".
        // Strict-typing the second argument as `String` throws
        // mid-render, which halts hydration on otherwise-clean pages.
        // `Coerced<String>` accepts whatever JS hands us and applies
        // `String(value)` semantics (so `true` → "true", `42` → "42");
        // wrapping in `Option` lets `null` and `undefined` route to
        // `removeAttribute` to match the spec's "if value is null,
        // remove the named attribute" branch.
        if let Some(n) = self.node_ref() {
            match value {
                Some(s) => n.set_attr(&name, &s.0),
                None => n.remove_attr(&name),
            }
        }
    }

    /// `element.removeAttribute(name)` — remove the attribute named
    /// `name`. Silently no-ops if the attribute isn't present.
    fn remove_attribute(&self, name: String) {
        if let Some(n) = self.node_ref() {
            n.remove_attr(&name);
        }
    }

    /// `element.querySelector(selector)` — return the first descendant
    /// matching `selector`, or `null`.
    ///
    /// Scope: descendants only. `selector` resolves against the
    /// subtree rooted at this element, not the full document.
    fn query_selector(&self, selector: String) -> Option<Element> {
        let n = self.node_ref()?;
        // Wrap this node as a one-element Selection, then run a
        // descendant select against `selector`.
        let sel = dom_query::Selection::from(n).try_select(&selector)?;
        let nodes = sel.nodes();
        let first = nodes.first()?;
        Some(Element::from_id(self.doc.clone(), first.id))
    }

    /// `element.querySelectorAll(selector)` — return all descendants
    /// matching `selector`, in document order.
    fn query_selector_all(&self, selector: String) -> Vec<Element> {
        let n = match self.node_ref() {
            Some(n) => n,
            None => return Vec::new(),
        };
        match dom_query::Selection::from(n).try_select(&selector) {
            Some(sel) => sel
                .nodes()
                .iter()
                .map(|nr| Element::from_id(self.doc.clone(), nr.id))
                .collect(),
            None => Vec::new(),
        }
    }

    /// `element.children` — direct element children (skip text /
    /// comment nodes), in document order.
    #[qjs(get)]
    fn children(&self) -> Vec<Element> {
        let n = match self.node_ref() {
            Some(n) => n,
            None => return Vec::new(),
        };
        n.element_children()
            .into_iter()
            .map(|nr| Element::from_id(self.doc.clone(), nr.id))
            .collect()
    }

    /// `element.parentNode` — the direct parent in the tree
    /// regardless of node type (element / document fragment / etc.),
    /// or `null` for the root. Distinct from
    /// [`Self::parent_element`], which skips non-element ancestors.
    ///
    /// Returned as an [`Element`] wrapper because Phase 1B doesn't
    /// have a separate `Node` class — callers that only need
    /// `appendChild` / `removeChild` / `insertBefore` can use the
    /// shared wrapper. Element-only properties (`tagName`, `id`)
    /// will look odd on document-typed parents but are not
    /// load-bearing for the render path.
    #[qjs(get)]
    fn parent_node(&self) -> Option<Element> {
        let n = self.node_ref()?;
        n.parent()
            .map(|p| Element::from_id(self.doc.clone(), p.id))
    }

    /// `element.parentElement` — closest element ancestor, or `null`
    /// for the root.
    #[qjs(get)]
    fn parent_element(&self) -> Option<Element> {
        let mut cur = self.node_ref()?.parent();
        while let Some(n) = cur {
            if n.is_element() {
                return Some(Element::from_id(self.doc.clone(), n.id));
            }
            cur = n.parent();
        }
        None
    }

    /// `element.appendChild(child)` — move `child` to be the last
    /// child of `self`.
    ///
    /// Matches DOM `Node.appendChild` semantics: if `child` already
    /// has a parent, it is removed from there first
    /// (`dom_query::NodeRef::append_child` calls
    /// `remove_from_parent` on the child before re-parenting).
    ///
    /// Returns the same `child` handle so JS callers can chain.
    fn append_child(&self, child: Element) -> Element {
        if let Some(n) = self.node_ref() {
            n.append_child(&child.node_id);
        }
        child
    }

    /// `element.insertBefore(newNode, referenceNode)` — insert
    /// `newNode` as a child of `self` immediately before
    /// `referenceNode`. If `referenceNode` is `null` / `undefined`,
    /// behaves like [`Self::append_child`] (appends to the end), per
    /// the DOM spec.
    ///
    /// If `referenceNode` is not a child of `self`, this is currently
    /// a no-op (the spec says `NotFoundError`; aligning with that is
    /// a Phase 1C follow-up). If `newNode` is already in the tree,
    /// `dom_query` re-parents it cleanly.
    fn insert_before(&self, new_node: Element, reference_node: Option<Element>) -> Element {
        match reference_node {
            Some(reference) => {
                if let Some(ref_n) = self.doc.tree.get(&reference.node_id) {
                    if let Some(parent) = ref_n.parent() {
                        if parent.id == self.node_id {
                            ref_n.insert_before(&new_node.node_id);
                        }
                    }
                }
            }
            None => {
                if let Some(n) = self.node_ref() {
                    n.append_child(&new_node.node_id);
                }
            }
        }
        new_node
    }

    /// `element.removeChild(child)` — detach `child` from `self`.
    ///
    /// If `child` is not a direct child of `self`, this is a no-op
    /// (the DOM spec actually throws `NotFoundError`; alignment with
    /// that is a Phase 1C follow-up).
    ///
    /// Returns the same `child` handle so JS callers can chain.
    fn remove_child<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        child: Element,
    ) -> rquickjs::Result<Element> {
        let self_id = this.0.borrow().node_id;
        let doc = this.0.borrow().doc.clone();
        // Collect descendant ids (incl. child itself) BEFORE detaching,
        // so we can clean their listener registry entries.
        let mut to_clear: Vec<NodeId> = Vec::new();
        if let Some(child_ref) = doc.tree.get(&child.node_id) {
            if let Some(parent) = child_ref.parent() {
                if parent.id == self_id {
                    to_clear.push(child.node_id);
                    for descendant in child_ref.descendants_it() {
                        to_clear.push(descendant.id);
                    }
                    child_ref.remove_from_parent();
                }
            }
        }
        // Drop registry entries for every NodeId in the removed
        // subtree, so stale listener records don't (a) leak across
        // long-lived sessions, and (b) contaminate a future element
        // that happens to receive the same NodeId. (dom_query 0.28
        // does not currently recycle NodeIds, but the cleanup is
        // cheap and protects against that becoming load-bearing.)
        if !to_clear.is_empty() {
            clear_listeners_for_nodes(&ctx, &to_clear)?;
            clear_idl_state_for_nodes(&ctx, &to_clear)?;
        }
        Ok(child)
    }

    /// `node.nodeType` — the WHATWG node-type constant: 1 for
    /// element, 3 for text, 8 for comment, 9 for document, 0 as a
    /// conservative fallback. Frameworks gate on `nodeType === 1`
    /// before they'll mount into a container (React 19's
    /// `createRoot` throws "Target container is not a DOM
    /// element" otherwise), so this isn't optional.
    #[qjs(get)]
    fn node_type(&self) -> u32 {
        match self.node_ref() {
            Some(n) if n.is_element() => 1,
            Some(n) if n.is_text() => 3,
            Some(n) if n.is_comment() => 8,
            Some(n) if n.is_document() => 9,
            _ => 0,
        }
    }

    /// `node.nodeName` — the uppercase tag name for elements,
    /// `"#text"` / `"#comment"` / `"#document"` for non-elements.
    /// Mirrors `tagName` for element nodes but is defined on every
    /// node type per the DOM spec, which the SSR-hydration walk
    /// (childNodes / firstChild) needs.
    #[qjs(get)]
    fn node_name(&self) -> String {
        match self.node_ref() {
            Some(n) if n.is_text() => "#text".to_owned(),
            Some(n) if n.is_comment() => "#comment".to_owned(),
            Some(n) if n.is_document() => "#document".to_owned(),
            Some(n) => n
                .node_name()
                .map(|s| s.as_ref().to_ascii_uppercase())
                .unwrap_or_default(),
            None => String::new(),
        }
    }

    /// `node.childNodes` — direct children of any node type
    /// (element, text, comment), in document order. Returned as a
    /// plain JS array because Phase 1B does not implement live
    /// `NodeList` semantics; callers iterate immediately
    /// (`Array.from(...)`, `forEach`, indexed access) and React /
    /// Preact / lit-html never depend on the liveness of the
    /// returned collection — they re-read on each diff pass.
    ///
    /// Distinct from `children` (which is element-only):
    /// `childNodes` is the load-bearing surface for SSR-hydration
    /// reconcilers that need to walk text-node siblings, and a
    /// React `cloneNode(true)` round-trip is meaningless without
    /// text-node visibility here.
    ///
    /// `dom_query::NodeRef::children_it(false)` iterates all child
    /// node types forward — the `false` argument means "do not
    /// reverse the iteration", not "skip text". Confirmed via the
    /// upstream source in dom_query 0.28.
    #[qjs(get)]
    fn child_nodes(&self) -> Vec<Element> {
        let n = match self.node_ref() {
            Some(n) => n,
            None => return Vec::new(),
        };
        n.children_it(false)
            .map(|nr| Element::from_id(self.doc.clone(), nr.id))
            .collect()
    }

    /// `node.firstChild` — first child of any node type, or `null`.
    ///
    /// Counterpart to `firstElementChild` (which filters to
    /// elements); React's reconciler depends on this returning text
    /// nodes too when matching server-rendered output against the
    /// client tree.
    #[qjs(get)]
    fn first_child<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.node_ref().and_then(|n| n.first_child()) {
            Some(child) => {
                let el = Element::from_id(self.doc.clone(), child.id);
                let instance = Class::instance(ctx.clone(), el)?;
                Ok(instance.into_value())
            }
            // DOM spec: firstChild is `null` (not `undefined`) when
            // the node has no children. rquickjs's `Option<T>` →
            // `undefined` conversion is the wrong shape; framework
            // code branches on `child === null` (strict), so we
            // emit JS `null` explicitly.
            None => Ok(Value::new_null(ctx)),
        }
    }

    /// `node.lastChild` — last child of any node type, or `null`.
    /// Uses `dom_query::NodeRef::last_child` directly (cheaper than
    /// walking `children_it` to the end).
    #[qjs(get)]
    fn last_child<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.node_ref().and_then(|n| n.last_child()) {
            Some(child) => {
                let el = Element::from_id(self.doc.clone(), child.id);
                let instance = Class::instance(ctx.clone(), el)?;
                Ok(instance.into_value())
            }
            None => Ok(Value::new_null(ctx)),
        }
    }

    /// `node.nextSibling` — next sibling of any node type, or
    /// `null`. Walks text-node siblings too — a `<a>a</a>text<b>`
    /// chain reads as `<a>.nextSibling` returning the text node.
    #[qjs(get)]
    fn next_sibling<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.node_ref().and_then(|n| n.next_sibling()) {
            Some(sib) => {
                let el = Element::from_id(self.doc.clone(), sib.id);
                let instance = Class::instance(ctx.clone(), el)?;
                Ok(instance.into_value())
            }
            None => Ok(Value::new_null(ctx)),
        }
    }

    /// `node.previousSibling` — previous sibling of any node
    /// type, or `null`.
    #[qjs(get)]
    fn previous_sibling<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self.node_ref().and_then(|n| n.prev_sibling()) {
            Some(sib) => {
                let el = Element::from_id(self.doc.clone(), sib.id);
                let instance = Class::instance(ctx.clone(), el)?;
                Ok(instance.into_value())
            }
            None => Ok(Value::new_null(ctx)),
        }
    }

    /// `element.firstElementChild` — first child that is an
    /// element, skipping text and comment siblings, or `null`.
    /// Counterpart to the existing `children[0]` shape.
    #[qjs(get)]
    fn first_element_child<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let n = match self.node_ref() {
            Some(n) => n,
            None => return Ok(Value::new_null(ctx)),
        };
        for child in n.children_it(false) {
            if child.is_element() {
                let el = Element::from_id(self.doc.clone(), child.id);
                let instance = Class::instance(ctx.clone(), el)?;
                return Ok(instance.into_value());
            }
        }
        Ok(Value::new_null(ctx))
    }

    /// `element.lastElementChild` — last child that is an
    /// element, or `null`. Uses `children_it(true)` (reverse
    /// iteration) so we stop at the first element from the end,
    /// avoiding a full children walk on long lists.
    #[qjs(get)]
    fn last_element_child<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let n = match self.node_ref() {
            Some(n) => n,
            None => return Ok(Value::new_null(ctx)),
        };
        for child in n.children_it(true) {
            if child.is_element() {
                let el = Element::from_id(self.doc.clone(), child.id);
                let instance = Class::instance(ctx.clone(), el)?;
                return Ok(instance.into_value());
            }
        }
        Ok(Value::new_null(ctx))
    }

    /// `element.nextElementSibling` — next sibling that is an
    /// element, or `null`. Walks the `next_sibling` chain past any
    /// text / comment nodes — React's reconciler reads this to
    /// match up server-rendered element siblings while ignoring
    /// the whitespace text between them.
    #[qjs(get)]
    fn next_element_sibling<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let mut cur = self.node_ref().and_then(|n| n.next_sibling());
        while let Some(n) = cur {
            if n.is_element() {
                let el = Element::from_id(self.doc.clone(), n.id);
                let instance = Class::instance(ctx.clone(), el)?;
                return Ok(instance.into_value());
            }
            cur = n.next_sibling();
        }
        Ok(Value::new_null(ctx))
    }

    /// `element.previousElementSibling` — previous sibling that
    /// is an element, or `null`.
    #[qjs(get)]
    fn previous_element_sibling<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let mut cur = self.node_ref().and_then(|n| n.prev_sibling());
        while let Some(n) = cur {
            if n.is_element() {
                let el = Element::from_id(self.doc.clone(), n.id);
                let instance = Class::instance(ctx.clone(), el)?;
                return Ok(instance.into_value());
            }
            cur = n.prev_sibling();
        }
        Ok(Value::new_null(ctx))
    }

    /// `element.childElementCount` — count of element children
    /// (skipping text and comment nodes). Used as a hydration
    /// sentinel by React: when the server-rendered HTML's child
    /// count disagrees with the client's expected count, React
    /// throws "Hydration failed".
    #[qjs(get)]
    fn child_element_count(&self) -> u32 {
        let n = match self.node_ref() {
            Some(n) => n,
            None => return 0,
        };
        n.children_it(false).filter(|c| c.is_element()).count() as u32
    }

    /// `node.hasChildNodes()` — true if this node has any child
    /// of any type. Defined for every node type per the DOM spec.
    fn has_child_nodes(&self) -> bool {
        self.node_ref()
            .map(|n| n.first_child().is_some())
            .unwrap_or(false)
    }

    /// `node.contains(other)` — true if `other` is a descendant
    /// of `self`, or `self` itself, per the DOM spec. `null`/missing
    /// `other` → `false` (the spec allows it; we get that for free
    /// via `Option<Element>`).
    ///
    /// Implementation walks `other`'s `ancestors_it()` and compares
    /// node ids against `self.node_id`. The walk is
    /// O(depth-of-other), which is the cheapest direction
    /// (descending from `self` would be O(subtree-size)).
    fn contains(&self, other: Option<Element>) -> bool {
        let Some(other) = other else { return false };
        if other.node_id == self.node_id {
            return true;
        }
        let Some(other_ref) = other.node_ref() else {
            return false;
        };
        // `ancestors_it(None)` yields all ancestors up to the
        // document root, excluding `other` itself (already
        // compared above). Both `other` and `self` must live in
        // the same `Tree` for the id-equality check to be
        // meaningful — guaranteed because `Element` instances are
        // minted only by `Document::*` methods on a single
        // `Arc<DqDocument>`.
        for ancestor in other_ref.ancestors_it(None) {
            if ancestor.id == self.node_id {
                return true;
            }
        }
        false
    }

    /// `node.isConnected` — true iff the node is in the document
    /// tree (i.e., a `parent()` walk eventually reaches the
    /// `dom_query::NodeData::Document` root). Returns `false` for
    /// `createElement`-built orphans that have never been
    /// `appendChild`'d, and for nodes that have been detached via
    /// `remove()` / `removeChild`.
    ///
    /// React's `createRoot` checks `container.isConnected` before
    /// mounting; passing an orphan container surfaces as
    /// "Target container is not a DOM element" otherwise.
    #[qjs(get)]
    fn is_connected(&self) -> bool {
        let Some(n) = self.node_ref() else {
            return false;
        };
        // Walk parents; if any ancestor is the Document root, the
        // node is connected. Orphans (no parent) return `false`
        // immediately because `ancestors_it` yields nothing.
        for ancestor in n.ancestors_it(None) {
            if ancestor.is_document() {
                return true;
            }
        }
        false
    }

    /// `node.cloneNode(deep?)` — return a copy of this node.
    ///
    /// Shallow (`deep` falsy or absent): copy this node's type and
    /// attributes (for elements) or text data (for text nodes)
    /// into a fresh orphan node in the same `dom_query::Tree`. No
    /// children are cloned.
    ///
    /// Deep (`deep === true`): also recursively clone every
    /// descendant. Each cloned subtree shares the source's tag
    /// names, attribute values, and text content. Listeners are
    /// NOT copied — the DOM spec is explicit that listeners
    /// registered via `addEventListener` do not clone. Inline
    /// handlers (`onclick="..."`) ARE preserved because they're
    /// stored as attributes.
    ///
    /// Used heavily by `lit-html` (templates clone a parsed
    /// `<template>` body per render) and `preact/compat` (the
    /// shim for React-compat code), so a `cloneNode is not a
    /// function` throw halts hydration on otherwise-clean pages.
    ///
    /// `dom_query` 0.28 does not expose a public `clone_node`
    /// primitive at the time of writing, so the implementation
    /// walks `children_it(false)` manually and rebuilds the
    /// subtree via `Tree::new_element` / `Tree::new_text`. Comment
    /// nodes are skipped (placeholder empty text) because
    /// dom_query's `Tree` has no public `new_comment` constructor
    /// and they don't appear in SSR output that matters for
    /// hydration.
    fn clone_node(&self, deep: Opt<bool>) -> Element {
        let deep = deep.0.unwrap_or(false);
        let new_id = clone_subtree(&self.doc, self.node_id, deep);
        Element::from_id(self.doc.clone(), new_id)
    }

    /// `node.remove()` — detach `self` from its parent. No-op on
    /// a node that has no parent (already-orphan `createElement`
    /// nodes or roots).
    ///
    /// Listener-registry entries for every NodeId in the removed
    /// subtree are dropped, matching [`Self::remove_child`]'s
    /// cleanup semantics: the registry is keyed off
    /// `dom_query::NodeId`, and stale entries would (a) leak
    /// across long-lived sessions and (b) contaminate a future
    /// element that happened to be allocated the same id.
    ///
    /// Used heavily by SPA route teardown and by
    /// `@floating-ui/dom` (popover dismissal walks
    /// `popover.remove()` on close).
    fn remove<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<()> {
        let self_id = this.0.borrow().node_id;
        let doc = this.0.borrow().doc.clone();
        let mut to_clear: Vec<NodeId> = Vec::new();
        if let Some(node_ref) = doc.tree.get(&self_id) {
            // Element.remove is defined as
            // "If this's parent is null, then return" — no-op
            // when already detached.
            if node_ref.parent().is_some() {
                to_clear.push(self_id);
                for descendant in node_ref.descendants_it() {
                    to_clear.push(descendant.id);
                }
                node_ref.remove_from_parent();
            }
        }
        if !to_clear.is_empty() {
            clear_listeners_for_nodes(&ctx, &to_clear)?;
        }
        Ok(())
    }

    /// `element.classList` — a freshly-constructed [`DomTokenList`]
    /// view of the element's space-separated `class` attribute.
    ///
    /// The DOM spec says `classList` is live — calls to
    /// `el.classList.add(...)` reflect on the element. Our
    /// [`DomTokenList`] holds an [`Element`] handle (which is itself a
    /// thin `(Arc<Document>, NodeId)` pair), so the liveness
    /// guarantee is preserved.
    #[qjs(get)]
    fn class_list(&self) -> DomTokenList {
        DomTokenList::new(self.clone())
    }

    /// `element.value` — IDL value getter for form controls per the
    /// HTML spec. Returns the *current* value (the typed-in text once
    /// `.value = ...` has fired), falling back to the `value` content
    /// attribute (= [`Self::default_value`]) when the IDL setter has
    /// not yet run on this node.
    ///
    /// The split matters for every controlled-input library: React
    /// Hook Form, Formik, and React's own controlled-input pattern
    /// detect dirty state by comparing `.value` against
    /// `getAttribute('value')` / `.defaultValue`. Collapsing the two
    /// (the Phase-1B shortcut) made every controlled `<input>` in
    /// React / Vue / Solid look pristine after a write.
    ///
    /// IDL state lives in the document-side
    /// [`PROP_NODE_IDL_STATE`] registry, keyed by [`NodeId`]; see the
    /// module-level helpers for the storage shape.
    ///
    /// Spec: <https://html.spec.whatwg.org/multipage/input.html#dom-input-value>.
    #[qjs(get)]
    fn value<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        let (node_id, doc) = {
            let borrowed = this.0.borrow();
            (borrowed.node_id, borrowed.doc.clone())
        };
        // If the IDL setter has fired on this node, prefer the IDL
        // value. Otherwise fall back to the content attribute.
        if let Some(state) = element_idl_state_opt(&ctx, node_id)? {
            let dirty: bool = state.get::<_, Option<bool>>(IDL_VALUE_DIRTY)?.unwrap_or(false);
            if dirty {
                let v: Option<String> = state.get::<_, Option<String>>(IDL_VALUE)?;
                return Ok(v.unwrap_or_default());
            }
        }
        Ok(doc
            .tree
            .get(&node_id)
            .and_then(|n| n.attr("value"))
            .map(|t| t.to_string())
            .unwrap_or_default())
    }

    /// `element.value = "..."` — IDL value setter. Stores the new
    /// value in the per-node IDL state map and marks the dirty bit;
    /// **does not** touch the `value` content attribute (= the spec's
    /// `defaultValue`), so `getAttribute('value')` keeps returning the
    /// original HTML.
    ///
    /// Does not itself fire `input` / `change` — those are dispatched
    /// by the caller (e.g. [`crate::JsEngine::set_input_value`]).
    #[qjs(set, rename = "value")]
    fn set_value<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        let node_id = this.0.borrow().node_id;
        let state = element_idl_state(&ctx, node_id)?;
        state.set(IDL_VALUE, value.0)?;
        state.set(IDL_VALUE_DIRTY, true)?;
        Ok(())
    }

    /// `element.defaultValue` — the `value` content attribute,
    /// reflecting the HTML-authored default. Empty string when the
    /// attribute is absent. The IDL [`Self::value`] property may
    /// diverge after a JS-side write; this stays pinned to the
    /// attribute. Spec:
    /// <https://html.spec.whatwg.org/multipage/input.html#dom-input-defaultvalue>.
    #[qjs(get, rename = "defaultValue")]
    fn default_value(&self) -> String {
        self.node_ref()
            .and_then(|n| n.attr("value"))
            .map(|t| t.to_string())
            .unwrap_or_default()
    }

    /// `element.defaultValue = "..."` — write the `value` content
    /// attribute. Per spec, this is the IDL reflection of the
    /// attribute, so assigning here calls `setAttribute('value', v)`.
    #[qjs(set, rename = "defaultValue")]
    fn set_default_value(&self, value: rquickjs::Coerced<String>) {
        if let Some(n) = self.node_ref() {
            n.set_attr("value", &value.0);
        }
    }

    /// `element.checked` — IDL checkedness getter. Mirrors `.value`:
    /// returns the in-memory bit once the JS setter has fired, falls
    /// back to `hasAttribute('checked')` (= [`Self::default_checked`])
    /// until then.
    ///
    /// Spec:
    /// <https://html.spec.whatwg.org/multipage/input.html#dom-input-checked>.
    #[qjs(get)]
    fn checked<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<bool> {
        let (node_id, doc) = {
            let borrowed = this.0.borrow();
            (borrowed.node_id, borrowed.doc.clone())
        };
        if let Some(state) = element_idl_state_opt(&ctx, node_id)? {
            let dirty: bool = state
                .get::<_, Option<bool>>(IDL_CHECKED_DIRTY)?
                .unwrap_or(false);
            if dirty {
                let v: Option<bool> = state.get::<_, Option<bool>>(IDL_CHECKED)?;
                return Ok(v.unwrap_or(false));
            }
        }
        Ok(doc
            .tree
            .get(&node_id)
            .map(|n| n.has_attr("checked"))
            .unwrap_or(false))
    }

    /// `element.checked = bool` — IDL checkedness setter. Stores the
    /// new bit and marks the dirty flag; does not touch the `checked`
    /// content attribute (= `defaultChecked`).
    #[qjs(set, rename = "checked")]
    fn set_checked<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<bool>,
    ) -> rquickjs::Result<()> {
        let node_id = this.0.borrow().node_id;
        let state = element_idl_state(&ctx, node_id)?;
        state.set(IDL_CHECKED, value.0)?;
        state.set(IDL_CHECKED_DIRTY, true)?;
        Ok(())
    }

    /// `element.defaultChecked` — reflects `hasAttribute('checked')`.
    /// Pinned to the parsed HTML even after the IDL setter has
    /// diverged.
    #[qjs(get, rename = "defaultChecked")]
    fn default_checked(&self) -> bool {
        self.node_ref()
            .map(|n| n.has_attr("checked"))
            .unwrap_or(false)
    }

    /// `element.defaultChecked = bool` — reflects writing the
    /// `checked` content attribute. `true` → `setAttribute('checked',
    /// '')`; `false` → `removeAttribute('checked')`. Per spec the
    /// attribute's *presence* (regardless of value) means checked.
    #[qjs(set, rename = "defaultChecked")]
    fn set_default_checked(&self, value: rquickjs::Coerced<bool>) {
        if let Some(n) = self.node_ref() {
            if value.0 {
                n.set_attr("checked", "");
            } else {
                n.remove_attr("checked");
            }
        }
    }

    /// `element.disabled` — IDL boolean *reflected* attribute. The
    /// HTML spec says the IDL property is true iff the content
    /// attribute is present, regardless of the attribute's value. No
    /// IDL/content split here (unlike `.value` / `.checked`), so the
    /// getter just probes `hasAttribute`.
    #[qjs(get)]
    fn disabled(&self) -> bool {
        self.node_ref()
            .map(|n| n.has_attr("disabled"))
            .unwrap_or(false)
    }

    /// `element.disabled = bool` — toggle the `disabled` content
    /// attribute. `true` → `setAttribute('disabled', '')`; `false` →
    /// `removeAttribute('disabled')`.
    #[qjs(set, rename = "disabled")]
    fn set_disabled(&self, value: rquickjs::Coerced<bool>) {
        if let Some(n) = self.node_ref() {
            if value.0 {
                n.set_attr("disabled", "");
            } else {
                n.remove_attr("disabled");
            }
        }
    }

    /// `element.readOnly` — IDL boolean reflected attribute for
    /// `readonly`. JavaScript name is `readOnly` (camelCase); HTML
    /// attribute is `readonly`.
    #[qjs(get, rename = "readOnly")]
    fn read_only(&self) -> bool {
        self.node_ref()
            .map(|n| n.has_attr("readonly"))
            .unwrap_or(false)
    }

    /// `element.readOnly = bool` — toggle the `readonly` content
    /// attribute.
    #[qjs(set, rename = "readOnly")]
    fn set_read_only(&self, value: rquickjs::Coerced<bool>) {
        if let Some(n) = self.node_ref() {
            if value.0 {
                n.set_attr("readonly", "");
            } else {
                n.remove_attr("readonly");
            }
        }
    }

    /// `element.required` — IDL boolean reflected attribute.
    #[qjs(get)]
    fn required(&self) -> bool {
        self.node_ref()
            .map(|n| n.has_attr("required"))
            .unwrap_or(false)
    }

    /// `element.required = bool` — toggle the `required` content
    /// attribute.
    #[qjs(set, rename = "required")]
    fn set_required(&self, value: rquickjs::Coerced<bool>) {
        if let Some(n) = self.node_ref() {
            if value.0 {
                n.set_attr("required", "");
            } else {
                n.remove_attr("required");
            }
        }
    }

    /// `element.type` — IDL string reflected attribute. Per spec the
    /// default value is `"text"` when the `type` attribute is
    /// missing on an `<input>`; non-input elements (button, link)
    /// have their own defaults, but every framework boots on
    /// `<input>` first, so the simple text default covers the
    /// failure mode this fixes.
    #[qjs(get, rename = "type")]
    fn input_type(&self) -> String {
        self.node_ref()
            .and_then(|n| n.attr("type"))
            .map(|t| t.to_string())
            .unwrap_or_else(|| "text".to_owned())
    }

    /// `element.type = "..."` — write the `type` content attribute.
    #[qjs(set, rename = "type")]
    fn set_input_type(&self, value: rquickjs::Coerced<String>) {
        if let Some(n) = self.node_ref() {
            n.set_attr("type", &value.0);
        }
    }

    /// `element.name` — IDL string reflected attribute. Empty string
    /// when absent.
    #[qjs(get)]
    fn name(&self) -> String {
        self.node_ref()
            .and_then(|n| n.attr("name"))
            .map(|t| t.to_string())
            .unwrap_or_default()
    }

    /// `element.name = "..."` — write the `name` content attribute.
    #[qjs(set, rename = "name")]
    fn set_name(&self, value: rquickjs::Coerced<String>) {
        if let Some(n) = self.node_ref() {
            n.set_attr("name", &value.0);
        }
    }

    /// `element.placeholder` — IDL string reflected attribute. Empty
    /// string when absent.
    #[qjs(get)]
    fn placeholder(&self) -> String {
        self.node_ref()
            .and_then(|n| n.attr("placeholder"))
            .map(|t| t.to_string())
            .unwrap_or_default()
    }

    /// `element.placeholder = "..."` — write the `placeholder`
    /// content attribute.
    #[qjs(set, rename = "placeholder")]
    fn set_placeholder(&self, value: rquickjs::Coerced<String>) {
        if let Some(n) = self.node_ref() {
            n.set_attr("placeholder", &value.0);
        }
    }

    // ===== HTMLHyperlinkElementUtils mixin (`<a>` + `<area>`) ================
    //
    // WHATWG HTML §4.6.6 — `href`, `protocol`, `host`, `hostname`,
    // `port`, `pathname`, `search`, `hash`, `origin`, `username`,
    // `password`. The getter on each property re-resolves the
    // element's `href` content attribute against `globalThis.location`
    // on every read (see [`resolve_anchor_url`]) so navigation via
    // `history.pushState` / `set_base_url` is reflected without an
    // explicit invalidation step.
    //
    // The mixin only applies to `<a>` and `<area>` per the spec; on
    // any other tag every property returns `""`. This matches the
    // existing per-tag-specific IDL gating we already do for
    // `<input>` (`value`, `checked`) — Element is one shared Rust
    // type and the tag check sorts out which behaviors apply.
    //
    // Bug-of-record: prior to this getter, `anchor.href` returned
    // `undefined`, forcing every Playwright migration to fall back to
    // `anchor.getAttribute('href')` (which, unlike `.href`, does NOT
    // resolve relative URLs). See `AGENT_FINDINGS.md` (commit
    // `2cebf12`) for the original bug report.
    //
    // Spec: <https://html.spec.whatwg.org/multipage/links.html#htmlhyperlinkelementutils>.

    /// `anchor.href` IDL getter per WHATWG HTML §4.6.6.
    ///
    /// Algorithm:
    /// 1. If this is not an `<a>` or `<area>`, return `""`.
    /// 2. If there's no `href` content attribute, return `""`.
    /// 3. Otherwise, "reinitialize url": parse the attribute against
    ///    `globalThis.location.href`. Return the serialized absolute
    ///    URL.
    /// 4. If parsing fails, fall back to the raw attribute value (the
    ///    spec's behavior when the URL record is unset).
    ///
    /// **The reason this method exists:** the agent-driven HN
    /// extraction test (May 2026) discovered `a.href` returned a
    /// falsy value where it should have returned the resolved URL,
    /// breaking every Playwright snippet that assumes `a.href` is the
    /// canonical absolute string. `getAttribute('href')` was the
    /// workaround; the real fix is here.
    #[qjs(get)]
    fn href<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        let (doc, node_id) = {
            let borrowed = this.0.borrow();
            (borrowed.doc.clone(), borrowed.node_id)
        };
        let Some(node) = doc.tree.get(&node_id) else {
            return Ok(String::new());
        };
        if !node
            .node_name()
            .map(|n| is_hyperlink_tag(n.as_ref()))
            .unwrap_or(false)
        {
            return Ok(String::new());
        }
        match resolve_anchor_url(&ctx, &node)? {
            Ok(Some(u)) => Ok(u.as_str().to_owned()),
            // Spec: when the URL record fails to parse, the IDL
            // getter returns the raw attribute value as-is.
            Err(raw) => Ok(raw),
            // No `href` content attribute — empty string per spec.
            Ok(None) => Ok(String::new()),
        }
    }

    /// `anchor.href = "…"` IDL setter — writes the `href` content
    /// attribute verbatim. Per spec the setter is "set the `href`
    /// content attribute to the given value"; URL re-parse happens
    /// lazily on the next getter call.
    #[qjs(set, rename = "href")]
    fn set_href(&self, value: rquickjs::Coerced<String>) {
        if let Some(n) = self.node_ref() {
            n.set_attr("href", &value.0);
        }
    }

    /// `anchor.protocol` — `scheme + ":"` of the resolved URL.
    /// Returns `""` on non-hyperlink tags, missing href, or parse
    /// failure.
    #[qjs(get)]
    fn protocol<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        match anchor_url(&this, &ctx)? {
            Some(u) => Ok(format!("{}:", u.scheme())),
            None => Ok(String::new()),
        }
    }

    /// `anchor.protocol = "https"` setter. Tolerates a trailing `":"`.
    /// Silently no-ops on illegal transitions (e.g. `http` → `mailto`)
    /// per the WHATWG "any setter that would produce an invalid URL
    /// leaves the URL unchanged" rule.
    ///
    /// Mutations write back into the `href` content attribute as the
    /// serialized absolute URL — that's the storage canonical per
    /// spec.
    #[qjs(set, rename = "protocol")]
    fn set_protocol<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        if let Some(mut u) = anchor_url(&this, &ctx)? {
            let trimmed = value.0.trim_end_matches(':');
            if u.set_scheme(trimmed).is_ok() {
                write_anchor_href(&this, &u);
            }
        }
        Ok(())
    }

    /// `anchor.host` — `hostname[:port]` of the resolved URL.
    #[qjs(get)]
    fn host<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        let Some(u) = anchor_url(&this, &ctx)? else {
            return Ok(String::new());
        };
        Ok(match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_owned(),
            _ => String::new(),
        })
    }

    /// `anchor.host = "example.com:8080"` setter.
    #[qjs(set, rename = "host")]
    fn set_host<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        if let Some(mut u) = anchor_url(&this, &ctx)? {
            if u.set_host(Some(&value.0)).is_ok() {
                write_anchor_href(&this, &u);
            }
        }
        Ok(())
    }

    /// `anchor.hostname` — host without port.
    #[qjs(get)]
    fn hostname<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        match anchor_url(&this, &ctx)? {
            Some(u) => Ok(u.host_str().unwrap_or("").to_owned()),
            None => Ok(String::new()),
        }
    }

    /// `anchor.hostname = "…"` setter.
    #[qjs(set, rename = "hostname")]
    fn set_hostname<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        if let Some(mut u) = anchor_url(&this, &ctx)? {
            if u.set_host(Some(&value.0)).is_ok() {
                write_anchor_href(&this, &u);
            }
        }
        Ok(())
    }

    /// `anchor.port` — empty string when no port is set, otherwise the
    /// port as a decimal string.
    #[qjs(get)]
    fn port<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        match anchor_url(&this, &ctx)? {
            Some(u) => Ok(u.port().map(|p| p.to_string()).unwrap_or_default()),
            None => Ok(String::new()),
        }
    }

    /// `anchor.port = "8080"` setter. Empty string clears the port.
    #[qjs(set, rename = "port")]
    fn set_port<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        if let Some(mut u) = anchor_url(&this, &ctx)? {
            let port = if value.0.is_empty() {
                None
            } else {
                value.0.parse::<u16>().ok()
            };
            if u.set_port(port).is_ok() {
                write_anchor_href(&this, &u);
            }
        }
        Ok(())
    }

    /// `anchor.pathname` — path portion of the resolved URL, starting
    /// with `/` for hierarchical URLs. Empty string on parse failure.
    #[qjs(get)]
    fn pathname<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        match anchor_url(&this, &ctx)? {
            Some(u) => Ok(u.path().to_owned()),
            None => Ok(String::new()),
        }
    }

    /// `anchor.pathname = "/foo"` setter.
    #[qjs(set, rename = "pathname")]
    fn set_pathname<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        if let Some(mut u) = anchor_url(&this, &ctx)? {
            u.set_path(&value.0);
            write_anchor_href(&this, &u);
        }
        Ok(())
    }

    /// `anchor.search` — query portion with leading `?`. Empty when no
    /// query.
    #[qjs(get)]
    fn search<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        match anchor_url(&this, &ctx)? {
            Some(u) => Ok(u.query().map(|q| format!("?{q}")).unwrap_or_default()),
            None => Ok(String::new()),
        }
    }

    /// `anchor.search = "?a=1"` setter. Tolerates a leading `?`.
    #[qjs(set, rename = "search")]
    fn set_search<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        if let Some(mut u) = anchor_url(&this, &ctx)? {
            let v = value.0.strip_prefix('?').unwrap_or(&value.0);
            if v.is_empty() {
                u.set_query(None);
            } else {
                u.set_query(Some(v));
            }
            write_anchor_href(&this, &u);
        }
        Ok(())
    }

    /// `anchor.hash` — fragment portion with leading `#`. Empty when
    /// no fragment.
    #[qjs(get)]
    fn hash<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        match anchor_url(&this, &ctx)? {
            Some(u) => Ok(u.fragment().map(|f| format!("#{f}")).unwrap_or_default()),
            None => Ok(String::new()),
        }
    }

    /// `anchor.hash = "#frag"` setter. Tolerates a leading `#`.
    #[qjs(set, rename = "hash")]
    fn set_hash<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        if let Some(mut u) = anchor_url(&this, &ctx)? {
            let v = value.0.strip_prefix('#').unwrap_or(&value.0);
            if v.is_empty() {
                u.set_fragment(None);
            } else {
                u.set_fragment(Some(v));
            }
            write_anchor_href(&this, &u);
        }
        Ok(())
    }

    /// `anchor.origin` — `scheme://host[:port]` for hierarchical
    /// schemes, `"null"` otherwise. Read-only per spec.
    #[qjs(get)]
    fn origin<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        match anchor_url(&this, &ctx)? {
            Some(u) => Ok(u.origin().ascii_serialization()),
            None => Ok(String::new()),
        }
    }

    /// `anchor.username` — percent-encoded username, or empty.
    #[qjs(get)]
    fn username<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        match anchor_url(&this, &ctx)? {
            Some(u) => Ok(u.username().to_owned()),
            None => Ok(String::new()),
        }
    }

    /// `anchor.username = "…"` setter.
    #[qjs(set, rename = "username")]
    fn set_username<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        if let Some(mut u) = anchor_url(&this, &ctx)? {
            if u.set_username(&value.0).is_ok() {
                write_anchor_href(&this, &u);
            }
        }
        Ok(())
    }

    /// `anchor.password` — percent-encoded password, or empty.
    #[qjs(get)]
    fn password<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        match anchor_url(&this, &ctx)? {
            Some(u) => Ok(u.password().unwrap_or("").to_owned()),
            None => Ok(String::new()),
        }
    }

    /// `anchor.password = "…"` setter.
    #[qjs(set, rename = "password")]
    fn set_password<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        value: rquickjs::Coerced<String>,
    ) -> rquickjs::Result<()> {
        if let Some(mut u) = anchor_url(&this, &ctx)? {
            let pw = if value.0.is_empty() {
                None
            } else {
                Some(value.0.as_str())
            };
            if u.set_password(pw).is_ok() {
                write_anchor_href(&this, &u);
            }
        }
        Ok(())
    }

    // ===== HTMLFormElement IDL (`<form>`) ====================================
    //
    // WHATWG HTML §4.10.3 — `action`, `method`, `enctype`, `encoding`
    // (alias for `enctype`), `target`, `acceptCharset`, `autocomplete`,
    // `noValidate`, `length`, `elements`, `submit()`, `reset()`.
    //
    // Gated by tag-name check on `<form>` per the same pattern as the
    // anchor mixin above and the `<input>`-specific IDL surface
    // (`value` / `checked`). Non-`<form>` tags return the spec's
    // "missing-value default" from getters and silent no-op from
    // methods.
    //
    // `form.name` and `form.placeholder` are intentionally *not* listed
    // here — the generic Element `.name` getter (further up) already
    // does attribute reflection that matches the form's `name` IDL.
    //
    // Bug-of-record: prior to this batch, `form.method` / `form.action`
    // / `form.enctype` all returned `undefined`, forcing scrapers to
    // use `form.getAttribute(...)` (which doesn't normalize per spec).
    // Sibling fix to the HTMLAnchorElement.href mixin landed in commit
    // `17ddf77`. See `AGENT_FINDINGS_V2.md` "Bonus findings" + "Top NEW
    // bugs" #3 for the original report.
    //
    // Spec: <https://html.spec.whatwg.org/multipage/forms.html#the-form-element>.

    /// `form.action` IDL getter per WHATWG HTML §4.10.3.
    ///
    /// Algorithm:
    /// 1. If this is not a `<form>`, return `""`.
    /// 2. Resolve the `action` content attribute against the document
    ///    base URL (`globalThis.location.href`).
    /// 3. When the attribute is missing/empty, the spec says use the
    ///    document URL itself (so the form posts back to the current
    ///    page).
    ///
    /// Unlike `<a>`/`<area>` URL decomposition, `<form>` only exposes
    /// `.action` as a single string — not the full `protocol`/`host`/
    /// etc. mixin.
    #[qjs(get)]
    fn action<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<String> {
        let doc = this.0.borrow().doc.clone();
        let Some(node) = form_node_ref(&this, &doc) else {
            return Ok(String::new());
        };
        resolve_form_action(&ctx, &node)
    }

    /// `form.action = "..."` IDL setter — writes the `action` content
    /// attribute verbatim. Per spec the setter is "set the content
    /// attribute to the given value"; URL resolution happens lazily
    /// on the next getter call.
    #[qjs(set, rename = "action")]
    fn set_action<'js>(this: This<Class<'js, Self>>, value: rquickjs::Coerced<String>) {
        let doc = this.0.borrow().doc.clone();
        if let Some(node) = form_node_ref(&this, &doc) {
            node.set_attr("action", &value.0);
        }
    }

    /// `form.method` IDL getter per WHATWG HTML §4.10.3.
    ///
    /// Returns one of `"get"`, `"post"`, `"dialog"` (lowercase).
    /// Missing or invalid attribute → `"get"` (the spec's
    /// "missing value default" / "invalid value default").
    ///
    /// Note: this is intentionally NOT `getAttribute('method')` —
    /// the IDL getter normalizes per spec, while `getAttribute`
    /// returns the raw attribute text as authored.
    #[qjs(get)]
    fn method<'js>(this: This<Class<'js, Self>>) -> String {
        let doc = this.0.borrow().doc.clone();
        let Some(node) = form_node_ref(&this, &doc) else {
            return String::new();
        };
        let raw = node.attr("method").map(|s| s.to_string());
        normalize_form_method(raw.as_deref()).to_owned()
    }

    /// `form.method = "..."` IDL setter — writes the `method` content
    /// attribute verbatim. Per spec the normalization happens on read,
    /// not write, so `form.method = "POST"` stores `"POST"` literally
    /// and `getAttribute('method')` returns `"POST"`.
    #[qjs(set, rename = "method")]
    fn set_method<'js>(this: This<Class<'js, Self>>, value: rquickjs::Coerced<String>) {
        let doc = this.0.borrow().doc.clone();
        if let Some(node) = form_node_ref(&this, &doc) {
            node.set_attr("method", &value.0);
        }
    }

    /// `form.enctype` IDL getter per WHATWG HTML §4.10.3.
    ///
    /// Returns one of `"application/x-www-form-urlencoded"`,
    /// `"multipart/form-data"`, `"text/plain"`. Missing or invalid
    /// attribute → `"application/x-www-form-urlencoded"` (the spec's
    /// "missing value default").
    #[qjs(get)]
    fn enctype<'js>(this: This<Class<'js, Self>>) -> String {
        let doc = this.0.borrow().doc.clone();
        let Some(node) = form_node_ref(&this, &doc) else {
            return String::new();
        };
        let raw = node.attr("enctype").map(|s| s.to_string());
        normalize_form_enctype(raw.as_deref()).to_owned()
    }

    /// `form.enctype = "..."` IDL setter — writes the `enctype` content
    /// attribute verbatim. Normalization on read, not write.
    #[qjs(set, rename = "enctype")]
    fn set_enctype<'js>(this: This<Class<'js, Self>>, value: rquickjs::Coerced<String>) {
        let doc = this.0.borrow().doc.clone();
        if let Some(node) = form_node_ref(&this, &doc) {
            node.set_attr("enctype", &value.0);
        }
    }

    /// `form.encoding` — spec-defined alias for `form.enctype` (same
    /// getter, same setter, same defaults). Real pages do read this
    /// alias.
    ///
    /// Spec: "The encoding IDL attribute, on getting, must return the
    /// result of running the corresponding getter steps for the enctype
    /// IDL attribute."
    #[qjs(get)]
    fn encoding<'js>(this: This<Class<'js, Self>>) -> String {
        Self::enctype(this)
    }

    /// `form.encoding = "..."` — alias for `form.enctype = "..."`.
    #[qjs(set, rename = "encoding")]
    fn set_encoding<'js>(this: This<Class<'js, Self>>, value: rquickjs::Coerced<String>) {
        Self::set_enctype(this, value);
    }

    /// `form.target` — reflects the `target` content attribute
    /// (the browsing context name to navigate on submit, e.g. `_blank`).
    /// Empty string when missing. No normalization on read per spec.
    #[qjs(get)]
    fn target<'js>(this: This<Class<'js, Self>>) -> String {
        let doc = this.0.borrow().doc.clone();
        let Some(node) = form_node_ref(&this, &doc) else {
            return String::new();
        };
        node.attr("target").map(|s| s.to_string()).unwrap_or_default()
    }

    /// `form.target = "..."` — write the `target` content attribute.
    #[qjs(set, rename = "target")]
    fn set_target<'js>(this: This<Class<'js, Self>>, value: rquickjs::Coerced<String>) {
        let doc = this.0.borrow().doc.clone();
        if let Some(node) = form_node_ref(&this, &doc) {
            node.set_attr("target", &value.0);
        }
    }

    /// `form.acceptCharset` — reflects the `accept-charset` content
    /// attribute. JS name camel-cases the kebab. Empty string when
    /// missing.
    #[qjs(get, rename = "acceptCharset")]
    fn accept_charset<'js>(this: This<Class<'js, Self>>) -> String {
        let doc = this.0.borrow().doc.clone();
        let Some(node) = form_node_ref(&this, &doc) else {
            return String::new();
        };
        node.attr("accept-charset")
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    /// `form.acceptCharset = "..."` — write the `accept-charset`
    /// content attribute.
    #[qjs(set, rename = "acceptCharset")]
    fn set_accept_charset<'js>(this: This<Class<'js, Self>>, value: rquickjs::Coerced<String>) {
        let doc = this.0.borrow().doc.clone();
        if let Some(node) = form_node_ref(&this, &doc) {
            node.set_attr("accept-charset", &value.0);
        }
    }

    /// `form.autocomplete` — reflects the `autocomplete` content
    /// attribute. Default per spec is `"on"`, but the getter returns
    /// the raw attribute when present (only the missing-value default
    /// is `"on"`). We return the raw attribute when set and `"on"`
    /// when missing — that matches the most common framework
    /// expectation.
    #[qjs(get)]
    fn autocomplete<'js>(this: This<Class<'js, Self>>) -> String {
        let doc = this.0.borrow().doc.clone();
        let Some(node) = form_node_ref(&this, &doc) else {
            return String::new();
        };
        node.attr("autocomplete")
            .map(|s| s.to_string())
            .unwrap_or_else(|| "on".to_owned())
    }

    /// `form.autocomplete = "..."` — write the `autocomplete`
    /// content attribute.
    #[qjs(set, rename = "autocomplete")]
    fn set_autocomplete<'js>(this: This<Class<'js, Self>>, value: rquickjs::Coerced<String>) {
        let doc = this.0.borrow().doc.clone();
        if let Some(node) = form_node_ref(&this, &doc) {
            node.set_attr("autocomplete", &value.0);
        }
    }

    /// `form.noValidate` — boolean IDL reflection of the `novalidate`
    /// content attribute. `true` iff the attribute is present.
    #[qjs(get, rename = "noValidate")]
    fn no_validate<'js>(this: This<Class<'js, Self>>) -> bool {
        let doc = this.0.borrow().doc.clone();
        let Some(node) = form_node_ref(&this, &doc) else {
            return false;
        };
        node.has_attr("novalidate")
    }

    /// `form.noValidate = bool` — toggle the `novalidate` content
    /// attribute. `true` → `setAttribute('novalidate', '')`;
    /// `false` → `removeAttribute('novalidate')`.
    #[qjs(set, rename = "noValidate")]
    fn set_no_validate<'js>(this: This<Class<'js, Self>>, value: rquickjs::Coerced<bool>) {
        let doc = this.0.borrow().doc.clone();
        if let Some(node) = form_node_ref(&this, &doc) {
            if value.0 {
                node.set_attr("novalidate", "");
            } else {
                node.remove_attr("novalidate");
            }
        }
    }

    /// `form.length` — number of listed form controls (`button`,
    /// `fieldset`, `input`, `object`, `output`, `select`, `textarea`)
    /// that are descendants of this form. Non-`<form>` tags return `0`.
    ///
    /// Spec: <https://html.spec.whatwg.org/multipage/forms.html#dom-form-length>.
    #[qjs(get)]
    fn length<'js>(this: This<Class<'js, Self>>) -> u32 {
        let (doc, node_id) = {
            let borrowed = this.0.borrow();
            (borrowed.doc.clone(), borrowed.node_id)
        };
        let Some(node) = doc.tree.get(&node_id) else {
            return 0;
        };
        let is_form = node
            .node_name()
            .map(|n| is_form_tag(n.as_ref()))
            .unwrap_or(false);
        if !is_form {
            return 0;
        }
        collect_form_listed_controls(&doc, node_id).len() as u32
    }

    /// `form.elements` — array of listed form controls (`button`,
    /// `fieldset`, `input`, `object`, `output`, `select`, `textarea`),
    /// in document order.
    ///
    /// Per spec this returns a live `HTMLFormControlsCollection`. We
    /// return a snapshot `Vec<Element>` for the same reason
    /// `document.getElementsByTagName` does — most callers iterate
    /// immediately, indexed access works (`form.elements[0]`,
    /// `form.elements.length`), and the engine has no observer model
    /// to invalidate a live collection on mutation. Real pages
    /// rarely depend on liveness; if a page does, a re-read of
    /// `form.elements` produces an up-to-date snapshot anyway.
    ///
    /// Spec: <https://html.spec.whatwg.org/multipage/forms.html#dom-form-elements>.
    #[qjs(get)]
    fn elements<'js>(this: This<Class<'js, Self>>) -> Vec<Element> {
        let (doc, node_id) = {
            let borrowed = this.0.borrow();
            (borrowed.doc.clone(), borrowed.node_id)
        };
        let Some(node) = doc.tree.get(&node_id) else {
            return Vec::new();
        };
        let is_form = node
            .node_name()
            .map(|n| is_form_tag(n.as_ref()))
            .unwrap_or(false);
        if !is_form {
            return Vec::new();
        }
        collect_form_listed_controls(&doc, node_id)
            .into_iter()
            .map(|id| Element::from_id(doc.clone(), id))
            .collect()
    }

    /// `form.submit()` — programmatically submit the form WITHOUT
    /// firing the `submit` event, per WHATWG HTML §4.10.3 and the
    /// jsdom WPT (`HTMLFormElement's submit() does not fire a
    /// SubmitEvent`).
    ///
    /// Implementation: walks the form to build the entry list,
    /// resolves the action URL against the document base, and routes
    /// to a globalThis-installed `__hesoFormSubmitNow` helper that
    /// synchronously issues the HTTP request via the engine's
    /// shared `reqwest::Client`. Returns nothing (per spec it's
    /// void).
    ///
    /// **Differences from real browsers / the verb path:**
    /// - Real browsers navigate the top-level browsing context to
    ///   the response URL. heso has no top-level context here (the
    ///   call site is inside `eval-dom` / `eval-js`, not a session
    ///   step), so we issue the HTTP request but DO NOT replace
    ///   the document. The session-level `JsSession::submit` path
    ///   (which fires the submit event AND navigates) is the
    ///   end-to-end equivalent.
    /// - Silent no-op when the engine was built without a fetch
    ///   client (`JsEngine::new()` rather than
    ///   `JsEngine::new_with_fetch`) — matches the spec's "no
    ///   browsing context" branch.
    ///
    /// Bug-of-record: V2 agent findings reported `form.submit()`
    /// throws `TypeError: not a function`. After this PR it
    /// dispatches a real HTTP request.
    fn submit<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<()> {
        let (doc, node_id) = {
            let borrowed = this.0.borrow();
            (borrowed.doc.clone(), borrowed.node_id)
        };
        let Some(node) = doc.tree.get(&node_id) else {
            return Ok(());
        };
        if !node
            .node_name()
            .map(|n| is_form_tag(n.as_ref()))
            .unwrap_or(false)
        {
            return Ok(());
        }
        // Route to the JS-installed `__hesoFormSubmitNow(form)` helper
        // which has captured a clone of the engine's `reqwest::Client`.
        // Installed only when fetch state exists; absent in
        // `JsEngine::new()` engines, in which case the call is a
        // silent no-op.
        let globals = ctx.globals();
        let Ok(submit_now) = globals.get::<_, Function<'js>>("__hesoFormSubmitNow") else {
            return Ok(());
        };
        let form_value: Value<'js> = this.0.clone().into_value();
        let _ = submit_now.call::<_, Value<'js>>((form_value,))?;
        Ok(())
    }

    /// `form.reset()` — reset every control in the form to its
    /// default value per WHATWG HTML §4.10.3.
    ///
    /// Implementation: walks the form's listed controls and clears
    /// the IDL value / checked dirty bits (so the next read of
    /// `input.value` falls back to the `value` content attribute,
    /// matching the spec). After resetting, fires a non-cancelable
    /// `reset` event on the form per spec.
    ///
    /// **What we DO reset:**
    /// - `<input>` IDL value (the dirty bit set by `input.value = ...`)
    /// - `<input>` checked state (the dirty bit set by
    ///   `input.checked = true/false`)
    /// - `<textarea>` IDL value (same mechanism)
    /// - `<select>` selected option (would clear; not stored as IDL
    ///   state in this engine, so no-op).
    ///
    /// **What we don't:**
    /// - File-input file lists (no file plumbing yet).
    /// - Custom-element form-associated reset callbacks (no custom
    ///   elements yet).
    fn reset<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<()> {
        let (doc, node_id) = {
            let borrowed = this.0.borrow();
            (borrowed.doc.clone(), borrowed.node_id)
        };
        let Some(node) = doc.tree.get(&node_id) else {
            return Ok(());
        };
        if !node
            .node_name()
            .map(|n| is_form_tag(n.as_ref()))
            .unwrap_or(false)
        {
            return Ok(());
        }
        // Walk listed controls and clear their per-node IDL state
        // (value dirty + checked dirty). The shared
        // `__nodeIdlState` registry lives on `document` — we just
        // delete the entry for each control's NodeId.
        let controls = collect_form_listed_controls(&doc, node_id);
        let document: Option<Object<'js>> =
            ctx.globals().get::<_, Option<Object<'js>>>("document")?;
        if let Some(document) = document {
            if let Some(registry) =
                document.get::<_, Option<Object<'js>>>(PROP_NODE_IDL_STATE)?
            {
                for control_id in controls {
                    let key = node_key(control_id);
                    let _ = registry.remove(key.as_str());
                }
            }
        }
        // Fire a non-cancelable `reset` event on the form, per spec.
        let event = events::Event::new_with_init(
            "reset".to_owned(),
            Some(events::EventInit {
                bubbles: true,
                cancelable: false,
                composed: false,
            }),
        );
        let event_class = Class::instance(ctx.clone(), event)?;
        let event_value: Value<'js> = event_class.into_value();
        let element = this.0.borrow().clone();
        let path = build_dispatch_path(&ctx, &element)?;
        let _ = dispatch_with_node_path(&ctx, &path, event_value)?;
        Ok(())
    }

    /// `element.addEventListener(type, listener, options?)` — register
    /// a JS callback for `type` events on this element. Mirrors
    /// [`crate::EventTarget`]; listener storage is JS-side on the
    /// element instance under the same hidden `__listeners` map shape,
    /// so dispatch logic (and the no-Persistent footgun avoidance)
    /// stays unified.
    fn add_event_listener<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        event_type: String,
        listener: Function<'js>,
        options: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        let (capture, once, passive) = parse_listener_options(&ctx, options.0)?;
        let node_id = this.0.borrow().node_id;
        let map = element_listener_map(&ctx, node_id)?;
        // Cache the JS-side Element wrapper that the caller used as
        // `this` for this addEventListener call. Framework code
        // (Preact in particular) mutates the wrapper directly
        // (`e.l = {...}`) and the dispatcher must use the same JS
        // object reference as `currentTarget` so those mutations
        // are visible inside event proxies. Without this, every
        // call to a query method synthesizes a fresh Element
        // wrapper around the same NodeId and the framework's
        // hidden state on the original wrapper is unreachable.
        //
        // First-wins: don't overwrite a previously-stored owner.
        // If two different JS-side query results both register
        // listeners on the same node, the first one becomes the
        // canonical dispatch wrapper.
        if map.get::<_, Option<Value<'js>>>(PROP_OWNER_WRAPPER)?.is_none() {
            let owner_value: Value<'js> = this.0.clone().into_value();
            map.set(PROP_OWNER_WRAPPER, owner_value)?;
        }
        add_listener_to_map(&ctx, &map, &event_type, &listener, capture, once, passive)
    }

    /// `element.removeEventListener(type, listener, options?)`.
    fn remove_event_listener<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        event_type: String,
        listener: Function<'js>,
        options: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        let (capture, _, _) = parse_listener_options(&ctx, options.0)?;
        let node_id = this.0.borrow().node_id;
        if let Some(map) = element_listener_map_opt(&ctx, node_id)? {
            remove_listener_from_map(&ctx, &map, &event_type, &listener, capture)?;
        }
        Ok(())
    }

    /// `element.dispatchEvent(event)` — fire `event` on this element
    /// using a W3C capture / at-target / bubble path walk. Returns
    /// `false` iff the event is cancelable and a listener called
    /// `preventDefault()`. See [`dispatch_with_node_path`].
    fn dispatch_event<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        event: Value<'js>,
    ) -> rquickjs::Result<bool> {
        let element = this.0.borrow().clone();
        let path = build_dispatch_path(&ctx, &element)?;
        dispatch_with_node_path(&ctx, &path, event)
    }

    /// `element.click()` — synthesize and dispatch a cancelable
    /// `"click"` event on this element. Equivalent to
    /// `element.dispatchEvent(new Event('click', { bubbles: true,
    /// cancelable: true }))`, which is what real browsers do for the
    /// HTMLElement.click() shortcut.
    ///
    /// Returns nothing — call sites that want to know whether
    /// `preventDefault()` was called should use `dispatchEvent`
    /// directly. (DOM spec says `.click()` is `void` too.)
    fn click<'js>(this: This<Class<'js, Self>>, ctx: Ctx<'js>) -> rquickjs::Result<()> {
        let event = events::Event::new_with_init(
            "click".to_owned(),
            Some(events::EventInit {
                bubbles: true,
                cancelable: true,
                composed: false,
            }),
        );
        let event_class = Class::instance(ctx.clone(), event)?;
        let event_value: Value<'js> = event_class.into_value();
        let element = this.0.borrow().clone();
        let path = build_dispatch_path(&ctx, &element)?;
        let _ = dispatch_with_node_path(&ctx, &path, event_value)?;
        Ok(())
    }

    // ===== Trivial browser-globals batch (layout-zero stubs) =================
    //
    // ADR 0016 says heso has no layout/paint. But frameworks like
    // Floating UI, Popper, Headless UI, Tippy, and React Aria call
    // these layout-reading methods unconditionally during init.
    // Throwing "X is not a function" halts hydration on otherwise-
    // clean pages. We return zero-valued shapes so the call succeeds
    // and the framework picks a sensible fallback (typically "render
    // at 0,0 until a real position is computed").

    /// `element.getBoundingClientRect()` — return a zero `DOMRect`.
    ///
    /// Real browsers return `{ x, y, width, height, top, right,
    /// bottom, left }` where all eight fields are layout-derived.
    /// heso has no layout, so every field is `0`. The returned object
    /// is a plain JS POJO (not a `DOMRect` class instance) because
    /// frameworks read the fields, never check the type. The
    /// `toJSON()` method exists because some serialization paths
    /// reach for it (`JSON.stringify(rect)` calls it).
    fn get_bounding_client_rect<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        ctx.eval::<Value<'js>, _>(
            r#"({
                x: 0, y: 0,
                width: 0, height: 0,
                top: 0, right: 0, bottom: 0, left: 0,
                toJSON: function() { return this; }
            })"#,
        )
    }

    /// `element.getClientRects()` — return an empty array (real
    /// browsers return a `DOMRectList`; an empty plain array is
    /// indistinguishable for the iteration-only patterns frameworks
    /// use). Floating UI calls this and falls back to
    /// `getBoundingClientRect()` when the list is empty.
    fn get_client_rects<'js>(&self, ctx: Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        ctx.eval::<Value<'js>, _>("[]")
    }

    /// `element.clientWidth` — always `0` (no layout).
    #[qjs(get)]
    fn client_width(&self) -> u32 {
        0
    }

    /// `element.clientHeight` — always `0` (no layout).
    #[qjs(get)]
    fn client_height(&self) -> u32 {
        0
    }

    /// `element.offsetWidth` — always `0` (no layout).
    #[qjs(get)]
    fn offset_width(&self) -> u32 {
        0
    }

    /// `element.offsetHeight` — always `0` (no layout).
    #[qjs(get)]
    fn offset_height(&self) -> u32 {
        0
    }

    /// `element.offsetTop` — always `0` (no layout).
    #[qjs(get)]
    fn offset_top(&self) -> u32 {
        0
    }

    /// `element.offsetLeft` — always `0` (no layout).
    #[qjs(get)]
    fn offset_left(&self) -> u32 {
        0
    }

    /// `element.offsetParent` — always `null` (no layout / no
    /// positioned-ancestor concept). Tippy / Popper read this to
    /// pick a positioning context; `null` means "use the viewport",
    /// which is the safe fallback when we have nothing better.
    #[qjs(get)]
    fn offset_parent(&self) -> Option<Element> {
        None
    }

    /// `element.scrollWidth` — always `0`.
    #[qjs(get)]
    fn scroll_width(&self) -> u32 {
        0
    }

    /// `element.scrollHeight` — always `0`.
    #[qjs(get)]
    fn scroll_height(&self) -> u32 {
        0
    }

    /// `element.scrollTop` — always `0`. Setter is a no-op (see below).
    #[qjs(get)]
    fn scroll_top(&self) -> u32 {
        0
    }

    /// `element.scrollTop = value` — silent no-op. Real browsers
    /// scroll the element; heso has nothing to scroll. Setter exists
    /// so `el.scrollTop = 100` doesn't throw on a read-only property.
    #[qjs(set, rename = "scrollTop")]
    fn set_scroll_top(&self, _value: f64) {
        // intentional no-op — no layout.
    }

    /// `element.scrollLeft` — always `0`.
    #[qjs(get)]
    fn scroll_left(&self) -> u32 {
        0
    }

    /// `element.scrollLeft = value` — silent no-op.
    #[qjs(set, rename = "scrollLeft")]
    fn set_scroll_left(&self, _value: f64) {
        // intentional no-op — no layout.
    }

    /// `element.focus(options?)` — no-op. heso has no focus tracker
    /// (yet). Real browsers move keyboard focus to the element and
    /// dispatch `focusin` / `focus` events; a follow-up agent will
    /// wire that path. For now: don't throw, don't do anything.
    fn focus(&self, _options: Opt<Value<'_>>) {
        // intentional no-op — focus model is a future item.
    }

    /// `element.blur()` — no-op. Same reasoning as [`Self::focus`].
    fn blur(&self) {
        // intentional no-op — focus model is a future item.
    }

    /// `element.scrollIntoView(opts?)` — no-op. Spec arg shape: a
    /// boolean or an options object; we accept either as an opaque
    /// `Value` and discard it so the caller doesn't crash.
    fn scroll_into_view(&self, _arg: Opt<Value<'_>>) {
        // intentional no-op — no layout.
    }
}

/// Recursively clone the subtree rooted at `source_id` into the
/// same `dom_query::Tree`. Returns the [`NodeId`] of the new
/// orphan root.
///
/// Used by [`Element::clone_node`]. The walk:
///
/// 1. Look up the source node's [`dom_query::NodeData`]. For
///    elements, create a fresh element with the same tag via
///    [`dom_query::Tree::new_element`] and copy every attribute
///    (the `attrs()` snapshot is taken once, so subsequent
///    mutations to the source's attributes do not bleed into the
///    clone). For text nodes, create a fresh text node with the
///    same data via [`dom_query::Tree::new_text`]. For comment /
///    doctype / processing-instruction / document / fragment
///    nodes, fall back to creating a placeholder text node with
///    an empty string — dom_query 0.28 has no public
///    constructor for those types on [`dom_query::Tree`], and
///    they don't appear in SSR output that matters for
///    hydration.
/// 2. If `deep` is `true`, walk `children_it(false)` of the
///    source and recursively clone each child, then `append_child`
///    the new child into the new parent. Depth-first pre-order,
///    matching `Node.cloneNode(true)` per the DOM spec.
/// 3. Otherwise, leave the new node childless.
///
/// Listeners are *not* copied — the DOM spec is explicit that
/// `addEventListener`-registered listeners do not clone. Inline
/// handlers (`onclick="..."`) are preserved because they live in
/// the attribute store, which step 1 copies.
fn clone_subtree(doc: &Arc<DqDocument>, source_id: NodeId, deep: bool) -> NodeId {
    let tree = &doc.tree;
    // Build the new orphan node first (mirroring source's kind and
    // immediate data); release the source borrow before any
    // recursion so the inner allocations don't deadlock on the
    // RefCell.
    let new_id = {
        let Some(source) = tree.get(&source_id) else {
            // Stale source NodeId — can't read what to clone.
            // Fall back to an empty text node so the JS contract
            // (cloneNode always returns a node) is preserved.
            return tree.new_text(String::new()).id;
        };
        if source.is_element() {
            // Element clone: copy tag name + every attribute.
            let tag = source
                .node_name()
                .map(|t| t.to_string())
                .unwrap_or_else(|| "div".to_owned());
            let new_node = tree.new_element(&tag);
            for attr in source.attrs() {
                // `attr.name.local` is the bare (non-namespaced)
                // attribute name — matches what
                // `dom_query::NodeRef::set_attr` writes, so the
                // clone's `getAttribute(name)` reads back the
                // same value as the source's would.
                new_node.set_attr(&attr.name.local, &attr.value);
            }
            new_node.id
        } else if source.is_text() {
            // Text node: replicate the contents. `text()` on a
            // pure text node yields exactly that node's data
            // (no recursion needed — text nodes have no children).
            let data = source.text().to_string();
            tree.new_text(data).id
        } else {
            // Comment / doctype / processing-instruction /
            // document / fragment. dom_query 0.28 has no public
            // `Tree::new_comment` etc., so we fall back to an
            // empty text-node placeholder. None of these appear
            // in SSR output that matters for hydration
            // (`<!-- -->` rarely survives a render diff intact).
            tree.new_text(String::new()).id
        }
    };

    if deep {
        // Snapshot child ids first so we don't hold the source
        // node's tree borrow across the recursive call (the
        // recursion mutates `tree.nodes` via `new_element` /
        // `new_text`, which would re-borrow).
        let child_ids: Vec<NodeId> = match tree.get(&source_id) {
            Some(n) => n.children_it(false).map(|c| c.id).collect(),
            None => Vec::new(),
        };
        for child_id in child_ids {
            let cloned_child_id = clone_subtree(doc, child_id, true);
            if let Some(parent) = tree.get(&new_id) {
                parent.append_child(&cloned_child_id);
            }
        }
    }

    new_id
}

/// Build the W3C event-dispatch path for `target` — `[root, ...,
/// target]`. Each entry pairs the node's listener map (looked up
/// read-only on the long-lived `__nodeListeners` registry; `None` if
/// no listeners were ever registered) with a freshly-instantiated JS
/// [`Element`] wrapper to populate `event.currentTarget` while that
/// node's listeners fire.
///
/// The walk follows [`Element::parent_element`] semantics: skip non-
/// element parents (text/comment nodes are not in the dispatch path
/// per the DOM spec). Termination of the element walk is the first
/// node with no element parent (i.e. the document element or an
/// orphan node still being constructed by `createElement`).
///
/// The [`Document`] is prepended at index 0 so that document-level
/// listeners fire **first** in the capture phase and **last** in
/// the bubble phase. React 19's synthetic-event system (and a great
/// deal of non-React inline JS) attaches its single global click
/// handler with `document.addEventListener`; without the document
/// in the path, none of those handlers would ever observe element-
/// rooted dispatches.
///
/// `event.target` is set by [`dispatch_with_node_path`] from the
/// last entry of the path, so prepending the document keeps the
/// target the element — which is what the spec requires.
fn build_dispatch_path<'js>(
    ctx: &Ctx<'js>,
    target: &Element,
) -> rquickjs::Result<Vec<(Option<Object<'js>>, Value<'js>)>> {
    // Collect node ids from target → root.
    let mut ids: Vec<NodeId> = Vec::new();
    ids.push(target.node_id);
    if let Some(start) = target.node_ref() {
        let mut cur = start.parent();
        while let Some(n) = cur {
            if n.is_element() {
                ids.push(n.id);
            }
            cur = n.parent();
        }
    }
    // Reverse so root is first, target last (matches
    // `dispatch_with_node_path`'s expected ordering).
    ids.reverse();

    let mut path: Vec<(Option<Object<'js>>, Value<'js>)> =
        Vec::with_capacity(ids.len() + 1);
    // Document sits at the root of the path. `fire_listeners_on_node`
    // skips when the map is `None`, so a session that has never
    // attached a document-level listener pays only one lookup.
    let doc_map = document_listener_map_opt(ctx)?;
    let doc_value: Value<'js> = ctx.globals().get("document")?;
    path.push((doc_map, doc_value));
    for id in ids {
        let map = element_listener_map_opt(ctx, id)?;
        // Prefer the cached `__owner` wrapper if `addEventListener`
        // has been called on this node. Frameworks (Preact) mutate
        // the JS wrapper directly between addEventListener and the
        // first dispatch (e.g. `el.l = {keydownfalse: handler}`),
        // and the registered proxy reads `this.l` — so dispatch
        // must use the same JS object reference, not a fresh one.
        let wrapper_value: Value<'js> = match map
            .as_ref()
            .and_then(|m| m.get::<_, Option<Value<'js>>>(PROP_OWNER_WRAPPER).ok().flatten())
        {
            Some(v) => v,
            None => Class::instance(ctx.clone(), Element::from_id(target.doc.clone(), id))?
                .into_value(),
        };
        path.push((map, wrapper_value));
    }
    Ok(path)
}

/// `element.classList` — a [DOMTokenList][spec] over the element's
/// space-separated `class` attribute.
///
/// Each method reads + rewrites the `class` attribute, so the list is
/// "live" by construction: there is no cached state to invalidate.
///
/// [spec]: https://dom.spec.whatwg.org/#interface-domtokenlist
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class]
pub struct DomTokenList {
    /// The element whose `class` attribute this token list reads/
    /// writes. Stored as an [`Element`] (which is itself just two
    /// `Copy`-cheap fields), so it survives JS call boundaries.
    element: Element,
}

impl DomTokenList {
    fn new(element: Element) -> Self {
        Self { element }
    }

    /// Read the `class` attribute as a Vec of tokens, splitting on
    /// ASCII whitespace and discarding empties. The DOM spec's "ordered
    /// set parser" is more elaborate; this matches the common case.
    fn tokens(&self) -> Vec<String> {
        self.element
            .node_ref()
            .and_then(|n| n.attr("class"))
            .map(|s| {
                s.split_ascii_whitespace()
                    .map(|t| t.to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    /// Write `tokens` back to the `class` attribute, joining on a
    /// single space. If `tokens` is empty, the attribute is removed
    /// (matches happy-dom and jsdom; the spec's "if empty: remove" is
    /// implicit for the serializer too).
    fn write(&self, tokens: &[String]) {
        let n = match self.element.node_ref() {
            Some(n) => n,
            None => return,
        };
        if tokens.is_empty() {
            n.remove_attr("class");
        } else {
            n.set_attr("class", &tokens.join(" "));
        }
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl DomTokenList {
    /// `classList.add(token)` — add `token` to the class list,
    /// deduping. Tokens with internal whitespace are accepted as-is
    /// (the DOM spec throws `InvalidCharacterError`; we permit them
    /// for now, but skip the empty string).
    fn add(&self, token: String) {
        if token.is_empty() {
            return;
        }
        let mut tokens = self.tokens();
        if !tokens.iter().any(|t| t == &token) {
            tokens.push(token);
        }
        self.write(&tokens);
    }

    /// `classList.remove(token)` — remove every occurrence of
    /// `token` from the class list. No-op if absent.
    fn remove(&self, token: String) {
        let mut tokens = self.tokens();
        let before = tokens.len();
        tokens.retain(|t| t != &token);
        if tokens.len() != before {
            self.write(&tokens);
        }
    }

    /// `classList.toggle(token)` — remove `token` if present, add it
    /// if absent. Returns the resulting presence (true = now present).
    fn toggle(&self, token: String) -> bool {
        if token.is_empty() {
            return false;
        }
        let mut tokens = self.tokens();
        if let Some(pos) = tokens.iter().position(|t| t == &token) {
            tokens.remove(pos);
            self.write(&tokens);
            false
        } else {
            tokens.push(token);
            self.write(&tokens);
            true
        }
    }

    /// `classList.contains(token)` — true if `token` is in the list.
    fn contains(&self, token: String) -> bool {
        self.tokens().iter().any(|t| t == &token)
    }
}

/// Register the [`Document`], [`Element`], and [`DomTokenList`]
/// classes on `ctx.globals()` so JS code can recognize their types
/// (and so the engine can later `Class::instance` them). Idempotent —
/// calling twice is safe; QuickJS will re-bind the constructor.
pub(crate) fn register_classes(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    Class::<Document>::define(&ctx.globals())?;
    Class::<Element>::define(&ctx.globals())?;
    Class::<DomTokenList>::define(&ctx.globals())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(html: &str) -> Document {
        Document::from_html(html)
    }

    // ===== Read-only methods (preserved from Phase 1B Day 1) =====

    #[test]
    fn document_query_selector_finds_element() {
        let d = doc(r#"<html><body><h1 id="hi">Hello</h1><p>world</p></body></html>"#);
        let h1 = d.query_selector_inner("h1").expect("h1 present");
        assert_eq!(h1.tag_name(), "H1");
        assert_eq!(h1.id(), "hi");
        assert_eq!(h1.text_content(), "Hello");
    }

    #[test]
    fn document_query_selector_returns_none_when_no_match() {
        let d = doc("<html><body><p>hi</p></body></html>");
        assert!(d.query_selector_inner("nav").is_none());
    }

    #[test]
    fn document_query_selector_all_returns_doc_order() {
        let d = doc(r#"<html><body><li>a</li><li>b</li><li>c</li></body></html>"#);
        let lis = d.query_selector_all("li".to_owned());
        assert_eq!(lis.len(), 3);
        assert_eq!(lis[0].text_content(), "a");
        assert_eq!(lis[1].text_content(), "b");
        assert_eq!(lis[2].text_content(), "c");
    }

    #[test]
    fn document_get_element_by_id_works_with_dotted_id() {
        // Dotted ids would be invalid CSS selectors, but valid HTML.
        let d = doc(r##"<html><body><div id="x.y.z">found</div></body></html>"##);
        let el = d.get_element_by_id("x.y.z".to_owned()).expect("el");
        assert_eq!(el.text_content(), "found");
    }

    #[test]
    fn document_get_element_by_id_returns_none_when_no_match() {
        let d = doc("<html><body><p>hi</p></body></html>");
        assert!(d.get_element_by_id("nope".to_owned()).is_none());
    }

    #[test]
    fn document_title_getter() {
        let d = doc("<html><head><title>  Hello World  </title></head><body></body></html>");
        assert_eq!(d.title(), "Hello World");
    }

    #[test]
    fn document_body_and_head_getters() {
        let d = doc("<html><head><meta charset=utf-8></head><body><p>x</p></body></html>");
        let body = d.body().expect("body");
        let head = d.head().expect("head");
        assert_eq!(body.tag_name(), "BODY");
        assert_eq!(head.tag_name(), "HEAD");
        assert_eq!(
            body.query_selector("p".to_owned()).unwrap().text_content(),
            "x"
        );
    }

    #[test]
    fn element_get_attribute_returns_some_and_none() {
        let d =
            doc(r#"<html><body><a href="https://example.com" class="btn">go</a></body></html>"#);
        let a = d.query_selector_inner("a").expect("a");
        assert_eq!(
            a.get_attribute("href".to_owned()),
            Some("https://example.com".to_owned())
        );
        assert_eq!(a.get_attribute("class".to_owned()), Some("btn".to_owned()));
        assert_eq!(a.get_attribute("missing".to_owned()), None);
    }

    #[test]
    fn element_has_attribute() {
        let d = doc(r#"<html><body><input type="text" required></body></html>"#);
        let input = d.query_selector_inner("input").expect("input");
        assert!(input.has_attribute("type".to_owned()));
        assert!(input.has_attribute("required".to_owned()));
        assert!(!input.has_attribute("nope".to_owned()));
    }

    #[test]
    fn element_inner_html_and_outer_html() {
        let d = doc(r#"<html><body><div class="wrap"><p>hi</p></div></body></html>"#);
        let div = d.query_selector_inner(".wrap").expect("div");
        assert!(div.inner_html().contains("<p>hi</p>"));
        let outer = div.outer_html();
        assert!(outer.contains(r#"<div class="wrap">"#));
        assert!(outer.contains("</div>"));
    }

    #[test]
    fn element_text_content_concatenates_descendants() {
        let d = doc("<html><body><div>foo <b>bar</b> baz</div></body></html>");
        let div = d.query_selector_inner("div").expect("div");
        assert_eq!(div.text_content(), "foo bar baz");
    }

    #[test]
    fn element_query_selector_is_scoped_to_subtree() {
        let d = doc("<html><body><div class=a><p>inside</p></div><p>outside</p></body></html>");
        let a = d.query_selector_inner(".a").expect("div.a");
        let p = a.query_selector("p".to_owned()).expect("p inside");
        // Should find "inside", not "outside" — scope is the subtree.
        assert_eq!(p.text_content(), "inside");
    }

    #[test]
    fn element_children_skips_text_nodes() {
        let d = doc("<html><body><ul>text<li>one</li>more text<li>two</li></ul></body></html>");
        let ul = d.query_selector_inner("ul").expect("ul");
        let kids = ul.children();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].text_content(), "one");
        assert_eq!(kids[1].text_content(), "two");
    }

    #[test]
    fn element_parent_element_walks_up() {
        let d = doc("<html><body><div><section><p>x</p></section></div></body></html>");
        let p = d.query_selector_inner("p").expect("p");
        let section = p.parent_element().expect("section");
        assert_eq!(section.tag_name(), "SECTION");
        let div = section.parent_element().expect("div");
        assert_eq!(div.tag_name(), "DIV");
    }

    #[test]
    fn element_tag_name_is_uppercase() {
        let d = doc("<html><body><Section><Article></Article></Section></body></html>");
        // The parser lowercases tag names; we re-uppercase per DOM spec.
        let s = d.query_selector_inner("section").expect("section");
        assert_eq!(s.tag_name(), "SECTION");
        assert_eq!(s.local_name(), "section");
    }

    #[test]
    fn element_class_name_property() {
        let d = doc(r#"<html><body><div class="a b c">x</div></body></html>"#);
        let dv = d.query_selector_inner("div").expect("div");
        assert_eq!(dv.class_name(), "a b c");
    }

    #[test]
    fn document_element_returns_html() {
        let d = doc("<html><body><p>x</p></body></html>");
        let root = d.document_element().expect("root");
        assert_eq!(root.tag_name(), "HTML");
    }

    #[test]
    fn invalid_selector_yields_empty_results_not_panic() {
        let d = doc("<html><body><p>x</p></body></html>");
        // ":::::" is not a parseable CSS selector.
        assert!(d.query_selector_inner(":::::").is_none());
        assert!(d.query_selector_all(":::::".to_owned()).is_empty());
    }

    // ===== Mutation surface (new in this phase) =====

    #[test]
    fn set_attribute_round_trips_through_get_attribute() {
        let d = doc(r#"<html><body><a href="/old">x</a></body></html>"#);
        let a = d.query_selector_inner("a").expect("a");
        a.set_attribute("href".to_owned(), Some(rquickjs::Coerced("/new".to_owned())));
        assert_eq!(a.get_attribute("href".to_owned()), Some("/new".to_owned()));
        // A new attribute name should also be writable.
        a.set_attribute("data-x".to_owned(), Some(rquickjs::Coerced("42".to_owned())));
        assert_eq!(a.get_attribute("data-x".to_owned()), Some("42".to_owned()));
        // outer_html reflects the change.
        assert!(a.outer_html().contains("data-x=\"42\""));
    }

    #[test]
    fn remove_attribute_drops_the_attribute() {
        let d = doc(r#"<html><body><input type="text" required disabled></body></html>"#);
        let i = d.query_selector_inner("input").expect("input");
        assert!(i.has_attribute("required".to_owned()));
        i.remove_attribute("required".to_owned());
        assert!(!i.has_attribute("required".to_owned()));
        // Removing absent attribute is a no-op (not a panic).
        i.remove_attribute("nope".to_owned());
        assert!(i.has_attribute("disabled".to_owned()));
    }

    #[test]
    fn inner_html_setter_parses_and_replaces_children() {
        let d = doc("<html><body><div><p>old</p></div></body></html>");
        let div = d.query_selector_inner("div").expect("div");
        div.set_inner_html("<span>new1</span><span>new2</span>".to_owned());
        // Old child is gone.
        assert!(!div.inner_html().contains("<p>old</p>"));
        // New children are parsed and present.
        assert!(div.inner_html().contains("<span>new1</span>"));
        assert!(div.inner_html().contains("<span>new2</span>"));
        // children() now yields two spans.
        let kids = div.children();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].tag_name(), "SPAN");
        assert_eq!(kids[1].text_content(), "new2");
    }

    #[test]
    fn text_content_setter_replaces_children_with_text_node() {
        let d = doc("<html><body><div><p>old</p><span>more</span></div></body></html>");
        let div = d.query_selector_inner("div").expect("div");
        div.set_text_content("Just a string with <not a tag>".to_owned());
        // textContent reflects the new value.
        assert_eq!(div.text_content(), "Just a string with <not a tag>");
        // Children are gone (text setter does not parse HTML).
        assert_eq!(div.children().len(), 0);
        // innerHTML escapes the angle brackets.
        let inner = div.inner_html();
        assert!(inner.contains("&lt;not a tag&gt;"), "got: {inner:?}");
    }

    #[test]
    fn append_child_reparents_existing_element() {
        let d = doc(
            "<html><body><div id=\"src\"><p id=\"item\">x</p></div><div id=\"dst\"></div></body></html>",
        );
        let item = d.get_element_by_id("item".to_owned()).expect("item");
        let dst = d.get_element_by_id("dst".to_owned()).expect("dst");
        let src = d.get_element_by_id("src".to_owned()).expect("src");

        // Before: item is inside src.
        assert_eq!(src.children().len(), 1);
        assert_eq!(dst.children().len(), 0);

        let returned = dst.append_child(item.clone());
        assert_eq!(returned.id(), "item");

        // After: item is inside dst, gone from src.
        assert_eq!(src.children().len(), 0);
        assert_eq!(dst.children().len(), 1);
        assert_eq!(dst.children()[0].id(), "item");
    }

    #[test]
    fn remove_child_detaches_from_parent_only() {
        let d = doc(
            "<html><body><div id=\"a\"><p id=\"p1\">x</p><p id=\"p2\">y</p></div><div id=\"b\"><p id=\"p3\">z</p></div></body></html>",
        );
        let a = d.get_element_by_id("a".to_owned()).expect("a");
        let b = d.get_element_by_id("b".to_owned()).expect("b");
        let p1 = d.get_element_by_id("p1".to_owned()).expect("p1");
        let p3 = d.get_element_by_id("p3".to_owned()).expect("p3");

        // Remove p1 from a: succeeds.
        a.remove_child_rs(p1);
        let remaining: Vec<String> = a.children().into_iter().map(|c| c.id()).collect();
        assert_eq!(remaining, vec!["p2".to_owned()]);

        // Try to remove p3 (child of b) from a: no-op.
        a.remove_child_rs(p3);
        assert_eq!(b.children().len(), 1);
        assert_eq!(b.children()[0].id(), "p3");
    }

    #[test]
    fn class_list_add_adds_and_dedups() {
        let d = doc(r#"<html><body><div class="a">x</div></body></html>"#);
        let div = d.query_selector_inner("div").expect("div");
        let cl = div.class_list();
        cl.add("b".to_owned());
        cl.add("c".to_owned());
        cl.add("b".to_owned()); // duplicate — should be a no-op
        assert!(cl.contains("a".to_owned()));
        assert!(cl.contains("b".to_owned()));
        assert!(cl.contains("c".to_owned()));
        let class = div.class_name();
        // Count of "b" is exactly one.
        assert_eq!(
            class.split_ascii_whitespace().filter(|t| *t == "b").count(),
            1
        );
    }

    #[test]
    fn class_list_remove_drops_token() {
        let d = doc(r#"<html><body><div class="a b c">x</div></body></html>"#);
        let div = d.query_selector_inner("div").expect("div");
        let cl = div.class_list();
        cl.remove("b".to_owned());
        assert!(!cl.contains("b".to_owned()));
        assert!(cl.contains("a".to_owned()));
        assert!(cl.contains("c".to_owned()));
        // Removing an absent token is a no-op.
        cl.remove("nope".to_owned());
        assert!(cl.contains("a".to_owned()));
    }

    #[test]
    fn class_list_toggle_flips_presence() {
        let d = doc(r#"<html><body><div class="a">x</div></body></html>"#);
        let div = d.query_selector_inner("div").expect("div");
        let cl = div.class_list();
        // a is present → toggle removes; returns false.
        assert!(!cl.toggle("a".to_owned()));
        assert!(!cl.contains("a".to_owned()));
        // a is absent → toggle adds; returns true.
        assert!(cl.toggle("a".to_owned()));
        assert!(cl.contains("a".to_owned()));
        // New token toggling on.
        assert!(cl.toggle("highlight".to_owned()));
        assert!(cl.contains("highlight".to_owned()));
    }

    #[test]
    fn class_list_contains_distinguishes_substring_from_token() {
        let d = doc(r#"<html><body><div class="alpha beta">x</div></body></html>"#);
        let div = d.query_selector_inner("div").expect("div");
        let cl = div.class_list();
        assert!(cl.contains("alpha".to_owned()));
        assert!(cl.contains("beta".to_owned()));
        // Substring "alp" is not a token.
        assert!(!cl.contains("alp".to_owned()));
    }

    #[test]
    fn class_list_remove_last_clears_attribute() {
        let d = doc(r#"<html><body><div class="solo">x</div></body></html>"#);
        let div = d.query_selector_inner("div").expect("div");
        let cl = div.class_list();
        cl.remove("solo".to_owned());
        // After removing the sole token, the `class` attribute is
        // gone from the serialized output entirely.
        assert!(!div.has_attribute("class".to_owned()));
        assert_eq!(div.class_name(), "");
    }
}
