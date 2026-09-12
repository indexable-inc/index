//! `nix-eval-driver` -- evaluate and instantiate Nix without the C++ CLI.
//!
//! # The plank
//!
//! CLAUDE.md's direction is a ship of Theseus: "each plank is a Rust
//! replacement that carries a differential gate proving it does the same
//! thing, and the C++ plank comes out in the same change or the one after."
//! This is the first plank for the *entry point*. Everything below it was
//! already Rust -- the parser, the compiler, the VM, `builtins.derivationStrict`,
//! the store-path computation and the ATerm writer -- and yet reaching any of
//! it meant running the C++ `nix` binary, because nothing else knew how to
//! read a command line, seed a machine, answer a store question and print a
//! value. That C++ shell is what this removes for the paths it covers.
//!
//! # What it does
//!
//! Two commands, matching the two things `nix-instantiate` is used for:
//!
//! ```text
//! nix-eval-driver eval        -E '1 + 1'          # --eval --strict
//! nix-eval-driver instantiate -E 'derivation {…}' # compute drvPath, write the .drv
//! ```
//!
//! Both accept several inputs at once, and both hand every input to
//! [`crate::run::evaluate`] together so the crate's own scheduler runs them.
//! See [`crate::run`] for the honest statement of what overlaps today.
//!
//! # What it refuses, by name
//!
//! Fetchers, flake locking, `import`-from-derivation and NAR ingestion. Each
//! refusal names itself and prints in the bridge's own spelling, `rust-eval
//! unimplemented: [token] detail`; see [`crate::host`] for why each one is
//! where it is. A refusal exits 2, distinct from an evaluation failure's 1,
//! so a harness can count the two apart without parsing prose.

use nix_eval_driver::host::DriverHost;
use nix_eval_driver::run::{Outcome, Render, Request, evaluate};
use nix_eval_driver::store::LocalStore;
use nix_eval_rs::eval::Settings;
use nix_eval_rs::task::SearchPathEntry;

const USAGE: &str = "\
usage: nix-eval-driver <eval|instantiate> [options] (-E EXPR | FILE)...

commands:
  eval          print the value, as `nix-instantiate --eval --strict` does
  instantiate   print the derivation path, writing the .drv to the store

options:
  -E, --expr EXPR      evaluate EXPR rather than a file (repeatable)
  -A, --attr ATTR      select ATTR from the result before using it
  -I, --include [P=]D  add a search-path entry (repeatable)
      --store-dir DIR  the store directory paths are computed against
                       (default: /nix/store, cppnix's compile-time default)
      --store-root DIR write under DIR/nix/store while still computing paths
                       against --store-dir; cppnix's `--store local?root=DIR`
      --read-only      compute paths and write nothing
      --system SYSTEM  what builtins.currentSystem reports
      --nix-version V  what builtins.nixVersion reports
      --quiet          send no trace or warning output to stderr
  -h, --help           this text

exit status:
  0  every input produced a value
  1  at least one input failed to evaluate
  2  at least one input asked for something this driver does not implement
";

/// Which of the two things to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Command {
    Eval,
    Instantiate,
}

/// One thing to evaluate, as the command line named it.
#[derive(Clone, Debug)]
enum Input {
    File(String),
    Expr(String),
}

#[derive(Debug)]
struct Options {
    command: Command,
    inputs: Vec<Input>,
    store_dir: String,
    store_root: Option<String>,
    read_only: bool,
    attr: Option<String>,
    system: Option<String>,
    nix_version: Option<String>,
    search_path: Vec<SearchPathEntry>,
    quiet: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&args) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{USAGE}");
            return;
        }
        Err(message) => {
            eprintln!("nix-eval-driver: {message}");
            eprint!("{USAGE}");
            std::process::exit(1);
        }
    };
    match run(&options) {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("nix-eval-driver: {message}");
            std::process::exit(1);
        }
    }
}

/// Parse the command line. `Ok(None)` is `--help`, which is not a failure.
///
/// Hand-written and not a dependency: `nix-eval-rs` has none beyond `rnix`
/// and the crate gate builds it offline, so a clap here would be the first
/// thing in this workspace to make the driver's dependency tree wider than
/// the evaluator's -- for a surface of eleven flags.
fn parse(args: &[String]) -> Result<Option<Options>, String> {
    let mut args = args.iter();
    let Some(first) = args.next() else {
        return Err(String::from("no command given"));
    };
    if first == "-h" || first == "--help" {
        return Ok(None);
    }
    let command = match first.as_str() {
        "eval" => Command::Eval,
        "instantiate" => Command::Instantiate,
        other => return Err(format!("unknown command '{other}'")),
    };

    let mut options = Options {
        command,
        inputs: Vec::new(),
        store_dir: String::from("/nix/store"),
        store_root: None,
        // `eval` writes nothing by default and `instantiate` does. The
        // asymmetry is cppnix's: `nix-instantiate --eval` does not write a
        // .drv either, and a default that wrote one would make reading an
        // expression a mutation of the store.
        read_only: command == Command::Eval,
        attr: None,
        system: None,
        nix_version: None,
        search_path: Vec::new(),
        quiet: false,
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-E" | "--expr" => options.inputs.push(Input::Expr(value_of(arg, &mut args)?)),
            "-A" | "--attr" => options.attr = Some(value_of(arg, &mut args)?),
            "-I" | "--include" => options
                .search_path
                .push(search_entry(&value_of(arg, &mut args)?)),
            "--store-dir" => options.store_dir = value_of(arg, &mut args)?,
            "--store-root" => options.store_root = Some(value_of(arg, &mut args)?),
            "--system" => options.system = Some(value_of(arg, &mut args)?),
            "--nix-version" => options.nix_version = Some(value_of(arg, &mut args)?),
            "--read-only" => options.read_only = true,
            "--quiet" => options.quiet = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option '{other}'"));
            }
            other => options.inputs.push(Input::File(other.to_owned())),
        }
    }
    if options.inputs.is_empty() {
        return Err(String::from("nothing to evaluate: pass a file or -E EXPR"));
    }
    // `NIX_PATH` AFTER the `-I` entries, which is the order cppnix consults
    // them: an earlier entry wins, and `-I` is meant to override the
    // environment rather than be overridden by it.
    //
    // Reading it at all is the fix for a wrong answer rather than a missing
    // feature: with `NIX_PATH=foo=/tmp` set, `<foo>` resolved under cppnix and
    // did not here, so `builtins.pathExists <foo>` took the other branch. A
    // refusal would have been survivable; a different boolean is not. Found
    // in review.
    if let Ok(nix_path) = std::env::var("NIX_PATH") {
        for spec in nix_path.split(':').filter(|s| !s.is_empty()) {
            options.search_path.push(search_entry(spec));
        }
    }
    Ok(Some(options))
}

/// The argument after `flag`, or a message naming the flag that is missing
/// one.
///
/// A free function and not a closure: a closure capturing `args` would need
/// it borrowed mutably for the whole `while let`, which is the same borrow
/// the loop itself holds.
fn value_of(flag: &str, args: &mut std::slice::Iter<'_, String>) -> Result<String, String> {
    args.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs an argument"))
}

/// `-I nixpkgs=/path` is a prefixed entry, `-I /path` an unprefixed one.
///
/// The split is on the first `=`, as cppnix's is, so a directory whose own
/// name contains `=` is reachable by giving it an explicit prefix.
fn search_entry(spec: &str) -> SearchPathEntry {
    match spec.split_once('=') {
        Some((prefix, path)) => SearchPathEntry {
            prefix: prefix.to_owned(),
            path: path.to_owned(),
        },
        None => SearchPathEntry {
            prefix: String::new(),
            path: spec.to_owned(),
        },
    }
}

/// Do the work and return the process exit code.
fn run(options: &Options) -> Result<i32, String> {
    let store = LocalStore::open(
        &options.store_dir,
        options.store_root.as_deref().map(std::path::Path::new),
        options.read_only,
    )?;
    // Read before the store moves into the host.
    let store_dir = store.store_dir().to_owned();
    let host = DriverHost::new(store, options.search_path.clone(), options.quiet);

    let settings = Settings {
        // `store_dir` is not optional for this driver the way it is for the
        // crate: `builtins.derivationStrict` refuses outright when the
        // embedder never said, and instantiating is the point of the command.
        // Off the store and not off `options`, so the string hashed into
        // every path is the same string the store writes under. They differed
        // by a trailing slash before -- `LocalStore::open` trims and the
        // settings did not -- so `--store-dir /nix/store/` computed
        // `/nix/store//x-g.drv` and wrote `/nix/store/x-g.drv`. Derivations
        // caught it via the crate's `expected` cross-check; `builtins.toFile`
        // has no such check and would simply have disagreed with cppnix,
        // which canonicalises. Found in review.
        store_dir: Some(store_dir),
        current_system: options.system.clone(),
        nix_version: options.nix_version.clone(),
        home_dir: std::env::var("HOME").ok(),
        // Struct update and not field assignment after `Default::default()`:
        // clippy denies the latter, and it is right to -- the settings this
        // driver does not name are the crate's defaults on purpose, and
        // spelling it this way makes a new field a decision somebody sees
        // rather than one that silently keeps its default.
        // Standalone drivers enable all implemented builtin features by
        // default; production embedding supplies its explicit feature policy.
        ..Settings::default()
    };

    let base = cwd()?;
    let requests: Vec<Request> = options
        .inputs
        .iter()
        .enumerate()
        .map(|(i, input)| request_for(input, i + 1, options, &base))
        .collect::<Result<Vec<Request>, String>>()?;

    let render = match options.command {
        // A drvPath is a string and is wanted bare, the way `nix eval --raw`
        // gives it: a quoted one would need unquoting by every caller,
        // including the parity gate.
        Command::Instantiate => Render::Raw,
        Command::Eval => Render::Strict,
    };
    let outcomes = evaluate(&requests, &settings, &host, render);
    Ok(report(&outcomes))
}

/// Print each outcome and return the exit code the set earns.
///
/// A refusal outranks a plain failure in the code, because a harness reading
/// a 2 knows not to score the run as a divergence, whereas a 1 that was
/// really a refusal would be counted as one.
fn report(outcomes: &[Outcome]) -> i32 {
    let mut code = 0;
    for outcome in outcomes {
        match &outcome.result {
            // Byte-wise, as cppnix writes it: a non-UTF-8 rendering reaches
            // stdout unrepaired (ENG-13147). A failed or short write must
            // not exit 0: truncated bytes with a clean status would read as
            // the answer to any differ downstream.
            Ok(text) => {
                use std::io::Write as _;
                let mut stdout = std::io::stdout().lock();
                if let Err(error) = stdout
                    .write_all(text)
                    .and_then(|()| stdout.write_all(b"\n"))
                {
                    eprintln!("{}: writing stdout: {error}", outcome.label);
                    code = code.max(1);
                }
            }
            Err(failure) => {
                eprintln!("{}: {}", outcome.label, failure.message());
                let earned = if failure.is_unimplemented() { 2 } else { 1 };
                code = code.max(earned);
            }
        }
    }
    code
}

/// Turn one command-line input into a [`Request`].
///
/// `-A attr` is applied by wrapping the source rather than by selecting on
/// the resulting value, so the selection happens inside the evaluation and a
/// lazy sibling attribute is never forced. Selecting afterwards would force
/// the whole set, which is how `nix-instantiate -A` on nixpkgs would become
/// an evaluation of nixpkgs.
fn request_for(
    input: &Input,
    ordinal: usize,
    options: &Options,
    base: &str,
) -> Result<Request, String> {
    let attr = options.attr.as_deref();
    let (label, source, base_dir, from_file) = match input {
        Input::Expr(expr) => (
            // Numbered, because every `-E` otherwise reported as `<expr>` and
            // three of them on one command line produced three
            // indistinguishable stderr lines. Found in review.
            format!("<expr:{ordinal}>"),
            expr.clone(),
            base.to_owned(),
            None::<String>,
        ),
        Input::File(path) => {
            let absolute = absolutise(path, base);
            let parent = std::path::Path::new(&absolute)
                .parent()
                .map_or_else(|| String::from("/"), |p| p.to_string_lossy().into_owned());
            // `import` rather than reading the bytes here: it is the
            // evaluator's own file entry point, so a directory resolves to
            // its `default.nix` and the module cache is used, exactly as an
            // `import` inside a Nix program would be.
            (
                absolute.clone(),
                format!("import {}", path_literal(&absolute)),
                parent,
                Some(absolute),
            )
        }
    };
    let source = match attr {
        // `builtins.getAttr` and not `.attr`, so an attribute whose name is
        // not a bare identifier still works and nothing has to be quoted into
        // the generated text twice.
        Some(attr) => format!("builtins.getAttr {} ({source})", escape_nix_string(attr)),
        None => source,
    };
    let source = match options.command {
        Command::Instantiate => format!("({source}).drvPath"),
        Command::Eval => source,
    };
    Ok(Request {
        label,
        source,
        base_dir,
        from_file,
    })
}

fn cwd() -> Result<String, String> {
    std::env::current_dir()
        .map_err(|e| format!("cannot read the working directory: {e}"))
        .map(|p| p.to_string_lossy().into_owned())
}

fn absolutise(path: &str, base: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), path)
    }
}

/// A Nix path literal for `path`, safe for any bytes a filename can hold.
///
/// `(/. + "…")` and not a bare `/abs/path`, because a path literal is lexed:
/// a space, a quote or a `#` in the name would end the token or start a
/// comment. The `+` form puts the whole name inside a string literal where
/// [`escape_nix_string`] can be relied on, and `/.` is the root, so the
/// result is the same path the literal would have denoted.
fn path_literal(absolute: &str) -> String {
    format!("(/. + {})", escape_nix_string(absolute))
}

fn escape_nix_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // `${` is the only two-character sequence with meaning inside a
            // string; escaping the `$` kills it and leaves a lone `$` alone,
            // which is what cppnix's own printer does.
            '$' => out.push_str("\\$"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Command, Input, Options, escape_nix_string, parse, path_literal, request_for, search_entry,
    };

    fn opts(args: &[&str]) -> Result<Options, String> {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        parse(&args)?.ok_or_else(|| String::from("parsed as --help"))
    }

    #[test]
    fn a_bare_dash_i_matches_every_name_and_a_prefixed_one_does_not() {
        let bare = search_entry("/some/dir");
        assert_eq!(bare.prefix, "");
        assert_eq!(bare.path, "/some/dir");
        let prefixed = search_entry("nixpkgs=/some/dir");
        assert_eq!(prefixed.prefix, "nixpkgs");
        assert_eq!(prefixed.path, "/some/dir");
        // First `=` wins, so a path containing one survives.
        let odd = search_entry("p=/a=b");
        assert_eq!(odd.path, "/a=b");
    }

    /// A filename holding characters the Nix lexer reacts to still names the
    /// same file.
    ///
    /// The generated source is what the evaluator parses, so a name that
    /// terminated the literal early would evaluate a *different* path and
    /// produce a confident wrong answer rather than an error.
    #[test]
    fn a_path_with_awkward_bytes_stays_inside_its_string_literal() -> Result<(), String> {
        // A newline and a backslash as well as the lexer-significant bytes:
        // the previous version of this test asserted `substring 0 0 ... == ""`,
        // which is `""` for EVERY path, so it caught a parse error and nothing
        // else. Deleting the backslash escape left it green while
        // `path_literal` produced a path with a real newline in it. Found in
        // review; now the round-trip is compared against the input.
        let awkward = "/tmp/a b\"c#d${e}/back\\slash/new\nline/f.nix";
        let literal = path_literal(awkward);
        // The comparison happens INSIDE the language rather than against a
        // rendered string, so the assertion does not depend on how the
        // printer chooses to escape what it prints -- Nix writes `\${` where
        // Rust's `{:?}` writes `${`, and comparing rendered text made a
        // correct round-trip look like a failure.
        let source = format!("toString {literal} == {}", escape_nix_string(awkward));
        // Round-trip it through the evaluator: the only judge of whether the
        // escaping worked is the parser this driver hands the text to.
        match nix_eval_rs::eval::eval_str(&source) {
            Ok(rendered) if rendered == "true" => Ok(()),
            Ok(rendered) => Err(format!(
                "{literal} did not round-trip to the original path (got {rendered})"
            )),
            Err(error) => Err(format!("{literal} did not parse: {error:?}")),
        }
    }

    #[test]
    fn eval_defaults_to_read_only_and_instantiate_does_not() -> Result<(), String> {
        let eval = opts(&["eval", "-E", "1"])?;
        if !eval.read_only || eval.command != Command::Eval {
            return Err(format!("eval parsed as {eval:?}"));
        }
        let inst = opts(&["instantiate", "-E", "1"])?;
        if inst.read_only || inst.command != Command::Instantiate {
            return Err(format!("instantiate parsed as {inst:?}"));
        }
        // And the flag still wins when it is given.
        let forced = opts(&["instantiate", "--read-only", "-E", "1"])?;
        if !forced.read_only {
            return Err(String::from("--read-only was ignored"));
        }
        Ok(())
    }

    /// An unknown option is refused, and the message names it.
    ///
    /// `Result` and not an assertion: the workspace denies `clippy::panic`,
    /// and `assert!(false, ..)` -- the obvious way around that -- is denied
    /// too, as `clippy::assertions_on_constants`. Returning the failure is
    /// the spelling that satisfies both.
    #[test]
    fn an_unknown_option_is_refused() -> Result<(), String> {
        let args: Vec<String> = ["eval", "--dry-run", "-E", "1"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        match parse(&args) {
            Err(message) if message.contains("--dry-run") => Ok(()),
            Err(message) => Err(format!("refused, but without naming it: {message}")),
            Ok(other) => Err(format!("--dry-run was accepted: {other:?}")),
        }
    }

    /// `-A` selects inside the evaluation and `instantiate` asks for the
    /// drvPath, so the generated source has both wrappers in the right order.
    #[test]
    fn attr_selection_happens_inside_the_evaluation() -> Result<(), String> {
        let options = opts(&["instantiate", "-A", "hello", "-E", "SET"])?;
        let Some(input) = options.inputs.first() else {
            return Err(String::from("no input parsed"));
        };
        let request = request_for(input, 1, &options, "/base")?;
        if request.source != "(builtins.getAttr \"hello\" (SET)).drvPath" {
            return Err(format!("generated {}", request.source));
        }
        Ok(())
    }

    #[test]
    fn a_file_input_is_imported_from_its_own_directory() -> Result<(), String> {
        let options = opts(&["eval", "sub/dir/thing.nix"])?;
        let Some(input @ Input::File(_)) = options.inputs.first() else {
            return Err(String::from("not parsed as a file"));
        };
        let request = request_for(input, 1, &options, "/base")?;
        if request.base_dir != "/base/sub/dir" {
            return Err(format!("base_dir is {}", request.base_dir));
        }
        if request.from_file.as_deref() != Some("/base/sub/dir/thing.nix") {
            return Err(format!("from_file is {:?}", request.from_file));
        }
        Ok(())
    }

    #[test]
    fn escaping_leaves_ordinary_text_alone() {
        assert_eq!(escape_nix_string("plain"), "\"plain\"");
        assert_eq!(escape_nix_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(escape_nix_string("${x}"), "\"\\${x}\"");
    }
}
