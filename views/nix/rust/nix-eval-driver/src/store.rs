//! Text ingestion into a local Nix store, with no daemon and no `nix` binary.
//!
//! # Why text ingestion, and why direct writes
//!
//! Two store operations carry this plank: `builtins.toFile` and the `.drv`
//! write behind `builtins.derivationStrict`. cppnix performs both with one
//! call -- `addToStoreFromDump(..., ContentAddressMethod::Raw::Text, ...)`,
//! which `src/libcmd/rust-eval-session.cc` uses for `rustStoreText` and
//! `rustWriteDerivation` alike -- and the path it lands on is
//! `makeFixedOutputPathFromCA` over a `TextInfo`. That computation is already
//! pure Rust in [`nix_eval_rs::drvpath::text_store_path`], so what is left is
//! putting the bytes where the computation says they go.
//!
//! The alternative first step was a Rust client for the nix daemon's worker
//! protocol. It was considered and declined for this plank; the reasoning and
//! what it costs are recorded on [`LocalStore`].
//!
//! # The store directory is not the directory bytes land in
//!
//! `store_dir` is hashed into every path ([`nix_eval_rs::drvpath::make_store_path`]
//! puts it inside the fingerprint), so it is part of the answer and not a
//! prefix that can be swapped. `real_store_dir` is where bytes physically go.
//! cppnix separates the two the same way -- `LocalFSStoreConfig::realStoreDir`
//! defaults to `rootDir / "nix" / "store"` while `storeDir` stays `/nix/store`
//! -- which is what makes `--store 'local?root=R'` produce a `/nix/store/...`
//! path whose file is at `R/nix/store/...`. Measured on this tree:
//!
//! ```console
//! $ nix-instantiate --store 'local?root=/private/tmp/probe/root' f.nix
//! /nix/store/5hy3ibxijhkhixjfm1d3ylrss5bdk5jp-chroot-probe.drv
//! $ find /private/tmp/probe/root -name '*.drv'
//! /private/tmp/probe/root/nix/store/5hy3ibxijhkhixjfm1d3ylrss5bdk5jp-chroot-probe.drv
//! ```
//!
//! Keeping the two apart is what lets the differential gate give each arm its
//! own root and still compare paths: the arms cannot alias one another's
//! files, so a byte comparison between them is a comparison of two
//! independently produced answers rather than of one file with itself.

use std::path::{Path, PathBuf};

/// Why a store write could not be performed.
///
/// Distinct from [`nix_eval_rs::host::StoreError`] because these are this
/// store's own failures; the caller in [`crate::host`] decides which
/// `StoreError` variant each one becomes, and that mapping is a judgement
/// about how the evaluator should report it rather than a fact about the
/// filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// The bytes could not be written. Carries the path and the reason.
    Io { path: String, why: String },
    /// A file already sits on the computed path with different contents.
    ///
    /// Reported rather than overwritten, and this is the check that stands in
    /// for the validity database a real store has. Everything here is
    /// content-addressed: the path is a hash of the bytes and the references,
    /// so two different byte strings on one path mean the store is corrupt or
    /// the hashing disagrees with whatever wrote first. Silently overwriting
    /// would erase exactly the evidence of a Tier 1 divergence, which is the
    /// one thing this plank exists to be able to detect.
    Collision { path: String },
    /// The name a store object was asked for is not one a store path may
    /// carry.
    ///
    /// A refusal and not a sanitisation: quietly rewriting the name would
    /// give a different store path than cppnix computes for the same
    /// expression, which is a tier-1 divergence dressed up as robustness.
    /// cppnix errors here and so does this.
    BadName { name: String, why: String },
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Io { path, why } => {
                write!(f, "writing store path '{path}': {why}")
            }
            WriteError::Collision { path } => write!(
                f,
                "store path '{path}' already exists with different contents; \
                 the store is inconsistent with this evaluator's hashing"
            ),
            WriteError::BadName { name, why } => {
                write!(f, "invalid store object name '{name}': {why}")
            }
        }
    }
}

/// A local Nix store this process writes into directly.
///
/// # What was given up by not speaking the daemon protocol
///
/// The other candidate for this plank was a Rust client for the nix daemon's
/// worker protocol, which is how a `nix` on a multi-user machine writes to
/// `/nix/store`. Direct writes were chosen for the first plank, and these are
/// the things that decision costs. Each is a reason to build the daemon
/// client next, not a reason this one is wrong:
///
/// * **Nothing is registered as valid.** A real store records every object in
///   `nix/var/nix/db/db.sqlite`; this writes bytes and stops. So a `.drv`
///   written here is a file, not a store object: `nix-store --query`,
///   `nix build`, garbage collection and substitution will not see it. The
///   gate compares paths and bytes, which is exactly what is unaffected.
/// * **No realisation, so no import from derivation.** Building needs a
///   builder and a database; [`crate::host::DriverHost`] therefore refuses
///   `realise` by name rather than pretending.
/// * **No writing into a store this process does not own.** The daemon exists
///   because `/nix/store` is root-owned on a multi-user machine. This store
///   refuses a `real_store_dir` it cannot create or write, which is why the
///   driver takes an explicit `--store-root`.
/// * **No GC roots.** Nothing written here is protected from a collection of
///   the store it was written into, had it been a real one.
/// * **No signatures and no trust model.** The daemon checks both; there is
///   nobody here to check.
///
/// Against that, the daemon client would have bought none of what this plank
/// needed. `addTextToStore` over the worker protocol carries the same bytes to
/// the same computed path, so drvPath, outPath and `.drv` bytes would be
/// identical -- while the protocol itself is a versioned surface with a
/// handshake, framing and a NAR serialiser to implement before the first byte
/// moves. The order that gets a checkable plank soonest is: text ingestion
/// direct, then NAR ingestion, then the daemon.
#[derive(Debug, Clone)]
pub struct LocalStore {
    store_dir: String,
    real_store_dir: PathBuf,
    read_only: bool,
}

impl LocalStore {
    /// Open a store whose paths are computed under `store_dir` and whose bytes
    /// land under `root`/`store_dir` when `root` is given, or directly in
    /// `store_dir` when it is not.
    ///
    /// # Errors
    ///
    /// When the directory bytes would land in cannot be created. Refused here,
    /// at open, rather than at the first write: a driver that discovers it
    /// cannot write half way through an evaluation has already told the
    /// evaluator a path it will not honour.
    pub fn open(store_dir: &str, root: Option<&Path>, read_only: bool) -> Result<Self, String> {
        let trimmed = store_dir.trim_end_matches('/');
        if !trimmed.starts_with('/') {
            return Err(format!(
                "store directory '{store_dir}' is not absolute; it is hashed into every \
                 store path, so a relative one would silently produce paths for no store"
            ));
        }
        // An empty `--store-root` is not "no root": pushing `trimmed` onto an
        // empty `OsString` produces exactly `store_dir`, so `--store-root ""`
        // -- which is what `--store-root "$ROOT"` does when `ROOT` is unset --
        // silently wrote unregistered objects into the real /nix/store. Found
        // in review, and reproduced: it created a file there that
        // `nix path-info` then called invalid.
        if let Some(root) = root {
            let text = root.to_string_lossy();
            if text.is_empty() || !text.starts_with('/') {
                return Err(format!(
                    "store root '{text}' is not an absolute path. An empty or relative root is \
                     how a `--store-root \"$UNSET\"` ends up writing into the real store."
                ));
            }
        }
        let real_store_dir = match root {
            // `store_dir` is absolute, so `join` would replace rather than
            // extend. cppnix concatenates for the same reason.
            Some(root) => {
                let mut real = root.as_os_str().to_owned();
                real.push(trimmed);
                PathBuf::from(real)
            }
            None => PathBuf::from(trimmed),
        };
        // Writing needs a root, always. Without one `real_store_dir` is the
        // machine's own /nix/store, and everything this store writes is
        // unregistered -- no validity entry, no signature, no reference
        // recorded -- so it looks like a store object to anything reading the
        // directory and is invalid to anything asking the daemon. That is a
        // worse state than either writing properly or refusing, and the
        // refusal is the only one of the three this crate can honestly
        // deliver.
        //
        // `read_only` is exempt because it computes paths and writes nothing,
        // which is what `eval` does and why `eval` needs no root.
        if !read_only && real_store_dir.as_os_str() == std::ffi::OsStr::new(trimmed) {
            return Err(format!(
                "refusing to write into '{trimmed}' itself. This store registers nothing valid, \
                 so objects it wrote there would be indistinguishable from real ones on disk and \
                 invalid to the daemon. Pass --store-root DIR to write under a root, or \
                 --read-only to compute paths without writing."
            ));
        }
        if !read_only && let Err(e) = std::fs::create_dir_all(&real_store_dir) {
            return Err(format!(
                "creating store directory '{}': {e}",
                real_store_dir.display()
            ));
        }
        Ok(LocalStore {
            store_dir: trimmed.to_owned(),
            real_store_dir,
            read_only,
        })
    }

    /// The directory hashed into every path this store computes.
    #[must_use]
    pub fn store_dir(&self) -> &str {
        &self.store_dir
    }

    /// Where bytes physically land.
    #[must_use]
    pub fn real_store_dir(&self) -> &Path {
        &self.real_store_dir
    }

    /// Whether this store computes paths without writing, cppnix's
    /// `settings.readOnlyMode`.
    #[must_use]
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Store `contents` under `name` with `references`, and answer with the
    /// store path.
    ///
    /// `name` is the complete store-object name, `.drv` suffix included where
    /// there is one, matching [`nix_eval_rs::drvpath::text_store_path`]. This
    /// is cppnix's `addTextToStore`, and it is the one call behind both
    /// `builtins.toFile` and `writeDerivation` -- the same collapsing the
    /// bridge does, and for the same reason: a second spelling of the hashing
    /// rule is a second thing to drift.
    ///
    /// Under [`LocalStore::read_only`] the path is computed and no bytes move,
    /// which is what `nix-instantiate --eval` does.
    ///
    /// # Errors
    ///
    /// [`WriteError::Io`] when the bytes cannot be written, and
    /// [`WriteError::Collision`] when a different file already holds the path.
    pub fn add_text(
        &self,
        name: &str,
        contents: &str,
        references: &[String],
    ) -> Result<String, WriteError> {
        // Validate the name BEFORE computing anything with it. cppnix does
        // this inside `makeTextPath` -> `StorePath::StorePath`, so a name it
        // rejects never becomes a path; here it has to be explicit, because
        // `text_store_path` will happily hash any string.
        //
        // Skipping it was a path traversal and a tier-1 divergence at once.
        // `builtins.toFile "../../../../../../tmp/pwned" "OWNED"` produced a
        // "store path" ending in `-../../../../../../tmp/pwned`, whose base
        // name went straight into `real_store_dir.join(..)`, and
        // `write_once`'s `create_dir_all` walked the `..` chain and wrote the
        // file outside the store root. cppnix refuses the same expression:
        // "name '../../x' contains illegal character '/'". Found in review.
        //
        // `check_name` is the crate's own copy of cppnix's rule
        // (`storepath.rs:59`, mirroring `path.cc:16`), so this is the same
        // predicate the bridge applies rather than a second opinion about it.
        nix_eval_rs::storepath::check_name(name).map_err(|why| WriteError::BadName {
            name: name.to_owned(),
            why,
        })?;
        let path =
            nix_eval_rs::drvpath::text_store_path(&self.store_dir, name, contents, references);
        if self.read_only {
            return Ok(path);
        }
        // The base name and not a `join` of the whole path: `path` starts with
        // `store_dir`, which is already the tail of `real_store_dir`.
        let base = path
            .strip_prefix(&self.store_dir)
            .map_or(name, |rest| rest.strip_prefix('/').unwrap_or(rest));
        // Defence in depth, and cheap. `check_name` above is the real guard,
        // but this one does not depend on `check_name` being complete: a
        // basename holding a separator or a `..` component cannot address
        // anything but the directory it is joined to, so refusing it here
        // means no future gap in the name rule can become a write outside the
        // store. A guard that restates the invariant at the point of use is
        // worth more than one that trusts a caller three functions away.
        if base.is_empty()
            || base.contains('/')
            || base.contains('\0')
            || base == "."
            || base == ".."
        {
            return Err(WriteError::BadName {
                name: base.to_owned(),
                why: String::from("a store object's base name may not address another directory"),
            });
        }
        let target = self.real_store_dir.join(base);
        self.write_once(&target, &path, contents.as_bytes())?;
        Ok(path)
    }

    /// Put `bytes` at `target`, or confirm that they are already there.
    ///
    /// Write-to-temp-then-rename, because a reader of this store has no
    /// database to tell it whether a file is finished: a half-written `.drv`
    /// under its final name is indistinguishable from a complete one, and
    /// `rename` within a directory is the primitive that makes the file appear
    /// whole or not at all.
    fn write_once(&self, target: &Path, path: &str, bytes: &[u8]) -> Result<(), WriteError> {
        let io = |why: std::io::Error| WriteError::Io {
            path: path.to_owned(),
            why: why.to_string(),
        };
        // Already there is the ordinary case: two derivations sharing an input
        // both ask for it. Content-addressed, so equal bytes mean the work is
        // done; unequal bytes mean something is wrong that must not be papered
        // over. See `WriteError::Collision`.
        match std::fs::read(target) {
            Ok(existing) => {
                return if existing == bytes {
                    Ok(())
                } else {
                    Err(WriteError::Collision {
                        path: path.to_owned(),
                    })
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
        let Some(dir) = target.parent() else {
            return Err(WriteError::Io {
                path: path.to_owned(),
                why: "the store path has no parent directory".to_owned(),
            });
        };
        std::fs::create_dir_all(dir).map_err(io)?;
        // In the destination directory, so the rename cannot cross a
        // filesystem. The pid and a counter, because two drivers may write one
        // store and two derivations in one driver may write in one instant.
        let scratch = dir.join(format!(
            ".tmp-nix-eval-driver-{}-{}",
            std::process::id(),
            next_scratch()
        ));
        if let Err(e) = std::fs::write(&scratch, bytes) {
            drop(std::fs::remove_file(&scratch));
            return Err(io(e));
        }
        // 0444, as cppnix's store objects are. It does not move a byte of the
        // file and so cannot affect the parity comparison; what it affects is
        // the collision check above, which exists to catch two different byte
        // strings landing on one content-addressed path. A store whose
        // objects are writable invites exactly the tampering that check is
        // there to notice.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Err(e) =
                std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o444))
            {
                drop(std::fs::remove_file(&scratch));
                return Err(io(e));
            }
        }
        // `hard_link` and not `rename`, because `rename` overwrites.
        //
        // The collision check above is a read followed by a write, and
        // between the two another driver writing the same store can create
        // the file: both processes see `NotFound`, both rename, and the
        // second silently clobbers the first -- erasing exactly the evidence
        // the check exists to produce. `hard_link` fails with `AlreadyExists`
        // instead, which turns the race into the same answer the
        // non-racing path gives.
        //
        // The re-read on `AlreadyExists` is what keeps the ordinary
        // concurrent case quiet: two drivers writing the SAME bytes to a
        // content-addressed path is normal and not a collision.
        let linked = std::fs::hard_link(&scratch, target);
        drop(std::fs::remove_file(&scratch));
        match linked {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match std::fs::read(target) {
                    Ok(existing) if existing == bytes => Ok(()),
                    Ok(_) => Err(WriteError::Collision {
                        path: path.to_owned(),
                    }),
                    Err(e) => Err(io(e)),
                }
            }
            Err(e) => Err(io(e)),
        }
    }
}

/// A per-process counter for scratch names. Not a nonce and not required to
/// be one: it only has to make two writes in one process pick different
/// scratch files, and the pid separates processes.
fn next_scratch() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{LocalStore, Path, WriteError};

    /// A temporary directory that removes itself, so a failing test does not
    /// leave a store behind. `std::env::temp_dir` on macOS is `/var/folders`,
    /// which is not a symlink, so nothing here needs the `/private/tmp`
    /// dance the gate does.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "nix-eval-driver-test-{tag}-{}-{}",
                std::process::id(),
                super::next_scratch()
            ));
            drop(std::fs::remove_dir_all(&dir));
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    /// The path is the one the evaluator's own computation produces, and the
    /// bytes land under the root rather than at the path.
    #[test]
    fn a_rooted_store_computes_the_logical_path_and_writes_under_the_root() -> Result<(), String> {
        let scratch = Scratch::new("rooted");
        let store = LocalStore::open("/nix/store", Some(&scratch.0), false)?;
        let path = store
            .add_text("hello.txt", "hello\n", &[])
            .map_err(|e| e.to_string())?;

        let expected =
            nix_eval_rs::drvpath::text_store_path("/nix/store", "hello.txt", "hello\n", &[]);
        if path != expected {
            return Err(format!("path {path} is not the computed {expected}"));
        }
        if !path.starts_with("/nix/store/") {
            return Err(format!("path {path} is not under the logical store dir"));
        }
        let on_disk = scratch.0.join("nix/store").join(
            path.strip_prefix("/nix/store/")
                .ok_or_else(|| format!("path {path} lost its prefix"))?,
        );
        let bytes = std::fs::read_to_string(&on_disk)
            .map_err(|e| format!("reading {}: {e}", on_disk.display()))?;
        if bytes != "hello\n" {
            return Err(format!("wrote {bytes:?}, not the contents"));
        }
        Ok(())
    }

    /// Read-only computes the same path and moves no bytes: cppnix's
    /// `readOnlyMode`, which `nix-instantiate --eval` turns on.
    #[test]
    fn read_only_computes_the_path_and_writes_nothing() -> Result<(), String> {
        let scratch = Scratch::new("readonly");
        let writing = LocalStore::open("/nix/store", Some(&scratch.0), false)?;
        let reading = LocalStore::open("/nix/store", Some(&scratch.0), true)?;

        let wrote = writing
            .add_text("f", "contents", &[])
            .map_err(|e| e.to_string())?;
        let computed = reading
            .add_text("g", "contents", &[])
            .map_err(|e| e.to_string())?;
        if wrote == computed {
            return Err(
                "the two names should differ; the test is not measuring what it says".to_owned(),
            );
        }
        let g = scratch.0.join("nix/store").join(
            computed
                .strip_prefix("/nix/store/")
                .ok_or_else(|| format!("path {computed} lost its prefix"))?,
        );
        if g.exists() {
            return Err(format!("read-only mode wrote {}", g.display()));
        }
        Ok(())
    }

    /// Writing the same thing twice is the ordinary case and is not an error;
    /// two different byte strings on one path is.
    ///
    /// The collision arm is reached by writing the file by hand, because the
    /// hashing cannot produce it: that is the point -- the check exists for
    /// the case where something else has already put the wrong bytes there.
    #[test]
    fn the_same_bytes_twice_is_fine_and_different_bytes_on_one_path_is_refused()
    -> Result<(), String> {
        let scratch = Scratch::new("collision");
        let store = LocalStore::open("/nix/store", Some(&scratch.0), false)?;

        let path = store.add_text("f", "one", &[]).map_err(|e| e.to_string())?;
        let again = store
            .add_text("f", "one", &[])
            .map_err(|e| format!("a repeat write was refused: {e}"))?;
        if path != again {
            return Err(format!("repeat write moved the path: {path} then {again}"));
        }

        let on_disk = scratch.0.join("nix/store").join(
            path.strip_prefix("/nix/store/")
                .ok_or_else(|| format!("path {path} lost its prefix"))?,
        );
        // Store objects are 0444, so tampering means making it writable
        // first -- which is the point of the mode, and is exactly what this
        // test has to do deliberately to reach the case below.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&on_disk, std::fs::Permissions::from_mode(0o644))
                .map_err(|e| e.to_string())?;
        }
        std::fs::write(&on_disk, "tampered").map_err(|e| e.to_string())?;
        match store.add_text("f", "one", &[]) {
            Err(WriteError::Collision { path: reported }) => {
                if reported != path {
                    return Err(format!("collision named {reported}, not {path}"));
                }
                Ok(())
            }
            Err(other) => Err(format!("expected a collision, got {other}")),
            Ok(_) => Err("a tampered store path was silently overwritten".to_owned()),
        }
    }

    /// A relative store directory is refused at open, because it is hashed
    /// into every path this store would then compute.
    /// A name that would climb out of the store is refused, and nothing is
    /// written anywhere.
    ///
    /// The regression this pins is a path traversal: `builtins.toFile` passes
    /// its name straight here, and without validation
    /// `"../../../../../../tmp/pwned"` produced a computed path whose base
    /// name was that whole traversal, which `create_dir_all` then walked --
    /// writing a file outside the store root. cppnix refuses the same name.
    ///
    /// The assertion is two-sided on purpose: an error return would be
    /// satisfied by a store that errored *after* writing, and the file
    /// landing outside the root is the actual harm.
    #[test]
    fn a_name_that_climbs_out_of_the_store_is_refused_and_writes_nothing() -> Result<(), String> {
        let root = std::env::temp_dir().join(format!("ned-climb-{}", std::process::id()));
        let outside = root.join("ESCAPED");
        let _ = std::fs::remove_dir_all(&root);
        let store = LocalStore::open("/nix/store", Some(&root), false)?;

        // Enough `..` to leave `<root>/nix/store` and land back in `<root>`.
        let climb = "../../ESCAPED";
        let result = store.add_text(climb, "OWNED", &[]);
        let verdict = match result {
            Err(WriteError::BadName { .. }) => Ok(()),
            Err(other) => Err(format!("refused, but as {other:?} rather than BadName")),
            Ok(path) => Err(format!("the traversal was accepted and returned {path}")),
        };
        let leaked = outside.exists();
        let _ = std::fs::remove_dir_all(&root);
        verdict?;
        if leaked {
            return Err(String::from("a file was written outside the store root"));
        }
        Ok(())
    }

    /// Writing needs an explicit root; an empty or relative one is refused
    /// rather than resolved to the real store.
    ///
    /// `--store-root ""` is what `--store-root "$ROOT"` becomes when `ROOT`
    /// is unset, and it used to be indistinguishable from passing no root at
    /// all -- which wrote unregistered objects into the machine's own
    /// /nix/store.
    #[test]
    fn writing_without_a_root_is_refused() -> Result<(), String> {
        // No root at all, and writing: refused.
        if LocalStore::open("/nix/store", None, false).is_ok() {
            return Err(String::from(
                "a writable store with no root was opened; it would write into the real store",
            ));
        }
        // The same thing spelled as an empty root.
        if LocalStore::open("/nix/store", Some(Path::new("")), false).is_ok() {
            return Err(String::from("an empty --store-root was accepted"));
        }
        // And a relative one, which would depend on the working directory.
        if LocalStore::open("/nix/store", Some(Path::new("rel")), false).is_ok() {
            return Err(String::from("a relative --store-root was accepted"));
        }
        // Read-only needs no root, because it writes nothing.
        LocalStore::open("/nix/store", None, true)
            .map_err(|e| format!("read-only refused: {e}"))?;
        Ok(())
    }

    #[test]
    fn a_relative_store_directory_is_refused() {
        let refused = LocalStore::open("nix/store", None, true);
        assert!(
            refused.is_err(),
            "a relative store directory was accepted: {refused:?}"
        );
    }
}
