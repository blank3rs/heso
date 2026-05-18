//! # dom
//!
//! Phase 1B (read-only) of the agent-shaped DOM per [ADR 0014].
//!
//! What this module gives you:
//!
//! - A [`Document`] Rust struct exposed to JavaScript as the global
//!   `document` once an HTML page has been loaded into the engine.
//! - An [`Element`] Rust struct returned from queries on [`Document`]
//!   or other elements.
//! - Just enough method surface for an agent's page to introspect
//!   itself: `document.querySelector`, `document.querySelectorAll`,
//!   `document.getElementById`, `element.querySelector`,
//!   `element.querySelectorAll`, `element.getAttribute`,
//!   `element.textContent`, `element.innerHTML`, `element.outerHTML`,
//!   `element.tagName`, `element.id`, `element.className`.
//!
//! What this module does NOT yet do:
//!
//! - **No mutations.** `element.setAttribute`, `appendChild`,
//!   `removeChild`, `textContent =`, `innerHTML =`, `classList.add/...`
//!   are Phase 1C. Real pages mutate during hydration; we currently
//!   silently ignore mutation attempts (or they don't compile, since
//!   we expose no setters).
//! - **No events.** `addEventListener`, `removeEventListener`,
//!   `dispatchEvent`, `click()` — Phase 1C / 1D.
//! - **No layout.** `getBoundingClientRect`, `offsetWidth`, etc. — out
//!   of scope per ADR 0014 and the [headless-browser-for-agents]
//!   positioning. We return zero / null.
//!
//! ## Why "agent-shaped"
//!
//! The DOM standard is huge. We implement the read-and-traverse half
//! that real pages and real agents use to find content, and leave the
//! layout-and-paint half out. Per [ADR 0016] this is the bet: the
//! "agent-relevant half" of the browser surface is what we ship; the
//! rest of Chromium is bloat for the agent use case.
//!
//! ## Lifetime story
//!
//! [`scraper::Html`] owns a `'static`-friendly tree. We wrap it in an
//! [`Arc`] so multiple [`Element`] handles can outlive any given
//! JavaScript call without borrowing. Each [`Element`] stores
//! `(Arc<Html>, NodeId)` and looks up its current `NodeRef` on every
//! access — cheap, since `ego_tree::Tree::get(node_id)` is O(1).
//!
//! [ADR 0014]: ../../decisions/0014-bundled-quickjs-agent-dom.md
//! [ADR 0016]: ../../decisions/0016-positioning-headless-browser-for-agents.md

use std::sync::Arc;

use ego_tree::NodeId;
use rquickjs::{class::Trace, Class, Ctx, JsLifetime};
use scraper::{ElementRef as ScraperElementRef, Html, Node, Selector};

/// The `document` global.
///
/// Wraps a parsed [`scraper::Html`]. Construction is from Rust only
/// — JavaScript cannot `new Document()` because no `#[qjs(constructor)]`
/// is provided. The engine installs a single instance under the
/// `document` global at page-load time.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class]
pub struct Document {
    /// Backing parse tree. Shared with all [`Element`] handles
    /// produced from this document.
    #[qjs(skip_trace)]
    html: Arc<Html>,
}

impl Document {
    /// Construct a new [`Document`] wrapping `html`. Called by the
    /// engine; not exposed to JS.
    pub fn new(html: Arc<Html>) -> Self {
        Self { html }
    }

    /// Borrow the underlying [`scraper::Html`] (useful for tests
    /// and for the engine to introspect the parse alongside the JS).
    pub fn html(&self) -> &Html {
        &self.html
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl Document {
    /// `document.querySelector(selector)` — return the first element
    /// matching `selector`, or `null`.
    fn query_selector(&self, selector: String) -> Option<Element> {
        let sel = Selector::parse(&selector).ok()?;
        let first = self.html.select(&sel).next()?;
        Some(Element::from_id(self.html.clone(), first.id()))
    }

    /// `document.querySelectorAll(selector)` — return all matching
    /// elements as an array, in document order. An invalid selector
    /// yields an empty array (DOM `Document.querySelectorAll`
    /// actually throws `SyntaxError`; we'll align in Phase 1C).
    fn query_selector_all(&self, selector: String) -> Vec<Element> {
        let sel = match Selector::parse(&selector) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        self.html
            .select(&sel)
            .map(|e| Element::from_id(self.html.clone(), e.id()))
            .collect()
    }

    /// `document.getElementById(id)` — return the first element whose
    /// `id` attribute equals `id`, or `null`.
    ///
    /// Implemented as a tree walk rather than a CSS-selector
    /// shortcut so we don't depend on `id` being a valid CSS
    /// identifier (real-world ids contain dots, brackets, slashes).
    fn get_element_by_id(&self, id: String) -> Option<Element> {
        for node in self.html.tree.nodes() {
            if let Some(elem) = node.value().as_element() {
                if elem.attr("id") == Some(&id) {
                    return Some(Element::from_id(self.html.clone(), node.id()));
                }
            }
        }
        None
    }

    /// `document.documentElement` — the root `<html>` element, or
    /// `null` if the parse is empty.
    #[qjs(get)]
    fn document_element(&self) -> Option<Element> {
        Selector::parse("html")
            .ok()
            .and_then(|sel| self.html.select(&sel).next())
            .map(|el| Element::from_id(self.html.clone(), el.id()))
    }

    /// `document.body` — the `<body>` element, or `null`.
    #[qjs(get)]
    fn body(&self) -> Option<Element> {
        Selector::parse("body")
            .ok()
            .and_then(|sel| self.html.select(&sel).next())
            .map(|el| Element::from_id(self.html.clone(), el.id()))
    }

    /// `document.head` — the `<head>` element, or `null`.
    #[qjs(get)]
    fn head(&self) -> Option<Element> {
        Selector::parse("head")
            .ok()
            .and_then(|sel| self.html.select(&sel).next())
            .map(|el| Element::from_id(self.html.clone(), el.id()))
    }

    /// `document.title` — text content of the `<title>` tag, or
    /// empty string.
    #[qjs(get)]
    fn title(&self) -> String {
        Selector::parse("title")
            .ok()
            .and_then(|sel| self.html.select(&sel).next())
            .map(|el| el.text().collect::<String>().trim().to_owned())
            .unwrap_or_default()
    }
}

/// A handle to a single element in a [`Document`]'s tree.
///
/// Holds a refcounted handle to the parent [`scraper::Html`] plus the
/// `NodeId` of this element. All access is via the parse tree — we
/// never store a borrowed `ElementRef` because that would tie the
/// type to a specific borrow lifetime that doesn't survive
/// JavaScript call boundaries.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class]
pub struct Element {
    #[qjs(skip_trace)]
    html: Arc<Html>,
    #[qjs(skip_trace)]
    node_id: NodeId,
}

impl Element {
    fn from_id(html: Arc<Html>, node_id: NodeId) -> Self {
        Self { html, node_id }
    }

    /// Resolve this element's [`NodeRef`] in the backing tree.
    fn node_ref(&self) -> ego_tree::NodeRef<'_, Node> {
        self.html
            .tree
            .get(self.node_id)
            .expect("Element NodeId always belongs to its backing Html tree")
    }

    /// Wrap as a `scraper::ElementRef` for `text()` / `inner_html()`
    /// etc. Returns `None` if the node has somehow stopped being an
    /// element — which can't happen via our constructors but is
    /// defensive.
    fn element_ref(&self) -> Option<ScraperElementRef<'_>> {
        ScraperElementRef::wrap(self.node_ref())
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl Element {
    /// `element.tagName` — uppercase per the DOM spec
    /// (e.g. `"DIV"`, `"A"`, `"H1"`). Returns empty string for
    /// non-element nodes, which shouldn't be reachable.
    #[qjs(get)]
    fn tag_name(&self) -> String {
        self.node_ref()
            .value()
            .as_element()
            .map(|e| e.name().to_ascii_uppercase())
            .unwrap_or_default()
    }

    /// `element.localName` — lowercase per the DOM spec.
    #[qjs(get)]
    fn local_name(&self) -> String {
        self.node_ref()
            .value()
            .as_element()
            .map(|e| e.name().to_owned())
            .unwrap_or_default()
    }

    /// `element.id` — the element's `id` attribute, or empty string.
    #[qjs(get)]
    fn id(&self) -> String {
        self.node_ref()
            .value()
            .as_element()
            .and_then(|e| e.attr("id"))
            .map(|s| s.to_owned())
            .unwrap_or_default()
    }

    /// `element.className` — the element's `class` attribute, or
    /// empty string. (Not parsed into a list — that's `classList`,
    /// Phase 1C.)
    #[qjs(get)]
    fn class_name(&self) -> String {
        self.node_ref()
            .value()
            .as_element()
            .and_then(|e| e.attr("class"))
            .map(|s| s.to_owned())
            .unwrap_or_default()
    }

    /// `element.textContent` — concatenated text of this element and
    /// all descendants, in document order.
    #[qjs(get)]
    fn text_content(&self) -> String {
        self.element_ref()
            .map(|e| e.text().collect::<String>())
            .unwrap_or_default()
    }

    /// `element.innerHTML` — serialized HTML of this element's
    /// children. Read-only in Phase 1B (no setter).
    ///
    /// Explicit rename: `camelCase` would produce `innerHtml`, but the
    /// DOM spec is `innerHTML` (all caps for the acronym).
    #[qjs(get, rename = "innerHTML")]
    fn inner_html(&self) -> String {
        self.element_ref()
            .map(|e| e.inner_html())
            .unwrap_or_default()
    }

    /// `element.outerHTML` — serialized HTML of this element including
    /// itself. Read-only in Phase 1B.
    ///
    /// Explicit rename for the same reason as [`Self::inner_html`].
    #[qjs(get, rename = "outerHTML")]
    fn outer_html(&self) -> String {
        self.element_ref()
            .map(|e| e.html())
            .unwrap_or_default()
    }

    /// `element.getAttribute(name)` — return the attribute value, or
    /// `null` if not present.
    fn get_attribute(&self, name: String) -> Option<String> {
        self.node_ref()
            .value()
            .as_element()?
            .attr(&name)
            .map(|s| s.to_owned())
    }

    /// `element.hasAttribute(name)` — return true if the attribute
    /// is present (even if empty).
    fn has_attribute(&self, name: String) -> bool {
        self.node_ref()
            .value()
            .as_element()
            .map(|e| e.attr(&name).is_some())
            .unwrap_or(false)
    }

    /// `element.querySelector(selector)` — return the first descendant
    /// matching `selector`, or `null`.
    ///
    /// Scope: descendants only. `selector` resolves against the
    /// subtree rooted at this element, not the full document.
    fn query_selector(&self, selector: String) -> Option<Element> {
        let sel = Selector::parse(&selector).ok()?;
        let el_ref = self.element_ref()?;
        let first = el_ref.select(&sel).next()?;
        Some(Element::from_id(self.html.clone(), first.id()))
    }

    /// `element.querySelectorAll(selector)` — return all descendants
    /// matching `selector`, in document order.
    fn query_selector_all(&self, selector: String) -> Vec<Element> {
        let sel = match Selector::parse(&selector) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let el_ref = match self.element_ref() {
            Some(e) => e,
            None => return Vec::new(),
        };
        el_ref
            .select(&sel)
            .map(|e| Element::from_id(self.html.clone(), e.id()))
            .collect()
    }

    /// `element.children` — direct element children (skip text /
    /// comment nodes), in document order.
    #[qjs(get)]
    fn children(&self) -> Vec<Element> {
        self.node_ref()
            .children()
            .filter(|n| n.value().as_element().is_some())
            .map(|n| Element::from_id(self.html.clone(), n.id()))
            .collect()
    }

    /// `element.parentElement` — closest element ancestor, or `null`
    /// for the root.
    #[qjs(get)]
    fn parent_element(&self) -> Option<Element> {
        let mut cur = self.node_ref().parent();
        while let Some(n) = cur {
            if n.value().as_element().is_some() {
                return Some(Element::from_id(self.html.clone(), n.id()));
            }
            cur = n.parent();
        }
        None
    }
}

/// Register the [`Document`] and [`Element`] classes on `ctx.globals()`
/// so JS code can recognize their types (and so the engine can later
/// `Class::instance` them). Idempotent — calling twice is safe;
/// QuickJS will re-bind the constructor.
pub(crate) fn register_classes(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    Class::<Document>::define(&ctx.globals())?;
    Class::<Element>::define(&ctx.globals())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn parse(html: &str) -> Arc<Html> {
        Arc::new(Html::parse_document(html))
    }

    #[test]
    fn document_query_selector_finds_element() {
        let doc = Document::new(parse(
            r#"<html><body><h1 id="hi">Hello</h1><p>world</p></body></html>"#,
        ));
        let h1 = doc.query_selector("h1".to_owned()).expect("h1 present");
        assert_eq!(h1.tag_name(), "H1");
        assert_eq!(h1.id(), "hi");
        assert_eq!(h1.text_content(), "Hello");
    }

    #[test]
    fn document_query_selector_returns_none_when_no_match() {
        let doc = Document::new(parse("<html><body><p>hi</p></body></html>"));
        assert!(doc.query_selector("nav".to_owned()).is_none());
    }

    #[test]
    fn document_query_selector_all_returns_doc_order() {
        let doc = Document::new(parse(
            r#"<html><body><li>a</li><li>b</li><li>c</li></body></html>"#,
        ));
        let lis = doc.query_selector_all("li".to_owned());
        assert_eq!(lis.len(), 3);
        assert_eq!(lis[0].text_content(), "a");
        assert_eq!(lis[1].text_content(), "b");
        assert_eq!(lis[2].text_content(), "c");
    }

    #[test]
    fn document_get_element_by_id_works_with_dotted_id() {
        // Dotted ids would be invalid CSS selectors, but valid HTML.
        let doc = Document::new(parse(
            r##"<html><body><div id="x.y.z">found</div></body></html>"##,
        ));
        let el = doc.get_element_by_id("x.y.z".to_owned()).expect("el");
        assert_eq!(el.text_content(), "found");
    }

    #[test]
    fn document_get_element_by_id_returns_none_when_no_match() {
        let doc = Document::new(parse("<html><body><p>hi</p></body></html>"));
        assert!(doc.get_element_by_id("nope".to_owned()).is_none());
    }

    #[test]
    fn document_title_getter() {
        let doc = Document::new(parse(
            "<html><head><title>  Hello World  </title></head><body></body></html>",
        ));
        assert_eq!(doc.title(), "Hello World");
    }

    #[test]
    fn document_body_and_head_getters() {
        let doc = Document::new(parse(
            "<html><head><meta charset=utf-8></head><body><p>x</p></body></html>",
        ));
        let body = doc.body().expect("body");
        let head = doc.head().expect("head");
        assert_eq!(body.tag_name(), "BODY");
        assert_eq!(head.tag_name(), "HEAD");
        assert_eq!(body.query_selector("p".to_owned()).unwrap().text_content(), "x");
    }

    #[test]
    fn element_get_attribute_returns_some_and_none() {
        let doc = Document::new(parse(
            r#"<html><body><a href="https://example.com" class="btn">go</a></body></html>"#,
        ));
        let a = doc.query_selector("a".to_owned()).expect("a");
        assert_eq!(a.get_attribute("href".to_owned()), Some("https://example.com".to_owned()));
        assert_eq!(a.get_attribute("class".to_owned()), Some("btn".to_owned()));
        assert_eq!(a.get_attribute("missing".to_owned()), None);
    }

    #[test]
    fn element_has_attribute() {
        let doc = Document::new(parse(
            r#"<html><body><input type="text" required></body></html>"#,
        ));
        let input = doc.query_selector("input".to_owned()).expect("input");
        assert!(input.has_attribute("type".to_owned()));
        assert!(input.has_attribute("required".to_owned()));
        assert!(!input.has_attribute("nope".to_owned()));
    }

    #[test]
    fn element_inner_html_and_outer_html() {
        let doc = Document::new(parse(
            r#"<html><body><div class="wrap"><p>hi</p></div></body></html>"#,
        ));
        let div = doc.query_selector(".wrap".to_owned()).expect("div");
        assert!(div.inner_html().contains("<p>hi</p>"));
        let outer = div.outer_html();
        assert!(outer.contains(r#"<div class="wrap">"#));
        assert!(outer.contains("</div>"));
    }

    #[test]
    fn element_text_content_concatenates_descendants() {
        let doc = Document::new(parse(
            "<html><body><div>foo <b>bar</b> baz</div></body></html>",
        ));
        let div = doc.query_selector("div".to_owned()).expect("div");
        assert_eq!(div.text_content(), "foo bar baz");
    }

    #[test]
    fn element_query_selector_is_scoped_to_subtree() {
        let doc = Document::new(parse(
            "<html><body><div class=a><p>inside</p></div><p>outside</p></body></html>",
        ));
        let a = doc.query_selector(".a".to_owned()).expect("div.a");
        let p = a.query_selector("p".to_owned()).expect("p inside");
        // Should find "inside", not "outside" — scope is the subtree.
        assert_eq!(p.text_content(), "inside");
    }

    #[test]
    fn element_children_skips_text_nodes() {
        let doc = Document::new(parse(
            "<html><body><ul>text<li>one</li>more text<li>two</li></ul></body></html>",
        ));
        let ul = doc.query_selector("ul".to_owned()).expect("ul");
        let kids = ul.children();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].text_content(), "one");
        assert_eq!(kids[1].text_content(), "two");
    }

    #[test]
    fn element_parent_element_walks_up() {
        let doc = Document::new(parse(
            "<html><body><div><section><p>x</p></section></div></body></html>",
        ));
        let p = doc.query_selector("p".to_owned()).expect("p");
        let section = p.parent_element().expect("section");
        assert_eq!(section.tag_name(), "SECTION");
        let div = section.parent_element().expect("div");
        assert_eq!(div.tag_name(), "DIV");
    }

    #[test]
    fn element_tag_name_is_uppercase() {
        let doc = Document::new(parse(
            "<html><body><Section><Article></Article></Section></body></html>",
        ));
        // scraper lowercases by default; we re-uppercase per DOM spec.
        let s = doc.query_selector("section".to_owned()).expect("section");
        assert_eq!(s.tag_name(), "SECTION");
        assert_eq!(s.local_name(), "section");
    }

    #[test]
    fn element_class_name_property() {
        let doc = Document::new(parse(
            r#"<html><body><div class="a b c">x</div></body></html>"#,
        ));
        let d = doc.query_selector("div".to_owned()).expect("div");
        assert_eq!(d.class_name(), "a b c");
    }

    #[test]
    fn document_element_returns_html() {
        let doc = Document::new(parse("<html><body><p>x</p></body></html>"));
        let root = doc.document_element().expect("root");
        assert_eq!(root.tag_name(), "HTML");
    }

    #[test]
    fn invalid_selector_yields_empty_results_not_panic() {
        let doc = Document::new(parse("<html><body><p>x</p></body></html>"));
        // ":invalid:::pseudo" is not a parseable CSS selector.
        assert!(doc.query_selector(":::::".to_owned()).is_none());
        assert!(doc.query_selector_all(":::::".to_owned()).is_empty());
    }
}
