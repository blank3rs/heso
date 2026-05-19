//! Ed25519 identity for signed receipts.
//!
//! Per [ADR 0005] every heso instance has a local Ed25519 keypair. The
//! private key is 32 raw bytes on disk at a caller-chosen path (typically
//! `heso-local-data/identity.key`). The public key is derived from the
//! private key on every load, so the on-disk file is small and there's no
//! risk of public/private mismatch.
//!
//! ## On-disk format
//!
//! The minimal possible: **32 bytes of the Ed25519 seed.** No header, no
//! metadata, no PEM. This is the same shape `ed25519_dalek::SigningKey`
//! accepts via `SigningKey::from_bytes(&[u8; 32])`. Two reasons for the
//! plain-bytes choice:
//!
//! 1. It's the simplest thing that can possibly work; tools like `xxd`
//!    can read it; no parser bugs.
//! 2. The directory (`heso-local-data/`) is already gitignored. There's
//!    no PEM-vs-binary debate to have when the bytes never leave the
//!    machine.
//!
//! **Permissions on Windows are not tightened to 0600** today — Windows
//! ACLs are not a one-line `chmod`. The directory is gitignored and the
//! file is only readable by the user account anyway under default NTFS
//! permissions. A follow-up can wire `cacls`-equivalents per platform.
//!
//! ## Signing
//!
//! [`IdentityKey::sign`] is a thin wrapper over `ed25519_dalek`'s
//! `Signer::sign`. [`IdentityKey::verify`] / [`Signature::verify`] use
//! the `verify_strict` variant which adds the "weak public key" check on
//! top of basic Ed25519 verification — a small extra cost we always pay
//! because the receipt format makes weak keys an attacker-controlled
//! input.
//!
//! [ADR 0005]: ../../../decisions/0005-ed25519-identity.md

use std::fs;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ed25519_dalek::{
    Signer as _, SigningKey, VerifyingKey, PUBLIC_KEY_LENGTH, SECRET_KEY_LENGTH, SIGNATURE_LENGTH,
};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};

/// The algorithm name embedded in the on-the-wire [`Signature`] envelope.
/// Currently the only supported choice.
pub const SIG_ALGORITHM: &str = "Ed25519";

/// A heso-local identity — wraps an Ed25519 keypair.
///
/// Construct via [`IdentityKey::generate`] (fresh random key) or
/// [`IdentityKey::load`] (read from disk). Use [`IdentityKey::sign`] to
/// sign a canonical-JSON payload; the matching [`Signature::verify`] is
/// publicly callable on the receipt-verify path with no key material.
pub struct IdentityKey {
    signing: SigningKey,
    /// Precomputed base64 of the verifying key. `IdentityKey` is
    /// long-lived (one per process for any sign-heavy workload) but
    /// `sign()` used to re-encode the public key every call. Caching
    /// at construction time keeps `sign()` allocation-free for the
    /// public-key portion of the envelope.
    public_key_b64: String,
}

impl std::fmt::Debug for IdentityKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never log private key material — only the public half.
        f.debug_struct("IdentityKey")
            .field("public_key_b64", &self.public_key_b64)
            .finish()
    }
}

impl IdentityKey {
    /// Generate a fresh random keypair from the OS entropy source.
    pub fn generate() -> Self {
        let mut rng = OsRng;
        Self::from_signing(SigningKey::generate(&mut rng))
    }

    /// Construct from 32 raw seed bytes (the secret-key half).
    pub fn from_bytes(seed: &[u8; SECRET_KEY_LENGTH]) -> Self {
        Self::from_signing(SigningKey::from_bytes(seed))
    }

    fn from_signing(signing: SigningKey) -> Self {
        let public_key_b64 = B64.encode(signing.verifying_key().to_bytes());
        Self {
            signing,
            public_key_b64,
        }
    }

    /// Load an identity from disk. The file must be exactly 32 bytes —
    /// the raw Ed25519 seed.
    pub fn load(path: &Path) -> Result<Self, IdentityError> {
        let bytes = fs::read(path).map_err(|e| IdentityError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if bytes.len() != SECRET_KEY_LENGTH {
            return Err(IdentityError::BadKeyLength {
                path: path.to_path_buf(),
                expected: SECRET_KEY_LENGTH,
                actual: bytes.len(),
            });
        }
        let mut seed = [0u8; SECRET_KEY_LENGTH];
        seed.copy_from_slice(&bytes);
        Ok(Self::from_bytes(&seed))
    }

    /// Write the 32-byte seed to `path`. Creates parent directories as
    /// needed. Refuses to overwrite an existing file — callers should
    /// delete first if rotation is intended.
    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        if path.exists() {
            return Err(IdentityError::AlreadyExists(path.to_path_buf()));
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| IdentityError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
        }
        fs::write(path, self.signing.to_bytes()).map_err(|e| IdentityError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        // On Unix, tighten permissions to 0600. On Windows, default NTFS
        // ACLs already restrict access to the user account; a follow-up
        // can run `icacls` to be even stricter.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(path, perms).map_err(|e| IdentityError::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
        }
        Ok(())
    }

    /// Raw 32-byte public key.
    pub fn public_key_bytes(&self) -> [u8; PUBLIC_KEY_LENGTH] {
        self.signing.verifying_key().to_bytes()
    }

    /// Base64-encoded (standard alphabet) public key. The shape that
    /// goes into a [`Signature`] envelope. Returns a clone of the
    /// precomputed value cached at construction time.
    pub fn public_key_b64(&self) -> String {
        self.public_key_b64.clone()
    }

    /// Sign `payload` and produce an on-the-wire [`Signature`] envelope.
    pub fn sign(&self, payload: &[u8]) -> Signature {
        let sig = self.signing.sign(payload);
        Signature {
            algorithm: SIG_ALGORITHM.to_owned(),
            public_key: self.public_key_b64.clone(),
            signature: B64.encode(sig.to_bytes()),
        }
    }

    /// Quick self-verify (used in tests).
    pub fn verify(&self, payload: &[u8], sig: &Signature) -> Result<(), IdentityError> {
        sig.verify(payload)
    }
}

/// The on-the-wire signature envelope embedded in a `Receipt`.
///
/// All fields are base64-encoded (standard alphabet) to keep the receipt
/// JSON-safe. The signed payload is the canonical-JSON of the receipt
/// with its `signature` field set to `null` — see `heso_trace`'s
/// `sign_receipt` for the canonicalization rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Always `"Ed25519"` for now. A bumpable string instead of an enum
    /// so future verifiers reading old receipts get a clearer error.
    pub algorithm: String,
    /// Base64-encoded 32-byte Ed25519 public key.
    pub public_key: String,
    /// Base64-encoded 64-byte Ed25519 signature.
    pub signature: String,
}

impl Signature {
    /// Verify this signature against `payload`. Returns `Ok(())` on
    /// success. Uses `VerifyingKey::verify_strict` which adds the
    /// "weak public key" check on top of standard Ed25519 verification.
    pub fn verify(&self, payload: &[u8]) -> Result<(), IdentityError> {
        if self.algorithm != SIG_ALGORITHM {
            return Err(IdentityError::UnknownAlgorithm(self.algorithm.clone()));
        }
        let pk_bytes = B64
            .decode(self.public_key.as_bytes())
            .map_err(|_| IdentityError::MalformedSignature("public_key not base64"))?;
        if pk_bytes.len() != PUBLIC_KEY_LENGTH {
            return Err(IdentityError::MalformedSignature("public_key wrong length"));
        }
        let mut pk_arr = [0u8; PUBLIC_KEY_LENGTH];
        pk_arr.copy_from_slice(&pk_bytes);
        let vk = VerifyingKey::from_bytes(&pk_arr)
            .map_err(|_| IdentityError::MalformedSignature("public_key not on curve"))?;

        let sig_bytes = B64
            .decode(self.signature.as_bytes())
            .map_err(|_| IdentityError::MalformedSignature("signature not base64"))?;
        if sig_bytes.len() != SIGNATURE_LENGTH {
            return Err(IdentityError::MalformedSignature("signature wrong length"));
        }
        let mut sig_arr = [0u8; SIGNATURE_LENGTH];
        sig_arr.copy_from_slice(&sig_bytes);
        let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);

        vk.verify_strict(payload, &sig)
            .map_err(|_| IdentityError::VerificationFailed)
    }
}

/// Marker trait for "this struct produces the payload bytes a Signature
/// is computed over." Use it on Receipt-shaped types so the canonical
/// form is one obviously-correct method, not a re-derivation at every
/// call site.
pub trait SignaturePayload {
    /// Produce the bytes that get signed / verified. Two equivalent
    /// values must produce byte-identical output.
    fn signing_payload(&self) -> Vec<u8>;
}

/// Errors produced by the identity / signature layer.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// I/O failure (read / write / mkdir) on a specific path.
    #[error("I/O on {path}: {source}")]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The on-disk key file had the wrong length (not 32 bytes).
    #[error("identity key file `{path}` has wrong length: expected {expected}, got {actual}")]
    BadKeyLength {
        /// The path read from.
        path: PathBuf,
        /// Required length (32 bytes for Ed25519 seed).
        expected: usize,
        /// Actual length read.
        actual: usize,
    },

    /// A key already exists at the target path (refusing to overwrite).
    #[error("identity key already exists at `{0}` — refusing to overwrite")]
    AlreadyExists(PathBuf),

    /// Signature envelope had an algorithm string we don't recognize.
    #[error("unsupported signature algorithm `{0}` — expected Ed25519")]
    UnknownAlgorithm(String),

    /// Signature envelope was structurally invalid (bad base64, wrong
    /// length, etc.).
    #[error("malformed signature envelope: {0}")]
    MalformedSignature(&'static str),

    /// Signature verification failed.
    #[error("signature verification failed")]
    VerificationFailed,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn generate_produces_distinct_keys() {
        let a = IdentityKey::generate();
        let b = IdentityKey::generate();
        assert_ne!(a.public_key_bytes(), b.public_key_bytes());
    }

    #[test]
    fn public_key_bytes_is_32_and_b64_is_44() {
        let k = IdentityKey::generate();
        assert_eq!(k.public_key_bytes().len(), 32);
        // 32 bytes base64-encoded with standard padding is 44 chars.
        assert_eq!(k.public_key_b64().len(), 44);
    }

    #[test]
    fn sign_then_verify_succeeds_with_same_key() {
        let k = IdentityKey::generate();
        let payload = b"the quick brown fox jumps over the lazy dog";
        let sig = k.sign(payload);
        sig.verify(payload).expect("signature verifies");
    }

    #[test]
    fn verify_rejects_a_tampered_payload() {
        let k = IdentityKey::generate();
        let sig = k.sign(b"original payload");
        match sig.verify(b"tampered payload") {
            Err(IdentityError::VerificationFailed) => {}
            other => panic!("expected VerificationFailed, got {other:?}"),
        }
    }

    #[test]
    fn verify_rejects_a_tampered_signature_byte() {
        let k = IdentityKey::generate();
        let payload = b"hello";
        let mut sig = k.sign(payload);
        // Flip a byte in the base64 signature. Decode, mutate, re-encode
        // so we stay valid base64 but the underlying bytes are wrong.
        let mut raw = B64.decode(sig.signature.as_bytes()).unwrap();
        raw[0] ^= 0x01;
        sig.signature = B64.encode(&raw);
        match sig.verify(payload) {
            Err(IdentityError::VerificationFailed) => {}
            other => panic!("expected VerificationFailed, got {other:?}"),
        }
    }

    #[test]
    fn verify_rejects_unknown_algorithm() {
        let k = IdentityKey::generate();
        let mut sig = k.sign(b"x");
        sig.algorithm = "RSA".into();
        match sig.verify(b"x") {
            Err(IdentityError::UnknownAlgorithm(a)) => assert_eq!(a, "RSA"),
            other => panic!("expected UnknownAlgorithm, got {other:?}"),
        }
    }

    #[test]
    fn verify_rejects_malformed_pubkey_base64() {
        let k = IdentityKey::generate();
        let mut sig = k.sign(b"x");
        sig.public_key = "!!!!not-base64!!!!".into();
        match sig.verify(b"x") {
            Err(IdentityError::MalformedSignature(_)) => {}
            other => panic!("expected MalformedSignature, got {other:?}"),
        }
    }

    #[test]
    fn save_and_load_roundtrip_preserves_keys() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("identity.key");

        let original = IdentityKey::generate();
        let original_pk = original.public_key_bytes();
        original.save(&path).expect("save ok");

        let loaded = IdentityKey::load(&path).expect("load ok");
        assert_eq!(loaded.public_key_bytes(), original_pk);

        // And a payload signed by the loaded key verifies.
        let sig = loaded.sign(b"after load");
        sig.verify(b"after load")
            .expect("loaded key signs+verifies");
    }

    #[test]
    fn save_refuses_to_overwrite_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("identity.key");
        IdentityKey::generate().save(&path).expect("first save ok");

        let err = IdentityKey::generate()
            .save(&path)
            .expect_err("second save must fail");
        match err {
            IdentityError::AlreadyExists(p) => assert_eq!(p, path),
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn load_rejects_a_wrong_length_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("identity.key");
        // Write 31 bytes instead of 32.
        fs::write(&path, [0u8; 31]).unwrap();
        match IdentityKey::load(&path) {
            Err(IdentityError::BadKeyLength {
                expected, actual, ..
            }) => {
                assert_eq!(expected, 32);
                assert_eq!(actual, 31);
            }
            other => panic!("expected BadKeyLength, got {other:?}"),
        }
    }

    #[test]
    fn load_missing_file_is_an_io_error() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist.key");
        match IdentityKey::load(&missing) {
            Err(IdentityError::Io { path, .. }) => assert_eq!(path, missing),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn signature_envelope_serializes_to_json_with_expected_keys() {
        let k = IdentityKey::generate();
        let sig = k.sign(b"payload");
        let j: serde_json::Value = serde_json::to_value(&sig).expect("envelope serializes");
        assert_eq!(j["algorithm"], "Ed25519");
        assert!(j["public_key"].is_string());
        assert!(j["signature"].is_string());
        // Roundtrip.
        let back: Signature = serde_json::from_value(j).expect("envelope round-trips");
        assert_eq!(sig, back);
    }

    #[test]
    fn debug_does_not_leak_private_key_bytes() {
        let k = IdentityKey::generate();
        let dbg = format!("{k:?}");
        // It must mention the public key but never expose the raw 32-byte
        // secret. We can't check exhaustively, but the b64 public key is
        // the only "key" string allowed in the Debug output.
        assert!(dbg.contains(&k.public_key_b64()));
        // Sanity: the raw bytes themselves are not present (they'd appear
        // as a `[NN, NN, ...]` array in any default Debug derivation).
        assert!(!dbg.contains("signing"));
    }
}
