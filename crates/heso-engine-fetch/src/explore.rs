//! # explore
//!
//! Pre-fetch link targets and embed them in the page result — V0 of "heso
//! as cartographer." The agent asks for a page; heso returns the page AND
//! mini-trees for every linked page, ready to read without further trips.
//!
//! ## Why
//!
//! ADR 0013 framed the engine as a **semantic extractor**: parse once, emit
//! several typed views (visible text, heading tree, structured metadata,
//! action graph). This module is the next installment: **link-graph
//! pre-fetching**. Instead of handing the agent a page and saying "now go
//! fetch each link you might care about," heso fetches them eagerly and
//! attaches them to the same payload. The agent reads a map; it doesn't
//! drive a live browser.
//!
//! ## Algorithm
//!
//! Given a parsed page's [`ElementRef`] action graph:
//!
//! 1. Filter to `role == "link"` with a usable `href`.
//! 2. Resolve each `href` against the page's final URL.
//! 3. Apply [`should_follow`] — skip cross-origin, fragment-only,
//!    `mailto:`/`tel:`/`javascript:`/`data:`, and links that resolve to the
//!    start URL itself.
//! 4. Dedupe by resolved URL, preserving document order.
//! 5. Cap at the configured limit ([`ExploreOptions::link_cap`]).
//! 6. Fetch each via the same [`crate::FetchEngine`] concurrently (using
//!    [`tokio::task::JoinSet`]) and rebuild tree + metadata + actions per
//!    page.
//! 7. Restore document order on the way out so output is deterministic.
//! 8. Recurse for `depth > 1`.
//!
//! Per-link errors do not fail the whole call; they're captured as
//! [`LinkedPage::error`] so the caller sees which links failed and why.
//!
//! ## Determinism
//!
//! - Link discovery is document-order ([`ElementRef`] is already sorted).
//! - The visited-set is filled in document order.
//! - Concurrency is fine — output is sorted by the document-order index
//!   captured at task spawn, so a given input HTML always produces an
//!   identical [`LinkedPage`] vector regardless of network jitter.
//!
//! ## Limits
//!
//! V0 honors `same-origin only`, the standard skip-list, a configurable
//! cap, and depth. It **does not** consult `robots.txt`, rotate the user
//! agent, sniff content-type before fetching, or honor `<link
//! rel="nofollow">`. Those are explicit punts; see the writeup attached to
//! the task that introduced this module.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::Arc;

use scraper::Html;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use url::Url;

use crate::actions::ElementRef;
use crate::{metadata, tree, FetchEngine};

// ============================================================================
// Public types
// ============================================================================

/// Hard upper bound on links followed per level, regardless of caller
/// request. Keeps a single call from accidentally fanning out across a
/// whole sitemap.
pub const HARD_LINK_CAP: usize = 50;

/// Default cap if the caller doesn't pass one.
pub const DEFAULT_LINK_CAP: usize = 20;

/// Options for [`crate::FetchEngine::open_with_explore`].
///
/// `depth = 0` is the "off" state — equivalent to a regular [`crate::FetchEngine::open`].
/// `depth = 1` fetches the page's direct links. `depth >= 2` recurses;
/// each fetched link itself gets `depth - 1` more exploration.
#[derive(Debug, Clone, Copy)]
pub struct ExploreOptions {
    /// How deep to recurse. `0` disables exploration.
    pub depth: u8,
    /// Maximum number of links followed at each level. Clamped to
    /// [`HARD_LINK_CAP`].
    pub link_cap: usize,
}

impl ExploreOptions {
    /// Exploration disabled — equivalent to a plain
    /// [`crate::FetchEngine::open`].
    pub const fn none() -> Self {
        Self {
            depth: 0,
            link_cap: DEFAULT_LINK_CAP,
        }
    }

    /// Build options with the given depth and the default cap.
    pub const fn with_depth(depth: u8) -> Self {
        Self {
            depth,
            link_cap: DEFAULT_LINK_CAP,
        }
    }

    /// True when no exploration is requested.
    pub const fn is_disabled(&self) -> bool {
        self.depth == 0 || self.link_cap == 0
    }

    /// Returns the cap clamped to [`HARD_LINK_CAP`].
    pub const fn effective_link_cap(&self) -> usize {
        if self.link_cap > HARD_LINK_CAP {
            HARD_LINK_CAP
        } else {
            self.link_cap
        }
    }

    /// Construct the options to use when recursing into a linked page —
    /// `depth - 1`, same cap.
    pub const fn child(self) -> Self {
        Self {
            depth: self.depth.saturating_sub(1),
            link_cap: self.link_cap,
        }
    }
}

impl Default for ExploreOptions {
    fn default() -> Self {
        Self::none()
    }
}

/// One linked page pre-fetched off the parent. Mirrors the agent-facing
/// shape of a [`crate::FetchPage`] but inlined as serializable data — no
/// nested [`crate::FetchPage`] type (which holds private fields). Failed
/// fetches still produce a [`LinkedPage`] with `error` set and the rest of
/// the structured views empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedPage {
    /// The action-graph ref (`@e0`, …) of the link on the parent page.
    /// Lets the agent correlate a [`LinkedPage`] back to where it came
    /// from in the source document.
    #[serde(rename = "from_ref")]
    pub from_ref: String,
    /// Final URL of the link target after redirect resolution. For failed
    /// fetches this is the pre-fetch resolved URL.
    pub url: String,
    /// Document title of the linked page. Empty string on a failed fetch
    /// or pages without `<title>`.
    pub title: String,
    /// Page description (meta + first-sentence fallback). `None` if the
    /// fetch failed or no description was available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Heading-derived tree. `None` if the fetch failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree: Option<tree::HtmlTree>,
    /// Structured page metadata. `None` if the fetch failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<metadata::PageMetadata>,
    /// Action graph of the linked page. Empty if the fetch failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ElementRef>,
    /// Inline-JSON `<script type="application/json">` blobs from the
    /// linked page (Next.js / Apple `__ACGH_DATA__` / Nuxt / Astro /
    /// generic Remix hydration payloads). Empty when the page has none
    /// or the fetch failed. See [`crate::inline_data`].
    #[serde(
        default,
        skip_serializing_if = "std::collections::BTreeMap::is_empty"
    )]
    pub inline_data:
        std::collections::BTreeMap<String, serde_json::Value>,
    /// Recursive children (this page's own linked pages). Always
    /// present, may be empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_pages: Vec<LinkedPage>,
    /// Error string if the fetch failed. `None` on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ============================================================================
// Link selection
// ============================================================================

/// A link target after URL resolution and filter checks. Internal —
/// the public surface is [`LinkedPage`].
#[derive(Debug, Clone)]
pub(crate) struct LinkTarget {
    /// `@eN` of the link in the parent's action graph.
    pub from_ref: String,
    /// Resolved absolute URL.
    pub url: Url,
}

/// Walk an action graph, apply the link filters, dedupe, cap, and return
/// the surviving link targets in document order.
pub(crate) fn select_links(
    actions: &[ElementRef],
    parent_url: &Url,
    cap: usize,
) -> Vec<LinkTarget> {
    let cap = cap.min(HARD_LINK_CAP);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<LinkTarget> = Vec::new();
    for el in actions {
        if out.len() >= cap {
            break;
        }
        if el.role != "link" {
            continue;
        }
        let Some(href) = el.attrs.get("href") else {
            continue;
        };
        let Some(resolved) = resolve_href(parent_url, href) else {
            continue;
        };
        if !should_follow(parent_url, &resolved) {
            continue;
        }
        let key = canonical_key(&resolved);
        if !seen.insert(key) {
            continue;
        }
        out.push(LinkTarget {
            from_ref: el.ref_id.clone(),
            url: resolved,
        });
    }
    out
}

/// Resolve `href` against `base`, returning `None` for unparseable values.
/// Strips the fragment (we treat `/foo#bar` and `/foo` as the same target —
/// the agent's cartography map cares about the page, not the in-page
/// anchor).
fn resolve_href(base: &Url, href: &str) -> Option<Url> {
    let trimmed = href.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Reject pseudo-schemes early so `Url::join` doesn't pretend it parsed
    // a valid URL ("mailto:foo@bar" parses but isn't navigable).
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("mailto:")
        || lower.starts_with("tel:")
        || lower.starts_with("javascript:")
        || lower.starts_with("data:")
    {
        return None;
    }
    let mut joined = base.join(trimmed).ok()?;
    joined.set_fragment(None);
    Some(joined)
}

/// Decide whether a resolved link should be followed.
///
/// Filters in order:
/// 1. Scheme must be `http` or `https` (no `file:`, no exotic schemes).
/// 2. Same-origin only (scheme + host + port).
/// 3. Fragment-only links and self-links to `base` itself are skipped.
pub(crate) fn should_follow(base: &Url, candidate: &Url) -> bool {
    let scheme = candidate.scheme();
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    if scheme != base.scheme() {
        return false;
    }
    // host_str() folds to lowercase already for URL spec compliance.
    if candidate.host_str() != base.host_str() {
        return false;
    }
    if candidate.port_or_known_default() != base.port_or_known_default() {
        return false;
    }
    // Self-reference: same URL (path + query + no fragment) as the parent.
    if canonical_key(candidate) == canonical_key(base) {
        return false;
    }
    true
}

/// Canonical comparison key for a resolved URL. We strip the fragment
/// (already stripped during resolution, but defensive) and lowercase the
/// scheme + host. Path/query are taken verbatim — they're case-sensitive
/// per RFC 3986 even though many servers normalize.
fn canonical_key(u: &Url) -> String {
    let scheme = u.scheme().to_ascii_lowercase();
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let port = u.port_or_known_default().map(|p| p.to_string()).unwrap_or_default();
    let path = u.path();
    let query = u.query().unwrap_or("");
    if query.is_empty() {
        format!("{scheme}://{host}:{port}{path}")
    } else {
        format!("{scheme}://{host}:{port}{path}?{query}")
    }
}

// ============================================================================
// Recursive exploration
// ============================================================================

/// Engine the cartographer uses to fetch each link. Implementing this as
/// a trait — instead of taking [`FetchEngine`] directly — lets unit tests
/// substitute a deterministic in-process double without going over the
/// network. The trait is `pub(crate)` because it's not part of the
/// public API.
pub(crate) trait LinkFetcher: Clone + Send + Sync + 'static {
    /// Fetch `url` and return `(final_url, html_bytes)`. The HTML is
    /// returned as a string so the caller can do all `scraper` work
    /// (which is `!Send`) inside its own future.
    fn fetch_html(
        &self,
        url: Url,
    ) -> impl std::future::Future<Output = Result<(Url, String), String>> + Send;
}

impl LinkFetcher for FetchEngine {
    async fn fetch_html(&self, url: Url) -> Result<(Url, String), String> {
        let resp = self
            .client_ref()
            .get(url.as_str())
            .send()
            .await
            .map_err(|e| format!("http: {e}"))?;
        let final_url_str = resp.url().as_str().to_owned();
        let final_url = Url::parse(&final_url_str)
            .map_err(|e| format!("bad redirected url: {e}"))?;
        let text = resp.text().await.map_err(|e| format!("body: {e}"))?;
        Ok((final_url, text))
    }
}

/// Boxed `Send` future alias — recursive `async fn`s need indirection
/// (`Box::pin`) and we need the result to satisfy [`JoinSet::spawn`]'s
/// `Send + 'static` bound.
type ExploreFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Vec<LinkedPage>> + Send>>;

/// Internal entry point. Returns the [`LinkedPage`] vector in document
/// order. `parent_actions` and `parent_url` are owned so the spawned
/// futures are `'static`.
///
/// `visited` carries the set of canonical URLs already discovered up the
/// recursion path so we don't loop. Each level adds the URLs it follows
/// before spawning child fetches.
pub(crate) fn explore<E: LinkFetcher>(
    engine: E,
    parent_actions: Vec<ElementRef>,
    parent_url: Url,
    options: ExploreOptions,
    visited: Arc<tokio::sync::Mutex<HashSet<String>>>,
) -> ExploreFuture {
    Box::pin(async move {
        if options.is_disabled() {
            return Vec::new();
        }
        let cap = options.effective_link_cap();
        let candidates = select_links(&parent_actions, &parent_url, cap);
        if candidates.is_empty() {
            return Vec::new();
        }

        // Reserve the URLs we're about to follow against the visited set so a
        // sibling task at the same level doesn't double-fetch.
        let to_fetch: Vec<(usize, LinkTarget)> = {
            let mut guard = visited.lock().await;
            candidates
                .into_iter()
                .filter(|t| guard.insert(canonical_key(&t.url)))
                .enumerate()
                .collect()
        };

        if to_fetch.is_empty() {
            return Vec::new();
        }

        let mut joinset: JoinSet<(usize, LinkedPage)> = JoinSet::new();
        for (idx, target) in to_fetch {
            let engine_clone = engine.clone();
            let visited_clone = visited.clone();
            let opts_child = options.child();
            joinset.spawn(async move {
                let page =
                    fetch_one(engine_clone, target, opts_child, visited_clone).await;
                (idx, page)
            });
        }

        let mut results: Vec<(usize, LinkedPage)> = Vec::with_capacity(joinset.len());
        while let Some(joined) = joinset.join_next().await {
            match joined {
                Ok(pair) => results.push(pair),
                Err(e) => {
                    // A panic in a worker — surface it as a failed
                    // LinkedPage at an unknown slot. Should never happen
                    // with our well-behaved tasks but documented for
                    // completeness.
                    results.push((
                        usize::MAX,
                        LinkedPage {
                            from_ref: String::new(),
                            url: String::new(),
                            title: String::new(),
                            description: None,
                            tree: None,
                            metadata: None,
                            actions: Vec::new(),
                            inline_data: Default::default(),
                            linked_pages: Vec::new(),
                            error: Some(format!("worker panicked: {e}")),
                        },
                    ));
                }
            }
        }

        results.sort_by_key(|(i, _)| *i);
        results.into_iter().map(|(_, p)| p).collect()
    })
}

/// Fetch one link target. Returns a [`LinkedPage`] either way — on error,
/// the structured views are `None` and `error` is filled.
async fn fetch_one<E: LinkFetcher>(
    engine: E,
    target: LinkTarget,
    child_opts: ExploreOptions,
    visited: Arc<tokio::sync::Mutex<HashSet<String>>>,
) -> LinkedPage {
    let LinkTarget { from_ref, url } = target;
    let pre_fetch_url = url.clone();
    let (final_url, body) = match engine.fetch_html(url).await {
        Ok(v) => v,
        Err(e) => {
            return LinkedPage {
                from_ref,
                url: pre_fetch_url.as_str().to_owned(),
                title: String::new(),
                description: None,
                tree: None,
                metadata: None,
                actions: Vec::new(),
                inline_data: Default::default(),
                linked_pages: Vec::new(),
                error: Some(e),
            };
        }
    };

    // All scraper work in a tightly-scoped block so `Html` (which is
    // !Send) never crosses the next .await. The block evaluates to owned
    // (tree, metadata, actions, inline_data) — none of which borrow the
    // parsed `Html`, so the cross-await state stays `Send`.
    let (tree, page_metadata, actions, page_inline_data) = {
        let doc = Html::parse_document(&body);
        let tree = tree::build_tree_from_doc(&doc, &final_url);
        let md = metadata::extract(&doc);
        let actions = crate::actions::extract(&doc);
        let inline = crate::inline_data::extract(&doc);
        (tree, md, actions, inline)
    };

    // Recurse if more depth is requested. The visited set is shared with
    // the caller so cycles can't form across levels either.
    let nested = if child_opts.is_disabled() {
        Vec::new()
    } else {
        explore(
            engine.clone(),
            actions.clone(),
            final_url.clone(),
            child_opts,
            visited,
        )
        .await
    };

    LinkedPage {
        from_ref,
        url: final_url.as_str().to_owned(),
        title: tree.title.clone(),
        description: tree.description.clone(),
        tree: Some(tree),
        metadata: Some(page_metadata),
        actions,
        inline_data: page_inline_data,
        linked_pages: nested,
        error: None,
    }
}

// ============================================================================
// Pretty-printing for the JSON-RPC + CLI surface
// ============================================================================

/// Render a [`LinkedPage`] vector into the JSON shape we expose via
/// `heso open` and `heso serve`. Centralized so the two surfaces stay
/// in sync without one drifting from the other.
pub fn linked_pages_to_json(
    linked_pages: &[LinkedPage],
) -> serde_json::Value {
    // We could just `serde_json::to_value(linked_pages)`, but doing it
    // ourselves means the field order on the wire is documented here
    // (helpful when the LLM is reading the JSON cold).
    let arr: Vec<serde_json::Value> = linked_pages
        .iter()
        .map(|p| {
            let mut obj: BTreeMap<String, serde_json::Value> = BTreeMap::new();
            obj.insert("from_ref".into(), serde_json::Value::String(p.from_ref.clone()));
            obj.insert("url".into(), serde_json::Value::String(p.url.clone()));
            obj.insert("title".into(), serde_json::Value::String(p.title.clone()));
            if let Some(d) = &p.description {
                obj.insert(
                    "description".into(),
                    serde_json::Value::String(d.clone()),
                );
            }
            if let Some(t) = &p.tree {
                obj.insert(
                    "tree".into(),
                    serde_json::to_value(t).unwrap_or(serde_json::Value::Null),
                );
            }
            if let Some(m) = &p.metadata {
                obj.insert(
                    "metadata".into(),
                    serde_json::to_value(m).unwrap_or(serde_json::Value::Null),
                );
            }
            if !p.actions.is_empty() {
                obj.insert(
                    "actions".into(),
                    serde_json::to_value(&p.actions).unwrap_or(serde_json::Value::Null),
                );
            }
            if !p.inline_data.is_empty() {
                obj.insert(
                    "inline_data".into(),
                    serde_json::to_value(&p.inline_data)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            if !p.linked_pages.is_empty() {
                obj.insert("linked_pages".into(), linked_pages_to_json(&p.linked_pages));
            }
            if let Some(e) = &p.error {
                obj.insert("error".into(), serde_json::Value::String(e.clone()));
            }
            serde_json::Value::Object(obj.into_iter().collect())
        })
        .collect();
    serde_json::Value::Array(arr)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::extract as extract_actions;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn parse(html: &str) -> Html {
        Html::parse_document(html)
    }

    // ----- select_links tests (synchronous, no network) -------------------

    #[test]
    fn select_links_keeps_document_order_for_same_origin() {
        let html = r#"
            <html><body>
              <a href="/a">A</a>
              <a href="/b">B</a>
              <a href="/c">C</a>
            </body></html>
        "#;
        let actions = extract_actions(&parse(html));
        let base = u("https://example.com/");
        let picks = select_links(&actions, &base, 10);
        assert_eq!(picks.len(), 3);
        assert_eq!(picks[0].url.as_str(), "https://example.com/a");
        assert_eq!(picks[1].url.as_str(), "https://example.com/b");
        assert_eq!(picks[2].url.as_str(), "https://example.com/c");
        // Refs match the action graph: link is the first interactive el.
        assert_eq!(picks[0].from_ref, "@e0");
        assert_eq!(picks[1].from_ref, "@e1");
        assert_eq!(picks[2].from_ref, "@e2");
    }

    #[test]
    fn select_links_skips_cross_origin() {
        let html = r#"
            <html><body>
              <a href="https://example.com/keep">keep</a>
              <a href="https://other.example.com/drop">drop</a>
              <a href="http://example.com:8080/drop-port">drop-port</a>
              <a href="//cdn.example.org/drop-proto">drop-proto</a>
              <a href="/relative-keep">keep</a>
            </body></html>
        "#;
        let actions = extract_actions(&parse(html));
        let base = u("https://example.com/");
        let picks = select_links(&actions, &base, 10);
        let urls: Vec<&str> = picks.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/keep",
                "https://example.com/relative-keep",
            ]
        );
    }

    #[test]
    fn select_links_skips_pseudo_schemes_and_fragments_and_empty() {
        let html = r##"
            <html><body>
              <a href="mailto:hi@example.com">mail</a>
              <a href="tel:+15551234567">tel</a>
              <a href="javascript:alert(1)">js</a>
              <a href="data:text/html,foo">data</a>
              <a href="#section">frag</a>
              <a href="">empty</a>
              <a href="    ">whitespace</a>
              <a href="/real">real</a>
            </body></html>
        "##;
        let actions = extract_actions(&parse(html));
        let base = u("https://example.com/page");
        let picks = select_links(&actions, &base, 10);
        let urls: Vec<&str> = picks.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, vec!["https://example.com/real"]);
    }

    #[test]
    fn select_links_skips_self_reference() {
        let html = r##"
            <html><body>
              <a href="/page">self path</a>
              <a href="/page#anchor">self with fragment</a>
              <a href="https://example.com/page">self abs</a>
              <a href="/page?with=query">different — has query</a>
              <a href="/other">other</a>
            </body></html>
        "##;
        let actions = extract_actions(&parse(html));
        let base = u("https://example.com/page");
        let picks = select_links(&actions, &base, 10);
        let urls: Vec<&str> = picks.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/page?with=query",
                "https://example.com/other",
            ]
        );
    }

    #[test]
    fn select_links_dedupes_identical_resolved_urls() {
        let html = r##"
            <html><body>
              <a href="/dup">first</a>
              <a href="/dup">second</a>
              <a href="/dup#anchor">third</a>
              <a href="https://example.com/dup">fourth</a>
              <a href="/unique">unique</a>
            </body></html>
        "##;
        let actions = extract_actions(&parse(html));
        let base = u("https://example.com/");
        let picks = select_links(&actions, &base, 10);
        let urls: Vec<&str> = picks.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            vec!["https://example.com/dup", "https://example.com/unique"]
        );
        // The deduper keeps the first ref — `@e0`, the first <a> in source.
        assert_eq!(picks[0].from_ref, "@e0");
    }

    #[test]
    fn select_links_caps_at_requested_limit_and_hard_cap() {
        // 75 distinct links; HARD_LINK_CAP is 50.
        let mut html = String::from("<html><body>");
        for i in 0..75 {
            html.push_str(&format!(r#"<a href="/p{i}">{i}</a>"#));
        }
        html.push_str("</body></html>");
        let actions = extract_actions(&parse(&html));
        let base = u("https://example.com/");

        let small = select_links(&actions, &base, 5);
        assert_eq!(small.len(), 5);
        assert_eq!(small[4].url.path(), "/p4");

        let big = select_links(&actions, &base, 1000);
        assert_eq!(big.len(), HARD_LINK_CAP);
    }

    #[test]
    fn select_links_zero_cap_returns_empty() {
        let html = r#"<html><body><a href="/a">A</a></body></html>"#;
        let actions = extract_actions(&parse(html));
        let base = u("https://example.com/");
        let picks = select_links(&actions, &base, 0);
        assert!(picks.is_empty());
    }

    // ----- end-to-end exploration with an in-memory fetcher --------------

    /// A deterministic in-process fetcher. Maps URLs to HTML bodies.
    /// Wrapped in `Arc<StdMutex<HashMap<...>>>` so `Clone` is cheap and
    /// state is shared across spawned tasks, but we never hold the lock
    /// across `.await` (Clippy `await_holding_lock` would catch that).
    #[derive(Clone)]
    struct MockFetcher {
        pages: Arc<StdMutex<HashMap<String, String>>>,
    }

    impl MockFetcher {
        fn new() -> Self {
            Self {
                pages: Arc::new(StdMutex::new(HashMap::new())),
            }
        }
        fn add(&self, url: &str, html: &str) {
            self.pages
                .lock()
                .unwrap()
                .insert(url.to_owned(), html.to_owned());
        }
    }

    impl LinkFetcher for MockFetcher {
        async fn fetch_html(&self, url: Url) -> Result<(Url, String), String> {
            // Acquire + release the std mutex BEFORE the (no-op) await.
            let body = {
                let g = self.pages.lock().unwrap();
                g.get(url.as_str()).cloned()
            };
            match body {
                Some(b) => Ok((url, b)),
                None => Err(format!("404: {url}")),
            }
        }
    }

    fn explore_blocking(
        engine: &MockFetcher,
        actions: &[ElementRef],
        parent_url: &Url,
        opts: ExploreOptions,
    ) -> Vec<LinkedPage> {
        let mut seed = HashSet::new();
        seed.insert(canonical_key(parent_url));
        let visited = Arc::new(tokio::sync::Mutex::new(seed));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(explore(
            engine.clone(),
            actions.to_vec(),
            parent_url.clone(),
            opts,
            visited,
        ))
    }

    #[test]
    fn depth_zero_returns_empty() {
        let engine = MockFetcher::new();
        engine.add(
            "https://example.com/page1",
            "<html><body>hello</body></html>",
        );
        let html = r#"<html><body><a href="/page1">go</a></body></html>"#;
        let actions = extract_actions(&parse(html));
        let parent = u("https://example.com/");
        let pages = explore_blocking(
            &engine,
            &actions,
            &parent,
            ExploreOptions::with_depth(0),
        );
        assert!(pages.is_empty());
    }

    #[test]
    fn depth_one_fetches_direct_links_only() {
        let engine = MockFetcher::new();
        engine.add(
            "https://example.com/about",
            r#"<html><head><title>About Us</title></head>
               <body>
                 <h1>About</h1><p>We do things.</p>
                 <a href="/team">team</a>
               </body></html>"#,
        );
        engine.add(
            "https://example.com/pricing",
            r#"<html><head><title>Pricing</title></head>
               <body><h1>Pricing</h1><p>Free.</p></body></html>"#,
        );

        let parent_html = r#"
            <html><head><title>Home</title></head>
            <body>
              <a href="/about">about</a>
              <a href="/pricing">pricing</a>
            </body></html>
        "#;
        let actions = extract_actions(&parse(parent_html));
        let parent_url = u("https://example.com/");
        let pages = explore_blocking(
            &engine,
            &actions,
            &parent_url,
            ExploreOptions::with_depth(1),
        );
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].url, "https://example.com/about");
        assert_eq!(pages[0].title, "About Us");
        assert!(pages[0].tree.is_some());
        // Depth-1 means the linked pages themselves do not recurse — even
        // though `about` has a `team` link in it, we shouldn't follow.
        assert!(pages[0].linked_pages.is_empty());
        assert!(pages[0].error.is_none());

        assert_eq!(pages[1].url, "https://example.com/pricing");
        assert_eq!(pages[1].title, "Pricing");
    }

    #[test]
    fn depth_two_nests_one_level_deep() {
        let engine = MockFetcher::new();
        engine.add(
            "https://example.com/about",
            r#"<html><head><title>About</title></head><body>
                 <h1>About</h1><p>x</p>
                 <a href="/team">team</a>
                 <a href="/history">history</a>
               </body></html>"#,
        );
        engine.add(
            "https://example.com/team",
            r#"<html><head><title>Team</title></head><body>
                 <h1>Team</h1><p>y</p>
               </body></html>"#,
        );
        engine.add(
            "https://example.com/history",
            r#"<html><head><title>History</title></head><body>
                 <h1>History</h1><p>z</p>
               </body></html>"#,
        );

        let parent_html =
            r#"<html><body><a href="/about">about</a></body></html>"#;
        let actions = extract_actions(&parse(parent_html));
        let parent_url = u("https://example.com/");
        let pages = explore_blocking(
            &engine,
            &actions,
            &parent_url,
            ExploreOptions::with_depth(2),
        );
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].url, "https://example.com/about");
        // depth-2 → about pulls in its own links.
        assert_eq!(pages[0].linked_pages.len(), 2);
        let nested_urls: Vec<&str> = pages[0]
            .linked_pages
            .iter()
            .map(|p| p.url.as_str())
            .collect();
        assert_eq!(
            nested_urls,
            vec![
                "https://example.com/team",
                "https://example.com/history"
            ]
        );
        // Depth-2 leaf pages don't recurse further.
        for nested in &pages[0].linked_pages {
            assert!(nested.linked_pages.is_empty());
        }
    }

    #[test]
    fn count_cap_is_honored() {
        let engine = MockFetcher::new();
        // Provide bodies for 10 pages.
        for i in 0..10 {
            engine.add(
                &format!("https://example.com/p{i}"),
                &format!(
                    "<html><head><title>P{i}</title></head>\
                     <body><h1>P{i}</h1></body></html>"
                ),
            );
        }
        let mut parent_html = String::from("<html><body>");
        for i in 0..10 {
            parent_html.push_str(&format!(r#"<a href="/p{i}">p{i}</a>"#));
        }
        parent_html.push_str("</body></html>");
        let actions = extract_actions(&parse(&parent_html));
        let parent_url = u("https://example.com/");
        let pages = explore_blocking(
            &engine,
            &actions,
            &parent_url,
            ExploreOptions {
                depth: 1,
                link_cap: 3,
            },
        );
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].url, "https://example.com/p0");
        assert_eq!(pages[1].url, "https://example.com/p1");
        assert_eq!(pages[2].url, "https://example.com/p2");
    }

    #[test]
    fn same_origin_filter_runs_through_the_explore_pipeline() {
        let engine = MockFetcher::new();
        engine.add(
            "https://example.com/keep",
            "<html><body><h1>Keep</h1></body></html>",
        );
        // The cross-origin URL is intentionally NOT registered with the
        // mock. If the filter is broken and a fetch is attempted, the
        // mock returns Err and the test fails on the surfaced error.
        let parent_html = r#"
            <html><body>
              <a href="https://other.example/drop">other</a>
              <a href="/keep">keep</a>
            </body></html>
        "#;
        let actions = extract_actions(&parse(parent_html));
        let parent_url = u("https://example.com/");
        let pages = explore_blocking(
            &engine,
            &actions,
            &parent_url,
            ExploreOptions::with_depth(1),
        );
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].url, "https://example.com/keep");
        assert!(pages[0].error.is_none());
    }

    #[test]
    fn skip_list_runs_through_the_explore_pipeline() {
        let engine = MockFetcher::new();
        engine.add(
            "https://example.com/real",
            "<html><body><h1>Real</h1></body></html>",
        );
        let parent_html = r##"
            <html><body>
              <a href="mailto:hi@example.com">mail</a>
              <a href="tel:+15551234567">tel</a>
              <a href="javascript:void(0)">js</a>
              <a href="data:text/html,x">data</a>
              <a href="#frag">frag</a>
              <a href="/real">real</a>
            </body></html>
        "##;
        let actions = extract_actions(&parse(parent_html));
        let parent_url = u("https://example.com/");
        let pages = explore_blocking(
            &engine,
            &actions,
            &parent_url,
            ExploreOptions::with_depth(1),
        );
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].url, "https://example.com/real");
    }

    #[test]
    fn duplicate_hrefs_produce_one_entry() {
        let engine = MockFetcher::new();
        engine.add(
            "https://example.com/x",
            "<html><body><h1>X</h1></body></html>",
        );
        let parent_html = r##"
            <html><body>
              <a href="/x">first</a>
              <a href="/x">second</a>
              <a href="/x#anchor">third</a>
            </body></html>
        "##;
        let actions = extract_actions(&parse(parent_html));
        let parent_url = u("https://example.com/");
        let pages = explore_blocking(
            &engine,
            &actions,
            &parent_url,
            ExploreOptions::with_depth(1),
        );
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].from_ref, "@e0", "first ref wins on dedupe");
    }

    #[test]
    fn cycle_prevention_page_links_to_itself() {
        let engine = MockFetcher::new();
        // page1 links to page2; page2 links back to page1. With depth-2,
        // page2 should NOT pull page1 in as a child because it's already
        // visited from the parent fetch.
        engine.add(
            "https://example.com/page1",
            r#"<html><head><title>P1</title></head><body>
                 <h1>P1</h1>
                 <a href="/page2">to-2</a>
               </body></html>"#,
        );
        engine.add(
            "https://example.com/page2",
            r#"<html><head><title>P2</title></head><body>
                 <h1>P2</h1>
                 <a href="/page1">back-to-1</a>
               </body></html>"#,
        );
        let parent_html =
            r#"<html><body><a href="/page1">go</a></body></html>"#;
        let actions = extract_actions(&parse(parent_html));
        let parent_url = u("https://example.com/");
        let pages = explore_blocking(
            &engine,
            &actions,
            &parent_url,
            ExploreOptions::with_depth(2),
        );
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].url, "https://example.com/page1");
        // page1 followed page2 (one level deeper); page2 must NOT follow
        // page1 again — that'd be the cycle. With our visited set, page2's
        // own linked_pages list is empty.
        assert_eq!(pages[0].linked_pages.len(), 1);
        assert_eq!(
            pages[0].linked_pages[0].url,
            "https://example.com/page2"
        );
        assert!(pages[0].linked_pages[0].linked_pages.is_empty(),
            "page2 must not have re-followed page1 (cycle prevention)");
    }

    #[test]
    fn per_link_failure_does_not_fail_whole_explore() {
        let engine = MockFetcher::new();
        engine.add(
            "https://example.com/good",
            "<html><head><title>Good</title></head><body></body></html>",
        );
        // example.com/bad NOT registered → mock returns Err.
        let parent_html = r#"
            <html><body>
              <a href="/good">good</a>
              <a href="/bad">bad</a>
            </body></html>
        "#;
        let actions = extract_actions(&parse(parent_html));
        let parent_url = u("https://example.com/");
        let pages = explore_blocking(
            &engine,
            &actions,
            &parent_url,
            ExploreOptions::with_depth(1),
        );
        assert_eq!(pages.len(), 2);
        assert!(pages[0].error.is_none(), "good page succeeds");
        assert_eq!(pages[0].title, "Good");
        let bad = &pages[1];
        assert!(bad.error.is_some(), "bad page records error");
        assert_eq!(bad.url, "https://example.com/bad");
        assert!(bad.tree.is_none() && bad.metadata.is_none());
    }

    #[test]
    fn output_order_is_document_order_across_runs() {
        // Concurrency could in principle perturb output. We assert that
        // it doesn't by running the same exploration five times and
        // checking output is byte-identical.
        let engine = MockFetcher::new();
        for i in 0..8 {
            engine.add(
                &format!("https://example.com/p{i}"),
                &format!(
                    "<html><head><title>P{i}</title></head>\
                     <body><h1>P{i}</h1></body></html>"
                ),
            );
        }
        let mut parent_html = String::from("<html><body>");
        for i in 0..8 {
            parent_html.push_str(&format!(r#"<a href="/p{i}">{i}</a>"#));
        }
        parent_html.push_str("</body></html>");
        let actions = extract_actions(&parse(&parent_html));
        let parent_url = u("https://example.com/");

        let serialize = |pages: &Vec<LinkedPage>| -> String {
            serde_json::to_string(&linked_pages_to_json(pages)).unwrap()
        };

        let first = explore_blocking(
            &engine,
            &actions,
            &parent_url,
            ExploreOptions::with_depth(1),
        );
        let first_json = serialize(&first);
        // URLs come out p0, p1, p2, ..., p7 — document order.
        let urls: Vec<&str> =
            first.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            (0..8)
                .map(|i| format!("https://example.com/p{i}"))
                .collect::<Vec<_>>()
        );

        for _ in 0..4 {
            let again = explore_blocking(
                &engine,
                &actions,
                &parent_url,
                ExploreOptions::with_depth(1),
            );
            assert_eq!(serialize(&again), first_json);
        }
    }

    #[test]
    fn explore_options_clamp_at_hard_cap() {
        let opts = ExploreOptions {
            depth: 1,
            link_cap: 1000,
        };
        assert_eq!(opts.effective_link_cap(), HARD_LINK_CAP);
    }

    #[test]
    fn explore_options_child_decreases_depth() {
        let opts = ExploreOptions {
            depth: 2,
            link_cap: 7,
        };
        let child = opts.child();
        assert_eq!(child.depth, 1);
        assert_eq!(child.link_cap, 7);
        let grand = child.child();
        assert_eq!(grand.depth, 0);
        assert!(grand.is_disabled());
        // Saturating sub stays at 0.
        let great = grand.child();
        assert_eq!(great.depth, 0);
    }
}
