//! Trust-on-first-use pin store for plat signers.
//!
//! The SSH `known_hosts` model, keyed by plat `lineage` (the stripped,
//! content-covered pin key derived from the normalized input URL — see
//! [`crate::derive_lineage`]). The first time `heso verify` sees a signed
//! plat for a lineage it has never seen, it **pins** that signer's
//! fingerprint; every later verify of the same lineage must present the
//! same fingerprint or it **fails loud**. A genuine signer rotation is an
//! explicit `--accept-new-signer` re-pin, never a silent overwrite.
//!
//! ## What a pin proves (and what it does not)
//!
//! A pin converts a self-signed plat's bare INTEGRITY ("these bytes are
//! unchanged since signing") into AUTHENTICITY ("the *same* key that
//! signed the first plat for this lineage signed this one"). It does NOT
//! defeat a forger on first contact — that is TOFU's documented
//! blind spot, closed out-of-band by `--expect-signer` / `--signer-key`.
//!
//! ## On-disk shape
//!
//! ```json
//! {
//!   "signers": {
//!     "site:6b3f…e1": {
//!       "fingerprint": "heso:<32-hex>",
//!       "public_key": "<b64-std-32B>",
//!       "first_seen": "<rfc3339-utc>"
//!     }
//!   }
//! }
//! ```
//!
//! The object-with-a-`signers`-map convention mirrors the trusted-keys
//! allowlist file (`receipts.rs`): a top-level object leaves room to add
//! fields without breaking the shape.
//!
//! Writes are atomic (temp file + rename) so a concurrent verify never
//! reads a half-written store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A single pinned signer for one lineage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PinnedSigner {
    /// The signer fingerprint, rendered `heso:<32-hex>` — the value
    /// `heso verify` prints and compares.
    pub(crate) fingerprint: String,
    /// Base64-encoded (standard alphabet) 32-byte Ed25519 public key the
    /// fingerprint was computed from. Stored alongside the fingerprint so
    /// a future verify can re-derive or compare the raw key, not just the
    /// short hash.
    pub(crate) public_key: String,
    /// RFC 3339 UTC timestamp of the first-use pin. Informational only —
    /// never part of any trust decision.
    pub(crate) first_seen: String,
}

/// The TOFU pin store: a lineage → [`PinnedSigner`] map plus the path it
/// loaded from (so [`pin`](PinStore::pin) / [`repin`](PinStore::repin)
/// can persist back to the same file).
///
/// A `BTreeMap` keeps the on-disk JSON key order stable across writes,
/// so the file diffs cleanly when a human inspects it.
#[derive(Debug)]
pub(crate) struct PinStore {
    path: PathBuf,
    signers: BTreeMap<String, PinnedSigner>,
}

/// The serialized form. Split from [`PinStore`] so the in-memory type can
/// carry its own path without leaking it into the JSON.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PinStoreFile {
    #[serde(default)]
    signers: BTreeMap<String, PinnedSigner>,
}

/// Errors from the pin store. I/O and malformed-file failures are surfaced
/// rather than silently treated as an empty store — a corrupt
/// `known_signers.json` is a trust-state problem the caller must see, not
/// a reason to fall back to "trust anyone".
#[derive(Debug)]
pub(crate) enum PinError {
    /// Reading or writing the store file failed.
    Io(std::io::Error),
    /// The store file exists but is not the expected JSON shape.
    Malformed(String),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinError::Io(e) => write!(f, "pin store I/O failed: {e}"),
            PinError::Malformed(why) => write!(f, "pin store is malformed: {why}"),
        }
    }
}

impl std::error::Error for PinError {}

impl PinStore {
    /// Load the pin store at `path`. A missing file is **not** an error —
    /// it is the empty store every fresh machine starts with, so the
    /// first verify of any lineage pins. A present-but-corrupt file IS an
    /// error (the caller must not silently lose its trust anchors).
    pub(crate) fn load(path: &Path) -> Result<Self, PinError> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let file: PinStoreFile = serde_json::from_slice(&bytes)
                    .map_err(|e| PinError::Malformed(e.to_string()))?;
                Ok(Self {
                    path: path.to_path_buf(),
                    signers: file.signers,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path: path.to_path_buf(),
                signers: BTreeMap::new(),
            }),
            Err(e) => Err(PinError::Io(e)),
        }
    }

    /// Look up the pinned signer for `lineage`, if any.
    pub(crate) fn lookup(&self, lineage: &str) -> Option<&PinnedSigner> {
        self.signers.get(lineage)
    }

    /// Pin `lineage` to (`fingerprint`, `public_key`) for the first time
    /// and persist. Refuses to overwrite an existing pin — a rotation
    /// goes through [`repin`](Self::repin), gated on `--accept-new-signer`,
    /// so a forger can never quietly clobber a pin by re-running verify.
    ///
    /// Returns `Ok(false)` (a no-op) when the same fingerprint is already
    /// pinned — the common "second verify of the same plat" case — so the
    /// caller need not special-case it.
    pub(crate) fn pin(
        &mut self,
        lineage: &str,
        fingerprint: &str,
        public_key: &str,
    ) -> Result<bool, PinError> {
        if let Some(existing) = self.signers.get(lineage) {
            if existing.fingerprint == fingerprint {
                return Ok(false);
            }
            return Err(PinError::Malformed(format!(
                "refusing to silently overwrite the pin for lineage `{lineage}` \
                 (pinned {}, got {fingerprint}) — use repin",
                existing.fingerprint
            )));
        }
        // Re-read the file before writing: the in-memory map is a snapshot
        // from `load`, but a concurrent process may have pinned this lineage
        // in the meantime. Without this, two first-run verifies of the same
        // lineage would both see "no pin", and the second writer's full-map
        // overwrite would silently clobber the first (a lost update that
        // would then read back as a SIGNER MISMATCH). Merging the on-disk
        // state in turns the atomic rename into a true compare-and-set.
        let on_disk = self.read_disk()?;
        if let Some(disk_pin) = on_disk.get(lineage) {
            if disk_pin.fingerprint == fingerprint {
                // A concurrent process already pinned the same signer — adopt
                // it and report a no-op, identical to the in-memory case.
                self.signers = on_disk;
                return Ok(false);
            }
            return Err(PinError::Malformed(format!(
                "lineage `{lineage}` was pinned to {} by a concurrent verify \
                 (this one carries {fingerprint}) — refusing to clobber it",
                disk_pin.fingerprint
            )));
        }
        // No conflict on disk. Merge any lineages a concurrent process wrote
        // so this write does not drop them, then add ours.
        self.signers = on_disk;
        self.signers.insert(
            lineage.to_owned(),
            PinnedSigner {
                fingerprint: fingerprint.to_owned(),
                public_key: public_key.to_owned(),
                first_seen: now_rfc3339(),
            },
        );
        self.persist()?;
        Ok(true)
    }

    /// Read the current on-disk signer map, treating a missing file as the
    /// empty map. Used by [`pin`](Self::pin) to re-check just before a write
    /// so a concurrent first-use pin is detected rather than clobbered.
    fn read_disk(&self) -> Result<BTreeMap<String, PinnedSigner>, PinError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let file: PinStoreFile = serde_json::from_slice(&bytes)
                    .map_err(|e| PinError::Malformed(e.to_string()))?;
                Ok(file.signers)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(PinError::Io(e)),
        }
    }

    /// Re-pin `lineage` to a new signer — the `--accept-new-signer`
    /// escape hatch from a mismatch. Overwrites any existing pin and
    /// persists.
    pub(crate) fn repin(
        &mut self,
        lineage: &str,
        fingerprint: &str,
        public_key: &str,
    ) -> Result<(), PinError> {
        self.signers.insert(
            lineage.to_owned(),
            PinnedSigner {
                fingerprint: fingerprint.to_owned(),
                public_key: public_key.to_owned(),
                first_seen: now_rfc3339(),
            },
        );
        self.persist()
    }

    /// Write the store atomically: serialize to a sibling temp file, then
    /// rename over the target. A concurrent reader sees either the old
    /// file or the new one, never a partial write.
    fn persist(&self) -> Result<(), PinError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(PinError::Io)?;
            }
        }
        let file = PinStoreFile {
            signers: self.signers.clone(),
        };
        let body = serde_json::to_vec_pretty(&file)
            .map_err(|e| PinError::Malformed(e.to_string()))?;

        // A per-process temp name keeps two concurrent verifies from
        // racing on the same temp path before either renames into place.
        let pid = std::process::id();
        let tmp = self.path.with_extension(format!("json.tmp.{pid}"));
        std::fs::write(&tmp, &body).map_err(PinError::Io)?;
        match std::fs::rename(&tmp, &self.path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Don't leave the temp file behind on a failed rename.
                let _ = std::fs::remove_file(&tmp);
                Err(PinError::Io(e))
            }
        }
    }
}

/// Current wall-clock time as an RFC 3339 UTC string with second
/// precision (`YYYY-MM-DDTHH:MM:SSZ`). Used only for the informational
/// `first_seen` field — never for a trust decision — so a coarse,
/// dependency-free formatter is sufficient.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = unix_seconds_to_ymdhms(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Civil date from a Unix-second count (Howard Hinnant's "civil from
/// days", integer-only). Self-contained so the pin store carries no
/// `chrono` / `time` dependency.
fn unix_seconds_to_ymdhms(total_seconds: u64) -> (i64, u32, u32, u32, u32, u32) {
    let seconds_per_day: u64 = 86_400;
    let days_since_epoch = (total_seconds / seconds_per_day) as i64;
    let secs_of_day = (total_seconds % seconds_per_day) as u32;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day / 60) % 60;
    let second = secs_of_day % 60;

    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hour, minute, second)
}

// ============================================================================
// Tests (§8.3 trust-layer unit tests)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store_path(dir: &TempDir) -> PathBuf {
        dir.path().join("heso-local-data").join("known_signers.json")
    }

    #[test]
    fn missing_file_loads_as_empty_store() {
        let dir = TempDir::new().unwrap();
        let store = PinStore::load(&store_path(&dir)).expect("missing file is empty, not an error");
        assert!(store.lookup("site:abc").is_none());
    }

    #[test]
    fn first_use_writes_a_pin_then_lookup_finds_it() {
        let dir = TempDir::new().unwrap();
        let path = store_path(&dir);
        let mut store = PinStore::load(&path).unwrap();

        let wrote = store
            .pin("site:abc", "heso:1111", "PUBKEY_A==")
            .expect("first pin succeeds");
        assert!(wrote, "first pin must report it wrote");

        // Reload from disk: the pin survives the atomic write.
        let reloaded = PinStore::load(&path).unwrap();
        let pin = reloaded.lookup("site:abc").expect("pin persisted");
        assert_eq!(pin.fingerprint, "heso:1111");
        assert_eq!(pin.public_key, "PUBKEY_A==");
        assert!(pin.first_seen.ends_with('Z'), "first_seen is RFC3339 UTC");
    }

    #[test]
    fn second_verify_of_same_signer_is_a_quiet_noop() {
        let dir = TempDir::new().unwrap();
        let mut store = PinStore::load(&store_path(&dir)).unwrap();
        assert!(store.pin("site:abc", "heso:1111", "PUBKEY_A==").unwrap());
        // Same fingerprint again: no write, no error.
        let wrote = store
            .pin("site:abc", "heso:1111", "PUBKEY_A==")
            .expect("matching re-pin is fine");
        assert!(!wrote, "matching pin must be a no-op (no second write)");
    }

    #[test]
    fn mutated_signer_pin_refuses_silent_overwrite() {
        let dir = TempDir::new().unwrap();
        let mut store = PinStore::load(&store_path(&dir)).unwrap();
        store.pin("site:abc", "heso:1111", "PUBKEY_A==").unwrap();
        // A different fingerprint for the same lineage must NOT clobber.
        match store.pin("site:abc", "heso:2222", "PUBKEY_B==") {
            Err(PinError::Malformed(_)) => {}
            other => panic!("expected refusal, got {other:?}"),
        }
        // The original pin is untouched.
        assert_eq!(store.lookup("site:abc").unwrap().fingerprint, "heso:1111");
    }

    #[test]
    fn repin_replaces_the_pinned_signer() {
        let dir = TempDir::new().unwrap();
        let path = store_path(&dir);
        let mut store = PinStore::load(&path).unwrap();
        store.pin("site:abc", "heso:1111", "PUBKEY_A==").unwrap();

        store
            .repin("site:abc", "heso:2222", "PUBKEY_B==")
            .expect("repin succeeds");
        // Reload: the new signer is on disk.
        let reloaded = PinStore::load(&path).unwrap();
        let pin = reloaded.lookup("site:abc").unwrap();
        assert_eq!(pin.fingerprint, "heso:2222");
        assert_eq!(pin.public_key, "PUBKEY_B==");
    }

    #[test]
    fn distinct_lineages_pin_independently() {
        let dir = TempDir::new().unwrap();
        let mut store = PinStore::load(&store_path(&dir)).unwrap();
        store.pin("site:aaa", "heso:1111", "K1==").unwrap();
        store.pin("site:bbb", "heso:2222", "K2==").unwrap();
        assert_eq!(store.lookup("site:aaa").unwrap().fingerprint, "heso:1111");
        assert_eq!(store.lookup("site:bbb").unwrap().fingerprint, "heso:2222");
    }

    #[test]
    fn corrupt_store_file_is_an_error_not_an_empty_store() {
        let dir = TempDir::new().unwrap();
        let path = store_path(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ this is not json").unwrap();
        match PinStore::load(&path) {
            Err(PinError::Malformed(_)) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn concurrent_first_use_pin_does_not_clobber_a_racing_writer() {
        // Two processes both load an empty store, both decide to pin the
        // same lineage to different signers. The first persists; the second
        // must re-read before writing and refuse rather than overwrite, so
        // the winner's pin survives.
        let dir = TempDir::new().unwrap();
        let path = store_path(&dir);

        // Process A loads empty and pins.
        let mut a = PinStore::load(&path).unwrap();
        // Process B also loads empty (snapshot taken before A wrote).
        let mut b = PinStore::load(&path).unwrap();

        assert!(a.pin("site:abc", "heso:aaaa", "KA==").unwrap());

        // B's in-memory snapshot still shows no pin, but the file now has
        // A's. B must detect the conflict and refuse, not clobber.
        match b.pin("site:abc", "heso:bbbb", "KB==") {
            Err(PinError::Malformed(_)) => {}
            other => panic!("expected concurrent-pin refusal, got {other:?}"),
        }

        // The winner's pin is what survives on disk.
        let reloaded = PinStore::load(&path).unwrap();
        assert_eq!(reloaded.lookup("site:abc").unwrap().fingerprint, "heso:aaaa");
    }

    #[test]
    fn concurrent_first_use_pin_same_signer_is_a_quiet_noop() {
        // Same race, but both processes carry the *same* signer (the common
        // benign case): the second writer adopts the on-disk pin and reports
        // a no-op instead of erroring.
        let dir = TempDir::new().unwrap();
        let path = store_path(&dir);
        let mut a = PinStore::load(&path).unwrap();
        let mut b = PinStore::load(&path).unwrap();

        assert!(a.pin("site:abc", "heso:aaaa", "KA==").unwrap());
        let wrote = b
            .pin("site:abc", "heso:aaaa", "KA==")
            .expect("matching concurrent pin is fine");
        assert!(!wrote, "a racing pin of the same signer is a no-op");
    }

    #[test]
    fn concurrent_pins_to_distinct_lineages_all_survive() {
        // Atomic temp+rename: many writers pinning different lineages to
        // the same file must not corrupt it. Sequential here (the store is
        // not Sync) but each `pin` performs the full read-absent /
        // write-temp / rename cycle a separate process would.
        let dir = TempDir::new().unwrap();
        let path = store_path(&dir);
        for i in 0..50 {
            let mut store = PinStore::load(&path).unwrap();
            store
                .pin(&format!("site:{i:04}"), &format!("heso:{i:032x}"), "K==")
                .unwrap();
        }
        let reloaded = PinStore::load(&path).unwrap();
        for i in 0..50 {
            assert!(
                reloaded.lookup(&format!("site:{i:04}")).is_some(),
                "lineage {i} survived the concurrent-write simulation"
            );
        }
    }
}
