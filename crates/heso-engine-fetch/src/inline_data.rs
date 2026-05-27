//! # inline_data
//!
//! Extract structured data that SSR frameworks embed into the HTML
//! itself — the JSON blobs and JS-assigned globals a "server-rendered
//! SPA" relies on to hydrate. Visible DOM on these pages often looks
//! empty (the content is in the embedded blob, not in `<p>`/`<div>`
//! elements). Surfacing the blob is what lets cartography work on
//! SSR sites without executing JavaScript.
//!
//! ## What we catch
//!
//! Three structural patterns, no site-specific knowledge. First write
//! to a key wins.
//!
//! 1. **`<script type="application/json">`** — the most explicit form.
//!    Examples in the wild: Next.js `__NEXT_DATA__`, Apple's CMS
//!    `__ACGH_DATA__`, Nuxt `__NUXT_DATA__`, Remix loader payloads,
//!    Astro islands. Keyed by the script's `id` attribute, or
//!    `unnamed-{n}` for unkeyed scripts in document order.
//! 2. **`self.__next_f.push([N, "..."])` streams** — modern Next.js
//!    14+ App Router (React Server Components). Each chunk is a JS
//!    string of newline-separated `<id>:<json>` entries; we concat
//!    chunks across all `<script>` tags, split by line, and parse each
//!    line's right-hand side. Stored under the key `__next_f` as an
//!    ordered JSON array of `{ "id": "...", "value": ... }` records.
//! 3. **`<path> = <JSON-shaped-value>`** — the generic assignment
//!    rule. `<path>` is any identifier (`__DATA__`), dotted chain
//!    (`netflix.reactContext`, `app.cfg.initial`), bracket-indexed
//!    chain (`window["__INITIAL__"]`), or any mix
//!    (`a["b"].c`). `<value>` is either a `{...}` object literal we
//!    validate by parsing as strict JSON, or a `JSON.parse("...")`
//!    call whose argument decodes to a JSON document. Keys are
//!    normalized: leading `window.` stripped, `["foo"]` rewritten to
//!    `.foo`, whitespace removed. Wins by first-write so the more
//!    explicit `application/json` and RSC paths take precedence.
//!
//! The general rule means we don't need a table of "frameworks we
//! know about" — the same regex catches Apollo's `__APOLLO_STATE__`,
//! Next.js's `__NEXT_DATA__`, Netflix's `netflix.reactContext`,
//! Redux's `__PRELOADED_STATE__`, and anything else that ships
//! structured data via inline assignment. Adding a new framework is
//! zero code change.
//!
//! ## What we still don't catch
//!
//! - `<script type="application/ld+json">` — that's Schema.org JSON-LD,
//!   already extracted by [`crate::metadata`] as `jsonld[]`. We
//!   deliberately don't double-count.
//! - Computed-expression hydration like
//!   `JSON.parse(atob(decodeURI(...)))` — the inner argument isn't a
//!   string literal, so we can't decode it without running JS. ADR
//!   0014's bundled QuickJS will handle that.
//! - JS-shaped object literals that aren't strict JSON (unquoted keys,
//!   single-quoted strings, `undefined`, function references). The
//!   `serde_json` parse rejects these. In practice, real hydration is
//!   `JSON.stringify` output and parses cleanly.
//! - RSC binary chunks (rare, `_rsc.bin` external fetches).
//!
//! ## Why this matters
//!
//! On apple.com today the visible body is ~3 KB of text. The actual
//! page content lives in `__ACGH_DATA__` — a ~30 KB JSON blob. On
//! modern Next.js sites the content arrives via `__next_f` streams.
//! On Apollo/Redux SSR sites it's `window.__APOLLO_STATE__`. Without
//! this module, an agent sees ~3 KB and hallucinates. With it, the
//! agent reads the structured payload that the page itself ships for
//! its own client hydration — no JS execution required.

use std::collections::BTreeMap;
use std::sync::{LazyLock, OnceLock};

use regex::Regex;
use scraper::{Html, Selector};

// Selectors lifted out of the per-page extractors — each used to
// `Selector::parse(...)` on every call.
static APPLICATION_JSON_SCRIPT_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"script[type="application/json"]"#).expect("valid"));
static SCRIPT_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("script").expect("valid"));

/// Extract inline data from a parsed page.
///
/// Runs three sub-extractors in priority order — `application/json`
/// blocks, then `__next_f` RSC streams, then `window.X` assignments —
/// merging results into a single [`BTreeMap`]. First write to a given
/// key wins, so explicit framework conventions take precedence over
/// inferred-from-JS assignments.
///
/// The sorted-key property of [`BTreeMap`] makes the serialized output
/// canonical regardless of insertion order. Combined with document
/// order on unnamed entries and on RSC line arrays, that preserves the
/// engine's deterministic-plat property ([`crate::plat`]).
pub fn extract(doc: &Html) -> BTreeMap<String, serde_json::Value> {
    let mut out: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    extract_application_json(doc, &mut out);
    extract_rsc(doc, &mut out);
    extract_js_data_assigns(doc, &mut out);
    out
}

/// Extract `<script type="application/json">` blocks.
///
/// Keyed by the script's `id` attribute; unkeyed scripts get
/// `unnamed-{n}` synthetic keys in document order. Malformed JSON
/// is skipped silently.
fn extract_application_json(doc: &Html, out: &mut BTreeMap<String, serde_json::Value>) {
    let mut unnamed_counter: usize = 0;
    for s in doc.select(&APPLICATION_JSON_SCRIPT_SEL) {
        let raw: String = s.text().collect();
        // Strip a leading UTF-8 BOM (U+FEFF) before trimming —
        // `serde_json::from_str` rejects a BOM, and some servers (or
        // hand-rolled blobs) emit one at the start of a JSON document.
        let no_bom = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
        let trimmed = no_bom.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let key = match s.value().attr("id") {
            Some(id) if !id.trim().is_empty() => id.trim().to_owned(),
            _ => {
                let k = format!("unnamed-{unnamed_counter}");
                unnamed_counter += 1;
                k
            }
        };
        out.entry(key).or_insert(value);
    }
}

/// Extract React Server Components flight streams.
///
/// Recognizes `self.__next_f.push([N, "..."])` calls across all inline
/// `<script>` tags, concatenates the string payloads in source order,
/// splits the concatenation by newline, and tries to JSON-parse each
/// line's right-hand side (after the first `:`). Lines that don't
/// parse as JSON are preserved as raw strings — text chunks
/// (`T<size>,"..."`) and module references (`I[...]`) fall into this
/// bucket and remain greppable.
///
/// Result is stored under the single key `__next_f` as a JSON array of
/// `{ "id": "<line-prefix>", "value": <parsed-or-string> }` records.
fn extract_rsc(doc: &Html, out: &mut BTreeMap<String, serde_json::Value>) {
    static PUSH_RE: OnceLock<Regex> = OnceLock::new();
    let push_re = PUSH_RE.get_or_init(|| {
        // self.__next_f.push([N, "STRING"])
        // The string body is captured WITH its surrounding quotes so we
        // can hand the whole literal to serde_json::from_str::<String>
        // for unescaping.
        Regex::new(r#"self\.__next_f\.push\(\s*\[\s*\d+\s*,\s*("(?:[^"\\]|\\.)*")\s*\]\s*\)"#)
            .expect("valid push regex")
    });

    let mut payload = String::new();

    for s in doc.select(&SCRIPT_SEL) {
        if !is_plain_js_script(s.value().attr("type")) {
            continue;
        }
        let text: String = s.text().collect();
        for cap in push_re.captures_iter(&text) {
            let quoted = &cap[1];
            if let Ok(unescaped) = serde_json::from_str::<String>(quoted) {
                payload.push_str(&unescaped);
            }
        }
    }

    if payload.is_empty() {
        return;
    }

    let mut entries: Vec<serde_json::Value> = Vec::new();
    for line in payload.lines() {
        if line.is_empty() {
            continue;
        }
        let (id, rest) = match line.split_once(':') {
            Some(pair) => pair,
            None => ("", line),
        };
        let value: serde_json::Value = serde_json::from_str(rest)
            .unwrap_or_else(|_| serde_json::Value::String(rest.to_owned()));
        entries.push(serde_json::json!({ "id": id, "value": value }));
    }

    if entries.is_empty() {
        return;
    }

    out.entry("__next_f".to_owned())
        .or_insert(serde_json::Value::Array(entries));
}

/// Extract `<path> = <JSON-shaped-value>` assignments from inline
/// `<script>` blocks.
///
/// One regex pattern for the left-hand side — any JS identifier
/// followed by zero or more `.ident` / `["str"]` continuations —
/// recognized in two right-hand-side forms:
///
/// - `JSON.parse("...")` — the argument is a JS string literal
///   containing a JSON document. We unescape the literal via
///   `serde_json::from_str::<String>`, then re-parse the inner content
///   as JSON. High-confidence: the developer chose `JSON.parse` because
///   they explicitly want a parsed structure on hydration.
/// - `{...}` object literals — naive matching-brace scan from the
///   first `{`. The captured substring is validated by
///   `serde_json::from_str`; non-JSON-shaped JS (unquoted keys, single
///   quotes, `undefined`, function refs, regex literals) is rejected,
///   which keeps false positives low. Real SSR-emitted state is
///   `JSON.stringify` output and parses cleanly.
///
/// Keys are normalized via [`normalize_assignment_key`]: a leading
/// `window.` is stripped (the global scope marker is implicit),
/// `["foo"]` indexing is rewritten to `.foo`, and whitespace is
/// removed. So `window.__APOLLO_STATE__`, `window["__APOLLO_STATE__"]`,
/// and `__APOLLO_STATE__` all collapse to the same key
/// `__APOLLO_STATE__`; `netflix.reactContext` stays as
/// `netflix.reactContext`.
///
/// First write wins; the `JSON.parse` pass runs before the object-
/// literal pass within each script. Across scripts, document order is
/// preserved.
fn extract_js_data_assigns(doc: &Html, out: &mut BTreeMap<String, serde_json::Value>) {
    static JSON_PARSE_RE: OnceLock<Regex> = OnceLock::new();
    let json_parse_re = JSON_PARSE_RE.get_or_init(|| {
        Regex::new(IDENT_PATH_LHS_PATTERN_JSON_PARSE).expect("valid JSON.parse regex")
    });

    static OBJECT_LITERAL_RE: OnceLock<Regex> = OnceLock::new();
    let object_literal_re = OBJECT_LITERAL_RE.get_or_init(|| {
        Regex::new(IDENT_PATH_LHS_PATTERN_OBJECT_LITERAL).expect("valid object literal regex")
    });

    for s in doc.select(&SCRIPT_SEL) {
        if !is_plain_js_script(s.value().attr("type")) {
            continue;
        }
        let raw_text: String = s.text().collect();
        // Strip JS line (`//...`) and block (`/* ... */`) comments
        // before regex matching, so commented-out assignments
        // (`// window.__X__ = {...};`) don't leak into output.
        // String-literal boundaries are respected so `//` inside a
        // string is preserved.
        let text = strip_js_comments(&raw_text);

        // Pass 1: <path> = JSON.parse("...")
        for cap in json_parse_re.captures_iter(&text) {
            let key = normalize_assignment_key(&cap[1]);
            if key.is_empty() {
                continue;
            }
            let inner: String = match serde_json::from_str(&cap[2]) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&inner) {
                out.entry(key).or_insert(v);
            }
        }

        // Pass 2: <path> = { ... } as a direct object literal.
        for cap in object_literal_re.captures_iter(&text) {
            let key = normalize_assignment_key(&cap[1]);
            if key.is_empty() {
                continue;
            }
            if out.contains_key(&key) {
                continue;
            }
            let brace_match = match cap.get(2) {
                Some(m) => m,
                None => continue,
            };
            let start = brace_match.start();
            let end = match find_matching_brace(&text, start) {
                Some(e) => e,
                None => continue,
            };
            let candidate = &text[start..=end];
            // Try strict JSON first; if that fails, rewrite JS-only
            // escapes (\xNN, \v, \0, \') to JSON-valid forms and
            // retry. Real hydration data is almost always emitted by
            // `JSON.stringify` (strict-JSON-clean), but some hand-
            // rolled SSR — e.g. Netflix's `netflix.reactContext` —
            // injects JS-style escapes in otherwise-JSON-shaped
            // values. The rewriter is a mechanical, universal
            // translation; no per-site special-casing.
            let parsed = serde_json::from_str::<serde_json::Value>(candidate)
                .ok()
                .or_else(|| {
                    let rewritten = rewrite_js_only_escapes(candidate);
                    serde_json::from_str::<serde_json::Value>(&rewritten).ok()
                });
            if let Some(v) = parsed {
                if !is_trivially_empty(&v) {
                    out.insert(key, v);
                }
            }
        }
    }
}

/// True if `v` is `{}` or `[]`. Such values are emitted by inline JS
/// as namespace initialization (`var ns = {}` / `obj.cache = {}`)
/// rather than as hydration data, so we skip them to keep
/// `inline_data` focused on signal.
fn is_trivially_empty(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(o) => o.is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

/// Translate JS-only string escapes inside double- or single-quoted
/// string literals so the surrounding object literal becomes
/// strict-JSON-parseable. Operates only inside string literals; code
/// outside strings is left untouched.
///
/// Translations (only the JS-only forms — valid JSON escapes pass
/// through unchanged):
///
/// - `\xNN` → `\u00NN` (hex byte escape — most common JS-ism)
/// - `\v` → `\u000b` (vertical tab)
/// - `\0` when NOT followed by `0-9` → `\u0000` (null char, avoiding
///   octal-escape misinterpretation)
/// - `\'` → `'` (single-quote escape — needed inside JS strings,
///   redundant in JSON)
///
/// We do NOT rewrite single-quoted strings to double-quoted — that's
/// a separate, much rarer JS-ism that requires a real string parser
/// to handle correctly (escaping internal double quotes, etc.). If a
/// page ships hydration data in single-quoted strings, it won't
/// parse, and we accept that miss.
fn rewrite_js_only_escapes(input: &str) -> String {
    // Operates on raw bytes to preserve multi-byte UTF-8 sequences
    // verbatim (casting `u8 as char` would treat each byte as
    // Latin-1, corrupting non-ASCII chars). All emitted replacement
    // bytes are ASCII, so the resulting `Vec<u8>` stays valid UTF-8.
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_string = false;
    let mut string_quote: u8 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !in_string {
            if b == b'"' || b == b'\'' {
                in_string = true;
                string_quote = b;
            }
            out.push(b);
            i += 1;
            continue;
        }
        if b == string_quote {
            in_string = false;
            out.push(b);
            i += 1;
            continue;
        }
        if b == b'\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            match next {
                b'x' if i + 3 < bytes.len()
                    && is_ascii_hex(bytes[i + 2])
                    && is_ascii_hex(bytes[i + 3]) =>
                {
                    out.extend_from_slice(JSON_ESC_PREFIX);
                    out.push(bytes[i + 2]);
                    out.push(bytes[i + 3]);
                    i += 4;
                    continue;
                }
                b'v' => {
                    out.extend_from_slice(JSON_ESC_VTAB);
                    i += 2;
                    continue;
                }
                b'0' if i + 2 < bytes.len() && !bytes[i + 2].is_ascii_digit() => {
                    out.extend_from_slice(JSON_ESC_NUL);
                    i += 2;
                    continue;
                }
                b'\'' => {
                    // JSON doesn't recognize `\'`; the single quote
                    // is not a JSON special char, so emit a bare `'`.
                    out.push(b'\'');
                    i += 2;
                    continue;
                }
                _ => {
                    // Valid JSON escape (or unknown — pass through
                    // and let serde_json reject it cleanly).
                    out.push(b);
                    out.push(next);
                    i += 2;
                    continue;
                }
            }
        }
        out.push(b);
        i += 1;
    }
    // Buffer is UTF-8 by construction: input was a `&str` (valid
    // UTF-8) and we only inserted ASCII replacement bytes between
    // existing bytes, never splitting a multi-byte sequence. The
    // `from_utf8` validity check is cheap and avoids `unsafe` per
    // the crate's `unsafe_code = "forbid"` policy.
    String::from_utf8(out).expect("rewrite preserves UTF-8 by construction")
}

/// ASCII byte slices used by [`rewrite_js_only_escapes`]. Spelled out
/// element-by-element so no string-escape sequences appear in this
/// source — the literal `` would otherwise be at risk of being
/// re-processed at edit time. Same reason `IDENT_PATH` uses
/// `[A-Za-z_$]` rather than a more compact `\w`-style class.
#[allow(clippy::byte_char_slices)]
const JSON_ESC_PREFIX: &[u8] = &[b'\\', b'u', b'0', b'0'];
#[allow(clippy::byte_char_slices)]
const JSON_ESC_VTAB: &[u8] = &[b'\\', b'u', b'0', b'0', b'0', b'b'];
#[allow(clippy::byte_char_slices)]
const JSON_ESC_NUL: &[u8] = &[b'\\', b'u', b'0', b'0', b'0', b'0'];

#[inline]
fn is_ascii_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || (b'A'..=b'F').contains(&b)
}

/// LHS pattern: a JS identifier followed by zero or more `.ident` or
/// `["str"]` continuations. Used by both RHS regexes below as the
/// shared left-hand side; kept in one constant so the two regexes
/// can't drift apart.
const IDENT_PATH: &str = r#"[A-Za-z_$][A-Za-z0-9_$]*(?:\s*(?:\.\s*[A-Za-z_$][A-Za-z0-9_$]*|\[\s*"(?:[^"\\]|\\.)*"\s*\]))*"#;

/// Full regex source for `<path> = JSON.parse("...")`. Group 1 is the
/// LHS path; group 2 is the JS string literal (with quotes) passed to
/// `JSON.parse`. Built once at module load (via `concat!`) so we keep
/// the LHS in one place.
const IDENT_PATH_LHS_PATTERN_JSON_PARSE: &str = concat!(
    r#"("#,
    r#"[A-Za-z_$][A-Za-z0-9_$]*(?:\s*(?:\.\s*[A-Za-z_$][A-Za-z0-9_$]*|\[\s*"(?:[^"\\]|\\.)*"\s*\]))*"#,
    r#")\s*=\s*JSON\s*\.\s*parse\s*\(\s*"#,
    r#"("(?:[^"\\]|\\.)*")"#,
    r#"\s*\)"#,
);

/// Full regex source for `<path> = {`. Group 1 is the LHS path; group 2
/// is the literal `{` opener (we capture it only to learn its byte
/// position; the actual object body is found via brace matching).
const IDENT_PATH_LHS_PATTERN_OBJECT_LITERAL: &str = concat!(
    r#"("#,
    r#"[A-Za-z_$][A-Za-z0-9_$]*(?:\s*(?:\.\s*[A-Za-z_$][A-Za-z0-9_$]*|\[\s*"(?:[^"\\]|\\.)*"\s*\]))*"#,
    r#")\s*=\s*(\{)"#,
);

/// Suppress the `dead_code` lint on [`IDENT_PATH`]: it's kept as the
/// source-of-truth fragment so the two compiled regexes can be audited
/// against the same pattern. The full regexes inline an expanded copy
/// because Rust `concat!` doesn't accept non-literal constants.
#[allow(dead_code)]
const _IDENT_PATH_AUDIT: &str = IDENT_PATH;

/// Normalize an identifier path captured by the regex into a stable,
/// agent-friendly key.
///
/// Rules:
/// - rewrite `["foo"]` indexing as `.foo` (drop the brackets and quotes)
/// - drop whitespace inside the path so `obj . key` and `obj.key`
///   produce the same key
/// - strip a leading `window.` since the global scope marker is
///   implicit (so `window.__APOLLO_STATE__` and
///   `window["__APOLLO_STATE__"]` both produce `__APOLLO_STATE__`)
///
/// Returns the empty string if the path was empty after normalization;
/// callers should drop empty keys.
fn normalize_assignment_key(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len());
    let mut in_bracket = false;
    let mut escape_next = false;
    for c in raw.chars() {
        if escape_next {
            escape_next = false;
            if in_bracket {
                s.push(c);
            }
            continue;
        }
        match c {
            '[' => {
                in_bracket = true;
                s.push('.');
            }
            ']' => {
                in_bracket = false;
            }
            '"' if in_bracket => {
                // Drop the bracket-string delimiters.
            }
            '\\' if in_bracket => {
                escape_next = true;
            }
            ' ' | '\t' | '\n' | '\r' => {
                // Whitespace inside an identifier path is just noise.
            }
            other => s.push(other),
        }
    }
    if let Some(rest) = s.strip_prefix("window.") {
        rest.to_owned()
    } else {
        s
    }
}

/// True if the script's `type` attribute means "executable JavaScript"
/// (or is absent — HTML default). We skip `application/json` here
/// because that's handled by [`extract_application_json`], and we skip
/// `application/ld+json` because that's owned by [`crate::metadata`].
///
/// Accepts: missing/empty `type`, `text/javascript`,
/// `application/javascript`, and `module` (ES modules — `<script
/// type="module">`). Modules can ship hydration assignments just like
/// classic scripts, and skipping them silently drops real data.
fn is_plain_js_script(type_attr: Option<&str>) -> bool {
    match type_attr {
        None => true,
        Some(t) => {
            let t = t.trim();
            t.is_empty()
                || t.eq_ignore_ascii_case("text/javascript")
                || t.eq_ignore_ascii_case("application/javascript")
                || t.eq_ignore_ascii_case("module")
        }
    }
}

/// Strip JS `//` line comments and `/* ... */` block comments from a
/// script body. Replaces comment bytes with ASCII spaces so byte
/// positions (used by [`find_matching_brace`] and regex captures) are
/// preserved exactly. String literals (`"..."`, `'...'`,
/// `` `...` ``) are respected: a `//` or `/*` inside a string is
/// left untouched.
///
/// Conservative: doesn't try to parse JS regex literals — that's the
/// same conservative stance taken by [`find_matching_brace`]. Real SSR
/// hydration assignments don't contain raw regex literals near the
/// LHS, so this is safe in practice.
fn strip_js_comments(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_string = false;
    let mut string_quote: u8 = 0;
    let mut escape_next = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escape_next {
                escape_next = false;
            } else if b == b'\\' {
                escape_next = true;
            } else if b == string_quote {
                in_string = false;
            }
            out.push(b);
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' || b == b'`' {
            in_string = true;
            string_quote = b;
            out.push(b);
            i += 1;
            continue;
        }
        // Line comment: `//...` until end of line.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            // Blank out bytes until newline (preserving the newline so
            // line counts and any newline-sensitive scans still work).
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        // Block comment: `/* ... */`.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            out.push(b' ');
            out.push(b' ');
            i += 2;
            while i < bytes.len() {
                if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    out.push(b' ');
                    out.push(b' ');
                    i += 2;
                    break;
                }
                // Preserve newlines so line-based positions don't
                // shift; replace everything else inside the comment
                // with a space.
                if bytes[i] == b'\n' {
                    out.push(b'\n');
                } else {
                    out.push(b' ');
                }
                i += 1;
            }
            continue;
        }
        out.push(b);
        i += 1;
    }
    // Output is UTF-8 by construction: all replacement bytes are ASCII
    // and we never split a multi-byte sequence (strings are passed
    // through verbatim, comments only ever contain ASCII space/newline
    // replacements).
    String::from_utf8(out).expect("strip_js_comments preserves UTF-8 by construction")
}

/// Find the position of the `}` that closes the `{` at `open_pos`,
/// respecting string literals (`"`, `'`, `` ` ``) so braces inside
/// strings don't fool the matcher.
///
/// Returns `None` if the input is unbalanced. Conservative: doesn't
/// understand JS regex literals, template literal expressions
/// (`${...}`), or comments — but real SSR-emitted state is
/// `JSON.stringify` output, so those constructs don't appear inside it.
fn find_matching_brace(s: &str, open_pos: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.get(open_pos) != Some(&b'{') {
        return None;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut string_quote: u8 = 0;
    let mut escape_next = false;
    let mut i = open_pos;
    while i < bytes.len() {
        let b = bytes[i];
        if escape_next {
            escape_next = false;
            i += 1;
            continue;
        }
        if in_string {
            if b == b'\\' {
                escape_next = true;
            } else if b == string_quote {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => {
                in_string = true;
                string_quote = b;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(html: &str) -> Html {
        Html::parse_document(html)
    }

    // ---- application/json (existing coverage) ----

    #[test]
    fn extracts_named_application_json_block() {
        let html = r#"
            <html><head>
              <script id="__NEXT_DATA__" type="application/json">
              {"props":{"pageProps":{"title":"Hello"}},"page":"/"}
              </script>
            </head><body></body></html>
        "#;
        let data = extract(&parse(html));
        assert_eq!(data.len(), 1);
        let v = data.get("__NEXT_DATA__").expect("named __NEXT_DATA__ key");
        assert_eq!(v["page"], "/");
        assert_eq!(v["props"]["pageProps"]["title"], "Hello");
    }

    #[test]
    fn ignores_application_ld_plus_json() {
        let html = r#"
            <html><head>
              <script type="application/ld+json">
              {"@type":"Organization","name":"Acme"}
              </script>
              <script id="state" type="application/json">
              {"counter":1}
              </script>
            </head><body></body></html>
        "#;
        let data = extract(&parse(html));
        assert_eq!(data.len(), 1, "ld+json should NOT be in inline_data");
        assert!(data.contains_key("state"));
    }

    #[test]
    fn unnamed_scripts_get_synthetic_keys_in_document_order() {
        let html = r#"
            <html><head>
              <script type="application/json">{"a":1}</script>
              <script type="application/json">{"b":2}</script>
              <script id="named" type="application/json">{"c":3}</script>
              <script type="application/json">{"d":4}</script>
            </head><body></body></html>
        "#;
        let data = extract(&parse(html));
        assert_eq!(data.len(), 4);
        assert_eq!(data.get("unnamed-0").unwrap()["a"], 1);
        assert_eq!(data.get("unnamed-1").unwrap()["b"], 2);
        assert_eq!(data.get("named").unwrap()["c"], 3);
        assert_eq!(data.get("unnamed-2").unwrap()["d"], 4);
    }

    #[test]
    fn malformed_blocks_are_skipped() {
        let html = r#"
            <html><head>
              <script id="bad" type="application/json">{ not json }</script>
              <script id="good" type="application/json">{"ok":true}</script>
            </head><body></body></html>
        "#;
        let data = extract(&parse(html));
        assert_eq!(data.len(), 1);
        assert!(data.contains_key("good"));
        assert!(!data.contains_key("bad"));
    }

    #[test]
    fn empty_page_yields_empty_map() {
        let data = extract(&parse("<html><body></body></html>"));
        assert!(data.is_empty());
    }

    #[test]
    fn empty_or_whitespace_blocks_are_skipped() {
        let html = r#"
            <html><head>
              <script id="empty" type="application/json"></script>
              <script id="whitespace" type="application/json">


              </script>
              <script id="real" type="application/json">{"v":1}</script>
            </head><body></body></html>
        "#;
        let data = extract(&parse(html));
        assert_eq!(data.len(), 1);
        assert!(data.contains_key("real"));
    }

    #[test]
    fn extracts_apple_acgh_data_shape() {
        let html = r#"
            <html><head>
              <script id="__ACGH_DATA__" type="application/json">
              {"channel":"acgh","content":{"hero":"iPhone 17","cta":"Buy"},"locale":"en-CA"}
              </script>
            </head><body></body></html>
        "#;
        let data = extract(&parse(html));
        let acgh = data.get("__ACGH_DATA__").expect("ACGH payload");
        assert_eq!(acgh["channel"], "acgh");
        assert_eq!(acgh["content"]["hero"], "iPhone 17");
        assert_eq!(acgh["locale"], "en-CA");
    }

    #[test]
    fn output_is_btreemap_sorted() {
        let html = r#"
            <html><head>
              <script id="zebra" type="application/json">{"x":1}</script>
              <script id="alpha" type="application/json">{"x":2}</script>
              <script id="mango" type="application/json">{"x":3}</script>
            </head><body></body></html>
        "#;
        let data = extract(&parse(html));
        let keys: Vec<&String> = data.keys().collect();
        assert_eq!(keys, vec!["alpha", "mango", "zebra"]);
    }

    // ---- RSC (__next_f) ----

    #[test]
    fn extracts_simple_next_f_stream() {
        // Two pushes that together encode three flight lines:
        //   0:["$","$L1",null,{"buildId":"abc"}]
        //   2:I["./page"]
        //   3:["hello"]
        let html = r#"
            <html><body>
              <script>self.__next_f=self.__next_f||[]</script>
              <script>self.__next_f.push([0])</script>
              <script>self.__next_f.push([1,"0:[\"$\",\"$L1\",null,{\"buildId\":\"abc\"}]\n2:I[\"./page\"]"])</script>
              <script>self.__next_f.push([1,"\n3:[\"hello\"]"])</script>
            </body></html>
        "#;
        let data = extract(&parse(html));
        let arr = data
            .get("__next_f")
            .expect("__next_f present")
            .as_array()
            .expect("array");
        assert_eq!(arr.len(), 3, "three flight lines expected");
        assert_eq!(arr[0]["id"], "0");
        assert_eq!(arr[0]["value"][0], "$");
        assert_eq!(arr[0]["value"][1], "$L1");
        assert_eq!(arr[0]["value"][3]["buildId"], "abc");
        assert_eq!(arr[1]["id"], "2");
        // The "I[...]" reference isn't valid JSON; should fall through
        // to a raw string preserving the structure.
        assert!(arr[1]["value"].as_str().unwrap().starts_with("I[\""));
        assert_eq!(arr[2]["id"], "3");
        assert_eq!(arr[2]["value"][0], "hello");
    }

    #[test]
    fn next_f_chunks_can_split_mid_line() {
        // Two pushes where the second extends the line started by the
        // first. After unescape+concat we should see one line:
        //   0:{"a":1,"b":2}
        let html = r#"
            <html><body>
              <script>self.__next_f.push([1,"0:{\"a\":1,"])</script>
              <script>self.__next_f.push([1,"\"b\":2}"])</script>
            </body></html>
        "#;
        let data = extract(&parse(html));
        let arr = data["__next_f"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "0");
        assert_eq!(arr[0]["value"]["a"], 1);
        assert_eq!(arr[0]["value"]["b"], 2);
    }

    #[test]
    fn page_without_next_f_doesnt_emit_key() {
        let html = r#"
            <html><body>
              <script>console.log('hi')</script>
            </body></html>
        "#;
        let data = extract(&parse(html));
        assert!(!data.contains_key("__next_f"));
    }

    // ---- window.X assignments ----

    #[test]
    fn extracts_apollo_state_object_literal() {
        let html = r##"
            <html><body>
              <script>window.__APOLLO_STATE__ = {"User:1":{"id":1,"name":"Alice"}};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data
            .get("__APOLLO_STATE__")
            .expect("Apollo state should be present");
        assert_eq!(v["User:1"]["id"], 1);
        assert_eq!(v["User:1"]["name"], "Alice");
    }

    #[test]
    fn extracts_preloaded_state_via_json_parse() {
        // Redux/Next.js classic pattern: stringify, escape, JSON.parse
        // on the client. Inside the JS string the inner JSON is
        // double-escaped: outer = JS string, inner = JSON document.
        let html = r#"
            <html><body>
              <script>window.__PRELOADED_STATE__ = JSON.parse("{\"count\":7,\"user\":\"bob\"}");</script>
            </body></html>
        "#;
        let data = extract(&parse(html));
        let v = data
            .get("__PRELOADED_STATE__")
            .expect("preloaded state present");
        assert_eq!(v["count"], 7);
        assert_eq!(v["user"], "bob");
    }

    #[test]
    fn extracts_bracket_assignment_window_form() {
        let html = r##"
            <html><body>
              <script>window["__INITIAL__"] = {"ok":true};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data.get("__INITIAL__").expect("bracket form captured");
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn skips_window_assignment_with_non_json_values() {
        // Real JS but not JSON-shaped: unquoted keys, function refs,
        // undefined. We reject these rather than emit junk.
        let html = r##"
            <html><body>
              <script>window.__BAD__ = {key: undefined, fn: function(){}};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert!(!data.contains_key("__BAD__"));
    }

    #[test]
    fn brace_matcher_handles_nested_objects_and_string_braces() {
        // The "ugh}" inside a string should not be mistaken for the
        // closing brace.
        let html = r##"
            <html><body>
              <script>window.__X__ = {"a":{"b":"ugh}"}};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data.get("__X__").expect("nested object captured");
        assert_eq!(v["a"]["b"], "ugh}");
    }

    #[test]
    fn json_parse_pattern_takes_priority_over_object_literal_first_wins() {
        // Two assignments to the same name; JSON.parse runs first and
        // should win via or_insert.
        let html = r#"
            <html><body>
              <script>window.__DUPE__ = JSON.parse("{\"from\":\"parse\"}");</script>
              <script>window.__DUPE__ = {"from":"literal"};</script>
            </body></html>
        "#;
        let data = extract(&parse(html));
        let v = data.get("__DUPE__").expect("dupe should resolve to first");
        assert_eq!(v["from"], "parse");
    }

    #[test]
    fn application_json_wins_over_window_assign_on_collision() {
        // Hypothetical: a page names its application/json id the same
        // as a window var. application/json runs first and should win.
        let html = r##"
            <html><body>
              <script id="__SHARED__" type="application/json">{"src":"app-json"}</script>
              <script>window.__SHARED__ = {"src":"window"};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data.get("__SHARED__").expect("shared key");
        assert_eq!(v["src"], "app-json");
    }

    #[test]
    fn extracts_namespace_dotted_assignment_netflix_shape() {
        // Regression for the netflix.com pattern: `window.netflix =
        // window.netflix || {};` followed by `netflix.reactContext =
        // {...}`. The hydration data is hung off a custom namespace,
        // not directly on `window`. A general extractor must catch
        // these without per-site knowledge.
        let html = r##"
            <html><body>
              <script>
                window.netflix = window.netflix || {};
                netflix.reactContext = {"models":{"esn":{"data":"ABC123"}},"locale":"en-CA"};
              </script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data
            .get("netflix.reactContext")
            .expect("netflix.reactContext should be captured under its full namespace path");
        assert_eq!(v["models"]["esn"]["data"], "ABC123");
        assert_eq!(v["locale"], "en-CA");
    }

    #[test]
    fn extracts_top_level_var_assignment() {
        // `var __DATA__ = {...}` inline scripts — common in older
        // hand-rolled hydration. The `var` keyword sits to the left of
        // the identifier and the regex correctly anchors on the
        // identifier rather than the keyword.
        let html = r##"
            <html><body>
              <script>var __DATA__ = {"items":[1,2,3]};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data.get("__DATA__").expect("__DATA__ captured");
        assert_eq!(v["items"][1], 2);
    }

    #[test]
    fn normalizes_bracket_indexing_to_dot_notation_in_key() {
        // `a["b"].c = {...}` and `a.b.c = {...}` should produce the
        // same key, so agents can find the data regardless of which
        // syntax the page used.
        let html_bracket = r##"
            <html><body>
              <script>config["nested"].value = {"answer":42};</script>
            </body></html>
        "##;
        let html_dot = r##"
            <html><body>
              <script>config.nested.value = {"answer":42};</script>
            </body></html>
        "##;
        let d1 = extract(&parse(html_bracket));
        let d2 = extract(&parse(html_dot));
        assert!(d1.contains_key("config.nested.value"));
        assert!(d2.contains_key("config.nested.value"));
        assert_eq!(d1["config.nested.value"], d2["config.nested.value"]);
    }

    #[test]
    fn deep_dotted_path_assignments_preserve_full_namespace() {
        let html = r##"
            <html><body>
              <script>app.feature.flags = {"darkMode":true,"experiment":"foo"};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data.get("app.feature.flags").expect("deep path captured");
        assert_eq!(v["darkMode"], true);
        assert_eq!(v["experiment"], "foo");
    }

    #[test]
    fn extracts_namespace_with_json_parse_too() {
        // The generic rule applies to JSON.parse just like object
        // literals — no special-casing of `window.` required.
        let html = r#"
            <html><body>
              <script>store.initial = JSON.parse("{\"k\":\"v\"}");</script>
            </body></html>
        "#;
        let data = extract(&parse(html));
        let v = data.get("store.initial").expect("store.initial captured");
        assert_eq!(v["k"], "v");
    }

    #[test]
    fn skips_object_literal_with_function_body_inside() {
        // A function literal nested in the RHS will not parse as JSON.
        // We must not extract it under a misleading key.
        let html = r##"
            <html><body>
              <script>obj.handler = {onClick: function(){return 1;}};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert!(!data.contains_key("obj.handler"));
    }

    #[test]
    fn first_write_wins_across_general_assignment_pattern() {
        // Two assignments to the same path — first should win.
        let html = r##"
            <html><body>
              <script>app.cfg = {"v":1};</script>
              <script>app.cfg = {"v":2};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert_eq!(data["app.cfg"]["v"], 1);
    }

    #[test]
    fn empty_object_assignment_is_skipped_as_namespace_init() {
        // `var ns = {}` is a namespace initializer, not hydration
        // data. Bundled JS polyfills do this dozens of times per
        // page and we shouldn't pollute `inline_data` with empty
        // entries.
        let html = r##"
            <html><body>
              <script>
                var _nativeErrors = {};
                ResizeObserverBoxOptions = {};
                app.cache = {};
              </script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert!(
            data.is_empty(),
            "empty {{}} initializers should not be captured, got keys: {:?}",
            data.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn empty_array_assignment_is_skipped_too() {
        let html = r##"
            <html><body>
              <script>app.events = [];</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert!(data.is_empty());
    }

    #[test]
    fn js_only_hex_escape_in_object_value_is_rewritten_to_json() {
        // Regression for netflix.com: hydration data contains
        // JS-style `\xNN` escapes inside string values. Strict JSON
        // rejects these, so we rewrite to `\u00NN` and retry. The
        // rewrite is a mechanical, universal translation — no
        // per-site code.
        let html = r##"
            <html><body>
              <script>
                window.netflix = window.netflix || {};
                netflix.reactContext = {"models":{"esn":"WWW\x2dBROWSE"},"type":"undefined\x20type"};
              </script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data
            .get("netflix.reactContext")
            .expect("netflix.reactContext should resolve after \\xNN rewrite");
        assert_eq!(v["models"]["esn"], "WWW-BROWSE");
        assert_eq!(v["type"], "undefined type");
    }

    #[test]
    fn rewrite_js_only_escapes_preserves_valid_json_escapes() {
        // Don't double-rewrite `\n`, `\t`, `\uXXXX`, `\"`, `\\` —
        // those are already valid JSON. Output should match input
        // verbatim when no JS-only escapes are present.
        let input =
            r#"{"a":"line1\nline2","b":"tab\there","c":"é","d":"quote\"inside","e":"slash\\here"}"#;
        let out = rewrite_js_only_escapes(input);
        assert_eq!(out, input);
    }

    #[test]
    fn rewrite_js_only_escapes_translates_hex_v_zero_and_apostrophe() {
        // \x20 inside a string ->
        // \v                   ->
        // \0 (not octal)       ->  
        // \'                   -> '
        // After rewrite the string should be strict-JSON parseable.
        let input = r#"{"a":"sp\x20ace","b":"\v","c":"null\0end","d":"\'apostrophe"}"#;
        let out = rewrite_js_only_escapes(input);
        let v: serde_json::Value =
            serde_json::from_str(&out).expect("rewritten string should parse as strict JSON");
        assert_eq!(v["a"], "sp ace");
        assert_eq!(v["b"], "\u{000b}");
        assert_eq!(v["c"], "null\u{0000}end");
        assert_eq!(v["d"], "'apostrophe");
    }

    #[test]
    fn rewrite_js_only_escapes_ignores_escapes_outside_strings() {
        // Anything outside a `"..."` or `'...'` literal is code, not
        // a string — leave it alone.
        let input = r#"{"a":1}\x20{"b":2}"#;
        let out = rewrite_js_only_escapes(input);
        assert_eq!(out, input);
    }

    #[test]
    fn application_json_body_with_utf8_bom_is_parsed() {
        // Some servers (or hand-built blobs) emit a UTF-8 BOM at the
        // start of a JSON document. `serde_json::from_str` rejects a
        // BOM, so a naive trim() leaves the body unparseable. We should
        // strip the BOM before parsing.
        let mut body = String::new();
        body.push('\u{FEFF}');
        body.push_str(r#"{"a":1}"#);
        let html = format!(
            r#"<html><head><script id="bom" type="application/json">{}</script></head><body></body></html>"#,
            body
        );
        let data = extract(&parse(&html));
        let v = data.get("bom").expect("BOM-prefixed JSON should parse");
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn assignment_inside_line_comment_is_not_extracted() {
        // A line comment containing what *looks* like an assignment
        // should not be picked up. Real JS bundlers occasionally leave
        // commented-out examples or debugging stubs in shipped code,
        // and we don't want them surfaced as hydration data.
        let html = r##"
            <html><body>
              <script>
                // window.__FAKE__ = {"src":"comment"};
                window.__REAL__ = {"src":"real"};
              </script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert!(
            !data.contains_key("__FAKE__"),
            "commented assignment must not leak into output; got keys: {:?}",
            data.keys().collect::<Vec<_>>()
        );
        assert_eq!(data["__REAL__"]["src"], "real");
    }

    #[test]
    fn assignment_inside_block_comment_is_not_extracted() {
        let html = r##"
            <html><body>
              <script>
                /* example only:
                   window.__FAKE_BLOCK__ = {"src":"comment"};
                */
                window.__REAL_BLOCK__ = {"src":"real"};
              </script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert!(
            !data.contains_key("__FAKE_BLOCK__"),
            "block-commented assignment must not leak; got keys: {:?}",
            data.keys().collect::<Vec<_>>()
        );
        assert_eq!(data["__REAL_BLOCK__"]["src"], "real");
    }

    #[test]
    fn url_with_double_slash_inside_string_value_is_preserved() {
        // Regression for the strip_js_comments helper: a `//` inside a
        // string literal must NOT be treated as a line comment. This
        // is the most common false-positive risk for naive comment
        // stripping — URLs in JSON values appear in real hydration
        // payloads everywhere.
        let html = r##"
            <html><body>
              <script>window.__LINKS__ = {"home":"https://example.com/path","api":"https://api.example.com/v1"};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data
            .get("__LINKS__")
            .expect("__LINKS__ should parse despite `//` in URL strings");
        assert_eq!(v["home"], "https://example.com/path");
        assert_eq!(v["api"], "https://api.example.com/v1");
    }

    #[test]
    fn next_f_push_with_non_numeric_first_arg_is_ignored() {
        // The RSC contract is `push([NUMBER, "STRING"])`. A non-numeric
        // first arg is malformed — we must not panic or misparse. The
        // regex requires `\d+` for the first slot, so this push should
        // simply not match.
        let html = r#"
            <html><body>
              <script>self.__next_f.push(["x","0:[\"ok\"]"])</script>
              <script>self.__next_f.push([1,"1:[\"good\"]"])</script>
            </body></html>
        "#;
        let data = extract(&parse(html));
        let arr = data["__next_f"].as_array().expect("__next_f present");
        assert_eq!(arr.len(), 1, "only the well-formed push should contribute");
        assert_eq!(arr[0]["id"], "1");
        assert_eq!(arr[0]["value"][0], "good");
    }

    #[test]
    fn next_f_push_with_only_one_arg_is_ignored() {
        // `self.__next_f.push([0])` is the bootstrap form — a single
        // numeric "current chunk index" entry with no payload string.
        // Common in real Next.js pages. We must NOT match this as a
        // push-with-payload (no string to parse) and we must not panic.
        let html = r#"
            <html><body>
              <script>self.__next_f=self.__next_f||[]</script>
              <script>self.__next_f.push([0])</script>
              <script>self.__next_f.push([1,"5:[\"good\"]"])</script>
            </body></html>
        "#;
        let data = extract(&parse(html));
        let arr = data["__next_f"].as_array().expect("__next_f present");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "5");
        assert_eq!(arr[0]["value"][0], "good");
    }

    #[test]
    fn extract_is_deterministic_byte_for_byte() {
        // Two parses of the same input must produce byte-identical
        // serialized output. BTreeMap gives sorted keys, and our
        // unnamed-{n} counter is in document order, so this should
        // hold. This is part of the engine's determinism contract.
        let html = r##"
            <html><body>
              <script id="zebra" type="application/json">{"x":1}</script>
              <script type="application/json">{"y":2}</script>
              <script id="alpha" type="application/json">{"x":3}</script>
              <script>self.__next_f.push([1,"0:[\"hi\"]"])</script>
              <script>window.__APOLLO_STATE__ = {"u":1};</script>
              <script>app.cfg = {"v":2};</script>
            </body></html>
        "##;
        let d1 = extract(&parse(html));
        let d2 = extract(&parse(html));
        let s1 = serde_json::to_string(&d1).expect("serialize d1");
        let s2 = serde_json::to_string(&d2).expect("serialize d2");
        assert_eq!(
            s1, s2,
            "two parses of the same input must serialize identically"
        );
    }

    #[test]
    fn object_with_string_value_containing_only_backslash_is_extracted() {
        // `{"a":"\\"}` — a single backslash inside a string value.
        // The brace matcher's `escape_next` handling must NOT confuse
        // `\\` (an escaped backslash inside a string) for an escape
        // of the closing quote.
        let html = r##"
            <html><body>
              <script>window.__SLASH__ = {"a":"\\"};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data.get("__SLASH__").expect("backslash value extracted");
        assert_eq!(v["a"], "\\");
    }

    #[test]
    fn unterminated_object_literal_does_not_crash_and_is_skipped() {
        // The brace matcher returns None when input ends before a
        // matching `}`. We must not extract a partial value and must
        // not panic.
        let html = r##"
            <html><body>
              <script>window.__BAD__ = {"a":1,"b":"unterminated
            </script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert!(
            !data.contains_key("__BAD__"),
            "unterminated literal must not be extracted; keys: {:?}",
            data.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn assignment_shape_inside_outer_string_is_not_double_extracted() {
        // The OUTER assignment is real hydration data. Inside its
        // string values is text that, taken out of context, looks
        // like its own assignment: `inner = {"x":1}`. A naive regex
        // scan would match that inner text too and emit a phantom
        // `inner` key with garbage data. We should only extract the
        // outer.
        let html = r##"
            <html><body>
              <script>window.__OUTER__ = {"note":"see inner = {\"x\":1} for legacy"};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data.get("__OUTER__").expect("__OUTER__ extracted");
        assert!(
            v["note"].as_str().unwrap().contains("inner = "),
            "outer note preserves its full text"
        );
        assert!(
            !data.contains_key("inner"),
            "phantom inner key must not appear; keys: {:?}",
            data.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn nested_assignment_shape_inside_outer_unquoted_position_does_not_phantom() {
        // Trickier variant: a real JSON-shaped fragment sitting inside
        // a string value of the outer object. The fragment, taken in
        // isolation, IS parseable as JSON. We must NOT promote it.
        let html = r##"
            <html><body>
              <script>window.__OUTER__ = {"example":"compose inner = {\"k\":1} like so"};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert!(data.contains_key("__OUTER__"), "outer should be present");
        assert!(
            !data.contains_key("inner"),
            "inner phantom key must not be promoted; keys: {:?}",
            data.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn very_long_dotted_path_does_not_blow_up_regex() {
        // 100+ dot segments. Rust's `regex` crate is linear-time by
        // design (no catastrophic backtracking), but verify here that
        // the LHS pattern terminates on a pathological dotted path.
        let mut path = String::from("a");
        for _ in 0..200 {
            path.push_str(".x");
        }
        let html = format!(
            r##"
            <html><body>
              <script>{} = {{"hit":true}};</script>
            </body></html>
        "##,
            path
        );
        let start = std::time::Instant::now();
        let data = extract(&parse(&html));
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 2,
            "extraction took {:?} on a 200-segment path — possible regex pathology",
            elapsed
        );
        // It's fine if we don't extract it (very long paths are
        // weird), as long as we don't hang or panic. But if we DO
        // extract, the key should be the full path.
        let _ = data;
    }

    #[test]
    fn duplicate_application_json_ids_first_wins() {
        // Two scripts with the same id — first should win per
        // entry().or_insert(). Document order is the tiebreaker.
        let html = r#"
            <html><head>
              <script id="dupe" type="application/json">{"who":"first"}</script>
              <script id="dupe" type="application/json">{"who":"second"}</script>
            </head><body></body></html>
        "#;
        let data = extract(&parse(html));
        let v = data.get("dupe").expect("dupe key present");
        assert_eq!(
            v["who"], "first",
            "first script in document order should win"
        );
    }

    #[test]
    fn id_attribute_with_surrounding_whitespace_is_trimmed() {
        // HTML may have an `id` attribute with stray whitespace.
        // The code trims the id before using it as a key, so this
        // should produce key "myid", not "  myid  ".
        let html = r#"
            <html><head>
              <script id="  myid  " type="application/json">{"v":1}</script>
            </head><body></body></html>
        "#;
        let data = extract(&parse(html));
        assert!(
            data.contains_key("myid"),
            "id should be trimmed; keys: {:?}",
            data.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn function_declaration_is_not_misidentified_as_assignment() {
        // `function foo() { ... }` has no `=` between the name and
        // the `{`, so the regex anchors `<ident>\s*=\s*\{` should not
        // match. Verify so a future regex tweak doesn't regress.
        let html = r##"
            <html><body>
              <script>
                function loaded() { return {"v":1}; }
                window.__REAL__ = {"ok":true};
              </script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert!(
            !data.contains_key("loaded"),
            "function name leaked: {:?}",
            data.keys().collect::<Vec<_>>()
        );
        assert!(!data.contains_key("foo"), "stray identifier leaked");
        assert_eq!(data["__REAL__"]["ok"], true);
    }

    #[test]
    fn module_script_type_is_treated_as_javascript() {
        // ES modules: `<script type="module">`. These are still JS
        // and may carry hydration assignments. We should scan them.
        // (If we don't today, this test will document a known miss
        // for a future fix.)
        let html = r##"
            <html><body>
              <script type="module">
                window.__MOD_HYD__ = {"loaded":"esm"};
              </script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        let v = data
            .get("__MOD_HYD__")
            .expect("module scripts should be scanned for assignments");
        assert_eq!(v["loaded"], "esm");
    }

    #[test]
    fn mixed_page_captures_all_three_extractors() {
        // application/json, RSC stream, AND window.X — all in one page.
        let html = r##"
            <html><body>
              <script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"title":"Hi"}}}</script>
              <script>self.__next_f.push([1,"0:[\"$\",\"div\",null,{\"children\":\"hi\"}]"])</script>
              <script>window.__APOLLO_STATE__ = {"User:1":{"name":"Alice"}};</script>
            </body></html>
        "##;
        let data = extract(&parse(html));
        assert_eq!(data.len(), 3);
        assert!(data.contains_key("__NEXT_DATA__"));
        assert!(data.contains_key("__next_f"));
        assert!(data.contains_key("__APOLLO_STATE__"));
        assert_eq!(data["__NEXT_DATA__"]["props"]["pageProps"]["title"], "Hi");
        assert_eq!(data["__next_f"][0]["value"][1], "div");
        assert_eq!(data["__APOLLO_STATE__"]["User:1"]["name"], "Alice");
    }
}
