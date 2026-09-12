//! Every primop that leaves through `Host`, and only those.
//!
//! Five kinds, which between them are all of it: the process environment
//! (`getEnv`); the filesystem reads (`import`, `readFile`, `pathExists`,
//! `readDir`, `readFileType`, and `findFile`, which is every `<x>` in the
//! language); the store writes and the coercions that copy a path into the
//! store on their way past (`toFile`, `toJSON`, `path`, `appendContext`, the
//! context-rewriting three, and `derivationStrict`, whose state machine lives
//! in [`crate::drvstrict`]); the fixed-output fetchers (`fetchurl`,
//! `fetchTarball`, `fetchTree`, `fetchGit`); and the two that write to stderr
//! (`trace`, `warn`).
//!
//! Nothing here reaches the world directly. Every one of them suspends with a
//! `NeedPath` or a fetch request and the embedder answers, which is what lets
//! a recording `Host` claim a read set is complete. `builtins::purity_tests`
//! holds both halves of that: `ROUTED_IMPURITIES` names them, and
//! `each_primop_lives_on_its_own_side_of_the_boundary` fails the build if one
//! of them is implemented in [`crate::primops_pure`] instead.
//!
//! Same continuation contract as `primops_pure`: a body is either pure over
//! already-forced arguments or hands back a continuation the machine steps,
//! so nothing here re-enters the interpreter and no walk costs host stack
//! proportional to a Nix value.

use crate::host::FileType;
use crate::primops_pure::{
    Begin, Coerced, Cont, ImportStage, PathPhase as ReadPhase, PathReady, PathStage,
    apply_rewrites, argv, ask, coerce_for_read, coerce_path_with_context, coerce_to_path,
    want_attrs, want_bool, want_list, want_text, want_text_no_ctx,
};
use crate::refusal::{Refusal, RefusalToken};
use crate::task::{
    AcceptedPath, FetchKind, FetchRequest, FetchTreeRequest, FilteredCopy, NeedPath, PathMethod,
    TreeAttr, TreeFetcher, Yield,
};
use crate::value2::{ContextElem, PathValue, Root, Slot, Sym, Value, type_name};
use crate::vm::{Result, Vm, VmError};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

/// A continuation owned by this module. One `Cont` variant carries all of
/// them, so the shared cursor enum in [`crate::primops_pure`] does not grow a
/// case for every host builtin that needs a state machine of its own.
pub enum Ext {
    /// `builtins.toJSON` and `builtins.toXML`: one strict deep walk with two
    /// renderers. Boxed because the driver carries the worklist and the
    /// output buffer, and every `Cont` in the machine would otherwise be as
    /// wide as it. See [`crate::deepwalk`] and
    /// `maintainers/ix/strict-deep-walk.md`.
    DeepWalk(Box<crate::deepwalk::DeepWalk>),
    /// `builtins.derivationStrict`, whose state machine lives in
    /// [`crate::drvstrict`] beside the derivation construction it drives.
    /// Boxed because it is by far the largest of the three and every `Cont`
    /// in the machine would otherwise carry its size.
    DrvStrict(Box<crate::drvstrict::DrvStrict>),
    /// `builtins.appendContext`.
    AppendContext(Box<AppendContext>),
    /// `builtins.storePath`: path coercion without `realisePath`, followed by
    /// preservation of the input string context.
    StorePath(StorePath),
    /// `builtins.findFile`, and so every `<x>` in the language.
    FindFile(Box<FindFile>),
    /// `builtins.convertHash`, whose attribute walk forces `hash`,
    /// `hashAlgo` and `toHashFormat` in cppnix's order and may pause on the
    /// `"base32"` deprecation warning.
    ConvertHash(Box<ConvertHash>),
    /// `builtins.hashFile`: `realisePath` on the second argument, one
    /// contents question, then a pure hash of the answer.
    HashFile(Box<HashFile>),
    /// `builtins.trace`, `builtins.warn` and `builtins.traceVerbose`, which
    /// differ in where the line goes, whether the message may be coerced,
    /// and -- for the third -- whether there is a line at all.
    Emit(Emit),
    /// `builtins.path`. Boxed for the reason `DrvStrict` is: it carries the
    /// walk's stack and its accepted list, and every `Cont` in the machine
    /// would otherwise be as wide as the largest one.
    Path(Box<PathBuiltin>),
    /// `builtins.flakeRefToString`, whose attribute walk forces each value
    /// in the set's order and classifies it with cppnix's two errors before
    /// the embedder prints the reference.
    FlakeRefToString(Box<FlakeRefString>),
    /// `builtins.fetchurl` and `builtins.fetchTarball`, which are cppnix's
    /// one `fetch()` under two names.
    Fetch(Box<FetchBuiltin>),
    /// `builtins.fetchTree` and `builtins.fetchGit`, cppnix's one
    /// `fetchTree()` under two `FetchTreeParams`.
    FetchTree(Box<FetchTreeBuiltin>),
    /// `builtins.getFlake`, which is cppnix's `lockFlake` in the embedder
    /// followed by `callFlake` here.
    GetFlake(GetFlake),
    /// `builtins.wasm`: a WebAssembly guest suspended in a host call, and
    /// the handle table it sees Nix values through. Boxed for the reason
    /// `DrvStrict` is. See [`crate::wasm`].
    Wasm(Box<crate::wasm::WasmCall>),
}

pub fn step(vm: &mut Vm, args: &[Slot], ext: &mut Ext, incoming: Option<Value>) -> Result<Yield> {
    match ext {
        Ext::DeepWalk(w) => w.step(vm, incoming),
        Ext::DrvStrict(d) => d.step(vm, incoming),
        Ext::AppendContext(a) => a.step(vm, incoming),
        Ext::StorePath(s) => s.step(args, incoming),
        Ext::FindFile(f) => f.step(incoming),
        Ext::ConvertHash(c) => c.step(vm, incoming),
        Ext::HashFile(h) => h.step(args, incoming),
        Ext::Emit(e) => e.step(args, incoming),
        Ext::Path(p) => p.step(vm, incoming),
        Ext::Fetch(f) => f.step(vm, incoming),
        Ext::GetFlake(g) => g.step(vm, incoming),
        Ext::FetchTree(f) => f.step(vm, incoming),
        Ext::FlakeRefToString(f) => f.step(vm, incoming),
        Ext::Wasm(w) => w.step(vm, incoming),
    }
}

// -- filesystem reads --------------------------------------------------------

// Each one is a question for the embedder: the body builds a `Cont::Path`,
// the driver coerces the argument to a path and suspends with the matching
// `NeedPath`, and the answer arrives as the continuation resumes. The
// coercion and the cursor are the driver's, in `primops_pure`; what lives
// here is only which question each builtin asks.

pub fn bi_import(_vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Cont(Cont::Import {
        stage: ImportStage::Path(PathStage::Value),
    }))
}

/// `builtins.readFile`.
///
/// **The one string builtin without a settled context verdict (ENG-12478).**
/// cppnix's `prim_readFile` does not merely propagate: when the file being
/// read is itself in the store it queries that path's references and then
/// *rescans the bytes it just read* for the ones that actually occur in them,
/// and the survivors become the result's context (`primops.cc`, `prim_readFile`).
///
/// This crate returns the bytes with no context, which is right for a file
/// outside the store -- every corpus file, and every source file in a normal
/// evaluation -- and under-reports for one inside it. Closing it needs two
/// things `Host` does not expose: the store directory, to know which case a
/// path is in, and a reference scan over the contents.
///
/// It is named here rather than silently returning none because an
/// under-reporting context is exactly the class of wrong answer ENG-12447 was,
/// and `builtins.getContext` can now observe it.
pub fn bi_read_file(_vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    ask(NeedPath::Contents)
}

pub fn bi_path_exists(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    // cppnix decides this on the argument BEFORE coercion -- "SourcePath
    // doesn't know about trailing slash" (`prim_pathExists`,
    // primops.cc:2105) -- so a *string* ending in `/` or `/.` must name a
    // directory, under full symlink resolution, and anything else there is
    // `false`. Only the string shape branches: a path value never carries a
    // trailing slash (the parser normalizes it away), and a set coercing
    // through `__toString` is not a string at the moment cppnix looks.
    // Measured (nix 2.34.7): `pathExists "<file>/"` and `"<file>/."` are
    // false while `pathExists "<dir>/"` and `"<dir>/."` are true; the corpus
    // case is `eval-okay-pathexists`.
    if let Ok(Value::Str(s)) = argv(args, 0) {
        let text = s.bytes();
        if text.ends_with(b"/") || text.ends_with(b"/.") {
            return ask(NeedPath::DirExists);
        }
    }
    ask(NeedPath::Exists)
}

pub fn bi_store_path(_vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Cont(Cont::Ext(Ext::StorePath(StorePath {
        stage: PathStage::Value,
        context: BTreeSet::new(),
        asked: false,
    }))))
}

/// The one path consumer that does not use cppnix's `realisePath`.
///
/// `prim_storePath` calls `coerceToPath`, checks that the result belongs to
/// the store, and copies the coercion context onto the validated result. It
/// never calls `realiseContext`; doing so can build a derivation merely to
/// validate a spelling and also replaces the dependency set with the single
/// returned store object.
pub struct StorePath {
    stage: PathStage,
    context: BTreeSet<ContextElem>,
    asked: bool,
}

impl StorePath {
    fn step(&mut self, args: &[Slot], incoming: Option<Value>) -> Result<Yield> {
        if !self.asked {
            let (coerced, context) = coerce_path_with_context(args, 0, &mut self.stage, incoming)?;
            return match coerced {
                Coerced::Run(y) => Ok(y),
                Coerced::Done(path) => {
                    self.context = context;
                    self.asked = true;
                    Ok(Yield::Need(NeedPath::UseStorePath(path)))
                }
            };
        }

        let answer =
            incoming.ok_or_else(|| VmError::eval("internal: builtins.storePath answer lost"))?;
        let Value::Str(answer) = answer else {
            return Err(VmError::eval(
                "internal: builtins.storePath answer is not a string",
            ));
        };
        self.context.extend(answer.context_set());
        Ok(Yield::Done(Value::Str(
            crate::value2::NixStr::with_context(
                answer.bytes_rc(),
                core::mem::take(&mut self.context),
            ),
        )))
    }
}

pub fn bi_read_dir(_vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    ask(NeedPath::Entries)
}

pub fn bi_read_file_type(_vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    ask(NeedPath::Kind)
}

// -- environment ------------------------------------------------------------

/// cppnix returns the empty string for an unset variable rather than
/// failing, and (under `pure-eval` or `restrict-eval`, neither of which this
/// backend is reachable under) for every variable.
/// Asks the scheduler rather than reading the process environment.
///
/// The reason is a property of the whole evaluator -- that the `Host` trait is
/// every way it reaches the world, which is what lets a recording `Host` claim
/// a read set is complete -- and this function is one of the places that could
/// falsify it. So it is held by `builtins::purity_tests`, which fails the
/// build when an impure builtin is implemented without going through the
/// trait, rather than by this comment. `readset.rs`'s header is where the
/// argument lives.
pub fn bi_get_env(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let name = want_text_no_ctx(&argv(args, 0)?)?;
    Ok(Begin::Cont(Cont::Ask {
        asked: false,
        need: NeedPath::Env(name.to_string()),
    }))
}

// -- toJSON and toXML --------------------------------------------------------

pub fn bi_to_json(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let root = crate::primops_pure::arg(args, 0)?.clone();
    Ok(Begin::Cont(Cont::Ext(Ext::DeepWalk(Box::new(
        crate::deepwalk::DeepWalk::new(root, Box::new(crate::deepwalk::Json::default())),
    )))))
}

/// A JSON string literal, escaped the way nlohmann's `dump()` escapes one
/// with the default `ensure_ascii = false`: the seven named escapes,
/// `\u00xx` for the remaining control characters, everything else
/// (including non-ASCII) verbatim -- after nlohmann's strict UTF-8
/// validation, which is what makes this fallible. `dump()` walks each string
/// through Hoehrmann's DFA and throws `type_error.316` at the first byte
/// where the decode fails (so `C3 78` is rejected at index 1, the `x`, not
/// at the `C3` that led it in), or after the loop for a string that ends
/// mid-sequence; nix wraps the exception as "JSON serialization error: %s"
/// (`value-to-json.cc`). Both messages below are byte-for-byte what
/// nix 2.34.7+ix prints (measured this change, not guessed).
/// Shared with `drvstrict`, which builds the `__json` object out of the same
/// pieces `builtins.toJSON` does.
pub(crate) fn json_string(s: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let type_error = |msg: String| {
        VmError::eval(format!(
            "JSON serialization error: [json.exception.type_error.316] {msg}"
        ))
    };
    out.push(b'"');
    // Hoehrmann-DFA-equivalent scan: `want` counts continuation bytes still
    // owed; `lo..=hi` bounds the next one (E0/ED/F0/F4 narrow their first
    // continuation byte to exclude overlongs and surrogates, exactly the
    // DFA's extra states).
    let mut want = 0u8;
    let (mut lo, mut hi) = (0x80u8, 0xBFu8);
    for (i, &c) in s.iter().enumerate() {
        if want > 0 {
            if !(lo..=hi).contains(&c) {
                return Err(type_error(format!(
                    "invalid UTF-8 byte at index {i}: 0x{c:02X}"
                )));
            }
            want -= 1;
            (lo, hi) = (0x80, 0xBF);
            out.push(c);
            continue;
        }
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0C => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            c if c < 0x20 => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            c if c < 0x80 => out.push(c),
            0xC2..=0xDF => {
                want = 1;
                out.push(c);
            }
            0xE0 => {
                (want, lo) = (2, 0xA0);
                out.push(c);
            }
            0xE1..=0xEC | 0xEE..=0xEF => {
                want = 2;
                out.push(c);
            }
            0xED => {
                (want, hi) = (2, 0x9F);
                out.push(c);
            }
            0xF0 => {
                (want, lo) = (3, 0x90);
                out.push(c);
            }
            0xF1..=0xF3 => {
                want = 3;
                out.push(c);
            }
            0xF4 => {
                (want, hi) = (3, 0x8F);
                out.push(c);
            }
            // 0x80..=0xBF (a stray continuation byte), 0xC0/0xC1 (overlong
            // leads) and 0xF5..=0xFF can never start a sequence.
            c => {
                return Err(type_error(format!(
                    "invalid UTF-8 byte at index {i}: 0x{c:02X}"
                )));
            }
        }
    }
    if want > 0 {
        // Mid-sequence implies non-empty; the claim is checked rather than
        // trusted, and a falsified claim is still an error, never a silent
        // closing quote.
        let Some(&last) = s.last() else {
            return Err(type_error(
                "incomplete UTF-8 string with no last byte (evaluator bug)".to_owned(),
            ));
        };
        return Err(type_error(format!(
            "incomplete UTF-8 string; last byte: 0x{last:02X}"
        )));
    }
    out.push(b'"');
    Ok(())
}

/// nlohmann prints a double with the shortest representation that round
/// trips, which is also what Rust's `Display` does, and always leaves a
/// decimal point behind so the value reads back as a float. The two disagree
/// on magnitudes where nlohmann switches to an exponent and Rust does not;
/// no corpus value reaches that range.
pub(crate) fn json_float(x: f64, out: &mut Vec<u8>) {
    if !x.is_finite() {
        // nlohmann emits null rather than an invalid JSON token.
        out.extend_from_slice(b"null");
        return;
    }
    let s = format!("{x}");
    out.extend_from_slice(s.as_bytes());
    if !s.contains(['.', 'e', 'E']) {
        out.extend_from_slice(b".0");
    }
}

// -- string contexts --------------------------------------------------------

/// `builtins.unsafeDiscardStringContext`: the same bytes, no dependencies.
///
/// cppnix coerces the argument (`context.cc:12`) with the default flags, and
/// its own documentation says "a value that can be coerced to a string": a
/// path is copied into the store first and it is the copy's context that gets
/// discarded, and a set goes through `__toString` or `outPath`. Matching a
/// string and a path here refused the set case (ENG-12628).
pub fn bi_unsafe_discard_string_context(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    recontext(args, Rewrite::DiscardStringContext)
}

// -- output dependencies ----------------------------------------------------

/// Which rewrite a [`Recontext`] performs.
///
/// The first two are inverses (`context.cc:56` and `context.cc:94`); all
/// three share a shape: map the coerced argument's context and hand the same
/// bytes back. The coercion itself is the driver's, declared in the table.
pub enum Rewrite {
    /// `builtins.addDrvOutputDependencies`: the string's single constant
    /// element becomes a dependency on every output of that derivation.
    AddDrvOutputDependencies,
    /// `builtins.unsafeDiscardOutputDependency`: every "derivation deep"
    /// element becomes a plain constant one.
    DiscardOutputDependency,
    /// `builtins.unsafeDiscardStringContext`: no context at all.
    DiscardStringContext,
}

impl Rewrite {
    /// The same bytes under a mapped context, which is all three rewrites.
    ///
    /// The argument arrives coerced: all three positions are
    /// `ArgType::Coerce(CoerceFlags::DEFAULTS)` in the table, which is
    /// `coerceToString` with `coerceMore` off and `copyToStore` on
    /// (`context.cc`, the three `prim_` bodies). So a path has been copied
    /// into the store and it is the copy whose context this rewrites.
    fn apply(self, s: &crate::value2::NixStr) -> Result<crate::value2::NixStr> {
        let context = match self {
            Rewrite::DiscardOutputDependency => discard_output_dependency(s),
            Rewrite::AddDrvOutputDependencies => add_drv_output_dependencies(s)?,
            Rewrite::DiscardStringContext => BTreeSet::new(),
        };
        Ok(s.replacing_context(context))
    }
}

fn recontext(args: &[Slot], rewrite: Rewrite) -> Result<Value> {
    let v = crate::primops_pure::argv(args, 0)?;
    Ok(Value::Str(
        rewrite.apply(crate::primops_pure::want_nix_str(&v)?)?,
    ))
}

fn discard_output_dependency(s: &crate::value2::NixStr) -> BTreeSet<ContextElem> {
    s.context_set()
        .into_iter()
        .map(|e| match e {
            ContextElem::DrvDeep(p) => ContextElem::Opaque(p),
            other => other,
        })
        .collect()
}

fn add_drv_output_dependencies(s: &crate::value2::NixStr) -> Result<BTreeSet<ContextElem>> {
    let context = s.context_set();
    if context.len() != 1 {
        return Err(VmError::eval(format!(
            "context of string '{}' must have exactly one element, but has {}",
            s.lossy(),
            context.len()
        )));
    }
    let elem = context
        .into_iter()
        .next()
        .ok_or_else(|| VmError::eval("internal: a one-element set with no element"))?;
    let out = match elem {
        ContextElem::Opaque(p) => {
            if !crate::storepath::is_derivation(&p) {
                return Err(VmError::eval(format!("path '{p}' is not a derivation")));
            }
            ContextElem::DrvDeep(p)
        }
        // Idempotence is the point: the corpus applies this twice and
        // compares, so a second application must be the identity rather than
        // an error.
        ContextElem::DrvDeep(p) => ContextElem::DrvDeep(p),
        ContextElem::Built { output, .. } => {
            return Err(VmError::eval(format!(
                "`addDrvOutputDependencies` can only act on derivations, not on a \
                 derivation output such as '{output}'"
            )));
        }
    };
    Ok(std::iter::once(out).collect())
}

/// `builtins.addDrvOutputDependencies`, arity 1.
pub fn bi_add_drv_output_dependencies(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    recontext(args, Rewrite::AddDrvOutputDependencies)
}

/// `builtins.unsafeDiscardOutputDependency`, arity 1.
pub fn bi_unsafe_discard_output_dependency(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    recontext(args, Rewrite::DiscardOutputDependency)
}

// -- appendContext ----------------------------------------------------------

/// Where an `appendContext` walk is: what the value the machine is about to
/// hand back means.
enum AppendStage {
    /// The store's answer to `ensurePath` for the current key. Carries
    /// nothing; the key is already known.
    Ensured,
    /// The current key's value, which must be an attribute set.
    Entry,
    /// Its `path` attribute.
    PathFlag,
    /// Its `allOutputs` attribute.
    AllOutputsFlag,
    /// Its `outputs` attribute, which must be a list.
    Outputs,
    /// One element of that list.
    OutputName,
}

/// `builtins.appendContext` in flight.
///
/// The oracle is cppnix's `prim_appendContext` (`context.cc:255`), and the
/// order it does things in per key is mirrored exactly, because it decides
/// which error a malformed argument reports: validate the key as a store
/// path, make it present, *then* force the value, then `path`, `allOutputs`
/// and `outputs` in that order.
pub struct AppendContext {
    /// The first argument. Its own context is where the result's starts, as
    /// cppnix's does -- `forceString` fills the same `context` the loop then
    /// appends to.
    text: crate::value2::NixStr,
    context: BTreeSet<ContextElem>,
    /// Keys still to walk, in reverse order so `pop` yields them forwards.
    pending: Vec<(String, Slot)>,
    /// The key being walked, printed the way cppnix prints a parsed store
    /// path: the store directory and the base name, so a key spelled
    /// `/nix/./store/h-x` records `/nix/store/h-x`.
    key: String,
    entry: Rc<crate::value2::Attrs>,
    outputs: Rc<Vec<Slot>>,
    output_index: usize,
    stage: AppendStage,
}

/// `builtins.appendContext`, arity 2.
pub fn bi_append_context(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    // `forceString`, not `coerceToString`: cppnix takes the first argument as
    // a string and does not coerce it (`context.cc:257`), so a path here is
    // a type error there and must be one here.
    let first = argv(args, 0)?;
    let text = crate::primops_pure::want_nix_str(&first)?.clone();
    let second = argv(args, 1)?;
    let attrs = want_attrs(&second)?;

    // Name order rather than the interner's, which is this crate's business
    // and not the program's. cppnix walks its `Bindings` in symbol-id order,
    // so the two agree on every argument with at most one bad key and can
    // differ on which of several bad keys is reported first; that is the only
    // thing this order is observable through.
    let mut pending: Vec<(String, Slot)> = attrs
        .iter()
        .map(|(k, s)| (vm.sym_name(*k).to_owned(), s.clone()))
        .collect();
    pending.sort_by(|a, b| a.0.cmp(&b.0));
    pending.reverse();

    let context = text.context_set();
    Ok(Begin::Cont(Cont::Ext(Ext::AppendContext(Box::new(
        AppendContext {
            text,
            context,
            pending,
            key: String::new(),
            entry: Rc::new(crate::value2::Attrs::default()),
            outputs: Rc::new(Vec::new()),
            output_index: 0,
            stage: AppendStage::Ensured,
        },
    )))))
}

impl AppendContext {
    fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        let Some(value) = incoming else {
            return self.next_key(vm);
        };
        match self.stage {
            AppendStage::Ensured => {
                self.stage = AppendStage::Entry;
                let slot = self
                    .pending
                    .last()
                    .map(|(_, s)| s.clone())
                    .ok_or_else(|| VmError::eval("internal: appendContext lost its key"))?;
                Ok(Yield::Force(slot))
            }
            AppendStage::Entry => {
                self.pending.pop();
                self.entry = want_attrs(&value)?;
                if let Some(y) = self.ask_flag(vm, "path", AppendStage::PathFlag) {
                    return Ok(y);
                }
                self.after_path(vm)
            }
            AppendStage::PathFlag => {
                if crate::primops_pure::want_bool(&value)? {
                    self.context
                        .insert(ContextElem::Opaque(self.key.as_str().into()));
                }
                self.after_path(vm)
            }
            AppendStage::AllOutputsFlag => {
                if crate::primops_pure::want_bool(&value)? {
                    if !crate::storepath::is_derivation(&self.key) {
                        return Err(VmError::eval(format!(
                            "tried to add all-outputs context of {}, which is not a \
                             derivation, to a string",
                            self.key
                        )));
                    }
                    self.context
                        .insert(ContextElem::DrvDeep(self.key.as_str().into()));
                }
                self.after_all_outputs(vm)
            }
            AppendStage::Outputs => {
                self.outputs = want_list(&value)?;
                if !self.outputs.is_empty() && !crate::storepath::is_derivation(&self.key) {
                    return Err(VmError::eval(format!(
                        "tried to add derivation output context of {}, which is not a \
                         derivation, to a string",
                        self.key
                    )));
                }
                self.output_index = 0;
                self.next_output(vm)
            }
            AppendStage::OutputName => {
                let name = crate::primops_pure::want_text_no_ctx(&value)?;
                self.context.insert(ContextElem::Built {
                    drv: self.key.as_str().into(),
                    output: name.as_str().into(),
                });
                self.output_index += 1;
                self.next_output(vm)
            }
        }
    }

    /// Force `name`'s value when the entry has one. `None` means the entry
    /// does not have it, which cppnix treats as absent rather than as false,
    /// and the caller moves on to the next attribute.
    fn ask_flag(&mut self, vm: &mut Vm, name: &str, stage: AppendStage) -> Option<Yield> {
        let sym = vm.intern(name);
        let slot = self.entry.get(&sym).cloned()?;
        self.stage = stage;
        Some(Yield::Force(slot))
    }

    fn after_path(&mut self, vm: &mut Vm) -> Result<Yield> {
        if let Some(y) = self.ask_flag(vm, "allOutputs", AppendStage::AllOutputsFlag) {
            return Ok(y);
        }
        self.after_all_outputs(vm)
    }

    fn after_all_outputs(&mut self, vm: &mut Vm) -> Result<Yield> {
        if let Some(y) = self.ask_flag(vm, "outputs", AppendStage::Outputs) {
            return Ok(y);
        }
        self.next_key(vm)
    }

    fn next_output(&mut self, vm: &Vm) -> Result<Yield> {
        match self.outputs.get(self.output_index) {
            Some(slot) => {
                self.stage = AppendStage::OutputName;
                Ok(Yield::Force(slot.clone()))
            }
            None => {
                self.outputs = Rc::new(Vec::new());
                self.next_key(vm)
            }
        }
    }

    /// Validate the next key and ask the store to make it present, or finish.
    fn next_key(&mut self, vm: &Vm) -> Result<Yield> {
        let Some((name, _)) = self.pending.last() else {
            return Ok(Yield::Done(Value::Str(
                self.text
                    .replacing_context(std::mem::take(&mut self.context)),
            )));
        };
        // The store directory is configuration the embedder hands over, and
        // without it "is this a store path" has no answer -- so the question
        // is refused by name rather than answered against a guessed
        // `/nix/store`, which would accept keys cppnix rejects.
        let store_dir = vm.settings().store_dir.clone().ok_or_else(|| {
            VmError::Unimplemented(Refusal::new(
                RefusalToken::StoreUnavailable,
                "builtins.appendContext without a store directory (the embedder has not \
                 called ixe_set_store_dir)",
            ))
        })?;
        let base = crate::storepath::parse_store_path(&store_dir, name)
            .ok_or_else(|| VmError::eval(format!("context key '{name}' is not a store path")))?;
        self.key = format!("{store_dir}/{base}");
        self.stage = AppendStage::Ensured;
        Ok(Yield::Need(NeedPath::EnsurePath(self.key.clone())))
    }
}

// -- trace and warn ---------------------------------------------------------

/// Where an emitted line goes, and how its message is obtained.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sink {
    /// `builtins.trace`: `printError("trace: %1%", ...)`, and a non-string
    /// message is *printed* rather than rejected (`primops.cc:1321`).
    Trace,
    /// `builtins.warn`: `lvlWarn`, and the message must already be a string --
    /// cppnix rejects a non-string deliberately, so that a later version can
    /// give the second argument a meaning (`primops.cc:1354`).
    Warn,
}

/// Which value the machine is about to be handed back.
#[derive(Clone, Copy)]
enum EmitStage {
    /// Argument 0, which nobody has forced yet. Only `traceVerbose` starts
    /// here: its table entry forces nothing, because with tracing off cppnix
    /// swaps in `prim_second`, which never touches the message.
    Message,
    /// The message, after the printer rendered it.
    Printed,
    /// Nothing: the line has been emitted -- or, for `traceVerbose` with
    /// tracing off, there was never a line to emit. Entering here *is*
    /// cppnix's `prim_second` (`primops.cc:1408`): force the second argument
    /// and return it, having looked at nothing else.
    Emitted,
    /// The warning has been emitted and `abort-on-warn` is set, so the next
    /// step is the failure rather than the second argument.
    Aborting,
    /// The second argument, forced.
    Value,
}

/// `builtins.trace` and `builtins.warn`, arity 2.
///
/// The order is cppnix's and it is the whole reason this is a state machine
/// rather than two lines: the line is emitted **before** the second argument
/// is forced, so `builtins.trace "a" (throw "b")` prints the trace and then
/// throws, in that order. Forcing both arguments up front -- which is what
/// the machine does by default -- would swallow the trace on every expression
/// that fails, which is exactly the case someone put the trace there for.
pub struct Emit {
    sink: Sink,
    stage: EmitStage,
    /// `abort-on-warn` as it stood when this machine was built.
    ///
    /// A field and not a read at emit time, because the machine has no `Vm`
    /// there and because a setting that changed mid-emission would make
    /// `builtins.warn` return a value on one run of the same expression and
    /// kill it on another (ENG-12939).
    abort_on_warn: bool,
}

pub fn bi_trace(vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Cont(Cont::Ext(Ext::Emit(Emit {
        sink: Sink::Trace,
        stage: EmitStage::Printed,
        abort_on_warn: vm.settings().abort_on_warn,
    }))))
}

pub fn bi_warn(vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Cont(Cont::Ext(Ext::Emit(Emit {
        sink: Sink::Warn,
        stage: EmitStage::Printed,
        abort_on_warn: vm.settings().abort_on_warn,
    }))))
}

/// `builtins.traceVerbose`, arity 2: `builtins.trace` when `trace-verbose` is
/// on and cppnix's `prim_second` when it is off.
///
/// The setting is read here, once per call, rather than baked into the
/// builtin table. cppnix reads it once per `EvalState` because it chooses the
/// function pointer at `createBaseEnv` time; this crate has one static table
/// shared by every evaluation in the process, so the same table entry has to
/// serve both. The observable difference is nil -- `eval::Settings` carries
/// `trace_verbose` into the memo key, so two evaluations that disagree about
/// it cannot share a result.
///
/// Which arm runs is not cosmetic: `prim_second` does not force argument 0,
/// so `builtins.traceVerbose (throw "x") 1` answers `1` with the setting off.
pub fn bi_trace_verbose(vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Cont(Cont::Ext(Ext::Emit(Emit {
        sink: Sink::Trace,
        stage: if vm.settings().trace_verbose {
            EmitStage::Message
        } else {
            EmitStage::Emitted
        },
        abort_on_warn: vm.settings().abort_on_warn,
    }))))
}

impl Emit {
    fn step(&mut self, args: &[Slot], incoming: Option<Value>) -> Result<Yield> {
        match (self.stage, incoming) {
            // `traceVerbose` with tracing on: nobody has forced the message.
            (EmitStage::Message, None) => Ok(Yield::Force(
                args.first()
                    .ok_or_else(|| VmError::eval("internal: trace lost its message"))?
                    .clone(),
            )),
            (EmitStage::Message, Some(message)) => self.begin(message),
            // First entry for `trace` and `warn`: argument 0 is forced (the
            // machine did it), argument 1 is not (it is in the builtin's lazy
            // list).
            (EmitStage::Printed, None) => {
                let message = argv(args, 0)?;
                self.begin(message)
            }
            (EmitStage::Printed, Some(printed)) => {
                self.emit(crate::primops_pure::want_text(&printed)?)
            }
            // Nothing left to emit; hand back the second argument, which is
            // the whole of `prim_second` when this is where the machine
            // started.
            (EmitStage::Emitted, _) => {
                self.stage = EmitStage::Value;
                let slot = args
                    .get(1)
                    .ok_or_else(|| VmError::eval("internal: trace lost its value"))?;
                Ok(Yield::Force(slot.clone()))
            }
            // cppnix warns first and dies second (`primops.cc:1366` then
            // `:1369`), so the line is on stderr before this fires. Ordering
            // it the other way round would lose the warning in exactly the
            // configuration somebody turned the setting on to read.
            //
            // Not catchable, and cppnix says so at its throw site: it raises
            // `EvalBaseError` rather than an `EvalError` precisely so the
            // failure is not stored in the eval cache. `VmError::eval` is
            // this crate's uncatchable kind, which `tryEval` does not
            // swallow.
            (EmitStage::Aborting, _) => Err(VmError::eval(
                "aborting to reveal stack trace of warning, as abort-on-warn is set",
            )),
            (EmitStage::Value, Some(value)) => Ok(Yield::Done(value)),
            (EmitStage::Value, None) => Err(VmError::eval(
                "internal: trace resumed with no value to return",
            )),
        }
    }

    /// Decide what to do with a forced message: emit it, refuse it, or hand
    /// it to the printer first.
    fn begin(&mut self, message: Value) -> Result<Yield> {
        match (&message, self.sink) {
            (Value::Str(s), _) => {
                let text = crate::primops_pure::text_of(s)?.to_owned();
                self.emit(text)
            }
            // cppnix names the coercion it refuses, and refuses only for
            // `warn`.
            (other, Sink::Warn) => Err(VmError::eval(format!(
                "expected a string but found {}: {other}",
                type_name(other)
            ))),
            // `ValuePrinter`, which is `nix eval`'s dialect and the one
            // `prim_trace` hands the value to, not nix-instantiate's.
            (_, Sink::Trace) => {
                self.stage = EmitStage::Printed;
                Ok(Yield::sub(crate::task::Task::Print(
                    crate::print::Print::value_printer(message.clone()),
                )))
            }
        }
    }

    fn emit(&mut self, message: String) -> Result<Yield> {
        match self.sink {
            Sink::Trace => {
                self.stage = EmitStage::Emitted;
                Ok(Yield::Need(NeedPath::Trace(message)))
            }
            Sink::Warn => {
                self.stage = if self.abort_on_warn {
                    EmitStage::Aborting
                } else {
                    EmitStage::Emitted
                };
                Ok(Yield::Need(NeedPath::Warn(message)))
            }
        }
    }
}

// -- convertHash and hashFile -------------------------------------------------

/// Which forced value [`ConvertHash::step`] is about to be handed back.
#[derive(Clone, Copy)]
enum ConvertStage {
    Hash,
    Algo,
    Format,
    /// The `"base32"` deprecation warning is out; the parsed format is in
    /// `format` and the next step finishes.
    Warned,
}

/// `builtins.convertHash` (`primops.cc`, `prim_convertHash`): locate and
/// force `hash`, then `hashAlgo` if present, and only then locate
/// `toHashFormat` -- the order matters because it decides which missing
/// attribute an incomplete call reports.
pub struct ConvertHash {
    entry: Rc<crate::value2::Attrs>,
    stage: ConvertStage,
    hash: String,
    algo: Option<crate::nixhash::HashAlgo>,
    format: Option<crate::nixhash::HashFormat>,
    blake3_hashes: bool,
}

pub fn bi_convert_hash(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let entry = want_attrs(&argv(args, 0)?)?;
    Ok(Begin::Cont(Cont::Ext(Ext::ConvertHash(Box::new(
        ConvertHash {
            entry,
            stage: ConvertStage::Hash,
            hash: String::new(),
            algo: None,
            format: None,
            blake3_hashes: vm.settings().blake3_hashes,
        },
    )))))
}

impl ConvertHash {
    fn attr(&self, vm: &mut Vm, name: &str) -> Option<Slot> {
        let sym = vm.intern(name);
        self.entry.get(&sym).cloned()
    }

    fn require(&self, vm: &mut Vm, name: &str) -> Result<Slot> {
        self.attr(vm, name)
            .ok_or_else(|| VmError::eval(format!("attribute '{name}' missing")))
    }

    fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        let Some(value) = incoming else {
            let slot = self.require(vm, "hash")?;
            return Ok(Yield::Force(slot));
        };
        match self.stage {
            ConvertStage::Hash => {
                self.hash = want_text_no_ctx(&value)?;
                if let Some(slot) = self.attr(vm, "hashAlgo") {
                    self.stage = ConvertStage::Algo;
                    return Ok(Yield::Force(slot));
                }
                self.ask_format(vm)
            }
            ConvertStage::Algo => {
                let name = want_text_no_ctx(&value)?;
                self.algo = Some(
                    crate::nixhash::parse_algo(&name)
                        .and_then(|algo| algo.require_enabled(self.blake3_hashes))
                        .map_err(|e| VmError::eval(e.to_string()))?,
                );
                self.ask_format(vm)
            }
            ConvertStage::Format => {
                let name = want_text_no_ctx(&value)?;
                let (format, warning) = crate::nixhash::parse_hash_format(&name)
                    .map_err(|e| VmError::eval(e.to_string()))?;
                self.format = Some(format);
                if let Some(message) = warning {
                    // The same shape as `builtins.path`'s empty-hash warning:
                    // the message goes out as a question and the answer only
                    // resumes the machine.
                    self.stage = ConvertStage::Warned;
                    return Ok(Yield::Need(NeedPath::Warn(message)));
                }
                self.finish()
            }
            ConvertStage::Warned => self.finish(),
        }
    }

    fn ask_format(&mut self, vm: &mut Vm) -> Result<Yield> {
        let slot = self.require(vm, "toHashFormat")?;
        self.stage = ConvertStage::Format;
        Ok(Yield::Force(slot))
    }

    fn finish(&mut self) -> Result<Yield> {
        let format = self
            .format
            .ok_or_else(|| VmError::eval("internal: convertHash finished without a format"))?;
        let hash = crate::nixhash::parse_any(&self.hash, self.algo, self.blake3_hashes)
            .map_err(|e| VmError::eval(e.to_string()))?;
        // `to_string(hf, hf == HashFormat::SRI)`: only SRI carries the
        // algorithm, and it always does.
        let rendered = hash.to_format(format, format == crate::nixhash::HashFormat::Sri);
        Ok(Yield::Done(Value::Str(rendered.as_str().into())))
    }
}

/// `builtins.hashFile` (`primops.cc`, `prim_hashFile`): the algorithm is
/// validated before the path argument is looked at, then the path goes
/// through the same coercion and realisation every read builtin uses, and
/// the digest comes back as the answer, hashed from the raw bytes on the
/// answering side (ENG-13146), with no context on the result -- cppnix
/// renders `hashString(...)` into a fresh string.
pub struct HashFile {
    algo: crate::nixhash::HashAlgo,
    phase: ReadPhase,
    /// Whether the path argument has been forced yet. It is not in the
    /// builtin's strict list, because cppnix validates the algorithm before
    /// it touches the path (`prim_hashFile`), so a refused algorithm must
    /// never force the path; this body forces it itself, after that.
    path_forced: bool,
}

pub fn bi_hash_file(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let algo = want_text_no_ctx(&argv(args, 0)?)?;
    let algo = crate::primops_pure::parse_algo_name(&algo, vm.settings().blake3_hashes)?;
    Ok(Begin::Cont(Cont::Ext(Ext::HashFile(Box::new(HashFile {
        algo,
        phase: ReadPhase::Coerce(PathStage::Value),
        path_forced: false,
    })))))
}

impl HashFile {
    fn step(&mut self, args: &[Slot], incoming: Option<Value>) -> Result<Yield> {
        if !self.path_forced {
            self.path_forced = true;
            let slot = args
                .get(1)
                .cloned()
                .ok_or_else(|| VmError::eval("builtins.hashFile: missing path argument"))?;
            return Ok(Yield::Force(slot));
        }
        match &mut self.phase {
            ReadPhase::Coerce(stage) => match coerce_for_read(args, 1, stage, incoming)? {
                PathReady::Run(y) => Ok(y),
                PathReady::Realise(p, context) => {
                    self.phase = ReadPhase::Realising(p);
                    Ok(Yield::Need(NeedPath::Realise(context)))
                }
                PathReady::Ready(p) => {
                    self.phase = ReadPhase::Asked;
                    Ok(Yield::Need(NeedPath::HashFile {
                        path: p,
                        algo: self.algo,
                    }))
                }
            },
            ReadPhase::Realising(path) => {
                let p = apply_rewrites(Rc::clone(path), incoming)?;
                self.phase = ReadPhase::Asked;
                Ok(Yield::Need(NeedPath::HashFile {
                    path: p,
                    algo: self.algo,
                }))
            }
            ReadPhase::Asked => {
                // The answer is already the digest, hashed from the raw
                // bytes on the answering side: hashing here would need the
                // contents as a string, which repairs invalid UTF-8 and
                // digests bytes the file does not have (ENG-13146).
                let value =
                    incoming.ok_or_else(|| VmError::eval("internal: hashFile answer lost"))?;
                let digest = want_text(&value)?;
                Ok(Yield::Done(Value::Str(digest.as_str().into())))
            }
        }
    }
}

// -- findFile ---------------------------------------------------------------

/// Where a `findFile` walk is: which forced value the machine is about to be
/// handed back.
#[derive(Clone, Copy)]
enum FindStage {
    /// One element of the list, expected to be an attribute set.
    Elem,
    /// That element's `prefix`.
    Prefix,
    /// That element's `path`, before coercion.
    PathValue,
    /// The same `path` after a coercion that needed the machine.
    PathCoerced,
    /// This entry's context has left as a [`NeedPath::Realise`]; the next
    /// value is the rewrite map, which gets applied to `pending`.
    PathRealising,
    /// The lookup itself has left through `Host`; the next value is its
    /// answer.
    Asked,
}

/// `builtins.findFile`, arity 2, and with it every `<x>` in the language:
/// cppnix's parser desugars a lookup path into `__findFile __nixPath "x"`
/// (`primops.cc:2242`), so both spellings run this.
///
/// The order of operations is `prim_findFile`'s (`primops.cc:2243`), because
/// it decides which error a malformed argument reports: force the list, then
/// per element force the set, read `prefix` (absent is the empty string),
/// read `path` and coerce it *without* copying to the store, and only when
/// the whole list is built ask for the file.
///
/// The resolution itself is not here and is not going to be: see
/// [`crate::host::Host::find_file`].
pub struct FindFile {
    items: Rc<Vec<Slot>>,
    i: usize,
    entries: Vec<crate::task::SearchPathEntry>,
    cur: Option<Rc<crate::value2::Attrs>>,
    prefix: String,
    name: String,
    stage: FindStage,
    /// The entry path whose context is out being realised. A field rather
    /// than a payload on [`FindStage`], which is `Copy` and stays that way.
    pending: String,
    path_sym: Sym,
    prefix_sym: Sym,
}

pub fn bi_find_file(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let items = want_list(&argv(args, 0)?)?;
    // `forceStringNoCtx`, as cppnix takes the second argument
    // (`primops.cc:2291`): a context here would name a store path the lookup
    // would have to realise, and cppnix does not do that for this argument.
    let name = want_text_no_ctx(&argv(args, 1)?)?;
    let path_sym = vm.intern("path");
    let prefix_sym = vm.intern("prefix");
    Ok(Begin::Cont(Cont::Ext(Ext::FindFile(Box::new(FindFile {
        items,
        i: 0,
        entries: Vec::new(),
        cur: None,
        prefix: String::new(),
        name,
        stage: FindStage::Elem,
        pending: String::new(),
        path_sym,
        prefix_sym,
    })))))
}

impl FindFile {
    fn step(&mut self, incoming: Option<Value>) -> Result<Yield> {
        let Some(value) = incoming else {
            return self.next_elem();
        };
        match self.stage {
            FindStage::Elem => {
                let attrs = want_attrs(&value)?;
                let prefix_slot = attrs.get(&self.prefix_sym).cloned();
                self.cur = Some(attrs);
                match prefix_slot {
                    Some(slot) => {
                        self.stage = FindStage::Prefix;
                        Ok(Yield::Force(slot))
                    }
                    // cppnix leaves `prefix` empty when the attribute is
                    // absent (`primops.cc:2255`) rather than reporting a
                    // missing attribute, which is what makes
                    // `[ { path = ./dir; } ]` a usable search path.
                    None => {
                        self.prefix.clear();
                        self.ask_path()
                    }
                }
            }
            FindStage::Prefix => {
                self.prefix = want_text_no_ctx(&value)?;
                self.ask_path()
            }
            FindStage::PathValue => match value {
                Value::Path(p) => self.push_entry(p.to_string()),
                Value::Str(s) => {
                    // A context here is a store path the entry depends on,
                    // which cppnix realises before using it and then rewrites
                    // the entry with (`primops.cc:2275`). So a search path may
                    // name a derivation output, and resolving `<x>` against it
                    // builds that derivation.
                    let text = crate::primops_pure::text_of(&s)?.to_owned();
                    self.settle_path(text, s.context_set())
                }
                // cppnix coerces the `path` attribute with `false, false`
                // (`primops.cc`, prim_findFile), so a set reaches
                // `__toString` or `outPath`, a path is not copied into the
                // store, and an integer is an error. The coercion is a call,
                // so it needs the machine.
                //
                // The flags are passed rather than picked from a named
                // constructor: this used to call `Task::coerce`, whose flags
                // are `builtins.toString`'s, so `coerceMore` was on and a set
                // whose `outPath` is a list coerced here where cppnix raises
                // a type error. Found by the class gate in
                // `tests/coercion_class.rs`, where this site is declared
                // (ENG-12854).
                Value::Attrs(_) => {
                    self.stage = FindStage::PathCoerced;
                    Ok(Yield::sub(crate::task::Task::coerce_as_primop(
                        Slot::value(value),
                        crate::print::CoerceFlags::NEITHER,
                    )))
                }
                other => Err(VmError::eval(format!(
                    "cannot coerce {} to a string",
                    type_name(&other)
                ))),
            },
            FindStage::PathCoerced => {
                // The context of a coerced set is whatever `__toString` or
                // `outPath` accumulated, and cppnix realises it here exactly
                // as it does the string case: one `NixStringContext` is
                // filled by `coerceToString` and realised after, whichever
                // shape filled it (`primops.cc:2264`).
                let context = crate::value2::context_of(&value);
                let text = want_text(&value)?;
                self.settle_path(text, context)
            }
            FindStage::PathRealising => {
                let path = crate::primops_pure::rewrite_spelling(
                    core::mem::take(&mut self.pending),
                    Some(value),
                )?;
                self.push_entry(path)
            }
            FindStage::Asked => Ok(Yield::Done(value)),
        }
    }

    /// Force this element's `path`. Missing is an error here, unlike
    /// `prefix`: cppnix uses `getAttr`, which reports it (`primops.cc:2260`).
    fn ask_path(&mut self) -> Result<Yield> {
        let attrs = self
            .cur
            .as_ref()
            .ok_or_else(|| VmError::eval("internal: findFile lost its element"))?;
        let slot = attrs
            .get(&self.path_sym)
            .cloned()
            .ok_or_else(|| VmError::eval("attribute 'path' missing"))?;
        self.stage = FindStage::PathValue;
        Ok(Yield::Force(slot))
    }

    /// cppnix's `realiseContext` then `rewriteStrings` on one entry
    /// (`primops.cc:2275`), or straight through when there is nothing to
    /// realise.
    ///
    /// cppnix wraps just this in a `try` and reports an invalid path as
    /// "cannot find '%1%', since path '%2%' is not valid". This backend lets
    /// the embedder's message through instead, which names the same path in
    /// different words: error wording is tier 2 here, and re-wrapping would
    /// mean the evaluator second-guessing which of `realiseContext`'s several
    /// failures it was looking at from a string.
    fn settle_path(
        &mut self,
        path: String,
        context: std::collections::BTreeSet<crate::value2::ContextElem>,
    ) -> Result<Yield> {
        if context.is_empty() {
            return self.push_entry(path);
        }
        self.pending = path;
        self.stage = FindStage::PathRealising;
        Ok(Yield::Need(NeedPath::Realise(
            context.into_iter().collect(),
        )))
    }

    fn push_entry(&mut self, path: String) -> Result<Yield> {
        self.entries.push(crate::task::SearchPathEntry {
            prefix: std::mem::take(&mut self.prefix),
            path,
        });
        self.i += 1;
        self.cur = None;
        self.stage = FindStage::Elem;
        self.next_elem()
    }

    fn next_elem(&mut self) -> Result<Yield> {
        match self.items.get(self.i) {
            Some(slot) => {
                self.stage = FindStage::Elem;
                Ok(Yield::Force(slot.clone()))
            }
            None => {
                self.stage = FindStage::Asked;
                Ok(Yield::Need(NeedPath::FindFile {
                    entries: std::mem::take(&mut self.entries),
                    name: std::mem::take(&mut self.name),
                }))
            }
        }
    }
}

/// The one request in `asked`, or a refusal naming what was there instead.
/// `what` describes the request for the message.
///
/// `unreachable!` rather than an index, because `indexing_slicing`,
/// `unwrap_used`, `expect_used` and `panic` are all denied here. One copy for
/// the three test modules below that each had their own.
#[cfg(test)]
fn only_request<T: Clone + std::fmt::Debug>(what: &str, asked: &[T]) -> T {
    let [request] = asked else {
        unreachable!(
            "expected exactly one {what}, got {}: {asked:?}",
            asked.len()
        )
    };
    request.clone()
}

#[cfg(test)]
mod tests {
    use crate::eval::render_str as render;

    #[test]
    fn generic_closure_closes_over_keys_breadth_first() {
        assert_eq!(
            render(
                "builtins.genericClosure {
                   startSet = [ { key = 1; } ];
                   operator = x: if x.key >= 4 then [ ] else [ { key = x.key + 1; } ];
                 }"
            ),
            "[ { key = 1; } { key = 2; } { key = 3; } { key = 4; } ]"
        );
        // A key already closed over is skipped and its operator never runs,
        // which is what stops a cyclic graph from diverging.
        assert_eq!(
            render(
                "builtins.genericClosure {
                   startSet = [ { key = 1; } { key = 1; } ];
                   operator = x: [ { key = 1; } ];
                 }"
            ),
            "[ { key = 1; } ]"
        );
    }

    #[test]
    fn generic_closure_argument_errors_match_cpp_classes() {
        // An empty startSet returns before `operator` is looked at, so a
        // missing operator is not an error there.
        assert_eq!(render("builtins.genericClosure { startSet = [ ]; }"), "[ ]");
        assert_eq!(
            render("builtins.genericClosure { operator = x: [ ]; }"),
            "Eval(Eval, \"attribute 'startSet' missing\")"
        );
        assert_eq!(
            render("builtins.genericClosure { startSet = [ { key = 1; } ]; }"),
            "Eval(Eval, \"attribute 'operator' missing\")"
        );
        assert_eq!(
            render("builtins.genericClosure { startSet = [ { nokey = 1; } ]; operator = x: [ ]; }"),
            "Eval(Eval, \"attribute 'key' missing\")"
        );
        assert_eq!(
            render(
                "builtins.genericClosure {
                   startSet = [ { key = 1; } { key = \"s\"; } ];
                   operator = x: [ ];
                 }"
            ),
            "Eval(Eval, \"cannot compare a string with an integer\")"
        );
        // Two set-valued keys: the first insert compares against nothing and
        // succeeds, exactly as std::map's does, so the failure needs two.
        assert_eq!(
            render(
                "builtins.genericClosure {
                   startSet = [ { key = { }; } { key = { }; } ];
                   operator = x: [ ];
                 }"
            ),
            "Eval(Eval, \"cannot compare a set with a set; values of that type are incomparable\")"
        );
    }

    #[test]
    fn get_env_reads_the_environment_and_defaults_to_empty() {
        // Set rather than assumed: the corpus runner exports TEST_VAR=foo,
        // and a unit test has no such runner.
        // SAFETY: single-threaded test, no other thread reads the
        // environment concurrently.
        unsafe { std::env::set_var("IXE_TEST_VAR", "foo") };
        assert_eq!(render("builtins.getEnv \"IXE_TEST_VAR\""), "\"foo\"");
        assert_eq!(render("builtins.getEnv \"IXE_NO_SUCH_VAR\""), "\"\"");
    }

    #[test]
    fn zip_attrs_with_groups_by_name_and_stays_lazy() {
        assert_eq!(
            render(
                "builtins.zipAttrsWith (n: vs: { inherit n vs; }) [ { a = 1; b = 2; } { a = 3; } ]"
            ),
            "{ a = { n = \"a\"; vs = [ 1 3 ]; }; b = { n = \"b\"; vs = [ 2 ]; }; }"
        );
        // cppnix builds each entry as an unapplied `f name values`, so an
        // entry nobody reads never calls `f`.
        assert_eq!(
            render(
                "(builtins.zipAttrsWith (n: v: throw n) [ { a = 1; b = 2; } ]).a or \"untouched\""
            ),
            "Eval(Thrown, \"a\")"
        );
        assert_eq!(
            render(
                "builtins.attrNames (builtins.zipAttrsWith (n: v: throw n) [ { a = 1; b = 2; } ])"
            ),
            "[ \"a\" \"b\" ]"
        );
    }

    /// Every quoted result below is this fork's cpp arm verbatim
    /// (nix-instantiate --eval --strict, clean config).
    #[test]
    fn convert_hash_matches_cppnix() {
        const H16: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let with = |fmt: &str| {
            render(&format!(
                "builtins.convertHash {{ hash = \"{H16}\"; hashAlgo = \"sha256\"; toHashFormat = \"{fmt}\"; }}"
            ))
        };
        assert_eq!(
            with("sri"),
            "\"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\""
        );
        assert_eq!(with("base16"), format!("\"{H16}\""));
        assert_eq!(
            with("nix32"),
            "\"0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73\""
        );
        assert_eq!(
            with("base64"),
            "\"47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\""
        );
        // The deprecated alias parses as nix32 and warns; the default test
        // host swallows the warning and the value is the nix32 rendering.
        assert_eq!(
            with("base32"),
            "\"0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73\""
        );
        // SRI input and the algo:hash prefix both carry their own algorithm.
        assert_eq!(
            render(
                "builtins.convertHash { hash = \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\"; toHashFormat = \"base16\"; }"
            ),
            format!("\"{H16}\"")
        );
        assert_eq!(
            render(&format!(
                "builtins.convertHash {{ hash = \"sha256:{H16}\"; toHashFormat = \"sri\"; }}"
            )),
            "\"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\""
        );
    }

    /// Every builtin surface that accepts Blake3 must reject it when the
    /// experimental feature is disabled. These are separate rows so removing
    /// any one surface's gate makes that row fail even while the others still
    /// reject it. `hashString` and `hashFile` preserve their named-refusal
    /// contract; `convertHash` preserves its evaluation-error contract. Its
    /// final row is the in-band `blake3:<hex>` parser route rather than the
    /// `hashAlgo` attribute route immediately above it.
    #[test]
    fn blake3_hash_builtins_require_the_feature_when_disabled() {
        for (label, expr) in [
            ("hashString", r#"builtins.hashString "blake3" "abc""#),
            (
                "hashFile",
                r#"builtins.hashFile "blake3" (throw "the path must stay unforced")"#,
            ),
        ] {
            let got = render(expr);
            assert!(
                got.contains("Unimplemented")
                    && got.contains("UnimplementedBuiltin")
                    && got.contains("blake3 hashes"),
                "{label} should refuse disabled Blake3 by name, got {got}"
            );
        }

        for (label, expr) in [
            (
                "convertHash hashAlgo",
                r#"builtins.convertHash {
                    hash = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
                    hashAlgo = "blake3";
                    toHashFormat = "sri";
                }"#,
            ),
            (
                "convertHash in-band prefix",
                r#"builtins.convertHash {
                    hash = "blake3:6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
                    toHashFormat = "sri";
                }"#,
            ),
            // The two above would still refuse with the early gate in
            // `ConvertStage::Algo` deleted, because `nixhash` rejects the
            // algorithm again further in. What only the early gate protects is
            // the ORDER: cppnix refuses the algorithm before it forces
            // `toHashFormat`, so a caller whose format argument throws sees the
            // Blake3 refusal and not its own throw. The case below pins that,
            // and it goes red if the early gate is removed -- which is the
            // property the pair above lacks. Same shape as the `hashFile` case just
            // above, which keeps its path unforced for the same reason.
            //
            // Only the `hashAlgo` spelling can be pinned this way. With no
            // `hashAlgo` attribute, `ConvertStage::Hash` goes straight to
            // `ask_format`, so the in-band `blake3:` prefix is not read until
            // after the format has been forced -- there is no ordering there to
            // protect. Whether cppnix forces in the same order on that route is
            // not established here.
            (
                "convertHash hashAlgo, format must stay unforced",
                r#"builtins.convertHash {
                    hash = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
                    hashAlgo = "blake3";
                    toHashFormat = throw "the format must stay unforced";
                }"#,
            ),
        ] {
            assert_eq!(
                render(expr),
                "Eval(Eval, \"blake3 hashes (cppnix gates these behind the blake3-hashes experimental feature)\")",
                "{label} should reject disabled Blake3 by name"
            );
        }
    }

    /// The enabled half of `convertHash` reaches decoding and literal SRI
    /// rendering, rather than merely accepting the algorithm name.
    #[test]
    fn blake3_convert_hash_matches_cppnixs_known_answer() {
        let settings = crate::eval::Settings {
            blake3_hashes: true,
            ..crate::eval::Settings::default()
        };
        assert_eq!(
            crate::eval::render_str_with(
                &settings,
                r#"builtins.convertHash {
                    hash = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
                    hashAlgo = "blake3";
                    toHashFormat = "sri";
                }"#,
            ),
            "\"blake3-ZDezrDhGUTP/tjt1JzqNtUjFWEZdedsD/TWcbNW9nYU=\""
        );
    }

    /// The failures, each one cppnix's wording: which attribute is located
    /// first decides which missing attribute an incomplete call reports.
    #[test]
    fn convert_hash_fails_the_way_cppnix_does() {
        assert_eq!(
            render("builtins.convertHash { toHashFormat = \"sri\"; }"),
            "Eval(Eval, \"attribute 'hash' missing\")"
        );
        assert_eq!(
            render("builtins.convertHash { hash = \"abc\"; }"),
            "Eval(Eval, \"attribute 'toHashFormat' missing\")"
        );
        assert_eq!(
            render(
                "builtins.convertHash { hash = \"abc\"; hashAlgo = \"sha256\"; toHashFormat = \"hex\"; }"
            ),
            "Eval(Eval, \"unknown hash format 'hex', expect 'base16', 'base32', 'base64', or 'sri'\")"
        );
        assert_eq!(
            render(
                "builtins.convertHash { hash = \"abc\"; hashAlgo = \"crc32\"; toHashFormat = \"sri\"; }"
            ),
            "Eval(Eval, \"unknown hash algorithm 'crc32', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'\")"
        );
        assert_eq!(
            render("builtins.convertHash { hash = \"abc\"; toHashFormat = \"sri\"; }"),
            "Eval(Eval, \"hash 'abc' does not include a type, nor is the type otherwise known from context\")"
        );
    }

    #[test]
    fn hash_string_matches_the_corpus_digests() {
        // The empty-string row of eval-okay-hashstring, all four algorithms.
        assert_eq!(
            render("builtins.hashString \"md5\" \"\""),
            "\"d41d8cd98f00b204e9800998ecf8427e\""
        );
        assert_eq!(
            render("builtins.hashString \"sha1\" \"\""),
            "\"da39a3ee5e6b4b0d3255bfef95601890afd80709\""
        );
        assert_eq!(
            render("builtins.hashString \"sha256\" \"text 1\""),
            "\"900a4469df00ccbfd0c145c6d1e4b7953dd0afafadd7534e3a4019e8d38fc663\""
        );
        assert_eq!(
            render("builtins.hashString \"sha512\" \"\""),
            "\"cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e\""
        );
        assert_eq!(
            render("builtins.hashString \"sha3\" \"\""),
            "Eval(Eval, \"unknown hash algorithm 'sha3', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'\")"
        );
    }

    /// The first Blake3 vector in cppnix's `libutil-tests/hash.cc:40-49`:
    /// digesting `abc` through the evaluator must produce the same 32 bytes
    /// as cppnix, rather than merely accepting the algorithm name.
    #[test]
    fn blake3_hash_string_matches_cppnixs_known_answer() {
        let settings = crate::eval::Settings {
            blake3_hashes: true,
            ..crate::eval::Settings::default()
        };
        assert_eq!(
            crate::eval::render_str_with(&settings, "builtins.hashString \"blake3\" \"abc\""),
            "\"6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85\""
        );
    }

    #[test]
    fn match_is_anchored_posix_ere() {
        // regex_match, not regex_search: a pattern covering part of the
        // subject does not match.
        assert_eq!(render("builtins.match \"fo*\" \"foobar\""), "null");
        assert_eq!(render("builtins.match \"foobar\" \"foobar\""), "[ ]");
        assert_eq!(
            render("builtins.match \"(.*)\\\\.nix\" \"foobar.nix\""),
            "[ \"foobar\" ]"
        );
        // POSIX bracket classes, which an ECMAScript engine reads as a set
        // of punctuation instead.
        assert_eq!(
            render("builtins.match \"[[:space:]]+([[:upper:]]+)[[:space:]]+\" \"  FOO   \""),
            "[ \"FOO\" ]"
        );
        // An optional group that did not participate is null, not "".
        assert_eq!(
            render("builtins.match \"((.*)/)?([^/]*)\\\\.(nix|cc)\" \"foobar.cc\""),
            "[ null null \"foobar\" \"cc\" ]"
        );
        // A period spans a newline in POSIX, so this matches across lines.
        assert_eq!(render("builtins.match \".*b.*\" \"a\\nb\\nc\""), "[ ]");
    }

    #[test]
    fn split_interleaves_runs_and_group_lists() {
        assert_eq!(
            render("builtins.split \"(a)b\" \"abc\""),
            "[ \"\" [ \"a\" ] \"c\" ]"
        );
        assert_eq!(
            render("builtins.split \"(a)|(c)\" \"abc\""),
            "[ \"\" [ \"a\" null ] \"b\" [ null \"c\" ] \"\" ]"
        );
        // No match at all hands the subject back as one element, not three.
        assert_eq!(render("builtins.split \"fo+\" \"f\""), "[ \"f\" ]");
        assert_eq!(
            render("builtins.split \"fo*\" \"foobar\""),
            "[ \"\" [ ] \"bar\" ]"
        );
    }

    #[test]
    fn from_json_keeps_the_int_float_distinction() {
        assert_eq!(render("builtins.fromJSON \"1\""), "1");
        assert_eq!(render("builtins.fromJSON \"1.0\""), "1");
        assert_eq!(
            render("builtins.typeOf (builtins.fromJSON \"1\")"),
            "\"int\""
        );
        assert_eq!(
            render("builtins.typeOf (builtins.fromJSON \"1.0\")"),
            "\"float\""
        );
        assert_eq!(
            render("builtins.fromJSON \"{\\\"x\\\": [1, 2], \\\"y\\\": null}\""),
            "{ x = [ 1 2 ]; y = null; }"
        );
        assert_eq!(
            render("builtins.fromJSON \"18446744073709551615\""),
            "Eval(Eval, \"unsigned json number 18446744073709551615 outside of Nix integer range\")"
        );
        // A NUL cannot live in a Nix string, in a value or in a key.
        assert_eq!(
            render("builtins.fromJSON ''\"a\\u0000b\"''"),
            "Eval(Eval, \"input string 'a\u{2400}b' cannot be represented as Nix string because it contains null bytes\")"
        );
    }

    #[test]
    fn from_toml_builds_tables_and_refuses_what_cpp_refuses() {
        assert_eq!(
            render("builtins.fromTOML ''\n  x=1\n  s=\"a\"\n  [table]\n  y=2\n''"),
            "{ s = \"a\"; table = { y = 2; }; x = 1; }"
        );
        assert_eq!(
            render("builtins.fromTOML ''arr = [ 1, 2, 3 ]''"),
            "{ arr = [ 1 2 3 ]; }"
        );
        assert_eq!(
            render("builtins.fromTOML ''k = \"a\\u0000b\"''"),
            "Eval(Eval, \"while parsing TOML: error: input string 'a\u{2400}b' cannot be represented as Nix string because it contains null bytes\")"
        );
        // The parse-toml-timestamps feature decides whether cppnix builds a
        // { _type = "timestamp"; } attrset or refuses; this harness runs with
        // the feature off, so a date is the refusal, worded and wrapped the
        // way cppnix's parse visitor wraps it. The feature-on shape is pinned
        // in `vm::tests::from_toml_timestamps_match_toml11s_normalization`.
        assert_eq!(
            render("builtins.fromTOML ''d = 1979-05-27T07:32:00''"),
            "Eval(Eval, \"while parsing TOML: Dates and times are not supported\")"
        );
    }

    #[test]
    fn to_json_matches_nlohmann_dump() {
        assert_eq!(
            render("builtins.toJSON { a = 123; b = -456; c = \"foo\"; }"),
            r#""{\"a\":123,\"b\":-456,\"c\":\"foo\"}""#
        );
        // Names come out in lexicographic order whatever order they were
        // written in, and there is no whitespace anywhere.
        assert_eq!(
            render("builtins.toJSON { z = 1; a = 2; }"),
            r#""{\"a\":2,\"z\":1}""#
        );
        assert_eq!(
            render("builtins.toJSON [ 1 [ \"b\" { } ] ]"),
            r#""[1,[\"b\",{}]]""#
        );
        assert_eq!(render("builtins.toJSON 1.44"), "\"1.44\"");
        // An integral float keeps a decimal point, so it reads back a float.
        assert_eq!(render("builtins.toJSON 5.0"), "\"5.0\"");
        assert_eq!(render("builtins.toJSON null"), "\"null\"");
        // The escapes nlohmann names, plus \u00xx for the rest of C0.
        assert_eq!(
            render("builtins.toJSON \"a\\nb\\\"c\\td\""),
            r#""\"a\\nb\\\"c\\td\"""#
        );
        assert_eq!(
            render("builtins.toJSON (x: x)"),
            "Eval(Eval, \"cannot convert a function to JSON\")"
        );
    }

    #[test]
    fn to_json_takes_a_set_through_its_to_string_or_out_path() {
        // tryAttrsToString: __toString wins over the object form, and is
        // called with the set itself.
        assert_eq!(
            render("builtins.toJSON { __toString = self: self.a; a = \"foo\"; }"),
            "\"\\\"foo\\\"\""
        );
        // A derivation is serialised through outPath.
        assert_eq!(
            render("builtins.toJSON { outPath = \"/nix/store/x\"; drvPath = \"ignored\"; }"),
            "\"\\\"/nix/store/x\\\"\""
        );
    }

    /// cppnix's JSON walk spends one `max-call-depth` slot per level, so a
    /// value nested past the limit is refused rather than serialised. This
    /// walker is flat and has to be told; eval-fail-toJSON-stack-overflow is
    /// the pair that notices.
    #[test]
    fn to_json_refuses_a_value_deeper_than_max_call_depth() {
        let deep = "builtins.foldl' (tail: head: { inherit head tail; }) null (builtins.genList (x: x) 20000)";
        assert_eq!(
            render(&format!("builtins.toJSON ({deep})")),
            "Eval(Eval, \"stack overflow; max-call-depth exceeded\")"
        );
        // Just under the limit still serialises, so the cap is a cap and not
        // a blanket refusal.
        let shallow = "builtins.foldl' (tail: head: { inherit head tail; }) null (builtins.genList (x: x) 100)";
        assert!(render(&format!("builtins.toJSON ({shallow})")).starts_with('"'));
    }

    /// The two out-of-range TOML integers are allowlisted as an error-text
    /// divergence; that entry is only honest while toml-rs still refuses
    /// them, so a dependency bump that started accepting one must break here
    /// rather than in the corpus differ.
    #[test]
    fn from_toml_still_refuses_integers_past_the_nix_range() {
        for src in [
            "builtins.fromTOML ''attr = 9223372036854775808''",
            "builtins.fromTOML ''attr = -9223372036854775809''",
        ] {
            let got = render(src);
            assert!(
                got.starts_with("Eval(Eval, \"while parsing TOML:"),
                "expected a TOML parse refusal, got {got}"
            );
        }
    }
}

/// The three context builtins that reach a store, exercised against a host
/// that answers for one.
///
/// Separate from `tests` above because they need a `Host` with a store behind
/// it; `eval_str` runs on `RealFs`, which has none, and every one of these
/// would report itself unimplemented there -- which is the right answer and
/// not a useful test.
#[cfg(test)]
mod context_tests {
    use crate::compile;
    use crate::eval::drive;
    use crate::host::{FileType, Host, StoreError};
    use crate::vm::{Vm, VmError};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A host with a store. `ensure_path` records what it was asked for, so a
    /// test can assert the call happened rather than only that the result
    /// looked right.
    #[derive(Default)]
    struct Store {
        ensured: RefCell<Vec<String>>,
        /// When set, `ensure_path` fails with this text. Used to prove the
        /// hook is on the path at all: with it set, every `appendContext`
        /// must fail.
        ensure_fails: Option<String>,
    }

    impl Host for Store {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            file_type_resolved,
            get_env,
            warn,
            find_file,
            nix_path,
            trace
        );
        fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
            Ok(String::new())
        }
        fn read_dir(
            &self,
            _p: &crate::value2::PathValue,
        ) -> Result<Vec<(String, FileType)>, String> {
            Ok(Vec::new())
        }
        fn path_exists_checked(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(true)
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
            Ok(Some(FileType::Regular))
        }
        /// Store-path-shaped, unlike the shorter fakes elsewhere in this
        /// crate's tests: `appendContext` validates its keys, so a fake that
        /// is not a valid store path makes the round-trip test fail on the
        /// fixture rather than on the code.
        fn copy_to_store(&self, path: &crate::value2::PathValue) -> Result<String, StoreError> {
            Ok(format!("/nix/store/{}-f", fake_hash(path)))
        }
        fn ensure_path(&self, path: &str) -> Result<(), StoreError> {
            self.ensured.borrow_mut().push(path.to_owned());
            match &self.ensure_fails {
                Some(message) => Err(StoreError::Failed(message.clone())),
                None => Ok(()),
            }
        }
    }

    /// 32 nix32 characters derived from `path`, so a fake copy lands
    /// somewhere `storepath::is_store_path` accepts. FNV-1a because the
    /// value only has to be well-formed and stable, not right.
    fn fake_hash(path: &str) -> String {
        const NIX32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in path.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        (0..32u64)
            .map(|i| {
                h ^= i;
                h = h.wrapping_mul(0x1000_0000_01b3);
                NIX32
                    .get(((h >> 32) as usize) % 32)
                    .map_or('0', |b| char::from(*b))
            })
            .collect()
    }

    fn run_on(host: &Store, src: &str) -> String {
        crate::eval::render_with(&crate::eval::settings_with_store(), host, src)
    }

    fn run(src: &str) -> String {
        run_on(&Store::default(), src)
    }

    /// A `.drv` path from a real `derivationStrict`, so the tests below carry
    /// contexts the evaluator actually produces rather than ones assembled by
    /// hand.
    const DRV: &str = r#"(builtins.derivationStrict {
        name = "a"; system = "x86_64-linux"; builder = "/bin/sh";
    })"#;

    #[test]
    fn discarding_an_output_dependency_turns_all_outputs_into_a_plain_path() {
        // `drvPath` carries a derivation-deep element, which getContext
        // renders as allOutputs.
        let deep = run(&format!("builtins.getContext {DRV}.drvPath"));
        assert!(deep.contains("allOutputs = true"), "{deep}");
        let flat = run(&format!(
            "builtins.getContext (builtins.unsafeDiscardOutputDependency {DRV}.drvPath)"
        ));
        assert!(flat.contains("path = true"), "{flat}");
        assert!(!flat.contains("allOutputs"), "{flat}");
        // The bytes are untouched, which is the half a context rewrite must
        // not disturb.
        assert_eq!(
            run(&format!(
                "builtins.unsafeDiscardOutputDependency {DRV}.drvPath"
            )),
            run(&format!("{DRV}.drvPath"))
        );
    }

    #[test]
    fn adding_an_output_dependency_is_the_inverse_and_is_idempotent() {
        let back = run(&format!(
            "builtins.getContext (builtins.addDrvOutputDependencies
               (builtins.unsafeDiscardOutputDependency {DRV}.drvPath))"
        ));
        assert_eq!(back, run(&format!("builtins.getContext {DRV}.drvPath")));
        // Already deep: the corpus applies it twice and compares.
        let twice = run(&format!(
            "builtins.getContext (builtins.addDrvOutputDependencies
               (builtins.addDrvOutputDependencies {DRV}.drvPath))"
        ));
        assert_eq!(twice, back);
    }

    #[test]
    fn adding_an_output_dependency_refuses_what_cppnix_refuses() {
        // Not a derivation: an interpolated source path is an opaque store
        // path that does not end in .drv.
        let not_a_drv = run(r#"builtins.addDrvOutputDependencies "${/m/f}""#);
        assert!(not_a_drv.contains("is not a derivation"), "{not_a_drv}");
        // An output rather than a derivation.
        let an_output = run(&format!("builtins.addDrvOutputDependencies {DRV}.out"));
        assert!(
            an_output.contains("can only act on derivations"),
            "{an_output}"
        );
        // No context at all, and two elements: both are "exactly one".
        let none = run(r#"builtins.addDrvOutputDependencies "plain""#);
        assert!(none.contains("must have exactly one element"), "{none}");
        let two = run(&format!(
            r#"builtins.addDrvOutputDependencies "${{{DRV}.out}}${{{DRV}.drvPath}}""#
        ));
        assert!(two.contains("must have exactly one element"), "{two}");
    }

    #[test]
    fn append_context_round_trips_get_context() {
        // The eta rule the corpus asserts, checked on the context rather than
        // on the string, because string equality ignores the context and so
        // would pass with appendContext returning its argument unchanged.
        let src = format!(
            r#"let s = "${{{DRV}.out}}${{{DRV}.drvPath}}${{/m/f}}"; in
               builtins.getContext
                 (builtins.appendContext
                    (builtins.unsafeDiscardStringContext s)
                    (builtins.getContext s))"#
        );
        let rebuilt = run(&src);
        let original = run(&format!(
            r#"builtins.getContext "${{{DRV}.out}}${{{DRV}.drvPath}}${{/m/f}}""#
        ));
        assert_eq!(rebuilt, original);
        // All three element kinds took part, so the round trip covered them.
        assert!(rebuilt.contains("path = true"), "{rebuilt}");
        assert!(rebuilt.contains("allOutputs = true"), "{rebuilt}");
        assert!(rebuilt.contains("outputs = "), "{rebuilt}");
    }

    #[test]
    fn append_context_adds_to_the_context_the_first_argument_already_had() {
        let both = run(&format!(
            r#"builtins.getContext
                 (builtins.appendContext "${{/m/f}}"
                    (builtins.getContext {DRV}.drvPath))"#
        ));
        assert!(both.contains("path = true"), "{both}");
        assert!(both.contains("allOutputs = true"), "{both}");
    }

    #[test]
    fn append_context_refuses_a_key_that_is_not_a_store_path() {
        let bad = run(r#"builtins.appendContext "x" { "/not/a/store/path" = { path = true; }; }"#);
        assert!(
            bad.contains("context key '/not/a/store/path' is not a store path"),
            "{bad}"
        );
        // A well-formed path in the wrong directory is the same refusal, which
        // is what makes the handed-over store directory load-bearing.
        let elsewhere = run(
            r#"builtins.appendContext "x" { "/other/x0sj6ynccvc1a8kxr8fifnlf7qlxw6hd-a" = { path = true; }; }"#,
        );
        assert!(elsewhere.contains("is not a store path"), "{elsewhere}");
    }

    #[test]
    fn append_context_refuses_output_context_on_something_that_is_not_a_derivation() {
        let all_outputs = run(r#"builtins.appendContext "x"
                 { "/nix/store/x0sj6ynccvc1a8kxr8fifnlf7qlxw6hd-a" = { allOutputs = true; }; }"#);
        assert!(
            all_outputs.contains("tried to add all-outputs context of"),
            "{all_outputs}"
        );
        let outputs = run(r#"builtins.appendContext "x"
                 { "/nix/store/x0sj6ynccvc1a8kxr8fifnlf7qlxw6hd-a" = { outputs = [ "out" ]; }; }"#);
        assert!(
            outputs.contains("tried to add derivation output context of"),
            "{outputs}"
        );
        // An empty list is allowed on a non-derivation, as cppnix's
        // `listSize() &&` guard says.
        let empty = run(r#"builtins.getContext (builtins.appendContext "x"
                 { "/nix/store/x0sj6ynccvc1a8kxr8fifnlf7qlxw6hd-a" = { outputs = [ ]; }; })"#);
        assert_eq!(empty, "{ }");
    }

    /// The store is asked about every key, and the answer is not ignored.
    ///
    /// Both halves matter: without the first this passes for an
    /// implementation that never calls the hook, and without the second it
    /// passes for one that calls it and drops the error. Watched failing by
    /// deleting the `Yield::Need` from `next_key`, which turns both
    /// assertions red.
    #[test]
    fn every_key_goes_to_the_store_and_a_store_failure_is_reported() {
        let host = Store::default();
        let ok = run_on(
            &host,
            r#"builtins.appendContext "x"
                 { "/nix/store/x0sj6ynccvc1a8kxr8fifnlf7qlxw6hd-a" = { path = true; };
                   "/nix/store/v15m7i45c5ihs7x3637463dfl8xmpk8r-c.drv" = { allOutputs = true; }; }"#,
        );
        assert_eq!(ok, "\"x\"");
        assert_eq!(
            *host.ensured.borrow(),
            vec![
                "/nix/store/v15m7i45c5ihs7x3637463dfl8xmpk8r-c.drv".to_owned(),
                "/nix/store/x0sj6ynccvc1a8kxr8fifnlf7qlxw6hd-a".to_owned(),
            ]
        );

        let failing = Store {
            ensure_fails: Some("path is not valid".to_owned()),
            ..Store::default()
        };
        let refused = run_on(
            &failing,
            r#"builtins.appendContext "x"
                 { "/nix/store/x0sj6ynccvc1a8kxr8fifnlf7qlxw6hd-a" = { path = true; }; }"#,
        );
        assert!(refused.contains("path is not valid"), "{refused}");
    }

    /// A host that owns no store says so, rather than admitting a key nothing
    /// checked. `Host::ensure_path`'s default is what answers here.
    #[test]
    fn without_a_store_append_context_refuses_by_name() {
        struct NoStore;
        impl Host for NoStore {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
                Ok(String::new())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
        }
        let src = r#"builtins.appendContext "x"
             { "/nix/store/x0sj6ynccvc1a8kxr8fifnlf7qlxw6hd-a" = { path = true; }; }"#;
        let Ok(module) = compile::compile_source(
            src,
            "/m",
            crate::compile::Origin::String,
            &crate::eval::settings_with_store(),
        ) else {
            unreachable!("the source parses")
        };
        let mut vm = Vm::with_settings(crate::eval::settings_with_store());
        vm.start_module(&Rc::new(module));
        let got = drive(&mut vm, &NoStore);
        assert!(
            matches!(&got, Err(VmError::Unimplemented(r)) if r.detail.contains("appendContext")),
            "{got:?}"
        );
    }
}

/// `builtins.toFile` (`primops.cc:2789`).
///
/// The store path is a function of the bytes and the declared references, so
/// the *path* is computable without writing; whether the bytes are actually
/// written is `settings.readOnlyMode`, which belongs to the embedder. Hence a
/// question rather than a computation here: guessing would be right under
/// `nix-instantiate --eval` and wrong under `nix build`. ENG-12607.
pub fn bi_to_file(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let name = crate::primops_pure::want_text_no_ctx(&crate::vm::forced(
        args.first()
            .ok_or_else(|| VmError::eval("internal: builtins.toFile without a name"))?,
    )?)?;
    let value = crate::vm::forced(
        args.get(1)
            .ok_or_else(|| VmError::eval("internal: builtins.toFile without contents"))?,
    )?;
    let contents = crate::primops_pure::want_nix_str(&value)?;

    // cppnix walks the context and refuses anything that is not a plain store
    // path, because the file it is about to write cannot depend on something
    // that has not been built. A `Built` or `DrvDeep` element would have to
    // become an input derivation, and a text blob has no way to carry one.
    let mut references: Vec<String> = Vec::new();
    for element in contents.context_set() {
        match &element {
            crate::value2::ContextElem::Opaque(p) => references.push(p.to_string()),
            other => {
                return Err(VmError::eval(format!(
                    "files created by builtins.toFile may not reference derivations, but \
                     {name} references {}",
                    // The base-name form: cpp renders this element through
                    // `NixStringContextElem::to_string`, which prints a
                    // `StorePath`, not a path. `display()` gives the whole
                    // path and is right for its other caller.
                    other.display_base_name()
                )));
            }
        }
    }
    // Sorted, because the references are stuffed into the store path's type
    // string in cppnix's `StorePathSet` order and an unsorted list is a
    // different path.
    references.sort();
    references.dedup();

    Ok(Begin::Cont(Cont::Ask {
        asked: false,
        need: NeedPath::StoreText {
            name,
            // The store leg of this question is text over the ABI today;
            // binary contents refuse at the boundary (NonUtf8Boundary) and
            // fall back rather than writing repaired bytes (ENG-13147).
            contents: crate::primops_pure::text_of(contents)?.to_owned(),
            references,
        },
    }))
}

#[cfg(test)]
mod to_file_tests {
    use crate::host::{FileType, Host, StoreError};
    use std::cell::RefCell;

    /// A store that records what it was asked to hold and answers with the
    /// path cpp would compute, so the test is about the *builtin* rather than
    /// about path arithmetic (`drvpath::make_text_path` has its own coverage).
    #[derive(Default)]
    struct TextStore(RefCell<Vec<(String, String, Vec<String>)>>);

    impl Host for TextStore {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            file_type_resolved,
            get_env,
            copy_to_store,
            ensure_path,
            warn,
            find_file,
            nix_path,
            trace
        );
        fn read_file(&self, _p: &crate::value2::PathValue) -> std::result::Result<String, String> {
            Ok(String::new())
        }
        fn read_dir(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, FileType)>, String> {
            Ok(Vec::new())
        }
        fn path_exists_checked(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(true)
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<Option<FileType>, String> {
            Ok(Some(FileType::Regular))
        }
        fn store_text(
            &self,
            name: &str,
            contents: &str,
            references: &[String],
        ) -> std::result::Result<String, StoreError> {
            self.0
                .borrow_mut()
                .push((name.to_owned(), contents.to_owned(), references.to_vec()));
            Ok(crate::drvpath::make_text_path(
                "/nix/store",
                name,
                contents,
                references.iter().map(String::as_str),
            ))
        }
    }

    fn run(src: &str) -> (String, Vec<(String, String, Vec<String>)>) {
        let host = TextStore::default();
        let rendered = crate::eval::render_with(&crate::eval::settings_with_store(), &host, src);
        (rendered, host.0.take())
    }

    /// ENG-12607. The bytes cpp nix 2.34.7 printed for the same expressions.
    ///
    /// `toFile` is not a path computation the evaluator may do itself: the
    /// store path is pure, but whether the bytes get *written* is the
    /// embedder's `readOnlyMode`, so what this checks is that the question
    /// leaves with the right three arguments and the answer comes back as the
    /// string with the right context.
    #[test]
    fn to_file_asks_the_store_and_returns_its_answer() {
        let (rendered, asked) = run(r#"builtins.toFile "hi" "hello""#);
        assert_eq!(
            rendered,
            "\"/nix/store/ppzz3n6kk6fzbspnm1nrag1g97j60mz2-hi\""
        );
        assert_eq!(
            asked,
            vec![("hi".to_owned(), "hello".to_owned(), Vec::new())]
        );

        // The result carries the new path as its context and *only* that.
        // cpp says so in its own comment: the store path already references
        // whatever the argument referenced, so propagating the argument's
        // context would give the string more dependencies than cpp gives it.
        assert_eq!(
            run(r#"builtins.getContext (builtins.toFile "hi" "hello")"#).0,
            "{ \"/nix/store/ppzz3n6kk6fzbspnm1nrag1g97j60mz2-hi\" = { path = true; }; }"
        );
    }

    /// References in the contents reach the store path, sorted.
    ///
    /// The gap this fills: every `toFile` case above passes contents with no
    /// context, so the reference list is always empty and nothing observes
    /// what happens to it. Deleting the `references.push` and deleting the
    /// `references.sort()` both left the suite green before this test.
    ///
    /// They are tier 1, not hygiene. `makeType` stuffs each reference into
    /// the store path's *type* string, in `StorePathSet` order, so a dropped
    /// or reordered reference is a well-formed path for a different store
    /// object. Two references rather than one, because a single one cannot
    /// tell a sort from an append.
    ///
    /// Goldens from cpp nix 2.34.7+ix.g69e4d9e9db39, store `/nix/store`.
    #[test]
    fn references_in_the_contents_reach_the_path() {
        let plain = r#"builtins.toFile "hello.txt" "hello world""#;
        assert_eq!(
            run(plain).0,
            "\"/nix/store/m6wswa7yn6x5gi6gdq7x1fqlwmlhfja9-hello.txt\""
        );
        let (rendered, asked) = run(&format!(
            r#"let plain = {plain}; in builtins.toFile "refs.txt" "see ${{plain}} for details""#
        ));
        assert_eq!(
            rendered,
            "\"/nix/store/31qrydcvwcpb21g22fpvmss7rpxzb2zv-refs.txt\""
        );
        // The question itself, so a right answer for a wrong reason is
        // visible: the reference has to leave the builtin, not be recovered
        // by the store.
        assert_eq!(
            asked.last().map(|(n, _, r)| (n.clone(), r.clone())),
            Some((
                "refs.txt".to_owned(),
                vec!["/nix/store/m6wswa7yn6x5gi6gdq7x1fqlwmlhfja9-hello.txt".to_owned()]
            ))
        );
        assert_eq!(
            run(&format!(
                r#"let plain = {plain}; empty = builtins.toFile "empty" "";
                   in builtins.toFile "refs2.txt" "${{plain}} and ${{empty}}""#
            ))
            .0,
            "\"/nix/store/qsk84ak4kpkb943gq6k4m3qyk4r05v92-refs2.txt\""
        );
    }

    /// What a derivation embedding a `toFile` result lands on.
    ///
    /// The result's `Opaque` element is already asserted above through
    /// `getContext`, and this is the row that says why it matters: the
    /// element becomes an `inputSrcs` entry, so it moves the consuming
    /// derivation's `.drv` bytes and its output path while the `toFile`
    /// string itself stays byte-identical. An assertion on the returned
    /// string alone cannot see that, which is the same shape as the `r:`
    /// prefix in #108.
    #[test]
    fn a_derivation_embedding_a_to_file_result_matches_cppnix() {
        let plain = r#"builtins.toFile "hello.txt" "hello world""#;
        assert_eq!(
            run(&format!(
                r#"builtins.derivationStrict {{
                     name = "c"; system = "x86_64-linux"; builder = "/bin/sh"; conf = {plain};
                   }}"#
            ))
            .0,
            "{ drvPath = \"/nix/store/mjzdhacm3jrxwpc040zgg77zkv2w39vw-c.drv\"; \
             out = \"/nix/store/3w6bf6wkc8w3yggvmcd01msxxn12x73c-c\"; }"
        );
        // One level deeper, so the chain is pinned rather than one link: the
        // embedded file has a reference of its own.
        assert_eq!(
            run(&format!(
                r#"let plain = {plain};
                       withRef = builtins.toFile "refs.txt" "see ${{plain}} for details";
                   in builtins.derivationStrict {{
                     name = "c"; system = "x86_64-linux"; builder = "/bin/sh"; conf = withRef;
                   }}"#
            ))
            .0,
            "{ drvPath = \"/nix/store/3qjpgz3gjrplldgfybn7w1cd48fwwj8w-c.drv\"; \
             out = \"/nix/store/6f4aij9f9kbsxs028lg9xamdjnlgy54n-c\"; }"
        );
    }

    /// A text blob has no way to carry an input derivation, so cpp refuses a
    /// context element that is not a plain store path -- by name, and naming
    /// the offending element.
    #[test]
    fn to_file_refuses_a_reference_to_a_derivation() {
        // `derivation` needs the store directory, and it is a `OnceLock`
        // shared with every other test in this process; `/nix/store` is what
        // the rest of them use.
        let got = run(r#"builtins.toFile "hi" "${(derivation {
                 name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; }).outPath}""#)
        .0;
        assert!(
            got.contains("may not reference derivations") && got.contains("hi references"),
            "want cpp's refusal naming the file and the element; got {got}"
        );
        // And the element is the BASE NAME, not the whole path. cpp renders
        // it through `NixStringContextElem::to_string`, which prints a
        // `StorePath`:
        //
        //   files created by builtins.toFile may not reference derivations,
        //   but bad.txt references !out!nrb4avj6...-d.drv
        //
        // Checking only the two clauses above is not enough, which is how
        // this diverged unnoticed: `!out!/nix/store/<hash>-d.drv` satisfies
        // both of them. The assertion has to name what must be ABSENT.
        assert!(
            got.contains("!out!") && !got.contains("!out!/nix/store/"),
            "the element should render as a base name, not a full path: {got}"
        );
    }

    /// Without a store the builtin refuses by name rather than computing a
    /// path whose bytes may never be written. The same reasoning as
    /// `copy_to_store` (ENG-12447): a path nothing wrote is a wrong answer
    /// wherever the caller then expects the file to be there.
    #[test]
    fn to_file_without_a_store_refuses_rather_than_computing() {
        struct NoStore;
        impl Host for NoStore {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Ok(String::new())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
        }
        let Ok(module) = crate::compile::compile_source(
            r#"builtins.toFile "hi" "x""#,
            "/m",
            crate::compile::Origin::String,
            &crate::eval::settings_with_store(),
        ) else {
            unreachable!()
        };
        let mut vm = crate::vm::Vm::with_settings(crate::eval::settings_with_store());
        vm.start_module(&std::rc::Rc::new(module));
        let outcome = match crate::eval::drive(&mut vm, &NoStore) {
            Err(crate::vm::VmError::Unimplemented(r)) => r.detail,
            other => format!("{other:?}"),
        };
        assert_eq!(outcome, "builtins.toFile (no store behind this evaluator)");
    }
}

#[cfg(test)]
mod to_json_path_tests {
    use crate::host::{FileType, Host, StoreError};

    /// A store that answers a copy with a fixed path, so the test is about
    /// the JSON renderer routing the question rather than about the copy.
    struct Copies;

    impl Host for Copies {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            file_type_resolved,
            get_env,
            ensure_path,
            warn,
            find_file,
            nix_path,
            trace
        );
        fn read_file(&self, _p: &crate::value2::PathValue) -> std::result::Result<String, String> {
            Ok(String::new())
        }
        fn read_dir(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, FileType)>, String> {
            Ok(Vec::new())
        }
        fn path_exists_checked(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(true)
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<Option<FileType>, String> {
            Ok(Some(FileType::Regular))
        }
        fn copy_to_store(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<String, StoreError> {
            Ok(format!(
                "/nix/store/0000000000000000000000000000000a-{}",
                path.rsplit('/').next().unwrap_or("x")
            ))
        }
    }

    fn run(src: &str) -> String {
        crate::eval::render_with(&crate::eval::settings_with_store(), &Copies, src)
    }

    /// The copy has to reach the `.drv`, not only the string.
    ///
    /// PR #108's lesson, applied here: a value that feeds a hash usually has
    /// more than one consumer, and asserting the headline output leaves the
    /// others unguarded. There it was the ATerm's `hashAlgo`, correct in
    /// `outPath` and wrong in the `.drv`. Here it is the *context* the copy
    /// produces: the JSON string is right either way, but if the context does
    /// not propagate, the copied path never becomes an `inputSrcs` entry of a
    /// derivation that uses the string, and that derivation's `drvPath` and
    /// every dependent's output path move.
    ///
    /// Relational rather than golden, deliberately. A golden here would have
    /// to come from cpp, and cpp copies against a real store while this test
    /// uses a fake one, so the only honest golden for the bytes is the
    /// dev-node differential. What is checkable without a store is that the
    /// copy became a *dependency*, and that is exactly the link that breaks.
    ///
    /// The bytes, for the record. Measured on dev-compute-2 at d066bdbfc with
    /// one binary and both arms, over a file added to a real store as
    /// `/nix/store/az07xyf0v17knmdywpnw3jllqd421h1v-tj-src.txt`:
    ///
    /// ```text
    ///                                    cpp and rust, identical
    ///   toJSON <path>   "/nix/store/3js6fihqp8z1dijir669wl33fdh12mh5-az07...-tj-src.txt"
    ///   consumer .drv   /nix/store/94p3hbhqb18k13z6h3vzjhb19snkgq9s-consumer.drv
    ///   consumer out    /nix/store/956bd0hy338a304b4hzark6m3r8sc5qg-consumer
    /// ```
    ///
    /// Note the first row: cpp does *not* pass an already-in-store path
    /// through unchanged, it re-copies it under a nested name. That is why
    /// this test cannot carry those bytes as goldens against its fake store,
    /// and why the differential above is where they live.
    ///
    /// Isolating it takes a pair. The JSON string is an environment variable
    /// either way, so its bytes move the `.drv` path whether or not the
    /// context propagated; changing the path and watching the `.drv` move
    /// proves nothing. `unsafeDiscardStringContext` gives the same bytes with
    /// no context, so the two derivations differ in `inputSrcs` and in
    /// nothing else. They must not land on the same `.drv`.
    #[test]
    fn a_path_copied_by_to_json_becomes_an_input_of_the_derivation_using_it() {
        let drv = |attr: &str| {
            run(&format!(
                r#"(builtins.derivationStrict {{
                     name = "c"; system = "x86_64-linux"; builder = "/bin/sh";
                     j = {attr};
                   }}).drvPath"#
            ))
        };

        let with_context = drv("builtins.toJSON /m/f");
        let same_bytes_no_context =
            drv("builtins.unsafeDiscardStringContext (builtins.toJSON /m/f)");

        assert!(
            with_context.contains("/nix/store/") && with_context.contains("-c.drv"),
            "expected a .drv path; got {with_context}"
        );
        assert_ne!(
            with_context, same_bytes_no_context,
            "the two derivations have identical environments and differ only in \
             whether the copied path is an input, so an equal .drv path means the \
             copy never became one and every dependent's output path is wrong"
        );
    }

    /// ENG-12607. `toJSON` of a path copies it into the store and emits the
    /// store path (`value-to-json.cc:83`, `copyToStore` on), which cpp does
    /// and this used to refuse outright -- the refusal's own comment said the
    /// backend had no store handle, which stopped being true when ENG-12447
    /// gave string interpolation one.
    ///
    /// Nested and beside other values, because the renderer is a worklist and
    /// the interesting failure is the answer landing in the wrong position.
    #[test]
    fn to_json_copies_a_path_into_the_store() {
        assert_eq!(
            run(r#"builtins.toJSON /m/f"#),
            "\"\\\"/nix/store/0000000000000000000000000000000a-f\\\"\""
        );
        assert_eq!(
            run(r#"builtins.toJSON { a = 1; p = /m/f; z = "s"; }"#),
            "\"{\\\"a\\\":1,\\\"p\\\":\\\"/nix/store/0000000000000000000000000000000a-f\\\",\\\"z\\\":\\\"s\\\"}\""
        );
        assert_eq!(
            run(r#"builtins.toJSON [ /m/a /m/b ]"#),
            "\"[\\\"/nix/store/0000000000000000000000000000000a-a\\\",\\\"/nix/store/0000000000000000000000000000000a-b\\\"]\""
        );
        // The copy is a dependency of the resulting string, as it is for a
        // path interpolated into one.
        assert_eq!(run(r#"builtins.hasContext (builtins.toJSON /m/f)"#), "true");
    }
}

// -- builtins.path ----------------------------------------------------------

/// A recognized attribute of `builtins.path`'s argument set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PathAttr {
    Path,
    Name,
    Filter,
    Recursive,
    Sha256,
}

impl PathAttr {
    fn of(name: &str) -> Option<PathAttr> {
        match name {
            "path" => Some(PathAttr::Path),
            "name" => Some(PathAttr::Name),
            "filter" => Some(PathAttr::Filter),
            "recursive" => Some(PathAttr::Recursive),
            "sha256" => Some(PathAttr::Sha256),
            _ => None,
        }
    }
}

/// What the machine is waiting for, and so how to read the next value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PathPhase {
    /// The value of the attribute in `current`.
    Attr,
    /// cppnix's empty-`sha256` warning has been emitted; nothing comes back.
    Warned,
    /// The type of `ancestors[i]`, which is either a component of the root's
    /// path or the root itself.
    Ancestor,
    /// A directory's entries.
    Entries,
    /// The filter applied to the path; the value is a partial application.
    FilterHalf,
    /// The filter applied to both arguments; the value is its verdict.
    FilterVerdict,
    /// The root's context has left as a [`NeedPath::Realise`]; the value is
    /// the rewrite map that decides the root's final spelling.
    RootRealising,
    /// The copy has left through `Host`; the value is the store path.
    Asked,
}

/// One directory the walk has opened, and how far through its entries it is.
struct DirFrame {
    dir: String,
    /// Name-sorted, which is the order cppnix's `dumpPath` consults the filter
    /// in: it copies `readDirectory` into a `std::map` keyed by name before it
    /// iterates (`libutil/archive.cc:79`).
    entries: Vec<(String, FileType)>,
    i: usize,
}

/// `builtins.path` (`primops.cc:3073`), including the filtered copy that
/// `lib.cleanSourceWith` and every `lib.sources` helper is spelled as.
///
/// # The walk is here because the filter is a Nix function
///
/// cppnix passes the filter into the copy and calls it from inside `dumpPath`.
/// This evaluator performs no IO, so the copy is a question for the embedder
/// -- and a question cannot call back into the interpreter. The walk therefore
/// runs on this side, through the same `Entries` and `Kind` questions
/// `builtins.readDir` uses, and the embedder is handed the finished list. See
/// [`crate::task::NeedPath::StoreFiltered`].
///
/// # Where this is not cppnix
///
/// The `path` attribute goes through the same [`coerce_to_path`] every other
/// path-family builtin uses (ENG-12669), because cppnix's `prim_path` reads it
/// with the same `coerceToPath`: a set with `__toString` or `outPath` names a
/// path here exactly as it does in `builtins.readFile`.
///
/// * Attributes are read in this crate's symbol order, which is its interning
///   order, exactly as cppnix reads them in its own. The two histories differ,
///   so when *two* attributes are malformed the pair may disagree about which
///   one is reported. One malformed attribute reports identically.
/// * A root path with a symlink anywhere in it is refused
///   ([`RefusalToken::AddPath`]) when a filter is given. cppnix resolves the
///   root before it walks (`SourcePath::resolveSymlinks`), so the strings its
///   filter sees are the resolved ones; this evaluator has no question that
///   resolves a symlink, and guessing would hand the filter a different string
///   and so possibly a different tree. Without a filter nothing observes the
///   difference and the embedder resolves it as cppnix does. The fix is a
///   host question that resolves a symlink; ENG-12700 has the shape.
pub struct PathBuiltin {
    /// Which builtin this machine is running as, for the two refusals and two
    /// errors a caller can see. `builtins.filterSource` drives the same
    /// machine (see [`bi_filter_source`]), and a refusal naming the wrong
    /// builtin sends the reader to the wrong line of their expression.
    builtin: &'static str,
    /// Recognized-or-not attributes still to read, in the argument set's own
    /// order. Popped one at a time rather than validated up front, because
    /// cppnix reports the first unsupported attribute only after evaluating
    /// the recognized ones that precede it.
    queue: VecDeque<(Sym, Slot)>,
    current: Option<PathAttr>,
    phase: PathPhase,
    /// How far [`coerce_to_path`] has got with the `path` attribute. A set
    /// there needs `__toString` run, which needs the machine, so the
    /// coercion arrives back here a second time.
    path_stage: PathStage,

    root: Option<Rc<PathValue>>,
    name: Option<String>,
    filter: Option<Value>,
    method: PathMethod,
    expected_sha256: Option<String>,

    /// The prefixes of the root path, shortest first, each of which is
    /// lstat'ed to catch a symlink cppnix would have resolved. The last is the
    /// root itself, so its answer also decides whether there is a tree to walk.
    ancestors: Vec<Rc<PathValue>>,
    ancestor: usize,

    stack: Vec<DirFrame>,
    accepted: Vec<AcceptedPath>,
    /// The entry whose verdict is in flight.
    pending: Option<AcceptedPath>,
    /// cppnix's `addPath` refs branch applied: the root coerced with a
    /// context and is in the store. See [`FilteredCopy::inherit_references`].
    inherit_references: bool,
    /// The root as coerced, held while its context is out being realised.
    pending_root: Option<Rc<PathValue>>,
}

pub fn bi_path(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let attrs = want_attrs(&argv(args, 0)?)?;
    Ok(Begin::Cont(Cont::Ext(Ext::Path(Box::new(PathBuiltin {
        builtin: "builtins.path",
        queue: attrs.iter().map(|(k, v)| (*k, v.clone())).collect(),
        current: None,
        phase: PathPhase::Attr,
        path_stage: PathStage::Value,
        root: None,
        name: None,
        filter: None,
        // cppnix's default: `recursive` absent means a NAR of the tree.
        method: PathMethod::NixArchive,
        expected_sha256: None,
        ancestors: Vec::new(),
        ancestor: 0,
        stack: Vec::new(),
        accepted: Vec::new(),
        pending: None,
        inherit_references: false,
        pending_root: None,
    })))))
}

/// `builtins.filterSource` (`primops.cc:3004`), which is `addPath` with the
/// argument set spelled positionally.
///
/// cppnix's body is four lines and every one of them is already in
/// [`PathBuiltin`]: coerce argument 1 to a path, force argument 0 as a
/// function, then `addPath(name = path.baseName(), path, filterFun,
/// NixArchive, no expectedHash)`. So this builds that machine with a
/// two-entry queue rather than walking the tree a second time -- the walk,
/// the ordering `dumpPath` imposes on the filter, the symlink refusal and
/// the store-path-context refusal are one implementation for both builtins
/// (ENG-12678). A second walk here is what would drift.
///
/// The queue order is cppnix's argument order and is load-bearing: the path
/// is coerced first, so a set whose `__toString` throws reports that throw
/// before the filter is looked at, and a non-function filter is reported
/// only after the coercion has finished. Reversing the two changes which
/// error a program sees.
///
/// The remaining three `addPath` parameters are cppnix's constants here,
/// not defaults inherited from `builtins.path`: `NixArchive` because
/// `filterSource` has no `recursive` argument, no `sha256` because it has no
/// hash argument, and the name from `path.baseName()` because it has no
/// `name` argument -- which is the whole of the warning in its own manual
/// entry, since the base name of the *unfiltered* directory ends up in the
/// store path.
pub fn bi_filter_source(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let path = vm.intern("path");
    let filter = vm.intern("filter");
    let source = arg_slot(args, 1)?;
    let filter_fun = arg_slot(args, 0)?;
    Ok(Begin::Cont(Cont::Ext(Ext::Path(Box::new(PathBuiltin {
        builtin: "builtins.filterSource",
        queue: VecDeque::from(vec![(path, source), (filter, filter_fun)]),
        current: None,
        phase: PathPhase::Attr,
        path_stage: PathStage::Value,
        root: None,
        name: None,
        filter: None,
        method: PathMethod::NixArchive,
        expected_sha256: None,
        ancestors: Vec::new(),
        ancestor: 0,
        stack: Vec::new(),
        accepted: Vec::new(),
        pending: None,
        inherit_references: false,
        pending_root: None,
    })))))
}

/// The unforced slot at a position, for a builtin that forces it itself.
fn arg_slot(args: &[Slot], i: usize) -> Result<Slot> {
    args.get(i)
        .cloned()
        .ok_or_else(|| VmError::eval("internal: missing builtin argument"))
}

impl PathBuiltin {
    fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        match (self.phase, incoming) {
            (PathPhase::Asked, Some(v)) => Ok(Yield::Done(v)),
            (PathPhase::Asked, None) => {
                Err(VmError::eval("internal: builtins.path lost its answer"))
            }
            (PathPhase::Attr, None) => self.next_attr(vm),
            (PathPhase::Attr, Some(v)) => self.take_attr(vm, v),
            (PathPhase::Warned, _) => self.next_attr(vm),
            (PathPhase::RootRealising, Some(v)) => {
                let pending = self
                    .pending_root
                    .take()
                    .ok_or_else(|| VmError::eval("internal: builtins.path lost its root"))?;
                self.root = Some(crate::primops_pure::apply_rewrites(pending, Some(v))?);
                self.inherit_references = true;
                self.next_attr(vm)
            }
            // `null` is [`NeedPath::MaybeKind`] saying the accessor has no
            // such path, which is an answer and not a missing one.
            (PathPhase::Ancestor, Some(Value::Null)) => self.take_ancestor(None),
            (PathPhase::Ancestor, Some(v)) => {
                let kind = want_text(&v)?;
                self.take_ancestor(Some(&kind))
            }
            (PathPhase::Entries, Some(v)) => self.take_entries(vm, &v),
            (PathPhase::FilterHalf, Some(v)) => {
                let file_type = self
                    .pending
                    .as_ref()
                    .ok_or_else(|| VmError::eval("internal: builtins.path lost its entry"))?
                    .file_type;
                self.phase = PathPhase::FilterVerdict;
                Ok(Yield::Apply(
                    v,
                    Slot::value(Value::Str(file_type.as_str().into())),
                ))
            }
            (PathPhase::FilterVerdict, Some(v)) => self.take_verdict(&v),
            (
                PathPhase::Ancestor
                | PathPhase::Entries
                | PathPhase::FilterHalf
                | PathPhase::FilterVerdict
                | PathPhase::RootRealising,
                None,
            ) => Err(VmError::eval("internal: builtins.path lost a value")),
        }
    }

    // -- reading the argument set --------------------------------------------

    fn next_attr(&mut self, vm: &mut Vm) -> Result<Yield> {
        self.phase = PathPhase::Attr;
        let Some((sym, slot)) = self.queue.pop_front() else {
            self.current = None;
            return self.begin_walk();
        };
        let name = vm.sym_name(sym);
        let Some(attr) = PathAttr::of(name) else {
            return Err(VmError::eval(format!(
                "unsupported argument '{name}' to '{}'",
                self.builtin
            )));
        };
        self.current = Some(attr);
        if attr == PathAttr::Path {
            self.path_stage = PathStage::Value;
        }
        Ok(Yield::Force(slot))
    }

    fn take_attr(&mut self, vm: &mut Vm, value: Value) -> Result<Yield> {
        let attr = self
            .current
            .ok_or_else(|| VmError::eval("internal: builtins.path lost an attribute"))?;
        match attr {
            PathAttr::Path => {
                // The context is read off whatever value the coercion is
                // looking at, at either stage: a string arrives carrying its
                // own, and a set arrives as the string its `__toString` or
                // `outPath` produced, which carries the accumulated one. A
                // path value carries none, which is right.
                let elements = match &value {
                    Value::Str(s) => s.context_set(),
                    _ => std::collections::BTreeSet::new(),
                };
                let path = match coerce_to_path(&value, &mut self.path_stage)? {
                    // A set: `__toString` has to run, and its result comes
                    // back here with the stage already advanced.
                    Coerced::Run(y) => return Ok(y),
                    Coerced::Done(p) => p,
                };
                // cppnix's `addPath` opens by asking whether the source is a
                // store path whose coercion carried a context. If it is, the
                // context is realised, the path rewritten to the realised
                // outputs, and the store object's own references become the
                // copy's (`primops.cc:2947`).
                //
                // # Why this site tests more than the rest of the family
                // # (do not "unify" this back)
                //
                // Everywhere else, `realisePath` realises whenever the context
                // is non-empty. Here the whole branch -- realise, rewrite,
                // inherit references -- is additionally conditional on the
                // root already being in the store, because that is what
                // `store->isInStore(path)` tests before any of it. A source
                // tree outside the store that happens to carry a context is
                // copied as written by cppnix, and so here.
                //
                // The references matter more than the rewrite: they are part
                // of the content address, so dropping them lands the copy on a
                // different, well-formed, wrong store path that feeds a
                // derivation hash with nothing downstream able to tell. That
                // is why the flag travels rather than being re-derived by the
                // embedder; see [`FilteredCopy::inherit_references`].
                if !elements.is_empty()
                    && vm
                        .settings()
                        .store_dir
                        .as_deref()
                        .is_none_or(|dir| crate::storepath::is_store_path(dir, path.as_ref()))
                {
                    self.pending_root = Some(path);
                    self.phase = PathPhase::RootRealising;
                    return Ok(Yield::Need(NeedPath::Realise(
                        elements.into_iter().collect(),
                    )));
                }
                self.root = Some(path);
            }
            PathAttr::Name => self.name = Some(want_text_no_ctx(&value)?),
            PathAttr::Filter => {
                if !is_callable(&value, vm) {
                    return Err(VmError::eval(format!(
                        "expected a function but found {}",
                        type_name(&value)
                    )));
                }
                self.filter = Some(value);
            }
            PathAttr::Recursive => {
                self.method = if want_bool(&value)? {
                    PathMethod::NixArchive
                } else {
                    PathMethod::Flat
                };
            }
            PathAttr::Sha256 => {
                let text = want_text_no_ctx(&value)?;
                // Parsed here rather than passed on raw, so a malformed hash
                // is an evaluation error at the point cppnix raises one, and
                // so the embedder receives one unambiguous spelling. An empty
                // string is not "absent": cppnix substitutes the all-zero hash
                // of the algorithm and warns (`hash.cc:278`), and the copy then
                // almost certainly fails the mismatch check.
                let (hash, warning) = crate::nixhash::new_hash_allow_empty(
                    &text,
                    Some(crate::nixhash::HashAlgo::Sha256),
                    false,
                )
                .map_err(|e| VmError::eval(e.to_string()))?;
                self.expected_sha256 = Some(hash.to_sri());
                if let Some(message) = warning {
                    self.phase = PathPhase::Warned;
                    return Ok(Yield::Need(NeedPath::Warn(message)));
                }
            }
        }
        self.next_attr(vm)
    }

    // -- the walk ------------------------------------------------------------

    fn begin_walk(&mut self) -> Result<Yield> {
        let Some(root) = self.root.clone() else {
            return Err(VmError::eval(format!(
                "missing required 'path' attribute in the first argument to '{}'",
                self.builtin
            )));
        };
        // cppnix: `if (name.empty()) name = path->baseName();`, so an explicit
        // empty name also falls back rather than naming the store object "".
        if self.name.as_ref().is_none_or(String::is_empty) {
            self.name = Some(base_name_of(root.as_ref()));
        }
        // `Flat` ingests one file's bytes and never opens a directory, so
        // cppnix's copy never consults the filter even when one was given
        // (`store-api.cc:124` hands it to `dumpPath` only for `NixArchive`).
        // Walking here would call a filter cppnix does not call, which is
        // observable through a `trace` in it.
        if self.filter.is_none() || self.method == PathMethod::Flat {
            return self.ask(None);
        }
        self.ancestors = ancestors_of(&root);
        self.ancestor = 0;
        self.next_ancestor()
    }

    /// Ask what `ancestors[i]` is, tolerating a path the accessor cannot see.
    ///
    /// # Why this is [`NeedPath::MaybeKind`] and not [`NeedPath::Kind`]
    ///
    /// This scan is cppnix's `SourcePath::resolveSymlinks`, which
    /// `prim_addPath` calls on the root before it copies (`primops.cc:2977`).
    /// That function walks every component of the path and `maybeLstat`s each
    /// one (`source-accessor.cc:91`); a component the accessor cannot see is
    /// nullopt, is recorded as the observation `absent`, and is simply not a
    /// symlink. It never throws for one.
    ///
    /// Asking `Kind` here made an absent ancestor an error instead, and under
    /// pure eval that is every filtered `builtins.path` in existence:
    /// `EvalState::rootFS` is then a mounted accessor holding `/` -> empty
    /// and `/nix/store` -> the store (`eval.cc:294`), so `/nix` -- the first
    /// ancestor of every store path, and `lib.fileset` puts every flake
    /// source there -- resolves to the empty accessor and reads as missing.
    /// `error: path '/nix' does not exist`, before the filter ran once, on 90
    /// of ix's 144 flake attributes. ENG-13123.
    ///
    /// # The other fix, and why it lost
    ///
    /// The scan could have started below the store directory instead, on the
    /// argument that a root already in the store has no ancestor worth
    /// checking. That is false in the direction that matters. `/nix` is
    /// perfectly capable of being a symlink -- a machine with its store on
    /// another volume is the ordinary way -- and cppnix would resolve it and
    /// hand the filter `/data/nix/store/...`. Skipping it would make this
    /// backend hand the filter `/nix/store/...` instead, silently, in exactly
    /// the case the scan exists to refuse: a plausible tree, a different NAR,
    /// a different store path. It also fixes only the store, leaving any
    /// other accessor whose mounts do not cover its ancestors to fail the
    /// same way.
    ///
    /// What that choice costs is a second contract on one read, spelled out
    /// at [`NeedPath::MaybeKind`]: `builtins.readFileType` still needs the
    /// throwing one, so absence crosses the host boundary as a value and the
    /// two callers each say what they make of it. One read, two readers --
    /// which is how cppnix has it, `lstat` being `maybeLstat` plus a throw.
    fn next_ancestor(&mut self) -> Result<Yield> {
        match self.ancestors.get(self.ancestor).cloned() {
            Some(path) => {
                self.phase = PathPhase::Ancestor;
                Ok(Yield::Need(NeedPath::MaybeKind(path)))
            }
            None => Err(VmError::eval("internal: builtins.path lost its root")),
        }
    }

    /// `kind` is `None` when the accessor has no such path. Absent is clean:
    /// see [`PathBuiltin::next_ancestor`].
    fn take_ancestor(&mut self, kind: Option<&str>) -> Result<Yield> {
        let path = self
            .ancestors
            .get(self.ancestor)
            .cloned()
            .ok_or_else(|| VmError::eval("internal: builtins.path lost its root"))?;
        if kind == Some("symlink") {
            return Err(VmError::Unimplemented(Refusal::new(
                RefusalToken::AddPath,
                format!(
                    "{} with a filter and a symlink ('{path}') in its source: cppnix \
                     resolves the root before it walks, so its filter sees the resolved \
                     spelling, and this backend has no question that resolves a symlink \
                     (ENG-12700)",
                    self.builtin
                ),
            )));
        }
        self.ancestor += 1;
        if self.ancestor < self.ancestors.len() {
            return self.next_ancestor();
        }
        // The last ancestor is the root, so this answer is also the root's own
        // type. A file or a symlink has no entries, so the filter is never
        // called and the accepted list is empty rather than absent -- absent
        // would mean "no filtering", which is a different request.
        //
        // A root that is not there takes the same branch, and the copy is
        // still asked for. That is cppnix's order and not a swallowed error:
        // `resolveSymlinks` returns the unresolved path, and the failure
        // comes out of `dumpPath` when `fetchToStore` reads it. Raising it
        // here would move the error earlier than cppnix raises it and would
        // have to invent the wording.
        if kind != Some("directory") {
            return self.ask(Some(Vec::new()));
        }
        self.open(path)
    }

    fn open(&mut self, dir: Rc<PathValue>) -> Result<Yield> {
        self.phase = PathPhase::Entries;
        Ok(Yield::Need(NeedPath::Entries(dir)))
    }

    fn take_entries(&mut self, vm: &mut Vm, value: &Value) -> Result<Yield> {
        // The directory this answers for is the one just descended into: the
        // root when the stack is empty, else the entry the last verdict
        // accepted.
        let dir = match &self.pending {
            Some(entry) => entry.path.clone(),
            None => self
                .root
                .as_ref()
                .map(ToString::to_string)
                .ok_or_else(|| VmError::eval("internal: builtins.path lost its root"))?,
        };
        self.pending = None;
        let attrs = want_attrs(value)?;
        let mut entries: Vec<(String, FileType)> = Vec::with_capacity(attrs.len());
        for (sym, slot) in attrs.iter() {
            let name = vm.sym_name(*sym).to_owned();
            entries.push((
                name,
                parse_file_type(&want_text(&crate::vm::forced(slot)?)?),
            ));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        self.stack.push(DirFrame { dir, entries, i: 0 });
        self.walk()
    }

    /// Advance to the next entry needing a verdict, or finish.
    fn walk(&mut self) -> Result<Yield> {
        loop {
            let Some(frame) = self.stack.last_mut() else {
                let accepted = std::mem::take(&mut self.accepted);
                return self.ask(Some(accepted));
            };
            let Some((name, file_type)) = frame.entries.get(frame.i).cloned() else {
                self.stack.pop();
                continue;
            };
            frame.i += 1;
            let path = format!("{}/{}", frame.dir, name);
            let filter = self
                .filter
                .clone()
                .ok_or_else(|| VmError::eval("internal: builtins.path lost its filter"))?;
            self.pending = Some(AcceptedPath {
                path: path.clone(),
                file_type,
            });
            self.phase = PathPhase::FilterHalf;
            // Two applications, as cppnix's `callPathFilter` makes: the path
            // as a plain string with no context, then the type name.
            return Ok(Yield::Apply(filter, Slot::value(Value::Str(path.into()))));
        }
    }

    fn take_verdict(&mut self, value: &Value) -> Result<Yield> {
        let entry = self
            .pending
            .take()
            .ok_or_else(|| VmError::eval("internal: builtins.path lost its entry"))?;
        if !want_bool(value)? {
            return self.walk();
        }
        let descend = entry.file_type == FileType::Directory;
        let path = entry.path.clone();
        self.accepted.push(entry);
        if descend {
            // Depth first and immediately, which is `dumpPath`'s order: the
            // whole of an accepted subdirectory is filtered before its parent's
            // next entry is. A filter that throws sees that order.
            self.pending = Some(AcceptedPath {
                path: path.clone(),
                file_type: FileType::Directory,
            });
            let root = self
                .root
                .as_ref()
                .ok_or_else(|| VmError::eval("internal: builtins.path lost its root"))?;
            return self.open(Rc::new(root.with_path(path)));
        }
        self.walk()
    }

    fn ask(&mut self, accepted: Option<Vec<AcceptedPath>>) -> Result<Yield> {
        let root = self
            .root
            .clone()
            .ok_or_else(|| VmError::eval("internal: builtins.path lost its root"))?;
        let name = self
            .name
            .clone()
            .ok_or_else(|| VmError::eval("internal: builtins.path lost its name"))?;
        self.phase = PathPhase::Asked;
        Ok(Yield::Need(NeedPath::StoreFiltered(Box::new(
            FilteredCopy {
                root,
                name,
                method: self.method,
                accepted,
                expected_sha256: self.expected_sha256.clone(),
                inherit_references: self.inherit_references,
            },
        ))))
    }
}

/// cppnix's `forceFunction`, as a predicate: a lambda, a primop or partial
/// application of one, or a set carrying `__functor` (`eval.cc:1929`).
fn is_callable(value: &Value, vm: &mut Vm) -> bool {
    match value {
        Value::Closure(_) | Value::Builtin(_) => true,
        Value::Attrs(m) => {
            let functor = vm.intern("__functor");
            m.contains_key(&functor)
        }
        _ => false,
    }
}

/// cppnix's `CanonPath::baseName`: the last component, empty for the root.
fn base_name_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((_, base)) => base.to_owned(),
        None => path.to_owned(),
    }
}

/// Every prefix of an absolute path, shortest first, ending in the path
/// itself. `/a/b` yields `/a` then `/a/b`; `/` yields nothing, which is right
/// -- there is no component to be a symlink.
fn ancestors_of(path: &Rc<PathValue>) -> Vec<Rc<PathValue>> {
    let mut out = Vec::new();
    let (mut acc, suffix) = match &path.root {
        Root::Ambient => (String::new(), path.as_ref().as_ref()),
        Root::Mounted(mount) => (
            mount.to_string(),
            path.as_ref()
                .as_ref()
                .strip_prefix(mount.as_ref())
                .unwrap_or(""),
        ),
    };
    for component in suffix.split('/').filter(|c| !c.is_empty()) {
        acc.push('/');
        acc.push_str(component);
        out.push(Rc::new(path.with_path(acc.clone())));
    }
    if out.is_empty() {
        out.push(Rc::clone(path));
    }
    out
}

/// The inverse of [`FileType::as_str`], for the `Entries` answer, which
/// carries the type as the spelling `builtins.readDir` shows.
fn parse_file_type(name: &str) -> FileType {
    match name {
        "regular" => FileType::Regular,
        "directory" => FileType::Directory,
        "symlink" => FileType::Symlink,
        _ => FileType::Unknown,
    }
}

#[cfg(test)]
mod path_tests {
    use crate::host::{FileType, Host, StoreError};
    use crate::task::FilteredCopy;
    use std::cell::RefCell;

    /// A fixed tree, answered from a table rather than from a disk, plus a
    /// store that records the request and answers with a path derived from
    /// it. Both halves matter: the tree is what the walk sees, and the
    /// recorded request is what the embedder would have been handed.
    ///
    /// ```text
    /// /t          directory
    /// /t/a.txt    regular
    /// /t/link     symlink
    /// /t/skip     directory
    /// /t/skip/c   regular
    /// /t/sub      directory
    /// /t/sub/b    regular
    /// /f          regular
    /// ```
    #[derive(Default)]
    struct Tree(RefCell<Vec<FilteredCopy>>);

    const ENTRIES: &[(&str, &[(&str, FileType)])] = &[
        // Deliberately not in name order. The `Entries` answer is an
        // attrset, so what comes back to the walk is *interner* order, which
        // in a real evaluation is the order names were first seen anywhere --
        // never the directory's. Sorting is what makes the filter see
        // `dumpPath`'s order, and a host that answered pre-sorted would hide
        // a missing sort.
        (
            "/t",
            &[
                ("sub", FileType::Directory),
                ("link", FileType::Symlink),
                ("a.txt", FileType::Regular),
                ("skip", FileType::Directory),
            ],
        ),
        ("/t/skip", &[("c", FileType::Regular)]),
        ("/t/sub", &[("b", FileType::Regular)]),
    ];

    const KINDS: &[(&str, FileType)] = &[
        ("/t", FileType::Directory),
        ("/t/a.txt", FileType::Regular),
        ("/t/link", FileType::Symlink),
        ("/t/skip", FileType::Directory),
        ("/t/skip/c", FileType::Regular),
        ("/t/sub", FileType::Directory),
        ("/t/sub/b", FileType::Regular),
        ("/f", FileType::Regular),
        ("/link-root", FileType::Symlink),
    ];

    impl Host for Tree {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            fetch,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            file_type_resolved,
            get_env,
            copy_to_store,
            ensure_path,
            find_file,
            nix_path,
            trace
        );
        fn read_file(&self, _p: &crate::value2::PathValue) -> std::result::Result<String, String> {
            Ok(String::new())
        }
        fn read_dir(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, FileType)>, String> {
            match ENTRIES.iter().find(|(dir, _)| *dir == p.path.as_ref()) {
                Some((_, items)) => Ok(items
                    .iter()
                    .map(|(name, t)| ((*name).to_owned(), *t))
                    .collect()),
                None => Err(format!("path '{p}' does not exist")),
            }
        }
        fn path_exists_checked(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(KINDS.iter().any(|(path, _)| *path == p.path.as_ref()))
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Option<FileType>, String> {
            match KINDS.iter().find(|(path, _)| *path == p.path.as_ref()) {
                Some((_, t)) => Ok(Some(*t)),
                None => Err(format!("path '{p}' does not exist")),
            }
        }
        fn warn(&self, _message: &str) {}
        fn store_filtered(
            &self,
            request: &FilteredCopy,
        ) -> std::result::Result<String, StoreError> {
            self.0.borrow_mut().push(request.clone());
            Ok(format!(
                "/nix/store/0000000000000000000000000000000a-{}",
                request.name
            ))
        }
    }

    /// The rendered result and every request the store was handed.
    fn run(src: &str) -> (String, Vec<FilteredCopy>) {
        let host = Tree::default();
        let rendered = crate::eval::render_with(&crate::eval::settings_with_store(), &host, src);
        let asked = host.0.borrow().clone();
        (rendered, asked)
    }

    fn only(asked: &[FilteredCopy]) -> FilteredCopy {
        super::only_request("copy request", asked)
    }

    /// The accepted paths of the single request `src` made.
    fn accepted(src: &str) -> Vec<String> {
        let (_, asked) = run(src);
        let request = only(&asked);
        match &request.accepted {
            None => vec!["unfiltered".to_owned()],
            Some(list) => list.iter().map(|e| e.path.clone()).collect(),
        }
    }

    /// No `filter` means no walk at all, which is not an optimisation: cppnix
    /// passes `defaultPathFilter` and never opens a directory to consult it,
    /// so walking here would be this backend reading a tree cppnix's
    /// evaluator never reads -- visible in a read set, and O(tree) per call.
    #[test]
    fn an_unfiltered_copy_is_one_question_and_no_walk() {
        let (rendered, asked) = run("builtins.path { path = /t; }");
        assert_eq!(
            rendered,
            "\"/nix/store/0000000000000000000000000000000a-t\""
        );
        let request = only(&asked);
        assert_eq!(request.root.path.as_ref(), "/t");
        // cppnix defaults the name to the root's base name.
        assert_eq!(request.name, "t");
        assert_eq!(request.accepted, None);
        assert_eq!(request.method, crate::task::PathMethod::NixArchive);
        assert_eq!(request.expected_sha256, None);
    }

    /// `name` and `recursive` are the two attributes that change the store
    /// object rather than its contents.
    #[test]
    fn the_name_and_the_method_reach_the_request() {
        let (_, asked) = run(r#"builtins.path { path = /f; name = "custom"; recursive = false; }"#);
        let request = only(&asked);
        assert_eq!(request.name, "custom");
        assert_eq!(request.method, crate::task::PathMethod::Flat);
    }

    /// `path` is coerced, not merely forced: cppnix's `prim_path` reads it
    /// with `coerceToPath`, so a set naming a path through `outPath` or
    /// `__toString` is a valid `path` there and has to be one here.
    ///
    /// This runs through the crate's one [`coerce_to_path`] (ENG-12669)
    /// rather than a copy local to this builtin, which is why the set case
    /// works at all -- it needs `__toString` applied, which needs the
    /// machine, and a local "must be a string" check would have rejected it.
    #[test]
    fn a_set_naming_a_path_is_coerced_the_way_the_rest_of_the_family_coerces_one() {
        for src in [
            "builtins.path { path = { outPath = /t; }; }",
            r#"builtins.path { path = { __toString = _: "/t"; }; }"#,
            // Through the machine *and* then walked, which is the pair that
            // would break if the coercion's second stage were mishandled.
            "builtins.path { path = { outPath = /t; }; filter = p: t: true; }",
        ] {
            let (_, asked) = run(src);
            assert_eq!(only(&asked).root.path.as_ref(), "/t", "{src}");
        }
        // The name still defaults to the coerced path's base name.
        let (_, asked) = run("builtins.path { path = { outPath = /t/sub; }; }");
        assert_eq!(only(&asked).name, "sub");
    }

    /// The whole of the walk: pre-order, name-sorted, an accepted directory
    /// descended into immediately, and a rejected one pruned with its subtree.
    ///
    /// `dumpPath` copies the directory into a `std::map` before iterating, so
    /// the order is the names' and not readdir's; a filter that traces sees
    /// it, and so does the store path when the filter is order-dependent.
    #[test]
    fn the_walk_is_cppnixs_pre_order_and_a_rejected_directory_prunes_its_subtree() {
        assert_eq!(
            accepted("builtins.path { path = /t; filter = p: t: true; }"),
            vec![
                "/t/a.txt",
                "/t/link",
                "/t/skip",
                "/t/skip/c",
                "/t/sub",
                "/t/sub/b"
            ]
        );
        assert_eq!(
            accepted(r#"builtins.path { path = /t; filter = p: t: baseNameOf p != "skip"; }"#),
            vec!["/t/a.txt", "/t/link", "/t/sub", "/t/sub/b"]
        );
        assert_eq!(
            accepted(r#"builtins.path { path = /t; filter = p: t: t != "symlink"; }"#),
            vec!["/t/a.txt", "/t/skip", "/t/skip/c", "/t/sub", "/t/sub/b"]
        );
        // The root itself is never offered to the filter, so rejecting
        // everything still names a directory: an empty one on the NAR road,
        // jj's empty tree on the tree-id road.
        assert_eq!(
            accepted("builtins.path { path = /t; filter = p: t: false; }"),
            Vec::<String>::new()
        );
    }

    /// The filter's two arguments, in cppnix's spelling: the absolute path as
    /// a plain string, and the `readFileType` name of what it is.
    #[test]
    fn the_filter_sees_the_path_and_the_type_cppnix_passes() {
        assert_eq!(
            accepted(
                r#"builtins.path { path = /t; filter = p: t: p == "/t/sub" && t == "directory"; }"#
            ),
            vec!["/t/sub"]
        );
    }

    /// A file root has no entries, so the filter is never called -- and the
    /// request still says "filtered", because "no filtering" is a different
    /// request the embedder answers differently for a directory.
    #[test]
    fn a_file_root_with_a_filter_asks_for_an_empty_accepted_list() {
        let (_, asked) =
            run(r#"builtins.path { path = /f; filter = p: t: throw "never called"; }"#);
        assert_eq!(only(&asked).accepted, Some(Vec::new()));
    }

    /// cppnix's flat ingestion hands the filter to nothing: `hashPath` with
    /// `FileSerialisationMethod::Flat` reads one file's bytes. Calling it here
    /// would run user code cppnix does not run.
    #[test]
    fn a_flat_copy_never_calls_the_filter() {
        let (rendered, asked) = run(
            r#"builtins.path { path = /f; recursive = false; filter = p: t: throw "never called"; }"#,
        );
        assert_eq!(
            rendered,
            "\"/nix/store/0000000000000000000000000000000a-f\""
        );
        assert_eq!(only(&asked).accepted, None);
    }

    /// A throwing filter propagates, and the throw arrives from the first
    /// entry rather than after the walk finished.
    #[test]
    fn a_throwing_filter_propagates() {
        let (rendered, asked) =
            run(r#"builtins.path { path = /t; filter = p: t: throw "boom ${p}"; }"#);
        // The first entry in name order, so the throw comes from where
        // cppnix's walk would have reached first rather than after the tree.
        assert!(rendered.contains("boom /t/a.txt"), "{rendered}");
        assert!(asked.is_empty(), "no copy should have been asked for");
    }

    /// cppnix's `forceBool` on the filter's result, with its wording.
    #[test]
    fn a_filter_returning_a_non_bool_is_cppnixs_error() {
        let (rendered, _) = run("builtins.path { path = /t; filter = p: t: 3; }");
        assert!(
            rendered.contains("expected a Boolean but found an integer"),
            "{rendered}"
        );
    }

    /// The three argument-shape errors, in cppnix's words. They are what a
    /// user sees, and the corpus compares the text.
    #[test]
    fn the_argument_errors_are_cppnixs() {
        for (src, wanted) in [
            (
                "builtins.path { path = /t; nonesuch = 1; }",
                "unsupported argument 'nonesuch' to 'builtins.path'",
            ),
            (
                r#"builtins.path { name = "x"; }"#,
                "missing required 'path' attribute in the first argument to 'builtins.path'",
            ),
            (
                "builtins.path { path = /t; filter = 3; }",
                "expected a function but found an integer",
            ),
            (
                "builtins.path { path = /t; name = 3; }",
                "expected a string but found an integer",
            ),
            (
                "builtins.path { path = 3; }",
                "cannot coerce an integer to a string",
            ),
            (
                r#"builtins.path { path = /t; sha256 = "zzz"; }"#,
                "hash 'zzz' has wrong length for hash algorithm 'sha256'",
            ),
            ("builtins.path 3", "expected a set but found an integer"),
        ] {
            let (rendered, _) = run(src);
            assert!(
                rendered.contains(wanted),
                "{src}: wanted {wanted:?}, got {rendered}"
            );
        }
    }

    /// cppnix parses `sha256` at evaluation time and substitutes the all-zero
    /// hash for an empty one, warning as it does. The embedder receives one
    /// spelling, SRI, so it never has to guess which encoding it was handed.
    #[test]
    fn the_expected_hash_reaches_the_request_as_sri() {
        let (_, asked) = run(
            r#"builtins.path { path = /t; sha256 = "1BdlSaqjNlSVCcgD/PocqAwbnGQ+lyfL6h9WK6+MCJc="; }"#,
        );
        assert_eq!(
            only(&asked).expected_sha256.as_deref(),
            Some("sha256-1BdlSaqjNlSVCcgD/PocqAwbnGQ+lyfL6h9WK6+MCJc=")
        );
        // An empty attribute is not an absent one: cppnix substitutes the
        // zero hash, which then almost certainly fails the mismatch check.
        let (_, asked) = run(r#"builtins.path { path = /t; sha256 = ""; }"#);
        assert_eq!(
            only(&asked).expected_sha256.as_deref(),
            Some("sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        );
    }

    /// The refusal that keeps a filtered copy honest. cppnix resolves the root
    /// before it walks, so its filter sees the resolved spelling; a filter
    /// that compares the path prefix would then see different strings here and
    /// could accept a different set, which is a different store path.
    #[test]
    fn a_symlinked_root_with_a_filter_refuses_by_name() {
        let (rendered, asked) = run("builtins.path { path = /link-root; filter = p: t: true; }");
        assert!(
            rendered.contains("AddPath") && rendered.contains("symlink ('/link-root')"),
            "{rendered}"
        );
        assert!(asked.is_empty(), "nothing should have been copied");
        // Without a filter nothing observes the difference: the embedder
        // resolves the root exactly as cppnix does.
        let (_, asked) = run("builtins.path { path = /link-root; }");
        assert_eq!(only(&asked).root.path.as_ref(), "/link-root");
    }

    /// Every directory the walk opened is an `Entries` question, so a
    /// filtered copy is as visible to a read set as `builtins.readDir` is.
    /// Without this the only recorded fact would be the copy's answer, and a
    /// file appearing in a walked directory would not move the key.
    #[test]
    fn the_walk_is_visible_to_a_read_set() {
        let inner = Tree::default();
        let host = crate::readset::RecordingHost::new(&inner);
        let Ok(module) = crate::compile::compile_source(
            "builtins.path { path = /t; filter = p: t: true; }",
            "/m",
            crate::compile::Origin::String,
            &crate::eval::settings_with_store(),
        ) else {
            unreachable!("the source parses")
        };
        let mut vm = crate::vm::Vm::with_settings(crate::eval::settings_with_store());
        vm.start_module(&std::rc::Rc::new(module));
        let _ = crate::eval::drive(&mut vm, &host);
        let asked = format!("{:?}", host.take().questions());
        for dir in ["/t", "/t/skip", "/t/sub"] {
            assert!(
                asked.contains(&format!("ReadDir({:?})", crate::value2::ambient_path(dir))),
                "{dir} missing from {asked}"
            );
        }
        assert!(
            asked.contains("StoreFiltered"),
            "the copy itself is missing from {asked}"
        );
    }

    /// Without a store the builtin says so rather than answering with a path
    /// nobody archived -- the same refusal `builtins.toFile` makes, and for
    /// the same reason.
    #[test]
    fn without_a_store_the_copy_refuses_by_name() {
        struct NoStore;
        impl Host for NoStore {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Ok(String::new())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Ok(Some(FileType::Directory))
            }
        }
        let Ok(module) = crate::compile::compile_source(
            "builtins.path { path = /t; }",
            "/m",
            crate::compile::Origin::String,
            &crate::eval::settings_with_store(),
        ) else {
            unreachable!("the source parses")
        };
        let mut vm = crate::vm::Vm::with_settings(crate::eval::settings_with_store());
        vm.start_module(&std::rc::Rc::new(module));
        let got = crate::eval::drive(&mut vm, &NoStore);
        assert!(
            matches!(&got, Err(crate::vm::VmError::Unimplemented(r))
                if r.detail == "a filtered copy into the store (no store behind this evaluator)"),
            "{got:?}"
        );
    }

    // -- builtins.filterSource, which is this machine under another name ----

    /// The claim the implementation rests on: `filterSource f p` and
    /// `builtins.path { path = p; filter = f; }` are the same `addPath` call,
    /// so they must produce the same request down to the last accepted entry.
    ///
    /// Asserted against the *whole* request rather than a field or two,
    /// because a divergence between the two spellings shows up in `accepted`
    /// -- the part that decides the store path -- while root, name and
    /// method still agree.
    ///
    /// What it does not catch, checked by breaking it: a bug in the walk
    /// itself. Deleting the name sort in `take_entries` leaves both arms
    /// equally wrong and this test green. That is the correct division --
    /// `the_walk_is_cppnixs_pre_order_and_a_rejected_directory_prunes_its_subtree`
    /// and `a_throwing_filter_propagates` are the two that fail on that
    /// break, and they own the walk. This one owns the claim that there is
    /// only one of it.
    #[test]
    fn filter_source_asks_exactly_what_the_path_spelling_asks() {
        for (positional, attrs) in [
            (
                "builtins.filterSource (p: t: true) /t",
                "builtins.path { path = /t; filter = p: t: true; }",
            ),
            (
                r#"builtins.filterSource (p: t: baseNameOf p != "skip") /t"#,
                r#"builtins.path { path = /t; filter = p: t: baseNameOf p != "skip"; }"#,
            ),
            (
                r#"builtins.filterSource (p: t: t != "symlink") /t"#,
                r#"builtins.path { path = /t; filter = p: t: t != "symlink"; }"#,
            ),
            // A file root: no entries, so an empty accepted list on both.
            (
                r#"builtins.filterSource (p: t: throw "never called") /f"#,
                r#"builtins.path { path = /f; filter = p: t: throw "never called"; }"#,
            ),
        ] {
            let (left_rendered, left) = run(positional);
            let (right_rendered, right) = run(attrs);
            assert_eq!(left_rendered, right_rendered, "{positional}");
            assert_eq!(only(&left), only(&right), "{positional}");
        }
    }

    /// The three `addPath` arguments `filterSource` fixes, which are the
    /// whole of its difference from `builtins.path`: the name is the root's
    /// base name with no way to override it, the method is always
    /// `NixArchive`, and there is no expected hash.
    ///
    /// The name in particular is not cosmetic. It is the base name of the
    /// *unfiltered* directory and it lands in the store path, which is the
    /// warning cppnix prints in `filterSource`'s own manual entry.
    #[test]
    fn filter_source_fixes_the_name_the_method_and_the_hash() {
        let (_, asked) = run("builtins.filterSource (p: t: true) /t/sub");
        let request = only(&asked);
        assert_eq!(request.name, "sub");
        assert_eq!(request.method, crate::task::PathMethod::NixArchive);
        assert_eq!(request.expected_sha256, None);
    }

    /// cppnix coerces argument 1 with the same `coerceToPath` the rest of the
    /// path family uses, so a set naming a path is a valid source here too --
    /// and it arrives through the machine, since `__toString` has to run.
    #[test]
    fn filter_source_coerces_its_source_the_way_the_family_does() {
        for src in [
            "builtins.filterSource (p: t: true) { outPath = /t; }",
            r#"builtins.filterSource (p: t: true) { __toString = _: "/t"; }"#,
        ] {
            let (_, asked) = run(src);
            assert_eq!(only(&asked).root.path.as_ref(), "/t", "{src}");
        }
    }

    /// cppnix coerces the source before it forces the filter
    /// (`primops.cc:3007` then `:3012`), so a source that throws is what a
    /// program with both problems sees. Reversing the two -- which is what a
    /// `strict` entry for position 0 would do -- reports the filter instead.
    #[test]
    fn the_source_is_read_before_the_filter_is_checked() {
        let (rendered, asked) = run(r#"builtins.filterSource 42 (throw "the source")"#);
        assert!(rendered.contains("the source"), "{rendered}");
        assert!(asked.is_empty(), "nothing should have been copied");
        // And with a sound source, the filter's type is cppnix's error.
        let (rendered, _) = run("builtins.filterSource 42 /t");
        assert!(
            rendered.contains("expected a function but found an integer"),
            "{rendered}"
        );
    }

    /// Both refusals `addPath` can make name `filterSource` when
    /// `filterSource` asked, rather than naming `builtins.path` because that
    /// is where the machine is written.
    #[test]
    fn the_refusals_name_the_builtin_that_asked() {
        let (rendered, asked) = run("builtins.filterSource (p: t: true) /link-root");
        assert!(
            rendered.contains("AddPath")
                && rendered.contains("builtins.filterSource with a filter and a symlink"),
            "{rendered}"
        );
        assert!(asked.is_empty(), "nothing should have been copied");
    }
}

// -- fetchurl and fetchTarball ----------------------------------------------

/// The fixed-output fetchers: `builtins.fetchurl` and `builtins.fetchTarball`,
/// which are cppnix's single `fetch()` (`primops/fetchTree.cc:462`) under two
/// names and one `unpack` flag.
///
/// # What runs here and what does not
///
/// Everything cppnix does *before* it touches the world runs here, because
/// all of it is either an evaluation the interpreter has to drive or a pure
/// string rule whose failure the program can see:
///
/// * reading the argument -- a bare string URL, or an attribute set whose
///   three recognised attributes are forced one at a time in the set's own
///   order, with anything else an error;
/// * `resolvePseudoUrl`, which rewrites a `channel:` URL, for the tarball
///   case only;
/// * defaulting the name (`baseNameOf` the URL for `fetchurl`, `"source"` for
///   `fetchTarball`) and putting it through
///   [`crate::storepath::check_name`];
/// * parsing `sha256` through `newHashAllowEmpty`, which turns an empty
///   attribute into the all-zero hash and warns.
///
/// Everything after that is one [`NeedPath::Fetch`]: the download, the
/// substituter lookup, the pinned-path short circuit and the hash check are
/// `libfetchers`' job and the embedder links it. See that variant's doc for
/// the guarantees the answer has to meet.
///
/// # Where this is not cppnix
///
/// * Attributes are read in this crate's symbol order, which is its interning
///   order, exactly as cppnix reads them in its own. The two histories differ,
///   so when *two* attributes are malformed the pair may disagree about which
///   one is reported -- the same caveat [`PathBuiltin`] carries.
/// * `checkURI` is not applied here. It reads `restrict-eval`, which is the
///   embedder's setting, and under either purity setting this evaluator
///   refuses the whole question channel before the fetch is asked (see
///   `answer_path`). So under `restrict-eval` cppnix reports a forbidden URI
///   where this reports a named refusal, and an expression that is *both*
///   restricted and badly named may be reported against the other fault.
/// * Under `pure-eval` cppnix serves a pinned fetch from the store and
///   refuses an unpinned one by name; this backend refuses both, with
///   [`RefusalToken::AccessControl`]. That is a refusal against a value, so
///   it is a named gap rather than a wrong answer -- and it is the gap that
///   has to close before a flake can evaluate here at all, since `nix eval
///   <flake>#x` is pure by default.
pub struct FetchBuiltin {
    kind: FetchKind,
    /// Recognized-or-not attributes still to read, in the argument set's own
    /// order. Popped one at a time for the reason [`PathBuiltin`] pops its
    /// queue: cppnix reports the first unsupported attribute only after
    /// evaluating the recognized ones that precede it.
    queue: VecDeque<(Sym, Slot)>,
    current: Option<FetchAttr>,
    phase: FetchPhase,
    /// Whether the argument was an attribute set. Not derivable from the
    /// queue being empty -- `builtins.fetchurl {}` is an empty set and a
    /// bare URL string is not a set at all -- and it picks which of three
    /// resolutions a bad name is reported with.
    from_attrs: bool,
    name_attr_passed: bool,
    url: Option<String>,
    name: Option<String>,
    expected_sha256: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FetchAttr {
    Url,
    Sha256,
    Name,
}

impl FetchAttr {
    fn of(name: &str) -> Option<FetchAttr> {
        match name {
            "url" => Some(FetchAttr::Url),
            "sha256" => Some(FetchAttr::Sha256),
            "name" => Some(FetchAttr::Name),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FetchPhase {
    /// Reading the argument set, one attribute at a time.
    Attr,
    /// A warning is out; the next step resumes the attribute walk.
    Warned,
    /// The fetch is out; the next value is its answer.
    Asked,
}

pub fn bi_fetchurl(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    begin_fetch(vm, args, FetchKind::File)
}

pub fn bi_fetch_tarball(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    begin_fetch(vm, args, FetchKind::Tarball)
}

fn begin_fetch(_vm: &mut Vm, args: &[Slot], kind: FetchKind) -> Result<Begin> {
    let arg = argv(args, 0)?;
    // cppnix's default `name`, before any attribute: empty for `fetchurl`,
    // which makes it fall back to the URL's base name, and "source" for
    // `fetchTarball`, which does not.
    let default_name = match kind {
        FetchKind::File => String::new(),
        FetchKind::Tarball => "source".to_owned(),
    };
    let mut machine = FetchBuiltin {
        kind,
        queue: VecDeque::new(),
        current: None,
        phase: FetchPhase::Attr,
        from_attrs: false,
        name_attr_passed: false,
        url: None,
        name: Some(default_name),
        expected_sha256: None,
    };
    if let Value::Attrs(attrs) = &arg {
        machine.from_attrs = true;
        machine.queue = attrs.iter().map(|(k, v)| (*k, v.clone())).collect();
        return Ok(Begin::Cont(Cont::Ext(Ext::Fetch(Box::new(machine)))));
    }
    // Not a set: cppnix reads the whole argument as the URL, with
    // `forceStringNoCtx`'s error for anything that is not a string. Already
    // forced by the argument table, so the error is raised here and the
    // machine starts with an empty queue, whose first step is the question.
    machine.url = Some(want_text_no_ctx(&arg)?);
    Ok(Begin::Cont(Cont::Ext(Ext::Fetch(Box::new(machine)))))
}

impl FetchBuiltin {
    fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        match (self.phase, incoming) {
            (FetchPhase::Asked, Some(v)) => Ok(Yield::Done(v)),
            (FetchPhase::Asked, None) => Err(VmError::eval(format!(
                "internal: builtins.{} lost its answer",
                self.kind.who()
            ))),
            (FetchPhase::Attr, None) => self.next_attr(vm),
            (FetchPhase::Attr, Some(v)) => self.take_attr(vm, v),
            (FetchPhase::Warned, _) => self.next_attr(vm),
        }
    }

    fn next_attr(&mut self, vm: &mut Vm) -> Result<Yield> {
        self.phase = FetchPhase::Attr;
        let Some((sym, slot)) = self.queue.pop_front() else {
            self.current = None;
            return self.ask();
        };
        let name = vm.sym_name(sym);
        let Some(attr) = FetchAttr::of(name) else {
            return Err(VmError::eval(format!(
                "unsupported argument '{name}' to '{}'",
                self.kind.who()
            )));
        };
        self.current = Some(attr);
        Ok(Yield::Force(slot))
    }

    fn take_attr(&mut self, vm: &mut Vm, value: Value) -> Result<Yield> {
        let attr = self.current.ok_or_else(|| {
            VmError::eval(format!(
                "internal: builtins.{} lost an attribute",
                self.kind.who()
            ))
        })?;
        match attr {
            FetchAttr::Url => self.url = Some(want_text_no_ctx(&value)?),
            FetchAttr::Name => {
                self.name_attr_passed = true;
                self.name = Some(want_text_no_ctx(&value)?);
            }
            FetchAttr::Sha256 => {
                let text = want_text_no_ctx(&value)?;
                // Parsed here rather than passed on raw, for the reason
                // `builtins.path` parses its own: a malformed hash is an
                // evaluation error where cppnix raises one, and the embedder
                // receives one unambiguous spelling.
                let (hash, warning) = crate::nixhash::new_hash_allow_empty(
                    &text,
                    Some(crate::nixhash::HashAlgo::Sha256),
                    false,
                )
                .map_err(|e| VmError::eval(e.to_string()))?;
                self.expected_sha256 = Some(hash.to_sri());
                if let Some(message) = warning {
                    self.phase = FetchPhase::Warned;
                    return Ok(Yield::Need(NeedPath::Warn(message)));
                }
            }
        }
        self.next_attr(vm)
    }

    /// Everything cppnix's `fetch()` does between the last attribute and the
    /// first byte of IO, then the question.
    fn ask(&mut self) -> Result<Yield> {
        let who = self.kind.who();
        let Some(url) = self.url.clone() else {
            // Only reachable from the attribute-set branch: a bare argument
            // is the URL.
            return Err(VmError::eval("'url' argument required"));
        };
        // `channel:foo` names a nixpkgs channel tarball, and only
        // `fetchTarball` rewrites it. Applied before the name defaults,
        // because the base name is taken from the rewritten URL.
        let url = match self.kind {
            FetchKind::Tarball => resolve_pseudo_url(&url),
            FetchKind::File => url,
        };
        let mut name = self.name.clone().unwrap_or_default();
        if name.is_empty() {
            name = url_base_name(&url).to_owned();
        }
        if let Err(message) = crate::storepath::check_name(&name) {
            // cppnix picks one of three resolutions by how the caller could
            // fix it, which is the whole value of the message: a bare URL
            // argument has no `name` to correct.
            let resolution = if self.name_attr_passed {
                format!(
                    "Please change the value for the 'name' attribute passed to '{who}', \
                     so that it can create a valid store path."
                )
            } else if self.from_attrs {
                format!(
                    "Please add a valid 'name' attribute to the argument for '{who}', \
                     so that it can create a valid store path."
                )
            } else {
                format!(
                    "Please pass an attribute set with 'url' and 'name' attributes to \
                     '{who}',  so that it can create a valid store path."
                )
            };
            return Err(VmError::eval(format!(
                "invalid store path name when fetching URL '{url}': {message}. {resolution}"
            )));
        }
        self.phase = FetchPhase::Asked;
        Ok(Yield::Need(NeedPath::Fetch(Box::new(FetchRequest {
            url,
            name,
            kind: self.kind,
            expected_sha256: self.expected_sha256.clone(),
        }))))
    }
}

/// `EvalSettings::resolvePseudoUrl` (`eval-settings.cc:123`): the one
/// non-URL spelling `fetchTarball` accepts.
fn resolve_pseudo_url(url: &str) -> String {
    match url.strip_prefix("channel:") {
        Some(rest) => format!("https://channels.nixos.org/{rest}/nixexprs.tar.xz"),
        None => url.to_owned(),
    }
}

/// cppnix's `baseNameOf` (`file-system.cc:137`) -- the utility, **not** the
/// `legacyBaseNameOf` that `builtins.baseNameOf` is.
///
/// The difference is load-bearing here because the result becomes a store
/// path's name: the utility strips every trailing separator, the legacy one
/// strips at most a single character and answers "" for `"a//"`. `fetch()`
/// calls the utility, so a URL ending in a slash names its last real
/// component rather than nothing at all.
fn url_base_name(url: &str) -> &str {
    if url.is_empty() {
        return "";
    }
    let b = url.as_bytes();
    let mut last = b.len() - 1;
    while last > 0 && b.get(last) == Some(&b'/') {
        last -= 1;
    }
    // `rfind` over the prefix up to and including `last`, which is what
    // cppnix's `rfindPathSep(path, last)` does.
    let head = url.get(..=last).unwrap_or(url);
    let pos = match head.rfind('/') {
        None => 0,
        Some(i) => i + 1,
    };
    url.get(pos..=last).unwrap_or("")
}

#[cfg(test)]
mod fetch_tests {
    use crate::host::{FileType, Host, StoreError};
    use crate::task::{FetchKind, FetchRequest};
    use std::cell::RefCell;

    /// A store that records the fetch it was handed and answers with a path
    /// derived from it. The path is not a real fixed-output path and does
    /// not have to be: what these tests check is the *question*, which is
    /// the whole of the evaluator's contribution -- the store path is
    /// cppnix's arithmetic, gated byte for byte by
    /// `maintainers/ix/fetch-parity.sh` against a real store.
    #[derive(Default)]
    struct Downloads {
        asked: RefCell<Vec<FetchRequest>>,
        warnings: RefCell<Vec<String>>,
    }

    impl Host for Downloads {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            file_type_resolved,
            get_env,
            copy_to_store,
            ensure_path,
            find_file,
            nix_path,
            trace
        );
        fn read_file(&self, p: &crate::value2::PathValue) -> std::result::Result<String, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn read_dir(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, FileType)>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn path_exists_checked(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(false)
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Option<FileType>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn warn(&self, message: &str) {
            self.warnings.borrow_mut().push(message.to_owned());
        }
        fn fetch(&self, request: &FetchRequest) -> std::result::Result<String, StoreError> {
            self.asked.borrow_mut().push(request.clone());
            Ok(format!(
                "/nix/store/0000000000000000000000000000000a-{}",
                request.name
            ))
        }
    }

    /// The rendered result, every fetch the store was handed, and every
    /// warning emitted.
    fn run(src: &str) -> (String, Vec<FetchRequest>, Vec<String>) {
        let host = Downloads::default();
        let rendered = crate::eval::render_with(&crate::eval::Settings::default(), &host, src);
        let asked = host.asked.borrow().clone();
        let warnings = host.warnings.borrow().clone();
        (rendered, asked, warnings)
    }

    fn only(src: &str) -> FetchRequest {
        let (rendered, asked, _) = run(src);
        super::only_request(&format!("fetch from {src:?} (result: {rendered})"), &asked)
    }

    fn fails_with(src: &str) -> String {
        let (rendered, asked, _) = run(src);
        assert!(
            asked.is_empty(),
            "{src:?} reached the store, which it should not have: {asked:?}"
        );
        rendered
    }

    /// A bare string argument is the URL, and the name falls back to the
    /// URL's base name -- which is what makes `builtins.fetchurl
    /// "https://.../hello-2.12.3.tar.gz"` land on a path named for the
    /// tarball.
    #[test]
    fn a_bare_url_names_itself() {
        let r = only(r#"builtins.fetchurl "https://example.invalid/a/hello-2.12.3.tar.gz""#);
        assert_eq!(r.url, "https://example.invalid/a/hello-2.12.3.tar.gz");
        assert_eq!(r.name, "hello-2.12.3.tar.gz");
        assert_eq!(r.kind, FetchKind::File);
        assert_eq!(r.expected_sha256, None);
    }

    /// cppnix's two defaults differ, and getting the tarball one wrong is a
    /// wrong *store path* rather than a wrong message: the name is hashed
    /// into it.
    #[test]
    fn fetch_tarball_defaults_its_name_to_source() {
        let r = only(r#"builtins.fetchTarball "https://example.invalid/a/nixpkgs.tar.gz""#);
        assert_eq!(r.name, "source");
        assert_eq!(r.kind, FetchKind::Tarball);
    }

    /// `fetch()` calls the `baseNameOf` *utility*, which strips every
    /// trailing separator, and not the `legacyBaseNameOf` that
    /// `builtins.baseNameOf` is, which strips at most one and answers "" for
    /// `"a//"`. Reusing the wrong one gives an empty name and so a
    /// `checkName` failure on a URL cppnix accepts.
    #[test]
    fn the_url_base_name_is_the_utility_not_the_legacy_one() {
        assert_eq!(
            only(r#"builtins.fetchurl "https://example.invalid/pkg//""#).name,
            "pkg"
        );
        assert_eq!(super::url_base_name("https://x/a/"), "a");
        assert_eq!(super::url_base_name("https://x/a//"), "a");
        assert_eq!(super::url_base_name("/"), "");
        assert_eq!(super::url_base_name(""), "");
        assert_eq!(super::url_base_name("bare"), "bare");
    }

    /// An explicit `name` wins over both defaults, and a `sha256` arrives
    /// re-rendered as SRI whatever spelling it was written in.
    #[test]
    fn the_attribute_set_form_carries_all_three() {
        let r = only(
            r#"builtins.fetchurl {
                 url = "https://example.invalid/x";
                 name = "chosen-name";
                 sha256 = "1x0wgim2s6xdrsnhh3vjmc8b2n3srgpxk0k8vp9wpj0qgzdcgvvi";
               }"#,
        );
        assert_eq!(r.name, "chosen-name");
        assert_eq!(
            r.expected_sha256.as_deref(),
            // The same hash the oracle renders:
            //   nix hash convert --hash-algo sha256 --to sri \
            //     1x0wgim2s6xdrsnhh3vjmc8b2n3srgpxk0k8vp9wpj0qgzdcgvvi
            Some("sha256-ce/H2n8YyMvT3WiC2e/LelixEKtyDwitzq0bLWp8HPQ=")
        );
    }

    /// `channel:x` is a nixpkgs channel, and only `fetchTarball` rewrites
    /// it -- `resolvePseudoUrl` is called from `fetch()` under a `who ==
    /// "fetchTarball"` test.
    ///
    /// The `fetchurl` half is checked through a `name` attribute rather than
    /// a bare URL, because a bare `builtins.fetchurl "channel:x"` dies before
    /// the question: the base name of an unrewritten `channel:` URL is the
    /// whole string, whose colon `checkName` rejects. cppnix fails it the
    /// same way, which is the second assertion here.
    #[test]
    fn only_fetch_tarball_resolves_a_pseudo_url() {
        assert_eq!(
            only(r#"builtins.fetchTarball "channel:nixos-24.05""#).url,
            "https://channels.nixos.org/nixos-24.05/nixexprs.tar.xz"
        );
        assert_eq!(
            only(r#"builtins.fetchurl { url = "channel:nixos-24.05"; name = "n"; }"#).url,
            "channel:nixos-24.05"
        );
        assert!(
            fails_with(r#"builtins.fetchurl "channel:nixos-24.05""#)
                .contains("contains illegal character ':'")
        );
    }

    /// cppnix reports the primop's bare name, not `builtins.fetchurl`, and
    /// the two differ per fetcher.
    #[test]
    fn an_unsupported_attribute_names_the_fetcher() {
        assert!(
            fails_with(r#"builtins.fetchurl { url = "https://x/y"; rev = "abc"; }"#)
                .contains("unsupported argument 'rev' to 'fetchurl'"),
            "got {}",
            fails_with(r#"builtins.fetchurl { url = "https://x/y"; rev = "abc"; }"#)
        );
        assert!(
            fails_with(r#"builtins.fetchTarball { rev = "abc"; }"#)
                .contains("unsupported argument 'rev' to 'fetchTarball'")
        );
    }

    #[test]
    fn an_attribute_set_without_a_url_is_an_error() {
        assert!(fails_with("builtins.fetchurl { }").contains("'url' argument required"));
    }

    /// The three resolutions cppnix picks between. They are the whole value
    /// of the message -- a bare URL argument has no `name` attribute to
    /// correct, so telling its caller to change one would be wrong advice.
    #[test]
    fn a_bad_name_is_reported_with_the_resolution_that_fits() {
        let from_bare = fails_with(r#"builtins.fetchurl "https://example.invalid/a b""#);
        assert!(
            from_bare.contains("invalid store path name when fetching URL")
                && from_bare.contains("contains illegal character ' '")
                && from_bare.contains("Please pass an attribute set with 'url' and 'name'"),
            "got {from_bare}"
        );

        let from_attrs =
            fails_with(r#"builtins.fetchurl { url = "https://example.invalid/a b"; }"#);
        assert!(
            from_attrs.contains("Please add a valid 'name' attribute"),
            "got {from_attrs}"
        );

        let from_name =
            fails_with(r#"builtins.fetchurl { url = "https://x/y"; name = "no spaces here"; }"#);
        assert!(
            from_name.contains("Please change the value for the 'name' attribute"),
            "got {from_name}"
        );
    }

    /// An empty `sha256` is not an absent one: cppnix substitutes the
    /// all-zero hash and warns, and the fetch then goes out *pinned* to a
    /// path that will not match. Serving it as unpinned would silently turn
    /// a mistake into a live download.
    #[test]
    fn an_empty_sha256_becomes_the_zero_hash_and_warns() {
        let src = r#"builtins.fetchurl { url = "https://example.invalid/x"; sha256 = ""; }"#;
        let r = only(src);
        assert_eq!(
            r.expected_sha256.as_deref(),
            // cppnix's `Hash(algo)` is zero-initialised, not the digest of
            // the empty string. Confirmed against the oracle:
            //   nix hash convert --hash-algo sha256 --to sri 000...0
            Some("sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        );
        let (_, _, warnings) = run(src);
        assert_eq!(
            warnings,
            vec![
                "found empty hash, assuming 'sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='"
            ]
        );
    }

    #[test]
    fn a_malformed_sha256_is_an_evaluation_error() {
        assert!(
            fails_with(r#"builtins.fetchurl { url = "https://x/y"; sha256 = "nope"; }"#)
                .contains("hash"),
            "got {}",
            fails_with(r#"builtins.fetchurl { url = "https://x/y"; sha256 = "nope"; }"#)
        );
    }

    /// The result is a string carrying the fetched path as its own context,
    /// which is what lets `import (fetchTarball ...)` and a derivation
    /// attribute both work. A plain string would lose the dependency.
    #[test]
    fn the_answer_is_a_string_carrying_the_store_path() {
        // Quoted, because this is the printer's rendering of a string --
        // which is itself the assertion: a *path* value would print bare.
        let (rendered, _, _) = run(r#"builtins.fetchurl "https://example.invalid/x""#);
        assert_eq!(
            rendered,
            "\"/nix/store/0000000000000000000000000000000a-x\""
        );
        let (context, _, _) =
            run(r#"builtins.getContext (builtins.fetchurl "https://example.invalid/x")"#);
        assert!(
            context.contains("/nix/store/0000000000000000000000000000000a-x"),
            "the fetched path is not in the string's context: {context}"
        );
    }

    /// Not a set and not a string: cppnix reads the whole argument with
    /// `forceStringNoCtx`, so an integer is a type error and not "url
    /// argument required".
    #[test]
    fn a_non_string_non_set_argument_is_a_type_error() {
        assert!(
            fails_with("builtins.fetchurl 1").contains("expected a string but found an integer"),
            "got {}",
            fails_with("builtins.fetchurl 1")
        );
    }

    /// A store with no fetcher behind it refuses by name rather than
    /// answering with a path nobody downloaded -- the same shape
    /// `builtins.toFile` and `builtins.path` take, and the reason
    /// `Host::fetch` defaults to `NoStore`.
    #[test]
    fn no_fetcher_behind_the_host_is_a_named_refusal() {
        struct NoStoreHost;
        impl Host for NoStoreHost {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                find_file,
                nix_path,
                trace,
                warn
            );
            fn read_file(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn read_dir(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Err(format!("path '{p}' does not exist"))
            }
        }
        let Ok(module) = crate::compile::compile_source(
            r#"builtins.fetchTarball "https://example.invalid/x.tar.gz""#,
            "/m",
            crate::compile::Origin::String,
            &crate::eval::Settings::default(),
        ) else {
            unreachable!("the source compiles")
        };
        let mut vm = crate::vm::Vm::with_settings(crate::eval::Settings::default());
        vm.start_module(&std::rc::Rc::new(module));
        match crate::eval::drive(&mut vm, &NoStoreHost) {
            Err(crate::vm::VmError::Unimplemented(refusal)) => {
                assert_eq!(
                    refusal.token,
                    crate::refusal::RefusalToken::StoreUnavailable
                );
                assert!(
                    refusal.detail.contains("builtins.fetchTarball"),
                    "the refusal does not name the fetcher: {}",
                    refusal.detail
                );
            }
            other => unreachable!("expected a named refusal, got {other:?}"),
        }
    }
}

// -- fetchTree and fetchGit -------------------------------------------------

/// The tree fetchers: `builtins.fetchTree`, `builtins.fetchGit` and the
/// internal `fetchFinalTree`, which are cppnix's one `fetchTree()`
/// (`primops/fetchTree.cc:236`) under three `FetchTreeParams`.
///
/// `fetchFinalTree` differs from `fetchTree` only in `isFinal`, which this
/// walk never reads: it is the embedder that sets `__final` on the input or
/// rejects an input carrying it. So every rule below that asks about the
/// fetcher asks [`TreeFetcher::is_fetch_git`], and a final fetch takes
/// `fetchTree`'s branch of each one.
///
/// # What runs here
///
/// Only the part that needs the interpreter or that raises an error a program
/// can catch, which for this primop is the argument walk: force each
/// attribute in the set's own order, classify it as a string, a Boolean or a
/// non-negative integer, and raise cppnix's error where it does not fit. The
/// classified bag then leaves as [`NeedPath::FetchTree`].
///
/// A string attribute goes through the same [`coerce_to_path`]-adjacent rule
/// cppnix uses -- `coerceToString` with `copyToStore` and `coerceMore` both
/// off -- which accepts a string or a path and nothing else. A path is
/// accepted because `fetchGit ./repo` is how a local repository is named.
///
/// # What deliberately does not run here
///
/// `fixGitURL`, the `exportIgnore` and `shallow` defaults, `Input::fromAttrs`,
/// the registry lookup, the locked-input check, `__final`, the input cache and
/// the mount. Those are how an `Input` is built and fetched; the embedder owns
/// them and `fetcher` is in the question so it can apply the right set. See
/// [`NeedPath::FetchTree`] for why `fixGitURL` in particular is not worth
/// reimplementing.
///
/// # Where this is not cppnix
///
/// * Attributes are read in this crate's symbol order, as in [`PathBuiltin`]
///   and [`FetchBuiltin`], so two malformed attributes may be reported in the
///   other order.
/// * `publicKeys` is refused by name. cppnix renders it with
///   `printValueAsJSON` into a string attribute, behind the `verified-fetches`
///   experimental feature. Rendering it here would be a second JSON writer
///   feeding an input attribute, and a differently-spaced document is a
///   different input and so a different store path.
/// * A bare string or path argument (`fetchTree "github:..."`,
///   `fetchGit ./repo`) is refused by name. cppnix routes it through
///   `Input::fromURL` for `fetchTree`, and through `fixGitURL` for `fetchGit`;
///   both are URL parsing this crate does not do. The attribute-set spelling
///   covers both and is what a lock file produces.
pub struct FetchTreeBuiltin {
    fetcher: TreeFetcher,
    /// Attributes still to read, in the argument set's own order.
    queue: VecDeque<(Sym, Slot)>,
    current: Option<String>,
    asked: bool,
    /// `type` seen as an attribute. Tracked rather than read off `attrs`,
    /// because `fetchGit` seeds `type = "git"` before the walk and cppnix
    /// still rejects an explicit `type` attribute there.
    type_attr_seen: bool,
    attrs: BTreeMap<String, TreeAttr>,
}

pub fn bi_fetch_tree(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    begin_fetch_tree(vm, args, TreeFetcher::Tree)
}

pub fn bi_fetch_git(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    begin_fetch_tree(vm, args, TreeFetcher::Git)
}

/// cppnix's `prim_fetchFinalTree`, which is `.internal = true` and so is in
/// neither `builtins` nor the global scope. It is reachable only through
/// `ixe_internal_primop`, which is this crate's `state.internalPrimOps`.
pub fn bi_fetch_final_tree(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    begin_fetch_tree(vm, args, TreeFetcher::FinalTree)
}

/// cppnix's `fixGitURL`, for the bare-string spelling of `builtins.fetchGit`:
/// an scp-style `user@host:path` becomes `ssh://host/path`, a `file:` URL and
/// anything with a scheme pass through, and an absolute path becomes a
/// `file://` URL. A relative path is the one case left to the embedder, so it
/// is returned as `None` and the caller keeps the refusal.
fn fix_git_url(url: &str) -> Option<String> {
    if !url.starts_with('/')
        && let Some((user_host, path)) = url.split_once(':')
        && let Some((_, host)) = user_host.split_once('@')
        && !user_host.contains('/')
        && !path.starts_with('/')
    {
        return Some(format!("ssh://{host}/{path}"));
    }
    if url.starts_with("file:") || url.contains("://") {
        return Some(url.to_owned());
    }
    if url.starts_with('/') {
        return Some(format!("file://{url}"));
    }
    None
}

fn begin_fetch_tree(_vm: &mut Vm, args: &[Slot], fetcher: TreeFetcher) -> Result<Begin> {
    let arg = argv(args, 0)?;
    // `fetchGit "file:///repo"` and `fetchGit ./repo`: cppnix runs the string
    // through `fixGitURL` and reads it as `{ url = ...; }`. That much URL
    // fixing is small enough to carry here; `fetchTree "github:..."` still goes
    // through `Input::fromURL` and stays refused.
    let bare_git_url = match (&arg, fetcher) {
        (Value::Str(s), TreeFetcher::Git) => {
            fix_git_url(crate::primops_pure::text_of(s)?)
        }
        (Value::Path(p), TreeFetcher::Git) => fix_git_url(&p.to_string()),
        _ => None,
    };
    let (queue, seeded_url): (VecDeque<(Sym, Slot)>, Option<String>) = match (&arg, bare_git_url) {
        (Value::Attrs(attrs), _) => (attrs.iter().map(|(k, v)| (*k, v.clone())).collect(), None),
        (_, Some(url)) => (VecDeque::new(), Some(url)),
        _ => {
            return Err(VmError::Unimplemented(Refusal::new(
                RefusalToken::UnimplementedBuiltin,
                format!(
                    "builtins.{} with a {} argument rather than an attribute set: cppnix turns \
                     it into an input with Input::fromURL or fixGitURL, which is URL parsing this \
                     backend does not do",
                    fetcher.error_name(),
                    type_name(&arg)
                ),
            )));
        }
    };
    let mut machine = FetchTreeBuiltin {
        fetcher,
        queue,
        current: None,
        asked: false,
        type_attr_seen: false,
        attrs: BTreeMap::new(),
    };
    if let Some(url) = seeded_url {
        machine.attrs.insert("url".to_owned(), TreeAttr::Str(url));
    }
    // cppnix seeds `type = "git"` for fetchGit before it reads the set, which
    // is why an explicit `type` there is "unexpected argument" rather than a
    // duplicate.
    if fetcher.is_fetch_git() {
        machine
            .attrs
            .insert("type".to_owned(), TreeAttr::Str("git".to_owned()));
    }
    Ok(Begin::Cont(Cont::Ext(Ext::FetchTree(Box::new(machine)))))
}

impl FetchTreeBuiltin {
    fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        match (self.asked, incoming) {
            (true, Some(v)) => Ok(Yield::Done(v)),
            (true, None) => Err(VmError::eval(format!(
                "internal: builtins.{} lost its answer",
                self.fetcher.error_name()
            ))),
            (false, None) => self.next_attr(vm),
            (false, Some(v)) => self.take_attr(vm, v),
        }
    }

    fn next_attr(&mut self, vm: &mut Vm) -> Result<Yield> {
        let Some((sym, slot)) = self.queue.pop_front() else {
            self.current = None;
            return self.ask();
        };
        let name = vm.sym_name(sym).to_owned();
        if name == "type" && self.fetcher.is_fetch_git() {
            // cppnix's `if (type) error "unexpected argument 'type'"`, which
            // for fetchGit fires on the first one because the primop seeded it.
            return Err(VmError::eval("unexpected argument 'type'"));
        }
        if name == "type" {
            self.type_attr_seen = true;
        }
        // `name` is only allowed where `allowNameArgument` is set, which is
        // fetchGit. Raised before the value is forced, as cppnix raises it
        // after `Input::fromAttrs` would have seen it -- see the note in
        // `ask`, which is where cppnix actually checks.
        self.current = Some(name);
        Ok(Yield::Force(slot))
    }

    fn take_attr(&mut self, vm: &mut Vm, value: Value) -> Result<Yield> {
        let name = self.current.clone().ok_or_else(|| {
            VmError::eval(format!(
                "internal: builtins.{} lost an attribute",
                self.fetcher.error_name()
            ))
        })?;
        let fetcher = self.fetcher.error_name();
        let attr = match &value {
            // cppnix's `coerceToString(..., copyToStore = false, coerceMore =
            // false)` over an already-forced value: a string or a path, and
            // nothing else reaches the string branch. Its context is
            // discarded, as cppnix discards `context` here.
            Value::Str(s) => TreeAttr::Str(crate::primops_pure::text_of(s)?.to_owned()),
            Value::Path(p) => TreeAttr::Str(p.to_string()),
            Value::Bool(b) => TreeAttr::Bool(*b),
            Value::Int(n) => {
                if *n < 0 {
                    return Err(VmError::eval(format!(
                        "negative value given for '{fetcher}' argument '{name}': {n}"
                    )));
                }
                // Non-negative, so the cast cannot wrap.
                TreeAttr::Int(u64::try_from(*n).map_err(|_| {
                    VmError::eval(format!("internal: '{name}' does not fit an integer"))
                })?)
            }
            other if name == "publicKeys" => {
                let _ = other;
                return Err(VmError::Unimplemented(Refusal::new(
                    RefusalToken::UnimplementedBuiltin,
                    format!(
                        "builtins.{fetcher} with a 'publicKeys' argument: cppnix renders it \
                         with printValueAsJSON into an input attribute, and a differently \
                         spaced document is a different input and so a different store path"
                    ),
                )));
            }
            other => {
                return Err(VmError::eval(format!(
                    "argument '{name}' to '{fetcher}' is {} while a string, Boolean or \
                     integer is expected",
                    type_name(other)
                )));
            }
        };
        self.attrs.insert(name, attr);
        self.next_attr(vm)
    }

    fn ask(&mut self) -> Result<Yield> {
        let fetcher = self.fetcher.error_name();
        if !self.fetcher.is_fetch_git() && !self.type_attr_seen {
            return Err(VmError::eval(format!(
                "argument 'type' is missing in call to '{fetcher}'"
            )));
        }
        // `allowNameArgument` is set for fetchGit only, and cppnix checks it
        // after the whole set is read -- so `fetchTree { type = "path"; name
        // = "x"; path = <throw>; }` raises the throw first, in both arms.
        if !self.fetcher.is_fetch_git() && self.attrs.contains_key("name") {
            return Err(VmError::eval(format!(
                "argument 'name' isn\u{2019}t supported in call to '{fetcher}'"
            )));
        }
        self.asked = true;
        Ok(Yield::Need(NeedPath::FetchTree(Box::new(
            FetchTreeRequest {
                attrs: std::mem::take(&mut self.attrs),
                fetcher: self.fetcher,
            },
        ))))
    }
}

#[cfg(test)]
mod fetch_tree_tests {
    use crate::host::{FileType, Host, StoreError};
    use crate::task::{FetchTreeRequest, TreeAttr, TreeFetcher};
    use std::cell::RefCell;

    /// A fetcher that records the request and answers with a minimal tree.
    /// As in `fetch_tests`, what is checked is the *question*: the answer's
    /// bytes are cppnix's and are gated by
    /// `maintainers/ix/fetch-tree-parity.sh` on a real store.
    #[derive(Default)]
    struct Trees(RefCell<Vec<FetchTreeRequest>>);

    impl Host for Trees {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            not_async,
        );
        crate::host::host_stubs!(
            file_type_resolved,
            get_env,
            copy_to_store,
            ensure_path,
            find_file,
            nix_path,
            trace,
            warn
        );
        fn read_file(&self, p: &crate::value2::PathValue) -> std::result::Result<String, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn read_dir(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, FileType)>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn path_exists_checked(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(false)
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Option<FileType>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn fetch_tree(
            &self,
            request: &FetchTreeRequest,
        ) -> std::result::Result<String, StoreError> {
            self.0.borrow_mut().push(request.clone());
            Ok(
                r#"{"outPath":"/nix/store/0000000000000000000000000000000a-source",
                   "narHash":"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                   "revCount":7,"submodules":false}"#
                    .to_owned(),
            )
        }
    }

    fn run(src: &str) -> (String, Vec<FetchTreeRequest>) {
        let host = Trees::default();
        let rendered = crate::eval::render_with(&crate::eval::Settings::default(), &host, src);
        let asked = host.0.borrow().clone();
        (rendered, asked)
    }

    fn only(src: &str) -> FetchTreeRequest {
        let (rendered, asked) = run(src);
        super::only_request(
            &format!("tree fetch from {src:?} (result: {rendered})"),
            &asked,
        )
    }

    /// The final fetcher travels under its own name and complains under
    /// cppnix's.
    ///
    /// Both halves matter and they pull in opposite directions. The wire and
    /// the witness have to tell a final fetch from a plain one, because they
    /// are different questions -- one sets `__final` on the input and the
    /// other rejects an input carrying it. Every error text the argument walk
    /// raises has to say `fetchTree`, because cppnix's `fetcher` local is
    /// derived from `isFetchGit` alone and a flake input failing to parse
    /// must not name a primop no program can call.
    #[test]
    fn the_final_fetcher_travels_apart_and_complains_as_fetch_tree() {
        assert_eq!(TreeFetcher::FinalTree.as_str(), "fetchFinalTree");
        assert_eq!(TreeFetcher::FinalTree.error_name(), "fetchTree");
        assert_eq!(TreeFetcher::Tree.error_name(), "fetchTree");
        assert_eq!(TreeFetcher::Git.error_name(), "fetchGit");
        assert!(!TreeFetcher::FinalTree.is_fetch_git());
        assert!(TreeFetcher::FinalTree.is_final());
        assert!(!TreeFetcher::Tree.is_final());
        assert_eq!(
            TreeFetcher::parse("fetchFinalTree"),
            Some(TreeFetcher::FinalTree)
        );

        // The first field of the wire form is what the bridge branches on, so
        // a round trip through it is the assertion that a final fetch cannot
        // arrive looking like a plain one.
        let request = FetchTreeRequest {
            attrs: [("type".to_owned(), TreeAttr::Str("path".to_owned()))]
                .into_iter()
                .collect(),
            fetcher: TreeFetcher::FinalTree,
        };
        let encoded = crate::capi::encode_fetch_tree(&request);
        let first = encoded
            .split(|b| *b == 0)
            .next()
            .map(|f| String::from_utf8_lossy(f).into_owned());
        assert_eq!(first.as_deref(), Some("fetchFinalTree"));
    }

    fn fails_with(src: &str) -> String {
        let (rendered, asked) = run(src);
        assert!(
            asked.is_empty(),
            "{src:?} reached the fetcher, which it should not have: {asked:?}"
        );
        rendered
    }

    /// The three value shapes cppnix's `fetchers::Attrs` holds, each tagged so
    /// the far side cannot mistake one for another. `{ shallow = true; }` and
    /// `{ shallow = "1"; }` are different inputs.
    #[test]
    fn the_three_attribute_shapes_travel_tagged() {
        let r = only(
            r#"builtins.fetchTree {
                 type = "git"; url = "/repo"; shallow = true; revCount = 7;
               }"#,
        );
        assert_eq!(r.fetcher, TreeFetcher::Tree);
        assert_eq!(r.attrs.get("type"), Some(&TreeAttr::Str("git".to_owned())));
        assert_eq!(r.attrs.get("url"), Some(&TreeAttr::Str("/repo".to_owned())));
        assert_eq!(r.attrs.get("shallow"), Some(&TreeAttr::Bool(true)));
        assert_eq!(r.attrs.get("revCount"), Some(&TreeAttr::Int(7)));
    }

    /// A path value is accepted where a string is, because `fetchGit ./repo`
    /// and `fetchTree { type = "path"; path = ./x; }` are how a local tree is
    /// named. cppnix coerces with `coerceToString(copyToStore = false)`, which
    /// takes a path and does not copy it.
    #[test]
    fn a_path_attribute_is_a_string_and_is_not_copied() {
        let r = only(r#"builtins.fetchTree { type = "path"; path = /tmp/tree; }"#);
        assert_eq!(
            r.attrs.get("path"),
            Some(&TreeAttr::Str("/tmp/tree".to_owned()))
        );
    }

    /// `fetchGit` seeds `type = "git"` before it reads the set, which is why
    /// the request carries it without the program writing it -- and why an
    /// explicit `type` there is "unexpected argument" rather than a duplicate.
    #[test]
    fn fetch_git_seeds_its_own_type_and_rejects_an_explicit_one() {
        let r = only(r#"builtins.fetchGit { url = "/repo"; ref = "main"; }"#);
        assert_eq!(r.fetcher, TreeFetcher::Git);
        assert_eq!(r.attrs.get("type"), Some(&TreeAttr::Str("git".to_owned())));
        assert!(
            fails_with(r#"builtins.fetchGit { url = "/repo"; type = "git"; }"#)
                .contains("unexpected argument 'type'")
        );
    }

    /// `fetchTree` has no default type and says so; `name` is a fetchGit-only
    /// argument (`allowNameArgument`).
    #[test]
    fn the_two_shape_errors_are_cppnixs() {
        assert!(
            fails_with(r#"builtins.fetchTree { url = "/repo"; }"#)
                .contains("argument 'type' is missing in call to 'fetchTree'"),
            "got {}",
            fails_with(r#"builtins.fetchTree { url = "/repo"; }"#)
        );
        assert!(
            fails_with(r#"builtins.fetchTree { type = "path"; path = "/x"; name = "n"; }"#)
                .contains("isn\u{2019}t supported in call to 'fetchTree'")
        );
        // The same attribute IS allowed for fetchGit.
        assert_eq!(
            only(r#"builtins.fetchGit { url = "/repo"; name = "n"; }"#)
                .attrs
                .get("name"),
            Some(&TreeAttr::Str("n".to_owned()))
        );
    }

    /// cppnix names the fetcher and the attribute, and rejects a negative
    /// integer before the fetcher sees it -- `fetchers::Attrs` holds a
    /// `uint64_t`, so a negative one would wrap into an enormous positive.
    #[test]
    fn a_bad_attribute_value_is_reported_the_way_cppnix_reports_it() {
        let neg = fails_with(r#"builtins.fetchGit { url = "/repo"; revCount = -1; }"#);
        assert!(
            neg.contains("negative value given for 'fetchGit' argument 'revCount': -1"),
            "got {neg}"
        );
        let bad = fails_with(r#"builtins.fetchTree { type = "path"; extra = [ ]; }"#);
        assert!(
            bad.contains("argument 'extra' to 'fetchTree' is a list while a string, Boolean or integer is expected"),
            "got {bad}"
        );
        let nul = fails_with(r#"builtins.fetchTree { type = "path"; extra = null; }"#);
        assert!(
            bad.contains("is expected") && nul.contains("null"),
            "got {nul}"
        );
    }

    /// `outPath` comes back carrying its own store path as string context.
    /// JSON cannot express that, so the scheduler rebuilds it -- and without
    /// it a derivation reading `(fetchTree ...).outPath` would silently lose
    /// an input.
    #[test]
    fn out_path_carries_the_store_path_as_context() {
        let (context, _) = run(
            r#"builtins.getContext (builtins.fetchTree { type = "path"; path = "/x"; }).outPath"#,
        );
        assert!(
            context.contains("/nix/store/0000000000000000000000000000000a-source"),
            "the fetched tree is not in outPath's context: {context}"
        );
        // The other attributes are plain: cppnix gives context to outPath and
        // to nothing else.
        let (narhash, _) = run(
            r#"builtins.getContext (builtins.fetchTree { type = "path"; path = "/x"; }).narHash"#,
        );
        assert_eq!(narhash, "{ }");
    }

    /// The whole emitted set arrives, with JSON's types preserved -- an
    /// integer stays an integer and a Boolean a Boolean, which
    /// `builtins.fromJSON`'s reader is what guarantees.
    #[test]
    fn the_answer_is_the_whole_attribute_set() {
        let (names, _) = run(
            r#"builtins.concatStringsSep "," (builtins.attrNames (builtins.fetchTree { type = "path"; path = "/x"; }))"#,
        );
        assert_eq!(names, "\"narHash,outPath,revCount,submodules\"");
        let (kinds, _) = run(
            r#"let t = builtins.fetchTree { type = "path"; path = "/x"; };
               in builtins.concatStringsSep "," [ (builtins.typeOf t.revCount) (builtins.typeOf t.submodules) ]"#,
        );
        assert_eq!(kinds, "\"int,bool\"");
    }

    /// A bare `fetchTree` string, a relative `fetchGit` string, and
    /// `publicKeys` are refused by name rather than approximated -- each would
    /// need this crate to reimplement something (flake-ref parsing, a
    /// relative-path anchor, a JSON writer) that decides a store path.
    #[test]
    fn the_two_shapes_this_backend_will_not_guess_are_refused_by_name() {
        for src in [
            r#"builtins.fetchTree "github:NixOS/nixpkgs""#,
            r#"builtins.fetchGit "repo""#,
            r#"builtins.fetchTree { type = "git"; url = "/r"; publicKeys = [ { key = "k"; } ]; }"#,
        ] {
            let out = fails_with(src);
            assert!(
                out.contains("Unimplemented"),
                "{src} should be a named refusal, got {out}"
            );
        }
    }

    /// The bare-string spelling of `fetchGit` goes through cppnix's
    /// `fixGitURL` and arrives at the fetcher as `{ type = "git"; url = ...; }`:
    /// an absolute path becomes a `file://` URL, an scp-style remote becomes
    /// `ssh://`, and anything that already carries a scheme is kept.
    #[test]
    fn fetch_git_bare_strings_go_through_fix_git_url() {
        for (src, url) in [
            (r#"builtins.fetchGit "/repo""#, "file:///repo"),
            (r#"builtins.fetchGit "file:///repo""#, "file:///repo"),
            (r#"builtins.fetchGit "https://h/r.git""#, "https://h/r.git"),
            (
                r#"builtins.fetchGit "git@host:owner/repo.git""#,
                "ssh://host/owner/repo.git",
            ),
        ] {
            let request = only(src);
            assert_eq!(request.fetcher, TreeFetcher::Git, "{src}");
            assert_eq!(
                request.attrs.get("type"),
                Some(&TreeAttr::Str("git".to_owned())),
                "{src}"
            );
            assert_eq!(
                request.attrs.get("url"),
                Some(&TreeAttr::Str(url.to_owned())),
                "{src}"
            );
        }
        // A path value is absolute by construction, so it takes the `file://`
        // branch whatever directory the test runs in.
        let request = only(r#"builtins.fetchGit ./repo"#);
        match request.attrs.get("url") {
            Some(TreeAttr::Str(url)) => {
                assert!(url.starts_with("file:///"), "got {url}");
                assert!(url.ends_with("/repo"), "got {url}");
            }
            other => panic!("path argument should seed a file:// url, got {other:?}"),
        }
    }

    /// No fetcher behind the host: a named refusal, as for the fixed-output
    /// pair, and never a guessed attribute set.
    #[test]
    fn no_fetcher_behind_the_host_is_a_named_refusal() {
        struct NoStoreHost;
        impl Host for NoStoreHost {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                find_file,
                nix_path,
                trace,
                warn
            );
            fn read_file(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn read_dir(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Err(format!("path '{p}' does not exist"))
            }
        }
        let Ok(module) = crate::compile::compile_source(
            r#"builtins.fetchGit { url = "/repo"; }"#,
            "/m",
            crate::compile::Origin::String,
            &crate::eval::Settings::default(),
        ) else {
            unreachable!("the source compiles")
        };
        let mut vm = crate::vm::Vm::with_settings(crate::eval::Settings::default());
        vm.start_module(&std::rc::Rc::new(module));
        match crate::eval::drive(&mut vm, &NoStoreHost) {
            Err(crate::vm::VmError::Unimplemented(refusal)) => {
                assert_eq!(
                    refusal.token,
                    crate::refusal::RefusalToken::StoreUnavailable
                );
                assert!(
                    refusal.detail.contains("builtins.fetchGit"),
                    "{}",
                    refusal.detail
                );
            }
            other => unreachable!("expected a named refusal, got {other:?}"),
        }
    }

    /// An embedder that declines (the read-set tracker case) is unimplemented
    /// and not an evaluation error: as an error it would score a mismatch
    /// against a cpp arm that answers fine. This is the whole reason
    /// `StoreError::Unsupported` exists.
    #[test]
    fn an_embedder_that_declines_is_unimplemented_not_an_error() {
        struct Declining;
        impl Host for Declining {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                find_file,
                nix_path,
                trace,
                warn
            );
            fn read_file(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn read_dir(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn fetch_tree(
                &self,
                _request: &FetchTreeRequest,
            ) -> std::result::Result<String, StoreError> {
                Err(StoreError::Unsupported(
                    "the read-set tracker is on".to_owned(),
                ))
            }
        }
        let Ok(module) = crate::compile::compile_source(
            r#"builtins.fetchGit { url = "/repo"; }"#,
            "/m",
            crate::compile::Origin::String,
            &crate::eval::Settings::default(),
        ) else {
            unreachable!("the source compiles")
        };
        let mut vm = crate::vm::Vm::with_settings(crate::eval::Settings::default());
        vm.start_module(&std::rc::Rc::new(module));
        match crate::eval::drive(&mut vm, &Declining) {
            Err(crate::vm::VmError::Unimplemented(refusal)) => {
                assert_eq!(
                    refusal.token,
                    crate::refusal::RefusalToken::UnimplementedBuiltin
                );
                assert!(
                    refusal.detail.contains("read-set tracker"),
                    "{}",
                    refusal.detail
                );
            }
            other => unreachable!("expected a named refusal, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod path_exists_tests {
    use crate::host::{FileType, Host};
    use std::cell::RefCell;

    /// A filesystem with one file and one directory, recording which of the
    /// two existence hooks each question reached. The split IS the behaviour
    /// under test: cppnix's `prim_pathExists` sends a string with a trailing
    /// `/` or `/.` through a full-resolution lstat and a directory test
    /// (primops.cc:2105-2114), and everything else through the plain
    /// ancestors-only probe.
    #[derive(Default)]
    struct Fs {
        plain: RefCell<Vec<String>>,
        resolved: RefCell<Vec<String>>,
    }

    impl Host for Fs {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            get_env,
            copy_to_store,
            ensure_path,
            find_file,
            nix_path,
            trace,
            warn
        );
        fn read_file(&self, p: &crate::value2::PathValue) -> std::result::Result<String, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn read_dir(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, FileType)>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn file_type(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Option<FileType>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn path_exists_checked(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.plain.borrow_mut().push(p.to_string());
            Ok(canon(p) == "/base/lib.nix" || canon(p) == "/base")
        }
        /// As the real hosts answer it: a path that is not there is `false`,
        /// only a resolver failure is an error.
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            match self.file_type_resolved(path) {
                Ok(kind) => Ok(kind == crate::host::FileType::Directory),
                Err(error) if error.contains("No such file or directory") => Ok(false),
                Err(error) => Err(error),
            }
        }
        fn file_type_resolved(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<FileType, String> {
            self.resolved.borrow_mut().push(p.to_string());
            match canon(p).as_str() {
                "/base/lib.nix" => Ok(FileType::Regular),
                "/base" => Ok(FileType::Directory),
                _ => Err(format!(
                    "getting status of '{p}': No such file or directory"
                )),
            }
        }
    }

    /// What the embedder does to every path a hook receives: the question
    /// travels with the program's own spelling, and `rustFileTypeResolved`
    /// (like every hook in `rust-eval-session.cc`) builds a `CanonPath` from
    /// it, which drops trailing `/` and `/.`. The predicate lives on this
    /// side; the spelling cleanup lives on that one.
    fn canon(p: &str) -> String {
        let mut q = p.to_owned();
        loop {
            if let Some(rest) = q.strip_suffix("/.") {
                q = rest.to_owned();
            } else if q.len() > 1 && q.ends_with('/') {
                q.pop();
            } else {
                return q;
            }
        }
    }

    fn run(src: &str) -> (String, Fs) {
        let host = Fs::default();
        let rendered = crate::eval::render_with(&crate::eval::Settings::default(), &host, src);
        (rendered, host)
    }

    /// The measured rows (nix 2.34.7, `eval-okay-pathexists`): a trailing
    /// `/` or `/.` on a *file* is `false`, on a directory `true`, and a
    /// missing path stays `false` through the error branch. The question is
    /// asked on the canonicalized path -- the slash decides the predicate
    /// and then leaves, as it does in cppnix's `CanonPath`, and the embedder
    /// refuses a non-canonical spelling rather than repairing one.
    #[test]
    fn a_trailing_slash_string_must_name_a_directory() {
        for (src, want) in [
            (r#"builtins.pathExists "/base/lib.nix""#, "true"),
            (r#"builtins.pathExists "/base/lib.nix/""#, "false"),
            (r#"builtins.pathExists "/base/lib.nix/.""#, "false"),
            (r#"builtins.pathExists "/base/""#, "true"),
            (r#"builtins.pathExists "/base/.""#, "true"),
            (r#"builtins.pathExists "/missing/""#, "false"),
        ] {
            let (rendered, _) = run(src);
            assert_eq!(rendered, want, "expr {src}");
        }
        let (_, host) = run(r#"builtins.pathExists "/base/lib.nix/""#);
        assert_eq!(
            host.resolved.borrow().as_slice(),
            ["/base/lib.nix".to_owned()],
            "the trailing-slash spelling resolves fully, and the question \
             carries the canonical spelling: the slash decided the predicate \
             and left, as in cppnix's CanonPath"
        );
        assert!(
            host.plain.borrow().is_empty(),
            "and never reaches the plain probe"
        );
        // The plain spelling keeps its ancestors-only probe: a broken
        // symlink exists (`lstat`), which full resolution would deny.
        let (_, host) = run(r#"builtins.pathExists "/base/lib.nix""#);
        assert_eq!(host.plain.borrow().as_slice(), ["/base/lib.nix".to_owned()]);
        assert!(host.resolved.borrow().is_empty());
    }
}

#[cfg(test)]
mod flake_ref_tests {
    use crate::host::{FileType, Host, StoreError};
    use crate::task::TreeAttr;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// A grammar that records each question and answers with fixed bytes. As
    /// in `fetch_tree_tests`, what these tests pin is the *question* and the
    /// argument walk; the answer's bytes are cppnix's own
    /// (`parseFlakeRef`/`FlakeRef::fromAttrs` on the far side) and are gated
    /// differentially by `maintainers/ix/lang-diff.sh`.
    #[derive(Default)]
    struct Grammar {
        parsed: RefCell<Vec<String>>,
        printed: RefCell<Vec<BTreeMap<String, TreeAttr>>>,
    }

    impl Host for Grammar {
        crate::host::host_stubs!(settle);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            file_type_resolved,
            get_env,
            copy_to_store,
            ensure_path,
            find_file,
            nix_path,
            trace,
            warn
        );
        fn read_file(&self, p: &crate::value2::PathValue) -> std::result::Result<String, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn read_dir(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, FileType)>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn path_exists_checked(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(false)
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Option<FileType>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn parse_flake_ref(&self, flake_ref: &str) -> Result<String, StoreError> {
            self.parsed.borrow_mut().push(flake_ref.to_owned());
            Ok(r#"{"type":"github","owner":"NixOS","repo":"nixpkgs"}"#.to_owned())
        }
        fn flake_ref_to_string(
            &self,
            attrs: &BTreeMap<String, TreeAttr>,
        ) -> Result<String, StoreError> {
            self.printed.borrow_mut().push(attrs.clone());
            Ok("github:NixOS/nixpkgs".to_owned())
        }
    }

    fn run(src: &str) -> (String, Grammar) {
        let host = Grammar::default();
        let rendered = crate::eval::render_with(&crate::eval::Settings::default(), &host, src);
        (rendered, host)
    }

    /// The grammar sees the reference string and its flat answer comes back
    /// as an attribute set -- cppnix's `toAttrs()` shape, scalars only.
    #[test]
    fn parse_flake_ref_asks_the_grammar_and_builds_attrs() {
        let (rendered, host) = run(r#"builtins.parseFlakeRef "github:NixOS/nixpkgs""#);
        assert_eq!(
            host.parsed.borrow().as_slice(),
            ["github:NixOS/nixpkgs".to_owned()]
        );
        assert_eq!(
            rendered,
            r#"{ owner = "NixOS"; repo = "nixpkgs"; type = "github"; }"#
        );
    }

    /// The three value shapes `fetchers::Attrs` holds travel tagged, exactly
    /// as they do for `fetchTree`: `true` and `"1"` are different inputs.
    #[test]
    fn flake_ref_to_string_sends_the_three_shapes_tagged() {
        let (rendered, host) = run(r#"builtins.flakeRefToString {
                 type = "github"; owner = "NixOS"; repo = "nixpkgs";
                 shallow = false; revCount = 7;
               }"#);
        assert_eq!(rendered, r#""github:NixOS/nixpkgs""#);
        let printed = host.printed.borrow();
        let [attrs] = printed.as_slice() else {
            unreachable!("one question, got {printed:?}");
        };
        assert_eq!(attrs.get("type"), Some(&TreeAttr::Str("github".to_owned())));
        assert_eq!(attrs.get("shallow"), Some(&TreeAttr::Bool(false)));
        assert_eq!(attrs.get("revCount"), Some(&TreeAttr::Int(7)));
        assert_eq!(attrs.len(), 5);
    }

    /// cppnix rejects a negative integer and a non-scalar attribute in the
    /// primop, before any flake-ref machinery runs (flake-primops.cc,
    /// `prim_flakeRefToString`), so the walk refuses them here and the
    /// grammar is never asked.
    #[test]
    fn bad_attributes_are_refused_before_the_grammar_is_asked() {
        let (rendered, host) =
            run(r#"builtins.flakeRefToString { type = "github"; revCount = -1; }"#);
        assert!(
            rendered.contains("negative value given for flake ref attr revCount: -1"),
            "{rendered}"
        );
        let (rendered, host2) = run(r#"builtins.flakeRefToString { type = "github"; x = [ 1 ]; }"#);
        assert!(
            rendered.contains(
                "flake reference attribute sets may only contain integers, Booleans, \
                 and strings, but attribute 'x' is a list"
            ),
            "{rendered}"
        );
        assert!(host.printed.borrow().is_empty());
        assert!(host2.printed.borrow().is_empty());
    }

    /// A host with no flake-ref grammar behind it (`NoStore`) makes the call
    /// unimplemented, not an evaluation error: scored as a skip in the
    /// differential gate, never as a mismatch.
    #[test]
    fn no_grammar_behind_the_evaluator_is_unimplemented() {
        struct NoGrammar;
        impl Host for NoGrammar {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                find_file,
                nix_path,
                trace,
                warn
            );
            fn read_file(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn read_dir(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Err(format!("path '{p}' does not exist"))
            }
        }
        let Ok(module) = crate::compile::compile_source(
            r#"builtins.parseFlakeRef "github:NixOS/nixpkgs""#,
            "/m",
            crate::compile::Origin::String,
            &crate::eval::Settings::default(),
        ) else {
            unreachable!("the source compiles")
        };
        let mut vm = crate::vm::Vm::with_settings(crate::eval::Settings::default());
        vm.start_module(&std::rc::Rc::new(module));
        match crate::eval::drive(&mut vm, &NoGrammar) {
            Err(crate::vm::VmError::Unimplemented(refusal)) => {
                assert_eq!(
                    refusal.token,
                    crate::refusal::RefusalToken::StoreUnavailable
                );
                assert!(
                    refusal.detail.contains("builtins.parseFlakeRef"),
                    "{}",
                    refusal.detail
                );
            }
            other => unreachable!("expected a named refusal, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod emit_tests {
    use crate::host::{FileType, Host};
    use std::cell::RefCell;

    /// A host that keeps every line the evaluator emitted, in order, tagged
    /// with the sink it went to. Order and sink are both part of the
    /// contract: cppnix emits before forcing the value, and `trace` and
    /// `warn` are different severities on the embedder's logger.
    #[derive(Default)]
    struct Lines {
        emitted: RefCell<Vec<String>>,
    }

    impl Host for Lines {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            file_type_resolved,
            get_env,
            copy_to_store,
            ensure_path,
            find_file,
            nix_path
        );
        fn read_file(&self, p: &crate::value2::PathValue) -> std::result::Result<String, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn read_dir(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, FileType)>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn path_exists_checked(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(false)
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(
            &self,
            p: &crate::value2::PathValue,
        ) -> std::result::Result<Option<FileType>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn trace(&self, message: &str) {
            self.emitted.borrow_mut().push(format!("trace: {message}"));
        }
        fn warn(&self, message: &str) {
            self.emitted.borrow_mut().push(format!("warn: {message}"));
        }
    }

    /// The rendered result and every line emitted along the way, under the
    /// default settings.
    fn run(src: &str) -> (String, Vec<String>) {
        run_under(&crate::eval::Settings::default(), src)
    }

    /// The same, under a configuration the caller states.
    ///
    /// `trace-verbose` and `abort-on-warn` are what these tests vary, and
    /// they used to vary them by moving the process globals under a write
    /// guard -- which is what every unguarded reader in the suite was racing
    /// (ENG-12939). The `Vm` carries them now, so two tests wanting opposite
    /// values are two values, not two moments.
    fn run_under(settings: &crate::eval::Settings, src: &str) -> (String, Vec<String>) {
        let host = Lines::default();
        let rendered = crate::eval::render_with(settings, &host, src);
        let emitted = host.emitted.borrow().clone();
        (rendered, emitted)
    }

    /// `trace-verbose` on, everything else default.
    fn verbose() -> crate::eval::Settings {
        crate::eval::Settings {
            trace_verbose: true,
            ..crate::eval::Settings::default()
        }
    }

    /// `abort-on-warn` on, everything else default.
    fn aborting() -> crate::eval::Settings {
        crate::eval::Settings {
            abort_on_warn: true,
            ..crate::eval::Settings::default()
        }
    }

    /// With `trace-verbose` off, `traceVerbose` is cppnix's `prim_second`:
    /// no line, and the message is not forced -- so a `throw` sitting there
    /// is never reached. Forcing it would turn a program cppnix answers into
    /// a dead one, which is why the table entry names no strict position.
    #[test]
    fn trace_verbose_off_emits_nothing_and_does_not_force_the_message() {
        let (rendered, emitted) = run(r#"builtins.traceVerbose (throw "unreached") 42"#);
        assert_eq!(rendered, "42");
        assert_eq!(emitted, Vec::<String>::new());
    }

    /// With it on, `traceVerbose` is `prim_trace`: the same line `trace`
    /// would emit, to the same sink.
    #[test]
    fn trace_verbose_on_emits_the_line_trace_would() {
        let (rendered, emitted) = run_under(&verbose(), r#"builtins.traceVerbose "hello" 42"#);
        assert_eq!(rendered, "42");
        assert_eq!(emitted, vec!["trace: hello".to_owned()]);
    }

    /// And with it on the message *is* forced, so the `throw` the off arm
    /// steps over kills the evaluation here. The pair is the point: one test
    /// alone would pass against an implementation that ignored the setting.
    #[test]
    fn trace_verbose_on_forces_the_message() {
        let (rendered, emitted) =
            run_under(&verbose(), r#"builtins.traceVerbose (throw "reached") 42"#);
        assert!(rendered.contains("reached"), "{rendered}");
        assert_eq!(emitted, Vec::<String>::new());
    }

    /// A non-string message goes through the value printer, which is what
    /// cppnix hands `prim_trace` -- `ValuePrinter`, not `printAmbiguous`.
    #[test]
    fn trace_verbose_prints_a_non_string_message() {
        let (rendered, emitted) = run_under(&verbose(), "builtins.traceVerbose { a = 1; } 42");
        assert_eq!(rendered, "42");
        assert_eq!(emitted, vec!["trace: { a = 1; }".to_owned()]);
    }

    /// `abort-on-warn` warns and *then* dies, in that order. Emitting after
    /// the failure, or instead of it, would lose the line in exactly the
    /// configuration somebody turned the setting on to read.
    #[test]
    fn abort_on_warn_emits_the_warning_before_failing() {
        let (rendered, emitted) = run_under(&aborting(), r#"builtins.warn "careful" 42"#);
        assert!(
            rendered.contains("abort-on-warn"),
            "expected the abort, got {rendered}"
        );
        assert_eq!(emitted, vec!["warn: careful".to_owned()]);
    }

    /// The abort is not catchable, matching cppnix, which raises
    /// `EvalBaseError` there precisely so `tryEval` cannot swallow it.
    #[test]
    fn the_abort_on_warn_failure_is_not_catchable() {
        let (rendered, _) = run_under(
            &aborting(),
            r#"builtins.tryEval (builtins.warn "careful" 42)"#,
        );
        assert!(
            rendered.contains("abort-on-warn"),
            "tryEval swallowed the abort: {rendered}"
        );
    }

    /// With the setting off, `warn` still warns and still returns its second
    /// argument. The arm that has to keep working, and the one a wrong
    /// stage transition would break.
    #[test]
    fn warn_without_the_setting_returns_its_value() {
        let (rendered, emitted) = run(r#"builtins.warn "careful" 42"#);
        assert_eq!(rendered, "42");
        assert_eq!(emitted, vec!["warn: careful".to_owned()]);
    }

    /// `trace` emits before forcing its value, so a throwing value still
    /// leaves the line behind. Held here as well as by the corpus because
    /// this is the property the state machine exists for, and the `Emit`
    /// restructuring for `traceVerbose` moved every stage transition in it.
    #[test]
    fn trace_emits_before_forcing_a_throwing_value() {
        let (rendered, emitted) = run(r#"builtins.trace "seen" (throw "boom")"#);
        assert!(rendered.contains("boom"), "{rendered}");
        assert_eq!(emitted, vec!["trace: seen".to_owned()]);
    }
}
// -- builtins.getFlake -------------------------------------------------------

/// Where `builtins.getFlake` is.
///
/// Four states because four things happen in order and each needs the one
/// before it: ask the embedder to lock, compile the `call-flake.nix` it sent
/// back, force that module's entry to get the function, apply it.
enum GetFlakeStage {
    /// Asked the embedder; the answer is next.
    Locking,
    /// Compiled and forced the program; the function is next.
    Program,
    /// Applied it; the flake's outputs are next.
    Applied,
}

pub struct GetFlake {
    stage: GetFlakeStage,
    flake_ref: String,
    /// The lock file and the overrides document, held between the step that
    /// receives them and the step that has a function to apply them to.
    /// Boxed so `Cont` does not carry two `String`s inline for every
    /// continuation in the machine.
    pending: Option<Box<(String, String)>>,
}

/// `builtins.getFlake`.
///
/// cppnix's `prim_getFlake` (`libflake/flake-primops.cc`) is two halves with a
/// clean line between them, and this is that line. Everything before
/// `callFlake` is locking -- parsing the reference, the pure-eval rule that
/// refuses an unlocked one, the registry, the input-graph walk, the fetches --
/// which is IO and policy the embedder owns and which happens behind
/// [`crate::task::NeedPath::Flake`]. `callFlake` itself is an ordinary Nix
/// application, and this performs it.
///
/// **The same seam the `<flake>#attr` command line uses.** There
/// `rustEvaluandOf` locks in C++ and hands the VM `call-flake.nix` plus its
/// three arguments; here the VM asks for the same three and applies them
/// itself. One program, one set of arguments, two ways in -- which is what
/// `rust-flake-entry.md` asks for and the reason this is not a second
/// implementation of flake evaluation. The thing to check if they ever
/// disagree is that both still send `Origin::String` and a `/` base for
/// `call-flake.nix`, because that is what decides `__curPos` and how its path
/// literals resolve.
pub fn bi_get_flake(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    // `forceStringNoCtx`, as cppnix does. A flake reference carrying string
    // context would be naming a derivation output as a place to fetch from,
    // which is not a thing; `ArgType::StrNoCtx` in the table is what makes the
    // driver say so in cppnix's words before this runs.
    let flake_ref = crate::primops_pure::want_text_no_ctx(&argv(args, 0)?)?;
    Ok(Begin::Cont(Cont::Ext(Ext::GetFlake(GetFlake {
        stage: GetFlakeStage::Locking,
        flake_ref,
        pending: None,
    }))))
}

/// `builtins.parseFlakeRef`.
///
/// cppnix's `prim_parseFlakeRef` (`libflake/flake-primops.cc`) is one line of
/// evaluation -- `forceStringNoCtx` -- and then grammar, and the grammar is
/// the embedder's: see [`NeedPath::ParseFlakeRef`] for why. The answer
/// arrives as the finished attribute set, built in `eval::flake_ref_attrs`
/// from the JSON the hook returned, so this continuation only hands it back.
///
/// One known ordering edge, deliberate: cppnix's disabled-feature stub
/// raises `MissingExperimentalFeature` at *application*, before forcing the
/// argument, where this side forces and validates first and meets the gate
/// only when the question reaches the hook. With flakes off, a call whose
/// argument is also invalid (`builtins.parseFlakeRef 5`) reports the type
/// error here and the feature error there. Carrying the feature bit into
/// the evaluator just to re-order two errors is not worth a setting; the
/// valid-argument case -- the only one a program can rely on -- matches
/// (measured: flakes off, `builtins.parseFlakeRef "github:a/b"` raises the
/// feature error on both arms). The same edge applies to
/// [`bi_flake_ref_to_string`]'s attribute walk.
pub fn bi_parse_flake_ref(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let flake_ref = crate::primops_pure::want_text_no_ctx(&argv(args, 0)?)?;
    Ok(Begin::Cont(Cont::Ask {
        asked: false,
        need: NeedPath::ParseFlakeRef(flake_ref),
    }))
}

/// `builtins.flakeRefToString`'s walk: force each attribute in the set's
/// order, classify it as one of the three `fetchers::Attr` shapes, then hand
/// the bag to the embedder's `FlakeRef::fromAttrs` behind
/// [`NeedPath::FlakeRefToString`].
///
/// The two classification errors are cppnix's own, raised here because they
/// are evaluation errors on values the interpreter had to produce
/// (`flake-primops.cc`, `prim_flakeRefToString`): a negative integer is
/// `negative value given for flake ref attr ...` -- the corpus compares that
/// line byte for byte (`eval-fail-flake-ref-to-string-negative-integer`) --
/// and anything that is not a string, an integer or a Boolean is the
/// may-only-contain error. A string's context is dropped, as cppnix's
/// `string_view()` drops it, and a path value is *not* coerced: cppnix tests
/// `nString` only, so a path lands in the wrong-type arm.
pub struct FlakeRefString {
    /// Attributes still to read, in the argument set's own order.
    queue: VecDeque<(Sym, Slot)>,
    current: Option<String>,
    asked: bool,
    attrs: BTreeMap<String, TreeAttr>,
}

pub fn bi_flake_ref_to_string(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let arg = argv(args, 0)?;
    let attrs = want_attrs(&arg)?;
    Ok(Begin::Cont(Cont::Ext(Ext::FlakeRefToString(Box::new(
        FlakeRefString {
            queue: attrs.iter().map(|(k, v)| (*k, v.clone())).collect(),
            current: None,
            asked: false,
            attrs: BTreeMap::new(),
        },
    )))))
}

impl FlakeRefString {
    fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        match (self.asked, incoming) {
            (true, Some(v)) => Ok(Yield::Done(v)),
            (true, None) => Err(VmError::eval(
                "internal: builtins.flakeRefToString lost its answer",
            )),
            (false, None) => self.next_attr(vm),
            (false, Some(v)) => self.take_attr(vm, v),
        }
    }

    fn next_attr(&mut self, vm: &mut Vm) -> Result<Yield> {
        let Some((sym, slot)) = self.queue.pop_front() else {
            self.current = None;
            self.asked = true;
            return Ok(Yield::Need(NeedPath::FlakeRefToString(std::mem::take(
                &mut self.attrs,
            ))));
        };
        self.current = Some(vm.sym_name(sym).to_owned());
        Ok(Yield::Force(slot))
    }

    fn take_attr(&mut self, vm: &mut Vm, value: Value) -> Result<Yield> {
        let name = self.current.clone().ok_or_else(|| {
            VmError::eval("internal: builtins.flakeRefToString lost an attribute")
        })?;
        let attr = match &value {
            Value::Str(s) => TreeAttr::Str(crate::primops_pure::text_of(s)?.to_owned()),
            Value::Bool(b) => TreeAttr::Bool(*b),
            Value::Int(n) => {
                if *n < 0 {
                    return Err(VmError::eval(format!(
                        "negative value given for flake ref attr {name}: {n}"
                    )));
                }
                // Non-negative, so the cast cannot wrap.
                TreeAttr::Int(u64::try_from(*n).map_err(|_| {
                    VmError::eval(format!("internal: '{name}' does not fit an integer"))
                })?)
            }
            other => {
                return Err(VmError::eval(format!(
                    "flake reference attribute sets may only contain integers, Booleans, \
                     and strings, but attribute '{name}' is {}",
                    type_name(other)
                )));
            }
        };
        self.attrs.insert(name, attr);
        self.next_attr(vm)
    }
}

impl GetFlake {
    fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        match self.stage {
            GetFlakeStage::Locking => {
                if incoming.is_none() {
                    return Ok(Yield::Need(NeedPath::Flake(self.flake_ref.clone())));
                }
                let answer =
                    incoming.ok_or_else(|| VmError::eval("internal: getFlake answer lost"))?;
                let m = want_attrs(&answer)?;
                let field = |vm: &mut Vm, name: &str| -> Result<String> {
                    let sym = vm.intern(name);
                    let slot = m
                        .get(&sym)
                        .ok_or_else(|| VmError::eval("internal: malformed getFlake answer"))?;
                    want_text(&crate::vm::forced(slot)?)
                };
                let source = field(vm, "source")?;
                let lock_file = field(vm, "lockFile")?;
                let overrides = field(vm, "overrides")?;
                // Held for the next step, where the function exists. Stashing
                // them in the machine rather than re-asking is the point of
                // the three-in-one answer.
                self.pending = Some(Box::new((lock_file, overrides)));
                self.stage = GetFlakeStage::Program;
                // `/` and not the flake's directory: nothing in
                // `call-flake.nix` names a relative path, and cppnix gives it
                // a source accessor with no directory either. Same two values
                // `rustEvaluandOf` sends.
                let module = vm.internal_module(&source, "/")?;
                let entry = module.entry;
                Ok(Yield::Force(Slot::thunk(
                    module,
                    entry,
                    std::rc::Rc::new(crate::value2::EnvNode::Root),
                )))
            }
            GetFlakeStage::Program => {
                let program =
                    incoming.ok_or_else(|| VmError::eval("internal: call-flake.nix lost"))?;
                let (lock_file, overrides) = *self
                    .pending
                    .take()
                    .ok_or_else(|| VmError::eval("internal: getFlake arguments lost"))?;
                let overrides_json: serde_json::Value =
                    serde_json::from_str(&overrides).map_err(|e| {
                        VmError::eval(format!("the embedder's flake overrides are not JSON: {e}"))
                    })?;
                let overrides_value =
                    crate::primops_pure::json_value_with_store_paths(vm, &overrides_json)?;
                // The third argument is this crate's own `fetchFinalTree`, not
                // the embedder's: it is a function, which the question
                // boundary cannot carry, and it is already implemented here as
                // `TreeFetcher::FinalTree`.
                let idx = crate::builtins::global_index("fetchFinalTree").ok_or_else(|| {
                    VmError::eval("internal: fetchFinalTree is not in the builtin table")
                })?;
                self.stage = GetFlakeStage::Applied;
                Ok(Yield::Force(Slot::pending(
                    Slot::value(program),
                    vec![
                        Slot::value(Value::Str(lock_file.into())),
                        Slot::value(overrides_value),
                        Slot::value(crate::builtins::mk_value(idx)),
                    ],
                )))
            }
            GetFlakeStage::Applied => incoming
                .map(Yield::Done)
                .ok_or_else(|| VmError::eval("internal: getFlake result lost")),
        }
    }
}
