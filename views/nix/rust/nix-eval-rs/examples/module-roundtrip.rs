//! The M-A gate: a module that has been through the content-addressed store
//! must evaluate to the same bytes as one that has not.
//!
//! Each file is compiled twice -- once straight, once through
//! encode/store/decode -- and the two printed values are compared. A
//! difference is a serialization bug; agreement is the only evidence that
//! caching compilation is safe.
//!
//! One file per invocation, for the same reason compile-share is: the corpus
//! contains expressions this evaluator does not terminate on, and the caller
//! bounds them from outside.
//!
//! Usage: module-roundtrip <base-dir> <file>...
//! Emits: name, verdict, cache outcome, detail.

use nix_eval_rs::eval::drive;
use nix_eval_rs::host::RealFs;
use nix_eval_rs::ir::Module;
use nix_eval_rs::modcache::{self, ModuleCache};
use nix_eval_rs::value2::Value;
use nix_eval_rs::vm::Vm;
use std::rc::Rc;

fn evaluate(module: &Rc<Module>) -> String {
    let mut vm = Vm::from_process_settings();
    vm.start_module(module);
    let outcome = drive(&mut vm, &RealFs).and_then(|value| {
        vm.start_print(value);
        drive(&mut vm, &RealFs)
    });
    match outcome {
        Ok(Value::Str(text)) => format!("ok:{}", text.expect_text()),
        Ok(other) => format!("nonstring:{other:?}"),
        Err(error) => format!("err:{error:?}"),
    }
}

fn main() -> Result<(), Box<dyn core::error::Error>> {
    let mut args = std::env::args().skip(1);
    let base = args
        .next()
        .ok_or("usage: module-roundtrip <base-dir> <file>...")?;
    for path in args {
        let Ok(source) = std::fs::read_to_string(&path) else {
            println!("{path}\tskip\t-\tunreadable");
            continue;
        };
        let origin = nix_eval_rs::compile::Origin::File(&path);
        let fresh = match nix_eval_rs::compile::compile_source(
            &source,
            &base,
            origin,
            &nix_eval_rs::eval::Settings::current(),
        ) {
            Ok(module) => Rc::new(module),
            Err(error) => {
                println!("{path}\tskip\t-\tcompile: {error:?}");
                continue;
            }
        };

        // A cache of its own per file, so the first call is always a miss and
        // the second always a hit: this checks both paths return the same
        // module, not just that the store accepted one.
        let mut cache = ModuleCache::in_memory();
        let stored = match cache.compile(
            &source,
            &base,
            origin,
            &nix_eval_rs::eval::Settings::current(),
        ) {
            Ok(compiled) => compiled,
            Err(error) => {
                println!("{path}\tFAIL\t-\tcache: {error}");
                continue;
            }
        };
        let again = match cache.compile(
            &source,
            &base,
            origin,
            &nix_eval_rs::eval::Settings::current(),
        ) {
            Ok(compiled) => compiled,
            Err(error) => {
                println!("{path}\tFAIL\t-\tcache second call: {error}");
                continue;
            }
        };
        if again.id != stored.id {
            println!("{path}\tFAIL\t-\thit returned a different object");
            continue;
        }

        // Byte-identical encodings, then byte-identical evaluations.
        let encoded = modcache::encode_module(&fresh)?;
        if encoded != modcache::encode_module(&stored.module)? {
            println!(
                "{path}\tFAIL\t{:?}\tencoding differs from a fresh compile",
                stored.outcome
            );
            continue;
        }
        let straight = evaluate(&fresh);
        let round_tripped = evaluate(&stored.module);
        if straight == round_tripped {
            println!(
                "{path}\tmatch\t{:?}\t{} bytes",
                again.outcome,
                encoded.len()
            );
        } else {
            println!(
                "{path}\tFAIL\t{:?}\tfresh={straight} stored={round_tripped}",
                again.outcome
            );
        }
    }
    Ok(())
}
