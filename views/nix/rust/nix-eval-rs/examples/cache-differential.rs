//! Evaluate a corpus and print exactly what came out, so two runs configured
//! differently can be diffed byte for byte.
//!
//! The gate this feeds is `maintainers/ix/cache-semantics-gate.sh`, and the
//! property it exists to hold is that `eval-cache-dir` changes speed and
//! nothing else. Three separate bugs said otherwise (ENG-12540), each one a
//! wrong answer visible only to somebody who had turned the option on, and
//! each one invisible to every gate that existed: the corpus arms compared
//! `nix` against `nix`, both configured the same way, so a setting that
//! changed meaning changed both arms identically.
//!
//! # Why the settings are command-line arguments and not a loop
//!
//! `store_dir` and `nix_version` are `OnceLock`s -- one value per process,
//! because the C ABI has no handle to hang configuration off and the embedder
//! sets them once before evaluating. So a harness that wanted to compare two
//! store directories could not do it in one process, and this deliberately
//! does not try. One process is one configuration; the script forks.
//!
//! # What a line means
//!
//! One tab-separated line per corpus file: the file's name, the outcome class,
//! a digest of the rendered value or error message, and a digest of the
//! warnings in order. Digests rather than text because a corpus value can be
//! megabytes and the comparison is equality, not inspection -- but the class
//! is printed plainly, because that is what a reader diffing two runs wants to
//! see first.

use ix_kernel::hash;
use nix_eval_rs::compile::Origin;
use nix_eval_rs::eval::EvalError;
use nix_eval_rs::modcache::ModuleCache;
use nix_eval_rs::session;
use std::sync::Mutex;

/// Warnings the evaluation emitted, in order.
///
/// A global because `host::FnHost`'s `warn` field is a plain `fn` pointer,
/// which is what the C ABI can carry; there is nowhere to hang state. One
/// evaluation at a time here, so the mutex is a formality rather than
/// contention.
static WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record_warning(message: &str) {
    if let Ok(mut warnings) = WARNINGS.lock() {
        warnings.push(message.to_owned());
    }
}

fn take_warnings() -> Vec<String> {
    WARNINGS
        .lock()
        .map(|mut w| core::mem::take(&mut *w))
        .unwrap_or_default()
}

struct Args {
    /// Repeatable: the lang corpus does not exercise every setting, so the
    /// gate passes a second directory holding one expression per setting it
    /// misses. See `maintainers/ix/cache-semantics-corpus/README.md`.
    corpus: Vec<std::path::PathBuf>,
    cache: Option<std::path::PathBuf>,
    store_dir: Option<String>,
    nix_version: Option<String>,
    current_system: Option<String>,
    verify_rate: u32,
    selftest: bool,
    max_call_depth: Option<u32>,
    pure_eval: bool,
    restrict_eval: bool,
}

fn usage() -> String {
    "cache-differential --corpus DIR [--corpus DIR ...] [--cache DIR] \
     [--store-dir S] [--nix-version V] [--current-system S] \
     [--max-call-depth N] [--pure-eval] [--restrict-eval] [--verify-rate N]"
        .to_owned()
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        corpus: Vec::new(),
        cache: None,
        store_dir: None,
        nix_version: None,
        current_system: None,
        verify_rate: 0,
        selftest: false,
        max_call_depth: None,
        pure_eval: false,
        restrict_eval: false,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--corpus" => args.corpus.push(std::path::PathBuf::from(value()?)),
            "--cache" => args.cache = Some(std::path::PathBuf::from(value()?)),
            "--store-dir" => args.store_dir = Some(value()?),
            "--nix-version" => args.nix_version = Some(value()?),
            "--current-system" => args.current_system = Some(value()?),
            "--verify-rate" => {
                let text = value()?;
                args.verify_rate = text.parse().map_err(|_| format!("bad rate {text:?}"))?;
            }
            "--max-call-depth" => {
                let text = value()?;
                args.max_call_depth =
                    Some(text.parse().map_err(|_| format!("bad depth {text:?}"))?);
            }
            "--pure-eval" => args.pure_eval = true,
            "--restrict-eval" => args.restrict_eval = true,
            "--verify-selftest" => args.selftest = true,
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
    }
    if args.corpus.is_empty() && !args.selftest {
        return Err(format!("--corpus is required\n{}", usage()));
    }
    Ok(args)
}

/// The outcome class, kept apart from the message.
///
/// An embedder raises a different exception per class and cannot recover the
/// class from the text, so a cache that preserved the message while losing
/// the class would pass a message-only comparison. That is not hypothetical:
/// it is how the compile-failure classes went wrong once already
/// (`session::tests::a_compile_failure_is_classified_the_same_with_and_without_a_cache`).
fn class_of(answer: &Result<String, EvalError>) -> &'static str {
    match answer {
        Ok(_) => "ok",
        Err(EvalError::Unimplemented(_)) => "unimplemented",
        Err(EvalError::Parse(_)) => "parse",
        Err(EvalError::Eval(nix_eval_rs::vm::ErrKind::Eval, _, _)) => "eval",
        Err(EvalError::Eval(nix_eval_rs::vm::ErrKind::Thrown, _, _)) => "thrown",
        Err(EvalError::Eval(nix_eval_rs::vm::ErrKind::Assertion, _, _)) => "assertion",
        Err(EvalError::Eval(nix_eval_rs::vm::ErrKind::ImportFromDerivation, _, _)) => {
            "import-from-derivation"
        }
        Err(EvalError::Eval(nix_eval_rs::vm::ErrKind::MissingArgument, _, _)) => "missing-argument",
    }
}

fn text_of(answer: &Result<String, EvalError>) -> &str {
    match answer {
        Ok(value) => value,
        Err(EvalError::Unimplemented(refusal)) => &refusal.detail,
        Err(EvalError::Parse(message) | EvalError::Eval(_, message, _)) => message,
    }
}

/// Prove the verifier in *this binary* actually catches a wrong answer.
///
/// `--verify-rate 1` is a setting, and a setting that never reached the code
/// passes every assertion built on it. This repo has been caught by exactly
/// that shape more than once: `nix config show` reports `eval-backend = rust`
/// on a binary compiled without the Rust evaluator, and one lang-diff run
/// scored `mismatch=249` against such a stub. So the gate probes the effect
/// before trusting the arms, the way `lang-diff.sh` evaluates `1` first.
///
/// Poisons a row and requires the verifier to report it. Exit 0 if the
/// mechanism is live.
fn verify_selftest() -> std::process::ExitCode {
    use ix_kernel::cas::{Cas, MemoryCas};
    use nix_eval_rs::compile::Origin;
    use nix_eval_rs::readset::{EvalId, EvalResult, ReadSet, ResultCache, Severity};
    use nix_eval_rs::vm::Vm;

    let cas = MemoryCas::new();
    let cas: &dyn Cas = &cas;
    let mut results = ResultCache::new(cas);
    let mut vm = Vm::from_process_settings();
    let source = "1 + 2";

    let Ok(compiled) = vm.compile(source, "/selftest", Origin::String) else {
        eprintln!("cache-differential: selftest could not compile `{source}`");
        return std::process::ExitCode::from(2);
    };
    let identity = EvalId::of(
        compiled.id.hash(),
        &nix_eval_rs::eval::Settings::current(),
        &nix_eval_rs::session::Arguments::none(),
        &nix_eval_rs::session::Question::Whole {
            render: nix_eval_rs::session::RenderMode::Plain,
        },
    );
    // File a wrong answer first: the memo table is Keyed, so a record after an
    // honest run would reuse the honest row rather than replace it.
    if results
        .record(
            &identity,
            &ReadSet::default(),
            &EvalResult {
                status: "ok".to_owned(),
                value: "4".to_owned(),
                ..EvalResult::default()
            },
            &nix_eval_rs::host::RealFs,
            &nix_eval_rs::eval::Settings::current(),
        )
        .is_err()
    {
        eprintln!("cache-differential: selftest could not plant a row");
        return std::process::ExitCode::from(2);
    }

    results.set_verify_rate(1);
    let (served, reuse) = session::evaluate(
        &mut vm,
        Some(&mut results),
        &nix_eval_rs::host::RealFs,
        source,
        "/selftest",
        Origin::String,
    );
    let counts = results.verifier();
    let shouted = results
        .take_corruption()
        .into_iter()
        .filter(|c| c.severity == Severity::Error)
        .count();

    if !reuse.memo_hit || served.value != "4" {
        eprintln!(
            "cache-differential: selftest never served the planted row \
             (memo_hit={}, value={:?}); the probe itself is broken",
            reuse.memo_hit, served.value
        );
        return std::process::ExitCode::from(2);
    }
    if counts.hits_disagreed != 1 || shouted != 1 {
        eprintln!(
            "cache-differential: THE VERIFIER IS NOT WIRED. A row saying \"4\" for \
             `1 + 2` was served and not reported: {counts:?}, {shouted} error-priority \
             complaints. Every --verify-rate arm in this gate is measuring nothing."
        );
        return std::process::ExitCode::from(1);
    }
    println!("verifier selftest: a planted wrong answer was caught ({counts:?})");
    std::process::ExitCode::SUCCESS
}

fn main() -> std::process::ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("cache-differential: {message}");
            return std::process::ExitCode::from(2);
        }
    };

    if args.selftest {
        return verify_selftest();
    }

    // Configure before evaluating, in the order the C++ bridge does
    // (`src/libcmd/rust-eval-session.cc`), so this measures the same
    // configuration path a user gets.
    if let Some(depth) = args.max_call_depth {
        nix_eval_rs::eval::set_max_call_depth(depth);
    }
    // Each of the three refuses a conflicting second call. One process is one
    // configuration here by design, so a refusal means the harness was asked
    // for something it cannot honour and must say so rather than measure a
    // configuration nobody requested.
    for outcome in [
        args.nix_version
            .as_deref()
            .map(nix_eval_rs::eval::set_nix_version),
        args.current_system
            .as_deref()
            .map(nix_eval_rs::eval::set_current_system),
        args.store_dir
            .as_deref()
            .map(nix_eval_rs::eval::set_store_dir),
    ]
    .into_iter()
    .flatten()
    {
        if let Err(conflict) = outcome {
            eprintln!("cache-differential: {conflict}");
            return std::process::ExitCode::from(2);
        }
    }
    nix_eval_rs::eval::set_pure_eval(args.pure_eval);
    nix_eval_rs::eval::set_restrict_eval(args.restrict_eval);
    // One host for the whole run: the only question this differential asks
    // the outside world is where a warning goes.
    let host = nix_eval_rs::host::FnHost {
        warn: Some(record_warning),
        ..nix_eval_rs::host::FnHost::default()
    };

    // The same set `maintainers/ix/lang-diff.sh` runs, discovered the same
    // way, so the two gates share a denominator and a divergence found here
    // can be looked up there. The rest of `tests/functional/lang` is inputs
    // to those cases and machinery that is not an evaluation at all --
    // `infinite-nesting.nix` among them, which has no `.exp` because no
    // evaluator finishes it.
    // The same set `maintainers/ix/lang-diff.sh` runs, discovered the same
    // way, so the two gates share a denominator and a divergence found here
    // can be looked up there. The rest of `tests/functional/lang` is inputs
    // to those cases and machinery that is not an evaluation at all --
    // `infinite-nesting.nix` among them, which has no `.exp` because no
    // evaluator finishes it.
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for dir in &args.corpus {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("cache-differential: cannot read {}: {error}", dir.display());
                return std::process::ExitCode::from(2);
            }
        };
        files.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|e| e == "nix"))
                .filter(|path| {
                    path.file_name().is_some_and(|name| {
                        let name = name.to_string_lossy();
                        name.starts_with("eval-okay-") || name.starts_with("eval-fail-")
                    })
                }),
        );
    }
    files.sort();

    // A corpus that matched nothing must not read as "nothing diverged".
    if files.is_empty() {
        eprintln!("cache-differential: no eval-okay-*.nix or eval-fail-*.nix in the corpus");
        return std::process::ExitCode::from(2);
    }

    // Outcomes are keyed on the basename, so two corpus directories holding
    // the same name would print two lines under one label and diff as noise.
    let mut names: Vec<String> = files
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    let unique = {
        let mut unique = names.clone();
        unique.dedup();
        unique
    };
    if names != unique {
        eprintln!("cache-differential: two corpus directories share a file name");
        return std::process::ExitCode::from(2);
    }

    for file in &files {
        // Its own directory, not one shared base: `./read-me.txt` inside a
        // corpus file has to resolve beside that file whichever corpus it
        // came from. Absolute, because `compile_source` makes path literals
        // absolute against this and a relative base would resolve them
        // against the shell's working directory instead -- which reads as an
        // evaluation error in a file that is fine.
        let base = file
            .parent()
            .map(|dir| std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
            .and_then(|dir| dir.to_str().map(str::to_owned))
            .unwrap_or_else(|| ".".to_owned());
        let name = file
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let Ok(source) = std::fs::read_to_string(file) else {
            // A file this crate cannot read is a fact about the corpus, and it
            // is the same fact in both arms, so it is printed rather than
            // skipped: skipping would shrink the denominator silently.
            println!("{name}\tunreadable\t-\t-");
            continue;
        };
        drop(take_warnings());
        let path = file.to_string_lossy().into_owned();
        // Uncapped: the differential compares answers, and a sweep between
        // arms would turn a hit into a cold evaluation that agrees for the
        // wrong reason.
        let (store, unopened) = session::open_cache(args.cache.as_deref(), 0);
        let mut vm = nix_eval_rs::vm::Vm::with_modules(
            nix_eval_rs::eval::Settings::current(),
            store.map_or_else(ModuleCache::in_memory, ModuleCache::persistent),
        );
        let (answer, mut complaints) = session::evaluate_once(
            &mut vm,
            &host,
            &source,
            &base,
            Origin::File(&path),
            true,
            args.verify_rate,
        );
        complaints.extend(unopened);
        let warnings = take_warnings();

        // A complaint means the run under test is not the run intended: the
        // cache refused to open and this arm silently became the uncached
        // one, which would compare equal for the wrong reason.
        for complaint in &complaints {
            eprintln!("cache-differential: {name}: {complaint}");
        }
        if !complaints.is_empty() {
            return std::process::ExitCode::from(3);
        }

        let value = hash::tagged("differential-value", &[text_of(&answer).as_bytes()]);
        let warned: Vec<&[u8]> = warnings.iter().map(|w| w.as_bytes()).collect();
        let warned = hash::tagged("differential-warnings", &warned);
        println!(
            "{name}\t{}\t{}\t{}",
            class_of(&answer),
            &value.to_hex()[..16],
            &warned.to_hex()[..16]
        );
    }
    eprintln!("cache-differential: {} files", files.len());
    std::process::ExitCode::SUCCESS
}
