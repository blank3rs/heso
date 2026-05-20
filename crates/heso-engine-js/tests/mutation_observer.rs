//! Integration tests for the real `MutationObserver` installed by
//! [`crate::mutation_observer::install`].
//!
//! Each test pins one spec-bearing behaviour from WHATWG DOM § 4.3.
//! Failures here mean a page that uses MutationObserver — Lit's
//! component-upgrade detector, Stencil's slot-reassignment hook,
//! Vue's reactive `<template>` bookkeeping, Solid's reactive root
//! attachment — will silently observe nothing on heso.

use heso_engine_js::JsSession;
use url::Url;

fn page(html: &str) -> JsSession {
    let url = Url::parse("https://example.com/").unwrap();
    JsSession::open(html, url).expect("open page").0
}

// ===== Surface: constructor, instance shape ====================================

#[test]
fn mutation_observer_is_a_real_function() {
    let sess = page("<html><body></body></html>");
    let out = sess.eval("typeof MutationObserver").expect("eval");
    assert_eq!(out.value, serde_json::json!("function"));
}

#[test]
fn mutation_observer_constructor_requires_new() {
    let sess = page("<html><body></body></html>");
    let err = sess
        .eval("MutationObserver(function(){})")
        .expect_err("bare-call without new should throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("requires 'new'") || msg.contains("TypeError"),
        "expected requires-new TypeError, got: {msg}"
    );
}

#[test]
fn mutation_observer_constructor_requires_callable_callback() {
    let sess = page("<html><body></body></html>");
    let err = sess
        .eval("new MutationObserver(null)")
        .expect_err("non-function callback should throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("not a function") || msg.contains("TypeError"),
        "expected not-a-function TypeError, got: {msg}"
    );
}

#[test]
fn mutation_record_is_exposed_globally() {
    let sess = page("<html><body></body></html>");
    let out = sess.eval("typeof MutationRecord").expect("eval");
    assert_eq!(out.value, serde_json::json!("function"));
}

// ===== childList observations ==================================================

#[test]
fn child_list_observes_append_child() {
    // Spec § 4.3 + § 4.4.1 mutation algorithms: appendChild produces
    // a childList MutationRecord with addedNodes containing the new
    // child. The contract test from the task spec.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let records = [];
                const target = document.createElement('div');
                document.body.appendChild(target);
                new MutationObserver(rs => { records = records.concat(rs); }).observe(target, { childList: true });
                target.appendChild(document.createElement('span'));
                target.appendChild(document.createElement('span'));
                await Promise.resolve();
                return records.length === 2 && records.every(r => r.type === 'childList');
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn child_list_includes_added_nodes() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let recs = [];
                const target = document.createElement('div');
                document.body.appendChild(target);
                new MutationObserver(rs => { recs = recs.concat(rs); }).observe(target, { childList: true });
                const a = document.createElement('a');
                target.appendChild(a);
                await Promise.resolve();
                return recs.length === 1 &&
                       Array.isArray(recs[0].addedNodes) &&
                       recs[0].addedNodes.length === 1 &&
                       recs[0].addedNodes[0] === a;
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn child_list_includes_removed_nodes() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let recs = [];
                const target = document.createElement('div');
                const child = document.createElement('span');
                target.appendChild(child);
                document.body.appendChild(target);
                new MutationObserver(rs => { recs = recs.concat(rs); }).observe(target, { childList: true });
                target.removeChild(child);
                await Promise.resolve();
                return recs.length === 1 &&
                       recs[0].type === 'childList' &&
                       recs[0].removedNodes.length === 1 &&
                       recs[0].removedNodes[0] === child;
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn child_list_records_batched_in_one_callback() {
    // Spec § 4.3.2: all mutations during a microtask should produce
    // a SINGLE callback invocation with the batched array. Two
    // separate appends in the same task → one callback, two records.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let callbackCount = 0;
                let totalRecords = 0;
                const target = document.createElement('div');
                document.body.appendChild(target);
                new MutationObserver(rs => {
                    callbackCount++;
                    totalRecords += rs.length;
                }).observe(target, { childList: true });
                target.appendChild(document.createElement('a'));
                target.appendChild(document.createElement('b'));
                target.appendChild(document.createElement('c'));
                await Promise.resolve();
                return callbackCount === 1 && totalRecords === 3;
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

// ===== attribute observations ==================================================

#[test]
fn attributes_observes_set_attribute() {
    // The second contract test from the task spec.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let fired = false;
                const el = document.createElement('div');
                document.body.appendChild(el);
                new MutationObserver(rs => {
                    if (rs[0].type === 'attributes' && rs[0].attributeName === 'data-x') fired = true;
                }).observe(el, { attributes: true, attributeOldValue: true });
                el.setAttribute('data-x', '1');
                await Promise.resolve();
                return fired === true;
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn attributes_old_value_captured_when_requested() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let captured = null;
                const el = document.createElement('div');
                el.setAttribute('data-x', 'initial');
                document.body.appendChild(el);
                new MutationObserver(rs => {
                    captured = rs[0].oldValue;
                }).observe(el, { attributes: true, attributeOldValue: true });
                el.setAttribute('data-x', 'updated');
                await Promise.resolve();
                return captured === 'initial';
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn attributes_old_value_null_when_not_requested() {
    // Spec § 4.3: oldValue is null on the queued record UNLESS the
    // observer's options have attributeOldValue: true.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let oldVal = 'sentinel';
                const el = document.createElement('div');
                el.setAttribute('data-x', 'initial');
                document.body.appendChild(el);
                new MutationObserver(rs => { oldVal = rs[0].oldValue; })
                    .observe(el, { attributes: true });
                el.setAttribute('data-x', 'updated');
                await Promise.resolve();
                return oldVal === null;
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn attribute_filter_restricts_observed_attrs() {
    // Observer with attributeFilter: ['data-y'] should miss
    // setAttribute('data-x', ...) but catch setAttribute('data-y', ...).
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let names = [];
                const el = document.createElement('div');
                document.body.appendChild(el);
                new MutationObserver(rs => {
                    for (const r of rs) names.push(r.attributeName);
                }).observe(el, { attributes: true, attributeFilter: ['data-y'] });
                el.setAttribute('data-x', '1');
                el.setAttribute('data-y', '2');
                await Promise.resolve();
                return names.length === 1 && names[0] === 'data-y';
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn remove_attribute_fires_observer() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let recs = [];
                const el = document.createElement('div');
                el.setAttribute('data-x', 'v');
                document.body.appendChild(el);
                new MutationObserver(rs => { recs = recs.concat(rs); })
                    .observe(el, { attributes: true, attributeOldValue: true });
                el.removeAttribute('data-x');
                await Promise.resolve();
                return recs.length === 1 &&
                       recs[0].type === 'attributes' &&
                       recs[0].attributeName === 'data-x' &&
                       recs[0].oldValue === 'v';
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn remove_attribute_skipped_when_attr_absent() {
    // Spec: "if element has no attribute named name, do nothing."
    // The MO record is queued only on actual removal.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let callCount = 0;
                const el = document.createElement('div');
                document.body.appendChild(el);
                new MutationObserver(rs => { callCount++; })
                    .observe(el, { attributes: true });
                el.removeAttribute('data-absent');
                await Promise.resolve();
                return callCount === 0;
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

// ===== subtree observations ====================================================

#[test]
fn subtree_observes_descendant_mutation() {
    // Observer on `outer` with subtree: true should see appendChild
    // calls on `inner`.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let recs = [];
                const outer = document.createElement('div');
                const inner = document.createElement('div');
                outer.appendChild(inner);
                document.body.appendChild(outer);
                new MutationObserver(rs => { recs = recs.concat(rs); })
                    .observe(outer, { childList: true, subtree: true });
                inner.appendChild(document.createElement('span'));
                await Promise.resolve();
                return recs.length === 1 && recs[0].type === 'childList';
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn subtree_off_misses_descendant_mutation() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let callCount = 0;
                const outer = document.createElement('div');
                const inner = document.createElement('div');
                outer.appendChild(inner);
                document.body.appendChild(outer);
                new MutationObserver(rs => { callCount++; })
                    .observe(outer, { childList: true /* no subtree */ });
                inner.appendChild(document.createElement('span'));
                await Promise.resolve();
                return callCount === 0;
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

// ===== disconnect / takeRecords ================================================

#[test]
fn disconnect_stops_observing() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let callCount = 0;
                const target = document.createElement('div');
                document.body.appendChild(target);
                const obs = new MutationObserver(rs => { callCount++; });
                obs.observe(target, { childList: true });
                obs.disconnect();
                target.appendChild(document.createElement('span'));
                await Promise.resolve();
                return callCount === 0;
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn take_records_drains_queue() {
    // Spec § 4.3 takeRecords: returns the queued records and clears
    // them, so the eventual microtask callback fires with [].
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let callbackArrived = [];
                const target = document.createElement('div');
                document.body.appendChild(target);
                const obs = new MutationObserver(rs => { callbackArrived = rs; });
                obs.observe(target, { childList: true });
                target.appendChild(document.createElement('span'));
                target.appendChild(document.createElement('span'));
                const taken = obs.takeRecords();
                await Promise.resolve();
                // takeRecords returned 2 records, callback fired
                // with the empty leftover (or didn't fire at all
                // because the queue was drained — both are spec-OK).
                return taken.length === 2 && callbackArrived.length === 0;
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

// ===== option validation =======================================================

#[test]
fn observe_requires_at_least_one_kind() {
    // Spec § 4.3.1 step 3: TypeError if all of childList, attributes,
    // characterData are false.
    let sess = page("<html><body></body></html>");
    let err = sess
        .eval(
            r#"
            const el = document.createElement('div');
            document.body.appendChild(el);
            new MutationObserver(() => {}).observe(el, {});
            "#,
        )
        .expect_err("observe with no kinds set must throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("at least one") || msg.contains("TypeError"),
        "expected at-least-one TypeError, got: {msg}"
    );
}

#[test]
fn observe_rejects_attribute_old_value_without_attributes() {
    let sess = page("<html><body></body></html>");
    let err = sess
        .eval(
            r#"
            const el = document.createElement('div');
            document.body.appendChild(el);
            new MutationObserver(() => {}).observe(el, {
                childList: true, attributes: false, attributeOldValue: true,
            });
            "#,
        )
        .expect_err("attributeOldValue without attributes must throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("attributeOldValue") || msg.contains("TypeError"),
        "expected attributeOldValue TypeError, got: {msg}"
    );
}

#[test]
fn observe_defaults_attributes_when_filter_set() {
    // Spec: attributes defaults to true iff attributeFilter or
    // attributeOldValue is set. Setting attributeFilter without
    // attributes should NOT throw — attributes defaults to true.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let fired = false;
                const el = document.createElement('div');
                document.body.appendChild(el);
                new MutationObserver(rs => { fired = true; })
                    .observe(el, { attributeFilter: ['data-x'] });
                el.setAttribute('data-x', 'v');
                await Promise.resolve();
                return fired === true;
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

// ===== callback receives (records, observer) ===================================

#[test]
fn callback_second_arg_is_observer() {
    // Spec § 4.3.2 step 4.2: the callback is invoked with two
    // arguments: the records sequence and a reference to the
    // observer itself.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                let receivedObs = null;
                const target = document.createElement('div');
                document.body.appendChild(target);
                const obs = new MutationObserver((rs, o) => { receivedObs = o; });
                obs.observe(target, { childList: true });
                target.appendChild(document.createElement('span'));
                await Promise.resolve();
                return receivedObs === obs;
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}
