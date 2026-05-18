//! Per-URL invariants for the pinned-URL inventory.
//!
//! Each test:
//! 1. Resolves a cassette via [`load_or_record_cassette`] (RECORD if
//!    `HESO_COMPAT_RECORD=1`, else REPLAY).
//! 2. Boots a local [`wiremock::MockServer`] that serves that cassette.
//! 3. Opens `mock_server.uri()` through [`FetchEngine::open`] — same code
//!    path as a real fetch.
//! 4. Asserts the small, stable invariants we expect from the live page.
//!
//! Invariants are intentionally weak — title text, action count >= N,
//! body length > N, a known phrase, etc. We don't snapshot the full HTML
//! because we want the cassette to be the source of truth and the
//! invariant to be "what would a human-readable assertion say about this
//! page after a year of incidental drift?"
//!
//! If you change an invariant, also rerecord the cassette in the same
//! commit so a future bisect can pinpoint what shifted.

use heso_compat_tests::{load_or_record_cassette, serve_cassette};
use heso_engine_api::EngineApi;
use heso_engine_fetch::{FetchEngine, TreeNode};
use url::Url;

/// Count every node in a [`TreeNode`] subtree, including the root.
fn count_tree_nodes(node: &TreeNode) -> usize {
    1 + node.children.iter().map(count_tree_nodes).sum::<usize>()
}

// ============================================================================
// 1. example.com — canonical smallest-static-SSR page
// ============================================================================

#[tokio::test]
async fn example_com_static_invariants() {
    let cassette = load_or_record_cassette("example_com", "https://example.com/")
        .await
        .expect("cassette resolves");
    let mock = serve_cassette(&cassette).await;
    let engine = FetchEngine::new().expect("engine builds");
    let url = Url::parse(&mock.uri()).expect("mock uri parses");
    let page = engine.open(&url).await.expect("open succeeds");

    assert_eq!(page.tree.title, "Example Domain");
    // example.com has at least one outbound link (the "More information…"
    // link to iana.org).
    assert!(
        !page.actions.is_empty(),
        "expected >=1 action on example.com, got {}",
        page.actions.len()
    );
    let has_link = page
        .actions
        .iter()
        .any(|a| a.role == "link" && a.tag == "a");
    assert!(has_link, "expected at least one <a> link in actions graph");
}

// ============================================================================
// 2. httpbin.org/html — Moby Dick excerpt, a predictable test fixture
// ============================================================================

#[tokio::test]
async fn httpbin_html_invariants() {
    let cassette = load_or_record_cassette("httpbin_html", "https://httpbin.org/html")
        .await
        .expect("cassette resolves");
    let mock = serve_cassette(&cassette).await;
    let engine = FetchEngine::new().expect("engine builds");
    let url = Url::parse(&mock.uri()).expect("mock uri parses");
    let page = engine.open(&url).await.expect("open succeeds");

    // httpbin /html serves a fragment of Herman Melville — the body
    // contains the word "Moby-Dick" reliably.
    let text = heso_engine_api::Page::text(&page)
        .await
        .expect("text works");
    assert!(
        text.contains("Moby"),
        "expected 'Moby' in body, got {} chars starting: {:?}",
        text.len(),
        &text.chars().take(120).collect::<String>()
    );
    // It has an h1 with "Herman Melville - Moby-Dick".
    assert!(
        page.tree.root.children.iter().any(|c| {
            c.heading
                .as_deref()
                .map(|h| h.contains("Melville"))
                .unwrap_or(false)
        }),
        "expected an h1 mentioning Melville, tree root children: {:?}",
        page.tree
            .root
            .children
            .iter()
            .map(|c| c.heading.clone())
            .collect::<Vec<_>>()
    );
}

// ============================================================================
// 3. httpbin.org/forms/post — form with text inputs + submit (action graph)
// ============================================================================

#[tokio::test]
async fn httpbin_form_invariants() {
    let cassette = load_or_record_cassette("httpbin_forms_post", "https://httpbin.org/forms/post")
        .await
        .expect("cassette resolves");
    let mock = serve_cassette(&cassette).await;
    let engine = FetchEngine::new().expect("engine builds");
    let url = Url::parse(&mock.uri()).expect("mock uri parses");
    let page = engine.open(&url).await.expect("open succeeds");

    // The page is a <form> with several inputs (custname, custtel,
    // custemail, size radios, topping checkboxes, delivery time, comments,
    // submit). Action-graph coverage check: we expect a <form>, several
    // textbox-shaped controls, and at least one button (the submit).
    let has_form = page.actions.iter().any(|a| a.role == "form");
    assert!(
        has_form,
        "expected a form action on /forms/post, got roles: {:?}",
        page.actions.iter().map(|a| &a.role).collect::<Vec<_>>()
    );
    let textbox_count = page.actions.iter().filter(|a| a.role == "textbox").count();
    assert!(
        textbox_count >= 3,
        "expected >=3 textboxes in form, got {textbox_count}"
    );
    let has_button = page.actions.iter().any(|a| a.role == "button");
    assert!(
        has_button,
        "expected at least one button (submit) on form page"
    );
}

// ============================================================================
// 4. en.wikipedia.org/wiki/HTML — large semantic SSR with meta + headings
// ============================================================================

#[tokio::test]
async fn wikipedia_html_article_invariants() {
    let cassette = load_or_record_cassette("wikipedia_html", "https://en.wikipedia.org/wiki/HTML")
        .await
        .expect("cassette resolves");
    let mock = serve_cassette(&cassette).await;
    let engine = FetchEngine::new().expect("engine builds");
    let url = Url::parse(&mock.uri()).expect("mock uri parses");
    let page = engine.open(&url).await.expect("open succeeds");

    // Title contains "HTML" — Wikipedia uses `<title>HTML - Wikipedia</title>`.
    assert!(
        page.tree.title.contains("HTML"),
        "expected 'HTML' in title, got {:?}",
        page.tree.title
    );
    // Wikipedia is a content-rich semantic page. The article body has one
    // top-level <h1> with many <h2> sub-sections nested underneath. Walk
    // the tree and require >=5 nodes total (root + h1 + a few h2s).
    let total_nodes = count_tree_nodes(&page.tree.root);
    assert!(
        total_nodes >= 5,
        "expected >=5 tree nodes total on Wikipedia article, got {total_nodes}"
    );
    // Wikipedia exposes a <link rel="canonical"> and metadata.
    assert!(
        page.metadata.canonical.is_some()
            || !page.metadata.opengraph.is_empty()
            || !page.metadata.meta.is_empty(),
        "expected non-empty metadata on Wikipedia article"
    );
    // Action graph has many links (Wikipedia is link-dense).
    let link_count = page.actions.iter().filter(|a| a.role == "link").count();
    assert!(
        link_count >= 20,
        "expected >=20 links on Wikipedia article, got {link_count}"
    );
}

// ============================================================================
// 5. news.ycombinator.com — table-based layout, dense action graph
// ============================================================================

#[tokio::test]
async fn hacker_news_front_page_invariants() {
    let cassette = load_or_record_cassette("hacker_news", "https://news.ycombinator.com/")
        .await
        .expect("cassette resolves");
    let mock = serve_cassette(&cassette).await;
    let engine = FetchEngine::new().expect("engine builds");
    let url = Url::parse(&mock.uri()).expect("mock uri parses");
    let page = engine.open(&url).await.expect("open succeeds");

    // HN's <title> is literally "Hacker News".
    assert!(
        page.tree.title.contains("Hacker News"),
        "expected 'Hacker News' in title, got {:?}",
        page.tree.title
    );
    // The front page is link-dense — every story title, plus user, plus
    // points, plus comments, plus pagination. At least 30 links is a
    // safe floor.
    let link_count = page.actions.iter().filter(|a| a.role == "link").count();
    assert!(
        link_count >= 30,
        "expected >=30 links on HN front page, got {link_count}"
    );
}

// ============================================================================
// 6. rust-lang.org — modern SSG with OpenGraph
// ============================================================================

#[tokio::test]
async fn rust_lang_invariants() {
    let cassette = load_or_record_cassette("rust_lang", "https://www.rust-lang.org/")
        .await
        .expect("cassette resolves");
    let mock = serve_cassette(&cassette).await;
    let engine = FetchEngine::new().expect("engine builds");
    let url = Url::parse(&mock.uri()).expect("mock uri parses");
    let page = engine.open(&url).await.expect("open succeeds");

    // The page mentions "Rust" prominently.
    let text = heso_engine_api::Page::text(&page)
        .await
        .expect("text works");
    assert!(text.contains("Rust"), "expected 'Rust' in body text");
    // Modern site: expect OpenGraph or canonical metadata.
    assert!(
        !page.metadata.opengraph.is_empty()
            || page.metadata.canonical.is_some()
            || !page.metadata.meta.is_empty(),
        "expected some metadata on rust-lang.org"
    );
    // And several outbound links.
    let link_count = page.actions.iter().filter(|a| a.role == "link").count();
    assert!(
        link_count >= 5,
        "expected >=5 links on rust-lang.org, got {link_count}"
    );
}

// ============================================================================
// 7. docs.rs — structured nav sidebar (a different shape)
// ============================================================================

#[tokio::test]
async fn docs_rs_invariants() {
    let cassette = load_or_record_cassette("docs_rs", "https://docs.rs/")
        .await
        .expect("cassette resolves");
    let mock = serve_cassette(&cassette).await;
    let engine = FetchEngine::new().expect("engine builds");
    let url = Url::parse(&mock.uri()).expect("mock uri parses");
    let page = engine.open(&url).await.expect("open succeeds");

    let text = heso_engine_api::Page::text(&page)
        .await
        .expect("text works");
    assert!(
        text.contains("Docs.rs") || text.contains("docs.rs"),
        "expected 'docs.rs' in body text"
    );
    // docs.rs landing has many crate links and search machinery — at
    // least a few action-graph entries.
    assert!(
        !page.actions.is_empty(),
        "expected >=1 action on docs.rs landing"
    );
}

// ============================================================================
// 8. iana.org reserved domains — second tiny stable page (paired with example.com)
// ============================================================================

#[tokio::test]
async fn iana_reserved_domains_invariants() {
    let cassette =
        load_or_record_cassette("iana_reserved", "https://www.iana.org/help/example-domains")
            .await
            .expect("cassette resolves");
    let mock = serve_cassette(&cassette).await;
    let engine = FetchEngine::new().expect("engine builds");
    let url = Url::parse(&mock.uri()).expect("mock uri parses");
    let page = engine.open(&url).await.expect("open succeeds");

    // The page documents the reserved second-level domains; the phrase
    // "example.com" is a very stable invariant.
    let text = heso_engine_api::Page::text(&page)
        .await
        .expect("text works");
    assert!(
        text.contains("example.com") || text.contains("Example"),
        "expected 'example.com' or 'Example' in iana.org page"
    );
    // IANA pages have stable section structure.
    assert!(
        !page.tree.root.children.is_empty(),
        "expected at least one heading section on iana.org page"
    );
}
