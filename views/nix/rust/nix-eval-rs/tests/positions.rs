//! Source positions survive from the CST to the two places they are observed:
//! `builtins.unsafeGetAttrPos`, and the position on a failing evaluation.
//!
//! ENG-12137. Every expectation here was read off cppnix
//! (`nix-instantiate --eval --strict`, system nix 2.32) on a file under
//! `/private/tmp` -- not `/tmp`, which on macOS is a symlink cppnix does not
//! resolve, so it silently loses the source and reports column `offset + 1`
//! on line 1 for everything. See `maintainers/ix/positions.md`.

#![expect(
    clippy::expect_used,
    reason = "an integration test crate: its helpers abort loudly like the tests they serve; clippy.toml's allow-expect-in-tests covers `#[test]` functions and `#[cfg(test)]` items, and a helper in `tests/` is neither"
)]

use nix_eval_rs::compile::Origin;
use nix_eval_rs::eval::{Settings, eval_str, eval_str_at, eval_str_with};
use std::path::PathBuf;

/// Evaluate `src` as if it were the file at `path`.
fn at_file(src: &str, path: &str) -> String {
    match eval_str_at(src, "/private/tmp", Origin::File(path)) {
        Ok(text) => text,
        Err(error) => format!("{error:?}"),
    }
}

const F: &str = "/private/tmp/pos/f.nix";

/// `builtins.unsafeGetAttrPos "<name>" (<set>)` on one line of a file. The
/// prefix is 31 bytes for a one-character name, so a column here is countable
/// by hand off the fixture and is compared against the oracle rows in
/// `maintainers/ix/positions.md`.
fn pos_of(name: &str, set: &str) -> String {
    at_file(&format!("builtins.unsafeGetAttrPos \"{name}\" ({set})"), F)
}

/// `{ column = C; file = F; line = 1; }`, the answer's printed form.
fn col(c: u32) -> String {
    format!(r#"{{ column = {c}; file = "{F}"; line = 1; }}"#)
}

fn position(path: &str, line: u32, column: u32) -> String {
    format!(r#"{{ column = {column}; file = "{path}"; line = {line}; }}"#)
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/positions")
        .join(name)
}

fn eval_fixture(name: &str) -> String {
    let path = fixture(name);
    let source = std::fs::read_to_string(&path).expect("position fixture is readable");
    let path = path.to_str().expect("fixture path is UTF-8");
    let base = std::path::Path::new(path)
        .parent()
        .and_then(std::path::Path::to_str)
        .expect("fixture directory is UTF-8");
    match eval_str_at(&source, base, Origin::File(path)) {
        Ok(text) => text,
        Err(error) => format!("{error:?}"),
    }
}

// -- the base case ----------------------------------------------------------

/// `builtins.unsafeGetAttrPos "b" ({ a = 1; b = 2; })` in a one-line file:
/// `b`'s name token is at byte offset 40, so column 41.
#[test]
fn a_literal_attribute_answers_where_its_name_was_written() {
    let src = "builtins.unsafeGetAttrPos \"b\" ({ a = 1; b = 2; })";
    assert_eq!(src.find("b = 2"), Some(40), "the fixture moved");
    assert_eq!(
        at_file(src, F),
        format!(r#"{{ column = 41; file = "{F}"; line = 1; }}"#)
    );
}

/// The position is the attribute's own, not the set's: two attributes of one
/// set answer two different columns.
#[test]
fn two_attributes_of_one_set_answer_different_columns() {
    assert_eq!(pos_of("a", "{ a = 1; b = 2; }"), col(34));
    assert_eq!(pos_of("b", "{ a = 1; b = 2; }"), col(41));
}

/// A line and a column, on a file with more than one line, so a `line_starts`
/// off-by-one cannot hide behind everything being line 1.
#[test]
fn a_multi_line_file_answers_the_right_line() {
    assert_eq!(
        at_file(
            "builtins.unsafeGetAttrPos \"b\" {\n  a = 1;\n  b = 2;\n}",
            F
        ),
        format!(r#"{{ column = 3; file = "{F}"; line = 3; }}"#)
    );
}

/// A `\r\n` file. cppnix's `Pos::LinesIterator` treats the pair as one line
/// ending rather than counting the carriage return into the next column.
#[test]
fn crlf_ends_a_line() {
    assert_eq!(
        at_file(
            "builtins.unsafeGetAttrPos \"b\" {\r\n  a = 1;\r\n  b = 2;\r\n}",
            F
        ),
        format!(r#"{{ column = 3; file = "{F}"; line = 3; }}"#)
    );
}

/// Bare `\r` is independently a line ending. A scanner that recognizes only
/// `\n` passes the CRLF fixture above and reports this binding on line 1.
#[test]
fn a_bare_carriage_return_ends_a_line() {
    assert_eq!(
        at_file(
            "builtins.unsafeGetAttrPos \"b\" {\r  a = 1;\r  b = 2;\r}",
            F
        ),
        position(F, 3, 3)
    );
}

/// Columns are byte offsets in cppnix, not character offsets: the `é` before
/// `b` is two bytes and moves the column by two.
#[test]
fn columns_count_bytes() {
    assert_eq!(pos_of("b", "{ b = 1; }"), col(34));
    // `é` is two bytes and one character, so `b` moves ten characters and
    // eleven bytes. A char-counting column would say 43; cppnix says 44.
    assert_eq!(
        at_file("builtins.unsafeGetAttrPos \"b\" ({ \"é\" = 0; b = 1; })", F),
        col(44)
    );
}

// -- the cases that must answer null ----------------------------------------

/// cppnix builds the record only for a `SourcePath` origin (`eval.cc`'s
/// `mkPos`), so text with no file behind it answers `null`. Confirmed:
/// `nix-instantiate --eval -E 'builtins.unsafeGetAttrPos "a" { a = 1; }'`
/// prints `null`.
#[test]
fn a_string_origin_answers_null() {
    assert_eq!(
        eval_str(r#"builtins.unsafeGetAttrPos "a" { a = 1; }"#).unwrap_or_default(),
        "null"
    );
}

#[test]
fn an_absent_attribute_answers_null() {
    assert_eq!(pos_of("zz", "{ a = 1; }"), "null");
}

/// A dynamic name is not in the source as text, but the `${` token that
/// produces it is, and that is what cppnix records (`ExprAttrs::eval` inserts
/// each `dynamicAttrs` entry with its own `i.pos`).
#[test]
fn a_runtime_computed_attribute_answers_its_interpolation() {
    let src = r#"let
  mk = first: second: {
    ${first} = 1;
    ${second} = 2;
  };
  first = mk "a" "b";
  second = mk "b" "a";
in builtins.seq first (builtins.seq second [
  (builtins.unsafeGetAttrPos "a" first)
  (builtins.unsafeGetAttrPos "a" second)
])"#;
    assert_eq!(
        at_file(src, F),
        format!(
            r#"[ {{ column = 5; file = "{F}"; line = 3; }} {{ column = 5; file = "{F}"; line = 4; }} ]"#
        )
    );
}

/// The original mismatch: adding one dynamic binding must not make the
/// literal's static position disappear when the set switches to a runtime
/// origin.
#[test]
fn static_and_runtime_computed_names_keep_their_own_positions() {
    let src = r#"let
  key = "dyn";
  set = {
    static = 1;
    ${key} = 2;
  };
in [
  (builtins.unsafeGetAttrPos "static" set)
  (builtins.unsafeGetAttrPos "dyn" set)
]"#;
    assert_eq!(
        at_file(src, F),
        format!(
            r#"[ {{ column = 5; file = "{F}"; line = 4; }} {{ column = 5; file = "{F}"; line = 5; }} ]"#
        )
    );
}

/// Every component of an attrpath has the path's starting position. Runtime
/// name capture must retain that parser position, not replace it with the
/// later `${` component's offset.
#[test]
fn a_runtime_computed_attrpath_component_answers_the_paths_start() {
    let set = r#"let k = "b"; in { a.${k} = 1; }.a"#;
    let src = format!("builtins.unsafeGetAttrPos \"b\" ({set})");
    let expected = src.find("a.${k}").expect("the fixture moved") + 1;
    assert_eq!(at_file(&src, F), col(expected as u32));
}

/// cppnix's `prim_listToAttrs` copies the winning input pair's `value`
/// attribute position to the result attribute. Column 69 is the `value` name,
/// not the pair's `name` or the result's runtime name.
#[test]
fn list_to_attrs_keeps_the_winning_pairs_value_position() {
    assert_eq!(
        pos_of(
            "a",
            r#"builtins.listToAttrs [ { name = "a"; value = 1; } ]"#
        ),
        col(69)
    );
}

/// Duplicate names do not replace either the first value or its provenance.
#[test]
fn list_to_attrs_duplicate_keeps_the_first_pairs_value_position() {
    let set = r#"builtins.listToAttrs [ { name = "a"; value = 1; } { name = "a"; value = 2; } ]"#;
    let src = format!("builtins.unsafeGetAttrPos \"a\" ({set})");
    let expected = src.find("value = 1").expect("the fixture moved") + 1;
    assert_eq!(at_file(&src, F), col(expected as u32));
}

// -- derived sets: the origin follows the values -----------------------------

/// `//` takes the right operand's values where they collide, so its projection
/// selects the right origin for that name. Answering with the left's would
/// report a position for an attribute whose value came from somewhere else.
#[test]
fn update_takes_the_right_operands_origin() {
    // Column 48 is the right `a`; the left one is at 34, so this fails
    // loudly if the left operand ever wins.
    assert_eq!(pos_of("a", "{ a = 1; } // { a = 2; }"), col(48));
}

/// The other half of the same rule: an attribute the right operand does not
/// have keeps the left's value and position (column 34).
#[test]
fn update_keeps_the_left_only_attributes_position() {
    assert_eq!(pos_of("a", "{ a = 1; } // { b = 2; }"), col(34));
    assert_eq!(pos_of("b", "{ a = 1; } // { b = 2; }"), col(48));
}

/// A source-less right value still wins. Falling back to the positioned left
/// operand here would return a real line for the wrong value; `null` is the
/// only safe answer and matches cppnix.
#[test]
fn update_does_not_expose_a_shadowed_left_position() {
    assert_eq!(
        pos_of("a", r#"{ a = 1; } // builtins.fromJSON ''{"a":2}''"#),
        "null"
    );
}

/// `rec { __overrides = ...; }` is the same rule reached by a different road,
/// and it is worth pinning separately because nothing in the source says
/// `//`: the compiler closes the statics into one set and appends the
/// override set with an `Update` (`compile::emit_rec_set_build`), so the
/// result projects positions from both sets.
///
/// An overridden attribute therefore answers column 54 inside `{ a = 20; }`,
/// which is where cppnix reads it from too. A static the override does not
/// name keeps the rec literal's column 72. Both columns were measured against
/// `nix-instantiate --eval --strict` on the wrapped fixture, 2026-08-06.
#[test]
fn a_rec_override_keeps_both_arms_positions() {
    let src = "rec { __overrides = { a = 20; }; a = 1; b = 2; }";
    assert_eq!(pos_of("a", src), col(54));
    assert_eq!(pos_of("b", src), col(72));
}

/// Each mixed `//` result carries one flat projection. An older accumulator's
/// position therefore survives more than one fold step instead of depending
/// on a chain through every intermediate result.
#[test]
fn an_update_fold_keeps_every_surviving_position() {
    let src = r#"let
  merged = builtins.foldl' (acc: next: acc // next) {} [
    { first = 1; }
    { middle = 2; }
    { last = 3; }
  ];
in [
  (builtins.unsafeGetAttrPos "first" merged)
  (builtins.unsafeGetAttrPos "middle" merged)
  (builtins.unsafeGetAttrPos "last" merged)
]"#;
    let position = |name: &str| {
        let offset = src.find(&format!("{name} =")).expect("the fixture moved");
        let prefix = &src[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = offset - prefix.rfind('\n').map_or(0, |newline| newline + 1) + 1;
        format!(r#"{{ column = {column}; file = "{F}"; line = {line}; }}"#)
    };
    assert_eq!(
        at_file(src, F),
        format!(
            "[ {} {} {} ]",
            position("first"),
            position("middle"),
            position("last")
        )
    );
}

/// Dynamic bindings are applied after `__overrides` by `MkAttrsOnto`. The
/// dynamic binding keeps its own position, and names it did not add fall back
/// to the post-`Update` projection. That fallback must preserve the override
/// position for `a` and the rec literal's position for `b`.
#[test]
fn a_rec_override_and_runtime_name_keep_all_positions() {
    let src = r#"let key = "dyn"; set = rec { __overrides = { a = 20; }; a = 1; b = 3; ${key} = 2; }; in [ (builtins.unsafeGetAttrPos "a" set) (builtins.unsafeGetAttrPos "b" set) (builtins.unsafeGetAttrPos "dyn" set) ]"#;
    let override_column = src.find("a = 20").expect("the fixture moved") + 1;
    let static_column = src.find("b = 3").expect("the fixture moved") + 1;
    let dynamic_column = src.find("${key}").expect("the fixture moved") + 1;
    assert_eq!(
        at_file(src, F),
        format!(
            r#"[ {{ column = {override_column}; file = "{F}"; line = 1; }} {{ column = {static_column}; file = "{F}"; line = 1; }} {{ column = {dynamic_column}; file = "{F}"; line = 1; }} ]"#
        )
    );
}

/// `removeAttrs` keeps the values it did not remove, so it keeps its own
/// origin and the survivors still answer.
#[test]
fn remove_attrs_keeps_the_surviving_positions() {
    assert_eq!(
        pos_of("a", r#"builtins.removeAttrs { a = 1; b = 2; } [ "b" ]"#),
        col(55)
    );
    assert_eq!(
        pos_of("b", r#"builtins.removeAttrs { a = 1; b = 2; } [ "b" ]"#),
        "null",
        "a removed attribute is absent, so it answers null even though its \
         name is still in the site table"
    );
}

/// `intersectAttrs` takes the second set's values, so it takes its origin.
#[test]
fn intersect_attrs_takes_the_second_sets_origin() {
    // 69 is the second set's `a`; the first set's is at 57.
    assert_eq!(
        pos_of("a", "builtins.intersectAttrs { a = 0; } { a = 1; b = 2; }"),
        col(69)
    );
}

/// `builtins.functionArgs` hands back a set whose attributes cppnix gives the
/// formals' own positions (`primops.cc`, `prim_functionArgs`).
#[test]
fn function_args_answers_the_formals_positions() {
    assert_eq!(
        at_file(
            "builtins.unsafeGetAttrPos \"b\" (builtins.functionArgs ({ a, b }: 0))",
            F,
        ),
        col(60)
    );
}

/// Every component of one binding takes the position of the whole attrpath,
/// which is what cppnix's parser hands `addAttr`. `{ a.b = 1; }` answers the
/// `a` for `b` as well as for `a`, and `{ a.b.c = 1; }` answers it for `c`.
#[test]
fn a_nested_attrpath_answers_where_the_path_starts() {
    assert_eq!(pos_of("a", "{ a.b = 1; }"), col(34));
    assert_eq!(pos_of("b", "{ a.b = 1; }.a"), col(34));
    assert_eq!(pos_of("c", "{ a.b.c = 1; }.a.b"), col(34));
}

/// An inherited attribute takes the position of the NAME in the `inherit`
/// list, not of whatever it was inherited from.
#[test]
fn an_inherited_attribute_answers_its_name_in_the_inherit_list() {
    assert_eq!(pos_of("a", "let a = 1; in { inherit a; }"), col(56));
}

#[test]
fn a_rec_set_answers_like_a_plain_one() {
    assert_eq!(pos_of("a", "rec { a = 1; }"), col(38));
}

/// A projected `//` entry keeps the module that supplied each winning name.
/// The foreign offsets are small enough to look plausible in the root file,
/// so the exact file assertions are what catch interpreting them in the slab
/// owner's module.
#[test]
fn update_positions_can_cross_an_import_boundary() {
    let root = fixture("update-root.nix");
    let foreign = fixture("update-foreign.nix");
    let root = root.to_str().expect("fixture path is UTF-8");
    let foreign = foreign.to_str().expect("fixture path is UTF-8");
    assert_eq!(
        eval_fixture("update-root.nix"),
        format!(
            "[ {} {} {} ]",
            position(root, 4, 5),
            position(foreign, 2, 3),
            position(foreign, 3, 3)
        )
    );
}

/// `listToAttrs` delegates each result name to its winning pair's `value`
/// attribute, including when the pair belongs to an imported module.
#[test]
fn list_to_attrs_positions_can_cross_an_import_boundary() {
    let root = fixture("list-root.nix");
    let foreign = fixture("list-foreign.nix");
    let root = root.to_str().expect("fixture path is UTF-8");
    let foreign = foreign.to_str().expect("fixture path is UTF-8");
    assert_eq!(
        eval_fixture("list-root.nix"),
        format!("[ {} {} ]", position(root, 4, 23), position(foreign, 3, 3))
    );
}

/// `inherit (e)` records the inherited name in the inherit list. It does not
/// reuse the position of `e.a`.
#[test]
fn inherit_from_an_expression_uses_the_inherited_names_position() {
    let source = r#"let e = { a = 1; };
in builtins.unsafeGetAttrPos "a" {
  inherit (e) a;
}"#;
    assert_eq!(at_file(source, F), position(F, 3, 15));
}

/// These builtins construct new attrsets without source `Attr`s. cppnix gives
/// their result names `noPos`, even when an input set had a source position.
#[test]
fn synthesized_builtin_sets_have_null_positions() {
    let source = r#"let
  mapped = builtins.mapAttrs (name: value: value) { a = 1; };
  zipped = builtins.zipAttrsWith (name: values: builtins.head values) [ { a = 1; } ];
  json = builtins.fromJSON ''{"a":1}'';
  toml = builtins.fromTOML ''a = 1'';
in [
  (builtins.unsafeGetAttrPos "a" mapped)
  (builtins.unsafeGetAttrPos "a" zipped)
  (builtins.unsafeGetAttrPos "a" json)
  (builtins.unsafeGetAttrPos "a" toml)
]"#;
    assert_eq!(at_file(source, F), "[ null null null null ]");
}

/// `catAttrs` copies the selected value slots. If those values are sets, their
/// own per-attribute origins survive the list construction.
#[test]
fn cat_attrs_preserves_origins_of_selected_set_values() {
    let source = r#"let
  values = builtins.catAttrs "payload" [
    { payload = { first = 1; }; }
    { payload = { second = 2; }; }
  ];
in [
  (builtins.unsafeGetAttrPos "first" (builtins.elemAt values 0))
  (builtins.unsafeGetAttrPos "second" (builtins.elemAt values 1))
]"#;
    assert_eq!(
        at_file(source, F),
        format!("[ {} {} ]", position(F, 3, 19), position(F, 4, 19))
    );
}

/// `getAttr` forces and returns the stored slot; it does not rebuild a set and
/// discard the selected value's origin.
#[test]
fn get_attr_preserves_the_selected_set_values_origin() {
    let source = r#"let
  source = {
    payload = {
      kept = 1;
    };
  };
in builtins.unsafeGetAttrPos "kept" (builtins.getAttr "payload" source)"#;
    assert_eq!(at_file(source, F), position(F, 4, 7));
}

/// `attrValues` orders by attribute text, not interner id or source order, and
/// each copied value retains its own nested-set origin.
#[test]
fn attr_values_sorts_by_name_and_preserves_selected_origins() {
    let source = r#"let
  values = builtins.attrValues {
    z = { fromZ = 1; };
    a = { fromA = 2; };
  };
in [
  (builtins.unsafeGetAttrPos "fromA" (builtins.elemAt values 0))
  (builtins.unsafeGetAttrPos "fromZ" (builtins.elemAt values 1))
]"#;
    assert_eq!(
        at_file(source, F),
        format!("[ {} {} ]", position(F, 4, 11), position(F, 3, 11))
    );
}

/// `head` is the generic slot-copying case: returning a set through a list
/// selector must not strip the set's own origin.
#[test]
fn a_slot_copying_builtin_preserves_a_set_origin() {
    let source = r#"builtins.unsafeGetAttrPos "copied" (builtins.head [
  { copied = 1; }
])"#;
    assert_eq!(at_file(source, F), position(F, 2, 5));
}

/// `filterAttrs` is a Nix-level composition, not an evaluator primitive. Pin
/// the `removeAttrs` composition so surviving names keep the input set's
/// positions and removed names remain absent.
#[test]
fn filter_attrs_composition_keeps_only_surviving_positions() {
    let source = r#"let
  filterAttrs = pred: set:
    builtins.removeAttrs set
      (builtins.filter (name: ! pred name (builtins.getAttr name set))
        (builtins.attrNames set));
  source = {
    keep = 1;
    drop = 2;
  };
  filtered = filterAttrs (name: value: name == "keep") source;
in [
  (builtins.unsafeGetAttrPos "keep" filtered)
  (builtins.unsafeGetAttrPos "drop" filtered)
]"#;
    assert_eq!(
        at_file(source, F),
        format!("[ {} null ]", position(F, 7, 5))
    );
}

/// The `derivation` wrapper copies the caller's input attributes through `//`
/// and builds its bookkeeping in cppnix's `/derivation-internal.nix` source.
/// The result is therefore a deliberate mix of caller and wrapper positions;
/// only an attribute absent from the result answers `null`.
#[test]
fn the_derivation_wrapper_distinguishes_copied_and_synthesized_fields() {
    let source = r#"let
  drv = derivation {
    name = "position-oracle";
    system = "x86_64-linux";
    builder = "/bin/sh";
  };
in [
  (builtins.unsafeGetAttrPos "name" drv)
  (builtins.unsafeGetAttrPos "system" drv)
  (builtins.unsafeGetAttrPos "builder" drv)
  (builtins.unsafeGetAttrPos "out" drv)
  (builtins.unsafeGetAttrPos "all" drv)
  (builtins.unsafeGetAttrPos "drvAttrs" drv)
  (builtins.unsafeGetAttrPos "outPath" drv)
  (builtins.unsafeGetAttrPos "drvPath" drv)
  (builtins.unsafeGetAttrPos "type" drv)
  (builtins.unsafeGetAttrPos "outputName" drv)
  (builtins.unsafeGetAttrPos "outputs" drv)
]"#;
    let settings = Settings {
        store_dir: Some("/nix/store".to_owned()),
        ..Settings::default()
    };
    let answer = match eval_str_with(source, "/private/tmp", Origin::File(F), &settings) {
        Ok(text) => text,
        Err(error) => format!("{error:?}"),
    };
    assert_eq!(
        answer,
        format!(
            "[ {} {} {} {} {} {} {} {} {} {} null ]",
            position(F, 3, 5),
            position(F, 4, 5),
            position(F, 5, 5),
            position("/derivation-internal.nix", 48, 5),
            position("/derivation-internal.nix", 42, 7),
            position("/derivation-internal.nix", 43, 15),
            position("/derivation-internal.nix", 49, 7),
            position("/derivation-internal.nix", 50, 7),
            position("/derivation-internal.nix", 51, 7),
            position("/derivation-internal.nix", 52, 15)
        )
    );
}

// -- positions on errors -----------------------------------------------------

/// A failing evaluation carries the position of the op that failed.
#[test]
fn an_error_carries_a_position() {
    let pos = eval_str_at("let x = 1; in\n  x.y", "/private/tmp", Origin::File(F))
        .err()
        .and_then(|e| e.pos().cloned());
    assert!(pos.is_some(), "no position on the error");
    let Some(pos) = pos else { return };
    assert_eq!(pos.line, 2, "{pos:?}");
    assert_eq!(pos.file.as_deref(), Some(F), "{pos:?}");
}

/// A `throw` carries the position of the `throw` token.
///
/// This is the case that made the attribution apply to every frame kind and
/// not only to `Frame::Unit`: `throw` raises from inside a builtin task, so
/// attributing only unit frames left it with no position at the top level and
/// with the enclosing unit's first op inside one.
#[test]
fn a_throw_carries_a_position() {
    let pos = eval_str_at(
        "let\n  boom = throw \"no\";\nin boom",
        "/private/tmp",
        Origin::File(F),
    )
    .err()
    .and_then(|e| e.pos().cloned());
    assert!(pos.is_some(), "a throw carried no position");
    let Some(pos) = pos else { return };
    assert_eq!((pos.line, pos.column), (2, 10), "{pos:?}");
}

/// Positions on the error shapes a user meets most, each read off cppnix on
/// the same file. The tuple is `(line, column)`.
#[test]
fn the_common_error_shapes_carry_cppnixs_position() {
    for (src, want) in [
        ("\n\n  throw \"no\"", (3, 3)),
        // The argument is an expression, so a unit runs between the call op
        // and the builtin task; the position must still be the `throw`.
        ("\n\n  throw (\"a\" + \"b\")", (3, 3)),
        ("\n\n  builtins.head [ (throw \"no\") ]", (3, 20)),
        ("\n\nlet f = _:\n  throw \"no\";\nin f 1", (4, 3)),
        ("\n\n  abort \"no\"", (3, 3)),
        ("\n\n  1 / 0", (3, 5)),
        ("\n\n  ({ a = 1; }).zz", (3, 3)),
        ("let\n  a = 1;\nin a + \"s\"", (3, 8)),
        ("let\n  a = 1;\nin assert a == 2; a", (3, 4)),
    ] {
        let pos = eval_str_at(src, "/private/tmp", Origin::File(F))
            .err()
            .and_then(|e| e.pos().cloned());
        assert!(pos.is_some(), "no position for {src:?}");
        let Some(pos) = pos else { continue };
        assert_eq!((pos.line, pos.column), want, "for {src:?}");
    }
}
