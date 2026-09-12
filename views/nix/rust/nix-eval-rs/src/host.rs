//! The evaluator's window onto the filesystem.
//!
//! The VM performs no IO: a builtin that needs a path suspends with
//! `Step::NeedPath` and the scheduler answers, so the machine's only state
//! stays the frame chain and a read is a plain return from `poll`. `Host` is
//! what the scheduler calls. Keeping it behind a trait is what lets the
//! effects kernel record a readset later without the evaluator changing, and
//! what lets tests answer paths without touching a disk.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::PoisonError;

use crate::value2::PathValue;

/// What a path turned out to be. Mirrors cppnix's `readFileType`, whose
/// spellings ("regular", "directory", "symlink", "unknown") are corpus-visible
/// through `builtins.readDir`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    Unknown,
}

impl FileType {
    pub fn as_str(self) -> &'static str {
        match self {
            FileType::Regular => "regular",
            FileType::Directory => "directory",
            FileType::Symlink => "symlink",
            FileType::Unknown => "unknown",
        }
    }
}

/// Why a path could not be turned into a store path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// Nothing behind this host owns a store. Reported as unimplemented and
    /// never as an evaluation failure: the expression is fine, this embedding
    /// simply cannot answer it, and calling that a Nix error would put a
    /// wrong message in front of a reader and a wrong outcome in the corpus.
    NoStore,
    /// The copy was attempted and failed; the text is what the evaluator
    /// reports, phrased the way cppnix phrases it.
    Failed(String),
    /// cppnix refused import from derivation. Kept apart from [`Failed`]
    /// because flake-show catches this exception class and prints an omitted
    /// node; flattening it into an evaluation error changes the document.
    ImportFromDerivation(String),
    /// The embedder could have answered and will not, because this backend
    /// cannot carry something the answer needs. Kept apart from `Failed` for
    /// the reason [`LookupError::Unsupported`] is kept apart from
    /// [`LookupError::Failed`]: it is a gap in this backend rather than a
    /// fault in the program, so it has to surface as unimplemented. As a Nix
    /// error it would score a mismatch against a cpp arm that answers fine.
    ///
    /// The one thing that raises it today is a tree fetch under the read-set
    /// tracker, whose per-attribute recording thunks this backend cannot
    /// carry.
    Unsupported(String),
}

impl std::fmt::Display for StoreError {
    /// `Failed` carries the text cppnix would print and shows bare; the other
    /// variants name themselves, each being a classification a caller matches
    /// on rather than a message written for a reader.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(why) => f.write_str(why),
            other => write!(f, "{other:?}"),
        }
    }
}

/// How [`FnHost`] reaches a store. The store belongs to the embedder, so the
/// embedder supplies the answer; a plain `fn` pointer because a Rust embedder
/// has nothing to capture. An embedder that does -- anything crossing the C
/// ABI -- carries its state on the context pointer of its own vtable instead
/// (`capi::IxeHostVtable`) rather than in a global.
pub type StoreCopyHook = fn(&PathValue) -> Result<String, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePathResult {
    pub path: String,
    pub store_path: String,
}

pub type StorePathHook = fn(&PathValue) -> Result<StorePathResult, String>;

/// How [`FnHost`] stores a text blob. Same shape and same reasoning as
/// [`StoreCopyHook`], with one extra: the embedder also owns the decision of
/// whether to *write* the bytes or only compute where they would go, because
/// that is `settings.readOnlyMode` and this evaluator cannot see it.
pub type StoreTextHook = fn(&str, &str, &[String]) -> Result<String, String>;

/// How [`FnHost`] writes a `.drv`. Same shape as [`StoreTextHook`], and the
/// embedder is expected to answer it with the same `addTextToStore` call,
/// because cppnix's `writeDerivation` is that call. It is a hook of its own
/// only because a missing one means something different -- see
/// [`Host::write_derivation`].
pub type WriteDrvHook = fn(&str, &str) -> Result<String, String>;

/// How [`FnHost`] performs a filtered copy into the store, for
/// `builtins.path`. Same shape and same reasoning as [`StoreTextHook`]: the
/// store is the embedder's, and so is the read-only decision.
pub type StoreFilteredHook = fn(&crate::task::FilteredCopy) -> Result<String, String>;

/// How [`FnHost`] fetches a URL into the store, for `builtins.fetchurl` and
/// `builtins.fetchTarball`. Same shape and same reasoning as
/// [`StoreFilteredHook`]: the download, the substituter lookup and the
/// tarball cache are all the embedder's, and so is the store the answer
/// names.
pub type FetchHook = fn(&crate::task::FetchRequest) -> Result<String, String>;

/// How [`FnHost`] fetches a tree, for `builtins.fetchTree` and
/// `builtins.fetchGit`. Same reasoning as [`FetchHook`], and the first hook
/// whose error side is a [`StoreError`] rather than a bare string: a tree
/// fetch is the first question an embedder can *decline* rather than fail
/// (see [`StoreError::Unsupported`]), and a string cannot say which of the
/// two happened. The older hooks keep the narrower type because none of them
/// has that third outcome; giving them one would be inventing a case.
pub type FetchTreeHook = fn(&crate::task::FetchTreeRequest) -> Result<String, StoreError>;

/// Everything `builtins.getFlake` needs to evaluate a flake's outputs, which
/// is exactly the three arguments cppnix's `callFlake` applies
/// `call-flake.nix` to.
///
/// The third of those three -- `fetchTreeFinal` -- is absent here because it
/// is not the embedder's to send: it is this crate's own `fetchFinalTree`
/// builtin, which the VM supplies. Sending it would mean the embedder handing
/// back a function, which this boundary cannot carry and should not.
#[derive(Debug, Clone)]
pub struct FlakeCall {
    /// `call-flake.nix`, verbatim. Supplied rather than embedded so the two
    /// backends run one copy; see [`crate::task::NeedPath::Flake`].
    pub source: String,
    /// The lock file, as the JSON text `call-flake.nix` calls `fromJSON` on.
    pub lock_file: String,
    /// The overrides document: one entry per lock node the embedder already
    /// fetched, each an `emitTreeAttrs` set plus a `dir`. JSON, with the
    /// `__storePath` escape for `outPath`, because a flake source path
    /// without its own context is a derivation input that has silently
    /// vanished.
    pub overrides: String,
}

/// One symlink under a store object, as [`Host::sealed_paths`] reports it:
/// its path relative to the object and its target text, verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symlink {
    pub path: String,
    pub target: String,
}

/// [`Host::sealed_paths`]'s answer: each sealed object (`<hash>-<name>`)
/// with every symlink under it. Which of them leave the object is the
/// verifier's reading (`readset::leaving_links`), not the host's.
pub type Links = std::collections::BTreeMap<String, Vec<Symlink>>;

/// The verifier's reading of [`Links`]: each sealed object with the symlinks
/// that LEAVE it, as paths relative to the object, so a read that stays
/// inside the object's bytes can be told from one that follows a link out
/// of them.
pub type Sealed = std::collections::BTreeMap<String, Vec<String>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportedSource {
    Nix { path: PathValue, text: String },
    /// Only the store host may classify an import as a valid derivation.
    /// Output strings are the store's static paths or downstream placeholders.
    Derivation(ImportedDerivation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedDerivation {
    pub path: String,
    pub name: String,
    pub outputs: BTreeMap<String, String>,
}

/// [`crate::value2::ContextElem`] with its `Rc<str>`s widened to `String`.
///
/// # Why this exists rather than sending the real thing
///
/// `ContextElem` is `Rc`-based and therefore `!Send`, and that is the rule
/// this whole seam obeys: nothing reference-counted leaves the thread that
/// owns the VM, because an `Rc` clone on two threads is a data race on a
/// non-atomic counter. It is the same rule that governs the answer side,
/// where every `Value` is built back on the VM thread.
///
/// A realise question is the one slow question whose payload is not already
/// plain owned data, so it is the one that needs a mirror. The worker rebuilds
/// the `Rc` form locally before calling the host: those `Rc`s are created,
/// used and dropped on that one thread and are never observed from another.
enum SendContextElem {
    Opaque(String),
    DrvDeep(String),
    Built { drv: String, output: String },
}

impl SendContextElem {
    fn of(elem: &crate::value2::ContextElem) -> Self {
        match elem {
            crate::value2::ContextElem::Opaque(path) => SendContextElem::Opaque(path.to_string()),
            crate::value2::ContextElem::DrvDeep(path) => SendContextElem::DrvDeep(path.to_string()),
            crate::value2::ContextElem::Built { drv, output } => SendContextElem::Built {
                drv: drv.to_string(),
                output: output.to_string(),
            },
        }
    }

    fn back(&self) -> crate::value2::ContextElem {
        match self {
            SendContextElem::Opaque(path) => {
                crate::value2::ContextElem::Opaque(path.as_str().into())
            }
            SendContextElem::DrvDeep(path) => {
                crate::value2::ContextElem::DrvDeep(path.as_str().into())
            }
            SendContextElem::Built { drv, output } => crate::value2::ContextElem::Built {
                drv: drv.as_str().into(),
                output: output.as_str().into(),
            },
        }
    }
}

/// A [`Host`] that answers the slow questions on other threads.
///
/// # What it is for
///
/// A fetch takes seconds and the evaluator has other roots it could be
/// running. Wrapping a host in this gives the scheduler in [`crate::eval`]
/// somewhere to put a fetch so that it stops being the only thing happening:
/// `begin` hands the question to a thread and returns immediately, the root
/// that asked is parked, and the machine goes and runs a sibling.
///
/// # Why it is opt-in rather than the default
///
/// Because whether the host behind it may be called from another thread is
/// not this crate's fact to assert. A host's effects land in the embedder's
/// own code -- for [`crate::capi::EmbedderHost`] that is a C function
/// pointer out of its session's vtable -- and a fetcher that is not
/// re-entrant would be broken by a wrapper that decided on its behalf.
/// So the bound is written out -- `H: Host + Send + Sync` -- and an embedder
/// opts in by naming it, which is the point at which someone has to know the
/// answer.
///
/// # Threads, not a pool
///
/// One thread per question in flight, and no pool, because the count is
/// already bounded by something small: a root has at most one open
/// suspension, so at most one thread exists per live root. A pool would add a
/// queue in front of a bound that is not being reached, and a queue is
/// exactly the stall this exists to remove.
pub struct ThreadedHost<H: Host + Send + Sync + 'static> {
    inner: std::sync::Arc<H>,
    next: std::sync::atomic::AtomicU64,
    /// Answers begun and not yet collected. A `Mutex` because the trait hands
    /// out `&self`; uncontended in practice, since only the evaluator's own
    /// thread ever calls `begin` or `collect`.
    inflight:
        std::sync::Mutex<std::collections::HashMap<u64, std::sync::mpsc::Receiver<SlowAnswer>>>,
}

impl<H: Host + Send + Sync + 'static> ThreadedHost<H> {
    pub fn new(inner: H) -> Self {
        ThreadedHost {
            inner: std::sync::Arc::new(inner),
            next: std::sync::atomic::AtomicU64::new(0),
            inflight: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The host behind this one, for a caller that has something to ask it
    /// synchronously.
    pub fn inner(&self) -> &H {
        &self.inner
    }

    /// Run `answer` on its own thread and file the receiver under a fresh
    /// ticket.
    fn spawn(&self, answer: impl FnOnce(&H) -> SlowAnswer + Send + 'static) -> Option<Ticket> {
        let id = self
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        let (tx, rx) = std::sync::mpsc::channel();
        let inner = std::sync::Arc::clone(&self.inner);
        // The send failing means the evaluation was abandoned and the
        // receiver dropped, which is not this thread's problem to report.
        std::thread::Builder::new()
            .name(format!("nix-eval-slow-{id}"))
            .spawn(move || drop(tx.send(answer(&inner))))
            .ok()?;
        let mut inflight = self
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inflight.insert(id, rx);
        Some(Ticket(id))
    }
}

impl<H: Host + Send + Sync + 'static> Host for ThreadedHost<H> {
    fn import_source(&self, path: &PathValue) -> Result<ImportedSource, String> {
        self.inner.import_source(path)
    }
    fn read_file(&self, path: &PathValue) -> Result<String, String> {
        self.inner.read_file(path)
    }
    fn read_file_bytes(&self, path: &PathValue) -> Result<Vec<u8>, String> {
        self.inner.read_file_bytes(path)
    }
    fn get_env(&self, name: &str) -> Option<String> {
        self.inner.get_env(name)
    }
    fn settle(&self) -> Result<(), StoreError> {
        self.inner.settle()
    }
    fn read_dir(&self, path: &PathValue) -> Result<Vec<(String, FileType)>, String> {
        self.inner.read_dir(path)
    }
    fn path_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
        self.inner.path_exists_checked(path)
    }
    fn dir_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
        self.inner.dir_exists_checked(path)
    }
    fn file_type(&self, path: &PathValue) -> Result<Option<FileType>, String> {
        self.inner.file_type(path)
    }
    fn file_type_resolved(&self, path: &PathValue) -> Result<FileType, String> {
        self.inner.file_type_resolved(path)
    }
    fn copy_to_store(&self, path: &PathValue) -> Result<String, StoreError> {
        self.inner.copy_to_store(path)
    }
    fn store_path(&self, path: &PathValue) -> Result<StorePathResult, StoreError> {
        self.inner.store_path(path)
    }
    fn ensure_path(&self, path: &str) -> Result<(), StoreError> {
        self.inner.ensure_path(path)
    }

    fn valid_paths(
        &self,
        paths: &[String],
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        self.inner.valid_paths(paths)
    }
    fn sealed_paths(&self, objects: &[String]) -> Result<Links, StoreError> {
        self.inner.sealed_paths(objects)
    }
    fn allow_paths(&self, paths: &[String]) -> Result<(), StoreError> {
        self.inner.allow_paths(paths)
    }
    fn allow_closures(&self, outputs: &[String]) -> Result<(), StoreError> {
        self.inner.allow_closures(outputs)
    }
    fn find_file(
        &self,
        entries: &[crate::task::SearchPathEntry],
        name: &str,
    ) -> Result<PathValue, LookupError> {
        self.inner.find_file(entries, name)
    }
    fn store_text(
        &self,
        name: &str,
        contents: &str,
        references: &[String],
    ) -> Result<String, StoreError> {
        self.inner.store_text(name, contents, references)
    }
    fn write_derivation(&self, name: &str, aterm: &str) -> Result<String, StoreError> {
        self.inner.write_derivation(name, aterm)
    }
    fn store_filtered(&self, request: &crate::task::FilteredCopy) -> Result<String, StoreError> {
        self.inner.store_filtered(request)
    }
    fn fetch(&self, request: &crate::task::FetchRequest) -> Result<String, StoreError> {
        self.inner.fetch(request)
    }
    fn lock_flake(&self, flake_ref: &str) -> Result<FlakeCall, StoreError> {
        self.inner.lock_flake(flake_ref)
    }
    fn parse_flake_ref(&self, flake_ref: &str) -> Result<String, StoreError> {
        self.inner.parse_flake_ref(flake_ref)
    }
    fn flake_ref_to_string(
        &self,
        attrs: &std::collections::BTreeMap<String, crate::task::TreeAttr>,
    ) -> Result<String, StoreError> {
        self.inner.flake_ref_to_string(attrs)
    }
    fn realise(
        &self,
        context: &[crate::value2::ContextElem],
    ) -> Result<BTreeMap<String, String>, StoreError> {
        self.inner.realise(context)
    }
    fn fetch_tree(&self, request: &crate::task::FetchTreeRequest) -> Result<String, StoreError> {
        self.inner.fetch_tree(request)
    }
    fn nix_path(&self) -> Result<Vec<crate::task::SearchPathEntry>, LookupError> {
        self.inner.nix_path()
    }
    fn trace(&self, message: &str) {
        self.inner.trace(message);
    }
    fn warn(&self, message: &str) {
        self.inner.warn(message);
    }

    fn begin(&self, question: &Slow<'_>) -> Option<Ticket> {
        match question {
            Slow::Fetch(request) => {
                let request = (*request).clone();
                self.spawn(move |h| SlowAnswer::Store(h.fetch(&request)))
            }
            Slow::FetchTree(request) => {
                let request = (*request).clone();
                self.spawn(move |h| SlowAnswer::Store(h.fetch_tree(&request)))
            }
            Slow::Flake(flake_ref) => {
                let flake_ref = (*flake_ref).to_owned();
                self.spawn(move |h| SlowAnswer::Flake(h.lock_flake(&flake_ref)))
            }
            Slow::Realise(context) => {
                let context: Vec<SendContextElem> =
                    context.iter().map(SendContextElem::of).collect();
                self.spawn(move |h| {
                    let context: Vec<crate::value2::ContextElem> =
                        context.iter().map(SendContextElem::back).collect();
                    SlowAnswer::Realise(h.realise(&context))
                })
            }
        }
    }

    fn collect(&self, ticket: Ticket, block: bool) -> Option<SlowAnswer> {
        let mut inflight = self
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rx = inflight.remove(&ticket.0)?;
        // Out of the map before waiting: holding the lock across a blocking
        // `recv` would make one slow answer block every other collect, which
        // is the stall this whole wrapper exists to remove.
        drop(inflight);
        let received = if block {
            rx.recv().ok()
        } else {
            rx.try_recv().ok()
        };
        match received {
            Some(answer) => Some(answer),
            None => {
                // Not ready. Put the receiver back so the next collect finds
                // it; a `recv` that failed while blocking means the worker
                // died without sending, and dropping the receiver here turns
                // the next collect into the "unknown ticket" case, which the
                // scheduler reports as a stuck evaluation rather than a hang.
                if !block {
                    let mut inflight = self
                        .inflight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    inflight.insert(ticket.0, rx);
                }
                None
            }
        }
    }
}

/// How [`FnHost`] locks a flake. Same shape and same reasoning as
/// [`FetchTreeHook`]: locking is the embedder's, and the third outcome
/// (`Unsupported`) is a real one here -- a standalone evaluator has no
/// `lockFlake` and must say so rather than invent a lock.
pub type FlakeHook = fn(&str) -> Result<FlakeCall, StoreError>;

/// How [`FnHost`] makes a store path present. Same shape and same reasoning
/// as [`StoreCopyHook`]: the store is the embedder's.
pub type StoreEnsureHook = fn(&str) -> Result<(), String>;

/// How a host realises a string context. Same shape and same reasoning
/// as [`StoreCopyHook`]: building a derivation is the embedder's, and so is
/// the `allow-import-from-derivation` setting that decides whether it may.
pub type RealiseHook =
    fn(&[crate::value2::ContextElem]) -> Result<BTreeMap<String, String>, StoreError>;

/// How [`FnHost`] reports a warning. Same shape and same reasoning as
/// [`StoreCopyHook`]: the logger, its verbosity and its formatting are the
/// embedder's.
pub type WarnHook = fn(&str);

/// Why a search path could not be resolved.
///
/// Three cases and not two, because they are three different outcomes for the
/// reader: nobody can answer, the answer is that there is no such file, and
/// the lookup itself went wrong. Only the middle one is a Nix-level error the
/// program can catch, and cppnix agrees -- `EvalState::findFile` raises a
/// `ThrownError` for a miss (`eval.cc:3413`), which `builtins.tryEval`
/// catches, and ordinary `Error`s for everything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupError {
    /// Nothing behind this host resolves search paths. Reported as
    /// unimplemented, never as an evaluation failure, for the reason
    /// [`StoreError::NoStore`] gives.
    NoResolver,
    /// No entry matched, with cppnix's own wording.
    NotFound(String),
    /// The lookup was attempted and failed.
    Failed(String),
    /// The lookup succeeded and this evaluator cannot use the answer. Kept
    /// apart from `Failed` because it is a gap in this backend rather than a
    /// fault in the program, so it has to be reported as unimplemented: as a
    /// Nix error it would score a mismatch against a cpp arm that answers
    /// fine.
    Unsupported(String),
}

/// How [`FnHost`] resolves a search path. Same shape and same reasoning as
/// [`StoreCopyHook`]: cppnix's `findFile` reaches fetchers, the `corepkgs`
/// accessor and its own access control, none of which live here.
pub type FindFileHook = fn(&[crate::task::SearchPathEntry], &str) -> Result<PathValue, LookupError>;

/// How [`FnHost`] learns the default search path, cppnix's
/// `builtins.nixPath`.
pub type NixPathHook = fn() -> Result<Vec<crate::task::SearchPathEntry>, LookupError>;

/// Files an embedder hands the evaluator by content, for paths it cannot
/// read off the filesystem.
///
/// cppnix resolves `<nix/fetchurl.nix>` into an in-memory accessor and
/// reports its path as `/fetchurl.nix`; there is no such file on disk. The
/// embedder's `findFile` answer carries the bytes beside that path, and the
/// host that received the answer serves every later question about the path
/// -- existence, kind, contents, and the atomic import -- from here, so
/// `builtins.toString <nix/fetchurl.nix>` is `/fetchurl.nix` on both arms
/// rather than a store path on one of them. ENG-12607.
///
/// Owned by the host that received the answer and dropped with it: two
/// sessions in one process cannot see each other's files, and a later
/// session cannot read bytes an earlier one was handed. The overlay used to
/// be a process-global and the atomic import did not consult it, which is how
/// `import <nix/fetchurl.nix>` refused under pure evaluation while every
/// other read of the same path answered.
///
/// Registered content wins over the filesystem, which is also cppnix's
/// order: its `findFile` short-circuits `nix/...` to `corepkgs` before it
/// consults the search path at all (`eval.cc:3445`). Only an ambient path
/// can be one; a mounted root is a real accessor.
///
/// A read set does not record these on their own: the bytes are compiled
/// into the embedder, and the `findFile` question that carried them is what
/// a replay re-asks, in order, so the registration precedes the reads that
/// depend on it exactly as it did when recorded.
///
/// `read_dir` has no counterpart on purpose: a registered file is a file, and
/// a directory listing has never been answerable from one.
#[derive(Clone, Default, Debug)]
pub struct VirtualFiles(std::sync::Arc<std::sync::Mutex<BTreeMap<String, String>>>);

impl VirtualFiles {
    /// Register `contents` for the ambient `path`. Last writer wins, which
    /// is what makes registration idempotent for an embedder that resolves
    /// the same lookup twice.
    pub fn insert(&self, path: &str, contents: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_owned(), contents.to_owned());
    }

    /// The contents registered for `path`, if it is an ambient path with one.
    fn get(&self, path: &PathValue) -> Option<String> {
        if !matches!(&path.root, crate::value2::Root::Ambient) {
            return None;
        }
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path.as_ref())
            .cloned()
    }

    /// Answer a file read out of the registered files when one matches.
    pub(crate) fn read_file_or(
        &self,
        path: &PathValue,
        ask: impl FnOnce() -> Result<String, String>,
    ) -> Result<String, String> {
        match self.get(path) {
            Some(contents) => Ok(contents),
            None => ask(),
        }
    }

    /// As [`VirtualFiles::read_file_or`], for the raw-bytes read. Registered
    /// content is a `String`, so its bytes are its UTF-8 encoding; every
    /// caller registers Nix source text.
    pub(crate) fn read_file_bytes_or(
        &self,
        path: &PathValue,
        ask: impl FnOnce() -> Result<Vec<u8>, String>,
    ) -> Result<Vec<u8>, String> {
        match self.get(path) {
            Some(contents) => Ok(contents.into_bytes()),
            None => ask(),
        }
    }

    /// The atomic import of a registered file is the file itself: a regular
    /// file resolves to its own path, with these bytes.
    pub(crate) fn import_source_or(
        &self,
        path: &PathValue,
        ask: impl FnOnce() -> Result<ImportedSource, String>,
    ) -> Result<ImportedSource, String> {
        match self.get(path) {
            Some(text) => Ok(ImportedSource::Nix {
                path: path.clone(),
                text,
            }),
            None => ask(),
        }
    }

    /// A registered file exists, whatever the filesystem or the embedder
    /// says. `import` asks this before it reads.
    pub(crate) fn path_exists_checked_or(
        &self,
        path: &PathValue,
        ask: impl FnOnce() -> Result<bool, String>,
    ) -> Result<bool, String> {
        match self.get(path) {
            Some(_) => Ok(true),
            None => ask(),
        }
    }

    /// The trailing-slash directory predicate: registered content is a
    /// regular file, so it exists but is not a directory. Answering `true`
    /// here would make `pathExists "<virtual>/"` disagree with the kind
    /// query import uses.
    pub(crate) fn dir_exists_checked_or(
        &self,
        path: &PathValue,
        ask: impl FnOnce() -> Result<bool, String>,
    ) -> Result<bool, String> {
        match self.get(path) {
            Some(_) => Ok(false),
            None => ask(),
        }
    }

    /// Registered content is a regular file, which is what decides whether
    /// `import` appends `/default.nix` ([`Host::resolve_import`]).
    ///
    /// Two functions and not one for the two kind queries: [`Host::file_type`]
    /// is `maybeLstat` and can answer "not there", [`Host::file_type_resolved`]
    /// is still `lstat` and cannot. Resolution has nothing to do with it --
    /// neither changes the answer for content held in memory -- so what the
    /// split records is the difference in contract.
    pub(crate) fn file_type_or(
        &self,
        path: &PathValue,
        ask: impl FnOnce() -> Result<Option<FileType>, String>,
    ) -> Result<Option<FileType>, String> {
        match self.get(path) {
            Some(_) => Ok(Some(FileType::Regular)),
            None => ask(),
        }
    }

    /// [`VirtualFiles::file_type_or`] for the resolving query, which reports
    /// a missing path as a failure because its caller is cppnix's `lstat`.
    pub(crate) fn file_type_resolved_or(
        &self,
        path: &PathValue,
        ask: impl FnOnce() -> Result<FileType, String>,
    ) -> Result<FileType, String> {
        match self.get(path) {
            Some(_) => Ok(FileType::Regular),
            None => ask(),
        }
    }
}

/// How [`FnHost`] prints a trace line. Same shape and same reasoning as
/// [`WarnHook`]: the sink and its formatting are the embedder's.
pub type TraceHook = fn(&str);

/// What `builtins.readDir` answers with: each entry's name and its type.
///
/// A named alias because the hook's type is otherwise nested three deep and
/// reads as noise at every site that mentions it.
pub type ReadDirFn = fn(&PathValue) -> Result<Vec<(String, FileType)>, String>;

/// The plain filesystem reads, answered by the embedder instead of by
/// `std::fs`.
///
/// One struct rather than independent hooks, and that is the whole point.
/// [`crate::purity`] decides whether `pure-eval` and `restrict-eval` can be
/// honoured by asking one question -- does a read go through cppnix's
/// `rootFS` or not -- and it has to be able to answer it for the group.
/// Separate fields would let an embedder supply four of five, leaving one
/// question silently reading the world outside the allow list while the table
/// said the setting was being honoured. There is no way to spell that here.
///
/// # Why `file_type` and `file_type_resolved` are two hooks
///
/// Because cppnix asks two different questions and gets two different
/// answers. `prim_readFileType` is the one primop that passes `std::nullopt`
/// to `realisePath` (`primops.cc:2492`), so it resolves nothing: on a path
/// with a symlinked *ancestor* it does not answer at all, it raises
/// `SymlinkNotAllowed` from the accessor. `import` resolves, in
/// `resolveExprPath` rather than in `realisePath` (`eval.cc:3423`), and its
/// directory test is `path.resolveSymlinks().lstat().type == tDirectory`
/// (`eval.cc:3440`) -- full resolution, then the type.
///
/// Serving both from one `lstat` hook is what ENG-12871 was: `import
/// a/symlinked-dir/f.nix` raised "path 'a/symlinked-dir' is a symlink" on
/// this backend and evaluated fine on cppnix. Merging them the other way, by
/// making the one hook resolve, breaks `builtins.readFileType` on the same
/// path, where cppnix raises and so must this.
#[derive(Debug, Clone, Copy)]
pub struct PathReadHooks {
    /// `builtins.readFile`, and the second half of an `import`.
    pub read_file: fn(&PathValue) -> Result<String, String>,
    /// `builtins.pathExists`. Missing paths and cppnix's
    /// `RestrictedPathError` are `Ok(false)`; a vanished mounted root and
    /// every other accessor or I/O failure stay `Err`. Keeping that third
    /// outcome is required by read-set replay: disappearance must not look
    /// like the same negative observation as an absent child.
    pub path_exists: fn(&PathValue) -> Result<bool, String>,
    /// The string-with-trailing-slash form of `builtins.pathExists`: full
    /// symlink resolution followed by a directory test. Missing paths and
    /// `RestrictedPathError` are `Ok(false)`; other failures stay `Err`.
    pub dir_exists: fn(&PathValue) -> Result<bool, String>,
    /// `builtins.readDir`.
    pub read_dir: ReadDirFn,
    /// `builtins.readFileType`: `lstat`, resolving nothing, not even
    /// ancestors. `Ok(None)` is cppnix's `maybeLstat` answering nullopt; see
    /// [`Host::file_type`].
    pub file_type: fn(&PathValue) -> Result<Option<FileType>, String>,
    /// The first half of an `import`: `stat`, ie the type of the path with
    /// every symlink in it resolved.
    pub file_type_resolved: fn(&PathValue) -> Result<FileType, String>,
}

/// Every question the evaluator can ask of the world outside it. Errors are
/// the message text the evaluator reports, so implementations phrase them the
/// way cppnix does rather than leaking an OS string.
///
/// # Exactly two methods here have bodies, and both are load-bearing
///
/// A defaulted method is a method a wrapper can forget to forward while still
/// compiling, and a wrapper that forgets one answers a question the host
/// behind it was never asked. This trait has three forwarding wrappers --
/// [`ThreadedHost`], [`crate::readset::RecordingHost`], and `&T` below -- and
/// the failure is silent at every one of them.
///
/// It has happened three times. `RecordingHost` inherited `ensure_path` and
/// `warn`, so with `eval-cache-dir` set, `builtins.appendContext` failed with
/// "no store behind this evaluator" against a host that had one, and every
/// warning was dropped (ENG-12555). `find_file`, `nix_path` and `trace`
/// arrived defaulted straight afterwards; `RecordingHost` happened to forward
/// them, but nothing made it. Then `&T` arrived on one branch while defaulted
/// `begin` and `collect` arrived on another, neither branch could see the
/// other, and the merge compiled clean with `&T` inheriting both -- which
/// meant every evaluation behind a `&`-wrapped host quietly went back to
/// blocking, a performance regression that looks exactly like correct code
/// (ENG-13107).
///
/// Two guard tests were written for that class, one per wrapper, each with
/// its own list of method names to remember. That is the same drift risk one
/// level up: adding a defaulted method meant remembering two test lists and
/// three impls, and nobody was going to.
///
/// So there are no defaults left to inherit. Every effect is bodiless, and a
/// wrapper that misses one does not compile -- at the wrapper, by name, with
/// no test involved. **This is the property to preserve: adding a method with
/// a body to this trait re-opens the class for all three wrappers at once.**
/// `the_trait_has_no_default_bodies_to_inherit` refuses one, because the one
/// thing the compiler cannot catch is the default that is never written.
///
/// # What that gave up, and where the convenience went
///
/// The seven store effects were defaulted for a real reason: a leaf host with
/// no store is cppnix's `readOnlyMode`, every test host in this crate is one,
/// and `Err(NoStore)` is the honest answer for all of them. Bodiless methods
/// move that cost onto every leaf -- about fifty of them here.
///
/// The convenience moved rather than disappearing: `host_stubs!` writes the
/// same refusals by name, so a leaf spells `host_stubs!(fetch, store_text)`
/// where it used to spell nothing. The difference is that the leaf now *says*
/// which questions it is refusing, in one greppable line, and a wrapper --
/// which must never reach for that macro -- has nothing to inherit. The
/// asymmetry is the whole point: a leaf saying "no store here" is telling the
/// truth, and a wrapper saying it is lying about the host behind it.
///
/// The alternative considered was generating all three forwarding impls from
/// one macro over a single enumeration of the trait. It fixes the same drift
/// and costs less churn, but it keeps the defaults, so a *leaf* can still
/// inherit one silently and the macro's own method list becomes the thing to
/// remember. Bodiless methods need nothing remembered.
///
/// [`Host::resolve_import`] is the one exception and keeps a body, because it
/// is not an effect. It is defined in terms of [`Host::file_type_resolved`],
/// so a wrapper that forwards the effects gets the derived answer right for
/// free -- and `RecordingHost` specifically must *not* forward it, because
/// forwarding would run the lookup on the inner host where nothing records
/// it.
/// A question whose answer comes from the network or a store rather than from
/// the local filesystem, and which a host may therefore want to begin now and
/// finish later.
///
/// # Why this is a separate enum and not `&NeedPath`
///
/// Because it is a promise, not a description. Every variant here is a
/// question the evaluator is willing to have answered *out of step with the
/// rest of its work*: it will park the root that asked, run something else,
/// and come back. `NeedPath` has thirty-odd variants and most of them are a
/// `stat` -- beginning those asynchronously would cost a thread apiece to
/// save nothing, and `NeedPath::Entries` in particular is answered from
/// `crate::eval`'s directory cache more often than it reaches a host at all.
///
/// Adding a variant here is therefore a decision about latency, and it has to
/// be written down rather than inferred from a question's shape.
#[derive(Debug)]
pub enum Slow<'a> {
    /// [`Host::fetch`].
    Fetch(&'a crate::task::FetchRequest),
    /// [`Host::fetch_tree`].
    FetchTree(&'a crate::task::FetchTreeRequest),
    /// [`Host::lock_flake`].
    Flake(&'a str),
    /// [`Host::realise`]. The one that *builds*, so the one with most to
    /// gain: cppnix's import from derivation blocks the whole evaluation on
    /// a build, and this is the seam that stops it doing so here.
    Realise(&'a [crate::value2::ContextElem]),
}

/// Names one slow answer a host has begun and has not handed back yet.
///
/// Opaque to the evaluator: the host mints it, the evaluator quotes it back.
/// Nothing here interprets the number, so a host is free to make it an index
/// into a table, a file descriptor or a pointer's low bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ticket(pub u64);

/// What a begun slow question turned into.
///
/// The shapes are exactly the return types of the three blocking methods, so
/// a host implements one answer path and exposes it twice rather than two
/// that can disagree.
#[derive(Debug)]
pub enum SlowAnswer {
    /// [`Slow::Fetch`]: a store path. [`Slow::FetchTree`]: the JSON.
    Store(Result<String, StoreError>),
    /// [`Slow::Flake`].
    Flake(Result<FlakeCall, StoreError>),
    /// [`Slow::Realise`]: the rewrites the build produced.
    Realise(Result<BTreeMap<String, String>, StoreError>),
}

pub trait Host {
    fn import_source(&self, path: &PathValue) -> Result<ImportedSource, String> {
        let path = self.resolve_import(path)?;
        let text = self.read_file(&path)?;
        Ok(ImportedSource::Nix { path, text })
    }
    fn read_file(&self, path: &PathValue) -> Result<String, String>;

    /// The raw bytes of `path`: what [`Host::read_file`] answers, before any
    /// text decoding. `builtins.hashFile` digests these, and a digest of
    /// UTF-8-repaired bytes is a digest of a file that does not exist
    /// (ENG-13146). A second method beside `read_file` rather than a change
    /// to it because the string answer is what `Value::Str` can carry today;
    /// collapsing the two is the byte-string redesign (ENG-13147).
    fn read_file_bytes(&self, path: &PathValue) -> Result<Vec<u8>, String>;

    /// An environment variable, or `None` when it is unset. cppnix renders an
    /// unset variable as the empty string, and the caller does that; keeping
    /// unset distinguishable here is what lets a read set record "was unset"
    /// rather than "was empty", which are different facts to re-check.
    fn get_env(&self, name: &str) -> Option<String>;
    fn read_dir(&self, path: &PathValue) -> Result<Vec<(String, FileType)>, String>;
    /// The three-outcome existence question. `Ok(false)` means the accessor
    /// answered absent (or cppnix deliberately hid a restricted path); `Err`
    /// means the question itself failed and must remain distinct in replay.
    /// There is deliberately no Boolean form: one that collapsed `Err` to
    /// `false` let a vanished mount replay as "still absent", and test builds
    /// derived this contract from that lossy spelling.
    fn path_exists_checked(&self, path: &PathValue) -> Result<bool, String>;
    /// The trailing-slash form of `builtins.pathExists`. This is distinct from
    /// [`Host::file_type_resolved`] because a restricted path is `false` here,
    /// while every other resolver failure remains an error.
    fn dir_exists_checked(&self, path: &PathValue) -> Result<bool, String>;

    /// The type of `path` itself, resolving no symlink, not even one in an
    /// ancestor -- cppnix's `SourceAccessor::maybeLstat`.
    ///
    /// `builtins.readFileType` is the caller this is shaped for: cppnix's
    /// `prim_readFileType` is the only primop that passes `std::nullopt` to
    /// `realisePath` (`primops.cc:2492`), so a symlinked ancestor is a
    /// `SymlinkNotAllowed` error there and has to be `Err` here.
    ///
    /// # `Ok(None)` is "the accessor has no such path", and is not a failure
    ///
    /// cppnix's accessor primitive is `maybeLstat`; `lstat` is that plus
    /// `throw FileNotFound` (`source-accessor.cc:73`). Answering `Err` for a
    /// path the accessor cannot see collapses the two and puts the throw in
    /// the embedder, where the caller can no longer decline it -- which is
    /// exactly the bug in ENG-13123. Under pure eval `rootFS` is a mounted
    /// accessor knowing only `/nix/store`, so `/nix` -- an ancestor of every
    /// store path -- reads as absent, and a `builtins.path` filter walk
    /// asking about it died on an error cppnix never raises.
    ///
    /// So absence comes back as a value and the two callers decide:
    /// [`crate::task::NeedPath::Kind`] turns it into cppnix's
    /// `path '%s' does not exist`, [`crate::task::NeedPath::MaybeKind`]
    /// hands it on as `null`.
    ///
    /// `Err` keeps its meaning: the read was refused or went wrong. A
    /// `RestrictedPathError`, a `SymlinkNotAllowed`, an unreadable directory.
    /// Do not fold one of those into `Ok(None)`; a refused read that reads as
    /// "not there" is a silently wrong answer.
    fn file_type(&self, path: &PathValue) -> Result<Option<FileType>, String>;

    /// The type of `path` with every symlink resolved: `stat` where
    /// [`Host::file_type`] is `lstat`.
    ///
    /// Asked only by [`Host::resolve_import`], and separate from `file_type`
    /// because cppnix's `import` resolves where its `readFileType` does not.
    /// See [`PathReadHooks`] for which cppnix line says which.
    fn file_type_resolved(&self, path: &PathValue) -> Result<FileType, String>;

    /// The store path a path coerces to when it appears inside a string.
    ///
    /// cppnix coerces such a path with `copyToStore = true`, which copies the
    /// file or directory into the store and yields the store path with that
    /// path in the string's context (`src/libexpr/eval.cc:2582`). So
    /// `"${./f}"` is a store path, and returning the source path instead is a
    /// wrong answer rather than a missing feature.
    ///
    /// Nothing in this crate can produce that itself. Under read-only mode --
    /// which `nix-instantiate --eval` turns on -- the answer is the path the
    /// copy *would* produce and no bytes move, and reimplementing that half of
    /// cppnix's store here would be a second implementation to keep in step.
    ///
    /// A host with no store behind it answers [`StoreError::NoStore`] rather
    /// than inventing a path.
    fn copy_to_store(&self, path: &PathValue) -> Result<String, StoreError>;
    #[cfg(not(test))]
    fn store_path(&self, path: &PathValue) -> Result<StorePathResult, StoreError>;
    #[cfg(test)]
    fn store_path(&self, _path: &PathValue) -> Result<StorePathResult, StoreError> {
        Err(StoreError::NoStore)
    }

    /// Make a store path present, substituting or building it if it is not.
    ///
    /// cppnix's `Store::ensurePath`, which `builtins.appendContext` calls for
    /// every key it is handed -- but only when `readOnlyMode` is off
    /// (`context.cc:275`). Whether it is on is the embedder's setting, so the
    /// embedder's implementation is where that branch belongs; a host that
    /// answers `Ok(())` because nothing had to happen is telling the truth.
    ///
    /// A host with no store behind it answers [`StoreError::NoStore`] for the
    /// same reason [`Host::copy_to_store`] does: better to say so than to let
    /// a key nobody validated into a string's context.
    fn ensure_path(&self, path: &str) -> Result<(), StoreError>;

    /// Which of these store paths the store holds now: cppnix's
    /// `Store::queryValidPaths` over the list, plus every path at which the
    /// evaluator has a tree mounted (`rustValidPaths`).
    ///
    /// Asked by the witness verifier, not by an evaluation: once as a batch
    /// for the paths the rows name, then of one path at each row that needs
    /// a path the batch did not hold (`readset::Validity::holds`: a
    /// derivation is held only from the row that writes it). Presence is
    /// asked of what a served result hands out (derivations, copies,
    /// realisations), never of a tree a row read under: a sealed tree's
    /// record stands whether or not the store still holds it. A `WriteDrv`
    /// row replays as "still there" for a held path and rewrites only the
    /// rest, where re-asking the write for each of a witness's 23k
    /// derivations was 23k parses and 23k round trips on a hit. A host that
    /// cannot say (no store) answers [`StoreError::NoStore`]: the batch
    /// fails closed and no late question is asked, so every row asks as
    /// before.
    ///
    /// Bodiless in production, as every effect here is, so a forwarding
    /// wrapper that skips it does not compile; the `cfg(test)` default is
    /// for the fake hosts, as `store_path`'s is.
    #[cfg(not(test))]
    fn valid_paths(
        &self,
        paths: &[String],
    ) -> Result<std::collections::BTreeSet<String>, StoreError>;
    #[cfg(test)]
    fn valid_paths(
        &self,
        _paths: &[String],
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        Err(StoreError::NoStore)
    }

    /// Which of these store objects (`<hash>-<name>`) are sealed -- held by
    /// the store and content-addressed, so the name pins the bytes, link
    /// text included -- and for each, the symlinks that leave it (a target
    /// resolving outside the object), as object-relative paths.
    ///
    /// Asked by the witness verifier, once per object it has not recorded
    /// as sealed (`readset::DirSealed`); a sealed object is a permanent fact
    /// about immutable bytes and is never asked again. Under it, every read
    /// row replays from the record as one under a mounted root does
    /// (`Question::replays_from_record`), except a read at or below a
    /// leaving link, the one read that can reach bytes the name does not
    /// pin. An object the store does not hold is simply absent from the
    /// answer. A host that cannot say (no store) answers
    /// [`StoreError::NoStore`], and every row asks as before.
    ///
    /// Bodiless in production for the reason `valid_paths` is.
    #[cfg(not(test))]
    fn sealed_paths(&self, objects: &[String]) -> Result<Links, StoreError>;
    #[cfg(test)]
    fn sealed_paths(&self, _objects: &[String]) -> Result<Links, StoreError> {
        Err(StoreError::NoStore)
    }

    /// Allow reads through these store paths (cppnix's `allowPath`), for a
    /// copy, a pinned fetch or a locked tree the witness verifier served by
    /// validity instead of re-running. Every live hook for those ends by
    /// allowing the path it produced -- the path alone, not what it
    /// references -- and under pure or restricted evaluation the root
    /// accessor is an allow list, so the rows recorded after the effect read
    /// through its object; without this they are refused on replay and every
    /// hit is a miss. A host with no store answers [`StoreError::NoStore`],
    /// and the verifier asks the effect again instead.
    ///
    /// Bodiless in production for the reason `valid_paths` is.
    #[cfg(not(test))]
    fn allow_paths(&self, paths: &[String]) -> Result<(), StoreError>;
    #[cfg(test)]
    fn allow_paths(&self, _paths: &[String]) -> Result<(), StoreError> {
        Err(StoreError::NoStore)
    }

    /// Allow reads through these realised output paths and their closures
    /// (cppnix's `allowClosure`, primops.cc:185; the third phase of
    /// [`Host::realise`] on its own), for a realisation the verifier served
    /// by validity. The closure form is what `realiseContext` grants, and
    /// only for outputs: a copy served by validity goes through
    /// [`Host::allow_paths`], since the live copy never allowed its
    /// references. Failure means the row asks.
    ///
    /// Bodiless in production for the reason `valid_paths` is.
    #[cfg(not(test))]
    fn allow_closures(&self, outputs: &[String]) -> Result<(), StoreError>;
    #[cfg(test)]
    fn allow_closures(&self, _outputs: &[String]) -> Result<(), StoreError> {
        Err(StoreError::NoStore)
    }

    /// Realise a string's context and answer with the rewrites it produced:
    /// cppnix's `EvalState::realiseContext` (`primops.cc:72`), and so this
    /// evaluator's import from derivation.
    ///
    /// The whole of what the embedder owes is on
    /// [`crate::task::NeedPath::Realise`], including the two settings that
    /// live on this side of the call and nowhere else --
    /// `allow-import-from-derivation` and `trace-import-from-derivation`.
    ///
    /// A leaf host with no store answers [`StoreError::NoStore`], and the
    /// consequence is the one that matters most here: without a store there
    /// is no build, so a read through an unbuilt derivation output is a named
    /// refusal rather than a read of a path that does not exist. It says that
    /// with a body of its own -- see the note on this trait -- or by reaching
    /// for `host_stubs!(realise)`.
    fn realise(
        &self,
        context: &[crate::value2::ContextElem],
    ) -> Result<BTreeMap<String, String>, StoreError>;

    /// Resolve a search path lookup: `entries` is the list to walk and
    /// `name` the file sought, so `<nixpkgs>` arrives here as the value of
    /// `__nixPath` and `"nixpkgs"`.
    ///
    /// cppnix's `EvalState::findFile` (`eval.cc:3386`) is the whole
    /// implementation and it is not a walk this crate could do instead. It
    /// resolves each entry through `resolveLookupPathPath`, which downloads a
    /// pseudo-URL entry into the store, consults the registered lookup-path
    /// hooks for a `scheme:rest` entry, resolves symlinks, and applies the
    /// evaluator's access control; it also falls back to the in-memory
    /// `corepkgs` accessor for a name starting `nix/`, which is what makes
    /// `<nix/fetchurl.nix>` resolve on a machine with no such file anywhere.
    ///
    /// A host nobody has taught about a search path answers
    /// [`LookupError::NoResolver`], rather than reporting every lookup as a
    /// miss -- which would be a wrong answer for the same reason returning a
    /// source path from [`Host::copy_to_store`] was.
    fn find_file(
        &self,
        entries: &[crate::task::SearchPathEntry],
        name: &str,
    ) -> Result<PathValue, LookupError>;

    /// Store `contents` under a name ending in `name`, with `references`, and
    /// answer with the store path. `builtins.toFile`.
    ///
    /// A leaf host with no store answers [`StoreError::NoStore`] for the
    /// reason [`Host::copy_to_store`] does: a computed path that nobody wrote
    /// is a wrong answer wherever the caller then expects the file to be
    /// there, and the evaluator cannot tell whether this process is going to
    /// write.
    fn store_text(
        &self,
        name: &str,
        contents: &str,
        references: &[String],
    ) -> Result<String, StoreError>;

    /// Write a finished `.drv` and answer with the store path it landed on.
    /// `builtins.derivationStrict`.
    ///
    /// The answer is cppnix's `writeDerivation`, which is [`Host::store_text`]
    /// of the ATerm under `{name}.drv`, and it must be a path a store would
    /// compute for these bytes rather than one taken from the request, since
    /// agreeing with the evaluator is the whole point of returning it. A host
    /// need not have performed the write when it answers: the path is a
    /// function of the bytes, and the bridge host computes it here and hands
    /// the derivations to the store in batches before anything could observe
    /// one missing (`capi::EmbedderHost::flush_derivations`). A write that
    /// then fails fails the evaluation, not silently the store.
    ///
    /// Asked once per derivation per evaluation: the VM answers a repeat of
    /// the same bytes itself (`Vm::note_drv_written`).
    ///
    /// A leaf host with no store answers [`StoreError::NoStore`], and here
    /// that refusal costs less than it does for [`Host::store_text`]: the
    /// caller is not asking where the bytes go -- it computed that from these
    /// same bytes before asking -- but only that they be put there. A host
    /// with no store leaves them unwritten, which is precisely what cppnix
    /// does under `readOnlyMode`, and the derivation's value is unchanged
    /// either way. See [`crate::task::NeedPath::WriteDrv`].
    fn write_derivation(&self, name: &str, aterm: &str) -> Result<String, StoreError>;

    /// Copy a filtered tree into the store and answer with its store path.
    /// `builtins.path`, and every `lib.cleanSource` spelled through it.
    ///
    /// The evaluator has already done the walk and applied the filter, because
    /// the filter is a Nix function; what arrives is a finished file list and
    /// the guarantees [`crate::task::NeedPath::StoreFiltered`] spells out.
    ///
    /// A leaf host with no store answers [`StoreError::NoStore`] for exactly
    /// the reason [`Host::store_text`] does.
    fn store_filtered(&self, request: &crate::task::FilteredCopy) -> Result<String, StoreError>;

    /// Fetch a URL into the store and answer with its store path.
    /// `builtins.fetchurl` and `builtins.fetchTarball`.
    ///
    /// The evaluator has already read the argument set, defaulted the name
    /// and validated it; what arrives is the request
    /// [`crate::task::NeedPath::Fetch`] describes, whose guarantees run both
    /// ways -- it says what the evaluator promises and what the answer must
    /// be.
    ///
    /// A leaf host with no store answers [`StoreError::NoStore`] for the
    /// reason [`Host::store_text`] does.
    fn fetch(&self, request: &crate::task::FetchRequest) -> Result<String, StoreError>;

    /// Lock a flake reference and answer with the three documents
    /// `call-flake.nix` needs. `builtins.getFlake`.
    ///
    /// See [`crate::task::NeedPath::Flake`] for what the evaluator has already
    /// decided and what it has deliberately left here. A leaf host with no
    /// store answers [`StoreError::NoStore`] for the reason [`Host::fetch`]
    /// does.
    fn lock_flake(&self, flake_ref: &str) -> Result<FlakeCall, StoreError>;

    /// Parse a flake reference and answer with the JSON of its exploded
    /// attribute form. `builtins.parseFlakeRef`.
    ///
    /// See [`crate::task::NeedPath::ParseFlakeRef`] for why the grammar is
    /// the embedder's. The flakes feature gate is the embedder's too, checked
    /// where the work is: cppnix registers the primop unconditionally and
    /// raises the feature-is-disabled error on call, so the hook does the
    /// same. A leaf host with no embedder answers [`StoreError::NoStore`]
    /// for the reason [`Host::lock_flake`] does.
    fn parse_flake_ref(&self, flake_ref: &str) -> Result<String, StoreError>;

    /// Print a flake reference's attribute form as its URL form.
    /// `builtins.flakeRefToString`.
    ///
    /// See [`crate::task::NeedPath::FlakeRefToString`] for the division of
    /// labour; the gate is the embedder's as for [`Host::parse_flake_ref`].
    fn flake_ref_to_string(
        &self,
        attrs: &std::collections::BTreeMap<String, crate::task::TreeAttr>,
    ) -> Result<String, StoreError>;

    /// Fetch a tree and answer with the JSON of the attribute set cppnix's
    /// `emitTreeAttrs` builds. `builtins.fetchTree` and `builtins.fetchGit`.
    ///
    /// See [`crate::task::NeedPath::FetchTree`] for what the evaluator has
    /// already decided and what it has deliberately left here. A leaf host
    /// with no store answers [`StoreError::NoStore`] for the reason
    /// [`Host::fetch`] does.
    fn fetch_tree(&self, request: &crate::task::FetchTreeRequest) -> Result<String, StoreError>;

    /// The default search path, cppnix's `builtins.nixPath`: the `-I` flags
    /// and `NIX_PATH` the embedder was started with, in order.
    fn nix_path(&self) -> Result<Vec<crate::task::SearchPathEntry>, LookupError>;

    /// The evaluation is finished (value or error): make every effect the
    /// host deferred on its behalf visible before the value leaves the
    /// evaluator. The scheduler calls it once per finished job
    /// (`eval::drive_concurrent`), and that is the only crossing there is, so
    /// a host that answers a store write from the bytes and queues the write
    /// (`capi::EmbedderHost::write_derivation`) has one place to flush and
    /// every route into an evaluation is covered by construction. No default
    /// body, like every other effect here: a leaf that defers nothing says
    /// `Ok(())` (`host_stubs!(settle)` in tests), and a wrapper forwards,
    /// because a wrapper that inherited a no-op would swallow the deferred
    /// writes of the host behind it and compile.
    fn settle(&self) -> Result<(), StoreError>;

    /// Print a trace line, as `builtins.trace` does.
    ///
    /// An output rather than a question, exactly like [`Host::warn`], and
    /// dropping one is a presentation divergence rather than a wrong value.
    /// It is still routed, because a builtin writing to stderr itself would
    /// make the "the VM performs no IO" claim in this module false.
    ///
    /// A host with nowhere to write drops it, which is not an error and loses
    /// no answer -- but it says so with an empty body of its own rather than
    /// by inheriting one.
    fn trace(&self, message: &str);

    /// Report a warning, as cppnix's `warn()` does.
    ///
    /// A warning is an output the evaluator produces and cannot answer for:
    /// the logger, its verbosity threshold and its formatting all belong to
    /// the embedder. Dropping one is not a wrong value, but it is a silent
    /// divergence -- cppnix warns about six derivation attributes that
    /// `__structuredAttrs` quietly disables, and a backend that stays quiet
    /// there is telling the reader less than cppnix does about a derivation
    /// that will not do what it says.
    ///
    /// A host with nowhere to write drops it, which is not an error and loses
    /// no answer -- but it says so with an empty body of its own rather than
    /// by inheriting one.
    fn warn(&self, message: &str);

    /// The file an `import` of `path` actually reads: a directory imports its
    /// `default.nix`, as cppnix does, so the importing file's own directory
    /// (which relative paths inside it resolve against) is the resolved
    /// file's parent, not the argument's.
    fn resolve_import(&self, path: &PathValue) -> Result<PathValue, String> {
        // `file_type_resolved` and not `file_type`, because cppnix's
        // `resolveExprPath` tests `path.resolveSymlinks().lstat().type ==
        // tDirectory` (`eval.cc:3440`). Asking the unresolved question here
        // refused `import a/symlinked-dir/f.nix`, which cppnix imports
        // (ENG-12871).
        match self.file_type_resolved(path) {
            Ok(FileType::Directory) => {
                Ok(path.with_path(format!("{}/default.nix", path.trim_end_matches('/'))))
            }
            Ok(_) => Ok(path.clone()),
            Err(e) => Err(e),
        }
    }

    // -- answering slowly ---------------------------------------------------
    //
    // Two methods. Every host in this crate except `ThreadedHost` answers
    // "I have no asynchronous path", and says so with a body of its own:
    // these used to be defaulted and that is the pair that produced the merge
    // bug this trait's header describes. A host that answers `None` drives
    // every other method exactly as before; the scheduler in `crate::eval`
    // asks `begin` first and falls back to the blocking method when it gets
    // `None`, so the two are never both in play for one question and there is
    // no second answer path to keep in step.

    /// Begin answering a slow question, without blocking.
    ///
    /// `None` means "this host has no asynchronous path for this question",
    /// and the scheduler will ask the blocking method instead. A host may
    /// answer `None` for one question and a ticket for another; nothing
    /// requires it to be all or nothing.
    ///
    /// The host must copy whatever it needs out of the request before
    /// returning, because the borrow ends here.
    ///
    /// # What the evaluator guarantees
    ///
    /// It will call [`Host::collect`] with this ticket exactly once, unless
    /// the evaluation is abandoned. It will not call `begin` again for the
    /// same root until the first ticket is collected, so the number of
    /// tickets a host can be holding is bounded by the number of roots.
    fn begin(&self, question: &Slow<'_>) -> Option<Ticket>;

    /// Collect an answer begun by [`Host::begin`].
    ///
    /// With `block` set, this waits for the answer and must return `Some`.
    /// Without it, `None` means "not ready yet" and the scheduler will find
    /// something else to do -- so a host that cannot test readiness cheaply
    /// should return `None` until asked to block rather than blocking anyway,
    /// which would give the scheduler no way to avoid the stall.
    ///
    /// A ticket the host does not recognise, or a second collect of one it
    /// has already answered, is a defect in the scheduler; a host may return
    /// `None` for it, and the scheduler will report the evaluation stuck
    /// rather than hang.
    fn collect(&self, ticket: Ticket, block: bool) -> Option<SlowAnswer>;
}

/// A borrowed host is a host.
///
/// What this exists for is [`crate::readset::RecordingHost`], which owns the
/// host it wraps so that a session can hand it one the session itself owns.
/// Every caller that has a host on the stack and wants to record against it
/// -- which is every test in this crate -- passes `&host`, and this is what
/// makes that spell the same thing it used to.
///
/// Every method is forwarded explicitly, including [`Host::resolve_import`],
/// which has a body on the trait. Forwarding it is right here and wrong on a
/// *recording* wrapper: `&T` and `T` are the same object, so the derived
/// answer and the recorded reads come from the same place either way, where
/// on a recorder forwarding would run the lookup on the inner host and record
/// nothing. See the note on the trait.
impl<T: Host + ?Sized> Host for &T {
    fn import_source(&self, path: &PathValue) -> Result<ImportedSource, String> {
        (**self).import_source(path)
    }
    fn read_file(&self, path: &PathValue) -> Result<String, String> {
        (**self).read_file(path)
    }

    fn read_file_bytes(&self, path: &PathValue) -> Result<Vec<u8>, String> {
        (**self).read_file_bytes(path)
    }

    fn get_env(&self, name: &str) -> Option<String> {
        (**self).get_env(name)
    }

    fn settle(&self) -> Result<(), StoreError> {
        (**self).settle()
    }

    fn read_dir(&self, path: &PathValue) -> Result<Vec<(String, FileType)>, String> {
        (**self).read_dir(path)
    }

    fn path_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
        (**self).path_exists_checked(path)
    }

    fn dir_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
        (**self).dir_exists_checked(path)
    }

    fn file_type(&self, path: &PathValue) -> Result<Option<FileType>, String> {
        (**self).file_type(path)
    }

    fn file_type_resolved(&self, path: &PathValue) -> Result<FileType, String> {
        (**self).file_type_resolved(path)
    }

    fn copy_to_store(&self, path: &PathValue) -> Result<String, StoreError> {
        (**self).copy_to_store(path)
    }

    fn store_path(&self, path: &PathValue) -> Result<StorePathResult, StoreError> {
        (**self).store_path(path)
    }

    fn ensure_path(&self, path: &str) -> Result<(), StoreError> {
        (**self).ensure_path(path)
    }

    fn valid_paths(
        &self,
        paths: &[String],
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        (**self).valid_paths(paths)
    }

    fn sealed_paths(&self, objects: &[String]) -> Result<Links, StoreError> {
        (**self).sealed_paths(objects)
    }

    fn allow_paths(&self, paths: &[String]) -> Result<(), StoreError> {
        (**self).allow_paths(paths)
    }

    fn allow_closures(&self, outputs: &[String]) -> Result<(), StoreError> {
        (**self).allow_closures(outputs)
    }

    fn realise(
        &self,
        context: &[crate::value2::ContextElem],
    ) -> Result<BTreeMap<String, String>, StoreError> {
        (**self).realise(context)
    }

    fn find_file(
        &self,
        entries: &[crate::task::SearchPathEntry],
        name: &str,
    ) -> Result<PathValue, LookupError> {
        (**self).find_file(entries, name)
    }

    fn store_text(
        &self,
        name: &str,
        contents: &str,
        references: &[String],
    ) -> Result<String, StoreError> {
        (**self).store_text(name, contents, references)
    }

    fn write_derivation(&self, name: &str, aterm: &str) -> Result<String, StoreError> {
        (**self).write_derivation(name, aterm)
    }

    fn store_filtered(&self, request: &crate::task::FilteredCopy) -> Result<String, StoreError> {
        (**self).store_filtered(request)
    }

    fn fetch(&self, request: &crate::task::FetchRequest) -> Result<String, StoreError> {
        (**self).fetch(request)
    }

    fn lock_flake(&self, flake_ref: &str) -> Result<FlakeCall, StoreError> {
        (**self).lock_flake(flake_ref)
    }
    fn parse_flake_ref(&self, flake_ref: &str) -> Result<String, StoreError> {
        (**self).parse_flake_ref(flake_ref)
    }
    fn flake_ref_to_string(
        &self,
        attrs: &std::collections::BTreeMap<String, crate::task::TreeAttr>,
    ) -> Result<String, StoreError> {
        (**self).flake_ref_to_string(attrs)
    }

    fn fetch_tree(&self, request: &crate::task::FetchTreeRequest) -> Result<String, StoreError> {
        (**self).fetch_tree(request)
    }

    fn nix_path(&self) -> Result<Vec<crate::task::SearchPathEntry>, LookupError> {
        (**self).nix_path()
    }

    fn trace(&self, message: &str) {
        (**self).trace(message);
    }

    fn warn(&self, message: &str) {
        (**self).warn(message);
    }

    fn resolve_import(&self, path: &PathValue) -> Result<PathValue, String> {
        (**self).resolve_import(path)
    }

    // The scheduler's pair, forwarded for the same reason every effect above
    // is -- and the pair this whole wrapper got wrong once. They were
    // defaulted, `&T` arrived on a branch that could not see them, and the
    // merge compiled clean with this impl inheriting both: `begin` answering
    // `None` for a host that can run the question off the critical path, so
    // every evaluation behind a `&`-wrapped host went back to blocking and
    // nothing said so. They are bodiless now, so the same merge would not
    // build (ENG-13107).
    fn begin(&self, question: &Slow<'_>) -> Option<Ticket> {
        (**self).begin(question)
    }

    fn collect(&self, ticket: Ticket, block: bool) -> Option<SlowAnswer> {
        (**self).collect(ticket, block)
    }
}

/// Fill in the [`Host`] effects a test host does not exercise, each answering
/// the way a leaf with nothing behind it should: no environment, no store,
/// nowhere to warn.
///
/// # Why this is not the trait defaults under another name
///
/// It is `#[cfg(test)]`, so nothing that ships can reach it, and every use
/// names the methods it is filling. Both of those are the opposite of an
/// inherited default: the fill appears in the diff and in the file, where a
/// reviewer looking at a *forwarding* host -- the shape that was actually
/// wrong (`RecordingHost`, ENG-12540) -- would see it and ask why a wrapper
/// is refusing rather than passing the question along.
///
/// Test hosts are leaves, not wrappers. A leaf saying "no store here" is
/// telling the truth; a wrapper saying it is lying about the host behind it.
///
/// This is where the convenience went when [`Host`]'s effects stopped having
/// default bodies (ENG-13107). A leaf used to get these answers by inheriting
/// them, which is the same mechanism that let a *wrapper* inherit them by
/// accident; naming them here keeps the convenience for the hosts entitled to
/// it and takes it away from the hosts that are not. A wrapper reaching for
/// this macro is a defect wherever it is found -- there is no legitimate use,
/// because every refusal it writes is a lie about the host behind it.
#[cfg(test)]
macro_rules! host_stubs {
    (@one settle) => {
        fn settle(&self) -> Result<(), $crate::host::StoreError> {
            Ok(())
        }
    };
    (@one read_file_bytes) => {
        fn read_file_bytes(&self, _path: &$crate::value2::PathValue) -> Result<Vec<u8>, String> {
            Err("no file bytes behind this test host".to_owned())
        }
    };
    (@one get_env) => {
        fn get_env(&self, _name: &str) -> Option<String> {
            None
        }
    };
    (@one copy_to_store) => {
        fn copy_to_store(&self, _path: &$crate::value2::PathValue) -> Result<String, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one store_path) => {
        fn store_path(
            &self,
            _path: &$crate::value2::PathValue,
        ) -> Result<$crate::host::StorePathResult, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one ensure_path) => {
        fn ensure_path(&self, _path: &str) -> Result<(), $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one valid_paths) => {
        fn valid_paths(
            &self,
            _paths: &[String],
        ) -> Result<std::collections::BTreeSet<String>, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one sealed_paths) => {
        fn sealed_paths(
            &self,
            _objects: &[String],
        ) -> Result<$crate::host::Links, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one allow_paths) => {
        fn allow_paths(&self, _paths: &[String]) -> Result<(), $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one allow_closures) => {
        fn allow_closures(&self, _outputs: &[String]) -> Result<(), $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one realise) => {
        fn realise(
            &self,
            _context: &[$crate::value2::ContextElem],
        ) -> Result<
            std::collections::BTreeMap<String, String>,
            $crate::host::StoreError,
        > {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one store_text) => {
        fn store_text(
            &self,
            _name: &str,
            _contents: &str,
            _references: &[String],
        ) -> Result<String, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one write_derivation) => {
        fn write_derivation(
            &self,
            _name: &str,
            _aterm: &str,
        ) -> Result<String, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one store_filtered) => {
        fn store_filtered(
            &self,
            _request: &$crate::task::FilteredCopy,
        ) -> Result<String, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one fetch) => {
        fn fetch(
            &self,
            _request: &$crate::task::FetchRequest,
        ) -> Result<String, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one fetch_tree) => {
        fn fetch_tree(
            &self,
            _request: &$crate::task::FetchTreeRequest,
        ) -> Result<String, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one lock_flake) => {
        fn lock_flake(
            &self,
            _flake_ref: &str,
        ) -> Result<$crate::host::FlakeCall, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one parse_flake_ref) => {
        fn parse_flake_ref(
            &self,
            _flake_ref: &str,
        ) -> Result<String, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    (@one flake_ref_to_string) => {
        fn flake_ref_to_string(
            &self,
            _attrs: &std::collections::BTreeMap<String, $crate::task::TreeAttr>,
        ) -> Result<String, $crate::host::StoreError> {
            Err($crate::host::StoreError::NoStore)
        }
    };
    // The scheduler's pair, and the only stubs here that are not a refusal.
    // `None` from `begin` is not "I cannot", it is "not off the critical
    // path", so a leaf taking these is choosing to answer every slow question
    // on the calling thread -- which is what every host in this crate except
    // `ThreadedHost` does. Taken together and never singly: a host that
    // begins nothing can be asked to collect nothing, and one that begins
    // something owes a real `collect`.
    (@one not_async) => {
        fn begin(&self, _question: &$crate::host::Slow<'_>) -> Option<$crate::host::Ticket> {
            None
        }
        fn collect(
            &self,
            _ticket: $crate::host::Ticket,
            _block: bool,
        ) -> Option<$crate::host::SlowAnswer> {
            None
        }
    };
    (@one warn) => {
        fn warn(&self, _message: &str) {}
    };
    (@one trace) => {
        fn trace(&self, _message: &str) {}
    };
    (@one find_file) => {
        fn find_file(
            &self,
            _entries: &[$crate::task::SearchPathEntry],
            _name: &str,
        ) -> Result<$crate::value2::PathValue, $crate::host::LookupError> {
            Err($crate::host::LookupError::NoResolver)
        }
    };
    (@one file_type_resolved) => {
        fn file_type_resolved(
            &self,
            path: &$crate::value2::PathValue,
        ) -> Result<$crate::host::FileType, String> {
            // Delegating and not refusing, unlike the effects above, because
            // this one has a right answer here: no test host in this crate
            // models a symlink, and with no symlink anywhere in the path
            // `stat` and `lstat` agree. A test host that grows symlinks must
            // write this method itself rather than reach for the stub.
            //
            // The contracts differ where the path is absent, so the throw
            // `file_type` no longer performs is performed here: this is
            // cppnix's `lstat` and a missing path fails it.
            self.file_type(path)?
                .ok_or_else(|| format!("path '{path}' does not exist"))
        }
    };
    (@one nix_path) => {
        fn nix_path(
            &self,
        ) -> Result<Vec<$crate::task::SearchPathEntry>, $crate::host::LookupError> {
            Err($crate::host::LookupError::NoResolver)
        }
    };
    // Two things here are load-bearing, both learned by watching this
    // recurse until the compiler gave up.
    //
    // `tt` and not `ident`: an `ident` fragment, once captured, stops
    // matching the literal-token rules above, so every name fell through to
    // this rule. A `tt` is re-emitted as the token it captured and matches.
    //
    // `@one`: without a marker this rule also matches a single name, and
    // macro_rules would still have to reach the literal rules first to avoid
    // recursing. The marker makes that structural rather than positional --
    // this rule cannot match `@one x` at all.
    ($($method:tt),+ $(,)?) => {
        $( $crate::host::host_stubs!(@one $method); )+
    };
}

#[cfg(test)]
pub(crate) use host_stubs;

/// Reads the real filesystem and has no embedder behind it: every store
/// question is [`StoreError::NoStore`], every lookup is
/// [`LookupError::NoResolver`], and a warning or a trace line goes nowhere.
///
/// The leaf of the host chain, and the one every other host in this crate
/// falls back to. An embedder that can answer more wraps it -- [`FnHost`] for
/// a Rust caller, `capi::EmbedderHost` for one crossing the C ABI -- rather
/// than installing itself into this one, which is what the retired
/// `set_*_hook` globals did.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealFs;

fn ambient_path(path: &PathValue) -> Result<&str, String> {
    match &path.root {
        crate::value2::Root::Ambient => Ok(path.as_ref()),
        crate::value2::Root::Mounted(mount) => Err(format!(
            "mounted path '{path}' requires accessor rooted at '{mount}'"
        )),
    }
}

impl Host for RealFs {
    /// Nothing is deferred: every effect of this host happens when asked.
    fn settle(&self) -> Result<(), StoreError> {
        Ok(())
    }

    fn import_source(&self, path: &PathValue) -> Result<ImportedSource, String> {
        let path = self.resolve_import(path)?;
        let text = self.read_file(&path)?;
        Ok(ImportedSource::Nix { path, text })
    }
    fn get_env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn copy_to_store(&self, _path: &PathValue) -> Result<String, StoreError> {
        Err(StoreError::NoStore)
    }

    fn store_path(&self, _path: &PathValue) -> Result<StorePathResult, StoreError> {
        Err(StoreError::NoStore)
    }

    fn store_text(
        &self,
        _name: &str,
        _contents: &str,
        _references: &[String],
    ) -> Result<String, StoreError> {
        Err(StoreError::NoStore)
    }

    fn ensure_path(&self, _path: &str) -> Result<(), StoreError> {
        Err(StoreError::NoStore)
    }

    fn valid_paths(
        &self,
        _paths: &[String],
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        Err(StoreError::NoStore)
    }

    fn sealed_paths(&self, _objects: &[String]) -> Result<Links, StoreError> {
        Err(StoreError::NoStore)
    }

    fn allow_paths(&self, _paths: &[String]) -> Result<(), StoreError> {
        Err(StoreError::NoStore)
    }

    fn allow_closures(&self, _outputs: &[String]) -> Result<(), StoreError> {
        Err(StoreError::NoStore)
    }

    fn realise(
        &self,
        _context: &[crate::value2::ContextElem],
    ) -> Result<BTreeMap<String, String>, StoreError> {
        Err(StoreError::NoStore)
    }

    fn write_derivation(&self, _name: &str, _aterm: &str) -> Result<String, StoreError> {
        Err(StoreError::NoStore)
    }

    fn store_filtered(&self, _request: &crate::task::FilteredCopy) -> Result<String, StoreError> {
        Err(StoreError::NoStore)
    }

    fn fetch(&self, _request: &crate::task::FetchRequest) -> Result<String, StoreError> {
        Err(StoreError::NoStore)
    }

    fn fetch_tree(&self, _request: &crate::task::FetchTreeRequest) -> Result<String, StoreError> {
        Err(StoreError::NoStore)
    }

    fn lock_flake(&self, _flake_ref: &str) -> Result<FlakeCall, StoreError> {
        Err(StoreError::NoStore)
    }

    fn parse_flake_ref(&self, _flake_ref: &str) -> Result<String, StoreError> {
        Err(StoreError::NoStore)
    }

    fn flake_ref_to_string(
        &self,
        _attrs: &std::collections::BTreeMap<String, crate::task::TreeAttr>,
    ) -> Result<String, StoreError> {
        Err(StoreError::NoStore)
    }

    fn find_file(
        &self,
        _entries: &[crate::task::SearchPathEntry],
        _name: &str,
    ) -> Result<PathValue, LookupError> {
        Err(LookupError::NoResolver)
    }

    fn nix_path(&self) -> Result<Vec<crate::task::SearchPathEntry>, LookupError> {
        Err(LookupError::NoResolver)
    }

    fn warn(&self, _message: &str) {}

    fn trace(&self, _message: &str) {}

    /// Nothing here runs off the critical path: a `std::fs` read is not a
    /// question worth a thread, and this host answers nothing slower. Written
    /// out rather than inherited, for the reason on the trait.
    fn begin(&self, _question: &Slow<'_>) -> Option<Ticket> {
        None
    }

    fn collect(&self, _ticket: Ticket, _block: bool) -> Option<SlowAnswer> {
        None
    }

    fn read_file(&self, path: &PathValue) -> Result<String, String> {
        let fs_path = ambient_path(path)?;
        // cppnix reports a missing path before it reports anything about
        // the contents, and with this wording; the corpus compares the
        // class.
        if !self.path_exists_checked(path)? {
            return Err(format!("path '{path}' does not exist"));
        }
        std::fs::read_to_string(fs_path).map_err(|e| format!("cannot read '{path}': {e}"))
    }

    fn read_file_bytes(&self, path: &PathValue) -> Result<Vec<u8>, String> {
        let fs_path = ambient_path(path)?;
        // The same wording and order as `read_file`, because the two are
        // one read with two answer types.
        if !self.path_exists_checked(path)? {
            return Err(format!("path '{path}' does not exist"));
        }
        std::fs::read(fs_path).map_err(|e| format!("cannot read '{path}': {e}"))
    }

    fn read_dir(&self, path: &PathValue) -> Result<Vec<(String, FileType)>, String> {
        let fs_path = ambient_path(path)?;
        // cppnix's two spellings, verbatim: a missing path is reported as
        // missing before anything about directories, and a non-directory is
        // reported with double quotes and no errno tail.
        if !self.path_exists_checked(path)? {
            return Err(format!("path '{path}' does not exist"));
        }
        // cppnix names the path it actually opened, so a symlink to a
        // non-directory is reported as its target rather than as the link.
        let shown = std::fs::canonicalize(fs_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string());
        let entries = std::fs::read_dir(fs_path)
            .map_err(|e| format!("cannot read directory \"{shown}\": {}", errno_text(&e)))?;
        let mut out = Vec::new();
        for e in entries {
            let e =
                e.map_err(|e| format!("cannot read directory \"{shown}\": {}", errno_text(&e)))?;
            let name = e.file_name().to_string_lossy().into_owned();
            // Not followed: cppnix reports a symlink as a symlink here, and
            // only resolves it when something reads through it.
            let t = match e.file_type() {
                Ok(t) if t.is_symlink() => FileType::Symlink,
                Ok(t) if t.is_dir() => FileType::Directory,
                Ok(t) if t.is_file() => FileType::Regular,
                _ => FileType::Unknown,
            };
            out.push((name, t));
        }
        // cppnix returns an attrset, which is name-sorted; sorting here keeps
        // the host's answer independent of readdir order.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn path_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
        let fs_path = ambient_path(path)?;
        match Path::new(fs_path).symlink_metadata() {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("cannot access '{path}': {error}")),
        }
    }

    fn dir_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
        let fs_path = ambient_path(path)?;
        match Path::new(fs_path).metadata() {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("cannot access '{path}': {error}")),
        }
    }

    fn file_type(&self, path: &PathValue) -> Result<Option<FileType>, String> {
        let fs_path = ambient_path(path)?;
        // Every error `symlink_metadata` can give is folded into "not
        // there", which is what it meant here before absence had a value
        // of its own: the message this used to build was
        // `path '...' does not exist` whatever the errno said. A host
        // reading with `std::fs` is refused under either purity setting
        // (`purity.rs`), so there is no allow list here to report a
        // refusal from -- the distinction `Err` now carries has no
        // producer on this path.
        let Ok(md) = std::fs::symlink_metadata(fs_path) else {
            return Ok(None);
        };
        Ok(Some(if md.file_type().is_symlink() {
            FileType::Symlink
        } else if md.is_dir() {
            FileType::Directory
        } else if md.is_file() {
            FileType::Regular
        } else {
            FileType::Unknown
        }))
    }

    fn file_type_resolved(&self, path: &PathValue) -> Result<FileType, String> {
        let fs_path = ambient_path(path)?;
        // `metadata` and not `symlink_metadata`: this is the `stat`
        // question. A dangling symlink therefore reports the missing
        // target, which is what cppnix's `resolveExprPath` does too --
        // `path.resolveSymlinks()` rewrites the path to the target before
        // `lstat` looks at it.
        let md = std::fs::metadata(fs_path).map_err(|_| format!("path '{path}' does not exist"))?;
        Ok(if md.is_dir() {
            FileType::Directory
        } else if md.is_file() {
            FileType::Regular
        } else {
            FileType::Unknown
        })
    }
}

/// A [`Host`] assembled from plain `fn` pointers, for an embedder that is not
/// crossing a C ABI: the nixpkgs probe, the differential harness, a test.
///
/// A value and not a set of process globals, which is the whole point. The
/// hooks used to be `static RwLock<Option<Fn>>` slots installed by
/// `set_*_hook`, so two evaluations in one process shared one store, one
/// logger and one resolver, and the second to install won for both. Handing
/// the set over as a struct makes "which host answers this" a property of the
/// evaluation rather than of the process, and there is no longer any way to
/// spell the race.
///
/// Every field is optional and an absent one falls through to [`RealFs`], so
/// a caller supplies exactly the effects it can answer and the rest report
/// [`StoreError::NoStore`] or [`LookupError::NoResolver`] -- never a guess.
/// [`FnHost::path_reads`] is the one that is all-or-nothing; see
/// [`PathReadHooks`] for why.
#[derive(Debug, Default, Clone)]
pub struct FnHost {
    /// Files the embedding hands over by content, known up front -- the
    /// C++ bridge hands them over lazily, in `findFile` answers, and its host
    /// holds the same value. Consulted before every path read.
    pub virtual_files: VirtualFiles,
    pub store_copy: Option<StoreCopyHook>,
    pub store_path: Option<StorePathHook>,
    pub store_text: Option<StoreTextHook>,
    pub write_drv: Option<WriteDrvHook>,
    pub store_filtered: Option<StoreFilteredHook>,
    pub fetch: Option<FetchHook>,
    pub fetch_tree: Option<FetchTreeHook>,
    pub flake: Option<FlakeHook>,
    pub store_ensure: Option<StoreEnsureHook>,
    pub realise: Option<RealiseHook>,
    pub find_file: Option<FindFileHook>,
    pub nix_path: Option<NixPathHook>,
    pub warn: Option<WarnHook>,
    pub trace: Option<TraceHook>,
    pub path_reads: Option<PathReadHooks>,
}

impl FnHost {
    /// Whether a plain filesystem read reaches the world through this host's
    /// hooks rather than through `std::fs`.
    ///
    /// Read by [`crate::purity::PathReads::of`] and by nothing else: it is a
    /// statement about who answers, which is the only thing that decides
    /// whether a purity setting can be honoured.
    #[must_use]
    pub fn path_reads(&self) -> crate::purity::PathReads {
        match self.path_reads {
            Some(_) => crate::purity::PathReads::ThroughEmbedder,
            None => crate::purity::PathReads::Direct,
        }
    }
}

impl Host for FnHost {
    /// Nothing is deferred: the store functions act when called.
    fn settle(&self) -> Result<(), StoreError> {
        Ok(())
    }

    fn import_source(&self, path: &PathValue) -> Result<ImportedSource, String> {
        let path = self.resolve_import(path)?;
        let text = self.read_file(&path)?;
        Ok(ImportedSource::Nix { path, text })
    }
    fn get_env(&self, name: &str) -> Option<String> {
        RealFs.get_env(name)
    }

    fn copy_to_store(&self, path: &PathValue) -> Result<String, StoreError> {
        let hook = self.store_copy.ok_or(StoreError::NoStore)?;
        hook(path).map_err(StoreError::Failed)
    }

    fn store_path(&self, path: &PathValue) -> Result<StorePathResult, StoreError> {
        let hook = self.store_path.ok_or(StoreError::NoStore)?;
        hook(path).map_err(StoreError::Failed)
    }

    fn store_text(
        &self,
        name: &str,
        contents: &str,
        references: &[String],
    ) -> Result<String, StoreError> {
        let hook = self.store_text.ok_or(StoreError::NoStore)?;
        hook(name, contents, references).map_err(StoreError::Failed)
    }

    fn ensure_path(&self, path: &str) -> Result<(), StoreError> {
        let hook = self.store_ensure.ok_or(StoreError::NoStore)?;
        hook(path).map_err(StoreError::Failed)
    }

    /// No hook for it: the fn-table host is the test and example embedder,
    /// whose replays may rewrite.
    fn valid_paths(
        &self,
        _paths: &[String],
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        Err(StoreError::NoStore)
    }

    /// No hook for it either, for the same reason: its replays ask.
    fn sealed_paths(&self, _objects: &[String]) -> Result<Links, StoreError> {
        Err(StoreError::NoStore)
    }

    /// Nor for this: a realisation this host served is re-run, not allowed.
    fn allow_paths(&self, _paths: &[String]) -> Result<(), StoreError> {
        Err(StoreError::NoStore)
    }

    fn allow_closures(&self, _outputs: &[String]) -> Result<(), StoreError> {
        Err(StoreError::NoStore)
    }

    fn realise(
        &self,
        context: &[crate::value2::ContextElem],
    ) -> Result<BTreeMap<String, String>, StoreError> {
        let hook = self.realise.ok_or(StoreError::NoStore)?;
        hook(context)
    }

    fn write_derivation(&self, name: &str, aterm: &str) -> Result<String, StoreError> {
        let hook = self.write_drv.ok_or(StoreError::NoStore)?;
        hook(name, aterm).map_err(StoreError::Failed)
    }

    fn store_filtered(&self, request: &crate::task::FilteredCopy) -> Result<String, StoreError> {
        let hook = self.store_filtered.ok_or(StoreError::NoStore)?;
        hook(request).map_err(StoreError::Failed)
    }

    fn fetch(&self, request: &crate::task::FetchRequest) -> Result<String, StoreError> {
        let hook = self.fetch.ok_or(StoreError::NoStore)?;
        hook(request).map_err(StoreError::Failed)
    }

    fn fetch_tree(&self, request: &crate::task::FetchTreeRequest) -> Result<String, StoreError> {
        let hook = self.fetch_tree.ok_or(StoreError::NoStore)?;
        hook(request)
    }

    fn lock_flake(&self, flake_ref: &str) -> Result<FlakeCall, StoreError> {
        let hook = self.flake.ok_or(StoreError::NoStore)?;
        hook(flake_ref)
    }

    fn parse_flake_ref(&self, _flake_ref: &str) -> Result<String, StoreError> {
        Err(StoreError::NoStore)
    }

    fn flake_ref_to_string(
        &self,
        _attrs: &std::collections::BTreeMap<String, crate::task::TreeAttr>,
    ) -> Result<String, StoreError> {
        Err(StoreError::NoStore)
    }

    fn find_file(
        &self,
        entries: &[crate::task::SearchPathEntry],
        name: &str,
    ) -> Result<PathValue, LookupError> {
        let hook = self.find_file.ok_or(LookupError::NoResolver)?;
        hook(entries, name)
    }

    fn nix_path(&self) -> Result<Vec<crate::task::SearchPathEntry>, LookupError> {
        let hook = self.nix_path.ok_or(LookupError::NoResolver)?;
        hook()
    }

    fn warn(&self, message: &str) {
        if let Some(hook) = self.warn {
            hook(message);
        }
    }

    fn trace(&self, message: &str) {
        if let Some(hook) = self.trace {
            hook(message);
        }
    }

    /// A Rust embedder that wants its fetches off the critical path wraps
    /// this in [`ThreadedHost`] rather than growing an asynchronous path of
    /// its own: the hooks here are plain `fn` pointers with nowhere to keep a
    /// ticket. Written out rather than inherited, for the reason on the
    /// trait.
    fn begin(&self, _question: &Slow<'_>) -> Option<Ticket> {
        None
    }

    fn collect(&self, _ticket: Ticket, _block: bool) -> Option<SlowAnswer> {
        None
    }

    fn read_file(&self, path: &PathValue) -> Result<String, String> {
        // With hooks the read goes through the embedder's accessor, which
        // for cppnix is `rootFS` -- so `pure-eval` and `restrict-eval`
        // are enforced there and their `RestrictedPathError` text comes
        // back as the error. `RealFs` below is the standalone embedding
        // and cannot do either (ENG-12792).
        self.virtual_files
            .read_file_or(path, || match self.path_reads {
                Some(hooks) => (hooks.read_file)(path),
                None => RealFs.read_file(path),
            })
    }

    fn read_file_bytes(&self, path: &PathValue) -> Result<Vec<u8>, String> {
        self.virtual_files
            .read_file_bytes_or(path, || match self.path_reads {
                // The hook table answers `String`, so a hooked read has already
                // decoded: this arm inherits the corruption ENG-13147 names, and
                // the byte-clean hook belongs to that redesign. No caller reaches
                // it today -- the bridge embeds through `capi`, whose host reads
                // the FFI bytes raw.
                Some(hooks) => (hooks.read_file)(path).map(String::into_bytes),
                None => RealFs.read_file_bytes(path),
            })
    }

    fn read_dir(&self, path: &PathValue) -> Result<Vec<(String, FileType)>, String> {
        match self.path_reads {
            Some(hooks) => (hooks.read_dir)(path),
            None => RealFs.read_dir(path),
        }
    }

    fn path_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
        self.virtual_files
            .path_exists_checked_or(path, || match self.path_reads {
                Some(hooks) => (hooks.path_exists)(path),
                None => RealFs.path_exists_checked(path),
            })
    }

    fn dir_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
        self.virtual_files
            .dir_exists_checked_or(path, || match self.path_reads {
                Some(hooks) => (hooks.dir_exists)(path),
                None => RealFs.dir_exists_checked(path),
            })
    }

    fn file_type(&self, path: &PathValue) -> Result<Option<FileType>, String> {
        self.virtual_files
            .file_type_or(path, || match self.path_reads {
                Some(hooks) => (hooks.file_type)(path),
                None => RealFs.file_type(path),
            })
    }

    fn file_type_resolved(&self, path: &PathValue) -> Result<FileType, String> {
        self.virtual_files
            .file_type_resolved_or(path, || match self.path_reads {
                Some(hooks) => (hooks.file_type_resolved)(path),
                None => RealFs.file_type_resolved(path),
            })
    }
}

/// The bare strerror text. Rust appends " (os error N)" to its Display, which
/// cppnix does not print, and the corpus compares the message.
fn errno_text(e: &std::io::Error) -> String {
    let full = e.to_string();
    match full.find(" (os error ") {
        Some(i) => full.get(..i).unwrap_or(&full).to_owned(),
        None => full,
    }
}

/// The one thing the compiler cannot catch about [`Host`]: a default body
/// nobody has written yet.
#[cfg(test)]
mod trait_shape_tests {
    /// Refuse a default body on [`super::Host`], except on helpers derived
    /// entirely from other host methods and test-only compatibility defaults.
    ///
    /// # Why a source parse and not a normal test
    ///
    /// Because the property is about the shape of the trait rather than about
    /// any behaviour, and there is no value to assert on. Every effect being
    /// bodiless is what makes the three forwarding wrappers safe -- a wrapper
    /// that misses one does not compile -- and that guarantee evaporates the
    /// moment somebody adds a method with a body, silently, for the same
    /// perfectly good reason the seven store effects had one until ENG-13107:
    /// a leaf host with no store is `readOnlyMode` and `Err(NoStore)` is the
    /// honest answer for all fifty of them.
    ///
    /// It is a good reason and it is still the wrong mechanism, because a
    /// default cannot tell a leaf from a wrapper. The convenience belongs in
    /// `host_stubs!`, which a leaf asks for by name. This test is what says
    /// so at the moment somebody reaches for the other one.
    ///
    /// `resolve_import` and `import_source` are derived from primitive path
    /// reads. The other exemptions have bodies only under `cfg(test)`, so the
    /// many focused fake hosts can keep implementing the older primitive
    /// surface; production wrappers still have to forward them explicitly.
    #[test]
    fn the_trait_has_no_default_bodies_to_inherit() {
        const EXEMPT: &[&str] = &[
            "import_source",
            "resolve_import",
            "store_path",
            "valid_paths",
            // The same shape as `valid_paths`: bodiless in production, a
            // `cfg(test)` default for the fake hosts.
            "sealed_paths",
            "allow_paths",
            "allow_closures",
        ];

        let source = include_str!("host.rs");
        let lines: Vec<&str> = source.lines().collect();

        let start = lines
            .iter()
            .position(|line| line.trim_start() == "pub trait Host {")
            .map(|i| i + 1);
        assert!(
            start.is_some(),
            "could not find `pub trait Host {{` in host.rs; the parse is wrong"
        );
        let Some(start) = start else { return };

        let mut bodiless: Vec<&str> = Vec::new();
        let mut bodied: Vec<&str> = Vec::new();
        // Depth inside a method body. Zero means we are at the trait's own
        // level, where a `fn` is a declaration rather than something nested.
        let mut depth: i32 = 0;
        let mut i = start;
        let mut closed = false;

        while let Some(raw) = lines.get(i) {
            let trimmed = raw.trim();
            // Dropped before the brace count, not after: the doc comments in
            // this trait contain `${./f}` and `{name}.drv`, and counting
            // those would put the scan permanently out of step.
            if trimmed.starts_with("//") {
                i += 1;
                continue;
            }
            if depth == 0 && trimmed == "}" {
                closed = true;
                break;
            }
            if depth == 0 && trimmed.starts_with("fn ") {
                let name = trimmed
                    .trim_start_matches("fn ")
                    .split('(')
                    .next()
                    .unwrap_or("")
                    .trim();
                // A signature may wrap over several lines; the line that ends
                // it says which kind of method this is.
                let mut k = i;
                let mut has_body = None;
                while let Some(sig) = lines.get(k) {
                    let sig = sig.trim_end();
                    if sig.ends_with(';') {
                        has_body = Some(false);
                        break;
                    }
                    if sig.ends_with('{') {
                        has_body = Some(true);
                        break;
                    }
                    k += 1;
                }
                assert!(
                    has_body.is_some(),
                    "unterminated signature for `{name}` in host.rs; the parse is wrong"
                );
                match has_body {
                    Some(true) => {
                        bodied.push(name);
                        depth = 1;
                    }
                    Some(false) => bodiless.push(name),
                    None => return,
                }
                i = k + 1;
                continue;
            }
            if depth > 0 {
                let opens = i32::try_from(raw.matches('{').count()).unwrap_or(0);
                let closes = i32::try_from(raw.matches('}').count()).unwrap_or(0);
                depth += opens - closes;
            }
            i += 1;
        }

        // Three ways this test can pass while having measured nothing: never
        // finding the trait, never finding its end, and parsing no methods.
        // All three look exactly like a clean bill of health, so all three
        // are failures.
        assert!(
            closed,
            "never found the end of `trait Host`; the parse is wrong"
        );
        assert!(
            bodiless.len() + bodied.len() >= 15,
            "parsed only {} methods out of `trait Host`; the parse is wrong, \
             not the trait",
            bodiless.len() + bodied.len()
        );
        for known in [
            "import_source",
            "read_file",
            "fetch",
            "begin",
            "collect",
            "resolve_import",
        ] {
            assert!(
                bodiless.contains(&known) || bodied.contains(&known),
                "`{known}` was not among the parsed methods; the parse is wrong"
            );
        }

        let unexpected: Vec<&str> = bodied
            .iter()
            .filter(|name| !EXEMPT.contains(name))
            .copied()
            .collect();
        assert!(
            unexpected.is_empty(),
            "`Host` has default bodies that are not exempt: {unexpected:?}.\n\
             \n\
             A defaulted method is one a forwarding wrapper can skip while still\n\
             compiling, and there are three wrappers -- `ThreadedHost`,\n\
             `readset::RecordingHost` and `impl Host for &T`. A wrapper that skips\n\
             one answers on behalf of the host behind it, silently: that is ENG-12555\n\
             (`ensure_path` and `warn` on the recorder) and ENG-13107 (`begin` and\n\
             `collect` on `&T`, where the only symptom was every evaluation quietly\n\
             going back on the critical path).\n\
             \n\
             If the body exists so that leaf hosts with no store need not write it,\n\
             put it in `host_stubs!` instead and let each leaf name it. A leaf saying\n\
             \"no store here\" is telling the truth; a wrapper saying it is lying.\n\
             \n\
             If it really is derived from other methods rather than an effect of its\n\
             own -- which is what `resolve_import` is -- add it to EXEMPT above with\n\
             the reason."
        );

        // A stale exemption is its own failure: the day an import helper stops
        // having a body, this list must shrink rather than sit there widening
        // the check for a method that no longer needs it.
        for exempt in EXEMPT {
            assert!(
                bodied.contains(exempt),
                "`{exempt}` is exempt from the no-default-bodies rule but no longer \
                 has a body; remove it from EXEMPT"
            );
        }
    }
}

#[cfg(test)]
mod threaded_host_tests {
    use super::{FileType, FlakeCall, Host, StoreError, ThreadedHost, Ticket};
    use std::sync::Mutex;

    /// `ThreadedHost::begin` must reach the same blocking method the
    /// scheduler would have called itself, for every [`Slow`] variant.
    ///
    /// This is what the compiler cannot say. Since ENG-13107 every effect on
    /// [`Host`] is bodiless, so a wrapper that fails to forward one does not
    /// compile and no test has to remember a list of method names -- the
    /// fourteen-name list that used to live here, and the second copy of it
    /// that guarded `&T`, are both gone. What survives is the routing: this
    /// wrapper does not forward `begin`, it *answers* it, by running the
    /// blocking method on a thread. Which blocking method it picks per
    /// variant is a hand-written `match` and is exactly the kind of thing a
    /// copy-paste gets wrong.
    #[test]
    fn the_threaded_wrapper_runs_the_blocking_method_on_the_inner_host() {
        struct Inner {
            asked: Mutex<Vec<String>>,
        }
        impl Host for Inner {
            host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                get_env,
                file_type_resolved,
                copy_to_store,
                ensure_path,
                store_text,
                write_derivation,
                store_filtered,
                find_file,
                nix_path,
                warn,
                trace,
                not_async,
            );
            // Not the subject here, and not refusals either: the four path
            // reads have to answer something for the host to be usable at
            // all.
            fn read_file(&self, _path: &crate::value2::PathValue) -> Result<String, String> {
                Ok(String::new())
            }
            fn read_dir(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
            fn fetch(&self, request: &crate::task::FetchRequest) -> Result<String, StoreError> {
                self.note(&format!("fetch {}", request.url));
                Ok("/nix/store/fetched".to_owned())
            }
            fn fetch_tree(
                &self,
                request: &crate::task::FetchTreeRequest,
            ) -> Result<String, StoreError> {
                self.note(&format!("fetch_tree {}", request.fetcher.as_str()));
                Ok("{}".to_owned())
            }
            fn lock_flake(&self, flake_ref: &str) -> Result<FlakeCall, StoreError> {
                self.note(&format!("lock_flake {flake_ref}"));
                Err(StoreError::Unsupported("no lock here".to_owned()))
            }
            fn realise(
                &self,
                context: &[crate::value2::ContextElem],
            ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
                self.note(&format!("realise {}", context.len()));
                Ok(std::collections::BTreeMap::new())
            }
        }
        impl Inner {
            fn note(&self, what: &str) {
                if let Ok(mut asked) = self.asked.lock() {
                    asked.push(what.to_owned());
                }
            }
        }

        let fetch = crate::task::FetchRequest {
            url: "https://u/slow".to_owned(),
            name: "slow".to_owned(),
            kind: crate::task::FetchKind::File,
            expected_sha256: None,
        };
        let tree = crate::task::FetchTreeRequest {
            attrs: std::collections::BTreeMap::new(),
            fetcher: crate::task::TreeFetcher::Tree,
        };
        // Every variant of `Slow`, so a variant added without a `begin` arm
        // shows up here as a missing line rather than as a silent fallback to
        // the critical path.
        let questions = [
            super::Slow::Fetch(&fetch),
            super::Slow::FetchTree(&tree),
            super::Slow::Flake("github:x/y"),
            super::Slow::Realise(&[]),
        ];
        let want = [
            "fetch https://u/slow",
            "fetch_tree fetchTree",
            "lock_flake github:x/y",
            "realise 0",
        ];

        let threaded = ThreadedHost::new(Inner {
            asked: Mutex::new(Vec::new()),
        });
        for question in &questions {
            let begun = threaded.begin(question);
            assert!(
                begun.is_some(),
                "ThreadedHost::begin answered None for {question:?}, \
                 so this question would run on the critical path"
            );
            let Some(ticket) = begun else { continue };
            assert!(
                threaded.collect(ticket, true).is_some(),
                "a blocking collect must produce an answer for {question:?}"
            );
        }
        let asked = threaded
            .inner()
            .asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            asked, want,
            "a begun question reached the wrong blocking method on the host behind the wrapper"
        );
    }

    /// A wrapper whose `begin` says "no" for a question it does not know how
    /// to run asynchronously must not also lose the ticket bookkeeping: an
    /// unknown ticket collects as `None` rather than blocking for ever, which
    /// is what lets the scheduler report a stuck evaluation.
    #[test]
    fn an_unknown_ticket_collects_as_nothing_rather_than_blocking() {
        struct Nothing;
        impl Host for Nothing {
            host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                find_file,
                nix_path,
                warn,
                trace
            );
            fn read_file(&self, _path: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
        }
        let host = ThreadedHost::new(Nothing);
        assert!(host.collect(Ticket(999), true).is_none());
    }
}
