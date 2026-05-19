//! Integration tests for the `HTMLFormElement` IDL surface on
//! `<form>` elements per WHATWG HTML §4.10.3:
//!
//! - `form.action` getter resolves the `action` content attribute
//!   against the document base URL.
//! - `form.method` getter normalizes to `"get"` / `"post"` /
//!   `"dialog"` (lowercase); defaults to `"get"` when missing/invalid.
//! - `form.enctype` / `form.encoding` getter normalize to one of
//!   the three valid types; default `"application/x-www-form-urlencoded"`.
//! - `form.name` / `form.target` / `form.acceptCharset` /
//!   `form.autocomplete` / `form.noValidate` — attribute reflections.
//! - `form.length` counts listed controls in the form.
//! - `form.elements` returns the listed-control array.
//! - `form.submit()` issues a real HTTP request WITHOUT firing the
//!   `submit` event (per WHATWG and jsdom WPT).
//! - `form.reset()` resets controls + fires `reset` event.
//!
//! Bug-of-record: V2 agent findings reported `form.method` /
//! `form.action` / `form.enctype` all returned `undefined` and
//! `form.submit()` threw `TypeError: not a function`. Sibling fix to
//! the `HTMLAnchorElement.href` mixin landed in commit `17ddf77`.
//! See `AGENT_FINDINGS_V2.md` "Bonus findings" + "Top NEW bugs" #3.
//!
//! Spec: <https://html.spec.whatwg.org/multipage/forms.html#the-form-element>.

use std::sync::Arc;

use heso_engine_js::{JsEngine, JsSession, ScriptFetchPolicy};
use url::Url;
use wiremock::matchers::{method as m_method, path as m_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Convenience base URL for documents that don't otherwise need a
/// "real" origin (the form IDL props only care about it for `action`
/// resolution).
fn base_url() -> Url {
    Url::parse("https://example.com/page").unwrap()
}

/// Bare-engine session (no fetch client). Used for IDL-property tests
/// that don't touch the wire. `form.submit()` on this session is a
/// silent no-op (per spec when there's no browsing context).
fn open_session(html: &str) -> JsSession {
    let (sess, _) = JsSession::open(html, base_url()).expect("open session");
    sess
}

/// Live-fetch session — used by the `form.submit()` wiremock test.
/// Mirrors `tests/form_submit.rs::open_session`.
fn shared_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-tests/0.0.1")
            .redirect(reqwest::redirect::Policy::limited(20))
            .build()
            .expect("client builds"),
    )
}

fn open_session_with_fetch(html: &str, url: Url) -> JsSession {
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    let engine = JsEngine::new_with_fetch(client, rt).expect("engine builds");
    let (sess, _outcome) =
        JsSession::open_on_engine(engine, html, url, ScriptFetchPolicy::default())
            .expect("session opens");
    sess
}

// =====================================================================
// form.action — resolved URL semantics (sibling to anchor.href)
// =====================================================================

#[test]
fn form_action_resolves_relative_attribute_against_document_base() {
    // The V2 agent reproducer: Wikipedia's `#searchform` has an
    // `action="/w/index.php"` attribute; the IDL getter must
    // surface the resolved absolute URL.
    let html = r#"<!doctype html><html><body>
        <form id="f" action="/w/index.php"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').action")
        .expect("eval action");
    assert_eq!(out.value, serde_json::json!("https://example.com/w/index.php"));
}

#[test]
fn form_action_returns_absolute_url_unchanged() {
    let html = r#"<!doctype html><html><body>
        <form id="f" action="https://httpbin.org/post"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').action")
        .expect("eval action");
    assert_eq!(out.value, serde_json::json!("https://httpbin.org/post"));
}

#[test]
fn form_action_falls_back_to_document_url_when_attribute_missing() {
    // Per spec: when `action` is missing/empty, use the document URL.
    let html = r#"<!doctype html><html><body>
        <form id="f"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').action")
        .expect("eval action");
    assert_eq!(out.value, serde_json::json!("https://example.com/page"));
}

#[test]
fn form_action_setter_writes_attribute_verbatim() {
    let html = r#"<!doctype html><html><body>
        <form id="f" action="/old"></form>
    </body></html>"#;
    let sess = open_session(html);
    // Set via IDL, read back via getAttribute → should be verbatim.
    let out = sess
        .eval(
            r#"
            const f = document.getElementById('f');
            f.action = "/new-path";
            f.getAttribute('action');
            "#,
        )
        .expect("eval setter");
    assert_eq!(out.value, serde_json::json!("/new-path"));
    // And via IDL → should be resolved.
    let resolved = sess
        .eval("document.getElementById('f').action")
        .expect("eval action after set");
    assert_eq!(resolved.value, serde_json::json!("https://example.com/new-path"));
}

// =====================================================================
// form.method — lowercase normalization
// =====================================================================

#[test]
fn form_method_normalizes_uppercase_post_to_lowercase() {
    let html = r#"<!doctype html><html><body>
        <form id="f" method="POST"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').method")
        .expect("eval method");
    assert_eq!(out.value, serde_json::json!("post"));
}

#[test]
fn form_method_lowercase_get_stays_lowercase() {
    let html = r#"<!doctype html><html><body>
        <form id="f" method="get"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').method")
        .expect("eval method");
    assert_eq!(out.value, serde_json::json!("get"));
}

#[test]
fn form_method_dialog_keyword_returns_dialog() {
    let html = r#"<!doctype html><html><body>
        <form id="f" method="dialog"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').method")
        .expect("eval method");
    assert_eq!(out.value, serde_json::json!("dialog"));
}

#[test]
fn form_method_defaults_to_get_when_missing() {
    let html = r#"<!doctype html><html><body>
        <form id="f"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').method")
        .expect("eval method");
    assert_eq!(out.value, serde_json::json!("get"));
}

#[test]
fn form_method_defaults_to_get_when_attribute_garbage() {
    // Per spec: invalid keyword → "missing value default" = "get".
    let html = r#"<!doctype html><html><body>
        <form id="f" method="bogus-method"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').method")
        .expect("eval method");
    assert_eq!(out.value, serde_json::json!("get"));
}

#[test]
fn form_method_setter_writes_attribute_verbatim() {
    let html = r#"<!doctype html><html><body>
        <form id="f" method="get"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const f = document.getElementById('f');
            f.method = "POST";
            // Attribute writes verbatim per spec — normalization is on read.
            [f.getAttribute('method'), f.method];
            "#,
        )
        .expect("eval setter");
    assert_eq!(
        out.value,
        serde_json::json!(["POST", "post"]),
        "setter writes raw, getter normalizes lowercase"
    );
}

// =====================================================================
// form.enctype + form.encoding — default + normalization
// =====================================================================

#[test]
fn form_enctype_defaults_to_urlencoded_when_attribute_missing() {
    let html = r#"<!doctype html><html><body>
        <form id="f"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').enctype")
        .expect("eval enctype");
    assert_eq!(
        out.value,
        serde_json::json!("application/x-www-form-urlencoded")
    );
}

#[test]
fn form_enctype_recognizes_multipart() {
    let html = r#"<!doctype html><html><body>
        <form id="f" enctype="multipart/form-data"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').enctype")
        .expect("eval enctype");
    assert_eq!(out.value, serde_json::json!("multipart/form-data"));
}

#[test]
fn form_enctype_recognizes_text_plain() {
    let html = r#"<!doctype html><html><body>
        <form id="f" enctype="text/plain"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').enctype")
        .expect("eval enctype");
    assert_eq!(out.value, serde_json::json!("text/plain"));
}

#[test]
fn form_enctype_invalid_falls_back_to_urlencoded() {
    let html = r#"<!doctype html><html><body>
        <form id="f" enctype="application/json"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').enctype")
        .expect("eval enctype");
    assert_eq!(
        out.value,
        serde_json::json!("application/x-www-form-urlencoded"),
        "non-spec enctype falls back to urlencoded default"
    );
}

#[test]
fn form_encoding_is_alias_for_enctype() {
    let html = r#"<!doctype html><html><body>
        <form id="f" enctype="multipart/form-data"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const f = document.getElementById('f');
            [f.enctype, f.encoding];
            "#,
        )
        .expect("eval enctype + encoding");
    assert_eq!(
        out.value,
        serde_json::json!(["multipart/form-data", "multipart/form-data"]),
        "encoding is a spec alias for enctype"
    );
}

// =====================================================================
// form.name + form.target — straight attribute reflections
// =====================================================================

#[test]
fn form_name_reflects_name_attribute() {
    let html = r#"<!doctype html><html><body>
        <form id="f" name="loginForm"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').name")
        .expect("eval name");
    assert_eq!(out.value, serde_json::json!("loginForm"));
}

#[test]
fn form_target_reflects_target_attribute() {
    let html = r#"<!doctype html><html><body>
        <form id="f" target="_blank"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').target")
        .expect("eval target");
    assert_eq!(out.value, serde_json::json!("_blank"));
}

#[test]
fn form_accept_charset_reflects_kebab_attribute() {
    let html = r#"<!doctype html><html><body>
        <form id="f" accept-charset="UTF-8"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').acceptCharset")
        .expect("eval acceptCharset");
    assert_eq!(out.value, serde_json::json!("UTF-8"));
}

#[test]
fn form_autocomplete_defaults_to_on_when_absent() {
    let html = r#"<!doctype html><html><body>
        <form id="f"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').autocomplete")
        .expect("eval autocomplete");
    assert_eq!(out.value, serde_json::json!("on"));
}

#[test]
fn form_autocomplete_off_when_set() {
    let html = r#"<!doctype html><html><body>
        <form id="f" autocomplete="off"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').autocomplete")
        .expect("eval autocomplete");
    assert_eq!(out.value, serde_json::json!("off"));
}

#[test]
fn form_no_validate_boolean_reflection() {
    let html = r#"<!doctype html><html><body>
        <form id="f1"></form>
        <form id="f2" novalidate></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            [
                document.getElementById('f1').noValidate,
                document.getElementById('f2').noValidate
            ];
            "#,
        )
        .expect("eval noValidate");
    assert_eq!(out.value, serde_json::json!([false, true]));
}

// =====================================================================
// form.length + form.elements
// =====================================================================

#[test]
fn form_length_counts_listed_controls() {
    let html = r#"<!doctype html><html><body>
        <form id="f">
            <input type="text" name="a">
            <input type="text" name="b">
            <select name="c"><option>x</option></select>
            <textarea name="d"></textarea>
            <button type="submit">Go</button>
            <!-- divs / spans should not count -->
            <div>not a control</div>
            <p>nor this</p>
        </form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').length")
        .expect("eval length");
    // 2 inputs + 1 select + 1 textarea + 1 button = 5
    assert_eq!(out.value, serde_json::json!(5));
}

#[test]
fn form_length_zero_for_empty_form() {
    let html = r#"<!doctype html><html><body>
        <form id="f"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').length")
        .expect("eval length");
    assert_eq!(out.value, serde_json::json!(0));
}

#[test]
fn form_elements_returns_indexed_array_of_controls() {
    // Indexed access (form.elements[0], etc.) — the agent idiom.
    let html = r#"<!doctype html><html><body>
        <form id="f">
            <input type="text" name="custname">
            <input type="email" name="custemail">
            <textarea name="comments"></textarea>
        </form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const f = document.getElementById('f');
            const els = f.elements;
            [
                els.length,
                els[0].getAttribute('name'),
                els[1].getAttribute('name'),
                els[2].tagName,
            ];
            "#,
        )
        .expect("eval elements[]");
    assert_eq!(
        out.value,
        serde_json::json!([3, "custname", "custemail", "TEXTAREA"]),
        "indexed access to form.elements"
    );
}

#[test]
fn form_elements_returns_empty_for_non_form_tag() {
    // Per spec gating: only `<form>` exposes the IDL — other tags
    // return the missing-value default.
    let html = r#"<!doctype html><html><body>
        <div id="d"><input type="text"></div>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const d = document.getElementById('d');
            [d.length, d.elements.length];
            "#,
        )
        .expect("eval non-form");
    assert_eq!(out.value, serde_json::json!([0, 0]));
}

// =====================================================================
// form.submit() — issues HTTP request WITHOUT firing submit event
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn form_submit_method_dispatches_post_via_wiremock() {
    // The bug-of-record: V2 agent findings reported `form.submit()`
    // throws `TypeError: not a function`. This test verifies the
    // JS-side method exists and actually issues a real HTTP POST.
    let server = MockServer::start().await;
    Mock::given(m_method("POST"))
        .and(m_path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/echo", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="custname" value="Jane Doe">
            <input type="text" name="comments" value="hello">
        </form>
        </body></html>"#,
    );
    let sess = open_session_with_fetch(&html, Url::parse(&server.uri()).unwrap());

    // Call the IDL method from JS — must not throw.
    let out = sess
        .eval("document.getElementById('f').submit(); 'called';")
        .expect("submit() should not throw");
    assert_eq!(out.value, serde_json::json!("called"));

    // Wiremock should have received exactly one POST.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        reqs.len(),
        1,
        "form.submit() should issue exactly one HTTP request"
    );
    let req = &reqs[0];
    let body = String::from_utf8_lossy(&req.body).into_owned();
    assert!(
        body.contains("custname=Jane+Doe"),
        "POST body missing custname: {body}"
    );
    assert!(
        body.contains("comments=hello"),
        "POST body missing comments: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn form_submit_method_does_not_fire_submit_event() {
    // Per WHATWG HTML §4.10.3 and the jsdom WPT
    // (`HTMLFormElement's submit() does not fire a SubmitEvent`):
    // the programmatic `form.submit()` method bypasses the submit
    // event entirely. We verify by registering a submit listener
    // and asserting it does NOT fire when `form.submit()` is called.
    let server = MockServer::start().await;
    Mock::given(m_method("POST"))
        .and(m_path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/echo", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="a" value="x">
        </form>
        <script>
            globalThis.__submitEventFired = false;
            document.getElementById('f').addEventListener('submit', () => {{
                globalThis.__submitEventFired = true;
            }});
        </script>
        </body></html>"#,
    );
    let sess = open_session_with_fetch(&html, Url::parse(&server.uri()).unwrap());

    sess.eval("document.getElementById('f').submit()")
        .expect("submit ok");

    let out = sess
        .eval("globalThis.__submitEventFired === true")
        .expect("eval submit fired");
    assert_eq!(
        out.value,
        serde_json::json!(false),
        "form.submit() must NOT fire the submit event per spec"
    );

    // ...but the HTTP request must still happen.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
}

#[test]
fn form_submit_method_no_op_when_no_fetch_client() {
    // Without a fetch client (`JsEngine::new()`), `form.submit()`
    // becomes a silent no-op — matches the spec's "no browsing
    // context" branch and avoids `TypeError: not a function`.
    let html = r#"<!doctype html><html><body>
        <form id="f" method="post" action="/post"><input name="a"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval("document.getElementById('f').submit(); 'ok';")
        .expect("submit no-op should not throw");
    assert_eq!(out.value, serde_json::json!("ok"));
}

// =====================================================================
// form.reset() — clears IDL state + fires reset event
// =====================================================================

#[test]
fn form_reset_clears_idl_value_state_on_inputs() {
    // Reset should revert `.value` to the `value` content attribute
    // (the spec's `defaultValue`).
    let html = r#"<!doctype html><html><body>
        <form id="f">
            <input id="i" type="text" name="a" value="default">
        </form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const inp = document.getElementById('i');
            inp.value = "user-typed";
            const before = inp.value;
            document.getElementById('f').reset();
            [before, inp.value];
            "#,
        )
        .expect("eval reset");
    assert_eq!(
        out.value,
        serde_json::json!(["user-typed", "default"]),
        "reset reverts input.value to defaultValue"
    );
}

#[test]
fn form_reset_clears_idl_checked_state_on_checkboxes() {
    let html = r#"<!doctype html><html><body>
        <form id="f">
            <input id="cb" type="checkbox" name="opt" checked>
        </form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const cb = document.getElementById('cb');
            cb.checked = false;
            const before = cb.checked;
            document.getElementById('f').reset();
            [before, cb.checked];
            "#,
        )
        .expect("eval reset");
    assert_eq!(
        out.value,
        serde_json::json!([false, true]),
        "reset reverts checkbox.checked to defaultChecked"
    );
}

#[test]
fn form_reset_fires_reset_event_on_form() {
    let html = r#"<!doctype html><html><body>
        <form id="f"><input name="a"></form>
        <script>
            globalThis.__resetFired = false;
            document.getElementById('f').addEventListener('reset', () => {
                globalThis.__resetFired = true;
            });
        </script>
    </body></html>"#;
    let sess = open_session(html);
    sess.eval("document.getElementById('f').reset()")
        .expect("eval reset");
    let out = sess
        .eval("globalThis.__resetFired")
        .expect("eval __resetFired");
    assert_eq!(out.value, serde_json::json!(true));
}

// =====================================================================
// Non-form tag gating
// =====================================================================

#[test]
fn non_form_tags_return_empty_strings_from_form_idl() {
    // The IDL gate: a `<div>` should not expose form-specific
    // semantics. (Generic attribute getters like `name` are NOT
    // form-specific and are intentionally global on Element.)
    let html = r#"<!doctype html><html><body>
        <div id="d"></div>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const d = document.getElementById('d');
            [d.action, d.method, d.enctype, d.target];
            "#,
        )
        .expect("eval non-form IDL");
    assert_eq!(out.value, serde_json::json!(["", "", "", ""]));
}

// =====================================================================
// document collections (scripts/forms/images/links/anchors)
// =====================================================================

#[test]
fn document_scripts_returns_every_script_in_document_order() {
    let html = r#"<!doctype html><html>
        <head><script id="s1" src="a.js"></script></head>
        <body>
            <script id="s2">/* inline 1 */</script>
            <p>text</p>
            <script id="s3" src="b.js"></script>
        </body>
    </html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const ss = document.scripts;
            [ss.length, ss[0].id, ss[1].id, ss[2].id];
            "#,
        )
        .expect("eval document.scripts");
    // Inline-script execution may run s2 during open; we only care
    // about the post-load collection.
    assert_eq!(out.value, serde_json::json!([3, "s1", "s2", "s3"]));
}

#[test]
fn document_forms_returns_every_form() {
    let html = r#"<!doctype html><html><body>
        <form id="login"></form>
        <p>text</p>
        <form id="search"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const fs = document.forms;
            [fs.length, fs[0].id, fs[1].id];
            "#,
        )
        .expect("eval document.forms");
    assert_eq!(out.value, serde_json::json!([2, "login", "search"]));
}

#[test]
fn document_images_returns_every_img() {
    let html = r#"<!doctype html><html><body>
        <img id="logo" src="logo.png">
        <p>text</p>
        <img id="banner" src="banner.jpg">
        <img id="footer-pic" src="footer.gif">
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const imgs = document.images;
            [imgs.length, imgs[0].id, imgs[1].id, imgs[2].id];
            "#,
        )
        .expect("eval document.images");
    assert_eq!(
        out.value,
        serde_json::json!([3, "logo", "banner", "footer-pic"])
    );
}

#[test]
fn document_links_includes_anchors_with_href() {
    // Per spec: only `<a>` / `<area>` with an `href` attribute
    // count — anchors without `href` (named anchors / nav stubs)
    // are excluded.
    let html = r#"<!doctype html><html><body>
        <a id="l1" href="/one">one</a>
        <a id="l2" href="https://example.com/two">two</a>
        <a id="l3">no-href</a>
        <area id="a1" href="/map" alt="map area">
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const ls = document.links;
            // length, plus collect ids
            const ids = [];
            for (let i = 0; i < ls.length; i++) ids.push(ls[i].id);
            [ls.length, ids];
            "#,
        )
        .expect("eval document.links");
    assert_eq!(
        out.value,
        serde_json::json!([3, ["l1", "l2", "a1"]]),
        "no-href anchors excluded; area with href included"
    );
}

#[test]
fn document_anchors_returns_only_anchors_with_name_attr() {
    // Per (deprecated) spec: `document.anchors` is `<a name="...">` only.
    let html = r#"<!doctype html><html><body>
        <a id="a1" name="top" href="/">top</a>
        <a id="a2" href="/no-name">no name</a>
        <a id="a3" name="bottom">bottom</a>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            const as = document.anchors;
            const ids = [];
            for (let i = 0; i < as.length; i++) ids.push(as[i].id);
            [as.length, ids];
            "#,
        )
        .expect("eval document.anchors");
    assert_eq!(out.value, serde_json::json!([2, ["a1", "a3"]]));
}

#[test]
fn document_collections_are_empty_arrays_when_no_matches() {
    // Common edge case: a page with no scripts / forms / etc.
    // — collections should be empty arrays, not undefined.
    let html = r#"<!doctype html><html><body><p>plain</p></body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            [
                document.scripts.length,
                document.forms.length,
                document.images.length,
                document.links.length,
                document.anchors.length,
            ];
            "#,
        )
        .expect("eval empty collections");
    assert_eq!(out.value, serde_json::json!([0, 0, 0, 0, 0]));
}

#[test]
fn document_collections_yield_real_elements_with_iter() {
    // The scraper idiom: `Array.from(document.forms).map(f => ...)`.
    let html = r#"<!doctype html><html><body>
        <form id="login" action="/login" method="post"></form>
        <form id="search" action="/search" method="get"></form>
    </body></html>"#;
    let sess = open_session(html);
    let out = sess
        .eval(
            r#"
            Array.from(document.forms).map(f => ({
                id: f.id,
                action: f.getAttribute('action'),
                method: f.method,
            }));
            "#,
        )
        .expect("eval Array.from + map");
    assert_eq!(
        out.value,
        serde_json::json!([
            {"id": "login", "action": "/login", "method": "post"},
            {"id": "search", "action": "/search", "method": "get"}
        ])
    );
}
