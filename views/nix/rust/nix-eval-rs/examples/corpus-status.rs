//! Per-file compile and eval status for a corpus, with the reason attached.
//!
//! The coverage counts ("compiles N/150, evaluates M/150") say how far the VM
//! gets but not what stops it, and a burndown needs the mechanism name, not
//! the total. This prints one TSV line per file:
//!
//!     path <TAB> compile-status <TAB> eval-status <TAB> reason
//!
//! One file per invocation is the supported shape, as in `compile-share`: the
//! corpus contains expressions this VM does not terminate on, and an
//! in-process loop has no way to bound one. The caller supplies `timeout` and
//! an address-space cap.

use nix_eval_rs::eval::drive;
use nix_eval_rs::host::{Host, RealFs};
use nix_eval_rs::value2::Value;
use nix_eval_rs::vm::Vm;
use std::rc::Rc;

/// Collapse a reason to one line: these land in a TSV that a shell loop
/// aggregates, and an embedded newline would split one file across two rows.
fn one_line(text: &str) -> String {
    text.replace(['\n', '\r', '\t'], " ")
}

fn main() -> Result<(), Box<dyn core::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: corpus-status <file.nix>")?;

    let real = RealFs;
    let resolved = match real.resolve_import(&nix_eval_rs::value2::ambient_path(path.as_str())) {
        Ok(resolved) => resolved,
        Err(error) => {
            println!("{path}\tunreadable\t-\t{}", one_line(&error));
            return Ok(());
        }
    };
    let source = match real.read_file(&resolved) {
        Ok(source) => source,
        Err(error) => {
            println!("{path}\tunreadable\t-\t{}", one_line(&error));
            return Ok(());
        }
    };
    let resolved_text = resolved.to_string();
    let base = match resolved_text.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir.to_owned(),
        _ => ".".to_owned(),
    };

    let origin = nix_eval_rs::compile::Origin::File(&resolved_text);
    let module = match nix_eval_rs::compile::compile_source(
        &source,
        &base,
        origin,
        &nix_eval_rs::eval::Settings::current(),
    ) {
        Ok(module) => Rc::new(module),
        Err(error) => {
            println!("{path}\tno\t-\t{}", one_line(&format!("{error:?}")));
            return Ok(());
        }
    };

    let mut vm = Vm::from_process_settings();
    vm.start_module(&module);
    let outcome = drive(&mut vm, &real).and_then(|value| {
        vm.start_print(value);
        drive(&mut vm, &real)
    });
    match outcome {
        Ok(Value::Str(_)) => println!("{path}\tyes\tyes\t-"),
        Ok(other) => println!(
            "{path}\tyes\tno\tnon-string print: {}",
            one_line(&format!("{other:?}"))
        ),
        Err(error) => println!("{path}\tyes\tno\t{}", one_line(&format!("{error:?}"))),
    }
    Ok(())
}
