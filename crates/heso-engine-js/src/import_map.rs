//! WHATWG `<script type="importmap">` parser + resolver.
//!
//! Implements the import-map portion of the HTML Standard
//! ([§8.1.5 Module specifier resolution][spec]). A page may declare
//! at most one import map; the caller picks the first
//! `<script type="importmap">` and hands the JSON body and the
//! document's base URL to [`parse_import_map`]. The resulting
//! [`ImportMap`] is consulted by the module loader (when M-A lands)
//! every time JS calls `import "…"` — given the referrer URL of the
//! importing module and the raw specifier string, [`ImportMap::resolve`]
//! returns the resolved absolute URL (or an [`ImportMapError`]).
//!
//! Pages with no `<script type="importmap">` get [`ImportMap::empty`],
//! which short-circuits to plain URL-relative resolution (relative
//! specifiers via [`Url::join`]; bare specifiers error with
//! [`ImportMapError::UnmappedBareSpecifier`]).
//!
//! ## Algorithm — resolve a module specifier
//!
//! Per [the spec][spec], with referrer URL `R` and specifier `S`:
//!
//! 1. Compute `as_url = try_url_like(S, R)` — non-`None` iff `S` looks
//!    URL-shaped (relative `./`, `../`, `/`, or absolute with scheme).
//!    The normalized specifier we look up in the maps is `as_url`'s
//!    serialization when present, else the raw `S` (covers bare names
//!    like `"lodash"` or `"@scope/pkg"`).
//! 2. Iterate matching scopes from most-specific to least-specific:
//!    - First, the exact-match scope whose key equals `R.as_str()`.
//!    - Then every scope whose key ends in `/` and is a prefix of
//!      `R.as_str()`, longest first.
//!
//!    For each scope, attempt an imports match on its specifier map.
//!    The first scope that yields a hit wins.
//! 3. If no scope matched, attempt an imports match on the top-level
//!    `"imports"` map.
//! 4. If neither matched and `as_url` is `Some`, return `as_url` as
//!    the resolved URL (relative + absolute specifiers without any
//!    mapping fall through to plain URL resolution).
//! 5. Else error with [`ImportMapError::UnmappedBareSpecifier`].
//!
//! An "imports match" against a specifier map `M`:
//!
//! - **Exact key**: `M[S]` exists → return its address (or error
//!   with [`ImportMapError::BlockedByNullEntry`] if the value is
//!   `null`).
//! - **Prefix key**: for each key `K` ending in `/`, longest first,
//!   if `S.starts_with(K)`, the resolved URL is the key's address
//!   with the remainder `S[K.len()..]` joined onto it. The result
//!   must still start with the address (no backtracking above the
//!   prefix via `../`).
//!
//! ## OSS reviewed before writing
//!
//! - [`denoland/import_map`](https://github.com/denoland/import_map)
//!   (MIT, the reference Rust implementation of the WICG spec — used
//!   by Deno itself). Cross-checked the spec algorithm against its
//!   `resolve_imports_match`, `resolve_scopes_match`,
//!   `normalize_specifier_key`, and `try_url_like_specifier`. We
//!   re-implement against the same algorithm rather than depend on
//!   the crate: it pulls `boxed_error` + `deno_error` + `indexmap`,
//!   exposes Deno-specific diagnostics, and includes `npm:` / `jsr:`
//!   extensions we don't need. The algorithm itself is ~120 LOC.
//! - [WICG import-maps explainer](https://github.com/WICG/import-maps)
//!   (archived 2025-02 — the spec moved into HTML proper) for the
//!   trailing-slash packages-as-paths rationale.
//! - [SpiderMonkey blog: JavaScript Import maps, Part 2][spidermonkey]
//!   for worked examples of nested scopes and longest-prefix wins.
//!
//! [spec]: https://html.spec.whatwg.org/multipage/webappapis.html#import-maps
//! [spidermonkey]: https://spidermonkey.dev/blog/2023/03/02/javascript-import-maps-part-2-in-depth-exploration.html

use std::cmp::Ordering;

use serde_json::{Map, Value};
use url::Url;

/// A normalized specifier map: every key is sorted by length descending
/// (then lexicographic), so the first key that matches under
/// `starts_with` is automatically the longest prefix. `None` values
/// represent a `"key": null` entry in the JSON, which the spec treats
/// as a deliberate block (every resolve through such a key errors).
type SpecifierMap = Vec<(String, Option<Url>)>;

/// A parsed `<script type="importmap">` body.
///
/// Construct via [`parse_import_map`] for a real map, or
/// [`ImportMap::empty`] for pages that didn't declare one.
#[derive(Debug, Clone)]
pub struct ImportMap {
    /// Top-level `"imports"` map. Keys are normalized (URL-shaped keys
    /// re-serialized via [`Url::parse`] / [`Url::join`]), then sorted
    /// longest-first so prefix lookup is a linear scan.
    imports: SpecifierMap,
    /// `"scopes"` map. Outer keys are normalized scope-prefix URLs
    /// (parsed via `base_url.join(raw_key)`); inner maps share the
    /// `SpecifierMap` shape. Sorted by scope key longest-first.
    scopes: Vec<(String, SpecifierMap)>,
}

/// Anything that can go wrong parsing or resolving an import map.
///
/// Per the spec, "resolve a module specifier" can throw `TypeError`
/// for unresolvable bare specifiers and for null-blocked entries; we
/// surface those as distinct variants so the module loader can map
/// them onto whatever JS error it raises.
#[derive(Debug, thiserror::Error)]
pub enum ImportMapError {
    /// The JSON body did not parse.
    #[error("import map is not valid JSON: {0}")]
    JsonParse(#[from] serde_json::Error),

    /// The top-level JSON value was not an object (`{...}`).
    #[error("import map JSON must be an object")]
    NotObject,

    /// `"imports"` was present but not an object.
    #[error("import map 'imports' must be an object")]
    ImportsNotObject,

    /// `"scopes"` was present but not an object.
    #[error("import map 'scopes' must be an object")]
    ScopesNotObject,

    /// A scope value (the map keyed by `"https://app.example/admin/"`,
    /// etc.) was not an object.
    #[error("import map scope {0:?} must be an object")]
    ScopeNotObject(String),

    /// A bare specifier had no matching entry in any applicable
    /// imports map. Surfaced from [`ImportMap::resolve`].
    #[error("unmapped bare specifier {specifier:?} (referrer {referrer:?})")]
    UnmappedBareSpecifier {
        /// The raw specifier string the JS module requested.
        specifier: String,
        /// The serialization of the referrer URL.
        referrer: String,
    },

    /// The matched key's value was `null` in the JSON — a deliberate
    /// block per the spec (e.g. an import map can shadow an entry
    /// with `"foo": null` to make `import "foo"` throw).
    #[error("resolution of {0:?} blocked by null import map entry")]
    BlockedByNullEntry(String),

    /// A prefix-matched key produced a target URL, but joining the
    /// after-prefix portion of the specifier onto it failed — usually
    /// because the after-prefix portion was syntactically a URL with
    /// its own scheme.
    #[error(
        "could not resolve specifier {specifier:?}: failed to join \
         after-prefix {after_prefix:?} onto address {address}"
    )]
    PrefixJoinFailed {
        /// The specifier under resolution.
        specifier: String,
        /// The portion of the specifier after the prefix key.
        after_prefix: String,
        /// The address the prefix key maps to.
        address: String,
    },

    /// A prefix-matched key resolved to a URL outside the address's
    /// own subtree (the after-prefix portion contained `..` segments
    /// that escaped above the prefix). The spec forbids this so
    /// import maps can't be used as an unrestricted redirect.
    #[error(
        "specifier {specifier:?} backtracks above its mapped prefix {prefix:?}"
    )]
    BacktracksAbovePrefix {
        /// The specifier under resolution.
        specifier: String,
        /// The matched prefix key.
        prefix: String,
    },
}

impl ImportMap {
    /// Returns an empty import map.
    ///
    /// Use this for pages with no `<script type="importmap">` block.
    /// [`resolve`](Self::resolve) on an empty map handles relative
    /// and absolute specifiers via plain URL resolution and errors
    /// with [`ImportMapError::UnmappedBareSpecifier`] on every bare
    /// name.
    pub fn empty() -> Self {
        Self {
            imports: Vec::new(),
            scopes: Vec::new(),
        }
    }

    /// Resolve a specifier against a referrer URL.
    ///
    /// `specifier` is the raw string passed to `import` (or its
    /// dynamic form). `referrer` is the URL of the importing module
    /// (or, for the entry module, the document base URL). On success,
    /// returns the absolute URL the loader should fetch.
    ///
    /// Runs the resolve-a-module-specifier algorithm from
    /// [WHATWG HTML §8.1.5][spec] — module-level docs cover the steps.
    ///
    /// [spec]: https://html.spec.whatwg.org/multipage/webappapis.html#resolve-a-module-specifier
    pub fn resolve(
        &self,
        specifier: &str,
        referrer: &Url,
    ) -> Result<Url, ImportMapError> {
        // Step 1: try to URL-parse the specifier against the
        // referrer. This catches relative (`./`, `../`, `/`) and
        // absolute (`https://…`) specifiers. Bare specifiers like
        // `"lodash"` return `None`.
        let as_url = try_url_like(specifier, referrer);
        let normalized = as_url
            .as_ref()
            .map(|u| u.as_str().to_string())
            .unwrap_or_else(|| specifier.to_string());

        // Step 2: walk matching scopes longest-first. First the exact
        // match against the referrer string, then every prefix scope
        // whose key ends with '/' and is a prefix of the referrer.
        let referrer_str = referrer.as_str();
        if let Some(scope_imports) = self.exact_scope(referrer_str) {
            if let Some(hit) = resolve_imports_match(scope_imports, &normalized)? {
                return Ok(hit);
            }
        }
        for (scope_key, scope_imports) in &self.scopes {
            // The `scopes` Vec is sorted longest-first, so the first
            // prefix match is the longest prefix.
            if scope_key.ends_with('/') && referrer_str.starts_with(scope_key.as_str()) {
                if let Some(hit) = resolve_imports_match(scope_imports, &normalized)? {
                    return Ok(hit);
                }
            }
        }

        // Step 3: top-level imports.
        if let Some(hit) = resolve_imports_match(&self.imports, &normalized)? {
            return Ok(hit);
        }

        // Step 4: unmapped URL-shaped specifier passes through.
        if let Some(url) = as_url {
            return Ok(url);
        }

        // Step 5: bare specifier with no match — error.
        Err(ImportMapError::UnmappedBareSpecifier {
            specifier: specifier.to_string(),
            referrer: referrer.to_string(),
        })
    }

    /// Returns the exact-match scope imports for a given referrer
    /// string, if any. Separate from the prefix walk because the
    /// spec checks the exact key first (a scope key without a
    /// trailing slash only ever matches exactly).
    fn exact_scope(&self, referrer_str: &str) -> Option<&SpecifierMap> {
        self.scopes
            .iter()
            .find(|(k, _)| k == referrer_str)
            .map(|(_, v)| v)
    }

    /// Number of top-level import entries (for inspection / tests).
    #[doc(hidden)]
    pub fn imports_len(&self) -> usize {
        self.imports.len()
    }

    /// Number of scope entries (for inspection / tests).
    #[doc(hidden)]
    pub fn scopes_len(&self) -> usize {
        self.scopes.len()
    }
}

/// Parse a `<script type="importmap">` body.
///
/// `json` is the script element's text content. `base_url` is the
/// document's base URL — used to resolve relative addresses inside
/// the JSON (a value like `"./foo.js"` is parsed against the
/// document base) and to normalize scope keys.
///
/// Per the spec, anything other than `"imports"` and `"scopes"` at
/// the top level is silently ignored (the spec calls these
/// "ignored top-level keys" — `"integrity"`, future extensions, etc.
/// land here). Invalid individual entries inside `"imports"` /
/// `"scopes"` (non-string addresses, addresses that fail to parse)
/// are also tolerated: they become `None` in the map, and any
/// resolve through such a key errors with
/// [`ImportMapError::BlockedByNullEntry`]. Hard structural errors
/// (the top-level value is not an object, `"imports"` is an array,
/// a scope value is a string) reject the whole map.
pub fn parse_import_map(
    json: &str,
    base_url: &Url,
) -> Result<ImportMap, ImportMapError> {
    let value: Value = serde_json::from_str(json)?;
    let mut obj = match value {
        Value::Object(map) => map,
        _ => return Err(ImportMapError::NotObject),
    };

    let imports = match obj.remove("imports") {
        Some(Value::Object(m)) => normalize_specifier_map(m, base_url),
        Some(_) => return Err(ImportMapError::ImportsNotObject),
        None => Vec::new(),
    };

    let scopes = match obj.remove("scopes") {
        Some(Value::Object(m)) => normalize_scopes_map(m, base_url)?,
        Some(_) => return Err(ImportMapError::ScopesNotObject),
        None => Vec::new(),
    };

    // Other top-level keys (e.g. "integrity", future extensions) are
    // silently dropped per spec.
    Ok(ImportMap { imports, scopes })
}

/// Normalize a JSON object into a sorted [`SpecifierMap`].
///
/// Each key is normalized (URL-shaped keys get re-serialized through
/// `Url`, bare keys pass through). Each value is parsed against
/// `base_url`; non-string or unparseable values become `None` (which
/// makes any resolve through them error per spec). Keys that are
/// empty strings or end-with-slash with a value that doesn't end
/// with slash also map to `None` per spec.
fn normalize_specifier_map(map: Map<String, Value>, base_url: &Url) -> SpecifierMap {
    let mut entries: Vec<(String, Option<Url>)> = Vec::with_capacity(map.len());
    for (raw_key, raw_value) in map {
        if raw_key.is_empty() {
            continue;
        }
        let normalized_key = normalize_specifier_key(&raw_key, base_url);

        let address = match raw_value {
            Value::String(s) => try_url_like(&s, base_url),
            Value::Null => None,
            _ => None, // non-string addresses are spec-illegal — drop to None.
        };

        // Spec: if the key ends in '/', the value must too. Otherwise
        // the entry is invalid (None).
        let address = match address {
            Some(url) if raw_key.ends_with('/') && !url.as_str().ends_with('/') => None,
            other => other,
        };

        entries.push((normalized_key, address));
    }
    sort_longest_first(&mut entries);
    entries
}

/// Normalize the `"scopes"` JSON object into a sorted list of
/// (scope-prefix-url, inner-specifier-map) pairs.
///
/// Scope keys are resolved against `base_url` so a key like
/// `"/admin/"` on a document at `https://app.example/` becomes
/// `https://app.example/admin/` — this is what makes the
/// `referrer.starts_with(scope_key)` check work later.
fn normalize_scopes_map(
    map: Map<String, Value>,
    base_url: &Url,
) -> Result<Vec<(String, SpecifierMap)>, ImportMapError> {
    let mut entries: Vec<(String, SpecifierMap)> = Vec::with_capacity(map.len());
    for (raw_scope, value) in map {
        let inner = match value {
            Value::Object(m) => m,
            _ => return Err(ImportMapError::ScopeNotObject(raw_scope)),
        };
        let normalized_scope = match base_url.join(&raw_scope) {
            Ok(url) => url.to_string(),
            // Spec: invalid scope keys are skipped (with a diagnostic).
            // We don't surface diagnostics; silently drop.
            Err(_) => continue,
        };
        let inner_map = normalize_specifier_map(inner, base_url);
        entries.push((normalized_scope, inner_map));
    }
    entries.sort_by(|a, b| length_then_lex(&a.0, &b.0));
    Ok(entries)
}

/// Normalize a specifier-map key. URL-shaped keys get parsed and
/// re-serialized so e.g. `"./foo.js"` against base
/// `https://x/y/` becomes `"https://x/y/foo.js"`; bare keys pass
/// through unchanged.
fn normalize_specifier_key(key: &str, base_url: &Url) -> String {
    try_url_like(key, base_url)
        .map(|u| u.to_string())
        .unwrap_or_else(|| key.to_string())
}

/// Try to parse a specifier as a URL — either relative (joined onto
/// `base`) or absolute. Returns `None` for bare specifiers.
///
/// Mirrors the spec's "parse a URL-like module specifier" — relative
/// specifiers must start with `./`, `../`, or `/`. Anything else is
/// only URL-shaped if [`Url::parse`] accepts it standalone (i.e. it
/// has a scheme). Bare names like `"lodash"` or `"@scope/pkg"` end
/// up `None`.
fn try_url_like(specifier: &str, base: &Url) -> Option<Url> {
    if specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/')
    {
        return base.join(specifier).ok();
    }
    Url::parse(specifier).ok()
}

/// Sort a specifier map so iteration is longest-key-first, with
/// lexicographic order as the tiebreak.
///
/// The prefix-match walk in `resolve_imports_match` relies on this:
/// the first key that satisfies `starts_with` is automatically the
/// most-specific (longest) match.
fn sort_longest_first(entries: &mut SpecifierMap) {
    entries.sort_by(|a, b| length_then_lex(&a.0, &b.0));
}

/// Comparator that orders strings by length descending, then by
/// lexicographic order ascending as a deterministic tiebreak.
fn length_then_lex(a: &str, b: &str) -> Ordering {
    match b.len().cmp(&a.len()) {
        Ordering::Equal => a.cmp(b),
        other => other,
    }
}

/// Try to find a match for `normalized_specifier` in `specifier_map`.
///
/// Returns `Ok(Some(url))` on hit, `Ok(None)` if nothing matched, or
/// `Err(...)` if a matching key had a `null` value (blocked entry)
/// or if a prefix match failed to join.
fn resolve_imports_match(
    specifier_map: &SpecifierMap,
    normalized_specifier: &str,
) -> Result<Option<Url>, ImportMapError> {
    // Exact-match first. We can't use a HashMap because the same Vec
    // also drives the longest-prefix walk; a linear scan is fine for
    // import maps, which are tiny in practice.
    for (key, value) in specifier_map {
        if key == normalized_specifier {
            return match value {
                Some(url) => Ok(Some(url.clone())),
                None => Err(ImportMapError::BlockedByNullEntry(
                    normalized_specifier.to_string(),
                )),
            };
        }
    }

    // Prefix match. Keys are sorted longest-first, so the first
    // prefix hit wins.
    for (key, value) in specifier_map {
        if !key.ends_with('/') {
            continue;
        }
        if !normalized_specifier.starts_with(key.as_str()) {
            continue;
        }
        let address = value.as_ref().ok_or_else(|| {
            ImportMapError::BlockedByNullEntry(key.clone())
        })?;
        let after_prefix = &normalized_specifier[key.len()..];
        let resolved = address.join(after_prefix).map_err(|_| {
            ImportMapError::PrefixJoinFailed {
                specifier: normalized_specifier.to_string(),
                after_prefix: after_prefix.to_string(),
                address: address.to_string(),
            }
        })?;
        // The spec forbids `..` segments in `after_prefix` from
        // escaping above the mapped address — that would let an
        // import map redirect outside its declared subtree.
        if !resolved.as_str().starts_with(address.as_str()) {
            return Err(ImportMapError::BacktracksAbovePrefix {
                specifier: normalized_specifier.to_string(),
                prefix: key.clone(),
            });
        }
        return Ok(Some(resolved));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://app.example/").unwrap()
    }

    #[test]
    fn parses_imports_section() {
        let json = r#"{
            "imports": {
                "lodash": "https://cdn.example/lodash.js",
                "react": "https://cdn.example/react.mjs"
            }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        assert_eq!(map.imports_len(), 2);
        assert_eq!(map.scopes_len(), 0);
    }

    #[test]
    fn parses_scopes_section() {
        let json = r#"{
            "imports": { "lodash": "https://cdn.example/lodash.js" },
            "scopes": {
                "https://app.example/admin/": {
                    "lodash": "https://cdn.example/lodash-v3.js"
                }
            }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        assert_eq!(map.imports_len(), 1);
        assert_eq!(map.scopes_len(), 1);
    }

    #[test]
    fn resolves_bare_specifier_via_imports() {
        let json = r#"{
            "imports": { "lodash": "https://cdn.example/lodash.js" }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        let referrer = Url::parse("https://app.example/page.js").unwrap();
        let resolved = map.resolve("lodash", &referrer).unwrap();
        assert_eq!(resolved.as_str(), "https://cdn.example/lodash.js");
    }

    #[test]
    fn resolves_scoped_specifier_when_referrer_matches() {
        let json = r#"{
            "imports": { "lodash": "https://cdn.example/lodash.js" },
            "scopes": {
                "https://app.example/admin/": {
                    "lodash": "https://cdn.example/lodash-v3.js"
                }
            }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        let referrer = Url::parse("https://app.example/admin/dash.js").unwrap();
        let resolved = map.resolve("lodash", &referrer).unwrap();
        assert_eq!(
            resolved.as_str(),
            "https://cdn.example/lodash-v3.js",
            "scoped match should win over top-level imports"
        );
    }

    #[test]
    fn falls_back_to_imports_when_no_scope_match() {
        let json = r#"{
            "imports": { "lodash": "https://cdn.example/lodash.js" },
            "scopes": {
                "https://app.example/admin/": {
                    "lodash": "https://cdn.example/lodash-v3.js"
                }
            }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        let referrer = Url::parse("https://app.example/public/home.js").unwrap();
        let resolved = map.resolve("lodash", &referrer).unwrap();
        assert_eq!(
            resolved.as_str(),
            "https://cdn.example/lodash.js",
            "non-matching scope should fall through to top-level imports"
        );
    }

    #[test]
    fn resolves_relative_specifier_against_referrer_when_not_in_map() {
        let map = ImportMap::empty();
        let referrer = Url::parse("https://app.example/pages/home.js").unwrap();
        let resolved = map.resolve("./util.js", &referrer).unwrap();
        assert_eq!(resolved.as_str(), "https://app.example/pages/util.js");
        let resolved_up = map.resolve("../other/util.js", &referrer).unwrap();
        assert_eq!(resolved_up.as_str(), "https://app.example/other/util.js");
        let resolved_root = map.resolve("/lib.js", &referrer).unwrap();
        assert_eq!(resolved_root.as_str(), "https://app.example/lib.js");
    }

    #[test]
    fn resolves_absolute_url_passthrough() {
        let map = ImportMap::empty();
        let referrer = Url::parse("https://app.example/p.js").unwrap();
        let resolved = map
            .resolve("https://cdn.example/x.js", &referrer)
            .unwrap();
        assert_eq!(resolved.as_str(), "https://cdn.example/x.js");
    }

    #[test]
    fn errors_on_unresolvable_bare_specifier() {
        let map = ImportMap::empty();
        let referrer = Url::parse("https://app.example/p.js").unwrap();
        let err = map.resolve("lodash", &referrer).unwrap_err();
        assert!(
            matches!(err, ImportMapError::UnmappedBareSpecifier { .. }),
            "expected UnmappedBareSpecifier, got {err:?}"
        );
    }

    #[test]
    fn errors_on_malformed_json() {
        let err = parse_import_map("{ not json", &base()).unwrap_err();
        assert!(
            matches!(err, ImportMapError::JsonParse(_)),
            "expected JsonParse, got {err:?}"
        );
    }

    #[test]
    fn errors_on_non_object_top_level() {
        let err = parse_import_map(r#"["not an object"]"#, &base()).unwrap_err();
        assert!(
            matches!(err, ImportMapError::NotObject),
            "expected NotObject, got {err:?}"
        );
    }

    #[test]
    fn chooses_longest_prefix_scope() {
        // Two overlapping scopes — the longer one must win.
        let json = r#"{
            "scopes": {
                "https://a.example/": {
                    "x": "https://cdn.example/short.js"
                },
                "https://a.example/b/c/": {
                    "x": "https://cdn.example/long.js"
                }
            }
        }"#;
        let map = parse_import_map(json, &Url::parse("https://a.example/").unwrap()).unwrap();
        let referrer = Url::parse("https://a.example/b/c/page.js").unwrap();
        let resolved = map.resolve("x", &referrer).unwrap();
        assert_eq!(
            resolved.as_str(),
            "https://cdn.example/long.js",
            "longer scope prefix should win over shorter"
        );

        // And a referrer under only the shorter scope still hits it.
        let other_referrer = Url::parse("https://a.example/other.js").unwrap();
        let resolved_other = map.resolve("x", &other_referrer).unwrap();
        assert_eq!(resolved_other.as_str(), "https://cdn.example/short.js");
    }

    #[test]
    fn trailing_slash_means_prefix_match_in_imports() {
        // Key "foo/" with value ending in "/" — `"foo/bar"` should
        // resolve to `<address>bar`.
        let json = r#"{
            "imports": {
                "shapes/": "https://cdn.example/shapes/"
            }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        let referrer = Url::parse("https://app.example/p.js").unwrap();
        let resolved = map.resolve("shapes/circle.js", &referrer).unwrap();
        assert_eq!(resolved.as_str(), "https://cdn.example/shapes/circle.js");
        let resolved_deep = map.resolve("shapes/sub/triangle.js", &referrer).unwrap();
        assert_eq!(
            resolved_deep.as_str(),
            "https://cdn.example/shapes/sub/triangle.js"
        );
    }

    #[test]
    fn longest_prefix_wins_in_imports() {
        // Both "shapes/" and "shapes/sub/" match `"shapes/sub/x.js"`;
        // the longer key wins.
        let json = r#"{
            "imports": {
                "shapes/":     "https://cdn.example/general/",
                "shapes/sub/": "https://cdn.example/specific/"
            }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        let referrer = Url::parse("https://app.example/p.js").unwrap();
        let resolved = map.resolve("shapes/sub/x.js", &referrer).unwrap();
        assert_eq!(resolved.as_str(), "https://cdn.example/specific/x.js");
    }

    #[test]
    fn null_entry_blocks_resolution() {
        // `"foo": null` means "this is intentionally blocked".
        let json = r#"{
            "imports": { "foo": null }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        let referrer = Url::parse("https://app.example/p.js").unwrap();
        let err = map.resolve("foo", &referrer).unwrap_err();
        assert!(
            matches!(err, ImportMapError::BlockedByNullEntry(_)),
            "expected BlockedByNullEntry, got {err:?}"
        );
    }

    #[test]
    fn ignored_top_level_keys_dont_error() {
        // The `"integrity"` key is allowed per spec but we don't store
        // it yet — the spec says implementations must tolerate unknown
        // top-level keys. Should parse, not error.
        let json = r#"{
            "imports": { "react": "https://cdn.example/react.mjs" },
            "integrity": { "https://cdn.example/react.mjs": "sha384-abc" }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        assert_eq!(map.imports_len(), 1);
    }

    #[test]
    fn absolute_url_with_exact_imports_key_is_remapped() {
        // "specifier-shaped key" case: an absolute URL specifier that
        // exactly matches an `imports` key gets substituted. The
        // import-map key gets normalized to its absolute form during
        // parse, so a JSON key like "https://cdn/x.js" lines up with
        // the absolute specifier "https://cdn/x.js".
        let json = r#"{
            "imports": {
                "https://cdn.example/old.js": "https://cdn.example/new.js"
            }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        let referrer = Url::parse("https://app.example/p.js").unwrap();
        let resolved = map
            .resolve("https://cdn.example/old.js", &referrer)
            .unwrap();
        assert_eq!(resolved.as_str(), "https://cdn.example/new.js");
    }

    #[test]
    fn scope_imports_fall_through_to_top_level_when_specifier_not_in_scope() {
        // A scope matches the referrer, but the specifier isn't in
        // that scope's imports — must fall back to top-level imports
        // (not error). This is the "scope match but no specifier"
        // case from the spec.
        let json = r#"{
            "imports": { "react": "https://cdn.example/react.mjs" },
            "scopes": {
                "https://app.example/admin/": {
                    "lodash": "https://cdn.example/lodash-v3.js"
                }
            }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        let referrer = Url::parse("https://app.example/admin/dash.js").unwrap();
        let resolved = map.resolve("react", &referrer).unwrap();
        assert_eq!(resolved.as_str(), "https://cdn.example/react.mjs");
    }

    #[test]
    fn relative_address_normalized_against_base_url() {
        // A JSON address like "./util.js" should be normalized to an
        // absolute URL using the document base, so resolution returns
        // an absolute URL even though the import map wrote a relative
        // path.
        let json = r#"{
            "imports": { "util": "./util.js" }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        let referrer = Url::parse("https://app.example/deep/page.js").unwrap();
        let resolved = map.resolve("util", &referrer).unwrap();
        assert_eq!(resolved.as_str(), "https://app.example/util.js");
    }

    #[test]
    fn scope_key_normalized_against_base_url() {
        // A scope key like "/admin/" should be normalized to an
        // absolute URL using the base, then matched against the
        // referrer string.
        let json = r#"{
            "imports": { "x": "https://cdn.example/default.js" },
            "scopes": {
                "/admin/": { "x": "https://cdn.example/admin.js" }
            }
        }"#;
        let map = parse_import_map(json, &base()).unwrap();
        let referrer = Url::parse("https://app.example/admin/page.js").unwrap();
        let resolved = map.resolve("x", &referrer).unwrap();
        assert_eq!(resolved.as_str(), "https://cdn.example/admin.js");
    }

    #[test]
    fn empty_map_passes_relative_specifiers_through() {
        let map = ImportMap::empty();
        let referrer = Url::parse("https://app.example/a/b/").unwrap();
        let resolved = map.resolve("./c.js", &referrer).unwrap();
        assert_eq!(resolved.as_str(), "https://app.example/a/b/c.js");
    }
}
