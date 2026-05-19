//! Integration tests for the `URL` + `URLSearchParams` globals
//! installed by [`heso_engine_js::url_search_params::install_url`].
//!
//! Pinned behaviors:
//!
//! 1. `new URL(href, base?)` parses absolute and relative URLs and
//!    exposes the WHATWG property surface.
//! 2. `url.searchParams.get/getAll/set/append/has/delete` round-trip
//!    correctly.
//! 3. Mutations on `url.searchParams` reflect back into
//!    `url.toString()` / `url.search` without an explicit sync.
//! 4. `new URLSearchParams(init)` accepts string, iterable, and
//!    record-shaped `init` arguments.
//! 5. `for (const [k,v] of params)` iterates entries in insertion
//!    order.
//! 6. `params.size` matches the number of entries.
//! 7. `params.sort()` stably orders by UTF-16 code-unit key order.

use heso_engine_js::JsEngine;

fn engine() -> JsEngine {
    JsEngine::new().expect("engine new")
}

// ===== URL constructor + canParse =====================================

#[test]
fn url_class_parses_absolute_url() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/foo?bar=1#frag');
            JSON.stringify({
                href: u.href,
                origin: u.origin,
                protocol: u.protocol,
                host: u.host,
                hostname: u.hostname,
                pathname: u.pathname,
                search: u.search,
                hash: u.hash,
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"href\":\"https://example.com/foo?bar=1#frag\""));
    assert!(s.contains("\"origin\":\"https://example.com\""));
    assert!(s.contains("\"protocol\":\"https:\""));
    assert!(s.contains("\"host\":\"example.com\""));
    assert!(s.contains("\"hostname\":\"example.com\""));
    assert!(s.contains("\"pathname\":\"/foo\""));
    assert!(s.contains("\"search\":\"?bar=1\""));
    assert!(s.contains("\"hash\":\"#frag\""));
}

#[test]
fn url_class_resolves_relative_against_base() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('/path/to?x=1', 'https://example.com/');
            u.href
            "#,
        )
        .expect("eval ok");
    assert_eq!(
        out.value.as_str().unwrap(),
        "https://example.com/path/to?x=1"
    );
}

#[test]
fn url_can_parse_static_method() {
    let out = engine()
        .eval(
            r#"
            [URL.canParse('https://example.com/'), URL.canParse('not a url')]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!([true, false]));
}

#[test]
fn url_constructor_throws_typeerror_on_garbage() {
    let err = engine()
        .eval("new URL('not a url')")
        .expect_err("should throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("invalid url") || msg.contains("URL"),
        "expected URL parse error, got: {msg}"
    );
}

#[test]
fn url_to_string_returns_href() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/foo?a=1');
            [u.toString(), String(u), JSON.stringify(u)]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], "https://example.com/foo?a=1");
    assert_eq!(out.value[1], "https://example.com/foo?a=1");
    // JSON.stringify uses toJSON.
    assert_eq!(out.value[2], "\"https://example.com/foo?a=1\"");
}

// ===== searchParams basic methods =====================================

#[test]
fn search_params_get_returns_first_value() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?a=1&b=2&a=3');
            [u.searchParams.get('a'), u.searchParams.get('b'), u.searchParams.get('missing')]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], "1");
    assert_eq!(out.value[1], "2");
    assert_eq!(out.value[2], serde_json::Value::Null);
}

#[test]
fn search_params_get_all_returns_all_values() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?a=1&b=2&a=3');
            u.searchParams.getAll('a')
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!(["1", "3"]));
}

#[test]
fn search_params_has_checks_existence() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?a=1');
            [
                u.searchParams.has('a'),
                u.searchParams.has('b'),
                u.searchParams.has('a', '1'),
                u.searchParams.has('a', '2'),
            ]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!([true, false, true, false]));
}

#[test]
fn search_params_set_replaces_first_and_drops_later() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?a=1&b=2&a=3&a=4');
            u.searchParams.set('a', '99');
            // Set should replace the first 'a' and drop later 'a's.
            [u.searchParams.getAll('a'), u.searchParams.get('b')]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], serde_json::json!(["99"]));
    assert_eq!(out.value[1], "2");
}

#[test]
fn search_params_append_always_adds() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?a=1');
            u.searchParams.append('a', '2');
            u.searchParams.append('a', '3');
            u.searchParams.getAll('a')
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!(["1", "2", "3"]));
}

#[test]
fn search_params_delete_removes_all_matching() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?a=1&b=2&a=3');
            u.searchParams.delete('a');
            u.searchParams.toString()
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value.as_str().unwrap(), "b=2");
}

#[test]
fn search_params_delete_with_value_removes_only_matching_value() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?a=1&a=2&a=3');
            u.searchParams.delete('a', '2');
            u.searchParams.getAll('a')
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!(["1", "3"]));
}

// ===== Parent-URL reflection (the load-bearing invariant) =============

#[test]
fn mutating_search_params_reflects_into_url_to_string() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?a=1');
            u.searchParams.set('b', '2');
            u.searchParams.append('a', '3');
            u.toString()
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().unwrap();
    // Order: existing 'a=1' kept, 'b=2' appended, then another 'a=3'
    // appended. (Set on 'b' appends because 'b' didn't exist.)
    assert_eq!(s, "https://example.com/?a=1&b=2&a=3");
}

#[test]
fn deleting_all_params_drops_query_from_url() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/path?a=1&b=2');
            u.searchParams.delete('a');
            u.searchParams.delete('b');
            // Empty searchParams should drop the trailing '?'.
            [u.toString(), u.search]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], "https://example.com/path");
    assert_eq!(out.value[1], "");
}

#[test]
fn url_search_setter_repopulates_search_params() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?old=value');
            u.search = '?fresh=1&other=2';
            [u.searchParams.get('fresh'), u.searchParams.get('other'), u.searchParams.get('old')]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], "1");
    assert_eq!(out.value[1], "2");
    assert_eq!(out.value[2], serde_json::Value::Null);
}

#[test]
fn search_setter_strips_leading_question_mark() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/');
            u.search = 'a=1';     // no leading '?'
            u.toString()
            "#,
        )
        .expect("eval ok");
    assert_eq!(
        out.value.as_str().unwrap(),
        "https://example.com/?a=1"
    );
}

// ===== Standalone URLSearchParams constructor =========================

#[test]
fn standalone_search_params_from_string() {
    let out = engine()
        .eval(
            r#"
            const p = new URLSearchParams('a=1&b=2');
            [p.get('a'), p.get('b'), p.size]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], "1");
    assert_eq!(out.value[1], "2");
    assert_eq!(out.value[2], serde_json::json!(2));
}

#[test]
fn standalone_search_params_strips_leading_question_mark() {
    let out = engine()
        .eval(
            r#"
            const p = new URLSearchParams('?a=1&b=2');
            p.toString()
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value.as_str().unwrap(), "a=1&b=2");
}

#[test]
fn standalone_search_params_from_array_of_pairs() {
    let out = engine()
        .eval(
            r#"
            const p = new URLSearchParams([['a', '1'], ['b', '2'], ['a', '3']]);
            [p.toString(), p.getAll('a')]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], "a=1&b=2&a=3");
    assert_eq!(out.value[1], serde_json::json!(["1", "3"]));
}

#[test]
fn standalone_search_params_from_record_object() {
    let out = engine()
        .eval(
            r#"
            const p = new URLSearchParams({ a: '1', b: '2' });
            // Object.keys gives insertion order.
            p.toString()
            "#,
        )
        .expect("eval ok");
    // Record path produces a=1&b=2 (insertion order from
    // Object.keys for own string-keyed properties).
    let s = out.value.as_str().unwrap();
    assert_eq!(s, "a=1&b=2");
}

#[test]
fn standalone_search_params_empty_init() {
    let out = engine()
        .eval(
            r#"
            const p1 = new URLSearchParams();
            const p2 = new URLSearchParams('');
            const p3 = new URLSearchParams(null);
            const p4 = new URLSearchParams(undefined);
            [p1.size, p2.size, p3.size, p4.size, p1.toString()]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], serde_json::json!(0));
    assert_eq!(out.value[1], serde_json::json!(0));
    assert_eq!(out.value[2], serde_json::json!(0));
    assert_eq!(out.value[3], serde_json::json!(0));
    assert_eq!(out.value[4], "");
}

// ===== toString / size ================================================

#[test]
fn search_params_to_string_no_leading_question_mark() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?a=1&b=2');
            u.searchParams.toString()
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value.as_str().unwrap(), "a=1&b=2");
}

#[test]
fn search_params_size_matches_entry_count() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?a=1&b=2&a=3');
            const before = u.searchParams.size;
            u.searchParams.append('c', '4');
            const after = u.searchParams.size;
            u.searchParams.delete('a');
            const afterDelete = u.searchParams.size;
            [before, after, afterDelete]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!([3, 4, 2]));
}

// ===== Iteration ======================================================

#[test]
fn search_params_for_of_iterates_entries_in_insertion_order() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?a=1&b=2&a=3');
            const out = [];
            for (const [k, v] of u.searchParams) {
                out.push(k + '=' + v);
            }
            out
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!(["a=1", "b=2", "a=3"]));
}

#[test]
fn search_params_entries_returns_array_of_pairs() {
    let out = engine()
        .eval(
            r#"
            const p = new URLSearchParams('a=1&b=2');
            const arr = p.entries();
            [Array.isArray(arr), arr.length, arr[0], arr[1]]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], serde_json::json!(true));
    assert_eq!(out.value[1], serde_json::json!(2));
    assert_eq!(out.value[2], serde_json::json!(["a", "1"]));
    assert_eq!(out.value[3], serde_json::json!(["b", "2"]));
}

#[test]
fn search_params_keys_and_values() {
    let out = engine()
        .eval(
            r#"
            const p = new URLSearchParams('a=1&b=2&a=3');
            [p.keys(), p.values()]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], serde_json::json!(["a", "b", "a"]));
    assert_eq!(out.value[1], serde_json::json!(["1", "2", "3"]));
}

#[test]
fn search_params_for_each_invokes_callback_with_value_key() {
    let out = engine()
        .eval(
            r#"
            const p = new URLSearchParams('a=1&b=2');
            const out = [];
            p.forEach(function(value, key) {
                out.push(key + ':' + value);
            });
            out
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!(["a:1", "b:2"]));
}

// ===== sort() =========================================================

#[test]
fn search_params_sort_orders_by_key_stably() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?b=1&a=2&a=1&c=0');
            u.searchParams.sort();
            // Stable: a=2 comes before a=1 because a=2 appeared first.
            u.searchParams.toString()
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value.as_str().unwrap(), "a=2&a=1&b=1&c=0");
}

#[test]
fn search_params_sort_reflects_into_url() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?b=1&a=2');
            u.searchParams.sort();
            u.toString()
            "#,
        )
        .expect("eval ok");
    assert_eq!(
        out.value.as_str().unwrap(),
        "https://example.com/?a=2&b=1"
    );
}

// ===== Percent-encoding round trip ====================================

#[test]
fn search_params_set_percent_encodes_special_chars() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/');
            u.searchParams.set('q', 'hello world & friends');
            // Per WHATWG form-urlencoded: space => '+', '&' => '%26'.
            u.toString()
            "#,
        )
        .expect("eval ok");
    assert_eq!(
        out.value.as_str().unwrap(),
        "https://example.com/?q=hello+world+%26+friends"
    );
}

#[test]
fn search_params_get_decodes_percent_escapes() {
    let out = engine()
        .eval(
            r#"
            const u = new URL('https://example.com/?q=hello+world+%26+friends');
            u.searchParams.get('q')
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value.as_str().unwrap(), "hello world & friends");
}
