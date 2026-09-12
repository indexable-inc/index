//! A VM that outlives one evaluation, optionally memoising results.
//!
//! One request per line on stdin, one JSON object per line on stdout, which
//! is the shape `nix eval-persistent --interactive` uses so the same harness
//! can drive either. A request is the path of a `.nix` file to evaluate.
//!
//! What persists across requests, and the one thing that deliberately does
//! not:
//!
//! * The **compile cache** persists, so a file seen before is not recompiled.
//! * The **VM's import cache and symbol interner** persist, because the `Vm`
//!   is reused; `start_module` resets the frame chain, so execution state does
//!   not leak between requests.
//! * With `--memo`, **evaluated results** persist too, keyed on the module and
//!   everything the evaluation read.
//! * The **source is re-read from disk on every request**, and under `--memo`
//!   so is everything the last evaluation read. That is what makes the caches
//!   safe rather than stale: all of them are keyed on content, so an edit
//!   mints a different key and misses. There is no invalidation pass and no
//!   dependency graph, because content addressing already is one.
//!
//! `--fresh` rebuilds everything per request. It is the oracle the other modes
//! are compared against: a retained process agreeing with itself proves
//! nothing.

use ix_kernel::cas::{Cas, MemoryCas};
use nix_eval_rs::host::{Host, RealFs};
use nix_eval_rs::modcache::ModuleCache;
use nix_eval_rs::modcache::compile_domain;
use nix_eval_rs::readset::{EvalResult, ResultCache, eval_domain};
use nix_eval_rs::session;
use nix_eval_rs::store::Store;
use nix_eval_rs::vm::Vm;
use std::io::{BufRead, Write};

/// JSON string escaping. Written out rather than pulled from serde_json
/// because the output is five known fields and a value that may contain any
/// byte a Nix string can.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct Answer {
    result: EvalResult,
    /// The compile cache already had this file.
    hit: bool,
    /// The result cache answered without evaluating.
    memo: bool,
}

/// Resolve a request to source text, then hand the actual work to
/// `session::evaluate`, which is the same function the C ABI calls. Path
/// resolution is the server's business; evaluation is not, and having two
/// copies of it was how the two embedders would drift.
fn evaluate(vm: &mut Vm, results: Option<&mut ResultCache<'_, dyn Cas>>, path: &str) -> Answer {
    let real = RealFs;
    let fail = |message: String| Answer {
        result: EvalResult {
            status: session::EVAL.to_owned(),
            value: message,
            emissions: Vec::new(),
            token: None,
            pos: None,
        },
        hit: false,
        memo: false,
    };

    let resolved = match real.resolve_import(&nix_eval_rs::value2::ambient_path(path)) {
        Ok(resolved) => resolved,
        Err(error) => return fail(error),
    };
    let resolved_text = resolved.to_string();
    // Re-read every time. The caches are keyed on this text, so reading it is
    // what lets them be trusted; skipping the read is what would make them
    // stale.
    let source = match real.read_file(&resolved) {
        Ok(source) => source,
        Err(error) => return fail(error),
    };
    let base = match resolved_text.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir.to_owned(),
        _ => ".".to_owned(),
    };

    let (result, reuse) = session::evaluate(
        vm,
        results,
        &real,
        &source,
        &base,
        nix_eval_rs::compile::Origin::File(&resolved_text),
    );
    Answer {
        result,
        hit: reuse.compile_hit,
        memo: reuse.memo_hit,
    }
}

fn main() -> Result<(), Box<dyn core::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let fresh = args.iter().any(|a| a == "--fresh");
    let memo = args.iter().any(|a| a == "--memo");
    // --store DIR keeps the caches on disk, so the next process starts warm.
    let store = args
        .iter()
        .position(|a| a == "--store")
        .and_then(|i| args.get(i + 1))
        .cloned();

    // Objects live in a DirCas when there is a store and a MemoryCas when
    // there is not; everything downstream is written against `dyn Cas` so
    // there is one code path rather than two.
    // A byte cap on the store, swept after the run rather than during it, so
    // the answers are already given before anything is reclaimed.
    let cap: Option<u64> = args
        .iter()
        .position(|a| a == "--store-max-bytes")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());
    let scrub = args.iter().any(|a| a == "--scrub");

    let disk = match &store {
        Some(root) => Some(Store::open(root)?),
        None => None,
    };

    // An offline integrity scan. It exists because lookups are by key, so a
    // row filed under a key nobody asks for is never consulted and never
    // reported; finding it needs a scan, and a scan is exactly what must not
    // happen on the request path.
    if scrub {
        let Some(disk) = &disk else {
            eprintln!("eval-server: --scrub needs --store");
            return Ok(());
        };
        return scrub_store(disk);
    }

    let memory = MemoryCas::new();
    let cas: &dyn Cas = match &disk {
        Some(disk) => disk.cas(),
        None => &memory,
    };

    // Nothing is read here: rows arrive one key at a time, as requests ask
    // for them. Whatever is refused is reported per request through
    // take_corruption below. The compile cache is the machine's own.
    let mut results = match &disk {
        Some(disk) => ResultCache::persistent(disk),
        None => ResultCache::new(cas),
    };
    let mut vm = Vm::with_modules(
        nix_eval_rs::eval::Settings::current(),
        match &disk {
            Some(disk) => ModuleCache::persistent(disk.clone()),
            None => ModuleCache::in_memory(),
        },
    );
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut n = 0u64;
    for line in stdin.lock().lines() {
        let line = line?;
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        n += 1;
        let answer = if fresh {
            // A fresh process in everything but the process. The gate also
            // compares against genuinely separate processes; this arm exists
            // so a difference can be localised to the caches.
            evaluate(&mut Vm::from_process_settings(), None, path)
        } else if memo {
            evaluate(&mut vm, Some(&mut results), path)
        } else {
            evaluate(&mut vm, None, path)
        };
        // A damaged store entry is a miss, and a miss nobody reports looks
        // exactly like a cold cache. Say it, at the priority it earns.
        for reason in vm.modules_mut().take_corruption() {
            eprintln!("eval-server: warning: compile cache: {reason}");
        }
        for reason in results.take_corruption() {
            eprintln!("eval-server: warning: result cache: {reason}");
        }
        writeln!(
            stdout,
            "{{\"n\":{n},\"file\":{},\"status\":{},\"hit\":{},\"memo\":{},\"value\":{}}}",
            quote(path),
            quote(&answer.result.status),
            answer.hit,
            answer.memo,
            quote(&answer.result.value)
        )?;
        // One line per request, flushed, so a driver can wait on a line rather
        // than on a guess at how long the request takes.
        stdout.flush()?;
    }
    if memo {
        eprintln!(
            "eval-server: memo hits={} misses={} wasted_replays={}",
            results.hits(),
            results.misses(),
            results.wasted_replays()
        );
    }

    // Sweep last. Eviction can only cause a future miss, never a wrong answer,
    // so it belongs after the work rather than in front of it, and a failure
    // here must not fail a run whose answers were already correct.
    if let (Some(disk), Some(cap)) = (&disk, cap) {
        match disk.sweep(cap) {
            Ok(report) => {
                // A sweep that emptied the witness directory is not a tidy
                // sweep, it is a cache that will serve nothing next time, and
                // it says so at error priority rather than as one number in a
                // line of five. ENG-12601 printed "swept 0 rows, 0 objects, 5
                // witnesses" and read as housekeeping.
                if report.witnesses_left == 0 && report.witnesses_removed > 0 {
                    eprintln!(
                        "eval-server: error: the sweep removed all {} witnesses and left none \
                         ({} of them unreadable). Every later process starts cold, so this \
                         store is write-only from here.",
                        report.witnesses_removed, report.witnesses_unreadable
                    );
                }
                if report.rows_removed
                    + report.objects_removed
                    + report.witnesses_removed
                    + report.leftovers_removed
                    > 0
                {
                    eprintln!(
                        "eval-server: swept {} rows, {} objects, {} witnesses, {} leftovers; {} -> {} bytes (cap {cap})",
                        report.rows_removed,
                        report.objects_removed,
                        report.witnesses_removed,
                        report.leftovers_removed,
                        report.bytes_before,
                        report.bytes_after,
                    );
                }
            }
            Err(error) => eprintln!("eval-server: warning: sweep failed: {error}"),
        }
    }
    Ok(())
}

/// Report what an offline scan finds wrong with a store, and change nothing.
fn scrub_store(disk: &Store) -> Result<(), Box<dyn core::error::Error>> {
    let rows = disk.rows();
    let cas = disk.cas();
    let domains = [("compile", compile_domain()), ("eval", eval_domain())];
    let mut problems = 0usize;
    let mut checked = 0usize;

    for (label, domain) in domains {
        let mut table = ix_kernel::MemoTable::new();
        // The bulk load is the scan: it is the one place a mis-filed row is
        // visible, which is why it stayed even after lookups went lazy.
        let report = rows.load(domain, &mut table, &|id| cas.has(id).unwrap_or(false))?;
        checked += report.loaded + report.rejected.len();
        for reason in &report.rejected {
            problems += 1;
            println!("{label}: {reason}");
        }
        println!(
            "{label}: {} rows usable, {} refused",
            report.loaded,
            report.rejected.len()
        );
    }

    // What the sweep would remove, from the sweep's own census, so this
    // report cannot drift from the rule (`Store::census`). ENG-12884 was the
    // drift: the sweep was fixed and this diagnostic kept the old filename
    // rule, calling every witness orphaned at exit 0.
    let census = disk.census()?;
    let orphaned_objects = census.unreferenced_objects();
    for name in &orphaned_objects {
        println!("objects: {name} is referenced by no row and no live witness");
    }
    let dead_witnesses = census.dead_witnesses();
    for (name, why) in &dead_witnesses {
        println!("witness: {name} {why}");
    }

    println!(
        "scrub: {checked} rows checked, {problems} refused, {} unreferenced objects, {} dead witnesses, {} leftovers, {} bytes",
        orphaned_objects.len(),
        dead_witnesses.len(),
        census.leftovers(),
        census.total()
    );
    // Orphans are normal after a sweep and are not failures; a refused row is.
    if problems > 0 {
        std::process::exit(1);
    }
    Ok(())
}
