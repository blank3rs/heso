//! Integration tests for the WHATWG `Blob` and `File` globals
//! installed by [`heso_engine_js::web_apis::install_web_apis`]. Per
//! AGENT_FINDINGS_V2.md F1 and "Top NEW bugs" #4 — these constructors
//! were the gap blocking every file-upload path.

use heso_engine_js::JsEngine;

fn engine() -> JsEngine {
    JsEngine::new().expect("engine new")
}

// =============================================================================
// Blob
// =============================================================================

#[test]
fn blob_construct_from_string_parts_records_utf8_bytes() {
    let out = engine()
        .eval(
            r#"
            const b = new Blob(["hello, world"]);
            ({ size: b.size, type: b.type })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["size"], 12);
    assert_eq!(out.value["type"], "");
}

#[test]
fn blob_construct_with_type_option_lowercases() {
    let out = engine()
        .eval(
            r#"
            const b = new Blob(["x"], { type: "Text/Plain" });
            b.type
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "text/plain");
}

#[test]
fn blob_text_round_trip() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            const b = new Blob(["agent-shaped"]);
            b.text().then(t => { globalThis.__t = t; });
            "#,
        )
        .expect("schedule");
    let out = e.eval("globalThis.__t").expect("observe");
    assert_eq!(out.value, "agent-shaped");
}

#[test]
fn blob_text_concatenates_multiple_string_parts() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            const b = new Blob(["foo", "bar", "baz"]);
            globalThis.__size = b.size;
            b.text().then(t => { globalThis.__t = t; });
            "#,
        )
        .expect("schedule");
    let out = e
        .eval("[globalThis.__size, globalThis.__t]")
        .expect("observe");
    assert_eq!(out.value[0], 9);
    assert_eq!(out.value[1], "foobarbaz");
}

#[test]
fn blob_size_zero_for_empty_parts() {
    let out = engine().eval("new Blob([]).size").expect("eval");
    assert_eq!(out.value, 0);
}

#[test]
fn blob_size_zero_for_undefined_parts() {
    let out = engine().eval("new Blob().size").expect("eval");
    assert_eq!(out.value, 0);
}

#[test]
fn blob_slice_returns_new_blob_with_correct_bytes() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            const b = new Blob(["abcdefghij"]);
            const s = b.slice(2, 6);
            globalThis.__size = s.size;
            globalThis.__type = s.type;
            s.text().then(t => { globalThis.__t = t; });
            "#,
        )
        .expect("schedule");
    let out = e
        .eval("[globalThis.__size, globalThis.__t, globalThis.__type]")
        .expect("observe");
    assert_eq!(out.value[0], 4);
    assert_eq!(out.value[1], "cdef");
    assert_eq!(out.value[2], "");
}

#[test]
fn blob_slice_with_content_type_sets_new_type() {
    let out = engine()
        .eval(
            r#"
            const b = new Blob(["hello"], { type: "text/plain" });
            const s = b.slice(0, 5, "Application/JSON");
            s.type
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "application/json");
}

#[test]
fn blob_slice_with_negative_start_counts_from_end() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            const b = new Blob(["0123456789"]);
            const s = b.slice(-3);
            globalThis.__size = s.size;
            s.text().then(t => { globalThis.__t = t; });
            "#,
        )
        .expect("schedule");
    let out = e
        .eval("[globalThis.__size, globalThis.__t]")
        .expect("observe");
    assert_eq!(out.value[0], 3);
    assert_eq!(out.value[1], "789");
}

#[test]
fn blob_slice_out_of_range_returns_empty_blob() {
    let out = engine()
        .eval(
            r#"
            const b = new Blob(["xyz"]);
            const s = b.slice(100, 200);
            s.size
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, 0);
}

#[test]
fn blob_array_buffer_yields_correct_bytes() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            const b = new Blob(["hi"]);
            b.arrayBuffer().then(ab => {
                const v = new Uint8Array(ab);
                globalThis.__bytes = [v.length, v[0], v[1]];
            });
            "#,
        )
        .expect("schedule");
    let out = e.eval("globalThis.__bytes").expect("observe");
    assert_eq!(out.value, serde_json::json!([2, 104, 105]));
}

#[test]
fn blob_bytes_method_returns_uint8array() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            const b = new Blob(["hi"]);
            b.bytes().then(u => {
                globalThis.__len = u.length;
                globalThis.__b0 = u[0];
                globalThis.__b1 = u[1];
                globalThis.__isU8 = u instanceof Uint8Array;
            });
            "#,
        )
        .expect("schedule");
    let out = e
        .eval("[globalThis.__len, globalThis.__b0, globalThis.__b1, globalThis.__isU8]")
        .expect("observe");
    assert_eq!(out.value, serde_json::json!([2, 104, 105, true]));
}

#[test]
fn blob_construct_from_blob_parts_copies_bytes() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            const inner = new Blob(["abc"]);
            const wrap = new Blob([inner, "def"]);
            globalThis.__size = wrap.size;
            wrap.text().then(t => { globalThis.__t = t; });
            "#,
        )
        .expect("schedule");
    let out = e
        .eval("[globalThis.__size, globalThis.__t]")
        .expect("observe");
    assert_eq!(out.value[0], 6);
    assert_eq!(out.value[1], "abcdef");
}

#[test]
fn blob_construct_from_uint8array() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            const arr = new Uint8Array([72, 105]);  // "Hi"
            const b = new Blob([arr]);
            globalThis.__size = b.size;
            b.text().then(t => { globalThis.__t = t; });
            "#,
        )
        .expect("schedule");
    let out = e
        .eval("[globalThis.__size, globalThis.__t]")
        .expect("observe");
    assert_eq!(out.value[0], 2);
    assert_eq!(out.value[1], "Hi");
}

#[test]
fn blob_stream_returns_undefined() {
    let out = engine()
        .eval(
            r#"
            const b = new Blob(["x"]);
            typeof b.stream()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "undefined");
}

#[test]
fn blob_invalid_part_throws_type_error() {
    let err = engine()
        .eval("new Blob([42])")
        .expect_err("number is not a valid BlobPart");
    let msg = format!("{err:?}");
    // The error message may surface as "each part must be..." or a
    // generic JS-side TypeError; either is fine — what matters is
    // that the call doesn't silently succeed with malformed bytes.
    assert!(
        msg.contains("TypeError")
            || msg.contains("BlobPart")
            || msg.contains("each part must be")
            || msg.contains("must be a string"),
        "expected TypeError-shaped error; got: {msg}"
    );
}

// =============================================================================
// File extends Blob
// =============================================================================

#[test]
fn file_is_instanceof_blob() {
    let out = engine()
        .eval(
            r#"
            const f = new File(["x"], "x.txt");
            f instanceof Blob
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, true);
}

#[test]
fn file_is_instanceof_file() {
    let out = engine()
        .eval(
            r#"
            const f = new File(["x"], "x.txt");
            f instanceof File
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, true);
}

#[test]
fn file_records_name_and_default_last_modified() {
    let out = engine()
        .eval(
            r#"
            const f = new File(["content"], "report.txt", { type: "text/plain" });
            ({
                size: f.size,
                type: f.type,
                name: f.name,
                hasLastModified: typeof f.lastModified === "number"
            })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["size"], 7);
    assert_eq!(out.value["type"], "text/plain");
    assert_eq!(out.value["name"], "report.txt");
    assert_eq!(out.value["hasLastModified"], true);
}

#[test]
fn file_last_modified_honors_option() {
    let out = engine()
        .eval(
            r#"
            const f = new File(["x"], "x", { lastModified: 1234567890 });
            f.lastModified
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, 1234567890);
}

#[test]
fn file_text_round_trip() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            const f = new File(["agent says hi"], "greet.txt");
            f.text().then(t => { globalThis.__t = t; });
            "#,
        )
        .expect("schedule");
    let out = e.eval("globalThis.__t").expect("observe");
    assert_eq!(out.value, "agent says hi");
}

#[test]
fn file_slice_returns_blob_not_file() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            const f = new File(["abcdef"], "x.bin");
            const s = f.slice(1, 4);
            globalThis.__isBlob = s instanceof Blob;
            globalThis.__isFile = s instanceof File;
            s.text().then(t => { globalThis.__t = t; });
            "#,
        )
        .expect("schedule");
    let out = e
        .eval("[globalThis.__isBlob, globalThis.__isFile, globalThis.__t]")
        .expect("observe");
    assert_eq!(out.value[0], true);
    // Per spec, slice() returns a Blob, not a File.
    assert_eq!(out.value[1], false);
    assert_eq!(out.value[2], "bcd");
}

#[test]
fn file_construct_with_no_options() {
    let out = engine()
        .eval(
            r#"
            const f = new File(["x"], "no-opts.txt");
            ({ size: f.size, name: f.name, type: f.type })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["size"], 1);
    assert_eq!(out.value["name"], "no-opts.txt");
    assert_eq!(out.value["type"], "");
}

#[test]
fn blob_constructor_is_function() {
    let out = engine().eval("typeof Blob").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn file_constructor_is_function() {
    let out = engine().eval("typeof File").expect("eval");
    assert_eq!(out.value, "function");
}
