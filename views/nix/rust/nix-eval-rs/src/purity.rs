//! What `pure-eval` and `restrict-eval` say about each host question.
//!
//! The two are separate settings in cppnix and they forbid different things,
//! so they are two fields here and one table decides each question. Before
//! ENG-12541 part 2 this crate carried a single `filesystem_access` flag that
//! the embedder set from `restrictEval || pureEval`, and every host question
//! was refused whenever either was on. That was wrong in the expensive
//! direction: `nix eval nixpkgs#lib.version` runs under pure eval, so the
//! wholesale refusal made no real flake evaluable on this backend at all
//! (`maintainers/ix/rust-flake-entry.md`).
//!
//! # Where the line falls, and why it is not the same line for both settings
//!
//! `restrict-eval` and `pure-eval` are both enforced in cppnix by wrapping
//! `EvalState::rootFS` in an `AllowListSourceAccessor` (`eval.cc:306`), plus
//! `checkURI` for `restrict-eval` (`eval.cc:485`) and a handful of per-primop
//! checks for `pure-eval`. So whether this evaluator can honour a setting for
//! a given question is decided by one thing: **does the answer come back
//! through cppnix's own accessor, or does this crate read the world itself?**
//!
//! * `StorePath` and `StoreFiltered` are answered by the bridge through
//!   `host.state.rootPath(...)` (`rust-eval-session.cc:67` and `:233`),
//!   which is `rootFS`. cppnix's access control applies unchanged and its own
//!   `RestrictedPathError` text comes back, so these are served.
//! * `Fetch` and `FetchTree` are answered by the bridge, which calls
//!   `state.checkURI` (`:322`, `:506`) and, for a tree, cppnix's own
//!   pure-eval locked-input check (`:494`). Served.
//! * `FindFile` and `NixPath` are answered from `host.state.findFile` and
//!   `getLookupPath()`, both of which cppnix already built with these
//!   settings applied. Served, including cppnix's "cannot look up '<%s>' in
//!   pure evaluation mode" (`eval.cc:3465`).
//! * `Import`, `Contents`, `Exists`, `Entries` and `Kind` are answered by the
//!   bridge too, through `host.state.rootPath(...)` again
//!   (`rust-eval-session.cc:369`, `:426`, `:483`, `:540`). Each of the four
//!   hooks behind them is a transcription of the cppnix primop that asks the
//!   same question -- `prim_readFile` (`primops.cc:2201`), `prim_pathExists`
//!   (`primops.cc:2081`), `prim_readDir` (`primops.cc:2508`) and
//!   `prim_readFileType` (`primops.cc:2490`) -- so the allow list decides and
//!   cppnix's own `RestrictedPathError` text comes back. `Import` is not a
//!   read of its own: `Host::resolve_import` asks `Kind` and then `Contents`,
//!   so it is served exactly when those two are. ENG-12792.
//!
//! # Without an embedder there are no hooks, and then these five refuse
//!
//! The bullets above describe the configuration the `nix` binary runs:
//! `RustEvalSetup` installs every hook. This crate also has a standalone
//! configuration with no embedder behind it -- the probe in
//! `examples/nixpkgs-probe.rs`, the differential harness the cache-semantics
//! gate builds, and every unit test -- and there `RealFs` reads the world
//! with `std::fs`, which consults no allow list.
//!
//! So the six rows are not a constant. [`PathReads`] is which of the two
//! configurations is in force, read from whether the hooks are installed, and
//! it is an argument to [`verdict`] beside the settings. Bridged, the five are
//! served. Standalone, they still refuse by name, because answering from
//! `std::fs` would give a weaker guarantee than the setting promises and give
//! it silently. With an embedder attached there is no `Refuse` row left.
//!
//! [`crate::readset::ReadSet::replay`] reads the same table, so this also
//! decides which witnesses may be replayed: one recorded with an embedder
//! cannot be replayed standalone under a purity setting, because replaying it
//! would ask questions the standalone configuration must refuse.
//!
//! # This is a policy table, not an approximation of cppnix
//!
//! Every row cites the cppnix line it was read off. A row that guessed would
//! be the worst kind of wrong here: a question served where cppnix refuses is
//! a purity setting that does not hold, and a question refused where cppnix
//! answers is a backend that cannot run the fleet's flakes.

use crate::task::NeedPath;

/// The two purity settings, as the embedder set them.
///
/// A struct of two `bool`s rather than a four-valued enum because the two are
/// independent settings a user can set independently, and cppnix reads them
/// independently -- `nix --extra-experimental-features ... --restrict-eval`
/// without `--pure-eval` is an ordinary configuration, and so is the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Purity {
    /// `pure-eval`: the result must be determined by declared inputs.
    /// cppnix's `EvalSettings::pureEval` (`eval-settings.hh:181`).
    pub pure_eval: bool,
    /// `restrict-eval`: no file outside `builtins.nixPath` and no URI outside
    /// `allowed-uris`. cppnix's `EvalSettings::restrictEval`
    /// (`eval-settings.hh:169`).
    pub restrict_eval: bool,
}

impl Purity {
    /// What the process is configured to do right now.
    #[must_use]
    pub fn current() -> Self {
        Self {
            pure_eval: crate::eval::pure_eval(),
            restrict_eval: crate::eval::restrict_eval(),
        }
    }

    /// Whether either setting is on.
    #[must_use]
    pub fn any(self) -> bool {
        self.pure_eval || self.restrict_eval
    }

    /// The settings that are on, spelled as `nix.conf` spells them, for a
    /// refusal detail. Both are named when both are on, because turning
    /// either one off on its own would not make the question servable.
    #[must_use]
    pub fn names(self) -> &'static str {
        match (self.pure_eval, self.restrict_eval) {
            (true, true) => "pure-eval and restrict-eval",
            (true, false) => "pure-eval",
            (false, true) => "restrict-eval",
            (false, false) => "no purity setting",
        }
    }
}

/// Who answers a plain filesystem read: `Contents`, `Exists`, `Entries`,
/// `Kind`, and the two of those an `Import` is made of.
///
/// Not a setting the user picks -- it is a property of the embedding, and the
/// only property that decides whether `pure-eval` and `restrict-eval` can be
/// honoured for those five questions at all. Kept apart from [`Purity`] for
/// exactly that reason: one is what the operator asked for and the other is
/// what this process is able to deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathReads {
    /// This crate's own `Host`, with `std::fs` and no allow list. The
    /// standalone configuration: the probe, the differential harness, the
    /// unit tests. The default, because a crate nobody has handed hooks to
    /// has no accessor to reach.
    #[default]
    Direct,
    /// The embedder's read hooks, which for the `nix` binary are
    /// `state->rootPath(...)`, i.e. `rootFS` with whatever access control
    /// these settings put on it.
    ///
    /// Which of the two applies is a property of the host the session was
    /// given, not of the process: [`crate::host::FnHost::path_reads`] and
    /// `capi::EmbedderHost::path_reads` each answer for their own host, and
    /// the session folds the answer into [`crate::eval::Settings`] when it is
    /// created.
    ThroughEmbedder,
}

/// What the purity settings say about one question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Ask the host. Either the setting does not touch this question, or the
    /// embedder answers it through cppnix's own access control and will raise
    /// cppnix's own error if the setting forbids it.
    Ask,
    /// Answer with the empty string and do not ask. cppnix's `prim_getEnv`
    /// reads `restrictEval || pureEval` and produces `""` without looking at
    /// the environment (`primops.cc:1261`), so asking would be both an IO the
    /// setting forbids and a read set entry the evaluation did not make.
    EmptyString,
    /// Fail with this message, which is cppnix's own for the same input.
    ///
    /// Not a refusal, and the difference is what a census counts: an
    /// unpinned fetch under `pure-eval` is a program cppnix rejects too, so
    /// filing it as a backend gap would be counting cppnix's behaviour as
    /// this crate's shortfall.
    Error(String),
    /// Refuse by name. In this configuration the crate's `Host` would answer
    /// the question by reading the world with `std::fs`, outside cppnix's
    /// `rootFS`, so it cannot honour the setting and must not pretend to.
    ///
    /// Reachable only under [`PathReads::Direct`]: with the embedder's read
    /// hooks installed every question is served or errors the way cppnix does.
    Refuse,
}

/// The policy table. Every arm cites the cppnix behaviour it mirrors.
#[must_use]
pub fn verdict(need: &NeedPath, purity: Purity, reads: PathReads) -> Verdict {
    if !purity.any() {
        // Neither setting on: cppnix's `rootFS` is the plain union accessor
        // with no allow list (`eval.cc:302`), `checkURI` returns immediately
        // (`eval.cc:487`), and every pure-eval primop check is skipped. There
        // is nothing to honour, so every question is the host's.
        return Verdict::Ask;
    }
    match need {
        // The four plain reads, and the `Import` built out of two of them.
        //
        // Served when the embedder answers, because then the read goes
        // through `host.state.rootPath(...)` -- `rootFS`, carrying this
        // process's `AllowListSourceAccessor` (`eval.cc:306`) -- and each
        // hook is a transcription of the cppnix primop that asks the same
        // thing: `prim_readFile` (`primops.cc:2201`), `prim_pathExists`
        // (`primops.cc:2081`), `prim_readDir` (`primops.cc:2508`),
        // `prim_readFileType` (`primops.cc:2490`). cppnix's allow list
        // decides and its own `RestrictedPathError` text is what fails.
        // ENG-12792.
        //
        // `Exists` rides the same hook and is still not a failure on either
        // side: `prim_pathExists` catches `RestrictedPathError` and returns
        // `false` (`primops.cc:2097`), and so does `rustPathExists`. That is
        // cppnix's answer rather than a blanket one, which is why serving it
        // needs the accessor -- a standalone `false` would be a wrong value
        // for an allowed path, the one outcome worse than refusing.
        //
        // Refused when this crate reads with `std::fs`, which sees no allow
        // list and so cannot tell an allowed path from a forbidden one.
        NeedPath::Import(_)
        | NeedPath::Contents(_)
        // The same read as `Contents` with a different answer type: the
        // embedder's accessor decides it, so it shares the row.
        | NeedPath::HashFile { .. }
        | NeedPath::Exists(_)
        // The trailing-slash spelling of the same read: it is served by the
        // dedicated full-resolution directory-existence hook, which also
        // goes through `rootFS`, and
        // `prim_pathExists`'s catch makes a forbidden path `false` for both
        // spellings alike.
        | NeedPath::DirExists(_)
        | NeedPath::Entries(_)
        | NeedPath::Kind(_)
        | NeedPath::MaybeKind(_) => match reads {
            PathReads::ThroughEmbedder => Verdict::Ask,
            PathReads::Direct => Verdict::Refuse,
        },

        // `primops.cc:1261`: the empty string under either setting, without
        // reading the environment.
        NeedPath::Env(_) => Verdict::EmptyString,

        // Answered by the bridge through `host.state.rootPath(...)`, i.e.
        // `rootFS`, so cppnix's allow list decides and its
        // `RestrictedPathError` comes back as the failure text
        // (`rust-eval-session.cc:67`, `:233`).
        NeedPath::StorePath(_) | NeedPath::StoreFiltered(_) => Verdict::Ask,

        // `prim_storePath` rejects only pure evaluation. Restrict evaluation
        // still asks through rootFS, whose allow list owns the path decision.
        NeedPath::UseStorePath(_) => {
            if purity.pure_eval {
                Verdict::Error(
                    "'builtins.storePath' is not allowed in pure evaluation mode".to_owned(),
                )
            } else {
                Verdict::Ask
            }
        }

        // `builtins.toFile` has no purity check in cppnix at all
        // (`prim_toFile`, `primops.cc:2789`): the path is a function of the
        // bytes and the references, and neither setting forbids naming one.
        // Measured under both settings on nix 2.34.7+ix.h24085346:
        // `builtins.toFile "x" "hi"` answers with a store path.
        NeedPath::StoreText { .. } => Verdict::Ask,

        // Writing a `.drv`. cppnix does it inside `derivationStrictInternal`
        // (`primops.cc:1937`) with no purity check of any kind, and it has to:
        // every flake evaluation runs under `pure-eval` and every one of them
        // produces derivations. Refusing here would make `nix build` of a
        // flake impossible under the setting flakes always set.
        //
        // Nothing it writes can escape the allow list either, but that is a
        // property of the rows above rather than of this one, and the
        // difference matters to whoever changes them next.
        //
        // The aterm is the evaluator's own rendering of a derivation it
        // already built, so whatever is in it arrived through some earlier
        // question. Under `ThroughEmbedder` those questions went through
        // `rootFS` and the allow list already decided. Under `Direct` the
        // six plain reads are `Refuse`, so an evaluation cannot have read a
        // forbidden file to put in the aterm in the first place. Either way
        // the content is policy-clean before this row is consulted, and this
        // row adds no channel of its own: no URI for `restrict-eval` to
        // check, references the store validates, and a path that is a hash of
        // bytes the evaluator already holds.
        //
        // So if the read rows ever start answering under `Direct` -- somebody
        // teaches the standalone `Host` an allow list, say -- the argument
        // for this row stops holding and it has to be re-made.
        // `write_drv_leans_on_the_plain_reads_refusing_under_direct` is what
        // will tell them. Found in review by shadow-traces, who pointed out
        // that the first version of this comment claimed an intrinsic
        // property. ENG-12799.
        NeedPath::WriteDrv { .. } => Verdict::Ask,

        // `state.store->ensurePath` on a path that is already in a string's
        // context. No accessor is involved, and cppnix's `appendContext`
        // (`context.cc:270`) applies no purity check.
        NeedPath::EnsurePath(_) => Verdict::Ask,

        // Import from derivation. `realiseContext` (`primops.cc:72`) reads
        // neither setting: it validates store paths, builds derivations and
        // then calls `allowClosure` on each output -- which *adds* to the
        // allow list rather than consulting it, so that the read which
        // follows is permitted under `restrict-eval`.
        //
        // Serving it under `pure-eval` is not a hole, it is the point.
        // Every flake evaluation runs under that setting and IFD is a normal
        // thing for one to do, so refusing here would make this backend
        // unable to evaluate the flakes cppnix evaluates -- the ENG-12541
        // shape, where a wholesale refusal made no real flake evaluable.
        // Measured on nix 2.34.7+ix.h24085346: `nix eval --pure-eval` of an
        // expression importing a derivation output builds it and answers.
        //
        // **The setting that does forbid this is not in [`Purity`] and must
        // not be added to it.** `allow-import-from-derivation` is an
        // `EvalSettings` field the embedder holds, cppnix checks it inside
        // `realiseContext` before any build, and the failure is an `IFDError`
        // with cppnix's own wording. So it is enforced on the far side of the
        // question, in the same transcription that performs the build
        // (`rust-eval-session.cc`, `rustRealise`). A copy of the check here
        // would be a second reader of a setting this crate is not given, and
        // it would disagree with the first one the day a default moves.
        //
        // Both columns are `Ask` for the reason `WriteDrv`'s are: the store
        // is always the embedder's -- there is no `std::fs` branch to fall
        // back to -- so `PathReads` cannot distinguish this row. Standalone,
        // the refusal comes from having no store at all, which is an answer
        // and not a policy.
        NeedPath::Realise(_) => Verdict::Ask,

        // `restrict-eval` is the bridge's `state.checkURI(url)`
        // (`rust-eval-session.cc:322`), which is cppnix's own
        // `allowed-uris` test.
        //
        // `pure-eval` is this side's, because cppnix raises it in the primop
        // before any IO: `fetchTree.cc:537`, after the name validation this
        // evaluator has already done and before the `ensurePath` early exit
        // the bridge performs. A *pinned* fetch is served under pure eval,
        // which is the point of the setting rather than an exception to it:
        // the store path is known from the hash and `ensurePath` can answer
        // without touching the network.
        NeedPath::Fetch(request) => {
            if purity.pure_eval && request.expected_sha256.is_none() {
                Verdict::Error(format!(
                    "in pure evaluation mode, '{}' requires a 'sha256' argument",
                    request.kind.who()
                ))
            } else {
                Verdict::Ask
            }
        }

        // Both settings are the bridge's here and it applies cppnix's own
        // checks: `input.isLocked` with cppnix's "doesn't fetch unlocked
        // input" wording (`rust-eval-session.cc:494`, from
        // `fetchTree.cc:286`), then `state.checkURI` (`:506`).
        NeedPath::FetchTree(_) => Verdict::Ask,

        // Served under both settings, for the reason the table's header gives:
        // the embedder answers it, and the embedder is cppnix. Deciding here
        // would be this crate second-guessing a lock it does not compute.
        //
        // `pure-eval` is two rules and the bridge carries both, transcribed
        // from `prim_getFlake`. The unlocked-reference refusal
        // (`flake-primops.cc:42`, "cannot call 'getFlake' on unlocked flake
        // reference '%s'") is `rust-eval-session.cc:611`, the same
        // `input.isLocked` test raising the same message. The two `LockFlags`
        // the setting turns off, `useRegistries = !pureEval` and
        // `allowUnlocked = !pureEval` (`flake-primops.cc:70`, `:71`), are
        // `rust-eval-session.cc:641` and `:642`.
        //
        // `restrict-eval` has no rule of its own here in either backend:
        // `lockFlake` is not one of cppnix's `checkURI` call sites, and its
        // reads run through this process's own `EvalState`, which the
        // embedder built with the setting already applied.
        NeedPath::Flake(_) => Verdict::Ask,

        // Served under both settings: neither primop has a purity check in
        // cppnix (`flake-primops.cc`, `prim_parseFlakeRef` and
        // `prim_flakeRefToString` -- string work over a fixed grammar, no
        // registry, no fetch). Measured: `nix eval` in its default pure mode
        // answers both. The flakes feature gate is not a purity rule and
        // lives behind the hook, where cppnix checks it.
        NeedPath::ParseFlakeRef(_) | NeedPath::FlakeRefToString(_) => Verdict::Ask,

        // Outputs, not questions. cppnix warns and traces under either
        // setting exactly as it does without them, and refusing would let a
        // purity setting change what a program prints.
        NeedPath::Warn(_) | NeedPath::Trace(_) => Verdict::Ask,

        // `host.state.findFile(...)` (`rust-eval-session.cc:651`) is
        // cppnix's own, so the pure-eval message at `eval.cc:3465` and the
        // restrict-eval allow list both arrive from there. Measured:
        // `(builtins.tryEval <nope>).success` is `false` under both settings,
        // and this backend keeps that because cppnix raises the miss as a
        // `ThrownError` the bridge forwards as catchable.
        NeedPath::FindFile { .. } => Verdict::Ask,

        // `host.state.getLookupPath()` (`rust-eval-session.cc:713`) is the
        // list cppnix built under these settings: `eval.cc:357` drops
        // everything under `pure-eval` and `eval.cc:365` drops the default
        // entries under `restrict-eval`. Measured on nix 2.34.7+ix.h24085346:
        // `builtins.nixPath` is `[ ]` under `--pure-eval` and the `-I` flags
        // under `--restrict-eval`.
        NeedPath::NixPath => Verdict::Ask,
    }
}

/// The question kinds, spelled once.
///
/// One list generates two things that must agree: [`question_kind`]'s
/// exhaustive `match` over `NeedPath`, and the [`QUESTION_KINDS`] array that
/// `perf` and the policy test read. Before ENG-13065 they were two lists
/// maintained by hand and they did not agree: `NeedPath::Flake` had an arm in
/// `question_kind` and no entry in `QUESTION_KINDS`, which cost two things at
/// once. `the_policy_table_covers_every_question` compared the two mirrors to
/// each other rather than either to the enum, so it passed with the flake row
/// of [`verdict`] untested; and `eval::drive` finds a question's counter
/// bucket by looking its name up in `QUESTION_KINDS`, so every `getFlake`
/// question was counted in the total and in no per-kind bucket.
///
/// A macro rather than a hand-written pair because a pair is the bug. Adding
/// a `NeedPath` variant now stops the generated `match` compiling, and the
/// one line that fixes that is also the line that puts the kind in
/// `QUESTION_KINDS`, widens `perf::KINDS`, and makes
/// `the_policy_table_covers_every_question` demand a row for it by name.
macro_rules! question_kinds {
    ($($pattern:pat => $name:literal,)+) => {
        /// Every question kind, in the order [`question_kind`]'s arms are
        /// written, which is `NeedPath`'s declaration order except that
        /// `Flake` is grouped with the other fetches rather than sitting
        /// second.
        ///
        /// Derived from the same list as [`question_kind`], so the two
        /// cannot disagree: a name here that no arm produces, or an arm
        /// whose name is not here, is not expressible.
        ///
        /// `perf` indexes its per-kind counter array by position in this
        /// list. The numbering is therefore load-bearing within a process
        /// and nowhere else -- nothing persists it -- so inserting a kind in
        /// the middle is fine.
        pub const QUESTION_KINDS: &[&str] = &[$($name),+];

        /// The question's variant name.
        ///
        /// Exhaustive on purpose. A new `NeedPath` variant stops this
        /// function and [`verdict`] compiling, and the `question_kinds!`
        /// entry that fixes this one is what carries the kind into
        /// [`QUESTION_KINDS`] and so into the policy test's coverage check.
        #[must_use]
        pub fn question_kind(need: &NeedPath) -> &'static str {
            match need {
                $($pattern => $name,)+
            }
        }

        /// The question's position in [`QUESTION_KINDS`], from the same list,
        /// so a counter indexed by it cannot land in a bucket the list does
        /// not have: this is what a name lookup with a fallback could not say.
        #[must_use]
        pub fn question_kind_index(need: &NeedPath) -> usize {
            let mut index = 0;
            $(
                if matches!(need, $pattern) {
                    return index;
                }
                index += 1;
            )+
            unreachable!("question_kind is exhaustive over NeedPath")
        }
    };
}

question_kinds! {
    NeedPath::Import(_) => "Import",
    NeedPath::Contents(_) => "Contents",
    NeedPath::HashFile { .. } => "HashFile",
    NeedPath::Exists(_) => "Exists",
    NeedPath::DirExists(_) => "DirExists",
    NeedPath::Entries(_) => "Entries",
    NeedPath::Kind(_) => "Kind",
    NeedPath::MaybeKind(_) => "MaybeKind",
    NeedPath::Env(_) => "Env",
    NeedPath::StorePath(_) => "StorePath",
    NeedPath::UseStorePath(_) => "UseStorePath",
    NeedPath::StoreText { .. } => "StoreText",
    NeedPath::WriteDrv { .. } => "WriteDrv",
    NeedPath::StoreFiltered(_) => "StoreFiltered",
    NeedPath::Fetch(_) => "Fetch",
    NeedPath::FetchTree(_) => "FetchTree",
    NeedPath::Flake(_) => "Flake",
    NeedPath::ParseFlakeRef(_) => "ParseFlakeRef",
    NeedPath::FlakeRefToString(_) => "FlakeRefToString",
    NeedPath::EnsurePath(_) => "EnsurePath",
    NeedPath::Realise(_) => "Realise",
    NeedPath::Warn(_) => "Warn",
    NeedPath::Trace(_) => "Trace",
    NeedPath::FindFile { .. } => "FindFile",
    NeedPath::NixPath => "NixPath",
}

#[cfg(test)]
mod tests {
    use super::{PathReads, Purity, QUESTION_KINDS, Verdict, question_kind, verdict};
    use crate::task::{
        FetchKind, FetchRequest, FetchTreeRequest, FilteredCopy, NeedPath, PathMethod, TreeFetcher,
    };
    use std::collections::HashSet;

    /// The four configurations, in the order the expected-verdict tuples
    /// below spell them.
    const CONFIGS: [(&str, Purity); 4] = [
        (
            "neither",
            Purity {
                pure_eval: false,
                restrict_eval: false,
            },
        ),
        (
            "pure",
            Purity {
                pure_eval: true,
                restrict_eval: false,
            },
        ),
        (
            "restrict",
            Purity {
                pure_eval: false,
                restrict_eval: true,
            },
        ),
        (
            "both",
            Purity {
                pure_eval: true,
                restrict_eval: true,
            },
        ),
    ];

    /// One derivation-output context element, which is the only shape that
    /// makes a `Realise` question a *build* rather than a validity check.
    fn built() -> crate::value2::ContextElem {
        crate::value2::ContextElem::Built {
            drv: "/nix/store/aaa-x.drv".into(),
            output: "out".into(),
        }
    }

    fn fetch(pinned: bool) -> NeedPath {
        NeedPath::Fetch(Box::new(FetchRequest {
            url: "https://example.invalid/x.tar.gz".to_owned(),
            name: "x.tar.gz".to_owned(),
            kind: FetchKind::File,
            expected_sha256: pinned.then(|| "sha256-".to_owned() + &"A".repeat(43) + "="),
        }))
    }

    /// Every question kind has a row, and only `Fetch` has two.
    ///
    /// The list this checks against is [`QUESTION_KINDS`], which the
    /// `question_kinds!` macro derives from [`question_kind`]'s own match, so
    /// this compares the table to the `NeedPath` enum rather than to a second
    /// hand-written list. Before ENG-13065 it compared two hand-written lists
    /// to each other and both were missing `Flake`, so it passed while the
    /// flake row of [`verdict`] went untested for as long as it existed.
    ///
    /// What a new `NeedPath` variant now runs into, in order: [`verdict`] and
    /// the generated `question_kind` stop compiling; the `question_kinds!`
    /// entry that fixes them puts the kind in `QUESTION_KINDS`; and this test
    /// then fails naming that kind until [`policy_table`] has a row for it.
    #[test]
    fn the_policy_table_covers_every_question() {
        let rows: Vec<&str> = policy_table()
            .iter()
            .map(|(need, _, _)| question_kind(need))
            .collect();
        let covered: HashSet<&str> = rows.iter().copied().collect();
        let missing: Vec<&str> = QUESTION_KINDS
            .iter()
            .copied()
            .filter(|kind| !covered.contains(kind))
            .collect();
        assert!(
            missing.is_empty(),
            "no row in the purity policy table for {missing:?}, so what `verdict` \
             answers for those questions is untested. Add a row to `policy_table` \
             citing the cppnix behaviour it mirrors."
        );
        // The set check above passes if a row is duplicated and another
        // dropped, because a duplicate still covers its own kind. This is
        // what says the table is one row per kind: `Fetch` is the only kind
        // written twice, for the pinned and unpinned cases.
        assert_eq!(
            rows.len(),
            QUESTION_KINDS.len() + 1,
            "the table has to hold one row per question, plus the second \
             `Fetch` row for the unpinned case"
        );
    }

    /// The table itself, one row per question, one column per configuration.
    ///
    /// Every expectation is derived from a cppnix source cited on the
    /// matching arm of [`verdict`], or from a measurement recorded there.
    ///
    /// A function rather than a local of the test below because
    /// `the_policy_table_covers_every_question` reads it too: a separate list
    /// of samples for the coverage check would be one more mirror of the
    /// `NeedPath` enum, which is the shape ENG-13065 was.
    fn policy_table() -> Vec<(NeedPath, [Verdict; 4], [Verdict; 4])> {
        let ask = [Verdict::Ask, Verdict::Ask, Verdict::Ask, Verdict::Ask];
        let refuse_when_set = [
            Verdict::Ask,
            Verdict::Refuse,
            Verdict::Refuse,
            Verdict::Refuse,
        ];
        let empty_when_set = [
            Verdict::Ask,
            Verdict::EmptyString,
            Verdict::EmptyString,
            Verdict::EmptyString,
        ];
        let unpinned_fetch = [
            Verdict::Ask,
            Verdict::Error(
                "in pure evaluation mode, 'fetchurl' requires a 'sha256' argument".to_owned(),
            ),
            Verdict::Ask,
            Verdict::Error(
                "in pure evaluation mode, 'fetchurl' requires a 'sha256' argument".to_owned(),
            ),
        ];
        // Expected, in CONFIGS order: neither, pure, restrict, both.
        //
        // Two expected tuples per row, one per `PathReads`: `Direct` is the
        // standalone embedding reading with `std::fs`, `ThroughEmbedder` is
        // the `nix` binary reading through `rootFS`. They differ on exactly
        // the six rows ENG-12792 moved, and writing both out is what says
        // so -- a single column would leave "this row does not depend on who
        // answers" as an unstated claim about thirteen of the eighteen.
        vec![
            // Refused when this crate reads with std::fs and served when the
            // embedder's rootFS answers. primops.cc:2201, :2081, :2508, :2490.
            (
                NeedPath::Import(crate::value2::ambient_path("/tmp/a.nix")),
                refuse_when_set.clone(),
                ask.clone(),
            ),
            (
                NeedPath::Contents(crate::value2::ambient_path("/tmp/a.txt")),
                refuse_when_set.clone(),
                ask.clone(),
            ),
            // The same read as `Contents` through the same accessor, so the
            // same row: cppnix's `prim_hashFile` reaches the file through
            // `realisePath` exactly as `prim_readFile` does (primops.cc:2440)
            // and only the answer type differs.
            (
                NeedPath::HashFile {
                    path: crate::value2::ambient_path("/tmp/a.txt"),
                    algo: crate::nixhash::HashAlgo::Sha256,
                },
                refuse_when_set.clone(),
                ask.clone(),
            ),
            (
                NeedPath::Exists(crate::value2::ambient_path("/tmp/a.txt")),
                refuse_when_set.clone(),
                ask.clone(),
            ),
            // The trailing-slash spelling rides the same hook and the same
            // catch (primops.cc:2116), so its row is `Exists`'s row.
            (
                NeedPath::DirExists(crate::value2::ambient_path("/tmp/a.txt")),
                refuse_when_set.clone(),
                ask.clone(),
            ),
            (
                NeedPath::Entries(crate::value2::ambient_path("/tmp")),
                refuse_when_set.clone(),
                ask.clone(),
            ),
            (
                NeedPath::Kind(crate::value2::ambient_path("/tmp/a.txt")),
                refuse_when_set.clone(),
                ask.clone(),
            ),
            // The same read as `Kind` through the same accessor, so the same
            // row: what the allow list permits cannot depend on whether the
            // asker wanted a missing path to be an error. cppnix has one
            // `maybeLstat` behind both.
            (
                NeedPath::MaybeKind(crate::value2::ambient_path("/tmp/a.txt")),
                refuse_when_set.clone(),
                ask.clone(),
            ),
            // primops.cc:1261. Nothing to do with who reads the filesystem:
            // cppnix answers "" without consulting the environment at all.
            (
                NeedPath::Env("HOME".to_owned()),
                empty_when_set.clone(),
                empty_when_set,
            ),
            // Through rootFS on the bridge side, and served in both
            // configurations because the *store copy* has always gone through
            // the embedder -- there is no std::fs branch for it to fall back
            // to. That is why these rows do not move with `PathReads`.
            (
                NeedPath::StorePath(crate::value2::ambient_path("/tmp/a.txt")),
                ask.clone(),
                ask.clone(),
            ),
            (
                NeedPath::UseStorePath(crate::value2::ambient_path(
                    "/nix/store/00000000000000000000000000000000-a",
                )),
                [
                    Verdict::Ask,
                    Verdict::Error(
                        "'builtins.storePath' is not allowed in pure evaluation mode".to_owned(),
                    ),
                    Verdict::Ask,
                    Verdict::Error(
                        "'builtins.storePath' is not allowed in pure evaluation mode".to_owned(),
                    ),
                ],
                [
                    Verdict::Ask,
                    Verdict::Error(
                        "'builtins.storePath' is not allowed in pure evaluation mode".to_owned(),
                    ),
                    Verdict::Ask,
                    Verdict::Error(
                        "'builtins.storePath' is not allowed in pure evaluation mode".to_owned(),
                    ),
                ],
            ),
            (
                NeedPath::StoreFiltered(Box::new(FilteredCopy {
                    root: crate::value2::ambient_path("/tmp/tree"),
                    name: "tree".to_owned(),
                    method: PathMethod::NixArchive,
                    accepted: None,
                    expected_sha256: None,
                    inherit_references: false,
                })),
                ask.clone(),
                ask.clone(),
            ),
            // No purity check in cppnix.
            (
                NeedPath::StoreText {
                    name: "x".to_owned(),
                    contents: "hi".to_owned(),
                    references: Vec::new(),
                },
                ask.clone(),
                ask.clone(),
            ),
            (
                NeedPath::EnsurePath("/nix/store/aaa-x".to_owned()),
                ask.clone(),
                ask.clone(),
            ),
            // primops.cc:72 reads neither setting, and `allowClosure` on the
            // outputs is what makes the read that follows legal under
            // restrict-eval. `allow-import-from-derivation` is a third
            // setting, enforced by the embedder inside the question.
            (NeedPath::Realise(vec![built()]), ask.clone(), ask.clone()),
            // primops.cc:1937 writes the `.drv` with no purity check, and
            // must: every flake evaluation is a pure-eval one and every one
            // of them produces derivations. Both columns are `ask` because
            // the write reads nothing -- the bytes are the evaluator's own
            // rendering of a derivation it already built -- so there is no
            // accessor for `PathReads` to distinguish. ENG-12799.
            (
                NeedPath::WriteDrv {
                    name: "x".to_owned(),
                    aterm: "Derive([],[],[],\"x86_64-linux\",\"/bin/sh\",[],[])".to_owned(),
                    expected: "/nix/store/aaa-x.drv".to_owned(),
                },
                ask.clone(),
                ask.clone(),
            ),
            // fetchTree.cc:537: pinned is served under pure eval, unpinned is
            // cppnix's own error. restrict-eval says nothing about the pin --
            // it checks the URI, which the bridge does.
            (fetch(true), ask.clone(), ask.clone()),
            (fetch(false), unpinned_fetch.clone(), unpinned_fetch),
            // The bridge applies isLocked and checkURI.
            (
                NeedPath::FetchTree(Box::new(FetchTreeRequest {
                    attrs: std::collections::BTreeMap::new(),
                    fetcher: TreeFetcher::Tree,
                })),
                ask.clone(),
                ask.clone(),
            ),
            // `builtins.getFlake`. Both settings are cppnix's own and both are
            // applied on the far side, so this row is `ask` everywhere -- and
            // it is `ask` under `Direct` too, which is not a gap: with no
            // embedder there is nothing to lock a flake with, and the question
            // fails as unanswerable rather than being served.
            //
            // `pure-eval` is two rules, and the bridge carries both.
            // `flake-primops.cc:42` refuses an unlocked flake reference with
            // "cannot call 'getFlake' on unlocked flake reference '%s'", and
            // `rust-eval-session.cc:611` raises that same message with the
            // same `input.isLocked` test. `flake-primops.cc:70` and `:71` then
            // hand `lockFlake` `useRegistries = !pureEval` and
            // `allowUnlocked = !pureEval`, which
            // `rust-eval-session.cc:641` and `:642` spell identically, so a
            // registry lookup and an unpinned input are off under the setting
            // in both backends.
            //
            // `restrict-eval` has no rule of its own here in either backend:
            // `lockFlake` is not one of cppnix's `checkURI` call sites, and
            // whatever its fetches and its read of `flake.nix` do run into is
            // this process's own `EvalState`, which the embedder built with
            // the setting applied. Deciding anything here would be this crate
            // second-guessing a lock it does not compute.
            (
                NeedPath::Flake("github:NixOS/nixpkgs".to_owned()),
                ask.clone(),
                ask.clone(),
            ),
            // `builtins.parseFlakeRef` / `builtins.flakeRefToString`. Both
            // are pure functions of their argument -- flake-primops.cc calls
            // `parseFlakeRef` / `FlakeRef::fromAttrs` and touches neither the
            // store nor the filesystem -- so neither `pure-eval` nor
            // `restrict-eval` says anything about them (measured: a
            // pure-default `nix eval` answers both). They live behind Host
            // only because the flake-ref grammar does; the flakes
            // experimental-feature check runs on the far side, mirroring
            // cppnix's call-time check in flake-primops.cc.
            (
                NeedPath::ParseFlakeRef("github:NixOS/nixpkgs".to_owned()),
                ask.clone(),
                ask.clone(),
            ),
            (
                NeedPath::FlakeRefToString(std::collections::BTreeMap::new()),
                ask.clone(),
                ask.clone(),
            ),
            // Outputs.
            (
                NeedPath::Warn("careful".to_owned()),
                ask.clone(),
                ask.clone(),
            ),
            (
                NeedPath::Trace("tracing".to_owned()),
                ask.clone(),
                ask.clone(),
            ),
            // cppnix's own findFile and lookup path.
            (
                NeedPath::FindFile {
                    entries: Vec::new(),
                    name: "nixpkgs".to_owned(),
                },
                ask.clone(),
                ask.clone(),
            ),
            (NeedPath::NixPath, ask.clone(), ask),
        ]
    }

    /// Every row of [`policy_table`], in all four configurations and both
    /// `PathReads`.
    #[test]
    fn each_question_gets_the_verdict_cppnix_behaviour_implies() {
        for (need, direct, bridged) in policy_table() {
            for (reads, expected) in [
                (PathReads::Direct, &direct),
                (PathReads::ThroughEmbedder, &bridged),
            ] {
                for ((label, purity), want) in CONFIGS.iter().zip(expected.iter()) {
                    assert_eq!(
                        &verdict(&need, *purity, reads),
                        want,
                        "{} under {label} with reads {reads:?} is not what cppnix does",
                        question_kind(&need)
                    );
                }
            }
        }
    }

    /// The five ENG-12792 moved, spelled out on their own rather than only
    /// inside the table, because the table asserts a shape and this asserts
    /// the claim: with an embedder attached there is no `Refuse` left, and
    /// without one every one of the six still refuses.
    #[test]
    fn write_drv_leans_on_the_plain_reads_refusing_under_direct() {
        // `WriteDrv` is `Ask` in both columns, and the reason it is safe under
        // `Direct` is not that a write reads nothing. It is that nothing a
        // forbidden read could have produced can be in the aterm, because
        // under `Direct` the six plain reads refuse. That is a dependency on
        // another row, and a comment saying so is a comment somebody can edit
        // past. This fails instead.
        //
        // It deliberately overlaps
        // `the_six_plain_reads_are_served_only_when_the_embedder_answers`.
        // That test asserts the read policy for its own sake; this one
        // asserts that `WriteDrv`'s justification still rests on something
        // true, and the two would be changed for different reasons.
        let reads = [
            NeedPath::Import(crate::value2::ambient_path("/tmp/a.nix")),
            NeedPath::Contents(crate::value2::ambient_path("/tmp/a.txt")),
            NeedPath::Exists(crate::value2::ambient_path("/tmp/a.txt")),
            // A separate host operation because canonicalisation has already
            // removed the slash that selected it.
            NeedPath::DirExists(crate::value2::ambient_path("/tmp/a.txt")),
            NeedPath::Entries(crate::value2::ambient_path("/tmp")),
            NeedPath::Kind(crate::value2::ambient_path("/tmp/a.txt")),
        ];
        let write = NeedPath::WriteDrv {
            name: "x".to_owned(),
            aterm: "Derive([],[],[],\"x86_64-linux\",\"/bin/sh\",[],[])".to_owned(),
            expected: "/nix/store/aaa-x.drv".to_owned(),
        };
        for (label, purity) in &CONFIGS {
            if !purity.any() {
                continue;
            }
            for need in &reads {
                assert_eq!(
                    verdict(need, *purity, PathReads::Direct),
                    Verdict::Refuse,
                    "{} is answered under {label} with reads Direct, so an evaluation \
                     can now read a file the allow list would have withheld and put it \
                     in a derivation. `WriteDrv` is `Ask` on the strength of that not \
                     being possible, so re-make its argument before changing this",
                    question_kind(need)
                );
            }
            for reads_mode in [PathReads::Direct, PathReads::ThroughEmbedder] {
                assert_eq!(
                    verdict(&write, *purity, reads_mode),
                    Verdict::Ask,
                    "WriteDrv under {label} with reads {reads_mode:?}"
                );
            }
        }
    }

    #[test]
    fn the_six_plain_reads_are_served_only_when_the_embedder_answers() {
        let six = [
            NeedPath::Import(crate::value2::ambient_path("/tmp/a.nix")),
            NeedPath::Contents(crate::value2::ambient_path("/tmp/a.txt")),
            NeedPath::Exists(crate::value2::ambient_path("/tmp/a.txt")),
            // A separate host operation because canonicalisation has already
            // removed the slash that selected it.
            NeedPath::DirExists(crate::value2::ambient_path("/tmp/a.txt")),
            NeedPath::Entries(crate::value2::ambient_path("/tmp")),
            NeedPath::Kind(crate::value2::ambient_path("/tmp/a.txt")),
        ];
        for need in &six {
            for (label, purity) in &CONFIGS {
                assert_eq!(
                    verdict(need, *purity, PathReads::ThroughEmbedder),
                    Verdict::Ask,
                    "{} under {label} is refused with an embedder attached, which \
                     is the whole of ENG-12792",
                    question_kind(need)
                );
                let standalone = verdict(need, *purity, PathReads::Direct);
                if purity.any() {
                    assert_eq!(
                        standalone,
                        Verdict::Refuse,
                        "{} under {label} is answered from std::fs, which honours \
                         no allow list",
                        question_kind(need)
                    );
                } else {
                    assert_eq!(standalone, Verdict::Ask);
                }
            }
        }
    }

    /// With an embedder attached the table has no `Refuse` row at all, over
    /// every question and every purity configuration. The module header says
    /// this; nothing else checks it, and a sixth question added later with a
    /// `Refuse` arm would make the header quietly wrong.
    ///
    /// Over [`policy_table`] rather than a sample list of its own, so
    /// `the_policy_table_covers_every_question` is what keeps this complete
    /// too. It read a separate list until ENG-13065, and that list was
    /// missing `Flake`.
    #[test]
    fn an_embedder_leaves_no_refusal_anywhere_in_the_table() {
        for (need, _, _) in policy_table() {
            for (label, purity) in &CONFIGS {
                assert_ne!(
                    verdict(&need, *purity, PathReads::ThroughEmbedder),
                    Verdict::Refuse,
                    "{} under {label} refuses even with an embedder",
                    question_kind(&need)
                );
            }
        }
    }

    /// The detail a refusal carries has to say which setting forbade it,
    /// because "restrict-eval or pure-eval" -- the old wording -- named a
    /// setting that might not be on and left the reader to guess.
    #[test]
    fn a_refusal_names_the_settings_that_are_actually_on() {
        for ((label, purity), want) in CONFIGS.iter().zip([
            "no purity setting",
            "pure-eval",
            "restrict-eval",
            "pure-eval and restrict-eval",
        ]) {
            assert_eq!(purity.names(), want, "wrong wording for {label}");
        }
    }

    /// A fetch pinned with `sha256` is the branch that makes pure eval usable
    /// at all, so it gets its own assertion rather than living only inside
    /// the table.
    #[test]
    fn a_pinned_fetch_is_served_under_pure_eval() {
        let pure = Purity {
            pure_eval: true,
            restrict_eval: false,
        };
        for reads in [PathReads::Direct, PathReads::ThroughEmbedder] {
            assert_eq!(verdict(&fetch(true), pure, reads), Verdict::Ask);
            assert!(matches!(
                verdict(&fetch(false), pure, reads),
                Verdict::Error(_)
            ));
        }
    }
}
