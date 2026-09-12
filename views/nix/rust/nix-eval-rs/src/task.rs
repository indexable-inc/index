//! Resumable frames for everything that walks a value: structural equality,
//! ordering, `with`-scope resolution, printing, string coercion, and builtins
//! mid-flight. Each is a worklist plus a cursor, so the depth of the Nix value
//! costs heap, never host stack.
//!
//! A task advances by returning a `Yield`. The machine turns `Force`, `Apply`
//! and `Sub` into frames pushed above the task and hands the resulting value
//! back on the next step; `Done` completes it. A task therefore never calls
//! back into evaluation, which is the property that keeps the interpreter
//! flat and lets a suspension unwind to the scheduler with no host frames to
//! rebuild.

use crate::primops_pure::{self, Cont};
use crate::print::{Coerce, Print};
use crate::value2::{ContextElem, Env, EnvNode, Slot, Sym, Value, type_name};
use crate::vm::{Result, Vm, VmError, forced};
use std::cmp::Ordering;
use std::rc::Rc;

/// What a task wants next.
pub enum Yield {
    Done(Value),
    /// Force this slot and step again with its value.
    Force(Slot),
    /// Apply this function and step again with the result.
    Apply(Value, Slot),
    /// Run this task and step again with its value. Boxed: `Task` is the fat
    /// state machine, and a `Yield` is returned by every `Task::step`.
    Sub(Box<Task>),
    /// Ask the scheduler about a path and step again with its answer. The
    /// only way a task reaches the filesystem: the VM itself performs no IO,
    /// so this leaves the machine through `Step::NeedPath` and comes back
    /// through `resume` with the frame chain untouched.
    Need(NeedPath),
}

/// What the scheduler is being asked for. Every question the evaluator can
/// ask the outside world is a variant here, which is the property that makes
/// a recorded read set complete rather than merely thorough: a builtin that
/// reaches the world some other way would not appear in one.
///
/// `Env` is not about a path and sits here anyway, because the alternative
/// was `builtins.getEnv` calling `std::env::var` itself, which is what it
/// used to do and which quietly made the "the VM performs no IO" claim in
/// `host` false.
#[derive(Debug, Clone)]
pub enum NeedPath {
    /// Resolved path and source text, for `import`.
    Import(Rc<crate::value2::PathValue>),
    /// Lock a flake reference and hand back everything needed to evaluate its
    /// outputs, for `builtins.getFlake`.
    ///
    /// # Why the whole lock is one question
    ///
    /// Locking is what cppnix's `lockFlake` does: read `flake.nix` to find its
    /// `inputs`, walk the input graph, consult the registry, fetch each node.
    /// It is IO and policy, the embedder already implements it, and a second
    /// implementation here would be a second set of answers for a store path
    /// to differ over -- the same argument [`NeedPath::Fetch`] makes about
    /// downloading.
    ///
    /// So this evaluator does only the part cppnix's `prim_getFlake` does
    /// before it touches the world -- force the argument to a context-free
    /// string -- and hands the reference over. What comes back is
    /// [`crate::host::FlakeCall`]: the `call-flake.nix` program, the lock file
    /// and the overrides document. Evaluating that program against those
    /// arguments is this side's job and stays here, which is what keeps
    /// `getFlake` and the `<flake>#attr` command line one seam rather than
    /// two: the bridge builds the same three arguments for both, and the VM
    /// evaluates `outputs` in both.
    ///
    /// The `call-flake.nix` source is part of the answer rather than embedded
    /// in this crate on purpose. A copy here would be a second copy of a
    /// 105-line program that decides which tree every flake input resolves
    /// to, and the two would drift.
    Flake(String),
    /// Parse a flake reference string into its exploded attribute form, for
    /// `builtins.parseFlakeRef`.
    ///
    /// String work in principle, the embedder's in practice: the grammar is
    /// cppnix's whole flake-ref surface -- URL schemes, path refs, indirect
    /// refs, `git+` prefixes -- and a second parser here would be a second
    /// set of attrs for one string to explode to. The evaluator does what
    /// `prim_parseFlakeRef` does before it calls out (force the argument to
    /// a context-free string) and hands the rest over. The answer is JSON,
    /// `fetchers::attrsToJSON` over `FlakeRef::toAttrs`: string, integer and
    /// Boolean fields only, the three shapes `fetchers::Attr` holds.
    ParseFlakeRef(String),
    /// Print a flake reference's attribute form as its URL form, for
    /// `builtins.flakeRefToString`.
    ///
    /// The attrs travel in name order, tagged like [`NeedPath::FetchTree`]'s,
    /// and for the same reason: the embedder's `FlakeRef::fromAttrs` owns the
    /// schema. What is decided here, before the question: forcing each
    /// attribute, the negative-integer error and the wrong-type error, both
    /// of which `prim_flakeRefToString` raises on values the interpreter had
    /// to produce (`flake-primops.cc`). The answer is the reference string.
    FlakeRefToString(std::collections::BTreeMap<String, TreeAttr>),

    /// The raw bytes of the file at this path, as a Nix string, for
    /// `builtins.readFile` and for `builtins.wasm` (its module, and the
    /// guest's `read_file`). Answered from [`crate::host::Host::read_file_bytes`]:
    /// a Nix string is a byte string, and cppnix's `readFile` hands back
    /// whatever the file holds, so an answer repaired to UTF-8 would be a
    /// different file (it was, until `builtins.wasm` needed its `.wasm`
    /// binary through here).
    Contents(Rc<crate::value2::PathValue>),
    /// Hash the file at this path and answer the base16 digest, for
    /// `builtins.hashFile`.
    ///
    /// # Why the digest travels back and the bytes do not
    ///
    /// cppnix's `prim_hashFile` (`primops.cc:2432`) hashes the raw bytes of
    /// the file and never builds an eval string from them. Neither does this:
    /// the algorithm travels with the question, the answering side reads the
    /// bytes ([`crate::host::Host::read_file_bytes`]) and hashes them, and no
    /// `Value` ever carries a file that is only being digested. Historically
    /// this is also where ENG-13146 was fixed: [`NeedPath::Contents`] used to
    /// answer text, repairing invalid UTF-8 to U+FFFD, and a digest of that
    /// was a digest of a file that does not exist.
    HashFile {
        path: Rc<crate::value2::PathValue>,
        algo: crate::nixhash::HashAlgo,
    },
    Exists(Rc<crate::value2::PathValue>),
    /// The trailing-slash half of `builtins.pathExists`: whether `path` is a
    /// DIRECTORY, under full symlink resolution.
    ///
    /// cppnix's `prim_pathExists` looks at the argument *before* coercion --
    /// "SourcePath doesn't know about trailing slash" (`primops.cc:2105`) --
    /// and a string ending in `/` or `/.` must name a directory: the lstat
    /// runs on the fully resolved path (`SymlinkResolution::Full` where the
    /// plain question resolves ancestors only) and anything but a directory
    /// answers `false`. A separate question from [`NeedPath::Exists`]
    /// because both the resolution and the predicate differ; its dedicated
    /// [`crate::host::Host::dir_exists_checked`] operation preserves the
    /// third, error outcome that an import's type query does not have.
    DirExists(Rc<crate::value2::PathValue>),
    Entries(Rc<crate::value2::PathValue>),
    /// `builtins.readFileType`: the type of a path that must be there.
    /// cppnix's `SourceAccessor::lstat`, which is `maybeLstat` plus a throw
    /// (`source-accessor.cc:73`). The throw is this side's, in
    /// [`crate::eval::answer`], not the embedder's -- see [`NeedPath::MaybeKind`].
    Kind(Rc<crate::value2::PathValue>),
    /// The type of a path that may not be there: cppnix's
    /// `SourceAccessor::maybeLstat`, answering `null` where [`NeedPath::Kind`]
    /// fails.
    ///
    /// # Why absence is a value here and a failure there
    ///
    /// One host method answers both ([`crate::host::Host::file_type`]), for
    /// the reason cppnix has one accessor method: they are the same read.
    /// What differs is who wants it. `builtins.readFileType` has cppnix's
    /// `lstat` behind it and a missing path is an evaluation error there.
    /// The symlink scan in [`crate::primops_host::PathBuiltin`] has cppnix's
    /// `resolveSymlinks` behind it (`source-accessor.cc:91`), which
    /// `maybeLstat`s every component of a path and treats a component the
    /// accessor cannot see as "not a symlink" -- it even records the
    /// observation as the literal string `absent`. A path nobody can see is
    /// not a symlink, so it cannot be one of the roots that scan refuses.
    ///
    /// Collapsing the two would have to pick one contract, and each is a real
    /// bug in the other's caller: erroring here fails every filtered
    /// `builtins.path` under pure eval (ENG-13123), and answering `null`
    /// there makes `builtins.readFileType` of a missing file evaluate to
    /// `null` instead of failing.
    MaybeKind(Rc<crate::value2::PathValue>),
    /// An environment variable, for `builtins.getEnv`.
    Env(String),
    /// Copy this path into the store and answer with the store path, for a
    /// path interpolated into a string. Not a read: it is the one question
    /// whose answer depends on a store rather than only on the filesystem,
    /// and it is here for the same reason `Env` is -- a coercion that reached
    /// the world some other way would be invisible to a read set.
    StorePath(Rc<crate::value2::PathValue>),
    /// Use an existing store object through `builtins.storePath`. Unlike
    /// `StorePath`, this does not copy bytes. The host validates the rooted
    /// source, ensures the object when cppnix would, and returns both the
    /// visible path and the store object carried in its string context.
    UseStorePath(Rc<crate::value2::PathValue>),
    /// Store this text and answer with its store path, for `builtins.toFile`.
    ///
    /// A store question like `StorePath`, and routed for the same reason, but
    /// it cannot be a path copy: cppnix hashes the *bytes* as `text` with the
    /// declared references stuffed into the path's type string, and whether it
    /// writes them at all depends on `readOnlyMode` -- which is the embedder's
    /// setting, not this evaluator's. `nix-instantiate --eval` computes the
    /// path without writing; `nix build` writes. Guessing either way from here
    /// would be right in one and wrong in the other, so the branch stays on
    /// the far side of this question (the shape `rustEnsurePath` already uses,
    /// ENG-12479). ENG-12607.
    StoreText {
        name: String,
        contents: String,
        /// The `Opaque` context elements of `contents`, in store-path order.
        /// cppnix refuses a derivation reference before it gets here.
        references: Vec<String>,
    },
    /// Put a finished `.drv` in the store, for `builtins.derivationStrict`.
    ///
    /// # Why this is not a `StoreText`
    ///
    /// The store operation is the same one: cppnix's `writeDerivation`
    /// (`derivations.cc:170`) is `addTextToStore` of the ATerm under
    /// `<name>.drv` with the input sources and input derivations as
    /// references. What differs is the *contract on the answer*, in two ways
    /// that matter:
    ///
    /// * `expected` is already known. The evaluator computed the path from
    ///   these very bytes on its way here, so an answer that disagrees is not
    ///   a value to accept but a defect to report: the store and this
    ///   evaluator would be naming two different derivations. `toFile` has no
    ///   such cross-check, because there the store's answer is the only one.
    /// * A host with no store may decline. The store path does not depend on
    ///   whether anything was written, so `StoreError::NoStore` here means
    ///   "nothing was written and the path stands" -- cppnix's `readOnlyMode`
    ///   branch -- rather than the refusal it means for `toFile`, where a
    ///   computed path nobody wrote is a wrong answer. That is what keeps a
    ///   hostless evaluation (`cargo test`, `examples/nixpkgs-probe.rs`) able
    ///   to answer `hello.outPath`, which is the frontier's milestone row.
    ///
    /// Folding the two into one question would have to pick one of those
    /// contracts, and either choice is wrong for the other caller. ENG-12799.
    WriteDrv {
        /// The derivation's name **without** the `.drv` suffix, which the
        /// embedder appends exactly as `writeDerivation` does. Handed over
        /// unsuffixed so the embedder's own `addTextToStore` call reads the
        /// same as cppnix's.
        name: String,
        /// The ATerm, already rendered. Nothing re-renders it: two renderings
        /// of one derivation are two chances to disagree about the path.
        aterm: String,
        /// Where the evaluator computed this `.drv` goes. The embedder is not
        /// asked to trust it: it is here so a disagreement is caught at the
        /// derivation that caused it rather than as a missing path much later.
        expected: String,
    },
    /// Copy a filtered tree into the store and answer with its store path,
    /// for `builtins.path`.
    ///
    /// # Why this exists rather than reusing `StorePath`
    ///
    /// cppnix applies the filter *during* the copy: `dumpPath` calls it once
    /// per directory entry it is about to serialise (`libutil/archive.cc:99`),
    /// and the filter is an ordinary Nix function. Running it needs the
    /// interpreter, which is on this side of the question, so "copy this
    /// directory" cannot express a filtered copy -- the walk has to interleave
    /// with evaluation. The evaluator therefore does the walk, through
    /// `Entries` and `Kind`, and hands over the finished list.
    ///
    /// That is also what keeps a filtered copy visible to a read set: every
    /// directory the filter saw is a recorded `Entries` question, so a file
    /// appearing in one of them invalidates the memoised result, and the
    /// answer to this question is the store path, which is a content hash of
    /// what was copied.
    ///
    /// # What the embedder must guarantee
    ///
    /// The answer is byte-identical to what cppnix's `addPath` would produce
    /// for the same name, method and content. Nothing here is a hint:
    ///
    /// * `accepted = Some(list)` means include exactly these paths below
    ///   `root` and nothing else. The embedder re-decides nothing; it may
    ///   still re-`lstat` and read bytes, which it must do anyway to build the
    ///   archive. The list is closed downwards -- a directory that is not in
    ///   it has no descendants in it -- so a membership test is a correct
    ///   `PathFilter`.
    /// * `accepted = None` means copy the whole tree, cppnix's
    ///   `defaultPathFilter`. It is sent when no `filter` attribute was given,
    ///   and when the method is `Flat`, where cppnix's ingestion never
    ///   consults a filter at all.
    /// * `expected_sha256` is the `sha256` attribute, already parsed and
    ///   re-rendered as SRI (so an empty attribute arrives as the all-zero
    ///   hash cppnix's `newHashAllowEmpty` substitutes, and a malformed one
    ///   never gets here). When it is set, cppnix computes the fixed-output
    ///   path from it, copies only if that path is not already valid, and
    ///   errors on a mismatch (`primops.cc:2967`). An embedder that cannot do
    ///   that must fail rather than answer with a path it did not check.
    StoreFiltered(Box<FilteredCopy>),
    /// Fetch a URL into the store and answer with the store path, for
    /// `builtins.fetchurl` and `builtins.fetchTarball`.
    ///
    /// # Why the whole fetch is one question
    ///
    /// Fetching is IO, and this evaluator does none. Downloading, unpacking,
    /// substituting, the CA cache under `~/.cache/nix/tarball-cache` and
    /// every retry policy around them belong to `libfetchers`, which the
    /// embedder already links; a second implementation here would be a second
    /// set of answers for a store path to differ over. So the evaluator does
    /// only the part cppnix's `fetch()` does before it touches the world --
    /// read the argument, default the name, validate it -- and hands the rest
    /// over.
    ///
    /// # What the embedder must guarantee
    ///
    /// The answer is byte-identical to the store path cppnix's `fetch()`
    /// (`primops/fetchTree.cc:462`) produces for the same arguments, and the
    /// two branches it takes are both the embedder's:
    ///
    /// * With `expected_sha256` set, cppnix computes the fixed-output path
    ///   from it and calls `ensurePath`; if that succeeds nothing is
    ///   downloaded and that path is the answer. **This branch is what makes
    ///   evaluation in CI hermetic**, and it is decided on the far side on
    ///   purpose: whether the store already holds a path is a fact about the
    ///   store, and an evaluator that guessed would either miss a cached
    ///   fetch or claim one that is not there.
    /// * Otherwise the content is downloaded, and a mismatch against
    ///   `expected_sha256` is an error rather than a path.
    ///
    /// # What is decided here and not there
    ///
    /// `name` is final: cppnix's defaulting (`baseNameOf` of the URL for
    /// `fetchurl`, `"source"` for `fetchTarball`, either overridden by a
    /// `name` attribute) and its `checkName` validation both happen before
    /// the question, because both are pure string rules and one of them
    /// raises an evaluation error the program can see. `url` is final too,
    /// including `fetchTarball`'s `resolvePseudoUrl` rewrite of a `channel:`
    /// URL. The embedder re-derives neither.
    Fetch(Box<FetchRequest>),
    /// Fetch a tree through `libfetchers` and answer with the attribute set
    /// cppnix's `emitTreeAttrs` builds, for `builtins.fetchTree` and
    /// `builtins.fetchGit`.
    ///
    /// # Why the payload is a bag of attributes rather than a struct
    ///
    /// cppnix's `fetchTree()` (`primops/fetchTree.cc:236`) does not know the
    /// schemes either. It forces each attribute, classifies it as a string, a
    /// Boolean or a non-negative integer, and hands the bag to
    /// `Input::fromAttrs`, which dispatches on `type` and decides what is
    /// meaningful. A `rev` means something to `git` and nothing to `path`.
    /// So the evaluator classifies and forwards, exactly as cppnix does, and
    /// a struct here would be this crate inventing a schema for a set of
    /// fetchers it does not own.
    ///
    /// # What is decided here and what is not
    ///
    /// Here: forcing each attribute in the set's own order, the string-or-path
    /// coercion, the type error for anything else, the negative-integer error,
    /// and the three shape errors (`type` given twice, `type` missing, `name`
    /// where it is not allowed). Every one of those is an evaluation error a
    /// program can see, raised on a value the interpreter had to produce.
    ///
    /// Not here, and deliberately: `fixGitURL`, the `exportIgnore` and
    /// `shallow` defaults, the registry lookup, the pure-eval locked-input
    /// check, `__final`, the input cache and the mount. Those are how an
    /// `Input` is *built and fetched*, they live beside `Input::fromAttrs`,
    /// and `fetcher` is in the question precisely so the embedder can apply
    /// the right ones. Reproducing `fixGitURL` here would mean a second URL
    /// parser deciding a store path, which is the quietest possible way to be
    /// wrong.
    ///
    /// # The answer
    ///
    /// JSON: the whole attribute set `emitTreeAttrs` produced, whose shape
    /// depends on the input type and on what the fetcher found (`narHash`,
    /// `rev`, `shortRev`, `revCount`, `lastModified`, `lastModifiedDate`,
    /// `submodules`, `dirtyRev`, a nested `history`). The evaluator parses it
    /// with the same reader `builtins.fromJSON` uses and then replaces
    /// `outPath` with a string carrying that store path as its context --
    /// which is the one thing JSON cannot express and the one thing a
    /// derivation depending on the tree needs.
    FetchTree(Box<FetchTreeRequest>),
    /// Make this store path present, for a `builtins.appendContext` key.
    /// Like `StorePath` it is a store question rather than a read, and it is
    /// here rather than inside the builtin so that a read set can see it.
    EnsurePath(String),
    /// Realise a string's context before the string is used as a path:
    /// cppnix's `EvalState::realiseContext` (`primops.cc:72`), which is what
    /// makes **import from derivation** work.
    ///
    /// Every builtin that takes a path reaches its file through
    /// `EvalState::realisePath` (`primops.cc:167`), and the first thing that
    /// does with a non-empty context is call `realiseContext` and rewrite the
    /// path with what comes back. So `import (drv + "/x")`, `readFile
    /// drv.outPath` and `builtins.path { path = drv; }` all want a
    /// *derivation built* in the middle of evaluation. This is the question
    /// that asks for it.
    ///
    /// # Why the whole context travels, not only the derivation outputs
    ///
    /// `realiseContext` is a function of the whole context and it does two
    /// separable things with it. Every element -- `Opaque`, `DrvDeep` and
    /// `Built` alike -- is checked for validity, and a missing one raises
    /// cppnix's `InvalidPathError`, which is a failure a program can see. Only
    /// the `Built` elements are then collected into a build. Sending just the
    /// `Built` ones would be a partial transcription that silently dropped the
    /// first half, so an evaluation naming a store path the store had lost
    /// would read it anyway rather than failing the way cppnix fails.
    ///
    /// It also means the common case costs nothing surprising: a context of
    /// plain `Opaque` elements -- what a path interpolation leaves behind --
    /// is a validity check and an empty answer, because `realiseContext`
    /// returns `{}` as soon as it finds no derivations to build.
    ///
    /// The elements arrive in `BTreeSet` order, which is
    /// `Opaque < DrvDeep < Built` and then by path. Order is fixed rather
    /// than incidental because it is part of a read-set key.
    ///
    /// # What the embedder must guarantee
    ///
    /// It performs cppnix's `realiseContext` and nothing less:
    ///
    /// * `isValidPath` on every element's base store path, raising cppnix's
    ///   own "path '%s' is not valid" for a miss;
    /// * `allow-import-from-derivation`, which is checked **there and not
    ///   here**. It is an `EvalSettings` field this crate is not given, and
    ///   the refusal it produces is an `IFDError` cppnix raises before any
    ///   build (`primops.cc:118`), so an embedder that skipped it would build
    ///   where cppnix refuses. `trace-import-from-derivation` rides along with
    ///   it for the same reason.
    /// * `buildPaths` on the `Built` elements, as one request rather than one
    ///   per output, because cppnix builds them together and a failure names
    ///   the set;
    /// * `copyClosure` to the evaluation store when the two differ, which is
    ///   what makes `--eval-store` work, and `allowClosure` on each output so
    ///   `restrict-eval` lets the read that follows through.
    ///
    /// # The answer is a rewrite map, and that is why this is not `EnsurePath`
    ///
    /// `realiseContext` returns a `StringMap`. Under `ca-derivations` the
    /// path the evaluator is holding contains a *downstream placeholder* --
    /// a hash standing in for an output whose store path is not known until
    /// it is built -- and the map says what each placeholder became. The
    /// caller applies it with `rewriteStrings` and only then reads. Without
    /// the answer travelling back, a CA derivation output would be read at a
    /// path that never exists.
    ///
    /// The map is empty for every input-addressed derivation, which is nearly
    /// all of them, and an embedder without the experimental feature enabled
    /// answers empty always. That is not a reason to drop it from the wire:
    /// an empty map and a map nobody sent look the same to the reader and
    /// differ in exactly the case this exists for.
    Realise(Vec<ContextElem>),
    /// Report a warning. Not a question at all, and here anyway for the same
    /// reason `Env` is: writing it from inside a builtin would make the "the
    /// VM performs no IO" claim in `host` false, and the answer (`null`)
    /// carries nothing precisely because there is nothing to carry.
    Warn(String),
    /// Print a trace line, for `builtins.trace`. A second output-shaped
    /// variant beside `Warn` rather than a reuse of it, because cppnix sends
    /// the two to different places: `warn` builds an `ErrorInfo` at `lvlWarn`
    /// and `trace` calls `printError` with its own `trace: ` prefix
    /// (`primops.cc:1325`), and which one a line came from is visible to
    /// anyone reading stderr.
    Trace(String),
    /// Resolve `name` against `entries`, for `builtins.findFile` and so for
    /// `<nixpkgs>`, which cppnix desugars into a call to it.
    ///
    /// The entries travel with the question rather than being looked up on
    /// the far side, because the list is an ordinary Nix value the program
    /// can supply or rebind: `let __nixPath = [...]; in <a.nix>` resolves
    /// against the local binding in cppnix, and a question naming only the
    /// file would silently answer from the process's `-I` flags instead.
    /// ENG-12443.
    FindFile {
        entries: Vec<SearchPathEntry>,
        name: String,
    },
    /// The default search path: cppnix's `builtins.nixPath`, which is the
    /// `-I` flags and `NIX_PATH` this process was started with.
    ///
    /// A question rather than configuration handed over at setup, unlike the
    /// store directory and the version string, because it changes what an
    /// expression evaluates to. A read set that did not carry it would let a
    /// memoised result survive a change of `-I` that moves the file the
    /// evaluation read.
    NixPath,
}

/// One entry of a search path: cppnix's `LookupPath::Elem`, whose two halves
/// are both plain strings by the time `builtins.findFile` sees them
/// (`primops.cc:2283`). Named rather than a pair because the two are the same
/// type and swapping them produces a lookup that silently finds nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPathEntry {
    /// Matched against the front of the sought name; empty matches anything.
    pub prefix: String,
    /// Where to look. Not necessarily a filesystem path: cppnix resolves a
    /// URL here by downloading it, which is one of the reasons the lookup
    /// belongs to the embedder.
    pub path: String,
}

/// How the bytes under a path become a content address. cppnix's
/// `ContentAddressMethod::Raw`, narrowed to the two `builtins.path` can pick:
/// `recursive = true` (its default) serialises the tree as a NAR, and
/// `recursive = false` hashes the bytes of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMethod {
    NixArchive,
    Flat,
}

impl PathMethod {
    /// The spelling that travels over the C ABI and into a witness. Written
    /// out rather than derived, because it is a wire format.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PathMethod::NixArchive => "nar",
            PathMethod::Flat => "flat",
        }
    }

    /// The inverse of [`PathMethod::as_str`].
    #[must_use]
    pub fn parse(s: &str) -> Option<PathMethod> {
        match s {
            "nar" => Some(PathMethod::NixArchive),
            "flat" => Some(PathMethod::Flat),
            _ => None,
        }
    }
}

/// One path the filter accepted, with the type the walk saw it as.
///
/// The type travels with the path so an embedder that stages the tree
/// somewhere else (the probe does) does not have to re-derive it, and so an
/// embedder that reads it back can check the two agree instead of silently
/// copying something else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedPath {
    /// Absolute, below the request's `root`.
    pub path: String,
    pub file_type: crate::host::FileType,
}

/// A filtered copy into the store: the whole of [`NeedPath::StoreFiltered`]'s
/// payload, which is also what [`crate::host::Host::store_filtered`] takes.
///
/// One struct rather than six arguments because it crosses three boundaries
/// (the question, the trait, the C ABI) and a positional list of strings at
/// each was how `references` and `contents` got swapped once already.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredCopy {
    /// The directory or file to copy, absolute. Not symlink-resolved: cppnix
    /// resolves it inside `addPath`, and so must the embedder.
    pub root: Rc<crate::value2::PathValue>,
    /// The store path's name. cppnix defaults it to the root's base name, and
    /// that default is applied before the question is asked.
    pub name: String,
    pub method: PathMethod,
    /// The filter's verdicts, or `None` for "no filtering". See
    /// [`NeedPath::StoreFiltered`].
    pub accepted: Option<Vec<AcceptedPath>>,
    /// The `sha256` attribute as SRI, or `None` when it was absent.
    pub expected_sha256: Option<String>,
    /// Whether the copy inherits the references of the store object `root`
    /// lives in: cppnix's `addPath` does that, and only that, when the root
    /// coerced with a non-empty context and is already in the store
    /// (`primops.cc:2947`).
    ///
    /// A flag and not the reference list, because the two halves of that
    /// branch belong on different sides of this question. *Whether* it
    /// applies is a pure test on the value the evaluator coerced -- it had a
    /// context, and it is under the store directory -- which only the
    /// evaluator can make. *What* the references are is
    /// `queryPathInfo(toStorePath(root))`, a store query only the embedder can
    /// make. Sending a list would mean the evaluator inventing one; sending
    /// nothing would mean the embedder guessing whether to look, and it would
    /// guess wrong for a root that is in the store and carried no context.
    ///
    /// It changes the answer, which is why it is here rather than being
    /// re-derived: the references go into the content address, so a copy that
    /// dropped them lands on a different, well-formed, wrong store path.
    /// The evaluator has already realised the context and rewritten `root` by
    /// the time this is sent, so the query is against the built output.
    pub inherit_references: bool,
}

impl FilteredCopy {
    /// How many paths the filter accepted, or `unfiltered`, for a trace line.
    #[must_use]
    pub fn accepted_label(&self) -> String {
        self.accepted.as_ref().map_or_else(
            || "unfiltered".to_owned(),
            |accepted| accepted.len().to_string(),
        )
    }
}

/// Which of cppnix's two fixed-output fetchers is being asked for.
///
/// Named rather than the bare `unpack` bool cppnix's `fetch()` takes,
/// because the flag decides three things at once -- the ingestion method,
/// the default name, and whether a `channel:` URL is rewritten -- and a bare
/// `true` at a call site says none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchKind {
    /// `builtins.fetchurl`: the bytes as they arrive, ingested flat.
    File,
    /// `builtins.fetchTarball`: unpacked, ingested as a NAR.
    Tarball,
}

impl FetchKind {
    /// The spelling that travels over the C ABI and into a witness. Written
    /// out rather than derived, because it is a wire format.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FetchKind::File => "file",
            FetchKind::Tarball => "tarball",
        }
    }

    /// The inverse of [`FetchKind::as_str`].
    #[must_use]
    pub fn parse(s: &str) -> Option<FetchKind> {
        match s {
            "file" => Some(FetchKind::File),
            "tarball" => Some(FetchKind::Tarball),
            _ => None,
        }
    }

    /// cppnix's `who`: the primop's name, which appears in three of its error
    /// messages, so it is derived from the kind rather than carried beside it
    /// where the two could disagree.
    #[must_use]
    pub fn who(self) -> &'static str {
        match self {
            FetchKind::File => "fetchurl",
            FetchKind::Tarball => "fetchTarball",
        }
    }

    /// The ingestion method cppnix's `fetch()` picks from its `unpack` flag.
    #[must_use]
    pub fn method(self) -> PathMethod {
        match self {
            FetchKind::File => PathMethod::Flat,
            FetchKind::Tarball => PathMethod::NixArchive,
        }
    }
}

/// A fetch into the store: the whole of [`NeedPath::Fetch`]'s payload, which
/// is also what [`crate::host::Host::fetch`] takes. One struct rather than
/// four arguments, for the reason [`FilteredCopy`] is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    /// The URL to fetch, after `resolvePseudoUrl`. Not validated against
    /// `allowed-uris` here: `checkURI` reads `restrict-eval`, which is the
    /// embedder's setting.
    pub url: String,
    /// The store object's name, already defaulted and already through
    /// [`crate::storepath::check_name`].
    pub name: String,
    pub kind: FetchKind,
    /// The `sha256` attribute as SRI, or `None` when it was absent. An empty
    /// attribute arrives as the all-zero hash cppnix's `newHashAllowEmpty`
    /// substitutes, so `Some` here means "the program pinned something",
    /// which is the distinction the early-exit branch turns on.
    pub expected_sha256: Option<String>,
}

/// Which of cppnix's three tree entry points was called: its
/// `FetchTreeParams`, carried as the identity of the call rather than as the
/// flags it stands for.
///
/// The identity travels because the embedder already has the struct, and
/// three booleans reassembled on the far side are three chances to reassemble
/// them wrong. What each variant means in cppnix
/// (`primops/fetchTree.cc`):
///
/// * `Tree` -- `prim_fetchTree`, no params set.
/// * `Git` -- `prim_fetchGit`, which sets `emptyRevFallback`,
///   `allowNameArgument` and `isFetchGit`.
/// * `FinalTree` -- `prim_fetchFinalTree`, which sets `isFinal` and nothing
///   else, so every rule in the argument walk is `Tree`'s. It is
///   `.internal = true`, so no program can name it; the flake machinery
///   receives it as `call-flake.nix`'s third argument, and this crate hands
///   it over through `ixe_internal_primop`.
///
/// A third variant rather than a Boolean beside `fetcher`, because this value
/// is what the C ABI and the read-set witness both carry, and a witness field
/// that exists in one of the two encodings is a witness that decodes to the
/// wrong question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeFetcher {
    Tree,
    Git,
    FinalTree,
}

impl TreeFetcher {
    /// The spelling that travels over the C ABI and into a witness.
    ///
    /// **Not the name cppnix puts in its error messages** -- that is
    /// [`TreeFetcher::error_name`], which calls a final fetch `fetchTree`
    /// because cppnix's `fetcher` variable is derived from `isFetchGit`
    /// alone. The two were one function until `FinalTree` existed, and
    /// keeping them one would have made every error text from a flake input
    /// name a primop no program can call.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TreeFetcher::Tree => "fetchTree",
            TreeFetcher::Git => "fetchGit",
            TreeFetcher::FinalTree => "fetchFinalTree",
        }
    }

    /// cppnix's `fetcher` local (`fetchTree.cc:186`), which is the name every
    /// error raised by the argument walk interpolates.
    #[must_use]
    pub fn error_name(self) -> &'static str {
        if self.is_fetch_git() {
            "fetchGit"
        } else {
            "fetchTree"
        }
    }

    /// cppnix's `params.isFetchGit`: the only flag the argument walk reads.
    #[must_use]
    pub fn is_fetch_git(self) -> bool {
        matches!(self, TreeFetcher::Git)
    }

    /// cppnix's `params.isFinal`, which the argument walk does not read at
    /// all -- it decides whether the embedder sets `__final` on the input or
    /// rejects an input that already carries it.
    #[must_use]
    pub fn is_final(self) -> bool {
        matches!(self, TreeFetcher::FinalTree)
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<TreeFetcher> {
        match s {
            "fetchTree" => Some(TreeFetcher::Tree),
            "fetchGit" => Some(TreeFetcher::Git),
            "fetchFinalTree" => Some(TreeFetcher::FinalTree),
            _ => None,
        }
    }
}

/// One input attribute, in the three shapes `fetchers::Attrs` holds.
///
/// cppnix's `fetchers::Attr` is `variant<string, uint64_t, Explicit<bool>>`
/// and this mirrors it. An integer is unsigned because the primop rejects a
/// negative one before it gets this far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeAttr {
    Str(String),
    Bool(bool),
    Int(u64),
}

impl TreeAttr {
    /// The tag that travels beside the value on the wire. One letter, because
    /// the alternative -- inferring the type from the text -- makes the string
    /// `"true"` a Boolean.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            TreeAttr::Str(_) => "s",
            TreeAttr::Bool(_) => "b",
            TreeAttr::Int(_) => "i",
        }
    }

    /// The value as it travels beside its tag.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            TreeAttr::Str(s) => s.clone(),
            TreeAttr::Bool(b) => if *b { "1" } else { "0" }.to_owned(),
            TreeAttr::Int(n) => n.to_string(),
        }
    }

    /// The inverse of [`TreeAttr::tag`] and [`TreeAttr::text`]. `None` for a
    /// tag this build does not know or a value that does not parse, which is
    /// a malformed request rather than an attribute to guess at.
    #[must_use]
    pub fn parse(tag: &str, text: &str) -> Option<TreeAttr> {
        match tag {
            "s" => Some(TreeAttr::Str(text.to_owned())),
            "b" => match text {
                "0" => Some(TreeAttr::Bool(false)),
                "1" => Some(TreeAttr::Bool(true)),
                _ => None,
            },
            "i" => text.parse().ok().map(TreeAttr::Int),
            _ => None,
        }
    }
}

/// A tree fetch: the whole of [`NeedPath::FetchTree`]'s payload, which is also
/// what [`crate::host::Host::fetch_tree`] takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchTreeRequest {
    /// The input attributes, including `type`, in name order. A `BTreeMap`
    /// rather than the argument set's own order because `fetchers::Attrs` is
    /// itself a `std::map<std::string, Attr>` -- the far side sorts anyway,
    /// and sorting here makes the read-set key independent of interning
    /// order.
    pub attrs: std::collections::BTreeMap<String, TreeAttr>,
    pub fetcher: TreeFetcher,
}

impl FetchTreeRequest {
    /// Whether this is a final tree with a `narHash`: `fetchFinalTree` over
    /// a locked input, whose every emitted attribute is one of the locked
    /// ones or computed from them (`emitTreeAttrs`; the `outPath` is the
    /// fixed-output path of the `narHash`), so the answer is a function of
    /// `attrs`, and asking again could return nothing else, or fail to
    /// fetch. The read-set replay takes such a row from its record
    /// (`readset::Question::answer_by_validity`).
    #[must_use]
    pub fn locked_final(&self) -> bool {
        self.fetcher == TreeFetcher::FinalTree && self.attrs.contains_key("narHash")
    }
}

pub enum Task {
    Builtin {
        idx: u16,
        args: Vec<Slot>,
        cont: Cont,
    },
    DeepEq(DeepEq),
    Compare(Compare),
    ResolveWith(ResolveWith),
    Print(Print),
    Coerce(Coerce),
    ApplyChain(ApplyChain),
    /// Calling an attribute set that carries `__functor`. See [`Functor`].
    Functor(Functor),
    /// `builtins.nixPath`, which is one question and its answer. A task
    /// rather than an op that suspends in place, because an op resumes by
    /// re-running itself and this one has no stack shape to distinguish "not
    /// asked yet" from "answered"; the task machinery already carries that
    /// distinction as `incoming`.
    NixPath,
}

impl Yield {
    /// The one constructor for [`Yield::Sub`]. The box is one allocation per
    /// sub-task start; from then on the task moves as one word, so the yield
    /// and the frame that carries it stay the size of their small variants.
    pub fn sub(task: Task) -> Yield {
        Yield::Sub(Box::new(task))
    }
}

impl Task {
    pub fn builtin(idx: u16, args: Vec<Slot>) -> Task {
        Task::Builtin {
            idx,
            args,
            cont: Cont::Args(0),
        }
    }

    pub fn deep_eq(l: Value, r: Value, negate: bool) -> Task {
        Task::DeepEq(DeepEq::new(Slot::value(l), Slot::value(r), negate))
    }

    pub fn deep_eq_slots(l: Slot, r: Slot) -> Task {
        Task::DeepEq(DeepEq::new(l, r, false))
    }

    pub fn compare(l: Value, r: Value, negate: bool) -> Task {
        Task::Compare(Compare::new(l, r, negate))
    }

    pub fn resolve_with(env: Env, sym: Sym) -> Task {
        Task::ResolveWith(ResolveWith {
            node: env,
            sym,
            stage: Stage::Walk,
        })
    }

    /// The same coercion with cppnix's `copyToStore` on, which is what a
    /// derivation attribute uses. See [`Coerce`].
    pub fn coerce_copying(slot: Slot) -> Task {
        Task::Coerce(Coerce::copying(slot))
    }

    /// The coercion a string literal performs on an interpolated set:
    /// `copyToStore` on, `coerceMore` off. See [`Coerce`].
    pub fn interpolate(slot: Slot) -> Task {
        Task::Coerce(Coerce::interpolating(slot))
    }

    /// `builtins.concatStringsSep`, elements and separator together. See
    /// [`Coerce`].
    pub fn coerce_joining(items: &[Slot], sep: &crate::value2::NixStr) -> Task {
        Task::Coerce(Coerce::joining(items, sep))
    }

    /// One operand of `+`. `coerceMore` is off, as it is for interpolation,
    /// but `copyToStore` is the caller's: cppnix takes it from the type of the
    /// concatenation's *first* part rather than from the part being coerced.
    /// See [`Coerce`].
    pub fn concat_coerce(slot: Slot, copy_to_store: bool) -> Task {
        Task::Coerce(Coerce::concatenating(slot, copy_to_store))
    }

    /// The coercion inside cppnix's `EvalState::coerceToPath`, which is what
    /// every builtin taking a path argument runs on it. See [`Coerce::to_path`].
    pub fn coerce_to_path(slot: Slot) -> Task {
        Task::Coerce(Coerce::to_path(slot))
    }

    /// The coercion `builtins.toJSON` applies to a `__toString` result. See
    /// [`Coerce::to_json_string`].
    pub fn coerce_to_json_string(slot: Slot) -> Task {
        Task::Coerce(Coerce::to_json_string(slot))
    }

    /// A primop argument, coerced with that primop's own `coerceToString`
    /// flags. Driven by the builtin driver for every position
    /// [`crate::builtins::TABLE`] declares `ArgType::Coerce`, so the body sees
    /// a string. See [`Coerce::as_primop`].
    pub fn coerce_as_primop(slot: Slot, flags: crate::print::CoerceFlags) -> Task {
        Task::Coerce(Coerce::as_primop(slot, flags))
    }

    /// `f a b ...`, one application per step. Backs `SlotState::PendingApply`,
    /// so a lazily-applied value costs frames rather than host stack however
    /// many arguments it carries.
    ///
    /// The callee arrives unforced and is forced here, at the first
    /// application, which is where cppnix forces the left of a `tApp`. A
    /// builtin that forces it earlier is strict in a position cppnix is lazy
    /// in; see `SlotState::PendingApply` (ENG-13124).
    pub fn apply_chain(f: Slot, args: Vec<Slot>) -> Task {
        Task::ApplyChain(ApplyChain {
            f: None,
            callee: Some(f),
            args,
            i: 0,
        })
    }

    pub fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        match self {
            Task::Builtin { idx, args, cont } => {
                primops_pure::drive(vm, *idx, args, cont, incoming)
            }
            Task::DeepEq(d) => d.step(vm),
            Task::Compare(c) => c.step(),
            Task::ResolveWith(r) => r.step(vm, incoming),
            Task::Print(p) => p.step(vm, incoming),
            Task::Coerce(c) => c.step(vm, incoming),
            Task::ApplyChain(a) => a.step(incoming),
            Task::Functor(f) => f.step(incoming),
            Task::NixPath => match incoming {
                Some(v) => Ok(Yield::Done(v)),
                None => Ok(Yield::Need(NeedPath::NixPath)),
            },
        }
    }

    /// `tryEval` is the only barrier in the language: it turns a catchable
    /// failure from the expression it is forcing into a value. Every other
    /// frame lets the error keep unwinding.
    pub fn catch(&self, vm: &mut Vm, e: &VmError) -> Option<Value> {
        match self {
            Task::Builtin {
                cont: Cont::TryEval { started: true },
                ..
            } => match e {
                VmError::Throw(c) if c.catchable => {
                    Some(primops_pure::try_eval_result(vm, false, Value::Bool(false)))
                }
                _ => None,
            },
            _ => None,
        }
    }
}

/// A deferred application, spent one argument at a time.
/// Calling a set: `set arg` where `set` has a `__functor` attribute means
/// `set.__functor set arg`, cppnix's `callFunction` at `eval.cc:1880`.
///
/// The set is passed as the first argument, which is what makes
/// `{ __functor = self: ...; }` able to see its own attributes, and is why
/// this cannot be rewritten as a plain two-argument application of something
/// already in hand: the `__functor` attribute is a `Slot` and may be a thunk,
/// so it has to be forced first.
///
/// One argument per call, not all of them: `f 1 2` re-enters the apply path
/// for the second argument, so a functor returning a function composes the
/// way cppnix's does.
pub struct Functor {
    /// The set being called, already forced -- it is the callee.
    set: Value,
    /// The argument the caller applied.
    arg: Slot,
    /// The `__functor` attribute, unforced until this task runs.
    functor: Option<Slot>,
    /// How many of the two applications have been made. Two, not one:
    /// `set.__functor set arg` is `(functor set) arg`, and the value
    /// delivered after the first is a partially applied function, not the
    /// answer. Getting this wrong returns the partial application as the
    /// result, which type-checks and is wrong.
    applied: u8,
}

impl Functor {
    pub fn new(set: Value, functor: Slot, arg: Slot) -> Self {
        Functor {
            set,
            arg,
            functor: Some(functor),
            applied: 0,
        }
    }

    fn step(&mut self, incoming: Option<Value>) -> Result<Yield> {
        if let Some(slot) = self.functor.take() {
            return Ok(Yield::Force(slot));
        }
        let value = incoming.ok_or_else(|| VmError::eval("internal: functor lost its function"))?;
        match self.applied {
            // cppnix hands the functor a heap copy of the set rather than the
            // caller's cell (`eval.cc:1883`). Here every value is already
            // reference-counted and immutable, so a clone is that copy.
            0 => {
                self.applied = 1;
                Ok(Yield::Apply(value, Slot::value(self.set.clone())))
            }
            1 => {
                self.applied = 2;
                Ok(Yield::Apply(value, self.arg.clone()))
            }
            _ => Ok(Yield::Done(value)),
        }
    }
}

pub struct ApplyChain {
    f: Option<Value>,
    /// The unforced callee, taken on the first step. `None` afterwards: from
    /// then on the function lives in `f`, refreshed by each application's
    /// result.
    callee: Option<Slot>,
    args: Vec<Slot>,
    i: usize,
}

impl ApplyChain {
    fn step(&mut self, incoming: Option<Value>) -> Result<Yield> {
        if let Some(v) = incoming {
            self.f = Some(v);
        }
        if self.f.is_none() {
            let callee = self
                .callee
                .take()
                .ok_or_else(|| VmError::eval("internal: apply chain lost its function"))?;
            // Most callees are already values -- `map` and `genList` are
            // handed one the driver forced, and the C API forces before it
            // builds the cell -- so spend a round trip through the scheduler
            // only on the ones that are still thunks.
            match callee.peek() {
                Some(v) => self.f = Some(v),
                None => return Ok(Yield::Force(callee)),
            }
        }
        let f = self
            .f
            .take()
            .ok_or_else(|| VmError::eval("internal: apply chain lost its function"))?;
        let Some(arg) = self.args.get(self.i).cloned() else {
            return Ok(Yield::Done(f));
        };
        self.i += 1;
        Ok(Yield::Apply(f, arg))
    }
}

// -- structural equality ----------------------------------------------------

/// One deferred comparison. `Fail` is a decided inequality queued behind the
/// comparisons cppnix would have performed first, so an attrset whose names
/// diverge at position k still forces (and can still throw on) the k values
/// before it, exactly as `eqValues` does.
enum Job {
    Pair(Slot, Slot),
    Fail,
}

pub struct DeepEq {
    work: Vec<Job>,
    cur: Option<(Slot, Slot)>,
    /// Where the current pair is. 0: the left side is being forced; 1: the
    /// right side is; 2 and 3: the two `type` attributes are, for the
    /// derivation short circuit in [`DeepEq::step`].
    stage: u8,
    negate: bool,
    /// The two `type` cells stages 2 and 3 are forcing.
    drv_type: Option<(Slot, Slot)>,
}

impl DeepEq {
    fn new(l: Slot, r: Slot, negate: bool) -> Self {
        DeepEq {
            work: vec![Job::Pair(l, r)],
            cur: None,
            stage: 0,
            negate,
            drv_type: None,
        }
    }

    fn step(&mut self, vm: &mut Vm) -> Result<Yield> {
        if let Some((l, r)) = self.cur.clone() {
            match self.stage {
                0 => {
                    self.stage = 1;
                    return Ok(Yield::Force(r));
                }
                1 => {
                    // eqValues' opening `&v1 == &v2`: the same cell is equal
                    // to itself whatever it holds, which is how `[f] == [f]`
                    // is true for a shared element while `f == f` is false.
                    // Checked after forcing, as cppnix does, so a throwing
                    // cell still throws.
                    if l.id() == r.id() {
                        self.cur = None;
                        self.stage = 0;
                    } else {
                        let (lv, rv) = (forced(&l)?, forced(&r)?);
                        // Two sets that both carry `type` might both be
                        // derivations, which cppnix compares by `outPath`
                        // alone and not structurally. Deciding that needs the
                        // attribute forced, so it costs two stages.
                        if let (Value::Attrs(a), Value::Attrs(b)) = (&lv, &rv) {
                            let type_sym = vm.intern("type");
                            if let (Some(ta), Some(tb)) = (a.get(&type_sym), b.get(&type_sym)) {
                                self.drv_type = Some((ta.clone(), tb.clone()));
                                self.stage = 2;
                                return Ok(Yield::Force(ta.clone()));
                            }
                        }
                        self.cur = None;
                        self.stage = 0;
                        if !self.shallow(&lv, &rv) {
                            return Ok(Yield::Done(Value::Bool(self.negate)));
                        }
                    }
                }
                2 => {
                    let (_, tb) = self
                        .drv_type
                        .clone()
                        .ok_or_else(|| VmError::eval("internal: eq lost a type cell"))?;
                    self.stage = 3;
                    return Ok(Yield::Force(tb));
                }
                _ => {
                    let (ta, tb) = self
                        .drv_type
                        .take()
                        .ok_or_else(|| VmError::eval("internal: eq lost a type cell"))?;
                    self.cur = None;
                    self.stage = 0;
                    let (lv, rv) = (forced(&l)?, forced(&r)?);
                    if let Some((oa, ob)) = self.derivation_out_paths(vm, &lv, &rv, &ta, &tb)? {
                        // Only the outputs are compared, and nothing else is
                        // walked. That is not an optimisation standing in for
                        // the structural answer, it *is* cppnix's answer:
                        // `eval-okay-eq-derivations` expects
                        // `drvA1 == (drvA1 // { dummy = 1; })` to be true, and
                        // structurally those two sets differ in size.
                        //
                        // It is also what makes the comparison terminate. A
                        // derivation contains itself through `all`, so the
                        // structural walk below would not stop.
                        self.work.push(Job::Pair(oa, ob));
                    } else if !self.shallow(&lv, &rv) {
                        return Ok(Yield::Done(Value::Bool(self.negate)));
                    }
                }
            }
        }
        match self.work.pop() {
            None => Ok(Yield::Done(Value::Bool(!self.negate))),
            Some(Job::Fail) => Ok(Yield::Done(Value::Bool(self.negate))),
            Some(Job::Pair(l, r)) => {
                self.cur = Some((l.clone(), r));
                self.stage = 0;
                Ok(Yield::Force(l))
            }
        }
    }

    /// The pair of `outPath` cells to compare instead of the two sets, when
    /// cppnix's `isDerivation` holds for both and both carry one.
    ///
    /// `eqValues` reads it as `isDerivation(v1) && isDerivation(v2)`, so it
    /// forces the left `type` even when the right set has none. This probe
    /// runs only when both sets carry `type`, so the two differ in one
    /// unobservable-in-practice case: a left-hand set whose `type` attribute
    /// throws, compared against a set with no `type` at all, throws under
    /// cppnix and compares structurally here.
    fn derivation_out_paths(
        &self,
        vm: &mut Vm,
        lv: &Value,
        rv: &Value,
        ta: &Slot,
        tb: &Slot,
    ) -> Result<Option<(Slot, Slot)>> {
        let is_drv = |v: &Value| matches!(v, Value::Str(s) if s.bytes() == b"derivation");
        if !is_drv(&forced(ta)?) || !is_drv(&forced(tb)?) {
            return Ok(None);
        }
        let (Value::Attrs(a), Value::Attrs(b)) = (lv, rv) else {
            return Ok(None);
        };
        let out = vm.intern("outPath");
        // cppnix falls through to the structural comparison when either set
        // is derivation-typed but has no `outPath`, rather than calling them
        // unequal.
        match (a.get(&out), b.get(&out)) {
            (Some(oa), Some(ob)) => Ok(Some((oa.clone(), ob.clone()))),
            _ => Ok(None),
        }
    }

    /// Decide the pair as far as forced values allow, queueing children.
    /// `false` means definitely unequal.
    fn shallow(&mut self, l: &Value, r: &Value) -> bool {
        match (l, r) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => {
                (*a as f64) == *b
            }
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Null, Value::Null) => true,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Path(a), Value::Path(b)) => a == b,
            (Value::List(a), Value::List(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                for (x, y) in a.iter().zip(b.iter()).rev() {
                    self.work.push(Job::Pair(x.clone(), y.clone()));
                }
                true
            }
            (Value::Attrs(a), Value::Attrs(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                // Names decide, but only after the values before the first
                // differing name have been compared.
                let stop = a
                    .keys()
                    .zip(b.keys())
                    .position(|(ka, kb)| ka != kb)
                    .unwrap_or(a.len());
                if stop < a.len() {
                    self.work.push(Job::Fail);
                }
                for (x, y) in a.values().zip(b.values()).take(stop).rev() {
                    self.work.push(Job::Pair(x.clone(), y.clone()));
                }
                true
            }
            // cppnix's eqValues: "functions are incomparable", with no
            // pointer fallback, so `let f = x: x; in f == f` is false. Two
            // sides that are literally the same cell never reach here --
            // `step` settles those by slot identity, mirroring the
            // `&v1 == &v2` short circuit eqValues opens with.
            (Value::Closure(_) | Value::Builtin(_), Value::Closure(_) | Value::Builtin(_)) => false,
            _ => false,
        }
    }
}

// -- ordering ---------------------------------------------------------------

struct ListPos {
    a: Rc<Vec<Slot>>,
    b: Rc<Vec<Slot>>,
    i: usize,
}

/// `a < b`, optionally negated. Lists compare lexicographically with a length
/// tiebreak, which the explicit position stack turns into a depth-first walk
/// rather than recursion.
pub struct Compare {
    stack: Vec<ListPos>,
    cur: Option<(Slot, Slot)>,
    stage: u8,
    negate: bool,
}

impl Compare {
    fn new(l: Value, r: Value, negate: bool) -> Self {
        Compare {
            stack: Vec::new(),
            // Both sides arrive forced from the operator that built us.
            cur: Some((Slot::value(l), Slot::value(r))),
            stage: 1,
            negate,
        }
    }

    fn step(&mut self) -> Result<Yield> {
        loop {
            if let Some((l, r)) = self.cur.clone() {
                if self.stage == 0 {
                    self.stage = 1;
                    return Ok(Yield::Force(r));
                }
                self.cur = None;
                self.stage = 0;
                let (lv, rv) = (forced(&l)?, forced(&r)?);
                match (&lv, &rv) {
                    (Value::List(a), Value::List(b)) => self.stack.push(ListPos {
                        a: a.clone(),
                        b: b.clone(),
                        i: 0,
                    }),
                    _ => {
                        let ord = scalar_cmp(&lv, &rv)?;
                        if ord != Ordering::Equal {
                            return Ok(Yield::Done(Value::Bool(
                                (ord == Ordering::Less) != self.negate,
                            )));
                        }
                    }
                }
            }
            let Some(last) = self.stack.len().checked_sub(1) else {
                // Everything compared equal, so `a < b` is false.
                return Ok(Yield::Done(Value::Bool(self.negate)));
            };
            let (alen, blen, i, x, y) = {
                let pos = self
                    .stack
                    .get(last)
                    .ok_or_else(|| VmError::eval("internal: compare position lost"))?;
                (
                    pos.a.len(),
                    pos.b.len(),
                    pos.i,
                    pos.a.get(pos.i).cloned(),
                    pos.b.get(pos.i).cloned(),
                )
            };
            if i >= alen || i >= blen {
                self.stack.pop();
                if alen != blen {
                    return Ok(Yield::Done(Value::Bool((alen < blen) != self.negate)));
                }
                continue;
            }
            if let Some(pos) = self.stack.get_mut(last) {
                pos.i += 1;
            }
            let (Some(x), Some(y)) = (x, y) else {
                return Err(VmError::eval("internal: compare element lost"));
            };
            self.cur = Some((x.clone(), y));
            self.stage = 0;
            return Ok(Yield::Force(x));
        }
    }
}

fn scalar_cmp(l: &Value, r: &Value) -> Result<Ordering> {
    let ord = match (l, r) {
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
        // cppnix compares the `std::string`s, which is a byte compare.
        (Value::Str(a), Value::Str(b)) => a.bytes().cmp(b.bytes()),
        // cppnix's `builtins.lessThan` compares only `pathStrView()` here;
        // accessor identity participates in equality, but not ordering.
        (Value::Path(a), Value::Path(b)) => a.path.as_ref().cmp(b.path.as_ref()),
        _ => {
            return Err(VmError::eval(format!(
                "cannot compare {} with {}",
                type_name(l),
                type_name(r)
            )));
        }
    };
    Ok(ord)
}

// -- with-scope resolution --------------------------------------------------

enum Stage {
    Walk,
    AwaitSubject,
    AwaitValue,
}

pub struct ResolveWith {
    node: Env,
    sym: Sym,
    stage: Stage,
}

impl ResolveWith {
    fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        let mut incoming = incoming;
        loop {
            match self.stage {
                Stage::AwaitValue => {
                    let v = incoming
                        .take()
                        .ok_or_else(|| VmError::eval("internal: with-resolve lost its value"))?;
                    return Ok(Yield::Done(v));
                }
                Stage::AwaitSubject => {
                    let v = incoming
                        .take()
                        .ok_or_else(|| VmError::eval("internal: with-resolve lost its subject"))?;
                    if let Value::Attrs(m) = v
                        && let Some(s) = m.get(&self.sym)
                    {
                        let s = s.clone();
                        self.stage = Stage::AwaitValue;
                        return Ok(Yield::Force(s));
                    }
                    self.stage = Stage::Walk;
                }
                Stage::Walk => {
                    let next = match &*self.node {
                        EnvNode::With { up, subject } => {
                            let (up, subject) = (up.clone(), subject.clone());
                            self.node = up;
                            self.stage = Stage::AwaitSubject;
                            return Ok(Yield::Force(subject));
                        }
                        EnvNode::Frame { up, .. } => up.clone(),
                        EnvNode::Root => {
                            return Err(VmError::eval(format!(
                                "undefined variable '{}'",
                                vm.sym_name(self.sym)
                            )));
                        }
                    };
                    self.node = next;
                }
            }
        }
    }
}
