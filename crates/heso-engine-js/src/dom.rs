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
        }
        Ok(child)
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

    /// `element.value` — read the current `value` attribute of a form
    /// control. Mirrors the standard DOM property for `<input>` /
    /// `<textarea>` / `<select>`. Returns the empty string when the
    /// attribute is absent.
    ///
    /// Phase 1B simplification: stored entirely as an attribute (so
    /// `getAttribute('value')` and `.value` agree). Real browsers
    /// distinguish the *content* attribute from the *IDL* property
    /// (the typed-in text), but until interactive input + reset wiring
    /// matters we collapse the two.
    #[qjs(get)]
    fn value(&self) -> String {
        self.node_ref()
            .and_then(|n| n.attr("value"))
            .map(|t| t.to_string())
            .unwrap_or_default()
    }

    /// `element.value = "..."` — set the `value` attribute. Does not
    /// itself fire `input` / `change` — those are dispatched by the
    /// caller (e.g. [`crate::JsEngine::set_input_value`]).
    #[qjs(set, rename = "value")]
    fn set_value(&self, value: String) {
        if let Some(n) = self.node_ref() {
            n.set_attr("value", &value);
        }
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
/// per the DOM spec). Termination is the first node with no element
/// parent (i.e. the document element or an orphan node still being
/// constructed by `createElement`).
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

    let mut path: Vec<(Option<Object<'js>>, Value<'js>)> = Vec::with_capacity(ids.len());
    for id in ids {
        let map = element_listener_map_opt(ctx, id)?;
        let wrapper = Class::instance(ctx.clone(), Element::from_id(target.doc.clone(), id))?;
        let wrapper_value: Value<'js> = wrapper.into_value();
        // We need the JS Object form for `set(PROP_CURRENT_TARGET, ...)`,
        // but the value the dispatcher pins is the JS Value (which can
        // be the Class instance wrapped). Just pass the Value; the
        // dispatcher stores it directly.
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
