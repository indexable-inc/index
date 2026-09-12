//! The driver's embedder: a [`Host`] that answers store questions for real.
//!
//! This is the third embedder of `nix-eval-rs` and it plays the same role as
//! the other two. `src/libcmd/rust-eval-session.cc` answers out of cppnix's
//! `Store`; `examples/nixpkgs-probe.rs` answers by shelling out to a `nix`
//! binary; this answers out of [`crate::store::LocalStore`], which is the
//! point -- the charter's rule is that a store write must not be a `nix`
//! subprocess, because that subprocess is the wiring the ship-of-Theseus
//! direction exists to stop paying for.
//!
//! # What it answers, and what it refuses by name
//!
//! Answered for real, with bytes moving:
//!
//! * `builtins.toFile` ([`Host::store_text`])
//! * the `.drv` write behind `builtins.derivationStrict`
//!   ([`Host::write_derivation`])
//!
//! Filesystem reads, `getEnv`, warnings and traces are answered too, the
//! first four by delegating to [`RealFs`] and the last two by writing to
//! stderr.
//!
//! Refused **by name**, which is a different outcome from a wrong answer and
//! is reported as unimplemented rather than as an evaluation failure:
//!
//! * `builtins.fetchurl`, `builtins.fetchTarball`, `builtins.fetchTree`,
//!   `builtins.fetchGit`, `builtins.getFlake`. Sanctioned bridge territory:
//!   CLAUDE.md keeps the fetchers and flake locking C++-side until Rust
//!   replacements pass parity gates of their own, "because a second fetcher
//!   implementation is a second set of answers for a store path to differ
//!   over".
//! * `import`-from-derivation ([`Host::realise`]) and
//!   `builtins.appendContext`'s `ensurePath`, because realising needs a
//!   builder and a validity database and this store has neither. See
//!   [`crate::store::LocalStore`] for what direct writes gave up.
//! * the witness verifier's store questions ([`Host::valid_paths`],
//!   [`Host::sealed_paths`], [`Host::allow_paths`], [`Host::allow_closures`]):
//!   each is a statement about validity, and this store has no validity
//!   database to make one from. The verifier reads a refusal as "cannot say"
//!   and asks the effect itself again, which lands on one of the answers or
//!   refusals above.
//! * `"${./path}"` ([`Host::copy_to_store`], [`Host::store_path`]) and `builtins.path`
//!   ([`Host::store_filtered`]). These are the honest gap in this plank
//!   rather than a scoping decision: both are NAR ingestion, cppnix's
//!   `addToStore` with `ContentAddressMethod::Raw::NixArchive`, and the
//!   crate has no NAR writer -- `rg -i nar rust/nix-eval-rs/src` finds
//!   nothing. Refusing is right until one exists; approximating with the
//!   source path would be a wrong answer, which is what
//!   [`Host::copy_to_store`]'s own doc says.
//!
//! Every refusal is [`StoreError::Unsupported`] and not [`StoreError::NoStore`],
//! and the difference is load-bearing: `NoStore` says "nothing behind this
//! host owns a store", which is false here and would be read as this driver
//! being storeless. `Unsupported` says this backend cannot carry the answer,
//! which is the true statement and the one that surfaces as unimplemented.

use nix_eval_rs::host::{FileType, Host, Links, LookupError, RealFs, StoreError, StorePathResult};
use nix_eval_rs::task::SearchPathEntry;
use nix_eval_rs::value2::PathValue;

use crate::store::LocalStore;

/// A [`Host`] with a real local store behind it.
///
/// Filesystem reads are [`RealFs`]'s, unchanged. Everything this adds is a
/// store answer or a search-path answer.
pub struct DriverHost {
    store: LocalStore,
    /// The `-I` entries and `NIX_PATH`, in the order cppnix consults them.
    search_path: Vec<SearchPathEntry>,
    /// Where `trace` and `warn` go. Behind a flag because a gate comparing
    /// stdout must not have evaluator chatter interleaved into it, and cppnix
    /// puts both on stderr.
    quiet: bool,
}

impl DriverHost {
    #[must_use]
    pub fn new(store: LocalStore, search_path: Vec<SearchPathEntry>, quiet: bool) -> Self {
        DriverHost {
            store,
            search_path,
            quiet,
        }
    }

    #[must_use]
    pub fn store(&self) -> &LocalStore {
        &self.store
    }

    /// The one place a [`crate::store::WriteError`] becomes a
    /// [`StoreError`].
    ///
    /// Both arms are `Failed` and neither is `Unsupported`, deliberately: a
    /// store this driver owns failing to write is a fault in the world or in
    /// this code, not a gap in the backend, and reporting it as unimplemented
    /// would let a gate score it as an expected refusal.
    fn failed(e: &crate::store::WriteError) -> StoreError {
        StoreError::Failed(e.to_string())
    }
}

/// The message a refusal carries.
///
/// One function so every refusal reads the same way and names the builtin
/// that asked, which is what makes a refusal actionable in a gate log rather
/// than a shrug. `why` says what is missing, so a reader learns whether this
/// is waiting on a Rust fetcher, on NAR ingestion, or on a daemon.
fn refuse(who: &str, why: &str) -> StoreError {
    StoreError::Unsupported(format!(
        "{who} is not available in the Rust evaluation driver: {why}"
    ))
}

impl Host for DriverHost {
    // -- filesystem: RealFs's, unchanged ------------------------------------

    fn read_file(&self, path: &PathValue) -> Result<String, String> {
        RealFs.read_file(path)
    }

    fn read_file_bytes(&self, path: &PathValue) -> Result<Vec<u8>, String> {
        RealFs.read_file_bytes(path)
    }

    fn get_env(&self, name: &str) -> Option<String> {
        RealFs.get_env(name)
    }

    fn read_dir(&self, path: &PathValue) -> Result<Vec<(String, FileType)>, String> {
        RealFs.read_dir(path)
    }

    fn path_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
        RealFs.path_exists_checked(path)
    }

    fn dir_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
        RealFs.dir_exists_checked(path)
    }

    fn file_type(&self, path: &PathValue) -> Result<Option<FileType>, String> {
        RealFs.file_type(path)
    }

    fn file_type_resolved(&self, path: &PathValue) -> Result<FileType, String> {
        RealFs.file_type_resolved(path)
    }

    // -- store: answered for real -------------------------------------------

    fn store_text(
        &self,
        name: &str,
        contents: &str,
        references: &[String],
    ) -> Result<String, StoreError> {
        self.store
            .add_text(name, contents, references)
            .map_err(|e| Self::failed(&e))
    }

    fn write_derivation(&self, name: &str, aterm: &str) -> Result<String, StoreError> {
        // The suffix is appended here, exactly as cppnix's `writeDerivation`
        // appends it and as the bridge does: the evaluator hands the name over
        // bare so that this call reads like cppnix's. Below the suffix this is
        // the same `add_text` that serves `builtins.toFile`, because cppnix's
        // `writeDerivation` *is* `addTextToStore` of the ATerm -- a second
        // spelling here would be a mirror to drift from.
        //
        // The references are read off the ATerm, as the bridge reads them
        // (`capi::EmbedderHost::write_derivation`): the trait hands over the
        // bytes alone, so that a memo replay, which has nothing else, asks the
        // same question.
        let drv = nix_eval_rs::drv::parse(aterm).map_err(|e| {
            StoreError::Failed(format!(
                "derivation '{name}': the ATerm handed to the store does not parse: {e:?}"
            ))
        })?;
        self.store
            .add_text(&format!("{name}.drv"), aterm, &drv.references())
            .map_err(|e| Self::failed(&e))
    }

    // -- store: refused by name ---------------------------------------------

    fn copy_to_store(&self, path: &PathValue) -> Result<String, StoreError> {
        Err(refuse(
            &format!("copying '{path}' into the store"),
            "it is NAR ingestion (cppnix's addToStore with the NixArchive method) \
             and this crate has no NAR writer yet",
        ))
    }

    fn store_path(&self, path: &PathValue) -> Result<StorePathResult, StoreError> {
        Err(refuse(
            &format!("coercing '{path}' to its store path"),
            "it is the path a NAR ingestion would produce (cppnix's addToStore with \
             the NixArchive method) and this crate has no NAR writer yet",
        ))
    }

    fn store_filtered(
        &self,
        request: &nix_eval_rs::task::FilteredCopy,
    ) -> Result<String, StoreError> {
        Err(refuse(
            &format!("builtins.path (name '{}')", request.name),
            "it is NAR ingestion (cppnix's addToStore with the NixArchive method) \
             and this crate has no NAR writer yet",
        ))
    }

    fn ensure_path(&self, path: &str) -> Result<(), StoreError> {
        // Under read-only mode cppnix's `ensurePath` does not touch the store
        // at all -- `context.cc:275` short-circuits -- so `builtins.
        // appendContext` succeeds there for a path that is merely well
        // formed. The crate's own `Host` doc says that branch is the
        // embedder's to implement, and not implementing it made `eval` refuse
        // where cppnix answers `"s"`. Found in review.
        if self.store.read_only() {
            return Ok(());
        }
        // Present is present: a path already in this store needs nothing done
        // and `Ok(())` is the truth, which is the case cppnix's `ensurePath`
        // returns from immediately. Absent is where this store runs out: it
        // cannot substitute and cannot build.
        //
        // A path outside this store is REFUSED rather than looked up. The
        // previous spelling fell back to the whole path when the store-dir
        // prefix was absent, and `Path::join` with an absolute argument
        // replaces the base rather than extending it -- so
        // `ensure_path("/etc/passwd")` asked whether /etc/passwd exists, and
        // answered `Ok(())` because it does. Unreachable today, since
        // `primops_host` normalises the key first, but answering wrongly
        // instead of refusing is the one property this host must not have.
        let Some(base) = path
            .strip_prefix(self.store.store_dir())
            .and_then(|rest| rest.strip_prefix('/'))
            .filter(|base| !base.is_empty() && !base.contains('/'))
        else {
            return Err(refuse(
                &format!("making '{path}' present"),
                "it does not name an object directly inside this store's directory",
            ));
        };
        if self.store.real_store_dir().join(base).exists() {
            return Ok(());
        }
        Err(refuse(
            &format!("making '{path}' present"),
            "this store cannot substitute or build, having no daemon and no validity database",
        ))
    }

    // -- the witness verifier's store questions: refused by name ------------
    //
    // Each is a statement about validity -- which paths the store holds,
    // which objects are sealed, which reads are allowed -- and this store has
    // no validity database to make one from (`ensure_path` says the same).
    // The verifier reads a refusal as "cannot say" and asks the effect itself
    // again, which is the outcome under a host with no store.

    fn valid_paths(
        &self,
        paths: &[String],
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        Err(refuse(
            &format!("querying the validity of {} store path(s)", paths.len()),
            "this store has no validity database",
        ))
    }

    fn sealed_paths(&self, objects: &[String]) -> Result<Links, StoreError> {
        Err(refuse(
            &format!(
                "querying which of {} store object(s) are sealed",
                objects.len()
            ),
            "this store has no validity database",
        ))
    }

    fn allow_paths(&self, paths: &[String]) -> Result<(), StoreError> {
        Err(refuse(
            &format!("allowing reads through {} store path(s)", paths.len()),
            "this host has no allow list, and the copies and fetches an allowance \
             would follow are refused above",
        ))
    }

    fn allow_closures(&self, outputs: &[String]) -> Result<(), StoreError> {
        Err(refuse(
            &format!(
                "allowing reads through the closures of {} output path(s)",
                outputs.len()
            ),
            "realising is refused above, so no realisation is ever served by validity",
        ))
    }

    /// Nothing is deferred: `add_text` writes when asked.
    fn settle(&self) -> Result<(), StoreError> {
        Ok(())
    }

    fn realise(
        &self,
        _context: &[nix_eval_rs::value2::ContextElem],
    ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
        Err(refuse(
            "import from derivation",
            "realising a derivation needs a builder and a validity database, \
             which direct store writes do not provide",
        ))
    }

    fn fetch(&self, request: &nix_eval_rs::task::FetchRequest) -> Result<String, StoreError> {
        Err(refuse(
            request.kind.who(),
            "fetchers stay in the C++ bridge until a Rust replacement passes a parity \
             gate of its own (CLAUDE.md, 'the bridge decides nothing')",
        ))
    }

    fn fetch_tree(
        &self,
        request: &nix_eval_rs::task::FetchTreeRequest,
    ) -> Result<String, StoreError> {
        Err(refuse(
            request.fetcher.as_str(),
            "fetchers stay in the C++ bridge until a Rust replacement passes a parity \
             gate of its own (CLAUDE.md, 'the bridge decides nothing')",
        ))
    }

    fn lock_flake(&self, flake_ref: &str) -> Result<nix_eval_rs::host::FlakeCall, StoreError> {
        Err(refuse(
            &format!("builtins.getFlake \"{flake_ref}\""),
            "flake locking is bridge machinery like the fetchers; a stand-in would be \
             inventing lock-file data (CLAUDE.md)",
        ))
    }

    fn parse_flake_ref(&self, flake_ref: &str) -> Result<String, StoreError> {
        Err(refuse(
            &format!("builtins.parseFlakeRef \"{flake_ref}\""),
            "the flake-ref grammar is bridge machinery like the fetchers; a stand-in \
             would be inventing reference syntax (CLAUDE.md)",
        ))
    }

    fn flake_ref_to_string(
        &self,
        _attrs: &std::collections::BTreeMap<String, nix_eval_rs::task::TreeAttr>,
    ) -> Result<String, StoreError> {
        Err(refuse(
            "builtins.flakeRefToString",
            "the flake-ref grammar is bridge machinery like the fetchers; a stand-in \
             would be inventing reference syntax (CLAUDE.md)",
        ))
    }

    // -- search path ---------------------------------------------------------

    fn find_file(&self, entries: &[SearchPathEntry], name: &str) -> Result<PathValue, LookupError> {
        find_in(entries, name).map(PathValue::ambient)
    }

    fn nix_path(&self) -> Result<Vec<SearchPathEntry>, LookupError> {
        Ok(self.search_path.clone())
    }

    // -- outputs -------------------------------------------------------------

    /// This host answers no slow question off-thread, so there is nothing to
    /// begin and nothing to collect.
    ///
    /// Not an oversight and not a stub: `begin` is only ever called for the
    /// four [`Slow`] variants -- `Fetch`, `FetchTree`, `Flake`, `Realise` --
    /// and this host refuses all four by name, so a ticket could never be
    /// redeemed. `None` tells the scheduler to answer inline, which is
    /// exactly right for a host whose answer is an immediate refusal.
    ///
    /// This is the concrete reason `run.rs` says nothing overlaps today. The
    /// day a Rust fetcher lands behind this host, these two grow bodies and
    /// the scheduler starts overlapping with no change to the driver.
    ///
    /// Written out rather than taken from `nix-eval-rs`'s `host_stubs!`,
    /// which is `#[cfg(test)]` and `pub(crate)` and so unreachable from this
    /// crate -- and which would be the wrong tool regardless: that macro
    /// writes `NoStore` refusals for test leaves, whereas these two are a
    /// deliberate statement about a production host.
    fn begin(&self, _question: &nix_eval_rs::host::Slow<'_>) -> Option<nix_eval_rs::host::Ticket> {
        None
    }

    fn collect(
        &self,
        _ticket: nix_eval_rs::host::Ticket,
        _block: bool,
    ) -> Option<nix_eval_rs::host::SlowAnswer> {
        None
    }

    fn trace(&self, message: &str) {
        if !self.quiet {
            eprintln!("trace: {message}");
        }
    }

    fn warn(&self, message: &str) {
        if !self.quiet {
            eprintln!("warning: {message}");
        }
    }
}

/// Resolve `<name>` against `entries`, cppnix's `EvalState::findFile`
/// narrowed to the part that is a filesystem walk.
///
/// Narrowed and not reimplemented. cppnix's version also downloads a
/// pseudo-URL entry into the store, consults the registered lookup-path hooks
/// for a `scheme:rest` entry and applies the evaluator's access control; none
/// of those belong to a driver with no fetcher, so an entry this cannot handle
/// is skipped rather than guessed at, and a name that matches nothing is
/// [`LookupError::NotFound`] with cppnix's own wording -- the one outcome a
/// program can catch with `builtins.tryEval`.
///
/// `<nix/fetchurl.nix>` is deliberately absent. The bridge serves it from
/// cppnix's compiled-in `corepkgs` accessor, and the probe mirrors that by
/// `include_str!`-ing the same file. Doing so here would make the driver
/// answer a lookup whose only consumer is `builtins.fetchurl`, which this
/// driver refuses; a resolvable path leading to a refused fetch is a worse
/// error message than a lookup that says it cannot be resolved.
fn find_in(entries: &[SearchPathEntry], name: &str) -> Result<String, LookupError> {
    for entry in entries {
        let Some(rest) = suffix_after_prefix(&entry.prefix, name) else {
            continue;
        };
        // A URL or a `scheme:rest` entry is not something this can walk. Skip
        // it: cppnix would resolve it through a fetcher, and pretending it
        // missed is closer to the truth than treating the text as a path.
        //
        // `has_scheme` and not `!starts_with('/')`, which is what this said
        // before: that test also swallowed an ordinary relative entry like
        // `-I rel=./sub`, so the lookup missed where cppnix resolves it
        // against the working directory -- a wrong answer, since `pathExists
        // <rel/x>` then takes the other branch. Found in review.
        if has_scheme(&entry.path) {
            continue;
        }
        let base = if entry.path.starts_with('/') {
            entry.path.clone()
        } else {
            match std::env::current_dir() {
                Ok(cwd) => format!("{}/{}", cwd.to_string_lossy(), entry.path),
                // No working directory is not this entry's fault, but it is
                // not resolvable either; the next entry may still match.
                Err(_) => continue,
            }
        };
        let candidate = if rest.is_empty() {
            base.clone()
        } else {
            format!("{}/{}", base.trim_end_matches('/'), rest)
        };
        let found = std::path::Path::new(&candidate);
        if found.exists() {
            // Canonicalised, because cppnix's search-path resolution goes
            // through `canonPath(..., resolveSymlinks = true)` and returns
            // the real path. Two measured divergences without it, both
            // Tier 2 but both a wrong VALUE rather than a refusal:
            // `NIX_PATH=foo=/tmp` gave `/tmp` where cppnix gives
            // `/private/tmp`, and `-I rel=./sub` gave a path with the `./`
            // still in it. A path that differs is a path that hashes
            // differently the moment it is coerced into a derivation.
            //
            // Only ever after `exists()`, since `canonicalize` fails on an
            // absent path; and the un-canonicalised path is the fallback
            // rather than an error, because a candidate that exists but
            // cannot be canonicalised is still a better answer than "not
            // found".
            return Ok(found.canonicalize().map_or(candidate.clone(), |real| {
                real.to_string_lossy().into_owned()
            }));
        }
    }
    Err(LookupError::NotFound(format!(
        "file '{name}' was not found in the Nix search path"
    )))
}

/// Whether `text` opens with a URL scheme, per RFC 3986's `scheme` rule.
///
/// This is the test for "not a path I can walk". A bare `contains(':')` would
/// be wrong on both sides: a Windows-ish `C:` is not a scheme we care about,
/// and more to the point a perfectly ordinary directory name may contain a
/// colon, which would make it unreachable.
fn has_scheme(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    let rest: String = chars.collect();
    let Some(colon) = rest.find(':') else {
        return false;
    };
    rest.get(..colon).is_some_and(|scheme| {
        scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// What is left of `name` after `prefix`, or `None` when the entry does not
/// apply. An empty prefix matches anything, which is how a bare `-I /path`
/// entry works.
fn suffix_after_prefix(prefix: &str, name: &str) -> Option<String> {
    if prefix.is_empty() {
        return Some(name.to_owned());
    }
    let rest = name.strip_prefix(prefix)?;
    if rest.is_empty() {
        return Some(String::new());
    }
    rest.strip_prefix('/').map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{DriverHost, find_in, suffix_after_prefix};
    use crate::store::LocalStore;
    use nix_eval_rs::host::{Host, StoreError};
    use nix_eval_rs::task::SearchPathEntry;
    use nix_eval_rs::value2::PathValue;

    fn host() -> Result<DriverHost, String> {
        Ok(DriverHost::new(
            LocalStore::open("/nix/store", None, true)?,
            Vec::new(),
            true,
        ))
    }

    /// Every effect this driver cannot perform says so as `Unsupported`, and
    /// names itself.
    ///
    /// Both halves matter. `NoStore` would claim this host has no store,
    /// which is false and would be read as the driver being storeless rather
    /// than as one capability being absent. And a refusal that does not name
    /// the builtin leaves a gate log saying only that something was refused.
    #[test]
    fn every_refusal_is_unsupported_and_names_what_asked() -> Result<(), String> {
        let host = host()?;
        let mut checked: Vec<&str> = Vec::new();

        let mut expect = |label: &'static str, got: Result<String, StoreError>| match got {
            Err(StoreError::Unsupported(message)) => {
                if message.contains(label) {
                    checked.push(label);
                    Ok(())
                } else {
                    Err(format!("refusal for {label} does not name it: {message}"))
                }
            }
            Err(other) => Err(format!("{label} refused with {other:?}, not Unsupported")),
            Ok(answer) => Err(format!("{label} was answered with {answer}")),
        };

        expect(
            "copying '/etc/hostname'",
            host.copy_to_store(&PathValue::ambient("/etc/hostname")),
        )?;
        expect(
            "coercing '/etc/hostname'",
            host.store_path(&PathValue::ambient("/etc/hostname"))
                .map(|found| found.store_path),
        )?;
        expect(
            "validity of 2 store path(s)",
            host.valid_paths(&[String::from("/nix/store/a"), String::from("/nix/store/b")])
                .map(|held| format!("{held:?}")),
        )?;
        expect(
            "import from derivation",
            host.realise(&[]).map(|m| format!("{m:?}")),
        )?;
        expect(
            "builtins.getFlake",
            host.lock_flake("nixpkgs").map(|_| String::new()),
        )?;
        if checked.len() != 5 {
            return Err(format!("only checked {checked:?}"));
        }
        Ok(())
    }

    /// `ensure_path` answers under read-only and refuses a path it cannot
    /// make present under a writable store.
    ///
    /// Three cases, because the interesting ones are the two that are not
    /// "absent, so refuse":
    ///
    /// * read-only answers whatever the path, because cppnix's `ensurePath`
    ///   does not touch the store in that mode (`context.cc:275`) and this is
    ///   the embedder's half of it. Refusing here made `builtins.
    ///   appendContext` fail under `eval` where cppnix returns the string.
    /// * a path OUTSIDE the store directory is refused rather than looked up.
    ///   The old spelling fell through to `Path::join` with an absolute
    ///   argument, which replaces the base, so `/etc/passwd` was answered
    ///   `Ok` because /etc/passwd exists.
    /// * an absent path inside the store is refused, which is the ordinary
    ///   case.
    #[test]
    fn ensure_path_answers_read_only_and_refuses_what_it_cannot_reach() -> Result<(), String> {
        let read_only = host()?;
        if read_only.ensure_path("/nix/store/x").is_err() {
            return Err(String::from(
                "read-only ensure_path refused; cppnix answers without touching the store",
            ));
        }

        let root = std::env::temp_dir().join(format!("ned-ensure-{}", std::process::id()));
        let writable = DriverHost::new(
            LocalStore::open("/nix/store", Some(&root), false)?,
            Vec::new(),
            true,
        );
        match writable.ensure_path("/etc/passwd") {
            Err(StoreError::Unsupported(_)) => {}
            other => {
                let _ = std::fs::remove_dir_all(&root);
                return Err(format!("a path outside the store gave {other:?}"));
            }
        }
        match writable.ensure_path("/nix/store/definitely-not-here") {
            Err(StoreError::Unsupported(_)) => {}
            other => {
                let _ = std::fs::remove_dir_all(&root);
                return Err(format!("an absent store path gave {other:?}"));
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    /// The two effects the driver does perform are not refusals, and the path
    /// is the evaluator's own computation.
    #[test]
    fn text_and_derivation_writes_are_answered() -> Result<(), String> {
        let host = host()?;
        let file = host
            .store_text("f", "contents", &[])
            .map_err(|e| format!("toFile was refused: {e:?}"))?;
        let drv = host
            .write_derivation("d", "Derive([],[],[],\"s\",\"/bin/sh\",[],[])")
            .map_err(|e| format!("a .drv write was refused: {e:?}"))?;

        if file != nix_eval_rs::drvpath::text_store_path("/nix/store", "f", "contents", &[]) {
            return Err(format!("toFile landed on {file}"));
        }
        // The suffix is this host's to append; a `.drv` that came back
        // without one would mean the derivation and the store disagree about
        // the file's name, which moves the drvPath.
        if !drv.ends_with("-d.drv") {
            return Err(format!(
                "the .drv landed on {drv}, which is not named d.drv"
            ));
        }
        Ok(())
    }

    #[test]
    fn a_prefixed_entry_matches_only_its_prefix() {
        assert_eq!(
            suffix_after_prefix("nixpkgs", "nixpkgs/lib"),
            Some("lib".to_owned())
        );
        assert_eq!(
            suffix_after_prefix("nixpkgs", "nixpkgs"),
            Some(String::new())
        );
        assert_eq!(suffix_after_prefix("nixpkgs", "nixpkgsy"), None);
        assert_eq!(
            suffix_after_prefix("", "anything"),
            Some("anything".to_owned())
        );
    }

    /// A miss is `NotFound`, which `tryEval` can catch, and not `Failed`.
    #[test]
    fn a_missing_lookup_is_not_found_rather_than_a_failure() -> Result<(), String> {
        let entries = vec![SearchPathEntry {
            prefix: String::new(),
            path: "/nonexistent-search-root".to_owned(),
        }];
        match find_in(&entries, "nixpkgs") {
            Err(nix_eval_rs::host::LookupError::NotFound(_)) => Ok(()),
            other => Err(format!("expected NotFound, got {other:?}")),
        }
    }
}
