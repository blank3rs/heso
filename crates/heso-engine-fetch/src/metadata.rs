//! # metadata
//!
//! Structured metadata extraction from a parsed HTML document — the engine's
//! "free meal." Modern web pages embed structured data for search engines and
//! social previews: Schema.org JSON-LD, OpenGraph, Twitter cards, standard
//! SEO meta. heso surfaces all of it as first-class agent context, so the LLM
//! doesn't have to read prose to find facts the page already declared in
//! structured form.
//!
//! ## What we extract
//!
//! - **JSON-LD** (`<script type="application/ld+json">`): full JSON values,
//!   often `Organization`, `Product`, `Article`, `BreadcrumbList`, `FAQPage`.
//!   Many pages have multiple blocks — we keep them all, in document order.
//! - **OpenGraph** (`<meta property="og:*">`): the social-preview vocabulary
//!   (`og:title`, `og:description`, `og:image`, `og:type`, `og:site_name`,
//!   `og:url`). Keys are stored without the `og:` prefix.
//! - **Twitter cards** (`<meta name="twitter:*">`): same idea as OG. Stored
//!   without the `twitter:` prefix.
//! - **Standard SEO meta** (everything else with `<meta name="...">`):
//!   `description`, `keywords`, `author`, `robots`, `theme-color`, ...
//! - **Canonical URL** (`<link rel="canonical">`): the page's canonical
//!   address, useful when the agent landed on a tracking URL.
//! - **Icons** (`<link rel="icon">`, `apple-touch-icon`, `shortcut-icon`):
//!   favicons, in document order.
//! - **Language** (`<html lang="...">`).
//!
//! ## Determinism
//!
//! Per [ADR 0008] the engine is deterministic by default. All maps here are
//! [`BTreeMap`]s so JSON serialization is sorted; vectors preserve document
//! order. No clocks, no RNG.
//!
//! [ADR 0008]: ../../../decisions/0008-determinism-by-default.md

use std::collections::BTreeMap;

use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};

/// Structured metadata extracted from a page.
///
/// All fields are populated from `<meta>`, `<link>`, and
/// `<script type="application/ld+json">` elements found in the parsed HTML.
/// They are independent of the prose the page renders — sites declare these
/// for SEO, social previews, and rich snippets; agents read them for free.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageMetadata {
    /// All JSON-LD blocks on the page, parsed as `serde_json::Value`. Each
    /// is a Schema.org document. Pages often have several; document order
    /// is preserved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jsonld: Vec<serde_json::Value>,

    /// OpenGraph `og:*` meta tags. Keys are stored *without* the `og:`
    /// prefix (e.g. `"title"`, `"image"`, `"site_name"`). Sorted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub opengraph: BTreeMap<String, String>,

    /// Twitter card `twitter:*` meta tags. Keys are stored *without* the
    /// `twitter:` prefix. Sorted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub twitter: BTreeMap<String, String>,

    /// Other `<meta name="...">` tags (everything not under `twitter:`).
    /// `description` is duplicated here in addition to
    /// `HtmlTree.description`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,

    /// `<link rel="canonical" href="...">` href, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,

    /// `<link rel="icon">`, `<link rel="apple-touch-icon">`, and
    /// `<link rel="shortcut-icon">` hrefs, in document order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<String>,

    /// `<html lang="...">` value if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

impl PageMetadata {
    /// True when nothing was found — useful for skipping in agent context.
    pub fn is_empty(&self) -> bool {
        self.jsonld.is_empty()
            && self.opengraph.is_empty()
            && self.twitter.is_empty()
            && self.meta.is_empty()
            && self.canonical.is_none()
            && self.icons.is_empty()
            && self.lang.is_none()
    }

    /// Total bytes of the JSON-LD payload (sum of each block's serialized
    /// size). Useful for context-budget accounting in agents.
    pub fn jsonld_bytes(&self) -> usize {
        self.jsonld
            .iter()
            .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
            .sum()
    }
}

/// Extract structured metadata from a parsed HTML document.
///
/// The caller passes an already-parsed [`scraper::Html`] so this can share
/// the single parse the rest of the engine uses.
pub fn extract(doc: &Html) -> PageMetadata {
    PageMetadata {
        jsonld: extract_jsonld(doc),
        opengraph: extract_meta_prefixed(doc, "property", "og:"),
        twitter: extract_meta_prefixed(doc, "name", "twitter:"),
        meta: extract_meta_unprefixed(doc),
        canonical: extract_canonical(doc),
        icons: extract_icons(doc),
        lang: extract_lang(doc),
    }
}

// ============================================================================
// Per-extractor helpers
// ============================================================================

fn extract_jsonld(doc: &Html) -> Vec<serde_json::Value> {
    let selector =
        Selector::parse(r#"script[type="application/ld+json"]"#).expect("valid selector");
    doc.select(&selector)
        .filter_map(|s| {
            let raw: String = s.text().collect();
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }
            // Sites occasionally embed multiple top-level objects in one
            // block by accident; we keep it simple and accept the first
            // valid value. Multi-block pages are extremely common — sites
            // emit a new <script> per Schema.org type instead.
            serde_json::from_str::<serde_json::Value>(trimmed).ok()
        })
        .collect()
}

fn extract_meta_prefixed(doc: &Html, attr: &str, prefix: &str) -> BTreeMap<String, String> {
    let selector = Selector::parse("meta").expect("valid selector");
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for el in doc.select(&selector) {
        let key = match el.value().attr(attr) {
            Some(k) => k,
            None => continue,
        };
        let Some(rest) = key.strip_prefix(prefix) else {
            continue;
        };
        if let Some(content) = el.value().attr("content") {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                // First occurrence wins. OG allows duplicates (e.g.
                // multiple `og:image`) but for an LLM-shaped map we
                // dedupe to the first.
                out.entry(rest.to_owned())
                    .or_insert_with(|| trimmed.to_owned());
            }
        }
    }
    out
}

fn extract_meta_unprefixed(doc: &Html) -> BTreeMap<String, String> {
    let selector = Selector::parse("meta[name]").expect("valid selector");
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for el in doc.select(&selector) {
        let key = match el.value().attr("name") {
            Some(k) => k,
            None => continue,
        };
        if key.starts_with("twitter:") {
            // Captured separately under `twitter`.
            continue;
        }
        if let Some(content) = el.value().attr("content") {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                out.entry(key.to_owned())
                    .or_insert_with(|| trimmed.to_owned());
            }
        }
    }
    out
}

fn extract_canonical(doc: &Html) -> Option<String> {
    let selector = Selector::parse(r#"link[rel="canonical"]"#).expect("valid selector");
    doc.select(&selector)
        .next()
        .and_then(|el| el.value().attr("href"))
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
}

fn extract_icons(doc: &Html) -> Vec<String> {
    // `rel` can hold multiple space-separated tokens. Match any link whose
    // rel set contains an icon-flavored token.
    let selector = Selector::parse("link[rel]").expect("valid selector");
    let mut out = Vec::new();
    for el in doc.select(&selector) {
        let rel = match el.value().attr("rel") {
            Some(r) => r,
            None => continue,
        };
        let is_icon = rel
            .split_ascii_whitespace()
            .any(|t| matches!(t, "icon" | "apple-touch-icon" | "shortcut-icon"));
        if !is_icon {
            continue;
        }
        if let Some(href) = el.value().attr("href") {
            let trimmed = href.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_owned());
            }
        }
    }
    out
}

fn extract_lang(doc: &Html) -> Option<String> {
    let selector = Selector::parse("html").expect("valid selector");
    doc.select(&selector)
        .next()
        .and_then(|el| el.value().attr("lang"))
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(html: &str) -> Html {
        Html::parse_document(html)
    }

    #[test]
    fn extracts_opengraph_twitter_meta_canonical_icons_lang() {
        let html = r#"
            <html lang="en"><head>
              <meta name="description" content="A test page">
              <meta name="author" content="Akshay">
              <meta name="keywords" content="agents, browsers">
              <meta property="og:title" content="OG Title">
              <meta property="og:type" content="website">
              <meta property="og:image" content="https://example.com/a.png">
              <meta property="og:site_name" content="heso">
              <meta name="twitter:card" content="summary_large_image">
              <meta name="twitter:title" content="Tweet Title">
              <link rel="canonical" href="https://example.com/canonical">
              <link rel="icon" href="/favicon.ico">
              <link rel="apple-touch-icon" href="/apple-icon.png">
              <link rel="stylesheet" href="/style.css">
            </head><body></body></html>
        "#;
        let md = extract(&parse(html));

        assert_eq!(md.lang.as_deref(), Some("en"));

        // OpenGraph: prefix stripped, sorted by key.
        assert_eq!(
            md.opengraph.get("title").map(String::as_str),
            Some("OG Title")
        );
        assert_eq!(
            md.opengraph.get("type").map(String::as_str),
            Some("website")
        );
        assert_eq!(
            md.opengraph.get("image").map(String::as_str),
            Some("https://example.com/a.png")
        );
        assert_eq!(
            md.opengraph.get("site_name").map(String::as_str),
            Some("heso")
        );

        // Twitter: prefix stripped, NOT also in `meta`.
        assert_eq!(
            md.twitter.get("card").map(String::as_str),
            Some("summary_large_image")
        );
        assert_eq!(
            md.twitter.get("title").map(String::as_str),
            Some("Tweet Title")
        );
        assert!(!md.meta.contains_key("twitter:card"));

        // Standard meta.
        assert_eq!(
            md.meta.get("description").map(String::as_str),
            Some("A test page")
        );
        assert_eq!(md.meta.get("author").map(String::as_str), Some("Akshay"));
        assert_eq!(
            md.meta.get("keywords").map(String::as_str),
            Some("agents, browsers")
        );

        // Canonical + icons (stylesheet link ignored).
        assert_eq!(
            md.canonical.as_deref(),
            Some("https://example.com/canonical")
        );
        assert_eq!(
            md.icons,
            vec!["/favicon.ico".to_owned(), "/apple-icon.png".to_owned()]
        );
    }

    #[test]
    fn extracts_jsonld_documents() {
        let html = r#"
            <html><head>
              <script type="application/ld+json">
              {"@context":"https://schema.org","@type":"Organization","name":"Acme","url":"https://acme.example"}
              </script>
              <script type="application/ld+json">
              {"@context":"https://schema.org","@type":"Product","name":"Widget","offers":{"@type":"Offer","price":"19.99","priceCurrency":"USD"}}
              </script>
              <script>not jsonld</script>
            </head><body></body></html>
        "#;
        let md = extract(&parse(html));
        assert_eq!(md.jsonld.len(), 2);
        assert_eq!(md.jsonld[0]["@type"], "Organization");
        assert_eq!(md.jsonld[0]["name"], "Acme");
        assert_eq!(md.jsonld[1]["@type"], "Product");
        assert_eq!(md.jsonld[1]["offers"]["price"], "19.99");
        assert!(md.jsonld_bytes() > 0);
    }

    #[test]
    fn empty_page_yields_empty_metadata() {
        let md = extract(&parse("<html><body></body></html>"));
        assert!(md.is_empty());
    }

    #[test]
    fn malformed_jsonld_is_skipped() {
        let html = r#"
            <html><head>
              <script type="application/ld+json">{ not json }</script>
              <script type="application/ld+json">{"@type":"Thing"}</script>
            </head><body></body></html>
        "#;
        let md = extract(&parse(html));
        assert_eq!(md.jsonld.len(), 1);
        assert_eq!(md.jsonld[0]["@type"], "Thing");
    }

    #[test]
    fn duplicate_meta_keys_keep_first() {
        let html = r#"
            <html><head>
              <meta property="og:image" content="first.png">
              <meta property="og:image" content="second.png">
            </head><body></body></html>
        "#;
        let md = extract(&parse(html));
        assert_eq!(
            md.opengraph.get("image").map(String::as_str),
            Some("first.png")
        );
    }

    #[test]
    fn icons_match_multi_token_rel() {
        // `rel` with multiple tokens, e.g. `rel="shortcut icon"`.
        let html = r#"
            <html><head>
              <link rel="shortcut icon" href="/legacy.ico">
              <link rel="icon" href="/favicon.ico">
            </head><body></body></html>
        "#;
        let md = extract(&parse(html));
        // `shortcut icon` matches the `icon` token check.
        assert_eq!(
            md.icons,
            vec!["/legacy.ico".to_owned(), "/favicon.ico".to_owned()]
        );
    }
}
