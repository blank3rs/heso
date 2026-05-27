//! Regression tests for **Bug A** — `heso click` on an `<a href>`
//! now follows the anchor and surfaces the destination page on the
//! response body. See `bug-reports/02-verb-ergonomics.md` for the
//! original repro.
//!
//! Pre-fix behavior:
//!   `heso click https://en.wikipedia.org/wiki/JavaScript --selector 'a[title="Brendan Eich"]'`
//!   returned `{value: true}` and the response URL still pointed at
//!   the origin page. Every multi-step navigation chain (HN, GitHub,
//!   Stripe, lobste.rs) hit this footgun — the agent had to manually
//!   concat the element's href against the page URL to know where
//!   it landed.
//!
//! Post-fix:
//!   - `<a href>` clicks resolve the href against the page URL
//!     (relative-URL semantics per WHATWG URL §5.2), fetch the
//!     destination, and surface `navigated: true`,
//!     `navigated_to: <url>`, plus the destination's `title`,
//!     `description`, `tree`, `actions`, `metadata`, and
//!     `http_status` on the response body.
//!   - Non-anchor clicks (`<button>`, form-submit buttons) keep
//!     their pre-fix shape — they don't navigate by definition.
//!   - `<a href="#frag">` (in-page anchor) is recognized as
//!     non-navigational and skipped.
//!   - `javascript:` / `mailto:` / `tel:` / `data:` pseudo-URLs
//!     are likewise skipped.
//!
//! All tests drive the release binary against a hermetic wiremock
//! server.

use std::path::PathBuf;
use std::process::Command;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn parse_body(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout not JSON: {e}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    })
}

/// `heso click @anchor-ref` against an `<a href="/landing">` must
/// follow the href, fetch the destination page, and return a body
/// that reflects the *destination's* title / description / tree /
/// actions. Pre-fix the body's `value: true` was the only signal
/// the click matched; the response URL still pointed at the origin
/// page.
#[tokio::test(flavor = "multi_thread")]
async fn click_anchor_follows_href_and_reports_destination() {
    let server = MockServer::start().await;
    // Origin page has one link to `/landing`.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><head><title>Origin</title></head><body>
                <a href="/landing">Landing</a>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    // Destination page has its own unique title + h1 so we can prove
    // the response really came from the destination, not the origin.
    Mock::given(method("GET"))
        .and(path("/landing"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><head><title>Landed Successfully</title></head><body>
                <h1>You Have Arrived</h1>
                <p>This is the destination page.</p>
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    let out = Command::new(heso_bin())
        .args(["click", &server.uri(), "--text", "Landing"])
        .output()
        .expect("spawn heso click");
    assert!(
        out.status.success(),
        "heso click failed: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let body = parse_body(&out);

    // Click's `value` is null in the unified envelope; the engine's
    // matched-flag rides on `result` and is folded into `ok`.
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["op"], serde_json::json!("click"));
    assert_eq!(body["value"], serde_json::Value::Null);
    assert_eq!(body["result"], serde_json::json!(true));

    // Bug A fix: navigation actually happened.
    assert_eq!(body["navigated"], serde_json::json!(true), "body={body}");
    let nav_to = body["navigated_to"]
        .as_str()
        .expect("navigated_to should be a string");
    assert!(
        nav_to.ends_with("/landing"),
        "navigated_to should point at /landing, got: {nav_to}"
    );

    // The destination page's title surfaces on the response, not the
    // origin's "Origin" title.
    assert_eq!(
        body["title"], serde_json::json!("Landed Successfully"),
        "body should reflect destination title, got: {}",
        body["title"]
    );
    // The destination's h1 lives in `tree.title` as well — pin both
    // so a regression that only swapped one field is caught.
    assert_eq!(
        body["tree"]["title"], serde_json::json!("Landed Successfully"),
    );
    // `actions` on the response reflect the destination page (which
    // has no interactive elements — empty array) rather than the
    // origin's `<a>` (which would be one entry).
    let actions = body["actions"]
        .as_array()
        .expect("actions should be an array on the destination");
    assert!(
        actions.is_empty(),
        "destination has no interactive elements, got {actions:?}"
    );
}

/// Relative hrefs must resolve against the page URL, not the bare
/// host. `<a href="page2">` on `http://srv/` should fetch
/// `http://srv/page2`.
#[tokio::test(flavor = "multi_thread")]
async fn click_anchor_resolves_relative_href() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            // Use a relative href without a leading slash. The page
            // URL is the server root, so resolution lands at
            // `/page2`.
            r#"<!doctype html><html><body>
                <a href="page2">Relative Link</a>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/page2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><head><title>Page Two</title></head><body><h1>Two</h1></body></html>"#,
        ))
        .mount(&server)
        .await;

    let out = Command::new(heso_bin())
        .args(["click", &server.uri(), "--text", "Relative Link"])
        .output()
        .expect("spawn heso click");
    assert!(out.status.success());
    let body = parse_body(&out);
    assert_eq!(body["navigated"], serde_json::json!(true));
    let nav_to = body["navigated_to"].as_str().unwrap();
    assert!(
        nav_to.ends_with("/page2"),
        "expected relative href to resolve to /page2, got {nav_to}"
    );
    assert_eq!(body["title"], serde_json::json!("Page Two"));
}

/// Non-anchor clicks must NOT mutate the response shape — clicking a
/// `<button>` with no href has no destination to follow, so the
/// `navigated`/`navigated_to`/destination-title fields should be
/// absent. (Form submission is `heso submit`'s job — see ADR
/// commentary.)
#[tokio::test(flavor = "multi_thread")]
async fn click_non_anchor_does_not_navigate() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><head><title>Origin</title></head><body>
                <button id="b">Click Me</button>
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    let out = Command::new(heso_bin())
        .args(["click", &server.uri(), "--text", "Click Me"])
        .output()
        .expect("spawn heso click on a button");
    assert!(out.status.success());
    let body = parse_body(&out);
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["value"], serde_json::Value::Null);
    assert_eq!(body["result"], serde_json::json!(true));
    // The augmented destination fields should NOT be present.
    assert!(
        body.get("navigated").is_none(),
        "button click must not surface a `navigated` field; got body={body}"
    );
    assert!(
        body.get("navigated_to").is_none(),
        "button click must not surface a `navigated_to` field; got body={body}"
    );
}

/// `<a href="#section">` (in-page anchor / fragment) is not a real
/// navigation — we should NOT issue a follow-up fetch for it.
#[tokio::test(flavor = "multi_thread")]
async fn click_fragment_only_anchor_does_not_navigate() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            // Use `r##"..."##` so the `"#` in `href="#top"` doesn't
            // terminate the raw string early.
            r##"<!doctype html><html><body>
                <a href="#top">Back to top</a>
            </body></html>"##,
        ))
        .mount(&server)
        .await;

    let out = Command::new(heso_bin())
        .args(["click", &server.uri(), "--text", "Back to top"])
        .output()
        .expect("spawn heso click on fragment anchor");
    assert!(out.status.success());
    let body = parse_body(&out);
    assert_eq!(body["ok"], serde_json::json!(true));
    assert!(
        body.get("navigated").is_none(),
        "fragment-only anchor must not navigate; got body={body}"
    );
}
