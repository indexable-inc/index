//! `__curPos` answers the position of its own token.
//!
//! ENG-12713. It was compile-time fatal (`undefined variable '__curPos'`),
//! and `nixos/modules/tasks/filesystems/zfs.nix:705` writes
//! `inherit (__curPos) file;` from a module that is in the default NixOS
//! module list, so every fleet host's system evaluation died on it. There was
//! no workaround either: the token failing at compile time means containing
//! it anywhere reachable fails, so `if false then __curPos else 7` failed
//! here and answered 7 under cppnix.
//!
//! Every expectation below was read off the fork's own binary
//! (`nix-instantiate --eval --strict`) rather than off cppnix's source, and
//! the oracle rows are in the PR body.

use nix_eval_rs::compile::Origin;
use nix_eval_rs::eval::{eval_str, eval_str_at};

fn at_file(src: &str, path: &str) -> String {
    match eval_str_at(src, "/tmp", Origin::File(path)) {
        Ok(text) => text,
        Err(error) => format!("{error:?}"),
    }
}

fn as_string(src: &str) -> String {
    match eval_str(src) {
        Ok(text) => text,
        Err(error) => format!("{error:?}"),
    }
}

/// `nix-instantiate --eval --strict /tmp/curpos-oracle/f.nix` where the file
/// is one line reading `__curPos`:
/// `{ column = 1; file = "/tmp/curpos-oracle/f.nix"; line = 1; }`.
#[test]
fn a_token_at_the_start_of_a_file() {
    assert_eq!(
        at_file("__curPos", "/tmp/curpos-oracle/f.nix"),
        r#"{ column = 1; file = "/tmp/curpos-oracle/f.nix"; line = 1; }"#
    );
}

/// The set `__curPos` builds takes its names from an attr site like every
/// other literal (`emit_cur_pos`), with NO position of their own, so
/// `unsafeGetAttrPos` answers `null` for them. The ordinary attribute is the
/// control: the same call answers a record for a name that was written.
#[test]
fn the_synthesised_attributes_have_no_position_of_their_own() {
    let src = "let p = __curPos; s = { a = 1; }; in [ (builtins.unsafeGetAttrPos \"file\" p) \
               (builtins.unsafeGetAttrPos \"line\" p) (builtins.unsafeGetAttrPos \"column\" p) \
               (builtins.unsafeGetAttrPos \"a\" s).line ]";
    assert_eq!(
        at_file(src, "/tmp/curpos-oracle/g.nix"),
        "[ null null null 1 ]"
    );
}

/// The corpus case, `tests/functional/lang/eval-okay-curpos.nix`, whose `.exp`
/// is `[ 3 7 4 9 ]`: line and column are 1-based and point at the first
/// character of the token.
#[test]
fn the_corpus_positions() {
    let src =
        "# Bla\nlet\n  x = __curPos;\n    y = __curPos;\nin [ x.line x.column y.line y.column ]\n";
    assert_eq!(at_file(src, "/tmp/eval-okay-curpos.nix"), "[ 3 7 4 9 ]");
}

/// `column` counts **bytes**, not characters: cppnix computes it as
/// `1 + (offset - lineStartOffset)` over the raw buffer
/// (`src/libutil/pos-table.cc:47`). Measured: the fork answers `column = 32`
/// for this line, where the token is the 26th character and the 32nd byte.
#[test]
fn the_column_is_a_byte_offset() {
    let src = "let s = \"h\u{e9}llo\u{2014}x\"; in { p = __curPos; }\n";
    assert_eq!(
        at_file(src, "/tmp/utf8.nix"),
        r#"{ p = { column = 32; file = "/tmp/utf8.nix"; line = 1; }; }"#
    );
}

/// A string origin has no file, and cppnix answers `null` for it rather than
/// inventing a name (`mkPos`, `eval.cc:1034`). Measured:
/// `nix-instantiate --eval --strict -E '__curPos'` prints `null`.
#[test]
fn an_expression_with_no_file_is_null() {
    assert_eq!(as_string("__curPos"), "null");
    assert_eq!(as_string("builtins.typeOf __curPos"), "\"null\"");
}

/// cppnix's parser turns the spelling into an `ExprPos` at the `expr_simple :
/// ID` rule (`parser.y:348`), before it is ever a variable, so nothing
/// shadows it. Measured on a file: `let __curPos = 1; in __curPos` answers a
/// position, and `(__curPos: __curPos) 42` answers one too.
#[test]
fn nothing_shadows_it() {
    assert_eq!(
        at_file("let __curPos = 1; in __curPos", "/tmp/shadow.nix"),
        r#"{ column = 22; file = "/tmp/shadow.nix"; line = 1; }"#
    );
    // A list element is a lazy position, which reaches the compiler through a
    // different path than a strict one; it was the path that got this wrong.
    assert_eq!(
        at_file("let __curPos = 1; in [ __curPos ]", "/tmp/lazy.nix"),
        r#"[ { column = 24; file = "/tmp/lazy.nix"; line = 1; } ]"#
    );
}

/// An attribute *name* spelled `__curPos` is an ordinary name: the attrpath
/// production is not `expr_simple`. Measured: `{ __curPos = 3; }.__curPos`
/// is `3`.
#[test]
fn an_attribute_of_that_name_is_untouched() {
    assert_eq!(at_file("{ __curPos = 3; }.__curPos", "/tmp/attr.nix"), "3");
}

/// And `inherit __curPos;` is a variable reference through yet another
/// production, so it stays an undefined variable on both arms. Measured:
/// `nix-instantiate --eval --strict -E '{ inherit __curPos; }'` fails with
/// `undefined variable '__curPos'`.
#[test]
fn inherit_of_that_name_is_still_undefined() {
    let answer = at_file("{ inherit __curPos; }", "/tmp/inherit.nix");
    assert!(
        answer.contains("undefined variable"),
        "expected an undefined variable, got {answer}"
    );
}

/// `inherit (__curPos) file;` is the shape `zfs.nix` uses, and the one that
/// blocked every NixOS evaluation.
#[test]
fn the_zfs_shape() {
    assert_eq!(
        at_file("{ inherit (__curPos) file line; }", "/tmp/zfs.nix"),
        r#"{ file = "/tmp/zfs.nix"; line = 1; }"#
    );
}

/// The reporter's no-workaround case: the token is fatal at compile time, so
/// merely being reachable from the compiler is enough. cppnix answers 7.
#[test]
fn an_unreached_branch_does_not_fail_the_compile() {
    assert_eq!(
        at_file("if false then __curPos else 7", "/tmp/branch.nix"),
        "7"
    );
    assert_eq!(as_string("if false then __curPos else 7"), "7");
}

/// The shape ENG-12713 was measured on: a module read from disk with
/// `import`, whose `__curPos` must name *that* file and not the importer.
/// This exercises `Vm::import_module`, which is the path every NixOS module
/// arrives through.
#[test]
fn an_imported_file_reports_its_own_path() -> Result<(), Box<dyn core::error::Error>> {
    let dir = std::env::temp_dir().join("ixe-curpos-import");
    std::fs::create_dir_all(&dir)?;
    let inner = dir.join("inner.nix");
    std::fs::write(&inner, "{ inherit (__curPos) file line; }\n")?;
    let outer = dir.join("outer.nix");
    let answer = at_file(
        &format!("import {}", inner.display()),
        &outer.to_string_lossy(),
    );
    assert_eq!(
        answer,
        format!(r#"{{ file = "{}"; line = 1; }}"#, inner.display()),
        "an imported module reported the importer's position"
    );
    Ok(())
}

/// Two files with identical text in one directory get their own positions.
/// The end-to-end half of `modcache::tests::the_same_source_at_two_paths_does_not_share_a_row`,
/// through the VM's own module cache rather than the on-disk one: both are
/// keyed on the base directory, and the base directory is the same here.
#[test]
fn two_identical_files_in_one_directory_do_not_share_a_position()
-> Result<(), Box<dyn core::error::Error>> {
    let dir = std::env::temp_dir().join("ixe-curpos-twins");
    std::fs::create_dir_all(&dir)?;
    let one = dir.join("one.nix");
    let two = dir.join("two.nix");
    std::fs::write(&one, "(__curPos).file\n")?;
    std::fs::write(&two, "(__curPos).file\n")?;
    let outer = dir.join("outer.nix");
    let answer = at_file(
        &format!("[ (import {}) (import {}) ]", one.display(), two.display()),
        &outer.to_string_lossy(),
    );
    assert_eq!(
        answer,
        format!(r#"[ "{}" "{}" ]"#, one.display(), two.display()),
        "the second file was served the first one's compiled position"
    );
    Ok(())
}
