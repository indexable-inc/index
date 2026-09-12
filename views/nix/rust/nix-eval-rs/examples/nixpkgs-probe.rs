//! Evaluate real nixpkgs through this crate alone: no `nix` build, no C++
//! bridge, no cluster. 3.5s for the twelve default rows on a laptop under
//! `--release`, 0.6s for one expression; four times that in a debug build.
//!
//! This exists because the usual way to ask "how far into nixpkgs does the
//! Rust backend get, and what stops it?" is `maintainers/ix/nixpkgs-frontier.sh`,
//! which needs a built `nix-instantiate`, which needs a dev node. For any
//! question whose answer lives in the *evaluator*, that whole round trip is
//! avoidable: the crate can evaluate `import <nixpkgs> {}` on its own given
//! five `Host` hooks and three process globals, which is what this wires up.
//! It located and confirmed the fix for ENG-12593 with no `nix` build at all.
//!
//! It is also the loop that keeps working when the tree does not. A crate-level
//! probe cannot be blocked by a C++ compile error, and on 2026-08-05 the fork
//! tip did not build under `-Dnix:rust-eval=enabled` for several hours because
//! of a header ordering bug (`ixe.h` used `IxeSession` above its forward
//! declaration). The full gate was unrunnable; this was not.
//!
//! # What it does not cover
//!
//! **A green probe is not a green gate.** It exercises the evaluator and
//! nothing else:
//!
//! * **No C++ bridge.** Every `ixe_*` entry point, the handle table, the
//!   refusal-token plumbing and all error mapping are absent. A divergence in
//!   how a refusal is *reported* is invisible here.
//! * **No CLI surface.** `eval-backend` selection, settings, `--strict`,
//!   printing and exit codes all belong to `nix-instantiate`, which is not in
//!   this process.
//! * **No store, unless you have one.** Store copies shell out to `nix store
//!   add-path` and `builtins.toFile` to `nix eval`; see `--no-store`. A
//!   `toFile` whose contents carry references is refused rather than
//!   approximated -- see `store_text`.
//! * **Single-arm by default.** The frontier compares two backends. Without
//!   `--cpp` this reports only what the Rust evaluator did, so it can tell you
//!   *that* an expression refuses and not *whether cpp agrees* with the value.
//!
//! Use it to bisect and to iterate. Confirm on the gate before believing a
//! result, and quote the gate in a PR, not this.
//!
//! # The trap this deliberately does not fall into, and which way it points
//!
//! A harness standing in for an embedder callback hides whatever that callback
//! refuses -- and, just as expensively, refuses whatever the callback learned
//! to answer. Both directions have now happened here, to the same lookup.
//!
//! An earlier throwaway version resolved `<nix/fetchurl.nix>` out of the nix
//! source tree because that made nixpkgs get further, which made the probe
//! more capable than the real binary and reported the remaining work in the
//! wrong order (ENG-12607). The fix was to refuse it, as the embedder did at
//! the time.
//!
//! The embedder then stopped refusing it. `rustFindFile` in
//! `src/libcmd/rust-eval-session.cc` resolves the lookup, notices the answer came
//! from an accessor that is not `rootFS`, reads the bytes and hands them over
//! in the answer, so the evaluator serves `corepkgs` from memory and
//! `builtins.toString <nix/fetchurl.nix>` is `/fetchurl.nix` on both arms.
//! The refusal here outlived that by long enough to stop the probe at stdenv
//! bootstrap on every expression that reaches `fetchurl`, which is most of
//! nixpkgs. This file now does what the bridge does, from the same one file
//! cppnix compiles in (`eval.cc:378`), and the guard test below pins the
//! agreement rather than the refusal.
//!
//! # Running it
//!
//! ```console
//! $ NIXPKGS=/nix/store/...-source cargo run --release --example nixpkgs-probe
//! $ NIXPKGS=...  cargo run --example nixpkgs-probe -- '(import <nixpkgs> {}).hello.name'
//! $ NIXPKGS=...  cargo run --example nixpkgs-probe -- --cpp "$(command -v nix-instantiate)"
//! ```
//!
//! `NIXPKGS` may be omitted if `nix` can resolve the `nixpkgs` flake. The
//! resolved path is printed, and its store hash is the revision.
//!
//! Exit status: 0 every expression evaluated, 1 at least one did not, 2 the
//! probe could not be set up (no nixpkgs, no store). A 1 is the normal state
//! while the frontier still has refusals in it.

use nix_eval_rs::eval::eval_str_on;
use nix_eval_rs::host::StoreError;
use nix_eval_rs::host::{self, FileType, LookupError};
use nix_eval_rs::task::{
    FetchRequest, FetchTreeRequest, FilteredCopy, PathMethod, SearchPathEntry,
};
use std::process::Command;

/// The expression list `maintainers/ix/nixpkgs-frontier.sh` asks, so that the
/// two can be read side by side. Kept in this order and with these labels for
/// that reason; the frontier is the gate and this is its fast rehearsal.
const FRONTIER: &[(&str, &str)] = &[
    ("the lookup itself", "builtins.typeOf <nixpkgs>"),
    ("lib alone", "(import <nixpkgs/lib>).version"),
    (
        "lib attr count",
        "builtins.length (builtins.attrNames (import <nixpkgs/lib>))",
    ),
    (
        "lib.strings",
        "(import <nixpkgs/lib>).strings.toUpper \"abc\"",
    ),
    (
        "the top-level function",
        "builtins.typeOf (import <nixpkgs>)",
    ),
    ("the package set", "builtins.typeOf (import <nixpkgs> {})"),
    ("one package name", "(import <nixpkgs> {}).hello.name"),
    ("one package outPath", "(import <nixpkgs> {}).hello.outPath"),
    ("stdenv", "(import <nixpkgs> {}).stdenv.name"),
    ("currentSystem", "builtins.currentSystem"),
    (
        "a small package set",
        "builtins.typeOf (import <nixpkgs> { system = \"x86_64-linux\"; })",
    ),
    (
        "package set attr count",
        "builtins.length (builtins.attrNames (import <nixpkgs> { system = \"x86_64-linux\"; }))",
    ),
];

/// Set from the command line and read by the hooks, which are plain `fn`
/// pointers and so cannot capture. `OnceLock` rather than `static mut` because
/// the hooks are called from the evaluator and a data race here would be a
/// wrong answer rather than a crash.
static NIXPKGS_ROOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static STORE_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn nixpkgs_root() -> Option<&'static str> {
    NIXPKGS_ROOT.get().map(String::as_str)
}

fn store_enabled() -> bool {
    STORE_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The one file cppnix's `corepkgs` accessor holds, compiled in here the way
/// `eval.cc:378` compiles it into the binary.
///
/// Read from the fork's own source rather than from disk at run time, because
/// that is what makes this the *same bytes* the embedder would serve. A copy
/// resolved from a directory the caller names could be any tree at all, which
/// is what `--corepkgs` used to be and why it is gone.
const COREPKGS_FETCHURL: &str = include_str!("../../../src/libexpr/fetchurl.nix");

/// `<nixpkgs>` and `<nixpkgs/...>` resolve against the tree under test, and
/// `<nix/fetchurl.nix>` is served from memory the way the embedder serves it.
///
/// The embedder's rule (`src/libcmd/rust-eval-session.cc`, `rustFindFile`) is not
/// "refuse what is not on disk". It is: resolve, and when the answer came from
/// an accessor that is not `rootFS`, hand the bytes over with the path cppnix
/// reports, and the host serves the path from them. Here the one such file is
/// known up front, so it is in the host's `virtual_files` from the start. So
/// the evaluator sees `/fetchurl.nix`, the file is readable, and
/// `builtins.toString <nix/fetchurl.nix>` matches the cpp arm. Mirroring that is what keeps this
/// probe neither more nor less capable than the binary; refusing it made the
/// probe stop at stdenv bootstrap while the binary walked past.
///
/// `corepkgs` holds exactly one file, so any other name under `nix/` resolves
/// to the accessor and then fails to read, which the embedder reports as a
/// refusal rather than a miss (`rustFindFile`'s code 2). Same here.
fn find_file(
    _entries: &[SearchPathEntry],
    name: &str,
) -> Result<nix_eval_rs::value2::PathValue, LookupError> {
    let Some(root) = nixpkgs_root() else {
        return Err(LookupError::NoResolver);
    };
    if name == "nixpkgs" {
        return Ok(nix_eval_rs::value2::PathValue::ambient(root));
    }
    if let Some(rest) = name.strip_prefix("nixpkgs/") {
        return Ok(nix_eval_rs::value2::PathValue::ambient(format!(
            "{root}/{rest}"
        )));
    }
    if let Some(rest) = name.strip_prefix("nix/") {
        // `CanonPath(path.substr(3))` in cppnix's `findFile`, so the path the
        // evaluator sees is the name minus `nix`, leading slash and all.
        let abs = format!("/{rest}");
        if rest == "fetchurl.nix" {
            return Ok(nix_eval_rs::value2::PathValue::ambient(abs));
        }
        return Err(LookupError::Unsupported(format!(
            "reading '<{name}>' from an accessor that is not the real filesystem: \
             cppnix's corepkgs holds only fetchurl.nix"
        )));
    }
    // Anything else is an ordinary miss, and a miss is a *throw* that
    // `builtins.tryEval` catches -- which nixpkgs relies on, since it probes
    // `<nixpkgs-overlays>` and expects to be told no. Reporting a miss as a
    // refusal would stop the walk at the first optional lookup, which is what
    // the first version of this hook did.
    Err(LookupError::NotFound(format!(
        "file '{name}' was not found in the Nix search path (add it using $NIX_PATH or -I)"
    )))
}

/// cppnix's in-memory `corepkgs`, as the bytes the host serves `/fetchurl.nix`
/// from: what `rustFindFile` hands over lazily, known up front here.
fn corepkgs() -> nix_eval_rs::host::VirtualFiles {
    let files = nix_eval_rs::host::VirtualFiles::default();
    files.insert("/fetchurl.nix", COREPKGS_FETCHURL);
    files
}

fn nix_path() -> Result<Vec<SearchPathEntry>, LookupError> {
    let Some(root) = nixpkgs_root() else {
        return Err(LookupError::NoResolver);
    };
    Ok(vec![SearchPathEntry {
        prefix: "nixpkgs".to_owned(),
        path: root.to_owned(),
    }])
}

/// The embedder copies through `state.store`; here that is the `nix` on
/// `PATH`. Slow (a process per path) and impure, which is why `--no-store`
/// exists -- but the default is the faithful one, because without a store the
/// evaluator refuses interpolations the real binary answers, and a probe that
/// refuses more than the binary is only marginally better than one that
/// refuses less.
fn copy_to_store(path: &nix_eval_rs::value2::PathValue) -> Result<String, String> {
    let path = path.accessor_path();
    if !store_enabled() {
        return Err("--no-store was given, so no path can be copied".to_owned());
    }
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "store",
            "add-path",
            path,
        ])
        .output()
        .map_err(|e| format!("running `nix store add-path {path}`: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `builtins.toFile`, asked of the `nix` on PATH rather than computed here.
///
/// The store path is a hash of the bytes under the `text` type string with
/// the references folded in, so a second implementation of that rule in the
/// probe would be a second thing to keep in step with `nixhash.rs` -- and a
/// probe that computed a *different* path than the binary would report a
/// frontier that does not exist. Shelling out to `builtins.toFile` itself
/// keeps one implementation of the rule.
///
/// References are refused rather than approximated. Reconstructing a string
/// context through the CLI means `builtins.storePath` on each reference,
/// which needs those paths to exist in this store; a probe that silently
/// dropped them would compute a plausible wrong path. nixpkgs reaches
/// `toFile` overwhelmingly with plain text, so the refusal is narrow and it
/// is loud.
fn store_text(name: &str, contents: &str, references: &[String]) -> Result<String, String> {
    if !store_enabled() {
        return Err("--no-store was given, so no text can be stored".to_owned());
    }
    if !references.is_empty() {
        return Err(format!(
            "builtins.toFile {name:?} with {} reference(s), which this probe refuses rather \
             than approximating (the real embedder handles them)",
            references.len()
        ));
    }
    // `${` as well as the quote and the backslash: the name goes into a Nix
    // string literal below, so an interpolation in it would silently produce
    // the path for a different name, which is the failure this check exists
    // to refuse.
    if name.contains('"') || name.contains('\\') || name.contains("${") {
        return Err(format!(
            "builtins.toFile name {name:?} is not expressible here"
        ));
    }
    let scratch = std::env::temp_dir().join(format!(
        "nixpkgs-probe-tofile-{}-{}",
        std::process::id(),
        TOFILE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    // `create_new` rather than `write`: the path is predictable, and `write`
    // follows a symlink planted there.
    {
        use std::io::Write;
        let mut f = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&scratch)
            .map_err(|e| format!("creating {scratch:?}: {e}"))?;
        f.write_all(contents.as_bytes())
            .map_err(|e| format!("writing {scratch:?}: {e}"))?;
    }
    let expr = format!(
        "builtins.toFile \"{name}\" (builtins.readFile {})",
        scratch.display()
    );
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "eval",
            "--raw",
            "--impure",
            "--expr",
            &expr,
        ])
        .output();
    let _ = std::fs::remove_file(&scratch);
    let out = out.map_err(|e| format!("running `nix eval` for builtins.toFile: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

static TOFILE_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

static FILTERED_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// `builtins.path`, asked of the `nix` on PATH.
///
/// # Why staging into a temp tree is sound, and how that was checked
///
/// The evaluator has already applied the filter, so what arrives is a set of
/// paths. This reproduces exactly that set under a scratch directory and runs
/// `nix store add --name <name>` on it. That is the same store path cppnix's
/// `addPath` produces because a NAR records only what this copy preserves: the
/// entry names, the file bytes, the executable bit and the symlink targets.
/// Nothing in a NAR carries an mtime, an owner or a non-executable permission
/// bit, so the scratch tree and the filtered original serialise identically.
///
/// That is an argument, and it was also measured before it was believed. On a
/// fixture with a subdirectory, a symlink, an executable file and a pruned
/// subtree, four staged copies landed on the paths `nix-instantiate --eval`
/// gave for the same `builtins.path` calls:
///
/// ```text
/// filter = p: t: t != "symlink"       b7kkdmpxys35llvcmmr3vv8gyq16v2r7-tree
/// filter = p: t: baseNameOf p != "skipme"  n2ivrcbw2j8pizd3rmf81a4vrfnaihdr-tree
/// filter = p: t: false                42w0fdvrafxfyb76rr8bayd7zw3w8ban-tree
/// recursive = false, name = "flatname"    n21s7cwi8ybjhvpgj601svn5ybcqd0kv-flatname
/// ```
///
/// The unfiltered case does not stage at all -- `nix store add` on the root is
/// the copy cppnix makes.
///
/// # What it refuses, by name
///
/// * an expected `sha256`, because cppnix answers the *precomputed*
///   fixed-output path when the store already has it and errors on a mismatch
///   otherwise, and a probe that only did the copy would turn cppnix's error
///   into a success;
/// * an accepted entry of unknown type (a fifo, a socket, a device), which
///   cannot be staged -- cppnix's own dump throws on one too, but with its own
///   wording, and inventing that wording here would be a divergence dressed up
///   as agreement;
/// * a filtered `flat` copy, which the evaluator never sends (cppnix's flat
///   ingestion has no filter), so receiving one means this probe and the
///   evaluator disagree and the answer should not be trusted.
fn store_filtered(request: &FilteredCopy) -> Result<String, String> {
    if !store_enabled() {
        return Err("--no-store was given, so no path can be copied".to_owned());
    }
    // cppnix computes the fixed-output path from a declared `sha256` and
    // answers with it *without copying* when the store already holds it,
    // copying and comparing otherwise (`primops.cc:2967`). Both branches are
    // reproduced, and the path arithmetic is the crate's own
    // `make_fixed_output_path` -- the one `hello.outPath` parity was reached
    // with -- rather than a second implementation of the rule.
    let expected = match &request.expected_sha256 {
        None => None,
        Some(sri) => {
            let hash = nix_eval_rs::nixhash::parse_any(
                sri,
                Some(nix_eval_rs::nixhash::HashAlgo::Sha256),
                false,
            )
            .map_err(|e| format!("parsing the sha256 attribute {sri:?}: {e}"))?;
            let ca = match request.method {
                PathMethod::NixArchive => nix_eval_rs::drvpath::CaMethod::NixArchive,
                PathMethod::Flat => nix_eval_rs::drvpath::CaMethod::Flat,
            };
            let store_dir = nix_eval_rs::eval::store_dir().unwrap_or("/nix/store");
            let path =
                nix_eval_rs::drvpath::make_fixed_output_path(store_dir, &request.name, ca, &hash);
            if is_valid_path(&path) {
                return Ok(path);
            }
            Some(path)
        }
    };
    let mode = match request.method {
        PathMethod::NixArchive => "nar",
        PathMethod::Flat => "flat",
    };
    let source = match &request.accepted {
        // cppnix resolves the root before it copies (`addPath` passes
        // `path.resolveSymlinks()` to `fetchToStore`), so a symlinked root
        // archives its *target*. `nix store add` on the link itself would
        // archive a symlink node instead, which is a different store path --
        // this arm got that wrong before `builtins.path { path = <symlink>; }`
        // was put in the differential.
        None => std::fs::canonicalize(request.root.accessor_path())
            .map_err(|e| format!("resolving {:?}: {e}", request.root))?,
        Some(accepted) => {
            if request.method == PathMethod::Flat {
                return Err(
                    "builtins.path with both a filter and recursive = false: cppnix's flat \
                     ingestion never consults a filter, so the evaluator should not have sent \
                     one and this probe will not guess"
                        .to_owned(),
                );
            }
            // The root's own node is not in the accepted list -- cppnix
            // never offers it to the filter -- so a non-directory root has
            // nothing to stage and is copied as it stands. Decided by an
            // lstat rather than by the list being empty, because a directory
            // whose every entry was rejected also arrives with an empty list
            // and must still become an empty *directory* in the store. The
            // first version of this staged that case as an empty directory
            // for a regular file too, and landed on a store path
            // `nix-instantiate` disagreed with.
            let root_is_dir = std::fs::symlink_metadata(request.root.accessor_path())
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if root_is_dir {
                stage(request.root.accessor_path(), accepted)?
            } else {
                if !accepted.is_empty() {
                    return Err(format!(
                        "builtins.path filtered {} entries below {:?}, which is not a \
                         directory",
                        accepted.len(),
                        request.root
                    ));
                }
                std::path::PathBuf::from(request.root.accessor_path())
            }
        }
    };
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "store",
            "add",
            "--mode",
            mode,
            "--name",
            &request.name,
        ])
        .arg(&source)
        .output()
        .map_err(|e| format!("running `nix store add {}`: {e}", source.display()))?;
    if source != std::path::Path::new(request.root.accessor_path()) {
        let _ = std::fs::remove_dir_all(&source);
    }
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    let copied = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if let Some(expected) = expected
        && expected != copied
    {
        // cppnix's own wording, so a corpus row comparing the two arms
        // compares the message and not just the class.
        return Err(format!(
            "store path mismatch in (possibly filtered) path added from '{}'",
            request.root
        ));
    }
    Ok(copied)
}

/// Whether the store already holds `path`, which is the branch cppnix takes
/// to answer a declared `sha256` without copying.
fn is_valid_path(path: &str) -> bool {
    Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "path-info",
            path,
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `builtins.fetchurl` and `builtins.fetchTarball`, asked of the store this
/// process can see.
///
/// # It serves exactly one case, and refuses the rest by name
///
/// The pinned-and-already-present case: a `sha256` was given, the
/// fixed-output path computed from it is already valid, and cppnix's own
/// early exit (`primops/fetchTree.cc:540`) therefore answers with that path
/// having downloaded nothing. The arithmetic is the crate's own
/// `make_fixed_output_path` -- the one `hello.outPath` parity was reached
/// with -- so this is a real check of the rule and not a re-ask of the
/// oracle.
///
/// Everything else needs the network, and a probe that downloaded would be
/// answering a different question from the one the gate asks. In particular
/// this does NOT fall back to `nix-prefetch-url`: an unpinned fetch is
/// non-deterministic by construction, so a green run would mean the network
/// was up rather than that the two evaluators agree.
///
/// The refusal is by name, as `store_text` refuses a `toFile` with
/// references, for the reason the module header gives: a stand-in that is
/// more capable than the thing it stands in for hides work.
fn fetch(request: &FetchRequest) -> Result<String, String> {
    if !store_enabled() {
        return Err("--no-store was given, so nothing can be fetched".to_owned());
    }
    let Some(sri) = &request.expected_sha256 else {
        return Err(format!(
            "builtins.{} without a 'sha256' argument: that is an unpinned download, \
             which this probe will not perform -- run it on the real bridge",
            request.kind.who()
        ));
    };
    let hash =
        nix_eval_rs::nixhash::parse_any(sri, Some(nix_eval_rs::nixhash::HashAlgo::Sha256), false)
            .map_err(|e| format!("parsing the sha256 attribute {sri:?}: {e}"))?;
    let ca = match request.kind.method() {
        PathMethod::NixArchive => nix_eval_rs::drvpath::CaMethod::NixArchive,
        PathMethod::Flat => nix_eval_rs::drvpath::CaMethod::Flat,
    };
    let store_dir = nix_eval_rs::eval::store_dir().unwrap_or("/nix/store");
    let path = nix_eval_rs::drvpath::make_fixed_output_path(store_dir, &request.name, ca, &hash);
    if is_valid_path(&path) {
        return Ok(path);
    }
    Err(format!(
        "builtins.{} of {:?} is pinned to a store path this store does not hold ({path}), \
         so serving it needs a download -- which this probe will not perform",
        request.kind.who(),
        request.url
    ))
}

/// `builtins.fetchTree` and `builtins.fetchGit`, asked of the `nix` on PATH.
///
/// Refused outright, by name, and that is the whole implementation.
///
/// The reason is the module header's rule: a stand-in that is more capable
/// than the callback it replaces hides work, and a stand-in that is *less*
/// faithful invents answers. There is no third option here. The answer to a
/// tree fetch is the attribute set `emitTreeAttrs` builds -- `narHash`, `rev`,
/// `revCount`, `lastModified` and a nested `history`, all read out of a
/// `fetchers::Input` after the input cache and the registry have had their
/// say. Reproducing that from the CLI would mean parsing `nix flake metadata`
/// output and guessing which attributes a scheme emits, and a guess here is a
/// plausible attribute set that feeds a lock file.
///
/// So the probe says so and the bridge is where these are exercised. The
/// differential that means something for them is
/// `maintainers/ix/fetch-tree-parity.sh`, on a dev node.
fn fetch_tree(request: &FetchTreeRequest) -> Result<String, StoreError> {
    Err(StoreError::Unsupported(format!(
        "builtins.{} is not reproduced by this probe: its answer is the attribute set \
         emitTreeAttrs reads out of a fetchers::Input, and a stand-in that guessed at those \
         attributes would be inventing lock-file data. Exercise it on the real bridge",
        request.fetcher.as_str()
    )))
}

/// Rebuild the accepted subset of `root` under a fresh scratch directory and
/// return it. The list is in the walk's pre-order, so a directory always
/// arrives before anything inside it.
fn stage(
    root: &str,
    accepted: &[nix_eval_rs::task::AcceptedPath],
) -> Result<std::path::PathBuf, String> {
    let dir = std::env::temp_dir().join(format!(
        "nixpkgs-probe-path-{}-{}",
        std::process::id(),
        FILTERED_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).map_err(|e| format!("creating {dir:?}: {e}"))?;
    for entry in accepted {
        let Some(rel) = entry
            .path
            .strip_prefix(root)
            .and_then(|r| r.strip_prefix('/'))
        else {
            return Err(format!(
                "accepted path {:?} is not below the root {root:?}",
                entry.path
            ));
        };
        let target = dir.join(rel);
        let result = match entry.file_type {
            FileType::Directory => std::fs::create_dir(&target).map_err(|e| e.to_string()),
            FileType::Regular => copy_file(&entry.path, &target),
            FileType::Symlink => std::fs::read_link(&entry.path)
                .and_then(|to| std::os::unix::fs::symlink(to, &target))
                .map_err(|e| e.to_string()),
            FileType::Unknown => Err(
                "is neither a file, a directory nor a symlink, so it cannot be staged".to_owned(),
            ),
        };
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("staging {:?}: {e}", entry.path));
        }
    }
    Ok(dir)
}

/// Copy the bytes and the executable bit, which is all a NAR records.
fn copy_file(from: &str, to: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::copy(from, to).map_err(|e| e.to_string())?;
    let mode = std::fs::metadata(from)
        .map_err(|e| e.to_string())?
        .permissions()
        .mode();
    let executable = mode & 0o111 != 0;
    std::fs::set_permissions(
        to,
        std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
    )
    .map_err(|e| e.to_string())
}

/// cppnix skips this under `readOnlyMode`, and the embedder's hook has the
/// same branch (ENG-12479). Nothing here is being built, so treat it as read
/// only.
fn ensure_path(_path: &str) -> Result<(), String> {
    Ok(())
}

fn warn(message: &str) {
    eprintln!("warning: {message}");
}

fn trace(message: &str) {
    eprintln!("trace: {message}");
}

struct Options {
    exprs: Vec<(String, String)>,
    cpp: Option<String>,
    system: String,
}

fn resolve_nixpkgs() -> Option<(String, &'static str)> {
    if let Ok(p) = std::env::var("NIXPKGS") {
        return Some((p, "NIXPKGS"));
    }
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "eval",
            "--raw",
            "--impure",
            "--expr",
            "(builtins.getFlake \"nixpkgs\").outPath",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if p.is_empty() {
        None
    } else {
        Some((p, "the nixpkgs flake"))
    }
}

fn parse_args() -> Result<Options, String> {
    let mut exprs: Vec<(String, String)> = Vec::new();
    let mut cpp = None;
    let mut system = "x86_64-linux".to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cpp" => cpp = Some(args.next().ok_or("--cpp needs a nix-instantiate path")?),
            "--system" => system = args.next().ok_or("--system needs a platform string")?,
            "--no-store" => STORE_ENABLED.store(false, std::sync::atomic::Ordering::Relaxed),
            other => exprs.push((String::from("(argument)"), other.to_owned())),
        }
    }
    if exprs.is_empty() {
        exprs = FRONTIER
            .iter()
            .map(|(label, e)| ((*label).to_owned(), (*e).to_owned()))
            .collect();
    }
    Ok(Options { exprs, cpp, system })
}

/// What the cpp arm answered, when one was asked for.
fn cpp_answer(bin: &str, root: &str, expr: &str) -> String {
    let out = Command::new(bin)
        .args([
            "--eval",
            "--strict",
            "-I",
            &format!("nixpkgs={root}"),
            "-E",
            expr,
        ])
        .output();
    match out {
        Err(e) => format!("<could not run {bin}: {e}>"),
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let line = err
                .lines()
                .find(|l| l.contains("error:"))
                .unwrap_or("error")
                .trim();
            format!("ERROR {line}")
        }
    }
}

fn main() -> std::process::ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("nixpkgs-probe: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    let Some((root, how)) = resolve_nixpkgs() else {
        eprintln!(
            "nixpkgs-probe: no nixpkgs. Set NIXPKGS=/nix/store/...-source, or make \
             `nix eval --impure --expr '(builtins.getFlake \"nixpkgs\").outPath'` work."
        );
        return std::process::ExitCode::from(2);
    };
    let _ = NIXPKGS_ROOT.set(root.clone());

    // One host, built once and handed to every evaluation below. A probe
    // needs the same answers throughout, so this is a value it carries rather
    // than a process it configures.
    let host = host::FnHost {
        virtual_files: corepkgs(),
        find_file: Some(find_file),
        nix_path: Some(nix_path),
        store_copy: Some(copy_to_store),
        store_text: Some(store_text),
        store_ensure: Some(ensure_path),
        store_filtered: Some(store_filtered),
        fetch: Some(fetch),
        fetch_tree: Some(fetch_tree),
        warn: Some(warn),
        trace: Some(trace),
        ..host::FnHost::default()
    };
    // The three the embedder pushes in. Without them nixpkgs dies on its first
    // line with `builtins.nixVersion`, which is a discouraging first result and
    // is the reason this file exists rather than being folk knowledge.
    // Each is set-once and reports a conflict rather than silently keeping the
    // first value (ENG-12543), so a mistyped `--system` on a second run in one
    // process says so instead of quietly answering for the wrong platform.
    for outcome in [
        nix_eval_rs::eval::set_nix_version(NIX_VERSION),
        nix_eval_rs::eval::set_current_system(&opts.system),
        nix_eval_rs::eval::set_store_dir("/nix/store"),
    ] {
        if let Err(conflict) = outcome {
            eprintln!("nixpkgs-probe: {conflict}");
            return std::process::ExitCode::from(2);
        }
    }

    // The revision is the store hash of this path.
    println!("nixpkgs={root} (from {how})");
    println!(
        "probe: evaluator only -- no C++ bridge, no CLI, {}. NOT the gate; \
         run maintainers/ix/nixpkgs-frontier.sh before believing a result.",
        if store_enabled() {
            "store copies via `nix store add-path`"
        } else {
            "no store (--no-store)"
        }
    );
    println!(
        "globals: nixVersion={NIX_VERSION} currentSystem={} storeDir=/nix/store",
        opts.system
    );
    if let Some(bin) = &opts.cpp {
        let version = Command::new(bin)
            .arg("--version")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_else(|| String::from("<unknown>"));
        println!("cpp arm: {bin} ({version})");
    }
    println!();

    let mut ok = 0usize;
    let mut refused = 0usize;
    let mut failed = 0usize;
    for (i, (label, expr)) in opts.exprs.iter().enumerate() {
        let mut vm = nix_eval_rs::vm::Vm::with_settings(nix_eval_rs::eval::Settings::current());
        let answer = eval_str_on(
            expr,
            "/",
            nix_eval_rs::compile::Origin::String,
            &mut vm,
            &host,
        );
        let (verdict, detail) = match &answer {
            Ok(v) => {
                ok += 1;
                ("OK", v.clone())
            }
            Err(nix_eval_rs::eval::EvalError::Unimplemented(r)) => {
                refused += 1;
                // Token and prose both: the token is what a census groups by,
                // and grouping by the prose made rewording an error reset the
                // population silently (ENG-12546).
                ("REFUSED", format!("[{:?}] {}", r.token, r.detail))
            }
            Err(e) => {
                failed += 1;
                ("ERROR", format!("{e:?}"))
            }
        };
        println!("{:<2} {label:<24} {verdict:<8} {detail}", i + 1);
        if let Some(bin) = &opts.cpp {
            println!(
                "{:<2} {:<24} {:<8} {}",
                "",
                "",
                "cpp",
                cpp_answer(bin, &root, expr)
            );
        }
    }

    println!();
    println!(
        "RESULT nixpkgs-probe rows={} ok={ok} refused={refused} error={failed} nixpkgs={root}",
        opts.exprs.len()
    );
    if refused + failed == 0 {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(1)
    }
}

/// Any version string would do for `builtins.nixVersion`; nixpkgs only
/// compares it. Matching the fork's is what keeps a probe answer comparable
/// with a `nix-instantiate` answer.
const NIX_VERSION: &str = "2.34.7";

#[cfg(test)]
mod tests {
    use super::{NIXPKGS_ROOT, find_file};
    use nix_eval_rs::host::{Host, LookupError};

    /// The property this file exists to preserve: the probe answers exactly
    /// what the embedder answers, refuses exactly what it refuses, and
    /// reports a plain miss as a miss.
    ///
    /// All three have been wrong here at some point. Resolving `<nix/...>`
    /// from an arbitrary directory made the probe more capable than the
    /// binary and put the remaining work in the wrong order (ENG-12607).
    /// Refusing it outright made the probe *less* capable once the bridge
    /// learned to serve it from memory, which stopped every expression that
    /// reaches stdenv bootstrap. Reporting a miss as a refusal stops the walk
    /// at the first optional lookup, which nixpkgs does on
    /// `<nixpkgs-overlays>`.
    ///
    /// One test rather than four, because these share a process-global root.
    #[test]
    fn the_probe_refuses_what_the_embedder_refuses_and_no_more() {
        let _ = NIXPKGS_ROOT.set(String::from("/tmp/nixpkgs-probe-test"));

        assert_eq!(
            find_file(&[], "nixpkgs").ok().as_deref(),
            Some("/tmp/nixpkgs-probe-test")
        );
        assert_eq!(
            find_file(&[], "nixpkgs/lib").ok().as_deref(),
            Some("/tmp/nixpkgs-probe-test/lib")
        );

        // corepkgs: answered under the path cppnix reports, and then readable
        // through the probe's host -- the half a bare `Ok` would not prove.
        // Through the host and not `RealFs`: the bytes are the host's, and
        // no filesystem holds `/fetchurl.nix`.
        assert_eq!(
            find_file(&[], "nix/fetchurl.nix").ok().as_deref(),
            Some("/fetchurl.nix")
        );
        let host = nix_eval_rs::host::FnHost {
            virtual_files: super::corepkgs(),
            ..nix_eval_rs::host::FnHost::default()
        };
        let contents = host.read_file(&nix_eval_rs::value2::ambient_path("/fetchurl.nix"));
        assert!(
            matches!(&contents, Ok(text) if text.contains("derivation")),
            "the registered corepkgs file must be readable through Host; got {contents:?}"
        );

        // corepkgs holds one file, so a second name under `nix/` is the
        // embedder's refusal and not a catchable miss.
        let absent = find_file(&[], "nix/nope.nix");
        assert!(
            matches!(&absent, Err(LookupError::Unsupported(m))
                if m.contains("corepkgs holds only fetchurl.nix")),
            "an unknown corepkgs name must be refused, not missed; got {absent:?}"
        );

        // An ordinary miss, which cppnix throws and `tryEval` catches.
        let miss = find_file(&[], "nixpkgs-overlays");
        assert!(
            matches!(&miss, Err(LookupError::NotFound(m))
                if m.contains("was not found in the Nix search path")),
            "an absent entry must be a catchable miss; got {miss:?}"
        );
    }
}
