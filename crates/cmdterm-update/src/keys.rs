//! Release signing keys: Ed25519 plus ML-DSA-65, like the remote machine
//! keys of `scriptschnell-term-remote`, but with a context of their own, so a release
//! signature never verifies as a handshake signature or the other way round.
//!
//! The secret is one line of base64 holding the two 32-byte seeds. The
//! public key is one line: `scriptschnell-release-v1 <base64> [comment]`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer as _, Verifier as _};
use ml_dsa::{EncodedSignature, EncodedVerifyingKey, Keypair as _, MlDsa65};
use zeroize::Zeroizing;

/// Algorithm name in public key lines.
pub const ALGORITHM: &str = "scriptschnell-release-v1";
/// ML-DSA context string.
const CONTEXT: &[u8] = b"scriptschnell-term-release-v1";

const ED_PUBLIC: usize = 32;
const ML_PUBLIC: usize = 1952;
const ED_SIGNATURE: usize = 64;
const ML_SIGNATURE: usize = 3309;
const PUBLIC_LEN: usize = ED_PUBLIC + ML_PUBLIC;
const SIGNATURE_LEN: usize = ED_SIGNATURE + ML_SIGNATURE;

/// A private release key. Only the release workflow holds one.
pub struct SigningKey {
    seed: Zeroizing<[u8; 64]>,
    ed: ed25519_dalek::SigningKey,
    ml: ml_dsa::SigningKey<MlDsa65>,
}

impl SigningKey {
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut seed = Zeroizing::new([0u8; 64]);
        getrandom::fill(seed.as_mut())?;
        Ok(Self::from_seed(&seed))
    }

    /// The Ed25519 seed followed by the ML-DSA seed.
    pub fn from_seed(seed: &[u8; 64]) -> Self {
        let mut ed_seed = Zeroizing::new([0u8; 32]);
        ed_seed.copy_from_slice(&seed[..32]);
        let ed = ed25519_dalek::SigningKey::from_bytes(&ed_seed);
        let mut ml_seed = ml_dsa::Seed::default();
        ml_seed.copy_from_slice(&seed[32..]);
        let ml = ml_dsa::SigningKey::<MlDsa65>::from_seed(&ml_seed);
        Self {
            seed: Zeroizing::new(*seed),
            ed,
            ml,
        }
    }

    /// The key of a [`Self::to_secret`] line.
    pub fn from_secret(secret: &str) -> Option<Self> {
        let body = Zeroizing::new(STANDARD.decode(secret.trim()).ok()?);
        let seed: Zeroizing<[u8; 64]> = Zeroizing::new(body.as_slice().try_into().ok()?);
        Some(Self::from_seed(&seed))
    }

    /// The seed as one line of base64.
    pub fn to_secret(&self) -> Zeroizing<String> {
        Zeroizing::new(STANDARD.encode(self.seed.as_ref()))
    }

    pub fn public(&self) -> VerifyingKey {
        let mut bytes = Vec::with_capacity(PUBLIC_LEN);
        bytes.extend_from_slice(self.ed.verifying_key().as_bytes());
        bytes.extend_from_slice(&self.ml.verifying_key().encode());
        VerifyingKey::from_bytes(&bytes).expect("own public key decodes")
    }

    /// Signs `message` with both keys.
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        let mut signature = Vec::with_capacity(SIGNATURE_LEN);
        signature.extend_from_slice(&self.ed.sign(message).to_bytes());
        let ml = self
            .ml
            .expanded_key()
            .sign_deterministic(message, CONTEXT)
            .expect("context is shorter than 255 bytes");
        signature.extend_from_slice(&ml.encode());
        signature
    }
}

/// A public release key, built into the app.
#[derive(Clone)]
pub struct VerifyingKey {
    bytes: Vec<u8>,
    ed: ed25519_dalek::VerifyingKey,
    ml: ml_dsa::VerifyingKey<MlDsa65>,
}

impl PartialEq for VerifyingKey {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for VerifyingKey {}

impl std::fmt::Debug for VerifyingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_line(""))
    }
}

impl VerifyingKey {
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != PUBLIC_LEN {
            return None;
        }
        let ed_bytes: [u8; ED_PUBLIC] = bytes[..ED_PUBLIC].try_into().ok()?;
        let ed = ed25519_dalek::VerifyingKey::from_bytes(&ed_bytes).ok()?;
        let ml_bytes = EncodedVerifyingKey::<MlDsa65>::try_from(&bytes[ED_PUBLIC..]).ok()?;
        let ml = ml_dsa::VerifyingKey::<MlDsa65>::decode(&ml_bytes);
        Some(Self {
            bytes: bytes.to_vec(),
            ed,
            ml,
        })
    }

    /// Parses `scriptschnell-release-v1 <base64> [comment]`.
    pub fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        if words.next()? != ALGORITHM {
            return None;
        }
        Self::from_bytes(&STANDARD.decode(words.next()?).ok()?)
    }

    pub fn to_line(&self, comment: &str) -> String {
        let key = STANDARD.encode(&self.bytes);
        if comment.is_empty() {
            format!("{ALGORITHM} {key}")
        } else {
            format!("{ALGORITHM} {key} {comment}")
        }
    }

    /// True when both halves of `signature` verify over `message`.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        if signature.len() != SIGNATURE_LEN {
            return false;
        }
        let Ok(ed_sig) = <[u8; ED_SIGNATURE]>::try_from(&signature[..ED_SIGNATURE]) else {
            return false;
        };
        let ed_ok = self
            .ed
            .verify(message, &ed25519_dalek::Signature::from_bytes(&ed_sig))
            .is_ok();
        let ml_ok = EncodedSignature::<MlDsa65>::try_from(&signature[ED_SIGNATURE..])
            .ok()
            .and_then(|enc| ml_dsa::Signature::<MlDsa65>::decode(&enc))
            .is_some_and(|sig| self.ml.verify_with_context(message, CONTEXT, &sig));
        ed_ok & ml_ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_need_both_halves() {
        let key = SigningKey::from_seed(&[7; 64]);
        let public = key.public();
        let sig = key.sign(b"manifest");
        assert!(public.verify(b"manifest", &sig));
        assert!(!public.verify(b"other", &sig));
        for flip in [0, ED_SIGNATURE + 10] {
            let mut bad = sig.clone();
            bad[flip] ^= 1;
            assert!(!public.verify(b"manifest", &bad), "flip at {flip}");
        }
        assert!(
            !SigningKey::from_seed(&[8; 64])
                .public()
                .verify(b"manifest", &sig)
        );
    }

    #[test]
    fn keys_round_trip_as_lines() {
        let key = SigningKey::from_seed(&[3; 64]);
        let again = SigningKey::from_secret(&key.to_secret()).unwrap();
        assert_eq!(key.public(), again.public());
        let line = key.public().to_line("release");
        assert_eq!(VerifyingKey::parse(&line), Some(key.public()));
        assert_eq!(VerifyingKey::parse("cmdterm-hybrid-v1 AAAA"), None);
    }
}
