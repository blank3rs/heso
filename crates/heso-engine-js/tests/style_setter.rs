//! Integration tests for the `element.style = "css string"` setter
//! (CSSOM §6). Closes bug-report 03 P1 and bug-report 01 P1: docs.rs
//! `menu.js` and reuters.com DataDome captcha agent both do
//! `el.style = "display:none;position:fixed;..."` and crash with
//! `no setter for property`.

use heso_engine_js::JsSession;
use url::Url;

fn page(html: &str) -> JsSession {
    let url = Url::parse("https://example.com/").unwrap();
    JsSession::open(html, url).expect("open page").0
}

#[test]
fn assigning_a_string_to_style_writes_the_style_attribute() {
    let s = page("<html><body><div id='x'>x</div></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const el = document.getElementById('x');
            el.style = "color: red; display: none";
            el.getAttribute('style')
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "color: red; display: none");
}

#[test]
fn style_string_setter_round_trips_through_proxy_reads() {
    let s = page("<html><body><div id='x'>x</div></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const el = document.getElementById('x');
            el.style = "color: red";
            [el.style.color, el.style.cssText]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], "red");
    assert_eq!(out.value[1], "color: red");
}

#[test]
fn style_setter_does_not_throw_no_setter() {
    // Bug-report 03 P1 repro: `el.style = "..."` was producing
    // `no setter for property`.
    let s = page("<html><body><div id='x'>x</div></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            try {
                const el = document.getElementById('x');
                el.style = "display:none;position:fixed;top:0";
                "ok"
            } catch (e) {
                e.message || String(e)
            }
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "ok");
}

#[test]
fn style_setter_empty_string_clears() {
    let s = page("<html><body><div id='x' style='color: red'>x</div></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const el = document.getElementById('x');
            el.style = "";
            el.getAttribute('style')
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "");
}

#[test]
fn style_setter_overwrites_previous_inline_styles() {
    let s = page("<html><body><div id='x' style='color: red; padding: 8px'>x</div></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const el = document.getElementById('x');
            el.style = "display: block";
            el.getAttribute('style')
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "display: block");
}

#[test]
fn style_setter_null_assignment_clears() {
    let s = page("<html><body><div id='x' style='color: red'>x</div></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const el = document.getElementById('x');
            el.style = null;
            el.getAttribute('style')
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "");
}

#[test]
fn docs_rs_menu_js_pattern_does_not_throw() {
    // The exact pattern from bug-report 01 P1: docs.rs/serde menu.js
    // does `backdrop.style = "display:none;position:fixed;..."`.
    let s = page("<html><body><div id='backdrop'>x</div></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const backdrop = document.getElementById('backdrop');
            backdrop.style = "display:none;position:fixed;top:0;left:0;right:0;bottom:0;background:rgba(0,0,0,0.5);z-index:1000";
            "ok"
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "ok");
}
