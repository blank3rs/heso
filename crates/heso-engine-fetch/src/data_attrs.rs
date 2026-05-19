//! # data_attrs
//!
//! Extract JSON-shaped hydration data hidden in `data-*` element
//! attributes.
//!
//! Older React (pre-RSC), Vue.js components, Stimulus controllers,
//! Alpine.js widgets, and a long tail of vanilla widgets commonly
//! stash configuration or component props directly on HTML elements
//! via `data-*` attributes:
//!
//! ```html
//! <div data-react-props='{"theme":"dark","items":[1,2,3]}'></div>
//! <button data-controller="modal" data-modal-config='{"size":"lg"}'>...</button>
//! <li data-item='{"id":42,"name":"Alice"}'>...</li>
//! ```
//!
//! The browser sees these as plain strings; the page's JavaScript
//! reads them at hydration time and turns them into component state.
//! Cartography parses them as JSON ahead of time so the agent can see
//! the same payload without running any JS.
//!
//! ## Filters
//!
//! - Only emit values that are **non-empty objects or arrays**.
//!   Trivial single-value `data-id="42"` / `data-toggle="modal"` /
//!   `data-flag="true"` blobs are not "data" in the
//!   hydration-payload sense; emitting them would flood the output
//!   with noise from random Bootstrap/Stimulus controllers.
//! - Skip attributes whose value doesn't parse as strict JSON. Real
//!   hydration data is usually `JSON.stringify`-output and parses
//!   cleanly; non-JSON `data-*` (slugs, IDs, CSS class names) is
//!   correctly rejected.
//!
//! ## Output shape
//!
//! [`BTreeMap`] keyed by the full attribute name including the
//! `data-` prefix (so `data-react-props`, not `react-props`). Values
//! are a [`Vec<DataAttrValue>`] preserving document order — when the
//! same attribute appears on multiple elements (a common
//! list-of-items pattern), all occurrences are captured.
//!
//! Each [`DataAttrValue`] carries the element's `tag` name in
//! addition to the parsed JSON, so an agent can distinguish
//! `data-config` on a `<form>` from `data-config` on a `<button>`.
//!
//! BTreeMap iteration order is stable, which keeps the engine's
//! deterministic-plat property ([`crate::plat`]) intact.

use std::collections::BTreeMap;

use scraper::Html;

/// One occurrence of a recognized `data-*` JSON attribute.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DataAttrValue {
    /// Element tag name (e.g. `"div"`, `"button"`, `"form"`). Always
    /// lowercased by the HTML parser.
    pub tag: String,
    /// Parsed JSON payload — an object or non-empty array.
    pub value: serde_json::Value,
}

/// Extract every `data-*` attribute whose value parses as a
/// non-empty JSON object or array.
///
/// Result is keyed by the attribute name; values are lists of
/// occurrences in document order.
pub fn extract(doc: &Html) -> BTreeMap<String, Vec<DataAttrValue>> {
    let mut out: BTreeMap<String, Vec<DataAttrValue>> = BTreeMap::new();

    // Walk the underlying ego_tree directly — bypasses parsing a `*`
    // selector and running selector matching on every node, which is
    // pure overhead for a universal walk.
    for node in doc.tree.values() {
        let scraper::Node::Element(elem) = node else {
            continue;
        };
        let mut tag: Option<&str> = None;
        for (attr_name, attr_value) in elem.attrs() {
            if !attr_name.starts_with("data-") {
                continue;
            }
            let trimmed = attr_value.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Cheap byte pre-filter: a meaningful payload must be a
            // JSON object or array, i.e. start with `{` or `[`. Real
            // pages (GitHub especially) cover every element with
            // `data-id="42"`, `data-toggle="modal"`, etc. — none of
            // them parse as JSON objects/arrays, but the unfiltered
            // path calls `serde_json::from_str` on every one.
            // Rejecting them by first non-whitespace char is ~free.
            let first = trimmed.as_bytes().first().copied();
            if !matches!(first, Some(b'{') | Some(b'[')) {
                continue;
            }
            let parsed: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !is_meaningful_payload(&parsed) {
                continue;
            }
            let tag_str = *tag.get_or_insert_with(|| elem.name());
            out.entry(attr_name.to_owned())
                .or_default()
                .push(DataAttrValue {
                    tag: tag_str.to_owned(),
                    value: parsed,
                });
        }
    }

    out
}

/// True if the parsed JSON is something an agent would care about:
/// a non-empty object or a non-empty array. Scalars (number, bool,
/// string, null) are rejected — they're attribute values not data
/// payloads.
fn is_meaningful_payload(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(o) => !o.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(html: &str) -> Html {
        Html::parse_document(html)
    }

    #[test]
    fn extracts_simple_react_props() {
        let html = r##"
            <html><body>
              <div data-react-props='{"theme":"dark","count":3}'></div>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let entries = data
            .get("data-react-props")
            .expect("data-react-props should be captured");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tag, "div");
        assert_eq!(entries[0].value["theme"], "dark");
        assert_eq!(entries[0].value["count"], 3);
    }

    #[test]
    fn skips_attrs_with_non_json_values() {
        // Note: raw-string delimiter is `##` (two hashes) because the
        // HTML contains a `"#"` href which would otherwise close a
        // single-hash raw string early.
        let html = r##"
            <html><body>
              <button data-toggle="modal" data-id="42" data-controller="alpine">go</button>
              <a data-method="delete" href="#">remove</a>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert!(
            data.is_empty(),
            "non-JSON data-* values should be skipped, got {:?}",
            data.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn skips_empty_objects_and_arrays() {
        let html = r#"
            <html><body>
              <div data-config='{}'></div>
              <div data-list='[]'></div>
              <div data-real='{"k":"v"}'></div>
            </body></html>
        "#;
        let data = extract(&parse(html));
        assert_eq!(data.len(), 1);
        assert!(data.contains_key("data-real"));
    }

    #[test]
    fn skips_primitive_json_values_even_though_they_parse() {
        // Valid JSON but not "data" — these would be noise.
        let html = r#"
            <html><body>
              <span data-count='42'></span>
              <span data-flag='true'></span>
              <span data-label='"alice"'></span>
              <span data-nothing='null'></span>
            </body></html>
        "#;
        let data = extract(&parse(html));
        assert!(data.is_empty());
    }

    #[test]
    fn groups_repeated_attr_names_in_document_order() {
        let html = r##"
            <html><body>
              <li data-item='{"id":1}'></li>
              <li data-item='{"id":2}'></li>
              <li data-item='{"id":3}'></li>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let entries = data.get("data-item").expect("data-item key present");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].value["id"], 1);
        assert_eq!(entries[1].value["id"], 2);
        assert_eq!(entries[2].value["id"], 3);
    }

    #[test]
    fn ignores_non_data_attributes() {
        // Same JSON-shaped content in non-`data-*` attrs should NOT
        // be captured. We're explicitly scoped to data-* per the
        // HTML5 author convention.
        let html = r##"
            <html><body>
              <div class='{"not":"json-here"}' aria-label='{"also":"not"}'></div>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert!(data.is_empty());
    }

    #[test]
    fn output_keys_are_btreemap_sorted() {
        let html = r##"
            <html><body>
              <div data-zebra='{"a":1}'></div>
              <div data-alpha='{"b":2}'></div>
              <div data-mango='{"c":3}'></div>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let keys: Vec<&String> = data.keys().collect();
        assert_eq!(keys, vec!["data-alpha", "data-mango", "data-zebra"]);
    }

    #[test]
    fn captures_tag_context_when_same_attr_on_different_tags() {
        let html = r##"
            <html><body>
              <button data-x='{"k":1}'></button>
              <form data-x='{"k":2}'></form>
              <article data-x='{"k":3}'></article>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let entries = data.get("data-x").expect("data-x present");
        assert_eq!(entries.len(), 3);
        // Document order is preserved across different tags.
        assert_eq!(entries[0].tag, "button");
        assert_eq!(entries[1].tag, "form");
        assert_eq!(entries[2].tag, "article");
    }

    #[test]
    fn handles_top_level_array_payload() {
        let html = r##"
            <html><body>
              <ul data-items='[{"id":1},{"id":2}]'></ul>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let entries = data.get("data-items").expect("data-items present");
        assert_eq!(entries[0].value.is_array(), true);
        assert_eq!(entries[0].value[0]["id"], 1);
        assert_eq!(entries[0].value[1]["id"], 2);
    }

    #[test]
    fn decodes_html_entity_escaped_json() {
        // Real HTML often uses `&quot;` instead of bare `"` to keep
        // double-quoted attribute syntax valid. scraper decodes the
        // entities when reading the attribute, so by the time we see
        // the value it's plain JSON.
        let html = r#"
            <html><body>
              <div data-x="{&quot;k&quot;:&quot;v&quot;,&quot;n&quot;:42}"></div>
            </body></html>
        "#;
        let data = extract(&parse(html));
        let entries = data.get("data-x").expect("data-x present");
        assert_eq!(entries[0].value["k"], "v");
        assert_eq!(entries[0].value["n"], 42);
    }

    #[test]
    fn empty_page_yields_empty_map() {
        let data = extract(&parse("<html><body></body></html>"));
        assert!(data.is_empty());
    }

    #[test]
    fn nested_structure_preserved() {
        let html = r##"
            <html><body>
              <section data-cfg='{"theme":{"primary":"#fff","fonts":["a","b"]},"flags":{"x":true}}'></section>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let entries = data.get("data-cfg").expect("data-cfg present");
        assert_eq!(entries[0].value["theme"]["primary"], "#fff");
        assert_eq!(entries[0].value["theme"]["fonts"][1], "b");
        assert_eq!(entries[0].value["flags"]["x"], true);
    }

    #[test]
    fn ignores_attribute_named_exactly_data_prefix_only() {
        // `data-` with no name suffix isn't a valid attribute, but
        // be defensive. Real HTML parsers may treat it variously.
        let html = r#"
            <html><body>
              <div data='{"k":"v"}'></div>
            </body></html>
        "#;
        let data = extract(&parse(html));
        // `data` alone isn't `data-*`, so should be skipped.
        assert!(!data.contains_key("data"));
    }

    #[test]
    fn captures_data_attrs_on_deeply_nested_elements() {
        // Sanity: depth doesn't matter, we walk every element.
        let html = r##"
            <html><body>
              <main><section><article><div>
                <p data-deep='{"buried":"deep"}'></p>
              </div></article></section></main>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let entries = data.get("data-deep").expect("data-deep present");
        assert_eq!(entries[0].tag, "p");
        assert_eq!(entries[0].value["buried"], "deep");
    }
}
