//! The one hash primitive, and the three identifiers built from it.
//!
//! Every hash in the kernel is `H(tag || fields)` with a versioned tag, so no
//! two kinds of identifier can ever collide even if their payloads are
//! byte-identical: a domain, a key and an object address over the same bytes
//! are three different hashes. Tags carry a version suffix so that changing
//! what goes into a hash is a rename rather than a silent reinterpretation of
//! everybody's existing lock files.
//!
//! Injectivity of the preimage: [`tagged`] writes each field as a u64
//! little-endian length followed by the field bytes, tag included. A
//! length-prefixed concatenation parses back to exactly one field list, so
//! distinct `(tag, fields)` inputs always produce distinct preimages, and the
//! only way to collide is to break blake3.

use core::fmt;

/// A blake3-256 digest. The only hash type in the kernel.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash([u8; 32]);

impl Hash {
    /// Wrap raw digest bytes. Prefer [`tagged`]; this exists for values that
    /// were hashed elsewhere and arrive as bytes (a lock file, a manifest).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex, the spelling used in `effect.lock` and in errors.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(nibble(byte >> 4));
            out.push(nibble(byte & 0x0f));
        }
        out
    }

    /// Parse the output of [`Hash::to_hex`]. Rejects odd lengths, wrong
    /// lengths and non-hex digits rather than truncating or padding, because a
    /// silently repaired hash is a silently wrong trust decision.
    pub fn from_hex(text: &str) -> Result<Self, HexError> {
        if text.len() != 64 {
            return Err(HexError::Length { got: text.len() });
        }
        let mut bytes = [0u8; 32];
        for (slot, pair) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
            let (Some(hi), Some(lo)) = (pair.first(), pair.last()) else {
                return Err(HexError::Length { got: text.len() });
            };
            *slot = (unnibble(*hi)? << 4) | unnibble(*lo)?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

const fn nibble(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'a' + value - 10) as char,
    }
}

const fn unnibble(byte: u8) -> Result<u8, HexError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(HexError::Digit { byte }),
    }
}

/// Why a hex string was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HexError {
    Length { got: usize },
    Digit { byte: u8 },
}

impl fmt::Display for HexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Length { got } => write!(f, "expected 64 hex digits, got {got} characters"),
            Self::Digit { byte } => {
                write!(f, "expected a lowercase hex digit, got byte 0x{byte:02x}")
            }
        }
    }
}

impl core::error::Error for HexError {}

/// `H(tag || fields)`, with every part length-prefixed so the preimage parses
/// back to exactly one input. This is the only place blake3 is called.
#[must_use]
pub fn tagged(tag: &str, fields: &[&[u8]]) -> Hash {
    let mut hasher = TaggedHasher::new(tag);
    for field in fields {
        hasher.field(field);
    }
    hasher.finish()
}

/// Incrementally hash the same length-prefixed fields as [`tagged`].
/// Callers can release each field after feeding it to the hasher.
pub struct TaggedHasher(blake3::Hasher);

impl TaggedHasher {
    #[must_use]
    pub fn new(tag: &str) -> Self {
        let mut hasher = Self(blake3::Hasher::new());
        hasher.field(tag.as_bytes());
        hasher
    }

    pub fn field(&mut self, bytes: &[u8]) {
        self.0.update(&(bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
    }

    #[must_use]
    pub fn finish(self) -> Hash {
        Hash(*self.0.finalize().as_bytes())
    }
}

/// Tag for [`crate::Domain`]. Bump when the field list changes.
pub const DOMAIN_TAG: &str = "ix-domain-v1";
/// Tag for [`crate::Key`].
pub const KEY_TAG: &str = "ix-key-v1";
/// Tag for [`crate::ObjId`].
pub const OBJ_TAG: &str = "ix-obj-v1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() -> Result<(), HexError> {
        let hash = tagged("t", &[b"x"]);
        assert_eq!(Hash::from_hex(&hash.to_hex())?, hash);
        Ok(())
    }

    #[test]
    fn hex_rejects_malformed_input() {
        assert_eq!(Hash::from_hex("ab"), Err(HexError::Length { got: 2 }));
        let upper = "A".repeat(64);
        assert!(matches!(
            Hash::from_hex(&upper),
            Err(HexError::Digit { byte: b'A' })
        ));
    }

    #[test]
    fn tag_separates_identical_payloads() {
        assert_ne!(tagged(DOMAIN_TAG, &[b"x"]), tagged(KEY_TAG, &[b"x"]));
    }

    /// The length prefixes are what stop `["ab", "c"]` and `["a", "bc"]` from
    /// sharing a preimage. Without them this assertion fails.
    #[test]
    fn field_boundaries_are_part_of_the_preimage() {
        assert_ne!(tagged("t", &[b"ab", b"c"]), tagged("t", &[b"a", b"bc"]));
    }
}
