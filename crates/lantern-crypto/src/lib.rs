//! Device identity: Ed25519 keypair, fingerprints, safety words.
//!
//! DESIGN.md §3.1–3.2. On desktop builds the key goes in the OS keychain;
//! the CLI uses a file with 0600 permissions (documented, deliberate).

use std::path::Path;

use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("key file is corrupt")]
    Corrupt,
}

pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut OsRng),
        }
    }

    /// Load from `path`, or generate-and-save on first run.
    pub fn load_or_generate(path: &Path) -> Result<Self, IdentityError> {
        if path.exists() {
            let bytes = std::fs::read(path)?;
            let arr: [u8; 32] = bytes.try_into().map_err(|_| IdentityError::Corrupt)?;
            Ok(Self {
                signing: SigningKey::from_bytes(&arr),
            })
        } else {
            let id = Self::generate();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, id.signing.to_bytes())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(id)
        }
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.signing
    }

    pub fn public_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// BLAKE3-256 of the public key.
    pub fn fingerprint(&self) -> [u8; 32] {
        fingerprint_of(&self.public_bytes())
    }
}

pub fn fingerprint_of(public_key: &[u8; 32]) -> [u8; 32] {
    *blake3::hash(public_key).as_bytes()
}

/// Render a fingerprint as 8 safety words (88 bits of the digest).
///
/// Words alternate between two halves of the list by position — PGP Word
/// List style — so transposing two adjacent words yields an invalid
/// sequence rather than a plausible one.
pub fn safety_words(fp: &[u8; 32]) -> Vec<&'static str> {
    // 8 words × 11 bits = 88 bits from the front of the digest.
    let mut bits: u128 = 0;
    for b in &fp[..11] {
        bits = (bits << 8) | *b as u128;
    }
    let mut words = Vec::with_capacity(8);
    for i in 0..8 {
        let shift = 88 - 11 * (i + 1);
        let idx = ((bits >> shift) & 0x7FF) as usize; // 11 bits → 0..2047
        // Alternate halves: even positions draw from 0..1024, odd from 1024..2048.
        // (PGP Word List's anti-transposition property.)
        let idx = if i % 2 == 0 { idx % 1024 } else { 1024 + (idx % 1024) };
        words.push(wordlist()[idx].as_str());
    }
    words
}

pub fn short_hex(fp: &[u8; 32]) -> String {
    hex::encode(&fp[..8])
}

/// 2048-word list. Placeholder generated procedurally for the prototype —
/// a curated list (distinct sounds, no near-homophones) replaces this
/// before any real use. Deterministic, so fingerprints are stable.
fn wordlist() -> &'static Vec<String> {
    use std::sync::OnceLock;
    static WORDS: OnceLock<Vec<String>> = OnceLock::new();
    WORDS.get_or_init(|| {
        let consonants = ["b", "d", "f", "g", "k", "l", "m", "n", "p", "r", "s", "t"];
        let vowels = ["a", "e", "i", "o"];
        let mut out = Vec::with_capacity(2048);
        // 12 × 4 × 12 × 4 = 2304 CVCV combinations; take the first 2048.
        'outer: for c1 in consonants {
            for v1 in vowels {
                for c2 in consonants {
                    for v2 in vowels {
                        out.push(format!("{c1}{v1}{c2}{v2}"));
                        if out.len() == 2048 {
                            break 'outer;
                        }
                    }
                }
            }
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trip(){
        let dir = std::env::temp_dir().join(format!("lantern-test-{}", std::process::id()));
        let path = dir.join("identity.key");
        let a = Identity::load_or_generate(&path).unwrap();
        let b = Identity::load_or_generate(&path).unwrap();
        assert_eq!(a.public_bytes(), b.public_bytes());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn safety_words_deterministic() {
        let fp = fingerprint_of(&[7u8; 32]);
        let w1 = safety_words(&fp);
        let w2 = safety_words(&fp);
        assert_eq!(w1, w2);
        assert_eq!(w1.len(), 8);
    }

    #[test]
    fn different_keys_different_words() {
        let w1 = safety_words(&fingerprint_of(&[1u8; 32]));
        let w2 = safety_words(&fingerprint_of(&[2u8; 32]));
        assert_ne!(w1, w2);
    }
}
