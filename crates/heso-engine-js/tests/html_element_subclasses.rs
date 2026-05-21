//! Integration tests for the HTMLElement subclass family installed by
//! `crate::custom_elements`. Closes bug-report 03 P1 / bug-report 01
//! P0 cluster.
//!
//! Real-world callers gate hydration on `instanceof HTMLXxxElement`
//! checks. Linear's webpack runtime crashes with
//! `HTMLScriptElement is not defined`; docs.rs hits
//! `HTMLLinkElement is not defined`; cloudflare's hero-video
//! hydration hits `HTMLVideoElement`. Each constructor must
//!   (a) exist on globalThis,
//!   (b) throw `Illegal constructor` on direct `new`,
//!   (c) return `true` for the matching DOM element.

use heso_engine_js::{JsEngine, JsSession};
use url::Url;

fn engine() -> JsEngine {
    JsEngine::new().expect("engine new")
}

fn page(html: &str) -> JsSession {
    let url = Url::parse("https://example.com/").unwrap();
    JsSession::open(html, url).expect("open page").0
}

// ===== Global constructors exist + throw on direct construction =====

#[test]
fn html_div_element_constructor_exists() {
    let out = engine().eval("typeof HTMLDivElement").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn html_script_element_constructor_exists() {
    let out = engine().eval("typeof HTMLScriptElement").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn html_anchor_element_constructor_exists() {
    let out = engine().eval("typeof HTMLAnchorElement").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn html_input_element_constructor_exists() {
    let out = engine().eval("typeof HTMLInputElement").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn html_form_element_constructor_exists() {
    let out = engine().eval("typeof HTMLFormElement").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn html_image_element_constructor_exists() {
    let out = engine().eval("typeof HTMLImageElement").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn html_button_element_constructor_exists() {
    let out = engine().eval("typeof HTMLButtonElement").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn html_link_element_constructor_exists() {
    // bug-report 01 P0 cluster repro: docs.rs/serde
    let out = engine().eval("typeof HTMLLinkElement").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn html_video_element_constructor_exists() {
    // bug-report 01 P0 cluster repro: cloudflare.com
    let out = engine().eval("typeof HTMLVideoElement").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn html_subclasses_throw_illegal_constructor() {
    let err = engine()
        .eval("new HTMLDivElement()")
        .expect_err("should throw");
    assert!(format!("{err:?}").contains("Illegal constructor"));
}

// ===== instanceof checks against real DOM elements =====

#[test]
fn div_element_is_instance_of_html_div_element() {
    let s = page("<html><body><div id='x'>x</div></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const el = document.getElementById('x');
            el instanceof HTMLDivElement
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, true);
}

#[test]
fn anchor_is_instance_of_html_anchor_element() {
    let s = page("<html><body><a id='x' href='/'>link</a></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const el = document.getElementById('x');
            el instanceof HTMLAnchorElement
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, true);
}

#[test]
fn input_is_instance_of_html_input_element() {
    let s = page("<html><body><input id='x'></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const el = document.getElementById('x');
            el instanceof HTMLInputElement
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, true);
}

#[test]
fn img_is_instance_of_html_image_element() {
    let s = page("<html><body><img id='x' src='/p.png'></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const el = document.getElementById('x');
            el instanceof HTMLImageElement
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, true);
}

#[test]
fn script_is_instance_of_html_script_element() {
    // Bug-report 03 P1 canonical repro: linear.app webpack chunk
    // does `instanceof HTMLScriptElement` to detect its own bundle.
    let s = page("<html><body><script id='x'>x=1</script></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const el = document.getElementById('x');
            el instanceof HTMLScriptElement
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, true);
}

#[test]
fn html_div_does_not_match_anchor_subclass() {
    // Discrimination: a div is NOT an HTMLAnchorElement.
    let s = page("<html><body><div id='x'>x</div></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const el = document.getElementById('x');
            el instanceof HTMLAnchorElement
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, false);
}

#[test]
fn html_heading_matches_any_h1_through_h6() {
    let s = page("<html><body><h1 id='a'>a</h1><h6 id='b'>b</h6></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const a = document.getElementById('a');
            const b = document.getElementById('b');
            [a instanceof HTMLHeadingElement, b instanceof HTMLHeadingElement]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([true, true]));
}

#[test]
fn html_media_element_matches_audio_and_video() {
    let s = page("<html><body><video id='v'></video><audio id='a'></audio></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const v = document.getElementById('v');
            const a = document.getElementById('a');
            [v instanceof HTMLMediaElement, a instanceof HTMLMediaElement]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([true, true]));
}

#[test]
fn node_list_is_a_function() {
    let out = engine().eval("typeof NodeList").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn html_collection_is_a_function() {
    let out = engine().eval("typeof HTMLCollection").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn query_selector_all_result_is_instance_of_node_list() {
    let s = page("<html><body><p>a</p><p>b</p></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const list = document.querySelectorAll('p');
            list instanceof NodeList
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, true);
}
