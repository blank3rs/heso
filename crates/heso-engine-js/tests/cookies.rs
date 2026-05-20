//! End-to-end cookie jar tests.
//!
//! Verifies the load-bearing scenario the cookie-jar agent shipped:
//! HTTP `Set-Cookie` responses populate the same jar that JS
//! `document.cookie` reads/writes against, and the jar persists
//! across `JsSession` calls. Together those properties unblock any
//! login flow (server sends `Set-Cookie`, subsequent `fetch` /
//! `navigate` sends it back).
//!
//! All tests use `wiremock::MockServer` for localhost HTTP exchanges
//! so the workspace `cargo test` stays hermetic.

use heso_engine_fetch::FetchEngine;
use heso_engine_js::{JsEngine, JsSession, ScriptFetchPolicy};
use url::Url;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Build a [`JsEngine`] with a fresh [`FetchEngine`] and shared jar
/// wired in. This is the production wiring — `FetchEngine` owns the
/// jar AND hands the same `Arc` to the engine, so `Set-Cookie`
/// responses arriving via `fetch()` populate the jar AND
/// `document.cookie` reads/writes operate on the exact same store.
fn engine_with_shared_jar() -> (JsEngine, FetchEngine) {
    let fetch_engine = FetchEngine::new().expect("fetch engine builds");
    let client = fetch_engine.client();
    let jar = fetch_engine.cookie_jar();
    let rt = tokio::runtime::Handle::current();
    let engine = JsEngine::new_with_fetch_and_cookies(client, rt, jar).expect("engine builds");
    (engine, fetch_engine)
}

/// Build a [`JsSession`] over the engine with the shared cookie jar.
/// `url` is set as the base so the bridge can scope cookies against
/// it. Returns both so the test can also reach `engine().cookie_jar()`.
fn session_at(html: &str, url: Url) -> (JsSession, FetchEngine) {
    let (engine, fetch_engine) = engine_with_shared_jar();
    let (session, _) =
        JsSession::open_on_engine(engine, html, url, ScriptFetchPolicy::default())
            .expect("open");
    (session, fetch_engine)
}

// ===== Test 1: response Set-Cookie surfaces in document.cookie =====

#[tokio::test(flavor = "multi_thread")]
async fn document_cookie_getter_returns_response_set_cookies() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Set-Cookie", "foo=bar; Path=/")
                .set_body_string("<html><body>hi</body></html>"),
        )
        .mount(&server)
        .await;

    let (engine, _fetch_engine) = engine_with_shared_jar();
    let server_url = Url::parse(&server.uri()).unwrap();

    // Fetch the page from JS so reqwest writes `Set-Cookie` into the
    // shared jar. The fetch must complete before we read
    // `document.cookie`, so we drive it via `.then` and let the
    // engine's pending-fetch drain run.
    engine.set_base_url(Some(server_url.clone()));
    let fetch_js = format!(
        r#"fetch({url:?}).then(r => r.text()).then(_ => {{ globalThis.__done = true; }});"#,
        url = server.uri()
    );
    engine.eval(&fetch_js).expect("schedule fetch");

    // Now install a document scoped to the server origin so the
    // `document.cookie` getter scopes against the same host the
    // Set-Cookie was scoped to.
    let html = "<html><body></body></html>";
    let doc = heso_engine_js::Document::from_html(html);
    engine
        .install_document(doc, ScriptFetchPolicy::default())
        .expect("install");

    let out = engine.eval("document.cookie").expect("eval cookie");
    assert_eq!(
        out.value,
        serde_json::json!("foo=bar"),
        "Set-Cookie response should surface in document.cookie"
    );
}

// ===== Test 2: JS-set cookie sent on next fetch =====

#[tokio::test(flavor = "multi_thread")]
async fn document_cookie_setter_sends_on_next_fetch() {
    let server = MockServer::start().await;
    // Match-on-cookie: respond differently when the request carries a
    // Cookie header. This is the only way to assert at-the-wire that
    // reqwest is including the cookie reqwest didn't set itself but
    // JS did via `document.cookie`.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(header_exists("cookie"))
        .respond_with(ResponseTemplate::new(200).set_body_string("with-cookie"))
        .mount(&server)
        .await;
    // Fallback for requests without a Cookie header — should NOT be
    // hit if the test passes.
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(500).set_body_string("missing-cookie"))
        .mount(&server)
        .await;

    let server_url = Url::parse(&server.uri()).unwrap();
    let (engine, _fetch_engine) = engine_with_shared_jar();
    engine.set_base_url(Some(server_url.clone()));

    // Set the cookie via JS first, then fetch.
    let api_url = format!("{}/api", server.uri());
    let script = format!(
        r#"
        document.cookie = 'session=abc; Path=/';
        fetch({api_url:?}).then(r => r.text()).then(t => {{ globalThis.__body = t; }});
        "#,
        api_url = api_url
    );
    let html = "<html><body></body></html>";
    let doc = heso_engine_js::Document::from_html(html);
    engine
        .install_document(doc, ScriptFetchPolicy::default())
        .expect("install");
    engine.eval(&script).expect("schedule fetch");

    let body = engine.eval("globalThis.__body").expect("observe");
    assert_eq!(
        body.value,
        serde_json::json!("with-cookie"),
        "fetch after document.cookie= should send the Cookie header"
    );
}

// ===== Test 3: HttpOnly hides from document.cookie but jar keeps it =====

#[tokio::test(flavor = "multi_thread")]
async fn http_only_cookies_excluded_from_document_cookie() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                // Use `append_header` for the second Set-Cookie —
                // `insert_header` would clobber the first. Real
                // servers emit `Set-Cookie` as a multi-valued header
                // (one cookie per line), which is what reqwest's
                // cookie integration walks.
                .append_header("Set-Cookie", "secret=value; HttpOnly; Path=/")
                .append_header("Set-Cookie", "visible=yes; Path=/")
                .set_body_string("<html><body></body></html>"),
        )
        .mount(&server)
        .await;
    // /api echoes whatever cookies arrive so we can assert that
    // HttpOnly cookies DO travel in HTTP requests even though they
    // hide from document.cookie.
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(|req: &Request| {
            let cookie_hdr = req
                .headers
                .get("cookie")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            ResponseTemplate::new(200).set_body_string(cookie_hdr)
        })
        .mount(&server)
        .await;

    let server_url = Url::parse(&server.uri()).unwrap();
    let (engine, _fetch_engine) = engine_with_shared_jar();
    engine.set_base_url(Some(server_url.clone()));

    // Land the Set-Cookie via the homepage fetch.
    let fetch_home = format!(
        r#"fetch({url:?}).then(r => r.text()).then(_ => {{ globalThis.__home = true; }});"#,
        url = server.uri()
    );
    engine.eval(&fetch_home).expect("fetch home");

    // Install a document at the same origin and read document.cookie.
    let doc = heso_engine_js::Document::from_html("<html><body></body></html>");
    engine
        .install_document(doc, ScriptFetchPolicy::default())
        .expect("install");
    let cookie_js = engine.eval("document.cookie").expect("eval");
    let cookie_str = cookie_js.value.as_str().expect("cookie is string");
    assert!(
        !cookie_str.contains("secret"),
        "HttpOnly cookie 'secret' must NOT appear in document.cookie (got {cookie_str:?})"
    );
    assert!(
        cookie_str.contains("visible=yes"),
        "Non-HttpOnly cookie 'visible' SHOULD appear (got {cookie_str:?})"
    );

    // Sanity check: both cookies are in the shared jar (even though
    // document.cookie hides the HttpOnly one).
    let jar = _fetch_engine.cookie_jar();
    let guard = jar.lock().unwrap();
    let names: Vec<String> = guard.iter_any().map(|c| c.name().to_string()).collect();
    drop(guard);
    assert!(
        names.iter().any(|n| n == "secret"),
        "HttpOnly cookie 'secret' MUST be present in the jar (names={names:?})"
    );
    assert!(
        names.iter().any(|n| n == "visible"),
        "Non-HttpOnly cookie 'visible' MUST be present in the jar (names={names:?})"
    );

    // And HttpOnly cookies DO travel in fetch requests — the spec
    // only hides them from JS reads, not from the cookie store.
    let api_url = format!("{}/api", server.uri());
    let echo_js = format!(
        r#"fetch({api_url:?}).then(r => r.text()).then(t => {{ globalThis.__echo = t; }});"#,
        api_url = api_url
    );
    engine.eval(&echo_js).expect("fetch api");
    let echo = engine.eval("globalThis.__echo").expect("observe echo");
    let echo_str = echo.value.as_str().expect("echo is string");
    assert!(
        echo_str.contains("secret=value"),
        "HttpOnly cookie SHOULD be sent in HTTP Cookie header (got {echo_str:?})"
    );
}

// ===== Test 4: cookies are origin-scoped =====

#[tokio::test(flavor = "multi_thread")]
async fn cookies_scoped_by_origin() {
    // Cookies are scoped by host per RFC 6265 §5.3 — a cookie set on
    // a.example.com is not visible to b.example.com. (Port is NOT
    // part of the cookie origin in RFC 6265, which is why this test
    // uses two distinct hostnames rather than two `wiremock`
    // MockServer instances on different localhost ports.)
    //
    // We exercise this through `document.cookie` directly because
    // wiremock can only listen on localhost — driving real HTTP
    // requests to two distinct hosts would require a hostname-
    // rewriting setup that's heavier than the property being tested.
    // The setter goes through `cookie_store::CookieStore::parse`
    // exactly the same way reqwest's `Set-Cookie` ingestion does, so
    // this still pins the real path.

    let html = "<html><body></body></html>";
    let url_a = Url::parse("https://a.example.com/").unwrap();
    let url_b = Url::parse("https://b.example.com/").unwrap();

    let (engine, _fetch_engine) = engine_with_shared_jar();

    // Set on host A.
    engine.set_base_url(Some(url_a.clone()));
    let doc_a1 = heso_engine_js::Document::from_html(html);
    engine
        .install_document(doc_a1, ScriptFetchPolicy::default())
        .expect("install A");
    engine
        .eval("document.cookie = 'from_a=alpha; Path=/'")
        .expect("set A");

    // Switch to host B and assert empty.
    engine.set_base_url(Some(url_b.clone()));
    let doc_b = heso_engine_js::Document::from_html(html);
    engine
        .install_document(doc_b, ScriptFetchPolicy::default())
        .expect("install B");
    let out_b = engine.eval("document.cookie").expect("eval cookie B");
    assert_eq!(
        out_b.value,
        serde_json::json!(""),
        "Cookies set on a.example.com must not leak to b.example.com (got {:?})",
        out_b.value
    );

    // And switching back to A sees the cookie again.
    engine.set_base_url(Some(url_a.clone()));
    let doc_a2 = heso_engine_js::Document::from_html(html);
    engine
        .install_document(doc_a2, ScriptFetchPolicy::default())
        .expect("install A2");
    let out_a = engine.eval("document.cookie").expect("eval cookie A");
    assert_eq!(out_a.value, serde_json::json!("from_a=alpha"));
}

// ===== Test 5: expired cookies are filtered =====

#[tokio::test(flavor = "multi_thread")]
async fn expired_cookies_not_returned() {
    // `Max-Age=0` is the spec-mandated way to immediately expire a
    // cookie (RFC 6265 §5.3 step 11). The setter should accept the
    // input but the getter should never return the expired cookie.
    let html = "<html><body></body></html>";
    let url = Url::parse("https://example.com/").unwrap();
    let (session, _fetch_engine) = session_at(html, url);

    // Set then immediately try to read.
    let out = session
        .eval(
            r#"
            document.cookie = 'gone=value; Max-Age=0; Path=/';
            document.cookie
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(""),
        "Cookie with Max-Age=0 should be filtered out as expired"
    );
}

// ===== Test 6: cookies persist across serve-style session calls =====

#[tokio::test(flavor = "multi_thread")]
async fn cookies_persist_across_serve_session_calls() {
    // End-to-end: navigate to /login (server sets cookie via
    // Set-Cookie), then navigate to /dashboard (the request should
    // carry the cookie back, so an auth-gated endpoint succeeds).
    //
    // This is the load-bearing scenario the cookie-jar work was
    // shipped for: a real login flow only works if the jar persists
    // across `navigate` calls within a single `JsSession`.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Set-Cookie", "session=alice; Path=/")
                .set_body_string("<html><body>logged in</body></html>"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dashboard"))
        .and(header_exists("cookie"))
        .respond_with(ResponseTemplate::new(200).set_body_string("dashboard for alice"))
        .mount(&server)
        .await;
    // Unauthenticated branch — should NOT fire if cookies persist.
    Mock::given(method("GET"))
        .and(path("/dashboard"))
        .respond_with(ResponseTemplate::new(401).set_body_string("not logged in"))
        .mount(&server)
        .await;

    let (engine, fetch_engine) = engine_with_shared_jar();
    let login_url = Url::parse(&format!("{}/login", server.uri())).unwrap();
    let dashboard_url = Url::parse(&format!("{}/dashboard", server.uri())).unwrap();

    // Drive the navigations through `FetchEngine` (the path
    // `heso serve` actually uses for `navigate`) — this exercises the
    // *static* path's reqwest cookie integration. The shared jar
    // means cookies set by /login are visible on the /dashboard
    // request too.
    let _login_page = fetch_engine.fetch_text(&login_url).await.expect("login");
    let dash = fetch_engine.fetch_text(&dashboard_url).await.expect("dashboard");
    assert_eq!(
        dash.1,
        "dashboard for alice",
        "/dashboard should serve the auth-gated body because /login's Set-Cookie persisted"
    );

    // And the JS engine sees the same jar — `document.cookie` reads
    // back what the server set, after we install a doc scoped to the
    // server origin.
    engine.set_base_url(Some(login_url.clone()));
    let doc = heso_engine_js::Document::from_html("<html><body></body></html>");
    engine
        .install_document(doc, ScriptFetchPolicy::default())
        .expect("install");
    let cookie = engine.eval("document.cookie").expect("eval cookie");
    assert_eq!(
        cookie.value,
        serde_json::json!("session=alice"),
        "JS should observe the cookie reqwest's static path ingested via Set-Cookie"
    );

}
