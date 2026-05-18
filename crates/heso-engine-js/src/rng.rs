//! # rng
//!
//! Seeded pseudo-random number generator backing the JS engine's
//! determinism shims, per [ADR 0008]. Wraps a single
//! [`rand_chacha::ChaCha20Rng`] behind an [`Arc`]`<`[`Mutex`]`>` so the
//! JS-side `Math.random`, `crypto.getRandomValues`, and
//! `crypto.randomUUID` closures can all draw from the same stream.
//!
//! ## Why ChaCha20
//!
//! Two properties matter for determinism:
//!
//! - **Portable.** The same seed must produce the same sequence on any
//!   host the agent runs on, today or three years from now. `ChaCha20Rng`
//!   is a fixed algorithm; `rand::rngs::StdRng` is explicitly *not*
//!   portable across `rand` versions.
//! - **Statistically reasonable.** Uniform output good enough that
//!   `Math.random()`-driven shuffles, retry jitter, and load-balancer
//!   hashes behave like a real RNG. ChaCha20 is a cryptographically
//!   secure stream cipher used as a PRNG here — overkill quality for
//!   our use case but free.
//!
//! ## Threading
//!
//! The JS engine is single-threaded; the [`Mutex`] is interior-mutability
//! for the multiple closures (Math.random, crypto.getRandomValues,
//! crypto.randomUUID) that share the same RNG, not for cross-thread
//! synchronization. Holding the lock across a draw is fine — the
//! critical section is microseconds.
//!
//! [ADR 0008]: ../../decisions/0008-deterministic-execution.md

use std::sync::{Arc, Mutex};

use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;

/// A seeded PRNG handed to the JS engine and shared across the three
/// determinism shims.
///
/// Internally an [`Arc`]`<`[`Mutex`]`<`[`ChaCha20Rng`]`>>`. Clone is
/// cheap (bumps the `Arc` refcount) — every JS-side closure that needs
/// the RNG gets its own clone.
#[derive(Debug, Clone)]
pub struct SeededRng {
    inner: Arc<Mutex<ChaCha20Rng>>,
}

impl SeededRng {
    /// Construct a fresh RNG seeded from `seed`. The same `seed` always
    /// produces the same sequence.
    ///
    /// `seed = 0` is the default for unseeded sessions — it's a real
    /// seed, not a sentinel, so two unseeded sessions are still
    /// reproducible against each other.
    pub fn new(seed: u64) -> Self {
        let chacha = ChaCha20Rng::seed_from_u64(seed);
        Self {
            inner: Arc::new(Mutex::new(chacha)),
        }
    }

    /// Draw a uniform `f64` in `[0.0, 1.0)` — the value
    /// [`Math.random()`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Math/random)
    /// returns.
    ///
    /// Returns `0.0` if the mutex is poisoned (which can only happen if
    /// a panic interrupted a prior draw — see [`Mutex`] docs). The
    /// engine is single-threaded so this is effectively unreachable;
    /// we degrade to `0.0` rather than panicking so a poisoned RNG
    /// never crashes the JS surface.
    pub fn next_f64(&self) -> f64 {
        match self.inner.lock() {
            // `Rng::gen()` for `f64` returns a uniform value in
            // `[0.0, 1.0)` — exactly the `Math.random()` contract.
            Ok(mut rng) => rng.gen::<f64>(),
            Err(_) => 0.0,
        }
    }

    /// Fill `out` with deterministic random bytes — the workhorse for
    /// `crypto.getRandomValues(view)`. After this returns, the slice
    /// contains `out.len()` bytes drawn from the seeded stream.
    ///
    /// No-ops on a poisoned mutex (same rationale as
    /// [`Self::next_f64`]).
    pub fn fill_bytes(&self, out: &mut [u8]) {
        if let Ok(mut rng) = self.inner.lock() {
            rng.fill_bytes(out);
        }
    }

    /// Generate a deterministic v4-format UUID string —
    /// `xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx` where `y` is one of
    /// `8`, `9`, `a`, `b` per [RFC 4122] §4.4.
    ///
    /// The output is lowercase hex with the standard 8-4-4-4-12
    /// dash layout. The version nibble (byte 6 high half) is forced to
    /// `0100` (= 4); the variant bits (byte 8 high half) are forced to
    /// `10xx` (RFC 4122).
    ///
    /// On a poisoned mutex returns the nil UUID
    /// (`00000000-0000-4000-8000-000000000000`, still v4-shaped) so the
    /// JS side never sees a malformed string.
    ///
    /// [RFC 4122]: https://www.rfc-editor.org/rfc/rfc4122#section-4.4
    pub fn random_uuid(&self) -> String {
        let mut bytes = [0u8; 16];
        self.fill_bytes(&mut bytes);
        // Force version 4 (top nibble of byte 6).
        bytes[6] = (bytes[6] & 0x0F) | 0x40;
        // Force variant 10xx (top two bits of byte 8).
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        format!(
            "{:02x}{:02x}{:02x}{:02x}-\
             {:02x}{:02x}-\
             {:02x}{:02x}-\
             {:02x}{:02x}-\
             {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
            bytes[8],
            bytes[9],
            bytes[10],
            bytes[11],
            bytes[12],
            bytes[13],
            bytes[14],
            bytes[15],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_produces_identical_f64_sequence() {
        // The core determinism guarantee: two fresh RNGs with the
        // same seed produce byte-identical streams.
        let a = SeededRng::new(42);
        let b = SeededRng::new(42);
        let seq_a: Vec<f64> = (0..5).map(|_| a.next_f64()).collect();
        let seq_b: Vec<f64> = (0..5).map(|_| b.next_f64()).collect();
        assert_eq!(
            seq_a, seq_b,
            "same seed must produce identical f64 sequence"
        );
        // And the values are in the Math.random contract range.
        for v in &seq_a {
            assert!(
                (0.0..1.0).contains(v),
                "next_f64 should yield [0,1): got {v}"
            );
        }
    }

    #[test]
    fn different_seed_produces_different_f64_sequence() {
        // ChaCha20Rng is a quality PRNG — two distinct seeds essentially
        // never collide on a 5-draw prefix.
        let a = SeededRng::new(1);
        let b = SeededRng::new(2);
        let seq_a: Vec<f64> = (0..5).map(|_| a.next_f64()).collect();
        let seq_b: Vec<f64> = (0..5).map(|_| b.next_f64()).collect();
        assert_ne!(
            seq_a, seq_b,
            "different seeds should produce different f64 sequences"
        );
    }

    #[test]
    fn random_uuid_is_valid_v4_format() {
        // Regex-style structural check: lowercase hex, dashes at the
        // canonical positions, version nibble = 4, variant nibble in
        // {8,9,a,b}.
        let rng = SeededRng::new(0);
        for _ in 0..32 {
            let s = rng.random_uuid();
            assert_eq!(
                s.len(),
                36,
                "UUID has 36 chars (32 hex + 4 dashes); got {s:?}"
            );
            let bytes = s.as_bytes();
            assert_eq!(bytes[8], b'-', "dash at idx 8 missing in {s:?}");
            assert_eq!(bytes[13], b'-', "dash at idx 13 missing in {s:?}");
            assert_eq!(bytes[18], b'-', "dash at idx 18 missing in {s:?}");
            assert_eq!(bytes[23], b'-', "dash at idx 23 missing in {s:?}");
            // Version nibble — the char at idx 14 must be '4'.
            assert_eq!(bytes[14], b'4', "version nibble must be 4 in {s:?}");
            // Variant nibble — char at idx 19 must be one of 8/9/a/b.
            let variant = bytes[19];
            assert!(
                matches!(variant, b'8' | b'9' | b'a' | b'b'),
                "variant nibble must be 8/9/a/b in {s:?} (got {})",
                variant as char
            );
            // Every non-dash char is lowercase hex.
            for (i, &c) in bytes.iter().enumerate() {
                if matches!(i, 8 | 13 | 18 | 23) {
                    continue;
                }
                assert!(
                    c.is_ascii_digit() || (b'a'..=b'f').contains(&c),
                    "non-lowercase-hex char at idx {i} in {s:?}: {}",
                    c as char
                );
            }
        }
    }

    #[test]
    fn fill_bytes_is_deterministic_per_seed() {
        // Same seed → identical bytes; different seeds → different bytes.
        let a = SeededRng::new(7);
        let b = SeededRng::new(7);
        let c = SeededRng::new(8);
        let mut buf_a = [0u8; 32];
        let mut buf_b = [0u8; 32];
        let mut buf_c = [0u8; 32];
        a.fill_bytes(&mut buf_a);
        b.fill_bytes(&mut buf_b);
        c.fill_bytes(&mut buf_c);
        assert_eq!(buf_a, buf_b, "same seed must produce identical bytes");
        assert_ne!(buf_a, buf_c, "different seeds must produce different bytes");
    }
}
