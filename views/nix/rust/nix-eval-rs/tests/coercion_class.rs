//! Exercise declared coercion rules through the Rust evaluator.

use nix_eval_rs::builtins::{ArgType, TABLE};
use nix_eval_rs::eval::{EvalError, eval_str};
use nix_eval_rs::print::CoerceFlags;
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Site {
    /// `*args[N]`.
    Arg(usize),
    /// `*elem`, `*elems[i]`, `*v2`: an element of a list argument.
    Element,
    /// `*i->value`, `*attr.value`: an attribute of a set argument.
    AttrValue,
}

/// A coercion cppnix performs, or this crate performs, keyed the same way.
type Rows = BTreeMap<(String, Site), CoerceFlags>;

const BODY_COERCIONS: &[(&str, Site, CoerceFlags, &str)] = &[
    (
        "dirOf",
        Site::Arg(0),
        CoerceFlags::NEITHER,
        "builtins.rs, bi_dir_of: cppnix answers a path for a path before it \
         coerces anything, so the body needs the value and cannot have it \
         replaced by its coercion",
    ),
    (
        "concatStringsSep",
        Site::Element,
        CoerceFlags::DEFAULTS,
        "print.rs, Coerce::joining: a list element, not an argument position",
    ),
    (
        "derivationStrict",
        Site::Element,
        CoerceFlags::DERIVATION_ATTR,
        "drvstrict.rs, Task::coerce_copying: an element of the `args` \
         attribute, not an argument position",
    ),
    (
        "derivationStrict",
        Site::AttrValue,
        CoerceFlags::DERIVATION_ATTR,
        "drvstrict.rs, Task::coerce_copying: a derivation attribute, not an \
         argument position",
    ),
    (
        "findFile",
        Site::AttrValue,
        CoerceFlags::NEITHER,
        "primops_host.rs, FindStage::PathValue: the `path` attribute of a \
         search-path entry, not an argument position",
    ),
];

fn implemented() -> Vec<&'static str> {
    TABLE.iter().map(|b| b.name).collect()
}

/// Every coercion this crate declares, from the table and the body list.
fn crate_coercions() -> Rows {
    let mut rows: Rows = BTreeMap::new();
    for b in TABLE {
        for &(pos, ty) in b.strict {
            if let ArgType::Coerce(flags) = ty {
                rows.insert((b.name.to_owned(), Site::Arg(pos)), flags);
            }
        }
    }
    for (name, site, flags, _) in BODY_COERCIONS {
        assert!(
            rows.insert(((*name).to_owned(), *site), *flags).is_none(),
            "{name} declares the same site twice, once in the table and once \
             in BODY_COERCIONS; one of them is not doing anything"
        );
    }
    rows
}

// -- the gate ---------------------------------------------------------------
// -- the behavioural half ---------------------------------------------------

/// A store that answers with a path derived from the source path, so a test
/// can tell a coerced path from an uncoerced one without a real store.
fn fake_copy(path: &nix_eval_rs::value2::PathValue) -> Result<String, String> {
    let base = path.accessor_path().rsplit('/').next().unwrap_or("x");
    Ok(format!("/nix/store/{}-{base}", "0".repeat(32)))
}

/// One expression per coercing builtin, with `SUBJECT` where the coerced
/// argument goes. Every builtin the gate says coerces an argument position
/// must appear here, which is what stops a row being added to the table
/// without anything ever running it.
const PROBES: &[(&str, &str)] = &[
    ("stringLength", "builtins.stringLength SUBJECT"),
    ("substring", "builtins.substring 0 11 SUBJECT"),
    ("toString", "builtins.toString SUBJECT"),
    ("baseNameOf", "builtins.baseNameOf SUBJECT"),
    ("dirOf", "builtins.dirOf SUBJECT"),
    (
        "throw",
        "(builtins.tryEval (builtins.throw SUBJECT)).success",
    ),
    ("abort", "builtins.typeOf SUBJECT"),
    (
        "unsafeDiscardStringContext",
        "builtins.unsafeDiscardStringContext SUBJECT",
    ),
    (
        "unsafeDiscardOutputDependency",
        "builtins.unsafeDiscardOutputDependency SUBJECT",
    ),
    (
        "addDrvOutputDependencies",
        "builtins.stringLength (builtins.unsafeDiscardStringContext SUBJECT)",
    ),
];

/// Every builtin whose argument position the gate says is coerced has a probe
/// that runs it. Without this, a row can be added to the table and be wrong
/// in the body, and the class gate above would still pass.
#[test]
fn every_coercing_argument_position_has_a_probe() {
    let mut unprobed: Vec<String> = crate_coercions()
        .keys()
        .filter(|(_, site)| matches!(site, Site::Arg(_)))
        .map(|(name, _)| name.clone())
        .filter(|name| !PROBES.iter().any(|(n, _)| n == name))
        .collect();
    unprobed.sort_unstable();
    unprobed.dedup();
    assert!(
        unprobed.is_empty(),
        "these coerce an argument and nothing evaluates them with a \
         non-string there: {unprobed:?}"
    );
    // And the probes are for builtins that exist.
    let implemented = implemented();
    for (name, _) in PROBES {
        assert!(
            implemented.contains(name),
            "{name} has a probe and is not implemented"
        );
    }
}

/// A set coerces at every position the table says coerces: through
/// `__toString`, and through `outPath` for a derivation.
///
/// This is the half the C++ scan cannot check. A body that reached for
/// `want_str` would raise "expected a string but found a set" here.
#[test]
fn a_set_coerces_at_every_coercing_position() {
    for (name, probe) in PROBES {
        for subject in [
            r#"{ __toString = self: "/nix/store/00000000000000000000000000000000-x"; }"#,
            r#"{ type = "derivation"; outPath = "/nix/store/00000000000000000000000000000000-x"; }"#,
        ] {
            let src = probe.replace("SUBJECT", subject);
            let out = eval_str(&src);
            assert!(
                out.is_ok(),
                "{name} rejects a set at its coerced position: {src} gave {out:?}"
            );
        }
    }
}

/// A path coerces too, and where cppnix's `copyToStore` is on the answer is
/// about the store path rather than the source path. `stringLength ./f` is
/// the length of the store path, which is ENG-12854's repro.
#[test]
fn a_path_reaches_the_store_where_copy_to_store_is_on() {
    /// Evaluate against a host whose one answer is the faked store copy.
    ///
    /// The host is this call's argument, so nothing else in the binary can
    /// see it and nothing else has to be held still while it exists.
    fn with_store(src: &str) -> Result<String, EvalError> {
        let host = nix_eval_rs::host::FnHost {
            store_copy: Some(fake_copy),
            ..nix_eval_rs::host::FnHost::default()
        };
        let mut vm = nix_eval_rs::vm::Vm::from_process_settings();
        nix_eval_rs::eval::eval_str_on(
            src,
            ".",
            nix_eval_rs::compile::Origin::String,
            &mut vm,
            &host,
        )
    }

    // 11 for "/nix/store/", 32 for the hash, 1 for the dash, and the name.
    let store_len = 11 + 32 + 1 + "lib.nix".len();
    assert_eq!(
        with_store("builtins.stringLength ./lib.nix").ok(),
        Some(store_len.to_string()),
        "stringLength of a path must be the length of the store path cppnix \
         copies it to (ENG-12854), not of the source path"
    );
    assert_eq!(
        with_store("builtins.substring 0 11 ./lib.nix").ok(),
        Some("\"/nix/store/\"".to_owned())
    );
    // `copyToStore` off: the source path, and no store question at all.
    assert_eq!(
        with_store("builtins.baseNameOf ./dir/lib.nix").ok(),
        Some("\"lib.nix\"".to_owned())
    );
}

/// `coerceMore` is off everywhere except `toString`, and that half matters as
/// much: a coercion that took every value would answer where cppnix raises a
/// type error, which is the same class of divergence pointing the other way.
#[test]
fn coerce_more_stays_off_where_cppnix_leaves_it_off() {
    for src in [
        "builtins.stringLength 42",
        "builtins.substring 0 1 42",
        "builtins.baseNameOf 42",
        "builtins.stringLength [ \"a\" ]",
    ] {
        let out = eval_str(src);
        assert!(
            matches!(&out, Err(EvalError::Eval(_, m, _)) if m.starts_with("cannot coerce")),
            "{src} must be cppnix's `cannot coerce ...` type error, got {out:?}"
        );
    }
    // toString is the one primop that sets it.
    assert_eq!(
        eval_str("builtins.toString 42").ok(),
        Some("\"42\"".to_owned())
    );
    assert_eq!(
        eval_str("builtins.toString [ 1 2 ]").ok(),
        Some("\"1 2\"".to_owned())
    );
}
