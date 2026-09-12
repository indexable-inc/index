//! Recording what an evaluation read, and memoising its result against that.
//!
//! # Why a read set can be trusted here
//!
//! `maintainers/ix/read-set-recall.md` is a record of read-set invalidation
//! going wrong in the C++ evaluator, so a read set arriving in this repo owes
//! an argument for why it is different. The argument is that this evaluator
//! has exactly one way to reach the world. Every question it can ask is a
//! [`Host`] call (`builtins.getEnv` included, since it stopped calling
//! `std::env::var` behind the trait's back), and `builtins.purity_tests`
//! fails the build if an impure builtin is implemented that does not go
//! through it. So a recording `Host` sees everything, by construction rather
//! than by having looked hard.
//!
//! # Why replaying a read set is sound
//!
//! The evaluator is deterministic: its result is a function of the module and
//! the answers it received, and so is the *next question it asks*. That second
//! half is what makes verified replay work.
//!
//! A memoised result is stored under `H(module, questions and answers, in
//! order)`. To look one up, the recorded question list from last time is
//! replayed against the host now, and the key is computed from the answers
//! *observed now*, never from the recorded ones. If a result exists under that
//! key, some past evaluation asked exactly this sequence and got exactly these
//! answers; determinism then says the evaluation being asked for would ask the
//! same questions, receive the same answers, and produce the same result.
//!
//! The consequence worth stating: **a stale witness cannot produce a wrong
//! answer, only a miss.** If the recorded question list no longer matches what
//! the evaluation would ask, the key computed from replaying it is a key no
//! evaluation ever stored a result under, so the lookup misses and the
//! evaluation runs. The witness is a hint about which questions to ask, and
//! correctness does not rest on it being right.

use crate::host::{FileType, Host, LookupError, StoreError};
use crate::task::SearchPathEntry;
use ix_kernel::ObjId;
use ix_kernel::hash::{self, Hash};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Domain separation for read-set digests.
const READ_TAG: &str = "ixe-read-v1";
/// Domain separation for the composed evaluation key. Version 2 is the
/// compacted first-occurrence format; version 1 keyed every repeated row.
/// Version 3 keys the rows after [`ReadSet::fold_trees`]: one
/// [`Question::Tree`] row stands for every read under an immutable tree.
const EVAL_TAG: &str = "ixe-eval-result-v3";
/// Domain separation for diagnostic question-argument cardinalities.
#[cfg(feature = "perf")]
const QUESTION_ARGUMENT_TAG: &str = "ixe-question-argument-v1";
/// Rooted path questions use version 2, with the merged `WriteDrv` question
/// on the previously unassigned tag 23. Version 3 drops `WriteDrv`'s
/// references field: the ATerm names its own inputs, and the embedder
/// derives the reference set from the parsed derivation. Version 4 stores
/// each row as the question beside the digest of the answer the recording
/// run observed, which is what lets a read under a mounted root replay
/// without asking (see [`Question::replays_from_record`]). Version 5 lets a
/// row keep the answer text instead of its digest when a store effect named
/// a store object (see [`Recorded`]). Version 6 drops the module field (the
/// sweep no longer reads a witness at all: it reads the `.refs` sidecar
/// [`DirWitness::put`] writes beside it). Version 7 records one
/// [`Question::Tree`] row for the reads under an immutable tree in place of
/// the rows themselves ([`ReadSet::fold_trees`]); a version-6 witness of
/// the same evaluation replays to a different key, so refusing it at the
/// marker saves parsing hundreds of megabytes for a guaranteed miss. An
/// older witness is unreadable to this build and so a miss, then swept.
/// Version 8 requires fingerprinted ownership metadata and bounded history.
const WITNESS_FORMAT: &str = "ixe-witness-v8";
const WITNESS_HISTORY: usize = 4;
const WITNESS_META: &str = "ixe-witness-meta-v1";

/// Suffix of the sidecar beside each witness listing the object names its
/// rows refer to, one per line. Read by [`crate::store::Store::sweep`]; a
/// witness without one is dead to the sweep.
pub(crate) const REFS_SUFFIX: &str = ".refs";

/// One witness row: a question and what the recording run remembered of its
/// answer. The question is what gets asked again; the record is what a row
/// that need not ask replays from.
pub type WitnessRow = (Question, Recorded);

pub(crate) mod retained;

/// What a witness row remembers of an answer.
///
/// A digest is enough to ask again and compare, and enough for a read under
/// a mounted root to take the record instead
/// ([`Question::replays_from_record`]). A store effect whose answer named a
/// store object keeps the answer itself, because a digest is one-way and a
/// row that replays from its record needs the name in it: the object a copy
/// or pinned fetch named, to ask whether the store still holds it
/// ([`present_objects`]); the tree a locked final fetch handed out, to allow
/// it as the fetch would have. [`Recorded::answer`] is the only constructor
/// of that form and refuses every question that cannot replay from its text
/// ([`Question::named_object`]), so an `Answer` is always the text of an
/// answer naming one store object and its digest is the one the recording
/// host computes from that text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recorded {
    Digest(Hash),
    Answer(String),
}

impl Recorded {
    /// The record of a store-copy answer: the text when `question` can
    /// replay from it ([`Question::named_object`]), its digest otherwise (a
    /// failure included).
    fn store_copy(question: &Question, answer: &Result<String, StoreError>) -> Self {
        match answer {
            Ok(text) => Self::answer(question, text.clone())
                .unwrap_or_else(|| Self::Digest(digest_store_copy(answer))),
            Err(_) => Self::Digest(digest_store_copy(answer)),
        }
    }

    /// `text` kept as the answer to `question`, or `None` when the question
    /// names no object in it and so nothing could license replaying it.
    #[must_use]
    pub fn answer(question: &Question, text: String) -> Option<Self> {
        question
            .named_object(&text)
            .is_some()
            .then_some(Self::Answer(text))
    }

    /// The digest the row keys with: the recorded one, or the store-copy
    /// digest of the kept answer.
    #[must_use]
    pub fn digest(&self) -> Hash {
        match self {
            Self::Digest(digest) => *digest,
            Self::Answer(text) => digest_store_outcome(b"store", Ok(text.as_str())),
        }
    }
}

/// One question, without its answer. This is what gets remembered so it can
/// be asked again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Question {
    Import(Rc<crate::value2::PathValue>),
    ReadFile(Rc<crate::value2::PathValue>),
    /// The same file as [`Question::ReadFile`], read as raw bytes: what
    /// `builtins.hashFile` records. A question of its own rather than a
    /// second recording under `ReadFile`, because each question kind owns
    /// one answer encoding, and this one digests the bytes where `ReadFile`
    /// digests the string answer. On a non-UTF-8 file those differ, and one
    /// key carrying two digests would invalidate itself.
    ReadFileBytes(Rc<crate::value2::PathValue>),
    ReadDir(Rc<crate::value2::PathValue>),
    PathExists(Rc<crate::value2::PathValue>),
    /// The trailing-slash spelling of `pathExists`: full resolution followed
    /// by a directory test, with errors kept distinct from `false`.
    DirExists(Rc<crate::value2::PathValue>),
    FileType(Rc<crate::value2::PathValue>),
    /// `builtins.readFileType`'s question asked the other way: the type of
    /// the path with every symlink in it resolved.
    ///
    /// A question of its own and not a duplicate of [`Question::FileType`],
    /// because the two have different answers for the same path and an
    /// `import` asks this one. Recording it as `FileType` would replay the
    /// `lstat`, get `"symlink"` where the evaluation saw `"directory"`, and
    /// compute a key no result was ever stored under -- a permanent miss for
    /// every expression importing through a symlink. ENG-12871.
    FileTypeResolved(Rc<crate::value2::PathValue>),
    GetEnv(String),
    /// A path copied into the store by a string coercion. A question rather
    /// than a bystander because the answer is a content hash of the file: an
    /// edit to it changes the store path, so an evaluation that interpolated
    /// it must miss afterwards.
    CopyToStore(Rc<crate::value2::PathValue>),
    /// An existing store object accepted by `builtins.storePath`.
    StorePath(Rc<crate::value2::PathValue>),
    /// A search path lookup: which entries were walked and what was sought.
    /// The entries are part of the question because the same `<x>` resolves
    /// differently under a different `-I`, and the answer is the file that
    /// won, so a new entry shadowing the old one misses.
    FindFile {
        entries: Vec<SearchPathEntry>,
        name: String,
    },
    /// The default search path itself. Asked whenever `__nixPath` is
    /// evaluated, which `<x>` does, so a change to `-I` invalidates every
    /// result that looked anything up even when the file it found is
    /// unchanged.
    NixPath,
    /// Text `builtins.toFile` asked the store to hold.
    ///
    /// The store *path* is a pure function of the name, the bytes and the
    /// references, so it cannot change under a fixed memo key. Recorded anyway,
    /// for the reason `EnsurePath` is: whether the bytes were actually written
    /// depends on the embedder's `readOnlyMode` and on the store still having
    /// them, so a memoised success could otherwise be served to a run whose
    /// store never wrote the file and whose caller then expects it to be there.
    StoreText {
        name: String,
        contents: String,
        references: Vec<String>,
    },
    /// A finished derivation written by `builtins.derivationStrict`.
    ///
    /// This must stay separate from [`Question::StoreText`]. The cppnix
    /// embedder adds a `StoreText` answer to the evaluator's allow list, but
    /// deliberately does not add a derivation path. Replaying one as the
    /// other changes the permissions of a served evaluation.
    ///
    /// The ATerm lives in the result cache's CAS. Keeping only its address in
    /// the witness makes repeated derivations share one copy of their bytes.
    WriteDrv {
        name: String,
        /// What the recording run's write answered. Not part of the
        /// question's identity (`key_parts` leaves it out; the answer digest
        /// beside the question covers it); carried so replay can say which
        /// path it expected when the write answers differently.
        answer: WriteDrvAnswer,
        aterm: ObjId,
    },
    /// A string context an evaluation realised before reading through it:
    /// import from derivation.
    ///
    /// **This is the read-set entry that makes a memoised IFD sound.** The
    /// answer is a rewrite map, which is empty for every input-addressed
    /// derivation, so it is emphatically not the interesting part of the
    /// digest. What is interesting is that the question was *asked at all*
    /// and that asking it succeeded: the outcome depends on whether the store
    /// can still produce those outputs, which is a fact about the world and
    /// not about the expression. Left unrecorded, a result memoised on a
    /// machine that built the derivation would be served, later, to a run
    /// whose store had been garbage-collected -- and the reads keyed beside
    /// it would replay against paths that are gone.
    ///
    /// That is the same argument [`Question::EnsurePath`] makes, and it is
    /// stronger here, because a `Realise` can *cause* a build. Replaying one
    /// re-runs it, so a witness naming this question is a witness whose
    /// replay is not free. It is recorded anyway: a cheap wrong answer is
    /// worse than an expensive right one. Replay asks the batched validity
    /// question first ([`Question::answer_by_validity`]): when the embedder
    /// reports every element present -- for a built output, only when
    /// realising it now would neither build, refuse nor warn -- the answer is
    /// the empty rewrite map `realiseContext` returns with nothing to build,
    /// and the question is not re-run.
    Realise(Vec<crate::value2::ContextElem>),
    /// A store path `builtins.appendContext` asked to be made present.
    ///
    /// A question for the same reason `CopyToStore` is: whether the store can
    /// produce the path decides whether the evaluation succeeds, so it is a
    /// fact about the world at the time it ran. It was forwarded and not
    /// recorded before, so a memoised success could be served to a run whose
    /// store had since lost the path.
    ///
    /// Tag 9 rather than the next small number: 7 and 8 belong to the search
    /// path questions, and a tag is a wire format, so reusing one would make
    /// an old witness decode as a different question.
    EnsurePath(String),
    /// A filtered copy `builtins.path` asked the store to make.
    ///
    /// The whole request is the key, not just the root: the name, the
    /// ingestion method, the expected hash and every accepted path change what
    /// gets archived and so change the store path. The *answer* is the store
    /// path, which is a content hash of the bytes copied -- and those bytes
    /// are the one thing the walk never read, so nothing else in a read set
    /// notices an edit inside an accepted file. This question is what does.
    ///
    /// A witness for a large filtered tree is correspondingly large: one entry
    /// per accepted path. That is the honest size of the question, and
    /// shrinking it to a digest would make a replayed witness unable to re-ask
    /// it.
    StoreFiltered(Box<crate::task::FilteredCopy>),
    /// A URL `builtins.fetchurl` or `builtins.fetchTarball` asked the store
    /// to hold.
    ///
    /// Recorded for two reasons at once, and either alone would be enough.
    /// An *unpinned* fetch is the most impure question in the language: the
    /// bytes behind a URL change with nobody's permission, so a memoised
    /// result keyed on anything less than "what did this URL answer" is a
    /// stale download served as a fresh one. A *pinned* fetch has a store
    /// path that is a pure function of the request -- and whether the store
    /// can still produce it is not, which is exactly the fact
    /// [`Question::EnsurePath`] exists to record.
    Fetch(Box<crate::task::FetchRequest>),
    /// A tree `builtins.fetchTree` or `builtins.fetchGit` asked for.
    ///
    /// The most impure question here after an unpinned [`Question::Fetch`],
    /// and impure in a second way that one is not: a `git` input with a `ref`
    /// and no `rev` resolves to whatever that branch points at *now*, so the
    /// same question answers differently between two runs a commit apart. The
    /// answer -- the whole emitted attribute set, digested -- is the only
    /// thing in the key that notices.
    FetchTree(Box<crate::task::FetchTreeRequest>),
    /// A flake reference `builtins.getFlake` asked the embedder to lock.
    ///
    /// Recorded, and the recording is what makes a memoised `getFlake` sound
    /// rather than merely fast. The flake reference alone does not determine
    /// the answer: `lockFlake` consults the registry, walks the input graph
    /// and reads a `flake.lock` that can change under a fixed reference, so
    /// two runs a commit apart can lock `path:/src/x` to two different trees.
    /// What notices is the digest of the *answer* -- the lock file and the
    /// overrides document -- which replay recomputes by asking again. A run
    /// whose lock moved gets a different digest, a different key, and a fresh
    /// evaluation.
    ///
    /// The alternative, leaving the question unrecorded, is the ENG-12540
    /// shape: the memoised outputs of yesterday's lock served to today's
    /// reference, with nothing in the key that could tell.
    LockFlake(String),
    /// A flake reference `builtins.parseFlakeRef` asked the embedder to
    /// explode.
    ///
    /// Nearly pure -- the grammar is fixed -- but not a function of the
    /// string alone: the answer can turn on the embedder's fetch settings,
    /// and the feature gate behind the hook decides between an answer and an
    /// error. Neither is in the settings fingerprint, so the question records
    /// itself and the digest of the answer is what notices.
    ParseFlakeRef(String),
    /// An attribute set `builtins.flakeRefToString` asked the embedder to
    /// print. Recorded for the reason [`Question::ParseFlakeRef`] is; the
    /// whole bag is the key, tagged like [`Question::FetchTree`]'s and for
    /// the same reason.
    FlakeRefToString(std::collections::BTreeMap<String, crate::task::TreeAttr>),
    /// A whole immutable tree: a mounted root at its mount point, or a
    /// sealed store object at `<store_dir>/<object>`. Never asked by an
    /// evaluation; [`ReadSet::fold_trees`] records one in place of every row
    /// under it that [`Question::replays_from_record`] vouches for, at the
    /// first such row. Those rows' digests were pinned by the tree's name,
    /// so this row carries the same fact in one row. On replay it takes its
    /// record while the tree is immutable (mounted, or sealed: whether or not
    /// the store still holds the object, since a served result reads nothing
    /// from it), like the rows it stands for; a tree that is not (unsealed)
    /// is asked and answers a constant the record never carries, so the key
    /// misses.
    Tree(Rc<crate::value2::PathValue>),
}

impl Question {
    /// Whether a replay may take this question's recorded answer instead of
    /// asking the host again.
    ///
    /// True for a read whose entire answer is a function of a mounted tree:
    /// the bridge resolves a `Root::Mounted` path inside that mount's own
    /// accessor (`rootedPath` in rust-eval-session.cc), where even an
    /// absolute symlink target restarts at the accessor's root
    /// (`SourceAccessor::resolveSymlinks`), so nothing outside the tree can
    /// reach the answer. A mount point is a complete store path whose name
    /// was computed from the tree's content by the fetcher that mounted it,
    /// so the name pins the bytes: asking again could only return the
    /// recorded answer, or an error for a mount that is gone -- and a
    /// served result needs nothing from the mount, only from the store
    /// objects the effect questions still verify. On one home-manager
    /// witness this covers 139k `ReadDir` and 25k `Import` rows, 2.9s of a
    /// 10.7s hit (round 9, goals/rust-eval.md).
    ///
    /// Also true for the same reads under an ambient path inside a SEALED
    /// store object (`sealed`, base names; [`sealed_objects`]): one the
    /// store walked once, content-addressed so its name pins its bytes, with no
    /// symlink resolving outside it, so a read under it can reach nothing
    /// the name does not pin. That is the mounted-root argument made about a
    /// plain `/nix/store/<hash>-<name>/...` path, which is what every flake
    /// input is on a host without lazy trees: the round-10 rule alone served
    /// no row of the home-manager witness (`copyMounted` 0, no
    /// `replay.<Kind>_recorded` line), and the reads under nixpkgs were the
    /// 4.7s it was written for.
    ///
    /// False for everything else. An ambient path outside a sealed object
    /// reads the real filesystem, which changes under a fixed name; every
    /// store effect (copy, filtered copy, write, realise, fetch) has an
    /// answer the store can lose, and replays by validity
    /// ([`Question::answer_by_validity`]) or by asking.
    #[must_use]
    pub fn replays_from_record(
        &self,
        sealed: &crate::host::Sealed,
        store_dir: Option<&str>,
    ) -> bool {
        let Some(path) = self.read_path() else {
            return false;
        };
        if !immutable_root(path, sealed, store_dir) {
            return false;
        }
        // An import of a directory reads `<dir>/default.nix`
        // (`resolveExprPath`), one level below the path it was asked about,
        // where a leaving link can sit unseen by the check above.
        match self {
            Self::Import(path) => immutable_root(
                &crate::value2::PathValue::new(
                    path.root.clone(),
                    format!("{}/default.nix", path.path),
                ),
                sealed,
                store_dir,
            ),
            _ => true,
        }
    }

    /// The path a read question reads, whose tree decides whether the row
    /// replays from its record: the eight reads, and a [`Question::Tree`]
    /// row standing for reads under its root. `None` for effects and for
    /// questions without a path.
    fn read_path(&self) -> Option<&crate::value2::PathValue> {
        match self {
            Self::Import(path)
            | Self::ReadFile(path)
            | Self::ReadFileBytes(path)
            | Self::ReadDir(path)
            | Self::PathExists(path)
            | Self::DirExists(path)
            | Self::FileType(path)
            | Self::FileTypeResolved(path)
            | Self::Tree(path) => Some(&**path),
            _ => None,
        }
    }

    /// The immutable tree this row reads inside when it replays from its
    /// record, as [`ReadSet::fold_trees`] records it: a mounted root at its
    /// mount point, a sealed store object at `<store_dir>/<object>`. `None`
    /// for a row that asks on replay.
    fn tree(
        &self,
        sealed: &crate::host::Sealed,
        store_dir: Option<&str>,
    ) -> Option<crate::value2::PathValue> {
        if !self.replays_from_record(sealed, store_dir) {
            return None;
        }
        let path = self.read_path()?;
        match &path.root {
            crate::value2::Root::Mounted(mount_point) => Some(crate::value2::PathValue::new(
                path.root.clone(),
                mount_point.clone(),
            )),
            crate::value2::Root::Ambient => {
                let object = store_object_of(store_dir, &path.path)?;
                Some(crate::value2::PathValue::ambient(format!(
                    "{}/{object}",
                    store_dir?
                )))
            }
        }
    }

    /// The ambient path this question reads or copies from, whose store
    /// object (if it has one) the verifier asks about sealing. `None` for a
    /// mounted path, which needs no such fact, and for every other question.
    fn ambient_path(&self) -> Option<&crate::value2::PathValue> {
        let path = match self {
            Self::Import(path)
            | Self::ReadFile(path)
            | Self::ReadFileBytes(path)
            | Self::ReadDir(path)
            | Self::PathExists(path)
            | Self::DirExists(path)
            | Self::FileType(path)
            | Self::FileTypeResolved(path)
            | Self::Tree(path)
            | Self::CopyToStore(path) => &**path,
            Self::StoreFiltered(request) => &*request.root,
            _ => return None,
        };
        matches!(path.root, crate::value2::Root::Ambient).then_some(path)
    }

    /// The store object this question's answer `text` named, for a question
    /// that can replay from that text without asking; `None` for one that
    /// cannot.
    ///
    /// The rule is one sentence: the question's inputs cannot change under
    /// its spelling, and its answer named a store object. A copy or a
    /// filtered copy copies bytes; whether their source cannot change is
    /// decided at replay ([`immutable_root`]: a mounted root, or a sealed
    /// store object), so the answer is always kept, and replays while the
    /// store holds the object. A fetch pinned by `sha256` names the
    /// fixed-output path cppnix's own `fetch` answers with as soon as it is
    /// valid. A final tree with a `narHash` is a function of its locked
    /// attributes ([`crate::task::FetchTreeRequest::locked_final`]) and
    /// replays whether or not the store holds the tree: a served result
    /// reads nothing from it, and the object is named for the allow list
    /// alone. An unpinned fetch asks the network, and a tree fetched through
    /// `fetchTree` or `fetchGit` may resolve a `ref` to whatever it points
    /// at now: those ask.
    #[must_use]
    pub fn named_object<'a>(&self, text: &'a str) -> Option<std::borrow::Cow<'a, str>> {
        use std::borrow::Cow;
        match self {
            Self::CopyToStore(_) | Self::StoreFiltered(_) => Some(Cow::Borrowed(text)),
            Self::Fetch(request) if request.expected_sha256.is_some() => Some(Cow::Borrowed(text)),
            Self::FetchTree(request) if request.locked_final() => {
                tree_out_path(text).map(Cow::Owned)
            }
            _ => None,
        }
    }

    /// The store paths this row asks the batched validity question about,
    /// appended to `out` as [`Host::valid_paths`] takes them: the derivation
    /// a write answered, the objects a realisation stands on
    /// ([`Realisable`]), the object a kept copy or pinned fetch named.
    /// Nothing for a row that replays by asking, nor for a locked final tree,
    /// which replays from its record without a presence question.
    fn validity_lines(&self, recorded: &Recorded, realisable: &Realisable, out: &mut Vec<String>) {
        match self {
            Self::WriteDrv {
                answer: WriteDrvAnswer::Written(path),
                ..
            } => out.push(path.clone()),
            Self::Realise(context) => {
                for elem in context {
                    if let Some(stands_on) = realisable.get(elem) {
                        out.extend(stands_on.needs.iter().cloned());
                    }
                }
            }
            Self::FetchTree(_) => {}
            _ => {
                if let Recorded::Answer(text) = recorded
                    && let Some(object) = self.named_object(text)
                {
                    out.push(object.into_owned());
                }
            }
        }
    }

    /// This row's answer once the store has said which of its
    /// [`Question::validity_lines`] it holds ([`Validity::holds`]), or `None`
    /// when the row must ask.
    ///
    /// Computed from what the store says now (`v.holds`), never from the
    /// record. A derivation the store holds is the path the write answered,
    /// since a write of the same ATerm could answer nothing else. A
    /// realisation every element of which stands on held objects
    /// ([`Realisable`]) is what `realiseContext` returns with nothing to
    /// build: the empty rewrite map, or under `ca-derivations` each built
    /// output's downstream placeholder mapped to its path
    /// (`realiseContextBuild`, primops.cc:153), both pure functions of the
    /// derivation; and its outputs are allowed through the host as
    /// `realiseContext` allows what it realised (primops.cc:185), or the
    /// row asks when the host cannot. A kept store-copy answer whose object
    /// is present is that answer, for a copy whose source cannot have
    /// changed ([`immutable_root`]): a copy of an ambient path outside a
    /// sealed object re-reads it. A locked final tree is its record: the
    /// answer is a function of the locked attributes
    /// ([`crate::task::FetchTreeRequest::locked_final`]), so the store is
    /// asked nothing and only allows the tree's path, as the fetch would
    /// have; an opaque realisation of that path after it stands on the mount
    /// the fetch would have left (round 15: four lazily mounted inputs, never
    /// registered with the store, were fetched again on every hit, 1.6s of a
    /// 6.5s warm run).
    fn answer_by_validity(&self, recorded: &Recorded, v: &Validity<'_>) -> Option<Hash> {
        match self {
            Self::WriteDrv {
                answer: WriteDrvAnswer::Written(path),
                ..
            } => v.holds(path).then(|| digest_write_drv(&Ok(path.clone()))),
            Self::Realise(context) => {
                let served = nothing_to_build(
                    context,
                    |elem| v.realisable.get(elem).cloned(),
                    |path: &str| v.mounted.borrow().contains(path),
                    |needs: &std::collections::BTreeSet<&str>| {
                        needs.iter().all(|path| v.holds(path))
                    },
                    v.ca_derivations,
                )?;
                if !served.outputs.is_empty() && v.host.allow_closures(&served.outputs).is_err() {
                    return None;
                }
                Some(digest_realise(&Ok(served.rewrites)))
            }
            Self::CopyToStore(_) | Self::StoreFiltered(_)
                if !self
                    .ambient_path()
                    .is_none_or(|path| immutable_root(path, &v.sealed, v.store_dir)) =>
            {
                None
            }
            Self::FetchTree(request) if request.locked_final() => {
                let Recorded::Answer(text) = recorded else {
                    return None;
                };
                let out_path = tree_out_path(text)?;
                // The fetch ended in `allowPath` and left the tree mounted, and
                // the rows after it saw both: the served row leaves the same
                // allow list behind it (the note on the arm below) and notes
                // the mount (`Validity::mounted`), which only an opaque
                // realisation of the tree's path stands on:
                // `realiseContextCheck` asks the store nothing for a mounted
                // path and otherwise accepts it only when the store holds it
                // (round 15: four such rows on the crane inputs asked, errored
                // and missed once their fetch rows were served). A copy or
                // pinned fetch naming the same path still needs the store to
                // hold it: `allowPath` materialises nothing, and the asked copy
                // would have.
                if v.host.allow_paths(std::slice::from_ref(&out_path)).is_err() {
                    return None;
                }
                v.mounted.borrow_mut().insert(out_path);
                Some(recorded.digest())
            }
            _ => {
                let Recorded::Answer(text) = recorded else {
                    return None;
                };
                let object = self.named_object(text)?;
                if !v.holds(object.as_ref()) {
                    return None;
                }
                // The copy, fetch and tree hooks end in `allowPath` (cppnix's
                // `allowAndSetStorePathString`, `fetchTree`'s `allowPath`), so
                // the rows recorded after them read through the object they
                // named. A row served by validity must leave the same allow
                // list behind it, or the next ambient read under an unsealed
                // object -- one with a symlink leaving it -- is refused on
                // replay and the hit is a miss (round 11: the `result` link
                // in a dirty flake input unsealed its tree, and its
                // `flake.nix` import asked and was denied).
                if v.host.allow_paths(&[object.into_owned()]).is_err() {
                    return None;
                }
                Some(recorded.digest())
            }
        }
    }

    /// The host question this was recorded from.
    ///
    /// A `Question` is a `NeedPath` that has been through a read set. The two
    /// carry the same payload except that `WriteDrv` replaces its ATerm with a
    /// CAS address. This conversion exists so the purity policy can be read
    /// through a recorded question without a second copy of the table: replay
    /// asks the same things a live evaluation asks, so it must obey the same
    /// rules, and two tables would be two chances to disagree about what
    /// `pure-eval` permits.
    ///
    /// `Warn` and `Trace` have no `Question`: they are outputs, replayed from
    /// [`Emission`] instead. An import is one rooted question because the
    /// answer selects the accessor and returns the source bytes atomically.
    #[must_use]
    pub fn as_need_path(&self) -> crate::task::NeedPath {
        use crate::task::NeedPath;
        match self {
            Self::Import(p) => NeedPath::Import(p.clone()),
            Self::ReadFile(p) => NeedPath::Contents(p.clone()),
            // `Contents` and not `HashFile`: the purity table is the only
            // consumer, the two share its filesystem-read arm, and the
            // algorithm is not recorded here -- the digest encoding, not the
            // verdict, is what tells the questions apart. The same reasoning
            // as `FileType` below.
            Self::ReadFileBytes(p) => NeedPath::Contents(p.clone()),
            Self::ReadDir(p) => NeedPath::Entries(p.clone()),
            Self::PathExists(p) => NeedPath::Exists(p.clone()),
            Self::DirExists(p) => NeedPath::DirExists(p.clone()),
            // `Kind` and not `MaybeKind`: one recorded question, because
            // one host method answers both and the read set records the
            // read, not which contract asked for it. They share a purity arm
            // (`purity.rs`), so the choice changes no verdict; `Kind` is the
            // spelling because it is the question this `Question` is named
            // after. The same reasoning as `FileTypeResolved` below.
            Self::FileType(p) => NeedPath::Kind(p.clone()),
            // `Import` and not `Kind`, because this is the question an import
            // asks and the purity table has to see it as one. The two share
            // an arm there (`purity.rs:214`), so today the choice changes no
            // verdict; naming the real question is what keeps that true if
            // they ever stop sharing.
            Self::FileTypeResolved(p) => NeedPath::Import(p.clone()),
            Self::GetEnv(n) => NeedPath::Env(n.clone()),
            Self::CopyToStore(p) => NeedPath::StorePath(p.clone()),
            Self::StorePath(p) => NeedPath::UseStorePath(p.clone()),
            Self::FindFile { entries, name } => NeedPath::FindFile {
                entries: entries.clone(),
                name: name.clone(),
            },
            Self::NixPath => NeedPath::NixPath,
            Self::EnsurePath(p) => NeedPath::EnsurePath(p.clone()),
            Self::Realise(context) => NeedPath::Realise(context.clone()),
            Self::StoreText {
                name,
                contents,
                references,
            } => NeedPath::StoreText {
                name: name.clone(),
                contents: contents.clone(),
                references: references.clone(),
            },
            // Purity examines only the question kind for `WriteDrv`. The
            // replay path loads the real ATerm from the CAS before asking the
            // host; manufacturing a second copy here would defeat the witness
            // compaction this representation provides.
            Self::WriteDrv { name, answer, .. } => NeedPath::WriteDrv {
                name: name.clone(),
                aterm: String::new(),
                expected: answer.render(),
            },
            Self::StoreFiltered(r) => NeedPath::StoreFiltered(r.clone()),
            Self::Fetch(r) => NeedPath::Fetch(r.clone()),
            Self::FetchTree(r) => NeedPath::FetchTree(r.clone()),
            Self::LockFlake(r) => NeedPath::Flake(r.clone()),
            Self::ParseFlakeRef(r) => NeedPath::ParseFlakeRef(r.clone()),
            Self::FlakeRefToString(a) => NeedPath::FlakeRefToString(a.clone()),
            // `Exists` of the root and not a need of its own: the purity
            // table judges a tree by its root, and every read folded into
            // this row was judged under the same settings when it ran.
            Self::Tree(p) => NeedPath::Exists(p.clone()),
        }
    }

    /// A stable tag per variant, written out rather than derived from
    /// position, so reordering the enum cannot silently change a key.
    const fn tag(&self) -> u8 {
        match self {
            Self::Import(_) => 20,
            Self::ReadFile(_) => 1,
            Self::ReadDir(_) => 2,
            Self::PathExists(_) => 3,
            Self::DirExists(_) => 22,
            Self::FileType(_) => 4,
            Self::GetEnv(_) => 5,
            Self::CopyToStore(_) => 6,
            Self::StorePath(_) => 21,
            Self::FindFile { .. } => 7,
            Self::NixPath => 8,
            Self::EnsurePath(_) => 9,
            Self::StoreText { .. } => 10,
            Self::StoreFiltered(_) => 11,
            Self::Fetch(_) => 12,
            Self::FetchTree(_) => 13,
            // The next free number, not one wedged in beside `FileType`: a
            // tag is stable, and renumbering the ones after it would make
            // every witness on disk decode to the wrong question.
            Self::FileTypeResolved(_) => 14,
            Self::LockFlake(_) => 15,
            Self::Realise(_) => 16,
            Self::ReadFileBytes(_) => 17,
            Self::ParseFlakeRef(_) => 18,
            Self::FlakeRefToString(_) => 19,
            Self::WriteDrv { .. } => 23,
            Self::Tree(_) => 24,
        }
    }

    /// Everything that identifies this question and nothing that answers it:
    /// the tag, [`Question::arg`] and [`Question::key_parts`], in that order.
    /// [`ReadSet::key_of`] folds it per row; [`CopyMemo`] keys one question
    /// on it. One definition, so the two cannot disagree about what a
    /// question is.
    fn key_material(&self) -> Vec<Vec<u8>> {
        let mut parts = vec![vec![self.tag()], self.arg().into_bytes()];
        parts.extend(self.key_parts());
        parts
    }

    /// The single string this question is about, for the digest. The search
    /// path questions carry more than one string, so they contribute their
    /// own parts in [`Question::key_parts`] and answer the empty string here.
    fn arg(&self) -> String {
        match self {
            Self::Import(a)
            | Self::ReadFile(a)
            | Self::ReadFileBytes(a)
            | Self::ReadDir(a)
            | Self::PathExists(a)
            | Self::DirExists(a)
            | Self::FileType(a)
            | Self::FileTypeResolved(a)
            | Self::CopyToStore(a)
            | Self::StorePath(a)
            | Self::Tree(a) => a.accessor_path().to_owned(),
            Self::GetEnv(a) => a.clone(),
            Self::FindFile { name, .. } => name.clone(),
            Self::NixPath => String::new(),
            Self::EnsurePath(path) => path.clone(),
            Self::StoreText { name, .. } => name.clone(),
            Self::WriteDrv { name, .. } => name.clone(),
            Self::StoreFiltered(r) => r.root.accessor_path().to_owned(),
            Self::Fetch(r) => r.url.clone(),
            Self::LockFlake(r) => r.clone(),
            Self::ParseFlakeRef(r) => r.clone(),
            // No single string is the subject: the whole bag is. `key_parts`
            // carries it, and the empty arg is what `NixPath` does for the
            // same reason.
            Self::FetchTree(_) | Self::Realise(_) | Self::FlakeRefToString(_) => String::new(),
        }
    }

    /// Everything beyond the tag and [`Question::arg`] that identifies this
    /// question. Empty for every question whose argument is one string.
    ///
    /// A search path lookup is keyed on the entries as well as the name,
    /// because they are an argument the program supplies: `findFile` takes
    /// the list, and two lookups of the same name against different lists are
    /// two different questions with two different answers.
    fn key_parts(&self) -> Vec<Vec<u8>> {
        match self {
            Self::Import(path)
            | Self::ReadFile(path)
            | Self::ReadFileBytes(path)
            | Self::ReadDir(path)
            | Self::PathExists(path)
            | Self::DirExists(path)
            | Self::FileType(path)
            | Self::FileTypeResolved(path)
            | Self::CopyToStore(path)
            | Self::StorePath(path)
            | Self::Tree(path) => vec![path.root.wire_name().as_bytes().to_vec()],
            Self::FindFile { entries, .. } => entries
                .iter()
                .flat_map(|e| [e.prefix.clone().into_bytes(), e.path.clone().into_bytes()])
                .collect(),
            // The bytes and the references are arguments too: the same name
            // with different contents is a different file at a different
            // path, so keying on the name alone would replay one as the other.
            Self::StoreText {
                contents,
                references,
                ..
            } => {
                let mut parts = vec![contents.clone().into_bytes()];
                parts.extend(references.iter().map(|r| r.clone().into_bytes()));
                parts
            }
            // The answer is not here: what identifies the question is what was
            // written, and the answer digest recorded beside it is what
            // says what came back.
            Self::WriteDrv { aterm, .. } => vec![aterm.hash().as_bytes().to_vec()],
            // Everything but the accessor-relative path, which `arg` already
            // contributes. The root is the first part below. The
            // markers keep "no filtering" apart from "a filter that accepted
            // nothing", and an absent `sha256` apart from one whose value is
            // the empty string: both pairs are different requests with
            // different answers.
            Self::StoreFiltered(r) => {
                let mut parts = vec![
                    r.root.root.wire_name().as_bytes().to_vec(),
                    r.name.clone().into_bytes(),
                    r.method.as_str().as_bytes().to_vec(),
                ];
                match &r.expected_sha256 {
                    Some(h) => {
                        parts.push(b"sha256".to_vec());
                        parts.push(h.clone().into_bytes());
                    }
                    None => parts.push(b"no-sha256".to_vec()),
                }
                match &r.accepted {
                    None => parts.push(b"unfiltered".to_vec()),
                    Some(list) => {
                        parts.push(b"filtered".to_vec());
                        for e in list {
                            parts.push(e.path.clone().into_bytes());
                            parts.push(e.file_type.as_str().as_bytes().to_vec());
                        }
                    }
                }
                // Pushed only when set, which is not a shortcut: a witness
                // recorded before this field existed described a copy that
                // did not inherit references, so the false case has to key
                // exactly as it did then or every one of them misses at once.
                // A `false` marker here would be correct and would also
                // invalidate the whole recorded corpus for nothing.
                if r.inherit_references {
                    parts.push(b"inherit-references".to_vec());
                }
                parts
            }
            // Everything but the URL, which `arg` already contributes. The
            // name is part of the store path, the kind decides both the
            // ingestion method and what is downloaded, and the marker keeps
            // an absent `sha256` apart from one whose value happens to be
            // the all-zero hash -- the first is an unpinned fetch and the
            // second is a pinned one that will fail its check.
            Self::Fetch(r) => {
                let mut parts = vec![
                    r.name.clone().into_bytes(),
                    r.kind.as_str().as_bytes().to_vec(),
                ];
                match &r.expected_sha256 {
                    Some(h) => {
                        parts.push(b"sha256".to_vec());
                        parts.push(h.clone().into_bytes());
                    }
                    None => parts.push(b"no-sha256".to_vec()),
                }
                parts
            }
            // Every element, in the order the evaluator sent them, rendered
            // the one way cppnix renders them. A context is a set and this
            // list came out of a `BTreeSet`, so the order is a function of
            // the elements rather than of how the string was built -- which
            // is what stops two evaluations that concatenated the same two
            // derivation outputs in opposite orders from keying differently.
            Self::Realise(context) => context.iter().map(|e| e.display().into_bytes()).collect(),
            // Every attribute, name and tagged value, in the map's order. The
            // tag is part of the key because `{ shallow = true; }` and
            // `{ shallow = "1"; }` are different inputs that would otherwise
            // digest alike.
            Self::FetchTree(r) => {
                let mut parts = vec![r.fetcher.as_str().as_bytes().to_vec()];
                for (name, value) in &r.attrs {
                    parts.push(name.clone().into_bytes());
                    parts.push(value.tag().as_bytes().to_vec());
                    parts.push(value.text().into_bytes());
                }
                parts
            }
            // Every attribute, name and tagged value, exactly as
            // `FetchTree`'s and for the reason given there.
            Self::FlakeRefToString(attrs) => {
                let mut parts = Vec::new();
                for (name, value) in attrs {
                    parts.push(name.clone().into_bytes());
                    parts.push(value.tag().as_bytes().to_vec());
                    parts.push(value.text().into_bytes());
                }
                parts
            }
            _ => Vec::new(),
        }
    }

    /// A dense index per variant, for tests that must cover the enum rather
    /// than a sample of it.
    ///
    /// # This is the guard, and the chain it starts is the point
    ///
    /// `CopyToStore` shipped with a tag and an `ask` arm and no decoder arm,
    /// so every witness naming one failed to parse and every evaluation
    /// containing `"${./x}"` missed the cache for ever -- silently, and
    /// without even registering as a wasted replay, because the bail happened
    /// before the replay ran. It was found by reading, and fixed by hand
    /// (ENG-12443); nothing stopped the next one.
    ///
    /// Adding a variant now walks a chain that ends in the decoder:
    ///
    /// 1. this match is exhaustive, so it does not compile until the variant
    ///    is named and given an index;
    /// 2. `every_question_variant_is_listed` requires that index to be below
    ///    [`Question::VARIANT_COUNT`], so the count has to be raised;
    /// 3. raising it makes the same test demand a sample in
    ///    [`Question::one_of_each`];
    /// 4. and the sample is fed through the codec by
    ///    `every_question_variant_round_trips_through_the_witness_codec`,
    ///    which fails until `question_from` learns the tag.
    ///
    /// A macro generating the enum would collapse that to one step, and was
    /// the first attempt here. It cannot express this enum: `FindFile` is a
    /// struct variant and `NixPath` a unit variant, and flattening them into
    /// a uniform shape to suit the guard would be the guard deciding the data
    /// model.
    #[cfg(test)]
    const fn variant_index(&self) -> usize {
        match self {
            Self::Import(_) => 19,
            Self::ReadFile(_) => 0,
            Self::ReadDir(_) => 1,
            Self::PathExists(_) => 2,
            Self::DirExists(_) => 21,
            Self::FileType(_) => 3,
            Self::GetEnv(_) => 4,
            Self::CopyToStore(_) => 5,
            Self::StorePath(_) => 20,
            Self::FindFile { .. } => 6,
            Self::NixPath => 7,
            Self::EnsurePath(_) => 8,
            Self::StoreText { .. } => 9,
            Self::StoreFiltered(_) => 10,
            Self::Fetch(_) => 11,
            Self::FetchTree(_) => 12,
            Self::FileTypeResolved(_) => 13,
            Self::LockFlake(_) => 14,
            Self::Realise(_) => 15,
            Self::ReadFileBytes(_) => 16,
            Self::ParseFlakeRef(_) => 17,
            Self::FlakeRefToString(_) => 18,
            Self::WriteDrv { .. } => 22,
            Self::Tree(_) => 23,
        }
    }

    /// How many variants [`Question::variant_index`] can return.
    #[cfg(test)]
    const VARIANT_COUNT: usize = 24;

    /// One instance of every variant, in index order.
    #[cfg(test)]
    fn one_of_each() -> Vec<Self> {
        let path = |spelling: &str| Rc::new(crate::value2::PathValue::ambient(spelling));
        vec![
            Self::ReadFile(path("/argument/read-file")),
            Self::ReadDir(path("/argument/read-dir")),
            Self::PathExists(path("/argument/path-exists")),
            Self::FileType(path("/argument/file-type")),
            Self::GetEnv("ARGUMENT_GET_ENV".to_owned()),
            Self::CopyToStore(path("/argument/copy-to-store")),
            Self::FindFile {
                entries: vec![
                    SearchPathEntry {
                        prefix: "nixpkgs".to_owned(),
                        path: "/argument/nixpkgs".to_owned(),
                    },
                    SearchPathEntry {
                        prefix: String::new(),
                        path: "/argument/fallback".to_owned(),
                    },
                ],
                name: "nixpkgs".to_owned(),
            },
            Self::NixPath,
            Self::EnsurePath("/argument/ensure-path".to_owned()),
            Self::StoreText {
                name: "argument-store-text".to_owned(),
                contents: "argument contents".to_owned(),
                references: vec![
                    "/argument/store-text-ref-a".to_owned(),
                    "/argument/store-text-ref-b".to_owned(),
                ],
            },
            Self::StoreFiltered(Box::new(crate::task::FilteredCopy {
                root: path("/argument/store-filtered"),
                name: "argument-name".to_owned(),
                method: crate::task::PathMethod::Flat,
                accepted: Some(vec![
                    crate::task::AcceptedPath {
                        path: "/argument/store-filtered/a".to_owned(),
                        file_type: FileType::Regular,
                    },
                    crate::task::AcceptedPath {
                        path: "/argument/store-filtered/d".to_owned(),
                        file_type: FileType::Directory,
                    },
                ]),
                expected_sha256: Some(
                    "sha256-1BdlSaqjNlSVCcgD/PocqAwbnGQ+lyfL6h9WK6+MCJc=".to_owned(),
                ),
                inherit_references: true,
            })),
            Self::Fetch(Box::new(crate::task::FetchRequest {
                url: "https://argument.example/fetch.tar.gz".to_owned(),
                name: "argument-fetch-name".to_owned(),
                kind: crate::task::FetchKind::Tarball,
                expected_sha256: Some(
                    "sha256-1BdlSaqjNlSVCcgD/PocqAwbnGQ+lyfL6h9WK6+MCJc=".to_owned(),
                ),
            })),
            Self::FetchTree(Box::new(crate::task::FetchTreeRequest {
                attrs: [
                    (
                        "type".to_owned(),
                        crate::task::TreeAttr::Str("git".to_owned()),
                    ),
                    (
                        "url".to_owned(),
                        crate::task::TreeAttr::Str("/argument/repo".to_owned()),
                    ),
                    ("shallow".to_owned(), crate::task::TreeAttr::Bool(true)),
                    ("revCount".to_owned(), crate::task::TreeAttr::Int(7)),
                ]
                .into_iter()
                .collect(),
                fetcher: crate::task::TreeFetcher::Git,
            })),
            Self::FileTypeResolved(path("/argument/file-type-resolved")),
            Self::LockFlake("path:/argument/flake".to_owned()),
            // All three element shapes, so the codec is exercised on the two
            // that carry a marker character as well as the bare one.
            Self::Realise(vec![
                crate::value2::ContextElem::Opaque("/argument/realise-opaque".into()),
                crate::value2::ContextElem::DrvDeep("/argument/realise-deep.drv".into()),
                crate::value2::ContextElem::Built {
                    drv: "/argument/realise-built.drv".into(),
                    output: "dev".into(),
                },
            ]),
            Self::ReadFileBytes(path("/argument/read-file-bytes")),
            Self::ParseFlakeRef("github:argument/parse-flake-ref".to_owned()),
            // All three attr shapes, so the codec round-trip exercises every
            // tag the triplet encoding can carry.
            Self::FlakeRefToString(
                [
                    (
                        "type".to_owned(),
                        crate::task::TreeAttr::Str("github".to_owned()),
                    ),
                    ("shallow".to_owned(), crate::task::TreeAttr::Bool(false)),
                    ("revCount".to_owned(), crate::task::TreeAttr::Int(11)),
                ]
                .into_iter()
                .collect(),
            ),
            Self::Import(path("/argument/import")),
            Self::StorePath(path("/argument/store-path")),
            Self::DirExists(path("/argument/dir-exists")),
            Self::WriteDrv {
                name: "argument-write-drv".to_owned(),
                answer: WriteDrvAnswer::Written("/nix/store/argument-write-drv.drv".to_owned()),
                aterm: ObjId::of(b"Derive([])"),
            },
            Self::Tree(path("/argument/tree")),
        ]
    }

    /// Ask this question of a host and digest the answer.
    ///
    /// The digest, not the answer, is what a read set carries: a read set
    /// holding the contents of every file read would be larger than the thing
    /// it is caching.
    ///
    /// Replay reaches this only for a row that neither takes its record
    /// ([`Question::replays_from_record`]) nor answers by validity
    /// ([`Question::answer_by_validity`]); a `WriteDrv` row here is one
    /// whose derivation the store no longer holds, and it is written again.
    pub fn ask<C: Cas + ?Sized>(&self, host: &dyn Host, cas: &C) -> Result<Hash, ReplayFailure> {
        let answer = match self {
            Self::Import(path) => digest_import(&host.import_source(path)),
            Self::LockFlake(flake_ref) => digest_flake_call(&host.lock_flake(flake_ref)),
            Self::ReadFile(path) => match host.read_file(path) {
                Ok(text) => digest(&[b"file-ok", text.as_bytes()]),
                Err(error) => digest(&[b"file-err", error.as_bytes()]),
            },
            Self::ReadFileBytes(path) => digest_file_bytes(&host.read_file_bytes(path)),
            Self::ReadDir(path) => match host.read_dir(path) {
                Ok(entries) => {
                    // read_dir already sorts by name, so the digest does not
                    // depend on the order the filesystem happened to return.
                    let mut parts: Vec<Vec<u8>> = vec![b"dir-ok".to_vec()];
                    for (name, kind) in entries {
                        parts.push(name.into_bytes());
                        parts.push(kind.as_str().as_bytes().to_vec());
                    }
                    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
                    digest(&refs)
                }
                Err(error) => digest(&[b"dir-err", error.as_bytes()]),
            },
            Self::PathExists(path) => digest_exists(&host.path_exists_checked(path)),
            Self::DirExists(path) => digest_exists(&host.dir_exists_checked(path)),
            Self::FileType(path) => digest_file_type(&host.file_type(path)),
            // A different domain prefix from `FileType`'s, so the two cannot
            // digest equal on a path where `lstat` and `stat` happen to
            // agree. They are different questions; a key must be able to say
            // which one was asked.
            Self::FileTypeResolved(path) => match host.file_type_resolved(path) {
                Ok(kind) => digest(&[b"kind-resolved-ok", kind.as_str().as_bytes()]),
                Err(error) => digest(&[b"kind-resolved-err", error.as_bytes()]),
            },
            // Unset and empty are different facts, and a read set that
            // conflated them would hit across a change between the two.
            Self::GetEnv(name) => match host.get_env(name) {
                Some(value) => digest(&[b"env-set", value.as_bytes()]),
                None => digest(&[b"env-unset"]),
            },
            Self::CopyToStore(path) => digest_store_copy(&host.copy_to_store(path)),
            Self::StorePath(path) => digest_store_path(&host.store_path(path)),
            Self::StoreText {
                name,
                contents,
                references,
            } => digest_store_text(&host.store_text(name, contents, references)),
            Self::WriteDrv {
                name,
                answer: expected,
                aterm,
            } => {
                let bytes = match cas.get_verified(*aterm) {
                    Err(error) => {
                        return Err(ReplayFailure::Cas {
                            aterm: *aterm,
                            error,
                        });
                    }
                    Ok(ix_kernel::cas::Verified::Missing) => {
                        return Err(ReplayFailure::MissingAterm(*aterm));
                    }
                    Ok(ix_kernel::cas::Verified::Corrupt) => {
                        return Err(ReplayFailure::CorruptAterm(*aterm));
                    }
                    Ok(ix_kernel::cas::Verified::Found(bytes)) => bytes,
                };
                let text =
                    String::from_utf8(bytes).map_err(|_| ReplayFailure::InvalidAterm(*aterm))?;
                let replayed = host.write_derivation(name, &text);
                if WriteDrvAnswer::of(&replayed) != *expected {
                    return match replayed {
                        Ok(written) => Err(ReplayFailure::DrvPathMismatch {
                            expected: expected.render(),
                            written,
                        }),
                        Err(error) => Err(ReplayFailure::WriteDrv {
                            drv_path: expected.render(),
                            error,
                        }),
                    };
                }
                digest_write_drv(&replayed)
            }
            Self::FindFile { entries, name } => digest_find_file(&host.find_file(entries, name)),
            Self::NixPath => digest_nix_path(&host.nix_path()),
            Self::EnsurePath(path) => digest_store_ensure(&host.ensure_path(path)),
            Self::Realise(context) => digest_realise(&host.realise(context)),
            Self::StoreFiltered(request) => digest_store_copy(&host.store_filtered(request)),
            Self::Fetch(request) => digest_store_copy(&host.fetch(request)),
            Self::FetchTree(request) => digest_store_copy(&host.fetch_tree(request)),
            Self::ParseFlakeRef(flake_ref) => digest_store_copy(&host.parse_flake_ref(flake_ref)),
            Self::FlakeRefToString(attrs) => digest_store_copy(&host.flake_ref_to_string(attrs)),
            // Reached only when the tree is not immutable now (an immutable
            // one replays from its record), where the answer has to differ
            // from the recorded one: a constant the record never carries.
            Self::Tree(_) => digest_tree(false),
        };
        Ok(answer)
    }
}

/// A 128-bit diagnostic digest of the exact argument fields a recording host
/// contributes to an evaluation key: the stable question tag, its primary
/// argument, and every value from [`Question::key_parts`]. The answer is
/// deliberately absent because this identifies repeated asks, not repeated
/// outcomes. Truncation can only undercount a diagnostic cardinality; it never
/// participates in evaluation or cache correctness. Its callers are the perf
/// counters in `vm.rs`, so it exists only with them.
#[cfg(feature = "perf")]
pub(crate) fn question_argument_digest(question: &Question) -> [u8; 16] {
    let tag = [question.tag()];
    let arg = question.arg();
    let key_parts = question.key_parts();
    let mut fields: Vec<&[u8]> = Vec::with_capacity(2 + key_parts.len());
    fields.push(&tag);
    fields.push(arg.as_bytes());
    fields.extend(key_parts.iter().map(Vec::as_slice));

    let digest = hash::tagged(QUESTION_ARGUMENT_TAG, &fields);
    let mut shortened = [0; 16];
    shortened.copy_from_slice(&digest.as_bytes()[..16]);
    shortened
}

#[derive(Debug)]
pub enum ReplayFailure {
    MissingAterm(ObjId),
    CorruptAterm(ObjId),
    InvalidAterm(ObjId),
    Cas { aterm: ObjId, error: KernelError },
    DrvPathMismatch { expected: String, written: String },
    WriteDrv { drv_path: String, error: StoreError },
}

fn digest(parts: &[&[u8]]) -> Hash {
    hash::tagged(READ_TAG, parts)
}

/// The record of a [`Question::Tree`] row. Recording writes `pinned`; a
/// replay that has to ask (the tree is not immutable now) observes
/// `unavailable`, so the two keys never meet.
fn digest_tree(pinned: bool) -> Hash {
    let state: &[u8] = if pinned { b"pinned" } else { b"unavailable" };
    digest(&[b"tree", state])
}

/// One name per way a store question can fail, kept apart: "no store here"
/// is not "the store could not", and neither is "this backend will not carry
/// it" or "answering would build a derivation". A witness recorded under one
/// must not hit under another, so each gets its own name and its own detail.
fn store_failure(error: &StoreError) -> (&'static str, &str) {
    match error {
        StoreError::Failed(detail) => ("err", detail),
        StoreError::ImportFromDerivation(detail) => ("ifd", detail),
        StoreError::Unsupported(detail) => ("unsupported", detail),
        StoreError::NoStore => ("absent", ""),
    }
}

/// Digest one store question's outcome, for the recorder and for replay.
///
/// One function shared by every store question, because the recorder writes
/// this digest into the read set and [`Question::ask`] recomputes it on replay.
/// A difference between separate implementations would be a permanent miss,
/// or worse, a hit that should not have been. The question kind is the first
/// part, so a copy and a text store that answer the same path cannot digest
/// equal.
fn digest_store_outcome(kind: &[u8], answer: Result<&str, &StoreError>) -> Hash {
    let (outcome, detail) = match answer {
        Ok(value) => ("ok", value),
        Err(error) => store_failure(error),
    };
    digest(&[kind, outcome.as_bytes(), detail.as_bytes()])
}

fn digest_store_text(answer: &Result<String, StoreError>) -> Hash {
    digest_store_outcome(b"text", answer.as_deref())
}

fn digest_store_copy(answer: &Result<String, StoreError>) -> Hash {
    digest_store_outcome(b"store", answer.as_deref())
}

fn digest_store_ensure(answer: &Result<(), StoreError>) -> Hash {
    digest_store_outcome(b"ensure", answer.as_ref().map(|()| ""))
}

fn digest_write_drv(answer: &Result<String, StoreError>) -> Hash {
    digest_store_outcome(b"write-drv", answer.as_deref())
}

/// What a derivation write answered: the path it produced, or the class and
/// text of its failure. Typed rather than a string with a marker in it, so
/// the two cases cannot be confused by a reader and a failure cannot be
/// mistaken for a path that happens to start the same way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteDrvAnswer {
    Written(String),
    Failed { outcome: String, detail: String },
}

impl WriteDrvAnswer {
    fn of(answer: &Result<String, StoreError>) -> Self {
        match answer {
            Ok(path) => Self::Written(path.clone()),
            Err(error) => {
                let (outcome, detail) = store_failure(error);
                Self::Failed {
                    outcome: outcome.to_owned(),
                    detail: detail.to_owned(),
                }
            }
        }
    }

    /// The one-string form the witness carries: the path itself, or the
    /// failure behind a leading NUL, which no store path can contain.
    fn render(&self) -> String {
        match self {
            Self::Written(path) => path.clone(),
            Self::Failed { outcome, detail } => format!("\0{outcome}\0{detail}"),
        }
    }

    /// The inverse of [`WriteDrvAnswer::render`]. A failure with the marker
    /// but no second separator is malformed, and refuses.
    fn parse(text: &str) -> Option<Self> {
        match text.strip_prefix('\0') {
            None => Some(Self::Written(text.to_owned())),
            Some(rest) => {
                let (outcome, detail) = rest.split_once('\0')?;
                Some(Self::Failed {
                    outcome: outcome.to_owned(),
                    detail: detail.to_owned(),
                })
            }
        }
    }
}

/// One body for the recorder and for replay, for the reason
/// [`digest_flake_call`] gives: a difference between the two is a permanent
/// miss or a false hit, never a reported mismatch.
fn digest_file_bytes(answer: &Result<Vec<u8>, String>) -> Hash {
    match answer {
        Ok(bytes) => digest(&[b"file-bytes-ok", bytes]),
        Err(error) => digest(&[b"file-bytes-err", error.as_bytes()]),
    }
}

fn digest_exists(answer: &Result<bool, String>) -> Hash {
    match answer {
        Ok(answer) => digest(&[b"exists", if *answer { b"1" } else { b"0" }]),
        Err(error) => digest(&[b"exists-error", error.as_bytes()]),
    }
}

fn digest_import(answer: &Result<crate::host::ImportedSource, String>) -> Hash {
    match answer {
        Ok(crate::host::ImportedSource::Nix { path, text }) => digest(&[
            b"import-ok",
            path.root.wire_name().as_bytes(),
            path.accessor_path().as_bytes(),
            text.as_bytes(),
        ]),
        Ok(crate::host::ImportedSource::Derivation(drv)) => {
            let mut fields: Vec<&[u8]> = vec![b"import-drv", drv.path.as_bytes(), drv.name.as_bytes()];
            for (name, path) in &drv.outputs {
                fields.push(name.as_bytes());
                fields.push(path.as_bytes());
            }
            digest(&fields)
        }
        Err(error) => digest(&[b"import-err", error.as_bytes()]),
    }
}

/// Digest a lock, for the recorder and for replay.
///
/// The `call-flake.nix` source is deliberately outside the digest. It is a
/// compile-time constant of the embedder binary, identical for every call in
/// a process, so including it would add a field that never varies; if it ever
/// did vary the binary changed, which the evaluator settings fingerprint
/// already covers.
fn digest_flake_call(answer: &Result<crate::host::FlakeCall, StoreError>) -> Hash {
    let rendered = answer
        .as_ref()
        .map(|call| format!("{}\0{}", call.lock_file, call.overrides))
        .map_err(Clone::clone);
    digest_store_outcome(b"flake-call", rendered.as_deref())
}

/// Digest both paths returned by `builtins.storePath` through the shared
/// store-outcome table.
fn digest_store_path(answer: &Result<crate::host::StorePathResult, StoreError>) -> Hash {
    let rendered = answer
        .as_ref()
        .map(|answer| format!("{}\0{}", answer.path, answer.store_path))
        .map_err(Clone::clone);
    digest_store_outcome(b"store-path", rendered.as_deref())
}

/// Digest a realisation, for the recorder and for replay.
///
/// The rewrite map is flattened with separators that cannot occur in a store
/// path, so a map of one pair cannot digest equal to a differently-split map
/// of the same bytes.
fn digest_realise(answer: &Result<std::collections::BTreeMap<String, String>, StoreError>) -> Hash {
    let rendered = answer
        .as_ref()
        .map(|rewrites| {
            rewrites
                .iter()
                .map(|(from, to)| format!("{from}\0{to}"))
                .collect::<Vec<_>>()
                .join("\0")
        })
        .map_err(Clone::clone);
    digest_store_outcome(b"realise", rendered.as_deref())
}

/// The distinct question and answer pairs one evaluation observed, in first
/// occurrence order.
///
/// Within one evaluation, asking the same question again must return the same
/// answer for the duplicate to be removed. The recorder digests the answer it
/// actually returned, so a re-ask that answers differently is a different
/// pair and remains in the read set. Replay asks each retained pair once and
/// therefore observes everything the evaluation observed. First-occurrence
/// order remains part of the key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadSet {
    entries: Vec<WitnessRow>,
    /// ATerms awaiting insertion into the result cache's CAS. Persisted
    /// witnesses carry only the map keys.
    aterms: BTreeMap<ObjId, Vec<u8>>,
    aterms_reused: u64,
    rows_deduped: u64,
}

/// A read set's rows after [`ReadSet::fold_trees`]. Only the rows: the
/// ATerms the read set carries are the recording's to publish and are not
/// copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folded {
    pub rows: Vec<WitnessRow>,
    /// Rows the fold removed: the read set had `rows.len() + rows_folded`.
    pub rows_folded: u64,
}

impl ReadSet {
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The rows with every one [`Question::replays_from_record`] vouches
    /// for replaced by one [`Question::Tree`] row per tree, at the position
    /// of the tree's first such row.
    ///
    /// Each folded row's digest is pinned by the tree's name (a mounted
    /// root's content address, a sealed store object's), so the tree row
    /// carries what they did in one row: a witness of one row per file read
    /// becomes one of a row per tree. Replay treats the tree row as it
    /// treated them (it takes the record while the tree is immutable, and
    /// observes a different answer once it is not), and the key is computed
    /// over the folded rows on both sides. Rows under an unsealed object, or
    /// at or below a link leaving one, are not folded and ask on replay as
    /// before. Sealing is permanent, so two recordings of one evaluation
    /// fold alike, and folding a folded set changes nothing.
    ///
    /// `memo` is the persistent record of sealed objects
    /// ([`sealed_objects`]); the objects the rows read under that it does
    /// not name are asked of `host` once, here rather than at the first
    /// replay.
    #[must_use]
    pub fn fold_trees(
        &self,
        host: &dyn Host,
        settings: &crate::eval::Settings,
        memo: Option<&DirSealed>,
    ) -> Folded {
        let store_dir = settings.store_dir.as_deref();
        let sealed = sealed_objects(&self.entries, host, store_dir, memo);
        let mut trees = std::collections::BTreeSet::new();
        let mut rows = Vec::with_capacity(self.entries.len());
        for (question, recorded) in &self.entries {
            match question.tree(&sealed, store_dir) {
                Some(tree) => {
                    if trees.insert(tree.clone()) {
                        rows.push((
                            Question::Tree(Rc::new(tree)),
                            Recorded::Digest(digest_tree(true)),
                        ));
                    }
                }
                None => rows.push((question.clone(), recorded.clone())),
            }
        }
        let rows_folded = (self.entries.len() - rows.len()) as u64;
        Folded { rows, rows_folded }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The questions alone.
    #[must_use]
    pub fn questions(&self) -> Vec<Question> {
        self.entries.iter().map(|(q, _)| q.clone()).collect()
    }

    /// Every question with the digest of its answer, in the order asked:
    /// what a witness remembers.
    #[must_use]
    pub fn entries(&self) -> &[WitnessRow] {
        &self.entries
    }

    /// `H(identity, (tag, arg, answer)*)`, order-sensitive.
    ///
    /// Order is part of the key because the question sequence is itself a
    /// function of the answers: two evaluations that asked the same questions
    /// in different orders are not the same evaluation.
    #[must_use]
    pub fn key(&self, identity: &EvalId) -> Hash {
        Self::key_of(&self.entries, identity)
    }

    /// [`ReadSet::key`] over rows outside a read set: the rows
    /// [`ReadSet::fold_trees`] produced, which are what a record keys on.
    #[must_use]
    pub fn key_of(rows: &[WitnessRow], identity: &EvalId) -> Hash {
        let mut hasher = hash::TaggedHasher::new(EVAL_TAG);
        hasher.field(identity.as_hash().as_bytes());
        for (question, recorded) in rows {
            Self::hash_row(&mut hasher, question, recorded);
        }
        hasher.finish()
    }

    fn hash_row(hasher: &mut hash::TaggedHasher, question: &Question, recorded: &Recorded) {
        for part in question.key_material() {
            hasher.field(&part);
        }
        hasher.field(recorded.digest().as_bytes());
    }

    /// Replay in effect order and hash observed answers without copying the witness.
    fn replay_key_with<C: Cas + ?Sized>(
        rows: &[WitnessRow],
        host: &dyn Host,
        settings: &crate::eval::Settings,
        cas: &C,
        memo: Option<&DirSealed>,
        identity: &EvalId,
    ) -> Result<Option<Hash>, ReplayFailure> {
        let mut hasher = hash::TaggedHasher::new(EVAL_TAG);
        hasher.field(identity.as_hash().as_bytes());
        let replayed =
            Self::replay_rows_with(rows, host, settings, cas, memo, |question, recorded| {
                Self::hash_row(&mut hasher, question, recorded);
            })?;
        Ok(replayed.map(|()| hasher.finish()))
    }

    /// Ask a remembered question list again and build the read set from the
    /// answers given *now*. The recorded answers are deliberately not an input:
    /// computing the key from what was observed rather than what was expected
    /// is what makes a stale witness a miss instead of a wrong answer.
    ///
    /// `None` when the embedder has forbidden the evaluator to reach the world
    /// and the witness names anything at all, because replaying it would do
    /// exactly the reads `restrict-eval` and `pure-eval` exist to prevent.
    ///
    /// # Why the check is here and not only in the memo key
    ///
    /// `Question::ask` calls `host.read_file` and friends directly. The
    /// evaluator's own access check lives in `eval::answer_path`, which replay
    /// never enters, so this path had no check at all (ENG-12543). Before the
    /// settings went into the memo key, that was live and not theoretical: a
    /// cache filled with reads allowed, then looked up under `pure-eval`, read
    /// the file again *and* served the answer, measured at 8065be845 as
    /// `status=ok value="secret" memo_hit=true reads=["/etc/shadow"]`.
    ///
    /// Folding the purity settings into the identity (ENG-12541) closed the
    /// exploit incidentally: the two settings now address different rows, so
    /// the pure-eval lookup finds no witness and never gets here. That is a
    /// real fix and a fragile place to leave the guarantee, because it holds
    /// only as long as those fields stay in the key -- a property of a
    /// different file, provable only by reading both. This check makes it
    /// local: no read happens here whatever the key does.
    ///
    /// The test is [`crate::purity::verdict`], question by question, and only
    /// `Verdict::Ask` replays. That is the same rule `eval::answer_path`
    /// applies to a live evaluation, read through the same table, so replay
    /// cannot permit a question the evaluator would have refused nor refuse
    /// one it would have served.
    ///
    /// Both arguments to that table are read here, not just the settings.
    /// [`crate::purity::PathReads`] says whether a plain filesystem read goes
    /// through the embedder's accessor, and it decides six of the rows
    /// (ENG-12792), so a witness recorded by the `nix` binary -- where it
    /// does -- must not be replayed by a standalone embedding under a purity
    /// setting, where the same reads would come from `std::fs` and honour
    /// nothing. Reading the table rather than the settings is what makes that
    /// automatic: the rows move and this check moves with them.
    ///
    /// The three non-`Ask` verdicts all block, for three different reasons.
    /// `Refuse` is the one this check exists for. `EmptyString` blocks because
    /// the recorded answer for `getEnv` was the environment's value and the
    /// live answer under either setting is `""`, so replaying would hand back
    /// the impure one. `Error` blocks because a live evaluation would have
    /// failed rather than asked, and a witness recorded before the setting was
    /// on has an answer for a question that now must not be asked at all.
    ///
    /// Being *coarser* than the table was the first attempt and it was wrong
    /// in the direction that looks safe: refusing every witness naming any
    /// question under either setting made `builtins.toFile` and a path
    /// interpolation -- both of which pure eval permits -- record a result
    /// that could never be found again, which
    /// `maintainers/ix/cache-semantics-gate.sh` reports as an unreachable memo
    /// key rather than tolerating.
    ///
    /// An empty witness still replays, and should. It names no reads, so there
    /// is nothing to forbid, and a pure expression keeps its cache under
    /// `pure-eval` -- which is the one case where caching is unambiguously
    /// safe.
    ///
    /// # Which rows are asked again
    ///
    /// Every row, except two kinds. One whose question
    /// [`Question::replays_from_record`] vouches for: a read under a mounted
    /// root or a sealed object, whose recorded digest is the only answer a
    /// re-ask could observe (or an error for a tree since lost, which a
    /// served result reads nothing from). Those rows take the digest the
    /// witness carries and touch no host. And one the store answers by
    /// validity ([`Question::answer_by_validity`]): a write, copy, fetch or
    /// realisation whose answer named a store object the store holds now,
    /// answered from what the store says; and a final tree fetch whose locked
    /// attributes are the whole of its answer. Neither is an exception to the
    /// rule above; both are the rule applied where "observed now" and
    /// "recorded" are the same value by construction.
    pub fn replay<C: Cas + ?Sized>(
        rows: &[WitnessRow],
        host: &dyn Host,
        settings: &crate::eval::Settings,
        cas: &C,
    ) -> Result<Option<Self>, ReplayFailure> {
        Self::replay_with(rows, host, settings, cas, None)
    }

    /// [`ReadSet::replay`] with the persistent record of sealed store
    /// objects (`memo`), so an object walked once is never walked again.
    /// `None` still asks the host about every object under which a row
    /// reads, and forgets the answer with the call.
    pub fn replay_with<C: Cas + ?Sized>(
        rows: &[WitnessRow],
        host: &dyn Host,
        settings: &crate::eval::Settings,
        cas: &C,
        memo: Option<&DirSealed>,
    ) -> Result<Option<Self>, ReplayFailure> {
        let mut entries = Vec::with_capacity(rows.len());
        let replayed =
            Self::replay_rows_with(rows, host, settings, cas, memo, |question, recorded| {
                entries.push((question.clone(), recorded.clone()));
            })?;
        Ok(replayed.map(|()| Self {
            entries,
            ..Self::default()
        }))
    }

    fn replay_rows_with<C: Cas + ?Sized>(
        rows: &[WitnessRow],
        host: &dyn Host,
        settings: &crate::eval::Settings,
        cas: &C,
        memo: Option<&DirSealed>,
        mut emit: impl FnMut(&Question, &Recorded),
    ) -> Result<Option<()>, ReplayFailure> {
        // A successful Built-context answer recorded under a buggy or different
        // policy must not bypass the host's IFD refusal, even if all outputs exist.
        // Reject before any replay effects; the fresh run reports the host error.
        if !settings.allow_import_from_derivation && rows.iter().any(|(question, _)| {
            matches!(question, Question::Realise(context) if context_requires_ifd(context))
        }) {
            return Ok(None);
        }
        let purity = settings.purity();
        let reads = settings.path_reads;
        if purity.any()
            && rows.iter().any(|(question, _)| {
                !matches!(
                    crate::purity::verdict(&question.as_need_path(), purity, reads),
                    crate::purity::Verdict::Ask
                )
            })
        {
            return Ok(None);
        }
        let store_dir = settings.store_dir.as_deref();
        let realisable = realisable(rows, cas);
        let sealed = sealed_objects(rows, host, store_dir, memo);
        let (present, store_answers) = match present_objects(rows, &realisable, host) {
            Ok(present) => (present, true),
            Err(error) => {
                // Fail closed -- every row that relied on it asks -- and count
                // it, so a store that cannot answer is not mistaken for one
                // holding nothing.
                crate::perf::note_validity_failed();
                if crate::perf::replay_trace() {
                    eprintln!("ixe replay: valid_paths failed: {error:?}");
                }
                (std::collections::BTreeSet::new(), false)
            }
        };
        let v = Validity {
            present: std::cell::RefCell::new(present),
            mounted: std::cell::RefCell::default(),
            store_answers: std::cell::Cell::new(store_answers),
            sealed,
            store_dir,
            realisable,
            ca_derivations: settings.ca_derivations,
            host,
        };
        for (question, recorded) in rows {
            let kind = crate::purity::question_kind_index(&question.as_need_path());
            if question.replays_from_record(&v.sealed, v.store_dir) {
                crate::perf::note_recorded(kind);
                emit(question, recorded);
                continue;
            }
            let digest = if let Some(digest) = question.answer_by_validity(recorded, &v) {
                crate::perf::note_validated(kind);
                digest
            } else {
                let (answer, nanos) = crate::perf::timed(|| question.ask(host, cas));
                crate::perf::note_replayed(kind, nanos);
                if crate::perf::replay_trace() {
                    eprintln!(
                        "ixe replay: {} {} asked {}ns",
                        crate::purity::QUESTION_KINDS
                            .get(kind)
                            .copied()
                            .unwrap_or("?"),
                        question.arg(),
                        nanos
                    );
                }
                answer?
            };
            if digest != recorded.digest() {
                crate::perf::note_changed(kind);
                // The one debugging knob in this module: which row turned
                // the hit into a miss, on stderr, when asked for by name.
                // `replay.<Kind>_changed` says how many; this says which.
                if crate::perf::replay_trace() {
                    eprintln!(
                        "ixe replay: {} {} changed (recorded {:?}, now {:?})",
                        crate::purity::QUESTION_KINDS
                            .get(kind)
                            .copied()
                            .unwrap_or("?"),
                        question.arg(),
                        recorded,
                        digest
                    );
                }
            }
            emit(question, &Recorded::Digest(digest));
        }
        Ok(Some(()))
    }
}

/// What one replay knows about the store: the sealed objects, how each
/// realised context element stands on store paths, and which paths the
/// store holds.
///
/// Sealing is permanent (content addressing, the link graph) and is
/// remembered across replays; a read under a sealed object replays from its
/// record whether or not the store still holds the object, since a served
/// result reads nothing from it and its name pins what any re-read could
/// see. Presence is a property of what a served result hands out -- the
/// derivations its writes answered, the objects its copies and pinned
/// fetches named, the paths its realisations stand on -- and is asked on
/// every replay: the paths the rows name as one batch before any row is
/// answered, and a path that batch did not hold again at the row that
/// needs it ([`Validity::holds`]), because a row replays what the rows
/// before it left behind: a derivation is absent until its `WriteDrv` row
/// writes it, a copied object until an asked copy makes it. Asked once up
/// front, every such object was absent and its row asked on every replay
/// (round 14, goals/rust-eval.md).
///
/// The row that establishes a path precedes the rows that need it: the
/// recorder notes an effect before handing its answer back
/// (`RecordingHost::collect` for a fetch begun in the background, the
/// blocking routes as they return), so a row naming the path comes after.
/// A negative answer is not remembered: the rows needing one path are few
/// (a derivation's write and the realisations standing on it), and a path
/// absent at one row may be held at the next once an effect between them
/// is asked.
struct Validity<'a> {
    /// Paths the store said it holds: the batch, then each late question
    /// answered yes. Never forgotten within a replay: the embedder roots
    /// what it serves.
    present: std::cell::RefCell<std::collections::BTreeSet<String>>,
    /// Trees served final-tree fetches handed out: the paths the asked
    /// fetches would have left mounted ([`Question::answer_by_validity`]).
    /// Apart from `present` because a mount satisfies `realiseContextCheck`
    /// on an opaque path and nothing else: a copy, filtered copy or pinned
    /// fetch answering the same path would have put the object in the store,
    /// which serving the fetch did not.
    mounted: std::cell::RefCell<std::collections::BTreeSet<String>>,
    /// Whether the host answers validity questions at all. False once one
    /// has failed: a path not already known held is then absent (its row
    /// asks) instead of asking a host that cannot answer once per row.
    store_answers: std::cell::Cell<bool>,
    sealed: crate::host::Sealed,
    store_dir: Option<&'a str>,
    realisable: Realisable,
    ca_derivations: bool,
    host: &'a dyn Host,
}

impl Validity<'_> {
    /// Whether the store holds `path` now: what the batch said, or one
    /// question at this row for a path the batch did not hold.
    fn holds(&self, path: &str) -> bool {
        if self.present.borrow().contains(path) {
            return true;
        }
        if !self.store_answers.get() {
            return false;
        }
        let asked = [path.to_owned()];
        let (answer, nanos) = crate::perf::timed(|| self.host.valid_paths(&asked));
        crate::perf::note_validity_late(nanos);
        match answer {
            Ok(held) if held.contains(path) => {
                self.present.borrow_mut().insert(path.to_owned());
                true
            }
            Ok(_) => false,
            Err(error) => {
                self.store_answers.set(false);
                crate::perf::note_validity_failed();
                if crate::perf::replay_trace() {
                    eprintln!("ixe replay: valid_paths failed: {error:?}");
                }
                false
            }
        }
    }
}

/// What a realised context element stands on: the store paths that must be
/// valid for `realiseContext` to have nothing to build, and for a built
/// output the output path itself (allowed on a hit, and mapped from its
/// placeholder under `ca-derivations`).
#[derive(Clone)]
struct StandsOn {
    needs: Vec<String>,
    output: Option<String>,
}

impl StandsOn {
    /// An opaque path or a deep derivation reference stands on itself:
    /// `realiseContextCheck` (primops.cc:100-114) asks of both only that the
    /// store hold the path.
    fn on_path(path: &str) -> Self {
        Self {
            needs: vec![path.to_owned()],
            output: None,
        }
    }

    /// A built output stands on its derivation and on the output path the
    /// derivation names; `None` for a floating output with no path yet, or
    /// an output the derivation does not have.
    fn built(derivation: &crate::drv::Derivation, drv: &str, output: &str) -> Option<Self> {
        let path = derivation
            .outputs
            .iter()
            .find(|o| o.name == output)
            .map(|o| o.path.as_str())
            .filter(|path| !path.is_empty())?;
        Some(Self {
            needs: vec![drv.to_owned(), path.to_owned()],
            output: Some(path.to_owned()),
        })
    }
}

/// Every element of every `Realise` row that can be resolved to the store
/// paths it stands on. An opaque path or a deep derivation reference stands
/// on itself. A built output stands on its derivation and its output path,
/// read from the derivation's ATerm -- the one the witness's own `WriteDrv`
/// row for that derivation keeps in the CAS, because a realisation during
/// evaluation is of a derivation that evaluation wrote. An element this
/// cannot resolve (no such write in the witness, an ATerm the CAS lost, a
/// floating output with no path yet, a derivation named by a derivation) is
/// absent, and its row asks.
type Realisable = std::collections::BTreeMap<crate::value2::ContextElem, StandsOn>;

fn realisable<C: Cas + ?Sized>(rows: &[WitnessRow], cas: &C) -> Realisable {
    use crate::value2::ContextElem;
    let aterms: std::collections::BTreeMap<&str, ObjId> = rows
        .iter()
        .filter_map(|(question, _)| match question {
            Question::WriteDrv {
                answer: WriteDrvAnswer::Written(path),
                aterm,
                ..
            } => Some((path.as_str(), *aterm)),
            _ => None,
        })
        .collect();
    let mut derivations: std::collections::BTreeMap<&str, Option<crate::drv::Derivation>> =
        std::collections::BTreeMap::new();
    let mut out = Realisable::new();
    for (question, _) in rows {
        let Question::Realise(context) = question else {
            continue;
        };
        for elem in context {
            if out.contains_key(elem) {
                continue;
            }
            let stands_on = match elem {
                ContextElem::Opaque(path) | ContextElem::DrvDeep(path) => {
                    Some(StandsOn::on_path(path))
                }
                ContextElem::Built { drv, output } => {
                    let parsed = derivations.entry(drv.as_ref()).or_insert_with(|| {
                        let aterm = aterms.get(drv.as_ref())?;
                        let ix_kernel::cas::Verified::Found(bytes) =
                            cas.get_verified(*aterm).ok()?
                        else {
                            return None;
                        };
                        crate::drv::parse(std::str::from_utf8(&bytes).ok()?).ok()
                    });
                    parsed
                        .as_ref()
                        .and_then(|derivation| StandsOn::built(derivation, drv, output))
                }
            };
            if let Some(stands_on) = stands_on {
                out.insert(elem.clone(), stands_on);
            }
        }
    }
    out
}

fn context_requires_ifd(context: &[crate::value2::ContextElem]) -> bool {
    context
        .iter()
        .any(|elem| matches!(elem, crate::value2::ContextElem::Built { .. }))
}

/// What `realiseContext` answers for `context` when it has nothing to build,
/// or `None` when that cannot be established and the question must be asked.
///
/// `stands_on` says which store paths an element stands on ([`StandsOn`]);
/// an element it cannot resolve means asking. `mounted` says which paths a
/// served fetch left mounted: `realiseContextCheck` (primops.cc:104) asks
/// the store nothing for a mounted opaque path. `holds` says whether the
/// store holds every path in a set now, each path once (a context names one
/// derivation per output). The answer is the empty rewrite map, or under
/// `ca-derivations` each built output's downstream placeholder mapped to its
/// path (`realiseContextBuild`, primops.cc:172), both pure functions of the
/// derivation; the built outputs are allowed through `host` as
/// `realiseContext` allows what it realised (primops.cc:185), or the
/// question is asked when the host cannot.
///
/// The verifier calls this for a recorded row
/// ([`Question::answer_by_validity`]) and the recorder for a live one
/// ([`RecordingHost::realised_by_validity`]): one definition of "nothing to
/// build", so the two routes cannot drift. Nothing here has an effect: the
/// caller allows the outputs ([`NothingToBuild::outputs`]) when it commits
/// to the answer, and asks instead when the host cannot.
fn nothing_to_build(
    context: &[crate::value2::ContextElem],
    stands_on: impl Fn(&crate::value2::ContextElem) -> Option<StandsOn>,
    mounted: impl Fn(&str) -> bool,
    holds: impl FnOnce(&std::collections::BTreeSet<&str>) -> bool,
    ca_derivations: bool,
) -> Option<NothingToBuild> {
    if context.is_empty() {
        return None;
    }
    let resolved: Vec<StandsOn> = context.iter().map(stands_on).collect::<Option<_>>()?;
    let mut needs = std::collections::BTreeSet::new();
    for (elem, stands_on) in context.iter().zip(&resolved) {
        if matches!(elem, crate::value2::ContextElem::Opaque(_))
            && stands_on.needs.iter().all(|path| mounted(path))
        {
            continue;
        }
        needs.extend(stands_on.needs.iter().map(String::as_str));
    }
    if !holds(&needs) {
        return None;
    }
    let mut rewrites = std::collections::BTreeMap::new();
    let mut outputs = Vec::new();
    for (elem, stands_on) in context.iter().zip(&resolved) {
        if let Some(output) = &stands_on.output {
            if ca_derivations && let crate::value2::ContextElem::Built { drv, output: name } = elem
            {
                rewrites.insert(
                    crate::drvpath::downstream_placeholder(drv, name),
                    output.clone(),
                );
            }
            outputs.push(output.clone());
        }
    }
    Some(NothingToBuild { rewrites, outputs })
}

/// What `realiseContext` answers with nothing to build ([`nothing_to_build`]).
struct NothingToBuild {
    /// The rewrite map: empty, or under `ca-derivations` each built output's
    /// downstream placeholder mapped to its path.
    rewrites: std::collections::BTreeMap<String, String>,
    /// The built outputs, for the caller to allow as `realiseContext` allows
    /// what it realised (primops.cc:185).
    outputs: Vec<String>,
}

/// Which of the store paths a witness's rows rely on the store holds now,
/// asked as one batch before any row replays ([`Validity`] asks again at a
/// row for a path the batch did not hold).
///
/// Every row contributes its [`Question::validity_lines`]: the derivation
/// paths `WriteDrv` rows answered (a write of the same ATerm could answer
/// nothing else, so what a served evaluation needs is that the derivation
/// exists), the paths `Realise` rows stand on ([`Realisable`]), and the
/// objects kept copy and pinned-fetch answers named (a locked final tree's
/// kept answer is asked of no store: its row replays regardless). One
/// `queryValidPaths` batch
/// replaces one write, one build or one copy per row: 23k derivation
/// writes, 11k realisations and 1k filtered copies on one home-manager
/// witness. A host with no store, or one whose query fails, is an error the
/// caller fails closed on: every row asks, and no presence is asked again
/// at a row ([`Validity::holds`]).
///
/// No temporary root is taken here: the embedder roots the derivation paths
/// it serves as answers, whose closures cover every input derivation.
fn present_objects(
    rows: &[WitnessRow],
    realisable: &Realisable,
    host: &dyn Host,
) -> Result<std::collections::BTreeSet<String>, StoreError> {
    let mut lines = Vec::new();
    for (question, recorded) in rows {
        question.validity_lines(recorded, realisable, &mut lines);
    }
    lines.sort_unstable();
    lines.dedup();
    if lines.is_empty() {
        return Ok(std::collections::BTreeSet::new());
    }
    let (answer, nanos) = crate::perf::timed(|| host.valid_paths(&lines));
    crate::perf::note_validity(lines.len(), nanos);
    answer
}

/// The store object (`<hash>-<name>`) an ambient path lies in, when it lies
/// under `store_dir` and the first component below it is a well-formed
/// store path name; the store object's name is what [`sealed_objects`]
/// asks about. `None` for a path anywhere else, and when the embedder never
/// said where the store is.
fn store_object_of(store_dir: Option<&str>, path: &str) -> Option<String> {
    let store_dir = store_dir?;
    let below = path.strip_prefix(store_dir)?.strip_prefix('/')?;
    let object = below
        .split('/')
        .next()
        .filter(|object| !object.is_empty())?;
    crate::storepath::parse_store_path(store_dir, &format!("{store_dir}/{object}"))
}

/// Whether a path's bytes cannot change under its spelling: a mounted root
/// (a store path the fetcher named from the tree's content, read through
/// its own accessor), or an ambient path inside a store object in `sealed`
/// that is not at or below a symlink leaving that object. Every byte of a
/// sealed object is pinned by its name, link text included; the one way a
/// read under it reaches bytes the name does not pin is by resolving a link
/// out of the object, so a path at or under such a link asks, and every
/// other path -- a sibling, the object root, a copy of the whole tree, which
/// copies links as links -- replays.
fn immutable_root(
    path: &crate::value2::PathValue,
    sealed: &crate::host::Sealed,
    store_dir: Option<&str>,
) -> bool {
    match path.root {
        crate::value2::Root::Mounted(_) => true,
        crate::value2::Root::Ambient => {
            let Some(store_dir) = store_dir else {
                return false;
            };
            let Some(object) = store_object_of(Some(store_dir), &path.path) else {
                return false;
            };
            let Some(leaving) = sealed.get(&object) else {
                return false;
            };
            let root = format!("{store_dir}/{object}");
            !leaving
                .iter()
                .any(|rel| at_or_below(&path.path, &format!("{root}/{rel}")))
        }
    }
}

/// The store objects under which a witness's rows read or copy that are
/// sealed ([`Host::sealed_paths`]), by base name, each with the symlinks
/// that leave it.
///
/// Each object is asked about once ever: `memo` holds every object the
/// embedder has called sealed, and an object in it is never sent again. The
/// rest go to the host in one question; those it names are recorded and
/// served, those it does not (absent from the store, or not
/// content-addressed) are asked again next time and every row under them
/// asks as before. Charged to `replay.sealing_objects` / `replay.sealing_ns`,
/// its own counters: a different call with a different unit from the
/// validity batch.
fn sealed_objects(
    rows: &[WitnessRow],
    host: &dyn Host,
    store_dir: Option<&str>,
    memo: Option<&DirSealed>,
) -> crate::host::Sealed {
    let mut sealed = crate::host::Sealed::new();
    let mut unknown = std::collections::BTreeSet::new();
    let trace = crate::perf::replay_trace();
    for (question, _) in rows {
        let Some(path) = question.ambient_path() else {
            continue;
        };
        let Some(object) = store_object_of(store_dir, &path.path) else {
            continue;
        };
        if sealed.contains_key(&object) || unknown.contains(&object) {
            continue;
        }
        if let Some(leaving) = memo.and_then(|memo| memo.get(&object)) {
            sealed.insert(object, leaving);
        } else if memo.is_some_and(|memo| memo.declined(&object)) {
            // Declined earlier through this handle: not sealed, and not asked.
        } else {
            if trace {
                // The row that makes the object a question: what to look at
                // when the walk behind the answer is slow (round 15b: 30s
                // for the flake's own tree, read once by an ambient spelling).
                let kind = crate::purity::question_kind_index(&question.as_need_path());
                eprintln!(
                    "ixe sealing: {object} unknown, first asked by {} {}",
                    crate::purity::QUESTION_KINDS
                        .get(kind)
                        .copied()
                        .unwrap_or("?"),
                    question.arg()
                );
            }
            unknown.insert(object);
        }
    }
    if unknown.is_empty() {
        return sealed;
    }
    let asked: Vec<String> = unknown.into_iter().collect();
    let (answer, nanos) = crate::perf::timed(|| host.sealed_paths(&asked));
    crate::perf::note_sealing(asked.len(), nanos);
    let answered = match answer {
        Ok(answered) => {
            // An object the host left out of a successful answer is declined
            // for the process; a failed question declines nothing, since the
            // next one may succeed.
            if let Some(memo) = memo {
                for object in asked
                    .iter()
                    .filter(|object| !answered.contains_key(*object))
                {
                    memo.decline(object);
                }
            }
            answered
        }
        Err(error) => {
            crate::perf::note_sealing_failed();
            if crate::perf::replay_trace() {
                eprintln!("ixe replay: sealed_paths failed: {error:?}");
            }
            crate::host::Links::new()
        }
    };
    for (object, links) in answered {
        let leaving = leaving_links(&links);
        if let Some(memo) = memo {
            // A fact that could not be written is a fact asked again next
            // time; it must not fail the lookup that learned it.
            drop(memo.insert(&object, &leaving));
        }
        sealed.insert(object, leaving);
    }
    sealed
}

/// Which of an object's symlinks leave it: the ones a read can follow to
/// bytes the object's name does not pin.
///
/// A link leaves outright when its target is absolute or climbs past the
/// object root; otherwise the target names a path inside the object, resolved
/// lexically against the link's directory (a dangling target that stays
/// inside does not leave: its text is part of the object). The resolution
/// is component by component, as cppnix's `resolveSymlinks` is, and it
/// stops the moment a component before the last is itself a link: what
/// follows (a `..`, a name) applies to THAT link's target, which this
/// lexical walk does not have, so the link is called leaving -- a read
/// through it asks, which is slow and right, where collapsing `dir/out/..`
/// first would have called `alias -> dir/out/../x` internal with
/// `dir/out -> /etc`. Then to a fixpoint: a link whose in-object target lies
/// at or below a leaving link resolves through it, and a link whose target
/// lies at or above one aliases a subtree containing it, so a read spelled
/// through the alias reaches the leaving link under a name the prefix rule
/// would not see. Both leave. A cycle of in-object links leaves nothing:
/// resolving it fails the same way every time, and that failure is recorded
/// like any answer.
pub(crate) fn leaving_links(links: &[crate::host::Symlink]) -> Vec<String> {
    let is_link: std::collections::BTreeSet<&str> =
        links.iter().map(|link| link.path.as_str()).collect();
    // `None`: leaves outright (or traverses a link, which is the same
    // verdict); `Some(p)`: names in-object path `p` ("" is the object root).
    let mut target_of: std::collections::BTreeMap<&str, Option<String>> =
        std::collections::BTreeMap::new();
    for link in links {
        let mut stack: Vec<&str> = link.path.split('/').collect();
        stack.pop();
        let resolved = if link.target.is_empty() || link.target.starts_with('/') {
            None
        } else {
            let components: Vec<&str> = link
                .target
                .split('/')
                .filter(|c| !c.is_empty() && *c != ".")
                .collect();
            let mut inside = true;
            for (i, component) in components.iter().enumerate() {
                if *component == ".." {
                    if stack.pop().is_none() {
                        inside = false;
                        break;
                    }
                    continue;
                }
                stack.push(*component);
                if i + 1 < components.len() && is_link.contains(stack.join("/").as_str()) {
                    inside = false;
                    break;
                }
            }
            inside.then(|| stack.join("/"))
        };
        target_of.insert(link.path.as_str(), resolved);
    }
    let mut leaving: std::collections::BTreeSet<String> = target_of
        .iter()
        .filter(|(_, target)| target.is_none())
        .map(|(path, _)| (*path).to_owned())
        .collect();
    loop {
        let before = leaving.len();
        for (path, target) in &target_of {
            if leaving.contains(*path) {
                continue;
            }
            let Some(target) = target else {
                continue;
            };
            if leaving
                .iter()
                .any(|link| at_or_below(target, link) || at_or_below(link, target))
            {
                leaving.insert((*path).to_owned());
            }
        }
        if leaving.len() == before {
            break;
        }
    }
    leaving.into_iter().collect()
}

/// Whether `path` is `prefix` or lies below it, both object-relative; the
/// empty prefix is the object root, below which everything lies.
fn at_or_below(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The `outPath` of a fetched tree's answer, which is the JSON of the
/// attribute set `emitTreeAttrs` built (`crate::task::NeedPath::FetchTree`).
/// `None` for anything else, which then replays by asking.
fn tree_out_path(json: &str) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(json).ok()?;
    doc.get("outPath")?.as_str().map(str::to_owned)
}

fn read_entry_fingerprint(question: &Question, answer: &Hash) -> Hash {
    let mut parts = vec![vec![question.tag()], question.arg().as_bytes().to_vec()];
    parts.extend(question.key_parts());
    parts.push(answer.as_bytes().to_vec());
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    hash::tagged("ixe-read-entry-v1", &refs)
}

/// A line the evaluation printed rather than returned.
///
/// `builtins.trace` and cppnix's `warn()` both go out through [`Host`], and
/// both are part of what an evaluation produced. One ordered list holds them
/// together rather than two lists side by side, because cppnix emits them
/// interleaved in the order they happen and two lists could only be replayed
/// in an order nothing produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Emission {
    Warn(String),
    Trace(String),
}

impl Emission {
    /// Say it again, through the host a served evaluation was given.
    pub fn replay(&self, host: &dyn Host) {
        match self {
            Self::Warn(message) => host.warn(message),
            Self::Trace(message) => host.trace(message),
        }
    }

    /// `(kind, text)`, for the codec.
    const fn parts(&self) -> (&'static str, &String) {
        match self {
            Self::Warn(message) => ("warn", message),
            Self::Trace(message) => ("trace", message),
        }
    }

    /// The inverse of [`Emission::parts`]. A kind this build does not know
    /// makes the whole row undecodable rather than a line silently dropped:
    /// the point of storing these is that a served run says the same thing,
    /// and half of what it said is not that.
    fn from_parts(kind: &str, message: String) -> Option<Self> {
        match kind {
            "warn" => Some(Self::Warn(message)),
            "trace" => Some(Self::Trace(message)),
            _ => None,
        }
    }
}

/// A host that answers through another host and remembers what it was asked.
///
/// `RefCell` rather than `&mut self` because [`Host`] takes `&self`: the
/// evaluator holds it immutably while the scheduler drives it, and recording
/// is the only mutation.
pub struct RecordingHost<H> {
    inner: H,
    /// Whether `warn` and `trace` reach `inner` as well as being recorded.
    ///
    /// Off for a sampled verification, which re-does work the memo already
    /// answered: the served answer's emissions are replayed instead, so a
    /// reader must not also see this run's copy. It is one field rather than
    /// a wrapper host because a wrapper has to forward every other method by
    /// hand, and the one that got written forgot `store_text` and answered
    /// "no store behind this evaluator" on behalf of a host that had one.
    /// [`Host`] has no default method bodies precisely so that mistake cannot
    /// be silent; not needing the wrapper at all is better still.
    forward_emissions: bool,
    log: RefCell<Vec<WitnessRow>>,
    seen: RefCell<std::collections::BTreeSet<Hash>>,
    rows_deduped: Cell<u64>,
    aterms: RefCell<BTreeMap<ObjId, Vec<u8>>>,
    aterms_reused: Cell<u64>,
    /// Warnings and traces the evaluation emitted, interleaved in the order
    /// it emitted them.
    ///
    /// Not questions: an emission is an output, so it has no answer to key
    /// on. It still has to be remembered, because a memoised result served on
    /// a later run reproduces the value and would otherwise stay silent about
    /// something the first run said out loud -- which is `eval-cache-dir`
    /// changing what the evaluator tells the reader.
    emissions: RefCell<Vec<Emission>>,
    /// Slow questions begun through [`Host::begin`] and not yet collected.
    ///
    /// A question begun in the background is not recorded when it is asked,
    /// because the answer -- which is half of what the read set stores -- does
    /// not exist yet. So the question is held here and noted when its answer
    /// arrives, which puts it in the log at the point the evaluation actually
    /// received it.
    ///
    /// That the log stays deterministic despite this is a property of the
    /// scheduler and not of this map. One evaluation can have several
    /// questions outstanding since ENG-13150, so this map does hold more
    /// than one entry at a time -- but `crate::eval::drive_concurrent`
    /// collects strictly oldest-token-first, and tokens are minted at ask,
    /// so begun questions land in the log in ask order however their
    /// answers raced. A begun question CAN land after a synchronous one
    /// asked later by a sibling strand, which reorders the log relative to
    /// a host that begins nothing; that moves `ReadSet::key` the same way
    /// on every run against the same host, a deterministic miss and not a
    /// wrong answer. `crate::vm::Fiber`'s doc is where the invariant that
    /// keeps all of this sound is written down.
    begun: RefCell<std::collections::HashMap<u64, Question>>,
    /// The per-question memo of filtered copies, when the session has an
    /// on-disk cache to keep it in. See [`CopyMemo`].
    copy_memo: Option<CopyMemo>,
    /// The derivations this evaluation wrote, by path, each to its ATerm in
    /// `aterms`: what a live realisation of one of their outputs stands on
    /// ([`RecordingHost::realised_by_validity`]). A derivation path is a
    /// text path of its ATerm (`writeDerivation`, `makeTextPath`), so one
    /// path names one ATerm and a second write of the path carries the same
    /// bytes (`aterms_reused`); should a store ever answer one path for two
    /// ATerms, the path maps to `None` for the rest of the evaluation and a
    /// realisation of it asks.
    written: RefCell<BTreeMap<String, Option<ObjId>>>,
    /// Whether a live realisation with nothing to build is answered without
    /// asking for the build, and under which `ca-derivations` setting its
    /// rewrite map is computed. See [`RecordingHost::realised_by_validity`].
    realise_by_validity: Option<RealiseByValidity>,
}

/// How [`RecordingHost::realised_by_validity`] computes the rewrite map: the
/// one setting `realiseContextBuild` reads (primops.cc:172).
#[derive(Clone, Copy)]
struct RealiseByValidity {
    ca_derivations: bool,
    allow_import_from_derivation: bool,
}

impl<H: Host> RecordingHost<H> {
    #[must_use]
    pub fn new(inner: H) -> Self {
        Self {
            inner,
            forward_emissions: true,
            log: RefCell::new(Vec::new()),
            seen: RefCell::new(std::collections::BTreeSet::new()),
            rows_deduped: Cell::new(0),
            aterms: RefCell::new(BTreeMap::new()),
            aterms_reused: Cell::new(0),
            emissions: RefCell::new(Vec::new()),
            begun: RefCell::new(std::collections::HashMap::new()),
            copy_memo: None,
            written: RefCell::new(BTreeMap::new()),
            realise_by_validity: None,
        }
    }

    /// Answer filtered copies from `memo` where it is sound to
    /// ([`CopyMemo`]), asking the host behind this one only on a miss. The
    /// recording is the same either way.
    #[must_use]
    pub fn with_copy_memo(mut self, memo: CopyMemo) -> Self {
        self.copy_memo = Some(memo);
        self
    }

    /// Answer a live realisation with nothing to build without asking the
    /// host behind this one to build ([`RecordingHost::realised_by_validity`]).
    /// The recording is the same either way.
    #[must_use]
    pub fn realising_by_validity(
        mut self,
        ca_derivations: bool,
        allow_import_from_derivation: bool,
    ) -> Self {
        self.realise_by_validity = Some(RealiseByValidity {
            ca_derivations,
            allow_import_from_derivation,
        });
        self
    }

    /// What `realiseContext` answers for `context` with nothing to build,
    /// when this recorder can establish that now, or `None`: the question is
    /// asked.
    ///
    /// The verifier serves a recorded realisation by validity
    /// ([`Question::answer_by_validity`]); this is the same answer for a live
    /// one, on a miss of the whole evaluation. Every element has to stand on
    /// store paths this recorder can name ([`nothing_to_build`]): an opaque
    /// path or a deep derivation reference on itself, a built output on a
    /// derivation this evaluation wrote (`written`; its ATerm names the
    /// output path; a realisation during evaluation is of a derivation that
    /// evaluation wrote, and one of any other derivation asks). The store is
    /// then asked once whether it holds them all (`valid_paths`, which lands
    /// a queued write of the derivation first). The caller commits to the
    /// answer by allowing its outputs as `realiseContext` allows what it
    /// realised ([`Host::realise`] below does; [`Host::begin`] only asks).
    /// What is skipped is the build the embedder runs for a context with
    /// nothing to build: 17 realisations at 110 ms of daemon-side
    /// `buildPaths` each per edit of the real config (bed l2db5c), every one
    /// of a derivation whose outputs were already valid. The row recorded
    /// is the row the asked route records when it has nothing to build, so
    /// a witness cannot tell which route answered.
    ///
    /// A path the store does not hold, an element that cannot be resolved,
    /// or a store that does not answer all mean `None`. Nothing is
    /// remembered between two realisations: the store is asked at each, so
    /// a path lost between them is seen.
    fn realised_by_validity(
        &self,
        context: &[crate::value2::ContextElem],
    ) -> Option<NothingToBuild> {
        let RealiseByValidity {
            ca_derivations,
            allow_import_from_derivation,
        } = self.realise_by_validity?;
        if !allow_import_from_derivation && context_requires_ifd(context) {
            return None;
        }
        nothing_to_build(
            context,
            |elem| self.stands_on_written(elem),
            |_: &str| false,
            |needs: &std::collections::BTreeSet<&str>| {
                let asked: Vec<String> = needs.iter().map(|path| (*path).to_owned()).collect();
                asked.is_empty()
                    || self
                        .inner
                        .valid_paths(&asked)
                        .is_ok_and(|held| asked.iter().all(|path| held.contains(path)))
            },
            ca_derivations,
        )
    }

    /// What one element of a live realisation stands on ([`StandsOn`]): a
    /// built output of a derivation this evaluation did not write is `None`.
    fn stands_on_written(&self, elem: &crate::value2::ContextElem) -> Option<StandsOn> {
        use crate::value2::ContextElem;
        match elem {
            ContextElem::Opaque(path) | ContextElem::DrvDeep(path) => Some(StandsOn::on_path(path)),
            ContextElem::Built { drv, output } => {
                let aterm = (*self.written.borrow().get(drv.as_ref())?)?;
                let derivation = {
                    let aterms = self.aterms.borrow();
                    crate::drv::parse(std::str::from_utf8(aterms.get(&aterm)?).ok()?).ok()?
                };
                StandsOn::built(&derivation, drv, output)
            }
        }
    }

    /// A recorder that answers every question and repeats nothing.
    ///
    /// Only the two outputs are dropped. Every read still goes through,
    /// because the point of a verification run is to evaluate against the
    /// same world the cached answer was taken from. See
    /// [`RecordingHost::forward_emissions`].
    #[must_use]
    pub fn quiet(inner: H) -> Self {
        Self {
            forward_emissions: false,
            ..Self::new(inner)
        }
    }

    /// Everything asked since the last [`take`].
    ///
    /// [`take`]: RecordingHost::take
    #[must_use]
    pub fn take(&self) -> ReadSet {
        self.seen.borrow_mut().clear();
        ReadSet {
            entries: core::mem::take(&mut self.log.borrow_mut()),
            aterms: core::mem::take(&mut self.aterms.borrow_mut()),
            aterms_reused: self.aterms_reused.replace(0),
            rows_deduped: self.rows_deduped.replace(0),
        }
    }

    /// Everything warned since the last call.
    #[must_use]
    pub fn take_emissions(&self) -> Vec<Emission> {
        core::mem::take(&mut self.emissions.borrow_mut())
    }

    /// Record a question and its answer. The answer is digested from the value
    /// actually returned, so the log cannot disagree with what the evaluator
    /// was told.
    ///
    /// # Why dropping a repeat is sound
    ///
    /// A repeat is dropped only when BOTH the question and the answer digest
    /// already occurred in this evaluation (`read_entry_fingerprint` covers
    /// both), so the log still names every distinct fact the value depends
    /// on, and a question that came back with a different answer the second
    /// time keeps both entries. Replay (`Question::ask`) re-asks each logged
    /// question once and compares digests; it does not re-perform the
    /// evaluation's effects in their original multiplicity, and no store
    /// question has an effect that a second identical ask changes -- a copy
    /// of the same bytes yields the same path, a derivation written twice is
    /// one file. The key is over the deduplicated sequence on both the
    /// recording and the replaying side, which is what makes this a smaller
    /// witness and not a different one.
    fn note(&self, question: Question, answer: Hash) {
        self.record(question, Recorded::Digest(answer));
    }

    /// [`RecordingHost::note`] with the record chosen by the caller: the
    /// store effects whose answer a row keeps ([`Recorded::store_copy`]).
    fn record(&self, question: Question, recorded: Recorded) {
        let fingerprint = read_entry_fingerprint(&question, &recorded.digest());
        if !self.seen.borrow_mut().insert(fingerprint) {
            self.rows_deduped
                .set(self.rows_deduped.get().wrapping_add(1));
            return;
        }
        self.log.borrow_mut().push((question, recorded));
    }
}

/// The [`Question`] a slow question records as.
///
/// The same question the blocking method would have recorded, which is the
/// point: a read set must not be able to say which of the two routes to the
/// host an evaluation happened to take. A witness recorded through `begin`
/// replays through `Question::ask`, which calls the blocking method.
fn slow_question(question: &crate::host::Slow<'_>) -> Question {
    match question {
        crate::host::Slow::Fetch(request) => Question::Fetch(Box::new((*request).clone())),
        crate::host::Slow::FetchTree(request) => Question::FetchTree(Box::new((*request).clone())),
        crate::host::Slow::Flake(flake_ref) => Question::LockFlake((*flake_ref).to_owned()),
        crate::host::Slow::Realise(context) => Question::Realise((*context).to_vec()),
    }
}

impl<H: Host> Host for RecordingHost<H> {
    /// Not a question, so nothing is recorded: the inner host makes the
    /// effects it deferred visible. It runs before this evaluation's result
    /// is recorded, which is what keeps a result whose derivations the store
    /// refused out of the memo.
    fn settle(&self) -> Result<(), crate::host::StoreError> {
        self.inner.settle()
    }

    fn import_source(
        &self,
        path: &crate::value2::PathValue,
    ) -> Result<crate::host::ImportedSource, String> {
        let answer = self.inner.import_source(path);
        self.note(
            Question::Import(Rc::new(path.clone())),
            digest_import(&answer),
        );
        answer
    }

    fn read_file(&self, path: &crate::value2::PathValue) -> Result<String, String> {
        let answer = self.inner.read_file(path);
        let digest = match &answer {
            Ok(text) => digest(&[b"file-ok", text.as_bytes()]),
            Err(error) => digest(&[b"file-err", error.as_bytes()]),
        };
        self.note(Question::ReadFile(Rc::new(path.clone())), digest);
        answer
    }

    fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
        let answer = self.inner.read_file_bytes(path);
        self.note(
            Question::ReadFileBytes(Rc::new(path.clone())),
            digest_file_bytes(&answer),
        );
        answer
    }

    fn read_dir(&self, path: &crate::value2::PathValue) -> Result<Vec<(String, FileType)>, String> {
        let answer = self.inner.read_dir(path);
        let digest = match &answer {
            Ok(entries) => {
                let mut parts: Vec<Vec<u8>> = vec![b"dir-ok".to_vec()];
                for (name, kind) in entries {
                    parts.push(name.clone().into_bytes());
                    parts.push(kind.as_str().as_bytes().to_vec());
                }
                let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
                super::readset::digest(&refs)
            }
            Err(error) => digest(&[b"dir-err", error.as_bytes()]),
        };
        self.note(Question::ReadDir(Rc::new(path.clone())), digest);
        answer
    }

    fn path_exists_checked(&self, path: &crate::value2::PathValue) -> Result<bool, String> {
        let answer = self.inner.path_exists_checked(path);
        self.note(
            Question::PathExists(Rc::new(path.clone())),
            digest_exists(&answer),
        );
        answer
    }

    fn dir_exists_checked(&self, path: &crate::value2::PathValue) -> Result<bool, String> {
        let answer = self.inner.dir_exists_checked(path);
        self.note(
            Question::DirExists(Rc::new(path.clone())),
            digest_exists(&answer),
        );
        answer
    }

    fn copy_to_store(&self, path: &crate::value2::PathValue) -> Result<String, StoreError> {
        let answer = self.inner.copy_to_store(path);
        let question = Question::CopyToStore(Rc::new(path.clone()));
        let recorded = Recorded::store_copy(&question, &answer);
        self.record(question, recorded);
        answer
    }

    fn store_path(
        &self,
        path: &crate::value2::PathValue,
    ) -> Result<crate::host::StorePathResult, StoreError> {
        let answer = self.inner.store_path(path);
        self.note(
            Question::StorePath(Rc::new(path.clone())),
            digest_store_path(&answer),
        );
        answer
    }

    fn store_text(
        &self,
        name: &str,
        contents: &str,
        references: &[String],
    ) -> Result<String, StoreError> {
        let answer = self.inner.store_text(name, contents, references);
        self.note(
            Question::StoreText {
                name: name.to_owned(),
                contents: contents.to_owned(),
                references: references.to_vec(),
            },
            digest_store_text(&answer),
        );
        answer
    }

    fn ensure_path(&self, path: &str) -> Result<(), StoreError> {
        let answer = self.inner.ensure_path(path);
        self.note(
            Question::EnsurePath(path.to_owned()),
            digest_store_ensure(&answer),
        );
        answer
    }

    fn valid_paths(
        &self,
        paths: &[String],
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        // The verifier's question about a witness, not an evaluation's
        // question about the world: it is not recorded.
        self.inner.valid_paths(paths)
    }

    fn sealed_paths(&self, objects: &[String]) -> Result<crate::host::Links, StoreError> {
        // The verifier's, as `valid_paths` is: not recorded.
        self.inner.sealed_paths(objects)
    }

    fn allow_paths(&self, paths: &[String]) -> Result<(), StoreError> {
        // The verifier's too; the effect it stands in for is the row.
        self.inner.allow_paths(paths)
    }

    fn allow_closures(&self, outputs: &[String]) -> Result<(), StoreError> {
        self.inner.allow_closures(outputs)
    }

    fn realise(
        &self,
        context: &[crate::value2::ContextElem],
    ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
        let answer = match self.realised_by_validity(context) {
            Some(served)
                if served.outputs.is_empty()
                    || self.inner.allow_closures(&served.outputs).is_ok() =>
            {
                crate::perf::note_realise_validated();
                Ok(served.rewrites)
            }
            _ => self.inner.realise(context),
        };
        self.note(Question::Realise(context.to_vec()), digest_realise(&answer));
        answer
    }

    fn write_derivation(&self, name: &str, aterm: &str) -> Result<String, StoreError> {
        let answer = self.inner.write_derivation(name, aterm);
        let aterm_id = ObjId::of(aterm.as_bytes());
        if let Ok(path) = &answer {
            self.written
                .borrow_mut()
                .entry(path.clone())
                .and_modify(|known| {
                    if *known != Some(aterm_id) {
                        *known = None;
                    }
                })
                .or_insert(Some(aterm_id));
        }
        match self.aterms.borrow_mut().entry(aterm_id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(aterm.as_bytes().to_vec());
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                self.aterms_reused
                    .set(self.aterms_reused.get().wrapping_add(1));
            }
        }
        self.note(
            Question::WriteDrv {
                name: name.to_owned(),
                answer: WriteDrvAnswer::of(&answer),
                aterm: aterm_id,
            },
            digest_write_drv(&answer),
        );
        answer
    }

    fn store_filtered(&self, request: &crate::task::FilteredCopy) -> Result<String, StoreError> {
        let started = std::time::Instant::now();
        let question = Question::StoreFiltered(Box::new(request.clone()));
        // The memo answers before the host and below the recording, so the
        // row is the same whichever answered: see `CopyMemo`.
        let (looked, cost) = match self.copy_memo.as_ref() {
            Some(memo) => {
                let (lookup, cost) = memo.look(&question, &self.inner);
                (Some((memo, lookup)), cost)
            }
            None => (None, CopyCost::default()),
        };
        let (route, answer) = match looked {
            Some((_, CopyLookup::Served(path))) => ("memo", Ok(path)),
            Some((memo, CopyLookup::Miss(key))) => {
                let answer = self.inner.store_filtered(request);
                if let Ok(path) = &answer {
                    memo.remember(&key, path);
                }
                ("walk", answer)
            }
            Some((_, CopyLookup::Mutable)) => ("mutable", self.inner.store_filtered(request)),
            None => ("nomemo", self.inner.store_filtered(request)),
        };
        let recording = std::time::Instant::now();
        let recorded = Recorded::store_copy(&question, &answer);
        self.record(question, recorded);
        if crate::perf::replay_trace() {
            eprintln!(
                "{}",
                store_filtered_trace(
                    route,
                    request,
                    &cost,
                    u64::try_from(recording.elapsed().as_nanos()).unwrap_or(u64::MAX),
                    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
                    &answer,
                )
            );
        }
        answer
    }

    fn fetch(&self, request: &crate::task::FetchRequest) -> Result<String, StoreError> {
        let answer = self.inner.fetch(request);
        let question = Question::Fetch(Box::new(request.clone()));
        let recorded = Recorded::store_copy(&question, &answer);
        self.record(question, recorded);
        answer
    }

    fn fetch_tree(&self, request: &crate::task::FetchTreeRequest) -> Result<String, StoreError> {
        let answer = self.inner.fetch_tree(request);
        let question = Question::FetchTree(Box::new(request.clone()));
        let recorded = Recorded::store_copy(&question, &answer);
        self.record(question, recorded);
        answer
    }

    fn lock_flake(&self, flake_ref: &str) -> Result<crate::host::FlakeCall, StoreError> {
        let answer = self.inner.lock_flake(flake_ref);
        // The lock file and the overrides go into the digest; the
        // `call-flake.nix` source does not. It is a compile-time constant of
        // the embedder binary, identical for every call in a process, so
        // digesting it would add a field that never varies -- and if it ever
        // did vary, the binary changed, which the evaluator settings
        // fingerprint already covers.
        self.note(
            Question::LockFlake(flake_ref.to_owned()),
            digest_flake_call(&answer),
        );
        answer
    }

    fn parse_flake_ref(&self, flake_ref: &str) -> Result<String, StoreError> {
        let answer = self.inner.parse_flake_ref(flake_ref);
        self.note(
            Question::ParseFlakeRef(flake_ref.to_owned()),
            digest_store_copy(&answer),
        );
        answer
    }

    fn flake_ref_to_string(
        &self,
        attrs: &std::collections::BTreeMap<String, crate::task::TreeAttr>,
    ) -> Result<String, StoreError> {
        let answer = self.inner.flake_ref_to_string(attrs);
        self.note(
            Question::FlakeRefToString(attrs.clone()),
            digest_store_copy(&answer),
        );
        answer
    }

    /// Forwarded *and* remembered, and the same for [`Host::trace`].
    ///
    /// Forwarded because the embedder's logger is where the line belongs on
    /// the run that produced it. Remembered because a run served from the
    /// memo table never executes the code that emitted it, and a cached run
    /// that stayed quiet would tell its reader less than the run that filled
    /// the cache did -- `eval-cache-dir` deciding how much the evaluator
    /// says, which is the same class of divergence as deciding what it
    /// answers.
    fn warn(&self, message: &str) {
        self.emissions
            .borrow_mut()
            .push(Emission::Warn(message.to_owned()));
        if self.forward_emissions {
            self.inner.warn(message);
        }
    }

    fn trace(&self, message: &str) {
        self.emissions
            .borrow_mut()
            .push(Emission::Trace(message.to_owned()));
        if self.forward_emissions {
            self.inner.trace(message);
        }
    }

    fn file_type(&self, path: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
        let answer = self.inner.file_type(path);
        let digest = digest_file_type(&answer);
        self.note(Question::FileType(Rc::new(path.clone())), digest);
        answer
    }

    /// Recorded separately from [`Host::file_type`] because it is a separate
    /// question with a separate answer. `Host::resolve_import` keeps its
    /// default body here, as its doc says it must, and that body now asks
    /// this -- so the import's world-read lands in the log through this
    /// method rather than through `file_type`.
    fn file_type_resolved(&self, path: &crate::value2::PathValue) -> Result<FileType, String> {
        let answer = self.inner.file_type_resolved(path);
        let digest = match &answer {
            Ok(kind) => digest(&[b"kind-resolved-ok", kind.as_str().as_bytes()]),
            Err(error) => digest(&[b"kind-resolved-err", error.as_bytes()]),
        };
        self.note(Question::FileTypeResolved(Rc::new(path.clone())), digest);
        answer
    }

    fn find_file(
        &self,
        entries: &[SearchPathEntry],
        name: &str,
    ) -> Result<crate::value2::PathValue, LookupError> {
        let answer = self.inner.find_file(entries, name);
        self.note(
            Question::FindFile {
                entries: entries.to_vec(),
                name: name.to_owned(),
            },
            digest_find_file(&answer),
        );
        answer
    }

    fn nix_path(&self) -> Result<Vec<SearchPathEntry>, LookupError> {
        let answer = self.inner.nix_path();
        self.note(Question::NixPath, digest_nix_path(&answer));
        answer
    }

    fn get_env(&self, name: &str) -> Option<String> {
        let answer = self.inner.get_env(name);
        let digest = match &answer {
            Some(value) => digest(&[b"env-set", value.as_bytes()]),
            None => digest(&[b"env-unset"]),
        };
        self.note(Question::GetEnv(name.to_owned()), digest);
        answer
    }

    /// Both halves are forwarded, and the recording happens on the collecting
    /// half.
    ///
    /// A wrapper that inherited the defaults here would be worse than one
    /// that forgot an effect: `begin` returning `None` does not break
    /// anything, it only turns the asynchronous path off silently, and a
    /// performance feature that quietly does not apply is the kind of thing
    /// that stays broken for a year. `the_recorder_forwards_a_begun_question`
    /// is what says it does not.
    fn begin(&self, question: &crate::host::Slow<'_>) -> Option<crate::host::Ticket> {
        // A realisation with nothing to build is not begun: `realise` answers
        // it on the spot, the way a question the access check refuses is
        // answered on the synchronous route (`eval::begin_one`). The store is
        // asked twice for it, here and there, at ~65 us a time against the
        // 110 ms build the answer skips; an answer stashed between the two
        // would be state with a second owner. Nothing is allowed here: the
        // outputs are allowed once, by the route that commits to the answer.
        // `None` is `Host::begin`'s own word for "ask the blocking method":
        // the scheduler (`eval::begin_one`) turns it into a `realise` call.
        if let crate::host::Slow::Realise(context) = question
            && self.realised_by_validity(context).is_some()
        {
            return None;
        }
        let ticket = self.inner.begin(question)?;
        self.begun
            .borrow_mut()
            .insert(ticket.0, slow_question(question));
        Some(ticket)
    }

    fn collect(&self, ticket: crate::host::Ticket, block: bool) -> Option<crate::host::SlowAnswer> {
        let answer = self.inner.collect(ticket, block)?;
        // Only now is there an answer to digest, so only now can the question
        // be recorded. A `begin` this recorder did not see -- which cannot
        // happen, since it is the only door -- leaves nothing to note rather
        // than noting a question with a made-up answer.
        if let Some(question) = self.begun.borrow_mut().remove(&ticket.0) {
            // A store answer keeps its text as the blocking route keeps it,
            // so a fetch begun asynchronously can replay by validity too.
            let recorded = match &answer {
                crate::host::SlowAnswer::Store(answer) => Recorded::store_copy(&question, answer),
                crate::host::SlowAnswer::Flake(answer) => {
                    Recorded::Digest(digest_flake_call(answer))
                }
                crate::host::SlowAnswer::Realise(answer) => {
                    Recorded::Digest(digest_realise(answer))
                }
            };
            self.record(question, recorded);
        }
        Some(answer)
    }
}

// -------------------------------------------------------------- result cache

use ix_kernel::canon::{self, CanonValue};
use ix_kernel::cas::Cas;
use ix_kernel::dispatch::{PerformCtx, on_perform};
use ix_kernel::rows::Lookup;
use ix_kernel::{Domain, EffectLock, KernelConfig, KernelError, MemoTable, Policy};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The effect being memoised: evaluating one module to a printed result.
#[must_use]
pub fn eval_domain() -> Domain {
    Domain::mint("ix-eval.evaluate", "module-result")
}

/// A printed evaluation outcome: everything the server would have said.
///
/// `emissions` is part of the outcome and not a bystander. cppnix warns about
/// six derivation attributes `__structuredAttrs` quietly disables, and
/// `builtins.trace` prints on demand; an evaluation served from the memo
/// table has to say both again, or the reader is told less than they would
/// have been without `eval-cache-dir` -- the setting changing what the
/// evaluator says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvalResult {
    pub status: String,
    pub value: String,
    pub emissions: Vec<Emission>,
    /// Which kind of refusal `status` records, when it records one.
    ///
    /// A field beside the status rather than something encoded into it. The
    /// stored form is a canonical map keyed by name and `emissions` above
    /// already showed that a reader tolerates a key it does not find, so
    /// there is no reason to smuggle a second fact through the status string
    /// and parse it back out.
    pub token: Option<crate::refusal::RefusalToken>,
    /// Where the failure happened, when it happened somewhere.
    ///
    /// Part of the outcome for the same reason `token` is: the embedder
    /// renders `at /path/file.nix:LINE:COL` from it and prints the source
    /// line underneath, so a served answer that dropped it would say less on
    /// the second run than on the first. Absent from a row written before
    /// positions existed, which reads back as `None` -- an error with no
    /// position, which is what those runs printed.
    pub pos: Option<crate::vm::SrcPos>,
}

/// What a memoised result is filed under: the module, the evaluator that runs
/// it, every process setting that can change what it produces, and the
/// question that was asked of it.
///
/// A newtype rather than a bare [`Hash`] so a module digest cannot be passed
/// where an identity is wanted. That was the bug: the key was the module
/// alone, so one `eval-cache-dir` shared between two store directories served
/// the first store's `outPath` to the second, and an `outPath` that is wrong
/// in all 32 characters looks exactly like a right one (ENG-12541).
///
/// The question joined it for ENG-12830. While the key was `(module,
/// settings)` only one caller could use the table at all -- the one that
/// always asks the same question, "render the whole expression" -- so `nix
/// eval` and `nix build` wrote module objects into `eval-cache-dir` and read
/// nothing back for the life of the setting. Two callers can share a module
/// and want entirely different bytes out of it, and a key that cannot say
/// which would serve one of them the other's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EvalId {
    /// `H(module, evaluator, settings, arguments, question)`, which is what
    /// everything is filed under.
    key: Hash,
}

/// Domain separation for an evaluation identity. Bumped from `-v1` when the
/// question joined the key (ENG-12830), from `-v2` when the applied arguments
/// did (ENG-12915), and from `-v3` when the evaluator fingerprint joined. The
/// `-v4` bump closes the positions round 4 scenario: unchanged `root.nix` and
/// `merged.nix`, a fixed evaluator, and a stale cached `null` served without
/// executing the fixed import. Version 5 retires witnesses from before row
/// deduplication and ATerm CAS references. Rows filed under an older scheme
/// are retired rather than reinterpreted. Version 6 introduces bounded
/// witness history and versioned ownership metadata.
const EVAL_ID_TAG: &str = "ixe-eval-identity-v6";

impl EvalId {
    /// File an evaluation under its module, this evaluator, its settings,
    /// what the module was applied to, and the question asked of the result.
    ///
    /// All five identity axes are required. The evaluator fingerprint comes
    /// from [`crate::modcache::compiler_fingerprint`] here, so callers cannot
    /// omit or override it. Each prior bug came from a key that left one axis
    /// out, and every such key looks exactly like a working one from the
    /// outside: it hits when the omitted field happens to match and serves the
    /// wrong answer when it does not.
    ///
    /// # What a hit is claiming
    ///
    /// That the same evaluator ran the same module, applied to the same values,
    /// under the same process settings, asked the same question, and with every
    /// world read the last run performed still giving the answer it gave then.
    /// The first five are the module, compiler fingerprint and the remaining
    /// three parameters. The sixth is the witness replay in
    /// [`ResultCache::lookup`]. The replay is not a substitute for the key --
    /// a witness is filed under the identity, so two evaluations sharing an
    /// identity share a witness, and the second one replays the first one's
    /// questions and is served the first one's answer. That was live for the
    /// flake path until the argument axis existed (ENG-12915).
    #[must_use]
    pub fn of(
        module: &Hash,
        settings: &crate::eval::Settings,
        arguments: &crate::session::Arguments,
        question: &crate::session::Question,
    ) -> Self {
        Self::compose(
            module,
            crate::modcache::compiler_fingerprint(),
            settings,
            arguments,
            question,
        )
    }

    #[cfg(test)]
    fn with_evaluator_fingerprint_for_test(
        module: &Hash,
        evaluator_fingerprint: &str,
        settings: &crate::eval::Settings,
        arguments: &crate::session::Arguments,
        question: &crate::session::Question,
    ) -> Self {
        Self::compose(module, evaluator_fingerprint, settings, arguments, question)
    }

    fn compose(
        module: &Hash,
        evaluator_fingerprint: &str,
        settings: &crate::eval::Settings,
        arguments: &crate::session::Arguments,
        question: &crate::session::Question,
    ) -> Self {
        let settings = settings.fingerprint();
        let arguments = arguments.fingerprint();
        let question = question.fingerprint();
        Self {
            key: hash::tagged(
                EVAL_ID_TAG,
                &[
                    module.as_bytes(),
                    evaluator_fingerprint.as_bytes(),
                    settings.as_bytes(),
                    arguments.as_bytes(),
                    question.as_bytes(),
                ],
            ),
        }
    }

    #[must_use]
    pub fn as_hash(&self) -> &Hash {
        &self.key
    }
}

// ------------------------------------------------------------ witness store

/// Remembered question lists, on disk, so a cold process has something to
/// replay. Filed under an [`EvalId`], so a witness recorded under one set of
/// evaluator settings is not replayed under another.
///
/// Without this a new process holds results it can never look up: the key is
/// built from answers to last time's questions, and last time's questions died
/// with the process that asked them.
///
/// # This store needs no integrity guarantee, and that is not an oversight
///
/// Every other persisted thing here is checked. A witness is not, because a
/// wrong one cannot produce a wrong answer. The key is computed from the
/// answers observed when the questions are replayed, so a witness that names
/// the wrong questions yields a key nothing was stored under, which is a miss.
/// A witness that will not parse is also a miss, reported through the cache's
/// complaint channel so persistent corruption does not look like an empty
/// store. The worst a corrupted witness can do is waste work.
#[derive(Clone, Debug)]
pub struct DirWitness {
    root: PathBuf,
}

/// The store objects the embedder has called sealed ([`Host::sealed_paths`]):
/// one file per object under `root`, named `<hash>-<name>`, whose body is
/// the symlinks that leave the object, one relative path per line (empty
/// when none does).
///
/// Permanent by construction. Sealed is a property of the object's bytes,
/// and a content-addressed object's name pins its bytes -- the links
/// included -- so nothing can make a recorded object unsealed or change
/// which of its links leave; the record is never swept. Each entry carries
/// a digest of its body (an unkeyed integrity check against truncation and
/// stray files, not authentication): an entry is authorisation to replay
/// reads without asking, so a body that does not match is no entry.
/// Presence is not recorded here; it is asked every replay.
#[derive(Clone, Debug)]
pub struct DirSealed {
    records: DirRecords,
    /// Objects the host declined to call sealed (absent from the store, or
    /// input-addressed), so no later question through this handle or its
    /// clones asks about them again; the evaluator opens one handle per
    /// process (`store.rs`), so in practice once per process. In memory
    /// only: the record on disk holds what is permanent, and absence is not
    /// (a build can register the object). A decline is never an answer, it
    /// only skips the question, so a declined object is treated as unknown:
    /// not sealed, the safe direction, its reads asked of the host as before
    /// this memo existed. Left by process exit, or by [`DirSealed::insert`]
    /// when a later answer does call the object sealed (a record always
    /// outranks a decline, in both [`sealed_objects`] and [`seal_one`]).
    declined: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
}

/// A directory of small facts, one file per name, each a digest line then
/// its body. The digest is an unkeyed integrity check against truncation
/// and stray files, not authentication: an entry is authorisation to answer
/// without asking, so a body that does not match is no entry. Written whole
/// then renamed into place, so a reader never sees a half-written entry.
/// [`DirSealed`] and [`DirCopies`] are two namings of this one shape.
#[derive(Clone, Debug)]
struct DirRecords {
    root: PathBuf,
}

impl DirRecords {
    fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// The body recorded under `name`, or `None` when there is no entry or
    /// the entry does not carry a digest matching its body.
    fn get(&self, name: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.path(name)).ok()?;
        let (digest_hex, body) = text.split_once('\n')?;
        if digest(&[body.as_bytes()]).to_hex() != digest_hex {
            return None;
        }
        Some(body.to_owned())
    }

    fn insert(&self, name: &str, body: &str) -> std::io::Result<()> {
        let temp = self.root.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        let text = format!("{}\n{body}", digest(&[body.as_bytes()]).to_hex());
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, self.path(name))
    }

    /// Mark `name` used now, so a prune ordering by recency keeps it ahead
    /// of entries written later and never read. Metadata only. False when
    /// there is no such entry.
    fn touch(&self, name: &str) -> bool {
        std::fs::File::open(self.path(name))
            .and_then(|file| file.set_modified(std::time::SystemTime::now()))
            .is_ok()
    }

    /// Remove the least recently used entries until at most `keep` remain,
    /// and the temporaries of writes that never renamed (a writer that died
    /// mid-`insert`): a temporary older than [`STALE_TEMPORARY`] has no live
    /// writer, since a write is milliseconds. Nothing here is an answer, so a
    /// listing or removal that fails leaves the entry for the next prune.
    fn prune_to(&self, keep: usize) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        let now = std::time::SystemTime::now();
        let mut aged: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let Some(modified) = entry.metadata().ok().and_then(|meta| meta.modified().ok()) else {
                continue;
            };
            if entry.file_name().to_string_lossy().starts_with(".tmp-") {
                if now.duration_since(modified).unwrap_or_default() > STALE_TEMPORARY {
                    drop(std::fs::remove_file(entry.path()));
                }
                continue;
            }
            aged.push((modified, entry.path()));
        }
        if aged.len() <= keep {
            return;
        }
        aged.sort();
        for (_, path) in aged.iter().take(aged.len() - keep) {
            drop(std::fs::remove_file(path));
        }
    }
}

/// How old a `.tmp-*` file has to be before a prune calls its writer dead.
const STALE_TEMPORARY: std::time::Duration = std::time::Duration::from_secs(3600);

impl DirSealed {
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        Ok(Self {
            records: DirRecords::open(root)?,
            declined: std::sync::Arc::default(),
        })
    }

    /// Whether the host declined to call `object` sealed earlier in this
    /// process ([`DirSealed::decline`]).
    #[must_use]
    pub fn declined(&self, object: &str) -> bool {
        self.declined
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(object)
    }

    /// Remember that the host did not call `object` sealed, so this handle
    /// does not ask again. Measured before this (bed l2cb1): two
    /// input-addressed `cargo-vendor-dir` roots asked about 32 times, 0.48 s,
    /// every answer "skipped".
    pub fn decline(&self, object: &str) {
        self.declined
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(object.to_owned());
    }

    /// The symlinks leaving `object` (a base name, which is also a safe file
    /// name: a store path name has no `/`) if it has been recorded sealed;
    /// `None` if it has not, or if the record does not carry its own digest
    /// -- an entry is authorisation to replay reads without asking, so a
    /// truncated, foreign or empty file is no entry.
    #[must_use]
    pub fn get(&self, object: &str) -> Option<Vec<String>> {
        let body = self.records.get(object)?;
        Some(
            body.split('\n')
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }

    /// Record `object` as sealed with the symlinks that leave it: a digest
    /// line, then one link per line. Leaves any earlier decline of the
    /// object: sealed now outranks not-sealed then.
    pub fn insert(&self, object: &str, leaving: &[String]) -> std::io::Result<()> {
        let mut body = leaving.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        self.records.insert(object, &body)?;
        self.declined
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(object);
        Ok(())
    }
}

/// Entries the copy memo is pruned down to. Unlike `sealed`, a memo of
/// copies is not a few hundred permanent facts: every edit of a filtered
/// tree mints new keys (the accepted set names the tree's files), 76 per
/// evaluation on the home-manager config, so it needs a leaver. 8192
/// entries is some 100 edits of every filtered tree; each is one small file,
/// so the bytes are the filesystem's block size times 8192.
const COPY_MEMO_KEEP: usize = 8192;

/// Inserts between prunes within one handle: the count is a directory
/// listing, and a listing per insert would cost more than the memo saves on
/// a small copy. The directory can therefore hold up to `COPY_MEMO_KEEP +
/// COPY_MEMO_PRUNE_EVERY - 1` entries between prunes; a process that inserts
/// fewer than this many leaves the pruning to the next [`DirCopies::open`].
const COPY_MEMO_PRUNE_EVERY: u64 = 64;

/// The store path each filtered copy answered, by [`copy_key`]:
/// `<cache>/copies/<key hex>`, body = the store path. See [`CopyMemo`] for
/// when an entry may be served; this type only remembers. A hit touches its
/// entry, so the prune ([`DirRecords::prune_to`]) is by recency of use.
///
/// The leaver runs at [`DirCopies::open`] (once per handle, so a directory
/// left over the cap by a process that inserted little is pruned by the
/// next process to open it) and every [`COPY_MEMO_PRUNE_EVERY`] inserts
/// through the handle; the cadence is per handle, not per process, so two
/// directories open in one process each prune on their own inserts.
#[derive(Clone, Debug)]
pub struct DirCopies {
    records: DirRecords,
    keep: usize,
    inserts: Cell<u64>,
}

impl DirCopies {
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        Self::open_keeping(root, COPY_MEMO_KEEP)
    }

    /// [`DirCopies::open`] with the cap as a parameter, so a test can watch
    /// the leaver work without writing 8192 files.
    pub fn open_keeping(root: impl Into<PathBuf>, keep: usize) -> std::io::Result<Self> {
        let records = DirRecords::open(root)?;
        records.prune_to(keep);
        Ok(Self {
            records,
            keep,
            inserts: Cell::new(0),
        })
    }

    /// The store path remembered under `key`, if any. A remembered path is a
    /// candidate, not an answer: the caller asks the store whether it is
    /// still valid.
    #[must_use]
    pub fn get(&self, key: &Hash) -> Option<String> {
        let name = key.to_hex();
        let body = self.records.get(&name)?;
        let path = body.trim_end_matches('\n');
        if path.is_empty() || path.contains('\n') {
            return None;
        }
        self.records.touch(&name);
        Some(path.to_owned())
    }

    /// Remember `path` as the answer under `key`, and every
    /// [`COPY_MEMO_PRUNE_EVERY`] inserts through this handle bring the
    /// directory back under its cap.
    pub fn insert(&self, key: &Hash, path: &str) -> std::io::Result<()> {
        self.records.insert(&key.to_hex(), &format!("{path}\n"))?;
        let inserts = self.inserts.get() + 1;
        self.inserts.set(inserts);
        if inserts.is_multiple_of(COPY_MEMO_PRUNE_EVERY) {
            self.records.prune_to(self.keep);
        }
        Ok(())
    }
}

const COPY_MEMO_TAG: &str = "ixe-copy-memo-v1";

/// What one `StoreFiltered` question is keyed on in the copy memo: the
/// question's own key material ([`Question::key_material`]: root, path,
/// name, method, `sha256`, the accepted set with its file types, the
/// references flag) and the store directory. The store directory is not in
/// the question and is in the answer (ENG-12541: a memo key blind to it
/// served one store's paths to another), so it is folded in here.
fn copy_key(question: &Question, store_dir: Option<&str>) -> Hash {
    let mut parts = question.key_material();
    parts.push(store_dir.unwrap_or("").as_bytes().to_vec());
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    hash::tagged(COPY_MEMO_TAG, &refs)
}

/// The trace line for one `StoreFiltered` ask, whichever route answered
/// (`memo`: served from the copy memo; `walk`: the memo missed and the store
/// copied; `mutable`: the root can change under its spelling, no memo
/// consulted; `nomemo`: this recorder has no memo). The whole ask's cost
/// sits beside the memo's steps because `q.StoreFiltered_ns` is measured
/// around [`RecordingHost::store_filtered`] and the memo's steps alone
/// accounted for under half of it (l2ba1). `total_ns` ends before the line
/// is written, so the perf counter exceeds it by the write.
fn store_filtered_trace(
    route: &str,
    request: &crate::task::FilteredCopy,
    cost: &CopyCost,
    record_ns: u64,
    total_ns: u64,
    answer: &Result<String, StoreError>,
) -> String {
    format!(
        "ixe question: StoreFiltered served={route} root={} accepted={} method={:?} {cost} record_ns={record_ns} \
         total_ns={total_ns} -> {}",
        request.root.accessor_path(),
        request.accepted_label(),
        request.method,
        answer.as_deref().unwrap_or("error")
    )
}

/// What one [`CopyMemo::look`] spent, step by step, for the trace line
/// [`RecordingHost::store_filtered`] prints under `IXE_REPLAY_TRACE`. A
/// served copy on the edit arm measured ~3.7 ms against ~0.25 ms for
/// cppnix's own fingerprint-cache hit; the breakdown named the step
/// (2026-09-03, l2ba1: `valid_ns` p50 70 us and p99 34 ms, the wait for
/// the store's one daemon connection while a worker held it for
/// `buildPaths`; the pool is four connections since).
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyCost {
    pub immutable_ns: u64,
    pub key_ns: u64,
    pub get_ns: u64,
    pub valid_ns: u64,
    pub allow_ns: u64,
}

impl std::fmt::Display for CopyCost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "immutable_ns={} key_ns={} get_ns={} valid_ns={} allow_ns={}",
            self.immutable_ns, self.key_ns, self.get_ns, self.valid_ns, self.allow_ns
        )
    }
}

/// What [`CopyMemo::look`] found for one question.
#[derive(Debug)]
pub enum CopyLookup {
    /// The root's bytes can change under its spelling: neither served nor
    /// remembered, the store is asked as if there were no memo.
    Mutable,
    /// Nothing valid remembered under this key; the caller asks the store
    /// and hands the answer to [`CopyMemo::remember`] under it.
    Miss(Hash),
    /// The remembered store path, confirmed valid by the store just now.
    Served(String),
}

/// The per-question memo of filtered copies.
///
/// cppnix has no cache for a filtered copy: `fetchToStore` voids the
/// fingerprint when a filter is passed, because the filter is an opaque
/// callback, so every `builtins.path { filter = ...; }` on a NAR-addressed
/// source walks and NAR-hashes its tree on every evaluation the
/// whole-evaluation memo does not serve. (A source served from a jj object
/// store takes the tree-id road, `EvalState::addPathToStore`, which reads
/// no file; this memo still spares it the filter walk.)
/// Measured on the home-manager config (2026-09-03, lm-f6a5-edit): 76
/// filtered copies, 5.6 s of the 33 s edit case, two of them the 11k-file
/// ix tree at 2.8 s and 1.4 s; the 968 unfiltered copies in the same run
/// cost 0.25 s, because those cppnix does cache by fingerprint.
///
/// In this evaluator the filter is not opaque: the question carries the
/// accepted set as data, so the copy is a function of its question whenever
/// the root's bytes cannot change under its spelling. That is exactly the
/// test the whole-evaluation replay applies before serving a kept copy
/// answer ([`immutable_root`]: a mounted root, or an ambient path inside a
/// sealed store object away from its leaving links), and it is the same
/// function here, fed by the same `sealed` record ([`DirSealed`]) and the
/// same [`Host::sealed_paths`] question for an object not yet recorded.
///
/// A hit is served only after the store confirms the remembered path is
/// valid ([`Host::valid_paths`], one round trip against a walk), so a swept
/// object is a miss and never a wrong answer. What the key does not carry is
/// the embedder's version: a filtered copy's store path is the store's own
/// content address of (bytes, name, method, references), which is the
/// store's compatibility contract and not this evaluator's, and the
/// whole-evaluation memo and cppnix's `sourcePathToHash` cache are exactly
/// as version-blind. The memo sits below the
/// recorder ([`RecordingHost::store_filtered`]): the question is recorded
/// with its answer whether the memo or the store produced it, so a served
/// run's read set is a walked run's read set, and the hazard that keeps
/// [`crate::eval::JobMemo`] inside one recording (a hit served across
/// recordings would record nothing) does not arise.
#[derive(Clone, Debug)]
pub struct CopyMemo {
    copies: DirCopies,
    sealed: DirSealed,
    store_dir: Option<String>,
}

impl CopyMemo {
    #[must_use]
    pub fn new(copies: DirCopies, sealed: DirSealed, store_dir: Option<String>) -> Self {
        Self {
            copies,
            sealed,
            store_dir,
        }
    }

    /// Consult the memo for `question`, which must be a
    /// [`Question::StoreFiltered`]; any other question is [`CopyLookup::Mutable`],
    /// because nothing else is remembered here. The [`CopyCost`] beside the
    /// verdict is what each step took.
    pub fn look(&self, question: &Question, host: &dyn Host) -> (CopyLookup, CopyCost) {
        let mut cost = CopyCost::default();
        let Question::StoreFiltered(request) = question else {
            return (CopyLookup::Mutable, cost);
        };
        let (immutable, ns) = crate::perf::timed(|| self.immutable(&request.root, host));
        cost.immutable_ns = ns;
        if !immutable {
            return (CopyLookup::Mutable, cost);
        }
        let (key, ns) = crate::perf::timed(|| copy_key(question, self.store_dir.as_deref()));
        cost.key_ns = ns;
        let (candidate, ns) = crate::perf::timed(|| self.copies.get(&key));
        cost.get_ns = ns;
        let Some(candidate) = candidate else {
            return (CopyLookup::Miss(key), cost);
        };
        let (valid, ns) = crate::perf::timed(|| host.valid_paths(std::slice::from_ref(&candidate)));
        cost.valid_ns = ns;
        match valid {
            Ok(present) if present.contains(&candidate) => {
                // The live copy's hook ends in `allowPath` (cppnix's
                // `allowAndSetStorePathString`), so the reads that follow
                // it go through the object it named. A served copy must
                // leave the same allow list behind it or the next read under
                // the copied object is refused in pure evaluation mode
                // (measured: the first edit-arm run under this memo failed
                // at `importTOML` on a served source's `Cargo.lock`). The
                // whole-evaluation replay does the same for the rows it
                // serves by validity (`Question::validity`).
                let (allowed, ns) =
                    crate::perf::timed(|| host.allow_paths(std::slice::from_ref(&candidate)));
                cost.allow_ns = ns;
                if allowed.is_err() {
                    return (CopyLookup::Miss(key), cost);
                }
                crate::perf::note_copy_memo_served();
                (CopyLookup::Served(candidate), cost)
            }
            // Swept, or the store would not say: the walk happens and its
            // answer replaces the entry. A failed validity question is not
            // an evaluation failure, since the store is about to be asked
            // for the copy itself and will fail that if it is really down.
            _ => (CopyLookup::Miss(key), cost),
        }
    }

    /// Remember `path` as the answer under `key`. A fact that could not be
    /// written is a fact asked again next time; it must not fail the copy
    /// that learned it.
    pub fn remember(&self, key: &Hash, path: &str) {
        drop(self.copies.insert(key, path));
    }

    /// [`immutable_root`] for one path, with the sealed record it needs
    /// obtained the way [`sealed_objects`] obtains it: from `sealed` when the
    /// object is already recorded, else by asking the host once and
    /// recording what it says.
    fn immutable(&self, root: &crate::value2::PathValue, host: &dyn Host) -> bool {
        let store_dir = self.store_dir.as_deref();
        let mut sealed = crate::host::Sealed::new();
        if matches!(root.root, crate::value2::Root::Ambient) {
            let Some(object) = store_object_of(store_dir, &root.path) else {
                return false;
            };
            let leaving = match self.sealed.get(&object) {
                Some(leaving) => leaving,
                None => match seal_one(&object, host, &self.sealed) {
                    Some(leaving) => leaving,
                    None => return false,
                },
            };
            sealed.insert(object, leaving);
        }
        immutable_root(root, &sealed, store_dir)
    }
}

/// One object's leaving links, asked of the host and recorded in `memo` when
/// it calls the object sealed; `None` when it does not (absent from the
/// store, or not content-addressed: declined for the handle, so the next
/// question about the object asks nothing) or the question failed. The
/// one-object form of the batch in [`sealed_objects`], charged to the same
/// counters; the caller ([`CopyMemo::immutable`]) has consulted the record
/// first, so a record always outranks a decline here too.
fn seal_one(object: &str, host: &dyn Host, memo: &DirSealed) -> Option<Vec<String>> {
    if memo.declined(object) {
        return None;
    }
    let asked = vec![object.to_owned()];
    let (answer, nanos) = crate::perf::timed(|| host.sealed_paths(&asked));
    crate::perf::note_sealing(1, nanos);
    let mut answered = answer
        .inspect_err(|_| crate::perf::note_sealing_failed())
        .ok()?;
    let Some(links) = answered.remove(object) else {
        memo.decline(object);
        return None;
    };
    let leaving = leaving_links(&links);
    drop(memo.insert(object, &leaving));
    Some(leaving)
}

/// The result of reading one persistent evaluation witness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WitnessLookup {
    Missing,
    Found(Vec<WitnessRow>),
    /// A file exists but cannot be used by this evaluator.
    Refused(String),
}

/// Slot zero keeps the primary name; older recipes have domain-separated names.
fn witness_slot(identity: &EvalId, slot: usize) -> EvalId {
    if slot == 0 {
        *identity
    } else {
        EvalId {
            key: hash::tagged(
                "ixe-witness-slot-v1",
                &[identity.as_hash().as_bytes(), &(slot as u64).to_le_bytes()],
            ),
        }
    }
}

/// Include every replay-affecting record, and frame each row independently.
/// The fixed-width row hashes prevent ambiguity between variable-arity questions.
fn witness_shape(rows: &[WitnessRow]) -> Hash {
    let mut shape = hash::TaggedHasher::new("ixe-witness-shape-v1");
    for (question, recorded) in rows {
        let mut row = hash::TaggedHasher::new("ixe-witness-shape-row-v1");
        for part in question.key_material() {
            row.field(&part);
        }
        // The result-key question identity omits this retained answer, but
        // replay uses it to validate writes and license realisation.
        if let Question::WriteDrv { answer, .. } = question {
            match answer {
                WriteDrvAnswer::Written(path) => {
                    row.field(b"written");
                    row.field(path.as_bytes());
                }
                WriteDrvAnswer::Failed { outcome, detail } => {
                    row.field(b"failed");
                    row.field(outcome.as_bytes());
                    row.field(detail.as_bytes());
                }
            }
        }
        match recorded {
            Recorded::Answer(text) => {
                row.field(b"answer");
                row.field(text.as_bytes());
            }
            Recorded::Digest(digest) => {
                row.field(b"digest");
                row.field(digest.as_bytes());
            }
        }
        shape.field(row.finish().as_bytes());
    }
    shape.finish()
}

fn witness_metadata(text: &str) -> Option<(&str, Vec<String>)> {
    let mut lines = text.lines();
    if lines.next()? != WITNESS_META {
        return None;
    }
    let shape = lines.next()?;
    let valid_hash = |text: &str| {
        text.len() == 64
            && text
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    };
    if !valid_hash(shape) {
        return None;
    }
    let refs = lines
        .map(|line| valid_hash(line).then(|| line.to_owned()))
        .collect::<Option<Vec<_>>>()?;
    Some((shape, refs))
}

impl DirWitness {
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub(crate) fn path(&self, identity: &EvalId) -> PathBuf {
        self.root.join(identity.as_hash().to_hex())
    }

    /// The sidecar beside the witness under `identity`: the object names its
    /// rows refer to, one per line, so a sweep can know what the witness
    /// keeps alive without reading it.
    pub(crate) fn refs_path(&self, identity: &EvalId) -> PathBuf {
        self.root
            .join(format!("{}{REFS_SUFFIX}", identity.as_hash().to_hex()))
    }

    /// Mark the witness under `identity` used now, so a sweep ordering by
    /// recency of use keeps it ahead of witnesses written later and never
    /// read. Metadata only, like [`ix_kernel::DirRows::touch`]. False when
    /// there is no such witness.
    pub fn touch(&self, identity: &EvalId) -> bool {
        std::fs::File::open(self.path(identity))
            .and_then(|file| file.set_modified(std::time::SystemTime::now()))
            .is_ok()
    }

    /// The questions recorded under an identity.
    ///
    /// Missing files are ordinary cold-cache misses. Existing files that
    /// cannot be read or decoded are refused separately so the caller can
    /// report a store that has become unusable instead of calling it empty.
    #[must_use]
    pub fn get(&self, identity: &EvalId) -> WitnessLookup {
        match self.get_with_shape(identity) {
            Ok(None) => WitnessLookup::Missing,
            Ok(Some((rows, _))) => WitnessLookup::Found(rows),
            Err(reason) => WitnessLookup::Refused(reason),
        }
    }

    fn get_with_shape(&self, identity: &EvalId) -> Result<Option<(Vec<WitnessRow>, Hash)>, String> {
        let id = identity.as_hash().to_hex();
        let bytes = match std::fs::read(self.path(identity)) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(format!(
                    "evaluation witness {id} could not be read: {source}"
                ));
            }
        };
        if let Some(rows) = witness_rows(&bytes) {
            let shape = witness_shape(&rows);
            if std::fs::read_to_string(self.refs_path(identity))
                .ok()
                .and_then(|text| {
                    witness_metadata(&text).map(|(recorded_shape, refs)| {
                        recorded_shape == shape.to_hex()
                            && refs == refs_of(&rows).into_iter().collect::<Vec<_>>()
                    })
                })
                == Some(true)
            {
                return Ok(Some((rows, shape)));
            }
        }
        Err(format!(
            "evaluation witness {id} is malformed or has an unsupported format"
        ))
    }

    /// Remove the witness under an identity and its sidecar. A missing
    /// witness is `Ok(false)`: the caller wants there to be none, and there
    /// is none.
    pub fn remove(&self, identity: &EvalId) -> std::io::Result<bool> {
        let removed = match std::fs::remove_file(self.path(identity)) {
            Ok(()) => true,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => false,
            Err(source) => return Err(source),
        };
        match std::fs::remove_file(self.refs_path(identity)) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(source),
        }
        Ok(removed)
    }

    /// Write a witness and, beside it, the sidecar naming the CAS objects its
    /// rows refer to (the ATerms of its `WriteDrv` rows).
    ///
    /// # Why the sidecar
    ///
    /// `Store::sweep` must know which objects a witness keeps alive, and it
    /// must not read the witness to find out: a home-target witness is
    /// hundreds of megabytes, and a sweep that parsed every one read the
    /// whole cache before deleting anything. So the writer, which has the
    /// rows in hand, writes the names out separately, and the sweep reads
    /// that. An existing body is invalidated before replacing its sidecar;
    /// the new body is published last. A writer interrupted between these
    /// steps leaves no live body with the wrong ownership metadata.
    /// Callers publishing to a shared Store must hold its publication lock.
    pub fn put(&self, identity: &EvalId, rows: &[WitnessRow]) -> std::io::Result<usize> {
        self.put_with_shape(identity, rows, witness_shape(rows))
    }

    fn put_with_shape(
        &self,
        identity: &EvalId,
        rows: &[WitnessRow],
        shape: Hash,
    ) -> std::io::Result<usize> {
        self.put_with_shape_hook(identity, rows, shape, |_| Ok(()))
    }

    fn put_with_shape_hook(
        &self,
        identity: &EvalId,
        rows: &[WitnessRow],
        shape: Hash,
        mut after_step: impl FnMut(u8) -> std::io::Result<()>,
    ) -> std::io::Result<usize> {
        let mut refs = refs_of(rows).into_iter().fold(
            format!("{WITNESS_META}\n{}\n", shape.to_hex()),
            |mut text, name| {
                text.push_str(&name);
                text.push('\n');
                text
            },
        );
        refs.shrink_to_fit();
        // A killed writer must leave an absent body, never an old body beside
        // a new ownership list. The publication lock also excludes GC here.
        remove_witness_file(&self.path(identity))?;
        after_step(0)?;
        self.rename_into_place(refs.as_bytes(), self.refs_path(identity))?;
        after_step(1)?;

        let value = CanonValue::map([
            ("format", CanonValue::str(WITNESS_FORMAT)),
            (
                "rows",
                CanonValue::Array(rows.iter().map(row_value).collect()),
            ),
        ]);
        let bytes = canon::encode(&value).map_err(std::io::Error::other)?;
        self.rename_into_place(&bytes, self.path(identity))?;
        after_step(2)?;
        Ok(bytes.len())
    }

    fn has_shape(&self, identity: &EvalId, shape: Hash) -> bool {
        std::fs::read_to_string(self.refs_path(identity))
            .ok()
            .and_then(|text| witness_metadata(&text).map(|(old, _)| old == shape.to_hex()))
            == Some(true)
    }

    /// Publish newest first under the store publication lock. Only small
    /// metadata is read from historical entries; their bodies are renamed.
    fn put_history(
        &self,
        identity: &EvalId,
        rows: &[WitnessRow],
        shape: Hash,
    ) -> std::io::Result<usize> {
        let duplicate = (0..WITNESS_HISTORY).find(|slot| {
            let id = witness_slot(identity, *slot);
            self.path(&id).is_file() && self.has_shape(&id, shape)
        });
        let end = duplicate.unwrap_or(WITNESS_HISTORY - 1);
        for slot in (1..=end).rev() {
            self.move_slot(
                &witness_slot(identity, slot - 1),
                &witness_slot(identity, slot),
            )?;
        }
        self.put_with_shape(identity, rows, shape)
    }

    /// Invalidate both live body names before moving their ownership metadata.
    /// At every interruption point GC sees either a complete pair or an absent
    /// body. A temporary body is deliberately unowned and safely discardable.
    fn move_slot(&self, from: &EvalId, to: &EvalId) -> std::io::Result<()> {
        self.move_slot_with_hook(from, to, |_| Ok(()))
    }

    fn move_slot_with_hook(
        &self,
        from: &EvalId,
        to: &EvalId,
        mut after_step: impl FnMut(u8) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.remove(to)?;
        after_step(0)?;
        let temporary = self.root.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        match std::fs::rename(self.path(from), &temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        after_step(1)?;
        match std::fs::rename(self.refs_path(from), self.refs_path(to)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                remove_witness_file(&temporary)?;
                return Ok(());
            }
            Err(error) => {
                drop(std::fs::remove_file(&temporary));
                return Err(error);
            }
        }
        let moved = after_step(2)
            .and_then(|()| std::fs::rename(&temporary, self.path(to)))
            .and_then(|()| after_step(3));
        if moved.is_err() {
            drop(std::fs::remove_file(&temporary));
        }
        moved
    }

    /// Write `bytes` whole, then rename onto `target`, so a reader never
    /// sees a partial file. A write or rename that fails takes its temporary
    /// with it; one this cannot remove (the process dying mid-write) is a
    /// leftover the next sweep reclaims.
    fn rename_into_place(&self, bytes: &[u8], target: PathBuf) -> std::io::Result<()> {
        let temp = self.root.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        let written = std::fs::write(&temp, bytes).and_then(|()| std::fs::rename(&temp, target));
        if written.is_err() {
            drop(std::fs::remove_file(&temp));
        }
        written
    }
}

/// The object names a witness's rows refer to: the ATerm behind every
/// `WriteDrv` row, each once.
fn refs_of(rows: &[WitnessRow]) -> std::collections::BTreeSet<String> {
    rows.iter()
        .filter_map(|(question, _)| match question {
            Question::WriteDrv { aterm, .. } => Some(aterm.hash().to_hex()),
            _ => None,
        })
        .collect()
}

/// The object names in a sidecar, or `None` if any line is not one: a
/// sidecar is authorisation to keep objects and to call a witness live, so
/// a line that is not an object name means the file is not a sidecar this
/// build wrote.
#[must_use]
pub(crate) fn witness_refs(text: &str) -> Option<Vec<String>> {
    witness_metadata(text).map(|(_, refs)| refs)
}

fn remove_witness_file(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// The rows in a versioned witness, or `None` if it does not parse. Both are
/// misses and neither is an error.
///
/// The format check happens before any question tag is interpreted. In
/// particular, a pre-ATerm witness's tag 10 derivation row cannot be replayed
/// as the current tag 10 `StoreText` question.
#[must_use]
pub fn witness_rows(bytes: &[u8]) -> Option<Vec<WitnessRow>> {
    let CanonValue::Map(entries) = canon::decode(bytes).ok()? else {
        return None;
    };
    entries.iter().find_map(|(k, v)| match (k, v) {
        (CanonValue::Str(k), CanonValue::Str(format))
            if k == "format" && format == WITNESS_FORMAT =>
        {
            Some(())
        }
        _ => None,
    })?;
    let items = entries.iter().find_map(|(k, v)| match (k, v) {
        (CanonValue::Str(k), CanonValue::Array(items)) if k == "rows" => Some(items),
        _ => None,
    })?;
    items.iter().map(row_from).collect()
}

/// One witness row: the question's array beside its record, the 32 digest
/// bytes or the kept answer text. Two CBOR types, so a reader cannot take
/// one for the other.
fn row_value((question, recorded): &WitnessRow) -> CanonValue {
    let recorded = match recorded {
        Recorded::Digest(digest) => CanonValue::Bytes(digest.as_bytes().to_vec()),
        Recorded::Answer(text) => CanonValue::str(text),
    };
    CanonValue::array(vec![question_value(question), recorded])
}

/// The inverse of [`row_value`]. `None` for anything but a two-element
/// array of a decodable question and either exactly 32 bytes or a text that
/// question can replay by validity ([`Recorded::answer`]): a witness is
/// bytes off disk, and a kept answer on a question that cannot use one would
/// key with a digest no recording produced.
fn row_from(value: &CanonValue) -> Option<WitnessRow> {
    let CanonValue::Array(parts) = value else {
        return None;
    };
    let [question, recorded] = parts.as_slice() else {
        return None;
    };
    let question = question_from(question)?;
    let recorded = match recorded {
        CanonValue::Bytes(digest) => {
            Recorded::Digest(Hash::from_bytes(digest.as_slice().try_into().ok()?))
        }
        CanonValue::Str(text) => Recorded::answer(&question, text.clone())?,
        _ => return None,
    };
    Some((question, recorded))
}

static WITNESS_COUNTER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn question_value(question: &Question) -> CanonValue {
    let mut parts = vec![
        CanonValue::int(question.tag()),
        CanonValue::str(question.arg()),
    ];
    match question {
        Question::ReadFile(path)
        | Question::Import(path)
        | Question::ReadFileBytes(path)
        | Question::ReadDir(path)
        | Question::PathExists(path)
        | Question::DirExists(path)
        | Question::FileType(path)
        | Question::FileTypeResolved(path)
        | Question::CopyToStore(path)
        | Question::StorePath(path)
        | Question::Tree(path) => {
            parts.push(CanonValue::str(path.root.wire_name()));
        }
        Question::StoreFiltered(request) => {
            parts.push(CanonValue::str(request.root.root.wire_name()));
        }
        _ => {}
    }
    // A search path lookup needs its entries to be replayable at all: the
    // name alone would be replayed against a different list and answered
    // differently, and the point of a witness is that replaying it asks what
    // the evaluation asked.
    if let Question::FindFile { entries, .. } = question {
        for e in entries {
            parts.push(CanonValue::str(&e.prefix));
            parts.push(CanonValue::str(&e.path));
        }
    }
    // Contents first, then the references, so the decoder can take the head
    // and treat the rest uniformly.
    if let Question::StoreText {
        contents,
        references,
        ..
    } = question
    {
        parts.push(CanonValue::str(contents));
        for r in references {
            parts.push(CanonValue::str(r));
        }
    }
    if let Question::WriteDrv { answer, aterm, .. } = question {
        parts.push(CanonValue::str(answer.render()));
        parts.push(CanonValue::Bytes(aterm.hash().as_bytes().to_vec()));
    }
    // A flat tail of rendered elements. Flat rather than nested because there
    // is exactly one variable-length field and it is last, so the decoder can
    // take everything after the argument without counting.
    if let Question::Realise(context) = question {
        for e in context {
            parts.push(CanonValue::str(e.display()));
        }
    }
    // Three nested arrays rather than a flat tail, because a flat one cannot
    // say where the accepted list starts without the decoder counting
    // fields -- and a filtered copy has a variable number of them.
    // Triples in a nested array, so the decoder does not have to count
    // fields to find where they start.
    if let Question::FetchTree(r) = question {
        let mut items = vec![CanonValue::str(r.fetcher.as_str())];
        for (name, value) in &r.attrs {
            items.push(CanonValue::str(name));
            items.push(CanonValue::str(value.tag()));
            items.push(CanonValue::str(value.text()));
        }
        parts.push(CanonValue::array(items));
    }
    // Triples in a nested array, exactly as the tree fetch above minus its
    // fetcher field, and for the same reason.
    if let Question::FlakeRefToString(attrs) = question {
        let mut items = Vec::new();
        for (name, value) in attrs {
            items.push(CanonValue::str(name));
            items.push(CanonValue::str(value.tag()));
            items.push(CanonValue::str(value.text()));
        }
        parts.push(CanonValue::array(items));
    }
    // One nested array rather than three flat fields, for the reason the
    // filtered copy below uses nested arrays: an optional field in a flat
    // tail cannot be told from a missing one.
    if let Question::Fetch(r) = question {
        parts.push(CanonValue::array(vec![
            CanonValue::str(&r.name),
            CanonValue::str(r.kind.as_str()),
        ]));
        parts.push(CanonValue::array(match &r.expected_sha256 {
            Some(h) => vec![CanonValue::str(h)],
            None => Vec::new(),
        }));
    }
    if let Question::StoreFiltered(r) = question {
        let mut head = vec![CanonValue::str(&r.name), CanonValue::str(r.method.as_str())];
        // Appended only when set, for the reason `key_parts` omits it: a
        // witness written before this field existed has a two-element head,
        // and the decoder below reads its absence as false.
        if r.inherit_references {
            head.push(CanonValue::str("inherit-references"));
        }
        parts.push(CanonValue::array(head));
        parts.push(CanonValue::array(match &r.expected_sha256 {
            Some(h) => vec![CanonValue::str(h)],
            None => Vec::new(),
        }));
        parts.push(CanonValue::array(match &r.accepted {
            None => vec![CanonValue::str("unfiltered")],
            Some(list) => {
                let mut items = vec![CanonValue::str("filtered")];
                for e in list {
                    items.push(CanonValue::str(&e.path));
                    items.push(CanonValue::str(e.file_type.as_str()));
                }
                items
            }
        }));
    }
    CanonValue::array(parts)
}

/// The inverse of [`question_value`].
///
/// The tag-to-variant table is hand-written and guarded by
/// `every_question_variant_round_trips_through_the_witness_codec`. `None` is a
/// miss: a witness written by a build that knew a question this one does not
/// is not something to guess at.
fn question_from(value: &CanonValue) -> Option<Question> {
    let CanonValue::Array(parts) = value else {
        return None;
    };
    let (CanonValue::Int(tag), CanonValue::Str(argument)) = (parts.first()?, parts.get(1)?) else {
        return None;
    };
    let arg = argument.clone();
    let rooted = || {
        let CanonValue::Str(root) = parts.get(2)? else {
            return None;
        };
        crate::value2::PathValue::from_wire(root, &arg)
            .ok()
            .map(Rc::new)
    };
    match tag {
        1 => Some(Question::ReadFile(rooted()?)),
        2 => Some(Question::ReadDir(rooted()?)),
        3 => Some(Question::PathExists(rooted()?)),
        4 => Some(Question::FileType(rooted()?)),
        5 => Some(Question::GetEnv(arg)),
        // 6 was missing until ENG-12443 went through this function, which
        // cost nothing visible and cost every witness containing a store copy
        // a silent parse failure, hence a miss it looked like a cold cache
        // for.
        6 => Some(Question::CopyToStore(rooted()?)),
        7 => {
            // Pairs, so an odd tail is a corrupt witness rather than an entry
            // with an empty path invented to fill the gap.
            let rest = parts.get(2..).unwrap_or_default();
            if rest.len() % 2 != 0 {
                return None;
            }
            let mut entries = Vec::with_capacity(rest.len() / 2);
            for pair in rest.chunks_exact(2) {
                let (CanonValue::Str(prefix), CanonValue::Str(path)) =
                    (pair.first()?, pair.get(1)?)
                else {
                    return None;
                };
                entries.push(SearchPathEntry {
                    prefix: prefix.clone(),
                    path: path.clone(),
                });
            }
            Some(Question::FindFile { entries, name: arg })
        }
        8 => Some(Question::NixPath),
        9 => Some(Question::EnsurePath(arg)),
        17 => Some(Question::ReadFileBytes(rooted()?)),
        10 => {
            let rest = parts.get(2..).unwrap_or_default();
            let (CanonValue::Str(contents), refs) = (rest.first()?, rest.get(1..)?) else {
                return None;
            };
            let mut references = Vec::with_capacity(refs.len());
            for r in refs {
                let CanonValue::Str(r) = r else {
                    return None;
                };
                references.push(r.clone());
            }
            Some(Question::StoreText {
                name: arg,
                contents: contents.clone(),
                references,
            })
        }
        11 => question_filtered(arg, parts.get(2..).unwrap_or_default()),
        12 => question_fetch(arg, parts.get(2..).unwrap_or_default()),
        13 => question_fetch_tree(parts.get(2..).unwrap_or_default()),
        14 => Some(Question::FileTypeResolved(rooted()?)),
        15 => Some(Question::LockFlake(arg)),
        18 => Some(Question::ParseFlakeRef(arg)),
        19 => question_flake_ref_to_string(parts.get(2..).unwrap_or_default()),
        20 => Some(Question::Import(rooted()?)),
        21 => Some(Question::StorePath(rooted()?)),
        22 => Some(Question::DirExists(rooted()?)),
        24 => Some(Question::Tree(rooted()?)),
        23 => {
            let rest = parts.get(2..).unwrap_or_default();
            let (CanonValue::Str(drv_path), CanonValue::Bytes(raw)) = (rest.first()?, rest.get(1)?)
            else {
                return None;
            };
            // Exactly the answer and the ATerm: a row with more (a version-2
            // witness carried the references here) is not this format.
            if rest.len() != 2 {
                return None;
            }
            let aterm = ObjId::from_hash(Hash::from_bytes(raw.as_slice().try_into().ok()?));
            Some(Question::WriteDrv {
                name: arg,
                answer: WriteDrvAnswer::parse(drv_path)?,
                aterm,
            })
        }
        16 => {
            let mut context = Vec::new();
            for item in parts.get(2..).unwrap_or_default() {
                let CanonValue::Str(rendered) = item else {
                    return None;
                };
                // `None` rather than a guess: an element this build cannot
                // parse would replay as a *different* question, which is the
                // one outcome worse than a miss.
                context.push(crate::value2::ContextElem::parse(rendered)?);
            }
            Some(Question::Realise(context))
        }
        _ => None,
    }
}

/// The tail of a tag-13 witness entry: one array of a fetcher name and then
/// (name, tag, value) triples. `None` for anything malformed, for the reason
/// [`question_filtered`] gives.
fn question_fetch_tree(rest: &[CanonValue]) -> Option<Question> {
    let CanonValue::Array(items) = rest.first()? else {
        return None;
    };
    let CanonValue::Str(fetcher) = items.first()? else {
        return None;
    };
    let tail = items.get(1..)?;
    if tail.len() % 3 != 0 {
        return None;
    }
    let mut attrs = std::collections::BTreeMap::new();
    for triple in tail.chunks_exact(3) {
        let (CanonValue::Str(name), CanonValue::Str(tag), CanonValue::Str(text)) =
            (triple.first()?, triple.get(1)?, triple.get(2)?)
        else {
            return None;
        };
        attrs.insert(name.clone(), crate::task::TreeAttr::parse(tag, text)?);
    }
    Some(Question::FetchTree(Box::new(
        crate::task::FetchTreeRequest {
            attrs,
            fetcher: crate::task::TreeFetcher::parse(fetcher)?,
        },
    )))
}

/// The tail of a tag-19 witness entry: one array of (name, tag, value)
/// triples, [`question_fetch_tree`]'s tail without its fetcher head. `None`
/// for anything malformed, for the reason [`question_filtered`] gives.
fn question_flake_ref_to_string(rest: &[CanonValue]) -> Option<Question> {
    let CanonValue::Array(items) = rest.first()? else {
        return None;
    };
    if items.len() % 3 != 0 {
        return None;
    }
    let mut attrs = std::collections::BTreeMap::new();
    for triple in items.chunks_exact(3) {
        let (CanonValue::Str(name), CanonValue::Str(tag), CanonValue::Str(text)) =
            (triple.first()?, triple.get(1)?, triple.get(2)?)
        else {
            return None;
        };
        attrs.insert(name.clone(), crate::task::TreeAttr::parse(tag, text)?);
    }
    Some(Question::FlakeRefToString(attrs))
}

/// The tail of a tag-12 witness entry: `[name, kind]` and `[sha256?]`.
/// `None` for anything malformed, for the reason [`question_filtered`] gives.
fn question_fetch(url: String, rest: &[CanonValue]) -> Option<Question> {
    let (CanonValue::Array(head), CanonValue::Array(sha)) = (rest.first()?, rest.get(1)?) else {
        return None;
    };
    let (CanonValue::Str(name), CanonValue::Str(kind)) = (head.first()?, head.get(1)?) else {
        return None;
    };
    let expected_sha256 = match sha.first() {
        None => None,
        Some(CanonValue::Str(h)) => Some(h.clone()),
        Some(_) => return None,
    };
    Some(Question::Fetch(Box::new(crate::task::FetchRequest {
        url,
        name: name.clone(),
        kind: crate::task::FetchKind::parse(kind)?,
        expected_sha256,
    })))
}

/// The tail of a tag-11 witness entry: `[name, method]`, `[sha256?]` and
/// `[marker, (path, type)...]`. `None` for anything malformed, which is a
/// miss rather than a guess -- the alternative is replaying a *different*
/// filtered copy and calling its answer this one's.
fn question_filtered(path: String, rest: &[CanonValue]) -> Option<Question> {
    let CanonValue::Str(root) = rest.first()? else {
        return None;
    };
    let rooted = Rc::new(crate::value2::PathValue::from_wire(root, &path).ok()?);
    let rest = rest.get(1..)?;
    let (CanonValue::Array(head), CanonValue::Array(sha), CanonValue::Array(list)) =
        (rest.first()?, rest.get(1)?, rest.get(2)?)
    else {
        return None;
    };
    let (CanonValue::Str(name), CanonValue::Str(method)) = (head.first()?, head.get(1)?) else {
        return None;
    };
    let inherit_references = match head.get(2) {
        None => false,
        Some(CanonValue::Str(m)) if m == "inherit-references" => true,
        // A spelling this build does not know is a witness from a different
        // build. `None` makes that a miss, where guessing false would replay
        // a copy that inherited references as one that did not.
        Some(_) => return None,
    };
    let expected_sha256 = match sha.first() {
        None => None,
        Some(CanonValue::Str(h)) => Some(h.clone()),
        Some(_) => return None,
    };
    let CanonValue::Str(marker) = list.first()? else {
        return None;
    };
    let accepted = match marker.as_str() {
        "unfiltered" => None,
        "filtered" => {
            let tail = list.get(1..)?;
            if tail.len() % 2 != 0 {
                return None;
            }
            let mut out = Vec::with_capacity(tail.len() / 2);
            for pair in tail.chunks_exact(2) {
                let (CanonValue::Str(path), CanonValue::Str(kind)) = (pair.first()?, pair.get(1)?)
                else {
                    return None;
                };
                out.push(crate::task::AcceptedPath {
                    path: path.clone(),
                    file_type: file_type_from(kind)?,
                });
            }
            Some(out)
        }
        _ => return None,
    };
    Some(Question::StoreFiltered(Box::new(
        crate::task::FilteredCopy {
            root: rooted,
            name: name.clone(),
            method: crate::task::PathMethod::parse(method)?,
            accepted,
            expected_sha256,
            inherit_references,
        },
    )))
}

/// The inverse of [`FileType::as_str`]. `None` rather than `Unknown` for an
/// unrecognised spelling: a witness naming a type this build does not know is
/// a witness from a different build, and decoding it as `Unknown` would key a
/// cache row on a type nobody recorded.
fn file_type_from(name: &str) -> Option<FileType> {
    match name {
        "regular" => Some(FileType::Regular),
        "directory" => Some(FileType::Directory),
        "symlink" => Some(FileType::Symlink),
        "unknown" => Some(FileType::Unknown),
        _ => None,
    }
}

/// A resolved search path, digested. As with a store copy, the answer's
/// *class* is part of the digest: "no resolver" and "not found" are different
/// facts, and a replay that turned one into the other would hit on a cache
/// entry built under the other.
/// The digest of one [`Host::file_type`] answer.
///
/// Written once and called from both sides -- [`RecordingHost::file_type`]
/// records with it and [`Question::digest`] re-asks with it -- because two
/// spellings of one key is two chances for a recorded witness to stop
/// matching the replay that checks it, silently and in the direction of
/// "everything is still valid".
///
/// Three outcomes, three tags. `kind-absent` is its own tag rather than a
/// `kind-err` carrying "does not exist" text, because a read set that cannot
/// tell "the accessor has no such path" from "the read was refused" would
/// replay one as the other, and the two differ in exactly the case ENG-13123
/// was: absence is an ordinary answer that evaluation continues past.
fn digest_file_type(answer: &Result<Option<FileType>, String>) -> Hash {
    match answer {
        Ok(Some(kind)) => digest(&[b"kind-ok", kind.as_str().as_bytes()]),
        Ok(None) => digest(&[b"kind-absent"]),
        Err(error) => digest(&[b"kind-err", error.as_bytes()]),
    }
}

fn digest_find_file(answer: &Result<crate::value2::PathValue, LookupError>) -> Hash {
    match answer {
        Ok(path) => digest(&[
            b"find-ok",
            path.root.wire_name().as_bytes(),
            path.accessor_path().as_bytes(),
        ]),
        Err(LookupError::NotFound(message)) => digest(&[b"find-miss", message.as_bytes()]),
        Err(LookupError::Failed(message)) => digest(&[b"find-err", message.as_bytes()]),
        Err(LookupError::Unsupported(message)) => {
            digest(&[b"find-unsupported", message.as_bytes()])
        }
        Err(LookupError::NoResolver) => digest(&[b"find-absent"]),
    }
}

fn digest_nix_path(answer: &Result<Vec<SearchPathEntry>, LookupError>) -> Hash {
    match answer {
        Ok(entries) => {
            let mut parts: Vec<Vec<u8>> = vec![b"nixpath-ok".to_vec()];
            for e in entries {
                parts.push(e.prefix.clone().into_bytes());
                parts.push(e.path.clone().into_bytes());
            }
            let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
            digest(&refs)
        }
        Err(
            LookupError::NotFound(message)
            | LookupError::Failed(message)
            | LookupError::Unsupported(message),
        ) => digest(&[b"nixpath-err", message.as_bytes()]),
        Err(LookupError::NoResolver) => digest(&[b"nixpath-absent"]),
    }
}

/// How loudly a cache complaint needs to be said.
///
/// Two levels and not one, because the existing channel carried everything at
/// the same volume and the two kinds are not the same news. A damaged row is
/// a slower run: the cache noticed, refused it, and re-evaluated, so nobody
/// got a wrong answer. A verifier disagreement is the cache having served an
/// answer that differs from what evaluating produces, which means something
/// already believed a wrong value.
///
/// Emitted at a real priority rather than as prose that opens with the word
/// "error": systemd hands journald `info` for any line without a syslog level
/// prefix, so a severity written into the message body is invisible to every
/// query that filters on one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The cache cost something. No answer was wrong.
    Warning,
    /// The cache served an answer that re-evaluating does not reproduce, or
    /// could not serve one it should have. Somebody has to look.
    Error,
}

/// One thing the cache has to tell the embedder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Complaint {
    pub severity: Severity,
    pub message: String,
}

impl Complaint {
    #[must_use]
    pub fn warning(message: String) -> Self {
        Self {
            severity: Severity::Warning,
            message,
        }
    }

    #[must_use]
    pub fn error(message: String) -> Self {
        Self {
            severity: Severity::Error,
            message,
        }
    }
}

impl std::fmt::Display for Complaint {
    /// `severity: message`, with the severity first.
    ///
    /// Leading, not buried: a reader scanning a log and a journal filtering on
    /// priority both key on the label, and a sentence whose body happens to
    /// contain the word "error" is not a severity.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        write!(f, "{label}: {}", self.message)
    }
}

/// What the sampling verifier found, counted so a run can be judged without
/// reading its log.
///
/// Separate counters for the two failure shapes, because they are different
/// bugs and a single "problems" total would hide which. A disagreement is a
/// wrong answer served. A miss that should have hit is a cache that is
/// correct and useless -- the shape that has now cost this repo twice, in the
/// `CopyToStore` decoder gap and again when a sweep deleted every witness
/// (ENG-12601), and the shape a verifier that only compares values on hits
/// cannot see at all.
/// # Three classes, three urgencies, deliberately not one total
///
/// A single "verifier failures" number would hide which fired, and they are
/// not equally bad:
///
/// - `hits_disagreed` is a **wrong answer already shipped**. Something
///   believed a value the evaluator does not produce.
/// - `records_not_replayable` is **lost speed**. Every answer was right; the
///   cache is paying to write rows it will never serve.
/// - a sweep post-condition failure is a **bug in the store**, and lives on
///   [`crate::store::SweepReport`] rather than here, because the sweep is
///   where both facts are and no in-process counter can see it. See the note
///   on `session::evaluate`'s hit side for why the three cannot be merged.
///
/// # Where these belong once the stats block exists
///
/// ENG-12546's part 2 adds a stats/histogram block to the C ABI. These four
/// counters and `SweepReport`'s two are meant to land in it as one accounting
/// path, keeping the classes distinguishable. That block is not merged at the
/// time of writing, so they are defined here, where they are produced, rather
/// than in a second block invented to hold them; the move is a rename, not a
/// redesign.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerifierCounts {
    /// Hits that were re-evaluated and agreed.
    pub hits_checked: u64,
    /// Hits that were re-evaluated and did not agree.
    pub hits_disagreed: u64,
    /// Records that were looked up again in the same process and hit.
    pub records_checked: u64,
    /// Records that were looked up again in the same process and missed.
    ///
    /// Blind, by construction, to anything that destroys the store after this
    /// process exits: the witness is still on disk while this check runs.
    /// ENG-12601 was exactly that, and passes this check.
    pub records_not_replayable: u64,
}

/// Memoised evaluation results, keyed on the module and everything the
/// evaluation read.
///
/// The witness map is a hint and nothing more. Correctness rests on the key
/// being computed from answers observed now; a witness that no longer
/// describes what the evaluation would ask produces a key nothing was stored
/// under, hence a miss. See the module header.
pub struct ResultCache<'a, C: Cas + ?Sized> {
    cas: &'a C,
    table: MemoTable,
    lock: EffectLock,
    config: KernelConfig,
    witness: BTreeMap<EvalId, Rc<Vec<WitnessRow>>>,
    retained: Option<retained::Shared>,
    witness_shapes: BTreeMap<EvalId, Hash>,
    /// One retained successful persistent recipe per evaluation, without
    /// moving its files. Pure memory caches already retain their bounded history.
    preferred_witness: BTreeMap<EvalId, usize>,
    /// Rows and witnesses that outlive the process. Both or neither: rows
    /// without witnesses is a cache holding answers it cannot address, and
    /// witnesses without rows is a set of reads that lead nowhere.
    store: Option<&'a crate::store::Store>,
    /// Corruption found while reading the store, drained by the caller. See
    /// the note on `ModuleCache::corruption`.
    corruption: Vec<Complaint>,
    /// The row the last successful lookup served, so a consumer that finds
    /// the answer unusable can name it to [`ResultCache::forget`]. The row
    /// key is a function of the replayed read set, not of the identity
    /// alone, so it exists only once a lookup has replayed.
    served: Option<ix_kernel::Key>,
    /// How often a hit is re-evaluated and a record looked up again. 0 is off,
    /// 1 checks everything, N checks about one in N. See
    /// [`ResultCache::set_verify_rate`].
    verify_rate: u32,
    /// Xorshift state, advanced once per sampling decision.
    verify_state: u64,
    verifier: VerifierCounts,
    hits: u64,
    misses: u64,
    /// How many questions replay asked without the lookup then hitting. A
    /// replay that keeps failing is paying for a cache that never pays back,
    /// and it is invisible unless counted.
    wasted_replays: u64,
    lookup_replayed: bool,
}

enum ResultObjectFailure {
    Missing,
    Invalid(String),
}

impl ResultObjectFailure {
    fn kind(&self) -> crate::perf::CacheCorruption {
        match self {
            Self::Missing => crate::perf::CacheCorruption::ObjectMissing,
            Self::Invalid(_) => crate::perf::CacheCorruption::ObjectInvalid,
        }
    }
}

impl<'a> ResultCache<'a, dyn Cas> {
    /// Open a cache backed by one store's objects, rows, and witnesses.
    ///
    /// Lazy for the same reason `ModuleCache::persistent` is: witnesses are
    /// read one at a time, and reading every row up front made a warm store
    /// slower than performing cheap evaluations again.
    #[must_use]
    pub fn persistent(store: &'a crate::store::Store) -> Self {
        Self {
            store: Some(store),
            ..Self::new(store.cas())
        }
    }
}

impl<'a, C: Cas + ?Sized> ResultCache<'a, C> {
    pub fn new(cas: &'a C) -> Self {
        Self {
            cas,
            table: MemoTable::new(),
            lock: EffectLock::new(),
            config: KernelConfig::default(),
            witness: BTreeMap::new(),
            retained: None,
            witness_shapes: BTreeMap::new(),
            preferred_witness: BTreeMap::new(),
            store: None,
            corruption: Vec::new(),
            served: None,
            verify_rate: 0,
            // Any non-zero seed will do; the sequence only has to be spread,
            // not unpredictable. Fixed rather than time-derived so a failing
            // run can be repeated.
            verify_state: 0x2545_f491_4f6c_dd1d,
            verifier: VerifierCounts::default(),
            hits: 0,
            misses: 0,
            wasted_replays: 0,
            lookup_replayed: false,
        }
    }

    pub(crate) fn with_retained(mut self, retained: Option<retained::Shared>) -> Self {
        self.retained = retained;
        self
    }

    fn forget_retained(&self, identity: &EvalId) {
        if let (Some(retained), Some(store)) = (&self.retained, self.store) {
            retained.borrow_mut().remove(&store.witness_dir(), identity);
        }
    }

    fn remember_retained(&self, identity: &EvalId, slot: usize) {
        let key = witness_slot(identity, slot);
        if let (Some(retained), Some(store), Some(rows), Some(shape)) = (
            &self.retained,
            self.store,
            self.witness.get(&key),
            self.witness_shapes.get(&key),
        ) {
            retained.borrow_mut().insert(
                store.witness_dir(),
                *identity,
                retained::Entry {
                    rows: rows.clone(),
                    shape: *shape,
                    slot,
                },
            );
        }
    }

    /// Record that an answer could not be memoised. Not an evaluation
    /// failure: the answer was right, the next run will just be slower.
    pub fn note_record_failure(&mut self, detail: String) {
        self.corruption.push(Complaint::warning(detail));
    }

    fn complain(&mut self, kind: crate::perf::CacheCorruption, complaint: Complaint) {
        crate::perf::note_cache_corruption(kind);
        self.corruption.push(complaint);
    }

    fn load_result_object(&self, output: ObjId) -> Result<EvalResult, ResultObjectFailure> {
        let bytes = match self.cas.get_verified(output) {
            Err(error) => return Err(ResultObjectFailure::Invalid(error.to_string())),
            Ok(ix_kernel::cas::Verified::Missing) => return Err(ResultObjectFailure::Missing),
            Ok(ix_kernel::cas::Verified::Corrupt) => {
                return Err(ResultObjectFailure::Invalid(
                    "it does not hash to its address".to_owned(),
                ));
            }
            Ok(ix_kernel::cas::Verified::Found(bytes)) => bytes,
        };
        decode_result(&bytes)
            .ok_or_else(|| ResultObjectFailure::Invalid("it is not a memoised result".to_owned()))
    }

    /// The row the last successful [`ResultCache::lookup`] served.
    #[must_use]
    pub fn served_key(&self) -> Option<ix_kernel::Key> {
        self.served
    }

    /// Forget a recorded evaluation, on disk and in memory, because its
    /// consumer found the answer unusable after the cache served it.
    ///
    /// The cache checks what it can -- the object hashes to its address, the
    /// witness replays -- but the answer's *shape* is the embedder's to judge:
    /// a derivation row must parse as a store path, a flake-show row as a
    /// document. An embedder that cannot decode a served answer must not
    /// invent one and must not leave the row to be served again, so it
    /// reports the rejection here and asks again, which then evaluates.
    pub fn forget(
        &mut self,
        identity: &EvalId,
        key: ix_kernel::Key,
        why: &str,
    ) -> Result<(), KernelError> {
        self.complain(
            crate::perf::CacheCorruption::ObjectInvalid,
            Complaint::warning(format!(
                "a memoised answer was rejected by its consumer ({why}); forgetting it and re-evaluating"
            )),
        );
        self.forget_retained(identity);
        self.preferred_witness.remove(identity);
        for slot in 0..WITNESS_HISTORY {
            let slot = witness_slot(identity, slot);
            self.witness.remove(&slot);
            self.witness_shapes.remove(&slot);
        }
        self.served = None;
        self.table.remove(eval_domain(), key);
        if let Some(store) = self.store {
            let _publication = store
                .publication_guard()
                .map_err(|source| KernelError::Io {
                    doing: "locking rejected evaluation witnesses".to_owned(),
                    source,
                })?;
            store.rows().remove(eval_domain(), key)?;
            for slot in 0..WITNESS_HISTORY {
                store
                    .witness()
                    .remove(&witness_slot(identity, slot))
                    .map_err(|source| KernelError::Io {
                        doing: "removing a rejected evaluation witness".to_owned(),
                        source,
                    })?;
            }
        }
        Ok(())
    }

    fn evict_result_object(
        &mut self,
        identity: &EvalId,
        key: ix_kernel::Key,
        output: ObjId,
        failure: ResultObjectFailure,
    ) {
        let detail = match &failure {
            ResultObjectFailure::Missing => "the store does not have it",
            ResultObjectFailure::Invalid(detail) => detail,
        };
        self.complain(
            failure.kind(),
            Complaint::warning(format!(
                "object {output} for a memoised result was unusable ({detail}); re-evaluating"
            )),
        );
        self.table.remove(eval_domain(), key);
        self.witness.remove(identity);
        self.witness_shapes.remove(identity);
    }

    /// Check one hit in `rate` by evaluating anyway, and one record in `rate`
    /// by looking it up again. 0 turns it off; 1 checks every one.
    ///
    /// # Why sample at all, rather than check everything or nothing
    ///
    /// Checking everything costs a full evaluation per hit, which is the
    /// entire saving the cache exists for. Checking nothing is what shipped,
    /// and ENG-12541 -- a memo key blind to the store directory, so a cache
    /// shared across stores served paths for the wrong one -- would have been
    /// found in production by a one-in-twenty check and was instead found by
    /// reading the code. A rate is the only setting under which the cache is
    /// both worth having and watched.
    ///
    /// # What a sampled run returns
    ///
    /// The **served** answer, on both paths, even when the check disagreed
    /// with it. Not because it is the more trustworthy of the two -- it is
    /// the less -- but because it is what every unsampled run of the same
    /// expression gets, and a command whose output depended on whether the
    /// sampler happened to pick it would be the harder bug to chase. The
    /// disagreement leaves an error-priority complaint, which is the part
    /// that must not be missed.
    ///
    /// It costs the handle path more than it costs
    /// [`crate::session::evaluate`]. There the check is an extra `run` this
    /// crate performs; on the handle path the embedder has to redo its walk,
    /// so `capi::ixe_session_eval_question` hands back both a root handle and
    /// the served answer and asks for the fresh one back to compare against.
    /// `capi::warm_starts::a_sampled_hit_is_checked_rather_than_trusted`
    /// exercises that arm, which is otherwise dead code: the default rate is
    /// 0, so nothing in an ordinary run or in any gate enters it.
    pub fn set_verify_rate(&mut self, rate: u32) {
        self.verify_rate = rate;
    }

    /// Whether this occasion is one of the sampled ones.
    ///
    /// Xorshift rather than a counter: every Nth is predictable, and a
    /// workload whose shape lines up with N would sample the same expressions
    /// for ever and never look at the others. The seed is fixed, so a run
    /// that finds something can be repeated.
    pub fn should_verify(&mut self) -> bool {
        match self.verify_rate {
            0 => false,
            1 => true,
            rate => {
                let mut x = self.verify_state;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.verify_state = x;
                x.is_multiple_of(u64::from(rate))
            }
        }
    }

    /// The sampler's state, for a caller that builds a cache per call.
    ///
    /// [`crate::session::QuestionCache`] does, because the store it borrows
    /// outlives it, and without carrying this the draw restarts from the
    /// fixed seed every time: "one hit in N" becomes "every hit or no hit",
    /// the same in every process, and a sampler that is off looks exactly
    /// like one that is on and happening not to fire.
    #[must_use]
    pub fn verify_state(&self) -> u64 {
        self.verify_state
    }

    /// Resume the sampler where another cache over the same store left off.
    pub fn set_verify_state(&mut self, state: u64) {
        self.verify_state = state;
    }

    /// What the verifier has seen. Counted even when nothing went wrong, so a
    /// run can tell "checked and agreed" from "never checked" -- the two look
    /// identical in a log that only speaks up on failure.
    #[must_use]
    pub fn verifier(&self) -> VerifierCounts {
        self.verifier
    }

    /// A sampled hit was re-evaluated and agreed.
    pub fn note_hit_verified(&mut self) {
        self.verifier.hits_checked += 1;
    }

    /// A sampled hit was re-evaluated and did not agree. The cache served an
    /// answer that evaluating does not reproduce.
    pub fn note_hit_disagreed(&mut self, identity: &EvalId, served: &str, fresh: &str) {
        self.verifier.hits_checked += 1;
        self.verifier.hits_disagreed += 1;
        self.corruption.push(Complaint::error(format!(
            "the evaluation cache served an answer that re-evaluating does not \
             reproduce. memo key {}: served {served:?}, evaluating now gives \
             {fresh:?}. Every answer this cache has given for this key is \
             suspect; the key is printed so the row can be found and the \
             inputs that differ identified.",
            identity.as_hash().to_hex()
        )));
    }

    /// A sampled record was looked up again in the same process, as it must
    /// be for the cache to ever pay back.
    pub fn note_record_replayable(&mut self, replayable: bool, identity: &EvalId) {
        self.verifier.records_checked += 1;
        if !replayable {
            self.verifier.records_not_replayable += 1;
            self.corruption.push(Complaint::error(format!(
                "the evaluation cache recorded a result and then could not \
                 find it again in the same process. memo key {}: everything \
                 filed under this key is unreachable, so the cache is paying \
                 to write answers it will never serve. This is what a key that \
                 differs between the record and lookup paths looks like, and \
                 what an unreadable witness looks like.",
                identity.as_hash().to_hex()
            )));
        }
    }

    /// Take the corruption found since the last call.
    pub fn take_corruption(&mut self) -> Vec<Complaint> {
        core::mem::take(&mut self.corruption)
    }

    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    #[must_use]
    pub fn wasted_replays(&self) -> u64 {
        self.wasted_replays
    }

    /// Try to answer without evaluating.
    ///
    /// Asks the host the questions last time's evaluation asked, keys on the
    /// answers it gets *now*, and returns a stored result only if one exists
    /// under exactly that key.
    pub fn lookup(
        &mut self,
        identity: &EvalId,
        host: &dyn Host,
        settings: &crate::eval::Settings,
    ) -> Option<EvalResult> {
        self.lookup_with_hook(identity, host, settings, || {})
    }

    #[cfg(test)]
    fn lookup_with_object_hook(
        &mut self,
        identity: &EvalId,
        host: &dyn Host,
        settings: &crate::eval::Settings,
        after_row: impl FnOnce(),
    ) -> Option<EvalResult> {
        self.lookup_with_hook(identity, host, settings, after_row)
    }

    fn lookup_with_hook(
        &mut self,
        identity: &EvalId,
        host: &dyn Host,
        settings: &crate::eval::Settings,
        after_row: impl FnOnce(),
    ) -> Option<EvalResult> {
        self.lookup_replayed = false;
        self.served = None;
        // Repair must perform effects again, regardless of witness history.
        if settings.repair {
            return None;
        }
        if let (Some(retained), Some(store)) = (&self.retained, self.store) {
            // Borrow only while cloning the immutable payload. Host replay can
            // enter another session sharing this owner without a RefCell borrow.
            let cached = retained.borrow_mut().get(&store.witness_dir(), identity);
            self.preferred_witness.remove(identity);
            for slot in 0..WITNESS_HISTORY {
                let key = witness_slot(identity, slot);
                self.witness.remove(&key);
                self.witness_shapes.remove(&key);
            }
            if let Some(entry) = cached {
                let key = witness_slot(identity, entry.slot);
                self.preferred_witness.insert(*identity, entry.slot);
                self.witness_shapes.insert(key, entry.shape);
                self.witness.insert(key, entry.rows);
            }
        }
        let mut after_row = Some(after_row);
        if self.store.is_none() {
            for slot in 0..WITNESS_HISTORY {
                if let Some(result) =
                    self.lookup_candidate(identity, slot, true, host, settings, &mut after_row)
                {
                    return Some(result);
                }
            }
            return None;
        }
        if let Some(preferred) = self.preferred_witness.get(identity).copied()
            && let Some(result) =
                self.lookup_candidate(identity, preferred, true, host, settings, &mut after_row)
        {
            return Some(result);
        }
        let cached_shape = self
            .preferred_witness
            .get(identity)
            .and_then(|slot| self.witness_shapes.get(&witness_slot(identity, *slot)))
            .copied();
        self.forget_retained(identity);
        // The cached recipe already missed. Keep only its small fingerprint
        // while loading alternatives, so large decoded witnesses never stack.
        if let Some(previous) = self.preferred_witness.remove(identity) {
            let previous = witness_slot(identity, previous);
            self.witness.remove(&previous);
            self.witness_shapes.remove(&previous);
        }
        for slot in 0..WITNESS_HISTORY {
            // Another process may have rotated this physical slot since our
            // cached recipe was read. Skip only an identical recipe, never a
            // slot merely because it used to contain the cached one.
            if let Some(shape) = cached_shape
                && let Some(store) = self.store
                && store
                    .witness()
                    .has_shape(&witness_slot(identity, slot), shape)
            {
                continue;
            }
            if let Some(result) =
                self.lookup_candidate(identity, slot, false, host, settings, &mut after_row)
            {
                return Some(result);
            }
        }
        None
    }

    fn lookup_candidate(
        &mut self,
        identity: &EvalId,
        slot_index: usize,
        use_cached: bool,
        host: &dyn Host,
        settings: &crate::eval::Settings,
        after_row: &mut Option<impl FnOnce()>,
    ) -> Option<EvalResult> {
        // Failed disk candidates live only for this call. Retain only the
        // successful one, replacing the previous persistent payload for this
        // evaluation. Repeated historical hits then need no disk decoding.
        let slot = &witness_slot(identity, slot_index);
        let disk = if use_cached && self.witness.contains_key(slot) {
            None
        } else {
            match self.store?.witness().get_with_shape(slot) {
                Ok(None) => return None,
                Ok(Some(witness)) => {
                    if let Some(retained) = &self.retained {
                        retained.borrow_mut().note_disk_load();
                    }
                    Some(witness)
                }
                Err(reason) => {
                    self.complain(
                        crate::perf::CacheCorruption::WitnessRefused,
                        Complaint::warning(format!("{reason}; treating it as a cache miss")),
                    );
                    return None;
                }
            }
        };
        let rows = disk
            .as_ref()
            .map(|(rows, _)| rows.as_slice())
            .or_else(|| self.witness.get(slot).map(|rows| rows.as_slice()))?;
        self.lookup_replayed = true;
        let observed = match ReadSet::replay_key_with(
            rows,
            host,
            settings,
            self.cas,
            self.store.map(crate::store::Store::sealed),
            identity,
        ) {
            Ok(Some(observed)) => observed,
            Ok(None) => return None,
            Err(failure) => {
                let (kind, detail) = match failure {
                    ReplayFailure::MissingAterm(id) => (
                        crate::perf::CacheCorruption::ObjectMissing,
                        format!("ATerm object {id} named by an evaluation witness is absent"),
                    ),
                    ReplayFailure::CorruptAterm(id) => (
                        crate::perf::CacheCorruption::ObjectInvalid,
                        format!(
                            "ATerm object {id} named by an evaluation witness does not hash to its address"
                        ),
                    ),
                    ReplayFailure::InvalidAterm(id) => (
                        crate::perf::CacheCorruption::ObjectInvalid,
                        format!("ATerm object {id} named by an evaluation witness is not UTF-8"),
                    ),
                    ReplayFailure::Cas { aterm, error } => (
                        crate::perf::CacheCorruption::ObjectInvalid,
                        format!(
                            "could not read ATerm object {aterm} named by an evaluation witness: {error}"
                        ),
                    ),
                    ReplayFailure::DrvPathMismatch { expected, written } => (
                        crate::perf::CacheCorruption::WitnessRefused,
                        format!(
                            "replaying a derivation write produced {written:?}, expected {expected:?}"
                        ),
                    ),
                    ReplayFailure::WriteDrv { drv_path, error } => (
                        crate::perf::CacheCorruption::WitnessRefused,
                        format!("replaying derivation write for {drv_path:?} failed: {error:?}"),
                    ),
                };
                self.complain(
                    kind,
                    Complaint::warning(format!("{detail}; treating the witness as a cache miss")),
                );
                self.witness.remove(slot);
                self.witness_shapes.remove(slot);
                return None;
            }
        };
        let key = row_key(&observed).ok()?;
        // Bring the row in from disk if this process has not seen it.
        if self.table.get(eval_domain(), key).is_none()
            && let Some(rows) = self.store.map(crate::store::Store::rows)
        {
            match rows.get(eval_domain(), key) {
                Lookup::Missing => {}
                Lookup::Refused(reason) => {
                    self.corruption.push(Complaint::warning(reason.to_string()))
                }
                Lookup::Found(output) => {
                    self.table.insert(
                        eval_domain(),
                        key,
                        ix_kernel::Entry {
                            output,
                            policy: Policy::Keyed,
                            provenance: ix_kernel::Provenance::Deterministic,
                        },
                    );
                }
            }
        }
        let output = self.table.get(eval_domain(), key)?.output;
        if let Some(after_row) = after_row.take() {
            after_row();
        }
        match self.load_result_object(output) {
            Ok(result) => {
                self.hits += 1;
                self.served = Some(key);
                if self.store.is_some() {
                    let previous = self
                        .preferred_witness
                        .insert(*identity, slot_index)
                        .unwrap_or(0);
                    if previous != slot_index {
                        let previous = witness_slot(identity, previous);
                        self.witness.remove(&previous);
                        self.witness_shapes.remove(&previous);
                    }
                    if let Some((rows, shape)) = disk {
                        self.witness_shapes.insert(*slot, shape);
                        self.witness.insert(*slot, Rc::new(rows));
                    }
                }
                self.remember_retained(identity, slot_index);
                // Both halves of the entry, so a sweep ordering by use keeps
                // the row and the witness it was served through together.
                if let Some(store) = self.store {
                    store.rows().touch(eval_domain(), key);
                    store.witness().touch(slot);
                }
                Some(result)
            }
            Err(failure) => {
                self.evict_result_object(slot, key, output, failure);
                None
            }
        }
    }

    /// Called when a lookup missed and the evaluation ran, so the counters can
    /// tell a first sighting from a replay that did not pay off.
    pub fn note_miss(&mut self, identity: &EvalId) {
        self.misses += 1;
        if self.lookup_replayed || self.witness.contains_key(identity) {
            self.wasted_replays += 1;
        }
    }

    /// Record what an evaluation read and what it produced. `host` and
    /// `settings` are the recording evaluation's, for the sealing question
    /// [`ReadSet::fold_trees`] asks before the rows are written.
    pub fn record(
        &mut self,
        identity: &EvalId,
        read_set: &ReadSet,
        result: &EvalResult,
        host: &dyn Host,
        settings: &crate::eval::Settings,
    ) -> Result<(), KernelError> {
        self.record_with_publication_hooks(identity, read_set, result, host, settings, || {}, || {})
    }

    #[cfg(test)]
    fn record_with_publication_hook(
        &mut self,
        identity: &EvalId,
        read_set: &ReadSet,
        result: &EvalResult,
        host: &dyn Host,
        settings: &crate::eval::Settings,
        after_objects: impl FnOnce(),
    ) -> Result<(), KernelError> {
        self.record_with_publication_hooks(
            identity,
            read_set,
            result,
            host,
            settings,
            || {},
            after_objects,
        )
    }

    #[cfg(test)]
    fn record_with_aterm_publication_hook(
        &mut self,
        identity: &EvalId,
        read_set: &ReadSet,
        result: &EvalResult,
        host: &dyn Host,
        settings: &crate::eval::Settings,
        after_reused_aterm: impl FnOnce(),
    ) -> Result<(), KernelError> {
        self.record_with_publication_hooks(
            identity,
            read_set,
            result,
            host,
            settings,
            after_reused_aterm,
            || {},
        )
    }

    // Two sequencing hooks for the tests beside the five inputs a record
    // has; bundling the inputs would name a struct for one call site.
    #[allow(clippy::too_many_arguments)]
    fn record_with_publication_hooks(
        &mut self,
        identity: &EvalId,
        read_set: &ReadSet,
        result: &EvalResult,
        host: &dyn Host,
        settings: &crate::eval::Settings,
        after_reused_aterm: impl FnOnce(),
        after_objects: impl FnOnce(),
    ) -> Result<(), KernelError> {
        // Folding asks the store which objects are sealed, a walk of each
        // unknown object; it runs before the publication lock so a sweep is
        // not held off by it.
        let folded =
            read_set.fold_trees(host, settings, self.store.map(crate::store::Store::sealed));
        // Persistent publication and sweeping share this operating-system
        // lock. It starts before the first object check because an object that
        // already exists can still be unreferenced and sweepable, and ends
        // only after both witness and row are in place.
        let _publication = self
            .store
            .map(crate::store::Store::publication_guard)
            .transpose()
            .map_err(|source| KernelError::Io {
                doing: "locking the evaluation store for result publication".to_owned(),
                source,
            })?;
        let mut aterms_stored = 0_u64;
        let mut aterms_reused = read_set.aterms_reused;
        let mut after_reused_aterm = Some(after_reused_aterm);
        for (expected, bytes) in &read_set.aterms {
            if ObjId::of(bytes) != *expected {
                return Err(KernelError::Perform {
                    domain: eval_domain(),
                    detail: format!("ATerm bytes do not hash to their recorded address {expected}"),
                });
            }
            if self
                .cas
                .get(*expected)?
                .is_some_and(|stored| stored.as_slice() == bytes.as_slice())
            {
                aterms_reused = aterms_reused.wrapping_add(1);
                if let Some(after_reused_aterm) = after_reused_aterm.take() {
                    after_reused_aterm();
                }
                continue;
            }
            let stored = self.cas.put(bytes)?;
            if stored != *expected {
                return Err(KernelError::Perform {
                    domain: eval_domain(),
                    detail: format!("CAS returned {stored} after storing ATerm {expected}"),
                });
            }
            aterms_stored = aterms_stored.wrapping_add(1);
        }
        crate::perf::note_aterms(aterms_stored, aterms_reused);

        let encoded = request_bytes(&ReadSet::key_of(&folded.rows, identity))?;
        let key = ix_kernel::Key::mint(eval_domain(), &encoded);
        if let Some(output) = self.table.get(eval_domain(), key).map(|entry| entry.output)
            && let Err(failure) = self.load_result_object(output)
        {
            self.evict_result_object(identity, key, output, failure);
        }
        let payload = encode_result(result)?;
        let performed = on_perform(
            PerformCtx {
                table: &mut self.table,
                lock: &mut self.lock,
                cas: self.cas,
                config: &self.config,
                performed_at: "",
                blessed_by: "",
            },
            eval_domain(),
            &Policy::Keyed,
            &encoded,
            || Ok::<_, KernelError>(payload),
        )?;
        after_objects();
        let rows = folded.rows;
        let shape = witness_shape(&rows);
        let mut witness_bytes = 0;
        if let Some(store) = self.store.map(crate::store::Store::witness) {
            // A witness that cannot be written costs future hits and nothing
            // else, so it must not fail the evaluation that produced it.
            witness_bytes = store
                .put_history(identity, &rows, shape)
                .map_err(|source| KernelError::Io {
                    doing: "recording an evaluation witness".to_owned(),
                    source,
                })?;
        }
        crate::perf::note_witness(
            rows.len() as u64,
            read_set.rows_deduped,
            folded.rows_folded,
            witness_bytes as u64,
        );
        if let Some(table) = self.store.map(crate::store::Store::rows) {
            table.put(eval_domain(), &encoded, performed.output)?;
        }
        if self.store.is_some() {
            if let Some(previous) = self.preferred_witness.insert(*identity, 0) {
                let previous = witness_slot(identity, previous);
                self.witness.remove(&previous);
                self.witness_shapes.remove(&previous);
            }
        } else {
            let duplicate = (0..WITNESS_HISTORY).find(|slot| {
                self.witness_shapes.get(&witness_slot(identity, *slot)) == Some(&shape)
            });
            for slot in (1..=duplicate.unwrap_or(WITNESS_HISTORY - 1)).rev() {
                let from = witness_slot(identity, slot - 1);
                let to = witness_slot(identity, slot);
                self.witness.remove(&to);
                self.witness_shapes.remove(&to);
                if let Some(rows) = self.witness.remove(&from) {
                    self.witness.insert(to, rows);
                }
                if let Some(shape) = self.witness_shapes.remove(&from) {
                    self.witness_shapes.insert(to, shape);
                }
            }
        }
        self.witness_shapes.insert(*identity, shape);
        self.witness.insert(*identity, Rc::new(rows));
        self.remember_retained(identity, 0);
        // The publication is complete; the sweep takes the same lock, so it
        // has to be released first.
        drop(_publication);
        self.sweep_to_cap();
        Ok(())
    }

    /// Bring a capped store under its cap now that a record is published.
    /// A sweep can only cost a later miss, and a failure here is a complaint
    /// rather than an error: the answer is already recorded and correct.
    fn sweep_to_cap(&mut self) {
        if let Some(complaint) = self.store.and_then(sweep_to_cap) {
            self.corruption.push(complaint);
        }
    }
}

/// Bring a store under its cap after a publication; the complaint to surface
/// if that failed. Shared by [`ResultCache::record`] and
/// [`crate::session::evaluate_value_once`], the two places a publication can
/// end, so both enforce the same cap the same way.
pub(crate) fn sweep_to_cap(store: &crate::store::Store) -> Option<Complaint> {
    store
        .sweep_to_cap()
        .err()
        .map(|error| Complaint::warning(format!("sweeping the evaluation cache failed: {error}")))
}

/// The canonical request bytes a result row is keyed by.
///
/// Both the lookup and the record path go through this. They used to build
/// the key separately, one from the raw digest and one from its canonical
/// encoding, so every lookup missed while every store succeeded: the cache
/// was correct, cost extra, and never once paid back. Correct-but-useless is
/// invisible to a gate that only compares answers, which is why the harness
/// counts how many answers came from the cache and not just whether they
/// agreed.
fn request_bytes(key: &Hash) -> Result<Vec<u8>, KernelError> {
    Ok(canon::encode(&CanonValue::Bytes(key.as_bytes().to_vec()))?)
}

fn row_key(key: &Hash) -> Result<ix_kernel::Key, KernelError> {
    Ok(ix_kernel::Key::mint(eval_domain(), &request_bytes(key)?))
}

/// `Key::mint` hashes the canonical request, and the request here is just the
/// read-set key, so this is the one place the two hashings meet.
fn encode_result(result: &EvalResult) -> Result<Vec<u8>, KernelError> {
    Ok(canon::encode(&CanonValue::map([
        ("status", CanonValue::str(result.status.as_str())),
        ("value", CanonValue::str(result.value.as_str())),
        (
            // Empty means "not a refusal", which is not the same as the key
            // being absent; absent is a row written before tokens existed,
            // and `decode_result` keeps the two apart.
            "token",
            CanonValue::str(result.token.map_or("", |t| t.as_str())),
        ),
        (
            // A failure with no position writes an empty array rather than
            // omitting the key, so a row that has one and a row from before
            // positions existed stay distinguishable on the way back in.
            "pos",
            match &result.pos {
                None => CanonValue::array([]),
                Some(pos) => CanonValue::array([
                    CanonValue::str(pos.file.as_deref().unwrap_or("")),
                    CanonValue::str(pos.line.to_string()),
                    CanonValue::str(pos.column.to_string()),
                ]),
            },
        ),
        (
            "emissions",
            CanonValue::array(result.emissions.iter().map(|emission| {
                let (kind, message) = emission.parts();
                CanonValue::array([CanonValue::str(kind), CanonValue::str(message.as_str())])
            })),
        ),
    ]))?)
}

fn decode_result(bytes: &[u8]) -> Option<EvalResult> {
    let CanonValue::Map(entries) = canon::decode(bytes).ok()? else {
        return None;
    };
    let field = |name: &str| {
        entries.iter().find_map(|(k, v)| match (k, v) {
            (CanonValue::Str(k), CanonValue::Str(v)) if k == name => Some(v.clone()),
            _ => None,
        })
    };
    // Every field of the current format is required, with its exact type.
    // There are no rows of an older shape to read: the evaluation identity
    // (`EVAL_ID_TAG`) retires every row written under another format, so a
    // key that is missing here is a damaged row, and `lookup` treats the
    // `None` as corruption rather than serving a partial answer.
    let array = |name: &str| {
        entries.iter().find_map(|(k, v)| match (k, v) {
            (CanonValue::Str(k), CanonValue::Array(items)) if k == name => Some(items),
            _ => None,
        })
    };
    let mut emissions = Vec::new();
    for item in array("emissions")? {
        // Anything that is not a `(kind, message)` pair of strings this
        // build knows is a damaged row.
        let CanonValue::Array(pair) = item else {
            return None;
        };
        let (Some(CanonValue::Str(kind)), Some(CanonValue::Str(message))) =
            (pair.first(), pair.get(1))
        else {
            return None;
        };
        emissions.push(Emission::from_parts(kind, message.clone())?);
    }
    let status = field("status")?;
    // An empty token is "not a refusal"; a name this build does not know is
    // a damaged row, never `Unrecorded`, which is reserved for refusals the
    // evaluator itself could not classify.
    let token = match field("token")? {
        name if name.is_empty() => None,
        name => Some(crate::refusal::RefusalToken::parse(&name)?),
    };
    // An empty array is "no position"; a malformed triple is a damaged row.
    let pos = match array("pos")? {
        items if items.is_empty() => None,
        items => {
            let (
                Some(CanonValue::Str(file)),
                Some(CanonValue::Str(line)),
                Some(CanonValue::Str(column)),
            ) = (items.first(), items.get(1), items.get(2))
            else {
                return None;
            };
            let (Ok(line), Ok(column)) = (line.parse(), column.parse()) else {
                return None;
            };
            Some(crate::vm::SrcPos {
                file: if file.is_empty() {
                    None
                } else {
                    Some(std::rc::Rc::from(file.as_str()))
                },
                line,
                column,
            })
        }
    };
    Some(EvalResult {
        status,
        value: field("value")?,
        emissions,
        token,
        pos,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Fake {
        contents: Cell<u8>,
    }
    impl Host for Fake {
        crate::host::host_stubs!(settle);
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
            copy_to_store,
            ensure_path,
            warn,
            find_file,
            nix_path,
            trace
        );
        fn read_file(&self, path: &crate::value2::PathValue) -> Result<String, String> {
            match path {
                p if p.path.as_ref() == "/a" => Ok(format!("v{}", self.contents.get())),
                _ => Err(format!("path '{path}' does not exist")),
            }
        }
        fn read_dir(
            &self,
            _p: &crate::value2::PathValue,
        ) -> Result<Vec<(String, FileType)>, String> {
            Ok(vec![("x".to_owned(), FileType::Regular)])
        }
        fn path_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(path.path.as_ref() == "/a")
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
            Ok(Some(FileType::Regular))
        }
        fn get_env(&self, name: &str) -> Option<String> {
            match name {
                "SET" => Some("yes".to_owned()),
                _ => None,
            }
        }
    }

    fn fake() -> Fake {
        Fake {
            contents: Cell::new(1),
        }
    }

    /// Replay under the default settings, where reads are allowed, so the
    /// Witness rows for `questions` with a digest no recording could
    /// produce, so a test that reaches the recorded answer by mistake keys
    /// differently from every host answer instead of passing by accident.
    fn rows_of(questions: &[Question]) -> Vec<WitnessRow> {
        let unrecorded = digest(&[b"test-row-never-recorded"]);
        questions
            .iter()
            .map(|question| (question.clone(), Recorded::Digest(unrecorded)))
            .collect()
    }

    /// Replay under the default settings, where reads are allowed, so the
    /// refusal `ReadSet::replay` returns under `pure-eval` is not silently
    /// swallowed as an empty read set. A test that meant to exercise the
    /// refusal asserts on the `None` itself.
    fn replayed(questions: &[Question], host: &dyn Host) -> ReadSet {
        replayed_rows(&rows_of(questions), host)
    }

    /// [`replayed`] over recorded rows, whose records a from-record replay
    /// takes.
    fn replayed_rows(rows: &[WitnessRow], host: &dyn Host) -> ReadSet {
        let cas = ix_kernel::cas::MemoryCas::new();
        match ReadSet::replay(rows, host, &crate::eval::Settings::default(), &cas) {
            Ok(Some(set)) => set,
            Ok(None) => unreachable!("replay refused; a purity setting is on in this test"),
            Err(error) => unreachable!("replay failed: {error:?}"),
        }
    }

    /// An evaluation identity for a made-up module under a stated
    /// configuration.
    ///
    /// `Settings::default()` and not `Settings::current()`, which is the
    /// whole of why `the_module_is_part_of_the_key` used to fail about once
    /// in fifty runs: the identity was built from the process settings, so a
    /// test moving `pure-eval` between this call and the next changed the key
    /// out from under a lookup that had to hit. That the settings are part of
    /// the key is the correct behaviour being tested elsewhere; a test about
    /// the *module* half has no business varying the settings half.
    fn id(module: &[u8]) -> EvalId {
        EvalId::of(
            &hash::tagged("m", &[module]),
            &crate::eval::Settings::default(),
            // These tests vary the read set and hold the arguments and the
            // question constant; those axes have their own tests in `session`.
            &crate::session::Arguments::none(),
            &crate::session::Question::Whole {
                render: crate::session::RenderMode::Plain,
            },
        )
    }

    /// A store path a fetcher mounted; `mounted` spells paths under it.
    const MOUNT: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src";

    fn mounted(spelling: &str) -> crate::value2::PathValue {
        crate::value2::PathValue::new(crate::value2::Root::mounted(MOUNT), spelling)
    }

    #[test]
    fn streamed_replay_matches_materialized_keys_for_every_question() {
        let cas = ix_kernel::cas::MemoryCas::new();
        let identity = id(b"streamed-question-census");
        let mut answered = 0;
        let mut failed = 0;
        for question in Question::one_of_each() {
            let rows = rows_of(&[question]);
            let settings = crate::eval::Settings::default();
            let materialized = ReadSet::replay(&rows, &fake(), &settings, &cas)
                .map(|set| set.map(|set| set.key(&identity)));
            let streamed =
                ReadSet::replay_key_with(&rows, &fake(), &settings, &cas, None, &identity);
            match (materialized, streamed) {
                (Ok(expected), Ok(actual)) => {
                    assert_eq!(actual, expected);
                    answered += 1;
                }
                (Err(expected), Err(actual)) => {
                    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
                    failed += 1;
                }
                (expected, actual) => panic!("replay differs: {expected:?} vs {actual:?}"),
            }
            let pure = crate::eval::Settings {
                pure_eval: true,
                ..settings
            };
            let expected = ReadSet::replay(&rows, &fake(), &pure, &cas)
                .map(|set| set.map(|set| set.key(&identity)));
            let actual = ReadSet::replay_key_with(&rows, &fake(), &pure, &cas, None, &identity);
            assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
        }
        assert_eq!(answered + failed, Question::VARIANT_COUNT);
        assert!(answered > 20, "the census must exercise successful hashing");
        assert!(failed > 0, "missing ATerm failures must be exercised");
    }

    #[test]
    fn streamed_replay_hashes_changed_answers_and_recovers_the_original_key() {
        let identity = id(b"streamed-world-edit");
        let settings = crate::eval::Settings::default();
        let cas = ix_kernel::cas::MemoryCas::new();
        let questions = [
            Question::ReadFile(crate::value2::ambient_path("/a")),
            Question::GetEnv("SET".to_owned()),
        ];
        let recorded = replayed(&questions, &fake());
        let original = recorded.key(&identity);
        for version in [1, 2, 1] {
            let materialized_host = Fake {
                contents: Cell::new(version),
            };
            let streamed_host = Fake {
                contents: Cell::new(version),
            };
            let materialized =
                ReadSet::replay(recorded.entries(), &materialized_host, &settings, &cas)
                    .expect("replay")
                    .expect("allowed")
                    .key(&identity);
            let streamed = ReadSet::replay_key_with(
                recorded.entries(),
                &streamed_host,
                &settings,
                &cas,
                None,
                &identity,
            )
            .expect("replay")
            .expect("allowed");
            assert_eq!(streamed, materialized);
            assert_eq!(streamed == original, version == 1);
        }
    }

    /// Run each mode in a separate process under a resource measurement tool.
    /// Fixture creation precedes replay; both paths must produce the recorded key.
    #[test]
    #[ignore = "manual replay allocation benchmark; set IXE_REPLAY_BENCH_MODE=materialized or streamed"]
    fn replay_allocation_benchmark() {
        let settings = in_store();
        let cas = ix_kernel::cas::MemoryCas::new();
        let identity = id(b"large-replay-witness");
        let root = crate::value2::ambient_path(format!("/nix/store/{OBJECT}/src"));
        let rows: Vec<_> = (0..32)
            .map(|index| {
                let name = format!("large-{index}");
                // Each row owns 1 MiB of ordinary paths beneath the sealed root.
                let accepted = (0..8192)
                    .map(|file| {
                        let mut path = format!("{}/file-{file:04}-", root.path);
                        let padding = 128usize.checked_sub(path.len()).expect("short prefix");
                        path.extend(std::iter::repeat_n('x', padding));
                        crate::task::AcceptedPath {
                            path,
                            file_type: FileType::Regular,
                        }
                    })
                    .collect();
                let answer = copied_to(&name);
                (
                    Question::StoreFiltered(Box::new(crate::task::FilteredCopy {
                        root: Rc::clone(&root),
                        name,
                        method: crate::task::PathMethod::NixArchive,
                        accepted: Some(accepted),
                        expected_sha256: None,
                        inherit_references: false,
                    })),
                    Recorded::Answer(answer),
                )
            })
            .collect();
        let expected = ReadSet::key_of(&rows, &identity);
        let host = Sealing::of(&[OBJECT]);
        let mode = std::env::var("IXE_REPLAY_BENCH_MODE").expect("explicit benchmark mode");
        let started = std::time::Instant::now();
        let actual = match mode.as_str() {
            "materialized" => {
                let copied = rows.clone();
                ReadSet::replay(&copied, &host, &settings, &cas)
                    .expect("replay")
                    .expect("allowed")
                    .key(&identity)
            }
            "streamed" => ReadSet::replay_key_with(&rows, &host, &settings, &cas, None, &identity)
                .expect("replay")
                .expect("allowed"),
            _ => panic!("unknown replay benchmark mode {mode:?}"),
        };
        assert_eq!(actual, expected);
        assert_eq!(host.copied.get(), 0);
        eprintln!(
            "replay-benchmark mode={mode} rows={} owned_bytes={} elapsed_ns={}",
            rows.len(),
            32 * 1024 * 1024,
            started.elapsed().as_nanos()
        );
    }

    fn questions(rows: &[WitnessRow]) -> Vec<&Question> {
        rows.iter().map(|(question, _)| question).collect()
    }

    /// A host whose store calls exactly `sealed` sealed, with no leaving
    /// links, holds every path but `absent`, counts the sealing questions it
    /// is asked, answers every read, walks every filtered copy it is asked
    /// for (counted in `copied`) to the path [`copied_to`] names, and records
    /// the paths it is asked to allow (`allowed`), or refuses them all when
    /// `refuse_allow` is set.
    struct Sealing {
        sealed: Vec<String>,
        absent: Vec<String>,
        asked_sealed: Cell<usize>,
        copied: Cell<usize>,
        allowed: RefCell<Vec<String>>,
        refuse_allow: Cell<bool>,
    }
    impl Sealing {
        fn of(sealed: &[&str]) -> Self {
            Self {
                sealed: sealed.iter().map(|object| (*object).to_owned()).collect(),
                absent: Vec::new(),
                asked_sealed: Cell::new(0),
                copied: Cell::new(0),
                allowed: RefCell::new(Vec::new()),
                refuse_allow: Cell::new(false),
            }
        }
        /// The same store after losing the store paths `absent`.
        fn absent(mut self, absent: &[&str]) -> Self {
            self.absent = absent.iter().map(|path| (*path).to_owned()).collect();
            self
        }
    }
    impl Host for Sealing {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            fetch,
            lock_flake,
            fetch_tree,
            not_async,
            ensure_path,
        );
        crate::host::host_stubs!(
            read_file_bytes,
            file_type_resolved,
            get_env,
            copy_to_store,
            warn,
            find_file,
            nix_path,
            trace
        );
        fn store_filtered(
            &self,
            request: &crate::task::FilteredCopy,
        ) -> Result<String, StoreError> {
            self.copied.set(self.copied.get() + 1);
            Ok(copied_to(&request.name))
        }
        fn read_file(&self, path: &crate::value2::PathValue) -> Result<String, String> {
            Ok(format!("contents of {}", path.path))
        }
        fn read_dir(
            &self,
            _p: &crate::value2::PathValue,
        ) -> Result<Vec<(String, FileType)>, String> {
            Ok(vec![("a".to_owned(), FileType::Regular)])
        }
        fn path_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
            Ok(true)
        }
        fn dir_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
            Ok(true)
        }
        fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
            Ok(Some(FileType::Regular))
        }
        fn sealed_paths(&self, objects: &[String]) -> Result<crate::host::Links, StoreError> {
            self.asked_sealed.set(self.asked_sealed.get() + 1);
            Ok(objects
                .iter()
                .filter(|object| self.sealed.contains(object))
                .map(|object| (object.clone(), Vec::new()))
                .collect())
        }
        fn valid_paths(
            &self,
            paths: &[String],
        ) -> Result<std::collections::BTreeSet<String>, StoreError> {
            Ok(paths
                .iter()
                .filter(|path| !self.absent.contains(path))
                .cloned()
                .collect())
        }
        /// `allowPath` per path, as the bridge's; the walked copy's own hook
        /// allows on the embedder's side and never comes through here, so
        /// what this records is what the memo allowed.
        fn allow_paths(&self, paths: &[String]) -> Result<(), StoreError> {
            if self.refuse_allow.get() {
                return Err(StoreError::Failed("allow refused".to_owned()));
            }
            self.allowed.borrow_mut().extend(paths.iter().cloned());
            Ok(())
        }
    }

    const OBJECT: &str = "00000000000000000000000000000000-source";

    /// Where [`Sealing`]'s store lands a filtered copy named `name`: a
    /// function of the request alone, as a real store's answer is.
    fn copied_to(name: &str) -> String {
        format!("/nix/store/11111111111111111111111111111111-{name}")
    }

    /// The default settings with the store at `/nix/store`, which is what
    /// places an ambient path inside a store object.
    fn in_store() -> crate::eval::Settings {
        crate::eval::Settings {
            store_dir: Some("/nix/store".to_owned()),
            ..crate::eval::Settings::default()
        }
    }

    /// A fresh on-disk evaluation cache for one test.
    fn scratch_store(label: &str) -> crate::store::Store {
        let dir = std::env::temp_dir().join(format!(
            "ixe-{label}-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        crate::store::Store::open(&dir).expect("a store under a fresh directory")
    }

    /// A filtered copy of `root/src` accepting `files` (paths relative to the
    /// root), the shape `builtins.path { filter = ...; }` asks in.
    fn filtered_copy(
        root: Rc<crate::value2::PathValue>,
        files: &[&str],
    ) -> crate::task::FilteredCopy {
        let accepted = files
            .iter()
            .map(|file| crate::task::AcceptedPath {
                path: format!("{}/{file}", root.path),
                file_type: FileType::Regular,
            })
            .collect();
        crate::task::FilteredCopy {
            root,
            name: "src".to_owned(),
            method: crate::task::PathMethod::NixArchive,
            accepted: Some(accepted),
            expected_sha256: None,
            inherit_references: false,
        }
    }

    /// A filtered copy under a sealed store object walks once per process
    /// lifetime of the cache, not once per evaluation: the second ask in the
    /// same recorder and the first ask of a fresh recorder over the same
    /// store are both served, the store is asked only whether the remembered
    /// path is still valid, and the object's sealing is asked once ever
    /// (`DirSealed`). The recording is the same row whichever answered. A
    /// different accepted set is a different question and walks.
    #[test]
    fn a_filtered_copy_under_a_sealed_object_walks_once_and_is_then_served() {
        let store = scratch_store("copy-memo");
        let memo = || store.copy_memo(Some("/nix/store".to_owned()));
        let root = crate::value2::ambient_path(format!("/nix/store/{OBJECT}/src"));
        let request = filtered_copy(Rc::clone(&root), &["a.nix", "b.nix"]);

        let sealing = Sealing::of(&[OBJECT]);
        let recorder = RecordingHost::new(&sealing).with_copy_memo(memo());
        assert_eq!(
            recorder.store_filtered(&request).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            (sealing.copied.get(), sealing.asked_sealed.get()),
            (1, 1),
            "the first ask walks"
        );
        assert!(
            sealing.allowed.borrow().is_empty(),
            "a walked copy is allowed by the embedder's own hook, not the memo"
        );
        assert_eq!(
            recorder.store_filtered(&request).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            (sealing.copied.get(), sealing.asked_sealed.get()),
            (1, 1),
            "the repeat is served"
        );
        assert_eq!(
            *sealing.allowed.borrow(),
            vec![copied_to("src")],
            "and the served copy is allowed, as the walk would have left it (reads under it follow)"
        );
        let rows = recorder.take();
        assert_eq!(
            rows.len(),
            1,
            "the served ask recorded the same row as the walked one, which the recorder dedupes to one"
        );
        assert_eq!(
            rows.entries()[0].1,
            Recorded::Answer(copied_to("src")),
            "a filtered copy's row keeps its answer, served or walked"
        );

        let later = Sealing::of(&[OBJECT]);
        let recorder = RecordingHost::new(&later).with_copy_memo(memo());
        assert_eq!(
            recorder.store_filtered(&request).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            (later.copied.get(), later.asked_sealed.get()),
            (0, 0),
            "a fresh recorder over the same cache is served without a walk or a sealing question"
        );
        let other = filtered_copy(root, &["a.nix"]);
        assert_eq!(
            recorder.store_filtered(&other).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            later.copied.get(),
            1,
            "another accepted set is another question"
        );
    }

    /// A remembered copy the embedder will not allow is not served: the walk
    /// happens, and its own hook allows. Serving without the allow was the
    /// first edit-arm failure under this memo (pure evaluation refused the
    /// next read under a served source).
    #[test]
    fn a_filtered_copy_the_host_will_not_allow_is_walked() {
        let store = scratch_store("copy-memo-allow-refused");
        let request = filtered_copy(Rc::new(mounted(&format!("{MOUNT}/src"))), &["a.nix"]);
        let memo = || store.copy_memo(Some("/nix/store".to_owned()));
        let first = Sealing::of(&[]);
        assert_eq!(
            RecordingHost::new(&first)
                .with_copy_memo(memo())
                .store_filtered(&request)
                .expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            first.copied.get(),
            1,
            "the first ask walks and is remembered"
        );

        let refusing = Sealing::of(&[]);
        refusing.refuse_allow.set(true);
        let recorder = RecordingHost::new(&refusing).with_copy_memo(memo());
        assert_eq!(
            recorder.store_filtered(&request).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            refusing.copied.get(),
            1,
            "the remembered copy could not be allowed, so it walked"
        );
        assert!(refusing.allowed.borrow().is_empty());
    }

    /// A mounted root is immutable by construction (the mount's store path
    /// name fixes the bytes), so it is served without any sealing question.
    #[test]
    fn a_filtered_copy_under_a_mounted_root_is_served_without_a_sealing_question() {
        let store = scratch_store("copy-memo-mounted");
        let request = filtered_copy(Rc::new(mounted(&format!("{MOUNT}/src"))), &["a.nix"]);
        let sealing = Sealing::of(&[]);
        let recorder = RecordingHost::new(&sealing)
            .with_copy_memo(store.copy_memo(Some("/nix/store".to_owned())));
        assert_eq!(
            recorder.store_filtered(&request).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            recorder.store_filtered(&request).expect("copied"),
            copied_to("src")
        );
        assert_eq!((sealing.copied.get(), sealing.asked_sealed.get()), (1, 0));
    }

    /// A root whose bytes could change under its spelling -- an ambient path
    /// outside the store, or inside an object the store does not call sealed
    /// -- is neither served nor remembered: every ask walks. The unsealed
    /// answer is not recorded (it is not a fact), only remembered for the
    /// handle (`DirSealed::decline`), so the question is asked once per
    /// handle rather than once per copy.
    #[test]
    fn a_filtered_copy_under_a_mutable_root_always_walks() {
        let store = scratch_store("copy-memo-mutable");
        let memo = || store.copy_memo(Some("/nix/store".to_owned()));
        let unsealed = Sealing::of(&[]);
        let recorder = RecordingHost::new(&unsealed).with_copy_memo(memo());
        let in_object = filtered_copy(
            crate::value2::ambient_path(format!("/nix/store/{OBJECT}/src")),
            &["a.nix"],
        );
        assert_eq!(
            recorder.store_filtered(&in_object).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            recorder.store_filtered(&in_object).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            (unsealed.copied.get(), unsealed.asked_sealed.get()),
            (2, 1),
            "an unsealed object walks every time and is asked about once per handle"
        );

        let outside = filtered_copy(crate::value2::ambient_path("/home/someone/src"), &["a.nix"]);
        assert_eq!(
            recorder.store_filtered(&outside).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            recorder.store_filtered(&outside).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            (unsealed.copied.get(), unsealed.asked_sealed.get()),
            (4, 1),
            "outside the store: no sealing question, every ask walks"
        );

        let sealed_now = Sealing::of(&[OBJECT]);
        let recorder = RecordingHost::new(&sealed_now).with_copy_memo(memo());
        assert_eq!(
            recorder.store_filtered(&in_object).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            sealed_now.copied.get(),
            1,
            "nothing was remembered while the object was unsealed"
        );
        // `memo()` shares the store's one `DirSealed`, as every question in a
        // process does, so the decline is still in force: the object the host
        // would now call sealed is not asked about, and the copy walks (the
        // correct answer, not the fastest) until a record is inserted or the
        // process ends.
        assert_eq!(
            sealed_now.asked_sealed.get(),
            0,
            "declined for the handle: not asked again"
        );
    }

    /// A remembered path the store no longer holds is a miss: the copy walks
    /// again and the entry is rewritten, so the next ask is served.
    #[test]
    fn a_remembered_copy_the_store_lost_is_walked_again_and_remembered_again() {
        let store = scratch_store("copy-memo-lost");
        let memo = || store.copy_memo(Some("/nix/store".to_owned()));
        let request = filtered_copy(
            crate::value2::ambient_path(format!("/nix/store/{OBJECT}/src")),
            &["a.nix"],
        );

        let first = Sealing::of(&[OBJECT]);
        assert_eq!(
            RecordingHost::new(&first)
                .with_copy_memo(memo())
                .store_filtered(&request)
                .expect("copied"),
            copied_to("src")
        );
        assert_eq!(first.copied.get(), 1);

        let lost_path = copied_to("src");
        let lost = Sealing::of(&[OBJECT]).absent(&[lost_path.as_str()]);
        let recorder = RecordingHost::new(&lost).with_copy_memo(memo());
        assert_eq!(
            recorder.store_filtered(&request).expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            lost.copied.get(),
            1,
            "the store does not hold the remembered path: walked"
        );

        let held = Sealing::of(&[OBJECT]);
        assert_eq!(
            RecordingHost::new(&held)
                .with_copy_memo(memo())
                .store_filtered(&request)
                .expect("copied"),
            copied_to("src")
        );
        assert_eq!(held.copied.get(), 0, "the walk rewrote the entry");
    }

    /// The memo's entries carry their own digest like `sealed`'s: a body that
    /// does not match, an empty body, or a body of two lines is no entry.
    #[test]
    fn a_copy_entry_that_fails_its_digest_or_shape_is_no_entry() {
        let dir = std::env::temp_dir().join(format!(
            "ixe-copies-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        let copies = DirCopies::open(&dir).expect("a copies record under a fresh directory");
        let key = digest(&[&b"k"[..]]);
        assert_eq!(copies.get(&key), None);
        copies.insert(&key, "/nix/store/x-src").expect("recorded");
        assert_eq!(copies.get(&key), Some("/nix/store/x-src".to_owned()));
        let path = dir.join(key.to_hex());
        let text = std::fs::read_to_string(&path).expect("the entry");
        let (digest_line, _) = text.split_once('\n').expect("a digest line");
        std::fs::write(&path, format!("{digest_line}\n/nix/store/y-src\n")).expect("tampered");
        assert_eq!(
            copies.get(&key),
            None,
            "a body that does not match its digest is no entry"
        );
        copies
            .records
            .insert(&key.to_hex(), "/nix/store/x-src\n/nix/store/y-src\n")
            .expect("two lines");
        assert_eq!(copies.get(&key), None, "two paths are no answer");
        copies.records.insert(&key.to_hex(), "").expect("empty");
        assert_eq!(copies.get(&key), None, "an empty body is no answer");
    }

    /// The trace line names its route and every cost field, whichever route
    /// answered, and prints `error` for a failed copy. A recorder without a
    /// memo is `nomemo`, not `mutable`: a reader could not tell the two
    /// apart, and only one of them says anything about the root.
    #[test]
    fn the_store_filtered_trace_line_names_its_route_and_every_cost_field() {
        let root = crate::value2::ambient_path(format!("/nix/store/{OBJECT}/src"));
        let request = filtered_copy(root, &["a.nix"]);
        let line = store_filtered_trace(
            "walk",
            &request,
            &CopyCost::default(),
            7,
            9,
            &Ok(copied_to("src")),
        );
        assert!(
            line.starts_with(&format!(
                "ixe question: StoreFiltered served=walk root=/nix/store/{OBJECT}/src "
            )),
            "{line}"
        );
        for field in [
            "accepted=1",
            "method=NixArchive",
            "immutable_ns=0",
            "key_ns=0",
            "get_ns=0",
            "valid_ns=0",
            "allow_ns=0",
            "record_ns=7",
            "total_ns=9",
        ] {
            assert!(line.contains(field), "{line} lacks {field}");
        }
        assert!(
            line.ends_with(&format!("-> {}", copied_to("src"))),
            "{line}"
        );
        let failed = store_filtered_trace(
            "nomemo",
            &request,
            &CopyCost::default(),
            0,
            0,
            &Err(StoreError::NoStore),
        );
        assert!(
            failed.contains("served=nomemo") && failed.ends_with("-> error"),
            "{failed}"
        );
    }
    /// ENG-12541 for this memo: the same question against another store
    /// directory is another key, so a cache shared between two stores never
    /// serves one store's path to the other.
    #[test]
    fn the_copy_memo_key_carries_the_store_directory() {
        let store = scratch_store("copy-memo-store-dir");
        let request = filtered_copy(Rc::new(mounted(&format!("{MOUNT}/src"))), &["a.nix"]);
        let here = Sealing::of(&[]);
        assert_eq!(
            RecordingHost::new(&here)
                .with_copy_memo(store.copy_memo(Some("/nix/store".to_owned())))
                .store_filtered(&request)
                .expect("copied"),
            copied_to("src")
        );
        let elsewhere = Sealing::of(&[]);
        assert_eq!(
            RecordingHost::new(&elsewhere)
                .with_copy_memo(store.copy_memo(Some("/other/store".to_owned())))
                .store_filtered(&request)
                .expect("copied"),
            copied_to("src")
        );
        assert_eq!(
            (here.copied.get(), elsewhere.copied.get()),
            (1, 1),
            "each store directory walks its own copy"
        );
    }

    /// The leaver: pruning keeps the most recently used entries, by the
    /// modification time a hit refreshes; a fresh temporary is left alone
    /// and a stale one (a writer that died) is removed.
    #[test]
    fn pruning_the_copy_memo_keeps_the_most_recently_used_entries() {
        let dir = std::env::temp_dir().join(format!(
            "ixe-copies-prune-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        let copies = DirCopies::open(&dir).expect("a copies record under a fresh directory");
        let keys: Vec<Hash> = (0..5u8).map(|i| digest(&[&[i][..]])).collect();
        let epoch = std::time::SystemTime::UNIX_EPOCH;
        let age = |name: &str, seconds: u64| {
            std::fs::File::open(dir.join(name))
                .and_then(|file| file.set_modified(epoch + std::time::Duration::from_secs(seconds)))
                .expect("aged");
        };
        for (i, key) in keys.iter().enumerate() {
            copies
                .insert(key, &format!("/nix/store/{i}-src"))
                .expect("recorded");
            age(&key.to_hex(), 1000 + i as u64);
        }
        std::fs::write(dir.join(".tmp-9-9"), "half written").expect("a fresh temporary");
        std::fs::write(dir.join(".tmp-8-8"), "abandoned").expect("a stale temporary");
        age(".tmp-8-8", 1000);
        // A hit on the oldest makes it the newest.
        assert_eq!(copies.get(&keys[0]), Some("/nix/store/0-src".to_owned()));
        copies.records.prune_to(2);
        let kept: Vec<Option<String>> = keys.iter().map(|key| copies.get(key)).collect();
        assert_eq!(
            kept,
            [
                Some("/nix/store/0-src".to_owned()),
                None,
                None,
                None,
                Some("/nix/store/4-src".to_owned())
            ],
            "the touched entry and the newest survive"
        );
        assert!(
            dir.join(".tmp-9-9").exists(),
            "a temporary with a live writer is not the prune's to remove"
        );
        assert!(
            !dir.join(".tmp-8-8").exists(),
            "a temporary an hour old has no writer and goes"
        );
    }

    /// The leaver's triggers: opening a directory over its cap prunes it,
    /// whatever process left it so; and a handle prunes on its own insert
    /// cadence, so two directories in one process each get pruned.
    #[test]
    fn the_copy_memo_prunes_on_open_and_on_its_own_insert_cadence() {
        let fresh = |label: &str| {
            std::env::temp_dir().join(format!(
                "ixe-copies-{label}-{}-{}",
                std::process::id(),
                WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
            ))
        };
        let count = |dir: &std::path::Path| {
            std::fs::read_dir(dir)
                .expect("listed")
                .filter_map(Result::ok)
                .filter(|entry| !entry.file_name().to_string_lossy().starts_with(".tmp-"))
                .count()
        };
        let key = |i: u64| digest(&[&i.to_le_bytes()[..]]);

        // Over the cap with fewer inserts than the cadence, then reopened.
        let dir = fresh("open-prune");
        let uncapped = DirCopies::open_keeping(&dir, usize::MAX).expect("opened");
        for i in 0..10 {
            uncapped
                .insert(&key(i), "/nix/store/x-src")
                .expect("recorded");
        }
        assert_eq!(count(&dir), 10);
        drop(DirCopies::open_keeping(&dir, 4).expect("reopened"));
        assert_eq!(count(&dir), 4, "opening over the cap prunes to it");

        // Two handles, two directories, interleaved: each prunes on its own
        // 64th insert; the pruned directory holds at most its cap.
        let (dir_a, dir_b) = (fresh("cadence-a"), fresh("cadence-b"));
        let a = DirCopies::open_keeping(&dir_a, 3).expect("opened a");
        let b = DirCopies::open_keeping(&dir_b, 3).expect("opened b");
        for i in 0..COPY_MEMO_PRUNE_EVERY {
            a.insert(&key(i), "/nix/store/a-src").expect("recorded a");
            if i % 2 == 0 {
                b.insert(&key(i), "/nix/store/b-src").expect("recorded b");
            }
        }
        assert_eq!(
            count(&dir_a),
            3,
            "the 64th insert through a pruned a to its cap"
        );
        assert_eq!(count(&dir_b), 32, "b has had 32 inserts and no prune yet");
        for i in COPY_MEMO_PRUNE_EVERY..(COPY_MEMO_PRUNE_EVERY + 32) {
            b.insert(&key(i), "/nix/store/b-src").expect("recorded b");
        }
        assert_eq!(count(&dir_b), 3, "b's own 64th insert pruned b");
    }

    /// Every read under a mounted root folds into one `Tree` row where the
    /// first of them was; a read elsewhere stays; the folded rows replay to
    /// the folded key, which is not the unfolded one.
    #[test]
    fn reads_under_a_mounted_root_fold_into_one_tree_row() {
        let inner = fake();
        let recorder = RecordingHost::new(&inner);
        drop(recorder.read_dir(&mounted(MOUNT)));
        drop(recorder.read_file(&mounted(&format!("{MOUNT}/a.nix"))));
        drop(recorder.read_file(&crate::value2::ambient_path("/a")));
        drop(recorder.path_exists_checked(&mounted(&format!("{MOUNT}/b"))));
        let unfolded = recorder.take();
        assert_eq!(unfolded.len(), 4);

        let settings = crate::eval::Settings::default();
        let folded = unfolded.fold_trees(&inner, &settings, None);
        assert_eq!(
            questions(&folded.rows),
            [
                &Question::Tree(Rc::new(mounted(MOUNT))),
                &Question::ReadFile(crate::value2::ambient_path("/a")),
            ],
            "one tree row where the first mounted read was; the ambient read kept"
        );
        assert_eq!(folded.rows_folded, 2, "three mounted rows became one");
        assert_eq!(folded.rows[0].1, Recorded::Digest(digest_tree(true)));
        let again = ReadSet {
            entries: folded.rows.clone(),
            ..ReadSet::default()
        }
        .fold_trees(&inner, &settings, None);
        assert_eq!(
            again,
            Folded {
                rows: folded.rows.clone(),
                rows_folded: 0
            },
            "folding is idempotent"
        );

        let replay = replayed_rows(&folded.rows, &inner);
        let key = ReadSet::key_of(&folded.rows, &id(b"m"));
        assert_eq!(replay.key(&id(b"m")), key, "the tree row takes its record");
        assert_ne!(key, unfolded.key(&id(b"m")));
    }

    /// Reads under a sealed store object fold like reads under a mount; the
    /// tree row replays from its record while the store calls the object
    /// sealed, and is asked -- to a different answer -- once it does not.
    /// Under an object the store does not call sealed nothing folds.
    #[test]
    fn reads_under_a_sealed_object_fold_and_replay_while_it_stays_sealed() {
        let settings = in_store();
        let sealing = Sealing::of(&[OBJECT]);
        let recorder = RecordingHost::new(&sealing);
        drop(recorder.read_file(&crate::value2::ambient_path(format!(
            "/nix/store/{OBJECT}/default.nix"
        ))));
        drop(recorder.read_dir(&crate::value2::ambient_path(format!(
            "/nix/store/{OBJECT}/lib"
        ))));
        drop(recorder.read_file(&crate::value2::ambient_path("/etc/hosts")));
        let unfolded = recorder.take();

        let folded = unfolded.fold_trees(&sealing, &settings, None);
        assert_eq!(
            questions(&folded.rows),
            [
                &Question::Tree(crate::value2::ambient_path(format!("/nix/store/{OBJECT}"))),
                &Question::ReadFile(crate::value2::ambient_path("/etc/hosts")),
            ]
        );
        assert_eq!(
            folded.rows_folded, 1,
            "two rows under the object became one"
        );
        let key = ReadSet::key_of(&folded.rows, &id(b"m"));

        let cas = ix_kernel::cas::MemoryCas::new();
        let replay = ReadSet::replay(&folded.rows, &sealing, &settings, &cas)
            .expect("replay")
            .expect("not refused");
        assert_eq!(
            replay.key(&id(b"m")),
            key,
            "sealed: the tree row takes its record"
        );
        let streamed = ReadSet::replay_key_with(
            &folded.rows,
            &Sealing::of(&[OBJECT]),
            &settings,
            &cas,
            None,
            &id(b"m"),
        )
        .expect("streamed replay")
        .expect("allowed");
        assert_eq!(streamed, key, "streaming retains sealed row answers");

        let unsealed = Sealing::of(&[]);
        let replay = ReadSet::replay(&folded.rows, &unsealed, &settings, &cas)
            .expect("replay")
            .expect("not refused");
        assert_eq!(
            replay.entries()[0].1,
            Recorded::Digest(digest_tree(false)),
            "unsealed: the tree row is asked and answers the other constant"
        );
        assert_ne!(replay.key(&id(b"m")), key);

        let kept = unfolded.fold_trees(&unsealed, &settings, None);
        assert_eq!(
            kept.rows.as_slice(),
            unfolded.entries(),
            "nothing folds under an unsealed object"
        );
        assert_eq!(kept.rows_folded, 0);
    }

    /// `record` folds before writing: the witness holds the tree row, the
    /// hit is served through it, and the object losing its sealing is a miss.
    #[test]
    fn a_recorded_witness_holds_the_tree_row_and_serves_the_hit()
    -> Result<(), Box<dyn core::error::Error>> {
        let settings = in_store();
        let sealing = Sealing::of(&[OBJECT]);
        let cas = ix_kernel::cas::MemoryCas::new();
        let mut cache = ResultCache::new(&cas);
        let module = id(b"module");

        let recorder = RecordingHost::new(&sealing);
        drop(recorder.read_file(&crate::value2::ambient_path(format!(
            "/nix/store/{OBJECT}/default.nix"
        ))));
        drop(recorder.read_file(&crate::value2::ambient_path(format!(
            "/nix/store/{OBJECT}/lib/a.nix"
        ))));
        cache.record(
            &module,
            &recorder.take(),
            &result("v1"),
            &sealing,
            &settings,
        )?;
        assert_eq!(
            cache.witness.get(&module).map(|rows| questions(rows)),
            Some(vec![&Question::Tree(crate::value2::ambient_path(format!(
                "/nix/store/{OBJECT}"
            )))]),
            "the witness holds one tree row"
        );

        assert_eq!(
            cache.lookup(&module, &sealing, &settings),
            Some(result("v1"))
        );
        assert_eq!(
            cache.lookup(&module, &Sealing::of(&[]), &settings),
            None,
            "unsealed: a miss"
        );
        Ok(())
    }

    /// With a persistent store the fold records the sealing answer in
    /// `DirSealed`, so the first replay never asks `sealed_paths`; and an
    /// object the store has since lost still replays through that same
    /// memo: the sealing pins what any re-read could see, and a served
    /// result reads nothing from the object. (Presence was asked at the
    /// tree row until round 15, and a lazily mounted input, which the store
    /// never registers, missed in every process that had not mounted it.)
    #[test]
    fn a_persistent_fold_remembers_the_sealing_and_a_lost_object_still_replays()
    -> Result<(), Box<dyn core::error::Error>> {
        let dir = std::env::temp_dir().join(format!(
            "ixe-tree-fold-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        let store = crate::store::Store::open(&dir)?;
        let settings = in_store();
        let module = id(b"module");
        let sealing = Sealing::of(&[OBJECT]);
        let recorder = RecordingHost::new(&sealing);
        drop(recorder.read_file(&crate::value2::ambient_path(format!(
            "/nix/store/{OBJECT}/default.nix"
        ))));
        ResultCache::persistent(&store).record(
            &module,
            &recorder.take(),
            &result("v1"),
            &sealing,
            &settings,
        )?;
        assert_eq!(
            sealing.asked_sealed.get(),
            1,
            "the fold asked about the object once"
        );
        assert_eq!(
            store.sealed().get(OBJECT),
            Some(Vec::new()),
            "the fold recorded the sealing"
        );

        let still = Sealing::of(&[OBJECT]);
        assert_eq!(
            ResultCache::persistent(&store).lookup(&module, &still, &settings),
            Some(result("v1"))
        );
        assert_eq!(
            still.asked_sealed.get(),
            0,
            "the replay took the recorded sealing"
        );

        let lost = Sealing::of(&[OBJECT]).absent(&[&format!("/nix/store/{OBJECT}")]);
        assert_eq!(
            ResultCache::persistent(&store).lookup(&module, &lost, &settings),
            Some(result("v1")),
            "the object is gone: the sealed read still replays from its record"
        );
        assert_eq!(lost.asked_sealed.get(), 0, "and asked no host for it");
        drop(std::fs::remove_dir_all(&dir));
        Ok(())
    }

    /// A host with a lazily mounted flake input: a fetch mounts its tree at
    /// its store-path name for the rest of the process, and the store holds
    /// the object and calls it sealed only from then on (`rustSealedPaths`
    /// walks a mount or a valid content-addressed object, nothing else;
    /// `rustValidPaths` counts a mount as held). Counts the fetches, the
    /// copies and the validity questions it is asked.
    struct Mounting {
        held: std::cell::RefCell<Vec<String>>,
        fetches: Cell<usize>,
        copies: Cell<usize>,
        validity: Cell<usize>,
    }
    impl Mounting {
        fn new() -> Self {
            Self {
                held: std::cell::RefCell::new(Vec::new()),
                fetches: Cell::new(0),
                copies: Cell::new(0),
                validity: Cell::new(0),
            }
        }
    }
    impl Host for Mounting {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        crate::host::host_stubs!(
            store_text,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            not_async,
            ensure_path,
        );
        crate::host::host_stubs!(
            read_file_bytes,
            file_type_resolved,
            get_env,
            warn,
            find_file,
            nix_path,
            trace
        );
        fn read_file(&self, path: &crate::value2::PathValue) -> Result<String, String> {
            Ok(format!("contents of {}", path.path))
        }
        /// `ensureLazyPathCopied` on a tree's root: the object enters the
        /// store at the path it was mounted at.
        fn copy_to_store(&self, path: &crate::value2::PathValue) -> Result<String, StoreError> {
            self.copies.set(self.copies.get() + 1);
            let object = path.path.to_string();
            self.held.borrow_mut().push(object.clone());
            Ok(object)
        }
        fn read_dir(
            &self,
            _p: &crate::value2::PathValue,
        ) -> Result<Vec<(String, FileType)>, String> {
            Ok(vec![("a".to_owned(), FileType::Regular)])
        }
        fn path_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
            Ok(true)
        }
        fn dir_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
            Ok(true)
        }
        fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
            Ok(Some(FileType::Regular))
        }
        /// `realiseContextCheck` on opaque paths: nothing to build while the
        /// path is mounted, an invalid-path error otherwise.
        fn realise(
            &self,
            context: &[crate::value2::ContextElem],
        ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
            for elem in context {
                let crate::value2::ContextElem::Opaque(path) = elem else {
                    return Err(StoreError::Failed("only opaque paths here".to_owned()));
                };
                if !self.held.borrow().iter().any(|held| held == path.as_ref()) {
                    return Err(StoreError::Failed(format!("path '{path}' is not valid")));
                }
            }
            Ok(std::collections::BTreeMap::new())
        }
        /// The answer `emitTreeAttrs` builds for the tree, mounted at its
        /// store path.
        fn fetch_tree(
            &self,
            _request: &crate::task::FetchTreeRequest,
        ) -> Result<String, StoreError> {
            self.fetches.set(self.fetches.get() + 1);
            let object = format!("/nix/store/{OBJECT}");
            self.held.borrow_mut().push(object.clone());
            Ok(format!(
                "{{\"narHash\":\"sha256-AAAA\",\"outPath\":\"{object}\"}}"
            ))
        }
        fn sealed_paths(&self, objects: &[String]) -> Result<crate::host::Links, StoreError> {
            Ok(objects
                .iter()
                .filter(|object| self.held.borrow().contains(&format!("/nix/store/{object}")))
                .map(|object| (object.clone(), Vec::new()))
                .collect())
        }
        fn valid_paths(
            &self,
            paths: &[String],
        ) -> Result<std::collections::BTreeSet<String>, StoreError> {
            self.validity.set(self.validity.get() + 1);
            Ok(paths
                .iter()
                .filter(|path| self.held.borrow().contains(path))
                .cloned()
                .collect())
        }
        /// A served fetch ends in `allowPath`, as the bridge's does.
        fn allow_paths(&self, _paths: &[String]) -> Result<(), StoreError> {
            Ok(())
        }
    }

    /// A locked final tree is its record. The witness has the fetch of a
    /// final tree with a `narHash` (a flake input: `fetchFinalTree` over the
    /// locked attributes), a read under the tree it mounted, and a
    /// realisation of the tree's path (`realiseContextCheck` has nothing to
    /// build for an opaque path while it is mounted). A new process holds
    /// nothing: the fetch row replays from its record without fetching, the
    /// tree row from the sealing the fold remembered, and the realisation is
    /// answered by validity because it stands on the mount the served fetch
    /// would have left; the one presence question is the batch for the
    /// realisation's path, answered absent, with no late question after it.
    /// (Round 15: the four crane inputs of the home-manager witness, lazily
    /// mounted and never registered with the store, were fetched again on
    /// every hit, 1.6s of a 6.5s warm run; served without the mount, their
    /// realisations errored and the run missed.) The mount is not the store
    /// holding the object: a copy of the tree's root, which names the same
    /// path, asks (the asked copy would have put the object in the store;
    /// the served fetch did not). The other controls: the same tree through
    /// `fetchTree`, whose `ref` may move, is asked; so is a final tree
    /// without a `narHash`.
    #[test]
    fn a_locked_final_tree_replays_from_its_record_without_fetching()
    -> Result<(), Box<dyn core::error::Error>> {
        let dir = std::env::temp_dir().join(format!(
            "ixe-tree-final-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        let store = crate::store::Store::open(&dir)?;
        let settings = in_store();
        let object = format!("/nix/store/{OBJECT}");
        let request = |fetcher: crate::task::TreeFetcher, locked: bool| {
            let mut attrs = std::collections::BTreeMap::new();
            attrs.insert(
                "type".to_owned(),
                crate::task::TreeAttr::Str("github".to_owned()),
            );
            if locked {
                attrs.insert(
                    "narHash".to_owned(),
                    crate::task::TreeAttr::Str("sha256-AAAA".to_owned()),
                );
            }
            crate::task::FetchTreeRequest { attrs, fetcher }
        };
        let record = |module: &EvalId,
                      request: &crate::task::FetchTreeRequest|
         -> Result<(), Box<dyn core::error::Error>> {
            let mounting = Mounting::new();
            let recorder = RecordingHost::new(&mounting);
            drop(recorder.fetch_tree(request));
            drop(recorder.read_file(&crate::value2::ambient_path(format!(
                "{object}/default.nix"
            ))));
            assert_eq!(
                recorder.realise(&[crate::value2::ContextElem::Opaque(object.as_str().into())]),
                Ok(std::collections::BTreeMap::new()),
                "mounted: nothing to build"
            );
            ResultCache::persistent(&store).record(
                module,
                &recorder.take(),
                &result("v1"),
                &mounting,
                &settings,
            )?;
            Ok(())
        };

        let locked_final = id(b"locked final");
        record(
            &locked_final,
            &request(crate::task::TreeFetcher::FinalTree, true),
        )?;
        assert_eq!(
            store.sealed().get(OBJECT),
            Some(Vec::new()),
            "the fold recorded the sealing through the mount"
        );
        let WitnessLookup::Found(rows) = store.witness().get(&locked_final) else {
            panic!("the witness was not written");
        };
        assert!(
            matches!(
                rows.as_slice(),
                [
                    (Question::FetchTree(_), Recorded::Answer(_)),
                    (Question::Tree(_), _),
                    (Question::Realise(_), _)
                ]
            ),
            "the fetch row with its answer kept, one tree row for the read under the object, the realisation: {rows:?}"
        );

        // A new process: nothing is mounted, the store holds nothing.
        let fresh = Mounting::new();
        assert_eq!(
            ResultCache::persistent(&store).lookup(&locked_final, &fresh, &settings),
            Some(result("v1"))
        );
        assert_eq!(fresh.fetches.get(), 0, "the fetch row took its record");
        assert_eq!(
            fresh.validity.get(),
            1,
            "the batch for the realisation's path; the realisation stood on the served fetch's mount, so no late question"
        );

        let copied = id(b"copied tree");
        {
            let mounting = Mounting::new();
            let recorder = RecordingHost::new(&mounting);
            drop(recorder.fetch_tree(&request(crate::task::TreeFetcher::FinalTree, true)));
            let root = crate::value2::PathValue::new(
                crate::value2::Root::mounted(object.as_str()),
                object.clone(),
            );
            assert_eq!(
                recorder.copy_to_store(&root),
                Ok(object.clone()),
                "the tree's root copies to its own path"
            );
            ResultCache::persistent(&store).record(
                &copied,
                &recorder.take(),
                &result("v1"),
                &mounting,
                &settings,
            )?;
        }
        let fresh = Mounting::new();
        assert_eq!(
            ResultCache::persistent(&store).lookup(&copied, &fresh, &settings),
            Some(result("v1"))
        );
        assert_eq!(fresh.fetches.get(), 0, "the fetch row took its record");
        assert_eq!(
            fresh.copies.get(),
            1,
            "the copy row asked: the store held no object at the path the served fetch handed out"
        );
        assert_eq!(
            fresh.validity.get(),
            2,
            "the batch, then the copy's path asked again at its row"
        );

        let moving = id(b"fetchTree");
        record(&moving, &request(crate::task::TreeFetcher::Tree, true))?;
        let asked = Mounting::new();
        assert_eq!(
            ResultCache::persistent(&store).lookup(&moving, &asked, &settings),
            Some(result("v1")),
            "the fetch answered the same: a hit"
        );
        assert_eq!(
            asked.fetches.get(),
            1,
            "a `fetchTree` row asks: its `ref` may have moved"
        );

        let unlocked = id(b"final unlocked");
        record(
            &unlocked,
            &request(crate::task::TreeFetcher::FinalTree, false),
        )?;
        let asked = Mounting::new();
        assert_eq!(
            ResultCache::persistent(&store).lookup(&unlocked, &asked, &settings),
            Some(result("v1"))
        );
        assert_eq!(
            asked.fetches.get(),
            1,
            "a final tree without a `narHash` asks"
        );
        drop(std::fs::remove_dir_all(&dir));
        Ok(())
    }

    /// A tree row under a mounted root carries the mount point as its root
    /// and survives the witness codec with it; the ambient root is the
    /// `one_of_each` sample's.
    #[test]
    fn a_mounted_tree_row_survives_the_witness_codec() {
        let question = Question::Tree(Rc::new(mounted(MOUNT)));
        assert_eq!(
            question_from(&question_value(&question)).as_ref(),
            Some(&question)
        );
    }

    /// The purity table judges a tree row as it judges every read it stands
    /// for, under both read policies: a replay refused for the one is
    /// refused for the others, never one without the other.
    #[test]
    fn a_tree_row_gets_the_purity_verdict_of_the_reads_it_stands_for() {
        let path = Rc::new(mounted(&format!("{MOUNT}/a.nix")));
        let tree = Question::Tree(Rc::new(mounted(MOUNT)));
        let reads = [
            Question::Import(path.clone()),
            Question::ReadFile(path.clone()),
            Question::ReadFileBytes(path.clone()),
            Question::ReadDir(path.clone()),
            Question::PathExists(path.clone()),
            Question::DirExists(path.clone()),
            Question::FileType(path.clone()),
            Question::FileTypeResolved(path.clone()),
        ];
        for path_reads in [
            crate::purity::PathReads::Direct,
            crate::purity::PathReads::ThroughEmbedder,
        ] {
            let settings = crate::eval::Settings {
                pure_eval: true,
                path_reads,
                ..crate::eval::Settings::default()
            };
            let asks = |question: &Question| {
                matches!(
                    crate::purity::verdict(
                        &question.as_need_path(),
                        settings.purity(),
                        settings.path_reads
                    ),
                    crate::purity::Verdict::Ask
                )
            };
            for read in &reads {
                assert_eq!(
                    asks(read),
                    asks(&tree),
                    "{read:?} and the tree row are judged apart"
                );
            }
        }
    }

    /// The list of samples covers the enum, and the compiler makes it stay
    /// that way. See [`Question::variant_index`] for the chain this is step 2
    /// and 3 of.
    #[test]
    fn every_question_variant_is_listed() {
        let all = Question::one_of_each();
        let mut seen = [false; Question::VARIANT_COUNT];
        for question in &all {
            let index = question.variant_index();
            assert!(
                index < Question::VARIANT_COUNT,
                "{question:?} has index {index}, past VARIANT_COUNT \
                 {}: raise the count and add a sample below it",
                Question::VARIANT_COUNT
            );
            let Some(slot) = seen.get_mut(index) else {
                // Unreachable given the bound asserted above; written as a
                // refusal rather than an index because the workspace denies
                // `indexing_slicing`, tests included.
                unreachable!("variant index {index} is out of range");
            };
            assert!(
                !*slot,
                "two samples share variant index {index}; one variant is \
                 covered twice and another not at all"
            );
            *slot = true;
        }
        let missing: Vec<usize> = seen
            .iter()
            .enumerate()
            .filter_map(|(i, hit)| (!hit).then_some(i))
            .collect();
        assert!(
            missing.is_empty(),
            "variant indexes {missing:?} have no sample in one_of_each, so \
             nothing round-trips them through the witness codec"
        );
    }

    #[test]
    fn answer_digests_keep_the_v1_domain() {
        let parts: &[&[u8]] = &[b"file-ok", b"contents"];
        assert_eq!(digest(parts), hash::tagged("ixe-read-v1", parts));
    }

    #[test]
    fn evaluation_keys_use_the_v3_domain() {
        let read_set = ReadSet::default();
        let identity = id(b"module");
        let parts = [identity.as_hash().as_bytes().as_slice()];
        assert_eq!(
            read_set.key(&identity),
            hash::tagged("ixe-eval-result-v3", &parts)
        );
        assert_ne!(
            read_set.key(&identity),
            hash::tagged("ixe-eval-result-v2", &parts)
        );
    }

    /// ENG-12543. Replaying a witness must not do the reads `pure-eval` and
    /// `restrict-eval` forbid.
    ///
    /// `Question::ask` calls the host directly and the evaluator's access
    /// check lives in `eval::answer_path`, which replay never enters, so this
    /// path had none. Measured at 8065be845, before the settings reached the
    /// memo key, a cache filled with reads allowed and then looked up under
    /// `pure-eval` returned `status=ok value="secret" memo_hit=true
    /// reads=["/etc/shadow"]` -- the setting bypassed in both directions.
    #[test]
    fn replay_refuses_to_read_when_access_is_off() {
        struct Counting {
            reads: RefCell<Vec<String>>,
        }
        impl Host for Counting {
            crate::host::host_stubs!(settle);
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
                copy_to_store,
                ensure_path,
                warn,
                trace,
                find_file,
                nix_path
            );
            fn read_file(&self, path: &crate::value2::PathValue) -> Result<String, String> {
                self.reads.borrow_mut().push(path.to_string());
                Ok("secret".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok({
                    self.reads.borrow_mut().push(format!("exists {path}"));
                    true
                })
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
            fn get_env(&self, name: &str) -> Option<String> {
                self.reads.borrow_mut().push(format!("env {name}"));
                Some("v".to_owned())
            }
        }

        let host = Counting {
            reads: RefCell::new(Vec::new()),
        };
        let questions = vec![
            Question::ReadFile(crate::value2::ambient_path("/etc/shadow")),
            Question::GetEnv("SECRET".to_owned()),
            Question::PathExists(crate::value2::ambient_path("/private")),
        ];

        // Stated, not installed. This used to set the process-global
        // `pure-eval` and put it back, under a write guard, which made every
        // unguarded reader in the suite race with it (ENG-12939).
        let pure = crate::eval::Settings {
            pure_eval: true,
            ..crate::eval::Settings::default()
        };
        let cas = ix_kernel::cas::MemoryCas::new();
        let refused = ReadSet::replay(&rows_of(&questions), &host, &pure, &cas);
        let read_with_access_off = host.reads.borrow().clone();

        assert!(
            read_with_access_off.is_empty(),
            "replay reached the world with access off: {read_with_access_off:?}"
        );
        assert!(
            matches!(refused, Ok(None)),
            "replay must refuse rather than return an empty read set, which \
             would key as though the questions had been asked and answered"
        );

        // And with access on it still works, so the guard is a refusal and
        // not a permanent disabling of replay.
        assert!(matches!(
            ReadSet::replay(
                &rows_of(&questions),
                &host,
                &crate::eval::Settings::default(),
                &cas
            ),
            Ok(Some(_))
        ));
        assert_eq!(host.reads.borrow().len(), 3);
    }

    /// ENG-12792, the other direction. With the embedder's read hooks
    /// installed the same three questions replay under `pure-eval`, because
    /// the reads then go through cppnix's `rootFS` and the setting is
    /// enforced there.
    ///
    /// Without this the change would be invisible here: the test above
    /// asserts a refusal and would keep passing if the six rows never moved.
    /// Two tests, one per configuration, is what says the table is a
    /// decision rather than a constant.
    ///
    /// The fixture host is a leaf and not `RealFs`, so the hooks do not
    /// change what it answers -- only whether replay is allowed to ask. That
    /// split is the point being tested. In the `nix` binary the two are the
    /// same object: the host chain ends in `RealFs`, which is where the hooks
    /// are read.
    #[test]
    fn replay_reads_again_when_the_embedder_answers() {
        struct Counting {
            reads: RefCell<Vec<String>>,
        }
        impl Host for Counting {
            crate::host::host_stubs!(settle);
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
                copy_to_store,
                ensure_path,
                warn,
                trace,
                find_file,
                nix_path
            );
            fn read_file(&self, path: &crate::value2::PathValue) -> Result<String, String> {
                self.reads.borrow_mut().push(path.to_string());
                Ok("from the accessor".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok({
                    self.reads.borrow_mut().push(format!("exists {path}"));
                    true
                })
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
                path: &crate::value2::PathValue,
            ) -> Result<Option<FileType>, String> {
                self.reads.borrow_mut().push(format!("kind {path}"));
                Ok(Some(FileType::Regular))
            }
            fn get_env(&self, _name: &str) -> Option<String> {
                None
            }
        }

        let host = Counting {
            reads: RefCell::new(Vec::new()),
        };
        let questions = vec![
            Question::ReadFile(crate::value2::ambient_path(
                "/nix/store/aaa-source/flake.nix",
            )),
            Question::PathExists(crate::value2::ambient_path("/nix/store/aaa-source")),
            Question::FileType(crate::value2::ambient_path("/nix/store/aaa-source")),
        ];

        // Two configurations named as values. This test used to install real
        // read hooks and clear them again purely to move
        // `PathReads::current()`, which meant a fake filesystem was briefly
        // visible to every other test in the process; now that `path_reads`
        // is a field of `Settings` there is nothing to install (ENG-12939).
        let standalone_settings = crate::eval::Settings {
            pure_eval: true,
            path_reads: crate::purity::PathReads::Direct,
            ..crate::eval::Settings::default()
        };
        let bridged_settings = crate::eval::Settings {
            path_reads: crate::purity::PathReads::ThroughEmbedder,
            ..standalone_settings.clone()
        };

        let cas = ix_kernel::cas::MemoryCas::new();
        let standalone = ReadSet::replay(&rows_of(&questions), &host, &standalone_settings, &cas);
        let bridged = ReadSet::replay(&rows_of(&questions), &host, &bridged_settings, &cas);
        let asked = host.reads.borrow().clone();

        assert!(
            matches!(standalone, Ok(None)),
            "a std::fs read set must not replay under pure-eval"
        );
        assert!(
            matches!(bridged, Ok(Some(_))),
            "with the embedder's read hooks installed these three go through \
             cppnix's rootFS, so replay has to ask them; refusing is the \
             pre-ENG-12792 behaviour and makes every flake witness unusable"
        );
        assert_eq!(asked.len(), 3, "replay asked {asked:?}");
    }

    /// A witness that names nothing still replays, so a pure expression keeps
    /// its cache under `pure-eval` -- the one case where caching is
    /// unambiguously safe. A guard that refused this would trade a real
    /// speedup for no safety.
    #[test]
    fn an_empty_witness_still_replays_with_access_off() {
        struct Nothing;
        impl Host for Nothing {
            crate::host::host_stubs!(settle);
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
                warn,
                trace,
                find_file,
                nix_path
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
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
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
        }

        let pure = crate::eval::Settings {
            pure_eval: true,
            ..crate::eval::Settings::default()
        };
        let replayed = ReadSet::replay(&[], &Nothing, &pure, &ix_kernel::cas::MemoryCas::new());
        assert!(matches!(replayed, Ok(Some(set)) if set.is_empty()));
    }

    /// The property that stops the `CopyToStore` bug recurring in a new
    /// variant's name.
    ///
    /// Every variant, not a list somebody maintains: `one_of_each` is
    /// generated from the same declaration the codec is, so a variant added
    /// tomorrow is covered here the moment it exists. What this would have
    /// caught is exactly what happened -- `CopyToStore` encoded to tag 6 and
    /// decoded to `None`, so every witness naming one was unreadable and
    /// every evaluation containing `"${./x}"` re-evaluated for ever.
    #[test]
    fn every_question_variant_round_trips_through_the_witness_codec() {
        let all = Question::one_of_each();
        assert!(all.len() >= 7, "the enum shrank: {all:?}");
        for question in &all {
            let encoded = question_value(question);
            let decoded = question_from(&encoded);
            assert_eq!(
                decoded.as_ref(),
                Some(question),
                "{question:?} does not survive the witness codec, so it can never cache-hit"
            );
        }
    }

    fn legacy_witness(module: &Hash) -> Vec<u8> {
        let legacy = CanonValue::map([
            ("module", CanonValue::Bytes(module.as_bytes().to_vec())),
            (
                "questions",
                CanonValue::array([CanonValue::array([
                    CanonValue::int(10),
                    CanonValue::str("legacy.drv"),
                    CanonValue::str("Derive([])"),
                ])]),
            ),
        ]);
        canon::encode(&legacy).expect("the legacy fixture is canonical")
    }

    /// A witness from before the format marker used tag 10 for derivation
    /// writes. The current question decoder assigns tag 10 to `StoreText`, so
    /// accepting an unmarked document would replay the wrong host effect.
    #[test]
    fn an_unversioned_legacy_witness_fails_before_question_decoding() {
        let module = hash::tagged("legacy-module", &[b"unchanged source"]);

        assert_eq!(
            witness_rows(&legacy_witness(&module)),
            None,
            "an unversioned tag 10 row was reinterpreted as StoreText"
        );
    }

    /// Two variants sharing a tag would make one decode as the other, which
    /// is a wrong question replayed rather than a miss.
    #[test]
    fn no_two_question_variants_share_a_tag() {
        let all = Question::one_of_each();
        let mut tags: Vec<u8> = all.iter().map(Question::tag).collect();
        tags.sort_unstable();
        let mut unique = tags.clone();
        unique.dedup();
        assert_eq!(tags, unique, "duplicate question tags: {tags:?}");
        assert!(!tags.contains(&0), "tag 0 is reserved for 'absent'");
    }

    /// A whole witness, not one question at a time: the list codec has its own
    /// failure mode, where one unreadable entry discards the entire list.
    #[test]
    fn a_witness_naming_every_question_reads_back() -> Result<(), Box<dyn core::error::Error>> {
        let dir = std::env::temp_dir().join(format!(
            "ixe-witness-roundtrip-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        let store = DirWitness::open(&dir)?;
        let rows: Vec<WitnessRow> = Question::one_of_each()
            .into_iter()
            .enumerate()
            .map(|(i, question)| {
                (
                    question,
                    Recorded::Digest(digest(&[b"answer", &[u8::try_from(i).unwrap_or(u8::MAX)]])),
                )
            })
            .collect();
        let identity = id(b"every-question");
        store.put(&identity, &rows)?;
        let back = store.get(&identity);
        std::fs::remove_dir_all(&dir)?;
        assert_eq!(back, WitnessLookup::Found(rows));
        Ok(())
    }

    /// ENG-12540 (2). `RecordingHost` implemented six of `Host`'s eight
    /// methods and inherited the other two from the trait, so `ensure_path`
    /// answered "no store here" against a host that had one, and every
    /// warning was dropped.
    ///
    /// # What this still guards, now that the trait has no defaults
    ///
    /// Presence is no longer this test's job. Since ENG-13107 every effect on
    /// `Host` is bodiless, so a recorder that forgets one does not compile,
    /// and the sibling test that used to hold `ThreadedHost` and `&T` to a
    /// hand-maintained list of method names is gone.
    ///
    /// What survives is the half the compiler cannot check. This wrapper does
    /// not merely forward: every method here has a real body that records the
    /// question and *then* asks the inner host, and a body that records
    /// without asking compiles perfectly. That is a plausible mistake -- it
    /// is what a recorder does for a question it can answer from its own log
    /// -- and it would strand the effect. So this drives each effect through
    /// the recorder and names the call it expects to arrive at the host
    /// behind it.
    #[test]
    fn the_recorder_forwards_every_effect_to_the_host_behind_it() {
        struct Inner {
            asked: RefCell<Vec<String>>,
        }
        impl Inner {
            fn note(&self, what: &str) {
                self.asked.borrow_mut().push(what.to_owned());
            }
        }
        impl Host for Inner {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(realise, lock_flake, not_async);
            crate::host::host_stubs!(file_type_resolved, find_file, nix_path);
            fn read_file(&self, path: &crate::value2::PathValue) -> Result<String, String> {
                self.note(&format!("read_file {path}"));
                Ok("text".to_owned())
            }
            fn read_dir(
                &self,
                path: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                self.note(&format!("read_dir {path}"));
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok({
                    self.note(&format!("path_exists {path}"));
                    true
                })
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
                path: &crate::value2::PathValue,
            ) -> Result<Option<FileType>, String> {
                self.note(&format!("file_type {path}"));
                Ok(Some(FileType::Regular))
            }
            fn get_env(&self, name: &str) -> Option<String> {
                self.note(&format!("get_env {name}"));
                Some("v".to_owned())
            }
            fn copy_to_store(&self, path: &crate::value2::PathValue) -> Result<String, StoreError> {
                self.note(&format!("copy_to_store {path}"));
                Ok("/nix/store/xyz".to_owned())
            }
            fn ensure_path(&self, path: &str) -> Result<(), StoreError> {
                self.note(&format!("ensure_path {path}"));
                Ok(())
            }

            fn store_text(
                &self,
                name: &str,
                _contents: &str,
                _references: &[String],
            ) -> Result<String, StoreError> {
                self.note(&format!("store_text {name}"));
                Ok("/nix/store/text".to_owned())
            }
            fn write_derivation(&self, name: &str, _aterm: &str) -> Result<String, StoreError> {
                self.note(&format!("write_derivation {name}"));
                Ok("/nix/store/a.drv".to_owned())
            }
            fn store_filtered(
                &self,
                request: &crate::task::FilteredCopy,
            ) -> Result<String, StoreError> {
                self.note(&format!("store_filtered {}", request.root));
                Ok("/nix/store/filtered".to_owned())
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
            fn warn(&self, message: &str) {
                self.note(&format!("warn {message}"));
            }
            fn trace(&self, message: &str) {
                self.note(&format!("trace {message}"));
            }
        }

        let inner = Inner {
            asked: RefCell::new(Vec::new()),
        };
        let host = RecordingHost::new(&inner);
        drop(host.read_file(&crate::value2::ambient_path("/f")));
        drop(host.read_dir(&crate::value2::ambient_path("/d")));
        let _ = host.path_exists_checked(&crate::value2::ambient_path("/e"));
        drop(host.file_type(&crate::value2::ambient_path("/t")));
        drop(host.get_env("V"));
        drop(host.copy_to_store(&crate::value2::ambient_path("/c")));
        // The two that were inherited. Both assertions are the divergence:
        // before the fix these answered `Err(NoStore)` and silence.
        assert_eq!(host.ensure_path("/p"), Ok(()));
        // The store effects. The compiler now insists the recorder define
        // each of these, so what is being checked below is not that they
        // exist but that each one's body reaches the inner host rather than
        // stopping at the log. `resolve_import` is the one method left with a
        // trait default and is deliberately not overridden -- it is derived
        // from `file_type_resolved`, which is forwarded and recorded, so
        // inheriting it records the same question either way.
        drop(host.store_text("t", "bytes", &[]));
        drop(host.write_derivation("a", "Derive([])"));
        drop(host.store_filtered(&crate::task::FilteredCopy {
            root: crate::value2::ambient_path("/s"),
            name: "s".to_owned(),
            method: crate::task::PathMethod::NixArchive,
            accepted: None,
            expected_sha256: None,
            inherit_references: false,
        }));
        drop(host.fetch(&crate::task::FetchRequest {
            url: "https://u/x".to_owned(),
            name: "x".to_owned(),
            kind: crate::task::FetchKind::File,
            expected_sha256: None,
        }));
        drop(host.fetch_tree(&crate::task::FetchTreeRequest {
            attrs: std::collections::BTreeMap::new(),
            fetcher: crate::task::TreeFetcher::Tree,
        }));
        host.warn("a warning cppnix would print");
        host.trace("a trace line builtins.trace would print");

        assert_eq!(
            *inner.asked.borrow(),
            vec![
                "read_file /f",
                "read_dir /d",
                "path_exists /e",
                "file_type /t",
                "get_env V",
                "copy_to_store /c",
                "ensure_path /p",
                "store_text t",
                "write_derivation a",
                "store_filtered /s",
                "fetch https://u/x",
                "fetch_tree fetchTree",
                "warn a warning cppnix would print",
                "trace a trace line builtins.trace would print",
            ],
            "an effect did not reach the host behind the recorder"
        );
        // Twelve effects are questions; the warning and the trace are outputs
        // and are kept separately, because they have no answer to key on.
        assert_eq!(host.take().len(), 12);
        assert_eq!(
            host.take_emissions(),
            vec![
                Emission::Warn("a warning cppnix would print".to_owned()),
                Emission::Trace("a trace line builtins.trace would print".to_owned()),
            ]
        );
    }

    /// A store effect under an immutable root keeps its answer in the row and
    /// replays from it once the store is seen to hold the object; the same
    /// effect under an ambient root keeps a digest and is made again.
    #[test]
    fn a_filtered_copy_under_a_mounted_root_replays_by_validity() {
        struct Store {
            held: Vec<String>,
            copied: RefCell<Vec<String>>,
            allowed: RefCell<Vec<Vec<String>>>,
        }
        impl Store {
            fn holding(held: &[&str]) -> Self {
                Self {
                    held: held.iter().map(|path| (*path).to_owned()).collect(),
                    copied: RefCell::new(Vec::new()),
                    allowed: RefCell::new(Vec::new()),
                }
            }
        }
        impl Host for Store {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
                ensure_path,
            );
            crate::host::host_stubs!(
                read_file_bytes,
                file_type_resolved,
                get_env,
                copy_to_store,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
            fn store_filtered(
                &self,
                request: &crate::task::FilteredCopy,
            ) -> Result<String, StoreError> {
                self.copied.borrow_mut().push(request.name.clone());
                Ok(format!(
                    "/nix/store/00000000000000000000000000000000-{}",
                    request.name
                ))
            }
            fn valid_paths(
                &self,
                paths: &[String],
            ) -> Result<std::collections::BTreeSet<String>, StoreError> {
                Ok(paths
                    .iter()
                    .filter(|path| self.held.contains(path))
                    .cloned()
                    .collect())
            }
            fn allow_paths(&self, paths: &[String]) -> Result<(), StoreError> {
                self.allowed.borrow_mut().push(paths.to_vec());
                Ok(())
            }
        }
        let request = |root: Rc<crate::value2::PathValue>, name: &str| crate::task::FilteredCopy {
            root,
            name: name.to_owned(),
            method: crate::task::PathMethod::Flat,
            accepted: None,
            expected_sha256: None,
            inherit_references: false,
        };
        let mount = "/nix/store/00000000000000000000000000000000-source";
        let mounted = Rc::new(crate::value2::PathValue::new(
            crate::value2::Root::mounted(mount),
            format!("{mount}/lib"),
        ));
        let ambient = crate::value2::ambient_path("/etc/lib");

        let recording = Store::holding(&[]);
        let recorder = RecordingHost::new(&recording);
        let mounted_path = recorder
            .store_filtered(&request(mounted.clone(), "mounted"))
            .expect("copy");
        recorder
            .store_filtered(&request(ambient.clone(), "ambient"))
            .expect("copy");
        let recorded = recorder.take();
        assert_eq!(
            recorded.entries()[0].1,
            Recorded::Answer(mounted_path.clone()),
            "the copy under the mounted root keeps its answer"
        );
        assert!(
            matches!(recorded.entries()[1].1, Recorded::Answer(_)),
            "the copy under the ambient root keeps its answer too; whether its \
             source could have changed is decided at replay"
        );
        let cas = ix_kernel::cas::MemoryCas::new();
        let settings = crate::eval::Settings::default();
        let identity = id(b"copies");

        let holding = Store::holding(&[mounted_path.as_str()]);
        let observed = ReadSet::replay(recorded.entries(), &holding, &settings, &cas)
            .expect("replay")
            .expect("not refused");
        assert_eq!(
            *holding.copied.borrow(),
            vec!["ambient".to_owned()],
            "only the copy under the ambient root is made again"
        );
        assert_eq!(
            *holding.allowed.borrow(),
            vec![vec![mounted_path.clone()]],
            "the copy served by validity is allowed as the live copy was; the \
             remade copy allows through its own hook"
        );
        assert_eq!(
            observed.key(&identity),
            recorded.key(&identity),
            "a held copy keys exactly as the copy did"
        );

        let lost = Store::holding(&[]);
        let remade = ReadSet::replay(recorded.entries(), &lost, &settings, &cas)
            .expect("replay")
            .expect("not refused");
        assert_eq!(
            *lost.copied.borrow(),
            vec!["mounted".to_owned(), "ambient".to_owned()],
            "a copy the store lost is made again"
        );
        assert_eq!(remade.key(&identity), recorded.key(&identity));
    }

    /// A realisation every element of which stands on held store paths
    /// replays as what `realiseContext` returns with nothing to build,
    /// computed from that fact and not read from the record, without
    /// building, and its outputs are allowed as a real realisation's would
    /// be; one that stands on a path the store lost is re-run. The built
    /// output's path comes from the ATerm the witness's own `WriteDrv` row
    /// keeps in the CAS.
    #[test]
    fn a_held_realisation_replays_as_nothing_to_build() {
        struct Store {
            held: Vec<String>,
            realised: Cell<usize>,
            allowed: RefCell<Vec<Vec<String>>>,
        }
        impl Store {
            fn holding(held: &[&str]) -> Self {
                Self {
                    held: held.iter().map(|path| (*path).to_owned()).collect(),
                    realised: Cell::new(0),
                    allowed: RefCell::new(Vec::new()),
                }
            }
        }
        impl Host for Store {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            crate::host::host_stubs!(
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
                ensure_path,
            );
            crate::host::host_stubs!(
                read_file_bytes,
                file_type_resolved,
                get_env,
                copy_to_store,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
            fn realise(
                &self,
                _context: &[crate::value2::ContextElem],
            ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
                self.realised.set(self.realised.get() + 1);
                Ok(std::collections::BTreeMap::new())
            }
            fn valid_paths(
                &self,
                paths: &[String],
            ) -> Result<std::collections::BTreeSet<String>, StoreError> {
                Ok(paths
                    .iter()
                    .filter(|path| self.held.contains(path))
                    .cloned()
                    .collect())
            }
            fn allow_closures(&self, outputs: &[String]) -> Result<(), StoreError> {
                self.allowed.borrow_mut().push(outputs.to_vec());
                Ok(())
            }
        }
        let drv = "/nix/store/00000000000000000000000000000000-a.drv";
        let out = "/nix/store/00000000000000000000000000000000-a";
        let src = "/nix/store/00000000000000000000000000000000-src";
        let aterm =
            format!(r#"Derive([("out","{out}","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#);
        let cas = ix_kernel::cas::MemoryCas::new();
        let rows = rows_of(&[
            Question::WriteDrv {
                name: "a".to_owned(),
                answer: WriteDrvAnswer::Written(drv.to_owned()),
                aterm: cas.put(aterm.as_bytes()).expect("a memory cas takes bytes"),
            },
            Question::Realise(vec![
                crate::value2::ContextElem::Built {
                    drv: drv.into(),
                    output: "out".into(),
                },
                crate::value2::ContextElem::Opaque(src.into()),
            ]),
        ]);
        let settings = crate::eval::Settings::default();

        let disabled = crate::eval::Settings {
            allow_import_from_derivation: false,
            ..settings.clone()
        };
        let blocked = Store::holding(&[drv, out, src]);
        assert!(
            ReadSet::replay(&rows, &blocked, &disabled, &cas)
                .expect("replay")
                .is_none(),
            "a preexisting successful Built row cannot bypass disabled IFD"
        );
        assert_eq!(blocked.realised.get(), 0, "rejection precedes host effects");
        assert!(blocked.allowed.borrow().is_empty());

        let holding = Store::holding(&[drv, out, src]);
        let observed = ReadSet::replay(&rows, &holding, &settings, &cas)
            .expect("replay")
            .expect("not refused");
        assert_eq!(holding.realised.get(), 0, "nothing is built");
        assert_eq!(
            *holding.allowed.borrow(),
            vec![vec![out.to_owned()]],
            "the built output is allowed as realiseContext would allow it"
        );
        assert_eq!(
            observed.entries()[1].1.digest(),
            digest_realise(&Ok(std::collections::BTreeMap::new())),
            "the answer is the empty rewrite map, not the recorded digest"
        );

        // Under ca-derivations the map realiseContextBuild returns maps
        // each built output's downstream placeholder to its path.
        let ca = crate::eval::Settings {
            ca_derivations: true,
            ..crate::eval::Settings::default()
        };
        let rewriting = Store::holding(&[drv, out, src]);
        let observed = ReadSet::replay(&rows, &rewriting, &ca, &cas)
            .expect("replay")
            .expect("not refused");
        assert_eq!(rewriting.realised.get(), 0);
        let mut expected = std::collections::BTreeMap::new();
        expected.insert(
            crate::drvpath::downstream_placeholder(drv, "out"),
            out.to_owned(),
        );
        assert_eq!(
            observed.entries()[1].1.digest(),
            digest_realise(&Ok(expected))
        );

        let partial = Store::holding(&[drv, src]);
        drop(ReadSet::replay(&rows, &partial, &settings, &cas).expect("replay"));
        assert_eq!(
            partial.realised.get(),
            1,
            "a realisation standing on a path the store lost is re-run"
        );
        assert!(partial.allowed.borrow().is_empty());
    }

    /// A live realisation of a derivation this evaluation wrote, every path
    /// of which the store holds, is answered without a build and not begun
    /// on a thread, and its row is the row the asked route records with
    /// nothing to build. One of a derivation not written here, one standing
    /// on a path the store lacks, and every one through a recorder not told
    /// to answer by validity ask for the build.
    #[test]
    fn a_live_realisation_of_a_held_derivation_this_evaluation_wrote_is_not_built() {
        struct Store {
            held: RefCell<Vec<String>>,
            realised: Cell<usize>,
            begun: Cell<usize>,
            allowed: RefCell<Vec<Vec<String>>>,
            allow_fails: Cell<bool>,
        }
        impl Store {
            fn holding(held: &[&str]) -> Self {
                Self {
                    held: RefCell::new(held.iter().map(|path| (*path).to_owned()).collect()),
                    realised: Cell::new(0),
                    begun: Cell::new(0),
                    allowed: RefCell::new(Vec::new()),
                    allow_fails: Cell::new(false),
                }
            }
        }
        impl Host for Store {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            crate::host::host_stubs!(
                store_text,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                ensure_path,
            );
            crate::host::host_stubs!(
                read_file_bytes,
                file_type_resolved,
                get_env,
                copy_to_store,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
            fn write_derivation(&self, name: &str, _aterm: &str) -> Result<String, StoreError> {
                if name == "fail" {
                    return Err(StoreError::Failed("refused".to_owned()));
                }
                Ok(format!(
                    "/nix/store/00000000000000000000000000000000-{name}.drv"
                ))
            }
            fn realise(
                &self,
                _context: &[crate::value2::ContextElem],
            ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
                self.realised.set(self.realised.get() + 1);
                Ok(std::collections::BTreeMap::new())
            }
            fn valid_paths(
                &self,
                paths: &[String],
            ) -> Result<std::collections::BTreeSet<String>, StoreError> {
                Ok(paths
                    .iter()
                    .filter(|path| self.held.borrow().contains(path))
                    .cloned()
                    .collect())
            }
            fn allow_closures(&self, outputs: &[String]) -> Result<(), StoreError> {
                if self.allow_fails.get() {
                    return Err(StoreError::Failed("cannot allow".to_owned()));
                }
                self.allowed.borrow_mut().push(outputs.to_vec());
                Ok(())
            }
            fn begin(&self, _question: &crate::host::Slow<'_>) -> Option<crate::host::Ticket> {
                self.begun.set(self.begun.get() + 1);
                None
            }
            fn collect(
                &self,
                _ticket: crate::host::Ticket,
                _block: bool,
            ) -> Option<crate::host::SlowAnswer> {
                None
            }
        }
        use crate::value2::ContextElem;
        let drv = "/nix/store/00000000000000000000000000000000-a.drv";
        let out = "/nix/store/00000000000000000000000000000000-a";
        let src = "/nix/store/00000000000000000000000000000000-src";
        let aterm =
            format!(r#"Derive([("out","{out}","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#);
        let context = vec![
            ContextElem::Built {
                drv: drv.into(),
                output: "out".into(),
            },
            ContextElem::Opaque(src.into()),
        ];
        let empty = std::collections::BTreeMap::new();

        let blocked_store = Store::holding(&[drv, out, src]);
        let blocked = RecordingHost::new(&blocked_store).realising_by_validity(false, false);
        assert_eq!(blocked.write_derivation("a", &aterm).expect("written"), drv);
        assert!(
            blocked
                .begin(&crate::host::Slow::Realise(&context))
                .is_none()
        );
        assert_eq!(
            blocked_store.begun.get(),
            1,
            "disabled IFD reaches the host policy check"
        );
        assert_eq!(blocked.realise(&context).expect("host answer"), empty);
        assert_eq!(
            blocked_store.realised.get(),
            1,
            "held outputs cannot bypass host policy"
        );
        assert!(blocked_store.allowed.borrow().is_empty());
        let opaque = vec![ContextElem::Opaque(src.into())];
        assert_eq!(blocked.realise(&opaque).expect("opaque path"), empty);
        assert_eq!(
            blocked_store.realised.get(),
            1,
            "ordinary paths keep the validity fast path"
        );

        let store = Store::holding(&[drv, out, src]);
        let recorder = RecordingHost::new(&store).realising_by_validity(false, true);
        assert_eq!(
            recorder.write_derivation("a", &aterm).expect("written"),
            drv
        );
        assert!(
            recorder
                .begin(&crate::host::Slow::Realise(&context))
                .is_none(),
            "nothing to build: not begun"
        );
        assert_eq!(
            store.begun.get(),
            0,
            "the host behind it is not asked to begin it either"
        );
        assert!(
            store.allowed.borrow().is_empty(),
            "declining to begin allows nothing"
        );
        assert_eq!(recorder.realise(&context).expect("realised"), empty);
        assert_eq!(store.realised.get(), 0, "nothing is built");
        assert_eq!(
            *store.allowed.borrow(),
            vec![vec![out.to_owned()]],
            "the built output is allowed once, as realiseContext would allow it"
        );
        let served = recorder.take();
        // The control: the asked route, with nothing to build (the fake host
        // builds nothing and answers the empty map), records the same row.
        let control = RecordingHost::new(&store);
        assert_eq!(control.write_derivation("a", &aterm).expect("written"), drv);
        assert_eq!(control.realise(&context).expect("realised"), empty);
        assert_eq!(store.realised.get(), 1, "the control asks");
        assert_eq!(
            served.entries()[1],
            control.take().entries()[1],
            "the served row is the asked route's row with nothing to build"
        );
        store.realised.set(0);

        // A derivation this evaluation did not write asks.
        let other = "/nix/store/11111111111111111111111111111111-b.drv";
        store.held.borrow_mut().push(other.to_owned());
        let recorder = RecordingHost::new(&store).realising_by_validity(false, true);
        let unwritten = vec![ContextElem::Built {
            drv: other.into(),
            output: "out".into(),
        }];
        assert_eq!(recorder.realise(&unwritten).expect("realised"), empty);
        assert_eq!(
            store.realised.get(),
            1,
            "a derivation not written here is asked about"
        );

        // A path the store lacks asks, and the loss is seen at the next
        // realisation through the same recorder, including one lost between
        // declining to begin and answering.
        let recorder = RecordingHost::new(&store).realising_by_validity(false, true);
        assert_eq!(
            recorder.write_derivation("a", &aterm).expect("written"),
            drv
        );
        assert_eq!(recorder.realise(&context).expect("realised"), empty);
        assert_eq!(store.realised.get(), 1);
        assert!(
            recorder
                .begin(&crate::host::Slow::Realise(&context))
                .is_none()
        );
        store.allowed.borrow_mut().clear();
        store.held.borrow_mut().retain(|path| path != out);
        assert_eq!(recorder.realise(&context).expect("realised"), empty);
        assert_eq!(
            store.realised.get(),
            2,
            "an output the store lost is asked for"
        );
        assert!(
            store.allowed.borrow().is_empty(),
            "nothing was allowed for the answer the store withdrew"
        );
        store.held.borrow_mut().push(out.to_owned());

        // Under ca-derivations the answer maps each built output's
        // downstream placeholder to its path, as realiseContextBuild does.
        let recorder = RecordingHost::new(&store).realising_by_validity(true, true);
        assert_eq!(
            recorder.write_derivation("a", &aterm).expect("written"),
            drv
        );
        let mut expected = std::collections::BTreeMap::new();
        expected.insert(
            crate::drvpath::downstream_placeholder(drv, "out"),
            out.to_owned(),
        );
        assert_eq!(recorder.realise(&context).expect("realised"), expected);
        assert_eq!(store.realised.get(), 2);

        // What cannot be shown to have nothing to build asks: an output of a
        // derivation whose write failed, an output name the derivation does
        // not have, a floating output with no path yet, and outputs the host
        // cannot allow (nothing is allowed for the asked answer either). An
        // opaque path and a deep reference the store holds are served, with
        // nothing to allow.
        let recorder = RecordingHost::new(&store).realising_by_validity(false, true);
        let unwritten_drv = "/nix/store/00000000000000000000000000000000-fail.drv";
        assert!(recorder.write_derivation("fail", &aterm).is_err());
        store.held.borrow_mut().push(unwritten_drv.to_owned());
        let of_failed_write = vec![ContextElem::Built {
            drv: unwritten_drv.into(),
            output: "out".into(),
        }];
        assert_eq!(recorder.realise(&of_failed_write).expect("realised"), empty);
        assert_eq!(
            store.realised.get(),
            3,
            "a failed write leaves nothing to stand on"
        );
        assert_eq!(
            recorder.write_derivation("a", &aterm).expect("written"),
            drv
        );
        let no_such_output = vec![ContextElem::Built {
            drv: drv.into(),
            output: "dev".into(),
        }];
        assert_eq!(recorder.realise(&no_such_output).expect("realised"), empty);
        assert_eq!(
            store.realised.get(),
            4,
            "an output the derivation lacks asks"
        );
        let floating = r#"Derive([("out","","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
        let floating_drv = recorder.write_derivation("f", floating).expect("written");
        store.held.borrow_mut().push(floating_drv.clone());
        let of_floating = vec![ContextElem::Built {
            drv: floating_drv.as_str().into(),
            output: "out".into(),
        }];
        assert_eq!(recorder.realise(&of_floating).expect("realised"), empty);
        assert_eq!(
            store.realised.get(),
            5,
            "a floating output has no path to stand on"
        );
        store.allowed.borrow_mut().clear();
        store.allow_fails.set(true);
        assert_eq!(recorder.realise(&context).expect("realised"), empty);
        assert_eq!(
            store.realised.get(),
            6,
            "outputs the host cannot allow are asked for"
        );
        store.allow_fails.set(false);
        let plain = vec![
            ContextElem::Opaque(src.into()),
            ContextElem::DrvDeep(drv.into()),
        ];
        assert_eq!(recorder.realise(&plain).expect("realised"), empty);
        assert_eq!(
            store.realised.get(),
            6,
            "held opaque and deep elements have nothing to build"
        );
        assert!(
            store.allowed.borrow().is_empty(),
            "no built output, nothing to allow"
        );

        // A recorder not told to answer by validity asks.
        let recorder = RecordingHost::new(&store);
        assert_eq!(
            recorder.write_derivation("a", &aterm).expect("written"),
            drv
        );
        assert_eq!(recorder.realise(&context).expect("realised"), empty);
        assert_eq!(store.realised.get(), 7);
    }

    /// A read under an ambient path inside a sealed store object replays
    /// from its record exactly as one under a mounted root does; the object
    /// is asked about once, then remembered, and an object the embedder does
    /// not call sealed leaves every read under it asking.
    #[test]
    fn a_read_under_a_sealed_store_object_replays_from_the_record_once_the_object_is_known() {
        struct Store {
            sealed: Vec<String>,
            asked_sealed: RefCell<Vec<Vec<String>>>,
            listed: RefCell<Vec<String>>,
        }
        impl Host for Store {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
                ensure_path,
            );
            crate::host::host_stubs!(
                read_file_bytes,
                file_type_resolved,
                get_env,
                copy_to_store,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                self.listed.borrow_mut().push(p.path.to_string());
                Ok(vec![("a".to_owned(), FileType::Regular)])
            }
            fn path_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
            fn sealed_paths(&self, objects: &[String]) -> Result<crate::host::Links, StoreError> {
                self.asked_sealed.borrow_mut().push(objects.to_vec());
                Ok(objects
                    .iter()
                    .filter(|object| self.sealed.contains(object))
                    .map(|object| (object.clone(), Vec::new()))
                    .collect())
            }
            fn valid_paths(
                &self,
                paths: &[String],
            ) -> Result<std::collections::BTreeSet<String>, StoreError> {
                // Everything the verifier asks about is present; presence is
                // the other test's subject.
                Ok(paths.iter().cloned().collect())
            }
        }
        let store = |sealed: &[&str]| Store {
            sealed: sealed.iter().map(|s| (*s).to_owned()).collect(),
            asked_sealed: RefCell::new(Vec::new()),
            listed: RefCell::new(Vec::new()),
        };
        let object = "00000000000000000000000000000000-source";
        let inside = crate::value2::ambient_path(format!("/nix/store/{object}/lib"));
        let outside = crate::value2::ambient_path("/etc/lib");
        let rows = rows_of(&[
            Question::ReadDir(inside.clone()),
            Question::ReadDir(outside.clone()),
        ]);
        let settings = crate::eval::Settings {
            store_dir: Some("/nix/store".to_owned()),
            ..crate::eval::Settings::default()
        };
        let cas = ix_kernel::cas::MemoryCas::new();
        let dir = std::env::temp_dir().join(format!(
            "ixe-sealed-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        let memo = DirSealed::open(&dir).expect("a sealed record under a fresh directory");

        let sealing = store(&[object]);
        drop(
            ReadSet::replay_with(&rows, &sealing, &settings, &cas, Some(&memo))
                .expect("replay")
                .expect("not refused"),
        );
        assert_eq!(
            *sealing.asked_sealed.borrow(),
            vec![vec![object.to_owned()]],
            "the object under which a row reads is asked about, once"
        );
        assert_eq!(
            *sealing.listed.borrow(),
            vec!["/etc/lib".to_owned()],
            "only the listing outside the sealed object is asked again"
        );
        assert_eq!(
            memo.get(object),
            Some(Vec::new()),
            "a sealed object is recorded, with no link leaving it"
        );

        let remembering = store(&[object]);
        drop(
            ReadSet::replay_with(&rows, &remembering, &settings, &cas, Some(&memo))
                .expect("replay")
                .expect("not refused"),
        );
        assert!(
            remembering.asked_sealed.borrow().is_empty(),
            "a recorded object is never asked about again"
        );
        assert_eq!(*remembering.listed.borrow(), vec!["/etc/lib".to_owned()]);
        std::fs::remove_dir_all(&dir).expect("scratch removed");

        let refusing = store(&[]);
        drop(
            ReadSet::replay(&rows, &refusing, &settings, &cas)
                .expect("replay")
                .expect("not refused"),
        );
        assert_eq!(
            *refusing.listed.borrow(),
            vec![format!("/nix/store/{object}/lib"), "/etc/lib".to_owned()],
            "under an object the embedder does not call sealed every read asks"
        );

        let elsewhere = crate::eval::Settings::default();
        let unplaced = store(&[object]);
        drop(
            ReadSet::replay(&rows, &unplaced, &elsewhere, &cas)
                .expect("replay")
                .expect("not refused"),
        );
        assert!(
            unplaced.asked_sealed.borrow().is_empty(),
            "with no store directory no path lies in a store object"
        );
        assert_eq!(unplaced.listed.borrow().len(), 2);
    }

    /// An object the host declines to call sealed (absent from the store, or
    /// input-addressed) is asked about once per handle, not once per
    /// question: the declined set a first answer fills is consulted by the
    /// batch question (`sealed_objects`) and the single one (`seal_one`)
    /// alike. The set is the handle's, not the directory's: a second handle
    /// on the same directory asks once more, and a record inserted later
    /// outranks the decline.
    #[test]
    fn an_object_the_host_declines_is_asked_about_once_per_handle() {
        struct Store {
            asked: RefCell<Vec<Vec<String>>>,
        }
        impl Host for Store {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
                ensure_path,
            );
            crate::host::host_stubs!(
                read_file_bytes,
                file_type_resolved,
                get_env,
                copy_to_store,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(vec![("a".to_owned(), FileType::Regular)])
            }
            fn path_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
            fn sealed_paths(&self, objects: &[String]) -> Result<crate::host::Links, StoreError> {
                self.asked.borrow_mut().push(objects.to_vec());
                // Nothing is sealed: the input-addressed case.
                Ok(crate::host::Links::new())
            }
            fn valid_paths(
                &self,
                paths: &[String],
            ) -> Result<std::collections::BTreeSet<String>, StoreError> {
                Ok(paths.iter().cloned().collect())
            }
        }
        let host = Store {
            asked: RefCell::new(Vec::new()),
        };
        let object = "00000000000000000000000000000000-cargo-vendor-dir";
        let dir = std::env::temp_dir().join(format!(
            "ixe-declined-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        let memo = DirSealed::open(&dir).expect("a sealed record under a fresh directory");

        assert_eq!(seal_one(object, &host, &memo), None);
        assert_eq!(seal_one(object, &host, &memo), None);
        assert_eq!(
            host.asked.borrow().len(),
            1,
            "the second single question is answered from the declined set"
        );

        let rows = rows_of(&[Question::ReadDir(crate::value2::ambient_path(format!(
            "/nix/store/{object}/lib"
        )))]);
        let settings = crate::eval::Settings {
            store_dir: Some("/nix/store".to_owned()),
            ..crate::eval::Settings::default()
        };
        let cas = ix_kernel::cas::MemoryCas::new();
        drop(ReadSet::replay_with(&rows, &host, &settings, &cas, Some(&memo)).expect("replay"));
        assert_eq!(
            host.asked.borrow().len(),
            1,
            "the batch question consults the same set"
        );

        let fresh = DirSealed::open(&dir).expect("a second handle on the same directory");
        drop(ReadSet::replay_with(&rows, &host, &settings, &cas, Some(&fresh)).expect("replay"));
        assert_eq!(
            host.asked.borrow().len(),
            2,
            "a second handle has its own set: it asks once"
        );
        drop(ReadSet::replay_with(&rows, &host, &settings, &cas, Some(&fresh)).expect("replay"));
        assert_eq!(host.asked.borrow().len(), 2, "and fills it");

        // A record outranks the decline and leaves it: once the object is
        // sealed, the batch question serves it from the record without asking.
        assert!(memo.declined(object));
        memo.insert(object, &[])
            .expect("a record under the temp dir");
        assert!(
            !memo.declined(object),
            "inserting a record leaves the declined state"
        );
        assert_eq!(memo.get(object), Some(Vec::new()));
        drop(ReadSet::replay_with(&rows, &host, &settings, &cas, Some(&memo)).expect("replay"));
        assert_eq!(
            host.asked.borrow().len(),
            2,
            "served from the record, not asked"
        );
        std::fs::remove_dir_all(&dir).expect("the temp dir is removable");
    }

    /// A sealed object's bytes are pinned by its name, link text included;
    /// the one read under it that can reach other bytes resolves a symlink
    /// out of the object. So a read at or below a leaving link asks, and a
    /// sibling, a name that merely extends the link's, and the object root
    /// (a copy of the whole tree takes links as links) replay from the
    /// record; the leaving links are remembered with the object.
    #[test]
    fn a_read_at_or_below_a_symlink_leaving_its_object_asks_and_its_siblings_replay() {
        let object = "00000000000000000000000000000000-source";
        let store_dir = Some("/nix/store");
        let sealed: crate::host::Sealed = [(
            object.to_owned(),
            vec!["result".to_owned(), "a/b".to_owned()],
        )]
        .into_iter()
        .collect();
        let path = |rel: &str| crate::value2::ambient_path(format!("/nix/store/{object}{rel}"));
        for (rel, expected) in [
            ("", true),
            ("/flake.nix", true),
            ("/resultant", true),
            ("/result", false),
            ("/result/x", false),
            ("/a", true),
            ("/a/b", false),
            ("/a/b/c", false),
            ("/a/bc", true),
        ] {
            assert_eq!(
                immutable_root(&path(rel), &sealed, store_dir),
                expected,
                "{object}{rel}"
            );
        }
        let unsealed = crate::host::Sealed::new();
        assert!(!immutable_root(&path("/flake.nix"), &unsealed, store_dir));
        assert!(!immutable_root(&path("/flake.nix"), &sealed, None));

        let dir = std::env::temp_dir().join(format!(
            "ixe-sealed-links-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        let memo = DirSealed::open(&dir).expect("a sealed record under a fresh directory");
        assert_eq!(memo.get(object), None);
        memo.insert(object, &["result".to_owned(), "a/b".to_owned()])
            .expect("recorded");
        assert_eq!(
            memo.get(object),
            Some(vec!["result".to_owned(), "a/b".to_owned()]),
            "the leaving links come back with the object"
        );
        std::fs::remove_dir_all(&dir).expect("scratch removed");
    }

    /// Which of an object's links leave it, to a fixpoint: an absolute or
    /// climbing target leaves; a link resolving through a leaving link leaves;
    /// a link aliasing a subtree that holds a leaving link leaves (a read
    /// spelled through the alias reaches the link under a name the prefix
    /// rule cannot see); a link to the object root aliases everything; a
    /// dangling in-object target and a cycle leave nothing.
    #[test]
    fn a_link_leaves_its_object_directly_through_another_or_by_aliasing_one() {
        let link = |path: &str, target: &str| crate::host::Symlink {
            path: path.to_owned(),
            target: target.to_owned(),
        };
        let leaving = |links: &[crate::host::Symlink]| leaving_links(links);
        assert_eq!(leaving(&[link("result", "/nix/store/x")]), vec!["result"]);
        assert_eq!(leaving(&[link("a/up", "../../etc")]), vec!["a/up"]);
        assert_eq!(
            leaving(&[link("a/b/up", "../..")]),
            Vec::<String>::new(),
            "to the root, not past it"
        );
        assert_eq!(
            leaving(&[link("dangling", "nowhere/here")]),
            Vec::<String>::new()
        );
        assert_eq!(
            leaving(&[link("x", "y"), link("y", "x")]),
            Vec::<String>::new(),
            "a cycle"
        );
        assert_eq!(
            leaving(&[link("escape", "/etc"), link("alias", "escape")]),
            vec!["alias", "escape"],
            "a link resolving through a leaving link"
        );
        assert_eq!(
            leaving(&[link("dir/out", "/etc"), link("alias", "dir")]),
            vec!["alias", "dir/out"],
            "a link aliasing the subtree that holds a leaving link"
        );
        assert_eq!(
            leaving(&[link("deep/out", "/etc"), link("a", "b"), link("b", "deep")]),
            vec!["a", "b", "deep/out"],
            "through two aliases, found at the fixpoint"
        );
        assert_eq!(
            leaving(&[link("out", "/etc"), link("self", ".")]),
            vec!["out", "self"],
            "a link to the object root aliases every leaving link"
        );
        assert_eq!(
            leaving(&[
                link("out", "/etc"),
                link("lib/inner", "../lib/other"),
                link("lib/other", "file")
            ]),
            vec!["out"],
            "links that stay inside and away from the leaving one do not leave"
        );
        assert_eq!(
            leaving(&[link("dir/out", "/etc"), link("alias", "dir/out/../passwd")]),
            vec!["alias", "dir/out"],
            "a `..` after a link component applies to the link's target, not to the spelling: \
             the traversal is called leaving rather than collapsed to `dir/passwd`"
        );
        assert_eq!(
            leaving(&[
                link("a/out", "/etc"),
                link("s", "a/b"),
                link("alias", "s/../out")
            ]),
            vec!["a/out", "alias"],
            "the same through an in-object link: `s` is traversed before `..`"
        );
        assert_eq!(
            leaving(&[link("s", "a/b"), link("alias", "s/c")]),
            vec!["alias"],
            "traversing any link before the last component is indeterminate here, so it leaves \
             (asks) even when the real resolution would stay inside"
        );
    }

    /// An import of a directory reads `<dir>/default.nix`, so a leaving link
    /// there makes the import ask even though the asked path is above it;
    /// the other reads of the same path are unaffected.
    #[test]
    fn an_import_of_a_directory_asks_when_its_default_nix_is_a_leaving_link() {
        let object = "00000000000000000000000000000000-source";
        let sealed: crate::host::Sealed = [(object.to_owned(), vec!["pkg/default.nix".to_owned()])]
            .into_iter()
            .collect();
        let dir = crate::value2::ambient_path(format!("/nix/store/{object}/pkg"));
        let other = crate::value2::ambient_path(format!("/nix/store/{object}/lib"));
        assert!(!Question::Import(dir.clone()).replays_from_record(&sealed, Some("/nix/store")));
        assert!(Question::ReadDir(dir).replays_from_record(&sealed, Some("/nix/store")));
        assert!(Question::Import(other).replays_from_record(&sealed, Some("/nix/store")));
    }

    /// A sealed record authorises replaying reads without asking, so only a
    /// record carrying its own digest counts: an empty file, a truncated one,
    /// or one written by anything else is no record and the object is asked
    /// about again.
    #[test]
    fn a_sealed_record_without_its_digest_is_no_record() {
        let object = "00000000000000000000000000000000-source";
        let dir = std::env::temp_dir().join(format!(
            "ixe-sealed-digest-{}-{}",
            std::process::id(),
            WITNESS_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        ));
        let memo = DirSealed::open(&dir).expect("a sealed record under a fresh directory");
        std::fs::write(dir.join(object), b"").expect("an empty entry");
        assert_eq!(memo.get(object), None, "the pre-digest format is no record");
        memo.insert(object, &["result".to_owned()])
            .expect("recorded");
        assert_eq!(memo.get(object), Some(vec!["result".to_owned()]));
        let text = std::fs::read_to_string(dir.join(object)).expect("the entry");
        let (digest_line, body) = text.split_once('\n').expect("a digest line");
        std::fs::write(dir.join(object), format!("{digest_line}\n{body}other\n"))
            .expect("tampered");
        assert_eq!(
            memo.get(object),
            None,
            "a body that does not match its digest is no record"
        );
        std::fs::remove_dir_all(&dir).expect("scratch removed");
    }

    /// Which ambient paths lie in a store object, by the store directory the
    /// embedder named.
    #[test]
    fn a_store_object_is_the_first_well_formed_name_below_the_store_directory() {
        let object = "00000000000000000000000000000000-source";
        let dir = Some("/nix/store");
        assert_eq!(
            store_object_of(dir, &format!("/nix/store/{object}/a/b")).as_deref(),
            Some(object)
        );
        assert_eq!(
            store_object_of(dir, &format!("/nix/store/{object}")).as_deref(),
            Some(object)
        );
        assert_eq!(store_object_of(dir, "/nix/store/not-a-store-name/a"), None);
        assert_eq!(store_object_of(dir, "/nix/store"), None);
        assert_eq!(store_object_of(dir, "/etc/lib"), None);
        assert_eq!(store_object_of(dir, "/nix/storeroom/x"), None);
        assert_eq!(
            store_object_of(None, &format!("/nix/store/{object}/a")),
            None
        );
    }

    /// A kept answer decodes only on a question that can replay from it.
    /// A witness is bytes off disk, and a kept answer anywhere else would
    /// key with a digest no recording produced.
    #[test]
    fn a_kept_answer_is_refused_on_a_question_that_cannot_use_it() {
        let mount = "/nix/store/00000000000000000000000000000000-source";
        let mounted = Question::CopyToStore(Rc::new(crate::value2::PathValue::new(
            crate::value2::Root::mounted(mount),
            format!("{mount}/x"),
        )));
        let read = Question::ReadFile(crate::value2::ambient_path("/etc/x"));
        let copy = "/nix/store/00000000000000000000000000000000-x";
        let kept = |question: &Question| {
            CanonValue::array(vec![question_value(question), CanonValue::str(copy)])
        };
        assert_eq!(
            row_from(&kept(&mounted)),
            Some((mounted.clone(), Recorded::Answer(copy.to_owned())))
        );
        assert_eq!(row_from(&kept(&read)), None);
        assert_eq!(Recorded::answer(&read, copy.to_owned()), None);
        assert_eq!(
            Recorded::Answer(copy.to_owned()).digest(),
            digest_store_copy(&Ok(copy.to_owned())),
            "a kept answer digests as the recording host digested it"
        );
    }

    /// Which fetches name their object: a fetch pinned by `sha256`, and a
    /// final tree with a `narHash`, whose object is the tree's `outPath`.
    /// Everything else asks.
    #[test]
    fn only_copies_pinned_fetches_and_locked_final_trees_name_their_object() {
        let fetch = |sha: Option<&str>| {
            Question::Fetch(Box::new(crate::task::FetchRequest {
                url: "https://u/x.tar.gz".to_owned(),
                name: "x".to_owned(),
                kind: crate::task::FetchKind::Tarball,
                expected_sha256: sha.map(str::to_owned),
            }))
        };
        let copy = "/nix/store/00000000000000000000000000000000-x";
        assert_eq!(
            fetch(Some("sha256-AAAA")).named_object(copy).as_deref(),
            Some(copy)
        );
        assert_eq!(fetch(None).named_object(copy), None);

        let tree = |fetcher: crate::task::TreeFetcher, locked: bool| {
            let mut attrs = std::collections::BTreeMap::new();
            attrs.insert(
                "type".to_owned(),
                crate::task::TreeAttr::Str("github".to_owned()),
            );
            if locked {
                attrs.insert(
                    "narHash".to_owned(),
                    crate::task::TreeAttr::Str("sha256-AAAA".to_owned()),
                );
            }
            Question::FetchTree(Box::new(crate::task::FetchTreeRequest { attrs, fetcher }))
        };
        let answer = format!("{{\"outPath\":\"{copy}\",\"narHash\":\"sha256-AAAA\"}}");
        assert_eq!(
            tree(crate::task::TreeFetcher::FinalTree, true)
                .named_object(&answer)
                .as_deref(),
            Some(copy)
        );
        assert_eq!(
            tree(crate::task::TreeFetcher::Tree, true).named_object(&answer),
            None
        );
        assert_eq!(
            tree(crate::task::TreeFetcher::Git, true).named_object(&answer),
            None
        );
        assert_eq!(
            tree(crate::task::TreeFetcher::FinalTree, false).named_object(&answer),
            None
        );
        assert_eq!(
            tree(crate::task::TreeFetcher::FinalTree, true).named_object("not json"),
            None,
            "an answer with no outPath has no witness and asks"
        );
    }

    /// A hit does not write a derivation the store still holds. The verifier
    /// asks which of the witness's expected paths are present, once for the
    /// whole witness, asks again at the row of one the batch did not hold
    /// (an earlier row could have written it), and rewrites only a missing
    /// one. The read set comes out identical to a replay that rewrote every
    /// row, because a write of the same ATerm can only answer the same path:
    /// what changed is the work, not the key.
    #[test]
    fn replay_checks_a_held_derivation_instead_of_rewriting_it() {
        struct Store {
            held: Vec<String>,
            asked: std::cell::RefCell<Vec<Vec<String>>>,
            written: std::cell::RefCell<Vec<String>>,
        }
        impl Store {
            fn holding(held: &[&str]) -> Self {
                Self {
                    held: held.iter().map(|path| (*path).to_owned()).collect(),
                    asked: std::cell::RefCell::new(Vec::new()),
                    written: std::cell::RefCell::new(Vec::new()),
                }
            }
        }
        impl Host for Store {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            crate::host::host_stubs!(
                realise,
                store_text,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
                ensure_path,
            );
            crate::host::host_stubs!(
                read_file_bytes,
                file_type_resolved,
                get_env,
                copy_to_store,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(&self, _p: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
            fn write_derivation(&self, name: &str, _aterm: &str) -> Result<String, StoreError> {
                self.written.borrow_mut().push(name.to_owned());
                Ok(format!("/nix/store/{name}.drv"))
            }
            fn valid_paths(
                &self,
                paths: &[String],
            ) -> Result<std::collections::BTreeSet<String>, StoreError> {
                self.asked.borrow_mut().push(paths.to_vec());
                Ok(paths
                    .iter()
                    .filter(|path| self.held.contains(path))
                    .cloned()
                    .collect())
            }
        }

        let cas = ix_kernel::cas::MemoryCas::new();
        let row = |name: &str, aterm: &[u8]| Question::WriteDrv {
            name: name.to_owned(),
            answer: WriteDrvAnswer::Written(format!("/nix/store/{name}.drv")),
            aterm: cas.put(aterm).expect("a memory cas takes bytes"),
        };
        let questions = vec![
            row("held", b"Derive([held])"),
            row("gone", b"Derive([gone])"),
        ];
        let settings = crate::eval::Settings::default();

        let checking = Store::holding(&["/nix/store/held.drv"]);
        let observed = ReadSet::replay(&rows_of(&questions), &checking, &settings, &cas)
            .expect("replay")
            .expect("not refused");
        assert_eq!(
            *checking.asked.borrow(),
            vec![
                vec![
                    "/nix/store/gone.drv".to_owned(),
                    "/nix/store/held.drv".to_owned()
                ],
                vec!["/nix/store/gone.drv".to_owned()]
            ],
            "one validity question for the whole witness, its paths sorted, then one at the row of \
             the derivation it did not hold"
        );
        assert_eq!(
            *checking.written.borrow(),
            vec!["gone".to_owned()],
            "only the derivation the store lost is written"
        );
        let streamed_host = Store::holding(&["/nix/store/held.drv"]);
        let streamed = ReadSet::replay_key_with(
            &rows_of(&questions),
            &streamed_host,
            &settings,
            &cas,
            None,
            &id(b"streamed-effects"),
        )
        .expect("streamed replay")
        .expect("allowed");
        assert_eq!(streamed, observed.key(&id(b"streamed-effects")));
        assert_eq!(*streamed_host.asked.borrow(), *checking.asked.borrow());
        assert_eq!(*streamed_host.written.borrow(), *checking.written.borrow());

        let rewriting = Store::holding(&[]);
        let rewritten = ReadSet::replay(&rows_of(&questions), &rewriting, &settings, &cas)
            .expect("replay")
            .expect("not refused");
        assert_eq!(
            *rewriting.written.borrow(),
            vec!["held".to_owned(), "gone".to_owned()]
        );
        assert_eq!(
            observed, rewritten,
            "a held derivation answers exactly as its write would"
        );
    }

    /// `ensure_path`'s answer decides whether `builtins.appendContext`
    /// succeeds, so it is a question and a changed answer has to move the key.
    #[test]
    fn a_store_that_stops_producing_a_path_changes_the_key() {
        struct Store(Cell<bool>);
        impl Host for Store {
            crate::host::host_stubs!(settle);
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
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
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
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
            fn ensure_path(&self, _p: &str) -> Result<(), StoreError> {
                if self.0.get() {
                    Ok(())
                } else {
                    Err(StoreError::Failed("path is not valid".to_owned()))
                }
            }
        }
        let inner = Store(Cell::new(true));
        let host = RecordingHost::new(&inner);
        drop(host.ensure_path("/nix/store/xyz"));
        let recorded = host.take();
        let identity = id(b"ensure");
        inner.0.set(false);
        assert_ne!(
            recorded.key(&identity),
            replayed(&recorded.questions(), &inner).key(&identity)
        );
    }

    /// The token has to survive the store, or a served refusal is counted
    /// under the wrong kind for the rest of the cache's life -- and unlike a
    /// wrong value, nothing downstream can tell.
    #[test]
    fn the_refusal_token_survives_the_result_codec() -> Result<(), Box<dyn core::error::Error>> {
        let result = EvalResult {
            status: crate::session::UNIMPLEMENTED.to_owned(),
            value: "effect domain 'x'".to_owned(),
            emissions: Vec::new(),
            token: Some(crate::refusal::RefusalToken::EffectDomain),
            pos: None,
        };
        let back = decode_result(&encode_result(&result)?).ok_or("did not decode")?;
        assert_eq!(back.token, Some(crate::refusal::RefusalToken::EffectDomain));
        assert_eq!(back.status, result.status);
        Ok(())
    }

    /// Every field of the format is required. A row missing one is damaged --
    /// there are no older shapes to read, the identity tag retires them -- and
    /// a damaged row is a miss rather than half an answer.
    #[test]
    fn a_result_row_missing_any_field_is_refused() -> Result<(), Box<dyn core::error::Error>> {
        let fields: [(&str, CanonValue); 5] = [
            ("status", CanonValue::str(crate::session::OK)),
            ("value", CanonValue::str("1")),
            ("emissions", CanonValue::array([])),
            ("token", CanonValue::str("")),
            ("pos", CanonValue::array([])),
        ];
        let complete = canon::encode(&CanonValue::map(fields.clone()))?;
        assert!(
            decode_result(&complete).is_some(),
            "the complete row decodes"
        );
        for omitted in 0..fields.len() {
            let partial: Vec<(&str, CanonValue)> = fields
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != omitted)
                .map(|(_, f)| f.clone())
                .collect();
            let bytes = canon::encode(&CanonValue::map(partial))?;
            assert_eq!(
                decode_result(&bytes),
                None,
                "a row without {:?} must not decode",
                fields[omitted].0
            );
        }
        Ok(())
    }

    /// A refusal token this build does not know is a damaged row, not an
    /// unclassified refusal: `Unrecorded` is what the evaluator writes when
    /// it could not classify, and a decoder that invented it would hide a
    /// format drift as a census category.
    #[test]
    fn a_result_row_with_an_unknown_token_is_refused() -> Result<(), Box<dyn core::error::Error>> {
        let bytes = canon::encode(&CanonValue::map([
            ("status", CanonValue::str(crate::session::UNIMPLEMENTED)),
            ("value", CanonValue::str("something")),
            ("emissions", CanonValue::array([])),
            ("token", CanonValue::str("no-such-token")),
            ("pos", CanonValue::array([])),
        ]))?;
        assert_eq!(decode_result(&bytes), None);
        Ok(())
    }

    /// A result carries what the evaluation said, so a served answer can say
    /// it again -- warnings and traces alike, in the order they happened.
    #[test]
    fn emissions_survive_the_result_codec() -> Result<(), Box<dyn core::error::Error>> {
        let result = EvalResult {
            status: "ok".to_owned(),
            value: "1".to_owned(),
            token: None,
            pos: None,
            emissions: vec![
                Emission::Warn("first".to_owned()),
                Emission::Trace("second".to_owned()),
            ],
        };
        assert_eq!(
            decode_result(&encode_result(&result)?).as_ref(),
            Some(&result)
        );
        Ok(())
    }

    /// An `emissions` key holding something other than `(kind, text)` pairs
    /// is a damaged row, and a damaged row is a miss rather than half an
    /// answer.
    #[test]
    fn a_result_row_with_a_malformed_emission_list_is_refused()
    -> Result<(), Box<dyn core::error::Error>> {
        let bad = canon::encode(&CanonValue::map([
            ("status", CanonValue::str("ok")),
            ("value", CanonValue::str("1")),
            ("emissions", CanonValue::array([CanonValue::int(7)])),
        ]))?;
        assert_eq!(decode_result(&bad), None);
        Ok(())
    }

    #[test]
    fn the_recorder_logs_every_kind_of_question_in_order() {
        let inner = fake();
        let host = RecordingHost::new(&inner);
        drop(host.read_file(&crate::value2::ambient_path("/a")));
        let _ = host.path_exists_checked(&crate::value2::ambient_path("/a"));
        drop(host.get_env("SET"));
        let read_set = host.take();
        assert_eq!(read_set.len(), 3);
        assert_eq!(
            read_set.questions(),
            vec![
                Question::ReadFile(crate::value2::ambient_path("/a")),
                Question::PathExists(crate::value2::ambient_path("/a")),
                Question::GetEnv("SET".to_owned()),
            ]
        );
        // Taking empties it, so the next evaluation starts clean.
        assert!(host.take().is_empty());
    }

    #[test]
    fn import_replay_reasks_the_callers_original_path() {
        struct MovingImport(Cell<bool>);
        impl Host for MovingImport {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(
                read_file_bytes,
                get_env,
                copy_to_store,
                ensure_path,
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                fetch_tree,
                lock_flake,
                parse_flake_ref,
                flake_ref_to_string,
                warn,
                trace,
                find_file,
                nix_path,
                not_async,
            );
            fn import_source(
                &self,
                path: &crate::value2::PathValue,
            ) -> Result<crate::host::ImportedSource, String> {
                let target = if path.ends_with("current.nix") && self.0.get() {
                    "v2.nix"
                } else {
                    "v1.nix"
                };
                let mount = "/nix/store/00000000000000000000000000000000-source";
                Ok(crate::host::ImportedSource::Nix {
                    path: crate::value2::PathValue::new(
                        crate::value2::Root::mounted(mount),
                        format!("{mount}/{target}"),
                    ),
                    text: target.to_owned(),
                })
            }
            fn read_file(&self, _path: &crate::value2::PathValue) -> Result<String, String> {
                Err("not asked".to_owned())
            }
            fn read_dir(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Err("not asked".to_owned())
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
                Ok(None)
            }
            fn file_type_resolved(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<FileType, String> {
                Err("not asked".to_owned())
            }
        }

        let inner = MovingImport(Cell::new(false));
        let recorder = RecordingHost::new(&inner);
        let asked = crate::value2::ambient_path(
            "/nix/store/00000000000000000000000000000000-source/current.nix",
        );
        recorder.import_source(&asked).expect("first import");
        let recorded = recorder.take();
        assert_eq!(recorded.questions(), vec![Question::Import(asked)]);

        inner.0.set(true);
        let identity = id(b"moving import");
        assert_ne!(
            recorded.key(&identity),
            replayed(&recorded.questions(), &inner).key(&identity)
        );
    }

    /// A read under a mounted root replays from its recorded digest and the
    /// host is never asked; the same read under an ambient root is asked
    /// again and keys from what the host says now. Both directions in one
    /// host so the recorded-side check cannot pass by the host answering
    /// the recorded value: it answers something else, and the mounted key
    /// still equals the recording while the ambient key does not.
    #[test]
    fn a_read_under_a_mounted_root_replays_from_the_record_without_asking() {
        struct Listing(Cell<bool>, RefCell<Vec<String>>);
        impl Host for Listing {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(
                read_file_bytes,
                get_env,
                copy_to_store,
                ensure_path,
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                fetch_tree,
                lock_flake,
                parse_flake_ref,
                flake_ref_to_string,
                warn,
                trace,
                find_file,
                nix_path,
                file_type_resolved,
                not_async,
            );
            fn read_file(&self, _path: &crate::value2::PathValue) -> Result<String, String> {
                Err("not asked".to_owned())
            }
            fn read_dir(
                &self,
                path: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                self.1.borrow_mut().push(path.accessor_path().to_owned());
                let name = if self.0.get() { "after" } else { "before" };
                Ok(vec![(name.to_owned(), FileType::Regular)])
            }
            fn path_exists_checked(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(&self, _path: &crate::value2::PathValue) -> Result<bool, String> {
                Ok(false)
            }
            fn file_type(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<Option<FileType>, String> {
                Ok(None)
            }
        }

        let mount = "/nix/store/00000000000000000000000000000000-source";
        let mounted = Rc::new(crate::value2::PathValue::new(
            crate::value2::Root::mounted(mount),
            format!("{mount}/lib"),
        ));
        let ambient = crate::value2::ambient_path("/etc/lib");
        let none = crate::host::Sealed::new();
        assert!(Question::ReadDir(mounted.clone()).replays_from_record(&none, None));
        assert!(!Question::ReadDir(ambient.clone()).replays_from_record(&none, None));
        assert!(
            !Question::CopyToStore(mounted.clone()).replays_from_record(&none, None),
            "an effect under a mounted root still asks, or replays by validity"
        );

        let inner = Listing(Cell::new(false), RefCell::new(Vec::new()));
        let recorder = RecordingHost::new(&inner);
        recorder.read_dir(&mounted).expect("mounted listing");
        recorder.read_dir(&ambient).expect("ambient listing");
        let recorded = recorder.take();
        assert_eq!(recorded.entries().len(), 2);
        let recorded_mounted_digest = recorded.entries()[0].1.digest();
        inner.1.borrow_mut().clear();

        // The world moves: every listing now answers differently.
        inner.0.set(true);
        let cas = ix_kernel::cas::MemoryCas::new();
        let observed = ReadSet::replay(
            recorded.entries(),
            &inner,
            &crate::eval::Settings::default(),
            &cas,
        )
        .expect("replay")
        .expect("not refused");
        assert_eq!(
            *inner.1.borrow(),
            vec!["/etc/lib".to_owned()],
            "only the ambient listing was asked again"
        );
        assert_eq!(
            observed.entries()[0].1.digest(),
            recorded_mounted_digest,
            "the mounted row keeps its recorded digest"
        );
        assert_ne!(
            observed.entries()[1].1.digest(),
            recorded.entries()[1].1.digest(),
            "the ambient row keys from the answer given now"
        );
        let identity = id(b"mounted-vs-ambient");
        assert_ne!(recorded.key(&identity), observed.key(&identity));
    }

    #[test]
    fn vanished_mount_is_not_replayed_as_the_same_negative_existence_answer() {
        struct Existence(Cell<bool>);
        impl Host for Existence {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(
                read_file_bytes,
                get_env,
                copy_to_store,
                ensure_path,
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                fetch_tree,
                lock_flake,
                parse_flake_ref,
                flake_ref_to_string,
                warn,
                trace,
                find_file,
                nix_path,
                file_type_resolved,
                not_async,
            );
            fn read_file(&self, _path: &crate::value2::PathValue) -> Result<String, String> {
                Err("not asked".to_owned())
            }
            fn read_dir(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Err("not asked".to_owned())
            }
            fn path_exists_checked(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<bool, String> {
                if self.0.get() {
                    Err("mounted root disappeared".to_owned())
                } else {
                    Ok(false)
                }
            }
            fn dir_exists_checked(&self, _path: &crate::value2::PathValue) -> Result<bool, String> {
                if self.0.get() {
                    Err("mounted root disappeared".to_owned())
                } else {
                    Ok(false)
                }
            }
            fn file_type(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<Option<FileType>, String> {
                Ok(None)
            }
        }

        let inner = Existence(Cell::new(false));
        let recorder = RecordingHost::new(&inner);
        let path = crate::value2::ambient_path("/missing");
        assert_eq!(recorder.path_exists_checked(&path), Ok(false));
        let recorded = recorder.take();
        inner.0.set(true);
        let identity = id(b"existence");
        assert_ne!(
            recorded.key(&identity),
            replayed(&recorded.questions(), &inner).key(&identity)
        );

        inner.0.set(false);
        let recorder = RecordingHost::new(&inner);
        let path = crate::value2::ambient_path("/missing/");
        assert_eq!(recorder.dir_exists_checked(&path), Ok(false));
        let recorded = recorder.take();
        assert_eq!(recorded.questions(), vec![Question::DirExists(path)]);
        inner.0.set(true);
        let identity = id(b"directory-existence");
        assert_ne!(
            recorded.key(&identity),
            replayed(&recorded.questions(), &inner).key(&identity)
        );
    }

    /// An `import`'s world-read has to land in the log, and under the name of
    /// the question that was actually asked.
    ///
    /// `Host::resolve_import` keeps its default body on `RecordingHost`, so
    /// what records the read is whichever method that body calls. When it
    /// called `file_type` the log said `FileType`; it now calls
    /// `file_type_resolved` and the log has to say `FileTypeResolved`, or a
    /// replay would ask the `lstat` and key on an answer the evaluation never
    /// saw. Break `RecordingHost::file_type_resolved`'s `note` and this is
    /// what fails.
    #[test]
    fn an_import_records_the_resolving_kind_question_and_not_the_plain_one() {
        struct Dir;
        impl Host for Dir {
            crate::host::host_stubs!(settle);
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
                get_env,
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _path: &crate::value2::PathValue) -> Result<String, String> {
                Err("not asked".to_owned())
            }
            fn read_dir(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Err("not asked".to_owned())
            }
            fn path_exists_checked(
                &self,
                _path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            // The disagreeing pair a symlink to a directory produces, so the
            // log says which of the two was asked.
            fn file_type(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<Option<FileType>, String> {
                Ok(Some(FileType::Symlink))
            }
            fn file_type_resolved(
                &self,
                _path: &crate::value2::PathValue,
            ) -> Result<FileType, String> {
                Ok(FileType::Directory)
            }
        }

        let inner = Dir;
        let host = RecordingHost::new(&inner);
        assert_eq!(
            host.resolve_import(&crate::value2::ambient_path("/link-to-dir"))
                .ok()
                .map(|p| p.to_string())
                .as_deref(),
            Some("/link-to-dir/default.nix"),
        );
        assert_eq!(
            host.take().questions(),
            vec![Question::FileTypeResolved(crate::value2::ambient_path(
                "/link-to-dir"
            ))],
        );
    }

    #[test]
    fn replaying_an_unchanged_world_reproduces_the_key() {
        let inner = fake();
        let host = RecordingHost::new(&inner);
        drop(host.read_file(&crate::value2::ambient_path("/a")));
        let recorded = host.take();
        let module = id(b"module");
        assert_eq!(
            recorded.key(&module),
            replayed(&recorded.questions(), &inner).key(&module)
        );
    }

    /// The property the memoisation rests on: a changed answer changes the
    /// key, so the lookup goes somewhere no result was stored.
    #[test]
    fn a_changed_file_changes_the_key() {
        let inner = fake();
        let host = RecordingHost::new(&inner);
        drop(host.read_file(&crate::value2::ambient_path("/a")));
        let recorded = host.take();
        let module = id(b"module");
        inner.contents.set(2);
        assert_ne!(
            recorded.key(&module),
            replayed(&recorded.questions(), &inner).key(&module)
        );
    }

    #[test]
    fn a_changed_environment_variable_changes_the_key() {
        struct Env(Cell<bool>);
        impl Host for Env {
            crate::host::host_stubs!(settle);
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
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
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
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
            fn get_env(&self, _n: &str) -> Option<String> {
                if self.0.get() {
                    Some("a".to_owned())
                } else {
                    None
                }
            }
        }
        let inner = Env(Cell::new(true));
        let host = RecordingHost::new(&inner);
        drop(host.get_env("V"));
        let recorded = host.take();
        let module = id(b"module");
        inner.0.set(false);
        assert_ne!(
            recorded.key(&module),
            replayed(&recorded.questions(), &inner).key(&module)
        );
    }

    /// Unset and empty are different facts. Digesting them the same way would
    /// serve a result taken with the variable unset to a run where it is set
    /// to the empty string.
    #[test]
    fn an_unset_variable_does_not_digest_as_an_empty_one() {
        struct Unset;
        impl Host for Unset {
            crate::host::host_stubs!(settle);
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
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
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
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
            fn get_env(&self, _n: &str) -> Option<String> {
                None
            }
        }
        struct Empty;
        impl Host for Empty {
            crate::host::host_stubs!(settle);
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
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
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
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err("no".to_owned())
            }
            fn get_env(&self, _n: &str) -> Option<String> {
                Some(String::new())
            }
        }
        let question = Question::GetEnv("V".to_owned());
        let cas = ix_kernel::cas::MemoryCas::new();
        assert_ne!(
            question.ask(&Unset, &cas).expect("plain replay"),
            question.ask(&Empty, &cas).expect("plain replay")
        );
    }

    /// A read error is part of the answer, not an absence of one: a file that
    /// did not exist and later does must not hit.
    #[test]
    fn a_file_appearing_changes_the_key() {
        let inner = fake();
        let host = RecordingHost::new(&inner);
        drop(host.read_file(&crate::value2::ambient_path("/missing")));
        let recorded = host.take();
        let module = id(b"module");

        struct NowPresent;
        impl Host for NowPresent {
            crate::host::host_stubs!(settle);
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
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Ok("appeared".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
        }
        assert_ne!(
            recorded.key(&module),
            replayed(&recorded.questions(), &NowPresent).key(&module)
        );
    }

    /// Order is part of the key, because the question sequence is itself a
    /// function of the answers.
    #[test]
    fn question_order_is_part_of_the_key() {
        let inner = fake();
        let module = id(b"module");

        let one = RecordingHost::new(&inner);
        drop(one.read_file(&crate::value2::ambient_path("/a")));
        let _ = one.path_exists_checked(&crate::value2::ambient_path("/a"));
        let first = one.take();

        let other = RecordingHost::new(&inner);
        let _ = other.path_exists_checked(&crate::value2::ambient_path("/a"));
        drop(other.read_file(&crate::value2::ambient_path("/a")));
        let second = other.take();

        assert_ne!(first.key(&module), second.key(&module));
    }

    /// The impurity guard for a coercion rather than a builtin.
    ///
    /// `builtins.purity_tests` enumerates BUILTINS that must go through
    /// `Host`, so it says nothing about `"${/a}"`, which is an op. That
    /// coercion is impure -- its answer is a hash of the file -- and until
    /// ENG-12447 it did not reach `Host` at all, so a read set could not see
    /// it and a memoised result that embedded a store path would have
    /// survived an edit to the file behind it. Drive a real evaluation, not
    /// the trait method, because the thing being asserted is the wiring.
    #[test]
    fn interpolating_a_path_is_a_question_the_read_set_sees() {
        struct WithStore(Cell<u8>);
        impl Host for WithStore {
            crate::host::host_stubs!(settle);
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
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Err("no".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
            /// A store path is a hash of the content, so an edit moves it.
            fn copy_to_store(&self, path: &crate::value2::PathValue) -> Result<String, StoreError> {
                Ok(format!(
                    "/nix/store/{}-{}",
                    self.0.get(),
                    path.trim_start_matches('/')
                ))
            }
        }

        let inner = WithStore(Cell::new(1));
        let host = RecordingHost::new(&inner);
        let rendered = match crate::compile::compile_source(
            r#""${/a}""#,
            "/",
            crate::compile::Origin::String,
            &crate::eval::Settings::default(),
        ) {
            Err(e) => format!("compile failed: {e:?}"),
            Ok(module) => {
                let mut vm = crate::vm::Vm::with_settings(crate::eval::Settings::default());
                vm.start_module(&std::rc::Rc::new(module));
                match crate::eval::drive(&mut vm, &host) {
                    Ok(crate::value2::Value::Str(s)) => s.expect_text(),
                    Ok(other) => format!("not a string: {other:?}"),
                    Err(e) => format!("evaluation failed: {e:?}"),
                }
            }
        };
        assert_eq!(rendered, "/nix/store/1-a");

        let recorded = host.take();
        assert_eq!(
            recorded.questions(),
            vec![Question::CopyToStore(crate::value2::ambient_path("/a"))],
            "the copy did not reach the host, so no read set can see it"
        );

        // And it invalidates: the same file with different content is a
        // different store path, hence a different key.
        let module_hash = id(b"module");
        inner.0.set(2);
        assert_ne!(
            recorded.key(&module_hash),
            replayed(&recorded.questions(), &inner).key(&module_hash)
        );
    }

    /// Two fetches of the same URL are two different questions when they
    /// differ in name, kind or pin, and the key has to say so -- each of the
    /// three changes the store path the answer names.
    ///
    /// Written for the reason the filtered-copy test below was: dropping a
    /// field from [`Question::key_parts`] leaves the codec round trip green,
    /// because that is a different encoding of the same request.
    #[test]
    fn two_fetches_of_one_url_do_not_share_a_key() {
        let request = |name: &str, kind: crate::task::FetchKind, sha: Option<&str>| {
            Question::Fetch(Box::new(crate::task::FetchRequest {
                url: "https://u/x.tar.gz".to_owned(),
                name: name.to_owned(),
                kind,
                expected_sha256: sha.map(str::to_owned),
            }))
        };
        let key = |q: Question| {
            let mut set = ReadSet::default();
            set.entries
                .push((q, Recorded::Digest(digest(&[b"same-answer"]))));
            set.key(&id(b"module"))
        };
        let pin = "sha256-1BdlSaqjNlSVCcgD/PocqAwbnGQ+lyfL6h9WK6+MCJc=";
        let base = key(request(
            "source",
            crate::task::FetchKind::Tarball,
            Some(pin),
        ));
        // A different name: a different store path for identical bytes.
        assert_ne!(
            base,
            key(request("other", crate::task::FetchKind::Tarball, Some(pin)))
        );
        // A different kind: flat ingestion of the tarball rather than the
        // unpacked tree.
        assert_ne!(
            base,
            key(request("source", crate::task::FetchKind::File, Some(pin)))
        );
        // Unpinned. The one that matters most: an unpinned fetch of a URL
        // whose bytes moved must not hit a row recorded when it was pinned.
        assert_ne!(
            base,
            key(request("source", crate::task::FetchKind::Tarball, None))
        );
        // And a different pin.
        assert_ne!(
            base,
            key(request(
                "source",
                crate::task::FetchKind::Tarball,
                Some("sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            ))
        );
    }

    /// Two filtered copies of the same root under the same name are two
    /// different questions when the filter accepted different files, and the
    /// key has to say so.
    ///
    /// The gap this closes was found by mutation, not by reading: dropping the
    /// accepted list from [`Question::key_parts`] left every test green. The
    /// codec round trip did not catch it because it exercises
    /// [`question_value`], which is a *different* encoding of the same
    /// request -- two encodings, so two things to get wrong.
    #[test]
    fn two_filtered_copies_of_one_root_do_not_share_a_key() {
        let request = |accepted: Option<Vec<crate::task::AcceptedPath>>, name: &str| {
            Question::StoreFiltered(Box::new(crate::task::FilteredCopy {
                root: crate::value2::ambient_path("/src"),
                name: name.to_owned(),
                method: crate::task::PathMethod::NixArchive,
                accepted,
                expected_sha256: None,
                inherit_references: false,
            }))
        };
        let entry = |path: &str| crate::task::AcceptedPath {
            path: path.to_owned(),
            file_type: FileType::Regular,
        };
        let key = |q: Question| {
            let mut set = ReadSet::default();
            set.entries
                .push((q, Recorded::Digest(digest(&[b"same-answer"]))));
            set.key(&id(b"module"))
        };

        let base = key(request(Some(vec![entry("/src/a")]), "src"));
        // A different accepted file: a different tree, a different NAR.
        assert_ne!(base, key(request(Some(vec![entry("/src/b")]), "src")));
        // One more accepted file.
        assert_ne!(
            base,
            key(request(Some(vec![entry("/src/a"), entry("/src/b")]), "src"))
        );
        // A different type for the same path: a symlink and a regular file
        // serialise differently.
        assert_ne!(
            base,
            key(request(
                Some(vec![crate::task::AcceptedPath {
                    path: "/src/a".to_owned(),
                    file_type: FileType::Symlink,
                }]),
                "src"
            ))
        );
        // "accepted nothing" and "no filtering" are different requests.
        assert_ne!(
            key(request(Some(Vec::new()), "src")),
            key(request(None, "src"))
        );
        // And the name is in the key, because it is in the store path.
        assert_ne!(base, key(request(Some(vec![entry("/src/a")]), "other")));
    }

    /// An evaluation that read nothing still has a key, and it is the module's
    /// alone: two different modules that read nothing must not share it.
    #[test]
    fn an_empty_read_set_keys_on_the_module() {
        let empty = ReadSet::default();
        assert_ne!(empty.key(&id(b"one")), empty.key(&id(b"two")));
    }

    // ---- result cache ----------------------------------------------------

    use ix_kernel::cas::MemoryCas;

    fn result(value: &str) -> EvalResult {
        EvalResult {
            status: "ok".to_owned(),
            value: value.to_owned(),
            emissions: Vec::new(),
            token: None,
            pos: None,
        }
    }

    /// A legacy witness is a corrupt stored row, not an absent row. Reporting
    /// its identity makes a store full of retired files visible in stats.
    #[test]
    fn an_unversioned_persistent_witness_is_reported_as_corruption()
    -> Result<(), Box<dyn core::error::Error>> {
        crate::perf::reset();
        let dir = crate::eval::scratch_dir("ixe-result-cache", "legacy-witness");
        let store = crate::store::Store::open(&dir)?;
        let module = hash::tagged("legacy-module", &[b"unchanged source"]);
        let identity = EvalId::of(
            &module,
            &crate::eval::Settings::default(),
            &crate::session::Arguments::none(),
            &crate::session::Question::Whole {
                render: crate::session::RenderMode::Plain,
            },
        );
        std::fs::write(store.witness().path(&identity), legacy_witness(&module))?;

        let mut cache = ResultCache::persistent(&store);
        assert_eq!(
            cache.lookup(&identity, &fake(), &crate::eval::Settings::default()),
            None
        );
        let complaints = cache.take_corruption();
        assert_eq!(
            complaints.len(),
            1,
            "legacy row was not counted: {complaints:?}"
        );
        assert!(
            complaints[0].message.contains(&identity.as_hash().to_hex()),
            "complaint does not identify the refused row: {complaints:?}"
        );
        assert_eq!(
            crate::perf::snapshot().cache_witnesses_refused,
            u64::from(cfg!(feature = "perf")),
            "the refused witness did not reach the stats snapshot"
        );

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// A record on a capped store is followed by a sweep to the cap
    /// (`Store::sweep_to_cap`), and the entry just recorded is the newest, so
    /// it survives: the cap costs old entries, never the answer just paid for.
    #[test]
    fn a_record_on_a_capped_store_sweeps_it_to_the_cap() -> Result<(), Box<dyn core::error::Error>>
    {
        let dir = crate::eval::scratch_dir("ixe-result-cache", "capped-record");
        let store = crate::store::Store::open(&dir)?;
        let cas = ix_kernel::DirCas::open(store.objects_dir())?;
        // Old rows nothing will look up again, aged explicitly so the order
        // does not depend on filesystem timestamp granularity.
        for i in 0..8_u8 {
            let mut payload = vec![b'j'; 4096];
            payload.push(i);
            let output = cas.put(&payload)?;
            store.rows().put(eval_domain(), &[i], output)?;
        }
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for row in store.rows().inventory(eval_domain())? {
            std::fs::File::open(&row.path)?.set_modified(old)?;
        }
        let cap = store.size()? / 2;
        let capped = crate::store::Store::open(&dir)?.with_max_bytes(cap);
        let settings = crate::eval::Settings::default();
        let identity = EvalId::of(
            &hash::tagged("module", &[b"capped"]),
            &settings,
            &crate::session::Arguments::none(),
            &crate::session::Question::Whole {
                render: crate::session::RenderMode::Plain,
            },
        );
        let answer = result("null");

        let mut results = ResultCache::persistent(&capped);
        results.record(&identity, &ReadSet::default(), &answer, &fake(), &settings)?;
        let complaints = results.take_corruption();
        let size = capped.size()?;
        let served = ResultCache::persistent(&capped).lookup(&identity, &fake(), &settings);
        let refs = std::fs::read_to_string(capped.witness().refs_path(&identity))?;
        std::fs::remove_dir_all(&dir)?;

        assert!(complaints.is_empty(), "{complaints:?}");
        assert!(
            size <= cap,
            "the store is {size} bytes against a cap of {cap} after a record"
        );
        assert_eq!(
            served,
            Some(answer),
            "the record that triggered the sweep was swept"
        );
        assert_eq!(
            witness_refs(&refs),
            Some(Vec::new()),
            "a witness with no derivation rows names objects"
        );
        Ok(())
    }

    /// The evaluator fingerprint is fixed by the production constructor. A
    /// separate constructor exists only in this unit-test build so the stale
    /// cross-evaluator row can be written without exposing that choice to
    /// callers.
    #[test]
    fn an_evaluator_change_cannot_hit_an_old_persistent_result()
    -> Result<(), Box<dyn core::error::Error>> {
        let dir = crate::eval::scratch_dir("ixe-result-cache", "evaluator-fingerprint");
        let store = crate::store::Store::open(&dir)?;
        let settings = crate::eval::Settings::default();
        let arguments = crate::session::Arguments::none();
        let question = crate::session::Question::Whole {
            render: crate::session::RenderMode::Plain,
        };
        let module = hash::tagged("module", &[b"unchanged importing root"]);
        let before = EvalId::with_evaluator_fingerprint_for_test(
            &module,
            "evaluator-before-per-attribute-positions",
            &settings,
            &arguments,
            &question,
        );
        let fixed = EvalId::with_evaluator_fingerprint_for_test(
            &module,
            "evaluator-with-per-attribute-positions",
            &settings,
            &arguments,
            &question,
        );
        let stale = result("null");

        {
            let mut results = ResultCache::persistent(&store);
            results.record(&before, &ReadSet::default(), &stale, &fake(), &settings)?;
        }

        let mut reopened = ResultCache::persistent(&store);
        assert_eq!(
            reopened.lookup(&before, &fake(), &settings),
            Some(stale),
            "the control row was not persisted, so the cross-evaluator miss proves nothing"
        );
        assert_eq!(
            reopened.lookup(&fixed, &fake(), &settings),
            None,
            "the fixed evaluator was served the stale result recorded by its predecessor"
        );
        assert_eq!(
            EvalId::of(&module, &settings, &arguments, &question),
            EvalId::with_evaluator_fingerprint_for_test(
                &module,
                crate::modcache::compiler_fingerprint(),
                &settings,
                &arguments,
                &question,
            ),
            "the production identity did not use the compiler fingerprint"
        );

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// `record` pauses after publishing its CAS objects while a sweep proves
    /// that the publication lock is held. The sweep then runs as soon as the
    /// witness and row are present, and a fresh cache must still serve them.
    #[test]
    fn a_sweep_cannot_observe_half_of_a_record() -> Result<(), Box<dyn core::error::Error>> {
        let dir = crate::eval::scratch_dir("ixe-result-cache", "record-sweep");
        let store = crate::store::Store::open(&dir)?;
        let module = store.cas().put(b"compiled module rooted by a row")?;
        store
            .rows()
            .put(crate::modcache::compile_domain(), b"module-row", module)?;
        let identity = EvalId::of(
            module.hash(),
            &crate::eval::Settings::default(),
            &crate::session::Arguments::none(),
            &crate::session::Question::Whole {
                render: crate::session::RenderMode::Plain,
            },
        );
        let inner = fake();
        let recorder = RecordingHost::new(&inner);
        drop(recorder.read_file(&crate::value2::ambient_path("/a")));
        let read_set = recorder.take();

        let (start_sweep, wait_for_record) = std::sync::mpsc::sync_channel(0);
        let (attempted_lock, wait_for_attempt) = std::sync::mpsc::sync_channel(0);
        let sweeping_store = store.clone();
        let sweep = std::thread::spawn(move || -> std::io::Result<_> {
            wait_for_record.recv().map_err(std::io::Error::other)?;
            let blocked = match sweeping_store.try_publication_guard() {
                Ok(guard) => {
                    drop(guard);
                    false
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
                Err(error) => return Err(error),
            };
            attempted_lock
                .send(blocked)
                .map_err(std::io::Error::other)?;
            sweeping_store.sweep(u64::MAX)
        });

        let mut cache = ResultCache::persistent(&store);
        cache.record_with_publication_hook(
            &identity,
            &read_set,
            &result("served"),
            &inner,
            &crate::eval::Settings::default(),
            || {
                start_sweep
                    .send(())
                    .expect("the sweep thread stopped before the sequencing point");
                assert!(
                    wait_for_attempt
                        .recv()
                        .expect("the sweep thread did not report its lock attempt"),
                    "sweep acquired the store while record had published objects but no roots"
                );
            },
        )?;
        sweep.join().expect("the sweep thread panicked")?;

        let mut reopened = ResultCache::persistent(&store);
        assert_eq!(
            reopened.lookup(&identity, &inner, &crate::eval::Settings::default()),
            Some(result("served")),
            "the sweep left the completed row unservable"
        );

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// A sweep after an in-memory row is selected but before its object is
    /// read turns that row into a miss. The miss must evict both hints so the
    /// following record republishes bytes rather than reusing the dangling
    /// address, and a fresh process must be able to serve the repaired row.
    #[test]
    fn a_result_swept_between_row_and_object_is_republished()
    -> Result<(), Box<dyn core::error::Error>> {
        let dir = crate::eval::scratch_dir("ixe-result-cache", "lookup-sweep-record");
        let store = crate::store::Store::open(&dir)?;
        let module = store.cas().put(b"compiled module rooted by a row")?;
        store
            .rows()
            .put(crate::modcache::compile_domain(), b"module-row", module)?;
        let identity = EvalId::of(
            module.hash(),
            &crate::eval::Settings::default(),
            &crate::session::Arguments::none(),
            &crate::session::Question::Whole {
                render: crate::session::RenderMode::Plain,
            },
        );
        let inner = fake();
        let recorder = RecordingHost::new(&inner);
        drop(recorder.read_file(&crate::value2::ambient_path("/a")));
        let read_set = recorder.take();

        let mut cache = ResultCache::persistent(&store);
        cache.record(
            &identity,
            &read_set,
            &result("before sweep"),
            &inner,
            &crate::eval::Settings::default(),
        )?;
        assert_eq!(
            cache.lookup_with_object_hook(
                &identity,
                &inner,
                &crate::eval::Settings::default(),
                || {
                    let guard = store
                        .try_publication_guard()
                        .expect("a result lookup held the publication lock");
                    drop(guard);
                    store
                        .sweep(0)
                        .expect("the sweep failed at the lookup sequencing point");
                },
            ),
            None,
            "lookup served an object removed after selecting its row"
        );
        let key = row_key(&read_set.key(&identity))?;
        assert!(
            cache.table.get(eval_domain(), key).is_none(),
            "failed lookup retained the in-memory row for the swept object"
        );
        assert!(
            !cache.witness.contains_key(&identity),
            "failed lookup retained the in-memory witness for the swept object"
        );

        cache.record(
            &identity,
            &read_set,
            &result("after sweep"),
            &inner,
            &crate::eval::Settings::default(),
        )?;
        let mut reopened = ResultCache::persistent(&store);
        assert_eq!(
            reopened.lookup(&identity, &inner, &crate::eval::Settings::default()),
            Some(result("after sweep")),
            "record republished a row without restoring its object"
        );

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// A reused ATerm is still sweepable until the witness names it. The
    /// result publication lock must cover that interval too.
    #[test]
    fn a_sweep_cannot_observe_a_reused_aterm_without_its_witness()
    -> Result<(), Box<dyn core::error::Error>> {
        let dir = crate::eval::scratch_dir("ixe-result-cache", "aterm-record-sweep");
        let store = crate::store::Store::open(&dir)?;
        let module = store.cas().put(b"compiled module rooted by a row")?;
        store
            .rows()
            .put(crate::modcache::compile_domain(), b"module-row", module)?;
        let identity = EvalId::of(
            module.hash(),
            &crate::eval::Settings::default(),
            &crate::session::Arguments::none(),
            &crate::session::Question::Whole {
                render: crate::session::RenderMode::Plain,
            },
        );
        let inner = fake();
        let recorder = RecordingHost::new(&inner);
        let aterm = b"Derive([])";
        drop(recorder.write_derivation("a", "Derive([])"));
        let read_set = recorder.take();
        assert_eq!(store.cas().put(aterm)?, ObjId::of(aterm));

        let (start_sweep, wait_for_record) = std::sync::mpsc::sync_channel(0);
        let (attempted_lock, wait_for_attempt) = std::sync::mpsc::sync_channel(0);
        let sweeping_store = store.clone();
        let sweep = std::thread::spawn(move || -> std::io::Result<_> {
            wait_for_record.recv().map_err(std::io::Error::other)?;
            let blocked = match sweeping_store.try_publication_guard() {
                Ok(guard) => {
                    drop(guard);
                    false
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
                Err(error) => return Err(error),
            };
            attempted_lock
                .send(blocked)
                .map_err(std::io::Error::other)?;
            sweeping_store.sweep(u64::MAX)
        });

        let mut cache = ResultCache::persistent(&store);
        cache.record_with_aterm_publication_hook(
            &identity,
            &read_set,
            &result("served"),
            &inner,
            &crate::eval::Settings::default(),
            || {
                start_sweep
                    .send(())
                    .expect("the sweep thread stopped before the ATerm sequencing point");
                assert!(
                    wait_for_attempt
                        .recv()
                        .expect("the sweep thread did not report its lock attempt"),
                    "sweep acquired the store after finding the ATerm but before its witness"
                );
            },
        )?;
        sweep.join().expect("the sweep thread panicked")?;

        let mut reopened = ResultCache::persistent(&store);
        assert_eq!(
            reopened.lookup(&identity, &inner, &crate::eval::Settings::default()),
            Some(result("served")),
            "the sweep left the ATerm-backed row unservable"
        );

        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// Record an evaluation of `/a`, then look it up with nothing changed.
    #[test]
    fn an_unchanged_world_serves_the_memoised_result() -> Result<(), Box<dyn core::error::Error>> {
        let inner = fake();
        let cas = MemoryCas::new();
        let mut cache = ResultCache::new(&cas);
        let module = id(b"module");

        let recorder = RecordingHost::new(&inner);
        drop(recorder.read_file(&crate::value2::ambient_path("/a")));
        cache.record(
            &module,
            &recorder.take(),
            &result("v1"),
            &inner,
            &crate::eval::Settings::default(),
        )?;

        assert_eq!(
            cache.lookup(&module, &inner, &crate::eval::Settings::default()),
            Some(result("v1"))
        );
        assert_eq!((cache.hits(), cache.wasted_replays()), (1, 0));
        Ok(())
    }

    /// The property the whole design turns on: editing a file the evaluation
    /// read makes the lookup miss, without anything having to notice the edit.
    #[test]
    fn editing_a_file_that_was_read_makes_the_lookup_miss()
    -> Result<(), Box<dyn core::error::Error>> {
        let inner = fake();
        let cas = MemoryCas::new();
        let mut cache = ResultCache::new(&cas);
        let module = id(b"module");

        let recorder = RecordingHost::new(&inner);
        drop(recorder.read_file(&crate::value2::ambient_path("/a")));
        cache.record(
            &module,
            &recorder.take(),
            &result("v1"),
            &inner,
            &crate::eval::Settings::default(),
        )?;
        assert!(
            cache
                .lookup(&module, &inner, &crate::eval::Settings::default())
                .is_some()
        );

        inner.contents.set(2);
        assert_eq!(
            cache.lookup(&module, &inner, &crate::eval::Settings::default()),
            None,
            "served a pre-edit result"
        );
        Ok(())
    }

    /// A file that was not read is not part of the key, so touching it must
    /// not cost a hit. Over-invalidating is safe but makes the cache useless,
    /// and nothing else in the suite would notice.
    #[test]
    fn editing_a_file_that_was_not_read_still_hits() -> Result<(), Box<dyn core::error::Error>> {
        let inner = fake();
        let cas = MemoryCas::new();
        let mut cache = ResultCache::new(&cas);
        let module = id(b"module");

        let recorder = RecordingHost::new(&inner);
        drop(recorder.get_env("SET"));
        cache.record(
            &module,
            &recorder.take(),
            &result("v1"),
            &inner,
            &crate::eval::Settings::default(),
        )?;

        // /a changes; the recorded evaluation never read it.
        inner.contents.set(2);
        assert_eq!(
            cache.lookup(&module, &inner, &crate::eval::Settings::default()),
            Some(result("v1"))
        );
        Ok(())
    }

    #[test]
    fn reverting_an_edit_reuses_the_previous_result_row() -> Result<(), Box<dyn core::error::Error>>
    {
        let inner = fake();
        let cas = MemoryCas::new();
        let mut cache = ResultCache::new(&cas);
        let identity = id(b"reverted-edit");
        let settings = crate::eval::Settings::default();
        for (version, value) in [(1, "before"), (2, "after")] {
            inner.contents.set(version);
            let recorder = RecordingHost::new(&inner);
            drop(recorder.read_file(&crate::value2::ambient_path("/a")));
            cache.record(
                &identity,
                &recorder.take(),
                &result(value),
                &inner,
                &settings,
            )?;
            assert_eq!(
                cache.lookup(&identity, &inner, &settings),
                Some(result(value))
            );
        }
        inner.contents.set(1);
        assert_eq!(
            cache.lookup(&identity, &inner, &settings),
            Some(result("before"))
        );
        assert_eq!(cache.hits(), 3);
        Ok(())
    }

    /// Two modules that read the same things do not share a row.
    #[test]
    fn the_module_is_part_of_the_key() -> Result<(), Box<dyn core::error::Error>> {
        let inner = fake();
        let cas = MemoryCas::new();
        let mut cache = ResultCache::new(&cas);
        let one = id(b"one");
        let other = id(b"two");

        let recorder = RecordingHost::new(&inner);
        drop(recorder.read_file(&crate::value2::ambient_path("/a")));
        cache.record(
            &one,
            &recorder.take(),
            &result("from-one"),
            &inner,
            &crate::eval::Settings::default(),
        )?;

        assert_eq!(
            cache.lookup(&one, &inner, &crate::eval::Settings::default()),
            Some(result("from-one"))
        );
        assert_eq!(
            cache.lookup(&other, &inner, &crate::eval::Settings::default()),
            None
        );
        Ok(())
    }

    /// A witness that no longer describes what the evaluation would ask is a
    /// miss, never a wrong answer: the key is built from the answers observed
    /// now, so it addresses a row nothing was ever stored under.
    #[test]
    fn a_stale_witness_misses_rather_than_answering_wrongly()
    -> Result<(), Box<dyn core::error::Error>> {
        let inner = fake();
        let cas = MemoryCas::new();
        let mut cache = ResultCache::new(&cas);
        let module = id(b"module");

        // Recorded reading /a and /missing, in that order.
        let recorder = RecordingHost::new(&inner);
        drop(recorder.read_file(&crate::value2::ambient_path("/a")));
        drop(recorder.read_file(&crate::value2::ambient_path("/missing")));
        cache.record(
            &module,
            &recorder.take(),
            &result("two-reads"),
            &inner,
            &crate::eval::Settings::default(),
        )?;
        assert!(
            cache
                .lookup(&module, &inner, &crate::eval::Settings::default())
                .is_some()
        );

        // A world where the second read now succeeds. The witness still names
        // both files, so replay asks both and gets a different answer for one.
        struct BothPresent;
        impl Host for BothPresent {
            crate::host::host_stubs!(settle);
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
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Ok("present".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
        }
        assert_eq!(
            cache.lookup(&module, &BothPresent, &crate::eval::Settings::default()),
            None
        );
        Ok(())
    }

    /// Recording the same evaluation twice is idempotent: one row, and the
    /// second record does not invent a second answer for the same key.
    #[test]
    fn recording_twice_keeps_one_row() -> Result<(), Box<dyn core::error::Error>> {
        let inner = fake();
        let cas = MemoryCas::new();
        let mut cache = ResultCache::new(&cas);
        let module = id(b"module");

        for _ in 0..2 {
            let recorder = RecordingHost::new(&inner);
            drop(recorder.read_file(&crate::value2::ambient_path("/a")));
            cache.record(
                &module,
                &recorder.take(),
                &result("v1"),
                &inner,
                &crate::eval::Settings::default(),
            )?;
        }
        assert_eq!(
            cache.lookup(&module, &inner, &crate::eval::Settings::default()),
            Some(result("v1"))
        );
        Ok(())
    }
}

/// The asynchronous route to a host records what the blocking route records.
#[cfg(test)]
mod begun_questions {
    use super::{ReadSet, RecordingHost};
    use crate::host::{FileType, Host, SlowAnswer, StoreError, Ticket};
    use crate::task::{FetchKind, FetchRequest};

    /// A host that answers a fetch, and can do it either way.
    ///
    /// The asynchronous half is not threaded: what is under test is what the
    /// recorder writes down, and a thread would only add a way for the test
    /// to be flaky. `begin` computes the answer at once and `collect` hands
    /// it over, which is a legal host -- `begin` promises not to block, not
    /// to be slow.
    #[derive(Default)]
    struct Both {
        answered: std::cell::RefCell<std::collections::HashMap<u64, String>>,
        asynchronous: bool,
    }

    impl Host for Both {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            lock_flake,
            fetch_tree,
        );
        crate::host::host_stubs!(
            get_env,
            copy_to_store,
            ensure_path,
            find_file,
            nix_path,
            trace,
            warn,
            file_type_resolved
        );
        fn read_file(&self, p: &crate::value2::PathValue) -> Result<String, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn read_dir(
            &self,
            p: &crate::value2::PathValue,
        ) -> Result<Vec<(String, FileType)>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn path_exists_checked(
            &self,
            _p: &crate::value2::PathValue,
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
        fn file_type(&self, p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn fetch(&self, request: &FetchRequest) -> Result<String, StoreError> {
            Ok(format!(
                "/nix/store/0000000000000000000000000000000a-{}",
                request.name
            ))
        }
        fn begin(&self, question: &crate::host::Slow<'_>) -> Option<Ticket> {
            if !self.asynchronous {
                return None;
            }
            let crate::host::Slow::Fetch(request) = question else {
                return None;
            };
            let answer = self.fetch(request).ok()?;
            let ticket = self.answered.borrow().len() as u64 + 1;
            self.answered.borrow_mut().insert(ticket, answer);
            Some(Ticket(ticket))
        }
        fn collect(&self, ticket: Ticket, _block: bool) -> Option<SlowAnswer> {
            let answer = self.answered.borrow_mut().remove(&ticket.0)?;
            Some(SlowAnswer::Store(Ok(answer)))
        }
    }

    fn request() -> FetchRequest {
        FetchRequest {
            url: "http://example.invalid/x".to_owned(),
            name: "x".to_owned(),
            kind: FetchKind::File,
            expected_sha256: None,
        }
    }

    /// The recorder logs the same question and the same answer digest either
    /// way round.
    ///
    /// This is what makes the asynchronous path invisible to the memo. A
    /// read set recorded through `begin`/`collect` has to key identically to
    /// one recorded through `fetch`, or a witness written by an embedder with
    /// an asynchronous host would never match one written by an embedder
    /// without -- two caches for one evaluation, and neither would say so.
    #[test]
    fn a_begun_question_records_as_the_blocking_one() -> Result<(), String> {
        let blocking = Both::default();
        let recorder = RecordingHost::new(&blocking);
        let _ = recorder.fetch(&request());
        let by_blocking: ReadSet = recorder.take();

        let asynchronous = Both {
            asynchronous: true,
            ..Both::default()
        };
        let recorder = RecordingHost::new(&asynchronous);
        let question = crate::host::Slow::Fetch(&request());
        let ticket = recorder
            .begin(&question)
            .ok_or_else(|| "the host declined to begin a fetch".to_owned())?;
        let _ = recorder
            .collect(ticket, true)
            .ok_or_else(|| "the host did not answer a ticket it minted".to_owned())?;
        let by_beginning: ReadSet = recorder.take();

        if by_blocking != by_beginning {
            return Err(format!(
                "the two routes recorded differently: blocking {by_blocking:?}, \
                 begun {by_beginning:?}"
            ));
        }
        Ok(())
    }

    /// Nothing is recorded until the answer exists.
    ///
    /// A question noted at `begin` would have to invent an answer digest, and
    /// a read set whose answer does not match what the evaluation was told is
    /// the one thing this file exists to prevent.
    #[test]
    fn beginning_a_question_records_nothing_on_its_own() -> Result<(), String> {
        let host = Both {
            asynchronous: true,
            ..Both::default()
        };
        let recorder = RecordingHost::new(&host);
        let question = crate::host::Slow::Fetch(&request());
        let _ = recorder
            .begin(&question)
            .ok_or_else(|| "the host declined to begin a fetch".to_owned())?;
        let so_far = recorder.take();
        if !so_far.questions().is_empty() {
            return Err(format!("a begun question was already recorded: {so_far:?}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod history_tests;

#[cfg(test)]
mod imported_derivation_witness_tests {
    use super::digest_import;
    use crate::host::{ImportedDerivation, ImportedSource};
    use crate::value2::PathValue;
    use std::collections::BTreeMap;

    #[test]
    fn import_witness_distinguishes_kind_name_and_every_output() {
        let drv = ImportedDerivation {
            path: "/nix/store/11111111111111111111111111111111-demo.drv".to_owned(),
            name: "demo".to_owned(),
            outputs: BTreeMap::from([("out".to_owned(), "/placeholder".to_owned())]),
        };
        let before = digest_import(&Ok(ImportedSource::Derivation(drv.clone())));
        let mut changed = drv.clone();
        changed.name = "other".to_owned();
        assert_ne!(before, digest_import(&Ok(ImportedSource::Derivation(changed))));
        let mut changed = drv.clone();
        changed.outputs.insert("out".to_owned(), "/different".to_owned());
        assert_ne!(before, digest_import(&Ok(ImportedSource::Derivation(changed))));
        let mut changed = drv.clone();
        changed.outputs.insert("dev".to_owned(), "/placeholder".to_owned());
        assert_ne!(before, digest_import(&Ok(ImportedSource::Derivation(changed))));
        assert_ne!(before, digest_import(&Ok(ImportedSource::Nix {
            path: PathValue::ambient(drv.path), text: "demo".to_owned(),
        })));
    }
}
