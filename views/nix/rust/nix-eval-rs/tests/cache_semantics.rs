//! `eval-cache-dir` is allowed to change speed and nothing else.
//!
//! Every test here evaluates the same source twice through
//! [`session::evaluate_once`], once with a cache directory and once without,
//! and asserts the two answers are the same bytes -- values, error classes,
//! error messages and warnings alike. A divergence found here is a wrong
//! answer served to a user who set a performance option, which is the worst
//! shape a caching bug can have: it appears only on the machine that turned
//! the option on, and the answer looks exactly like a right one.
//!
//! These live in `tests/` rather than beside the code because the property is
//! about the crate's outside edge. The unit tests in `session` and `readset`
//! can reach past `evaluate_once` and set up a cache by hand; the thing a user
//! configures is `eval-cache-dir`, and this is the only entry point that
//! reads it.
//!
//! # The process globals make these tests serial
//!
//! `max-call-depth`, the store directory and the version string are process
//! state (`eval.rs`), so two tests changing them at once would read each
//! other's settings. [`serial`] takes one lock for the whole crate. It is a
//! test-harness detail and not a claim about the evaluator, which is
//! single-threaded per VM.
#![expect(
    clippy::expect_used,
    reason = "an integration test crate: its helpers abort loudly like the tests they serve; clippy.toml's allow-expect-in-tests covers `#[test]` functions and `#[cfg(test)]` items, and a helper in `tests/` is neither"
)]

use nix_eval_rs::eval::{self, EvalError};
use nix_eval_rs::modcache::ModuleCache;
use nix_eval_rs::session;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

static SERIAL: Mutex<()> = Mutex::new(());

/// Every hit in these tests is checked against a fresh evaluation, and every
/// record is looked up again. Free at this scale, and it makes each assertion
/// below a verifier assertion too: a cache that agrees with itself but not
/// with evaluating fails here as well as in the verifier's own tests.
const VERIFY_EVERY: u32 = 1;

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A scratch directory that removes itself, so a failing test does not leave
/// a store behind for the next run to hit in.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ixe-cache-semantics-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What one evaluation produced, flattened to the bytes a comparison can be
/// made on. The error class is kept apart from the message because an
/// embedder raises a different exception for each, and a cache that preserved
/// the text while losing the class would pass a message-only comparison.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    class: &'static str,
    text: String,
}

fn outcome(answer: &Result<String, EvalError>) -> Outcome {
    match answer {
        Ok(value) => Outcome {
            class: "ok",
            text: value.clone(),
        },
        Err(EvalError::Unimplemented(refusal)) => Outcome {
            class: "unimplemented",
            text: refusal.detail.clone(),
        },
        Err(EvalError::Parse(message)) => Outcome {
            class: "parse",
            text: message.clone(),
        },
        Err(EvalError::Eval(kind, message, _)) => Outcome {
            class: match kind {
                nix_eval_rs::vm::ErrKind::Eval => "eval",
                nix_eval_rs::vm::ErrKind::Thrown => "thrown",
                nix_eval_rs::vm::ErrKind::Assertion => "assertion",
                nix_eval_rs::vm::ErrKind::ImportFromDerivation => "import-from-derivation",
                nix_eval_rs::vm::ErrKind::MissingArgument => "missing-argument",
            },
            text: message.clone(),
        },
    }
}

fn uncached(source: &str) -> Outcome {
    let mut vm = nix_eval_rs::vm::Vm::from_process_settings();
    let (answer, warnings) = session::evaluate_once(
        &mut vm,
        &nix_eval_rs::host::RealFs,
        source,
        "/base",
        nix_eval_rs::compile::Origin::String,
        true,
        VERIFY_EVERY,
    );
    assert!(
        warnings.is_empty(),
        "cache complaints without a cache: {warnings:?}"
    );
    outcome(&answer)
}

fn cached(source: &str, dir: &std::path::Path) -> Outcome {
    // A store complaint is not an answer, but it means the run under test was
    // not the run intended: reporting it beats silently comparing a fallback.
    let (store, unopened) = session::open_cache(Some(dir), 0);
    assert!(unopened.is_none(), "the cache did not open: {unopened:?}");
    let store = store.expect("open_cache returns a store when it does not complain");
    let mut vm = nix_eval_rs::vm::Vm::with_modules(
        nix_eval_rs::eval::Settings::current(),
        ModuleCache::persistent(store),
    );
    let (answer, warnings) = session::evaluate_once(
        &mut vm,
        &nix_eval_rs::host::RealFs,
        source,
        "/base",
        nix_eval_rs::compile::Origin::String,
        true,
        VERIFY_EVERY,
    );
    assert!(warnings.is_empty(), "the cache complained: {warnings:?}");
    outcome(&answer)
}

/// ENG-12540 (1). The cached path builds its own VM and, before this test,
/// built it with the crate's compiled-in default ceiling rather than the one
/// the embedder set through `ixe_set_max_call_depth`. So a recursion the
/// uncached arm refuses ran to completion with a cache directory configured:
/// `--max-call-depth 50` was silently ignored by `eval-cache-dir`.
#[test]
fn the_call_depth_ceiling_reaches_the_cached_path() {
    let _guard = serial();
    let scratch = Scratch::new("depth");
    let source = "let f = n: if n == 0 then 0 else 1 + f (n - 1); in f 200";

    eval::set_max_call_depth(50);
    let without = uncached(source);
    let with = cached(source, &scratch.0);
    eval::set_max_call_depth(nix_eval_rs::vm::DEFAULT_MAX_CALL_DEPTH);

    assert_eq!(
        without.class, "eval",
        "the uncached arm should refuse `f 200` under a ceiling of 50, but said {without:?}"
    );
    assert_eq!(
        with, without,
        "eval-cache-dir changed what a call-depth ceiling means"
    );
}

/// ENG-12540 (3). `RecordingHost` records a `CopyToStore` question for every
/// path interpolated into a string, and the witness decoder rejected that
/// question's tag. A rejected witness is a miss, so every expression
/// containing `"${./x}"` re-evaluated on every run for ever -- silently, and
/// without even registering as a wasted replay, because the bail happened
/// before the replay.
///
/// The observable is a hit. Whether the decoder works is not visible in the
/// answer, which was right the whole time; it is visible in whether a second
/// evaluation has to run the VM again.
#[test]
fn an_expression_that_interpolates_a_path_can_cache_hit() -> Result<(), Box<dyn core::error::Error>>
{
    use nix_eval_rs::host::RealFs;
    use nix_eval_rs::readset::ResultCache;
    use nix_eval_rs::vm::Vm;

    let _guard = serial();
    let scratch = Scratch::new("interpolate");
    let file = scratch.0.join("src.txt");
    assert!(std::fs::create_dir_all(&scratch.0).is_ok());
    assert!(std::fs::write(&file, "hello").is_ok());
    // No store hook is installed, so the interpolation is refused -- but it
    // is refused *after* `copy_to_store` is asked, which is what puts a
    // `CopyToStore` question in the witness. That is the entry whose tag the
    // decoder rejected, and the refusal is a memoisable answer like any other.
    let source = format!("\"${{{}}}\"", file.display());

    let store = nix_eval_rs::store::Store::open(scratch.0.join("store"))?;

    let go = || {
        let mut results = ResultCache::persistent(&store);
        let mut vm = Vm::with_modules(
            nix_eval_rs::eval::Settings::default(),
            ModuleCache::persistent(store.clone()),
        );
        let (result, reuse) = session::evaluate(
            &mut vm,
            Some(&mut results),
            &RealFs,
            &source,
            "/base",
            nix_eval_rs::compile::Origin::String,
        );
        (result, reuse, results.wasted_replays())
    };

    let (first, first_reuse, _) = go();
    assert_eq!(
        first.status, "unimplemented",
        "expected the no-store refusal, got {first:?}"
    );
    assert!(!first_reuse.memo_hit, "the first run cannot be a hit");

    let (second, second_reuse, wasted) = go();
    assert_eq!(
        second, first,
        "the served answer differs from the computed one"
    );
    assert!(
        second_reuse.memo_hit,
        "a witness naming a CopyToStore question could not be read back, so this \
         expression can never cache-hit (wasted_replays={wasted}, which stays 0 \
         because the bail happens before the replay)"
    );
    Ok(())
}

/// ENG-12541. The store directory is hashed into every path
/// `builtins.derivationStrict` computes, and it lived in a process global
/// outside the memo key, so one cache directory shared between two stores
/// would serve the first store's `outPath` to the second.
///
/// Held on the fingerprint rather than on a derivation because the store
/// directory is a `OnceLock` and cannot be moved twice in one process. The
/// exhaustive field-by-field half is `eval::tests::every_setting_is_in_the_memo_key`;
/// this is the end-to-end half, naming the setting whose divergence is a
/// wrong output path.
#[test]
fn two_store_directories_do_not_share_a_memo_key() {
    let _guard = serial();
    let base = eval::Settings {
        store_dir: Some("/nix/store".to_owned()),
        nix_version: Some("2.34.7".to_owned()),
        host_build_identity: Some("/test/host-a".to_owned()),
        current_system: Some("x86_64-linux".to_owned()),
        max_call_depth: 10_000,
        pure_eval: false,
        restrict_eval: false,
        builtin_features: nix_eval_rs::eval::BuiltinFeatures::ALL,
        path_reads: nix_eval_rs::purity::PathReads::Direct,
        trace_verbose: false,
        abort_on_warn: false,
        home_dir: Some("/home/nixer".to_owned()),
        ca_derivations: false,
        allow_import_from_derivation: true,
        allowed_uris: None,
        repair: false,
        blake3_hashes: false,
        lint_url_literals: nix_eval_rs::eval::Diagnose::Ignore,
        lint_short_path_literals: nix_eval_rs::eval::Diagnose::Ignore,
        lint_absolute_path_literals: nix_eval_rs::eval::Diagnose::Ignore,
        pipe_operators: false,
        parse_toml_timestamps: false,
    };
    let elsewhere = eval::Settings {
        store_dir: Some("/tmp/other/store".to_owned()),
        ..base.clone()
    };
    assert_ne!(
        base.fingerprint(),
        elsewhere.fingerprint(),
        "two store directories share a memo key, so a cache shared across \
         stores serves wrong outPaths"
    );

    // And the identity a result is filed under moves with it, which is the
    // thing the cache actually uses.
    let module = ix_kernel::hash::tagged("module", &[b"same source"]);
    assert_ne!(
        nix_eval_rs::readset::EvalId::of(&module, &base, &none(), &whole(),),
        nix_eval_rs::readset::EvalId::of(&module, &elsewhere, &none(), &whole(),),
        "the same module under two store directories is one cache entry"
    );
}

/// The question these identity tests hold constant.
///
/// They vary the settings and require the row to move; the question has its
/// own test (`session::tests::each_question_is_its_own_memo_row`), so pinning
/// it here keeps each assertion about one axis.
fn none() -> nix_eval_rs::session::Arguments {
    nix_eval_rs::session::Arguments::none()
}

fn whole() -> nix_eval_rs::session::Question {
    nix_eval_rs::session::Question::Whole {
        render: nix_eval_rs::session::RenderMode::Plain,
    }
}

#[derive(Default)]
struct DrvHostState {
    write_derivation_calls: usize,
    store_text_calls: usize,
    writes: Vec<(String, String, Vec<String>)>,
    present: std::collections::BTreeMap<String, String>,
}

static DRV_HOST_STATE: Mutex<Option<DrvHostState>> = Mutex::new(None);

fn with_drv_host_state<T>(f: impl FnOnce(&mut DrvHostState) -> T) -> T {
    let mut slot = DRV_HOST_STATE
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    f(slot.get_or_insert_with(DrvHostState::default))
}

fn write_drv(name: &str, aterm: &str) -> Result<String, String> {
    // As the embedder does: the reference set comes from the parsed ATerm.
    let references = nix_eval_rs::drv::parse(aterm)
        .map_err(|e| format!("unparseable ATerm: {e:?}"))?
        .references();
    let path = nix_eval_rs::drvpath::text_store_path(
        "/nix/store",
        &format!("{name}.drv"),
        aterm,
        &references,
    );
    with_drv_host_state(|state| {
        state.write_derivation_calls += 1;
        state
            .writes
            .push((name.to_owned(), aterm.to_owned(), references.to_vec()));
        state.present.insert(path.clone(), aterm.to_owned());
    });
    Ok(path)
}

fn store_text(name: &str, contents: &str, references: &[String]) -> Result<String, String> {
    with_drv_host_state(|state| state.store_text_calls += 1);
    Ok(nix_eval_rs::drvpath::text_store_path(
        "/nix/store",
        name,
        contents,
        references,
    ))
}

fn drv_host() -> nix_eval_rs::host::FnHost {
    nix_eval_rs::host::FnHost {
        write_drv: Some(write_drv),
        store_text: Some(store_text),
        ..nix_eval_rs::host::FnHost::default()
    }
}

fn evaluate_in_store(
    source: &str,
    store: &nix_eval_rs::store::Store,
    host: &dyn nix_eval_rs::host::Host,
) -> Result<
    (
        nix_eval_rs::readset::EvalResult,
        bool,
        Vec<nix_eval_rs::readset::Complaint>,
    ),
    Box<dyn core::error::Error>,
> {
    use nix_eval_rs::readset::ResultCache;
    use nix_eval_rs::vm::Vm;

    let mut results = ResultCache::persistent(store);
    let settings = eval::Settings {
        store_dir: Some("/nix/store".to_owned()),
        current_system: Some("x86_64-linux".to_owned()),
        ..eval::Settings::default()
    };
    let mut vm = Vm::with_modules(settings, ModuleCache::persistent(store.clone()));
    let (result, reuse) = session::evaluate(
        &mut vm,
        Some(&mut results),
        host,
        source,
        "/base",
        nix_eval_rs::compile::Origin::String,
    );
    Ok((result, reuse.memo_hit, results.take_corruption()))
}

fn one_derivation() -> &'static str {
    r#"(builtins.derivationStrict {
      name = "witness-one";
      system = "x86_64-linux";
      builder = "/bin/sh";
      args = [];
    }).drvPath"#
}

fn witness_questions(store: &nix_eval_rs::store::Store) -> Vec<nix_eval_rs::readset::Question> {
    std::fs::read_dir(store.witness_dir())
        .expect("read witness directory")
        .filter_map(Result::ok)
        .find_map(|entry| {
            let bytes = std::fs::read(entry.path()).ok()?;
            let rows = nix_eval_rs::readset::witness_rows(&bytes)?;
            Some(rows.into_iter().map(|(question, _)| question).collect())
        })
        .expect("one readable evaluation witness")
}

#[test]
fn a_served_derivation_replays_write_derivation_without_store_text()
-> Result<(), Box<dyn core::error::Error>> {
    let _guard = serial();
    let scratch = Scratch::new("write-drv-fidelity");
    let store = nix_eval_rs::store::Store::open(scratch.0.join("store"))?;
    with_drv_host_state(|state| *state = DrvHostState::default());

    let (cold, cold_hit, cold_complaints) =
        evaluate_in_store(one_derivation(), &store, &drv_host())?;
    assert!(!cold_hit);
    assert!(cold_complaints.is_empty(), "{cold_complaints:?}");
    with_drv_host_state(|state| {
        state.write_derivation_calls = 0;
        state.store_text_calls = 0;
        state.writes.clear();
    });

    let (served, served_hit, served_complaints) =
        evaluate_in_store(one_derivation(), &store, &drv_host())?;
    assert_eq!(served, cold);
    assert!(served_hit, "the derivation was evaluated instead of served");
    assert!(served_complaints.is_empty(), "{served_complaints:?}");
    with_drv_host_state(|state| {
        assert_eq!(state.write_derivation_calls, 1);
        assert_eq!(state.store_text_calls, 0);
    });
    Ok(())
}

#[test]
fn a_deleted_drv_is_rewritten_from_the_aterm_cas() -> Result<(), Box<dyn core::error::Error>> {
    let _guard = serial();
    let scratch = Scratch::new("write-drv-deleted");
    let store = nix_eval_rs::store::Store::open(scratch.0.join("store"))?;
    with_drv_host_state(|state| *state = DrvHostState::default());
    let (_, hit, complaints) = evaluate_in_store(one_derivation(), &store, &drv_host())?;
    assert!(!hit);
    assert!(complaints.is_empty(), "{complaints:?}");
    let original = with_drv_host_state(|state| {
        let aterm = state
            .writes
            .first()
            .expect("cold derivation write")
            .1
            .clone();
        state.present.clear();
        state.write_derivation_calls = 0;
        state.writes.clear();
        aterm
    });

    let (_, hit, complaints) = evaluate_in_store(one_derivation(), &store, &drv_host())?;
    assert!(hit, "deleting the drv should be repaired during replay");
    assert!(complaints.is_empty(), "{complaints:?}");
    with_drv_host_state(|state| {
        assert_eq!(state.write_derivation_calls, 1);
        assert_eq!(state.writes.first().expect("replayed write").1, original);
        assert_eq!(state.present.len(), 1);
    });
    Ok(())
}

#[test]
fn repeated_read_file_questions_share_one_witness_row() -> Result<(), Box<dyn core::error::Error>> {
    let _guard = serial();
    let scratch = Scratch::new("read-file-dedup");
    std::fs::create_dir_all(&scratch.0)?;
    let input = scratch.0.join("input");
    std::fs::write(&input, "a")?;
    let source = format!(
        "builtins.concatStringsSep \"\" (builtins.genList (_: builtins.readFile \"{}\") 1000)",
        input.display()
    );
    let store = nix_eval_rs::store::Store::open(scratch.0.join("store"))?;
    nix_eval_rs::perf::reset();

    let (first, first_hit, complaints) =
        evaluate_in_store(&source, &store, &nix_eval_rs::host::RealFs)?;
    assert!(!first_hit);
    assert!(complaints.is_empty(), "{complaints:?}");
    let questions = witness_questions(&store);
    assert_eq!(
        questions.len(),
        1,
        "witness was not compacted: {questions:?}"
    );
    // `readFile` asks for the bytes, as cppnix's does (a Nix string is a byte
    // string); the text question is `import`'s.
    assert!(matches!(
        questions.first(),
        Some(nix_eval_rs::readset::Question::ReadFileBytes(_))
    ));
    if cfg!(feature = "perf") {
        let stats = nix_eval_rs::perf::snapshot();
        assert_eq!(stats.witness_rows, 1);
        assert_eq!(stats.witness_rows_deduped, 999);
    }

    let (second, second_hit, complaints) =
        evaluate_in_store(&source, &store, &nix_eval_rs::host::RealFs)?;
    assert_eq!(second, first);
    assert!(second_hit, "unchanged answers produced a different key");
    assert!(complaints.is_empty(), "{complaints:?}");

    std::fs::write(&input, "b")?;
    let (changed, changed_hit, complaints) =
        evaluate_in_store(&source, &store, &nix_eval_rs::host::RealFs)?;
    assert!(!changed_hit, "changed file content hit the old key");
    assert_ne!(changed, first);
    assert!(complaints.is_empty(), "{complaints:?}");
    Ok(())
}

#[test]
fn aterms_are_content_addressed_and_counted() -> Result<(), Box<dyn core::error::Error>> {
    let _guard = serial();
    let distinct = r#"let mk = name: (builtins.derivationStrict {
      inherit name;
      system = "x86_64-linux";
      builder = "/bin/sh";
      args = [];
    }).drvPath; in [ (mk "a") (mk "b") ]"#;
    let repeated = distinct.replace("(mk \"b\")", "(mk \"a\")");

    let distinct_scratch = Scratch::new("aterms-distinct");
    let distinct_store = nix_eval_rs::store::Store::open(distinct_scratch.0.join("store"))?;
    with_drv_host_state(|state| *state = DrvHostState::default());
    nix_eval_rs::perf::reset();
    let (_, hit, complaints) = evaluate_in_store(distinct, &distinct_store, &drv_host())?;
    assert!(!hit);
    assert!(complaints.is_empty(), "{complaints:?}");
    let distinct_aterms: std::collections::BTreeSet<_> = witness_questions(&distinct_store)
        .into_iter()
        .filter_map(|question| match question {
            nix_eval_rs::readset::Question::WriteDrv { aterm, .. } => Some(aterm),
            _ => None,
        })
        .collect();
    assert_eq!(distinct_aterms.len(), 2);
    assert!(distinct_aterms.iter().all(|id| {
        distinct_store
            .objects_dir()
            .join(id.hash().to_hex())
            .is_file()
    }));
    if cfg!(feature = "perf") {
        let stats = nix_eval_rs::perf::snapshot();
        assert_eq!(stats.aterms_stored, 2);
        assert_eq!(stats.aterms_reused, 0);
    }

    // One evaluation building one derivation twice: the repeat never reaches
    // the host (`Vm::note_drv_written`), so the witness holds one WriteDrv
    // and the CAS stored one ATerm. Reuse is across evaluations: a second
    // evaluation in the same store builds `a` again and finds its ATerm.
    let repeated_scratch = Scratch::new("aterms-repeated");
    let repeated_store = nix_eval_rs::store::Store::open(repeated_scratch.0.join("store"))?;
    with_drv_host_state(|state| *state = DrvHostState::default());
    nix_eval_rs::perf::reset();
    let (_, hit, complaints) = evaluate_in_store(&repeated, &repeated_store, &drv_host())?;
    assert!(!hit);
    assert!(complaints.is_empty(), "{complaints:?}");
    let repeated_aterms: std::collections::BTreeSet<_> = witness_questions(&repeated_store)
        .into_iter()
        .filter_map(|question| match question {
            nix_eval_rs::readset::Question::WriteDrv { aterm, .. } => Some(aterm),
            _ => None,
        })
        .collect();
    assert_eq!(repeated_aterms.len(), 1);
    if cfg!(feature = "perf") {
        let stats = nix_eval_rs::perf::snapshot();
        assert_eq!(stats.aterms_stored, 1);
        assert_eq!(stats.aterms_reused, 0);
        assert_eq!(
            stats.write_drv_skipped, 1,
            "the repeat is skipped in the VM"
        );
    }

    with_drv_host_state(|state| *state = DrvHostState::default());
    nix_eval_rs::perf::reset();
    let (_, hit, complaints) = evaluate_in_store(distinct, &repeated_store, &drv_host())?;
    assert!(!hit, "a different expression hit the repeated one's key");
    assert!(complaints.is_empty(), "{complaints:?}");
    if cfg!(feature = "perf") {
        let stats = nix_eval_rs::perf::snapshot();
        assert_eq!(stats.aterms_stored, 1, "b's ATerm is new to the CAS");
        assert_eq!(stats.aterms_reused, 1, "a's ATerm was already in the CAS");
    }
    Ok(())
}

#[test]
fn a_missing_aterm_object_is_a_counted_cache_miss() -> Result<(), Box<dyn core::error::Error>> {
    let _guard = serial();
    let scratch = Scratch::new("aterm-missing");
    let store = nix_eval_rs::store::Store::open(scratch.0.join("store"))?;
    with_drv_host_state(|state| *state = DrvHostState::default());
    let (first, hit, complaints) = evaluate_in_store(one_derivation(), &store, &drv_host())?;
    assert!(!hit);
    assert!(complaints.is_empty(), "{complaints:?}");
    let aterm = witness_questions(&store)
        .into_iter()
        .find_map(|question| match question {
            nix_eval_rs::readset::Question::WriteDrv { aterm, .. } => Some(aterm),
            _ => None,
        })
        .expect("WriteDrv witness row");
    std::fs::remove_file(store.objects_dir().join(aterm.hash().to_hex()))?;

    let (second, hit, complaints) = evaluate_in_store(one_derivation(), &store, &drv_host())?;
    assert_eq!(second, first);
    assert!(!hit, "a dangling ATerm address was served");
    assert_eq!(
        complaints.len(),
        1,
        "corruption was not counted: {complaints:?}"
    );
    assert!(
        matches!(complaints.as_slice(), [complaint] if format!("{complaint:?}").contains("ATerm object")),
        "wrong corruption report: {complaints:?}"
    );
    Ok(())
}

/// ENG-12541 part 2. The same expression under two purity regimes must be two
/// memo rows.
///
/// The fingerprint half is `eval::tests::each_purity_configuration_is_its_own_memo_key`;
/// this is the identity a result is actually filed under, which is what the
/// cache looks up. It is a separate assertion because a fingerprint that moved
/// and an `EvalId` that did not would still let one regime read the other's
/// row, and the two are computed in different files.
///
/// `pure-eval` alone and `restrict-eval` alone are the pair that matters. They
/// were one bit before the split -- the embedder passed
/// `restrictEval || pureEval` -- and they forbid different questions, so
/// sharing a row means a result computed where `getEnv` was answered from the
/// environment can be served where it must be `""`, and a result computed
/// where a URI passed `allowed-uris` can be served where it was never checked.
#[test]
fn each_purity_configuration_is_its_own_memo_row() {
    let _guard = serial();
    let base = eval::Settings {
        store_dir: Some("/nix/store".to_owned()),
        nix_version: Some("2.34.7".to_owned()),
        host_build_identity: Some("/test/host-a".to_owned()),
        current_system: Some("x86_64-linux".to_owned()),
        max_call_depth: 10_000,
        pure_eval: false,
        restrict_eval: false,
        builtin_features: nix_eval_rs::eval::BuiltinFeatures::ALL,
        path_reads: nix_eval_rs::purity::PathReads::Direct,
        trace_verbose: false,
        abort_on_warn: false,
        home_dir: Some("/home/nixer".to_owned()),
        ca_derivations: false,
        allow_import_from_derivation: true,
        allowed_uris: None,
        repair: false,
        blake3_hashes: false,
        lint_url_literals: nix_eval_rs::eval::Diagnose::Ignore,
        lint_short_path_literals: nix_eval_rs::eval::Diagnose::Ignore,
        lint_absolute_path_literals: nix_eval_rs::eval::Diagnose::Ignore,
        pipe_operators: false,
        parse_toml_timestamps: false,
    };
    let module = ix_kernel::hash::tagged("module", &[b"same source"]);
    let mut rows: Vec<(&str, nix_eval_rs::readset::EvalId)> = Vec::new();
    for (label, pure_eval, restrict_eval) in [
        ("neither", false, false),
        ("pure only", true, false),
        ("restrict only", false, true),
        ("both", true, true),
    ] {
        let settings = eval::Settings {
            pure_eval,
            restrict_eval,
            ..base.clone()
        };
        let row = nix_eval_rs::readset::EvalId::of(&module, &settings, &none(), &whole());
        for (other, previous) in &rows {
            assert_ne!(
                &row, previous,
                "the same module under {label} and under {other} is one cache entry"
            );
        }
        rows.push((label, row));
    }
    assert_eq!(rows.len(), 4);
}

/// ENG-12543. `pure-eval` and `restrict-eval` must mean the same thing with
/// and without a cache directory, including on the lookup path.
///
/// Measured at 8065be845, before the settings reached the memo key: a cache
/// filled with reads allowed, then looked up under `pure-eval`, returned
/// `status=ok value="secret" memo_hit=true reads=["/etc/shadow"]`. The setting
/// was bypassed twice over -- the file was read, and the answer computed from
/// reading it was served.
///
/// Two things stop it now and this asserts the outcome both produce: the
/// settings are in the memo identity, so the pure-eval lookup addresses a row
/// nothing wrote; and `ReadSet::replay` refuses outright when access is off,
/// so no read happens even if a witness were somehow found.
#[test]
fn pure_eval_survives_the_cache() {
    let _guard = serial();
    let scratch = Scratch::new("pure-eval");
    let secret = scratch.0.join("secret.txt");
    assert!(std::fs::create_dir_all(&scratch.0).is_ok());
    assert!(std::fs::write(&secret, "the contents").is_ok());
    let source = format!("builtins.readFile {}", secret.display());
    let store = scratch.0.join("store");

    // Fill the cache with reads allowed, and confirm the read really happens
    // -- otherwise the rest of this test is comparing two refusals.
    eval::set_pure_eval(false);
    let filled = cached(&source, &store);
    assert_eq!(
        (filled.class, filled.text.as_str()),
        ("ok", "\"the contents\""),
        "the fill did not read the file, so nothing below is being tested"
    );

    // Now the same expression under pure-eval, against that same cache.
    eval::set_pure_eval(true);
    let sealed_cached = cached(&source, &store);
    let sealed_plain = uncached(&source);
    eval::set_pure_eval(false);

    assert_eq!(
        sealed_cached.class, "unimplemented",
        "a pure-eval run was served a cached answer computed with reads \
         allowed: {sealed_cached:?}"
    );
    assert_eq!(
        sealed_cached, sealed_plain,
        "eval-cache-dir changed what pure-eval means"
    );
}

/// The class ENG-12540 belongs to, checked over the shapes that have bitten:
/// a value, each error class, and an expression that reads the filesystem.
#[test]
fn cached_and_uncached_agree_on_every_shape() {
    let _guard = serial();
    let scratch = Scratch::new("shapes");
    for (n, source) in [
        "1 + 2",
        "builtins.toString (builtins.genList (i: i * 2) 5)",
        "throw \"boom\"",
        "assert false; 1",
        "let a = a; in a",
        "1 +",
        "nosuchvariable",
        "builtins.unsafeGetAttrPos",
        "builtins.pathExists /definitely/not/here",
        "builtins.readFile /definitely/not/here",
    ]
    .into_iter()
    .enumerate()
    {
        let dir = scratch.0.join(format!("case-{n}"));
        assert_eq!(
            cached(source, &dir),
            uncached(source),
            "eval-cache-dir changed the meaning of `{source}`"
        );
    }
}

/// Host code can change without changing the language version, source or observed inputs.
#[test]
fn immutable_host_builds_have_separate_persistent_question_rows() -> Result<(), String> {
    use nix_eval_rs::readset::{EvalId, EvalResult, ReadSet};
    use nix_eval_rs::session::{QuestionCache, Served};
    let scratch = Scratch::new("host-build-identity");
    let mut vm = nix_eval_rs::vm::Vm::with_settings(eval::Settings::default());
    let module = vm
        .compile("1 + 2", "/base", nix_eval_rs::compile::Origin::String)
        .map_err(|error| format!("{error:?}"))?;
    let result = EvalResult {
        status: "ok".to_owned(),
        value: "3".to_owned(),
        ..EvalResult::default()
    };
    for (host, expect_hit) in [
        ("/host/a", false),
        ("/host/b", false),
        ("/host/a", true),
        ("/host/b", true),
    ] {
        let settings = eval::Settings {
            nix_version: Some("same-language-version".to_owned()),
            host_build_identity: Some(host.to_owned()),
            ..eval::Settings::default()
        };
        let identity = EvalId::of(module.id.hash(), &settings, &none(), &whole());
        // Reopen for each request so this is persisted reuse, not an in-memory cache.
        let mut cache = QuestionCache::open(&scratch.0, 0, 0)?;
        match cache.serve(&identity, &nix_eval_rs::host::RealFs, &settings) {
            Served::Answer(answer) => {
                assert!(expect_hit, "{host} reused another host's answer");
                assert_eq!(answer, result);
            }
            Served::Evaluate { verifying } => {
                assert!(!expect_hit, "{host} lost its own persisted answer");
                assert!(verifying.is_none());
                cache.settle(
                    &identity,
                    &nix_eval_rs::host::RealFs,
                    &settings,
                    &ReadSet::default(),
                    &result,
                    None,
                    false,
                );
            }
        }
        assert!(cache.take_complaints().is_empty());
    }
    Ok(())
}
