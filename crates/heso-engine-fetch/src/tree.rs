//! # tree
//!
//! Page-as-filesystem view of a parsed HTML document.
//!
//! Native realization of [ADR 0010]'s mental model: the page is the working
//! directory, sections (defined by `<h1>`–`<h6>`) are folders, and the visible
//! text under each heading is the file you `cat`. The tree is built from the
//! document's heading structure in one walk, then exposed via `ls`, `cat`, and
//! `pwd` — the same three primitives an agent already knows from a shell.
//!
//! The purpose is **LLM context-budget control**. Instead of dumping a whole
//! 50KB page into the model's context, the agent first asks for the outline
//! (`ls /`), and then drills into only the sections that look relevant
//! (`cat /features`, `cat /pricing/enterprise`). For a typical marketing or
//! docs page this collapses from tens of KB to ~2KB of structure + a handful
//! of selected sections.
//!
//! ## Walk algorithm
//!
//! 1. Parse with [`scraper::Html`].
//! 2. Title from `<title>`. Description from `<meta name="description">` if
//!    present; otherwise the first sentence of the first `<p>` in `<body>`.
//! 3. Walk `<body>` in document order. Maintain a stack of *open* sections,
//!    one per heading level currently active.
//! 4. On `<hN>`: pop the stack down to depth `N-1`, create a fresh
//!    [`TreeNode`] at level `N`, attach it as a child of the new top, push it.
//! 5. On any other visible content (`<p>`, list items, blockquote, table
//!    cells, plain text, ...): append its text to the current top-of-stack
//!    node's `intro`.
//! 6. Skip `<script>`, `<style>`, `<noscript>`, `<template>` (same rule as
//!    [`super::extract_visible_text`]).
//! 7. Slugs are kebab-case ASCII, max 40 chars, suffix `-2`, `-3`, ... to
//!    disambiguate siblings.
//!
//! [ADR 0010]: ../../../decisions/0010-primitives-as-terminal-commands.md

use std::collections::HashMap;

use scraper::{ElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};
use url::Url;

// ============================================================================
// Errors
// ============================================================================

/// Errors produced by tree navigation (`ls`, `cat`).
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// No node exists at the requested path.
    #[error("no node at path `{0}`")]
    NotFound(String),

    /// The path string was syntactically invalid (e.g. didn't start with `/`).
    #[error("invalid path `{0}`: {1}")]
    BadPath(String, String),
}

// ============================================================================
// Tree types
// ============================================================================

/// A full page expressed as a navigable tree of heading-defined sections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HtmlTree {
    /// Final URL the page lives at (post-redirect).
    pub url: String,
    /// Document `<title>` (empty string if absent).
    pub title: String,
    /// Page description, sourced from `<meta name="description">` if present,
    /// else the first sentence of the first `<p>` in `<body>`. `None` if
    /// neither is available.
    pub description: Option<String>,
    /// The synthetic root node. Its children are the top-level sections.
    pub root: TreeNode,
}

/// A single node in the page tree. Corresponds to either the synthetic root
/// (level 0) or a heading element (level 1–6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeNode {
    /// Canonical path from root. Root is `"/"`; e.g. `/features/pixel-awareness`.
    pub path: String,
    /// Last segment of `path`, kebab-case slug of `heading`.
    pub slug: String,
    /// Raw heading text (`None` for the synthetic root).
    pub heading: Option<String>,
    /// Heading level: 0 for root, 1–6 for `<h1>`–`<h6>`.
    pub level: u8,
    /// One-line summary: `heading` + first 100 chars of `intro` (trimmed).
    pub summary: String,
    /// Visible text under this heading but BEFORE the next sub-heading.
    pub intro: String,
    /// `intro.len()`.
    pub byte_count: usize,
    /// `children.len()`.
    pub child_count: usize,
    /// Direct children (sub-sections).
    pub children: Vec<TreeNode>,
}

/// One row in an `ls` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LsRow {
    /// Absolute path of this child.
    pub path: String,
    /// Slug (last segment).
    pub slug: String,
    /// Original heading text.
    pub heading: Option<String>,
    /// Heading level (1–6).
    pub level: u8,
    /// One-line summary for the LLM.
    pub summary: String,
    /// Bytes of intro text under this node.
    pub byte_count: usize,
    /// Number of direct sub-sections under this node.
    pub child_count: usize,
}

/// Top-level overview returned by `pwd`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PwdRow {
    /// Page URL.
    pub url: String,
    /// Page title.
    pub title: String,
    /// Page description (if any).
    pub description: Option<String>,
    /// Slugs of the immediate (level-1) children — the top-level outline.
    pub top_level: Vec<String>,
}

// ============================================================================
// HtmlTree API
// ============================================================================

impl HtmlTree {
    /// List the children of the node at `path`. Returns one [`LsRow`] per
    /// child. `path = "/"` returns the top-level sections.
    pub fn ls(&self, path: &str) -> Result<Vec<LsRow>, TreeError> {
        let node = self.resolve(path)?;
        Ok(node
            .children
            .iter()
            .map(|c| LsRow {
                path: c.path.clone(),
                slug: c.slug.clone(),
                heading: c.heading.clone(),
                level: c.level,
                summary: c.summary.clone(),
                byte_count: c.byte_count,
                child_count: c.child_count,
            })
            .collect())
    }

    /// Return the `intro` text of the node at `path`.
    pub fn cat(&self, path: &str) -> Result<String, TreeError> {
        Ok(self.resolve(path)?.intro.clone())
    }

    /// Top-level overview: URL + title + description + the first-level slugs.
    pub fn pwd(&self) -> PwdRow {
        PwdRow {
            url: self.url.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            top_level: self.root.children.iter().map(|c| c.slug.clone()).collect(),
        }
    }

    fn resolve(&self, path: &str) -> Result<&TreeNode, TreeError> {
        if !path.starts_with('/') {
            return Err(TreeError::BadPath(
                path.to_owned(),
                "must start with `/`".into(),
            ));
        }
        if path == "/" {
            return Ok(&self.root);
        }
        let mut node = &self.root;
        for seg in path.trim_start_matches('/').split('/') {
            if seg.is_empty() {
                return Err(TreeError::BadPath(
                    path.to_owned(),
                    "empty path segment".into(),
                ));
            }
            match node.children.iter().find(|c| c.slug == seg) {
                Some(child) => node = child,
                None => return Err(TreeError::NotFound(path.to_owned())),
            }
        }
        Ok(node)
    }
}

// ============================================================================
// Builder
// ============================================================================

/// Build an [`HtmlTree`] from raw HTML and the page's final URL. Convenience
/// wrapper around [`build_tree_from_doc`] for callers that don't already
/// hold a parsed [`Html`].
pub fn build_tree(html: &str, url: &Url) -> HtmlTree {
    build_tree_from_doc(&Html::parse_document(html), url)
}

/// Build an [`HtmlTree`] from an already-parsed [`Html`] and the page's
/// final URL. The engine prefers this so the document is parsed exactly
/// once and shared across text / metadata / tree extraction.
pub fn build_tree_from_doc(doc: &Html, url: &Url) -> HtmlTree {
    let title = extract_title(doc);
    let description = extract_description(doc);

    let mut builder = TreeBuilder::new();
    if let Some(body) = doc
        .select(&Selector::parse("body").expect("`body` is a valid selector"))
        .next()
    {
        for child in body.children() {
            walk_node(child, &mut builder);
        }
    } else {
        for child in doc.root_element().children() {
            walk_node(child, &mut builder);
        }
    }

    let mut root = builder.into_root();
    finalize_node(&mut root);

    HtmlTree {
        url: url.as_str().to_owned(),
        title,
        description,
        root,
    }
}

// ============================================================================
// Title / description extraction
// ============================================================================

fn extract_title(doc: &Html) -> String {
    let selector = Selector::parse("title").expect("`title` is a valid selector");
    doc.select(&selector)
        .next()
        .map(|t| collapse_ws(&t.text().collect::<Vec<_>>().join(" ")))
        .unwrap_or_default()
}

fn extract_description(doc: &Html) -> Option<String> {
    let meta_selector =
        Selector::parse(r#"meta[name="description"]"#).expect("valid selector");
    if let Some(meta) = doc.select(&meta_selector).next() {
        if let Some(content) = meta.value().attr("content") {
            let trimmed = collapse_ws(content);
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    let p_selector = Selector::parse("body p").expect("valid selector");
    if let Some(p) = doc.select(&p_selector).next() {
        let text = collapse_ws(&p.text().collect::<String>());
        if text.is_empty() {
            return None;
        }
        return Some(first_sentence(&text));
    }
    None
}

fn first_sentence(text: &str) -> String {
    let mut end = text.len();
    for (i, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            end = (i + ch.len_utf8()).min(text.len());
            break;
        }
    }
    text[..end].trim().to_owned()
}

// ============================================================================
// Builder internals
// ============================================================================

struct TreeBuilder {
    /// Stack of open sections; index 0 is the root, deeper indices are nested
    /// children currently being filled.
    stack: Vec<TreeNode>,
    /// Per-parent path -> sibling-slug -> count, used to disambiguate slug
    /// collisions.
    slug_counts: HashMap<String, HashMap<String, u32>>,
}

impl TreeBuilder {
    fn new() -> Self {
        Self {
            stack: vec![TreeNode {
                path: "/".into(),
                slug: String::new(),
                heading: None,
                level: 0,
                summary: String::new(),
                intro: String::new(),
                byte_count: 0,
                child_count: 0,
                children: Vec::new(),
            }],
            slug_counts: HashMap::new(),
        }
    }

    fn append_text(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let top = self
            .stack
            .last_mut()
            .expect("stack always has at least the root");
        if !top.intro.is_empty() {
            top.intro.push(' ');
        }
        top.intro.push_str(trimmed);
    }

    fn open_section(&mut self, level: u8, heading: String) {
        // Pop until top.level < level.
        while self.stack.len() > 1 {
            let top_level = self.stack.last().expect("non-empty").level;
            if top_level < level {
                break;
            }
            self.close_top();
        }
        let parent_path = self.stack.last().expect("non-empty").path.clone();
        let base_slug = slugify(&heading);
        let slug = self.unique_slug(&parent_path, &base_slug);
        let path = if parent_path == "/" {
            format!("/{slug}")
        } else {
            format!("{parent_path}/{slug}")
        };
        self.stack.push(TreeNode {
            path,
            slug,
            heading: Some(heading),
            level,
            summary: String::new(),
            intro: String::new(),
            byte_count: 0,
            child_count: 0,
            children: Vec::new(),
        });
    }

    fn close_top(&mut self) {
        if self.stack.len() <= 1 {
            return;
        }
        let mut node = self.stack.pop().expect("checked > 1");
        finalize_node(&mut node);
        self.stack
            .last_mut()
            .expect("still non-empty after pop")
            .children
            .push(node);
    }

    fn unique_slug(&mut self, parent_path: &str, base: &str) -> String {
        // Delegate to the free function so [`crate::actions`] can share it.
        crate::tree::unique_slug(&mut self.slug_counts, parent_path, base)
    }

    fn into_root(mut self) -> TreeNode {
        while self.stack.len() > 1 {
            self.close_top();
        }
        let mut root = self.stack.pop().expect("root always present");
        // `intro` on the root is preamble before the first heading — keep it
        // available via `cat /`. Refinalize so byte_count/child_count are set.
        finalize_node(&mut root);
        root
    }
}

fn walk_node(node: ego_tree::NodeRef<'_, Node>, builder: &mut TreeBuilder) {
    match node.value() {
        Node::Text(t) => {
            builder.append_text(t);
        }
        Node::Element(el) => {
            let tag = el.name();
            if matches!(tag, "script" | "style" | "noscript" | "template") {
                return;
            }
            // Skip semantic landmarks that are *navigation*, not prose. A
            // `<nav>` is unambiguously nav-shaped; `<header>` and `<footer>`
            // when used as page chrome are too. Walking them when building
            // the prose tree pulls "Home / Pricing / Sign in" into the
            // first section's intro, which is noise for an agent. The
            // action graph (`crate::actions`) still walks them — links and
            // buttons in headers/footers/nav are interactive and worth
            // keeping; only the *intro text* extraction skips them.
            //
            // We're conservative on `<header>` and `<footer>`: skip them
            // only at the top level (direct children of `<body>`), so an
            // `<article>` with its own `<header>` (a real per-article
            // intro) still contributes.
            if tag == "nav" {
                return;
            }
            if matches!(tag, "header" | "footer")
                && node
                    .parent()
                    .and_then(|p| p.value().as_element())
                    .is_some_and(|p| p.name() == "body")
            {
                return;
            }
            if let Some(level) = heading_level(tag) {
                if let Some(elem) = ElementRef::wrap(node) {
                    // Join text nodes with a space so inline children
                    // (`<br>`, `<span>`, `<em>`, ...) don't smash adjacent
                    // words together. `collapse_ws` then squashes any
                    // resulting whitespace runs.
                    let heading =
                        collapse_ws(&elem.text().collect::<Vec<_>>().join(" "));
                    if !heading.is_empty() {
                        builder.open_section(level, heading);
                        return;
                    }
                }
            }
            for child in node.children() {
                walk_node(child, builder);
            }
        }
        _ => {}
    }
}

fn heading_level(tag: &str) -> Option<u8> {
    match tag {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn finalize_node(node: &mut TreeNode) {
    node.intro = collapse_ws(&node.intro);
    node.byte_count = node.intro.len();
    node.child_count = node.children.len();
    node.summary = build_summary(node);
}

fn build_summary(node: &TreeNode) -> String {
    let head = node.heading.clone().unwrap_or_default();
    let intro_snippet: String = node.intro.chars().take(100).collect();
    let suffix = if node.intro.chars().count() > 100 {
        "…"
    } else {
        ""
    };
    match (head.is_empty(), intro_snippet.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("{intro_snippet}{suffix}"),
        (false, true) => head,
        (false, false) => format!("{head} — {intro_snippet}{suffix}"),
    }
}

// ============================================================================
// Slug + whitespace helpers
// ============================================================================

/// Disambiguate `base` against siblings already seen under `parent_path`.
/// First occurrence keeps `base`; subsequent occurrences get `-2`, `-3`, ...
/// Shared with [`crate::actions`] so the action graph's section paths use
/// the same slug arithmetic the tree does.
pub(crate) fn unique_slug(
    counts: &mut HashMap<String, HashMap<String, u32>>,
    parent_path: &str,
    base: &str,
) -> String {
    let counter = counts
        .entry(parent_path.to_owned())
        .or_default()
        .entry(base.to_owned())
        .or_insert(0);
    *counter += 1;
    if *counter == 1 {
        base.to_owned()
    } else {
        format!("{base}-{}", *counter)
    }
}

pub(crate) fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_dash = true;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 40 {
        out.truncate(40);
        while out.ends_with('-') {
            out.pop();
        }
    }
    if out.is_empty() {
        "section".to_owned()
    } else {
        out
    }
}

pub(crate) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_url() -> Url {
        Url::parse("https://example.com/").unwrap()
    }

    #[test]
    fn builds_a_simple_two_level_tree() {
        let html = r#"
            <html>
              <head>
                <title>My Page</title>
                <meta name="description" content="A test page about agents.">
              </head>
              <body>
                <p>Intro paragraph.</p>
                <h1>Features</h1>
                <p>Top features blurb.</p>
                <h2>Pixel Awareness</h2>
                <p>It sees pixels.</p>
                <h2>Smart Caching</h2>
                <p>It caches smart.</p>
                <h1>Pricing</h1>
                <p>Free for now.</p>
              </body>
            </html>
        "#;
        let tree = build_tree(html, &dummy_url());
        assert_eq!(tree.title, "My Page");
        assert_eq!(
            tree.description.as_deref(),
            Some("A test page about agents.")
        );
        assert_eq!(tree.root.children.len(), 2, "expected 2 top-level sections");

        let features = &tree.root.children[0];
        assert_eq!(features.slug, "features");
        assert_eq!(features.path, "/features");
        assert_eq!(features.level, 1);
        assert!(features.intro.contains("Top features blurb"));
        assert_eq!(features.children.len(), 2);

        let pixel = &features.children[0];
        assert_eq!(pixel.path, "/features/pixel-awareness");
        assert_eq!(pixel.heading.as_deref(), Some("Pixel Awareness"));
        assert!(pixel.intro.contains("It sees pixels"));

        let pricing = &tree.root.children[1];
        assert_eq!(pricing.slug, "pricing");
        assert!(pricing.intro.contains("Free for now"));
    }

    #[test]
    fn ls_and_cat_walk_the_tree() {
        let html = r#"
            <html><body>
              <h1>Alpha</h1><p>aaa</p>
              <h2>Beta</h2><p>bbb</p>
              <h1>Gamma</h1><p>ccc</p>
            </body></html>
        "#;
        let tree = build_tree(html, &dummy_url());

        let root_rows = tree.ls("/").unwrap();
        assert_eq!(root_rows.len(), 2);
        assert_eq!(root_rows[0].slug, "alpha");
        assert_eq!(root_rows[1].slug, "gamma");

        let alpha_rows = tree.ls("/alpha").unwrap();
        assert_eq!(alpha_rows.len(), 1);
        assert_eq!(alpha_rows[0].slug, "beta");

        let beta_text = tree.cat("/alpha/beta").unwrap();
        assert_eq!(beta_text, "bbb");

        let pwd = tree.pwd();
        assert_eq!(pwd.top_level, vec!["alpha".to_owned(), "gamma".to_owned()]);
    }

    #[test]
    fn slug_collisions_get_numeric_suffix() {
        let html = r#"
            <html><body>
              <h1>Notes</h1><p>one</p>
              <h1>Notes</h1><p>two</p>
              <h1>Notes</h1><p>three</p>
            </body></html>
        "#;
        let tree = build_tree(html, &dummy_url());
        let rows = tree.ls("/").unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].slug, "notes");
        assert_eq!(rows[1].slug, "notes-2");
        assert_eq!(rows[2].slug, "notes-3");
    }

    #[test]
    fn skips_script_style_noscript_template() {
        let html = r#"
            <html><body>
              <h1>Hidden</h1>
              <script>var leaked='no'</script>
              <style>body{color:red}</style>
              <noscript>fallback</noscript>
              <template>tpl</template>
              <p>visible text</p>
            </body></html>
        "#;
        let tree = build_tree(html, &dummy_url());
        let intro = tree.cat("/hidden").unwrap();
        assert!(intro.contains("visible text"));
        assert!(!intro.contains("leaked"));
        assert!(!intro.contains("color:red"));
        assert!(!intro.contains("fallback"));
        assert!(!intro.contains("tpl"));
    }

    #[test]
    fn description_falls_back_to_first_sentence_of_first_paragraph() {
        let html = r#"
            <html><head><title>T</title></head><body>
              <p>This is the first sentence. And another one.</p>
              <h1>Heading</h1><p>body</p>
            </body></html>
        "#;
        let tree = build_tree(html, &dummy_url());
        assert_eq!(
            tree.description.as_deref(),
            Some("This is the first sentence.")
        );
    }

    #[test]
    fn cat_root_returns_preamble() {
        let html = r#"
            <html><body>
              <p>preamble bit</p>
              <h1>S</h1><p>section</p>
            </body></html>
        "#;
        let tree = build_tree(html, &dummy_url());
        assert!(tree.cat("/").unwrap().contains("preamble bit"));
    }

    #[test]
    fn not_found_path_errors() {
        let tree = build_tree("<html><body><h1>x</h1></body></html>", &dummy_url());
        let err = tree.cat("/does-not-exist").unwrap_err();
        match err {
            TreeError::NotFound(p) => assert_eq!(p, "/does-not-exist"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn bad_path_rejected() {
        let tree = build_tree("<html><body></body></html>", &dummy_url());
        let err = tree.cat("relative/path").unwrap_err();
        assert!(matches!(err, TreeError::BadPath(_, _)));
    }

    #[test]
    fn slug_caps_at_forty_chars() {
        let long = "h".repeat(80);
        let html = format!("<html><body><h1>{long}</h1><p>x</p></body></html>");
        let tree = build_tree(&html, &dummy_url());
        assert_eq!(tree.root.children[0].slug.len(), 40);
    }

    #[test]
    fn skips_top_level_nav_header_footer_in_intro() {
        // Regression for V0 cartography stress test (rust-lang.org): every
        // page's `<header>`/`<nav>` was bleeding into the intro as the
        // navigation menu ("Home Install Learn Tools ...") because the
        // walker treated all body children equally. Fix: skip `<nav>`
        // outright and skip `<header>`/`<footer>` when they're direct
        // children of `<body>`. Per-article `<header>` (e.g. inside an
        // `<article>`) is preserved.
        let html = r#"
            <html><body>
              <header>
                <nav><a href="/">Home</a> <a href="/about">About</a></nav>
              </header>
              <main>
                <p>Welcome to the site.</p>
                <h1>Real content</h1>
                <article>
                  <header><p>Article subtitle here.</p></header>
                  <p>The real body of the article.</p>
                </article>
              </main>
              <footer>Copyright 2026</footer>
            </body></html>
        "#;
        let tree = build_tree(html, &dummy_url());
        // Preamble (root intro) should be the welcome line, NOT the nav.
        assert!(
            tree.root.intro.contains("Welcome to the site"),
            "preamble missing real content: {:?}",
            tree.root.intro,
        );
        assert!(
            !tree.root.intro.contains("Home"),
            "nav leaked into preamble: {:?}",
            tree.root.intro,
        );
        assert!(
            !tree.root.intro.contains("Copyright"),
            "top-level footer leaked: {:?}",
            tree.root.intro,
        );
        // Per-article header (`<header>` inside `<article>`) is NOT skipped.
        let h1 = &tree.root.children[0];
        assert!(
            h1.intro.contains("Article subtitle here"),
            "nested article header was wrongly skipped: {:?}",
            h1.intro,
        );
        assert!(h1.intro.contains("The real body"));
    }

    #[test]
    fn heading_text_with_inline_children_keeps_word_boundaries() {
        // Regression: `<h1>Workplace AI<br>Shouldn't Be Hard</h1>` was
        // slugifying to `workplace-aishouldn-t-be-hard` because adjacent
        // text nodes collected without a separator. Fix joins with " ".
        let html = r#"
            <html><body>
              <h1>Workplace AI<br>Shouldn't Be Hard</h1>
              <p>body</p>
            </body></html>
        "#;
        let tree = build_tree(html, &dummy_url());
        let only = &tree.root.children[0];
        assert_eq!(
            only.heading.as_deref(),
            Some("Workplace AI Shouldn't Be Hard"),
        );
        assert_eq!(only.slug, "workplace-ai-shouldn-t-be-hard");

        // Also covers nested inline like `<em>` and `<span>`.
        let html2 = r#"<html><body><h1>Hello <em>brave</em> world</h1></body></html>"#;
        let tree2 = build_tree(html2, &dummy_url());
        assert_eq!(
            tree2.root.children[0].heading.as_deref(),
            Some("Hello brave world"),
        );
        assert_eq!(tree2.root.children[0].slug, "hello-brave-world");
    }

    #[test]
    fn deep_nesting_pops_correctly() {
        let html = r#"
            <html><body>
              <h1>A</h1>
                <h2>A1</h2>
                  <h3>A1a</h3>
              <h1>B</h1>
            </body></html>
        "#;
        let tree = build_tree(html, &dummy_url());
        assert_eq!(tree.root.children.len(), 2);
        let a = &tree.root.children[0];
        assert_eq!(a.slug, "a");
        assert_eq!(a.children.len(), 1);
        let a1 = &a.children[0];
        assert_eq!(a1.slug, "a1");
        assert_eq!(a1.children.len(), 1);
        assert_eq!(a1.children[0].slug, "a1a");
        assert_eq!(tree.root.children[1].slug, "b");
    }
}
