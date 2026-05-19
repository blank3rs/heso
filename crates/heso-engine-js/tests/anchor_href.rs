//! Integration tests for the `HTMLHyperlinkElementUtils` mixin on
//! `<a>` and `<area>` elements per WHATWG HTML §4.6.6:
//!
//! - `href` getter resolves the `href` content attribute against the
//!   document base URL (`globalThis.location.href`).
//! - `href` setter writes the `href` content attribute verbatim.
//! - Decomposition getters/setters (`protocol`, `host`, `hostname`,
//!   `port`, `pathname`, `search`, `hash`, `origin`, `username`,
//!   `password`) read/write into the parsed URL and round-trip
//!   through `href`.
//!
//! Bug-of-record: the May 2026 agent-driven HN extraction test
//! discovered `a.href` returned `undefined`, forcing every Playwright
//! migration to fall back to `a.getAttribute('href')`. See
//! `AGENT_FINDINGS.md` (commit `2cebf12`) for the original report.
//! These tests pin the spec-correct behavior so the bug stays fixed.
//!
//! Spec: <https://html.spec.whatwg.org/multipage/links.html#htmlhyperlinkelementutils>.

use heso_engine_js::{JsEngine, JsSession};
use url::Url;

/// Convenience: a representative document base URL for "the page is
/// served from <https://news.ycombinator.com/>". Matches the
/// `AGENT_FINDINGS.md` reproducer.
fn hn_url() -> Url {
    Url::parse("https://news.ycombinator.com/").unwrap()
}

/// Bare engine pointed at a base URL. Mirrors the helper in
/// `tests/history.rs`.
fn engine_at(href: &str) -> JsEngine {
    let e = JsEngine::new().expect("engine new");
    e.set_base_url(Some(Url::parse(href).expect("parse base url")));
    e
}

// =====================================================================
// href getter — the original AGENT_FINDINGS.md failure
// =====================================================================

#[test]
fn anchor_href_returns_resolved_absolute_url_when_attribute_is_absolute() {
    // The AGENT_FINDINGS.md reproducer: HN's story-title anchors use
    // absolute `href` attributes. `a.href` MUST return the canonical
    // absolute string, not undefined / empty.
    let html = r#"<!doctype html><html><body>
        <a id="story" href="https://example.com/foo">title</a>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval("document.getElementById('story').href")
        .unwrap();
    assert_eq!(out.value, "https://example.com/foo");
}

#[test]
fn anchor_href_resolves_relative_path_against_document_base() {
    // The bug that broke Wikipedia-style internal links: a relative
    // `href="/wiki/Anthropic"` on a page at `https://en.wikipedia.org/`
    // must serialize to the absolute URL via the document base.
    let html = r#"<!doctype html><html><body>
        <a id="link" href="/foo/bar">link</a>
    </body></html>"#;
    let target = Url::parse("https://en.wikipedia.org/wiki/Anthropic").unwrap();
    let (sess, _) = JsSession::open(html, target).unwrap();
    let out = sess.eval("document.getElementById('link').href").unwrap();
    assert_eq!(out.value, "https://en.wikipedia.org/foo/bar");
}

#[test]
fn anchor_href_resolves_relative_filename_against_document_base() {
    // Trailing-slash-significance corner from the `url` crate docs:
    // `base = "https://example.com/a/b.html"` + `href = "c.png"` →
    // `https://example.com/a/c.png` (because `b.html` is treated as
    // a file).
    let html = r#"<!doctype html><html><body>
        <a id="link" href="c.png">link</a>
    </body></html>"#;
    let target = Url::parse("https://example.com/a/b.html").unwrap();
    let (sess, _) = JsSession::open(html, target).unwrap();
    let out = sess.eval("document.getElementById('link').href").unwrap();
    assert_eq!(out.value, "https://example.com/a/c.png");
}

#[test]
fn anchor_href_returns_empty_string_when_no_href_attribute() {
    // Per spec: "If this's href content attribute is absent, return
    // the empty string." No attribute → "".
    let html = r#"<!doctype html><html><body>
        <a id="naked">no href</a>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval("document.getElementById('naked').href")
        .unwrap();
    assert_eq!(out.value, "");
}

#[test]
fn anchor_href_setter_writes_through_to_attribute() {
    // The setter is "set the href content attribute to the given
    // string" — no URL parsing happens at write time. Both the IDL
    // `.href` and `.getAttribute('href')` must reflect the new value
    // on the next read.
    let html = r#"<!doctype html><html><body>
        <a id="a" href="https://old.example/">old</a>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            a.href = 'https://new.example/path';
            [a.href, a.getAttribute('href')]
            "#,
        )
        .unwrap();
    assert_eq!(
        out.value,
        serde_json::json!([
            "https://new.example/path",
            "https://new.example/path"
        ])
    );
}

#[test]
fn anchor_href_setter_can_write_relative_path_which_serializes_absolute_on_read() {
    // Setter writes the attribute verbatim, getter resolves against
    // base. After `a.href = "/abs"` we expect getAttribute to show
    // the literal string and .href to show the absolute resolution.
    let html = r#"<!doctype html><html><body>
        <a id="a" href="https://old.example/">old</a>
    </body></html>"#;
    let target = Url::parse("https://docs.example/api").unwrap();
    let (sess, _) = JsSession::open(html, target).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            a.href = '/abs';
            [a.href, a.getAttribute('href')]
            "#,
        )
        .unwrap();
    assert_eq!(
        out.value,
        serde_json::json!(["https://docs.example/abs", "/abs"])
    );
}

#[test]
fn anchor_href_returns_raw_attribute_on_parse_failure_with_no_base() {
    // Spec corner: an engine constructed without a base URL has
    // `location.href = "about:blank"`. A relative href on a
    // not-fully-resolvable base falls through to the raw string.
    // Use a `javascript:` URL which `Url::parse` accepts as an
    // absolute URL with the non-relative flag set — should round-trip
    // verbatim.
    let html = r#"<!doctype html><html><body>
        <a id="js" href="javascript:alert(1)">x</a>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess.eval("document.getElementById('js').href").unwrap();
    assert_eq!(out.value, "javascript:alert(1)");
}

// =====================================================================
// URL decomposition getters
// =====================================================================

#[test]
fn anchor_protocol_includes_trailing_colon() {
    // Per spec, `protocol` includes the trailing `":"`.
    let html = r#"<a id="a" href="https://example.com/p">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess.eval("document.getElementById('a').protocol").unwrap();
    assert_eq!(out.value, "https:");
}

#[test]
fn anchor_protocol_reflects_non_relative_scheme() {
    // jsdom WPT corner: `mycustomprotocol:abc` is a non-relative URL,
    // and `protocol` must surface the custom scheme verbatim.
    let html = r#"<a id="a" href="mycustomprotocol:abc">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess.eval("document.getElementById('a').protocol").unwrap();
    assert_eq!(out.value, "mycustomprotocol:");
}

#[test]
fn anchor_host_hostname_and_port_split() {
    let html = r#"<a id="a" href="https://example.com:8443/p">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            [a.host, a.hostname, a.port]
            "#,
        )
        .unwrap();
    assert_eq!(
        out.value,
        serde_json::json!(["example.com:8443", "example.com", "8443"])
    );
}

#[test]
fn anchor_default_port_is_empty_string() {
    // Per spec, when the URL uses the default port for its scheme
    // (443 for https), `.port` is the empty string.
    let html = r#"<a id="a" href="https://example.com/p">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            [a.host, a.port]
            "#,
        )
        .unwrap();
    assert_eq!(out.value, serde_json::json!(["example.com", ""]));
}

#[test]
fn anchor_pathname_search_hash_decompose_full_url() {
    let html = r#"<a id="a" href="https://example.com/foo/bar?x=1&y=2#frag">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            [a.pathname, a.search, a.hash]
            "#,
        )
        .unwrap();
    assert_eq!(
        out.value,
        serde_json::json!(["/foo/bar", "?x=1&y=2", "#frag"])
    );
}

#[test]
fn anchor_search_and_hash_empty_string_when_absent() {
    let html = r#"<a id="a" href="https://example.com/p">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            [a.search, a.hash]
            "#,
        )
        .unwrap();
    assert_eq!(out.value, serde_json::json!(["", ""]));
}

#[test]
fn anchor_origin_for_http_url() {
    let html = r#"<a id="a" href="https://example.com:443/foo?x=1#h">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess.eval("document.getElementById('a').origin").unwrap();
    // Default port collapses to the scheme's default (port == None
    // in the serialized origin).
    assert_eq!(out.value, "https://example.com");
}

#[test]
fn anchor_origin_for_non_hierarchical_is_null() {
    // `data:` / `javascript:` / opaque-origin schemes serialize to
    // "null" per WHATWG URL spec.
    let html = r#"<a id="a" href="javascript:alert(1)">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess.eval("document.getElementById('a').origin").unwrap();
    assert_eq!(out.value, "null");
}

#[test]
fn anchor_username_and_password_from_userinfo() {
    let html = r#"<a id="a" href="https://alice:s3cret@example.com/p">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            [a.username, a.password]
            "#,
        )
        .unwrap();
    assert_eq!(out.value, serde_json::json!(["alice", "s3cret"]));
}

// =====================================================================
// URL decomposition setters round-trip through href
// =====================================================================

#[test]
fn anchor_pathname_setter_round_trips() {
    let html = r#"<a id="a" href="https://example.com/old">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            a.pathname = '/new';
            a.href
            "#,
        )
        .unwrap();
    assert_eq!(out.value, "https://example.com/new");
}

#[test]
fn anchor_search_setter_strips_leading_question_mark() {
    let html = r#"<a id="a" href="https://example.com/p">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            a.search = '?k=v';
            [a.search, a.href]
            "#,
        )
        .unwrap();
    assert_eq!(
        out.value,
        serde_json::json!(["?k=v", "https://example.com/p?k=v"])
    );
}

#[test]
fn anchor_hash_setter_strips_leading_hash() {
    let html = r#"<a id="a" href="https://example.com/p">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            a.hash = '#frag';
            [a.hash, a.href]
            "#,
        )
        .unwrap();
    assert_eq!(
        out.value,
        serde_json::json!(["#frag", "https://example.com/p#frag"])
    );
}

#[test]
fn anchor_port_setter_writes_through_href() {
    let html = r#"<a id="a" href="https://example.com/p">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            a.port = '8080';
            [a.port, a.host, a.href]
            "#,
        )
        .unwrap();
    assert_eq!(
        out.value,
        serde_json::json!([
            "8080",
            "example.com:8080",
            "https://example.com:8080/p"
        ])
    );
}

#[test]
fn anchor_protocol_setter_round_trips() {
    let html = r#"<a id="a" href="http://example.com/p">x</a>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('a');
            a.protocol = 'https';
            [a.protocol, a.href]
            "#,
        )
        .unwrap();
    assert_eq!(
        out.value,
        serde_json::json!(["https:", "https://example.com/p"])
    );
}

// =====================================================================
// <area> — same mixin per spec
// =====================================================================

#[test]
fn area_href_returns_resolved_absolute_url() {
    // `<area>` shares the same `HTMLHyperlinkElementUtils` mixin as
    // `<a>` per HTML §4.6.6.
    let html = r#"<!doctype html><html><body>
        <map name="m">
            <area id="hot" shape="rect" coords="0,0,10,10" href="/clicked">
        </map>
    </body></html>"#;
    let target = Url::parse("https://example.com/page.html").unwrap();
    let (sess, _) = JsSession::open(html, target).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('hot');
            [a.tagName, a.href, a.protocol, a.host, a.pathname]
            "#,
        )
        .unwrap();
    assert_eq!(
        out.value,
        serde_json::json!([
            "AREA",
            "https://example.com/clicked",
            "https:",
            "example.com",
            "/clicked"
        ])
    );
}

#[test]
fn area_href_setter_writes_through_to_attribute() {
    let html = r#"<map name="m"><area id="hot" href="/old"></map>"#;
    let target = Url::parse("https://example.com/").unwrap();
    let (sess, _) = JsSession::open(html, target).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.getElementById('hot');
            a.href = 'https://new.example/x';
            [a.href, a.getAttribute('href')]
            "#,
        )
        .unwrap();
    assert_eq!(
        out.value,
        serde_json::json!(["https://new.example/x", "https://new.example/x"])
    );
}

// =====================================================================
// Tag gating — mixin is anchor/area-only
// =====================================================================

#[test]
fn href_getter_on_non_hyperlink_tag_returns_empty_string() {
    // `<link>` has its own `href` reflection but not the
    // `HTMLHyperlinkElementUtils` mixin. We don't (yet) support
    // `link.href` either, so the universal "non-mixin tag" answer is
    // `""`. This pins the gate so a future agent doesn't accidentally
    // resolve `<link>` URLs through the anchor pathway.
    let html = r#"<!doctype html><html><head>
        <link id="css" rel="stylesheet" href="/style.css">
    </head><body></body></html>"#;
    let target = Url::parse("https://example.com/page").unwrap();
    let (sess, _) = JsSession::open(html, target).unwrap();
    let out = sess.eval("document.getElementById('css').href").unwrap();
    assert_eq!(out.value, "");
}

#[test]
fn anchor_props_on_div_return_empty_strings() {
    // Reading `.protocol` / `.host` / `.pathname` etc. on a `<div>`
    // must be `""` — these IDL properties live on Element (shared
    // class) but the mixin is gated by tag. Frameworks that
    // feature-detect via `'protocol' in el` see a falsy answer; we
    // give them `""` rather than throwing.
    let html = r#"<div id="d"></div>"#;
    let (sess, _) = JsSession::open(html, hn_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const d = document.getElementById('d');
            [d.href, d.protocol, d.host, d.pathname, d.origin]
            "#,
        )
        .unwrap();
    assert_eq!(out.value, serde_json::json!(["", "", "", "", ""]));
}

// =====================================================================
// createElement('a') — programmatically-created anchors
// =====================================================================

#[test]
fn anchor_created_via_create_element_resolves_href() {
    // Anchors created by `document.createElement('a')` share the
    // same Element class as parsed anchors, so the mixin must work
    // identically.
    let html = r#"<!doctype html><html><body></body></html>"#;
    let target = Url::parse("https://example.com/page").unwrap();
    let (sess, _) = JsSession::open(html, target).unwrap();
    let out = sess
        .eval(
            r#"
            const a = document.createElement('a');
            a.href = '/clicked';
            [a.tagName, a.href, a.pathname]
            "#,
        )
        .unwrap();
    assert_eq!(
        out.value,
        serde_json::json!(["A", "https://example.com/clicked", "/clicked"])
    );
}

// =====================================================================
// `location.href` reflection — base URL is observable via the IDL
// =====================================================================

#[test]
fn anchor_href_reflects_new_base_url_after_navigate() {
    // After `JsSession::navigate`, the document base URL is updated
    // and a parsed-from-attribute relative href must serialize
    // against the new base.
    let html_a = r#"<a id="link" href="/foo">x</a>"#;
    let html_b = r#"<a id="link" href="/foo">x</a>"#;
    let url_a = Url::parse("https://a.example/").unwrap();
    let url_b = Url::parse("https://b.example/").unwrap();
    let (mut sess, _) = JsSession::open(html_a, url_a).unwrap();
    let out_a = sess
        .eval("document.getElementById('link').href")
        .unwrap();
    assert_eq!(out_a.value, "https://a.example/foo");
    sess.navigate(html_b, url_b).unwrap();
    let out_b = sess
        .eval("document.getElementById('link').href")
        .unwrap();
    assert_eq!(out_b.value, "https://b.example/foo");
}

// =====================================================================
// Engine-level sanity (no session) — exercises `set_base_url` directly
// =====================================================================

#[test]
fn engine_eval_with_html_resolves_anchor_href() {
    // No JsSession wrapper — just JsEngine + eval_with_html + a
    // pre-set base URL. Confirms the helper path doesn't depend on
    // session lifecycle.
    let engine = engine_at("https://example.com/path/");
    let html = r#"<a id="a" href="./relative">x</a>"#;
    let out = engine
        .eval_with_html(html, "document.getElementById('a').href")
        .unwrap();
    assert_eq!(out.value, "https://example.com/path/relative");
}
