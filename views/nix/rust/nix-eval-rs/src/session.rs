//! One evaluation, with whatever caches the caller has.
//!
//! This is the path both embedders take: the C ABI in [`crate::capi`], which
//! runs one expression per process, and the persistent server example, which
//! runs many in one. They differ only in how long the caches live, so the work
//! itself lives here rather than being written twice and drifting.
//!
//! # What a cached evaluation is allowed to skip
//!
//! Compilation is skipped when the compile cache has the module. Evaluation is
//! skipped when the result cache has an answer whose recorded questions still
//! give the recorded answers. Nothing else is skipped: the source is re-read
//! by the caller on every request, because both caches are keyed on it.

use crate::compile::CompileError;
use crate::compile::Origin;
use crate::eval::EvalError;
use crate::host::Host;
use crate::modcache::CacheError;
use crate::readset::{Complaint, EvalId, EvalResult, RecordingHost, ResultCache};
use crate::refusal::{Refusal, RefusalToken};
use crate::value2::Value;
use crate::vm::{ErrKind, Vm};
use ix_kernel::cas::Cas;

/// What one evaluation did, beyond producing an answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reuse {
    /// The module was already compiled.
    pub compile_hit: bool,
    /// The answer came from the result cache; the VM did not run.
    pub memo_hit: bool,
}

/// The status names an [`EvalResult`] carries.
///
/// The class is part of the memoised answer, not just the message, because an
/// embedder reports a throw and an assertion failure as different exceptions
/// and cannot recover the difference from the text. Memoising only the message
/// would turn a cached `throw` into a generic evaluation error the first time
/// it was served from cache, which is a wrong answer that only appears on the
/// second run.
pub const OK: &str = "ok";
pub const UNIMPLEMENTED: &str = "unimplemented";
pub const PARSE: &str = "parse";
pub const EVAL: &str = "eval";
pub const THROWN: &str = "thrown";
pub const ASSERTION: &str = "assertion";
pub const IMPORT_FROM_DERIVATION: &str = "import-from-derivation";
pub const MISSING_ARGUMENT: &str = "missing-argument";

#[must_use]
pub fn result_of(error: &EvalError) -> EvalResult {
    let (status, value) = match error {
        EvalError::Unimplemented(refusal) => {
            return EvalResult {
                status: UNIMPLEMENTED.to_owned(),
                value: refusal.detail.clone(),
                emissions: Vec::new(),
                token: Some(refusal.token),
                pos: None,
            };
        }
        EvalError::Parse(message) => (PARSE, message.clone()),
        EvalError::Eval(ErrKind::Eval, message, _) => (EVAL, message.clone()),
        EvalError::Eval(ErrKind::Thrown, message, _) => (THROWN, message.clone()),
        EvalError::Eval(ErrKind::Assertion, message, _) => (ASSERTION, message.clone()),
        EvalError::Eval(ErrKind::ImportFromDerivation, message, _) => {
            (IMPORT_FROM_DERIVATION, message.clone())
        }
        EvalError::Eval(ErrKind::MissingArgument, message, _) => {
            (MISSING_ARGUMENT, message.clone())
        }
    };
    EvalResult {
        status: status.to_owned(),
        value,
        emissions: Vec::new(),
        token: None,
        // The position travels with the message for the same reason the class
        // does: an embedder renders `at file:line:col` from it, and a result
        // served from the memo table that dropped it would print a shorter
        // error on the second run than on the first -- `eval-cache-dir`
        // changing what the evaluator says.
        pos: error.pos().cloned(),
    }
}

/// Turn a memoised result back into the error an embedder should raise.
///
/// The inverse of [`result_of`], so a served answer raises the same exception
/// class the original evaluation did.
#[must_use]
pub fn error_of(result: &EvalResult) -> Option<EvalError> {
    let message = result.value.clone();
    let pos = result.pos.clone();
    match result.status.as_str() {
        OK => None,
        // `decode_result` has already turned a tokenless row into
        // `Unrecorded`, so there is nothing to guess at here.
        UNIMPLEMENTED => Some(EvalError::Unimplemented(Refusal::new(
            result.token.unwrap_or(RefusalToken::Unrecorded),
            message,
        ))),
        PARSE => Some(EvalError::Parse(message)),
        THROWN => Some(EvalError::Eval(ErrKind::Thrown, message, pos)),
        ASSERTION => Some(EvalError::Eval(ErrKind::Assertion, message, pos)),
        IMPORT_FROM_DERIVATION => {
            Some(EvalError::Eval(ErrKind::ImportFromDerivation, message, pos))
        }
        MISSING_ARGUMENT => Some(EvalError::Eval(ErrKind::MissingArgument, message, pos)),
        // Anything unrecognised is treated as a plain evaluation error rather
        // than trusted: a status this build does not know is a store written
        // by a different one.
        _ => Some(EvalError::Eval(ErrKind::Eval, message, pos)),
    }
}

/// Compile and evaluate one source.
///
/// The compile cache is the machine's own ([`Vm::modules`]), the one its
/// imports go through, so a top-level program and the files it imports are
/// cached alike. `results` is optional because result memoisation is a
/// separate opt-in from compilation caching: the first is only sound with a
/// recorded read set, and a caller that does not want the recording overhead
/// can have the compile cache alone.
pub fn evaluate(
    vm: &mut Vm,
    results: Option<&mut ResultCache<'_, dyn Cas>>,
    host: &dyn Host,
    source: &str,
    base_dir: &str,
    origin: Origin<'_>,
) -> (EvalResult, Reuse) {
    let mut reuse = Reuse::default();

    let before = vm.modules().hits();
    let compiled = match vm.compile(source, base_dir, origin) {
        Ok(compiled) => compiled,
        Err(error) => return (compile_failure(&error), reuse),
    };
    reuse.compile_hit = vm.modules().hits() > before;
    let module_id = *compiled.id.hash();

    let Some(results) = results else {
        return (run(vm, &compiled.module, host), reuse);
    };

    // The module says what the source is, the settings say what the process
    // was configured to do with it, and the question says which bytes the
    // caller wants back. All three decide the answer, so all three are in the
    // key (ENG-12541, ENG-12830). This caller renders the whole expression
    // with the plain printer and has no other shape available, which is
    // exactly why it was the only caller a `(module, settings)` key could
    // serve.
    let identity = EvalId::of(
        &module_id,
        vm.settings(),
        // This caller applies nothing: it compiles one source and renders
        // what it evaluates to. The axis exists for the handle path, whose
        // flake evaluand is `call-flake.nix` applied to three values.
        &Arguments::none(),
        &Question::Whole {
            render: RenderMode::Plain,
        },
    );

    let verifying = match serve(results, &identity, host, vm.settings()) {
        Served::Answer(result) => {
            reuse.memo_hit = true;
            return (result, reuse);
        }
        Served::Evaluate { verifying } => verifying,
    };
    reuse.memo_hit = verifying.is_some();

    // Quiet while verifying: this run is a check of an answer the cache
    // already gave, and `settle` replays that answer's emissions, so a reader
    // must not also see this copy.
    let recorder = if verifying.is_some() {
        RecordingHost::quiet(host)
    } else {
        RecordingHost::new(host)
    };
    let previous_import_cache = vm.set_import_cache_enabled(verifying.is_none());
    let mut result = run(vm, &compiled.module, &recorder);
    vm.set_import_cache_enabled(previous_import_cache);
    let read_set = recorder.take();
    result.emissions = recorder.take_emissions();
    settle(
        results,
        &identity,
        host,
        vm.settings(),
        &read_set,
        &result,
        verifying.as_ref(),
        vm.interrupted(),
    );
    match verifying {
        // The served answer is what the caller gets, even when the check
        // disagreed with it. Not because it is the more trustworthy of the
        // two -- it is the less -- but because it is what every unsampled run
        // of the same expression is getting, and a command whose answer
        // depended on whether the sampler picked it would be the harder bug.
        // The disagreement is an error-priority complaint, which is the part
        // that must not be missed.
        Some(served) => (served, reuse),
        None => (result, reuse),
    }
}

/// What [`serve`] decided.
pub enum Served {
    /// The cache answered, and its emissions have been replayed. The caller
    /// has nothing to evaluate.
    Answer(EvalResult),
    /// The caller must evaluate, then call [`settle`].
    ///
    /// `verifying` carries the answer the cache gave when this occasion is
    /// one of the sampled checks of it, and is `None` on a plain miss.
    Evaluate { verifying: Option<EvalResult> },
}

/// Ask the cache last time's questions again, and decide whether the caller
/// still has to evaluate.
///
/// One rule for two callers: [`evaluate`], whose evaluation is [`run`], and
/// the handle path, whose evaluation is an embedder walking a value through
/// [`QuestionCache`]. Written out here rather than inline in the first of
/// them because the second one used not to exist, and the reason it did not
/// is that there was nothing to reuse -- so `nix eval` and `nix build` ran
/// with `eval-cache-dir` set and never read a row for the life of the setting
/// (ENG-12830).
///
/// A sampled hit comes back as `Evaluate`, not as an answer: verifying a
/// cache means doing the work anyway, and the comparison happens in
/// [`settle`] once the caller has produced something to compare against.
pub fn serve(
    results: &mut ResultCache<'_, dyn Cas>,
    identity: &EvalId,
    host: &dyn Host,
    settings: &crate::eval::Settings,
) -> Served {
    // Ask last time's questions again. The key is built from the answers
    // given now, so a hit means some past evaluation asked exactly this and
    // got exactly this.
    let Some(result) = results.lookup(identity, host, settings) else {
        results.note_miss(identity);
        return Served::Evaluate { verifying: None };
    };
    // Sampled verification: evaluate anyway and compare. A cache is the one
    // component that cannot be checked by looking at its output, because its
    // output is by construction what it was told to say.
    //
    // This side sees wrong answers and nothing else. It cannot see a cache
    // that stopped serving, because it only runs when a hit happened -- and a
    // cache that never hits never reaches this line at all. That is not a
    // hypothetical gap: both of this repo's real cache bugs served no wrong
    // answer, and a verifier built only from this half would have reported
    // perfect health through both. The miss side in [`settle`] covers it, and
    // the sweep post-condition in `Store::sweep` covers what neither can. Do
    // not read a clean `hits_disagreed` as a healthy cache.
    if results.should_verify() {
        return Served::Evaluate {
            verifying: Some(result),
        };
    }
    // The value came back from disk; everything the evaluation said out loud
    // has to be said again. A run served from cache that stayed quiet would
    // tell its reader less than the run that filled the cache did, and the
    // difference would be `eval-cache-dir`.
    for emission in &result.emissions {
        emission.replay(host);
    }
    Served::Answer(result)
}

/// Record what the caller produced, or compare it against what was served.
///
/// The other half of [`serve`]. `verifying` is what that returned for a
/// sampled hit; when it is set nothing is recorded, because the row is
/// already there and this run existed only to check it.
// Eight because the settings joined the seven that were already here: an
// evaluation's identity, its world, its answer and the answer it was checking
// against are all separate facts, and bundling them into a struct nobody else
// constructs would hide that rather than simplify it.
#[allow(clippy::too_many_arguments)]
pub fn settle(
    results: &mut ResultCache<'_, dyn Cas>,
    identity: &EvalId,
    host: &dyn Host,
    settings: &crate::eval::Settings,
    read_set: &crate::readset::ReadSet,
    result: &EvalResult,
    verifying: Option<&EvalResult>,
    interrupted: bool,
) {
    if let Some(served) = verifying {
        if served == result {
            results.note_hit_verified();
        } else {
            results.note_hit_disagreed(
                identity,
                &format!("{}/{}", served.status, served.value),
                &format!("{}/{}", result.status, result.value),
            );
        }
        // The check ran quiet, so the served answer's emissions are what the
        // reader gets. Replayed here and not in `serve`, because `serve` did
        // not know yet whether this occasion was a check.
        for emission in &served.emissions {
            emission.replay(host);
        }
        return;
    }
    // An interrupt is the operator's, not the expression's, so it is never
    // stored. Without this one Ctrl-C with `eval-cache-dir` set made that
    // expression answer "interrupted by the user" for ever: the interrupt
    // arrives as an ordinary evaluation error carrying cppnix's wording, and
    // the recorder could not tell it from an expression that genuinely fails
    // (ENG-12540). Asked of the VM rather than matched against the message,
    // because a message comparison would break the day somebody rewords it
    // and would break silently, in the direction of caching the interrupt.
    if interrupted {
        return;
    }
    // A result that could not be recorded is a slower next run, not a wrong
    // answer, so the failure is reported through the corruption channel the
    // caller already drains rather than replacing the answer.
    if let Err(error) = results.record(identity, read_set, result, host, settings) {
        results.note_record_failure(format!("could not memoise: {error}"));
        return;
    }

    // The other half of the verifier, and the half the ticket did not ask
    // for. A cache can fail in two directions: it can serve a wrong answer,
    // which the check above catches, and it can silently stop serving at all,
    // which that check cannot see -- a miss looks exactly like a cold cache.
    // Both of this repo's real cache bugs were the second kind: a witness
    // decoder that rejected its own encoder's tag, and a sweep that deleted
    // every witness (ENG-12601). Neither produced a single wrong answer.
    //
    // So a sampled record is looked up again immediately. It must hit: the
    // questions were just recorded, the world has not moved, and the key is
    // computed the same way. A miss here means the record and lookup paths
    // disagree about the key, or the witness cannot be read back.
    //
    // What this does NOT catch, said plainly: anything that destroys the
    // store after this process ends. The witness is still on disk while we
    // are looking, so ENG-12601 passes this check. That class needs a
    // post-condition on the sweep, which `Store::sweep` now carries.
    if results.should_verify() {
        let replayable = results.lookup(identity, host, settings).is_some();
        results.note_record_replayable(replayable, identity);
    }
}

pub(crate) fn compile_failure(error: &CacheError) -> EvalResult {
    // A compile that failed because the *source* is bad is the embedder's
    // business and keeps its class; one that failed because the store is
    // broken is not the expression's fault and is reported as an evaluation
    // error rather than blamed on the user's syntax.
    //
    // The four source-level classes map exactly as `From<CompileError> for
    // EvalError` maps them, because the same expression must be reported the
    // same way whether or not the caller configured a cache. That mapping is
    // exhaustive over `CompileError`; keeping this one exhaustive too is what
    // makes "exactly" checkable rather than aspirational.
    match error {
        // Destructured variant by variant, with no catch-all inside this
        // arm, so a new `CompileError` does not compile until somebody
        // decides how the cached path reports it. The catch-all that used to
        // stand here read as harmless and was not: `CompileError::Eval` --
        // the class for a source cppnix's parser rejects with a plain error
        // rather than a parse error -- fell through it to
        // `CacheError::Display`, which formats the compile error with `{:?}`.
        // So `1 |> 2` with pipe-operators off answered
        // `Eval("experimental Nix feature ... is disabled")` with a cache
        // configured and the bare message without one, and `~/x` under
        // pure-eval did the same. Rust debug syntax in a user-facing message,
        // reachable only by setting `eval-cache-dir`, which is exactly the
        // shape `maintainers/ix/cache-semantics-gate.sh` exists to refuse: it
        // failed arms 1 and 4 on three corpus files under all seven
        // configurations.
        CacheError::Compile(compile) => match compile {
            CompileError::Parse(message) => EvalResult {
                status: PARSE.to_owned(),
                value: message.clone(),
                emissions: Vec::new(),
                token: None,
                pos: None,
            },
            CompileError::Unimplemented(refusal) => EvalResult {
                status: UNIMPLEMENTED.to_owned(),
                value: refusal.detail.clone(),
                emissions: Vec::new(),
                token: Some(refusal.token),
                pos: None,
            },
            CompileError::UndefinedVariable(name) => EvalResult {
                status: EVAL.to_owned(),
                value: format!("undefined variable '{name}'"),
                emissions: Vec::new(),
                token: None,
                pos: None,
            },
            // The bare message, matching `From<CompileError> for EvalError`,
            // which maps this to `EvalError::Eval(ErrKind::Eval, m, None)`.
            CompileError::Eval(message) => EvalResult {
                status: EVAL.to_owned(),
                value: message.clone(),
                emissions: Vec::new(),
                token: None,
                pos: None,
            },
        },
        // The store or the table failed, which is not the expression's fault
        // and has no uncached counterpart to match: there is no such failure
        // when there is no cache. Reported as an evaluation error rather than
        // blamed on the user's syntax.
        CacheError::Kernel(_) | CacheError::Dangling { .. } => EvalResult {
            status: EVAL.to_owned(),
            value: error.to_string(),
            emissions: Vec::new(),
            token: None,
            pos: None,
        },
    }
}

fn run(vm: &mut Vm, module: &std::rc::Rc<crate::ir::Module>, host: &dyn Host) -> EvalResult {
    // One memo across the evaluation and the render of it: the printer forces
    // too, and both answer into the one recording the caller holds.
    let mut memo = crate::eval::JobMemo::default();
    let rendered = run_to_value_with(vm, module, host, &mut memo)
        .and_then(|value| render_with(vm, host, value, RenderMode::Plain, &mut memo))
        .and_then(|bytes| {
            // The serve row's answer is text today (the cache encodes it as
            // a string); a non-UTF-8 rendering refuses by name here rather
            // than being repaired into a wrong row (ENG-13147).
            crate::primops_pure::text_of_bytes(&bytes)
                .map(str::to_owned)
                .map_err(crate::eval::map_vm_error)
        });
    match rendered {
        Ok(text) => EvalResult {
            status: OK.to_owned(),
            value: text,
            emissions: Vec::new(),
            token: None,
            pos: None,
        },
        Err(error) => result_of(&error),
    }
}

/// Run a compiled module and stop at its value, in weak head normal form.
///
/// The one place a user's expression starts the VM, through
/// [`run_to_value_with`]: [`run`] renders what it returns, the handle API's
/// questions (`capi`) hand it to the embedder as handles, and
/// [`evaluate_value`] as a live value. One loop for all of them.
pub(crate) fn run_to_value(
    vm: &mut Vm,
    module: &std::rc::Rc<crate::ir::Module>,
    host: &dyn Host,
) -> Result<Value, EvalError> {
    let mut memo = crate::eval::JobMemo::default();
    run_to_value_with(vm, module, host, &mut memo)
}

/// [`run_to_value`] with the evaluation's memo lent by the caller, for a
/// caller holding one recording across several drives
/// ([`crate::eval::drive_with`] says who).
pub(crate) fn run_to_value_with(
    vm: &mut Vm,
    module: &std::rc::Rc<crate::ir::Module>,
    host: &dyn Host,
    memo: &mut crate::eval::JobMemo,
) -> Result<Value, EvalError> {
    // The ceiling is applied here, at the one point a user's expression
    // starts, and deliberately not where each embedder builds its VM.
    //
    // Two reasons it belongs here rather than in a configuring constructor.
    // The embedders construct at different times and one of them does it
    // before the bridge has set anything -- `ixe_session_new` runs from a C++
    // constructor -- so a ceiling read at construction can be the default
    // when the real one arrives a moment later. And an entry point that
    // forgets a configuring constructor compiles and runs unbounded, which is
    // what `session::evaluate_once` did: `--max-call-depth 50` bounded a run
    // without `eval-cache-dir` and did not bound it with one (ENG-12540).
    //
    // Applying it in both places was the first fix, and it was worse than
    // either alone: removing it from here still passed
    // `maintainers/ix/cache-semantics-gate.sh`, because the other copy
    // covered the arm under test. One place means a break test can see it.
    // The ceiling is already in `vm.settings()`, taken when the VM was built
    // (ENG-12939); re-reading the global here is what let the two disagree.
    // Per run, not per machine: this VM may have been interrupted on an
    // earlier evaluation and is about to be asked for another.
    vm.clear_interrupted();
    vm.start_module(module);
    crate::eval::drive_with(vm, host, memo).map_err(crate::eval::map_vm_error)
}

/// How a value is turned into the bytes a command prints.
///
/// One renderer per mode, all of them on this side of the C ABI, because they
/// already exist here and are compared against cppnix by the corpus:
/// `Plain` is the printer every `eval-okay` file is diffed through, `Json` is
/// `builtins.toJSON`'s walker (the same `__toString`/`outPath` rules
/// `printValueAsJSON` applies), and `Raw` is cppnix's `coerceToString` with
/// `coerceMore = false`, which is what `nix eval --raw` passes. Rendering
/// from C++ instead would have meant a second implementation of each, since
/// cppnix's own printers take a cppnix `Value` and there is no such thing
/// here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
    /// `nix-instantiate --eval --strict`: cppnix's `printAmbiguous`.
    Plain,
    /// `nix-instantiate --eval` without `--strict`: cppnix prints the value
    /// forced to weak head normal form and writes `<CODE>` for every child
    /// still a thunk. Which children are thunks is evaluator-internal, so
    /// this backend serves only the values that have no children -- a
    /// string, a number, a Boolean, null, a path -- for which lazy and
    /// strict printing are one answer, and refuses the rest by name.
    PlainLazy,
    /// `nix eval` with no output flag: cppnix's `ValuePrinter`. A different
    /// function from `printAmbiguous` and not everywhere the same one, so it
    /// is a different mode rather than the same one reused. See
    /// [`crate::print::Print::value_printer`] for what the two disagree about
    /// and what this refuses because of it.
    ValuePrinter,
    /// `nix eval --json`. Compact; the embedder re-dumps it if it wants
    /// `--pretty`, so the two spellings cannot drift.
    Json,
    /// `nix eval --raw`.
    Raw,
    /// `nix-instantiate --eval --strict --xml --no-location`:
    /// `builtins.toXML`'s walker, which is the same `printValueAsXML` cppnix
    /// calls for both (`primops.cc`, `prim_toXML`; `nix-instantiate.cc`)
    /// once `--no-location` turns the source positions off. With locations
    /// on the documents differ, so the bridge refuses that spelling rather
    /// than serving the other one.
    Xml,
}

/// The whole of what a caller wants, beyond the source text.
///
/// Part of the memo key with the module and the settings, because two
/// commands can compile the same source and want entirely different bytes out
/// of it: `nix eval -f x.nix a.b --json` and `nix eval --raw -f x.nix` share a
/// module and share nothing else.
///
/// This type is the fix for ENG-12830, and it is worth saying what the
/// obstacle actually was. [`evaluate`] could memoise because its answer is a
/// rendered string; [`evaluate_value`] could not, because its answer is a
/// live value and a live value is not an answer until somebody has said which
/// part of it and in what shape. That was read as "a handle walk does not
/// have the whole question up front" (ENG-12470), which is true of the handle
/// *table* and false of every *command* that uses one: `rustEvalRender` and
/// `rustEvalDerivations` each know all of it before they open a session. The
/// key was being built one layer below the layer that knew the question, so
/// it could only ever serve the one caller whose question never varies.
///
/// Kept here rather than in the C++ bridge on purpose. A field the bridge
/// forgot to put in the key would not fail; it would serve the first caller's
/// answer to the second, which is indistinguishable from a right answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Question {
    /// Render the whole expression. `nix-instantiate --eval`, which has no
    /// selection and one output shape.
    Whole { render: RenderMode },
    /// Perform `selection`, then render what it reached. `nix eval`.
    Select {
        selection: Selection,
        render: RenderMode,
    },
    /// Perform `selection`, then report the derivation there. `nix build`,
    /// which wants a drvPath and an output set rather than printable bytes.
    Derivation { selection: Selection },
    /// Walk EVERY attribute path of `selection` -- a list to visit, not a
    /// ladder of candidates -- and report every derivation reachable from
    /// each the way cppnix's `getDerivations` reaches them. `nix-build`,
    /// whose `-A a -A b` builds both from one evaluation of the root. A
    /// different question from [`Question::Derivation`] because the same
    /// selection answers differently: one derivation's outputs there, the
    /// closure of a set walk here.
    DerivationSet { selection: Selection },
    /// Perform `selection`, then report only the derivation path.
    DerivationPath { selection: Selection },
    /// Perform `selection`, then report the executable and its dependencies.
    Application { selection: Selection },
    /// Select a package and read its authoritative `meta.position` location.
    SourcePosition { selection: Selection },
    /// Unfiltered package metadata. Filtering and presentation do not change evaluation.
    SearchPackages {
        selection: Selection,
        scope: SearchScope,
    },
    /// Strict validation with independently captured Hydra/regular IFD policy.
    FlakeCheck {
        selection: Selection,
        options: crate::flake_check::Options,
    },
    /// Lazily classify a flake's output tree. `flags` carries the two show
    /// options, which affect the answer and therefore belong in the key.
    FlakeShow { selection: Selection, flags: i32 },
    /// Read the expression as a `flake.nix`: the document
    /// [`crate::flake_doc::flake_document`] produces, which is what cppnix's
    /// `readFlake` reads off the value. No selection and no render mode --
    /// the whole file is the question -- so the tag alone distinguishes it;
    /// the module digest, the settings and the read set are the rest of the
    /// row as for every question.
    FlakeDocument,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchScope {
    Selected,
    FlakeDefaults,
}

/// The walk a question performs on the value before it reads anything out.
///
/// A list of candidates rather than one path, because a flake installable is
/// a list: `nixpkgs#hello` means the first of
/// `packages.<sys>.hello`, `legacyPackages.<sys>.hello`, ... that resolves,
/// and which one that is depends on the value. The whole ladder is what
/// decides the answer, so the whole ladder is in the key -- keying on the
/// first candidate alone would put two commands whose ladders share a head
/// and differ in the tail into one row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Dotted attribute paths, tried in order; the first that resolves wins.
    /// Exactly one entry for `--expr` and `--file`.
    pub attr_paths: Vec<String>,
    /// Whether an all-digit component indexes a list rather than naming an
    /// attribute. True where cppnix walks with `findAlongAttrPath`
    /// (`--expr`/`--file`), false where it walks with
    /// `AttrCursor::findAlongAttrPath` (a flake), which only ever asks for an
    /// attribute.
    ///
    /// In the key even though nothing can reach two rows through it today: a
    /// flake always carries arguments and a non-flake never does, so the
    /// argument axis already separates the two settings of this flag. That is
    /// an argument from a fact three fields away, and the day a caller
    /// carries arguments and indexes lists it stops holding silently -- one
    /// tag is cheaper than the invariant.
    pub index_lists: bool,
    /// `nix eval --apply`: an expression applied to the selected value
    /// before the question is answered, with the directory it is parsed
    /// under (cppnix parses it under `rootPath(".")`, the working
    /// directory). In the key, text and base both: the answer is the
    /// application's, and a relative path inside the expression means
    /// something else under another directory.
    pub apply: Option<Apply>,
    /// `--arg` and `--argstr`, in command-line order: cppnix's `autoArgs`.
    /// They decide what a function met on the walk is applied to
    /// (`findAlongAttrPath` auto-calls before every component, and
    /// `getDerivations` at every level), so a walk with different arguments
    /// is a different walk, and the arguments are in the key whole: a
    /// `--arg` is an expression and its base directory, a `--argstr` is its
    /// bytes.
    pub auto_args: Vec<AutoArg>,
    /// Whether the selected value itself is auto-called at the end, which
    /// `nix-instantiate --eval` does when it has arguments (`processExpr`)
    /// and `nix eval` never does. cppnix skips the call when there are no
    /// arguments, and so does the evaluator; the flag is in the key
    /// regardless, because keying it only when it matters is a rule a
    /// reader would have to rediscover.
    pub auto_call: bool,
}

/// An expression to apply to the selected value, and where it is parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Apply {
    pub text: String,
    pub base: String,
}

/// One `--arg name expr` or `--argstr name string`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoArg {
    pub name: String,
    pub value: AutoArgValue,
}

/// What an auto-argument binds the name to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoArgValue {
    /// `--arg`: an expression, parsed under `base` (cppnix's
    /// `rootPath(".")`, the working directory) and evaluated lazily -- one
    /// thunk shared by every formal it is bound to, so `--arg a (trace ..)`
    /// traces once however many functions take `a`.
    Expr { text: String, base: String },
    /// `--argstr`: the bytes, as a string with no context.
    Str(String),
}

impl Selection {
    /// The single-candidate walk `--expr` and `--file` perform.
    #[must_use]
    pub fn one(attr_path: impl Into<String>) -> Self {
        Self {
            attr_paths: vec![attr_path.into()],
            index_lists: true,
            apply: None,
            auto_args: Vec::new(),
            auto_call: false,
        }
    }
}

/// One value the embedder applies to the compiled source before the question
/// is asked of the result.
///
/// Two kinds because the one caller that has arguments takes two kinds:
/// `call-flake.nix` is applied to data cppnix computed (a lock file and an
/// overrides document, both JSON) and to one of cppnix's internal primops.
/// Neither this type nor the key below knows a flake is being built.
///
/// **The whole of an argument is its bytes, and that is what makes this
/// keyable.** A JSON document is its text; an internal primop is its name,
/// which selects a body compiled into this binary. There is no third kind
/// carrying a handle, a callback or anything else whose behaviour is not
/// determined by the bytes here -- and there must not be, because a memo key
/// can only cover what it can digest. See [`Arguments::fingerprint`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Argument {
    /// A JSON document in `ixe_alloc_json`'s dialect: `builtins.fromJSON`'s,
    /// plus the `{"__storePath": "..."}` escape for a string that carries a
    /// store path as its context.
    Json(String),
    /// The registered name of one of cppnix's `.internal = true` primops.
    InternalPrimop(String),
}

/// What the source is applied to, in order, before the question is asked.
///
/// Empty for `--expr` and `--file`. Three entries for a flake.
///
/// # Why this is a key axis and not a footnote
///
/// Every flake in the world evaluates the same `call-flake.nix` from the same
/// base directory with the same question, so without this axis the module
/// digest, the settings fingerprint and the question fingerprint are all
/// equal for two different flakes and the memo table holds one row for both
/// of them. The row is addressed by the read set as well, which separates
/// *some* pairs, but a witness is filed under the identity alone: the second
/// flake replays the first flake's questions, those questions still have the
/// answers they had, and the first flake's `drvPath` is served for the
/// second. That is the worst failure this backend has -- silent, durable, and
/// a different package than the one asked for. ENG-12915.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Arguments(Vec<Argument>);

impl Arguments {
    /// The empty list, for a caller that applies nothing.
    #[must_use]
    pub fn none() -> Self {
        Self(Vec::new())
    }

    #[must_use]
    pub fn new(arguments: Vec<Argument>) -> Self {
        Self(arguments)
    }

    #[must_use]
    pub fn as_slice(&self) -> &[Argument] {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// What this list contributes to [`crate::readset::EvalId`].
    ///
    /// Every byte of every argument, in order, each behind a kind tag.
    /// Nothing is summarised: a digest of a lock file would be sound too, but
    /// digesting the bytes that are actually applied costs one pass over a
    /// document that has just been built and removes the question of whether
    /// the summary covers the document.
    #[must_use]
    pub fn fingerprint(&self) -> ix_kernel::hash::Hash {
        let mut parts: Vec<&[u8]> = Vec::with_capacity(self.0.len() * 2 + 1);
        // The count, so a caller applying `[a]` cannot digest as one applying
        // `[a, <empty json>]` if a kind tag is ever added whose payload can
        // be empty.
        let count = (self.0.len() as u64).to_be_bytes();
        parts.push(&count);
        for argument in &self.0 {
            // Destructured so a new kind will not compile until somebody has
            // decided what it contributes -- and, more to the point, has been
            // made to notice that a kind whose behaviour is not determined by
            // its bytes cannot be memoised at all.
            let (tag, text): (&[u8], &str) = match argument {
                Argument::Json(text) => (b"json", text),
                Argument::InternalPrimop(name) => (b"internal-primop", name),
            };
            parts.push(tag);
            parts.push(text.as_bytes());
        }
        ix_kernel::hash::tagged(ARGUMENTS_TAG, &parts)
    }
}

/// Domain separation for a question.
const QUESTION_TAG: &str = "ixe-question-v2";

/// Domain separation for an argument list.
const ARGUMENTS_TAG: &str = "ixe-arguments-v1";

impl Question {
    /// What this question contributes to [`crate::readset::EvalId`].
    ///
    /// Destructured variant by variant and field by field, for the reason
    /// [`crate::eval::Settings::fingerprint`] is: a new field will not
    /// compile until somebody has decided where it goes in the key.
    #[must_use]
    pub fn fingerprint(&self) -> ix_kernel::hash::Hash {
        // Owned, because a candidate ladder contributes a length as well as
        // its bytes and the length has to live somewhere.
        let mut parts: Vec<Vec<u8>> = Vec::new();
        match self {
            Question::Whole { render } => {
                parts.push(b"whole".to_vec());
                parts.push(render_tag(*render).to_vec());
            }
            Question::Select { selection, render } => {
                parts.push(b"select".to_vec());
                selection.extend(&mut parts);
                parts.push(render_tag(*render).to_vec());
            }
            Question::Derivation { selection } => {
                parts.push(b"derivation".to_vec());
                selection.extend(&mut parts);
            }
            Question::DerivationSet { selection } => {
                parts.push(b"derivation-set".to_vec());
                selection.extend(&mut parts);
            }
            Question::DerivationPath { selection } => {
                parts.push(b"derivation-path".to_vec());
                selection.extend(&mut parts);
            }
            Question::Application { selection } => {
                parts.push(b"application".to_vec());
                selection.extend(&mut parts);
            }
            Question::SourcePosition { selection } => {
                parts.push(b"source-position".to_vec());
                selection.extend(&mut parts);
            }
            Question::SearchPackages { selection, scope } => {
                parts.push(b"search-packages".to_vec());
                selection.extend(&mut parts);
                parts.push(match scope {
                    SearchScope::Selected => b"selected".to_vec(),
                    SearchScope::FlakeDefaults => b"flake-defaults".to_vec(),
                });
            }
            Question::FlakeCheck { selection, options } => {
                parts.push(b"flake-check".to_vec());
                selection.extend(&mut parts);
                parts.push(options.flags().to_be_bytes().to_vec());
            }
            Question::FlakeShow { selection, flags } => {
                parts.push(b"flake-show".to_vec());
                selection.extend(&mut parts);
                parts.push(flags.to_be_bytes().to_vec());
            }
            Question::FlakeDocument => parts.push(b"flake-document".to_vec()),
        }
        let parts: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        // `hash::tagged` length-prefixes every field, so an attribute path
        // that happens to end where the next field begins cannot digest the
        // same as a different split of the same bytes.
        ix_kernel::hash::tagged(QUESTION_TAG, &parts)
    }
}

impl Selection {
    /// Append this walk's contribution to a question fingerprint.
    fn extend(&self, parts: &mut Vec<Vec<u8>>) {
        let Self {
            attr_paths,
            index_lists,
            apply,
            auto_args,
            auto_call,
        } = self;
        // The count first: `hash::tagged` length-prefixes each part, so two
        // ladders cannot merge, but a ladder followed by the render tag and a
        // shorter ladder followed by a path that spells the render tag would
        // otherwise be the same sequence of parts.
        parts.push((attr_paths.len() as u64).to_be_bytes().to_vec());
        for path in attr_paths {
            parts.push(path.as_bytes().to_vec());
        }
        parts.push(if *index_lists {
            b"index-lists".to_vec()
        } else {
            b"attrs-only".to_vec()
        });
        // Three parts or one, so "no apply" cannot spell an apply whose text
        // and base happen to be empty.
        match apply {
            Some(Apply { text, base }) => {
                parts.push(b"apply".to_vec());
                parts.push(text.as_bytes().to_vec());
                parts.push(base.as_bytes().to_vec());
            }
            None => parts.push(b"no-apply".to_vec()),
        }
        // The count first, as for the ladder, then each argument as its
        // name, its kind, and the kind's own parts: a `--arg` is text and
        // base, a `--argstr` is text alone. The kind tag decides how many
        // parts follow it, so `--arg a x` and `--argstr a x` are different
        // sequences even where their bytes agree.
        parts.push((auto_args.len() as u64).to_be_bytes().to_vec());
        for AutoArg { name, value } in auto_args {
            parts.push(name.as_bytes().to_vec());
            match value {
                AutoArgValue::Expr { text, base } => {
                    parts.push(b"arg".to_vec());
                    parts.push(text.as_bytes().to_vec());
                    parts.push(base.as_bytes().to_vec());
                }
                AutoArgValue::Str(text) => {
                    parts.push(b"argstr".to_vec());
                    parts.push(text.as_bytes().to_vec());
                }
            }
        }
        parts.push(if *auto_call {
            b"auto-call".to_vec()
        } else {
            b"no-auto-call".to_vec()
        });
    }
}

/// The key bytes for one render mode.
///
/// Spelled out rather than taken from the discriminant, so reordering the
/// enum cannot silently re-point every stored row at a different mode.
fn render_tag(mode: RenderMode) -> &'static [u8] {
    match mode {
        RenderMode::Plain => b"plain",
        RenderMode::PlainLazy => b"plain-lazy",
        RenderMode::ValuePrinter => b"value-printer",
        RenderMode::Json => b"json",
        RenderMode::Raw => b"raw",
        RenderMode::Xml => b"xml",
    }
}

/// Render an already-evaluated value the way `mode` says.
///
/// Takes the host because rendering forces, and forcing can still ask
/// questions: `{ a = import ./b.nix; }` reaches the filesystem inside the
/// printer, not before it.
pub fn render(
    vm: &mut Vm,
    host: &dyn Host,
    value: Value,
    mode: RenderMode,
) -> Result<Vec<u8>, EvalError> {
    let mut memo = crate::eval::JobMemo::default();
    render_with(vm, host, value, mode, &mut memo)
}

/// [`render`] with the evaluation's memo lent by the caller
/// ([`crate::eval::drive_with`] says who lends one).
pub(crate) fn render_with(
    vm: &mut Vm,
    host: &dyn Host,
    value: Value,
    mode: RenderMode,
    memo: &mut crate::eval::JobMemo,
) -> Result<Vec<u8>, EvalError> {
    match mode {
        RenderMode::Plain => {
            vm.start_print(value);
            finish_string(vm, host, memo, "printer")
        }
        RenderMode::PlainLazy => {
            if !matches!(
                value,
                Value::Str(_)
                    | Value::Int(_)
                    | Value::Float(_)
                    | Value::Bool(_)
                    | Value::Null
                    | Value::Path(_)
            ) {
                return Err(EvalError::Unimplemented(crate::refusal::Refusal::new(
                    crate::refusal::RefusalToken::LazyPrint,
                    format!(
                        "lazy top-level printing of {} (run with --strict)",
                        crate::value2::type_name(&value)
                    ),
                )));
            }
            vm.start_print(value);
            finish_string(vm, host, memo, "printer")
        }
        RenderMode::ValuePrinter => {
            vm.start_task(crate::task::Task::Print(
                crate::print::Print::value_printer(value),
            ));
            finish_string(vm, host, memo, "printer")
        }
        RenderMode::Json => {
            let Some(idx) = crate::builtins::TABLE
                .iter()
                .position(|b| b.name == "toJSON")
            else {
                return Err(EvalError::eval(
                    ErrKind::Eval,
                    "internal: no toJSON builtin to render with",
                ));
            };
            let idx = u16::try_from(idx).unwrap_or(u16::MAX);
            vm.start_task(crate::task::Task::builtin(
                idx,
                vec![crate::value2::Slot::value(value)],
            ));
            finish_string(vm, host, memo, "toJSON")
        }
        // Through `builtins.toXML`'s own walker, the way `Json` goes through
        // `toJSON`'s: cppnix's `--xml` and `prim_toXML` are one function,
        // `printValueAsXML`, so the string the builtin would have produced
        // *is* the document -- already ending in a newline, which is why the
        // bridge prints these bytes without appending one.
        RenderMode::Xml => {
            let Some(idx) = crate::builtins::TABLE
                .iter()
                .position(|b| b.name == "toXML")
            else {
                return Err(EvalError::eval(
                    ErrKind::Eval,
                    "internal: no toXML builtin to render with",
                ));
            };
            let idx = u16::try_from(idx).unwrap_or(u16::MAX);
            vm.start_task(crate::task::Task::builtin(
                idx,
                vec![crate::value2::Slot::value(value)],
            ));
            finish_string(vm, host, memo, "toXML")
        }
        // cppnix's `--raw` calls coerceToString with coerceMore = false, so
        // an integer or a Boolean is an error here even though `toString`
        // would take it.
        //
        // Paths and derivation-shaped attrsets are the two cases it accepts
        // and this does not. Both end in the store. The reason used to be
        // written here as "the VM has no handle on the store", which stopped
        // being true when ENG-12447 landed a store-copy hook -- so the claim
        // is now about this function instead: rendering is a pure walk, and
        // asking the store is a suspension it cannot make. Routing `Raw`
        // through the machine the way `Json` goes is ENG-12493.
        //
        // Refusing by name rather than approximating, because an approximated
        // path is a plausible wrong string and nothing downstream can tell.
        // Held by `raw_of_a_path_refuses_by_name`, not by this paragraph.
        RenderMode::Raw => match value {
            // Bytes, as cppnix writes them (ENG-13147): a C string carries
            // any byte but NUL, so a non-UTF-8 value crosses the ABI intact.
            Value::Str(text) => Ok(text.bytes().to_vec()),
            Value::Path(_) => Err(EvalError::Unimplemented(Refusal::new(
                RefusalToken::UnsupportedRender,
                "--raw of a path (cppnix copies it to the store first; ENG-12493)",
            ))),
            Value::Attrs(_) => Err(EvalError::Unimplemented(Refusal::new(
                RefusalToken::UnsupportedRender,
                "--raw of an attribute set (cppnix coerces one through __toString or outPath)",
            ))),
            // cppnix's wording, minus the printed value it appends: that
            // suffix goes through its truncating error printer, and
            // reproducing the truncation rules to match a message nothing
            // compares byte for byte is not worth a second printer.
            other => Err(EvalError::eval(
                ErrKind::Eval,
                format!(
                    "cannot coerce {} to a string",
                    crate::value2::type_name(&other)
                ),
            )),
        },
    }
}

/// Drive the machine to a `Value::Str` and unwrap its bytes. Every renderer
/// above ends in one, so a non-string here is this crate's bug and says so.
fn finish_string(
    vm: &mut Vm,
    host: &dyn Host,
    memo: &mut crate::eval::JobMemo,
    what: &str,
) -> Result<Vec<u8>, EvalError> {
    match crate::eval::drive_with(vm, host, memo) {
        Ok(Value::Str(text)) => Ok(text.bytes().to_vec()),
        Ok(other) => Err(EvalError::eval(
            ErrKind::Eval,
            format!("internal: {what} produced {other:?}"),
        )),
        Err(error) => Err(crate::eval::map_vm_error(error)),
    }
}

/// Compile and evaluate one source to a live value, for an embedder that
/// wants to walk it rather than read it rendered.
///
/// # Why this one does not memoise, and why that is no longer a limit
///
/// [`evaluate`] can serve its answer from the result cache because its answer
/// is a string: the memo stores rendered text keyed on the questions the
/// evaluation asked. A value is not a rendered answer -- the caller has not
/// said yet which part of it it wants or how -- so there is nothing to look
/// up here, and there still is not. The compile cache applies and the result
/// cache cannot.
///
/// What changed is where the question comes from. This used to carry the
/// conclusion that memoising `nix eval` "needs the whole question up front,
/// which a handle walk deliberately does not have" (ENG-12470), and the
/// second half of that is false. A handle *walk* does not have it; every
/// *command* that performs one does, before it opens a session. So the
/// question is stated up front as a [`Question`], folded into
/// [`crate::readset::EvalId`], and answered by
/// `capi::ixe_session_eval_question`, which serves the memo and then hands
/// back a root handle only when it could not. This function is what that one
/// falls back to when there is no cache configured, and the one entry point
/// for an embedder that genuinely cannot describe its question in advance.
/// ENG-12470, ENG-12830.
pub fn evaluate_value(
    vm: &mut Vm,
    host: &dyn Host,
    source: &str,
    base_dir: &str,
    origin: Origin<'_>,
) -> (Result<Value, EvalError>, Reuse) {
    let mut reuse = Reuse::default();
    let before = vm.modules().hits();
    let compiled = match vm.compile(source, base_dir, origin) {
        Ok(compiled) => compiled,
        Err(error) => {
            let failure = compile_failure(&error);
            let error = error_of(&failure)
                .unwrap_or_else(|| EvalError::eval(ErrKind::Eval, failure.value.clone()));
            return (Err(error), reuse);
        }
    };
    reuse.compile_hit = vm.modules().hits() > before;
    (run_to_value(vm, &compiled.module, host), reuse)
}

/// Evaluate one source, the shape an embedder that runs one expression per
/// process needs. The machine's compile cache decides whether there is a store
/// ([`crate::modcache::ModuleCache::store`]); with one, the result memo is
/// consulted and published too.
///
/// Everything else is built per call. That is affordable because nothing is
/// loaded eagerly: rows are point lookups by key, so the result cache over an
/// open store is three paths. The win is not within the process, it is that
/// the next process finds this one's work.
///
/// Warnings about damaged store entries are returned rather than printed, so
/// the embedder decides where they go.
pub fn evaluate_once(
    vm: &mut Vm,
    host: &dyn Host,
    source: &str,
    base_dir: &str,
    origin: Origin<'_>,
    memoise_results: bool,
    verify_rate: u32,
) -> (Result<String, EvalError>, Vec<Complaint>) {
    // Cloned out of the machine because the result cache borrows the store for
    // the length of the run while the machine is borrowed mutably. A `Store`
    // is a handle, so the clone is the same cache.
    let Some(store) = vm.modules().store().cloned() else {
        return (
            crate::eval::eval_str_on(source, base_dir, origin, vm, host),
            Vec::new(),
        );
    };

    let mut results = ResultCache::persistent(&store);
    results.set_verify_rate(verify_rate);
    let (result, _) = evaluate(
        vm,
        memoise_results.then_some(&mut results),
        host,
        source,
        base_dir,
        origin,
    );

    let mut warnings: Vec<Complaint> = vm
        .modules_mut()
        .take_corruption()
        .into_iter()
        .map(Complaint::warning)
        .collect();
    warnings.extend(results.take_corruption());

    let answer = match error_of(&result) {
        None => Ok(result.value),
        Some(error) => Err(error),
    };
    (answer, warnings)
}

/// Evaluate one source to a live value, through the machine's compile cache.
///
/// The value-shaped twin of [`evaluate_once`], and the reason it takes the VM
/// rather than making one: the value it returns points into that VM's modules
/// and interner, so the machine has to outlive the answer. `evaluate_once`
/// hands back a `String` and can therefore own its VM for the length of one
/// call; this cannot.
///
/// Warnings about damaged cache entries are returned, not printed, for the
/// same reason they are there: the embedder owns where they go.
///
/// This path publishes modules but never a result, so no `record` runs the
/// cap sweep for it; it sweeps once itself at the end, so a store reached
/// only through the handle API is bounded by its cap (0: never) like every
/// other.
pub fn evaluate_value_once(
    vm: &mut Vm,
    host: &dyn Host,
    source: &str,
    base_dir: &str,
    origin: Origin<'_>,
) -> (Result<Value, EvalError>, Vec<Complaint>) {
    let (answer, _) = evaluate_value(vm, host, source, base_dir, origin);
    let mut warnings: Vec<Complaint> = vm
        .modules_mut()
        .take_corruption()
        .into_iter()
        .map(Complaint::warning)
        .collect();
    warnings.extend(vm.modules().store().and_then(crate::readset::sweep_to_cap));
    (answer, warnings)
}

fn open_store(dir: &std::path::Path) -> Result<crate::store::Store, String> {
    crate::store::Store::open(dir).map_err(|error| {
        format!(
            "cannot open the evaluation cache at {}: {error}",
            dir.display()
        )
    })
}

/// Open the on-disk cache at `dir`, capped at `cache_max_bytes` (0 for no
/// cap), or say why not.
///
/// A cache is an optimisation and an expression is still owed an answer when
/// it is missing, so a directory that will not open is a warning and no store,
/// never a failure; `None` for `dir` is the embedder choosing no cache, and is
/// silent. The caller puts the store behind its machine
/// ([`crate::modcache::ModuleCache::persistent`] into [`Vm::with_modules`]);
/// from then on the machine is the one place that knows whether there is a
/// cache, which is what lets [`evaluate_once`] take a VM and nothing else.
pub fn open_cache(
    dir: Option<&std::path::Path>,
    cache_max_bytes: u64,
) -> (Option<crate::store::Store>, Option<Complaint>) {
    match dir.map(open_store) {
        None => (None, None),
        Some(Ok(store)) => (Some(store.with_max_bytes(cache_max_bytes)), None),
        Some(Err(reason)) => (
            None,
            Some(Complaint::warning(format!(
                "{reason}; evaluating without it"
            ))),
        ),
    }
}

/// The on-disk evaluation cache a handle-API session memoises into.
///
/// Owned by the session rather than rebuilt per call, because this caller
/// asks its question in two halves -- "have you got this?" before the walk,
/// "here is what I got" after it -- and the store, the recording and the
/// sampler all have to survive in between. [`evaluate_once`] needs none of
/// that and keeps building its pieces per call.
///
/// The [`ResultCache`] itself is still built per half. It borrows the store,
/// so a struct owning both would be self-referential. The sampler and optional
/// bounded decoded-witness owner are carried explicitly across halves. A CAPI
/// cache handle can share that owner across fresh sessions without retaining
/// VM values or host contexts.
pub struct QuestionCache {
    store: crate::store::Store,
    retained: Option<crate::readset::retained::Shared>,
    verify_rate: u32,
    /// The sampler's xorshift state, carried between the halves and between
    /// questions.
    ///
    /// Explicit because a `ResultCache` built fresh each time restarts from
    /// the fixed seed, which turns "one hit in N" into "every hit, or no hit,
    /// identically in every process" -- a sampler that is off in exactly the
    /// same way everywhere, and looks on.
    verify_state: u64,
    complaints: Vec<Complaint>,
    /// What the last [`QuestionCache::serve`] answered from, for
    /// [`QuestionCache::forget_served`]: the identity and the row key the
    /// replayed read set produced.
    last_served: Option<(crate::readset::EvalId, ix_kernel::Key)>,
}

impl QuestionCache {
    /// Open the cache at `dir`, capped at `cache_max_bytes` (0 for no cap),
    /// or say why not.
    pub fn open(
        dir: &std::path::Path,
        verify_rate: u32,
        cache_max_bytes: u64,
    ) -> Result<Self, String> {
        Ok(Self {
            store: open_store(dir)?.with_max_bytes(cache_max_bytes),
            retained: None,
            verify_rate,
            // The same seed `ResultCache::new` uses: any non-zero value will
            // do, and a fixed one makes a run that finds something repeatable.
            verify_state: 0x2545_f491_4f6c_dd1d,
            complaints: Vec::new(),
            last_served: None,
        })
    }

    pub(crate) fn with_retained(
        mut self,
        retained: Option<crate::readset::retained::Shared>,
    ) -> Self {
        self.retained = retained;
        self
    }

    /// The filtered-copy memo backed by this store, for the recorder of a
    /// question ([`crate::readset::RecordingHost::with_copy_memo`]). The
    /// store directory is the embedder's ([`crate::eval::store_dir`]), the
    /// same one the replay's sealing uses.
    #[must_use]
    pub fn copy_memo(&self) -> crate::readset::CopyMemo {
        self.store
            .copy_memo(crate::eval::store_dir().map(str::to_owned))
    }

    /// [`serve`], against this store.
    pub fn serve(
        &mut self,
        identity: &crate::readset::EvalId,
        host: &dyn Host,
        settings: &crate::eval::Settings,
    ) -> Served {
        let (served, key) = self.with_results(|results| {
            let served = serve(results, identity, host, settings);
            (served, results.served_key())
        });
        self.last_served = key.map(|key| (*identity, key));
        served
    }

    /// What the last [`QuestionCache::serve`] answered from, if it answered.
    #[must_use]
    pub fn last_served(&self) -> Option<(crate::readset::EvalId, ix_kernel::Key)> {
        self.last_served
    }

    /// Forget a row this cache (or another process's) served, because the
    /// embedder could not use the answer. See [`ResultCache::forget`].
    pub fn forget(
        &mut self,
        identity: &crate::readset::EvalId,
        key: ix_kernel::Key,
        why: &str,
    ) -> Result<(), String> {
        let result = self.with_results(|results| results.forget(identity, key, why));
        result.map_err(|error| error.to_string())
    }

    /// [`settle`], against this store.
    #[allow(clippy::too_many_arguments)]
    pub fn settle(
        &mut self,
        identity: &crate::readset::EvalId,
        host: &dyn Host,
        settings: &crate::eval::Settings,
        read_set: &crate::readset::ReadSet,
        result: &EvalResult,
        verifying: Option<&EvalResult>,
        interrupted: bool,
    ) {
        self.with_results(|results| {
            settle(
                results,
                identity,
                host,
                settings,
                read_set,
                result,
                verifying,
                interrupted,
            );
        });
    }

    /// Run `body` against a result cache over this store, carrying the
    /// sampler across and draining whatever the store complained about.
    fn with_results<T>(&mut self, body: impl FnOnce(&mut ResultCache<'_, dyn Cas>) -> T) -> T {
        // Destructured so the shared borrow of the store and the mutable
        // borrow of the sampler are of different fields.
        let Self {
            store,
            retained,
            verify_rate,
            verify_state,
            complaints,
            last_served: _,
        } = self;
        let mut results = ResultCache::persistent(store).with_retained(retained.clone());
        results.set_verify_rate(*verify_rate);
        results.set_verify_state(*verify_state);
        let answer = body(&mut results);
        *verify_state = results.verify_state();
        complaints.extend(results.take_corruption());
        answer
    }

    /// Add a complaint of the caller's own, so everything the embedder has to
    /// hear about the cache leaves by one door.
    pub fn complain(&mut self, complaint: Complaint) {
        self.complaints.push(complaint);
    }

    /// Everything the store has complained about since the last call.
    pub fn take_complaints(&mut self) -> Vec<Complaint> {
        core::mem::take(&mut self.complaints)
    }
}

/// The question and argument axes of the memo key: ENG-12830's and
/// ENG-12915's soundness halves.
///
/// Making the handle path memoise means commands that compile the same source
/// share a module digest and a settings fingerprint and are told apart by
/// these two alone. Every flake in the world compiles the same
/// `call-flake.nix` from the same base directory, so for the flake path the
/// argument axis is the *only* thing between two packages and one row. If
/// either fingerprint drops a field, `nix eval --raw` is served `nix eval
/// --json`'s answer, `a.b` is served `a.c`'s, or one flake is served
/// another's `drvPath`. All three are wrong answers that look exactly like
/// right ones, none appears until the second run, and nothing but this is
/// watching.
#[cfg(test)]
mod question_key {
    use super::*;

    fn select(attr_path: &str, render: RenderMode) -> Question {
        Question::Select {
            selection: Selection::one(attr_path),
            render,
        }
    }

    fn derivation(attr_path: &str) -> Question {
        Question::Derivation {
            selection: Selection::one(attr_path),
        }
    }

    /// Fail with the label of whichever pair collided.
    fn all_distinct(keys: Vec<(String, ix_kernel::hash::Hash)>, expected: usize, what: &str) {
        let mut seen: Vec<(String, ix_kernel::hash::Hash)> = Vec::new();
        for (label, key) in keys {
            for (other, previous) in &seen {
                assert_ne!(
                    &key, previous,
                    "'{label}' and '{other}' are one memo row, so one of them \
                     will be served the other's answer"
                );
            }
            seen.push((label, key));
        }
        assert_eq!(seen.len(), expected, "the {what} table lost an entry");
    }

    /// Every field of every variant moves the key.
    ///
    /// Written as a list of distinct questions rather than as one assertion
    /// per field, so the failure names which pair collided. A new variant or
    /// a new field will not compile in `Question::fingerprint` until it is
    /// placed; this is the other half, checking that placing it did something.
    #[test]
    fn each_question_is_its_own_memo_row() {
        let questions = [
            (
                "whole/plain",
                Question::Whole {
                    render: RenderMode::Plain,
                },
            ),
            (
                "whole/json",
                Question::Whole {
                    render: RenderMode::Json,
                },
            ),
            (
                "whole/raw",
                Question::Whole {
                    render: RenderMode::Raw,
                },
            ),
            (
                "whole/value-printer",
                Question::Whole {
                    render: RenderMode::ValuePrinter,
                },
            ),
            ("select ''/plain", select("", RenderMode::Plain)),
            ("select a/plain", select("a", RenderMode::Plain)),
            ("select a/json", select("a", RenderMode::Json)),
            ("select a.b/plain", select("a.b", RenderMode::Plain)),
            ("select ab/plain", select("ab", RenderMode::Plain)),
            ("derivation ''", derivation("")),
            ("derivation a", derivation("a")),
            (
                "derivation-set a",
                Question::DerivationSet {
                    selection: Selection::one("a"),
                },
            ),
            (
                "derivation-path a",
                Question::DerivationPath {
                    selection: Selection::one("a"),
                },
            ),
            (
                "application a",
                Question::Application {
                    selection: Selection::one("a"),
                },
            ),
            (
                "source-position a",
                Question::SourcePosition {
                    selection: Selection::one("a"),
                },
            ),
            (
                "source-position b",
                Question::SourcePosition {
                    selection: Selection::one("b"),
                },
            ),
            (
                "flake-show/default",
                Question::FlakeShow {
                    selection: Selection::one(""),
                    flags: 0,
                },
            ),
            (
                "flake-show/legacy",
                Question::FlakeShow {
                    selection: Selection::one(""),
                    flags: 1,
                },
            ),
            ("flake-document", Question::FlakeDocument),
        ];
        all_distinct(
            questions
                .iter()
                .map(|(label, question)| ((*label).to_owned(), question.fingerprint()))
                .collect(),
            19,
            "question",
        );
    }

    /// The full cross product of the two dimensions a `Select` has, so a
    /// collapse in either one is caught wherever it is.
    ///
    /// The table above is a hand-picked list, and a hand-picked list is
    /// exactly the shape that passes while one part is collapsed: it happens
    /// to contain the pairs somebody thought of. This walks every
    /// (attribute path, render mode) combination and requires all of them to
    /// be distinct rows, which is the same treatment
    /// `each_purity_configuration_is_its_own_memo_key` gives the settings.
    ///
    /// The two dimensions are independent and both change the answer:
    /// `a` and `a.b` are different values, and `--json` and `--raw` are
    /// different bytes for one value. So every pair here is two questions and
    /// must be two rows.
    #[test]
    fn every_select_dimension_moves_the_row_independently() {
        let paths = ["", "a", "b", "a.b", "ab", "a.b.c"];
        let renders = [
            ("plain", RenderMode::Plain),
            ("value-printer", RenderMode::ValuePrinter),
            ("json", RenderMode::Json),
            ("raw", RenderMode::Raw),
        ];
        let mut keys = Vec::new();
        for path in paths {
            for (render_name, render) in renders {
                keys.push((
                    format!("{path:?}/{render_name}"),
                    select(path, render).fingerprint(),
                ));
            }
        }
        all_distinct(keys, paths.len() * renders.len(), "select cross product");
    }

    /// A candidate ladder is keyed whole, not by its first entry.
    ///
    /// `nixpkgs#hello` is `packages.<sys>.hello` *or*
    /// `legacyPackages.<sys>.hello`, whichever resolves, and which one that is
    /// depends on the value being walked. Two commands whose ladders share a
    /// head and differ after it can therefore reach different attributes, so
    /// they must be different rows -- and they were not while the bridge
    /// passed `attrPaths.front()` as the key's attribute path.
    #[test]
    fn a_candidate_ladder_is_keyed_whole() {
        let ladder = |paths: &[&str]| {
            Question::Select {
                selection: Selection {
                    attr_paths: paths.iter().map(|p| (*p).to_owned()).collect(),
                    index_lists: false,
                    apply: None,
                    auto_args: Vec::new(),
                    auto_call: false,
                },
                render: RenderMode::Raw,
            }
            .fingerprint()
        };
        all_distinct(
            vec![
                ("[a]".to_owned(), ladder(&["a"])),
                ("[a, b]".to_owned(), ladder(&["a", "b"])),
                ("[a, c]".to_owned(), ladder(&["a", "c"])),
                ("[b, a]".to_owned(), ladder(&["b", "a"])),
                ("[a, b, c]".to_owned(), ladder(&["a", "b", "c"])),
                ("[]".to_owned(), ladder(&[])),
            ],
            6,
            "candidate ladder",
        );
    }

    /// `--apply` moves the key by its text AND by the directory it is
    /// parsed under: `x: x` and `x: x + 1` are two answers, and so are one
    /// expression naming `./f` from two directories.
    #[test]
    fn the_apply_expression_and_its_base_are_in_the_key() {
        let with = |apply: Option<(&str, &str)>| {
            Question::Select {
                selection: Selection {
                    attr_paths: vec!["a".to_owned()],
                    index_lists: true,
                    apply: apply.map(|(text, base)| Apply {
                        text: text.to_owned(),
                        base: base.to_owned(),
                    }),
                    auto_args: Vec::new(),
                    auto_call: false,
                },
                render: RenderMode::Raw,
            }
            .fingerprint()
        };
        let keys = vec![
            ("none".to_owned(), with(None)),
            ("identity".to_owned(), with(Some(("x: x", "/w")))),
            ("plus one".to_owned(), with(Some(("x: x + 1", "/w")))),
            ("identity elsewhere".to_owned(), with(Some(("x: x", "/v")))),
            ("empty".to_owned(), with(Some(("", "")))),
        ];
        all_distinct(keys, 5, "apply");
    }

    /// `--arg`/`--argstr` and the final auto-call are in the key: the same
    /// bytes bound by the two flags, the same expression under two
    /// directories, two names, two orders, and with or without the call.
    #[test]
    fn the_auto_arguments_and_the_auto_call_are_in_the_key() {
        let arg = |name: &str, text: &str, base: &str| AutoArg {
            name: name.to_owned(),
            value: AutoArgValue::Expr {
                text: text.to_owned(),
                base: base.to_owned(),
            },
        };
        let str_arg = |name: &str, text: &str| AutoArg {
            name: name.to_owned(),
            value: AutoArgValue::Str(text.to_owned()),
        };
        let with = |auto_args: Vec<AutoArg>, auto_call: bool| {
            Question::Select {
                selection: Selection {
                    attr_paths: vec![String::new()],
                    index_lists: true,
                    apply: None,
                    auto_args,
                    auto_call,
                },
                render: RenderMode::Plain,
            }
            .fingerprint()
        };
        let keys = vec![
            ("none".to_owned(), with(vec![], false)),
            ("none, called".to_owned(), with(vec![], true)),
            ("arg".to_owned(), with(vec![arg("a", "1", "/w")], true)),
            (
                "arg elsewhere".to_owned(),
                with(vec![arg("a", "1", "/v")], true),
            ),
            ("argstr".to_owned(), with(vec![str_arg("a", "1")], true)),
            (
                "other name".to_owned(),
                with(vec![arg("b", "1", "/w")], true),
            ),
            (
                "uncalled".to_owned(),
                with(vec![arg("a", "1", "/w")], false),
            ),
            (
                "two".to_owned(),
                with(vec![arg("a", "1", "/w"), arg("b", "2", "/w")], true),
            ),
            (
                "two, swapped".to_owned(),
                with(vec![arg("b", "2", "/w"), arg("a", "1", "/w")], true),
            ),
            // An empty text under an empty base is still an `--arg`, not
            // the absence of one.
            ("empty arg".to_owned(), with(vec![arg("a", "", "")], true)),
        ];
        all_distinct(keys, 10, "auto-args");
    }

    /// `nix-build`'s set walk and `nix build`'s single derivation are two
    /// rows for one selection.
    #[test]
    fn the_derivation_set_walk_is_its_own_question() {
        let selection = Selection::one("");
        let single = Question::Derivation {
            selection: selection.clone(),
        }
        .fingerprint();
        let set = Question::DerivationSet { selection }.fingerprint();
        assert_ne!(single, set, "one selection, two questions, one key");
    }

    /// The list-indexing rule is in the key.
    ///
    /// `xs.0` is the first element of a list under `--expr`'s walker and a
    /// missing attribute named `0` under a flake's. Nothing can reach both
    /// settings with everything else equal today, because a flake always
    /// carries arguments and nothing else carries any -- but that is an
    /// invariant three fields away, and this costs one tag.
    #[test]
    fn the_list_indexing_rule_is_in_the_key() {
        let with = |index_lists| {
            Question::Select {
                selection: Selection {
                    attr_paths: vec!["xs.0".to_owned()],
                    index_lists,
                    apply: None,
                    auto_args: Vec::new(),
                    auto_call: false,
                },
                render: RenderMode::Raw,
            }
            .fingerprint()
        };
        assert_ne!(with(true), with(false));
    }

    /// A `Select` and a `Whole` that render the same way are still two rows.
    ///
    /// They are not the same question even when the attribute path is empty:
    /// `nix eval -f x.nix` walks nothing and renders with the value printer,
    /// `nix-instantiate --eval -f x.nix` renders with `printAmbiguous`, and
    /// the two printers do not always agree. Held separately from the table
    /// above because this is the pair a reader is most likely to think is one.
    #[test]
    fn an_empty_selection_is_not_the_whole_expression() {
        assert_ne!(
            Question::Whole {
                render: RenderMode::Plain
            }
            .fingerprint(),
            select("", RenderMode::Plain).fingerprint(),
        );
    }

    /// The attribute path cannot bleed into the next field.
    ///
    /// `hash::tagged` length-prefixes, so this holds; asserted because the
    /// failure it guards against is silent and specific -- a question whose
    /// path ends where the render tag begins colliding with a different split
    /// of the same bytes.
    #[test]
    fn an_attribute_path_cannot_absorb_the_render_tag() {
        assert_ne!(
            select("aplain", RenderMode::Json).fingerprint(),
            select("a", RenderMode::Plain).fingerprint(),
        );
    }

    /// Every argument list is its own row: ENG-12915's core assertion.
    ///
    /// The pairs that matter are the ones a flake produces. Two flakes differ
    /// in the lock file they are applied to, or in the overrides document, or
    /// in both; a lock file that differs in one character is a different
    /// closure of inputs, and an overrides document that differs in one
    /// character names a different store path for some input's source. Every
    /// one of those has to be a different row, because every one of them can
    /// produce a different `drvPath`.
    #[test]
    fn each_argument_list_is_its_own_memo_row() {
        let json = |text: &str| Argument::Json(text.to_owned());
        let primop = |name: &str| Argument::InternalPrimop(name.to_owned());
        let lists = [
            ("none", vec![]),
            ("one lock", vec![json(r#"{"nodes":{}}"#)]),
            ("another lock", vec![json(r#"{"nodes":{"a":1}}"#)]),
            (
                "lock + overrides",
                vec![json(r#"{"nodes":{}}"#), json(r#"{"root":{}}"#)],
            ),
            (
                "lock + other overrides",
                vec![json(r#"{"nodes":{}}"#), json(r#"{"root":{"dir":""}}"#)],
            ),
            (
                "swapped",
                vec![json(r#"{"root":{}}"#), json(r#"{"nodes":{}}"#)],
            ),
            ("a primop", vec![primop("fetchFinalTree")]),
            (
                "the flake three",
                vec![
                    json(r#"{"nodes":{}}"#),
                    json(r#"{"root":{}}"#),
                    primop("fetchFinalTree"),
                ],
            ),
        ];
        all_distinct(
            lists
                .iter()
                .map(|(label, list)| {
                    (
                        (*label).to_owned(),
                        Arguments::new(list.clone()).fingerprint(),
                    )
                })
                .collect(),
            8,
            "argument",
        );
    }

    /// A JSON document and a primop name that read the same are two arguments.
    ///
    /// The kind tag is what keeps them apart. Without it a document whose text
    /// happens to be a primop's name would address the primop's row, which is
    /// a value of an entirely different type.
    #[test]
    fn an_argument_s_kind_is_in_the_key() {
        assert_ne!(
            Arguments::new(vec![Argument::Json("fetchFinalTree".to_owned())]).fingerprint(),
            Arguments::new(vec![Argument::InternalPrimop("fetchFinalTree".to_owned())])
                .fingerprint(),
        );
    }

    /// One argument's bytes cannot run into the next one's.
    ///
    /// `hash::tagged` length-prefixes every part, so `["ab", "c"]` and
    /// `["a", "bc"]` are different keys. Asserted because a lock file and an
    /// overrides document are adjacent JSON blobs and a boundary that could
    /// slide would let two different (lock, overrides) pairs address one row.
    #[test]
    fn arguments_cannot_run_into_each_other() {
        let json = |text: &str| Argument::Json(text.to_owned());
        assert_ne!(
            Arguments::new(vec![json("ab"), json("c")]).fingerprint(),
            Arguments::new(vec![json("a"), json("bc")]).fingerprint(),
        );
    }

    /// The identity moves with the question and with the arguments, not just
    /// with their fingerprints.
    ///
    /// A separate assertion because the two halves are computed in different
    /// files, and a fingerprint that moved while `EvalId::of` ignored it would
    /// let one question read another's row while every other test in this
    /// module passed. ENG-12541 was exactly that shape.
    #[test]
    fn the_identity_carries_the_question_and_the_arguments() {
        let _held = crate::eval::globals_shared();
        let module = ix_kernel::hash::tagged("module", &[b"one source"]);
        // Any settings will do: this test varies the question and the
        // arguments and holds the settings constant, so reading the process
        // ones only made its answer depend on other tests (ENG-12939).
        let settings = crate::eval::Settings::default();
        let none = Arguments::none();
        assert_ne!(
            EvalId::of(&module, &settings, &none, &select("a", RenderMode::Raw),),
            EvalId::of(&module, &settings, &none, &select("b", RenderMode::Raw),),
            "two attribute paths of one module are one cache entry"
        );
        let one = Arguments::new(vec![Argument::Json(r#"{"nodes":{"a":1}}"#.to_owned())]);
        let two = Arguments::new(vec![Argument::Json(r#"{"nodes":{"b":1}}"#.to_owned())]);
        let question = select("packages.x86_64-linux.hello", RenderMode::Raw);
        assert_ne!(
            EvalId::of(&module, &settings, &one, &question,),
            EvalId::of(&module, &settings, &two, &question,),
            "two flakes asking one module for one attribute are one cache entry: \
             the applied arguments are not reaching EvalId"
        );
        assert_ne!(
            EvalId::of(&module, &settings, &none, &question,),
            EvalId::of(&module, &settings, &one, &question,),
            "applying an argument and applying none are one cache entry"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Host, RealFs};
    use crate::modcache::ModuleCache;
    use ix_kernel::cas::MemoryCas;

    fn once(source: &str) -> (EvalResult, Reuse) {
        once_under(&crate::eval::Settings::default(), source)
    }

    fn once_under(settings: &crate::eval::Settings, source: &str) -> (EvalResult, Reuse) {
        let cas = MemoryCas::new();
        let cas: &dyn Cas = &cas;
        let mut results = ResultCache::new(cas);
        let mut vm = Vm::with_settings(settings.clone());
        evaluate(
            &mut vm,
            Some(&mut results),
            &RealFs,
            source,
            "/base",
            Origin::String,
        )
    }

    /// ENG-12540 (1). The memoising path builds its own VM, and built it
    /// unconfigured, so `--max-call-depth` bounded a run without
    /// `eval-cache-dir` and did not bound it with one. Held at the ceiling
    /// rather than at the constructor because a getter on `Vm` would only say
    /// the field was written, not that it stopped anything.
    #[test]
    fn a_memoised_evaluation_runs_under_the_configured_ceiling() {
        // The ceiling is a value the VM is built with, not a global this
        // test moves while everything else reads it (ENG-12939).
        let bounded = crate::eval::Settings {
            max_call_depth: 50,
            ..crate::eval::Settings::default()
        };
        let (result, _) = once_under(
            &bounded,
            "let f = n: if n == 0 then 0 else 1 + f (n - 1); in f 200",
        );
        assert_eq!(
            (result.status.as_str(), result.value.as_str()),
            (EVAL, "stack overflow; max-call-depth exceeded")
        );
    }

    /// A cache hit reproduces the warnings, not just the value. Before this,
    /// a run served from the memo table stayed silent about something the run
    /// that filled it said out loud, so `eval-cache-dir` decided how much the
    /// evaluator told its reader.
    #[test]
    fn a_served_result_repeats_the_warnings_the_first_run_emitted() {
        use std::cell::RefCell;

        let _held = crate::eval::globals_shared();

        struct Warner {
            heard: RefCell<Vec<String>>,
        }
        impl Host for Warner {
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
            ) -> Result<Vec<(String, crate::host::FileType)>, String> {
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
            fn file_type(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Option<crate::host::FileType>, String> {
                Err("no".to_owned())
            }
            fn warn(&self, message: &str) {
                self.heard.borrow_mut().push(message.to_owned());
            }
        }

        // `builtins.derivationStrict` warns about attributes
        // `__structuredAttrs` disables; that is the one warning the evaluator
        // emits today, so it is what this drives.
        let source = r#"(builtins.derivationStrict {
            name = "w"; system = "x86_64-linux"; builder = "/bin/sh";
            __structuredAttrs = true; allowedReferences = [ ];
        }).out"#;

        let host = Warner {
            heard: RefCell::new(Vec::new()),
        };
        let cas = MemoryCas::new();
        let cas: &dyn Cas = &cas;
        let mut results = ResultCache::new(cas);
        let mut vm = Vm::with_settings(crate::eval::settings_with_store());
        let (first, first_reuse) = evaluate(
            &mut vm,
            Some(&mut results),
            &host,
            source,
            "/base",
            Origin::String,
        );
        let said_first = core::mem::take(&mut *host.heard.borrow_mut());
        let (second, second_reuse) = evaluate(
            &mut vm,
            Some(&mut results),
            &host,
            source,
            "/base",
            Origin::String,
        );
        let said_second = host.heard.borrow().clone();

        assert!(!first_reuse.memo_hit);
        assert!(
            second_reuse.memo_hit,
            "the second run was not served from the memo"
        );
        assert_eq!(
            first, second,
            "the served result differs from the computed one"
        );
        assert!(
            !said_first.is_empty(),
            "this expression no longer warns, so the test proves nothing; pick one that does. \
             First run said: {said_first:?}, status {}",
            first.status
        );
        assert_eq!(
            said_first, said_second,
            "a served result said something different from the run that filled the cache"
        );
    }

    /// One Ctrl-C must not poison an expression for ever.
    ///
    /// An interrupt arrives as an ordinary `EvalError::Eval` carrying
    /// cppnix's wording, so the recorder could not tell it from an expression
    /// that genuinely fails, and stored it. With `eval-cache-dir` set that
    /// made the interrupted expression answer "interrupted by the user" on
    /// every later run, out of a cache the operator had no reason to suspect.
    ///
    /// Found by writing the interrupt row in `capi::SETTER_ACCOUNTING`: the
    /// row had to say why the setting could not change an answer, and it
    /// could not.
    #[test]
    fn an_interrupted_evaluation_is_not_memoised() {
        use std::cell::Cell;
        use std::rc::Rc;

        // Long enough to cross an interrupt stride, and pure, so the only
        // thing that can stop it is the hook.
        let source = "builtins.foldl' (a: b: a + b) 0 (builtins.genList (x: x) 200000)";
        let cas = MemoryCas::new();
        let cas: &dyn Cas = &cas;
        let mut results = ResultCache::new(cas);

        // The flag reaches exactly one machine, so this test arms nothing
        // another test can see and needs no lock over the process. It used
        // to take `globals_moving()`, because the hook was a process-wide
        // slot and arming it here disarmed whoever else held it.
        let armed = Rc::new(Cell::new(true));
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        vm.set_interrupt({
            let armed = Rc::clone(&armed);
            Box::new(move || armed.get())
        });
        let (interrupted, _) = evaluate(
            &mut vm,
            Some(&mut results),
            &RealFs,
            source,
            "/base",
            Origin::String,
        );
        armed.set(false);

        assert_eq!(
            (interrupted.status.as_str(), interrupted.value.as_str()),
            (EVAL, "interrupted by the user"),
            "the hook did not interrupt, so this test proves nothing"
        );

        // The same expression again, uninterrupted, on the *same* machine:
        // the interrupt flag is per run, and a VM that stayed flagged would
        // quietly stop memoising everything after the first Ctrl-C.
        let mut vm = vm;
        let (second, reuse) = evaluate(
            &mut vm,
            Some(&mut results),
            &RealFs,
            source,
            "/base",
            Origin::String,
        );
        assert!(
            !reuse.memo_hit,
            "the interrupted run was memoised, so this expression is poisoned"
        );
        assert_eq!(
            (second.status.as_str(), second.value.as_str()),
            (OK, "19999900000")
        );

        // And the machine went back to memoising. Refusing to record is the
        // fix; refusing for ever afterwards would be a cache that silently
        // stops working after the first Ctrl-C, which the assertions above
        // cannot see because they only check that answers stay right.
        let (third, reuse) = evaluate(
            &mut vm,
            Some(&mut results),
            &RealFs,
            source,
            "/base",
            Origin::String,
        );
        assert!(
            reuse.memo_hit,
            "the second run was not recorded either, so one interrupt stopped \
             this machine memoising anything ever again"
        );
        assert_eq!(third.value, second.value);
    }

    /// The verifier's hit side, against a deliberately poisoned row.
    ///
    /// A cache is the one component that cannot be checked by reading its
    /// output, because its output is by construction whatever it was told to
    /// say. So the check is to evaluate anyway and compare, and the way to
    /// know the check works is to file a wrong answer under a key and watch
    /// it get caught.
    #[test]
    fn the_verifier_catches_a_poisoned_row() -> Result<(), Box<dyn core::error::Error>> {
        use crate::readset::Severity;

        let _held = crate::eval::globals_shared();
        let cas = MemoryCas::new();
        let cas: &dyn Cas = &cas;
        let mut results = ResultCache::new(cas);
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let source = "1 + 2";

        // Poison first, then evaluate. Recording after an honest run is a
        // no-op: the memo table is `Keyed`, so a second record under a key
        // that already has a row reuses the row rather than replacing it --
        // which is correct, and means the poison has to get there first.
        // `1 + 2` asks the host nothing, so its read set is empty and the key
        // an honest run computes is the one written here.
        let module_id = *vm.compile(source, "/base", Origin::String)?.id.hash();
        // The same settings the VM below runs under. `evaluate` builds the
        // identity from `vm.settings()`, so a key minted from
        // `Settings::current()` here would only match by luck (ENG-12939).
        let identity = EvalId::of(
            &module_id,
            &crate::eval::Settings::default(),
            &Arguments::none(),
            &Question::Whole {
                render: RenderMode::Plain,
            },
        );
        let read_set = crate::readset::ReadSet::default();
        results.record(
            &identity,
            &read_set,
            &EvalResult {
                status: OK.to_owned(),
                value: "4".to_owned(),
                ..EvalResult::default()
            },
            &RealFs,
            &crate::eval::Settings::default(),
        )?;

        results.set_verify_rate(1);
        let (served, reuse) = evaluate(
            &mut vm,
            Some(&mut results),
            &RealFs,
            source,
            "/base",
            Origin::String,
        );
        let counts = results.verifier();
        let complaints = results.take_corruption();

        assert!(
            reuse.memo_hit,
            "the poisoned row was not served, so nothing was verified"
        );
        assert_eq!(served.value, "4", "the poison did not take");
        assert_eq!(
            counts.hits_disagreed, 1,
            "the verifier did not notice: {counts:?}"
        );

        let shouted: Vec<_> = complaints
            .iter()
            .filter(|c| c.severity == Severity::Error)
            .collect();
        assert_eq!(
            shouted.len(),
            1,
            "expected one error-priority complaint: {complaints:?}"
        );
        let Some(shouted) = shouted.first() else {
            unreachable!("no error-priority complaint: {complaints:?}");
        };
        // The memo key and both answers have to be in the message. Without the
        // key the reader cannot find the row; without both answers they cannot
        // tell which side is wrong.
        assert!(
            shouted.message.contains(&identity.as_hash().to_hex()),
            "the complaint does not name the memo key: {}",
            shouted.message
        );
        assert!(shouted.message.contains("\"ok/4\""), "{}", shouted.message);
        assert!(shouted.message.contains("\"ok/3\""), "{}", shouted.message);
        Ok(())
    }

    /// The verifier's miss side, which the ticket did not ask for and which
    /// covers the failures this repo has actually had.
    ///
    /// Both real cache bugs here served no wrong answer at all: a witness
    /// decoder that rejected its own encoder's tag, and a sweep that deleted
    /// every witness. Each showed up as a miss, and a miss is indistinguishable
    /// from a cold cache unless something expected a hit.
    #[test]
    fn the_verifier_notices_a_record_it_cannot_find_again() {
        let _held = crate::eval::globals_shared();
        let cas = MemoryCas::new();
        let cas: &dyn Cas = &cas;
        let mut results = ResultCache::new(cas);
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        results.set_verify_rate(1);

        let (result, _) = evaluate(
            &mut vm,
            Some(&mut results),
            &RealFs,
            "1 + 2",
            "/base",
            Origin::String,
        );
        assert_eq!(result.value, "3");

        let counts = results.verifier();
        assert_eq!(
            (counts.records_checked, counts.records_not_replayable),
            (1, 0),
            "a freshly recorded result was not findable in the same process: {counts:?}"
        );
    }

    /// Off by default, and off means no cost.
    ///
    /// A verifier that ran when nobody asked would double the cost of every
    /// hit, which is the entire saving the cache exists for.
    #[test]
    fn verification_is_off_unless_asked_for() {
        let _held = crate::eval::globals_shared();
        let cas = MemoryCas::new();
        let cas: &dyn Cas = &cas;
        let mut results = ResultCache::new(cas);
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        for _ in 0..3 {
            evaluate(
                &mut vm,
                Some(&mut results),
                &RealFs,
                "1 + 2",
                "/base",
                Origin::String,
            );
        }
        assert_eq!(
            results.verifier(),
            crate::readset::VerifierCounts::default()
        );
    }

    /// A sampled hit must not make the reader hear a warning twice.
    ///
    /// The verification run evaluates the same expression, which emits the
    /// same warnings; forwarding them would mean turning the verifier on
    /// changed what the evaluator says, which is the thing this whole lane
    /// exists to prevent.
    #[test]
    fn verifying_a_hit_does_not_repeat_its_warnings() {
        use std::cell::RefCell;

        let _held = crate::eval::globals_shared();

        struct Warner(RefCell<Vec<String>>);
        impl Host for Warner {
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
            ) -> Result<Vec<(String, crate::host::FileType)>, String> {
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
            fn file_type(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Option<crate::host::FileType>, String> {
                Err("no".to_owned())
            }
            fn warn(&self, message: &str) {
                self.0.borrow_mut().push(message.to_owned());
            }
        }

        let source = r#"(builtins.derivationStrict {
            name = "w"; system = "x86_64-linux"; builder = "/bin/sh";
            __structuredAttrs = true; allowedReferences = [ ];
        }).out"#;

        let host = Warner(RefCell::new(Vec::new()));
        let cas = MemoryCas::new();
        let cas: &dyn Cas = &cas;
        let mut results = ResultCache::new(cas);
        let mut vm = Vm::with_settings(crate::eval::settings_with_store());

        evaluate(
            &mut vm,
            Some(&mut results),
            &host,
            source,
            "/base",
            Origin::String,
        );
        let said_uncached = core::mem::take(&mut *host.0.borrow_mut());
        assert!(!said_uncached.is_empty(), "this expression no longer warns");

        results.set_verify_rate(1);
        let (_, reuse) = evaluate(
            &mut vm,
            Some(&mut results),
            &host,
            source,
            "/base",
            Origin::String,
        );
        let said_verified = host.0.borrow().clone();

        assert!(reuse.memo_hit);
        assert_eq!(results.verifier().hits_disagreed, 0);
        assert_eq!(
            said_verified, said_uncached,
            "verifying a hit changed what the evaluator said out loud"
        );
    }

    #[test]
    fn a_value_comes_back_as_ok() {
        let (result, reuse) = once("1 + 2");
        assert_eq!((result.status.as_str(), result.value.as_str()), (OK, "3"));
        assert!(!reuse.memo_hit);
    }

    /// The class has to survive memoisation, or a served `throw` would be
    /// reported as a plain evaluation error on the second run only.
    #[test]
    fn language_failure_classes_round_trip_through_a_memoised_result() {
        let _held = crate::eval::globals_shared();
        for (source, want) in [
            ("throw \"boom\"", THROWN),
            ("assert false; 1", ASSERTION),
            ("let a = a; in a", EVAL),
        ] {
            let cas = MemoryCas::new();
            let cas: &dyn Cas = &cas;
            let mut results = ResultCache::new(cas);
            let mut vm = Vm::with_settings(crate::eval::Settings::default());
            let go = |vm: &mut Vm, r: &mut ResultCache<'_, dyn Cas>| {
                evaluate(vm, Some(r), &RealFs, source, "/base", Origin::String)
            };
            let (first, _) = go(&mut vm, &mut results);
            let (second, reuse) = go(&mut vm, &mut results);
            assert_eq!(first.status, want, "source {source}");
            assert_eq!(
                second.status, want,
                "class lost on the cached path: {source}"
            );
            assert_eq!(first.value, second.value, "message lost: {source}");
            assert!(reuse.memo_hit, "second run of {source} was not served");
            // And the class survives the trip back out to an embedder.
            assert!(error_of(&second).is_some());
        }
    }

    /// `builtins.storePath` keeps the context its argument carried and adds
    /// the store object it validated, without realising anything: cppnix's
    /// `prim_storePath` calls `coerceToPath` and never `realiseContext`
    /// (primops.cc:2029-2047). The realise hook here denies every build, so
    /// a route that realised the argument's `.drv` context would fail the
    /// evaluation rather than answer.
    #[test]
    fn store_path_preserves_input_context_without_realising_it() {
        use crate::host::{FnHost, StoreError, StorePathResult};

        fn validate(path: &crate::value2::PathValue) -> Result<StorePathResult, String> {
            Ok(StorePathResult {
                path: path.to_string(),
                store_path: path.to_string(),
            })
        }
        fn deny(
            _context: &[crate::value2::ContextElem],
        ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
            Err(StoreError::ImportFromDerivation(
                "a build would be import from derivation".to_owned(),
            ))
        }
        fn present(_path: &str) -> Result<(), String> {
            Ok(())
        }
        let host = FnHost {
            store_path: Some(validate),
            store_ensure: Some(present),
            realise: Some(deny),
            ..FnHost::default()
        };
        let source = concat!(
            "builtins.getContext (builtins.storePath (builtins.appendContext ",
            "\"/nix/store/00000000000000000000000000000000-a\" ",
            "{ \"/nix/store/11111111111111111111111111111111-x.drv\" = { outputs = [ \"out\" ]; }; }))"
        );
        let cas = MemoryCas::new();
        let cas: &dyn Cas = &cas;
        let mut results = ResultCache::new(cas);
        let mut vm = Vm::with_settings(crate::eval::settings_with_store());
        let (result, _) = evaluate(
            &mut vm,
            Some(&mut results),
            &host,
            source,
            "/base",
            Origin::String,
        );
        assert_eq!(
            result.status, OK,
            "storePath realised or failed: {}",
            result.value
        );
        assert!(
            result
                .value
                .contains("11111111111111111111111111111111-x.drv"),
            "the argument's derivation context was dropped: {}",
            result.value
        );
        assert!(
            result.value.contains("00000000000000000000000000000000-a")
                && result.value.contains("path = true"),
            "the validated store object is not in the result's context: {}",
            result.value
        );
    }

    /// An IFD refusal is its own exception class before and after a persistent
    /// memo hit. The second cache is fresh, so its hit can only come from
    /// replaying the witness and serving the row recorded by the first one.
    #[test]
    fn a_served_ifd_failure_keeps_its_typed_error_class() -> Result<(), Box<dyn core::error::Error>>
    {
        use crate::host::{FnHost, StoreError};

        const DENIED: &str = "import from derivation is disabled";
        const WITH_CONTEXT: &str = r#"builtins.appendContext "/nix/store/00000000000000000000000000000000-out" { "/nix/store/11111111111111111111111111111111-x.drv" = { outputs = [ "out" ]; }; }"#;

        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn ensure_path(_path: &str) -> Result<(), String> {
            Ok(())
        }

        fn deny_ifd(
            _context: &[crate::value2::ContextElem],
        ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
            Err(StoreError::ImportFromDerivation(DENIED.to_owned()))
        }

        fn assert_ifd(result: &EvalResult) {
            assert_eq!(result.status, IMPORT_FROM_DERIVATION);
            assert!(matches!(
                error_of(result),
                Some(EvalError::Eval(
                    ErrKind::ImportFromDerivation,
                    message,
                    _
                )) if message == DENIED
            ));
        }

        let _held = crate::eval::globals_shared();
        let scratch =
            Scratch(std::env::temp_dir().join(format!("ixe-served-ifd-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&scratch.0);
        let store = crate::store::Store::open(&scratch.0)?;
        let settings = crate::eval::settings_with_store();
        let source = format!("builtins.readFile ({WITH_CONTEXT})");
        let host = FnHost {
            store_ensure: Some(ensure_path),
            realise: Some(deny_ifd),
            ..FnHost::default()
        };

        let first = {
            let mut results = ResultCache::persistent(&store);
            let mut vm = Vm::with_modules(settings.clone(), ModuleCache::persistent(store.clone()));
            evaluate(
                &mut vm,
                Some(&mut results),
                &host,
                &source,
                "/base",
                Origin::String,
            )
        };
        assert!(!first.1.memo_hit, "the recording run was already served");
        assert_ifd(&first.0);

        let served = {
            let mut results = ResultCache::persistent(&store);
            let mut vm = Vm::with_modules(settings, ModuleCache::persistent(store.clone()));
            evaluate(
                &mut vm,
                Some(&mut results),
                &host,
                &source,
                "/base",
                Origin::String,
            )
        };
        assert!(
            served.1.memo_hit,
            "the fresh cache did not serve the IFD row"
        );
        assert_eq!(served.0.value, first.0.value);
        assert_ifd(&served.0);
        Ok(())
    }

    #[test]
    fn the_second_evaluation_is_served_without_running_the_vm() {
        let _held = crate::eval::globals_shared();
        let cas = MemoryCas::new();
        let cas: &dyn Cas = &cas;
        let mut results = ResultCache::new(cas);
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let first = evaluate(
            &mut vm,
            Some(&mut results),
            &RealFs,
            "1 + 2",
            "/base",
            Origin::String,
        );
        let second = evaluate(
            &mut vm,
            Some(&mut results),
            &RealFs,
            "1 + 2",
            "/base",
            Origin::String,
        );
        assert!(!first.1.memo_hit);
        assert!(second.1.memo_hit);
        assert!(second.1.compile_hit);
        assert_eq!(second.0.value, "3");
    }

    /// Without a result cache the VM runs every time, and that must still be
    /// correct rather than merely slower.
    #[test]
    fn compilation_caching_alone_still_answers() {
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        for _ in 0..3 {
            let (result, _) = evaluate(&mut vm, None, &RealFs, "1 + 2", "/base", Origin::String);
            assert_eq!(result.value, "3");
        }
        assert_eq!(vm.modules().hits(), 2);
    }

    /// The compile cache must not change what a bad expression is reported
    /// as. It did: every compile failure came back through the kernel's
    /// effect channel as a plain evaluation error reading "effect in domain
    /// <64 hex> failed: Parse(...)", so setting `eval-cache-dir` turned a
    /// parse error into an eval error and an unimplemented construct into one
    /// too -- which would have moved lang-diff's `unimplemented` count
    /// depending on a setting that is supposed to change only speed.
    #[test]
    fn a_compile_failure_is_classified_the_same_with_and_without_a_cache() {
        for source in [
            "1 +",                       // parse
            "nosuchvariable",            // undefined variable
            "builtins.scopedImport",     // unimplemented
            "builtins.unsafeGetAttrPos", // implemented, so: a success
            // `CompileError::Eval`: a source the compiler rejects with a
            // plain error rather than a parse error. It is the variant a
            // reader is least likely to think of, and it is the one the
            // cached arm rendered with `{:?}` for as long as `compile_failure`
            // had a catch-all: `Eval("experimental Nix feature ...")`, Rust
            // debug syntax and all, against a bare message uncached.
            // `maintainers/ix/cache-semantics-gate.sh` saw it on three corpus
            // files under all seven configurations; this is the same defect
            // in one line.
            "1 |> 2",
        ] {
            // The same settings `once` gives its VM, so the two arms differ
            // only in whether a cache is present (ENG-12939).
            let uncached = crate::eval::eval_str_with(
                source,
                "/base",
                Origin::String,
                &crate::eval::Settings::default(),
            );
            let (cached, _) = once(source);
            let cached_error = error_of(&cached);
            let (want_status, want_message) = match &uncached {
                Err(error) => {
                    let r = result_of(error);
                    (r.status, r.value)
                }
                Ok(value) => (OK.to_owned(), value.clone()),
            };
            let (got_status, got_message) = match &cached_error {
                Some(error) => {
                    let r = result_of(error);
                    (r.status, r.value)
                }
                // A success round-trips through `error_of` to `None`, which
                // carries no value, so the memoised result is compared
                // directly. Reading an empty placeholder here instead passes
                // only while every source in the list fails -- which is what
                // this branch did until `builtins.unsafeGetAttrPos` was
                // implemented (ENG-12591) and one of them started succeeding.
                // A fixture list that has to keep failing is a fixture list
                // that rots, so a success is now one of the cases.
                None => (cached.status.clone(), cached.value.clone()),
            };
            assert_eq!(
                (got_status.as_str(), got_message.as_str()),
                (want_status.as_str(), want_message.as_str()),
                "the cache changed how {source} is reported"
            );
        }
    }

    #[test]
    fn an_ok_result_maps_to_no_error() {
        assert!(
            error_of(&EvalResult {
                status: OK.to_owned(),
                value: "3".to_owned(),
                ..EvalResult::default()
            })
            .is_none()
        );
    }

    /// A status written by a build that knew a class this one does not must
    /// not be trusted into the wrong exception.
    #[test]
    fn an_unknown_status_degrades_to_a_plain_eval_error() {
        let error = error_of(&EvalResult {
            status: "from-the-future".to_owned(),
            value: "x".to_owned(),
            ..EvalResult::default()
        });
        assert!(matches!(error, Some(EvalError::Eval(ErrKind::Eval, _, _))));
    }
}

#[cfg(test)]
mod eng12543_e2e_probe {
    use super::*;
    use crate::host::{FileType, Host};
    use ix_kernel::cas::MemoryCas;
    use std::cell::RefCell;

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
        fn get_env(&self, _n: &str) -> Option<String> {
            None
        }
    }

    /// Is the replay read reachable from the public path, or does the
    /// settings key already stop it? This decides whether ENG-12543 is live
    /// or latent, and the answer belongs in the report either way.
    #[test]
    fn probe_end_to_end_pure_eval_lookup() {
        let host = Counting {
            reads: RefCell::new(Vec::new()),
        };
        let cas = MemoryCas::new();
        let cas: &dyn Cas = &cas;
        let mut results = ResultCache::new(cas);
        let source = "builtins.readFile /etc/shadow";

        // The two regimes as two values. Each `Vm` carries the one it runs
        // under, so this test states the axis it varies instead of moving a
        // process global that every other test is reading (ENG-12939).
        let impure = crate::eval::Settings::default();
        let pure = crate::eval::Settings {
            pure_eval: true,
            ..impure.clone()
        };

        // Fill the cache with reads allowed.
        let mut vm = Vm::with_settings(impure);
        let (first, _) = evaluate(
            &mut vm,
            Some(&mut results),
            &host,
            source,
            "/base",
            Origin::String,
        );
        let recorded = core::mem::take(&mut *host.reads.borrow_mut());
        eprintln!("PROBE fill: status={} reads={recorded:?}", first.status);

        // Now look it up with reads forbidden.
        let mut vm = Vm::with_settings(pure);
        let (second, reuse) = evaluate(
            &mut vm,
            Some(&mut results),
            &host,
            source,
            "/base",
            Origin::String,
        );
        let during_lookup = host.reads.borrow().clone();

        eprintln!(
            "PROBE lookup under pure-eval: status={} value={} memo_hit={} reads={during_lookup:?}",
            second.status, second.value, reuse.memo_hit
        );
        assert!(
            during_lookup.is_empty(),
            "a pure-eval lookup read the filesystem: {during_lookup:?}"
        );
    }
}
