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

use std::collections::{BTreeMap, HashMap, HashSet};
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
    /// (form controls) `alt` / submit-button `value` / `placeholder` /
    /// `name` → text content → `title`. `None` if nothing identifying
    /// was found.
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

/// Errors produced by [`resolve_locator`].
#[derive(Debug, thiserror::Error)]
pub enum LocatorError {
    /// The supplied CSS selector did not parse via [`scraper::Selector`].
    #[error("invalid CSS selector `{selector}`: {message}")]
    BadSelector {
        /// The selector string that failed to parse.
        selector: String,
        /// A short human-readable parse error.
        message: String,
    },
}

/// Convenience wrapper for [`resolve_locator`] that takes the raw HTML
/// body instead of a pre-parsed [`Html`]. Owned-result variant: returns
/// fully owned [`ElementRef`]s so the CLI can drop the parse without
/// borrow trouble.
pub fn resolve_locator_from_html(
    html: &str,
    refs: &[ElementRef],
    text: Option<&str>,
    css_selector: Option<&str>,
    aria_label: Option<&str>,
) -> Result<Vec<ElementRef>, LocatorError> {
    let doc = Html::parse_document(html);
    let matches = resolve_locator(&doc, refs, text, css_selector, aria_label)?;
    Ok(matches.into_iter().cloned().collect())
}

/// Resolve a locator to its set of matching [`ElementRef`]s, preserving
/// document order. All three filters are optional; passing `None` for all
/// of them returns an empty `Vec` (the caller should treat that as a
/// usage error — `resolve_locator` is the locator-flag path, not the
/// "list everything" path).
///
/// Matching semantics (mirrors Playwright `get_by_text` / `get_by_role`):
/// - `text`: case-insensitive substring match against the element's
///   accessible `name` (which already prefers `aria-label` →
///   placeholder/value/name → text content → title).
/// - `css_selector`: passed through `scraper::Selector`; a matched
///   element must also be in the action graph (i.e. interactive).
///   Non-interactive selector hits are ignored.
/// - `aria_label`: case-insensitive substring against the element's
///   `aria-label` attribute. Elements without an `aria-label` are
///   skipped.
///
/// When multiple filters are supplied they AND together — an element
/// must satisfy every supplied filter to be returned.
pub fn resolve_locator<'a>(
    doc: &Html,
    refs: &'a [ElementRef],
    text: Option<&str>,
    css_selector: Option<&str>,
    aria_label: Option<&str>,
) -> Result<Vec<&'a ElementRef>, LocatorError> {
    // No filters supplied: locator is empty, so the result is empty.
    if text.is_none() && css_selector.is_none() && aria_label.is_none() {
        return Ok(Vec::new());
    }

    // Pre-compile a scraper selector when one was supplied. Map any
    // parse error into our public LocatorError so the CLI can render a
    // clean message without depending on scraper's error type.
    let compiled_css = match css_selector {
        Some(s) => {
            let parsed = Selector::parse(s).map_err(|e| LocatorError::BadSelector {
                selector: s.to_owned(),
                message: e.to_string(),
            })?;
            Some(parsed)
        }
        None => None,
    };

    // Collect node ids of CSS-selector matches once. We compare each
    // interactive element's ego_tree NodeId against this set as we walk
    // the document. Using NodeId (an integer wrapper) keeps the lookup
    // O(1) and survives the second walk we do for aria-label / text.
    let css_node_ids: Option<HashSet<ego_tree::NodeId>> = compiled_css.as_ref().map(|sel| {
        doc.select(sel)
            .map(|el| el.id())
            .collect::<HashSet<_>>()
    });

    let text_needle = text.map(|s| s.to_lowercase());
    let aria_needle = aria_label.map(|s| s.to_lowercase());

    // Re-walk the doc the same way `extract` does. We need the walk
    // (not just the pre-built `refs` slice) because aria-label is not
    // stored on ElementRef and CSS matching needs each element's
    // NodeId. The walk counter has to match `extract`'s output exactly
    // so we can index back into `refs`.
    let mut probe = LocatorProbe {
        css_node_ids,
        text_needle,
        aria_needle,
        counter: 0,
        matched_indices: Vec::new(),
    };
    if let Some(body) = doc.select(&BODY_SEL).next() {
        for child in body.children() {
            locator_walk(child, &mut probe);
        }
    } else {
        for child in doc.root_element().children() {
            locator_walk(child, &mut probe);
        }
    }

    // Map matched indices back to `&ElementRef` slots. The counter
    // walks interactive elements in document order, exactly as
    // `extract` does, so `refs[idx]` is the matching entry. Guard
    // against the (defensive) case where the caller passed in a `refs`
    // slice from a different parse of the same URL.
    let out: Vec<&'a ElementRef> = probe
        .matched_indices
        .into_iter()
        .filter_map(|idx| refs.get(idx))
        .collect();
    Ok(out)
}

struct LocatorProbe {
    css_node_ids: Option<HashSet<ego_tree::NodeId>>,
    text_needle: Option<String>,
    aria_needle: Option<String>,
    counter: usize,
    matched_indices: Vec<usize>,
}

fn locator_walk(node: ego_tree::NodeRef<'_, Node>, probe: &mut LocatorProbe) {
    let Node::Element(el) = node.value() else {
        return;
    };
    let tag = el.name();
    if matches!(tag, "script" | "style" | "noscript" | "template") {
        return;
    }

    if let Some(elem) = ScraperElementRef::wrap(node) {
        if compute_role(&elem).is_some() {
            let idx = probe.counter;
            probe.counter += 1;

            // Each supplied filter must match. CSS filter: NodeId must
            // appear in the pre-collected set. text: case-insensitive
            // substring against the accessible name. aria-label:
            // case-insensitive substring against the `aria-label`
            // attribute.
            let css_ok = match &probe.css_node_ids {
                None => true,
                Some(ids) => ids.contains(&node.id()),
            };
            let text_ok = match &probe.text_needle {
                None => true,
                Some(needle) => {
                    let have = compute_name(&elem).unwrap_or_default().to_lowercase();
                    have.contains(needle)
                }
            };
            let aria_ok = match &probe.aria_needle {
                None => true,
                Some(needle) => match elem.value().attr("aria-label") {
                    Some(v) => v.to_lowercase().contains(needle),
                    None => false,
                },
            };

            if css_ok && text_ok && aria_ok {
                probe.matched_indices.push(idx);
            }
        }
    }

    for child in node.children() {
        locator_walk(child, probe);
    }
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

/// Path of the section currently on top of the heading stack, or `"/"`
/// when the walker is still before any heading. Returns a borrow so the
/// caller decides whether ownership is needed — earlier versions always
/// cloned, which paid for an allocation per interactive element even
/// though only `build_element_ref` actually needs the owned form.
fn current_section(state: &WalkState) -> &str {
    state
        .heading_stack
        .last()
        .map(|(_, p)| p.as_str())
        .unwrap_or("/")
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
        // `current_section` borrows from `state.heading_stack`; clone here
        // since the field is owned. Same observable behaviour, one less
        // allocation when the walker is still at `/`.
        section: current_section(state).to_owned(),
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

    /// Regression test for agent regression testing "NEW MINOR BUG — `heso find
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

    // ========================================================================
    // resolve_locator — text / CSS-selector / aria-label
    // ========================================================================

    #[test]
    fn locator_text_substring_case_insensitive() {
        let html = r#"
            <html><body>
              <a href="/a">Submit form</a>
              <button>SUBMIT</button>
              <a href="/c">cancel</a>
            </body></html>
        "#;
        let doc = parse(html);
        let refs = extract(&doc);
        let matches =
            resolve_locator(&doc, &refs, Some("submit"), None, None).expect("ok");
        assert_eq!(matches.len(), 2);
        let names: Vec<&str> = matches
            .iter()
            .map(|r| r.name.as_deref().unwrap_or(""))
            .collect();
        assert!(names.contains(&"Submit form"));
        assert!(names.contains(&"SUBMIT"));
    }

    #[test]
    fn locator_text_matches_input_placeholder() {
        let html = r#"
            <html><body>
              <input type="search" name="q" placeholder="Search the web">
              <input type="text" name="other" placeholder="other field">
            </body></html>
        "#;
        let doc = parse(html);
        let refs = extract(&doc);
        let matches =
            resolve_locator(&doc, &refs, Some("search"), None, None).expect("ok");
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].attrs.get("name").map(String::as_str),
            Some("q")
        );
    }

    #[test]
    fn locator_css_selector_filters_to_interactive() {
        let html = r#"
            <html><body>
              <p class="hint">Some hint text</p>
              <input type="search" name="q" placeholder="Search">
              <button class="primary">Go</button>
            </body></html>
        "#;
        let doc = parse(html);
        let refs = extract(&doc);
        // Selector matches the input by `[name=q]`.
        let matches = resolve_locator(&doc, &refs, None, Some("input[name=q]"), None)
            .expect("valid selector");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].tag, "input");

        // A selector that matches a non-interactive element returns
        // zero matches — `.hint` is a <p>.
        let none = resolve_locator(&doc, &refs, None, Some(".hint"), None).expect("ok");
        assert_eq!(none.len(), 0);
    }

    #[test]
    fn locator_css_selector_invalid_returns_error() {
        let html = r#"<html><body><a href="/x">X</a></body></html>"#;
        let doc = parse(html);
        let refs = extract(&doc);
        let err = resolve_locator(&doc, &refs, None, Some(">>> bad <<<"), None)
            .expect_err("should be Err(BadSelector)");
        match err {
            LocatorError::BadSelector { selector, .. } => assert_eq!(selector, ">>> bad <<<"),
        }
    }

    #[test]
    fn locator_aria_label_substring_case_insensitive() {
        let html = r#"
            <html><body>
              <a href="/x" aria-label="Close dialog">×</a>
              <a href="/y" aria-label="Open Menu">≡</a>
              <button>No aria here</button>
            </body></html>
        "#;
        let doc = parse(html);
        let refs = extract(&doc);
        // "menu" substring matches "Open Menu" — case-insensitive.
        let matches = resolve_locator(&doc, &refs, None, None, Some("menu")).expect("ok");
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].attrs.get("href").map(String::as_str),
            Some("/y")
        );
        // Element without an aria-label is filtered out even though
        // its text content contains the needle.
        let none = resolve_locator(&doc, &refs, None, None, Some("no aria")).expect("ok");
        assert_eq!(none.len(), 0);
    }

    #[test]
    fn locator_filters_and_together() {
        let html = r#"
            <html><body>
              <a href="/a" aria-label="Submit query">go</a>
              <button>Submit form</button>
            </body></html>
        "#;
        let doc = parse(html);
        let refs = extract(&doc);
        // text="submit" matches both, aria-label="query" matches only the link.
        let matches =
            resolve_locator(&doc, &refs, Some("submit"), None, Some("query")).expect("ok");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].tag, "a");
    }

    #[test]
    fn locator_empty_filters_returns_empty() {
        let html = r#"<html><body><a href="/x">X</a></body></html>"#;
        let doc = parse(html);
        let refs = extract(&doc);
        let matches = resolve_locator(&doc, &refs, None, None, None).expect("ok");
        assert!(matches.is_empty());
    }

    #[test]
    fn locator_preserves_document_order() {
        let html = r#"
            <html><body>
              <a href="/1">a item</a>
              <a href="/2">a item</a>
              <a href="/3">b item</a>
              <a href="/4">a item</a>
            </body></html>
        "#;
        let doc = parse(html);
        let refs = extract(&doc);
        let matches = resolve_locator(&doc, &refs, Some("a item"), None, None).expect("ok");
        assert_eq!(matches.len(), 3);
        let hrefs: Vec<&str> = matches
            .iter()
            .filter_map(|r| r.attrs.get("href").map(String::as_str))
            .collect();
        assert_eq!(hrefs, vec!["/1", "/2", "/4"]);
    }
}
