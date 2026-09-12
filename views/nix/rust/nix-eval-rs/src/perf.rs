//! Where an evaluation's time went, counted rather than guessed at.
//!
//! # Why counters and not a profiler
//!
//! The VM is a flat trampoline: the whole dispatch loop inlines into one
//! function, so a sampling profiler attributes essentially all of an
//! evaluation to `eval::drive` and nothing below it. Measured on the crate
//! probe: 4284 of 4291 samples on `eval::drive+3380`, with no callees. The
//! only reason a profile of the *bridged* binary decomposes at all is that
//! the C++ callbacks are not inlined and act as accidental instrumentation.
//!
//! So the numbers this evaluator needs about itself have to come from
//! inside it. `maintainers/ix/nixos-toplevel-profile.md` is what that costs
//! without them: five sampled runs, a hand-written call-graph parser, and a
//! compile share that still came out as a range of 12.9% to 32.6% because
//! the phase is front-loaded and the sampler attaches at a variable offset.
//!
//! # The shape, and what it is not
//!
//! Counters only. This module performs no IO, opens no file and reads no
//! environment variable: it accumulates numbers and hands out a snapshot.
//! Printing is the embedder's job, through [`crate::capi::ixe_perf_snapshot`].
//! That is not fastidiousness -- "the VM performs no IO" is what makes a
//! recorded read set complete, and a perf module that read `IXE_PERF` itself
//! would be the same defect as `getEnv` reaching `std::env` behind `Host`'s
//! back, which is a bug this crate already had once.
//!
//! Counters are also **not** part of the memo key and must never become an
//! input to evaluation. Nothing here is readable by a Nix program.
//!
//! # Cost
//!
//! Everything here is a thread-local `Cell<u64>` increment. The VM is
//! single-threaded, so an atomic would buy nothing and cost a lock prefix.
//!
//! The coarse counters -- one per compile, one per host question, one per
//! store-path computation -- sit beside work measured in microseconds and
//! are free at any resolution anyone cares about.
//!
//! The per-op counter is different: it sits in the innermost loop, and
//! `counting cannot cost what it measures` is a requirement rather than a
//! hope. It is therefore behind the `perf-ops` cargo feature, off by
//! default. `maintainers/ix/perf-counter-overhead.md` records the on/off
//! pair that justifies the split.

use std::cell::Cell;

/// One evaluation's numbers. `Copy`, so a snapshot cannot alias the live
/// counters and drift from them while a caller formats it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Modules compiled, and the wall time inside `compile_source`.
    pub compiles: u64,
    pub compile_ns: u64,
    /// Source bytes handed to the compiler. The denominator for a
    /// per-byte compile cost.
    pub compile_bytes: u64,
    /// Compiles the module cache answered without compiling, from a module
    /// it already held or from the store on disk. `compiles + compile_hits`
    /// is how many modules the evaluation needed; a cold process over a warm
    /// store shows them all here and `compiles=0`.
    pub compile_hits: u64,
    /// Host questions asked, and the sum of completed-answer latency.
    /// Synchronous questions measure the bridge call. Asynchronous questions
    /// measure begin-to-collect latency and can overlap other VM work. Begun
    /// questions the program abandons remain in the count and have a separate
    /// per-kind abandoned counter rather than invented completion latency.
    pub questions: u64,
    pub question_ns: u64,
    /// Wall time of complete `Host::collect` calls. This includes the host's
    /// receive, locking/map work, and post-receive answer finishing; it is not
    /// claimed as scheduler blocking time.
    pub collect_call_ns: u64,
    /// Distinct argument hashes asked of the two high-volume store questions.
    /// The sets that establish uniqueness live on `Vm`; these are only their
    /// cardinalities, so this IO-free module does not retain payloads.
    pub store_path_unique: u64,
    pub write_drv_unique: u64,
    /// `WriteDrv` asks the VM answered itself because the same `.drv` was
    /// already handed over in this evaluation (`Vm::note_drv_written`).
    pub write_drv_skipped: u64,
    /// `WriteDrv` asks the embedder host answered from the bytes and queued
    /// for the store, and the batches it handed over
    /// (`EmbedderHost::flush_derivations`). `deferred / flushes` is the
    /// batch size the store saw.
    pub write_drv_deferred: u64,
    pub write_drv_flushes: u64,
    /// `StoreFiltered` asks answered from the persistent copy memo
    /// (`readset::CopyMemo`) after one validity question, instead of by a
    /// walk of the tree. `q.StoreFiltered - q.StoreFiltered_served` is how
    /// many walked.
    pub copy_memo_served: u64,
    /// Live `Realise` asks answered by validity (`RecordingHost::
    /// realised_by_validity`): every path the context stands on held, so
    /// nothing to build and no build asked for. `q.Realise - realise_hits -
    /// q.Realise_validated` is how many reached the embedder's realise.
    pub realise_validated: u64,
    /// Persisted whole-evaluation witness size and compaction.
    pub witness_rows: u64,
    pub witness_rows_deduped: u64,
    /// Rows the tree fold removed before the witness was written: the
    /// evaluation recorded `witness_rows + witness_rows_folded`.
    pub witness_rows_folded: u64,
    pub witness_bytes: u64,
    /// The validity questions a witness replay asks: the lines of the batch
    /// asked up front plus every late question at a row
    /// (`replay_validity_late`), and how long the store took to answer them.
    /// Zero on a fresh evaluation, which has no witness to verify.
    pub replay_validity_lines: u64,
    pub replay_validity_ns: u64,
    pub replay_validity_failed: u64,
    /// Validity questions asked at a row, for a path the batch did not hold
    /// (`readset::Validity::holds`): each is one line, counted in
    /// `replay_validity_lines` and `replay_validity_ns` as well.
    pub replay_validity_late: u64,
    /// The batched sealing question, likewise: objects asked about, time.
    pub replay_sealing_objects: u64,
    pub replay_sealing_ns: u64,
    pub replay_sealing_failed: u64,
    /// Store sweeps after a publication: how many ran, their time, the
    /// entries (rows, witnesses, objects) and bytes they reclaimed.
    pub sweep_runs: u64,
    pub sweep_ns: u64,
    pub sweep_removed: u64,
    pub sweep_bytes_freed: u64,
    /// Sweeps that failed (census or removal error). Counted apart, and
    /// their time in `sweep_ns` too: a cap whose every enforcement fails is
    /// unenforced, and the failure is a warning the embedder may not print.
    pub sweep_failed: u64,
    /// Derivation ATerms added to or reused from the evaluation CAS.
    pub aterms_stored: u64,
    pub aterms_reused: u64,
    /// Persistent cache corruption refused before it could become an answer.
    /// Kept per kind because an unreadable witness, a swept object, and bytes
    /// that do not match their address have different owners and remedies.
    pub cache_witnesses_refused: u64,
    pub cache_objects_missing: u64,
    pub cache_objects_invalid: u64,
    /// Questions answered from the persistent memo without evaluating. A
    /// memo that never hits is correct and useless, and nothing else in this
    /// census can tell a warm run from a cold one on an expression that
    /// asks no questions.
    pub memo_served: u64,
    pub import_cache_hits: u64,
    pub import_cache_misses: u64,
    pub import_entry_forces: u64,
    /// Store-path and hash computations (`storepath`, `nixhash`, `drvpath`).
    pub hashes: u64,
    pub hash_ns: u64,
    /// Symbols interned. `Vm::intern` is 8% of a NixOS toplevel eval
    /// (ENG-12861) and this is its denominator.
    pub interns: u64,
    /// How many of those were the *first* sight of a name, so
    /// `interns - intern_misses` is the hit count and `intern_misses` is
    /// the interner's final size.
    ///
    /// Without it `interns` alone cannot say what a probe costs: eight
    /// million probes over a hundred names and over a hundred thousand are
    /// different situations, and only the second is worth a data structure
    /// argument. It is also the only allocating path left in `intern`, so
    /// it is the denominator for that too.
    pub intern_misses: u64,
    /// Constants placed in a module's pool. `Compiler::konst` linear-scans
    /// the pool, so this count squared is the shape of ENG-12860.
    pub konsts: u64,
    /// `Entries` questions answered from the driver's per-evaluation
    /// directory cache rather than by reading the directory again.
    ///
    /// `q.Entries` deliberately keeps counting every question the VM *asks*,
    /// because that is what makes the question census complete. This is the
    /// other half: `q.Entries - dir_hits` is how many directories were
    /// actually read. Reporting only the first would hide the cache
    /// entirely, and reporting only the second would quietly redefine a
    /// counter other gates already parse (ENG-12862).
    pub dir_hits: u64,
    /// `StorePath` questions answered from the per-evaluation copy memo:
    /// the same accounting as `dir_hits`, for coercions of a path to a
    /// string. `q.StorePath - copy_hits` is how many reached the host.
    pub copy_hits: u64,
    /// `Import` questions answered from the per-evaluation import memo.
    /// `q.Import - import_hits` is how many files were read.
    pub import_hits: u64,
    /// `Realise` questions answered from the per-evaluation realise memo.
    /// `q.Realise - realise_hits` is how many asks reached the host.
    pub realise_hits: u64,
    /// IR ops executed. Zero unless built with the `perf-ops` feature; a
    /// reader must not take a zero here for "no ops ran".
    pub ops: u64,
    /// Of `interns`, the probes that came from constructing an attrset:
    /// one per DYNAMIC name (`${e} = v;`) per set built. Static names never
    /// reach the interner (they come from the op's attr site through the link
    /// table); until 2026-09-04 they did, 50.7M times on one NixOS toplevel,
    /// and this counter is what showed it. Counted apart from `interns`
    /// because it decides where an interner fix belongs: making the interner
    /// faster, or not calling it. Behind `perf-ops` with `ops`, and legible
    /// through the same `ops_counted` flag.
    pub attr_name_interns: u64,
    /// Slot cells allocated holding a value: cppnix's `nrValues`, less the
    /// thunks, which are counted apart below. One `Rc<RefCell<SlotState>>`
    /// each; the census that says what the evaluator's footprint is made
    /// of, because the allocator's own statistics have no per-size bins
    /// and a heap profile of a 15 GB peak names allocation sites, not
    /// language objects.
    pub slot_values: u64,
    /// Slot cells allocated as thunks (`nrThunks`). A thunk that is later
    /// forced is still one allocation; the count is cells, not states.
    pub slot_thunks: u64,
    /// Slot cells allocated as pending applications (`f a` awaiting its
    /// force); cppnix folds these into `nrThunks`.
    pub slot_pending: u64,
    /// Slot cells holding a refusal (an unimplemented builtin).
    pub slot_refusals: u64,
    /// Attribute sets built, clones included (`nrAttrsets`): every `Attrs`
    /// that came into existence, which is what the heap held at some point,
    /// not what the program observed as distinct.
    pub attr_sets: u64,
    /// Entries across those sets (`nrAttrsInAttrsets`): 16 bytes each in
    /// the sorted `Vec<(Sym, Slot)>`. `attr_entries / attr_sets` is the
    /// mean set width, the number that decides whether a per-set header is
    /// worth shrinking.
    pub attr_entries: u64,
    /// Lists built: one `Rc<Vec<Slot>>` each.
    pub lists: u64,
    /// Elements across those lists (`nrListElems`), one `Slot` handle each.
    pub list_elems: u64,
    /// Environment frames built (`nrEnvs`), whether by a call, a `let`, or
    /// a `rec`. `with` scopes are not frames and are not counted here.
    pub frames: u64,
    /// Slots across those frames (`nrValuesInEnvs`): a frame built empty
    /// and filled once its thunks exist counts its slots at the fill.
    pub frame_slots: u64,
    /// Whether the `perf-ops` counters were compiled in, so their zeros are
    /// legible.
    ///
    /// It governs two fields, not one: `ops` and `attr_name_interns`.
    /// Naming only `ops` here is how a reader comes to
    /// treat `attr_name_interns=0` as "no attribute names were interned" --
    /// which happened, on a default build, to someone who grepped the
    /// rendered line for the intern fields and filtered this flag out of
    /// their own output.
    pub ops_counted: bool,
}

/// The `Yield` kinds a task machine can return, in declaration order.
///
/// These are the evaluator's own equivalents of cppnix's `nrThunks` and
/// `nrFunctionCalls`, and they are the denominators for the part of a run
/// neither the compile timer nor the question timer accounts for. On the
/// minimal NixOS toplevel that residue was 38% of cpuTime with nothing named
/// in it; these are what let somebody divide it by something.
pub const YIELD_KINDS: &[&str] = &["Done", "Force", "Apply", "Sub", "Need"];

/// Questions `eval::begin_slow` can hand to an asynchronous host worker:
/// [`crate::eval::SlowShape`]'s names, derived rather than listed again.
#[expect(
    clippy::indexing_slicing,
    reason = "a const initialiser: slice `get` is not callable here, `i < names.len()` is the loop \
              guard, and both arrays have `SlowShape::ALL.len()` elements by their types"
)]
pub const SLOW_QUESTION_KINDS: [&str; crate::eval::SlowShape::ALL.len()] = {
    let mut names = [""; crate::eval::SlowShape::ALL.len()];
    let mut i = 0;
    while i < names.len() {
        names[i] = crate::eval::SlowShape::ALL[i].name();
        i += 1;
    }
    names
};

/// Per-question-kind counts, indexed the way `purity::QUESTION_KINDS` is.
///
/// A fixed array rather than a map because the kinds are a closed set that
/// `purity` already enumerates, and because a map allocation per question
/// would be the counter costing what it measures.
///
/// Derived rather than written, since ENG-13065. It was `17` beside a
/// seventeen-name list, guarded by a test that the two numbers matched --
/// which held while the list itself was one short of the `NeedPath` enum, so
/// `getFlake` questions were counted in the total and in no bucket. The list
/// is now generated from `purity::question_kind`'s own match, and taking the
/// width from it means the array cannot be narrower than the set of kinds
/// that can reach it.
pub const KINDS: usize = crate::purity::QUESTION_KINDS.len();

/// Persistent cache corruption kinds visible in [`Snapshot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CacheCorruption {
    WitnessRefused,
    ObjectMissing,
    ObjectInvalid,
}

thread_local! {
    static COMPILES: Cell<u64> = const { Cell::new(0) };
    static COMPILE_NS: Cell<u64> = const { Cell::new(0) };
    static COMPILE_BYTES: Cell<u64> = const { Cell::new(0) };
    static COMPILE_HITS: Cell<u64> = const { Cell::new(0) };
    static QUESTIONS: Cell<u64> = const { Cell::new(0) };
    static QUESTION_NS: Cell<u64> = const { Cell::new(0) };
    static COLLECT_CALL_NS: Cell<u64> = const { Cell::new(0) };
    static STORE_PATH_UNIQUE: Cell<u64> = const { Cell::new(0) };
    static WRITE_DRV_UNIQUE: Cell<u64> = const { Cell::new(0) };
    static WRITE_DRV_SKIPPED: Cell<u64> = const { Cell::new(0) };
    static WRITE_DRV_DEFERRED: Cell<u64> = const { Cell::new(0) };
    static WRITE_DRV_FLUSHES: Cell<u64> = const { Cell::new(0) };
    static COPY_MEMO_SERVED: Cell<u64> = const { Cell::new(0) };
    static REALISE_VALIDATED: Cell<u64> = const { Cell::new(0) };
    static WITNESS_ROWS: Cell<u64> = const { Cell::new(0) };
    static WITNESS_ROWS_DEDUPED: Cell<u64> = const { Cell::new(0) };
    static WITNESS_ROWS_FOLDED: Cell<u64> = const { Cell::new(0) };
    static WITNESS_BYTES: Cell<u64> = const { Cell::new(0) };
    static ATERMS_STORED: Cell<u64> = const { Cell::new(0) };
    static ATERMS_REUSED: Cell<u64> = const { Cell::new(0) };
    static CACHE_WITNESSES_REFUSED: Cell<u64> = const { Cell::new(0) };
    static CACHE_OBJECTS_MISSING: Cell<u64> = const { Cell::new(0) };
    static CACHE_OBJECTS_INVALID: Cell<u64> = const { Cell::new(0) };
    static MEMO_SERVED: Cell<u64> = const { Cell::new(0) };
    static IMPORT_CACHE_HITS: Cell<u64> = const { Cell::new(0) };
    static IMPORT_CACHE_MISSES: Cell<u64> = const { Cell::new(0) };
    static IMPORT_ENTRY_FORCES: Cell<u64> = const { Cell::new(0) };
    static HASHES: Cell<u64> = const { Cell::new(0) };
    static HASH_NS: Cell<u64> = const { Cell::new(0) };
    static INTERNS: Cell<u64> = const { Cell::new(0) };
    static INTERN_MISSES: Cell<u64> = const { Cell::new(0) };
    static KONSTS: Cell<u64> = const { Cell::new(0) };
    static DIR_HITS: Cell<u64> = const { Cell::new(0) };
    static COPY_HITS: Cell<u64> = const { Cell::new(0) };
    static IMPORT_HITS: Cell<u64> = const { Cell::new(0) };
    static REALISE_HITS: Cell<u64> = const { Cell::new(0) };
    static OPS: Cell<u64> = const { Cell::new(0) };
    static ATTR_NAME_INTERNS: Cell<u64> = const { Cell::new(0) };
    static SLOT_VALUES: Cell<u64> = const { Cell::new(0) };
    static SLOT_THUNKS: Cell<u64> = const { Cell::new(0) };
    static SLOT_PENDING: Cell<u64> = const { Cell::new(0) };
    static SLOT_REFUSALS: Cell<u64> = const { Cell::new(0) };
    static ATTR_SETS: Cell<u64> = const { Cell::new(0) };
    static ATTR_ENTRIES: Cell<u64> = const { Cell::new(0) };
    static LISTS: Cell<u64> = const { Cell::new(0) };
    static LIST_ELEMS: Cell<u64> = const { Cell::new(0) };
    static FRAMES: Cell<u64> = const { Cell::new(0) };
    static FRAME_SLOTS: Cell<u64> = const { Cell::new(0) };
    static BY_KIND: [Cell<u64>; KINDS] = const { [const { Cell::new(0) }; KINDS] };
    static BY_KIND_NS: [Cell<u64>; KINDS] = const { [const { Cell::new(0) }; KINDS] };
    static SLOW_BY_KIND: [Cell<u64>; KINDS] = const { [const { Cell::new(0) }; KINDS] };
    static SLOW_BY_KIND_NS: [Cell<u64>; KINDS] = const { [const { Cell::new(0) }; KINDS] };
    static REPLAY_BY_KIND_NS: [Cell<u64>; KINDS] = const { [const { Cell::new(0) }; KINDS] };
    static REPLAY_ASKED_BY_KIND: [Cell<u64>; KINDS] = const { [const { Cell::new(0) }; KINDS] };
    static REPLAY_RECORDED_BY_KIND: [Cell<u64>; KINDS] = const { [const { Cell::new(0) }; KINDS] };
    static REPLAY_VALIDATED_BY_KIND: [Cell<u64>; KINDS] = const { [const { Cell::new(0) }; KINDS] };
    static REPLAY_CHANGED_BY_KIND: [Cell<u64>; KINDS] = const { [const { Cell::new(0) }; KINDS] };
    static REPLAY_VALIDITY_LINES: Cell<u64> = const { Cell::new(0) };
    static REPLAY_VALIDITY_FAILED: Cell<u64> = const { Cell::new(0) };
    static REPLAY_VALIDITY_LATE: Cell<u64> = const { Cell::new(0) };
    static REPLAY_SEALING_OBJECTS: Cell<u64> = const { Cell::new(0) };
    static REPLAY_SEALING_NS: Cell<u64> = const { Cell::new(0) };
    static REPLAY_SEALING_FAILED: Cell<u64> = const { Cell::new(0) };
    static SWEEP_RUNS: Cell<u64> = const { Cell::new(0) };
    static SWEEP_NS: Cell<u64> = const { Cell::new(0) };
    static SWEEP_REMOVED: Cell<u64> = const { Cell::new(0) };
    static SWEEP_BYTES_FREED: Cell<u64> = const { Cell::new(0) };
    static SWEEP_FAILED: Cell<u64> = const { Cell::new(0) };
    static REPLAY_VALIDITY_NS: Cell<u64> = const { Cell::new(0) };
    static ABANDONED_BY_KIND: [Cell<u64>; KINDS] = const { [const { Cell::new(0) }; KINDS] };
    static BY_YIELD: [Cell<u64>; 5] = const { [const { Cell::new(0) }; 5] };
    static BY_OP: [Cell<u64>; crate::ir::OpKind::COUNT] =
        const { [const { Cell::new(0) }; crate::ir::OpKind::COUNT] };
}

/// Gated alongside its callers: with both features off every `note_*` body
/// is compiled out, and an ungated helper nothing calls is a dead-code
/// warning in the build the gating exists to keep clean.
#[cfg(any(feature = "perf", feature = "perf-ops"))]
fn bump(cell: &'static std::thread::LocalKey<Cell<u64>>, by: u64) {
    cell.with(|c| c.set(c.get().wrapping_add(by)));
}

/// A compile finished: its source size and how long it took.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_compile(bytes: usize, nanos: u64) {
    #[cfg(feature = "perf")]
    {
        bump(&COMPILES, 1);
        bump(&COMPILE_BYTES, bytes as u64);
        bump(&COMPILE_NS, nanos);
    }
}

/// The module cache served a compile without compiling.
pub fn note_compile_hit() {
    #[cfg(not(feature = "perf"))]
    return;
    #[cfg(feature = "perf")]
    bump(&COMPILE_HITS, 1);
}

/// A host question crossed out of the VM. `kind` is the index of
/// `purity::question_kind`'s answer in `purity::QUESTION_KINDS`.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_question_asked(kind: usize) {
    #[cfg(feature = "perf")]
    {
        bump(&QUESTIONS, 1);
        if kind < KINDS {
            BY_KIND.with(|k| {
                if let Some(c) = k.get(kind) {
                    c.set(c.get().wrapping_add(1));
                }
            });
        }
    }
}

/// A host question produced the answer delivered to the VM. Questions whose
/// speculative strand is abandoned never call this; their count is preserved
/// by [`note_question_asked`] and classified by [`note_question_abandoned`].
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_question_answered(kind: usize, nanos: u64) {
    #[cfg(feature = "perf")]
    {
        bump(&QUESTION_NS, nanos);
        if kind < KINDS {
            BY_KIND_NS.with(|k| {
                if let Some(c) = k.get(kind) {
                    c.set(c.get().wrapping_add(nanos));
                }
            });
        }
    }
}

/// The witness verifier asked one recorded question again and got its
/// answer, on a memo lookup. Timed per kind, because a hit's whole cost is
/// this replay and `question_ns` counts only what the VM asked.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_replayed(kind: usize, nanos: u64) {
    #[cfg(feature = "perf")]
    {
        if kind < KINDS {
            REPLAY_BY_KIND_NS.with(|k| {
                if let Some(c) = k.get(kind) {
                    c.set(c.get().wrapping_add(nanos));
                }
            });
            REPLAY_ASKED_BY_KIND.with(|k| {
                if let Some(c) = k.get(kind) {
                    c.set(c.get().wrapping_add(1));
                }
            });
        }
    }
}

/// One witness row of `kind` replayed from its recorded answer, with no
/// host call (a read under a mounted root; `Question::replays_from_record`).
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_recorded(kind: usize) {
    #[cfg(feature = "perf")]
    {
        if kind < KINDS {
            REPLAY_RECORDED_BY_KIND.with(|k| {
                if let Some(c) = k.get(kind) {
                    c.set(c.get().wrapping_add(1));
                }
            });
        }
    }
}

/// One witness row of `kind` answered from the store's presence answers
/// (the batch, or a late question at the row), with no host call of its
/// own (`Question::answer_by_validity`).
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_validated(kind: usize) {
    #[cfg(feature = "perf")]
    {
        if kind < KINDS {
            REPLAY_VALIDATED_BY_KIND.with(|k| {
                if let Some(c) = k.get(kind) {
                    c.set(c.get().wrapping_add(1));
                }
            });
        }
    }
}

/// One witness row of `kind` whose replayed digest differs from the one
/// recorded: the row that turns a hit into a miss. A replay that keeps
/// missing is paying for a cache that never pays back, and without this the
/// only visible fact is `memo_served=0` with no kind attached.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_changed(kind: usize) {
    #[cfg(feature = "perf")]
    {
        if kind < KINDS {
            REPLAY_CHANGED_BY_KIND.with(|k| {
                if let Some(c) = k.get(kind) {
                    c.set(c.get().wrapping_add(1));
                }
            });
        }
    }
}

/// The verifier asked the store which of `lines` objects it holds, once for
/// a whole witness (`readset::present_objects`), taking `nanos`.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_validity(lines: usize, nanos: u64) {
    #[cfg(feature = "perf")]
    {
        bump(
            &REPLAY_VALIDITY_LINES,
            u64::try_from(lines).unwrap_or(u64::MAX),
        );
        bump(&REPLAY_VALIDITY_NS, nanos);
    }
}

/// A validity question of one path asked at a row, for a path the batch did
/// not hold: counted in the aggregate `replay_validity_lines` and
/// `replay_validity_ns` and on its own, so a replay that asks late is told
/// apart from one that never needed to.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_validity_late(nanos: u64) {
    #[cfg(feature = "perf")]
    {
        bump(&REPLAY_VALIDITY_LINES, 1);
        bump(&REPLAY_VALIDITY_NS, nanos);
        bump(&REPLAY_VALIDITY_LATE, 1);
    }
}

/// The validity question failed (`Host::valid_paths` answered an error):
/// every row that relied on it asks instead, which is correct and slow, and
/// without this count it would be indistinguishable from a store that holds
/// nothing.
pub fn note_validity_failed() {
    #[cfg(feature = "perf")]
    bump(&REPLAY_VALIDITY_FAILED, 1);
}

/// The batched sealing question (`Host::sealed_paths`): how many objects it
/// asked about and how long the host took, kept apart from the validity
/// question because they are different calls with different units.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_sealing(objects: usize, nanos: u64) {
    #[cfg(feature = "perf")]
    {
        bump(
            &REPLAY_SEALING_OBJECTS,
            u64::try_from(objects).unwrap_or(u64::MAX),
        );
        bump(&REPLAY_SEALING_NS, nanos);
    }
}

/// The sealing question failed: every read under the objects it was asked
/// about asks instead.
pub fn note_sealing_failed() {
    #[cfg(feature = "perf")]
    bump(&REPLAY_SEALING_FAILED, 1);
}

/// One store sweep ran (`Store::sweep`, `Store::sweep_to_cap`): its wall
/// time including the census, how many entries it removed across rows,
/// witnesses, objects and leftovers (0 when the store was under its cap),
/// and the bytes it freed. The claim the sweep makes is that it costs
/// entries and not bytes, and this is the number that claim is checked
/// against.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_sweep(nanos: u64, removed: u64, bytes_freed: u64) {
    #[cfg(feature = "perf")]
    {
        bump(&SWEEP_RUNS, 1);
        bump(&SWEEP_NS, nanos);
        bump(&SWEEP_REMOVED, removed);
        bump(&SWEEP_BYTES_FREED, bytes_freed);
    }
}

/// One store sweep failed before or during removal, after `nanos` of work.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_sweep_failed(nanos: u64) {
    #[cfg(feature = "perf")]
    {
        bump(&SWEEP_FAILED, 1);
        bump(&SWEEP_NS, nanos);
    }
}

/// One new argument hash was observed for `StorePath`.
pub fn note_unique_store_path() {
    #[cfg(feature = "perf")]
    bump(&STORE_PATH_UNIQUE, 1);
}

/// One new argument hash was observed for `WriteDrv`.
pub fn note_unique_write_drv() {
    #[cfg(feature = "perf")]
    bump(&WRITE_DRV_UNIQUE, 1);
}

/// One `WriteDrv` the VM answered itself: the derivation was already handed
/// over in this evaluation.
pub fn note_drv_write_skipped() {
    #[cfg(feature = "perf")]
    bump(&WRITE_DRV_SKIPPED, 1);
}

/// One `WriteDrv` the embedder host answered from the bytes and queued.
pub fn note_drv_write_deferred() {
    #[cfg(feature = "perf")]
    bump(&WRITE_DRV_DEFERRED, 1);
}

/// One `StoreFiltered` the persistent copy memo answered, validity confirmed.
pub fn note_copy_memo_served() {
    #[cfg(feature = "perf")]
    bump(&COPY_MEMO_SERVED, 1);
}

/// One live `Realise` answered by validity: nothing to build, no build asked.
pub fn note_realise_validated() {
    #[cfg(feature = "perf")]
    bump(&REALISE_VALIDATED, 1);
}

/// One batch of `count` queued derivations was handed to the store.
pub fn note_drv_write_flush(count: u64) {
    #[cfg(feature = "perf")]
    bump(&WRITE_DRV_FLUSHES, 1);
    #[cfg(not(feature = "perf"))]
    let _ = count;
    #[cfg(feature = "perf")]
    debug_assert!(count > 0, "an empty batch is not a flush");
}

/// A certified imported scalar was restored instead of forcing its root.
pub fn note_import_cache_hit() {
    #[cfg(feature = "perf")]
    bump(&IMPORT_CACHE_HITS, 1);
}

/// A certified imported scalar had no reusable result.
pub fn note_import_cache_miss() {
    #[cfg(feature = "perf")]
    bump(&IMPORT_CACHE_MISSES, 1);
}

/// An import requested its ordinary root force, including when caching is off.
pub fn note_import_entry_force() {
    #[cfg(feature = "perf")]
    bump(&IMPORT_ENTRY_FORCES, 1);
}

/// One evaluation witness was recorded after duplicate rows were removed
/// and the reads under immutable trees folded into tree rows.
pub fn note_witness(rows: u64, rows_deduped: u64, rows_folded: u64, bytes: u64) {
    #[cfg(not(feature = "perf"))]
    let _ = (rows, rows_deduped, rows_folded, bytes);
    #[cfg(feature = "perf")]
    {
        bump(&WITNESS_ROWS, rows);
        bump(&WITNESS_ROWS_DEDUPED, rows_deduped);
        bump(&WITNESS_ROWS_FOLDED, rows_folded);
        bump(&WITNESS_BYTES, bytes);
    }
}

/// Derivation ATerms stored in or reused from the result cache's CAS.
pub fn note_aterms(stored: u64, reused: u64) {
    #[cfg(not(feature = "perf"))]
    let _ = (stored, reused);
    #[cfg(feature = "perf")]
    {
        bump(&ATERMS_STORED, stored);
        bump(&ATERMS_REUSED, reused);
    }
}

/// One persistent cache entry was refused rather than trusted.
pub(crate) fn note_cache_corruption(kind: CacheCorruption) {
    #[cfg(not(feature = "perf"))]
    let _ = kind;
    #[cfg(feature = "perf")]
    bump(
        match kind {
            CacheCorruption::WitnessRefused => &CACHE_WITNESSES_REFUSED,
            CacheCorruption::ObjectMissing => &CACHE_OBJECTS_MISSING,
            CacheCorruption::ObjectInvalid => &CACHE_OBJECTS_INVALID,
        },
        1,
    );
}

/// A question was answered from the persistent memo, with no evaluation.
pub(crate) fn note_memo_served() {
    #[cfg(feature = "perf")]
    bump(&MEMO_SERVED, 1);
}

/// A slow question was accepted by `Host::begin` and started asynchronously.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_slow_question_started(kind: usize) {
    #[cfg(feature = "perf")]
    if kind < KINDS {
        SLOW_BY_KIND.with(|k| {
            if let Some(c) = k.get(kind) {
                c.set(c.get().wrapping_add(1));
            }
        });
    }
}

/// A begun slow question completed. `nanos` is its end-to-end latency from
/// `Host::begin` until collection, including time overlapped with VM work.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_slow_question_answered(kind: usize, nanos: u64) {
    #[cfg(feature = "perf")]
    if kind < KINDS {
        SLOW_BY_KIND_NS.with(|k| {
            if let Some(c) = k.get(kind) {
                c.set(c.get().wrapping_add(nanos));
            }
        });
    }
}

/// An asynchronously begun question was dropped because its speculative
/// strand was no longer needed. It was still an ask, but produced no delivered
/// answer and therefore contributes no completed-answer latency.
pub fn note_question_abandoned(kind: usize) {
    #[cfg(not(feature = "perf"))]
    let _ = kind;
    #[cfg(feature = "perf")]
    if kind < KINDS {
        ABANDONED_BY_KIND.with(|k| {
            if let Some(c) = k.get(kind) {
                c.set(c.get().wrapping_add(1));
            }
        });
    }
}

/// Wall time of one complete `Host::collect` call.
pub fn note_collect_call(nanos: u64) {
    #[cfg(not(feature = "perf"))]
    let _ = nanos;
    #[cfg(feature = "perf")]
    bump(&COLLECT_CALL_NS, nanos);
}

/// A task machine yielded. `kind` indexes [`YIELD_KINDS`].
///
/// One site, the single `match y` in `Vm::advance_task`, so the count is
/// complete the way the question count is. Not as hot as the op counter --
/// a yield is a task suspension, not an instruction -- so it rides with the
/// coarse counters rather than behind `perf-ops`.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_yield(kind: usize) {
    #[cfg(feature = "perf")]
    BY_YIELD.with(|y| {
        if let Some(c) = y.get(kind) {
            c.set(c.get().wrapping_add(1));
        }
    });
}

/// A store path or hash was computed.
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_hash(nanos: u64) {
    #[cfg(feature = "perf")]
    {
        bump(&HASHES, 1);
        bump(&HASH_NS, nanos);
    }
}

/// A symbol was interned. Called on every `Vm::intern`, hit or miss, because
/// the cost ENG-12861 is about is the *lookup*, not the insert.
pub fn note_intern() {
    #[cfg(feature = "perf")]
    bump(&INTERNS, 1);
}

/// A name was interned for the first time: one allocation, one new slot.
pub fn note_intern_miss() {
    #[cfg(not(feature = "perf"))]
    return;
    #[cfg(feature = "perf")]
    bump(&INTERN_MISSES, 1);
}

/// A constant was placed in a module's pool.
pub fn note_konst() {
    #[cfg(feature = "perf")]
    bump(&KONSTS, 1);
}

/// An `Entries` question was answered from the per-evaluation directory
/// cache instead of by reading the directory.
pub fn note_dir_hit() {
    #[cfg(not(feature = "perf"))]
    return;
    #[cfg(feature = "perf")]
    bump(&DIR_HITS, 1);
}

/// A `StorePath` question was answered from the per-evaluation copy memo
/// instead of by asking the host.
pub fn note_copy_hit() {
    #[cfg(not(feature = "perf"))]
    return;
    #[cfg(feature = "perf")]
    bump(&COPY_HITS, 1);
}

/// A `Realise` question was answered from the per-evaluation realise memo
/// instead of by asking the host to build.
pub fn note_realise_hit() {
    #[cfg(not(feature = "perf"))]
    return;
    #[cfg(feature = "perf")]
    bump(&REALISE_HITS, 1);
}

/// An `Import` question was answered from the per-evaluation import memo
/// instead of by reading the file.
pub fn note_import_hit() {
    #[cfg(not(feature = "perf"))]
    return;
    #[cfg(feature = "perf")]
    bump(&IMPORT_HITS, 1);
}

/// One IR op executed. Compiled out entirely without `perf-ops`.
///
/// Takes the op rather than its kind so that without the feature the caller
/// hands over a reference it already holds and nothing at all is computed;
/// `Op::kind` runs inside the gate.
#[inline(always)]
pub fn note_op(op: &crate::ir::Op) {
    #[cfg(not(feature = "perf-ops"))]
    let _ = op;
    #[cfg(feature = "perf-ops")]
    {
        bump(&OPS, 1);
        let i = op.kind() as usize;
        BY_OP.with(|k| {
            if let Some(c) = k.get(i) {
                c.set(c.get().wrapping_add(1));
            }
        });
    }
}

/// A dynamic attribute name was interned while building an attrset, from
/// `Op::MkAttrs` or `Op::MkAttrsOnto`.
///
/// A subset of [`note_intern`], not a sibling: every call here is
/// accompanied by one there, so `attr_name_interns <= interns` always (the
/// `perf-ops` feature implies `perf` for exactly that reason). Behind
/// `perf-ops` with the other per-op counters.
#[inline(always)]
pub fn note_attr_name_intern() {
    #[cfg(feature = "perf-ops")]
    bump(&ATTR_NAME_INTERNS, 1);
}

/// A slot cell was allocated holding a value.
///
/// The allocation census (`alloc.*` on the rendered line) is cppnix's
/// `nrValues` / `nrThunks` / `nrAttrsets` / `nrListElems` / `nrEnvs` family:
/// one thread-local increment per language object, the same price cppnix
/// pays unconditionally. Behind `perf` like the other counters; the
/// constructors in `value2` are the only callers, so a count here is a
/// heap object and a heap object is a count here.
#[inline(always)]
pub fn note_slot_value() {
    #[cfg(feature = "perf")]
    bump(&SLOT_VALUES, 1);
}

/// A slot cell was allocated as a thunk.
#[inline(always)]
pub fn note_slot_thunk() {
    #[cfg(feature = "perf")]
    bump(&SLOT_THUNKS, 1);
}

/// A slot cell was allocated as a pending application.
#[inline(always)]
pub fn note_slot_pending() {
    #[cfg(feature = "perf")]
    bump(&SLOT_PENDING, 1);
}

/// A slot cell was allocated holding a refusal.
#[inline(always)]
pub fn note_slot_refusal() {
    #[cfg(feature = "perf")]
    bump(&SLOT_REFUSALS, 1);
}

/// An attribute set with `entries` bindings was built (or cloned).
#[inline(always)]
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_attrs(entries: usize) {
    #[cfg(feature = "perf")]
    {
        bump(&ATTR_SETS, 1);
        bump(&ATTR_ENTRIES, entries as u64);
    }
}

/// A list with `elems` elements was built.
#[inline(always)]
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_list(elems: usize) {
    #[cfg(feature = "perf")]
    {
        bump(&LISTS, 1);
        bump(&LIST_ELEMS, elems as u64);
    }
}

/// An environment frame was built with `slots` slots.
#[inline(always)]
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_frame(slots: usize) {
    #[cfg(feature = "perf")]
    {
        bump(&FRAMES, 1);
        bump(&FRAME_SLOTS, slots as u64);
    }
}

/// A frame built empty received its `slots` slots.
#[inline(always)]
#[cfg_attr(
    not(feature = "perf"),
    expect(
        unused_variables,
        reason = "the counters are compiled out without `perf`"
    )
)]
pub fn note_frame_filled(slots: usize) {
    #[cfg(feature = "perf")]
    bump(&FRAME_SLOTS, slots as u64);
}

/// Read the counters. Does not reset them; see [`reset`].
#[must_use]
pub fn snapshot() -> Snapshot {
    Snapshot {
        compiles: COMPILES.with(Cell::get),
        compile_ns: COMPILE_NS.with(Cell::get),
        compile_bytes: COMPILE_BYTES.with(Cell::get),
        compile_hits: COMPILE_HITS.with(Cell::get),
        questions: QUESTIONS.with(Cell::get),
        question_ns: QUESTION_NS.with(Cell::get),
        collect_call_ns: COLLECT_CALL_NS.with(Cell::get),
        store_path_unique: STORE_PATH_UNIQUE.with(Cell::get),
        write_drv_unique: WRITE_DRV_UNIQUE.with(Cell::get),
        write_drv_skipped: WRITE_DRV_SKIPPED.with(Cell::get),
        write_drv_deferred: WRITE_DRV_DEFERRED.with(Cell::get),
        write_drv_flushes: WRITE_DRV_FLUSHES.with(Cell::get),
        copy_memo_served: COPY_MEMO_SERVED.with(Cell::get),
        realise_validated: REALISE_VALIDATED.with(Cell::get),
        witness_rows: WITNESS_ROWS.with(Cell::get),
        witness_rows_deduped: WITNESS_ROWS_DEDUPED.with(Cell::get),
        witness_rows_folded: WITNESS_ROWS_FOLDED.with(Cell::get),
        witness_bytes: WITNESS_BYTES.with(Cell::get),
        replay_validity_lines: REPLAY_VALIDITY_LINES.with(Cell::get),
        replay_validity_ns: REPLAY_VALIDITY_NS.with(Cell::get),
        replay_validity_failed: REPLAY_VALIDITY_FAILED.with(Cell::get),
        replay_validity_late: REPLAY_VALIDITY_LATE.with(Cell::get),
        replay_sealing_objects: REPLAY_SEALING_OBJECTS.with(Cell::get),
        replay_sealing_ns: REPLAY_SEALING_NS.with(Cell::get),
        replay_sealing_failed: REPLAY_SEALING_FAILED.with(Cell::get),
        sweep_runs: SWEEP_RUNS.with(Cell::get),
        sweep_ns: SWEEP_NS.with(Cell::get),
        sweep_removed: SWEEP_REMOVED.with(Cell::get),
        sweep_bytes_freed: SWEEP_BYTES_FREED.with(Cell::get),
        sweep_failed: SWEEP_FAILED.with(Cell::get),
        aterms_stored: ATERMS_STORED.with(Cell::get),
        aterms_reused: ATERMS_REUSED.with(Cell::get),
        cache_witnesses_refused: CACHE_WITNESSES_REFUSED.with(Cell::get),
        cache_objects_missing: CACHE_OBJECTS_MISSING.with(Cell::get),
        cache_objects_invalid: CACHE_OBJECTS_INVALID.with(Cell::get),
        memo_served: MEMO_SERVED.with(Cell::get),
        import_cache_hits: IMPORT_CACHE_HITS.with(Cell::get),
        import_cache_misses: IMPORT_CACHE_MISSES.with(Cell::get),
        import_entry_forces: IMPORT_ENTRY_FORCES.with(Cell::get),
        hashes: HASHES.with(Cell::get),
        hash_ns: HASH_NS.with(Cell::get),
        interns: INTERNS.with(Cell::get),
        intern_misses: INTERN_MISSES.with(Cell::get),
        konsts: KONSTS.with(Cell::get),
        dir_hits: DIR_HITS.with(Cell::get),
        copy_hits: COPY_HITS.with(Cell::get),
        import_hits: IMPORT_HITS.with(Cell::get),
        realise_hits: REALISE_HITS.with(Cell::get),
        ops: OPS.with(Cell::get),
        attr_name_interns: ATTR_NAME_INTERNS.with(Cell::get),
        slot_values: SLOT_VALUES.with(Cell::get),
        slot_thunks: SLOT_THUNKS.with(Cell::get),
        slot_pending: SLOT_PENDING.with(Cell::get),
        slot_refusals: SLOT_REFUSALS.with(Cell::get),
        attr_sets: ATTR_SETS.with(Cell::get),
        attr_entries: ATTR_ENTRIES.with(Cell::get),
        lists: LISTS.with(Cell::get),
        list_elems: LIST_ELEMS.with(Cell::get),
        frames: FRAMES.with(Cell::get),
        frame_slots: FRAME_SLOTS.with(Cell::get),
        ops_counted: cfg!(feature = "perf-ops"),
    }
}

/// Per-question-kind counts, parallel to [`crate::purity::QUESTION_KINDS`].
#[must_use]
pub fn by_kind() -> [u64; KINDS] {
    BY_KIND.with(|k| std::array::from_fn(|i| k.get(i).map_or(0, Cell::get)))
}

/// Per-question-kind answer latency, parallel to
/// [`crate::purity::QUESTION_KINDS`].
#[must_use]
pub fn by_kind_ns() -> [u64; KINDS] {
    BY_KIND_NS.with(|k| std::array::from_fn(|i| k.get(i).map_or(0, Cell::get)))
}

/// Counts of asynchronously begun slow questions, by question kind.
#[must_use]
pub fn slow_by_kind() -> [u64; KINDS] {
    SLOW_BY_KIND.with(|k| std::array::from_fn(|i| k.get(i).map_or(0, Cell::get)))
}

/// End-to-end latency of asynchronously begun slow questions, by kind.
#[must_use]
pub fn slow_by_kind_ns() -> [u64; KINDS] {
    SLOW_BY_KIND_NS.with(|k| std::array::from_fn(|i| k.get(i).map_or(0, Cell::get)))
}

/// Nanoseconds the witness verifier spent re-asking each kind of question.
#[must_use]
pub fn replay_by_kind_ns() -> [u64; KINDS] {
    REPLAY_BY_KIND_NS.with(|k| std::array::from_fn(|i| k.get(i).map_or(0, Cell::get)))
}

/// Witness rows of each kind the verifier asked the host again (the `_ns`
/// counter is their cost; this is their number, so a cost has a
/// denominator).
#[must_use]
pub fn replay_asked_by_kind() -> [u64; KINDS] {
    REPLAY_ASKED_BY_KIND.with(|k| std::array::from_fn(|i| k.get(i).map_or(0, Cell::get)))
}

/// Witness rows of each kind the verifier answered from the record instead
/// of asking.
#[must_use]
pub fn replay_recorded_by_kind() -> [u64; KINDS] {
    REPLAY_RECORDED_BY_KIND.with(|k| std::array::from_fn(|i| k.get(i).map_or(0, Cell::get)))
}

/// Witness rows of each kind the verifier answered from the batched
/// validity question instead of asking.
#[must_use]
pub fn replay_validated_by_kind() -> [u64; KINDS] {
    REPLAY_VALIDATED_BY_KIND.with(|k| std::array::from_fn(|i| k.get(i).map_or(0, Cell::get)))
}

/// Witness rows of each kind whose replayed digest differs from the record.
#[must_use]
pub fn replay_changed_by_kind() -> [u64; KINDS] {
    REPLAY_CHANGED_BY_KIND.with(|k| std::array::from_fn(|i| k.get(i).map_or(0, Cell::get)))
}

/// Counts of asynchronously begun questions the program abandoned, by kind.
#[must_use]
pub fn abandoned_by_kind() -> [u64; KINDS] {
    ABANDONED_BY_KIND.with(|k| std::array::from_fn(|i| k.get(i).map_or(0, Cell::get)))
}

/// Per-yield-kind counts, parallel to [`YIELD_KINDS`].
#[must_use]
pub fn by_yield() -> [u64; 5] {
    BY_YIELD.with(|y| std::array::from_fn(|i| y.get(i).map_or(0, Cell::get)))
}

/// Per-op-kind counts, parallel to [`crate::ir::OpKind::ALL`].
///
/// All zero unless built with `perf-ops`; `Snapshot::ops_counted` is what
/// separates that from an evaluation that ran no ops.
#[must_use]
pub fn by_op() -> [u64; crate::ir::OpKind::COUNT] {
    BY_OP.with(|k| std::array::from_fn(|i| k.get(i).map_or(0, Cell::get)))
}

/// How many times [`reset`] has run. State that lives outside this module
/// but belongs to one measurement -- the VM's sets of distinct question
/// arguments -- compares against it and starts over when it moves, so a
/// reset that zeroes the counters also empties what feeds them.
static RESET_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The current measurement epoch; see [`RESET_EPOCH`].
#[must_use]
pub fn reset_epoch() -> u64 {
    RESET_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Zero everything. An embedder measuring one evaluation calls this first;
/// without it a second evaluation in one process reports the sum, which is
/// the sort of number that looks plausible and is wrong.
pub fn reset() {
    RESET_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for c in [
        &COMPILES,
        &COMPILE_NS,
        &COMPILE_BYTES,
        &COMPILE_HITS,
        &QUESTIONS,
        &QUESTION_NS,
        &COLLECT_CALL_NS,
        &STORE_PATH_UNIQUE,
        &WRITE_DRV_UNIQUE,
        &WRITE_DRV_SKIPPED,
        &WRITE_DRV_DEFERRED,
        &WRITE_DRV_FLUSHES,
        &COPY_MEMO_SERVED,
        &REALISE_VALIDATED,
        &WITNESS_ROWS,
        &WITNESS_ROWS_DEDUPED,
        &WITNESS_ROWS_FOLDED,
        &WITNESS_BYTES,
        &REPLAY_VALIDITY_LINES,
        &REPLAY_VALIDITY_FAILED,
        &REPLAY_VALIDITY_LATE,
        &REPLAY_SEALING_OBJECTS,
        &REPLAY_SEALING_NS,
        &REPLAY_SEALING_FAILED,
        &SWEEP_RUNS,
        &SWEEP_NS,
        &SWEEP_REMOVED,
        &SWEEP_BYTES_FREED,
        &SWEEP_FAILED,
        &REPLAY_VALIDITY_NS,
        &ATERMS_STORED,
        &ATERMS_REUSED,
        &CACHE_WITNESSES_REFUSED,
        &CACHE_OBJECTS_MISSING,
        &CACHE_OBJECTS_INVALID,
        &MEMO_SERVED,
        &IMPORT_CACHE_HITS,
        &IMPORT_CACHE_MISSES,
        &IMPORT_ENTRY_FORCES,
        &HASHES,
        &HASH_NS,
        &INTERNS,
        &INTERN_MISSES,
        &KONSTS,
        &DIR_HITS,
        &COPY_HITS,
        &IMPORT_HITS,
        &REALISE_HITS,
        &OPS,
        &ATTR_NAME_INTERNS,
        &SLOT_VALUES,
        &SLOT_THUNKS,
        &SLOT_PENDING,
        &SLOT_REFUSALS,
        &ATTR_SETS,
        &ATTR_ENTRIES,
        &LISTS,
        &LIST_ELEMS,
        &FRAMES,
        &FRAME_SLOTS,
    ] {
        c.with(|c| c.set(0));
    }
    {
        let cells = &BY_KIND;
        cells.with(|k| {
            for c in k {
                c.set(0);
            }
        });
    }
    for cells in [
        &BY_KIND_NS,
        &SLOW_BY_KIND,
        &SLOW_BY_KIND_NS,
        &REPLAY_BY_KIND_NS,
        &REPLAY_ASKED_BY_KIND,
        &REPLAY_RECORDED_BY_KIND,
        &REPLAY_VALIDATED_BY_KIND,
        &REPLAY_CHANGED_BY_KIND,
        &ABANDONED_BY_KIND,
    ] {
        cells.with(|k| {
            for c in k {
                c.set(0);
            }
        });
    }
    BY_YIELD.with(|y| {
        for c in y {
            c.set(0);
        }
    });
    BY_OP.with(|k| {
        for c in k {
            c.set(0);
        }
    });
}

/// Every counter family [`render`] reports, read together so no caller can
/// hand the renderer thirteen arrays in the wrong order.
pub struct Counters {
    pub snap: Snapshot,
    pub kinds: [u64; KINDS],
    pub kind_nanos: [u64; KINDS],
    pub slow_kinds: [u64; KINDS],
    pub slow_kind_nanos: [u64; KINDS],
    pub replay_kind_nanos: [u64; KINDS],
    pub replay_asked: [u64; KINDS],
    pub replay_recorded: [u64; KINDS],
    pub replay_validated: [u64; KINDS],
    pub replay_changed: [u64; KINDS],
    pub abandoned_kinds: [u64; KINDS],
    pub yields: [u64; 5],
    pub ops: [u64; crate::ir::OpKind::COUNT],
}

/// This thread's counters, read now.
#[must_use]
pub fn counters() -> Counters {
    Counters {
        snap: snapshot(),
        kinds: by_kind(),
        kind_nanos: by_kind_ns(),
        slow_kinds: slow_by_kind(),
        slow_kind_nanos: slow_by_kind_ns(),
        replay_kind_nanos: replay_by_kind_ns(),
        replay_asked: replay_asked_by_kind(),
        replay_recorded: replay_recorded_by_kind(),
        replay_validated: replay_validated_by_kind(),
        replay_changed: replay_changed_by_kind(),
        abandoned_kinds: abandoned_by_kind(),
        yields: by_yield(),
        ops: by_op(),
    }
}

/// The snapshot as one line of `key=value` pairs, which is what every gate
/// in `maintainers/ix` already parses.
///
/// Rendered here rather than in the embedder so the field names have one
/// spelling. A second formatter is how a dashboard and a gate come to
/// disagree about what `questions` means.
///
/// The `o.<OpKind>` fields are emitted only when non-zero, because there are
/// 45 of them and a build without `perf-ops` would otherwise spend most of
/// the line saying zero. A missing `o.` field therefore means either "this
/// op did not run" or "ops were not counted", and `ops_counted` is the field
/// that tells those apart. Read it first.
#[must_use]
pub fn render(counters: &Counters) -> String {
    let Counters {
        snap,
        kinds,
        kind_nanos,
        slow_kinds,
        slow_kind_nanos,
        replay_kind_nanos,
        replay_asked,
        replay_recorded,
        replay_validated,
        replay_changed,
        abandoned_kinds,
        yields,
        ops,
    } = counters;
    let mut out = format!(
        "compiles={} compile_ns={} compile_bytes={} compile_hits={} questions={} question_ns={} \
         collect_call_ns={} witness_rows={} witness_rows_deduped={} witness_rows_folded={} \
         witness_bytes={} aterms_stored={} aterms_reused={} \
         cache_witnesses_refused={} cache_objects_missing={} cache_objects_invalid={} memo_served={} \
         hashes={} hash_ns={} interns={} intern_misses={} konsts={} dir_hits={} copy_hits={} import_hits={} \
         realise_hits={} ops={} attr_name_interns={} ops_counted={} \
         subtreeMemoHits={} subtreeMemoMisses={} subtreeEntryForces={}",
        snap.compiles,
        snap.compile_ns,
        snap.compile_bytes,
        snap.compile_hits,
        snap.questions,
        snap.question_ns,
        snap.collect_call_ns,
        snap.witness_rows,
        snap.witness_rows_deduped,
        snap.witness_rows_folded,
        snap.witness_bytes,
        snap.aterms_stored,
        snap.aterms_reused,
        snap.cache_witnesses_refused,
        snap.cache_objects_missing,
        snap.cache_objects_invalid,
        snap.memo_served,
        snap.hashes,
        snap.hash_ns,
        snap.interns,
        snap.intern_misses,
        snap.konsts,
        snap.dir_hits,
        snap.copy_hits,
        snap.import_hits,
        snap.realise_hits,
        snap.ops,
        snap.attr_name_interns,
        snap.ops_counted,
        snap.import_cache_hits,
        snap.import_cache_misses,
        snap.import_entry_forces,
    );
    for (i, name) in crate::purity::QUESTION_KINDS.iter().enumerate() {
        if let Some(n) = kinds.get(i) {
            out.push_str(&format!(" q.{name}={n}"));
        }
        if let Some(n) = kind_nanos.get(i) {
            out.push_str(&format!(" q.{name}_ns={n}"));
        }
        if let Some(n) = abandoned_kinds.get(i) {
            out.push_str(&format!(" q.{name}_abandoned={n}"));
        }
        if SLOW_QUESTION_KINDS.contains(name) {
            if let Some(n) = slow_kinds.get(i) {
                out.push_str(&format!(" slow.{name}={n}"));
            }
            if let Some(n) = slow_kind_nanos.get(i) {
                out.push_str(&format!(" slow.{name}_ns={n}"));
            }
        }
        // Only when a replay happened: a fresh evaluation has no witness to
        // verify, and thirty zeros would say nothing.
        if let Some(n) = replay_kind_nanos.get(i)
            && *n != 0
        {
            out.push_str(&format!(" replay.{name}_ns={n}"));
        }
        if let Some(n) = replay_asked.get(i)
            && *n != 0
        {
            out.push_str(&format!(" replay.{name}_asked={n}"));
        }
        if let Some(n) = replay_recorded.get(i)
            && *n != 0
        {
            out.push_str(&format!(" replay.{name}_recorded={n}"));
        }
        if let Some(n) = replay_validated.get(i)
            && *n != 0
        {
            out.push_str(&format!(" replay.{name}_validated={n}"));
        }
        if let Some(n) = replay_changed.get(i)
            && *n != 0
        {
            out.push_str(&format!(" replay.{name}_changed={n}"));
        }
    }
    if snap.replay_validity_lines != 0 || snap.replay_validity_failed != 0 {
        out.push_str(&format!(
            " replay.validity_lines={} replay.validity_ns={} replay.validity_failed={} replay.validity_late={}",
            snap.replay_validity_lines,
            snap.replay_validity_ns,
            snap.replay_validity_failed,
            snap.replay_validity_late
        ));
    }
    if snap.replay_sealing_objects != 0 || snap.replay_sealing_failed != 0 {
        out.push_str(&format!(
            " replay.sealing_objects={} replay.sealing_ns={} replay.sealing_failed={}",
            snap.replay_sealing_objects, snap.replay_sealing_ns, snap.replay_sealing_failed
        ));
    }
    if snap.sweep_runs != 0 || snap.sweep_failed != 0 {
        out.push_str(&format!(
            " sweep.runs={} sweep.ns={} sweep.removed={} sweep.bytes_freed={} sweep.failed={}",
            snap.sweep_runs,
            snap.sweep_ns,
            snap.sweep_removed,
            snap.sweep_bytes_freed,
            snap.sweep_failed
        ));
    }
    out.push_str(&format!(
        " q.StorePath_unique={} q.WriteDrv_unique={} q.WriteDrv_skipped={} q.WriteDrv_deferred={} q.WriteDrv_flushes={} q.StoreFiltered_served={} q.Realise_validated={}",
        snap.store_path_unique,
        snap.write_drv_unique,
        snap.write_drv_skipped,
        snap.write_drv_deferred,
        snap.write_drv_flushes,
        snap.copy_memo_served,
        snap.realise_validated
    ));
    // Always rendered, zeros included: an evaluation that built nothing is
    // a real observation, and a reader dividing a peak footprint by these
    // must not have to wonder whether the field exists on this build.
    out.push_str(&format!(
        " alloc.slot_values={} alloc.slot_thunks={} alloc.slot_pending={} alloc.slot_refusals={} \
         alloc.attr_sets={} alloc.attr_entries={} alloc.lists={} alloc.list_elems={} \
         alloc.frames={} alloc.frame_slots={}",
        snap.slot_values,
        snap.slot_thunks,
        snap.slot_pending,
        snap.slot_refusals,
        snap.attr_sets,
        snap.attr_entries,
        snap.lists,
        snap.list_elems,
        snap.frames,
        snap.frame_slots
    ));
    for (i, name) in YIELD_KINDS.iter().enumerate() {
        if let Some(n) = yields.get(i) {
            out.push_str(&format!(" y.{name}={n}"));
        }
    }
    for (i, kind) in crate::ir::OpKind::ALL.iter().enumerate() {
        match ops.get(i) {
            Some(&n) if n > 0 => out.push_str(&format!(" o.{}={n}", kind.name())),
            _ => {}
        }
    }
    out
}

/// Whether `IXE_REPLAY_TRACE` is set: the per-question trace lines the slow
/// records are diagnosed with. Read once; the environment does not change
/// under a running evaluation.
pub fn replay_trace() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("IXE_REPLAY_TRACE").is_some())
}

/// Time `f`, returning its value and the nanoseconds it took.
///
/// One helper so every call site measures the same way. `Instant` rather
/// than a coarse clock because a host question can be microseconds when the
/// embedder answers from a cache.
pub fn timed<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let start = std::time::Instant::now();
    let value = f();
    (
        value,
        start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counting, reading and clearing, in that order. The reset half is
    /// what stops a second evaluation in one process from reporting the sum
    /// of both, which is a plausible-looking wrong number.
    ///
    /// Both halves of the `perf` feature are asserted, rather than the test
    /// being compiled away without it: with the counters out, these same
    /// calls must leave every counter at zero, and "the counters are
    /// compiled out" is a claim worth a test of its own. Branching on
    /// `cfg!` rather than gating the whole body keeps one test name across
    /// both configurations, which is what
    /// `the_op_counter_says_whether_it_was_compiled_in` beneath this already
    /// does; a `#[cfg]`-gated pair is two tests that can drift, and only one
    /// of them is ever the one somebody is looking at.
    ///
    /// ENG-13005: this asserted the counting half unconditionally and failed
    /// under `--no-default-features`, which nothing ran, because that
    /// configuration was only ever *built*.
    #[test]
    fn counters_accumulate_and_reset() {
        reset();
        note_compile(100, 7);
        note_compile(50, 3);
        note_question_asked(3);
        note_question_answered(3, 11);
        note_slow_question_started(3);
        note_slow_question_answered(3, 13);
        note_question_abandoned(4);
        note_collect_call(7);
        note_unique_store_path();
        note_unique_write_drv();
        note_drv_write_skipped();
        note_drv_write_deferred();
        note_drv_write_deferred();
        note_drv_write_flush(2);
        note_copy_memo_served();
        note_copy_memo_served();
        note_copy_memo_served();
        note_witness(7, 11, 5, 13);
        note_aterms(2, 3);
        note_sweep(5, 3, 100);
        note_sweep_failed(7);
        note_cache_corruption(CacheCorruption::WitnessRefused);
        note_cache_corruption(CacheCorruption::ObjectMissing);
        note_cache_corruption(CacheCorruption::ObjectInvalid);
        note_intern();
        note_intern();
        note_intern_miss();
        note_konst();
        note_dir_hit();
        note_copy_hit();
        note_copy_hit();
        note_import_hit();
        note_import_cache_hit();
        note_import_cache_miss();
        note_import_entry_force();
        note_import_entry_force();
        note_realise_hit();
        note_realise_hit();
        note_realise_hit();
        note_hash(5);
        note_slot_value();
        note_slot_value();
        note_slot_thunk();
        note_slot_pending();
        note_slot_refusal();
        note_attrs(3);
        note_attrs(0);
        note_list(4);
        note_frame(2);
        note_frame(0);
        note_frame_filled(5);

        let s = snapshot();
        // Without `perf` every one of these is a no-op, so the accumulate
        // half of the test has nothing to assert and only the reset half
        // still means anything. Asserting the counts unconditionally is why
        // the `--no-default-features` arm of the suite was red.
        if cfg!(feature = "perf") {
            assert_eq!((s.compiles, s.compile_bytes, s.compile_ns), (2, 150, 10));
            assert_eq!((s.questions, s.question_ns, s.collect_call_ns), (1, 11, 7));
            assert_eq!((s.store_path_unique, s.write_drv_unique), (1, 1));
            assert_eq!(
                (
                    s.write_drv_skipped,
                    s.write_drv_deferred,
                    s.write_drv_flushes
                ),
                (1, 2, 1)
            );
            assert_eq!(s.copy_memo_served, 3);
            assert_eq!(
                (
                    s.witness_rows,
                    s.witness_rows_deduped,
                    s.witness_rows_folded,
                    s.witness_bytes,
                    s.aterms_stored,
                    s.aterms_reused,
                ),
                (7, 11, 5, 13, 2, 3)
            );
            assert_eq!(
                (
                    s.sweep_runs,
                    s.sweep_ns,
                    s.sweep_removed,
                    s.sweep_bytes_freed,
                    s.sweep_failed
                ),
                (1, 12, 3, 100, 1)
            );
            assert_eq!(
                (
                    s.cache_witnesses_refused,
                    s.cache_objects_missing,
                    s.cache_objects_invalid,
                ),
                (1, 1, 1)
            );
            assert_eq!(
                (s.interns, s.intern_misses, s.konsts, s.hashes, s.hash_ns),
                (2, 1, 1, 1, 5)
            );
            assert_eq!(
                (
                    s.slot_values,
                    s.slot_thunks,
                    s.slot_pending,
                    s.slot_refusals
                ),
                (2, 1, 1, 1)
            );
            assert_eq!((s.attr_sets, s.attr_entries), (2, 3));
            assert_eq!((s.lists, s.list_elems), (1, 4));
            assert_eq!((s.frames, s.frame_slots), (2, 7));
            assert_eq!(
                (s.dir_hits, s.copy_hits, s.import_hits, s.realise_hits),
                (1, 2, 1, 3)
            );
            assert_eq!(
                (
                    s.import_cache_hits,
                    s.import_cache_misses,
                    s.import_entry_forces
                ),
                (1, 1, 2)
            );
            assert_eq!(by_kind().get(3), Some(&1));
            assert_eq!(by_kind_ns().get(3), Some(&11));
            assert_eq!(by_kind_ns().iter().sum::<u64>(), s.question_ns);
            assert_eq!(slow_by_kind().get(3), Some(&1));
            assert_eq!(slow_by_kind_ns().get(3), Some(&13));
            assert_eq!(abandoned_by_kind().get(4), Some(&1));
        } else {
            // `ops_counted` and not a bare `Snapshot::default()`: with
            // `perf-ops` on and `perf` off -- a legal configuration --
            // `snapshot()` reports `ops_counted: true`, and comparing against
            // the plain default fails there for a reason that has nothing to
            // do with what this test is about.
            assert_eq!(
                s,
                Snapshot {
                    ops_counted: cfg!(feature = "perf-ops"),
                    ..Snapshot::default()
                },
                "without `perf` the counters are compiled out, so note_* \
                 calls must leave the snapshot at its default"
            );
            assert_eq!(by_kind(), [0; KINDS]);
            assert_eq!(by_kind_ns(), [0; KINDS]);
            assert_eq!(slow_by_kind(), [0; KINDS]);
            assert_eq!(slow_by_kind_ns(), [0; KINDS]);
            assert_eq!(abandoned_by_kind(), [0; KINDS]);
        }

        // The reset half is asserted in both configurations: clearing
        // counters that are already zero has to stay a no-op rather than,
        // say, resizing an array.
        reset();
        let s = snapshot();
        assert_eq!(
            s,
            Snapshot {
                ops_counted: cfg!(feature = "perf-ops"),
                ..Snapshot::default()
            }
        );
        assert_eq!(by_kind(), [0; KINDS]);
        assert_eq!(by_kind_ns(), [0; KINDS]);
        assert_eq!(slow_by_kind(), [0; KINDS]);
        assert_eq!(slow_by_kind_ns(), [0; KINDS]);
        assert_eq!(abandoned_by_kind(), [0; KINDS]);
        assert_eq!(by_yield(), [0; 5]);
    }

    /// The yield array and the name list are indexed together, the same
    /// coupling `the_kind_array_is_as_wide_as_the_kind_list` guards for
    /// questions. A `Yield` variant added without widening both would land
    /// in no bucket and shorten the rendered line by one.
    ///
    /// The width assertion and `by_yield()`'s own length are what this test
    /// is named for, and they run in both feature configurations; only the
    /// count of what was recorded depends on `perf`. ENG-13005.
    #[test]
    fn the_yield_array_is_as_wide_as_the_yield_list() {
        assert_eq!(YIELD_KINDS.len(), 5);
        assert_eq!(
            by_yield().len(),
            YIELD_KINDS.len(),
            "array and name list disagree"
        );
        reset();
        for i in 0..YIELD_KINDS.len() {
            note_yield(i);
        }
        // The width check above holds either way; only the counting below
        // needs `perf`.
        let expected = if cfg!(feature = "perf") {
            YIELD_KINDS.len() as u64
        } else {
            0
        };
        assert_eq!(by_yield().iter().sum::<u64>(), expected);
    }

    /// `ops` is zero without the feature, and `ops_counted` is what tells a
    /// reader that the zero means "not compiled in" rather than "no ops
    /// ran". A zero whose meaning is ambiguous is exactly the shape this
    /// crate keeps getting wrong.
    #[test]
    fn the_op_counter_says_whether_it_was_compiled_in() {
        reset();
        note_op(&crate::ir::Op::Ret);
        let s = snapshot();
        assert_eq!(s.ops_counted, cfg!(feature = "perf-ops"));
        if s.ops_counted {
            assert_eq!(s.ops, 1);
        } else {
            // Both `perf-ops` counters, not just `ops`: this flag is the
            // only thing that makes any of their zeros legible, so a counter
            // that escaped the feature would read as a real zero forever.
            assert_eq!(
                (s.ops, s.attr_name_interns),
                (0, 0),
                "without the feature nothing may be counted"
            );
        }
    }

    /// The per-kind counts must file under the op that ran, and their sum
    /// must be the aggregate. A per-kind array that drifts from `ops` turns
    /// a decomposition into a set of numbers that do not add up, which is
    /// worse than not having one.
    #[test]
    fn ops_are_counted_under_their_own_kind() {
        reset();
        note_op(&crate::ir::Op::Apply);
        note_op(&crate::ir::Op::Apply);
        note_op(&crate::ir::Op::Select { sym: 7 });
        let by = by_op();
        let total = snapshot().ops;
        assert_eq!(
            by.iter().sum::<u64>(),
            total,
            "per-kind counts do not sum to ops"
        );
        if !snapshot().ops_counted {
            assert_eq!(total, 0, "without the feature nothing may be counted");
            return;
        }
        let count = |k: crate::ir::OpKind| by.get(k as usize).copied();
        assert_eq!(total, 3);
        assert_eq!(count(crate::ir::OpKind::Apply), Some(2));
        assert_eq!(count(crate::ir::OpKind::Select), Some(1));
        assert_eq!(count(crate::ir::OpKind::Ret), Some(0));
    }

    /// An op that ran is named on the line; one that did not is absent
    /// rather than zero, and `ops_counted` is what makes the absence legible.
    #[test]
    fn only_ops_that_ran_are_rendered() {
        reset();
        note_op(&crate::ir::Op::Apply);
        let line = render(&counters());
        assert!(
            !line.contains("o.Ret="),
            "an unexecuted op is named in {line}"
        );
        assert!(!line.contains(" sweep."), "no sweep ran, yet {line}");
        for field in [
            "cache_witnesses_refused=",
            "cache_objects_missing=",
            "cache_objects_invalid=",
            "memo_served=",
            "subtreeMemoHits=",
            "subtreeMemoMisses=",
            "subtreeEntryForces=",
        ] {
            assert!(line.contains(field), "{field} missing from {line}");
        }
        if snapshot().ops_counted {
            assert!(line.contains("o.Apply=1"), "Apply missing from {line}");
            assert!(line.contains("ops_counted=true"), "{line}");
        } else {
            assert!(
                !line.contains("o.Apply="),
                "counted without the feature: {line}"
            );
            assert!(line.contains("ops_counted=false"), "{line}");
        }
    }

    /// A sweep that ran, or failed, is rendered with all five fields, so a
    /// reader of the perf line sees what the cap cost and whether enforcing
    /// it worked.
    #[test]
    fn a_sweep_that_ran_or_failed_is_rendered() {
        reset();
        note_sweep_failed(9);
        let line = render(&counters());
        if cfg!(feature = "perf") {
            assert!(
                line.contains(
                    " sweep.runs=0 sweep.ns=9 sweep.removed=0 sweep.bytes_freed=0 sweep.failed=1"
                ),
                "{line}"
            );
        } else {
            assert!(
                !line.contains(" sweep."),
                "counted without the feature: {line}"
            );
        }
    }

    /// Every kind gets a named field, so a reader grepping for `q.Entries`
    /// finds it whether or not the run asked one.
    #[test]
    fn every_question_kind_is_rendered_even_at_zero() {
        reset();
        let line = render(&counters());
        for name in crate::purity::QUESTION_KINDS {
            assert!(
                line.contains(&format!("q.{name}=")),
                "{name} missing from {line}"
            );
            assert!(
                line.contains(&format!("q.{name}_ns=")),
                "{name} latency missing from {line}"
            );
            assert!(
                line.contains(&format!("q.{name}_abandoned=")),
                "{name} abandoned count missing from {line}"
            );
        }
        for name in SLOW_QUESTION_KINDS {
            assert!(line.contains(&format!("slow.{name}=0")), "{line}");
            assert!(line.contains(&format!("slow.{name}_ns=0")), "{line}");
        }
        assert!(line.contains("q.StorePath_unique=0"), "{line}");
        assert!(line.contains("q.WriteDrv_unique=0"), "{line}");
        assert!(line.contains("q.WriteDrv_skipped=0"), "{line}");
        assert!(line.contains("q.WriteDrv_deferred=0"), "{line}");
        assert!(line.contains("q.WriteDrv_flushes=0"), "{line}");
        assert!(line.contains("q.StoreFiltered_served=0"), "{line}");
        assert!(line.contains("witness_rows=0"), "{line}");
        assert!(line.contains("witness_rows_deduped=0"), "{line}");
        assert!(line.contains("witness_rows_folded=0"), "{line}");
        assert!(line.contains("witness_bytes=0"), "{line}");
        assert!(line.contains("aterms_stored=0"), "{line}");
        assert!(line.contains("aterms_reused=0"), "{line}");
        for field in [
            "slot_values",
            "slot_thunks",
            "slot_pending",
            "slot_refusals",
            "attr_sets",
            "attr_entries",
            "lists",
            "list_elems",
            "frames",
            "frame_slots",
        ] {
            assert!(
                line.contains(&format!(" alloc.{field}=0")),
                "alloc.{field} missing from {line}"
            );
        }
        for name in YIELD_KINDS {
            assert!(
                line.contains(&format!("y.{name}=")),
                "{name} missing from {line}"
            );
        }
    }
}
