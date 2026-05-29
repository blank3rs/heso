//! Per-engine HTML parsers for the search verb plus their text helpers.
//! These functions are pure: HTML in, [`RawResult`]s out, no I/O — so
//! they fixture-test cleanly against pinned markup (see the `tests`
//! module below). The network orchestration that feeds them lives in
//! [`super::http`].

use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;

use super::{BackendId, RawResult};

/// Parse one DDG HTML page into `RawResult`s. Selectors:
///
/// - `.result` — each search-result block
/// - `a.result__a` — the title link (text + wrapped href)
/// - `.result__snippet` — the snippet (an `<a>` sibling, despite the name)
///
/// The href on `a.result__a` looks like
/// `//duckduckgo.com/l/?uddg=<urlencoded-real-url>&rut=...`; we extract
/// `uddg` and percent-decode it to the canonical destination.
pub(super) fn ddg_parse_html(html: &str) -> Vec<RawResult> {
    let doc = Html::parse_document(html);
    let result_sel = Selector::parse(".result").expect("static selector .result");
    let title_sel = Selector::parse("a.result__a").expect("static selector a.result__a");
    // `.result__snippet` is sometimes an `<a>`, sometimes a `<div>` —
    // the class selector covers both.
    let snippet_sel = Selector::parse(".result__snippet").expect("static selector .result__snippet");

    let mut out = Vec::new();
    for result in doc.select(&result_sel) {
        let title_el = match result.select(&title_sel).next() {
            Some(el) => el,
            None => continue,
        };
        let title = collapse_ws(&extract_text(&title_el));
        let raw_href = match title_el.value().attr("href") {
            Some(h) => h,
            None => continue,
        };
        let url = match unwrap_ddg_href(raw_href) {
            Some(u) => u,
            None => continue,
        };
        // Filter DDG-internal "y.js" pixel links the way the ddgs
        // Python library does.
        if url.starts_with("https://duckduckgo.com/y.js?")
            || url.starts_with("http://duckduckgo.com/y.js?")
        {
            continue;
        }
        let snippet = result
            .select(&snippet_sel)
            .next()
            .map(|el| collapse_ws(&extract_text(&el)))
            .unwrap_or_default();
        if title.is_empty() || url.is_empty() {
            continue;
        }
        out.push(RawResult {
            title,
            url,
            snippet,
            source: BackendId::DdgHtml,
        });
    }
    out
}

/// Parse one Mojeek results page. Each result is a
/// `<ul class="results-standard"> <li>` carrying an
/// `<a class="title" href="…">title</a>` (the href is the direct
/// destination, not a redirect wrapper) plus a `<p class="s">` snippet.
pub(super) fn mojeek_parse_html(html: &str) -> Vec<RawResult> {
    let doc = Html::parse_document(html);
    let item_sel = Selector::parse("ul.results-standard li")
        .expect("static selector ul.results-standard li");
    let title_sel = Selector::parse("a.title").expect("static selector a.title");
    let snippet_sel = Selector::parse("p.s").expect("static selector p.s");

    let mut out = Vec::new();
    for item in doc.select(&item_sel) {
        let title_el = match item.select(&title_sel).next() {
            Some(el) => el,
            None => continue,
        };
        let url = match title_el.value().attr("href") {
            Some(h) => h.trim().to_owned(),
            None => continue,
        };
        // Mojeek result hrefs are absolute; defend against the on-site
        // "see more results from <host>" refinement links (relative
        // `/search?q=site:…`) sneaking in if the markup shifts.
        if !url.starts_with("http://") && !url.starts_with("https://") {
            continue;
        }
        let title = collapse_ws(&extract_text(&title_el));
        if title.is_empty() {
            continue;
        }
        let snippet = item
            .select(&snippet_sel)
            .next()
            .map(|el| collapse_ws(&extract_text(&el)))
            .unwrap_or_default();
        out.push(RawResult {
            title,
            url,
            snippet,
            source: BackendId::Mojeek,
        });
    }
    out
}

/// Parse one Brave Search results page (`search.brave.com/search?q=&source=web`).
/// Each organic result is a `<div class="snippet" data-type="web">` carrying
/// a title anchor (`a` with a nested `.title` element) and a
/// `.snippet-description` body. Brave's markup wraps the destination URL
/// directly on the result anchor's `href` (absolute, no redirect hop).
pub(super) fn brave_parse_html(html: &str) -> Vec<RawResult> {
    let doc = Html::parse_document(html);
    // Brave tags organic web results with `data-type="web"`; the
    // attribute selector pins us to those and skips news/video/ad blocks.
    let result_sel = Selector::parse("div.snippet[data-type=\"web\"]")
        .expect("static selector div.snippet[data-type=web]");
    let anchor_sel = Selector::parse("a.heading-serpresult, a.result-header, a[href]")
        .expect("static selector for brave result anchor");
    let title_sel = Selector::parse(".title").expect("static selector .title");
    let snippet_sel =
        Selector::parse(".snippet-description").expect("static selector .snippet-description");

    let mut out = Vec::new();
    for result in doc.select(&result_sel) {
        let anchor = match result.select(&anchor_sel).next() {
            Some(a) => a,
            None => continue,
        };
        let url = match anchor.value().attr("href") {
            Some(h) => h.trim().to_owned(),
            None => continue,
        };
        // Brave result hrefs are absolute destinations; defend against
        // in-page relative refinement links if the markup shifts.
        if !url.starts_with("http://") && !url.starts_with("https://") {
            continue;
        }
        // Prefer the dedicated `.title` node; fall back to the anchor's
        // own text when Brave inlines the title on the anchor.
        let title = result
            .select(&title_sel)
            .next()
            .map(|el| collapse_ws(&extract_text(&el)))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| collapse_ws(&extract_text(&anchor)));
        if title.is_empty() {
            continue;
        }
        let snippet = result
            .select(&snippet_sel)
            .next()
            .map(|el| collapse_ws(&extract_text(&el)))
            .unwrap_or_default();
        out.push(RawResult {
            title,
            url,
            snippet,
            source: BackendId::Brave,
        });
    }
    out
}

/// One entry in Marginalia's public JSON API response. Marginalia returns
/// `{ "results": [ { "url", "title", "description", ... } ] }`; the fields
/// we map are `url`, `title`, and `description` (the snippet).
#[derive(Debug, Deserialize)]
struct MarginaliaResult {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MarginaliaResponse {
    #[serde(default)]
    results: Vec<MarginaliaResult>,
}

/// Parse Marginalia's public JSON API body into `RawResult`s. Returns an
/// error string when the body is not the expected JSON shape, so the
/// caller can surface a config/transport error rather than silently
/// dropping a malformed response. An empty `results` array is a genuine
/// no-match (`Ok(vec![])`), not an error.
pub(super) fn marginalia_parse_json(body: &str) -> Result<Vec<RawResult>, String> {
    let parsed: MarginaliaResponse =
        serde_json::from_str(body).map_err(|e| format!("marginalia JSON parse failed: {e}"))?;
    let out = parsed
        .results
        .into_iter()
        .filter_map(|r| {
            let url = r.url?.trim().to_owned();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return None;
            }
            Some(RawResult {
                title: collapse_ws(&r.title.unwrap_or_default()),
                url,
                snippet: collapse_ws(&r.description.unwrap_or_default()),
                source: BackendId::Marginalia,
            })
        })
        .collect();
    Ok(out)
}

/// Parse one DuckDuckGo *lite* page (`lite.duckduckgo.com/lite/`). The
/// lite endpoint renders a flat table: each result is a `<tr>` whose
/// `a.result-link` carries the (DDG-redirect-wrapped) destination, and the
/// snippet lives in a following `<td class="result-snippet">`. The href
/// uses the same `//duckduckgo.com/l/?uddg=` wrapper as the HTML endpoint,
/// so it shares [`unwrap_ddg_href`].
pub(super) fn ddg_lite_parse_html(html: &str) -> Vec<RawResult> {
    let doc = Html::parse_document(html);
    let link_sel = Selector::parse("a.result-link").expect("static selector a.result-link");
    let snippet_sel =
        Selector::parse("td.result-snippet").expect("static selector td.result-snippet");

    // The lite layout is a single table where each result link sits in one
    // row and its snippet in a later row. Pair them positionally: the Nth
    // `a.result-link` matches the Nth `td.result-snippet`.
    let snippets: Vec<String> = doc
        .select(&snippet_sel)
        .map(|el| collapse_ws(&extract_text(&el)))
        .collect();

    let mut out = Vec::new();
    for (idx, link) in doc.select(&link_sel).enumerate() {
        let raw_href = match link.value().attr("href") {
            Some(h) => h,
            None => continue,
        };
        let url = match unwrap_ddg_href(raw_href) {
            Some(u) => u,
            None => continue,
        };
        if url.starts_with("https://duckduckgo.com/y.js?")
            || url.starts_with("http://duckduckgo.com/y.js?")
        {
            continue;
        }
        let title = collapse_ws(&extract_text(&link));
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let snippet = snippets.get(idx).cloned().unwrap_or_default();
        out.push(RawResult {
            title,
            url,
            snippet,
            source: BackendId::DdgLite,
        });
    }
    out
}

fn extract_text(el: &ElementRef) -> String {
    let mut s = String::new();
    for t in el.text() {
        s.push_str(t);
    }
    s
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Extract the real destination URL from a DDG redirect href.
///
/// DDG wraps every result link in `//duckduckgo.com/l/?uddg=<urlencoded>`
/// (note the leading `//` — protocol-relative). Some configurations
/// also serve direct hrefs without the wrapping; we pass those
/// through unchanged. The `uddg` parameter is percent-encoded with
/// the standard `url::form_urlencoded` rules.
fn unwrap_ddg_href(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // Normalize protocol-relative URLs to https for parsing.
    let normalized = if let Some(rest) = raw.strip_prefix("//") {
        format!("https://{rest}")
    } else if raw.starts_with('/') {
        // Relative path with no host — not useful as a search result.
        return None;
    } else {
        raw.to_owned()
    };
    let parsed = url::Url::parse(&normalized).ok()?;
    if parsed.host_str() == Some("duckduckgo.com")
        && (parsed.path() == "/l/" || parsed.path() == "/l")
    {
        for (k, v) in parsed.query_pairs() {
            if k == "uddg" {
                return Some(v.into_owned());
            }
        }
        // The expected `uddg` was missing — no usable destination.
        return None;
    }
    // Already a direct URL (some DDG modes), pass through.
    Some(parsed.into())
}

// ============================================================================
// Tests — parser fixtures (hosts aren't configurable, so unit-tested here)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwrap_ddg_href_decodes_uddg() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Ffoo&rut=abc";
        let real = unwrap_ddg_href(href).unwrap();
        assert_eq!(real, "https://example.com/foo");
    }

    #[test]
    fn unwrap_ddg_href_passes_direct_urls_through() {
        let real = unwrap_ddg_href("https://example.com/foo").unwrap();
        assert_eq!(real, "https://example.com/foo");
    }

    #[test]
    fn unwrap_ddg_href_handles_missing_uddg() {
        // A `/l/` redirect with no `uddg=` is unusable — return None.
        assert!(unwrap_ddg_href("//duckduckgo.com/l/?rut=abc").is_none());
    }

    #[test]
    fn unwrap_ddg_href_rejects_pure_relative() {
        assert!(unwrap_ddg_href("/local/path").is_none());
        assert!(unwrap_ddg_href("").is_none());
    }

    #[test]
    fn ddg_parse_html_extracts_title_url_snippet() {
        // Minimal fixture mimicking the real DDG HTML structure: each
        // .result wraps an a.result__a (title + href with uddg) and a
        // .result__snippet sibling.
        let html = r#"<!doctype html><html><body>
            <div class="result">
                <div class="result__title">
                    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&rut=abc">
                        Rust Programming Language
                    </a>
                </div>
                <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F">
                    Rust is a fast, reliable, and productive language.
                </a>
            </div>
            <div class="result">
                <div class="result__title">
                    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2F&rut=def">
                        docs.rs
                    </a>
                </div>
                <div class="result__snippet">Documentation host for Rust crates.</div>
            </div>
        </body></html>"#;
        let rows = ddg_parse_html(html);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "Rust Programming Language");
        assert_eq!(rows[0].url, "https://rust-lang.org/");
        assert!(rows[0].snippet.contains("fast"));
        assert_eq!(rows[1].url, "https://docs.rs/");
    }

    #[test]
    fn ddg_parse_html_skips_y_js_pixel_links() {
        let html = r#"<!doctype html><html><body>
            <div class="result">
                <div class="result__title">
                    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fduckduckgo.com%2Fy.js%3Fabc&rut=def">
                        ad
                    </a>
                </div>
                <a class="result__snippet">ad</a>
            </div>
        </body></html>"#;
        assert!(ddg_parse_html(html).is_empty());
    }

    #[test]
    fn ddg_parse_html_empty_for_no_match_page() {
        // DDG renders the search page even for nonsense; selectors
        // simply find no .result rows. We must return [] without
        // panicking — this test pins that contract.
        let html = "<!doctype html><html><body><h1>No results</h1></body></html>";
        assert!(ddg_parse_html(html).is_empty());
    }

    #[test]
    fn mojeek_parse_html_extracts_title_url_snippet() {
        // Mirrors the live Mojeek markup: each result is a
        // `ul.results-standard > li` with an `a.title` (direct href) and
        // a `p.s` snippet, followed by a relative `p.more` refinement
        // link that must NOT be promoted to a result.
        let html = r#"<!doctype html><html><body>
            <ul class="results-standard">
                <li>
                    <h2><a class="title" title="https://www.rust-lang.org/" href="https://www.rust-lang.org/">Rust Programming Language</a></h2>
                    <p class="s">A language empowering everyone to build <strong>reliable</strong> and efficient software.</p>
                    <p class="more"><a href="/search?q=site%3Awww.rust-lang.org+rust">See more results &raquo;</a></p>
                </li>
                <li>
                    <h2><a class="title" title="https://docs.rs/" href="https://docs.rs/">docs.rs</a></h2>
                    <p class="s">Documentation host for Rust crates.</p>
                </li>
            </ul>
        </body></html>"#;
        let rows = mojeek_parse_html(html);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "Rust Programming Language");
        assert_eq!(rows[0].url, "https://www.rust-lang.org/");
        assert!(rows[0].snippet.contains("reliable"));
        assert_eq!(rows[0].source, BackendId::Mojeek);
        assert_eq!(rows[1].url, "https://docs.rs/");
        // The relative "see more results" refinement link must be filtered.
        assert!(rows.iter().all(|r| r.url.starts_with("http")));
    }

    #[test]
    fn mojeek_parse_html_empty_for_no_results() {
        // A page with no `ul.results-standard` yields zero rows without
        // panicking — search then falls through to the other engines.
        let html = "<!doctype html><html><body><p>No results found.</p></body></html>";
        assert!(mojeek_parse_html(html).is_empty());
    }

    #[test]
    fn collapse_ws_normalises_runs() {
        assert_eq!(collapse_ws("  a  \n\t b   c "), "a b c");
        assert_eq!(collapse_ws(""), "");
    }

    #[test]
    fn brave_parse_html_extracts_title_url_snippet() {
        // Mirrors Brave's organic-result markup: a
        // `div.snippet[data-type="web"]` wrapping a result anchor (the
        // `.title` node + absolute href) and a `.snippet-description`. A
        // non-web block (`data-type="news"`) must NOT be picked up.
        let html = r#"<!doctype html><html><body>
            <div class="snippet" data-type="web">
                <a class="heading-serpresult" href="https://www.rust-lang.org/">
                    <div class="title">Rust Programming Language</div>
                </a>
                <div class="snippet-description">A language empowering everyone to build reliable software.</div>
            </div>
            <div class="snippet" data-type="web">
                <a class="heading-serpresult" href="https://docs.rs/">
                    <div class="title">docs.rs</div>
                </a>
                <div class="snippet-description">Documentation host for Rust crates.</div>
            </div>
            <div class="snippet" data-type="news">
                <a class="heading-serpresult" href="https://example.com/news">
                    <div class="title">A news item that is not an organic web result</div>
                </a>
            </div>
        </body></html>"#;
        let rows = brave_parse_html(html);
        assert_eq!(rows.len(), 2, "only web results, not the news block");
        assert_eq!(rows[0].title, "Rust Programming Language");
        assert_eq!(rows[0].url, "https://www.rust-lang.org/");
        assert!(rows[0].snippet.contains("reliable"));
        assert_eq!(rows[0].source, BackendId::Brave);
        assert_eq!(rows[1].url, "https://docs.rs/");
    }

    #[test]
    fn brave_parse_html_empty_for_no_results() {
        let html = "<!doctype html><html><body><p>No results.</p></body></html>";
        assert!(brave_parse_html(html).is_empty());
    }

    #[test]
    fn marginalia_parse_json_extracts_results() {
        let body = r#"{
            "results": [
                {"url": "https://www.rust-lang.org/", "title": "Rust", "description": "A systems language."},
                {"url": "https://docs.rs/", "title": "docs.rs", "description": "Crate docs."}
            ]
        }"#;
        let rows = marginalia_parse_json(body).expect("valid JSON parses");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "Rust");
        assert_eq!(rows[0].url, "https://www.rust-lang.org/");
        assert!(rows[0].snippet.contains("systems language"));
        assert_eq!(rows[0].source, BackendId::Marginalia);
        assert_eq!(rows[1].url, "https://docs.rs/");
    }

    #[test]
    fn marginalia_parse_json_empty_results_is_ok() {
        let rows = marginalia_parse_json(r#"{"results": []}"#).expect("empty list parses");
        assert!(rows.is_empty());
    }

    #[test]
    fn marginalia_parse_json_rejects_non_json() {
        // An HTML error page (or any non-JSON body) is a parse error, not
        // a silent empty — the caller surfaces it loudly.
        assert!(marginalia_parse_json("<html>503 Service Unavailable</html>").is_err());
    }

    #[test]
    fn ddg_lite_parse_html_extracts_title_url_snippet() {
        // The lite endpoint renders a flat table: each `a.result-link`
        // carries the uddg-wrapped destination, and the snippet is a
        // later `td.result-snippet`. The Nth link pairs with the Nth
        // snippet positionally.
        let html = r#"<!doctype html><html><body>
            <table>
                <tr><td><a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&rut=a">Rust Lang</a></td></tr>
                <tr><td class="result-snippet">A fast, reliable language.</td></tr>
                <tr><td><a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2F&rut=b">docs.rs</a></td></tr>
                <tr><td class="result-snippet">Crate documentation host.</td></tr>
            </table>
        </body></html>"#;
        let rows = ddg_lite_parse_html(html);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "Rust Lang");
        assert_eq!(rows[0].url, "https://rust-lang.org/");
        assert!(rows[0].snippet.contains("reliable"));
        assert_eq!(rows[0].source, BackendId::DdgLite);
        assert_eq!(rows[1].url, "https://docs.rs/");
        assert!(rows[1].snippet.contains("documentation"));
    }

    #[test]
    fn ddg_lite_parse_html_empty_for_no_match() {
        let html = "<!doctype html><html><body><table></table></body></html>";
        assert!(ddg_lite_parse_html(html).is_empty());
    }
}
