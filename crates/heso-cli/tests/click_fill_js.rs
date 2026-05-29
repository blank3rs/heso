//! `heso click --js` / `heso fill --js` — resolve refs against the
//! HYDRATED DOM and dispatch against the live session. The default
//! (no-`--js`) path resolves against the static parse; `--js` opens a
//! fetch+cookie session, runs inline + linked scripts, re-extracts the
//! action graph from the post-hydration document, and dispatches there.
//!
//! Coverage:
//!   - A control that exists only after an inline script runs is
//!     unreachable without `--js` (`ref_not_found`) and reachable with
//!     it (`matched: true`, post-snapshot reflects the handler mutation).
//!   - `fill --js` sets the value and attaches a post-fill snapshot.
//!   - A ref present statically but renumbered/removed by hydration
//!     returns the distinct `ref_needs_js` code so the agent retries the
//!     matched-pair (`read --js-fetch` + `click --js`).
//!   - `--js` reuses the body already fetched for resolution — one HTTP
//!     hit for the page, no double-fetch.
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

/// A page whose only interactive control is appended by an inline
/// script at parse time. Statically the action graph is empty, so the
/// element exists only after hydration. The appended button's handler
/// rewrites `#out` so a post-click snapshot can prove the live DOM
/// dispatch landed.
const POST_HYDRATION_BUTTON_PAGE: &str = r#"<!doctype html><html><head><title>Hydrated</title></head><body>
    <p id="out">initial</p>
    <div id="root"></div>
    <script>
        const b = document.createElement('button');
        b.id = 'go';
        b.textContent = 'Go';
        b.addEventListener('click', function () {
            document.getElementById('out').textContent = 'mutated by handler';
        });
        document.getElementById('root').appendChild(b);
    </script>
</body></html>"#;

/// `click @e0` without `--js` resolves against the static parse, which
/// has no interactive elements — the ref names nothing, so the verb
/// fails with `ref_not_found`.
#[tokio::test(flavor = "multi_thread")]
async fn click_post_hydration_ref_without_js_is_ref_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(POST_HYDRATION_BUTTON_PAGE))
        .mount(&server)
        .await;

    let out = Command::new(heso_bin())
        .args(["click", &server.uri(), "@e0"])
        .output()
        .expect("spawn heso click");
    assert!(
        !out.status.success(),
        "static click of a post-hydration-only ref must fail; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let body = parse_body(&out);
    assert_eq!(body["ok"], serde_json::json!(false), "body={body}");
    assert_eq!(
        body["error"]["code"], serde_json::json!("ref_not_found"),
        "body={body}"
    );
}

/// `click --js @e0` resolves against the hydrated DOM, where the
/// script-appended button is `@e0`. The click matches and the
/// handler's `#out` mutation surfaces in the post-click snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn click_js_resolves_post_hydration_ref_and_snapshots_mutation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(POST_HYDRATION_BUTTON_PAGE))
        .mount(&server)
        .await;

    let out = Command::new(heso_bin())
        .args(["click", "--js", &server.uri(), "@e0"])
        .output()
        .expect("spawn heso click --js");
    assert!(
        out.status.success(),
        "click --js failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let body = parse_body(&out);
    assert_eq!(body["ok"], serde_json::json!(true), "body={body}");
    assert_eq!(body["op"], serde_json::json!("click"));
    assert_eq!(body["result"], serde_json::json!(true));
    assert_eq!(body["ref"], serde_json::json!("@e0"));

    // The button has no href, so no navigation — the post-click
    // snapshot must carry the handler's DOM mutation.
    assert!(
        body.get("navigated").is_none(),
        "button click must not navigate; body={body}"
    );
    let text = body["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected post-click `text`; body={body}"));
    assert!(
        text.contains("mutated by handler"),
        "post-click text must reflect the handler mutation, got: {text:?}"
    );
    assert!(
        !text.contains("initial"),
        "post-click text must not still carry the pre-click value, got: {text:?}"
    );
}

/// `fill --js @e0 <value>` against an input that exists only after
/// hydration must set the input's value and attach a post-fill
/// snapshot (the same `text`/`tree`/`content_hash` fields a
/// non-navigating click surfaces) so an agent can see the live DOM.
#[tokio::test(flavor = "multi_thread")]
async fn fill_js_sets_value_and_attaches_post_fill_snapshot() {
    let server = MockServer::start().await;
    // The input is appended by an inline script; its `input` listener
    // mirrors the typed value into `#echo`, so the post-fill snapshot
    // proves the live `input` event fired against the hydrated DOM.
    let page = r#"<!doctype html><html><head><title>Form</title></head><body>
        <p id="echo">empty</p>
        <div id="root"></div>
        <script>
            const inp = document.createElement('input');
            inp.id = 'name';
            inp.type = 'text';
            inp.addEventListener('input', function (e) {
                document.getElementById('echo').textContent = 'typed: ' + e.target.value;
            });
            document.getElementById('root').appendChild(inp);
        </script>
    </body></html>"#;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(page))
        .mount(&server)
        .await;

    let out = Command::new(heso_bin())
        .args(["fill", "--js", &server.uri(), "@e0", "Ada Lovelace"])
        .output()
        .expect("spawn heso fill --js");
    assert!(
        out.status.success(),
        "fill --js failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let body = parse_body(&out);
    assert_eq!(body["ok"], serde_json::json!(true), "body={body}");
    assert_eq!(body["op"], serde_json::json!("fill"));
    assert_eq!(body["result"], serde_json::json!(true));
    // `value` echoes exactly what the verb wrote.
    assert_eq!(body["value"], serde_json::json!("Ada Lovelace"));

    // Post-fill snapshot present and reflects the `input` listener's
    // mutation.
    let text = body["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected post-fill `text`; body={body}"));
    assert!(
        text.contains("typed: Ada Lovelace"),
        "post-fill text must reflect the input listener mutation, got: {text:?}"
    );
    let hash = body["content_hash"]
        .as_str()
        .unwrap_or_else(|| panic!("expected post-fill `content_hash`; body={body}"));
    assert!(
        hash.starts_with("blake3:"),
        "content_hash should be a blake3 digest, got: {hash}"
    );
    assert_eq!(body["tree"]["title"], serde_json::json!("Form"));
}

/// A ref that resolves in the static parse but NOT in the hydrated DOM
/// (the inline script removed the element) is a stale ref from a
/// non-`--js` read. `click --js` reports the distinct `ref_needs_js`
/// code rather than the generic `selector_not_matched`, so the agent
/// knows the matched pair broke.
#[tokio::test(flavor = "multi_thread")]
async fn click_js_static_only_ref_is_ref_needs_js() {
    let server = MockServer::start().await;
    // Static parse: one button (`@e0`). Hydration removes it, so the
    // post-hydration action graph is empty.
    let page = r#"<!doctype html><html><head><title>Vanishing</title></head><body>
        <button id="gone">Static Button</button>
        <script>
            const b = document.getElementById('gone');
            b.parentNode.removeChild(b);
        </script>
    </body></html>"#;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(page))
        .mount(&server)
        .await;

    let out = Command::new(heso_bin())
        .args(["click", "--js", &server.uri(), "@e0"])
        .output()
        .expect("spawn heso click --js");
    assert!(
        !out.status.success(),
        "click --js of a static-only ref must fail; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let body = parse_body(&out);
    assert_eq!(body["ok"], serde_json::json!(false), "body={body}");
    assert_eq!(
        body["error"]["code"], serde_json::json!("ref_needs_js"),
        "static-only ref under --js must be ref_needs_js, not ref_not_found; body={body}"
    );
}

/// `--js` reuses the body already fetched to resolve the target, so
/// the page is fetched exactly once — the static path's second
/// `fetch_text` round-trip is skipped on the `--js` path.
#[tokio::test(flavor = "multi_thread")]
async fn click_js_fetches_the_page_only_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(POST_HYDRATION_BUTTON_PAGE))
        .mount(&server)
        .await;

    let out = Command::new(heso_bin())
        .args(["click", "--js", &server.uri(), "@e0"])
        .output()
        .expect("spawn heso click --js");
    assert!(
        out.status.success(),
        "click --js failed: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );

    let hits = server.received_requests().await.expect("recorded requests");
    let page_hits = hits
        .iter()
        .filter(|r| r.url.path() == "/")
        .count();
    assert_eq!(
        page_hits, 1,
        "--js must fetch the page exactly once, saw {page_hits}"
    );
}
