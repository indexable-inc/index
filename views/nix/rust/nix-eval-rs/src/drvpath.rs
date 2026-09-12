//! Where a derivation's outputs land: `hashDerivationModulo` and the store
//! path construction on top of it.
//!
//! [`crate::drv`] is the bytes of a `.drv` and deliberately knows nothing about
//! what they mean. This module is the other half of rung C step 1: given those
//! bytes, produce the same `outPath` cppnix would. Everything here mirrors a
//! named function in cppnix and the comments say which, because the whole
//! value of the module is that it agrees with that code and not with a reading
//! of the manual.
//!
//! **The oracle this is written against is cppnix's own
//! `Derivation::checkInvariants`** (`src/libstore/derivations.cc:1398`), which
//! asserts that every input-addressed output's path equals
//! `makeOutputPath(name, hashDerivationModulo(drv, true).hashes[name],
//! drvName)`. A real store is hundreds of thousands of derivations that cppnix
//! already checked, so recomputing the path from a `.drv`'s own bytes and
//! comparing to the path written inside it exercises the ATerm writer, the
//! output masking, the modulo recursion, `compressHash` and the base-32
//! encoding together, on every shape the store happens to contain. That is a
//! far larger denominator than one `hello.outPath`, and it needs no evaluator.

use crate::drv::{Derivation, EnvVar, InputDrv, Output, OutputKind, canonicalise, unparse};
use crate::nixhash::{Hash, HashAlgo};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// cppnix's `BaseNix32::characters` (`libutil/base-nix-32.hh`). E, O, U and T
/// are omitted, which is why this is not RFC 4648 base32 and why a stock
/// base32 crate produces a plausible-looking wrong path.
const NIX32_CHARS: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// `BaseNix32::encode` (`libutil/base-nix-32.cc:20`), which walks the output
/// from its last character backwards and reads a 5-bit window that straddles
/// two input bytes. Transcribed rather than rewritten: an encoder written the
/// natural way, most-significant-bit first, produces a different string for
/// the same bytes.
#[must_use]
pub fn nix32_encode(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let len = (bytes.len() * 8 - 1) / 5 + 1;
    let mut out = String::with_capacity(len);
    for n in (0..len).rev() {
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        let lo = bytes.get(i).copied().unwrap_or(0) >> j;
        let hi = if i + 1 < bytes.len() {
            // `<< (8 - j)` with j == 0 would be a shift by 8, which is
            // undefined in C and a panic in debug Rust. cppnix never reaches
            // it because `i + 1 < size` and `j == 0` cannot both hold on its
            // last window; guarding is cheaper than proving that here.
            bytes
                .get(i + 1)
                .copied()
                .unwrap_or(0)
                .checked_shl(8 - j as u32)
                .unwrap_or(0)
        } else {
            0
        };
        let c = (lo | hi) & 0x1f;
        out.push(char::from(
            *NIX32_CHARS.get(usize::from(c)).unwrap_or(&b'0'),
        ));
    }
    out
}

/// `compressHash` (`libutil/hash.cc:416`): fold the digest down by XORing byte
/// `i` into byte `i % new_size`. Not a truncation, which is the mistake that
/// produces a store path differing only in its first characters.
#[must_use]
pub fn compress_hash(hash: &[u8], new_size: usize) -> Vec<u8> {
    let mut out = vec![0u8; new_size];
    if new_size == 0 {
        return out;
    }
    for (i, byte) in hash.iter().enumerate() {
        if let Some(slot) = out.get_mut(i % new_size) {
            *slot ^= *byte;
        }
    }
    out
}

/// `builtins.placeholder`: the string a derivation's own output path is
/// spelled as before that path exists, which is what `$out` expands to inside
/// a content-addressed build.
///
/// cppnix's `hashPlaceholder` (`derivations.cc:1067`): a slash, then the
/// SHA-256 of `nix-output:<name>` in the base-32 alphabet, uncompressed --
/// all 32 bytes, unlike an output path's hash, which `compressHash` folds to
/// 20 first. It is a pure function of the name with no store and no
/// derivation behind it, which is why it can live here rather than behind a
/// `Host` hook.
#[must_use]
pub fn hash_placeholder(output_name: impl AsRef<[u8]>) -> String {
    // Byte concatenation, because cppnix's `hashString(SHA256, "nix-output:"
    // + name)` hashes whatever bytes the name holds (ENG-13147).
    let mut preimage = b"nix-output:".to_vec();
    preimage.extend_from_slice(output_name.as_ref());
    let digest = sha256(&preimage);
    format!("/{}", nix32_encode(&digest))
}

/// `DownstreamPlaceholder::unknownCaOutput` (`downstream-placeholder.cc:12`):
/// how a consumer spells an output of a floating content-addressed (or
/// deferred) derivation before that output's path exists.
///
/// The preimage is `nix-upstream-output:<drvHashPart>:<outputPathName>`, where
/// the hash part is the base-32 run between the store directory and the first
/// `-`, and the name half reuses [`output_path_name`] over the `.drv` name
/// with its extension removed. Rendered like [`hash_placeholder`]: a slash,
/// then the uncompressed SHA-256 in the base-32 alphabet.
///
/// cppnix gates this behind `ca-derivations`; the callers here are only
/// reachable with the feature on (`derivationStrict` checked it when it read
/// `__contentAddressed`, and a deferred derivation needs such an input), so
/// there is no second check to disagree with the first.
#[must_use]
pub fn downstream_placeholder(drv_path: &str, output_name: &str) -> String {
    let base = drv_path.rsplit('/').next().unwrap_or(drv_path);
    let (hash_part, rest) = base.split_once('-').unwrap_or((base, ""));
    let drv_name = rest.strip_suffix(".drv").unwrap_or(rest);
    let clear = format!(
        "nix-upstream-output:{hash_part}:{}",
        output_path_name(drv_name, output_name)
    );
    format!("/{}", nix32_encode(&sha256(clear.as_bytes())))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // `{:02x}` per byte; written as a loop because `hex` is not a
        // dependency of this crate and one format call per byte is not worth
        // adding one.
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// `StoreDirConfig::makeStorePath` (`store-dir-config.cc:72`).
///
/// The fingerprint is `<type>:<hash>:<storeDir>:<name>` where `<hash>` carries
/// its algorithm prefix, because cppnix's two-argument overload passes
/// `hash.to_string(HashFormat::Base16, /*includeAlgo=*/true)`. Dropping the
/// `sha256:` is a silent wrong answer, so callers hand in the prefixed form
/// and this does not add it.
#[must_use]
pub fn make_store_path(store_dir: &str, ty: &str, hash_with_algo: &str, name: &str) -> String {
    // The one chokepoint for the hash phase: `make_output_path`,
    // `make_fixed_output_path` and `make_text_path` all end here, so counting
    // at the three of them would triple-count and counting inside `sha256`
    // would answer "how many block transforms" rather than "how many store
    // paths did this evaluation compute".
    let (path, nanos) = crate::perf::timed(|| {
        let fingerprint = format!("{ty}:{hash_with_algo}:{store_dir}:{name}");
        let compressed = compress_hash(&sha256(fingerprint.as_bytes()), 20);
        format!("{store_dir}/{}-{name}", nix32_encode(&compressed))
    });
    crate::perf::note_hash(nanos);
    path
}

/// `outputPathName` (`derivations.cc:785`): the default output keeps the
/// derivation's name, any other output appends `-<outputName>`.
#[must_use]
pub fn output_path_name(drv_name: &str, output_name: &str) -> String {
    if output_name == "out" {
        drv_name.to_owned()
    } else {
        format!("{drv_name}-{output_name}")
    }
}

/// `StoreDirConfig::makeOutputPath` (`store-dir-config.cc:85`).
#[must_use]
pub fn make_output_path(
    store_dir: &str,
    output_name: &str,
    hash: &[u8; 32],
    drv_name: &str,
) -> String {
    make_store_path(
        store_dir,
        &format!("output:{output_name}"),
        &format!("sha256:{}", hex(hash)),
        &output_path_name(drv_name, output_name),
    )
}

/// cppnix's `ContentAddressMethod` (`libstore/content-address.hh`), the
/// `outputHashMode` attribute once parsed.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CaMethod {
    /// `flat`: the file's own bytes.
    Flat,
    /// `nar`, and `recursive` which is its back-compat spelling.
    NixArchive,
    /// `text`, behind cppnix's `dynamic-derivations` experimental feature.
    Text,
    /// `git`, behind cppnix's `git-hashing` experimental feature.
    Git,
}

impl CaMethod {
    /// `ContentAddressMethod::parse` (`content-address.cc:61`) together with
    /// the `recursive` back-compat spelling `handleHashMode` applies before it
    /// (`primops.cc:1581`). `None` is cppnix's "invalid value for
    /// 'outputHashMode'".
    #[must_use]
    pub fn parse(s: &str) -> Option<CaMethod> {
        match s {
            "flat" => Some(CaMethod::Flat),
            // "back compat, new name is nar"
            "recursive" | "nar" => Some(CaMethod::NixArchive),
            "text" => Some(CaMethod::Text),
            "git" => Some(CaMethod::Git),
            _ => None,
        }
    }

    /// `ContentAddressMethod::renderPrefix`. Flat is empty "for back compat",
    /// which is why the two `fixed:out:` payloads below are not symmetrical.
    #[must_use]
    pub fn render_prefix(self) -> &'static str {
        match self {
            CaMethod::Flat => "",
            CaMethod::NixArchive => "r:",
            CaMethod::Text => "text:",
            CaMethod::Git => "git:",
        }
    }

    /// `ContentAddress::printMethodAlgo` (`content-address.cc:221`): the third
    /// field of a fixed output in the ATerm, e.g. `r:sha256`.
    #[must_use]
    pub fn print_method_algo(self, algo: HashAlgo) -> String {
        format!("{}{}", self.render_prefix(), algo.name())
    }
}

/// `StoreDirConfig::makeFixedOutputPath` (`store-dir-config.cc:104`) for the
/// reference-free case, which is the only one `derivationStrict` produces
/// (`CAFixed::path` passes `ContentAddressWithReferences::withoutRefs`).
///
/// Two branches, and picking the wrong one gives a well-formed path that is
/// not cppnix's:
///
/// * sha256 or Blake3 **and** NAR ingestion is the `source` type, hashing the
///   declared hash directly -- the same shape `nix store add-path` produces,
///   which is why a recursive fetch and an added directory can land on the
///   same path;
/// * everything else hashes a `fixed:out:` payload *first* and feeds that
///   digest to `output:out`.
///
/// Note the payload's hash carries its `sha256:` prefix (`to_string(Base16,
/// true)`) while the ATerm's does not, and the modulo hash in
/// [`hash_derivation_modulo`] uses a third spelling again.
#[must_use]
pub fn make_fixed_output_path(
    store_dir: &str,
    name: &str,
    method: CaMethod,
    hash: &Hash,
) -> String {
    if method == CaMethod::NixArchive && matches!(hash.algo, HashAlgo::Blake3 | HashAlgo::Sha256) {
        return make_store_path(store_dir, "source", &hash.to_base16(true), name);
    }
    let payload = format!(
        "fixed:out:{}{}:",
        method.render_prefix(),
        hash.to_base16(true)
    );
    let digest = sha256(payload.as_bytes());
    make_store_path(
        store_dir,
        "output:out",
        &format!("sha256:{}", hex(&digest)),
        name,
    )
}

/// `makeFixedOutputPathFromCA` over a `TextInfo` (`store-dir-config.cc:134`):
/// the store path of a text blob with references, which is what
/// `builtins.toFile` and the `.drv` writer both land on.
///
/// The references are stuffed into the *type* string (`makeType`), so they
/// have to be in cppnix's `StorePathSet` order. Every path here shares the
/// store directory, so sorting the printed form agrees with sorting by base
/// name.
#[must_use]
pub fn make_text_path<'a>(
    store_dir: &str,
    name: &str,
    contents: &str,
    references: impl IntoIterator<Item = &'a str>,
) -> String {
    make_text_path_from_hash(store_dir, name, &sha256(contents.as_bytes()), references)
}

/// Construct a text object's path when its SHA-256 is already available.
/// The store batch uses the same digest in its content-address metadata.
#[must_use]
pub(crate) fn make_text_path_from_hash<'a>(
    store_dir: &str,
    name: &str,
    hash: &[u8; 32],
    references: impl IntoIterator<Item = &'a str>,
) -> String {
    let sorted: BTreeSet<&str> = references.into_iter().collect();
    let mut ty = String::from("text");
    for reference in &sorted {
        ty.push(':');
        ty.push_str(reference);
    }
    make_store_path(store_dir, &ty, &format!("sha256:{}", hex(hash)), name)
}

/// `infoForDerivation` / `computeStorePath` (`derivations.cc:109`): where the
/// `.drv` file itself lands.
///
/// This is the other path step 1 needs, and it is not the same computation as
/// [`make_output_path`]. A derivation is stored as text, so its path is
/// `makeFixedOutputPathFromCA` over a `TextInfo`, which reduces to
/// `makeStorePath` with the type string `text` followed by one `:` and the
/// full path of every reference, in store-path order.
///
/// The references are `inputSrcs` **plus every `inputDrvs` key**, and
/// deliberately not the outputs: an output can legitimately be missing, and a
/// reference would hold it through a collection. Getting that set wrong moves
/// the `.drv` path without moving any output path, which is the shape of
/// divergence rung C step 2 exists to catch.
///
/// `mask_outputs` is false here. The `.drv` on disk carries its real output
/// paths, and it is those bytes that are hashed.
#[must_use]
pub fn derivation_store_path(store_dir: &str, drv: &Derivation, drv_name: &str) -> String {
    let mut references: BTreeSet<&str> = drv.input_srcs.iter().map(String::as_str).collect();
    for input in &drv.input_drvs {
        references.insert(&input.drv_path);
    }
    let references: Vec<String> = references.into_iter().map(str::to_owned).collect();
    text_store_path(
        store_dir,
        &format!("{drv_name}.drv"),
        &unparse(drv, false),
        &references,
    )
}

/// `makeFixedOutputPathFromCA` over a `TextInfo`: where bytes stored as
/// `text` with these references land.
///
/// Its own function because two callers need the same rule and one of them is
/// not a derivation: `builtins.toFile` is the same computation, and so is the
/// answer an embedder gives back from a `.drv` write. A second spelling of
/// the type string is a second thing to keep in step, and the reference order
/// inside it is the part that is easy to get wrong.
///
/// `name` is the full store-object name, `.drv` suffix included where there
/// is one, because `toFile` has no suffix to append.
#[must_use]
pub fn text_store_path(
    store_dir: &str,
    name: &str,
    contents: &str,
    references: &[String],
) -> String {
    make_text_path(
        store_dir,
        name,
        contents,
        references.iter().map(String::as_str),
    )
}

/// cppnix's `DrvHash::Kind`. `Deferred` means the output paths cannot be
/// known yet, so nothing downstream may compute one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrvHashKind {
    Regular,
    Deferred,
}

/// cppnix's `DrvHash`: one hash per output name, plus whether they are usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrvHash {
    pub hashes: BTreeMap<String, [u8; 32]>,
    pub kind: DrvHashKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashError {
    /// An input derivation could not be read. Carries the path so a failure
    /// names the file rather than the derivation that happened to reach it.
    UnreadableInput { drv_path: String, why: String },
    /// cppnix's "no hash for output '%s' of derivation '%s'": an input names
    /// an output the input derivation does not have.
    NoHashForOutput { output: String, drv_name: String },
    /// cppnix's "can't mix derivation output types".
    MixedOutputTypes,
    /// cppnix's "must have at least one output".
    NoOutputs,
    /// An output whose four raw fields are none of the five kinds. Refused
    /// rather than assumed, for the reason `OutputKind::Unrecognised` exists.
    UnrecognisedOutput { output: String },
}

impl core::fmt::Display for HashError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HashError::UnreadableInput { drv_path, why } => {
                write!(f, "cannot read input derivation '{drv_path}': {why}")
            }
            HashError::NoHashForOutput { output, drv_name } => {
                write!(
                    f,
                    "no hash for output '{output}' of derivation '{drv_name}'"
                )
            }
            HashError::MixedOutputTypes => write!(f, "can't mix derivation output types"),
            HashError::NoOutputs => write!(f, "must have at least one output"),
            HashError::UnrecognisedOutput { output } => {
                write!(
                    f,
                    "output '{output}' matches none of the five derivation output kinds"
                )
            }
        }
    }
}

impl core::error::Error for HashError {}

type Result<T> = core::result::Result<T, HashError>;

/// Where an input derivation's bytes come from.
///
/// `hashDerivationModulo` recurses into every input, so it needs to read other
/// `.drv` files. In cppnix that is `store.readInvalidDerivation`; here it is
/// the caller's problem, for the same reason the evaluator's filesystem access
/// goes through `Host` -- a store is the embedder's, and a trait keeps a test
/// able to answer without one.
pub trait DrvSource {
    /// The name is `BasicDerivation::nameFromPath`'s: the store path's name
    /// part with `.drv` removed. The implementation has it already from the
    /// path, and recomputing it here would duplicate the parsing.
    fn read_drv(&self, drv_path: &str) -> core::result::Result<(Derivation, String), String>;
}

/// Which derivation type the outputs spell, reduced to the one bit
/// `hashDerivationModulo` branches on plus the resulting hash kind.
/// Mirrors `BasicDerivation::type()` (`derivations.cc:795`) and the `kind`
/// visit at `derivations.cc:915`.
fn classify(drv: &Derivation) -> Result<(bool, DrvHashKind)> {
    let mut fixed = false;
    let mut kind: Option<DrvHashKind> = None;
    let mut seen: Option<OutputKind> = None;
    for output in &drv.outputs {
        let k = output.kind();
        if k == OutputKind::Unrecognised {
            return Err(HashError::UnrecognisedOutput {
                output: output.name.clone(),
            });
        }
        // cppnix's `decide` refuses a mixture. Two `Deferred` outputs and two
        // `InputAddressed` outputs are the same `DerivationType`, so compare
        // the derivation type rather than the output kind.
        let as_type = match k {
            OutputKind::InputAddressed | OutputKind::Deferred => OutputKind::InputAddressed,
            other => other,
        };
        match seen {
            None => seen = Some(as_type),
            Some(prev) if prev == as_type => {}
            Some(_) => return Err(HashError::MixedOutputTypes),
        }
        match k {
            OutputKind::CaFixed => {
                fixed = true;
                kind = Some(DrvHashKind::Regular);
            }
            OutputKind::InputAddressed | OutputKind::Deferred => kind = Some(DrvHashKind::Regular),
            OutputKind::CaFloating | OutputKind::Impure => kind = Some(DrvHashKind::Deferred),
            OutputKind::Unrecognised => {}
        }
    }
    match kind {
        Some(k) => Ok((fixed, k)),
        None => Err(HashError::NoOutputs),
    }
}

/// `hashDerivationModulo` (`derivations.cc:893`).
///
/// Three things about this are easy to get wrong and each one moves every
/// downstream output path:
///
/// * **A fixed-output derivation does not go through `unparse` at all.** Each
///   output hashes `fixed:out:<methodAlgo>:<hash>:<path>` on its own, so that
///   a dependent cannot tell where a fixed output came from.
/// * **Input derivation paths are replaced by their own modulo hashes**, and
///   the resulting map is keyed on those hex hashes, so the list is re-sorted
///   into hash order rather than staying in path order.
/// * **The recursion passes `mask = false`.** Only the top-level call masks
///   (`derivations.cc:1304` passes `true`, `derivations.cc:872` passes
///   `false`), so an implementation that masks all the way down agrees with
///   cppnix on leaves and diverges on everything above them.
// `store_dir` is threaded through the recursion and read by nothing today.
// Removing it is the real fix and is a signature change to a tier 1 function
// with callers in `drvstrict` and the audit example, so it is not being made
// as a rider on a formatting pass. ENG-13021.
#[allow(clippy::only_used_in_recursion)]
pub fn hash_derivation_modulo(
    store_dir: &str,
    drv: &Derivation,
    drv_name: &str,
    mask_outputs: bool,
    source: &impl DrvSource,
    memo: &mut BTreeMap<String, DrvHash>,
) -> Result<DrvHash> {
    let (fixed, mut kind) = classify(drv)?;

    if fixed {
        let mut hashes = BTreeMap::new();
        for output in &drv.outputs {
            // The path is taken from the file rather than recomputed with
            // `makeFixedOutputPath`. For a derivation read off disk the two
            // are the same string by construction, and cppnix's own
            // `checkInvariants` likewise only compares the on-disk path
            // against the environment variable for this kind. A derivation
            // *constructed* by `derivationStrict` must compute it.
            let payload = format!(
                "fixed:out:{}:{}:{}",
                output.hash_algo, output.hash, output.path
            );
            hashes.insert(output.name.clone(), sha256(payload.as_bytes()));
        }
        return Ok(DrvHash {
            hashes,
            kind: DrvHashKind::Regular,
        });
    }

    let mut inputs2: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for input in &drv.input_drvs {
        let res = match memo.get(&input.drv_path) {
            Some(cached) => cached.clone(),
            None => {
                let (input_drv, input_name) =
                    source
                        .read_drv(&input.drv_path)
                        .map_err(|why| HashError::UnreadableInput {
                            drv_path: input.drv_path.clone(),
                            why,
                        })?;
                let h = hash_derivation_modulo(
                    store_dir,
                    &input_drv,
                    &input_name,
                    false,
                    source,
                    memo,
                )?;
                memo.insert(input.drv_path.clone(), h.clone());
                h
            }
        };
        if res.kind == DrvHashKind::Deferred {
            kind = DrvHashKind::Deferred;
        }
        for output_name in &input.outputs {
            let Some(h) = res.hashes.get(output_name) else {
                return Err(HashError::NoHashForOutput {
                    output: output_name.clone(),
                    drv_name: drv_name.to_owned(),
                });
            };
            inputs2
                .entry(hex(h))
                .or_default()
                .insert(output_name.clone());
        }
    }

    // cppnix passes `inputs2` as `actualInputs` and `unparse` writes it in the
    // slot `inputDrvs` would occupy. Substituting the list and reusing the one
    // writer keeps a single implementation of the ATerm grammar, which is the
    // property the store-wide round trip is evidence for.
    let mut hashed = drv.clone();
    hashed.input_drvs = inputs2
        .into_iter()
        .map(|(hash, outputs)| InputDrv {
            drv_path: hash,
            outputs: outputs.into_iter().collect(),
        })
        .collect();
    canonicalise(&mut hashed);
    let hash = sha256(unparse(&hashed, mask_outputs).as_bytes());

    let mut hashes = BTreeMap::new();
    for output in &drv.outputs {
        hashes.insert(output.name.clone(), hash);
    }
    Ok(DrvHash { hashes, kind })
}

/// What recomputing one derivation's output paths found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathCheck {
    /// Every input-addressed output's path is the one this recomputes.
    Agrees { outputs: usize },
    /// At least one differs. Carries the first, since a derivation whose
    /// hash is wrong has every output wrong and listing them all says the
    /// same thing many times.
    Differs {
        output: String,
        want: String,
        got: String,
    },
    /// Nothing to compare: the derivation is fixed-output, floating or
    /// impure, so cppnix does not compute an input-addressed path for it
    /// either. Reported rather than counted as agreement -- a run whose
    /// corpus is all fixed-output derivations proves nothing about
    /// `makeOutputPath` and must not be able to look like one that does.
    NotInputAddressed,
    /// The kind says no path can exist yet.
    Deferred,
}

/// cppnix's `Derivation::checkInvariants` (`derivations.cc:1398`), reimplemented
/// as a comparison rather than a throw.
///
/// `drv_name` is `BasicDerivation::nameFromPath`: the `.drv`'s name part with
/// the extension removed. It goes into the fingerprint, so passing the file
/// name including `.drv` produces a wrong path that looks like a hash bug.
pub fn check_output_paths(
    store_dir: &str,
    drv: &Derivation,
    drv_name: &str,
    source: &impl DrvSource,
    memo: &mut BTreeMap<String, DrvHash>,
) -> Result<PathCheck> {
    let modulo = hash_derivation_modulo(store_dir, drv, drv_name, true, source, memo)?;
    if modulo.kind == DrvHashKind::Deferred {
        return Ok(PathCheck::Deferred);
    }
    let mut compared = 0usize;
    // Through the helper rather than an inline filter, so "which outputs
    // `checkInvariants` looks at" has one implementation. It had two, and the
    // helper was the unused one -- written for the audit harness and never
    // called by it, so nothing could tell whether the two agreed (ENG-13020).
    for output in input_addressed_outputs(drv) {
        let Some(h) = modulo.hashes.get(&output.name) else {
            return Err(HashError::NoHashForOutput {
                output: output.name.clone(),
                drv_name: drv_name.to_owned(),
            });
        };
        let want = make_output_path(store_dir, &output.name, h, drv_name);
        if want != output.path {
            return Ok(PathCheck::Differs {
                output: output.name.clone(),
                want,
                got: output.path.clone(),
            });
        }
        compared += 1;
    }
    if compared == 0 {
        return Ok(PathCheck::NotInputAddressed);
    }
    Ok(PathCheck::Agrees { outputs: compared })
}

/// The parts `derivationStrict` has after it has forced the attribute set,
/// before any of them is a derivation.
///
/// Split out from the primop deliberately. Everything from here down is a pure
/// function of these strings, so it can be unit-tested and, more usefully,
/// checked against every derivation in a real store; what is left in the
/// primop is forcing values and coercing them, which is the part that needs a
/// VM. Contexts are already resolved into `input_srcs` and `input_drvs` by the
/// caller, because `ContextElem` is a value-level type and this module has no
/// business knowing about values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivationInputs {
    /// The `name` attribute, not the store path's name part. They differ for
    /// a non-default output.
    pub name: String,
    pub platform: String,
    pub builder: String,
    pub args: Vec<String>,
    /// Output names. cppnix defaults this to `["out"]` when the `outputs`
    /// attribute is absent; the caller has already applied that.
    pub output_names: Vec<String>,
    /// Every attribute that becomes an environment variable, already coerced
    /// to a string. Includes `name`, `system`, `builder` and the rest, since
    /// cppnix passes them all through to the builder. Must **not** contain an
    /// entry for an output name: those are filled in below, and an entry here
    /// would be overwritten rather than merged.
    pub env: BTreeMap<String, String>,
    pub input_srcs: BTreeSet<String>,
    /// Input derivations, each with the output names depended on.
    pub input_drvs: BTreeMap<String, BTreeSet<String>>,
}

/// A finished derivation and where everything it names lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltDerivation {
    pub drv: Derivation,
    /// Where the `.drv` itself goes. Under `readOnlyMode` this is the whole
    /// answer and nothing is written; otherwise it is what `addTextToStore`
    /// must agree with.
    pub drv_path: String,
    /// Output name to store path.
    pub outputs: BTreeMap<String, String>,
    /// The `.drv`'s bytes, so a caller that does write does not re-render.
    pub aterm: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// cppnix's "derivation names are not allowed to end in '.drv'".
    NameEndsInDrv {
        name: String,
    },
    /// cppnix's "required attribute 'name' missing" reaches the caller before
    /// this; an empty name here is the caller's bug.
    EmptyName,
    NoOutputs,
    /// A fixed-output derivation whose outputs are not exactly `{"out"}`.
    FixedOutputNotSingleOut,
    /// An output name the caller also supplied an environment variable for.
    /// Refused rather than silently overwritten: the two disagree about what
    /// the builder will see.
    OutputAlsoInEnv {
        output: String,
    },
    /// Propagated from the hashing pass.
    Hash(HashError),
}

impl core::fmt::Display for BuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BuildError::NameEndsInDrv { name } => {
                write!(
                    f,
                    "derivation names are not allowed to end in '.drv': {name}"
                )
            }
            BuildError::EmptyName => write!(f, "derivation has an empty name"),
            BuildError::NoOutputs => write!(f, "derivation must have at least one output"),
            BuildError::FixedOutputNotSingleOut => {
                write!(
                    f,
                    "multiple outputs are not supported in fixed-output derivations"
                )
            }
            BuildError::OutputAlsoInEnv { output } => {
                write!(
                    f,
                    "output '{output}' also has an environment variable of its own"
                )
            }
            BuildError::Hash(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for BuildError {}

/// Build an input-addressed derivation: cppnix's `derivationStrictInternal`
/// from the point where the attributes are forced, minus the content-addressed
/// and fixed-output branches.
///
/// The order of operations is cppnix's and is load-bearing at every step
/// (`primops.cc:1903` onwards, then `Derivation::fillInOutputPaths`):
///
/// 1. every output gets an **empty** environment variable and a `Deferred`
///    output entry, so that the set of output *names* is part of the hash
///    even though none of their paths is;
/// 2. `hashDerivationModulo(drv, mask = true)` over that;
/// 3. each output's path becomes `makeOutputPath(name, hash, drvName)` and
///    its environment variable is rewritten from `""` to that path;
/// 4. only then is the `.drv` rendered and its own path computed, over the
///    filled-in form.
///
/// Doing 4 before 3 produces a `.drv` path that hashes empty output variables,
/// which is self-consistent, reproducible, and not cppnix's.
pub fn build_input_addressed(
    store_dir: &str,
    inputs: &DerivationInputs,
    source: &impl DrvSource,
    memo: &mut BTreeMap<String, DrvHash>,
) -> core::result::Result<BuiltDerivation, BuildError> {
    if inputs.name.is_empty() {
        return Err(BuildError::EmptyName);
    }
    if inputs.name.ends_with(".drv") {
        return Err(BuildError::NameEndsInDrv {
            name: inputs.name.clone(),
        });
    }
    if inputs.output_names.is_empty() {
        return Err(BuildError::NoOutputs);
    }
    for output in &inputs.output_names {
        if inputs.env.contains_key(output) {
            return Err(BuildError::OutputAlsoInEnv {
                output: output.clone(),
            });
        }
    }

    let mut env: Vec<EnvVar> = inputs
        .env
        .iter()
        .map(|(name, value)| EnvVar {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();
    for output in &inputs.output_names {
        env.push(EnvVar {
            name: output.clone(),
            value: String::new(),
        });
    }

    let mut drv = Derivation {
        outputs: inputs
            .output_names
            .iter()
            .map(|name| Output {
                name: name.clone(),
                path: String::new(),
                hash_algo: String::new(),
                hash: String::new(),
            })
            .collect(),
        input_drvs: inputs
            .input_drvs
            .iter()
            .map(|(drv_path, outputs)| InputDrv {
                drv_path: drv_path.clone(),
                outputs: outputs.iter().cloned().collect(),
            })
            .collect(),
        input_srcs: inputs.input_srcs.iter().cloned().collect(),
        platform: inputs.platform.clone(),
        builder: inputs.builder.clone(),
        args: inputs.args.clone(),
        env,
    };
    canonicalise(&mut drv);

    let modulo = hash_derivation_modulo(store_dir, &drv, &inputs.name, true, source, memo)
        .map_err(BuildError::Hash)?;

    // A `Deferred` hash means an input somewhere below is floating
    // content-addressed, so no output path can be computed yet. cppnix's
    // `fillInOutputPaths` (`processDerivationOutputPaths`,
    // `derivations.cc:1330`) leaves the outputs `Deferred` and the output
    // environment variables empty in that case, and the value a consumer
    // sees for each output is a downstream placeholder rather than a path
    // (`mkOutputStringRaw`, `eval.cc:1059`, with no static output path).
    if modulo.kind == DrvHashKind::Deferred {
        let drv_path = derivation_store_path(store_dir, &drv, &inputs.name);
        let aterm = unparse(&drv, false);
        let outputs = inputs
            .output_names
            .iter()
            .map(|name| (name.clone(), downstream_placeholder(&drv_path, name)))
            .collect();
        return Ok(BuiltDerivation {
            drv,
            drv_path,
            outputs,
            aterm,
        });
    }

    let mut outputs = BTreeMap::new();
    for name in &inputs.output_names {
        let Some(h) = modulo.hashes.get(name) else {
            return Err(BuildError::Hash(HashError::NoHashForOutput {
                output: name.clone(),
                drv_name: inputs.name.clone(),
            }));
        };
        let path = make_output_path(store_dir, name, h, &inputs.name);
        for output in &mut drv.outputs {
            if output.name == *name {
                output.path.clone_from(&path);
            }
        }
        for var in &mut drv.env {
            if var.name == *name {
                var.value.clone_from(&path);
            }
        }
        outputs.insert(name.clone(), path);
    }

    let drv_path = derivation_store_path(store_dir, &drv, &inputs.name);
    let aterm = unparse(&drv, false);
    Ok(BuiltDerivation {
        drv,
        drv_path,
        outputs,
        aterm,
    })
}

/// Build a floating content-addressed derivation: cppnix's
/// `derivationStrictInternal` `else if (contentAddressed || isImpure)` branch
/// (`primops.cc:1878`), minus impure, which stays a named refusal.
///
/// No chicken and egg here either: no output path exists at all. Each output
/// becomes a `CAFloating` entry whose ATerm row is `(name, "", methodAlgo,
/// "")`, and its environment variable is `hashPlaceholder(name)` -- the
/// string the builder sees for `$out` until the build produces the real
/// path. The defaults are cppnix's: SHA-256 and NAR ingestion
/// (`primops.cc:1882`).
///
/// The `outputs` map a consumer reads holds downstream placeholders, which is
/// what `mkOutputStringRaw` renders for an output with no static path.
pub fn build_content_addressed(
    store_dir: &str,
    inputs: &DerivationInputs,
    method: CaMethod,
    algo: crate::nixhash::HashAlgo,
) -> core::result::Result<BuiltDerivation, BuildError> {
    if inputs.name.is_empty() {
        return Err(BuildError::EmptyName);
    }
    if inputs.name.ends_with(".drv") {
        return Err(BuildError::NameEndsInDrv {
            name: inputs.name.clone(),
        });
    }
    if inputs.output_names.is_empty() {
        return Err(BuildError::NoOutputs);
    }
    for output in &inputs.output_names {
        if inputs.env.contains_key(output) {
            return Err(BuildError::OutputAlsoInEnv {
                output: output.clone(),
            });
        }
    }

    let mut env: Vec<EnvVar> = inputs
        .env
        .iter()
        .map(|(name, value)| EnvVar {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();
    for output in &inputs.output_names {
        env.push(EnvVar {
            name: output.clone(),
            value: hash_placeholder(output),
        });
    }

    let method_algo = method.print_method_algo(algo);
    let mut drv = Derivation {
        outputs: inputs
            .output_names
            .iter()
            .map(|name| Output {
                name: name.clone(),
                path: String::new(),
                hash_algo: method_algo.clone(),
                hash: String::new(),
            })
            .collect(),
        input_drvs: inputs
            .input_drvs
            .iter()
            .map(|(drv_path, outputs)| InputDrv {
                drv_path: drv_path.clone(),
                outputs: outputs.iter().cloned().collect(),
            })
            .collect(),
        input_srcs: inputs.input_srcs.iter().cloned().collect(),
        platform: inputs.platform.clone(),
        builder: inputs.builder.clone(),
        args: inputs.args.clone(),
        env,
    };
    canonicalise(&mut drv);

    let drv_path = derivation_store_path(store_dir, &drv, &inputs.name);
    let aterm = unparse(&drv, false);
    let outputs = inputs
        .output_names
        .iter()
        .map(|name| (name.clone(), downstream_placeholder(&drv_path, name)))
        .collect();
    Ok(BuiltDerivation {
        drv,
        drv_path,
        outputs,
        aterm,
    })
}

/// Build a fixed-output derivation: cppnix's `derivationStrictInternal`
/// `if (outputHash)` branch (`primops.cc:1853`).
///
/// Much shorter than [`build_input_addressed`] because there is no chicken and
/// egg: the output path comes from the declared hash alone, so nothing has to
/// be hashed with the outputs masked and then filled back in. cppnix assigns
/// `drv.env["out"]` the finished path directly.
///
/// The three spellings of the hash in play here are all different and all
/// load-bearing:
///
/// * the path payload takes `to_string(Base16, /*includeAlgo=*/true)`;
/// * the ATerm's third field is `printMethodAlgo()` (`r:sha256`) and its
///   fourth is `to_string(Base16, false)`, with no algorithm;
/// * [`hash_derivation_modulo`] joins the two with `:` and appends the path.
pub fn build_fixed_output(
    store_dir: &str,
    inputs: &DerivationInputs,
    method: CaMethod,
    hash: &Hash,
) -> core::result::Result<BuiltDerivation, BuildError> {
    if inputs.name.is_empty() {
        return Err(BuildError::EmptyName);
    }
    if inputs.name.ends_with(".drv") {
        return Err(BuildError::NameEndsInDrv {
            name: inputs.name.clone(),
        });
    }
    // cppnix checks this in `derivationStrictInternal` (`primops.cc:1858`),
    // before its equivalent of this function. Here it is the only enforcement
    // point rather than a second one: `finish_fixed_output` used to repeat it
    // and the copy was unobservable, so it went (ENG-13020). The message
    // `BuildError::FixedOutputNotSingleOut` renders is cppnix's.
    if inputs.output_names.len() != 1
        || inputs.output_names.first().map(String::as_str) != Some("out")
    {
        return Err(BuildError::FixedOutputNotSingleOut);
    }
    if inputs.env.contains_key("out") {
        return Err(BuildError::OutputAlsoInEnv {
            output: "out".to_owned(),
        });
    }

    let path = make_fixed_output_path(
        store_dir,
        &output_path_name(&inputs.name, "out"),
        method,
        hash,
    );

    let mut env: Vec<EnvVar> = inputs
        .env
        .iter()
        .map(|(name, value)| EnvVar {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();
    env.push(EnvVar {
        name: "out".to_owned(),
        value: path.clone(),
    });

    let mut drv = Derivation {
        outputs: vec![Output {
            name: "out".to_owned(),
            path: path.clone(),
            hash_algo: method.print_method_algo(hash.algo),
            hash: hash.to_base16(false),
        }],
        input_drvs: inputs
            .input_drvs
            .iter()
            .map(|(drv_path, outputs)| InputDrv {
                drv_path: drv_path.clone(),
                outputs: outputs.iter().cloned().collect(),
            })
            .collect(),
        input_srcs: inputs.input_srcs.iter().cloned().collect(),
        platform: inputs.platform.clone(),
        builder: inputs.builder.clone(),
        args: inputs.args.clone(),
        env,
    };
    canonicalise(&mut drv);

    let drv_path = derivation_store_path(store_dir, &drv, &inputs.name);
    let aterm = unparse(&drv, false);
    let mut outputs = BTreeMap::new();
    outputs.insert("out".to_owned(), path);
    Ok(BuiltDerivation {
        drv,
        drv_path,
        outputs,
        aterm,
    })
}

/// Take a derivation apart into the [`DerivationInputs`] that would rebuild
/// it, so a real `.drv` can be used as a test case for the builder.
///
/// Only meaningful for an input-addressed derivation, and returns `None`
/// otherwise: for any other kind the output paths are not a function of these
/// inputs, so a round trip through the builder would be asserting the wrong
/// thing.
#[must_use]
pub fn inputs_of(drv: &Derivation, drv_name: &str) -> Option<DerivationInputs> {
    if drv.outputs.is_empty()
        || drv
            .outputs
            .iter()
            .any(|o| o.kind() != OutputKind::InputAddressed)
    {
        return None;
    }
    let output_names: Vec<String> = drv.outputs.iter().map(|o| o.name.clone()).collect();
    let env = drv
        .env
        .iter()
        // The output variables hold the paths the builder is about to
        // recompute. Feeding them back in would be assuming the answer.
        .filter(|var| !output_names.contains(&var.name))
        .map(|var| (var.name.clone(), var.value.clone()))
        .collect();
    Some(DerivationInputs {
        name: drv_name.to_owned(),
        platform: drv.platform.clone(),
        builder: drv.builder.clone(),
        args: drv.args.clone(),
        output_names,
        env,
        input_srcs: drv.input_srcs.iter().cloned().collect(),
        input_drvs: drv
            .input_drvs
            .iter()
            .map(|i| (i.drv_path.clone(), i.outputs.iter().cloned().collect()))
            .collect(),
    })
}

/// `BasicDerivation::nameFromPath` (`derivations.cc:984`).
///
/// Returns `None` for a path that is not a `.drv` or carries no `<hash>-`
/// prefix, rather than guessing: the name goes straight into the store-path
/// fingerprint, so a wrong one is a wrong path and never an error.
#[must_use]
pub fn name_from_drv_path(drv_path: &str) -> Option<&str> {
    let base = drv_path.rsplit('/').next()?;
    let without_ext = base.strip_suffix(".drv")?;
    // A store path's name part starts after the 32-character base-32 hash and
    // the hyphen that follows it.
    let (hash, name) = without_ext.split_at_checked(32)?;
    if !hash.bytes().all(|b| NIX32_CHARS.contains(&b)) {
        return None;
    }
    name.strip_prefix('-')
}

/// The derivation's outputs as cppnix's `checkInvariants` sees them: the
/// input-addressed ones, which are the only kind whose path this module can
/// recompute.
///
/// Used by [`check_output_paths`] and by the audit harness in
/// `examples/drv-outpath.rs`, which is the point -- the harness scores a store
/// against the same rule the check applies, rather than against its own copy
/// of it.
#[must_use]
pub fn input_addressed_outputs(drv: &Derivation) -> Vec<&Output> {
    drv.outputs
        .iter()
        .filter(|o| o.kind() == OutputKind::InputAddressed)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::hash_placeholder;

    /// cppnix's own bytes, from `nix-instantiate --eval -E 'builtins.placeholder
    /// "out"'` under `eval-backend = cpp` on dev-compute-4. Three names rather
    /// than one because the encoder is the part most likely to be wrong and a
    /// single sample can agree by luck about padding.
    ///
    /// Placeholders feed the `.drv` bytes of a content-addressed build, so
    /// this is tier 1: a wrong string here is a wrong output path.
    #[test]
    fn placeholders_are_cppnixs() {
        assert_eq!(
            hash_placeholder("out"),
            "/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9"
        );
        assert_eq!(
            hash_placeholder("dev"),
            "/02qcpld1y6xhs5gz9bchpxaw0xdhmsp5dv88lh25r2ss44kh8dxz"
        );
        assert_eq!(
            hash_placeholder("lib"),
            "/0sra2y18lr3h6j58qjm0w46yv36h1wjmilb09n8aimdpivdymscx"
        );
    }

    use super::{
        BuildError, CaMethod, DerivationInputs, DrvHash, DrvHashKind, DrvSource, PathCheck,
        build_fixed_output, build_input_addressed, check_output_paths, compress_hash,
        hash_derivation_modulo, inputs_of, make_output_path, make_store_path, name_from_drv_path,
        nix32_encode, output_path_name,
    };
    use crate::drv::Derivation;
    use std::collections::{BTreeMap, BTreeSet};

    /// A store with nothing in it. Every test here builds a derivation with no
    /// inputs, so any read is a bug in the test rather than a missing fixture,
    /// and saying so beats returning an empty derivation that would quietly
    /// hash to something.
    struct NoInputs;

    impl DrvSource for NoInputs {
        fn read_drv(&self, drv_path: &str) -> Result<(Derivation, String), String> {
            Err(format!(
                "this test has no store; nothing should have read '{drv_path}'"
            ))
        }
    }

    fn simple(name: &str, outputs: &[&str]) -> DerivationInputs {
        DerivationInputs {
            name: name.to_owned(),
            platform: "x86_64-linux".to_owned(),
            builder: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "true".to_owned()],
            output_names: outputs.iter().map(|s| (*s).to_owned()).collect(),
            env: [
                ("builder".to_owned(), "/bin/sh".to_owned()),
                ("name".to_owned(), name.to_owned()),
                ("system".to_owned(), "x86_64-linux".to_owned()),
            ]
            .into_iter()
            .collect(),
            input_srcs: BTreeSet::new(),
            input_drvs: BTreeMap::new(),
        }
    }

    /// The alphabet and the backwards window are both easy to get subtly
    /// wrong, so this pins an encoding whose expected value comes from
    /// cppnix's own algorithm rather than from another base32 implementation.
    #[test]
    fn nix32_omits_eout_and_reads_five_bit_windows_from_the_end() {
        assert_eq!(nix32_encode(&[]), "");
        // One byte is two characters: low five bits, then the top three.
        assert_eq!(nix32_encode(&[0x00]), "00");
        assert_eq!(nix32_encode(&[0x1f]), "0z");
        assert_eq!(nix32_encode(&[0xff]), "7z");
        // No output character may be one of the four cppnix drops.
        let s = nix32_encode(&(0u8..=255).collect::<Vec<_>>());
        assert!(
            !s.contains(['e', 'o', 'u', 't']),
            "forbidden character in {s}"
        );
    }

    /// A fold, not a truncation: byte 20 lands back on byte 0.
    #[test]
    fn compress_hash_xors_wrapping_rather_than_truncating() {
        // Byte 0 is 0x0f and byte 20 is 0xf0; the fold lands them both on
        // output byte 0. Built by index rather than mutated, because
        // `indexing_slicing` is denied and a `get_mut` dance is how the first
        // version of this test silently asserted nothing.
        let input: Vec<u8> = (0u8..32)
            .map(|i| match i {
                0 => 0x0f,
                20 => 0xf0,
                _ => 0,
            })
            .collect();
        let out = compress_hash(&input, 20);
        assert_eq!(out.len(), 20);
        // Truncation would have given 0x0f here; the fold gives 0x0f ^ 0xf0.
        assert_eq!(out.first().copied(), Some(0xff));
    }

    // ---- check_output_paths ---------------------------------------------
    //
    // `check_output_paths` is this module's oracle comparison: it is the
    // reimplementation of cppnix's `Derivation::checkInvariants`, and it is
    // what `examples/drv-outpath.rs` uses to score a whole real store. Until
    // these tests it had no unit test at all, because that example is a
    // manual harness and `cargo test` never runs it -- so every mutation of
    // its body survived, including inverting the comparison that decides
    // whether a recomputed path matches the one on disk (ENG-13020).
    //
    // The four cases below are its four outcomes. A harness that scores a
    // store is worthless if the scorer always says "agrees", and worse than
    // worthless if it always says "nothing to compare", which is the outcome
    // an empty count produces.

    /// A derivation built by this module agrees with itself, and the count is
    /// the number of outputs actually compared.
    ///
    /// `Agrees { outputs: 1 }` and not just "is an `Agrees`": the count is
    /// what stops a run over a store from reporting success having compared
    /// nothing, and it is a separate assertion because `outputs: 0` is
    /// reachable by three different mistakes in the loop above.
    #[test]
    fn a_built_derivation_agrees_with_its_own_recomputed_paths() {
        let mut memo = BTreeMap::new();
        let Ok(built) =
            build_input_addressed("/nix/store", &simple("x", &["out"]), &NoInputs, &mut memo)
        else {
            unreachable!("the fixture must build");
        };
        let mut memo = BTreeMap::new();
        let got = check_output_paths("/nix/store", &built.drv, "x", &NoInputs, &mut memo);
        assert_eq!(got, Ok(PathCheck::Agrees { outputs: 1 }));
    }

    /// Every input-addressed output is compared, not just the first.
    ///
    /// Two outputs must count two. A loop that compares one and stops, or that
    /// fails to increment, still returns `Agrees` and still looks right.
    #[test]
    fn every_input_addressed_output_is_compared_and_counted() {
        let mut memo = BTreeMap::new();
        let Ok(built) = build_input_addressed(
            "/nix/store",
            &simple("x", &["out", "dev"]),
            &NoInputs,
            &mut memo,
        ) else {
            unreachable!("the fixture must build");
        };
        let mut memo = BTreeMap::new();
        let got = check_output_paths("/nix/store", &built.drv, "x", &NoInputs, &mut memo);
        assert_eq!(got, Ok(PathCheck::Agrees { outputs: 2 }));
    }

    /// A path that is not the one this recomputes is reported, with both
    /// sides.
    ///
    /// The direction that matters: `checkInvariants` exists to catch a wrong
    /// output path, so a comparison that cannot fail is the whole function
    /// broken. `want` and `got` are asserted apart because a report that
    /// swapped them would read as a passing check of the other derivation.
    #[test]
    fn an_output_path_that_is_not_the_recomputed_one_is_reported() {
        let mut memo = BTreeMap::new();
        let Ok(built) =
            build_input_addressed("/nix/store", &simple("x", &["out"]), &NoInputs, &mut memo)
        else {
            unreachable!("the fixture must build");
        };
        let correct = built.outputs.get("out").cloned().unwrap_or_default();

        let mut damaged = built.drv.clone();
        let Some(output) = damaged.outputs.first_mut() else {
            unreachable!("the fixture has one output");
        };
        output.path = "/nix/store/00000000000000000000000000000000-x".to_owned();
        let wrong = output.path.clone();

        let mut memo = BTreeMap::new();
        let got = check_output_paths("/nix/store", &damaged, "x", &NoInputs, &mut memo);
        assert_eq!(
            got,
            Ok(PathCheck::Differs {
                output: "out".to_owned(),
                want: correct,
                got: wrong,
            })
        );
    }

    /// A fixed-output derivation has no input-addressed output, so there is
    /// nothing to compare and that is its own answer.
    ///
    /// Reported rather than counted as agreement, for the reason `PathCheck`
    /// gives: a corpus that is all fixed-output derivations proves nothing
    /// about `makeOutputPath` and must not be able to look like one that does.
    #[test]
    fn a_fixed_output_derivation_has_nothing_to_compare() {
        let hash = crate::nixhash::Hash {
            algo: crate::nixhash::HashAlgo::Sha256,
            bytes: vec![0u8; 32],
        };
        let Ok(built) =
            build_fixed_output("/nix/store", &simple("x", &["out"]), CaMethod::Flat, &hash)
        else {
            unreachable!("the fixture must build");
        };
        let mut memo = BTreeMap::new();
        let got = check_output_paths("/nix/store", &built.drv, "x", &NoInputs, &mut memo);
        assert_eq!(got, Ok(PathCheck::NotInputAddressed));
    }

    /// A floating input defers the whole derivation.
    ///
    /// `hash_derivation_modulo` walks the input derivations and takes the
    /// deferred-ness of any one of them: a derivation depending on an output
    /// whose path is not yet known cannot have its own path computed either.
    /// Without this branch a build against a content-addressed input would
    /// get an input-addressed path computed from a hash cppnix would have
    /// refused to use.
    ///
    /// The input is `CaFloating` -- `(name, "", methodAlgo, "")` -- and not
    /// `Deferred`. That distinction is the reason this test exists in this
    /// shape: `classify` maps a `Deferred` *output* to `DrvHashKind::Regular`
    /// and only `CaFloating` and `Impure` to `Deferred`, so a fixture built
    /// from the name alone tests the opposite branch and passes for the wrong
    /// reason.
    #[test]
    fn a_floating_input_defers_the_whole_derivation() {
        const DEP: &str = "/nix/store/00000000000000000000000000000000-dep.drv";

        struct FloatingInput;

        impl DrvSource for FloatingInput {
            fn read_drv(&self, _drv_path: &str) -> Result<(Derivation, String), String> {
                Ok((
                    Derivation {
                        outputs: vec![crate::drv::Output {
                            name: "out".to_owned(),
                            path: String::new(),
                            hash_algo: "r:sha256".to_owned(),
                            hash: String::new(),
                        }],
                        input_drvs: Vec::new(),
                        input_srcs: Vec::new(),
                        platform: "x86_64-linux".to_owned(),
                        builder: "/bin/sh".to_owned(),
                        args: Vec::new(),
                        env: Vec::new(),
                    },
                    "dep".to_owned(),
                ))
            }
        }

        // The dependent is an ordinary input-addressed derivation. Its own
        // outputs say `Regular`; the input is what moves it.
        let drv = Derivation {
            outputs: vec![crate::drv::Output {
                name: "out".to_owned(),
                path: "/nix/store/00000000000000000000000000000000-x".to_owned(),
                hash_algo: String::new(),
                hash: String::new(),
            }],
            input_drvs: vec![crate::drv::InputDrv {
                drv_path: DEP.to_owned(),
                outputs: vec!["out".to_owned()],
            }],
            input_srcs: Vec::new(),
            platform: "x86_64-linux".to_owned(),
            builder: "/bin/sh".to_owned(),
            args: Vec::new(),
            env: Vec::new(),
        };

        let mut memo = BTreeMap::new();
        let got = hash_derivation_modulo("/nix/store", &drv, "x", true, &FloatingInput, &mut memo);
        assert_eq!(got.map(|h| h.kind), Ok(DrvHashKind::Deferred));

        // And the check reports it rather than comparing the on-disk path
        // against one it could not have computed.
        let mut memo = BTreeMap::new();
        let got = check_output_paths("/nix/store", &drv, "x", &FloatingInput, &mut memo);
        assert_eq!(got, Ok(PathCheck::Deferred));
    }

    /// Every `HashError` says what went wrong and names the thing it went
    /// wrong on.
    ///
    /// The same gap `drv::tests::every_parse_error_renders_what_it_carries`
    /// covers for the ATerm parser: a `Display` returning `Ok(())` without
    /// writing satisfies every assertion made on the error *value* and puts a
    /// blank line where the message should be. Two of these name a store path
    /// or an output, which is the part a reader needs.
    #[test]
    fn every_hash_error_renders_what_it_carries() {
        let unreadable = super::HashError::UnreadableInput {
            drv_path: "/nix/store/aaa-x.drv".to_owned(),
            why: "no such file".to_owned(),
        }
        .to_string();
        assert!(unreadable.contains("/nix/store/aaa-x.drv"), "{unreadable}");
        assert!(unreadable.contains("no such file"), "{unreadable}");

        let no_hash = super::HashError::NoHashForOutput {
            output: "dev".to_owned(),
            drv_name: "x".to_owned(),
        }
        .to_string();
        assert!(
            no_hash.contains("dev") && no_hash.contains("x"),
            "{no_hash}"
        );

        let unrecognised = super::HashError::UnrecognisedOutput {
            output: "weird".to_owned(),
        }
        .to_string();
        assert!(unrecognised.contains("weird"), "{unrecognised}");

        // The two unit variants carry nothing, so what they have to be is
        // distinct and non-empty.
        let mixed = super::HashError::MixedOutputTypes.to_string();
        let none = super::HashError::NoOutputs.to_string();
        assert!(!mixed.is_empty() && !none.is_empty());
        assert_ne!(mixed, none);
        assert_ne!(mixed, unrecognised);
    }

    #[test]
    fn only_a_non_default_output_gets_its_name_appended() {
        assert_eq!(output_path_name("hello-2.12.2", "out"), "hello-2.12.2");
        assert_eq!(output_path_name("hello-2.12.2", "dev"), "hello-2.12.2-dev");
    }

    /// The digest is over `<type>:<hash>:<storeDir>:<name>`, so changing any
    /// of the four moves the path. Pinning the shape rather than a literal
    /// keeps this a test of the fingerprint's composition; agreement with
    /// cppnix's actual values is what the store-wide run establishes.
    #[test]
    fn every_component_of_the_fingerprint_moves_the_path() {
        let base = make_store_path("/nix/store", "output:out", "sha256:00", "x");
        assert!(base.starts_with("/nix/store/"));
        assert!(base.ends_with("-x"));
        assert_ne!(
            base,
            make_store_path("/nix/store", "output:dev", "sha256:00", "x")
        );
        assert_ne!(
            base,
            make_store_path("/nix/store", "output:out", "sha256:01", "x")
        );
        assert_ne!(
            base,
            make_store_path("/other/store", "output:out", "sha256:00", "x")
        );
        assert_ne!(
            base,
            make_store_path("/nix/store", "output:out", "sha256:00", "y")
        );
        // The hash part is 20 compressed bytes in base 32, which is 32 chars.
        assert_eq!(base.len(), "/nix/store/".len() + 32 + "-x".len());
    }

    #[test]
    fn a_non_default_output_path_is_named_after_the_output() {
        let out = make_output_path("/nix/store", "out", &[0u8; 32], "hello");
        let dev = make_output_path("/nix/store", "dev", &[0u8; 32], "hello");
        assert!(out.ends_with("-hello"));
        assert!(dev.ends_with("-hello-dev"));
    }

    /// A derivation built from its parts comes back out as the same parts,
    /// and both paths are filled in. The store-wide `drv-rebuild` run is the
    /// real evidence; this pins the shape so a failure there has somewhere
    /// small to be reproduced.
    #[test]
    fn a_built_derivation_carries_its_output_paths_in_both_places() {
        let mut memo = BTreeMap::new();
        let built =
            build_input_addressed("/nix/store", &simple("x", &["out"]), &NoInputs, &mut memo);
        let Ok(built) = built else {
            return assert_eq!(format!("{built:?}"), "Ok(..)");
        };
        let out = built.outputs.get("out").cloned().unwrap_or_default();
        assert!(
            out.starts_with("/nix/store/") && out.ends_with("-x"),
            "{out}"
        );
        assert!(built.drv_path.ends_with("-x.drv"), "{}", built.drv_path);
        // The output path has to be in the outputs list and in the
        // environment variable named after the output; cppnix's
        // `checkInvariants` compares them and they are written in two places.
        assert_eq!(
            built.drv.outputs.first().map(|o| o.path.clone()),
            Some(out.clone())
        );
        assert_eq!(
            built
                .drv
                .env
                .iter()
                .find(|v| v.name == "out")
                .map(|v| v.value.clone()),
            Some(out)
        );
        assert_eq!(built.aterm, crate::drv::unparse(&built.drv, false));
        assert!(
            crate::drv::is_canonical(&built.drv),
            "builder produced an unsorted derivation"
        );
    }

    /// The set of output *names* is part of the hash even though none of
    /// their paths is, which is the entire reason the outputs are given empty
    /// environment variables before hashing. Two derivations differing only in
    /// their output names must not collide.
    #[test]
    fn adding_an_output_name_moves_every_path() {
        let mut memo = BTreeMap::new();
        let one = build_input_addressed("/nix/store", &simple("x", &["out"]), &NoInputs, &mut memo);
        let two = build_input_addressed(
            "/nix/store",
            &simple("x", &["out", "dev"]),
            &NoInputs,
            &mut memo,
        );
        let (Ok(one), Ok(two)) = (one, two) else {
            return assert_eq!("both should build", "");
        };
        assert_ne!(one.outputs.get("out"), two.outputs.get("out"));
        assert_ne!(one.drv_path, two.drv_path);
        assert_eq!(two.outputs.len(), 2);
    }

    /// Refused rather than silently overwritten: an environment variable
    /// named after an output and the output path itself are two answers to
    /// one question, and picking one quietly is how a derivation ends up
    /// telling its builder something different from what the store says.
    #[test]
    fn an_env_var_named_after_an_output_is_refused() {
        let mut inputs = simple("x", &["out"]);
        inputs
            .env
            .insert("out".to_owned(), "/nix/store/whatever".to_owned());
        let mut memo: BTreeMap<String, DrvHash> = BTreeMap::new();
        assert_eq!(
            build_input_addressed("/nix/store", &inputs, &NoInputs, &mut memo),
            Err(BuildError::OutputAlsoInEnv {
                output: "out".to_owned()
            })
        );
    }

    #[test]
    fn a_name_ending_in_drv_is_refused() {
        let mut memo: BTreeMap<String, DrvHash> = BTreeMap::new();
        assert_eq!(
            build_input_addressed(
                "/nix/store",
                &simple("x.drv", &["out"]),
                &NoInputs,
                &mut memo
            ),
            Err(BuildError::NameEndsInDrv {
                name: "x.drv".to_owned()
            })
        );
    }

    /// `inputs_of` must drop the output variables, or feeding a derivation
    /// back through the builder would be handing it the answer.
    #[test]
    fn taking_a_derivation_apart_drops_the_output_variables() {
        let mut memo = BTreeMap::new();
        let Ok(built) =
            build_input_addressed("/nix/store", &simple("x", &["out"]), &NoInputs, &mut memo)
        else {
            return assert_eq!("should build", "");
        };
        let Some(back) = inputs_of(&built.drv, "x") else {
            return assert_eq!("input-addressed, so should come apart", "");
        };
        assert!(
            !back.env.contains_key("out"),
            "output variable survived: {:?}",
            back.env
        );
        assert_eq!(back, simple("x", &["out"]));
    }

    /// The name feeds the fingerprint, so anything this cannot parse has to
    /// be refused rather than guessed at.
    #[test]
    fn a_derivation_name_comes_from_the_path_and_is_refused_when_it_cannot() {
        assert_eq!(
            name_from_drv_path("/nix/store/00000000000000000000000000000000-hello-2.12.2.drv"),
            Some("hello-2.12.2")
        );
        assert_eq!(
            name_from_drv_path("/nix/store/00000000000000000000000000000000-hello.drv"),
            Some("hello")
        );
        // Not a derivation.
        assert_eq!(
            name_from_drv_path("/nix/store/00000000000000000000000000000000-hello"),
            None
        );
        // 'e' is not in the base-32 alphabet, so this hash part is not one.
        assert_eq!(
            name_from_drv_path("/nix/store/e0000000000000000000000000000000-hello.drv"),
            None
        );
        // Too short to carry a hash.
        assert_eq!(name_from_drv_path("/nix/store/short-hello.drv"), None);
    }
}

#[cfg(test)]
mod fixed_output_tests {
    use super::*;
    use crate::nixhash::parse_any;

    /// The bytes cpp nix 2.34.7 answered for `(import <nixpkgs> {}).hello.src`
    /// at nixpkgs `llgwlxshmy0ifvxh7f8wq53vk5x7vd13`, which is a `fetchurl`:
    /// an SRI sha256 with `outputHashMode = "flat"` and a null
    /// `outputHashAlgo`. Flat, so it takes the `fixed:out:` branch and not the
    /// `source` one.
    #[test]
    fn hello_src_lands_on_the_path_cpp_computes() {
        let hash = parse_any(
            "sha256-DV9gFUOC/uELEUocNOeF2LH0kgc64tOm97FHaHs2aqA=",
            None,
            false,
        );
        // Rendered rather than unwrapped: the workspace denies `panic` and
        // `unwrap`, tests included, so a test says what happened and compares.
        let got = match &hash {
            Ok(h) if h.algo == HashAlgo::Sha256 => {
                make_fixed_output_path("/nix/store", "hello-2.12.3.tar.gz", CaMethod::Flat, h)
            }
            other => format!("{other:?}"),
        };
        assert_eq!(
            got,
            "/nix/store/wj7phsmi7ncidl8k00p489krqss7n9sd-hello-2.12.3.tar.gz"
        );
    }
}
