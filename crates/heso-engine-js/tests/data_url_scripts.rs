//! Integration tests for `<script src="data:text/javascript,...">`
//! handling per RFC 2397. Closes bug-report 01 P1: reddit ships three
//! `data:text/javascript,...` `<script src=...>` tags (window.STICKY_CANARY,
//! window.PRE_PRODUCTION, fetch-wrapping) and they ALL crashed with
//! `send: builder error for url (data:text/javascript,...)` because
//! reqwest only speaks HTTP(S).
//!
//! Now the script-src fetcher short-circuits data URLs through the
//! same RFC-2397 parser the in-JS `fetch()` global uses.

use std::sync::Arc;

use heso_engine_js::{JsEngine, ScriptFetchPolicy};
use url::Url;

fn shared_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-data-url-tests/0.0.1")
            .build()
            .expect("client builds"),
    )
}

fn engine_with_fetch() -> JsEngine {
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    let engine = JsEngine::new_with_fetch(client, rt).expect("engine builds");
    engine.set_base_url(Some(Url::parse("https://reddit.example/").unwrap()));
    engine
}

#[tokio::test(flavor = "multi_thread")]
async fn data_url_text_javascript_executes_inline() {
    // Plain-text payload — percent-decoded if needed.
    let engine = engine_with_fetch();
    let html = r#"<html><body>
        <script src="data:text/javascript,globalThis.__sticky = 'ok'"></script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(html, "globalThis.__sticky", ScriptFetchPolicy::Fetch)
        .expect("eval ok");
    assert_eq!(out.value, "ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn data_url_with_percent_encoded_payload() {
    // `data:text/javascript,globalThis.__x = 1; globalThis.__y = 2`
    // — semicolon must be percent-encoded only if it would otherwise
    // be ambiguous with the mediatype params; here `%3B` and a bare
    // `;` both work because they're after the comma.
    let engine = engine_with_fetch();
    let html = r#"<html><body>
        <script src="data:text/javascript,globalThis.__x%20%3D%201"></script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(html, "globalThis.__x", ScriptFetchPolicy::Fetch)
        .expect("eval ok");
    assert_eq!(out.value, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn data_url_base64_executes_inline() {
    // Base64-encoded `globalThis.__b64 = 'ok'`.
    use base64::Engine as _;
    let src = "globalThis.__b64 = 'ok'";
    let b64 = base64::engine::general_purpose::STANDARD.encode(src.as_bytes());
    let engine = engine_with_fetch();
    let html = format!(
        r#"<html><body>
            <script src="data:text/javascript;base64,{b64}"></script>
        </body></html>"#,
    );
    let out = engine
        .eval_with_html_policy(&html, "globalThis.__b64", ScriptFetchPolicy::Fetch)
        .expect("eval ok");
    assert_eq!(out.value, "ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn reddit_three_inline_runtime_config_scripts_all_run() {
    // The exact bug-report 01 P1 pattern: reddit's three inline runtime-
    // config blobs. We invent a representative shape (set a few globals).
    let engine = engine_with_fetch();
    let html = r#"<html><body>
        <script src="data:text/javascript,globalThis.STICKY_CANARY = 'a'"></script>
        <script src="data:text/javascript,globalThis.PRE_PRODUCTION = 'b'"></script>
        <script src="data:text/javascript,globalThis.WRAPPED_FETCH = 'c'"></script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(
            html,
            "[globalThis.STICKY_CANARY, globalThis.PRE_PRODUCTION, globalThis.WRAPPED_FETCH]",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!(["a", "b", "c"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn data_url_script_does_not_make_an_http_request() {
    // Sanity: no base_url, no live server. If the fetch path tried to
    // route data: through reqwest, we'd get "send: builder error".
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    let engine = JsEngine::new_with_fetch(client, rt).expect("engine builds");
    // Deliberately do NOT call set_base_url — that ensures relative
    // resolution would fail. data: must work without a base.
    let html = r#"<html><body>
        <script src="data:text/javascript,globalThis.__nb = 'no-base'"></script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(html, "globalThis.__nb", ScriptFetchPolicy::Fetch)
        .expect("eval ok");
    assert_eq!(out.value, "no-base");
}
