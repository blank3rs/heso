//! On-disk TTL result cache for the search verb (A.6).
//!
//! A search CLI is one-shot per process, so an in-process cache buys
//! nothing — but an agent re-running the same query during a session
//! shouldn't re-hit (and re-throttle) a backend. This is a tiny on-disk
//! cache keyed by `(backend, query, page)`: a fresh entry short-circuits
//! the HTTP call entirely.
//!
//! The cache is always on — there is no flag. It is best-effort by
//! design: a miss, a corrupt file, an unreadable directory, or any I/O
//! error simply falls through to a live fetch. The cache never fails a
//! search.
//!
//! - Location: `heso-local-data/search-cache/` (the gitignored data dir
//!   family the identity key already lives under), one JSON file per key.
//! - Key: `blake3(backend | query | page).json` — the backend wire name,
//!   the raw query, and the 0-based page index, NUL-separated so distinct
//!   inputs can't collide on a shared boundary.
//! - Record: `{ stamped_at_unix, rows }`. `rows` carries the parsed
//!   `title` / `url` / `snippet` triples; the backend (hence each row's
//!   `source`) is implied by the cache key, so it is reconstructed on read
//!   rather than stored redundantly.
//! - TTL: 5 minutes by default; `HESO_SEARCH_CACHE_TTL` (seconds) tunes it.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{BackendId, RawResult};

/// Default freshness window for a cached page. Short enough that a result
/// set never goes badly stale within a session, long enough that an agent
/// iterating on one query doesn't re-hit a backend each time.
const DEFAULT_TTL_SECS: u64 = 5 * 60;

/// Environment variable that overrides [`DEFAULT_TTL_SECS`] (in seconds).
/// A `0` (or unparseable) value disables the cache for this process — the
/// read always misses and nothing is written.
const TTL_ENV: &str = "HESO_SEARCH_CACHE_TTL";

/// The on-disk record. `rows` stores only the string fields; `source` is
/// reconstructed from the cache key's backend on read.
#[derive(Serialize, Deserialize)]
struct CacheRecord {
    stamped_at_unix: u64,
    rows: Vec<CachedRow>,
}

#[derive(Serialize, Deserialize)]
struct CachedRow {
    title: String,
    url: String,
    snippet: String,
}

/// The TTL in effect for this process, honouring [`TTL_ENV`]. A `0` TTL
/// disables the cache.
fn ttl_secs() -> u64 {
    match std::env::var(TTL_ENV) {
        // An explicit but unparseable value is treated as `0` (cache off):
        // honouring the documented contract is safer than silently caching
        // under a value the operator clearly did not intend.
        Ok(v) => v.trim().parse::<u64>().unwrap_or(0),
        Err(_) => DEFAULT_TTL_SECS,
    }
}

/// Seconds since the Unix epoch, or `0` if the clock is before the epoch
/// (which would make every entry read as stale — the safe direction).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The cache directory, under the gitignored `heso-local-data/` family.
fn cache_dir() -> PathBuf {
    PathBuf::from("heso-local-data").join("search-cache")
}

/// The on-disk path for one `(backend, query, page)` key.
fn cache_path(backend: BackendId, query: &str, page: usize) -> PathBuf {
    let mut hasher = blake3::Hasher::new();
    hasher.update(backend.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(query.as_bytes());
    hasher.update(b"\0");
    hasher.update(page.to_string().as_bytes());
    let key = hasher.finalize().to_hex().to_string();
    cache_dir().join(format!("{key}.json"))
}

/// Look up a cached page. Returns `Some(rows)` only when a record exists
/// AND is still within the TTL; a miss, a stale or corrupt entry, or any
/// I/O error returns `None` so the caller falls through to a live fetch.
pub(super) fn get(backend: BackendId, query: &str, page: usize) -> Option<Vec<RawResult>> {
    let ttl = ttl_secs();
    if ttl == 0 {
        return None;
    }
    let path = cache_path(backend, query, page);
    let raw = std::fs::read_to_string(&path).ok()?;
    let record: CacheRecord = serde_json::from_str(&raw).ok()?;
    let age = now_unix().saturating_sub(record.stamped_at_unix);
    if age > ttl {
        return None;
    }
    Some(
        record
            .rows
            .into_iter()
            .map(|r| RawResult {
                title: r.title,
                url: r.url,
                snippet: r.snippet,
                source: backend,
            })
            .collect(),
    )
}

/// Store a freshly-fetched page. Best-effort: a failure to create the
/// directory or write the file is swallowed — a search must never fail
/// because the cache is unwritable.
pub(super) fn put(backend: BackendId, query: &str, page: usize, rows: &[RawResult]) {
    if ttl_secs() == 0 {
        return;
    }
    let path = cache_path(backend, query, page);
    let record = CacheRecord {
        stamped_at_unix: now_unix(),
        rows: rows
            .iter()
            .map(|r| CachedRow {
                title: r.title.clone(),
                url: r.url.clone(),
                snippet: r.snippet.clone(),
            })
            .collect(),
    };
    let Ok(serialized) = serde_json::to_vec(&record) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _ = std::fs::write(&path, serialized);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes every test that touches process-global state (the CWD and
    /// `HESO_SEARCH_CACHE_TTL`) so they don't race each other.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point the cache dir's parent at a tempdir for one test by running
    /// from a unique CWD. Tests share the process, so each uses a distinct
    /// query string to avoid colliding cache keys.
    fn unique_query(tag: &str) -> String {
        format!("cache-test-{tag}-{}", now_unix_nanos())
    }

    fn now_unix_nanos() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    #[test]
    fn round_trip_in_tempdir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = CwdGuard::enter(tmp.path());
        let q = unique_query("rt");
        let rows = vec![RawResult {
            title: "t".into(),
            url: "https://example.com/a".into(),
            snippet: "s".into(),
            source: BackendId::Mojeek,
        }];
        assert!(get(BackendId::Mojeek, &q, 0).is_none(), "cold miss");
        put(BackendId::Mojeek, &q, 0, &rows);
        let hit = get(BackendId::Mojeek, &q, 0).expect("warm hit");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].url, "https://example.com/a");
        assert_eq!(hit[0].source, BackendId::Mojeek);
    }

    #[test]
    fn distinct_pages_and_backends_do_not_collide() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = CwdGuard::enter(tmp.path());
        let q = unique_query("collide");
        let p0 = vec![RawResult {
            title: "p0".into(),
            url: "https://example.com/0".into(),
            snippet: String::new(),
            source: BackendId::Mojeek,
        }];
        put(BackendId::Mojeek, &q, 0, &p0);
        // Same query, different page → independent key (a miss).
        assert!(get(BackendId::Mojeek, &q, 1).is_none());
        // Same query+page, different backend → independent key (a miss).
        assert!(get(BackendId::Brave, &q, 0).is_none());
        // The original key still hits.
        assert_eq!(get(BackendId::Mojeek, &q, 0).expect("hit").len(), 1);
    }

    #[test]
    fn stale_entry_is_a_miss() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = CwdGuard::enter(tmp.path());
        let q = unique_query("stale");
        let path = cache_path(BackendId::DdgHtml, &q, 0);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Hand-write a record stamped well outside any sane TTL.
        let stale = CacheRecord {
            stamped_at_unix: 1,
            rows: Vec::new(),
        };
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(get(BackendId::DdgHtml, &q, 0).is_none(), "stale must miss");
    }

    #[test]
    fn corrupt_file_is_a_miss_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = CwdGuard::enter(tmp.path());
        let q = unique_query("corrupt");
        let path = cache_path(BackendId::DdgLite, &q, 0);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json at all").unwrap();
        assert!(get(BackendId::DdgLite, &q, 0).is_none());
    }

    #[test]
    fn unparseable_ttl_disables_cache() {
        // An explicit-but-garbage `HESO_SEARCH_CACHE_TTL` disables the
        // cache, matching the documented `0`-or-unparseable contract: a put
        // writes nothing and a subsequent get misses.
        let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(tmp.path()).expect("set cwd");
        let prev_ttl = std::env::var(TTL_ENV).ok();
        std::env::set_var(TTL_ENV, "soon");

        assert_eq!(ttl_secs(), 0, "unparseable TTL must disable the cache");
        let q = unique_query("unparseable");
        let rows = vec![RawResult {
            title: "t".into(),
            url: "https://example.com/a".into(),
            snippet: "s".into(),
            source: BackendId::Mojeek,
        }];
        put(BackendId::Mojeek, &q, 0, &rows);
        assert!(
            get(BackendId::Mojeek, &q, 0).is_none(),
            "disabled cache must miss after a put"
        );

        match prev_ttl {
            Some(v) => std::env::set_var(TTL_ENV, v),
            None => std::env::remove_var(TTL_ENV),
        }
        let _ = std::env::set_current_dir(&prev);
    }

    /// Restore the process CWD on drop so a `set_current_dir` in one test
    /// doesn't leak into the next. Tests touching the cache run serially
    /// via this guard's coarse lock.
    struct CwdGuard {
        prev: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl CwdGuard {
        fn enter(dir: &std::path::Path) -> Self {
            let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::current_dir().expect("cwd");
            std::env::set_current_dir(dir).expect("set cwd");
            CwdGuard { prev, _lock: lock }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.prev);
        }
    }
}
