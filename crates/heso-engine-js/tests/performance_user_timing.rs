//! Integration tests for `performance.mark` / `measure` / `clearMarks`
//! / `clearMeasures` / `getEntriesByName` / `getEntriesByType` per
//! WHATWG user-timing level 2.
//!
//! Closes bug-report 03 P1 (cluster `performance.mark` missing): on
//! github.com every chunk starts with
//! `performance.mark("js-parse-end:<asset-id>")` and 93/93 external
//! scripts died on line 1 with `not a function`.

use heso_engine_js::JsEngine;

fn engine() -> JsEngine {
    JsEngine::new().expect("engine new")
}

#[test]
fn performance_mark_is_a_function() {
    let out = engine().eval("typeof performance.mark").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn performance_measure_is_a_function() {
    let out = engine().eval("typeof performance.measure").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn performance_mark_does_not_throw_on_string_name() {
    // The github bug-report repro: every chunk's first call.
    let out = engine()
        .eval(
            r#"
            performance.mark("js-parse-end:high-contrast-cookie");
            performance.getEntriesByType("mark").length
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, 1);
}

#[test]
fn performance_mark_returns_entry_with_zero_duration() {
    let out = engine()
        .eval(
            r#"
            const m = performance.mark("step-1");
            ({ name: m.name, entryType: m.entryType, duration: m.duration })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["name"], "step-1");
    assert_eq!(out.value["entryType"], "mark");
    assert_eq!(out.value["duration"], 0);
}

#[test]
fn performance_get_entries_by_name_filters_correctly() {
    let out = engine()
        .eval(
            r#"
            performance.mark("a");
            performance.mark("b");
            performance.mark("a");
            performance.getEntriesByName("a").length
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, 2);
}

#[test]
fn performance_measure_with_two_marks_returns_entry() {
    let e = engine();
    let out = e
        .eval(
            r#"
            performance.mark("start");
            performance.mark("end");
            const m = performance.measure("range", "start", "end");
            ({ name: m.name, entryType: m.entryType, hasDuration: typeof m.duration === 'number' })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["name"], "range");
    assert_eq!(out.value["entryType"], "measure");
    assert_eq!(out.value["hasDuration"], true);
}

#[test]
fn performance_measure_without_marks_uses_now() {
    let out = engine()
        .eval(
            r#"
            const m = performance.measure("solo");
            ({ name: m.name, entryType: m.entryType, isFinite: isFinite(m.duration) })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["name"], "solo");
    assert_eq!(out.value["entryType"], "measure");
    assert_eq!(out.value["isFinite"], true);
}

#[test]
fn performance_clear_marks_clears_only_marks() {
    let out = engine()
        .eval(
            r#"
            performance.mark("a");
            performance.mark("b");
            performance.measure("m");
            performance.clearMarks();
            ({
                marks: performance.getEntriesByType("mark").length,
                measures: performance.getEntriesByType("measure").length
            })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["marks"], 0);
    assert_eq!(out.value["measures"], 1);
}

#[test]
fn performance_clear_marks_with_name_only_clears_matching() {
    let out = engine()
        .eval(
            r#"
            performance.mark("a");
            performance.mark("b");
            performance.mark("a");
            performance.clearMarks("a");
            performance.getEntriesByType("mark").map(e => e.name)
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(["b"]));
}

#[test]
fn performance_clear_measures_clears_only_measures() {
    let out = engine()
        .eval(
            r#"
            performance.mark("a");
            performance.measure("m");
            performance.clearMeasures();
            ({
                marks: performance.getEntriesByType("mark").length,
                measures: performance.getEntriesByType("measure").length
            })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["marks"], 1);
    assert_eq!(out.value["measures"], 0);
}

#[test]
fn performance_measure_options_form_with_numeric_start_and_duration() {
    let out = engine()
        .eval(
            r#"
            const m = performance.measure("m", { start: 100, duration: 50 });
            ({ start: m.startTime, dur: m.duration })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["start"], 100);
    assert_eq!(out.value["dur"], 50);
}

#[test]
fn github_chunk_first_line_pattern_does_not_throw() {
    // Exact pattern from bug-report-01 cluster P0.
    let out = engine()
        .eval(
            r#"
            performance.mark("js-parse-end:high-contrast-cookie-abc123");
            "ok"
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "ok");
}
