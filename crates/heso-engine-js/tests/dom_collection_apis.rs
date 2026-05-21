//! Integration tests for the DOM collection APIs added to close
//! bug-report 03 cluster P0:
//!
//! - `document.getElementsByClassName(className)`
//! - `document.getElementsByName(name)`
//! - `element.getElementsByClassName(className)`
//! - `element.getElementsByTagName(name)`
//!
//! These are gating fixes for ~10 real-world sites whose first inline
//! script crashes on the missing function (HN's `hn.js`, Sphinx-
//! generated python docs, kubernetes.io, anthropic.com, etc.).

use heso_engine_js::JsSession;
use url::Url;

fn page(html: &str) -> JsSession {
    let url = Url::parse("https://example.com/").unwrap();
    JsSession::open(html, url).expect("open page").0
}

// ===== getElementsByClassName ==========================================

#[test]
fn get_elements_by_class_name_is_a_function() {
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval("typeof document.getElementsByClassName")
        .expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn get_elements_by_class_name_returns_matching_elements() {
    let s = page(
        r#"<html><body>
            <p class="hi">a</p>
            <p>b</p>
            <p class="hi target">c</p>
        </body></html>"#,
    );
    let out = s
        .engine()
        .eval(
            r#"
            const list = document.getElementsByClassName('hi');
            [list.length, list[0].textContent, list[1].textContent]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], 2);
    assert_eq!(out.value[1], "a");
    assert_eq!(out.value[2], "c");
}

#[test]
fn get_elements_by_class_name_intersects_when_multiple_tokens() {
    // Per WHATWG DOM spec: "the document's elements which have all
    // the classes that match" — multiple tokens => intersection.
    let s = page(
        r#"<html><body>
            <p class="a b">match</p>
            <p class="a">a-only</p>
            <p class="b">b-only</p>
            <p class="a b c">also-match</p>
        </body></html>"#,
    );
    let out = s
        .engine()
        .eval(
            r#"
            const list = document.getElementsByClassName('a b');
            [list.length, list[0].textContent, list[1].textContent]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], 2);
    assert_eq!(out.value[1], "match");
    assert_eq!(out.value[2], "also-match");
}

#[test]
fn get_elements_by_class_name_empty_returns_empty() {
    let s = page("<html><body><p class='x'>a</p></body></html>");
    let out = s
        .engine()
        .eval("document.getElementsByClassName('').length")
        .expect("eval");
    assert_eq!(out.value, 0);
}

#[test]
fn get_elements_by_class_name_no_match_returns_empty() {
    let s = page("<html><body><p class='x'>a</p></body></html>");
    let out = s
        .engine()
        .eval("document.getElementsByClassName('y').length")
        .expect("eval");
    assert_eq!(out.value, 0);
}

#[test]
fn element_get_elements_by_class_name_scopes_to_subtree() {
    let s = page(
        r#"<html><body>
            <p class="hi">outside</p>
            <div id="root">
                <p class="hi">inside</p>
                <span class="hi">inside-too</span>
            </div>
        </body></html>"#,
    );
    let out = s
        .engine()
        .eval(
            r#"
            const root = document.getElementById('root');
            const inside = root.getElementsByClassName('hi');
            [inside.length, inside[0].textContent, inside[1].textContent]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], 2);
    assert_eq!(out.value[1], "inside");
    assert_eq!(out.value[2], "inside-too");
}

// ===== HN regression (bug-report 03 cluster P0 canonical repro) =======

#[test]
fn hn_byclass_pattern_does_not_throw() {
    // hn.js does roughly:
    //     function byClass(el, cl) { return el.getElementsByClassName(cl); }
    //     byClass(document, 'comment')[0].innerHTML = '...'
    // Before this fix the first call threw `not a function` and the
    // rest of the script never ran.
    let s = page(
        r#"<html><body>
            <div class="comment">first</div>
            <div class="comment">second</div>
        </body></html>"#,
    );
    let out = s
        .engine()
        .eval(
            r#"
            function byClass(el, cl) { return el.getElementsByClassName(cl); }
            const list = byClass(document, 'comment');
            list.length
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, 2);
}

// ===== getElementsByName ===============================================

#[test]
fn get_elements_by_name_returns_matching_inputs() {
    let s = page(
        r#"<html><body>
            <input name="user">
            <input name="other">
            <input name="user">
        </body></html>"#,
    );
    let out = s
        .engine()
        .eval(
            r#"
            const list = document.getElementsByName('user');
            list.length
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, 2);
}

// ===== element.getElementsByTagName ====================================

#[test]
fn element_get_elements_by_tag_name_scopes_to_subtree() {
    let s = page(
        r#"<html><body>
            <p>outside</p>
            <div id="root">
                <p>inside1</p>
                <p>inside2</p>
            </div>
        </body></html>"#,
    );
    let out = s
        .engine()
        .eval(
            r#"
            const root = document.getElementById('root');
            const inside = root.getElementsByTagName('p');
            [inside.length, inside[0].textContent, inside[1].textContent]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], 2);
    assert_eq!(out.value[1], "inside1");
    assert_eq!(out.value[2], "inside2");
}
