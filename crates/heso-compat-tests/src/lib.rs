//! # heso-compat-tests
//!
//! Compatibility test harness for [`heso_engine_fetch`]. A pinned inventory
//! of cooperative real-world URLs, each paired with a recorded HTML cassette
//! and an inline invariant check, so that as the engine grows (script-on-load,
//! `fetch()` in JS, determinism shims, planner v0, …) we have a regression
//! net that catches the moment a once-working page stops working.
//!
//! ## Two modes
//!
//! - **REPLAY (default)** — cassettes on disk are served via a local
//!   [`wiremock::MockServer`]. Tests open `mock_server.uri()` through the
//!   real [`heso_engine_fetch::FetchEngine`] and assert against the parsed
//!   page. **Zero network traffic.** CI runs in this mode.
//! - **RECORD (`HESO_COMPAT_RECORD=1`)** — cassettes are refreshed by
//!   fetching the live URL with the same `FetchEngine`, then written to
//!   disk. Humans run this periodically when a real page legitimately
//!   changes. Never run in CI.
//!
//! Cassettes live in `crates/heso-compat-tests/cassettes/<slug>/`:
//! - `body.html` — verbatim response body (easy to diff in PRs)
//! - `meta.json` — `{ url, fetched_at, status, content_type }`
//!
//! ## Why wiremock-rs and not VCR-style middleware
//!
//! We surveyed the Rust record/replay landscape (see web-research notes
//! attached to the implementing PR):
//!
//! - **rvcr** ([ChorusOne/rvcr](https://github.com/ChorusOne/rvcr),
//!   Apache-2.0) — the closest existing solution. Hooks into
//!   `reqwest_middleware::ClientWithMiddleware`. Rejected: heso's
//!   [`heso_engine_fetch::FetchEngine`] builds a plain `reqwest::Client`
//!   with no middleware seam. Adopting rvcr would force a refactor of the
//!   fetch crate, which is out of scope for an "F-item" regression harness.
//! - **surf-vcr**, **http-client-vcr** — bound to clients we don't use.
//! - **magneto-serge** — cross-language JSON cassettes; overkill for our
//!   single-language shape.
//!
//! Instead we use [`wiremock::MockServer`] (MIT, actively maintained) as
//! a localhost replay surface: tests construct a `FetchEngine`, point it
//! at `mock_server.uri()`, and the engine sees a real `reqwest` response
//! — same code path, no network. Inspiration acknowledged: VCR (Ruby),
//! surf-vcr, rvcr.
//!
//! ## Adding a URL
//!
//! 1. Add a `#[tokio::test]` to `tests/invariants.rs` that calls
//!    [`load_or_record_cassette`] with a unique slug and the live URL.
//! 2. Run once with `HESO_COMPAT_RECORD=1 cargo test -p heso-compat-tests`
//!    to populate the cassette directory.
//! 3. Eyeball the recorded `body.html` and `meta.json`.
//! 4. Run `cargo test -p heso-compat-tests` (REPLAY) to confirm the
//!    invariants pass against the stored cassette.
//! 5. Commit `body.html` + `meta.json`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use wiremock::matchers::{method, path as path_matcher, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ============================================================================
// Errors
// ============================================================================

/// Errors produced by the compatibility harness. The crate is test-only so
/// most callers will just `.unwrap()` — this enum exists to keep error sites
/// legible when a recording or load fails.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem I/O failed while reading or writing a cassette.
    #[error("cassette I/O at {path}: {source}")]
    Io {
        /// Path that was being read or written.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// `meta.json` failed to parse.
    #[error("cassette meta parse at {path}: {source}")]
    Meta {
        /// Path of the bad `meta.json`.
        path: PathBuf,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// Live recording failed (RECORD mode only).
    #[error("recording {url}: {source}")]
    Record {
        /// URL we were trying to fetch.
        url: String,
        /// Underlying HTTP error.
        #[source]
        source: reqwest::Error,
    },

    /// `HESO_COMPAT_RECORD` was unset *and* no cassette exists on disk. The
    /// fix is to run the test once with `HESO_COMPAT_RECORD=1` to populate
    /// the cassette.
    #[error(
        "no cassette at {path} and HESO_COMPAT_RECORD is not set — \
         run `HESO_COMPAT_RECORD=1 cargo test -p heso-compat-tests` once \
         to populate, then commit cassettes/{slug}/"
    )]
    Missing {
        /// Cassette directory we looked at.
        path: PathBuf,
        /// Slug the test passed in.
        slug: String,
    },

    /// A URL string could not be parsed.
    #[error("url parse: {0}")]
    Url(#[from] url::ParseError),
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;

// ============================================================================
// Cassette
// ============================================================================

/// One recorded HTTP response — body + the slim metadata needed to replay it
/// faithfully through [`wiremock`].
///
/// Body and metadata are persisted as two separate files (`body.html` +
/// `meta.json`) under a slug-named directory so PR diffs stay readable.
/// One big JSON-with-embedded-string format was rejected for diff-quality
/// reasons.
#[derive(Debug, Clone)]
pub struct Cassette {
    /// The original live URL this cassette was recorded from. Stored in
    /// `meta.json` for documentation; not used by replay (replay points
    /// the engine at `mock_server.uri()`).
    pub original_url: String,
    /// Verbatim response body bytes (kept as `String` because every
    /// pinned URL serves text/html or text/plain).
    pub body: String,
    /// HTTP status code the live server returned at record time.
    pub status: u16,
    /// `Content-Type` header at record time. Default
    /// `"text/html; charset=utf-8"` if the server omitted it.
    pub content_type: String,
    /// Unix-epoch seconds when this cassette was recorded.
    pub fetched_at: u64,
}

/// On-disk shape of `meta.json`. Public-via-serde only; the in-memory type
/// is [`Cassette`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CassetteMeta {
    url: String,
    fetched_at: u64,
    status: u16,
    content_type: String,
}

impl Cassette {
    /// Path on disk this cassette lives at, given a slug. Resolved relative
    /// to this crate's `CARGO_MANIFEST_DIR` so the harness works from any
    /// `cargo test` invocation.
    pub fn dir(slug: &str) -> PathBuf {
        cassettes_root().join(slug)
    }

    /// Read a cassette from disk. Returns [`Error::Missing`] if the
    /// directory doesn't exist — callers use that as the cue to fall back
    /// to RECORD mode (when enabled) or fail with a helpful hint.
    pub fn load(slug: &str) -> Result<Self> {
        let dir = Self::dir(slug);
        if !dir.exists() {
            return Err(Error::Missing {
                path: dir,
                slug: slug.to_string(),
            });
        }
        let meta_path = dir.join("meta.json");
        let body_path = dir.join("body.html");
        let meta_raw = fs::read_to_string(&meta_path).map_err(|e| Error::Io {
            path: meta_path.clone(),
            source: e,
        })?;
        let meta: CassetteMeta = serde_json::from_str(&meta_raw).map_err(|e| Error::Meta {
            path: meta_path,
            source: e,
        })?;
        let body = fs::read_to_string(&body_path).map_err(|e| Error::Io {
            path: body_path,
            source: e,
        })?;
        Ok(Cassette {
            original_url: meta.url,
            body,
            status: meta.status,
            content_type: meta.content_type,
            fetched_at: meta.fetched_at,
        })
    }

    /// Write this cassette to disk under `cassettes/<slug>/`. Creates
    /// the directory if absent.
    pub fn save(&self, slug: &str) -> Result<()> {
        let dir = Self::dir(slug);
        fs::create_dir_all(&dir).map_err(|e| Error::Io {
            path: dir.clone(),
            source: e,
        })?;
        let body_path = dir.join("body.html");
        fs::write(&body_path, &self.body).map_err(|e| Error::Io {
            path: body_path,
            source: e,
        })?;
        let meta = CassetteMeta {
            url: self.original_url.clone(),
            fetched_at: self.fetched_at,
            status: self.status,
            content_type: self.content_type.clone(),
        };
        let meta_json = serde_json::to_string_pretty(&meta).map_err(|e| Error::Meta {
            path: dir.join("meta.json"),
            source: e,
        })?;
        let meta_path = dir.join("meta.json");
        fs::write(&meta_path, format!("{meta_json}\n")).map_err(|e| Error::Io {
            path: meta_path,
            source: e,
        })?;
        Ok(())
    }
}

// ============================================================================
// Modes
// ============================================================================

/// True when the harness is in **RECORD** mode — i.e. the
/// `HESO_COMPAT_RECORD` environment variable is set (to any value).
///
/// Default is REPLAY: CI must not record. The variable is checked once per
/// call so individual tests can flip modes in the same process via
/// `std::env::set_var`, but you almost never want to.
pub fn is_record_mode() -> bool {
    std::env::var("HESO_COMPAT_RECORD").is_ok()
}

// ============================================================================
// Cassette resolution — the main entry point per test
// ============================================================================

/// Resolve a cassette for `slug`, recording from `original_url` first if
/// [`is_record_mode`] is set and no cassette exists yet (or unconditionally
/// in RECORD, to refresh).
///
/// REPLAY behavior — the common case:
/// - If `cassettes/<slug>/` exists, load and return it.
/// - Else return [`Error::Missing`] with a helpful hint telling the human
///   to run with `HESO_COMPAT_RECORD=1` once.
///
/// RECORD behavior:
/// - Fetch `original_url` via a fresh [`reqwest::Client`] (with the same
///   defaults [`heso_engine_fetch::FetchEngine`] uses: user-agent
///   `heso-compat-tests/<version>`, redirects allowed). Note: we don't
///   reuse `FetchEngine` here because it returns a parsed `FetchPage`; we
///   need the raw HTML body and status.
/// - Persist the resulting [`Cassette`] to `cassettes/<slug>/`.
/// - Return it.
pub async fn load_or_record_cassette(slug: &str, original_url: &str) -> Result<Cassette> {
    if is_record_mode() {
        let cassette = record_live(original_url).await?;
        cassette.save(slug)?;
        return Ok(cassette);
    }
    Cassette::load(slug)
}

/// Hit `original_url` over the real network and return a fresh [`Cassette`].
/// Used by [`load_or_record_cassette`] in RECORD mode; exposed so a future
/// `cargo xtask refresh-cassettes` could call it directly.
pub async fn record_live(original_url: &str) -> Result<Cassette> {
    let client = Client::builder()
        .user_agent(concat!("heso-compat-tests/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::limited(20))
        .build()
        .map_err(|e| Error::Record {
            url: original_url.to_string(),
            source: e,
        })?;
    let response = client
        .get(original_url)
        .send()
        .await
        .map_err(|e| Error::Record {
            url: original_url.to_string(),
            source: e,
        })?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/html; charset=utf-8")
        .to_string();
    let final_url = response.url().as_str().to_string();
    let body = response.text().await.map_err(|e| Error::Record {
        url: original_url.to_string(),
        source: e,
    })?;
    Ok(Cassette {
        original_url: final_url,
        body,
        status,
        content_type,
        fetched_at: now_unix(),
    })
}

// ============================================================================
// MockServer wiring — serve a cassette as if it were the real origin
// ============================================================================

/// Boot a [`wiremock::MockServer`] that serves `cassette.body` at every GET
/// (path `/` plus any sub-path), preserving the recorded status code and
/// `Content-Type`. Returns the live server so the caller can read
/// `server.uri()` and feed it to [`heso_engine_fetch::FetchEngine::open`].
///
/// The mock matches any GET because some recorded pages link to
/// same-origin assets the engine *might* fetch (favicons, follow-up
/// links). We respond identically; the goal isn't a perfect site replica,
/// it's deterministic invariants on the *primary* document. If a future
/// test wants per-path fidelity it can call [`Mock::given`] directly.
pub async fn serve_cassette(cassette: &Cassette) -> MockServer {
    let server = MockServer::start().await;
    let body = cassette.body.clone();
    let status = cassette.status;
    let content_type = cassette.content_type.clone();
    let response = ResponseTemplate::new(status)
        .insert_header("content-type", content_type.as_str())
        .set_body_string(body);
    // Two mounts: exact "/" and any other path. Cleaner than relying on a
    // regex that could accidentally match nothing.
    Mock::given(method("GET"))
        .and(path_matcher("/"))
        .respond_with(response.clone())
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/.+"))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

// ============================================================================
// Path helpers
// ============================================================================

/// Filesystem path to `cassettes/` under this crate. Resolved at compile
/// time from `CARGO_MANIFEST_DIR` so tests work no matter where `cargo
/// test` is invoked from.
fn cassettes_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("cassettes")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ============================================================================
// Unit tests (no network, no filesystem-of-truth)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cassette_dir_is_under_crate() {
        let dir = Cassette::dir("example_com");
        assert!(dir.ends_with("cassettes/example_com") || dir.ends_with("cassettes\\example_com"));
    }

    #[test]
    fn missing_cassette_gives_helpful_error() {
        let res = Cassette::load("definitely-does-not-exist-12345");
        match res {
            Err(Error::Missing { slug, .. }) => {
                assert_eq!(slug, "definitely-does-not-exist-12345");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let slug = "_unit_roundtrip";
        let cassette = Cassette {
            original_url: "https://example.test/".to_string(),
            body: "<html><body>hi</body></html>".to_string(),
            status: 200,
            content_type: "text/html; charset=utf-8".to_string(),
            fetched_at: 1700000000,
        };
        cassette.save(slug).expect("save works");
        let loaded = Cassette::load(slug).expect("load works");
        assert_eq!(loaded.original_url, cassette.original_url);
        assert_eq!(loaded.body, cassette.body);
        assert_eq!(loaded.status, cassette.status);
        assert_eq!(loaded.content_type, cassette.content_type);
        assert_eq!(loaded.fetched_at, cassette.fetched_at);
        // Cleanup so this test stays hermetic.
        let _ = fs::remove_dir_all(Cassette::dir(slug));
    }

    #[tokio::test]
    async fn serve_cassette_responds_with_body() {
        let cassette = Cassette {
            original_url: "https://example.test/".to_string(),
            body: "<!doctype html><title>Test</title>".to_string(),
            status: 200,
            content_type: "text/html; charset=utf-8".to_string(),
            fetched_at: 0,
        };
        let server = serve_cassette(&cassette).await;
        let body = reqwest::get(server.uri())
            .await
            .expect("get works")
            .text()
            .await
            .expect("text works");
        assert_eq!(body, cassette.body);
    }

    #[test]
    fn record_mode_defaults_false_in_tests() {
        // Defensive: if anyone ever sets HESO_COMPAT_RECORD globally in
        // CI by accident, this test will scream.
        if std::env::var("HESO_COMPAT_RECORD").is_ok() {
            // Don't fail — just emit a note. RECORD-mode CI is a foot-gun
            // we want to catch in code review.
            eprintln!("HESO_COMPAT_RECORD is set; harness will write cassettes.");
        }
    }
}
