#![cfg(test)]
//! All C ABI headers against the exported Rust ABI modules, read as text.
//! Includes the evaluator, command, and lock-graph interfaces.
//!
//! The header is hand-written on purpose: it carries the prose that explains
//! what each hook must do, which a generator would delete. The price of
//! writing it by hand is that nothing checks it, and the C++ compile only
//! catches the drift that is also a type error. A reordered `void * ctx`, a
//! `size_t` and a `*const u8` swapped, or a field moved within
//! `IxeHostVtable` all compile on both sides and land as a call through the
//! wrong function pointer at run time.
//!
//! So this module parses both files and compares them. cbindgen with a
//! checked-in diff would cover the same failure; ENG-13092 blesses this
//! instead because it keeps the prose.
//!
//! What "equivalent" means here is deliberately narrow, because the ABI is
//! narrow. Parameter *names* are not compared -- the header calls
//! `ixe_set_home_dir`'s argument `dir` and `capi.rs` calls it `v`, and neither
//! spelling reaches a caller. Struct *field* names are compared, because a
//! C caller writes them and a rename is a source break even when the layout
//! holds. Everything else is types, counts and order.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The header the cppnix bridge compiles against, relative to the crate root.
const HEADER_PATH: &str = "include/ixe.h";

/// The Rust side of the same ABI.
const CAPI_PATH: &str = "src/capi.rs";

/// Read one of the two files, from the crate root this build was compiled
/// against.
///
/// A runtime read and not `include_str!`, which is what this wants to be and
/// what the first version of it was. `modcache`'s
/// `the_fingerprint_input_set_names_everything_that_can_change_the_compiler`
/// requires every `include_str!` target that is not a sibling `.rs` to appear
/// in `EMBEDDED_FILES`, and `EMBEDDED_FILES` is folded into the compiler
/// fingerprint -- which is the compile cache's key and part of every memoised
/// evaluation's. That rule is right for what put it there: `derivation.nix`
/// is the body of the `derivation` global, so its text reaches every
/// derivation an expression builds and a stale entry answers wrongly
/// (ENG-13010).
///
/// It is wrong for `ixe.h`. Nothing in an evaluation reads the header: it is
/// declarations for the C++ compiler and text for this test, it is four
/// fifths prose, and no edit to it can change what any Nix expression
/// evaluates to. Putting it in the key would discard every cached module and
/// every memoised answer on the fleet each time someone reworded a comment,
/// and would quietly redefine `EMBEDDED_FILES` from "files that decide what a
/// compiled `Module` means" into "files some test reads" -- which is the
/// version the next person adding a fixture would follow.
///
/// `capi.rs` is read the same way even though embedding it would be free (it
/// is a sibling `.rs`, already in the fingerprint through the source walk), so
/// that there is one rule here rather than two spellings a reader has to
/// account for. `CARGO_MANIFEST_DIR` is resolved at compile time, exactly as
/// `modcache`'s `crate_root` does it, so this still needs no working
/// directory; only the read happens at run time, and it sees the same bytes
/// `include_str!` would have because cargo rebuilds this crate whenever
/// either file moves.
fn read_source(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // The crate's own tree, during the crate's own test suite.
        // `modcache`'s `fingerprint_inputs` says the same thing the same way.
        Err(error) => unreachable!("cannot read {} for the ABI check: {error}", path.display()),
    }
}

/// Entry points `capi.rs` exports that `ixe.h` deliberately does not declare.
///
/// An exemption here is a hole in the check, so each one is asserted from
/// both ends: the name must still be an export, and must still be absent from
/// the header. Declaring it, or deleting it, fails this test until the row is
/// removed -- the exemption cannot outlive the thing it excuses.
///
/// Empty, and meant to stay that way. The one row it ever held was
/// `ixe_set_cache_verify_rate`, found by this test on the day it was written
/// (ENG-13092): exported and documented in capi.rs, declared in no header and
/// called from nowhere in this tree, so no embedder could reach the cache
/// self-check it turns on. The row said "either ixe.h should declare it or
/// capi.rs should stop exporting it"; ixe.h declares it and the cppnix bridge
/// calls it from `eval-cache-verify-rate`, so the exemption expired and the
/// signature is compared like every other.
const UNDECLARED_EXPORTS: &[(&str, &str)] = &[];

/// Rust primitives and their C spellings in this header.
///
/// A base type absent from this table passes through unchanged, which is what
/// carries the shared struct names -- `IxeSession`, `IxeHostVtable`,
/// `IxeBytes`, `IxeArgument` are spelled identically on both sides. The
/// consequence for a genuinely unknown type is a mismatch naming it, not a
/// silent pass, because the header will not have spelled it the Rust way.
const RUST_TO_C: &[(&str, &str)] = &[
    ("()", "void"),
    ("bool", "_Bool"),
    ("c_char", "char"),
    ("c_int", "int"),
    ("c_uint", "unsigned int"),
    ("c_void", "void"),
    ("f32", "float"),
    ("f64", "double"),
    ("i16", "short"),
    ("i32", "int"),
    ("i64", "int64_t"),
    ("i8", "signed char"),
    ("isize", "ptrdiff_t"),
    ("u16", "unsigned short"),
    ("u32", "unsigned int"),
    ("u64", "uint64_t"),
    ("u8", "unsigned char"),
    ("usize", "size_t"),
];

/// Words that end a C type rather than name a declarator.
///
/// `split_declarator` peels a trailing identifier off `size_t path_len` to get
/// the type; without this list it would peel `int` off an unnamed `unsigned
/// int` and report the type as `unsigned`.
const C_TYPE_WORDS: &[&str] = &[
    "_Bool",
    "char",
    "double",
    "float",
    "int",
    "int16_t",
    "int32_t",
    "int64_t",
    "int8_t",
    "long",
    "ptrdiff_t",
    "short",
    "signed",
    "size_t",
    "uint16_t",
    "uint32_t",
    "uint64_t",
    "uint8_t",
    "unsigned",
    "void",
];

/// One parameter or field: what the source says, and what it means.
///
/// Both are kept because a failure message quoting only the canonical form
/// sends the reader looking for a string that is in neither file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Typed {
    /// The declarator as written, e.g. `size_t path_len` or `*const u8`.
    written: String,
    /// The canonical C spelling both sides are reduced to.
    canon: String,
}

/// A function, reduced to the part the ABI fixes.
#[derive(Clone, Debug)]
struct Signature {
    ret: Typed,
    params: Vec<Typed>,
}

/// A struct field: the name a C caller writes, and its type.
#[derive(Clone, Debug)]
struct Field {
    name: String,
    ty: Typed,
}

/// Everything one side of the boundary declares.
#[derive(Default)]
struct Surface {
    /// Function name to signature. Ordered so failure messages are stable.
    functions: BTreeMap<String, Signature>,
    /// Struct name to its fields, in declaration order.
    structs: BTreeMap<String, Vec<Field>>,
    /// Function-pointer typedef name to its canonical signature string.
    fn_pointers: BTreeMap<String, String>,
    /// Anything the parser could not classify. A non-empty list fails the
    /// test rather than being skipped, because a construct the parser drops
    /// is a construct nothing compares.
    problems: Vec<String>,
    /// How many `#[unsafe(no_mangle)]` and `#[repr(C)]` attribute lines the
    /// file carries, counted without going through the parser.
    ///
    /// The parser's own totals cannot police the parser: a pattern that
    /// matched nothing reports "0 functions, 0 disagreements", which reads
    /// exactly like a header that agrees with everything. These two are the
    /// independent reading the totals are checked against. Zero on the C
    /// side, where there are no attributes to count.
    no_mangle_attributes: usize,
    repr_c_attributes: usize,
}

// ---------------------------------------------------------------- utilities

/// Collapse every run of whitespace to one space.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split at the parenthesised group opening at the first `(`, returning the
/// text before it, the text inside it, and the text after its match.
fn split_paren_group(text: &str) -> Option<(&str, &str, &str)> {
    let open = text.find('(')?;
    let mut depth = 0usize;
    let mut close = None;
    for (i, ch) in text.char_indices() {
        if i < open {
            continue;
        }
        if ch == '(' {
            depth = depth.saturating_add(1);
        } else if ch == ')' {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                close = Some(i);
                break;
            }
        }
    }
    let close = close?;
    Some((
        text.get(..open)?,
        text.get(open.saturating_add(1)..close)?,
        text.get(close.saturating_add(1)..)?,
    ))
}

/// Split a comma-separated list, ignoring commas inside parentheses.
///
/// Angle brackets are not tracked because nothing this is handed contains a
/// comma inside them: `Option<CopyToStoreFn>` has one type argument, and a
/// struct field list is split on lines rather than commas.
fn split_commas(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in text.char_indices() {
        match ch {
            '(' => depth = depth.saturating_add(1),
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                if let Some(part) = text.get(start..i) {
                    let part = part.trim();
                    if !part.is_empty() {
                        out.push(part);
                    }
                }
                start = i.saturating_add(1);
            }
            _ => {}
        }
    }
    if let Some(part) = text.get(start..) {
        let part = part.trim();
        if !part.is_empty() {
            out.push(part);
        }
    }
    out
}

/// Split a C declarator such as `const unsigned char ** out` into its type
/// and the name it declares, if it declares one.
fn split_declarator(text: &str) -> (String, Option<String>) {
    let text = text.trim();
    let mut cut = text.len();
    for (i, ch) in text.char_indices().rev() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cut = i;
        } else {
            break;
        }
    }
    let (Some(head), Some(tail)) = (text.get(..cut), text.get(cut..)) else {
        return (text.to_owned(), None);
    };
    let head = head.trim();
    if head.is_empty() || tail.is_empty() || C_TYPE_WORDS.contains(&tail) {
        return (text.to_owned(), None);
    }
    (head.to_owned(), Some(tail.to_owned()))
}

/// Reduce a C type to one spelling: pointee constness, base, pointer depth.
///
/// Constness is collapsed to a single flag rather than tracked per level.
/// That is exact for every type in this header, which only ever writes
/// `const T *` and `const T **`, and would conflate a top-level `T * const`
/// with `const T *` if one ever appeared. The Rust side makes the same
/// simplification, so the two agree on what they both ignore.
fn canon_c(text: &str) -> String {
    let spaced = text.replace('*', " * ");
    let mut stars = 0usize;
    let mut is_const = false;
    let mut words: Vec<&str> = Vec::new();
    for word in spaced.split_whitespace() {
        match word {
            "*" => stars = stars.saturating_add(1),
            "const" => is_const = true,
            "struct" => {}
            other => words.push(other),
        }
    }
    let mut out = String::new();
    if is_const {
        out.push_str("const ");
    }
    // On supported x86-64/AArch64 Linux and Darwin ABIs, stdint.h uses
    // unsigned char for uint8_t and unsigned int for uint32_t. This is
    // typedef identity, not merely equal width: signedness, other integer
    // types and pointer qualifiers remain distinct. Unknown ABIs fail closed.
    let known_integer_abi = cfg!(all(
        any(target_os = "linux", target_os = "macos"),
        any(target_arch = "x86_64", target_arch = "aarch64")
    ));
    match words.as_slice() {
        ["uint8_t"] if known_integer_abi => out.push_str("unsigned char"),
        ["uint32_t"] if known_integer_abi => out.push_str("unsigned int"),
        _ => out.push_str(&words.join(" ")),
    }
    if stars > 0 {
        out.push(' ');
        for _ in 0..stars {
            out.push('*');
        }
    }
    out
}

#[test]
fn fixed_width_aliases_preserve_qualifiers_and_integer_types() {
    let aliases = BTreeMap::new();
    if cfg!(all(
        any(target_os = "linux", target_os = "macos"),
        any(target_arch = "x86_64", target_arch = "aarch64")
    )) {
        for (c, rust) in [
            ("uint8_t", "u8"),
            ("uint8_t *", "*mut u8"),
            ("const uint8_t *", "*const u8"),
            ("const uint8_t **", "*mut *const u8"),
            ("uint32_t", "u32"),
            ("uint32_t *", "*mut u32"),
            ("const uint32_t *", "*const u32"),
            ("const uint32_t **", "*mut *const u32"),
        ] {
            assert_eq!(canon_c(c), canon_rust(rust, &aliases));
        }
    } else {
        assert_eq!(canon_c("uint8_t"), "uint8_t");
        assert_eq!(canon_c("uint32_t"), "uint32_t");
    }
    assert_ne!(canon_c("unsigned char"), canon_c("char"));
    assert_ne!(canon_c("unsigned char"), canon_c("signed char"));
    assert_ne!(canon_c("unsigned char"), canon_c("uint16_t"));
    assert_ne!(canon_c("uint8_t *"), canon_c("const uint8_t *"));
    assert_ne!(canon_c("uint8_t *"), canon_c("uint8_t **"));
    assert_ne!(canon_c("uint32_t"), canon_c("int"));
    assert_ne!(canon_c("uint32_t"), canon_c("unsigned long"));
    assert_ne!(canon_c("uint32_t"), canon_c("uint64_t"));
    assert_ne!(canon_c("uint32_t"), canon_c("uint8_t"));
    assert_ne!(canon_c("uint32_t *"), canon_c("const uint32_t *"));
    assert_ne!(canon_c("uint32_t *"), canon_c("uint32_t **"));
}

/// Resolve a canonical C type through the header's own typedefs.
///
/// `IxeHandle *` becomes `uint64_t *`; `ixe_warn_fn` becomes the expanded
/// function-pointer signature, so a vtable field is compared against what the
/// typedef says rather than against its name.
fn resolve_c(
    canon: &str,
    values: &BTreeMap<String, String>,
    fns: &BTreeMap<String, String>,
) -> String {
    let stars = canon.matches('*').count();
    let base = canon.replace('*', "");
    let base = base.trim().trim_start_matches("const ").trim();
    if let Some(expanded) = fns.get(base) {
        if stars == 0 {
            return expanded.clone();
        }
        // A pointer to a function-pointer typedef. Nothing here declares one;
        // report it rather than expanding it wrongly.
        return format!("{expanded} {}", "*".repeat(stars));
    }
    let Some(alias) = values.get(base) else {
        return canon.to_owned();
    };
    let combined = format!("{alias} {}", "*".repeat(stars));
    canon_c(&combined)
}

/// Reduce a Rust type to the C spelling it lowers to.
///
/// Pointer qualifiers follow the innermost one, which is how C writes the
/// shapes this ABI uses: `*mut *const c_char` is `const char **`, because the
/// outer `mut` in C would have to be spelled `char * const *` and never is.
fn canon_rust(text: &str, fns: &BTreeMap<String, String>) -> String {
    let mut rest = text.trim();
    if let Some(inner) = rest
        .strip_prefix("Option<")
        .and_then(|t| t.strip_suffix('>'))
    {
        rest = inner.trim();
    }
    if let Some(expanded) = fns.get(rest) {
        return expanded.clone();
    }
    let mut stars = 0usize;
    let mut innermost_const = false;
    loop {
        if let Some(inner) = rest.strip_prefix("*const ") {
            stars = stars.saturating_add(1);
            innermost_const = true;
            rest = inner.trim();
        } else if let Some(inner) = rest.strip_prefix("*mut ") {
            stars = stars.saturating_add(1);
            innermost_const = false;
            rest = inner.trim();
        } else {
            break;
        }
    }
    let base = RUST_TO_C
        .iter()
        .find(|(rust, _)| *rust == rest)
        .map_or(rest, |(_, c)| *c);
    let mut out = String::new();
    if innermost_const {
        out.push_str("const ");
    }
    out.push_str(base);
    if stars > 0 {
        out.push(' ');
        for _ in 0..stars {
            out.push('*');
        }
    }
    out
}

/// One printable line for a function-pointer signature, the form both sides
/// are reduced to before being compared.
fn fn_pointer_text(ret: &str, params: &[String]) -> String {
    format!("{ret} (*)({})", params.join(", "))
}

// ----------------------------------------------------------- the C header

/// Remove `/* ... */` and `// ...` runs, replacing each with a space so that
/// tokens either side of a comment do not fuse.
fn strip_c_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch == '/' {
            match chars.peek().map(|(_, c)| *c) {
                Some('*') => {
                    chars.next();
                    let mut prev = '\0';
                    for (_, c) in chars.by_ref() {
                        if prev == '*' && c == '/' {
                            break;
                        }
                        // Newlines are kept so the preprocessor-line filter
                        // below still sees the file's line structure.
                        if c == '\n' {
                            out.push('\n');
                        }
                        prev = c;
                    }
                    out.push(' ');
                    continue;
                }
                Some('/') => {
                    for (_, c) in chars.by_ref() {
                        if c == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                    continue;
                }
                _ => {}
            }
        }
        out.push(ch);
    }
    out
}

/// Split the header into top-level declarations.
///
/// Comments go first, then preprocessor lines, then the `extern "C" {`
/// wrapper -- which has to go before the brace counting, or its unmatched
/// brace holds the depth at one for the whole file and nothing splits.
fn header_statements(src: &str) -> (Vec<String>, Vec<String>) {
    let mut problems = Vec::new();
    let stripped = strip_c_comments(src);
    let body: String = stripped
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("extern \"C\" {", "");

    let mut statements = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in body.chars() {
        match ch {
            '{' => {
                depth = depth.saturating_add(1);
                current.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ';' if depth == 0 => {
                let statement = collapse(&current);
                if !statement.is_empty() {
                    statements.push(statement);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    // What follows the last `;` should be the close of the `extern "C"` block
    // and nothing else. Anything more is a declaration this parser dropped.
    let tail = collapse(&current).replace('}', "");
    if !tail.trim().is_empty() {
        problems.push(format!(
            "ixe.h has text after its last declaration that this parser did \
             not classify: `{tail}`"
        ));
    }
    (statements, problems)
}

/// Parse the evaluator, command, flake-show, and lock-graph headers as one ABI surface.
fn parse_header() -> Surface {
    let mut surface = Surface::default();
    let mut statements = Vec::new();
    for path in [
        HEADER_PATH,
        "include/ixe-command.h",
        "include/ixe-flake-show.h",
        "include/ixe-search.h",
        "include/ixe-flake-check.h",
        "include/ixe-source-position.h",
        "include/ixe-lock-graph.h",
    ] {
        let (parsed, problems) = header_statements(&read_source(path));
        statements.extend(parsed);
        surface.problems.extend(problems);
    }

    // Two passes: typedefs first, so a declaration mentioning `IxeHandle` or
    // `ixe_warn_fn` can be resolved whatever order the file puts them in.
    let mut value_aliases: BTreeMap<String, String> = BTreeMap::new();
    let mut struct_bodies: Vec<(String, String)> = Vec::new();
    let mut function_statements: Vec<String> = Vec::new();

    for statement in &statements {
        if let Some(rest) = statement.strip_prefix("typedef ") {
            if rest.starts_with("struct ") && !rest.contains('{') {
                // `typedef struct IxeSession IxeSession;` -- an opaque type,
                // named the same on both sides, so nothing to record.
                continue;
            }
            if rest.contains("(*") {
                match parse_c_fn_pointer(rest) {
                    Ok((name, ret, params)) => {
                        surface
                            .fn_pointers
                            .insert(name, fn_pointer_text(&ret, &params));
                    }
                    Err(why) => surface.problems.push(why),
                }
                continue;
            }
            if let Some(after) = rest.strip_prefix("struct ") {
                match parse_c_anonymous_struct(after) {
                    Ok((name, body)) => struct_bodies.push((name, body)),
                    Err(why) => surface.problems.push(why),
                }
                continue;
            }
            let (ty, name) = split_declarator(rest);
            let Some(name) = name else {
                surface
                    .problems
                    .push(format!("ixe.h typedef names nothing: `typedef {rest};`"));
                continue;
            };
            // `typedef ixe_read_file_fn ixe_import_fn;` is a function-pointer
            // typedef under a second name, like capi.rs's `pub type ImportFn
            // = ReadFileFn;`, and is counted and resolved as one.
            if let Some(text) = surface.fn_pointers.get(ty.trim()).cloned() {
                surface.fn_pointers.insert(name, text);
                continue;
            }
            value_aliases.insert(name, canon_c(&ty));
            continue;
        }
        if let Some(rest) = statement.strip_prefix("struct ") {
            match parse_c_named_struct(rest) {
                Ok((name, body)) => struct_bodies.push((name, body)),
                Err(why) => surface.problems.push(why),
            }
            continue;
        }
        if statement.contains('(') {
            function_statements.push(statement.clone());
            continue;
        }
        surface.problems.push(format!(
            "ixe.h declaration this parser cannot classify, so nothing \
             compares it: `{statement};`"
        ));
    }

    for statement in &function_statements {
        let Some((head, params, after)) = split_paren_group(statement) else {
            surface.problems.push(format!(
                "ixe.h declaration has no argument list: `{statement};`"
            ));
            continue;
        };
        if !after.trim().is_empty() {
            surface.problems.push(format!(
                "ixe.h declaration has text after its argument list, which \
                 this parser does not understand: `{statement};`"
            ));
            continue;
        }
        let (ret, name) = split_declarator(head);
        let Some(name) = name else {
            surface.problems.push(format!(
                "ixe.h declaration names no function: `{statement};`"
            ));
            continue;
        };
        let mut typed_params = Vec::new();
        for param in split_commas(params) {
            if param == "void" {
                continue;
            }
            let (ty, _) = split_declarator(param);
            typed_params.push(Typed {
                written: collapse(param),
                canon: resolve_c(&canon_c(&ty), &value_aliases, &surface.fn_pointers),
            });
        }
        surface.functions.insert(
            name,
            Signature {
                ret: Typed {
                    written: collapse(&ret),
                    canon: resolve_c(&canon_c(&ret), &value_aliases, &surface.fn_pointers),
                },
                params: typed_params,
            },
        );
    }

    for (name, body) in struct_bodies {
        let mut fields = Vec::new();
        for entry in body.split(';') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (ty, field_name) = split_declarator(entry);
            let Some(field_name) = field_name else {
                surface.problems.push(format!(
                    "ixe.h struct {name} has a member with no name: `{entry};`"
                ));
                continue;
            };
            fields.push(Field {
                name: field_name,
                ty: Typed {
                    written: collapse(entry),
                    canon: resolve_c(&canon_c(&ty), &value_aliases, &surface.fn_pointers),
                },
            });
        }
        surface.structs.insert(name, fields);
    }

    surface
}

/// `typedef int (*ixe_warn_fn)(void * ctx, ...)`, without the leading
/// `typedef ` and the trailing `;`.
fn parse_c_fn_pointer(rest: &str) -> Result<(String, String, Vec<String>), String> {
    let Some((ret, after)) = rest.split_once("(*") else {
        return Err(format!(
            "ixe.h function-pointer typedef is malformed: `typedef {rest};`"
        ));
    };
    let Some((name, after)) = after.split_once(')') else {
        return Err(format!(
            "ixe.h function-pointer typedef has no name: `typedef {rest};`"
        ));
    };
    let Some((_, params, tail)) = split_paren_group(after) else {
        return Err(format!(
            "ixe.h function-pointer typedef has no argument list: `typedef {rest};`"
        ));
    };
    if !tail.trim().is_empty() {
        return Err(format!(
            "ixe.h function-pointer typedef has trailing text: `typedef {rest};`"
        ));
    }
    let mut typed = Vec::new();
    for param in split_commas(params) {
        if param == "void" {
            continue;
        }
        let (ty, _) = split_declarator(param);
        typed.push(canon_c(&ty));
    }
    Ok((name.trim().to_owned(), canon_c(ret), typed))
}

/// `typedef struct { ... } IxeBytes`, without `typedef struct `.
fn parse_c_anonymous_struct(after: &str) -> Result<(String, String), String> {
    let after = after.trim();
    let Some(open) = after.strip_prefix('{') else {
        return Err(format!(
            "ixe.h anonymous struct typedef is malformed: `{after}`"
        ));
    };
    let Some((body, name)) = open.rsplit_once('}') else {
        return Err(format!(
            "ixe.h anonymous struct typedef is unclosed: `{after}`"
        ));
    };
    Ok((name.trim().to_owned(), body.to_owned()))
}

/// `struct IxeHostVtable { ... }`, without the leading `struct `.
fn parse_c_named_struct(rest: &str) -> Result<(String, String), String> {
    let Some((name, open)) = rest.split_once('{') else {
        return Err(format!(
            "ixe.h struct definition has no body: `struct {rest};`"
        ));
    };
    let Some(body) = open.trim_end().strip_suffix('}') else {
        return Err(format!(
            "ixe.h struct definition is unclosed: `struct {rest};`"
        ));
    };
    Ok((name.trim().to_owned(), body.to_owned()))
}

// -------------------------------------------------------------- the Rust

/// True when the item on `index` has `#[unsafe(no_mangle)]` somewhere in the
/// contiguous run of attributes above it.
///
/// The run, not the line immediately above: `ixe_session_eval_question`
/// carries `#[allow(clippy::too_many_arguments)]` between its attribute and
/// its signature. `every_c_entry_point_is_exported` in `capi.rs` walks the
/// same run for the same reason.
fn exported(lines: &[&str], index: usize) -> bool {
    let mut j = index;
    while let Some(prev) = j.checked_sub(1).and_then(|k| lines.get(k)) {
        let prev = prev.trim();
        if prev == "#[unsafe(no_mangle)]" {
            return true;
        }
        if !prev.starts_with("#[") {
            return false;
        }
        j = j.saturating_sub(1);
    }
    false
}

/// Gather the lines of a multi-line item starting at `index`, stopping once
/// the parentheses balance and the line carrying the closing one is complete.
fn gather_item(lines: &[&str], index: usize) -> Option<String> {
    let mut text = String::new();
    let mut depth = 0usize;
    let mut opened = false;
    for line in lines.iter().skip(index) {
        text.push(' ');
        text.push_str(line);
        for ch in line.chars() {
            if ch == '(' {
                depth = depth.saturating_add(1);
                opened = true;
            } else if ch == ')' {
                depth = depth.saturating_sub(1);
            }
        }
        if opened && depth == 0 {
            return Some(collapse(&text));
        }
        // An item with no parentheses at all -- `pub type ImportFn =
        // ReadFileFn;` -- ends at its semicolon. Reading on would glue the
        // next item onto it and count an alias as a function pointer with
        // its neighbour's signature.
        if !opened && line.trim_end().ends_with(';') {
            return Some(collapse(&text));
        }
    }
    None
}

/// Parse every module exporting the C ABI as one surface.
fn parse_capi() -> Surface {
    let src = [
        read_source(CAPI_PATH),
        read_source("src/capi/command.rs"),
        read_source("src/capi/flake_show.rs"),
        read_source("src/capi/search.rs"),
        read_source("src/capi/flake_check.rs"),
        read_source("src/capi/source_position.rs"),
        read_source("src/lock_graph/ffi.rs"),
    ]
    .join("\n");
    let mut surface = Surface {
        no_mangle_attributes: count_lines_equal(&src, "#[unsafe(no_mangle)]"),
        repr_c_attributes: count_lines_equal(&src, "#[repr(C)]"),
        ..Surface::default()
    };
    let lines: Vec<&str> = src.lines().collect();

    // Function-pointer typedefs first: a `#[repr(C)]` field is spelled
    // `Option<CopyToStoreFn>` and has to be resolved through them.
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("pub type ") {
            continue;
        }
        let Some(item) = gather_item(&lines, i) else {
            continue;
        };
        if !item.contains("extern \"C\" fn") {
            // `pub type ImportFn = ReadFileFn;`: the same shape under a second
            // name, registered once its target is known so a vtable field
            // spelled with the alias resolves exactly as one spelled with the
            // target. ixe.h's `typedef ixe_read_file_fn ixe_import_fn;` is
            // read the same way below, which is what keeps the two counts
            // comparable.
            if let Some((name, target)) = item
                .trim_start()
                .trim_start_matches("pub type ")
                .split_once('=')
                && let Some(text) = surface
                    .fn_pointers
                    .get(target.trim().trim_end_matches(';').trim())
                    .cloned()
            {
                surface.fn_pointers.insert(name.trim().to_owned(), text);
            }
            continue;
        }
        let Some((name, after)) = item
            .trim_start()
            .trim_start_matches("pub type ")
            .split_once('=')
        else {
            surface
                .problems
                .push(format!("capi.rs type alias is malformed: `{item}`"));
            continue;
        };
        let Some((_, params, tail)) = split_paren_group(after) else {
            surface.problems.push(format!(
                "capi.rs function-pointer alias has no argument list: `{item}`"
            ));
            continue;
        };
        let ret = tail
            .trim()
            .trim_end_matches(';')
            .trim()
            .strip_prefix("->")
            .map_or_else(|| "()".to_owned(), |r| r.trim().to_owned());
        let empty = BTreeMap::new();
        let typed: Vec<String> = split_commas(params)
            .iter()
            .filter_map(|p| p.split_once(':'))
            .map(|(_, ty)| canon_rust(ty, &empty))
            .collect();
        surface.fn_pointers.insert(
            name.trim().to_owned(),
            fn_pointer_text(&canon_rust(&ret, &empty), &typed),
        );
    }

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let is_export = trimmed.starts_with("pub extern \"C\" fn ")
            || trimmed.starts_with("pub unsafe extern \"C\" fn ");
        if is_export {
            if !exported(&lines, i) {
                // Not part of the ABI as far as the linker is concerned.
                // `every_c_entry_point_is_exported` is what complains.
                continue;
            }
            let Some(item) = gather_item(&lines, i) else {
                surface.problems.push(format!(
                    "capi.rs entry point has an unterminated signature: `{trimmed}`"
                ));
                continue;
            };
            let Some((head, params, tail)) = split_paren_group(&item) else {
                surface.problems.push(format!(
                    "capi.rs entry point has no argument list: `{item}`"
                ));
                continue;
            };
            let Some(name) = head.rsplit(' ').next() else {
                surface
                    .problems
                    .push(format!("capi.rs entry point names no function: `{item}`"));
                continue;
            };
            let ret = tail
                .trim()
                .trim_end_matches('{')
                .trim()
                .strip_prefix("->")
                .map_or_else(|| "()".to_owned(), |r| r.trim().to_owned());
            let params: Vec<Typed> = split_commas(params)
                .iter()
                .filter_map(|p| p.split_once(':').map(|(_, ty)| ty.trim()))
                .map(|ty| Typed {
                    written: ty.to_owned(),
                    canon: canon_rust(ty, &surface.fn_pointers),
                })
                .collect();
            surface.functions.insert(
                name.to_owned(),
                Signature {
                    ret: Typed {
                        written: ret.clone(),
                        canon: canon_rust(&ret, &surface.fn_pointers),
                    },
                    params,
                },
            );
            continue;
        }

        if trimmed != "#[repr(C)]" {
            continue;
        }
        // Skip the rest of the attribute run to reach the item itself.
        let mut j = i.saturating_add(1);
        while lines
            .get(j)
            .is_some_and(|l| l.trim_start().starts_with("#["))
        {
            j = j.saturating_add(1);
        }
        let Some(header_line) = lines.get(j).map(|l| l.trim()) else {
            surface
                .problems
                .push("capi.rs has a #[repr(C)] attribute with no item under it".to_owned());
            continue;
        };
        let Some(name) = header_line
            .strip_prefix("pub struct ")
            .and_then(|r| r.strip_suffix('{'))
        else {
            surface.problems.push(format!(
                "capi.rs #[repr(C)] item is not a `pub struct <name> {{` this \
                 parser understands, so its layout goes uncompared: \
                 `{header_line}`"
            ));
            continue;
        };
        let mut fields = Vec::new();
        let mut k = j.saturating_add(1);
        let mut closed = false;
        while let Some(field_line) = lines.get(k) {
            let field_line = field_line.trim();
            if field_line == "}" {
                closed = true;
                break;
            }
            k = k.saturating_add(1);
            if field_line.is_empty() || field_line.starts_with("//") {
                continue;
            }
            let Some((field_name, ty)) = field_line
                .trim_end_matches(',')
                .strip_prefix("pub ")
                .and_then(|f| f.split_once(':'))
            else {
                surface.problems.push(format!(
                    "capi.rs struct {} has a member this parser cannot read: `{field_line}`",
                    name.trim()
                ));
                continue;
            };
            let ty = ty.trim();
            fields.push(Field {
                name: field_name.trim().to_owned(),
                ty: Typed {
                    written: ty.to_owned(),
                    canon: canon_rust(ty, &surface.fn_pointers),
                },
            });
        }
        if !closed {
            surface
                .problems
                .push(format!("capi.rs struct {} is unclosed", name.trim()));
        }
        surface.structs.insert(name.trim().to_owned(), fields);
    }

    surface
}

// --------------------------------------------------------------- the tests

/// Count the lines whose whole trimmed content is `needle`.
fn count_lines_equal(src: &str, needle: &str) -> usize {
    src.lines().filter(|l| l.trim() == needle).count()
}

/// Refuse a parse that produced nothing, before any comparison runs.
///
/// Called by all three tests rather than by one of them, because an empty
/// parse compares cleanly against anything: "compared 0 functions, found 0
/// disagreements" is a green test that means neither file was read. A test
/// that relies on a sibling to have checked this is a test that passes on its
/// own when the sibling is deleted or filtered out.
///
/// The counts are also checked against an independent reading of the
/// attributes in `capi.rs`, so a parser that sees *some* of the file cannot
/// hide behind a positive number either.
fn refuse_empty(header: &Surface, capi: &Surface) {
    assert!(
        header.problems.is_empty(),
        "the C ABI headers parser did not understand part of the header, so that part \
         is compared against nothing: {:#?}",
        header.problems
    );
    assert!(
        capi.problems.is_empty(),
        "the Rust ABI modules parser did not understand part of the file, so that part \
         is compared against nothing: {:#?}",
        capi.problems
    );
    assert!(
        capi.no_mangle_attributes > 0 && capi.repr_c_attributes > 0,
        "Rust ABI modules read as {} #[unsafe(no_mangle)] attributes and {} #[repr(C)] \
         attributes. Zero of either means the file was not read at all, and \
         every comparison would then pass vacuously.",
        capi.no_mangle_attributes,
        capi.repr_c_attributes
    );
    assert_eq!(
        capi.functions.len(),
        capi.no_mangle_attributes,
        "parsed {} exported functions out of Rust ABI modules but those modules carry {} \
         #[unsafe(no_mangle)] attributes. The parser is missing entry points, \
         and the ones it misses are compared against nothing.",
        capi.functions.len(),
        capi.no_mangle_attributes
    );
    assert_eq!(
        capi.structs.len(),
        capi.repr_c_attributes,
        "parsed {} #[repr(C)] structs out of Rust ABI modules but those modules carry {} \
         #[repr(C)] attributes. Either the parser is missing a struct or the \
         attribute is on something that is not one; both leave a layout \
         uncompared.",
        capi.structs.len(),
        capi.repr_c_attributes
    );
    assert!(
        !header.functions.is_empty()
            && !header.structs.is_empty()
            && !header.fn_pointers.is_empty(),
        "parsed {} functions, {} structs and {} function-pointer typedefs out \
         of C ABI headers. A zero here is a parser that read nothing, which looks \
         exactly like a header that agrees with everything.",
        header.functions.len(),
        header.structs.len(),
        header.fn_pointers.len()
    );
}

#[test]
fn both_files_parse_whole() {
    let header = parse_header();
    let capi = parse_capi();
    refuse_empty(&header, &capi);

    assert_eq!(
        header.fn_pointers.len(),
        capi.fn_pointers.len(),
        "C ABI headers declares {} function-pointer typedefs and Rust ABI modules declares {}. \
         They are compared through the vtable fields that use them, so an \
         unequal count means one side has a hook shape the other does not.",
        header.fn_pointers.len(),
        capi.fn_pointers.len()
    );

    // Those typedefs are only ever compared as the types of vtable fields, so
    // one that no field mentions would be declared here and checked nowhere.
    // Assert the coverage rather than assuming it.
    let mut referenced: Vec<&String> = Vec::new();
    for fields in header.structs.values() {
        for field in fields {
            for (name, expanded) in &header.fn_pointers {
                if field.ty.canon == *expanded && !referenced.contains(&name) {
                    referenced.push(name);
                }
            }
        }
    }
    let unreferenced: Vec<&String> = header
        .fn_pointers
        .keys()
        .filter(|k| !referenced.contains(k))
        .collect();
    assert!(
        unreferenced.is_empty(),
        "these C ABI headers function-pointer typedefs are the type of no struct \
         field, so nothing in this module compares their signatures against \
         Rust ABI modules: {unreferenced:#?}"
    );
}

/// The function-pointer shape cannot catch a documentation-only ABI break.
/// Keep the success payload and both root meanings beside the declaration so
/// an external embedder is not told to return the obsolete plain path.
#[test]
fn find_file_header_documents_the_rooted_wire_answer() {
    let header = read_source(HEADER_PATH);
    let Some((before, _)) = header.split_once("typedef int (*ixe_find_file_fn)") else {
        panic!("C ABI headers no longer declares ixe_find_file_fn")
    };
    let Some((_, comment)) = before.rsplit_once("/*") else {
        panic!("ixe_find_file_fn has no preceding contract comment")
    };
    for required in [
        "root NUL accessor-relative-path",
        "ambient rootFS accessor",
        "storeFS mount table",
    ] {
        assert!(
            comment.contains(required),
            "ixe_find_file_fn's contract omits {required:?}: {comment}"
        );
    }
}

#[test]
fn every_export_matches_its_header_declaration() {
    let header = parse_header();
    let capi = parse_capi();
    refuse_empty(&header, &capi);
    let mut faults: Vec<String> = Vec::new();

    for (name, rust) in &capi.functions {
        let Some(c) = header.functions.get(name) else {
            if UNDECLARED_EXPORTS.iter().any(|(n, _)| n == name) {
                continue;
            }
            faults.push(format!(
                "{name}: exported from Rust ABI modules and declared nowhere in C ABI headers, \
                 so the bridge cannot call it and nothing checks its shape"
            ));
            continue;
        };
        if c.ret.canon != rust.ret.canon {
            faults.push(format!(
                "{name}: returns `{}` in C ABI headers and `{}` in Rust ABI modules (written `{}`)",
                c.ret.canon, rust.ret.canon, rust.ret.written
            ));
        }
        if c.params.len() != rust.params.len() {
            faults.push(format!(
                "{name}: takes {} parameters in C ABI headers and {} in Rust ABI modules",
                c.params.len(),
                rust.params.len()
            ));
            continue;
        }
        for (i, (cp, rp)) in c.params.iter().zip(rust.params.iter()).enumerate() {
            if cp.canon != rp.canon {
                let position = i.saturating_add(1);
                faults.push(format!(
                    "{name}: parameter {position} is `{}` in C ABI headers (written \
                     `{}`) and `{}` in Rust ABI modules (written `{}`)",
                    cp.canon, cp.written, rp.canon, rp.written
                ));
            }
        }
    }

    for name in header.functions.keys() {
        if !capi.functions.contains_key(name) {
            faults.push(format!(
                "{name}: declared in C ABI headers and exported by no \
                 #[unsafe(no_mangle)] function in Rust ABI modules. A caller that \
                 believes the header gets an undefined symbol at link time."
            ));
        }
    }

    // An exemption that no longer describes reality is a hole nobody can see,
    // so each one is checked from both ends and expires on its own.
    for (name, why) in UNDECLARED_EXPORTS {
        assert!(
            capi.functions.contains_key(*name),
            "UNDECLARED_EXPORTS excuses `{name}`, which Rust ABI modules no longer \
             exports. Delete the row. The reason it carried was: {why}"
        );
        assert!(
            !header.functions.contains_key(*name),
            "UNDECLARED_EXPORTS excuses `{name}` from having a declaration, \
             and C ABI headers now declares it. Delete the row so the declaration is \
             compared. The reason it carried was: {why}"
        );
    }

    let mut message = format!(
        "compared {} exported functions in Rust ABI modules against {} declarations in \
         C ABI headers and found {} disagreement(s). The C++ compile catches only the \
         subset that is also a type error; the rest is a call through the \
         wrong signature.\n",
        capi.functions.len(),
        header.functions.len(),
        faults.len()
    );
    for fault in &faults {
        let _ = writeln!(message, "  - {fault}");
    }
    assert!(faults.is_empty(), "{message}");
}

#[test]
fn every_repr_c_struct_matches_its_header_definition() {
    let header = parse_header();
    let capi = parse_capi();
    refuse_empty(&header, &capi);
    let mut faults: Vec<String> = Vec::new();

    for (name, rust) in &capi.structs {
        let Some(c) = header.structs.get(name) else {
            faults.push(format!(
                "{name}: #[repr(C)] in Rust ABI modules and defined nowhere in C ABI headers"
            ));
            continue;
        };
        if c.len() != rust.len() {
            faults.push(format!(
                "{name}: has {} fields in C ABI headers and {} in Rust ABI modules",
                c.len(),
                rust.len()
            ));
            continue;
        }
        for (i, (cf, rf)) in c.iter().zip(rust.iter()).enumerate() {
            let position = i.saturating_add(1);
            if cf.name != rf.name {
                faults.push(format!(
                    "{name}: field {position} is named `{}` in C ABI headers and `{}` \
                     in Rust ABI modules. Field order is the ABI here -- two fields of \
                     the same type swapped is a call through the wrong \
                     function pointer, which compiles on both sides.",
                    cf.name, rf.name
                ));
            }
            if cf.ty.canon != rf.ty.canon {
                faults.push(format!(
                    "{name}: field {position} (`{}`) is `{}` in C ABI headers and \
                     `{}` in Rust ABI modules (written `{}`)",
                    cf.name, cf.ty.canon, rf.ty.canon, rf.ty.written
                ));
            }
        }
    }

    for name in header.structs.keys() {
        if !capi.structs.contains_key(name) {
            faults.push(format!(
                "{name}: defined in C ABI headers and matched by no #[repr(C)] struct \
                 in Rust ABI modules, so its layout is whatever the C side imagines"
            ));
        }
    }

    let compared: usize = capi.structs.values().map(Vec::len).sum();
    let mut message = format!(
        "compared {} #[repr(C)] structs ({compared} fields) in Rust ABI modules against \
         {} struct definitions in C ABI headers and found {} disagreement(s).\n",
        capi.structs.len(),
        header.structs.len(),
        faults.len()
    );
    for fault in &faults {
        let _ = writeln!(message, "  - {fault}");
    }
    assert!(faults.is_empty(), "{message}");
}

/// Every `NAME = value,` enumerator and `#define NAME value` in the header
/// whose name starts with `prefix`, in header order. Comment lines are
/// skipped, so prose that quotes a constant is not read as declaring it.
fn header_constants(header: &str, prefix: &str) -> Vec<(String, i32)> {
    let mut found = Vec::new();
    for raw in header.lines() {
        let line = raw.trim().trim_end_matches(',');
        if line.starts_with('*') || line.starts_with('/') {
            continue;
        }
        let (name, value) = if let Some(rest) = line.strip_prefix("#define ") {
            let mut words = rest.split_whitespace();
            match (words.next(), words.next()) {
                (Some(name), Some(value)) => (name, value),
                _ => continue,
            }
        } else if let Some((name, value)) = line.split_once('=') {
            (name.trim(), value.trim())
        } else {
            continue;
        };
        if !name.starts_with(prefix) {
            continue;
        }
        let value = value
            .parse()
            .unwrap_or_else(|e| panic!("{name} has a non-integer value in ixe.h: {e}"));
        found.push((name.to_owned(), value));
    }
    found
}

/// The header's constants under `prefix` are exactly `expected`, value for
/// value. The struct parser above compares declarations, not values: a
/// header that renumbers a constant while the Rust side keeps its number
/// would pass every other check and misread every answer that carries it.
/// The count is pinned too, so a constant added on one side only cannot
/// hide.
fn assert_header_constants_agree(prefix: &str, expected: &[(&str, i32)]) {
    let header = read_source(HEADER_PATH);
    let found = header_constants(&header, prefix);
    for (name, value) in &found {
        let rust = expected
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("{name} is declared in ixe.h but has no Rust constant"))
            .1;
        assert_eq!(
            *value, rust,
            "{name}: ixe.h says {value}, capi.rs says {rust}"
        );
    }
    assert_eq!(
        found.len(),
        expected.len(),
        "ixe.h declares {} {prefix}* constants, capi.rs has {}",
        found.len(),
        expected.len()
    );
}

/// The realise error class crosses by value: `IFDError` stays its own
/// exception class through it, and a misnumbering would misclassify every
/// disabled-IFD failure.
#[test]
fn realise_error_class_discriminants_agree_with_the_header() {
    use crate::capi::IxeRealiseErrorClass as C;
    assert_header_constants_agree(
        "IXE_REALISE_ERROR_",
        &[
            ("IXE_REALISE_ERROR_OTHER", C::Other as i32),
            (
                "IXE_REALISE_ERROR_IMPORT_FROM_DERIVATION",
                C::ImportFromDerivation as i32,
            ),
        ],
    );
}

/// The realise check status crosses by value: it is what decides whether a
/// build is spawned, skipped, or refused.
#[test]
fn realise_check_statuses_agree_with_the_header() {
    use crate::capi::IxeRealiseCheck as C;
    assert_header_constants_agree(
        "IXE_REALISE_CHECK_",
        &[
            ("IXE_REALISE_CHECK_BUILD", C::Build as i32),
            ("IXE_REALISE_CHECK_FAILED", C::Failed as i32),
            ("IXE_REALISE_CHECK_NOTHING", C::Nothing as i32),
        ],
    );
}

/// The question kinds cross by value too, as `#define`s in the header and
/// `const`s in capi.rs: a served question of the wrong kind would decode
/// the wrong payload.
#[test]
fn question_kinds_agree_with_the_header() {
    use crate::capi as c;
    assert_header_constants_agree(
        "IXE_QUESTION_",
        &[
            ("IXE_QUESTION_SELECT", c::IXE_QUESTION_SELECT),
            ("IXE_QUESTION_DERIVATION", c::IXE_QUESTION_DERIVATION),
            ("IXE_QUESTION_APP", c::IXE_QUESTION_APP),
            ("IXE_QUESTION_FLAKE_SHOW", c::IXE_QUESTION_FLAKE_SHOW),
            (
                "IXE_QUESTION_DERIVATION_PATH",
                c::IXE_QUESTION_DERIVATION_PATH,
            ),
            (
                "IXE_QUESTION_DERIVATION_SET",
                c::IXE_QUESTION_DERIVATION_SET,
            ),
            (
                "IXE_QUESTION_FLAKE_DOCUMENT",
                c::IXE_QUESTION_FLAKE_DOCUMENT,
            ),
            (
                "IXE_QUESTION_SOURCE_POSITION",
                c::IXE_QUESTION_SOURCE_POSITION,
            ),
            (
                "IXE_QUESTION_SEARCH_PACKAGES",
                c::IXE_QUESTION_SEARCH_PACKAGES,
            ),
            ("IXE_QUESTION_FLAKE_CHECK", c::IXE_QUESTION_FLAKE_CHECK),
        ],
    );
}

/// The two auto-argument kinds, which cross as integers.
#[test]
fn auto_argument_kinds_agree_with_the_header() {
    use crate::capi as c;
    assert_header_constants_agree(
        "IXE_AUTO_ARG_",
        &[
            ("IXE_AUTO_ARG_EXPR", c::IXE_AUTO_ARG_EXPR),
            ("IXE_AUTO_ARG_STRING", c::IXE_AUTO_ARG_STRING),
        ],
    );
}
