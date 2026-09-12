//! A type error in one builtin argument must beat a `throw` in another
//! (ENG-12674), and the table that decides it must stay in step with the
//! primops it describes.
//!
//! The failure this guards against is a wrong *value*, not a wrong message.
//! cppnix's `prim_tryEval` catches `AssertionError` only, so a `TypeError`
//! from `forceInt` kills the evaluation while a `throw` is caught. A driver
//! that forces every strict argument before any body checks a type hands the
//! throw the race, and `builtins.tryEval` answers `{ success = false; }`
//! where cppnix dies. Nothing downstream can tell that apart from a program
//! that legitimately threw.
//!
//! Every expectation below was read off `nix-instantiate (Nix) 2.34.7+ix`
//! before it was written down; the wording is cppnix's minus the `: <value>`
//! suffix this evaluator does not print.

use nix_eval_rs::builtins::{ArgType, TABLE};
use nix_eval_rs::eval::{EvalError, eval_str};
use nix_eval_rs::vm::ErrKind;

/// What the expression did, in the two categories that matter: an error
/// `tryEval` cannot catch, or a value. Rendered rather than asserted branch
/// by branch because the workspace denies `panic` in tests too.
fn outcome(src: &str) -> String {
    match eval_str(src) {
        Ok(v) => format!("value {v}"),
        Err(EvalError::Eval(ErrKind::Eval, m, _)) => format!("uncatchable {m}"),
        Err(e) => format!("other {e:?}"),
    }
}

/// The four divergences measured against cppnix on 2026-08-05. Each puts a
/// wrong-typed value at one position and a `throw` at a later one; cppnix
/// dies on the type, this evaluator used to answer `{ success = false; }`.
///
/// `substring` is the one that pins the *order* as well as the type: cppnix's
/// `prim_substring` forces the start offset, then the length, then coerces
/// the subject, so a string start beats a throwing subject.
#[test]
fn a_type_error_beats_a_throw_in_a_later_argument() {
    assert_eq!(
        outcome(r#"builtins.tryEval (builtins.concatStringsSep 1 (throw "x"))"#),
        "uncatchable expected a string but found an integer"
    );
    assert_eq!(
        outcome(r#"builtins.tryEval (builtins.substring "a" 1 (throw "x"))"#),
        "uncatchable expected an integer but found a string"
    );
    assert_eq!(
        outcome(r#"builtins.tryEval (builtins.hashString 1 (throw "x"))"#),
        "uncatchable expected a string but found an integer"
    );
    assert_eq!(
        outcome(r#"builtins.tryEval (builtins.replaceStrings 1 [] (throw "x"))"#),
        "uncatchable expected a list but found an integer"
    );
}

/// cppnix does not force builtin arguments left to right, and where it does
/// not the order is observable. `builtins.map` forces its list first, so a
/// non-list there is a type error even when the function throws -- under a
/// positional walk the throw wins and the answer is a caught failure.
///
/// The other half is the reason `map`'s function position is `Any`: cppnix
/// only reaches `forceFunction` when the list is non-empty, so a check there
/// would reject `map 1 []`, which cppnix accepts.
#[test]
fn the_argument_order_is_cppnixs_and_not_left_to_right() {
    assert_eq!(
        outcome(r#"builtins.tryEval (builtins.map (throw "x") 1)"#),
        "uncatchable expected a list but found an integer"
    );
    assert_eq!(
        outcome(r#"builtins.tryEval (builtins.elemAt (throw "x") "s")"#),
        "uncatchable expected an integer but found a string"
    );
    assert_eq!(outcome(r#"builtins.map 1 []"#), "value [ ]");
}

/// A `throw` in an argument whose type is right still throws, and one in an
/// argument cppnix never forces still never fires. Without this the test
/// above is satisfied by an evaluator that simply raises a type error more
/// often than cppnix.
#[test]
fn a_well_typed_argument_still_lets_the_throw_through() {
    assert_eq!(
        outcome(r#"builtins.tryEval (builtins.concatStringsSep "," (throw "x"))"#),
        "value { success = false; value = false; }"
    );
    // `foldl'` leaves its accumulator unforced, which is why the position is
    // absent from the entry's `strict` list rather than tagged.
    assert_eq!(
        outcome(r#"builtins.foldl' (a: b: b) (throw "x") [ 1 2 ]"#),
        "value 2"
    );
}

/// `forceFunction` passes a `__functor` set, so a position tagged `Function`
/// must too -- everything built on `lib.makeOverridable` is one, and
/// rejecting it would break `map` over a list of packages.
#[test]
fn a_functor_set_satisfies_a_function_position() {
    assert_eq!(
        outcome(r#"builtins.any { __functor = self: x: x > 1; } [ 2 ]"#),
        "value true"
    );
    assert_eq!(
        outcome(r#"builtins.tryEval (builtins.any { a = 1; } [ 2 ])"#),
        "uncatchable expected a function but found a set"
    );
}

/// The positions each builtin leaves unforced, named one by one.
///
/// A count would not do this job: the point of the check is that a position
/// silently dropped from a `strict` list turns a strict argument lazy, which
/// is a wrong value and not an error, and the population it would be counted
/// against is the same size either way.
const UNFORCED: &[(&str, &[usize])] = &[
    // cppnix's `foldl'` forces the function and the list but not the initial
    // accumulator, which the corpus checks with a `throw` there.
    ("foldl'", &[1]),
    // `prim_mapAttrs` forces the set and never the function: it builds one
    // `mkApp` per attribute and hands the callee over without inspecting it.
    // Declaring position 0 made the builtin strict in its function, which is
    // observable wherever a set reaches itself through one -- nixpkgs'
    // `idrisPackages` is `{ ... } // mapAttrs self.f { ... }` and reported
    // infinite recursion (ENG-13124). The callee is forced at the
    // application instead, by `ApplyChain`.
    ("mapAttrs", &[0]),
    // `prim_hashFile` validates the algorithm before it touches the path,
    // so a bad algorithm name is reported even when the path diverges. The
    // body forces position 1 itself once the name has passed.
    ("hashFile", &[1]),
    ("deepSeq", &[0, 1]),
    ("tryEval", &[0]),
    ("toJSON", &[0]),
    // Same reason, same walk: `prim_toXML` hands the argument to
    // `printValueAsXML`, whose first act is `forceValue` with no type
    // demanded, so the machine forces it and no tag here could name a type
    // cppnix checks.
    ("toXML", &[0]),
    // The message is coerced only on the error path.
    ("addErrorContext", &[0]),
    // The line is printed before the value is forced, which is cppnix's
    // order and the order that matters when the value throws.
    ("trace", &[1]),
    ("warn", &[1]),
    // Both positions, and the reason is the setting. With `trace-verbose` off
    // cppnix runs `prim_second` (`primops.cc:1408`), which forces argument 1
    // and never touches argument 0, so a strict entry for position 0 would
    // kill `builtins.traceVerbose (throw "x") 1`, which cppnix answers `1`.
    // With the setting on it is `prim_trace`, and the machine forces the
    // message itself.
    ("traceVerbose", &[0, 1]),
    // Both positions, and the machine forces both -- just not before the
    // body. `prim_filterSource` (`primops.cc:3004`) coerces argument 1 to a
    // path and only then calls `forceFunction` on argument 0, and the
    // coercion has to run a `__toString` to completion when the argument is
    // a set. A `strict` list can order the two forces; it cannot put a
    // sub-evaluation between them, so declaring position 0 here would report
    // a non-function filter ahead of a `__toString` that throws. The machine
    // drives both in cppnix's order instead (`bi_filter_source`).
    ("filterSource", &[0, 1]),
    // The guest decides. `builtins.wasm` hands its argument to the module as
    // a handle, and the module forces exactly what it inspects (`get_type`
    // on a thunk forces it, `copy_list` into a sizing buffer does not), so
    // `builtins.wasm cfg [ (throw "a") 1 ]` can answer `2` where a strict
    // position 1 would throw before the guest ran. The config set at 0 is
    // read by the machine itself and stays strict.
    ("wasm", &[1]),
];

/// Every `strict` list names each position at most once and none out of
/// range, and the positions it leaves out are exactly the ones above.
#[test]
fn every_strict_list_agrees_with_its_arity() {
    let mut wrong: Vec<String> = Vec::new();
    for b in TABLE {
        let named: Vec<usize> = b.strict.iter().map(|&(i, _)| i).collect();
        for (n, &i) in named.iter().enumerate() {
            if i >= b.arity {
                wrong.push(format!(
                    "{}: position {i} is past arity {}",
                    b.name, b.arity
                ));
            }
            if named.iter().take(n).any(|&j| j == i) {
                wrong.push(format!("{}: position {i} forced twice", b.name));
            }
        }
        let mut unforced: Vec<usize> = (0..b.arity).filter(|i| !named.contains(i)).collect();
        unforced.sort_unstable();
        let expected: &[usize] = UNFORCED
            .iter()
            .find(|(n, _)| *n == b.name)
            .map(|&(_, p)| p)
            .unwrap_or(&[]);
        if unforced != expected {
            wrong.push(format!(
                "{}: leaves {unforced:?} unforced, expected {expected:?}",
                b.name
            ));
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
}

/// Every position states a tag, and the ones that state `Any` are the ones
/// this change knowingly left open. Listed rather than counted for the same
/// reason as `UNFORCED`: a tag quietly downgraded to `Any` reinstates the
/// swallowed type error, and the total does not move.
#[test]
fn the_untagged_positions_are_the_ones_named_here() {
    // (builtin, position) -> why cppnix does not check a type there.
    const ANY: &[(&str, usize)] = &[
        // Coerced, and the primop branches on the argument's type before it
        // coerces, so the tag cannot be `Coerce` and the body owns the
        // machine. `dirOf` is the only one; `BODY_COERCIONS` in
        // `tests/coercion_class.rs` is the list, and the class gate holds it
        // to the C++ like any other row.
        ("dirOf", 0),
        // NOT coerced: `prim_replaceStrings` takes its subject with
        // `forceString` (`primops.cc:5169`), so a path there is a type error
        // in both arms. `Any` rather than `Str` only because the body's
        // `want_str` raises the same message at the same moment; the class
        // gate asserts this position is not a coercion site.
        ("replaceStrings", 2),
        // Not forced until the container is known non-empty, so a check here
        // would reject `map 1 []`, which cppnix accepts.
        ("map", 0),
        ("filter", 0),
        ("sort", 0),
        // Never forced by the primop at all. `mapAttrs` is the same shape
        // and is deliberately absent: it declares no position 0, so the
        // driver does not force it either, and it is named in `UNFORCED`
        // instead (ENG-13124). `elem` still declares one, so `elem (throw
        // "x") []` still diverges under ENG-12698.
        ("elem", 0),
        // A union no tag spells: `forceValue` and then a numeric dispatch.
        ("add", 0),
        ("add", 1),
        ("sub", 0),
        ("sub", 1),
        ("mul", 0),
        ("mul", 1),
        ("div", 0),
        ("div", 1),
        ("lessThan", 0),
        ("lessThan", 1),
        ("seq", 0),
        ("seq", 1),
        ("floor", 0),
        ("ceil", 0),
        // Any value at all: the type tests, `typeOf`, and the two positions
        // whose whole job is to hand a value back.
        ("isNull", 0),
        ("typeOf", 0),
        ("isInt", 0),
        ("isFloat", 0),
        ("isBool", 0),
        ("isString", 0),
        ("isPath", 0),
        ("isList", 0),
        ("isAttrs", 0),
        ("isFunction", 0),
        ("addErrorContext", 1),
        ("trace", 0),
        // `functionArgs` raises its own message rather than `forceFunction`'s.
        ("functionArgs", 0),
        ("import", 0),
        ("readFile", 0),
        ("pathExists", 0),
        ("readDir", 0),
        ("readFileType", 0),
        // Same family: cppnix's `prim_toPath` coerces
        // (`coerceToPath`, primops.cc), so a set with `outPath` and a bare
        // string are both accepted and a tag would reject them.
        ("toPath", 0),
        // `prim_storePath` coerces with the argument's string context kept:
        // a store-path string that carries a `.drv` reference hands that
        // reference on to its result without realising it. The `Coerce`
        // tag yields a bare path, so the body does the coercion.
        ("storePath", 0),
        // An attribute set, but `Attrs` is not the tag either:
        // `prim_flakeRefToString` uses `forceAttrs` with its own error
        // context, and the body's `want_attrs` raises that same message at
        // the same moment (the same reason `replaceStrings` 2 is untagged).
        ("flakeRefToString", 0),
        // The fixed-output fetchers take either a bare URL string or an
        // attribute set, and cppnix's `fetch()` opens with a plain
        // `forceValue` and branches on the type. A `Str` tag here would
        // reject `builtins.fetchurl { url = ...; }`, which is the spelling
        // nixpkgs uses.
        ("fetchurl", 0),
        ("fetchTarball", 0),
        // The tree fetchers, same reason: cppnix's `fetchTree()` opens with
        // `forceValue` and branches. The attribute-set spelling is the one
        // this backend serves and a bare string is refused by name, but the
        // refusal is the body's, not a type error the table could raise --
        // cppnix accepts the string, so tagging `Str` here would report a
        // *type* failure where cppnix has none.
        ("fetchTree", 0),
        ("fetchGit", 0),
        // The internal third spelling of the tree question, which takes the
        // same argument under the same rule. Untagged for the same reason,
        // even though no program can reach it: the tag decides what the
        // driver raises, and it must raise what the shared body would.
        ("fetchFinalTree", 0),
    ];
    let mut untagged: Vec<(&str, usize)> = Vec::new();
    for b in TABLE {
        for &(i, ty) in b.strict {
            if ty == ArgType::Any {
                untagged.push((b.name, i));
            }
        }
    }
    untagged.sort_unstable();
    let mut expected = ANY.to_vec();
    expected.sort_unstable();
    assert_eq!(untagged, expected);
}
