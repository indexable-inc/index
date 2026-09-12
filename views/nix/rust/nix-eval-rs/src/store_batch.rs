//! Derivation ingestion: validated ATerms become one store import stream.
//!
//! The C++ adapter only materialises lazy sources and submits these bytes.
//! Wire fields use the store's little-endian u64/string framing, with strings
//! padded to eight bytes. The envelope carries an accounting count and a set
//! of lazy sources; the remaining bytes are an AddMultipleToStore stream with
//! WorkerProto 1.16 metadata followed by a regular-file NAR for each entry.

use crate::drv::{Derivation, OutputKind};
use crate::drvpath::{CaMethod, make_text_path_from_hash};
use crate::nixhash::{Hash, HashAlgo};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;

/// A validated derivation whose bytes and reference set are immutable until
/// ingestion. Parsing happens once, before the evaluator answers its path.
pub(crate) struct PendingDerivation {
    pub(crate) expected: String,
    aterm: String,
    references: Vec<String>,
    input_sources: Vec<String>,
    text_hash: [u8; 32],
}

impl PendingDerivation {
    pub(crate) fn new(store_dir: &str, name: &str, aterm: &str) -> Result<Self, String> {
        let validate = || {
            let drv_name = format!("{name}.drv");
            crate::storepath::check_name(&drv_name)?;
            let drv = crate::drv::parse(aterm).map_err(|e| e.to_string())?;
            validate_derivation(store_dir, name, &drv)?;
            if crate::drv::unparse(&drv, false) != aterm {
                return Err("ATerm is not the canonical rendering".to_owned());
            }
            let references = drv.references();
            let text_hash: [u8; 32] = Sha256::digest(aterm.as_bytes()).into();
            let expected = make_text_path_from_hash(
                store_dir,
                &drv_name,
                &text_hash,
                references.iter().map(String::as_str),
            );
            Ok(Self {
                expected,
                aterm: aterm.to_owned(),
                references,
                input_sources: drv.input_srcs,
                text_hash,
            })
        };
        validate().map_err(|error| format!("derivation '{name}': {error}"))
    }
}

fn strictly_sorted<'a>(mut values: impl Iterator<Item = &'a str>) -> bool {
    let Some(mut previous) = values.next() else {
        return true;
    };
    for value in values {
        if previous >= value {
            return false;
        }
        previous = value;
    }
    true
}

fn validate_path(store_dir: &str, path: &str) -> Result<(), String> {
    let canonical = crate::storepath::parse_store_path(store_dir, path)
        .map(|base| format!("{store_dir}/{base}"));
    if canonical.as_deref() != Some(path) {
        return Err(format!("invalid or noncanonical store path '{path}'"));
    }
    Ok(())
}

fn validate_derivation(store_dir: &str, name: &str, drv: &Derivation) -> Result<(), String> {
    if !strictly_sorted(drv.outputs.iter().map(|output| output.name.as_str()))
        || !strictly_sorted(drv.input_drvs.iter().map(|input| input.drv_path.as_str()))
        || !strictly_sorted(drv.input_srcs.iter().map(String::as_str))
        || !strictly_sorted(drv.env.iter().map(|entry| entry.name.as_str()))
        || drv
            .input_drvs
            .iter()
            .any(|input| !strictly_sorted(input.outputs.iter().map(String::as_str)))
    {
        return Err("ATerm sets and maps must be sorted without duplicate keys".to_owned());
    }
    for path in drv
        .input_srcs
        .iter()
        .map(String::as_str)
        .chain(drv.input_drvs.iter().map(|input| input.drv_path.as_str()))
    {
        validate_path(store_dir, path)?;
    }
    for output in &drv.outputs {
        if !output.path.is_empty() {
            validate_path(store_dir, &output.path)?;
        }
        let kind = output.kind();
        if kind == OutputKind::Unrecognised {
            return Err(format!("output '{}' has an invalid address", output.name));
        }
        if output.hash_algo.is_empty() {
            continue;
        }
        let (method, algo_name) = if let Some(algo) = output.hash_algo.strip_prefix("r:") {
            (CaMethod::NixArchive, algo)
        } else if let Some(algo) = output.hash_algo.strip_prefix("text:") {
            (CaMethod::Text, algo)
        } else if let Some(algo) = output.hash_algo.strip_prefix("git:") {
            (CaMethod::Git, algo)
        } else {
            (CaMethod::Flat, output.hash_algo.as_str())
        };
        let algo = crate::nixhash::parse_algo(algo_name).map_err(|e| e.to_string())?;
        if method == CaMethod::Git && !matches!(algo, HashAlgo::Sha1 | HashAlgo::Sha256) {
            return Err("Git output hashing requires SHA-1 or SHA-256".to_owned());
        }
        if method == CaMethod::Text && algo != HashAlgo::Sha256 {
            return Err("text output hashing requires SHA-256".to_owned());
        }
        if kind == OutputKind::CaFixed {
            let hash = crate::nixhash::parse_any(&output.hash, Some(algo), true)
                .map_err(|e| e.to_string())?;
            if hash.to_base16(false) != output.hash {
                return Err(format!("output '{}' has a noncanonical hash", output.name));
            }
            let output_name = crate::drvpath::output_path_name(name, &output.name);
            let expected = if method == CaMethod::Text {
                let digest: &[u8; 32] = hash
                    .bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| "invalid SHA-256 output hash length".to_owned())?;
                make_text_path_from_hash(store_dir, &output_name, digest, std::iter::empty())
            } else {
                crate::drvpath::make_fixed_output_path(store_dir, &output_name, method, &hash)
            };
            if output.path != expected {
                return Err(format!(
                    "output '{}' has an incorrect fixed path",
                    output.name
                ));
            }
        }
    }
    Ok(())
}

fn integer(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn string(bytes: &mut Vec<u8>, value: &[u8]) {
    integer(bytes, value.len() as u64);
    bytes.extend_from_slice(value);
    // Every preceding wire field ends on an eight-byte boundary.
    bytes.resize(bytes.len().next_multiple_of(8), 0);
}

fn strings<'a>(bytes: &mut Vec<u8>, values: impl ExactSizeIterator<Item = &'a str>) {
    integer(bytes, values.len() as u64);
    for value in values {
        string(bytes, value.as_bytes());
    }
}

fn regular_nar(bytes: &mut Vec<u8>, contents: &[u8]) {
    for value in [
        b"nix-archive-1".as_slice(),
        b"(",
        b"type",
        b"regular",
        b"contents",
        contents,
        b")",
    ] {
        string(bytes, value);
    }
}

fn hex_digest(digest: &[u8]) -> String {
    Hash {
        algo: HashAlgo::Sha256,
        bytes: digest.to_vec(),
    }
    .to_base16(false)
}

/// Build one packet without allocating an intermediate NAR per derivation.
/// The NAR hash is filled into its reserved metadata field after the bytes
/// have been appended. Signatures, ultimate trust and registration time stay
/// empty/false/zero; the store receives the same content-addressed import
/// contract as for any other locally evaluated derivation.
pub(crate) fn encode(pending: &[PendingDerivation]) -> Result<Vec<u8>, String> {
    let sources: BTreeSet<&str> = pending
        .iter()
        .flat_map(|drv| drv.input_sources.iter().map(String::as_str))
        .collect();
    let mut bytes = Vec::with_capacity(pending.iter().map(|drv| drv.aterm.len() + 512).sum());
    integer(&mut bytes, pending.len() as u64);
    strings(&mut bytes, sources.iter().copied());
    integer(&mut bytes, pending.len() as u64);
    for drv in pending {
        string(&mut bytes, drv.expected.as_bytes());
        string(&mut bytes, b""); // No deriver for a .drv object.
        let hash_at = bytes.len() + 8;
        string(&mut bytes, &[0; 64]);
        strings(&mut bytes, drv.references.iter().map(String::as_str));
        integer(&mut bytes, 0); // Registration time belongs to the store.
        integer(
            &mut bytes,
            (112 + drv.aterm.len().next_multiple_of(8)) as u64,
        );
        integer(&mut bytes, 0); // Not marked ultimately trusted.
        integer(&mut bytes, 0); // No signatures.
        string(
            &mut bytes,
            format!(
                "text:sha256:{}",
                crate::drvpath::nix32_encode(&drv.text_hash)
            )
            .as_bytes(),
        );
        let nar_at = bytes.len();
        regular_nar(&mut bytes, drv.aterm.as_bytes());
        let nar = bytes
            .get(nar_at..)
            .ok_or_else(|| "invalid NAR boundary in derivation batch".to_owned())?;
        let hash = hex_digest(&Sha256::digest(nar));
        bytes
            .get_mut(hash_at..hash_at + 64)
            .ok_or_else(|| "invalid hash boundary in derivation batch".to_owned())?
            .copy_from_slice(hash.as_bytes());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = r#"Derive([],[],[],"x86_64-linux","/bin/sh",[],[])"#;
    const SOURCE: &str = "/nix/store/00000000000000000000000000000000-source";
    const DEPENDENCY: &str = "/nix/store/11111111111111111111111111111111-dependency.drv";

    struct Reader<'a> {
        rest: &'a [u8],
    }

    impl<'a> Reader<'a> {
        fn take(&mut self, size: usize) -> Result<&'a [u8], String> {
            let (value, rest) = self
                .rest
                .split_at_checked(size)
                .ok_or_else(|| "truncated packet".to_owned())?;
            self.rest = rest;
            Ok(value)
        }

        fn number(&mut self) -> Result<u64, String> {
            let bytes = self.take(8)?;
            let word = bytes.try_into().map_err(|_| "invalid word".to_owned())?;
            Ok(u64::from_le_bytes(word))
        }

        fn string(&mut self) -> Result<&'a [u8], String> {
            let size = usize::try_from(self.number()?).map_err(|e| e.to_string())?;
            let value = self.take(size)?;
            let padding = (8 - size % 8) % 8;
            if self.take(padding)?.iter().any(|byte| *byte != 0) {
                return Err("nonzero padding".to_owned());
            }
            Ok(value)
        }

        fn strings(&mut self) -> Result<Vec<&'a [u8]>, String> {
            let count = self.number()?;
            (0..count).map(|_| self.string()).collect()
        }
    }

    #[test]
    fn import_metadata_and_nar_match_independent_digest_vectors() -> Result<(), String> {
        let drv = PendingDerivation::new("/nix/store", "empty", EMPTY)?;
        let expected = drv.expected.clone();
        let packet = encode(&[drv])?;
        let mut reader = Reader { rest: &packet };
        assert_eq!(reader.number()?, 1); // Envelope accounting.
        assert!(reader.strings()?.is_empty()); // Lazy sources.
        assert_eq!(reader.number()?, 1); // Store stream count.
        assert_eq!(reader.string()?, expected.as_bytes());
        assert_eq!(reader.string()?, b""); // No deriver.
        assert_eq!(
            reader.string()?,
            b"a1fae479b23e098ebf7a62ed676e475eb01fe621549d8e475e26b6c74b78d843"
        );
        assert!(reader.strings()?.is_empty()); // References.
        assert_eq!(reader.number()?, 0); // Registration time.
        assert_eq!(reader.number()?, 160); // Independently framed NAR size.
        assert_eq!(reader.number()?, 0); // Ultimate trust must stay false.
        assert!(reader.strings()?.is_empty()); // No fabricated signatures.
        let hash = crate::nixhash::parse_any(
            "cefba38ac615a46c47b1b61b1a1deafc4d520f914e282152c525493aabfddd63",
            Some(HashAlgo::Sha256),
            true,
        )
        .map_err(|e| e.to_string())?;
        let ca = format!("text:sha256:{}", crate::drvpath::nix32_encode(&hash.bytes));
        assert_eq!(reader.string()?, ca.as_bytes());
        for field in [
            b"nix-archive-1".as_slice(),
            b"(",
            b"type",
            b"regular",
            b"contents",
            EMPTY.as_bytes(),
            b")",
        ] {
            assert_eq!(reader.string()?, field);
        }
        assert!(reader.rest.is_empty());
        Ok(())
    }

    #[test]
    fn references_include_inputs_but_never_outputs_and_lazy_sources_are_deduplicated()
    -> Result<(), String> {
        let text = format!(
            r#"Derive([("out","/nix/store/22222222222222222222222222222222-output","","")],[("{DEPENDENCY}",["out"])],["{SOURCE}"],"x86_64-linux","/bin/sh",[],[])"#
        );
        let first = PendingDerivation::new("/nix/store", "first", &text)?;
        let second = PendingDerivation::new("/nix/store", "second", &text)?;
        let expected = first.expected.clone();
        let packet = encode(&[first, second])?;
        let mut reader = Reader { rest: &packet };
        assert_eq!(reader.number()?, 2);
        assert_eq!(reader.strings()?, vec![SOURCE.as_bytes()]);
        assert_eq!(reader.number()?, 2);
        assert_eq!(reader.string()?, expected.as_bytes());
        reader.string()?; // Deriver.
        reader.string()?; // NAR hash.
        assert_eq!(
            reader.strings()?,
            vec![SOURCE.as_bytes(), DEPENDENCY.as_bytes()]
        );
        let mut changed = crate::drv::parse(&text).map_err(|e| e.to_string())?;
        changed.input_srcs.clear();
        let without_source =
            PendingDerivation::new("/nix/store", "first", &crate::drv::unparse(&changed, false))?;
        assert_ne!(expected, without_source.expected);
        Ok(())
    }

    #[test]
    fn malformed_noncanonical_and_corrupt_derivations_are_refused() {
        let duplicate_source =
            format!(r#"Derive([],[],["{SOURCE}","{SOURCE}"],"x86_64-linux","/bin/sh",[],[])"#);
        let invalid_path = EMPTY.replace("[],[],[]", "[],[],[\"/tmp/source\"]");
        let wrong_fixed_path = format!(
            r#"Derive([("out","{SOURCE}","sha256","{}")],[],[],"x86_64-linux","/bin/sh",[],[])"#,
            "0".repeat(64)
        );
        for text in [
            "not an ATerm".to_owned(),
            format!("{EMPTY}trailing"),
            duplicate_source,
            invalid_path,
            wrong_fixed_path,
            EMPTY.replace("/bin/sh", "\\/bin/sh"),
            EMPTY.replace(
                "[],[],[]",
                "[],[],[\"/nix/store/00000000000000000000000000000000-source/\"]",
            ),
        ] {
            let result = PendingDerivation::new("/nix/store", "broken", &text);
            assert!(result.is_err(), "accepted corrupt derivation: {text}");
            if let Err(error) = result {
                assert!(error.contains("derivation 'broken'"), "{error}");
            }
        }
        assert!(PendingDerivation::new("/nix/store", "../escape", EMPTY).is_err());
    }

    #[test]
    fn fixed_output_paths_are_validated_for_each_address_method() -> Result<(), String> {
        let hash = Hash::zero(HashAlgo::Sha256);
        for method in [
            CaMethod::Flat,
            CaMethod::NixArchive,
            CaMethod::Text,
            CaMethod::Git,
        ] {
            let output = if method == CaMethod::Text {
                make_text_path_from_hash("/nix/store", "fixed", &[0; 32], std::iter::empty())
            } else {
                crate::drvpath::make_fixed_output_path("/nix/store", "fixed", method, &hash)
            };
            let aterm = format!(
                r#"Derive([("out","{output}","{}","{}")],[],[],"x86_64-linux","/bin/sh",[],[])"#,
                method.print_method_algo(HashAlgo::Sha256),
                hash.to_base16(false),
            );
            PendingDerivation::new("/nix/store", "fixed", &aterm)?;
            assert!(PendingDerivation::new("/nix/store", "different-name", &aterm).is_err());
        }
        Ok(())
    }

    #[test]
    fn binary_contents_and_every_padding_width_have_exact_nar_size() -> Result<(), String> {
        for length in 0..16 {
            let mut drv = crate::drv::parse(EMPTY).map_err(|e| e.to_string())?;
            drv.args = vec![format!("before\0after{}", "x".repeat(length))];
            let aterm = crate::drv::unparse(&drv, false);
            let pending = PendingDerivation::new("/nix/store", "binary", &aterm)?;
            let packet = encode(&[pending])?;
            let mut reader = Reader { rest: &packet };
            reader.number()?;
            reader.strings()?;
            reader.number()?;
            reader.string()?;
            reader.string()?;
            let recorded_hash = reader.string()?;
            reader.strings()?;
            reader.number()?;
            let size = reader.number()?;
            reader.number()?;
            reader.strings()?;
            reader.string()?;
            assert_eq!(size, reader.rest.len() as u64);
            assert_eq!(
                recorded_hash,
                hex_digest(&Sha256::digest(reader.rest)).as_bytes()
            );
            for _ in 0..5 {
                reader.string()?;
            }
            assert_eq!(reader.string()?, aterm.as_bytes());
            assert_eq!(reader.string()?, b")");
            assert!(reader.rest.is_empty());
        }
        Ok(())
    }
}
