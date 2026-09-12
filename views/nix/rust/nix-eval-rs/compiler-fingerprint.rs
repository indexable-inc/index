// How the compiler fingerprint is computed, in one place.
//
// `include!`d by `build.rs`, which stamps the result into the binary as
// `IXE_COMPILER_FINGERPRINT`, and by the tests in `modcache` that recompute it
// from the same tree and refuse to agree with a constant. Sharing the source
// is what makes those tests a check on the build script rather than a second
// implementation that can drift from it.
//
// Not a module of the crate: it has to be reachable from `build.rs`, which is
// compiled on its own.

/// The non-Rust files the binary embeds, as `(logical name, path relative to
/// the crate root)`.
///
/// One list, `include!`d by both the build script and the test that recomputes
/// the fingerprint, so the two cannot come to disagree about what the input
/// set is. ENG-13010.
const EMBEDDED_FILES: &[(&str, &str)] = &[
    // cppnix's own `derivation.nix`, the body of the `derivation` global.
    // Shared with the C++ tree rather than copied so the two evaluators cannot
    // drift; see `vm.rs`'s `DERIVATION_INTERNAL`.
    ("derivation.nix", "../../src/libexpr/primops/derivation.nix"),
];

/// One thing the compile cache's key covers.
///
/// `path` is `None` for an input that is not a file -- the feature set -- and
/// is what `build.rs` turns into its `rerun-if-changed` lines, so the set
/// cargo watches is derived from the set that is hashed instead of being a
/// hand-written list beside it that can fall out of step.
struct FingerprintInput {
    /// Stable across machines: never an absolute path.
    name: String,
    path: Option<std::path::PathBuf>,
    bytes: Vec<u8>,
}

/// Everything that can change what a compiled `Module` means, as a named list.
///
/// Returned as a list rather than hashed on the spot because a hash cannot be
/// asked what it read. `Cargo.lock`, the feature set and the embedded
/// `derivation.nix` each sat outside this hash for as long as they did
/// precisely because the only test recomputed it
/// through this same function, which agrees with itself whatever it omits; a
/// named list is something a test can check the contents of, and
/// `the_fingerprint_input_set_names_everything_that_can_change_the_compiler`
/// does.
///
/// What is in it and why:
///
/// - Every `.rs` file under `src`, recursively, and not only `compile.rs`,
///   because "the compiler" is more than the compiler: `Op::Builtin` and
///   `Op::CallBuiltin` carry indices into the builtin table, and
///   `compile_select` folds `builtins.<name>` against that same table, so a
///   module compiled before the table moved decodes into ops that now name
///   different builtins. Recursively, so a module moved into a subdirectory
///   does not silently leave the hash.
/// - Every file in [`EMBEDDED_FILES`]. `vm.rs` `include_str!`s cppnix's
///   `derivation.nix` from three levels above the crate, and that file is the
///   body of the `derivation` global: editing it changes what expressions
///   evaluate to and moved neither key -- not the compile-cache request, which
///   folds this fingerprint in, and not `EvalId`, whose module half comes from
///   compiling the *user's* source. So a store written before the edit
///   answered questions after it (ENG-13010). A file is no less carried into
///   an answer for being a `.nix` rather than a `.rs`.
/// - `Cargo.lock`, because the compiler is not only this crate: `regex`,
///   `serde_json` and `toml` decide what `builtins.match`, `fromJSON` and
///   `fromTOML` answer, and a dependency bump changes those answers without
///   touching a byte of `src`.
/// - `Cargo.toml`, because it is where a feature that gates code is declared
///   and where a dependency requirement is written.
/// - The enabled feature set, because two builds of identical sources with
///   different features are different compilers.
///
/// Over-invalidating (a comment in `print.rs` mints a new fingerprint) is the
/// safe direction: a compile cache that occasionally misses is a cache, one
/// that occasionally hits wrongly is a correctness bug.
fn compiler_fingerprint_inputs(
    crate_root: &std::path::Path,
    features: &[String],
) -> Result<Vec<FingerprintInput>, String> {
    let src_dir = crate_root.join("src");
    let mut sources = Vec::new();
    collect_rust_sources(&src_dir, &src_dir, &mut sources)?;
    // Walk order is filesystem order, which is not stable across machines;
    // the fingerprint has to be.
    sources.sort();

    let mut inputs = Vec::with_capacity(sources.len() + EMBEDDED_FILES.len() + 3);
    for relative in sources {
        let path = src_dir.join(&relative);
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("cannot read {} to fingerprint: {error}", path.display()))?;
        inputs.push(FingerprintInput {
            name: format!("src/{relative}"),
            path: Some(path),
            bytes,
        });
    }

    // The logical name is hashed rather than the path, so the value does not
    // move with where the caller stands relative to the file: `build.rs` and
    // the test that recomputes reach these by different relative paths and
    // must agree. In the table's order, which is source rather than a
    // directory listing, so this half needs no sort.
    for (name, relative) in EMBEDDED_FILES {
        let path = crate_root.join(relative);
        inputs.push(FingerprintInput {
            name: (*name).to_owned(),
            bytes: std::fs::read(&path).map_err(|error| {
                format!("cannot read {} to fingerprint: {error}", path.display())
            })?,
            path: Some(path),
        });
    }

    let manifest = crate_root.join("Cargo.toml");
    inputs.push(FingerprintInput {
        name: "Cargo.toml".to_owned(),
        bytes: std::fs::read(&manifest).map_err(|error| {
            format!("cannot read {} to fingerprint: {error}", manifest.display())
        })?,
        path: Some(manifest),
    });

    let lock = workspace_lock_file(crate_root)?;
    inputs.push(FingerprintInput {
        name: "Cargo.lock".to_owned(),
        bytes: std::fs::read(&lock)
            .map_err(|error| format!("cannot read {} to fingerprint: {error}", lock.display()))?,
        path: Some(lock),
    });

    inputs.push(FingerprintInput {
        name: "features".to_owned(),
        path: None,
        bytes: features.join(",").into_bytes(),
    });

    Ok(inputs)
}

/// The `Cargo.lock` governing `crate_root`: this crate is a workspace member,
/// so the lock is a directory up, but the search walks ancestors rather than
/// hard-coding `..` so a layout change moves the lock instead of quietly
/// dropping it out of the hash.
///
/// Absent is an error and not an empty input, for the reason `build.rs` fails
/// rather than emitting a constant: a key that does not describe the compiler
/// is worse than no cache.
fn workspace_lock_file(crate_root: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let mut dir = Some(crate_root);
    while let Some(here) = dir {
        let candidate = here.join("Cargo.lock");
        if candidate.is_file() {
            return Ok(candidate);
        }
        dir = here.parent();
    }
    Err(format!(
        "no Cargo.lock in {} or any ancestor: the fingerprint cannot describe the \
         dependency versions the compiler was built against",
        crate_root.display()
    ))
}

/// Every `.rs` file under `dir`, as paths relative to `root`, with `/`
/// separators so the names do not change with the host's path separator.
fn collect_rust_sources(
    root: &std::path::Path,
    dir: &std::path::Path,
    into: &mut Vec<String>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("cannot list {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot list {}: {error}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| format!("cannot stat {}: {error}", path.display()))?;
        if kind.is_dir() {
            collect_rust_sources(root, &path, into)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let relative = path.strip_prefix(root).map_err(|error| {
                format!(
                    "{} is not under {}: {error}",
                    path.display(),
                    root.display()
                )
            })?;
            let mut name = String::new();
            for part in relative.components() {
                if !name.is_empty() {
                    name.push('/');
                }
                name.push_str(&part.as_os_str().to_string_lossy());
            }
            into.push(name);
        }
    }
    Ok(())
}

/// Hash a finished input set. Separate from building the set so a test can
/// mutate a copy and watch the hash move, which is the property the cache
/// depends on and the one a recompute-and-compare test cannot see.
fn hash_compiler_fingerprint_inputs(inputs: &[FingerprintInput]) -> Result<String, String> {
    use std::fmt::Write as _;

    let mut hasher = blake3::Hasher::new();
    // v2, and it stays v2 through this merge. `ix-patched` bumped v1 -> v2
    // when the embedded files joined the input set and this branch bumped it
    // for `Cargo.lock`, the feature set and the recursive walk; the union of
    // the two changes the *input list*, so every hash moves on its own and a
    // v3 would retire the same rows a second time for nothing. The tag is for
    // a change of scheme that leaves the inputs looking identical, which this
    // is not.
    hasher.update(b"ixe-compiler-fingerprint-v2");
    for input in inputs {
        // Length-prefixed, so two different splits of the input set cannot
        // share a preimage (the same argument hash::tagged makes in
        // ix-kernel).
        hasher.update(&(input.name.len() as u64).to_le_bytes());
        hasher.update(input.name.as_bytes());
        hasher.update(&(input.bytes.len() as u64).to_le_bytes());
        hasher.update(&input.bytes);
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize().as_bytes() {
        write!(hex, "{byte:02x}").map_err(|error| format!("cannot format the hash: {error}"))?;
    }
    Ok(hex)
}
