//! # actions
//!
//! Action graph — every interactive element on the page (links, buttons,
//! inputs, forms) gets a stable `@e0/@e1/...` ref the agent can name in
//! primitives like `cat @e7` or `click @e3`. This is installment **#2** of
//! the engine-as-semantic-extractor program (ADR 0013); installment #1
//! was structured metadata ([`crate::metadata`]).
//!
//! ## What we extract
//!
//! - **Links** (`<a href>`) → role `link`. Name is text content or
//!   `aria-label`. `href` preserved in attrs.
//! - **Buttons** (`<button>`, `<input type="submit|button|reset|image|file">`,
//!   `[role="button"]`) → role `button`.
//! - **Text inputs** (`<input>` with text-flavored type, default text, or
//!   `<textarea>`) → role `textbox`.
//! - **Checkboxes / radios** → role `checkbox` / `radio`.
//! - **`<select>`** → role `combobox`.
//! - **`<form>`** → role `form`. `action` + `method` preserved.
//!
//! Hidden inputs (`<input type="hidden">`) are skipped — they're not
//! agent-actionable.
//!
//! ## Ref stability
//!
//! Refs are assigned in **document order**, so the first interactive
//! element in DOM source order is `@e0`. They are **stable within a
//! single fetch** but may shift across fetches if elements are inserted
//! or removed earlier in the document.
//!
//! Cross-fetch stability via content addressing is future work. Callers
//! that need a re-fetch should re-resolve refs by `(role, name, section)`
//! instead of relying on the numeric id.
//!
//! ## Section path
//!
//! Each element carries the heading-tree path of its enclosing section
//! (e.g. `/pricing` for an element under an `<h1>Pricing</h1>`). The
//! algorithm reuses [`crate::tree`]'s slug + collision logic so refs and
//! tree paths share one vocabulary — `heso cat /pricing` and the elements
//! whose `section == "/pricing"` agree exactly.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use scraper::{ElementRef as ScraperElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};

use crate::tree::{collapse_ws, join_with_space, slugify, unique_slug};

// Compiled once per process; `extract` ran `Selector::parse("body")` per
// page fetch before.
static BODY_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("body").expect("valid"));

// ============================================================================
// Types
// ============================================================================

/// One interactive element in the page's action graph.
///
/// Serializes with `ref` (not `ref_id`) as the field name so the LLM-facing
/// JSON matches the shell vocabulary the rest of heso uses (`heso cat
/// @e7`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementRef {
    /// Stable id, e.g. `"@e0"`, `"@e1"`, …
    #[serde(rename = "ref")]
    pub ref_id: String,
    /// Computed ARIA role: `link`, `button`, `textbox`, `checkbox`,
    /// `radio`, `combobox`, `form`. Explicit `role=` attribute wins;
    /// otherwise derived from tag + `type`.
    pub role: String,
    /// Lowercase HTML tag name (`a`, `button`, `input`, `textarea`,
    /// `select`, `form`).
    pub tag: String,
    /// Accessible name. Order of precedence: `aria-label` →
    /// (inputs) `placeholder` / button `value` / `name` → text content →
    /// `alt` → `title`. `None` if nothing identifying was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Heading-tree path of the section this element is in (`"/"` if
    /// before the first heading). Matches the same paths
    /// [`crate::tree`] hands out.
    pub section: String,
    /// Selected attributes preserved verbatim — `href`, `type`, `name`,
    /// `value`, `placeholder`, `required`, `alt`, `title`, `action`,
    /// `method`, `id`, `target`, `rel`, plus any `aria-*`. Sorted (it's a
    /// `BTreeMap`) so JSON serialization is deterministic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, String>,
}

// ============================================================================
// Public API
// ============================================================================

/// Walk `doc` and produce the page's action graph in document order.
pub fn extract(doc: &Html) -> Vec<ElementRef> {
    let mut state = WalkState::default();
    if let Some(body) = doc.select(&BODY_SEL).next() {
        for child in body.children() {
            walk(child, &mut state);
        }
    } else {
        for child in doc.root_element().children() {
            walk(child, &mut state);
        }
    }
    state.refs
}

/// Look up an element by its `@eN` ref. `None` if not found.
pub fn resolve<'a>(refs: &'a [ElementRef], ref_str: &str) -> Option<&'a ElementRef> {
    refs.iter().find(|r| r.ref_id == ref_str)
}

/// Filter the action graph by any combination of role / name-substring /
/// section. `name_substr` matches case-insensitively against the element's
/// accessible `name`. `section` is matched as a path prefix — passing
/// `/pricing` returns everything under `/pricing` and below.
pub fn filter<'a>(
    refs: &'a [ElementRef],
    role: Option<&str>,
    name_substr: Option<&str>,
    section: Option<&str>,
) -> Vec<&'a ElementRef> {
    let needle = name_substr.map(|s| s.to_lowercase());
    refs.iter()
        .filter(|r| {
            if let Some(want_role) = role {
                if r.role != want_role {
                    return false;
                }
            }
            if let Some(ref n) = needle {
                let have = r.name.as_deref().unwrap_or("").to_lowercase();
                if !have.contains(n) {
                    return false;
                }
            }
            if let Some(want_section) = section {
                let want_section = want_section.trim_end_matches('/');
                let want_with_slash = format!("{want_section}/");
                if r.section != want_section && !r.section.starts_with(&want_with_slash) {
                    return false;
                }
            }
            true
        })
        .collect()
}

// ============================================================================
// Walk
// ============================================================================

#[derive(Default)]
struct WalkState {
    refs: Vec<ElementRef>,
    /// `(heading_level, full_path)` for each section currently open.
    heading_stack: Vec<(u8, String)>,
    /// Same shape as [`crate::tree::TreeBuilder::slug_counts`] — shared
    /// so the section paths we mint match the tree's paths exactly.
    slug_counts: HashMap<String, HashMap<String, u32>>,
    counter: usize,
}

fn walk(node: ego_tree::NodeRef<'_, Node>, state: &mut WalkState) {
    let Node::Element(el) = node.value() else {
        return;
    };
    let tag = el.name();
    if matches!(tag, "script" | "style" | "noscript" | "template") {
        return;
    }

    // Headings update the section stack BEFORE we recurse, so any
    // interactive descendants get attributed to the new section.
    if let Some(level) = heading_level(tag) {
        if let Some(elem) = ScraperElementRef::wrap(node) {
            let heading_text = collapse_ws(&join_with_space(elem.text()));
            if !heading_text.is_empty() {
                open_section(state, level, &heading_text);
            }
        }
    }

    // Interactive element?
    if let Some(elem) = ScraperElementRef::wrap(node) {
        if let Some(role) = compute_role(&elem) {
            let ref_id = format!("@e{}", state.counter);
            state.counter += 1;
            state
                .refs
                .push(build_element_ref(&elem, role, ref_id, state));
        }
    }

    for child in node.children() {
        walk(child, state);
    }
}

fn open_section(state: &mut WalkState, level: u8, text: &str) {
    // Pop until the open stack's top level is strictly less than `level`.
    while let Some(&(lvl, _)) = state.heading_stack.last() {
        if lvl < level {
            break;
        }
        state.heading_stack.pop();
    }
    let parent_path = state
        .heading_stack
        .last()
        .map(|(_, p)| p.clone())
        .unwrap_or_else(|| "/".to_owned());
    let base = slugify(text);
    let slug = unique_slug(&mut state.slug_counts, &parent_path, &base);
    let path = if parent_path == "/" {
        format!("/{slug}")
    } else {
        format!("{parent_path}/{slug}")
    };
    state.heading_stack.push((level, path));
}

fn current_section(state: &WalkState) -> String {
    state
        .heading_stack
        .last()
        .map(|(_, p)| p.clone())
        .unwrap_or_else(|| "/".to_owned())
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

// ============================================================================
// Role detection
// ============================================================================

/// Compute the ARIA role for an element, or `None` if the element is not
/// agent-interactive. Explicit `role=` wins for the roles we care about;
/// otherwise derive from tag + `type`.
fn compute_role(el: &ScraperElementRef) -> Option<&'static str> {
    // Explicit role first — but only honor roles we actually surface, to
    // avoid leaking ARIA exotica (region, complementary, ...) the agent
    // can't act on. Compare case-insensitively without allocating the
    // lowercase intermediate.
    if let Some(r) = el.value().attr("role") {
        let r = r.trim();
        if r.eq_ignore_ascii_case("link") {
            return Some("link");
        } else if r.eq_ignore_ascii_case("button") {
            return Some("button");
        } else if r.eq_ignore_ascii_case("textbox") || r.eq_ignore_ascii_case("searchbox") {
            return Some("textbox");
        } else if r.eq_ignore_ascii_case("checkbox") {
            return Some("checkbox");
        } else if r.eq_ignore_ascii_case("radio") {
            return Some("radio");
        } else if r.eq_ignore_ascii_case("combobox") || r.eq_ignore_ascii_case("listbox") {
            return Some("combobox");
        } else if r.eq_ignore_ascii_case("form") {
            return Some("form");
        }
        // unrecognised role → fall through to implicit role from tag
    }

    let tag = el.value().name();
    match tag {
        "a" => {
            // Even <a> without href is a link semantically (anchor target).
            // Most useful are href links; we include both.
            Some("link")
        }
        "button" => Some("button"),
        "input" => {
            // Compare without allocating; HTML type attribute is ASCII.
            let t = el
                .value()
                .attr("type")
                .map(|s| s.trim())
                .unwrap_or("text");
            if t.eq_ignore_ascii_case("hidden") {
                None
            } else if t.eq_ignore_ascii_case("submit")
                || t.eq_ignore_ascii_case("button")
                || t.eq_ignore_ascii_case("reset")
                || t.eq_ignore_ascii_case("image")
                || t.eq_ignore_ascii_case("file")
            {
                Some("button")
            } else if t.eq_ignore_ascii_case("checkbox") {
                Some("checkbox")
            } else if t.eq_ignore_ascii_case("radio") {
                Some("radio")
            } else {
                // Everything else (text, email, search, tel, url, password,
                // date, time, datetime-local, month, week, color, number,
                // range, plus unknown values) is a textbox-shaped input.
                Some("textbox")
            }
        }
        "textarea" => Some("textbox"),
        "select" => Some("combobox"),
        "form" => Some("form"),
        _ => None,
    }
}

// ============================================================================
// Name + attrs
// ============================================================================

fn compute_name(el: &ScraperElementRef) -> Option<String> {
    // 1. aria-label wins.
    if let Some(label) = el.value().attr("aria-label") {
        let t = collapse_ws(label);
        if !t.is_empty() {
            return Some(t);
        }
    }

    let tag = el.value().name();

    // 2. For form controls, prefer placeholder / value / name / alt over
    //    text content (text content for an <input> is usually empty
    //    anyway).
    if matches!(tag, "input" | "textarea" | "select") {
        // input type="image" name from alt
        if let Some(a) = el.value().attr("alt") {
            let t = collapse_ws(a);
            if !t.is_empty() {
                return Some(t);
            }
        }
        // Submit/button: value is the visible label.
        if matches!(el.value().attr("type"), Some("submit" | "button" | "reset")) {
            if let Some(v) = el.value().attr("value") {
                let t = collapse_ws(v);
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
        if let Some(p) = el.value().attr("placeholder") {
            let t = collapse_ws(p);
            if !t.is_empty() {
                return Some(t);
            }
        }
        if let Some(n) = el.value().attr("name") {
            let t = collapse_ws(n);
            if !t.is_empty() {
                return Some(t);
            }
        }
    }

    // 3. Text content — the natural label for links, buttons, forms.
    let text = collapse_ws(&join_with_space(el.text()));
    if !text.is_empty() {
        // Cap length so a button containing a whole paragraph of nested
        // content doesn't blow up the JSON. 120 chars is plenty for an
        // accessible name.
        let trimmed = if text.chars().count() > 120 {
            let mut s: String = text.chars().take(120).collect();
            s.push('…');
            s
        } else {
            text
        };
        return Some(trimmed);
    }

    // 4. title attribute as last resort.
    if let Some(t) = el.value().attr("title") {
        let s = collapse_ws(t);
        if !s.is_empty() {
            return Some(s);
        }
    }

    None
}

fn pick_relevant_attrs(el: &ScraperElementRef) -> BTreeMap<String, String> {
    // Single pass over the element's attrs: dispatch into the BTreeMap if
    // the attribute is in our keep-list OR is `aria-*` (excluding the
    // already-surfaced `aria-label`). Replaces 22 `elv.attr(k)` lookups
    // plus a second pass for aria-* with one walk of the underlying
    // attr map.
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in el.value().attrs() {
        let keep = matches!(
            k,
            "href"
                | "type"
                | "name"
                | "value"
                | "placeholder"
                | "required"
                | "alt"
                | "title"
                | "action"
                | "method"
                | "id"
                | "target"
                | "rel"
                | "checked"
                | "disabled"
                | "readonly"
                | "max"
                | "min"
                | "step"
                | "pattern"
                | "multiple"
                | "for"
        ) || (k.starts_with("aria-") && k != "aria-label");
        if !keep {
            continue;
        }
        let t = collapse_ws(v);
        if !t.is_empty() {
            out.insert(k.to_owned(), t);
        }
    }
    out
}

fn build_element_ref(
    el: &ScraperElementRef,
    role: &'static str,
    ref_id: String,
    state: &WalkState,
) -> ElementRef {
    ElementRef {
        ref_id,
        role: role.to_owned(),
        tag: el.value().name().to_owned(),
        name: compute_name(el),
        section: current_section(state),
        attrs: pick_relevant_attrs(el),
    }
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
    fn extracts_link_button_input_in_document_order() {
        let html = r#"
            <html><body>
              <h1>Welcome</h1>
              <a href="/pricing">Pricing</a>
              <button>Get started</button>
              <input type="email" placeholder="you@example.com" name="email">
            </body></html>
        "#;
        let refs = extract(&parse(html));
        assert_eq!(refs.len(), 3);

        assert_eq!(refs[0].ref_id, "@e0");
        assert_eq!(refs[0].role, "link");
        assert_eq!(refs[0].tag, "a");
        assert_eq!(refs[0].name.as_deref(), Some("Pricing"));
        assert_eq!(refs[0].section, "/welcome");
        assert_eq!(
            refs[0].attrs.get("href").map(String::as_str),
            Some("/pricing")
        );

        assert_eq!(refs[1].ref_id, "@e1");
        assert_eq!(refs[1].role, "button");
        assert_eq!(refs[1].name.as_deref(), Some("Get started"));

        assert_eq!(refs[2].ref_id, "@e2");
        assert_eq!(refs[2].role, "textbox");
        assert_eq!(refs[2].tag, "input");
        // Placeholder wins over `name` for the accessible label here.
        assert_eq!(refs[2].name.as_deref(), Some("you@example.com"));
        assert_eq!(refs[2].attrs.get("type").map(String::as_str), Some("email"));
        assert_eq!(refs[2].attrs.get("name").map(String::as_str), Some("email"));
    }

    #[test]
    fn input_types_map_to_correct_roles() {
        let html = r#"
            <html><body>
              <input type="text" name="t">
              <input type="email" name="e">
              <input type="password" name="p">
              <input type="checkbox" name="c">
              <input type="radio" name="r">
              <input type="submit" value="Go">
              <input type="button" value="Click">
              <input type="reset" value="Clear">
              <input type="file" name="f">
              <input type="image" alt="logo">
              <input type="hidden" name="h">
              <input name="default">
              <textarea name="ta"></textarea>
              <select name="sel"><option>1</option></select>
            </body></html>
        "#;
        let refs = extract(&parse(html));
        // Hidden is filtered → 13 visible.
        assert_eq!(refs.len(), 13);
        let roles: Vec<&str> = refs.iter().map(|r| r.role.as_str()).collect();
        assert_eq!(
            roles,
            vec![
                "textbox", "textbox", "textbox", "checkbox", "radio", "button", "button", "button",
                "button", "button",
                // type="hidden" skipped
                "textbox",  // <input> with no type → text → textbox
                "textbox",  // <textarea>
                "combobox", // <select>
            ]
        );
        assert!(refs.iter().all(|r| !r.attrs.contains_key("type")
            || r.attrs.get("type").map(String::as_str) != Some("hidden")));
    }

    #[test]
    fn aria_label_wins_over_text_content() {
        let html = r#"
            <html><body>
              <a href="/x" aria-label="Close dialog">×</a>
              <button>Plain text label</button>
            </body></html>
        "#;
        let refs = extract(&parse(html));
        assert_eq!(refs[0].name.as_deref(), Some("Close dialog"));
        assert_eq!(refs[1].name.as_deref(), Some("Plain text label"));
    }

    #[test]
    fn section_path_tracks_heading_stack() {
        let html = r#"
            <html><body>
              <a href="/top">top-level link</a>
              <h1>Features</h1>
              <a href="/f1">f1</a>
              <h2>Caching</h2>
              <a href="/cache">cache</a>
              <h1>Pricing</h1>
              <a href="/p">price</a>
            </body></html>
        "#;
        let refs = extract(&parse(html));
        assert_eq!(refs.len(), 4);
        assert_eq!(refs[0].section, "/"); // before first heading
        assert_eq!(refs[1].section, "/features");
        assert_eq!(refs[2].section, "/features/caching");
        assert_eq!(refs[3].section, "/pricing");
    }

    #[test]
    fn explicit_role_overrides_tag() {
        let html = r#"
            <html><body>
              <div role="button">Fake button</div>
              <span role="link">Fake link</span>
            </body></html>
        "#;
        let refs = extract(&parse(html));
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].role, "button");
        assert_eq!(refs[0].tag, "div");
        assert_eq!(refs[1].role, "link");
    }

    #[test]
    fn form_carries_action_and_method() {
        let html = r#"
            <html><body>
              <form action="/search" method="get">
                <input type="search" name="q" placeholder="Search">
                <button type="submit">Go</button>
              </form>
            </body></html>
        "#;
        let refs = extract(&parse(html));
        // form, input, button — three refs.
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].role, "form");
        assert_eq!(
            refs[0].attrs.get("action").map(String::as_str),
            Some("/search")
        );
        assert_eq!(refs[0].attrs.get("method").map(String::as_str), Some("get"));
        assert_eq!(refs[1].role, "textbox");
        assert_eq!(refs[2].role, "button");
    }

    #[test]
    fn filter_by_role_and_name() {
        let html = r#"
            <html><body>
              <a href="/a">About</a>
              <a href="/p">Pricing</a>
              <button>Get started</button>
              <button>About us</button>
            </body></html>
        "#;
        let refs = extract(&parse(html));
        let links = filter(&refs, Some("link"), None, None);
        assert_eq!(links.len(), 2);
        let about = filter(&refs, None, Some("about"), None);
        assert_eq!(about.len(), 2); // "About" link + "About us" button
        let about_links = filter(&refs, Some("link"), Some("about"), None);
        assert_eq!(about_links.len(), 1);
        assert_eq!(about_links[0].name.as_deref(), Some("About"));
    }

    /// Regression test for AGENT_FINDINGS_V3.md "NEW MINOR BUG — `heso find
    /// --name <regex>` is not substring-matching". The V3 agent reported
    /// that `heso find --role link --name "comment"` on news.ycombinator.com
    /// returned `count: 0` even though 29 anchors with names like
    /// `"35 comments"` / `"31 comments"` were present in the unfiltered set.
    ///
    /// Empirically the bug did not reproduce against the current binary
    /// (`heso find --role link --name comment` returns 30 matches against a
    /// live HN snapshot — the same `contains`-based logic shipped from the
    /// initial commit). This test locks in the substring semantics so the
    /// `--name` filter cannot silently regress to a stricter (anchored or
    /// regex-full-string) match in a future refactor.
    ///
    /// Four required cases:
    ///   1. Substring match in the middle of the field
    ///   2. Substring match at the start of the field
    ///   3. Substring match at the end of the field
    ///   4. Needle with regex-special chars treated as a literal substring
    #[test]
    fn filter_name_is_substring_not_full_string_anchor() {
        let html = r#"
            <html><body>
              <a href="/c1">35 comments</a>
              <a href="/c2">31 comments</a>
              <a href="/start">comment thread on widgets</a>
              <a href="/end">unsubscribe from comment</a>
              <a href="/punct">v1.0.0 release notes</a>
              <a href="/punct2">go to v1.0.0 page</a>
              <a href="/plus">a+b weighting (literal)</a>
              <a href="/unrelated">About</a>
            </body></html>
        "#;
        let refs = extract(&parse(html));

        // Case 1: substring in the middle of the field. "comment" appears
        // between leading digits and the trailing "s" in "35 comments" /
        // "31 comments". Also matches the start/end variants below — total 4.
        let mid = filter(&refs, None, Some("comment"), None);
        assert_eq!(
            mid.len(),
            4,
            "needle 'comment' should match all four `*comment*` anchors as a substring; got {:?}",
            mid.iter().map(|r| r.name.as_deref().unwrap_or("")).collect::<Vec<_>>()
        );

        // Case 2: substring at the START of the field. "35" is a prefix of
        // "35 comments" — nothing else has it.
        let start = filter(&refs, None, Some("35"), None);
        assert_eq!(start.len(), 1);
        assert_eq!(start[0].name.as_deref(), Some("35 comments"));

        // Case 3: substring at the END of the field. "thread on widgets"
        // closes out exactly one anchor.
        let end = filter(&refs, None, Some("thread on widgets"), None);
        assert_eq!(end.len(), 1);
        assert_eq!(end[0].name.as_deref(), Some("comment thread on widgets"));

        // Case 4: needle contains regex-special characters (`.`, `+`). The
        // current implementation matches as a literal substring — `.` is a
        // literal period, NOT "any character" — so `v1.0.0` matches only the
        // two anchors that literally contain the dotted string, and `a+b`
        // matches the one anchor that literally contains `a+b`. This pins
        // the "no implicit regex interpretation" contract: a future switch
        // to a `Regex::new(needle)` path would have to opt in explicitly.
        let dots = filter(&refs, None, Some("v1.0.0"), None);
        assert_eq!(dots.len(), 2, "literal `.` must match a `.`, not any char");
        let plus = filter(&refs, None, Some("a+b"), None);
        assert_eq!(plus.len(), 1, "literal `+` must match a `+`, not one-or-more");
        // And the negative side of (4): a needle containing `.` should NOT
        // match an anchor that lacks that literal period — i.e. the dot is
        // not behaving as the regex any-char metacharacter.
        let dot_negative = filter(&refs, None, Some("v1.0.x"), None);
        assert_eq!(dot_negative.len(), 0);
    }

    #[test]
    fn filter_by_section_is_path_prefix() {
        let html = r#"
            <html><body>
              <h1>Pricing</h1>
              <a href="/p1">P1</a>
              <h2>Enterprise</h2>
              <a href="/p2">P2</a>
              <h1>Other</h1>
              <a href="/o">O</a>
            </body></html>
        "#;
        let refs = extract(&parse(html));
        let pricing = filter(&refs, None, None, Some("/pricing"));
        // /pricing AND /pricing/enterprise both match the prefix.
        assert_eq!(pricing.len(), 2);
        let enterprise = filter(&refs, None, None, Some("/pricing/enterprise"));
        assert_eq!(enterprise.len(), 1);
    }

    #[test]
    fn resolve_finds_by_ref_id() {
        let html = r#"<html><body><a href="/x">X</a><a href="/y">Y</a></body></html>"#;
        let refs = extract(&parse(html));
        let e1 = resolve(&refs, "@e1").expect("should find @e1");
        assert_eq!(e1.attrs.get("href").map(String::as_str), Some("/y"));
        assert!(resolve(&refs, "@e99").is_none());
    }

    #[test]
    fn script_and_style_subtrees_are_skipped() {
        let html = r#"
            <html><body>
              <script><a href="/script-link">hidden</a></script>
              <noscript><a href="/no-link">also hidden</a></noscript>
              <a href="/visible">visible</a>
            </body></html>
        "#;
        let refs = extract(&parse(html));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name.as_deref(), Some("visible"));
    }

    #[test]
    fn aria_state_attrs_are_preserved() {
        let html = r#"
            <html><body>
              <button aria-expanded="true" aria-pressed="false">Menu</button>
            </body></html>
        "#;
        let refs = extract(&parse(html));
        assert_eq!(
            refs[0].attrs.get("aria-expanded").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            refs[0].attrs.get("aria-pressed").map(String::as_str),
            Some("false")
        );
    }

    #[test]
    fn long_name_is_truncated() {
        let long = "x ".repeat(200);
        let html = format!(r#"<html><body><a href="/">{long}</a></body></html>"#);
        let refs = extract(&parse(&html));
        let name = refs[0].name.as_deref().unwrap();
        assert!(name.chars().count() <= 121); // 120 + the …
        assert!(name.ends_with('…'));
    }
}
