//! Builtin functions. The table index is the IR-level contract: a compiled
//! module referencing builtin N means entry N of this table. The compiler
//! fingerprint invalidates cached modules whenever this source changes.
//!
//! A builtin is either `Pure` -- every argument arrives already forced and the
//! result needs no further evaluation -- or `Start`, which returns a
//! continuation the machine drives. No builtin re-enters the interpreter, so
//! none of them can put Nix-value-proportional depth on the host stack.

use crate::primops_host as host;
use crate::primops_pure::{self as pure, Begin, argv};
use crate::print::CoerceFlags;
use crate::value2::{Attrs, NixStr, Slot, Value, type_name};
use crate::vm::{Vm, VmError};
use std::collections::BTreeMap;
use std::rc::Rc;

type Result<T> = std::result::Result<T, VmError>;

pub enum Kind {
    /// Arguments pre-forced -- and pre-coerced, where the position is
    /// [`ArgType::Coerce`] -- and the result immediate.
    Pure(fn(&mut Vm, &[Slot]) -> Result<Value>),
    /// Needs more evaluation; says how to continue.
    Start(fn(&mut Vm, &[Slot]) -> Result<Begin>),
}

/// The type an argument position is checked against the moment it is forced,
/// which is the moment cppnix's primop checks it.
///
/// A tag is not documentation: the machine raises the error, so a position
/// tagged wrongly is a wrong answer. Each variant delegates to the `want_*`
/// helper the bodies use, so cppnix's wording has one definition per type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgType {
    /// Checked by nobody here. Two different reasons, both of them real:
    /// cppnix accepts a union the tag cannot spell (`add`, `lessThan`), or it
    /// does not force the position until later and a check here would fire on
    /// a program cppnix accepts (`map`'s function, which is never forced when
    /// the list is empty).
    ///
    /// **Not** the tag for a position cppnix coerces. That is
    /// [`ArgType::Coerce`], which says so and makes it happen. `Any` plus a
    /// body reaching for `want_str` looks exactly like a position cppnix
    /// demands a string at, which is how `stringLength` came to reject a path
    /// cppnix answers about (ENG-12854).
    Any,
    /// cppnix takes this position with `coerceToString`, with these flags.
    ///
    /// The driver runs the coercion and **replaces the argument with the
    /// coerced string**, so the body sees a string however it was written and
    /// cannot get the rule wrong. That is why this is a tag and not a
    /// convention: four fixes in a row (ENG-12628, ENG-12669, ENG-12670,
    /// ENG-12854) were one builtin each reaching for `want_str` where cppnix
    /// coerces, and a fifth was free.
    ///
    /// A position whose primop branches on the argument's type *before*
    /// coercing cannot be tagged here, because the body needs the value and
    /// not the string; `dirOf` is the one such primop. Those sites are listed
    /// in `BODY_COERCIONS` in `tests/coercion_class.rs` and the class gate
    /// holds them to the C++ the same way.
    Coerce(CoerceFlags),
    /// `forceString`: a string, context allowed.
    Str,
    /// `forceStringNoCtx`: a string that may not refer to a store path.
    StrNoCtx,
    Int,
    List,
    Attrs,
    /// `forceFunction`, which accepts a `__functor` set as well as a lambda
    /// or a primop.
    Function,
}

pub struct Builtin {
    pub name: &'static str,
    pub arity: usize,
    pub kind: Kind,
    /// The arguments the machine forces before the body runs -- in the order
    /// cppnix's primop forces them, each with the type cppnix checks there.
    ///
    /// Two things are encoded here rather than assumed, because assuming
    /// either one produces a wrong value rather than a wrong message:
    ///
    /// * **Which positions are forced at all.** A position absent from this
    ///   list is never forced before the body, which is `foldl'`'s
    ///   accumulator and `tryEval`'s argument. Absence rather than a second
    ///   `lazy` slice, so the two cannot disagree.
    /// * **The order, and the type at each step.** cppnix checks a type the
    ///   instant it forces the argument, so a type error at one position
    ///   beats a `throw` at a later one. Forcing everything and letting the
    ///   body check afterwards loses that race and hands `tryEval` a
    ///   `{ success = false; }` where cppnix dies uncatchably (ENG-12674).
    ///   The order is cppnix's, not left to right: `builtins.map` forces its
    ///   list before its function, so `map (throw "x") 1` is a type error
    ///   there and a caught throw under a positional walk.
    pub strict: &'static [(usize, ArgType)],
}

impl ArgType {
    /// Raise cppnix's type error for this position, or pass.
    ///
    /// Uncatchable on purpose: every `want_*` here builds a `VmError::eval`,
    /// which `tryEval` does not swallow, and neither does cppnix -- its
    /// `prim_tryEval` catches `AssertionError` only, so a `TypeError` from
    /// `forceInt` kills the whole evaluation. That asymmetry is the entire
    /// point of checking here: a caught `throw` in a later argument would
    /// otherwise turn a dead program into `{ success = false; }`.
    pub fn check(self, vm: &mut Vm, v: &Value) -> Result<()> {
        match self {
            ArgType::Any => Ok(()),
            // The driver has already run the coercion and put its result in
            // the argument's place; there is nothing left to reject.
            ArgType::Coerce(_) => Ok(()),
            ArgType::Str => pure::want_nix_str(v).map(|_| ()),
            ArgType::StrNoCtx => pure::want_bytes_no_ctx(v).map(|_| ()),
            ArgType::Int => pure::want_int(v).map(|_| ()),
            ArgType::List => pure::want_list(v).map(|_| ()),
            ArgType::Attrs => pure::want_attrs(v).map(|_| ()),
            ArgType::Function => want_function(vm, v),
        }
    }
}

/// cppnix's `forceFunction` (`eval.cc:2505`), which passes a set carrying
/// `__functor` as well as a lambda or a primop -- the shape everything built
/// on `lib.makeOverridable` has, so refusing it would break `map` over a list
/// of them. A set without the attribute is still "expected a function but
/// found a set", cppnix's wording for the same case.
fn want_function(vm: &mut Vm, v: &Value) -> Result<()> {
    let ok = match v {
        Value::Closure(_) | Value::Builtin(_) => true,
        Value::Attrs(m) => {
            let functor = vm.intern("__functor");
            m.contains_key(&functor)
        }
        _ => false,
    };
    if ok {
        return Ok(());
    }
    Err(VmError::eval(format!(
        "expected a function but found {}",
        type_name(v)
    )))
}

macro_rules! pure_bi {
    ($name:literal, $arity:literal, $f:path, $strict:expr) => {
        Builtin {
            name: $name,
            arity: $arity,
            kind: Kind::Pure($f),
            strict: $strict,
        }
    };
}

macro_rules! start_bi {
    ($name:literal, $arity:literal, $f:path, $strict:expr) => {
        Builtin {
            name: $name,
            arity: $arity,
            kind: Kind::Start($f),
            strict: $strict,
        }
    };
}

/// In global scope (usable bare, like `map` or `throw`); everything is also
/// reachable as `builtins.<name>`.
pub static TABLE: &[Builtin] = &[
    // The message is coerced, not demanded: `throw ./f` copies the file into
    // the store and throws its store path (`primops.cc`, prim_throw, which
    // passes neither flag), and the same for `abort`.
    pure_bi!(
        "throw",
        1,
        bi_throw,
        &[(0, ArgType::Coerce(CoerceFlags::DEFAULTS))]
    ),
    pure_bi!(
        "abort",
        1,
        bi_abort,
        &[(0, ArgType::Coerce(CoerceFlags::DEFAULTS))]
    ),
    // `coerceToString(..., true, false)` (`primops.cc`, prim_toString): the
    // one primop that sets `coerceMore` and clears `copyToStore`, which is
    // why `toString 1` is "1" and `toString ./f` is the source path. The
    // coercion is the whole builtin, so the body hands its argument back.
    pure_bi!(
        "toString",
        1,
        bi_to_string,
        &[(0, ArgType::Coerce(CoerceFlags::TO_STRING))]
    ),
    // cppnix forces the list first and only reaches `forceFunction` when it
    // is non-empty (`prim_map`), which is both halves of this entry: the
    // order is (1, 0), and position 0 is `Any` because a check there would
    // reject `map 1 []`, which cppnix answers `[ ]`. Same shape in `filter`
    // and `sort`. This driver still forces position 0, so `map (throw "x")
    // []` diverges; closing that needs the body to force it (ENG-12698).
    start_bi!(
        "map",
        2,
        pure::bi_map,
        &[(1, ArgType::List), (0, ArgType::Any)]
    ),
    pure_bi!("isNull", 1, bi_is_null, &[(0, ArgType::Any)]),
    // `false, false` (`primops.cc`, prim_baseNameOf): a set coerces through
    // `__toString` or `outPath`, and a path stays a source path rather than
    // being copied, so `baseNameOf ./x/y.nix` is "y.nix" and not the basename
    // of a store path.
    pure_bi!(
        "baseNameOf",
        1,
        bi_base_name_of,
        &[(0, ArgType::Coerce(CoerceFlags::NEITHER))]
    ),
    // The same flags, and the coercion cannot be declared here: cppnix's
    // `prim_dirOf` answers a PATH for a path before it coerces anything, so
    // the body needs the value. It runs the same machine itself.
    start_bi!("dirOf", 1, bi_dir_of, &[(0, ArgType::Any)]),
    pure_bi!("length", 1, pure::bi_length, &[(0, ArgType::List)]),
    start_bi!("head", 1, pure::bi_head, &[(0, ArgType::List)]),
    pure_bi!("tail", 1, pure::bi_tail, &[(0, ArgType::List)]),
    // `prim_elemAt` forces the index before the list, so `elemAt (throw "x")
    // "s"` is a type error in cppnix and a caught throw under a left-to-right
    // walk.
    start_bi!(
        "elemAt",
        2,
        pure::bi_elem_at,
        &[(1, ArgType::Int), (0, ArgType::List)]
    ),
    // cppnix never forces the value being searched for: `eqValues` forces it
    // element by element, so it stays `Any` here. It is still forced eagerly
    // by this driver, which is why `elem (throw "x") []` diverges (ENG-12698).
    start_bi!(
        "elem",
        2,
        pure::bi_elem,
        &[(1, ArgType::List), (0, ArgType::Any)]
    ),
    start_bi!(
        "filter",
        2,
        pure::bi_filter,
        &[(1, ArgType::List), (0, ArgType::Any)]
    ),
    start_bi!(
        "any",
        2,
        pure::bi_any,
        &[(0, ArgType::Function), (1, ArgType::List)]
    ),
    start_bi!(
        "all",
        2,
        pure::bi_all,
        &[(0, ArgType::Function), (1, ArgType::List)]
    ),
    start_bi!(
        "concatLists",
        1,
        pure::bi_concat_lists,
        &[(0, ArgType::List)]
    ),
    start_bi!(
        "concatMap",
        2,
        pure::bi_concat_map,
        &[(0, ArgType::Function), (1, ArgType::List)]
    ),
    // Length first, then the generator, and unlike `map` the generator is
    // forced whatever the length is (`prim_genList`).
    start_bi!(
        "genList",
        2,
        pure::bi_gen_list,
        &[(1, ArgType::Int), (0, ArgType::Function)]
    ),
    start_bi!(
        "foldl'",
        3,
        pure::bi_foldl_strict,
        &[(0, ArgType::Function), (2, ArgType::List)]
    ),
    start_bi!(
        "sort",
        2,
        pure::bi_sort,
        &[(1, ArgType::List), (0, ArgType::Any)]
    ),
    pure_bi!("attrNames", 1, pure::bi_attr_names, &[(0, ArgType::Attrs)]),
    pure_bi!(
        "attrValues",
        1,
        pure::bi_attr_values,
        &[(0, ArgType::Attrs)]
    ),
    start_bi!(
        "getAttr",
        2,
        pure::bi_get_attr,
        &[(0, ArgType::StrNoCtx), (1, ArgType::Attrs)]
    ),
    pure_bi!(
        "hasAttr",
        2,
        pure::bi_has_attr,
        &[(0, ArgType::StrNoCtx), (1, ArgType::Attrs)]
    ),
    start_bi!(
        "removeAttrs",
        2,
        pure::bi_remove_attrs,
        &[(0, ArgType::Attrs), (1, ArgType::List)]
    ),
    pure_bi!(
        "intersectAttrs",
        2,
        pure::bi_intersect_attrs,
        &[(0, ArgType::Attrs), (1, ArgType::Attrs)]
    ),
    start_bi!(
        "catAttrs",
        2,
        pure::bi_cat_attrs,
        &[(0, ArgType::StrNoCtx), (1, ArgType::List)]
    ),
    start_bi!(
        "listToAttrs",
        1,
        pure::bi_list_to_attrs,
        &[(0, ArgType::List)]
    ),
    // `prim_mapAttrs` forces the set and never the function -- it builds
    // applications without inspecting it -- so position 0 is absent from this
    // list and the driver never forces it. The body keeps it as a slot and
    // `SlotState::PendingApply` forces it at the application, which is where
    // cppnix does.
    //
    // Declaring it here cost a flip: nixpkgs' `idrisPackages` is
    // `{ ... } // mapAttrs self.build-builtin-package { ... }`, so forcing
    // the function re-entered the `self` thunk that was still being built and
    // reported `infinite recursion encountered` for a package cppnix
    // evaluates (ENG-13124). `mapAttrs (throw "x") {}` is the small version.
    start_bi!("mapAttrs", 2, pure::bi_map_attrs, &[(1, ArgType::Attrs)]),
    start_bi!(
        "groupBy",
        2,
        pure::bi_group_by,
        &[(0, ArgType::Function), (1, ArgType::List)]
    ),
    start_bi!(
        "partition",
        2,
        pure::bi_partition,
        &[(0, ArgType::Function), (1, ArgType::List)]
    ),
    // Both take their subject with `coerceToString` and neither flag
    // (`primops.cc`, prim_stringLength and prim_substring), so a path is
    // copied into the store and the answer is about the store path:
    // `stringLength ./f.sh` is 48, not 5. Reached from `lib.hasSuffix` and
    // through it from `makeSetupHook`, which is three nixpkgs packages that
    // stopped evaluating while this said `Any` (ENG-12854).
    pure_bi!(
        "stringLength",
        1,
        pure::bi_string_length,
        &[(0, ArgType::Coerce(CoerceFlags::DEFAULTS))]
    ),
    pure_bi!(
        "substring",
        3,
        pure::bi_substring,
        &[
            (0, ArgType::Int),
            (1, ArgType::Int),
            (2, ArgType::Coerce(CoerceFlags::DEFAULTS))
        ]
    ),
    start_bi!(
        "concatStringsSep",
        2,
        pure::bi_concat_strings_sep,
        &[(0, ArgType::Str), (1, ArgType::List)]
    ),
    start_bi!(
        "replaceStrings",
        3,
        pure::bi_replace_strings,
        &[(0, ArgType::List), (1, ArgType::List), (2, ArgType::Any)]
    ),
    pure_bi!(
        "splitVersion",
        1,
        pure::bi_split_version,
        &[(0, ArgType::StrNoCtx)]
    ),
    pure_bi!(
        "seq",
        2,
        pure::bi_seq,
        &[(0, ArgType::Any), (1, ArgType::Any)]
    ),
    start_bi!("deepSeq", 2, pure::bi_deep_seq, &[]),
    start_bi!("tryEval", 1, pure::bi_try_eval, &[]),
    pure_bi!(
        "functionArgs",
        1,
        pure::bi_function_args,
        &[(0, ArgType::Any)]
    ),
    pure_bi!("typeOf", 1, pure::bi_type_of, &[(0, ArgType::Any)]),
    pure_bi!("isInt", 1, pure::bi_is_int, &[(0, ArgType::Any)]),
    pure_bi!("isFloat", 1, pure::bi_is_float, &[(0, ArgType::Any)]),
    pure_bi!("isBool", 1, pure::bi_is_bool, &[(0, ArgType::Any)]),
    pure_bi!("isString", 1, pure::bi_is_string, &[(0, ArgType::Any)]),
    pure_bi!("isPath", 1, pure::bi_is_path, &[(0, ArgType::Any)]),
    pure_bi!("isList", 1, pure::bi_is_list, &[(0, ArgType::Any)]),
    pure_bi!("isAttrs", 1, pure::bi_is_attrs, &[(0, ArgType::Any)]),
    pure_bi!("isFunction", 1, pure::bi_is_function, &[(0, ArgType::Any)]),
    pure_bi!(
        "add",
        2,
        pure::bi_add,
        &[(0, ArgType::Any), (1, ArgType::Any)]
    ),
    pure_bi!(
        "sub",
        2,
        pure::bi_sub,
        &[(0, ArgType::Any), (1, ArgType::Any)]
    ),
    pure_bi!(
        "mul",
        2,
        pure::bi_mul,
        &[(0, ArgType::Any), (1, ArgType::Any)]
    ),
    pure_bi!(
        "div",
        2,
        pure::bi_div,
        &[(0, ArgType::Any), (1, ArgType::Any)]
    ),
    start_bi!(
        "lessThan",
        2,
        pure::bi_less_than,
        &[(0, ArgType::Any), (1, ArgType::Any)]
    ),
    pure_bi!(
        "bitAnd",
        2,
        pure::bi_bit_and,
        &[(0, ArgType::Int), (1, ArgType::Int)]
    ),
    pure_bi!(
        "bitOr",
        2,
        pure::bi_bit_or,
        &[(0, ArgType::Int), (1, ArgType::Int)]
    ),
    pure_bi!(
        "bitXor",
        2,
        pure::bi_bit_xor,
        &[(0, ArgType::Int), (1, ArgType::Int)]
    ),
    pure_bi!("floor", 1, pure::bi_floor, &[(0, ArgType::Any)]),
    pure_bi!("ceil", 1, pure::bi_ceil, &[(0, ArgType::Any)]),
    pure_bi!(
        "compareVersions",
        2,
        pure::bi_compare_versions,
        &[(0, ArgType::StrNoCtx), (1, ArgType::StrNoCtx)]
    ),
    // The path family takes a string, a path, or a set that coerces through
    // `__toString` or `outPath` -- cppnix's `coerceToPath`, which ENG-12669
    // brought over and which can call back into evaluation. There is no type
    // to check, so these positions stay `Any` and that coercion owns them.
    start_bi!("import", 1, host::bi_import, &[(0, ArgType::Any)]),
    start_bi!("readFile", 1, host::bi_read_file, &[(0, ArgType::Any)]),
    start_bi!("pathExists", 1, host::bi_path_exists, &[(0, ArgType::Any)]),
    start_bi!("storePath", 1, host::bi_store_path, &[(0, ArgType::Any)]),
    start_bi!("readDir", 1, host::bi_read_dir, &[(0, ArgType::Any)]),
    start_bi!(
        "readFileType",
        1,
        host::bi_read_file_type,
        &[(0, ArgType::Any)]
    ),
    start_bi!(
        "genericClosure",
        1,
        pure::bi_generic_closure,
        &[(0, ArgType::Attrs)]
    ),
    start_bi!("getEnv", 1, host::bi_get_env, &[(0, ArgType::StrNoCtx)]),
    start_bi!(
        "zipAttrsWith",
        2,
        pure::bi_zip_attrs_with,
        &[(0, ArgType::Function), (1, ArgType::List)]
    ),
    // The algorithm name is validated between the two arguments, so
    // `hashString "nope" (throw "x")` is "unknown hash algorithm" in cppnix
    // and a caught throw here: the check that would fix it is not a type
    // check and has no place in this table (ENG-12697).
    start_bi!(
        "convertHash",
        1,
        host::bi_convert_hash,
        &[(0, ArgType::Attrs)]
    ),
    // The algorithm is validated between the two arguments for the same
    // reason as `hashString` below: cppnix parses it before it touches the
    // path (`prim_hashFile`), so a refused algorithm never forces the path.
    // The path position is therefore not in this list at all -- the body
    // coerces it through `coerce_for_read`, which accepts the whole path
    // family (a set with `outPath`, a string with context) that no single
    // tag spells, and only after the algorithm has been accepted.
    start_bi!("hashFile", 2, host::bi_hash_file, &[(0, ArgType::StrNoCtx)]),
    // The config set is forced and checked by the machine; `path` and
    // `function` inside it are forced by the body, in cppnix's order, and
    // the argument is never forced here at all -- the guest decides.
    start_bi!("wasm", 2, crate::wasm::bi_wasm, &[(0, ArgType::Attrs)]),
    pure_bi!(
        "hashString",
        2,
        pure::bi_hash_string,
        &[(0, ArgType::StrNoCtx), (1, ArgType::Str)]
    ),
    start_bi!(
        "toFile",
        2,
        host::bi_to_file,
        &[(0, ArgType::StrNoCtx), (1, ArgType::Str)]
    ),
    pure_bi!(
        "match",
        2,
        pure::bi_match,
        &[(0, ArgType::StrNoCtx), (1, ArgType::Str)]
    ),
    pure_bi!(
        "split",
        2,
        pure::bi_split,
        &[(0, ArgType::StrNoCtx), (1, ArgType::Str)]
    ),
    pure_bi!("fromJSON", 1, pure::bi_from_json, &[(0, ArgType::StrNoCtx)]),
    pure_bi!("fromTOML", 1, pure::bi_from_toml, &[(0, ArgType::StrNoCtx)]),
    start_bi!("toJSON", 1, host::bi_to_json, &[]),
    // Appended, never inserted: the table index is the IR-level contract, so
    // a compiled module referencing builtin N must keep meaning entry N.
    // The three context rewrites take their argument with `coerceToString`
    // and neither flag (`context.cc`), so each copies a path into the store
    // and rewrites the context of the copy.
    pure_bi!(
        "unsafeDiscardStringContext",
        1,
        host::bi_unsafe_discard_string_context,
        &[(0, ArgType::Coerce(CoerceFlags::DEFAULTS))]
    ),
    pure_bi!("hasContext", 1, pure::bi_has_context, &[(0, ArgType::Str)]),
    pure_bi!("getContext", 1, pure::bi_get_context, &[(0, ArgType::Str)]),
    start_bi!(
        "derivationStrict",
        1,
        crate::drvstrict::bi_derivation_strict,
        &[(0, ArgType::Attrs)]
    ),
    pure_bi!(
        "unsafeDiscardOutputDependency",
        1,
        host::bi_unsafe_discard_output_dependency,
        &[(0, ArgType::Coerce(CoerceFlags::DEFAULTS))]
    ),
    pure_bi!(
        "addDrvOutputDependencies",
        1,
        host::bi_add_drv_output_dependencies,
        &[(0, ArgType::Coerce(CoerceFlags::DEFAULTS))]
    ),
    start_bi!(
        "appendContext",
        2,
        host::bi_append_context,
        &[(0, ArgType::Str), (1, ArgType::Attrs)]
    ),
    // cppnix walks the search-path entries between forcing the list and
    // forcing the name, so a throwing entry beats a non-string name there and
    // not here. Both die; only the message differs (ENG-12697).
    start_bi!(
        "findFile",
        2,
        host::bi_find_file,
        &[(0, ArgType::List), (1, ArgType::StrNoCtx)]
    ),
    // Lazy in the message, as cppnix is: it is coerced only on the error
    // path, so a `throw` there never fires when nothing fails.
    start_bi!(
        "addErrorContext",
        2,
        pure::bi_add_error_context,
        &[(1, ArgType::Any)]
    ),
    // Lazy in the value, so the line is emitted before it is forced, which is
    // cppnix's order and the order that matters when the value throws.
    start_bi!("trace", 2, host::bi_trace, &[(0, ArgType::Any)]),
    start_bi!("warn", 2, host::bi_warn, &[(0, ArgType::Str)]),
    // Nothing strict, unlike `trace`. With `trace-verbose` off cppnix runs
    // `prim_second`, which forces argument 1 and never looks at argument 0,
    // so forcing it here would kill programs cppnix answers. The machine
    // forces the message itself on the arm that needs it.
    start_bi!("traceVerbose", 2, host::bi_trace_verbose, &[]),
    pure_bi!(
        "placeholder",
        1,
        pure::bi_placeholder,
        &[(0, ArgType::StrNoCtx)]
    ),
    pure_bi!(
        "unsafeGetAttrPos",
        2,
        pure::bi_unsafe_get_attr_pos,
        &[(0, ArgType::StrNoCtx), (1, ArgType::Attrs)]
    ),
    // cppnix's `prim_path` opens with `forceAttrs(*args[0], ...)` and does
    // nothing before it, so one step and one type.
    start_bi!("path", 1, host::bi_path, &[(0, ArgType::Attrs)]),
    // cppnix's `fetch()` opens with a bare `forceValue` and then branches on
    // the type, so the position is forced with no type demanded: an attribute
    // set and a string are both accepted, and anything else gets
    // `forceStringNoCtx`'s error from inside the body rather than a check
    // here that would report it against the wrong call.
    start_bi!("fetchurl", 1, host::bi_fetchurl, &[(0, ArgType::Any)]),
    start_bi!(
        "fetchTarball",
        1,
        host::bi_fetch_tarball,
        &[(0, ArgType::Any)]
    ),
    // Same shape as the two above: cppnix's `fetchTree()` opens with a bare
    // `forceValue` and branches on the type, so no type is demanded here.
    start_bi!("fetchTree", 1, host::bi_fetch_tree, &[(0, ArgType::Any)]),
    start_bi!("fetchGit", 1, host::bi_fetch_git, &[(0, ArgType::Any)]),
    // `prim_parseDrvName` opens with `forceStringNoCtx` and does nothing else
    // before the parse, so one position and one type. Appended here rather
    // than filed beside `splitVersion` and `compareVersions`, which are the
    // rest of cppnix's `DrvName` family: the index is the IR contract.
    pure_bi!(
        "parseDrvName",
        1,
        pure::bi_parse_drv_name,
        &[(0, ArgType::StrNoCtx)]
    ),
    // Nothing strict, and that is cppnix's order rather than an omission.
    // `prim_filterSource` coerces argument 1 to a path and only then calls
    // `forceFunction` on argument 0, and the coercion has to *complete* --
    // running a `__toString` if the argument is a set -- before argument 0 is
    // touched. This list can say "force position 1 first"; it cannot say
    // "finish coercing it first", so declaring position 0 here would report a
    // non-function filter ahead of a `__toString` that throws, which cppnix
    // reports the other way round. The machine forces both in cppnix's order
    // instead (see `bi_filter_source`).
    start_bi!("filterSource", 2, host::bi_filter_source, &[]),
    // Nothing strict, for the reason `toJSON` above has none: `prim_toXML`
    // hands the argument straight to `printValueAsXML`, whose first act is
    // `forceValue` with no type demanded, so a check here would report a
    // type cppnix never names.
    start_bi!("toXML", 1, pure::bi_to_xml, &[]),
    // cppnix's `fetchFinalTree`, which is `.internal = true` and therefore in
    // neither `builtins` nor the global scope; the owned catalogue marks it internal. A table entry
    // all the same, because the table index is how a compiled module names a
    // builtin and this one has to be nameable: `ixe_internal_primop` hands a
    // value built from this index to the embedder, which is cppnix's
    // `state.internalPrimOps` lookup with the same one member.
    start_bi!(
        "fetchFinalTree",
        1,
        host::bi_fetch_final_tree,
        &[(0, ArgType::Any)]
    ),
    // cppnix registers this one from libflake rather than from `primops.cc`:
    // `flake::Settings` pushes it onto `evalSettings.extraPrimOps`
    // (`settings.cc:14`) and `flake-primops.cc` declares
    // `experimentalFeature = Xp::Flakes`.
    //
    // Public flake helpers are exposed only when the flakes feature is enabled.
    start_bi!("getFlake", 1, host::bi_get_flake, &[(0, ArgType::StrNoCtx)]),
    // Flake reference helpers share the same feature gate as getFlake.
    start_bi!(
        "parseFlakeRef",
        1,
        host::bi_parse_flake_ref,
        &[(0, ArgType::StrNoCtx)]
    ),
    // `Any` and not a set-typed coercion: `prim_flakeRefToString` forces the
    // set itself and then each attribute in order, raising its own errors
    // between the forces, so the walk owns the argument.
    start_bi!(
        "flakeRefToString",
        1,
        host::bi_flake_ref_to_string,
        &[(0, ArgType::Any)]
    ),
    // `Any` and not `Coerce` for the same reason as `dirOf`: the coercion is
    // the builtin's whole body (a path value must come back untouched by
    // `coerceToString`, a set must run `__toString`), so the driver may not
    // replace the argument with a string before the body sees it.
    start_bi!("toPath", 1, pure::bi_to_path, &[(0, ArgType::Any)]),
];

pub fn global_index(name: &str) -> Option<u16> {
    TABLE.iter().position(|b| b.name == name).map(|i| i as u16)
}

/// Whether the language catalogue exposes this global under the active settings.
pub fn is_global(settings: &crate::eval::Settings, name: &str) -> bool {
    (crate::builtin_catalogue::GLOBAL_NAMES.contains(&name)
        || crate::builtin_catalogue::EXTRA_GLOBALS.contains(&name))
        && crate::builtin_catalogue::enabled(settings, name)
}

pub fn mk_value(idx: u16) -> Value {
    Value::Builtin(Rc::new(crate::value2::BuiltinData {
        idx,
        args: Vec::new(),
    }))
}

fn arg(args: &[Slot], i: usize) -> Result<&Slot> {
    args.get(i)
        .ok_or_else(|| VmError::eval("internal: missing builtin argument"))
}

fn bi_throw(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Err(VmError::thrown(want_message(&argv(args, 0)?)?))
}

fn bi_abort(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let msg = want_message(&argv(args, 0)?)?;
    // abort is deliberately not catchable: tryEval must not swallow it.
    Err(VmError::eval(format!(
        "evaluation aborted with the following error message: '{msg}'"
    )))
}

fn want_message(v: &Value) -> Result<String> {
    match v {
        // The message becomes error text, which is text: refuse a non-UTF-8
        // one by name rather than repairing it.
        Value::Str(s) => Ok(pure::text_of(s)?.to_owned()),
        other => Err(VmError::eval(format!(
            "expected a string but found {}",
            type_name(other)
        ))),
    }
}

/// `builtins.toString` IS its coercion, and the driver has run it already
/// (`ArgType::Coerce`), so what is left is to hand the string back. The
/// context comes with it: `toString` of a string that depends on a store path
/// depends on it too.
fn bi_to_string(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    argv(args, 0)
}

fn bi_is_null(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Ok(Value::Bool(matches!(argv(args, 0)?, Value::Null)))
}

fn bi_base_name_of(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let v = argv(args, 0)?;
    // Always a string: the driver coerced the argument, so a path arrived as
    // its own source path and a set through `__toString` or `outPath`.
    let s = pure::want_nix_str(&v)?;
    // The basename of a store-path-bearing string still refers to it, which
    // is the shape `eval-okay-context` is built out of.
    Ok(Value::Str(NixStr::with_context(
        legacy_base_name_of(s.bytes()),
        s.context_set(),
    )))
}

/// cppnix's baseNameOf is `legacyBaseNameOf`, not the utility of the same
/// name: it strips at most ONE trailing slash, so `baseNameOf "a//"` is ""
/// rather than "a". Upstream calls the behavior regrettable and keeps it.
fn legacy_base_name_of(path: &[u8]) -> &[u8] {
    if path.is_empty() {
        return b"";
    }
    let mut last = path.len() - 1;
    if path.get(last) == Some(&b'/') && last > 0 {
        last -= 1;
    }
    let pos = match path
        .get(..=last)
        .and_then(|head| head.iter().rposition(|&c| c == b'/'))
    {
        Some(i) => i + 1,
        None => 0,
    };
    if pos > last {
        return b"";
    }
    path.get(pos..=last).unwrap_or(b"")
}

/// cppnix's `prim_dirOf` forces its argument and answers a PATH for a path,
/// before any coercion happens; everything else it coerces with `false,
/// false` and answers a string. That type test is why this position cannot be
/// `ArgType::Coerce`: a coerced argument has already lost the distinction the
/// primop branches on.
fn bi_dir_of(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let v = argv(args, 0)?;
    match &v {
        Value::Path(p) => Ok(Begin::Done(Value::Path(Rc::new(p.parent())))),
        // A string needs no machine: coercing one is the identity, so this is
        // the same answer one round trip earlier.
        Value::Str(s) => Ok(Begin::Done(Value::Str(NixStr::with_context(
            dir_of(s.bytes()),
            s.context_set(),
        )))),
        _ => pure::coerce_in_body(arg(args, 0)?.clone(), CoerceFlags::NEITHER, finish_dir_of),
    }
}

fn finish_dir_of(_vm: &mut Vm, vals: &[Value], _args: &[Slot]) -> Result<Value> {
    let v = vals
        .first()
        .ok_or_else(|| VmError::eval("internal: dirOf lost its coerced argument"))?;
    let s = pure::want_nix_str(v)?;
    Ok(Value::Str(NixStr::with_context(
        dir_of(s.bytes()),
        s.context_set(),
    )))
}

/// Everything up to the last slash, cppnix's `prim_dirOf` arithmetic: no
/// slash is ".", a slash at position 0 is "/".
fn dir_of(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|&c| c == b'/') {
        Some(0) => b"/".to_vec(),
        Some(i) => path.get(..i).unwrap_or(b"/").to_vec(),
        None => b".".to_vec(),
    }
}

/// Every name the `builtins` attrset binds.
///
/// The set is built from this list and the compiler tests membership against
/// it when it resolves `builtins.<name>` without building the set, so the two
/// cannot disagree about which names exist. `builtins_set_keys_are_the_member_names`
/// holds the list to the set it describes.
pub fn set_member_names(
    settings: &crate::eval::Settings,
) -> impl Iterator<Item = &'static str> + use<'_> {
    TABLE
        .iter()
        .map(|builtin| builtin.name)
        .chain(crate::builtin_catalogue::EXTRA_MEMBERS.iter().copied())
        .filter(|name| crate::builtin_catalogue::enabled(settings, name))
        .chain(["true", "false", "null", "builtins"])
}

/// The table index `builtins.<name>` evaluates to, for the names where that
/// is all it evaluates to.
///
/// `None` covers three different situations and the caller wants the same
/// thing from all of them -- go through the set: the name is not a member at
/// all (`builtins.nope`, which must still throw `attribute missing`), it is a
/// member bound to something other than a plain primop (`derivation`,
/// `langVersion`, `nixVersion`, `true`), or it is a member this evaluator has
/// not implemented and whose slot must report that when forced.
pub fn set_member_index(settings: &crate::eval::Settings, name: &str) -> Option<u16> {
    member_index_among(name, set_member_names(settings))
}

/// `set_member_index` against an arbitrary member list.
///
/// Split out so the case this tree does not contain yet can still be tested: a
/// builtin in `TABLE` that cppnix does not register, and which therefore must
/// not be in `builtins` (or `builtins ? name` diverges) and must not fold
/// either. Today every implemented builtin is also a member, so calling this
/// with the real list never takes the `false` branch.
fn member_index_among(name: &str, members: impl Iterator<Item = &'static str>) -> Option<u16> {
    let mut members = members;
    let idx = global_index(name)?;
    members.any(|member| member == name).then_some(idx)
}

/// The slot `builtins.<name>` is bound to. One rule for every member, so the
/// value a name has does not depend on how the compiler selects it.
fn member_slot(vm: &mut Vm, name: &str) -> crate::vm::Result<Slot> {
    Ok(match name {
        // cppnix's `addConstant("derivation", ...)` puts one value in both the
        // global scope and this set, so `builtins.derivation` is the bare
        // global and not a second thing.
        "derivation" => vm.derivation_cell()?,
        // Same shape as `derivation` and for the same reason: the value is
        // not a primop, and unlike a constant nobody here can write it down
        // -- it is the embedder's `-I` flags. The bare global spelling is
        // `__nixPath`, which the compiler resolves to the same op this cell's
        // one-line module compiles to.
        "nixPath" => vm.nix_path_cell()?,
        "true" => Slot::value(Value::Bool(true)),
        "false" => Slot::value(Value::Bool(false)),
        "null" => Slot::value(Value::Null),
        "builtins" => Slot::value(builtins_set_marker()),
        _ => match global_index(name) {
            Some(i) => Slot::value(mk_value(i)),
            None => match constant(vm.settings(), name) {
                Some(v) => Slot::value(v),
                None => Slot::unimplemented(&format!("builtins.{name}")),
            },
        },
    })
}

/// Build the available builtin set. The VM retains it for this settings snapshot.
/// Fallible because the derivation wrapper is compiled lazily.
pub fn builtins_set(vm: &mut Vm) -> crate::vm::Result<Value> {
    let mut map = BTreeMap::new();
    let names: Vec<&'static str> = set_member_names(vm.settings()).collect();
    for name in names {
        let sym = vm.intern(name);
        let slot = member_slot(vm, name)?;
        map.insert(sym, slot);
    }
    Ok(Value::Attrs(Rc::new(Attrs::new(map))))
}

struct Constant {
    name: &'static str,
    value: fn(&crate::eval::Settings) -> Option<Value>,
}

/// The entries of `builtins` that are values rather than functions. Kept
/// apart from the primop table because they take no arguments and so have no
/// slot in it, and because one of them is not ours to state: `nixVersion` is
/// whatever binary we are linked into, which only the embedder knows.
///
/// A table rather than a `match` so that adding a constant means declaring
/// where its value comes from.
///
/// Each entry used to carry a `Volatility` saying whether the embedder could
/// move it, read by a guard that checked `Vm`'s staleness witness listed the
/// volatile ones. The witness is gone: replacing a `Vm`'s settings drops its
/// cached set outright, so every constant is covered without anyone
/// classifying it, and there is nothing left for the annotation to protect
/// (ENG-12939).
static CONSTANTS: &[Constant] = &[
    // primops.cc: `v.mkInt(6)` under "__langVersion". A property of the
    // language rather than of the build, so it belongs here; the comment
    // there says to bump it when a language feature lands, not when a
    // primop does.
    Constant {
        name: "langVersion",
        value: |_| Some(Value::Int(6)),
    },
    Constant {
        name: "nixVersion",
        value: |s| s.nix_version.as_deref().map(|v| Value::Str(v.into())),
    },
    // `addConstant("__currentSystem", mkString(settings.thisSystem))`. A
    // constant and not a question: cppnix reads the setting once at startup,
    // so an evaluation cannot see it change under it.
    Constant {
        name: "currentSystem",
        value: |s| s.current_system.as_deref().map(|v| Value::Str(v.into())),
    },
    // `addConstant("__storeDir", mkString(settings.nixStore))`. A constant for
    // the same reason `currentSystem` is, and from the same place
    // `derivationStrict` already takes it (`ixe_set_store_dir`): nixpkgs reads
    // it on the way to `hello.outPath`, and it was the last thing refusing
    // there. ENG-12607.
    Constant {
        name: "storeDir",
        value: |s| s.store_dir.as_deref().map(|v| Value::Str(v.into())),
    },
];

fn constant(settings: &crate::eval::Settings, name: &str) -> Option<Value> {
    (CONSTANTS.iter().find(|c| c.name == name)?.value)(settings)
}

/// builtins.builtins is self-referential in cppnix; a second level is
/// enough for the corpus and avoids a cyclic Rc.
fn builtins_set_marker() -> Value {
    Value::Attrs(Rc::new(Attrs::default()))
}

#[cfg(test)]
mod purity_tests {
    use super::TABLE;

    /// Impure cppnix primops that do NOT currently route through `Host`, and
    /// so would be invisible to a recorded read set.
    ///
    /// Enumerated from cppnix's own primop list (`builtin_catalogue::GLOBAL_NAMES`)
    /// rather than from imagination, because the mistake this guards against is
    /// implementing one of these without noticing that a memoised result keyed
    /// on a read set would then be wrong. Both spellings are listed: cppnix
    /// registers some of these with a `__` prefix and the bare name is a
    /// separate global.
    const UNROUTED_IMPURITIES: &[&str] = &[
        "currentTime",
        "__currentTime",
        "exec",
        "__exec",
        "fetchClosure",
        "__fetchClosure",
        "fetchMercurial",
        "derivation",
        // The bare spelling is implemented and routed (see below); this one
        // is not implemented at all, so it stays here.
        "__derivationStrict",
    ];

    /// Outputs rather than questions, and routed all the same: a builtin that
    /// wrote to stderr itself would make `host`'s "the VM performs no IO"
    /// claim false. They record nothing in a read set because they answer
    /// nothing -- see `RecordingHost`, which forwards both without noting
    /// them.
    const ROUTED_OUTPUTS: &[&str] = &["trace", "warn", "traceVerbose"];

    /// Every routed output is implemented; the list is not a wish.
    #[test]
    fn the_routed_outputs_are_implemented() {
        let implemented: Vec<&str> = TABLE.iter().map(|b| b.name).collect();
        let missing: Vec<&&str> = ROUTED_OUTPUTS
            .iter()
            .filter(|name| !implemented.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "listed as routed outputs but not implemented: {missing:?}"
        );
    }

    /// Impure builtins this evaluator implements, every one of which asks the
    /// scheduler through `Host` and therefore lands in a read set.
    const ROUTED_IMPURITIES: &[&str] = &[
        "import",
        "readFile",
        "readDir",
        "pathExists",
        "storePath",
        "__storePath",
        "readFileType",
        "getEnv",
        // Reaches the store, for the path case and for a set whose `outPath`
        // is a path: cppnix coerces its
        // argument with `copyToStore` on, so `unsafeDiscardStringContext ./f`
        // copies the file and then throws away the context that created.
        "unsafeDiscardStringContext",
        // The same coercion, for the same reason: both take their argument
        // through `coerceToString` with `copyToStore` left on before touching
        // the context (`context.cc:56`, `context.cc:94`).
        "unsafeDiscardOutputDependency",
        "addDrvOutputDependencies",
        // Reads the file it is asked to hash through `Host` exactly the way
        // `readFile` does (realise, then one contents question); only the
        // digest of the answer is pure.
        "hashFile",
        // Pure but for one edge: the deprecated `"base32"` format name emits
        // cppnix's deprecation warning, and warnings leave through the host
        // (`NeedPath::Warn`), which is also what lets `abort-on-warn`
        // treatment stay the embedder's.
        "convertHash",
        // `<x>` is this builtin, because cppnix's parser desugars a lookup
        // path into a call to it. The entries and the file both leave through
        // `Host::find_file`, so a read set records which search path answered
        // and with what (ENG-12443).
        "findFile",
        // The fixed-output fetchers. Every byte they can produce comes back
        // through `Host::fetch`, including the pinned case, where the store
        // path is computable without the network and whether the store *has*
        // it is not. A read set records the whole request and the path it
        // answered, so an unpinned fetch of a URL whose content moved does
        // not hit.
        "fetchurl",
        "fetchTarball",
        // The tree fetchers. The evaluator classifies the input attributes and
        // forwards them; every byte of the answer, including the revision a
        // `ref` resolved to today, comes back through `Host::fetch_tree` and
        // lands in the read set.
        "fetchTree",
        "fetchGit",
        // The third spelling of the same question, reachable only through
        // `ixe_internal_primop`. It leaves through `Host::fetch_tree` like the
        // other two, so a read set sees it.
        "fetchFinalTree",
        // Locking is the embedder's, and the whole of it -- the registry, the
        // input-graph walk, every fetch -- happens behind one
        // `Host::lock_flake`. The read set records the reference and a digest
        // of the lock it produced, so a flake whose lock moved does not hit
        // (ENG-12995).
        "getFlake",
        // The flake-ref grammar is the embedder's, in both directions: these
        // leave through `Host::parse_flake_ref` and `Host::flake_ref_to_string`
        // because a second parser or printer of that grammar here would
        // drift, and the flakes feature gate -- which decides between an
        // answer and an error -- lives behind the hook where cppnix checks
        // it. The read set records question and answer, since neither is in
        // the settings fingerprint.
        "parseFlakeRef",
        "flakeRefToString",
        // Asks the store to make every key it is handed present, through
        // `Host::ensure_path` (ENG-12479). Under `readOnlyMode` the embedder
        // answers without doing anything, which is cppnix's own branch
        // (`context.cc:275`) rather than a shortcut taken here.
        "appendContext",
        // Every path it reaches goes out through the same coercion, and it
        // performs no read or write of its own: under `readOnlyMode` cppnix
        // computes the `.drv` path rather than writing the file
        // (`primops.cc:1917`), and the modulo hashes of the inputs come from
        // the VM's own table rather than from the store. The store directory
        // it needs is configuration the embedder hands over, like the version
        // string, not a question asked at evaluation time. **When
        // `addTextToStore` lands (ENG-12491) that stops being true**, and the
        // write has to leave through `Host` before this entry stays honest.
        "derivationStrict",
        // Three that were in none of these lists until the module split
        // needed every implemented builtin classified. Each reaches the store
        // through `Host` and each was found by reading the body rather than
        // the section it sat under, which is how they stayed unlisted:
        //
        // `toFile` suspends with `NeedPath::StoreText` and the embedder
        // answers with the path it wrote.
        "toFile",
        // `toJSON` coerces on its way past, and a path inside the value is
        // copied into the store before it is rendered, so the body yields
        // `Yield::Need` (ENG-12670). The section header called it a data
        // format, which it also is, and that is what hid the store write.
        "toJSON",
        // `path` reads the source tree through `Host` and hands the accepted
        // set over for the store to hold (ENG-12678).
        "path",
        // The same machine as `path`, driven from cppnix's positional
        // spelling (`prim_filterSource`, `primops.cc:3004`, which is
        // `addPath` with `name = path.baseName()` and no `sha256`). Every
        // question it asks is `path`'s: `Kind` and `Entries` for the walk,
        // then one `StoreFiltered` for the copy.
        "filterSource",
    ];

    /// Impurities that are implemented and routed, but are not builtins, so
    /// the two lists above cannot hold them: `__nixPath` is a global bound to
    /// `Op::NixPathGlobal`, which suspends with `NeedPath::NixPath`.
    ///
    /// It has its own list because the guard below reads `TABLE`, and a name
    /// implemented outside the table is invisible to it -- which is exactly
    /// how `__nixPath` could have been implemented while still sitting in
    /// `UNROUTED_IMPURITIES` and nothing would have said so.
    const ROUTED_NON_TABLE_IMPURITIES: &[&str] = &["nixPath", "__nixPath"];

    /// Implemented, not a table entry, and **not routed**: process
    /// configuration the embedder hands over once, which a read set therefore
    /// cannot see.
    ///
    /// Listed rather than left out of all three lists, because "not in any
    /// list" is how an unrouted impurity hides. Each of these changes what an
    /// expression evaluates to, so a memoised result keyed only on the
    /// questions asked survives a change to one of them and is then wrong.
    /// That is ENG-12541, and it is one fix for all three rather than three
    /// fixes: the memo key has to carry the process globals.
    const UNROUTED_PROCESS_GLOBALS: &[&str] = &["currentSystem", "nixVersion", "storeDir"];

    /// Implemented, reaches nothing, and worth writing down because its
    /// sibling does reach something.
    ///
    /// `toJSON` is in `ROUTED_IMPURITIES` because it copies a path into the
    /// store on its way past (`copyToStore` on). `toXML` runs the same walk
    /// on the same driver and writes a path verbatim -- cppnix's
    /// `value-to-xml.cc:89` is `v.path().to_string()` with no copy -- so it
    /// asks the world nothing at all. Two builtins, one machine, different
    /// purity, which is exactly the pair a reader of the lists would
    /// otherwise assume was a filing mistake.
    const PURE_DESPITE_A_ROUTED_SIBLING: &[&str] = &["toXML"];

    /// The name is implemented, so the note above is describing something
    /// real, and it really is absent from both impurity lists.
    #[test]
    fn the_pure_siblings_are_implemented_and_in_no_impurity_list() {
        let implemented = implemented_spellings();
        for name in PURE_DESPITE_A_ROUTED_SIBLING {
            assert!(implemented.contains(name), "{name} is not implemented");
            assert!(
                !ROUTED_IMPURITIES.contains(name) && !UNROUTED_IMPURITIES.contains(name),
                "{name} is listed as an impurity but is documented as pure"
            );
        }
    }

    /// The three above are the whole set. A fourth arriving without a line in
    /// ENG-12541's fix is the failure this catches: it fails when a name is
    /// added to `constant()` without being classified here.
    #[test]
    fn every_process_global_constant_is_listed() {
        // `constant()` is private to the parent module and answers `None` for
        // anything it does not serve, which is what makes this enumerable.
        let unlisted: Vec<&&str> = crate::builtin_catalogue::EXTRA_MEMBERS
            .iter()
            .filter(|name| {
                super::constant(&crate::eval::Settings::default(), name).is_some()
                    || **name == "storeDir"
            })
            .filter(|name| !UNROUTED_PROCESS_GLOBALS.contains(name) && **name != "langVersion")
            .collect();
        assert!(
            unlisted.is_empty(),
            "these are constants the embedder supplies and are in no purity \
             list: {unlisted:?}. Classify them, or route them through Host."
        );
    }

    /// If one of those becomes a table entry, its routing has to be
    /// re-examined against the builtin's own implementation rather than
    /// against the op, so the lists have to be revisited. This fails at that
    /// moment instead of letting the name sit in a list that no longer
    /// describes it.
    #[test]
    fn the_non_table_impurities_are_still_not_in_the_table() {
        let implemented: Vec<&str> = TABLE.iter().map(|b| b.name).collect();
        let moved: Vec<&&str> = ROUTED_NON_TABLE_IMPURITIES
            .iter()
            .filter(|name| implemented.contains(name))
            .collect();
        assert!(
            moved.is_empty(),
            "these are implemented as ops and listed as such, but are now \
             table entries too: {moved:?}. Decide which implementation is \
             live and move them to ROUTED_IMPURITIES."
        );
    }

    /// Every spelling that reaches a `TABLE` entry, which is more than the
    /// table's own names.
    ///
    /// `compile.rs`'s bare-global resolution strips a leading `__` before it
    /// looks the name up (`is_global` then `global_index(impl_name)`), so
    /// implementing `toFile` implements the global `__toFile` in the same
    /// commit. A guard reading only `TABLE` cannot see that half, and did
    /// not: `__toFile` sat in `UNROUTED_IMPURITIES` while
    /// `builtins.typeOf __toFile` answered `"lambda"` through the routed
    /// implementation, and nothing failed. Both spellings are listed in the
    /// purity lists, so both have to be checked against them.
    ///
    /// The `__` spelling is included only when cppnix registers it, because
    /// that is the condition `is_global` tests: `derivationStrict` is
    /// registered bare, so `__derivationStrict` is not a global here and its
    /// row in `UNROUTED_IMPURITIES` is describing a name that really is
    /// unimplemented.
    fn implemented_spellings() -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for builtin in TABLE {
            out.push(builtin.name);
            if let Some(prefixed) = crate::builtin_catalogue::GLOBAL_NAMES
                .iter()
                .find(|cpp| cpp.strip_prefix("__") == Some(builtin.name))
            {
                out.push(prefixed);
            }
        }
        out
    }

    /// The helper above has to find the prefixed spellings, or the guard it
    /// feeds silently narrows back to `TABLE` and every `__` row in
    /// `UNROUTED_IMPURITIES` stops being checked. `__toFile` is the case that
    /// motivated it, so it is the case asserted here.
    #[test]
    fn the_prefixed_global_spellings_are_counted_as_implemented() {
        let implemented = implemented_spellings();
        assert!(
            implemented.contains(&"toFile") && implemented.contains(&"__toFile"),
            "both spellings of an implemented builtin must be present; got {} entries",
            implemented.len()
        );
        assert!(
            !implemented.contains(&"__derivationStrict"),
            "`derivationStrict` is registered bare by cppnix, so `__derivationStrict` \
             is not a global and must not be claimed as implemented"
        );
    }

    /// The refusal M-C needs: implementing an impurity the read set cannot see
    /// must break something loudly, at the moment it is implemented, rather
    /// than silently making every memoised result for a file using it wrong.
    ///
    /// If this fails, the fix is one of: route the new builtin's question
    /// through `Host` (add a `NeedPath` variant, as `getEnv` did) and move it
    /// to `ROUTED_IMPURITIES`; or teach the memoiser to refuse any evaluation
    /// that touched it.
    #[test]
    fn no_implemented_builtin_reaches_the_world_behind_the_host() {
        let implemented = implemented_spellings();
        let unrouted: Vec<&str> = implemented
            .iter()
            .copied()
            .filter(|name| UNROUTED_IMPURITIES.contains(name))
            .collect();
        assert!(
            unrouted.is_empty(),
            "these builtins are implemented but reach the world outside Host, \
             so a read set cannot see them and a memoised result keyed on one \
             would be wrong: {unrouted:?}. Route them through Host or make the \
             memoiser refuse files that use them."
        );
    }

    /// The other half: the impurities that ARE implemented are the ones the
    /// list says they are. Without this, moving a builtin out of
    /// ROUTED_IMPURITIES would silently shrink what the guard above covers.
    #[test]
    fn the_routed_impurity_list_matches_what_is_implemented() {
        let implemented = implemented_spellings();
        let missing: Vec<&&str> = ROUTED_IMPURITIES
            .iter()
            .filter(|name| !implemented.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "listed as routed impurities but not implemented: {missing:?}"
        );
    }

    /// Every TABLE entry paired with the module its body is written in, read
    /// out of the table's own source.
    ///
    /// A `fn` pointer carries no module, so there is nothing to ask at
    /// runtime. The path prefix on each entry is the only place the answer is
    /// written down, and it is the same text a reviewer reads, so the test and
    /// the reviewer cannot reach different conclusions about where a builtin
    /// lives.
    ///
    /// Malformed entries are skipped rather than guessed at, and the caller
    /// checks the count against `TABLE`. That is deliberate: a parser that
    /// silently matched nothing would report an empty disagreement list, which
    /// is exactly what a passing run looks like.
    fn table_entry_modules() -> Vec<(&'static str, &'static str)> {
        const SRC: &str = include_str!("builtins.rs");
        let Some((_, after)) = SRC.split_once("pub static TABLE") else {
            return Vec::new();
        };
        let Some((table, _)) = after.split_once("\n];") else {
            return Vec::new();
        };
        // The table refers to the two modules through short aliases, so
        // resolve them from the `use` lines rather than matching the alias
        // text: renaming an alias must not quietly turn this check off.
        let mut aliases: Vec<(&str, &str)> = Vec::new();
        for line in SRC.lines() {
            let Some(rest) = line.strip_prefix("use crate::") else {
                continue;
            };
            let rest = rest.trim_end_matches(';');
            if let Some((module, items)) = rest.split_once("::{") {
                for item in items.trim_end_matches('}').split(',') {
                    if let Some(("self", alias)) = item.trim().split_once(" as ") {
                        aliases.push((alias.trim(), module.trim()));
                    }
                }
            } else if let Some((module, alias)) = rest.split_once(" as ") {
                aliases.push((alias.trim(), module.trim()));
            }
        }

        let mut out = Vec::new();
        for entry in table.split("_bi!(").skip(1) {
            let Some(rest) = entry.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some((name, rest)) = rest.split_once('"') else {
                continue;
            };
            // `, <arity>, <path>, &[...]`: step over the arity to the path.
            let Some((_, rest)) = rest.split_once(',') else {
                continue;
            };
            let Some((_, rest)) = rest.split_once(',') else {
                continue;
            };
            let Some((path, _)) = rest.split_once(',') else {
                continue;
            };
            let path = path.trim().trim_start_matches("crate::");
            let module = match path.split_once("::") {
                // No qualifier: the body is in this file.
                None => "builtins",
                Some((prefix, _)) => aliases
                    .iter()
                    .find(|(alias, _)| *alias == prefix)
                    .map_or(prefix, |(_, module)| *module),
            };
            out.push((name, module));
        }
        out
    }

    /// The module split is the purity split, and this is what holds it.
    ///
    /// `primops_pure` and `primops_host` are named for a property rather than
    /// for the order they were written in, and a name like that is only worth
    /// having if something refuses to let it go stale. The names they replaced
    /// -- `builtins2` and `builtins3` -- carried a header each describing a
    /// charter neither module had kept to, and nothing anywhere noticed.
    ///
    /// So: adding a host-reaching builtin to `primops_pure` fails here, at the
    /// moment it is added, rather than later when someone trusts the module
    /// name and memoises a result against a read set that cannot see what it
    /// asked.
    #[test]
    fn each_primop_lives_on_its_own_side_of_the_boundary() {
        let entries = table_entry_modules();
        let table_names: Vec<&str> = TABLE.iter().map(|b| b.name).collect();
        let parsed_names: Vec<&str> = entries.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            parsed_names, table_names,
            "the entries read out of builtins.rs do not match TABLE, so this \
             test is checking something other than the table. Fix the parser \
             in table_entry_modules before trusting the assertion below."
        );

        let misplaced: Vec<String> = entries
            .iter()
            .filter_map(|(name, module)| {
                let reaches_host =
                    ROUTED_IMPURITIES.contains(name) || ROUTED_OUTPUTS.contains(name);
                // `drvstrict` is the host side too: derivationStrict's state
                // machine lives beside the derivation construction it drives.
                let on_host_side = *module == "primops_host" || *module == "drvstrict";
                if reaches_host == on_host_side {
                    return None;
                }
                Some(if reaches_host {
                    format!("{name} reaches the host but is implemented in {module}")
                } else {
                    format!("{name} is implemented in {module} but reaches nothing outside its arguments")
                })
            })
            .collect();
        assert!(
            misplaced.is_empty(),
            "these builtins are on the wrong side of the module boundary the \
             two module names claim: {misplaced:?}. Either move the body, or \
             -- if it really does leave through Host -- add it to \
             ROUTED_IMPURITIES and move it to primops_host."
        );
    }
}

#[cfg(test)]
mod set_tests {
    use super::{
        CONSTANTS, TABLE, builtins_set, global_index, member_index_among, set_member_index,
        set_member_names,
    };
    use crate::value2::{Slot, Value};
    use crate::vm::Vm;
    use std::collections::BTreeSet;

    /// One entry of the built set, by the name it is reachable under.
    struct Member {
        name: String,
        slot: Slot,
    }

    /// The names the set actually binds, paired with their slots. Returns
    /// empty rather than unwinding when the set does not build, because the
    /// workspace denies `panic` in tests too; the assertion is what reports it.
    fn built_set(vm: &mut Vm) -> Vec<Member> {
        let built = builtins_set(vm);
        assert!(
            matches!(built, Ok(Value::Attrs(_))),
            "builtins_set did not produce an attrset"
        );
        let Ok(Value::Attrs(map)) = built else {
            return Vec::new();
        };
        map.iter()
            .map(|(sym, slot)| Member {
                name: vm.sym_name(*sym).to_owned(),
                slot: slot.clone(),
            })
            .collect()
    }

    /// `set_member_names` is the list the compiler consults to decide whether
    /// `builtins.<name>` exists without building the set. If the set were to
    /// gain or lose a name without the list moving, the compiler would resolve
    /// a name that is not there, or refuse one that is.
    #[test]
    fn builtins_set_keys_are_the_member_names() {
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let built: BTreeSet<String> = built_set(&mut vm).into_iter().map(|m| m.name).collect();
        let listed: BTreeSet<String> = set_member_names(&crate::eval::Settings::default())
            .map(str::to_owned)
            .collect();
        assert_eq!(built, listed);
    }

    /// The compile-time answer and the run-time answer for the same name, held
    /// against each other for every member. `set_member_index` saying `Some(i)`
    /// is a promise that the set binds that name to exactly `mk_value(i)`, and
    /// the fold in `compile_select` is only sound while the promise holds.
    #[test]
    fn compile_time_resolution_matches_the_slot_the_set_holds() {
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        for member in built_set(&mut vm) {
            let folded = set_member_index(&crate::eval::Settings::default(), &member.name);
            let held = match member.slot.peek() {
                Some(Value::Builtin(b)) if b.args.is_empty() => Some(b.idx),
                _ => None,
            };
            assert_eq!(folded, held, "builtins.{}", member.name);
        }
    }

    /// Replacing a `Vm`'s settings drops its cached `builtins` set, for every
    /// constant that reads them and not just the ones somebody listed.
    ///
    /// This used to compare a hand-written `EmbedderInputs` witness naming
    /// three constants. Two things were wrong with that and only one was ever
    /// caught: a fourth embedder-supplied constant would have needed the
    /// witness widened by hand, and `pure-eval` -- which decides which *keys*
    /// the set has, not just what one of them holds -- was never in it at all.
    /// Invalidating on the settings value covers both, so what is checked here
    /// is the invalidation itself rather than the contents of a list.
    /// What `builtins.<name>` is bound to in the VM's cached set, or `None`
    /// while the slot is still the unimplemented one.
    fn reported(vm: &mut crate::vm::Vm, name: &str) -> Option<String> {
        let sym = vm.intern(name);
        let Ok(Value::Attrs(map)) = vm.builtins_value() else {
            return None;
        };
        match map.get(&sym)?.peek() {
            Some(Value::Str(s)) => Some(s.expect_text()),
            _ => None,
        }
    }

    /// Whether the VM's cached set has `name` as a key at all.
    fn has_member(vm: &mut crate::vm::Vm, name: &str) -> bool {
        let sym = vm.intern(name);
        matches!(vm.builtins_value(), Ok(Value::Attrs(map)) if map.contains_key(&sym))
    }

    #[test]
    fn replacing_the_settings_rebuilds_the_cached_builtins_set() {
        let _moving = crate::eval::globals_moving();

        // Warm the cache under settings with no version, then move the
        // process on and re-take. `nixVersion` is an `Embedder` constant and
        // `currentSystem`'s presence is what `pure-eval` decides, so between
        // them they cover both ways the set can go stale.
        let mut vm = crate::vm::Vm::with_settings(crate::eval::Settings::default());
        assert_eq!(reported(&mut vm, "nixVersion"), None);

        let before = crate::eval::pure_eval();
        crate::eval::set_pure_eval(true);
        vm.reload_settings_from_process();
        let under_pure = has_member(&mut vm, "currentSystem");
        crate::eval::set_pure_eval(before);
        vm.reload_settings_from_process();
        let under_impure = has_member(&mut vm, "currentSystem");

        assert!(
            !under_pure,
            "the cached set still named currentSystem after pure-eval came on, so \
             the settings moved and the set did not"
        );
        assert!(
            under_impure,
            "currentSystem did not come back, so the set is not being rebuilt at all \
             and the assertion above passes for the wrong reason"
        );
        // Every constant is a member, so none of them is unreachable.
        for c in CONSTANTS {
            assert!(
                set_member_names(&crate::eval::Settings::default()).any(|name| name == c.name),
                "{} is a constant but not a member of builtins",
                c.name
            );
        }
    }

    /// A builtin this evaluator implements but cppnix does not register would
    /// not be in `builtins`, so `builtins.<name>` has to keep throwing
    /// `attribute missing` rather than fold to the implementation.
    ///
    /// No such builtin exists in this tree -- every `TABLE` name is also a
    /// member -- so the membership test in `set_member_index` currently never
    /// changes an answer and cannot be broken by mutating it. The list is
    /// therefore doctored here instead, which exercises the branch that will
    /// start doing work the day the first ix-only builtin lands.
    #[test]
    fn an_implemented_builtin_outside_the_set_does_not_fold() {
        let present = member_index_among(
            "stringLength",
            set_member_names(&crate::eval::Settings::default()),
        );
        assert_eq!(present, global_index("stringLength"));
        let absent = member_index_among(
            "stringLength",
            set_member_names(&crate::eval::Settings::default())
                .filter(|name| *name != "stringLength"),
        );
        assert_eq!(absent, None);
    }

    /// The three ways a name can fail to fold, named, so that a change which
    /// starts folding one of them fails here rather than in a corpus diff.
    #[test]
    fn only_plain_primops_fold() {
        assert_eq!(
            set_member_index(&crate::eval::Settings::default(), "stringLength"),
            global_index("stringLength")
        );
        // A member, but the compiled derivation wrapper rather than a primop.
        assert_eq!(
            set_member_index(&crate::eval::Settings::default(), "derivation"),
            None
        );
        // Members bound to constants, one of which the embedder supplies.
        assert_eq!(
            set_member_index(&crate::eval::Settings::default(), "langVersion"),
            None
        );
        assert_eq!(
            set_member_index(&crate::eval::Settings::default(), "nixVersion"),
            None
        );
        assert_eq!(
            set_member_index(&crate::eval::Settings::default(), "true"),
            None
        );
        // Not a member at all: has to keep throwing `attribute missing`.
        assert_eq!(
            set_member_index(&crate::eval::Settings::default(), "nope"),
            None
        );
        // Unsupported names cannot fold to a builtin implementation.
        assert!(
            !TABLE.iter().any(|b| b.name == "fetchMercurial"),
            "fetchMercurial became implemented; pick another unimplemented member"
        );
        assert_eq!(
            set_member_index(&crate::eval::Settings::default(), "fetchMercurial"),
            None
        );
    }
}

#[cfg(test)]
mod rooted_path_tests {
    use super::{bi_base_name_of, bi_dir_of};
    use crate::primops_pure::Begin;
    use crate::value2::{NixStr, PathValue, Root, Slot, Value};
    use crate::vm::Vm;
    use std::rc::Rc;

    #[test]
    fn dir_of_keeps_a_paths_root_and_base_name_remains_text() {
        let mount = "/nix/store/00000000000000000000000000000000-source";
        let path = Value::Path(Rc::new(PathValue::new(
            Root::mounted(mount),
            format!("{mount}/dir/file"),
        )));
        let args = [Slot::value(path)];
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let dir = bi_dir_of(&mut vm, &args);
        assert!(matches!(
            dir,
            Ok(Begin::Done(Value::Path(path)))
                if path.root == Root::mounted(mount)
                    && path.as_ref().as_ref() == format!("{mount}/dir")
        ));
        // `baseNameOf` receives what the driver coerced the path to -- its
        // visible spelling, root and all -- and answers text.
        let coerced = [Slot::value(Value::Str(NixStr::from(
            format!("{mount}/dir/file").as_str(),
        )))];
        let base = bi_base_name_of(&mut vm, &coerced);
        assert!(matches!(base, Ok(Value::Str(text)) if text.bytes() == b"file"));

        let root_args = [Slot::value(Value::Path(Rc::new(PathValue::new(
            Root::mounted(mount),
            mount,
        ))))];
        let root_dir = bi_dir_of(&mut vm, &root_args);
        assert!(matches!(
            root_dir,
            Ok(Begin::Done(Value::Path(path)))
                if path.root == Root::mounted(mount) && path.path.as_ref() == mount
        ));
    }
}
