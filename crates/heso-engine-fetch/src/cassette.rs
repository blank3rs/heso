//! # cassette
//!
//! Network-trace recording for [ADR 0008]-mandated byte-identical
//! replay. A [`Cassette`] is a deterministic, ordered log of every
//! `(method, url, request-body, status, response-headers, response-
//! body)` tuple that the engine observed during a recording run.
//! Replays consult the cassette by exact-match lookup on
//! `(method, url, request-body)`; misses surface as [`CassetteMiss`]
//! errors rather than silently re-fetching, so a website that has
//! drifted since the recording is *visible* to the agent instead of
//! quietly producing a different plat.
//!
//! The cassette serializes deterministically (JCS-compatible because
//! its only fields are strings, integers, and ordered arrays), which
//! means it can ride inside the plat body and contribute to the plat
//! hash. Tampering with the cassette changes the plat hash; the
//! signed receipt over the canonical bytes is the integrity proof.
//!
//! ## Wire shape
//!
//! Bodies are stored as standard base64 strings (RFC 4648). Plain
//! base64 is portable across JSON consumers, deterministic in length,
//! and avoids escape ambiguities — strictly cheaper than embedding
//! UTF-8 + a binary fallback path. Empty bodies are the empty string,
//! not `null`, so a record with no request body is still a complete
//! `Record`.
//!
//! ```text
//! {
//!   "records": [
//!     {
//!       "method": "GET",
//!       "url":    "https://example.com/",
//!       "request_body_b64":  "",
//!       "status":  200,
//!       "response_headers": [["content-type","text/html"], …],
//!       "response_body_b64": "PCFET0NUWVBFIGh0bWw+…"
//!     }
//!   ]
//! }
//! ```
//!
//! ## Lookup semantics
//!
//! [`Cassette::lookup`] matches the *first* record whose
//! `(method, url, request_body_b64)` triple equals the request — the
//! cassette is order-preserving so a page that fetches the same URL
//! twice with different post-bodies (e.g. a POST followed by a GET)
//! gets distinct records.
//!
//! [ADR 0008]: ../../decisions/0008-deterministic-execution.md

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

/// A single recorded HTTP request/response pair.
///
/// Field order in this struct matches the canonical-JSON output order
/// after [`serde_jcs`] runs (which sorts object keys alphabetically),
/// so a reader of the wire shape sees the same key order regardless
/// of the field declaration here. Kept aligned for source readability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// HTTP method, uppercase canonical form: `"GET"`, `"POST"`,
    /// `"PUT"`, `"DELETE"`, `"PATCH"`, `"HEAD"`, `"OPTIONS"`.
    pub method: String,
    /// Final URL as the engine saw it, after `Url::parse`
    /// normalization (lowercase scheme + host, percent-encoding
    /// canonicalized).
    pub url: String,
    /// Base64 of the request body bytes. Empty string for `GET` /
    /// `HEAD` / any request without a body.
    pub request_body_b64: String,
    /// HTTP status code observed in the response.
    pub status: u16,
    /// Response headers as ordered `(name, value)` pairs. Preserves
    /// repeated headers (`Set-Cookie` can appear multiple times) and
    /// the server's exact ordering so replay produces byte-identical
    /// `headers` lists. Lowercased name per HTTP/2; raw value.
    pub response_headers: Vec<(String, String)>,
    /// Base64 of the response body bytes.
    pub response_body_b64: String,
}

/// A sequence of recorded requests captured during a stamping run.
///
/// `records` are kept in insertion order — the first call to
/// `record(...)` is `records[0]`, the second is `records[1]`, etc.
/// Replay walks records front-to-back the same way the original run
/// produced them, so two requests to the same URL with different
/// bodies (or two identical requests in sequence) are disambiguated
/// by position when the body alone would not differentiate them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cassette {
    /// Recorded request/response pairs in the order they were
    /// captured during the stamping run.
    pub records: Vec<Record>,
}

impl Cassette {
    /// Construct an empty cassette ready to accept [`Self::record`]
    /// calls.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a (request, response) pair to the cassette.
    ///
    /// Headers are stored verbatim. Bodies are base64-encoded at
    /// store time. Method is uppercased (HTTP method names are case-
    /// insensitive per RFC 9110 §9.1 but the wire form is uppercase).
    pub fn record(
        &mut self,
        method: &str,
        url: &str,
        request_body: &[u8],
        status: u16,
        response_headers: Vec<(String, String)>,
        response_body: &[u8],
    ) {
        self.records.push(Record {
            method: method.to_ascii_uppercase(),
            url: url.to_owned(),
            request_body_b64: B64.encode(request_body),
            status,
            response_headers,
            response_body_b64: B64.encode(response_body),
        });
    }

    /// Find the first record whose `(method, url, request_body)`
    /// triple matches the query. Returns `None` if no record
    /// matches; the caller is expected to surface a [`CassetteMiss`]
    /// to the agent rather than silently degrading to a live fetch.
    ///
    /// Method comparison is case-insensitive on the query side so a
    /// caller passing `"get"` matches a recorded `"GET"`. URL and
    /// body comparison are byte-exact — any normalization must
    /// happen before the lookup.
    pub fn lookup(&self, method: &str, url: &str, request_body: &[u8]) -> Option<&Record> {
        let method_upper = method.to_ascii_uppercase();
        let body_b64 = B64.encode(request_body);
        self.records
            .iter()
            .find(|r| r.method == method_upper && r.url == url && r.request_body_b64 == body_b64)
    }

    /// Decode the response body bytes for `record`. Helper that wraps
    /// the base64 decode; reserved for callers that have a `&Record`
    /// in hand from [`Self::lookup`] or by indexing `records`.
    pub fn decode_response_body(record: &Record) -> Result<Vec<u8>, base64::DecodeError> {
        B64.decode(record.response_body_b64.as_bytes())
    }

    /// Total number of records on the cassette. Convenience for the
    /// [`CassetteMiss`] error message and for tests that assert
    /// record counts.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// `true` iff the cassette has no records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Surfaced when a replaying client is asked for a request that the
/// cassette does not contain. The agent receives a structured error
/// instead of a silent live fetch — the contract from ADR 0008.
///
/// The `Display` form is the human-readable, debugger-friendly
/// message; the structured fields (`method`, `url`, `recorded_count`)
/// are what programmatic callers should match on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "cassette miss: {method} {url} not recorded (cassette has {recorded_count} entries); \
     the page may have changed since the cassette was stamped — re-stamp to refresh"
)]
pub struct CassetteMiss {
    /// HTTP method of the request that missed.
    pub method: String,
    /// URL of the request that missed.
    pub url: String,
    /// Number of records on the cassette at the time of the miss.
    /// Included in the message so the operator can immediately see
    /// "0 records → the plat was probably built without --record" vs
    /// "30 records → the plat is real but this specific request
    /// drifted".
    pub recorded_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cassette_is_empty() {
        let c = Cassette::new();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn record_appends_in_order() {
        let mut c = Cassette::new();
        c.record(
            "GET",
            "https://example.com/a",
            &[],
            200,
            vec![("content-type".into(), "text/html".into())],
            b"<html>a</html>",
        );
        c.record(
            "GET",
            "https://example.com/b",
            &[],
            404,
            vec![("content-type".into(), "text/plain".into())],
            b"not found",
        );
        assert_eq!(c.len(), 2);
        assert_eq!(c.records[0].url, "https://example.com/a");
        assert_eq!(c.records[1].status, 404);
    }

    #[test]
    fn method_is_uppercased_at_record() {
        let mut c = Cassette::new();
        c.record("get", "https://x/", &[], 200, vec![], b"");
        assert_eq!(c.records[0].method, "GET");
    }

    #[test]
    fn lookup_matches_recorded_request() {
        let mut c = Cassette::new();
        c.record(
            "GET",
            "https://example.com/",
            &[],
            200,
            vec![],
            b"<html></html>",
        );
        let r = c
            .lookup("GET", "https://example.com/", &[])
            .expect("recorded GET should hit");
        assert_eq!(r.status, 200);
    }

    #[test]
    fn lookup_method_is_case_insensitive() {
        let mut c = Cassette::new();
        c.record("POST", "https://x/", b"hi", 201, vec![], b"ok");
        assert!(c.lookup("post", "https://x/", b"hi").is_some());
        assert!(c.lookup("Post", "https://x/", b"hi").is_some());
        assert!(c.lookup("POST", "https://x/", b"hi").is_some());
    }

    #[test]
    fn lookup_url_is_byte_exact() {
        let mut c = Cassette::new();
        c.record("GET", "https://example.com/", &[], 200, vec![], b"");
        assert!(c.lookup("GET", "https://example.com/", &[]).is_some());
        // Trailing-slash difference is a different URL byte-wise; the
        // caller is expected to normalize before lookup.
        assert!(c.lookup("GET", "https://example.com", &[]).is_none());
    }

    #[test]
    fn lookup_body_is_byte_exact() {
        let mut c = Cassette::new();
        c.record("POST", "https://x/", b"alpha", 200, vec![], b"a");
        c.record("POST", "https://x/", b"beta", 200, vec![], b"b");
        let a = c.lookup("POST", "https://x/", b"alpha").expect("alpha hit");
        let b = c.lookup("POST", "https://x/", b"beta").expect("beta hit");
        assert_eq!(B64.decode(&a.response_body_b64).unwrap(), b"a");
        assert_eq!(B64.decode(&b.response_body_b64).unwrap(), b"b");
        assert!(c.lookup("POST", "https://x/", b"gamma").is_none());
    }

    #[test]
    fn lookup_returns_first_match_for_duplicate_requests() {
        // Same request recorded twice (e.g. a poll loop). Replay
        // walks records front-to-back; the first call hits index 0,
        // the second hit walks past it to index 1. Today `lookup`
        // returns the first match — sequential consumption is a
        // future refinement (track a cursor per (method,url,body) so
        // poll loops replay deterministically).
        let mut c = Cassette::new();
        c.record("GET", "https://x/", &[], 200, vec![], b"first");
        c.record("GET", "https://x/", &[], 200, vec![], b"second");
        let r = c.lookup("GET", "https://x/", &[]).expect("hit");
        assert_eq!(B64.decode(&r.response_body_b64).unwrap(), b"first");
    }

    #[test]
    fn decode_response_body_round_trips() {
        let mut c = Cassette::new();
        let payload = vec![0u8, 1, 2, 255, 254, 253]; // arbitrary binary
        c.record("GET", "https://x/", &[], 200, vec![], &payload);
        let r = &c.records[0];
        assert_eq!(Cassette::decode_response_body(r).unwrap(), payload);
    }

    #[test]
    fn cassette_round_trips_through_json() {
        let mut c = Cassette::new();
        c.record(
            "GET",
            "https://example.com/",
            &[],
            200,
            vec![
                ("content-type".into(), "text/html".into()),
                ("set-cookie".into(), "id=1".into()),
                ("set-cookie".into(), "id=2".into()),
            ],
            b"<html></html>",
        );
        let s = serde_json::to_string(&c).expect("serialize");
        let c2: Cassette = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(c, c2);
    }

    #[test]
    fn cassette_canonical_json_is_deterministic() {
        // Two cassettes constructed in the same order produce the
        // same canonical bytes. Plat-hash determinism relies on this.
        let mk = || {
            let mut c = Cassette::new();
            c.record("GET", "https://example.com/", &[], 200, vec![], b"hi");
            c
        };
        let a = serde_jcs::to_string(&mk()).expect("jcs a");
        let b = serde_jcs::to_string(&mk()).expect("jcs b");
        assert_eq!(a, b);
    }

    #[test]
    fn cassette_miss_display_includes_diagnostic_count() {
        let miss = CassetteMiss {
            method: "GET".into(),
            url: "https://drifted.example/".into(),
            recorded_count: 7,
        };
        let msg = miss.to_string();
        assert!(msg.contains("cassette miss"), "msg: {msg}");
        assert!(msg.contains("GET"), "msg: {msg}");
        assert!(msg.contains("https://drifted.example/"), "msg: {msg}");
        assert!(msg.contains("7 entries"), "msg: {msg}");
    }
}
